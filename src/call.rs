//! `tailbus call` and `tailbus cancel`: use an agent the way a script uses a
//! command. A call sends a request, waits for its own reply, prints it, and
//! exits with a code that says how it went.

use std::fs::File;
use std::io::{self, IsTerminal, Read};
use std::path::{Path, PathBuf};
use std::process::{self, ExitCode};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use serde_json::json;

use crate::attach::Attachments;
use crate::bridge::on_stop_signal;
use crate::config::{Config, Paths};
use crate::local::{self, Awaited, Client, Item, Request, Sent};
use crate::proto::{Extras, MAX_BODY, RunOptions, RunStatus};

/// The run was `failed`.
const FAILED: u8 = 1;
/// The run hit its timeout, as the `timeout` command's own code says.
const TIMED_OUT: u8 = 124;
/// This call's own `--timeout` passed first.
const WAIT_TIMED_OUT: u8 = 125;
/// The agent refused it, or marked it handled without answering.
const REFUSED: u8 = 77;
/// The run was cancelled, as a shell reports an interrupt.
const CANCELLED: u8 = 130;
/// Interrupted with `--no-cancel`, so the run goes on.
const LEFT_RUNNING: u8 = 131;
/// It never reached the agent: the bus is down, or it couldn't be delivered.
const UNDELIVERED: u8 = 69;
/// A local problem before anything was sent, such as a file too large.
const LOCAL_PROBLEM: u8 = 66;

/// How long a cancelled run gets to report back before the call gives up.
const CANCEL_WAIT: Duration = Duration::from_secs(15);
/// With no `--timeout`, a call waits as long as the bus lets it.
const FOREVER: u64 = 7 * 86_400;
const RECONNECT_EVERY: Duration = Duration::from_secs(1);

pub struct Args {
    pub to: String,
    pub body: Option<String>,
    pub options: RunOptions,
    pub attach: Vec<PathBuf>,
    pub timeout: Option<u64>,
    pub no_cancel: bool,
    pub json: bool,
    pub verbose: bool,
}

/// The exit code for a run's status.
pub fn exit_code(status: RunStatus) -> u8 {
    match status {
        RunStatus::Ok => 0,
        RunStatus::Failed => FAILED,
        RunStatus::Timeout => TIMED_OUT,
        RunStatus::Refused => REFUSED,
        RunStatus::Cancelled => CANCELLED,
    }
}

fn say(code: u8, why: impl std::fmt::Display) -> ExitCode {
    eprintln!("tailbus: {why}");
    ExitCode::from(code)
}

fn read_body(body: Option<String>) -> Result<String> {
    match body.as_deref() {
        None | Some("-") => {
            if io::stdin().is_terminal() {
                eprintln!("Type the message, then Ctrl-D:");
            }
            let mut text = String::new();
            io::stdin().read_to_string(&mut text)?;
            Ok(text)
        }
        Some(text) => Ok(text.to_string()),
    }
}

fn attachments(paths: &Paths) -> Result<Attachments> {
    let cfg = Config::load(&paths.config)?;
    Ok(Attachments::new(
        paths.attachments(),
        cfg.max_attachment_bytes,
        cfg.attachment_store_bytes,
    ))
}

/// What goes beside a call's text. A message too long to send is attached
/// instead, so nothing's cut.
fn extras(
    paths: &Paths,
    options: RunOptions,
    files: &[PathBuf],
    body: &mut String,
) -> Result<Extras> {
    let store = attachments(paths)?;
    let mut attached = Vec::new();
    for file in files {
        attached.push(store.add_file(file)?);
    }
    if body.len() > MAX_BODY {
        let size = body.len() as u64;
        attached.push(store.add("message.txt", &mut body.as_bytes(), size)?);
        *body = format!("The full message, {size} bytes, is in the attached file message.txt.");
    }
    Ok(Extras {
        options: (options != RunOptions::default()).then_some(options),
        attachments: attached,
        ..Extras::default()
    })
}

/// Reads the rest of a call's wait, coming back to it if the daemon
/// restarts.
fn wait(
    socket: &Path,
    mut client: Client,
    agent: &str,
    id: &str,
    until: Instant,
) -> Result<Awaited> {
    loop {
        match client.reply::<Awaited>() {
            Ok(got) => return Ok(got),
            Err(e) if e.is::<local::Refused>() => return Err(e),
            Err(_) => {}
        }
        client = loop {
            if Instant::now() >= until {
                return Ok(Awaited::TimedOut);
            }
            thread::sleep(RECONNECT_EVERY);
            let Ok(mut client) = Client::connect(socket) else {
                continue;
            };
            let left = until.saturating_duration_since(Instant::now());
            let again = client.send(&Request::Await {
                agent: agent.to_string(),
                id: id.to_string(),
                wait_secs: left.as_secs().max(1),
            });
            if again.is_ok() {
                break client;
            }
        };
    }
}

