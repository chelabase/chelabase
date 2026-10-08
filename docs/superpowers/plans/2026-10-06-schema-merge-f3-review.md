# Step 6g T14 F3 review: merge state machine and apply

Scope: `git diff schema-merge...schema-merge-f3` (`2bad3a2`, `562f6f3`, `59c3b79`), against the T14 review's state-machine and apply/planner slices, rulings E27–E35 and the plan Status's "For F3 (collected)" list. This review changed no code.

Commands (run in this worktree):
- `cargo test -p worker -p orchestrator -p chelabase-metadata`: green. 515 worker, 300 orchestrator lib, 102 orchestrator api and 83 metadata tests pass, plus the smaller suites; the live tests are ignored and compile.
- `cargo clippy -p worker -p orchestrator -p chelabase-metadata --all-targets -- -D warnings`: clean.
- No live tests were run.

## Verdict

**Needs fixes.** There are 2 Important findings and 11 Minor ones; nothing is Critical.

Every in-scope finding, ruling and "For F3" item has a fix in code with a test that asserts it. The table in `MergeState` matches the spec and E27/E34. The SQL predicates, migration `20261013000001` and the Rust table are tied together by `the_migration_uses_the_state_predicates` (in CI) and by `merge_state_sql_matches_the_table` (live). The M12 locking is sound:
- A claim takes `FOR UPDATE` on one branch row, then reads the merges in a new READ COMMITTED statement.
- A merge move takes `FOR SHARE` on both branch rows, in id order.
- No path takes branch locks in the opposite order, so there's no deadlock.

The two Important findings are holes in the invariant outside the API: the worker's own after-merge reset and delete, and a recompute that follows a phase 1 the worker couldn't settle.

## Important

**I1. The worker's after-merge reset and delete of the source ignore E27's hold.**
- Where: `services/worker/src/merge/finish.rs:606-614` (`after_reset` moves the branch with `set_branch_status` Ready→Resetting) and `:638` (`after_delete` calls `begin_branch_delete`). Neither runs `branch_busy`, so neither sees a merge that holds the branch.
- Scenario:
  1. Merge M2 (`main → dev`, from_parent, target `dev`) fails in `online`, or is still `applying`/`online`/`rehearsing`. Nothing stops this: `applying_on` is per target, and M1's target is main.
  2. Meanwhile merge M1 (`dev → main`, `after: reset` or `delete`) finishes.
  3. M1's finish resets or deletes `dev`, which M2 holds.
