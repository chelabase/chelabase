# One stack per worktree: Implementation Plan (infra chore)

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:subagent-driven-development. Strict TDD where code is testable (Rust, shell helpers with a test script); RED before code. Work only in your task's worktree, with absolute paths. Read `CLAUDE.md`.

**Goal:** several worktrees (and sessions) run their own app tier side by side: their own `just dev`, ports, metadata database, NATS, Valkey and proxy, over one shared heavy tier (Neon storage, Zitadel, Postgres server). Live tests and e2e runs from different worktrees stop colliding.

**Origin:** the user asked for it on 2026-10-02 ("the sooner the better"); roadmap entry "One stack per worktree" under "Can be done at any point". The user approved Part 1 of the design and then said "just build it": the rest of the design is the rulings below, reported to the user at the end.

## Status

- **Handoff:**
  - 6c (`data-api-databases`) must merge `main` and run `just stack-claim` before any `deps-up`, `dev`, `deps-down` or `sweep-dbs`.
  - 6c's merge will conflict in about 13 shared files (`live_db.rs`, `reconcile.rs`, `backend.rs`, `worker/config.rs`, the orchestrator's `lib.rs`/`main.rs`, and others); its new `rest_live`/`live_db` code must keep S8's per-run scoping.
  - The fork's Task 12 edits `docker-compose.yml`, which is now `infra/compose/shared.yml`; the storage pin is repeated in `slot.yml` and checked by `slots.sh`.
