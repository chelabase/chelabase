# Step 6g T15: diagnosis of failures 2, 5 and 6

Branch `schema-merge` at `a772030`, stack on the pinned Neon images `c8d1bb68b81f-dev`, run 2026-10-08. Diagnosis only: no product code or test was changed. The temporary test edits described below were reverted (`git status` is clean apart from this file). No images were built.

## Failure 2: `branch_live restore_a_child_branch` (`services/worker/tests/branch_live.rs:113`)

**Root cause: the test is wrong, and in two ways.** Step 5b (T14 M8, added in F2 `33d28ad`, lines 548-561) has two problems.

1. **It restores a branch that has a live child.** `dev-child` is created at line 538, and 5b restores `dev` through `restore()` (line 399), whose `Story::claim` asserts `live_children(dev)` is empty (line 113). The product refuses this too, on purpose and in both layers:
   - API: `claim()` in `services/orchestrator/src/branches.rs:688-706` returns 409 `has_children` ("cannot restore dev: it has child branches (dev-child); delete them first") for delete, reset and restore alike (callers at lines 791, 895, 977).
   - Worker: `branch_switch.rs:683-697` gives the job back with the same sentence if a child appeared after the API's check.
   - Documented: `apps/docs/content/docs/using/branches.mdx:50` ("Delete, reset-to-parent and restore are refused (`has_children`) while the branch has live normal child branches … Backup branches never count as children"), and `reference/errors.mdx:61`.
   - T13l's "restore at any depth" (plan, Task 13l rules) is about the **depth of the restored branch** (main, a child or a grandchild, one `multi_level_and_no_reparent` detach), not about a branch that **has** children. Nothing in the spec or T13l asks to detach and carry children (that is hosted Neon's behaviour, which the spec only describes, line 18). So the product refuses, and the test contradicts its own helper.
