//! What the bus knows of where it is: this machine's name and owner, the
//! other machines it can reach, and who is on the other end of a connection.
//!
//! A local bus is the one every program knows: its name and owner come from
//! the config, and it reaches no other machine. A program built on acrbus
//! can know more, such as tailbus's Tailscale, through its `App`.

use std::net::{IpAddr, Ipv4Addr};

use anyhow::{Result, bail};

use crate::config::{Config, app};
use crate::util::{normalize_name, valid_name};

#[derive(Debug, Clone)]
pub struct Status {
    pub machine: String,
    pub ipv4: Option<Ipv4Addr>,
    /// Who owns this machine: a Tailscale login, or a local bus's `owner`.
    pub owner: String,
    pub peers: Vec<Peer>,
}

#[derive(Debug, Clone)]
pub struct Peer {
    pub machine: String,
    pub ipv4: Option<Ipv4Addr>,
    pub online: bool,
    /// The Tailscale login that owns it.
    pub owner: String,
}

#[derive(Debug, Clone)]
pub struct Whois {
    pub machine: String,
    pub owner: String,
}

/// Where the bus is.
pub trait Network: Send + Sync {
    /// This machine, its owner, and the machines it can reach.
    fn status(&self) -> Result<Status>;
    /// The machine at `ip`, which connected to this one, and its owner.
    fn whois(&self, ip: IpAddr) -> Result<Whois>;
    /// Whether other machines are in reach. A bus with none neither listens
    /// for them nor sends to them.
    fn remote(&self) -> bool;
}

/// A network a bus can be on besides `local`: what `network` calls it in
/// config.toml, and how it's opened.
#[derive(Debug)]
pub struct Kind {
    pub name: &'static str,
    pub open: fn(&Config) -> Result<Box<dyn Network>>,
}

/// The network every program knows: this machine alone.
pub const LOCAL: &str = "local";

/// This machine alone, with no Tailscale.
#[derive(Debug, Clone)]
pub struct Local {
    machine: String,
    owner: String,
}

impl Local {
    /// `machine` defaults to the hostname. `owner` has no default: it's who
    /// every message here comes from, which each bridge's `owner` must match.
    pub fn new(machine: Option<&str>, owner: Option<&str>) -> Result<Local> {
        let owner = owner.map(str::trim).unwrap_or_default();
        if owner.is_empty() {
            bail!(
                "a local bus needs `owner` in config.toml: who its messages come from, \
                 such as your email address. Each bridge's `owner` must match it."
            );
        }
        let machine = match machine {
            Some(name) if valid_name(name) => name.to_string(),
            Some(name) => bail!(
                "`machine = {name:?}` isn't a valid machine name: use lowercase letters, digits and hyphens"
            ),
            None => match hostname().as_deref().map(machine_of_host) {
                Some(Some(name)) => name,
                _ => bail!(
                    "this machine's hostname gives no name for it: set `machine` in config.toml"
                ),
            },
        };
        Ok(Local {
            machine,
            owner: owner.to_string(),
        })
    }
}

impl Network for Local {
    fn status(&self) -> Result<Status> {
        Ok(Status {
            machine: self.machine.clone(),
            ipv4: None,
            owner: self.owner.clone(),
            peers: Vec::new(),
        })
    }

    fn whois(&self, ip: IpAddr) -> Result<Whois> {
        bail!("this bus is local, so nothing from another machine reaches it, as {ip} tried to")
    }

    fn remote(&self) -> bool {
        false
    }
}

/// The network the config names, or the program's own.
pub fn open(cfg: &Config) -> Result<Box<dyn Network>> {
    open_for(app(), cfg)
}

/// The network the config names, or `app`'s own, for `app`.
pub fn open_for(app: &crate::config::App, cfg: &Config) -> Result<Box<dyn Network>> {
    let name = cfg.network.as_deref().unwrap_or(app.network);
    if name == LOCAL {
        return Ok(Box::new(Local::new(
            cfg.machine.as_deref(),
            cfg.owner.as_deref(),
        )?));
    }
    let Some(kind) = app.networks.iter().find(|k| k.name == name) else {
        let known: Vec<&str> = std::iter::once(LOCAL)
            .chain(app.networks.iter().map(|k| k.name))
            .collect();
        bail!(
            "`network = {name:?}` in config.toml isn't a network {} knows: it knows {}",
            app.name,
            known.join(", ")
        );
    };
    if cfg.machine.is_some() || cfg.owner.is_some() {
        bail!(
            "`machine` and `owner` in config.toml are for a local bus \
             (`network = \"local\"`): on {name}, the network names this machine and its owner"
        );
    }
    (kind.open)(cfg)
}

