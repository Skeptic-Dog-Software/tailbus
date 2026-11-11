//! Small helpers: the clock, message IDs, and agent and machine names.

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Result, bail};

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

const CROCKFORD: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// A ULID: 48 bits of milliseconds and 80 random bits in Crockford base32,
/// so IDs sort by creation time and need no coordination between machines.
pub fn new_id() -> String {
    let mut buf = [0u8; 16];
    File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf[6..]))
        .expect("reading /dev/urandom");
    ulid(now_ms() as u64, u128::from_be_bytes(buf))
}

fn ulid(ms: u64, random: u128) -> String {
    let value = (u128::from(ms & 0xFFFF_FFFF_FFFF) << 80) | (random & ((1 << 80) - 1));
    (0..26)
        .map(|i| CROCKFORD[((value >> (125 - 5 * i)) & 31) as usize] as char)
        .collect()
}

pub fn is_id(s: &str) -> bool {
    s.len() == 26 && s.bytes().all(|b| CROCKFORD.contains(&b))
}

/// When an ID was made, in milliseconds: its first 48 bits.
pub fn id_ms(id: &str) -> Option<i64> {
    if !is_id(id) {
        return None;
    }
    id.bytes().take(10).try_fold(0i64, |ms, b| {
        let digit = CROCKFORD.iter().position(|&c| c == b)?;
        Some(ms * 32 + digit as i64)
    })
}

/// Agent and machine names: lowercase letters, digits and hyphens, so a name
/// can't carry anything else into a notice or a prompt.
pub fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 63
        && !s.starts_with('-')
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// A name given for an agent, made valid if it isn't: `WebApp` is
/// `webapp`.
pub fn as_agent_name(raw: &str) -> String {
    if valid_name(raw) {
        raw.to_string()
    } else {
        normalize_name(raw)
    }
}

/// Turns a directory name or the like into a valid name.
pub fn normalize_name(raw: &str) -> String {
    let mut out = String::new();
    for c in raw.chars().flat_map(char::to_lowercase) {
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            out.push(c);
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    out.truncate(63);
    let out = out.trim_end_matches('-').to_string();
    if out.is_empty() { "agent".into() } else { out }
}

/// The agent name for a session working in `cwd`: the repository's directory
/// name, plus the worktree's inside a linked worktree, or else the
/// directory's own name.
pub fn derive_agent_name(cwd: &Path) -> String {
    let git = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args([
            "rev-parse",
            "--path-format=absolute",
            "--show-toplevel",
            "--git-common-dir",
        ])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output();
    let base = |p: &Path| {
        p.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    };
    if let Ok(out) = git
        && out.status.success()
    {
        let text = String::from_utf8_lossy(&out.stdout);
        let mut lines = text.lines();
        if let (Some(top), Some(common)) = (lines.next(), lines.next()) {
            let (top, common) = (Path::new(top), Path::new(common));
            let main = if common.file_name().is_some_and(|n| n == ".git") {
                common.parent().unwrap_or(common)
            } else {
                common
            };
            let repo = base(main);
            if main == top {
                return normalize_name(&repo);
            }
            let tree = base(top);
            let tree = tree
                .strip_prefix(&format!("{repo}-"))
                .unwrap_or(&tree)
                .to_string();
            return normalize_name(&format!("{repo}-{tree}"));
        }
    }
    normalize_name(&base(cwd))
}

/// An agent's address on the bus, `name@machine`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Address {
    pub agent: String,
    pub machine: String,
}

impl Address {
    /// Parses `name@machine`, or a bare `name` on `here`.
    pub fn parse(s: &str, here: &str) -> Result<Address> {
        let s = s.trim().to_ascii_lowercase();
        let (agent, machine) = match s.split_once('@') {
            Some((a, m)) => (a.to_string(), m.split('.').next().unwrap_or(m).to_string()),
            None => (s.clone(), here.to_string()),
        };
        if !valid_name(&agent) || !valid_name(&machine) {
            bail!("{s:?} isn't an agent address; expected name@machine");
        }
        Ok(Address { agent, machine })
    }
}

