//! The command line, which every program built on acrbus shares: `main`
//! runs it as whichever program `config::set_app` said this is.

use std::env;
use std::io::{self, IsTerminal, Read};
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Result, anyhow, bail};
use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};

use crate::config::{APP_NAME, Paths, app, env_var};
use crate::format::Tools;
use crate::local::{Client, Events, Inbox, Moved, Peers, Request, Sent, Status, Thread};
use crate::proto::{AgentList, RunOptions};
use crate::util::{derive_agent_name, rfc3339};
use crate::{acr, bridge, call, daemon, format, hook, local, mcp, setup, usage, util};

/// The program's name and what it is come from its `App`, at run time.
#[derive(Parser)]
#[command(version)]
struct Cli {
    /// Act as this agent. Defaults to $ACRBUS_AGENT (or $TAILBUS_AGENT), then
    /// the repository you're in.
    #[arg(long = "as", global = true, value_name = "NAME")]
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
        /// Ask for an answer. A new message does unless you say otherwise.
        #[arg(long)]
        expects_reply: bool,
        /// Ask for nothing back. A reply asks for nothing unless you say
        /// otherwise.
        #[arg(long, conflicts_with = "expects_reply")]
        no_reply: bool,
        /// A progress note on the request `--reply-to` names, which you're
        /// still working on: it shows under the request without answering it.
        #[arg(long, requires = "reply_to", conflicts_with = "expects_reply")]
        progress: bool,
    },
    /// Send a request, wait for its reply, print it, and exit with its status.
    Call {
        /// The recipient, as name@machine.
        to: String,
        /// The message. Read from stdin when left out or `-`. One over 64
        /// KiB is sent as an attachment.
        body: Option<String>,
        /// Limit the run to these of the tools the agent's config allows.
        #[arg(long, value_delimiter = ',', value_name = "NAMES")]
        tools: Option<Vec<String>>,
        /// `fresh` or `per-sender`, if the agent's config allows it.
        #[arg(long, value_name = "MODE")]
        session: Option<String>,
        /// How long the run can take, up to the agent's configured maximum.
        #[arg(long, value_name = "SECS")]
        run_timeout: Option<u64>,
        /// Run in this directory, if the agent's config allows it.
        #[arg(long, value_name = "DIR")]
        workdir: Option<String>,
        /// Run on this model, if the agent's config allows it.
        #[arg(long, value_name = "NAME")]
        model: Option<String>,
        /// Think this hard, such as `high`, if the agent's config allows it.
        #[arg(long, value_name = "LEVEL")]
        effort: Option<String>,
        /// A file to attach. Can be given more than once.
        #[arg(long, value_name = "FILE")]
        attach: Vec<PathBuf>,
        /// How long this command waits for the reply, and then cancels. It
        /// counts time spent queued behind the agent's other runs. To limit
        /// only the run, use --run-timeout and leave this out.
        #[arg(long, value_name = "SECS")]
        timeout: Option<u64>,
        /// Leave the run going if this command is interrupted or times out.
        #[arg(long)]
        no_cancel: bool,
        /// Print the reply and its details as JSON.
        #[arg(long)]
        json: bool,
        /// Also print the run's ID and what it used to stderr.
        #[arg(long)]
        verbose: bool,
    },
    /// Cancel a run of yours, by its ID.
    Cancel { run_id: String },
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
    /// Count the tokens a Claude Code session used, with its subagents'.
    Usage {
        /// The session's ID, or its transcript's path.
        session: String,
        /// Count only from this time, such as 2026-10-27T17:51:33Z, or from
        /// when this message was sent.
        #[arg(long, value_name = "TIME OR ID")]
        since: Option<String>,
    },
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
    /// Review a change with adversarial convergence review.
    Acr {
        #[command(subcommand)]
        command: acr::AcrCmd,
    },
    /// Answer an agent's messages with a headless CLI agent.
    Bridge {
        #[command(subcommand)]
        command: BridgeCmd,
    },
}

#[derive(Subcommand)]
enum AgentCmd {
    /// Subscribe an agent, so messages can queue before its first session.
    Add { name: String },
    /// List the agents subscribed on this machine.
    List,
    /// Remove an agent nobody's using any more. Messages waiting for it go
    /// stale, and their senders hear.
    #[command(alias = "remove")]
    Rm { name: String },
    /// Move an agent to a new name, such as after starting its session
    /// again as another. Its messages, history and open requests go with it,
    /// nothing goes stale, and the old name leaves every peer list. Replies
    /// and messages still sent to the old name reach the new one.
    #[command(alias = "migrate")]
    Mv {
        /// The name it had.
        from: String,
        /// The name it has now. Left out, the agent you act as (`--as` or
        /// ACRBUS_AGENT), as in that agent's own session.
        to: Option<String>,
    },
}

