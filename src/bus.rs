//! The bus itself: sessions, sending, delivery and receipts. The daemon's
//! local and tailnet listeners and its forwarder all work through this.

use std::collections::{HashMap, HashSet, VecDeque};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, RwLock, Weak};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::alert;
use crate::ask::{self, Asks, Prompt};
use crate::config::Config;
use crate::local::{Event, Inbox, Item, PeerMachine, Peers, Sent};
use crate::net;
use crate::proto::{
    AgentInfo, AgentList, Billing, MAX_BATCH, MAX_BODY, Message, MessageBatch, Pipe, Receipt,
    ReceiptBatch, SendState, StoreResult, StoreResults, StoreStatus,
};
use crate::store::{Acked, Incoming, Store, Stored, machine_part, name_part};
use crate::tailscale::{Status, Tailscale, Whois};
use crate::util::{Address, is_id, new_id, now_ms, valid_name};

const DAY_MS: i64 = 86_400_000;
const STATUS_TTL: Duration = Duration::from_secs(10);
const WHOIS_TTL: Duration = Duration::from_secs(60);
const MAX_EVENTS: usize = 1000;
/// How long the dialog about a paused session stays up.
const PAUSE_DIALOG: Duration = Duration::from_secs(86_400);

/// This machine as Tailscale names it.
#[derive(Debug, Clone)]
pub struct Me {
    pub machine: String,
    pub owner: String,
    pub ipv4: Option<Ipv4Addr>,
}

pub struct Bus {
    pub cfg: Config,
    /// This bus, for work that outlives a call.
    this: Weak<Bus>,
    ts: Tailscale,
    store: Mutex<Store>,
    live: Mutex<Live>,
    changed: Condvar,
    me: RwLock<Option<Me>>,
    status: Mutex<Option<(Instant, Status)>>,
    whois: Mutex<HashMap<IpAddr, (Instant, Whois)>>,
    /// Warnings the forwarder learned for messages `send` is waiting on.
    notes: Mutex<HashMap<String, String>>,
    /// The tailnet address the bus listens on, or why it isn't listening.
    pub listening: Mutex<Result<SocketAddr, String>>,
    /// Dialogs out to the user's Macs, and shown here for theirs.
    pub asks: Asks,
}

#[derive(Default)]
struct Live {
    /// Bumped on every change anyone might be waiting for.
    generation: u64,
    holders: HashMap<String, Holder>,
    waiters: HashMap<String, Waiter>,
    /// Open `recv --wait` calls per agent.
    receivers: HashMap<String, usize>,
    next_token: u64,
    events: VecDeque<Event>,
    event_seq: u64,
    online: HashMap<String, bool>,
    /// Messages being handed over right now, which nobody else may take.
    in_flight: HashSet<String>,
    /// Each agent's handoffs since its user last typed in its session.
    runs: HashMap<String, Run>,
    /// How each agent's current turn started.
    turns: HashMap<String, Turn>,
    /// Permission prompts the agents' hooks are holding, by ID.
    perms: HashMap<String, Perm>,
}

/// Handoffs to an agent's sessions in a row, without its user typing.
#[derive(Default)]
struct Run {
    handoffs: u32,
    /// Set at the limit, until the user types or lets it continue.
    paused: bool,
    /// The ask that says so, while it's out.
    ask: Option<String>,
}

#[derive(Default)]
struct Turn {
    /// The wait hook woke the session with a message from this agent, and
    /// the turn that starts hasn't begun yet.
    woke: Option<String>,
    /// The current turn was started by a message from this agent.
    by: Option<String>,
    /// The user is in the loop: they typed the prompt that started the
    /// turn, or answered a question carousel in it.
    attended: bool,
}

/// A permission prompt, until it's answered or withdrawn.
struct Perm {
    agent: String,
    tool: String,
    /// The tool's input, as JSON.
    input: String,
    /// Answered at the terminal, or the session moved on.
    done: bool,
}

/// How messages reached a session, for the handoff limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Handoff {
    /// By the wait hook, waking the session: counts toward the limit.
    Wake,
    /// By `msg_recv` or `msg_wait`: counts too.
    Tool,
    /// With a prompt the user typed, or to the CLI: doesn't.
    Free,
}

/// The session holding an agent name.
struct Holder {
    token: u64,
    session: String,
    app: Option<String>,
    client_pid: u32,
    /// The client it's a worker for, once the tag has been checked.
    client: Option<String>,
    billing: Option<Billing>,
    /// Why its client tag didn't hold.
    tag_refused: Option<String>,
}

/// What a session says about itself when it claims an agent name.
#[derive(Default)]
pub struct Claim<'a> {
    pub session: &'a str,
    pub app: Option<&'a str>,
    pub client_pid: u32,
    /// The client it's a worker for, from `TAILBUS_CLIENT`.
    pub client: Option<&'a str>,
    /// How it's billed, from `claude auth status`.
    pub billing: Option<&'a Billing>,
}

/// The push adapter waiting on an agent's behalf.
struct Waiter {
    token: u64,
    session: String,
    client_pid: u32,
}

pub enum Waited {
    Messages(Vec<Incoming>),
    Replaced,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn backoff(attempts: u32) -> i64 {
    (1000i64 << attempts.min(6)).min(60_000)
}

/// A one-line preview of a body, for the event log.
fn preview(body: &str) -> String {
    let line = body.lines().next().unwrap_or_default();
    let mut s: String = line.chars().take(60).collect();
    if s.len() < body.len() {
        s.push('…');
    }
    s
}

impl Bus {
    pub fn start(cfg: Config, store: Store) -> Arc<Bus> {
        Arc::new_cyclic(|this| Bus {
            this: this.clone(),
            ts: Tailscale::find(cfg.tailscale.as_deref()),
            cfg,
            store: Mutex::new(store),
            live: Mutex::new(Live::default()),
            changed: Condvar::new(),
            me: RwLock::new(None),
            status: Mutex::new(None),
            whois: Mutex::new(HashMap::new()),
            notes: Mutex::new(HashMap::new()),
            listening: Mutex::new(Err("starting".into())),
            asks: Asks::default(),
        })
    }

    /// This bus, for work that outlives a call.
    pub fn arc(&self) -> Option<Arc<Bus>> {
        self.this.upgrade()
    }

    pub fn db(&self) -> MutexGuard<'_, Store> {
        lock(&self.store)
    }

    // Tailscale.

    pub fn me(&self) -> Result<Me> {
        if let Some(me) = self.me.read().unwrap_or_else(|e| e.into_inner()).clone() {
            return Ok(me);
        }
        self.refresh_status()
            .context("Tailscale isn't ready, so this machine has no name on the bus yet")?;
        Ok(self
            .me
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .expect("set by refresh_status"))
    }

    /// Tailscale's view of the tailnet, at most a few seconds old.
    pub fn status(&self) -> Result<Status> {
        if let Some((at, status)) = &*lock(&self.status)
            && at.elapsed() < STATUS_TTL
        {
            return Ok(status.clone());
        }
        self.refresh_status()
    }

    pub fn refresh_status(&self) -> Result<Status> {
        let status = self.ts.status()?;
        *self.me.write().unwrap_or_else(|e| e.into_inner()) = Some(Me {
            machine: status.machine.clone(),
            owner: status.owner.clone(),
            ipv4: status.ipv4,
        });
        *lock(&self.status) = Some((Instant::now(), status.clone()));
        let mut back = Vec::new();
        {
            let mut live = lock(&self.live);
            for peer in &status.peers {
                let was = live.online.insert(peer.machine.clone(), peer.online);
                if peer.online && was == Some(false) {
                    back.push(peer.machine.clone());
                }
            }
        }
        for machine in back {
            if self.db().retry_machine_now(&machine)? > 0 {
                self.event(format!(
                    "{machine} is back online; sending what's waiting for it"
                ));
            }
        }
        Ok(status)
    }

