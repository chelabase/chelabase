# Step 6g T14, F3 implementer report

F3 = the merge state machine and apply (opus). Branch `schema-merge-f3`, commits `2bad3a2` (state table, failed-online hold, Abandon, API), `562f6f3` (typed admin timeout, static LSN check, minor 21), `59c3b79` (worker state machine and apply). At the last commit: workspace build, `cargo test --workspace`, clippy `-D warnings`, `just api-gen` + drift tests, and the Studio typecheck all pass. At `2bad3a2` alone two orchestrator openapi unit tests fail (the Abandon route's registry entry landed in `59c3b79`). Live tests compiled (`--no-run`), not run.

## 1. Checklist (SM = state-machine slice, AP = apply/planner slice)

- **SM I1 / AP I1** (phase 1 committed but recorded failed in tx): F3. Tests `a_phase1_timeout_after_commit_goes_on_to_online`, `a_retry_after_a_tx_failure_with_the_marker_goes_on_to_online`, `a_phase1_failure_without_the_marker_stays_failed_in_tx` (guard). Retry route description fixed. RED: the first two failed.
- **SM I2 / AP M22 / T8b M7 / Studio I5 (worker wording)**: F3. The API logs `status → applying` in the move's transaction; the worker scopes "started" and `online_done` to the current attempt; `INTERRUPTED` reworded. Tests `a_redelivered_apply_after_a_retry_runs`, `online_steps_of_an_earlier_attempt_dont_count`, `crash_before_commit_returns_to_rehearsed` (updated), API `apply_and_retry_log_the_attempt_start`. RED: all failed.
- **SM I3 / E27** (failed-online hold): F3. Migration `20261013000001` rebuilds both unique indexes; online resume checks the marker; a gone target fails for good. Tests `a_resume_in_online_without_the_marker_fails_for_good`, `a_resume_with_the_target_gone_fails_for_good`, plus the exhaustive tests. RED: both failed.
- **SM I4** (cancelled rehearsal frees branches early): F3. A live rehearsal child holds its parent. API test `a_leftover_rehearsal_holds_its_branch`; no RED (the API fake models the new query); real SQL in live `claim_refuses_a_branch_a_merge_holds`.
- **SM M1 / AP M4 / roles I3** (phase 0 skips a pending row): F3. Tests `phase0_fails_on_a_settled_row_with_other_attributes`, `phase0_drives_a_pending_row_again`. RED: both failed.
- **SM M2 / AP M1, M2** (phase 1 blame, stale classification): F1 wrote the script parts; F3 wired `at_line`, guard-only stale, typed timeout. Tests `phase1_failures_map_by_the_script_parts`, `a_timeout_is_told_by_its_type`, neon `admin_sql_with_passes_app_name_and_timeout`. RED: all failed.
- **SM M3 / AP M5 / roles I4** (rehearsal skips membership/administer checks): F3. Test `the_rehearsal_refuses_what_the_apply_would`; `rehearsal_phase0_creates_nologin_roles_by_sql` updated. RED: failed.
- **SM M4** (refused resolution dropped silently): F3. `a_refused_resolution_is_named_in_the_error`. RED: empty error.
- **SM M5** (cancel during rehearsal build): F3. `a_cancel_during_the_build_stops_it`. RED: timed out after 10 s.
- **SM M6** (`failed_phase` NULL after the marker): F3. `an_unreadable_plan_after_the_marker_fails_in_online`. RED: failed.
- **SM M7** (reconcile doc, young rehearsals): F3. `a_young_rehearsal_is_left_to_its_job`. RED shown by reverting the change.
- **SM M8** (finish edge cases): F3. `finishMerge` guarded on the result it read; the worker runs the recorded choice; reconcile re-publishes `Finish` for lost jobs; reset/delete record `started` first. Tests: API `concurrent_finish_requests_publish_once`, `finish_by_hand_runs_the_recorded_choice`, `a_finish_run_again_after_its_reset_reports_it_done`, `a_finish_run_again_after_its_delete_reports_it_done` (passed already), `a_lost_finish_job_is_published_again`. RED: failed except where noted.
- **SM M9 / AP M9** (pre-check projection): F1.
- **SM M10** (duplicate Resume jobs): F3. `STALE_MERGE_CLAIM` 180 s, `REHAND_AFTER` 30 min, column `merges.handed_back_at`. Tests `a_handed_back_merge_waits_for_its_job`, `a_merge_is_silent_after_three_claim_refreshes`, live `hand_backs_wait_for_their_job`. RED: compile error.
- **SM M11 / Studio minors 4, 14** (API surface): F3. Plans served as `{steps}`; reverts after a failed publish logged at `error!` (no test); cross-database 404 on every route; rehearse version-guarded; rehearse/apply 409 `branch_not_ready`. Tests `get_merge_serves_only_the_plan_steps`, `every_merge_route_is_404_for_another_database`, `rehearse_is_guarded_by_the_version`, `rehearse_and_apply_refuse_a_branch_that_isnt_ready`. RED: all failed.
- **SM M12**: TOCTOU (claim checks under the branch row lock; holding moves take share locks on both branches): `a_merge_that_takes_the_branch_after_the_precheck_is_refused_by_the_claim`, RED failed. `after_delete` cancels the source's drafts: `the_after_merge_delete_cancels_the_source_s_drafts`, RED failed. Rehearsal data API key: F2. Gap tests (passed at once): `a_marker_setup_failure_fails_in_tx_before_the_transaction`, `checks_skip_a_validated_constraint_and_a_set_not_null`, `base_source_round_trips`.
- **AP M3** (step id collisions): F1 planner, F3 API. `step_ids_with_reserved_characters_are_refused`. RED failed.
- **AP M21**: F3 (trait doc, `PGAPPNAME` debug_assert, imports, `Static.lsn` validation, `neon.rs` docs); F1 did `steps.rs`. Tests `an_overlong_app_name_is_caught`, `a_static_lsn_must_be_x_slash_y`. RED: compile error.
- **AP M6–M20**: F1.
- **For F3 (collected)**: `needs_acknowledgement` in worker and API (`merge::tests::a_breaking_step_needs_an_acknowledgement`, API same name; RED failed); `plan_options` uses `plan::lock_timeout_ms` (`plan_options_reads_the_lock_timeout_as_the_planner_does`); `member_of` not the exact set (`a_membership_list_never_revokes_reserved_groups`, passed at once).
- **E29** phase 0 rename: `phase0_renames_through_the_control_plane`, `the_rehearsal_renames_by_sql`. RED failed.
- **E31** wiring: `an_object_following_a_rename_is_checked_against_the_rehearsal`, `objects_following_a_rename_are_produced_until_rehearsed`. RED: `follows_rename` not stored.
- **E34**: `acknowledgements_keep_a_rehearsed_merge_rehearsed`, live `a_keep_plan_save_keeps_the_rehearsal`. RED failed.
- **E27 Abandon**: `abandon_cancels_a_half_applied_merge_and_lists_leftovers`. RED: 404.
- **Left to F4**: F2's M3; Studio slice I2–I5, minors 1–3, 5–13, 15, 16; docs for E16, E27–E35.

## 2. Invariant

`MergeState` = status, plus the phase for `failed` (roles, enum_add, tx, online, none): 16 states.
- **Holds** (no reset/restore/delete of source or target): `rehearsing`, `applying`, `online`, `finishing`, `failed(online)`; a live rehearsal child holds its parent.
- **Applying** (blocks other merges into the target): `applying`, `online`, `finishing`, `failed(online)`.
- **Open** (blocks a new merge of the same pair): all but `done`, `applied_with_drift`, `stale`, `cancelled`, and `failed` outside online. Applying ⇒ holding ⇒ open.
- **Actions:** Edit draft|rehearsed → computing; acks/after-choice only (E34) draft → draft, rehearsed → rehearsed; Rehearse draft → rehearsing; Apply rehearsed → applying; Recompute draft|stale|failed(not online) → computing; Retry failed(roles|enum_add|tx) → applying, failed(online) → online; Cancel computing|draft|rehearsing|rehearsed → cancelled; Abandon failed(online) → cancelled; Finish applied_with_drift → applied_with_drift.
- **Tests:** `merges::tests::the_state_machine_table` (literal table), `every_action_in_every_state_follows_the_table` (16 × 9 through the API), `branch_ops_in_every_state_follow_the_table` (16 × reset/restore/delete × source/target), live `merge_state_sql_matches_the_table`, `the_migration_uses_the_state_predicates`.

## 3. API changes

- New `POST …/merges/{id}/abandon` (`abandonMerge`): 200 with the merge; `result.leftovers: [{kind: not_valid_constraint | not_null_check, schema, table, name}]`.
- `branch_merge_busy` may carry `error.rehearsal_branch_id`.
- Rehearse, apply, retry may return 409 `branch_not_ready`.
- `updateMerge`: a save of only acknowledgements and/or `options.after` (or an empty save) keeps status and rehearsal, recomputes nothing.
- `getMerge` / `updateMerge`: `plan` is `{steps}` only.
- Step ids with `:`, `.` or control characters: 422.
- New `error` prefixes: `merge_target_changed:`, `merge_target_gone:` (failed, phase null, not retryable), `merge_resolution_refused:` (on a draft). Interrupted wording changed.
- New `result.after`: `started: true` (with `timeline`) while a reset/delete runs; `outcome: "started"` for a reset still switching.
- New column `merges.handed_back_at`; plan JSON gains `follows_rename`, `expected_rehearsed`.
- Retry, updateMerge, recompute descriptions updated.

## 4. Handoff to F4 (Studio and docs)

1. Abandon button for a merge failed in `online`; show `result.leftovers`.
2. Retry for roles/enum_add/tx failures (Studio I2), worded so the earlier attempt's roles and enum values stay.
3. Finish button (Studio I3), handling `requested`, `started`, `outcome: started|done`.
4. Error wording for `merge_target_changed`, `merge_target_gone`, `merge_resolution_refused` (may be a first line before plan step errors in `stepErrors`), the new interrupted text, heading chosen by status (Studio I5).
5. Acks/after-choice saves keep the rehearsal: remove the "returns to draft" warnings. A breaking role rename needs an acknowledgement; show its live sessions and keys (E29a).
6. Branch actions show `branch_merge_busy` for failed-online merges and rehearsal holds (`rehearsal_branch_id`).
7. Studio minors 1–3, 5–13; `openMergeFor` per database; `exportMergePlan` client (minor 15); F2's M3.
8. Docs: Studio I4 (retry, finish, E19, stale check, recovery wording, rehearsal outcomes, the 30-minute rehearsal hold, the DDL transaction note, E22, `not_advanced`), E27, E29/E29a, E31, E34, reconcile hand-back (3 min, 30-min re-hand, Finish re-publish), minors 6 and 13.

## 5. Live tests for T15

- `live_db`: `merge_state_sql_matches_the_table`, `claim_refuses_a_branch_a_merge_holds`, `status_write_guards_and_logs`, `hand_backs_wait_for_their_job`, `a_keep_plan_save_keeps_the_rehearsal`; updated `busy_merge_is_found_for_source_and_target`, `status_transition_is_guarded`.
- `merge_live`: `a_failed_online_merge_holds_its_target_and_abandon_lists_leftovers`; `recovery_reads_the_marker` (wording). The fixture's apply/retry log the attempt start and take the guards.
- Migration `20261013000001` adds a column and rebuilds two indexes.

## 6. Questions (for the user)

1. Abandon's leftovers: an INVALID index from an interrupted concurrent build is left too. (a, rec.) add `kind: invalid_index`; (b) keep as ruled.
2. `created_roles` is not scoped to the latest attempt (attempt 2 skips attempt 1's roles, which would otherwise vanish from the result). (a, rec.) keep; (b) scope it.
3. A failed tx whose marker can't be read ends `failed` in `tx` with a note that Retry checks first. (a, rec.) keep; (b) redeliver the job (recovery returns it to `rehearsed`, losing the error).
4. An empty `updateMerge` save no longer recomputes (E34). (a, rec.) keep; (b) empty save = recompute.

**User answers (2026-10-08): all (a).** Q1 extended by the coordinator's ruling D2 (every index the merge left).

## 7. Fix round (`c12a725`, opus) for the review `2f21dae`

Verification: workspace build, `cargo test --workspace` 1999 passed / 0 failed / 375 ignored, clippy clean, `just api-gen` (clients regenerated), Studio typecheck and 605 tests pass. Coordinator checked the diff.

- **I1:** worker `the_after_merge_reset_and_delete_follow_the_hold_table`, `the_delete_job_follows_the_hold_table`; API `a_delete_without_a_claim_is_refused_under_the_lock`; live `every_branch_path_follows_the_hold_table`, `the_after_merge_choice_waits_for_a_merge_holding_the_source`.
- **I2 / D1:** `a_recompute_after_a_committed_phase1_fails_in_online`, `a_recompute_reads_the_marker_only_after_an_apply`; live `a_recompute_after_a_committed_phase1_stays_half_applied`. New error prefix `merge_half_applied:`. A marker that can't be read during a compute redelivers the job.
- **M1** `a_finish_lost_after_it_started_is_published_again`, `a_started_choice_keeps_its_request`; **M2** `a_lost_finish_is_handed_back_once_per_rehand_window`; **M3** `a_failed_publish_isnt_recorded_as_a_hand_back` (`hand_back_merges(publish)` replaces `merges_to_resume`); **M4** `apply_and_rehearse_take_the_version_the_user_saw`; **M5** `the_rehearsal_rename_refuses_what_the_apply_would` (shared `check_rename`, `rename_is_ours`); **M6** `an_online_failure_stops_the_merge_s_statements_first`; **M7** `branch_rewrites_document_the_merge_hold`, `merge_busy_names_a_half_applied_merge`; **M8** `PHASE1_STEP_ID` refused (API and planner); **M9** the widened exhaustive action test (found: Abandon left `failed_phase = online`; now cleared); **M11** `update_role_documents_the_memberships_that_stay`.
- **D2:** leftover kinds `invalid_index` (incl. an interrupted `DROP INDEX CONCURRENTLY`) and `unattached_index`; `not_valid_constraint` only for a constraint that exists. Tests `abandon_lists_the_indexes_of_a_replaced_index`, `abandon_lists_a_unique_index_not_attached`, `abandon_lists_a_not_valid_constraint_only_once_it_exists`.

**Extended hold invariant:** every path that moves a normal branch out of `ready` toward reset, restore or delete checks, under the branch's row lock, that no merge in a holding state (`MergeState::holds_branches`) has it as source or target and no live rehearsal child exists, excluding the finishing merge's own id. Metadata: `branch_busy(…, except)`, `claim_branch_for_switch_except`, `begin_branch_delete_except` (busy check under `FOR UPDATE`, after the database row), `BranchDeleteOutcome::MergeBusy`, `merge_busy_refusal`. Covered: API reset/restore/delete (incl. a not-ready branch's delete), the worker delete job, `finish.rs` after-merge reset (claim) and delete (`except`). No check needed: the worker's reset/restore jobs and reconcile's FinishSwitch act on an already claimed branch. Exhaustive over 16 states × source/target (API, worker, live).

**API changes for F4 (in addition to section 4):** `applyMerge` optional `version`, `rehearseMerge` optional `RehearseRequest {version}`, mismatch 409 `merge_version_conflict` with `error.current` (Studio sends `{}` today; F4 sends `merge.version`); leftover kinds `invalid_index`, `unattached_index`; abandoned merges have `failed_phase` null; `merge_half_applied:` prefix; step id `phase1` is 422; `merge_busy` text changed; reset/restore/delete 409s document `branch_merge_busy` with `error.merge_id` / `error.rehearsal_branch_id`; `result.after` keeps `requested: true` while `started`; `updateRole`/`member_of` descriptions.

**Live tests for T15 (in addition to section 5):** `live_db` `every_branch_path_follows_the_hold_table`, updated `hand_backs_wait_for_their_job`; `merge_live` `the_after_merge_choice_waits_for_a_merge_holding_the_source`, `a_recompute_after_a_committed_phase1_stays_half_applied`, recheck `a_failed_online_merge_holds_its_target_and_abandon_lists_leftovers` (expected 1 leftover).

**T15, before migration `20261013000001` runs on the dev metadata DB** (review Minor 10): both queries must return no rows; otherwise set the extra rows to `cancelled` (keep the newest real one per group) before starting the stack, or the index rebuild stops startup.

```sql
SELECT target_branch_id FROM merges
 WHERE status IN ('applying','online','finishing') OR (status='failed' AND failed_phase='online')
 GROUP BY 1 HAVING count(*) > 1;
SELECT source_branch_id, target_branch_id, database_name FROM merges
 WHERE NOT (status IN ('done','applied_with_drift','stale','cancelled') OR (status='failed' AND failed_phase IS DISTINCT FROM 'online'))
 GROUP BY 1,2,3 HAVING count(*) > 1;
```

**Open question (fix round):** database and project delete (`begin_delete`) still take held branches along; E27's target-gone rule then ends those merges.
