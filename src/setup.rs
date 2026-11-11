//! `tailbus setup` and `tailbus service`: wiring tailbus into Claude Code,
//! setting up ACR's agent, and running the daemon and bridges as services:
//! launchd agents on macOS, systemd user units on Linux.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::json;

use crate::bridge;
use crate::config::{APP_NAME, Paths, app, env_var};
use crate::local::{self, Hello, Request};

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
            "Note: {} looks like a build directory. Install {APP_NAME} first \
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
    let name = app().name;
    let root = data.join(name).join("claude");
    let plugin = root.join(name);
    let description = app().about;

    write(
        &root.join(".claude-plugin/marketplace.json"),
        &pretty(&json!({
            "name": name,
            "owner": {"name": "Skeptic Dog Software"},
            "plugins": [{"name": name, "source": format!("./{name}"), "description": description}]
        })),
    )?;
    write(
        &plugin.join(".claude-plugin/plugin.json"),
        &pretty(&json!({
            "name": name,
            "version": env!("CARGO_PKG_VERSION"),
            "description": description,
            "repository": format!("https://github.com/Skeptic-Dog-Software/{name}")
        })),
    )?;
    write(
        &plugin.join(".mcp.json"),
        &pretty(&json!({"mcpServers": {name: {"command": exe, "args": ["mcp"]}}})),
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
            "description": format!("{name}: show messages from other agents, waking the session when it's idle"),
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
    write(&plugin.join("commands/pipe.md"), &pipe_text())?;
    write(
        &plugin.join("skills/acr/SKILL.md"),
        include_str!("../plugin/skills/acr/SKILL.md"),
    )?;
    println!("Wrote the Claude Code plugin to {}", plugin.display());
    install_pipe_skill()?;

    if !install {
        println!(
            "To install it:\n  claude plugin marketplace add {}\n  claude plugin install {APP_NAME}@{APP_NAME}",
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
        .args(["plugin", "install"])
        .arg(format!("{name}@{name}"))
        .stdin(Stdio::null())
        .output()?;
    let said = String::from_utf8_lossy(&installed.stdout).to_string()
        + &String::from_utf8_lossy(&installed.stderr);
    if !installed.status.success() && !said.contains("already") {
        bail!("`claude plugin install` failed: {}", said.trim());
    }
    println!(
        "Installed the {APP_NAME} plugin. Sessions started from now on load it. Restart \
         sessions that are already open: /reload-plugins rereads the plugin's files, but \
         keeps a {APP_NAME} server that's already running on the old version."
    );
    Ok(())
}

const PIPE: &str = include_str!("../plugin/commands/pipe.md");

/// The `/pipe` command, naming this program where it says "tailbus".
fn pipe_text() -> String {
    PIPE.replace("tailbus", app().name)
}

/// The end of the mark on the personal `/pipe` skill that says setup wrote
/// it, so setup replaces only its own: the same whichever program wrote it.
const PIPE_MARK_END: &str = "setup claude`, which rewrites this file. -->";

fn pipe_mark() -> String {
    format!("<!-- Installed by `{APP_NAME} {PIPE_MARK_END}")
}

/// The `/pipe` command as a personal skill. A plugin's commands always
/// carry the plugin's name, as `/tailbus:pipe`, and a personal skill is the
/// way to have a plain `/pipe`. Only your user starts it.
fn pipe_skill() -> String {
    let pipe = pipe_text();
    let rest = pipe.strip_prefix("---\n").unwrap_or(&pipe);
    let (front, body) = rest.split_once("\n---\n").unwrap_or(("", rest));
    let mark = pipe_mark();
    format!("---\nname: pipe\ndisable-model-invocation: true\n{front}\n---\n{mark}\n{body}")
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
        && !existing.contains(PIPE_MARK_END)
    {
        println!(
            "Left {} alone, since {APP_NAME} didn't write it. /{APP_NAME}:pipe works either way.",
            path.display()
        );
        return Ok(());
    }
    write(&path, &pipe_skill())?;
    println!("Wrote {}, so /pipe works in every session.", path.display());
    Ok(())
}

pub fn find_claude() -> Result<PathBuf> {
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
    // Homebrew's, which a shell started without a login PATH misses.
    let brew = PathBuf::from("/opt/homebrew/bin/claude");
    if brew.is_file() {
        return Ok(brew);
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

/// Which service manager runs tailbus here.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Manager {
    Launchd,
    Systemd,
}

fn manager(what: &str, instead: &str) -> Result<Manager> {
    if cfg!(target_os = "macos") {
        return Ok(Manager::Launchd);
    }
    let systemd = Command::new("systemctl")
        .args(["--user", "--version"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if systemd {
        return Ok(Manager::Systemd);
    }
    bail!(
        "`{what}` needs launchd or a systemd user session; elsewhere, run `{instead}` under your service manager"
    )
}

fn systemctl(args: &[&str]) -> Result<()> {
    let out = Command::new("systemctl")
        .arg("--user")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .context("running systemctl")?;
    if !out.status.success() {
        bail!(
            "systemctl --user {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// A systemd user unit that runs this binary and restarts it if it stops.
struct UserUnit<'a> {
    name: &'a str,
    description: &'a str,
    args: &'a [&'a str],
    /// A unit it needs running first.
    after: Option<&'a str>,
}

fn unit_path(name: &str) -> Result<PathBuf> {
    let config = env::var_os("XDG_CONFIG_HOME")
        .filter(|d| !d.is_empty())
        .map_or_else(
            || home().map(|h| h.join(".config")),
            |d| Ok(PathBuf::from(d)),
        )?;
    Ok(config.join("systemd/user").join(format!("{name}.service")))
}

impl UserUnit<'_> {
    fn text(&self, exe: &Path) -> String {
        let after = match self.after {
            Some(unit) => format!("After={unit}.service\nRequires={unit}.service\n"),
            None => "After=network-online.target\n".to_string(),
        };
        let quoted = |s: &str| format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""));
        let command: Vec<String> = std::iter::once(exe.to_string_lossy().into_owned())
            .chain(self.args.iter().map(|a| a.to_string()))
            .map(|a| quoted(&a))
            .collect();
        // ACRBUS_HOME is what's written now; TAILBUS_HOME is still read.
        let tailbus_home = env_var("HOME")
            .map(|h| format!("Environment={}\n", quoted(&format!("ACRBUS_HOME={h}"))))
            .unwrap_or_default();
        format!(
            "[Unit]\nDescription={}\n{after}\n[Service]\nExecStart={}\n{tailbus_home}Restart=on-failure\nRestartSec=5\n\n[Install]\nWantedBy=default.target\n",
            self.description,
            command.join(" ")
        )
    }

    /// Writes the unit and starts it, replacing a running one.
    fn start(&self) -> Result<String> {
        write(&unit_path(self.name)?, &self.text(&this_exe()?))?;
        let unit = format!("{}.service", self.name);
        systemctl(&["daemon-reload"])?;
        systemctl(&["enable", &unit])?;
        systemctl(&["restart", &unit])?;
        Ok(format!("systemd --user {unit}"))
    }
}

/// Stops a systemd user unit and removes it. Returns its path.
fn stop_unit(name: &str) -> Result<PathBuf> {
    let _ = systemctl(&["disable", "--now", &format!("{name}.service")]);
    let path = unit_path(name)?;
    if path.exists() {
        fs::remove_file(&path)?;
    }
    let _ = systemctl(&["daemon-reload"]);
    Ok(path)
}

/// Without linger, a user's services stop when they log out.
fn linger_hint() {
    let user = env::var("USER").unwrap_or_default();
    let shown = Command::new("loginctl")
        .args(["show-user", &user, "-p", "Linger"])
        .stdin(Stdio::null())
        .output();
    if shown.is_ok_and(|o| String::from_utf8_lossy(&o.stdout).trim() == "Linger=no") {
        println!(
            "It runs while you're logged in. To keep it running after you log out: \
             sudo loginctl enable-linger {user}"
        );
    }
}

/// What tells this home's services from the default home's: nothing for
/// the default, or a short hash of this home's folder, so a second home on
/// the same machine, such as a test's, never replaces the first one's
/// daemon or bridges.
fn instance(paths: &Paths) -> Option<String> {
    if Paths::default_state().ok().as_deref() == Some(paths.state.as_path()) {
        return None;
    }
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(paths.state.as_os_str().as_encoded_bytes());
    Some(digest.iter().take(4).map(|b| format!("{b:02x}")).collect())
}

fn daemon_label(paths: &Paths) -> String {
    match instance(paths) {
        Some(i) => format!("{}.{i}", app().label),
        None => app().label.to_string(),
    }
}

fn bridge_label(paths: &Paths, agent: &str) -> String {
    format!("{}.bridge.{agent}", daemon_label(paths))
}

fn daemon_unit(paths: &Paths) -> String {
    match instance(paths) {
        Some(i) => format!("{}-{i}", app().name),
        None => app().name.to_string(),
    }
}

fn bridge_unit(paths: &Paths, agent: &str) -> String {
    format!("{}-bridge-{agent}", daemon_unit(paths))
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
        if let Some(home) = env_var("HOME") {
            env_vars.push_str(&format!(
                "        <key>ACRBUS_HOME</key><string>{}</string>\n",
                xml(&home)
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

/// The longest an update waits for what's under way to finish.
const QUIET_WAIT: Duration = Duration::from_secs(30 * 60);

/// What a restart would cut short now: runs on these bridges, and, if
/// `reviews`, ACR reviews on this machine.
fn under_way(paths: &Paths, bridges: &[String], reviews: bool) -> (usize, usize) {
    let runs = bridges.iter().map(|b| bridge::busy(paths, b)).sum();
    let under_way = if reviews {
        crate::acr::active_reviews(paths)
    } else {
        0
    };
    (runs, under_way)
}

/// Waits until nothing a restart would cut short is under way: runs on
/// these bridges, and, if `reviews`, ACR reviews on this machine. `now`
/// doesn't wait.
fn wait_for_quiet(paths: &Paths, bridges: &[String], reviews: bool, now: bool) -> Result<()> {
    if now {
        return Ok(());
    }
    let until = Instant::now() + QUIET_WAIT;
    let mut told = false;
    loop {
        let (runs, under_way) = under_way(paths, bridges, reviews);
        if runs + under_way == 0 {
            return Ok(());
        }
        if !told {
            println!(
                "Waiting for {runs} bridge run(s) and {under_way} review(s) under way to finish \
                 before restarting. --now restarts at once."
            );
            told = true;
        }
        if Instant::now() >= until {
            bail!(
                "still busy after {} minutes: try again later, or pass --now",
                QUIET_WAIT.as_secs() / 60
            );
        }
        thread::sleep(Duration::from_secs(2));
    }
}

/// Runs the daemon as a service that starts at login and restarts if it
/// stops. Running it again replaces the running daemon, for upgrades.
pub fn service_install(paths: &Paths, now: bool) -> Result<()> {
    let manager = manager(
        &format!("{APP_NAME} service"),
        &format!("{APP_NAME} daemon"),
    )?;
    paths.make_state_dir()?;
    // A restart drops every connection to the daemon: bridges reconnect, but
    // a run's reply or a review's call can be cut short.
    let bridges: Vec<String> = crate::config::Config::load(&paths.config)
        .map(|c| c.bridges.keys().cloned().collect())
        .unwrap_or_default();
    wait_for_quiet(paths, &bridges, true, now)?;
    let (label, unit) = (daemon_label(paths), daemon_unit(paths));
    let job = match manager {
        Manager::Launchd => {
            let domain = LaunchAgent {
                label: &label,
                args: &["daemon"],
                log: &paths.log(),
                throttle: 10,
                exit_timeout: None,
            }
            .start()?;
            format!("launchd {domain}/{label}")
        }
        Manager::Systemd => UserUnit {
            name: &unit,
            description: &format!("{APP_NAME}: {}", app().about),
            args: &["daemon"],
            after: None,
        }
        .start()?,
    };
    let log = match manager {
        Manager::Launchd => paths.log().display().to_string(),
        Manager::Systemd => format!("journalctl --user -u {unit}"),
    };
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
                println!("{APP_NAME} {} is running {on} ({job}).", hello.version);
                println!("Log: {log}");
                if manager == Manager::Systemd {
                    linger_hint();
                }
                return Ok(());
            }
            Err(e) if Instant::now() >= until => {
                bail!("{job} started the daemon, but it isn't answering ({e:#}). See {log}");
            }
            Err(_) => thread::sleep(Duration::from_millis(250)),
        }
    }
}

pub fn service_uninstall(paths: &Paths) -> Result<()> {
    let path = match manager(
        &format!("{APP_NAME} service"),
        &format!("{APP_NAME} daemon"),
    )? {
        Manager::Launchd => stop_launch_agent(&daemon_label(paths))?,
        Manager::Systemd => stop_unit(&daemon_unit(paths))?,
    };
    println!(
        "Stopped the {APP_NAME} daemon and removed {}.",
        path.display()
    );
    Ok(())
}

/// Runs a bridge as a service, the way `service_install` runs the daemon.
/// Running it again replaces the running bridge.
pub fn bridge_install(paths: &Paths, agent: &str, now: bool) -> Result<()> {
    let manager = manager(
        &format!("{APP_NAME} bridge install"),
        &format!("{APP_NAME} bridge run"),
    )?;
    paths.make_state_dir()?;
    let settings = bridge::load(paths, agent)?;
    wait_for_quiet(paths, &[agent.to_string()], false, now)?;
    let dir = bridge::state_dir(paths, agent);
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let log = dir.join("bridge.log");
    let since = crate::util::now_ms();
    let (job, shown) = match manager {
        Manager::Launchd => {
            let label = bridge_label(paths, agent);
            let domain = LaunchAgent {
                label: &label,
                args: &["bridge", "run", agent],
                log: &log,
                throttle: 30,
                // Long enough for its runs' grace period and to stop what's left.
                exit_timeout: Some(settings.shutdown_grace_secs + 10),
            }
            .start()?;
            (
                format!("launchd {domain}/{label}"),
                log.display().to_string(),
            )
        }
        Manager::Systemd => {
            let name = bridge_unit(paths, agent);
            let daemon = daemon_unit(paths);
            let description = format!("{APP_NAME} bridge: {agent}");
            let job = UserUnit {
                name: &name,
                description: &description,
                args: &["bridge", "run", agent],
                after: Some(&daemon),
            }
            .start()?;
            (job, format!("journalctl --user -u {name}"))
        }
    };
    let until = Instant::now() + Duration::from_secs(10);
    while Instant::now() < until {
        if bridge::started_since(paths, agent, since) {
            println!("The {agent} bridge is running ({job}).");
            println!("Log: {shown}");
            if manager == Manager::Systemd {
                linger_hint();
            }
            return Ok(());
        }
        thread::sleep(Duration::from_millis(250));
    }
    bail!(
        "{job} started the {agent} bridge, but it isn't up. Is the {APP_NAME} daemon running? See {shown}"
    )
}

pub fn bridge_uninstall(paths: &Paths, agent: &str) -> Result<()> {
    let path = match manager(
        &format!("{APP_NAME} bridge uninstall"),
        &format!("{APP_NAME} bridge run"),
    )? {
        Manager::Launchd => stop_launch_agent(&bridge_label(paths, agent))?,
        Manager::Systemd => stop_unit(&bridge_unit(paths, agent))?,
    };
    println!("Stopped the {agent} bridge and removed {}.", path.display());
    Ok(())
}

/// Where a bridge's service is running, if it is.
pub fn bridge_job(paths: &Paths, agent: &str) -> Option<String> {
    if cfg!(target_os = "macos") {
        let label = bridge_label(paths, agent);
        return domains().into_iter().find_map(|domain| {
            let job = format!("{domain}/{label}");
            let out = launchctl(&["print", &job]).ok()?;
            out.status.success().then_some(job)
        });
    }
    let unit = format!("{}.service", bridge_unit(paths, agent));
    systemctl(&["is-active", "--quiet", &unit])
        .ok()
        .map(|()| format!("systemd --user {unit}"))
}

// ACR's agent on this machine.

/// The bridge `tailbus setup acr` sets up: headless Claude Code, which ACR
/// runs every role through, on this machine.
pub const ACR_AGENT: &str = "claude-acr";

/// Settings, in `~/.config/tailbus/acr/`, that keep a review's Claude Code
/// file tools to the code under review, subagents' too.
pub const CONFINE_FILE: &str = "confine.json";

/// The confinement settings: Claude's file tools, a subagent's too, refuse
/// paths outside the working directories.
///
/// Its commands aren't confined. Claude Code's sandbox would do that, but
/// with the read block it stops a session that signs in with a token, as
/// every reviewer here does, before its first request.
fn confine_settings() -> String {
    let settings = serde_json::json!({
        "permissions": {
            "blockReadsOutsideWorkingDirectories": true,
            "blockWritesOutsideWorkingDirectories": true
        }
    });
    let mut text = serde_json::to_string_pretty(&settings).unwrap_or_default();
    text.push('\n');
    text
}

/// The bridge's table in config.toml.
fn acr_bridge(
    owner: &str,
    claude: &Path,
    home: &Path,
    token: &Path,
    confine: &Path,
    path: &str,
) -> String {
    let q = |s: &str| format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""));
    let p = |p: &Path| q(&p.to_string_lossy());
    format!(
        r#"
# ACR on this machine, set up by `{APP_NAME} setup acr`: headless Claude Code,
# on your own subscription, running in the repository under review. Allow a
# repository with `{APP_NAME} acr add-repo <path>`.
[bridges.{ACR_AGENT}]
owner = {owner}
command = {claude}
args = ["-p", "--model", "{{MODEL}}", "--strict-mcp-config", "--output-format", "text",
  "--settings", {confine},
  "--add-dir", "{{ATTACHMENTS_DIR}}", "--add-dir", "{{RUN_DIR}}/tmp"]
model = "claude-sonnet-5-5"
working_directory = {home}
home = {home}
path = {path}
# 4 hours: a cap no review should ever reach. A run ends when it's done.
timeout_secs = 14400
max_concurrent = 3

[bridges.{ACR_AGENT}.env]
# A fresh config for each run: none of your settings, plugins or MCP
# servers, and its transcript where usage is read.
CLAUDE_CONFIG_DIR = "{{SESSION_DIR}}"
# {BG_WAIT_NOTE}
{BG_WAIT} = "0"

[bridges.{ACR_AGENT}.secrets]
CLAUDE_CODE_OAUTH_TOKEN = {token}

[bridges.{ACR_AGENT}.options]
max_timeout_secs = 14400
tools = ["Read", "Grep", "Glob", "Bash", "Write"]
tools_args = ["--tools", "{{TOOLS}}", "--allowedTools", "{{TOOLS}}"]
models = ["claude-haiku-5-5", "claude-sonnet-5-5", "claude-opus-5-5"]
efforts = ["low", "medium", "high"]
effort_args = ["--effort", "{{EFFORT}}"]
workdirs = []

[bridges.{ACR_AGENT}.usage]
files = "**/*.jsonl"
when = {{ type = "assistant" }}
dedupe = "message.id"
input = "message.usage.input_tokens"
output = "message.usage.output_tokens"
cache_read = "message.usage.cache_read_input_tokens"
cache_write = "message.usage.cache_creation_input_tokens"
model = "message.model"
"#,
        owner = q(owner),
        claude = p(claude),
        BG_WAIT = BG_WAIT,
        BG_WAIT_NOTE = BG_WAIT_NOTE,
        home = p(home),
        token = p(token),
        confine = p(confine),
        path = q(path),
    )
}

/// Headless Claude Code stops a session's background work, a test run say,
/// some minutes after the session ends its turn, unless this says to wait.
/// "0" waits as long as the work takes; the bridge's own cap still ends
/// every run.
const BG_WAIT: &str = "CLAUDE_CODE_PRINT_BG_WAIT_CEILING_MS";
const BG_WAIT_NOTE: &str = "A session's background work, such as a test run, lives until it's done: \
     otherwise it's stopped some minutes after the session's turn ends, and \
     what it found is lost. The bridge's timeout still ends every run.";

/// Adds the background wait to an existing ACR bridge's environment, if it
/// isn't there. Returns whether it added it.
fn bg_wait_bridge(doc: &mut toml_edit::DocumentMut) -> Result<bool> {
    let bridge = doc
        .get_mut("bridges")
        .and_then(|b| b.get_mut(ACR_AGENT))
        .and_then(|a| a.as_table_like_mut())
        .context("there's no ACR bridge")?;
    let env = bridge
        .entry("env")
        .or_insert(toml_edit::Item::Table(toml_edit::Table::new()))
        .as_table_like_mut()
        .context("its env isn't a table")?;
    if env.contains_key(BG_WAIT) {
        return Ok(false);
    }
    env.insert(BG_WAIT, toml_edit::value("0"));
    Ok(true)
}

/// Adds `--settings <confine>` to an existing ACR bridge's arguments, if it
/// isn't there. Returns whether it added it.
fn confine_bridge(doc: &mut toml_edit::DocumentMut, confine: &Path) -> Result<bool> {
    let args = doc
        .get_mut("bridges")
        .and_then(|b| b.get_mut(ACR_AGENT))
        .and_then(|a| a.get_mut("args"))
        .and_then(|a| a.as_array_mut())
        .context("the ACR bridge has no args")?;
    if args.iter().any(|a| a.as_str() == Some("--settings")) {
        return Ok(false);
    }
    args.push("--settings");
    args.push(confine.to_string_lossy().as_ref());
    Ok(true)
}

/// The ACR config `tailbus setup acr` writes, if there's none: Anthropic
/// models sized to each role, all on this machine's agent.
fn acr_bindings(machine: &str) -> String {
    let agent = format!("{ACR_AGENT}@{machine}");
    format!(
        r#"# ACR's bindings, set up by `{APP_NAME} setup acr`: Anthropic models through
# {agent}, headless Claude Code on your subscription, so they count in
# tokens. ACR runs on one machine: every role works in the repository, and
# reviewers read it in place, read-only. Each timeout is 4 hours, a cap no
# review should ever reach.
version: 1
allowed_bindings:
  haiku-low:       # the Sizer: one quick call to pick the tier
    agent: {agent}
    model: claude-haiku-5-5
    effort: low
    tools: [Read, Grep, Glob]
    timeout: 14400
    subscription: true
  sonnet-high:     # reviewers: most of the calls, and they attack
    agent: {agent}
    model: claude-sonnet-5-5
    effort: high
    tools: [Read, Grep, Glob]
    timeout: 14400
    subscription: true
  opus-medium:     # the Orchestrator: few calls; without writes, fixes escalate
    agent: {agent}
    model: claude-opus-5-5
    effort: medium
    tools: [Read, Grep, Glob]
    timeout: 14400
    subscription: true
  opus-judge:      # the Judge: checks claims, and may run scripts in its scratch space
    agent: {agent}
    model: claude-opus-5-5
    effort: medium
    tools: [Read, Grep, Glob, Bash, Write]
    timeout: 14400
    subscription: true
defaults:
  bindings:
    sizer: haiku-low
    reviewer: sonnet-high
    orchestrator: opus-medium
    judge: opus-judge
"#
    )
}

/// Reads a line without showing it, for a secret pasted at a terminal.
fn read_hidden(prompt: &str) -> Result<String> {
    use std::io::{BufRead, Write};
    print!("{prompt}");
    std::io::stdout().flush()?;
    // SAFETY: reads and restores the terminal settings of this process's
    // own stdin.
    let saved = unsafe {
        let mut t: libc::termios = std::mem::zeroed();
        (libc::tcgetattr(0, &mut t) == 0).then_some(t)
    };
    if let Some(t) = saved {
        let mut quiet = t;
        quiet.c_lflag &= !libc::ECHO;
        unsafe { libc::tcsetattr(0, libc::TCSANOW, &quiet) };
    }
    let mut line = String::new();
    let read = std::io::stdin().lock().read_line(&mut line);
    if let Some(t) = saved {
        unsafe { libc::tcsetattr(0, libc::TCSANOW, &t) };
    }
    println!();
    read?;
    Ok(line.trim().to_string())
}

/// Makes the long-lived token headless Claude Code signs in with, by
/// `claude setup-token` and the user pasting what it prints.
fn make_token(claude: &Path, token: &Path) -> Result<()> {
    // SAFETY: asks whether this process's stdin is a terminal.
    if unsafe { libc::isatty(0) } != 1 {
        bail!(
            "ACR's agent needs a token, which takes a terminal: run `{} setup-token`, and save \
             what it prints in {} (chmod 600)",
            claude.display(),
            token.display()
        );
    }
    println!(
        "ACR's reviewers run as headless Claude Code, which signs in with a long-lived token \
         for your subscription. Making one now:"
    );
    let made = Command::new(claude)
        .arg("setup-token")
        .status()
        .context("running claude setup-token")?;
    if !made.success() {
        bail!("`claude setup-token` didn't finish");
    }
    let text = read_hidden("Paste the token it printed (it won't show): ")?;
    if text.is_empty() || text.contains(char::is_whitespace) {
        bail!("that doesn't look like a token; run `{APP_NAME} setup acr` again");
    }
    if let Some(dir) = token.parent() {
        fs::create_dir_all(dir)?;
    }
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(token)
        .with_context(|| format!("writing {}", token.display()))?;
    file.write_all(text.as_bytes())?;
    println!("Saved it in {}, readable only by you.", token.display());
    Ok(())
}

fn read_config(paths: &Paths) -> Result<toml_edit::DocumentMut> {
    let text = match fs::read_to_string(&paths.config) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).with_context(|| format!("reading {}", paths.config.display())),
    };
    text.parse()
        .with_context(|| format!("in {}", paths.config.display()))
}

