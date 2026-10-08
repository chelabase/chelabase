# T14 review: state machine

Slice: merge state machine (worker merge jobs, claims, recovery, reconcile, finish; orchestrator merge API transitions and branch-op refusals). Read-only, nothing written.

## Verdict
Needs fixes: 4 Important findings and 12 Minor ones. Nothing Critical.

## Important

**I1. Phase 1 can commit and still be recorded as "failed in tx". A Retry then calls the merge `stale` and phase 2 never runs.**
- Where: `services/worker/src/merge/phases.rs:339-343` and `:497-512`, `services/worker/src/merge/apply.rs:194-196`.
- Scenario: the psql/docker exec dies after COMMIT, or the 16 min admin timeout fires during COMMIT and `cancel_sql` arrives too late. `run_phases` returns `Failed` or `TimedOut` in phase `Tx`, and `fail_phase` stores `failed`/`tx`. Retry (E20) publishes Apply, and `apply_fresh` runs `target_moved`. The digest now differs because phase 1 applied, so the merge goes `stale` with STALE_BEFORE_RETRY ("the plan's transaction wasn't applied"). That is false. Phase 2 never runs: NOT VALID constraints and `chelabase_merge_nn_*` checks are left on main, and a recompute treats them as target changes. This breaks Part 5's "phase 1 exactly once: the marker decides".
- Fix: in Real mode, after any phase 1 failure that isn't `Stale`, read `marker_present` (with `stop_merge_backends` first). If the marker is there, advance `applying → online` and go to `continue_online`. Also read the marker at the start of `apply_fresh` when `apply_started` is true. Tests: a phase 1 timeout after commit goes on to online; a Retry after a tx failure with the marker present goes on to online.

