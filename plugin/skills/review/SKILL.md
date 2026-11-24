---
name: review
description: Whole-change review. Two fresh reviewers from different model families, GLM on high and Opus on medium, each read the entire change at once against a standing list of cross-cutting checks and the repository's review lenses. Each family then proves or refutes the other's findings with tests, run at the head, after the smallest fix and at the base, and the finding's author checks each test; evidence decides what's a defect, and your user decides only what to do about each and which tests to keep, and each lens gets its wait or confirmed label on the PR. It reports; it doesn't fix. Lean on tokens. Use it to review a branch, a PR or a working tree when your user asks for a review and doesn't ask for a full ACR.
---

# Whole-change review

Two reviewers from different model families each read the whole change against a short list of checks that cut across files. Each family then writes tests that prove or refute each finding the other raised, and each finding's author checks the test written for it. Evidence decides what's a defect, and your user decides what to do about each and which tests to keep. That's six model calls in three steps, however many lenses the repository has, plus one for each lens your user names as deep, and one more per family when an author objects to a test. It reports and doesn't fix: your user fixes by their own method, then asks for a new review.

**Why whole-change.** Many real defects live between files: the same kind of message handled on two paths that disagree, a widened helper whose other callers weren't checked, a rule enforced at the step methods but not at Start. A reviewer that reads one dimension, or one path, at a time can miss them. A reader holding the whole change, pointed at those seams, can catch them.

**Lenses without a reviewer each.** A repository's review lenses become criteria both reviewers answer, so covering more of them adds no calls. A lens gets a reviewer of its own only when your user names it deep.

**Why two families.** Models from different families miss different things, so a second family is a second reading at little cost. Each finding is tested by the family that didn't raise it, because a model checking its own finding mostly ratifies it, and a test settles what reading only argues. The session that wrote the change doesn't review it, for the same reason.

## 1. Gather the change

Gather all of this with commands, not with a model.

- **Base and head:** the PR's base branch, or what your user names. Fetch first (`git fetch origin`) and use the remote's branch, `origin/<base>`, since a local branch can be far behind and turn a small change into a large one. With no head, review the working tree against the base.
- **The whole diff:** `git diff <base>...<head>`, or `git diff <base>` for a working tree.
- **The change's claims:** the PR's title and description, if there is one (`gh pr view <n>`). They're claims to check, not to trust.
- **The repository's own checks:** `.review/checks.md`, or a "Review checks" section in CONTRIBUTING.md. Add them to the standing checks, and pass on any non-issues the file lists.
- **The repository's lenses,** when it has a review config: `.acr/config.yaml` in the repository at the base.
  - Match each lens's `triggers` against the diff yourself, and keep the lenses that match.
  - Each lens carries its `id`, `criteria`, `blocker` and `attack`. Take them from there.
  - Leave out a lens that names a `stage`, such as `tests` or `probes`: it judges runs this review doesn't make. Say which you left out.
- **A context pack:** what reviewers would otherwise spend tool calls finding. At the head:
  - for each function, type or constant the change adds or changes, its callers and references (`git grep -n -w <name>`);
  - where the change adds a case to a message kind, enum or switch, every other place that handles the same type;
  - constructors and Start, recovery or restore paths that touch the state the change touches;
  - the tests that name what changed.

  Give each hit as `file:line` with a few lines around it. Keep the pack under about 2,000 lines, and say what you cut.

## 2. Two fresh reviewers, in parallel, through the bus

Call both through bridge agents, so each starts fresh, holds only what you send it, and has what it used counted:

- **GLM on high:** `tailbus call <agent> --model <glm model> --effort high --session fresh`, to an agent that runs GLM, such as a pi bridge.
- **Opus on medium:** `tailbus call <agent> --model claude-opus-5-5 --effort medium --session fresh`, to an agent that runs Opus, such as a Claude Code bridge.

Give each reviewer only its read-only tools with `--tools`, and add `--verbose` so each call prints what it used, for the report. Use the bindings your user names instead, when they name them. If either can't be reached, ask your user whether to go on with one, and say so in the report. Don't give either reviewer your reasoning or this conversation.

