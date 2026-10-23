//! tailbus: direct messages between AI agents on a tailnet, queued until
//! they're read. See DESIGN.md.

mod bus;
mod config;
mod daemon;
mod format;
mod hook;
mod local;
mod mcp;
mod net;
mod proto;
mod setup;
mod store;
mod tailscale;
mod util;

use std::env;
use std::io::{self, IsTerminal, Read};
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};

use crate::config::Paths;
use crate::format::Tools;
use crate::local::{Client, Events, Inbox, Peers, Request, Sent, Status, Thread};
use crate::proto::AgentList;
use crate::util::{derive_agent_name, normalize_name, rfc3339, valid_name};

#[derive(Parser)]
#[command(
    name = "tailbus",
    version,
    about = "Direct messages between AI agents on a tailnet"
)]
struct Cli {
    /// Act as this agent. Defaults to the repository you're in.
    #[arg(long = "as", global = true, env = "TAILBUS_AGENT", value_name = "NAME")]
    agent: Option<String>,

    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Send a message to another agent.
    Send {
        /// The recipient, as name@machine.
        to: String,
        /// The message. Read from stdin when left out or `-`.
        body: Option<String>,
        /// The ID of the message this answers.
        #[arg(long, value_name = "ID")]
        reply_to: Option<String>,
        /// Say you'll wait for an answer.
        #[arg(long)]
        expects_reply: bool,
    },
    /// Take your next messages.
    Recv {
        /// Wait for one to arrive.
        #[arg(long)]
        wait: bool,
        /// How long to wait, in seconds.
        #[arg(long, default_value_t = 300, value_name = "SECS")]
        timeout: u64,
        /// The most to take.
        #[arg(long, default_value_t = 10)]
        max: usize,
        /// Print JSON, one message per line.
        #[arg(long)]
        json: bool,
    },
    /// Mark a message handled without answering it.
    Ack { id: String },
    /// List messages to you not yet handled, and your messages still open.
    Inbox {
        #[arg(long)]
        json: bool,
    },
    /// Show the conversation a message belongs to.
    Thread { id: String },
    /// Subscribe agents ahead of time, or list this machine's.
    Agent {
        #[command(subcommand)]
        command: AgentCmd,
    },
    /// List the agents you can reach.
    Peers,
    /// Show the daemon's state.
    Status,
    /// Follow the daemon's log of messages and sessions.
    Tail,
    /// Run the daemon in the foreground.
    Daemon,
    /// Run the MCP server, as an agent app does for each session.
    Mcp,
    /// Hook commands, run by the Claude Code plugin.
    #[command(hide = true)]
    Hook {
        #[command(subcommand)]
        command: HookCmd,
    },
    /// Connect an agent app to the bus.
    Setup {
        #[command(subcommand)]
        app: SetupApp,
    },
    /// Run the daemon at login, with launchd.
    Service {
        #[command(subcommand)]
        command: ServiceCmd,
    },
}

#[derive(Subcommand)]
enum AgentCmd {
    /// Subscribe an agent, so messages can queue before its first session.
    Add { name: String },
    /// List the agents subscribed on this machine.
    List,
}

#[derive(Subcommand)]
enum HookCmd {
    SessionStart,
    Wait,
}

#[derive(Subcommand)]
enum SetupApp {
    /// Install the Claude Code plugin: the bus tools, and push into sessions.
    Claude {
        /// Only write the plugin's files; don't run `claude plugin install`.
        #[arg(long)]
        no_install: bool,
    },
}

#[derive(Subcommand)]
enum ServiceCmd {
    /// Start the daemon now and at every login, replacing a running one.
    Install,
    /// Stop the daemon and stop starting it at login.
    Uninstall,
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("tailbus: {e:#}");
            ExitCode::FAILURE
        }
    }
}

/// The agent the CLI or MCP server acts as.
fn agent_name(explicit: Option<String>) -> Result<String> {
    match explicit {
        Some(name) if valid_name(&name) => Ok(name),
        Some(name) => bail!(
            "{name:?} isn't a valid agent name: use lowercase letters, digits and hyphens \
             (such as {:?})",
            normalize_name(&name)
        ),
        None => {
            let dir = env::var_os("CLAUDE_PROJECT_DIR")
                .map(PathBuf::from)
                .map_or_else(env::current_dir, Ok)?;
            Ok(derive_agent_name(&dir))
        }
    }
}

