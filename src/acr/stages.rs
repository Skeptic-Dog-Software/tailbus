//! Stages: what a careful reviewer does by habit, made part of every round,
//! so a review doesn't depend on a reviewer choosing to.
//!
//! - **Context**: a pack of what the repository's history says about the
//!   changed files, gathered from git, ahead of the diff for every role.
//! - **Claims**: every claim the change makes about itself, listed, for a
//!   reviewer to check one by one.
//! - **Mutation**: a reviewer proposes small breaks in the changed code;
//!   the pipeline applies each in a copy of its own and runs the tests. A
//!   break the tests don't catch is a finding, with its proof.
//! - **Probes**: a reviewer writes and runs scenarios of what the change
//!   says it does, and the edges around it, and reports the ones that fail.
//!
//! Claims, mutation and probes are lenses the framework adds, so a Judge
//! rates what they find, and a review to convergence attacks with them
//! every round.

use std::fs;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use regex::Regex;
use serde::Deserialize;

use super::change::{Change, copy_of, git};
use super::config::Stage;
use super::roles::Raw;

/// The lens a stage runs as, if it runs as one.
pub fn lens_id(stage: Stage) -> Option<&'static str> {
    match stage {
        Stage::Brief | Stage::Context => None,
        Stage::Claims => Some("claims"),
        Stage::Mutation => Some("mutation"),
        Stage::Probes => Some("probes"),
    }
}

/// The stage a lens is, among those turned on.
pub fn of_lens(id: &str, on: &[Stage]) -> Option<Stage> {
    on.iter().copied().find(|s| lens_id(*s) == Some(id))
}

/// What wrong means for a stage's lens, which its Judge rates by.
pub fn criteria(stage: Stage) -> &'static str {
    match stage {
        Stage::Brief | Stage::Context => "",
        Stage::Claims => {
            "A claim the change makes about itself is false, or the code and tests don't bear \
             it out: a comment, a test's name or header, a doc or ledger line, a commit message \
             or the description. A false claim blocks when someone relying on it would ship a \
             defect, skip a test, or misread what the code does."
        }
        Stage::Mutation => {
            "A small break in the code the change adds or changes passes every test that should \
             catch it. It blocks when the change, its tests or its docs claim that behaviour is \
             covered, or when the break would ship the defect the change exists to prevent."
        }
        Stage::Probes => {
            "Run as a scenario, the change doesn't do what it says, or breaks at an edge: done \
             twice, in another order, after a reopen, at a boundary, with something missing. A \
             failure that a user would meet blocks."
        }
    }
}

// ------------------------------------------------------------------ brief

/// A lens, as the brief stage is told of it.
pub struct ToBrief<'a> {
    pub id: &'a str,
    pub criteria: &'a str,
    pub attack: Option<&'a str>,
}

/// The brief stage's prompt: one call writes every open lens a brief for
/// this change.
pub fn briefer_prompt(scope: &str, lenses: &[ToBrief], runbook: Option<&str>) -> String {
    let list: String = lenses
        .iter()
        .map(|l| {
            let attack = l
                .attack
                .map(|a| format!("\n  How it's attacked: {}", a.trim()))
                .unwrap_or_default();
            format!("- {}: {}{attack}\n", l.id, l.criteria.trim())
        })
        .collect();
    let runbook = runbook
        .map(|r| format!("# How to work in this repository\n\n{}\n\n", r.trim()))
        .unwrap_or_default();
    format!(
        "You brief the reviewers of a change, one per lens, before they attack it. You don't \
         review it yourself, and you have no case to make for it: you didn't write it. Scope: \
         {scope}. The change is the attached file: what it says it's for, then its diff. Your \
         working directory is the repository as the change leaves it: read the code the change \
         touches, what calls it and what it calls, and its tests, enough to know where it could \
         be wrong. Change nothing.\n\n\
         For each lens below, write its reviewer a brief for this change. Name the parts of \
         the change to attack in that lens: functions, branches, data, tests, docs. For each, \
         say what to settle and how to settle it here: what to run, what to break and see \
         caught, which scenarios and edges to try, which callers and old tests to check. Ask \
         questions to settle, not conclusions: the reviewer decides what's wrong. Be specific \
         to this change, since a reviewer already has its lens's criteria and techniques. A \
         few sharp lines beat a long list.\n\n\
         # The lenses\n\n{list}\n\
         {runbook}\
         Answer with only JSON: {{\"briefs\": {{\"<lens id>\": \"<the brief>\"}}}}, with a brief \
         for every lens above."
    )
}

