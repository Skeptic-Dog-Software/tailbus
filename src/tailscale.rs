//! The bus on a tailnet: Tailscale names this machine and its owner, lists
//! its peers, and says who is on the other end of a connection.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde::de::DeserializeOwned;

use acrbus::config::Config;
use acrbus::network::{Kind, Network, Peer, Status, Whois};

/// tailbus's network: the bus on a tailnet, its default.
pub const KIND: Kind = Kind {
    name: "tailscale",
    open: open_tailscale,
    settings: &[CLI_SETTING],
};

/// config.toml's key for the Tailscale CLI, when it isn't the app's or the
/// one on `PATH`.
const CLI_SETTING: &str = "tailscale";

fn open_tailscale(cfg: &Config) -> Result<Box<dyn Network>> {
    let cli = match cfg.network_settings.get(CLI_SETTING) {
        None => None,
        Some(value) => Some(PathBuf::from(value.as_str().with_context(|| {
            format!("`{CLI_SETTING}` in config.toml should be the path to the Tailscale CLI")
        })?)),
    };
    Ok(Box::new(Tailscale::find(cli.as_deref())))
}

/// The macOS app's own binary. The `tailscale` symlinks Homebrew and the app
/// install crash with a bundle identifier error, so this goes first.
const MAC_APP_CLI: &str = "/Applications/Tailscale.app/Contents/MacOS/Tailscale";

pub struct Tailscale {
    bin: PathBuf,
}

impl Network for Tailscale {
    fn status(&self) -> Result<Status> {
        Tailscale::status(self)
    }

    fn whois(&self, ip: IpAddr) -> Result<Whois> {
        Tailscale::whois(self, ip)
    }

    fn remote(&self) -> bool {
        true
    }
}

impl Tailscale {
    pub fn find(configured: Option<&Path>) -> Tailscale {
        let bin = match configured {
            Some(path) => path.to_path_buf(),
            None if Path::new(MAC_APP_CLI).exists() => PathBuf::from(MAC_APP_CLI),
            None => PathBuf::from("tailscale"),
        };
        Tailscale { bin }
    }

    pub fn status(&self) -> Result<Status> {
        let raw: RawStatus = self.json(&["status", "--json"])?;
        if raw.backend_state != "Running" {
            bail!("Tailscale isn't connected (state {:?})", raw.backend_state);
        }
        let me = raw.myself.context("tailscale status has no Self")?;
        let owner = raw
            .user
            .get(&me.user_id.to_string())
            .map(|u| u.login_name.clone())
            .unwrap_or_default();
        Ok(Status {
            machine: machine_of(&me.dns_name)?,
            ipv4: ipv4_of(&me.tailscale_ips),
            owner,
            peers: raw
                .peer
                .values()
                .filter_map(|p| {
                    Some(Peer {
                        machine: machine_of(&p.dns_name).ok()?,
                        ipv4: ipv4_of(&p.tailscale_ips),
                        online: p.online,
                        owner: raw
                            .user
                            .get(&p.user_id.to_string())
                            .map(|u| u.login_name.clone())
                            .unwrap_or_default(),
                    })
                })
                .collect(),
        })
    }

    pub fn whois(&self, ip: IpAddr) -> Result<Whois> {
        let raw: RawWhois = self.json(&["whois", "--json", &ip.to_string()])?;
        Ok(Whois {
            machine: machine_of(&raw.node.name)?,
            owner: raw.user_profile.login_name,
        })
    }

    fn json<T: DeserializeOwned>(&self, args: &[&str]) -> Result<T> {
        // Under launchd, the macOS app's binary acts as the CLI only when a
        // shell starts it. Started any other way (by this daemon, by launchd
        // itself, by /usr/bin/env), it tries to start the GUI and fails with
        // "The Tailscale GUI failed to start".
        let out = Command::new("/bin/sh")
            .arg("-c")
            .arg("exec \"$0\" \"$@\"")
            .arg(&self.bin)
            .args(args)
            .stdin(Stdio::null())
            .output()
            .with_context(|| format!("running {}", self.bin.display()))?;
        if !out.status.success() {
            bail!(
                "`tailscale {}` failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        serde_json::from_slice(&out.stdout).with_context(|| {
            let said = String::from_utf8_lossy(if out.stdout.is_empty() {
                &out.stderr
            } else {
                &out.stdout
            });
            let said: String = said.trim().chars().take(200).collect();
            format!(
                "reading `tailscale {}`, which said {said:?}",
                args.join(" ")
            )
        })
    }
}

