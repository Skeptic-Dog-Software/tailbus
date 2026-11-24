//! The daemon's hourly cleanup. Bridges delete nothing as their runs
//! finish: every run's log and folder stays, to be looked at, until it's
//! older than its keeping. Each is packed into a `.tar.gz` once it's
//! finished, so an agent that searches files later doesn't come on earlier
//! runs' work, and each is named for when it began, which is how old this
//! takes it to be.
//! This clears them once they're past keeping, and packs any finished one
//! that wasn't. Nothing else removes them. Cheap: a look at each folder's
//! names.

use std::fs;
use std::time::Duration;

use crate::config::Paths;

/// Days what a run left is kept, packed, unless a bridge's
/// `log_retention_days` says otherwise for its runs.
pub const KEEP_DAYS: u32 = 30;
/// How often the daemon cleans up.
pub const EVERY: Duration = Duration::from_secs(3600);
/// How long something finished may stay unpacked before the cleanup packs
/// it, as when packing failed as it finished.
pub const PACK_AFTER: Duration = Duration::from_secs(3600);

/// Clears what's past keeping in this machine's state. Returns what it did,
/// a line for each place, for the daemon's log.
pub fn sweep(paths: &Paths) -> Vec<String> {
    let mut said = Vec::new();
    let bridges = fs::read_dir(paths.state.join("bridge"));
    let mut agents: Vec<String> = bridges
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    agents.sort();
    for agent in agents {
        let swept = crate::bridge::sweep(paths, &agent);
        if swept.runs > 0 {
            said.push(format!(
                "cleared {} run(s) the {agent} bridge kept past their keeping",
                swept.runs
            ));
        }
        if swept.packed > 0 {
            said.push(format!(
                "packed {} finished run(s) the {agent} bridge hadn't",
                swept.packed
            ));
        }
        if swept.log_cut {
            said.push(format!(
                "cut the {agent} bridge's log back to its last lines"
            ));
        }
    }
    said
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::time::SystemTime;

    /// Makes `path` look last changed `days` ago.
    fn age(path: &Path, days: u64) {
        let then = SystemTime::now() - Duration::from_secs(days * 86_400);
        fs::File::open(path).unwrap().set_modified(then).unwrap();
    }

    #[test]
    fn the_cleanup_clears_only_whats_past_keeping_and_never_an_open_run() {
        let home = std::env::temp_dir().join(format!("tb-cleanup-{}", crate::util::new_id()));
        let paths = Paths {
            config: home.join("config.toml"),
            state: home.join("state"),
        };
        let days_ago =
            |days: u32| crate::util::stamp(crate::util::now_ms() - i64::from(days) * 86_400_000);
        let (old, new) = (days_ago(KEEP_DAYS + 1), days_ago(1));
        let bridge = paths.state.join("bridge/claude");
        let place = |p: &str| paths.state.join(p);
        let file = |p: &str| {
            let path = place(p);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, "x").unwrap();
            path
        };
        let folder = |p: &str| {
            let path = place(p);
            fs::create_dir_all(path.join("1")).unwrap();
            path
        };
        let gone = [file(&format!("bridge/claude/runs/{old}-OLD.tar.gz"))];
        // One from before names had times goes by when it last changed.
        let legacy = folder("bridge/claude/log/LEGACY");
        age(&legacy, u64::from(KEEP_DAYS) + 1);
        let stay = [
            file(&format!("bridge/claude/runs/{new}-NEW.tar.gz")),
            folder(&format!("bridge/claude/runs/{old}-OPEN")),
        ];
        fs::create_dir_all(bridge.join("inflight")).unwrap();
        fs::write(bridge.join("inflight/OPEN.json"), "{}").unwrap();
        let said = sweep(&paths);
        assert!(
            said.iter()
                .any(|l| l.contains("cleared 2 run(s) the claude bridge")),
            "{said:?}"
        );
        for path in gone.iter().chain([&legacy]) {
            assert!(!path.exists(), "{} wasn't cleared", path.display());
        }
        for path in &stay {
            assert!(path.exists(), "{} was cleared", path.display());
        }
        assert!(sweep(&paths).is_empty(), "nothing more to clear");
        fs::remove_dir_all(&home).unwrap();
    }
}
