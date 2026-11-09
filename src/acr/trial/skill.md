---
name: acr
description: Adversarial convergence review (ACR), builder-led. You brief clean-context reviewers, each sized to its dimension and free to test what it doubts, and you judge what they find, round by round, until nothing blocks.
---

# ACR: adversarial convergence review

This method comes from Ray Karnes's essay "The Adversarial Agent Fleet: Trust, but Verify" (24 May 2026), and from the review process he runs on his repositories, which follows it.

**Why it works this way.** An agent pursues whatever framing it's handed, all the way to a confident conclusion, and its confidence reads the same whether that framing was sound or not. Asking the agent that did the work to check it mostly ratifies what it concluded. The opposition has to come from outside, by structure, not by prompting.

## Roles

- **You orchestrate and judge.** You did the work, so you hold the intent: what the change is for, and why each part matters. That's why you write the briefs, size the reviewers and rule on what they find, and why you never attack the work yourself.
- **Reviewers attack.** Each is a fresh subagent with a clean context. It gets its brief and the repository, and nothing of your reasoning, your case for the work, or earlier rounds' rebuttals.

## The method

1. **Dimensions.** Use the ones your user chose, and skip any the change can't affect. For each, write what "wrong" means for this change in that dimension. Start from the repository's own criteria for it, where it has them, and keep their bar: a reviewer judges by the criteria you give it, and so do you.

2. **Build the strongest version first.** Make the work the version its most capable advocate would stand behind. An attack that survives a strong version tells you something.

3. **Size the change, then each reviewer.** First size the change as a whole: what it touches, and whether it changes logic.
   - A **small or mechanical change**, such as a dependency bump, a config or docs edit, a rename, or generated code, gets a light review. Review only the dimensions it can really affect, and say why you skip the rest. Combine its pattern checks into one reviewer. Size the rest down a step from the table below, and tell those reviewers the change is small, so they should attack in proportion: read the change and what it touches, and run something only to settle a claim.
   - But a small change keeps **one full attack on what it's for**. The dimension that carries its purpose or its risk is sized by the table, or up, and gets the whole attack, tests, probes and mutations included. That might be the trust roots a security bump exists to fix, or the key a one-line crypto change touches. Ask its reviewer whether the change really does what it's for, all of what it does, and whether anything would notice if it didn't.
   - **Any other change** is sized by the table, dimension by dimension.

   Then choose each reviewer's model and effort by what the dimension needs and what a miss would cost. Not one size for all:

   | The dimension needs | Model | Effort |
   | --- | --- | --- |
   | Checks against a known pattern: docs that match the code, naming, config, a checklist | `haiku` | `medium` |
   | Reading and reasoning about the change and what it touches: most correctness, regression and compatibility | `sonnet` | `high` |
   | Deep or adversarial reasoning where a miss is costly: security, crypto, concurrency, data loss, money, migrations, or a large or subtle change | `opus` | `high` |

   Size up for a large or unfamiliar area, or where a silent failure would reach production. Size down where the change only grazes the dimension.

4. **Brief each reviewer.** Every brief has these parts, in this order:
   - **Role and verb:** "You are the reviewer for <dimension>. Find what is wrong." Never "check this", which invites ratification.
   - **The change's purpose:** what it's for and how it does it, in a few sentences. Not your reasoning, and not your case for it.
   - **What wrong means here:** the dimension's criteria, made specific to this change.
   - **Where to attack:** three to six questions about where a defect in this dimension could hide in this change. Ask them as questions; never state your own conclusions.
   - **The artifact:** the range under review (base..head). The reviewer works in the repository, at head.
   - **How to attack:** read the code and its dependencies' source, run the tests, and write probes and mutation tests to see whether the tests would notice a break. Do anything that changes files in a scratch copy: `git worktree add` a throwaway worktree under `$TMPDIR`, and remove it when done. Never change the checkout.
   - **What to return:** each finding marked **blocker** or **non-blocker**, with evidence: the file and line, and what you ran and saw. Say when a finding is a judgment call rather than a fact. End with a list of what you checked, so that finding nothing shows what was looked at.

5. **Run a round.** Start one reviewer per open dimension with the Agent tool, passing the model and effort you chose for each. Run at most 5 at once, and start more as slots free up.

6. **Judge.** You rule on every finding, because you know what the change is for:
   - **Severity:** decide whether it blocks by what it would cost, in use or to the next person who relies on the work. Judge by the dimension's criteria, which can set a stricter bar.
     - A guard whose failure would be silent in production blocks.
     - So does a **false claim the work makes about itself**, when someone would rely on it: a comment, test name, doc or ledger entry, or a guard's stated coverage that says more than the code and tests show. A false "this is tested", "this is demonstrated" or "this stops startup" leaves a gap that nobody will look for.
     - A reviewer's blocker stands unless you can show it's wrong. "Minor", "pre-existing" and "only a comment" are not reasons. If you think it's real but shouldn't block, record that as a disagreement for your user, with both sides, and leave it a blocker until they rule.
   - **Fold** each blocker into the work, making it stronger.
   - **Reject** with a recorded reason. A rejection is a disagreement, not a resolution.
   - Merge findings that are the same defect.

7. **Attack again.** For each dimension that had blockers, brief a fresh reviewer, sized again, against the revised work.

8. **Converge.** A dimension is **confirmed** when its latest round has zero blockers.

9. **At most 3 rounds.** If a dimension still has blockers after round 3, stop and ask your user: one verify-only round, in which only the earlier findings can block; waive the dimension; or stop.

## Disagreement is output

When reviewers split, when a finding is arguable, or when you reject one, record the finding, each side's case, and that it's your user's call. Don't settle values calls yourself.

## The report

- **For each dimension:** its criteria, each round's blockers and non-blockers, how each blocker was folded in, and whether it's confirmed, waived or still open.
- **Disagreements:** each one, with both sides, marked as your user's call.
- **What ran:** each reviewer's model and effort.
- Label reviewers as AI agents in roles, for example "Reviewer (AI agent), correctness". Say only what each review actually covered.
