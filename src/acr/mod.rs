//! `tailbus acr`: adversarial convergence review for any repository. The
//! framework holds no knowledge of a repository. Roles, tiers, lenses and
//! bindings all come from config, in `config`.

mod change;
mod config;
mod flow;
mod generated;
mod keep;
mod plan;
mod report;
mod roles;
mod stages;
mod trial;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, Subcommand};

use self::change::Change;
use self::config::{Config, Flags, HumanGate, REPO_CONFIG, ReviewScope, Role, SCHEMA};
use self::flow::{Answer, Call, Caller, Decision, Known, Run, Setting, Status};
use crate::attach::Attachments;
use crate::config::{Config as BusConfig, Paths};
use crate::local::{Awaited, Client, Request, Sent};
use crate::proto::{Extras, MAX_BODY, RunOptions, RunStatus};
use crate::util::new_id;

/// The bus agent reviews are sent as, always: a review's traffic is its
/// own, and never lands in a session's inbox.
const SENDER: &str = "acr";
/// How long a review waits for each reply. It counts time queued behind
/// the bridge's other runs, so it's long: the bridge's cap on the run
/// itself is what stops one that runs away.
const REPLY_WAIT: u64 = 24 * 3600;
/// `run` and `resume` exit with these for a review that needs a person.
const WAITING: u8 = 3;
const ESCALATED: u8 = 4;

#[derive(Args, Clone)]
pub struct Target {
    /// The repository.
    #[arg(long, default_value = ".")]
    repo: PathBuf,
    /// The branch the change goes into. A config in the repository is read
    /// from here, never from the change.
    #[arg(long)]
    base: String,
    /// The repository's config, kept outside it. Left out, it's
    /// `~/.config/tailbus/acr/<repository>.yaml` if that's there, or else the
    /// repository's own `.acr/config.yaml`, from `--base`.
    #[arg(long, value_name = "FILE")]
    config: Option<PathBuf>,
    /// The change's head. Left out, the working tree.
    #[arg(long)]
    head: Option<String>,
    #[arg(long, value_parser = parse_gate)]
    human_gate: Option<HumanGate>,
    #[arg(long)]
    max_rounds: Option<u32>,
    #[arg(long, value_parser = parse_scope)]
    review_scope: Option<ReviewScope>,
    /// Use a binding for a role, as `role=name`. Can be repeated.
    #[arg(long = "binding", value_name = "ROLE=NAME", value_parser = parse_binding)]
    bindings: Vec<(Role, String)>,
    /// What the change says it's for, in a file, such as its pull request's
    /// description. Every role gets it, with the commit messages, ahead of
    /// the diff.
    #[arg(long, value_name = "FILE", conflicts_with = "pr")]
    description: Option<PathBuf>,
    /// The pull request whose title and description say what the change is
    /// for, read with `gh`. `--post-pr` reads its own when this is left out.
    #[arg(long, value_name = "NUMBER")]
    pr: Option<u64>,
    /// The building session's brief for round 1's reviewers: a YAML file
    /// with a `scope` statement, and `lenses`, each lens's where and how
    /// to attack. See docs/acr-repo-setup.md.
    #[arg(long, value_name = "FILE")]
    brief: Option<PathBuf>,
}