- Result: this is exactly the T14 I3 scenario A or B that E27 was meant to close. If M2 is failed in `online`, a Retry then finds no marker and fails for good with `merge_target_changed`, and phase 1's NOT VALID constraints and checks are gone or orphaned with the old timeline. If M2 is running, its compute is switched under phase 2.
- Fix: run the same check under the branch's row lock.
  - Give `claim_branch_for_switch` an `except: Option<Uuid>` (the finishing merge's own id, since `finishing` holds its source).
  - Use it in `after_reset` instead of the bare `set_branch_status`.
  - For `after_delete`, add the busy check (with `except`) inside `begin_branch_delete`'s transaction, after the branch row is locked. Or lock the row and check first.
  - Report `{"choice": …, "refused": "a merge holds <branch> …"}`.
  - Test: a from_parent merge failed in `online` on dev, plus a dev→main merge with `after: reset`. The reset is refused and dev's timeline is unchanged.

**I2. A recompute after a phase 1 the worker couldn't settle can skip the new plan's phase 1.**
- Where:
  - `services/worker/src/merge/apply.rs:250-262`: a tx failure whose marker can't be read ends `failed` in `tx` ("Retry checks it first").
  - `crates/chelabase-metadata/src/merges.rs` `MergeState::after`: Recompute is allowed from `failed(tx)`.
  - `apply.rs:191-200`: `ever_started` reads the whole log, which is never cleared, then trusts the marker.
- Scenario:
  1. Phase 1 commits, but the exec drops and the marker read fails too, so the merge ends `failed(tx)`.
  2. The user presses Recompute instead of Retry. Studio I2 currently offers only Recompute for these phases.
  3. compute → draft → rehearse → apply. `apply_fresh` sees steps in the log from the old attempt and finds the marker, which is keyed by merge id only. It goes on to online with the **new** plan and never runs that plan's phase 1.
- Result: in the best case the drift check ends the merge `applied_with_drift`. In every case the target keeps a half-applied old plan while the merge claims the new one.
- Running the new phase 1 wouldn't help either: its `INSERT INTO chelabase_merge.applied` would hit the primary key on every attempt.
- Rule to enforce: once the marker exists on the target, the merge is half applied and must never be recomputed (E27).
- Fix (mechanical, whichever answer the user gives to the report's Q3):
  - The compute job, for a merge with any `step` event in its log, stops the merge's backends and reads the target's marker before computing.
  - If the marker is present, the merge goes back to `failed`/`online` ("phase 1 committed; retry or abandon"), and nothing is computed.
  - If the read fails, the job is redelivered.
  - Test: `failed(tx)` with the marker present, then a Recompute. The merge ends `failed(online)`, the plan is unchanged and no guard runs.
  - See the product section for the alternative (Q3 (b)).

## Minor

1. **A lost Finish job after `started` is never re-published.**
   - Where: `crates/chelabase-metadata/src/merges.rs:800` matches only `requested = true`, but `finish.rs:456` replaces `result.after` with `{choice, started, timeline}` and so drops `requested`.
   - Scenario: in `applied_with_drift`, the worker crashes after the `started` write and the job isn't redelivered (max deliveries, or a stream purge). `finishMerge` refuses ("already run or requested"), and reconcile never re-publishes. The merge is stuck in `started`.
   - Fix: also match `result->'after'->'started' = 'true'` in `stale_finish_requests`, and keep `requested: true` in the `started` write.

2. **Lost-Finish re-publishes pile up behind a long job, as Resume did before M10.**
   - Where: `services/worker/src/merge/mod.rs:65` sets `STALE_FINISH_REQUEST` to 10 min, and `stale_finish_requests` re-hands once that has passed since the last hand-back.
   - Scenario: a Finish queued behind another merge's 6 h online step (P11) is re-published about 36 times. Each extra copy is a no-op, but this is the duplication M10 removed.
   - Fix: hand back at most once per `REHAND_AFTER` (30 min) after the first 10 min, as `stale_merge_claims` does.

3. **The hand-back is recorded before the publish.**
   - Where: `services/worker/src/reconcile.rs:451` (`handed_back` writes `handed_back_at`) runs before `main.rs:320`'s publish, and a failed publish is only logged.
   - Scenario: a NATS blip during the publish leaves a silent merge un-resumed for 30 min (`REHAND_AFTER`). Before F3 the wait was 2 min.
   - Fix: return the jobs first, publish, then `mark_merge_handed_back` only on success. Or clear `handed_back_at` when the publish fails.

4. **Apply and rehearse take no version from the client, and E34 widens the gap.**
   - Where: `services/orchestrator/src/merges.rs:1144` and `:1247`. `guard_version` is the version the request itself just read, not the one the user saw.
   - Scenario: user A views a rehearsed merge with `after: keep`. User B saves `options.after: delete`; under E34 it stays `rehearsed`. A clicks Apply and A's source branch is deleted after the merge, which A never saw.
   - The M11 fix asked to "pass the version".
   - Fix: take an optional `version` in the apply and rehearse bodies (Studio sends it), with a 409 `merge_version_conflict` on a mismatch. Regenerate the OpenAPI spec.

5. **The rehearsal's role rename skips the checks the real rename makes.**
   - Where: `services/worker/src/merge/phases.rs:583-600` (the rehearsal runs a bare `ALTER ROLE … RENAME`), compared with `roles.rs:978-1037` (`rename_role`).
   - What it skips:
     - the name checks (`check_any_name`, reserved targets, the owner);
     - "ours" (the owner administers the role, or it has an API row on the target, via `rows_endpoint`);
     - the existing-name check on `to`.
   - Scenario: the rehearsal passes, then phase 0 refuses with `not_found` or `reserved`. This is the SM M3 / T8a M6 class that F3 fixed for creates and updates.
   - Fix: share a `rename_checks` helper and run it on the rehearsal compute and rows endpoint before the SQL. Add a test like `the_rehearsal_refuses_what_the_apply_would`.

6. **A phase 2 failure that isn't a SQL error leaves the statement running, and Abandon can then miss it.**
   - Where: `apply.rs:430-447` (`fail_merge` in `online` without `stop_merge_backends`) and `merges.rs` `abandon_merge`. Only a timeout cancels (`phases.rs` `merge_sql`).
   - Scenario: the docker exec drops during `CREATE INDEX CONCURRENTLY`. The merge is `failed(online)` while psql still builds in the compute. Abandon frees the branches, `leftovers` doesn't list the build, and it can still finish or leave an INVALID index after the user has read the list.
   - Fix: in `fail_phase` for `Phase::Online` with `FailureKind::Failed`, run `stop_merge_backends` before writing `failed` (best effort, logged), as `committed_after_all` does for phase 1.

7. **The OpenAPI and error texts don't describe the new holds.**
   - `services/orchestrator/src/branches.rs:756`, `:853`, `:939` (the reset, restore and delete 409s) never name `branch_merge_busy`, `error.merge_id` or the new `error.rehearsal_branch_id`. `openapi/management.json` has the code only in the enum.
   - `merges.rs:485` `busy()` says "another merge is changing the target branch", which is wrong for a `failed(online)` holder (create, apply and retry). It should say "…or failed half applied; retry or abandon it".
   - Fix: update the three descriptions and the `busy` text, then `just api-gen`.

8. **The user step id `phase1` isn't reserved.** `merges.rs` `draft_patch` refuses `:`, `.` and control characters, but the worker logs the whole of phase 1 as step `phase1` (`phases.rs:45`). A user tx step with that id is indistinguishable in the log, in Studio's progress (Studio minor 1) and in `failed_step`. Fix: refuse `phase1` too, or log phase 1 as `chelabase:phase1`, which the `:` rule already keeps users from using.

9. **The exhaustive action test checks only the status.**
   - Where: `services/orchestrator/tests/api.rs:9521-9545`.
   - What it misses for an allowed action:
     - which job was published (Retry from `failed(online)` → `Resume`; from roles/enum_add/tx → `Apply` with a `status→applying` event);
     - that `failed_phase` is cleared;
     - that Abandon writes `result.leftovers`.
   - A wiring regression in those parts would pass. Fix: assert the published job kind per `(state, action)` and `failed_phase == None` after a successful move.

10. **Check the migration against the shared dev DB first.**
    - `20261013000001` rebuilds the two unique indexes with wider predicates. On a metadata DB that already holds a `failed(online)` merge, the rebuild fails, and the migrator stops startup, if either:
      - the same pair also has an open merge;
      - another `applying`/`online`/`finishing`/`failed(online)` merge has the same target.
    - Before T15's `just live`, run on the dev metadata DB:
      ```sql
      SELECT target_branch_id FROM merges
       WHERE status IN ('applying','online','finishing') OR (status='failed' AND failed_phase='online')
       GROUP BY 1 HAVING count(*) > 1;
      SELECT source_branch_id, target_branch_id, database_name FROM merges
       WHERE NOT (status IN ('done','applied_with_drift','stale','cancelled') OR (status='failed' AND failed_phase IS DISTINCT FROM 'online'))
       GROUP BY 1,2,3 HAVING count(*) > 1;
      ```
      Both must return no rows; otherwise cancel the extra rows first. Adding the column is safe (nullable, no default).

11. **`updateRole`'s description says "`member_of` replaces the whole list"** (`services/orchestrator/src/roles.rs:633`). Since F1/F3, `pg_*` and `chelabase_*` memberships are never revoked by a list that leaves them out (`a_membership_list_never_revokes_reserved_groups`). Fix: "…replaces the list of the role's other memberships (`pg_*` and Chelabase's own groups stay)". Regenerate the OpenAPI spec. This is server work; it shouldn't wait for F4's Studio pass.

## Checked and fine

- **SM I1 / AP I1**: the marker settles a non-stale tx failure (`committed_after_all`), and a Retry reads the marker first (`ever_started`). The tests fail without the fix.
- **SM I2**: `log_status` writes the `status→applying` event in the move's transaction (`merges.rs` `set_merge_status`). `attempt_start` scopes `attempt_started` and `online_done` in both the worker and the API (Abandon).
- **SM I3 / E27**: the indexes, the busy SQL, the marker check in `merge_online`, and the gone target (`merge_target_gone`).
  - Abandon can't race a job: only `failed` with `failed_phase = 'online'` moves, under the same guard Retry uses, and a stale Resume of a cancelled merge is a no-op at `claim_merge`.
  - Its leftovers match the planner's NN-check SQL (`tables.rs:968-975`).
- **SM I4**: a live rehearsal child holds its parent, in the claim and in the pre-check.
- **M1–M12**: done as reported.
  - The `finishMerge` guard (`result IS NOT DISTINCT FROM` the result read) lets only one of two requests publish.
  - The worker runs the recorded choice.
  - A reset or delete records `started` first, and a re-run reports what the branch shows.
- **The M10 numbers**: `STALE_MERGE_CLAIM` 180 s (3 × refresh, pinned by a test). A hand-back counts against `claimed_at`, so a claim after it re-arms the merge.
- **E29**: phase 0 renames through `rename_role` and skips a rename an earlier attempt finished. The rehearsal renames by SQL; see Minor 5.
- **E31**: `follows_rename` and `expected_rehearsed` are stored at compute. The rehearsal stores `with_rehearsed`, and the finish counts these objects as produced until a rehearsal has run.
- **E34**: `keep_plan` leaves the status and rehearsal as they are.
- **"For F3"**: every item is done (`needs_acknowledgement` in both places, `lock_timeout_ms` shared, the step-id charset, the `member_of` check, `at_line`).
- **OpenAPI**: the committed `management.json` matches (the drift test passes). The `abandonMerge` 200 + `Merge` response is fine for a synchronous move.

## Needs a product decision

**D1 (I2 and the report's Q3): a phase 1 whose outcome couldn't be read.**
- (a) **Recommended:** keep `failed(tx)` with the note (the report's choice), and add I2's marker check to the compute job, so a recompute of a half-applied merge turns it into `failed(online)`.
  - Cost: one marker read per recompute of a merge that has ever applied.
- (b) Leave the merge in `applying` and redeliver the job until the marker can be read.
  - Recovery then returns the merge to `rehearsed` (losing the error text) or goes on to online. The user sees "applying" with no reason while the target is unreachable.

**D2 (the report's Q1): what Abandon lists.**
- (a) **Recommended:** add `kind: merge_index` for leftovers that start with `chelabase_merge_ix_`:
  - an E19 temporary unique index, valid but never attached by its `ADD CONSTRAINT … USING INDEX` step;
  - an INVALID index from a concurrent build that was interrupted.

  Read them from the plan's `IndexValid` checks that `done` doesn't cover, the same way the NN checks are read. Without this, the user is left with an unused unique index that still costs on every write.
- (b) Keep the list as ruled (NOT VALID constraints and NN checks only), and say in the docs that temporary indexes may be left.
