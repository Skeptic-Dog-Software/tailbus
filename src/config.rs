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
/// none.
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
    /// What its environment variables start with, first first:
    /// `<PREFIX>_HOME`, `<PREFIX>_AGENT` and `<PREFIX>_CLIENT`.
    pub env: &'static [&'static str],
}

/// The bus on this machine alone, with no network: what runs when no
/// program has said what it is, as in a test.
pub const LOCAL: App = App {
    name: "tailbus-local",
    label: "com.skepticdogsoftware.tailbus-local",
    about: "Direct messages between AI agents on this machine",
    network: "local",
    networks: &[],
    env: &["TAILBUS_LOCAL"],
};

static CURRENT: OnceLock<&'static App> = OnceLock::new();

/// Says which program this is, once, before anything else runs. A program
/// that never says is `LOCAL`.
pub fn set_app(app: &'static App) {
    let _ = CURRENT.set(app);
}

pub fn app() -> &'static App {
    CURRENT.get().copied().unwrap_or(&LOCAL)
}

/// The program's name in what it says: `{APP_NAME}` in a format string.
pub struct AppName;

impl fmt::Display for AppName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(app().name)
    }
}

pub const APP_NAME: AppName = AppName;

/// One of the program's environment variables, `<PREFIX>_<name>`, under
/// the first of its prefixes that's set; an empty one counts as unset.
pub fn env_var(name: &str) -> Option<String> {
    env_var_for(app(), name)
}

fn env_var_for(app: &App, name: &str) -> Option<String> {
    app.env
        .iter()
        .find_map(|prefix| env::var(format!("{prefix}_{name}")).ok())
        .filter(|v| !v.is_empty())
}

#[derive(Clone)]
pub struct Paths {
    /// Database, socket and log. Kept `0700`.
    pub state: PathBuf,
    pub config: PathBuf,
}

impl Paths {
    /// `$TAILBUS_HOME` holds everything when set.
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

    /// Where state goes when `$TAILBUS_HOME` doesn't say: the default home,
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
                "the socket path {} is too long for a Unix socket; set TAILBUS_HOME to a shorter \
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
pub struct Config {
    /// Which network the bus is on: `local`, this machine alone, or one the
    /// program knows besides. The program's own when left out.
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
    /// Agents that run in another OS account and reach the bus only through
    /// a door of their own: agent name to its door. A door agent takes
    /// requests from agents on this machine and can only answer them.
    #[serde(default)]
    pub doors: std::collections::HashMap<String, DoorConfig>,
    /// Settings a network reads, such as the path to a CLI it runs: keys
    /// only the program's networks declare (`network::Kind::settings`).
    /// Anything else is refused when the config is loaded.
    #[serde(flatten)]
    pub network_settings: std::collections::BTreeMap<String, toml::Value>,
}

/// `[doors.<agent>]` in config.toml: an agent in another OS account, and the
/// socket it reaches the bus through.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DoorConfig {
    /// The OS account the agent runs as. Only its processes get through.
    pub account: String,
    /// The door's Unix socket. Its folder must already exist, belong to the
    /// user the daemon runs as, and be writable by nobody else; the setup
    /// lets the account reach into it (see the README's Doors).
    pub socket: PathBuf,
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

/// Sections an earlier version read and this one doesn't, such as ACR's
/// Grayboard server: one left in config.toml is ignored, so an
/// update never stops a daemon over it.
const RETIRED: [&str; 1] = ["grayboard"];

impl Config {
    /// Reads the config, if there is one.
    pub fn load(path: &Path) -> Result<Config> {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        let cfg: Config =
            toml::from_str(&text).with_context(|| format!("in {}", path.display()))?;
        cfg.check_settings(app())
            .with_context(|| format!("in {}", path.display()))?;
        Ok(cfg)
    }

