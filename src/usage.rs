//! What a Claude Code session's transcripts say about its use: the tokens
//! it and its subagents used, with each API response counted once, and
//! which subagents are still running. Counts only, never prices.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::env;
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use anyhow::{Context, Result};
use serde_json::Value;

use crate::util::parse_rfc3339;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Tokens {
    pub input: u64,
    pub output: u64,
    pub cache_writes: u64,
    pub cache_reads: u64,
    /// The API responses they came from.
    pub responses: u64,
}

impl Tokens {
    pub fn total(&self) -> u64 {
        self.input + self.output + self.cache_writes + self.cache_reads
    }

    fn from_usage(usage: &Value) -> Tokens {
        let n = |k: &str| usage[k].as_u64().unwrap_or(0);
        Tokens {
            input: n("input_tokens"),
            output: n("output_tokens"),
            cache_writes: n("cache_creation_input_tokens"),
            cache_reads: n("cache_read_input_tokens"),
            responses: 1,
        }
    }

    /// The larger of each count. Claude Code writes a response once per
    /// content block, so the same response can turn up several times.
    fn max(self, other: Tokens) -> Tokens {
        Tokens {
            input: self.input.max(other.input),
            output: self.output.max(other.output),
            cache_writes: self.cache_writes.max(other.cache_writes),
            cache_reads: self.cache_reads.max(other.cache_reads),
            responses: 1,
        }
    }

    fn add(&mut self, other: Tokens) {
        self.input += other.input;
        self.output += other.output;
        self.cache_writes += other.cache_writes;
        self.cache_reads += other.cache_reads;
        self.responses += other.responses;
    }
}

#[derive(Debug, Default)]
pub struct Usage {
    /// The session's own.
    pub session: Tokens,
    /// All its subagents', together.
    pub subagents: Tokens,
    /// How many subagents used any.
    pub subagent_count: usize,
    pub models: BTreeSet<String>,
}

impl Usage {
    pub fn total(&self) -> Tokens {
        let mut all = self.session;
        all.add(self.subagents);
        all
    }
}

/// Where Claude Code keeps transcripts, one folder per project.
fn projects() -> Option<PathBuf> {
    env::var_os("CLAUDE_CONFIG_DIR")
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|h| PathBuf::from(h).join(".claude")))
        .map(|config| config.join("projects"))
}

/// A session's transcript, found by its ID in whichever project it's in.
pub fn transcript(session: &str) -> Option<PathBuf> {
    if session.is_empty() || session.contains(['/', '.']) {
        return None;
    }
    let name = format!("{session}.jsonl");
    fs::read_dir(projects()?)
        .ok()?
        .flatten()
        .map(|project| project.path().join(&name))
        .find(|path| path.is_file())
}

/// What a session used from `since` on, in milliseconds: its transcript,
/// plus each one in the `subagents` folder beside it.
pub fn measure(transcript: &Path, since: i64) -> Result<Usage> {
    let mut calls = HashMap::new();
    read(transcript, since, false, &mut calls)
        .with_context(|| format!("reading {}", transcript.display()))?;
    let mut usage = Usage::default();
    if let Ok(entries) = fs::read_dir(transcript.with_extension("").join("subagents")) {
        for path in entries.flatten().map(|e| e.path()) {
            if path.extension().is_some_and(|x| x == "jsonl")
                && read(&path, since, true, &mut calls).unwrap_or(0) > 0
            {
                usage.subagent_count += 1;
            }
        }
    }
    for (subagent, model, tokens) in calls.into_values() {
        if subagent {
            usage.subagents.add(tokens);
        } else {
            usage.session.add(tokens);
        }
        usage.models.insert(model);
    }
    Ok(usage)
}

/// A subagent whose transcript hasn't changed for this long has stopped,
/// even if nothing says it finished: its session was killed, say.
const STALE_MS: i64 = 60 * 60 * 1000;

