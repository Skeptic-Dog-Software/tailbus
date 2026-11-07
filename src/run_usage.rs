//! What a bridge's agent says its model calls used. Agents such as omp
//! write a line of JSON to their session for each call, with its tokens and
//! often its price, so a bridge that's told where to look can report real
//! usage instead of a guess from the text.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use globset::{Glob, GlobMatcher};
use serde::Deserialize;
use serde_json::Value;

use crate::proto::RunUsage;

/// The most files a run's usage is read from, so a session full of files
/// can't hold up its reply.
const MOST_FILES: usize = 1000;

/// `[bridges.<agent>.usage]`: which files under a run's session directory
/// hold the agent's record, one JSON object a line, and where in each line
/// the numbers are. Paths into a line are dotted, as `message.usage.input`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UsageFrom {
    /// A pattern for the files, relative to the session directory.
    pub files: String,
    /// A line counts as a model call when each path here has its value.
    #[serde(default)]
    pub when: BTreeMap<String, String>,
    /// A path whose value names the call, for an agent that writes a call
    /// more than once, as it streams. The last line with each value counts.
    pub dedupe: Option<String>,
    /// Input that wasn't read from a cache.
    pub input: Option<String>,
    /// Output, thinking included.
    pub output: Option<String>,
    pub cache_read: Option<String>,
    pub cache_write: Option<String>,
    /// The thinking part of the output.
    pub reasoning: Option<String>,
    /// What the agent says the call cost, in US dollars.
    pub usd: Option<String>,
    pub model: Option<String>,
}

/// How long each file was before a run, so that a session kept between
/// runs counts only what the run adds.
pub type Before = BTreeMap<PathBuf, u64>;

impl UsageFrom {
    pub fn check(&self) -> Result<()> {
        self.matcher()?;
        if self.input.is_none() && self.output.is_none() {
            bail!("usage needs at least one of input and output");
        }
        let paths = [
            &self.dedupe,
            &self.input,
            &self.output,
            &self.cache_read,
            &self.cache_write,
            &self.reasoning,
            &self.usd,
            &self.model,
        ];
        let named = paths.into_iter().flatten().chain(self.when.keys());
        if let Some(bad) = named.into_iter().find(|p| p.split('.').any(str::is_empty)) {
            bail!("usage: {bad:?} isn't a dotted path");
        }
        Ok(())
    }

    fn matcher(&self) -> Result<GlobMatcher> {
        Ok(Glob::new(&self.files)
            .with_context(|| format!("usage.files: bad pattern {:?}", self.files))?
            .compile_matcher())
    }

    /// The files' lengths now.
    pub fn before(&self, dir: &Path) -> Before {
        let Ok(matcher) = self.matcher() else {
            return Before::new();
        };
        files_under(dir, &matcher)
            .into_iter()
            .filter_map(|f| Some((f.clone(), fs::metadata(&f).ok()?.len())))
            .collect()
    }

    /// What the lines added since `before` say. None if no line counted.
    pub fn read(&self, dir: &Path, before: &Before) -> Option<RunUsage> {
        let matcher = self.matcher().ok()?;
        let mut usage = RunUsage::default();
        let mut priced = true;
        // Calls an agent wrote more than once, by name, the latest kept.
        let mut named: Vec<(String, Value)> = Vec::new();
        for file in files_under(dir, &matcher) {
            let from = before.get(&file).copied().unwrap_or(0);
            let Ok(mut f) = File::open(&file) else {
                continue;
            };
            // A file that got shorter was rewritten, so all of it is new.
            let len = f.metadata().map_or(0, |m| m.len());
            let from = if len < from { 0 } else { from };
            if f.seek(SeekFrom::Start(from)).is_err() {
                continue;
            }
            for line in BufReader::new(f).split(b'\n').map_while(Result::ok) {
                let Ok(record) = serde_json::from_slice::<Value>(&line) else {
                    // Half a line from an agent that was stopped, or not JSON.
                    continue;
                };
                if !self.counts(&record) {
                    continue;
                }
                let name = self
                    .dedupe
                    .as_deref()
                    .and_then(|p| at(&record, p))
                    .map(|v| v.to_string());
                match name {
                    Some(name) => match named.iter_mut().find(|(n, _)| *n == name) {
                        Some(seen) => seen.1 = record,
                        None => named.push((name, record)),
                    },
                    None => priced &= self.add(&mut usage, &record),
                }
            }
        }
        for (_, record) in &named {
            priced &= self.add(&mut usage, record);
        }
        if usage.calls == 0 {
            return None;
        }
        if !priced {
            usage.usd = None;
        }
        Some(usage)
    }

