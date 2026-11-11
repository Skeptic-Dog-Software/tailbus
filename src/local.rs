//! The local API: JSON lines over the daemon's Unix socket, used by the CLI,
//! the MCP server and the hooks.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::config::APP_NAME;
use crate::proto::{AgentList, Message};
use crate::store::{Incoming, Outgoing};

/// How `tailbus bridge` sessions' names start. Like the CLI's, their
/// handoffs don't count toward the limit: nobody can type in a bridge's
/// session to clear it, and a bridge only ever sends responses.
pub const BRIDGE_SESSION: &str = "bridge-";

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    Hello,
    /// Claims `agent` for a session. The reply says whether it was granted;
    /// then the connection stays open for as long as the session lives.
    Hold {
        agent: String,
        session: String,
        app: Option<String>,
        /// The agent app's process, which owns the session.
        client_pid: u32,
        /// The client the session is a worker for, from `TAILBUS_CLIENT`.
        #[serde(default)]
        client: Option<String>,
        /// How the session is billed, from `claude auth status`.
        #[serde(default)]
        billing: Option<crate::proto::Billing>,
    },
    Send {
        agent: String,
        to: String,
        body: String,
        reply_to: Option<String>,
        /// Whether it asks for an answer. Left out: a new message does and
        /// a reply doesn't.
        #[serde(default)]
        expects_reply: Option<bool>,
        /// A `/pipe` step.
        #[serde(default)]
        pipe: Option<crate::proto::Pipe>,
        /// A bridge's result and attachments. An older daemon drops these,
        /// which costs the detail and not the reply.
        #[serde(default)]
        ext: Option<Box<crate::proto::Extras>>,
    },
    /// Sends a message that may carry extras, which an older daemon turns
    /// down instead of dropping them. With `wait_secs`, a second reply on
    /// this connection is an `Awaited`.
    Call {
        agent: String,
        to: String,
        body: String,
        #[serde(default)]
        reply_to: Option<String>,
        #[serde(default)]
        expects_reply: Option<bool>,
        #[serde(default)]
        ext: Option<Box<crate::proto::Extras>>,
        #[serde(default)]
        wait_secs: Option<u64>,
    },
    /// Waits for the reply to a message already sent, as a blocking call
    /// does after a daemon restart. The reply is an `Awaited`.
    Await {
        agent: String,
        id: String,
        wait_secs: u64,
    },
    /// Asks the bridge running one of this agent's runs to cancel it. The
    /// reply is a `Sent`.
    Cancel {
        agent: String,
        run_id: String,
    },
    /// Takes messages, waiting up to `wait_secs` for one. A reply with
    /// messages must be followed by `Confirm` before they count as delivered.
    Recv {
        agent: String,
        session: String,
        max: usize,
        wait_secs: u64,
    },
    /// The push adapter's wait: blocks until a message arrives, or another
    /// waiter for the agent replaces this one.
    Wait {
        agent: String,
        session: String,
        client_pid: u32,
    },
    /// A turn is starting in a session, either from a prompt its user typed
    /// or from a wake. The reply hands over what's waiting, which must be
    /// confirmed like `Recv`.
    Turn {
        agent: String,
        session: String,
        client_pid: u32,
        typed: bool,
    },
    /// The permission hook's prompt: held until the user answers an alert
    /// on one of their Macs, or answers at the terminal. The reply is a
    /// `Verdict`.
    Permission {
        agent: String,
        session: String,
        tool: String,
        input: serde_json::Value,
        #[serde(default)]
        cwd: String,
        /// Claude Code's suggested permission updates.
        #[serde(default)]
        suggestions: serde_json::Value,
        /// A session nobody's attending, such as `claude -p`.
        #[serde(default)]
        unattended: bool,
    },
    /// A tool call ran, so any prompt for it was answered at the terminal.
    ToolDone {
        agent: String,
        tool: String,
        input: serde_json::Value,
    },
    /// The subagent hook: whether a session may start another subagent. The
    /// reply is a `Verdict`, with the hook's refusal if it may not.
    Subagent {
        agent: String,
        /// The session's transcript, beside which its subagents' are kept.
        transcript: String,
        /// The tool call that would start it.
        #[serde(default)]
        tool_use_id: String,
    },
    /// The messages just received were handed on.
    Confirm,
    /// A progress note on a request `agent` was sent and hasn't answered.
    /// Its own request, so a daemon too old to know it says no rather than
    /// taking it as the answer.
    Progress {
        agent: String,
        reply_to: String,
        body: String,
    },
    Ack {
        agent: String,
        id: String,
    },
    Inbox {
        agent: String,
    },
    Thread {
        id: String,
    },
    AgentAdd {
        name: String,
    },
    /// Removes an agent nobody holds; what was waiting for it goes stale.
    /// The reply is `{"stale": n}`.
    AgentRemove {
        name: String,
    },
    /// Moves an agent nobody holds to a new name, taking its messages with
    /// it. The reply is a `Moved`.
    AgentMove {
        from: String,
        to: String,
    },
    Agents,
    Peers,
    Status,
    /// Events after `after`, waiting a while for one.
    Tail {
        after: u64,
    },
}