**I2. A redelivered Apply after a Retry (or after a recompute) is treated as an interrupted run that never started (T13 note, still open).**
- Where: `services/worker/src/merge/mod.rs:197` (`recovering = attempt > 1`), `services/worker/src/merge/apply.rs:113-116` and `:475-482` (`apply_started` = any `step` event in the merge's whole log).
- Scenario: a merge fails in roles, enum_add or tx and the user presses Retry. The worker restarts before running it, or the pre-check hits a transient error, so the job is nak'd. The redelivery (attempt 2) sees no marker, but `apply_started` is true from the first attempt's events. It goes back to `rehearsed` with "interrupted, nothing applied" without ever running, while the first attempt's roles and enum values are on the target. The same happens after `failed (roles) → recompute → rehearse → apply`, because events survive a recompute.
- Fix: scope "started" to the current attempt. The API (apply and retry) and the worker append a `status` event `{to: applying}` at each transition, and `apply_started` only counts `step` events after the newest one. `online_done` and `created_roles` should use the same scoping. Also reword INTERRUPTED: phases 0 and 1a may have run (this is also T8b M7).

**I3. A merge `failed` in `online` (half-applied) doesn't hold its branches or its pair.**
- Where: `crates/chelabase-metadata/migrations/20261010000001_schema_merge.sql:51-54`, `crates/chelabase-metadata/src/merges.rs:589-599` (`branch_has_busy_merge`) and `:79-90` (`is_open` / `is_applying`), `services/worker/src/merge/apply.rs:208-225` (`merge_online` doesn't check the marker).
- Scenario A: phase 1 committed, then an online step hits its lock_timeout, leaving `failed`/`online`. The user resets or restores the target (from_parent: the child), which isn't refused. Retry then runs phase 2 (CREATE INDEX CONCURRENTLY, SET NOT NULL, …) on a timeline where phase 1 never happened, which is a partial apply, then finishes with drift and may advance the base on keep.
- Scenario B: the user deletes the target or source instead. Retry moves the merge to `online`. `recovery_target` then errors ("no longer exists" or "no compute"), so the job is nak'd forever and reconcile re-queues it every 120 s. Cancel and recompute both refuse `online`, so the merge is stuck for good.
- Scenario C: a new merge for the same pair, or another merge into the same target, can be created and applied while phase 2 of the first is still pending.
- Fix:
  - count `failed` with `failed_phase = 'online'` as open and as applying, in both unique indexes and in `branch_has_busy_merge`;
  - in `merge_online`, check the marker before resuming;
  - when the target is gone, fail the merge permanently instead of returning `Err` forever.
  - The indexes need a new migration (6g's isn't on main yet, so editing `20261010000001` is possible if the shared dev DB is reset; otherwise add a migration).

**I4. Cancelling a rehearsing merge frees its branches before the rehearsal is torn down (T10 minor, T8b concern 3, still open).**
- Where: `services/orchestrator/src/merges.rs:1403-1417`, `crates/chelabase-metadata/src/merges.rs:589-599`. Rehearsal children aren't counted by `live_children` (`branches.rs:489`) or by `begin_branch_delete`.
- Scenario: cancel → `cancelled` at once. The rehearse job only notices at its next claim refresh (up to 60 s, longer while it's still building, see M5 below). A failed teardown waits for reconcile (30 min). Meanwhile a reset, restore or delete of the target is accepted, and Neon refuses the parent timeline's delete because a child (the rehearsal) exists. The branch op fails or retries for up to 30 min.
- Fix: `branch_has_busy_merge` also returns busy while the branch has a live `kind = 'rehearsal'` child, which covers both the cancel window and a leftover rehearsal. Do the check under the branch claim (see M12).

## Minor

1. **Phase 0 skips a pending role row** (T8a M1). `services/worker/src/merge/phases.rs:544-548` skips any API row by name, even with `pending = true` (row stored, SQL failed). The role may not exist when phase 1 grants on it. Fix: skip only when `!r.pending`; otherwise run `exists_sql` and create if it's missing.
2. **Phase 1 failure mapping can be wrong or steered** (T8a M2, M3). `phases.rs:231-245` blames a marker-insert or COMMIT failure on the last tx step, and a user step's `-- step <id>` line can steer which step gets blamed. `phases.rs:467` and `:504` classify by substring, so a user `RAISE` with the STALE_ERROR text turns a failure into `stale`. Fix: the script emits unique per-run sentinels (`-- chelabase-step <nonce> <id>`), and the guard raises with a distinct SQLSTATE (for example `P0001` plus a HINT nonce) that is matched instead of the text.
3. **The rehearsal's phase 0 skips the real path's checks** (T8a M6). `phases.rs:564-596` runs only `check_spec`, not the membership/administer checks (`membership_check_sql`). A rehearsal passes and the real apply then fails in `roles`. Fix: run the same membership check SQL on the rehearsal compute.
4. **A refused resolution disappears without a reason** (T8a M7). `services/worker/src/merge/compute.rs:251-253` drops an `InvalidChoice` resolution silently. Fix: collect the dropped ids with reasons into `error`, or into an `analysis.dropped_resolutions` field.
5. **Cancel during the rehearsal build isn't seen** (T8b M8). `services/worker/src/merge/rehearse.rs:139-145` builds the rehearsal under `keep_merge_claimed`, so a cancel during the build is noticed only after it, plus one more refresh. Fix: use `run_while_claimed` for `make_rehearsal`; on `None`, `remove_rehearsals` and return.
6. **`failed_phase` is NULL after the marker** (T8b M3). `services/worker/src/merge/apply.rs:101` fails with `phase = None` when `stored_plan` fails after the marker. The merge can then be recomputed although phase 1 committed. Fix: `Some(Phase::Online)`.
7. **Reconcile docs and young rehearsals** (T8b M4, M5). In `services/worker/src/reconcile.rs:404-412`, `merges_to_resume`'s doc swallowed `reconcile_once`'s "Serial on purpose…" doc: move it back above line 439. At `:213-220`, rehearsals younger than 30 min fall through to the normal branch rules: `continue` for every `Rehearsal` and only add `RemoveRehearsal` when it's old.
8. **Finish edge cases** (T8b re-review minors, all still open):
   - `services/orchestrator/src/merges.rs:1480-1489`: the `requested` write has no guard on `result.after.skipped`. Two concurrent `finishMerge` calls both get 202 and both publish. Only one runs (the worker checks `requested`), and it uses the job's `after`, not the stored choice. Fix: a conditional update (`WHERE result->'after' ? 'skipped'`), and use the stored choice in the worker.
   - Lost Finish job: `requested: true` stays forever, because `stale_merge_claims` excludes `applied_with_drift` and the API refuses a second request. Fix: reconcile re-publishes Finish for `requested` older than N minutes.
   - `services/worker/src/merge/finish.rs:520-566`: a crash after `after_reset` finished re-runs the reset, losing the source's writes since. A crash mid-reset records "isn't ready" (refused) although the reset happens. `after_delete` re-run reports `refused`. Fix: write `result.after = {choice, started: true}` before acting, and on re-run report what the branch shows (new timeline: `done`).
9. **The pre-check projection hides too much** (T8b re-review and T13c minor). `crates/chelabase-schema/src/fingerprint.rs:30-62`:
   - roles named by `Update` steps are removed entirely (use `Create` names only, and project out only the patched attributes);
   - every label of a touched enum is cleared (remove only the labels the plan adds);
   - an unparseable 1a statement clears every enum (`every_enum`); treat it as stale instead.
10. **Duplicate Resume jobs pile up** (T8b M6). `services/worker/src/reconcile.rs:412-437` re-publishes a Resume every 120 s for a merge queued behind a long job: about 180 duplicates behind a 6 h online step. They're harmless no-ops under P11, but `claim_refresh` 60 s vs a stale limit of 120 s leaves little margin. Fix: skip the hand-back when an undelivered Resume for that id is pending (track the last hand-back time in the row), or raise STALE_MERGE_CLAIM to 3× `claim_refresh`.
11. **API surface** (T10 and T8b M10 minors):
   - `services/orchestrator/src/merges.rs:701, 941, 1058, 1167, 1275, 1358, 1495`: reverts after a failed publish swallow errors with `let _ =`; log at `error!`.
   - `:236-265`: `getMerge` and the PATCH responses (including the 409's `error.current`) serve `plan.expected` and `own_sql` (about 100 KB); strip them.
   - Cross-database 404 is tested only for `getMerge` (`tests/api.rs:8732-8737`); add it for each route.
   - `rehearse_merge` (`:1049-1051`) moves draft→rehearsing without a version guard. The worker re-checks, so this is low risk; still, pass the version.
12. **Smaller gaps:**
   - TOCTOU: `refuse_if_merge_busy` (`services/orchestrator/src/branches.rs:752, 853, 932`) runs before `claim`, and `apply_merge` doesn't check the target's claim. Do the merge-busy check inside the claim's SQL.
   - The worker's `after_delete` (`finish.rs:569-588`) doesn't cancel the deleted source's other open merges (a from_parent draft is a different pair). Call `cancel_drafts_for_branch`.
   - A rehearsal still gets a sealed public data API key (`services/worker/src/data_api.rs:678-718` via `bootstrap_served_on`, `branch_create.rs:491`). Skip `ensure_public_key` for `rec.rehearsal`.
   - Test gaps (T8a M9): no test for a marker-setup failure, or for the `ConstraintValidated` / `NotNullSet` skips in `phases.rs`.
   - No unit test for `BaseSource` round trips (T5 minor).

## Status item checklist

| Item | Status |
|---|---|
| T5: save + compute in one statement | Fixed (`update_merge_draft` with `start_compute`) |
| T5: create race gives a retryable 409 | Fixed (`merges.rs:693`, `merge_busy` with Retry-After) |
| T5: `set_merge_status` has no version check | Fixed (`claim_merge` re-reads after the claim; writes are status-guarded) |
| T5: `merges_one_applying` | Fixed for the API (`refused_by`); the worker never enters applying. Gap: see I3 |
| T5 minor: `BaseSource` unit test | Open → M12 |
| T5 minor: no CHECK on `merge_events` | Ruled no action → closed |
| T8a M1 (pending role skipped) | Open → M1 |
| T8a M2, M3 | Open → M2 |
| T8a M5 (cancel before `run_online`) | Fixed (`apply.rs:214`) |
| T8a M6 | Open → M3 |
| T8a M7 | Open → M4 |
| T8a M8 (re-check acks at apply) | Fixed (`apply.rs:125-145`) |
| T8a M9 test gaps | Partly open → M12 |
| T8b re-review: finish once-only race | Open → M8 |
| T8b re-review: lost Finish job | Open → M8 |
| T8b re-review: crash mid-finish re-runs reset | Open → M8 |
| T8b re-review: projection too broad | Open → M9 |
| T8b re-review: unparseable 1a statement | Open → M9 |
| T8b M3 | Open → M6 |
| T8b M4 | Open → M7 |
| T8b M5 | Open → M7 |
| T8b M6 | Open → M10 |
| T8b M7 (wording) | Open → I2 |
| T8b M8 | Open → M5 |
| T8b M9 (Studio) | Not in this slice |
| T8b M10 | Open → M11 |
| T8b concern 3 | Open → I4 |
| T10: reverts swallow errors | Open → M11 |
| T10: rehearse/apply don't check Ready | Fixed in the worker (`merge_target` returns `branch_not_ready`) |
| T10: cancel from rehearsing frees branch first | Open → I4 |
| T10: cross-db 404 tested only for getMerge | Open → M11 |
| T10 race: rehearseMerge newer draft | Mitigated by the worker's re-check, no version guard → M11 |
| T10 race: recompute rebaseline lost | Fixed in effect (every compute writes `plan`, so `version` bumps and the old save gets 409) |
| T13: Apply redelivered after Retry | Open → I2 |
| T13: rehearsal gets a sealed data API key | Open → M12 |
| E12e: resolutions dropped when their conflict changed | Fixed (`compute.rs:63-93`) |

## Questions for the user
1. **A merge failed in `online` (I3).** Should it hold its branches like a running merge, or can it be abandoned?
   - (a) Recommended: it counts as busy and open, so reset, restore and delete of either branch and new merges of the pair are refused until it's retried to the end. The user is told to finish with Retry. Recompute stays refused.
   - (b) Same as (a), plus an "abandon" action that marks it `cancelled` and leaves the NOT VALID constraints and `chelabase_merge_nn_*` checks on the target, reported as leftovers.
   - (c) Leave branch ops free, and have Retry refuse when the marker is missing or the target has changed timeline.

   My recommendation is (a), with (c)'s marker check added as a backstop.# T14 review: apply/planner

Slice: apply safety and the planner (plan/**, steps, export, compare, sql, the worker's merge/phases.rs and apply.rs, and the neon backend's admin_sql_with / spec static mode). Everything below comes from reading the code at `0e005f3`; I built and tested nothing.

**Verdict:** no Critical findings and one Important one. The phase 1 transaction is atomic: psql runs with `ON_ERROR_STOP=1`, the exit code is checked (`backend.rs:333`), and P8 refuses transaction control. Every `cloud_admin` site I traced pins its `search_path`: `cancel_sql`, `check_sql`, `backends_sql`, `exists_sql`, `role_read_sql`, the digest prelude and `marker_setup_sql`. The marker setup re-checks ownership after its `CREATE … IF NOT EXISTS`. Phase 2 temporary index names carry the merge id, and the IndexValid check also checks `indrelid`. E19 is implemented (`plan/mod.rs:506-540`).

## Important

**I1. A phase 1 that committed can end as "failed in tx", and Retry then says nothing was applied.**
- Where: `services/worker/src/merge/phases.rs:466-483` and `:497-512`; `services/worker/src/merge/apply.rs:123-175`.
- Scenario: phase 1's `COMMIT` completes, but the worker loses the result. Either the 16 min admin timeout fires while psql is returning (the cancel comes too late), or the docker exec fails while reading output or inspecting the exit code (`backend.rs:319`, `:332`). `merge_sql` then returns TimedOut or Failed, and `fail_phase` sets `failed` with `failed_phase = tx`.
- The Retry route's description (orchestrator `merges.rs:1291`) promises "the plan's transaction didn't commit". E20 sends the merge back to `applying`. `apply_fresh` never reads the marker, so `target_moved` sees the digest changed by our own phase 1 and ends the merge `stale` with STALE_BEFORE_RETRY ("the plan's transaction wasn't applied").
- Result: main keeps phase 1 (NOT VALID FKs and checks, and the temporary `chelabase_merge_nn_*` checks, which also enforce on new rows). Phase 2 never runs, and the merge claims nothing happened.
- Fix:
  - In `apply_fresh`, before `target_moved`, call `marker_present`. If it's there, `advance(Applying→Online)` and `continue_online`, as recovery already does.
  - In `fail_phase`, for a Tx failure that isn't Stale (TimedOut, or an error without a psql `ERROR:` line), read the marker best-effort and go on to online when it's present.
  - Fix the Retry route's description to match.
  - Test: a fake whose phase 1 exec times out after the marker is written. Assert the merge ends online, then done, not stale.

## Minor

1. **Stale and TimedOut are classified by substring and can be spoofed** (T8a M3).
   - Where: `phases.rs:504` (`contains(STALE_ERROR)`) and `:467` (`"timed out after"`).
   - A phase 1 user step that runs `RAISE EXCEPTION 'chelabase_merge_stale'`, or any Postgres error quoting an object whose name contains that text, ends the merge `stale` (a rehearsal ends `stale` too, so recompute loops). A RAISE containing "timed out after" is classified as a timeout.
   - Fix: count as stale only an error whose psql line falls inside the guard's line range (before the first `-- step`), or give the guard a per-run random nonce. Return a typed timeout error from `admin_sql_with` (downcast `Elapsed`) instead of matching text.
2. **A failure at the marker insert or at COMMIT is blamed on the last tx step** (T8a M2).
   - Where: `plan/mod.rs:423-427`, `phases.rs:231-245`.
   - Real case: a `DEFERRABLE INITIALLY DEFERRED` FK fails at COMMIT and the last unrelated step gets the blame. A user statement with an inner `-- step <other id>` comment line can also steer the mapping.
   - Fix: emit `-- step chelabase:marker` before the insert and `-- step chelabase:commit` before COMMIT, mapped to `phase1` / "at commit". Match only marker lines the script generator wrote (track their line numbers instead of re-scanning the text).
3. **User step ids can collide with generated ids.**
   - Where: orchestrator `merges.rs:842-848` checks only length and duplicates among user steps; `plan/mod.rs:284-289` builds `<id>.<n>` ids.
   - A user id equal to a generated online id (`create-index:public.t.ix`) or to another step's `x.1` makes `online_done` (`phases.rs:422-437`) skip both steps once either logs ok. Phase 2 then finishes with a missing index (drift).
   - Fix: refuse in `plan()` any user id that equals a generated id or another statement id, and restrict the charset, or reserve `:` and `.`.
4. **Phase 0 skips a pending API role row** (T8a M1, still open).
   - Where: `phases.rs:544-549` skips any `api` row with no check on `pending`.
   - A role stored but whose SQL failed is skipped, so it's missing on the target and phase 1's GRANT to it fails.
   - Fix: skip only when `!r.pending`, or when `exists_sql` finds the role.
5. **The rehearsal's phase 0 skips the membership and administer checks** (T8a M6).
   - Where: `phases.rs:569-571` runs only `check_spec`.
   - A membership the real path refuses (`role_membership_not_allowed`, F1-a) passes the rehearsal, then fails phase 0 for real after earlier roles were created.
   - Fix: run the same membership and administer SQL checks on the rehearsal compute.
6. **A change to the owner's own attributes plans a refused `role:update:postgres`** (T13c).
   - Where: `plan/diff/mod.rs:872-909` exempts the owner (`OWNER_ROLE`) only for `member_of`.
   - A source change to `postgres`'s CONNECTION LIMIT or VALID UNTIL fails phase 0.
   - Fix: never plan an update for `OWNER_ROLE` and report it as "apply by hand". The test comment at `plan/tests.rs:698` is fine as written.
7. **Role comments and `other_attributes` cause false drift.**
   - Where: `compare.rs:540-553` compares them; `plan/diff/mod.rs:856-865` never plans them, and the roles path can't set them.
   - A source role with CREATEDB or a comment ends `applied_with_drift`, and U2's "anything else is reported" doesn't happen.
   - Fix: report them as unmerged changes, and in `expected_model` keep the target's (or no) comment and `other_attributes` for created and updated roles.
8. **The drift check compares only the names of the owner's memberships** (T13c): `compare.rs:540-542` ignores grantor and the admin/inherit/set options. Fix: compare `(role, grantor, admin, inherit, set)`.
9. **`precheck_fingerprint` projects out too much** (T13c and T8b re-review).
   - Where: `fingerprint.rs:30-62`.
   - It drops whole roles the plan only updates, and their owner grants; it clears every label of a touched enum; an unparseable 1a step clears every enum (`every_enum`). Concurrent target changes there go undetected until the finish drift check.
   - Fix: project out only `RoleOp::Create` roles plus the planned patch fields of updated roles, and only the added labels. Make an unparseable 1a step count as stale, not as a match.
10. **Phase 1a misses enum labels that need escaping.**
    - Where: `plan/mod.rs:309-311` matches `literal(label)`, which is `E'a\\b'` for a label with a backslash, while `pg_get_expr` writes `'a\b'`.
    - The value isn't moved to 1a, so phase 1 uses it in the same transaction that adds it (the rehearsal fails).
    - Fix: also match the standard-conforming form `'…'` with only quotes doubled.
11. **Phase 1a misses an add after a value is renamed away.**
    - Where: `plan/diff/objects.rs:239-271`.
    - Case: rename `a→b` and add a new `a`. 1a's `ADD VALUE IF NOT EXISTS 'a'` is a no-op on the target, then phase 1 renames `a→b`, so `a` is missing (drift).
    - Fix: when an added label equals a target label the plan renames, refuse it as Unplannable ("own SQL") or don't move it to 1a.
12. **A valid 1a user step is refused before the used-enum move.**
    - Where: `plan/mod.rs:253-282` checks the phase floor before `:303-322` moves values to 1a.
    - A user `ADD VALUE` anchored after a generated add-value step that later becomes 1a is refused.
    - Fix: run the enum move before placing user steps, or compute the floor with the final phases.
13. **The generated-column conversion is left out of the policy detach.**
    - Where: `plan/diff/tables.rs:150`.
    - `DROP COLUMN` (`:957`) fails when a policy uses the column. It also silently drops indexes and constraints on that column, which the plan doesn't re-create (drift).
    - Fix: include conversions in `retyped` for the policy detach, and re-create the dependent indexes and constraints.
14. **Policy comments are lost.** `Policy` has no comment field (`model.rs:300-310`), and the digest's `pg_description` covers only class, namespace and type. A re-created policy loses its comment unseen. Fix: model policy comments, or report them.
15. **The rewrite flag misses volatile defaults.** At `plan/diff/tables.rs:930`, `ADD COLUMN … DEFAULT random()` or `clock_timestamp()` rewrites the table without the flag. Fix: flag `rewrite` when the default isn't a constant (or call a volatility check during the rehearsal).
16. **User phase 2 statements and user `ADD VALUE` aren't resumable.**
    - Where: `plan/mod.rs:290`, where user steps get no `OnlineCheck`.
    - A crash during a user `CREATE INDEX CONCURRENTLY` leaves an INVALID index, and the retry fails with "already exists".
    - Fix: derive an IndexValid check for user `IndexStmt` statements with `concurrent` and a name, and refuse unnamed concurrent indexes. For `ADD VALUE`, require `IF NOT EXISTS` or add it.
17. **The temporary NOT NULL check name isn't unique per merge, and its attnum isn't documented.**
    - Where: `plan/diff/tables.rs:863`; doc at `exclusions.rs:71` and `tables.rs:857`.
    - A leftover check from an abandoned merge makes the next phase 1's `ADD CONSTRAINT` fail. The doc doesn't say the attnum is the target's (or the planned one for a new column).
    - Fix: add the merge-id suffix, as the temporary index names have, and document the attnum.
18. **INCLUDE quoting isn't pinned.** `catalog_live.rs:398` pins only `["email"]`. Collation quoting is pinned at `:312`. Fix: add a mixed-case INCLUDE column to the fixture and assert it comes back quoted.
19. **The export's role comments can be escaped with a bare `\r`.**
    - Where: `export.rs:21` uses `.lines()`, which doesn't split on `\r`, while Postgres ends a `--` comment at `\r`.
    - A source role name containing `\rDROP TABLE …;--` runs that SQL when someone executes the exported file.
    - Fix: split on `['\n','\r']`, or replace `\r` before commenting.
20. **The phase 1 guard's PL/pgSQL isn't parsed in a unit test.** The marker check is parsed (`plan/tests.rs:1145`), but the guard is only compared as text (`:838-854`) and covered live. Fix: parse the guard's DO body with `pg_query::parse_plpgsql`.
21. **Stale docs and T6 leftovers:**
    - `steps.rs:38-40` and `:205` still say "run as postgres inside a superuser session" (outdated since E15).
    - The trait doc at `backend.rs:87` says "as cloud_admin" and doesn't mention that a timeout leaves psql running in the compute (callers cancel by `application_name`).
    - `neon_endpoint` and `neon_role_secret` (`metadata/src/neon.rs:601`, `:639`) don't say that proxy paths must use the `routable_*` versions.
    - There's no `debug_assert` on the `PGAPPNAME` length and charset (`backend.rs:234`).
    - A stray `use std::time::Duration` sits at `backend.rs:450`.
    - `ComputeMode::Static.lsn` isn't validated as `X/Y` (`spec.rs:35-38`).
22. **An Apply redelivered after a Retry can say "interrupted, nothing applied" while roles exist** (T13 note and T8b M7).
    - Where: `apply.rs:113-115`. `apply_started` looks at the whole log, so a redelivery after a Retry finds the first attempt's steps. The wording is also false whenever phase 0 or 1a ran.
    - Fix: log an attempt start and check only for steps after the latest one. Reword to "interrupted before its transaction committed; roles and enum values it created stay".

## Status items

| Item | Status |
|---|---|
| T4 rr: generated-column conversion left out of the policy detach | still open, Minor 13 |
| T4 rr: valid 1a placement refused before the used-enum move | still open, Minor 12 |
| T4 rr: policy comments lost on detach and re-create | still open, Minor 14 |
| T4: user phase 2 statements and user ADD VALUE not resumable | still open, Minor 16 |
| T4: 1a misses an add after a value is renamed away | still open, Minor 11 |
| T4: backslash labels | still open, Minor 10 (`literal` itself is correct) |
| T4: false drift from a role's comment or `other_attributes` | still open, Minor 7 |
| T4: owner-level not flagged `security` | still open (`diff/mod.rs:868`, `:903`); question below |
| T4: rewrite flag misses volatile defaults | still open, Minor 15 |
| T4: document the nn-check attnum | still open, Minor 17 |
| T4: pin that T2 returns collation and INCLUDE quoted | collation fixed; INCLUDE open, Minor 18 |
| T4: parse the guard's PL/pgSQL body | fixed for the marker check; phase 1 guard open, Minor 20 |
| T4: default ACL read before apply; phase 0 mustn't change ACLs | fixed: the expected model is computed at compute time, the digest covers `pg_default_acl`, and the roles path issues no object GRANTs |
| T4: dependency gaps the rehearsal catches | still open by design (the rehearsal catches them); list them on the docs page, or close them as accepted |
| T6: doc `routable_*` on `neon_endpoint` / `neon_role_secret` | still open, Minor 21 |
| T6: doc that an `admin_sql_with` timeout leaves psql running | still open, Minor 21 (only phases.rs says it) |
| T6: `admin_psql_command`'s doc says "terse" | fixed (now accurate); the trait doc is stale instead, Minor 21 |
| T6: `debug_assert` the `PGAPPNAME` | still open, Minor 21 |
| T6: stray `use Duration` in `fake` | still open, Minor 21 |
| T6: `Static.lsn` not validated | still open, Minor 21 |
| T8a M2: marker/COMMIT failure misattributed; `-- step` steerable | still open, Minor 2 |
| T8a M3: Stale/TimedOut by substring | still open, Minor 1 |
| T8a M1 and M6 (phase 0, same files) | still open, Minor 4 and 5 |
| E19 | implemented (`plan/mod.rs:504-540`, `ADD CONSTRAINT … USING INDEX` labelled ACCESS EXCLUSIVE) |
| T13c: `postgres`'s own attributes plan a refused update | still open, Minor 6 (the test comment needs no change) |
| T13c: drift compares only the owner's membership set | still open, Minor 8 |
| T13c/T8b: `precheck_fingerprint` projection too wide | still open, Minor 9 |

## Questions for the user

1. **Owner-level in a merge (T4 minor).** A merge that creates a role with owner-level, or turns it on, gets no security flag; only BYPASSRLS does.
   - (a) **Recommended:** flag owner-level like BYPASSRLS, so it needs an acknowledgement, since owner-level reaches more than BYPASSRLS does. It costs one more click.
   - (b) Keep it unflagged, since the roles path already restricts owner-level; that relies on reviewers reading the plan.
2. **Role attributes the roles path can't set** (CREATEDB, CREATEROLE, REPLICATION, comments).
   - (a) **Recommended:** report them as "apply by hand" and leave them out of the expected model, as U2 says. No drift.
   - (b) Extend U2 to allow comments only (harmless). That's more API surface.# T14 review: roles/bases

Scope: the roles, keys and bases slice of `schema-merge` (diff base `933294d`), plus a re-check of the Status items in that slice. Read-only, nothing built or run.

**Verdict: Needs fixes.** No Critical findings, 5 Important, 9 Minor.

## Critical
None.

## Important

**I1. A second restore to a point before the first restore finds no base (the T7b "(fix in T14)" item is still open).**
- Where: `crates/chelabase-metadata/src/schema_bases.rs:96-125`, called from `branches.rs:1042-1051`.
- Scenario:
  1. The branch is on T1 and is restored to X1. The copied base lands on T2 at `branch_lsn = X1`.
  2. Later T2 is restored to X2, with X2 < X1. Restoring that far back is allowed, since `history_start` is kept.
  3. The query only looks at `branch_timeline_id = T2 AND branch_lsn <= X2`, so it finds nothing.
  4. `load_base` (`services/worker/src/schema_base.rs:289-300`) then returns Unavailable. The merge falls back to re-baseline even though T1's entry at or before X2 is still stored.
- No test covers this case.
- Fix: walk the restore chain.
  - Add `copied_from bigint NULL REFERENCES branch_schema_bases(id)`, or `restored_from_timeline text`, to the copied entries.
  - Turn `copy_bases_for_restore` into a recursive CTE: when the timeline has no entry at or before `at`, follow its oldest entry's `copied_from` to the earlier timeline and search there at or before `at`, at any depth.
  - Add the same walk to the fake in `services/worker/src/fakes.rs:341`.
  - Tests: a fake test for two restores (restore to X1, then to X2 < X1, then `load_base` returns the T1 entry) and a `live_db` test.

**I2. Login-role cap bypass through PATCH (new in 6g).**
- Where: `neon.rs:1250-1299` (`update_api_role`) and `neon.rs:1305-1338` (`store_role_secret`) never call `copy_count`. `copy_count` (`neon.rs:1739`) counts only rows with a verifier. The caller is worker `roles.rs:577-613`.
- Scenario:
  1. With `CHELABASE_BRANCH_ROLE_CAP=50` and 49 login roles, create any number of NOLOGIN roles. The docs say groups don't count.
  2. PATCH `login: true` on each of them.
  3. The branch now has hundreds of login roles with proxy copies. The same works for SQL NOLOGIN roles through `store_role_secret`.
- Fix:
  - When `wanted_login && !login`, check the cap in the same transaction (lock the endpoint row, `copy_count >= cap` returns `CapReached`).
  - Map it to `role_cap_reached` in the reply, and add that 422 to the `updateRole` OpenAPI responses (`services/orchestrator/src/roles.rs:646`).
  - Add a worker test and an API test.

**I3. Phase 0 skips a role whose API row exists, even a pending one or one the user made by hand (T8a M1, still open).**
- Where: `services/worker/src/merge/phases.rs:539-549` skips on any `origin == Api` row.
- Scenarios:
  - (a) The first attempt stored a `pending` row, then its SQL failed or the worker crashed. The retry skips the role, and phase 1 then fails on `GRANT … TO <role>` / `CREATE POLICY … TO <role>` until a resync happens to create it.
  - (b) Between compute and apply, the user created a role of the same name through the API with other attributes. `precheck_fingerprint` projects the plan's own roles out, so the merge isn't stale, phase 0 skips the role, and the result drifts.
- Fix:
  - Skip only when the row is `!pending` and its stored attributes equal the spec; a pending row is re-driven (as `update_role` does).
  - A non-pending row with different attributes fails phase 0 (or marks the merge stale) with a clear message.
  - Test both cases.

**I4. A rehearsal's phase 0 skips the membership and administration checks the real path runs (T8a M6, still open).**
- Where: `phases.rs:564-596` (`rehearse_create`) and `phases.rs:600-643` (`rehearse_update`). They run only the static `check_spec` / `check_patch`, then apply the change as `cloud_admin`, which can change any role.
- Scenarios:
  - A source role is a member of a group on the target that is itself a member of `pg_write_all_data`, or a group the owner doesn't administer: the rehearsal passes, and the apply fails at phase 0 with `role_membership_not_allowed`.
  - An update to a target role the owner doesn't administer: the rehearsal passes, and the real path returns `NotFound`.
- Fix:
  - Factor `check_member_targets` out of `roles.rs:666-729` into a SQL-only helper and call it in both rehearse functions.
  - In `rehearse_update`, refuse with the real outcome when the role has no API row and `!o.owner_admin`.
  - Test both refusals.

**I5. Upgrading reverts memberships and attributes users set in SQL on existing API-made roles, and the docs say the opposite (T9 "tell the user", still open).**
- Where: `resync.rs:503-556` makes the control plane win for INHERIT, CONNECTION LIMIT, VALID UNTIL, BYPASSRLS and memberships. `memberships_sql` (`resync.rs:1080-1101`) revokes every owner-granted group the row doesn't list.
- Scenario:
  1. Migration `20261010000002` gives existing API rows the defaults (`member_of '{}'`, `connection_limit -1`).
  2. On a pre-6g install, an app role made in Studio got `GRANT readers TO app` in a migration months ago.
  3. The first resync after the upgrade revokes `readers` from `app`, and the app breaks.
- The docs say SQL attributes and grants "take effect … Chelabase reads them back" (`apps/docs/content/docs/using/login-roles.mdx:79`), which is wrong for API-made roles. The receiver also lets SQL `NOLOGIN` stick on API rows (`ddl.rs` `set_login`) but not these attributes, which is inconsistent.
- Fix (direction depends on Q1 below):
  - At minimum, adopt once: add an `attributes_adopted boolean` column (false for existing rows), have the first resync record the observed attributes and memberships into the row instead of repairing, then the control plane wins after that.
  - Document the rule in `login-roles.mdx`.

## Minor

- **M1. An unrelated PATCH re-grants a dropped group (T9, still open).**
  - Where: worker `roles.rs:574`. `patch.apply(&attributes)` uses the stored `member_of` unfiltered, while the resync filters out groups that no longer exist (`resync.rs:516-523`).
  - Scenario: a group is dropped in SQL, then an unrelated PATCH (say `connection_limit`) emits `GRANT "g"`, fails, and the whole PATCH fails.
  - Fix: filter `member_of` to groups that exist (from the catalog read) before `apply`, as the resync does.
- **M2. The resync's header still says the control plane wins for LOGIN (T10b note, still open).**
  - Where: `resync.rs:1-4`.
  - Scenario: the receiver's `SetLogin` writes `login` on API rows, so an SQL `NOLOGIN` sticks; the header is wrong.
  - Fix: reword it to "existence, password, the U2 attributes and memberships; LOGIN follows forwarded SQL".
- **M3. The `updateRole` description says it works on any role made in SQL (T12, still open).**
  - Where: orchestrator `roles.rs:632`, and `openapi/management.json:3730`.
  - Scenario: "Works on roles made through the API and in SQL" contradicts the 404 text, which refuses roles the owner doesn't administer.
  - Fix: "… roles made through the API, and roles made in SQL that the owner administers", then `just api-gen`.
- **M4. The static-compute cleanup timeout isn't tied to the read's timeouts (T7, still open).**
  - Where: `schema_base.rs:29`. `STALE_STATIC_COMPUTE_SECS = 600` sits apart from `ready_timeout` (60 s) and `CATALOG_READ_TIMEOUT` (60 s).
  - Fix: a `const _: () = assert!(…)` against the timeouts (moving `ready_timeout`'s default into a const), or at least a comment giving the arithmetic.
- **M5. Waiting for a static compute always runs to the full timeout (T7, still open).**
  - Where: `provision.rs:273-284` (`wait_until_ready`).
  - Scenario: a static compute that exits at once (for example a basebackup refused) still waits the whole 60 s.
  - Fix: check `computes.status` in the loop, and fail at once on `NotRunning`.
- **M6. Long `use` line (T7, still open).**
  - Where: `services/worker/src/branch_create.rs:5`.
  - Fix: wrap it by hand (no workspace `cargo fmt`).
- **M7. Base history grows without bound, and every restore copies a full model (T7b, still open).**
  - Where: `schema_bases.rs`. `prune_schema_bases` only removes databases the branch no longer has.
  - Fix: add a `recorded_at` column and prune entries made unreachable by a newer entry on the same timeline recorded more than `history_window` ago. Restores can't reach before the window, so this loses nothing. Keep the newest entry per timeline.
- **M8. A restore by time or LSN isn't bounded below by the branch's start (T13b concern 4, still open).**
  - Where: `services/worker/src/point.rs:56-64` (LSN restores only check `lsn > head`); the orchestrator's restore route doesn't check either.
  - Scenario: after a first restore, a child branch can be restored to before it was created, which yields the parent's older state and a base re-baseline.
  - Fix: refuse a point before `history_start` (time), or before `branched_at_lsn` on the first timeline (LSN), with a clear 422.
- **M9. Reconcile can drop a restore's pending timeline while its detach may still be running (T13b concern 5, still open).**
  - Where: `branch_switch.rs:451-459`. `finish_switch` does this after `STALE_PROVISION_SECS` (600 s), while detaches are allowed up to `STUCK_DETACH_SECS` (20 min).
  - Fix: for a restore whose pending timeline is younger than `STUCK_DETACH_SECS`, resume it with `resume_restore` instead of dropping it.

Still open but cosmetic (no separate finding): a rehearsal still gets a public data API key from its bootstrap. It is never served (`lookup_data_key` only matches `normal` branches) and goes with the branch; skipping the key insert for rehearsals would remove it. The proxy's "password authentication failed" for an unroutable endpoint is in the checklist.

## Checklist of Status items in this slice
- T7b "(fix in T14)" second restore walks the ancestor chain: **still open → I1**
- T7b history unbounded / a full model copied per restore: **still open → M7**
- T7b copy's live location points at the backup timeline (document it): **fixed** (`merging-schema.mdx:273` describes the deleted-backup case)
- T7b re-baseline wording mustn't blame a restore: **fixed** (Studio has no restore wording; the worker's message only lists it as one possible cause)
- T7 `wait_until_ready` waits the whole timeout: **still open → M5**
- T7 cancelled `load_base` cleanup and tying `STALE_STATIC_COMPUTE_SECS` to the timeouts: **still open → M4** (reconcile does clean up after 600 s)
- T7 long `use` line in `branch_create.rs`: **still open → M6**
- T7 create-time read equals the live fallback; static compute boot, `cloud_admin`, `CATALOG_SQL` on a standby: **fixed** (live `a_base_read_live_equals_the_create_time_base` plus the T13l live tests)
- T9 first resync after upgrade revokes SQL-made grants and attributes on API roles: **still open → I5**
- T9 partial strip of an escalated plain API role (only BYPASSRLS reverted; CREATEDB/CREATEROLE/REPLICATION/`neon_superuser` stay): **still open → Q2**
- T9 dropped group re-granted by an unrelated PATCH: **still open → M1**
- T9 psql stderr carrying a verifier in `Failed`: **fixed** (`VERBOSITY=terse`, so no `LINE` excerpt; `chelabase-neon/src/backend.rs:180-186`)
- T8a concern / I3, `exists_sql` and every `cloud_admin` site pinned: **fixed** (`exists_sql`, `membership_check_sql`, `role_read_sql`, `resync_sql`, `run_groups`, `check_sql`, `cancel_sql`, `backends_sql`, `role_check_sql`, `schema_check_sql`, `KEY_ROLES_SQL` and `DATABASES_SQL` all pin; the drop path's `REASSIGN`/`DROP OWNED`/`DROP ROLE` are utility statements with no operator lookup)
- T8a M1, pending API row skipped in phase 0: **still open → I3**
- T8a M6, rehearsal phase 0 skips the membership and administer checks: **still open → I4**
- T13b (2) a branch from the backup at head may miss writes: **fixed** (round 2's D11 head uses the safekeepers' commit LSN)
- T13b (3) a failed restore leaves the timeline detached: **fixed** (one-detach flow detaches only the pending timeline; `a_grandchild_restore_takes_one_detach_and_retries_a_503` asserts the old ancestor is unchanged)
- T13b (4) a restore by time can reach before `history_start`: **still open → M8**
- T13b (5) reconcile drops the restored timeline after 600 s while a detach may run: **still open → M9**
- T13 note, the proxy says "password authentication failed" for an unroutable endpoint: **still open** (cosmetic; `internal/proxy.rs` returns not found, so the proxy's text follows). Fix: have `access_control` answer an unroutable endpoint with the same not-found wording `wake_compute` uses.
- T13 note, a rehearsal still gets a data API key: **mitigated** (`lookup_data_key` serves only `normal` branches); the key itself is cosmetic and goes with the branch.
- T10b note, the `resync.rs:3` header: **still open → M2**
- T12 deferred, `updateRole` description: **still open → M3**
- E26 (key bound to OID + timeline: Follow only on the same timeline, re-stamp or revoke after a switch): **verified** (`resync.rs:68-115`, exhaustive test; the bootstrap regrant is OID-bound)
- U1/U2, F1-a, prefix-aware reserved names, PATCH on non-API roles needing `owner_admin`: **verified** (worker `roles.rs:249-305` and `:562-566`, `role_name.rs`)
- E17 narrowed and the T13l parent-timeline read: **verified** (`schema_base.rs:428-447` reads behind GC only when the point equals the timeline's branch point, on the ancestor)
- P4 static and rehearsal endpoints unroutable: **verified** (`routable_*` lookups in `access_control` and `wake_compute`; static rows cleaned by reconcile)

## Questions for the user
- **Q1. Who wins for attributes and memberships of API-made roles changed in SQL? (decides I5)**
  - (a) **Recommended:** the control plane wins, with a one-time adoption on upgrade, and the docs say so. This is the current design, minus the upgrade break.
  - (b) The database wins: the touched resync records SQL changes onto API rows, as LOGIN already does. That is simpler for users, but a merge's phase 0 changes can be undone by hand.
  - (c) Keep the code as it is (reverts on upgrade) and only document it.
- **Q2. A plain API role escalated in SQL (CREATEROLE/CREATEDB/REPLICATION or `neon_superuser` granted by the owner):**
  - (a) **Recommended:** the resync strips it back to plain (control plane wins for owner level), matching how owner-level is restored today.
  - (b) Keep only the BYPASSRLS revert, as now, and document the asymmetry.

The coordinator collects the merge state machine, schema-crate (E22) and Studio items from the other slices; I didn't review those.# T14 review: analysis/catalog

Scope: review of the engine's model, catalog reader and three-way analysis (`crates/chelabase-schema`: model, identity, catalog plus its SQL, exclusions, fingerprint, `analysis/**`, `catalog_live`), plus the Status items in that slice. Read-only, nothing written.

**Verdict: Needs fixes.** Two user rulings are still unimplemented (E16 and the T3 "fix before merge" item), and role changes the roles path can't make are merged anyway, so they fail at apply or show as false drift.

## Important

**I1. E16 is not implemented, and the docs already claim it.**
- Where: `catalog_sql.sql:295-307`, `exclusions.rs` (no entry), no test.
- What happens: `default_privileges_j` keeps every `defaclnamespace = 0` row. So the data API's `FOR ROLE postgres REVOKE EXECUTE ON FUNCTIONS FROM PUBLIC` row stays in the model.
- Failure: a branch with the data API on, merged with one where it's off, plans a default-privilege change. T12's merge docs page says this row is ignored.
- Fix: in `default_privileges_j`, drop a row only when all of these hold: `defaclnamespace = 0`, `defaclobjtype = 'f'`, the role is the owner, and its ACL is exactly the owner's EXECUTE. Name it in `exclusions.rs`. Add a unit test (`catalog_sql_names_every_exclusion`) and a `catalog_live` assertion. Also check that `default_acl_sql()` still keeps the row for P7 simulation (functions aren't simulated, so dropping it there too is fine).

**I2. The E12a check is still one-sided (T3 re-review "fix before merge").**
- Where: `analysis/integrity.rs:82`. Only `Pick::Source` texts are checked.
- Failure: the source renames `t.a` to `t.b`. Main, meanwhile, adds `CREATE INDEX ... ON t (a)` (or a policy or check naming `a`). That item is `TargetOnly`, so M keeps the text `(a)`. After the apply, Postgres prints `(b)`, so compare reports `applied_with_drift`. Under E21 that also skips the after-merge choice.
- Fix: for target-picked entries whose `text_refs` resolve to an object whose merged name differs from the target's (the mirror of lines 87-93), add their keys to a `follows_rename` list on `MergedModel`. Compare treats those keys like `own_sql_keys`: it checks them against what the rehearsal produced. The other option is a symmetric `Dependency` conflict, as the reviewer suggested. Test both directions.

**I3. Role changes the roles path can't make are still classified Apply and planned.**
- Where: `analysis/index.rs:905-934` (a role's canonical form compares every field), `plan/diff/mod.rs:851-911`, `compare.rs:539-555`.
- Failure cases:
  - (a) **Role rename on the source:** the analysis follows the OID, so it's an Alter (`tests.rs:1300` asserts M uses the new name). The planner matches roles by name, so it emits `role:create` for a new role with a new password and moves grants to it. Main's live role then loses its grants and secret keys, with no flag.
  - (b) **`other_attributes` (CREATEDB…) or comment changes:** never planned, but compare checks them, so the result is false drift (T4 minor).
  - (c) **`postgres`'s own attributes:** this plans `role:update:postgres`, which phase 0 refuses at apply (T13c minor).
  - (d) **Memberships in `pg_*` or reserved roles:** planned, and refused only at the real apply. The rehearsal skips the membership checks (T8a M6), so it passes.
- Fix: in the analysis, turn any role change outside U2 into a `ReportedChange` ("apply by hand") and keep the target's value in M. That covers renames, `other_attributes`, comment, any change to `OWNER_ROLE`, and memberships the `is_reserved_target` / `pg_` rule refuses. Add a test for each case.

**I4. The index validity flag leaks into the merge.**
- Where: `index.rs:772-775` (the canonical form keeps `valid`), `merge.rs` (copies it), `compare.rs:410` (compares it). The planner ignores `valid`.
- Failure: a failed `CREATE INDEX CONCURRENTLY` on the branch leaves an INVALID index. The analysis applies it as a create, the planner builds it valid, and the drift check sees `valid=false` expected against `true`, so the result is `applied_with_drift`.
- Fix: report invalid source indexes as `Reported` (not merged), and leave `valid` out of the canonical form and the merged model, or force it to true there.

## Minor

- **M1. Pre-check fingerprint is too permissive** (`fingerprint.rs:30-45`, the T13c and T8b minors are still open):
  - it hides roles that the plan only updates, and the owner's grants in them;
  - a 1a statement that doesn't parse clears every enum's labels, which leans permissive rather than stale;
  - it clears all labels of a touched enum instead of only the added ones.
  - Fix: use `RoleOp::Create` names only for the owner-grant projection; on a parse failure return a value that compares as stale; remove only the labels the step adds.
- **M2. Own-SQL exemption is inconsistent** (`merge.rs:131-139` vs `349-363` and `445-457`): `broken()` skips items involving own SQL, but `Asm::attnum` still raises `BrokenReference`. Apply the same `own` exemption in `Asm::model`, or skip such references.
- **M3. `named_columns` drops everything when one part fails to parse** (`index.rs:379-383`): an unparsable index or predicate loses all its text references. Parse each key, the INCLUDE list and the predicate separately.
- **M4. IdentityMap duplicate side OIDs are silent** (`identity.rs:159-166`): `index_pair` overwrites. Detect a second base object for the same `(class, side OID)` in `from_pairs` and `record`, and return an error or drop both mappings so they're matched by name. Add a test.
- **M5. Default-ACL filter ignores the role** (`catalog_sql.sql:295-296`): the anon/authenticated per-schema filter doesn't check `defaclrole`, so a user's own `FOR ROLE other … TO authenticated` row is hidden. Add `AND dr.rolname = 'postgres'`, and the same in `DATA_API_DEFAULT_ACL_FILTER`.
- **M6. INHERITS children lose their parent link** (`catalog_sql.sql:39-46`): they're read as plain tables, and a merge recreates them without INHERITS. Add a reported row for `pg_inherits` children that aren't partitions (for example `Other("inherits")`), and skip planning those tables.
- **M7. Reserved roles only list two `chelabase_` names** (`catalog_sql.sql:33-38`, `exclusions.rs:26`): the roles path reserves every `chelabase_*` name, so a SQL-made `chelabase_x` role gets planned and is then refused. Use `rolname LIKE 'chelabase\_%'`.
- **M8. Fixture assertion gaps** (`catalog_live.rs`):
  - no assertion on schema grants;
  - no check that enum label OIDs are distinct;
  - INCLUDE is only pinned with a plain name (`email`): add a quoted INCLUDE column;
  - `two_reads_are_identical` (495-506) reads twice in one session: use a second connection.
- **M9. Backslash enum labels:** `sql::literal` handles them, but no planner or digest test uses a label containing `\`. Add one to the planner tests.

## Status item checklist

| Item | Status |
|---|---|
| E16 (exact-shape exclusion, test, `exclusions.rs`) | still open → I1 |
| T2: anon/authenticated filter applies to any role | still open → M5 |
| T2: unparsable index definition fails the read | fixed (the error carries the definition, which names the index) |
| T2: INHERITS link lost | still open → M6 |
| T2: fixture gaps (schema grant, label OIDs) | still open → M8 |
| T2: nn check absent from constraints | fixed (`merge_leftovers_are_reported`) |
| T2: `two_reads_are_identical` uses one session | still open → M8 |
| T2: cluster-wide `pg_roles` entries | fixed by design (reserved list; roles are per-branch) |
| T1: IdentityMap side-index overwrite | still open → M4 |
| T3: E12a symmetric | still open → I2 |
| T3: own-SQL exemption inconsistency | still open → M2 |
| T3: `named_columns` all-or-nothing | still open → M3 |
| T3: policy text false positives | still open, accepted conservative (not listed as a finding: they only add conflicts) |
| T13c: drift compares only the membership set | still open (`compare.rs:540`); fold into the I3 test suite |
| T13c: pre-check hides owner grants in updated roles | still open → M1 |
| T13c: `postgres` attributes plan a refused update | still open → I3(c) |
| T7: create-time read vs live fallback produce equal models | fixed (`merge_live` `a_base_read_live_equals_the_create_time_base`) |
| T4: collation and INCLUDE already quoted | collation pinned (`pg_catalog."C"`); INCLUDE only with a plain name → M8 |
| T4: backslash labels | partly: `sql::literal` handles it, no planner test → M9 |
| T8b: projection hides changes to planned roles and all labels of a touched enum; parse failure isn't stale-leaning | still open → M1 |
| T4: owner-level isn't flagged `security` | still open → question 1 |

## Questions for the user

1. **Should becoming owner-level (membership in `neon_superuser`) need an acknowledgement, like BYPASSRLS (P9)?** Today `classify.rs:381` and the planner flag only BYPASSRLS.
   - (A) **Recommended:** yes, flag `security`, since owner-level is stronger than BYPASSRLS.
   - (B) No, keep BYPASSRLS only.
2. **What should a role rename on the source do (I3a)?**
   - (A) **Recommended:** report it, never merge it (keys and passwords are bound to the name and OID on the target).
   - (B) Rename it on the target through the control plane: needs a rename path, and keys move with it.
   - (C) Make it a conflict (keep target or own SQL).
3. **How should the false drift in I2 be handled?**
   - (A) **Recommended:** compare those objects against what the rehearsal produced (no extra user step).
   - (B) A symmetric `Dependency` conflict: safer, but it asks the user about a rename Postgres applies correctly on its own.

Outside this slice: the planner matches roles by name (`plan/diff/mod.rs:854`, the root of I3a), and the worker side of T8a M6 (the rehearsal's phase 0 skips the membership checks) is still open.# T14 review: Studio/docs/API

**Scope:** I reviewed the 6g surfaces (Studio from T11, docs from T12, the OpenAPI spec and client) against the code, and ran the Studio and docs checks. This was read-only: no edits, no commits.

**Verdict: Needs fixes.** Two Studio paths that rulings E20 and E21 promised are missing. E16 is documented but not implemented. Several docs claims describe the code as it was before T8b.

## Command results
- Studio typecheck: passes.
- Studio tests: 605 of 605 pass (71 files).
- Docs typecheck: passes.
- Docs build: passes.
- Docs tests: 14 of 14 pass after the build. Run before the build, `tests/build-output.test.ts` fails because `dist/client` is missing, so the tests depend on a prior build.

## Important
**I1. E16 isn't implemented, but the docs say it is.**
- Code: `crates/chelabase-schema/src/catalog_sql.sql:295-307` filters anon/authenticated only on per-schema rows. Every global row (`defaclnamespace = 0`) is kept, including the data API's `FOR ROLE postgres REVOKE EXECUTE ON FUNCTIONS FROM PUBLIC`.
- Docs claim it's excluded: `merging-schema.mdx:263` and `architecture/schema-merge.mdx:32`.
- Failure: merging a branch with the data API on against one with it off shows a spurious default-privilege change. The plan item says "done in the T14 fix batch", which hasn't run yet.
- Fix: drop the row only when it is exactly the owner's global `'f'` EXECUTE revoke from PUBLIC. Add a pinning test, name it in `exclusions.rs`, and add a live check.

**I2. Studio has no Retry for a merge that failed in `roles`, `enum_add` or `tx` (E20).**
- Code: `merge-apply.tsx:120` shows Retry only for `failed_phase === "online"`. `merge-page.tsx:386-389` tells the user "Recompute to try again".
- Failure: after phase 0 created roles, or phase 1a committed enum values, the user can't use the E20 apply-from-start path. Recompute instead builds a new plan around the partial changes.
- Fix: show "Retry the apply" for those three phases (`retryMerge`, which the API already supports at `orchestrator/src/merges.rs:1321`). Word it to say the roles and enum values from the first attempt may already be on the target. Add a test.

**I3. No Studio button for `finishMerge` (E21); the Status says it comes in the T14 fix batch.**
- Code: no `finishMerge` anywhere in `apps/studio/src`. `merge-finish.tsx` only saves `options.after`, and that is disabled because the merge isn't editable in `applied_with_drift`.
- Failure: a drifted merge's keep, reset or delete can't be run from Studio at all.
- Fix: in the Finish section, when the status is `applied_with_drift` and `result.after.skipped` is set, offer "Run the after-merge choice". Into the parent it offers keep, reset or delete; from the parent, keep only. Poll until `requested` is replaced by an outcome. Add tests.

**I4. The docs predate T8b, E19, E20 and E21 in several places.**
- `merging-schema.mdx:55` and `:229` describe retry as online-only. E20 also re-runs the apply from the start after a `roles`, `enum_add` or `tx` failure.
- The route table (`merging-schema.mdx:46-57`) leaves out `POST …/finish`. The "After apply" section (`:233-243`) doesn't say that drift skips the after-merge choice, or how to run it. `architecture/schema-merge.mdx:12` lists the jobs without `finish`.
- E19 isn't documented. Phase 2 steps that take strong locks (and phase 1a's `ADD VALUE`) use the merge's `lock_timeout`; a timeout fails the merge, and Retry resumes. The docs say the opposite: `merging-schema.mdx:214` gives `lock_timeout` for phase 1 only, `architecture:109` says phase 2 runs with `statement_timeout = 0` and nothing else, and `architecture:155` says phase 2 steps have no timeout inside Postgres.
- The stale check (`architecture:115`, `merging-schema.mdx:217`):
  - The digest was read at compute time, not "the one the rehearsal saw".
  - The worker's pre-check (digest plus the full fingerprint) runs before phase 0, not in the API request. So a role or enum-value change on the target ends `stale` before anything runs, rather than "phase 1 fail and roll back".
  - Phase 1's guard is the backstop.
- `architecture:119` and `:154` say the owner session uses the compute's local socket. The code connects to `127.0.0.1` (`crates/chelabase-neon/src/backend.rs:187-196`).
- Recovery (`architecture:144`, `merging-schema.mdx:231`):
  - It omits that recovery first cancels the merge's running statements and fails closed while they still run.
  - It omits that an apply that never started simply runs.
  - "Nothing applied" is wrong when phase 0 or 1a already ran (see I5).
- Rehearsal outcomes (`merging-schema.mdx:188-189`, `architecture:138`):
  - A rehearsal that hits the stale guard ends `stale`, not `draft`.
  - A rehearsal with drift ends `rehearsed`, with the drift shown.
- Missing entirely:
  - A rehearsal left behind by a failed teardown can block reset or delete of the target for up to 30 minutes, until reconcile removes it.
  - An open DDL transaction on the target holds the data API's `chelabase.schema_version` row, which blocks every merge DDL statement.
  - Changes the source makes to the owner's memberships are never merged (E22).
  - Keep's `base: not_advanced`, when the source changed since the compute.
- Fix: rewrite those paragraphs to match the code.

**I5. The "interrupted, nothing applied" wording is wrong (T8b M7, still open).**
- Code: `services/worker/src/merge/apply.rs:39` and `:113-115`. Studio's `merge-page.tsx:261-264` then shows that error under the heading "The last compute couldn't finish the plan."
- Failure: after a crash between phase 0 and the commit, roles or enum values are on the target, while both the message and the heading say nothing happened. The stale notice (`merge-page.tsx:379`, "nothing was applied") has the same problem after a retried apply (`STALE_BEFORE_RETRY`).
- Fix: change the text to "interrupted before the plan's transaction committed; roles and enum values from phases 0 and 1a may be on the target". Choose the Studio heading by status and error, not always "compute".

## Minor
1. **The apply progress never counts phase 1** (T8b M9). `merge-apply.tsx:56-57` counts done steps by the plan's step ids, but the worker logs phase 1 once, as `phase1` (`phases.rs:44`, `:340`), so "Phase 1: n of m" stays at 0. Fix: when a `phase1` ok event exists, count all `tx` steps as done.
2. **`result.after` is shown as raw values** (T8b M9). `merge-finish.tsx:65-70` joins the strings, giving text like "keep: not_advanced" or "reset: …". Fix: give `refused`, `skipped`, `error`, `base: recorded`, `base: not_advanced`, `base: not_recorded` and `outcome: done` their own wording.
3. **Own-SQL output is listed as drift after apply.** `merge-finish.tsx:128-137` lists every `result.drift` item in red, including the ones marked `produced`. Fix: filter them as `merge-rehearsal.tsx:34-35` does.
4. **The full plan document is served in `getMerge`** (T8b M10, open). `orchestrator/src/merges.rs:261` includes `plan.expected`, `own_sql`, `target_digest` and `precheck_fingerprint`, about 100 KB per poll while the page polls. Fix: project the plan to `{steps}` in `From<MergeRecord> for Merge`.
5. **Re-baseline wording.** `merge-page.tsx:235-238` says the base "wasn't saved when X was made, and it is now older than the history window". That's wrong for a model-version upgrade, a restore copy whose backup was deleted, or a newest entry on another timeline (T7b ruling R2). The page also hides the worker's reason, because the error panel is gated on `!gone`. Fix: neutral wording plus the text after `merge_base_unavailable: `. It already doesn't blame a restore.
6. **The docs say `error` equals `merge_base_unavailable`** (`merging-schema.mdx:273`, `errors.mdx`). It actually starts with `merge_base_unavailable: ` followed by the reason (`compute.rs:145`).
7. **The `updateRole` description is still contradictory** (T12 minor). `orchestrator/src/roles.rs:632` says it "works on roles made through the API and in SQL". Fix: "…and roles made in SQL by the owner", then regenerate the OpenAPI spec and clients.
8. **Five `<pre tabIndex={0}>` blocks have no accessible name** (T11 minor). They're at `merge-changes.tsx:56`, `merge-plan.tsx:141` and `:234`, and `merge-rehearsal.tsx:106` (fine there, inside a `<figure>` but…). Fix: add an `aria-label` naming the step or object and its side.
9. **Unsaved edits vanish** (T11 minor). `sectionKey = version-status` (`merge-page.tsx:185`) remounts every section after any save or version conflict, so unsaved work in other sections (conflict choices, step SQL, acknowledgements) is lost. Fix: keep drafts in the page state, keyed by item, and reset only the saved section.
10. **Two primary buttons show during the protected confirm** (T11 minor). In `merge-apply.tsx:78-118`, the disabled "Apply to X" stays rendered next to "Apply to protected X". Fix: hide the first one while confirming.
11. **The open-merge check ignores the database** (`lib/branches.ts` `openMergeFor`). The API's one-open rule is per (source, target, Postgres database), so an open merge on another database disables the button wrongly. The merges list query isn't polled either, so the button can stay disabled after a merge ends. Fix: filter on the main database, and refetch the list on focus or after a merge page action.
12. **The `.sql` export has no `SET LOCAL lock_timeout` or `statement_timeout` in its phase 1 block** (`crates/chelabase-schema/src/export.rs:37-44`). A hand run with `psql` can queue behind long locks. Fix: emit both from the merge's options, with a comment that the stale guard and the marker aren't included.
13. **The docs' delete-refusal example is wrong.** `merging-schema.mdx:241` gives "`has_children`", but `result.after.refused` holds a sentence, not a code (`finish.rs:578`).
14. **T10 minors still open:**
    - The reverts after a publish failure swallow errors (`let _ =` at `orchestrator/src/merges.rs:701`, `941`, `1058`, `1167`, `1275`, `1358`, `1495`). Fix: log them at `error!`.
    - Rehearse and apply don't refuse a branch that isn't ready in the API (`:1036`, `:1107`). The worker covers it by returning to draft or rehearsed with `branch_not_ready`, so this is a 202 that doesn't stick. Fix: pre-check and return 409 `branch_not_ready`.
15. **The generated `exportMergePlan` client parses JSON and fails on the `text/plain` answer.** Only `fetchMergePlanSql` works. Fix: mark the generated one deprecated, or drop it from the package's re-exports.
16. **The docs owe a short Studio section** (T12 note) and don't have one. Fix: add it to `merging-schema.mdx` (branch actions with their reasons, the merge page sections, the merges list).

## Checklist (my slice)
| Item | Status |
|---|---|
| E21 Studio finish button | open, I3 |
| T8b M9: phase 1 ticks | open, Minor 1 |
| T8b M9: `after` wording | open, Minor 2 |
| T8b M10: strip `expected` / `own_sql` | open, Minor 4 |
| T11: `<pre>` accessible name | open, Minor 8 |
| T11: tell users acknowledging returns to draft | fixed (`merge-plan.tsx:201-203`, `merge-conflicts.tsx:140`) |
| T11: progress counts exclude rehearsal events | not applicable (rehearsals log no events, `phases.rs:395`) |
| T11: unsaved edits vanish | open, Minor 9 |
| T11: two sand buttons | open, Minor 10 |
| T7b: re-baseline wording mustn't blame a restore | fixed as stated; the wording is still inaccurate, Minor 5 |
| T7b: document the backup-timeline copy | fixed (`merging-schema.mdx:273`) |
| T12: `merge_base_unavailable` leaves a draft | true; the docs' exact error text is off, Minor 6 |
| T12: rehearsal name, unroutable, NOLOGIN, timings only | fixed |
| T12: failure returns to draft | partly; stale and drift outcomes undocumented, I4 |
| T12: phase 0 through the roles path; 5 s default; cancel by `application_name` | fixed (E19 missing, I4) |
| T12: keep, reset, delete; refusal in `result` | fixed (wording, Minor 13; `not_advanced`, I4) |
| T12: `result.created_roles` (after T13c) | fixed (`finish.rs:349` matches `merging-schema.mdx:243`) |
| T12: `applied_with_drift` and the identity map | correct; the skipped choice is undocumented, I4 |
| T12: recovery from the marker; reconcile rules | open, I4 and I5 |
| T12: resolutions kept only if their conflict is unchanged | fixed (`compute.rs` `kept_resolutions`, tests) |
| T12: `updateRole` description | open, Minor 7 |
| T12: Studio section | open, Minor 16 |
| T8b docs: stale check is pre-check plus guard; digest is compute's | open, I4 |
| T8b docs: a stale rehearsal ends `stale` | open, I4 |
| T8b docs: recovery cancels running statements first | open, I4 |
| T8b docs: a leftover rehearsal blocks for up to 30 min | open, I4 |
| T8b docs: owner session on `127.0.0.1` | open, I4 |
| T13 docs: open DDL transaction blocks merge DDL | open, I4 |
| T13c docs: owner memberships never merged (E22) | open, I4 |
| T13b docs: double detach / 120 s wait | superseded and absent (`branches.mdx` says one detach at any depth) |
| E16 documented | the docs claim it, but the code doesn't do it, I1 |
| E17 narrowed text | fixed (`merging-schema.mdx:273`, `architecture:62`, `errors.mdx`) |
| No role-lag notes | fixed (none in 6g docs) |
| T10: reverts swallow errors; rehearse/apply ready check | open, Minor 14 |
| T10: cross-database 404 tested only for `getMerge` | not checked (Rust tests, outside my slice) |

Outside my slice, but worth noting:
- The T13 note "an Apply redelivered after a Retry is treated as recovery" still holds: `merge/mod.rs:197` and `apply.rs:113` send it back to `rehearsed` when the log has any step.
- The T2 minor still holds: the anon/authenticated default-ACL filter applies to any owner role (`catalog_sql.sql:295`).

## Questions for the user
1. **Should acknowledgement-only or after-choice-only saves keep a rehearsed merge rehearsed?** Today every edit returns to `draft`, which means recompute and rehearse again, though neither change touches the plan.
   - (a) Keep `rehearsed` for acknowledgement and `options.after` saves; any other edit returns to draft. **Recommended.**
   - (b) Keep today's rule (spec D14 wording); Studio already warns.
   - (c) Also keep it for `lock_timeout` changes. Not recommended: the phase 1 script changes.
2. **Should the `.sql` export include phase 1's `SET LOCAL lock_timeout` and `statement_timeout`** (Minor 12)?
   - (a) Yes, from the merge's options, with a comment that the stale guard and the marker are left out. **Recommended.**
   - (b) No; it's a reference listing only.