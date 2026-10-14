# workbench-stack: `test`/`run --stack`, and the Studio fixes (implementation plan)

**Goal:** `just workbench test`/`run` from a task worktree run on the plan's one stack (recorded by `just wt-add`, or named with `--stack`) without starting or rebuilding it, and refuse when no stack is recorded; the flaky Studio test `accept-invite-form` is fixed at its cause; Studio on branch stacks reacts to input again (root-caused, fixed, with a regression check); then the protected-branches browser pass.

**Spec:** `docs/superpowers/specs/2026-10-14-workbench-stack-design.md` (approved 2026-10-14). Implementers read the spec section their task names.

**Architecture:** `tools/cli/src/workbench/`: a new `target.rs` (pure: which stack, readiness, archive path, the base's slug), `commands.rs` (`test`/`run_script` take the target; `in_runner` builds its command through a new `runner_command`), `main.rs` (`--stack` on `test`/`run`, new `slug-of`); `justfile` `wt-add` writes `.workbench-stack`; `.gitignore`; CLAUDE.md. Studio: `apps/studio/src/components/login/` (the flake) and wherever T3's diagnosis points.

**Tech:** Rust (existing crates only, no new dependency), bash/just, React/vitest. No fork change: no image build; the live pass runs on the pinned images.

**Standing rule:** the coordinator writes and commits each task's RED tests (below) before the brief, runs them RED and records the result in the Status. Implementers make the tests pass without editing, weakening, skipping or deleting them. Reviewers check that `git diff <red-commit>..HEAD -- <test files>` is empty. When a RED commit needs a module to exist so that its tests compile, the coordinator adds only the empty module declaration.

## Status

- **RESUME HERE (2026-10-14): done on its branch; paused for the user's merge decision.** Every task, the end-of-step review and its fixes, `just check` (green on `e16ffb3e`) and the live pass are done; the live pass found L1 (fixed, `b3df4bc3`). The plan's stack stays up in `.worktrees/workbench-stack` until the merge, then is purged. On merge: `git merge --no-ff`, push `main`, `just wt-remove workbench-stack`, delete the branch on origin and locally; then reconnect Playwright MCP so the project `.mcp.json` (isolated) takes effect.
- **Live pass (2026-10-14, plan's stack on the merged binaries):** `just wt-add workbench-stack-live workbench-stack` recorded `workbench-stack`; from it `just workbench test -p chelabase-nats -- --run-ignored only` 2/2 (archive `test/workbench-stack-live.tar.zst`, run in the plan's runner, no `up`, no new stack in `status`); `just sql-editor-e2e` first failed (**L1**: the stack env's `CHELABASE_CLI` points into the stack's checkout, which a `--stack` run doesn't mount), fixed (RED `1dee4c9b`, fix `a3cf3fa6`: `-e CHELABASE_CLI=<task checkout>/target/debug/chelabase`; chelabase 199/199, clippy clean; verified by the coordinator; merged `b3df4bc3`), then all passed (95 s); `--stack nope` → "no stack \"nope\" is registered; known stacks: workbench-stack"; `up` there refuses (record names another stack). `scripts/studio-create-db-check.mjs`: isolated OK (POST aborted), `persistent` FAIL (input dropped), as diagnosed. R2 live: `/__tsd/console-pipe/sse` through the gateway held 75 s (curl's own limit). Browser pass (Playwright MCP, persistent profile + the `about:blank` workaround): sign-in, create database `browserpass` through the form, Branches → Protect (explanation, "Guard login roles" on, "Nothing to review", Confirm protect), Protection panel (switch off → metadata `guard_logins=f`, on again; Acknowledge → hash stored), Roles → create `deploy` → "Exempt deploy from the login guard" on (row in `branch_guard_exemptions`) and off (row gone), Unprotect (`protected=f`). Console: only a `favicon.ico` 404 (pre-existing). Not seen in a browser: "Review unavailable" (needs a missing compute; covered by vitest). L1 is a code change after the gate: ruling (coordinator) it touches only `runner_command` (unit-tested, clippy clean) and nothing `just check` exercises beyond `cargo check`; the gate isn't rerun. Cost if wrong: a compile error the next gate finds.
- **Done: final fix round** (sonnet): RED `a6c60fb4`, fix `afcd8793` (verified by the coordinator: RED files untouched; `runner_command` always passes `CHELABASE_STACK_CHECKOUT`; `roles_live`'s `compose()` interpolates with it; `followed_stack()` backs `env`/`psql`/`logs`/`exec`; `up` calls `check_up_allowed`; `wt-add` skips `origin/main` and survives a failing `slug-of`; `stack_for_run` helper; `get` removed); chelabase 198/198, recipes ok, `roles_live` compiles; merged `e16ffb3e`.
- **For the user at the pause:** `cargo clippy … -D warnings` also fails on two pre-existing lints in `crates/chelabase-metadata/src/members.rs` (:1362 complex type, :2068 too many arguments), and `just check` runs neither clippy nor the workspace's Rust unit tests (`cargo check --workspace` plus a few named tests). Second case of the same kind as M5, so the invariant is "the workspace is clippy-clean and its unit tests pass in the gate"; adding that to `just check` is a gate-scope decision.
- **End-of-step review (opus):** I1 (Important) a suite that recreates apps through Compose (`roles_live`'s `compose()`) inherits the runner's `CHELABASE_CHECKOUT=<task checkout>`, so a `--stack` run would recreate the plan stack's orchestrator on the task checkout; m1 `env`/`psql`/`logs`/`exec` ignore `.workbench-stack` and their error hints suggest `workbench up` (a second stack); m2 `wt-add <x> origin/main` records an empty file; m3 the preamble still says a `workbench test` starts a down stack; m4 the check's `persistent` mode leaves its temp profile, and its demo-data assumption is undocumented. Checked and fine: no path from `--stack` starts/builds/recreates the stack in the CLI itself, parallel task worktrees don't collide, a gone checkout or purged stack fails cleanly, R2's SSE limit adds nothing an `Upgrade` header didn't already allow (site routes include Auth and Mail too), the layout effect runs on the same commits as before.
- **Final fix round rulings (coordinator):** I1 the runner always exports `CHELABASE_STACK_CHECKOUT` (the stack's own checkout) and app-recreating harnesses interpolate with it; m1 `env`/`psql`/`logs`/`exec` follow the record (never starting anything) and `up` refuses where the record names another stack (`check_up_allowed`); m2 `origin/main` counts as `main`, `slug-of` strips `origin/`. Cost if wrong: small (CLI ergonomics). RED `a6c60fb4`: `chelabase` tests don't compile (no `check_up_allowed`); `workbench-recipes.sh` fails at "roles_live.rs recreates apps without CHELABASE_STACK_CHECKOUT". Live proof so far: `just wt-add workbench-stack-fix workbench-stack` from the step branch recorded `workbench-stack`.
- **Done: T3b** (opus/none): RED `3fc46782`; implementation `15656825` (gateway: `site_read_timeout`, a site request whose `Accept` lists `text/event-stream` gets the 1 h limit; 3 unit tests; the workbench-entry spec's Limits sentence updated) and `06bdf48a` (`.mcp.json` with `@playwright/mcp@0.0.83 --headless --browser chromium --isolated`; preamble "Browser passes" note; `scripts/studio-create-db-check.mjs`, Playwright from `apps/landing`); gateway 133/133; the check printed OK against the plan's stack (POST aborted). Verified by the coordinator: RED test untouched (only the sibling's comment changed). Merged `53ab21ea`.
- **Done: T3a** (opus/none): reproduced every time. Root cause: not Studio, the gateway or Vite. Playwright MCP runs headless Chrome with a persistent profile; a sign-in through Studio's login form (`autocomplete` username/current-password) opens Chrome's "Save password?" prompt, invisible headless, and it swallows all trusted input to the tab until a cross-site navigation (survives reloads and `restart studio`). A/B: headless shell ok, full Chromium non-persistent ok, persistent + form sign-in dead, persistent + fetch sign-in ok. Users in headed Chrome see and dismiss the bubble. Separate, harmless: the gateway cuts TanStack devtools' `/__tsd/console-pipe/sse` every 60 s (site routes only exempt websocket upgrades from the upstream read timeout). Scripted check: `studio-create-db-check.mjs` (isolated context: OK, create request sent and aborted; `persistent`: FAIL, input dropped). Demo admin: Zitadel's first-instance human `zitadel-admin@zitadel.auth.<slug>.chelabase.localhost` / `Password1!`.
- **R1 (user, 2026-10-14):** browser passes use a project `.mcp.json` running `@playwright/mcp` (0.0.83) `--headless --isolated` (in-memory profile per session), plus a preamble note and the scripted check committed under `scripts/`. Cost if wrong: browser sign-ins don't persist across MCP sessions.
- **R2 (user, 2026-10-14):** on site routes, a request that accepts `text/event-stream` gets the websocket idle limit (1 h) as its upstream read timeout. Cost if wrong: one more long-lived connection class, bounded at 1 h.
- **T3b scope (from R1, R2; replaces "T3a's proposed fix"):** the gateway change (R2) and its spec comment; project `.mcp.json` (R1); `docs/agent-preamble.md` browser-pass note (isolated profile; if input dies after a form sign-in, it's Chrome's hidden save-password prompt: navigate to `about:blank` and back); the scripted check as `scripts/studio-create-db-check.mjs` resolving Playwright from the workspace (no hard-coded `.pnpm` path), run in the live pass. RED `3fc46782`: `a_site_event_stream_may_idle_past_the_upstream_timeout` fails (the body read errors: the stream is cut after the 1 s upstream timeout); its sibling `a_hung_site_request_times_out_like_the_api` passes.
- **Done: T1** (sonnet/sonnet): RED `89379969`, implementation `e390f785`; nextest -p chelabase 194/194, clippy clean, `workbench-recipes.sh` ok; review: spec ✅, quality approved, 4 Minor (deferred, below); merged `8e605fa0`. Coordinator test bug (listed): the RED commit added `..` to two older `Test`/`Run` patterns in `main.rs` but missed four (`workbench_test_splits_selection_and_run_args` ×2, `workbench_run_takes_env_then_the_script_and_its_args`, `workbench_run_passes_the_scripts_flags_through`); the implementer added `..` there only (pattern-only, accepted). The manual `wt-add` probe can't record a registered slug until `slug-of` is on the base branch (now merged); the live pass covers it.
- **Done: T2** (sonnet/none): diagnosis: the "ending" heading's focus came from a passive `useEffect`, a scheduler task after the commit; `findByRole` resolves in a microtask after the commit and RTL's `setTimeout(0)` can beat React's `setImmediate` under load, so `toHaveFocus` saw `<body>` (probe: 10/150 failed under 8 CPU burners). RED `81a1039b` (MutationObserver records `activeElement` at insertion: fails deterministically, 1 failed / 12 passed), fix `827e5750` (`useLayoutEffect` for that one effect; verified by the coordinator: one file, 3+/2−); probe 150/150 under load, Studio 1131/1131, tsc clean; merged `214b3653`.
- **Deferred to the final fix round (T1 review Minors):** M1 `wt-add`'s header comment still says it "runs nothing" (it now builds and runs the CLI in the new worktree); M2 a failing `cargo run … slug-of` aborts `wt-add` half-done (after the worktree, before `.workbench-stack` and `pnpm install`): fall back to an empty slug with the warning; M3 a non-`main` base with no registered stack records an empty file, so `test`/`run` there refuse even if the worktree later gets its own stack (spec D2: no code change; the refusal message should also name `.workbench-stack` so the user knows what to edit or delete); M4 the `resolve_stack_target … ensure_current` block is duplicated in `test` and `run_script`: one helper. **M5 (T3b, found by its implementer):** `cargo clippy -p chelabase-routes --all-targets -- -D warnings` fails: `const fn get` (`crates/chelabase-routes/src/lib.rs:931`, since `ff9635d1`) has no callers; the gate doesn't run clippy with `-D warnings`, so `main` stayed green; remove it.
- **T1 RED `89379969`:** `cargo nextest run -p chelabase --no-run` fails to compile, 7 errors, all missing interfaces (`Test`/`Run` have no `stack`, no `SlugOf`, no `runner_command`, empty `target`, `Stack::new` private); `workbench-recipes.sh` fails at "wt-add asks the CLI for the base's stack".
- Branch `workbench-stack`, spec `f89c870f`, plan `590e1f44`.

## Plan rulings (mechanical, listed for the user)

- **P1** T2 (the flake) and T3 (the Studio input bug) can't have RED tests written before their cause is known. Each runs as diagnosis first, then the coordinator writes the failing test from the diagnosis (as in a fix round), then the fix. T2 does both halves in one task with a stop in between: the implementer reports the cause and stops; the coordinator commits the RED test; the same agent (SendMessage) fixes. T3 is split: T3a (diagnosis, report only), T3b (fix). Cost if wrong: one extra brief.
- **P2** The protected-branches browser pass runs in "End of step" 5 on the merged branch (it's a live pass, not a task). Cost if wrong: none.
- **P3** `main` counts as green: its last gate ran green on the protected-branches merge (`2e963d9f`); the only commits since are docs. The one known flaky test is T2. Cost if wrong: a red gate at the end, fixed in this step.
- **P4** `--stack` goes before the archive selection (`workbench test --stack feat -p rest -- …`) and before the script (`workbench run --stack feat s.sh`); after the script, every argument is the script's (as `--env` today). Cost if wrong: none (the parse tests pin it).

## Task table (binding)

| Task | Scope | Implementer | Reviewer | Wave | Depends |
|---|---|---|---|---|---|
| T1 | CLI `--stack`, `.workbench-stack`, `slug-of`, `wt-add`, `.gitignore`, CLAUDE.md | sonnet | sonnet | 1 | — |
| T2 | flaky `accept-invite-form` test: cause, then fix | sonnet | none (leaf; end-of-step review) | 1 | — |
| T3a | Studio on a branch stack not reacting to input: root cause, live, report only | opus | none (diagnosis; T3b's RED test checks it) | 1 | — |
| T3b | T3a's fix, against the coordinator's RED test | opus | none (leaf; end-of-step review) | 2 | T3a |

Waves run in parallel when their files are disjoint (pre-flight scan), each task in its own worktree (`just wt-add workbench-stack-tN workbench-stack`), with at most 3 builds or test suites at once. One stack serves the whole plan (up when the plan is written, purged after the final merge). T1 runs no live suite; T3a drives the stack's Studio in a browser and may `just workbench restart studio` (no other task uses the stack in wave 1).

## Shared interfaces (pinned)

`tools/cli/src/workbench/target.rs` (new, `pub mod target;` in `mod.rs`):

```rust
/// The file `just wt-add` writes in a task worktree: the plan's stack slug, or empty.
pub const STACK_FILE: &str = ".workbench-stack";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// The current checkout's stack (today's behaviour: auto-start, rebuild).
    Own,
    /// Another checkout's stack: never started, rebuilt or recreated.
    Other(String),
}

/// `--stack` (trimmed) first, else the recorded file's contents (trimmed; `None` = no file), else
/// `Own`. A slug equal to `own_slug` is `Own`. An empty flag is an error; an empty record (no
/// flag) is an error whose message names `--stack` and `just workbench up`.
pub fn resolve_target(flag: Option<&str>, recorded: Option<&str>, own_slug: &str) -> anyhow::Result<Target>;

/// `<checkout>/.workbench-stack`'s raw contents; `None` when the file doesn't exist.
pub fn read_recorded(checkout: &Path) -> anyhow::Result<Option<String>>;

/// Another stack is usable when registered, holding its slot (`running_since`) and with every
/// service of `expected` running. Errors: `no stack "<slug>" is registered…`; `stack "<slug>"
/// isn't running: run `just workbench up` in <entry.checkout>`; partly up: names the missing
/// services and the same `just workbench up` hint.
pub fn check_ready(slug: &str, entry: Option<&Entry>, expected: &[&str], running: &[String]) -> anyhow::Result<()>;

/// `<state>/test/archive.tar.zst` for the stack's own checkout, else
/// `<state>/test/<checkout_slug>.tar.zst`.
pub fn archive_path(stack_state: &Path, stack_slug: &str, checkout_slug: &str) -> PathBuf;

/// The slug `wt-add` records for a base branch: `main` → `main`; else `slug_for_dir` of the
/// branch with `/` → `-`; returned only if that slug is registered (running or not), else "".
pub fn base_stack_slug(base_branch: &str, registry: &Registry) -> anyhow::Result<String>;
```

`tools/cli/src/workbench/commands.rs`:

```rust
impl Stack { pub(crate) fn new(slug: &str, checkout: &Path, main: &Path) -> Self; } // was private

/// The runner's `docker compose … --profile runner run --rm --no-deps [-e K=V]… runner <cmd…>`
/// for `stack`, run on `checkout`'s files: when `checkout` isn't `stack.checkout`, the process env
/// gets `CHELABASE_CHECKOUT=<checkout>` (set after `compose_command`'s env-file values, so it wins
/// the interpolation) and `-e CHELABASE_CHECKOUT=<checkout>` comes before `env`'s pairs.
pub(crate) fn runner_command(stack: &Stack, checkout: &Path, env: &[(String, String)], cmd: &[String]) -> anyhow::Result<Command>;

pub fn test(stack: Option<&str>, selection: &[String], run_args: &[String]) -> anyhow::Result<()>;
pub fn run_script(stack: Option<&str>, env: &[String], script: &str, args: &[String]) -> anyhow::Result<()>;
/// `workbench slug-of <branch>`: prints `base_stack_slug(branch, registry)` (possibly empty) and a newline.
pub fn slug_of(branch: &str) -> anyhow::Result<()>;
```

`tools/cli/src/main.rs`: `WorkbenchCommands::Test { stack: Option<String>, archive, run }`, `Run { stack: Option<String>, env, script, args }`, new `SlugOf { branch: String }` (subcommand `slug-of`); `WorkbenchCli` gets `#[arg(long)] stack: Option<String>` on both.

## Pre-flight scan (2026-10-14)

- Wave 1 files: T1 `tools/cli/**`, `justfile`, `.gitignore`, `CLAUDE.md`, `docs/agent-preamble.md` (only if it mentions the gap), `scripts/tests/workbench-recipes.sh` (coordinator); T2 `apps/studio/src/components/login/**`; T3a none (report only). Disjoint.
- `compose_command` (`commands.rs:310`) puts every env-file pair into the process env so the caller's shell can't override them; the override must be set on the `Command` after it. `in_runner` (`commands.rs:1494`) already builds `-e` pairs; `chosen_stack` (`commands.rs:208`) already resolves a registered slug to a `Stack` (and falls back to the current compose file when the checkout is gone).
- `ensure_current` (`commands.rs:1458`) is today's own-stack path; `missing_services(&APPS, …)` is the partly-up check to reuse in `check_ready`.
- The runner mounts `${CHELABASE_CHECKOUT}` read-write and as `working_dir` (`docker-compose.yml:34,52-55`), plus the stack's state dir: the archive under the named stack's `state` is visible to it.
- Every `just <x>-e2e` recipe calls `workbench run` without a stack, so the recorded default covers them with no recipe change.
- Existing tests that destructure `Test { archive, run }` / `Run { env, script, args }` in `main.rs` (`test_args`, `exec_needs_a_command…`) get `..` in the RED commit.
- `wt-add` runs the CLI from the new worktree (`env -C "$dir" cargo run -q -p chelabase -- workbench slug-of "$base"`): the main checkout's CLI may predate `slug-of`.

---

## T1: `--stack`, the recorded stack, `wt-add` (sonnet / sonnet)

**Files:** create `tools/cli/src/workbench/target.rs`, `tools/cli/src/workbench/stack_target_tests.rs` (coordinator); modify `tools/cli/src/workbench/mod.rs`, `tools/cli/src/workbench/commands.rs`, `tools/cli/src/main.rs` (its tests: coordinator), `justfile` (`wt-add`), `.gitignore`, `scripts/tests/workbench-recipes.sh` (coordinator), `CLAUDE.md` (drop "Gap until the CLI supports it…"; say task worktrees run `just workbench test`/`run` as usual and land on the plan's stack recorded by `wt-add`, `--stack` overrides), `docs/agent-preamble.md` (same, only if it mentions the gap).

**Behaviour:** spec Parts 1 and 2; Shared interfaces. `test`/`run_script`: `current_stack()` → `resolve_target(flag, read_recorded(checkout)?, here.slug)`; `Own` → today's path (`ensure_current`, archive `state/test/archive.tar.zst`); `Other(slug)` → `chosen_stack(here, Some(slug))`, `check_ready(slug, entry, &APPS, &stack.running_services()?)`, no `ensure_current`; archive (test) with `cargo nextest archive` in `here.checkout` to `archive_path(stack.state, stack.slug, here.slug)`; nextest's `--workspace-remap` is `here.checkout`; `in_runner` takes the checkout and builds via `runner_command`. `wt-add`: when `base` isn't `main`, after the worktree exists: `slug="$(env -C "$dir" cargo run -q -p chelabase -- workbench slug-of "$base")"`, `printf '%s\n' "$slug" > "$dir/.workbench-stack"`, and when empty print `no stack for <base>: bring it up there (just workbench up) or pass --stack` to stderr (not a failure). `.gitignore`: `/.workbench-stack`.

**RED tests (coordinator):**

`tools/cli/src/workbench/mod.rs` gets `pub mod target;` (empty file) and `#[cfg(test)] mod stack_target_tests;`.

`tools/cli/src/workbench/stack_target_tests.rs`:

```rust
//! workbench-stack T1: `test`/`run` on a named stack (spec Parts 1 and 2).

use super::commands::{Stack, runner_command};
use super::registry::{Entry, Registry};
use super::target::{
    STACK_FILE, Target, archive_path, base_stack_slug, check_ready, read_recorded, resolve_target,
};
use std::path::{Path, PathBuf};
use std::process::Command;

fn entry(slug: &str, running: bool) -> Entry {
    Entry {
        slug: slug.into(),
        checkout: PathBuf::from(format!("/m/.worktrees/{slug}")),
        gateway_port: 8101,
        proxy_port: 4501,
        created_at: 1,
        running_since: running.then_some(2),
    }
}

fn other(s: &str) -> Target {
    Target::Other(s.to_string())
}

#[test]
fn the_flag_wins_then_the_recorded_file_then_the_own_stack() {
    assert_eq!(resolve_target(Some("feat"), Some("other"), "feat-t1").unwrap(), other("feat"));
    assert_eq!(resolve_target(Some(" feat "), None, "feat-t1").unwrap(), other("feat"));
    assert_eq!(
        resolve_target(Some("feat-t1"), Some("feat"), "feat-t1").unwrap(),
        Target::Own,
        "naming one's own stack is the own stack"
    );
    assert_eq!(resolve_target(None, Some("feat"), "feat-t1").unwrap(), other("feat"));
    assert_eq!(resolve_target(None, Some("feat\n"), "feat-t1").unwrap(), other("feat"));
    assert_eq!(resolve_target(None, None, "feat-t1").unwrap(), Target::Own);
    assert_eq!(
        resolve_target(Some("feat"), Some(""), "feat-t1").unwrap(),
        other("feat"),
        "--stack overrides an empty record"
    );
}

#[test]
fn an_empty_record_or_flag_refuses_with_a_hint() {
    let err = resolve_target(None, Some(""), "feat-t1").unwrap_err().to_string();
    assert!(err.contains("--stack") && err.contains("just workbench up"), "{err}");
    let err = resolve_target(None, Some("  \n"), "feat-t1").unwrap_err().to_string();
    assert!(err.contains("--stack"), "{err}");
    assert!(resolve_target(Some(""), None, "feat-t1").is_err());
    assert!(resolve_target(Some("  "), Some("feat"), "feat-t1").is_err());
}

#[test]
fn the_record_is_read_from_the_checkout() {
    assert_eq!(STACK_FILE, ".workbench-stack");
    let tmp = tempfile::tempdir().unwrap();
    assert_eq!(read_recorded(tmp.path()).unwrap(), None);
    std::fs::write(tmp.path().join(STACK_FILE), "feat\n").unwrap();
    assert_eq!(read_recorded(tmp.path()).unwrap().as_deref().map(str::trim), Some("feat"));
    std::fs::write(tmp.path().join(STACK_FILE), "").unwrap();
    assert_eq!(read_recorded(tmp.path()).unwrap().as_deref(), Some(""));
}

#[test]
fn another_stack_must_be_registered_running_and_fully_up() {
    let apps = ["orchestrator", "studio"];
    let up = vec!["orchestrator".to_string(), "studio".to_string(), "proxy".to_string()];
    let err = check_ready("feat", None, &apps, &up).unwrap_err().to_string();
    assert!(err.contains("no stack \"feat\" is registered"), "{err}");
    let err = check_ready("feat", Some(&entry("feat", false)), &apps, &up).unwrap_err().to_string();
    assert!(
        err.contains("isn't running") && err.contains("just workbench up") && err.contains("/m/.worktrees/feat"),
        "{err}"
    );
    let err = check_ready("feat", Some(&entry("feat", true)), &apps, &up[..1]).unwrap_err().to_string();
    assert!(err.contains("studio") && err.contains("just workbench up"), "{err}");
    check_ready("feat", Some(&entry("feat", true)), &apps, &up).unwrap();
}

#[test]
fn each_checkout_archives_to_its_own_file() {
    let state = Path::new("/m/.chelabase/stacks/feat");
    assert_eq!(archive_path(state, "feat", "feat"), state.join("test/archive.tar.zst"));
    assert_eq!(archive_path(state, "feat", "feat-t1"), state.join("test/feat-t1.tar.zst"));
    assert_ne!(archive_path(state, "feat", "feat-t1"), archive_path(state, "feat", "feat-t2"));
}

#[test]
fn a_task_worktree_records_its_bases_registered_stack() {
    let mut reg = Registry::default();
    reg.stacks.push(entry("feat-x", false));
    assert_eq!(base_stack_slug("feat/x", &reg).unwrap(), "feat-x", "registered, running or not");
    assert_eq!(base_stack_slug("feat-x", &reg).unwrap(), "feat-x");
    assert_eq!(base_stack_slug("other", &reg).unwrap(), "", "no stack: empty");
    assert_eq!(base_stack_slug("main", &reg).unwrap(), "", "main unregistered: empty");
    reg.stacks.push(entry("main", true));
    assert_eq!(base_stack_slug("main", &reg).unwrap(), "main", "main's own stack, not main-wt");
}

fn stack_with_env(tmp: &Path) -> Stack {
    let main = tmp.join("m");
    let stack = Stack::new("feat", &main.join(".worktrees/feat"), &main);
    std::fs::create_dir_all(stack.env_file.parent().unwrap()).unwrap();
    std::fs::write(
        &stack.env_file,
        format!("CHELABASE_CHECKOUT={}\nCHELABASE_PUBLIC_SCHEME=http\n", stack.checkout.display()),
    )
    .unwrap();
    stack
}

fn env_of(cmd: &Command, key: &str) -> Option<String> {
    cmd.get_envs()
        .find(|(k, _)| *k == key)
        .and_then(|(_, v)| v.map(|v| v.to_string_lossy().into_owned()))
}

fn args_of(cmd: &Command) -> Vec<String> {
    cmd.get_args().map(|a| a.to_string_lossy().into_owned()).collect()
}

fn has_pair(args: &[String], flag: &str, value: &str) -> bool {
    args.windows(2).any(|w| w[0] == flag && w[1] == value)
}

#[test]
fn another_checkouts_run_mounts_that_checkout_in_the_stacks_runner() {
    let tmp = tempfile::tempdir().unwrap();
    let stack = stack_with_env(tmp.path());
    let task = tmp.path().join("m/.worktrees/feat-t1");
    let cmd = runner_command(
        &stack,
        &task,
        &[("A".to_string(), "1".to_string())],
        &["scripts/x.sh".to_string(), "--y".to_string()],
    )
    .unwrap();
    assert_eq!(
        env_of(&cmd, "CHELABASE_CHECKOUT").as_deref(),
        Some(task.to_str().unwrap()),
        "Compose interpolates the mount and working_dir from the process env"
    );
    let args = args_of(&cmd);
    assert!(has_pair(&args, "-e", &format!("CHELABASE_CHECKOUT={}", task.display())), "{args:?}");
    assert!(has_pair(&args, "-e", "A=1"), "{args:?}");
    assert!(has_pair(&args, "-p", "chelabase-feat"), "{args:?}");
    assert!(has_pair(&args, "--env-file", stack.env_file.to_str().unwrap()), "{args:?}");
    assert!(has_pair(&args, "--profile", "runner"), "{args:?}");
    let runner = args.iter().rposition(|a| a == "runner").unwrap();
    assert_eq!(&args[runner + 1..], ["scripts/x.sh", "--y"]);
    for flag in ["run", "--rm", "--no-deps"] {
        assert!(args[..runner].iter().any(|a| a == flag), "{flag}: {args:?}");
    }
}

#[test]
fn the_stacks_own_checkout_runs_as_today() {
    let tmp = tempfile::tempdir().unwrap();
    let stack = stack_with_env(tmp.path());
    let cmd = runner_command(&stack, &stack.checkout.clone(), &[], &["s.sh".to_string()]).unwrap();
    assert_eq!(
        env_of(&cmd, "CHELABASE_CHECKOUT").as_deref(),
        Some(stack.checkout.to_str().unwrap()),
        "the env file's own value"
    );
    assert!(!args_of(&cmd).iter().any(|a| a.starts_with("CHELABASE_CHECKOUT=")), "no override");
}
```

`tools/cli/src/main.rs`, in `mod workbench_cli_tests`: `test_args` matches `WorkbenchCommands::Test { archive, run, .. }`; `exec_needs_a_command…` matches `WorkbenchCommands::Run { env, script, args, .. }`. In `mod tests`, after `workbench_up_takes_sites`:

```rust
    #[test]
    fn test_and_run_take_a_stack() {
        let WorkbenchCommands::Test { stack, archive, run } =
            workbench(&["test", "--stack", "feat", "-p", "rest", "--", "--run-ignored", "only"])
        else {
            panic!()
        };
        assert_eq!(stack.as_deref(), Some("feat"));
        assert_eq!(archive, ["-p", "rest"]);
        assert_eq!(run, ["--run-ignored", "only"]);
        let WorkbenchCommands::Test { stack, .. } = workbench(&["test", "-p", "rest"]) else {
            panic!()
        };
        assert_eq!(stack, None);
        let WorkbenchCommands::Run { stack, env, script, args } =
            workbench(&["run", "--stack", "feat", "--env", "A=1", "s.sh", "--stack", "x"])
        else {
            panic!()
        };
        assert_eq!(stack.as_deref(), Some("feat"));
        assert_eq!(env, ["A=1"]);
        assert_eq!(script, "s.sh");
        assert_eq!(args, ["--stack", "x"], "after the script, everything is the script's");
    }

    #[test]
    fn slug_of_takes_a_branch() {
        let WorkbenchCommands::SlugOf { branch } = workbench(&["slug-of", "feat/x"]) else {
            panic!()
        };
        assert_eq!(branch, "feat/x");
    }
```

`scripts/tests/workbench-recipes.sh`, after the `wt-add` `.env.local` line:

```bash
just --show wt-add | grep -q 'workbench slug-of' ||
  fail "wt-add asks the CLI for the base's stack, never recomputes the slug"
just --show wt-add | grep -q '\.workbench-stack' || fail "wt-add records the base's stack in the task worktree"
grep -qx '/.workbench-stack' .gitignore || fail ".workbench-stack is git-ignored"
```

**Gotchas:** set `CHELABASE_CHECKOUT` on the `Command` after `compose_command` (it loads the env file into the process env, `commands.rs:310`). `Other` never calls `up_stack`, `cargo_build`, `recreate` or `record_binaries`. `chosen_stack` already errors on an unknown slug with the known list. `run_script`'s scripts run from the task checkout (`working_dir` follows `CHELABASE_CHECKOUT`). Only rustfmt the files you create; never `cargo fmt` the workspace.

**Verify:** `cargo nextest run -p chelabase` (in the task worktree), `cargo clippy -p chelabase --all-targets -- -D warnings`, `bash scripts/tests/workbench-recipes.sh`. Manual (implementer, no stack needed): `just wt-add workbench-stack-t1-probe workbench-stack` from the task worktree's main checkout shows the recorded slug, then `just wt-remove workbench-stack-t1-probe` and `git branch -D workbench-stack-t1-probe`.

**Live runs (the plan's one stack):** none in the task. The proof run from a task worktree is in "End of step" 5.

---

## T2: the flaky `accept-invite-form` test (sonnet / none)

**Files:** `apps/studio/src/components/login/accept-invite-form.tsx`, `accept-invite-form.test.tsx` (the coordinator's RED test only; existing assertions are not weakened).

**Behaviour:** spec Part 3. Phase 1 (report, then stop): reproduce under load (e.g. `pnpm --filter studio exec vitest run src/components/login/accept-invite-form.test.tsx` in a loop of ~50 while the whole Studio suite runs alongside), name the failing assertion and the cause (where focus is set: effect, `autoFocus`, after an async render), and propose the fix (component or test, at the cause). Phase 2 (after the coordinator's RED commit, by SendMessage): fix it; prove it with ~50 loop runs under the same load, all green, plus the whole Studio suite once.

**RED tests (coordinator):** written after phase 1 from its report (P1): a test that fails deterministically for the cause (e.g. forcing the slow ordering with fake timers or a delayed response), committed as `t2: RED test`.

**Gotchas:** no `retry`, no longer timeouts as the fix; a focus that has to wait for an async render is asserted with `await waitFor(...)` only if the product behaviour is "focus after load" (say so in the report).

**Verify:** `pnpm --filter studio exec vitest run` (whole suite once), the loop above, `pnpm --filter studio exec tsc --noEmit`.

**Live runs (the plan's one stack):** none.

---

## T3a: Studio on a branch stack not reacting to input: root cause (opus / none)

**Files:** none changed; the report goes to the coordinator (who records it in the Status).

**Behaviour:** spec Part 3. On the plan's stack (`.worktrees/workbench-stack`, its Studio URL from `just workbench status` there): reproduce (sign in, "Create database": typing never enables the button; once enabled, a click sends no request; the console showed the dev-server SSE channel dropping). Systematic debugging: browser console and network (Playwright MCP), `just workbench logs studio gateway`, Vite's HMR/SSE path through the gateway (host-based routing, `studio.<slug>.chelabase.localhost:<port>`), hydration. Compare with the `main` stack's Studio only if `main`'s stack is already running (don't start it). Report: the root cause with evidence, the proposed fix (files), and a regression check the coordinator can write as a failing test (a Studio unit test, a gateway test, or a scripted browser check run in the live pass), and whether the fix needs a product or design choice.

**Gotchas:** `just workbench restart studio` is allowed; `up`/`down` are not. No code changes, no commits.

**Live runs (the plan's one stack):** the browser session only; nothing else uses the stack in wave 1.

---

## T3b: Studio input fix (opus / none)

**Files:** from T3a's report (recorded in the Status before the brief).

**Behaviour:** T3a's proposed fix, as the Status records it (a product or design choice in it goes to the user first).

**RED tests (coordinator):** written from T3a's report (P1), committed as `t3b: RED test` in `.worktrees/workbench-stack-t3b`, run RED.

**Verify:** the RED test, the touched crate's or app's unit suite, clippy/tsc for what changed; then the regression check on the plan's stack.

**Live runs (the plan's one stack):** a browser check of "Create database" on the plan's stack after `just workbench restart` of the changed app(s) (the step worktree's binaries; if the fix is in a Rust app, the check is deferred to "End of step" 5, after the merge into the step branch).

---

## End of step

0. `main` green (P3). No red-team pass (no security or concurrency core).
1. Whole-branch review (opus) on `main..workbench-stack`; it also covers T2 and T3b.
2. The one final fix round: the end-of-step review's findings plus every task finding deferred to it (the Status lists them); the coordinator writes a failing test for each finding first.
3. `just check`, once (the step's only gate run; a flaky test seen here is fixed, not rerun).
4. No integration build: no fork changed.
5. Live pass on the plan's stack (`.worktrees/workbench-stack`, after `just workbench restart` there so it runs the merged binaries):
   - the `--stack` proof: `just wt-add workbench-stack-live workbench-stack`, check `.workbench-stack` holds `workbench-stack`; from it, `just workbench test -p chelabase-nats -- --run-ignored only` (short) and `just sql-editor-e2e`, both on the plan's stack without an `up` (no new stack in `just workbench status`); `just workbench test --stack nope …` fails listing the known stacks; with the plan's stack stopped this isn't tested (it stays up); then `just wt-remove workbench-stack-live`;
   - the Studio regression check from T3a/T3b;
   - the protected-branches browser pass (P2): on a protected branch, the protect toggle and its review panel, the guard-logins switch, exemptions add/remove, the review badge and "Review unavailable", acknowledge.
6. Update `docs/landing-highlights.md` on `main` (only if a change is landing-worthy: probably none), the plan Status and the roadmap's "Resume here", then pause for the user.
