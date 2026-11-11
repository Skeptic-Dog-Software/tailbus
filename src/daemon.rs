//! `tailbus daemon`: one per machine. Owns the database and serves the local
//! API on a Unix socket, the API between machines on the tailnet address,
//! and the forwarder.

use std::fs;
use std::io::{self, BufRead, BufReader, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::Serialize;

use crate::ask;
use crate::bus::{Bus, Claim, Draft, Handoff, Waited};
use crate::config::APP_NAME;
use crate::config::{Config, Paths};
use crate::local::{
    self, AgentStatus, Awaited, BRIDGE_SESSION, Events, Held, Hello, Item, Messages, Request,
    Status, Thread, Verdict, Woken,
};
use crate::net;
use crate::permission;
use crate::proto::{MAX_BATCH, PipeStage};
use crate::store::{Incoming, Store};
use crate::util::{now_ms, valid_name};

/// How long a client has to confirm it took the messages it was given.
const CONFIRM_WITHIN: Duration = Duration::from_secs(30);
/// The longest `recv --wait`.
const MAX_WAIT: u64 = 3600;
/// The longest a blocking call waits.
const MAX_CALL_WAIT: u64 = 7 * 86_400;
/// The longest a permission prompt waits for an alert's answer: the
/// permission hook's own timeout.
const MAX_PERMISSION_WAIT: Duration = Duration::from_secs(86_400);
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// How often the daemon's own log is looked at, to be cut back.
const LOG_TRIM_EVERY: Duration = Duration::from_secs(3600);

pub fn run(paths: &Paths) -> Result<()> {
    crate::util::append_to_logs();
    paths.make_state_dir()?;
    let cfg = Config::load(&paths.config)?;
    let socket = paths.socket();
    if UnixStream::connect(&socket).is_ok() {
        bail!(
            "a {APP_NAME} daemon is already running on {}",
            socket.display()
        );
    }
    // Before anything is opened: a local bus with no owner can't start.
    let network = crate::network::open(&cfg)?;
    let _ = fs::remove_file(&socket);
    let store = Store::open(&paths.db())?;
    let bus = Bus::start(cfg, network, store, paths.attachments());
    {
        // launchd's log for the daemon, which nothing else cuts back.
        let (bus, log) = (bus.clone(), paths.log());
        thread::spawn(move || {
            loop {
                if matches!(
                    crate::util::trim_log(&log, crate::util::LOG_MAX, crate::util::LOG_KEEP),
                    Ok(true)
                ) {
                    bus.event(format!("cut {} back to its last lines", log.display()));
                }
                thread::sleep(LOG_TRIM_EVERY);
            }
        });
    }
    let listener = UnixListener::bind(&socket)
        .with_context(|| format!("listening on {}", socket.display()))?;
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))?;

    let local = if bus.remote() { "" } else { ", local only" };
    match bus.refresh_status() {
        Ok(s) => bus.event(format!(
            "{APP_NAME} {VERSION} up on {} ({}){local}",
            s.machine, s.owner
        )),
        Err(e) => bus.event(format!(
            "{APP_NAME} {VERSION} up; waiting for its network: {e:#}"
        )),
    }
    if bus.remote() {
        let bus = bus.clone();
        thread::spawn(move || net::serve(bus));
    } else {
        *bus.listening.lock().unwrap_or_else(|e| e.into_inner()) =
            Err("this bus is local, so it doesn't listen for other machines".into());
    }
    {
        // Also what prunes old messages, so a local bus runs it too, with
        // nothing to forward.
        let bus = bus.clone();
        thread::spawn(move || net::forward(bus));
    }

    let me = unsafe { libc::geteuid() };
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        // The socket is 0600 already; this also turns away root's processes.
        if peer_uid(&stream).ok() != Some(me) {
            continue;
        }
        let bus = bus.clone();
        thread::spawn(move || {
            let _ = serve_conn(&bus, stream);
        });
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn peer_uid(stream: &UnixStream) -> io::Result<u32> {
    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&raw mut cred).cast(),
            &mut len,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(cred.uid)
}

#[cfg(not(target_os = "linux"))]
fn peer_uid(stream: &UnixStream) -> io::Result<u32> {
    let (mut uid, mut gid) = (0, 0);
    if unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(uid)
}

struct Conn {
    reader: BufReader<UnixStream>,
    out: UnixStream,
}

impl Conn {
    fn write<T: Serialize>(&mut self, data: &T) -> io::Result<()> {
        let mut line = local::ok_line(data);
        line.push('\n');
        self.out.write_all(line.as_bytes())
    }