    fn counts(&self, record: &Value) -> bool {
        self.when.iter().all(|(path, want)| {
            at(record, path).is_some_and(|v| match v {
                Value::String(s) => s == want,
                // A number or a boolean, written as it would be in JSON.
                other => serde_json::from_str::<Value>(want).is_ok_and(|w| w == *other),
            })
        })
    }

    /// Adds one call. False if it had no price.
    fn add(&self, usage: &mut RunUsage, record: &Value) -> bool {
        let tokens = |path: &Option<String>| -> u64 {
            path.as_deref()
                .and_then(|p| at(record, p))
                .and_then(number)
                .map_or(0, |n| n.max(0.0).round() as u64)
        };
        usage.calls += 1;
        usage.input_tokens += tokens(&self.input);
        usage.output_tokens += tokens(&self.output);
        usage.cache_read_tokens += tokens(&self.cache_read);
        usage.cache_write_tokens += tokens(&self.cache_write);
        if self.reasoning.is_some() {
            *usage.reasoning_tokens.get_or_insert(0) += tokens(&self.reasoning);
        }
        if let Some(model) = self
            .model
            .as_deref()
            .and_then(|p| at(record, p))
            .and_then(Value::as_str)
            && !usage.models.iter().any(|m| m == model)
        {
            usage.models.push(model.to_string());
        }
        let usd = self
            .usd
            .as_deref()
            .and_then(|p| at(record, p))
            .and_then(number)
            .filter(|n| n.is_finite() && *n >= 0.0);
        match usd {
            Some(usd) => {
                *usage.usd.get_or_insert(0.0) += usd;
                true
            }
            None => false,
        }
    }
}

fn at<'a>(record: &'a Value, path: &str) -> Option<&'a Value> {
    path.split('.').try_fold(record, |v, key| v.get(key))
}

fn number(v: &Value) -> Option<f64> {
    v.as_f64()
}