fn write_config(paths: &Paths, doc: &toml_edit::DocumentMut) -> Result<()> {
    // Checked before it's written, so a mistake never lands.
    let text = doc.to_string();
    toml::from_str::<crate::config::Config>(&text).context("the config it would write")?;
    write(&paths.config, &text)
}

/// Repositories ACR may review here: a bridge's workdirs, made if it has
/// none yet.
fn acr_workdirs<'a>(
    doc: &'a mut toml_edit::DocumentMut,
    agent: &str,
) -> Result<&'a mut toml_edit::Array> {
    let bridge = doc
        .get_mut("bridges")
        .and_then(|b| b.get_mut(agent))
        .and_then(|a| a.as_table_like_mut())
        .with_context(|| match agent {
            ACR_AGENT => format!("there's no ACR agent here yet: run `{APP_NAME} setup acr` first"),
            _ => format!("there's no {agent} bridge in this machine's config"),
        })?;
    let options = bridge
        .entry("options")
        .or_insert(toml_edit::Item::Table(toml_edit::Table::new()))
        .as_table_like_mut()
        .context("its options aren't a table")?;
    options
        .entry("workdirs")
        .or_insert(toml_edit::value(toml_edit::Array::new()))
        .as_array_mut()
        .context("its workdirs aren't a list")
}

