//! Doors: how an agent that runs in another OS account reaches the bus.
//!
//! A door is a Unix socket of its own, for one agent name and one account.
//! Only that account's processes get through it, and a connection through it
//! is that agent and no other. All it can do is take its own messages and
//! answer the requests among them, each once, to whoever sent it: it can't
//! start a conversation, message another agent, hand off work, or run
//! anything. Only agents on this machine can send to it.
//!
//! The daemon's main socket belongs to its own user, and a door agent's name
//! can't be used there, so nothing but its door acts as it.

use std::ffi::CString;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;

use anyhow::{Context, Result, bail};

use crate::bus::Bus;
use crate::config::Config;
use crate::local::{Item, Request};
use crate::store::{Incoming, machine_part, name_part};
use crate::util::Address;

/// The session a door's connections take and hold their agent under. Fixed,
/// so a door can't name another session and take over the names it held,
/// and so its handoffs don't count toward the limit: a door agent only ever
/// answers, so it can't start a loop.
pub const DOOR_SESSION: &str = "door-";

/// What a door refuses with, for anything it doesn't do.
const ONLY: &str = "takes, acknowledges and answers its own messages, and nothing else";

/// A door, with its account's uid.
#[derive(Debug, Clone)]
pub struct Door {
    pub name: String,
    pub account: String,
    pub uid: u32,
    pub socket: PathBuf,
}

/// The session name for `door`'s agent.
pub fn session(door: &str) -> String {
    format!("{DOOR_SESSION}{door}")
}

/// Whether `agent` is a door's.
pub fn is_door(cfg: &Config, agent: &str) -> bool {
    cfg.doors.contains_key(agent)
}

/// `account`'s uid, if there's such a user.
pub fn uid_of(account: &str) -> Option<u32> {
    let name = CString::new(account).ok()?;
    // SAFETY: getpwnam takes a NUL-terminated name and returns a pointer to
    // static storage, or null; uid is read before anything else calls it.
    let pw = unsafe { libc::getpwnam(name.as_ptr()) };
    if pw.is_null() {
        None
    } else {
        Some(unsafe { (*pw).pw_uid })
    }
}

/// The doors `cfg` names, each with its account's uid, or why it can't open.
pub fn configured(cfg: &Config) -> Vec<Result<Door, String>> {
    let mut names: Vec<&String> = cfg.doors.keys().collect();
    names.sort();
    names
        .into_iter()
        .map(|name| {
            let d = &cfg.doors[name];
            if !crate::util::valid_name(name) {
                return Err(format!("door {name:?}: not a valid agent name"));
            }
            let uid = uid_of(&d.account)
                .ok_or_else(|| format!("door {name}: there's no account {}", d.account))?;
            if uid == 0 {
                return Err(format!("door {name}: root can't have a door"));
            }
            Ok(Door {
                name: name.clone(),
                account: d.account.clone(),
                uid,
                socket: d.socket.clone(),
            })
        })
        .collect()
}

/// The door's folder must be the daemon user's and writable by nobody else,
/// so nobody can put a socket of their own in its place. It isn't made here:
/// the setup makes it, with the account let in to reach the socket.
pub fn check_folder(socket: &Path, me: u32) -> Result<()> {
    if socket.as_os_str().len() >= 104 {
        bail!("{} is too long for a Unix socket", socket.display());
    }
    let dir = socket
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .with_context(|| format!("{} has no folder", socket.display()))?;
    let meta = fs::symlink_metadata(dir)
        .with_context(|| format!("{} isn't there: the setup makes it", dir.display()))?;
    if !meta.is_dir() {
        bail!("{} isn't a folder", dir.display());
    }
    if meta.uid() != me {
        bail!(
            "{} belongs to uid {}, not the daemon's user ({me})",
            dir.display(),
            meta.uid()
        );
    }
    if meta.mode() & 0o022 != 0 {
        bail!(
            "{} is writable by others (mode {:o}): it must be writable only by the daemon's user",
            dir.display(),
            meta.mode() & 0o7777
        );
    }
    Ok(())
}

