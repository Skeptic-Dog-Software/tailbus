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
        Stage::Brief | Stage::Context | Stage::TestWriting => None,
        Stage::Claims => Some("claims"),
        Stage::Mutation => Some("mutation"),
        Stage::Gate => Some("tests"),
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
        Stage::Brief | Stage::Context | Stage::TestWriting => "",
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
        Stage::Gate => {
            "The change's tests aren't enough: a line it adds or changes that no test runs, a \
             small break in its code that every test passes, or one of its tests that fails \
             when run another way, in another order, alone, or in another time zone. It blocks \
             when a test of the change fails another way, or when the untested \
             code is what the change exists to do or fix, or when the change, its tests or \
             its docs claim that behaviour is covered. A break that changes nothing a user or \
             caller could see isn't a finding."
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
    let attached = super::change::ATTACHED;
    format!(
        "You brief the reviewers of a change, one per lens, before they attack it. You don't \
         review it yourself, and you have no case to make for it: you didn't write it. Scope: \
         {scope}. {attached} Your \
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

/// The building session's brief for the reviewers: what the change does,
/// mechanism by mechanism, and for each lens where to attack it and how.
/// It's the builder's account, so every role gets its scope statement as
/// claims to check, and each reviewer only its own lens's part.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct BuilderBrief {
    #[serde(default)]
    pub scope: Option<String>,
    #[serde(default)]
    pub lenses: std::collections::BTreeMap<String, String>,
}

pub fn read_builder_brief(path: &Path) -> Result<BuilderBrief> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("reading the brief {}", path.display()))?;
    serde_yaml_ng::from_str(&text).with_context(|| format!("in the brief {}", path.display()))
}

/// The builder's scope statement, for every role, among what's said about
/// the change ([`super::change::SAID`]): claims to check like the rest.
pub fn scope_section(brief: &BuilderBrief) -> String {
    match brief.scope.as_deref().filter(|s| !s.trim().is_empty()) {
        Some(scope) => format!("## What its builder says it does\n\n{}\n\n", scope.trim()),
        None => String::new(),
    }
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
            "# Where to attack in this change\n\nFrom a brief written for this change: leads, \
             not findings, and what it says about the change is a claim to check, not to take \
             on trust.\n\n{}\n\n",
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

/// The context pack, in two parts. `said` is what earlier commits and the
/// docs say about the changed files: people's accounts, so it goes among
/// what's said about the change ([`super::change::SAID`]), as claims to
/// check. `leads` is where the tests and the touched functions' callers
/// are: pointers into the code, not anyone's claims.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Pack {
    pub said: String,
    pub leads: String,
}

/// Earlier commits to the changed files, docs and tests that mention them,
/// and where the functions the change touches are used. Gathered from git at
/// the base and in the checkout, so nothing in it comes from after the
/// change.
pub fn context_pack(repo: &Path, base: &str, change: &Change) -> Pack {
    let left_out: Vec<&str> = change.generated.iter().map(|g| g.path.as_str()).collect();
    let files: Vec<&String> = change
        .files
        .iter()
        .filter(|f| !left_out.contains(&f.as_str()))
        .take(PACK_FILES)
        .collect();
    let mut out = String::new();

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
        "Earlier commits to the changed files, before this change",
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
    section(&mut out, "Docs that mention the changed files", &docs);
    let said = capped(out);

    let mut out = String::new();
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
    section(&mut out, "Tests that mention the changed files", &tests);

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
    let leads = capped(out);
    Pack {
        said,
        leads: if leads.is_empty() {
            leads
        } else {
            format!(
                "# Leads in the repository\n\nGathered from git: where to look, not findings.\n\n{leads}"
            )
        },
    }
}