#[derive(Deserialize)]
struct Briefs {
    briefs: std::collections::BTreeMap<String, String>,
}

pub fn read_briefs(text: &str) -> Result<std::collections::BTreeMap<String, String>> {
    Ok(super::roles::json_in::<Briefs>(text)?.briefs)
}

/// What a reviewer gets besides its lens's criteria: how to attack in the
/// lens, where to attack in this change, from the brief stage, and, if it
/// can run things, how to work in this repository.
pub fn guide(attack: Option<&str>, brief: Option<&str>, runbook: Option<&str>) -> String {
    let mut out = String::new();
    if let Some(a) = attack.filter(|a| !a.trim().is_empty()) {
        out.push_str(&format!("# How to attack in this lens\n\n{}\n\n", a.trim()));
    }
    if let Some(b) = brief.filter(|b| !b.trim().is_empty()) {
        out.push_str(&format!(
            "# Where to attack in this change\n\nFrom a brief written for this change by \
             someone who read it but didn't write it: leads, not findings.\n\n{}\n\n",
            b.trim()
        ));
    }
    if let Some(r) = runbook.filter(|r| !r.trim().is_empty()) {
        out.push_str(&format!(
            "# How to work in this repository\n\n{}\n\n",
            r.trim()
        ));
    }
    out
}

// ---------------------------------------------------------------- context

/// The most a context pack holds.
const PACK_MAX: usize = 16 * 1024;
/// The most changed files a pack covers.
const PACK_FILES: usize = 15;

/// What the repository's history says about the changed files: earlier
/// commits to them, docs and tests that mention them, and where the
/// functions the change touches are used. Gathered from git at the base and
/// in the checkout, so nothing in it comes from after the change.
pub fn context_pack(repo: &Path, base: &str, change: &Change) -> String {
    let left_out: Vec<&str> = change.generated.iter().map(|g| g.path.as_str()).collect();
    let files: Vec<&String> = change
        .files
        .iter()
        .filter(|f| !left_out.contains(&f.as_str()))
        .take(PACK_FILES)
        .collect();
    let mut out = String::from(
        "# What the repository's history says about the changed files\n\n\
         Gathered from git, not written by anyone: leads to follow, not findings.\n\n",
    );

    let mut earlier = String::new();
    for f in &files {
        let log = git(
            repo,
            &[
                "log",
                "-n",
                "5",
                "--format=%h %ad %s",
                "--date=short",
                base,
                "--",
                f,
            ],
        )
        .unwrap_or_default();
        if !log.trim().is_empty() {
            earlier.push_str(&format!("{f}:\n"));
            for line in log.trim().lines() {
                earlier.push_str(&format!("  {line}\n"));
            }
        }
    }
    section(
        &mut out,
        "Earlier commits to them, before this change",
        &earlier,
    );

    let mut docs = String::new();
    for f in &files {
        let name = file_name(f);
        let hits = git(
            repo,
            &[
                "grep", "-n", "-I", "-F", "-e", name, "--", "*.md", "*.txt", "*.rst",
            ],
        )
        .unwrap_or_default();
        for line in hits.lines().filter(|l| !l.starts_with(f.as_str())).take(8) {
            docs.push_str(&format!("- {}\n", clip(line, 240)));
        }
    }
    section(&mut out, "Docs that mention them", &docs);

    let mut tests = String::new();
    for f in &files {
        let stem = stem(f);
        if stem.len() < 4 {
            continue;
        }
        let hits = git(repo, &["grep", "-l", "-I", "-F", "-e", stem]).unwrap_or_default();
        let named: Vec<&str> = hits
            .lines()
            .filter(|p| p != f && is_test_path(p))
            .take(8)
            .collect();
        if !named.is_empty() {
            tests.push_str(&format!("{f}: {}\n", named.join(", ")));
        }
    }
    section(&mut out, "Tests that mention them", &tests);

    let mut used = String::new();
    for name in changed_functions(&change.diff) {
        let hits = git(repo, &["grep", "-n", "-I", "-w", "-F", "-e", &name]).unwrap_or_default();
        let lines: Vec<&str> = hits.lines().take(6).collect();
        if lines.len() > 1 {
            used.push_str(&format!("{name}:\n"));
            for line in lines {
                used.push_str(&format!("  {}\n", clip(line, 200)));
            }
        }
    }
    section(
        &mut out,
        "Where the functions the change touches are used",
        &used,
    );

    if out.len() > PACK_MAX {
        let mut end = PACK_MAX;
        while !out.is_char_boundary(end) {
            end -= 1;
        }
        out.truncate(end);
        out.push_str("\n…(cut here: the pack has a size limit)\n");
    }
    out.push('\n');
    out
}