    fn write_err(&mut self, e: &anyhow::Error) -> io::Result<()> {
        let mut line = local::err_line(e);
        line.push('\n');
        self.out.write_all(line.as_bytes())
    }

    fn read(&mut self) -> io::Result<Option<String>> {
        let mut line = String::new();
        Ok((self.reader.read_line(&mut line)? > 0).then_some(line))
    }

    /// Whether the client has hung up. Clients send nothing while waiting
    /// for a reply, so anything readable means end of file.
    fn gone(&self) -> bool {
        if !self.reader.buffer().is_empty() {
            return false;
        }
        let mut b = [0u8; 1];
        let n = unsafe {
            libc::recv(
                self.reader.get_ref().as_raw_fd(),
                b.as_mut_ptr().cast(),
                1,
                libc::MSG_PEEK | libc::MSG_DONTWAIT,
            )
        };
        match n {
            0 => true,
            n if n > 0 => false,
            _ => io::Error::last_os_error().kind() != io::ErrorKind::WouldBlock,
        }
    }

    /// Hands over messages `take` reserved: sends them, records them
    /// delivered once the client confirms, and frees them either way.
    fn hand_over(
        &mut self,
        bus: &Bus,
        agent: &str,
        session: &str,
        reply: impl Serialize,
        found: &[Incoming],
        how: Handoff,
    ) -> Result<()> {
        let handed = self.try_hand_over(bus, agent, session, reply, found, how);
        bus.unreserve(found);
        handed
    }

    fn try_hand_over(
        &mut self,
        bus: &Bus,
        agent: &str,
        session: &str,
        reply: impl Serialize,
        found: &[Incoming],
        how: Handoff,
    ) -> Result<()> {
        self.write(&reply)?;
        if found.is_empty() {
            return Ok(());
        }
        self.reader
            .get_ref()
            .set_read_timeout(Some(CONFIRM_WITHIN))?;
        let line = self.read();
        self.reader.get_ref().set_read_timeout(None)?;
        let confirmed = matches!(
            line.ok()
                .flatten()
                .map(|l| serde_json::from_str::<Request>(&l)),
            Some(Ok(Request::Confirm))
        );
        if confirmed {
            bus.deliver(agent, found, session, how)?;
            self.write(&serde_json::json!({}))?;
        }
        Ok(())
    }
}

fn items(bus: &Bus, found: &[Incoming]) -> Vec<Item> {
    let mut items: Vec<Item> = found.iter().cloned().map(Item::from).collect();
    for item in &mut items {
        bus.locate(item);
    }
    items
}

/// Waits for the reply to `id`, for a blocking call on this connection, and
/// hands it over.
fn await_reply(bus: &Bus, conn: &mut Conn, agent: &str, id: &str, wait_secs: u64) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(wait_secs.min(MAX_CALL_WAIT));
    let mut acked = None;
    let got = bus.wait_until(
        deadline,
        || bus.check_awaited(agent, id, &mut acked),
        || conn.gone(),
    );
    let got = got.inspect_err(|_| bus.stop_awaiting(id))?;
    let gone = conn.gone();
    let Some(got) = got else {
        bus.stop_awaiting(id);
        return if gone {
            Ok(())
        } else {
            Ok(conn.write(&Awaited::TimedOut)?)
        };
    };
    let written = conn.write(&got);
    if let (Ok(()), Awaited::Reply { item }) = (&written, &got) {
        let reply = item.msg.id.clone();
        // In a statement of its own, so the lock is let go before `deliver`.
        let found = bus.db().incoming(&reply)?;
        if let Some(found) = found {
            bus.deliver(agent, &[found], "cli", Handoff::Free)?;
        }
        bus.ack(agent, &reply)?;
    }
    bus.stop_awaiting(id);
    Ok(written?)
}

pub(crate) fn serve_conn(bus: &Bus, stream: UnixStream) -> Result<()> {
    let mut conn = Conn {
        reader: BufReader::new(stream.try_clone()?),
        out: stream,
    };
    while let Some(line) = conn.read()? {
        let req: Request = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                conn.write_err(&anyhow::anyhow!("bad request: {e}"))?;
                continue;
            }
        };
        if let Err(e) = serve_one(bus, &mut conn, req) {
            conn.write_err(&e)?;
        }
    }
    Ok(())
}

fn check_name(agent: &str) -> Result<()> {
    if !valid_name(agent) {
        bail!("{agent:?} isn't a valid agent name: use lowercase letters, digits and hyphens");
    }
    Ok(())
}