/// Half the pack's limit, for each of its parts.
fn capped(mut out: String) -> String {
    let max = PACK_MAX / 2;
    if out.len() > max {
        let mut end = max;
        while !out.is_char_boundary(end) {
            end -= 1;
        }
        out.truncate(end);
        out.push_str("\n…(cut here: the pack has a size limit)\n");
    }
    if !out.is_empty() {
        out.push('\n');
    }
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

/// Text cut to `max` bytes, at a character's edge.
pub fn clip_text(text: &str, max: usize) -> String {
    clip(text, max)
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
        format!(
            "Read to check each, and where reading leaves a doubt, run something in your \
             working directory, your own copy of the repository: nothing you do there reaches \
             the change. Nobody is here to grant a permission, so a shell command that would \
             need one is refused: name paths in full, not through variables or `$(...)`, and \
             put a check of more than one step in a script, written with the Write tool and \
             run by its path. {}",
            super::roles::WAIT
        )
    } else {
        "Read the repository, as the change leaves it, to check each, but change nothing."
            .to_string()
    };
    let attached = super::change::ATTACHED;
    format!(
        "You check what a change claims about itself. Scope: {scope}. {attached}\n\n\
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
    let attached = super::change::ATTACHED;
    format!(
        "You probe a change by running it. Scope: {scope}. {attached} Your working directory is your own copy of the \
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
         with the Write tool and run by its path. {wait}\n\n\
         Not findings, which are already decided, so leave them out:\n{decided}\n\n\
         Answer with only JSON: {{\"findings\": [{{\"title\": \"<one line>\", \"detail\": \
         \"<the scenario, its code, and what it printed>\", \"file\": \"<path>\", \"line\": \
         <number>}}]}}. If every scenario passes, say so with an empty list.",
        decided = decided(not_findings),
        wait = super::roles::WAIT,
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
    let attached = super::change::ATTACHED;
    format!(
        "You find where a change's tests are weak, by proposing small breaks in its code that a \
         test should catch. Scope: {scope}. {attached} Your working directory is the repository, as the change leaves \
         it: read what you need, and change nothing. The pipeline applies each break you \
         propose in a copy of its own and runs the tests you name, so don't run anything.\n\n\
         Propose {count}, the most telling first, in the code the change adds or changes: a \
         lookup by the wrong id or key; a condition flipped or dropped; a call or a write left \
         out; a boundary moved by one; a value not cleared or not reset; an error swallowed; an \
         order swapped. Fewer only if the change has fewer places a break could go, and never \
         pad the list with a break that changes nothing. Each must still compile. Name the test files that should catch each: the change's own tests, and the \
         existing tests of the code it touches.\n\n\
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
    /// Its tests ran past the limit, as they may on a busy machine: no
    /// verdict either way.
    TimedOut,
}

/// How a run of the tests ended.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Ran {
    Passed,
    Failed,
    TimedOut,
}

/// Runs each mutant in a copy of its own, from one copy where the tests
/// passed first, and returns the survivors as findings, with a summary.
pub fn run_mutants(r: &Runner, mutants: &[Mutant]) -> Result<(Vec<Raw>, String)> {
    let (survivors, summary) = mutate(r, mutants)?;
    Ok((survivors.into_iter().map(|(_, raw)| raw).collect(), summary))
}

/// The survivors, each with its mutant's place in `mutants`.
fn mutate(r: &Runner, mutants: &[Mutant]) -> Result<(Vec<(usize, Raw)>, String)> {
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
    let (ran, out) = run_tests(&base, r.command, &named, r.limit, &r.dir.join("base.log"))?;
    match ran {
        Ran::Passed => {}
        Ran::Failed => bail!(
            "the tests fail before any break, so no mutant can be judged ({}): {}",
            command_line(r.command, &named),
            clip(&out, 1500)
        ),
        Ran::TimedOut => bail!(
            "the tests ran past {} s before any break, so no mutant can be judged ({})",
            r.limit.as_secs(),
            command_line(r.command, &named)
        ),
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
    let (mut killed, mut survived, mut unusable, mut timed_out) = (0, 0, 0, 0);
    for (i, fate) in fates {
        let m = &mutants[i];
        match fate {
            Fate::Killed => killed += 1,
            Fate::Unusable => unusable += 1,
            Fate::TimedOut => timed_out += 1,
            Fate::Survived(out) => {
                survived += 1;
                let tests = if m.tests.is_empty() {
                    "the tests named for the others".to_string()
                } else {
                    m.tests.join(", ")
                };
                found.push((
                    i,
                    Raw {
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
                    },
                ));
            }
        }
    }
    let mut summary = format!(
        "{} mutants: {killed} caught, {survived} missed, {unusable} unusable",
        mutants.len()
    );
    if timed_out > 0 {
        summary.push_str(&format!(
            ", {timed_out} timed out (past {} s, so no verdict)",
            r.limit.as_secs()
        ));
    }
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
        let (ran, out) = run_tests(&copy, r.command, tests, r.limit, &dir.join("run.log"))?;
        Ok(match ran {
            Ran::Passed => Fate::Survived(out),
            Ran::Failed => Fate::Killed,
            Ran::TimedOut => Fate::TimedOut,
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

/// Runs the tests in `dir`. Returns how they ended, and the end of what they
/// printed. A run past `limit` is stopped.
fn run_tests(
    dir: &Path,
    command: &str,
    tests: &[String],
    limit: Duration,
    log: &Path,
) -> Result<(Ran, String)> {
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
    let ran = match status {
        None => Ran::TimedOut,
        Some(s) if s.success() => Ran::Passed,
        Some(_) => Ran::Failed,
    };
    Ok((ran, tail))
}

/// Changed lines no test runs, from an lcov report, one finding for each
/// file. A changed file the report doesn't have, of a kind it covers, is a
/// file no test loads at all. Tests and docs aren't counted.
pub fn uncovered(report: &str, root: &Path, diff: &str) -> Vec<Raw> {
    use std::collections::{BTreeMap, BTreeSet};
    let root = format!("{}/", root.display());
    let mut zero: BTreeMap<String, BTreeSet<u64>> = BTreeMap::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut file = String::new();
    for line in report.lines() {
        if let Some(path) = line.strip_prefix("SF:") {
            file = path.strip_prefix(root.as_str()).unwrap_or(path).to_string();
            seen.insert(file.clone());
        } else if let Some(rest) = line.strip_prefix("DA:") {
            let mut parts = rest.split(',');
            let (Some(n), Some(hits)) = (parts.next(), parts.next()) else {
                continue;
            };
            if hits.trim() == "0"
                && let Ok(n) = n.trim().parse()
            {
                zero.entry(file.clone()).or_default().insert(n);
            }
        }
    }
    let kinds: BTreeSet<String> = seen
        .iter()
        .filter_map(|f| f.rsplit_once('.').map(|(_, x)| x.to_string()))
        .collect();
    let mut gaps: BTreeMap<String, Vec<(u64, String)>> = BTreeMap::new();
    let mut unloaded: BTreeSet<String> = BTreeSet::new();
    for (f, n, text) in added_lines(diff) {
        let code = text.trim();
        if is_test_path(&f) || is_doc(&f) || code.is_empty() || comment_of(code).is_some() {
            continue;
        }
        if seen.contains(&f) {
            if zero.get(&f).is_some_and(|z| z.contains(&n)) {
                gaps.entry(f).or_default().push((n, code.to_string()));
            }
        } else if f.rsplit_once('.').is_some_and(|(_, x)| kinds.contains(x)) {
            unloaded.insert(f);
        }
    }
    let mut found = Vec::new();
    for (f, lines) in gaps {
        let shown: String = lines
            .iter()
            .take(15)
            .map(|(n, code)| format!("- {n}: {}\n", clip(code, 160)))
            .collect();
        found.push(Raw {
            title: format!("No test runs {} changed line(s) in {f}", lines.len()),
            detail: format!(
                "The coverage report shows these lines, which the change adds or changes, \
                 run by no test:\n{shown}"
            ),
            file: Some(f),
            line: lines.first().map(|(n, _)| *n),
        });
    }
    for f in unloaded {
        found.push(Raw {
            title: format!("No test loads {f}, which the change touches"),
            detail: format!(
                "{f} isn't in the coverage report at all, so no test runs any of the change's \
                 lines in it."
            ),
            file: Some(f),
            line: None,
        });
    }
    found
}

/// The test gate's runs.
pub struct GateRuns<'a> {
    pub runner: Runner<'a>,
    /// The coverage command, if there's one.
    pub coverage: Option<&'a str>,
    /// Where its lcov report is, from the root.
    pub report: &'a str,
    pub diff: &'a str,
    /// Other ways to run the change's own tests, each once.
    pub variants: &'a [super::config::Variant],
    /// How many plain runs of them, beyond the first.
    pub repeat: u32,
}

/// The test files a diff adds or changes.
pub fn changed_tests(diff: &str) -> Vec<String> {
    let mut files: Vec<String> = Vec::new();
    for (f, _, _) in added_lines(diff) {
        if is_test_path(&f) && !is_doc(&f) && !files.contains(&f) {
            files.push(f);
        }
    }
    files
}

/// Runs the change's own tests each other way, and again plainly: a run
/// that fails is a finding.
fn vary(g: &GateRuns) -> Result<(Vec<Raw>, String)> {
    let r = &g.runner;
    let name = r
        .repo
        .file_name()
        .map_or_else(|| "repo".into(), |n| n.to_owned());
    let copy = r.dir.join("checks").join(&name);
    let tests: Vec<String> = changed_tests(g.diff)
        .into_iter()
        .filter(|t| r.repo.join(t).is_file())
        .collect();
    if tests.is_empty() || (g.variants.is_empty() && g.repeat == 0) {
        return Ok((Vec::new(), String::new()));
    }
    copy_of(r.repo, &copy).context("copying the change to run its tests other ways")?;
    let mut runs: Vec<(String, &str)> = g
        .variants
        .iter()
        .map(|v| (v.name.clone(), v.command.as_str()))
        .collect();
    for n in 0..g.repeat {
        runs.push((
            format!("on plain run {} of {}", n + 2, g.repeat + 1),
            r.command,
        ));
    }
    let mut found = Vec::new();
    for (i, (what, command)) in runs.iter().enumerate() {
        let log = r.dir.join(format!("check-{i}.log"));
        let (ran, out) = run_tests(&copy, command, &tests, r.limit, &log)?;
        if ran != Ran::Passed {
            let how = if ran == Ran::TimedOut {
                format!("ran past {} s", r.limit.as_secs())
            } else {
                "failed".to_string()
            };
            found.push(Raw {
                title: format!("The change's tests fail {what}"),
                detail: format!(
                    "They pass when run as the change's tests command runs them, but {how} \
                     run as `{}`. The end of the run:\n```\n{}\n```",
                    command_line(command, &tests),
                    clip(&out, 1500)
                ),
                file: tests.first().cloned(),
                line: None,
            });
        }
    }
    let _ = fs::remove_dir_all(r.dir.join("checks"));
    Ok((
        found.clone(),
        format!(
            "the change's tests run {} other way(s): {} failed",
            runs.len(),
            found.len()
        ),
    ))
}

/// Runs the gate: coverage first, which has to pass as it stands, then the
/// mutants. Returns the gaps as findings, with a summary.
/// A gap the gate found, and whether a fact it measured blocks by rule: a
/// mutant that survives in a line the change adds, against a test the change
/// adds or changes, or a test of the change's that fails run another way.
#[derive(Debug, Clone)]
pub struct Gap {
    pub raw: Raw,
    pub blocks: bool,
}

/// The lines a change adds, by file.
pub type ChangedLines = std::collections::BTreeMap<String, std::collections::BTreeMap<u64, String>>;

/// The lines a diff adds, by file and number, with their text, leaving out
/// ones it only moves: a line whose text the diff also removes somewhere is
/// old code in a new place.
pub fn changed_lines(diff: &str) -> ChangedLines {
    let removed: std::collections::BTreeSet<&str> = diff
        .lines()
        .filter(|l| l.starts_with('-') && !l.starts_with("---"))
        .map(|l| l[1..].trim())
        .filter(|l| !l.is_empty())
        .collect();
    let mut lines: ChangedLines = Default::default();
    for (f, n, text) in added_lines(diff) {
        if !removed.contains(text.trim()) {
            lines.entry(f).or_default().insert(n, text);
        }
    }
    lines
}

/// Whether the change adds these words: a quote from it, found in one
/// file's added lines read as a single text, with comment markers, quote
/// marks, case and spacing evened out. A quote can skip words with "...",
/// and one too short to tell apart from any other line never matches.
pub fn adds_text(changed: &ChangedLines, quote: &str) -> bool {
    let Some(parts) = quote_parts(quote) else {
        return false;
    };
    changed.values().any(|lines| {
        let text = plain_words(
            &lines
                .values()
                .map(|l| uncommented(l))
                .collect::<Vec<_>>()
                .join(" "),
        );
        parts.iter().all(|p| text.contains(p.as_str()))
    })
}

/// Whether `text` has the words `quote` gives, evened out as `adds_text`
/// evens them out: for a quote from a lens's own definition of a blocker.
pub fn quotes_text(text: &str, quote: &str) -> bool {
    let Some(parts) = quote_parts(quote) else {
        return false;
    };
    let text = plain_words(text);
    parts.iter().all(|p| text.contains(p.as_str()))
}

/// A quote's parts, split where it skips words with "...", evened out; none
/// if it's too short to tell apart from any other text.
fn quote_parts(quote: &str) -> Option<Vec<String>> {
    let parts: Vec<String> = quote
        .split("...")
        .flat_map(|p| p.split('\u{2026}'))
        .map(plain_words)
        .filter(|p| !p.is_empty())
        .collect();
    (parts.iter().map(|p| p.chars().count()).sum::<usize>() >= 8).then_some(parts)
}

fn uncommented(line: &str) -> &str {
    let line = line.trim_start();
    ["///", "//!", "//", "/**", "/*", "<!--", "--", "#", "*"]
        .iter()
        .find_map(|m| line.strip_prefix(m))
        .unwrap_or(line)
}

fn plain_words(text: &str) -> String {
    text.chars()
        .filter(|c| {
            !matches!(
                c,
                '\'' | '"' | '`' | '\\' | '\u{2018}' | '\u{2019}' | '\u{201c}' | '\u{201d}'
            )
        })
        .collect::<String>()
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Whether a mutant breaks a line the change adds, and is set against a
/// test the change adds or changes.
fn in_the_change(repo: &Path, m: &Mutant, diff: &str) -> bool {
    let changed = changed_lines(diff);
    let Some(lines) = changed.get(&m.file) else {
        return false;
    };
    let Ok(text) = fs::read_to_string(repo.join(&m.file)) else {
        return false;
    };
    let Some(at) = text.find(&m.find) else {
        return false;
    };
    let first = text[..at].matches('\n').count() as u64 + 1;
    let last = first + m.find.matches('\n').count() as u64;
    let touched = (first..=last).any(|n| lines.contains_key(&n));
    let own = changed_tests(diff);
    touched && m.tests.iter().any(|t| own.contains(t))
}

pub fn run_gate(g: &GateRuns, mutants: &[Mutant]) -> Result<(Vec<Gap>, String)> {
    let r = &g.runner;
    let mut found: Vec<Gap> = Vec::new();
    let mut summary = Vec::new();
    if let Some(command) = g.coverage {
        let name = r
            .repo
            .file_name()
            .map_or_else(|| "repo".into(), |n| n.to_owned());
        let copy = r.dir.join("coverage").join(&name);
        copy_of(r.repo, &copy).context("copying the change to measure coverage")?;
        let (ran, out) = run_tests(&copy, command, &[], r.limit, &r.dir.join("coverage.log"))?;
        if ran != Ran::Passed {
            bail!(
                "the coverage run didn't pass as the change stands ({command}): {}",
                clip(&out, 1500)
            );
        }
        let text = fs::read_to_string(copy.join(g.report))
            .with_context(|| format!("reading the coverage report {}", g.report))?;
        let gaps = uncovered(&text, &copy, g.diff);
        summary.push(format!(
            "coverage: {} file(s) with changed lines no test runs",
            gaps.len()
        ));
        found.extend(gaps.into_iter().map(|raw| Gap { raw, blocks: false }));
        let _ = fs::remove_dir_all(r.dir.join("coverage"));
    }
    let (survivors, mutated) = mutate(r, mutants)?;
    summary.push(mutated);
    found.extend(survivors.into_iter().map(|(i, raw)| Gap {
        raw,
        blocks: in_the_change(r.repo, &mutants[i], g.diff),
    }));
    let (varied, ways) = vary(g)?;
    if !ways.is_empty() {
        summary.push(ways);
    }
    found.extend(varied.into_iter().map(|raw| Gap { raw, blocks: true }));
    Ok((found, summary.join("; ")))
}

/// Whether `tests` pass in `dir`: Some(true) if they do, Some(false) if they
/// fail, None if they ran past `limit`.
pub fn tests_pass(
    dir: &Path,
    command: &str,
    tests: &[String],
    limit: Duration,
    log: &Path,
) -> Result<Option<bool>> {
    Ok(match run_tests(dir, command, tests, limit, log)?.0 {
        Ran::Passed => Some(true),
        Ran::Failed => Some(false),
        Ran::TimedOut => None,
    })
}

/// The test writer's brief: pins for the blockers just fixed, and tests
/// that close the gate's gaps, written by a session that didn't fix them.
pub fn writer_prompt(intent: &str, blockers: &str, runbook: Option<&str>) -> String {
    let runbook = runbook
        .filter(|r| !r.trim().is_empty())
        .map(|r| format!("# How to work in this repository\n\n{}\n\n", r.trim()))
        .unwrap_or_default();
    format!(
        "You write tests for a change, between rounds of an adversarial review. Fresh \
         reviewers attacked it, a Judge upheld the blockers below, and they've just been fixed \
         in the working directory. You didn't fix them, and you have no case to make for the \
         fixes: write the tests that show whether they hold.\n\n\
         {intent}\n\
         # The blockers just fixed\n\n{blockers}\n\
         # What to write\n\n\
         For each blocker, a test that fails on the code as it was before the fix and passes \
         now: its pin. Where a finding comes with a test or scenario that shows it, use that. \
         Where a blocker is a gap in the tests, a line no test runs or a break every test \
         passes, write the test that closes it: one that runs the line, or fails on the \
         break. Put each where the repository keeps its tests, in their style. Change nothing \
         but tests. Run what you write. Don't commit. Each test file you name is then run on \
         the code from before the fixes, where it must fail, and after, where it must pass.\n\n\
         {runbook}\
         Answer with only JSON: {{\"tests\": [{{\"file\": \"<test file, from the root>\", \
         \"pins\": [\"<finding id>\"]}}], \"done\": \"<what you wrote, in a few lines>\"}}."
    )
}

#[derive(Debug, Clone, Deserialize)]
pub struct Written {
    #[serde(default)]
    pub tests: Vec<WrittenTest>,
    #[serde(default)]
    pub done: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WrittenTest {
    pub file: String,
    #[serde(default)]
    pub pins: Vec<String>,
}

pub fn read_written(text: &str) -> Result<Written> {
    super::roles::json_in::<Written>(text)
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
        let Pack { said, leads } = context_pack(&repo, base.trim(), &change);
        // What people wrote, earlier commits and docs, is said about the
        // change: claims, under SAID's frame, with no heading of its own.
        assert!(
            said.contains("## Earlier commits to the changed files")
                && said.contains("lib/editor.dart:")
                && said.contains("docs/notes.md:1:editor.dart saves edits")
                && !said.contains("\n# ")
                && !said.starts_with("# "),
            "{said}"
        );
        // Where the tests and callers are is a lead.
        assert!(
            leads.starts_with("# Leads in the repository")
                && leads.contains("lib/editor.dart: test/editor_test.dart")
                && leads.contains("saveEdit:")
                && !leads.contains("docs/notes.md"),
            "{leads}"
        );
    }

    #[test]
    fn the_mutant_proposer_is_asked_for_the_full_count_and_no_padding() {
        // One proposer, told "up to 12", gave one break and a placeholder
        // that changed nothing.
        let prompt = mutants_prompt("the change", 12, "");
        assert!(
            prompt.contains("Propose 12,") && !prompt.contains("up to"),
            "{prompt}"
        );
        assert!(prompt.contains("never pad the list"), "{prompt}");
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
        // A run past the limit gives no verdict, rather than counting as caught.
        let hangs = vec![Mutant {
            file: "lib.sh".into(),
            find: "add() { echo".into(),
            replace: "add() { sleep 30; echo".into(),
            why: "add returns".into(),
            tests: vec!["t.sh".into()],
        }];
        let quick = Runner {
            repo: &repo,
            dir: &dir,
            command: "sh {tests}",
            parallel: 1,
            limit: Duration::from_secs(3),
        };
        let (found, summary) = run_mutants(&quick, &hangs).unwrap();
        assert!(found.is_empty());
        assert_eq!(
            summary,
            "1 mutants: 0 caught, 0 missed, 0 unusable, 1 timed out (past 3 s, so no verdict)"
        );
        // Tests that fail before any break judge nothing.
        let failing = Runner {
            command: "false",
            ..runner
        };
        assert!(run_mutants(&failing, &mutants).is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn changed_lines_no_test_runs_are_gaps_and_an_unloaded_file_is_one() {
        let report = "SF:/copy/lib/a.dart\nDA:10,3\nDA:11,0\nDA:12,0\nend_of_record\n\
                      SF:lib/c.dart\nDA:1,1\nend_of_record\n";
        let diff = [
            "+++ b/lib/a.dart",
            "@@ -9,1 +10,4 @@",
            "+  x = 1;",
            "+  y = 2;",
            "+  // a comment",
            "+  z = 3;",
            "+++ b/lib/b.dart",
            "@@ -0,0 +1,1 @@",
            "+void f() {}",
            "+++ b/test/a_test.dart",
            "@@ -0,0 +1,1 @@",
            "+test();",
            "+++ b/pubspec.yaml",
            "@@ -0,0 +1,1 @@",
            "+name: x",
        ]
        .join("\n");
        let found = uncovered(report, Path::new("/copy"), &diff);
        let titles: Vec<&str> = found.iter().map(|f| f.title.as_str()).collect();
        assert_eq!(
            titles,
            [
                "No test runs 1 changed line(s) in lib/a.dart",
                "No test loads lib/b.dart, which the change touches"
            ]
        );
        assert!(found[0].detail.contains("- 11: y = 2;"));
        assert_eq!(found[0].line, Some(11));
    }

    #[test]
    fn the_changes_tests_are_run_other_ways_and_a_failure_is_a_finding() {
        let repo = repo_with(&[("test/t.sh", "[ \"$1\" != strict ]\n")]);
        let dir = std::env::temp_dir().join(format!("tb-acr-vary-{}", crate::util::new_id()));
        let variants = vec![
            crate::acr::config::Variant {
                name: "in strict mode".into(),
                command: "sh {tests} strict".into(),
            },
            crate::acr::config::Variant {
                name: "in loose mode".into(),
                command: "sh {tests} loose".into(),
            },
        ];
        let runs = GateRuns {
            runner: Runner {
                repo: &repo,
                dir: &dir,
                command: "sh {tests}",
                parallel: 1,
                limit: Duration::from_secs(30),
            },
            coverage: None,
            report: "coverage/lcov.info",
            diff: "+++ b/test/t.sh\n@@ -0,0 +1 @@\n+[ \"$1\" != strict ]\n",
            variants: &variants,
            repeat: 2,
        };
        let (found, summary) = run_gate(&runs, &[]).unwrap();
        assert_eq!(found.len(), 1);
        assert!(
            found[0].blocks,
            "a test of the change's own that fails another way blocks"
        );
        assert_eq!(found[0].raw.title, "The change's tests fail in strict mode");
        assert!(found[0].raw.detail.contains("sh 'test/t.sh' strict"));
        assert!(
            summary.contains("run 4 other way(s): 1 failed"),
            "{summary}"
        );
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