fn section(out: &mut String, title: &str, body: &str) {
    if !body.trim().is_empty() {
        out.push_str(&format!("## {title}\n\n{body}\n"));
    }
}

fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn stem(path: &str) -> &str {
    let name = file_name(path);
    name.split('.').next().unwrap_or(name)
}

fn is_test_path(path: &str) -> bool {
    let p = path.to_ascii_lowercase();
    p.contains("test") || p.contains("spec")
}

fn clip(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

/// The functions the diff's hunks are in, by their headers.
fn changed_functions(diff: &str) -> Vec<String> {
    const COMMON: [&str; 14] = [
        "if", "for", "while", "switch", "return", "build", "main", "test", "func", "function",
        "new", "init", "setUp", "group",
    ];
    let re = Regex::new(r"([A-Za-z_][A-Za-z0-9_]*)\s*\(").expect("a fixed pattern");
    let mut names: Vec<String> = Vec::new();
    for line in diff.lines().filter(|l| l.starts_with("@@")) {
        let Some(context) = line.splitn(3, "@@").nth(2) else {
            continue;
        };
        if let Some(name) = re
            .captures_iter(context)
            .map(|c| c[1].to_string())
            .find(|n| n.len() >= 4 && !COMMON.contains(&n.as_str()))
            && !names.contains(&name)
        {
            names.push(name);
        }
        if names.len() >= 10 {
            break;
        }
    }
    names
}

// ----------------------------------------------------------------- claims

/// A claim the change makes about itself, and where it makes it.
#[derive(Debug, Clone, PartialEq)]
pub struct Claim {
    pub at: String,
    pub text: String,
}

/// The most claims a round lists.
const CLAIMS_MAX: usize = 80;

/// Every claim the change makes about itself: the doc lines it adds, the
/// comments it adds, its tests' names, its commit messages and its
/// description.
pub fn claims(change: &Change, description: Option<&str>) -> Vec<Claim> {
    let test_name = Regex::new(
        r#"\b(?:test|testWidgets|it|describe|group|context)\s*\(\s*['"]([^'"]{6,})['"]"#,
    )
    .expect("a fixed pattern");
    let test_fn = Regex::new(r"\b(?:fn|func|def)\s+([Tt]est\w*)").expect("a fixed pattern");
    let mut found: Vec<Claim> = Vec::new();
    for (file, line, text) in added_lines(&change.diff) {
        let at = format!("{file}:{line}");
        let trimmed = text.trim();
        if is_doc(&file) {
            if words(trimmed) >= 4 {
                found.push(Claim {
                    at,
                    text: clip(trimmed, 400),
                });
            }
            continue;
        }
        if let Some(c) = test_name.captures(trimmed) {
            found.push(Claim {
                at,
                text: format!("a test named \"{}\"", &c[1]),
            });
            continue;
        }
        if let Some(c) = test_fn.captures(trimmed) {
            found.push(Claim {
                at,
                text: format!("a test named {}", &c[1]),
            });
            continue;
        }
        if let Some(comment) = comment_of(trimmed) {
            // A comment's lines run on into one claim.
            if let Some(last) = found.last_mut()
                && last.at.starts_with(&format!("{file}:"))
                && last.text.starts_with("comment: ")
                && last
                    .at
                    .rsplit(':')
                    .next()
                    .and_then(|n| n.parse::<u64>().ok())
                    == Some(line.saturating_sub(last.text.matches('\n').count() as u64 + 1))
            {
                last.text.push('\n');
                last.text.push_str(comment);
                continue;
            }
            found.push(Claim {
                at,
                text: format!("comment: {comment}"),
            });
        }
    }
    // Comments too short to claim anything are dropped once joined.
    found.retain(|c| !c.text.starts_with("comment: ") || words(&c.text) >= 5);
    found.truncate(CLAIMS_MAX);
    for commit in &change.commits {
        found.push(Claim {
            at: format!("commit {}", commit.id),
            text: clip(commit.message.trim(), 1500),
        });
    }
    if let Some(d) = description.filter(|d| !d.trim().is_empty()) {
        found.push(Claim {
            at: "the description".into(),
            text: clip(d.trim(), 3000),
        });
    }
    found
}

fn is_doc(file: &str) -> bool {
    let f = file.to_ascii_lowercase();
    [".md", ".txt", ".rst", ".adoc"]
        .iter()
        .any(|x| f.ends_with(x))
}

fn words(text: &str) -> usize {
    text.split_whitespace().count()
}

/// A comment line's text, if the line is one.
fn comment_of(line: &str) -> Option<&str> {
    if line.starts_with("#[") || line.starts_with("#!") || line.starts_with("#include") {
        return None;
    }
    for marker in ["///", "//", "/**", "/*", "<!--", "--", "#", "* ", "*"] {
        if let Some(rest) = line.strip_prefix(marker) {
            let rest = rest
                .trim()
                .trim_end_matches("*/")
                .trim_end_matches("-->")
                .trim();
            return (!rest.is_empty()).then_some(rest);
        }
    }
    None
}

/// The lines a diff adds: each with its file and its line number there.
fn added_lines(diff: &str) -> Vec<(String, u64, String)> {
    let mut out = Vec::new();
    let mut file = String::new();
    let mut line = 0u64;
    let hunk = Regex::new(r"^@@ -\d+(?:,\d+)? \+(\d+)").expect("a fixed pattern");
    for text in diff.lines() {
        if let Some(path) = text.strip_prefix("+++ ") {
            file = path.strip_prefix("b/").unwrap_or(path).to_string();
            continue;
        }
        if text.starts_with("--- ") || text.starts_with("diff --git") {
            continue;
        }
        if let Some(c) = hunk.captures(text) {
            line = c[1].parse().unwrap_or(1);
            continue;
        }
        if let Some(added) = text.strip_prefix('+') {
            if !file.is_empty() && file != "/dev/null" {
                out.push((file.clone(), line, added.to_string()));
            }
            line += 1;
        } else if text.starts_with(' ') {
            line += 1;
        }
    }
    out
}

/// A claims reviewer's brief.
pub fn claims_prompt(
    claims: &[Claim],
    scope: &str,
    not_findings: &str,
    own_copy: bool,
    guide: &str,
) -> String {
    let list: String = claims
        .iter()
        .enumerate()
        .map(|(i, c)| format!("{}. [{}] {}\n", i + 1, c.at, c.text))
        .collect();
    let how = if own_copy {
        "Read to check each, and where reading leaves a doubt, run something in your working \
         directory, your own copy of the repository: nothing you do there reaches the change. \
         Nobody is here to grant a permission, so a shell command that would need one is \
         refused: name paths in full, not through variables or `$(...)`, and put a check of \
         more than one step in a script, written with the Write tool and run by its path."
    } else {
        "Read the repository, as the change leaves it, to check each, but change nothing."
    };
    format!(
        "You check what a change claims about itself. Scope: {scope}. The change is the \
         attached file: what it says it's for, then its diff.\n\n\
         Each item below is a claim the change makes: a doc or ledger line it adds, a comment \
         it adds, a test's name, a commit message, or its description. Check each against the \
         code and the tests. {how}\n\n\
         # The claims\n\n{list}\n\
         {guide}\
         Report each claim that's false, or that the code and tests don't bear out: what it \
         says, where, and the evidence. A true claim isn't a finding.\n\n\
         Not findings, which are already decided, so leave them out:\n{decided}\n\n\
         Answer with only JSON: {{\"findings\": [{{\"title\": \"<one line>\", \"detail\": \
         \"<the claim, and the evidence>\", \"file\": \"<path>\", \"line\": <number>}}]}}. If \
         every claim holds, say so with an empty list.",
        decided = decided(not_findings),
    )
}

fn decided(not_findings: &str) -> String {
    match not_findings.trim() {
        "" => "None.".to_string(),
        text => text.to_string(),
    }
}

// ----------------------------------------------------------------- probes

/// A prober's brief: it writes scenarios as tests and runs them, in its own
/// copy.
pub fn probes_prompt(
    scope: &str,
    not_findings: &str,
    command: Option<&str>,
    guide: &str,
) -> String {
    let command = match command {
        Some(c) => format!(
            "This repository's tests run with `{}`, from its root.",
            c.replace("{tests}", "<test files>")
        ),
        None => "Find how this repository runs its tests from its own files.".to_string(),
    };
    format!(
        "You probe a change by running it. Scope: {scope}. The change is the attached file: \
         what it says it's for, then its diff. Your working directory is your own copy of the \
         repository, as the change leaves it, and nothing you do there reaches the change.\n\n\
         Write small scenarios, as tests in this repository's own test framework, of what the \
         change says it does and the edges around it: doing it twice, or in another order; \
         after a reopen or a restart; at a boundary, such as midnight, a month's end, the first \
         or last item, or an empty or missing value; and after something it depends on was \
         changed or deleted. Run each. {command}\n\n\
         {guide}\
         A scenario that fails, or shows wrong behaviour, is a finding: give its test code and \
         what it printed. One that passes isn't. Wrong behaviour that was there before the \
         change, in what it touches, is a finding too: say it's pre-existing. Nobody is here to grant a permission, so a \
         shell command that would need one is refused: name paths in full, not through \
         variables or `$(...)`, and put anything of more than one step in a script, written \
         with the Write tool and run by its path.\n\n\
         Not findings, which are already decided, so leave them out:\n{decided}\n\n\
         Answer with only JSON: {{\"findings\": [{{\"title\": \"<one line>\", \"detail\": \
         \"<the scenario, its code, and what it printed>\", \"file\": \"<path>\", \"line\": \
         <number>}}]}}. If every scenario passes, say so with an empty list.",
        decided = decided(not_findings),
    )
}

// --------------------------------------------------------------- mutation

/// A small break in the changed code, which a test should catch.
#[derive(Debug, Clone, Deserialize)]
pub struct Mutant {
    pub file: String,
    /// Text that occurs once in the file, as the change leaves it.
    pub find: String,
    pub replace: String,
    /// What a test should catch.
    #[serde(default)]
    pub why: String,
    /// The test files that should catch it.
    #[serde(default)]
    pub tests: Vec<String>,
}

#[derive(Deserialize)]
struct Proposed {
    mutants: Vec<Mutant>,
}

pub fn read_mutants(text: &str) -> Result<Vec<Mutant>> {
    Ok(super::roles::json_in::<Proposed>(text)?.mutants)
}

/// A mutation reviewer's brief: propose the breaks; the pipeline runs them.
pub fn mutants_prompt(scope: &str, count: u32, guide: &str) -> String {
    format!(
        "You find where a change's tests are weak, by proposing small breaks in its code that a \
         test should catch. Scope: {scope}. The change is the attached file: what it says it's \
         for, then its diff. Your working directory is the repository, as the change leaves \
         it: read what you need, and change nothing. The pipeline applies each break you \
         propose in a copy of its own and runs the tests you name, so don't run anything.\n\n\
         Propose up to {count}, the most telling first, in the code the change adds or \
         changes: a lookup by the wrong id or key; a condition flipped or dropped; a call or a \
         write left out; a boundary moved by one; a value not cleared or not reset; an error \
         swallowed; an order swapped. Each must still compile. Name the test files that should \
         catch each: the change's own tests, and the existing tests of the code it touches.\n\n\
         For each: the file; `find`, text copied exactly from that file, long enough to occur \
         only once in it; `replace`, what it becomes; and `why`, the behaviour a test should \
         catch.\n\n\
         {guide}\
         Answer with only JSON: {{\"mutants\": [{{\"file\": \"<path>\", \"find\": \"\", \
         \"replace\": \"\", \"why\": \"\", \"tests\": [\"<test file>\"]}}]}}."
    )
}

/// The command that runs a repository's tests, from its root, found from its
/// files: `{tests}` in it stands for the test files to run.
pub fn test_command(repo: &Path) -> Option<String> {
    let has = |f: &str| repo.join(f).is_file();
    if has("pubspec.yaml") {
        Some("flutter test {tests}".into())
    } else if has("Cargo.toml") {
        Some("cargo test --quiet".into())
    } else if has("go.mod") {
        Some("go test ./...".into())
    } else if has("package.json") {
        Some("npm test -- {tests}".into())
    } else if has("pyproject.toml") || has("pytest.ini") || has("setup.cfg") {
        Some("python3 -m pytest -q {tests}".into())
    } else {
        None
    }
}

/// How mutants are run.
pub struct Runner<'a> {
    /// The repository, as the change leaves it.
    pub repo: &'a Path,
    /// A folder of the run's own, for the copies.
    pub dir: &'a Path,
    pub command: &'a str,
    pub parallel: usize,
    pub limit: Duration,
}