/// Prints a reply, and returns how the call exits.
fn show(item: &Item, args: &Args) -> Result<u8> {
    let msg = &item.msg;
    let result = msg.result();
    let code = result.map_or(0, |r| exit_code(r.status));
    if args.json {
        let out = json!({
            "status": result.map_or("ok", |r| r.status.as_str()),
            "run_id": result.map_or(msg.reply_to.as_deref(), |r| Some(r.run_id.as_str())),
            "exit_code": result.and_then(|r| r.exit_code),
            "duration_ms": result.map(|r| r.duration_ms),
            "stderr_tail": result.and_then(|r| r.stderr_tail.as_deref()),
            "applied": result.and_then(|r| r.applied.as_ref()),
            "usage": result.and_then(|r| r.usage.as_ref()),
            "body": msg.body,
            "attachments": msg.attachments(),
        });
        println!("{out}");
        return Ok(code);
    }
    let full = result.and_then(|r| r.full_reply.as_ref());
    let file = full.and_then(|sha| msg.attachments().iter().find(|a| &a.sha256 == sha));
    match file.and_then(|a| a.path.as_ref()) {
        Some(path) => {
            let mut file = File::open(path).with_context(|| format!("reading {path}"))?;
            io::copy(&mut file, &mut io::stdout())?;
        }
        None => println!("{}", msg.body),
    }
    if let Some(tail) = result.and_then(|r| r.stderr_tail.as_ref()) {
        eprintln!("{tail}");
    }
    for a in msg.attachments().iter().filter(|a| Some(&a.sha256) != full) {
        eprintln!(
            "tailbus: attached {} at {}",
            a.name,
            a.path.as_deref().unwrap_or("nowhere on this machine")
        );
    }
    if let (true, Some(r)) = (args.verbose, result) {
        eprintln!(
            "tailbus: run {}: {}, {} ms, {}",
            r.run_id,
            r.status.as_str(),
            r.duration_ms,
            serde_json::to_string(&r.applied)?
        );
        if let Some(u) = &r.usage {
            let usd = u
                .usd
                .map_or(String::new(), |usd| format!(", ${usd:.4} by its count"));
            eprintln!(
                "tailbus: used {} model call(s): {} tokens in ({} read from cache, {} written to it), {} out{usd}",
                u.calls,
                u.input_tokens + u.cache_read_tokens + u.cache_write_tokens,
                u.cache_read_tokens,
                u.cache_write_tokens,
                u.output_tokens
            );
        }
    }
    Ok(code)
}

fn cancel_run(socket: &Path, agent: &str, run: &str) -> Result<Sent> {
    local::call(
        socket,
        &Request::Cancel {
            agent: agent.to_string(),
            run_id: run.to_string(),
        },
    )
}

/// On an interrupt, cancels the run unless told not to, and leaves the call
/// to report how that went. If it can't, this ends the call.
fn watch_for_interrupt(
    socket: PathBuf,
    agent: String,
    run: Arc<Mutex<Option<String>>>,
    no_cancel: bool,
) {
    let watched = on_stop_signal(move |_| {
        let id = run.lock().ok().and_then(|r| r.clone());
        let Some(id) = id else {
            process::exit(i32::from(CANCELLED));
        };
        if no_cancel {
            eprintln!("tailbus: run {id} is still going");
            process::exit(i32::from(LEFT_RUNNING));
        }
        if let Err(e) = cancel_run(&socket, &agent, &id) {
            eprintln!("tailbus: couldn't cancel run {id}: {e:#}");
            process::exit(i32::from(UNDELIVERED));
        }
        thread::sleep(CANCEL_WAIT);
        eprintln!("tailbus: run {id} hasn't said it stopped");
        process::exit(i32::from(CANCELLED));
    });
    if let Err(e) = watched {
        eprintln!("tailbus: an interrupt won't cancel this run: {e:#}");
    }
}