/// What the change says it's for: `--description`'s file, or a pull
/// request's title and description. None, if neither is given: then the
/// roles go by the commit messages and the change's own comments.
fn description_of(target: &Target, post_pr: Option<u64>) -> Result<Option<String>> {
    if let Some(file) = &target.description {
        let text = fs::read_to_string(file)
            .with_context(|| format!("reading the description, {}", file.display()))?;
        return Ok(Some(text));
    }
    let Some(pr) = target.pr.or(post_pr) else {
        return Ok(None);
    };
    let out = Command::new("gh")
        .args(["pr", "view", &pr.to_string(), "--json", "title,body"])
        .args(["--jq", r##""# " + .title + "\n\n" + (.body // "")"##])
        .current_dir(&target.repo)
        .stdin(Stdio::null())
        .output()
        .context("running gh, to read the pull request's description")?;
    if !out.status.success() {
        bail!(
            "couldn't read pull request #{pr}'s description with gh: {}. Pass \
             --description <file> instead.",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(Some(
        String::from_utf8_lossy(&out.stdout).trim().to_string(),
    ))
}

fn parse_gate(s: &str) -> Result<HumanGate, String> {
    serde_yaml_ng::from_str(s).map_err(|_| "use per-round, ends or exception".to_string())
}

fn parse_scope(s: &str) -> Result<ReviewScope, String> {
    serde_yaml_ng::from_str(s).map_err(|_| "use whole-pr or bottom-of-stack".to_string())
}

fn parse_binding(s: &str) -> Result<(Role, String), String> {
    let (role, name) = s.split_once('=').ok_or("use role=name")?;
    let role = serde_yaml_ng::from_str(role).map_err(|_| format!("{role} isn't a role"))?;
    Ok((role, name.to_string()))
}

#[derive(Subcommand)]
pub enum AcrCmd {
    /// Check a repository's config against the schema and its rules.
    Validate {
        /// The config file. Left out, the repository's, from `--base`.
        file: Option<PathBuf>,
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        #[arg(long)]
        base: Option<String>,
    },
    /// Print the published schema.
    Schema,
    /// How the pipeline's Judge rates what each key's own review found, set
    /// against how that review rated it: blockers agreed on, missed, and
    /// overrated. Runs no reviewer: one Judge call for each dimension.
    Calibrate {
        keys: Vec<PathBuf>,
        /// Append each key's result to this file, as a line of JSON.
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Score saved trials of new work, once its own review has filled in the
    /// key's findings.
    Rescore {
        key: PathBuf,
        /// The results file the trials were written to.
        #[arg(long)]
        results: PathBuf,
    },
    /// Measure review designs on a change, blind, with nobody in the loop,
    /// leaving nothing behind. Prints each design's result as a line of
    /// JSON, scored against the key's findings when it has them.
    Trial {
        /// The answer key: the change, what it's for, and what an earlier
        /// review found. For new work, leave the findings out: the trials
        /// are saved unscored, for `rescore` once its own review is done.
        key: PathBuf,
        /// One design. Left out, the three that review one round, one after
        /// another; `converge` runs only when named.
        #[arg(long, value_enum)]
        arm: Option<trial::Arm>,
        /// Add each result, as a line of JSON, to this file too.
        #[arg(long, value_name = "FILE")]
        out: Option<PathBuf>,
    },
    /// Check that a trial on this key can run, before spending one on it.
    /// A session is started as a builder is, and one through each bridge and
    /// set of tools the separated design uses: each must start, read the
    /// trial's copy, fail to read a file outside it, and run a command where
    /// it has a shell. A few small calls.
    Check {
        /// The answer key, as for `trial`.
        key: PathBuf,
    },
    /// Allow ACR to review a repository on this machine.
    AddRepo {
        /// The repository's top folder.
        path: PathBuf,
        /// The bridge ACR reaches its models through here. Not `agent`, which
        /// the global `--as` and `TAILBUS_AGENT` would fill.
        #[arg(long, default_value = crate::setup::ACR_AGENT)]
        bridge: String,
    },
    /// Stop allowing ACR to review a repository on this machine.
    RemoveRepo {
        path: PathBuf,
        #[arg(long, default_value = crate::setup::ACR_AGENT)]
        bridge: String,
    },
    /// Show the tier, lenses, bindings and cost, with no model calls.
    Plan {
        #[command(flatten)]
        target: Target,
        #[arg(long)]
        json: bool,
    },
    /// Review a change.
    Run {
        #[command(flatten)]
        target: Target,
        /// Put the result on this pull request, if the repository turned
        /// that on.
        #[arg(long, value_name = "NUMBER")]
        post_pr: Option<u64>,
    },
    /// Carry on with a review that's waiting for a person.
    Resume {
        run_id: String,
        /// Blockers to fix, by ID, with commas.
        #[arg(long, value_delimiter = ',')]
        fold: Vec<String>,
        /// Blockers to waive, by ID, with commas.
        #[arg(long, value_delimiter = ',')]
        waive: Vec<String>,
        /// Go on past an escalation, or approve the scope or the result.
        #[arg(long)]
        approve: bool,
        /// End the review here.
        #[arg(long)]
        stop: bool,
        #[arg(long, value_name = "NUMBER")]
        post_pr: Option<u64>,
    },
    /// Score a binding set against a commit with known blockers.
    Benchmark {
        #[command(flatten)]
        target: Target,
        /// A YAML list of the blockers it holds: id, pattern and maybe file.
        #[arg(long)]
        known: PathBuf,
    },
}

fn user_config_path(paths: &Paths) -> PathBuf {
    paths.config.with_file_name("acr.yaml")
}

fn run_dir(paths: &Paths, id: &str) -> PathBuf {
    keep::dir(paths).join(id)
}

fn flags(t: &Target) -> Flags {
    Flags {
        human_gate: t.human_gate,
        max_rounds: t.max_rounds,
        review_scope: t.review_scope,
        bindings: t.bindings.iter().cloned().collect(),
    }
}

/// Where the repository's layer came from.
#[derive(Debug)]
enum Source {
    /// A file outside the repository, whose `not_findings` is beside it.
    File(PathBuf),
    /// The repository's own, from its base.
    Base,
}

/// The repository's layer: a file outside it, if one's named or kept in
/// `acr/`, or else its own, from `base`. Either way nothing in the change can
/// alter it.
fn repo_layer(
    paths: &Paths,
    repo: &Path,
    base: Option<&str>,
    file: Option<&Path>,
) -> Result<(String, Source)> {
    let kept = match file {
        Some(file) => Some(file.to_path_buf()),
        None => {
            let path = outside_config(paths, repo)?;
            path.exists().then_some(path)
        }
    };
    if let Some(file) = kept {
        let text =
            fs::read_to_string(&file).with_context(|| format!("reading {}", file.display()))?;
        return Ok((text, Source::File(file)));
    }
    let base =
        base.with_context(|| format!("give a config file, or --base to read {REPO_CONFIG} from"))?;
    Ok((change::repo_config(repo, base)?, Source::Base))
}

/// Where a repository's config is kept outside it.
fn outside_config(paths: &Paths, repo: &Path) -> Result<PathBuf> {
    let dir = paths.config.with_file_name("acr");
    Ok(dir.join(format!("{}.yaml", change::name(repo)?)))
}

/// The config for a review of a change into `base`.
fn load_config(paths: &Paths, t: &Target, flags: &Flags) -> Result<(Config, Source)> {
    let user = config::load_user(&user_config_path(paths))?;
    let (text, source) = repo_layer(paths, &t.repo, Some(&t.base), t.config.as_deref())?;
    let mut cfg = config::resolve(&user, config::parse_repo(&text)?, flags)?;
    cfg.outside = matches!(source, Source::File(_));
    Ok((cfg, source))
}

/// The decided non-issues: beside an outside config, or on the base.
fn not_findings(cfg: &Config, source: &Source, repo: &Path, base: &str) -> Result<String> {
    let Some(path) = cfg.not_findings.as_deref() else {
        return Ok(String::new());
    };
    match source {
        Source::File(file) => {
            let dir = file.parent().unwrap_or(Path::new("."));
            let path = dir.join(path);
            fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))
        }
        Source::Base => flow::from_base(repo, base, Some(path)),
    }
}

/// This machine's name on the tailnet, as the bus knows it.
fn this_machine(paths: &Paths) -> Result<String> {
    let status: crate::local::Status = crate::local::call(&paths.socket(), &Request::Status)
        .context("asking the bus which machine this is")?;
    status
        .machine
        .context("the bus doesn't know this machine's name yet")
}

fn reload(cfg: &Config, repo: &Path, base: &str, head: Option<&str>) -> Result<Change> {
    change::load(repo, base, head, cfg.review_scope, &cfg.generated)
}

/// Reaches models through bridges, as `tailbus call` does. Each call is a
/// fresh session, limited to its binding's tools.
struct BridgeCaller {
    socket: PathBuf,
    agent: String,
    store: Attachments,
    /// Attachments this put in the store, or got back there, to remove when
    /// it's done: a review leaves nothing behind.
    made: std::sync::Mutex<Vec<String>>,
    /// Asks each bridge to keep its run's log and transcripts, which a
    /// review otherwise leaves none of: for a trial, to see what each
    /// reviewer did.
    keep_logs: bool,
}

impl Drop for BridgeCaller {
    fn drop(&mut self) {
        let made = std::mem::take(&mut *self.made.lock().unwrap_or_else(|e| e.into_inner()));
        for sha in made {
            let _ = fs::remove_file(self.store.path(&sha));
        }
    }
}

impl BridgeCaller {
    fn new(paths: &Paths) -> Result<BridgeCaller> {
        let bus = BusConfig::load(&paths.config)?;
        Ok(BridgeCaller {
            socket: paths.socket(),
            // Its own name, never a session's: what it sends and gets back
            // is the review's, and no session should see it or act on it.
            agent: SENDER.to_string(),
            store: Attachments::new(
                paths.attachments(),
                bus.max_attachment_bytes,
                bus.attachment_store_bytes,
            ),
            made: std::sync::Mutex::default(),
            keep_logs: false,
        })
    }

    /// Adds an attachment, noting it to remove later if it wasn't there
    /// already, since something else might need one that was.
    fn attach(&self, name: &str, bytes: &[u8]) -> Result<crate::proto::Attachment> {
        use sha2::{Digest, Sha256};
        let sha: String = Sha256::digest(bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let there = self.store.path(&sha).exists();
        let a = self.store.add(name, &mut &bytes[..], bytes.len() as u64)?;
        if !there {
            self.noted(&a.sha256);
        }
        Ok(a)
    }

    /// Waits for the reply to `id` again, on a new connection to the daemon,
    /// giving it a minute to come back.
    fn await_again(&self, id: &str, wait: u64) -> Result<Awaited> {
        let until = std::time::Instant::now() + std::time::Duration::from_secs(60);
        loop {
            let tried = Client::connect(&self.socket).and_then(|mut client| {
                client.send(&Request::Await {
                    agent: self.agent.clone(),
                    id: id.to_string(),
                    wait_secs: wait,
                })?;
                client.reply::<Awaited>()
            });
            match tried {
                Ok(awaited) => return Ok(awaited),
                Err(e) if std::time::Instant::now() >= until => {
                    return Err(e).context("the tailbus daemon went away mid-call");
                }
                Err(_) => std::thread::sleep(std::time::Duration::from_secs(1)),
            }
        }
    }

    fn noted(&self, sha: &str) {
        let mut made = self.made.lock().unwrap_or_else(|e| e.into_inner());
        made.push(sha.to_string());
    }
}

impl Caller for BridgeCaller {
    fn fingerprint(&self, repo: &Path) -> Option<String> {
        change::fingerprint(repo).ok()
    }

    fn snapshot(&self, repo: &Path) -> Option<String> {
        change::worktree_tree(repo).ok()
    }

    fn delta(&self, repo: &Path, from: &str, to: &str) -> Option<(Vec<String>, String)> {
        change::tree_diff(repo, from, to).ok()
    }

    fn call(&self, call: &Call) -> Result<Answer> {
        let b = call.binding;
        // Fixes only from a binding the user lets write.
        if call.writes && !b.writes {
            bail!(
                "the {} binding can't apply fixes: give it `writes: true` in your acr.yaml",
                call.binding_name
            );
        }
        let options = RunOptions {
            // The binding's tools, writing ones too: some headless agents
            // may use only the tools they're named, and refuse a write they
            // would otherwise have to ask about. Without
            // a list, an Orchestrator's fixes get what its bridge gives.
            tools: b.tools.clone(),
            session: Some("fresh".into()),
            timeout: b.timeout,
            // Every role works in the repository, so reviewers read the
            // code itself, read-only, and an Orchestrator fixes it there.
            workdir: Some(call.repo.display().to_string()),
            model: b.model.clone(),
            effort: b.effort.clone(),
            // A review keeps its own record, and leaves nothing behind,
            // unless it's a trial's, kept to see what each agent did.
            log: Some(self.keep_logs),
            // And runs on one machine, which the bridge checks too.
            review: Some(true),
        };
        let mut attachments = Vec::new();
        if let Some(path) = call.artifact {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "change.diff".into());
            let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
            attachments.push(self.attach(&name, &bytes)?);
        }
        let mut body = call.prompt.to_string();
        if body.len() > MAX_BODY {
            attachments.push(self.attach("brief.txt", body.as_bytes())?);
            body = "Your brief is in the attached file brief.txt.".to_string();
        }
        let ext = Extras {
            options: Some(options),
            attachments,
            ..Extras::default()
        };
        let wait = REPLY_WAIT;
        let asked = std::time::Instant::now();
        let mut client = Client::connect(&self.socket)?;
        client.send(&Request::Call {
            agent: self.agent.clone(),
            to: b.agent.clone(),
            body,
            reply_to: None,
            expects_reply: Some(true),
            ext: Some(Box::new(ext)),
            wait_secs: Some(wait),
        })?;
        let sent: Sent = client.reply()?;
        let awaited = match client.reply::<Awaited>() {
            Ok(awaited) => awaited,
            // The daemon went away mid-wait, as it does when it's updated.
            // The reply is kept for this agent, so wait for it again.
            Err(_) => {
                let left = wait.saturating_sub(asked.elapsed().as_secs()).max(1);
                self.await_again(&sent.id, left)?
            }
        };
        match awaited {
            Awaited::Reply { item } => {
                let msg = &item.msg;
                let result = msg.result();
                let status = result.map_or(RunStatus::Ok, |r| r.status);
                if status != RunStatus::Ok {
                    bail!("{} {}: {}", b.agent, status.as_str(), msg.body);
                }
                let whole = result
                    .and_then(|r| r.full_reply.as_ref())
                    .and_then(|sha| msg.attachments().iter().find(|a| &a.sha256 == sha));
                let text = match whole.and_then(|a| a.path.as_ref()) {
                    Some(path) => fs::read_to_string(path)?,
                    None => msg.body.clone(),
                };
                // The bridge made it for this reply alone.
                if let Some(a) = whole {
                    self.noted(&a.sha256);
                }
                Ok(Answer {
                    text,
                    run_id: sent.id,
                    usage: result.and_then(|r| r.usage.clone()),
                })
            }
            Awaited::Undelivered { why } => bail!("{}: {why}", b.agent),
            Awaited::Unanswered => bail!("{} marked it handled without answering", b.agent),
            Awaited::TimedOut => bail!("{} didn't answer in {wait} seconds", b.agent),
        }
    }
}

fn save(paths: &Paths, run: &Run, cfg: &Config) -> Result<()> {
    let dir = run_dir(paths, &run.id);
    fs::create_dir_all(&dir)?;
    fs::write(dir.join("run.json"), serde_json::to_vec_pretty(run)?)?;
    fs::write(dir.join("config.json"), serde_json::to_vec_pretty(cfg)?)?;
    let report = report::report(run, cfg);
    fs::write(dir.join("report.json"), serde_json::to_vec_pretty(&report)?)?;
    Ok(())
}

/// Puts a result on a pull request, as the repository's config says.
fn post(run: &Run, cfg: &Config, pr: u64) -> Result<()> {
    let number = pr.to_string();
    let wanted = report::labels(run, cfg);
    let stale: Vec<String> = report::all_labels(run, cfg)
        .into_iter()
        .filter(|l| !wanted.contains(l))
        .collect();
    let gh = |args: &[&str], input: Option<&str>| -> Result<()> {
        let mut command = Command::new("gh");
        command.args(args).current_dir(&run.repo);
        command.stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        });
        let mut child = command.spawn().context("running gh")?;
        if let (Some(text), Some(mut stdin)) = (input, child.stdin.take()) {
            use std::io::Write;
            stdin.write_all(text.as_bytes())?;
        }
        if !child.wait()?.success() {
            bail!("gh {} failed", args.join(" "));
        }
        Ok(())
    };
    if !wanted.is_empty() {
        let (add, remove) = (wanted.join(","), stale.join(","));
        let mut args = vec!["pr", "edit", number.as_str(), "--add-label", add.as_str()];
        if !stale.is_empty() {
            args.extend(["--remove-label", remove.as_str()]);
        }
        gh(&args, None)?;
    }
    if let Some(text) = report::comment(run, cfg) {
        gh(&["pr", "comment", &number, "--body-file", "-"], Some(&text))?;
    }
    Ok(())
}

/// Prints a run's report, and says what to do next.
fn finish(paths: &Paths, run: &Run, cfg: &Config, pr: Option<u64>) -> Result<ExitCode> {
    save(paths, run, cfg)?;
    keep::finished(paths, run, true)?;
    println!(
        "{}",
        serde_json::to_string_pretty(&report::report(run, cfg))?
    );
    if let Some(pr) = pr {
        post(run, cfg, pr)?;
    }
    Ok(match run.status {
        Status::Confirmed => ExitCode::SUCCESS,
        Status::Waiting => {
            let code = match run.waiting {
                Some(flow::Gate::Escalation) => ESCALATED,
                _ => WAITING,
            };
            eprintln!("tailbus: waiting for you: tailbus acr resume {}", run.id);
            ExitCode::from(code)
        }
        Status::Running | Status::Stopped => ExitCode::FAILURE,
    })
}

/// Where reviewers that can change files get their copies of the checkout.
fn copies_root(paths: &Paths) -> PathBuf {
    paths.state.join("acr-copies")
}

/// The bridges that reviewers who can change files go through: each must
/// let them work in the copies.
fn copy_bridges(cfg: &Config) -> Result<Vec<String>> {
    let mut bridges = Vec::new();
    for lens in &cfg.lenses {
        let b = cfg.binding(Role::Reviewer, Some(lens))?;
        let bridge = b.agent.split('@').next().unwrap_or(&b.agent).to_string();
        if b.changes_files() && !bridges.contains(&bridge) {
            bridges.push(bridge);
        }
    }
    Ok(bridges)
}

/// Lets a bridge work in any trial's copy, which needs a restart only the
/// first time: after that, a trial's copy is already allowed, and no trial
/// restarts a bridge that other agents' runs are going through.
fn allow_trials(paths: &Paths, bridge: &str) -> Result<()> {
    crate::setup::acr_dir(paths, &paths.state.join("acr-trials"), bridge)?;
    Ok(())
}

/// Lets those bridges work in the copies, which needs a bridge restart only
/// the first time.
fn allow_copies(paths: &Paths, cfg: &Config) -> Result<()> {
    for bridge in copy_bridges(cfg)? {
        crate::setup::acr_dir(paths, &copies_root(paths), &bridge)?;
    }
    Ok(())
}

/// What a run holds to, kept with it: its non-issues are read once, at the
/// start, so resuming can't pick up a list that's changed since.
fn setting<'a>(
    paths: &Paths,
    cfg: &'a Config,
    run_id: &str,
    list: Option<String>,
) -> Result<Setting<'a>> {
    let dir = run_dir(paths, run_id);
    let kept = dir.join("not-findings.md");
    let not_findings = match list {
        Some(text) => {
            fs::create_dir_all(&dir)?;
            fs::write(&kept, &text)?;
            text
        }
        None => fs::read_to_string(&kept).unwrap_or_default(),
    };
    Ok(Setting {
        cfg,
        not_findings,
        work_dir: dir,
        description: None,
        converge: false,
        copies: Some(copies_root(paths)),
        builder_brief: None,
    })
}

