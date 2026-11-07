//! What a review reports: machine-readable, with adapters for the labels and
//! comments a repository turns on.

use std::collections::BTreeMap;

use serde::Serialize;

use super::config::{CommentFormat, Config, Role, Severity};
use super::flow::{
    Disagreement, Escalation, Finding, FindingState, Gate, LensResult, Outcome, Run, Status, Usage,
};

#[derive(Debug, Serialize)]
pub struct BindingUsed {
    pub role: Role,
    pub binding: String,
    pub agent: String,
    pub model: Option<String>,
    pub calls: u64,
}

#[derive(Debug, Serialize)]
pub struct Report<'a> {
    pub run_id: &'a str,
    pub status: Status,
    pub waiting_for: Option<Gate>,
    pub tier: &'a str,
    pub tier_why: &'a str,
    pub rounds: u32,
    pub lenses: Vec<&'a LensResult>,
    pub escalations: &'a [Escalation],
    pub disagreements: &'a [Disagreement],
    pub bindings_used: Vec<BindingUsed>,
    /// Every call's tokens: the agent's count where it gave one, and an
    /// estimate where it didn't.
    pub tokens: u64,
    pub calls_measured: usize,
    pub calls_estimated: usize,
    /// Calls that came out of a subscription, which count only in tokens.
    pub calls_on_subscription: usize,
    /// For the calls billed in dollars: by the config's prices, or an
    /// agent's where the config has none. None if one of those has no
    /// price, or none was billed in dollars.
    pub usd: Option<f64>,
    /// What the agents said, for the calls they priced.
    pub agent_usd: Option<f64>,
    pub labels: Vec<String>,
}

fn bindings_used(run: &Run) -> Vec<BindingUsed> {
    let mut used: BTreeMap<(Role, String), BindingUsed> = BTreeMap::new();
    for u in &run.usage {
        used.entry((u.role, u.binding.clone()))
            .or_insert_with(|| BindingUsed {
                role: u.role,
                binding: u.binding.clone(),
                agent: u.agent.clone(),
                model: u.model.clone(),
                calls: 0,
            })
            .calls += 1;
    }
    used.into_values().collect()
}

pub fn report<'a>(run: &'a Run, cfg: &Config) -> Report<'a> {
    Report {
        run_id: &run.id,
        status: run.status,
        waiting_for: run.waiting,
        tier: &run.tier,
        tier_why: &run.tier_why,
        rounds: run.round,
        lenses: run.lenses.values().collect(),
        escalations: &run.escalations,
        disagreements: &run.disagreements,
        bindings_used: bindings_used(run),
        tokens: run.usage.iter().map(Usage::tokens).sum(),
        calls_measured: run.usage.iter().filter(|u| u.measured).count(),
        calls_estimated: run.usage.iter().filter(|u| !u.measured).count(),
        calls_on_subscription: run.usage.iter().filter(|u| u.subscription).count(),
        usd: super::flow::dollars(&run.usage),
        agent_usd: run
            .usage
            .iter()
            .filter_map(|u| u.agent_usd)
            .reduce(|a, b| a + b),
        labels: labels(run, cfg),
    }
}

/// The labels a run calls for, if the repository turned them on.
pub fn labels(run: &Run, cfg: &Config) -> Vec<String> {
    let Some(names) = &cfg.integrations.labels else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let done = run.status == Status::Confirmed;
    let overall = if done { &names.ready } else { &names.not_ready };
    out.extend(overall.clone());
    for lens in run.lenses.values() {
        let name = match lens.outcome {
            Outcome::Open => &names.waiting,
            Outcome::Confirmed | Outcome::Waived => &names.confirmed,
        };
        out.extend(name.as_ref().map(|n| n.replace("{lens}", &lens.id)));
    }
    out
}

/// Every label this config could put on a change, so old ones can come off.
pub fn all_labels(run: &Run, cfg: &Config) -> Vec<String> {
    let Some(names) = &cfg.integrations.labels else {
        return Vec::new();
    };
    let mut out: Vec<String> = names
        .ready
        .iter()
        .chain(&names.not_ready)
        .cloned()
        .collect();
    for lens in run.lenses.values() {
        for name in [&names.waiting, &names.confirmed].into_iter().flatten() {
            out.push(name.replace("{lens}", &lens.id));
        }
    }
    out
}

fn finding_line(f: &Finding) -> String {
    let at = f
        .file
        .as_deref()
        .map(|p| format!(" `{p}`"))
        .unwrap_or_default();
    let state = match f.state {
        FindingState::Open => "open",
        FindingState::Folded => "folded in",
        FindingState::Waived => "waived",
        FindingState::Logged => "logged",
        FindingState::Dropped => "rejected",
    };
    let reason = match f.severity {
        Severity::Rejected => format!(" Reason: {}", f.reason),
        _ => String::new(),
    };
    format!(
        "- **{:?}**{at}: {} ({state}).{reason}\n",
        f.severity, f.title
    )
}

/// The PR comment, if the repository turned it on. Reviewers are labeled as
/// the AI agents they are.
pub fn comment(run: &Run, cfg: &Config) -> Option<String> {
    let format = cfg.integrations.comment?;
    let mut out = format!(
        "## ACR: {:?}\n\nTier {} ({}). {} round(s). Reviews by AI agents.\n\n",
        run.status, run.tier, run.tier_why, run.round
    );
    for lens in run.lenses.values() {
        out.push_str(&format!(
            "### {} ({:?})\n\n{}\n\n",
            lens.id, lens.outcome, lens.criteria
        ));
        if format == CommentFormat::Full {
            for round in &lens.rounds {
                out.push_str(&format!(
                    "Round {}: {}\n\n",
                    round.round,
                    round.reviewers.join("; ")
                ));
                for f in &round.findings {
                    out.push_str(&finding_line(f));
                }
                out.push('\n');
            }
        } else if let Some(round) = lens.rounds.last() {
            for f in round.findings.iter().filter(|f| f.blocks()) {
                out.push_str(&finding_line(f));
            }
        }
    }
    for e in &run.escalations {
        out.push_str(&format!("\n**Escalated:** {}\n", e.reason));
    }
    for d in &run.disagreements {
        out.push_str(&format!(
            "\n**Disagreement on {}:** {}\n",
            d.finding,
            d.sides.join(" / ")
        ));
    }
    Some(out)
}