    pub fn whois(&self, ip: IpAddr) -> Result<Whois> {
        if let Some((at, who)) = lock(&self.whois).get(&ip)
            && at.elapsed() < WHOIS_TTL
        {
            return Ok(who.clone());
        }
        let who = self.ts.whois(ip)?;
        lock(&self.whois).insert(ip, (Instant::now(), who.clone()));
        Ok(who)
    }

    /// Machines owned by this one's owner, plus whatever the config allows.
    pub fn allowed(&self, who: &Whois) -> Result<bool> {
        let me = self.me()?;
        let same_owner =
            !me.owner.is_empty() && who.owner == me.owner && who.owner != "tagged-devices";
        Ok(same_owner
            || self.cfg.allow.iter().any(|a| {
                a.eq_ignore_ascii_case(&who.owner) || a.eq_ignore_ascii_case(&who.machine)
            }))
    }

    // Waiting and events.

    pub fn notify(&self) {
        lock(&self.live).generation += 1;
        self.changed.notify_all();
    }

    /// Logs an event and keeps it for `tailbus tail`.
    pub fn event(&self, text: impl Into<String>) {
        let text = text.into();
        let at = now_ms();
        eprintln!("{} {text}", crate::util::rfc3339(at));
        let mut live = lock(&self.live);
        live.event_seq += 1;
        let seq = live.event_seq;
        live.events.push_back(Event { seq, at, text });
        if live.events.len() > MAX_EVENTS {
            live.events.pop_front();
        }
        live.generation += 1;
        drop(live);
        self.changed.notify_all();
    }

    pub fn events_after(&self, after: u64) -> Vec<Event> {
        let live = lock(&self.live);
        live.events
            .iter()
            .filter(|e| e.seq > after)
            .cloned()
            .collect()
    }

    /// Calls `check` after every change until it returns something, the
    /// deadline passes, or `gone` says the caller went away.
    pub fn wait_until<T>(
        &self,
        deadline: Instant,
        mut check: impl FnMut() -> Result<Option<T>>,
        mut gone: impl FnMut() -> bool,
    ) -> Result<Option<T>> {
        loop {
            let generation = lock(&self.live).generation;
            if let Some(found) = check()? {
                return Ok(Some(found));
            }
            let now = Instant::now();
            if now >= deadline || gone() {
                return Ok(None);
            }
            let live = lock(&self.live);
            if live.generation == generation {
                let wait = (deadline - now).min(Duration::from_secs(2));
                drop(self.changed.wait_timeout(live, wait));
            }
        }
    }

    /// Sleeps until something changes or `d` passes.
    pub fn nap(&self, d: Duration) {
        let live = lock(&self.live);
        drop(self.changed.wait_timeout(live, d));
    }

    // Sessions.

    /// Claims `agent` for a session, unless another live session holds it.
    /// The same session may claim it again. (Matching on the app's process
    /// is only for hooks: one Codex server can host many sessions.)
    pub fn hold(&self, agent: &str, claim: &Claim) -> Result<Result<u64, String>> {
        let Claim {
            session,
            app,
            client_pid,
            ..
        } = *claim;
        if !valid_name(agent) {
            bail!("{agent:?} isn't a valid agent name: use lowercase letters, digits and hyphens");
        }
        let (client, tag_refused) = self.worker_tag(agent, claim);
        let mut live = lock(&self.live);
        if let Some(h) = live.holders.get(agent)
            && h.session != session
        {
            return Ok(Err(format!(
                "another live session already holds agent {agent} ({}, process {}). \
                 Set TAILBUS_AGENT to give this session a name of its own.",
                h.app.as_deref().unwrap_or("unknown app"),
                h.client_pid
            )));
        }
        live.next_token += 1;
        let token = live.next_token;
        live.holders.insert(
            agent.to_string(),
            Holder {
                token,
                session: session.to_string(),
                app: app.map(str::to_string),
                client_pid,
                client: client.clone(),
                billing: claim.billing.cloned(),
                tag_refused: tag_refused.clone(),
            },
        );
        if live
            .waiters
            .get(agent)
            .is_some_and(|w| w.session != session && w.client_pid != client_pid)
        {
            live.waiters.remove(agent);
        }
        drop(live);
        self.notify();
        self.db().agent_live(agent, app, now_ms())?;
        let mut about = app.unwrap_or("unknown app").to_string();
        if let Some(billing) = claim.billing {
            about.push_str(&format!(", billed to {billing}"));
        }
        match (&client, &tag_refused) {
            (Some(client), _) => about.push_str(&format!(", {client}'s worker")),
            (None, Some(why)) => about.push_str(&format!("; not a worker: {why}")),
            (None, None) => {}
        }
        self.event(format!("{agent}: session started ({about})"));
        Ok(Ok(token))
    }

    /// The client a claiming session is a worker for, or why its tag
    /// doesn't hold. A worker must be billed to an API key, so a client's
    /// work never lands on a subscription.
    fn worker_tag(&self, agent: &str, claim: &Claim) -> (Option<String>, Option<String>) {
        let Some(client) = claim
            .client
            .map(str::to_string)
            .or_else(|| self.cfg.clients.get(agent).cloned())
        else {
            return (None, None);
        };
        if !valid_name(&client) {
            return (
                None,
                Some(format!(
                    "{client:?} isn't a valid client name: use lowercase letters, digits and hyphens"
                )),
            );
        }
        match claim.billing {
            Some(b) if b.api_key() => (Some(client), None),
            Some(b) => (
                None,
                Some(format!(
                    "tagged for {client}, but billed to {b}, not an API key"
                )),
            ),
            None => (
                None,
                Some(format!(
                    "tagged for {client}, but how the session is billed couldn't be checked"
                )),
            ),
        }
    }

    pub fn release(&self, agent: &str, token: u64) {
        let mut live = lock(&self.live);
        if live.holders.get(agent).is_some_and(|h| h.token == token) {
            live.holders.remove(agent);
            drop(live);
            self.notify();
            let _ = self.db().agent_live(agent, None, now_ms());
            self.event(format!("{agent}: session ended"));
        }
    }

    /// Registers the push adapter's wait for `agent`, replacing any other.
    /// Returns its token and the session messages are handed to.
    pub fn add_waiter(
        &self,
        agent: &str,
        session: &str,
        client_pid: u32,
    ) -> Result<Result<(u64, String), String>> {
        if !valid_name(agent) {
            bail!("{agent:?} isn't a valid agent name");
        }
        let mut live = lock(&self.live);
        let session = match live.holders.get(agent) {
            Some(h) if h.session == session || h.client_pid == client_pid => h.session.clone(),
            Some(h) => {
                return Ok(Err(format!(
                    "another live session holds agent {agent} (process {})",
                    h.client_pid
                )));
            }
            None => session.to_string(),
        };
        live.next_token += 1;
        let token = live.next_token;
        live.waiters.insert(
            agent.to_string(),
            Waiter {
                token,
                session: session.clone(),
                client_pid,
            },
        );
        drop(live);
        self.notify();
        self.db().agent_live(agent, None, now_ms())?;
        Ok(Ok((token, session)))
    }

    pub fn remove_waiter(&self, agent: &str, token: u64) {
        let mut live = lock(&self.live);
        if live.waiters.get(agent).is_some_and(|w| w.token == token) {
            live.waiters.remove(agent);
        }
    }