fn serve_one(bus: &Bus, conn: &mut Conn, req: Request) -> Result<()> {
    match req {
        Request::Hello => {
            // Answers even before Tailscale is ready, with no names yet.
            let me = bus.me().ok();
            conn.write(&Hello {
                machine: me.as_ref().map(|m| m.machine.clone()).unwrap_or_default(),
                owner: me.map(|m| m.owner).unwrap_or_default(),
                version: VERSION.into(),
            })?;
        }
        Request::Hold {
            agent,
            session,
            app,
            client_pid,
            client,
            billing,
        } => match bus.hold(
            &agent,
            &Claim {
                session: &session,
                app: app.as_deref(),
                client_pid,
                client: client.as_deref(),
                billing: billing.as_ref(),
            },
        )? {
            Err(reason) => conn.write(&Held {
                held: false,
                reason: Some(reason),
            })?,
            Ok(token) => {
                conn.write(&Held {
                    held: true,
                    reason: None,
                })?;
                // Held for as long as the session keeps this connection.
                while matches!(conn.read(), Ok(Some(_))) {}
                bus.release(&agent, token);
            }
        },
        Request::Send {
            agent,
            to,
            body,
            reply_to,
            expects_reply,
            pipe,
            ext,
        } => {
            // A pipe task goes only to a live session, found by client name
            // or address.
            let to = match &pipe {
                Some(p) if p.stage == PipeStage::Task => bus.pipe_target(&to)?,
                _ => to,
            };
            let draft = Draft {
                reply_to,
                expects_reply,
                pipe,
                ext,
                awaited: false,
            };
            let sent = bus.send_draft(&agent, &to, body, draft)?;
            conn.write(&sent)?;
        }
        Request::Call {
            agent,
            to,
            body,
            reply_to,
            expects_reply,
            ext,
            wait_secs,
        } => {
            let draft = Draft {
                reply_to,
                expects_reply,
                pipe: None,
                ext,
                awaited: wait_secs.is_some(),
            };
            let sent = bus.send_draft(&agent, &to, body, draft)?;
            let id = sent.id.clone();
            conn.write(&sent)?;
            if let Some(wait_secs) = wait_secs {
                await_reply(bus, conn, &agent, &id, wait_secs)?;
            }
        }
        Request::Progress {
            agent,
            reply_to,
            body,
        } => {
            let draft = Draft {
                reply_to: Some(reply_to),
                expects_reply: Some(false),
                pipe: None,
                ext: Some(Box::new(crate::proto::Extras {
                    progress: true,
                    ..Default::default()
                })),
                awaited: false,
            };
            let sent = bus.send_draft(&agent, "", body, draft)?;
            conn.write(&sent)?;
        }
        Request::Await {
            agent,
            id,
            wait_secs,
        } => {
            bus.start_awaiting(&id);
            await_reply(bus, conn, &agent, &id, wait_secs)?;
        }
        Request::Cancel { agent, run_id } => conn.write(&bus.cancel_run(&agent, &run_id)?)?,
        Request::Recv {
            agent,
            session,
            max,
            wait_secs,
        } => {
            check_name(&agent)?;
            bus.db().agent_live(&agent, None, now_ms())?;
            let max = max.clamp(1, 100);
            // An agent session's handoffs count toward the limit; the CLI's
            // and a bridge's don't.
            let how = if session == "cli" || session.starts_with(BRIDGE_SESSION) {
                Handoff::Free
            } else {
                Handoff::Tool
            };
            let paused = || match how {
                Handoff::Free => None,
                _ => bus.paused(&agent),
            };
            let mut note = paused();
            let found = if note.is_some() {
                Vec::new()
            } else if wait_secs == 0 {
                bus.take(&agent, &session, max, false)?
            } else {
                let deadline = Instant::now() + Duration::from_secs(wait_secs.min(MAX_WAIT));
                bus.receiving(&agent, 1);
                let got = bus.wait_until(
                    deadline,
                    || {
                        if let Some(why) = paused() {
                            return Ok(Some(Err(why)));
                        }
                        let found = bus.take(&agent, &session, max, false)?;
                        Ok((!found.is_empty()).then_some(Ok(found)))
                    },
                    || conn.gone(),
                );
                bus.receiving(&agent, -1);
                match got? {
                    Some(Ok(found)) => found,
                    Some(Err(why)) => {
                        note = Some(why);
                        Vec::new()
                    }
                    None => Vec::new(),
                }
            };
            let reply = Messages {
                messages: items(bus, &found),
                paused: note,
            };
            conn.hand_over(bus, &agent, &session, reply, &found, how)?;
        }
        Request::Wait {
            agent,
            session,
            client_pid,
        } => {
            let (token, session) = match bus.add_waiter(&agent, &session, client_pid)? {
                Ok(granted) => granted,
                Err(reason) => return Ok(conn.write(&Woken::Refused { reason })?),
            };
            // Waits start at session start and after every turn.
            bus.turn_ended(&agent)?;
            let waited = bus.wait_as_waiter(&agent, &session, token, || conn.gone());
            let result = match waited {
                Ok(Some(Waited::Messages(found))) => {
                    let reply = Woken::Messages {
                        messages: items(bus, &found),
                    };
                    conn.hand_over(bus, &agent, &session, reply, &found, Handoff::Wake)
                }
                Ok(Some(Waited::Replaced)) => Ok(conn.write(&Woken::Replaced)?),
                Ok(None) => Ok(()),
                Err(e) => Err(e),
            };
            bus.remove_waiter(&agent, token);
            result?;
        }
        Request::Turn {
            agent,
            session,
            client_pid,
            typed,
        } => {
            check_name(&agent)?;
            let Some(session) = bus.turn(&agent, &session, client_pid, typed) else {
                return Ok(conn.write(&Messages::default())?);
            };
            // What's waiting comes along with the turn, unless the agent is
            // paused. A turn the user started counts for nothing; a woken
            // one was counted when it was woken.
            let found = if bus.paused(&agent).is_some() {
                Vec::new()
            } else {
                bus.take(&agent, &session, MAX_BATCH, false)?
            };
            let reply = Messages {
                messages: items(bus, &found),
                paused: None,
            };
            conn.hand_over(bus, &agent, &session, reply, &found, Handoff::Free)?;
        }
        Request::Permission {
            agent,
            session: _,
            tool,
            input,
            cwd,
            suggestions,
            unattended,
        } => {
            check_name(&agent)?;
            if permission::ANSWERED_AT_TERMINAL.contains(&tool.as_str()) {
                return Ok(conn.write(&Verdict::default())?);
            }
            let key = serde_json::to_string(&input)?;
            let id = bus.perm_open(&agent, &tool, &key);
            let asked = Asked {
                agent: &agent,
                tool: &tool,
                input: &input,
                cwd: &cwd,
                suggestions: &suggestions,
                unattended,
            };
            let verdict = permission(bus, conn, &id, &asked);
            bus.perm_close(&id);
            conn.write(&Verdict { output: verdict? })?;
        }
        Request::ToolDone { agent, tool, input } => {
            check_name(&agent)?;
            bus.tool_done(&agent, &tool, &serde_json::to_string(&input)?);
            conn.write(&serde_json::json!({}))?;
        }
        Request::Subagent {
            agent,
            transcript,
            tool_use_id,
        } => {
            check_name(&agent)?;
            let refused = if transcript.is_empty() {
                None
            } else {
                bus.subagent_refused(&agent, transcript.as_ref(), &tool_use_id)?
            };
            conn.write(&Verdict {
                output: refused.map(|why| permission::refuse_tool(&why)),
            })?;
        }
        Request::Confirm => bail!("there's nothing to confirm"),
        Request::Ack { agent, id } => {
            check_name(&agent)?;
            bus.ack(&agent, &id)?;
            conn.write(&serde_json::json!({}))?;
        }
        Request::Inbox { agent } => {
            check_name(&agent)?;
            conn.write(&bus.inbox(&agent)?)?;
        }
        Request::Thread { id } => {
            conn.write(&Thread {
                entries: bus.thread(&id)?,
            })?;
        }
        Request::AgentAdd { name } => {
            check_name(&name)?;
            let added = bus.db().add_agent(&name, now_ms())?;
            if added {
                bus.event(format!("{name}: subscribed"));
            }
            conn.write(&serde_json::json!({ "added": added }))?;
        }
        Request::AgentRemove { name } => {
            check_name(&name)?;
            let stale = bus.remove_agent(&name)?;
            conn.write(&serde_json::json!({ "stale": stale }))?;
        }
        Request::AgentMove { from, to } => {
            check_name(&from)?;
            check_name(&to)?;
            conn.write(&bus.move_agent(&from, &to)?)?;
        }
        Request::Agents => conn.write(&bus.agent_list()?)?,
        Request::Peers => conn.write(&bus.peers()?)?,
        Request::Status => conn.write(&status(bus)?)?,
        Request::Tail { after } => {
            let deadline = Instant::now() + Duration::from_secs(25);
            let events = bus
                .wait_until(
                    deadline,
                    || {
                        let events = bus.events_after(after);
                        Ok((!events.is_empty()).then_some(events))
                    },
                    || conn.gone(),
                )?
                .unwrap_or_default();
            conn.write(&Events { events })?;
        }
    }
    Ok(())
}

