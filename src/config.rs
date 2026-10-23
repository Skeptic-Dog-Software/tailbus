//! Where tailbus keeps its state, and its optional config file.

use std::env;
use std::fs;
use std::io;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

pub const DEFAULT_PORT: u16 = 4747;

pub struct Paths {
    /// Database, socket and log. Kept `0700`.
    pub state: PathBuf,
    pub config: PathBuf,
}

impl Paths {
    /// `$TAILBUS_HOME` holds everything when set. Otherwise state goes in
    /// `$XDG_STATE_HOME/tailbus` and the config in
    /// `$XDG_CONFIG_HOME/tailbus/config.toml`.
    pub fn find() -> Result<Paths> {
        if let Some(home) = env_path("TAILBUS_HOME") {
            return Ok(Paths {
                config: home.join("config.toml"),
                state: home,
            });
        }
        let home = env_path("HOME").context("HOME is not set")?;
        let state = env_path("XDG_STATE_HOME").unwrap_or_else(|| home.join(".local/state"));
        let config = env_path("XDG_CONFIG_HOME").unwrap_or_else(|| home.join(".config"));
        Ok(Paths {
            state: state.join("tailbus"),
            config: config.join("tailbus").join("config.toml"),
        })
    }

    pub fn socket(&self) -> PathBuf {
        self.state.join("bus.sock")
    }

    /// Unix socket paths have a small limit (104 bytes on macOS).
    pub fn check_socket(&self) -> Result<()> {
        let socket = self.socket();
        if socket.as_os_str().len() >= 104 {
            anyhow::bail!(
                "the socket path {} is too long for a Unix socket; set TAILBUS_HOME to a shorter directory",
                socket.display()
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
    /// The port every machine's bus listens on, on its tailnet address.
    #[serde(default = "default_port")]
    pub port: u16,
    /// Tailscale logins, or machine names, that may send to agents here
    /// besides machines owned by this machine's own Tailscale user.
    #[serde(default)]
    pub allow: Vec<String>,
    /// Days to keep messages after they're acknowledged.
    #[serde(default = "default_retention")]
    pub retention_days: u32,
    /// The Tailscale CLI, if not the app's or the one on `PATH`.
    pub tailscale: Option<PathBuf>,
}

fn default_port() -> u16 {
    DEFAULT_PORT
}

fn default_retention() -> u32 {
    30
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
        assert!(cfg.allow.is_empty());

        let cfg: Config = toml::from_str("port = 5000\nallow = [\"a@b.com\"]").unwrap();
        assert_eq!(cfg.port, 5000);
        assert_eq!(cfg.allow, ["a@b.com"]);
        assert!(toml::from_str::<Config>("prot = 1").is_err());
    }
}