    /// Waits on a waiter's behalf for messages that may wake its session.
    /// The ones returned are reserved until `unreserve`.
    pub fn wait_as_waiter(
        &self,
        agent: &str,
        session: &str,
        token: u64,
        gone: impl FnMut() -> bool,
    ) -> Result<Option<Waited>> {
        let far = Instant::now() + Duration::from_secs(7 * 86_400);
        self.wait_until(
            far,
            || {
                let (current, receiving) = {
                    let live = lock(&self.live);
                    let current = live.waiters.get(agent).is_some_and(|w| w.token == token);
                    (current, live.receivers.contains_key(agent))
                };
                if !current {
                    return Ok(Some(Waited::Replaced));
                }
                // While the session waits in `msg_wait` itself, what arrives
                // is for that call, which would otherwise wait on for
                // nothing.
                if receiving || self.paused(agent).is_some() {
                    return Ok(None);
                }
                let found = self.take(agent, session, MAX_BATCH, true)?;
                Ok((!found.is_empty()).then_some(Waited::Messages(found)))
            },
            gone,
        )
    }

    /// Counts a `recv --wait` as the agent being live while it waits.
    pub fn receiving(&self, agent: &str, delta: isize) {
        let mut live = lock(&self.live);
        let n = live.receivers.entry(agent.to_string()).or_default();
        *n = n.saturating_add_signed(delta);
        if *n == 0 {
            live.receivers.remove(agent);
            drop(live);
            // The waiter may take messages again.
            self.notify();
        }
    }

    fn is_live(live: &Live, agent: &str) -> bool {
        live.holders.contains_key(agent)
            || live.waiters.contains_key(agent)
            || live.receivers.contains_key(agent)
    }

    /// Sessions whose pending messages aren't to be handed to anyone else.
    fn live_sessions(&self, agent: &str) -> HashSet<String> {
        let live = lock(&self.live);
        let holder = live.holders.get(agent).map(|h| h.session.clone());
        let waiter = live.waiters.get(agent).map(|w| w.session.clone());
        holder.into_iter().chain(waiter).collect()
    }

    // Receiving.

    /// Takes up to `max` messages for `session`, reserving them until
    /// `unreserve` so no other session or call hands out the same ones
    /// meanwhile. With `to_wake`, takes nothing unless one of them may wake
    /// a session; the others then come along with it.
    pub fn take(
        &self,
        agent: &str,
        session: &str,
        max: usize,
        to_wake: bool,
    ) -> Result<Vec<Incoming>> {
        let sessions = self.live_sessions(agent);
        let found = self.db().takeable(agent, session, &sessions, usize::MAX)?;
        let mut live = lock(&self.live);
        let found: Vec<Incoming> = found
            .into_iter()
            .filter(|m| !live.in_flight.contains(&m.msg.id))
            .take(max)
            .collect();
        if to_wake && !found.iter().any(|m| m.wakes) {
            return Ok(Vec::new());
        }
        live.in_flight
            .extend(found.iter().map(|m| m.msg.id.clone()));
        Ok(found)
    }

    /// Frees messages `take` reserved, whether or not they were delivered.
    pub fn unreserve(&self, found: &[Incoming]) {
        if found.is_empty() {
            return;
        }
        let mut live = lock(&self.live);
        for m in found {
            live.in_flight.remove(&m.msg.id);
        }
        drop(live);
        self.notify();
    }

    /// Records a confirmed hand-off, sends receipts for first deliveries,
    /// and counts it toward the handoff limit.
    pub fn deliver(
        &self,
        agent: &str,
        found: &[Incoming],
        session: &str,
        how: Handoff,
    ) -> Result<()> {
        let ids: Vec<String> = found.iter().map(|m| m.msg.id.clone()).collect();
        let first = self.db().mark_delivered(&ids, session, now_ms())?;
        for msg in first {
            self.receipt(&msg.from, &msg.id, SendState::Delivered)?;
            self.event(format!(
                "{} -> {agent}: delivered {}",
                msg.from,
                preview(&msg.body)
            ));
        }
        if how == Handoff::Wake {
            let from = found.iter().find(|m| m.wakes).map(|m| m.msg.from.clone());
            lock(&self.live)
                .turns
                .entry(agent.to_string())
                .or_default()
                .woke = from;
        }
        // A batch sent entirely with its senders' users in the loop isn't a
        // run of agents talking among themselves.
        if how != Handoff::Free
            && !found.iter().all(|m| m.msg.attended)
            && let Some(last) = found.last()
        {
            self.count_handoff(agent, &last.msg);
        }
        Ok(())
    }

    // The handoff limit.

    /// Why `agent` is being handed nothing, if it's paused.
    pub fn paused(&self, agent: &str) -> Option<String> {
        let live = lock(&self.live);
        let run = live.runs.get(agent)?;
        run.paused.then(|| {
            format!(
                "tailbus has paused your messages. Other agents' messages have reached you {} \
                 times in a row without your user typing in this session, so new ones wait \
                 until your user does, or lets them continue from the dialog on their Mac. \
                 Don't wait for replies now: finish what you're doing and tell your user \
                 where things stand.",
                run.handoffs
            )
        })
    }

    fn count_handoff(&self, agent: &str, latest: &Message) {
        let limit = self.cfg.handoff_limit;
        let mut live = lock(&self.live);
        let run = live.runs.entry(agent.to_string()).or_default();
        run.handoffs += 1;
        if limit == 0 || run.handoffs < limit || run.paused {
            return;
        }
        run.paused = true;
        drop(live);
        self.event(format!(
            "{agent}: paused after {limit} handoffs in a row from other agents without its user"
        ));
        self.show_pause(agent, latest, limit);
    }

    /// Tells the user, in a dialog on each of their Macs, that `agent` is
    /// paused, and lets it continue if they say so.
    fn show_pause(&self, agent: &str, latest: &Message, limit: u32) {
        let Some(bus) = self.arc() else {
            return;
        };
        let machine = self.me().map(|m| m.machine).unwrap_or_default();
        let prompt = Prompt {
            title: "tailbus: agents paused".into(),
            text: format!(
                "{agent}@{machine} has been handed messages from other agents {limit} times in \
                 a row without you typing in its session. The latest is from {}:\n\n{}\n\n\
                 New messages to it wait until you type in that session, or let them continue.",
                latest.from,
                alert::clean(&latest.body, 400)
            ),
            buttons: vec!["Keep waiting".into(), "Let them continue".into()],
        };
        let agent = agent.to_string();
        thread::spawn(move || {
            let id = match ask::ask(&bus, prompt) {
                Ok((id, 0)) => {
                    bus.event(format!("{agent}: no Mac could show the pause dialog"));
                    ask::close(&bus, &id);
                    return;
                }
                Ok((id, _)) => id,
                Err(e) => {
                    bus.event(format!("couldn't show the pause dialog: {e:#}"));
                    return;
                }
            };
            {
                let mut live = lock(&bus.live);
                match live.runs.get_mut(&agent) {
                    Some(run) if run.paused => run.ask = Some(id.clone()),
                    // Resumed while the dialogs were opening.
                    _ => {
                        drop(live);
                        ask::close(&bus, &id);
                        return;
                    }
                }
            }
            let got = bus.wait_until(
                Instant::now() + PAUSE_DIALOG,
                || {
                    if let Some(answer) = ask::answer(&bus, &id) {
                        return Ok(Some(Some(answer)));
                    }
                    let out = lock(&bus.live)
                        .runs
                        .get(&agent)
                        .is_some_and(|r| r.ask.as_deref() == Some(id.as_str()));
                    Ok((!out).then_some(None))
                },
                || false,
            );
            ask::close(&bus, &id);
            if let Ok(Some(Some(answer))) = got
                && answer.choice == 1
            {
                bus.resume(
                    &agent,
                    &format!("you let it continue, from {}", answer.machine),
                );
            }
        });
    }

    /// Starts `agent`'s count over, ending any pause.
    fn resume(&self, agent: &str, why: &str) {
        let mut live = lock(&self.live);
        let Some(run) = live.runs.remove(agent) else {
            return;
        };
        drop(live);
        if let Some(id) = &run.ask {
            ask::close(self, id);
        }
        if run.paused {
            self.event(format!("{agent}: resumed, as {why}"));
            self.notify();
        }
    }