/// A repository's top folder, as ACR and the bridge name it.
fn repo_root(repo: &Path) -> Result<PathBuf> {
    let repo = repo
        .canonicalize()
        .with_context(|| format!("{} isn't there", repo.display()))?;
    let out = Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args(["rev-parse", "--show-toplevel"])
        .stdin(Stdio::null())
        .output()
        .context("running git")?;
    if !out.status.success() {
        bail!("{} isn't in a git repository", repo.display());
    }
    let top = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim()).canonicalize()?;
    if top != repo {
        bail!("give the repository's top folder: {}", top.display());
    }
    Ok(top)
}

/// Allows ACR to review a repository here through `agent`'s bridge, or
/// stops allowing it, and restarts the bridge if it's running.
pub fn acr_repo(paths: &Paths, repo: &Path, allow: bool, agent: &str) -> Result<()> {
    let repo = if allow {
        repo_root(repo)?
    } else {
        repo.canonicalize().unwrap_or_else(|_| repo.to_path_buf())
    };
    let shown = repo.to_string_lossy().into_owned();
    let mut doc = read_config(paths)?;
    let workdirs = acr_workdirs(&mut doc, agent)?;
    let there = workdirs.iter().any(|v| v.as_str() == Some(&shown));
    // A folder the bridge allows takes in what's under it, with no restart:
    // a trial's copy under the trials' folder, say, while another agent's
    // runs are going through the bridge.
    if allow
        && !there
        && let Some(dir) = workdirs
            .iter()
            .filter_map(|v| v.as_str())
            .find(|d| repo.starts_with(d))
    {
        println!("ACR may already review {shown}, inside {dir}.");
        return Ok(());
    }
    match (allow, there) {
        (true, true) => {
            println!("ACR may already review {shown}.");
            return Ok(());
        }
        (false, false) => {
            println!("ACR wasn't allowed to review {shown}.");
            return Ok(());
        }
        (true, false) => workdirs.push(shown.as_str()),
        (false, true) => workdirs.retain(|v| v.as_str() != Some(&shown)),
    }
    write_config(paths, &doc)?;
    if bridge_job(paths, agent).is_some() {
        bridge_install(paths, agent, false)?;
    }
    let verb = if allow {
        "may now review"
    } else {
        "may no longer review"
    };
    println!("ACR {verb} {shown}, through {agent}.");
    Ok(())
}