/// What earlier reviews of `repo` say, from ACR's history.
fn history(paths: &Paths, repo: &Path) -> plan::History {
    plan::History::of(keep::history(paths, repo))
}

/// 1234567 as 1,234,567.
fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, d) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(d);
    }
    out
}

fn describe_plan(plan: &plan::Plan) {
    let how = if plan.tier_decided {
        "from its paths"
    } else {
        "assumed: the Sizer decides on a real run, and this is the highest it could be"
    };
    println!(
        "Tier: {} ({how}), {} reviewer(s) per lens",
        plan.tier, plan.fan_out
    );
    println!("Files: {}", plan.files);
    if !plan.generated.is_empty() {
        let added: u64 = plan.generated.iter().map(|g| g.added).sum();
        let removed: u64 = plan.generated.iter().map(|g| g.removed).sum();
        println!(
            "Generated, left out of what reviewers get: {} file(s), +{}/-{} lines",
            plan.generated.len(),
            thousands(added),
            thousands(removed)
        );
        for g in &plan.generated {
            println!("  {} (+{} -{}): {}", g.path, g.added, g.removed, g.why);
        }
    }
    println!("Lenses:");
    for lens in &plan.lenses {
        println!("  {} on {} ({:?})", lens.id, lens.binding, lens.why);
    }
    let tokens = |cost: &plan::RoleCost| {
        let usd = match (cost.subscription, cost.usd) {
            (true, _) => "subscription".to_string(),
            (false, Some(u)) => format!("${u:.4}"),
            (false, None) => "no price".to_string(),
        };
        let cached = match cost.cache_read {
            0 => String::new(),
            n => format!(", {} read from cache,", thousands(n)),
        };
        format!(
            "about {} tokens in{cached} and {} out, {usd}",
            thousands(cost.tokens_in),
            thousands(cost.tokens_out)
        )
    };
    println!("First round:");
    for cost in &plan.first_round {
        println!(
            "  {} on {}: {} call(s), {}",
            cost.role.as_str(),
            cost.binding,
            cost.calls,
            tokens(cost)
        );
    }
    if let Some(fix) = &plan.fix {
        println!(
            "Each fix between rounds: orchestrator on {}, {}",
            fix.binding,
            tokens(fix)
        );
    }
    // Tokens always. Dollars only for calls billed in them.
    let figure = |e: &plan::Estimate| {
        let usd = match (plan.dollars, e.usd) {
            (false, _) => String::new(),
            (true, Some(u)) => format!(" (${u:.2})"),
            (true, None) => " (dollars: a binding has no price)".to_string(),
        };
        format!("about {} tokens{usd}", thousands(e.tokens))
    };
    println!("Tokens:");
    println!(
        "  At least, with every lens done in one round: {}",
        figure(&plan.least)
    );
    match &plan.expected {
        None => {
            println!("  Expected: can't say yet, with no finished reviews of this repository here")
        }
        Some(e) => println!(
            "  Expected, from the last {} finished review(s) here: {}",
            plan.history_runs,
            figure(e)
        ),
    }
    println!(
        "  At most, with every lens through every round and a fix between: {}",
        figure(&plan.most)
    );
    if !plan.unpriced.is_empty() {
        println!(
            "No price for {}: add it under costs.prices, or mark its binding `subscription: true`",
            plan.unpriced.join(", ")
        );
    }
    if plan.per_call.is_empty() {
        println!(
            "These count only the text ACR sends and reads back. What an agent's tools read and \
             what it thinks add more: set costs.per_call to count them."
        );
    }
    for (role, extra) in &plan.per_call {
        println!(
            "Each {} call adds {} tokens in, {} read from cache and {} out, by costs.per_call",
            role.as_str(),
            extra.input_tokens,
            extra.cache_read_tokens,
            extra.output_tokens
        );
    }
    for e in &plan.escalates {
        println!("Escalates: {e}");
    }
}

