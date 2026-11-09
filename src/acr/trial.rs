//! A blind trial of a review design on a change whose findings are known:
//! the benchmark that says which design catches what, and at what cost.
//!
//! Each trial checks the change's head out in a throwaway worktree. It
//! runs the design with nobody in the loop, and has a scorer compare what
//! it found with an answer key. It leaves nothing behind: the worktree,
//! the temporary Claude config and its transcripts all go.
//!
//! This is tooling for measuring designs, so unlike the rest of ACR it
//! names the agent it runs: headless Claude Code.

use std::fs;
use std::io::Write;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::ValueEnum;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::config::Paths;
use crate::util::new_id;

/// The method a builder follows in a trial, given to it as its ACR skill.
const SKILL: &str = include_str!("trial/skill.md");
/// The builder in a trial: the agent that knows the change.
const BUILDER_MODEL: &str = "claude-opus-5-5";
/// The scorer, which only compares lists.
const SCORER_MODEL: &str = "claude-sonnet-5-5";
/// The longest a builder's run may take.
/// Caps a trial should never reach: a whole review can take a while.
const BUILDER_TIMEOUT: Duration = Duration::from_secs(6 * 3600);
const SCORER_TIMEOUT: Duration = Duration::from_secs(3600);

/// What a change's in-session review found, as its builder ruled.
#[derive(Debug, Clone, Deserialize)]
pub struct Key {
    pub repo: PathBuf,
    #[serde(default)]
    pub pr: Option<u64>,
    #[serde(deserialize_with = "commit")]
    pub base: String,
    #[serde(deserialize_with = "commit")]
    pub head: String,
    /// What the change is for: all a trial's builder is told.
    pub context: String,
    pub dimensions: Vec<String>,
    /// What review found. Left out for new work, whose trials run before
    /// its own review, and which is scored once that review is done.
    #[serde(default)]
    pub findings: Vec<KeyFinding>,
}

