//! `tailbus setup` and `tailbus service`: wiring tailbus into Claude Code,
//! and running the daemon and bridges as services: launchd agents on
//! macOS, systemd user units on Linux.

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

/// Whether a binary at `path` is an installed copy of this program: named
/// as the program, and outside any build directory. A `.new` waiting to be
/// moved into place, a `.prev` kept to go back to, or a `target/` build
/// isn't, and a path to one written into the plugin or a service would
/// break when it's moved, replaced or cleaned.
pub(crate) fn is_installed(path: &Path) -> bool {
    path.file_name().is_some_and(|n| n == app().name)
        && !path.components().any(|c| c.as_os_str() == "target")
}

/// The binary the daemon's service runs, from its systemd unit or launchd
/// plist, if one is installed.
fn service_exe(paths: &Paths) -> Option<PathBuf> {
    if let Ok(unit) = unit_path(&daemon_unit(paths))
        && let Ok(text) = fs::read_to_string(unit)
        && let Some(exe) = unit_exe(&text)
    {
        return Some(exe);
    }
    let plist = fs::read_to_string(plist_path(&daemon_label(paths)).ok()?).ok()?;
    plist_exe(&plist)
}

/// The program a unit's `ExecStart` runs: its first word, quoted as setup
/// writes it or not.
fn unit_exe(text: &str) -> Option<PathBuf> {
    let line = text
        .lines()
        .find_map(|l| l.strip_prefix("ExecStart="))?
        .trim();
    let first = match line.strip_prefix('"') {
        Some(rest) => rest.split('"').next()?,
        None => line.split_whitespace().next()?,
    };
    (!first.is_empty()).then(|| PathBuf::from(first.replace("\\\\", "\\")))
}

/// The program a plist's `ProgramArguments` runs: its first string.
fn plist_exe(text: &str) -> Option<PathBuf> {
    let after = text.split("<key>ProgramArguments</key>").nth(1)?;
    let start = after.find("<string>")? + "<string>".len();
    let end = after[start..].find("</string>")?;
    let raw = &after[start..start + end];
    let unxml = raw
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&");
    Some(PathBuf::from(unxml))
}

/// The installed binary: this one if it is, or else the one the daemon's
/// service runs, or the program in `~/.local/bin` or `~/.cargo/bin`.
/// Everything setup writes, the plugin and the services, runs this path, so
/// a binary run from where it waits or is built writes the one in place.
pub fn installed_exe(paths: &Paths) -> Result<PathBuf> {
    let exe = env::current_exe()?;
    let exe = exe.canonicalize().unwrap_or(exe);
    let home = home().ok();
    installed_from(&exe, service_exe(paths), home.as_deref())
}