/// What became of a mutant.
#[derive(Debug, Clone, PartialEq)]
pub enum Fate {
    /// A test failed: it's caught.
    Killed,
    /// Every test passed: the tests miss it.
    Survived(String),
    /// It couldn't be applied, as its text isn't in the file just once.
    Unusable,
}

/// Runs each mutant in a copy of its own, from one copy where the tests
/// passed first, and returns the survivors as findings, with a summary.
pub fn run_mutants(r: &Runner, mutants: &[Mutant]) -> Result<(Vec<Raw>, String)> {
    if mutants.is_empty() {
        return Ok((Vec::new(), "no mutants were proposed".into()));
    }
    let name = r
        .repo
        .file_name()
        .map_or_else(|| "repo".into(), |n| n.to_owned());
    let base = r.dir.join("base").join(&name);
    copy_of(r.repo, &base).context("copying the change to run its tests")?;
    let mut named: Vec<String> = Vec::new();
    for m in mutants {
        for t in &m.tests {
            if base.join(t).is_file() && !named.contains(t) {
                named.push(t.clone());
            }
        }
    }
    let (ok, out) = run_tests(&base, r.command, &named, r.limit, &r.dir.join("base.log"))?;
    if !ok {
        bail!(
            "the tests fail before any break, so no mutant can be judged ({}): {}",
            command_line(r.command, &named),
            clip(&out, 1500)
        );
    }
    let next = AtomicUsize::new(0);
    let fates: Mutex<Vec<(usize, Fate)>> = Mutex::new(Vec::new());
    thread::scope(|scope| {
        for _ in 0..r.parallel.max(1) {
            scope.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::SeqCst);
                    let Some(m) = mutants.get(i) else {
                        break;
                    };
                    let tests: Vec<String> = m
                        .tests
                        .iter()
                        .filter(|t| base.join(t).is_file())
                        .cloned()
                        .collect();
                    let tests = if tests.is_empty() {
                        named.clone()
                    } else {
                        tests
                    };
                    let fate = one_mutant(r, &base, &name, i, m, &tests).unwrap_or(Fate::Unusable);
                    fates.lock().expect("not poisoned").push((i, fate));
                }
            });
        }
    });
    let mut fates = fates.into_inner().expect("not poisoned");
    fates.sort_by_key(|(i, _)| *i);
    let _ = fs::remove_dir_all(r.dir.join("base"));
    let mut found = Vec::new();
    let (mut killed, mut survived, mut unusable) = (0, 0, 0);
    for (i, fate) in fates {
        let m = &mutants[i];
        match fate {
            Fate::Killed => killed += 1,
            Fate::Unusable => unusable += 1,
            Fate::Survived(out) => {
                survived += 1;
                let tests = if m.tests.is_empty() {
                    "the tests named for the others".to_string()
                } else {
                    m.tests.join(", ")
                };
                found.push(Raw {
                    title: format!("The tests pass with this break: {}", clip(&m.why, 160)),
                    detail: format!(
                        "In {file}, replacing\n```\n{find}\n```\nwith\n```\n{replace}\n```\n\
                         leaves {tests} passing. What a test should catch: {why}\n\
                         The end of the run:\n```\n{out}\n```",
                        file = m.file,
                        find = clip(&m.find, 1200),
                        replace = clip(&m.replace, 1200),
                        why = m.why,
                        out = clip(&out, 1200),
                    ),
                    file: Some(m.file.clone()),
                    line: None,
                });
            }
        }
    }
    let summary = format!(
        "{} mutants: {killed} caught, {survived} missed, {unusable} unusable",
        mutants.len()
    );
    Ok((found, summary))
}

