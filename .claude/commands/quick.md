---
description: Small change as one conventional PR from its own worktree; squash after checks.
disable-model-invocation: true
argument-hint: "<change> | #<issue>"
---

# /quick

One small change, one PR. Don't implement in this chat; launch a builder subagent (or do it
yourself only when the change is tiny and local).

Task: `$ARGUMENTS`

Use `gh -R chelabase/chelabase`. Follow root `AGENTS.md` and `pr-body`.

## Refuse (recommend `/epic`) when

The change spans several independently verifiable steps, adds new platform surface, or needs
a white-room audit to trust (auth, crypto, permissions, a migration, a breaking contract
change, risky fork C). Say which part triggered the refusal.

## Workflow

1. Title: `type(area): summary`. From an issue, take type from its `type:*` label and the
   summary from its title. Branch: `<type>/<area>-<short>`.
2. Worktree: `just wt-add <branch>` from main. Never branch in the main checkout.
   From an issue, claim it first: `gh issue edit N --add-assignee @me`, label
   `status:building` (drop `status:ready`/`status:triage`).
3. Restate the goal and what you're deliberately not changing. Respect Out of scope.
4. Implement test-first per `AGENTS.md` Tests (skip with a stated reason for pure plumbing);
   keep one failing run per new test from before the fix for the report.
5. Run the affected tests (not the whole gate unless cheap). Commit with explicit paths.
6. Push the branch; open a PR with a flat Test plan (`pr-body`). `Closes #N` when
   it closes an issue.
7. After CI green (or local checks when CI isn't available), the PR is squash-merged when
   the human says so. Leave it open and report.
8. `just wt-remove <branch>` when done. Report PR URL, the failing runs, tests run/skipped,
   follow-ups.

## Rules

- Never push to `main` or merge with failing checks.
- Never create epic machinery (milestone issues, epic PR). That needs `/epic`.
- Never pass harness `isolation: "worktree"`.
