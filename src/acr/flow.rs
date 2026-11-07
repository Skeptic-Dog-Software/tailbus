//! The review loop. Code tracks the rounds, so a run is repeatable and can
//! stop and start again: a model is called only for what needs judgment.
//!
//! A lens is confirmed only by a round that really ran. A reviewer or Judge
//! that fails leaves its lens open and goes to a person, because a review
//! that didn't happen must never read as a clean one.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::thread;

use anyhow::{Result, anyhow, bail};
use serde::{Deserialize, Serialize};

use crate::proto::RunUsage;

use super::change::{self, Change};
use super::config::{Action, Binding, Config, HumanGate, Lens, Role, Severity};
use super::plan::{self, Why, tokens_in};
use super::roles::{self, Raw};

/// What a role's call needs.
pub struct Call<'a> {
    pub role: Role,
    pub binding_name: &'a str,
    pub binding: &'a Binding,
    pub prompt: &'a str,
    /// The change, as a file.
    pub artifact: Option<&'a Path>,
    /// It may change the working tree: only an Orchestrator fixing.
    pub writes: bool,
}

pub struct Answer {
    pub text: String,
    pub run_id: String,
    /// What the call used, if its agent said.
    pub usage: Option<RunUsage>,
}

/// How a role reaches its model.
pub trait Caller: Sync {
    fn call(&self, call: &Call) -> Result<Answer>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Status {
    Running,
    /// Waiting for a person, for the gate in `waiting`.
    Waiting,
    /// Every lens confirmed or waived.
    Confirmed,
    /// It ended with lenses open.
    Stopped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Gate {
    Scope,
    Fold,
    Final,
    Escalation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Stage {
    Scope,
    Review,
    Decide,
    Fix,
    Final,
    Done,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Outcome {
    Open,
    Confirmed,
    Waived,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FindingState {
    /// A blocker with nothing done about it yet.
    Open,
    /// To be fixed, or was.
    Folded,
    Waived,
    /// Doesn't block: logged.
    Logged,
    /// Rejected: dropped, with its reason kept.
    Dropped,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Finding {
    pub id: String,
    pub title: String,
    pub detail: String,
    pub file: Option<String>,
    pub line: Option<u64>,
    pub severity: Severity,
    pub action: Action,
    pub reason: String,
    pub state: FindingState,
}

impl Finding {
    pub fn blocks(&self) -> bool {
        matches!(self.action, Action::Blocks | Action::BlocksUnlessWaived)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Round {
    pub round: u32,
    /// Each reviewer that ran, labeled as the AI agent it is.
    pub reviewers: Vec<String>,
    /// What went wrong, if the round didn't fully run.
    pub failures: Vec<String>,
    pub findings: Vec<Finding>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LensResult {
    pub id: String,
    pub criteria: String,
    pub rounds: Vec<Round>,
    /// What the Orchestrator did about its blockers.
    pub done: Vec<String>,
    pub outcome: Outcome,
}

impl LensResult {
    fn latest(&self) -> Option<&Round> {
        self.rounds.last()
    }

    pub fn blockers(&self) -> Vec<&Finding> {
        let round = self.latest().map_or(&[][..], |r| &r.findings);
        round
            .iter()
            .filter(|f| f.blocks() && f.state == FindingState::Open)
            .collect()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Escalation {
    pub reason: String,
    /// A person has seen it.
    pub seen: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Disagreement {
    pub lens: String,
    pub finding: String,
    pub sides: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Usage {
    pub role: Role,
    pub binding: String,
    pub agent: String,
    pub model: Option<String>,
    pub run_id: String,
    /// Input not read from or written to a cache.
    pub tokens_in: u64,
    #[serde(default)]
    pub cache_read: u64,
    #[serde(default)]
    pub cache_write: u64,
    pub tokens_out: u64,
    /// By the config's prices, or the agent's if the config has none.
    pub usd: Option<f64>,
    /// The tokens are the agent's own count, not an estimate.
    #[serde(default)]
    pub measured: bool,
    /// What the agent said the call cost.
    #[serde(default)]
    pub agent_usd: Option<f64>,
}

impl Usage {
    pub fn tokens(&self) -> u64 {
        self.tokens_in + self.cache_read + self.cache_write + self.tokens_out
    }
}

/// A review, as far as it's got. It's all saved, so that it can stop for a
/// person and carry on.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Run {
    pub id: String,
    pub repo: PathBuf,
    pub base: String,
    pub head: Option<String>,
    pub tier: String,
    pub tier_why: String,
    pub fan_out: u32,
    pub round: u32,
    pub stage: Stage,
    pub status: Status,
    pub waiting: Option<Gate>,
    pub lenses: BTreeMap<String, LensResult>,
    pub escalations: Vec<Escalation>,
    pub disagreements: Vec<Disagreement>,
    pub usage: Vec<Usage>,
}

/// What stays the same through a review.
pub struct Setting<'a> {
    pub cfg: &'a Config,
    pub not_findings: String,
    /// Where a round's diff is written.
    pub work_dir: PathBuf,
}

/// A person's answer at a gate.
#[derive(Debug, Clone, Default)]
pub struct Decision {
    pub fold: Vec<String>,
    pub waive: Vec<String>,
    pub approve: bool,
    pub stop: bool,
}

/// What a call cost: by the agent's count if it gave one, or else by the
/// text it took and gave and by the config. Either is priced by the config,
/// falling back to what the agent said.
fn usage(cfg: &Config, call: &Call, input_bytes: usize, answer: &Answer) -> Usage {
    let mut usage = Usage {
        role: call.role,
        binding: call.binding_name.to_string(),
        agent: call.binding.agent.clone(),
        model: call.binding.model.clone(),
        run_id: answer.run_id.clone(),
        tokens_in: 0,
        cache_read: 0,
        cache_write: 0,
        tokens_out: 0,
        usd: None,
        measured: false,
        agent_usd: None,
    };
    match &answer.usage {
        Some(counted) => {
            usage.tokens_in = counted.input_tokens;
            usage.cache_read = counted.cache_read_tokens;
            usage.cache_write = counted.cache_write_tokens;
            usage.tokens_out = counted.output_tokens;
            usage.measured = true;
            usage.agent_usd = counted.usd;
            usage.usd = cfg
                .price(call.binding_name, call.binding)
                .map(|p| {
                    p.usd(
                        usage.tokens_in,
                        usage.cache_read,
                        usage.cache_write,
                        usage.tokens_out,
                    )
                })
                .or(counted.usd);
        }
        None => {
            let guess = plan::cost(
                cfg,
                call.role,
                call.binding_name,
                Some(call.binding),
                tokens_in(input_bytes),
                tokens_in(answer.text.len()),
            );
            usage.tokens_in = guess.tokens_in;
            usage.cache_read = guess.cache_read;
            usage.tokens_out = guess.tokens_out;
            usage.usd = guess.usd;
        }
    }
    usage
}

fn record(run: &mut Run, cfg: &Config, call: &Call, input_bytes: usize, answer: &Answer) {
    run.usage.push(usage(cfg, call, input_bytes, answer));
}

fn escalate(run: &mut Run, reason: String) {
    if !run.escalations.iter().any(|e| e.reason == reason) {
        run.escalations.push(Escalation {
            reason,
            seen: false,
        });
    }
}

fn input_bytes(prompt: &str, artifact: Option<&Path>) -> usize {
    prompt.len()
        + artifact
            .and_then(|p| fs::metadata(p).ok())
            .map_or(0, |m| m.len() as usize)
}

/// One call to a role, on the record.
fn ask(
    run: &mut Run,
    s: &Setting,
    caller: &dyn Caller,
    role: Role,
    prompt: &str,
    artifact: Option<&Path>,
) -> Result<String> {
    let name = s.cfg.binding_name(role, None)?;
    let call = Call {
        role,
        binding_name: name,
        binding: s.cfg.binding(role, None)?,
        prompt,
        artifact,
        writes: false,
    };
    let answer = caller.call(&call)?;
    record(run, s.cfg, &call, input_bytes(prompt, artifact), &answer);
    Ok(answer.text)
}

fn write_artifact(s: &Setting, change: &Change, round: u32) -> Result<PathBuf> {
    fs::create_dir_all(&s.work_dir)?;
    let path = s.work_dir.join(format!("round-{round}.diff"));
    fs::write(&path, &change.diff)?;
    Ok(path)
}

/// The tier: from paths if they settle it, or else the Sizer's, and higher
/// when it isn't sure.
fn choose_tier(
    run: &mut Run,
    s: &Setting,
    change: &Change,
    caller: &dyn Caller,
    artifact: &Path,
) -> Result<()> {
    let cfg = s.cfg;
    let tier = if let Some(tier) = plan::tier_by_paths(cfg, change)? {
        run.tier_why = "its paths match".into();
        tier
    } else {
        let described = plan::described_tiers(cfg);
        let prompt = roles::sizer_prompt(&described, &change.files);
        let answer = ask(run, s, caller, Role::Sizer, &prompt, Some(artifact))
            .and_then(|text| roles::read_sized(&text));
        match answer {
            Ok(sized) => match cfg.tiers.iter().position(|t| t.id == sized.tier) {
                Some(at) if sized.sure => {
                    run.tier_why = format!("the Sizer: {}", sized.reason);
                    &cfg.tiers[at]
                }
                Some(at) => {
                    run.tier_why =
                        format!("the Sizer wasn't sure of {}, so one higher", sized.tier);
                    &cfg.tiers[(at + 1).min(cfg.tiers.len() - 1)]
                }
                None => {
                    run.tier_why =
                        format!("the Sizer named no tier ({}), so the highest", sized.tier);
                    plan::highest(cfg)
                }
            },
            Err(e) => {
                run.tier_why = format!("the Sizer failed ({e:#}), so the highest");
                plan::highest(cfg)
            }
        }
    };
    run.tier = tier.id.clone();
    run.fan_out = tier.fan_out;
    Ok(())
}

/// The lenses: those triggered, plus those the Orchestrator says apply. If
/// it can't say, every candidate stays in, and a person hears why.
fn choose_lenses(
    run: &mut Run,
    s: &Setting,
    change: &Change,
    caller: &dyn Caller,
    artifact: &Path,
) -> Result<()> {
    let cfg = s.cfg;
    let tier = cfg
        .tiers
        .iter()
        .find(|t| t.id == run.tier)
        .ok_or_else(|| anyhow!("no tier {}", run.tier))?;
    let candidates = plan::select(cfg, tier, change)?;
    let described: Vec<(&str, &str)> = candidates
        .iter()
        .filter_map(|(lens, why)| match why {
            Why::Description(d) => Some((lens.id.as_str(), d.as_str())),
            _ => None,
        })
        .collect();
    let mut wanted: Option<Vec<String>> = None;
    if !described.is_empty() || !cfg.always_escalate.is_empty() {
        let prompt = roles::orchestrator_prompt(&described, &cfg.always_escalate);
        let answer = ask(run, s, caller, Role::Orchestrator, &prompt, Some(artifact))
            .and_then(|text| roles::read_selected(&text));
        match answer {
            Ok(selected) => {
                for item in selected.escalate {
                    if cfg.always_escalate.contains(&item) {
                        escalate(run, format!("always_escalate: {item}"));
                    }
                }
                wanted = Some(selected.lenses);
            }
            Err(e) => escalate(
                run,
                format!("the Orchestrator couldn't check this change's descriptions: {e:#}"),
            ),
        }
    }
    for (lens, why) in candidates {
        let described = matches!(why, Why::Description(_));
        if described && wanted.as_ref().is_some_and(|w| !w.contains(&lens.id)) {
            continue;
        }
        run.lenses.insert(
            lens.id.clone(),
            LensResult {
                id: lens.id.clone(),
                criteria: lens.criteria.clone(),
                rounds: Vec::new(),
                done: Vec::new(),
                outcome: Outcome::Open,
            },
        );
    }
    if run.lenses.is_empty() {
        escalate(run, "no lens applies to this change".into());
    }
    Ok(())
}

/// Starts a review of `change`.
pub fn start(
    id: &str,
    repo: &Path,
    base: &str,
    head: Option<&str>,
    s: &Setting,
    change: &Change,
    caller: &dyn Caller,
) -> Result<Run> {
    let mut run = Run {
        id: id.to_string(),
        // Whole, so that history finds it from anywhere.
        repo: repo.canonicalize().unwrap_or_else(|_| repo.to_path_buf()),
        base: base.to_string(),
        head: head.map(str::to_string),
        tier: String::new(),
        tier_why: String::new(),
        fan_out: 1,
        round: 1,
        stage: Stage::Review,
        status: Status::Running,
        waiting: None,
        lenses: BTreeMap::new(),
        escalations: Vec::new(),
        disagreements: Vec::new(),
        usage: Vec::new(),
    };
    let artifact = write_artifact(s, change, 0)?;
    choose_tier(&mut run, s, change, caller, &artifact)?;
    choose_lenses(&mut run, s, change, caller, &artifact)?;
    if let Some(why) = plan::config_escalation(s.cfg, change) {
        escalate(&mut run, why);
    }
    if s.cfg.human_gate == HumanGate::Ends {
        run.stage = Stage::Scope;
    }
    Ok(run)
}

fn lens_of<'a>(cfg: &'a Config, id: &str) -> Result<&'a Lens> {
    cfg.lenses
        .iter()
        .find(|l| l.id == id)
        .ok_or_else(|| anyhow!("no lens {id}"))
}

/// Every open lens's reviewers, at once, each a fresh session that sees
/// only its brief and the change.
fn attack(
    run: &mut Run,
    s: &Setting,
    caller: &dyn Caller,
    artifact: &Path,
) -> Result<BTreeMap<String, Vec<Result<Vec<Raw>>>>> {
    let cfg = s.cfg;
    let scope = match run.head.as_deref() {
        Some(head) => format!("the change from {} to {head}", run.base),
        None => format!("the working tree against {}", run.base),
    };
    let open: Vec<&LensResult> = run
        .lenses
        .values()
        .filter(|l| l.outcome == Outcome::Open)
        .collect();
    let mut briefs = Vec::new();
    for result in open {
        let lens = lens_of(cfg, &result.id)?;
        let prompt = roles::reviewer_prompt(lens, &scope, &s.not_findings);
        for _ in 0..run.fan_out {
            briefs.push((lens, prompt.clone()));
        }
    }
    let answers: Vec<_> = thread::scope(|scope| {
        let handles: Vec<_> = briefs
            .iter()
            .map(|(lens, prompt)| {
                scope.spawn(move || -> Result<(Usage, Vec<Raw>)> {
                    let name = cfg.binding_name(Role::Reviewer, Some(lens))?;
                    let binding = cfg.binding(Role::Reviewer, Some(lens))?;
                    let call = Call {
                        role: Role::Reviewer,
                        binding_name: name,
                        binding,
                        prompt,
                        artifact: Some(artifact),
                        writes: false,
                    };
                    let answer = caller.call(&call)?;
                    let usage = usage(cfg, &call, input_bytes(prompt, Some(artifact)), &answer);
                    Ok((usage, roles::read_findings(&answer.text)?))
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| {
                h.join()
                    .unwrap_or_else(|_| Err(anyhow!("a reviewer's thread died")))
            })
            .collect()
    });
    let mut by_lens: BTreeMap<String, Vec<Result<Vec<Raw>>>> = BTreeMap::new();
    for ((lens, _), answer) in briefs.iter().zip(answers) {
        let result = answer.map(|(usage, findings)| {
            run.usage.push(usage);
            findings
        });
        by_lens.entry(lens.id.clone()).or_default().push(result);
    }
    Ok(by_lens)
}

/// Turns a Judge's ratings into a lens's findings for this round.
fn rate(run: &mut Run, cfg: &Config, lens: &Lens, ratings: Vec<roles::Rating>) -> Vec<Finding> {
    let mut findings = Vec::new();
    for (n, rating) in ratings.into_iter().enumerate() {
        let severity = if rating.relevant && rating.on_task {
            rating.severity
        } else {
            Severity::Rejected
        };
        let action = cfg.action(lens, severity);
        let state = match action {
            Action::Blocks | Action::BlocksUnlessWaived => FindingState::Open,
            Action::NeverBlocks => FindingState::Logged,
            Action::Drop => FindingState::Dropped,
        };
        let id = format!("{}-r{}-{}", lens.id, run.round, n + 1);
        if rating.split || (rating.values_call && severity == Severity::Rejected) {
            let what = if rating.split {
                "reviewers split"
            } else {
                "a values call"
            };
            escalate(run, format!("{what} on {id}: {}", rating.title));
            run.disagreements.push(Disagreement {
                lens: lens.id.clone(),
                finding: id.clone(),
                sides: rating.sides.clone(),
            });
        }
        findings.push(Finding {
            id,
            title: rating.title,
            detail: rating.detail,
            file: rating.file,
            line: rating.line,
            severity,
            action,
            reason: rating.reason,
            state,
        });
    }
    findings
}

/// One round: attack every open lens, judge what comes back, and settle
/// what's next. Judging stays out of the orchestration's context.
fn review_round(run: &mut Run, s: &Setting, change: &Change, caller: &dyn Caller) -> Result<()> {
    let cfg = s.cfg;
    let artifact = write_artifact(s, change, run.round)?;
    let reviews = attack(run, s, caller, &artifact)?;
    for (id, results) in reviews {
        let lens = lens_of(cfg, &id)?;
        let name = cfg.binding_name(Role::Reviewer, Some(lens))?;
        let mut round = Round {
            round: run.round,
            ..Round::default()
        };
        let mut lists = Vec::new();
        for (n, result) in results.into_iter().enumerate() {
            let label = format!("Reviewer {} (AI agent, {name}), {id}", n + 1);
            match result {
                Ok(list) => {
                    round.reviewers.push(label);
                    lists.push(list);
                }
                Err(e) => round.failures.push(format!("{label}: {e:#}")),
            }
        }
        if lists.iter().any(|l| !l.is_empty()) {
            let judged = roles::judge_prompt(lens, &lists, &s.not_findings).and_then(|prompt| {
                let text = ask(run, s, caller, Role::Judge, &prompt, None)?;
                roles::read_ratings(&text)
            });
            match judged {
                Ok(ratings) => round.findings = rate(run, cfg, lens, ratings),
                Err(e) => round.failures.push(format!("the Judge: {e:#}")),
            }
        }
        for failure in &round.failures {
            escalate(
                run,
                format!(
                    "{id} wasn't fully reviewed in round {}: {failure}",
                    run.round
                ),
            );
        }
        let confirmed = round.failures.is_empty()
            && !round
                .findings
                .iter()
                .any(|f| f.blocks() && f.state == FindingState::Open);
        let result = run.lenses.get_mut(&id).expect("reviewed because it's open");
        result.rounds.push(round);
        if confirmed {
            result.outcome = Outcome::Confirmed;
        }
    }
    settle(run, cfg);
    Ok(())
}

fn all_settled(run: &Run) -> bool {
    run.lenses.values().all(|l| l.outcome != Outcome::Open)
}

/// What comes after a round.
fn settle(run: &mut Run, cfg: &Config) {
    let open: Vec<String> = run
        .lenses
        .values()
        .filter(|l| l.outcome == Outcome::Open)
        .map(|l| l.id.clone())
        .collect();
    if open.is_empty() {
        run.stage = if cfg.human_gate == HumanGate::Ends {
            Stage::Final
        } else {
            Stage::Done
        };
        return;
    }
    let blocked = run.lenses.values().any(|l| !l.blockers().is_empty());
    if run.round >= cfg.max_rounds {
        for id in &open {
            escalate(
                run,
                format!("{id} still has blockers at max_rounds ({})", cfg.max_rounds),
            );
        }
        run.stage = Stage::Done;
        return;
    }
    if !blocked {
        // Open with no blockers: its round failed, and a person has heard.
        // They decide whether it goes again.
        run.round += 1;
        run.stage = Stage::Review;
        return;
    }
    if cfg.human_gate == HumanGate::PerRound {
        run.stage = Stage::Decide;
    } else {
        for lens in run.lenses.values_mut() {
            if let Some(round) = lens.rounds.last_mut() {
                for f in round.findings.iter_mut() {
                    if f.blocks() && f.state == FindingState::Open {
                        f.state = FindingState::Folded;
                    }
                }
            }
        }
        run.stage = Stage::Fix;
    }
}

/// Has the Orchestrator fix what's been folded in.
fn fix(run: &mut Run, s: &Setting, caller: &dyn Caller) -> Result<()> {
    let folded: Vec<(String, Finding)> = run
        .lenses
        .values()
        .filter_map(|l| l.latest().map(|r| (l.id.clone(), r)))
        .flat_map(|(id, r)| {
            r.findings
                .iter()
                .filter(|f| f.state == FindingState::Folded)
                .map(move |f| (id.clone(), f.clone()))
        })
        .collect();
    let list: String = folded
        .iter()
        .map(|(_, f)| {
            let at = f.file.as_deref().unwrap_or("somewhere in the change");
            format!("- {} ({at}): {}\n  {}\n", f.id, f.title, f.detail)
        })
        .collect();
    let prompt = roles::fix_prompt(&list);
    let cfg = s.cfg;
    let name = cfg.binding_name(Role::Orchestrator, None)?;
    let call = Call {
        role: Role::Orchestrator,
        binding_name: name,
        binding: cfg.binding(Role::Orchestrator, None)?,
        prompt: &prompt,
        artifact: None,
        writes: true,
    };
    let fixed = caller.call(&call).and_then(|answer| {
        record(run, cfg, &call, prompt.len(), &answer);
        roles::read_fixed(&answer.text)
    });
    match fixed {
        Ok(fixed) => {
            for (id, _) in &folded {
                let lens = run.lenses.get_mut(id).expect("found above");
                if !lens.done.contains(&fixed.done) {
                    lens.done.push(fixed.done.clone());
                }
            }
            run.round += 1;
            run.stage = Stage::Review;
        }
        Err(e) => {
            escalate(
                run,
                format!("the Orchestrator couldn't fix the blockers: {e:#}"),
            );
            run.stage = Stage::Done;
        }
    }
    Ok(())
}

fn unseen(run: &Run) -> bool {
    run.escalations.iter().any(|e| !e.seen)
}

fn pause(run: &mut Run, gate: Gate) {
    run.status = Status::Waiting;
    run.waiting = Some(gate);
}

/// Carries a review on until it ends or needs a person. An escalation
/// always stops it, whatever the gate. `reload` gets the change as it
/// stands, which is the working tree once there are fixes in it.
pub fn drive(
    run: &mut Run,
    s: &Setting,
    caller: &dyn Caller,
    reload: &dyn Fn(bool) -> Result<Change>,
) -> Result<()> {
    run.status = Status::Running;
    run.waiting = None;
    loop {
        if unseen(run) {
            pause(run, Gate::Escalation);
            return Ok(());
        }
        match run.stage {
            Stage::Scope => {
                pause(run, Gate::Scope);
                return Ok(());
            }
            Stage::Review => {
                let fixed = run.lenses.values().any(|l| !l.done.is_empty());
                review_round(run, s, &reload(fixed)?, caller)?;
            }
            Stage::Decide => {
                pause(run, Gate::Fold);
                return Ok(());
            }
            Stage::Fix => fix(run, s, caller)?,
            Stage::Final => {
                pause(run, Gate::Final);
                return Ok(());
            }
            Stage::Done => {
                run.status = if all_settled(run) && !run.lenses.is_empty() {
                    Status::Confirmed
                } else {
                    Status::Stopped
                };
                return Ok(());
            }
        }
    }
}

/// Applies a person's answer at a gate.
pub fn decide(run: &mut Run, decision: &Decision) -> Result<()> {
    let Some(gate) = run.waiting else {
        bail!("run {} isn't waiting for anyone", run.id);
    };
    if decision.stop {
        for e in &mut run.escalations {
            e.seen = true;
        }
        run.stage = Stage::Done;
        return Ok(());
    }
    match gate {
        Gate::Escalation | Gate::Scope | Gate::Final => {
            if !decision.approve {
                bail!("this needs --approve or --stop");
            }
            for e in &mut run.escalations {
                e.seen = true;
            }
            match run.stage {
                Stage::Scope => run.stage = Stage::Review,
                Stage::Final => run.stage = Stage::Done,
                _ => {}
            }
        }
        Gate::Fold => decide_blockers(run, decision)?,
    }
    Ok(())
}

/// Every blocker must be folded in or waived, and only what blocks unless
/// waived can be waived.
fn decide_blockers(run: &mut Run, decision: &Decision) -> Result<()> {
    let open: Vec<String> = run
        .lenses
        .values()
        .flat_map(|l| l.blockers())
        .map(|f| f.id.clone())
        .collect();
    let named = |id: &String| decision.fold.contains(id) || decision.waive.contains(id);
    if let Some(id) = open.iter().find(|id| !named(id)) {
        bail!("{id} needs --fold or --waive");
    }
    if let Some(id) = decision
        .fold
        .iter()
        .chain(&decision.waive)
        .find(|id| !open.contains(id))
    {
        bail!("{id} isn't an open blocker");
    }
    let mut folding = false;
    for lens in run.lenses.values_mut() {
        let Some(round) = lens.rounds.last_mut() else {
            continue;
        };
        for f in round.findings.iter_mut() {
            if decision.waive.contains(&f.id) {
                if f.action != Action::BlocksUnlessWaived {
                    bail!("{} is {:?}, which can't be waived", f.id, f.severity);
                }
                f.state = FindingState::Waived;
            } else if decision.fold.contains(&f.id) {
                f.state = FindingState::Folded;
                folding = true;
            }
        }
        let open_left = round
            .findings
            .iter()
            .any(|f| f.blocks() && matches!(f.state, FindingState::Open | FindingState::Folded));
        if lens.outcome == Outcome::Open && !open_left && round.failures.is_empty() {
            lens.outcome = Outcome::Waived;
        }
    }
    run.stage = if folding { Stage::Fix } else { Stage::Done };
    Ok(())
}

/// The blockers a benchmark knows are in a commit.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Known {
    pub id: String,
    #[serde(default)]
    pub file: Option<String>,
    /// A pattern a finding's title or detail must match.
    pub pattern: String,
}

#[derive(Debug, Serialize)]
pub struct Score {
    pub caught: Vec<String>,
    pub missed: Vec<String>,
    /// Blocking findings that match nothing known.
    pub false_findings: Vec<String>,
    pub usd: Option<f64>,
    pub tokens: u64,
    pub failures: Vec<String>,
}

/// How a run's first round did against what's known to be there.
pub fn score(run: &Run, known: &[Known]) -> Result<Score> {
    let findings: Vec<&Finding> = run
        .lenses
        .values()
        .flat_map(|l| l.rounds.iter())
        .flat_map(|r| r.findings.iter())
        .filter(|f| f.blocks())
        .collect();
    let mut caught = Vec::new();
    let mut missed = Vec::new();
    let mut matched = Vec::new();
    for k in known {
        let re = regex::Regex::new(&k.pattern)?;
        let hit: Vec<&&Finding> = findings
            .iter()
            .filter(|f| {
                let file = k.file.as_ref().is_none_or(|p| f.file.as_ref() == Some(p));
                file && (re.is_match(&f.title) || re.is_match(&f.detail))
            })
            .collect();
        if hit.is_empty() {
            missed.push(k.id.clone());
        } else {
            caught.push(k.id.clone());
            matched.extend(hit.iter().map(|f| f.id.clone()));
        }
    }
    let false_findings = findings
        .iter()
        .filter(|f| !matched.contains(&f.id))
        .map(|f| format!("{}: {}", f.id, f.title))
        .collect();
    Ok(Score {
        caught,
        missed,
        false_findings,
        usd: run.usage.iter().map(|u| u.usd).sum(),
        tokens: run.usage.iter().map(Usage::tokens).sum(),
        failures: run
            .lenses
            .values()
            .flat_map(|l| l.rounds.iter())
            .flat_map(|r| r.failures.iter().cloned())
            .collect(),
    })
}

/// Reads a repository's config and decided non-issues from its base.
pub fn from_base(repo: &Path, base: &str, path: Option<&str>) -> Result<String> {
    Ok(match path {
        Some(path) => change::at_base(repo, base, path)?.unwrap_or_default(),
        None => String::new(),
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::super::config::{Flags, parse_repo, parse_user, resolve};
    use super::super::report;
    use super::*;
    use crate::util::new_id;

    const USER: &str = include_str!("../../examples/acr/user.yaml");
    const NOTHING: &str = r#"{"lenses": [], "escalate": []}"#;
    const CLEAN: &str = r#"{"findings": []}"#;
    const ONE: &str = r#"{"findings": [{"title": "a defect", "detail": "why", "file": "a.src"}]}"#;

    /// A small repository's config: two lenses, and two tiers.
    fn config(gate: &str, rounds: u32, fan_out: u32, extra: &str, top: &str) -> Config {
        let text = format!(
            r#"
version: 1
human_gate: {gate}
max_rounds: {rounds}
{top}
tiers:
  - {{id: small, match: {{description: "small things"}}, fan_out: 1}}
  - {{id: big, match: {{paths: ["**/*.src"], description: "big things"}}, fan_out: {fan_out}}}
lenses:
  - {{id: alpha, criteria: "alpha criteria", triggers: {{paths: ["**/*.src"]}}}}
  - {{id: beta, criteria: "beta criteria", triggers: {{paths: ["**/*.src"]}}}}
{extra}
"#
        );
        let user = parse_user(USER).unwrap();
        resolve(&user, parse_repo(&text).unwrap(), &Flags::default()).unwrap()
    }

    fn plain(gate: &str, rounds: u32, fan_out: u32) -> Config {
        config(gate, rounds, fan_out, "", "")
    }

    fn change(files: &[&str]) -> Change {
        Change {
            files: files.iter().map(|f| f.to_string()).collect(),
            diff: "+let x = 1;\n".to_string(),
        }
    }

    /// Answers each call from a function, and keeps a record of the calls.
    struct Fake<F> {
        answer: F,
        calls: Mutex<Vec<(Role, String, bool, String)>>,
        /// What every call says it used, as a bridge with `usage` would.
        usage: Option<RunUsage>,
    }

    impl<F: Fn(Role, &str) -> Result<String> + Sync> Fake<F> {
        fn new(answer: F) -> Fake<F> {
            Fake {
                answer,
                calls: Mutex::new(Vec::new()),
                usage: None,
            }
        }

        fn count(&self, role: Role) -> usize {
            let calls = self.calls.lock().unwrap();
            calls.iter().filter(|c| c.0 == role).count()
        }
    }

    impl<F: Fn(Role, &str) -> Result<String> + Sync> Caller for Fake<F> {
        fn call(&self, call: &Call) -> Result<Answer> {
            self.calls.lock().unwrap().push((
                call.role,
                call.binding_name.to_string(),
                call.writes,
                call.prompt.to_string(),
            ));
            let text = (self.answer)(call.role, call.prompt)?;
            Ok(Answer {
                text,
                run_id: new_id(),
                usage: self.usage.clone(),
            })
        }
    }

    #[test]
    fn a_call_its_agent_counted_is_recorded_as_counted() {
        let prices = "costs: {prices: {example/glm-flash: {input_per_mtok: 1, output_per_mtok: 2, cached_input_per_mtok: 0.1}, \
                      opus: {input_per_mtok: 1, output_per_mtok: 2, cached_input_per_mtok: 0.1}}, \
                      per_call: {reviewer: {input_tokens: 99999}}}";
        let cfg = config("exception", 1, 1, "", prices);
        let mut fake = Fake::new(defect_in("alpha", 99));
        fake.usage = Some(RunUsage {
            calls: 4,
            input_tokens: 1_000_000,
            output_tokens: 1_000_000,
            cache_read_tokens: 1_000_000,
            usd: Some(9.0),
            ..RunUsage::default()
        });
        let (run, _) = go(&cfg, &["a.src"], &fake);
        assert!(!run.usage.is_empty());
        for u in &run.usage {
            assert!(u.measured);
            // The agent's count, never the config's guess on top of it.
            assert_eq!(
                (u.tokens_in, u.cache_read, u.tokens_out),
                (1_000_000, 1_000_000, 1_000_000)
            );
            assert!((u.usd.unwrap() - 3.1).abs() < 1e-9, "priced by the config");
            assert_eq!(u.agent_usd, Some(9.0));
        }
        let json = serde_json::to_value(report::report(&run, &cfg)).unwrap();
        assert_eq!(json["calls_measured"], run.usage.len());
        assert_eq!(json["calls_estimated"], 0);
        let calls = run.usage.len() as f64;
        assert!((json["agent_usd"].as_f64().unwrap() - 9.0 * calls).abs() < 1e-9);
        // With no price in the config, the agent's own is used.
        let bare = plain("exception", 1, 1);
        let (run, _) = go(&bare, &["a.src"], &fake);
        assert!(run.usage.iter().all(|u| u.usd == Some(9.0)));
    }

    fn rating(severity: &str) -> String {
        format!(
            r#"{{"findings": [{{"title": "a defect", "file": "a.src", "severity": "{severity}"}}]}}"#
        )
    }

    fn setting(cfg: &Config) -> Setting<'_> {
        Setting {
            cfg,
            not_findings: "- the decided thing".into(),
            work_dir: std::env::temp_dir().join(format!("tb-acr-run-{}", new_id())),
        }
    }

    fn go<F: Fn(Role, &str) -> Result<String> + Sync>(
        cfg: &Config,
        files: &[&str],
        fake: &Fake<F>,
    ) -> (Run, Change) {
        let s = setting(cfg);
        let c = change(files);
        let mut run = start("r", Path::new("."), "main", None, &s, &c, fake).unwrap();
        let again = c.clone();
        drive(&mut run, &s, fake, &|_| Ok(again.clone())).unwrap();
        (run, c)
    }

    fn resume<F: Fn(Role, &str) -> Result<String> + Sync>(
        run: &mut Run,
        cfg: &Config,
        c: &Change,
        fake: &Fake<F>,
        decision: Decision,
    ) -> Result<()> {
        decide(run, &decision)?;
        let s = setting(cfg);
        let again = c.clone();
        drive(run, &s, fake, &|_| Ok(again.clone()))
    }

    /// Reviewers find a defect in `lens` until it's been fixed enough times.
    fn defect_in(
        lens: &'static str,
        fixes_needed: usize,
    ) -> impl Fn(Role, &str) -> Result<String> + Sync {
        let fixes = Mutex::new(0);
        move |role, prompt| {
            Ok(match role {
                Role::Reviewer
                    if prompt.contains(lens) && *fixes.lock().unwrap() < fixes_needed =>
                {
                    ONE.to_string()
                }
                Role::Reviewer => CLEAN.to_string(),
                Role::Judge => rating("High"),
                Role::Orchestrator if prompt.contains("Fix these") => {
                    *fixes.lock().unwrap() += 1;
                    r#"{"done": "fixed it"}"#.to_string()
                }
                _ => NOTHING.to_string(),
            })
        }
    }

    #[test]
    fn a_clean_change_is_confirmed_in_one_round() {
        let cfg = plain("exception", 3, 1);
        let fake = Fake::new(|_, _: &str| Ok(CLEAN.to_string()));
        let (run, _) = go(&cfg, &["a.src"], &fake);
        assert_eq!(run.status, Status::Confirmed);
        assert_eq!(run.round, 1);
        assert!(run.lenses.values().all(|l| l.outcome == Outcome::Confirmed));
        assert_eq!(fake.count(Role::Reviewer), 2);
        assert_eq!(fake.count(Role::Judge), 0, "nothing to judge");
    }

    #[test]
    fn a_finished_review_is_noted_once_and_tidied() {
        let cfg = plain("exception", 3, 1);
        let (run, _) = go(&cfg, &["a.src"], &Fake::new(defect_in("alpha", 1)));
        let home = std::env::temp_dir().join(format!("tb-done-{}", &new_id()[14..]));
        let paths = crate::config::Paths {
            config: home.join("config.toml"),
            state: home.clone(),
        };
        let review = super::super::keep::dir(&paths).join(&run.id);
        fs::create_dir_all(&review).unwrap();
        fs::write(review.join("round-1.diff"), "+x").unwrap();
        fs::write(review.join("run.json"), "{}").unwrap();
        super::super::keep::finished(&paths, &run, true).unwrap();
        super::super::keep::finished(&paths, &run, true).unwrap();
        assert!(
            !review.join("round-1.diff").exists(),
            "a finished review needs no diffs"
        );
        assert!(review.join("run.json").exists());
        let noted = super::super::keep::history(&paths, &run.repo);
        assert_eq!(noted.len(), 1, "noted once");
        assert_eq!(noted[0].lenses["alpha"], 2);
        fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn history_is_the_rounds_finished_reviews_took() {
        let prices = "costs: {prices: {example/glm-flash: {input_per_mtok: 1, output_per_mtok: 2}, \
                      opus: {input_per_mtok: 5, output_per_mtok: 25}}}";
        let cfg = config("exception", 3, 1, "", prices);
        let fake = Fake::new(defect_in("alpha", 1));
        let (done, _) = go(&cfg, &["a.src"], &fake);
        assert_eq!(done.status, Status::Confirmed);
        let waiting = config("per-round", 3, 1, "", "");
        let (unfinished, _) = go(&waiting, &["a.src"], &Fake::new(defect_in("alpha", 9)));
        assert_eq!(unfinished.status, Status::Waiting);
        assert!(plan::Record::of(&unfinished).is_none());
        let record = plan::Record::of(&done).unwrap();
        assert_eq!(record.lenses["alpha"], 2);
        assert_eq!(record.lenses["beta"], 1);
        assert_eq!(record.fixes, 1);
        let history = plan::History::of([record]);
        assert_eq!(history.runs, 1, "only a finished review counts");
        let plan = plan::dry_run(&cfg, &change(&["a.src"]), &history).unwrap();
        let (low, expected, worst) = (
            plan.low_usd.unwrap(),
            plan.expected_usd.unwrap(),
            plan.worst_usd.unwrap(),
        );
        assert!(
            low < expected && expected < worst,
            "{low} {expected} {worst}"
        );
        assert_eq!(plan.history_runs, 1);
    }

    #[test]
    fn only_lenses_with_blockers_are_attacked_again_by_new_reviewers() {
        let cfg = plain("exception", 3, 1);
        let fake = Fake::new(defect_in("alpha", 1));
        let (run, _) = go(&cfg, &["a.src"], &fake);
        assert_eq!(run.status, Status::Confirmed);
        assert_eq!(run.round, 2);
        // Both in the first round, and only alpha in the second.
        assert_eq!(fake.count(Role::Reviewer), 3);
        assert_eq!(run.lenses["alpha"].rounds.len(), 2);
        assert_eq!(run.lenses["beta"].rounds.len(), 1);
        assert_eq!(run.lenses["alpha"].done, ["fixed it"]);
        let first = &run.lenses["alpha"].rounds[0].findings[0];
        assert_eq!(first.state, FindingState::Folded);
        let calls = fake.calls.lock().unwrap();
        let fix = calls.iter().find(|c| c.3.contains("Fix these")).unwrap();
        assert_eq!(fix.0, Role::Orchestrator);
        assert!(fix.2, "the Orchestrator's fix writes");
        let others = calls.iter().filter(|c| !c.3.contains("Fix these"));
        assert!(others.into_iter().all(|c| !c.2), "nothing else may write");
    }

    #[test]
    fn a_blocker_at_max_rounds_stops_and_escalates() {
        let cfg = plain("exception", 2, 1);
        let fake = Fake::new(defect_in("alpha", 99));
        let (mut run, c) = go(&cfg, &["a.src"], &fake);
        assert_eq!(run.status, Status::Waiting);
        assert_eq!(run.waiting, Some(Gate::Escalation));
        assert!(run.escalations[0].reason.contains("max_rounds"));
        assert_eq!(run.lenses["alpha"].outcome, Outcome::Open);
        assert_eq!(run.lenses["alpha"].rounds.len(), 2);
        let approve = Decision {
            approve: true,
            ..Decision::default()
        };
        resume(&mut run, &cfg, &c, &fake, approve).unwrap();
        assert_eq!(run.status, Status::Stopped, "seeing it doesn't pass it");
    }

    #[test]
    fn a_failed_reviewer_is_never_a_clean_pass() {
        let cfg = plain("exception", 3, 1);
        for fail in [true, false] {
            let fake = Fake::new(|role, prompt: &str| match role {
                Role::Reviewer if prompt.contains("alpha") && fail => bail!("the bridge is down"),
                Role::Reviewer if prompt.contains("alpha") => Ok("It all looks fine!".into()),
                Role::Reviewer => Ok(CLEAN.into()),
                _ => Ok(NOTHING.into()),
            });
            let (run, _) = go(&cfg, &["a.src"], &fake);
            assert_ne!(run.status, Status::Confirmed);
            assert_eq!(run.waiting, Some(Gate::Escalation));
            assert_eq!(run.lenses["alpha"].outcome, Outcome::Open);
            assert_eq!(run.lenses["beta"].outcome, Outcome::Confirmed);
            assert!(run.escalations[0].reason.contains("wasn't fully reviewed"));
        }
    }

    #[test]
    fn one_failed_reviewer_of_several_leaves_the_lens_open() {
        let cfg = plain("exception", 3, 3);
        let tries = Mutex::new(0);
        let fake = Fake::new(|role, prompt: &str| match role {
            Role::Reviewer if prompt.contains("alpha") => {
                let mut tries = tries.lock().unwrap();
                *tries += 1;
                if *tries == 2 {
                    bail!("one of three failed");
                }
                Ok(CLEAN.into())
            }
            Role::Reviewer => Ok(CLEAN.into()),
            _ => Ok(NOTHING.into()),
        });
        let (run, _) = go(&cfg, &["a.src"], &fake);
        assert_eq!(run.lenses["alpha"].outcome, Outcome::Open);
    }

    #[test]
    fn a_failed_judge_leaves_the_lens_open() {
        let cfg = plain("exception", 3, 1);
        let fake = Fake::new(|role, _: &str| match role {
            Role::Reviewer => Ok(ONE.into()),
            Role::Judge => Ok("couldn't say".into()),
            _ => Ok(NOTHING.into()),
        });
        let (run, _) = go(&cfg, &["a.src"], &fake);
        assert_eq!(run.waiting, Some(Gate::Escalation));
        assert!(run.lenses.values().all(|l| l.outcome == Outcome::Open));
    }

    #[test]
    fn fan_out_merges_into_one_result_for_the_lens() {
        let cfg = plain("exception", 3, 3);
        let fake = Fake::new(defect_in("alpha", 1));
        let (run, _) = go(&cfg, &["a.src"], &fake);
        // Three reviewers for each of two lenses, then three for alpha.
        assert_eq!(fake.count(Role::Reviewer), 9);
        let first = &run.lenses["alpha"].rounds[0];
        assert_eq!(first.reviewers.len(), 3);
        assert_eq!(first.findings.len(), 1, "the Judge merges them");
        assert!(first.reviewers.iter().all(|r| r.contains("AI agent")));
    }

    #[test]
    fn severities_do_what_they_say() {
        for (severity, status, state) in [
            ("Critical", Status::Waiting, FindingState::Open),
            ("High", Status::Waiting, FindingState::Open),
            ("Medium", Status::Waiting, FindingState::Open),
            ("Low", Status::Confirmed, FindingState::Logged),
            ("Rejected", Status::Confirmed, FindingState::Dropped),
        ] {
            let cfg = plain("per-round", 3, 1);
            let fake = Fake::new(|role, _: &str| match role {
                Role::Reviewer => Ok(ONE.into()),
                Role::Judge => Ok(rating(severity)),
                _ => Ok(NOTHING.into()),
            });
            let (run, _) = go(&cfg, &["a.src"], &fake);
            assert_eq!(run.status, status, "{severity}");
            assert_eq!(run.lenses["alpha"].rounds[0].findings[0].state, state);
        }
    }

    #[test]
    fn what_isnt_relevant_or_on_task_is_rejected_with_its_reason() {
        let cfg = plain("exception", 3, 1);
        let fake = Fake::new(|role, _: &str| match role {
            Role::Reviewer => Ok(ONE.into()),
            Role::Judge => Ok(r#"{"findings": [{"title": "t", "severity": "Critical",
                "on_task": false, "reason": "another lens's job"}]}"#
                .into()),
            _ => Ok(NOTHING.into()),
        });
        let (run, _) = go(&cfg, &["a.src"], &fake);
        assert_eq!(run.status, Status::Confirmed);
        let f = &run.lenses["alpha"].rounds[0].findings[0];
        assert_eq!(f.severity, Severity::Rejected);
        assert_eq!(f.reason, "another lens's job");
    }

    #[test]
    fn a_lens_can_change_what_a_medium_does() {
        let extra = r#"  - {id: gamma, criteria: "gamma criteria", triggers: {paths: ["**/*.src"]}, severity: {Medium: never-blocks}}"#;
        let cfg = config("exception", 3, 1, extra, "");
        let fake = Fake::new(|role, prompt: &str| match role {
            Role::Reviewer if prompt.contains("gamma") => Ok(ONE.into()),
            Role::Reviewer => Ok(CLEAN.into()),
            Role::Judge => Ok(rating("Medium")),
            _ => Ok(NOTHING.into()),
        });
        let (run, _) = go(&cfg, &["a.src"], &fake);
        assert_eq!(run.status, Status::Confirmed);
    }

    #[test]
    fn per_round_lets_a_person_decide_what_goes_in() {
        let cfg = plain("per-round", 3, 1);
        let fake = Fake::new(defect_in("alpha", 1));
        let (mut run, c) = go(&cfg, &["a.src"], &fake);
        assert_eq!(run.waiting, Some(Gate::Fold));
        let id = run.lenses["alpha"].rounds[0].findings[0].id.clone();
        let fixes = |fake: &Fake<_>| {
            let calls: &Mutex<Vec<(Role, String, bool, String)>> = &fake.calls;
            let calls = calls.lock().unwrap();
            calls.iter().filter(|c| c.2).count()
        };
        assert_eq!(fixes(&fake), 0, "nothing's fixed until they say");

        // Every blocker needs an answer, and a real one.
        let err = resume(&mut run.clone(), &cfg, &c, &fake, Decision::default()).unwrap_err();
        assert!(err.to_string().contains("--fold or --waive"), "{err}");
        let wrong = Decision {
            fold: vec![id.clone(), "no-such".into()],
            ..Decision::default()
        };
        assert!(resume(&mut run.clone(), &cfg, &c, &fake, wrong).is_err());
        // A High can't be waived.
        let waive = Decision {
            waive: vec![id.clone()],
            ..Decision::default()
        };
        let err = resume(&mut run.clone(), &cfg, &c, &fake, waive).unwrap_err();
        assert!(err.to_string().contains("can't be waived"), "{err}");

        let fold = Decision {
            fold: vec![id],
            ..Decision::default()
        };
        resume(&mut run, &cfg, &c, &fake, fold).unwrap();
        assert_eq!(run.status, Status::Confirmed);
        assert_eq!(fixes(&fake), 1);
    }

    #[test]
    fn a_medium_can_be_waived() {
        let cfg = plain("per-round", 3, 1);
        let fake = Fake::new(|role, prompt: &str| match role {
            Role::Reviewer if prompt.contains("alpha") => Ok(ONE.into()),
            Role::Reviewer => Ok(CLEAN.into()),
            Role::Judge => Ok(rating("Medium")),
            _ => Ok(NOTHING.into()),
        });
        let (mut run, c) = go(&cfg, &["a.src"], &fake);
        let id = run.lenses["alpha"].rounds[0].findings[0].id.clone();
        let waive = Decision {
            waive: vec![id],
            ..Decision::default()
        };
        resume(&mut run, &cfg, &c, &fake, waive).unwrap();
        assert_eq!(run.lenses["alpha"].outcome, Outcome::Waived);
        assert_eq!(run.status, Status::Confirmed);
    }

    #[test]
    fn the_ends_gate_asks_about_the_scope_and_the_result() {
        let cfg = plain("ends", 3, 1);
        let fake = Fake::new(|_, _: &str| Ok(CLEAN.to_string()));
        let (mut run, c) = go(&cfg, &["a.src"], &fake);
        assert_eq!(run.waiting, Some(Gate::Scope));
        assert_eq!(fake.count(Role::Reviewer), 0);
        let approve = || Decision {
            approve: true,
            ..Decision::default()
        };
        resume(&mut run, &cfg, &c, &fake, approve()).unwrap();
        assert_eq!(run.waiting, Some(Gate::Final));
        resume(&mut run, &cfg, &c, &fake, approve()).unwrap();
        assert_eq!(run.status, Status::Confirmed);
    }

    #[test]
    fn a_person_can_stop_a_review() {
        let cfg = plain("per-round", 3, 1);
        let fake = Fake::new(defect_in("alpha", 9));
        let (mut run, c) = go(&cfg, &["a.src"], &fake);
        let stop = Decision {
            stop: true,
            ..Decision::default()
        };
        resume(&mut run, &cfg, &c, &fake, stop).unwrap();
        assert_eq!(run.status, Status::Stopped);
    }

    #[test]
    fn split_reviewers_and_values_calls_escalate_in_any_mode() {
        for body in [
            r#"{"findings": [{"title": "t", "severity": "Low", "split": true,
                "sides": ["it's a bug", "it's fine"]}]}"#,
            r#"{"findings": [{"title": "t", "severity": "Rejected", "values_call": true,
                "sides": ["speed", "safety"]}]}"#,
        ] {
            let cfg = plain("exception", 3, 1);
            let fake = Fake::new(|role, _: &str| match role {
                Role::Reviewer => Ok(ONE.into()),
                Role::Judge => Ok(body.to_string()),
                _ => Ok(NOTHING.into()),
            });
            let (run, _) = go(&cfg, &["a.src"], &fake);
            assert_eq!(run.waiting, Some(Gate::Escalation));
            assert_eq!(run.disagreements[0].sides.len(), 2);
        }
    }

    #[test]
    fn a_plain_rejection_doesnt_escalate() {
        let cfg = plain("exception", 3, 1);
        let fake = Fake::new(|role, _: &str| match role {
            Role::Reviewer => Ok(ONE.into()),
            Role::Judge => Ok(rating("Rejected")),
            _ => Ok(NOTHING.into()),
        });
        let (run, _) = go(&cfg, &["a.src"], &fake);
        assert_eq!(run.status, Status::Confirmed);
        assert!(run.escalations.is_empty());
    }

    #[test]
    fn always_escalate_reaches_a_person_whatever_the_gate() {
        let top = "always_escalate: [\"a public API change\"]";
        let cfg = config("exception", 3, 1, "", top);
        let fake = Fake::new(|role, _: &str| match role {
            Role::Orchestrator => {
                Ok(r#"{"lenses": [], "escalate": ["a public API change", "made up"]}"#.into())
            }
            _ => Ok(CLEAN.into()),
        });
        let (run, _) = go(&cfg, &["a.src"], &fake);
        assert_eq!(run.waiting, Some(Gate::Escalation));
        assert_eq!(run.escalations.len(), 1, "only what the config names");
        assert!(run.escalations[0].reason.contains("public API"));
    }

    #[test]
    fn changing_the_config_always_escalates() {
        let cfg = plain("exception", 3, 1);
        let fake = Fake::new(|role, _: &str| match role {
            Role::Sizer => Ok(r#"{"tier": "big", "sure": true}"#.into()),
            _ => Ok(CLEAN.into()),
        });
        let (run, _) = go(&cfg, &[super::super::config::REPO_CONFIG], &fake);
        assert_eq!(run.waiting, Some(Gate::Escalation));
        assert!(run.escalations.iter().any(|e| e.reason.contains("edits")));
    }

    #[test]
    fn an_unsure_sizer_means_the_higher_tier() {
        let cfg = plain("exception", 3, 2);
        for (answer, tier) in [
            (
                r#"{"tier": "small", "sure": true, "reason": "tiny"}"#,
                "small",
            ),
            (r#"{"tier": "small", "sure": false}"#, "big"),
            (r#"{"tier": "made-up", "sure": true}"#, "big"),
            ("no idea", "big"),
        ] {
            let fake = Fake::new(|role, _: &str| match role {
                Role::Sizer => Ok(answer.to_string()),
                _ => Ok(CLEAN.into()),
            });
            let s = setting(&cfg);
            let c = change(&["x.other"]);
            let run = start("r", Path::new("."), "main", None, &s, &c, &fake).unwrap();
            assert_eq!(run.tier, tier, "{answer}");
        }
    }

    #[test]
    fn a_lens_only_a_description_triggers_needs_the_orchestrator() {
        let extra = r#"  - {id: gamma, criteria: "gamma criteria", triggers: {description: "when it's gamma"}}"#;
        let cfg = config("exception", 3, 1, extra, "");
        let says = |text: &'static str| {
            Fake::new(move |role, _: &str| match role {
                Role::Orchestrator => Ok(text.to_string()),
                _ => Ok(CLEAN.into()),
            })
        };
        let yes = says(r#"{"lenses": ["gamma"]}"#);
        assert!(go(&cfg, &["a.src"], &yes).0.lenses.contains_key("gamma"));
        let no = says(r#"{"lenses": []}"#);
        assert!(!go(&cfg, &["a.src"], &no).0.lenses.contains_key("gamma"));
        // If it can't say, the lens stays in, and a person hears.
        let broken = says("no idea");
        let (run, _) = go(&cfg, &["a.src"], &broken);
        assert!(run.lenses.contains_key("gamma"));
        assert_eq!(run.waiting, Some(Gate::Escalation));
    }

    #[test]
    fn a_lens_can_have_its_own_binding() {
        let extra = r#"  - {id: gamma, criteria: "gamma criteria", triggers: {paths: ["**/*.src"]}, binding: glm-flash}"#;
        let cfg = config("exception", 3, 1, extra, "");
        let fake = Fake::new(|_, _: &str| Ok(CLEAN.to_string()));
        go(&cfg, &["a.src"], &fake);
        let calls = fake.calls.lock().unwrap();
        let gamma = calls
            .iter()
            .find(|c| c.3.contains("gamma criteria"))
            .unwrap();
        assert_eq!(gamma.1, "glm-flash");
        let alpha = calls
            .iter()
            .find(|c| c.3.contains("alpha criteria"))
            .unwrap();
        assert_eq!(alpha.1, "glm-flash-high");
    }

    #[test]
    fn the_brief_and_the_judge_get_the_decided_non_issues() {
        let cfg = plain("exception", 3, 1);
        let fake = Fake::new(defect_in("alpha", 1));
        go(&cfg, &["a.src"], &fake);
        let calls = fake.calls.lock().unwrap();
        for role in [Role::Reviewer, Role::Judge] {
            let call = calls.iter().find(|c| c.0 == role).unwrap();
            assert!(call.3.contains("the decided thing"));
        }
    }

    #[test]
    fn the_report_can_be_read_by_a_machine() {
        let top = "integrations:\n  comment: full\n  labels: {not_ready: nr, ready: rd, waiting: \"wait:{lens}\", confirmed: \"ok:{lens}\"}\n\
                   costs: {prices: {example/glm-flash: {input_per_mtok: 0.1, output_per_mtok: 0.4}, \
                   opus: {input_per_mtok: 5, output_per_mtok: 25}}, \
                   per_call: {reviewer: {input_tokens: 1000, output_tokens: 500}}}";
        let cfg = config("exception", 2, 1, "", top);
        let fake = Fake::new(defect_in("alpha", 99));
        let (run, _) = go(&cfg, &["a.src"], &fake);
        let json = serde_json::to_value(report::report(&run, &cfg)).unwrap();
        assert_eq!(json["status"], "waiting");
        assert_eq!(json["waiting_for"], "escalation");
        let lenses = json["lenses"].as_array().unwrap();
        let alpha = lenses.iter().find(|l| l["id"] == "alpha").unwrap();
        assert_eq!(alpha["criteria"], "alpha criteria");
        assert_eq!(alpha["outcome"], "open");
        assert_eq!(alpha["rounds"][0]["findings"][0]["severity"], "High");
        assert_eq!(alpha["done"][0], "fixed it");
        let used = json["bindings_used"].as_array().unwrap();
        assert!(
            used.iter()
                .any(|b| b["role"] == "reviewer" && b["binding"] == "glm-flash-high")
        );
        assert!(json["tokens"].as_u64().unwrap() > 0);
        assert!(json["usd"].as_f64().unwrap() > 0.0);
        assert_eq!(json["calls_measured"], 0);
        assert_eq!(json["agent_usd"], serde_json::Value::Null);
        // A real call counts what the config says its role takes, too.
        for u in run.usage.iter().filter(|u| u.role == Role::Reviewer) {
            assert!(u.tokens_in >= 1000 && u.tokens_out >= 500, "{u:?}");
        }
        assert!(run.usage.iter().all(|u| u.usd.is_some()));
        let labels = report::labels(&run, &cfg);
        assert!(labels.contains(&"nr".to_string()));
        assert!(labels.contains(&"wait:alpha".to_string()));
        assert!(labels.contains(&"ok:beta".to_string()));
        assert!(!labels.contains(&"rd".to_string()));
        let text = report::comment(&run, &cfg).unwrap();
        assert!(text.contains("AI agent"), "{text}");
        assert!(text.contains("Escalated"), "{text}");
    }

    #[test]
    fn a_repository_that_turned_nothing_on_gets_no_labels_or_comment() {
        let cfg = plain("exception", 3, 1);
        let fake = Fake::new(|_, _: &str| Ok(CLEAN.to_string()));
        let (run, _) = go(&cfg, &["a.src"], &fake);
        assert!(report::labels(&run, &cfg).is_empty());
        assert!(report::comment(&run, &cfg).is_none());
    }

    #[test]
    fn a_benchmark_counts_caught_missed_and_false() {
        let cfg = plain("exception", 1, 1);
        let fake = Fake::new(|role, _: &str| match role {
            Role::Reviewer => Ok(ONE.into()),
            Role::Judge => Ok(r#"{"findings": [
                {"title": "off by one in the loop", "file": "a.src", "severity": "High"},
                {"title": "a phantom", "file": "a.src", "severity": "High"},
                {"title": "a nit", "severity": "Low"}]}"#
                .into()),
            _ => Ok(NOTHING.into()),
        });
        let (run, _) = go(&cfg, &["a.src"], &fake);
        let known: Vec<Known> = serde_yaml_ng::from_str(
            "- {id: k1, file: a.src, pattern: 'off by one'}\n- {id: k2, pattern: 'a leak'}\n",
        )
        .unwrap();
        let score = score(&run, &known).unwrap();
        assert_eq!(score.caught, ["k1"]);
        assert_eq!(score.missed, ["k2"]);
        assert_eq!(score.false_findings.len(), 2, "one phantom in each lens");
        assert!(score.false_findings.iter().all(|f| f.contains("phantom")));
        assert!(score.tokens > 0);
    }
}
