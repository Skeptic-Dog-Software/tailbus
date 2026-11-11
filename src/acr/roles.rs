//! What each role is asked, and what it must answer. Answers are JSON, and
//! one that can't be read is a failure, never an empty result.

use anyhow::{Result, bail};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use super::config::{Lens, Severity, Tier};

/// The JSON in an answer, which a model may have wrapped in other words,
/// braces among them: the first whole object in it that reads as `T`. If
/// none does, the error is the first object's.
pub(super) fn json_in<T: DeserializeOwned>(text: &str) -> Result<T> {
    let mut first_error = None;
    let mut from = 0;
    while let Some(start) = text[from..].find('{').map(|i| from + i) {
        let mut objects =
            serde_json::Deserializer::from_str(&text[start..]).into_iter::<serde_json::Value>();
        match objects.next() {
            Some(Ok(value)) => {
                from = start + objects.byte_offset();
                match serde_json::from_value(value) {
                    Ok(answer) => return Ok(answer),
                    Err(e) => {
                        first_error.get_or_insert(e);
                    }
                }
            }
            _ => from = start + 1,
        }
    }
    match first_error {
        Some(e) => Err(anyhow::Error::new(e).context("reading the answer's JSON")),
        None => bail!("the answer has no JSON"),
    }
}

/// Readers for what models write where a strict one would throw a whole
/// answer away: a range for a line, a severity in lower case, one side as a
/// string, a flag as a word.
mod lenient {
    use serde::Deserialize;
    use serde::de::{Deserializer, Error};
    use serde_json::Value;

    use super::Severity;

    /// A line, or the first number in text such as "169-171" or "L42".
    pub fn line<'de, D: Deserializer<'de>>(d: D) -> Result<Option<u64>, D::Error> {
        Ok(match Value::deserialize(d)? {
            Value::Number(n) => n.as_u64(),
            Value::String(s) => {
                let digits: String = s
                    .chars()
                    .skip_while(|c| !c.is_ascii_digit())
                    .take_while(char::is_ascii_digit)
                    .collect();
                digits.parse().ok()
            }
            _ => None,
        })
    }

    /// A severity in any case, such as "high", or "High: a real defect".
    pub fn severity<'de, D: Deserializer<'de>>(d: D) -> Result<Severity, D::Error> {
        let text = String::deserialize(d)?;
        let word: String = text
            .trim()
            .chars()
            .take_while(char::is_ascii_alphabetic)
            .collect::<String>()
            .to_ascii_lowercase();
        Ok(match word.as_str() {
            "critical" => Severity::Critical,
            "high" => Severity::High,
            "medium" => Severity::Medium,
            "low" => Severity::Low,
            "rejected" | "reject" => Severity::Rejected,
            _ => return Err(D::Error::custom(format!("{text:?} isn't a severity"))),
        })
    }

    /// True or false, or the words for them.
    pub fn flag<'de, D: Deserializer<'de>>(d: D) -> Result<bool, D::Error> {
        match Value::deserialize(d)? {
            Value::Bool(b) => Ok(b),
            Value::String(s) => match s.trim().to_ascii_lowercase().as_str() {
                "true" | "yes" => Ok(true),
                "false" | "no" => Ok(false),
                _ => Err(D::Error::custom(format!("{s:?} isn't true or false"))),
            },
            other => Err(D::Error::custom(format!("{other} isn't true or false"))),
        }
    }

    /// A list of strings, or one string.
    pub fn strings<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
        Ok(match Value::deserialize(d)? {
            Value::Null => Vec::new(),
            Value::String(s) => vec![s],
            Value::Array(items) => items
                .into_iter()
                .map(|v| match v {
                    Value::String(s) => s,
                    other => other.to_string(),
                })
                .collect(),
            other => vec![other.to_string()],
        })
    }
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
        "Classify the attached change into one tier. It starts with what the change says \
         it's for, then its diff. The tiers run from lowest to highest:\n\n\
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
        "Read the attached change, which starts with what's said about it, then its diff, \
         and say which of these apply to it.\n\n\
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
    #[serde(default, deserialize_with = "lenient::line")]
    pub line: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct Reviewed {
    findings: Vec<Raw>,
}