Give both the same brief, with the parts they share first, in this order: the role, the checks and lenses, the context pack, the diff, then the PR's claims. A brief over 64 KB goes to the bridge as an attachment by itself, so pass it in one file.

- **Role:** "You are the reviewer for this change. Find what is wrong."
- **The checks below,** plus the repository's own.
- **The lenses:** each one's id, what's wrong in it (`criteria`) and what blocks (`blocker`).
- **Reading:** a few read-only tool calls, only to confirm or rule out a specific suspicion. The pack and the diff are the review's main input. Nothing enforces a number, so the report gives the calls each made.
- **Answer every lens and every check,** with findings, or with what you checked and found sound.
- **A defect that depends on state counts** when you give the sequence of events that triggers it.
- **The answer:** a list of findings. Each has the lens and the check it falls under, or "other"; blocker or non-blocker; `file:line`; the evidence; and whether it's a fact or a judgment call. No findings is a valid answer.

### Deep lenses

A lens your user names as deep for this review also gets a reviewer of its own: one more fresh `tailbus call` on the whole change.
- It gets the same brief, with that lens's `attack` added at the end, so the cache serves everything before it, and the same guidance on reading.
- It runs on the model your user names, or else GLM on high. The other family checks its findings.
- At most two deep lenses per review, unless your user agrees to more after seeing the extra calls.

### Standing checks

1. **Symmetry across paths.** For each concept the change touches, such as a message kind, a state transition or a resource, list every path that handles it: live and replay, sync and async, success and error, start and stop. Show that they agree. Something journaled, counted, released or validated on one path must be on the others.
2. **Blast radius of a widened helper.** When the change widens what a shared function accepts or returns, check every caller, including examples, tests and secondary entry points. Check every analogous kind too: if it adds something for one kind of thing, do its siblings need it?
3. **Every entry seam.** A rule enforced at the step methods must hold at construction, Start, recovery or restore, and shutdown.
4. **Hot-path cost on unbounded data.** Flag per-step work that copies history, walks the whole configuration, or grows with run length when the step touched nothing relevant.
5. **Dead error returns and read-before-check.** Look for errors that are returned but can never be set, or are never checked, and for values read before the check that guards them.

## 3. Each family tests the other's findings

Evidence settles whether a finding is a defect, not a reading of it, and not your user's ruling. Nobody can reliably rule a confident write-up real or not, and a confident write-up reads the same either way.

- **Merge** the two lists. Two findings at the same place about the same defect are one, found by both.
- **Make a copy for the tests,** so nothing touches your user's checkout: `git worktree add --detach <copies>/<review id> <head>`. `<copies>` is a folder the bridges may work in: one of their `options.workdirs`. Remove the copy when the review ends.
- **Sort each finding:**
  - **behavior:** a claim about what the code does, which a test can show;
  - **advice:** a claim about wording, naming, structure, scope or design that no test can show. Advice is never a defect: it never blocks and never sets a lens waiting. It goes to your user as advice, to take or leave.
- **Send each behavior finding to a fresh checker** from the family that didn't raise it: GLM's findings to Opus, Opus's to GLM. A finding both raised goes to Opus. That's one call per family, through the same bridges, fresh and in parallel, each with `--workdir <copy>` and tools to read, write and run. Give it the context pack, the diff and its findings. With no behavior findings, skip this step.
- **For each finding, the checker writes the smallest test that fails at the head if the finding is right:** a unit or integration test in the repository's own framework, a probe script, or a benchmark for a hot-path cost. It runs only that test, not the whole suite, and gives each command and its output. Each finding's test goes in a file of its own, named for the finding, so it can be kept or dropped alone. Then:
  - **at the head:** does it fail the way the finding says?
  - **after the smallest fix:** the checker makes the smallest change that should fix it, runs the test again, and reverts the change. A test that still fails isn't testing this defect.
  - **at the base** (`git -C <copy> checkout <base>`, then back to the head): a test that fails there too shows a defect the change didn't introduce.

  A test that fails for another reason, such as a build error, missing setup or a wrong assumption about the code, is no evidence: the checker fixes the test or says it couldn't write one.