/// How many reviews are under way on this machine now.
pub fn active_reviews(paths: &Paths) -> usize {
    keep::active(paths).len()
}

/// Checks the text of a user's ACR config, as `tailbus setup acr` writes it.
pub fn check_user_config(text: &str) -> Result<()> {
    config::parse_user(text).map(|_| ())
}

pub fn run(paths: &Paths, command: AcrCmd) -> Result<ExitCode> {
    keep::prune(paths);
    match command {
        AcrCmd::Schema => print!("{SCHEMA}"),
        AcrCmd::AddRepo { path, bridge } => crate::setup::acr_repo(paths, &path, true, &bridge)?,
        AcrCmd::RemoveRepo { path, bridge } => {
            crate::setup::acr_repo(paths, &path, false, &bridge)?
        }
        AcrCmd::Validate { file, repo, base } => {
            let user = config::load_user(&user_config_path(paths))?;
            let (text, source) = repo_layer(paths, &repo, base.as_deref(), file.as_deref())?;
            let cfg = config::resolve(&user, config::parse_repo(&text)?, &Flags::default())?;
            let from = match &source {
                Source::File(f) => f.display().to_string(),
                Source::Base => format!("{REPO_CONFIG} on the base"),
            };
            println!(
                "Valid: {} tiers, {} lenses, from {from}.",
                cfg.tiers.len(),
                cfg.lenses.len()
            );
            if cfg.allowed_bindings.is_empty() {
                println!(
                    "Bindings not checked: there's no {}.",
                    user_config_path(paths).display()
                );
            }
            match this_machine(paths) {
                Ok(machine) => cfg.check_local(&machine)?,
                Err(e) => println!("Not checked that every agent is on this machine: {e:#}."),
            }
        }
        AcrCmd::Plan { target, json } => {
            let (cfg, source) = load_config(paths, &target, &flags(&target))?;
            cfg.check_local(&this_machine(paths)?)?;
            let change = reload(&cfg, &target.repo, &target.base, target.head.as_deref())?;
            let plan = plan::dry_run(&cfg, &change, &history(paths, &target.repo))?;
            if !json && let Source::File(file) = &source {
                println!("Config: {}", file.display());
            }
            if json {
                println!("{}", serde_json::to_string_pretty(&plan)?);
            } else {
                describe_plan(&plan);
            }
        }
        AcrCmd::Run { target, post_pr } => {
            let (cfg, source) = load_config(paths, &target, &flags(&target))?;
            cfg.check_bindings()?;
            cfg.check_local(&this_machine(paths)?)?;
            change::checked_out(&target.repo, target.head.as_deref())?;
            allow_copies(paths, &cfg)?;
            let description = description_of(&target, post_pr)?;
            let id = new_id();
            let list = not_findings(&cfg, &source, &target.repo, &target.base)?;
            let mut s = setting(paths, &cfg, &id, Some(list))?;
            s.description = description;
            s.builder_brief = target
                .brief
                .as_deref()
                .map(stages::read_builder_brief)
                .transpose()?;
            // Under way until this ends, so an update to tailbus waits for it.
            let _active = keep::active_guard(paths, &id)?;
            let (repo, base, head) = (&target.repo, target.base.as_str(), target.head.as_deref());
            let change = reload(&cfg, repo, base, head)?;
            let caller = BridgeCaller::new(paths)?;
            let mut run = flow::start(&id, repo, base, head, &s, &change, &caller)?;
            // After a fix, the change is the working tree.
            let reloading = |fixed: bool| reload(&cfg, repo, base, head.filter(|_| !fixed));
            save(paths, &run, &cfg)?;
            flow::drive(&mut run, &s, &caller, &reloading)?;
            return finish(paths, &run, &cfg, post_pr);
        }
        AcrCmd::Resume {
            run_id,
            fold,
            waive,
            approve,
            stop,
            post_pr,
        } => {
            let dir = run_dir(paths, &run_id);
            let read =
                |name: &str| fs::read(dir.join(name)).with_context(|| format!("no run {run_id}"));
            let mut run: Run = serde_json::from_slice(&read("run.json")?)?;
            let _active = keep::active_guard(paths, &run_id)?;
            let cfg: Config = stored_config(&read("config.json")?)?;
            cfg.check_local(&this_machine(paths)?)?;
            // Until a fix, the review is of its head, which must be checked out.
            if run.lenses.values().all(|l| l.done.is_empty()) {
                change::checked_out(&run.repo, run.head.as_deref())?;
            }
            let decision = Decision {
                fold,
                waive,
                approve,
                stop,
            };
            flow::decide(&mut run, &decision)?;
            let mut s = setting(paths, &cfg, &run_id, None)?;
            s.description = run.description.clone();
            let (repo, base) = (run.repo.clone(), run.base.clone());
            let caller = BridgeCaller::new(paths)?;
            let head = run.head.clone();
            let reloading =
                |fixed: bool| reload(&cfg, &repo, &base, head.as_deref().filter(|_| !fixed));
            flow::drive(&mut run, &s, &caller, &reloading)?;
            return finish(paths, &run, &cfg, post_pr);
        }
        AcrCmd::Benchmark { target, known } => {
            let known: Vec<Known> = serde_yaml_ng::from_str(
                &fs::read_to_string(&known)
                    .with_context(|| format!("reading {}", known.display()))?,
            )
            .context("in the known blockers")?;
            let description = description_of(&target, None)?;
            let run = one_round(paths, &target, description)?;
            let score = flow::score(&run, &known)?;
            println!("{}", serde_json::to_string_pretty(&score)?);
        }
        AcrCmd::Calibrate { keys, out } => {
            for key in keys {
                let result = calibrate(paths, &key)?;
                println!(
                    "{}: blockers agreed {}, missed {}, overrated {}; non-blockers agreed {}; unrated {}",
                    result["key"].as_str().unwrap_or_default(),
                    result["agreed_blockers"],
                    result["missed_blockers"],
                    result["overrated"],
                    result["agreed_non_blockers"],
                    result["unrated"],
                );
                if let Some(out) = &out {
                    let mut file = fs::OpenOptions::new().create(true).append(true).open(out)?;
                    std::io::Write::write_all(&mut file, format!("{result}\n").as_bytes())?;
                }
            }
        }
        AcrCmd::Rescore { key, results } => {
            let n = trial::rescore(paths, &key, &results)?;
            println!(
                "Scored {n} trial(s) of {} in {}.",
                key.display(),
                results.display()
            );
        }
        AcrCmd::Trial { key, arm, out } => {
            let arms = match arm {
                Some(arm) => vec![arm],
                None => vec![
                    trial::Arm::OneSize,
                    trial::Arm::Sized,
                    trial::Arm::Separated,
                ],
            };
            let pipeline = |tree: &Path, key: &trial::Key, arm| pipeline_arm(paths, tree, key, arm);
            for arm in arms {
                let outcome = trial::run(paths, &key, arm, &pipeline)?;
                let line = serde_json::to_string(&outcome)?;
                println!("{line}");
                if let Some(out) = &out {
                    let mut file = fs::OpenOptions::new().create(true).append(true).open(out)?;
                    std::io::Write::write_all(&mut file, format!("{line}\n").as_bytes())?;
                }
            }
        }
        AcrCmd::Check { key } => {
            if !check(paths, &key)? {
                return Ok(ExitCode::FAILURE);
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// `tailbus acr check`: prints how each session did. Returns whether all did
/// as they should.
fn check(paths: &Paths, key_path: &Path) -> Result<bool> {
    let (key, s) = trial::copy(paths, trial::resolved(trial::read_key(key_path)?)?)?;
    let canary = trial::Canary::new(paths)?;
    let mut ok = true;
    let mut report = |what: &str, started: std::time::Instant, wrong: Result<Vec<String>>| {
        let secs = started.elapsed().as_secs();
        match wrong {
            Ok(wrong) if wrong.is_empty() => println!("{what}: ok, in {secs} s"),
            Ok(wrong) => {
                ok = false;
                println!("{what}: wrong, in {secs} s");
                for w in wrong {
                    println!("  {w}");
                }
            }
            Err(e) => {
                ok = false;
                println!("{what}: failed after {secs} s: {e:#}");
            }
        }
    };
    let started = std::time::Instant::now();
    let wrong = trial::check_session(paths, &s, &key, &canary);
    report("a builder's session", started, wrong);

    // The separated design's bindings: one call for each bridge and set of
    // tools, since that's what decides how a session starts.
    let target = Target {
        repo: s.tree.clone(),
        base: key.base.clone(),
        config: None,
        head: Some(key.head.clone()),
        human_gate: None,
        max_rounds: None,
        review_scope: None,
        bindings: Vec::new(),
        description: None,
        pr: None,
        brief: None,
    };
    let (cfg, _) = load_config(paths, &target, &flags(&target))?;
    let mut names: Vec<&String> = cfg
        .bindings
        .values()
        .chain(cfg.lenses.iter().filter_map(|l| l.binding.as_ref()))
        .collect();
    names.sort();
    names.dedup();
    // A reviewer that can change files works in a copy of its own, which
    // its bridge must let it into, so it's checked there.
    let reviewers: Vec<&str> = cfg
        .lenses
        .iter()
        .filter_map(|l| cfg.binding_name(Role::Reviewer, Some(l)).ok())
        .collect();
    let in_copy = |name: &str, b: &config::Binding| b.changes_files() && reviewers.contains(&name);
    let mut chosen: Vec<(&String, &config::Binding)> = Vec::new();
    for name in names {
        let Some(b) = cfg.allowed_bindings.get(name) else {
            continue;
        };
        if !chosen.iter().any(|(n, c)| {
            c.agent == b.agent && c.tools == b.tools && in_copy(n, c) == in_copy(name, b)
        }) {
            chosen.push((name, b));
        }
    }
    let mut allowed = AllowedFor {
        paths,
        tree: s.tree.clone(),
        bridges: Vec::new(),
    };
    for (_, b) in &chosen {
        let bridge = b.agent.split('@').next().unwrap_or(&b.agent).to_string();
        if !allowed.bridges.contains(&bridge) {
            allow_trials(paths, &bridge)?;
            crate::setup::acr_repo(paths, &s.tree, true, &bridge)?;
            allowed.bridges.push(bridge);
        }
    }
    allow_copies(paths, &cfg)?;
    let copies = copies_root(paths).join(format!("check-{}", new_id()));
    let caller = BridgeCaller::new(paths)?;
    for (name, b) in chosen {
        // Without a list, it gets whatever its bridge gives, a shell maybe.
        let bash = b
            .tools
            .as_ref()
            .is_none_or(|t| t.iter().any(|t| t == "Bash"));
        let binding = config::Binding {
            timeout: Some(trial::CHECK_SECS),
            ..b.clone()
        };
        let in_copy = in_copy(name, b);
        let tree = if in_copy {
            let copy = copies.join(s.tree.file_name().unwrap_or_default());
            change::copy_of(&s.tree, &copy)?;
            copy
        } else {
            s.tree.clone()
        };
        let prompt = trial::check_prompt(&tree, &canary.path, bash, None);
        let started = std::time::Instant::now();
        let answer = caller.call(&Call {
            role: Role::Reviewer,
            binding_name: name,
            binding: &binding,
            prompt: &prompt,
            artifact: None,
            writes: false,
            repo: &tree,
        });
        let wrong = answer.map(|a| trial::check_answer(&a.text, &key.head, &canary.secret, bash));
        let place = if in_copy {
            ", in a copy of its own"
        } else {
            ""
        };
        report(
            &format!("{name} through {}{place}", b.agent),
            started,
            wrong,
        );
    }
    let _ = fs::remove_dir_all(&copies);
    // A review to convergence's steelman, which writes in the copy.
    if let Some(b) = cfg.allowed_bindings.get(STEELMAN_BINDING) {
        let bridge = b.agent.split('@').next().unwrap_or(&b.agent).to_string();
        if !allowed.bridges.contains(&bridge) {
            allow_trials(paths, &bridge)?;
            crate::setup::acr_repo(paths, &s.tree, true, &bridge)?;
            allowed.bridges.push(bridge);
        }
        let binding = config::Binding {
            timeout: Some(trial::CHECK_SECS),
            ..b.clone()
        };
        let result = s.tree.join(".acr-check-steelman.json");
        let prompt = trial::check_prompt(&s.tree, &canary.path, true, Some(&result));
        let started = std::time::Instant::now();
        let answer = caller.call(&Call {
            role: Role::Orchestrator,
            binding_name: STEELMAN_BINDING,
            binding: &binding,
            prompt: &prompt,
            artifact: None,
            writes: true,
            repo: &s.tree,
        });
        let wrong = answer.map(|a| {
            let mut wrong = trial::check_answer(&a.text, &key.head, &canary.secret, true);
            if fs::read_to_string(&result).is_err() {
                wrong.push(format!("it didn't write {} in the copy", result.display()));
            }
            wrong
        });
        let _ = fs::remove_file(&result);
        report(
            &format!("{STEELMAN_BINDING} through {}, writing", b.agent),
            started,
            wrong,
        );
    }
    drop(allowed);
    if !ok && let Some(kept) = s.keep(paths) {
        println!(
            "What the builder's session left is kept in {}.",
            kept.display()
        );
    }
    Ok(ok)
}

/// The most rounds a review to convergence may take: a cap no review should
/// reach, so one that does is a review that isn't converging.
const CONVERGE_CAP: u32 = 20;

/// The Orchestrator's binding in a review to convergence. It must be able to
/// write, since its steelman folds findings into the change.
pub const STEELMAN_BINDING: &str = "steelman";

/// One round of the pipeline, as a change first comes out, with nobody
/// fixing anything and escalations taken as seen. Leaves nothing behind.
fn one_round(paths: &Paths, target: &Target, description: Option<String>) -> Result<Run> {
    pipeline_run(paths, target, description, false, false)
}

/// The pipeline with nobody in the loop: one round, or to convergence. An
/// escalation is recorded and taken as seen, and the review goes on.
fn pipeline_run(
    paths: &Paths,
    target: &Target,
    description: Option<String>,
    converge: bool,
    keep_logs: bool,
) -> Result<Run> {
    let mut flags = flags(target);
    flags.max_rounds = Some(if converge { CONVERGE_CAP } else { 1 });
    flags.human_gate = Some(HumanGate::Exception);
    let (cfg, source) = load_config(paths, target, &flags)?;
    cfg.check_bindings()?;
    cfg.check_local(&this_machine(paths)?)?;
    if converge {
        let name = cfg.binding_name(Role::Orchestrator, None)?;
        if !cfg.binding(Role::Orchestrator, None)?.writes {
            bail!(
                "a review to convergence needs an Orchestrator that can fold findings in, \
                 and {name} can't: give it `writes: true` in acr.yaml"
            );
        }
    }
    change::checked_out(&target.repo, target.head.as_deref())?;
    allow_copies(paths, &cfg)?;
    let id = new_id();
    let list = not_findings(&cfg, &source, &target.repo, &target.base)?;
    let mut s = setting(paths, &cfg, &id, Some(list))?;
    s.description = description;
    s.converge = converge;
    s.builder_brief = target
        .brief
        .as_deref()
        .map(stages::read_builder_brief)
        .transpose()?;
    // Under way until this ends, so an update to tailbus waits for it.
    let _active = keep::active_guard(paths, &id)?;
    let (repo, base, head) = (&target.repo, target.base.as_str(), target.head.as_deref());
    let change = reload(&cfg, repo, base, head)?;
    let mut caller = BridgeCaller::new(paths)?;
    caller.keep_logs = keep_logs;
    let mut run = flow::start(&id, repo, base, head, &s, &change, &caller)?;
    // After a steelman, the change is the working tree, with any file it
    // added in the diff too.
    let reloading = |fixed: bool| {
        if fixed {
            change::git(repo, &["add", "--intent-to-add", "--all"])?;
        }
        reload(&cfg, repo, base, head.filter(|_| !fixed))
    };
    loop {
        for e in &mut run.escalations {
            e.seen = true;
        }
        flow::drive(&mut run, &s, &caller, &reloading)?;
        match run.waiting {
            None => break,
            Some(flow::Gate::Escalation) => {}
            Some(gate) => bail!(
                "run {} is waiting at {gate:?}, which nobody here answers",
                run.id
            ),
        }
    }
    for e in &mut run.escalations {
        e.seen = true;
    }
    save(paths, &run, &cfg)?;
    // Forced through with nobody in the loop, it says nothing about how many
    // rounds reviews take.
    keep::finished(paths, &run, false)?;
    Ok(run)
}

/// Allows a trial's worktree through every local bridge its bindings use,
/// for as long as it lives.
struct AllowedFor<'a> {
    paths: &'a Paths,
    tree: PathBuf,
    bridges: Vec<String>,
}

impl Drop for AllowedFor<'_> {
    fn drop(&mut self) {
        for bridge in &self.bridges {
            let _ = crate::setup::acr_repo(self.paths, &self.tree, false, bridge);
        }
    }
}

/// What each lens means in the repository at `tree`, from its review config,
/// for a trial's builder, which is otherwise told only the dimensions. Empty
/// if the repository has none.
fn lens_criteria(paths: &Paths, tree: &Path, key: &trial::Key) -> Vec<(String, String)> {
    let target = Target {
        repo: tree.to_path_buf(),
        base: key.base.clone(),
        config: None,
        head: Some(key.head.clone()),
        human_gate: None,
        max_rounds: None,
        review_scope: None,
        bindings: Vec::new(),
        description: None,
        pr: None,
        brief: None,
    };
    match load_config(paths, &target, &flags(&target)) {
        Ok((cfg, _)) => cfg
            .lenses
            .iter()
            .map(|l| (l.id.clone(), l.criteria.clone()))
            .collect(),
        Err(e) => {
            eprintln!("tailbus: no review criteria for the builder: {e:#}");
            Vec::new()
        }
    }
}

/// The pipeline's designs on a trial's worktree: one round, or to
/// convergence.
fn pipeline_arm(
    paths: &Paths,
    tree: &Path,
    key: &trial::Key,
    arm: trial::Arm,
) -> Result<trial::Ran> {
    let converge = arm == trial::Arm::Converge;
    let target = Target {
        repo: tree.to_path_buf(),
        base: key.base.clone(),
        config: None,
        head: Some(key.head.clone()),
        human_gate: None,
        max_rounds: None,
        review_scope: None,
        bindings: if converge {
            vec![(Role::Orchestrator, STEELMAN_BINDING.to_string())]
        } else {
            Vec::new()
        },
        description: None,
        pr: None,
        brief: key.brief.clone(),
    };
    let (cfg, _) = load_config(paths, &target, &flags(&target))?;
    let mut bridges: Vec<String> = cfg
        .bindings
        .values()
        .filter_map(|name| cfg.allowed_bindings.get(name))
        .map(|b| b.agent.split('@').next().unwrap_or(&b.agent).to_string())
        .collect();
    bridges.sort();
    bridges.dedup();
    let mut allowed = AllowedFor {
        paths,
        tree: tree.to_path_buf(),
        bridges: Vec::new(),
    };
    for bridge in &bridges {
        allow_trials(paths, bridge)?;
        crate::setup::acr_repo(paths, tree, true, bridge)?;
        allowed.bridges.push(bridge.clone());
    }
    // The key's context, frozen before any review, stands in for the pull
    // request's description, which may have changed since.
    let run = pipeline_run(paths, &target, Some(key.context.clone()), converge, true)?;
    drop(allowed);
    let mut found = Vec::new();
    let mut failures: Vec<String> = run.escalations.iter().map(|e| e.reason.clone()).collect();
    let mut notes: Vec<String> = Vec::new();
    for lens in run.lenses.values() {
        for round in &lens.rounds {
            for failure in &round.failures {
                let failure = format!("{}, round {}: {failure}", lens.id, round.round);
                if !failures.iter().any(|f| f.contains(&failure)) {
                    failures.push(failure);
                }
            }
            for note in &round.notes {
                notes.push(format!("{}, round {}: {note}", lens.id, round.round));
            }
            for f in &round.findings {
                found.push(trial::Found {
                    dimension: lens.id.clone(),
                    title: f.title.clone(),
                    detail: f.detail.clone(),
                    file: f.file.clone().unwrap_or_default(),
                    blocker: f.blocks(),
                    round: round.round,
                });
            }
        }
    }
    let mut counts = trial::Counts::default();
    let mut models = Vec::new();
    let mut by_round: Vec<u64> = Vec::new();
    for u in &run.usage {
        counts.input += u.tokens_in;
        counts.cache_read += u.cache_read;
        counts.cache_write += u.cache_write;
        counts.output += u.tokens_out;
        if let Some(m) = &u.model
            && !models.contains(m)
        {
            models.push(m.clone());
        }
        let at = u.round.max(1) as usize - 1;
        if by_round.len() <= at {
            by_round.resize(at + 1, 0);
        }
        by_round[at] += u.tokens();
    }
    counts.total = counts.input + counts.output + counts.cache_read + counts.cache_write;
    let converged = if converge {
        // What the steelmans left, as a patch on the key's head.
        change::git(tree, &["add", "--all"])?;
        let patch = change::git(tree, &["diff", "--cached", "--binary", &key.head])?;
        let summary = trial::Converged {
            rounds: run.round,
            converged: run.status == flow::Status::Confirmed,
            run: run.id.clone(),
            patch: None,
            tokens_by_round: by_round,
            steelmans: run.steelmans.clone(),
            disagreements: run.disagreements.clone(),
        };
        Some((summary, patch))
    } else {
        None
    };
    Ok(trial::Ran {
        findings: found,
        tokens: counts,
        models,
        failures,
        converged,
        notes,
    })
}

/// `tailbus acr calibrate`, for one key: its own review's findings, each
/// tagged, go to the pipeline's Judge one dimension at a time, in a copy of
/// the change, and its ratings are set against the review's.
fn calibrate(paths: &Paths, key_path: &Path) -> Result<serde_json::Value> {
    use serde_json::json;
    let (key, s) = trial::copy(paths, trial::resolved(trial::read_key(key_path)?)?)?;
    if key.findings.is_empty() {
        bail!("{} has no findings to calibrate on", key_path.display());
    }
    let target = Target {
        repo: s.tree.clone(),
        base: key.base.clone(),
        config: None,
        head: Some(key.head.clone()),
        human_gate: None,
        max_rounds: None,
        review_scope: None,
        bindings: Vec::new(),
        description: None,
        pr: None,
        brief: None,
    };
    let (cfg, source) = load_config(paths, &target, &flags(&target))?;
    let judge = cfg.binding(Role::Judge, None)?;
    let bridge = judge
        .agent
        .split('@')
        .next()
        .unwrap_or(&judge.agent)
        .to_string();
    let mut allowed = AllowedFor {
        paths,
        tree: s.tree.clone(),
        bridges: Vec::new(),
    };
    allow_trials(paths, &bridge)?;
    crate::setup::acr_repo(paths, &s.tree, true, &bridge)?;
    allowed.bridges.push(bridge);
    let change = reload(&cfg, &s.tree, &key.base, Some(&key.head))?;
    let intent = change::intent(Some(&key.context), &change.commits);
    let artifact = s.dir.join("calibrate.diff");
    fs::write(&artifact, format!("{intent}# The diff\n\n{}", change.diff))?;
    let decided = not_findings(&cfg, &source, &s.tree, &key.base)?;
    let mut caller = BridgeCaller::new(paths)?;
    caller.keep_logs = true;
    let tag = regex::Regex::new(r"\[K(\d+)\]").expect("a fixed pattern");
    let mut judged: Vec<Option<(bool, String)>> = vec![None; key.findings.len()];
    let mut dimensions: Vec<&str> = key.findings.iter().map(|f| f.dimension.as_str()).collect();
    dimensions.sort();
    dimensions.dedup();
    for d in dimensions {
        let lens = cfg
            .lenses
            .iter()
            .find(|l| l.id == d)
            .cloned()
            .unwrap_or(config::Lens {
                id: d.to_string(),
                criteria: "(this repository's review config has no criteria for it)".into(),
                attack: None,
                triggers: config::Triggers::default(),
                binding: None,
                severity: Default::default(),
                include_generated: false,
            });
        let raws: Vec<roles::Raw> = key
            .findings
            .iter()
            .enumerate()
            .filter(|(_, f)| f.dimension == d)
            .map(|(i, f)| roles::Raw {
                title: format!(
                    "[K{i}] {}",
                    f.what
                        .lines()
                        .next()
                        .unwrap_or_default()
                        .chars()
                        .take(200)
                        .collect::<String>()
                ),
                detail: f.what.clone(),
                file: None,
                line: None,
            })
            .collect();
        let prompt = roles::judge_prompt(&lens, &[raws], &decided)?
            + "\n\nKeep each finding's title as it's given, with its [K..] tag, and judge each \
               on its own: don't merge them.";
        let answer = caller.call(&Call {
            role: Role::Judge,
            binding_name: cfg.binding_name(Role::Judge, None)?,
            binding: judge,
            prompt: &prompt,
            artifact: Some(&artifact),
            writes: false,
            repo: &s.tree,
        })?;
        for r in roles::read_ratings(&answer.text)? {
            let Some(i) = tag
                .captures(&r.title)
                .and_then(|c| c[1].parse::<usize>().ok())
                .filter(|i| *i < judged.len())
            else {
                continue;
            };
            let action = if r.relevant && r.on_task {
                lens.severity
                    .get(&r.severity)
                    .copied()
                    .unwrap_or(r.severity.default_action())
            } else {
                config::Action::Drop
            };
            let blocks = matches!(
                action,
                config::Action::Blocks | config::Action::BlocksUnlessWaived
            );
            judged[i] = Some((blocks, format!("{:?}", r.severity)));
        }
    }
    drop(allowed);
    let mut rows = Vec::new();
    let (mut agreed_b, mut missed, mut over, mut agreed_n, mut unrated) = (0, 0, 0, 0, 0);
    for (f, j) in key.findings.iter().zip(&judged) {
        match (f.blocker, j.as_ref().map(|(b, _)| *b)) {
            (true, Some(true)) => agreed_b += 1,
            (true, Some(false)) => missed += 1,
            (false, Some(true)) => over += 1,
            (false, Some(false)) => agreed_n += 1,
            (_, None) => unrated += 1,
        }
        rows.push(json!({
            "id": f.id, "dimension": f.dimension, "key_blocker": f.blocker,
            "judged_blocker": j.as_ref().map(|(b, _)| *b),
            "severity": j.as_ref().map(|(_, s)| s.clone()),
        }));
    }
    Ok(json!({
        "key": key_path.file_stem().map(|n| n.to_string_lossy().into_owned()),
        "agreed_blockers": agreed_b, "missed_blockers": missed, "overrated": over,
        "agreed_non_blockers": agreed_n, "unrated": unrated, "findings": rows,
    }))
}

/// A run's config, as it was when it started: resuming can't pick up a/// A run's config, as it was when it started: resuming can't pick up a
/// config that's changed since.
fn stored_config(bytes: &[u8]) -> Result<Config> {
    serde_json::from_slice(bytes).map_err(|e| anyhow!("reading the run's config: {e}"))
}

#[cfg(test)]
mod tests {
    use super::change::tests::{branch, commit, repo_with};
    use super::*;

    const LENS: &str = "version: 1\ntiers: [{id: t, match: {paths: ['**']}, fan_out: 1}]\n";

    fn lens(id: &str) -> String {
        format!(
            "{LENS}not_findings: decided.md\nlenses: [{{id: {id}, criteria: c, triggers: {{paths: ['**']}}}}]\n"
        )
    }

    fn paths() -> Paths {
        let dir = std::env::temp_dir().join(format!("tb-acr-paths-{}", new_id()));
        fs::create_dir_all(&dir).unwrap();
        Paths {
            state: dir.join("state"),
            config: dir.join("config.toml"),
        }
    }

    fn target(repo: &Path, config: Option<PathBuf>) -> Target {
        Target {
            repo: repo.to_path_buf(),
            base: "main".into(),
            head: Some("work".into()),
            config,
            human_gate: None,
            max_rounds: None,
            review_scope: None,
            bindings: Vec::new(),
            description: None,
            pr: None,
            brief: None,
        }
    }

    #[test]
    fn a_description_comes_from_its_file_and_nothing_is_none() {
        let dir = std::env::temp_dir().join(format!("tb-desc-{}", new_id()));
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("pr.md");
        fs::write(&file, "# Fix B-1\n\nAn edit keeps the date.").unwrap();
        let mut t = target(&dir, None);
        assert_eq!(description_of(&t, None).unwrap(), None);
        t.description = Some(file);
        assert_eq!(
            description_of(&t, Some(7)).unwrap().as_deref(),
            Some("# Fix B-1\n\nAn edit keeps the date.")
        );
        t.description = Some(dir.join("missing.md"));
        assert!(description_of(&t, None).is_err());
    }

    #[test]
    fn a_config_kept_outside_the_repository_wins_and_brings_its_own_list() {
        let repo = repo_with(&[
            (REPO_CONFIG, &lens("inside")),
            ("decided.md", "inside list"),
        ]);
        branch(&repo, "work");
        commit(&repo, &[("a.txt", "x")], "a change");
        let paths = paths();
        // With nothing outside, it's the repository's own.
        let (cfg, source) = load_config(&paths, &target(&repo, None), &Flags::default()).unwrap();
        assert_eq!(cfg.lenses[0].id, "inside");
        assert!(!cfg.outside);
        assert_eq!(
            not_findings(&cfg, &source, &repo, "main").unwrap(),
            "inside list"
        );

        // Kept in acr/, under the repository's name.
        let kept = outside_config(&paths, &repo).unwrap();
        fs::create_dir_all(kept.parent().unwrap()).unwrap();
        fs::write(&kept, lens("outside")).unwrap();
        fs::write(kept.with_file_name("decided.md"), "outside list").unwrap();
        let (cfg, source) = load_config(&paths, &target(&repo, None), &Flags::default()).unwrap();
        assert_eq!(cfg.lenses[0].id, "outside");
        assert!(cfg.outside);
        assert_eq!(
            not_findings(&cfg, &source, &repo, "main").unwrap(),
            "outside list"
        );

        // Named on the command line, which beats both.
        let named = paths.state.with_file_name("named.yaml");
        fs::write(&named, lens("named")).unwrap();
        fs::write(named.with_file_name("decided.md"), "named list").unwrap();
        let t = target(&repo, Some(named));
        let (cfg, source) = load_config(&paths, &t, &Flags::default()).unwrap();
        assert_eq!(cfg.lenses[0].id, "named");
        assert_eq!(
            not_findings(&cfg, &source, &repo, "main").unwrap(),
            "named list"
        );
    }

    #[test]
    fn with_no_config_anywhere_it_says_where_it_looked() {
        let repo = repo_with(&[("a.txt", "x")]);
        branch(&repo, "work");
        let err = load_config(&paths(), &target(&repo, None), &Flags::default()).unwrap_err();
        assert!(format!("{err:#}").contains(REPO_CONFIG), "{err:#}");
    }

    #[test]
    fn a_caller_removes_only_the_attachments_it_made() {
        let paths = paths();
        let theirs = {
            let caller = BridgeCaller::new(&paths).unwrap();
            // Someone else's, already in the store.
            let theirs = caller
                .store
                .add("theirs.txt", &mut &b"shared"[..], 6)
                .unwrap();
            let again = caller.attach("same.txt", b"shared").unwrap();
            assert_eq!(again.sha256, theirs.sha256);
            let mine = caller.attach("mine.txt", b"only mine").unwrap();
            assert!(caller.store.path(&mine.sha256).exists());
            let mine_path = caller.store.path(&mine.sha256);
            drop(caller);
            assert!(!mine_path.exists(), "it leaves nothing behind");
            theirs
        };
        let store = Attachments::new(paths.attachments(), 1 << 20, 1 << 30);
        assert!(
            store.path(&theirs.sha256).exists(),
            "and never takes another's"
        );
        fs::remove_dir_all(paths.state.parent().unwrap()).unwrap();
    }

    #[test]
    fn a_review_sends_as_itself_never_a_session() {
        let paths = paths();
        assert_eq!(BridgeCaller::new(&paths).unwrap().agent, "acr");
    }

    #[test]
    fn a_fix_needs_somewhere_to_go() {
        let paths = paths();
        let caller = BridgeCaller::new(&paths).unwrap();
        let binding = config::Binding {
            agent: "a@b".into(),
            ..config::Binding::default()
        };
        let call = Call {
            role: Role::Orchestrator,
            binding_name: "large",
            binding: &binding,
            prompt: "Fix these",
            artifact: None,
            writes: true,
            repo: Path::new("."),
        };
        let err = caller.call(&call).err().unwrap();
        assert!(format!("{err:#}").contains("writes: true"), "{err:#}");
    }

    #[test]
    fn a_runs_list_is_the_one_it_started_with() {
        let paths = paths();
        let cfg = config::resolve(
            &Default::default(),
            config::parse_repo(&lens("l")).unwrap(),
            &Flags::default(),
        )
        .unwrap();
        let s = setting(&paths, &cfg, "r1", Some("the list".into())).unwrap();
        assert_eq!(s.not_findings, "the list");
        let again = setting(&paths, &cfg, "r1", None).unwrap();
        assert_eq!(again.not_findings, "the list");
    }
}
