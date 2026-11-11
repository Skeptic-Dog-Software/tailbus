//! Whether a repository's review config is calibrated. Its keys come from
//! one full ACR of a real change, and `acr calibrate` has the pipeline's
//! Judge rule on their findings. A person then reads where the two
//! disagreed and accepts it, with `acr calibration --accept`. A full review's
//! rulings aren't ground truth, so the bar isn't strict agreement: it's that
//! a person has read the disagreements, twice on the same config and Judge.
//!
//! `acr run` refuses a repository that was never calibrated, or whose config
//! has changed since, and warns when only the Judge has: a new Judge prompt
//! or rule is tailbus's doing, not the repository's.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Runs kept for each repository.
const KEPT: usize = 20;

/// Runs on the same config and Judge before a calibration can be accepted:
/// one result could be the Judge's luck.
pub const RUNS_NEEDED: usize = 2;

/// What a calibration was made against.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fingerprint {
    /// The repository's config and its decided non-issues.
    pub config: String,
    /// The Judge's binding, its prompt, and the rules that decide what
    /// blocks.
    pub judge: String,
}

/// One `acr calibrate` of a repository's keys.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CalibrationRun {
    pub at: i64,
    pub keys: Vec<String>,
    pub fingerprint: Fingerprint,
    pub agreed: usize,
    pub missed: usize,
    pub overrated: usize,
    /// Key findings the Judge rated Rejected: real ones lost, unless the
    /// Judge is right that they aren't defects.
    pub dropped: usize,
}

/// A person read a calibration's disagreements and accepted it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Accepted {
    pub at: i64,
    pub fingerprint: Fingerprint,
    /// The runs it was accepted on.
    pub runs: usize,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Record {
    #[serde(default)]
    pub runs: Vec<CalibrationRun>,
    #[serde(default)]
    pub accepted: Option<Accepted>,
}

impl Record {
    /// The runs made against `now`, latest last.
    pub fn runs_on(&self, now: &Fingerprint) -> Vec<&CalibrationRun> {
        self.runs.iter().filter(|r| &r.fingerprint == now).collect()
    }
}

/// Where a repository stands for `acr run`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Standing {
    Never,
    ConfigChanged,
    JudgeChanged,
    Calibrated,
}

pub fn standing(record: &Record, now: &Fingerprint) -> Standing {
    match &record.accepted {
        None => Standing::Never,
        Some(a) if a.fingerprint.config != now.config => Standing::ConfigChanged,
        Some(a) if a.fingerprint.judge != now.judge => Standing::JudgeChanged,
        Some(_) => Standing::Calibrated,
    }
}