    /// A turn is starting in a session of `agent`. Returns the session to
    /// hand messages to, or `None` if another session holds the agent.
    /// A prompt the user typed starts the handoff count over.
    pub fn turn(&self, agent: &str, session: &str, client_pid: u32, typed: bool) -> Option<String> {
        let session = {
            let live = lock(&self.live);
            match live.holders.get(agent) {
                Some(h) if h.session == session || h.client_pid == client_pid => h.session.clone(),
                Some(_) => return None,
                None => session.to_string(),
            }
        };
        if typed {
            self.resume(agent, "its user typed in its session");
        }
        {
            let mut live = lock(&self.live);
            let turn = live.turns.entry(agent.to_string()).or_default();
            turn.by = if typed { None } else { turn.woke.take() };
            turn.attended = typed;
            if typed {
                turn.woke = None;
            }
        }
        // A prompt the user typed means they're at the terminal, where any
        // open prompt is. A wake doesn't: a background subagent's prompt may
        // still be waiting.
        if typed {
            self.perms_done(agent);
        }
        Some(session)
    }

    /// A turn of `agent`'s ended: the responses and notices it was handed
    /// are handled now.
    pub fn turn_ended(&self, agent: &str) -> Result<()> {
        // Open prompts stay open: background subagents outlive the turn.
        if let Some(turn) = lock(&self.live).turns.get_mut(agent) {
            turn.by = None;
            turn.attended = false;
        }
        let acked = self.db().ack_responses(agent, now_ms())?;
        for (id, sender) in acked {
            self.receipt(&sender, &id, SendState::Acknowledged)?;
        }
        Ok(())
    }

    /// The agent whose message started `agent`'s current turn, if one did.
    pub fn woken_by(&self, agent: &str) -> Option<String> {
        lock(&self.live).turns.get(agent).and_then(|t| t.by.clone())
    }

    // Permission prompts.

    /// Notes a permission prompt a hook is holding. Returns its ID.
    pub fn perm_open(&self, agent: &str, tool: &str, input: &str) -> String {
        let id = new_id();
        lock(&self.live).perms.insert(
            id.clone(),
            Perm {
                agent: agent.to_string(),
                tool: tool.to_string(),
                input: input.to_string(),
                done: false,
            },
        );
        id
    }

    /// Whether the prompt was answered at the terminal, or its session
    /// moved on.
    pub fn perm_done(&self, id: &str) -> bool {
        lock(&self.live).perms.get(id).is_none_or(|p| p.done)
    }

    pub fn perm_close(&self, id: &str) {
        lock(&self.live).perms.remove(id);
    }

    /// A tool call ran, so a prompt for it was answered at the terminal.
    /// Matched on the tool and its exact input: subagents run calls in
    /// parallel, so anything looser could settle someone else's prompt.
    pub fn tool_done(&self, agent: &str, tool: &str, input: &str) {
        let mut live = lock(&self.live);
        // Only the user answers a question carousel.
        if tool == "AskUserQuestion" {
            live.turns.entry(agent.to_string()).or_default().attended = true;
        }
        let mut any = false;
        for p in live
            .perms
            .values_mut()
            .filter(|p| p.agent == agent && p.tool == tool && p.input == input && !p.done)
        {
            p.done = true;
            any = true;
        }
        drop(live);
        if any {
            self.notify();
        }
    }

    fn perms_done(&self, agent: &str) {
        let mut live = lock(&self.live);
        let mut any = false;
        for p in live.perms.values_mut().filter(|p| p.agent == agent) {
            any |= !p.done;
            p.done = true;
        }
        drop(live);
        if any {
            self.notify();
        }
    }

    pub fn ack(&self, agent: &str, id: &str) -> Result<()> {
        let acked = self.db().ack(agent, id, now_ms())?;
        match acked {
            Acked::Now { sender } => {
                self.receipt(&sender, id, SendState::Acknowledged)?;
                self.event(format!("{agent}: acknowledged {id} from {sender}"));
                Ok(())
            }
            Acked::Already => Ok(()),
            Acked::NotFound => bail!("{agent} has no message {id}; msg_inbox lists yours"),
        }
    }

    fn receipt(&self, sender: &str, id: &str, state: SendState) -> Result<()> {
        let me = self.me()?;
        let machine = machine_part(sender);
        let receipt = Receipt {
            id: id.to_string(),
            state,
            at: now_ms(),
        };
        if machine == me.machine {
            self.db().apply_receipt(machine, &receipt, now_ms())?;
        } else {
            self.db().queue_receipt(machine, &receipt)?;
        }
        self.notify();
        Ok(())
    }

    // Sending.

    pub fn send(
        &self,
        from: &str,
        to: &str,
        body: String,
        reply_to: Option<String>,
        expects_reply: Option<bool>,
        pipe: Option<Pipe>,
    ) -> Result<Sent> {
        // A new message is a request; a reply is a response unless it asks
        // something back.
        let expects_reply = expects_reply.unwrap_or(reply_to.is_none());
        let me = self.me()?;
        if !valid_name(from) {
            bail!("{from:?} isn't a valid agent name: use lowercase letters, digits and hyphens");
        }
        if body.trim().is_empty() {
            bail!("the message is empty");
        }
        if body.len() > MAX_BODY {
            bail!(
                "the message is {} KiB; the limit is {} KiB",
                body.len().div_ceil(1024),
                MAX_BODY / 1024
            );
        }
        if let Some(r) = &reply_to
            && !is_id(r)
        {
            bail!("{r:?} isn't a message ID");
        }
        // A reply may leave out who it's to: the sender of what it answers.
        let replying_to = match (to.trim(), &reply_to) {
            ("", Some(r)) => match self.db().incoming(r)? {
                Some(m) if name_part(&m.msg.to) == from => m.msg.from,
                _ => bail!("{from} has no message {r} to reply to"),
            },
            ("", None) => bail!("the message needs a recipient"),
            (to, _) => to.to_string(),
        };
        if let Some(p) = &pipe {
            check_pipe(p)?;
        }
        let to = Address::parse(&replying_to, &me.machine)?;
        let now = now_ms();
        // Sending subscribes the sender, so replies have somewhere to go, and
        // counts as it being active.
        self.db().agent_live(from, None, now)?;
        let msg = Message {
            id: new_id(),
            from: format!("{from}@{}", me.machine),
            to: to.to_string(),
            body,
            reply_to,
            expects_reply,
            created_at: now,
            pipe,
            // The bus says so, not the agent.
            attended: lock(&self.live).turns.get(from).is_some_and(|t| t.attended),
        };

        let (state, warning) = if to.machine == me.machine {
            let stored = self.db().store_incoming(&msg, &me.owner, now)?;
            if stored == Stored::UnknownAgent {
                bail!(
                    "there's no agent {} on {}. peer_list shows the agents you can reach.",
                    to.agent,
                    me.machine
                );
            }
            self.db().record_sent(&msg, SendState::Stored, now)?;
            self.notify();
            let warning = self
                .agent_list()?
                .agents
                .into_iter()
                .find(|a| a.name == to.agent)
                .and_then(|a| staleness(&to.to_string(), a.live, a.last_live, now));
            (SendState::Stored, warning)
        } else {
            let status = self.status()?;
            let Some(peer) = status.peers.iter().find(|p| p.machine == to.machine) else {
                bail!(
                    "there's no machine named {} on this tailnet. peer_list shows the agents you can reach.",
                    to.machine
                );
            };
            let online = peer.online;
            self.db().record_sent(&msg, SendState::Queued, now)?;
            self.notify();
            // Give the forwarder a moment, so the answer can say whether it
            // got there.
            let settled = if online {
                self.wait_until(
                    Instant::now() + Duration::from_secs(5),
                    || {
                        let sent = self.db().outgoing(&msg.id)?;
                        Ok(sent.filter(|s| s.state != SendState::Queued))
                    },
                    || false,
                )?
            } else {
                None
            };
            match settled {
                Some(s) if s.state == SendState::Failed => {
                    bail!("{}", s.error.unwrap_or_else(|| "not delivered".into()))
                }
                Some(s) => (s.state, lock(&self.notes).remove(&msg.id)),
                None if !online => (
                    SendState::Queued,
                    Some(format!(
                        "{} is offline; the message goes as soon as it's back",
                        to.machine
                    )),
                ),
                None => (
                    SendState::Queued,
                    Some(format!(
                        "{} hasn't taken it yet; it's queued and will keep being retried",
                        to.machine
                    )),
                ),
            }
        };
        // A reply acknowledges the message it answers.
        if let Some(r) = &msg.reply_to {
            let acked = self.db().ack(from, r, now)?;
            if let Acked::Now { sender } = acked {
                self.receipt(&sender, r, SendState::Acknowledged)?;
            }
        }
        self.event(format!(
            "{} -> {}: {} ({})",
            msg.from,
            msg.to,
            preview(&msg.body),
            state.as_str()
        ));
        Ok(Sent {
            kind: msg.kind().into(),
            id: msg.id,
            to: msg.to,
            state: state.as_str().into(),
            warning,
        })
    }