/// Opens `door` on `bus`: its socket, and its agent, so requests can be sent
/// to it. Its connections are served on threads of their own.
pub fn open(bus: &Arc<Bus>, door: Door) -> Result<()> {
    let me = unsafe { libc::geteuid() };
    check_folder(&door.socket, me)?;
    // Only a socket is ever replaced: anything else there is someone's.
    if let Ok(meta) = fs::symlink_metadata(&door.socket) {
        if !std::os::unix::fs::FileTypeExt::is_socket(&meta.file_type()) {
            bail!("{} is there and isn't a socket", door.socket.display());
        }
        fs::remove_file(&door.socket)?;
    }
    let listener = UnixListener::bind(&door.socket)
        .with_context(|| format!("listening on {}", door.socket.display()))?;
    // Anyone who can reach the folder may connect; the peer's uid is what
    // lets them in.
    fs::set_permissions(&door.socket, fs::Permissions::from_mode(0o666))?;
    bus.db().add_agent(&door.name, crate::util::now_ms())?;
    bus.event(format!(
        "{}: door open at {}, for {}",
        door.name,
        door.socket.display(),
        door.account
    ));
    let bus = bus.clone();
    thread::spawn(move || serve(&bus, &door, listener));
    Ok(())
}

/// Serves `door`'s connections, turning away every process but its account's.
pub fn serve(bus: &Arc<Bus>, door: &Door, listener: UnixListener) {
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        match crate::daemon::peer_uid(&stream) {
            Ok(uid) if uid == door.uid => {}
            Ok(uid) => {
                bus.event(format!(
                    "{}: door turned away uid {uid}; only {} gets through",
                    door.name, door.account
                ));
                continue;
            }
            Err(_) => continue,
        }
        let (bus, name) = (bus.clone(), door.name.clone());
        thread::spawn(move || {
            let _ = crate::daemon::serve_door(&bus, stream, &name);
        });
    }
}

/// What `door` may ask, as the daemon will carry it out, or why not. Fails
/// closed: anything not named here is refused.
pub fn check(bus: &Bus, door: &str, req: Request) -> Result<Request> {
    let mine = |agent: &str| -> Result<()> {
        if agent != door {
            bail!("this door is {door}'s: it can't act as {agent}");
        }
        Ok(())
    };
    Ok(match req {
        Request::Hello => Request::Hello,
        Request::Hold {
            agent,
            app,
            client_pid,
            ..
        } => {
            mine(&agent)?;
            Request::Hold {
                agent,
                session: session(door),
                app,
                client_pid,
                // Never a client's worker: nothing can be piped to it.
                client: None,
                billing: None,
            }
        }
        Request::Recv {
            agent,
            max,
            wait_secs,
            ..
        } => {
            mine(&agent)?;
            Request::Recv {
                agent,
                session: session(door),
                max,
                wait_secs,
            }
        }
        Request::Ack { agent, id } => {
            mine(&agent)?;
            Request::Ack { agent, id }
        }
        Request::Inbox { agent } => {
            mine(&agent)?;
            Request::Inbox { agent }
        }
        // Answered by `thread`, with only its own part of the thread.
        Request::Thread { id } => Request::Thread { id },
        Request::Send {
            agent,
            to,
            body,
            reply_to,
            expects_reply,
            pipe,
            ext,
        } => {
            mine(&agent)?;
            let Some(id) = reply_to else {
                bail!("{door} can only answer requests: a new message needs reply_to");
            };
            if pipe.is_some() {
                bail!("{door} can't send a /pipe step");
            }
            if ext.is_some() {
                bail!("{door}'s answers are text only");
            }
            if expects_reply == Some(true) {
                bail!("{door}'s answers can't ask anything back");
            }
            let request = open_request(bus, door, &id)?;
            let sender = request.msg.from.clone();
            if !to.trim().is_empty() {
                let here = bus.me()?.machine;
                let to = Address::parse(&to, &here)?;
                if to != Address::parse(&sender, &here)? {
                    bail!("{door} answers {id} to its sender, {sender}, not {to}");
                }
            }
            Request::Send {
                agent,
                to: sender,
                body,
                reply_to: Some(id),
                expects_reply: Some(false),
                pipe: None,
                ext: None,
            }
        }
        // Confirm only means something inside a Recv, where the daemon reads
        // it itself: on its own it's refused like everywhere else.
        Request::Confirm => Request::Confirm,
        other => bail!("{door}'s door {ONLY}; it can't {}", what(&other)),
    })
}

