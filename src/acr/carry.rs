//! Work that isn't code, carried into git. ACR reviews a change: a
//! repository, the commit the change was made on, and its head. A design,
//! a document or a plan has versions but no repository, so each version is
//! committed to a carrier repository kept in tailbus's state. The rest of
//! ACR then reviews it like any other change, and never needs to know.
//!
//! A version is a file or a folder of files, as text: a design exported to
//! Markdown, say, in a stable order, so two exports of the same version
//! match. With no earlier version, the whole of the work is new.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

use crate::util::valid_name;

/// Commits carry fixed names and dates, so the same versions always give
/// the same commits, and carrying them again changes nothing.
const WHO: [(&str, &str); 6] = [
    ("GIT_AUTHOR_NAME", "acr"),
    ("GIT_AUTHOR_EMAIL", "acr@localhost"),
    ("GIT_AUTHOR_DATE", "2000-01-01T00:00:00Z"),
    ("GIT_COMMITTER_NAME", "acr"),
    ("GIT_COMMITTER_EMAIL", "acr@localhost"),
    ("GIT_COMMITTER_DATE", "2000-01-01T00:00:00Z"),
];

/// A carried change: the carrier repository, and the two commits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Carried {
    pub repo: PathBuf,
    pub base: String,
    pub head: String,
}

/// Where a carrier repository lives. Its name is its folder's, so its
/// review config is `~/.config/tailbus/acr/<name>.yaml`, as any
/// repository's is.
pub fn carrier(state: &Path, name: &str) -> PathBuf {
    state.join("acr-carried").join(name)
}

/// Commits `base` (if there is one) and then `head` to the carrier `name`,
/// and checks `head` out there, so a review can read it in place.
pub fn carry(state: &Path, name: &str, base: Option<&Path>, head: &Path) -> Result<Carried> {
    if !valid_name(name) {
        bail!("{name:?} isn't a carrier name: use lowercase letters, digits and hyphens");
    }
    let repo = carrier(state, name);
    if !repo.join(".git").exists() {
        fs::create_dir_all(&repo)?;
        git(&repo, &["init", "--quiet"], None, None)?;
    }
    let base_tree = match base {
        Some(b) => tree(&repo, &files(b)?)?,
        None => tree(&repo, &BTreeMap::new())?,
    };
    let what = if base.is_some() {
        "The version before the work"
    } else {
        "Nothing yet: the whole of the work is new"
    };
    let base_commit = commit(&repo, &base_tree, None, what)?;
    let head_tree = tree(&repo, &files(head)?)?;
    let head_commit = commit(
        &repo,
        &head_tree,
        Some(&base_commit),
        "The version under review",
    )?;
    // A ref keeps both commits from being collected as unreachable.
    git(
        &repo,
        &[
            "update-ref",
            &format!("refs/carried/{head_commit}"),
            &head_commit,
        ],
        None,
        None,
    )?;
    git(
        &repo,
        &["checkout", "--quiet", "--force", "--detach", &head_commit],
        None,
        None,
    )?;
    git(&repo, &["clean", "-fdq"], None, None)?;
    Ok(Carried {
        repo,
        base: base_commit,
        head: head_commit,
    })
}

/// A version's files, by the path they get in the carrier: a file by its
/// own name, a folder's files by their paths inside it.
fn files(version: &Path) -> Result<BTreeMap<String, Vec<u8>>> {
    let mut out = BTreeMap::new();
    if version.is_file() {
        let name = version
            .file_name()
            .with_context(|| format!("{} has no file name", version.display()))?
            .to_string_lossy()
            .into_owned();
        out.insert(
            name,
            fs::read(version).with_context(|| format!("reading {}", version.display()))?,
        );
        return Ok(out);
    }
    if !version.is_dir() {
        bail!("{} isn't a file or a folder", version.display());
    }
    walk(version, version, &mut out)?;
    if out.is_empty() {
        bail!("{} has no files", version.display());
    }
    Ok(out)
}

fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, Vec<u8>>) -> Result<()> {
    for entry in fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let path = entry?.path();
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        if name.starts_with('.') {
            continue;
        }
        if path.is_dir() {
            walk(root, &path, out)?;
        } else if path.is_file() {
            let rel = path
                .strip_prefix(root)?
                .to_string_lossy()
                .replace('\\', "/");
            out.insert(rel, fs::read(&path)?);
        }
    }
    Ok(())
}

/// A tree of these files, written through an index of its own so the
/// carrier's checkout isn't touched.
fn tree(repo: &Path, files: &BTreeMap<String, Vec<u8>>) -> Result<String> {
    let index = repo.join(".git").join("carry-index");
    let _ = fs::remove_file(&index);
    for (path, bytes) in files {
        let blob = git(repo, &["hash-object", "-w", "--stdin"], Some(bytes), None)?;
        git(
            repo,
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("100644,{},{path}", blob.trim()),
            ],
            None,
            Some(&index),
        )?;
    }
    let tree = git(repo, &["write-tree"], None, Some(&index))?;
    let _ = fs::remove_file(&index);
    Ok(tree.trim().to_string())
}

fn commit(repo: &Path, tree: &str, parent: Option<&str>, message: &str) -> Result<String> {
    let mut args = vec!["commit-tree", tree, "-m", message];
    if let Some(p) = parent {
        args.extend(["-p", p]);
    }
    Ok(git(repo, &args, None, None)?.trim().to_string())
}

fn git(repo: &Path, args: &[&str], input: Option<&[u8]>, index: Option<&Path>) -> Result<String> {
    let mut cmd = Command::new("git");
    cmd.arg("-C")
        .arg(repo)
        .args(["-c", "commit.gpgsign=false"])
        .args(args)
        .envs(WHO)
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(index) = index {
        cmd.env("GIT_INDEX_FILE", index);
    }
    let mut child = cmd.spawn().context("running git")?;
    if let Some(bytes) = input
        && let Some(mut stdin) = child.stdin.take()
    {
        stdin.write_all(bytes)?;
    }
    let out = child.wait_with_output()?;
    if !out.status.success() {
        bail!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tb-carry-{}", crate::util::new_id()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_documents_two_versions_become_a_change_git_can_diff() {
        let dir = scratch();
        let (base, head) = (dir.join("base.md"), dir.join("head.md"));
        fs::write(&base, "# Board\n\n- Step 1\n- Step 2\n").unwrap();
        fs::write(&head, "# Board\n\n- Step 1\n- Step 2b\n- Step 3\n").unwrap();
        let state = dir.join("state");
        let c = carry(&state, "design", Some(&base), &head).unwrap();
        // The file keeps the head's name in both versions, so the change is
        // a diff of one file, not a rename.
        let diff = super::super::change::git(&c.repo, &["diff", &c.base, &c.head]).unwrap();
        assert!(
            diff.contains("+- Step 2b") && diff.contains("-- Step 2"),
            "{diff}"
        );
        // It's checked out at the head, clean, for a review in place.
        let status = super::super::change::git(&c.repo, &["status", "--short"]).unwrap();
        assert!(status.trim().is_empty(), "{status}");
        assert_eq!(
            fs::read_to_string(c.repo.join("head.md")).unwrap(),
            fs::read_to_string(&head).unwrap()
        );
        // Carrying the same versions again gives the same commits.
        assert_eq!(carry(&state, "design", Some(&base), &head).unwrap(), c);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn with_no_earlier_version_the_whole_work_is_new_and_folders_keep_their_paths() {
        let dir = scratch();
        let head = dir.join("v2");
        fs::create_dir_all(head.join("frames")).unwrap();
        fs::write(head.join("frames/one.md"), "one\n").unwrap();
        fs::write(head.join("index.md"), "index\n").unwrap();
        fs::write(head.join(".DS_Store"), "junk").unwrap();
        let c = carry(&dir.join("state"), "plan", None, &head).unwrap();
        let names =
            super::super::change::git(&c.repo, &["diff", "--name-status", &c.base, &c.head])
                .unwrap();
        assert_eq!(names.trim(), "A\tframes/one.md\nA\tindex.md");
        assert!(carry(&dir.join("state"), "Bad Name", None, &head).is_err());
        fs::remove_dir_all(&dir).unwrap();
    }
}