#[derive(Subcommand)]
enum HookCmd {
    SessionStart,
    Prompt,
    Wait,
    Permission,
    ToolDone,
    Subagent,
}

#[derive(Subcommand)]
enum SetupApp {
    /// Install the Claude Code plugin: the bus tools, and push into sessions.
    Claude {
        /// Only write the plugin's files; don't run `claude plugin install`.
        #[arg(long)]
        no_install: bool,
    },
    /// Set up ACR on this machine: headless Claude Code as its agent, its
    /// token, its bindings and its service. Safe to run again.
    Acr {
        /// A repository ACR may review here. Can be repeated.
        #[arg(long = "repo", value_name = "PATH")]
        repos: Vec<PathBuf>,
    },
    /// Everything: the daemon's service, the Claude Code plugin, and ACR.
    All {
        /// A repository ACR may review here. Can be repeated.
        #[arg(long = "repo", value_name = "PATH")]
        repos: Vec<PathBuf>,
    },
}

#[derive(Subcommand)]
enum BridgeCmd {
    /// Run a bridge in the foreground.
    Run { agent: String },
    /// Start a bridge now and at every login, replacing a running one.
    Install {
        agent: String,
        /// Restart at once, cutting short its runs under way.
        #[arg(long)]
        now: bool,
    },
    /// Stop a bridge and stop starting it at login.
    Uninstall { agent: String },
    /// Show how the bridges in the config are doing.
    Status { agent: Option<String> },
    /// Show what's kept of a run: its record, or its output.
    Log {
        agent: String,
        run_id: String,
        /// Print its stdout instead.
        #[arg(long, conflicts_with = "stderr")]
        stdout: bool,
        /// Print its stderr instead.
        #[arg(long)]
        stderr: bool,
    },
}

#[derive(Subcommand)]
enum ServiceCmd {
    /// Start the daemon now and at every login, replacing a running one. It
    /// waits for bridge runs and reviews under way to finish first.
    Install {
        /// Restart at once, cutting short what's under way.
        #[arg(long)]
        now: bool,
    },
    /// Stop the daemon and stop starting it at login.
    Uninstall,
}

pub fn main() -> ExitCode {
    match run(parse()) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("{APP_NAME}: {e:#}");
            ExitCode::FAILURE
        }
    }
}

/// The command line, named as this program, with `--as` from
/// `ACRBUS_AGENT` (or `TAILBUS_AGENT`) when it's left out.
fn parse() -> Cli {
    let app = app();
    let command = Cli::command()
        .name(app.name)
        .bin_name(app.name)
        .about(app.about);
    let mut cli = Cli::from_arg_matches(&command.get_matches()).unwrap_or_else(|e| e.exit());
    if cli.agent.is_none() {
        cli.agent = env_var("AGENT");
    }
    cli
}