    /// Sends what's due in the outbox, and receipts owed to other machines.
    pub fn forward_once(&self, http: &ureq::Agent) -> Result<()> {
        let now = now_ms();
        let port = self.cfg.port;
        let due = self.db().due_sends(now, 10 * MAX_BATCH)?;
        if !due.is_empty() {
            let status = self.status()?;
            let mut by_machine: HashMap<String, Vec<_>> = HashMap::new();
            for d in due {
                by_machine
                    .entry(machine_part(&d.item.to).to_string())
                    .or_default()
                    .push(d);
            }
            for (machine, items) in by_machine {
                let peer = status.peers.iter().find(|p| p.machine == machine);
                let ip = match peer {
                    None => {
                        for d in &items {
                            let why = format!("there's no machine named {machine} on this tailnet");
                            self.db().set_send_state(
                                &d.item.id,
                                SendState::Failed,
                                Some(&why),
                                now,
                            )?;
                        }
                        continue;
                    }
                    Some(p) => match p.ipv4 {
                        Some(ip) if p.online => ip,
                        _ => {
                            for d in &items {
                                let next = now + backoff(d.attempts);
                                self.db().retry_send(
                                    &d.item.id,
                                    next,
                                    &format!("{machine} is offline"),
                                )?;
                            }
                            continue;
                        }
                    },
                };
                for chunk in items.chunks(MAX_BATCH) {
                    let batch = MessageBatch {
                        messages: chunk.iter().map(|d| d.item.clone()).collect(),
                    };
                    match net::post::<_, StoreResults>(http, ip, port, "/v1/messages", &batch) {
                        Ok(res) => self.apply_results(chunk, res.results, now)?,
                        Err(e) => {
                            let why = format!("{machine}: {e:#}");
                            for d in chunk {
                                self.db().retry_send(
                                    &d.item.id,
                                    now + backoff(d.attempts),
                                    &why,
                                )?;
                            }
                            if chunk.iter().all(|d| d.attempts == 0) {
                                self.event(format!("couldn't reach {why}; will retry"));
                            }
                        }
                    }
                }
            }
            self.notify();
        }

        let due = self.db().due_receipts(now, 10 * MAX_BATCH)?;
        if !due.is_empty() {
            let status = self.status()?;
            let mut by_machine: HashMap<String, Vec<_>> = HashMap::new();
            for d in due {
                by_machine
                    .entry(d.item.machine.clone())
                    .or_default()
                    .push(d);
            }
            for (machine, items) in by_machine {
                let peer = status.peers.iter().find(|p| p.machine == machine);
                let ip = match peer {
                    None => {
                        for d in &items {
                            self.db().drop_receipt(d.item.seq)?;
                        }
                        continue;
                    }
                    Some(p) => match p.ipv4 {
                        Some(ip) if p.online => ip,
                        _ => {
                            for d in &items {
                                self.db()
                                    .retry_receipt(d.item.seq, now + backoff(d.attempts))?;
                            }
                            continue;
                        }
                    },
                };
                for chunk in items.chunks(MAX_BATCH) {
                    let batch = ReceiptBatch {
                        receipts: chunk.iter().map(|d| d.item.receipt.clone()).collect(),
                    };
                    let sent =
                        net::post::<_, serde_json::Value>(http, ip, port, "/v1/receipts", &batch);
                    for d in chunk {
                        match &sent {
                            Ok(_) => self.db().drop_receipt(d.item.seq)?,
                            Err(_) => self
                                .db()
                                .retry_receipt(d.item.seq, now + backoff(d.attempts))?,
                        }
                    }
                }
            }
        }
        Ok(())
    }

    fn apply_results(
        &self,
        sent: &[crate::store::Due<Message>],
        results: Vec<StoreResult>,
        now: i64,
    ) -> Result<()> {
        let by_id: HashMap<&str, &StoreResult> =
            results.iter().map(|r| (r.id.as_str(), r)).collect();
        for d in sent {
            let msg = &d.item;
            match by_id.get(msg.id.as_str()) {
                Some(r) if r.status == StoreStatus::Stored => {
                    self.db()
                        .set_send_state(&msg.id, SendState::Stored, None, now)?;
                    if let Some(w) = staleness(&msg.to, r.live.unwrap_or(true), r.last_live, now) {
                        lock(&self.notes).insert(msg.id.clone(), w);
                    }
                }
                Some(r) => {
                    let why = r.error.clone().unwrap_or_else(|| format!("{:?}", r.status));
                    self.db()
                        .set_send_state(&msg.id, SendState::Failed, Some(&why), now)?;
                    self.event(format!("{} -> {}: failed: {why}", msg.from, msg.to));
                }
                None => {
                    self.db()
                        .retry_send(&msg.id, now + backoff(d.attempts), "no answer for it")?;
                }
            }
        }
        Ok(())
    }

    // Arriving from other machines.

    pub fn accept(&self, who: &Whois, batch: MessageBatch) -> Result<StoreResults> {
        let me = self.me()?;
        let now = now_ms();
        let agents = self.agent_list()?.agents;
        let mut results = Vec::new();
        let mut arrived = Vec::new();
        for mut msg in batch.messages.into_iter().take(MAX_BATCH) {
            let mut result = StoreResult {
                id: msg.id.clone(),
                status: StoreStatus::Rejected,
                error: None,
                live: None,
                last_live: None,
            };
            let to = Address::parse(&msg.to, &me.machine);
            let from = name_part(&msg.from).to_string();
            let problem = match &to {
                _ if !is_id(&msg.id) => Some("bad message ID".to_string()),
                Err(e) => Some(format!("{e:#}")),
                Ok(to) if to.machine != me.machine => {
                    Some(format!("this is {}, not {}", me.machine, to.machine))
                }
                _ if !valid_name(&from) => Some(format!("{from:?} isn't a valid agent name")),
                _ if msg.body.len() > MAX_BODY => Some("the message is over 64 KiB".into()),
                _ if msg.reply_to.as_deref().is_some_and(|r| !is_id(r)) => {
                    Some("bad reply_to ID".into())
                }
                _ if msg.pipe.as_ref().is_some_and(|p| check_pipe(p).is_err()) => {
                    Some("bad /pipe model or effort".into())
                }
                _ => None,
            };
            if let Some(problem) = problem {
                result.error = Some(problem);
                results.push(result);
                continue;
            }
            let to = to.expect("checked above");
            // The sending machine is whoever Tailscale says it is, and its
            // word that its user was in the loop counts only if it's the
            // same user's.
            msg.from = format!("{from}@{}", who.machine);
            msg.to = to.to_string();
            msg.attended &= !me.owner.is_empty() && who.owner == me.owner;
            let stored = self.db().store_incoming(&msg, &who.owner, now)?;
            match stored {
                Stored::UnknownAgent => {
                    result.status = StoreStatus::UnknownAgent;
                    result.error = Some(format!(
                        "there's no agent {} on {}. peer_list shows the agents you can reach.",
                        to.agent, me.machine
                    ));
                }
                Stored::New | Stored::Duplicate => {
                    result.status = StoreStatus::Stored;
                    if let Some(a) = agents.iter().find(|a| a.name == to.agent) {
                        result.live = Some(a.live);
                        result.last_live = a.last_live;
                    }
                    if stored == Stored::New {
                        arrived.push(msg);
                    }
                }
            }
            results.push(result);
        }
        if !arrived.is_empty() {
            self.notify();
            for msg in arrived {
                self.event(format!(
                    "{} -> {}: stored {}",
                    msg.from,
                    msg.to,
                    preview(&msg.body)
                ));
            }
        }
        Ok(StoreResults { results })
    }