/// Lets ACR's `agent` work in `dir` and anything under it, as it does in a
/// repository it may review: where reviewers that can change files get
/// their copies. Restarts the bridge only if that's new. Returns whether it
/// was.
pub fn acr_dir(paths: &Paths, dir: &Path, agent: &str) -> Result<bool> {
    fs::create_dir_all(dir)?;
    let shown = dir.canonicalize()?.to_string_lossy().into_owned();
    let mut doc = read_config(paths)?;
    let workdirs = acr_workdirs(&mut doc, agent)?;
    if workdirs.iter().any(|v| v.as_str() == Some(&shown)) {
        return Ok(false);
    }
    workdirs.push(shown.as_str());
    write_config(paths, &doc)?;
    if bridge_job(paths, agent).is_some() {
        bridge_install(paths, agent, false)?;
    }
    println!("ACR's reviewers may now work in copies under {shown}, through {agent}.");
    Ok(true)
}

/// Sets up ACR on this machine: a token for headless Claude Code, the
/// bridge it runs through, ACR's bindings, and the bridge's service. Each
/// part is left alone if it's there already, so running it again is safe.
pub fn acr(paths: &Paths, repos: &[PathBuf]) -> Result<()> {
    let status: crate::local::Status = local::call(&paths.socket(), &Request::Status)
        .with_context(|| {
            format!("the {APP_NAME} daemon has to be running: {APP_NAME} service install")
        })?;
    let (Some(machine), Some(owner)) = (status.machine, status.owner) else {
        bail!("the daemon doesn't know this machine's name yet; try again in a moment");
    };
    let claude = find_claude()?;
    let dir = paths
        .config
        .parent()
        .context("the config has no folder")?
        .to_path_buf();
    let token = dir.join("claude-token");
    if token.exists() {
        println!("Using the token in {}.", token.display());
    } else {
        make_token(&claude, &token)?;
    }
    let confine = dir.join("acr").join(CONFINE_FILE);
    let settings = confine_settings();
    match fs::read_to_string(&confine) {
        Ok(text) if text == settings => {
            println!("Kept the reviewers' confinement, {}.", confine.display());
        }
        was => {
            fs::create_dir_all(dir.join("acr"))?;
            write(&confine, &settings)?;
            let done = if was.is_ok() { "Updated" } else { "Wrote" };
            println!(
                "{done} {}, which keeps reviewers' file tools to the code under review.",
                confine.display()
            );
        }
    }
    let mut doc = read_config(paths)?;
    let has_bridge = doc.get("bridges").and_then(|b| b.get(ACR_AGENT)).is_some();
    if has_bridge {
        println!(
            "Kept the {ACR_AGENT} bridge already in {}.",
            paths.config.display()
        );
        if confine_bridge(&mut doc, &confine)? {
            println!("Its reviewers now run confined to the code under review.");
        }
        if bg_wait_bridge(&mut doc)? {
            println!("Its sessions' background work now lives until it's done.");
        }
    } else {
        let home = paths.state.join(ACR_AGENT).join("home");
        fs::create_dir_all(&home).with_context(|| format!("creating {}", home.display()))?;
        let claude_dir = claude.parent().map(|d| d.to_string_lossy().into_owned());
        let path: Vec<String> = claude_dir
            .into_iter()
            .chain(["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin", "/bin"].map(String::from))
            .fold(Vec::new(), |mut seen, d| {
                if !seen.contains(&d) {
                    seen.push(d);
                }
                seen
            });
        let mut text = doc.to_string();
        text.push_str(&acr_bridge(
            &owner,
            &claude,
            &home,
            &token,
            &confine,
            &path.join(":"),
        ));
        doc = text.parse().context("adding the ACR agent to the config")?;
        println!(
            "Added the {ACR_AGENT} bridge to {}.",
            paths.config.display()
        );
    }
    let workdirs = acr_workdirs(&mut doc, ACR_AGENT)?;
    for repo in repos {
        let root = repo_root(repo)?.to_string_lossy().into_owned();
        if !workdirs.iter().any(|v| v.as_str() == Some(&root)) {
            workdirs.push(root.as_str());
            println!("ACR may review {root}.");
        }
    }
    write_config(paths, &doc)?;
    let bindings = dir.join("acr.yaml");
    if bindings.exists() {
        println!("Kept your ACR config, {}.", bindings.display());
    } else {
        let text = acr_bindings(&machine);
        crate::acr::check_user_config(&text)?;
        write(&bindings, &text)?;
        fs::set_permissions(
            &bindings,
            std::os::unix::fs::PermissionsExt::from_mode(0o600),
        )?;
        println!("Wrote ACR's bindings to {}.", bindings.display());
    }
    bridge_install(paths, ACR_AGENT, false)?;
    let empty = acr_workdirs(&mut doc, ACR_AGENT)?.is_empty();
    if empty {
        println!("Next, allow each repository ACR reviews here: {APP_NAME} acr add-repo <path>");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_second_home_names_its_services_apart_from_the_default() {
        let default = Paths {
            state: Paths::default_state().unwrap(),
            config: PathBuf::from("/c/config.toml"),
        };
        // Named after the program: acrbus here, tailbus in tailbus.
        let (label, name) = (app().label, app().name);
        assert_eq!(daemon_label(&default), label);
        assert_eq!(
            bridge_label(&default, "claude-acr"),
            format!("{label}.bridge.claude-acr")
        );
        assert_eq!(daemon_unit(&default), name);
        assert_eq!(bridge_unit(&default, "omp"), format!("{name}-bridge-omp"));

        let other = Paths {
            state: PathBuf::from("/tmp/a-test-home"),
            config: PathBuf::from("/tmp/a-test-home/config.toml"),
        };
        let label = bridge_label(&other, "claude-acr");
        assert!(label.starts_with(&format!("{}.", app().label)), "{label}");
        assert!(label.ends_with(".bridge.claude-acr"), "{label}");
        assert_ne!(label, bridge_label(&default, "claude-acr"));
        assert_ne!(daemon_label(&other), daemon_label(&default));
        assert!(bridge_unit(&other, "omp").starts_with(&format!("{name}-")));
        assert_ne!(bridge_unit(&other, "omp"), bridge_unit(&default, "omp"));
        // The same home always gets the same names.
        assert_eq!(label, bridge_label(&other, "claude-acr"));
    }

    #[test]
    fn confinement_blocks_reads_and_writes_outside_and_nothing_else() {
        let v: serde_json::Value = serde_json::from_str(&confine_settings()).unwrap();
        assert_eq!(
            v["permissions"]["blockReadsOutsideWorkingDirectories"],
            true
        );
        assert_eq!(
            v["permissions"]["blockWritesOutsideWorkingDirectories"],
            true
        );
        // The sandbox stops token sessions at startup when reads are blocked.
        assert!(v.get("sandbox").is_none());
    }

    #[test]
    fn an_acr_bridge_lets_background_work_finish() {
        let fresh = acr_bridge(
            "o",
            Path::new("/bin/claude"),
            Path::new("/h"),
            Path::new("/t"),
            Path::new("/c.json"),
            "/bin",
        );
        let doc: toml_edit::DocumentMut = fresh.parse().unwrap();
        assert_eq!(
            doc["bridges"][ACR_AGENT]["env"][BG_WAIT].as_str(),
            Some("0")
        );
        let mut old: toml_edit::DocumentMut =
            "[bridges.claude-acr]\nargs = [\"-p\"]\n[bridges.claude-acr.env]\nA = \"b\"\n"
                .parse()
                .unwrap();
        assert!(bg_wait_bridge(&mut old).unwrap());
        assert!(!bg_wait_bridge(&mut old).unwrap());
        assert_eq!(old["bridges"]["claude-acr"]["env"]["A"].as_str(), Some("b"));
        assert_eq!(
            old["bridges"]["claude-acr"]["env"][BG_WAIT].as_str(),
            Some("0")
        );
    }

    #[test]
    fn an_existing_acr_bridge_is_confined_once() {
        let mut doc: toml_edit::DocumentMut =
            "[bridges.claude-acr]\nargs = [\"-p\"]\n".parse().unwrap();
        let confine = Path::new("/c/confine.json");
        assert!(confine_bridge(&mut doc, confine).unwrap());
        assert!(!confine_bridge(&mut doc, confine).unwrap());
        let args = doc["bridges"]["claude-acr"]["args"].as_array().unwrap();
        let args: Vec<&str> = args.iter().filter_map(|a| a.as_str()).collect();
        assert_eq!(args, ["-p", "--settings", "/c/confine.json"]);
    }

    fn temp(name: &str) -> PathBuf {
        let dir = env::temp_dir().join(format!("tb-setup-{name}-{}", &crate::util::new_id()[14..]));
        fs::create_dir_all(&dir).unwrap();
        dir.canonicalize().unwrap()
    }

    fn git(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[test]
    fn an_update_waits_for_runs_and_reviews_under_way() {
        let dir = temp("quiet");
        let paths = Paths {
            config: dir.join("config.toml"),
            state: dir.join("state"),
        };
        let bridges = ["busy".to_string(), "idle".to_string()];
        assert_eq!(under_way(&paths, &bridges, true), (0, 0));
        let me = std::process::id();
        let status = |agent: &str, running: &str| {
            let d = paths.state.join("bridge").join(agent);
            fs::create_dir_all(&d).unwrap();
            fs::write(
                d.join("status.json"),
                format!(
                    r#"{{"pid": {me}, "started_at": 0, "stopped_at": null, "running": {{{running}}},
                    "answered": 0, "failed": 0, "ignored": 0, "last_failure": null}}"#
                ),
            )
            .unwrap();
        };
        status(
            "busy",
            r#""r1": {"sender": "acr@here", "since": 0, "attempt": 1}"#,
        );
        status("idle", "");
        let review = paths.state.join("acr").join("rv1");
        fs::create_dir_all(&review).unwrap();
        fs::write(review.join("pid"), me.to_string()).unwrap();
        assert_eq!(under_way(&paths, &bridges, true), (1, 1));
        // A bridge's own restart only waits for its own runs.
        assert_eq!(under_way(&paths, &["idle".to_string()], false), (0, 0));
        assert!(
            wait_for_quiet(&paths, &bridges, true, true).is_ok(),
            "--now doesn't wait"
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn what_setup_acr_writes_is_a_working_config() {
        let dir = temp("acr");
        let token = dir.join("claude-token");
        fs::write(&token, "sk-ant-test").unwrap();
        fs::set_permissions(&token, std::os::unix::fs::PermissionsExt::from_mode(0o600)).unwrap();
        let claude = dir.join("claude");
        fs::write(&claude, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&claude, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        let text = acr_bridge(
            "ray@example.com",
            &claude,
            &dir,
            &token,
            &dir.join("confine.json"),
            "/usr/bin:/bin",
        );
        let cfg: crate::config::Config = toml::from_str(&text).unwrap();
        let table = cfg.bridges[ACR_AGENT].clone();
        let settings: bridge::Settings = table.try_into().unwrap();
        settings.check().unwrap();
        assert!(settings.usage.is_some());
        crate::acr::check_user_config(&acr_bindings("box")).unwrap();
        assert!(acr_bindings("box").contains("claude-acr@box"));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_repository_is_allowed_once_and_can_be_dropped() {
        let dir = temp("repo");
        let repo = dir.join("project");
        fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q"]);
        let token = dir.join("claude-token");
        fs::write(&token, "t").unwrap();
        let paths = Paths {
            config: dir.join("config.toml"),
            state: dir.join("state"),
        };
        let start = "# mine, kept\nalerts = false\n".to_string()
            + &acr_bridge(
                "o",
                Path::new("/bin/claude"),
                &dir,
                &token,
                &dir.join("confine.json"),
                "/bin",
            );
        fs::write(&paths.config, &start).unwrap();
        let workdirs = || -> Vec<String> {
            let mut doc = read_config(&paths).unwrap();
            acr_workdirs(&mut doc, ACR_AGENT)
                .unwrap()
                .iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        };
        acr_repo(&paths, &repo, true, ACR_AGENT).unwrap();
        acr_repo(&paths, &repo, true, ACR_AGENT).unwrap();
        assert_eq!(workdirs(), [repo.to_string_lossy()]);
        let text = fs::read_to_string(&paths.config).unwrap();
        assert!(text.starts_with("# mine, kept\n"), "comments stay");
        // Only a repository's top folder.
        let inner = repo.join("src");
        fs::create_dir_all(&inner).unwrap();
        assert!(acr_repo(&paths, &inner, true, ACR_AGENT).is_err());
        acr_repo(&paths, &repo, false, ACR_AGENT).unwrap();
        assert!(workdirs().is_empty());
        // A repository inside a folder already allowed needs nothing more.
        assert!(acr_dir(&paths, &dir, ACR_AGENT).unwrap());
        assert!(!acr_dir(&paths, &dir, ACR_AGENT).unwrap());
        acr_repo(&paths, &repo, true, ACR_AGENT).unwrap();
        assert_eq!(workdirs(), [dir.canonicalize().unwrap().to_string_lossy()]);
        acr_repo(&paths, &repo, false, ACR_AGENT).unwrap();
        assert_eq!(workdirs().len(), 1);
        let mut doc = read_config(&paths).unwrap();
        acr_workdirs(&mut doc, ACR_AGENT).unwrap().clear();
        write_config(&paths, &doc).unwrap();
        // Another bridge, such as one set up by hand, with no workdirs yet.
        let mut text = fs::read_to_string(&paths.config).unwrap();
        text.push_str("\n[bridges.other]\nowner = \"o\"\ncommand = \"/bin/sh\"\nworking_directory = \"/\"\nhome = \"/tmp\"\npath = \"/bin\"\n");
        fs::write(&paths.config, text).unwrap();
        acr_repo(&paths, &repo, true, "other").unwrap();
        let doc = read_config(&paths).unwrap();
        assert_eq!(
            doc["bridges"]["other"]["options"]["workdirs"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert!(acr_repo(&paths, &repo, true, "nobody").is_err());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_pipe_skill_is_the_command_under_its_own_name() {
        let skill = pipe_skill();
        assert!(skill.starts_with("---\nname: pipe\ndisable-model-invocation: true\ndescription:"));
        assert!(skill.contains("argument-hint:"));
        assert!(skill.contains(&format!("---\n{}\n", pipe_mark())));
        assert!(skill.contains("Arguments: $ARGUMENTS"));
        assert_eq!(skill.matches("\n---\n").count(), 1);
    }
}
