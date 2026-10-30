# AGENTS.md

Chelabase: a self-hosted database platform on a hard fork of Neon's storage engine — Postgres
databases with copy-on-write branches, a typed data API, login roles, app-user auth and Studio.

Remote: <https://github.com/chelabase/chelabase>. Default branch: `main`. Use
`gh -R chelabase/chelabase` for every GitHub call.

## Map

| Path | What it owns |
|---|---|
| [`apps/studio/`](apps/studio/) | Studio (web console) |
| [`apps/docs/`](apps/docs/) | Documentation site |
| [`apps/landing/`](apps/landing/) | Landing page |
| [`services/`](services/) | gateway, orchestrator, worker, rest, auth, realtime |
| [`crates/`](crates/) | Shared Rust libraries |
| [`packages/`](packages/) | TypeScript packages (`client`, `management-client`, …) |
| [`openapi/`](openapi/) | Generated OpenAPI documents |
| [`infra/`](infra/) | Compose, Neon image pins, workbench |
| [`tools/cli/`](tools/cli/) | The `chelabase` binary |

History only (never read or search unless a human points at a specific file):
[`docs/roadmap/`](docs/roadmap/), [`docs/superpowers/`](docs/superpowers/). Current work lives in
GitHub issues and epic PRs.

## Tooling

- **just**: `just check`, `just test`, `just workbench up|down|status|test|run`, `just wt-add`,
  `just wt-remove`, and the `*-e2e` recipes. See `CONTRIBUTING.md`.
- **Workbench**: one isolated stack per checkout. `/build` brings it up in the epic worktree
  only when a milestone needs live tests; task worktrees reuse it via `.workbench-stack`.
  Builders and auditors never run `workbench up`/`down`.
- **Worktrees**: main checkout stays on `main`. Every branch lives in `.worktrees/<branch>`
  (`just wt-add <branch>`). Never `EnterWorktree` and never harness `isolation: "worktree"`.

## Product rules

- Self-hosted mode is what runs today. Opt-in conveniences that don't affect correctness are
  fine; correctness and safety checks are never relaxed (the metadata migrator stays strict).
- The Neon and Postgres forks (`chelabase/neon`, `chelabase/postgres`) are hard forks. Every
  line is ours. Where code came from is never a reason to leave a bug. PostgreSQL's own
  releases are the one live upstream.
- Before adding a dependency, check the registry for the latest release. Prefer the
  security-stronger option when equivalent (aws-lc-rs over ring). Respect pnpm's
  minimum-release-age check.
- `tokio-postgres` stays pinned to `chelabase/rust-postgres` until upstream has the needed
  fixes; see `CONTRIBUTING.md`.

## Decisions

Product, design, wording or scope choices you're unsure about go to the human: one question,
2–4 concrete options, recommended first and marked "(Recommended)", each option's trade-off
in its description. No mandated brainstorming ritual beyond that.

## Git and safety

- Stage explicit paths only; never `git add -A`.
- Never force-push. If a push is rejected, fetch and integrate first.
- Never run `cargo fmt` on the workspace; rustfmt only files you create.
- Never overwrite `.chelabase/secrets/secrets.key`, the gateway key pair, the cookie key or
  the compute-JWT key pair.
- Feature branches only. Merge to `main` only when the human says.

## Gotchas

- No `sleep`, `pgrep -f` or `until …; do …; done` wait loops.
- `gh pr edit` fails on GitHub's Projects (classic) deprecation (`repository.pullRequest.projectCards`).
  Use `gh issue edit N` (it works on PR numbers too) or
  `gh api -X PATCH repos/chelabase/chelabase/pulls/N -F body=@body.md`.
- Before a long filtered test run, check what the filter selects.
- One cargo command at a time per checkout (`cargo nextest`, `clippy`, `build` share
  `target/` and only queue on the build lock).
- Fork builds go through the fork's own `.github/scripts/ci-local.sh` (in the `chelabase/neon`
  checkout, not this repo).
- Fork images: build once with `publish-dev.sh --no-push --allow-unpushed`, test, then push
  as-is with `--push-only`. Never rebuild between test and push.
- When a second bug turns up in the same area, write the invariant down and test it with a
  model, property or exhaustive test.

## White-room audits

Each milestone issue declares one review level with a one-line reason:

- **whiteroom** — a fresh agent audits with only the issue and the diff, writes adversarial
  tests (not committed), and does not fix the code. Required when the milestone:
  - touches authentication, crypto, secrets, roles, privileges, RLS or member access
    (`crates/chelabase-auth`, `crates/chelabase-secrets`, `services/auth`,
    `crates/chelabase-zitadel`, privilege code in `crates/chelabase-metadata`);
  - adds or changes a migration, or the metadata migrator;
  - makes a breaking change to a published contract (`openapi/`, `packages/client`, the
    compute spec);
  - changes fork C in replication, WAL, auth or locking, or lock ordering in the metadata
    path;
  - or the human asks for it.
- **standard** (default) — no separate audit; the whole-epic review covers it.
- **none** — docs, chore, CI, Studio layout/styling, refactors that change no behaviour.
  Never applies to a whiteroom path.

## Tests

- Backend behaviour and bug fixes are test-first. A bug fix starts from a failing
  reproduction. The builder writes both the tests and the code; there is no separate test
  agent, so for each new test the builder's report shows one failing run from before the
  fix.
- UI logic (hooks, reducers, validation) gets unit tests next to the code. Layout and
  styling get no unit tests; a browser check or Impeccable's finish review covers them when
  they matter.
- Docs, config and plumbing need no tests; say so in the PR.
- Each milestone runs only its own crate's tests and its new or changed live tests.
  `just check` and the full live pass run once per epic, before the PR is marked ready.

## Workflow

1. **Plan.** Anything non-trivial: `/epic <goal>` → draft milestones as issues → human OK →
   issues, `epic/<slug>` branch, draft PR.
2. **Build.** `/build <slug>`: ready milestones in parallel worktrees (disjoint files).
   Whiteroom milestones go to `/audit` before anything builds on them.
3. **Close.** Whole-epic review, one `just check`, one live pass → mark the PR ready.
   Human merges.
4. **Small change.** `/quick <change|#issue>`: one worktree, one PR, affected tests, squash.

| Command | Args | What it does |
|---|---|---|
| `/epic` | `<goal>` \| `#<issue>` \| `APPROVED <slug>` | Draft milestone issues → after OK, file them, open `epic/<slug>` and a draft PR |
| `/build` | `<slug> [n]` | Build ready milestones; audit whiteroom; finish with epic review + gate |
| `/audit` | `<slug> <n>` | White-room audit of a landed milestone (issue + diff only) |
| `/quick` | `<change>` \| `#<issue>` | Small change → one conventional PR |

Issue bodies follow the `issue-spec` skill; PR bodies follow `pr-body`. Labels:
`epic:<slug>`, `type:*`, `area:*`, `status:{ready,blocked,building,landed,audited,changes-requested}`,
`review:whiteroom`.