2. **`before_dev` isn't before dev's history**, so even without the child the expected refusal doesn't come. I confirmed this with a temporary edit (reverted) that deleted `dev-child` before 5b and printed the inputs:
   ```
   DIAG before_dev=2026-10-08 07:16:10.226 UTC found=Ok(("0/1F85F70", "Present")) branched_at_lsn=Some("0/1F7C998") history_start=Some(2026-10-08T07:16:08.211681Z)
   DIAG after: error=None timeline_changed=true rows=A
   ```
   `history_start` is the commit time at the branch point (A's commit; `history_start_at`, `services/worker/src/point.rs:72`, stored by `crates/chelabase-metadata/src/branches.rs:958-959`). `noted_time()` puts `before_dev` 2 s **after** that commit (line 472), so the time is inside dev's history: the API's `within_history` accepts it (`branches.rs:293-300`, `time < history_start` only). The worker resolves it to `0/1F85F70`, which is past the branch point `0/1F7C998`, so the M8 guard (`branch_switch.rs:266-280`, `lsn < branched_at_lsn`) correctly doesn't fire. The restore ran and dev holds `A`, which is dev's state at that time. That's correct behaviour, not an M8 hole.

**Category: test bug.** **`main`:** not affected (`restore_a_child_branch`, step 5b and M8 are all 6g only; `main`'s `branch_live.rs` has neither).

**What a correct test asserts:** a restore point that is really before dev's history, on a dev with no live child. Then the job ends with dev `ready`, `error` containing `before dev's history begins`, the timeline unchanged, and the rows unchanged.

**Fix options:**

1. **(Recommended)** Move 5b before `dev-child` is created (or delete `dev-child` first), and restore to **an LSN one below `dev.branched_at_lsn`** (read from the row). It's deterministic and mirrors the unit test `a_restore_before_the_branch_point_is_refused` (`branch_switch.rs:2771`). Expect the exact message `the LSN … is before dev's history begins (its branch point, …)`. Optionally keep a time variant using a time noted **before `CREATE TABLE t`** (before A's commit, so before `history_start`). The worker reaches its guard only if its lookup resolves below the branch point. I didn't run that variant.
2. Keep the time point but take it before A's commit, and assert either refusal message (`resolving the point: … before dev's history begins`). It's closer to what a user sends, but whether the worker's refusal comes from the M8 LSN guard or from `too_old` depends on the pageserver's timestamp lookup on the detached timeline. Less deterministic.
3. Drop 5b from the live test and rely on the unit test plus the API's 422. It's the cheapest, but loses the only live check that a detached (restored) timeline still refuses pre-branch-point LSNs.

Also, the scenario that 5b's comment describes ("its restored timeline carries main's older history") is real: after the first restore, dev's timeline is detached and holds main's history below the branch point. Option 1 is the precise live check for it.

**Confidence:** high for both causes (code, docs and a live diagnostic run). I didn't run option 1's corrected test.

## Failure 5: `merge_live a_keep_merge_base_behind_gc_is_unavailable` (`services/worker/tests/merge_live.rs:2565`)

**Root cause: the pageserver's on-disk position, not a lease or a retain point.** `Timeline::gc` caps the space cutoff at `disk_consistent_lsn` (fork `pageserver/src/tenant/timeline.rs:6866`, `let space_cutoff = min(gc_info.cutoffs.space, self.get_disk_consistent_lsn())`; then `new_gc_cutoff = space_cutoff.min(time_cutoff)`, line 6888). GC can never move the cutoff past WAL that is still only in the open in-memory layer.

The test's sequence makes `disk_consistent_lsn` equal the base point exactly:

1. The keep merge records dev's base at dev's head at the merge (`0/1FC0FA0`); nothing writes to dev after that.
2. `fast_gc` (`merge_live.rs:2469-2536`) restarts `chelabase-pageserver-1`. The graceful shutdown flushes the open layer, so dev's `disk_consistent_lsn` becomes its `last_record_lsn` at shutdown, which is the base point.
3. After the restart the churn (`wait_gc_past`) writes into a new in-memory layer. Nothing flushes it within the 300 s `GC_WAIT`: the stack runs the default `checkpoint_distance` (256 MiB; the churn was observed writing about 5 MiB per 9 s, so roughly 170 MiB in 300 s) and the default `checkpoint_timeout` (10 min). The test's tenant patch sets neither.
4. So every GC iteration computes `new_gc_cutoff = 0/1FC0FA0` and logs "Nothing to GC".

**Evidence (live, while the test was in `wait_gc_past`):**

- Pageserver `GET /v1/tenant/13fdc06c…/timeline/ac3f109e…` (dev): `applied_gc_cutoff_lsn` `0/1FC0FA0`, `disk_consistent_lsn` `0/1FC0FA0`, `remote_consistent_lsn_visible` `0/1FC0FA0`, `last_record_lsn` `0/30F60B0` (and climbing), `ancestor_lsn` `0/1F7AAA0` (dev's branch point, which isn't the base point).
- Pageserver log, every second: `find_gc_cutoffs{timeline_id=ac3f109e…}: … cutoffs=GcCutoffs { space: 0/35E0A10, time: Some(0/35E0A10) } applied_cutoff=0/1FC0FA0` followed by `gc_timeline{… cutoff=0/1FC0FA0}: Nothing to GC: new_gc_cutoff_lsn 0/1FC0FA0`. The time and space cutoffs are far past the point; only the disk-consistent cap holds it.
- The failure reproduced identically (`GC didn't pass 0/1FC0FA0 within 300s: [("ac3f109e…", Some(Lsn(33296288)))]`, 317 s).
- **Confirmation run:** with `"checkpoint_timeout": "1s"` added (temporarily) to `fast_gc`'s tenant patch, the test passed in 30 s: `GC passed 0/1FC0FA0 after 11.36s`, then the merge stayed a draft with `merge_base_unavailable` and no static compute (the test's own assertions). The edit was reverted.