    /// Refuses a key nothing reads: every key outside the ones above must be
    /// a setting one of `app`'s networks declares.
    pub fn check_settings(&self, app: &App) -> Result<()> {
        for key in self.network_settings.keys() {
            if RETIRED.contains(&key.as_str()) {
                continue;
            }
            if !app
                .networks
                .iter()
                .any(|n| n.settings.contains(&key.as_str()))
            {
                anyhow::bail!("`{key}` isn't a setting {} knows", app.name);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn never_opened(_: &Config) -> Result<Box<dyn crate::network::Network>> {
        anyhow::bail!("not in this test")
    }

    /// A program with a network of its own, which reads one setting, and
    /// two environment prefixes.
    const WITH_VPN: App = App {
        name: "withvpn",
        label: "com.example.withvpn",
        about: "A test program",
        network: "vpn",
        networks: &[crate::network::Kind {
            name: "vpn",
            open: never_opened,
            settings: &["vpn_cli"],
        }],
        env: &["WITHVPN", "OLDVPN"],
    };

    #[test]
    fn a_key_only_a_network_declares_is_a_setting_and_a_typo_is_refused() {
        let cfg: Config = toml::from_str("vpn_cli = \"/bin/vpn\"").unwrap();
        assert_eq!(cfg.network_settings["vpn_cli"].as_str(), Some("/bin/vpn"));
        cfg.check_settings(&WITH_VPN).unwrap();
        let e = cfg.check_settings(&LOCAL).unwrap_err().to_string();
        assert!(
            e.contains("`vpn_cli` isn't a setting tailbus-local knows"),
            "{e}"
        );
        let typo: Config = toml::from_str("retention_dayz = 3").unwrap();
        assert!(typo.check_settings(&WITH_VPN).is_err());
        // Settings the bus itself reads aren't network settings.
        let known: Config = toml::from_str("retention_days = 3").unwrap();
        assert!(known.network_settings.is_empty());
    }

    #[test]
    fn environment_variables_go_by_the_programs_prefixes_in_order() {
        // SAFETY: names no other test uses.
        unsafe {
            std::env::set_var("OLDVPN_TESTVAR", "old");
            std::env::remove_var("WITHVPN_TESTVAR");
        }
        assert_eq!(env_var_for(&WITH_VPN, "TESTVAR").as_deref(), Some("old"));
        unsafe { std::env::set_var("WITHVPN_TESTVAR", "new") };
        assert_eq!(env_var_for(&WITH_VPN, "TESTVAR").as_deref(), Some("new"));
        // The local bus reads only its own.
        assert_eq!(env_var_for(&LOCAL, "TESTVAR"), None);
        unsafe {
            std::env::remove_var("OLDVPN_TESTVAR");
            std::env::remove_var("WITHVPN_TESTVAR");
        }
    }

    #[test]
    fn a_door_names_its_account_and_socket() {
        let cfg: Config = toml::from_str(
            "[doors.open-source]\naccount = \"grayboard-oss\"\nsocket = \"/Users/Shared/tailbus-doors/open-source/door.sock\"",
        )
        .unwrap();
        let door = &cfg.doors["open-source"];
        assert_eq!(door.account, "grayboard-oss");
        assert_eq!(
            door.socket,
            Path::new("/Users/Shared/tailbus-doors/open-source/door.sock")
        );
        cfg.check_settings(&LOCAL).unwrap();
        // A misspelled door setting is refused, not ignored.
        assert!(
            toml::from_str::<Config>("[doors.x]\naccount = \"a\"\nsocket = \"/s\"\nsockt = 1")
                .is_err()
        );
    }

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
        assert!(cfg.doors.is_empty());
        assert_eq!(cfg.network, None, "the program's own network");
        assert!(cfg.machine.is_none() && cfg.owner.is_none());
        assert_eq!(cfg.max_attachment_bytes, 1 << 30);
        assert_eq!(cfg.attachment_retention_days, 7);
        let cfg: Config = toml::from_str("[bridges.coder]\nnot_a_setting = 1").unwrap();
        assert!(cfg.bridges.contains_key("coder"));
        let cfg: Config = toml::from_str("[clients]\nwork = \"acme\"").unwrap();
        assert_eq!(cfg.clients["work"], "acme");

        let cfg: Config = toml::from_str("port = 5000\nallow = [\"a@b.com\"]").unwrap();
        assert_eq!(cfg.port, 5000);
        assert_eq!(cfg.allow, ["a@b.com"]);
        // A misspelled key parses, and loading the config refuses it.
        let typo: Config = toml::from_str("prot = 1").unwrap();
        assert!(typo.check_settings(&LOCAL).is_err());
    }
}
