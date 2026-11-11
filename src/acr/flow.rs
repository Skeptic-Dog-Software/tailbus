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

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

use crate::proto::RunUsage;

use super::change::{self, Change};
use super::config::{
    Action, Binding, Config, GateMode, HumanGate, Lens, Role, Severity, Stage as Kind,
};
use super::plan::{self, Why, tokens_in};
use super::roles::{self, Raw};
use super::stages;

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
    /// The repository, which every role works in: reviewers read it, and an
    /// Orchestrator fixes it.
    pub repo: &'a Path,
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

    /// What the checkout is now, to tell whether a role that may only read
    /// changed it. None if this caller can't say.
    fn fingerprint(&self, _repo: &Path) -> Option<String> {
        None
    }

    /// The checkout as it is now, to compare later. None if this caller
    /// can't say, and every lens attacks again after a fix.
    fn snapshot(&self, _repo: &Path) -> Option<String> {
        None
    }

    /// The files that changed between two snapshots, and the diff.
    fn delta(&self, _repo: &Path, _from: &str, _to: &str) -> Option<(Vec<String>, String)> {
        None
    }
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
    /// What a stage reports of its own work, such as how many mutants the
    /// tests caught.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LensResult {
    pub id: String,
    pub criteria: String,
    pub rounds: Vec<Round>,
    /// What the Orchestrator did about its blockers.
    pub done: Vec<String>,
    pub outcome: Outcome,
    /// For a lens a later round's version of the change brought in: when,
    /// and what of it did.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub joined: String,
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
    /// It came out of a subscription: tokens count, dollars don't.
    #[serde(default)]
    pub subscription: bool,
    /// The round it was spent in.
    #[serde(default)]
    pub round: u32,
}

impl Usage {
    pub fn tokens(&self) -> u64 {
        self.tokens_in + self.cache_read + self.cache_write + self.tokens_out
    }
}

/// The dollars of the calls billed in them. None if one of those has no
/// price, or none was billed in dollars at all.
pub fn dollars(usage: &[Usage]) -> Option<f64> {
    let mut billed = usage.iter().filter(|u| !u.subscription).peekable();
    billed.peek()?;
    billed.map(|u| u.usd).sum()
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
    /// What the change says it's for, such as its pull request's
    /// description, kept so a resumed review sends the same.
    #[serde(default)]
    pub description: Option<String>,
    /// What the last steelman's fixes changed, in a review to convergence:
    /// the next round attacks it first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fix: Option<FixDelta>,
    /// This round is the full sweep: every lens that hasn't attacked the
    /// change as it is now does, before the review can converge.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub sweep: bool,
    /// Reviewed to convergence: see `Setting::converge`.
    #[serde(default)]
    pub converge: bool,
    /// What each steelman between rounds did, in order.
    #[serde(default)]
    pub steelmans: Vec<Steelman>,
}

/// A synthesis between rounds of a review to convergence: what the
/// Orchestrator was given to fold in, what it did, and what it declined and
/// why. A declined finding is a disagreement, not a resolution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Steelman {
    /// The round whose findings it folded in.
    pub round: u32,
    pub blockers: Vec<String>,
    pub done: String,
    /// What the test writer that followed it wrote, and how each test
    /// fared on the code from before the fixes and after.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pins: Vec<Pin>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub tests_done: String,
    /// Its briefs for the next round's reviewers, by lens.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub briefs: BTreeMap<String, String>,
    pub declined: Vec<roles::Declined>,
}

/// What a steelman's fixes changed: the round whose blockers they fixed,
/// and the files. The diff is kept beside the round's own.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FixDelta {
    pub round: u32,
    pub files: Vec<String>,
}

/// A test file the writer wrote between rounds/// A test file the writer wrote between rounds, and how it fared: a pin
/// fails on the code from before the fixes and passes after. None where it
/// couldn't be run, or ran past the limit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Pin {
    pub file: String,
    /// The findings it pins.
    #[serde(default)]
    pub pins: Vec<String>,
    pub fails_before: Option<bool>,
    pub passes_after: Option<bool>,
}