fn one_mutant(
    r: &Runner,
    base: &Path,
    name: &std::ffi::OsStr,
    i: usize,
    m: &Mutant,
    tests: &[String],
) -> Result<Fate> {
    let dir = r.dir.join(format!("m{i}"));
    let copy = dir.join(name);
    let fate = (|| -> Result<Fate> {
        copy_of(base, &copy)?;
        let path = copy.join(&m.file);
        let Ok(text) = fs::read_to_string(&path) else {
            return Ok(Fate::Unusable);
        };
        if m.find.is_empty() || m.find == m.replace || text.matches(&m.find).count() != 1 {
            return Ok(Fate::Unusable);
        }
        fs::write(&path, text.replacen(&m.find, &m.replace, 1))?;
        let (ok, out) = run_tests(&copy, r.command, tests, r.limit, &dir.join("run.log"))?;
        Ok(if ok {
            Fate::Survived(out)
        } else {
            Fate::Killed
        })
    })();
    let _ = fs::remove_dir_all(&dir);
    fate
}

fn command_line(command: &str, tests: &[String]) -> String {
    let quoted: Vec<String> = tests
        .iter()
        .map(|t| format!("'{}'", t.replace('\'', "'\\''")))
        .collect();
    command.replace("{tests}", &quoted.join(" "))
}

