//! What each role is asked, and what it must answer. Answers are JSON, and
//! one that can't be read is a failure, never an empty result.

use anyhow::{Context, Result, bail};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use super::config::{Lens, Severity, Tier};

/// The JSON in an answer, which a model may have wrapped in other words.
fn json_in<T: DeserializeOwned>(text: &str) -> Result<T> {
    let start = text.find('{').context("the answer has no JSON")?;
    let end = text.rfind('}').context("the answer has no JSON")?;
    if end < start {
        bail!("the answer has no JSON");
    }
    serde_json::from_str(&text[start..=end]).context("reading the answer's JSON")
}

#[derive(Debug, Deserialize)]
pub struct Sized {
    pub tier: String,
    #[serde(default)]
    pub sure: bool,
    #[serde(default)]
    pub reason: String,
}

pub fn sizer_prompt(tiers: &[&Tier], files: &[String]) -> String {
    let rules: String = tiers
        .iter()
        .filter_map(|t| {
            let description = t.rule.description.as_ref()?;
            Some(format!("- {}: {description}\n", t.id))
        })
        .collect();
    format!(
        "Classify the attached change into one tier. The tiers run from lowest to highest:\n\n\
         {rules}\nThe changed files are: {}.\n\n\
         Answer with only JSON: {{\"tier\": \"<id>\", \"sure\": true or false, \
         \"reason\": \"<one line>\"}}. If you're not sure, say so.",
        files.join(", ")
    )
}

pub fn read_sized(text: &str) -> Result<Sized> {
    json_in(text)
}

#[derive(Debug, Default, Deserialize)]
pub struct Selected {
    /// Lenses whose descriptions apply.
    #[serde(default)]
    pub lenses: Vec<String>,
    /// `always_escalate` items that apply.
    #[serde(default)]
    pub escalate: Vec<String>,
}

pub fn orchestrator_prompt(lenses: &[(&str, &str)], escalate: &[String]) -> String {
    let lens_list: String = lenses
        .iter()
        .map(|(id, d)| format!("- {id}: {d}\n"))
        .collect();
    let escalate_list: String = escalate.iter().map(|e| format!("- {e}\n")).collect();
    format!(
        "Read the attached change, and say which of these apply to it.\n\n\
         Lenses, each with when it applies:\n{lens_list}\n\
         Things that must go to a person:\n{escalate_list}\n\
         Answer with only JSON: {{\"lenses\": [\"<id>\"], \"escalate\": [\"<item, word for word>\"]}}. \
         When in doubt, include it."
    )
}

pub fn read_selected(text: &str) -> Result<Selected> {
    json_in(text)
}

/// One thing a reviewer found.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Raw {
    pub title: String,
    #[serde(default)]
    pub detail: String,
    #[serde(default)]
    pub file: Option<String>,
    #[serde(default)]
    pub line: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct Reviewed {
    findings: Vec<Raw>,
}

/// A reviewer's whole brief: the lens, the scope, and what's already been
/// decided. It gets nothing else: not the author's reasoning, and not the
/// rebuttals of earlier rounds. The artifact is the attached file.
pub fn reviewer_prompt(lens: &Lens, scope: &str, not_findings: &str) -> String {
    let decided = match not_findings.trim() {
        "" => "None.".to_string(),
        text => text.to_string(),
    };
    format!(
        "You are the reviewer for {id}. Find what is wrong.\n\n\
         What wrong means here: {criteria}\n\n\
         Scope: {scope}. The change is the attached diff. Attack that, and nothing outside this \
         lens. Your working directory is the repository, as the change leaves it: read any of \
         it you need, but change nothing.\n\n\
         Not findings, which are already decided, so leave them out:\n{decided}\n\n\
         Answer with only JSON: {{\"findings\": [{{\"title\": \"<one line>\", \"detail\": \
         \"<the evidence>\", \"file\": \"<path>\", \"line\": <number>}}]}}. If you find nothing, \
         say so with an empty list.",
        id = lens.id,
        criteria = lens.criteria,
    )
}

pub fn read_findings(text: &str) -> Result<Vec<Raw>> {
    Ok(json_in::<Reviewed>(text)?.findings)
}