/// What stays the same through a review./// What stays the same through a review.
pub struct Setting<'a> {
    pub cfg: &'a Config,
    pub not_findings: String,
    /// Where a round's diff is written.
    pub work_dir: PathBuf,
    /// What the change says it's for, such as its pull request's
    /// description. Every role gets it, with the commit messages, ahead of
    /// the diff.
    pub description: Option<String>,
    /// Review to convergence, as the essay ACR comes from prescribes: build,
    /// attack, steelman, attack again, until the attackers agree. After each
    /// round with a blocker, the Orchestrator folds in the blockers the Judge
    /// upheld, and then every lens is attacked again by fresh reviewers,
    /// since a fix for one can break another. It ends when one round, on one
    /// version of the change, has no blocker in any lens.
    pub converge: bool,
    /// End the run at the first failure: a reviewer, Judge or stage that
    /// didn't finish its work, or a brief that couldn't be written. Without
    /// it, a failure is recorded and the run goes on without that work.
    pub stop_on_failure: bool,
    /// Where a reviewer whose binding can change files gets a copy of the
    /// checkout of its own, for the round. Without it, every reviewer works
    /// in the checkout.
    pub copies: Option<PathBuf>,
    /// The building session's brief for round 1's reviewers, if it wrote
    /// one. Later rounds are briefed by the steelman, which holds the
    /// change's context by then.
    pub builder_brief: Option<stages::BuilderBrief>,
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
        subscription: call.binding.subscription,
        round: 0,
    };
    match &answer.usage {
        Some(counted) => {
            usage.tokens_in = counted.input_tokens;
            usage.cache_read = counted.cache_read_tokens;
            usage.cache_write = counted.cache_write_tokens;
            usage.tokens_out = counted.output_tokens;
            usage.measured = true;
            if usage.subscription {
                return usage;
            }
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
    let mut usage = usage(cfg, call, input_bytes, answer);
    usage.round = run.round;
    run.usage.push(usage);
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
    let repo = run.repo.clone();
    let call = Call {
        role,
        binding_name: name,
        binding: s.cfg.binding(role, None)?,
        prompt,
        artifact,
        writes: false,
        repo: &repo,
    };
    let answer = caller.call(&call)?;
    record(run, s.cfg, &call, input_bytes(prompt, artifact), &answer);
    Ok(answer.text)
}

/// A round's diff, as files reviewers are sent.
pub struct Artifacts {
    /// Without generated files, which it lists.
    pub diff: PathBuf,
    /// Everything, when generated files were left out.
    pub full: Option<PathBuf>,
}

impl Artifacts {
    /// The diff a lens's reviewers get.
    fn for_lens(&self, lens: &Lens) -> &Path {
        match (&self.full, lens.include_generated) {
            (Some(full), true) => full,
            _ => &self.diff,
        }
    }
}

/// A round's change as every role gets it: what the change says it's for,
/// then its diff.
fn write_artifact(s: &Setting, run: &Run, change: &Change, round: u32) -> Result<Artifacts> {
    fs::create_dir_all(&s.work_dir)?;
    let mut intent = super::change::intent(s.description.as_deref(), &change.commits);
    if let Some(brief) = &s.builder_brief {
        intent.push_str(&stages::scope_section(brief));
    }
    // What earlier commits and the docs say is said about the change too;
    // where its tests and callers are is a lead, after it.
    if s.cfg.stages.contains(&Kind::Context) {
        let pack = stages::context_pack(&run.repo, &run.base, change);
        intent.push_str(&pack.said);
        intent.push_str(&pack.leads);
    }
    if let Some(fix) = run
        .fix
        .as_ref()
        .filter(|f| !run.sweep && round == f.round + 1)
    {
        let diff = fs::read_to_string(s.work_dir.join(format!("round-{}.fix.diff", fix.round)))
            .unwrap_or_default();
        if !diff.trim().is_empty() {
            intent.push_str(&format!(
                "# What the fixes after round {} changed\n\nThis round, attack these \
                 changes, and what they touch, first: the rest of the change was attacked \
                 before, and is attacked again before the review ends.\n\n```diff\n{}\n```\n\n",
                fix.round,
                stages::clip_text(&diff, 30 * 1024)
            ));
        }
    }
    let diff = s.work_dir.join(format!("round-{round}.diff"));
    fs::write(&diff, format!("{intent}# The diff\n\n{}", change.diff))?;
    let full = match &change.full {
        Some(text) => {
            let path = s.work_dir.join(format!("round-{round}.full.diff"));
            fs::write(&path, format!("{intent}# The diff\n\n{text}"))?;
            Some(path)
        }
        None => None,
    };
    Ok(Artifacts { diff, full })
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
    // Asked again each round, of the change as it is then, since fixes can
    // take it where a lens it didn't need at first applies. It's judged
    // afresh, without what was ruled out before, which may no longer hold.
    // A lens that has run stays on, so the version it ends at is one every
    // lens that attacked agrees on.
    let later = run.round > 1;
    let candidates: Vec<(&Lens, Why)> = plan::select(cfg, tier, change)?
        .into_iter()
        .filter(|(lens, _)| !run.lenses.contains_key(&lens.id))
        .collect();
    let described: Vec<(&str, &str)> = candidates
        .iter()
        .filter_map(|(lens, why)| match why {
            Why::Description(d) => Some((lens.id.as_str(), d.as_str())),
            _ => None,
        })
        .collect();
    // What always escalates is asked once, at the start.
    let escalating: &[String] = if later { &[] } else { &cfg.always_escalate };
    let mut wanted: Option<Vec<String>> = None;
    if !described.is_empty() || !escalating.is_empty() {
        let prompt = roles::orchestrator_prompt(&described, escalating);
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
        let joined = if later {
            let what = match &why {
                Why::Matched(what) => format!("its {what} matched"),
                Why::Required => "its tier requires it".to_string(),
                Why::Description(_) => "the Orchestrator chose it from its description".to_string(),
            };
            format!("round {}: {what}", run.round)
        } else {
            String::new()
        };
        run.lenses.insert(
            lens.id.clone(),
            LensResult {
                id: lens.id.clone(),
                criteria: lens.criteria.clone(),
                rounds: Vec::new(),
                done: Vec::new(),
                outcome: Outcome::Open,
                joined,
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
        description: s.description.clone(),
        converge: s.converge,
        steelmans: Vec::new(),
        fix: None,
        sweep: false,
    };
    let artifacts = write_artifact(s, &run, change, 0)?;
    choose_tier(&mut run, s, change, caller, &artifacts.diff)?;
    choose_lenses(&mut run, s, change, caller, &artifacts.diff)?;
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
/// What one reviewer is asked to do.
enum Brief {
    /// Review, and answer with findings.
    Review { prompt: String, repo: PathBuf },
    /// Propose mutants, which are then run against the tests in `runs`.
    Mutate {
        prompt: String,
        repo: PathBuf,
        runs: PathBuf,
        command: String,
    },
    /// Can't run, and why.
    Fail(String),
}

/// What attacking a round gives back: each lens's reviewers' findings, and
/// what its stages noted.
type Attacked = (
    BTreeMap<String, Vec<Result<Vec<Raw>>>>,
    BTreeMap<String, Vec<String>>,
);

/// Where a round's copies go, and what they're made from: the round's warm
/// copy, or the checkout.
#[derive(Clone, Copy)]
struct Copying<'a> {
    dir: Option<&'a Path>,
    source: &'a Path,
}

fn attack(
    run: &mut Run,
    s: &Setting,
    caller: &dyn Caller,
    artifacts: &Artifacts,
    change: &Change,
    copying: Copying,
    gate_text: &str,
) -> Result<Attacked> {
    let Copying { dir, source } = copying;
    let cfg = s.cfg;
    let scope = match run.head.as_deref() {
        Some(head) => format!("the change from {} to {head}", run.base),
        None => format!("the working tree against {}", run.base),
    };
    let open: Vec<String> = run
        .lenses
        .values()
        .filter(|l| l.outcome == Outcome::Open)
        .map(|l| l.id.clone())
        .collect();
    let mut notes: BTreeMap<String, Vec<String>> = BTreeMap::new();
    // Each open lens's brief for this change, from one call, before any
    // reviewer starts.
    // Round 1's lenses are briefed by the change's builder, if it wrote a
    // brief; the next round's, after a fix, by the steelman. The brief
    // stage covers any lens they left out.
    let given: BTreeMap<String, String> = if run.round == 1 {
        s.builder_brief
            .as_ref()
            .map(|b| b.lenses.clone())
            .unwrap_or_default()
    } else {
        run.steelmans
            .last()
            .filter(|m| m.round + 1 == run.round)
            .map(|m| m.briefs.clone())
            .unwrap_or_default()
    };
    let unbriefed: Vec<String> = open
        .iter()
        .filter(|id| !given.contains_key(*id))
        .cloned()
        .collect();
    let mut briefs: BTreeMap<String, String> =
        if cfg.stages.contains(&Kind::Brief) && !unbriefed.is_empty() {
            let lenses: Vec<stages::ToBrief> = unbriefed
                .iter()
                .map(|id| {
                    lens_of(cfg, id).map(|l| stages::ToBrief {
                        id: &l.id,
                        criteria: &l.criteria,
                        attack: l.attack.as_deref(),
                    })
                })
                .collect::<Result<_>>()?;
            let prompt = stages::briefer_prompt(&scope, &lenses, cfg.runbook.as_deref());
            match ask(
                run,
                s,
                caller,
                Role::Orchestrator,
                &prompt,
                Some(&artifacts.diff),
            )
            .and_then(|text| stages::read_briefs(&text))
            {
                Ok(briefs) => briefs,
                Err(e) => {
                    for id in &unbriefed {
                        notes
                            .entry(id.clone())
                            .or_default()
                            .push(format!("no brief: {e:#}"));
                    }
                    BTreeMap::new()
                }
            }
        } else {
            BTreeMap::new()
        };
    briefs.extend(given);
    let copies = dir.map(Path::to_path_buf);
    let name = run
        .repo
        .file_name()
        .map_or_else(|| "repo".into(), |n| n.to_owned());
    let command = cfg
        .tests
        .command
        .clone()
        .or_else(|| stages::test_command(&run.repo));
    let mut asks: Vec<(&Lens, Brief)> = Vec::new();
    for id in &open {
        let lens = lens_of(cfg, id)?;
        let binding = cfg.binding(Role::Reviewer, Some(lens))?;
        let own = binding.changes_files() && copies.is_some();
        let stage = stages::of_lens(&lens.id, &cfg.stages);
        // The gate ran before the reviewers.
        if stage == Some(Kind::Gate) {
            continue;
        }
        // A reviewer that can run things gets the runbook too.
        let guide = stages::guide(
            lens.attack.as_deref(),
            briefs.get(&lens.id).map(String::as_str),
            cfg.runbook.as_deref().filter(|_| own),
        ) + gate_text;
        // A mutation reviewer only proposes, so one is enough.
        let fan_out = if stage == Some(Kind::Mutation) {
            1
        } else {
            run.fan_out
        };
        for n in 1..=fan_out {
            let repo = match &copies {
                Some(dir) if own => {
                    let copy = dir.join(format!("{}-{n}", lens.id)).join(&name);
                    super::change::copy_of(source, &copy)
                        .with_context(|| format!("making {}'s reviewer a copy", lens.id))?;
                    copy
                }
                _ => run.repo.clone(),
            };
            let brief = match stage {
                None
                | Some(Kind::Context)
                | Some(Kind::Brief)
                | Some(Kind::Gate)
                | Some(Kind::TestWriting) => Brief::Review {
                    prompt: roles::reviewer_prompt(lens, &scope, &s.not_findings, own, &guide),
                    repo,
                },
                Some(Kind::Claims) => Brief::Review {
                    prompt: stages::claims_prompt(
                        &stages::claims(change, s.description.as_deref()),
                        &scope,
                        &s.not_findings,
                        own,
                        &guide,
                    ),
                    repo,
                },
                Some(Kind::Probes) if !own => Brief::Fail(
                    "the probes stage runs scenarios, so its reviewer's binding needs a shell"
                        .into(),
                ),
                Some(Kind::Probes) => Brief::Review {
                    prompt: stages::probes_prompt(
                        &scope,
                        &s.not_findings,
                        command.as_deref(),
                        &guide,
                    ),
                    repo,
                },
                Some(Kind::Mutation) => match (&copies, &command) {
                    (Some(dir), Some(command)) => Brief::Mutate {
                        prompt: stages::mutants_prompt(
                            &scope,
                            cfg.tests.mutants.unwrap_or(12),
                            &guide,
                        ),
                        repo,
                        runs: stages::mutation_dir(dir, &lens.id, n),
                        command: command.clone(),
                    },
                    (None, _) => Brief::Fail("the mutation stage needs a place for copies".into()),
                    (_, None) => Brief::Fail(
                        "the mutation stage can't tell how this repository's tests run: \
                         set tests.command in its review config"
                            .into(),
                    ),
                },
            };
            asks.push((lens, brief));
        }
    }
    let limit = std::time::Duration::from_secs(cfg.tests.timeout.unwrap_or(900));
    let parallel = cfg.tests.parallel.unwrap_or(2) as usize;
    let checkout = source;
    type Done = (Option<Usage>, Vec<Raw>, Option<String>);
    let answers: Vec<Result<Done>> = thread::scope(|scope| {
        let handles: Vec<_> = asks
            .iter()
            .map(|(lens, brief)| {
                scope.spawn(move || -> Result<Done> {
                    let artifact = artifacts.for_lens(lens);
                    let name = cfg.binding_name(Role::Reviewer, Some(lens))?;
                    let binding = cfg.binding(Role::Reviewer, Some(lens))?;
                    let (prompt, repo) = match brief {
                        Brief::Fail(why) => bail!("{why}"),
                        Brief::Review { prompt, repo } | Brief::Mutate { prompt, repo, .. } => {
                            (prompt, repo)
                        }
                    };
                    let call = Call {
                        role: Role::Reviewer,
                        binding_name: name,
                        binding,
                        prompt,
                        artifact: Some(artifact),
                        writes: false,
                        repo: repo.as_path(),
                    };
                    let answer = caller.call(&call)?;
                    let usage = usage(cfg, &call, input_bytes(prompt, Some(artifact)), &answer);
                    match brief {
                        Brief::Mutate { runs, command, .. } => {
                            let mutants = stages::read_mutants(&answer.text)?;
                            let runner = stages::Runner {
                                repo: checkout,
                                dir: runs,
                                command,
                                parallel,
                                limit,
                            };
                            let (found, summary) = stages::run_mutants(&runner, &mutants)?;
                            Ok((Some(usage), found, Some(summary)))
                        }
                        _ => Ok((Some(usage), roles::read_findings(&answer.text)?, None)),
                    }
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
    for ((lens, _), answer) in asks.iter().zip(answers) {
        let result = answer.map(|(usage, findings, note)| {
            if let Some(mut usage) = usage {
                usage.round = run.round;
                run.usage.push(usage);
            }
            if let Some(note) = note {
                notes.entry(lens.id.clone()).or_default().push(note);
            }
            findings
        });
        by_lens.entry(lens.id.clone()).or_default().push(result);
    }
    Ok((by_lens, notes))
}

/// The round's warm copy: a copy of the change with the repository's setup
/// run in it once, for every other copy this round to be made from. None,
/// with why, if there's no setup or it failed, so copies start cold.
fn warm_copy(run: &Run, s: &Setting, dir: Option<&Path>) -> (Option<PathBuf>, Option<String>) {
    let (Some(setup), Some(dir)) = (s.cfg.tests.setup.as_deref(), dir) else {
        return (None, None);
    };
    let copy = dir
        .join("warm")
        .join(run.repo.file_name().unwrap_or_default());
    let limit = std::time::Duration::from_secs(s.cfg.tests.timeout.unwrap_or(900));
    let ready = super::change::copy_of(&run.repo, &copy)
        .and_then(|_| stages::tests_pass(&copy, setup, &[], limit, &dir.join("warm.log")));
    match ready {
        Ok(Some(true)) => (Some(copy), None),
        Ok(Some(false)) => (
            None,
            Some("the setup command failed, so copies started cold".into()),
        ),
        Ok(None) => (
            None,
            Some(format!(
                "the setup command ran past {} s, so copies started cold",
                limit.as_secs()
            )),
        ),
        Err(e) => (
            None,
            Some(format!("no warm copy, so copies started cold: {e:#}")),
        ),
    }
}

/// What the test gate found this round, for the reviewers who follow it:
/// what it ran, and each gap with how it was rated.
fn gate_report(round: &Round) -> String {
    let mut out = String::from(
        "# What the test gate found this round\n\nIt ran the change's tests with coverage, \
         against mutants of its code, and other ways: don't repeat those runs.\n",
    );
    for note in &round.notes {
        out.push_str(&format!("- {note}\n"));
    }
    for f in &round.findings {
        out.push_str(&format!("- {:?}: {}\n", f.severity, f.title));
    }
    out.push('\n');
    out
}

/// The test gate's round for its lens/// The test gate's round for its lens/// The test gate's round for its lens: mutants proposed by a reviewer and
/// run with coverage against the tests, and the gaps judged.
fn gate_round(
    run: &mut Run,
    s: &Setting,
    caller: &dyn Caller,
    artifacts: &Artifacts,
    change: &Change,
    id: &str,
    copying: Copying,
) -> Result<Round> {
    let Copying { dir, source } = copying;
    let cfg = s.cfg;
    let lens = lens_of(cfg, id)?;
    let mut round = Round {
        round: run.round,
        ..Round::default()
    };
    let Some(root) = dir else {
        round
            .failures
            .push("the test gate needs a place for copies".into());
        return Ok(round);
    };
    let Some(command) = cfg
        .tests
        .command
        .clone()
        .or_else(|| stages::test_command(&run.repo))
    else {
        round.failures.push(
            "the test gate can't tell how this repository's tests run: set tests.command in \
             its review config"
                .into(),
        );
        return Ok(round);
    };
    let scope = match run.head.as_deref() {
        Some(head) => format!("the change from {} to {head}", run.base),
        None => format!("the working tree against {}", run.base),
    };
    let dir = Copies(Some(root.join("gate")));
    let guide = stages::guide(lens.attack.as_deref(), None, None);
    let prompt = stages::mutants_prompt(&scope, cfg.tests.mutants.unwrap_or(12), &guide);
    let proposed = ask(
        run,
        s,
        caller,
        Role::Reviewer,
        &prompt,
        Some(&artifacts.diff),
    )
    .and_then(|text| stages::read_mutants(&text));
    let mutants = match proposed {
        Ok(m) => m,
        Err(e) => {
            round.failures.push(format!("its mutants: {e:#}"));
            Vec::new()
        }
    };
    let runs = stages::GateRuns {
        runner: stages::Runner {
            repo: source,
            dir: dir.0.as_deref().expect("set above"),
            command: &command,
            parallel: cfg.tests.parallel.unwrap_or(2) as usize,
            limit: std::time::Duration::from_secs(cfg.tests.timeout.unwrap_or(900)),
        },
        coverage: cfg.tests.coverage.as_deref(),
        report: cfg.tests.report.as_deref().unwrap_or("coverage/lcov.info"),
        diff: &change.diff,
        variants: &cfg.tests.variants,
        repeat: cfg.tests.repeat.unwrap_or(0),
    };
    let gaps = match stages::run_gate(&runs, &mutants) {
        Ok((gaps, summary)) => {
            round.notes.push(summary);
            gaps
        }
        Err(e) => {
            round.failures.push(format!("{e:#}"));
            Vec::new()
        }
    };
    drop(dir);
    round.reviewers.push(format!(
        "The test gate, {id}: coverage and mutants run here"
    ));
    if !gaps.is_empty() {
        // Tagged, so a fact the gate measured follows its finding through
        // the Judge.
        let tagged: Vec<Raw> = gaps
            .iter()
            .enumerate()
            .map(|(i, g)| Raw {
                title: format!("[G{i}] {}", g.raw.title),
                ..g.raw.clone()
            })
            .collect();
        let judged = roles::judge_prompt(lens, &[tagged], &s.not_findings).and_then(|prompt| {
            let prompt = prompt
                + "\n\nKeep each finding's title as it's given, with its [G..] tag, and don't \
                   merge them.";
            let text = ask(
                run,
                s,
                caller,
                Role::Judge,
                &prompt,
                Some(artifacts.for_lens(lens)),
            )?;
            roles::read_ratings(&text)
        });
        match judged {
            Ok(ratings) => {
                let tag = regex::Regex::new(r"^\[G(\d+)\]").expect("a fixed pattern");
                let mut findings = rate(
                    run,
                    cfg,
                    lens,
                    ratings,
                    &stages::changed_lines(&change.diff),
                );
                for f in &mut findings {
                    let measured = tag
                        .captures(&f.title)
                        .and_then(|c| c[1].parse::<usize>().ok())
                        .and_then(|i| gaps.get(i))
                        .is_some_and(|g| g.blocks);
                    // What the gate measured blocks by rule, unless the
                    // Judge found it no defect at all.
                    if measured && f.state != FindingState::Dropped && !f.blocks() {
                        f.severity = Severity::High;
                        f.action = cfg.action(lens, Severity::High);
                        f.state = FindingState::Open;
                        f.reason = format!(
                            "{} (Blocks by rule: the gate measured it in the change's own \
                             code and tests.)",
                            f.reason
                        );
                    }
                }
                round.findings = findings;
            }
            Err(e) => round.failures.push(format!("the Judge: {e:#}")),
        }
    }
    Ok(round)
}

/// The test-writing round between rounds: a fresh writer pins the blockers
/// just fixed, and each test it names is run on the code from before the
/// fixes, where it must fail, and after, where it must pass. A test that
/// passes before pins nothing, and a person hears so.
fn write_tests(
    run: &mut Run,
    s: &Setting,
    caller: &dyn Caller,
    blockers: &str,
    intent: &str,
    before: Option<&Path>,
) -> (Vec<Pin>, String) {
    let cfg = s.cfg;
    let prompt = stages::writer_prompt(intent, blockers, cfg.runbook.as_deref());
    let written = (|| -> Result<stages::Written> {
        let name = cfg.binding_name(Role::Orchestrator, None)?;
        let repo = run.repo.clone();
        let call = Call {
            role: Role::Orchestrator,
            binding_name: name,
            binding: cfg.binding(Role::Orchestrator, None)?,
            prompt: &prompt,
            artifact: None,
            writes: true,
            repo: &repo,
        };
        let answer = caller.call(&call)?;
        record(run, cfg, &call, prompt.len(), &answer);
        stages::read_written(&answer.text)
    })();
    let written = match written {
        Ok(w) => w,
        Err(e) => {
            escalate(
                run,
                format!("the test writer after round {}: {e:#}", run.round),
            );
            return (Vec::new(), String::new());
        }
    };
    let command = cfg
        .tests
        .command
        .clone()
        .or_else(|| stages::test_command(&run.repo));
    let limit = std::time::Duration::from_secs(cfg.tests.timeout.unwrap_or(900));
    // The code after the fixes and the new tests, in a copy, so running the
    // tests leaves nothing in the checkout.
    let after = Copies(
        s.copies
            .as_ref()
            .map(|root| root.join(format!("{}-r{}-after", run.id, run.round))),
    );
    let after_copy = after.0.as_ref().and_then(|dir| {
        let copy = dir.join(run.repo.file_name().unwrap_or_default());
        super::change::copy_of(&run.repo, &copy).ok().map(|_| copy)
    });
    let mut pins = Vec::new();
    for t in &written.tests {
        let mut pin = Pin {
            file: t.file.clone(),
            pins: t.pins.clone(),
            fails_before: None,
            passes_after: None,
        };
        if let (Some(command), true) = (&command, run.repo.join(&t.file).is_file()) {
            let files = [t.file.clone()];
            if let Some(before) = before {
                let to = before.join(&t.file);
                let copied = to
                    .parent()
                    .map_or(Ok(()), fs::create_dir_all)
                    .and_then(|_| fs::copy(run.repo.join(&t.file), &to).map(|_| ()));
                if copied.is_ok() {
                    let log = before.with_extension("before.log");
                    pin.fails_before = stages::tests_pass(before, command, &files, limit, &log)
                        .ok()
                        .flatten()
                        .map(|passed| !passed);
                }
            }
            if let Some(after) = &after_copy {
                let log = after.with_extension("after.log");
                pin.passes_after = stages::tests_pass(after, command, &files, limit, &log)
                    .ok()
                    .flatten();
            }
        }
        if pin.fails_before == Some(false) {
            escalate(
                run,
                format!(
                    "{} pins nothing: it passes on the code from before round {}'s fixes",
                    pin.file, run.round
                ),
            );
        }
        if pin.passes_after == Some(false) {
            escalate(
                run,
                format!(
                    "{} fails on the code after round {}'s fixes",
                    pin.file, run.round
                ),
            );
        }
        pins.push(pin);
    }
    (pins, written.done)
}

/// A round's reviewer copies, removed when it's dropped.
struct Copies(Option<PathBuf>);

impl Drop for Copies {
    fn drop(&mut self) {
        if let Some(dir) = &self.0 {
            let _ = fs::remove_dir_all(dir);
        }
    }
}

/// How long a reviewer's copy may outlive its round: only one whose review
/// was killed is left past it.
const COPY_HOURS: u64 = 24;

/// Removes the copies of killed reviews.
fn prune_copies(root: &Path) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    let limit = std::time::Duration::from_secs(COPY_HOURS * 3600);
    for entry in entries.flatten() {
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > limit);
        if old {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
}

/// Turns a Judge's ratings into a lens's findings for this round.
/// What blocks, decided by fixed rules from the facts the Judge states, not
/// by how a prompt words them. Rules only raise or route: a finding no rule
/// covers keeps the Judge's severity, so a missing rule never silences one.
/// Only two lower it, both explicit:
/// - not a real defect, or a break that changes nothing: dropped;
/// - there before the change, which the change neither claims to fix nor
///   makes worse: logged, never blocking. A Critical one also goes to a
///   person, so it's never silent.
///
/// And four raise it to High at least: a test the change adds that can't
/// fail when what it pins breaks; a claim that would lead someone to ship a
/// defect; a pre-existing defect the change claims to fix or makes worse;
/// and a defect of the change's own that meets the lens's definition of a
/// blocker, which the repository's calibration set.
/// A real finding outside the reviewer's own lens is rated like any other.
pub(super) fn ruling(cfg: &Config, lens: &Lens, r: &roles::Rating) -> Ruled {
    if !r.relevant || r.equivalent || r.severity == Severity::Rejected {
        return Ruled::plain(Severity::Rejected, Action::Drop);
    }
    if r.weak_pin
        || r.misleads
        || (r.pre_existing && (r.claims_fix || r.worsens))
        || (r.meets_blocker && !r.pre_existing)
    {
        let severity = if r.severity == Severity::Critical {
            Severity::Critical
        } else {
            Severity::High
        };
        return Ruled::plain(severity, cfg.action(lens, severity));
    }
    if r.pre_existing {
        return Ruled {
            severity: Severity::Low,
            action: Action::NeverBlocks,
            escalate: r.severity == Severity::Critical,
        };
    }
    Ruled::plain(r.severity, cfg.action(lens, r.severity))
}

/// The text of the rules that decide what blocks: a calibration made before
/// they changed is stale.
pub(super) fn rules_source() -> String {
    let flow = super::calibration::source_of(
        include_str!("flow.rs"),
        &["pub(super) fn ruling(", "pub(super) fn grounded("],
    );
    let stages = super::calibration::source_of(
        include_str!("stages.rs"),
        &[
            "pub fn changed_lines(",
            "pub fn adds_text(",
            "pub fn quotes_text(",
            "fn quote_parts(",
        ],
    );
    format!("{flow}\n{stages}")
}

/// A ruling: the severity and action rules give a finding, and whether it
/// also goes to a person.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct Ruled {
    pub severity: Severity,
    pub action: Action,
    pub escalate: bool,
}

impl Ruled {
    fn plain(severity: Severity, action: Action) -> Ruled {
        Ruled {
            severity,
            action,
            escalate: false,
        }
    }
}

/// Facts the diff and the config settle, whatever the Judge said: a finding
/// in a line the change adds isn't pre-existing; a weak pin or a misleading
/// claim holds only when its quote is something the change adds; and a
/// finding meets the lens's definition of a blocker only when its quote is
/// words of that definition.
pub(super) fn grounded(
    mut r: roles::Rating,
    changed: &stages::ChangedLines,
    blocker: Option<&str>,
) -> roles::Rating {
    if let (Some(file), Some(line)) = (r.file.as_deref(), r.line)
        && changed
            .get(file)
            .is_some_and(|lines| lines.contains_key(&line))
    {
        r.pre_existing = false;
    }
    let quoted = |quotes: &[String]| quotes.iter().any(|q| stages::adds_text(changed, q));
    r.weak_pin = r.weak_pin && quoted(&r.weak_pin_test);
    r.misleads = r.misleads && quoted(&r.misleading_claim);
    r.meets_blocker = r.meets_blocker
        && blocker.is_some_and(|b| r.blocker_clause.iter().any(|q| stages::quotes_text(b, q)));
    r
}

fn rate(
    run: &mut Run,
    cfg: &Config,
    lens: &Lens,
    ratings: Vec<roles::Rating>,
    changed: &stages::ChangedLines,
) -> Vec<Finding> {
    let mut findings = Vec::new();
    for (n, rating) in ratings.into_iter().enumerate() {
        let rating = grounded(rating, changed, lens.blocker.as_deref());
        let Ruled {
            severity,
            action,
            escalate: to_person,
        } = ruling(cfg, lens, &rating);
        let state = match action {
            Action::Blocks | Action::BlocksUnlessWaived => FindingState::Open,
            Action::NeverBlocks => FindingState::Logged,
            Action::Drop => FindingState::Dropped,
        };
        let id = format!("{}-r{}-{}", lens.id, run.round, n + 1);
        if to_person {
            escalate(
                run,
                format!(
                    "{id} is Critical and was there before the change, which neither claims to \
                     fix it nor makes it worse, so it doesn't block: {}",
                    rating.title
                ),
            );
        }
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
            // Recorded, never lowered: a real defect blocks whichever lens
            // finds it.
            reason: if rating.on_task {
                rating.reason
            } else {
                format!("{} (outside this lens's criteria)", rating.reason)
            },
            state,
        });
    }
    findings
}

/// One round: attack every open lens, judge what comes back, and settle
/// what's next. Judging stays out of the orchestration's context.
fn review_round(run: &mut Run, s: &Setting, change: &Change, caller: &dyn Caller) -> Result<()> {
    let cfg = s.cfg;
    let artifacts = write_artifact(s, run, change, run.round)?;
    if run.round > 1 {
        choose_lenses(run, s, change, caller, &artifacts.diff)?;
    }
    // Only an Orchestrator may change the checkout.
    let before = caller.fingerprint(&run.repo);
    // This round's copies, gone when it ends, however it ends, made from
    // one copy set up once: the package install and a first build.
    let round_dir = Copies(
        s.copies
            .as_ref()
            .map(|root| root.join(format!("{}-r{}", run.id, run.round))),
    );
    if let Some(root) = &s.copies {
        prune_copies(root);
    }
    let (warm, warm_note) = warm_copy(run, s, round_dir.0.as_deref());
    let source = warm.clone().unwrap_or_else(|| run.repo.clone());
    let copying = Copying {
        dir: round_dir.0.as_deref(),
        source: &source,
    };
    // What the gate found, for every reviewer this round.
    let mut gate_text = String::new();
    // The test gate first. A gap that blocks ends the round before any
    // reviewer runs only if `tests.gate` is `stop`.
    if let Some(id) = stages::lens_id(Kind::Gate).filter(|id| {
        cfg.stages.contains(&Kind::Gate)
            && run
                .lenses
                .get(*id)
                .is_some_and(|l| l.outcome == Outcome::Open)
    }) {
        let mut round = gate_round(run, s, caller, &artifacts, change, id, copying)?;
        round.notes.extend(warm_note.clone());
        let blocks = round
            .findings
            .iter()
            .any(|f| f.blocks() && f.state == FindingState::Open);
        for failure in &round.failures {
            escalate(
                run,
                format!(
                    "{id} wasn't fully checked in round {}: {failure}",
                    run.round
                ),
            );
        }
        gate_text = gate_report(&round);
        let lens = run.lenses.get_mut(id).expect("open");
        if round.failures.is_empty() && !blocks {
            lens.outcome = Outcome::Confirmed;
        }
        lens.rounds.push(round);
        if blocks && cfg.tests.gate.unwrap_or_default() == GateMode::Stop {
            settle(run, cfg);
            return Ok(());
        }
    }
    let (reviews, mut notes) = attack(run, s, caller, &artifacts, change, copying, &gate_text)?;
    if let Some(note) = &warm_note {
        for id in reviews.keys() {
            notes.entry(id.clone()).or_default().push(note.clone());
        }
    }
    // Each lens's reviewers, then its Judge: the Judges all at once, since
    // each sees only its own lens.
    let mut lenses: Vec<(String, &Lens, Round, Vec<Vec<Raw>>)> = Vec::new();
    for (id, results) in reviews {
        let lens = lens_of(cfg, &id)?;
        let name = cfg.binding_name(Role::Reviewer, Some(lens))?;
        let mut round = Round {
            round: run.round,
            notes: notes.remove(&id).unwrap_or_default(),
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
        lenses.push((id, lens, round, lists));
    }
    let judge_name = cfg.binding_name(Role::Judge, None)?;
    let judge = cfg.binding(Role::Judge, None)?;
    let repo = run.repo.clone();
    let artifacts = &artifacts;
    // A lens's Judge's answer, if it had findings to judge.
    type Judged = Option<Result<(Usage, Vec<roles::Rating>)>>;
    let judged: Vec<Judged> = thread::scope(|scope| {
        let handles: Vec<_> = lenses
            .iter()
            .map(|(_, lens, _, lists)| {
                let repo = repo.as_path();
                lists.iter().any(|l| !l.is_empty()).then(|| {
                    scope.spawn(move || -> Result<(Usage, Vec<roles::Rating>)> {
                        let prompt = roles::judge_prompt(lens, lists, &s.not_findings)?;
                        let diff = artifacts.for_lens(lens);
                        let call = Call {
                            role: Role::Judge,
                            binding_name: judge_name,
                            binding: judge,
                            prompt: &prompt,
                            artifact: Some(diff),
                            writes: false,
                            repo,
                        };
                        let answer = caller.call(&call)?;
                        let usage = usage(cfg, &call, input_bytes(&prompt, Some(diff)), &answer);
                        Ok((usage, roles::read_ratings(&answer.text)?))
                    })
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| {
                h.map(|h| {
                    h.join()
                        .unwrap_or_else(|_| Err(anyhow!("a Judge's thread died")))
                })
            })
            .collect()
    });
    let changed = before.is_some() && caller.fingerprint(&run.repo) != before;
    let lines = stages::changed_lines(&change.diff);
    for ((id, lens, mut round, _), judged) in lenses.into_iter().zip(judged) {
        match judged {
            Some(Ok((mut usage, ratings))) => {
                usage.round = run.round;
                run.usage.push(usage);
                round.findings = rate(run, cfg, lens, ratings, &lines);
            }
            Some(Err(e)) => round.failures.push(format!("the Judge: {e:#}")),
            None => {}
        }
        if changed {
            round.failures.push(
                "the repository changed during the reviewers' or the Judges' turn, and only an \
                 Orchestrator may change it"
                    .into(),
            );
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
    // In a review to convergence, nothing blocks now, but a lens that
    // hasn't attacked the change as it is now must, before it can end: the
    // full sweep.
    if open.is_empty() && run.converge && run.round < cfg.max_rounds {
        let fixed_after = run.steelmans.last().map_or(0, |m| m.round);
        let stale: Vec<String> = run
            .lenses
            .values()
            .filter(|l| {
                l.outcome == Outcome::Confirmed
                    && fixed_after > 0
                    && l.rounds.last().is_none_or(|r| r.round <= fixed_after)
            })
            .map(|l| l.id.clone())
            .collect();
        if !stale.is_empty() {
            for id in stale {
                if let Some(l) = run.lenses.get_mut(&id) {
                    l.outcome = Outcome::Open;
                }
            }
            run.sweep = true;
            run.round += 1;
            run.stage = Stage::Review;
            return;
        }
    }
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

/// Has the Orchestrator fix what's been folded in: only the blockers, or in
/// a review to convergence, a steelman of everything the Judge upheld.
fn fix(
    run: &mut Run,
    s: &Setting,
    caller: &dyn Caller,
    reload: &dyn Fn(bool) -> Result<Change>,
) -> Result<()> {
    // Each lens's latest round, if no steelman has had it yet.
    let since = run.steelmans.last().map_or(0, |m| m.round);
    let latest = |state: FindingState| -> Vec<(String, Finding)> {
        run.lenses
            .values()
            .filter_map(|l| l.latest().map(|r| (l.id.clone(), r)))
            .filter(|(_, r)| r.round > since)
            .flat_map(|(id, r)| {
                r.findings
                    .iter()
                    .filter(move |f| f.state == state)
                    .map(move |f| (id.clone(), f.clone()))
            })
            .collect()
    };
    // Only blockers: what's upheld without blocking goes to a person, since
    // folding it in grows the change, and gives the next round more to attack.
    let folded = latest(FindingState::Folded);
    let list = |findings: &[(String, Finding)]| -> String {
        findings
            .iter()
            .map(|(_, f)| {
                let at = f.file.as_deref().unwrap_or("somewhere in the change");
                format!("- {} ({at}): {}\n  {}\n", f.id, f.title, f.detail)
            })
            .collect()
    };
    let writing = run.converge && s.cfg.stages.contains(&Kind::TestWriting);
    let intent = if run.converge {
        let fixed = run.lenses.values().any(|l| !l.done.is_empty());
        let change = reload(fixed)?;
        super::change::intent(s.description.as_deref(), &change.commits)
    } else {
        String::new()
    };
    let prompt = if run.converge {
        let earlier: String = run
            .steelmans
            .iter()
            .map(|m| format!("- After round {}: {}\n", m.round, m.done))
            .collect();
        {
            let lenses: String = run
                .lenses
                .keys()
                .filter_map(|id| s.cfg.lenses.iter().find(|l| &l.id == id))
                .map(|l| format!("- {}: {}\n", l.id, l.criteria.trim()))
                .collect();
            roles::steelman_prompt(&intent, &earlier, &list(&folded), writing, &lenses)
        }
    } else {
        roles::fix_prompt(&list(&folded))
    };
    // The code as it is before the fixes, for the pins to fail on.
    let before = Copies(
        s.copies
            .as_ref()
            .filter(|_| writing)
            .map(|root| root.join(format!("{}-r{}-before", run.id, run.round))),
    );
    let snapshot = before.0.as_ref().and_then(|dir| {
        let copy = dir.join(run.repo.file_name().unwrap_or_default());
        super::change::copy_of(&run.repo, &copy).ok().map(|_| copy)
    });
    let cfg = s.cfg;
    let name = cfg.binding_name(Role::Orchestrator, None)?;
    let repo = run.repo.clone();
    let call = Call {
        role: Role::Orchestrator,
        binding_name: name,
        binding: cfg.binding(Role::Orchestrator, None)?,
        prompt: &prompt,
        artifact: None,
        writes: true,
        repo: &repo,
    };
    // The checkout before the fixes, to see what they changed.
    let before_fix = run.converge.then(|| caller.snapshot(&run.repo)).flatten();
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
            if run.converge {
                let lens_of = |finding: &str| {
                    folded
                        .iter()
                        .find(|(_, f)| f.id == finding)
                        .map(|(lens, _)| lens.clone())
                };
                for d in &fixed.declined {
                    run.disagreements.push(Disagreement {
                        lens: lens_of(&d.finding).unwrap_or_default(),
                        finding: d.finding.clone(),
                        sides: vec![
                            "The Judge upheld it.".into(),
                            format!("The steelman declined it: {}", d.reason),
                        ],
                    });
                }
                run.steelmans.push(Steelman {
                    round: run.round,
                    blockers: folded.iter().map(|(_, f)| f.id.clone()).collect(),
                    done: fixed.done.clone(),
                    declined: fixed.declined.clone(),
                    pins: Vec::new(),
                    tests_done: String::new(),
                    briefs: fixed.briefs.clone(),
                });
                if writing {
                    let (pins, done) =
                        write_tests(run, s, caller, &list(&folded), &intent, snapshot.as_deref());
                    if let Some(m) = run.steelmans.last_mut() {
                        m.pins = pins;
                        m.tests_done = done;
                    }
                }
                // The change is a new version now. A lens attacks it again
                // if the fixes touched what it covers, with the fixes as its
                // focus; the rest wait for the sweep before it can end.
                let delta = before_fix.as_deref().and_then(|from| {
                    let to = caller.snapshot(&run.repo)?;
                    caller.delta(&run.repo, from, &to)
                });
                run.sweep = false;
                match delta {
                    Some((files, diff)) => {
                        let _ = fs::write(
                            s.work_dir.join(format!("round-{}.fix.diff", run.round)),
                            &diff,
                        );
                        let touched = Change {
                            files: files.clone(),
                            diff,
                            ..Change::default()
                        };
                        run.fix = Some(FixDelta {
                            round: run.round,
                            files,
                        });
                        for lens in run.lenses.values_mut() {
                            let applies = cfg
                                .lenses
                                .iter()
                                .find(|l| l.id == lens.id)
                                .and_then(|l| plan::lens_applies(l, &touched).ok().flatten())
                                .is_some();
                            if lens.outcome == Outcome::Confirmed && applies {
                                lens.outcome = Outcome::Open;
                            }
                        }
                    }
                    None => {
                        run.fix = None;
                        for lens in run.lenses.values_mut() {
                            if lens.outcome == Outcome::Confirmed {
                                lens.outcome = Outcome::Open;
                            }
                        }
                    }
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
        let known = failures(run).len();
        let stepped = step(run, s, caller, reload)?;
        if s.stop_on_failure {
            let now = failures(run);
            if now.len() > known {
                run.status = Status::Stopped;
                bail!(
                    "stopped at the first failure, as asked:\n- {}",
                    now[known..].join("\n- ")
                );
            }
        }
        if !stepped {
            return Ok(());
        }
    }
}

/// Everything that didn't finish its work in a run so far, lens by lens and
/// round by round: what `stop_on_failure` stops at.
pub fn failures(run: &Run) -> Vec<String> {
    let mut out = Vec::new();
    for lens in run.lenses.values() {
        for round in &lens.rounds {
            let notes = round.notes.iter().filter(|n| n.starts_with("no brief:"));
            for f in round.failures.iter().chain(notes) {
                out.push(format!("{}, round {}: {f}", lens.id, round.round));
            }
        }
    }
    out
}

/// One step of a run: false once it waits for a person or is done.
fn step(
    run: &mut Run,
    s: &Setting,
    caller: &dyn Caller,
    reload: &dyn Fn(bool) -> Result<Change>,
) -> Result<bool> {
    if unseen(run) {
        pause(run, Gate::Escalation);
        return Ok(false);
    }
    match run.stage {
        Stage::Scope => {
            pause(run, Gate::Scope);
            return Ok(false);
        }
        Stage::Review => {
            let fixed = run.lenses.values().any(|l| !l.done.is_empty());
            review_round(run, s, &reload(fixed)?, caller)?;
        }
        Stage::Decide => {
            pause(run, Gate::Fold);
            return Ok(false);
        }
        Stage::Fix => fix(run, s, caller, reload)?,
        Stage::Final => {
            pause(run, Gate::Final);
            return Ok(false);
        }
        Stage::Done => {
            run.status = if all_settled(run) && !run.lenses.is_empty() {
                Status::Confirmed
            } else {
                Status::Stopped
            };
            return Ok(false);
        }
    }
    Ok(true)
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
        usd: dollars(&run.usage),
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
        let mut cfg = resolve(&user, parse_repo(&text).unwrap(), &Flags::default()).unwrap();
        // These tests count lenses and calls: the universal lens, which
        // every real config gets, is tested on its own.
        cfg.lenses
            .retain(|l| l.id != super::super::config::ELEGANCE);
        cfg
    }

    fn plain(gate: &str, rounds: u32, fan_out: u32) -> Config {
        config(gate, rounds, fan_out, "", "")
    }

    fn change(files: &[&str]) -> Change {
        Change {
            files: files.iter().map(|f| f.to_string()).collect(),
            diff: "+let x = 1;\n".to_string(),
            ..Change::default()
        }
    }

    /// Answers each call from a function, and keeps a record of the calls.
    struct Fake<F> {
        answer: F,
        calls: Mutex<Vec<(Role, String, bool, String)>>,
        /// What every call says it used, as a bridge with `usage` would.
        usage: Option<RunUsage>,
        /// A role whose calls change the checkout, which none may but an
        /// Orchestrator fixing.
        tamper: Option<Role>,
        changes: std::sync::atomic::AtomicU32,
        /// The roles that got the change as a file.
        with_diff: Mutex<Vec<Role>>,
        /// What each of them got in it.
        sent: Mutex<Vec<(Role, String)>>,
        /// Where each call worked, and whether the change's file was there.
        repos: Mutex<Vec<(Role, PathBuf, bool)>>,
        /// Whether each call's working directory held what a setup leaves.
        warmed: Mutex<Vec<bool>>,
        /// What a fix changes, as the checkout's snapshots would say.
        fixes: Option<Vec<String>>,
    }

    impl<F: Fn(Role, &str) -> Result<String> + Sync> Fake<F> {
        fn new(answer: F) -> Fake<F> {
            Fake {
                answer,
                calls: Mutex::new(Vec::new()),
                usage: None,
                tamper: None,
                changes: Default::default(),
                with_diff: Mutex::new(Vec::new()),
                sent: Mutex::new(Vec::new()),
                repos: Mutex::new(Vec::new()),
                warmed: Mutex::new(Vec::new()),
                fixes: None,
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
            if let Some(path) = call.artifact {
                self.with_diff.lock().unwrap().push(call.role);
                let text = fs::read_to_string(path).unwrap_or_default();
                self.sent.lock().unwrap().push((call.role, text));
            }
            self.repos.lock().unwrap().push((
                call.role,
                call.repo.to_path_buf(),
                call.repo.join("a.src").is_file(),
            ));
            if call.role == Role::Reviewer {
                self.warmed
                    .lock()
                    .unwrap()
                    .push(call.repo.join("warmed").is_file());
            }
            if self.tamper == Some(call.role) {
                self.changes
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
            let text = (self.answer)(call.role, call.prompt)?;
            Ok(Answer {
                text,
                run_id: new_id(),
                usage: self.usage.clone(),
            })
        }

        fn snapshot(&self, _repo: &Path) -> Option<String> {
            self.fixes.as_ref().map(|_| new_id())
        }

        fn delta(&self, _repo: &Path, _from: &str, _to: &str) -> Option<(Vec<String>, String)> {
            self.fixes
                .clone()
                .map(|files| (files, "+the fix\n".to_string()))
        }

        fn fingerprint(&self, _repo: &Path) -> Option<String> {
            self.tamper.map(|_| {
                self.changes
                    .load(std::sync::atomic::Ordering::SeqCst)
                    .to_string()
            })
        }
    }

    #[test]
    fn a_call_its_agent_counted_is_recorded_as_counted() {
        let prices = "costs: {prices: {example/small-model: {input_per_mtok: 1, output_per_mtok: 2, cached_input_per_mtok: 0.1}, \
                      example/medium-model: {input_per_mtok: 1, output_per_mtok: 2, cached_input_per_mtok: 0.1}, \
                      example/large-model: {input_per_mtok: 1, output_per_mtok: 2, cached_input_per_mtok: 0.1}}, \
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
        // On a subscription, neither the config's price nor the agent's
        // counts: only tokens.
        let mut subscribed = cfg.clone();
        for b in subscribed.allowed_bindings.values_mut() {
            b.subscription = true;
        }
        let (run, _) = go(&subscribed, &["a.src"], &fake);
        assert!(
            run.usage
                .iter()
                .all(|u| u.subscription && u.usd.is_none() && u.agent_usd.is_none())
        );
        assert!(run.usage.iter().all(|u| u.tokens() == 3_000_000));
        let json = serde_json::to_value(report::report(&run, &subscribed)).unwrap();
        assert_eq!(json["usd"], serde_json::Value::Null);
        assert_eq!(json["calls_on_subscription"], run.usage.len());
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
            description: None,
            converge: false,
            stop_on_failure: false,
            copies: None,
            builder_brief: None,
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
    fn a_lens_that_takes_generated_files_gets_the_whole_diff() {
        let cfg = plain("exception", 1, 1);
        let s = setting(&cfg);
        let mut c = change(&["a.src"]);
        let fake = Fake::new(defect_in("alpha", 99));
        let run = start("r", Path::new("."), "main", None, &s, &c, &fake).unwrap();
        let lean = write_artifact(&s, &run, &c, 1).unwrap();
        assert!(lean.full.is_none());
        c.full = Some("the whole diff".into());
        let both = write_artifact(&s, &run, &c, 2).unwrap();
        let mut lens = cfg.lenses[0].clone();
        assert_eq!(both.for_lens(&lens), both.diff.as_path());
        lens.include_generated = true;
        let whole = both.for_lens(&lens);
        assert!(
            fs::read_to_string(whole)
                .unwrap()
                .ends_with("# The diff\n\nthe whole diff")
        );
    }

    #[test]
    fn every_role_gets_what_the_change_says_its_for_ahead_of_the_diff() {
        let cfg = plain("exception", 1, 1);
        let mut s = setting(&cfg);
        s.description = Some("Fixes B-1: an edited reminder keeps the user's date.".into());
        let mut c = change(&["a.src"]);
        c.commits = vec![super::super::change::Commit {
            id: "abc1234".into(),
            message: "Keep the edited date\n\nThe save path dropped it.".into(),
        }];
        let fake = Fake::new(defect_in("alpha", 0));
        let mut run = start("r", Path::new("."), "main", None, &s, &c, &fake).unwrap();
        let again = c.clone();
        drive(&mut run, &s, &fake, &|_| Ok(again.clone())).unwrap();
        // Kept, so a resumed review sends the same.
        assert_eq!(run.description, s.description);
        let sent = fake.sent.lock().unwrap();
        assert!(sent.iter().any(|(r, _)| *r == Role::Reviewer));
        for (role, text) in sent.iter() {
            assert!(
                text.starts_with(super::super::change::SAID),
                "{role:?}: {text}"
            );
            let said = text
                .find("an edited reminder keeps the user's date")
                .unwrap();
            let commit = text.find("Keep the edited date").unwrap();
            let diff = text.find("# The diff\n\n+let x = 1;").unwrap();
            assert!(said < commit && commit < diff, "{role:?}: {text}");
        }
    }

    #[test]
    fn a_judge_gets_the_diff_and_may_not_change_the_checkout() {
        let cfg = plain("exception", 3, 1);
        let fake = Fake::new(defect_in("alpha", 9));
        let (run, _) = go(&cfg, &["a.src"], &fake);
        assert!(fake.with_diff.lock().unwrap().contains(&Role::Judge));
        assert!(
            run.escalations
                .iter()
                .all(|e| !e.reason.contains("changed"))
        );
        // A Judge that writes to the repository, rather than its scratch
        // space, fails the round and escalates.
        let mut tampering = Fake::new(defect_in("alpha", 9));
        tampering.tamper = Some(Role::Judge);
        let (run, _) = go(&cfg, &["a.src"], &tampering);
        let alpha = &run.lenses["alpha"];
        assert_eq!(alpha.outcome, Outcome::Open);
        assert!(
            alpha.rounds[0]
                .failures
                .iter()
                .any(|f| f.contains("only an Orchestrator"))
        );
        assert!(
            run.escalations
                .iter()
                .any(|e| e.reason.contains("only an Orchestrator"))
        );
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
        assert!(!review.exists(), "a finished review leaves nothing behind");
        let noted = super::super::keep::history(&paths, &run.repo);
        assert_eq!(noted.len(), 1, "noted once");
        assert_eq!(noted[0].lenses["alpha"], 2);
        fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn history_is_the_rounds_finished_reviews_took() {
        let prices = "costs: {prices: {example/small-model: {input_per_mtok: 1, output_per_mtok: 2}, \
                      example/medium-model: {input_per_mtok: 1, output_per_mtok: 2}, \
                      example/large-model: {input_per_mtok: 1, output_per_mtok: 2}}}";
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
            plan.least.usd.unwrap(),
            plan.expected.unwrap().usd.unwrap(),
            plan.most.usd.unwrap(),
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

    /// A review to convergence of a change to a.src.
    fn converging<F: Fn(Role, &str) -> Result<String> + Sync>(cfg: &Config, fake: &Fake<F>) -> Run {
        let mut s = setting(cfg);
        s.converge = true;
        s.description = Some("Makes x one.".into());
        let c = change(&["a.src"]);
        let mut run = start("r", Path::new("."), "main", None, &s, &c, fake).unwrap();
        let again = c.clone();
        drive(&mut run, &s, fake, &|_| Ok(again.clone())).unwrap();
        run
    }

    #[test]
    fn a_review_to_convergence_attacks_every_lens_after_each_steelman() {
        let cfg = plain("exception", 20, 1);
        let fixes = Mutex::new(0);
        let fake = Fake::new(|role, prompt: &str| {
            Ok(match role {
                Role::Reviewer if prompt.contains("alpha") && *fixes.lock().unwrap() < 2 => {
                    ONE.to_string()
                }
                Role::Reviewer => CLEAN.to_string(),
                Role::Judge => rating("High"),
                Role::Orchestrator if prompt.contains("You synthesize") => {
                    *fixes.lock().unwrap() += 1;
                    r#"{"done": "made it stronger"}"#.to_string()
                }
                _ => NOTHING.to_string(),
            })
        });
        let run = converging(&cfg, &fake);
        assert_eq!(run.status, Status::Confirmed);
        assert_eq!(run.round, 3);
        // beta had nothing to fix, but the change changed under it twice.
        assert_eq!(fake.count(Role::Reviewer), 6);
        assert_eq!(run.lenses["beta"].rounds.len(), 3);
        assert_eq!(run.steelmans.len(), 2);
        assert_eq!(run.steelmans[0].blockers, ["alpha-r1-1"]);
        assert_eq!(run.steelmans[1].round, 2);
        let calls = fake.calls.lock().unwrap();
        let steelmans: Vec<_> = calls
            .iter()
            .filter(|c| c.3.contains("You synthesize"))
            .collect();
        assert_eq!(steelmans.len(), 2);
        assert!(
            steelmans.iter().all(|c| c.0 == Role::Orchestrator && c.2),
            "it writes"
        );
        // It holds the thread: what the change is for, and what came before.
        assert!(steelmans[0].3.contains("Makes x one."));
        assert!(steelmans[0].3.contains("Nothing yet"));
        assert!(steelmans[1].3.contains("After round 1: made it stronger"));
        assert!(!calls.iter().any(|c| c.3.contains("Fix these")));
        drop(calls);
        // Each round's tokens are kept apart.
        assert!(run.usage.iter().any(|u| u.round == 3));
    }

    #[test]
    fn a_writer_pins_the_fixes_and_each_pin_must_fail_before_and_pass_after() {
        use super::super::change::tests::repo_with;
        let top = "stages: [test-writing]\ntests: {command: 'sh {tests}'}";
        let cfg = config("exception", 20, 1, "", top);
        let repo = repo_with(&[("a.src", "let x = 1;\n")]);
        let root = std::env::temp_dir().join(format!("tb-acr-pins-{}", new_id()));
        let mut s = setting(&cfg);
        s.converge = true;
        s.copies = Some(root.clone());
        let fixed = Mutex::new(false);
        let checkout = repo.clone();
        let fake = Fake::new(|role, prompt: &str| {
            Ok(match role {
                Role::Reviewer if prompt.contains("alpha") && !*fixed.lock().unwrap() => {
                    ONE.to_string()
                }
                Role::Reviewer => CLEAN.to_string(),
                Role::Judge => rating("High"),
                Role::Orchestrator if prompt.contains("You synthesize") => {
                    assert!(prompt.contains("Leave the tests that pin these fixes"));
                    fs::write(checkout.join("a.src"), "let x = 2;\n").unwrap();
                    *fixed.lock().unwrap() = true;
                    r#"{"done": "x is two now"}"#.to_string()
                }
                Role::Orchestrator if prompt.contains("You write tests for a change") => {
                    assert!(prompt.contains("alpha-r1-1"));
                    fs::write(checkout.join("pin.sh"), "grep -q 'x = 2' a.src\n").unwrap();
                    fs::write(checkout.join("idle.sh"), "true\n").unwrap();
                    r#"{"tests": [{"file": "pin.sh", "pins": ["alpha-r1-1"]}, {"file": "idle.sh", "pins": ["alpha-r1-1"]}], "done": "pinned"}"#
                        .to_string()
                }
                _ => NOTHING.to_string(),
            })
        });
        let c = change(&["a.src"]);
        let mut run = start("r", &repo, "main", None, &s, &c, &fake).unwrap();
        drive(&mut run, &s, &fake, &|_| Ok(c.clone())).unwrap();
        // The pin that pins nothing stops the review for a person; with
        // nobody in the loop, it's taken as seen and the review goes on.
        assert_eq!(run.waiting, Some(Gate::Escalation));
        for e in &mut run.escalations {
            e.seen = true;
        }
        drive(&mut run, &s, &fake, &|_| Ok(c.clone())).unwrap();
        let _ = fs::remove_dir_all(&root);
        assert_eq!(run.status, Status::Confirmed);
        let m = &run.steelmans[0];
        assert_eq!(m.tests_done, "pinned");
        let pin = |f: &str| m.pins.iter().find(|p| p.file == f).unwrap().clone();
        assert_eq!(pin("pin.sh").fails_before, Some(true));
        assert_eq!(pin("pin.sh").passes_after, Some(true));
        // A test that passes on the old code pins nothing, and a person hears.
        assert_eq!(pin("idle.sh").fails_before, Some(false));
        assert!(
            run.escalations
                .iter()
                .any(|e| e.reason.contains("idle.sh pins nothing"))
        );
    }

    fn gated(mode: Option<&str>) -> (Run, Vec<(Role, String, bool, String)>) {
        use super::super::change::tests::repo_with;
        let top = match mode {
            Some(mode) => format!("stages: [gate]\ntests: {{command: 'true', gate: {mode}}}"),
            None => "stages: [gate]\ntests: {command: 'true'}".to_string(),
        };
        let cfg = config("exception", 1, 1, "", &top);
        assert!(cfg.lenses.iter().any(|l| l.id == "tests"));
        let repo = repo_with(&[("a.src", "let x = 1;\n")]);
        let root = std::env::temp_dir().join(format!("tb-acr-gate-{}", new_id()));
        let mut s = setting(&cfg);
        s.copies = Some(root.clone());
        // `true` passes whatever the code, so the mutant survives.
        let fake = Fake::new(|role, prompt: &str| {
            Ok(match role {
                Role::Reviewer if prompt.contains("proposing small breaks") => {
                    r#"{"mutants": [{"file": "a.src", "find": "x = 1", "replace": "x = 2", "why": "x is one", "tests": []}]}"#
                        .to_string()
                }
                Role::Reviewer => CLEAN.to_string(),
                Role::Judge => rating("High"),
                _ => NOTHING.to_string(),
            })
        });
        let c = change(&["a.src"]);
        let mut run = start("r", &repo, "main", None, &s, &c, &fake).unwrap();
        drive(&mut run, &s, &fake, &|_| Ok(c.clone())).unwrap();
        let _ = fs::remove_dir_all(&root);
        let calls = fake.calls.lock().unwrap().clone();
        (run, calls)
    }

    #[test]
    fn a_test_gap_that_blocks_stops_the_round_before_the_reviewers() {
        let (run, calls) = gated(Some("stop"));
        let gate = &run.lenses["tests"].rounds[0];
        assert_eq!(gate.findings.len(), 1);
        assert!(gate.findings[0].blocks());
        assert!(
            gate.notes[0].contains("1 mutants: 0 caught, 1 missed"),
            "{:?}",
            gate.notes
        );
        // Only the mutants' proposer ran: no lens's reviewer spent a thing.
        let reviewers: Vec<_> = calls.iter().filter(|c| c.0 == Role::Reviewer).collect();
        assert_eq!(reviewers.len(), 1);
        assert!(reviewers[0].3.contains("proposing small breaks"));
        assert!(run.lenses["alpha"].rounds.is_empty() && run.lenses["beta"].rounds.is_empty());
    }

    #[test]
    fn a_warning_gate_lets_the_reviewers_run_and_is_the_default() {
        for mode in [Some("warn"), None] {
            let (run, calls) = gated(mode);
            assert!(run.lenses["tests"].rounds[0].findings[0].blocks());
            assert_eq!(
                calls.iter().filter(|c| c.0 == Role::Reviewer).count(),
                3,
                "{mode:?}"
            );
            assert_eq!(run.lenses["alpha"].rounds.len(), 1, "{mode:?}");
        }
    }

    #[test]
    fn a_rounds_judges_run_at_once() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let cfg = plain("exception", 1, 1);
        let (now, most) = (AtomicUsize::new(0), AtomicUsize::new(0));
        let fake = Fake::new(|role, _: &str| {
            Ok(match role {
                Role::Reviewer => ONE.to_string(),
                Role::Judge => {
                    let n = now.fetch_add(1, Ordering::SeqCst) + 1;
                    most.fetch_max(n, Ordering::SeqCst);
                    std::thread::sleep(std::time::Duration::from_millis(200));
                    now.fetch_sub(1, Ordering::SeqCst);
                    rating("Low")
                }
                _ => NOTHING.to_string(),
            })
        });
        let (run, _) = go(&cfg, &["a.src"], &fake);
        assert_eq!(fake.count(Role::Judge), 2);
        assert_eq!(
            most.load(Ordering::SeqCst),
            2,
            "alpha's and beta's Judges overlap"
        );
        assert!(run.lenses.values().all(|l| l.rounds[0].findings.len() == 1));
        assert_eq!(
            run.usage.iter().filter(|u| u.role == Role::Judge).count(),
            2
        );
    }

    #[test]
    fn the_builder_briefs_round_one_and_the_steelman_the_next() {
        let cfg = config("exception", 20, 1, "", "stages: [brief]");
        let fixed = Mutex::new(false);
        let fake = Fake::new(|role, prompt: &str| {
            Ok(match role {
                Role::Orchestrator if prompt.contains("You brief the reviewers") => {
                    // Only the lens the builder left out.
                    assert!(prompt.contains("- beta:") && !prompt.contains("- alpha:"));
                    r#"{"briefs": {"beta": "Read the callers."}}"#.to_string()
                }
                Role::Reviewer if prompt.contains("alpha") && !*fixed.lock().unwrap() => {
                    ONE.to_string()
                }
                Role::Reviewer => CLEAN.to_string(),
                Role::Judge => rating("High"),
                Role::Orchestrator if prompt.contains("You synthesize") => {
                    assert!(prompt.contains("# Brief the next round"));
                    *fixed.lock().unwrap() = true;
                    r#"{"done": "fixed", "briefs": {"alpha": "Check my fix in a.src at midnight."}}"#
                        .to_string()
                }
                _ => NOTHING.to_string(),
            })
        });
        let mut s = setting(&cfg);
        s.converge = true;
        s.builder_brief = Some(stages::BuilderBrief {
            scope: Some("It makes x one, in a.src.".into()),
            lenses: BTreeMap::from([("alpha".to_string(), "Break the lookup.".to_string())]),
        });
        let c = change(&["a.src"]);
        let mut run = start("r", Path::new("."), "main", None, &s, &c, &fake).unwrap();
        drive(&mut run, &s, &fake, &|_| Ok(c.clone())).unwrap();
        assert_eq!(run.status, Status::Confirmed);
        let calls = fake.calls.lock().unwrap();
        let alpha: Vec<&String> = calls
            .iter()
            .filter(|c| c.0 == Role::Reviewer && c.3.contains("reviewer for alpha."))
            .map(|c| &c.3)
            .collect();
        assert!(alpha[0].contains("Break the lookup."));
        assert!(alpha[1].contains("Check my fix in a.src at midnight."));
        assert!(
            calls
                .iter()
                .any(|c| c.0 == Role::Reviewer && c.3.contains("Read the callers."))
        );
        drop(calls);
        let sent = fake.sent.lock().unwrap();
        assert!(
            sent.iter()
                .all(|(_, text)| text.starts_with(super::super::change::SAID)
                    && text.contains("## What its builder says it does"))
        );
        let _ = fs::remove_dir_all(&s.work_dir);
    }

    #[test]
    fn the_brief_stage_briefs_each_lens_and_only_runners_get_the_runbook() {
        let top = "stages: [brief]\nrunbook: Run tests with scripts/test.sh.";
        let extra = r#"  - {id: gamma, criteria: "gamma criteria", attack: "Run it in another time zone.", triggers: {paths: ["**/*.src"]}}"#;
        let cfg = config("exception", 1, 1, extra, top);
        let fake = Fake::new(|role, prompt: &str| {
            Ok(match role {
                Role::Orchestrator if prompt.contains("You brief the reviewers") => {
                    assert!(prompt.contains(
                        "- gamma: gamma criteria\n  How it's attacked: Run it in another time zone."
                    ));
                    assert!(prompt.contains("Run tests with scripts/test.sh."));
                    r#"{"briefs": {"alpha": "Break the lookup in a.src.", "gamma": "Try midnight."}}"#
                        .to_string()
                }
                Role::Reviewer => CLEAN.to_string(),
                _ => NOTHING.to_string(),
            })
        });
        let (run, _) = go(&cfg, &["a.src"], &fake);
        assert_eq!(run.status, Status::Confirmed);
        let calls = fake.calls.lock().unwrap();
        let brief_of = |id: &str| {
            calls
                .iter()
                .find(|c| c.0 == Role::Reviewer && c.3.contains(&format!("reviewer for {id}.")))
                .map(|c| c.3.clone())
                .unwrap()
        };
        let alpha = brief_of("alpha");
        assert!(
            alpha.contains("# Where to attack in this change")
                && alpha.contains("Break the lookup in a.src.")
        );
        let gamma = brief_of("gamma");
        assert!(gamma.contains("# How to attack in this lens\n\nRun it in another time zone."));
        assert!(gamma.contains("Try midnight."));
        // beta had no brief, and these reviewers can't run things, so none
        // gets the runbook.
        assert!(!brief_of("beta").contains("# Where to attack"));
        assert!(
            calls
                .iter()
                .all(|c| c.0 != Role::Reviewer || !c.3.contains("scripts/test.sh"))
        );
        drop(calls);
        assert_eq!(fake.count(Role::Orchestrator), 1, "the brief, once a round");
    }

    #[test]
    fn stages_join_as_lenses_and_each_runs_its_own_way() {
        use super::super::change::tests::repo_with;
        let top = "stages: [context, claims, mutation, probes]\ntests: {command: 'true'}";
        let cfg = config("exception", 1, 1, "", top);
        for id in ["claims", "mutation", "probes"] {
            assert!(cfg.lenses.iter().any(|l| l.id == id), "{id}");
        }
        let repo = repo_with(&[("a.src", "let x = 1;\n")]);
        let s = setting(&cfg);
        let fake = Fake::new(|role, prompt: &str| {
            Ok(match role {
                // The mutation reviewer proposes nothing.
                Role::Reviewer if prompt.contains("proposing small breaks") => {
                    r#"{"mutants": []}"#.to_string()
                }
                Role::Reviewer => CLEAN.to_string(),
                _ => NOTHING.to_string(),
            })
        });
        let mut c = change(&["a.src"]);
        c.diff = "+++ b/a.src\n@@ -0,0 +1 @@\n+// Sets x to one, as the spec says.\n".into();
        let mut run = start("r", &repo, "main", None, &s, &c, &fake).unwrap();
        drive(&mut run, &s, &fake, &|_| Ok(c.clone())).unwrap();
        let calls = fake.calls.lock().unwrap();
        let claims = calls
            .iter()
            .find(|c| c.3.contains("You check what a change claims"))
            .expect("a claims reviewer");
        assert!(
            claims
                .3
                .contains("[a.src:1] comment: Sets x to one, as the spec says.")
        );
        drop(calls);
        // Read-only reviewers can't run scenarios, and with nowhere for
        // copies the mutation stage can't run either: each says so.
        let failed = |id: &str| run.lenses[id].rounds[0].failures.join(" ");
        assert!(
            failed("probes").contains("needs a shell"),
            "{}",
            failed("probes")
        );
        assert!(
            failed("mutation").contains("place for copies"),
            "{}",
            failed("mutation")
        );
        // Every role got the history pack ahead of the diff, among what's
        // said about the change. This repository has no tests or callers to
        // point to, so there are no leads.
        let sent = fake.sent.lock().unwrap();
        assert!(sent.iter().all(|(_, text)| {
            let said = text.find("## Earlier commits to the changed files");
            said.is_some_and(|at| text.starts_with(super::super::change::SAID) && at > 0)
                && !text.contains("# Leads in the repository")
        }));
    }

    #[test]
    fn reviewers_copies_come_from_one_warm_copy_set_up_once() {
        use super::super::change::tests::repo_with;
        let mut cfg = config("exception", 1, 1, "", "tests: {setup: 'touch warmed'}");
        let name = cfg.bindings[&Role::Reviewer].clone();
        cfg.allowed_bindings.get_mut(&name).unwrap().tools =
            Some(vec!["Read".into(), "Bash".into()]);
        let repo = repo_with(&[("a.src", "let x = 1;\n")]);
        let root = std::env::temp_dir().join(format!("tb-acr-warm-{}", new_id()));
        let mut s = setting(&cfg);
        s.copies = Some(root.clone());
        let fake = Fake::new(defect_in("alpha", 99));
        let c = change(&["a.src"]);
        let mut run = start("r", &repo, "main", None, &s, &c, &fake).unwrap();
        drive(&mut run, &s, &fake, &|_| Ok(c.clone())).unwrap();
        let warmed = fake.warmed.lock().unwrap().clone();
        assert_eq!(
            warmed,
            [true, true],
            "each reviewer's copy has the setup's work"
        );
        // The setup ran in a copy, never in the checkout, and the round's
        // copies are gone.
        assert!(!repo.join("warmed").exists());
        assert!(!root.join("r-r1").exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_reviewer_that_can_change_files_works_in_a_copy_of_its_own() {
        use super::super::change::tests::repo_with;
        let mut cfg = plain("exception", 1, 1);
        let name = cfg.bindings[&Role::Reviewer].clone();
        cfg.allowed_bindings.get_mut(&name).unwrap().tools =
            Some(vec!["Read".into(), "Bash".into()]);
        let repo = repo_with(&[("a.src", "let x = 1;\n")]);
        let root = std::env::temp_dir().join(format!("tb-acr-copies-{}", new_id()));
        let mut s = setting(&cfg);
        s.copies = Some(root.clone());
        let fake = Fake::new(defect_in("alpha", 99));
        let c = change(&["a.src"]);
        let mut run = start("r", &repo, "main", None, &s, &c, &fake).unwrap();
        drive(&mut run, &s, &fake, &|_| Ok(c.clone())).unwrap();
        let repos = fake.repos.lock().unwrap();
        let reviewers: Vec<_> = repos.iter().filter(|r| r.0 == Role::Reviewer).collect();
        assert_eq!(reviewers.len(), 2);
        for (_, dir, had) in &reviewers {
            assert!(dir.starts_with(&root), "{}", dir.display());
            assert!(had, "{} held the change", dir.display());
        }
        assert_ne!(reviewers[0].1, reviewers[1].1, "each its own");
        // Every other role works in the checkout.
        let checkout = repo.canonicalize().unwrap();
        assert!(
            repos
                .iter()
                .filter(|r| r.0 != Role::Reviewer)
                .all(|r| r.1 == checkout)
        );
        drop(repos);
        let calls = fake.calls.lock().unwrap();
        assert!(
            calls
                .iter()
                .any(|c| c.0 == Role::Reviewer && c.3.contains("your own copy"))
        );
        drop(calls);
        // The round's copies go with it.
        assert!(!root.join("r-r1").exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn after_a_fix_only_what_it_touched_attacks_then_a_sweep_covers_the_rest() {
        let extra =
            r#"  - {id: gamma, criteria: "gamma criteria", triggers: {paths: ["**/*.md"]}}"#;
        let cfg = config("exception", 20, 1, extra, "");
        let fixed = Mutex::new(false);
        let mut fake = Fake::new(|role, prompt: &str| {
            Ok(match role {
                Role::Reviewer if prompt.contains("alpha") && !*fixed.lock().unwrap() => {
                    ONE.to_string()
                }
                Role::Reviewer => CLEAN.to_string(),
                Role::Judge => rating("High"),
                Role::Orchestrator if prompt.contains("You synthesize") => {
                    *fixed.lock().unwrap() = true;
                    r#"{"done": "fixed alpha"}"#.to_string()
                }
                _ => NOTHING.to_string(),
            })
        });
        // The fix touches only the code, not the docs gamma covers.
        fake.fixes = Some(vec!["a.src".into()]);
        let mut s = setting(&cfg);
        s.converge = true;
        let c = change(&["a.src", "README.md"]);
        let mut run = start("r", Path::new("."), "main", None, &s, &c, &fake).unwrap();
        drive(&mut run, &s, &fake, &|_| Ok(c.clone())).unwrap();
        assert_eq!(run.status, Status::Confirmed);
        // Round 1 all; round 2 alpha and beta, which the fix touched, with
        // the fix as their focus; round 3 the sweep: gamma, on the version
        // it hadn't seen.
        assert_eq!(run.round, 3);
        let rounds =
            |id: &str| -> Vec<u32> { run.lenses[id].rounds.iter().map(|r| r.round).collect() };
        assert_eq!(rounds("alpha"), [1, 2]);
        assert_eq!(rounds("beta"), [1, 2]);
        assert_eq!(rounds("gamma"), [1, 3]);
        assert_eq!(
            run.fix.as_ref().map(|f| f.files.clone()),
            Some(vec!["a.src".to_string()])
        );
        let sent = fake.sent.lock().unwrap();
        let focused = sent
            .iter()
            .filter(|(r, text)| {
                *r == Role::Reviewer && text.contains("What the fixes after round 1 changed")
            })
            .count();
        assert_eq!(
            focused, 2,
            "round 2's two reviewers got the fix as their focus"
        );
        let _ = fs::remove_dir_all(&s.work_dir);
    }

    #[test]
    fn a_lens_a_later_version_of_the_change_needs_joins_and_none_leaves() {
        let extra =
            r#"  - {id: gamma, criteria: "gamma criteria", triggers: {paths: ["**/*.db"]}}"#;
        let cfg = config("exception", 20, 1, extra, "");
        let fixed = Mutex::new(false);
        let fake = Fake::new(|role, prompt: &str| {
            Ok(match role {
                Role::Reviewer if prompt.contains("alpha") && !*fixed.lock().unwrap() => {
                    ONE.to_string()
                }
                Role::Reviewer => CLEAN.to_string(),
                Role::Judge => rating("High"),
                Role::Orchestrator if prompt.contains("You synthesize") => {
                    *fixed.lock().unwrap() = true;
                    r#"{"done": "moved it into the store"}"#.to_string()
                }
                _ => NOTHING.to_string(),
            })
        });
        let mut s = setting(&cfg);
        s.converge = true;
        let first = change(&["a.src"]);
        let mut run = start("r", Path::new("."), "main", None, &s, &first, &fake).unwrap();
        assert!(!run.lenses.contains_key("gamma"));
        // The steelman's version touches only the store: no lens that ran
        // matches it now, and gamma does.
        let moved = change(&["store.db"]);
        drive(&mut run, &s, &fake, &|fixed| {
            Ok(if fixed { moved.clone() } else { first.clone() })
        })
        .unwrap();
        assert_eq!(run.status, Status::Confirmed);
        assert_eq!(run.round, 2);
        let gamma = &run.lenses["gamma"];
        assert_eq!(gamma.rounds.len(), 1);
        assert_eq!(gamma.joined, "round 2: its path store.db matched");
        for id in ["alpha", "beta"] {
            assert_eq!(run.lenses[id].rounds.len(), 2, "{id} stays on");
            assert!(run.lenses[id].joined.is_empty());
        }
    }

    #[test]
    fn a_steelman_folds_only_blockers_and_its_declines_are_disagreements() {
        let cfg = plain("exception", 20, 1);
        let fixed = Mutex::new(false);
        let fake = Fake::new(|role, prompt: &str| {
            Ok(match role {
                Role::Reviewer if prompt.contains("alpha") && !*fixed.lock().unwrap() => {
                    ONE.to_string()
                }
                Role::Reviewer if prompt.contains("beta") => ONE.to_string(),
                Role::Reviewer => CLEAN.to_string(),
                Role::Judge if prompt.contains("alpha criteria") => rating("High"),
                Role::Judge => rating("Low"),
                Role::Orchestrator if prompt.contains("You synthesize") => {
                    *fixed.lock().unwrap() = true;
                    r#"{"done": "fixed alpha", "declined": [{"finding": "alpha-r1-1", "reason": "it's a style call"}]}"#
                        .to_string()
                }
                _ => NOTHING.to_string(),
            })
        });
        let run = converging(&cfg, &fake);
        assert_eq!(run.status, Status::Confirmed);
        // beta's Low finding never blocks, so round 2 ends it.
        assert_eq!(run.round, 2);
        assert_eq!(run.steelmans.len(), 1);
        assert_eq!(run.steelmans[0].blockers, ["alpha-r1-1"]);
        let calls = fake.calls.lock().unwrap();
        let steelman = calls
            .iter()
            .find(|c| c.3.contains("You synthesize"))
            .unwrap();
        // What's upheld without blocking goes to a person, not into the work.
        assert!(steelman.3.contains("alpha-r1-1") && !steelman.3.contains("beta-r1-1"));
        drop(calls);
        let d = run
            .disagreements
            .iter()
            .find(|d| d.finding == "alpha-r1-1")
            .unwrap();
        assert_eq!(d.lens, "alpha");
        assert!(d.sides[1].contains("style call"));
        for round in &run.lenses["beta"].rounds {
            assert_eq!(round.findings[0].state, FindingState::Logged);
        }
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
    fn stop_on_failure_ends_the_run_at_the_first_reviewer_or_judge_failure() {
        let cfg = plain("exception", 3, 1);
        let mut s = setting(&cfg);
        s.stop_on_failure = true;
        let c = change(&["a.src"]);
        let reviewer = Fake::new(|role, prompt: &str| match role {
            Role::Reviewer if prompt.contains("alpha") => bail!("the bridge is down"),
            Role::Reviewer => Ok(CLEAN.into()),
            _ => Ok(NOTHING.into()),
        });
        let judge = Fake::new(|role, _: &str| match role {
            Role::Reviewer => Ok(ONE.into()),
            Role::Judge => Ok("couldn't say".into()),
            _ => Ok(NOTHING.into()),
        });
        let stops = |fake: &dyn Caller, says: &str| {
            let mut run = start("r", Path::new("."), "main", None, &s, &c, fake).unwrap();
            let err = drive(&mut run, &s, fake, &|_| Ok(c.clone()))
                .unwrap_err()
                .to_string();
            assert!(
                err.starts_with("stopped at the first failure") && err.contains(says),
                "{err}"
            );
            assert_eq!(run.status, Status::Stopped);
            assert!(
                run.lenses.values().all(|l| l.rounds.len() == 1),
                "no round after the failure"
            );
        };
        stops(&reviewer, "the bridge is down");
        stops(&judge, "the Judge");
        // Without it, the same failure is recorded and the run goes on to
        // a person.
        s.stop_on_failure = false;
        let mut run = start("r", Path::new("."), "main", None, &s, &c, &reviewer).unwrap();
        drive(&mut run, &s, &reviewer, &|_| Ok(c.clone())).unwrap();
        assert_eq!(run.waiting, Some(Gate::Escalation));
        assert!(failures(&run)[0].contains("alpha, round 1: "));
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
            ("Medium", Status::Confirmed, FindingState::Logged),
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
    fn a_real_finding_another_lens_owns_is_rated_like_any_and_noise_is_dropped() {
        let cfg = plain("exception", 3, 1);
        let fake = Fake::new(|role, _: &str| match role {
            Role::Reviewer => Ok(ONE.into()),
            Role::Judge => Ok(r#"{"findings": [{"title": "t", "severity": "Critical",
                "on_task": false, "reason": "another lens's job"},
                {"title": "n", "severity": "High", "real_defect": false, "reason": "not so"}]}"#
                .into()),
            _ => Ok(NOTHING.into()),
        });
        let (run, _) = go(&cfg, &["a.src"], &fake);
        let f = &run.lenses["alpha"].rounds[0].findings;
        // A real Critical blocks, whichever lens finds it.
        assert_eq!(f[0].severity, Severity::Critical);
        assert_eq!(
            f[0].reason,
            "another lens's job (outside this lens's criteria)"
        );
        assert_eq!(f[1].severity, Severity::Rejected);
        assert_eq!(f[1].state, FindingState::Dropped);
    }

    #[test]
    fn every_combination_of_facts_has_one_ruling_and_none_hides_a_defect() {
        let cfg = plain("exception", 3, 1);
        let lens = cfg.lenses[0].clone();
        let severities = [
            Severity::Critical,
            Severity::High,
            Severity::Medium,
            Severity::Low,
            Severity::Rejected,
        ];
        let rank = |s: Severity| match s {
            Severity::Critical => 4,
            Severity::High => 3,
            Severity::Medium => 2,
            Severity::Low => 1,
            Severity::Rejected => 0,
        };
        let mut cases = 0;
        for bits in 0u32..512 {
            let fact = |i: u32| bits & (1 << i) != 0;
            for &severity in &severities {
                let mut r: roles::Rating = serde_json::from_value(serde_json::json!({
                    "title": "t", "severity": format!("{severity:?}")
                }))
                .unwrap();
                r.relevant = fact(0);
                r.on_task = fact(1);
                r.pre_existing = fact(2);
                r.claims_fix = fact(3);
                r.worsens = fact(4);
                r.weak_pin = fact(5);
                r.misleads = fact(6);
                r.equivalent = fact(7);
                r.meets_blocker = fact(8);
                let ruled = ruling(&cfg, &lens, &r);
                cases += 1;
                let real = r.relevant && !r.equivalent && severity != Severity::Rejected;
                // Only what isn't a real defect is dropped.
                assert_eq!(ruled.action == Action::Drop, !real, "{r:?} -> {ruled:?}");
                if !real {
                    continue;
                }
                let raised = r.weak_pin
                    || r.misleads
                    || (r.pre_existing && (r.claims_fix || r.worsens))
                    || (r.meets_blocker && !r.pre_existing);
                if raised {
                    assert!(rank(ruled.severity) >= 3, "{r:?} -> {ruled:?}");
                    assert!(rank(ruled.severity) >= rank(severity));
                } else if r.pre_existing {
                    // The one lowering rule for a real defect: logged, and a
                    // Critical one goes to a person.
                    assert_eq!(ruled.action, Action::NeverBlocks);
                    assert_eq!(ruled.escalate, severity == Severity::Critical);
                } else {
                    // Otherwise the Judge's severity stands, on task or not.
                    assert_eq!(ruled.severity, severity, "{r:?} -> {ruled:?}");
                }
            }
        }
        assert_eq!(cases, 512 * 5);
    }

    #[test]
    fn the_diff_settles_that_a_finding_in_an_added_line_isnt_pre_existing() {
        let changed = stages::changed_lines(
            "+++ b/test/a_test.dart\n@@ -3,1 +5,3 @@\n-moved();\n+x\n+y\n+  moved();\n",
        );
        // A line the diff only moves isn't new.
        assert!(!changed["test/a_test.dart"].contains_key(&7));
        let rating = |line: u64| -> roles::Rating {
            roles::read_ratings(&format!(
                r#"{{"findings": [{{"title": "t", "severity": "Low", "file": "test/a_test.dart", "line": {line}, "pre_existing": true, "worsens": true}}]}}"#
            ))
            .unwrap()
            .remove(0)
        };
        assert!(!grounded(rating(6), &changed, None).pre_existing);
        assert!(grounded(rating(40), &changed, None).pre_existing);
        // So a new test's flaw isn't raised as a worsened old defect.
        let cfg = plain("exception", 3, 1);
        let ruled = ruling(&cfg, &cfg.lenses[0], &grounded(rating(6), &changed, None));
        assert_eq!(ruled.severity, Severity::Low);
    }

    #[test]
    fn a_weak_pin_or_misleading_claim_holds_only_with_a_quote_the_change_adds() {
        let changed = stages::changed_lines(
            &[
                "+++ b/test/edit_test.dart",
                "@@ -1,2 +1,4 @@",
                "-  // old words that were here before",
                "+  testWidgets('an edit keeps the rest of it', (t) async {",
                "+  // Notices for the old date are",
                "+  // cancelled when the date moves.",
                "   unchanged context line about a claim",
            ]
            .join("\n"),
        );
        let judged = |facts: &str| -> roles::Rating {
            let r = roles::read_ratings(&format!(
                r#"{{"findings": [{{"title": "t", "severity": "Low", {facts}}}]}}"#
            ))
            .unwrap()
            .remove(0);
            grounded(r, &changed, None)
        };
        // The test's name, quoted with or without its marks and case.
        assert!(
            judged(r#""weak_pin": true, "weak_pin_test": "An edit keeps the rest of it""#).weak_pin
        );
        // A claim over two comment lines, and one that skips words.
        assert!(
            judged(
                r#""misleads": true, "misleading_claim": "notices for the old date are cancelled""#
            )
            .misleads
        );
        assert!(
            judged(r#""misleads": true, "misleading_claim": ["old date ... when the date moves"]"#)
                .misleads
        );
        // No quote, a quote the change doesn't add, or one too short: false.
        assert!(!judged(r#""weak_pin": true"#).weak_pin);
        assert!(
            !judged(r#""misleads": true, "misleading_claim": "old words that were here before""#)
                .misleads
        );
        assert!(
            !judged(r#""misleads": true, "misleading_claim": "unchanged context line""#).misleads
        );
        assert!(!judged(r#""weak_pin": true, "weak_pin_test": "edit""#).weak_pin);
        // So the rules raise only what the change itself says.
        let cfg = plain("exception", 3, 1);
        let raised = |facts: &str| ruling(&cfg, &cfg.lenses[0], &judged(facts)).severity;
        assert_eq!(raised(r#""misleads": true"#), Severity::Low);
        assert_eq!(
            raised(r#""misleads": true, "misleading_claim": "cancelled when the date moves""#),
            Severity::High
        );
    }

    #[test]
    fn a_finding_blocks_by_the_lenss_definition_only_quoting_it() {
        let changed = stages::changed_lines("+++ b/lib/a.dart\n@@ -1,1 +1,1 @@\n+x\n");
        let blocker = "a wrong result, a wrong or overstated claim, or a test that passes for \
                       the wrong reason; not style";
        let judged = |facts: &str, def: Option<&str>| -> roles::Rating {
            let r = roles::read_ratings(&format!(
                r#"{{"findings": [{{"title": "t", "severity": "Low", {facts}}}]}}"#
            ))
            .unwrap()
            .remove(0);
            grounded(r, &changed, def)
        };
        let meets = r#""meets_blocker": true, "blocker_clause": "a wrong or overstated claim""#;
        assert!(judged(meets, Some(blocker)).meets_blocker);
        // No quote, words the definition doesn't have, or a lens with no
        // definition: false.
        assert!(!judged(r#""meets_blocker": true"#, Some(blocker)).meets_blocker);
        assert!(
            !judged(
                r#""meets_blocker": true, "blocker_clause": "anything that looks untidy""#,
                Some(blocker)
            )
            .meets_blocker
        );
        assert!(!judged(meets, None).meets_blocker);
        // Meeting it raises a Low to High, so it blocks; a defect that was
        // there before the change stays logged.
        let cfg = plain("exception", 3, 1);
        let ruled = ruling(&cfg, &cfg.lenses[0], &judged(meets, Some(blocker)));
        assert_eq!(ruled.severity, Severity::High);
        let old = judged(&format!("{meets}, \"pre_existing\": true"), Some(blocker));
        assert_eq!(
            ruling(&cfg, &cfg.lenses[0], &old).action,
            Action::NeverBlocks
        );
        // The Judge and the reviewers are given the definition; without
        // one, neither is asked about it.
        let mut lens = cfg.lenses[0].clone();
        lens.blocker = Some(blocker.into());
        let prompt = roles::judge_prompt(&lens, &[Vec::new()], "").unwrap();
        assert!(prompt.contains(blocker) && prompt.contains("\"blocker_clause\""));
        let plain_prompt = roles::judge_prompt(&cfg.lenses[0], &[Vec::new()], "").unwrap();
        assert!(!plain_prompt.contains("meets_blocker"));
        let review = roles::reviewer_prompt(&lens, "the change", "", false, "");
        assert!(review.contains(&format!("What blocks here: {blocker}")));
    }

    #[test]
    fn a_calibration_fingerprints_every_rule() {
        let rules = rules_source();
        for f in [
            "fn ruling(",
            "fn grounded(",
            "fn changed_lines(",
            "fn adds_text(",
            "fn quotes_text(",
            "fn quote_parts(",
        ] {
            assert!(
                rules.contains(f),
                "{f} is missing from the rules' fingerprint"
            );
        }
    }

    #[test]
    fn rules_decide_what_blocks_from_the_judges_facts() {
        let cfg = plain("exception", 3, 1);
        let lens = cfg.lenses[0].clone();
        let rated = |json: &str| -> (Severity, Action) {
            let r = roles::read_ratings(&format!("{{\"findings\": [{json}]}}")).unwrap();
            let ruled = ruling(&cfg, &lens, &r[0]);
            (ruled.severity, ruled.action)
        };
        // A test the change adds that can't fail when what it pins breaks.
        assert_eq!(
            rated(r#"{"title": "t", "severity": "Low", "weak_pin": true}"#),
            (Severity::High, Action::Blocks)
        );
        // A claim that would lead someone to ship a defect.
        assert_eq!(
            rated(r#"{"title": "t", "severity": "Medium", "misleads": true}"#).0,
            Severity::High
        );
        // There before, neither claimed fixed nor made worse: logged.
        assert_eq!(
            rated(r#"{"title": "t", "severity": "Medium", "pre_existing": true}"#),
            (Severity::Low, Action::NeverBlocks)
        );
        assert_eq!(
            rated(r#"{"title": "t", "severity": "Medium", "pre_existing": true, "worsens": true}"#)
                .0,
            Severity::High
        );
        // A break nobody could see, and a Critical stays Critical.
        assert_eq!(
            rated(r#"{"title": "t", "severity": "High", "equivalent_mutant": true}"#).1,
            Action::Drop
        );
        assert_eq!(
            rated(r#"{"title": "t", "severity": "Critical", "weak_pin": true}"#).0,
            Severity::Critical
        );
        assert_eq!(
            rated(r#"{"title": "t", "severity": "Medium"}"#),
            (Severity::Medium, Action::NeverBlocks)
        );
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
    fn a_medium_a_lens_makes_block_can_be_waived() {
        let extra = r#"  - {id: gamma, criteria: "gamma criteria", triggers: {paths: ["**/*.src"]}, severity: {Medium: blocks-unless-waived}}"#;
        let cfg = config("per-round", 3, 1, extra, "");
        let fake = Fake::new(|role, prompt: &str| match role {
            Role::Reviewer if prompt.contains("reviewer for gamma") => Ok(ONE.into()),
            Role::Reviewer => Ok(CLEAN.into()),
            Role::Judge => Ok(rating("Medium")),
            _ => Ok(NOTHING.into()),
        });
        let (mut run, c) = go(&cfg, &["a.src"], &fake);
        let id = run.lenses["gamma"].rounds[0].findings[0].id.clone();
        let waive = Decision {
            waive: vec![id],
            ..Decision::default()
        };
        resume(&mut run, &cfg, &c, &fake, waive).unwrap();
        assert_eq!(run.lenses["gamma"].outcome, Outcome::Waived);
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
        let extra = r#"  - {id: gamma, criteria: "gamma criteria", triggers: {paths: ["**/*.src"]}, binding: small}"#;
        let cfg = config("exception", 3, 1, extra, "");
        let fake = Fake::new(|_, _: &str| Ok(CLEAN.to_string()));
        go(&cfg, &["a.src"], &fake);
        let calls = fake.calls.lock().unwrap();
        let gamma = calls
            .iter()
            .find(|c| c.3.contains("gamma criteria"))
            .unwrap();
        assert_eq!(gamma.1, "small");
        let alpha = calls
            .iter()
            .find(|c| c.3.contains("alpha criteria"))
            .unwrap();
        assert_eq!(alpha.1, "medium");
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
                   costs: {prices: {example/small-model: {input_per_mtok: 0.1, output_per_mtok: 0.4}, \
                   example/medium-model: {input_per_mtok: 0.1, output_per_mtok: 0.4}, \
                   example/large-model: {input_per_mtok: 0.1, output_per_mtok: 0.4}}, \
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
                .any(|b| b["role"] == "reviewer" && b["binding"] == "medium")
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