/// The request with ID `id` that `door` may answer: to it, from an agent on
/// this machine, asking for an answer, and not answered yet.
fn open_request(bus: &Bus, door: &str, id: &str) -> Result<Incoming> {
    let here = bus.me()?.machine;
    let found = bus.db().incoming(id)?;
    let Some(m) = found else {
        bail!("{door} has no request {id} to answer");
    };
    if name_part(&m.msg.to) != door || machine_part(&m.msg.to) != here {
        bail!("{id} isn't a message to {door}");
    }
    if !m.msg.expects_reply || m.msg.is_progress() {
        bail!("{id} isn't a request: {door} only answers requests");
    }
    if m.msg.pipe.is_some() {
        bail!("{id} is a /pipe step, which {door} can't answer");
    }
    if machine_part(&m.msg.from) != here {
        bail!("{id} came from another machine, which {door} can't answer");
    }
    if m.acked_at.is_some() {
        bail!("{id} is already answered or handled: {door} answers each request once");
    }
    Ok(m)
}

/// The messages in `id`'s thread that are to or from `door`: its own part
/// of the conversation, and nothing between other agents.
pub fn thread(bus: &Bus, door: &str, id: &str) -> Result<Vec<Item>> {
    let here = bus.me()?.machine;
    let ours = |addr: &str| name_part(addr) == door && machine_part(addr) == here;
    let entries: Vec<Item> = bus
        .thread(id)?
        .into_iter()
        .filter(|i| ours(&i.msg.to) || ours(&i.msg.from))
        .collect();
    if entries.is_empty() {
        bail!("{door} has no message {id}");
    }
    Ok(entries)
}

/// On the daemon's main socket: a door agent's name is used only through its
/// door, so nothing else takes its messages or speaks as it. Looking at its
/// inbox, and sending to it, are fine.
pub fn check_main(bus: &Bus, req: &Request) -> Result<()> {
    let acting_as = match req {
        Request::Hold { agent, .. }
        | Request::Recv { agent, .. }
        | Request::Wait { agent, .. }
        | Request::Turn { agent, .. }
        | Request::Send { agent, .. }
        | Request::Call { agent, .. }
        | Request::Progress { agent, .. }
        | Request::Cancel { agent, .. }
        | Request::Await { agent, .. }
        | Request::Ack { agent, .. } => Some(agent.as_str()),
        _ => None,
    };
    if let Some(agent) = acting_as.filter(|a| is_door(&bus.cfg, a)) {
        bail!("{agent} is a door agent: only its door acts as it");
    }
    let renamed = match req {
        Request::AgentRemove { name } => vec![name.as_str()],
        Request::AgentMove { from, to } => vec![from.as_str(), to.as_str()],
        _ => Vec::new(),
    };
    if let Some(name) = renamed.into_iter().find(|n| is_door(&bus.cfg, n)) {
        bail!("{name} is a door agent, set in config: change it there");
    }
    Ok(())
}

