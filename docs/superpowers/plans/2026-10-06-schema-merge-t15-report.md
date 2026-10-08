# Step 6g T15, verification report

Branch `schema-merge` at `9772ccb`. No product code or test was changed. No image builds. Run 2026-10-08.

## Pre-start checks of the shared dev metadata DB

- Only the Compose containers were up (`just live just deps-up`); the apps were not.
- `merges` table did not exist (`to_regclass` null): the two section-7 queries of the F3 report do not apply.
- `_sqlx_migrations` held 19 versions, max `20261011000001` (checksum equals this branch's file). `20261012000003`/`04` were never applied, so F2's in-place edit caused no checksum failure.
- Note: the DB had `20261011000001` applied but none of `20261010000001`-`04` (those were never on main). sqlx applied them out of order without complaint. `just live just dev` then applied `20261010000001`-`04`, `20261012000001`-`04`, `20261013000001`, all `success = t`.

## Offline gates

| Command | Result | Duration |
|---|---|---|
| `just test` (nextest workspace + doctests) | 2011 passed, 375 skipped, 0 failed | 52 s (plus compile) |
| `cargo clippy --workspace --all-targets -- -D warnings` | clean | 31 s |
| `just check` | pass (cargo check, openapi tests, generated code current, scalar, client/studio/docs typecheck+tests, docs build) | a few minutes |
| `pnpm --filter @chelabase/studio test` | 692/692 in 71 files (first run, concurrent with cargo: 2 timeouts, `branch-roles.test.tsx:73` and `tables-view.test.tsx:151` "paging_and_sort_build_the_right_tree", 5 s timeouts under CPU load; rerun alone: all pass) | 35 s |
| Studio `typecheck`, `build` | pass | n/a |
| Docs `typecheck`, `build`, `test` | pass, 14/14 tests | 1 s test |

## Live suites (all through `just live`, one at a time)

| Suite | Result | Duration |
|---|---|---|
| `chelabase-metadata --test live_db` | 183 pass, **1 fail** | 57 s |
| `chelabase-metadata --test neon_live` | 9/9 | 10 s |
| `chelabase-schema --test catalog_live` | 8/8 | 4 s |
| `worker --test branch_live` | 3 pass, **1 fail** | 103 s |
| `worker --test roles_live` (42, incl. `role_sessions_lists_the_live_connections`) | 42/42 | 480 s |
| `worker --test merge_live`, plain `cargo test` | aborted (stack overflow) after 12 tests, plus 1 fail | n/a |
| `worker --test merge_live`, nextest one process per test | 23 pass, **3 fail** | 880 s |
| `rest --test rest_live` (37, incl. `secret_key_of_an_owner_level_role_is_refused`) | 37/37 | 371 s |
| `just merge-e2e` | all passed | 26 s |
| `just branch-e2e` | all passed | 38 s |
| `just data-e2e` | all passed | 38 s |
| `just app-users-e2e` | all passed | 58 s |
| `just sql-editor-e2e` | 1st run FAIL (flake), rerun all passed | 99 s |

`chelabase-pageserver-1` is restarted by the merge_live GC tests (`docker restart`), as expected. Not run (outside the brief): worker `app_users_live`/`neon_live`, orchestrator `sql_*_live`, `chelabase-query` and `chelabase-neon` live tests.

## Failures and causes

1. **`live_db confirm_clears_attributes_by_change_id`**, `crates/chelabase-metadata/tests/live_db.rs:7318` (`assert!(confirmed)`). Cause: the test's earlier confirm with the older change id (line 7310) is called with the current `roles_version`, so `confirm_role` (`crates/chelabase-metadata/src/neon.rs:1501-1511`) also clears `pending` (the unconditional `UPDATE ... SET pending = false`; only the attributes clear is guarded by the change id). The final confirm at line 7312 then finds `pending` already false and returns `cleared == 1` false. Either the test expects a stale-id confirm to leave `pending` alone (code would need the id guard on `pending`), or the test's last assertion is wrong. Needs a ruling.
2. **`branch_live restore_a_child_branch`**, `services/worker/tests/branch_live.rs:113` (`live_children(id).is_empty()` in `Story::claim`). Cause: step 5b (added by the T14 fix batch, M8, line 552) restores `dev` through the helper `restore()` (line 405), whose `claim` asserts no live children, but `dev` has the live child `dev-child` created at line 538. Test helper contradicts the test's own scenario. Cleanup ran.
3. **`merge_live keep_reset_delete_after_merge`**: `thread has overflowed its stack`, SIGABRT after `Rehearse` (in `fx.apply`, the reset-after-merge path) in a debug build on the default 2 MiB test thread. It passes with `RUST_MIN_STACK=33554432` (40 s), so it is a large unoptimized async future, not a recursion. It aborts the whole process under plain `cargo test` (hides later tests); nextest isolates it.
4. **`merge_live recovery_reads_the_marker`**, `services/worker/tests/merge_live.rs:2338` (`left: Done, right: Online`). Cause: in part 2 the poller (`wait_merge`, line 2331-2335, which stops on Online or any non-working status) never observed `Online` because the only online step (`CREATE INDEX CONCURRENTLY`) is quick and the merge reached `Done` first. Racy test.
5. **`merge_live a_keep_merge_base_behind_gc_is_unavailable`**, `merge_live.rs:2565`: "GC didn't pass 0/1FC0FA0 within 300s: applied_gc_cutoff_lsn = 33296288" (= the point, 0x1FC0FA0). It failed identically in both runs (plain cargo and nextest, 318 s). The cutoff on dev's timeline sticks exactly at the merge's base point despite churn on main and dev; the precondition `cutoff > point` is never reached. Not diagnosed further (something pins GC at that LSN on dev's timeline; the other GC tests, e.g. `a_branch_point_behind_gc_is_read_on_the_parent`, pass).
6. **`sql-editor-e2e` first run**: database bootstrap failed `ERROR: tuple concurrently updated` in `psql in compute ... exited with 3` (the data API bootstrap, `chelabase_schema_version_*` event triggers). Passed on rerun; a catalog race in the bootstrap, not investigated.

## Playwright pass

Deferred by the user (UI revamp next). The flow lists to reuse are in `docs/superpowers/plans/2026-10-06-schema-merge-f4-report.md`: "Flows for T15's Playwright pass" (1-11) and "Added for T15" (12-14), plus the plan's 7-step flow. I had started it (Playwright browser `chromium-1247` was installed with `pnpm dlx @playwright/mcp install-browser chrome-for-testing`; an account was created; plan steps 1 and 2 and the conflict page reached) when the scope change arrived. I removed the screenshots and the `.playwright-mcp` output; nothing was recorded for any flow.

## Test data left behind

- Two Chelabase databases in the demo project from the failed live tests: `live-ac34b7b7` and `live-efb7490c` (ready; from the aborted/failed merge_live and branch/GC tests). Possibly also a failed database `e9a70227-59ea-4ff0-8c01-7387ead873fc` from the sql-editor flake (not visible in Studio's list).
- A Studio/Zitadel account `t15-verify@example.com` (password `T15-verify-Pass#2026x`) in the demo org.
- My `t15_main` database (branch `dev`, one draft merge) was deleted at the end (delete accepted, 202).
- All e2e scripts clean up after themselves. The metadata DB now carries the 6g migrations.

## Stack

Left running on `schema-merge` (`.chelabase/dev/worktree` = this worktree). It was not moved back to main: the dev metadata DB now has migrations `20261010000001`-`04` and `20261012000001`-`20261013000001` applied, which main's migrator doesn't have, so main's stack would refuse to start ("previously applied but is missing"). Moving it back needs the user's go (D13).

## Fix round (2026-10-08, commit `180d7c2`)

Failures 1-5 fixed on the user's rulings; failure 6 untouched (parked for the next fork round). No image builds, no fork changes.

| # | Fix | Test | RED / GREEN |
|---|---|---|---|
| 1 | Test fix: the stale confirm carries the first write's version and change id (`confirm_role` unchanged); asserts `after_stale == (true, true)`, then the final confirm clears both. | `live_db confirm_clears_attributes_by_change_id` | RED: the T15 run (`assert!(confirmed)` at 7318). GREEN: `live_db` whole run. |
| 2 | Test fix: step 5b runs before `dev-child` exists, restores to the LSN one below `dev.branched_at_lsn`, asserts "is before dev's history begins (its branch point,", timeline and rows unchanged. The 409 `has_children` variant isn't added (the worker helper doesn't go through the API). | `branch_live restore_a_child_branch` | RED: T15 run (`Story::claim` at 113). GREEN: `branch_live` whole run. |
| 3 | Product fix: the large async fns on the merge job path are plain `fn`s returning `BoxFuture` (`merge_apply`, `apply_fresh`, `continue_online`, `merge_online`, `merge_rehearse`, `merge_compute`, `merge_resume`, `merge_finish`, `finish_merge`, `after_merge`, `after_reset`, `branch_reset_attempt`, `finish_reset`, `recreate_compute_with`), each over a `*_inner` async fn. Cause (measured with stack-address probes, 32 MiB run): in debug builds each caller's poll frame holds a copy of every callee future (`merge_apply`'s frame alone was ~335 KB, `after_reset`'s ~133 KB), summing to about 1.8 MB on the reset path; a plain `Box::pin` at the call site doesn't shrink those frames, so the boxing is in the callee's `fn`. | unit `merge::finish::tests::reset_after_merge_fits_a_small_stack` (fakes, 448 KiB thread, 1 reset-after-merge apply) | RED: before the fix the test needs over 600 KiB (stack overflow at 512 KiB, SIGABRT). GREEN: passes down to about 300 KiB. Live `keep_reset_delete_after_merge` passes on the default 2 MiB stack. |
| 4 | Test fix: a holder transaction (xid, `pg_sleep(25)`, app name `t15-holder`) is open on main, so `CREATE INDEX CONCURRENTLY` waits and the merge sits in `online`; the test polls `Online`, asserts the holder is still running, aborts the apply, lets the holder end, then resumes. | `merge_live recovery_reads_the_marker` | RED: T15 run (`Done` vs `Online`). GREEN: `merge_live` whole run. |
| 5 | Test fix: `fast_gc`'s tenant patch adds `"checkpoint_timeout": "1s"` (doc comment says why). `fast_gc` is shared; all GC tests passed. | `merge_live a_keep_merge_base_behind_gc_is_unavailable` and the other GC tests | RED: T15 run (300 s timeout). GREEN: `merge_live` whole run. |

RED for 1, 2, 4, 5 is the T15 failing run (same code, same stack); I did not re-run the old tests before editing.

### Suite results

- `cargo build --workspace --all-targets`: ok. `cargo clippy --workspace --all-targets -- -D warnings`: clean. `cargo test --workspace`: 2012 passed, 0 failed.
- `live_db`: 184 passed, 0 failed (57 s).
- `branch_live`: 4 passed (95 s).
- `merge_live` (plain `cargo test`, one thread, no overflow): 25 passed, 1 failed (622 s). The failure is `second_merge_carries_only_newer_changes`: `provision failed` after 6.8 s (the database never reached Ready), at the test's provision step. It looks like the parked failure 6 (the provisioning bootstrap race) but I did not capture the cause (the test cleaned up). The same test alone passed on rerun (22 s).
- `just merge-e2e`: all passed (29 s). `just branch-e2e`: all passed (about 40 s).

### Test data left

Nothing new beyond the T15 list (`live-ac34b7b7`, `live-efb7490c`, `t15-verify@example.com`); the merge_live provision failure was cleaned up by the test. The stack is left running on `schema-merge`.