/// What moving an agent did: how many messages were waiting, and what in
/// config still names the old name.
#[derive(Debug, Serialize, Deserialize)]
pub struct Moved {
    pub waiting: usize,
    #[serde(default)]
    pub notes: Vec<String>,
}

/// The daemon's answer was no, as opposed to there being no answer.
#[derive(Debug)]
pub struct Refused(pub String);

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Refused {}

#[derive(Serialize, Deserialize)]
struct Reply {
    ok: bool,
    #[serde(default)]
    data: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

pub fn ok_line<T: Serialize>(data: &T) -> String {
    let reply = Reply {
        ok: true,
        data: serde_json::to_value(data).unwrap_or_default(),
        error: None,
    };
    serde_json::to_string(&reply).unwrap_or_default()
}

pub fn err_line(error: &anyhow::Error) -> String {
    let reply = Reply {
        ok: false,
        data: serde_json::Value::Null,
        error: Some(format!("{error:#}")),
    };
    serde_json::to_string(&reply).unwrap_or_default()
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Hello {
    pub machine: String,
    pub owner: String,
    pub version: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Held {
    pub held: bool,
    /// Why not, when it wasn't.
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Sent {
    pub id: String,
    pub to: String,
    /// `request`, `response` or `notice`.
    #[serde(default)]
    pub kind: String,
    pub state: String,
    /// For example, that the recipient hasn't had a session in a long time.
    #[serde(default)]
    pub warning: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Messages {
    pub messages: Vec<Item>,
    /// Why there are none, when the agent is paused by the handoff limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paused: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Woken {
    Messages {
        messages: Vec<Item>,
    },
    /// A newer waiter took over.
    Replaced,
    /// Another live session holds the agent.
    Refused {
        reason: String,
    },
}

/// How a blocking call's wait came out.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Awaited {
    Reply {
        item: Box<Item>,
    },
    /// It can't be delivered.
    Undelivered {
        why: String,
    },
    /// Its recipient marked it handled without answering.
    Unanswered,
    TimedOut,
}

/// The answer to a `Permission` request.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Verdict {
    /// What the hook prints, or nothing when the prompt was settled at the
    /// terminal or no alert could be shown.
    #[serde(default)]
    pub output: Option<serde_json::Value>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Inbox {
    pub machine: String,
    /// Messages to you not yet acknowledged.
    pub incoming: Vec<Item>,
    /// Your messages queued here, awaiting a reply, or recently failed.
    pub outgoing: Vec<Item>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Thread {
    pub entries: Vec<Item>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Peers {
    pub here: AgentList,
    pub machines: Vec<PeerMachine>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PeerMachine {
    pub machine: String,
    pub online: bool,
    /// Set when the machine's bus answered.
    #[serde(default)]
    pub agents: Option<AgentList>,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Status {
    pub machine: Option<String>,
    pub owner: Option<String>,
    /// The tailnet address the bus listens on, if it's listening.
    pub listening: Option<String>,
    /// Why it isn't, if it isn't.
    pub problem: Option<String>,
    pub agents: Vec<AgentStatus>,
    pub queued: usize,
    pub version: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AgentStatus {
    pub name: String,
    pub live: bool,
    pub app: Option<String>,
    pub open: usize,
    /// Held back by the handoff limit.
    #[serde(default)]
    pub paused: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Events {
    pub events: Vec<Event>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub seq: u64,
    pub at: i64,
    pub text: String,
}

/// A message as the local API shows it, sent or received.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Item {
    #[serde(flatten)]
    pub msg: Message,
    pub incoming: bool,
    /// Received: `new`, `pending` or `acknowledged`. Sent: its delivery state.
    pub state: String,
    /// Received: the Tailscale login that owns the sending machine.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    /// Sent: why it failed, or why it's still queued.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// Sent and still open: the latest progress note on it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<Progress>,
}

/// A progress note's time and text.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Progress {
    pub at: i64,
    pub body: String,
}

impl From<Incoming> for Item {
    fn from(m: Incoming) -> Item {
        let state = if m.acked_at.is_some() {
            "acknowledged"
        } else if m.delivered_at.is_some() {
            "pending"
        } else {
            "new"
        };
        Item {
            msg: m.msg,
            incoming: true,
            state: state.into(),
            owner: Some(m.owner),
            note: None,
            progress: None,
        }
    }
}

impl From<Outgoing> for Item {
    fn from(m: Outgoing) -> Item {
        Item {
            msg: m.msg,
            incoming: false,
            state: m.state.as_str().into(),
            owner: None,
            note: m.error,
            progress: None,
        }
    }
}

/// A connection to the daemon.
pub struct Client {
    writer: UnixStream,
    reader: BufReader<UnixStream>,
}

impl Client {
    pub fn connect(socket: &Path) -> Result<Client> {
        let stream = UnixStream::connect(socket).map_err(|e| {
            anyhow!(
                "the {APP_NAME} daemon isn't running ({e} at {}). Start it with \
                 `{APP_NAME} service install`, or run `{APP_NAME} daemon`.",
                socket.display()
            )
        })?;
        Ok(Client {
            reader: BufReader::new(stream.try_clone()?),
            writer: stream,
        })
    }

    /// The underlying socket, for shutting the connection down from
    /// another thread.
    pub fn socket(&self) -> Result<UnixStream> {
        Ok(self.writer.try_clone()?)
    }

    pub fn send(&mut self, req: &Request) -> Result<()> {
        let mut line = serde_json::to_string(req)?;
        line.push('\n');
        self.writer
            .write_all(line.as_bytes())
            .with_context(|| format!("writing to the {APP_NAME} daemon"))
    }

    /// Reads the next reply. A dropped connection is an error.
    pub fn reply<T: DeserializeOwned>(&mut self) -> Result<T> {
        let mut line = String::new();
        if self
            .reader
            .read_line(&mut line)
            .with_context(|| format!("reading from the {APP_NAME} daemon"))?
            == 0
        {
            bail!("the {APP_NAME} daemon closed the connection");
        }
        let reply: Reply = serde_json::from_str(&line).context("reading the daemon's reply")?;
        if !reply.ok {
            let why = reply.error.unwrap_or_else(|| "unknown error".into());
            return Err(Refused(why).into());
        }
        Ok(serde_json::from_value(reply.data)?)
    }

    pub fn call<T: DeserializeOwned>(&mut self, req: &Request) -> Result<T> {
        self.send(req)?;
        self.reply()
    }

    /// Blocks until the daemon closes the connection.
    pub fn wait_closed(&mut self) {
        let mut line = String::new();
        while matches!(self.reader.read_line(&mut line), Ok(n) if n > 0) {
            line.clear();
        }
    }
}

/// One request on a fresh connection.
pub fn call<T: DeserializeOwned>(socket: &Path, req: &Request) -> Result<T> {
    Client::connect(socket)?.call(req)
}

/// `Recv` or `Turn`, confirming the hand-off of whatever arrives.
pub fn recv(client: &mut Client, req: &Request) -> Result<Messages> {
    let got: Messages = client.call(req)?;
    if !got.messages.is_empty() {
        client.call::<serde_json::Value>(&Request::Confirm)?;
    }
    Ok(got)
}
