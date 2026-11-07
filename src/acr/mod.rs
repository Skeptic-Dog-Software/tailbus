//! `tailbus acr`: adversarial convergence review for any repository. The
//! framework holds no knowledge of a repository. Roles, tiers, lenses and
//! bindings all come from config, in `config`.

mod change;
mod config;
mod flow;
mod keep;
mod plan;
mod report;
mod roles;

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

/// The bus agent reviews are sent as, unless `--as` says otherwise.
const SENDER: &str = "acr";
/// Seconds a call waits past its binding's timeout, or with none.
const WAIT_MARGIN: u64 = 120;
const DEFAULT_CALL_SECS: u64 = 1800;
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
    fn new(paths: &Paths, agent: Option<String>) -> Result<BridgeCaller> {
        let bus = BusConfig::load(&paths.config)?;
        Ok(BridgeCaller {
            socket: paths.socket(),
            agent: agent.unwrap_or_else(|| SENDER.to_string()),
            store: Attachments::new(
                paths.attachments(),
                bus.max_attachment_bytes,
                bus.attachment_store_bytes,
            ),
            made: std::sync::Mutex::default(),
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

    fn noted(&self, sha: &str) {
        let mut made = self.made.lock().unwrap_or_else(|e| e.into_inner());
        made.push(sha.to_string());
    }
}

impl Caller for BridgeCaller {
    fn fingerprint(&self, repo: &Path) -> Option<String> {
        change::fingerprint(repo).ok()
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
            // An Orchestrator's fixes use what its bridge gives it.
            tools: if call.writes { None } else { b.tools.clone() },
            session: Some("fresh".into()),
            timeout: b.timeout,
            // Every role works in the repository, so reviewers read the
            // code itself, read-only, and an Orchestrator fixes it there.
            workdir: Some(call.repo.display().to_string()),
            model: b.model.clone(),
            effort: b.effort.clone(),
            // A review keeps its own record, and leaves nothing behind.
            log: Some(false),
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
        let wait = b.timeout.unwrap_or(DEFAULT_CALL_SECS) + WAIT_MARGIN;
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
        match client.reply::<Awaited>()? {
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

pub fn run(paths: &Paths, agent: Option<String>, command: AcrCmd) -> Result<ExitCode> {
    keep::prune(paths);
    match command {
        AcrCmd::Schema => print!("{SCHEMA}"),
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
            let id = new_id();
            let list = not_findings(&cfg, &source, &target.repo, &target.base)?;
            let s = setting(paths, &cfg, &id, Some(list))?;
            let (repo, base, head) = (&target.repo, target.base.as_str(), target.head.as_deref());
            let change = reload(&cfg, repo, base, head)?;
            let caller = BridgeCaller::new(paths, agent)?;
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
            let s = setting(paths, &cfg, &run_id, None)?;
            let (repo, base) = (run.repo.clone(), run.base.clone());
            let caller = BridgeCaller::new(paths, agent)?;
            let head = run.head.clone();
            let reloading =
                |fixed: bool| reload(&cfg, &repo, &base, head.as_deref().filter(|_| !fixed));
            flow::drive(&mut run, &s, &caller, &reloading)?;
            return finish(paths, &run, &cfg, post_pr);
        }
        AcrCmd::Benchmark { target, known } => {
            let mut flags = flags(&target);
            // One round, as it first comes out, with nobody fixing anything.
            flags.max_rounds = Some(1);
            flags.human_gate = Some(HumanGate::Exception);
            let (cfg, source) = load_config(paths, &target, &flags)?;
            cfg.check_bindings()?;
            cfg.check_local(&this_machine(paths)?)?;
            change::checked_out(&target.repo, target.head.as_deref())?;
            let known: Vec<Known> = serde_yaml_ng::from_str(
                &fs::read_to_string(&known)
                    .with_context(|| format!("reading {}", known.display()))?,
            )
            .context("in the known blockers")?;
            let id = new_id();
            let list = not_findings(&cfg, &source, &target.repo, &target.base)?;
            let s = setting(paths, &cfg, &id, Some(list))?;
            let (repo, base, head) = (&target.repo, target.base.as_str(), target.head.as_deref());
            let change = reload(&cfg, repo, base, head)?;
            let caller = BridgeCaller::new(paths, agent)?;
            let mut run = flow::start(&id, repo, base, head, &s, &change, &caller)?;
            for e in &mut run.escalations {
                e.seen = true;
            }
            flow::drive(&mut run, &s, &caller, &|_| reload(&cfg, repo, base, head))?;
            for e in &mut run.escalations {
                e.seen = true;
            }
            save(paths, &run, &cfg)?;
            // One round, forced, says nothing about how many reviews take.
            keep::finished(paths, &run, false)?;
            let score = flow::score(&run, &known)?;
            println!("{}", serde_json::to_string_pretty(&score)?);
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// A run's config, as it was when it started: resuming can't pick up a
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
        }
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
            let caller = BridgeCaller::new(&paths, None).unwrap();
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
    fn a_fix_needs_somewhere_to_go() {
        let paths = paths();
        let caller = BridgeCaller::new(&paths, None).unwrap();
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
