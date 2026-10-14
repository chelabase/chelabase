# `workbench test`/`run --stack`, and the Studio fixes it carries (design)

Date: 2026-10-14. Branch `workbench-stack`. Approved in chat 2026-10-14 (three parts).

## Why

CLAUDE.md's "Stack lifecycle" rule: one stack per plan, in the step worktree; task worktrees never get
stacks of their own. Today `workbench test`/`run` always use (and auto-start, and rebuild) the
current checkout's stack, so a task worktree can't run its live tests on the plan's stack. The same
chore fixes a flaky Studio unit test (needed for "One gate, green base") and a Studio bug on branch
stacks that blocked the protected-branches browser pass.

## Decisions (user, 2026-10-14)

- **D1 default stack:** recorded at `wt-add` time in the task worktree; `--stack` overrides.
- **D2 no base stack:** `wt-add` records an empty value and warns; `test`/`run` there refuse with a
  hint instead of starting a stack.
- **D3 mount:** the named stack's runner mounts the task worktree by overriding `CHELABASE_CHECKOUT`
  for that one `compose run` (no compose change).
- **Ruling (coordinator, listed to the user):** a worktree whose base is `main` is a step worktree:
  nothing is recorded and it keeps its own stack, as today.

## Part 1: `test`/`run` on a named stack

- `workbench test` and `workbench run` take `--stack <slug>`. Resolution order: `--stack`; else the
  checkout's `.workbench-stack` file (Part 2); else the current checkout's stack.
- **Target is the current checkout's stack:** unchanged (`ensure_current`: auto-start, rebuild,
  recreate stale apps).
- **Target is another checkout's stack:**
  - It must be registered, its slot running (`running_since`), and every app service running
    (`missing_services(&APPS, …)` empty). Otherwise: an error naming the stack and the fix, e.g.
    ``stack "table-editor" isn't running: run `just workbench up` in .worktrees/table-editor``. Never
    `up`, never `cargo build` of the apps, never recreate.
  - An unknown slug: the error `chosen_stack` already gives (lists the known stacks).
  - `test`: `cargo nextest archive` runs in the current checkout into
    `<named stack state>/test/<current checkout slug>.tar.zst` (parallel task worktrees never share an
    archive path).
  - Both commands: `docker compose` with the named stack's project, compose file and env file;
    `--profile runner run --rm --no-deps runner`; the process environment sets
    `CHELABASE_CHECKOUT=<current checkout>` (Compose interpolation: the shell beats `--env-file`, so
    the bind mount and `working_dir` are the current checkout) and `-e CHELABASE_CHECKOUT=<current
    checkout>` (inside the container, over the stack env file's value). nextest's
    `--workspace-remap` is the current checkout. Gateway URL, hosts, proxy address and the rest of
    `in_runner`'s env come from the named stack and its registry entry.
  - The stack's apps keep running the step checkout's binaries (by design: a suite that needs the
    task's own binaries is deferred to the end-of-step live pass, per CLAUDE.md).
- **Tests (unit, `tools/cli`):** a pure helper that, given the named stack, its entry and the current
  checkout, returns the archive path, the compose args and the env pairs; asserted for the
  other-stack case (archive path, both `CHELABASE_CHECKOUT` settings, remap) and the same-stack case
  (today's values). A pure readiness check (registry entry + running services → `Ok` or the
  "isn't running" error) is tested for: unregistered, registered but not running, partly up, up.

## Part 2: `wt-add` records the stack

- `just wt-add <branch>` with base `main`: no file, as today.
- `just wt-add <branch> <base>` with any other base writes `<worktree>/.workbench-stack` (git-ignored,
  one line): the base's stack slug when that slug is registered, else an empty line and a warning
  ``no stack for <base>: bring it up there (`just workbench up`) or pass --stack``.
- The slug comes from the CLI (`workbench slug-of <branch>`: the same rules as `slug_for_dir` on the
  branch's `.worktrees/` directory name, plus whether it's registered), so the recipe and the Rust
  code never disagree.
- `test`/`run` without `--stack`: a non-empty file → that slug; an empty file → refuse with the hint
  above; no file → the current checkout's stack.
- `wt-remove`: no change (the file goes with the worktree; a task worktree registers no stack).
- Docs: CLAUDE.md's "Gap until the CLI supports it" sentence goes; the "Stack lifecycle" rule says
  task worktrees run `just workbench test`/`run` as usual and land on the plan's stack;
  `docs/agent-preamble.md` gets the same if it mentions the gap.

## Part 3: Studio fixes and the browser pass

- **Flaky `apps/studio/src/components/login/accept-invite-form.test.tsx`:** a focus assertion failed
  once in the protected-branches gate and passed alone. Find the cause (likely focus set in an effect
  or after an async render, asserted synchronously) and fix it at the cause, component or test. Proof:
  ~50 repeated runs under parallel load, all green.
- **Studio on branch stacks not reacting to input** (protected-branches browser pass: typing in
  "Create database" never enabled the button; once enabled, a click sent no request, even after
  `workbench restart studio`; the console showed the dev-server SSE channel dropping). Root cause
  first, live, on a branch stack (systematic debugging); suspects: the client not hydrating or
  re-mounting behind the gateway's host-based routing, HMR/SSE through the gateway. Then the fix, with
  a regression check that types into "Create database" and sees the request go out. A fix that needs
  a product or design choice goes to the user first.
- **Then the protected-branches browser pass** of its new panels, on the fixed stack.

## Out of scope

- Changing which binaries the plan's stack runs (no per-task app binaries).
- Any fork change; no image build.

## Done when

- `workbench test`/`run` from a task worktree run on the plan's stack without starting or rebuilding
  it, and refuse when no stack is recorded; unit tests cover it, and one live run from a task
  worktree proves it.
- The flaky test is fixed at its cause; the Studio input bug is root-caused, fixed and has a
  regression check; the protected-branches browser pass is done.
- End-of-step review fixed, `just check` green once, live pass green.
