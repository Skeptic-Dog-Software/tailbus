---
name: review-score
description: Score a finished review, or compare several, against a key of defects whose truth rests on evidence, not on anyone's ruling. The key comes from defects planted in a copy of the change, from later commits that fixed bugs the change introduced, or from findings proven by tests. A fresh scorer matches each key defect to the reviews' findings (caught, partial or missed), and your user sets only policy, never what's real. Use it after a review when your user wants to know what it missed, or to compare review setups on the same change.
---

# Score a review against a key

A review's findings say what it caught, but not what it missed. This compares a finished review, or several, with a **key**: the defects in the same change. It reports what each caught, half caught and missed, and which lens or check should have caught each miss.

**The key's truth rests on evidence, never on a ruling.** Nobody can reliably rule whether a confident write-up is a real defect: not your user, not an AI scorer, since it reads the same either way. A defect is in the key only because it was planted, because someone later had to fix it, or because a test proves it.

## 1. The key

Use one of these sources, or several, and say which each key defect came from.

- **Planted defects: true by construction.** A fresh agent plants defects in a copy of the change, of the kinds its lenses and checks claim to catch, such as a dropped re-arm, a missing lock, a broken old format or an off-by-one. Each comes with a test that fails with the plant and passes without it. The reviews run on the planted copy, and the key is the list of plants. Run the unplanted change alongside as a control. Plants never go into your user's checkout or a pushed branch.
  - Planted defects can be easier to spot than natural ones, so this measures a ceiling. It's cheap to rerun whenever a review setup changes.
- **Historical defects: true because someone had to fix them.** Find commits that fixed bugs, such as by message, linked issue or revert, and trace each fixed line back with `git blame` on the fix's parent to the change that introduced it. Review that change's head, and the key is what was later fixed, with each fix commit as its proof. Keep only defects present at the head reviewed.
  - These are real bugs that survived the change's own review. They miss bugs nobody has found yet.
- **Proven findings: true because a test shows it.** Pool the findings of every review of the change, and prove each the way the review skill's step 3 does:
  - a test written by an agent from another family than the finding's author;
  - it fails at the head the way the finding says, and passes after the smallest fix;
  - the finding's author agrees it checks the claim.

  A finding that passes is a key defect. A failing test at the base too makes it pre-existing.

**Not in the key:**
- **unproven findings,** with no valid test. List them, with what's missing to settle them.
- **advice:** wording, naming, structure, scope and design, which no test can show. Count it apart, by review, and never score it as a catch or a miss.

**Check the proof step with decoys** whenever the key includes proven findings. Before proving, mix in a few false claims the code refutes, written to read like real findings, and a few planted true ones, so the provers can't tell them apart. If a decoy is "proven", or a plant isn't, the proof step isn't reliable: say so, and mark the score provisional.

## 2. A fresh scorer matches them

Use one fresh agent that took no part in any review or in building the key, from another model family than the reviewers where you can. Brief it with the key, the findings, and the diff. Label each review A, B, C, so the scorer can't tell which setup made which. Tell it:

- **For each key defect, per review:**
  - **caught:** a finding names the same defect, at its place or by its cause;
  - **partial:** a finding is at the right place but gives a wrong or incomplete cause, or gives the cause but misses a path where it happens;
  - **missed:** nothing names it.

  Cite the finding id, and the key defect's test or fix commit, and say why. A finding catches a key defect when that defect's test is what fixing the finding would make pass.
- **For each miss:** which lens or standing check should have caught it, or **none**, when no lens or check covers that kind of defect.
- **For each finding that matches no key defect:** **advice**, or a behavior claim. A behavior claim goes through the proof step in section 1 before it counts: proven, it joins the key, and every review is scored against the grown key. Otherwise it's unproven.
- Read the code to settle a match, with tool calls, and say how many it made.

**Disputed matches go to a second fresh scorer** from another family: every partial, and every catch the first scorer gave with no cited line. Where the two disagree, report both calls. Don't settle them yourself.

## 3. Your user sets policy, not truth

Never ask your user whether a defect is real, or whether a finding matches. Ask them, with AskUserQuestion, only about what's theirs:

- whether pre-existing defects count toward the score;
- whether any kind of defect weighs more, such as a missed replay or compatibility bug against a missed naming one;
- which unproven findings get another try at a test, and how much to spend on it.

Never answer for your user.

## 4. Report

- **The key:** its size and sources (planted, historical, proven), what was left out as unproven or advice, and what was dropped as not at the head, and why.
- **The proof step's decoy check:** how many decoys were "proven" and plants missed, or that there were none.
- **For each review:** caught, partial and missed, out of the key's size; advice, counted apart; its cost and time, if you have them; and the cost per defect caught.
- **Each key defect, by id:** its source and evidence, and what each review did with it.
- **The misses, by the lens or check that should have caught them,** and those no lens or check covers. These are what a recalibration should take up.
- **The scorers:** their models and tool calls, and where the two disagreed.
- **Your user's policy calls.**

Label the scorers as AI agents, for example "Scorer (AI agent)". Say only what the score covers: one change is evidence about that change, not a rate. Planted defects measure a ceiling, and historical ones only what was found later.

When findings or key text can't leave this machine, send only ids, counts and calls to anyone else.
