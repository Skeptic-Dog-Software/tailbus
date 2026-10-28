//! Claude Code's push adapter: hook commands run by the tailbus plugin.
//!
//! `wait` runs as an `asyncRewake` hook, armed at session start and after
//! every turn. It blocks on the bus, and when a message arrives it prints it
//! and exits with status 2, which shows it to Claude and wakes the session
//! even when it's idle.
//!
//! `prompt` runs as each turn starts. It tells the bus whether the user
//! typed the prompt, which starts the handoff count over, and brings along
//! messages that don't wake a session by themselves.

use std::env;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::thread;
use std::time::Duration;

use anyhow::Result;
use serde::Deserialize;

use crate::format::{self, Tools};
use crate::local::{self, Client, Inbox, Messages, Request, Verdict, Woken};
use crate::permission;
use crate::util::derive_agent_name;

/// Wakes Claude Code and shows it what the hook printed.
const REWAKE: u8 = 2;

#[derive(Default, Deserialize)]
struct Input {
    #[serde(default)]
    session_id: String,
    #[serde(default)]
    cwd: String,
    /// `UserPromptSubmit`: what starts the turn.
    #[serde(default)]
    prompt: String,
    /// Tool events: the tool and what it was called with.
    #[serde(default)]
    tool_name: String,
    #[serde(default)]
    tool_input: serde_json::Value,
    /// `PermissionRequest`: the permission updates Claude Code suggests.
    #[serde(default)]
    permission_suggestions: serde_json::Value,
    #[serde(default)]
    transcript_path: String,
    /// `PreToolUse`: the call's ID.
    #[serde(default)]
    tool_use_id: String,
}

fn input() -> Input {
    let mut text = String::new();
    let _ = io::stdin().read_to_string(&mut text);
    serde_json::from_str(&text).unwrap_or_default()
}

/// Exits when Claude Code does: it doesn't stop a hook that's still running
/// when it's killed, or when the prompt a hook holds is answered.
fn exit_when_orphaned() -> u32 {
    let client_pid = std::os::unix::process::parent_id();
    thread::spawn(move || {
        loop {
            thread::sleep(Duration::from_secs(2));
            if std::os::unix::process::parent_id() != client_pid {
                std::process::exit(0);
            }
        }
    });
    client_pid
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

/// UserPromptSubmit: reports the turn, and adds what's waiting to it as
/// context.
pub fn prompt(socket: &Path, explicit: Option<String>) -> Result<ExitCode> {
    if !in_claude_code() {
        return Ok(ExitCode::SUCCESS);
    }
    let input = input();
    // A wake from the wait hook starts a turn too, with this prompt.
    let typed = !input.prompt.trim_start().starts_with("<task-notification>");
    let Ok(mut client) = Client::connect(socket) else {
        return Ok(ExitCode::SUCCESS);
    };
    let got = client.call::<Messages>(&Request::Turn {
        agent: agent_for(explicit, &input),
        session: session_of(&input),
        client_pid: std::os::unix::process::parent_id(),
        typed,
    });
    if let Ok(got) = got
        && !got.messages.is_empty()
    {
        // Printed is handed over: Claude Code adds it to the turn.
        print!("{}", format::messages_for_agent(&got.messages, Tools::Mcp));
        let _ = io::stdout().flush();
        let _ = client.call::<serde_json::Value>(&Request::Confirm);
    }
    Ok(ExitCode::SUCCESS)
}

/// PermissionRequest: holds the prompt for an alert on the user's Macs and
/// returns the answer. Without one, it returns nothing, and the prompt
/// stays with the terminal, which is open the whole time.
pub fn permission(socket: &Path, explicit: Option<String>) -> Result<ExitCode> {
    if !in_claude_code() {
        return Ok(ExitCode::SUCCESS);
    }
    let input = input();
    if permission::ANSWERED_AT_TERMINAL.contains(&input.tool_name.as_str()) {
        return Ok(ExitCode::SUCCESS);
    }
    exit_when_orphaned();
    let Ok(mut client) = Client::connect(socket) else {
        return Ok(ExitCode::SUCCESS);
    };
    let verdict = client.call::<Verdict>(&Request::Permission {
        agent: agent_for(explicit, &input),
        session: session_of(&input),
        tool: input.tool_name.clone(),
        input: input.tool_input.clone(),
        cwd: input.cwd.clone(),
        suggestions: input.permission_suggestions.clone(),
        unattended: unattended(),
    });
    if let Ok(Verdict {
        output: Some(output),
    }) = verdict
    {
        println!("{output}");
    }
    Ok(ExitCode::SUCCESS)
}

/// PostToolUse and PostToolUseFailure: a tool ran, so a prompt for it was
/// answered at the terminal.
pub fn tool_done(socket: &Path, explicit: Option<String>) -> Result<ExitCode> {
    if !in_claude_code() {
        return Ok(ExitCode::SUCCESS);
    }
    let input = input();
    let _ = local::call::<serde_json::Value>(
        socket,
        &Request::ToolDone {
            agent: agent_for(explicit, &input),
            tool: input.tool_name.clone(),
            input: input.tool_input.clone(),
        },
    );
    Ok(ExitCode::SUCCESS)
}

/// PreToolUse, for the Agent tool: refuses a subagent beyond its `/pipe`
/// task's cap. Otherwise it says nothing, and the call goes on to the usual
/// permission check. With the daemon down there's no cap.
pub fn subagent(socket: &Path, explicit: Option<String>) -> Result<ExitCode> {
    if !in_claude_code() {
        return Ok(ExitCode::SUCCESS);
    }
    let input = input();
    let verdict = local::call::<Verdict>(
        socket,
        &Request::Subagent {
            agent: agent_for(explicit, &input),
            transcript: input.transcript_path.clone(),
            tool_use_id: input.tool_use_id.clone(),
        },
    );
    if let Ok(Verdict {
        output: Some(output),
    }) = verdict
    {
        println!("{output}");
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
    // An orphaned wait would take messages for a session that's gone.
    let client_pid = exit_when_orphaned();
    let orphaned = move || std::os::unix::process::parent_id() != client_pid;
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
                if orphaned() {
                    return Ok(ExitCode::SUCCESS);
                }
                eprint!("{}", format::messages_for_agent(&messages, Tools::Mcp));
                let _ = client.call::<serde_json::Value>(&Request::Confirm);
                return Ok(ExitCode::from(REWAKE));
            }
            Ok(Woken::Replaced | Woken::Refused { .. }) => return Ok(ExitCode::SUCCESS),
            Err(_) => thread::sleep(Duration::from_secs(2)),
        }
    }
}
