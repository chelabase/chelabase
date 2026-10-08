# Step 6g T14, F4 implementer report

F4 = Studio, docs and OpenAPI wording (sonnet). Branch `schema-merge-f4`. Commits: `216ae09` (generated `exportMergePlan` returns the SQL text), `fef6888` (View as and secret keys), `80a4177` (Studio merge pages), `52de831` (OpenAPI descriptions), `ef8a91b` (docs). Verification: Studio typecheck, tests 672/672, build; management-client tests 20/20; docs typecheck, build, tests 14/14; `just api-gen` + openapi tests pass. No live tests, stack, Playwright or image builds.

**Gap (stopped as instructed): E29a sessions.** No API lists live sessions per role (`pg_stat_activity`). Studio lists the secret keys that use the renamed role (they follow the rename) and says connections open under the old name aren't listed and their connection strings must change. Listing sessions needs a new server route.

## Checklist (RED shown before the code unless noted; tests in `apps/studio/src`)

- **Studio I2** Retry for roles/enum_add/tx: `merge-apply.tsx`, `merge-page.tsx`; `merge-apply.test` "retries a merge that failed in %s from the start…" (3), "gives no retry for a merge whose target changed or is gone". RED.
- **Studio I3** Finish button: `merge-finish.tsx`; `merge-finish.test` "running a skipped after-merge choice (E21)" (4). RED.
- **Studio I4** docs: below.
- **Studio I5** heading/wording by status and code: `lib/merges.ts` `explainMergeError`, `merge-error.tsx`; `merge-page.test` "what the page says about the merge's error" (7), `merges.test` "the worker's error texts". RED.
- **Minor 1** phase 1 ticks scoped to the current attempt: `merges.test` "apply progress", `merge-apply.test` "counts the plan's transaction as done once phase1 is logged". RED.
- **Minor 2** `describeAfter` wording; `merges.test`, `merge-finish.test`. RED.
- **Minor 3** produced drift filtered after apply: `merge-finish.test`. RED.
- **Minor 5** neutral re-baseline wording with the worker's reason: `merge-page.test`. RED.
- **Minor 8** `aria-label` on `<pre>` blocks: `merge-changes.test`, `merge-conflicts.test`, `merge-rehearsal.test`. RED.
- **Minor 9** unsaved edits survive other saves (per-section keys): `merge-page.test`. RED by restoring the old key.
- **Minor 10** one Apply button while the protected confirm is open: `merge-apply.test`. RED.
- **Minor 11** `openMergeFor` per Postgres database; merges list refreshes: `lib/branches.ts`, `database-branches.tsx`; `branches.test`, `database-branches.test`. RED.
- **Minor 12** export timeouts: F1 (E35); OpenAPI export text described here.
- **Minor 13** delete-refusal example: docs.
- **Minor 15** `exportMergePlan`: `packages/management-client/src/fetch.ts` returns text for `…/plan.sql`; `fetch.test` "the_generated_export_client_returns_the_text_too". RED.
- **Minor 16** "In Studio" section in `merging-schema.mdx`, plus `studio.mdx`.
- **Minors 4, 7, 14**: F3. **Minor 6**: docs error text.
- **F3 handoff:**
  - Abandon with leftovers (4 kinds, SQL, confirm): `merge-apply.test` "abandons a merge that failed half applied…", "can back out of the abandon". RED.
  - E27 holds: `isMergeOpen`/`holdsBranches` take `failed_phase`; reset/restore/delete show the hold with a link: `merges.test`, `branches.test`, `branch-actions.test`. RED.
  - `branch_merge_busy` (`rehearsal_branch_id`), `branch_not_ready` texts: `mergeErrorText`. RED.
  - Error prefixes `merge_target_changed`, `merge_target_gone`, `merge_resolution_refused` (before planner lines), `merge_half_applied`, `merge_stale`, interrupted: under I5.
  - E34 warnings replaced: `merge-plan.test` "keeps a rehearsed merge rehearsed…". RED.
  - `merge.version` on apply/rehearse: `merge-apply.test`, `merge-rehearsal.test` "sends the version it saw". RED. A version conflict on apply/rehearse/retry/abandon/finish reloads the merge.
  - E29a/E28: "Breaks clients" badge, own acknowledgement, keys listed; `applyBlockers` text: `merge-plan.test`, `merges.test`. RED (owner-level security text: no RED run).
  - E33: `data-api-keys.tsx` marks "Bypasses RLS"; a blocked key's reason says owner-level. RED.
- **F2 M3** View as: BYPASSRLS role selectable with a warning banner; owner-level disabled: `tables-view.test` "view_as_a_bypassrls_role_warns_like_the_owner (M3)". RED. Console `role_refused` text in `lib/console.ts` fixed too.
- **OpenAPI** (`52de831`): console descriptions said BYPASSRLS roles are refused (only owner-level are): fixed in `services/orchestrator/src/console.rs`; `getMerge` (poll while an after-merge choice is pending), `cancelMerge` (rehearsal hold), export (E35) descriptions.
- **Docs** (`ef8a91b`): `merging-schema.mdx` rewritten in place (E16, E19, E22, E27–E35, E29/E29a, E30, E31, stale check, rehearsal outcomes, 30-minute hold, DDL transaction note, `not_advanced`, recovery, Finish, routes, minors 6 and 13, "In Studio"); `architecture/schema-merge.mdx` (phases, plan flags, stale check (removed the false claim that the apply request checks the target), state machine, local socket `127.0.0.1`, recovery/reconcile timings); `reference/errors.mdx`, `using/branches.mdx`, `using/login-roles.mdx` (`member_of`), `using/studio.mdx`.