- **Current (2026-10-02): MERGED into `main` (`eb06cda`) at the user's word.** All tasks done (T1–T5, Phase B); the end-of-step review (opus) found 0 Critical, 2 Important (README migration steps; pre-slot branches' handoff) and 9 Minor; the fix batch `bc26563`..`4561ff2` fixed all of them (re-review clean), merged `2feed3d`.
- **Verification run (2026-10-02, on `a2872d5`, slot 0 on scratch `cheladb_metadata_verify`):** nextest 999/999, clippy clean, `just check`, `stack-test`, `live_db` 89/89, `roles_live` 30/30, `rest_live` 24/24, `neon-test`, `neon-e2e`, `data-e2e`, `role-e2e`, `branch-e2e`; **concurrency:** slot 1's `neon-e2e` and `data-e2e` passed while slot 0 ran `role-e2e`. After the fix batch: `stack-test` and the relay's 11 tests pass. Stack left with the shared tier up, no slots, computes or dev processes.
- **Ruling:** Z1. Two Studios on `localhost` share their session cookies, so one signed-in Studio at a time (documented, not changed). *Cost: re-sign-in when switching slots.*
- **User's ruling W8 (2026-10-02):** "turn off required only in non-platform mode": in self-hosted mode the bootstrap switches off Zitadel's instance "Login v2 required" and gives every Studio app (slot 0 included) its own login base URI; platform mode is unchanged.
- **Phase B (live, opus):** slots 0 (scratch metadata DB) and 1 ran side by side; `neon-e2e` passed in both, the relay routed each slot's hooks to it, slot 1's reveal showed 4532/4544. Fixed on the way: `8991ba4` (relay: no system CA certs), `26cf710` (register-safekeepers one-shot race in `deps-up`), `79fd66d` (`check.mjs` proxy port), `7dfaf3c` (Studio `--port` flag). Torn down; the shared tier stays up on the new pins.
- **T4** `0caf9ef` docs merged. **T5** `535964f` (W8) and `c2de5ba` (Zitadel rejects an app update whose name is unchanged, so no app update ever applied live): slot 1 signs in on 3100 with slot 0's Studio stopped; sign-out stays on 3100. The Zitadel console now uses Zitadel's built-in v1 login.
- **More user decisions (2026-10-02):** **W6.** Phase B approved (stack-migrate, slot 0 on a scratch DB + slot 1 side by side, tear down, shared tier left up). **W7.** One Studio OIDC app per slot in Zitadel, each with its own login URL (port 3000+100N), so slot N signs in on its own.
- **Done:** T1 `99b31b1`+`f19e00f` (one fix round: 5 s per slot, 10 s overall deadline), T3 `9e62657`+`46042e6` (one fix round: per-slot `CHELADB_DEV_DIR`), T2 `67f1c47`..`e3cc629` (two fix rounds: slot proxy ports in the orchestrator; teardown only from the registry entry; hooks 404 for tenants a slot doesn't own; W7; `wt-add` never frees a live slot; neon-e2e asserts hook delivery). Pins: `neon-compute-v17` and `neon-storage` `532981ad7de5-dev` (fork F3, 11a, Postgres minors).
- **Rulings during execution:** Y1. Computes carry a `cheladb.dev/slot` label and each worker lists only its own slot's (otherwise slot N's reconcile would remove slot 0's computes). *Cost: none.* Y2. Stale registry entries are never reused; `just stack-free N` wipes one. *Cost: one command.* Y3. The relay image copies a host-built glibc binary onto `ubuntu:26.04` (dev only). *Cost: a base bump if the other computer's glibc is newer than 2.43.* Y4. `neon-smoke` and `storcon_live` no longer assert hook delivery for a throwaway tenant (every slot 404s it by design); `neon-e2e` asserts it for a real tenant instead. *Cost: none.*
- **User's decisions (2026-10-02):**
  - **W1. Isolation:** the app tier is per worktree (metadata *database* on the shared Postgres server, NATS, Valkey, proxy, the dev services and their ports, compute labels); the heavy tier is shared (Postgres server, storage-controller database, Neon storage, Zitadel, Mailpit).
  - **W2. Ports:** numbered slots. The main checkout is slot 0 with today's ports; `just wt-add` takes the lowest free slot 1–9; every port = today's port + slot × 100.
  - **W3. Two Compose projects:** `infra/compose/shared.yml` (project `cheladb`) and `infra/compose/slot.yml` (project `cheladb-s<N>`).
  - **W4. Storage-controller hooks:** a small hook relay in the shared tier forwards each hook to the slot that knows the tenant.
  - **W5. Design Part 1 approved as presented** (slots registry, `.env.local`, `just deps-up`/`deps-down`/`dev-down` per slot, `just stack`).
- **Rulings (the rest of the design; each with its cost if wrong):**
  - **S1. Keep the shared project name `cheladb` and the existing service and volume names** for everything that moves to `shared.yml`, so today's metadata, storcon, pageserver, safekeeper, object-store and Zitadel volumes are reused with no migration. NATS, Valkey and the proxy move to `slot.yml`; their old containers in project `cheladb` are removed once by the migration step (`just stack-migrate`), their data is disposable. *Cost: one manual `just stack-migrate` per machine (both computers).*
  - **S2. Slot registry:** `.cheladb/slots/<N>.env` (in the shared `.cheladb`, symlinked into worktrees) holds `CHELADB_SLOT`, the worktree path and every derived variable; the worktree's `.env.local` sources it. Slot 0 is implicit for the main checkout (no file needed; defaults = today). A registry entry whose worktree path no longer exists is free. *Cost: none.*
  - **S3. Metadata database per slot:** `cheladb_metadata` stays slot 0's; slot N uses `cheladb_metadata_s<N>`, created by `just deps-up` if missing (`CREATE DATABASE … OWNER cheladb`), migrated by `just dev` as today. The storage controller's own database is shared (it's one storage controller). *Cost: none.*
  - **S4. Zitadel:** one instance; `just zitadel-bootstrap` registers Studio's redirect and post-logout URIs and the gateway's allowed origins for all ten slots (ports 3000…3900) once. The demo tenant/project ids stay the same in every slot (each slot's metadata database seeds its own rows). *Cost: ten redirect URIs on a dev Zitadel.*
  - **S5. Hook relay:** a Rust binary `cheladb-hook-relay` (new crate `services/hook-relay`, axum, no new dependencies) in the shared tier, listening on 8083's replacement port **8099**; the storage controller's `--control-plane-url` points at it (`http://host.docker.internal:8099/storcon`). For each request it reads the registry directory (mounted read-only), and tries each registered slot's internal listener (`http://host.docker.internal:<8083+100N>/storcon/...`, same path, method, headers and body) in slot order; the first answer that isn't 404 is returned; all 404 (or unreachable) → 404; it forwards the bearer unchanged and never logs bodies. It runs as a Compose service built from a tiny Dockerfile that copies a binary built by `just hook-relay-build` (musl or the host target, whichever the existing images use; T1 decides and states it). *Cost: one small service; a tenant known to no running slot gets 404 (the storage controller retries).*
  - **S6. Proxy per slot:** the slot's proxy publishes `4432+100N`/`4444+100N` and authenticates against its slot's internal listener (`--auth-endpoint=http://host.docker.internal:<8083+100N>/proxy/api/v1`); the proxy CA/cert are shared files. Computes stay on the shared Docker network (`cheladb_default`) so they reach storage; they get a `cheladb.dev/slot=<N>` label. *Cost: none.*
  - **S7. Compute `neon.console_url` and spec URL** use the slot's internal listener port (already config-driven: `CHELADB_INTERNAL_ADDR`/`CHELADB_CONTROL_PLANE_URI` per slot in `.env.local`). *Cost: none.*
  - **S8. Sweeps are per slot:** `just sweep-dbs` removes only containers labelled with this slot and only this slot's metadata rows/tenants; `live_db`'s `sweep_leftovers` (and any other global cleanup in tests/scripts) only touches rows the run created (unique per-run name prefix), never everything. *Cost: leftovers from crashed runs are swept by the slot's own `sweep-dbs`.*
  - **S9. Scripts and tests read ports from the environment** (`.env.local` sourced by `just`; tests read `CHELADB_*` vars with today's values as defaults), so slot 0 behaves exactly as today. *Cost: none.*
  - **S10. Docs:** CLAUDE.md's "Compose is one stack per machine" rule becomes "one app tier per worktree; the shared tier is shared: don't `deps-down` it while other slots run"; README section "Running several worktrees"; roadmap entry marked done. *Cost: none.*

## Global Constraints

- Slot 0 (the main checkout, and any worktree without a registry entry) must behave exactly as today: same ports, same database names, same commands. Every task proves it.
- Never overwrite `.cheladb/secrets/secrets.key`, the gateway key pair, the cookie key or the compute-JWT key pair; never delete the shared volumes.
- Pin images by digest as today; no new dependencies (crates/npm/images) without checking the latest release first and recording it.
- Never `cargo fmt` the workspace. Commits: `git commit`, `Co-Authored-By` and `Claude-Session` trailers; implementers don't push or merge.
- Don't touch the running Compose stack until the coordinator says it's free (the fork session's e2e holds it next).

## Models and schedule

| Task | What | Implementer | Review |
|---|---|---|---|
| T1 | `services/hook-relay` (S5) with tests | sonnet | sonnet |
| T2 | Compose split, slots, justfile, `.env.local`, Zitadel, proxy, migration, `just stack` (S1–S4, S6, S7, S9), then live bring-up of slot 0 and a slot 1 side by side | opus | opus |
| T3 | Per-run test/script cleanup scoping (S8) and env-driven ports in tests/scripts (S9) | sonnet | sonnet |
| T4 | Docs (S10) | haiku | none (end review) |
| — | End-of-step review | — | opus |

- **Wave 1:** T1 ∥ T2 (code only) ∥ T3. T2 owns `infra/compose/*`, `justfile`, `scripts/lib/*`, `.env.example`; T3 owns test files and the e2e scripts' cleanup lines; T1 owns the new crate.
- **Wave 2:** T2's live bring-up (needs T1's binary and the free stack), then T4, the verification (slot 0: the usual full run; slot 1: `just neon-e2e` and `data-e2e` concurrently with slot 0's `role-e2e`), the end review.

## Task 1: hook relay

**Files:** `services/hook-relay/{Cargo.toml,src/main.rs,src/lib.rs}`, root `Cargo.toml` (member), `infra/compose/hook-relay.Dockerfile`, `justfile` recipe `hook-relay-build` (coordinate: T2 owns the justfile — put the recipe text in your report and T2 adds it, or add it in a clearly separate block; state which).
**Interfaces:** `cheladb-hook-relay --listen 0.0.0.0:8099 --slots-dir /slots`; slot files are `KEY=VALUE` lines with `CHELADB_SLOT` and `CHELADB_INTERNAL_PORT`.
**Tests:** `forwards_to_the_slot_that_knows_the_tenant`; `all_404_is_404`; `an_unreachable_slot_is_skipped`; `headers_body_and_method_are_forwarded`; `slot_order_is_stable`; `bodies_are_never_logged` (a tracing capture or a code-level guarantee you can test).

## Task 2: Compose split and slots

**Files:** `infra/compose/{shared.yml,slot.yml}` (from `docker-compose.yml`), `justfile` (`deps-up`, `deps-down`, `dev`, `dev-down`, `wt-add`, `wt-remove`, `sweep-dbs`, `zitadel-bootstrap`, new `stack`, `stack-migrate`, `hook-relay-build`), `scripts/lib/*`, `scripts/zitadel-*`, `.env.example`, `apps/studio` env wiring if ports are read there, `infra/neon/tests/run.sh` if it asserts compose text.
**Behaviour:** S1–S4, S6, S7, S9. Code first without touching the running stack; then, when the coordinator says the stack is free: `just stack-migrate` on this machine, slot 0 up and `just neon-e2e` green, a slot 1 worktree (`just wt-add stacks-probe`) up with its own `just dev` and `just neon-e2e` green **while slot 0's stack is up**, `just stack` listing both, `just wt-remove stacks-probe` freeing slot 1.
**Tests:** a shell test script `infra/compose/tests/slots.sh` (run by a `just stack-test` recipe): slot allocation (lowest free, reuse after remove, stale path frees), port arithmetic, generated `.env.local` contents for slots 0 and 3, compose config renders (`docker compose -f … config`) with slot variables, slot 0 unchanged.

## Task 3: test and script scoping

**Files:** `crates/cheladb-metadata/tests/live_db.rs` (`sweep_leftovers` and helpers), other live tests with global cleanup (`services/worker/tests/*_live.rs`, `services/rest/tests/rest_live.rs`, `crates/cheladb-neon/tests/*live*`), `scripts/*-e2e.sh` (ports/URLs from env with today's defaults; cleanup only their own database).
**Behaviour:** S8, S9. Each live run uses a unique name prefix and deletes only what it created; hard-coded `127.0.0.1:5433`, `:8083`, `:8000`, `:4432`, `:4444` become env-driven with today's defaults.
**Tests:** `live_db` runs twice concurrently against the same database without failures (show it); the e2e scripts' `bash -n`; a grep check that no test file hard-codes those ports outside defaults.

## Task 4: docs

S10, plus the roadmap entry marked done with a pointer here.

## Verification

Slot 0: `cargo nextest run --workspace`, clippy, `just check`, `live_db`, `roles_live`, `rest_live`, `neon-test`, `neon-e2e`, `data-e2e`, `role-e2e`, `branch-e2e`. Slot 1 concurrently: `neon-e2e` and `data-e2e` while slot 0 runs `role-e2e`. Then the opus end review and the user's merge pause.