impl fmt::Display for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}@{}", self.agent, self.machine)
    }
}

/// `2026-10-23T18:04:05Z`.
pub fn rfc3339(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// Howard Hinnant's algorithm: days since 1970-01-01 to (year, month, day).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

/// Reads `2026-10-23T14:42:46.637Z`, as Claude Code's transcripts write
/// times, or `rfc3339`'s own form without the fraction. UTC only.
pub fn parse_rfc3339(s: &str) -> Option<i64> {
    let (date, time) = s.strip_suffix('Z')?.split_once('T')?;
    let mut ymd = date.splitn(3, '-').map(|p| p.parse::<i64>().ok());
    let (y, m, d) = (ymd.next()??, ymd.next()??, ymd.next()??);
    let (hms, fraction) = time.split_once('.').unwrap_or((time, ""));
    let mut hms = hms.splitn(3, ':').map(|p| p.parse::<i64>().ok());
    let (h, min, sec) = (hms.next()??, hms.next()??, hms.next()??);
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) || h > 23 || min > 59 || sec > 60 {
        return None;
    }
    let ms = match fraction {
        "" => 0,
        f if f.bytes().all(|b| b.is_ascii_digit()) => format!("{f:0<3}")[..3].parse().ok()?,
        _ => return None,
    };
    Some((days_from_civil(y, m, d) * 86_400 + h * 3600 + min * 60 + sec) * 1000 + ms)
}

/// Howard Hinnant's algorithm the other way: (year, month, day) to days
/// since 1970-01-01.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// `just now`, `5m ago`, `3h ago`, `2d ago`.
pub fn ago(then: i64, now: i64) -> String {
    let s = (now - then).max(0) / 1000;
    match s {
        0..60 => "just now".into(),
        60..3600 => format!("{}m ago", s / 60),
        3600..86_400 => format!("{}h ago", s / 3600),
        _ => format!("{}d ago", s / 86_400),
    }
}

/// A log is cut back when it passes this, in bytes.
pub const LOG_MAX: u64 = 1 << 20;
/// What's kept of it: about the last this many bytes, from a line's start.
pub const LOG_KEEP: u64 = 256 << 10;

/// Cuts a log that's grown past `max` bytes back to about its last `keep`,
/// from the start of a line. Whoever writes it must append, as
/// `append_to_logs` makes this process do, or they'd write past the end.
pub fn trim_log(path: &Path, max: u64, keep: u64) -> io::Result<bool> {
    let len = fs::metadata(path)?.len();
    if len <= max {
        return Ok(false);
    }
    let mut file = OpenOptions::new().read(true).write(true).open(path)?;
    file.seek(SeekFrom::Start(len.saturating_sub(keep)))?;
    let mut tail = Vec::new();
    file.read_to_end(&mut tail)?;
    let start = tail.iter().position(|&b| b == b'\n').map_or(0, |i| i + 1);
    file.set_len(0)?;
    // Appended, so a line written meanwhile isn't written over.
    OpenOptions::new()
        .append(true)
        .open(path)?
        .write_all(&tail[start..])?;
    Ok(true)
}

