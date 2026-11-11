//! Which program this is, where it keeps its state, and its optional config
//! file.

use std::env;
use std::fmt;
use std::fs;
use std::io;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::{Context, Result};
use serde::Deserialize;

pub const DEFAULT_PORT: u16 = 4747;

/// Which program this is: its name, where it keeps things, what its
/// services are called, and which network a bus is on when its config names
/// none. The only thing that tells acrbus and a program built on it, such
/// as tailbus, apart.
#[derive(Debug)]
pub struct App {
    /// Its name, in what it says and runs as, its folders under XDG's, and
    /// its systemd units.
    pub name: &'static str,
    /// Its launchd labels.
    pub label: &'static str,
    /// What `--help` says it is.
    pub about: &'static str,
    /// The network a config that names none is on.
    pub network: &'static str,
    /// The networks it knows besides `local`, which every one knows.
    pub networks: &'static [crate::network::Kind],
}

/// acrbus itself: a bus on this machine alone.
pub const ACRBUS: App = App {
    name: "acrbus",
    label: "com.skepticdogsoftware.acrbus",
    about: "Adversarial convergence review (ACR) on a local agent bus",
    network: "local",
    networks: &[],
};

static CURRENT: OnceLock<&'static App> = OnceLock::new();

/// Says which program this is, once, before anything else runs. A program
/// that never says is acrbus.
pub fn set_app(app: &'static App) {
    let _ = CURRENT.set(app);
}

pub fn app() -> &'static App {
    CURRENT.get().copied().unwrap_or(&ACRBUS)
}

/// The program's name in what it says: `{APP_NAME}` in a format string.
pub struct AppName;

impl fmt::Display for AppName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(app().name)
    }
}

pub const APP_NAME: AppName = AppName;

/// One of our environment variables, `ACRBUS_<name>`, or `TAILBUS_<name>`,
/// which sessions started before acrbus set. Either is accepted, whichever
/// program this is; an empty one counts as unset.
pub fn env_var(name: &str) -> Option<String> {
    [format!("ACRBUS_{name}"), format!("TAILBUS_{name}")]
        .iter()
        .find_map(|var| env::var(var).ok().filter(|v| !v.is_empty()))
}

pub struct Paths {
    /// Database, socket and log. Kept `0700`.
    pub state: PathBuf,
    pub config: PathBuf,
}

impl Paths {
    /// `$ACRBUS_HOME` (or `$TAILBUS_HOME`) holds everything when set.
    /// Otherwise state goes in `$XDG_STATE_HOME/<app>` and the config in
    /// `$XDG_CONFIG_HOME/<app>/config.toml`.
    pub fn find() -> Result<Paths> {
        if let Some(home) = env_var("HOME").map(PathBuf::from) {
            return Ok(Paths {
                config: home.join("config.toml"),
                state: home,
            });
        }
        let config = match env_path("XDG_CONFIG_HOME") {
            Some(dir) => dir,
            None => env_path("HOME").context("HOME is not set")?.join(".config"),
        };
        Ok(Paths {
            state: Paths::default_state()?,
            config: config.join(app().name).join("config.toml"),
        })
    }

    /// Where state goes when `$ACRBUS_HOME` doesn't say: the default home,
    /// whose services keep the plain names.
    pub fn default_state() -> Result<PathBuf> {
        let state = match env_path("XDG_STATE_HOME") {
            Some(dir) => dir,
            None => env_path("HOME")
                .context("HOME is not set")?
                .join(".local/state"),
        };
        Ok(state.join(app().name))
    }

    pub fn socket(&self) -> PathBuf {
        self.state.join("bus.sock")
    }

    /// Unix socket paths have a small limit (104 bytes on macOS).
    pub fn check_socket(&self) -> Result<()> {
        let socket = self.socket();
        if socket.as_os_str().len() >= 104 {
            anyhow::bail!(
                "the socket path {} is too long for a Unix socket; set ACRBUS_HOME to a shorter \
                 directory",
                socket.display(),
            );
        }
        Ok(())
    }

    pub fn db(&self) -> PathBuf {
        self.state.join("bus.db")
    }

    pub fn log(&self) -> PathBuf {
        self.state.join("daemon.log")
    }

    pub fn attachments(&self) -> PathBuf {
        self.state.join("attachments")
    }

    /// Creates the state directory, or tightens it to `0700` if it exists.
    pub fn make_state_dir(&self) -> Result<()> {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&self.state)
            .with_context(|| format!("creating {}", self.state.display()))?;
        fs::set_permissions(&self.state, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("securing {}", self.state.display()))
    }
}

