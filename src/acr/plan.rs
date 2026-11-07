//! What a review would do: its tier, its lenses and its cost. This much
//! needs no model, which is what makes a dry run possible.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::Result;
use globset::{Glob, GlobSet, GlobSetBuilder};
use regex::Regex;
use serde::{Deserialize, Serialize};

use super::change::{Change, LeftOut};
use super::config::{Binding, Config, Lens, PerCall, Role, Tier};
use super::flow::{Run, Status, Usage};

/// Roughly how many bytes make a token.
const BYTES_PER_TOKEN: usize = 4;
/// What a brief adds to the artifact, in tokens.
const BRIEF_TOKENS: u64 = 1500;
/// What a call is assumed to write back, in tokens, before it's made.
const ANSWER_TOKENS: u64 = 1500;

pub fn tokens_in(bytes: usize) -> u64 {
    (bytes / BYTES_PER_TOKEN) as u64
}

fn globs(paths: &[String]) -> Result<GlobSet> {
    let mut set = GlobSetBuilder::new();
    for path in paths {
        set.add(Glob::new(path)?);
    }
    Ok(set.build()?)
}

/// The tier whose paths every changed file matches, in the order given.
/// Anything else takes the Sizer.
pub fn tier_by_paths<'a>(cfg: &'a Config, change: &Change) -> Result<Option<&'a Tier>> {
    for tier in cfg.tiers.iter().filter(|t| !t.rule.paths.is_empty()) {
        let set = globs(&tier.rule.paths)?;
        if !change.files.is_empty() && change.files.iter().all(|f| set.is_match(f)) {
            return Ok(Some(tier));
        }
    }
    Ok(None)
}

/// When unsure, the higher tier: the last, since they're ordered.
pub fn highest(cfg: &Config) -> &Tier {
    cfg.tiers.last().expect("checked to have a tier")
}