/// Makes this process's stdout and stderr append, so a log they go to can
/// be cut back under them. launchd's log files are these.
pub fn append_to_logs() {
    for fd in [libc::STDOUT_FILENO, libc::STDERR_FILENO] {
        // SAFETY: reads and sets the flags of this process's own descriptors.
        unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFL);
            if flags >= 0 && flags & libc::O_APPEND == 0 {
                libc::fcntl(fd, libc::F_SETFL, flags | libc::O_APPEND);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_long_log_keeps_its_last_whole_lines() {
        let dir = std::env::temp_dir().join(format!("tb-log-{}", &new_id()[14..]));
        fs::create_dir_all(&dir).unwrap();
        let log = dir.join("daemon.log");
        let lines: String = (0..1000).map(|n| format!("line {n:04}\n")).collect();
        fs::write(&log, &lines).unwrap();
        assert!(!trim_log(&log, 20_000, 100).unwrap(), "short enough");
        // A writer that appends, as launchd's log is once append_to_logs runs.
        let mut writer = OpenOptions::new().append(true).open(&log).unwrap();
        assert!(trim_log(&log, 1000, 100).unwrap());
        let kept = fs::read_to_string(&log).unwrap();
        assert!(kept.len() <= 100 && kept.starts_with("line "), "{kept:?}");
        assert!(kept.ends_with("line 0999\n"));
        writer.write_all(b"after\n").unwrap();
        let kept = fs::read_to_string(&log).unwrap();
        assert!(kept.ends_with("line 0999\nafter\n"), "{kept:?}");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_given_agent_name_is_made_valid_not_refused() {
        assert_eq!(as_agent_name("writer"), "writer");
        assert_eq!(as_agent_name("WebApp"), "webapp");
        assert_eq!(as_agent_name("Sci Fi"), "sci-fi");
    }

    #[test]
    fn ulids_sort_by_time() {
        assert_eq!(ulid(0, 0), "00000000000000000000000000");
        assert_eq!(ulid(1, 0), "00000000010000000000000000");
        let (a, b) = (ulid(1_000, u128::MAX), ulid(1_001, 0));
        assert!(a < b);
        assert!(is_id(&new_id()));
        assert_ne!(new_id(), new_id());
        assert!(!is_id("0000000000000000000000000I"));
        assert_eq!(
            id_ms(&ulid(1_793_196_173_546, u128::MAX)),
            Some(1_793_196_173_546)
        );
        assert_eq!(id_ms("01M61D4A7AN5PDZ56KWS6GHNNM"), Some(1_793_196_173_546));
        assert_eq!(id_ms("not an id"), None);
    }

    #[test]
    fn names() {
        assert!(valid_name("api"));
        assert!(valid_name("server"));
        assert!(!valid_name("API"));
        assert!(!valid_name("-x"));
        assert!(!valid_name("a b"));
        assert!(!valid_name(""));
        assert_eq!(normalize_name("My_Repo.rs"), "my-repo-rs");
        assert_eq!(normalize_name("--x--"), "x");
        assert_eq!(normalize_name("..."), "agent");
    }

    #[test]
    fn addresses() {
        let a = Address::parse("Sim@Server", "here").unwrap();
        assert_eq!(a.to_string(), "sim@server");
        let a = Address::parse("sim@server.example.ts.net", "here").unwrap();
        assert_eq!(a.machine, "server");
        assert_eq!(Address::parse("sim", "here").unwrap().machine, "here");
        assert!(Address::parse("si m@x", "here").is_err());
        assert!(Address::parse("@x", "here").is_err());
    }

    #[test]
    fn formats_times() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(1_793_210_645_000), "2026-10-28T18:04:05Z");
        assert_eq!(rfc3339(951_782_400_000), "2000-02-29T00:00:00Z");
        for ms in [0, 951_782_400_000, 1_793_210_645_000] {
            assert_eq!(parse_rfc3339(&rfc3339(ms)), Some(ms));
        }
        assert_eq!(
            parse_rfc3339("2026-10-23T14:42:46.637Z"),
            Some(1_792_766_566_637)
        );
        assert_eq!(
            parse_rfc3339("2026-10-23T14:42:46.6Z"),
            Some(1_792_766_566_600)
        );
        assert_eq!(parse_rfc3339("2026-10-23 14:42:46Z"), None);
        assert_eq!(parse_rfc3339("2026-13-23T14:42:46Z"), None);
        assert_eq!(parse_rfc3339("2026-10-23T14:42:46+01:00"), None);
        assert_eq!(ago(0, 59_000), "just now");
        assert_eq!(ago(0, 7_200_000), "2h ago");
    }
}