/// A session's subagents, from the `.meta.json` Claude Code keeps for each.
#[derive(Debug, Default)]
pub struct Subagents {
    /// Not finished, and active since `since`.
    pub running: usize,
    /// The tool calls that started each one, finished or not.
    pub started_by: HashSet<String>,
}

pub fn subagents(transcript: &Path, since: i64, now: i64) -> Subagents {
    let mut found = Subagents::default();
    let Ok(entries) = fs::read_dir(transcript.with_extension("").join("subagents")) else {
        return found;
    };
    let modified = |path: &Path| {
        fs::metadata(path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_millis() as i64)
    };
    for meta in entries.flatten().map(|e| e.path()) {
        let Some(stem) = meta
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.strip_suffix(".meta.json"))
        else {
            continue;
        };
        let Ok(info) = fs::read(&meta).map(|b| serde_json::from_slice::<Value>(&b)) else {
            continue;
        };
        let info = info.unwrap_or_default();
        if let Some(id) = info["toolUseId"].as_str() {
            found.started_by.insert(id.to_string());
        }
        let last = modified(&meta).max(modified(&meta.with_file_name(format!("{stem}.jsonl"))));
        if info["finished"] != true && last >= since.max(now - STALE_MS) {
            found.running += 1;
        }
    }
    found
}

/// Adds a transcript's API responses to `calls`, by message ID, and returns
/// how many lines it found with usage in the window.
fn read(
    path: &Path,
    since: i64,
    subagent: bool,
    calls: &mut HashMap<String, (bool, String, Tokens)>,
) -> Result<usize> {
    let mut found = 0;
    for line in BufReader::new(File::open(path)?).split(b'\n') {
        let line = line?;
        // Most lines are something else, and parsing them all is slow.
        if !line.windows(8).any(|w| w == b"\"usage\":") {
            continue;
        }
        // The last line may still be being written.
        let Ok(entry) = serde_json::from_slice::<Value>(&line) else {
            continue;
        };
        let message = &entry["message"];
        let (Some(id), Some(model)) = (message["id"].as_str(), message["model"].as_str()) else {
            continue;
        };
        let in_window = entry["timestamp"]
            .as_str()
            .and_then(parse_rfc3339)
            .is_some_and(|t| t >= since);
        // Claude Code's own stand-ins for responses, such as an error.
        if entry["type"] != "assistant" || model == "<synthetic>" || !in_window {
            continue;
        }
        found += 1;
        let tokens = Tokens::from_usage(&message["usage"]);
        calls
            .entry(id.to_string())
            .and_modify(|(_, _, t)| *t = t.max(tokens))
            .or_insert((subagent, model.to_string(), tokens));
    }
    Ok(found)
}