/// `server.example.ts.net.` -> `server`.
fn machine_of(dns_name: &str) -> Result<String> {
    let first = dns_name.split('.').next().unwrap_or_default();
    if first.is_empty() {
        bail!("no MagicDNS name");
    }
    Ok(first.to_ascii_lowercase())
}

fn ipv4_of(ips: &[String]) -> Option<Ipv4Addr> {
    ips.iter().find_map(|ip| ip.parse().ok())
}

#[derive(Deserialize)]
struct RawStatus {
    #[serde(rename = "BackendState", default)]
    backend_state: String,
    #[serde(rename = "Self")]
    myself: Option<RawNode>,
    #[serde(rename = "Peer", default)]
    peer: HashMap<String, RawNode>,
    #[serde(rename = "User", default)]
    user: HashMap<String, RawUser>,
}

#[derive(Deserialize)]
struct RawNode {
    #[serde(rename = "DNSName", default)]
    dns_name: String,
    /// `null` while Tailscale is stopped.
    #[serde(rename = "TailscaleIPs", default, deserialize_with = "null_as_empty")]
    tailscale_ips: Vec<String>,
    #[serde(rename = "Online", default)]
    online: bool,
    #[serde(rename = "UserID", default)]
    user_id: u64,
}

fn null_as_empty<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    Ok(Option::<Vec<String>>::deserialize(d)?.unwrap_or_default())
}

#[derive(Deserialize)]
struct RawUser {
    #[serde(rename = "LoginName", default)]
    login_name: String,
}

#[derive(Deserialize)]
struct RawWhois {
    #[serde(rename = "Node")]
    node: RawWhoisNode,
    #[serde(rename = "UserProfile")]
    user_profile: RawUser,
}

#[derive(Deserialize)]
struct RawWhoisNode {
    #[serde(rename = "Name")]
    name: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tailscale_is_tailbus_default_and_takes_no_local_identity() {
        let open = |text: &str| {
            let cfg: Config = toml::from_str(text).unwrap();
            acrbus::network::open_for(&crate::TAILBUS, &cfg)
        };
        assert!(open("").unwrap().remote());
        let e = open("owner = \"me@example.com\"").err().unwrap();
        assert!(format!("{e:#}").contains("for a local bus"), "{e:#}");
        // A local bus is still there for tailbus to choose.
        let local = open("network = \"local\"\nowner = \"me@example.com\"").unwrap();
        assert!(!local.remote());
        let e = open("network = \"carrier-pigeon\"").err().unwrap();
        assert!(
            format!("{e:#}").contains("tailbus knows: it knows local, tailscale"),
            "{e:#}"
        );
        // Its one setting, the CLI's path, is tailbus's to read, not acrbus's.
        let cfg: Config = toml::from_str("tailscale = \"/opt/ts/tailscale\"").unwrap();
        cfg.check_settings(&crate::TAILBUS).unwrap();
        assert!(cfg.check_settings(&acrbus::config::ACRBUS).is_err());
        assert!(open("tailscale = \"/opt/ts/tailscale\"").unwrap().remote());
        let e = open("tailscale = 3").err().unwrap();
        assert!(
            format!("{e:#}").contains("path to the Tailscale CLI"),
            "{e:#}"
        );
    }

    #[test]
    fn reads_status_and_whois() {
        let status = r#"{
            "BackendState": "Running",
            "Self": {"DNSName": "Laptop.example.ts.net.", "TailscaleIPs": ["fd7a::1", "100.1.2.3"], "Online": true, "UserID": 7},
            "Peer": {"k": {"DNSName": "server.example.ts.net.", "TailscaleIPs": ["100.4.5.6"], "Online": false, "UserID": 7}},
            "User": {"7": {"LoginName": "ray@example.com"}}
        }"#;
        let raw: RawStatus = serde_json::from_str(status).unwrap();
        let me = raw.myself.unwrap();
        assert_eq!(machine_of(&me.dns_name).unwrap(), "laptop");
        assert_eq!(
            ipv4_of(&me.tailscale_ips),
            Some(Ipv4Addr::new(100, 1, 2, 3))
        );
        assert_eq!(raw.user["7"].login_name, "ray@example.com");
        assert!(!raw.peer["k"].online);
        assert_eq!(raw.peer["k"].user_id, 7);

