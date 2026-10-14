<!--
Plan template. Copy to docs/superpowers/plans/<date>-<step>.md and fill in.
Rules it follows: CLAUDE.md ("How each roadmap step runs", "Checkpoints") and the
roadmap's "Working rules". A lean plan pins files, cross-task interfaces, behaviour
rules, the FULL code of every RED test, gotchas and verification commands; it does
not hold implementation code, except small tricky snippets such as SQL migrations
or subtle orderings. Delete these comments when you copy it.
-->
# <Step id>: <title> (implementation plan)

**Goal:** <one paragraph: what ships>

**Spec:** `docs/superpowers/specs/<file>` (approved <date>). Implementers read the spec section their task names.

**Architecture:** <the modules and crates, and where each piece lives>

**Tech:** <languages, libraries with exact versions, new dependencies checked against the registry>. <Fork change? If none: "No fork change: no image build; the live pass runs on the pinned images.">

**Standing rule:** the coordinator writes and commits each task's RED tests (below) before the brief, runs them RED and records the result in the Status. Implementers make the tests pass without editing, weakening, skipping or deleting them. Reviewers check that `git diff <red-commit>..HEAD -- <test files>` is empty. When a RED commit needs a module to exist so that its tests compile, the coordinator adds only the empty module declaration.

## Status

<!-- The source of truth for the step in progress. Rewrite superseded lines; never append contradictions. -->
- **RESUME HERE (<date>): <current task and its state>; next: <the next action>.** <What is running, and where the stack runs.>
- **Done: T<n>** (<implementer>/<reviewer>): RED `<sha>`, implementation `<sha>`, review: <verdict, findings by severity>; fix-round RED `<sha>`, fix `<sha>` (<verified by the coordinator>); <live result>; merged `<sha>`.
- **R<n> (<user|coordinator>, <date>):** <ruling>. Cost if wrong: <…>.
- Branch `<branch>`, spec `<sha>`, plan `<sha>`.

## Plan rulings (mechanical, listed for the user)

- **P1** <ruling>. Cost if wrong: <…>.

## Task table (binding)

<!-- Each task runs with exactly these models and does only what it lists. "none" = no review of its own (state what covers it). -->
| Task | Scope | Implementer | Reviewer | Wave | Depends |
|---|---|---|---|---|---|
| T1 | <scope> | sonnet | sonnet | 1 | — |

Waves run in parallel when their files are disjoint (pre-flight scan), each task in its own worktree (`just wt-add <branch>-tN <branch>`), with at most 3 builds or test suites at once. One stack serves the whole plan (up when the plan is written, purged after the final merge); tasks in a wave never run conflicting live suites on it at the same time (see each task's "Live runs").

## Shared interfaces (pinned)

<!-- Exact signatures, types and names that cross task boundaries. Implementers write them exactly. -->

## Pre-flight scan (<date>)

<!-- Facts checked in the code before the plan was approved: files touched per wave (disjointness), match sites a new variant breaks, existing helpers to reuse, traps. -->

---

## T1: <title> (<implementer> / <reviewer>)

**Files:** <paths; mark the test files the coordinator writes with "(coordinator)">.

**Behaviour:** <spec section, plus rules not in the spec>.

**RED tests (coordinator):** <the full code of every test, with where it goes>. Before briefing: every RED target compiles (live ones with `--no-run`), and each assumption about harnesses, helpers and system behaviour is checked in the code.

**Gotchas:** <traps>.

**Verify:** <exact commands: unit, clippy, live tests compiled with `--no-run`, and any live run named for the coordinator>.

**Live runs (the plan's one stack):** <each suite this task runs on the shared stack, when, and what it would conflict with (branch/role/database state, app restarts, suites needing the task's own binaries); a conflicting run whose result nothing later in the task needs is deferred to "End of step" 5 and named there>.

---

## End of step

0. (Before the plan, security/concurrency steps: the spec's red-team pass; at the step's start: `main` green.)
1. Whole-branch review (opus) on `main..<branch>`; it also covers the leaf tasks that had no review of their own.
2. The one final fix round: the end-of-step review's findings plus every task finding deferred to it (the Status lists them); the coordinator writes a failing test for each finding first.
3. `just check`, once (the step's only gate run; a flaky test seen here is fixed, not rerun).
4. Integration, plus the one gate and the one image build, only if a fork changed (list pending fork changes in the Status first; there must be none).
5. Live pass: <suites, e2e runs and browser pass, with exact commands>.
6. Update `docs/landing-highlights.md` on `main`, the plan Status and the roadmap's "Resume here", then pause for the user.