/// `1,234,567`.
pub fn count(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// `249,732,674 tokens in 1,400 API responses, on claude-opus-5-5: …`
pub fn describe(u: &Usage) -> String {
    let all = u.total();
    if all.responses == 0 {
        return "none recorded.".into();
    }
    let models: Vec<&str> = u.models.iter().map(String::as_str).collect();
    let mut out = format!(
        "{} tokens in {} API response{}, on {}: {} input, {} output, {} cache writes and {} \
         cache reads.",
        count(all.total()),
        count(all.responses),
        if all.responses == 1 { "" } else { "s" },
        models.join(", "),
        count(all.input),
        count(all.output),
        count(all.cache_writes),
        count(all.cache_reads),
    );
    match u.subagent_count {
        0 => out.push_str(" No subagents used any."),
        n => out.push_str(&format!(
            " The session itself used {}, and its {n} subagent{} {}.",
            count(u.session.total()),
            if n == 1 { "" } else { "s" },
            count(u.subagents.total()),
        )),
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::new_id;

    fn line(id: &str, time: &str, model: &str, output: u64) -> String {
        serde_json::json!({
            "type": "assistant", "timestamp": time,
            "message": {"id": id, "model": model, "usage": {
                "input_tokens": 2, "output_tokens": output,
                "cache_creation_input_tokens": 100, "cache_read_input_tokens": 1000,
            }},
        })
        .to_string()
    }

    #[test]
    fn counts_each_response_once_within_the_window() {
        let dir = env::temp_dir().join(format!("tailbus-usage-{}", new_id()));
        let subagents = dir.join("s1/subagents");
        fs::create_dir_all(&subagents).unwrap();
        let opus = "claude-opus-5-5";
        let main = [
            // Before the task: left out.
            line("m0", "2026-10-27T17:00:00Z", opus, 50),
            // Written once per content block, the second time complete.
            line("m1", "2026-10-27T18:00:00.100Z", opus, 10),
            line("m1", "2026-10-27T18:00:00.200Z", opus, 40),
            line("m2", "2026-10-27T18:05:00Z", "<synthetic>", 0),
            r#"{"type":"user","message":{"role":"user","content":"hi"}}"#.into(),
            r#"{"type":"assistant","timestamp":"2026-10-27T18:06:00Z","message":{"usage":"#.into(),
        ];
        fs::write(dir.join("s1.jsonl"), main.join("\n")).unwrap();
        fs::write(
            subagents.join("agent-a.jsonl"),
            line("a1", "2026-10-27T19:00:00Z", opus, 500),
        )
        .unwrap();
        fs::write(
            subagents.join("agent-b.jsonl"),
            line("b1", "2026-10-27T17:30:00Z", opus, 900),
        )
        .unwrap();
        fs::write(subagents.join("agent-a.meta.json"), "{}").unwrap();

        let since = parse_rfc3339("2026-10-27T17:51:33Z").unwrap();
        let usage = measure(&dir.join("s1.jsonl"), since).unwrap();
        fs::remove_dir_all(&dir).unwrap();

        let session = Tokens {
            input: 2,
            output: 40,
            cache_writes: 100,
            cache_reads: 1000,
            responses: 1,
        };
        assert_eq!(usage.session, session);
        assert_eq!(usage.subagents.output, 500);
        assert_eq!(usage.subagent_count, 1);
        assert_eq!(usage.total().total(), 1142 + 1602);
        assert_eq!(
            describe(&usage),
            "2,744 tokens in 2 API responses, on claude-opus-5-5: 4 input, 540 output, \
             200 cache writes and 2,000 cache reads. The session itself used 1,142, and its \
             1 subagent 1,602."
        );
        assert_eq!(describe(&Usage::default()), "none recorded.");
    }

    #[test]
    fn running_subagents_are_the_unfinished_recent_ones() {
        let dir = env::temp_dir().join(format!("tailbus-subagents-{}", new_id()));
        let folder = dir.join("s1/subagents");
        fs::create_dir_all(&folder).unwrap();
        let transcript = dir.join("s1.jsonl");
        let meta = |name: &str, json: &str| {
            fs::write(folder.join(format!("{name}.meta.json")), json).unwrap();
        };
        meta("agent-a", r#"{"toolUseId":"toolu_a","finished":true}"#);
        meta("agent-b", r#"{"toolUseId":"toolu_b"}"#);
        fs::write(folder.join("agent-b.jsonl"), "").unwrap();
        meta("agent-c", r#"{"toolUseId":"toolu_c","finished":false}"#);
        meta("agent-d", "not yet written");

        let now = crate::util::now_ms();
        let found = subagents(&transcript, now - 60_000, now);
        assert_eq!(found.running, 3);
        assert_eq!(found.started_by.len(), 3);
        // Started before the task, or quiet for an hour: not running.
        assert_eq!(subagents(&transcript, now + 60_000, now).running, 0);
        assert_eq!(
            subagents(&transcript, 0, now + STALE_MS + 60_000).running,
            0
        );
        fs::remove_dir_all(&dir).unwrap();
        assert_eq!(subagents(&transcript, 0, now).running, 0);
    }

    #[test]
    fn transcripts_are_found_by_session_id_only() {
        assert_eq!(transcript(""), None);
        assert_eq!(transcript("../x"), None);
        assert_eq!(count(0), "0");
        assert_eq!(count(999), "999");
        assert_eq!(count(249_732_674), "249,732,674");
    }
}