fn run(cli: Cli) -> Result<ExitCode> {
    let paths = Paths::find()?;
    paths.check_socket()?;
    let socket = paths.socket();
    let call = |req: &Request| -> Result<serde_json::Value> { local::call(&socket, req) };

    match cli.command {
        Cmd::Daemon => daemon::run(&paths)?,
        Cmd::Mcp => {
            let agent = agent_name(cli.agent)?;
            let session = env::var("CLAUDE_CODE_SESSION_ID")
                .or_else(|_| env::var("CODEX_THREAD_ID"))
                .ok()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| format!("mcp-{}-{}", std::process::id(), util::new_id()));
            mcp::run(socket, agent, session, std::os::unix::process::parent_id())?;
        }
        Cmd::Hook { command } => {
            let explicit = cli.agent.filter(|a| valid_name(a));
            return match command {
                HookCmd::SessionStart => hook::session_start(&socket, explicit),
                HookCmd::Wait => hook::wait(&socket, explicit),
            };
        }
        Cmd::Setup {
            app: SetupApp::Claude { no_install },
        } => setup::claude(!no_install)?,
        Cmd::Service { command } => match command {
            ServiceCmd::Install => setup::service_install(&paths)?,
            ServiceCmd::Uninstall => setup::service_uninstall()?,
        },
        Cmd::Send {
            to,
            body,
            reply_to,
            expects_reply,
        } => {
            let body = match body.as_deref() {
                None | Some("-") => {
                    if io::stdin().is_terminal() {
                        eprintln!("Type the message, then Ctrl-D:");
                    }
                    let mut text = String::new();
                    io::stdin().read_to_string(&mut text)?;
                    text
                }
                Some(text) => text.to_string(),
            };
            let sent: Sent = local::call(
                &socket,
                &Request::Send {
                    agent: agent_name(cli.agent)?,
                    to,
                    body,
                    reply_to,
                    expects_reply,
                },
            )?;
            println!("{}", format::sent(&sent));
        }
        Cmd::Recv {
            wait,
            timeout,
            max,
            json,
        } => {
            let agent = agent_name(cli.agent)?;
            let mut client = Client::connect(&socket)?;
            let got = local::recv(
                &mut client,
                &Request::Recv {
                    agent,
                    // One session for all CLI use, so a script that reads a
                    // message without acknowledging it doesn't get it again.
                    session: "cli".into(),
                    max,
                    wait_secs: if wait { timeout.max(1) } else { 0 },
                },
            )?;
            if json {
                for item in &got {
                    println!("{}", serde_json::to_string(item)?);
                }
            } else if got.is_empty() {
                eprintln!("No new messages.");
            } else if io::stdout().is_terminal() {
                for item in &got {
                    println!("{}", format::for_terminal(item));
                }
            } else {
                print!("{}", format::messages_for_agent(&got, Tools::Cli));
            }
            if got.is_empty() && wait {
                return Ok(ExitCode::from(1));
            }
        }
        Cmd::Ack { id } => {
            call(&Request::Ack {
                agent: agent_name(cli.agent)?,
                id,
            })?;
        }
        Cmd::Inbox { json } => {
            let agent = agent_name(cli.agent)?;
            let inbox: Inbox = local::call(
                &socket,
                &Request::Inbox {
                    agent: agent.clone(),
                },
            )?;
            if json {
                println!("{}", serde_json::to_string_pretty(&inbox)?);
            } else {
                print!("{}", format::inbox(&agent, &inbox));
            }
        }
        Cmd::Thread { id } => {
            let thread: Thread = local::call(&socket, &Request::Thread { id })?;
            print!("{}", format::thread(&thread.entries));
        }
        Cmd::Agent { command } => match command {
            AgentCmd::Add { name } => {
                let name = agent_name(Some(name))?;
                let added = call(&Request::AgentAdd { name: name.clone() })?;
                if added["added"].as_bool() == Some(true) {
                    println!("Subscribed {name}.");
                } else {
                    println!("{name} was already subscribed.");
                }
            }
            AgentCmd::List => {
                let list: AgentList = local::call(&socket, &Request::Agents)?;
                let peers = Peers {
                    here: list,
                    machines: Vec::new(),
                };
                print!("{}", format::peers(&peers));
            }
        },
        Cmd::Peers => {
            let peers: Peers = local::call(&socket, &Request::Peers)?;
            print!("{}", format::peers(&peers));
        }
        Cmd::Status => {
            let s: Status = local::call(&socket, &Request::Status)?;
            println!(
                "tailbus {} on {} ({})",
                s.version,
                s.machine.as_deref().unwrap_or("?"),
                s.owner.as_deref().unwrap_or("Tailscale not ready")
            );
            match (&s.listening, &s.problem) {
                (Some(addr), _) => println!("Listening on {addr}"),
                (None, Some(why)) => println!("Not listening: {why}"),
                (None, None) => println!("Not listening yet"),
            }
            println!("Outbox: {} queued", s.queued);
            if s.agents.is_empty() {
                println!("No agents yet.");
            }
            for a in &s.agents {
                println!(
                    "  {}  {}{}, {} open",
                    a.name,
                    if a.live { "live" } else { "idle" },
                    a.app
                        .as_deref()
                        .map(|a| format!(" ({a})"))
                        .unwrap_or_default(),
                    a.open
                );
            }
        }
        Cmd::Tail => {
            let mut client = Client::connect(&socket)?;
            let mut after = 0;
            loop {
                let got: Events = client.call(&Request::Tail { after })?;
                for e in got.events {
                    println!("{} {}", rfc3339(e.at), e.text);
                    after = e.seq;
                }
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}