/// A commit, which YAML reads as a number when it's all digits.
fn commit<'de, D: serde::Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Either {
        Text(String),
        Number(u64),
    }
    Ok(match Either::deserialize(d)? {
        Either::Text(s) => s,
        Either::Number(n) => n.to_string(),
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyFinding {
    pub id: String,
    pub blocker: bool,
    pub dimension: String,
    pub what: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub how_found: Option<String>,
    #[serde(default)]
    pub found_later: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum Arm {
    /// Builder-led, every reviewer at one size.
    OneSize,
    /// Builder-led, each reviewer sized to its dimension.
    Sized,
    /// The separate-roles pipeline: router, orchestrator, reviewers, judge.
    Separated,
}

/// A finding as a design reported it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Found {
    #[serde(default)]
    pub dimension: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub detail: String,
    #[serde(default)]
    pub file: String,
    #[serde(default)]
    pub blocker: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Counts {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub total: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Matched {
    pub id: String,
    pub blocker: bool,
    /// The earlier review found it only after round 1.
    pub found_later: bool,
    pub caught: bool,
    pub as_blocker: bool,
}

/// What a design caught of the key. The main counts are against what the
/// earlier review found in its own round 1; `later_` counts are for defects
/// already there that it found only in later rounds, which a round-1 trial
/// may or may not catch.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Score {
    pub blockers: usize,
    pub blockers_caught: usize,
    /// Caught, and ruled blockers.
    pub blockers_held: usize,
    pub non_blockers: usize,
    pub non_blockers_caught: usize,
    pub later_blockers: usize,
    pub later_blockers_caught: usize,
    pub later_blockers_held: usize,
    pub later_non_blockers: usize,
    pub later_non_blockers_caught: usize,
    /// Findings that match nothing in the key, which the scorer thought real.
    pub extra_plausible: usize,
    /// And those it thought noise.
    pub extra_noise: usize,
    pub per_finding: Vec<Matched>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Outcome {
    pub key: String,
    pub pr: Option<u64>,
    pub arm: Arm,
    pub seconds: u64,
    pub tokens: Counts,
    /// The models the design's calls ran on.
    pub models: Vec<String>,
    /// Against the key. None until the key has findings: a change reviewed
    /// before anyone knew what was wrong with it is scored later, with
    /// `tailbus acr rescore`.
    #[serde(default)]
    pub score: Option<Score>,
    pub findings: Vec<Found>,
    /// What went wrong in the design's run, such as a Judge whose answer
    /// couldn't be read. A score beside failures says less than it seems.
    #[serde(default)]
    pub failures: Vec<String>,
}

/// A trial's scratch folder and its copy of the change, removed however it
/// ends.
struct Scratch {
    dir: PathBuf,
    tree: PathBuf,
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// A fresh repository holding the head and only what it reaches: no branches,
/// no remote, and no later commit, so nothing in a trial can see how the
/// change turned out. It's named after the repository, so its review config
/// is found.
/// The key with its base and head as full commit names, since the trial's
/// copy can fetch a commit only by its full name. The base has to be in the
/// head's history: nothing else is in the copy, and a branch that has moved
/// past the change would bring its fixes in.
fn resolved(mut key: Key) -> Result<Key> {
    let commit = |rev: &str| -> Result<String> {
        let full = super::change::git(
            &key.repo,
            &["rev-parse", "--verify", &format!("{rev}^{{commit}}")],
        )
        .with_context(|| format!("{rev} isn't a commit in {}", key.repo.display()))?;
        Ok(full.trim().to_owned())
    };
    let head = commit(&key.head)?;
    let base = commit(&key.base)?;
    let ancestor = Command::new("git")
        .arg("-C")
        .arg(&key.repo)
        .args(["merge-base", "--is-ancestor", &base, &head])
        .stdin(Stdio::null())
        .status()
        .context("running git")?;
    if !ancestor.success() {
        bail!(
            "the key's base {} isn't in the history of its head {}: give the commit the change was made on",
            key.base,
            key.head
        );
    }
    key.base = base;
    key.head = head;
    Ok(key)
}

fn scratch(paths: &Paths, key: &Key) -> Result<Scratch> {
    let dir = paths.state.join("acr-trials").join(new_id());
    fs::create_dir_all(&dir)?;
    let dir = dir.canonicalize()?;
    let name = super::change::name(&key.repo)?;
    let s = Scratch {
        tree: dir.join(&name),
        dir,
    };
    let git = |args: &[&str], env: &[(&str, &str)]| -> Result<()> {
        let out = Command::new("git")
            .args(args)
            .envs(env.iter().copied())
            .stdin(Stdio::null())
            .output()
            .context("running git")?;
        if !out.status.success() {
            bail!(
                "making the trial's copy of {} at {}: git {}: {}",
                key.repo.display(),
                key.head,
                args.first().copied().unwrap_or_default(),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(())
    };
    let tree = s.tree.to_string_lossy().into_owned();
    let repo = key.repo.to_string_lossy().into_owned();
    git(&["init", "--quiet", &tree], &[])?;
    // A local fetch runs the repository's upload-pack, which this lets give
    // one commit by name without a branch that points at it.
    git(
        &[
            "-C",
            &tree,
            "-c",
            "uploadpack.allowAnySHA1InWant=true",
            "fetch",
            "--quiet",
            "--no-tags",
            &repo,
            &key.head,
        ],
        &[],
    )?;
    git(
        &["-C", &tree, "checkout", "--quiet", "--detach", "FETCH_HEAD"],
        &[("GIT_LFS_SKIP_SMUDGE", "1")],
    )?;
    // Large files come from the repository's own store, which the copy
    // doesn't keep a link to.
    let lfs = Command::new("git")
        .args(["-C", &tree, "lfs", "ls-files", "--name-only"])
        .stdin(Stdio::null())
        .output();
    if lfs.is_ok_and(|o| o.status.success() && !o.stdout.is_empty()) {
        let common = super::change::git(
            &key.repo,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )?;
        let storage = format!("lfs.storage={}/lfs", common.trim());
        git(&["-C", &tree, "-c", &storage, "lfs", "checkout"], &[])?;
    }
    Ok(s)
}

/// The settings that keep a trial's Claude Code, its subagents and their
/// commands to the trial's copy, its scratch space and the toolchains they
/// need. A trial won't run without them.
fn confine_settings(paths: &Paths) -> Result<PathBuf> {
    let path = paths
        .config
        .with_file_name("acr")
        .join(crate::setup::CONFINE_FILE);
    if !path.is_file() {
        bail!(
            "{} is missing, which keeps reviewers to the code under review: run `tailbus setup acr`",
            path.display()
        );
    }
    Ok(path)
}

/// One headless Claude Code run.
struct Ask<'a> {
    model: &'a str,
    effort: &'a str,
    tools: &'a str,
    prompt: &'a str,
    limit: Duration,
}

/// Runs headless Claude Code with its own config folder, so it loads none of
/// the user's settings or plugins and leaves its transcripts in `config`.
/// Returns what it printed.
fn claude(paths: &Paths, config: &Path, workdir: &Path, ask: &Ask) -> Result<String> {
    let Ask {
        model,
        effort,
        tools,
        prompt,
        limit,
    } = *ask;
    let confine = confine_settings(paths)?;
    let token_file = paths.config.with_file_name("claude-token");
    let token = fs::read_to_string(&token_file)
        .with_context(|| format!("reading {}: run `tailbus setup acr`", token_file.display()))?;
    let tmp = config.with_file_name(format!(
        "{}-tmp",
        config.file_name().unwrap_or_default().to_string_lossy()
    ));
    fs::create_dir_all(&tmp)?;
    let out = config.join("out.txt");
    let mut child = Command::new(crate::setup::find_claude()?)
        .args(["-p", "--model", model, "--effort", effort])
        .args(["--strict-mcp-config", "--output-format", "text"])
        .args(["--allowedTools", tools, "--tools", tools])
        .arg("--settings")
        .arg(&confine)
        .arg("--add-dir")
        .arg(config.parent().unwrap_or(config))
        .current_dir(workdir)
        .env("CLAUDE_CONFIG_DIR", config)
        .env("CLAUDE_CODE_OAUTH_TOKEN", token.trim())
        .env("TMPDIR", &tmp)
        .stdin(Stdio::piped())
        .stdout(fs::File::create(&out)?)
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .context("starting claude")?;
    child
        .stdin
        .take()
        .context("claude's stdin")?
        .write_all(prompt.as_bytes())?;
    let until = Instant::now() + limit;
    loop {
        if child.try_wait()?.is_some() {
            break;
        }
        if Instant::now() >= until {
            // SAFETY: signals the process group it leads, which this started.
            unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
            let _ = child.wait();
            bail!("it ran past {} minutes", limit.as_secs() / 60);
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    Ok(fs::read_to_string(&out).unwrap_or_default())
}

/// The tokens every session in a Claude config folder used, subagents too.
fn measured(config: &Path) -> (Counts, Vec<String>) {
    let mut counts = Counts::default();
    let mut models = std::collections::BTreeSet::new();
    let Ok(projects) = fs::read_dir(config.join("projects")) else {
        return (counts, Vec::new());
    };
    for project in projects.flatten() {
        let Ok(entries) = fs::read_dir(project.path()) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|x| x != "jsonl") {
                continue;
            }
            if let Ok(usage) = crate::usage::measure(&path, 0) {
                let t = usage.total();
                counts.input += t.input;
                counts.output += t.output;
                counts.cache_read += t.cache_reads;
                counts.cache_write += t.cache_writes;
                models.extend(usage.models);
            }
        }
    }
    counts.total = counts.input + counts.output + counts.cache_read + counts.cache_write;
    (counts, models.into_iter().collect())
}

/// The last JSON object in an agent's answer, past any prose or fences.
/// Objects nested in another don't count on their own.
pub fn last_json(text: &str) -> Option<Value> {
    let mut found = None;
    let mut at = 0;
    while let Some(offset) = text[at..].find('{') {
        let start = at + offset;
        let mut stream = serde_json::Deserializer::from_str(&text[start..]).into_iter::<Value>();
        match stream.next() {
            Some(Ok(value)) if value.is_object() => {
                found = Some(value);
                at = start + stream.byte_offset();
            }
            _ => at = start + 1,
        }
    }
    found
}

/// The dimensions, each with what wrong means for it in this repository,
/// where its review config says.
fn dimensions_text(key: &Key, criteria: &[(String, String)]) -> String {
    key.dimensions
        .iter()
        .map(|d| match criteria.iter().find(|(id, _)| id == d) {
            Some((_, c)) => format!("- {d}: {}", c.trim()),
            None => format!("- {d}"),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn builder_prompt(key: &Key, arm: Arm, result: &Path, criteria: &[(String, String)]) -> String {
    let sizing = match arm {
        Arm::OneSize => {
            "Run every reviewer on opus at xhigh effort, whatever its dimension: this trial \
             tests one size for all."
        }
        _ => "Size each reviewer as the skill says.",
    };
    format!(
        "You built this change. It's the range {base}..HEAD in this working directory, where \
         HEAD is {head}.\n\n\
         What it's for:\n{context}\n\n\
         Review it with the acr skill: load it with the Skill tool and follow it. The \
         dimensions, each with this repository's criteria for it where it has them, which \
         set its bar:\n{dimensions}\n\n{sizing}\n\n\
         Run round 1 only: brief the reviewers, run them, and judge what they find. Fix \
         nothing, and change nothing in this working directory.\n\n\
         Then write your judged findings as JSON to {result}, in this shape:\n\
         {{\"reviewers\": [{{\"dimension\": \"\", \"model\": \"\", \"effort\": \"\"}}],\n \
         \"findings\": [{{\"dimension\": \"\", \"title\": \"\", \"detail\": \"\", \"file\": \"\", \
         \"blocker\": true, \"ruling\": \"fold or reject\", \"reason\": \"\"}}]}}\n\
         Keep a finding you reject in the list, with blocker false and ruling reject.",
        base = key.base,
        head = key.head,
        context = key.context.trim(),
        dimensions = dimensions_text(key, criteria),
        result = result.display(),
    )
}

/// The builder-led designs: a fresh headless builder with the method as its
/// skill, sized one way or the other.
fn builder_led(paths: &Paths, s: &Scratch, key: &Key, arm: Arm) -> Result<Ran> {
    let config = s.dir.join("builder");
    let skill = config.join("skills").join("acr");
    fs::create_dir_all(&skill)?;
    fs::write(skill.join("SKILL.md"), SKILL)?;
    let result = s.dir.join("result.json");
    let criteria = super::lens_criteria(paths, &s.tree, key);
    let prompt = builder_prompt(key, arm, &result, &criteria);
    claude(
        paths,
        &config,
        &s.tree,
        &Ask {
            model: BUILDER_MODEL,
            effort: "high",
            tools: "Skill,Agent,Bash,Read,Grep,Glob,Write,Edit,TodoWrite",
            prompt: &prompt,
            limit: BUILDER_TIMEOUT,
        },
    )?;
    let (counts, models) = measured(&config);
    let text = fs::read_to_string(&result)
        .with_context(|| format!("the builder wrote no {}", result.display()))?;
    let value = last_json(&text).context("the builder's result isn't JSON")?;
    let findings = value["findings"]
        .as_array()
        .map(|list| {
            list.iter()
                .filter_map(|f| serde_json::from_value::<Found>(f.clone()).ok())
                .collect()
        })
        .unwrap_or_default();
    Ok(Ran {
        findings,
        tokens: counts,
        models,
        failures: Vec::new(),
    })
}

fn score(paths: &Paths, dir: &Path, key: &Key, found: &[Found]) -> Result<Score> {
    let config = dir.join("scorer");
    fs::create_dir_all(&config)?;
    let keyed: Vec<Value> = key
        .findings
        .iter()
        .map(
            |f| json!({"id": f.id, "dimension": f.dimension, "blocker": f.blocker, "what": f.what}),
        )
        .collect();
    let numbered: Vec<Value> = found
        .iter()
        .enumerate()
        .map(|(i, f)| {
            json!({"index": i, "dimension": f.dimension, "title": f.title,
                   "detail": f.detail, "file": f.file, "blocker": f.blocker})
        })
        .collect();
    let prompt = format!(
        "You score a code review against an answer key.\n\n\
         For each key finding: is the same defect among the review's findings? The same \
         defect means the same root cause in the same place; different wording is fine. Give \
         the indices of the review findings that match it, and say whether the review ruled \
         any of them a blocker.\n\n\
         Then, for each review finding that matches no key finding, say whether it's plausibly \
         a real defect or noise, with a few words why.\n\n\
         Answer key:\n{key}\n\n\
         The review's findings:\n{review}\n\n\
         Answer with only JSON:\n\
         {{\"key\": [{{\"id\": \"\", \"caught\": true, \"matches\": [0], \"as_blocker\": true}}],\n \
         \"extra\": [{{\"index\": 0, \"plausible\": true, \"why\": \"\"}}]}}",
        key = serde_json::to_string_pretty(&keyed)?,
        review = serde_json::to_string_pretty(&numbered)?,
    );
    let answer = claude(
        paths,
        &config,
        dir,
        &Ask {
            model: SCORER_MODEL,
            effort: "high",
            tools: "Read",
            prompt: &prompt,
            limit: SCORER_TIMEOUT,
        },
    )?;
    let value = last_json(&answer).context("the scorer's answer isn't JSON")?;
    let mut per_finding = Vec::new();
    for f in &key.findings {
        let row = value["key"]
            .as_array()
            .and_then(|rows| rows.iter().find(|r| r["id"] == f.id.as_str()));
        let caught = row.is_some_and(|r| r["caught"].as_bool() == Some(true));
        let as_blocker = caught && row.is_some_and(|r| r["as_blocker"].as_bool() == Some(true));
        per_finding.push(Matched {
            id: f.id.clone(),
            blocker: f.blocker,
            found_later: f.found_later,
            caught,
            as_blocker,
        });
    }
    let extra = value["extra"].as_array().cloned().unwrap_or_default();
    let plausible = extra
        .iter()
        .filter(|e| e["plausible"].as_bool() == Some(true))
        .count();
    let count = |blocker: bool, later: bool, test: &dyn Fn(&Matched) -> bool| {
        per_finding
            .iter()
            .filter(|m| m.blocker == blocker && m.found_later == later && test(m))
            .count()
    };
    Ok(Score {
        blockers: count(true, false, &|_| true),
        blockers_caught: count(true, false, &|m| m.caught),
        blockers_held: count(true, false, &|m| m.as_blocker),
        non_blockers: count(false, false, &|_| true),
        non_blockers_caught: count(false, false, &|m| m.caught),
        later_blockers: count(true, true, &|_| true),
        later_blockers_caught: count(true, true, &|m| m.caught),
        later_blockers_held: count(true, true, &|m| m.as_blocker),
        later_non_blockers: count(false, true, &|_| true),
        later_non_blockers_caught: count(false, true, &|m| m.caught),
        extra_plausible: plausible,
        extra_noise: extra.len() - plausible,
        per_finding,
    })
}

/// The separate-roles design runs through `tailbus acr`'s own pipeline,
/// which ACR's module provides.
/// What a design did: what it found, what it used, and what went wrong
/// along the way, so that a design that broke isn't scored as one that
/// found nothing.
#[derive(Debug, Default)]
pub struct Ran {
    pub findings: Vec<Found>,
    pub tokens: Counts,
    pub models: Vec<String>,
    pub failures: Vec<String>,
}

pub type Separated<'a> = &'a dyn Fn(&Path, &Key) -> Result<Ran>;

/// Runs one design on one change, and scores it.
fn read_key(key_path: &Path) -> Result<Key> {
    serde_yaml_ng::from_str(
        &fs::read_to_string(key_path).with_context(|| format!("reading {}", key_path.display()))?,
    )
    .with_context(|| format!("in {}", key_path.display()))
}

/// What a key is called in results: its file's name.
fn key_name(key_path: &Path) -> String {
    key_path
        .file_stem()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

pub fn run(paths: &Paths, key_path: &Path, arm: Arm, separated: Separated) -> Result<Outcome> {
    let key = resolved(read_key(key_path)?)?;
    let s = scratch(paths, &key)?;
    let started = Instant::now();
    let ran = match arm {
        Arm::OneSize | Arm::Sized => builder_led(paths, &s, &key, arm)?,
        Arm::Separated => separated(&s.tree, &key)?,
    };
    let seconds = started.elapsed().as_secs();
    // New work has no findings yet: it's scored once its own review is done.
    let score = if key.findings.is_empty() {
        None
    } else {
        Some(score(paths, &s.dir, &key, &ran.findings)?)
    };
    Ok(Outcome {
        key: key_name(key_path),
        pr: key.pr,
        arm,
        seconds,
        tokens: ran.tokens,
        models: ran.models,
        score,
        findings: ran.findings,
        failures: ran.failures,
    })
}

/// Scores the saved runs on a key that aren't scored yet, now that the key
/// has findings, and rewrites `results` with them in place. Other lines stay
/// as they are. Returns how many it scored.
pub fn rescore(paths: &Paths, key_path: &Path, results: &Path) -> Result<usize> {
    let key = read_key(key_path)?;
    if key.findings.is_empty() {
        bail!(
            "{} has no findings yet: add them from the change's own review first",
            key_path.display()
        );
    }
    let name = key_name(key_path);
    let text =
        fs::read_to_string(results).with_context(|| format!("reading {}", results.display()))?;
    let dir = paths.state.join("acr-trials").join(new_id());
    fs::create_dir_all(&dir)?;
    let s = Scratch {
        tree: dir.clone(),
        dir: dir.canonicalize()?,
    };
    let mut out = String::new();
    let mut scored = 0;
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let value: Value = serde_json::from_str(line).context("reading a results line")?;
        if value["key"] == name.as_str() && value["score"].is_null() {
            let mut outcome: Outcome = serde_json::from_value(value)?;
            outcome.score = Some(score(paths, &s.dir, &key, &outcome.findings)?);
            out.push_str(&serde_json::to_string(&outcome)?);
            scored += 1;
        } else {
            out.push_str(line);
        }
        out.push('\n');
    }
    let tmp = results.with_extension("jsonl.rescore");
    fs::write(&tmp, out)?;
    fs::rename(&tmp, results)?;
    Ok(scored)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rev(dir: &Path, what: &str) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["rev-parse", what])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    #[test]
    fn a_trials_copy_holds_the_head_and_nothing_after_it() {
        use super::super::change::tests::{branch, commit, repo_with};
        let repo = repo_with(&[("a.txt", "one\n")]);
        branch(&repo, "work");
        commit(&repo, &[("a.txt", "two\n")], "the change");
        let head = rev(&repo, "HEAD");
        commit(&repo, &[("a.txt", "fixed\n")], "the fix");
        let fixed = rev(&repo, "HEAD");
        let dir = std::env::temp_dir().join(format!("tb-trial-paths-{}", new_id()));
        let paths = Paths {
            state: dir.join("state"),
            config: dir.join("config.toml"),
        };
        // Keys name commits the way people write them: short, or by branch.
        let key = Key {
            repo: repo.clone(),
            pr: None,
            base: "main".into(),
            head: head[..8].into(),
            context: "c".into(),
            dimensions: vec!["correctness".into()],
            findings: Vec::new(),
        };
        let past = Key {
            base: fixed.clone(),
            ..key.clone()
        };
        let err = resolved(past).unwrap_err().to_string();
        assert!(err.contains("isn't in the history"), "{err}");
        let key = resolved(key).unwrap();
        assert_eq!(key.head, head);
        let s = scratch(&paths, &key).unwrap();
        assert_eq!(rev(&s.tree, "HEAD"), head);
        assert_eq!(rev(&s.tree, &key.base), rev(&repo, "main"));
        assert_eq!(fs::read_to_string(s.tree.join("a.txt")).unwrap(), "two\n");
        // Named after the repository, so its review config is found.
        assert_eq!(
            super::super::change::name(&s.tree).unwrap(),
            super::super::change::name(&repo).unwrap()
        );
        // No branch, no remote, and the fix isn't there at all.
        let out = Command::new("git")
            .arg("-C")
            .arg(&s.tree)
            .args(["for-each-ref"])
            .output()
            .unwrap();
        assert!(
            out.stdout.is_empty(),
            "{}",
            String::from_utf8_lossy(&out.stdout)
        );
        let out = Command::new("git")
            .arg("-C")
            .arg(&s.tree)
            .args(["cat-file", "-e", &fixed])
            .output()
            .unwrap();
        assert!(!out.status.success(), "the later commit is in the copy");
        let tree = s.tree.clone();
        drop(s);
        assert!(!tree.exists(), "the copy goes when the trial ends");
    }

    #[test]
    fn new_work_needs_no_findings_and_is_saved_unscored() {
        let key: Key = serde_yaml_ng::from_str(
            "repo: /r\nbase: \"b\"\nhead: \"h\"\ncontext: c\ndimensions: [correctness]\n",
        )
        .unwrap();
        assert!(key.findings.is_empty());
        let outcome = Outcome {
            key: "k".into(),
            pr: None,
            arm: Arm::Sized,
            seconds: 1,
            tokens: Counts::default(),
            models: Vec::new(),
            score: None,
            findings: Vec::new(),
            failures: Vec::new(),
        };
        let line = serde_json::to_string(&outcome).unwrap();
        let value: Value = serde_json::from_str(&line).unwrap();
        assert!(value["score"].is_null());
        let back: Outcome = serde_json::from_str(&line).unwrap();
        assert!(back.score.is_none());
    }

    #[test]
    fn finds_the_last_json_object_in_an_answer() {
        let text = "Here's my answer:\n```json\n{\"a\": 1}\n```\nOn second thought:\n\
                    ```json\n{\"key\": [{\"id\": \"x\"}], \"extra\": []}\n```\n";
        let v = last_json(text).unwrap();
        assert_eq!(v["key"][0]["id"], "x");
        assert!(last_json("no json here").is_none());
    }

    #[test]
    fn a_key_takes_commits_quoted_or_not() {
        let text = [
            "repo: /r",
            "base: 3055453",
            "head: \"5ad8c13\"",
            "context: c",
            "dimensions: [correctness]",
            "findings:",
            "  - id: a",
            "    blocker: true",
            "    dimension: correctness",
            "    what: w",
            "    found_later: true",
        ]
        .join("\n");
        let text = text.as_str();
        let key: Key = serde_yaml_ng::from_str(text).unwrap();
        assert_eq!(
            (key.base.as_str(), key.head.as_str()),
            ("3055453", "5ad8c13")
        );
        assert!(key.findings[0].found_later);
    }

    #[test]
    fn a_one_size_trial_tells_its_builder_so() {
        let key = Key {
            repo: "/r".into(),
            pr: Some(3),
            base: "b".into(),
            head: "h".into(),
            context: "Sends a welcome email once.".into(),
            dimensions: vec!["correctness".into(), "regression-safety".into()],
            findings: Vec::new(),
        };
        let criteria = [(
            "correctness".to_string(),
            "Comments claim only what the code does.".to_string(),
        )];
        let one = builder_prompt(&key, Arm::OneSize, Path::new("/s/result.json"), &criteria);
        assert!(one.contains("opus at xhigh"));
        assert!(one.contains(
            "- correctness: Comments claim only what the code does.\n- regression-safety"
        ));
        let sized = builder_prompt(&key, Arm::Sized, Path::new("/s/result.json"), &[]);
        assert!(sized.contains("Size each reviewer") && !sized.contains("xhigh"));
        assert!(sized.contains("b..HEAD") && sized.contains("/s/result.json"));
        assert!(sized.contains("Sends a welcome email once."));
    }
}