struct Asked<'a> {
    agent: &'a str,
    tool: &'a str,
    input: &'a serde_json::Value,
    cwd: &'a str,
    suggestions: &'a serde_json::Value,
    unattended: bool,
}

/// Holds a permission prompt until the user answers it, on a Mac or at the
/// terminal. Returns what the hook prints, if the answer came from an
/// alert.
fn permission(
    bus: &Bus,
    conn: &Conn,
    id: &str,
    asked: &Asked,
) -> Result<Option<serde_json::Value>> {
    let agent = asked.agent;
    let woken_by = bus.woken_by(agent);
    // In a turn the user started, they're likely at the terminal: give
    // them a while to answer there first.
    if !asked.unattended && woken_by.is_none() {
        let wait = Duration::from_secs(bus.cfg.ask_after_secs);
        bus.wait_until(
            Instant::now() + wait,
            || Ok(bus.perm_done(id).then_some(())),
            || conn.gone(),
        )?;
        if bus.perm_done(id) {
            bus.event(format!(
                "{agent}: {} was answered at its terminal",
                asked.tool
            ));
        }
        if bus.perm_done(id) || conn.gone() {
            return Ok(None);
        }
    }
    let me = bus.me()?;
    let rules = permission::session_rules(asked.tool, asked.input, asked.suggestions);
    let prompt = permission::prompt(&permission::Request {
        agent: &format!("{agent}@{}", me.machine),
        tool: asked.tool,
        input: asked.input,
        cwd: asked.cwd,
        woken_by: woken_by.as_deref(),
        rules: &rules,
    });
    let (ask_id, shown) = ask::ask(bus, prompt)?;
    if shown == 0 {
        ask::close(bus, &ask_id);
        bus.event(format!(
            "{agent}: no Mac could show the permission alert for {}",
            asked.tool
        ));
        return Ok(None);
    }
    bus.event(format!(
        "{agent}: asking you, on {shown} Mac(s), to allow {}",
        asked.tool
    ));
    let answer = bus.wait_until(
        Instant::now() + MAX_PERMISSION_WAIT,
        || {
            if bus.perm_done(id) {
                return Ok(Some(None));
            }
            Ok(ask::answer(bus, &ask_id).map(Some))
        },
        || conn.gone(),
    )?;
    ask::close(bus, &ask_id);
    let Some(Some(answer)) = answer else {
        if bus.perm_done(id) {
            bus.event(format!(
                "{agent}: {} was answered at its terminal; alert withdrawn",
                asked.tool
            ));
        }
        return Ok(None);
    };
    let tool = asked.tool;
    let what = match answer.choice {
        permission::ALLOW => format!("allowed {tool}"),
        permission::ALLOW_FOR_SESSION => format!("allowed {tool} for the session"),
        permission::DENY => format!("denied {tool}"),
        // The dialog's buttons are the ones above; anything else is a deny.
        _ => format!("denied {tool}"),
    };
    bus.event(format!("{agent}: you {what}, from {}", answer.machine));
    Ok(Some(permission::decision(
        answer.choice,
        &answer.machine,
        &rules,
    )))
}

fn status(bus: &Bus) -> Result<Status> {
    let me = bus.me();
    let listening = bus.listening.lock().unwrap().clone();
    let (open, queued) = bus.db().counts()?;
    let agents = match bus.agent_list() {
        Ok(list) => list
            .agents
            .into_iter()
            .map(|a| AgentStatus {
                open: open.get(&a.name).copied().unwrap_or(0),
                paused: bus.paused(&a.name).is_some(),
                live: a.live,
                app: a.app,
                name: a.name,
            })
            .collect(),
        Err(_) => Vec::new(),
    };
    let problem = match (&me, &listening) {
        (Err(e), _) => Some(format!("{e:#}")),
        (_, Err(why)) => Some(why.clone()),
        _ => None,
    };
    Ok(Status {
        machine: me.as_ref().ok().map(|m| m.machine.clone()),
        owner: me.as_ref().ok().map(|m| m.owner.clone()),
        listening: listening.ok().map(|a| a.to_string()),
        problem,
        agents,
        queued,
        version: VERSION.into(),
    })
}