    pub fn accept_receipts(&self, who: &Whois, batch: ReceiptBatch) -> Result<()> {
        let now = now_ms();
        for receipt in batch.receipts.iter().take(MAX_BATCH) {
            let applied = self.db().apply_receipt(&who.machine, receipt, now)?;
            if applied {
                self.event(format!(
                    "{}: {} {}",
                    who.machine,
                    receipt.state.as_str(),
                    receipt.id
                ));
            }
        }
        self.notify();
        Ok(())
    }

    // Listings.

    pub fn agent_list(&self) -> Result<AgentList> {
        let me = self.me()?;
        let rows = self.db().agents()?;
        let live = lock(&self.live);
        let now = now_ms();
        Ok(AgentList {
            machine: me.machine,
            agents: rows
                .into_iter()
                .map(|r| {
                    let is_live = Bus::is_live(&live, &r.name);
                    let holder = live.holders.get(&r.name);
                    let app = holder.and_then(|h| h.app.clone()).or(r.app);
                    AgentInfo {
                        live: is_live,
                        app,
                        last_live: if is_live { Some(now) } else { r.last_live },
                        client: holder.and_then(|h| h.client.clone()),
                        billing: holder.and_then(|h| h.billing.clone()),
                        tag_refused: holder.and_then(|h| h.tag_refused.clone()),
                        name: r.name,
                    }
                })
                .collect(),
        })
    }

    pub fn inbox(&self, agent: &str) -> Result<Inbox> {
        let me = self.me()?;
        let db = self.db();
        Ok(Inbox {
            machine: me.machine,
            incoming: db
                .open_incoming(agent)?
                .into_iter()
                .map(Item::from)
                .collect(),
            outgoing: db
                .open_outgoing(agent, now_ms() - DAY_MS)?
                .into_iter()
                .map(Item::from)
                .collect(),
        })
    }

    pub fn thread(&self, id: &str) -> Result<Vec<Item>> {
        let entries = self.db().thread(id)?;
        if entries.is_empty() {
            bail!("this machine has no message {id}");
        }
        Ok(entries
            .into_iter()
            .map(|e| match e {
                crate::store::ThreadEntry::In(m) => Item::from(m),
                crate::store::ThreadEntry::Out(m) => Item::from(m),
            })
            .collect())
    }

