# Login roles follow-ups (chore after step 6a)

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:subagent-driven-development. Strict TDD (RED run shown before code). Work only in your task's worktree, with absolute paths.

**Origin:** the open follow-ups in step 6a's Status (`docs/superpowers/plans/2026-10-02-login-roles-v2.md`, "Follow-ups that stay open"). The user put all of them in scope on 2026-10-02 and decided the four design calls below. Spec: 6a's spec (`docs/superpowers/specs/2026-10-01-login-roles-design.md`) still wins on behaviour except where the decisions below change it.

**Tech stack:** unchanged from 6a. **No new dependencies.**

## Status

- **Current (2026-10-02): plan approved by the user; T1–T5 and T7 merged; T6 done; full verification passed; the end-of-step review (opus) and its fix batch are done; AT THE USER'S MERGE PAUSE.** The fork session recorded F3 as its Task 11e (tag to follow) and released the Compose stack to us until it asks for a 45–60 min slot. Branch `login-roles-followups` (`.worktrees/login-roles-followups`), from `main` `8d67f8c`.
- **User's decisions (2026-10-02):**
  - **D1. Scope:** all ten open follow-ups: the clear fixes, the small design calls, the provisioning slowdown and rename + `PASSWORD NULL`.
  - **D2. An unopenable sealed password gets its own `password_state`: `unreadable`** (password null, URI without a password). Studio: "Stored password can't be read; reset it."
  - **D3. The revealed URI keeps `sslmode=verify-full`.** In self-hosted mode the reveal adds a hint for the dev CA (README and Studio show the `sslrootcert=` form). No downgrade.
  - **D4. Restore and reset-to-parent clear the `forwarded_at` guard** for the endpoint right before their resync (the catalog was replaced, so the guard means nothing).
  - **D5. Rename + `PASSWORD NULL`: fork task F3 plus receiver support now.** The receiver honours an explicit `"password": null` on a rename; F3 (below) makes the fork send it.
- **Pre-flight scan (2026-10-02, static, sonnet):** findings are folded into the tasks. Two premises were wrong:
  - The ddl route already has `DefaultBodyLimit::max(64 KiB)` (`internal/mod.rs:35,221-225`, test `ddl_body_is_limited_to_64_kib`). What's true is that the `Bytes` extractor buffers the body before `authorize_compute` runs.
  - The reveal doesn't drop an unopenable password: it answers 500 `credentials_unavailable` (deliberate: the whole secrets key is wrong). The silent drops are in the worker's `child_role_copies` (`branch_create.rs:307-322`, unprotected parent) and the receiver's `reseal_password` (`internal/ddl.rs:~818-846`, rename). Both leave `password_enc` NULL, so the role reads `set_in_sql`.
  - Provisioning has no timing at all today; readiness polls every 500 ms through `docker exec psql` (`provision.rs:241-253`, `config.rs:140-142`).