/// The agent the CLI or MCP server acts as. A name given that isn't valid,
/// such as `WebApp`, is made valid the same way the hooks make it, so a
/// session's server and its hooks are always the same agent: a server that
/// refused it left the session getting messages it couldn't answer.
fn agent_name(explicit: Option<String>) -> Result<String> {
    match explicit {
        Some(name) => Ok(util::as_agent_name(&name)),
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
            let client = env_var("CLIENT");
            mcp::run(
                socket,
                agent,
                session,
                std::os::unix::process::parent_id(),
                client,
            )?;
        }
        Cmd::Hook { command } => {
            let explicit = cli.agent.as_deref().map(util::as_agent_name);
            return match command {
                HookCmd::SessionStart => hook::session_start(&socket, explicit),
                HookCmd::Prompt => hook::prompt(&socket, explicit),
                HookCmd::Wait => hook::wait(&socket, explicit),
                HookCmd::Permission => hook::permission(&socket, explicit),
                HookCmd::ToolDone => hook::tool_done(&socket, explicit),
                HookCmd::Subagent => hook::subagent(&socket, explicit),
            };
        }
        Cmd::Setup { app } => match app {
            SetupApp::Claude { no_install } => setup::claude(!no_install)?,
            SetupApp::Acr { repos } => setup::acr(&paths, &repos)?,
            SetupApp::All { repos } => {
                setup::service_install(&paths, false)?;
                setup::claude(true)?;
                setup::acr(&paths, &repos)?;
            }
        },
        Cmd::Service { command } => match command {
            ServiceCmd::Install { now } => setup::service_install(&paths, now)?,
            ServiceCmd::Uninstall => setup::service_uninstall(&paths)?,
        },
        Cmd::Acr { command } => return acr::run(&paths, command),
        Cmd::Bridge { command } => match command {
            BridgeCmd::Run { agent } => bridge::run(&paths, &agent)?,
            BridgeCmd::Install { agent, now } => setup::bridge_install(&paths, &agent, now)?,
            BridgeCmd::Uninstall { agent } => setup::bridge_uninstall(&paths, &agent)?,
            BridgeCmd::Status { agent } => bridge::status(&paths, agent.as_deref())?,
            BridgeCmd::Log {
                agent,
                run_id,
                stdout,
                stderr,
            } => {
                let part = match (stdout, stderr) {
                    (true, _) => Some("stdout"),
                    (_, true) => Some("stderr"),
                    _ => None,
                };
                bridge::show_log(&paths, &agent, &run_id, part)?;
            }
        },
        Cmd::Call {
            to,
            body,
            tools,
            session,
            run_timeout,
            workdir,
            model,
            effort,
            attach,
            timeout,
            no_cancel,
            json,
            verbose,
        } => {
            let args = call::Args {
                to,
                body,
                options: RunOptions {
                    tools,
                    session,
                    timeout: run_timeout,
                    workdir,
                    model,
                    effort,
                    log: None,
                    review: None,
                },
                attach,
                timeout,
                no_cancel,
                json,
                verbose,
            };
            return Ok(call::call(&paths, agent_name(cli.agent)?, args));
        }
        Cmd::Cancel { run_id } => return Ok(call::cancel(&paths, agent_name(cli.agent)?, &run_id)),
        Cmd::Send {
            to,
            body,
            reply_to,
            expects_reply,
            no_reply,
            progress,
        } => {
            let expects_reply = match (expects_reply, no_reply) {
                (true, _) => Some(true),
                (_, true) => Some(false),
                _ => None,
            };
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
            let request = match reply_to {
                Some(reply_to) if progress => Request::Progress {
                    agent: agent_name(cli.agent)?,
                    reply_to,
                    body,
                },
                reply_to => Request::Send {
                    agent: agent_name(cli.agent)?,
                    to,
                    body,
                    reply_to,
                    expects_reply,
                    pipe: None,
                    ext: None,
                },
            };
            let sent: Sent = local::call(&socket, &request)?;
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
            )?
            .messages;
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
            AgentCmd::Rm { name } => {
                let removed = call(&Request::AgentRemove { name: name.clone() })?;
                let stale = removed["stale"].as_u64().unwrap_or(0);
                println!("Removed {name}; {stale} message(s) waiting for it went stale.");
            }
            AgentCmd::Mv { from, to } => {
                let Some(to) = to.or_else(|| cli.agent.clone()) else {
                    bail!(
                        "say what {from} is called now: {APP_NAME} agent mv {from} <name>, \
                         or run it in that agent's session"
                    );
                };
                let to = agent_name(Some(to))?;
                let moved: Moved = local::call(
                    &socket,
                    &Request::AgentMove {
                        from: from.clone(),
                        to: to.clone(),
                    },
                )?;
                println!(
                    "Moved {from} to {to}, with {} waiting message(s). {from} is gone from \
                     every peer list, and what's still sent to it reaches {to}.",
                    moved.waiting
                );
                for note in moved.notes {
                    println!("Note: {note}");
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
                "{APP_NAME} {} on {} ({})",
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
                    "  {}  {}{}, {} open{}",
                    a.name,
                    if a.live { "live" } else { "idle" },
                    a.app
                        .as_deref()
                        .map(|a| format!(" ({a})"))
                        .unwrap_or_default(),
                    a.open,
                    if a.paused {
                        ", paused until its user types in its session"
                    } else {
                        ""
                    }
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
        Cmd::Usage { session, since } => {
            let path = if session.ends_with(".jsonl") {
                PathBuf::from(&session)
            } else {
                usage::transcript(&session)
                    .ok_or_else(|| anyhow!("found no transcript for session {session}"))?
            };
            let since = match since.as_deref() {
                None => 0,
                Some(s) => util::id_ms(s)
                    .or_else(|| util::parse_rfc3339(s))
                    .ok_or_else(|| {
                        anyhow!(
                            "--since takes a time, such as 2026-10-27T17:51:33Z, or a message ID"
                        )
                    })?,
            };
            let used = usage::measure(&path, since)?;
            if used.total().responses == 0 {
                println!("No token use recorded.");
            } else {
                println!("{}", usage::describe(&used));
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}