fn env_path(var: &str) -> Option<PathBuf> {
    env::var_os(var)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Which network the bus is on: `local`, this machine alone, or one the
    /// program knows besides, such as tailbus's `tailscale`. The program's
    /// own when left out.
    #[serde(default)]
    pub network: Option<String>,
    /// On a local bus, this machine's name on it. The hostname by default.
    pub machine: Option<String>,
    /// On a local bus, who every message here comes from, which each
    /// bridge's `owner` must match. A local bus needs it.
    pub owner: Option<String>,
    /// The port every machine's bus listens on, on its tailnet address.
    #[serde(default = "default_port")]
    pub port: u16,
    /// Tailscale logins, or machine names, that may send to agents here
    /// besides machines owned by this machine's own Tailscale user.
    #[serde(default)]
    pub allow: Vec<String>,
    /// Days to keep messages after they're acknowledged, and how long one
    /// can go undelivered or unanswered before it's stale.
    #[serde(default = "default_retention")]
    pub retention_days: u32,
    /// Days a stale message is kept, to be seen, before it evaporates.
    #[serde(default = "default_stale")]
    pub stale_days: u32,
    /// A network's own setting: for tailbus's `tailscale` network, the
    /// Tailscale CLI, if not the app's or the one on `PATH`.
    pub tailscale: Option<PathBuf>,
    /// How many times in a row a session can be handed other agents'
    /// messages, with no prompt from its user in between, before the bus
    /// stops handing it more.
    #[serde(default = "default_handoff_limit")]
    pub handoff_limit: u32,
    /// Whether this Mac shows tailbus's dialogs: permission prompts and
    /// paused sessions, from any of the user's machines.
    #[serde(default = "default_alerts")]
    pub alerts: bool,
    /// How long a permission prompt in a turn the user started waits at its
    /// own terminal before going out as an alert. Prompts in turns another
    /// agent's message started go out at once.
    #[serde(default = "default_ask_after")]
    pub ask_after_secs: u64,
    /// Agents here that are clients' workers: agent name to client name.
    /// The tag holds only while the agent's session is billed to an API
    /// key. `TAILBUS_CLIENT` can tag a session instead.
    #[serde(default)]
    pub clients: std::collections::HashMap<String, String>,
    /// The most subagents a `/pipe` task may have running at once. A hub
    /// sends its own with each task, and a worker keeps to the smaller of
    /// that and this.
    #[serde(default = "default_max_subagents")]
    pub max_subagents: u32,
    /// The largest attachment, sent or received.
    #[serde(default = "default_max_attachment")]
    pub max_attachment_bytes: u64,
    /// The most all attachments here can take up together.
    #[serde(default = "default_attachment_store")]
    pub attachment_store_bytes: u64,
    /// Days to keep attachments no open message needs.
    #[serde(default = "default_attachment_retention")]
    pub attachment_retention_days: u32,
    /// Headless agents `tailbus bridge` runs: agent name to its settings.
    /// Kept unparsed here, so a mistake in one stops that bridge and not
    /// the bus.
    #[serde(default)]
    pub bridges: std::collections::HashMap<String, toml::Value>,
}

fn default_max_subagents() -> u32 {
    5
}

fn default_max_attachment() -> u64 {
    1 << 30
}

fn default_attachment_store() -> u64 {
    16 << 30
}

fn default_attachment_retention() -> u32 {
    7
}

fn default_port() -> u16 {
    DEFAULT_PORT
}

fn default_retention() -> u32 {
    30
}

fn default_stale() -> u32 {
    3
}

fn default_handoff_limit() -> u32 {
    6
}

fn default_alerts() -> bool {
    true
}

fn default_ask_after() -> u64 {
    60
}

impl Config {
    /// Reads the config, if there is one.
    pub fn load(path: &Path) -> Result<Config> {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        toml::from_str(&text).with_context(|| format!("in {}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn everything_is_optional() {
        let cfg: Config = toml::from_str("").unwrap();
        assert_eq!(cfg.port, DEFAULT_PORT);
        assert_eq!(cfg.retention_days, 30);
        assert_eq!(cfg.stale_days, 3);
        assert!(cfg.allow.is_empty());
        assert_eq!(cfg.handoff_limit, 6);
        assert!(cfg.alerts);
        assert_eq!(cfg.ask_after_secs, 60);
        assert!(cfg.clients.is_empty());
        assert_eq!(cfg.max_subagents, 5);
        assert!(cfg.bridges.is_empty());
        assert_eq!(cfg.network, None, "the program's own network");
        assert!(cfg.machine.is_none() && cfg.owner.is_none());
        assert_eq!(cfg.max_attachment_bytes, 1 << 30);
        assert_eq!(cfg.attachment_retention_days, 7);
        let cfg: Config = toml::from_str("[bridges.omp]\nnot_a_setting = 1").unwrap();
        assert!(cfg.bridges.contains_key("omp"));
        let cfg: Config = toml::from_str("[clients]\nwork = \"acme\"").unwrap();
        assert_eq!(cfg.clients["work"], "acme");

        let cfg: Config = toml::from_str("port = 5000\nallow = [\"a@b.com\"]").unwrap();
        assert_eq!(cfg.port, 5000);
        assert_eq!(cfg.allow, ["a@b.com"]);
        assert!(toml::from_str::<Config>("prot = 1").is_err());
    }
}
