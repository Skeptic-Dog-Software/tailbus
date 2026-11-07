//! What ACR keeps, and for how long. A finished review leaves a line of
//! history, which estimates come from, and its own files go after a week.
//! Every `tailbus acr` command cleans up first, which is cheap: a look at
//! each review's directory, and nothing read.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};

use super::flow::Run;
use super::plan::Record;
use crate::config::Paths;

const HISTORY: &str = "history.jsonl";
/// The finished reviews of each repository that history keeps.
pub const HISTORY_RUNS: usize = 20;
/// How long a finished review's files stay, for reading its report.
const FINISHED: Duration = Duration::from_secs(7 * 86_400);
/// How long a review waiting for a person stays before it's taken as left.
const UNFINISHED: Duration = Duration::from_secs(30 * 86_400);
/// In a review's directory once it's finished and noted.
const FINISHED_MARK: &str = "finished";

pub fn dir(paths: &Paths) -> PathBuf {
    paths.state.join("acr")
}

/// Holds the history's lock, so two reviews finishing at once can't lose
/// one's line.
struct Locked(File);

impl Locked {
    fn take(paths: &Paths) -> Result<Locked> {
        fs::create_dir_all(dir(paths))?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(0o600)
            .open(dir(paths).join("history.lock"))?;
        // SAFETY: flock on a descriptor this owns.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(std::io::Error::last_os_error()).context("locking ACR's history");
        }
        Ok(Locked(file))
    }
}

impl Drop for Locked {
    fn drop(&mut self) {
        // SAFETY: as above.
        unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}

fn read_history(paths: &Paths) -> Vec<Record> {
    let Ok(file) = File::open(dir(paths).join(HISTORY)) else {
        return Vec::new();
    };
    BufReader::new(file)
        .lines()
        .map_while(Result::ok)
        .filter_map(|line| serde_json::from_str(&line).ok())
        .collect()
}

/// The newest `HISTORY_RUNS` of each repository's, oldest first.
fn trimmed(mut records: Vec<Record>) -> Vec<Record> {
    records.sort_by(|a, b| a.id.cmp(&b.id));
    records.dedup_by(|a, b| a.id == b.id);
    let mut per_repo: BTreeMap<PathBuf, usize> = BTreeMap::new();
    let mut kept: Vec<Record> = records
        .into_iter()
        .rev()
        .filter(|r| {
            let n = per_repo.entry(r.repo.clone()).or_default();
            *n += 1;
            *n <= HISTORY_RUNS
        })
        .collect();
    kept.reverse();
    kept
}

fn add(paths: &Paths, record: Record) -> Result<()> {
    let _lock = Locked::take(paths)?;
    let mut records = read_history(paths);
    records.push(record);
    let mut text = String::new();
    for r in trimmed(records) {
        text.push_str(&serde_json::to_string(&r)?);
        text.push('\n');
    }
    let path = dir(paths).join(HISTORY);
    let tmp = path.with_extension("tmp");
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(&tmp)?;
    file.write_all(text.as_bytes())?;
    file.sync_all()?;
    fs::rename(&tmp, &path)?;
    Ok(())
}

/// The finished reviews of `repo` that history has.
pub fn history(paths: &Paths, repo: &Path) -> Vec<Record> {
    let want = repo.canonicalize().unwrap_or_else(|_| repo.to_path_buf());
    read_history(paths)
        .into_iter()
        .filter(|r| r.repo == want)
        .collect()
}

/// Once a review is finished: notes it in history, if `noted`, and drops
/// what only a review under way needs. Does nothing the second time.
pub fn finished(paths: &Paths, run: &Run, noted: bool) -> Result<()> {
    if !matches!(
        run.status,
        super::flow::Status::Confirmed | super::flow::Status::Stopped
    ) {
        return Ok(());
    }
    let review = dir(paths).join(&run.id);
    let mark = review.join(FINISHED_MARK);
    if mark.exists() {
        return Ok(());
    }
    if noted && let Some(record) = Record::of(run) {
        add(paths, record)?;
    }
    for entry in fs::read_dir(&review)?.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("round-") && name.ends_with(".diff") {
            let _ = fs::remove_file(entry.path());
        }
    }
    File::create(&mark)?;
    Ok(())
}

/// Removes finished reviews after `FINISHED`, and ones left waiting after
/// `UNFINISHED`, going by when each was last saved. Returns how many.
pub fn prune(paths: &Paths) -> usize {
    prune_at(paths, SystemTime::now())
}

fn prune_at(paths: &Paths, now: SystemTime) -> usize {
    let Ok(entries) = fs::read_dir(dir(paths)) else {
        return 0;
    };
    let mut gone = 0;
    for entry in entries.flatten() {
        if !entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let review = entry.path();
        let saved = fs::metadata(review.join("run.json"))
            .or_else(|_| entry.metadata())
            .and_then(|m| m.modified());
        let Ok(saved) = saved else {
            continue;
        };
        let keep = if review.join(FINISHED_MARK).exists() {
            FINISHED
        } else {
            UNFINISHED
        };
        if now.duration_since(saved).is_ok_and(|age| age > keep)
            && fs::remove_dir_all(&review).is_ok()
        {
            gone += 1;
        }
    }
    gone
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::new_id;

    fn paths() -> Paths {
        let home = std::env::temp_dir().join(format!("tb-keep-{}", &new_id()[14..]));
        fs::create_dir_all(&home).unwrap();
        Paths {
            config: home.join("config.toml"),
            state: home,
        }
    }

    fn record(id: &str, repo: &str) -> Record {
        Record {
            id: id.to_string(),
            repo: PathBuf::from(repo),
            lenses: BTreeMap::from([("a".to_string(), 2)]),
            fixes: 1,
            tokens: 10,
            usd: Some(0.5),
        }
    }

    #[test]
    fn history_keeps_the_newest_of_each_repository() {
        let p = paths();
        for n in 0..HISTORY_RUNS + 5 {
            add(&p, record(&format!("{n:04}"), "/r/one")).unwrap();
        }
        add(&p, record("0002", "/r/two")).unwrap();
        // The same review twice is one review.
        add(&p, record("0002", "/r/two")).unwrap();
        let one = history(&p, Path::new("/r/one"));
        assert_eq!(one.len(), HISTORY_RUNS);
        assert_eq!(one[0].id, "0005", "the oldest went");
        assert_eq!(history(&p, Path::new("/r/two")).len(), 1);
        fs::remove_dir_all(&p.state).unwrap();
    }

    #[test]
    fn old_reviews_go_and_finished_ones_sooner() {
        let p = paths();
        for (id, done) in [("done", true), ("waiting", false)] {
            let review = dir(&p).join(id);
            fs::create_dir_all(&review).unwrap();
            fs::write(review.join("run.json"), "{}").unwrap();
            if done {
                File::create(review.join(FINISHED_MARK)).unwrap();
            }
        }
        let now = SystemTime::now();
        assert_eq!(prune_at(&p, now), 0, "nothing is old yet");
        assert_eq!(prune_at(&p, now + FINISHED + Duration::from_secs(60)), 1);
        assert!(!dir(&p).join("done").exists());
        assert!(dir(&p).join("waiting").exists());
        assert_eq!(prune_at(&p, now + UNFINISHED + Duration::from_secs(60)), 1);
        assert!(!dir(&p).join("waiting").exists());
        fs::remove_dir_all(&p.state).unwrap();
    }
}
