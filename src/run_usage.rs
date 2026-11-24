//! What a bridge's agent says its model calls used. Some agents' CLIs
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

use crate::proto::{RunUsage, TurnUsage};

/// The most files a run's usage is read from, so a session full of files
/// can't hold up its reply.
const MOST_FILES: usize = 1000;
/// The most files a run says it read.
const MOST_OPENED: usize = 500;
/// The most tools a run's usage names, and the longest name it keeps.
const MOST_TOOL_NAMES: usize = 200;
const MOST_TOOL_NAME: usize = 120;

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
    /// What it says the fresh input cost, and the input read from a cache,
    /// for an agent that splits its cost.
    #[serde(default)]
    pub usd_input: Option<String>,
    #[serde(default)]
    pub usd_cache_read: Option<String>,
    pub model: Option<String>,
    /// Where a counted line names the files its call read.
    pub opened: Option<OpenedFrom>,
}

/// `[bridges.<agent>.usage.opened]`: the blocks of a counted line that are
/// tool calls reading a file, and where in each the file is.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OpenedFrom {
    /// A dotted path, in a counted line, to the array of its blocks.
    pub items: String,
    /// A block counts when each path here, within it, has its value.
    #[serde(default)]
    pub when: BTreeMap<String, String>,
    /// The path, within a block, to its tool's name.
    pub name: String,
    /// The tools that read a file.
    pub names: Vec<String>,
    /// The path, within a block, to the file it read.
    pub path: String,
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
            &self.usd_input,
            &self.usd_cache_read,
            &self.model,
        ];
        let mut named: Vec<&String> = paths
            .into_iter()
            .flatten()
            .chain(self.when.keys())
            .collect();
        if let Some(o) = &self.opened {
            if o.names.is_empty() {
                bail!("usage.opened needs at least one tool in names");
            }
            named.extend([&o.items, &o.name, &o.path]);
            named.extend(o.when.keys());
        }
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
        // In name order, so the first call is the main session's first.
        let mut files = files_under(dir, &matcher);
        files.sort();
        for file in files {
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
            // The prompt the session's last turn sent: what its next turn
            // should read back from the cache.
            let mut last_prompt: Option<u64> = None;
            for line in BufReader::new(f).split(b'\n').map_while(Result::ok) {
                let Ok(record) = serde_json::from_slice::<Value>(&line) else {
                    // Half a line from an agent that was stopped, or not JSON.
                    continue;
                };
                if !self.counts(&record) {
                    continue;
                }
                self.note_opened(&mut usage, &record);
                // A turn streamed more than once is counted once, at the end;
                // a session's turns, in order, say what each should have read
                // back.
                if self.dedupe.is_none() {
                    let turn = self.turn(&record);
                    if let Some(last) = last_prompt {
                        usage.cache_shortfall_tokens += last
                            .min(turn.prompt())
                            .saturating_sub(turn.cache_read_tokens);
                    }
                    last_prompt = Some(turn.prompt());
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
                    None => {
                        if usage.first_turn.is_none() {
                            usage.first_turn = Some(self.turn(&record));
                        }
                        priced &= self.add(&mut usage, &record);
                    }
                }
            }
        }
        if usage.first_turn.is_none()
            && let Some((_, record)) = named.first()
        {
            usage.first_turn = Some(self.turn(record));
        }
        for (_, record) in &named {
            priced &= self.add(&mut usage, record);
        }
        if usage.calls == 0 {
            return None;
        }
        usage.counts_tools = self.opened.is_some();
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

    /// The files a counted line's tool calls read, added to `usage`'s, each
    /// once and in the order first read.
    fn note_opened(&self, usage: &mut RunUsage, record: &Value) {
        let Some(o) = &self.opened else {
            return;
        };
        let Some(items) = at(record, &o.items).and_then(Value::as_array) else {
            return;
        };
        for item in items {
            let wanted = o.when.iter().all(|(path, want)| {
                at(item, path)
                    .and_then(Value::as_str)
                    .is_some_and(|v| v == want)
            });
            let reads = at(item, &o.name)
                .and_then(Value::as_str)
                .is_some_and(|n| o.names.iter().any(|t| t == n));
            let Some(file) = at(item, &o.path).and_then(Value::as_str) else {
                continue;
            };
            let file = file.trim();
            if !wanted || !reads || file.is_empty() || usage.opened.len() >= MOST_OPENED {
                continue;
            }
            if !usage.opened.iter().any(|f| f == file) {
                usage.opened.push(file.to_string());
            }
        }
    }

    /// How many tool calls a counted line made, whatever the tools: the
    /// blocks `usage.opened` describes, by its `when`.
    fn tool_calls_in(&self, record: &Value) -> u64 {
        let Some(o) = &self.opened else {
            return 0;
        };
        let Some(items) = at(record, &o.items).and_then(Value::as_array) else {
            return 0;
        };
        items
            .iter()
            .filter(|item| {
                o.when.iter().all(|(path, want)| {
                    at(item, path)
                        .and_then(Value::as_str)
                        .is_some_and(|v| v == want)
                })
            })
            .count() as u64
    }

    /// The tools a counted line's tool calls name, each once per call.
    fn tool_names_in(&self, record: &Value) -> Vec<String> {
        let Some(o) = &self.opened else {
            return Vec::new();
        };
        let Some(items) = at(record, &o.items).and_then(Value::as_array) else {
            return Vec::new();
        };
        items
            .iter()
            .filter(|item| {
                o.when.iter().all(|(path, want)| {
                    at(item, path)
                        .and_then(Value::as_str)
                        .is_some_and(|v| v == want)
                })
            })
            .filter_map(|item| at(item, &o.name).and_then(Value::as_str))
            .map(|name| name.chars().take(MOST_TOOL_NAME).collect())
            .collect()
    }

    /// One call's tokens.
    fn turn(&self, record: &Value) -> TurnUsage {
        let tokens = |path: &Option<String>| -> u64 {
            path.as_deref()
                .and_then(|p| at(record, p))
                .and_then(number)
                .map_or(0, |n| n.max(0.0).round() as u64)
        };
        TurnUsage {
            input_tokens: tokens(&self.input),
            output_tokens: tokens(&self.output),
            cache_read_tokens: tokens(&self.cache_read),
            cache_write_tokens: tokens(&self.cache_write),
        }
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
        usage.tool_calls += self.tool_calls_in(record);
        for name in self.tool_names_in(record) {
            if usage.tools.len() < MOST_TOOL_NAMES || usage.tools.contains_key(&name) {
                *usage.tools.entry(name).or_default() += 1;
            }
        }
        let dollars = |path: &Option<String>| {
            path.as_deref()
                .and_then(|p| at(record, p))
                .and_then(number)
                .filter(|n| n.is_finite() && *n >= 0.0)
        };
        if let Some(d) = dollars(&self.usd_input) {
            *usage.usd_input.get_or_insert(0.0) += d;
        }
        if let Some(d) = dollars(&self.usd_cache_read) {
            *usage.usd_cache_read.get_or_insert(0.0) += d;
        }
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

    /// The shape one agent CLI writes, one line per model call.
    fn jsonl() -> UsageFrom {
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

    #[test]
    fn what_a_turn_billed_fresh_that_the_last_one_sent_is_its_cache_shortfall() {
        let mut from = jsonl();
        from.usd_input = Some("message.usage.cost.input".into());
        from.usd_cache_read = Some("message.usage.cost.cacheRead".into());
        from.opened = Some(OpenedFrom {
            items: "message.content".into(),
            when: [("type".to_string(), "toolCall".to_string())].into(),
            name: "name".into(),
            names: vec!["read".into()],
            path: "arguments.path".into(),
        });
        let d = dir();
        let turn = |input: u64, cached: u64, tools: usize| {
            let blocks: Vec<String> = (0..tools)
                .map(|i| {
                    format!(r#"{{"type":"toolCall","name":"read","arguments":{{"path":"f{i}"}}}}"#)
                })
                .collect();
            format!(
                r#"{{"type":"message","message":{{"role":"assistant","model":"m","usage":{{"input":{input},"output":10,"cacheRead":{cached},"cacheWrite":0,"cost":{{"total":0.01,"input":{fi},"cacheRead":{fc}}}}},"content":[{blocks}]}}}}"#,
                fi = input as f64 * 1e-6,
                fc = cached as f64 * 1e-7,
                blocks = blocks.join(",")
            )
        };
        // Prompts 1000, 1500 (all of the last read back), 1800 (none of it:
        // another server), 2000 (part of it).
        let lines = [
            turn(1000, 0, 2),
            turn(500, 1000, 3),
            turn(1800, 0, 0),
            turn(1000, 1000, 1),
        ];
        fs::write(d.join("a.jsonl"), lines.join("\n") + "\n").unwrap();
        let u = from.read(&d, &Before::new()).unwrap();
        // 0, then 1500 - 0, then 1800 - 1000.
        assert_eq!(u.cache_shortfall_tokens, 1500 + 800);
        assert_eq!(u.tool_calls, 6);
        assert!(
            u.counts_tools,
            "its tool calls are counted, so 0 would mean none"
        );
        // And by name, each as often as it was asked for.
        assert_eq!(u.tools, [("read".to_string(), 6)].into());
        assert!((u.usd_input.unwrap() - 4300.0 * 1e-6).abs() < 1e-12);
        assert!((u.usd_cache_read.unwrap() - 2000.0 * 1e-7).abs() < 1e-12);
        // A new file is a new session: its first turn has no last one.
        fs::write(d.join("b.jsonl"), turn(3000, 0, 0) + "\n").unwrap();
        let both = from.read(&d, &Before::new()).unwrap();
        assert_eq!(both.cache_shortfall_tokens, 2300);
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
        let u = jsonl().read(&d, &Before::new()).unwrap();
        assert_eq!(u.calls, 2, "the user's line isn't a call");
        assert_eq!(u.input_tokens, 2821);
        assert_eq!(u.output_tokens, 66);
        assert_eq!(u.cache_read_tokens, 2000);
        assert_eq!(u.reasoning_tokens, Some(6));
        assert!((u.usd.unwrap() - 0.0006).abs() < 1e-12);
        assert_eq!(u.models, ["m/flash"]);
        fs::remove_dir_all(&d).unwrap();
    }

    /// The kinds' own defaults, as each bridge kind ships them.
    fn kind_usage(toml_text: &str) -> UsageFrom {
        let v: toml::Value = toml::from_str(toml_text).unwrap();
        let u: UsageFrom = v["usage"].clone().try_into().unwrap();
        u.check().unwrap();
        u
    }

    #[test]
    fn a_codesmith_run_counts_its_steps_not_its_tool_calls() {
        let d = dir();
        append(
            &d.join("usage.jsonl"),
            &[
                r#"{"type":"step","session":"s","model":"p/m","reason":"tool-calls","input":1200,"output":40,"cache_read":800,"cache_write":100,"reasoning":12,"usd":0.002,"time":1}"#.to_string(),
                r#"{"type":"tool","session":"s","name":"read","path":"src/a.rs"}"#.to_string(),
                r#"{"type":"step","session":"s","model":"p/m","reason":"stop","input":300,"output":60,"cache_read":2000,"cache_write":0,"reasoning":0,"time":2}"#.to_string(),
            ],
        );
        let u = kind_usage(include_str!("bridges/codesmith.toml"))
            .read(&d, &Before::new())
            .unwrap();
        assert_eq!(u.calls, 2, "a tool line isn't a model call");
        assert_eq!(u.input_tokens, 1500);
        assert_eq!(u.output_tokens, 100);
        assert_eq!(u.cache_read_tokens, 2800);
        assert_eq!(u.reasoning_tokens, Some(12));
        assert_eq!(u.models, ["p/m"]);
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn the_files_a_pi_call_read_are_named_once_in_the_order_first_read() {
        let d = dir();
        let read = |path: &str| {
            format!(
                r#"{{"type":"toolCall","id":"c","name":"read","arguments":{{"path":"{path}"}}}}"#
            )
        };
        let line = |blocks: &[String]| {
            format!(
                r#"{{"type":"message","message":{{"role":"assistant","model":"m","usage":{{"input":10,"output":1,"cacheRead":0,"cacheWrite":0,"cost":{{"total":0.1}}}},"content":[{}]}}}}"#,
                blocks.join(",")
            )
        };
        append(
            &d.join("s.jsonl"),
            &[
                line(&[
                    r#"{"type":"text","text":"reading"}"#.to_string(),
                    read("src/a.rs"),
                    read("/copies/r1/repo/src/b.rs"),
                    r#"{"type":"toolCall","id":"g","name":"grep","arguments":{"path":"src","pattern":"x"}}"#.to_string(),
                ]),
                line(&[read("src/a.rs"), read("docs/c.md")]),
                // A tool result, not a call, names nothing read.
                r#"{"type":"message","message":{"role":"toolResult","toolName":"read","content":[{"type":"toolCall","name":"read","arguments":{"path":"x"}}]}}"#.to_string(),
            ],
        );
        for kind in [
            include_str!("bridges/pi.toml"),
            include_str!("bridges/omp.toml"),
        ] {
            let u = kind_usage(kind).read(&d, &Before::new()).unwrap();
            assert_eq!(
                u.opened,
                ["src/a.rs", "/copies/r1/repo/src/b.rs", "docs/c.md"],
                "reads only, each once, first read first"
            );
        }
        // Without `opened`, nothing is named.
        assert!(jsonl().read(&d, &Before::new()).unwrap().opened.is_empty());
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn the_files_a_claude_code_call_read_are_named_from_its_transcript() {
        let d = dir();
        let line = |id: &str, file: &str| {
            format!(
                r#"{{"type":"assistant","message":{{"id":"{id}","model":"claude","usage":{{"input_tokens":5,"output_tokens":1,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}},"content":[{{"type":"tool_use","id":"t","name":"Read","input":{{"file_path":"{file}"}}}},{{"type":"tool_use","id":"u","name":"Bash","input":{{"command":"ls"}}}}]}}}}"#
            )
        };
        append(
            &d.join("t.jsonl"),
            &[
                line("m1", "/repo/src/lib.rs"),
                // The same call written again as it streamed.
                line("m1", "/repo/src/lib.rs"),
                line("m2", "/repo/tests/t.rs"),
            ],
        );
        let u = kind_usage(include_str!("bridges/claude.toml"))
            .read(&d, &Before::new())
            .unwrap();
        assert_eq!(u.opened, ["/repo/src/lib.rs", "/repo/tests/t.rs"]);
        assert_eq!(u.calls, 2, "the streamed call counts once");
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn an_opened_setting_must_name_its_tools_and_paths() {
        let bad: UsageFrom = toml::from_str(
            r#"
            files = "*.jsonl"
            input = "u.in"
            opened = { items = "content", name = "name", names = [], path = "arguments.path" }
            "#,
        )
        .unwrap();
        assert!(bad.check().unwrap_err().to_string().contains("names"));
        let dotted: UsageFrom = toml::from_str(
            r#"
            files = "*.jsonl"
            input = "u.in"
            opened = { items = "content", name = "name", names = ["read"], path = "arguments..path" }
            "#,
        )
        .unwrap();
        assert!(dotted.check().is_err());
    }

    #[test]
    fn the_first_call_is_kept_apart_to_show_what_its_first_turn_read() {
        // pi's normalized usage through a gateway: the first turn wrote the
        // prefix, the second read it, in whole blocks.
        let d = dir();
        append(
            &d.join("session.jsonl"),
            &[call(13644, 200, 0, 0.002), call(1356, 150, 12288, 0.001)],
        );
        let u = jsonl().read(&d, &Before::new()).unwrap();
        assert_eq!(u.calls, 2);
        assert_eq!((u.input_tokens, u.cache_read_tokens), (15_000, 12_288));
        let first = u.first_turn.expect("the first call, on its own");
        assert_eq!(
            (
                first.input_tokens,
                first.cache_read_tokens,
                first.output_tokens
            ),
            (13_644, 0, 200)
        );
        assert_eq!(first.prompt(), 13_644);
        // A later session file doesn't displace the first one's first call:
        // files are read in name order.
        let e = dir();
        append(&e.join("b.jsonl"), &[call(9, 9, 9, 0.1)]);
        append(&e.join("a.jsonl"), &[call(205, 104, 37_376, 0.001)]);
        let v = jsonl().read(&e, &Before::new()).unwrap();
        assert_eq!(v.first_turn.unwrap().cache_read_tokens, 37_376);
        fs::remove_dir_all(&d).unwrap();
        fs::remove_dir_all(&e).unwrap();
    }

    #[test]
    fn a_kept_session_counts_only_what_the_run_added() {
        let d = dir();
        let session = d.join("s.jsonl");
        append(&session, &[call(1000, 10, 0, 0.1)]);
        let before = jsonl().before(&d);
        append(&session, &[call(7, 3, 0, 0.2)]);
        // A subagent's own file, new in this run.
        fs::create_dir_all(d.join("sub")).unwrap();
        append(&d.join("sub/t.jsonl"), &[call(5, 5, 0, 0.3)]);
        let u = jsonl().read(&d, &before).unwrap();
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
        let u = jsonl().read(&d, &Before::new()).unwrap();
        assert_eq!(u.calls, 2);
        assert_eq!(u.usd, None);
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn nothing_found_is_none_and_bad_lines_are_skipped() {
        let d = dir();
        assert_eq!(jsonl().read(&d, &Before::new()), None);
        append(&d.join("other.txt"), &[call(1, 1, 0, 0.1)]);
        append(&d.join("s.jsonl"), &["{not json".to_string()]);
        let mut half = call(9, 9, 0, 0.1);
        half.truncate(30);
        fs::write(d.join("h.jsonl"), half).unwrap();
        assert_eq!(jsonl().read(&d, &Before::new()), None);
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn never_follows_a_link_out_of_the_session() {
        let d = dir();
        let outside = dir();
        append(&outside.join("x.jsonl"), &[call(1, 1, 0, 0.1)]);
        std::os::unix::fs::symlink(outside.join("x.jsonl"), d.join("x.jsonl")).unwrap();
        std::os::unix::fs::symlink(&outside, d.join("dir")).unwrap();
        assert_eq!(jsonl().read(&d, &Before::new()), None);
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
        // The first message's own figures, as last written.
        assert_eq!(u.first_turn.map(|f| f.output_tokens), Some(7));
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn a_bad_config_is_refused() {
        let mut u = jsonl();
        u.files = "[".into();
        assert!(u.check().is_err());
        let mut u = jsonl();
        u.input = None;
        u.output = None;
        assert!(u.check().is_err());
        let mut u = jsonl();
        u.usd = Some("message..cost".into());
        assert!(u.check().is_err());
        assert!(jsonl().check().is_ok());
    }
}