/// Runs the tests in `dir`. Returns whether they passed, and the end of
/// what they printed. One that runs past `limit` is stopped, and fails.
fn run_tests(
    dir: &Path,
    command: &str,
    tests: &[String],
    limit: Duration,
    log: &Path,
) -> Result<(bool, String)> {
    if let Some(parent) = log.parent() {
        fs::create_dir_all(parent)?;
    }
    let file = fs::File::create(log)?;
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(command_line(command, tests))
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(file.try_clone()?)
        .stderr(file)
        .process_group(0)
        .spawn()
        .context("running the tests")?;
    let until = Instant::now() + limit;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break Some(status);
        }
        if Instant::now() >= until {
            // SAFETY: signals the process group it leads, which this started.
            unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
            let _ = child.wait();
            break None;
        }
        thread::sleep(Duration::from_millis(500));
    };
    let text = fs::read_to_string(log).unwrap_or_default();
    let mut start = text.len().saturating_sub(3000);
    while !text.is_char_boundary(start) {
        start += 1;
    }
    let tail = text[start..].to_string();
    Ok((status.is_some_and(|s| s.success()), tail))
}

/// Where a lens's mutants run, for one reviewer in one round.
pub fn mutation_dir(copies: &Path, lens: &str, n: u32) -> PathBuf {
    copies.join(format!("{lens}-{n}-runs"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acr::change::tests::{commit, repo_with};

    const DIFF_LINES: [&str; 20] = [
        "diff --git a/lib/a.dart b/lib/a.dart",
        "--- a/lib/a.dart",
        "+++ b/lib/a.dart",
        "@@ -10,3 +10,8 @@ class Editor {",
        "   int x = 1;",
        "+  // Saves the edit, then reschedules every notice for the",
        "+  // reminder, so nothing old fires.",
        "+  void save() {}",
        "-  int y = 2;",
        "+  #[test]",
        "diff --git a/docs/LEDGER.md b/docs/LEDGER.md",
        "--- a/docs/LEDGER.md",
        "+++ b/docs/LEDGER.md",
        "@@ -1,1 +1,2 @@",
        " | id | what |",
        "+| B-1 | fixed: an edit reschedules its notices |",
        "--- /dev/null",
        "+++ b/test/a_test.dart",
        "@@ -0,0 +1,2 @@",
        "+  testWidgets('an edit is checked whole', (t) async {});\n+  // ok",
    ];

    fn diff() -> String {
        DIFF_LINES.join("\n") + "\n"
    }

    #[test]
    fn a_changes_claims_are_its_docs_comments_test_names_commits_and_description() {
        let change = Change {
            diff: diff(),
            commits: vec![crate::acr::change::Commit {
                id: "abc123".into(),
                message: "Fix B-1: an edit reschedules".into(),
            }],
            ..Change::default()
        };
        let found = claims(&change, Some("Edits save."));
        let texts: Vec<(&str, &str)> = found
            .iter()
            .map(|c| (c.at.as_str(), c.text.as_str()))
            .collect();
        assert_eq!(
            texts,
            [
                (
                    "lib/a.dart:11",
                    "comment: Saves the edit, then reschedules every notice for the\nreminder, so nothing old fires."
                ),
                (
                    "docs/LEDGER.md:2",
                    "| B-1 | fixed: an edit reschedules its notices |"
                ),
                (
                    "test/a_test.dart:1",
                    "a test named \"an edit is checked whole\""
                ),
                ("commit abc123", "Fix B-1: an edit reschedules"),
                ("the description", "Edits save."),
            ]
        );
        assert_eq!(changed_functions(&diff()), Vec::<String>::new());
        let with_fn = "@@ -1 +1 @@ Future<void> showEditor(BuildContext c) {\n";
        assert_eq!(changed_functions(with_fn), ["showEditor"]);
    }

    #[test]
    fn a_context_pack_holds_earlier_commits_docs_tests_and_uses() {
        let repo = repo_with(&[
            ("lib/editor.dart", "void saveEdit() {}\n"),
            ("docs/notes.md", "editor.dart saves edits\n"),
            (
                "test/editor_test.dart",
                "import 'editor.dart'; saveEdit();\n",
            ),
        ]);
        let base = git(&repo, &["rev-parse", "HEAD"]).unwrap();
        commit(
            &repo,
            &[("lib/editor.dart", "void saveEdit() { x(); }\n")],
            "change",
        );
        let change = Change {
            files: vec!["lib/editor.dart".into()],
            diff: "@@ -1 +1 @@ void saveEdit() {\n+void saveEdit() { x(); }\n".into(),
            ..Change::default()
        };
        let pack = context_pack(&repo, base.trim(), &change);
        assert!(pack.contains("Earlier commits to them") && pack.contains("lib/editor.dart:"));
        assert!(
            pack.contains("docs/notes.md:1:editor.dart saves edits"),
            "{pack}"
        );
        assert!(
            pack.contains("lib/editor.dart: test/editor_test.dart"),
            "{pack}"
        );
        assert!(pack.contains("saveEdit:"), "{pack}");
    }

    #[test]
    fn a_mutant_the_tests_miss_is_a_finding_and_one_they_catch_isnt() {
        let repo = repo_with(&[
            (
                "lib.sh",
                "add() { echo $(( $1 + $2 )); }\nsub() { echo $(( $1 - $2 )); }\n",
            ),
            ("t.sh", ". ./lib.sh\n[ \"$(add 1 2)\" = 3 ] || exit 1\n"),
        ]);
        let dir = std::env::temp_dir().join(format!("tb-acr-mut-{}", crate::util::new_id()));
        let mutants = vec![
            Mutant {
                file: "lib.sh".into(),
                find: "$1 + $2".into(),
                replace: "$1 - $2".into(),
                why: "add adds".into(),
                tests: vec!["t.sh".into()],
            },
            Mutant {
                file: "lib.sh".into(),
                find: "$1 - $2".into(),
                replace: "$1 + $2".into(),
                why: "sub subtracts".into(),
                tests: vec!["t.sh".into()],
            },
            Mutant {
                file: "lib.sh".into(),
                find: "not there".into(),
                replace: "x".into(),
                why: "unusable".into(),
                tests: Vec::new(),
            },
        ];
        let runner = Runner {
            repo: &repo,
            dir: &dir,
            command: "sh {tests}",
            parallel: 2,
            limit: Duration::from_secs(30),
        };
        let (found, summary) = run_mutants(&runner, &mutants).unwrap();
        assert_eq!(summary, "3 mutants: 1 caught, 1 missed, 1 unusable");
        assert_eq!(found.len(), 1);
        assert!(found[0].title.contains("sub subtracts"));
        assert_eq!(found[0].file.as_deref(), Some("lib.sh"));
        // Every copy goes.
        assert!(fs::read_dir(&dir).map_or(true, |d| {
            d.flatten().all(|e| e.path().extension().is_some())
        }));
        // Tests that fail before any break judge nothing.
        let failing = Runner {
            command: "false",
            ..runner
        };
        assert!(run_mutants(&failing, &mutants).is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_repositorys_test_command_is_found_from_its_files() {
        let repo = repo_with(&[("pubspec.yaml", "name: x\n")]);
        assert_eq!(test_command(&repo).as_deref(), Some("flutter test {tests}"));
        let none = repo_with(&[("README", "x\n")]);
        assert_eq!(test_command(&none), None);
        assert_eq!(
            command_line("flutter test {tests}", &["test/a b.dart".into()]),
            "flutter test 'test/a b.dart'"
        );
    }
}