/// For every role that runs things. One reviewer on a dependency bump spent
/// 4.3M of its 12.3M tokens checking on a test run it had started in the
/// background, 28 times, each check rereading a context of up to 174K tokens.
pub(super) const WAIT: &str = "Run a long command in the foreground, with a time limit, and \
     wait for it. Don't start it in the background and keep checking on it: each check \
     rereads everything you've read so far, which can cost more than the run.";

/// A reviewer's whole brief: the lens, the scope, and what's already been
/// decided. The attached file is the change: what's said about it, then its
/// diff. Those are its claims; the reviewer gets nothing of the case made
/// for them, and none of the rebuttals of earlier rounds.
/// A reviewer's brief. One with `own_copy` works in a copy of the checkout
/// that's its alone, and is told to settle doubts by running things there.
pub fn reviewer_prompt(
    lens: &Lens,
    scope: &str,
    not_findings: &str,
    own_copy: bool,
    guide: &str,
) -> String {
    let decided = match not_findings.trim() {
        "" => "None.".to_string(),
        text => text.to_string(),
    };
    // What blocks in this lens, from the repository's calibration.
    let blocks = match lens.blocker.as_deref().map(str::trim) {
        Some(b) if !b.is_empty() => format!("What blocks here: {b}\n\n"),
        _ => String::new(),
    };
    let workdir = if own_copy {
        format!(
            "Your working directory is your own copy of the repository, as the change leaves \
             it, and nothing you do there reaches the change. Read any of it you need. Where \
             reading leaves a doubt, settle it by running something: the tests that cover the \
             change, a probe of what it claims, or a mutation, breaking the code a test should \
             guard to see whether the test fails. Report a test that still passes against \
             broken code. Nobody is here to grant a permission, so a shell command that would \
             need one is refused: name paths in full, not through variables or `$(...)`, and \
             put a probe of more than one step in a script, written with the Write tool and run \
             by its path. {WAIT}"
        )
    } else {
        "Your working directory is the repository, as the change leaves it: read any of it \
         you need, but change nothing."
            .to_string()
    };
    let attached = super::change::ATTACHED;
    format!(
        "You are the reviewer for {id}. Find what is wrong: assume defects exist until \
         you've shown otherwise.\n\n\
         What wrong means here: {criteria}\n\n\
         {blocks}\
         Scope: {scope}. {attached} Attack that, and nothing outside \
         this lens. What's said about it, and the comments and docs the change adds, are claims \
         to check, not to take on trust: check them against the code and tests too, and \
         report one they don't bear out. Report what's wrong in the code the change touches or relies on even if it \
         was wrong before the change, and say it's pre-existing: that it isn't the change's \
         doing doesn't make it not a finding. {workdir}\n\n\
         {guide}\
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
    #[serde(default, deserialize_with = "lenient::line")]
    pub line: Option<u64>,
    #[serde(deserialize_with = "lenient::severity")]
    pub severity: Severity,
    /// It's a real defect, whatever caused it: false only for something
    /// that isn't a defect at all.
    #[serde(
        default = "yes",
        rename = "real_defect",
        deserialize_with = "lenient::flag"
    )]
    pub relevant: bool,
    #[serde(default = "yes", deserialize_with = "lenient::flag")]
    pub on_task: bool,
    #[serde(default)]
    pub reason: String,
    /// The reviewers disagreed about it.
    #[serde(default, deserialize_with = "lenient::flag")]
    pub split: bool,
    /// Rejecting it is a values call, not a fact.
    #[serde(default, deserialize_with = "lenient::flag")]
    pub values_call: bool,
    /// Both sides, when it's split or a values call.
    #[serde(default, deserialize_with = "lenient::strings")]
    pub sides: Vec<String>,
    /// Facts the Judge states, from which rules decide what blocks. The
    /// defect was there before the change.
    #[serde(default, deserialize_with = "lenient::flag")]
    pub pre_existing: bool,
    /// The change says it fixes it.
    #[serde(default, deserialize_with = "lenient::flag")]
    pub claims_fix: bool,
    /// The change makes it worse.
    #[serde(default, deserialize_with = "lenient::flag")]
    pub worsens: bool,
    /// A test the change adds or changes that can't fail when what it's
    /// there to pin breaks.
    #[serde(default, deserialize_with = "lenient::flag")]
    pub weak_pin: bool,
    /// A claim the change makes that would lead someone relying on it to
    /// ship a defect or skip a test.
    #[serde(default, deserialize_with = "lenient::flag")]
    pub misleads: bool,
    /// For `weak_pin`, the test's name as the change writes it. The rules
    /// hold the fact only when the change adds that text.
    #[serde(default, deserialize_with = "lenient::strings")]
    pub weak_pin_test: Vec<String>,
    /// For `misleads`, the claim's own words, quoted from the change. The
    /// rules hold the fact only when the change adds that text.
    #[serde(default, deserialize_with = "lenient::strings")]
    pub misleading_claim: Vec<String>,
    /// It's a blocker by the lens's own definition of one.
    #[serde(default, deserialize_with = "lenient::flag")]
    pub meets_blocker: bool,
    /// For `meets_blocker`, the words of the definition it meets. The rules
    /// hold the fact only when the definition has them.
    #[serde(default, deserialize_with = "lenient::strings")]
    pub blocker_clause: Vec<String>,
    /// A deliberate break of the code, a mutant, that changes nothing a
    /// user or caller could see. Only ever about a mutant.
    #[serde(
        default,
        rename = "equivalent_mutant",
        deserialize_with = "lenient::flag"
    )]
    pub equivalent: bool,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Deserialize)]