- **Rulings (mechanical; each with its cost if wrong):**
  - **C1. Resync repairs run one role group per `admin_sql` call.** `plan_resync` returns `Vec<RepairGroup>` (one per role: create/set password, owner attributes, GRANT ADMIN). A failed group is logged, its role's metadata upsert is skipped, the rest go on, and the resync returns `Err` naming the failed roles after `apply_resync` and the publish. Same for the fresh-password script. *Cost: N `docker exec` calls instead of one when there are N repairs (repairs are rare).*
  - **C2. `branch_databases` guard is one stamp per endpoint:** a new `neon_endpoints.dbs_forwarded_at timestamptz`, stamped by every `DdlTx` database write (`upsert_database`, `rename_database`, `remove_database`). `set_databases` leaves `branch_databases` alone while it's under 10 s old (a drop leaves no row to stamp, so per-row stamps can't cover it). *Cost: a resync within 10 s of any database DDL skips all database repairs for one round.*
  - **C3. D4's clear** is `MetadataClient::clear_forwarded(endpoint_id)` (sets `neon_roles.forwarded_at` and `neon_endpoints.dbs_forwarded_at` to NULL), called from `finish_reset` and `finish_restore` (and reconcile's `finish_switch`) after `lock_endpoint`, before the resync. Not on `UpgradeCompute`/`RecreateBranchCompute` (the catalog wasn't replaced). *Cost: none.*
  - **C4. D2 is stored, not computed:** a new `neon_roles.password_unreadable boolean NOT NULL DEFAULT false`, set where a sealed password is dropped (worker `child_role_copies`; receiver `reseal_password`) and cleared by any write that stores a new sealed password or verifier for the row (reset, receiver `set`, resync password change). `list_roles` derives `Unreadable` from it (before `SetInSql`). The reveal's 500 for a sealed value that won't open at reveal time stays (that means the secrets key itself is wrong). *Cost: one column.*
  - **C5. D3's hint:** `ConnectionDetails` gains `ca_file: Option<String>`. The orchestrator keeps `mode: Mode` in `AppState`; in self-hosted mode `ca_file` is `CHELADB_PROXY_CA_FILE` (default `.cheladb/neon/proxy-ca.crt`), in platform mode `None`. Studio's connection panel and the role reveal panel render the `sslrootcert=` line from it, replacing the hard-coded note. *Cost: one optional field.*
  - **C6. The ddl route authorizes before buffering:** `receive` takes `axum::body::Body` and calls `axum::body::to_bytes(body, DDL_BODY_LIMIT)` after `authorize_compute`; over the limit stays 413. *Cost: none.*
  - **C7. D5's shape:** `RoleOp.password` becomes `Option<Option<String>>` (`#[serde(default, deserialize_with = "nullable")]`). Absent → as today. Explicit `null` with `old_name` → `RenameRole` then `SetRole { password: None }` (so the copy is removed, or the owner text E8 refuses). Explicit `null` without `old_name` → as today's missing password. *Cost: none if F3 sends exactly `"password": null`; the F3 text pins that.*
  - **C8. The protected flag joins the endpoint insert:** `create_endpoint_for_branch` gains `parent_protected_seen: bool`; inside its transaction it reads `SELECT protected FROM branches WHERE id = <parent> FOR SHARE` and returns `MetadataError::ParentProtectionChanged` when it differs. The worker re-runs `child_role_copies` and the insert once more on that error (at most 2 retries, then fails the branch). *Cost: a retry when the user toggles Protect during a child create.*
  - **C9. `list_branches` batch:** `MetadataClient::branches_with_main_database(ids: &[BranchId]) -> Result<HashSet<BranchId>, MetadataError>` (in `crates/cheladb-metadata/src/branches.rs`), one query; `branch_view` keeps its single-branch path for create/update. *Cost: none.*
  - **C10. Studio requests:** `DatabaseActions` enables `useListBranches` only while its Delete panel is open. `DataApiKeys` stops calling `useListRoles`: each key in the data keys list gains `role_blocked: bool` (the key's role is owner-level, the same check as `blocked_keys`). This reverses 6a's R21. *Cost: one field on the keys API.*
  - **C11. `just neon-test`'s curl check:** the smoke run gets `NEON_SMOKE_MAX_TIME=3` and the check accepts `curl: (7)` or `curl: (28)` (this machine drops packets to port 1). *Cost: none.*
- **Fork task to queue for the fork session (the user relays it, or approves sending it):**
  - **F3. Say when a rename removes the password.** When one transaction renames a role and sets `PASSWORD NULL`, the forwarded role entry carries `"password": null` (an explicit JSON null, with no `encrypted_password` key) next to `old_name`. Every other entry stays as today (no password key when the password didn't change).
- **Done:**
  - T2: `626b807` (no review, per plan), merged `ebcbcfd`.
  - T1: `3ad96af` (review clean, sonnet), merged `0eecaa6`.
  - T4: `e23a40d`..`4ae9ed4` (review clean, sonnet), merged `80b2832`.
  - T3: `0eecaa6`..`b476389` (review clean, sonnet), merged `ba2e521`.
  - T7: `6a4a6f3` (no review, per plan), merged `347bd58`.
  - T5: `64b3b14` (review clean, haiku), merged; see X5.
  - T6 (opus, live, no code change, no F4 needed): **no regression.** 11 alternating provisions per image: current pin `b2402faa2394` median 6096 ms (3850–7807), old `fa6917ea6ba6` median 6342 ms (3971–7641); child branch 2879 vs 2892 ms; compute_ctl start-up equal. The 3.9–8.2 s spread is host CPU load (the fork session's Neon CI ran during almost every run); `create_timeline` (pageserver initdb, 1.4–2.8 s) and `CREATE DATABASE` in the 1-CPU compute stretch under load. Our polling, bootstrap and resync add 0.3–0.9 s; the 1 s republish runs in the background. Cheap wins proposed, not built: readiness poll 100–150 ms (~150 ms), one bootstrap SQL call instead of two (~100–200 ms), Docker `"userland-proxy": false` (user's call; host-port publishing costs ~0.4–0.6 s per start, unmeasured fix), and `create_timeline` (~2.2 s) as the storage-side target. A pre-existing orphan `neon_endpoints` row `ep-7a709dcf2d54` (2026-10-01, no branch, no container) was left alone.
  - Verification run (sonnet) on `04ba633`: nextest 961 passed, clippy clean, `just check`, Studio 302 + typecheck + build, `live_db` 87/87 (first run collided with the fix agent's `live_db` run on the shared metadata DB; rerun alone clean), `roles_live` 30/30, `rest_live` 24/24 (first run compiled against 6c's artifacts in the shared target dir; clean after a touch), `branch_live` 1/1, `neon-test`, `role-e2e`, `data-e2e`, `neon-e2e`, `branch-e2e`.
  - End-of-step review (opus): ready after fixes. I1 a deadlock between a child endpoint insert (child row, then parent FOR SHARE) and `begin_delete` (every branch in scan order); I2 the README claimed more than built (F3 dependency, the `unreadable` cause, what `CHELADB_PROXY_CA_FILE` does). Fix batch (sonnet) `bde7c35`..`c742f6f`: I1 (the insert locks the database row first), I2, and minors (rename keeps the `unreadable` mark when nothing was resealed; the keys list survives a failing role lookup; test name/text; log wording; a test that a failed role isn't published). Scoped re-review: all addressed. Merged `8c663ae`; after it: metadata/orchestrator/worker nextest 476/476, clippy clean, `live_db` 89/89, `branch_live` 1/1, `branch-e2e` passed.
  - Deferred minors kept (triaged by the review): T1 over-long lines, stamps on no-op database writes, unknown-endpoint no-ops; T4 `to_bytes` errors → 413, keys list's extra `list_roles` query; T3 X4, `Result<Result<..>>`, marks lost on a crash between insert and marks (fix later by putting `password_unreadable` on `NewRoleCopy`), half-failed repair groups.
  - The metadata DB holds 71 soft-deleted `databases` rows, 6 `neon_endpoints` rows and 119 `branches` rows from test runs, with no computes behind them (not swept).
- **Rulings during execution:**
  - **X1.** `role_blocked` is computed in the orchestrator (T4), not metadata. *Cost: none.*
  - **X2.** T4 added `AppState.proxy_ca_file` next to `mode`, and removed the ddl route's `DefaultBodyLimit` layer (`to_bytes` enforces 64 KiB after auth). *Cost: none.*
  - **X3.** T3: a failed `mark_password_unreadable` only WARNs; the create goes on. *Cost: that row reads `set_in_sql` until a later write.*
  - **X4.** T3: the restore-only clear is a `Catalog { Kept, Replaced }` argument; `finish_switch` passes `Replaced` whenever it re-creates a missing compute. *Cost: the 10 s guard can lift early after a crash before the switch.*
  - **X5.** T5: the database Delete button stays enabled; the confirm button in the panel is disabled while branches load and while any branch is protected, with the reason shown. (Before, the Delete button itself was disabled, which needed the branches list on every dashboard row.) *Cost: one more click to see the reason. Approved by the user 2026-10-02.*
- **Deferred minors:**
  - T1: over-long lines; DdlTx database writes stamp even when a rename/remove matched nothing; `clear_forwarded`/`mark_password_unreadable` treat an unknown endpoint as a no-op.
  - T4: every `to_bytes` error maps to 413; a stray blank line in `ddl.rs` `mod text`; the owner explicit-null test name oversells; listing keys costs one `list_roles` query.
  - T3: `finish_switch` (X4); log wording "repairing the role failed"; no test that a failed role is left out of the published names; `Result<Result<..>>` in `branch_create`; a crash between insert and marks loses the unreadable marks; a half-failed repair group leaves partial catalog changes until the next resync.
  - T7: check that the README's rename + `PASSWORD NULL` text says it depends on the fork image (F3).

## Models and schedule

Waves; parallel tasks touch disjoint files and each gets its own worktree (`just wt-add login-roles-followups-tN login-roles-followups`), merged back `--no-ff`.

| Task | What | Implementer | Review |
|---|---|---|---|
| T1 | Metadata: migration + metadata functions | sonnet | sonnet (everything builds on it) |
| T2 | `just neon-test` curl check | haiku | none (end-of-step review) |
| T3 | Worker: resync isolation, restore clear, protected race, unreadable flag, timing logs | opus | sonnet |
| T4 | Orchestrator: receiver (C6, C7, unreadable), `list_branches` batch, reveal hint, key `role_blocked`, OpenAPI | sonnet | sonnet |
| T5 | Studio + regenerated client | sonnet | haiku |
| T6 | Provisioning slowdown: live measurement and fix (or a fork task) | opus | none (findings go to the user; end-of-step review covers any code) |
| T7 | README, `.env.example`, 6a Status and roadmap | haiku | none (end-of-step review) |
| — | End-of-step whole-branch review | — | opus |

- **Wave 1:** T1 ∥ T2.
- **Wave 2 (after T1 merged):** T3 ∥ T4.
- **Wave 3 (after T4 merged; T5 needs the OpenAPI document):** T5 ∥ T7. T6 starts after T3 merged (it uses T3's timing logs) and runs alone on the Compose stack.
- Then: the live verification run (one at a time), the end-of-step review, fixes, and the user's merge pause.

## Task 1: metadata

**Files:** `crates/cheladb-metadata/migrations/20261005000001_role_followups.sql` (new), `crates/cheladb-metadata/src/{neon.rs,ddl.rs,branches.rs,lib.rs}`, `crates/cheladb-metadata/tests/live_db.rs`.

**Migration (new file; never edit `20261004000001`):**
```sql
ALTER TABLE neon_endpoints ADD COLUMN dbs_forwarded_at timestamptz;
ALTER TABLE neon_roles ADD COLUMN password_unreadable boolean NOT NULL DEFAULT false;
```

**Interfaces:**
- `PasswordState::Unreadable` (serde `unreadable`), derived in `list_roles` from `password_unreadable` (order: `Md5Unusable`, `Generated`, `Unreadable`, `SetInSql`).
- `DdlTx::{upsert_database, rename_database, remove_database}` also run `UPDATE neon_endpoints SET dbs_forwarded_at = now() WHERE id = <endpoint>`.
- `set_databases` returns early (no change) when `dbs_forwarded_at > now() - interval '10 seconds'`.
- `MetadataClient::clear_forwarded(&self, endpoint_id: &str) -> Result<(), MetadataError>` (C3).
- `MetadataClient::mark_password_unreadable(&self, endpoint_id: &str, name: &str) -> Result<(), MetadataError>`, and `DdlTx::mark_password_unreadable(&mut self, name: &str)`. Every write that sets `password_enc` or `scram_verifier` on a row also sets `password_unreadable = false`.
- `create_endpoint_for_branch(…, parent_protected_seen: bool)`, `MetadataError::ParentProtectionChanged` (C8).
- `MetadataClient::branches_with_main_database(&self, ids: &[BranchId]) -> Result<HashSet<BranchId>, MetadataError>` (C9).
- `MetadataClient::data_keys` rows gain `role_blocked: bool` only if the owner-level check lives in metadata; otherwise T4 computes it (implementer reports which).

**Tests (live_db, names and assertions):** `database_ddl_stamps_the_endpoint` (each of the three writes sets `dbs_forwarded_at`); `resync_skips_databases_after_a_recent_ddl` (stamp now → `set_databases` changes nothing; stamp 11 s ago → it applies); `clear_forwarded_lifts_both_guards`; `unreadable_password_state` (mark → `list_roles` says `unreadable`; a later reseal → `generated`); `endpoint_insert_refuses_a_changed_parent_protection`; `branches_with_main_database_in_one_query` (two branches, one missing main).

**Verify:** `just --justfile <wt>/justfile --working-directory <wt> test -p cheladb-metadata`, the `live_db` tests, clippy.

## Task 2: `just neon-test` curl check

**Files:** `infra/neon/tests/run.sh` (lines ~151-158). Apply C11. RED: the check fails today on this machine (show it). **Verify:** `just neon-test` (needs only the deps stack being up or not; this check uses port 1).

## Task 3: worker

**Files:** `services/worker/src/{resync.rs,branch_switch.rs,branch_create.rs,reconcile.rs,provision.rs,ports.rs,fakes.rs}`, `crates/cheladb-neon/src/backend.rs` (fake only: a needle-based failure hook, `fail_when(needle)`).

**Behaviour:** C1, C3, C8, and C4's worker side (`child_role_copies` calls `mark_password_unreadable` after the insert for every copy whose sealed password didn't open, keeping the WARN). Timing: `handle_provision` and `handle_branch_create` log `elapsed_ms` on "database ready"/"branch ready", plus `debug!` per phase (`create_tenant`, `create_timeline`, `computes.create`, `wait_until_ready`, bootstrap, resync).

**Tests:** `a_failing_repair_does_not_stop_the_others` (two api roles, the first role's repair fails → the second's runs, its metadata row is written, the error names the first); `failed_fresh_password_group_is_isolated`; `restore_clears_the_forwarded_guard_before_resync` (journal order: `lock_endpoint`, `clear_forwarded`, resync); `upgrade_does_not_clear_the_guard`; `child_create_retries_on_changed_protection` (first insert returns `ParentProtectionChanged`, second succeeds with fresh passwords); `child_create_marks_unopenable_passwords_unreadable`; `provision_logs_elapsed` (optional, via tracing-test only if already a dev-dependency; otherwise skip and say so).

**Verify:** `test -p cheladb-worker`, clippy; `roles_live` and `branch_live` run later in the verification run.

## Task 4: orchestrator

**Files:** `services/orchestrator/src/{internal/ddl.rs,internal/mod.rs,branches.rs,lib.rs,main.rs,data_keys.rs,openapi.rs}`, `services/orchestrator/tests/{internal.rs,api.rs}`, `openapi/management.json` and the generated client via `just api-gen` (commit both).

**Behaviour:** C6, C7, C4's receiver side (`reseal_password` failure → `mark_password_unreadable` in the same `DdlTx`), C9 in `list_branches`, C5 (`AppState.mode`, `CHELADB_PROXY_CA_FILE`), C10's `role_blocked`, and `PasswordState::Unreadable` in the documents.

**Tests:** `ddl_token_is_checked_before_the_body_is_buffered` (no token + a body over the limit → 401, not 413); `ddl_rename_with_explicit_null_password_removes_the_copy`; `ddl_rename_with_explicit_null_on_the_owner_is_refused` (E8 text); `ddl_rename_without_password_key_keeps_the_copy` (unchanged behaviour); `ddl_rename_marks_an_unopenable_password_unreadable`; `list_branches_reads_databases_once` (fake counts calls); `reveal_carries_the_ca_file_in_self_hosted_mode` / `reveal_has_no_ca_file_in_platform_mode`; `data_keys_mark_blocked_roles`; `list_roles_reports_unreadable`. Existing `sslmode=verify-full` URI assertions stay unchanged.

**Verify:** `test -p cheladb-orchestrator`, clippy, `just api-gen` then `git diff --exit-code` clean after commit, `just check`.

## Task 5: Studio

**Files:** `apps/studio/src/lib/password-state.ts`, `apps/studio/src/components/{database-actions.tsx,branch-roles.tsx,data-api-keys.tsx}` and their tests.

**Behaviour:** `unreadable` text (D2); the CA line from `ca_file` in both reveal panels (shown only when present; replaces the hard-coded note, the `hostaddr=127.0.0.1` tip stays); C10 (lazy branches list; `role_blocked` instead of the roles request).

**Tests:** `password_state_unreadable_text`; `connection_shows_ca_file_hint` / `no_ca_hint_without_ca_file`; `role_reveal_shows_ca_file_hint`; `branches_list_fetched_only_for_delete_panel`; `blocked_key_badge` rewritten on `role_blocked` and asserting no roles request.

**Verify:** Studio `test`, `typecheck`, `build`.

## Task 6: provisioning slowdown

**Live, alone on the Compose stack.** Measure provision and child-create phases (T3's logs) on the current image `neon-compute-v17:b2402faa2394-dev` and, for comparison, the pre-F1 image `compute-node-v17:fa6917ea6ba6-dev`; 5 runs each. Find which phase grew. If the cause is on our side (polling, bootstrap, resync), fix it with a test; if it's in the image, write the fork task text (F4) with the evidence and stop. Report the table to the coordinator. Delete every test database afterwards.

## Task 7: docs

**Files:** `README.md` (`CHELADB_PROXY_CA_FILE`, the `unreadable` state, `ca_file`, the database guard window), `.env.example`, 6a's plan Status (mark the follow-ups done, pointing here), the roadmap's "Resume here" (only the line about this chore).

## Verification run (one at a time)

`cargo nextest run --workspace`, clippy (`-D warnings`), `just check`, Studio test/typecheck/build, `live_db`, `roles_live`, `rest_live`, `branch_live`, `just neon-test`, `just role-e2e`, `data-e2e`, `neon-e2e`, `branch-e2e`. Touch the sources before verifying (shared target dir, 6a E7).
