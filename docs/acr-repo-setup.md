# Setting up ACR review for a repository

How to set up a repository you don't know yet, on a machine that may be new,
so that its review is configured correctly. The pipeline reviews well only
when its reviewers know two things the diff can't tell them. Each lens needs
to know how to attack, and every reviewer needs to know how to work in this
repository on this machine. The brief stage then adds where to attack in
each change. This guide covers what you write once, per repository and
machine.

The review config lives outside the repository, at
`~/.config/tailbus/acr/<repository name>.yaml`, so nothing in a change can
switch off its own review. Its schema is `schema/acr.schema.json`, and
`examples/acr/` has two small configs to start from.

## 1. The machine

```sh
tailbus service install
tailbus setup acr                 # token, bridge, bindings, confinement
tailbus acr add-repo <path>       # let ACR review the repository here
```

Every role's binding comes from `~/.config/tailbus/acr.yaml`. Reviewers do
best when they can run things: give the reviewer binding a shell and the
writing tools. A reviewer with them works in a copy of its own, so nothing
it does reaches the checkout.

```yaml
allowed_bindings:
  reviewer-runs:
    agent: <the ACR agent>@<this machine>
    model: <a mid-size model>
    effort: high
    tools: [Read, Grep, Glob, Bash, Write, Edit]
defaults:
  bindings:
    reviewer: reviewer-runs
```

## 2. Learn the repository first

Before writing the config, find out the following, from the repository's
files, its README and scripts, and its CI:

- **How it builds and tests offline.** List the commands, the caches they
  need, and any environment they assume, such as a time zone or a locale.
  Note anything that reaches for the network on a first run, such as a
  package resolve or a build hook that downloads a native library, and how
  to avoid it.
- **What can hurt users or production.** That means data and migrations,
  keys and secrets, a live backend, purchases, and release builds. These
  become lenses.
- **Where claims live.** Look for a bug ledger, docs that cite code by line,
  and comments that promise behaviour. False claims are findings.
- **Where reviewers will need to read beyond the repository.** That means
  dependency sources, the SDK, and sibling repositories it talks to.
- **What must never run in a review.** For example, anything that touches a
  real backend, a store, or the network.

## 3. Lenses: what's wrong, and how to attack it

Each lens is one dimension of review:

```yaml
lenses:
  - id: regression-safety
    criteria: "No guard or test is weakened: no deleted or loosened assertion, ..."
    attack: "Run the new tests in another time zone, in random order, and one at
      a time; mutation-test in your own copy: each mutant must compile and fail
      a test on an assertion; was an old test replaced by a looser one?"
    triggers:
      paths: ["lib/**", "test/**"]
```

- **`criteria`** is the bar: what counts as wrong in this lens. The Judge
  rates by it.
- **`attack`** gives the techniques that find what the criteria call wrong.
  Write how to find it, not what to check: run something, break something,
  try an edge. For example:
  - mutation testing, where each mutant must compile and fail on an
    assertion;
  - determinism across time zones, orders and repeat runs;
  - network denied, with a positive control;
  - claims checked against the code, line numbers included;
  - a generated-code regeneration compared with what's committed.
- **`triggers`** choose the lens for a change: by paths, content patterns,
  or a plain-language description the Orchestrator judges. Each round
  checks them again, on the change as it is then.
- **Keep everything about a particular change out of the config.** The brief
  stage writes that for each change.

## 4. The runbook: how to work here

The runbook is text that every role able to run things gets, and so does the
brief stage. It's operational. Write it for a reviewer starting cold in a
fresh copy of the repository, on this machine:

```yaml
runbook: |
  In your copy, first run `<offline dependency install>`, then copy
  <the cache a first run would otherwise download> into <where it goes>.
  Then pass <the flag that stops resolving again> to every test and build.
  Run tests with `<the test script> <files>`: it sets <the environment the
  tests need>.
  Wrap any command that could hang: `perl -e 'alarm 600; exec @ARGV' <command>`.
  Never touch <the backend, stores, the network>: no fetch, pull or push.
  Stop only processes you started, by PID: never kill by pattern.
  Read-only sources: dependencies under <path>, the SDK under <path>, and
  sibling repositories under <path>. Read them with grep and cat.
  Before <a command that needs the network>, stop.
```

