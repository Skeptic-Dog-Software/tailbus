//! Claude Code's push adapter: hook commands run by the tailbus plugin.
//!
//! `wait` runs as an `asyncRewake` hook, armed at session start and after
//! every turn. It blocks on the bus, and when a message arrives it prints it
//! and exits with status 2, which shows it to Claude and wakes the session
//! even when it's idle.

use std::env;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::thread;
use std::time::Duration;

use anyhow::Result;
use serde::Deserialize;

use crate::format::{self, Tools};
use crate::local::{self, Client, Inbox, Request, Woken};
use crate::util::derive_agent_name;

/// Wakes Claude Code and shows it what the hook printed.
const REWAKE: u8 = 2;

#[derive(Default, Deserialize)]
struct Input {
    #[serde(default)]
    session_id: String,
    #[serde(default)]
    cwd: String,
}

fn input() -> Input {
    let mut text = String::new();
    let _ = io::stdin().read_to_string(&mut text);
    serde_json::from_str(&text).unwrap_or_default()
}

/// Grok Build also runs the hooks in Claude Code's settings, so these do
/// nothing unless Claude Code itself is the caller.
fn in_claude_code() -> bool {
    env::var("CLAUDECODE").is_ok_and(|v| v == "1")
}

/// `claude -p` waits for a running `asyncRewake` hook instead of exiting,
/// so the wait stands down in sessions nobody is attending. Claude Code sets
/// this to 0 for `-p` runs and 1 for interactive sessions.
fn unattended() -> bool {
    env::var("CLAUDE_CODE_SESSION_ATTENDED").is_ok_and(|v| v == "0")
}

fn agent_for(explicit: Option<String>, input: &Input) -> String {
    explicit.unwrap_or_else(|| {
        let dir = env::var_os("CLAUDE_PROJECT_DIR")
            .map(PathBuf::from)
            .or_else(|| (!input.cwd.is_empty()).then(|| PathBuf::from(&input.cwd)))
            .or_else(|| env::current_dir().ok())
            .unwrap_or_default();
        derive_agent_name(&dir)
    })
}

fn session_of(input: &Input) -> String {
    if input.session_id.is_empty() {
        env::var("CLAUDE_CODE_SESSION_ID").unwrap_or_default()
    } else {
        input.session_id.clone()
    }
}

/// SessionStart: mentions messages handed to an earlier session and not yet
/// handled. New ones arrive through `wait`.
pub fn session_start(socket: &Path, explicit: Option<String>) -> Result<ExitCode> {
    if !in_claude_code() {
        return Ok(ExitCode::SUCCESS);
    }
    let input = input();
    let agent = agent_for(explicit, &input);
    let Ok(inbox) = local::call::<Inbox>(
        socket,
        &Request::Inbox {
            agent: agent.clone(),
        },
    ) else {
        return Ok(ExitCode::SUCCESS);
    };
    let earlier = inbox
        .incoming
        .iter()
        .filter(|i| i.state == "pending")
        .count();
    if earlier > 0 {
        println!(
            "tailbus: {earlier} message{} to you ({agent}@{}) from other agents {} read \
             earlier but not answered or marked handled. msg_inbox lists them.",
            if earlier == 1 { "" } else { "s" },
            inbox.machine,
            if earlier == 1 { "was" } else { "were" },
        );
    }
    Ok(ExitCode::SUCCESS)
}

/// The background wait: exits 2 with the messages when some arrive, or 0
/// when a newer wait replaces this one.
pub fn wait(socket: &Path, explicit: Option<String>) -> Result<ExitCode> {
    if !in_claude_code() || unattended() {
        return Ok(ExitCode::SUCCESS);
    }
    let input = input();
    let agent = agent_for(explicit, &input);
    let session = session_of(&input);
    let client_pid = std::os::unix::process::parent_id();
    loop {
        // A daemon that's down or restarting just means waiting longer;
        // Claude Code's timeout for the hook ends it eventually.
        let Ok(mut client) = Client::connect(socket) else {
            thread::sleep(Duration::from_secs(5));
            continue;
        };
        let woken = client.call::<Woken>(&Request::Wait {
            agent: agent.clone(),
            session: session.clone(),
            client_pid,
        });
        match woken {
            Ok(Woken::Messages { messages }) => {
                eprint!("{}", format::messages_for_agent(&messages, Tools::Mcp));
                let _ = client.call::<serde_json::Value>(&Request::Confirm);
                return Ok(ExitCode::from(REWAKE));
            }
            Ok(Woken::Replaced | Woken::Refused { .. }) => return Ok(ExitCode::SUCCESS),
            Err(_) => thread::sleep(Duration::from_secs(2)),
        }
    }
}
