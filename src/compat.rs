//! The versions of the outside tools bridges run that this binary was tested
//! with, and how what's installed compares.
//!
//! A bridge drives an agent CLI it doesn't ship: Claude Code, pi and the
//! node it runs on, or CodeSmith. A release is tested with particular
//! versions of each.
//! One older than tested may lack a flag or behave as an earlier release
//! did, so it's something to fix; one newer is only said.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::bridge::Kind;
use crate::config::{APP_NAME, Config, Paths};

/// Each tool, and the versions this release was tested with. A tool with
/// more than one release line, node, is compared within its line.
pub const TESTED: &[(&str, &[&str])] = &[
    ("claude", &["2.1.325"]),
    ("pi", &["1.5.2"]),
    // A codesmith bridge passes `--output final`, which CodeSmith 0.7.0 added.
    ("codesmith", &["0.8.0"]),
    ("node", &["22.22.1", "24.22.0"]),
];

/// How long a tool gets to say its version.
const ASK_TIMEOUT: Duration = Duration::from_secs(20);

/// The numbers of a version, such as `[2, 1, 325]` for "v2.1.325".
pub fn numbers(version: &str) -> Option<Vec<u64>> {
    let v = version.trim().trim_start_matches('v');
    let parts: Option<Vec<u64>> = v.split('.').map(|p| p.parse().ok()).collect();
    parts.filter(|p| !p.is_empty())
}

/// The first version-looking word in a tool's `--version` output.
pub fn version_in(text: &str) -> Option<String> {
    text.split(|c: char| c.is_whitespace() || c == '(' || c == ')' || c == ',')
        .map(|w| w.trim_start_matches('v'))
        .find(|w| w.contains('.') && numbers(w).is_some_and(|n| n.len() >= 2))
        .map(str::to_string)
}

/// Whether version `a` is later than `b`.
pub fn later(a: &str, b: &str) -> bool {
    match (numbers(a), numbers(b)) {
        (Some(a), Some(b)) => a > b,
        _ => false,
    }
}

/// How a version compares with what was tested.
#[derive(Debug, PartialEq, Eq)]
pub enum Fit {
    Tested,
    Older(String),
    Newer(String),
    /// A release line none of the tested versions is on.
    OtherLine(Vec<String>),
}

/// How `version` of `tool` compares, or None for a tool not in [`TESTED`].
pub fn fit(tool: &str, version: &str) -> Option<Fit> {
    let (_, tested) = TESTED.iter().find(|(t, _)| *t == tool)?;
    let have = numbers(version)?;
    let line: Vec<&&str> = if tested.len() > 1 {
        tested
            .iter()
            .filter(|t| numbers(t).and_then(|n| n.first().copied()) == have.first().copied())
            .collect()
    } else {
        tested.iter().collect()
    };
    let Some(want) = line.first() else {
        return Some(Fit::OtherLine(
            tested.iter().map(|t| t.to_string()).collect(),
        ));
    };
    let want_n = numbers(want)?;
    Some(if have == want_n {
        Fit::Tested
    } else if have < want_n {
        Fit::Older(want.to_string())
    } else {
        Fit::Newer(want.to_string())
    })
}

/// What a check says, as `setup refresh` prints it.
#[derive(Debug, PartialEq, Eq)]
pub enum Level {
    Ok,
    Note,
    Fix,
}

/// The line for a bridge's tool at a version.
pub fn said(bridge: &str, tool: &str, version: Option<&str>) -> Option<(Level, String)> {
    let Some(version) = version else {
        return Some((
            Level::Note,
            format!("the {bridge} bridge's {tool} didn't say its version"),
        ));
    };
    Some(match fit(tool, version)? {
        Fit::Tested => (
            Level::Ok,
            format!(
                "the {bridge} bridge's {tool} is {version}, which this {APP_NAME} was tested with"
            ),
        ),
        Fit::Older(want) => (
            Level::Fix,
            format!(
                "the {bridge} bridge's {tool} is {version}, older than {want}, which this \
                 {APP_NAME} was tested with: update it"
            ),
        ),
        Fit::Newer(want) => (
            Level::Note,
            format!(
                "the {bridge} bridge's {tool} is {version}, newer than {want}, which this \
                 {APP_NAME} was tested with"
            ),
        ),
        Fit::OtherLine(tested) => (
            Level::Note,
            format!(
                "the {bridge} bridge's {tool} is {version}; this {APP_NAME} was tested with {}",
                tested.join(" and ")
            ),
        ),
    })
}