fn installed_from(exe: &Path, service: Option<PathBuf>, home: Option<&Path>) -> Result<PathBuf> {
    if is_installed(exe) {
        return Ok(exe.to_path_buf());
    }
    let name = app().name;
    let candidates = service.into_iter().chain(home.into_iter().flat_map(|h| {
        [
            h.join(".local/bin").join(name),
            h.join(".cargo/bin").join(name),
        ]
    }));
    for candidate in candidates {
        if is_installed(&candidate) && candidate.is_file() {
            eprintln!(
                "Note: {} isn't where {APP_NAME} is installed, so what setup writes runs {}.",
                exe.display(),
                candidate.display()
            );
            return Ok(candidate);
        }
    }
    bail!(
        "{} isn't an installed {APP_NAME}, and there's none to point at instead: install it \
         as ~/.local/bin/{name} (or with `cargo install --path .`), then run setup from there, \
         so the plugin and services run a binary that stays put",
        exe.display()
    )
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
pub fn claude(paths: &Paths, install: bool) -> Result<()> {
    let exe = installed_exe(paths)?;
    let plugin = write_plugin(&exe)?;
    println!("Wrote the Claude Code plugin to {}", plugin.display());
    install_pipe_skill()?;
    let (root, name) = (plugin_root()?, app().name);

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

/// The local marketplace the plugin lives in, which Claude Code loads in
/// place: `<data>/<app>/claude`.
pub(crate) fn plugin_root() -> Result<PathBuf> {
    let data = env::var_os("XDG_DATA_HOME")
        .filter(|d| !d.is_empty())
        .map_or_else(
            || home().map(|h| h.join(".local/share")),
            |d| Ok(PathBuf::from(d)),
        )?;
    Ok(data.join(app().name).join("claude"))
}

/// The plugin's folder in it.
pub(crate) fn plugin_dir() -> Result<PathBuf> {
    Ok(plugin_root()?.join(app().name))
}

/// Writes every file of the plugin, running `exe`: its marketplace entry,
/// manifest, MCP server, hooks, the /pipe command and the skills. Returns
/// its folder.
pub(crate) fn write_plugin(exe: &Path) -> Result<PathBuf> {
    let exe = exe.to_string_lossy();
    let name = app().name;
    let root = plugin_root()?;
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
    // A lighter review: two fresh reviewers from different model families on the whole change.
    write(
        &plugin.join("skills/review/SKILL.md"),
        include_str!("../plugin/skills/review/SKILL.md"),
    )?;
    // Scores a finished review against defects known some other way.
    write(
        &plugin.join("skills/review-score/SKILL.md"),
        include_str!("../plugin/skills/review-score/SKILL.md"),
    )?;
    Ok(plugin)
}

const PIPE: &str = include_str!("../plugin/commands/pipe.md");

/// The home variable a service is given: under the program's first
/// environment prefix, which is the one it writes.
fn home_var() -> String {
    format!("{}_HOME", app().env.first().copied().unwrap_or("TAILBUS"))
}

/// The `/pipe` command.
fn pipe_text() -> String {
    PIPE.to_string()
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

/// Where the personal `/pipe` skill goes.
pub(crate) fn pipe_skill_path() -> Result<PathBuf> {
    let config = env::var_os("CLAUDE_CONFIG_DIR")
        .filter(|d| !d.is_empty())
        .map_or_else(
            || home().map(|h| h.join(".claude")),
            |d| Ok(PathBuf::from(d)),
        )?;
    Ok(config.join("skills/pipe/SKILL.md"))
}

/// What's at the personal `/pipe` skill's place.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PipeSkill {
    Absent,
    /// Written by setup: current, or from another version.
    Ours {
        current: bool,
    },
    /// Someone else's, which setup leaves alone.
    Theirs,
}

pub(crate) fn pipe_skill_state(path: &Path) -> PipeSkill {
    match fs::read_to_string(path) {
        Err(_) => PipeSkill::Absent,
        Ok(text) if !text.contains(PIPE_MARK_END) => PipeSkill::Theirs,
        Ok(text) => PipeSkill::Ours {
            current: text == pipe_skill(),
        },
    }
}

/// Writes the personal `/pipe` skill, the version this binary has.
pub(crate) fn write_pipe_skill(path: &Path) -> Result<()> {
    write(path, &pipe_skill())
}

fn install_pipe_skill() -> Result<()> {
    let path = pipe_skill_path()?;
    if pipe_skill_state(&path) == PipeSkill::Theirs {
        println!(
            "Left {} alone, since {APP_NAME} didn't write it. /{APP_NAME}:pipe works either way.",
            path.display()
        );
        return Ok(());
    }
    write_pipe_skill(&path)?;
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
        let home_env = env_var("HOME")
            .map(|h| format!("Environment={}\n", quoted(&format!("{}={h}", home_var()))))
            .unwrap_or_default();
        format!(
            "[Unit]\nDescription={}\n{after}\n[Service]\nExecStart={}\n{home_env}Restart=on-failure\nRestartSec=5\n\n[Install]\nWantedBy=default.target\n",
            self.description,
            command.join(" ")
        )
    }

    /// Writes the unit, running `exe`, and starts it, replacing a running
    /// one.
    fn start(&self, exe: &Path) -> Result<String> {
        write(&unit_path(self.name)?, &self.text(exe))?;
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
                "        <key>{}</key><string>{}</string>\n",
                home_var(),
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
    fn start(&self, exe: &Path) -> Result<String> {
        let path = plist_path(self.label)?;
        write(&path, &self.plist(exe))?;
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

/// What a restart would cut short now: runs on these bridges.
fn under_way(paths: &Paths, bridges: &[String]) -> usize {
    bridges.iter().map(|b| bridge::busy(paths, b)).sum()
}

/// Waits until no run on these bridges, which a restart would cut short,
/// is under way. `now` doesn't wait.
fn wait_for_quiet(paths: &Paths, bridges: &[String], now: bool) -> Result<()> {
    if now {
        return Ok(());
    }
    let until = Instant::now() + QUIET_WAIT;
    let mut told = false;
    loop {
        let runs = under_way(paths, bridges);
        if runs == 0 {
            return Ok(());
        }
        if !told {
            println!(
                "Waiting for {runs} bridge run(s) under way to finish before restarting. \
                 --now restarts at once."
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
    // a run's reply can be cut short.
    let bridges: Vec<String> = crate::config::Config::load(&paths.config)
        .map(|c| c.bridges.keys().cloned().collect())
        .unwrap_or_default();
    let exe = installed_exe(paths)?;
    wait_for_quiet(paths, &bridges, now)?;
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
            .start(&exe)?;
            format!("launchd {domain}/{label}")
        }
        Manager::Systemd => UserUnit {
            name: &unit,
            description: &format!("{APP_NAME}: {}", app().about),
            args: &["daemon"],
            after: None,
        }
        .start(&exe)?,
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
    let exe = installed_exe(paths)?;
    wait_for_quiet(paths, &[agent.to_string()], now)?;
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
            .start(&exe)?;
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
            .start(&exe)?;
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

pub(crate) fn read_config(paths: &Paths) -> Result<toml_edit::DocumentMut> {
    let text = match fs::read_to_string(&paths.config) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).with_context(|| format!("reading {}", paths.config.display())),
    };
    text.parse()
        .with_context(|| format!("in {}", paths.config.display()))
}

pub(crate) fn write_config(paths: &Paths, doc: &toml_edit::DocumentMut) -> Result<()> {
    // Checked before it's written, so a mistake never lands.
    let text = doc.to_string();
    toml::from_str::<crate::config::Config>(&text).context("the config it would write")?;
    write(&paths.config, &text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_setup_writes_runs_the_installed_binary_not_a_waiting_or_built_one() {
        let name = app().name;
        let dir = std::env::temp_dir().join(format!("tb-installed-{}", crate::util::new_id()));
        let bin = dir.join(".local/bin");
        fs::create_dir_all(&bin).unwrap();
        let installed = bin.join(name);
        fs::write(&installed, "").unwrap();
        // The installed one itself.
        assert_eq!(
            installed_from(&installed, None, Some(&dir)).unwrap(),
            installed
        );
        // One waiting to be moved in, one kept to go back to, and a build all
        // write the installed one.
        for waiting in [
            bin.join(format!("{name}.new")),
            bin.join(format!("{name}.prev")),
            dir.join(format!("src/target/release/{name}")),
        ] {
            assert_eq!(
                installed_from(&waiting, None, Some(&dir)).unwrap(),
                installed
            );
        }
        // The service's binary wins over ~/.local/bin.
        let service = dir.join(format!("opt/{name}"));
        fs::create_dir_all(service.parent().unwrap()).unwrap();
        fs::write(&service, "").unwrap();
        let waiting = bin.join(format!("{name}.new"));
        assert_eq!(
            installed_from(&waiting, Some(service.clone()), Some(&dir)).unwrap(),
            service
        );
        // With none installed, it says so rather than write a path that moves.
        let e = installed_from(&waiting, None, Some(&dir.join("elsewhere"))).unwrap_err();
        assert!(format!("{e:#}").contains("isn't an installed"), "{e:#}");
    }

    #[test]
    fn the_services_binary_is_read_from_its_unit_or_plist() {
        let unit = UserUnit {
            name: "x",
            description: "x",
            args: &["daemon"],
            after: None,
        }
        .text(Path::new("/home/me/.local/bin/my app"));
        assert_eq!(
            unit_exe(&unit),
            Some(PathBuf::from("/home/me/.local/bin/my app"))
        );
        assert_eq!(
            unit_exe("ExecStart=/usr/bin/x daemon\n"),
            Some(PathBuf::from("/usr/bin/x"))
        );
        let log = PathBuf::from("/tmp/log");
        let plist = LaunchAgent {
            label: "l",
            args: &["daemon"],
            log: &log,
            throttle: 10,
            exit_timeout: None,
        }
        .plist(Path::new("/Users/me/.local/bin/a&b"));
        assert_eq!(
            plist_exe(&plist),
            Some(PathBuf::from("/Users/me/.local/bin/a&b"))
        );
    }

    #[test]
    fn a_second_home_names_its_services_apart_from_the_default() {
        let default = Paths {
            state: Paths::default_state().unwrap(),
            config: PathBuf::from("/c/config.toml"),
        };
        // Named after the program: tailbus here, tailbus in tailbus.
        let (label, name) = (app().label, app().name);
        assert_eq!(daemon_label(&default), label);
        assert_eq!(
            bridge_label(&default, "claude-acr"),
            format!("{label}.bridge.claude-acr")
        );
        assert_eq!(daemon_unit(&default), name);
        assert_eq!(
            bridge_unit(&default, "coder"),
            format!("{name}-bridge-coder")
        );

        let other = Paths {
            state: PathBuf::from("/tmp/a-test-home"),
            config: PathBuf::from("/tmp/a-test-home/config.toml"),
        };
        let label = bridge_label(&other, "claude-acr");
        assert!(label.starts_with(&format!("{}.", app().label)), "{label}");
        assert!(label.ends_with(".bridge.claude-acr"), "{label}");
        assert_ne!(label, bridge_label(&default, "claude-acr"));
        assert_ne!(daemon_label(&other), daemon_label(&default));
        assert!(bridge_unit(&other, "coder").starts_with(&format!("{name}-")));
        assert_ne!(bridge_unit(&other, "coder"), bridge_unit(&default, "coder"));
        // The same home always gets the same names.
        assert_eq!(label, bridge_label(&other, "claude-acr"));
    }

    /// A bridge setup writes, as the bridge reads it: over its kind's
    /// defaults.
    fn temp(name: &str) -> PathBuf {
        let dir = env::temp_dir().join(format!("tb-setup-{name}-{}", &crate::util::new_id()[14..]));
        fs::create_dir_all(&dir).unwrap();
        dir.canonicalize().unwrap()
    }

    #[test]
    fn an_update_waits_for_runs_under_way() {
        let dir = temp("quiet");
        let paths = Paths {
            config: dir.join("config.toml"),
            state: dir.join("state"),
        };
        let bridges = ["busy".to_string(), "idle".to_string()];
        assert_eq!(under_way(&paths, &bridges), 0);
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
            r#""r1": {"sender": "a@here", "since": 0, "attempt": 1}"#,
        );
        status("idle", "");
        assert_eq!(under_way(&paths, &bridges), 1);
        // A bridge's own restart only waits for its own runs.
        assert_eq!(under_way(&paths, &["idle".to_string()]), 0);
        assert!(
            wait_for_quiet(&paths, &bridges, true).is_ok(),
            "--now doesn't wait"
        );
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
