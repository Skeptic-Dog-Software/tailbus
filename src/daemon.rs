//! `tailbus daemon`: one per machine. Owns the database and serves the local
//! API on a Unix socket, the API between machines on the tailnet address,
//! and the forwarder.

use std::fs;
use std::io::{self, BufRead, BufReader, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::Serialize;

use crate::bus::{Bus, Waited};
use crate::config::{Config, Paths};
use crate::local::{
    self, AgentStatus, Events, Held, Hello, Item, Messages, Request, Status, Thread, Woken,
};
use crate::net;
use crate::store::{Incoming, Store};
use crate::util::{now_ms, valid_name};

/// How long a client has to confirm it took the messages it was given.
const CONFIRM_WITHIN: Duration = Duration::from_secs(30);
/// The longest `recv --wait`.
const MAX_WAIT: u64 = 3600;
const VERSION: &str = env!("CARGO_PKG_VERSION");

pub fn run(paths: &Paths) -> Result<()> {
    paths.make_state_dir()?;
    let cfg = Config::load(&paths.config)?;
    let socket = paths.socket();
    if UnixStream::connect(&socket).is_ok() {
        bail!(
            "a tailbus daemon is already running on {}",
            socket.display()
        );
    }
    let _ = fs::remove_file(&socket);
    let store = Store::open(&paths.db())?;
    let bus = Arc::new(Bus::new(cfg, store));
    let listener = UnixListener::bind(&socket)
        .with_context(|| format!("listening on {}", socket.display()))?;
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))?;

    match bus.refresh_status() {
        Ok(s) => bus.event(format!(
            "tailbus {VERSION} up on {} ({})",
            s.machine, s.owner
        )),
        Err(e) => bus.event(format!(
            "tailbus {VERSION} up; waiting for Tailscale: {e:#}"
        )),
    }
    {
        let bus = bus.clone();
        thread::spawn(move || net::serve(bus));
    }
    {
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

    /// Hands messages over: sends them, and records them delivered once the
    /// client confirms.
    fn hand_over(
        &mut self,
        bus: &Bus,
        agent: &str,
        session: &str,
        reply: impl Serialize,
        found: &[Incoming],
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
            let ids: Vec<String> = found.iter().map(|m| m.msg.id.clone()).collect();
            bus.deliver(agent, &ids, session)?;
            self.write(&serde_json::json!({}))?;
        }
        Ok(())
    }
}

fn items(found: &[Incoming]) -> Vec<Item> {
    found.iter().cloned().map(Item::from).collect()
}

fn serve_conn(bus: &Bus, stream: UnixStream) -> Result<()> {
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
        } => match bus.hold(&agent, &session, app.as_deref(), client_pid)? {
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
        } => {
            let sent = bus.send(&agent, &to, body, reply_to, expects_reply)?;
            conn.write(&sent)?;
        }
        Request::Recv {
            agent,
            session,
            max,
            wait_secs,
        } => {
            check_name(&agent)?;
            bus.db().agent_live(&agent, None, now_ms())?;
            let max = max.clamp(1, 100);
            let found = if wait_secs == 0 {
                bus.takeable(&agent, &session, max)?
            } else {
                let deadline = Instant::now() + Duration::from_secs(wait_secs.min(MAX_WAIT));
                bus.receiving(&agent, 1);
                let found = bus.wait_until(
                    deadline,
                    || {
                        let found = bus.takeable(&agent, &session, max)?;
                        Ok((!found.is_empty()).then_some(found))
                    },
                    || conn.gone(),
                );
                bus.receiving(&agent, -1);
                found?.unwrap_or_default()
            };
            let reply = Messages {
                messages: items(&found),
            };
            conn.hand_over(bus, &agent, &session, reply, &found)?;
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
            let waited = bus.wait_as_waiter(&agent, &session, token, || conn.gone());
            let result = match waited {
                Ok(Some(Waited::Messages(found))) => {
                    let reply = Woken::Messages {
                        messages: items(&found),
                    };
                    conn.hand_over(bus, &agent, &session, reply, &found)
                }
                Ok(Some(Waited::Replaced)) => Ok(conn.write(&Woken::Replaced)?),
                Ok(None) => Ok(()),
                Err(e) => Err(e),
            };
            bus.remove_waiter(&agent, token);
            result?;
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