## Flows for T15's Playwright pass

1. Branch actions: with a half-applied merge, dev's Reset/Restore/Delete are disabled with the hold reason and an "Open the merge" link, and Merge into main is disabled; `rehearsing` shows "a merge is rehearsing or applying on this branch"; the buttons free up without a reload when the merge ends; another Postgres database's merge doesn't block.
2. Failed in `tx`/`roles`/`enum_add`: "Retry the apply" with the "roles and enum values stay" wording; clicking moves to Applying.
3. Failed in `online`: "Retry the online steps" and "Abandon the merge"; Abandon's confirm gives Cancelled "Abandoned half applied"; Finish lists "Left on main" with SQL per leftover; branches are free afterwards.
4. `applied_with_drift` with `after.skipped`: keep/reset/delete (keep only for an update from the parent); "Waiting to…" → "Resetting…" → the outcome, polling; a second request answers 409.
5. Role rename plan: "Breaks clients" badge, rename text, keys using the role; Apply disabled until the breaking-change checkbox is saved.
6. Rehearsed merge: saving acknowledgements or the after-merge choice keeps it rehearsed and Apply sends its version; saving a conflict choice or step returns it to draft.
7. Error panels: refused resolution on a draft; interrupted apply on rehearsed; stale after a retried apply (no "nothing was applied"); `merge_target_gone`/`merge_target_changed` with Recompute (gone has no retry).
8. Version conflict on apply/rehearse: "This merge changed elsewhere…", then the next click uses the reloaded version.
9. Unsaved "Step SQL" text survives saving conflict choices.
10. Tables → View as: a BYPASSRLS role shows the warning; owner-level roles are disabled.
11. Data API keys: a BYPASSRLS key shows "Bypasses RLS"; an owner-level key shows Blocked "owner-level".

## Implementer's choices

- E29a sessions: keys only (gap above).
- `isMergeOpen` and the failed-online hold also drive the branch-action reasons and links.
- `customFetch` returns text only for URLs ending `/plan.sql` (keying on `text/plain` broke the login client's fallback test); `fetchMergePlanSql` is kept.
- A `rehearsed` merge with drift says "The rehearsal ran, but its result differs from the merged schema (below)…" instead of "passed".
- Docs claims taken from the review without a separate code check: a rehearsal that hits the guard ends `stale`; one with differences ends `rehearsed`.
- No RED run: the owner-level security warning text; the docs (prose, build passes).

## Review and fix round

Sonnet review `0d48f9e` (`…-f4-review.md`): Needs fixes, I1 open-merge database match, I2 rehearsal-failure heading, I3 E29a sessions, 7 minors. User rulings (2026-10-08): D1 expose the Postgres name, D2 a sessions route, D3 specific rehearsal wording, and an apply refused before it ran says "The apply didn't start.".

Opus fix round `35872f3` (every finding; RED for each code item): 
- D1/I1: `postgres_database` on `DatabaseSummary`/`Database`; Studio `openMergeFor` compares it (`me_and_get_database_show_the_postgres_database_name`, `database-branches.test` "finds an open merge by the Postgres database name…").
- D2/I3: `GET /v1/databases/{id}/branches/{branch_id}/roles/{role}/sessions` (`listRoleSessions`, management auth): 200 `RoleSessions {compute_running, count, sessions: [{client_addr, application_name, backend_start}]}`, oldest first; a stopped compute answers `compute_running: false` and is never started; errors 400, 401/403, 404, 409 `backup_branch`/`branch_not_ready`/`credentials_unavailable`, 422 `invalid_role_name`, 500, 503, 504. NATS `SUBJECT_ROLE_SESSIONS`; worker `role_sessions.rs` (name validated, quoted literal, compute state read first); Studio `RenameImpact`/`LiveSessions` on a rename step, with the "clients not connected right now" note. Docs: `login-roles.mdx` "Live sessions", `projects-and-databases.mdx` `postgres_database`.
- D3/I2: kinds `rehearsal_failed`, `rehearsal_unread`, `rehearsal_not_run`; no panel when the Rehearsal section shows the failed step.
- Minors 1–7 and the `apply.rs:65-66` comment; the stale/acknowledgement texts corrected in server, worker, Studio and docs.
- Verification: 2011 unit tests, clippy, api-gen, Studio 684, management-client 20, docs 14.

Sonnet copy fix `2c0c5f8`: on a `rehearsed` merge, `branch_not_ready:`, `merge_conflicts_open:`, `merge_drops_unacknowledged:` get "The apply didn't start." plus the reason (`explainMergeError(error, status)`); a real interruption keeps "interrupted". Studio 692 tests. Coordinator checked both diffs.

**Added for T15:** live `roles_live role_sessions_lists_the_live_connections`; Playwright: (12) a rename step with a client connected as the old name shows "Connected to main as a right now: N sessions." with address · application · since, and "No one is connected…" with none; (13) a database "web-1 prod" with an open dev → main merge disables dev's Merge into main with the "already open" reason and link, and another Postgres database's merge doesn't; (14) a rehearsed merge whose apply was refused shows "The apply didn't start." with the reason.
