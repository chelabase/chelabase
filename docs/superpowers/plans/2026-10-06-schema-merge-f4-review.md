# Step 6g T14, F4 review (Studio, docs, OpenAPI wording)

Reviewed `git diff schema-merge...schema-merge-f4` (`216ae09`, `fef6888`, `80a4177`, `52de831`, `ef8a91b`, plus the report commit `fdf8309`). Read-only; nothing run live.

**Verdict: Needs fixes.** 0 Critical, 3 Important (one regression, one wrong message, one ruling still undone), 7 Minor. The docs match the code on every behavioral claim I traced. The Studio state predicates match the server table exactly.

## Command results

- Studio typecheck: passes. Studio tests: 672/672 (71 files).
- management-client tests: 20/20. Docs build: passes. Docs tests: 14/14.

## Verified against the code (no finding)

- **The two claims taken from the review unchecked are true.** A rehearsal that hits the guard ends `stale` (`services/worker/src/merge/rehearse.rs:218-230`, `merge_stale:` text). One with differences ends `rehearsed` (`rehearse.rs:190-202`, `Passed{drift}`).
- **Timings.** Three-minute claim (`STALE_MERGE_CLAIM` 180 s), 30-minute re-hand (`REHAND_AFTER`), 10-minute lost Finish (`STALE_FINISH_REQUEST`), 30-minute rehearsal (`STALE_REHEARSAL_SECS`, and `remove_stale_rehearsal` spares a `rehearsing` merge's branch), 5 s default and 100 ms to 10 min `lock_timeout` (`DEFAULT_LOCK_TIMEOUT_MS`, `LOCK_TIMEOUT_MS`), 15 min phase 1 `statement_timeout`, 6 h online step, limits of 200 steps / 100,000 bytes / 256 bytes: all match.
- **Error prefixes.** `merge_stale:`, `merge_target_changed:`, `merge_target_gone:`, `merge_half_applied:`, `merge_resolution_refused:` (`; `-joined, before the planner lines, `compute.rs:313`), `merge_base_unavailable: ` and the interrupted text match the worker constants (`apply.rs:47-63`, `compute.rs:46-52`), and `explainMergeError` parses the same strings.
- **State predicates.** `isMergeOpen` and `holdsBranches` in `apps/studio/src/lib/merges.ts` equal `OPEN_SQL` / `HOLDS_SQL` in `crates/chelabase-metadata/src/merges.rs:131-203` for all 16 states (failed(online) open and holding; every other failed ended; rehearsing holds; draft and rehearsed don't). Leftover kinds (four), `Breaking` (`role_rename`), `AfterDoc` fields, the `finishMerge` 409 `conflict` on a second request, the order of apply's refusals (docs list matches `merges.rs:1241-1284`), phase-2 blocking locks (`ACCESS EXCLUSIVE`, `SHARE ROW EXCLUSIVE`, `plan/mod.rs:756`), E16's exact shape (`catalog_sql.sql:310`), the pre-check projection and its unparseable-1a-is-stale rule (`fingerprint.rs`): all match.
- **Version.** Studio sends `version` on apply (`merge-page.tsx`, test asserts the body) and rehearse (`merge-rehearsal.test.tsx:43`), and a `merge_version_conflict` reloads the merge via `fail()`.
- **`customFetch`.** Only `getExportMergePlanUrl` ends in `/plan.sql`; every other generated call keeps `JSON.parse`; errors still go through `readApiError`; the login client has its own fetch. Nothing else breaks. `fetch.test` covers text and JSON.
- **Copy.** No em dashes, no emoji; voice is plain. Every new `<pre>` has an `aria-label`; disabled Apply names its reasons through `aria-describedby`; branch actions list each reason once.
- **Done with a test that asserts behavior:** I2, I3, I5 (except below), minors 1, 2, 3, 5, 8, 9, 10, 11 (except below), 13, 15, 16, E27 holds, E33, F2 M3, E34 wording, version, abandon, finish, rename badge.

## Important

**I1. The open-merge database filter compares the wrong names, so an open merge is never found for most databases (regression from minor 11).**
- Where: `apps/studio/src/lib/branches.ts` `openMergeFor` (`m.database === database`), fed by `databaseName` = `database.name` (`database-branches.tsx:75` → `branch-tree.tsx:68` → `branch-actions.tsx:89`).
- Problem: `m.database` is the Postgres database (`MergeSummary.database`, "The Postgres database being merged"). `database.name` is the Chelabase display name. The worker names the main Postgres database `sanitize_db_name(name)` (`services/worker/src/provision.rs:130`), e.g. "web-1 prod" becomes `web_1_prod`. Whenever the two differ (space, hyphen, uppercase and similar) the filter matches nothing.
- Failure scenario: the database "My App" has an open merge dev → main. The Merge into main button is enabled with no "is already open" reason and no link. The click answers 409 `merge_open_exists`. Before F4 the pair-only match worked. The tests use `web1` (a name that sanitizes to itself), so they pass.
- Fix: see decision D1.

**I2. A draft left by a failed rehearsal says "The last compute couldn't finish the plan."**
- Where: `apps/studio/src/components/merges/merge-page.tsx` `ErrorPanel` (the draft/computing branch), `lib/merges.ts` `explainMergeError` (no kind for these texts).
- Problem: a failed rehearsal returns the merge to `draft` with `error` = "the rehearsal failed at step <id>: …" (`rehearse.rs:236`); an unreadable result gives "the rehearsal: …" (`:207`); a branch that couldn't be made gives "creating the rehearsal: …" / "branch_not_ready: …" (`:167`, `:290`, `merge/mod.rs:190-197`). All fall to `kind: "other"` and get the compute heading, plus Postgres text printed a second time next to the Rehearsal section. This is the most common failure on the page, and Studio I5 asked for the heading by status and error.
- Fix: add kinds for these prefixes (`rehearsal_failed`, `rehearsal_unread`, `branch_not_ready`) and word the heading per kind (see D3). Test each in `merge-page.test.tsx`. Add the rows to the "Error texts" table in `merging-schema.mdx` (see Minor 5).

**I3. E29a is only half done: no live-sessions information (reported by the implementer, still a ruled item).**
- Where: `merge-plan.tsx` `RenameImpact` ("Connections open as {role} aren't listed here").
- E29a (user ruling) says Studio shows whether the target has live sessions or secret keys for the role. Keys are shown; sessions can't be, because no route reads `pg_stat_activity` per role. The docs claim only keys (correct), but the ruling isn't met.
- Fix: see decision D2.

## Minor

1. **A stale merge shows its sentence twice.** `merge-page.tsx`: `ErrorPanel` renders for every non-failed status with an `error`, so a `stale` merge gets "The merge reported a problem." plus the worker's text, and `EndNotice` repeats the same sentence. Skip `ErrorPanel` when `EndNotice` already shows the error (stale; and `failed`/`cancelled` if they ever carry one). The new stale test passes because `findByText` matches only `EndNotice`'s capitalised copy.
2. **"The plan's transaction rolled back, so none of its changes are in" is stated as fact** (`merge-apply.tsx` `WHAT_RAN.tx`). F3 ruled Q3: a `failed(tx)` whose marker couldn't be read stays `tx` with a note that Retry checks first, so the transaction may have committed. The next paragraph of the same panel says a retry checks. Say "should have rolled back" or leave the claim to the worker's `error` text.
3. **`studio.mdx:30` says "The four buttons on a row"**, but Branches, Merges, Data API, Tables and SQL are five (the list below it has five items).
4. **Server texts and one docs row still say the old things.** `services/orchestrator/src/merges.rs:1211` (applyMerge description: "data-loss or security steps", and "if the target's schema changed since the rehearsal"), `:1244` (409 message "the schemas changed since the rehearsal"), `:1266` (message "acknowledge the data loss or security change"), and `reference/errors.mdx` `merge_stale` ("target or source schema changed after the rehearsal"). The apply request doesn't read the target; the worker compares against the plan's compute-time target (docs `architecture/schema-merge.mdx` says so), and breaking steps (renames) and owner-level also need acknowledgement. The generated management reference pages show the OpenAPI text. Fix the wording, then `just api-gen`.
5. **`merging-schema.mdx` "Error texts"** (`:343`) says "Anything else is Postgres's own text (`step <id>: …`)". The worker also writes "the rehearsal failed at step …", "the rehearsal: …", "creating the rehearsal: …" and `branch_not_ready: …`. Add them to the table. Also `:247` says a stale pre-check leaves "nothing applied", and `:251` then qualifies it for a retry; make `:247` "the plan's transaction is not applied".
6. **Saving acknowledgements remounts the whole Plan section** (`merge-page.tsx` `planKey` includes `merge.acknowledgements`), so unsaved text in "Step SQL" in that same section is lost. The cross-section case F4 set out to fix works (tested). Either key on server data the section doesn't edit or keep the step draft in page state. Note in the docs line "Unsaved edits in one section stay when another section is saved": correct as written.
7. **The Blocked reason lists a shorter set than the rule** (`data-api-keys.tsx`: "it can create roles or databases, or belongs to neon_superuser"). The rule (and `using/data-api/keys.mdx`) also covers superuser and REPLICATION. Use the same list as the console text in `lib/console.ts`.

Aside (not in F4's diff): the comment at `services/worker/src/merge/apply.rs:65-66` ("before it reads the marker anyway") contradicts `stop_merge_backends`, which fails closed. The docs describe the code correctly.

## Decisions needed

**D1. How should Studio match an open merge to a database (I1)?**
- (a) **Recommended:** expose the main Postgres database name on the API (a field on `DatabaseSummary`, or on `Branch`), and compare it exactly. It costs one API field and `just api-gen`; no duplicated sanitizing in Studio.
- (b) Go back to matching the pair only (no database filter). No API change. A merge started over the API on another Postgres database then disables the button with a reason that names the wrong merge, which only affects API users.
- (c) Copy `postgres_database_name` into Studio. Rejected: two copies of a naming rule drift.

**D2. E29a live sessions.**
- (a) **Recommended:** add a small read route (worker reads `pg_stat_activity` for the role on the target, as the admin role, and the orchestrator serves count and application names), and show it in `RenameImpact`. Meets the ruling as written; one more route, one more test slice.
- (b) Keep keys only and amend E29a. Cost: a user can't see who is connected before acknowledging the rename; the text already tells them to change connection strings.

**D3. Wording for a draft after a rehearsal problem (I2).**
- (a) **Recommended:** no error panel when the Rehearsal section already shows the failure (a "failed at step" text with the step and SQL); for "creating the rehearsal" / `branch_not_ready` / unread, the heading "The rehearsal couldn't run." with the text below.
- (b) One heading for all: "The last rehearsal didn't pass." Simpler, repeats the Rehearsal section.
