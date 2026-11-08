//! What's under review, from git. The repository's config and its list of
//! decided non-issues always come from the base, never the change, so a
//! change can't switch off its own review.

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::LazyLock;

use anyhow::{Context, Result, bail};
use globset::{Glob, GlobSet, GlobSetBuilder};
use regex::Regex;
use serde::Serialize;

use super::config::{Generated, REPO_CONFIG, ReviewScope};

#[derive(Debug, Clone, Default)]
pub struct Change {
    /// Every changed file, generated ones too.
    pub files: Vec<String>,
    /// What reviewers get: the diff without generated files, which are
    /// listed at its top.
    pub diff: String,
    /// The whole diff, when files were left out, for a lens that reads them.
    pub full: Option<String>,
    pub generated: Vec<LeftOut>,
}

/// A generated file left out of the diff a review sends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LeftOut {
    pub path: String,
    pub added: u64,
    pub removed: u64,
    /// What says it's generated.
    pub why: String,
}

/// How generated files say so in their first lines, by the common
/// conventions of code generators. Nothing here is any one language's.
static MARKER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)@generated\b|\bgenerated\b.*\bdo not (edit|modify)\b|\bdo not (edit|modify)\b.*\bgenerated\b",
    )
    .expect("a valid pattern")
});
/// How far into a file its marker can be.
const MARKER_LINES: usize = 5;

