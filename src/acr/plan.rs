//! What a review would do: its tier, its lenses and its cost. This much
//! needs no model, which is what makes a dry run possible.

use anyhow::Result;
use globset::{Glob, GlobSet, GlobSetBuilder};
use regex::Regex;
use serde::Serialize;

use super::change::Change;
use super::config::{Binding, Config, Lens, Role, Tier};

/// Roughly how many bytes make a token.
const BYTES_PER_TOKEN: usize = 4;
/// What a brief adds to the artifact, in tokens.
const BRIEF_TOKENS: u64 = 1500;
/// What a call is assumed to write back, in tokens.
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
    pub tokens: u64,
    /// None if its binding has no prices.
    pub usd: Option<f64>,
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
    /// With every lens going all the way to `max_rounds`.
    pub worst_usd: Option<f64>,
    pub escalates: Vec<String>,
}

pub fn price(binding: &Binding, input: u64, output: u64) -> Option<f64> {
    let per_million = |tokens: u64, rate: f64| tokens as f64 / 1_000_000.0 * rate;
    Some(
        per_million(input, binding.input_per_mtok?) + per_million(output, binding.output_per_mtok?),
    )
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

/// A dry run: everything here comes from config and git, with no model.
pub fn dry_run(cfg: &Config, change: &Change) -> Result<Plan> {
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
    let artifact = tokens_in(change.diff.len()) + BRIEF_TOKENS;
    let fan_out = u64::from(tier.fan_out);
    let needs_orchestrator = chosen.iter().any(|(_, w)| matches!(w, Why::Description(_)))
        || !cfg.always_escalate.is_empty();
    let mut first_round = Vec::new();
    let mut add = |role: Role, lens: Option<&Lens>, calls: u64, input: u64| -> Result<()> {
        if calls == 0 {
            return Ok(());
        }
        let name = cfg.binding_name(role, lens)?.to_string();
        let binding = cfg.allowed_bindings.get(&name);
        let usd = binding
            .and_then(|b| price(b, input, ANSWER_TOKENS))
            .map(|one| one * calls as f64);
        first_round.push(RoleCost {
            role,
            binding: name,
            calls,
            tokens: calls * (input + ANSWER_TOKENS),
            usd,
        });
        Ok(())
    };
    add(Role::Sizer, None, u64::from(by_paths.is_none()), artifact)?;
    add(
        Role::Orchestrator,
        None,
        u64::from(needs_orchestrator),
        artifact,
    )?;
    for (lens, _) in &chosen {
        add(Role::Reviewer, Some(lens), fan_out, artifact)?;
    }
    add(Role::Judge, None, chosen.len() as u64, artifact)?;
    let review_usd: Option<f64> = first_round
        .iter()
        .filter(|c| matches!(c.role, Role::Reviewer | Role::Judge))
        .map(|c| c.usd)
        .sum();
    let setup_usd: Option<f64> = first_round
        .iter()
        .filter(|c| matches!(c.role, Role::Sizer | Role::Orchestrator))
        .map(|c| c.usd)
        .sum();
    let worst_usd = match (review_usd, setup_usd) {
        (Some(review), Some(setup)) => Some(setup + review * f64::from(cfg.max_rounds)),
        _ => None,
    };
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
        worst_usd,
        escalates,
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
        }
    }

    fn ids(plan: &Plan) -> Vec<&str> {
        plan.lenses.iter().map(|l| l.id.as_str()).collect()
    }

    #[test]
    fn a_path_tier_needs_no_sizer() {
        let cfg = example();
        let plan = dry_run(&cfg, &change(&["docs/a.md", "b.md"], "+words\n")).unwrap();
        assert_eq!(plan.tier, "docs");
        assert!(plan.tier_decided);
        assert_eq!(plan.fan_out, 1);
        assert!(!plan.first_round.iter().any(|c| c.role == Role::Sizer));
    }

    #[test]
    fn unsure_means_the_higher_tier() {
        let cfg = example();
        let plan = dry_run(&cfg, &change(&["a.go", "b.md"], "+x\n")).unwrap();
        assert_eq!(plan.tier, "core");
        assert!(!plan.tier_decided);
        assert_eq!(plan.fan_out, 3);
        assert!(plan.first_round.iter().any(|c| c.role == Role::Sizer));
    }

    #[test]
    fn lenses_come_from_their_triggers() {
        let cfg = example();
        let plan = dry_run(&cfg, &change(&["pkg/a.go"], "+\tgo func() {}()\n")).unwrap();
        let chosen = ids(&plan);
        for id in ["correctness", "goroutine-safety"] {
            assert!(chosen.contains(&id), "{id}");
        }
        assert!(!chosen.contains(&"storage"));
        // A description might apply, so it's counted in.
        assert!(chosen.contains(&"http-compatibility"));
        let store = dry_run(&cfg, &change(&["internal/store/s.go"], "+x\n")).unwrap();
        assert!(ids(&store).contains(&"storage"));
        // Content in a file header doesn't count as the change.
        let header = dry_run(&cfg, &change(&["a.txt"], "+++ b/sync.Mutex\n")).unwrap();
        assert!(!ids(&header).contains(&"goroutine-safety"));
    }

    #[test]
    fn a_tier_can_require_a_lens() {
        let mut cfg = example();
        cfg.tiers[0].required.push("correctness".into());
        let plan = dry_run(&cfg, &change(&["a.md"], "+x\n")).unwrap();
        assert_eq!(plan.tier, "docs");
        assert!(ids(&plan).contains(&"correctness"));
    }

    #[test]
    fn a_dry_run_estimates_its_cost() {
        let cfg = example();
        let plan = dry_run(&cfg, &change(&["a.go"], "+x\n")).unwrap();
        let reviews: u64 = plan
            .first_round
            .iter()
            .filter(|c| c.role == Role::Reviewer)
            .map(|c| c.calls)
            .sum();
        assert_eq!(reviews, plan.lenses.len() as u64 * 3);
        assert!(plan.worst_usd.unwrap() > 0.0);
        let first: f64 = plan.first_round.iter().map(|c| c.usd.unwrap()).sum();
        assert!(plan.worst_usd.unwrap() > first);
    }

    #[test]
    fn with_no_prices_it_says_it_cant_tell() {
        let mut cfg = example();
        for b in cfg.allowed_bindings.values_mut() {
            b.input_per_mtok = None;
        }
        let plan = dry_run(&cfg, &change(&["a.go"], "+x\n")).unwrap();
        assert_eq!(plan.worst_usd, None);
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