        let whois = r#"{"Node": {"Name": "server.example.ts.net.", "User": 7},
                        "UserProfile": {"ID": 7, "LoginName": "ray@example.com"}}"#;
        let raw: RawWhois = serde_json::from_str(whois).unwrap();
        assert_eq!(machine_of(&raw.node.name).unwrap(), "server");
        assert!(machine_of("").is_err());
    }

    /// A Tailscale CLI that answers `status` and `whois` with these, so
    /// the tests never reach the real tailnet.
    fn fake_cli(status: &str, whois: &str) -> (Tailscale, PathBuf) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static N: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "tailbus-fake-cli-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("status.json"), status).unwrap();
        std::fs::write(dir.join("whois.json"), whois).unwrap();
        let cli = dir.join("tailscale");
        std::fs::write(
            &cli,
            "#!/bin/sh\nhere=$(dirname \"$0\")\ncase \"$1\" in\n  status) cat \"$here/status.json\" ;;\n  \
             whois) cat \"$here/whois.json\" ;;\n  *) exit 2 ;;\nesac\n",
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o755)).unwrap();
        (Tailscale::find(Some(&cli)), dir)
    }

    const ME_AND_PEERS: &str = r#"{
        "BackendState": "Running",
        "Self": {"DNSName": "laptop.example.ts.net.", "TailscaleIPs": ["100.64.0.1"], "Online": true, "UserID": 7},
        "Peer": {
            "a": {"DNSName": "server.example.ts.net.", "TailscaleIPs": ["100.64.0.2"], "Online": true, "UserID": 7},
            "b": {"DNSName": "shared.other.ts.net.", "TailscaleIPs": ["100.64.0.3"], "Online": true, "UserID": 9}
        },
        "User": {"7": {"LoginName": "me@example.com"}, "9": {"LoginName": "dana@example.org"}}
    }"#;

    #[test]
    fn each_peer_has_its_own_owner_not_this_machines() {
        let whois = r#"{"Node": {"Name": "shared.other.ts.net."}, "UserProfile": {"LoginName": "dana@example.org"}}"#;
        let (ts, dir) = fake_cli(ME_AND_PEERS, whois);
        let status = ts.status().unwrap();
        assert_eq!(status.machine, "laptop");
        assert_eq!(status.owner, "me@example.com");
        assert_eq!(status.ipv4, Some(Ipv4Addr::new(100, 64, 0, 1)));
        let owner = |machine: &str| {
            status
                .peers
                .iter()
                .find(|p| p.machine == machine)
                .map(|p| p.owner.clone())
        };
        // Only this owner's own machines count as theirs: a machine shared
        // in from someone else must not get their permission prompts.
        assert_eq!(owner("server").as_deref(), Some("me@example.com"));
        assert_eq!(owner("shared").as_deref(), Some("dana@example.org"));
        let who = ts.whois("100.64.0.3".parse().unwrap()).unwrap();
        assert_eq!(
            (who.machine.as_str(), who.owner.as_str()),
            ("shared", "dana@example.org")
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn status_is_refused_until_tailscale_is_running() {
        for state in ["Starting", "Stopped", "NeedsLogin", ""] {
            let status = ME_AND_PEERS.replace(r#""Running""#, &format!("{state:?}"));
            let (ts, dir) = fake_cli(&status, "{}");
            let e = ts.status().expect_err(state);
            assert!(
                format!("{e:#}").contains("isn't connected"),
                "{state}: {e:#}"
            );
            std::fs::remove_dir_all(dir).unwrap();
        }
    }

    #[test]
    fn acrbus_names_are_read_before_tailbus_ones() {
        acrbus::config::set_app(&crate::TAILBUS);
        let name = "PREFIX_ORDER_TEST";
        // SAFETY: no other test reads or writes these two variables.
        unsafe {
            std::env::set_var(format!("ACRBUS_{name}"), "new");
            std::env::set_var(format!("TAILBUS_{name}"), "old");
        }
        assert_eq!(acrbus::config::env_var(name).as_deref(), Some("new"));
        // A session started before the split still has only its own.
        unsafe { std::env::remove_var(format!("ACRBUS_{name}")) };
        assert_eq!(acrbus::config::env_var(name).as_deref(), Some("old"));
        unsafe { std::env::remove_var(format!("TAILBUS_{name}")) };
        assert_eq!(acrbus::config::env_var(name), None);
    }

    #[test]
    fn reads_a_stopped_tailscale() {
        // As Tailscale reports itself while it's starting after a reboot.
        let status = r#"{"BackendState": "Stopped", "TailscaleIPs": null,
                         "Self": {"DNSName": "", "TailscaleIPs": null}}"#;
        let raw: RawStatus = serde_json::from_str(status).unwrap();
        assert_eq!(raw.backend_state, "Stopped");
        assert!(raw.myself.unwrap().tailscale_ips.is_empty());
    }
}