struct Judged {
    findings: Vec<Rating>,
}

pub fn judge_prompt(lens: &Lens, reviews: &[Vec<Raw>], not_findings: &str) -> Result<String> {
    let attached = super::change::ATTACHED;
    // The lens's own definition of a blocker, from the repository's
    // calibration, and the fact that says a finding meets it.
    let (blocker, blocker_keys) = match lens.blocker.as_deref().map(str::trim) {
        Some(b) if !b.is_empty() => (
            format!(
                "In this lens, a blocker is: {b}\nState `meets_blocker`, true if a finding is a \
                 blocker by that definition, and copy into `blocker_clause` the words of the \
                 definition it meets: a fact without its quote counts as false. A defect that was \
                 there before the change, which the change neither claims to fix nor makes worse, \
                 doesn't block whatever it meets.\n\n"
            ),
            ", \"meets_blocker\", \"blocker_clause\"",
        ),
        _ => (String::new(), ""),
    };
    Ok(format!(
        "Judge these findings from {n} reviewer(s) of the {id} lens. Its criteria: {criteria}\n\n\
         {attached} Your working \
         directory is the repository, as the change leaves it. Check each finding against the \
         code. Change nothing in the \
         repository: you repair nothing. To test a claim, you may write and run a script, but \
         only under $TMPDIR, which is yours for this call and goes when it ends.\n\n\
         Merge findings that are the same defect into one. Rate each by what it costs, given \
         what the change says it's for: a defect that defeats that purpose, or a claim about \
         meeting it that the code and tests don't bear out, costs more than the same defect \
         elsewhere. Rate each as:\n\
         - Critical: wrong result, data loss, security hole\n\
         - High: real defect in a realistic case\n\
         - Medium: real defect in a narrow case\n\
         - Low: nit or unlikely edge case\n\
         - Rejected: a decided non-issue, with the reason\n\n\
         A real defect outside this lens's criteria is rated like any other, with `on_task` \
         false: don't reject it for that.\n\n\
         For each finding, also state these facts, each true or false, from the code: \
         `real_defect`, it's a real defect, whatever caused it and whichever lens it belongs \
         to: false only for something that isn't a defect at all. A missing test, or a \
         comment, doc or ledger line that's incomplete, is a real defect: rate it Low if it's \
         minor; `pre_existing`, the defect is in code as it was before the change, which the change \
         didn't add or alter: a test or line the change adds is never pre-existing; \
         `claims_fix`, the change says it fixes it; `worsens`, the change makes a pre-existing \
         defect worse; `weak_pin`, it's a test the change adds or changes that can't fail when \
         what it's there to pin breaks; `misleads`, it's a claim the change makes, in a \
         comment, doc, ledger line or test name, that would lead someone relying on it to ship \
         a defect or skip a test; with `weak_pin` give the test's name in `weak_pin_test`, \
         and with `misleads` the claim's exact words in `misleading_claim`, both copied from \
         lines the change adds: a fact without its quote counts as false; `equivalent_mutant`, only for a deliberate break of the code \
         (a mutant) that the tests passed: true if that break changes nothing a user or caller \
         could see, and false for every other finding. Fixed rules decide from these facts what \
         blocks: rate the severity for what the defect costs on its own.\n\n\
         {blocker}\
         Decided non-issues:\n{not_findings}\n\n\
         Set split when the reviewers disagree about a finding. Set values_call when rejecting it \
         comes down to values, not fact. Give both sides in `sides` when you set either.\n\n\
         The reviewers' findings, one list each:\n{findings}\n\n\
         Answer with only JSON: {{\"findings\": [{{\"title\", \"detail\", \"file\", \"line\", \
         \"severity\", \"real_defect\", \"on_task\", \"reason\", \"split\", \"values_call\", \
         \"sides\", \"pre_existing\", \"claims_fix\", \"worsens\", \"weak_pin\", \"weak_pin_test\", \
         \"misleads\", \"misleading_claim\", \"equivalent_mutant\"{blocker_keys}}}]}}.",
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

/// The synthesis between rounds of a review to convergence. The
/// Orchestrator holds the change's whole thread, which is why it synthesizes
/// and never attacks.
/// `writer_follows`: a test writer pins the fixes next, so the steelman
/// leaves that to it.
pub fn steelman_prompt(
    intent: &str,
    earlier: &str,
    blockers: &str,
    writer_follows: bool,
    lenses: &str,
) -> String {
    let pins = if writer_follows {
        "Leave the tests that pin these fixes to the test writer who follows you. Run only the \
         existing tests that cover what you change: no mutation checks or whole-suite runs, \
         which the review runs itself."
    } else {
        "Where a blocker comes with a test or scenario that shows it, keep that test as its \
         pin; where a blocker is a gap in the tests, write the test that closes it. Run only \
         the tests that cover what you change: no mutation checks or whole-suite runs."
    };
    let earlier = if earlier.is_empty() {
        "Nothing yet: this is the first.\n"
    } else {
        earlier
    };
    format!(
        "You synthesize this change between rounds of an adversarial review. Fresh reviewers, \
         each with a clean context, attacked it one dimension at a time, and a Judge upheld the \
         blockers below. Make the work the strongest version of itself: the version its most \
         capable advocate would stand behind. You never review it yourself; new reviewers will \
         attack what you leave, on every dimension.\n\n\
         {intent}\n\
         # What earlier rounds changed\n\n{earlier}\n\
         # Blockers\n\n\
         Fold each into the work in the working directory, so that a fresh reviewer finds \
         nothing in it to block, with the smallest change that does it: a fix that reaches \
         code the change didn't touch gives the next round more to attack. If you're sure one \
         is wrong, decline it and say why.\n\n\
         {blockers}\n\
         # How\n\n\
         Fold in these and nothing else: what the Judge upheld without blocking goes to a \
         person, not into the work. Keep the change to what it's for. Where the work claims \
         something about itself, in a comment, a test name, a doc or a ledger row, make the \
         claim true or take it out. {pins} Don't commit.\n\n\
         A declined finding is recorded as a disagreement, not a resolution.\n\n\
         # Brief the next round\n\n\
         You know what you just changed better than anyone the next round sends. For each \
         lens below, brief its reviewer: where your fixes could be wrong, or could have \
         broken something they touch, and how to test it here. Questions to settle, not \
         reassurance. Leave out a lens your fixes can't affect.\n\n{lenses}\n\
         Answer with only JSON: {{\"done\": \"<what you changed, in a few lines>\", \
         \"declined\": [{{\"finding\": \"<its id>\", \"reason\": \"\"}}], \
         \"briefs\": {{\"<lens id>\": \"<the brief>\"}}}}."
    )
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Declined {
    pub finding: String,
    #[serde(default)]
    pub reason: String,
}

#[derive(Debug, Deserialize)]
pub struct Fixed {
    pub done: String,
    /// A steelman's: what it left out, and why.
    #[serde(default)]
    pub declined: Vec<Declined>,
    /// A steelman's briefs for the next round's reviewers, by lens.
    #[serde(default)]
    pub briefs: std::collections::BTreeMap<String, String>,
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
    fn a_judge_holds_a_lens_blocking_condition_and_false_claims() {
        let lens = Lens {
            id: "mutation".into(),
            criteria: "It blocks when the tests claim the behaviour is covered.".into(),
            attack: None,
            blocker: None,
            triggers: Default::default(),
            binding: None,
            severity: Default::default(),
            include_generated: false,
        };
        let prompt = judge_prompt(&lens, &[Vec::new()], "").unwrap();
        assert!(prompt.contains("It blocks when the tests claim the behaviour is covered."));
        assert!(prompt.contains("`weak_pin`"));
        assert!(prompt.contains("a fact without its quote counts as false"));
        assert!(prompt.contains("A missing test, or a comment, doc or ledger line"));
        assert!(prompt.contains("`pre_existing`"));
        assert!(prompt.contains("Fixed rules decide from these facts"));
    }

    #[test]
    fn a_judge_answer_survives_what_models_write() {
        let text = r#"Checked each claim.
```json
{"findings": [
  {"title": "Lost email", "file": "welcome.go", "line": "128-138", "severity": "high",
   "split": "true", "sides": "one reviewer says it's fine"},
  {"title": "Nit", "line": 7, "severity": "Low: an unlikely edge", "real_defect": "no"},
  {"title": "Noise", "line": null, "severity": "REJECTED", "reason": "decided"}
]}
```
Note: `{AGENT}` in the prompt is filled in by the bridge, so it's fine."#;
        let ratings = read_ratings(text).unwrap();
        assert_eq!(ratings.len(), 3);
        assert_eq!(ratings[0].line, Some(128));
        assert_eq!(ratings[0].severity, Severity::High);
        assert!(ratings[0].split);
        assert_eq!(ratings[0].sides, ["one reviewer says it's fine"]);
        assert_eq!(ratings[1].severity, Severity::Low);
        assert!(!ratings[1].relevant);
        assert_eq!(ratings[2].line, None);
        assert_eq!(ratings[2].severity, Severity::Rejected);
        let err = format!(
            "{:#}",
            read_ratings(r#"{"findings": [{"title": "x", "severity": "dire"}]}"#).unwrap_err()
        );
        assert!(err.contains("isn't a severity"), "{err}");
    }

    #[test]
    fn the_answer_is_the_first_object_that_fits() {
        let text = "An example: {\"note\": 1}. The answer: {\"findings\": []} {stray";
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
            attack: None,
            blocker: None,
            triggers: Default::default(),
            binding: None,
            severity: Default::default(),
            include_generated: false,
        };
        let brief = reviewer_prompt(&lens, "the whole change", "- decided thing", false, "");
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
        assert!(brief.contains("change nothing") && !brief.contains("mutation"));
        // One with a copy of its own is told to run and break things there.
        let running = reviewer_prompt(
            &lens,
            "the whole change",
            "- decided thing",
            true,
            "# How to attack in this lens\n\nbreak it\n\n",
        );
        assert!(running.contains("# How to attack in this lens\n\nbreak it\n\nNot findings"));
        assert!(running.contains("your own copy") && running.contains("mutation"));
        assert!(!running.contains("change nothing"));
        // It's told to wait on a long run, not keep checking on one; a
        // reviewer that runs nothing isn't.
        assert!(running.contains(WAIT) && !brief.contains(WAIT));
        let probes = super::super::stages::probes_prompt("the whole change", "", None, "");
        let claims = super::super::stages::claims_prompt(&[], "the whole change", "", true, "");
        assert!(probes.contains(WAIT) && claims.contains(WAIT));
    }

    #[test]
    fn a_rating_has_a_fixed_severity() {
        let ok = r#"{"findings": [{"title": "t", "severity": "High"}]}"#;
        assert_eq!(read_ratings(ok).unwrap()[0].severity, Severity::High);
        let bad = r#"{"findings": [{"title": "t", "severity": "Spicy"}]}"#;
        assert!(read_ratings(bad).is_err());
    }
}