/// The tiers a Sizer chooses among.
pub fn described_tiers(cfg: &Config) -> Vec<&Tier> {
    cfg.tiers
        .iter()
        .filter(|t| t.rule.description.is_some())
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum Why {
    /// A path or content trigger matched.
    Matched(String),
    /// The tier requires it.
    Required,
    /// Only the Orchestrator can say, from this description.
    Description(String),
}

/// Why a lens applies to a change, if it might.
pub fn lens_applies(lens: &Lens, change: &Change) -> Result<Option<Why>> {
    let t = &lens.triggers;
    if !t.paths.is_empty() {
        let set = globs(&t.paths)?;
        if let Some(file) = change.files.iter().find(|f| set.is_match(f.as_str())) {
            return Ok(Some(Why::Matched(format!("path {file}"))));
        }
    }
    for pattern in &t.content {
        let re = Regex::new(pattern)?;
        if change.changed_lines().any(|l| re.is_match(l)) {
            return Ok(Some(Why::Matched(format!("content {pattern}"))));
        }
    }
    Ok(t.description.clone().map(Why::Description))
}

#[derive(Debug, Clone, Serialize)]
pub struct PlannedLens {
    pub id: String,
    pub why: Why,
    pub binding: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct RoleCost {
    pub role: Role,
    pub binding: String,
    pub calls: u64,
    /// Input not read from a cache.
    pub tokens_in: u64,
    pub cache_read: u64,
    pub tokens_out: u64,
    /// None if its binding has no prices, or is a subscription.
    pub usd: Option<f64>,
    /// Counted in tokens only.
    pub subscription: bool,
}

impl RoleCost {
    pub fn tokens(&self) -> u64 {
        self.tokens_in + self.cache_read + self.tokens_out
    }
}

/// What a review could take: always its tokens, and its dollars when they
/// mean something.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Estimate {
    pub tokens: u64,
    /// For the calls billed in dollars. None if one of those has no price.
    pub usd: Option<f64>,
}

/// Tokens and dollars, added and scaled together. A subscription's calls
/// add no dollars.
#[derive(Debug, Clone, Copy)]
struct Amount {
    tokens: f64,
    usd: Option<f64>,
}

impl Amount {
    const ZERO: Amount = Amount {
        tokens: 0.0,
        usd: Some(0.0),
    };

    fn of(c: &RoleCost) -> Amount {
        Amount {
            tokens: c.tokens() as f64,
            usd: if c.subscription { Some(0.0) } else { c.usd },
        }
    }

    fn times(self, n: f64) -> Amount {
        Amount {
            tokens: self.tokens * n,
            usd: self.usd.map(|u| u * n),
        }
    }

    fn plus(self, other: Amount) -> Amount {
        Amount {
            tokens: self.tokens + other.tokens,
            usd: all([self.usd, other.usd]),
        }
    }

    fn estimate(self) -> Estimate {
        Estimate {
            tokens: self.tokens.round() as u64,
            usd: self.usd,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Plan {
    pub tier: String,
    /// False when it's the highest because only the Sizer could say.
    pub tier_decided: bool,
    pub fan_out: u32,
    pub lenses: Vec<PlannedLens>,
    pub files: usize,
    pub first_round: Vec<RoleCost>,
    /// One fix by the Orchestrator, made between rounds. None with one round.
    pub fix: Option<RoleCost>,
    /// With every lens done in one round.
    pub least: Estimate,
    /// With each lens taking the rounds it took before here, on average.
    /// None without earlier reviews.
    pub expected: Option<Estimate>,
    /// The finished reviews of this repository that went into it.
    pub history_runs: usize,
    /// With every lens going all the way to `max_rounds`, and a fix between
    /// each round.
    pub most: Estimate,
    /// Whether any call is billed in dollars, rather than all coming out of
    /// subscriptions.
    pub dollars: bool,
    /// Bindings in use that nothing gives a price, as `name (model)`.
    pub unpriced: Vec<String>,
    /// What the config says each call by a role adds, for the roles above.
    pub per_call: Vec<(Role, PerCall)>,
    pub escalates: Vec<String>,
    /// Generated files left out of the diff reviewers get.
    pub generated: Vec<LeftOut>,
}

/// One call's tokens and dollars.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Cost {
    /// Input not read from a cache.
    pub tokens_in: u64,
    pub cache_read: u64,
    pub tokens_out: u64,
    /// None if it has no price, or is on a subscription.
    pub usd: Option<f64>,
    pub subscription: bool,
}

/// What a call is estimated to cost: the text ACR sends and reads back,
/// plus what the config says a call by its role takes besides.
pub fn cost(
    cfg: &Config,
    role: Role,
    name: &str,
    binding: Option<&Binding>,
    text_in: u64,
    text_out: u64,
) -> Cost {
    let extra = cfg.per_call(role);
    let (tokens_in, cache_read, tokens_out) = (
        text_in + extra.input_tokens,
        extra.cache_read_tokens,
        text_out + extra.output_tokens,
    );
    let subscription = binding.is_some_and(|b| b.subscription);
    Cost {
        tokens_in,
        cache_read,
        tokens_out,
        usd: binding
            .filter(|_| !subscription)
            .and_then(|b| cfg.price(name, b))
            .map(|p| p.usd(tokens_in, cache_read, 0, tokens_out)),
        subscription,
    }
}

/// The lenses a tier selects, by triggers and by what it requires. Lenses
/// that only a description can settle are in, to be sure of the upper bound.
pub fn select<'a>(cfg: &'a Config, tier: &Tier, change: &Change) -> Result<Vec<(&'a Lens, Why)>> {
    let mut chosen = Vec::new();
    for lens in &cfg.lenses {
        let why = if tier.required.contains(&lens.id) {
            Some(Why::Required)
        } else {
            lens_applies(lens, change)?
        };
        if let Some(why) = why {
            chosen.push((lens, why));
        }
    }
    Ok(chosen)
}

/// Why a change must reach a person, whatever the gate: it edits the rules
/// it's reviewed by. A config kept outside the repository can't be edited
/// by a change to it.
pub fn config_escalation(cfg: &Config, change: &Change) -> Option<String> {
    if cfg.outside {
        return None;
    }
    let touched = |path: &str| change.files.iter().any(|f| f == path);
    if touched(super::config::REPO_CONFIG) {
        return Some(format!("this change edits {}", super::config::REPO_CONFIG));
    }
    let list = cfg.not_findings.as_deref()?;
    touched(list).then(|| format!("this change edits {list}, which reviewers are told to believe"))
}

/// Several calls by one role, as one line.
fn sum_of(role: Role, binding: &str, parts: &[RoleCost]) -> RoleCost {
    RoleCost {
        role,
        binding: binding.to_string(),
        calls: parts.iter().map(|c| c.calls).sum(),
        tokens_in: parts.iter().map(|c| c.tokens_in).sum(),
        cache_read: parts.iter().map(|c| c.cache_read).sum(),
        tokens_out: parts.iter().map(|c| c.tokens_out).sum(),
        usd: if parts.is_empty() {
            Some(0.0)
        } else {
            parts.iter().map(|c| c.usd).sum()
        },
        subscription: parts.iter().any(|c| c.subscription),
    }
}

/// Each part's dollars added up, or None if any part has no price.
fn all(parts: impl IntoIterator<Item = Option<f64>>) -> Option<f64> {
    parts.into_iter().sum()
}

/// How earlier reviews of a repository went, for an expected cost.
#[derive(Debug, Clone, Default)]
pub struct History {
    pub runs: usize,
    /// The rounds each lens took, by its id.
    lens_rounds: BTreeMap<String, Vec<u32>>,
    /// The fixes in each run: one for each round after the first.
    fixes: Vec<u32>,
}

fn mean(values: &[u32]) -> Option<f64> {
    (!values.is_empty())
        .then(|| values.iter().map(|&n| f64::from(n)).sum::<f64>() / values.len() as f64)
}

/// A finished review, as history keeps it: a line, where the review's own
/// files are many and go after a week.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Record {
    pub id: String,
    pub repo: PathBuf,
    /// The rounds each lens took.
    pub lenses: BTreeMap<String, u32>,
    /// One for each round after the first.
    pub fixes: u32,
    #[serde(default)]
    pub tokens: u64,
    #[serde(default)]
    pub usd: Option<f64>,
}

impl Record {
    /// None for a review that isn't finished, since one still going would
    /// make reviews look shorter than they are, or that reviewed nothing.
    pub fn of(run: &Run) -> Option<Record> {
        if !matches!(run.status, Status::Confirmed | Status::Stopped) {
            return None;
        }
        let lenses: BTreeMap<String, u32> = run
            .lenses
            .values()
            .filter(|l| !l.rounds.is_empty())
            .map(|l| (l.id.clone(), l.rounds.len() as u32))
            .collect();
        let most = lenses.values().copied().max()?;
        Some(Record {
            id: run.id.clone(),
            repo: run.repo.clone(),
            lenses,
            fixes: most - 1,
            tokens: run.usage.iter().map(Usage::tokens).sum(),
            usd: run.usage.iter().map(|u| u.usd).sum(),
        })
    }
}

impl History {
    pub fn of(records: impl IntoIterator<Item = Record>) -> History {
        let mut history = History::default();
        for record in records {
            history.runs += 1;
            for (id, n) in record.lenses {
                history.lens_rounds.entry(id).or_default().push(n);
            }
            history.fixes.push(record.fixes);
        }
        history
    }

    /// A lens's own average, or every lens's if it's new.
    fn rounds(&self, lens: &str) -> f64 {
        let every: Vec<u32> = self.lens_rounds.values().flatten().copied().collect();
        self.lens_rounds
            .get(lens)
            .and_then(|v| mean(v))
            .or_else(|| mean(&every))
            .unwrap_or(1.0)
    }

    fn fixes(&self) -> f64 {
        mean(&self.fixes).unwrap_or(0.0)
    }
}

/// A dry run: everything here comes from config and git, with no model.
/// `history` is earlier reviews of the same repository.
pub fn dry_run(cfg: &Config, change: &Change, history: &History) -> Result<Plan> {
    let by_paths = tier_by_paths(cfg, change)?;
    let tier = by_paths.unwrap_or(highest(cfg));
    let chosen = select(cfg, tier, change)?;
    let mut lenses = Vec::new();
    for (lens, why) in &chosen {
        lenses.push(PlannedLens {
            id: lens.id.clone(),
            why: why.clone(),
            binding: cfg.binding_name(Role::Reviewer, Some(lens))?.to_string(),
        });
    }
    // What reviewers read: the diff without generated files, unless their
    // lens takes those too.
    let artifact = tokens_in(change.diff.len()) + BRIEF_TOKENS;
    let whole = change
        .full
        .as_ref()
        .map_or(artifact, |full| tokens_in(full.len()) + BRIEF_TOKENS);
    let fan_out = u64::from(tier.fan_out);
    let needs_orchestrator = chosen.iter().any(|(_, w)| matches!(w, Why::Description(_)))
        || !cfg.always_escalate.is_empty();
    let role_cost = |role: Role, lens: Option<&Lens>, calls: u64, input: u64| -> Result<RoleCost> {
        let name = cfg.binding_name(role, lens)?.to_string();
        let one = cost(
            cfg,
            role,
            &name,
            cfg.allowed_bindings.get(&name),
            input,
            ANSWER_TOKENS,
        );
        Ok(RoleCost {
            role,
            binding: name,
            calls,
            tokens_in: calls * one.tokens_in,
            cache_read: calls * one.cache_read,
            tokens_out: calls * one.tokens_out,
            // Nothing costs nothing, priced or not.
            usd: if calls == 0 && !one.subscription {
                Some(0.0)
            } else {
                one.usd.map(|usd| usd * calls as f64)
            },
            subscription: one.subscription,
        })
    };
    let sizer = role_cost(Role::Sizer, None, u64::from(by_paths.is_none()), artifact)?;
    let orchestrator = role_cost(
        Role::Orchestrator,
        None,
        u64::from(needs_orchestrator),
        artifact,
    )?;
    let mut reviewers = Vec::new();
    // A Judge reads the diff its lens's reviewers got, and their findings,
    // one list from each.
    let mut judge_each = Vec::new();
    for (lens, _) in &chosen {
        let reads = if lens.include_generated {
            whole
        } else {
            artifact
        };
        reviewers.push(role_cost(Role::Reviewer, Some(lens), fan_out, reads)?);
        judge_each.push(role_cost(
            Role::Judge,
            None,
            1,
            reads + fan_out * ANSWER_TOKENS,
        )?);
    }
    let judges = sum_of(
        Role::Judge,
        cfg.binding_name(Role::Judge, None)?,
        &judge_each,
    );
    // A fix is told the findings, not the whole change.
    let fix = (cfg.max_rounds > 1)
        .then(|| role_cost(Role::Orchestrator, None, 1, BRIEF_TOKENS))
        .transpose()?;

    // What a lens's round costs: its reviewers, and its call to the Judge.
    let rounds: Vec<(&str, Amount)> = chosen
        .iter()
        .zip(reviewers.iter().zip(&judge_each))
        .map(|((lens, _), (r, j))| (lens.id.as_str(), Amount::of(r).plus(Amount::of(j))))
        .collect();
    let setup = Amount::of(&sizer).plus(Amount::of(&orchestrator));
    let one_fix = fix.as_ref().map_or(Amount::ZERO, Amount::of);
    let max = f64::from(cfg.max_rounds);
    let at = |lens_rounds: &dyn Fn(&str) -> f64, fixes: f64| {
        rounds
            .iter()
            .fold(setup, |sum, (id, round)| {
                sum.plus(round.times(lens_rounds(id)))
            })
            .plus(one_fix.times(fixes))
            .estimate()
    };
    let least = at(&|_| 1.0, 0.0);
    let most = at(&|_| max, max - 1.0);
    let expected = (history.runs > 0).then(|| {
        at(
            &|id| history.rounds(id).min(max),
            history.fixes().min(max - 1.0),
        )
    });

    let first_round: Vec<RoleCost> = [sizer, orchestrator]
        .into_iter()
        .chain(reviewers)
        .chain([judges])
        .filter(|c| c.calls > 0)
        .collect();
    let dollars = first_round
        .iter()
        .chain(fix.as_ref())
        .any(|c| !c.subscription);
    let mut unpriced = Vec::new();
    for c in first_round.iter().chain(fix.as_ref()) {
        if c.usd.is_none() && !c.subscription {
            let model = cfg
                .allowed_bindings
                .get(&c.binding)
                .and_then(|b| b.model.as_deref());
            let label = match model {
                Some(model) => format!("{} ({model})", c.binding),
                None => c.binding.clone(),
            };
            if !unpriced.contains(&label) {
                unpriced.push(label);
            }
        }
    }
    let mut per_call: Vec<(Role, PerCall)> = Vec::new();
    for c in &first_round {
        let extra = cfg.per_call(c.role);
        if extra != PerCall::default() && !per_call.iter().any(|(r, _)| *r == c.role) {
            per_call.push((c.role, extra));
        }
    }
    let mut escalates: Vec<String> = config_escalation(cfg, change).into_iter().collect();
    escalates.extend(
        cfg.always_escalate
            .iter()
            .map(|e| format!("if the Orchestrator finds: {e}")),
    );
    Ok(Plan {
        tier: tier.id.clone(),
        tier_decided: by_paths.is_some(),
        fan_out: tier.fan_out,
        lenses,
        files: change.files.len(),
        first_round,
        fix,
        least,
        expected,
        history_runs: history.runs,
        most,
        dollars,
        unpriced,
        per_call,
        escalates,
        generated: change.generated.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::super::config::{Flags, parse_repo, parse_user, resolve};
    use super::*;

    const GO: &str = include_str!("../../examples/acr/go.yaml");
    const USER: &str = include_str!("../../examples/acr/user.yaml");

    fn example() -> Config {
        let user = parse_user(USER).unwrap();
        resolve(&user, parse_repo(GO).unwrap(), &Flags::default()).unwrap()
    }

    fn change(files: &[&str], diff: &str) -> Change {
        Change {
            files: files.iter().map(|f| f.to_string()).collect(),
            diff: diff.to_string(),
            ..Change::default()
        }
    }

    fn dry(cfg: &Config, change: &Change) -> Result<Plan> {
        dry_run(cfg, change, &History::default())
    }

    fn ids(plan: &Plan) -> Vec<&str> {
        plan.lenses.iter().map(|l| l.id.as_str()).collect()
    }

    #[test]
    fn a_path_tier_needs_no_sizer() {
        let cfg = example();
        let plan = dry(&cfg, &change(&["docs/a.md", "b.md"], "+words\n")).unwrap();
        assert_eq!(plan.tier, "docs");
        assert!(plan.tier_decided);
        assert_eq!(plan.fan_out, 1);
        assert!(!plan.first_round.iter().any(|c| c.role == Role::Sizer));
    }

    #[test]
    fn unsure_means_the_higher_tier() {
        let cfg = example();
        let plan = dry(&cfg, &change(&["a.go", "b.md"], "+x\n")).unwrap();
        assert_eq!(plan.tier, "core");
        assert!(!plan.tier_decided);
        assert_eq!(plan.fan_out, 3);
        assert!(plan.first_round.iter().any(|c| c.role == Role::Sizer));
    }

    #[test]
    fn lenses_come_from_their_triggers() {
        let cfg = example();
        let plan = dry(&cfg, &change(&["pkg/a.go"], "+\tgo func() {}()\n")).unwrap();
        let chosen = ids(&plan);
        for id in ["correctness", "goroutine-safety"] {
            assert!(chosen.contains(&id), "{id}");
        }
        assert!(!chosen.contains(&"storage"));
        // A description might apply, so it's counted in.
        assert!(chosen.contains(&"http-compatibility"));
        let store = dry(&cfg, &change(&["internal/store/s.go"], "+x\n")).unwrap();
        assert!(ids(&store).contains(&"storage"));
        // Content in a file header doesn't count as the change.
        let header = dry(&cfg, &change(&["a.txt"], "+++ b/sync.Mutex\n")).unwrap();
        assert!(!ids(&header).contains(&"goroutine-safety"));
    }

    #[test]
    fn a_tier_can_require_a_lens() {
        let mut cfg = example();
        cfg.tiers[0].required.push("correctness".into());
        let plan = dry(&cfg, &change(&["a.md"], "+x\n")).unwrap();
        assert_eq!(plan.tier, "docs");
        assert!(ids(&plan).contains(&"correctness"));
    }

    #[test]
    fn a_dry_run_estimates_its_cost() {
        let cfg = example();
        let plan = dry(&cfg, &change(&["a.go"], "+x\n")).unwrap();
        let reviews: u64 = plan
            .first_round
            .iter()
            .filter(|c| c.role == Role::Reviewer)
            .map(|c| c.calls)
            .sum();
        assert_eq!(reviews, plan.lenses.len() as u64 * 3);
        assert!(plan.most.usd.unwrap() > 0.0);
        let first: f64 = plan.first_round.iter().map(|c| c.usd.unwrap()).sum();
        assert!(plan.most.usd.unwrap() > first);
        assert!(plan.unpriced.is_empty());
    }

    #[test]
    fn with_no_prices_it_says_it_cant_tell_and_which() {
        let mut cfg = example();
        cfg.costs.prices.clear();
        let plan = dry(&cfg, &change(&["a.go"], "+x\n")).unwrap();
        assert_eq!(plan.most.usd, None);
        assert!(
            plan.unpriced
                .contains(&"medium (example/medium-model)".to_string()),
            "{:?}",
            plan.unpriced
        );
    }

    fn usd(plan: &Plan, role: Role) -> f64 {
        plan.first_round
            .iter()
            .filter(|c| c.role == role)
            .map(|c| c.usd.unwrap())
            .sum()
    }

    #[test]
    fn the_most_is_every_round_with_a_fix_between() {
        let cfg = example();
        let plan = dry(&cfg, &change(&["a.go"], "+x\n")).unwrap();
        let setup = usd(&plan, Role::Sizer) + usd(&plan, Role::Orchestrator);
        let round = usd(&plan, Role::Reviewer) + usd(&plan, Role::Judge);
        let fix = plan.fix.as_ref().unwrap().usd.unwrap();
        let max = f64::from(cfg.max_rounds);
        let close = |a: f64, b: f64| (a - b).abs() < 1e-9;
        assert!(close(plan.least.usd.unwrap(), setup + round));
        assert!(close(
            plan.most.usd.unwrap(),
            setup + round * max + fix * (max - 1.0)
        ));
        assert!(plan.expected.is_none(), "no history, no expectation");
        let mut once = cfg.clone();
        once.max_rounds = 1;
        let plan = dry(&once, &change(&["a.go"], "+x\n")).unwrap();
        assert!(plan.fix.is_none());
        assert!(close(plan.most.usd.unwrap(), plan.least.usd.unwrap()));
    }

    #[test]
    fn the_expected_cost_goes_by_the_rounds_lenses_took_before() {
        let cfg = example();
        let go = change(&["a.go"], "+x\n");
        let base = dry(&cfg, &go).unwrap();
        // goroutine-safety took 1 and 3 rounds; the rest have no history of
        // their own, so they take every lens's average, 2.
        let history = History {
            runs: 2,
            lens_rounds: BTreeMap::from([("goroutine-safety".to_string(), vec![1, 3])]),
            fixes: vec![0, 2],
        };
        let plan = dry_run(&cfg, &go, &history).unwrap();
        let setup = usd(&base, Role::Sizer) + usd(&base, Role::Orchestrator);
        let round = usd(&base, Role::Reviewer) + usd(&base, Role::Judge);
        let fix = base.fix.as_ref().unwrap().usd.unwrap();
        let want = setup + round * 2.0 + fix * 1.0;
        assert!((plan.expected.unwrap().usd.unwrap() - want).abs() < 1e-9);
        assert_eq!(plan.history_runs, 2);
        // Never past max_rounds, whatever happened before.
        let long = History {
            runs: 1,
            lens_rounds: BTreeMap::from([("x".to_string(), vec![9])]),
            fixes: vec![8],
        };
        let plan = dry_run(&cfg, &go, &long).unwrap();
        assert!((plan.expected.unwrap().usd.unwrap() - plan.most.usd.unwrap()).abs() < 1e-9);
    }

    #[test]
    fn on_a_subscription_it_counts_tokens_and_no_dollars() {
        let mut cfg = example();
        for b in cfg.allowed_bindings.values_mut() {
            b.subscription = true;
        }
        let plan = dry(&cfg, &change(&["a.go"], "+x\n")).unwrap();
        assert!(!plan.dollars);
        assert!(plan.unpriced.is_empty(), "a subscription needs no price");
        assert!(plan.least.tokens > 0 && plan.most.tokens > plan.least.tokens);
        assert!(
            plan.first_round
                .iter()
                .all(|c| c.subscription && c.usd.is_none())
        );
        // One billed binding brings dollars back, for its calls alone.
        cfg.allowed_bindings.get_mut("medium").unwrap().subscription = false;
        let mixed = dry(&cfg, &change(&["a.go"], "+x\n")).unwrap();
        assert!(mixed.dollars);
        let reviewers: f64 = mixed
            .first_round
            .iter()
            .filter(|c| c.role == Role::Reviewer)
            .map(|c| c.usd.unwrap())
            .sum();
        assert!((mixed.least.usd.unwrap() - reviewers).abs() < 1e-9);
        assert_eq!(mixed.least.tokens, plan.least.tokens, "tokens don't change");
    }

    #[test]
    fn tokens_need_no_price() {
        let mut cfg = example();
        cfg.costs.prices.clear();
        let plan = dry(&cfg, &change(&["a.go"], "+x\n")).unwrap();
        assert_eq!(plan.most.usd, None);
        assert!(plan.least.tokens > 0 && plan.most.tokens > plan.least.tokens);
    }

    #[test]
    fn a_lens_and_its_judge_are_priced_on_the_diff_they_get() {
        let mut cfg = example();
        cfg.costs.per_call.clear();
        let small = "+x\n".to_string();
        let big = format!("+x\n{}", "+generated line\n".repeat(4000));
        let mut go = change(&["a.go"], &small);
        go.full = Some(big.clone());
        let lean = dry(&cfg, &go).unwrap();
        let everything = dry(&cfg, &change(&["a.go"], &big)).unwrap();
        let reviewer_in = |p: &Plan| -> u64 {
            p.first_round
                .iter()
                .filter(|c| c.role == Role::Reviewer)
                .map(|c| c.tokens_in)
                .sum()
        };
        assert!(reviewer_in(&lean) < reviewer_in(&everything));
        // A Judge reads the diff its lens's reviewers got.
        let judge_in = |p: &Plan| {
            p.first_round
                .iter()
                .find(|c| c.role == Role::Judge)
                .unwrap()
                .tokens_in
        };
        assert!(judge_in(&lean) < judge_in(&everything));
        // A lens that takes generated files is priced on all of it.
        let id = lean.lenses[0].id.clone();
        cfg.lenses
            .iter_mut()
            .find(|l| l.id == id)
            .unwrap()
            .include_generated = true;
        let opted = dry(&cfg, &go).unwrap();
        assert!(reviewer_in(&opted) > reviewer_in(&lean));
    }

    #[test]
    fn every_call_adds_what_its_role_takes() {
        let cfg = example();
        let mut bare = cfg.clone();
        bare.costs.per_call.clear();
        let go = change(&["a.go"], "+x\n");
        let (with, without) = (dry(&cfg, &go).unwrap(), dry(&bare, &go).unwrap());
        let reviewers = |p: &Plan| {
            p.first_round
                .iter()
                .filter(|c| c.role == Role::Reviewer)
                .fold((0, 0, 0), |t, c| {
                    (t.0 + c.calls, t.1 + c.tokens_in, t.2 + c.tokens_out)
                })
        };
        let (calls, tokens_in, tokens_out) = reviewers(&with);
        let (_, bare_in, bare_out) = reviewers(&without);
        assert_eq!(tokens_in - bare_in, calls * 40_000);
        assert_eq!(tokens_out - bare_out, calls * 6_000);
        assert!(with.most.usd.unwrap() > without.most.usd.unwrap());
        assert!(with.per_call.iter().any(|(r, _)| *r == Role::Reviewer));
        assert!(without.per_call.is_empty());
    }

    #[test]
    fn a_change_to_the_config_or_its_list_escalates() {
        let cfg = example();
        let own = change(&[super::super::config::REPO_CONFIG], "+x\n");
        assert!(config_escalation(&cfg, &own).unwrap().contains("edits"));
        let list = change(&["not-findings.md"], "+x\n");
        assert!(config_escalation(&cfg, &list).is_some());
        assert!(config_escalation(&cfg, &change(&["a.go"], "+x\n")).is_none());
        // Kept outside the repository, no change can touch either.
        let mut outside = cfg.clone();
        outside.outside = true;
        assert!(config_escalation(&outside, &own).is_none());
        assert!(config_escalation(&outside, &list).is_none());
    }
}