- **The family that raised each finding reads the test written for it,** with one fresh call per family again. It says whether the test checks the claim it made, and if not, what the test misses. A checker can write a test that passes for the wrong reason, and only the finding's author is sure to object. On an objection, the checker gets one more try with it.
- **Each behavior finding's verdict comes from that evidence:**
  - **proven:** the test fails at the head the way the finding says and passes after the smallest fix, and the finding's author agrees it checks the claim. It's **introduced** if the test passes at the base, and **pre-existing** if it fails there too.
  - **refuted:** the test passes at the head, and the finding's author agrees it checks the claim.
  - **unproven:** no valid test, or the author still objects after the second try. Unproven isn't a defect and isn't refuted: say what's missing to settle it.
- **Blocking:** only a proven, introduced defect can block, by its lens's `blocker` when it has one. The checker says which with evidence. A pre-existing defect never blocks.
- **Not defects:** what a compiler, linter or type checker would catch; a change of behavior the PR says it intends; anything the change neither caused nor exposed.
- **Nothing is dropped.** Every finding goes to your user with its sort, its test's results and its verdict.

## 4. Your user decides what to do, not what's true

- Read each finding's cited lines, so you can say what it points at. Don't drop or downgrade a finding yourself, don't change a verdict, and don't fix anything.
- Never ask your user whether a finding is real. Their calls are about policy and priority, with AskUserQuestion:
  - **each proven, introduced defect:** **fix now** or **ticket**. Give the finding, who found it, its tests' results and whether it blocks.
  - **pre-existing defects,** in one question: ticket each, or leave it.
  - **unproven findings,** in one question: another try at a test, with what's missing, or leave it unsettled.
  - **advice,** in one question: take it, ticket it, or leave it.
  - Group them, so it's a few questions, not one per finding. Never answer for your user.
- **Ask which tests to keep,** in one multi-select question listing each finding's test with its results. A kept test is saved as a patch, `.review/tests/<short head>.patch` in your user's checkout, and on their word applied to their working tree with `git apply`. The review never commits it. A proven defect's test is the regression test for its fix.
- Fixing is your user's, by their own method. When they've fixed, they ask for a new review. Give that review the earlier findings, their verdicts and the kept tests. It runs each kept test first: a proven defect whose test now passes is fixed, one whose test still fails still stands, and a refuted finding isn't raised again.

## 5. Labels on the PR

When the change is a PR and the repository's config sets `integrations.labels`, label each lens this review covered:
- **waiting** (such as `wait:{lens}`): a proven, introduced defect in it that your user chose to fix now. It stays waiting until a review after their fix finds that defect's test passing.
- **confirmed** (such as `confirmed:{lens}`): no such defect in it. A ticket, a pre-existing defect, an unproven finding or advice leaves it confirmed.
- **Neither:** a lens left out, or one neither reviewer answered. Never confirm a lens nobody checked.
- **ready** or **not-ready:** not-ready while any lens is waiting. When every lens covered is confirmed, it's ready if none was left out. If some were, ask your user whether to mark it ready, naming them.

Use only labels the repository already has (`gh label list`). They're how everyone who works on it names its lenses, so never create one. If a lens has no label there, don't label it. Tell your user, naming the repository's lens labels closest to it, since it may be one of those under another name.

Show your user the labels. Once they agree, add them and take off this config's labels that no longer apply, with `gh pr edit <n> --add-label … --remove-label …`. A review after your user's fixes sets them again.

## Report

Give your user:
- the change reviewed;
- what was found, by lens and by check, with who found each finding and the checker's verdict, and where the reviewer and the checker disagree on whether it blocks;
- each finding's sort (behavior or advice) and verdict (proven, introduced or pre-existing; refuted; unproven, and what's missing), with its tests' commands and results at the head, after the smallest fix and at the base, and whether its author agreed the test checks the claim;
- what your user chose for each (fix now, ticket, leave, another try), and which tests were kept;
- each lens's outcome (waiting, confirmed, or not checked, and why), and the labels set;
- for a review after fixes, which earlier defects still stand;
- the model and effort each reviewer and checker actually ran on, and the tokens, tool calls and test runs each used;
- what wasn't checked.

Label reviewers and checkers as AI agents, for example "Reviewer (AI agent), GLM".