/// For tests: a network that reaches other machines, which the tests never
/// ask, setting the bus's view of it themselves.
#[cfg(test)]
pub struct Remote;

#[cfg(test)]
impl Network for Remote {
    fn status(&self) -> Result<Status> {
        bail!("a test's remote network isn't asked")
    }

    fn whois(&self, ip: IpAddr) -> Result<Whois> {
        bail!("a test's remote network isn't asked about {ip}")
    }

    fn remote(&self) -> bool {
        true
    }
}

/// `Rays-MacBook-Pro.local` -> `rays-macbook-pro`, as Tailscale would name it.
fn machine_of_host(host: &str) -> Option<String> {
    let first = host.split('.').next().unwrap_or_default();
    if !first.chars().any(|c| c.is_ascii_alphanumeric()) {
        return None;
    }
    Some(normalize_name(first))
}

fn hostname() -> Option<String> {
    let mut buf = [0u8; 256];
    // SAFETY: the buffer is valid for its length, and gethostname writes at
    // most that many bytes.
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
    if rc != 0 {
        return None;
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    let name = String::from_utf8_lossy(&buf[..end]).trim().to_string();
    (!name.is_empty()).then_some(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(text: &str) -> Config {
        toml::from_str(text).unwrap()
    }

    #[test]
    fn a_local_bus_is_its_configured_machine_with_no_peers() {
        let net = open(&cfg(
            "network = \"local\"\nmachine = \"build-box\"\nowner = \"me@example.com\"",
        ))
        .unwrap();
        assert!(!net.remote());
        let s = net.status().unwrap();
        assert_eq!(s.machine, "build-box");
        assert_eq!(s.owner, "me@example.com");
        assert!(s.peers.is_empty() && s.ipv4.is_none());
        let refused = net.whois("100.64.0.1".parse().unwrap()).unwrap_err();
        assert!(format!("{refused:#}").contains("local"), "{refused:#}");
    }

    #[test]
    fn a_local_bus_names_itself_from_the_hostname_but_needs_an_owner() {
        let net = open(&cfg("network = \"local\"\nowner = \"me@example.com\"")).unwrap();
        assert!(valid_name(&net.status().unwrap().machine));

        let e = open(&cfg("network = \"local\"")).err().unwrap();
        assert!(format!("{e:#}").contains("needs `owner`"), "{e:#}");
        let e = open(&cfg("network = \"local\"\nowner = \" \""))
            .err()
            .unwrap();
        assert!(format!("{e:#}").contains("needs `owner`"), "{e:#}");
        let e = open(&cfg(
            "network = \"local\"\nowner = \"o\"\nmachine = \"Build Box\"",
        ))
        .err()
        .unwrap();
        assert!(
            format!("{e:#}").contains("isn't a valid machine name"),
            "{e:#}"
        );
    }

    #[test]
    fn acrbus_is_local_by_default_and_names_the_networks_it_knows() {
        // acrbus's own network is local, so it needs an owner.
        let e = open(&cfg("")).err().unwrap();
        assert!(format!("{e:#}").contains("needs `owner`"), "{e:#}");
        assert!(!open(&cfg("owner = \"me@example.com\"")).unwrap().remote());
        let e = open(&cfg("network = \"carrier-pigeon\"")).err().unwrap();
        assert!(
            format!("{e:#}").contains("isn't a network acrbus knows: it knows local"),
            "{e:#}"
        );
    }

    #[test]
    fn hostnames_become_machine_names() {
        assert_eq!(
            machine_of_host("Rays-MacBook-Pro.local").as_deref(),
            Some("rays-macbook-pro")
        );
        assert_eq!(machine_of_host("build_box").as_deref(), Some("build-box"));
        assert_eq!(machine_of_host("server").as_deref(), Some("server"));
        assert_eq!(machine_of_host("...").as_deref(), None);
    }
}
