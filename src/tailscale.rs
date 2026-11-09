//! The bus on a tailnet: Tailscale names this machine and its owner, lists
//! its peers, and says who is on the other end of a connection.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde::de::DeserializeOwned;

use crate::network::{Network, Peer, Status, Whois};

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