fn git(repo: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .context("running git")?;
    if !out.status.success() {
        bail!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// A file as it is on `base`, if it's there.
pub fn at_base(repo: &Path, base: &str, path: &str) -> Result<Option<String>> {
    git(
        repo,
        &["rev-parse", "--verify", &format!("{base}^{{commit}}")],
    )
    .with_context(|| format!("there's no {base} to read the config from"))?;
    Ok(git(repo, &["show", &format!("{base}:{path}")]).ok())
}

/// The repository's config, from its base.
pub fn repo_config(repo: &Path, base: &str) -> Result<String> {
    at_base(repo, base, REPO_CONFIG)?
        .with_context(|| format!("{base} has no {REPO_CONFIG}; a change can't bring its own"))
}

/// The repository's name: its top directory's.
/// The repository's name: its main checkout's folder, so a worktree of it
/// finds the same config.
pub fn name(repo: &Path) -> Result<String> {
    let common = git(
        repo,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?;
    let common = Path::new(common.trim());
    let main = match common.file_name() {
        Some(n) if n == ".git" => common.parent().unwrap_or(common).to_path_buf(),
        _ => common.to_path_buf(),
    };
    main.file_name()
        .map(|n| n.to_string_lossy().trim_end_matches(".git").to_string())
        .filter(|n| !n.is_empty())
        .context("the repository has no name")
}

/// What the checkout is now: its commit, and every change and untracked file
/// in it. Only an Orchestrator may change it during a review.
pub fn fingerprint(repo: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    let mut hash = Sha256::new();
    for args in [
        &["rev-parse", "HEAD"][..],
        // Untracked folders count once, not by what's in them, so an
        // agent's worktrees under one don't look like a change.
        &["status", "--porcelain", "--untracked-files=normal"],
        &["diff", "HEAD", "--binary"],
    ] {
        hash.update(git(repo, args)?.as_bytes());
    }
    Ok(hash.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// Reviewers read the checkout itself, so a commit under review must be the
/// one checked out, with nothing changed on top. Without `head`, the review
/// is of the working tree, which is the checkout already.
pub fn checked_out(repo: &Path, head: Option<&str>) -> Result<()> {
    let Some(head) = head else {
        return Ok(());
    };
    let want = git(repo, &["rev-parse", &format!("{head}^{{commit}}")])?;
    let have = git(repo, &["rev-parse", "HEAD"])?;
    if want.trim() != have.trim() {
        bail!(
            "reviewers read the checkout, so {head} has to be what's checked out, and it's \
             {}. Check {head} out, or leave --head out to review the working tree.",
            &have.trim()[..12.min(have.trim().len())]
        );
    }
    let changed = git(repo, &["status", "--porcelain", "--untracked-files=no"])?;
    if !changed.trim().is_empty() {
        bail!(
            "reviewers read the checkout, which has changes on top of {head}. Commit or \
             stash them, or leave --head out to review the working tree."
        );
    }
    Ok(())
}

/// The commit a stack's review stops at: the first after the base.
fn bottom_of_stack(repo: &Path, base: &str, head: &str) -> Result<Option<String>> {
    let range = format!("{base}..{head}");
    let commits = git(repo, &["rev-list", "--reverse", &range])?;
    Ok(commits.lines().next().map(str::to_string))
}

/// The change from `base` to `head`, or to the working tree with no `head`.
pub fn load(
    repo: &Path,
    base: &str,
    head: Option<&str>,
    scope: ReviewScope,
    generated: &Generated,
) -> Result<Change> {
    let head = match (scope, head) {
        (ReviewScope::BottomOfStack, Some(head)) => {
            bottom_of_stack(repo, base, head)?.or(Some(head.to_string()))
        }
        (_, head) => head.map(str::to_string),
    };
    let range = match &head {
        Some(head) => format!("{base}...{head}"),
        None => base.to_string(),
    };
    let names = git(repo, &["diff", "--name-only", &range])?;
    let change = Change {
        files: names.lines().map(str::to_string).collect(),
        diff: git(repo, &["diff", &range])?,
        ..Change::default()
    };
    leave_out_generated(repo, head.as_deref(), change, generated)
}

fn globs(paths: &[String]) -> Result<GlobSet> {
    let mut set = GlobSetBuilder::new();
    for path in paths {
        set.add(Glob::new(path)?);
    }
    Ok(set.build()?)
}

/// Files `.gitattributes` marks `linguist-generated`, as of `head`.
fn linguist_generated(repo: &Path, head: Option<&str>, files: &[String]) -> Vec<String> {
    if files.is_empty() {
        return Vec::new();
    }
    let mut args = vec!["check-attr"];
    let source = head.map(|h| format!("--source={h}"));
    if let Some(source) = &source {
        args.push(source);
    }
    args.extend(["linguist-generated", "--"]);
    args.extend(files.iter().map(String::as_str));
    let Ok(out) = git(repo, &args) else {
        return Vec::new();
    };
    out.lines()
        .filter_map(|l| l.rsplit_once(": linguist-generated: "))
        .filter(|(_, v)| matches!(*v, "set" | "true"))
        .map(|(p, _)| p.to_string())
        .collect()
}

/// A file's first lines, as of `head`, or in the working tree.
fn first_lines(repo: &Path, head: Option<&str>, path: &str) -> String {
    let text = match head {
        Some(head) => git(repo, &["show", &format!("{head}:{path}")]).unwrap_or_default(),
        None => {
            let Ok(file) = File::open(repo.join(path)) else {
                return String::new();
            };
            let lines: Vec<String> = BufReader::new(file)
                .lines()
                .take(MARKER_LINES)
                .map_while(Result::ok)
                .collect();
            return lines.join("\n");
        }
    };
    text.lines()
        .take(MARKER_LINES)
        .collect::<Vec<_>>()
        .join("\n")
}

/// Splits a diff into each file's part, by its `diff --git` line.
fn per_file(diff: &str) -> Vec<(String, String)> {
    let mut parts: Vec<(String, String)> = Vec::new();
    for line in diff.split_inclusive('\n') {
        if let Some(header) = line.strip_prefix("diff --git ") {
            let path = header
                .trim_end()
                .rsplit_once(" b/")
                .map_or(header.trim_end(), |(_, b)| b)
                .to_string();
            parts.push((path, String::new()));
        }
        match parts.last_mut() {
            Some((_, text)) => text.push_str(line),
            None => parts.push((String::new(), line.to_string())),
        }
    }
    parts
}

fn counts(part: &str) -> (u64, u64) {
    let mut added = 0;
    let mut removed = 0;
    for line in part.lines() {
        if line.starts_with('+') && !line.starts_with("+++") {
            added += 1;
        } else if line.starts_with('-') && !line.starts_with("---") {
            removed += 1;
        }
    }
    (added, removed)
}

/// Takes generated files out of the diff, listing each at its top. Never
/// silently: a hand edit hidden in a generated file still shows up there.
fn leave_out_generated(
    repo: &Path,
    head: Option<&str>,
    mut change: Change,
    rules: &Generated,
) -> Result<Change> {
    if !rules.exclude || change.files.is_empty() {
        return Ok(change);
    }
    let (paths, keep) = (globs(&rules.paths)?, globs(&rules.keep)?);
    let attributed = linguist_generated(repo, head, &change.files);
    let why = |path: &str| -> Option<String> {
        if keep.is_match(path) {
            return None;
        }
        if paths.is_match(path) {
            return Some("generated.paths".into());
        }
        if attributed.iter().any(|p| p == path) {
            return Some("linguist-generated".into());
        }
        let marked = rules.markers && MARKER.is_match(&first_lines(repo, head, path));
        marked.then(|| "marked generated".into())
    };
    let mut kept = String::new();
    for (path, part) in per_file(&change.diff) {
        match (!path.is_empty()).then(|| why(&path)).flatten() {
            Some(why) => {
                let (added, removed) = counts(&part);
                change.generated.push(LeftOut {
                    path,
                    added,
                    removed,
                    why,
                });
            }
            None => kept.push_str(&part),
        }
    }
    if change.generated.is_empty() {
        return Ok(change);
    }
    let mut diff = String::from(
        "# Generated files are left out of this diff, with their lines added and removed.\n\
         # Each is in the repository, to read if a finding needs it.\n",
    );
    for g in &change.generated {
        diff.push_str(&format!(
            "#   {} (+{} -{}): {}\n",
            g.path, g.added, g.removed, g.why
        ));
    }
    diff.push('\n');
    diff.push_str(&kept);
    change.full = Some(std::mem::replace(&mut change.diff, diff));
    Ok(change)
}

impl Change {
    /// Lines the change adds or removes, without the file headers or the
    /// generated files left out.
    pub fn changed_lines(&self) -> impl Iterator<Item = &str> {
        self.diff.lines().filter(|l| {
            (l.starts_with('+') && !l.starts_with("+++"))
                || (l.starts_with('-') && !l.starts_with("---"))
        })
    }
}

#[cfg(test)]
pub mod tests {
    use std::fs;
    use std::path::PathBuf;

    use super::*;
    use crate::util::new_id;

    /// A git repository with one commit of `files` on `main`.
    pub fn repo_with(files: &[(&str, &str)]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tb-acr-{}", new_id()));
        fs::create_dir_all(&dir).unwrap();
        let run = |args: &[&str]| {
            let out = Command::new("git")
                .arg("-C")
                .arg(&dir)
                .args(args)
                .output()
                .unwrap();
            assert!(out.status.success(), "{args:?}");
        };
        run(&["init", "-q", "-b", "main"]);
        run(&["config", "user.email", "t@example.com"]);
        run(&["config", "user.name", "T"]);
        commit(&dir, files, "first");
        dir
    }

    pub fn commit(dir: &Path, files: &[(&str, &str)], message: &str) {
        for (path, text) in files {
            let path = dir.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        }
        for args in [vec!["add", "-A"], vec!["commit", "-q", "-m", message]] {
            let out = Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(&args)
                .output()
                .unwrap();
            assert!(out.status.success(), "{args:?}");
        }
    }

    #[test]
    fn generated_files_are_left_out_and_listed() {
        let dir = repo_with(&[("a.txt", "one\n")]);
        branch(&dir, "work");
        let marked = "// GENERATED CODE - DO NOT MODIFY BY HAND\nline\nline\n";
        let tooled = "// Code generated by some-tool. DO NOT EDIT.\n\npackage x\n";
        commit(
            &dir,
            &[
                ("a.txt", "two\n"),
                ("lib/db.g.dart", marked),
                ("x/pb.txt", tooled),
                ("vendor/v.txt", "plain\n"),
                ("data/big.csv", "1,2\n"),
                (".gitattributes", "vendor/** linguist-generated\n"),
            ],
            "work",
        );
        let rules = Generated {
            paths: vec!["data/**".into()],
            ..Generated::default()
        };
        let c = load(&dir, "main", Some("work"), ReviewScope::WholePr, &rules).unwrap();
        let out: Vec<(&str, &str)> = c
            .generated
            .iter()
            .map(|g| (g.path.as_str(), g.why.as_str()))
            .collect();
        assert_eq!(
            out,
            [
                ("data/big.csv", "generated.paths"),
                ("lib/db.g.dart", "marked generated"),
                ("vendor/v.txt", "linguist-generated"),
                ("x/pb.txt", "marked generated"),
            ]
        );
        let db = c
            .generated
            .iter()
            .find(|g| g.path == "lib/db.g.dart")
            .unwrap();
        assert_eq!((db.added, db.removed), (3, 0));
        // Listed at the top, never dropped silently, and still changed files.
        assert!(
            c.diff
                .contains("#   lib/db.g.dart (+3 -0): marked generated")
        );
        assert!(!c.diff.contains("+line"));
        assert!(c.diff.contains("+two"));
        assert!(c.full.as_ref().unwrap().contains("+line"));
        assert_eq!(c.files.len(), 6);
        assert!(!c.changed_lines().any(|l| l.contains("GENERATED")));
        // A repository can keep one, or turn it all off.
        let keep = Generated {
            keep: vec!["lib/**".into()],
            ..Generated::default()
        };
        let kept = load(&dir, "main", Some("work"), ReviewScope::WholePr, &keep).unwrap();
        assert!(kept.generated.iter().all(|g| g.path != "lib/db.g.dart"));
        let off = Generated {
            exclude: false,
            ..Generated::default()
        };
        let all = load(&dir, "main", Some("work"), ReviewScope::WholePr, &off).unwrap();
        assert!(all.generated.is_empty() && all.full.is_none());
        let no_markers = Generated {
            markers: false,
            ..Generated::default()
        };
        let plain = load(
            &dir,
            "main",
            Some("work"),
            ReviewScope::WholePr,
            &no_markers,
        )
        .unwrap();
        assert_eq!(plain.generated.len(), 1, "only linguist-generated");
        // The working tree is read the same way.
        let tree = load(&dir, "main", None, ReviewScope::WholePr, &rules).unwrap();
        assert_eq!(tree.generated.len(), 4);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_worktree_has_its_repositorys_name() {
        let dir = repo_with(&[("a.txt", "one\n")]);
        let tree = dir.with_file_name(format!("pr-{}", new_id()));
        let out = Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args(["worktree", "add", "-q", "-b", "pr"])
            .arg(&tree)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(name(&tree).unwrap(), name(&dir).unwrap());
        assert!(name(&dir).unwrap().starts_with("tb-acr-"));
        fs::remove_dir_all(&tree).unwrap();
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_reviewed_commit_must_be_what_is_checked_out() {
        let dir = repo_with(&[("a.txt", "one\n")]);
        branch(&dir, "change");
        commit(&dir, &[("a.txt", "two\n")], "two");
        // The working tree is always fine: it's the checkout.
        assert!(checked_out(&dir, None).is_ok());
        assert!(checked_out(&dir, Some("change")).is_ok());
        let err = format!("{:#}", checked_out(&dir, Some("main")).unwrap_err());
        assert!(err.contains("checked out"), "{err}");
        fs::write(dir.join("a.txt"), "three\n").unwrap();
        let err = format!("{:#}", checked_out(&dir, Some("change")).unwrap_err());
        assert!(err.contains("changes on top"), "{err}");
        fs::remove_dir_all(&dir).unwrap();
    }

    pub fn branch(dir: &Path, name: &str) {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["checkout", "-q", "-b", name])
            .output()
            .unwrap();
        assert!(out.status.success());
    }

    #[test]
    fn the_config_comes_from_the_base_never_the_change() {
        let repo = repo_with(&[(REPO_CONFIG, "base config"), ("a.txt", "one\n")]);
        branch(&repo, "work");
        commit(&repo, &[(REPO_CONFIG, "head config")], "switch it off");
        assert_eq!(repo_config(&repo, "main").unwrap(), "base config");
        assert_eq!(repo_config(&repo, "work").unwrap(), "head config");
        // A config that only the change has doesn't count.
        let bare = repo_with(&[("a.txt", "one\n")]);
        branch(&bare, "work");
        commit(&bare, &[(REPO_CONFIG, "brought along")], "bring my own");
        let err = format!("{:#}", repo_config(&bare, "main").unwrap_err());
        assert!(err.contains("can't bring its own"), "{err}");
        assert!(repo_config(&repo, "no-such-ref").is_err());
    }

    #[test]
    fn reads_a_change_whole_or_from_the_bottom_of_its_stack() {
        let repo = repo_with(&[("a.txt", "one\n")]);
        branch(&repo, "work");
        commit(&repo, &[("b.txt", "two\n")], "first of the stack");
        commit(&repo, &[("c.txt", "three\n")], "second of the stack");
        let whole = load(
            &repo,
            "main",
            Some("work"),
            ReviewScope::WholePr,
            &Generated::default(),
        )
        .unwrap();
        assert_eq!(whole.files, ["b.txt", "c.txt"]);
        let bottom = load(
            &repo,
            "main",
            Some("work"),
            ReviewScope::BottomOfStack,
            &Generated::default(),
        )
        .unwrap();
        assert_eq!(bottom.files, ["b.txt"]);
        assert_eq!(bottom.changed_lines().collect::<Vec<_>>(), ["+two"]);
        fs::write(repo.join("a.txt"), "changed\n").unwrap();
        let tree = load(
            &repo,
            "main",
            None,
            ReviewScope::WholePr,
            &Generated::default(),
        )
        .unwrap();
        assert!(tree.files.contains(&"a.txt".to_string()));
    }
}
