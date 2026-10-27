//! The bus's SQLite database: each local agent's queue, the outbox and sent
//! history, and receipts owed to other machines.
//!
//! The queue works like a Redis stream with one consumer group per agent.
//! A message handed to a session stays pending until it's acknowledged, and
//! messages pending with a session that has ended are handed out again.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::OpenOptions;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::proto::{Message, Receipt, SendState};

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS agents (
    name       TEXT PRIMARY KEY,
    created_at INTEGER NOT NULL,
    app        TEXT,
    last_live  INTEGER
);
CREATE TABLE IF NOT EXISTS inbox (
    seq           INTEGER PRIMARY KEY AUTOINCREMENT,
    id            TEXT NOT NULL UNIQUE,
    agent         TEXT NOT NULL,
    sender        TEXT NOT NULL,
    recipient     TEXT NOT NULL,
    owner         TEXT NOT NULL,
    body          TEXT NOT NULL,
    reply_to      TEXT,
    expects_reply INTEGER NOT NULL,
    wakes         INTEGER NOT NULL DEFAULT 1,
    created_at    INTEGER NOT NULL,
    stored_at     INTEGER NOT NULL,
    delivered_at  INTEGER,
    session       TEXT,
    acked_at      INTEGER,
    pipe          TEXT,
    attended      INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS inbox_open ON inbox (agent, acked_at, seq);
CREATE INDEX IF NOT EXISTS inbox_reply ON inbox (reply_to);
CREATE TABLE IF NOT EXISTS sent (
    id            TEXT PRIMARY KEY,
    agent         TEXT NOT NULL,
    sender        TEXT NOT NULL,
    recipient     TEXT NOT NULL,
    body          TEXT NOT NULL,
    reply_to      TEXT,
    expects_reply INTEGER NOT NULL,
    created_at    INTEGER NOT NULL,
    state         TEXT NOT NULL,
    error         TEXT,
    attempts      INTEGER NOT NULL DEFAULT 0,
    next_try      INTEGER NOT NULL DEFAULT 0,
    updated_at    INTEGER NOT NULL,
    pipe          TEXT,
    attended      INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS sent_due ON sent (state, next_try);
CREATE INDEX IF NOT EXISTS sent_agent ON sent (agent, created_at);
CREATE TABLE IF NOT EXISTS receipts (
    seq      INTEGER PRIMARY KEY AUTOINCREMENT,
    machine  TEXT NOT NULL,
    id       TEXT NOT NULL,
    state    TEXT NOT NULL,
    at       INTEGER NOT NULL,
    attempts INTEGER NOT NULL DEFAULT 0,
    next_try INTEGER NOT NULL DEFAULT 0
);
";

pub struct Store {
    db: Connection,
}

/// A message in a local agent's queue.
#[derive(Debug, Clone)]
pub struct Incoming {
    pub msg: Message,
    /// The Tailscale login that owns the sending machine.
    pub owner: String,
    pub delivered_at: Option<i64>,
    /// The session it was last handed to.
    pub session: Option<String>,
    pub acked_at: Option<i64>,
    /// Whether it may wake an idle session: it's a request, or it answers
    /// one of the recipient's requests. Anything else waits for the
    /// session's next turn.
    pub wakes: bool,
}

/// A message a local agent sent.
#[derive(Debug, Clone)]
pub struct Outgoing {
    pub msg: Message,
    pub state: SendState,
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct AgentRow {
    pub name: String,
    pub app: Option<String>,
    pub last_live: Option<i64>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Stored {
    New,
    Duplicate,
    UnknownAgent,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Acked {
    /// Newly acknowledged; the sender gets a receipt.
    Now {
        sender: String,
    },
    Already,
    NotFound,
}

pub struct Due<T> {
    pub item: T,
    pub attempts: u32,
}

/// A receipt waiting to go to `machine`.
pub struct OwedReceipt {
    pub seq: i64,
    pub machine: String,
    pub receipt: Receipt,
}

/// A message in a thread, from whichever side this machine saw it.
pub enum ThreadEntry {
    In(Incoming),
    Out(Outgoing),
}

impl ThreadEntry {
    pub fn msg(&self) -> &Message {
        match self {
            ThreadEntry::In(m) => &m.msg,
            ThreadEntry::Out(m) => &m.msg,
        }
    }
}

// Columns added later go at the end of each list, so earlier indexes hold.
const INCOMING: &str = "id, sender, recipient, body, reply_to, expects_reply, created_at, \
                        owner, delivered_at, session, acked_at, wakes, pipe, attended";
const OUTGOING: &str = "id, sender, recipient, body, reply_to, expects_reply, created_at, \
                        state, error, pipe, attended";

/// A message from a row, with its `pipe` and `attended` columns at `extra`
/// and the one after.
fn message(r: &Row, extra: usize) -> rusqlite::Result<Message> {
    let pipe: Option<String> = r.get(extra)?;
    Ok(Message {
        id: r.get(0)?,
        from: r.get(1)?,
        to: r.get(2)?,
        body: r.get(3)?,
        reply_to: r.get(4)?,
        expects_reply: r.get(5)?,
        created_at: r.get(6)?,
        pipe: pipe.and_then(|p| serde_json::from_str(&p).ok()),
        attended: r.get(extra + 1)?,
    })
}

fn pipe_column(msg: &Message) -> Option<String> {
    msg.pipe
        .as_ref()
        .and_then(|p| serde_json::to_string(p).ok())
}

fn incoming(r: &Row) -> rusqlite::Result<Incoming> {
    Ok(Incoming {
        msg: message(r, 12)?,
        owner: r.get(7)?,
        delivered_at: r.get(8)?,
        session: r.get(9)?,
        acked_at: r.get(10)?,
        wakes: r.get(11)?,
    })
}

fn outgoing(r: &Row) -> rusqlite::Result<Outgoing> {
    Ok(Outgoing {
        msg: message(r, 9)?,
        state: SendState::parse(&r.get::<_, String>(7)?),
        error: r.get(8)?,
    })
}

/// The machine part of `name@machine`.
pub fn machine_part(addr: &str) -> &str {
    addr.rsplit_once('@').map_or("", |(_, m)| m)
}

/// The name part of `name@machine`.
pub fn name_part(addr: &str) -> &str {
    addr.split_once('@').map_or(addr, |(n, _)| n)
}

impl Store {
    /// Opens the database, creating it readable by this user only.
    pub fn open(path: &Path) -> Result<Store> {
        OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(path)
            .with_context(|| format!("creating {}", path.display()))?;
        let db = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        Store::init(db)
    }

    #[cfg(test)]
    pub fn memory() -> Store {
        Store::init(Connection::open_in_memory().unwrap()).unwrap()
    }

    fn init(db: Connection) -> Result<Store> {
        db.pragma_update_and_check(None, "journal_mode", "WAL", |r| r.get::<_, String>(0))?;
        // FULL, so an acknowledged store survives a crash or power loss.
        db.pragma_update(None, "synchronous", "FULL")?;
        db.busy_timeout(std::time::Duration::from_secs(5))?;
        db.execute_batch(SCHEMA)?;
        // Columns added after 0.1.0. Queues from then are all requests, so
        // their messages wake.
        for (table, column, kind) in [
            ("inbox", "wakes", "INTEGER NOT NULL DEFAULT 1"),
            ("inbox", "pipe", "TEXT"),
            ("inbox", "attended", "INTEGER NOT NULL DEFAULT 0"),
            ("sent", "pipe", "TEXT"),
            ("sent", "attended", "INTEGER NOT NULL DEFAULT 0"),
        ] {
            let has = db
                .prepare(&format!(
                    "SELECT 1 FROM pragma_table_info('{table}') WHERE name = '{column}'"
                ))?
                .exists([])?;
            if !has {
                db.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column} {kind}"))?;
            }
        }
        Ok(Store { db })
    }

    // Agents.

    /// Subscribes `name`. Returns whether it's new.
    pub fn add_agent(&self, name: &str, now: i64) -> Result<bool> {
        let n = self.db.execute(
            "INSERT OR IGNORE INTO agents (name, created_at) VALUES (?1, ?2)",
            params![name, now],
        )?;
        Ok(n == 1)
    }

    pub fn has_agent(&self, name: &str) -> Result<bool> {
        Ok(self
            .db
            .query_row("SELECT 1 FROM agents WHERE name = ?1", [name], |_| Ok(()))
            .optional()?
            .is_some())
    }

    /// Records that a session holds `name` now, subscribing it if needed.
    pub fn agent_live(&self, name: &str, app: Option<&str>, now: i64) -> Result<()> {
        self.db.execute(
            "INSERT INTO agents (name, created_at, app, last_live) VALUES (?1, ?2, ?3, ?2)
             ON CONFLICT (name) DO UPDATE
             SET app = coalesce(excluded.app, app), last_live = excluded.last_live",
            params![name, now, app],
        )?;
        Ok(())
    }

    pub fn agents(&self) -> Result<Vec<AgentRow>> {
        let mut stmt = self
            .db
            .prepare("SELECT name, app, last_live FROM agents ORDER BY name")?;
        let rows = stmt.query_map([], |r| {
            Ok(AgentRow {
                name: r.get(0)?,
                app: r.get(1)?,
                last_live: r.get(2)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    // The queue.

    /// Appends a message to `agent`'s queue. A message seen before is
    /// left as it is.
    pub fn store_incoming(&self, msg: &Message, owner: &str, now: i64) -> Result<Stored> {
        let agent = name_part(&msg.to);
        if !self.has_agent(agent)? {
            return Ok(Stored::UnknownAgent);
        }
        // The recipient's own messages are in this machine's sent history,
        // so whether a reply answers one of its requests is known here.
        let wakes = msg.expects_reply
            || match &msg.reply_to {
                Some(r) => self
                    .db
                    .prepare("SELECT 1 FROM sent WHERE id = ?1 AND agent = ?2 AND expects_reply")?
                    .exists(params![r, agent])?,
                None => false,
            };
        let n = self.db.execute(
            "INSERT OR IGNORE INTO inbox
             (id, agent, sender, recipient, owner, body, reply_to, expects_reply, wakes,
              created_at, stored_at, pipe, attended)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                msg.id,
                agent,
                msg.from,
                msg.to,
                owner,
                msg.body,
                msg.reply_to,
                msg.expects_reply,
                wakes,
                msg.created_at,
                now,
                pipe_column(msg),
                msg.attended
            ],
        )?;
        Ok(if n == 1 {
            Stored::New
        } else {
            Stored::Duplicate
        })
    }

    /// The messages `session` may take from `agent`'s queue, oldest first:
    /// ones never handed out, and ones pending with a session that isn't
    /// in `live`.
    pub fn takeable(
        &self,
        agent: &str,
        session: &str,
        live: &HashSet<String>,
        max: usize,
    ) -> Result<Vec<Incoming>> {
        let mut stmt = self.db.prepare(&format!(
            "SELECT {INCOMING} FROM inbox WHERE agent = ?1 AND acked_at IS NULL ORDER BY seq"
        ))?;
        let mut out = Vec::new();
        for row in stmt.query_map([agent], incoming)? {
            let row = row?;
            let free = match &row.session {
                None => true,
                Some(s) => s != session && !live.contains(s),
            };
            if free {
                out.push(row);
                if out.len() == max {
                    break;
                }
            }
        }
        Ok(out)
    }

    /// Records that `session` took these messages. Returns the ones handed
    /// out for the first time, whose senders get a receipt.
    pub fn mark_delivered(
        &mut self,
        ids: &[String],
        session: &str,
        now: i64,
    ) -> Result<Vec<Message>> {
        let tx = self.db.transaction()?;
        let mut first = Vec::new();
        for id in ids {
            let row = tx
                .query_row(
                    &format!("SELECT {INCOMING} FROM inbox WHERE id = ?1 AND acked_at IS NULL"),
                    [id],
                    incoming,
                )
                .optional()?;
            let Some(row) = row else { continue };
            tx.execute(
                "UPDATE inbox SET delivered_at = coalesce(delivered_at, ?2), session = ?3
                 WHERE id = ?1",
                params![id, now, session],
            )?;
            if row.delivered_at.is_none() {
                first.push(row.msg);
            }
        }
        tx.commit()?;
        Ok(first)
    }

    pub fn incoming(&self, id: &str) -> Result<Option<Incoming>> {
        Ok(self
            .db
            .query_row(
                &format!("SELECT {INCOMING} FROM inbox WHERE id = ?1"),
                [id],
                incoming,
            )
            .optional()?)
    }

    /// `agent`'s messages that aren't acknowledged yet.
    pub fn open_incoming(&self, agent: &str) -> Result<Vec<Incoming>> {
        let mut stmt = self.db.prepare(&format!(
            "SELECT {INCOMING} FROM inbox WHERE agent = ?1 AND acked_at IS NULL ORDER BY seq"
        ))?;
        let rows = stmt.query_map([agent], incoming)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn ack(&self, agent: &str, id: &str, now: i64) -> Result<Acked> {
        let row: Option<(String, Option<i64>)> = self
            .db
            .query_row(
                "SELECT sender, acked_at FROM inbox WHERE id = ?1 AND agent = ?2",
                params![id, agent],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        match row {
            None => Ok(Acked::NotFound),
            Some((_, Some(_))) => Ok(Acked::Already),
            Some((sender, None)) => {
                self.db.execute(
                    "UPDATE inbox SET acked_at = ?2, delivered_at = coalesce(delivered_at, ?2)
                     WHERE id = ?1",
                    params![id, now],
                )?;
                Ok(Acked::Now { sender })
            }
        }
    }

    /// Marks handled the messages handed to `agent` that ask for nothing
    /// back, once the turn they arrived in is over. Returns each one's ID
    /// and sender, for receipts.
    pub fn ack_responses(&self, agent: &str, now: i64) -> Result<Vec<(String, String)>> {
        let mut stmt = self.db.prepare(
            "SELECT id, sender FROM inbox
             WHERE agent = ?1 AND acked_at IS NULL AND delivered_at IS NOT NULL
               AND NOT expects_reply",
        )?;
        let rows = stmt
            .query_map([agent], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<Vec<(String, String)>>>()?;
        for (id, _) in &rows {
            self.db.execute(
                "UPDATE inbox SET acked_at = ?2 WHERE id = ?1",
                params![id, now],
            )?;
        }
        Ok(rows)
    }

    // The outbox and sent history.

    pub fn record_sent(&self, msg: &Message, state: SendState, now: i64) -> Result<()> {
        self.db.execute(
            "INSERT INTO sent
             (id, agent, sender, recipient, body, reply_to, expects_reply, created_at, state,
              updated_at, pipe, attended)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                msg.id,
                name_part(&msg.from),
                msg.from,
                msg.to,
                msg.body,
                msg.reply_to,
                msg.expects_reply,
                msg.created_at,
                state.as_str(),
                now,
                pipe_column(msg),
                msg.attended
            ],
        )?;
        Ok(())
    }

    pub fn outgoing(&self, id: &str) -> Result<Option<Outgoing>> {
        Ok(self
            .db
            .query_row(
                &format!("SELECT {OUTGOING} FROM sent WHERE id = ?1"),
                [id],
                outgoing,
            )
            .optional()?)
    }

    /// Queued messages whose next try is due, oldest first.
    pub fn due_sends(&self, now: i64, limit: usize) -> Result<Vec<Due<Message>>> {
        let mut stmt = self.db.prepare(&format!(
            "SELECT {OUTGOING}, attempts FROM sent
             WHERE state = 'queued' AND next_try <= ?1 ORDER BY created_at LIMIT ?2"
        ))?;
        let rows = stmt.query_map(params![now, limit as i64], |r| {
            Ok(Due {
                item: message(r, 9)?,
                attempts: r.get(11)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn set_send_state(
        &self,
        id: &str,
        state: SendState,
        error: Option<&str>,
        now: i64,
    ) -> Result<()> {
        self.db.execute(
            "UPDATE sent SET state = ?2, error = ?3, updated_at = ?4 WHERE id = ?1",
            params![id, state.as_str(), error, now],
        )?;
        Ok(())
    }

    pub fn retry_send(&self, id: &str, next_try: i64, error: &str) -> Result<()> {
        self.db.execute(
            "UPDATE sent SET attempts = attempts + 1, next_try = ?2, error = ?3 WHERE id = ?1",
            params![id, next_try, error],
        )?;
        Ok(())
    }

    /// Makes everything waiting for `machine` due now, for when it comes
    /// back online.
    pub fn retry_machine_now(&self, machine: &str) -> Result<usize> {
        let pattern = format!("%@{machine}");
        let a = self.db.execute(
            "UPDATE sent SET next_try = 0 WHERE state = 'queued' AND next_try > 0 AND recipient LIKE ?1",
            [&pattern],
        )?;
        let b = self.db.execute(
            "UPDATE receipts SET next_try = 0 WHERE next_try > 0 AND machine = ?1",
            [machine],
        )?;
        Ok(a + b)
    }

    /// When the next queued message or receipt is due, if any.
    pub fn next_due(&self) -> Result<Option<i64>> {
        Ok(self.db.query_row(
            "SELECT min(t) FROM (
                SELECT min(next_try) AS t FROM sent WHERE state = 'queued'
                UNION ALL SELECT min(next_try) FROM receipts)",
            [],
            |r| r.get(0),
        )?)
    }

    /// What `agent` is still owed: messages queued here, messages that
    /// expect a reply and haven't had one, and failures since `since`.
    pub fn open_outgoing(&self, agent: &str, since: i64) -> Result<Vec<Outgoing>> {
        let mut stmt = self.db.prepare(&format!(
            "SELECT {OUTGOING} FROM sent WHERE agent = ?1 AND (
                state = 'queued'
                OR (expects_reply AND state IN ('stored', 'delivered'))
                OR (state = 'failed' AND updated_at >= ?2))
             ORDER BY created_at"
        ))?;
        let rows = stmt.query_map(params![agent, since], outgoing)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Applies a receipt from `machine`, if it's for a message sent there
    /// and moves it forward.
    pub fn apply_receipt(&self, machine: &str, receipt: &Receipt, now: i64) -> Result<bool> {
        let Some(sent) = self.outgoing(&receipt.id)? else {
            return Ok(false);
        };
        if machine_part(&sent.msg.to) != machine
            || sent.state == SendState::Failed
            || sent.state >= receipt.state
            || !matches!(
                receipt.state,
                SendState::Delivered | SendState::Acknowledged
            )
        {
            return Ok(false);
        }
        self.set_send_state(&receipt.id, receipt.state, None, now)?;
        Ok(true)
    }

    // Receipts owed to other machines.

    pub fn queue_receipt(&self, machine: &str, receipt: &Receipt) -> Result<()> {
        self.db.execute(
            "INSERT INTO receipts (machine, id, state, at) VALUES (?1, ?2, ?3, ?4)",
            params![machine, receipt.id, receipt.state.as_str(), receipt.at],
        )?;
        Ok(())
    }

    pub fn due_receipts(&self, now: i64, limit: usize) -> Result<Vec<Due<OwedReceipt>>> {
        let mut stmt = self.db.prepare(
            "SELECT seq, machine, id, state, at, attempts FROM receipts
             WHERE next_try <= ?1 ORDER BY seq LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![now, limit as i64], |r| {
            Ok(Due {
                item: OwedReceipt {
                    seq: r.get(0)?,
                    machine: r.get(1)?,
                    receipt: Receipt {
                        id: r.get(2)?,
                        state: SendState::parse(&r.get::<_, String>(3)?),
                        at: r.get(4)?,
                    },
                },
                attempts: r.get(5)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn drop_receipt(&self, seq: i64) -> Result<()> {
        self.db
            .execute("DELETE FROM receipts WHERE seq = ?1", [seq])?;
        Ok(())
    }

    pub fn retry_receipt(&self, seq: i64, next_try: i64) -> Result<()> {
        self.db.execute(
            "UPDATE receipts SET attempts = attempts + 1, next_try = ?2 WHERE seq = ?1",
            params![seq, next_try],
        )?;
        Ok(())
    }

    // Threads, housekeeping.

    /// Every message this machine has seen in the thread containing `id`,
    /// oldest first.
    pub fn thread(&self, id: &str) -> Result<Vec<ThreadEntry>> {
        let mut parent: HashMap<String, Option<String>> = HashMap::new();
        for table in ["inbox", "sent"] {
            let mut stmt = self
                .db
                .prepare(&format!("SELECT id, reply_to FROM {table}"))?;
            for row in stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))? {
                let (id, reply_to): (String, Option<String>) = row?;
                parent.insert(id, reply_to);
            }
        }
        if !parent.contains_key(id) {
            return Ok(Vec::new());
        }
        let mut root = id.to_string();
        let mut seen = HashSet::new();
        while let Some(Some(up)) = parent.get(&root) {
            if !parent.contains_key(up) || !seen.insert(root.clone()) {
                break;
            }
            root = up.clone();
        }
        let mut children: HashMap<&str, Vec<&str>> = HashMap::new();
        for (child, up) in &parent {
            if let Some(up) = up {
                children
                    .entry(up.as_str())
                    .or_default()
                    .push(child.as_str());
            }
        }
        let mut members = HashSet::new();
        let mut todo = VecDeque::from([root.as_str()]);
        while let Some(next) = todo.pop_front() {
            if members.insert(next.to_string()) {
                todo.extend(children.get(next).into_iter().flatten());
            }
        }
        let mut out = Vec::new();
        for id in members {
            if let Some(m) = self.incoming(&id)? {
                out.push(ThreadEntry::In(m));
            } else if let Some(m) = self.outgoing(&id)? {
                out.push(ThreadEntry::Out(m));
            }
        }
        out.sort_by(|a, b| {
            (a.msg().created_at, &a.msg().id).cmp(&(b.msg().created_at, &b.msg().id))
        });
        Ok(out)
    }

    /// Deletes acknowledged and failed messages, and stuck receipts, older
    /// than `before`. Messages that aren't acknowledged are kept however old.
    pub fn prune(&self, before: i64) -> Result<usize> {
        let mut n = self.db.execute(
            "DELETE FROM inbox WHERE acked_at IS NOT NULL AND acked_at < ?1",
            [before],
        )?;
        n += self.db.execute(
            "DELETE FROM sent WHERE state IN ('acknowledged', 'failed') AND updated_at < ?1",
            [before],
        )?;
        n += self
            .db
            .execute("DELETE FROM receipts WHERE at < ?1", [before])?;
        Ok(n)
    }

    /// Unacknowledged messages per local agent, and queued outgoing ones.
    pub fn counts(&self) -> Result<(HashMap<String, usize>, usize)> {
        let mut stmt = self
            .db
            .prepare("SELECT agent, count(*) FROM inbox WHERE acked_at IS NULL GROUP BY agent")?;
        let open = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get::<_, i64>(1)? as usize)))?
            .collect::<rusqlite::Result<_>>()?;
        let queued: i64 = self.db.query_row(
            "SELECT count(*) FROM sent WHERE state = 'queued'",
            [],
            |r| r.get(0),
        )?;
        Ok((open, queued as usize))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::{Pipe, PipeStage};

    fn msg(id: &str, from: &str, to: &str, reply_to: Option<&str>) -> Message {
        Message {
            id: id.into(),
            from: from.into(),
            to: to.into(),
            body: format!("body of {id}"),
            reply_to: reply_to.map(Into::into),
            expects_reply: false,
            created_at: 1000,
            pipe: None,
            attended: false,
        }
    }

    fn ids(rows: &[Incoming]) -> Vec<&str> {
        rows.iter().map(|r| r.msg.id.as_str()).collect()
    }

    #[test]
    fn queues_need_a_subscribed_agent_and_drop_duplicates() {
        let s = Store::memory();
        let m = msg("a", "x@there", "sim@here", None);
        assert_eq!(s.store_incoming(&m, "o", 1).unwrap(), Stored::UnknownAgent);
        assert!(s.add_agent("sim", 1).unwrap());
        assert!(!s.add_agent("sim", 2).unwrap());
        assert_eq!(s.store_incoming(&m, "o", 1).unwrap(), Stored::New);
        assert_eq!(s.store_incoming(&m, "o", 2).unwrap(), Stored::Duplicate);
        assert_eq!(s.open_incoming("sim").unwrap().len(), 1);
    }

    #[test]
    fn pending_messages_go_back_out_when_their_session_ends() {
        let mut s = Store::memory();
        s.add_agent("sim", 1).unwrap();
        for id in ["a", "b", "c"] {
            s.store_incoming(&msg(id, "x@there", "sim@here", None), "o", 1)
                .unwrap();
        }
        let mut live = HashSet::from(["s1".to_string()]);
        let got = s.takeable("sim", "s1", &live, 2).unwrap();
        assert_eq!(ids(&got), ["a", "b"]);
        let first = s
            .mark_delivered(&["a".into(), "b".into()], "s1", 5)
            .unwrap();
        assert_eq!(first.len(), 2);

        // The same session doesn't get them twice; another live one doesn't
        // get them at all.
        assert_eq!(ids(&s.takeable("sim", "s1", &live, 10).unwrap()), ["c"]);
        live.insert("s2".into());
        assert_eq!(ids(&s.takeable("sim", "s2", &live, 10).unwrap()), ["c"]);

        // Once s1 is gone, its pending messages are free again, but they
        // were delivered already, so no second receipt.
        live.remove("s1");
        assert_eq!(
            ids(&s.takeable("sim", "s2", &live, 10).unwrap()),
            ["a", "b", "c"]
        );
        let first = s
            .mark_delivered(&["a".into(), "c".into()], "s2", 6)
            .unwrap();
        assert_eq!(
            first.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            ["c"]
        );

        assert_eq!(
            s.ack("sim", "a", 7).unwrap(),
            Acked::Now {
                sender: "x@there".into()
            }
        );
        assert_eq!(s.ack("sim", "a", 8).unwrap(), Acked::Already);
        assert_eq!(s.ack("other", "b", 8).unwrap(), Acked::NotFound);
        assert_eq!(ids(&s.takeable("sim", "s3", &live, 10).unwrap()), ["b"]);
    }

    #[test]
    fn sends_retry_and_receipts_only_move_forward() {
        let s = Store::memory();
        let m = msg("a", "api@here", "sim@there", None);
        s.record_sent(&m, SendState::Queued, 1).unwrap();
        assert_eq!(s.due_sends(1, 10).unwrap().len(), 1);
        s.retry_send("a", 500, "timed out").unwrap();
        assert!(s.due_sends(100, 10).unwrap().is_empty());
        assert_eq!(s.next_due().unwrap(), Some(500));
        let due = s.due_sends(500, 10).unwrap();
        assert_eq!(due[0].attempts, 1);
        s.retry_machine_now("there").unwrap();
        assert_eq!(s.next_due().unwrap(), Some(0));

        s.set_send_state("a", SendState::Stored, None, 2).unwrap();
        let delivered = Receipt {
            id: "a".into(),
            state: SendState::Delivered,
            at: 3,
        };
        // Only the recipient's machine can send a receipt.
        assert!(!s.apply_receipt("elsewhere", &delivered, 3).unwrap());
        assert!(s.apply_receipt("there", &delivered, 3).unwrap());
        assert!(!s.apply_receipt("there", &delivered, 3).unwrap());
        let acked = Receipt {
            state: SendState::Acknowledged,
            ..delivered.clone()
        };
        assert!(s.apply_receipt("there", &acked, 4).unwrap());
        assert!(!s.apply_receipt("there", &delivered, 5).unwrap());
        assert_eq!(
            s.outgoing("a").unwrap().unwrap().state,
            SendState::Acknowledged
        );
    }

    #[test]
    fn open_outgoing_lists_what_is_still_owed() {
        let s = Store::memory();
        let mut asks = msg("a", "api@here", "sim@there", None);
        asks.expects_reply = true;
        s.record_sent(&asks, SendState::Stored, 1).unwrap();
        s.record_sent(
            &msg("b", "api@here", "sim@there", None),
            SendState::Stored,
            1,
        )
        .unwrap();
        s.record_sent(
            &msg("c", "api@here", "sim@there", None),
            SendState::Queued,
            1,
        )
        .unwrap();
        let open = s.open_outgoing("api", 0).unwrap();
        let ids: Vec<_> = open.iter().map(|o| o.msg.id.as_str()).collect();
        assert_eq!(ids, ["a", "c"]);
        s.set_send_state("a", SendState::Acknowledged, None, 2)
            .unwrap();
        assert_eq!(s.open_outgoing("api", 0).unwrap().len(), 1);
    }

    #[test]
    fn threads_follow_replies_both_ways() {
        let s = Store::memory();
        s.add_agent("api", 1).unwrap();
        let mut a = msg("a", "api@here", "sim@there", None);
        let mut b = msg("b", "sim@there", "api@here", Some("a"));
        let mut c = msg("c", "api@here", "sim@there", Some("b"));
        let mut d = msg("d", "api@here", "sim@there", None);
        for (m, t) in [(&mut a, 1), (&mut b, 2), (&mut c, 3), (&mut d, 4)] {
            m.created_at = t;
        }
        s.record_sent(&a, SendState::Stored, 1).unwrap();
        s.store_incoming(&b, "o", 2).unwrap();
        s.record_sent(&c, SendState::Stored, 3).unwrap();
        s.record_sent(&d, SendState::Stored, 4).unwrap();
        let thread: Vec<_> = s
            .thread("b")
            .unwrap()
            .iter()
            .map(|e| e.msg().id.clone())
            .collect();
        assert_eq!(thread, ["a", "b", "c"]);
        assert!(s.thread("zzz").unwrap().is_empty());
    }

    #[test]
    fn only_requests_and_answers_to_requests_wake() {
        let s = Store::memory();
        s.add_agent("api", 1).unwrap();
        let mut ask = msg("q", "api@here", "sim@there", None);
        ask.expects_reply = true;
        s.record_sent(&ask, SendState::Stored, 1).unwrap();
        s.record_sent(
            &msg("n", "api@here", "sim@there", None),
            SendState::Stored,
            1,
        )
        .unwrap();
        // An answer to api's request wakes it; a reply to its notice doesn't.
        for m in [
            msg("a", "sim@there", "api@here", Some("q")),
            msg("b", "sim@there", "api@here", Some("n")),
        ] {
            s.store_incoming(&m, "o", 2).unwrap();
        }
        // A new request wakes it; a new notice doesn't.
        let mut req = msg("c", "sim@there", "api@here", None);
        req.expects_reply = true;
        s.store_incoming(&req, "o", 2).unwrap();
        s.store_incoming(&msg("d", "sim@there", "api@here", None), "o", 2)
            .unwrap();
        let wakes: Vec<_> = s
            .open_incoming("api")
            .unwrap()
            .into_iter()
            .map(|m| (m.msg.id, m.wakes))
            .collect();
        assert_eq!(
            wakes,
            [
                ("a".to_string(), true),
                ("b".to_string(), false),
                ("c".to_string(), true),
                ("d".to_string(), false)
            ]
        );
    }

    #[test]
    fn responses_are_handled_once_delivered() {
        let mut s = Store::memory();
        s.add_agent("api", 1).unwrap();
        let mut req = msg("a", "sim@there", "api@here", None);
        req.expects_reply = true;
        s.store_incoming(&req, "o", 1).unwrap();
        for m in [
            msg("b", "sim@there", "api@here", Some("x")),
            msg("c", "sim@there", "api@here", Some("y")),
        ] {
            s.store_incoming(&m, "o", 1).unwrap();
        }
        s.mark_delivered(&["a".into(), "b".into()], "s1", 2)
            .unwrap();
        // The request waits for an answer; the response not yet handed out
        // waits too.
        assert_eq!(
            s.ack_responses("api", 3).unwrap(),
            [("b".to_string(), "sim@there".to_string())]
        );
        assert_eq!(ids(&s.open_incoming("api").unwrap()), ["a", "c"]);
    }

    #[test]
    fn older_databases_gain_the_new_columns() {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch(
            "CREATE TABLE inbox (
                seq INTEGER PRIMARY KEY AUTOINCREMENT, id TEXT NOT NULL UNIQUE,
                agent TEXT NOT NULL, sender TEXT NOT NULL, recipient TEXT NOT NULL,
                owner TEXT NOT NULL, body TEXT NOT NULL, reply_to TEXT,
                expects_reply INTEGER NOT NULL, created_at INTEGER NOT NULL,
                stored_at INTEGER NOT NULL, delivered_at INTEGER, session TEXT,
                acked_at INTEGER);
             CREATE TABLE sent (
                id TEXT PRIMARY KEY, agent TEXT NOT NULL, sender TEXT NOT NULL,
                recipient TEXT NOT NULL, body TEXT NOT NULL, reply_to TEXT,
                expects_reply INTEGER NOT NULL, created_at INTEGER NOT NULL,
                state TEXT NOT NULL, error TEXT, attempts INTEGER NOT NULL DEFAULT 0,
                next_try INTEGER NOT NULL DEFAULT 0, updated_at INTEGER NOT NULL);
             INSERT INTO inbox (id, agent, sender, recipient, owner, body, expects_reply,
                                created_at, stored_at)
             VALUES ('a', 'api', 'x@t', 'api@h', 'o', 'hi', 0, 1, 1);
             INSERT INTO sent (id, agent, sender, recipient, body, expects_reply, created_at,
                               state, updated_at)
             VALUES ('b', 'api', 'api@h', 'x@t', 'hi', 0, 1, 'queued', 1);",
        )
        .unwrap();
        let s = Store::init(db).unwrap();
        let old = &s.open_incoming("api").unwrap()[0];
        assert!(old.wakes && old.msg.pipe.is_none() && !old.msg.attended);
        let due = s.due_sends(10, 10).unwrap();
        assert_eq!(due[0].item.id, "b");
        assert!(due[0].item.pipe.is_none() && !due[0].item.attended);
    }

    #[test]
    fn keeps_pipe_steps_and_attended_both_ways() {
        let s = Store::memory();
        s.add_agent("sim", 1).unwrap();
        let mut m = msg("a", "x@t", "sim@h", None);
        m.pipe = Some(Pipe {
            stage: PipeStage::Task,
            model: Some("sonnet".into()),
            effort: Some("high".into()),
        });
        m.attended = true;
        s.store_incoming(&m, "o", 1).unwrap();
        assert_eq!(s.open_incoming("sim").unwrap()[0].msg, m);
        s.record_sent(&m, SendState::Queued, 1).unwrap();
        assert_eq!(s.outgoing("a").unwrap().unwrap().msg, m);
        assert_eq!(s.due_sends(10, 10).unwrap()[0].item, m);
    }

    #[test]
    fn prunes_only_finished_messages() {
        let s = Store::memory();
        s.add_agent("sim", 1).unwrap();
        s.store_incoming(&msg("a", "x@t", "sim@h", None), "o", 1)
            .unwrap();
        s.store_incoming(&msg("b", "x@t", "sim@h", None), "o", 1)
            .unwrap();
        s.ack("sim", "a", 10).unwrap();
        assert_eq!(s.prune(5).unwrap(), 0);
        assert_eq!(s.prune(11).unwrap(), 1);
        assert_eq!(s.open_incoming("sim").unwrap().len(), 1);
    }
}
