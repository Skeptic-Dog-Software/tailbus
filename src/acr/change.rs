//! What's under review, from git. The repository's config and its list of
//! decided non-issues always come from the base, never the change, so a
//! change can't switch off its own review.

use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

use super::config::{REPO_CONFIG, ReviewScope};

#[derive(Debug, Clone)]
pub struct Change {
    pub files: Vec<String>,
    pub diff: String,
}

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
pub fn name(repo: &Path) -> Result<String> {
    let top = git(repo, &["rev-parse", "--show-toplevel"])?;
    Path::new(top.trim())
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .context("the repository has no name")
}

/// The commit a stack's review stops at: the first after the base.
fn bottom_of_stack(repo: &Path, base: &str, head: &str) -> Result<Option<String>> {
    let range = format!("{base}..{head}");
    let commits = git(repo, &["rev-list", "--reverse", &range])?;
    Ok(commits.lines().next().map(str::to_string))
}

/// The change from `base` to `head`, or to the working tree with no `head`.
pub fn load(repo: &Path, base: &str, head: Option<&str>, scope: ReviewScope) -> Result<Change> {
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
    Ok(Change {
        files: names.lines().map(str::to_string).collect(),
        diff: git(repo, &["diff", &range])?,
    })
}

impl Change {
    /// Lines the change adds or removes, without the file headers.
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
        let whole = load(&repo, "main", Some("work"), ReviewScope::WholePr).unwrap();
        assert_eq!(whole.files, ["b.txt", "c.txt"]);
        let bottom = load(&repo, "main", Some("work"), ReviewScope::BottomOfStack).unwrap();
        assert_eq!(bottom.files, ["b.txt"]);
        assert_eq!(bottom.changed_lines().collect::<Vec<_>>(), ["+two"]);
        fs::write(repo.join("a.txt"), "changed\n").unwrap();
        let tree = load(&repo, "main", None, ReviewScope::WholePr).unwrap();
        assert!(tree.files.contains(&"a.txt".to_string()));
    }
}