Then test it. Make a copy of the repository the way a reviewer gets one,
follow the runbook by hand, and run one test file. If anything reaches the
network or fails, fix the runbook.

## 5. Stages and the test command

```yaml
stages: [gate, brief, context, claims, probes, test-writing]
tests:
  setup: "<the offline package install>; <the cache copy>; <a first build or test run>"
  command: "<setup that is cheap to repeat>; <the test script> {tests}"
  coverage: "<setup>; <the test script with coverage>"
  report: coverage/lcov.info
  parallel: 1
  timeout: 900
  mutants: 12
```

- **`gate`** runs first in every round. It finds changed lines no test runs
  (from the coverage report) and mutants of the changed code every test
  passes. A gap that blocks ends the round before the brief or any
  reviewer, so reviewers attack only code whose tests are enough. Set
  `tests.coverage` to a command that writes an lcov report. Most test tools
  have a flag for that.
- **`test-writing`** works between rounds of a review to convergence. After
  the steelman's fixes, a fresh writer pins each fixed blocker with a test.
  Each pin must fail on the code from before the fixes and pass after.
- **`tests.setup`** runs once a round, in one copy of the change, before
  any reviewer's copy is made from it. Every copy that round, reviewers'
  and mutants', is then cloned from it with its build caches, so none
  starts cold. Put the package install and a first build or test run in
  it.
- Size `tests.parallel` to the machine: test runs are CPU- and
  memory-bound, so on a small machine, run one at a time.

- **`brief`**: before each round, the Orchestrator writes each lens a brief
  for this change. It covers where to attack and how, as questions to
  settle, not conclusions.
- **`context`**: every role gets what git says about the changed files.
- **`claims`**, **`mutation`** and **`probes`** are lenses the framework
  adds. Mutation runs `tests.command` itself, in throwaway copies, once
  clean and then once per mutant, so the command must work in a fresh copy
  with no one at the keyboard. `{tests}` stands for the test files a mutant
  names. Put cheap setup in front of it, such as the offline dependency
  install and the cache copy from the runbook.

## 5a. The builder's brief

The session that built a change briefs its reviewers best: it knows what
the change does and where it could be wrong. Pass its brief with
`tailbus acr run --brief <file>`:

```yaml
scope: |
  What the change does, mechanism by mechanism: what it fixes, how,
  what it keeps, what its tests now pin, and what it doesn't touch.
lenses:
  correctness: |
    The parts to attack in this lens, and how: what to run, what to
    break, which edges and callers to check.
  regression-safety: |
    ...
```

Every role gets the scope statement, as the builder's claims to check.
Each reviewer gets its own lens's part. The `brief` stage covers any lens
the builder left out. After a fix, the steelman briefs the next round,
since by then it holds the change's context.

## 6. Check it

```sh
tailbus acr validate ~/.config/tailbus/acr/<name>.yaml
tailbus acr plan --base <ref>     # tier, lenses, bindings, cost; no model calls
```

`tailbus acr check <key>` checks that each kind of session starts. It also
checks that each one reads the copy, is refused a file outside it, and, for
a reviewer with a shell, works in a copy of its own.

## 7. Calibrate it on real work

A config is right when the pipeline finds what your most careful review
finds. To measure that:

1. Take a real change, new work nobody has reviewed. Freeze a key with its
   repository, base, head, a paragraph of what it's for, and the dimensions.
2. Run `tailbus acr trial <key> --arm separated` before any other review, so
   nothing can leak.
3. Review the change by your most careful method to convergence, and write
   its findings into the key, each marked as a blocker or not, and found in
   round 1 or later.
4. Run `tailbus acr rescore <key> --results <file>`.
5. Read what the pipeline missed, and why. A trial keeps every reviewer's
   transcript in `<state>/bridge/<agent>/log/<run>/transcripts`. Most misses
   trace to a missing technique in a lens's `attack`, or to a runbook gap
   that kept a reviewer from running something. Fix those, and run the next
   change.

## What not to put in the config

Don't put severity tables or objectives written per repository or project.
A change's objective comes from its own description, comments and commit
messages. The config says what's wrong in each lens, how to find it, and how
to work here.