    /// Agents on this machine and on every online peer whose bus answers.
    /// Where a `/pipe` task goes: a client name finds that client's live
    /// worker, here or on a peer, and an address must have a live session.
    /// Refuses otherwise, saying who is live.
    pub fn pipe_target(&self, to: &str) -> Result<String> {
        let peers = self.peers()?;
        let lists = std::iter::once(&peers.here)
            .chain(peers.machines.iter().filter_map(|m| m.agents.as_ref()));
        let live: Vec<(String, Option<String>)> = lists
            .flat_map(|l| {
                l.agents
                    .iter()
                    .filter(|a| a.live)
                    .map(|a| (format!("{}@{}", a.name, l.machine), a.client.clone()))
            })
            .collect();
        let me = self.me()?;
        let found: Vec<&String> = if to.contains('@') {
            let addr = Address::parse(to, &me.machine)?.to_string();
            live.iter()
                .map(|(a, _)| a)
                .filter(|a| **a == addr)
                .collect()
        } else {
            let workers: Vec<&String> = live
                .iter()
                .filter(|(_, c)| c.as_deref() == Some(to))
                .map(|(a, _)| a)
                .collect();
            if workers.is_empty() {
                let here = format!("{to}@{}", me.machine);
                live.iter()
                    .map(|(a, _)| a)
                    .filter(|a| **a == here)
                    .collect()
            } else {
                workers
            }
        };
        match found.as_slice() {
            [one] => Ok(one.to_string()),
            [] => {
                let now: Vec<String> = live
                    .iter()
                    .map(|(a, c)| match c {
                        Some(c) => format!("{a} ({c}'s worker)"),
                        None => a.clone(),
                    })
                    .collect();
                bail!(
                    "no live session matches {to}, so the task wasn't sent. Live now: {}.",
                    if now.is_empty() {
                        "nobody".to_string()
                    } else {
                        now.join(", ")
                    }
                )
            }
            many => bail!(
                "{to} has more than one live worker: {}. Name one of them.",
                many.iter()
                    .map(|a| a.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }

    pub fn peers(&self) -> Result<Peers> {
        let here = self.agent_list()?;
        let status = self.status()?;
        let port = self.cfg.port;
        let asks: Vec<_> = status
            .peers
            .iter()
            .filter(|p| p.online)
            .filter_map(|p| p.ipv4.map(|ip| (p.machine.clone(), ip)))
            .map(|(machine, ip)| thread::spawn(move || (machine, net::get_agents(ip, port))))
            .collect();
        let mut machines: Vec<PeerMachine> = asks
            .into_iter()
            .filter_map(|h| h.join().ok())
            .map(|(machine, got)| match got {
                Ok(list) => PeerMachine {
                    machine,
                    online: true,
                    agents: Some(list),
                    error: None,
                },
                Err(e) => PeerMachine {
                    machine,
                    online: true,
                    agents: None,
                    error: Some(format!("{e:#}")),
                },
            })
            .collect();
        machines.extend(
            status
                .peers
                .iter()
                .filter(|p| !p.online)
                .map(|p| PeerMachine {
                    machine: p.machine.clone(),
                    online: false,
                    agents: None,
                    error: None,
                }),
        );
        machines.sort_by(|a, b| a.machine.cmp(&b.machine));
        Ok(Peers { here, machines })
    }

    pub fn prune(&self) {
        let before = now_ms() - i64::from(self.cfg.retention_days) * DAY_MS;
        match self.db().prune(before) {
            Ok(0) => {}
            Ok(n) => self.event(format!("pruned {n} finished messages")),
            Err(e) => self.event(format!("pruning failed: {e:#}")),
        }
    }
}

/// The models and efforts a pipe task may ask for, as Claude Code names
/// them. A model is an alias such as `opus`, or a full model ID.
fn check_pipe(p: &Pipe) -> Result<()> {
    if let Some(e) = &p.effort
        && !matches!(e.as_str(), "low" | "medium" | "high" | "xhigh" | "max")
    {
        bail!("{e:?} isn't an effort level: use low, medium, high, xhigh or max");
    }
    if let Some(m) = &p.model
        && (m.is_empty()
            || m.len() > 64
            || !m
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-._[]".contains(&b)))
    {
        bail!("{m:?} isn't a model name: use an alias such as opus or sonnet, or a model ID");
    }
    Ok(())
}

/// A warning for a message to an agent that hasn't had a session lately,
/// which usually means a misaddress.
fn staleness(addr: &str, live: bool, last_live: Option<i64>, now: i64) -> Option<String> {
    if live {
        return None;
    }
    match last_live {
        None => Some(format!(
            "{addr} hasn't had a session yet. It's queued for when one starts; check the address if that's unexpected."
        )),
        Some(t) if now - t > 7 * DAY_MS => Some(format!(
            "{addr} hasn't had a session in {} days, so check the address.",
            (now - t) / DAY_MS
        )),
        _ => None,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn backs_off_to_a_minute() {
        assert_eq!(backoff(0), 1000);
        assert_eq!(backoff(3), 8000);
        assert_eq!(backoff(40), 60_000);
    }

    #[test]
    fn warns_about_agents_without_recent_sessions() {
        let now = 100 * DAY_MS;
        assert!(staleness("a@b", true, None, now).is_none());
        assert!(staleness("a@b", false, Some(now - DAY_MS), now).is_none());
        assert!(
            staleness("a@b", false, Some(now - 9 * DAY_MS), now)
                .unwrap()
                .contains("9 days")
        );
        assert!(staleness("a@b", false, None, now).is_some());
    }

    #[test]
    fn previews_the_first_line() {
        assert_eq!(preview("hello"), "hello");
        assert_eq!(preview("hello\nworld"), "hello…");
    }

    /// A bus on machine `here` with no Tailscale, no dialogs, and `api`
    /// subscribed.
    pub fn bus() -> Arc<Bus> {
        let mut cfg: Config = toml::from_str("alerts = false").unwrap();
        cfg.handoff_limit = 3;
        let bus = Bus::start(cfg, Store::memory());
        *bus.me.write().unwrap() = Some(Me {
            machine: "here".into(),
            owner: "o".into(),
            ipv4: None,
        });
        bus.db().add_agent("api", 1).unwrap();
        bus
    }

    fn arrive(bus: &Bus, id: &str, expects_reply: bool) {
        let msg = Message {
            id: id.into(),
            from: "sim@here".into(),
            to: "api@here".into(),
            body: format!("body of {id}"),
            reply_to: None,
            expects_reply,
            created_at: 1,
            pipe: None,
            attended: false,
        };
        bus.db().store_incoming(&msg, "o", 1).unwrap();
    }

    fn ids(found: &[Incoming]) -> Vec<&str> {
        found.iter().map(|m| m.msg.id.as_str()).collect()
    }

    #[test]
    fn notices_ride_along_with_a_message_that_wakes() {
        let bus = bus();
        arrive(&bus, "n", false);
        assert!(bus.take("api", "s1", 10, true).unwrap().is_empty());
        arrive(&bus, "r", true);
        let found = bus.take("api", "s1", 10, true).unwrap();
        assert_eq!(ids(&found), ["n", "r"]);
    }

    #[test]
    fn the_waiter_leaves_messages_to_an_open_msg_wait() {
        let bus = bus();
        let (token, session) = bus.add_waiter("api", "s1", 1).unwrap().unwrap();
        arrive(&bus, "r", true);
        bus.receiving("api", 1);
        assert!(
            bus.wait_as_waiter("api", &session, token, || true)
                .unwrap()
                .is_none()
        );
        bus.receiving("api", -1);
        let got = bus.wait_as_waiter("api", &session, token, || true).unwrap();
        assert!(matches!(got, Some(Waited::Messages(m)) if m.len() == 1));
    }

    #[test]
    fn reserved_messages_go_to_one_taker_at_a_time() {
        let bus = bus();
        arrive(&bus, "r", true);
        let first = bus.take("api", "s1", 10, false).unwrap();
        assert_eq!(ids(&first), ["r"]);
        assert!(bus.take("api", "s1", 10, false).unwrap().is_empty());
        bus.unreserve(&first);
        assert_eq!(ids(&bus.take("api", "s1", 10, false).unwrap()), ["r"]);
    }

    #[test]
    fn pauses_at_the_limit_until_the_user_types() {
        let bus = bus();
        for i in 0..3 {
            assert!(bus.paused("api").is_none());
            let id = format!("r{i}");
            arrive(&bus, &id, true);
            let found = bus.take("api", "s1", 10, false).unwrap();
            bus.deliver("api", &found, "s1", Handoff::Tool).unwrap();
            bus.unreserve(&found);
        }
        assert!(bus.paused("api").unwrap().contains("3 times in a row"));

        // Handoffs to the CLI, or with a typed prompt, don't count; a wake
        // that starts a turn doesn't reset the count.
        bus.turn("api", "s1", 1, false).unwrap();
        assert!(bus.paused("api").is_some());
        bus.turn("api", "s1", 1, true).unwrap();
        assert!(bus.paused("api").is_none());
        arrive(&bus, "c", true);
        let found = bus.take("api", "cli", 10, false).unwrap();
        bus.deliver("api", &found, "cli", Handoff::Free).unwrap();
        assert!(!lock(&bus.live).runs.contains_key("api"));
    }

    #[test]
    fn remembers_which_message_started_a_turn() {
        let bus = bus();
        arrive(&bus, "r", true);
        let found = bus.take("api", "s1", 10, true).unwrap();
        bus.deliver("api", &found, "s1", Handoff::Wake).unwrap();
        // Not until the turn the wake starts.
        assert_eq!(bus.woken_by("api"), None);
        bus.turn("api", "s1", 1, false).unwrap();
        assert_eq!(bus.woken_by("api").as_deref(), Some("sim@here"));
        bus.turn_ended("api").unwrap();
        assert_eq!(bus.woken_by("api"), None);
        // A prompt the user types starts a turn of their own.
        arrive(&bus, "r2", true);
        let found = bus.take("api", "s1", 10, true).unwrap();
        bus.deliver("api", &found, "s1", Handoff::Wake).unwrap();
        bus.turn("api", "s1", 1, true).unwrap();
        assert_eq!(bus.woken_by("api"), None);
    }

    #[test]
    fn prompts_settle_when_their_tool_runs_or_the_session_moves_on() {
        let bus = bus();
        let a = bus.perm_open("api", "Bash", r#"{"command":"ls"}"#);
        let b = bus.perm_open("api", "Bash", r#"{"command":"pwd"}"#);
        let c = bus.perm_open("api", "Write", r#"{"file_path":"/x"}"#);
        bus.tool_done("api", "Bash", r#"{"command":"ls"}"#);
        assert!(bus.perm_done(&a) && !bus.perm_done(&b) && !bus.perm_done(&c));
        // Another call to the same tool, as a subagent running in parallel
        // makes, doesn't settle it.
        bus.tool_done("api", "Write", r#"{"file_path":"/y"}"#);
        assert!(!bus.perm_done(&c));
        bus.tool_done("other", "Bash", r#"{"command":"pwd"}"#);
        assert!(!bus.perm_done(&b));
        // Neither does a wake, or the end of the turn, which background
        // subagents outlive.
        bus.turn("api", "s1", 1, false).unwrap();
        bus.turn_ended("api").unwrap();
        assert!(!bus.perm_done(&b) && !bus.perm_done(&c));
        // A prompt the user types does: they're at the terminal.
        bus.turn("api", "s1", 1, true).unwrap();
        assert!(bus.perm_done(&b) && bus.perm_done(&c));
        bus.perm_close(&b);
        assert!(bus.perm_done(&b), "a closed prompt counts as settled");
    }

    #[test]
    fn messages_sent_with_the_user_in_the_loop_dont_count() {
        let bus = bus();
        bus.db().add_agent("sim", 1).unwrap();
        // api's user typed the prompt, so what api sends is attended, and
        // handing it to sim doesn't count.
        bus.turn("api", "s1", 1, true).unwrap();
        let task = bus
            .send("api", "sim", "do this".into(), None, None, None)
            .unwrap();
        let found = bus.take("sim", "s2", 10, true).unwrap();
        assert!(found[0].msg.attended);
        bus.deliver("sim", &found, "s2", Handoff::Wake).unwrap();
        bus.unreserve(&found);
        assert!(!lock(&bus.live).runs.contains_key("sim"));
        // sim was woken, not typed to, so its answer isn't, and counts.
        bus.turn("sim", "s2", 2, false).unwrap();
        bus.send(
            "sim",
            "api",
            "a question".into(),
            Some(task.id),
            Some(true),
            None,
        )
        .unwrap();
        let found = bus.take("api", "s1", 10, true).unwrap();
        assert!(!found[0].msg.attended);
        bus.deliver("api", &found, "s1", Handoff::Wake).unwrap();
        bus.unreserve(&found);
        assert_eq!(lock(&bus.live).runs["api"].handoffs, 1);
        // In a woken turn, the user answering a carousel puts them in the
        // loop, until the turn ends.
        bus.turn("api", "s1", 1, false).unwrap();
        bus.tool_done("api", "AskUserQuestion", "{}");
        let answers = bus
            .send("api", "sim", "answers".into(), None, None, None)
            .unwrap();
        assert!(
            bus.db()
                .outgoing(&answers.id)
                .unwrap()
                .unwrap()
                .msg
                .attended
        );
        bus.turn_ended("api").unwrap();
        let later = bus
            .send("api", "sim", "later".into(), None, None, None)
            .unwrap();
        assert!(!bus.db().outgoing(&later.id).unwrap().unwrap().msg.attended);
    }

    #[test]
    fn only_the_owners_own_machines_can_say_their_user_was_there() {
        let bus = bus();
        let from = |owner: &str, id: &str| {
            let msg = Message {
                id: id.into(),
                from: "x@there".into(),
                to: "api@here".into(),
                body: "hi".into(),
                reply_to: None,
                expects_reply: true,
                created_at: 1,
                pipe: None,
                attended: true,
            };
            let who = Whois {
                machine: "there".into(),
                owner: owner.into(),
            };
            bus.accept(
                &who,
                MessageBatch {
                    messages: vec![msg],
                },
            )
            .unwrap();
        };
        from("o", "01K0000000000000000000000A");
        from("someone-else", "01K0000000000000000000000B");
        let open = bus.db().open_incoming("api").unwrap();
        let attended: Vec<_> = open.iter().map(|m| m.msg.attended).collect();
        assert_eq!(attended, [true, false]);
    }

    #[test]
    fn a_worker_must_be_billed_to_an_api_key() {
        let mut cfg: Config = toml::from_str("alerts = false").unwrap();
        cfg.clients.insert("work".into(), "acme".into());
        let bus = Bus::start(cfg, Store::memory());
        let billed = |method: &str| Billing {
            method: method.into(),
            plan: None,
        };
        let tag = |agent: &str, client: Option<&str>, billing: Option<&Billing>| {
            bus.worker_tag(
                agent,
                &Claim {
                    client,
                    billing,
                    ..Claim::default()
                },
            )
        };
        let api = billed("api_key");
        assert_eq!(
            tag("api", Some("acme"), Some(&api)),
            (Some("acme".into()), None)
        );
        // The config tags `work` without TAILBUS_CLIENT.
        assert_eq!(
            tag("work", None, Some(&billed("third_party"))).0.as_deref(),
            Some("acme")
        );
        let (client, why) = tag("api", Some("acme"), Some(&billed("claude.ai")));
        assert!(client.is_none() && why.unwrap().contains("not an API key"));
        let (client, why) = tag("api", Some("acme"), None);
        assert!(client.is_none() && why.unwrap().contains("couldn't be checked"));
        let (client, why) = tag("api", Some("Acme Corp"), Some(&api));
        assert!(client.is_none() && why.unwrap().contains("isn't a valid client name"));
        assert_eq!(tag("api", None, Some(&api)), (None, None));
    }

    #[test]
    fn pipe_tasks_find_a_live_worker_by_client_or_address() {
        let bus = bus();
        *bus.status.lock().unwrap() = Some((
            Instant::now(),
            Status {
                machine: "here".into(),
                ipv4: None,
                owner: "o".into(),
                peers: Vec::new(),
            },
        ));
        let api_key = Billing {
            method: "api_key".into(),
            plan: None,
        };
        bus.db().add_agent("work", 1).unwrap();
        bus.hold(
            "work",
            &Claim {
                session: "w",
                client: Some("acme"),
                billing: Some(&api_key),
                ..Claim::default()
            },
        )
        .unwrap()
        .unwrap();
        assert_eq!(bus.pipe_target("acme").unwrap(), "work@here");
        assert_eq!(bus.pipe_target("work@here").unwrap(), "work@here");
        assert_eq!(bus.pipe_target("work").unwrap(), "work@here");
        // api has no live session, and nobody works for globex.
        let refused = bus.pipe_target("api@here").unwrap_err().to_string();
        assert!(
            refused.contains("Live now: work@here (acme's worker)"),
            "{refused}"
        );
        assert!(bus.pipe_target("globex").is_err());
    }

    #[test]
    fn pipe_settings_are_checked() {
        let pipe = |model: Option<&str>, effort: Option<&str>| Pipe {
            stage: crate::proto::PipeStage::Task,
            model: model.map(Into::into),
            effort: effort.map(Into::into),
        };
        assert!(check_pipe(&pipe(Some("claude-opus-5-5"), Some("xhigh"))).is_ok());
        assert!(check_pipe(&pipe(None, None)).is_ok());
        assert!(check_pipe(&pipe(None, Some("turbo"))).is_err());
        assert!(check_pipe(&pipe(Some("opus\" onload=\""), None)).is_err());
    }

    #[test]
    fn a_turn_from_another_session_is_ignored() {
        let bus = bus();
        let claim = Claim {
            session: "s1",
            client_pid: 100,
            ..Claim::default()
        };
        bus.hold("api", &claim).unwrap().unwrap();
        assert_eq!(bus.turn("api", "s2", 200, true), None);
        // After /clear, the hook's session changes but its app is the same.
        assert_eq!(bus.turn("api", "s2", 100, true).as_deref(), Some("s1"));
    }

    #[test]
    fn responses_are_handled_when_the_turn_ends() {
        let bus = bus();
        arrive(&bus, "r", true);
        arrive(&bus, "n", false);
        let found = bus.take("api", "s1", 10, false).unwrap();
        bus.deliver("api", &found, "s1", Handoff::Tool).unwrap();
        bus.turn_ended("api").unwrap();
        let open: Vec<_> = bus.inbox("api").unwrap().incoming;
        assert_eq!(
            open.iter().map(|i| i.msg.id.as_str()).collect::<Vec<_>>(),
            ["r"]
        );
    }

    #[test]
    fn new_messages_are_requests_and_replies_are_responses() {
        let bus = bus();
        bus.db().add_agent("sim", 1).unwrap();
        let ask = bus
            .send("api", "sim", "6 times 7?".into(), None, None, None)
            .unwrap();
        assert_eq!(ask.kind, "request");
        let answer = bus
            .send("sim", "api", "42".into(), Some(ask.id.clone()), None, None)
            .unwrap();
        assert_eq!(answer.kind, "response");
        // The answer to api's request wakes it.
        let found = bus.take("api", "s1", 10, true).unwrap();
        assert_eq!(ids(&found), [answer.id.as_str()]);
        bus.unreserve(&found);
        // A thank-you for the answer doesn't wake sim.
        bus.send("api", "sim", "thanks".into(), Some(answer.id), None, None)
            .unwrap();
        let sim = bus.take("sim", "s2", 10, true).unwrap();
        assert!(sim.is_empty(), "{:?}", ids(&sim));
    }
}
