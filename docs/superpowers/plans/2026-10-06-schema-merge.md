# Step 6g, three-way schema merge: Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:subagent-driven-development. Strict TDD: show a failing run (RED) before writing code, then GREEN; a compile error counts as RED only when the missing item is the code under test; reports without RED evidence go back. Work only in your task's worktree, with absolute paths, `git -C <wt>`, `cargo … --manifest-path <wt>/Cargo.toml` (or `-p` from `<wt>`), `pnpm --dir <wt>/…` and `just --justfile <wt>/justfile --working-directory <wt>`. Read `CLAUDE.md`. One simple command per Bash call; `Write` rather than heredocs. Never run `cargo fmt` on the workspace (rustfmt only files you create). Never call `EnterWorktree`. Implementer worktrees never run the dev stack; live tests go through the coordinator's `just live …` queue. List every correction you made to this plan in your report.

**Goal:** move schema changes between a branch and its parent (both directions) with a three-way merge (base / target / source), conflicts resolved per item, every merge rehearsed on a throwaway branch of the target, then applied in one transaction plus a resumable online phase; for tables, columns, constraints, indexes, enums, sequences, RLS, policies, grants and roles.

**Architecture:** a new pure crate `crates/chelabase-schema` (catalog SQL that returns one JSON document → normalized model with OIDs → three-way analysis → merged model → phased plan → `.sql` export → drift compare). The worker runs the catalog SQL and the plan through `ComputeBackend::admin_sql_with` (docker exec psql as `cloud_admin`, `SET ROLE postgres`), stores base snapshots, and drives a merge state machine on a new `chelabase.merge` job subject. The orchestrator exposes merge requests; Studio gets a merge page; the control-plane role path grows NOLOGIN roles, memberships and attribute updates.

**Tech stack:** Rust 2024 (tokio-postgres fork for live tests, serde, sha2, pg_query 6.2.1, petgraph 0.8.3), sqlx metadata migrations, NATS JetStream, utoipa/orval, React 19 + TanStack Router/Query + CodeMirror 6 (Studio), Fumadocs (docs).

**Spec:** `docs/superpowers/specs/2026-10-06-schema-merge-design.md` (D1–D18, R1–R6), as amended by U1–U3 and P1–P16 below.

## Status