**Ruled out:**

- *Patch 4's lease at the exact LSN:* leases don't hold the cutoff. `gc()` passes `max_lsn_with_valid_lease` to `gc_timeline` only to keep layers (line 7055); `new_gc_cutoff` doesn't depend on it. Note that `StorconClient::lsn_by_timestamp` asks for `with_lease=true` (`crates/chelabase-neon/src/storcon.rs:324`), so leases are taken, but they can't explain a pinned cutoff.
- *`retain_lsns` (a child, the rehearsal or a restore backup):* they also only keep layers, never cap the cutoff; and dev has no children here (the rehearsal branched from main and was deleted).
- *A standby horizon (the only other cap, line 6889-6905):* none is reported (static computes don't stream from safekeepers), and the cap that applied was the disk-consistent one, as the "space" value in the log shows.
- *The static compute:* none runs during the wait.

**Why the other GC tests pass:** in `a_branch_point_behind_gc_is_read_on_the_parent` and the grandchild and restore tests, the point is a branch point, and both timelines had writes after it before the restart (`m` on main, `d` on dev), so the restart's flush leaves `disk_consistent_lsn` past the point and the cutoff can pass it. The keep test is the only one whose point is the timeline's head at the restart.

**Category: test bug** (a timing premise). The product behaviour is right: GC can't collect WAL that isn't on disk yet, and in production `checkpoint_timeout` (10 min) or `checkpoint_distance` flushes it, after which GC passes the point and the base becomes unavailable as the test expects. A keep base on a branch that stays idle stays readable until new writes are flushed past it, which is correct (and harmless). Nothing in the fork needs changing.

**`main`:** not affected. The test and E17's narrowed fallback are 6g only (T13l).

**Fix options:**

1. **(Recommended)** Add `"checkpoint_timeout": "1s"` to `fast_gc`'s tenant patch (`merge_live.rs:2492-2497`), with a doc-comment line saying why (GC can't pass `disk_consistent_lsn`). Verified: passes in 30 s. It makes every GC test independent of the restart's flush, and faster.
2. Write to dev (`CHURN`) before `fast_gc` in this test, so the restart flush leaves `disk_consistent_lsn` past the point. One line, but it leans on the shutdown flush, which is the same coincidence the other tests rely on.
3. Force a flush with the pageserver's `PUT …/checkpoint`. Not viable: it's a `testing_api_handler` route (fork `pageserver/src/http/routes.rs:4134`), absent from release images.

**Confidence:** high. Mechanism read in the fork source, observed live, and the fix confirmed by a passing run.

## Failure 6: `just sql-editor-e2e`, `ERROR: tuple concurrently updated`

**What failed.** Database `e9a70227-…` (`sql_editor_e2e_7b66`, endpoint `ep-70eb7cbc22cc`) at provision. Worker log `.chelabase/dev/worker.log:549-556`:

```
06:49:20.395 provisioning database e9a70227…
06:49:27.130 provision failed: bootstrapping the data API: psql in compute ep-70eb7cbc22cc exited with 3:
  NOTICE: event trigger "chelabase_schema_version_ddl" does not exist, skipping
  NOTICE: event trigger "chelabase_schema_version_drop" does not exist, skipping
  NOTICE: event trigger "chelabase_extension_functions" does not exist, skipping
  ERROR:  tuple concurrently updated
06:49:28.797 touched-roles resync: syncing login roles: the branch has no compute (e9a70227…)
```