/// A judged finding, merged across a lens's reviewers.
#[derive(Debug, Clone, Deserialize)]
pub struct Rating {
    pub title: String,
    #[serde(default)]
    pub detail: String,
    #[serde(default)]
    pub file: Option<String>,
    #[serde(default)]
    pub line: Option<u64>,
    pub severity: Severity,
    #[serde(default = "yes")]
    pub relevant: bool,
    #[serde(default = "yes")]
    pub on_task: bool,
    #[serde(default)]
    pub reason: String,
    /// The reviewers disagreed about it.
    #[serde(default)]
    pub split: bool,
    /// Rejecting it is a values call, not a fact.
    #[serde(default)]
    pub values_call: bool,
    /// Both sides, when it's split or a values call.
    #[serde(default)]
    pub sides: Vec<String>,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Deserialize)]
struct Judged {
    findings: Vec<Rating>,
}

pub fn judge_prompt(lens: &Lens, reviews: &[Vec<Raw>], not_findings: &str) -> Result<String> {
    Ok(format!(
        "Judge these findings from {n} reviewer(s) of the {id} lens. Its criteria: {criteria}\n\n\
         The change is the attached diff, and your working directory is the repository, as the \
         change leaves it. Check each finding against the code. Change nothing in the \
         repository: you repair nothing. To test a claim, you may write and run a script, but \
         only under $TMPDIR, which is yours for this call and goes when it ends.\n\n\
         Merge findings that are the same defect into one. Rate each as:\n\
         - Critical: wrong result, data loss, security hole\n\
         - High: real defect in a realistic case\n\
         - Medium: real defect in a narrow case\n\
         - Low: nit or unlikely edge case\n\
         - Rejected: off task, noise, or a decided non-issue, with the reason\n\n\
         Decided non-issues:\n{not_findings}\n\n\
         Set split when the reviewers disagree about a finding. Set values_call when rejecting it \
         comes down to values, not fact. Give both sides in `sides` when you set either.\n\n\
         The reviewers' findings, one list each:\n{findings}\n\n\
         Answer with only JSON: {{\"findings\": [{{\"title\", \"detail\", \"file\", \"line\", \
         \"severity\", \"relevant\", \"on_task\", \"reason\", \"split\", \"values_call\", \
         \"sides\"}}]}}.",
        n = reviews.len(),
        id = lens.id,
        criteria = lens.criteria,
        findings = serde_json::to_string_pretty(reviews)?,
    ))
}

pub fn read_ratings(text: &str) -> Result<Vec<Rating>> {
    Ok(json_in::<Judged>(text)?.findings)
}

pub fn fix_prompt(blockers: &str) -> String {
    format!(
        "Fix these blockers in the working directory. Change only what fixing them needs, and \
         don't commit.\n\n{blockers}\n\n\
         Answer with only JSON: {{\"done\": \"<what you changed, in a line or two>\"}}."
    )
}

#[derive(Debug, Deserialize)]
pub struct Fixed {
    pub done: String,
}

pub fn read_fixed(text: &str) -> Result<Fixed> {
    json_in(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_json_among_other_words() {
        let text = "Here you go:\n```json\n{\"findings\": []}\n```\nDone.";
        assert!(read_findings(text).unwrap().is_empty());
    }

    #[test]
    fn an_unreadable_answer_is_an_error_not_an_empty_result() {
        for text in [
            "no findings!",
            "",
            "{\"findings\": ",
            "}{",
            "{\"other\": 1}",
        ] {
            assert!(read_findings(text).is_err(), "{text}");
        }
    }

    #[test]
    fn a_brief_holds_what_a_reviewer_needs_and_only_that() {
        let lens = Lens {
            id: "lens-x".into(),
            criteria: "the criteria".into(),
            triggers: Default::default(),
            binding: None,
            severity: Default::default(),
            include_generated: false,
        };
        let brief = reviewer_prompt(&lens, "the whole change", "- decided thing");
        for part in [
            "lens-x",
            "the criteria",
            "the whole change",
            "- decided thing",
        ] {
            assert!(brief.contains(part), "{part}");
        }
        assert!(brief.contains("Find what is wrong"));
        assert!(!brief.contains("check this"));
    }

    #[test]
    fn a_rating_has_a_fixed_severity() {
        let ok = r#"{"findings": [{"title": "t", "severity": "High"}]}"#;
        assert_eq!(read_ratings(ok).unwrap()[0].severity, Severity::High);
        let bad = r#"{"findings": [{"title": "t", "severity": "Spicy"}]}"#;
        assert!(read_ratings(bad).is_err());
    }
}