/// Asks a program for its version, on a bridge's PATH, giving up after a
/// while.
fn ask_version(program: &Path, path: &str) -> Option<String> {
    let mut command = Command::new(program);
    command
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if !path.is_empty() {
        command.env("PATH", path);
    }
    let mut child = command.spawn().ok()?;
    let until = Instant::now() + ASK_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < until => std::thread::sleep(Duration::from_millis(50)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    let out = child.wait_with_output().ok()?;
    version_in(&String::from_utf8_lossy(&out.stdout))
}

/// A line for the tools each bridge here runs: its agent CLI, and node for
/// pi, each against what this release was tested with.
pub fn bridge_versions(paths: &Paths) -> Vec<(Level, String)> {
    let Ok(cfg) = Config::load(&paths.config) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for agent in cfg.bridges.keys() {
        let Ok(settings) = crate::bridge::load(paths, agent) else {
            continue;
        };
        let tools: &[&str] = match settings.agent_kind() {
            Some(Kind::Claude) => &["claude"],
            Some(Kind::Pi) => &["pi", "node"],
            Some(Kind::Codesmith) => &["codesmith"],
            _ => continue,
        };
        for tool in tools {
            let program = if *tool == "node" {
                Path::new("node").to_path_buf()
            } else {
                settings.command.clone()
            };
            let version = ask_version(&program, &settings.path);
            out.extend(said(agent, tool, version.as_deref()));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_older_codesmith_is_something_to_fix() {
        assert_eq!(version_in("codesmith 0.8.0").as_deref(), Some("0.8.0"));
        assert_eq!(said("w", "codesmith", Some("0.8.0")).unwrap().0, Level::Ok);
        assert_eq!(said("w", "codesmith", Some("0.6.1")).unwrap().0, Level::Fix);
        assert_eq!(
            said("w", "codesmith", Some("0.9.0")).unwrap().0,
            Level::Note
        );
    }

    #[test]
    fn a_version_is_read_from_what_a_tool_prints() {
        assert_eq!(
            version_in("2.1.325 (Claude Code)").as_deref(),
            Some("2.1.325")
        );
        assert_eq!(version_in("v24.22.0\n").as_deref(), Some("24.22.0"));
        assert_eq!(version_in("pi 1.5.2").as_deref(), Some("1.5.2"));
        assert_eq!(version_in("no version here"), None);
        assert!(later("0.1.35", "0.1.34") && !later("0.1.9", "0.1.34"));
    }

    #[test]
    fn older_than_tested_is_to_fix_and_newer_is_only_said() {
        assert_eq!(fit("claude", "2.1.325"), Some(Fit::Tested));
        assert_eq!(fit("claude", "2.1.300"), Some(Fit::Older("2.1.325".into())));
        assert_eq!(fit("claude", "2.2.0"), Some(Fit::Newer("2.1.325".into())));
        // node is compared within its own release line.
        assert_eq!(fit("node", "v22.1.0"), Some(Fit::Older("22.22.1".into())));
        assert_eq!(fit("node", "24.22.0"), Some(Fit::Tested));
        assert!(matches!(fit("node", "20.0.0"), Some(Fit::OtherLine(_))));
        assert_eq!(fit("omp", "1.0.0"), None, "omp isn't pinned");
        let (level, text) = said("claude-acr", "claude", Some("2.1.1")).unwrap();
        assert_eq!(level, Level::Fix);
        assert!(
            text.contains("older than 2.1.325") && text.contains("update it"),
            "{text}"
        );
        let (level, _) = said("pi", "pi", Some("1.6.0")).unwrap();
        assert_eq!(level, Level::Note);
        assert_eq!(said("pi", "pi", None).unwrap().0, Level::Note);
    }
}
