//! `tailbus setup claude` and `tailbus service`: wiring tailbus into Claude
//! Code, and running the daemon and bridges under launchd.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::json;

use crate::bridge;
use crate::config::Paths;
use crate::local::{self, Hello, Request};

const LABEL: &str = "com.skepticdogsoftware.tailbus";
/// The wait hook's timeout. Claude Code enforces it on `asyncRewake` hooks,
/// and once it passes an idle session can't be woken until its next turn.
const WAIT_HOOK_TIMEOUT_SECS: u64 = 7 * 86_400;
/// How long the permission hook holds a prompt for an alert's answer. If it
/// passes, the prompt stays with the terminal.
const PERMISSION_HOOK_TIMEOUT_SECS: u64 = 86_400;
/// How long launchd gives a job to stop, unless it says otherwise.
const LAUNCHD_EXIT_TIMEOUT: u64 = 20;

fn home() -> Result<PathBuf> {
    env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
        .context("HOME is not set")
}

/// This binary, which the plugin and the service run.
fn this_exe() -> Result<PathBuf> {
    let exe = env::current_exe()?.canonicalize()?;
    if exe.components().any(|c| c.as_os_str() == "target") {
        eprintln!(
            "Note: {} looks like a build directory. Install tailbus first \
             (`cargo install --path .`) so this points at a binary that stays put.",
            exe.display()
        );
    }
    Ok(exe)
}

fn write(path: &Path, text: &str) -> Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    fs::write(path, text).with_context(|| format!("writing {}", path.display()))
}

fn pretty(v: &serde_json::Value) -> String {
    serde_json::to_string_pretty(v).unwrap_or_default() + "\n"
}

/// Writes the Claude Code plugin: the MCP server, and the hooks that push
/// messages into sessions. It lives in a local marketplace, so Claude Code
/// loads it in place.
pub fn claude(install: bool) -> Result<()> {
    let exe = this_exe()?;
    let exe = exe.to_string_lossy();
    let data = env::var_os("XDG_DATA_HOME")
        .filter(|d| !d.is_empty())
        .map_or_else(
            || home().map(|h| h.join(".local/share")),
            |d| Ok(PathBuf::from(d)),
        )?;
    let root = data.join("tailbus").join("claude");
    let plugin = root.join("tailbus");
    let description = "Direct messages between AI agents on your tailnet";

    write(
        &root.join(".claude-plugin/marketplace.json"),
        &pretty(&json!({
            "name": "tailbus",
            "owner": {"name": "Skeptic Dog Software"},
            "plugins": [{"name": "tailbus", "source": "./tailbus", "description": description}]
        })),
    )?;
    write(
        &plugin.join(".claude-plugin/plugin.json"),
        &pretty(&json!({
            "name": "tailbus",
            "version": env!("CARGO_PKG_VERSION"),
            "description": description,
            "repository": "https://github.com/Skeptic-Dog-Software/tailbus"
        })),
    )?;
    write(
        &plugin.join(".mcp.json"),
        &pretty(&json!({"mcpServers": {"tailbus": {"command": exe, "args": ["mcp"]}}})),
    )?;
    let wait = json!({
        "type": "command", "command": exe, "args": ["hook", "wait"],
        "asyncRewake": true, "timeout": WAIT_HOOK_TIMEOUT_SECS
    });
    // Fired after every tool call, so it doesn't hold the session up.
    let done = json!({
        "type": "command", "command": exe, "args": ["hook", "tool-done"],
        "async": true, "timeout": 10
    });
    write(
        &plugin.join("hooks/hooks.json"),
        &pretty(&json!({
            "description": "tailbus: show messages from other agents, waking the session when it's idle",
            "hooks": {
                "SessionStart": [{"hooks": [
                    {"type": "command", "command": exe, "args": ["hook", "session-start"], "timeout": 10},
                    wait.clone()
                ]}],
                "UserPromptSubmit": [{"hooks": [
                    {"type": "command", "command": exe, "args": ["hook", "prompt"], "timeout": 10}
                ]}],
                "PermissionRequest": [{"hooks": [
                    {"type": "command", "command": exe, "args": ["hook", "permission"],
                     "timeout": PERMISSION_HOOK_TIMEOUT_SECS}
                ]}],
                // Holds a /pipe task to its cap on subagents running at once.
                "PreToolUse": [{"matcher": "Agent|Task", "hooks": [
                    {"type": "command", "command": exe, "args": ["hook", "subagent"], "timeout": 10}
                ]}],
                "PostToolUse": [{"hooks": [done.clone()]}],
                "PostToolUseFailure": [{"hooks": [done]}],
                "Stop": [{"hooks": [wait]}]
            }
        })),
    )?;
    // The /pipe command, and the ACR method its workers follow.
    write(&plugin.join("commands/pipe.md"), PIPE)?;
    write(
        &plugin.join("skills/acr/SKILL.md"),
        include_str!("../plugin/skills/acr/SKILL.md"),
    )?;
    println!("Wrote the Claude Code plugin to {}", plugin.display());
    install_pipe_skill()?;

    if !install {
        println!(
            "To install it:\n  claude plugin marketplace add {}\n  claude plugin install tailbus@tailbus",
            root.display()
        );
        return Ok(());
    }
    let claude = find_claude()?;
    let added = Command::new(&claude)
        .args(["plugin", "marketplace", "add"])
        .arg(&root)
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("running {}", claude.display()))?;
    let said = String::from_utf8_lossy(&added.stdout).to_string()
        + &String::from_utf8_lossy(&added.stderr);
    if !added.status.success() && !said.contains("already") {
        bail!("`claude plugin marketplace add` failed: {}", said.trim());
    }
    let installed = Command::new(&claude)
        .args(["plugin", "install", "tailbus@tailbus"])
        .stdin(Stdio::null())
        .output()?;
    let said = String::from_utf8_lossy(&installed.stdout).to_string()
        + &String::from_utf8_lossy(&installed.stderr);
    if !installed.status.success() && !said.contains("already") {
        bail!("`claude plugin install` failed: {}", said.trim());
    }
    println!(
        "Installed the tailbus plugin. Sessions started from now on load it. Restart \
         sessions that are already open: /reload-plugins rereads the plugin's files, but \
         keeps a tailbus server that's already running on the old version."
    );
    Ok(())
}