/// What a refused request was, in words.
fn what(req: &Request) -> &'static str {
    match req {
        Request::Wait { .. } | Request::Turn { .. } => "wait like an agent session",
        Request::Call { .. } | Request::Await { .. } => "make a blocking call",
        Request::Progress { .. } => "send progress notes",
        Request::Cancel { .. } => "cancel runs",
        Request::Permission { .. } | Request::ToolDone { .. } | Request::Subagent { .. } => {
            "take part in permission prompts"
        }
        Request::AgentAdd { .. } | Request::AgentRemove { .. } | Request::AgentMove { .. } => {
            "change agents"
        }
        Request::Agents | Request::Peers | Request::Status => "list agents or machines",
        Request::Tail { .. } => "read the bus's events",
        _ => "do that",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::Claim;
    use crate::local::{self, Client, Messages, Sent};
    use crate::network::Whois;
    use crate::proto::{Message, MessageBatch, Pipe, PipeStage, StoreStatus};
    use crate::util::new_id;
    use serde_json::Value;
    use std::env;

    /// A bus on `here` with door agent `oss`, and the door opened in a
    /// folder of this test's own, letting in uid `uid`.
    struct Rig {
        bus: Arc<Bus>,
        dir: PathBuf,
        socket: PathBuf,
    }

    impl Drop for Rig {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    fn me() -> u32 {
        unsafe { libc::geteuid() }
    }

    fn rig(uid: u32) -> Rig {
        let dir = env::temp_dir().join(format!("tb-door-{}", &new_id()[16..]));
        fs::create_dir_all(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        let dir = dir.canonicalize().unwrap();
        let socket = dir.join("door.sock");
        let extra = format!(
            "[doors.oss]\naccount = \"nobody-here\"\nsocket = \"{}\"",
            socket.display()
        );
        let bus = crate::bus::tests::bus_on("here", 1, &extra);
        let door = Door {
            name: "oss".into(),
            account: "nobody-here".into(),
            uid,
            socket: socket.clone(),
        };
        open(&bus, door).unwrap();
        Rig { bus, dir, socket }
    }

    impl Rig {
        fn client(&self) -> Client {
            Client::connect(&self.socket).unwrap()
        }

        fn call(&self, req: Value) -> Result<Value> {
            let mut c = self.client();
            let req: Request = serde_json::from_value(req).unwrap();
            c.call(&req)
        }

        /// A request from `api`, a local agent, to the door's agent.
        fn ask(&self, body: &str) -> String {
            self.bus
                .send("api", "oss", body.into(), None, None, None)
                .unwrap()
                .id
        }

        fn reply(&self, id: &str, body: &str) -> Result<Value> {
            self.call(serde_json::json!({
                "op": "send", "agent": "oss", "to": "", "body": body, "reply_to": id
            }))
        }
    }

    fn refused(r: Result<Value>, says: &str) {
        let e = format!("{:#}", r.expect_err("should be refused"));
        assert!(e.contains(says), "{e}");
    }

    #[test]
    fn only_the_doors_account_gets_through() {
        let r = rig(me() + 1);
        // Turned away before a word: the connection is closed under it.
        let e = format!(
            "{:#}",
            r.call(serde_json::json!({ "op": "hello" })).unwrap_err()
        );
        // Under load the close can reach the client as a reset, or another
        // socket error, instead.
        assert!(
            e.contains("closed")
                || e.contains("Broken pipe")
                || e.contains("reset")
                || e.contains("os error"),
            "{e}"
        );
        // Its own account's processes do.
        let ok = rig(me());
        ok.call(serde_json::json!({ "op": "hello" })).unwrap();
    }

    #[test]
    fn a_door_is_its_own_agent_and_no_other() {
        let r = rig(me());
        for req in [
            serde_json::json!({ "op": "recv", "agent": "api", "session": "s", "max": 5, "wait_secs": 0 }),
            serde_json::json!({ "op": "inbox", "agent": "api" }),
            serde_json::json!({ "op": "ack", "agent": "api", "id": new_id() }),
            serde_json::json!({ "op": "send", "agent": "api", "to": "oss", "body": "hi" }),
        ] {
            refused(r.call(req), "it can't act as api");
        }
        r.call(serde_json::json!({ "op": "inbox", "agent": "oss" }))
            .unwrap();
    }

    #[test]
    fn a_door_holds_under_its_own_session_and_takes_no_other_names() {
        let r = rig(me());
        // A name some session held before, and doesn't now.
        let claim = Claim {
            session: "someone-else",
            ..Claim::default()
        };
        let token = r.bus.hold("api", &claim).unwrap().unwrap();
        r.bus.release("api", token);
        let mut c = r.client();
        let held: local::Held = c
            .call(&Request::Hold {
                agent: "oss".into(),
                session: "someone-else".into(),
                app: Some("pi".into()),
                client_pid: std::process::id(),
                client: Some("acme".into()),
                billing: None,
            })
            .unwrap();
        assert!(held.held);
        let names: Vec<String> = r
            .bus
            .agent_list()
            .unwrap()
            .agents
            .into_iter()
            .map(|a| a.name)
            .collect();
        assert!(
            names.contains(&"api".to_string()),
            "api wasn't moved: {names:?}"
        );
        // Held as the door's session, and never a client's worker.
        let oss = r
            .bus
            .agent_list()
            .unwrap()
            .agents
            .into_iter()
            .find(|a| a.name == "oss")
            .unwrap();
        assert!(oss.live && oss.client.is_none());
        let other = r.call(serde_json::json!({
            "op": "hold", "agent": "api", "session": "x", "app": null, "client_pid": 1
        }));
        refused(other, "it can't act as api");
    }

    #[test]
    fn it_takes_its_requests_and_answers_each_once_to_its_sender() {
        let r = rig(me());
        let id = r.ask("what's new?");
        let mut c = r.client();
        let got = local::recv(
            &mut c,
            &Request::Recv {
                agent: "oss".into(),
                session: "anything".into(),
                max: 10,
                wait_secs: 0,
            },
        )
        .unwrap();
        assert_eq!(got.messages.len(), 1);
        assert_eq!(got.messages[0].msg.id, id);
        let sent: Sent = serde_json::from_value(r.reply(&id, "this").unwrap()).unwrap();
        assert_eq!(
            (sent.to.as_str(), sent.kind.as_str()),
            ("api@here", "response")
        );
        let answers = r.bus.db().open_incoming("api").unwrap();
        assert_eq!(answers.len(), 1);
        assert_eq!(answers[0].msg.reply_to.as_deref(), Some(id.as_str()));
        assert!(!answers[0].msg.expects_reply);
        refused(r.reply(&id, "and again"), "answers each request once");
        // Named explicitly, the sender is fine too; anyone else isn't.
        let id2 = r.ask("two?");
        refused(
            r.call(serde_json::json!({
                "op": "send", "agent": "oss", "to": "other@here", "body": "x", "reply_to": id2
            })),
            "to its sender",
        );
        r.call(serde_json::json!({
            "op": "send", "agent": "oss", "to": "api@here", "body": "x", "reply_to": id2
        }))
        .unwrap();
    }

    #[test]
    fn it_answers_only_open_requests_to_it() {
        let r = rig(me());
        refused(r.reply(&new_id(), "x"), "has no request");
        // A notice, which asks nothing back.
        let notice = r
            .bus
            .send("api", "oss", "fyi".into(), None, Some(false), None)
            .unwrap()
            .id;
        refused(r.reply(&notice, "x"), "isn't a request");
        // A request to someone else.
        r.bus.db().add_agent("other", 1).unwrap();
        let theirs = r
            .bus
            .send("api", "other", "q".into(), None, None, None)
            .unwrap()
            .id;
        refused(r.reply(&theirs, "x"), "isn't a message to oss");
        // A request it handled already.
        let handled = r.ask("q");
        r.call(serde_json::json!({ "op": "ack", "agent": "oss", "id": handled }))
            .unwrap();
        refused(r.reply(&handled, "x"), "already answered");
    }

    #[test]
    fn it_starts_nothing_and_asks_nothing() {
        let r = rig(me());
        let id = r.ask("q");
        refused(
            r.call(
                serde_json::json!({ "op": "send", "agent": "oss", "to": "api", "body": "hello" }),
            ),
            "needs reply_to",
        );
        refused(
            r.call(serde_json::json!({
                "op": "send", "agent": "oss", "to": "", "body": "?", "reply_to": id, "expects_reply": true
            })),
            "can't ask anything back",
        );
        let pipe = serde_json::to_value(Pipe::new(PipeStage::Result)).unwrap();
        refused(
            r.call(serde_json::json!({
                "op": "send", "agent": "oss", "to": "", "body": "x", "reply_to": id, "pipe": pipe
            })),
            "/pipe",
        );
        refused(
            r.call(serde_json::json!({
                "op": "send", "agent": "oss", "to": "", "body": "x", "reply_to": id,
                "ext": { "progress": true }
            })),
            "text only",
        );
        for (req, says) in [
            (
                serde_json::json!({ "op": "call", "agent": "oss", "to": "api", "body": "x" }),
                "blocking call",
            ),
            (
                serde_json::json!({ "op": "progress", "agent": "oss", "reply_to": id, "body": "x" }),
                "progress",
            ),
            (
                serde_json::json!({ "op": "cancel", "agent": "oss", "run_id": new_id() }),
                "cancel",
            ),
            (
                serde_json::json!({ "op": "wait", "agent": "oss", "session": "s", "client_pid": 1 }),
                "wait",
            ),
            (serde_json::json!({ "op": "agents" }), "list agents"),
            (serde_json::json!({ "op": "peers" }), "list agents"),
            (serde_json::json!({ "op": "status" }), "list agents"),
            (
                serde_json::json!({ "op": "agent_add", "name": "x" }),
                "change agents",
            ),
            (
                serde_json::json!({ "op": "agent_remove", "name": "api" }),
                "change agents",
            ),
            (serde_json::json!({ "op": "tail", "after": 0 }), "events"),
            (serde_json::json!({ "op": "confirm" }), "nothing to confirm"),
        ] {
            refused(r.call(req), says);
        }
        // Still open: none of that answered it.
        r.reply(&id, "the answer").unwrap();
    }

    #[test]
    fn its_thread_shows_only_its_own_conversation() {
        let r = rig(me());
        r.bus.db().add_agent("other", 1).unwrap();
        // api asked other something, then asks oss about it, in reply.
        let side = r
            .bus
            .send("api", "other", "between us".into(), None, None, None)
            .unwrap()
            .id;
        let id = r
            .bus
            .send(
                "api",
                "oss",
                "q".into(),
                Some(side.clone()),
                Some(true),
                None,
            )
            .unwrap()
            .id;
        let all = r.bus.thread(&id).unwrap();
        assert!(all.iter().any(|i| i.msg.id == side));
        let t: local::Thread = serde_json::from_value(
            r.call(serde_json::json!({ "op": "thread", "id": id }))
                .unwrap(),
        )
        .unwrap();
        let ids: Vec<&str> = t.entries.iter().map(|i| i.msg.id.as_str()).collect();
        assert_eq!(ids, [id.as_str()]);
        // From the other end too: still only its own part.
        let t: local::Thread = serde_json::from_value(
            r.call(serde_json::json!({ "op": "thread", "id": side }))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(t.entries.len(), 1);
        assert_eq!(t.entries[0].msg.id, id);
        // A thread it has no part in isn't there for it.
        let elsewhere = r
            .bus
            .send("api", "other", "unrelated".into(), None, None, None)
            .unwrap()
            .id;
        refused(
            r.call(serde_json::json!({ "op": "thread", "id": elsewhere })),
            "has no message",
        );
    }

    #[test]
    fn other_machines_cant_send_to_a_door_agent() {
        let r = rig(me());
        let msg = Message {
            id: new_id(),
            from: "api@there".into(),
            to: "oss@here".into(),
            body: "from afar".into(),
            reply_to: None,
            expects_reply: true,
            created_at: 1,
            pipe: None,
            attended: false,
            ext: None,
        };
        let who = Whois {
            machine: "there".into(),
            owner: "o".into(),
        };
        let results = r
            .bus
            .accept(
                &who,
                MessageBatch {
                    messages: vec![msg],
                },
            )
            .unwrap();
        assert_eq!(results.results[0].status, StoreStatus::Rejected);
        let why = results.results[0].error.clone().unwrap();
        assert!(why.contains("only from agents on here"), "{why}");
        assert!(r.bus.db().open_incoming("oss").unwrap().is_empty());
        let got: Messages = serde_json::from_value(
            r.call(serde_json::json!({
                "op": "recv", "agent": "oss", "session": "s", "max": 5, "wait_secs": 0
            }))
            .unwrap(),
        )
        .unwrap();
        assert!(got.messages.is_empty());
    }

    #[test]
    fn the_main_socket_cant_act_as_a_door_agent() {
        let r = rig(me());
        let bus = &r.bus;
        let id = r.ask("q");
        for req in [
            Request::Hold {
                agent: "oss".into(),
                session: "s".into(),
                app: None,
                client_pid: 1,
                client: None,
                billing: None,
            },
            Request::Recv {
                agent: "oss".into(),
                session: "cli".into(),
                max: 1,
                wait_secs: 0,
            },
            Request::Send {
                agent: "oss".into(),
                to: "api".into(),
                body: "spoof".into(),
                reply_to: Some(id.clone()),
                expects_reply: None,
                pipe: None,
                ext: None,
            },
            Request::Ack {
                agent: "oss".into(),
                id: id.clone(),
            },
        ] {
            let e = check_main(bus, &req).unwrap_err().to_string();
            assert!(e.contains("only its door acts as it"), "{e}");
        }
        for req in [
            Request::AgentRemove { name: "oss".into() },
            Request::AgentMove {
                from: "api".into(),
                to: "oss".into(),
            },
        ] {
            let e = check_main(bus, &req).unwrap_err().to_string();
            assert!(e.contains("set in config"), "{e}");
        }
        // Sending to it, and looking at its inbox, are fine.
        check_main(
            bus,
            &Request::Send {
                agent: "api".into(),
                to: "oss".into(),
                body: "q".into(),
                reply_to: None,
                expects_reply: None,
                pipe: None,
                ext: None,
            },
        )
        .unwrap();
        check_main(
            bus,
            &Request::Inbox {
                agent: "oss".into(),
            },
        )
        .unwrap();
    }

    #[test]
    fn its_folder_must_be_the_daemons_users_alone() {
        let dir = env::temp_dir().join(format!("tb-dir-{}", &new_id()[16..]));
        fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("door.sock");
        let mode = |m: u32| fs::set_permissions(&dir, fs::Permissions::from_mode(m)).unwrap();
        mode(0o700);
        check_folder(&socket, me()).unwrap();
        mode(0o750);
        check_folder(&socket, me()).unwrap();
        for bad in [0o770, 0o777, 0o702] {
            mode(bad);
            let e = check_folder(&socket, me()).unwrap_err().to_string();
            assert!(e.contains("writable by others"), "{e}");
        }
        mode(0o700);
        let e = check_folder(&socket, me() + 1).unwrap_err().to_string();
        assert!(e.contains("not the daemon's user"), "{e}");
        let e = check_folder(&dir.join("missing/door.sock"), me())
            .unwrap_err()
            .to_string();
        assert!(e.contains("the setup makes it"), "{e}");
        let long = dir.join("x".repeat(120));
        assert!(check_folder(&long, me()).is_err());
        // Something that isn't a socket where the socket goes is left alone.
        fs::write(&socket, "mine").unwrap();
        let bus = crate::bus::tests::bus_on("here", 1, "");
        let door = Door {
            name: "oss".into(),
            account: "a".into(),
            uid: me(),
            socket: socket.clone(),
        };
        let e = open(&bus, door).unwrap_err().to_string();
        assert!(e.contains("isn't a socket"), "{e}");
        assert_eq!(fs::read_to_string(&socket).unwrap(), "mine");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn configured_doors_need_a_real_account_and_not_root() {
        let cfg: Config = toml::from_str(
            "[doors.a]\naccount = \"root\"\nsocket = \"/x/s\"\n\
             [doors.b]\naccount = \"no-such-account-here\"\nsocket = \"/x/s\"",
        )
        .unwrap();
        let doors = configured(&cfg);
        assert!(doors[0].as_ref().unwrap_err().contains("root can't"));
        assert!(doors[1].as_ref().unwrap_err().contains("no account"));
        assert!(uid_of("root") == Some(0));
    }
}
