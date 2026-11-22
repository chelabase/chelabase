---
description: Build ready epic milestones in worktrees; send whiteroom ones to /audit; finish with epic review and gate.
disable-model-invocation: true
argument-hint: "<slug> [n]"
---

# /build

Build milestones for epic `<slug>`. Don't implement in this chat; launch builder subagents.

Task: `$ARGUMENTS`

Use `gh -R chelabase/chelabase`. Follow root `AGENTS.md`, `issue-spec` and `pr-body`.

## Local epics

If `.claude/.cache/epics/<slug>/state.json` (in the main checkout) has `"local": true`, the
epic came from `/epic-security` and nothing about it may reach GitHub before the advisory:

- Milestones are `<cache>/m<n>.md`. Status, claims and the checklist live in `state.json`.
  Don't call `gh issue`, `gh pr` or `gh label`.
- Builders and fix rounds **never push**. Brief each builder with the absolute cache path
  and this rule. Merge milestone branches into `epic/<slug>` locally. Slots are local
  worktrees and work the same.
- Commit messages are fix-only (see the writing rule in `/epic-security`).
- The whole-epic review goes to `<cache>/review.md` and the gate output to `<cache>/gate.md`.
  Finish step 3 becomes "write the Test plan and Review record into `review.md`". The next
  step is `/epic-security PUBLISH <slug>`, not a ready PR.

Everything else below applies as written.

## Setup

1. Find the epic: `gh issue list --label "epic:<slug>" --state open`, and the draft PR for
   `epic/<slug>`. Read each open milestone's body and `epic-meta`.
2. Ready = `status:ready` (or landed with `status:changes-requested` for a fix), dependencies
   integrated, and (for whiteroom deps) audited. Optionally build only milestone `n`.
3. Ensure the epic worktree exists (`just wt-add epic/<slug>` if needed). Bring its stack up
   (`just workbench up` in the epic worktree) only when a milestone's Verification has live
   tests; slots reuse it via `.workbench-stack`. Builders and the auditor never run
   `workbench up`/`down`. `just wt-remove epic-<slug>` purges it after the human merges.
4. Builders and auditors work in **slots**: reused worktrees `.worktrees/slot-<k>`, one pool
   shared by every epic and `/quick` change, whose `target/` stays built, so only changed
   crates recompile. `just wt-claim <ref> [branch]` (from the main checkout) claims a free
   one atomically, taking the one closest to what it will hold and making a new one only
   when all are claimed; `just wt-slots` lists them. The pool grows to as many agents as
   run at once; how many that is stays your call, not the tooling's. A claim whose agent
   is gone is yours to clear: `just wt-slots` shows it and says how.

## Build a milestone

For each ready milestone whose `touches` don't overlap others already building:

1. Launch one subagent on the issue's `model`. Brief: issue number and body, epic branch,
   `touches`, acceptance criteria, tests to write, out of scope, verification commands.
   The builder's first step is `just wt-claim epic/<slug> epic-<slug>-m<n>` from the
   main checkout (the last line of output is its slot; it records the epic stack) and it
   works only there, one cargo command at a time. A fix round claims the same way: the
   existing branch is checked out, and fast-forwarded to the epic tip when it has nothing
   of its own. Never `isolation: "worktree"`. Never spawn further agents.
   On launch, claim the issue: `gh issue edit N --add-assignee @me`, label
   `status:building` (drop `status:ready`).
2. Builder: test-first per `AGENTS.md` Tests; commit with explicit paths; run only the
   issue's Verification and its new/changed live tests; push the milestone branch; then
   `just wt-release <slot>` (it refuses until everything is committed); report commit SHAs, one failing run per new test from before the fix, tests run/skipped,
   deviations. The sketch is the ideal outcome, not a script: a builder may pick a better
   design and reports each deviation with its reason (`AGENTS.md`, Judgment over the
   letter). A builder calls the advisor only when the same error comes back after a fix
   attempt (`AGENTS.md`, Advisor). **Scope stop:** if the work needs code the design sketch doesn't name
   (re-implementing what a dependency did, a new validation layer, a diff well past the
   sketch's size), the builder stops and reports before writing it. Check each builder's
   `git diff --stat` mid-build; a diff far beyond the sketch gets the same stop.
3. On green: merge `--no-ff` into `epic/<slug>`, set issue label `status:landed`. Update the
   PR checklist. The slot stays for the next claim; never remove one here. When the host
   has a spare cargo seat, `just wt-warm epic/<slug> -p <crate>…` builds the free slots at
   the new tip so the next claim starts compiled.
4. If `review: whiteroom`, launch the **auditor** agent yourself (same prompt and model rule
   as `/audit`) before any later milestone that depends on it builds. On
   `changes-requested`, fix the findings in place: a fix round on that milestone, then its
   dependents build.

## Finish the epic

When every milestone is integrated (and whiteroom ones audited):

0. If the `advisor` tool is available, call it before the gate, and whenever a milestone
   needs a second fix round (`AGENTS.md`, Advisor).
1. Whole-epic review on the epic branch (one pass). Fix its blocking and should-fix
   findings in place before the gate; fix nits when cheap, otherwise file them.
2. One `just check` and one live pass for suites the epic touched.
3. Fill the PR Review record and Test plan; mark the PR ready for review.
4. Report PR URL, what ran, what's left for the human (merge). After the human merges, in a
   later step than the merge: `just wt-remove epic-<slug>`. The slots stay; they belong to
   the shared pool, not the epic.

Never publish images from the epic branch: merges are squash-only, so its commits leave no
history. When the epic needs new images, test with locally built ones, and after the human
merges, publish and pin from `main` in a follow-up PR (CONTRIBUTING "Releasing images").

## Rules

- Fix findings where they're found; never defer them to a later combined round.
- No second review agent per milestone unless the fix changed design.
- Don't rerun a suite on a commit where it already passed.
- Don't merge to `main`; the human does that.