const PIPE: &str = include_str!("../plugin/commands/pipe.md");
/// Marks the personal `/pipe` skill as tailbus's, so setup replaces only
/// its own.
const PIPE_MARK: &str = "<!-- Installed by `tailbus setup claude`, which rewrites this file. -->";

/// The `/pipe` command as a personal skill. A plugin's commands always
/// carry the plugin's name, as `/tailbus:pipe`, and a personal skill is the
/// way to have a plain `/pipe`. Only your user starts it.
fn pipe_skill() -> String {
    let rest = PIPE.strip_prefix("---\n").unwrap_or(PIPE);
    let (front, body) = rest.split_once("\n---\n").unwrap_or(("", rest));
    format!("---\nname: pipe\ndisable-model-invocation: true\n{front}\n---\n{PIPE_MARK}\n{body}")
}

fn install_pipe_skill() -> Result<()> {
    let config = env::var_os("CLAUDE_CONFIG_DIR")
        .filter(|d| !d.is_empty())
        .map_or_else(
            || home().map(|h| h.join(".claude")),
            |d| Ok(PathBuf::from(d)),
        )?;
    let path = config.join("skills/pipe/SKILL.md");
    if let Ok(existing) = fs::read_to_string(&path)
        && !existing.contains(PIPE_MARK)
    {
        println!(
            "Left {} alone, since tailbus didn't write it. /tailbus:pipe works either way.",
            path.display()
        );
        return Ok(());
    }
    write(&path, &pipe_skill())?;
    println!("Wrote {}, so /pipe works in every session.", path.display());
    Ok(())
}