- **Current (2026-10-08): resuming after Neon fork round 2** (shipped and merged into `main` at `11e64b2`; images `c8d1bb68b81f-dev`, pinned by digest; plan with full history: `docs/superpowers/plans/2026-10-07-neon-fork-round-2.md` on `main`). Round 2 shipped all six patches (1, 2, 3, 4, 5, 6 as D11), plus T3b (multixact after a minor upgrade). Nothing is running. **Resume list (T10b, 2026-10-08; supersedes the "On resume" list below):**
  0. **Done (2026-10-08):** merge `9b1ff0e` (opus; conflicts resolved keeping both sides; `RealStorage::gc_cutoff` via `self.storcon`; sqlx 0.9 applies 6g's older migrations out of order, so no dev DB reset is needed for `schema-merge`; D13's reverse block stands). Sonnet review Approved, 2 minors fixed in `1da5c99` (coordinator checked the diff; the extended `live_db` test `resync_updates_catalog_columns_of_a_recently_forwarded_row` runs in the live pass). Unit tests 1852, clippy clean, no API drift. Note from the merge: main's receiver `SetLogin` writes `login` on API rows, so SQL `ALTER ROLE api_role NOLOGIN` is kept by the resync (softens "control plane wins for LOGIN" in `resync.rs:3`'s header; matches main's design).
     - **E26 (user, 2026-10-08): a secret key is bound to OID + timeline.** D24's OID binding holds on one timeline, but a timeline switch (reset, restore, 6g's reset-after-merge) brings a catalog whose OID counter diverged at the branch point, so `plan_key_roles`' Follow (`resync.rs:66`) could move a key to a different role that reuses the OID. Fix (task **T13k**, opus implements, opus reviews; before the restore rework): store the timeline with the OID; Follow only on the same timeline; after a switch, a key whose OID still carries its name is re-stamped and kept, otherwise revoked and recorded. *Cost: a role renamed and then restored across the rename loses its key.*
     - **T13k DONE:** `0c109dc` (opus; migration `20261012000001` adds `role_timeline`; the timeline comes from the role-check reply, read before and after, `not_ready` if it moved; rotation via `rotated_identity`; the resync skips key repairs if the timeline moved during its read; no backfill, NULL takes the cross-timeline rule; exhaustive invariant test over 34 catalogs). Opus review **Approved**; it judged "same name and same OID, different role after a switch" the ruling's accepted cost (Postgres has no identity beyond OID + name; the receiver refuses the rename/drop that would set it up while a key is live). 3 minors fixed in `b711d72` (regrant only the bound OID; pre-sync serving asserted in the invariant test; accurate no-timeline message), coordinator checked the diff. Live pass: `live_db` `secret_key_role_timeline_round_trips`, `fix_key_roles_applies_each_repair_conditionally`; `roles_live` `reset_does_not_move_a_key_to_a_reused_oid`.
     - **T13l DONE** (opus): `e23e29e`, `99d2188`, fix round `c0b7754`; merged `--no-ff` into `schema-merge`, branch and worktree removed. Restore needed no flow change (already one `multi_level_and_no_reparent` detach). **Implementer's deviation (approved by the opus review):** `schema_base::read_base_live` reads a base that is behind the GC cutoff and sits exactly at the timeline's branch point on the PARENT timeline at that LSN (patch 4 retains it there; `TimelineInfo.ancestor_timeline_id`, `Storage::ancestor()`); any other point re-baselines. **Ruling:** `STUCK_DETACH_SECS` stays 20 min total across attempts (one attempt cut at `DETACH_TIMEOUT` 600 s). Review "Needs fixes" (no Important): docs overclaim, comment arithmetic, exact ancestor assertions, live tests: all fixed in `c0b7754`, coordinator checked the diff. Live pass: `branch_live` `restore_a_child_branch`, `restore_a_grandchild_branch`; `merge_live` `restore_before_a_keep_merge_uses_the_older_base`, `a_branch_point_behind_gc_is_read_on_the_parent`, `a_keep_merge_base_behind_gc_is_unavailable`, `a_restored_branch_reads_its_base_until_the_backup_is_deleted`, `a_grandchild_branch_point_behind_gc_is_read_on_its_parent` (these restart `chelabase-pageserver-1` to shorten the lease deadline). Items 3–6 below are done (no role-lag notes existed). `schema-merge-t13b` discarded (R4; worktree and local branch removed; never on origin).
     - **T14 review DONE (2026-10-08):** five opus slices (state machine, apply/planner, roles/bases, analysis/catalog, Studio/docs/API), findings in `docs/superpowers/plans/2026-10-06-schema-merge-t14-review.md` (`72fb916`); each slice has a checklist of this Status's old deferred/T14 items. Nothing Critical; ~19 Important; many Minor. Build, unit tests, clippy, Studio (605) and docs (14) checks passed.
     - **T14 rulings (user, 2026-10-08):**
       - **E27 (Q1):** a merge `failed` in `online` holds its branches like a running one (reset/restore/delete of either branch and new merges into the target refused), Retry checks the marker first and a gone target fails the merge for good; plus an **Abandon** action: the merge becomes `cancelled` and the NOT VALID constraints and `chelabase_merge_nn_*` checks left on the target are listed as leftovers.
       - **E28 (Q2):** owner-level (created or turned on by a merge) is flagged `security` and needs an acknowledgement, like BYPASSRLS.
       - **E29 (Q3):** a source role rename (OID identity) is applied on the target **through the control plane**: phase 0 renames the role; its proxy copy, stored row and secret keys move with it. **E29a (user, same day):** a rename is flagged breaking and needs an acknowledgement like a drop (clients connecting to the target with the old name fail to authenticate after the apply); Studio shows whether the target has live sessions or secret keys for the role.
       - **E30 (Q4):** role changes the roles path can't make (CREATEDB, CREATEROLE, REPLICATION, comments, any change to `postgres` itself) are reported "apply by hand"; main keeps its values; the drift check ignores them.
       - **E31 (Q5):** target objects whose text names an object the merge renames are compared against what the rehearsal produced (like own-SQL objects), not against M's text.
       - **E32 (Q6): SQL controls everything.** SQL changes to any role stick, API-made or not (memberships, INHERIT, CONNECTION LIMIT, VALID UNTIL, BYPASSRLS, owner-level, LOGIN); the resync records them into the stored row instead of reverting (supersedes T9's "control plane wins" for API roles and the BYPASSRLS strip). Docs updated to say so.
       - **E33 (Q7):** a secret key may use a role with BYPASSRLS; roles that are `neon_superuser` members or have CREATEROLE, CREATEDB or REPLICATION stay refused at create and blocked per request (403 `role_not_allowed`) if SQL escalates them. Studio marks such keys "bypasses RLS".
       - **E34 (Q8):** acknowledgements and the after-merge choice keep a rehearsed merge rehearsed; other edits return it to draft.
       - **E35 (Q9):** the `.sql` export's phase 1 starts with the merge's `lock_timeout` and `statement_timeout`, with a comment that the stale guard and marker are left out.
     - **T14 fix batch (DONE 2026-10-08: F1–F4 merged):** every finding and minor in the review file, in waves by files. Wave 1 in parallel: **F1** schema crate (opus implements, opus reviews; `.worktrees/schema-merge-f1`) and **F2** roles/keys/bases (opus, opus; `-f2`). Wave 2: **F3** merge state machine and apply (opus, opus; `-f3`), using F1/F2's interfaces. Wave 3: **F4** Studio, docs, OpenAPI (sonnet implements, sonnet reviews). Then T15.
       - **F1** `7ad7a74`, `197f113`, `078ce4e` (schema crate; every item in its slices). Implementer rulings: swapped role renames (a↔b) are Unplannable (keys would move twice; the user renames on the target first); membership grantor/options are reported, not merged (E30). **No `MODEL_VERSION` bump** (reviewer agreed): older stored bases lack the E16 row filter, INHERITS reporting and `chelabase_*` role filter, but those differences appear on both sides alike (no-ops; at most a false policy-comment conflict once); a bump would force re-baselines where the base point isn't retained. The digest now includes policy comments, so in-flight merges go stale once. Opus review: Needs fixes (I1 owner memberships after a rename, I2 apply-by-hand SQL undoing main's changes, 7 minors); fix round running.
       - **F2** `33d28ad` (roles/keys/bases; E32, E33, E29 control-plane rename; migrations `20261012000002`–`04`). Deviations: `role_cap_reached` is 409 on updateRole (consistent with create); superseded base models are dropped but entries kept; under E32 existence, password and the owner's ADMIN stay the control plane's. Opus review: Needs fixes (I-1 E32 confirm race reverting later SQL changes, I-2 rename rollback after a committed rename, 7 minors); fix round running. **E33a (user, 2026-10-08):** keys whose role gained BYPASSRLS in SQL work after the upgrade (E33's intent); the worker syncs key-role marks for every branch at start; docs note it. **Accepted:** restores made before the upgrade have no `branch_restores` link, so their chains still re-baseline.
       - **F2 DONE (2026-10-08):** fix round finished on the second machine from WIP `4483e1e` (opus): `f8efb6b` (I-2 gained the "neither name exists: keep the move, Failed" arm; M5 worker test; clippy fix). Findings and brief: `docs/superpowers/plans/2026-10-06-schema-merge-f2-review.md`. Coordinator checked `33d28ad..f8efb6b` (I-1 change-id confirm + resync on a lost confirm; I-2 catalog-decided rollback; M1, M2, M4–M7, E33a) and merged `--no-ff` (`240fa82`); merged branch: 1929 unit tests, clippy clean; `schema-merge-f2` deleted. **Watch for T15:** F2 edited migrations `20261012000003`/`04` in place, so a metadata DB that applied `33d28ad`'s versions fails the checksum; reset it (D13) before the live pass. F2 live tests for T15: `live_db` `confirm_clears_attributes_by_change_id`, `only_a_secret_key_bypasses_rls`, `rename_role_copy_moves_the_row_and_keys`, `attributes_pending_follows_api_writes`, `api_role_attributes_insert_and_update`, `confirm_and_replace_secret_and_delete_copy`, `login_on_respects_the_cap`; `rest_live` `secret_key_of_an_owner_level_role_is_refused`. Interfaces for F3: `RenameCopyOutcome::{Renamed{row,was_unreadable},NotFound}`, `rename_role_copy(…, unreadable)`, `confirm_role(…, attributes_change)`, `change_id` on `NewApiRole`/`ApiRoleChange`, `Worker.resync_requests`, `KEY_ROLE_REFUSED_GROUPS`. M3 (Studio "View as" BYPASSRLS warning) goes to F4.
       - **F3 DONE (2026-10-08):** opus implemented `2bad3a2`, `562f6f3`, `59c3b79`; opus review `2f21dae` (Needs fixes: I1 worker after-merge reset/delete skipped the hold, I2 recompute after an unread phase 1, Minors 1–11); opus fix round `c12a725` (every finding; hold invariant extended to every reset/restore/delete path, exhaustive over 16 states × source/target; 1999 unit tests, clippy, api-gen, Studio 605 pass). Coordinator checked the diff; merged `--no-ff` (`77ccee8`; the merged tree equals `c12a725` plus Status). Report, fix-round results, F4 handoff, T15 live tests and **the two pre-migration queries T15 must run** are in `docs/superpowers/plans/2026-10-06-schema-merge-f3-report.md`; review in `…-f3-review.md`. **User rulings (2026-10-08):** Q1 Abandon lists every index the merge left (D2, coordinator ruling: kinds `invalid_index`, `unattached_index`); Q2 `created_roles` kept across attempts; Q3/D1 unread phase 1 stays `failed(tx)` + note, a compute after a committed phase 1 ends `failed(online)` (`merge_half_applied:`); Q4 an empty save is a no-op; **Q5** a database/project delete is allowed while a merge holds a branch (the merge ends target-gone). `schema-merge-f3` deleted.
       - **F4 DONE (2026-10-08), merged `--no-ff` after the copy fix `2c0c5f8` (coordinator checked both diffs; `schema-merge-f4` deleted).** Sonnet implemented `216ae09`, `fef6888`, `80a4177`, `52de831`, `ef8a91b` on `schema-merge-f4` (report `docs/superpowers/plans/2026-10-06-schema-merge-f4-report.md`, with the 11 Playwright flows for T15; Studio 672 tests, docs and management-client tests pass). Sonnet review `0d48f9e` (`…-f4-review.md`): Needs fixes, I1 open-merge database match uses the display name, I2 rehearsal failures get the generic heading, I3 E29a sessions, 7 minors. **User rulings (2026-10-08): D1 the databases API exposes the main Postgres name and Studio compares exactly; D3 specific rehearsal wording (no panel when the Rehearsal section shows the step; "The rehearsal couldn't run." for create/`branch_not_ready`).** Fix round (opus) `35872f3`: every finding (route `GET /v1/databases/{id}/branches/{branch_id}/roles/{role}/sessions` `listRoleSessions`, never starts a compute; `postgres_database` on `DatabaseSummary`/`Database`; rehearsal headings; minors); 2011 unit tests, clippy, api-gen, Studio 684, docs pass; coordinator checked the route (validated name, quoted literal, compute state read first). **User ruling (2026-10-08): an apply refused back to `rehearsed` (`branch_not_ready:`, `merge_conflicts_open:`, `merge_drops_unacknowledged:`) says "The apply didn't start." plus the reason** (sonnet, small task, running). T15 adds: `roles_live role_sessions_lists_the_live_connections`; Playwright flows for live sessions on a rename step and a database whose name sanitizing changes. **E29a gap, user ruling (2026-10-08): add a sessions route** (the worker reads `pg_stat_activity` for the renamed role on the target; Studio shows the count and client addresses beside the keys; the connection-string note stays for clients not connected at that moment). It goes into F4's single fix round with the review findings, done by **opus** since it is server work. Then the coordinator checks the diff, merge; then **T15** (sonnet, no review, per the plan table): full verification, the live suites on the pinned `c8d1bb68b81f-dev` images (no image build: 6g changes no fork), the F3 report's pre-migration queries first, Playwright pass.
       - **T15 RAN (2026-10-08, sonnet; report `docs/superpowers/plans/2026-10-06-schema-merge-t15-report.md`, `54183ee`).** Pre-start: no `merges` table yet (F3 queries n/a), F2's edited migrations never applied (no checksum clash). Passed: `just test` 2011, clippy, `just check`, Studio 692 + typecheck + build, docs 14, `neon_live` 9, `catalog_live` 8, `roles_live` 42 (incl. `role_sessions_lists_the_live_connections`), `rest_live` 37, `merge-e2e`, `branch-e2e`, `data-e2e`, `app-users-e2e`, `sql-editor-e2e` (on rerun). **Playwright pass DEFERRED (user, 2026-10-08: UI revamp next);** the plan's 7 steps and F4's flows 1–14 stay listed for reuse. **Failures:** (1) `live_db confirm_clears_attributes_by_change_id` — test bug (its stale confirm passes the current version; the code follows F2's brief: change id guards `attributes_pending`, version guards `pending`); (2) `branch_live restore_a_child_branch` (`:113`, restore helper asserts no live children); (3) `merge_live keep_reset_delete_after_merge` — stack overflow in debug on the reset-after-merge path (large async future; passes with a 32 MB stack); (4) `merge_live recovery_reads_the_marker` (`:2338`) — racy poll; (5) `merge_live a_keep_merge_base_behind_gc_is_unavailable` (`:2565`) — dev's GC cutoff pinned at the base point for 300 s; (6) `sql-editor-e2e` first run: `tuple concurrently updated` in the data API bootstrap. **User (2026-10-08): investigate first, then decide.** Opus diagnosis `b7a65f8` (`…-t15-diagnosis.md`): 2 test bug (restore of a branch with live children is refused by design; `before_dev` was past the branch point); 5 test bug (GC cutoff never passes `disk_consistent_lsn`; `fast_gc` needs `checkpoint_timeout: 1s`); 6 product race, `main` too (compute_ctl applies its migrations after reporting running; migration 0002 `ALTER ROLE … NOBYPASSRLS` collides with the bootstrap's `ALTER ROLE authenticator`). **User ruling (2026-10-08): fix 1–5 now; 6 parked for the next Neon fork round (roadmap entry on `main`), no fork build in 6g and no worker retry.** Fix round (sonnet) `180d7c2` + report section `8386e23`, coordinator checked the diff: 1, 2, 4 test fixes; 5 `checkpoint_timeout: 1s` in `fast_gc`; 3 product fix: the merge job path's large async fns return `BoxFuture` over `*_inner` fns (debug frames summed ~1.8 MB on the reset path), unit test `reset_after_merge_fits_a_small_stack` (448 KiB thread). Re-run: `cargo test --workspace` 2012, clippy clean, `live_db` 184/184, `branch_live` 4/4, `merge_live` 25/26, `merge-e2e`, `branch-e2e` pass. **New flake:** `merge_live second_merge_carries_only_newer_changes` failed once at provision ("provision failed" after 6.8 s) in the whole-suite run, passed alone; cause not captured (in-process worker log not kept), possibly the parked failure 6. **Re-run (2026-10-08, coordinator, logs kept): `merge_live` whole 26/26 in 641 s, no provision failure, no `tuple concurrently updated`; the flake did not recur.** **END-OF-STEP PAUSE (2026-10-08): waiting for the user's go to merge `schema-merge` into `main`.** Not run (outside the brief): worker `app_users_live`, `neon_live`, orchestrator `sql_*_live`, `chelabase-query`/`chelabase-neon` live. Test data left: demo-project databases `live-ac34b7b7`, `live-efb7490c`, maybe a failed `e9a70227-…`; Studio account `t15-verify@example.com`. **Stack: running from `schema-merge`;** the dev metadata DB now has 6g's migrations, so main's stack refuses to start until 6g merges (D13); leave it here until then.
       - **For F3 (collected):** wire `follows_rename` / `compare::with_rehearsed` into the rehearsal and finish; use `Step::needs_acknowledgement()` in the worker (`merge/mod.rs` `unacknowledged`) and orchestrator (`merges.rs`); execute `RoleOp::Rename` via `Worker::rename_role`; map phase 1 failures with `Phase1Script::at_line`; the worker's `plan_options` duplicates `plan::lock_timeout_ms`; the orchestrator's step-id check must refuse `:`/`.`/control chars; check the roles path doesn't treat `member_of` as the exact set (it would revoke `pg_*` memberships F1 now leaves out of patches); phase 0 pending-row rule and rehearsal membership check per F2's interfaces.
  1. **Merge `main` into `schema-merge`** (done, item 0). A trial merge (`git merge-tree`) conflicts in 6 files: `crates/chelabase-metadata/src/neon.rs`, `crates/chelabase-metadata/tests/live_db.rs`, `crates/chelabase-nats/src/lib.rs` (both sides add subjects: 6g's `SUBJECT_MERGE`/`MergeJob`, main's `SUBJECT_RESYNC_BRANCH`/`ResyncBranchRequest` and `CheckDataRoleReply.role_oid`; keep both), `services/worker/src/lib.rs`, `services/worker/src/main.rs`, `services/worker/src/resync.rs` (6g's T9 resync line format and U1/U2 attributes vs main's `ResyncPort`, the delayed per-branch resync on `chelabase.resync.branch`, and `fix_key_roles`). Semantic conflicts the textual merge doesn't flag:
     - (a) `Storage` is no longer implemented on `StorconClient`: main has `RealStorage { storcon, safekeepers }` (D11) with `Placement` on it too. 6g's `Storage::gc_cutoff` (`ports.rs`, reads `applied_gc_cutoff_lsn`) moves into `impl Storage for RealStorage` and calls `self.storcon.get_timeline`. Every test that called `worker.storage.delete_tenant` now uses `worker.storage.storcon.delete_tenant`.
     - (b) `Storage::get_timeline`'s head is now the max of the pageserver's `last_record_lsn` and the safekeepers' `commit_lsn`. 6g's `merge/finish.rs` `head_of` then records bases at the committed head (correct); anything polling the head to wait for ingest returns at once.
     - (c) The DDL receiver (`services/orchestrator/src/internal/ddl.rs`) has new rules: payloads are tracked by role OID; `recreated` entries run DROP's live-key check (409); the role cap is checked after the whole delta; a rename moves the keys row (or there is no row); `VALID UNTIL` and other attributes and `GRANT` are forwarded as `touched`, which triggers the delayed resync. Reconcile with 6g's U1/U2 (NOLOGIN roles, memberships, attribute updates, `20261010000002_role_attributes.sql`): a merge's phase 0 role changes go through the roles path, so check they don't trip the receiver's recreated/cap rules, and that the resync doesn't undo attributes 6g stores.
     - (d) **Secret keys are bound to their role's OID** (D24, `20261011000001_secret_key_role_oids`): `rest` only switches to the role if the name still has that OID; the resync revokes keys whose OID is gone. A merge that drops and re-creates a role (or a role created on the target by phase 0) gets a new OID, so keys on the target bound to the old OID are revoked. Check the merge plan never drop-and-creates a role that has live keys without saying so (a conflict or a drop acknowledgement).
     - (e) **Migration order:** main's `20261011000001` sorts after 6g's `20261010000001`–`04`. The shared dev metadata DB currently has main's migrations; when `schema-merge` starts the stack it applies its four older ones. Confirm sqlx applies those (out of order) without refusing, before any live run; otherwise reset the dev DB (D13's procedure) with the user's go.
     - Then `cargo build --workspace --all-targets`, `cargo test --workspace`, clippy `--workspace --all-targets`, `just api-gen` and the openapi drift tests.
  2. **Dev metadata DB (D13).** After the merge, main's stack can't start on a DB where 6g's migrations are applied ("previously applied but is missing") until 6g merges or the DB is reset. The migrator stays strict.
  3. **Restore: one detach at any depth.** Discard `schema-merge-t13b` (`d8ea1fa`, held; remove its worktree and branch, local and origin) under ruling R4. Fork patches 1 and 3 make a single detach wait for the ancestors' WAL and copy every level, so restore returns to its one-detach flow: no double copy, no `wait_for_catch_up`, no two-detach recovery states. Keep T13b's live test `restore_a_child_branch` (it exists only on `schema-merge-t13b`'s `branch_live.rs`; port it to `schema-merge`) and add `restore_a_grandchild_branch`; `merge_live` `restore_before_a_keep_merge_uses_the_older_base` is already on `schema-merge` and must pass on the one-detach flow. The detach's waits are sequential (up to 30 s × depth, bounded by `STUCK_DETACH_SECS`).
  4. **Strike E24, E25 and the grandchild refusal** from this plan ("superseded by Neon fork round 2").
  5. **E17 narrows:** patch 4 shipped (reads at retained LSNs below the GC cutoff, with a lease keyed at the exact LSN), so the live fallback reads any branch point the pageserver retains (a child's branch point is retained while the child exists). Re-baseline is left only for a truly unavailable base (no stored snapshot and the point not retained). Update the spec's E17 text and the docs' merge page.
  6. **Role-lag notes:** patch 5 shipped, so the docs' "role changes lag" notes are gone (round 2's T13e removed them from `apps/docs`); make sure 6g's own docs (T12's merge page) don't add any back.
  7. **Then T14 (end-of-step review) and T15 (verification) as planned,** with the T14 items listed below.

  Earlier status (2026-10-07, kept for history; the list above supersedes its "On resume" items): **PAUSED for Neon fork round 2** (the roadmap's "Resume here" holds its brainstorm state, R1–R5). Nothing was running. T1–T13 (with T7b, T8a, T8b, T13c) are merged. **T13b is held, not merged** (`schema-merge-t13b` at `d8ea1fa`, worktree kept). It will be discarded under fork-round ruling R4: restore returns to one detach, which works at any depth once fork patches 1, 3 and 6 land, and its live test `restore_a_child_branch` plus a new grandchild test are kept. E24, E25 and the grandchild refusal come out with it. Its review (opus) said "Needs fixes": I1, the first detach's lag window and the same race between the wait and the second detach; I2, grandchild restores still hit `AncestorNotEmpty`. Both are fixed by the fork round. **On resume, after the fork ships:**
  1. **Step 0: dev metadata DB.** D13 (fork round 2): the DB was reset 2026-10-07 (6g's migrations `20261010000001`–`04` had blocked this branch). Schema-merge's own migrations work; 6g's re-apply when its worktree starts the stack. From then on, main's stack (or another branch's) refuses to start with "previously applied but is missing" until 6g merges or the DB is reset again. Record this in the roadmap (the entry was linked in fork round 2's T10b).
  2. **Merge `main` into `schema-merge`.** Conflicts beyond `NEON_REF`, compose digests and `infra/neon/README.md`: (a) `services/worker/src/ports.rs`: `impl Storage for StorconClient` became `RealStorage { storcon, safekeepers }` (D11 adds safekeepers field); `RealWorker`'s type changed in `config.rs`. (b) Fork round's D11 adds `TimelineInfo::safekeeper_commit_lsn` to the Neon crate; 6g's `services/worker/src/ports.rs` `get_timeline` path that uses `applied_gc_cutoff_lsn` must move into `RealStorage` and call `self.storcon`. (c) `services/rest/tests/rest_live.rs:115` and `tests/common/mod.rs` must use `worker.storage.storcon.delete_tenant` (D11: `Storage` is no longer `StorconClient`). (d) `FakeTimeline` gained `commit` (D11 in live tests); `Storage::get_timeline` is now commit-aware (returns max LSN from safekeepers). For tests: 6g's `merge/finish.rs` `head_of` records bases at the safekeeper head—fine; anything polling `get_timeline` to wait for ingest (like T13b's `wait_for_catch_up`) now returns at once.
  3. **Restore.** Becomes one detach with fork's multi-level behavior at any depth (no double copy, no `wait_for_catch_up`, no two-detach recovery states); add live `restore_a_grandchild_branch`. The detach wait can return 503 after up to 30 s × depth in the retry path, bounded by `STUCK_DETACH_SECS`.
  4. **Strike E24, E25 and the grandchild refusal from the plan** ("superseded by fork round 2").
  5. **E17 stays** as decided (re-baseline outside the history window when the stored snapshot is missing), since patch 4 is parked.
  6. **Patch 6 is D11** (fork round 2): worker reads safekeepers' `commit_lsn` directly over HTTP; no pageserver field. Also from round 2 (D18): patch 5 changes `services/orchestrator/src/internal/ddl.rs` (forwarded `login`, `touched` → `chelabase.resync.branch` resync), which overlaps 6g's role-attribute work (U1/U2, `20261010000002_role_attributes.sql`): reconcile on merge; and patch 4 (reads at retained LSNs below the GC cutoff) ships, so E17's re-baseline is only needed when the branch point isn't retained. T13b concern (2) (a branch "at head" right after a reload may miss writes) is addressed by D11's worker change (the worker's safekeepers read, not a fork patch), covered by unit tests only until round 2's reworked live test passes; (3)–(5) stay for T14.
  7. **Then T14 and T15 as planned.** (The role-lag notes stay, since patch 5 is parked.)
  
  The dev stack runs from `schema-merge`. **Items:**
  1. **E15. P2 answered by the user (2026-10-06): (A) an owner session for all merge SQL.** Phases 0–2, user steps and own SQL run as the owner (`OWNER_ROLE` = `postgres`); only catalog reads, the digest and the marker setup stay on `cloud_admin`.
     - **Transport (ruling, mechanical):** the same docker exec psql, with `-U postgres` over the compute's local socket. The compute's `pg_hba` trusts local connections, so no password is needed, and there's no refusal case for an unsealed owner password. Checked on the stack: in that session `set_config('role','cloud_admin',false)` fails with `permission denied to set role "cloud_admin"`. *Cost if wrong: a compute image whose pg_hba drops local trust would need the sealed-password path (tokio-postgres) instead.*
     - **T6 follow-up, owned by T8a:** `SqlOpts` gains `user: Option<String>` (None = `cloud_admin`), passed as psql `-U`.
     - **T4:** the scripts drop `SET ROLE postgres` / `RESET ROLE`. The phase 1 marker insert runs as the owner, inside the phase 1 transaction. A separate `marker_setup_sql()` runs on `cloud_admin` and carries the Critical 1 hardening. It creates `chelabase_merge.applied` with `GRANT USAGE` on the schema and `GRANT SELECT, INSERT` on the table to the owner (no UPDATE/DELETE). *Cost: the owner could forge a marker, which only harms their own merge, and the drift check after apply would flag it.*
  2. **T2** (catalog reader, opus) `c4b6763` (base `eff028e`): **sonnet review Approved; merged into `schema-merge`.** 29 unit tests and 4/4 live tests passed; re-run the 4 live tests once from `schema-merge` (they ran from a base without T6's migration).
     - **E16 (user, 2026-10-06): exclude it, and document it well.** The catalog SQL drops the default-ACL row only when it is exactly the owner's global (`defaclnamespace = 0`, `defaclobjtype = 'f'`) EXECUTE revoke from PUBLIC; a test pins it, and `exclusions.rs` names it. Done in the T14 fix batch (sonnet). **T12** documents, on the merge docs page, what merges ignore (the data API's default privileges, the users table's managed parts, Chelabase's own schemas and roles, extension members) and why. *Cost: a user who writes the same row by hand won't see it merged.*
     - Was open: the data API's global `FOR ROLE postgres REVOKE EXECUTE ON FUNCTIONS FROM PUBLIC` default-ACL row (written at `services/worker/src/data_api.rs` ~394, ~1347) is kept in the model, so a branch with the data API on vs one with it off shows a spurious difference. Option: exclude it when `defaclnamespace = 0`, `defaclobjtype = 'f'` and the ACL is exactly the owner's EXECUTE.
     - **T2 review minors (deferred list):** the anon/authenticated default-ACL filter applies to any owner role, not only `FOR ROLE postgres` (a user's own `FOR ROLE other … TO authenticated` row is hidden); an unparseable index definition fails the whole read without naming the index; `INHERITS` children lose their parent link silently (report `Other("inherits")`?); fixture assertion gaps (schema grant, enum label OIDs distinct, the nn check absent from `constraints`); `two_reads_are_identical` only proves determinism in one session; T3 must not treat the cluster-wide `pg_roles` entries as user changes (same on every branch).
     - Earlier notes:
     - **RED:** shown for the unit tests. The live RED against the stub never ran: the live queue's dev-stack swap from t2 failed on the missing T6 migration (E13).
     - **Not reviewed yet: dispatch its review** (sonnet; report `.superpowers/sdd/2026-10-06-schema-merge/task-2-report.md`). Before any further live run from t2, merge `schema-merge` into it (E13).
     - **T2 rulings to list for the user:**
       - the users table's constraints that cover only managed columns are excluded;
       - only table-level grants on the users table to `authenticated` / `chelabase_auth_sync` are excluded, column grants stay;
       - the data API's global "REVOKE EXECUTE ON FUNCTIONS FROM PUBLIC" default-privilege row is kept, its per-schema anon/authenticated rows are excluded;
       - extensions themselves are reported, and only their members are excluded.
     - **Plan corrections:**
       - the SQL is in `src/catalog_sql.sql` (`include_str!`), with `SET LOCAL DateStyle/TimeZone` and a DO block that finds the users table through `set_config`;
       - `acldefault` for sequences is `'s'`;
       - a policy `TO role, PUBLIC` is stored as `{0}`;
       - no model types changed.
  3. **T5** (metadata, sonnet) `f20c1bc` plus `f081157` (E13 merge): **sonnet review Approved; merged into `schema-merge`.** The live_db suite passed with 146, including the 12 new tests.
     - **T5 review notes passed on:** to T10: make save-and-move-to-computing one statement, map `create_merge`'s `MetadataError::Conflict` race to a retryable 409, the `large_enum_variant` clippy warning on `MergeCreate`. To T8a/T8b: `set_merge_status` has no version check, so workers read draft inputs after their claim transition; entering `applying` for a second merge hits the `merges_one_applying` unique index as `Err` (pre-check or map it, or the job redelivers forever).
     - **T5 review minors (deferred list):** no unit test for `BaseSource` round trips (live only); `merge_events.kind`/`outcome` have no CHECK (consistent with other event tables, no action).
     - Notes from T5:
     - the openapi drift tests fail until T10 runs `just api-gen`, as expected since `ErrorCode` changed;
     - sqlx `migrate!` isn't re-embedded when only a migration file is added, so touch `crates/chelabase-metadata/src/lib.rs`;
     - **P14 correction:** hide rehearsals in the orchestrator's list route, not in metadata's `list_branches` (the worker needs every branch);
     - `create_rehearsal_branch`'s `created_by` is unused.
  4. **T9** (roles, opus) `d200ab5`, fix round 1 `01990da` (merge of `schema-merge`) + `168128d`: **sonnet re-review Approved; merged into `schema-merge`** (conflict in `api_error.rs` resolved by hand: T5 already carried the two role codes). Live from `schema-merge`: `roles_live nologin` passed, `live_db` 148/148 passed; T2's `catalog_live` 4/4 passed. Worker 343 and orchestrator API 250 unit tests pass; only the expected openapi drift tests fail until T10's `just api-gen`.
     - **F1-a (user, 2026-10-06): keep it.** The membership check also refuses targets that reach `cloud_admin` or the owner at any depth, so a superuser group and a group owning the database are refused too. *Cost: owner-level only through the explicit attribute.*
     - **Running worker live tests** needs the dev env: `just live bash -c 'set -a; source .chelabase/zitadel/demo.env; eval "$(scripts/neon-images.sh)"; set +a; CHELABASE_COMPUTE_IMAGE="$COMPUTE_NODE_IMAGE" cargo test -p worker --test <name> -- --ignored --test-threads=1 <filter>'`.
     - Review findings (fixed in round 1):
     - **Important 1:** PATCH reaches roles the user doesn't administer. For a role without an `api` row, require `owner_admin`, otherwise `not_found` / `reserved_role_name`. Use a prefix-aware reserved check (`chelabase_*`) on the PATCH target in both worker and orchestrator. Add a test with a non-admin role made in SQL.
     - **Important 2:** a membership in a group can reach reserved roles or `neon_superuser` indirectly. `membership_check_sql` must refuse targets that are members, at any depth (`pg_has_role(…,'MEMBER')`), of `neon_superuser` / `authenticator` / `anon` / `authenticated` / `chelabase_%` / `pg_%` (`role_membership_not_allowed`). Add a test.
     - **Minors** (to the deferred list):
       - the first resync after upgrade revokes SQL-made grants and attributes on API roles (tell the user); T8a must change API roles only through `update_role`;
       - the partial strip of an escalated plain API role (only BYPASSRLS);
       - a dropped group re-granted by an unrelated PATCH;
       - psql stderr (it could carry a verifier) in the `Failed` message on the SQL-role path.
  5. **T4** (planner, opus) `18e150e`, fix round 1 `a8dc942` (merge) + `1b02837`, fix round 2 `51c28d3`: **opus re-review Approved; merged into `schema-merge`** (125 unit tests). Round 2 fixed: same-anchor user steps escaping the phase-order refusal; the digest hijackable through `||` operator resolution on the admin session (now `OPERATOR(pg_catalog.||)` plus `DIGEST_PRELUDE`); the marker check re-run after its CREATEs. **T8a: the `cloud_admin` session reads the digest only with `plan::admin_digest_sql()`** (psql `-q -A -t`), never bare `TARGET_DIGEST_SQL`; the phase 1 guard (owner session) is unchanged. **Important 2 ruling:** a user statement placed after a later-phase statement is refused (`SchemaError::Steps`), never moved, since a moved phase 1 statement loses the transaction. T8a/T13 should cover a pre-created `chelabase_merge` schema live. Re-review deferred minors: the generated-column conversion is excluded from the policy detach; a valid phase 1a placement refused before the used-enum move; policy comments lost on detach and re-create. T8a notes from round 1: run `plan::marker_setup_sql()` as `cloud_admin` (idempotent) before phase 1 on rehearsal and target; run phases 1a/1/2 and user/own-SQL steps with `SqlOpts.user = Some(plan::OWNER_ROLE)`; `OnlineCheck::IndexValid { schema, table, name }` also checks `indrelid`; temp index names are `chelabase_merge_ix_<n>_<32 hex merge id>`; `plan::MARKER_SQL` is gone (`marker_sql(id)` is the INSERT only); new `SchemaError::Steps` case for a statement placed after a later-phase one. Original findings:**
     - **Critical 1:** the marker runs as superuser on objects the tenant can pre-create. After `RESET ROLE`: `SET LOCAL search_path = pg_catalog, pg_temp`; assert in a DO block that any existing `chelabase_merge` schema and `applied` table are owned by `session_user`, are a plain table and have no triggers or rules; fully qualify `pg_catalog.now()` and the types; add a test.
     - **Important 1:** user-step escalation, the P2 decision above.
     - **Important 2:** a phase 1 user step anchored after a phase 2 step runs before its anchor. Run it in phase 2 as a single statement, or refuse it, with a test.
     - **Important 3:** false drift when a role is left out of the merged model. `expected_model` keeps the target's roles, with a test.
     - **Important 4:** the attnum rule for new columns isn't pinned. **Ruling to record:** T3 already numbers the source's new columns after the target's last attnum (T3 contract); T4 must rely on that and never pair a new column with a dropped target column by attnum; test both.
     - **Important 5:** a leftover `chelabase_merge_ix_<n>` can be renamed into place. Make temp names unique per merge (a merge-id suffix) and check `indrelid` in `IndexValid`.
     - **Important 6:** a column type change under a policy fails. Detach and re-create the policies that use the column, with a test.
     - **Minors** (deferred list):
       - no `lock_timeout` in phases 1a and 2 (ask the user);
       - user phase 2 statements and user `ADD VALUE` steps aren't resumable;
       - phase 1a misses an add after a value is renamed away;
       - backslash labels;
       - false drift from a role's comment or `other_attributes`;
       - owner-level isn't flagged `security` (ask the user);
       - the `rewrite` flag misses volatile defaults;
       - document the nn-check attnum;
       - pin that T2 returns collation and INCLUDE already quoted;
       - parse the guard's PL/pgSQL body in a test;
       - T8 reads `default_acl` before the apply, and phase 0 mustn't change ACLs;
       - dependency gaps the rehearsal catches (a changed unique under an unchanged FK, generated-column conversion under policies, invalid target indexes).

     **T4 plan corrections accepted** (record as E14): `compare(&[ObjKey])` with `own_sql_keys`; `OnlineCheck::ConstraintExists`; `SchemaError::Unplannable`; `marker_sql(Uuid)`, `step_script`, `STALE_ERROR`. The digest leaves out roles and enum values, because phases 0 and 1a change them before the guard: T8 runs a full pre-check before phase 0. T13 plans from a fresh database's own model (it already has `public`).
  6. **Dev stack:** fixed. Restarted from `schema-merge` with `just live just dev` after T9 merged; the pending live tests passed (item 4).
  7. **T7** (base snapshots, opus) `aad2a60`: **sonnet review Approved; merged into `schema-merge`**, plus `step 6g T7: test database lines…` (a T7 test fake still used the pre-T9 resync line format). Worker 358 unit tests pass; live `static_endpoint_is_unroutable_and_reconcile_sees_its_age` passed.
     - **T7 plan corrections (accepted):**
       - metadata gained `NewStaticEndpoint` / `create_static_endpoint`;
       - `EndpointState` gained `routable`, `static_lsn` and `age_secs`;
       - storcon `TimelineInfo` gained `applied_gc_cutoff_lsn`;
       - the GC horizon is pre-checked from the cutoff before starting a compute, with the error text kept as a backstop;
       - a failed base read stores the location without the model;
       - reconcile's `finish_switch` records the reset base;
       - a branch with no base row is read at its own timeline and branch LSN;
       - `load_base` is `#[allow(dead_code)]` until T8a.
     - **T7 rulings:**
       - the base location is the branch's own timeline at its branch-point LSN (the reviewer confirmed this is correct for Neon);
       - a stored identity map with pairs is kept on re-read;
       - an undeletable static compute row is left for reconcile;
       - reconcile treats an endpoint that is unlinked, unroutable and has a `static_lsn` as a static compute's.
     - **E17 (user, 2026-10-06): re-baseline outside the history window.** The pageserver refuses reads below the applied GC cutoff without an LSN lease, so the live fallback only works within the history window. Outside it, the merge offers re-baseline and says why. The spec's retention sentence is corrected. *Cost: on such branches, renames show as drop plus create, and main's own changes show as undos (two-way diff), guarded by drop acknowledgements.*
     - **E18 (user, 2026-10-06): bases are a history** (new task **T7b**, below). A restore picks the newest base at or before the restore point. The spec's restore line is amended. *Cost: about 10–25 KB per entry per database (measured: our metadata DB's model is 99 KB raw, 6.6 KB gzipped).*
     - **T7 review minors (deferred list):**
       - `wait_until_ready` waits the whole timeout before a non-GC compute failure;
       - a cancelled `load_base` future skips cleanup until reconcile; tie `STALE_STATIC_COMPUTE_SECS` to create + ready + read timeouts with a const assertion or a comment;
       - the long `use` line in `branch_create.rs`;
       - nothing tests that the create-time read and the live fallback produce equal models (T13);
       - T13 must cover a static compute booting with no roles or databases, `cloud_admin` access, and `CATALOG_SQL` on a hot standby.
       - Minor 3 (a stale database row after reset) moved into T7b.
     - **Pre-existing clippy `-D warnings` failures:**
       - `chelabase-metadata/src/merges.rs` `large_enum_variant`: T10 adds the allow;
       - `chelabase-neon/src/backend.rs` ~932 `cloned_ref_to_slice_refs` (T6): T8a fixes it, since it edits that file.
  8. **T10** (merge API, sonnet) `5112231`, then `2918082` (merge of `schema-merge`, generated files regenerated): orchestrator 74 + 274 + 90 pass, registry 27, client 19, Studio typecheck clean. **Sonnet review Approved; merged into `schema-merge`**, then `0fd90b9` (the live test expected `Stale` where the status guard gives `NotEditable(Computing)`; test fix only). Live `saving_with_start_compute…` passed. Openapi drift tests pass on `schema-merge`. **T10 review minors (deferred list):** reverts after a publish failure swallow errors (`let _ =`; log at `error!`); rehearse and apply don't check that both branches are `Ready` (refuse with `branch_not_ready`); cancel from `rehearsing` frees the branch before the rehearsal is torn down (T8b teardown must tolerate it); the cross-database 404 is tested only for `getMerge`.
     - **T10 corrections:**
       - `MergeStatusPatch.options` (recompute stores `options.rebaseline`);
       - `MetadataError::is_unique_violation` (unique races give 409);
       - `MergeDraftPatch.start_compute` (save and compute in one UPDATE);
       - rehearsals hidden in `load_branch` (404) and the list route;
       - the create race gives 409 `merge_busy` with `Retry-After: 1`;
       - the `fetchMergePlanSql` client helper.
     - **T8a/T8b must expect the row already in its working status** (`computing`, `rehearsing`, `applying`, `online`, `cancelled`): the API sets it before publishing.
     - **Known narrow races:** `rehearseMerge` can rehearse a newer draft than the one it checked; a concurrent recompute's `options.rebaseline` can be lost by a draft save.
  9. **T12** (docs, sonnet) `c11def9`: **merged into `schema-merge`**; no review of its own, so the end-of-step review covers it. Docs typecheck, build and tests (14) pass.
     - **For T14 to re-check once T8a/T8b land** (these docs claims rest on the plan, not on code):
       - `merge_base_unavailable` leaves the merge in `draft`;
       - the rehearsal details: the name, unroutable, NOLOGIN phase 0, timings only, a failure returning to `draft`;
       - phase 0 goes through the roles path; `lock_timeout` defaults to 5 s; cancel uses `application_name`;
       - keep, reset and delete after the merge, a delete refused by children going into `result`;
       - `result.created_roles`;
       - `applied_with_drift` and the identity map;
       - recovery from the marker, and the reconcile rules;
       - resolutions are kept on recompute only if their conflict is unchanged.
     - **T12 notes:**
       - the docs follow the code where the spec differs: `chelabase_merge.applied`, `merge_confirm_required`, the SQL digest guard;
       - E16's exclusion is documented as the intended behaviour (it lands in T14);
       - **deferred minor:** `openapi/management.json`'s `updateRole` description says PATCH works on roles "made through the API and in SQL", which contradicts its 404 text; fix the utoipa description in the orchestrator.
     - Studio isn't documented yet; T14 adds a short Studio section once T11 merges.
  10. **T7b** (base history, opus) `5573ca0`: **sonnet review Approved; merged into `schema-merge`**. Worker 364 unit tests pass. Live: the full `live_db` suite passed 153/153 from `schema-merge`, so migration `20261010000004` is now applied to the shared metadata DB. Before review, the suite was also run against a scratch database, with `CHELABASE_METADATA_DATABASE_URL=…/t7b_scratch` and `cargo --manifest-path <task wt>` from `schema-merge`'s `just live`. **That's a way to live-test a task's metadata SQL without touching the shared DB.**
     - **T7b plan correction:** the restore copy happens inside `switch_to_restored_timeline`, which now takes `restore_lsn`, in the same transaction. That covers fresh, resumed and reconcile-finished restores. On resume, the pending timeline's ancestor LSN is read before the detach.
     - **T7b rulings:**
       - R1: a copy keeps the original's `taken_at`;
       - R2: `load_base` returns `Unavailable` when the newest entry sits on another timeline than the branch's current one, which gives re-baseline;
       - R3: a database counts as missing only when the error names it;
       - R4: pruning runs after every base write;
       - R5: the restore copies every database with an entry.
     - **Metadata API changes:**
       - `append_schema_base` (replacing `upsert_schema_base`), `update_schema_base` (replacing `set_schema_base_model`);
       - new `schema_base_at` and `prune_schema_bases`;
       - `record_merge_base(…)` for T8b.
     - **T7b deferred minors:**
       - **(fix in T14)** a second restore to a point before the first restore's point finds no entry, because only the immediately previous timeline is searched; walk the ancestor chain;
       - the history is unbounded, and each restore duplicates a full model;
       - a copy's live location still points at the old (backup) timeline, so after the backup is deleted, a copy with no usable model falls back to re-baseline (document it);
       - the re-baseline UI wording mustn't blame a restore.
  11. **T8a** (merge worker core, opus) `e6891ec`, plus `dd58d7f` (merge of `schema-merge` with T7b): worker 385, neon 59 and metadata 15 unit tests pass; clippy is clean, including the pre-existing `backend.rs` lint. **Opus review running.**
     - **Live test pending:** `live_db status_patch_writes_resolutions_and_null_clears`.
     - **T8a plan corrections:**
       - `run_phases(target, plan, opts, digest, mode)` takes the E4 digest, and `run_online` was added for resume and retry;
       - the files are `merge/phases.rs` and `merge/testkit.rs`; `branch_create.rs` is left to T8b;
       - the plan JSON is `StoredPlan { steps, target_digest }`;
       - the merge timeouts are constants (16 min and 6 h) on `Worker`, with no env vars;
       - `MergeStatusPatch.resolutions`, and `Some(Null)` clears `analysis` and `plan`.
     - **Interface for T8b:** `RunMode`, `Target`, `PhaseReport`, `StepRun`, `StepOutcome`, `PhaseFailure`, `FailureKind`, `PHASE1_STEP`, `phase_name`, `app_name`, `claim_merge`, `keep_merge_claimed`, `plan_options`.
     - **T8a concerns:**
       - the owner session uses `psql -h localhost -U postgres` (TCP; pg_hba trusts 127.0.0.1) and hasn't run on a real compute yet (T13);
       - rehearsal roles are NOLOGIN, so the drift compare must expect that;
       - a role whose metadata row was stored but whose SQL failed is skipped on redelivery;
       - T9's `exists_sql` runs on `cloud_admin` without pinning `search_path` (deferred minor);
       - Rehearse, Apply, Resume and Cancel are only logged and acked until T8b;
       - T8b must map the `merges_one_applying` error to a status.
     - **T8a opus review: "Needs fixes"; fix round 1 `76e1688` and fix round 2 `66be17c` (the unique `USING INDEX` step was mislabelled SHARE UPDATE EXCLUSIVE; every online lock label is now pinned by `online_lock_labels_match_postgres_17`); opus re-review Approved; merged into `schema-merge`.** Worker 388, schema 129, neon 59, orchestrator 274+90+74 unit tests pass; live `status_patch_writes_resolutions_and_null_clears` passed. Owner session: `psql -h 127.0.0.1 -U postgres`. It covers four findings:
       - I1, which becomes E19;
       - I2: a failed compute cleared `analysis`, so every resolution was lost on recompute;
       - I3, security, pre-existing in T9: `exists_sql` on `cloud_admin` had no `search_path` pin, so an owner-made `public.=` operator ran as superuser. Every `cloud_admin` site gets pinned;
       - M4: use the local socket or `127.0.0.1`, not `localhost`.
     - **E19 (user, 2026-10-06): phase 2 locking steps use the merge's `lock_timeout`, with manual retry.** This covers steps taking ACCESS EXCLUSIVE or SHARE ROW EXCLUSIVE (`SET NOT NULL`, `DROP CONSTRAINT`, the online FK) and phase 1a's `ADD VALUE`. On a timeout the merge fails (online or enum_add), and Retry resumes from that step. Concurrent index builds and `VALIDATE` keep no timeout. It also settles T4's deferred "no lock_timeout in phases 1a and 2" minor. *Cost: a busy table may need a few retries.*
     - **T8a review minors (deferred list):**
       - M1: a pending API role row is skipped in phase 0. Skip only when `!pending`; otherwise check that the role exists;
       - M2: a failure at the marker insert or COMMIT is blamed on the last tx step, and a user step's `-- step <id>` line can steer the mapping;
       - M3: Stale and TimedOut are classified by substring and can be spoofed by `RAISE`;
       - M6: the rehearsal's phase 0 skips the membership and administer checks;
       - M7: a resolution refused with `InvalidChoice` disappears without a reason;
       - M9: test gaps (a 1a failure maps to `EnumAdd`, marker-setup failure, the `ConstraintValidated` / `NotNullSet` skips).
     - **To T8b:**
       - M5: run `cancel_sql(merge_id)` once before `run_online` on resume;
       - M8: re-check the drop and BYPASSRLS acknowledgements when the apply starts;
       - expect NOLOGIN on rehearsal roles in the drift compare.
  12. **T11** (Studio, opus) `992657c` (a merge of `origin/main`: `schema-merge` lacked the app shell) and `03bfc5f`. Studio tests 598, typecheck and build pass. **Sonnet review "Needs fixes"; fix round 1 `1979f4b` (merge) + `f0ec19d` (Studio tests 605); sonnet re-review Approved; merged into `schema-merge`.** U3 is now a link to `/app/projects/$projectId?database=&branch=&panel=connection&role=`, and the project page opens the existing Connection panel (validated search params, `lib/project-search.ts`). T15 Playwright additions: follow a created role's link, and protect main after loading the merge page, then Apply.
     - **The three findings:**
       - re-baseline dead-ends when the merge is `failed`;
       - the protected confirm has no path when the branch list is stale;
       - **U3:** the merge page revealed the role password. It must link to the Connection panel instead, through a new project-route search param.
     - **Coordination:** `main` merged into `schema-merge`.
     - **T11 rulings for the user** (in `task-11-report.md`, to show at the step-end pause):
       - R1: the route file is `app.projects.$projectId_.databases…`, a non-nesting route, since the project route has no `<Outlet/>`;
       - R3: the three panes show indented JSON;
       - R4: own steps are a plain textarea;
       - R8: a save button per section;
       - R9: the after-merge choice is shown only for merges into the parent;
       - R10: an inline confirm only for a protected target;
       - R11: Apply is the only sand button;
       - R13: the re-baseline wording;
       - R15: the role Attributes column shows only non-default values.
     - **T11 review minors (deferred list):**
       - focusable `<pre>` blocks have no accessible name;
       - acknowledging on a rehearsed merge sends it back to draft, so tell users to acknowledge before rehearsing;
       - the apply progress counts must exclude rehearsal events;
       - unsaved edits vanish on a version conflict;
       - two sand buttons show for a moment during the protected confirm.
     - **Shapes Studio reads**, passed to T8b: `rehearsal`, `result`, `merge.error` step lines.
  13. **T8b** (merge worker jobs, opus) `a544662`: worker 423, schema 130 pass; clippy clean. **Opus review "Needs fixes"; fix round 1 `8e30b90` (I1 pre-check, E20, E21 `finishMerge` + `MergeJob::Finish`, M1, M2), fix round 2 `d4cfc5b` (`precheck_fingerprint` projects out the plan's own roles and enum labels so a retry isn't falsely stale); opus re-review Approved; merged into `schema-merge`.** Workspace unit tests pass. Re-review deferred minors: the finish "only once" race (guard the write on `result`); a lost Finish job leaves `requested` stuck (re-publish or reconcile); a crash mid-finish re-runs reset; the projection hides changes to a planned role and every label of a touched enum (finish drift catches them; tighten to the added labels only); an unparseable 1a statement projects out every enum (make it stale-leaning). A second `finishMerge` is 409 `conflict` (no `unprocessable` code exists).
     - **The review's Important finding (I1):** the apply had no pre-check before phase 0, so a stale merge still created roles and enum values; the fix adds the fingerprint and digest pre-check. The fix round also takes M1 (recovery fails closed while backends run) and M2 (narrow the marker "does not exist" match), because both can leave main half-merged.
     - **E20 (user, 2026-10-06): Retry re-runs the apply from the start after a failure in `roles`, `enum_add` or `tx`.** That moves the merge `failed → applying` and publishes `Apply`, with every pre-check. `online` keeps its resume path. *Cost: a user `ADD VALUE` step without IF NOT EXISTS fails again, and needs a recompute.*
     - **E21 (user, 2026-10-06): with drift, the after-merge choice is skipped, and the user runs it by hand.** New route `finishMerge` (`POST …/merges/{id}/finish {after}`), allowed once from `applied_with_drift`, with a new `MergeJob::Finish`. Keep advances the base only if the source fingerprint still matches compute's. Studio's button for it comes in the T14 fix batch. *Cost: one more click after a drift.*
     - **T8b plan corrections:**
       - the expected model is computed at compute time (`catalog::default_acl_sql()` reads the target's default ACL) and stored in the plan JSON as `expected` and `own_sql`;
       - keep reads the source's head and schema at finish, and advances the base only when the fingerprint is unchanged (`base: not_advanced` otherwise);
       - the re-queue publishes from `main.rs`;
       - there is no `online.rs`.
     - **T8b rulings:**
       - drift skips the after-merge choice (now E21);
       - a rehearsal with drift ends `rehearsed`, with the drift shown;
       - a stale rehearsal ends `stale`;
       - `merges_one_applying` can't be hit by the worker.
     - **T8b review minors (deferred list):**
       - M3: `failed_phase` is `NULL` when `stored_plan` fails after the marker (use `online`);
       - M4: `merges_to_resume`'s doc landed inside `reconcile_once`'s;
       - M5: young rehearsals get normal reconcile rules;
       - M6: duplicate `Resume` jobs pile up behind long jobs, and `claim_refresh` (60 s) vs 120 s leaves little margin (platform mode);
       - M7: the "interrupted, nothing applied" wording;
       - M8: a cancel during the rehearsal build is only noticed at the next refresh;
       - M9: Studio — phase 1 is logged as `phase1`, so the `tx` steps never tick; word `after.refused`, `after.skipped` and `base: not_advanced`;
       - M10: strip `plan.expected` / `own_sql` from `getMerge` and the list (size only, about 100 KB);
       - concern 3: a leftover rehearsal blocks the target's reset or delete (Neon 412) for up to 30 min after a failed teardown.
     - **To T13 (M11):**
       - a rehearsal of a target with the data API on must not be falsely `stale` because of the rehearsal's data API bootstrap;
       - the data API gateway must not serve a `_rehearsal-*` branch.
     - **Docs to fix in T14 (from T8b):**
       - the apply's stale check is now the pre-check plus the phase 1 guard;
       - the digest is compute's;
       - a stale rehearsal ends `stale`;
       - recovery cancels the running statements first;
       - a leftover rehearsal can block reset or delete for up to 30 min;
       - the owner session uses `127.0.0.1`, not the local socket.
  14. **T13** (live scenarios, opus) `6edba1a..2a32329`: **merged into `schema-merge`** (no review of its own; T14 covers it).
     - **Results:**
       - `merge_live` 16/18 pass; the 2 failures are B1 and B2 below;
       - `catalog_live` 6/6, including `create_everything_round_trip` and `target_digest_runs_and_tracks_ddl`;
       - `live_db` 155/155;
       - `just live merge-e2e` passes;
       - unit tests and clippy are clean.
     - **Four live bugs fixed:**
       - `e71f6c4`: the owner session set `neon.forward_ddl`, which is superuser-only, so phases 1a, 1 and 2 had never worked on a real compute;
       - `6ac4e5a`: the digest's `text || "char"` was ambiguous, so the digest never ran; `"char"` is now cast to text;
       - `021778f`: verbose sessions read the script with `-f -`, so a phase 1 failure names its step;
       - `653d751` (M11): `lookup_data_key` resolves only `normal` branches' keys, so the gateway served `_rehearsal-*`.
     - **B1 → E22 (user, 2026-10-06): simulate the owner's automatic memberships** (task **T13c**). *Cost: more code than ignoring them; drift still flags hand-made owner grants.*
     - **B2 → E23 (user, 2026-10-06): fix child-branch restore in 6g** (task **T13b**; patching the Neon fork is allowed). *Cost: unknown size; delays T14.*
     - **T13 notes for T14:**
       - an Apply redelivered after a Retry is treated as recovery, so it can go back to `rehearsed` with "interrupted, nothing applied" while the first attempt's roles and enum values are on the target;
       - the proxy refuses an unroutable endpoint with "password authentication failed" (right refusal, misleading text);
       - a rehearsal still gets a sealed data API key;
       - `protected_target_needs_confirm` lives in `merge-e2e.sh`.
     - **Docs for T14:**
       - an open DDL transaction on the target holds the data API's `chelabase.schema_version` row, so it blocks every merge DDL statement;
       - the owner session uses `127.0.0.1`;
       - confirm `result.created_roles` after T13c.
     - **Leftover data:** none from the live tests; two soft-deleted `merge_e2e_*` databases, as other e2e scripts leave.
  15. **T13c** (owner memberships, E22; opus) `dbe093f`: **opus review Approved; merged into `schema-merge`.** Workspace unit tests pass.
     - **Grants observed live (Postgres 17)**, the same for every way of creating a role:
       - grantor `cloud_admin` (OID 10): admin only;
       - grantor `postgres`: inherit and set, from `createrole_self_grant`.
     - **What it does:**
       - `model::owner_grants` simulates those grants;
       - the analysis clears the owner's `member_of`;
       - the merged owner keeps the target's memberships;
       - the planner backstop never touches them;
       - `expected_model` adds the grants for every role the merge creates;
       - `precheck_fingerprint` ignores the owner's memberships in the plan's own roles.
     - **Live:**
       - `merge_live` 17/19, then the T13 role test passed when re-run alone after a string fix; `result.created_roles` confirmed live;
       - `restore_before_a_keep_merge…` still fails (B2, waiting on T13b);
       - the E20 role-retry case passes;
       - `catalog_live` 6/6.
     - **T13c review minors (deferred list):**
       - drift compares only which roles the owner belongs to, not grantor or options;
       - `precheck_fingerprint` also hides owner grants in roles the plan only updates (use `RoleOp::Create` names only);
       - a change to `postgres`'s own attributes still plans a refused `role:update:postgres`: decide in T14 whether to report it instead, and fix the test's comment.
     - **Docs for T14:** the source's changes to the owner's memberships are never merged (E22).
  16. **T13b** (child-branch restore): **E24 (user, 2026-10-07): two one-level detaches, no Neon change.** *Cost: on child restores only, the parent's data up to the branch point is copied twice and the computes reconnect twice.* Both Neon patches (one-copy multi-level detach; reads at kept LSNs for E17) are parked on the roadmap (user, 2026-10-07). **Done** `d8ea1fa` (opus): live `branch_live` 2/2 (main's restore unchanged; the new `restore_a_child_branch` passes), and `merge_live restore_before_a_keep_merge_uses_the_older_base` passes. Worker 447 unit tests pass. **Opus review running.** **E25 (user, 2026-10-07): keep the catch-up wait.** Between the two detaches the worker waits (`wait_for_catch_up`, `Worker.wal_catch_up_timeout`, 120 s) until the old timeline's WAL reaches the restore point again. Live, without it, the first detach's tenant reload made the second detach copy only up to the branch point, so the branch's own writes were silently lost. *Cost: about 1 s per child restore, at most 120 s before a retry.* **T13b concerns for T14:** (2) a branch made from the backup "at head" within about 1 s of the last detach's reload may miss writes; this predates 6g and applies to main too; the suggested fix is to make branch creation wait for its source to catch up; (3) a failed restore can leave dev's timeline detached from main (data unchanged); (4) after a child restore, a restore by time can reach before `history_start`, because the API doesn't check it for restores; (5) a lost job lets reconcile drop the restored timeline after 600 s while a detach may still run. **Docs for T14:** a child restore detaches twice and copies twice; the restore can wait up to 120 s; the backup no longer shares storage with the parent. The investigation (2026-10-06):
     - **Cause:** our restore detaches with `multi_level_and_no_reparent`. For a child branch the chain is restored timeline → old branch timeline → parent's timeline. Neon's `detach_ancestor.rs` `prepare` refuses with `AncestorNotEmpty` unless the restore LSN equals the branch's own branch point; its comment calls this a "non-technical requirement… needs us to copy and cut more layers".
     - **Options put to the user:**
       - two detaches in Chelabase (recommended): first detach the old timeline from its parent, then the restored one, using only upstream-tested one-level detaches; the cost is a double copy and two compute reconnects, paid only on child restores;
       - refuse clearly and fix later;
       - patch Neon's multi-level detach (about 150–300 pageserver lines plus an image rebuild).
     - **Also asked:** whether to park the Neon patch on the roadmap.
     - **The user will say when they're ready to answer.**
     - Then T14 (end-of-step review and fix batch), T15 (verification).
- **E11:** the crate tasks T2–T4 depend only on T1, so they start as soon as T1 merges, not after all of wave 1. *Cost: none.*
- **Done:**
  - spec `df0fa69` (approved by the user 2026-10-06); plan `4a0010c` (approved 2026-10-06); execution rulings `4d9a332`;
  - **T1** `368e6a2`..`094f8fb` (opus):
    - opus review "Needs fixes": an Important finding (an `EnumLabel` identity class) plus the E9 contract minors;
    - fix round 1, sonnet re-review: all addressed;
    - merged into `schema-merge`.
  - **T6** `07b17ae` (sonnet):
    - RED evidence was shown after the fact by temporarily neutralising code; the worktree was checked clean and no tests were removed;
    - sonnet review: Approved, minors only;
    - merged into `schema-merge`;
    - the worktree is kept until its queued live GREEN re-run finishes.
  - **T3** `b25f347`..`8b139dd` (opus):
    - opus review "Needs fixes": two Important findings (integrity refs; definitions naming something renamed on the other side) plus minors;
    - E12 rulings;
    - fix round 1, opus re-review: all addressed;
    - merged into `schema-merge`.
    - **T3's own extra ruling:** when all of an item's missing references are decided by one other conflict, the item joins that conflict.
- **User answers at plan time (2026-10-06):**
  - **U1. Roles: extend the control-plane path** (overrides the spec's "LOGIN roles through `create_role`, the rest reported"): `create_role` gains NOLOGIN roles, memberships and attributes; a new role update path; merges use both.
  - **U2. Allowed role attributes:** LOGIN/NOLOGIN, owner-level, INHERIT/NOINHERIT, CONNECTION LIMIT, VALID UNTIL, **BYPASSRLS**, and memberships in roles the user owns (never in Chelabase's own roles: `authenticator`, `anon`, `authenticated`, `chelabase_*`, `neon_superuser` except through owner-level). Anything else is reported.
  - **U3. Passwords:** no reveal-once route. A merge's result lists the roles it created, each linking to the target branch's Connection panel (the existing `GET …/connection?role=` reveal). Overrides the spec's "revealed once from the merge page".
- **Pre-flight scan (2026-10-06, three read-only agents):** findings folded into the rulings and tasks below; nothing lives in scratch.
- **Rulings (plan, each with its cost if wrong):**
  - **P1. Recovery marker lives in its own schema `chelabase_merge`** (table `applied(merge_id uuid PRIMARY KEY, applied_at timestamptz NOT NULL DEFAULT now())`, owned by `cloud_admin`, no privileges for PUBLIC), not in `chelabase`: the data API bootstrap's tamper check quarantines any extra relation in `chelabase` (`services/worker/src/data_api.rs` ~270–316), and `chelabase` exists only where the data API was bootstrapped. `chelabase_merge` is excluded from the model like `chelabase`. *Cost: one more internal schema per merged database.*
  - **P2. The worker runs merge SQL through a new `ComputeBackend::admin_sql_with(ep, db, sql, SqlOpts)`** (docker exec psql as `cloud_admin`, like `admin_sql`), with per-call timeout, `PGAPPNAME`, `VERBOSITY=default` and DDL forwarding off. Scripts start with `SET ROLE postgres` so objects stay owner-owned and the owner's default privileges fire. There's no owner connection in the worker, and owner passwords set by SQL aren't sealed. The spec's "DDL forwarding on" in phase 1 is dropped: role DDL never runs in phase 1 (roles are phase 0, P5), and user steps may not contain role or database DDL (P8). *Cost: psql's stdout is the result channel; errors are parsed from stderr.*
  - **P3. Timeouts:** phase 1 `SET LOCAL lock_timeout` from the merge's option (default 5 s, R2) and `statement_timeout = '15min'`, admin timeout 16 min; each phase 2 step: `statement_timeout = 0`, admin timeout 6 h. On an admin timeout the worker runs `SELECT pg_cancel_backend(pid) FROM pg_stat_activity WHERE application_name = 'chelabase-merge-<merge id>'`. Worker config: `merge_tx_timeout`, `merge_online_step_timeout`. *Cost: constants.*
  - **P4. Rehearsal and static endpoints are unroutable:** `neon_endpoints` gains `routable boolean NOT NULL DEFAULT true` and `static_lsn text NULL`. `wake_compute` and `access_control` use a new routable-only lookup. Rehearsal branches copy role verifiers, so without this they'd be connectable through the proxy. *Cost: none.*
  - **P5. Phase 0 runs the extended role path (U1) directly in the worker** (`Worker::create_role` / `Worker::update_role`), not over the NATS RPC. In a rehearsal, phase 0 creates and alters roles by SQL with no password and no metadata. *Cost: none.*
  - **P6. Exclusions are computed inside the catalog SQL:**
    - always: schemas `chelabase` and `chelabase_merge`, and objects with `pg_depend` type `e`;
    - the users table named in `chelabase.app_users_table` (when it exists): its fixed columns (`MANAGED_COLUMNS`), the guard triggers, the policies `chelabase_users_select_own` / `chelabase_users_update_own`, the index `chelabase_users_email_key`, and the grants to `authenticated` / `chelabase_auth_sync`;
    - roles `authenticator`, `anon`, `authenticated`, `chelabase_admin`, `chelabase_auth_sync`, `cloud_admin`, `neon_superuser` and every `pg_*` role;
    - default-privilege rows the data API owns (`FOR ROLE postgres IN SCHEMA <served>` to `anon`/`authenticated`).

    The name lists are copied into `chelabase-schema::exclusions` with a comment pointing at their sources, and a worker test asserts they match the worker's constants (`MANAGED_COLUMNS`, `USERS_SYNC_ROLE`). *Cost: a list to keep in step; the test catches drift.*
  - **P7. Default privileges are simulated:** when the merged model creates a table or sequence, the planner and the expected model add the grants the target's `pg_default_acl` (for role `postgres`, global and per-schema) would create, so the drift check doesn't flag them. *Cost: none.*
  - **P8. User steps and own SQL are classified with pg_query in `chelabase-schema`:**
    - refused: transaction control (`TransactionStmt`), role and database DDL (`CreateRoleStmt`, `AlterRoleStmt`, `DropRoleStmt`, `GrantRoleStmt`, `CreatedbStmt`, `DropdbStmt`, `AlterDatabase*`), `AlterSystemStmt`, `VacuumStmt`;
    - sent to phase 2: concurrent `IndexStmt` / `DropStmt` and `ReindexStmt CONCURRENTLY`;
    - an `AlterEnumStmt` adding a value: phase 1a;
    - everything else: phase 1.

    *Cost: a user who wants a role in a step uses the roles API first.*
  - **P9. BYPASSRLS needs an acknowledgement in a merge:** a phase 0 step that creates a role with BYPASSRLS, or turns it on, is flagged `security` and needs a per-item acknowledgement before apply, like data-loss drops. *Cost: one click; protects the RLS that the data API and app users rely on.*
  - **P10. Error codes:** `merge_conflicts_open`, `merge_drops_unacknowledged` (also used for security acknowledgements), `merge_not_rehearsed`, `merge_stale`, `merge_busy`, `merge_open_exists`, `merge_base_unavailable`, `merge_version_conflict` (409 with `error.current`, like `saved_query_conflict`), `merge_confirm_required` (new; `sql_confirm_required` is SQL-editor wording), `branch_merge_busy` (branch ops refused while a merge runs), `role_attribute_not_allowed`, `role_membership_not_allowed`. *Cost: none.*
  - **P11. One merge job at a time:** the merge consumer handles one message at a time, like the branch consumer, so a long rehearsal delays other merges on this machine (dev mode). Revisit for platform mode. *Cost: queued merges wait.*
  - **P12. Studio route `/app/projects/$projectId/databases/$databaseId/merges/$mergeId`**, since Studio has no database-level route (a database opens inline in the project page). The merges list is a new `DatabaseView` `"merges"` in `project-page.tsx`. *Cost: none.*
  - **P13. The roles API extension (U1) is API and worker only in 6g:** Studio's role form stays as it is, and the role list shows the new attributes read-only. *Cost: NOLOGIN roles, memberships and attributes are set by API or merge until a Studio form follows.*
  - **P15. No lock-wait sampling in 6g:** the rehearsal records per-step timings only (the spec's Part 5 also asks for lock waits). Sampling `pg_locks` during a rehearsal is parked. *Cost: the preview shows durations, not lock waits; a long step on a big table is still visible by its time.*
  - **P16. Unknown merges are 404 `not_found`** (no `merge_not_found` code), like other resources. *Cost: none.*
  - **P14. `BranchKind::Rehearsal`:**
    - every check that refuses `== Backup` becomes `!= Normal` where the operation needs a normal branch;
    - `list_branches` hides rehearsals;
    - rehearsal names are `_rehearsal-<merge id 8 hex>` (a leading `_` is invalid for user branch names; T5 confirms the name rule);
    - the cap and child checks already count `normal` only;
    - a rehearsal is a child timeline of the target, so reset, restore and delete of the target are refused while its merge is rehearsing (`branch_merge_busy`).

    *Cost: none.*
- **Execution rulings (pre-execution scan, 2026-10-06; they override the task text below):**
  - **E1. T6 owns endpoint routing.** The `routable`/`static_lsn` columns move out of T5's migration into T6's own `20261010000003_endpoint_routing.sql`, with the `neon.rs` lookups `routable_endpoint` / `routable_role_secret` and the live test `routable_lookup_skips_unroutable_endpoints`. T6 runs in parallel with T5 and needs them. *Cost: none.*
  - **E2. T7 starts when T2 is merged**, not at the start of wave 2: its tests feed catalog JSON through `parse_catalog`. *Cost: T7 starts later in wave 2.*
  - **E3. T8a/T8b split corrected:** T8a's rehearsal needs the phase executor, so they swap.
    - **T8a:** jobs, claims, compute (with rebaseline), and `run_phases` in both modes. Its tests: the compute tests, the claim test, and phase 0 idempotent, the phase 1 re-check mismatch, phase 1 failure, the invalid-index resume, the online-step timeout, online failure then retry.
    - **T8b:** the rehearse job, the apply job, finish, crash recovery, cancel and reconcile. Its tests: the rehearse tests, the apply happy-path journal, crash recovery, finish, cancel and reconcile.

    *Cost: none.*
  - **E4. The phase 1 guard uses a SQL digest, not the model fingerprint.** T4 adds `pub const TARGET_DIGEST_SQL: &str` (one `SELECT` returning an `md5` text over a catalog projection of the 6g object types). `phase1_script(plan, opts, expected_digest: &str)` raises inside the transaction when the live digest differs. T8a reads the digest alongside the target model and stores it in the plan JSON as `target_digest`. The apply request's stale check (T8b) compares the same digest. *Cost: none.*
  - **E5. T4's `RoleSpecModel` / `RolePatchModel`** are the schema crate's own serde types, with the same fields as T9's `RoleSpec` / `RolePatch`. T8a converts them. *Cost: none.*
  - **E6. The identity map is full, not sparse.** T1 found that after a branch point both sides allocate OIDs independently from the same starting value, so unrelated new objects can share an OID. "No pair means the OID is unchanged" is therefore unsafe.
    - Every base write records an explicit pair for every base object: create and reset bases use `IdentityMap::self_mapped(base)`; merge-with-keep bases (T8b) record the source OID and the matched target OID, or `None`.
    - `to_base` and `from_base` return `Option`. `None` means no base identity, so the object is matched by name.

    *Cost: maps grow with the schema (one small pair per object).*
  - **E7. T1 plan corrections accepted:**
    - C1 `uuid` dependency;
    - C2 an attnum per side in a pair;
    - C4 column grants on `Column.grants`;
    - C5 boxed `Alter` snapshots;
    - T1 defines `ObjKey`, `ObjSnapshot` and `ReportedChange` in `analysis.rs` for T3 and T4 to share. Renames need both to agree.
  - **E8. T6 correction:** the proxy and spec lookups live on `NeonMetadataPort` (`services/orchestrator/src/internal/mod.rs`, fake in `tests/internal.rs`), and the record type is `NeonEndpoint`. Primary specs now always carry `"mode":"Primary"`, which is the fork's default.
  - **E9. T1 review minors that shape the shared contract are fixed now**, not deferred:
    - identity sequences carry grants;
    - memberships carry a grantor and are sorted deterministically;
    - identity pairs are kept sorted;
    - `Table::shallow()` for table-level snapshots;
    - `ObjKey: Ord`;
    - a comment on the impossible sample model.

    Also `ObjClass::EnumLabel` (the review's Important finding), so renamed enum values are followed by OID. Three parallel tasks build on these types next. *Cost: one extra fix round on T1.*
  - **E10. Schema and sequence snapshots also embed their grants** (T1 fix round 1 left them). T3 adds `shallow()` for both, as `Table::shallow()` does, and snapshots them only through it. Other T1 fix-round changes later tasks rely on:
    - `IdentityMap.pairs` is private: read it through `pairs()`, change it only through `record` or `from_pairs` (T7 and T8b build whole maps with `from_pairs`);
    - table snapshots are built only through `ObjSnapshot::table()`.

    *Cost: none.*
  - **E12. T3 review rulings:**
    - **(a)** When the other side renamed an object, a picked item whose references or definition text name that object (by its old name) becomes a `Dependency` conflict. The engine never rewrites definition text. *Cost: these cases need own SQL or keep target.*
    - **(b)** On a `ColumnOrder` conflict only `TargetColumnsFirst` and `OwnSql` are valid; `KeepTarget` is invalid, as the spec says. *Cost: none.*
    - **(c)** Re-baseline uses an empty `IdentityMap`, so everything is matched by name. *Cost: renames show as a drop plus a create in a re-baselined merge (the preview already warns).*
    - **(d)** References to objects outside the model are matched by name downstream (T4).
    - **(e)** T8a and T10 drop a stored resolution when its conflict's kind or snapshots changed on recompute, so a stored choice can't change meaning. *Cost: the user re-chooses.*
  - **E13. Live runs from a task worktree first merge the current `schema-merge`** into the task branch. Every metadata migration already applied to the shared database must exist in the worktree, or sqlx refuses to start.
    - T9's `just live` moved the dev stack to `schema-merge-t9`, which lacked T6's applied migration `20261010000003`, so the orchestrator wouldn't start.
    - Dev apps are restarted from `schema-merge` (`just live just dev`) once T5 and T9 are merged.

    *Cost: one merge per task before its live run.*
  - **T3 implementer rulings, listed for the user:**
    1. An enum change the target can't take (a value dropped or reordered on the source) is an `Edit` conflict, and values added on both sides conflict (no per-value merge).
    2. A drop vs edit covers the whole subtree (a dropped table vs anything changed inside it).
    3. Reversed by E12b.
    4. A dependency conflict holds the referencing and the referenced item.
    5. Owners, memberships and enum column types are also integrity-checked.
    6. Flags describe the step from the target's version to the source's.
    7. Safe type widenings: `varchar(n)`→`varchar(m≥n)`/`text`, `smallint`→`integer`→`bigint`, `numeric(p,s)`→`numeric(q≥p,s)`, `real`→`double precision`, `varbit(n)`→`varbit(m≥n)`, element-wise for arrays; everything else is data loss.
    8. Reported changes are matched by kind, schema, name, parent and arguments.
    9. A resolution for an id that no longer exists is ignored; for duplicates the last one wins; empty own SQL is invalid.
    10. Base objects need a pair in the map (see E12c).
    11. When both sides add the same column (an overlap), the source's relative column order isn't kept.
- **Deferred minors:**
  - **(T1 re-review)** `IdentityMap`'s side indexes are keyed by `(class, side OID)`. Two base objects mapped to the same side OID would silently overwrite each other; `from_pairs` and `record` don't detect it.
  - **(T3 re-review)**
    - **Fix before merge:** the E12a check covers only source-picked texts. A target-picked text that names something the source renamed leaves `M` stale. The apply works, but the drift compare against `M` could flag false drift. Make the check symmetric.
    - The own-SQL exemption in `broken()` vs `Asm::attnum`'s `BrokenReference` is inconsistent.
    - `named_columns` drops all of an index's column references if one key doesn't parse.
    - Policy text references can be false positives (subquery columns).
  - **(T6 review)**
    - document on `neon_endpoint` / `neon_role_secret` that proxy paths must use the `routable_*` versions;
    - document that an `admin_sql_with` timeout leaves the in-container psql running, so callers cancel by `application_name` (P3);
    - `admin_psql_command`'s doc still says "terse";
    - `debug_assert` the `PGAPPNAME` length and charset;
    - the stray `use std::time::Duration` placement in `fake`;
    - `ComputeMode::Static.lsn` isn't validated as `X/Y`.
- **Left running:** nothing.

## Global Constraints

- Postgres 17 only: the catalog reader refuses any other `server_version_num` major with `SchemaError::UnsupportedServer`. The model stores `server_major`.
- Catalog reads run with `SET LOCAL search_path = pg_catalog` inside `BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY`, so the `pg_get_*def` text is stable. Every generated identifier is quoted with a `quote_ident` equivalent (`chelabase_schema::sql::ident`), and literals with `sql::literal`.
- `MODEL_VERSION: u32 = 1`. A snapshot whose `model_version` or `server_major` differs is never trusted.
- Temporary NOT NULL checks are named `chelabase_merge_nn_<attnum>_<table oid>`, and replacement indexes `chelabase_merge_ix_<n>`. The reader reports leftovers with these prefixes as `ReportedKind::MergeLeftover`, never as user changes.
- Passwords are never read, copied, logged or exported.
- New dependencies at the versions the pre-flight checked: `petgraph = "0.8.3"` (only if the planner needs it; a hand-written Kahn sort is fine), `insta` is **not** used (it's too new for the release-age rule; use plain `assert_eq!` on strings). The Studio side needs no new npm dependency (the side-by-side view is a structured per-object comparison, not `@codemirror/merge`).
- New routes go into `crates/chelabase-routes` `ENDPOINTS`. New error codes go into `ErrorCode`, `as_str`, the `ALL` table (bump its length), `apps/docs/content/docs/reference/errors.mdx` and the regenerated clients (`just api-gen`). New `MetadataPort` / `JobPort` / `WorkerMetadata` methods need the real impl, `Doc` (openapi.rs) and every fake (`services/orchestrator/tests/api.rs`, `services/worker/src/fakes.rs`, any other implementor).
- Never mirror Supabase. Commit identity: `git commit`, with the `Co-Authored-By` and `Claude-Session` trailers. Push `schema-merge` after every merge-back and checkpoint; `-tN` branches are not pushed.

## Review Focus

1. **Identifiers that need quoting:** mixed case, spaces, reserved words, quotes inside names (`"Order"`, `"my table"`, `"a""b"`) must round-trip and appear quoted in every generated statement and in the export. *Pinned by:* T2's fixture includes such names; T4 `quotes_every_identifier`; T2 round trip.
2. **The target changes between rehearsal and apply** (someone runs DDL on main): the merge goes `stale` and nothing changes. *Pinned by:* T8b `phase1_recheck_mismatch_rolls_back_and_marks_stale` and the live scenario `concurrent_ddl_on_target_makes_the_merge_stale`.
3. **A crash or timeout in the middle of `CREATE INDEX CONCURRENTLY`** leaves an INVALID index: the resume drops it concurrently and rebuilds it, and never reports the merge done with an invalid index. *Pinned by:* T8b `resume_drops_an_invalid_index_and_rebuilds` and `online_step_timeout_cancels_the_backend`.
4. **A merged object depends on a reported one** (a default or policy calling a function that only the source has): the preview lists the dependency, and the rehearsal fails with the Postgres error on that step, never a silent partial apply. *Pinned by:* T3 `dependency_on_reported_function_is_listed`; T8a `rehearsal_failure_returns_to_draft_with_step_and_error`.
5. **Default privileges on the target** (the data API grants new tables to `anon`/`authenticated`) mustn't show up as drift or as a change to merge back. *Pinned by:* T4 `default_privileges_are_simulated_for_new_tables`; T13 live `new_table_in_served_schema_has_no_drift`.

## Models and schedule

| Task | Implementer | Review | Wave |
|---|---|---|---|
| T1 crate scaffold, model types, SQL helpers, module stubs | opus | opus (every crate task builds on it) | 1 |
| T5 metadata migration and queries, BranchKind, error codes, NATS merge contracts | sonnet | sonnet (worker and API build on it) | 1 |
| T6 compute access: `admin_sql_with`, static mode, routable endpoints | sonnet | sonnet (merge worker builds on it) | 1 |
| T9 roles path extension (NOLOGIN, attributes, memberships, update) | opus | sonnet (merge phase 0 builds on it) | 1 |
| T2 catalog reader (SQL + JSON parse + exclusions + round-trip fixture) | opus | sonnet | 2 |
| T3 three-way analysis, conflicts, integrity, resolutions, merged model | opus | opus (nothing else checks the merge rules) | 2 |
| T4 planner, phases, user steps (pg_query), export, drift compare | opus | opus (nothing else checks DDL order) | 2 |
| T7 base snapshots (create hook, reset fix, live fallback, identity map) | opus | sonnet | 2 |
| T10 orchestrator merge API, branch-op refusals, rehearsal filtering | sonnet | sonnet (Studio builds on it) | 2 |
| T8a merge worker: jobs, claims, compute, rehearsal | opus | opus | 3 |
| T8b merge worker: apply phases, online resume, finish, after-merge, reconcile | opus | opus | 3 (after T8a) |
| T11 Studio: branch actions, merges list, merge page | opus | sonnet | 3 |
| T12 docs pages and errors table | sonnet | none (end review) | 3 |
| T13 live scenarios (`merge_live.rs`), round trip on the stack, `just merge-e2e` | opus | none (end review; it is verification) | 4 |
| T14 end-of-step review, fix batch, scoped re-review | opus review, sonnet fixes, opus re-review | — | 4 |
| T15 full verification and Playwright pass | sonnet | — | 4 |

**Parallel work:**
- Every task in a wave runs in its own worktree from `schema-merge`: `just wt-add schema-merge-tN schema-merge`, e.g. `schema-merge-t1`, `schema-merge-t8a`.
- A task with a review is reviewed there. It's then merged into `schema-merge` with `--no-ff`, and the worktree is removed (`just wt-remove schema-merge-tN`, then `git branch -d schema-merge-tN`).

**Wave 1** (T1 ∥ T5 ∥ T6 ∥ T9):
- Overlaps resolved by hand at merge time:
  - `crates/chelabase-nats/src/lib.rs` (T5 adds the merge subject, T9 extends `RoleCreateRequest`);
  - `crates/chelabase-types/src/api_error.rs` (T5 adds merge codes, T9 adds role codes);
  - `crates/chelabase-metadata/src/lib.rs` module lines;
  - `services/worker/src/fakes.rs` (T6 adds `admin_sql_with`, T9 role fields).
- Merge order: T1, T5, T6, T9.

**Wave 2** starts when wave 1 is merged:
- T2, T3 and T4 touch disjoint files of the crate (`catalog.rs` / `analysis.rs` / `plan.rs` and `export.rs`, each with its own tests). T4's round-trip test lands after T2 merges, as a follow-up commit by T4 or T13.
- T7 and T10 run alongside.

**Wave 3:**
- T8a starts when T2, T3, T4 and T7 are merged; T8b follows T8a.
- T11 and T12 start when T10 is merged.

**Wave 4:** after wave 3.

---

## Task 1: crate scaffold, model types, SQL helpers, module stubs

**Files (create):**
- `crates/chelabase-schema/Cargo.toml`
- `crates/chelabase-schema/src/{lib.rs, model.rs, identity.rs, sql.rs, exclusions.rs, fingerprint.rs, error.rs}`
- stubs `src/{catalog.rs, analysis.rs, plan.rs, export.rs, compare.rs, steps.rs}`

**Modify:** root `Cargo.toml` (`members`, `[workspace.dependencies] chelabase-schema = { path = … }`, `pg_query` already present, `petgraph` only if T4 asks).

**Dependencies:** `serde` (derive), `serde_json`, `sha2`, `hex`, `thiserror`, `pg_query` (all `workspace = true`). Dev: `tokio`, `tokio-postgres`, `getrandom`.

**Interfaces (produced; later tasks rely on these names):**

```rust
pub const MODEL_VERSION: u32 = 1;
pub const SUPPORTED_SERVER_MAJOR: u32 = 17;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Oid(pub u32);

/// Catalog class of an object; with an Oid it is the object's identity at the base.
pub enum ObjClass { Schema, Table, Column, Constraint, Index, Enum, Sequence, Policy, Role }

pub struct SchemaModel {
    pub model_version: u32,
    pub server_major: u32,
    pub schemas: Vec<Schema>,
    pub tables: Vec<Table>,          // columns, constraints, indexes, policies, grants nested
    pub enums: Vec<EnumType>,
    pub sequences: Vec<Sequence>,    // standalone and OWNED BY; identity sequences live on the column
    pub roles: Vec<Role>,            // attributes (U2 set + reported extras) and member_of
    pub default_privileges: Vec<DefaultPrivilege>,
    pub reported: Vec<Reported>,     // views, functions, triggers, partitioning, domains, …, MergeLeftover
}
```

- **Field sets** come from Part 1 of the spec and the pre-flight's catalog facts. Notable details:
  - `Column` has `attnum: i16`, `not_null: bool` (PG17 `attnotnull`), `identity: Option<Identity>` (with its sequence parameters), `generated: Option<String>`, `default: Option<String>`.
  - `Constraint` keeps `definition` from `pg_get_constraintdef(oid, true)`, plus `validated`.
  - `Index` keeps normalized parts with the name taken out of the definition, plus `valid`. Indexes backing constraints are **not** listed separately.
  - `Grant { grantee: RoleRef, privilege: String, grantable: bool, columns: Option<…> }` with `RoleRef::Public | RoleRef::Role{oid, name}`.
- **Every nameable item** carries `oid: Oid` and `name` (qualified where it applies). All types derive `Clone, Debug, PartialEq, Serialize, Deserialize`. The JSON field names are the contract for T2 and the stored snapshots.
- **Sorting:** the model is kept sorted by `(schema, name)` at every level, so serialization is deterministic.
- `identity.rs`: `pub enum Side { Parent, Branch }`; `pub enum Direction { IntoParent, FromParent }` with `fn source_side()` / `fn target_side()`; `pub struct IdentityMap { pub pairs: Vec<IdentityPair> }`; `pub struct IdentityPair { pub class: ObjClass, pub parent: Oid, pub branch: Oid, pub sub: Option<i16> }`; `impl IdentityMap { pub fn identity() -> Self /* empty = "same OIDs on both sides" */; pub fn to_base(&self, side: Side, class: ObjClass, oid: Oid, sub: Option<i16>) -> (Oid, Option<i16>); pub fn from_base(&self, side, class, base_oid, sub) -> (Oid, Option<i16>); pub fn record(&mut self, class, parent: Oid, branch: Oid, sub) }`. When there's no pair, the OID is unchanged.
- `sql.rs`: `pub fn ident(s: &str) -> String` (Postgres `quote_ident` rules: unquoted only if `[a-z_][a-z0-9_$]*` and not a keyword; the keyword list comes from `pg_query::scan`'s keyword kinds or a reserved list); `pub fn qualified(schema, name) -> String`; `pub fn literal(s) -> String`.
- `fingerprint.rs`: `pub fn fingerprint(m: &SchemaModel) -> String` (SHA-256 hex of the canonical JSON).
- `error.rs`: `pub enum SchemaError { UnsupportedServer(u32), Parse(String), UnknownCatalogValue{what, value}, Steps(Vec<StepError>) }`.
- **Stubs:** each later module exists with its pinned signature (Tasks 2–4) whose body is `unimplemented!("task N")`, so wave 2 starts from compiling code.

**Tests (in `src/*`):**
- `ident_quotes_when_needed`: `users` → `users`; `Order` → `"Order"`; `my table` → `"my table"`; `a"b` → `"a""b"`; `select` → `"select"`.
- `literal_escapes_quotes_and_backslashes`.
- `model_serializes_deterministically`: two models built in different orders serialize to equal strings after `normalize()`.
- `fingerprint_changes_with_any_field`.
- `identity_map_round_trips`: `to_base(from_base(x)) == x` for mapped and unmapped OIDs.
- `direction_sides`: IntoParent → source Branch, target Parent; FromParent the reverse.

**Commit:** `step 6g T1: chelabase-schema crate, model types and SQL helpers`.

## Task 5: metadata, BranchKind, error codes, NATS merge contracts

**Files:**
- Create: `crates/chelabase-metadata/migrations/20261010000001_schema_merge.sql`, `crates/chelabase-metadata/src/merges.rs`, `crates/chelabase-metadata/src/schema_bases.rs`.
- Modify:
  - `crates/chelabase-metadata/src/{lib.rs, branches.rs, neon.rs}`
  - `crates/chelabase-metadata/tests/live_db.rs` (add the new tables to `sweep_leftovers` unless they cascade)
  - `crates/chelabase-types/src/api_error.rs` (P10 codes, the `ALL` length)
  - `crates/chelabase-nats/src/lib.rs`

**Migration:**

```sql
ALTER TABLE branches DROP CONSTRAINT branches_kind_check;   -- verify the generated name first (\d branches)
ALTER TABLE branches ADD CONSTRAINT branches_kind_check CHECK (kind IN ('normal','backup','rehearsal'));

ALTER TABLE neon_endpoints ADD COLUMN routable boolean NOT NULL DEFAULT true;
ALTER TABLE neon_endpoints ADD COLUMN static_lsn text NULL;

CREATE TABLE branch_schema_bases (
    branch_id      uuid NOT NULL REFERENCES branches(id) ON DELETE CASCADE,
    database_name  text NOT NULL,
    model          jsonb NULL,              -- NULL: not read yet (R6); the location still says where to read it
    model_version  integer NULL,
    server_major   integer NULL,
    source         text NOT NULL CHECK (source IN ('create','reset','merge','update','rebaseline')),
    timeline_id    text NULL,               -- live location; NULL for 'rebaseline'
    lsn            text NULL,
    identity_map   jsonb NOT NULL DEFAULT '{"pairs":[]}',
    taken_at       timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (branch_id, database_name)
);

CREATE TABLE merges (
    id                 uuid PRIMARY KEY,
    database_id        uuid NOT NULL REFERENCES databases(id) ON DELETE CASCADE,
    source_branch_id   uuid NOT NULL REFERENCES branches(id) ON DELETE CASCADE,
    target_branch_id   uuid NOT NULL REFERENCES branches(id) ON DELETE CASCADE,
    direction          text NOT NULL CHECK (direction IN ('into_parent','from_parent')),
    database_name      text NOT NULL,
    status             text NOT NULL CHECK (status IN ('computing','draft','rehearsing','rehearsed','applying',
                        'online','finishing','done','applied_with_drift','failed','stale','cancelled')),
    failed_phase       text NULL,
    version            bigint NOT NULL DEFAULT 1,
    base_fingerprint   text NULL, source_fingerprint text NULL, target_fingerprint text NULL,
    analysis           jsonb NULL,          -- chelabase_schema::Analysis
    resolutions        jsonb NOT NULL DEFAULT '[]',
    user_steps         jsonb NOT NULL DEFAULT '[]',
    acknowledgements   jsonb NOT NULL DEFAULT '[]',
    options            jsonb NOT NULL DEFAULT '{"lock_timeout_ms":5000,"after":"keep"}',
    plan               jsonb NULL,
    rehearsal          jsonb NULL,
    result             jsonb NULL,          -- created roles, drift, after-merge outcome
    rebaselined        boolean NOT NULL DEFAULT false,
    error              text NULL,
    claimed_at         timestamptz NULL,    -- claim heartbeat (P11/T8a)
    created_by         text NOT NULL,
    created_at         timestamptz NOT NULL DEFAULT now(),
    updated_at         timestamptz NOT NULL DEFAULT now()
);
CREATE UNIQUE INDEX merges_one_open ON merges (source_branch_id, target_branch_id, database_name)
    WHERE status NOT IN ('done','applied_with_drift','failed','stale','cancelled');
CREATE UNIQUE INDEX merges_one_applying ON merges (target_branch_id)
    WHERE status IN ('applying','online','finishing');

CREATE TABLE merge_events (
    id          bigserial PRIMARY KEY,
    merge_id    uuid NOT NULL REFERENCES merges(id) ON DELETE CASCADE,
    at          timestamptz NOT NULL DEFAULT now(),
    kind        text NOT NULL,              -- status | step | note
    step_id     text NULL,
    phase       text NULL,
    outcome     text NULL,                  -- ok | failed | skipped
    duration_ms bigint NULL,
    detail      jsonb NULL
);
```

**Interfaces (produced):**
- `BranchKind::Rehearsal` (`"rehearsal"`), with `as_str` and `parse`.
- `neon.rs`: `routable_endpoint(id) -> Option<NeonEndpointRecord>` and `routable_role_secret(ep, role)`, both requiring `routable`.
- `branches.rs`:
  - `create_rehearsal_branch(database_id, parent: BranchId, name, created_by) -> Result<BranchRecord, MetadataError>`: kind `rehearsal`, status `pending`.
  - `switch_to_pending_timeline(..)` gains `branched_at_lsn: &str`, written atomically with the switch.
- `schema_bases.rs`:
  - `pub struct SchemaBase { branch_id, database_name, model: Option<serde_json::Value>, model_version: Option<i32>, server_major: Option<i32>, source: BaseSource, timeline_id: Option<String>, lsn: Option<String>, identity_map: serde_json::Value, taken_at }`
  - `upsert_schema_base(&SchemaBase)`, `schema_base(branch, db) -> Option<SchemaBase>`, `set_schema_base_model(branch, db, model, version, major) -> bool`
- `merges.rs`:
  - `pub struct MergeRecord` (every column); `pub enum MergeStatus` (12 values; `is_open()`, `is_applying()`); `pub enum MergeDirection`
  - `create_merge(NewMerge) -> Result<MergeCreate, _>` where `MergeCreate { Created(MergeRecord), OpenExists(Uuid), ApplyingExists(Uuid) }`
  - `merge(id) -> Option<MergeRecord>`; `list_merges(database_id, branch: Option<BranchId>) -> Vec<MergeRecord>`
  - `update_merge_draft(id, expected_version, MergeDraftPatch) -> MergeWrite`, where `MergeWrite { Saved(MergeRecord), Stale(MergeRecord), NotFound, NotEditable(MergeStatus) }`; a save in `rehearsed` moves the merge to `draft` and clears `rehearsal`
  - `set_merge_status(id, from: &[MergeStatus], to, patch: MergeStatusPatch) -> bool`, where `MergeStatusPatch { analysis, plan, rehearsal, result, error, failed_phase, fingerprints… }` (all `Option`)
  - `touch_merge_claim(id, status) -> bool`; `stale_merge_claims(older_than) -> Vec<MergeRecord>`
  - `append_merge_event(MergeEvent)`; `merge_events(id) -> Vec<MergeEvent>`
  - `cancel_drafts_for_branch(branch) -> u64`
  - `branch_has_busy_merge(branch) -> Option<Uuid>`: rehearsing or applying, as source or target
- `chelabase-nats`:
  - `SUBJECT_MERGE = "chelabase.merge"`, `CONSUMER_MERGE = "chelabase-worker-merge"`, added to `control_subjects()`
  - `#[serde(rename_all = "snake_case", tag = "kind")] enum MergeJob { Compute { merge_id: Uuid }, Rehearse { merge_id }, Apply { merge_id }, Resume { merge_id }, Cancel { merge_id } }`
  - `publish_merge(js, &JobEnvelope<MergeJob>)`, `run_merge_consumer(js, handler)` (same `ConsumerSpec` as branch: `ACK_WAIT` 300 s with progress acks)
- `ErrorCode`: the P10 codes.

**Tests:**
- Live (`live_db.rs`, `#[ignore]`):
  - `rehearsal_kind_round_trips_and_is_not_counted`;
  - `branched_at_lsn_updates_with_the_switch`;
  - `routable_lookup_skips_unroutable_endpoints`;
  - `schema_base_upsert_and_fill`;
  - `one_open_merge_per_pair` (second create → `OpenExists`);
  - `one_applying_merge_per_target`;
  - `draft_update_checks_version` (`Stale` carries the current row);
  - `saving_a_rehearsed_merge_returns_it_to_draft`;
  - `status_transition_is_guarded`;
  - `cancel_drafts_for_branch_leaves_applying_alone`;
  - `busy_merge_is_found_for_source_and_target`;
  - `merge_events_in_order`;
  - `deleting_a_branch_cascades_its_bases`.
- Unit:
  - `merge_status_round_trips`;
  - `merge_job_serde_shape` (pins the JSON: `{"kind":"apply","merge_id":"…"}`);
  - `control_subjects_include_merge`;
  - the `ErrorCode` table test.

**Verification:** `cargo test -p chelabase-metadata -p chelabase-nats -p chelabase-types`. The live tests run later, through the coordinator's queue: `just live cargo test -p chelabase-metadata --test live_db -- --ignored --test-threads=1`.

**Commit:** `step 6g T5: merge metadata, rehearsal kind, merge job contracts`.

## Task 6: compute access (admin_sql_with, static mode, routable endpoints)

**Files (modify):**
- `crates/chelabase-neon/src/{backend.rs, spec.rs}`
- `services/orchestrator/src/internal/{proxy.rs, spec.rs}`
- `services/orchestrator/src/lib.rs`, plus the `MetadataPort` additions with their `Doc` and fakes
- `services/worker/src/{ports.rs, fakes.rs, provision.rs}` (the endpoint row writer gains `routable` and `static_lsn`)

**Interfaces (produced):**

```rust
pub struct SqlOpts { pub timeout: Duration, pub app_name: Option<String>, pub verbose_errors: bool }
impl SqlOpts { pub fn admin_default() -> Self /* 30 s, None, false: today's admin_sql */ }
// ComputeBackend:
async fn admin_sql_with(&self, ep: &str, db: &str, sql: &str, opts: &SqlOpts) -> anyhow::Result<String>;
// admin_sql becomes a provided method calling admin_sql_with(.., &SqlOpts::admin_default()).

pub enum ComputeMode { Primary, Static { lsn: String } }   // SpecInputs gains `mode`
// config_response emits "mode": "Primary" or {"Static": "<X/Y>"} (fork's externally tagged shape).
```

**Behaviour:**
- **`admin_sql_with`:**
  - sets `PGAPPNAME` when `app_name` is given;
  - uses `VERBOSITY=default` when `verbose_errors` is set, so the error text keeps `LINE`/`DETAIL`/`HINT`;
  - on timeout it returns `Err` containing `timed out after`.
- **`FakeBackend`:** journals `compute.sql {ep} {db} {sql}` exactly as `admin_sql` does, plus the timeout, so existing journal tests stay valid.
- **Internal spec route:** serves `mode` from the endpoint's `static_lsn`.
- **`wake_compute` and `access_control`:** use the routable-only lookups (T5), so a non-routable endpoint answers like an unknown one.

**Tests:**
- `admin_sql_with_passes_app_name_and_timeout` (fake: journal and timeout path);
- `admin_sql_is_admin_sql_with_defaults`;
- `static_mode_serializes_like_the_fork` (`{"Static":"0/16B3748"}`; the shape is pinned against the fork's `libs/compute_api/src/spec.rs` serde);
- `primary_spec_unchanged` (an existing spec snapshot test still passes);
- orchestrator `wake_compute_refuses_unroutable_endpoint`, `access_control_refuses_unroutable_endpoint`, `spec_route_serves_static_mode`.

**Commit:** `step 6g T6: admin_sql_with, static compute mode, unroutable endpoints`.

## Task 9: roles path extension (U1, U2)

**Files (modify):**
- `services/worker/src/{roles.rs, resync.rs, ports.rs, fakes.rs}`
- `services/orchestrator/src/roles.rs`
- `crates/chelabase-nats/src/lib.rs` (`RoleCreateRequest`, new `RoleUpdateRequest`, `SUBJECT_ROLE_UPDATE = "chelabase.rpc.roles.update"`)
- `crates/chelabase-metadata/src/neon.rs` and a migration `20261010000002_role_attributes.sql`
- `crates/chelabase-types/src/{role_name.rs, api_error.rs}`
- `crates/chelabase-routes/src/lib.rs`

**Migration:**

```sql
ALTER TABLE neon_roles
    ADD COLUMN login            boolean     NOT NULL DEFAULT true,
    ADD COLUMN inherit          boolean     NOT NULL DEFAULT true,
    ADD COLUMN connection_limit integer     NOT NULL DEFAULT -1,
    ADD COLUMN valid_until      timestamptz NULL,
    ADD COLUMN bypass_rls       boolean     NOT NULL DEFAULT false,
    ADD COLUMN member_of        text[]      NOT NULL DEFAULT '{}';
```

`password_enc` and the verifier stay as they are, and are NULL for NOLOGIN roles. Check the existing NOT NULL constraints, and relax them for NOLOGIN.

**Interfaces (produced):**

```rust
pub struct RoleSpec {
    pub name: String, pub owner_level: bool, pub login: bool, pub inherit: bool,
    pub connection_limit: i32, pub valid_until: Option<DateTime<Utc>>, pub bypass_rls: bool,
    pub member_of: Vec<String>,
}
pub struct RolePatch {   // every field optional; None = unchanged
    pub owner_level: Option<bool>, pub login: Option<bool>, pub inherit: Option<bool>,
    pub connection_limit: Option<i32>, pub valid_until: Option<Option<DateTime<Utc>>>,
    pub bypass_rls: Option<bool>, pub member_of: Option<Vec<String>>,
}
impl Worker { pub async fn create_role(&self, branch: BranchId, spec: &RoleSpec) -> RoleOutcome;   // replaces (name, owner_level)
              pub async fn update_role(&self, branch: BranchId, name: &str, patch: &RolePatch) -> RoleOutcome; }
```

**Rules:**
- **LOGIN roles:** get a generated password (as today).
- **NOLOGIN roles:**
  - no password and no verifier;
  - not added to the compute spec's role list (the T9 implementer confirms how compute_ctl treats spec roles and documents the choice);
  - created by `admin_sql` (`CREATE ROLE … NOLOGIN …`), the same way owner-level grants are applied today.
- **`member_of`:** each target must be an existing role on the branch that isn't reserved (`is_reserved_for`, plus `authenticator`, `anon`, `authenticated`, `chelabase_*`, `neon_superuser`, `pg_*`); otherwise `role_membership_not_allowed`.
- **Attributes outside U2 can't be expressed:** the request types have no such fields; unknown JSON fields are refused by `deny_unknown_fields`.
- **Turning `login` off** clears the sealed password. **Turning it on** generates one, revealable through the Connection panel (U3).
- **Resync:** API-made roles keep "control plane wins", now including the new attributes and memberships.
- **API:**
  - `POST …/branches/{bid}/roles` accepts the new optional fields, with today's defaults, so it stays backward compatible;
  - new `PATCH …/branches/{bid}/roles/{name}` (202, like reset), registered in `ENDPOINTS`;
  - responses include the attributes, never the password.

**Tests:**
- Worker, with fakes:
  - `create_nologin_role_has_no_password`;
  - `create_role_applies_attributes_and_memberships` (journal shows the exact `CREATE ROLE … NOLOGIN NOINHERIT CONNECTION LIMIT 3 VALID UNTIL '…' BYPASSRLS` and `GRANT "g" TO "r"`);
  - `membership_in_reserved_role_is_refused`;
  - `update_role_toggles_login_and_reseals`;
  - `update_role_bypass_rls`;
  - `resync_restores_api_role_attributes`.
- Orchestrator:
  - `create_role_accepts_new_fields`;
  - `create_role_refuses_unknown_attribute`;
  - `patch_role_is_202_and_publishes`;
  - `role_list_shows_attributes_without_password`.
- Live (`services/worker/tests/roles_live.rs`, ignored): `nologin_group_role_and_membership_on_a_real_compute`.

**Commit:** `step 6g T9: roles path: NOLOGIN, attributes, memberships, update`.

## Task 2: catalog reader

**Files:**
- Modify: `crates/chelabase-schema/src/{catalog.rs, exclusions.rs}`.
- Create: `crates/chelabase-schema/tests/catalog_live.rs`, `crates/chelabase-schema/tests/fixtures/full.sql`.

**Interfaces (produced):**
- `pub const CATALOG_SQL: &str`:
  - one script: `BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY; SET LOCAL search_path = pg_catalog; SELECT <one json document>; COMMIT;`;
  - output: exactly one line of JSON on psql's `-A -t -q` stdout.
- `pub fn parse_catalog(json: &str) -> Result<SchemaModel, SchemaError>`: refuses a server major other than 17; normalizes and sorts.

**Rules:**
- the pre-flight's PG17 catalog facts:
  - `attnotnull` is the NOT NULL source;
  - identity sequences (`pg_depend` type `i`) live on their column, and sequences owned by a column (type `a`) are standalone with `owned_by`;
  - indexes backing constraints (`conindid`) aren't listed as indexes;
  - `relacl` NULL becomes `acldefault('r', relowner)`, and column ACLs are `attacl` only when non-NULL;
  - `aclexplode` grantee 0 means PUBLIC;
  - policy `polroles = {0}` means PUBLIC;
  - enum values are ordered by `enumsortorder` (the raw value is never stored);
- `relkind` `p` and `f`, partitions (`relispartition`), views (`v`), materialized views (`m`), functions, procedures, triggers (non-internal), domains, composite types, extensions, rules, casts, operators, publications and user event triggers become `reported` entries, with their names and OIDs;
- the P6 exclusions are computed inside the SQL;
- roles come from `pg_roles`, never `pg_authid`.

**Tests:**
- Unit:
  - `parse_refuses_pg16` (`server_major 16` gives `UnsupportedServer(16)`);
  - `parse_sorts_and_normalizes`;
  - `unknown_attgenerated_is_an_error` (`'v'`).
- Live (`catalog_live.rs`, `#[ignore = "needs the metadata Postgres (just deps-up)"]`): scratch databases `cs_<12 hex>` on `CHELABASE_METADATA_DATABASE_URL` (default `postgres://chelabase:chelabase@127.0.0.1:5433/chelabase_metadata`). Cleanup uses the `Scratch::new()`/`finish()` pattern from `crates/chelabase-query/tests/engine_live.rs` with `DROP DATABASE … WITH (FORCE)`, and a stale sweep of `cs_%` databases older than 1 hour. Roles get unique names and are dropped in `finish`.
  - `fixture_reads_every_object_type`: `full.sql` covers every 6g type plus quoted names, a partitioned table, a view, a function and a trigger. The test asserts named entries: an identity column with its sequence parameters, a NOT VALID FK, a partial unique index with INCLUDE, a policy with USING and WITH CHECK, a column grant, a default privilege, a NOLOGIN role with membership, and the view, function and partitioned table in `reported`.
  - `two_reads_are_identical`.
  - `excluded_objects_are_absent`: the fixture also creates a `chelabase` schema with a table, a fake `chelabase.app_users_table` row naming a users table with managed columns, the reserved roles, and an extension (`pgcrypto` from contrib). None of them appears in the model.
  - `merge_leftovers_are_reported`: a check named `chelabase_merge_nn_…` becomes `MergeLeftover`.
- The round trip (`create_everything_round_trip`) is added after T4 merges (T13 owns it if T4 has finished).

**Commit:** `step 6g T2: catalog reader`.

## Task 3: three-way analysis, conflicts, integrity, resolutions, merged model

**Files:** modify `crates/chelabase-schema/src/analysis.rs`, with tests in the same file and fixtures built in Rust.

**Interfaces (produced):**

```rust
pub struct Analysis {
    pub items: Vec<Item>,           // every changed sub-object on either side, with its outcome
    pub conflicts: Vec<Conflict>,
    pub reported: Vec<ReportedChange>,   // source-side changes to reported objects + dependencies on them
}
pub struct Item { pub id: ItemId, pub key: ObjKey, pub source: Option<Change>, pub target: Option<Change>,
                  pub outcome: Outcome, pub data_loss: bool, pub security: bool }
pub enum Outcome { Apply, TargetOnly, Overlap, Conflict(ConflictId) }
pub enum Change { Create(ObjSnapshot), Drop(ObjSnapshot), Alter { before: ObjSnapshot, after: ObjSnapshot } }
pub struct Conflict { pub id: ConflictId, pub kind: ConflictKind, pub items: Vec<ItemId>,
                      pub base: Option<ObjSnapshot>, pub target: Option<ObjSnapshot>, pub source: Option<ObjSnapshot> }
pub enum ConflictKind { Edit, DropVsEdit, CreateCollision, ColumnOrder, Dependency }
pub enum Choice { KeepTarget, TakeSource, OwnSql(String), TargetColumnsFirst }
pub struct Resolution { pub conflict: ConflictId, pub choice: Choice }
pub struct MergedModel { pub model: SchemaModel, pub own_sql: Vec<(ConflictId, String)> }

pub fn analyze(base: &SchemaModel, target: &SchemaModel, source: &SchemaModel,
               map: &IdentityMap, dir: Direction) -> Analysis;
pub fn merge(analysis: &Analysis, resolutions: &[Resolution], base: &SchemaModel,
             target: &SchemaModel, source: &SchemaModel, map: &IdentityMap, dir: Direction)
             -> Result<MergedModel, MergeError>;   // MergeError::{Unresolved(Vec<ConflictId>), InvalidChoice{..}}
```

- **`ItemId` and `ConflictId`** are stable strings derived from the object key, e.g. `column:<base table oid>:<attnum>` or `table:name:<schema>.<name>` for newer objects. Resolutions survive a recompute when the same id appears again.

**Rules:** the spec's Part 3 table, and also:
- **Base objects are matched by OID** through `map` for each side; newer objects by qualified name.
- **A rename is an `Alter`** whose name changes.
- **An identical change on both sides is an `Overlap`.**
- **Column order:** both sides adding columns to one table is a `ColumnOrder` conflict.
- **Integrity of the merged model:**
  - every FK target, policy role, grant role, owned-by column and identity sequence must resolve, otherwise a `Dependency` conflict naming both items;
  - a merged default, check or policy expression that names a function only the source added (found by parsing the expression with pg_query and matching function names against `reported`) is a `ReportedChange` dependency, not a conflict.
- **Flags:**
  - `data_loss`: drop table or column, a type change that isn't a known-safe widening (e.g. `varchar(n)` → `varchar(m>n)` or `text`, `int4` → `int8`), drop NOT NULL → never;
  - `security`: a role that gains or is created with BYPASSRLS (P9).
- **`TakeSource` on `ColumnOrder`** is `InvalidChoice`. **`TargetColumnsFirst`** is valid only for `ColumnOrder`.

**Tests (one per rule):**
- **Outcomes:**
  - `source_only_change_applies`;
  - `target_only_change_is_ignored`;
  - `identical_changes_overlap`;
  - `different_changes_conflict`;
  - `drop_vs_edit_conflicts_both_ways`;
  - `same_name_creates_with_different_definitions_collide`;
  - `same_name_identical_creates_overlap`;
  - `both_sides_adding_columns_is_an_order_conflict`.
- **Identity:**
  - `rename_is_alter_not_drop_create`;
  - `set_schema_is_alter`;
  - `drop_and_recreate_same_name_is_drop_plus_create_with_data_loss`;
  - `identity_map_matches_objects_created_by_a_previous_merge`.
- **Integrity:**
  - `fk_to_column_dropped_on_target_is_a_dependency_conflict`;
  - `policy_naming_missing_role_is_a_dependency_conflict`;
  - `dependency_on_reported_function_is_listed` (Review Focus 4).
- **Directions:** `from_parent_direction_swaps_sides`.
- **Flags:**
  - `type_widening_is_not_data_loss`;
  - `bypass_rls_is_flagged_security`.
- **Resolutions:**
  - `merge_refuses_unresolved_conflicts`;
  - `keep_target_leaves_item_out`;
  - `take_source_uses_source_version`;
  - `own_sql_is_carried`;
  - `target_columns_first_appends_source_columns`;
  - `take_source_on_order_conflict_is_invalid`;
  - `ids_are_stable_across_recompute`.

**Commit:** `step 6g T3: three-way analysis and merged model`.

## Task 4: planner, phases, user steps, export, drift compare

**Files:** modify `crates/chelabase-schema/src/{plan.rs, steps.rs, export.rs, compare.rs}`, with tests in each file.

**Interfaces (produced):**

```rust
pub enum Phase { Roles, EnumAdd, Tx, Online }          // 0, 1a, 1, 2
pub enum StepKind { Generated, User, OwnSql }
pub struct Step { pub id: StepId, pub kind: StepKind, pub phase: Phase, pub sql: String,
                  pub items: Vec<ItemId>, pub data_loss: bool, pub rewrite: bool, pub security: bool,
                  pub locks: Vec<String>, pub role_op: Option<RoleOp>, pub online: Option<OnlineCheck> }
pub enum RoleOp { Create(RoleSpecModel), Update { name: String, patch: RolePatchModel } }  // mirrors T9's RoleSpec/RolePatch
pub enum OnlineCheck { IndexValid { schema: String, name: String }, ConstraintValidated { schema, table, name },
                       NotNullSet { schema, table, column } }   // what resume checks before re-running a step
pub struct UserStep { pub id: StepId, pub after: Option<StepId>, pub sql: String }
pub struct Plan { pub steps: Vec<Step> }
pub struct PlanOptions { pub lock_timeout_ms: u32, pub merge_id: Uuid }

pub fn plan(target: &SchemaModel, merged: &MergedModel, analysis: &Analysis,
            user_steps: &[UserStep], opts: &PlanOptions) -> Result<Plan, SchemaError>;
pub fn classify_steps(sql: &str) -> Result<Vec<ClassifiedStatement>, StepError>;   // P8, pg_query
pub fn phase1_script(plan: &Plan, opts: &PlanOptions, expected_target_fingerprint_sql: &str) -> String;
pub fn export_sql(plan: &Plan) -> String;
pub fn expected_model(target: &SchemaModel, merged: &MergedModel, default_acl: &SchemaModel) -> SchemaModel;  // P7
pub fn compare(expected: &SchemaModel, actual: &SchemaModel, own_sql_items: &[ItemId]) -> Vec<Drift>;
pub const MARKER_SQL: &str;   // CREATE SCHEMA IF NOT EXISTS chelabase_merge … ; INSERT INTO chelabase_merge.applied
```

**Rules:**
- **Order and phases:** the spec's Part 4 order and phase transforms. R5 decides which tables "existed on the target before the merge": present in `target`.
- **`phase1_script`:** `SET ROLE postgres; BEGIN; SET LOCAL lock_timeout …; SET LOCAL statement_timeout '15min';`, then a guard statement that raises if the target's live fingerprint doesn't match (computed in SQL by comparing a narrow catalog digest; T4 defines the digest SQL and T8b pairs it with the stored value), then the steps, then the `MARKER_SQL` insert, then `COMMIT`.
- **Phase 2 steps** are single statements with no `BEGIN`, each with its `OnlineCheck`.
- **User steps** are classified by `classify_steps` (P8). A concurrent statement inserted among phase 1 steps is moved into phase 2 at the same relative position. Refused statements give `SchemaError::Steps`.
- **Export:** `-- Phase 0: roles (create through the roles API; listed only)`, `-- Phase 1a`, `-- Phase 1: one transaction` wrapped in `BEGIN/COMMIT`, and `-- Phase 2: run each statement outside a transaction`. No passwords.
- **`compare`:** matches by name (the target's OIDs for new objects are new). It ignores `own_sql_items`, whose drift is reported as "produced" for confirmation.

**Tests:**
- **Order and quoting:**
  - `drops_before_creates_in_reverse_dependency_order`;
  - `renames_come_first`;
  - `fk_after_referenced_unique`;
  - `quotes_every_identifier` (Review Focus 1).
- **Phase transforms for tables that already exist on the target:**
  - `new_index_on_existing_table_is_concurrent_phase2`;
  - `new_table_gets_everything_in_phase1`;
  - `new_fk_is_not_valid_then_validated`;
  - `new_unique_uses_concurrent_index_then_using_index`;
  - `new_not_null_uses_named_check_then_set_not_null_then_drops_check`;
  - `changed_index_is_replaced_concurrently`;
  - `enum_value_used_in_plan_goes_to_phase1a`.
- **Roles:** `roles_are_phase0_role_ops`.
- **User steps:**
  - `user_step_is_placed_after_its_anchor`;
  - `concurrent_user_step_moves_to_phase2`;
  - `transaction_control_in_step_is_refused`;
  - `role_ddl_in_step_is_refused`.
- **Scripts and export:**
  - `phase1_script_has_guard_marker_and_timeouts`;
  - `export_has_phase_headers_and_no_passwords`.
- **Expected model and compare:**
  - `default_privileges_are_simulated_for_new_tables` (Review Focus 5);
  - `compare_finds_missing_column`;
  - `compare_ignores_own_sql_items`.

**Commit:** `step 6g T4: planner, user steps, export, drift compare`.

## Task 7: base snapshots

**Files:**
- Create: `services/worker/src/schema_base.rs`.
- Modify:
  - `services/worker/src/{branch_create.rs, branch_switch.rs, ports.rs, fakes.rs, lib.rs, reconcile.rs}`
  - `services/worker/Cargo.toml` (adds `chelabase-schema`)

**Interfaces (produced):**

```rust
impl Worker {
    /// Read `db` on `ep` (CATALOG_SQL through admin_sql_with, 60 s) and parse it.
    pub(crate) async fn read_schema(&self, ep: &str, db: &str) -> Result<SchemaModel, String>;
    /// Best effort (R6): read every non-system database of a ready branch and upsert its base.
    pub(crate) async fn record_bases(&self, branch: BranchId, ep: &str, source: BaseSource,
                                     timeline: &str, lsn: &str);
    /// The trusted base for (branch, db): stored if version-compatible, else read live through a
    /// static compute (Part 2), else Err(BaseUnavailable).
    pub(crate) async fn load_base(&self, branch: &BranchRecord, db: &str) -> Result<(SchemaModel, IdentityMap), BaseError>;
}
pub enum BaseError { Unavailable(String), Transient(String) }
```

**Rules:**
- **Hooks:**
  - **Create:** after `Built::Compute` in `branch_create_attempt`, before `finish_branch`, for normal branches only (not rehearsals): `record_bases(.., Create, timeline, branched_at_lsn)`.
  - **Reset:** the switch writes the reset LSN to `branched_at_lsn` (T5's new parameter). `finish_reset` then calls `record_bases(.., Reset, new timeline, reset lsn)`, which covers the resumed path too.
- **Databases:** `metadata.branch_databases(branch)`, minus `DatabaseOrigin::System`.
- **Live fallback:**
  - a `neon_endpoints` row with `routable = false` and `static_lsn = <lsn>` on the recorded timeline, and a new endpoint id and token (`provision::new_endpoint_id`, `new_compute_token`);
  - `computes.create`, `wait_until_ready` (read-only: `SELECT 1`), `read_schema`, store the model, then `computes.delete` and delete the endpoint row, always (also on error).
  - A Neon error saying the LSN is behind the GC horizon is `Unavailable`. Other errors are `Transient`.
  - **E17 narrowed (T13l, 2026-10-08; patch 4 shipped):** behind the timeline's GC cutoff, a location at the timeline's branch point (its ancestor LSN) is read on the ancestor timeline at that point, which retains it while the timeline exists (`Storage::ancestor`, from storcon's `ancestor_timeline_id`); any other such point is `Unavailable` (re-baseline). The stored entry keeps its own location.
- **Reconcile row:** an unroutable endpoint with `static_lsn` older than 10 minutes (R3) is torn down.
- **P6 constants test:** `exclusion_lists_match_worker_constants` asserts that `chelabase_schema::exclusions` contains `app_users_sql::MANAGED_COLUMNS` and `USERS_SYNC_ROLE`.

**Tests (with fakes):**
- `create_records_a_base_per_database`;
- `a_failed_base_read_does_not_fail_the_create`;
- `rehearsal_branches_record_no_base`;
- `reset_updates_branched_at_lsn_and_rebases`;
- `stored_base_of_current_version_is_used`;
- `old_version_base_is_read_live_through_a_static_compute` (journal: endpoint row unroutable with `static_lsn`, compute create, catalog read, compute delete, row delete);
- `static_compute_is_removed_on_read_error`;
- `gc_horizon_error_is_unavailable`;
- `a_branch_point_behind_gc_is_read_on_the_ancestor_that_retains_it` (T13l);
- `reconcile_removes_a_stale_static_compute`;
- `exclusion_lists_match_worker_constants`.

**Commit:** `step 6g T7: base snapshots and live fallback`.

## Task 7b: base history (E18; added 2026-10-06)

**Implementer:** opus. **Review:** sonnet. Runs alongside T8a.

**Files:**
- Create: `crates/chelabase-metadata/migrations/20261010000004_schema_base_history.sql`.
- Modify:
  - `crates/chelabase-metadata/src/schema_bases.rs`, `tests/live_db.rs`;
  - `services/worker/src/{schema_base.rs, branch_switch.rs, ports.rs, fakes.rs}`.

**Rules:**
- **History:** `branch_schema_bases` holds a history: one entry per write, no longer one row per (branch, database).
  - Each entry adds `branch_timeline_id` and `branch_lsn`: the branch's own position from which the base is valid.
    - For create and reset, the new timeline and its start LSN.
    - For a merge or update with keep (T8b), the kept branch's timeline and its LSN when the merge finished.
  - The current base is the newest entry by insertion order (a `bigserial` id).
  - Existing rows migrate as the first entries, positioned at the branch's current timeline and `branched_at_lsn`.
- **Writes:** `upsert_schema_base` becomes an append. The live-fallback fill (T7's "location without model, filled later") updates that same entry in place.
- **Restore:**
  - On a branch restore to LSN X from timeline T, the worker appends a copy of the newest entry with `branch_timeline_id = T` and `branch_lsn <= X`, including its model, map and location, positioned at the new timeline's start.
  - The copy's `source` stays the original's.
  - With no such entry, nothing is appended, and the merge offers re-baseline (E17).
  - The hook runs where the restore finishes, including the reconcile-finished path, like T7's reset hook.
- **Cleanup:** entries cascade with the branch. Entries of a database that no longer exists on the branch are deleted when the base is next recorded.
- **T7 review minor 3:** after a reset, a stale `branch_databases` row for a database that only existed on the old timeline makes the read fail. Treat Postgres "database does not exist" as skip-and-delete, not as a stored location-only base.
- **Interface for T8b:** `record_merge_base(branch, db, model, map, source: Merge | Update, location, branch_timeline, branch_lsn)`.
- **`load_base`:** unchanged for T8a (it reads the current entry).

**Tests:**
- Live (`live_db`):
  - `schema_base_history_appends_and_current_is_newest`;
  - `schema_base_at_or_before_picks_the_right_entry`;
  - `migration_keeps_existing_bases_as_first_entries`.
- Worker:
  - `restore_before_a_keep_merge_restores_the_older_base`;
  - `restore_after_a_keep_merge_keeps_it`;
  - `restore_with_no_entry_leaves_no_base`;
  - `reset_skips_a_database_missing_on_the_new_timeline`.

**Commit:** `step 6g T7b: base history and restore`.

## Task 13b: child-branch restore (E23; added 2026-10-06)

**Implementer:** opus (live debugging). **Review:** opus.

**The bug (B2, from step 5):** `BranchJob::Restore` on a child branch (e.g. `dev`, a child of main) fails at the detach. The storage controller rejects it with `400 Bad Request: "1 (pageserver): \"ancestor is not empty\""`. The branch is handed back `ready`, unchanged, with that error. `branch_live` only restored main.

**Root cause (investigated 2026-10-06):** the restore detaches with `multi_level_and_no_reparent`. For a child branch, the chain is restored timeline → old branch timeline → parent timeline. The pageserver's `prepare` refuses with `AncestorNotEmpty` unless the restore LSN equals the branch's own branch point.

~~**Rules (E24, user 2026-10-07: two detaches, no Neon change)**, with **E25** (the catch-up wait between them) and the grandchild refusal~~: **superseded by Neon fork round 2.** Fork patches 1 and 3 make one `multi_level_and_no_reparent` detach wait for each ancestor's WAL and copy every level, so restore keeps its one-detach flow at any depth (T13l). `schema-merge-t13b` (`d8ea1fa`) is not merged.

**Rules (T13l, 2026-10-08):**
- **One detach** of the restored timeline, in `services/worker/src/branch_switch.rs`, for main, a child or a grandchild alike; nothing refuses or special-cases depth. The old timeline (the backup's) stays where it was in the tree.
- **Recovery** is the existing one-detach path: a 503 from the detach (an ancestor still catching up, about 30 s per level, one level after another) or a timeout keeps the pending timeline and hands the job back; the redelivery detaches that timeline again. `STUCK_DETACH_SECS` (20 minutes of wall time since the pending timeline was recorded) needs no depth term.

**Tests:**
- Unit `a_grandchild_restore_takes_one_detach_and_retries_a_503`.
- Live `branch_live` `restore_a_child_branch` and `restore_a_grandchild_branch` (the old timeline keeps its ancestor and point).
- Live `merge_live` (E17 narrowed, GC sped up per tenant and the pageserver restarted so GC isn't held by the lease deadline): `a_branch_point_behind_gc_is_read_on_the_parent`, `a_keep_merge_base_behind_gc_is_unavailable`, `a_restored_branch_reads_its_base_until_the_backup_is_deleted`, `a_grandchild_branch_point_behind_gc_is_read_on_its_parent`.
- `merge_live` `restore_before_a_keep_merge_uses_the_older_base` (T13's test) passes.

**Commit:** `step 6g T13l: restore is one detach at any depth`.

## Task 13c: owner memberships in merges (E22; added 2026-10-06)

**Implementer:** opus. **Review:** opus.

**The bug (B1):** a role created in the owner's session makes `postgres` a member of it automatically (an ADMIN grant, plus one from `createrole_self_grant`). Merges then plan `role:update:postgres` (refused as reserved) or end with drift.

**Rules (E22, simulate):**
- Keep these memberships in the model.
- The analysis and the planner never plan changes to the owner's memberships.
- `expected_model` adds the automatic grants Postgres 17 makes for each role the merge creates in the owner's session, with the same grantor, admin, inherit and set options the live catalog shows. Check those on the stack.
- Drift still flags owner grants made by hand that the merge didn't produce.

**Tests:**
- Unit tests in `chelabase-schema` (analysis, plan, expected model).
- `merge_live` `nologin_group_role_and_login_role_created_through_control_plane` and the role half of E20 pass.

**Commit:** `step 6g T13c: owner memberships in merges`.

## Task 10: orchestrator merge API

**Files:**
- Create: `services/orchestrator/src/merges.rs`.
- Modify:
  - `services/orchestrator/src/{lib.rs, openapi.rs, branches.rs, roles.rs, console.rs, data_api_databases.rs, sql_editor/routes.rs}` (P14 `!= Normal` checks, rehearsals hidden)
  - `crates/chelabase-routes/src/lib.rs`
  - `services/orchestrator/tests/api.rs`
  - `openapi/management.json` and the generated clients (`just api-gen`)

**Routes** (paths per the spec's Part 6, under `/v1/databases/{id}`; operation ids are camelCase, checked by `operation_ids_are_the_contract`):

| Operation id | Route |
|---|---|
| `createMerge` | `POST …/branches/{branch_id}/merges` |
| `listMerges` | `GET …/merges` (query `branch`) |
| `getMerge` | `GET …/merges/{merge_id}` |
| `updateMerge` | `PATCH …/merges/{merge_id}` |
| `rehearseMerge` | `POST …/rehearse` |
| `applyMerge` | `POST …/apply` |
| `recomputeMerge` | `POST …/recompute` |
| `retryMerge` | `POST …/retry` |
| `cancelMerge` | `POST …/cancel` |
| `exportMergePlan` | `GET …/plan.sql` |

**Behaviour:**
- **`createMerge`:**
  - refused for main with `into_parent`, or for a branch without a parent, with `unprocessable`;
  - refused for a non-normal branch;
  - refused when either branch isn't ready (`branch_not_ready`);
  - an existing open merge gives `merge_open_exists` with `error.merge_id`;
  - otherwise it creates the `computing` row and publishes `MergeJob::Compute`. On a publish failure the row is marked `failed`, then the usual `WorkerUnavailable`.
- **`getMerge`:** returns the full record plus events. **`updateMerge`** maps `MergeWrite` (`Stale` gives 409 `merge_version_conflict` with `error.current`), and on `Saved` sets the status to `computing` and publishes `MergeJob::Compute`, so the worker recomputes the analysis and the plan. Request bodies use `deny_unknown_fields`.
- **`recomputeMerge`:** body `{ rebaseline?: bool }`, allowed from `draft`, `stale` and `failed` (`failed_phase` not `online`). It moves the merge to `computing` and publishes `Compute`. `rebaseline: true` is stored in `options.rebaseline`, which the worker reads (T8a).
- **`applyMerge`** refuses, in this order:
  1. a non-`rehearsed` merge: `merge_not_rehearsed`;
  2. open conflicts: `merge_conflicts_open`;
  3. unacknowledged data-loss or security items: `merge_drops_unacknowledged`;
  4. a protected target without `confirm: true`: `merge_confirm_required` (R1);
  5. another applying merge on the target: `merge_busy`.

  Then it publishes `Apply`.
- **`retryMerge`:** only `failed` with `failed_phase = online`. **`cancelMerge`:** only non-applying states.
- **`exportMergePlan`:** `text/plain; charset=utf-8`, `Content-Disposition: attachment; filename="merge-<id>.sql"`, `Cache-Control: no-store`. The OpenAPI response is `content = (String, content_type = "text/plain")`. The orval mutator's handling of non-JSON is checked; if needed, add a `responseType: 'text'` override or document a raw fetch helper `fetchMergePlanSql` in `packages/management-client/src/`.
- **Branch operations:**
  - reset, restore and delete of a branch with `branch_has_busy_merge` give 409 `branch_merge_busy` with `error.merge_id`;
  - deleting a branch also cancels its drafts;
  - `list_branches` hides rehearsals.

**Tests:**
- `create_merge_is_202_and_publishes_compute`;
- `create_merge_refuses_main_into_parent`;
- `create_merge_refuses_unready_branch`;
- `second_open_merge_is_409_with_id`;
- `update_merge_with_stale_version_is_409_with_current`;
- `apply_refusal_order` (one case per refusal);
- `apply_on_protected_needs_confirm`;
- `retry_only_after_online_failure`;
- `export_is_text_plain_attachment`;
- `branch_ops_refused_while_merge_busy`;
- `delete_branch_cancels_drafts`;
- `rehearsal_branches_are_not_listed`;
- `rehearsal_branch_refused_by_console_and_sql_editor`;
- the registry tests (`every_declared_orchestrator_endpoint_is_routed`, `document_matches_the_registry`, `document_is_current`).

**Commit:** `step 6g T10: merge API`.

## Task 8a: merge worker: jobs, claims, compute, rehearsal

**Files:**
- Create: `services/worker/src/merge/{mod.rs, compute.rs, rehearse.rs, claim.rs}`.
- Modify: `services/worker/src/{main.rs, lib.rs, ports.rs, fakes.rs, branch_create.rs}`. `branch_create_attempt` admits `Rehearsal` (today it refuses non-normal at `:110`) and skips the app-users bootstrap and reconcile for it; the target's copy already has them.

**Interfaces (produced):**
- `impl Worker { pub async fn handle_merge_job(&self, job: MergeJob, attempt: Attempt) -> Result<(), String> }`, wired in `main.rs` with `run_merge_consumer`.
- `keep_merge_claimed(id, status, work)`: touches `claimed_at` every `claim_refresh`, in the style of `keep_claimed`.

**Behaviour:**
- **Compute** (`computing` → `draft`):
  1. `load_base` for the branch, giving the base and identity map. `BaseError::Unavailable` gives `draft` with `error = merge_base_unavailable` and no analysis; the API's recompute with `rebaseline: true` (T10 adds that body field) uses the target's schema as the base and sets `rebaselined`.
  2. `read_schema` of the source and the target.
  3. `analyze`, store the analysis and the three fingerprints.
  4. If no conflicts are open, `merge` and `plan` are run and stored. With user steps and resolutions from the record, the plan is recomputed on each `updateMerge`; the orchestrator publishes `Compute` after a draft save, so the worker recomputes `analysis` and `plan` and keeps any resolution whose id still exists.
- **Rehearse** (`rehearsed` requires the latest draft, with no open conflicts and a plan):
  1. `create_rehearsal_branch` from the target's head (P14 name), then reuse `branch_create_attempt`, which skips the base and users steps for rehearsals.
  2. Run the phases on the rehearsal endpoint. Phase 0 runs as SQL with `NOLOGIN`, no password and no metadata (P5); phase 1a, 1 and 2 use T8b's executor in rehearsal mode.
  3. `read_schema` and `compare` against `expected_model`.
  4. Store the rehearsal (per-step timings, lock waits from `pg_stat_activity` sampling is **not** done; timings only), the drift, and the own-SQL outcomes.
  5. Tear down the rehearsal: `teardown_branch_parts` and `mark_branch_deleted`, always.
  6. A failure gives `draft`, with `rehearsal.failed_step` and the Postgres error text.
- **Cancel:** sets `cancelled` and tears down a running rehearsal.

**Tests (fakes):**
- **Compute:**
  - `compute_stores_analysis_and_fingerprints`;
  - `compute_without_base_offers_rebaseline`;
  - `rebaseline_uses_target_as_base`;
  - `recompute_keeps_resolutions_whose_ids_remain`.
- **Rehearse:**
  - `rehearse_creates_unroutable_rehearsal_and_tears_it_down` (journal order);
  - `rehearse_refused_with_open_conflicts`;
  - `rehearsal_phase0_uses_sql_without_password`;
  - `rehearsal_failure_returns_to_draft_with_step_and_error` (Review Focus 4);
  - `rehearsal_drift_is_recorded`.
- **Claims:**
  - `claim_is_refreshed_while_rehearsing`;
  - `cancel_tears_down_a_running_rehearsal`.

**Commit:** `step 6g T8a: merge jobs, compute and rehearsal`.

## Task 8b: merge worker: apply, online resume, finish, reconcile

**Files:**
- Create: `services/worker/src/merge/{apply.rs, online.rs, finish.rs}`.
- Modify: `services/worker/src/{reconcile.rs, fakes.rs}`.

**Interfaces (produced):** `pub(crate) async fn run_phases(&self, target: &Target, plan: &Plan, opts: &PlanOptions, mode: RunMode) -> Result<PhaseReport, PhaseFailure>`, with `RunMode::{Rehearsal, Real}`; T8a's rehearsal uses it.

**Behaviour (Real):**
- **Order:** the apply job moves the merge `rehearsed` → `applying`, under a claim. The phases then run in order:
  1. **Phase 0:** `create_role` or `update_role`; an existing role with the same spec is skipped (idempotent).
  2. **Phase 1a:** each enum addition in its own `admin_sql_with` call.
  3. **Phase 1:** `phase1_script` with the stored target fingerprint, `app_name` `chelabase-merge-<id>` and the P3 timeouts.
     - A guard mismatch rolls back, gives `stale` and changes nothing.
     - Any other failure gives `failed` with `failed_phase = tx`.
  4. **Then** `online` and phase 2: each step checks its `OnlineCheck` first (skip when already done; an invalid index is dropped concurrently, then rebuilt), and logs a `merge_events` step.
     - A failure gives `failed` with `failed_phase = online`; `retry` resumes.
     - On an admin timeout, cancel the backend by `application_name` (P3).
  5. **Then** `finishing`.
- **Finish:**
  1. `read_schema` the target and `compare` with `expected_model`. Drift gives `applied_with_drift`, with the difference in `result`.
  2. Record the identity map for objects the merge created (matched by name in the target's re-read; the source OIDs come from the source model).
  3. The after-merge choice:
     - **keep:** a new base with source `merge`, or `update` for `from_parent`, holding the source model that was read, its timeline and the LSN at read time (`resolve_point` head). The identity map is updated.
     - **reset:** the existing reset flow.
     - **delete:** the existing delete flow. Its refusals (children) are recorded in `result`, and the merge is still `done`.
  4. `done`, with `result.created_roles` (names and the branch they were created on, for the Connection panel link, U3).
- **Recovery after a crash** (redelivery, or reconcile re-queue as `Resume`):
  - in `applying`: if `chelabase_merge.applied` has the merge id, go to phase 2; otherwise back to `rehearsed`, with "interrupted, nothing applied";
  - in `online`: resume phase 2;
  - in `finishing`: redo finish (idempotent).
- **Reconcile rows:**
  - re-queue merges whose `claimed_at` is older than 120 s in a working status;
  - delete `rehearsal` branches older than 30 minutes whose merge isn't `rehearsing` (R3).

**Tests (fakes):**
- **Happy path:** `apply_happy_path_journal_order` (roles → enum → tx with guard and marker → online steps → re-read → base → done).
- **Phase 0:**
  - `phase0_is_idempotent`;
  - `security_role_created_through_create_role_with_bypass_rls`.
- **Phase 1:**
  - `phase1_recheck_mismatch_rolls_back_and_marks_stale` (Review Focus 2);
  - `phase1_failure_marks_failed_tx`.
- **Phase 2 and resume:**
  - `resume_drops_an_invalid_index_and_rebuilds` (Review Focus 3);
  - `online_step_timeout_cancels_the_backend`;
  - `online_failure_then_retry_continues_after_last_ok_step`.
- **Crash recovery:**
  - `crash_after_commit_resumes_from_marker`;
  - `crash_before_commit_returns_to_rehearsed`.
- **Finish:**
  - `drift_marks_applied_with_drift`;
  - `keep_writes_new_base_and_identity_map`;
  - `reset_after_merge_runs_reset`;
  - `delete_after_merge_refused_by_children_is_recorded`.
- **Reconcile:**
  - `reconcile_requeues_silent_merges`;
  - `reconcile_deletes_old_rehearsals`.

**Commit:** `step 6g T8b: merge apply, resume and finish`.

## Task 11: Studio

**Files:**
- Create:
  - `apps/studio/src/routes/app.projects.$projectId.databases.$databaseId.merges.$mergeId.tsx`
  - `apps/studio/src/components/merges/{merges-list.tsx, merge-page.tsx, merge-changes.tsx, merge-conflicts.tsx, merge-plan.tsx, merge-rehearsal.tsx, merge-apply.tsx, merge-finish.tsx}`, each with a `*.test.tsx`
  - `apps/studio/src/lib/merges.ts` (status labels, busy predicate, refusal-reason text) with `merges.test.ts`
- Modify:
  - `apps/studio/src/components/{project-page.tsx, branch-actions.tsx}` and `apps/studio/src/lib/branches.ts`
  - `apps/studio/src/test/render-router.tsx`
  - `apps/studio/src/routeTree.gen.ts` (generated)
  - the role list component (P13: show the new attributes read-only)

**Behaviour:**
- **Branch actions:**
  - `ActionKey` gains `mergeInto` and `updateFrom`;
  - `actionReasons(branch, all, openMerges)` returns a reason for main, for a branch that isn't ready, for a non-normal branch, and for an existing open merge (the reason links to it).
- **Merges view** (`DatabaseView` `"merges"`): a list with status, direction, source/target and dates.
- **Merge page:** polls with `pollWhile` while the status is `computing`, `rehearsing`, `applying`, `online` or `finishing`. Its sections follow the spec's Part 6:
  - **Changes:** grouped by object type, each with apply, overlap, conflict or target-only; base, target and source side by side as read-only text panes.
  - **Conflicts:** radio choices. Own SQL uses `CodeEditor` with `sqlExtensions(pglsClient)`, fed by `useGetSqlSchema` for the target branch (lifecycle copied from `sql-editor.tsx`).
  - **Plan:** steps grouped by phase; data-loss and security badges with acknowledgement checkboxes; insert, move or delete own steps; lock timeout.
  - **Rehearsal:** timings, the failing step with the Postgres error, own-SQL outcomes.
  - **Apply:** progress from `merge_events`; confirm for a protected target.
  - **Finish:** the after-merge choice, created roles linking to the Connection panel (U3), export download.
- **Saves:** send `version`. A 409 `merge_version_conflict` reloads and shows "changed elsewhere".
- **Design:** follow `PRODUCT.md`'s principles (risky states visible: data loss, security and protected target in plain words), keyboard paths and screen-reader names, and the existing Tailwind and `ui/button`/`ui/dialog` kit. No new npm dependencies.

**Tests (vitest + fake-fetch):**
- **Branch actions:**
  - `merge_actions_disabled_with_reasons`;
  - `open_merge_reason_links_to_it`.
- **Merges list and page shell:**
  - `merges_list_renders_history`;
  - `merge_page_polls_while_busy_and_stops`.
- **Changes and conflicts:**
  - `changes_grouped_by_type_with_three_panes`;
  - `conflict_choice_saves_with_version`;
  - `version_conflict_reloads`;
  - `own_sql_editor_saves_text`.
- **Plan:**
  - `acknowledgements_required_before_apply_button_enables`;
  - `user_step_insert_and_move`.
- **Rehearsal and apply:**
  - `rehearsal_failure_shows_step_and_error`;
  - `apply_on_protected_target_asks_confirm`.
- **Finish:**
  - `created_roles_link_to_connection_panel`;
  - `export_downloads_sql`.
- **Roles:** `role_list_shows_new_attributes`.
- **Gates:** `pnpm --filter @chelabase/studio test`, `typecheck`, `build`.

**Commit:** `step 6g T11: Studio merge pages`.

## Task 12: docs

**Files:**
- Create: `apps/docs/content/docs/using/merging-schema.mdx` and `apps/docs/content/docs/architecture/schema-merge.mdx`.
- Modify:
  - `using/meta.json` (after `branches`) and `architecture/meta.json` (after `sql-editor`);
  - `reference/errors.mdx` (P10 codes and role codes);
  - `using/login-roles.mdx` (U1 attributes, NOLOGIN, memberships, PATCH).

**Content:**
- **Using:** the workflow (merge into parent, update from parent), conflicts and choices, own SQL and steps, rehearsal, the online phase, after-merge choices, export, what isn't merged in 6g, re-baseline.
- **Architecture:** OID identity and the identity map, base snapshots and the live fallback, the three-way rules, phases and recovery (the marker and resume), rulings P1–P4 in prose.
- **Style:** no history markers and no Supabase comparisons.

**Verification:** `pnpm --filter @chelabase/docs typecheck`, `build`, `test`.

**Commit:** `step 6g T12: docs for schema merge`.

## Task 13: live scenarios, round trip, merge-e2e

**Files:**
- Create: `services/worker/tests/merge_live.rs` and `scripts/merge-e2e.sh`.
- Modify: `justfile` (`merge-e2e: _live-check`, then `scripts/merge-e2e.sh`, with a comment line above), and `crates/chelabase-schema/tests/catalog_live.rs` (the `create_everything_round_trip` test, if T4 didn't add it).

**Tests:**
- **Round trip:** `create_everything_round_trip`. Apply `full.sql` to scratch database A, read it, plan "create everything" from an empty model, apply that to scratch database B, read B; the two models are equal, modulo OIDs.
- **Live scenarios** (`#[ignore]`, run through `just live`, using `tests/common`):
  - `merge_into_parent_end_to_end`;
  - `update_from_parent_end_to_end`;
  - `rename_on_branch_with_new_column_on_main`;
  - `each_resolution_choice`;
  - `not_null_fails_in_rehearsal_then_passes_with_backfill_step`;
  - `nologin_group_role_and_login_role_created_through_control_plane`;
  - `protected_target_needs_confirm`;
  - `keep_reset_delete_after_merge`;
  - `second_merge_carries_only_newer_changes`;
  - `concurrent_ddl_on_target_makes_the_merge_stale` (Review Focus 2);
  - `new_table_in_served_schema_has_no_drift` (Review Focus 5);
  - `rehearsal_endpoint_is_not_routable` (psql through the proxy is refused).
- **`scripts/merge-e2e.sh`:** mirrors `scripts/sql-editor-e2e.sh`. It creates a database and a branch, changes the schema on both sides with one conflict, then runs create merge → resolve → rehearse → apply through the gateway API, and checks main's schema with psql. Cleanup unless `KEEP=1`.
- **Run:** `just live cargo nextest run -p worker --run-ignored ignored-only -E 'binary(merge_live)'`, `just live cargo test -p chelabase-schema --test catalog_live -- --ignored --test-threads=1`, then `just live merge-e2e`.

**Commit:** `step 6g T13: merge live scenarios and e2e`.

## Task 14: end-of-step review

Whole-branch review (opus) against the spec, U1–U3 and P1–P14, covering the deferred minors and the unreviewed tasks (T12, T13). Then one fix batch (sonnet) and a scoped re-review (opus). Record the findings in the Status.

## Task 15: verification

- `cargo test --workspace` (or `just test`) and `cargo clippy --workspace --all-targets -- -D warnings`.
- `just check`.
- Live, one at a time through `just live`:
  - the metadata live tests;
  - `catalog_live`;
  - `branch_live`;
  - `roles_live`;
  - `merge_live`;
  - `just merge-e2e`, `just branch-e2e`, `just data-e2e`, `just app-users-e2e`, `just sql-editor-e2e`;
  - Studio `test`, `typecheck`, `build`;
  - the docs build and test.
- Playwright pass on the dev stack (Playwright MCP; pnpm, never npx):
  1. create a branch;
  2. add a column on the branch and change the same column on main;
  3. merge into main and resolve the conflict;
  4. insert a backfill step;
  5. rehearse, apply, keep;
  6. check main in the Tables view;
  7. update from parent.
- Record the test data left behind in the Status.

## Verification commands (summary)

- Crate: `cargo test -p chelabase-schema`; live `just live cargo test -p chelabase-schema --test catalog_live -- --ignored --test-threads=1`
- Worker: `cargo test -p worker`; live `just live cargo nextest run -p worker --run-ignored ignored-only -E 'binary(merge_live)'`
- Orchestrator: `cargo test -p orchestrator`
- Metadata: `just live cargo test -p chelabase-metadata --test live_db -- --ignored --test-threads=1`
- API docs: `just api-gen` then `just check`
- Studio: `pnpm --filter @chelabase/studio test`, `typecheck`, `build`
- Docs: `pnpm --filter @chelabase/docs typecheck`, `build`, `test`
- E2E: `just live merge-e2e`