pub fn call(paths: &Paths, agent: String, mut args: Args) -> ExitCode {
    let mut body = match read_body(args.body.take()) {
        Ok(body) => body,
        Err(e) => return say(LOCAL_PROBLEM, format!("{e:#}")),
    };
    let ext = match extras(
        paths,
        std::mem::take(&mut args.options),
        &args.attach,
        &mut body,
    ) {
        Ok(ext) => ext,
        Err(e) => return say(LOCAL_PROBLEM, format!("{e:#}")),
    };
    let socket = paths.socket();
    let run = Arc::new(Mutex::new(None));
    watch_for_interrupt(socket.clone(), agent.clone(), run.clone(), args.no_cancel);
    match request(&socket, &agent, body, ext, &args, &run) {
        Ok(code) => ExitCode::from(code),
        Err(e) => say(UNDELIVERED, format!("{e:#}")),
    }
}

pub(crate) fn request(
    socket: &Path,
    agent: &str,
    body: String,
    ext: Extras,
    args: &Args,
    run: &Mutex<Option<String>>,
) -> Result<u8> {
    let wait_secs = args.timeout.unwrap_or(FOREVER);
    let until = Instant::now() + Duration::from_secs(wait_secs);
    let mut client = Client::connect(socket)?;
    client.send(&Request::Call {
        agent: agent.to_string(),
        to: args.to.clone(),
        body,
        reply_to: None,
        expects_reply: Some(true),
        ext: Some(Box::new(ext)),
        wait_secs: Some(wait_secs),
    })?;
    let sent: Sent = client.reply()?;
    if let Ok(mut run) = run.lock() {
        *run = Some(sent.id.clone());
    }
    if args.verbose {
        eprintln!("tailbus: run {}", sent.id);
    }
    match wait(socket, client, agent, &sent.id, until)? {
        Awaited::Reply { item } => show(&item, args),
        Awaited::Undelivered { why } => {
            eprintln!("tailbus: {why}");
            Ok(UNDELIVERED)
        }
        Awaited::Unanswered => {
            eprintln!(
                "tailbus: {} marked it handled without answering. A bridge does that with \
                 requests from anyone but its owner.",
                sent.to
            );
            Ok(REFUSED)
        }
        Awaited::TimedOut => {
            eprintln!("tailbus: no reply within {wait_secs} seconds");
            if !args.no_cancel {
                cancel_run(socket, agent, &sent.id)
                    .map_err(|e| eprintln!("tailbus: couldn't cancel run {}: {e:#}", sent.id))
                    .ok();
            }
            Ok(WAIT_TIMED_OUT)
        }
    }
}

/// Cancels one of `agent`'s runs, and says how the bridge took it.
pub fn cancel(paths: &Paths, agent: String, run: &str) -> ExitCode {
    let socket = paths.socket();
    let asked = cancel_run(&socket, &agent, run).and_then(|sent| {
        let mut client = Client::connect(&socket)?;
        client.call::<Awaited>(&Request::Await {
            agent: agent.clone(),
            id: sent.id,
            wait_secs: CANCEL_WAIT.as_secs(),
        })
    });
    match asked {
        Ok(Awaited::Reply { item }) => {
            println!("{}", item.msg.body);
            ExitCode::from(item.msg.result().map_or(0, |r| exit_code(r.status)))
        }
        Ok(Awaited::Undelivered { why }) => say(UNDELIVERED, why),
        Ok(Awaited::Unanswered | Awaited::TimedOut) => {
            say(UNDELIVERED, anyhow!("the agent hasn't answered"))
        }
        Err(e) if e.is::<local::Refused>() => say(REFUSED, format!("{e:#}")),
        Err(e) => say(UNDELIVERED, format!("{e:#}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn every_status_has_its_own_exit_code() {
        let statuses = [
            RunStatus::Ok,
            RunStatus::Failed,
            RunStatus::Timeout,
            RunStatus::Refused,
            RunStatus::Cancelled,
        ];
        let mut codes: HashSet<u8> = statuses.into_iter().map(exit_code).collect();
        assert_eq!(codes.len(), statuses.len());
        assert_eq!(exit_code(RunStatus::Ok), 0);
        // Nor do the call's own outcomes pass for one of them.
        for own in [WAIT_TIMED_OUT, LEFT_RUNNING, UNDELIVERED, LOCAL_PROBLEM] {
            assert!(codes.insert(own), "{own}");
        }
        // Or for a usage error, which is clap's 2.
        assert!(!codes.contains(&2));
    }
}
