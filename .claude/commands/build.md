---
description: Build ready epic milestones in worktrees; send whiteroom ones to /audit; finish with epic review and gate.
disable-model-invocation: true
argument-hint: "<slug> [n]"
---

# /build

Build milestones for epic `<slug>`. Don't implement in this chat; launch builder subagents.

Task: `$ARGUMENTS`

Use `gh -R chelabase/chelabase`. Follow root `AGENTS.md`, `issue-spec` and `pr-body`.

## Setup

1. Find the epic: `gh issue list --label "epic:<slug>" --state open`, and the draft PR for
   `epic/<slug>`. Read each open milestone's body and `epic-meta`.
2. Ready = `status:ready` (or landed with `status:changes-requested` for a fix), dependencies
   integrated, and (for whiteroom deps) audited. Optionally build only milestone `n`.
3. Ensure the epic worktree exists (`just wt-add epic/<slug>` if needed). Bring its stack up
   (`just workbench up` in the epic worktree) only when a milestone's Verification has live
   tests; task worktrees reuse it via `.workbench-stack`. Builders and the auditor never run
   `workbench up`/`down`. `just wt-remove epic-<slug>` purges it after the human merges.

## Build a milestone

For each ready milestone whose `touches` don't overlap others already building:

1. Launch one subagent on the issue's `model`. Brief: issue number and body, epic branch,
   `touches`, acceptance criteria, tests to write, out of scope, verification commands.
   Work in `just wt-add epic-<slug>-m<n> epic/<slug>` (records the epic stack). Never
   `isolation: "worktree"`. Never spawn further agents.
   On launch, claim the issue: `gh issue edit N --add-assignee @me`, label
   `status:building` (drop `status:ready`).
2. Builder: test-first per `AGENTS.md` Tests; commit with explicit paths; run only the
   issue's Verification and its new/changed live tests; push the milestone branch; report
   commit SHAs, one failing run per new test from before the fix, tests run/skipped,
   deviations. **Scope stop:** if the work needs code the design sketch doesn't name
   (re-implementing what a dependency did, a new validation layer, a diff well past the
   sketch's size), the builder stops and reports before writing it. Check each builder's
   `git diff --stat` mid-build; a diff far beyond the sketch gets the same stop.
3. On green: merge `--no-ff` into `epic/<slug>`, remove the task worktree, set issue label
   `status:landed`. Update the PR checklist.
4. If `review: whiteroom`, launch the **auditor** agent yourself (same prompt and model rule
   as `/audit`) before any later milestone that depends on it builds. On
   `changes-requested`, fix the findings in place: a fix round on that milestone, then its
   dependents build.

## Finish the epic

When every milestone is integrated (and whiteroom ones audited):

1. Whole-epic review on the epic branch (one pass). Fix its blocking and should-fix
   findings in place before the gate; fix nits when cheap, otherwise file them.
2. One `just check` and one live pass for suites the epic touched.
3. Fill the PR Review record and Test plan; mark the PR ready for review.
4. Report PR URL, what ran, what's left for the human (merge).

Never publish images from the epic branch: merges are squash-only, so its commits leave no
history. When the epic needs new images, test with locally built ones, and after the human
merges, publish and pin from `main` in a follow-up PR (CONTRIBUTING "Releasing images").

## Rules

- Fix findings where they're found; never defer them to a later combined round.
- No second review agent per milestone unless the fix changed design.
- Don't rerun a suite on a commit where it already passed.
- Don't merge to `main`; the human does that.