**The statement.** psql prints NOTICEs and the ERROR in order, so the failing statement comes after the three `DROP EVENT TRIGGER IF EXISTS` at `services/worker/src/data_api.rs:388-390` (the quarantine block's drops at 312-314 can't run on a fresh database), and it's one of lines 391-419. Of those, only these update an existing catalog row:
- `ALTER ROLE authenticator LOGIN NOINHERIT PASSWORD …` (line 394; `pg_authid`, shared across databases);
- the `ALTER DEFAULT PRIVILEGES FOR ROLE <owner>` (395-401; the owner's own `pg_default_acl` rows);
- `GRANT USAGE ON SCHEMA public …` (402; `pg_namespace`).

The `CREATE EVENT TRIGGER`s insert new rows, and the `ON ALL TABLES` grants find nothing in a new database. "tuple concurrently updated" is Postgres's `simple_heap_update` error. The statement updated a catalog row that another transaction had updated and not committed yet. It waited, the other transaction committed, and the update failed. ALTER ROLE takes no lock that would serialize this.

**The concurrent writer: compute_ctl's background migrations** (most likely; see confidence below).
- The worker treats the compute as ready once `SELECT 1` succeeds in the new database (`services/worker/src/provision.rs:275-306`, doc comment: "the compute has applied its spec once the new database answers"). Then it runs `bootstrap_sql` (`CREATE ROLE anon/authenticated/authenticator`, `GRANT anon, authenticated TO authenticator`, autocommit; lines 24-46) and then the data API script (lines 255-262).
- compute_ctl creates the database in its `CreateAndAlterDatabases` phase, then runs more phases, sets `Running` and **then spawns the migrations in a background task** "to not hold up cold starts" (fork `compute_tools/src/compute.rs:1916-1941`). Each migration is one transaction (`compute_tools/src/migration.rs`, `run_migration`).
- Migration **0002** (`compute_tools/src/migrations/0002-alter_roles.sql`) runs `ALTER ROLE %I NOBYPASSRLS` for **every** role that isn't a member of `neon_superuser` and isn't `pg_*`. That includes `authenticator` (and `anon`, `authenticated`) if `bootstrap_sql` already committed them. It also runs `ALTER ROLE %I INHERIT` for neon_superuser's members (the owner).
- A healthy provision measured on this stack (compute `ep-12a21c8121aa`, `docker logs`): the database exists at about `07:06:40.92`, compute_ctl reports `Running` at `41.026`, migration 1 runs at `41.050`, migration 2 at `41.082-41.101`, and the migrations end at `41.17`. In that run, migration 2's NOBYPASSRLS loop found no Chelabase roles: they were created after it. The race needs the opposite order. The worker's first successful `SELECT 1` lands in the ~100 ms between the database's creation and the migrations, `bootstrap_sql` commits before migration 2's loop, and the data API script's `ALTER ROLE authenticator` hits while migration 2's transaction is open. It's a narrow window, which fits a one-off flake that passed on rerun.
- **Mechanism reproduced** on a throwaway `postgres:17.11-alpine` container (removed). Session A ran `BEGIN; DO $$ … ALTER ROLE authenticator NOBYPASSRLS … $$; SELECT pg_sleep(2); COMMIT;`. Session B ran `BEGIN; ALTER ROLE authenticator LOGIN NOINHERIT PASSWORD 'p'; COMMIT;` during A's sleep and got `ERROR: tuple concurrently updated`.

**Ruled out:**
- A second bootstrap or a reconcile bootstrap: both take `pg_advisory_xact_lock(BOOTSTRAP_LOCK_KEY)` first (line 248), so they serialize. Reconcile logged nothing for this database.
- The touched-roles resync (patch 5's DDL receiver, `services/orchestrator/src/internal/ddl.rs:700-740`, `TOUCHED_RESYNC_DELAY` 3 s, `services/worker/src/resync.rs:174`, `1096-1108`): it ran at 28.797, after the failure (it found the endpoint gone). It's a symptom: some role DDL committed on that compute at about 25.8, from `bootstrap_sql` or migration 0002. It isn't the writer.
- E33a's `sync_key_marks_at_start`: it runs at worker start, and the worker had been up for about 50 minutes.
- compute_ctl's own spec phases: `ChangeSchemaPerms`' `ALTER SCHEMA public OWNER` only fires when `public` is owned by `cloud_admin`/`zenith_admin` (on PG17 it's `pg_database_owner`), and `default_grants.sql` writes cloud_admin's `pg_default_acl` rows, not the owner's. The role phases finish before the database exists.

The same class of race exists on `bootstrap_sql`'s `ALTER ROLE <owner> SUPERUSER` (when `owner_superuser`) against 0002's `ALTER ROLE <owner> INHERIT`. It also exists whenever a newer compute image adds a migration that touches roles, which then runs on every compute's first start after the upgrade.

**Category: product bug** (a readiness race in provision, which predates 6g).

**`main`:** affected. `main`'s provision has the same `SELECT 1` readiness and the same order (the 6g diff in `provision.rs` only adds the exited-container check and `SET search_path`). `main`'s data API script has the same `ALTER ROLE authenticator` (`git grep` on `main`: `services/worker/src/data_api.rs:393`). The compute_ctl migrations are the same in both images. 6g didn't introduce it.

**Fix options:**

1. **(Recommended, the root fix)** In the fork, run `handle_migrations` before compute_ctl reports `Running` (`compute_tools/src/compute.rs:1916-1941`: await it instead of spawning). In the worker, make `wait_until_ready` wait for compute_ctl's status `running` as well as `SELECT 1` (compute_ctl's `/status`: its external HTTP port 3080 is already published to the host, `crates/chelabase-neon/src/compute.rs:51`, but the worker has no client for it yet, and it may need the compute JWT; `ComputeBackend::status` today is the container's state). Cost: about 120 ms on a compute's first start (only while migrations are pending; measured 41.05 to 41.17), nothing afterwards. It needs a fork change, so a fork gate and an image build. It also closes the owner-SUPERUSER variant and any future migration.
2. **Worker only:** retry the whole data API script (and `bootstrap_sql`) when psql fails with `tuple concurrently updated`: up to 3 tries with a short backoff. The script is one idempotent transaction, so a retry is safe. It's cheap and unit-testable with the fake compute's `fail_when`, and it also absorbs other concurrent catalog writers (an owner's DDL during a reconcile bootstrap). It doesn't remove the race, only hides it.
3. **Worker only, coupling to compute internals:** before the bootstrap, poll `neon_migration.migration_id` (as cloud_admin, in `postgres`) until no `compute_ctl:migrations` backend is active. This is racy at start (the migration task may not have connected yet), and it needs the image's migration count to be robust. Not recommended.

The recommendation is option 1. If the user doesn't want a fork change in 6g (T15 runs after the build), option 2 now and option 1 in the next fork round. Both are small.

**Confidence:** medium. The mechanism is certain (reproduced). The writer is identified by elimination and from compute_ctl's source and timings, but the failing compute was torn down, so its compute_ctl log (which would show migration 2's `EXECUTING ALTER ROLE authenticator NOBYPASSRLS` NOTICE and its time) is gone. I didn't loop the e2e to reproduce it on the stack. The window is narrow, and each try provisions a database (about 100 s per e2e run). If confirmation is wanted: loop provisions and keep the compute logs (`docker logs` before teardown), or add a sleep in a local compute_ctl build to widen the window.

## Test data created and not removed

- None of mine are left. My four live runs (two of `a_keep_merge_base_behind_gc_is_unavailable`, two of `restore_a_child_branch` with temporary edits) cleaned up after themselves: no `live-*` database of theirs remains. The throwaway Postgres container `t15diag-pg` ran with `--rm` and was stopped.
- Still present from T15 (not mine): `live-ac34b7b7` and `live-efb7490c` (ready), the account `t15-verify@example.com`.
- The `a_keep…` runs restarted `chelabase-pageserver-1` twice (the test's own `fast_gc`), as expected.
- The stack is still on this worktree (`schema-merge`).