fn find_claude() -> Result<PathBuf> {
    if let Some(path) = env::var_os("PATH") {
        for dir in env::split_paths(&path) {
            let candidate = dir.join("claude");
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }
    let local = home()?.join(".local/bin/claude");
    if local.is_file() {
        return Ok(local);
    }
    bail!("can't find the `claude` command; install Claude Code, or use --no-install")
}

fn plist_path(label: &str) -> Result<PathBuf> {
    Ok(home()?
        .join("Library/LaunchAgents")
        .join(format!("{label}.plist")))
}

fn xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn launchctl(args: &[&str]) -> Result<std::process::Output> {
    Ok(Command::new("launchctl")
        .args(args)
        .stdin(Stdio::null())
        .output()?)
}

fn require_launchd(what: &str, instead: &str) -> Result<()> {
    if !cfg!(target_os = "macos") {
        bail!(
            "`{what}` uses launchd, so it's macOS only for now; elsewhere, run `{instead}` under your service manager"
        );
    }
    Ok(())
}

/// A launchd agent that runs this binary at login and restarts it if it
/// stops.
struct LaunchAgent<'a> {
    label: &'a str,
    args: &'a [&'a str],
    log: &'a Path,
    /// The fewest seconds between starts.
    throttle: u32,
    /// How long it gets to stop before launchd kills it, if not launchd's
    /// default.
    exit_timeout: Option<u64>,
}

impl LaunchAgent<'_> {
    fn plist(&self, exe: &Path) -> String {
        let mut env_vars = String::from(
            "        <key>PATH</key><string>/usr/bin:/bin:/usr/sbin:/sbin:/opt/homebrew/bin:/usr/local/bin</string>\n",
        );
        if let Some(home) = env::var_os("TAILBUS_HOME") {
            env_vars.push_str(&format!(
                "        <key>TAILBUS_HOME</key><string>{}</string>\n",
                xml(&home.to_string_lossy())
            ));
        }
        let args: String = self
            .args
            .iter()
            .map(|a| format!("        <string>{}</string>\n", xml(a)))
            .collect();
        let exit_timeout = self
            .exit_timeout
            .map(|s| format!("    <key>ExitTimeOut</key><integer>{s}</integer>\n"))
            .unwrap_or_default();
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key><string>{label}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{exe}</string>
{args}    </array>
    <key>EnvironmentVariables</key>
    <dict>
{env_vars}    </dict>
    <key>RunAtLoad</key><true/>
    <key>KeepAlive</key><true/>
    <key>ThrottleInterval</key><integer>{throttle}</integer>
{exit_timeout}    <key>StandardOutPath</key><string>{log}</string>
    <key>StandardErrorPath</key><string>{log}</string>
</dict>
</plist>
"#,
            label = xml(self.label),
            exe = xml(&exe.to_string_lossy()),
            throttle = self.throttle,
            log = xml(&self.log.to_string_lossy()),
        )
    }

    /// Writes the plist and starts the job, replacing a running one.
    /// Returns the launchd domain it's in.
    fn start(&self) -> Result<String> {
        let path = plist_path(self.label)?;
        write(&path, &self.plist(&this_exe()?))?;
        let path_str = path.to_string_lossy().to_string();
        // A GUI session's domain when there is one; over ssh with nobody
        // logged in at the screen, the user domain.
        for domain in domains() {
            let booted = launchctl(&["bootout", &format!("{domain}/{}", self.label)])?;
            // A job on its way out can't be replaced until it's gone, which
            // can take as long as launchd gives it.
            let wait = match booted.status.success() {
                true => self.exit_timeout.unwrap_or(LAUNCHD_EXIT_TIMEOUT) + 5,
                false => 0,
            };
            let until = Instant::now() + Duration::from_secs(wait);
            loop {
                let out = launchctl(&["bootstrap", &domain, &path_str])?;
                if out.status.success() {
                    return Ok(domain);
                }
                if Instant::now() >= until {
                    break;
                }
                thread::sleep(Duration::from_millis(250));
            }
        }
        bail!("launchctl couldn't load {}", path.display())
    }
}

fn domains() -> [String; 2] {
    let uid = unsafe { libc::getuid() };
    [format!("gui/{uid}"), format!("user/{uid}")]
}

/// Stops a launchd agent and removes its plist. Returns the plist's path.
fn stop_launch_agent(label: &str) -> Result<PathBuf> {
    for domain in domains() {
        let _ = launchctl(&["bootout", &format!("{domain}/{label}")]);
    }
    let path = plist_path(label)?;
    if path.exists() {
        fs::remove_file(&path)?;
    }
    Ok(path)
}