/// A short, stable hash of some texts, in order.
pub fn hash(parts: &[&str]) -> String {
    let mut h = Sha256::new();
    for p in parts {
        h.update(p.as_bytes());
        h.update([0]);
    }
    h.finalize()[..12]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The functions named, cut from a source file's text: each runs from its
/// `fn` line to the first line that's only `}`. Used to fingerprint the
/// rules, so changing them voids calibrations made before.
pub fn source_of(src: &str, starts: &[&str]) -> String {
    starts
        .iter()
        .filter_map(|start| {
            let i = src.find(start)?;
            let end = src[i..].find("\n}\n")?;
            Some(&src[i..i + end])
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn path(state: &Path, repo: &str) -> PathBuf {
    state.join("acr-calibration").join(format!("{repo}.json"))
}

pub fn load(state: &Path, repo: &str) -> Result<Record> {
    let path = path(state, repo);
    match fs::read_to_string(&path) {
        Ok(text) => {
            serde_json::from_str(&text).with_context(|| format!("reading {}", path.display()))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Record::default()),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

fn save(state: &Path, repo: &str, record: &Record) -> Result<()> {
    let path = path(state, repo);
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_string_pretty(record)?)?;
    fs::rename(&tmp, &path)?;
    Ok(())
}

/// Records a run, keeping the latest few.
pub fn add_run(state: &Path, repo: &str, run: CalibrationRun) -> Result<Record> {
    let mut record = load(state, repo)?;
    record.runs.push(run);
    let over = record.runs.len().saturating_sub(KEPT);
    record.runs.drain(..over);
    save(state, repo, &record)?;
    Ok(record)
}

/// Accepts the calibration made against `now`, once it has run enough.
pub fn accept(state: &Path, repo: &str, now: &Fingerprint, at: i64) -> Result<Accepted> {
    let mut record = load(state, repo)?;
    let runs = record.runs_on(now).len();
    if runs < RUNS_NEEDED {
        bail!(
            "{repo} has {runs} calibration run(s) on its current config and Judge; it needs \
             {RUNS_NEEDED}: run `tailbus acr calibrate` on its keys again first"
        );
    }
    let accepted = Accepted {
        at,
        fingerprint: now.clone(),
        runs,
    };
    record.accepted = Some(accepted.clone());
    save(state, repo, &record)?;
    Ok(accepted)
}

/// Where a repository's calibration stands, and its runs on the current
/// config and Judge.
pub fn report(name: &str, record: &Record, now: &Fingerprint) -> String {
    let stands = match standing(record, now) {
        Standing::Calibrated => "calibrated".to_string(),
        Standing::JudgeChanged => "calibrated, but on an earlier Judge: recalibrate soon".into(),
        Standing::ConfigChanged => "not calibrated: its config has changed since".into(),
        Standing::Never => "not calibrated".into(),
    };
    let mut out = format!("{name}: {stands}.");
    let runs = record.runs_on(now);
    if runs.is_empty() {
        out.push_str("\nNo calibration runs on its current config and Judge.");
    }
    for r in runs {
        out.push_str(&format!(
            "\n- {}: blockers agreed {}, missed {}, overrated {}; dropped {} ({})",
            crate::util::rfc3339(r.at),
            r.agreed,
            r.missed,
            r.overrated,
            r.dropped,
            r.keys.join(", ")
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fp(config: &str, judge: &str) -> Fingerprint {
        Fingerprint {
            config: config.into(),
            judge: judge.into(),
        }
    }

    fn run(f: &Fingerprint) -> CalibrationRun {
        CalibrationRun {
            at: 1,
            keys: vec!["k".into()],
            fingerprint: f.clone(),
            agreed: 1,
            missed: 0,
            overrated: 0,
            dropped: 0,
        }
    }

    #[test]
    fn a_calibration_is_accepted_after_two_runs_and_voided_by_a_new_config() {
        let state = std::env::temp_dir().join(format!("tb-cal-{}", crate::util::new_id()));
        let now = fp("c1", "j1");
        assert_eq!(standing(&load(&state, "r").unwrap(), &now), Standing::Never);
        add_run(&state, "r", run(&now)).unwrap();
        // One run isn't enough, nor is one on another config.
        add_run(&state, "r", run(&fp("c0", "j1"))).unwrap();
        let err = accept(&state, "r", &now, 2).unwrap_err().to_string();
        assert!(err.contains("needs 2"), "{err}");
        add_run(&state, "r", run(&now)).unwrap();
        accept(&state, "r", &now, 3).unwrap();
        let record = load(&state, "r").unwrap();
        assert_eq!(standing(&record, &now), Standing::Calibrated);
        // A changed config refuses; a changed Judge only warns.
        assert_eq!(standing(&record, &fp("c2", "j1")), Standing::ConfigChanged);
        assert_eq!(standing(&record, &fp("c1", "j2")), Standing::JudgeChanged);
        fs::remove_dir_all(&state).unwrap();
    }

    #[test]
    fn only_the_latest_runs_are_kept() {
        let state = std::env::temp_dir().join(format!("tb-cal-{}", crate::util::new_id()));
        let now = fp("c", "j");
        for _ in 0..KEPT + 5 {
            add_run(&state, "r", run(&now)).unwrap();
        }
        assert_eq!(load(&state, "r").unwrap().runs.len(), KEPT);
        fs::remove_dir_all(&state).unwrap();
    }

    #[test]
    fn a_function_is_cut_from_its_source() {
        let src = "fn a() {\n    1\n}\n\nfn b() {\n    2\n}\n";
        assert_eq!(source_of(src, &["fn b("]), "fn b() {\n    2");
        assert_eq!(hash(&["x", "y"]), hash(&["x", "y"]));
        assert_ne!(hash(&["x", "y"]), hash(&["xy", ""]));
    }
}