/// The regular files under `dir` whose path from it matches, never
/// following a link the agent might have left there.
fn files_under(dir: &Path, matcher: &GlobMatcher) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut dirs = vec![dir.to_path_buf()];
    while let Some(d) = dirs.pop() {
        let Ok(entries) = fs::read_dir(&d) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            if kind.is_dir() {
                dirs.push(path);
            } else if kind.is_file()
                && path
                    .strip_prefix(dir)
                    .is_ok_and(|rel| matcher.is_match(rel))
            {
                found.push(path);
                if found.len() == MOST_FILES {
                    return found;
                }
            }
        }
    }
    found.sort();
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// The shape omp 18.17.0 writes, one line per model call.
    fn omp() -> UsageFrom {
        toml::from_str(
            r#"
            files = "**/*.jsonl"
            when = { type = "message", "message.role" = "assistant" }
            input = "message.usage.input"
            output = "message.usage.output"
            cache_read = "message.usage.cacheRead"
            cache_write = "message.usage.cacheWrite"
            reasoning = "message.usage.reasoningTokens"
            usd = "message.usage.cost.total"
            model = "message.model"
            "#,
        )
        .unwrap()
    }

    fn call(input: u64, output: u64, cached: u64, usd: f64) -> String {
        format!(
            r#"{{"type":"message","message":{{"role":"assistant","model":"m/flash","usage":{{"input":{input},"output":{output},"cacheRead":{cached},"cacheWrite":0,"reasoningTokens":3,"cost":{{"total":{usd}}}}}}}}}"#
        )
    }

    fn dir() -> PathBuf {
        let d = std::env::temp_dir().join(format!("tailbus-usage-{}", crate::util::new_id()));
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn append(path: &Path, lines: &[String]) {
        let mut f = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        for l in lines {
            writeln!(f, "{l}").unwrap();
        }
    }

    #[test]
    fn counts_the_calls_an_agent_recorded() {
        let d = dir();
        let session = d.join("2026-11-07_x.jsonl");
        append(
            &session,
            &[
                r#"{"type":"session","id":"x"}"#.to_string(),
                r#"{"type":"message","message":{"role":"user","usage":{"input":999}}}"#.to_string(),
                call(2721, 16, 0, 0.0004),
                call(100, 50, 2000, 0.0002),
            ],
        );
        let u = omp().read(&d, &Before::new()).unwrap();
        assert_eq!(u.calls, 2, "the user's line isn't a call");
        assert_eq!(u.input_tokens, 2821);
        assert_eq!(u.output_tokens, 66);
        assert_eq!(u.cache_read_tokens, 2000);
        assert_eq!(u.reasoning_tokens, Some(6));
        assert!((u.usd.unwrap() - 0.0006).abs() < 1e-12);
        assert_eq!(u.models, ["m/flash"]);
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn a_kept_session_counts_only_what_the_run_added() {
        let d = dir();
        let session = d.join("s.jsonl");
        append(&session, &[call(1000, 10, 0, 0.1)]);
        let before = omp().before(&d);
        append(&session, &[call(7, 3, 0, 0.2)]);
        // A subagent's own file, new in this run.
        fs::create_dir_all(d.join("sub")).unwrap();
        append(&d.join("sub/t.jsonl"), &[call(5, 5, 0, 0.3)]);
        let u = omp().read(&d, &before).unwrap();
        assert_eq!((u.calls, u.input_tokens, u.output_tokens), (2, 12, 8));
        assert!((u.usd.unwrap() - 0.5).abs() < 1e-12);
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn a_call_without_a_price_leaves_the_total_unknown() {
        let d = dir();
        let unpriced =
            r#"{"type":"message","message":{"role":"assistant","usage":{"input":5,"output":5}}}"#;
        append(
            &d.join("s.jsonl"),
            &[call(1, 1, 0, 0.1), unpriced.to_string()],
        );
        let u = omp().read(&d, &Before::new()).unwrap();
        assert_eq!(u.calls, 2);
        assert_eq!(u.usd, None);
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn nothing_found_is_none_and_bad_lines_are_skipped() {
        let d = dir();
        assert_eq!(omp().read(&d, &Before::new()), None);
        append(&d.join("other.txt"), &[call(1, 1, 0, 0.1)]);
        append(&d.join("s.jsonl"), &["{not json".to_string()]);
        let mut half = call(9, 9, 0, 0.1);
        half.truncate(30);
        fs::write(d.join("h.jsonl"), half).unwrap();
        assert_eq!(omp().read(&d, &Before::new()), None);
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn never_follows_a_link_out_of_the_session() {
        let d = dir();
        let outside = dir();
        append(&outside.join("x.jsonl"), &[call(1, 1, 0, 0.1)]);
        std::os::unix::fs::symlink(outside.join("x.jsonl"), d.join("x.jsonl")).unwrap();
        std::os::unix::fs::symlink(&outside, d.join("dir")).unwrap();
        assert_eq!(omp().read(&d, &Before::new()), None);
        fs::remove_dir_all(&d).unwrap();
        fs::remove_dir_all(&outside).unwrap();
    }

    #[test]
    fn a_call_written_twice_counts_once() {
        // Claude Code's shape: a line for each part of a streamed response.
        let claude: UsageFrom = toml::from_str(
            r#"
            files = "**/*.jsonl"
            when = { type = "assistant" }
            dedupe = "message.id"
            input = "message.usage.input_tokens"
            output = "message.usage.output_tokens"
            cache_read = "message.usage.cache_read_input_tokens"
            cache_write = "message.usage.cache_creation_input_tokens"
            model = "message.model"
            "#,
        )
        .unwrap();
        let line = |id: &str, out: u64| {
            format!(
                r#"{{"type":"assistant","message":{{"id":"{id}","model":"m","usage":{{"input_tokens":10,"output_tokens":{out},"cache_read_input_tokens":100,"cache_creation_input_tokens":5}}}}}}"#
            )
        };
        let d = dir();
        append(
            &d.join("s.jsonl"),
            &[
                line("a", 1),
                line("a", 7),
                line("b", 3),
                r#"{"type":"user"}"#.into(),
            ],
        );
        let u = claude.read(&d, &Before::new()).unwrap();
        assert_eq!(u.calls, 2);
        assert_eq!(
            (u.input_tokens, u.output_tokens),
            (20, 10),
            "a's last line counts"
        );
        assert_eq!((u.cache_read_tokens, u.cache_write_tokens), (200, 10));
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn a_bad_config_is_refused() {
        let mut u = omp();
        u.files = "[".into();
        assert!(u.check().is_err());
        let mut u = omp();
        u.input = None;
        u.output = None;
        assert!(u.check().is_err());
        let mut u = omp();
        u.usd = Some("message..cost".into());
        assert!(u.check().is_err());
        assert!(omp().check().is_ok());
    }
}