/// Runs the daemon as a launchd agent that starts at login and restarts if
/// it stops. Running it again replaces the running daemon, for upgrades.
pub fn service_install(paths: &Paths) -> Result<()> {
    require_launchd("tailbus service", "tailbus daemon")?;
    paths.make_state_dir()?;
    let domain = LaunchAgent {
        label: LABEL,
        args: &["daemon"],
        log: &paths.log(),
        throttle: 10,
        exit_timeout: None,
    }
    .start()?;
    let socket = paths.socket();
    let until = Instant::now() + Duration::from_secs(10);
    loop {
        match local::call::<Hello>(&socket, &Request::Hello) {
            Ok(hello) => {
                let on = if hello.machine.is_empty() {
                    "waiting for Tailscale".to_string()
                } else {
                    format!("on {}, owned by {}", hello.machine, hello.owner)
                };
                println!(
                    "tailbus {} is running {on} (launchd {domain}/{LABEL}).",
                    hello.version
                );
                println!("Log: {}", paths.log().display());
                return Ok(());
            }
            Err(e) if Instant::now() >= until => {
                bail!(
                    "launchd started the daemon, but it isn't answering ({e:#}). See {}",
                    paths.log().display()
                );
            }
            Err(_) => thread::sleep(Duration::from_millis(250)),
        }
    }
}

pub fn service_uninstall() -> Result<()> {
    let path = stop_launch_agent(LABEL)?;
    println!("Stopped the tailbus daemon and removed {}.", path.display());
    Ok(())
}

fn bridge_label(agent: &str) -> String {
    format!("{LABEL}.bridge.{agent}")
}

/// Runs a bridge as a launchd agent, the way `service_install` runs the
/// daemon. Running it again replaces the running bridge.
pub fn bridge_install(paths: &Paths, agent: &str) -> Result<()> {
    require_launchd("tailbus bridge install", "tailbus bridge run")?;
    paths.make_state_dir()?;
    let settings = bridge::load(paths, agent)?;
    let dir = bridge::state_dir(paths, agent);
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let log = dir.join("bridge.log");
    let label = bridge_label(agent);
    let since = crate::util::now_ms();
    let domain = LaunchAgent {
        label: &label,
        args: &["bridge", "run", agent],
        log: &log,
        throttle: 30,
        // Long enough for its runs' grace period and to stop what's left.
        exit_timeout: Some(settings.shutdown_grace_secs + 10),
    }
    .start()?;
    let until = Instant::now() + Duration::from_secs(10);
    while Instant::now() < until {
        if bridge::started_since(paths, agent, since) {
            println!("The {agent} bridge is running (launchd {domain}/{label}).");
            println!("Log: {}", log.display());
            return Ok(());
        }
        thread::sleep(Duration::from_millis(250));
    }
    bail!(
        "launchd started the {agent} bridge, but it isn't up. Is the tailbus daemon running? See {}",
        log.display()
    )
}

pub fn bridge_uninstall(agent: &str) -> Result<()> {
    let path = stop_launch_agent(&bridge_label(agent))?;
    println!("Stopped the {agent} bridge and removed {}.", path.display());
    Ok(())
}

/// The launchd domain a bridge's job is loaded in, if it is.
pub fn bridge_job(agent: &str) -> Option<String> {
    let label = bridge_label(agent);
    domains().into_iter().find_map(|domain| {
        let job = format!("{domain}/{label}");
        let out = launchctl(&["print", &job]).ok()?;
        out.status.success().then_some(job)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_pipe_skill_is_the_command_under_its_own_name() {
        let skill = pipe_skill();
        assert!(skill.starts_with("---\nname: pipe\ndisable-model-invocation: true\ndescription:"));
        assert!(skill.contains("argument-hint:"));
        assert!(skill.contains(&format!("---\n{PIPE_MARK}\n")));
        assert!(skill.contains("Arguments: $ARGUMENTS"));
        assert_eq!(skill.matches("\n---\n").count(), 1);
    }
}
