# Members hub, step 1: hub, roles and privileges, personal logins (implementation plan)

**Goal:** `/app/members` becomes the one place for access. Step 1 ships the following:
- the roster grouped by level (cards/list), with `access` on `GET /v1/members`;
- a member page (without teams);
- the Roles tab, a role page and the privilege editor, backed by new schema-change privilege ops, a privileges read, run-as-owner (D6), checked results, an ACL-staleness check and the all-rows toggle;
- personal Postgres logins per (member, Chelabase database), converged by the member-role reconciler on every branch, with immediate revocation (metadata verifier cleared in the access-reducing transaction, proxy cache cleared), the Zitadel webhook path, a 10-minute Zitadel sweep and `VALID UNTIL` 90 days;
- Suspend / Reinstate and "Revoke Postgres access now";
- multi-email invites;
- the project Access tab and the branch Roles panel rebuilt on the shared components;
- the new access events and `access_events.branch_id`.

Step 1 also introduces the effective-access types step 2 builds on (`GrantSources`, `Effective` with per-role levels), with `teams` always empty (P1).

**Spec:** `docs/superpowers/specs/2026-10-23-members-hub-design.md` (`792044ce`, approved; H1–H13, D1–D6, R1–R10, and the adopted MH rulings). **Red-team:** `docs/superpowers/specs/2026-10-23-members-hub-red-team.md` (MH-1–38, per-area checklist). Where a spec section and its "After the red-team" text or an MH ruling disagree, the later text wins. Implementers read the spec parts their task names.

**Architecture:**
- `crates/chelabase-types`: `access.rs` (T2), `table_api.rs` (T4), `api_error.rs` (T2).
- `crates/chelabase-metadata`: `members.rs` (T2, T3), new `personal_logins.rs` (T3), `neon.rs` (T3), one migration (T3).
- `crates/chelabase-schema`: `ddl/` (T4), `catalog_sql.sql` and `catalog_parts/head.sql` (T4), new `privileges.rs` and `privileges_sql.sql` (T4).
- `crates/chelabase-zitadel`: `client.rs`, `lock_user`/`unlock_user` (T8).
- `services/orchestrator`:
  - `access.rs` (T2, T3, T8);
  - `members.rs` (T2, T9, T8);
  - `table_api/` (T5);
  - new `privileges.rs` (T5);
  - new `personal_logins.rs` (T8);
  - `roles.rs` (T9, T8);
  - `data_keys.rs` (T8);
  - `internal/app.rs` (T8);
  - `internal/ddl.rs` (T8);
  - `lib.rs` (T2, T3, T8);
  - `main.rs` (T8).
- `services/worker`: `member_roles.rs` and `member_roles/` (T2, T6), `resync.rs`, `roles.rs`, `data_api.rs`, new `personal_logins.rs` (T6), new `identity_sweep.rs`, `main.rs` and `config.rs` (T7).
- `crates/chelabase-routes`: one endpoint block per API task (T5, T8, T9).
- `apps/studio`: `components/members/`, new `components/members/hub/`, `components/project-access/`, `components/branch-roles.tsx`, and the routes (S1–S4).
- `scripts/personal-logins-e2e.sh` and its `justfile` recipe (T8).

**Tech:** Rust (workspace toolchain), TypeScript/React (Studio, TanStack Router, Base UI, Vitest), SQL (PG17: metadata Postgres 17.11, computes v17 fork). **No new dependency** in any crate or package (`pg_query`, `proptest`, `sha2`, `redis` and the Zitadel mock are already in the workspace). **No fork change:** no image build; the live pass runs on the pinned images (round 4's `0741b56deb67-dev`).

**Task pipeline (CLAUDE.md "Task pipeline"; replaces the template's coordinator-written RED tests):** the coordinator writes each task's brief from this plan: behaviour, files, pinned interfaces, the edge-case checklist and the RED list below. Every brief starts with "Read `<repo root>/docs/agent-preamble.md` and follow it" and says no agent may spawn agents. Then, in sequence:
1. A **test agent** writes the RED tests listed under the task (plus tests for its checklist where they're cheap to write first) and commits them as `tN: RED tests`. It compiles every target, live ones with `--no-run`. It runs every runnable one against the base and shows each failing on its first assertion for the intended reason; a compile error counts only when the missing item is the code under test. It checks every assumption about harnesses, helpers and system behaviour against the code.
2. A **coder agent** makes them GREEN without editing, weakening, skipping or deleting them. It may fix lint, format and typing in RED files without changing assertions (user, 2026-10-21). It adds edge tests for the checklist and lists them. A test it believes wrong is a deviation sent back to the test agent and listed in the Status as a process failure.
3. A **refactor/audit agent** reviews and tidies without changing behaviour. It confirms `git diff <red-commit>..HEAD` on the RED files is empty, apart from lint-only edits (any assertion change is Critical). It reports defects and hardening separately. The audit is the task's one review.

A leaf task has no audit: the coordinator checks the RED diff itself, and the end-of-step review covers it. Each stage's model is in the task table.

## Status

- **RESUME HERE (2026-10-23): plan approved (P12 (a), P16 (b), P24 (a)); seam pass done (S1–S20 below, applied to the task sections and "Shared interfaces"; S21 decided by the user: refuse).** The plan's stack `members-hub` is up (Studio `http://studio.members-hub.chelabase.localhost:8100`). `main` was made green first (`d1306f44`: tokio 1.54.1, RUSTSEC-2026-0384; three hook-deadline tests moved off the paused clock, which raced loopback connects on WSL2) and merged into this branch (`3c1af85d`). Next: wave W1 (T1, T2, T4).
- **Done: T1** (opus, no review): probe `services/worker/tests/d6_feasibility_live.rs` + report, `d7508299`, live 1/1 on the stack (PG 17.11; four runs: three test-side corrections by the agent); merged `89bf267e`. D6 feasible; rulings P26–P30 (P30 the user's). Deviation: item 3 used `reconcile_member_roles` (as `member_roles_live` does) to install the guard.
- **Done: D** (opus, `/impeccable` Operate): 16 comps `ac6aa91c`, approved by the user (P32); merged into `members-hub`.
- **Done: T2** (opus/opus/opus): RED `75cf288e`, implementation `1900e415` (1872 tests, `members_live` changed tests green), tidy `aecaf5e5`; review: approve, 0 Critical; merged `cf922541`. Coder extensions accepted: R21 chains only through roles held at the route's minimum; a key's role must be held at Developer.
- **Deferred findings (final fix round), from T2's review:**
  - **T2-1 (Important):** `console.rs:463-470` write check uses the member's top level, not the role's own level. Fix: `level_of(role) >= Developer` for a named role. Not reachable in step 1.
  - **T2-3 (Minor):** `decide` lets `SignedIn` routes through before the suspension check (`realtime.ticket`, routes `lib.rs:1902`). P14 allows only `GET /v1/me`. **Folded into T8's brief.**
  - **T2-4 (Important, P1):** `/me` (`lib.rs:2460`), the project list (`projects.rs:199`) and realtime projects (`internal/realtime.rs:409`) read `member_projects` directly. Needs a metadata read returning `GrantSources` per project, with a team-only-member test per route.
  - **T2-5 (Minor):** `Effective.platform_role`/`roles` are public and mutable. Noted, since they're pinned public.
- **Open for step 2 (ask the user when step 2 is planned):** **T2-2.** The SQL editor's hidden member login (`member_roles.rs:1275`) is granted every effective role, Viewer-level ones included. A Developer script can `RESET ROLE` to it and `SET ROLE` into a team-given Viewer role, then write (MH-1). The options are a separate read-only login, or granting only `roles_at_least(Developer)` and serving Viewer reads another way. Not reachable in step 1.
- **T4 review** (opus): approve once T4-1 is fixed; RED diff clean (only the 4 accepted lines); tidy `fea856fb`.
  - **Fixed now** (T5 depends on them; test → coder pipeline, coordinator verifies the diff):
    - **T4-1 (Important):** the privileges read can't see PUBLIC's entries (`privileges.rs:183-191`, `target` from `pg_roles` only), so a PUBLIC revoke that didn't land passes verify.
    - **T4-8:** the exposure check takes its starting roles, so P31's flag is exactly `anon`/`authenticated` plus what they inherit.
  - **Deferred (final round):**
    - **T4-2:** a table-wide `REVOKE GRANT OPTION FOR` doesn't check column entries.
    - **T4-3:** `SetAllRows` on then off in one request leaves the policy on; verify doesn't cover `SetAllRows`.
    - **T4-4:** `acl_hash` treats a NULL ACL like `{}`. Hash `coalesce(acl, acldefault(kind, owner))`, amending S3.
    - **T4-5:** an explicit refusal of privilege ops on hidden schemas and their functions (fails closed today).
    - **T4-6:** a malformed `after` in object mode is a SQL error (500). Validate it in T5 or guard it in SQL.
    - **T4-7:** policy roles in the catalog aren't filtered by the marked-role exclusion; add a unit plan/diff test for a role marked on one side only.
- **Done: T4** (opus/opus/opus): RED `80f37007`, implementation `edbbaca5` + `d9c07927`, tidy `fea856fb`. Fix round (T4-1, T4-8): RED `f40ed8f2`, fix `b222ee8a`, diff verified by the coordinator. The 2 live PUBLIC tests and `cargo nextest -p chelabase-schema` (300) are green. Merged `f394571f`.
  - Pinned for T5: `exposure_chain(edges, role, starting) -> Option<Vec<String>>`. The privileges read takes `role=public` (exact, lower case) for PUBLIC.
  - The coder's extra live test `public_is_reserved_and_read_by_its_exact_name_only` (passed) was left out, since it would edit a RED file. It goes to the final round's test agent.
- **T3 GREEN** (test opus, coder opus):
  - RED `9d2038f3` (33 tests). Its deliberate rewrite of `a_personal_origin_round_trips_and_never_reaches_the_spec`, which asserted the resync's personal→sql flip that S1 forbids, is accepted.
  - Merge of T4 `c93c72fe`; implementation `5de738ec`. Migration `20261023000001_members_hub.sql` written once, applied only on scratch databases so far.
  - `members_live` 55/55; `live_db` 6/6; the coder's `personal_logins_live` 9/9; crates 1836/1836; api-gen done.
  - Accepted deviations: `revoke_unqualified_logins(actor)`; `upsert_personal_role_row` stores the record's own verifier (no stale write-back); the `EventRow` wrapper.
- **Process notes:** test and coder agents used a heredoc or compound command four times so far (T2 test, T4 test, T3 test, T3 coder), against the preamble's one-simple-command rule. Contents are in the commits; no harm found.
- **Done: T3**: review approves it (0 Critical; revocation present in every writer; S1 guards complete; RED files clean); tidy `92d6e07c`; merged `1827ae8c`.
  - **Fixing now (T6 depends):** **T3-I1**, a race between the worker's `upsert_personal_role_row` and a reset or revoke (stale verifier on a row). Fix: the proxy answers the record's verifier for personal rows, the upsert reads the record `FOR SHARE`, and `revoke_where` locks live records `FOR UPDATE` first. Fix-round pipeline on `members-hub-t3fix`.
  - **Deferred (final round):**
    - **T3-M1:** `rename_role_copy` on a personal row answers `Renamed{row:false}`; it should refuse (unreachable today).
    - **T3-M2:** the upsert doesn't check that the endpoint belongs to the record's database.
    - **T3-M3:** soft-deleting a database doesn't revoke its logins.
  - **To T8's brief:** **T3-M4**, call `member_login_endpoints` before a create that replaces an expired record, so the proxy cache is cleared for its old rows.
  - **To T6's brief:** the verifier argument of `upsert_personal_role_row` is ignored; the record is the single source.
- **Done: T3-I1** fix round: RED `d57ca5f5` (4 deterministic interleaving live tests), fix `4c9081b5`, diff verified by the coordinator. Lock order: member, then records `FOR UPDATE`, then rows; the worker's write takes the record `FOR SHARE`, then the row. Merged `1611502b`.
- **R-T9a (coordinator):** a role route whose worker reply is `not_applied` also writes its event, with `after.applied: false` (`Done`: `true`). Refusals and transport failures write none. Cost if wrong: the log shows a change that is still being retried.
- **Done: T9** (sonnet/sonnet/opus): RED `d13f77ef`, implementation `effc795c` (R-T9a; `access` also on update/invite/approve responses), RED test fixes by the test agent `71f19ff6`, tidy `bb61f586`.
  - **Process failure:** two RED test bugs. One expected 403 where an ungranted project is correctly 404; one was order-dependent on random ids, which the coder had masked in the fakes (reverted). Stress 20/20 after the fix.
  - Review: approve, 0 Critical. Merged `af02492e`.
- **Deferred (final round), from T9's review:**
  - **T9-I1 (Important):** invite, approve and added-user read metadata after their side effects (`members.rs:1870`, `:2302`, `:1742`), so a failed read answers 500 after the commit and loses a link invite's URL. Read the visible set first, build `access` from the written grants, and fall back rather than 500.
  - **T9-m1:** a worker `Timeout` writes no role event.
  - **T9-m2:** `login: null` is kept on update events.
  - **T9-m3:** the delete event carries `{role, applied}` only.
  - **T9-H1 (pre-existing, for the end-of-step review):** the project log (`Project(Admin)`) returns raw `before`/`after` that names other projects' grants and `pg_roles` (`members.rs:2023`). Filter them to the route's project.
- **T6 GREEN** (test opus, coder opus): RED `6275dae9` (28 worker tests, including the 48-case convergence model), implementation `cac7f060`; 971/972, plus 10 edge tests the coder added. The live suites are compiled and deferred to the end-of-step pass (P22).
  - **Process failure:** the RED test `the_resync_never_recreates_or_records_a_personal_login` rejected any SQL naming `managed_role`, the resync's own catalog read included, which another pinned test requires. Back with the test agent.
- **T5 RED** `c7f790ff` (test opus): 25 tests, 24 failing as intended plus the P30 guard.
  - **P27a (coordinator, proven by the test agent on PG 17.11; replaces P27's wording):** `owner_settable` = `o = current_user OR pg_has_role(current_user, o, 'SET') OR EXISTS (an admin_option row on o whose member current_user has USAGE of)`. ADMIN reached only through a non-INHERIT membership isn't settable: the transient GRANT fails with "no possible grantors".
  - **P31 interface (coordinator):** the dry run answers `data_api_exposure: [{op_index, grantee, chain}]` in `SchemaChangeResponse`.
  - **`privileges_changed`:** `after.ops[].object` takes the `PrivilegeTarget` serde form, and `op` the op's snake-case name.
- **T6:** the test agent's needle fix is `78b6d955`; the audit's tidy is `bcf1f5e4`.
  - **Review verdict:** approve after **T6-C1 (Critical)**. `personal_cleanup_script` reassigned a login's objects to the owner (`member_roles.rs:521-528`), so a planted `SECURITY DEFINER` function or view would run as the owner.
  - **User ruling (spec MH-18 amended, `036a9a9c`):** objects go to a quarantine role through the canonical procedure: `REASSIGN OWNED … TO chelabase_orphaned`, then `DROP OWNED`. The quarantine role is NOLOGIN with no members and no privileges, and EXECUTE is revoked from PUBLIC on functions moved to it. It's being fixed now, with M5 (a test pinning P16 (b)).
  - **Deferred, minor:**
    - **T6-M1:** a qualifying record with no effective source gets a login with no memberships. It should be skipped.
    - **T6-M2:** a marked role under a reserved or owner name makes every resync request a reconcile.
    - **T6-M3:** a wanted login whose name has a legacy `sql` row upserts nothing, forever.
    - **T6-M4:** `Worker::create_personal_login` is still `pub` (tests only).
    - **T6-M6:** a marked login's per-role settings aren't read.
- **T5:** coder `7617f659`, with 16 of 19 live tests green, 1260 crate tests, and api-gen run.
  - **Process failure:** three RED test bugs, now back with the test agent. D1 checks a PG15+ default privilege; D2 has the wrong ACL string order; D3 calls `fetch_one` on zero rows.
  - **Coder's deviations, accepted:**
    - `owner_settable_predicate` is shared (P27a), with MEMBER before PG16.
    - `pg_database_owner`-owned objects count as the owner's.
    - An unreachable owner's object is refused unless every op on it is a plain revoke (P30).
    - `data_api_exposure` is always present.
    - The role-listed 404 is a separate check.
- **Done: T5** (opus/opus/opus): RED `c7f790ff`, coder `7617f659`, RED-test fixes `1616b187` (all 19 live green), tidy `b5b822f8`. Review: approve, 0 Critical. Merged `56ff196d`.
- **Deferred (final round), from T5's review:**
  - **T5-I1 (Important):** function ops render `oid::regprocedure` under the owner's search path and then run under `SET LOCAL ROLE X` (`privilege_ops.rs:328`, `:628`). Same-named functions resolve wrongly. It fails closed (409); fix by producing the text under `search_path = ''`.
  - **T5-I2 (Important, fails open):** verify is skipped when a statement in a mixed request was skipped (`changes.rs:873`). Verify whenever `every_ran` holds.
  - **T5-M1:** the warning path's 409 carries `mismatches: []`. Run the re-read before failing so the object, privilege and grantor are named. **S3 must handle an empty list.**
  - **T5-M2:** the hash check and `owners` run before `locks()`.
  - **T5-M3:** `leave` compares all of X's rows, so it can fail spuriously. Compare only the owner's rows.
  - **T5-M4:** `owner_run` matches only the literal `owner`. Use `Requested::from_name`.
  - **T5-M5:** the page-cap error lacks `on_dry`, and aggregate/window functions answer 404.
- **T10 (new, user 2026-10-23, after T6's C1): role delete with a successor.** Today `services/worker/src/roles.rs:1264` reassigns a deleted role's objects to the owner, the same escalation as C1 for group roles.
  - **User's ruling:** the Admin picks the successor: another role the owner can SET to (not owner-level, not reserved, not a personal login), or `chelabase_orphaned`. Functions reassigned anywhere lose PUBLIC EXECUTE, and SECURITY DEFINER ones are listed in the answer for review.
  - **Pinned API:** `DELETE …/roles/{role}?successor=<role>|chelabase_orphaned`. It defaults to `chelabase_orphaned` when omitted; an invalid successor is 422 `invalid_successor`. The answer lists `reassigned: {count, definers: [identity]}`.
  - **Models:** test opus, coder opus, audit opus. Wave: with T8, before S3. S3's delete dialog gains the successor picker.
- **Done: T6** (opus/opus/opus): RED `6275dae9`, coder `cac7f060`, RED-test fix `78b6d955`, tidy `bcf1f5e4`.
  - **C1 fix round:** RED `bb29fe22` (5 failing, 4 guards), fix `12d0e87e` (948/948), diff verified by the coordinator. Probed on PG16 and PG17 containers.
  - The coder added: the quarantine role is made powerless again if it was tampered with.
  - Accepted open point: EXECUTE grants the login made to named roles survive, with `chelabase_orphaned` as grantor. Calls through them run with no rights.
  - Merged `4620ef9d`.
- **P33 (user, 2026-10-23): a default role in the personal-login connection string.** S2's reveal dialog (and the member page's login rows) offer a "Default role" picker over the login's groups (roles held at Developer or above). It yields a connection string with `?options=-c%20role%3D<group>` (URL-encoded), so seeds and migrations create objects owned by the group without a manual `SET ROLE`.
  - **Checked by reading the fork at `0741b56de`:** the proxy forwards `options` and strips only `endpoint=`/`project=`/`neon_*` (`proxy/src/compute/mod.rs:229-233`, `filtered_options` `:381-394`). Postgres accepts the `role` GUC at startup when the login holds SET on the group, which personal logins do (R10). `session_user` stays the login.
  - **End-of-step live check** (added to the personal-logins e2e): connect with `options=-c role=<group>`; `current_user` is the group, `session_user` is the login, a created table is owned by the group, and a group the login doesn't hold is refused at connect.
- **S1 GREEN** (sonnet/sonnet): RED `635c5a34` (43 tests; the test agent's pinned names are binding), implementation `0058a7d5`. Typecheck and build are clean; Studio passes 2072/2073.
  - Existing tests got fixture fields only, plus the "Email" → "Emails" label.
  - **Flaky test seen (to fix in this step, final round):** `table-editor/fz-edges.test.tsx > two_quick_outside_presses_save_once` failed under load (expected 1 to be 2) and passes alone.
  - **Design deviation for the Impeccable finish review:** the view toggle shows an icon plus a word, where the comps show icon only.
- **Done: S1**. Review (sonnet): approve, 0 Critical.
  - **Fix round, S1-I1:** `escapeName` didn't escape `\`, so `café` looked like `café` (MH-38 d). RED `2a1244ae` (exhaustive injectivity up to length 6), fix `ab8fc340`, diff verified by the coordinator.
  - Merged `d2c5d2bf`.
  - **Deferred, minor:**
    - **S1-m2:** checklist rows re-sort on every click.
    - **S1-m3:** removing a chip drops focus to the body.
    - **S1-m4:** closing the results with Escape skips `onDone`.
    - **S1-m5:** `LocalPeopleTab`'s `<a href>` and a redundant `isUuid`.
- **T8 RED** `51b5da8b` (test opus): about 30 tests.
  - Choices accepted: a key bound to a personal login is refused 403 `role:<name>`; endpoint names `orchestrator.put_personal_login` and the like; the "not yours" 404 uses a message other than "not found".
  - **To the final round:** add P33's default-role check to `scripts/personal-logins-e2e.sh` (written before P33), through the test agent.
- **T10 RED** `ae21a578` (test opus): 31 failing as intended; 2 new and 4 changed live tests deferred.
  - **Pinned:** `RoleDeleteRequest{branch_id, name, successor}`, `Reassigned{count, definers}`, `RoleOutcome::Deleted(Reassigned)`, HTTP 200 `{successor, reassigned}`, `role_deleted.after.successor`, 422 `invalid_successor`.
  - **Probed on PG 17.11:** `REASSIGN OWNED` needs INHERIT on both roles for a non-superuser, so the delete runs as `cloud_admin` with no run-as.
  - **User ruling (2026-10-23):** a Postgres database the deleted role owns goes to the branch owner; the objects inside go to the chosen successor.
- **Done: T8** (opus/opus/opus): RED `51b5da8b`, coder `e410e913` (1065 tests, 11 edge tests the coder added, a new 409 `credentials_unavailable` before any write), tidy `5e3210bc`. Review: approve, 0 Critical. Merged `f3af5f25`.
- **Deferred (final round), from T8's review:**
  - **T8-1:** `realtime.ticket` is served by the realtime service, which never calls `decide`, so a suspended member still gets a ticket (joining is refused). Add a status check at ticket issue.
  - **T8-2:** a failed webhook tenant lookup fails open (`internal/app.rs:~587`): it answers 200 and skips revocation. Answer 500 for Gone/Deactivated (P17).
  - **T8-3:** `suspend_member` decides its side effects from a stale `m.status`. Run `access_changed_for` unconditionally.
  - **T8-4:** `PersonalLoginSecret` derives `Debug` with the password in it. Redact it.
  - **T8-5:** `converge` doesn't re-enqueue main on a non-`Done` reply.
  - **T8-6:** use the count `revoke_where` returns for revoke-now's `revoked`.
  - **T8-8:** `delete_personal_login` reads the endpoints before taking the record lock.
  - **T8-7 (coordinator):** guard exemptions may name a personal login. **Kept as designed**: spec Part 6 says "guarded unless exempted, Admins' included".
- **T10:** coder `f7cf9389` (2000/2001 tests, 13 edge tests the coder added, including the database-owner ruling); RED-test fixes `c9620cab` (**process failure:** two RED bugs, a quoted role name and a database owner contradicting the ruling); tidy `f21a9cb7`. Review: pass, 0 Critical.
  - **Fixing now (S3 depends; user ruling 2026-10-23, "preview, then confirm"):** `GET …/roles/{role}/delete-preview?successor=S` lists exposures, meaning moved SECURITY DEFINER routines and non-invoker views granted to anyone but S. `DELETE …?successor=S&confirm=<token>` rechecks inside the transaction and answers 409 `exposure_unconfirmed` on a missing or stale token. The quarantine role never needs a confirm, and confirmed grants are kept.
  - **Deferred (final round):**
    - **T10-1:** a membership granted between the guard and REASSIGN slips through. Add `LOCK pg_auth_members IN SHARE MODE`.
    - **T10-2:** a guard raise answers 500 instead of 422, after earlier databases have already moved.
    - **T10-3:** no `lock_timeout`/`statement_timeout` in `role_delete_script`.
    - **T10-4:** an older worker ignores `successor`, falls back to the owner and replies ok without `reassigned`. Treat that as an error.
    - **T10-5:** `successor_check_sql` lacks `standard_conforming_strings = on`.
    - **T10-6:** `definers` carry no database (changing that changes the API; note it for S3).
- **T10 preview/confirm fix round:** RED `9d51f07f` (about 40 tests plus 2 deferred live tests; test agent opus). The coder (opus) is running.
- **Done: S2** (sonnet/sonnet, leaf, no review of its own): RED `52a28296` (66), coder `ca43c269` (plus 10 edge tests the coder added, and P33's per-row "Copy connection string").
  - **Round 2** added database narrowing and invite actions, which the coder had left out against spec Part 3: RED `ddc48ab0`, coder `0ec51097`, RED-test fix `33598e3a`. 96 member-page tests; Studio 2179/2180 before the last test fix.
  - **Process failures:** two RED-test bugs, a spread overwriting `routes` and a DELETE body where the API takes a query param.
  - Shared edits: `Modal` gains a medium size, `ConfirmDialog`'s description is a `<div>`, and `people-tab.tsx` exports `copyFor`.
  - **Not done (no data):** the "Suspended on {date} by {actor}" line, because `Member` carries neither.
  - Merged `4c803947`.
- **Done: T10 preview/confirm**: coder `0eff730a` (2040/2040, probed on PG16 and PG17).
  - **Integration:** T10 branched before T8, so `members-hub` was merged into it by a coder (`bfecfd27`; source conflicts in `roles.rs`/`openapi.rs` resolved, generated files regenerated, not hand-merged, P21; 2111/2111). The integration also hides personal logins from the new preview route (MH-11).
  - **Coordinator process failure:** the first merge attempt was sent with its cleanup in parallel. The branch was deleted although the merge conflicted; it was restored from `0eff730a`.
  - Merged `be445f97`.
- **Done: S3** (sonnet/opus, leaf): RED `957e8ffa` (110), coder `fd3798cf` (plus 25 tests; caught and fixed the create dialog accepting `anon`/`authenticated`), RED-test fix `e6eaaf51` (**process failure:** a `/^Members/` regex matched "Memberships"). 135 role tests; Studio 2312 green before the fix. Merged `6c0f29fb`.
  - **Coder's UI rulings, for the end-of-step review:** Admins aren't counted as holders; owner-level roles aren't offered in "Member of" or as a successor; the matrix is locked during review; hidden memberships are kept.
- **T7:** RED `b6db3086`. A RED test hung for 76 minutes on the test agent's own mutex deadlock; the coordinator stopped it (**process failure**). The coder (sonnet) is running, every test under a bound.
- **Done: T7** (sonnet/sonnet, leaf): RED `b6db3086`, coder `84fd32a6` (956 worker tests). Merged `842b5436`.
  - **Process failure:** the test fake journaled each revoke twice; the coder fixed the fake, and the coordinator verified the diff.
- **Done: S4** (sonnet/sonnet, leaf): RED `1556cb8a` (19 new tests plus adapted existing ones), coder `acc33874` (Studio 2322/2322). Merged `9043dda5`.
  - **Coder fixes:** a latent `GrantCard` bug ("on a deleted database"), with a `databases` prop now passed on the member page; `owner-password-notice` finds its project from `/me`; keys in the role-in-use error are listed.
  - **S4-1 (RED-test bug, process failure, final round):** two adapted Access-tab tests pin refusal text without the database, so the Access tab lost "…on <database>" and its line flag. Fix: the test agent updates `grant_refusal_marks_the_row` and `a_refusal_on_a_database_that_isn't_narrowed_has_no_narrowing_note`; the coder passes `databases={project.databases}` in `grant-editor.tsx`.
- **P34 (coordinator, 2026-10-23): Impeccable's finish review runs in the live pass.** Its first run answered `recapture`: it needs captures of the built pages, and the stack still runs the pre-step apps (P22: no mid-step refresh).
  - After the final fix round and the one app recreate, a capture agent saves Playwright screenshots of every comp screen (01–16) plus the rebuilt Access tab and branch Roles panel, at 1440 and 390, into `.impeccable/review/` (`desktop.png`, `mobile.png`, `NN-<comp>-1440.png`/`-390.png`). Dialogs and error states are captured open over their page.
  - The finish review runs on those captures. Its design fixes are a Studio-only round before the user's pause; no image build is involved.
- **End-of-step review** (opus, `84e45387`): approve once the final round lands; 0 Critical, no new Important. Invariants held: no membership in owner, owner-level, LOGIN or reserved roles, `chelabase_orphaned` included; no reassignment to the owner except database ownership; personal rows answer their record's verifier only while it's live; generated code and `errors.mdx` are current.
  - **New findings for the final round:**
    - **EOS-1:** `upsert_member_branch_role` (`members.rs:2755`) can overwrite `password_enc = ''` from an in-flight reconcile, losing revoke-now's rotation. Use a compare-and-set.
    - **EOS-2:** Studio's `passwordlessString` uses `sslmode=require`; use `verify-full`, as the server does.
    - **EOS-3:** P33's Default-role picker offers owner-level or LOGIN roles. Filter them with `ruleRefusal`.
    - **EOS-4:** the sweep without Zitadel doesn't log once.
    - **EOS-5:** T9-H1 also covers revoke-now's `rotated_branches` across projects.
    - **EOS-6:** T3-M3 also covers project delete (databases stay `deleting`).
  - **Hardening:**
    - **EOS-H1:** recheck all-rows exposure at key create and role change.
    - **EOS-H2:** a sweep 404 revokes only if the org matches, or not when every lookup in a pass is a 404.
    - **EOS-H3:** the reveal secret lives in `PersonalLogins`'s state; move it into the dialog.
- **D7 (user, 2026-10-23): an offboarded mark.** The webhook (`user.deactivated`/`user.locked`/removal) and the sweep set `offboarded` on the member. `decide` refuses offboarded members on every route except `GET /v1/me`, as for suspended ones (403 `member_offboarded`). That cuts new personal logins and the SQL editor at once, with no Zitadel call per request. `user.reactivated`/`user.unlocked`, or the sweep seeing the user active, clears it.
  - **Ruling P19a (coordinator):** D7 adds a second migration, `20261023000002_members_offboarded.sql`, rather than editing T3's.
- **The final fix round** (every deferred finding above plus EOS and D7, batched by area; the test, coder and coordinator-diff-check pipeline):
  - **Area A (opus)**, access, personal logins and offboarding, in metadata, orchestrator and worker: D7, EOS-1, EOS-4, EOS-5, EOS-6, EOS-H2, T2-1, T2-4, T2-5 (note only), T3-M1–M3, T6-M1–M4, T6-M6, T8-1–T8-6, T8-8, T9-I1, T9-m1–m3, T9-H1, P33's e2e check.
  - **Area B (opus)**, privilege ops and role delete: T4-2–T4-7, T5-I1, T5-I2, T5-M1–M5, T10-1–T10-5, EOS-H1, plus the coder's extra live test `public_is_reserved_and_read_by_its_exact_name_only`.
  - **Area C (sonnet)**, Studio: S1-m2–m5, S4-1, EOS-2, EOS-3, EOS-H3, T10-6 (note only), and the flaky `two_quick_outside_presses_save_once`.
  - Order: A and C in parallel (disjoint files), then B.
- **Area C done** (sonnet/sonnet): RED `20efc113` (13 failing; S4-1's tests restored to name the database), fix `98a992ac` (Studio 2337/2337), diff verified by the coordinator (`verify-full`; the secret only in `RevealDialog`). Merged `74b42246`.
  - The flaky `two_quick_outside_presses_save_once` was a test bug: presence is reported from a passive effect, which the test sampled before it ran. Fixed with `waitFor` in it and two siblings.
- **Area A:** RED `f0e5ba9b` (test opus); the coder (opus) is running.
  - **Pinned:** `members.offboarded_at` (migration `…000002`), `clear_offboarded`, `offboarded_members`, `upsert_member_branch_role_if` (EOS-1 compare-and-set), `POST /internal/realtime/member`, 403 `member_offboarded`.
  - **EOS-H2 ruling:** a sweep pass where every lookup is a 404 revokes nothing. A lone deleted user is left to the webhook and VALID UNTIL.
- **Area B:** RED `f98d17a3` (test opus). The coder waits for Area A's merge.
  - **T10-2 ruling:** a guard raise after some databases moved says which ("objects already moved in: d1"); all-or-nothing would need `max_prepared_transactions`.
  - **T4-7:** no plan/diff test (the models can't tell marked-on-one-side from created or removed); the catalog filter is pinned.
  - **T4-6:** already 422, pinned by T5.
  - **T4-4:** the coder may update the older `the_reads_share_one_hash_expression_and_leave_no_fragment` deliberately.
- **Area A done:** fix `e6adbacb` (2541 tests; `members_live` 14/14 on the stack). The coordinator verified the migration (`offboarded_at`; `qualifies` refuses offboarded members and deleting or deleted databases and projects) and `decide` (`Offboarded` → 403). Merged `302a890e`.
  - **Coder's deviations, accepted:**
    - two RED match arms gained `ResetDatabaseSettings` (no assertion changed);
    - T7's `a_second_pass_does_nothing` expects one more lookup (D7 clears marks);
    - the realtime rate-limit test now sends its requests concurrently (the coder's own flake);
    - offboarded members' `cb_m_*` roles are dropped;
    - `create_personal_login` is now test-support only;
    - the new read is `member_grant_sources(tenant, member)`.
- **Area B:** fix `5f0f217c` (2546/2549; the 2 OpenAPI failures need api-gen on Area A's base). Live: `privileges_live` 2/2, `catalog_live` 1/1; the orchestrator live tests are deferred.
  - **Coder's deviations:**
    - the reserved `FOR ROLE` check moved to the in-transaction render (accepted, with a test);
    - EOS-H1 also starts from `anon`/`authenticated`/`authenticator` and refuses only chains the change adds (accepted);
    - the T10-4 event is recorded before the 500 (accepted).
  - **D8 (user, 2026-10-23): EOS-H1 fails closed, via the worker.** The coder had made it fail open when the owner connection was unavailable. A new worker RPC runs the check as `cloud_admin` (waking the compute). The orchestrator refuses 422 `exposes_data_api` on an exposure and 503 when the check can't complete.
  - **Flaky test seen:** `chelabase-neon` `create_tenant_timeouts_stop_at_the_budget` (fails under load, passes alone); fixed in this round.
  - Round 2 in progress.
- **Area B round 2 done** (opus/opus): RED `5475e7f8`, merge of `members-hub` `1d8da927` (no conflicts), fix `9df85f2e`.
  - The worker RPC `chelabase.rpc.roles.all_rows_check` runs as `cloud_admin` and fails closed. The orchestrator answers 422 `exposes_data_api` on an exposure and 503 when the check fails or times out; the old owner-connection check is removed.
  - Workspace 4177/4179 before api-gen (the 2 OpenAPI checks pass after it). The coder's edge tests caught an empty decoded role name, now refused.
  - The flaky `create_tenant_timeouts_stop_at_the_budget` was a test bug (wall-clock bounds over real sockets): a paused-clock unit test now pins the logic, and the socket test keeps only load-proof asserts.
  - The coordinator verified the diff. Merged `9b3b864a`.
- **The final fix round is complete.** Every deferred finding, EOS-1–EOS-6, H1–H3, D7 and D8 are fixed.
- **Process:** the agent preamble gained "one cargo command at a time per checkout" (user, `eb54b936` on `main`), after three concurrent cargo runs in one worktree only queued on its lock.
- **Gate: `just check` green on `719b8071`.** Rust 4179/4179 (673 skipped); Studio 2337/2337; the management-client, client, lagoon and docs suites; clippy, deny and the generated-code check clean. It took three attempts, none of them failures of this step's code:
  1. A textual lint: `personal_logins_live.rs` named only `CHELABASE_PROXY_DOMAIN` although it dialled the host address. Fixed with a comment, `b6c1b854`.
  2. A pre-existing SIGPIPE flake: `producer | grep -q` under `pipefail` in `workbench-recipes.sh`/`workbench-compose.sh`. Fixed with a `gq` helper that reads all input (`193be04b`; 30/30 runs).
  3. A new advisory, RUSTSEC-2026-0385 (`ctutils` 0.4.2, published 2026-10-23; `main` is affected too). Bumped to 0.4.5 (`719b8071`).
- **Live pass (items 4, 5.1–5.5) on `719b8071` (+ the plan doc):** apps restarted once (both migrations applied). All 7 e2e runs green, `personal-logins-e2e` included (P33's `options=-c role=…`, plus a refused group).
  - **Live suites:**
    - `members_live` 65/65, `live_db` 195/195;
    - `privileges_live` 10/10, `catalog_live` 15/15, `review_live` 2/2;
    - `api` 57/58, `sql_run_live` 22/22;
    - `d6_feasibility_live` 1/1, `personal_logins_live` 6/7, `member_roles_live` 30/30, `compute_fork_live` 11/11, `roles_live` 47/47, `branch_live` 5/5, `merge_live` 22/26.
  - **Six failures, all test bugs, being fixed:**
    1. A vacuous "nothing ran" check on PG15+ `public` USAGE (from `final B: RED tests`; the second time this mistake was made).
    2. `personal_logins_live` expected `None` where the contract is `Some(None)` (from T6's RED tests).
    3. Four pre-existing `merge_live` tests: the `fast_gc` helper sends no storage-controller bearer token, required since `136e1cc9` (2026-10-16 on `main`).
  - Only those six are rerun, by name.
- **Live test-bug fixes `993c4fbc`** (test opus; also a second vacuous `public` USAGE check, in `owner_run_grants_on_a_table_owned_by_a_settable_role`). `merge_live` authenticates with `storage_token_from_dir(…, StorageScope::Admin)`, as `branch_live` does.
  - Rerun by the coordinator, by name: `api` 2/2; `personal_logins_live` `a_removed_member_loses_the_login_on_every_branch` 1/1, now including its 57P01 session-end check; the four `merge_live` GC tests running.
- **Plan gap, ruled P35 (coordinator):** item 4 restarted the six backend apps but not `studio`. Studio's Vite dev server (up 14 h) served a stale generated client without `useListDatabasePersonalLogins`, so the hub pages crashed and the first browser pass was blocked. The coordinator restarted `studio` once (`workbench restart studio`) and checked that the module now serves the new hook. Future plans list `studio` in the live pass's restart.
- **All six fixed live tests are green:** `api` 2/2, `personal_logins_live` 1/1, and the four `merge_live` GC tests 4/4, which had been broken on `main` since 2026-10-16. **Every live suite and e2e run is now green.**
- **Browser pass (item 5.6, sonnet):** as `admin.maria` and `dev.tomas`; 38 captures in `.impeccable/review/` (untracked).
  - **Working:** the roster, grant edit and Save, the personal-login reveal with the Default role, reset, suspend and reinstate (logins revoked), the privilege edit, review and apply, the stale-read conflict with reload, the all-rows refusal, revoke invite, and both "Manage in Members" links.
  - **Defects for the Studio round:**
    - **BP-1:** re-inviting a pending invitee says "Already a member" and sends no new link; the comp says "Invited again with a new link". Use the resend path.
    - **BP-2:** Enter in the invite's role search with nothing highlighted submits the invite.
    - **BP-3:** 390 px horizontal overflow on roster cards (long email), the role page tab row, the branch Roles table and the Access tab.
    - **BP-4:** the member page's "Effective" line ignores unsaved changes.
    - **BP-5:** the Access tab lists `billing_reader` twice for one member (chain plus explicit).
- **Impeccable finish review (on the 38 captures): disposition `fix`, eight material fixes.**
  1. Invite results report per-email server outcomes truthfully (re-invite vs already a member vs failed), count only real sends, add the batch subtitle and "Edit and retry the failed one", and the post-invite status doesn't push the tabs down.
  2. The grant card: tab-tone segmented control inside cards and dialogs (DESIGN.md), Basalt's themed checkbox, Save in the footer with Discard and hidden when there's nothing to save, a "Filter N roles" field in a bordered frame, inline mono "also gives" and mono Effective role names, a 2px sand edge on changed rows, a summary next to "Narrow on a database", the No access segment, and an Effective line that reflects unsaved state (BP-4).
  3. A refused review keeps its evidence: the exposure flag, the dry-run SQL of the changes that passed, and "Dry run passed for changes 1 and 2".
  4. Roster list: the initials square, a non-wrapping level badge, `d MMM yyyy` dates, a bare project count, tags in the project filter (tenant admins marked), "—" and no tag for invited people, a one-row toolbar with the filter magnifier (Roles filter too), and "+" on Invite people.
  5. Member page: one shared `initials()`, actions on the name row with a bordered overflow button, a full-width coral suspend banner without the duplicate status line, an inline Add access row, a channel header band on the logins table with Delete neutral, a wrapping connection string in the reveal with icon copy buttons joined to their fields, and the comp's full read-only role lists on the self page (05).
  6. Invite compose: delivery first as one inline row, one-line access rows with the tab-tone control, mono email chips, and "+ Add a project".
  7. Browser-default controls: themed selects (`appearance:none` plus a Basalt chevron), a themed date-time field, and a fixed checkbox grid for New tables.
  8. 390 px: emails wrap; the role-page tab row, the branch Roles table and the Access tab scroll inside their frames (BP-3); member count in the Roles list view.
  - **Accepted deviations:** the icon-plus-word view toggle (the toolbar must stay one row); the "Every table" row carrying the column toggles; full grant cards on the suspended page.
  - **Process note:** the build had no Impeccable pipeline artefacts (`state.json`, `spec.json`, comp-diff reports), so fidelity was judged by eye. Recorded, not fixed.
- **D9 (user, 2026-10-24):** the shell changes in the comps (the rail turning into a header menu at 390 px, header breadcrumbs on every page) go to a follow-up step for all of Studio (roadmap). The hub keeps its back links.
- **D10 (user, 2026-10-24):** the member record gains `suspended_at` (date-time or null) and `suspended_by` (member id or null). Suspend writes them, reinstate clears them, and the suspend banner shows "Suspended on <d MMM yyyy> by <name>". They come from a new migration `20261023000003_members_suspended_by.sql` (the stack's metadata database already applied 000001 and 000002).
- **P36 (coordinator):** P33's Default role on login rows stays (the user asked for it on rows), moved with "Copy connection string" into the row's menu so the row doesn't wrap.
- **Studio fix round:** finish-review fixes 1–8, BP-1, BP-2, BP-5 (BP-3 and BP-4 are inside 8 and 2), D10's banner and P36.
  - Order: **K** (backend, sonnet: D10's columns, API fields and api-gen), then **S** (Studio, sonnet test and coder), which uses K's generated client.
  - Then a check of the Studio suites, a Studio restart, a targeted browser recapture of the changed screens, the user's pause, and `main`'s `ctutils` bump.
- **Done:**
  - **K** (D10): RED `074dd17a`, fix `5d565784`, then the test agent's fix of two RED bugs plus the realtime-config flake, `d9588332`. The flake: the config tests opened the real Docker socket while Docker was down; they now use an unreachable stand-in, and the worker passed 3×1027. Merged `37f8a8e6`.
  - **S:** RED `b4801710`, fix `917f810a`, plus two stale Access-tab tests `22e802df` (**process failure:** missed in the test agent's list of changed tests). Merged `3f675ed9`.
  - **Gate** green again on `3f675ed9`: Rust 4182/4182, Studio 2410/2410. Apps and Studio restarted.
  - **S2** (390 frames, header status, text fit, the action confirmation next to the header actions): RED `752691f5`, fix `49f4b5e4`, one RED-test fix `79f13d13`. The coder edited an older test that contradicted the new header rule, which was accepted and noted. Studio 2421/2421. Merged `d0d73cb1`; Studio restarted.
  - Reinstate works; the live report of "nothing happens" was the confirmation rendering off-screen at 390.
- **Finish review round 2** (on fresh captures, `fix`): first-review fixes 1, 3, 4 and 7 are done (7 apart from the grid); 2 and 5 are done apart from items 5 and 6 below; 6 and 8 are partial. Remaining material fixes:
  1. The branch Roles panel renders as a full-width row, not inside the branch table's Actions cell: `d MMM yyyy` dates, a one-line NOLOGIN, column gaps, and scroll frames at 390, including the older branch table.
  2. Invite compose: delivery as one inline row; access rows as one line at ≥sm; "Access · the same for everyone in this invite"; a ghost "+ Add a project".
  3. The All-rows labels at 390.
  4. The platform-role control at 390, as a 4-column grid that doesn't wrap.
  5. The suspend banner: full width, tinted, square marker, bold lead-in, and the suspended Project access subtitle.
  6. The role list: inline "also gives", a full border frame, and the hint.
  7. The New tables fixed checkbox grid.
  8. At 390, list-view emails wrap and the Roles list keeps the member count.
  - **Accepted deviations (recorded):**
    - the icon-plus-word toggle;
    - the "Every table" row;
    - full grant cards when suspended;
    - P36's row menu;
    - the self page's platform role shown as text;
    - D9's shell deferral;
    - the filter shown only above `FILTER_ABOVE`;
    - invite results as divided rows;
    - icon-only copy buttons in the reveal.
- **Studio polish rounds after the second review:**
  - **S3:** RED `18889c13`, fix `ac777d2e`, a RED type fix `7742c224`. Merged `66d121ce`.
  - **S4b:** RED `71f7d58d`, fix `01688b5a`. Merged `0534fba6`. The live "self page shows no roles" report wasn't a bug: an earlier capture round had removed `dev.tomas`'s roles (his Activity records it).
  - **S5:** RED `50408bb9`, fix `c92d2d5b`. Merged `c2d1b72b`; Studio 2472/2472.
  - In S2, S4b and S5, coders minimally edited older tests that contradicted the new pins: accepted, process deviation.
- **Third finish review:** round-2 items 3–8 done; items 1 (column gaps) and 2 (the invite access row) fixed in S5. After S5's recapture every hub page measures 390 px wide at 390.
- **Known issues, by the user's choice to pause now (2026-10-24):**
  - **KI-1:** the project Branches page (`branch-tree.tsx`, `project-page.tsx`) hides overflowing content instead of scrolling it: at 1440 the branch row's actions and the opened Roles panel, at 390 the Roles table, the create-owner-role button and the branch form. It goes to the D9 Studio shell step.
  - **KI-2:** in the invite form at 1440, a long role chip's text spills past its chip and the picker grows taller when focused.
  - Optional polish from the third review is done in S5 apart from KI-2.
- **RESUME HERE (2026-10-24):** every task, both fix rounds, the live pass (all suites and e2e green), the browser pass and three Impeccable reviews are done. The final `just check` on `c2d1b72b` is green (Rust 4182/4182, Studio 2472/2472, all other suites). **Paused for the user's merge decision.** Also at the pause: `main` needs the `ctutils` 0.4.5 bump (RUSTSEC-2026-0385); the branch carries it in `719b8071`. After the merge: tear down and purge the `members-hub` stack, delete the branch, then step 2 (teams), whose open question T2-2 (the SQL editor login's Viewer-level roles) goes to the user when step 2 is planned. Next: item 5.6 (the browser pass, captures and Impeccable's finish review, P34), then the user's pause, including `main`'s `ctutils` bump. Next: the live pass (app recreate, live suites, e2e, captures plus Impeccable's finish review, P34), then the user's pause. Next: the one final fix round (every deferred finding above, plus these reviews' findings), then `just check`, the live pass, and the pause for the user.
- **T4 GREEN** (coder opus): `edbbaca5` + `d9c07927` (R-T4a; its struct field forced `column: None,` into 4 RED-file literals, accepted). `privileges_live` 6/6, `catalog_live` 2/2 (marked-role tests). 3 orchestrator OpenAPI failures expected until T3's api-gen.
- **Carried to later briefs (from T4):**
  - **T3:** `schema_names_are_api_facing`'s list (`services/orchestrator/src/openapi.rs`) gains `DefaultObject`, `ExpectedAcl`, `FunctionRef`, `PrivilegeName`, `PrivilegeTarget`, `SchemaPrivilege`, `SequencePrivilege` (api-gen doesn't fix it).
  - **T5:**
    - fill `server_version_num` in every orchestrator `RenderContext` (until then MAINTAIN is refused);
    - an `AlterDefaultPrivileges`-only request puts its `DefaultAcl` target in `owners` (an owner run counts only with non-empty `owners`);
    - widen `owner_settable` to P27 and **deliberately update T4's `owner_settable_follows_direct_admin_or_set`** (written before T1's ruling; not a test bug), listed in T5's RED commit;
    - map `PrivilegeMismatch.column`;
    - the render allow-list's owner is `ctx.session_role`.
- **Housekeeping noted by D (not in scope):** impeccable reports `apps/studio/.impeccable/design.json` older than DESIGN.md and a stale `app-shell.tsx` brief (`/impeccable document` refreshes them); impeccable v4.7.0 is available.
- **R-T2a (coordinator, listed to the user):** a role a source names explicitly survives a narrowing below Admin even when another source is Admin (adding a source never takes access away); with every role in the union, an override's list is kept as is (R17). Agrees with today's `effective` on all 2,277 single-grant shapes. Cost if wrong: a member narrowed on a database keeps a team-named role there.
- **R-T4a (coordinator):** `PrivilegeMismatch` gains `column: Option<String>` (the seam gap the T4 test agent found); T5 maps it. Also accepted: `parse_privileges` returns `SchemaError`; the page type names the test agent chose are pinned; function ops render `ON ROUTINE`; T4 touches `changes.rs:109`/`:1214` (`..Default::default()`); T5 fills `server_version_num`.
- **Process notes:** the T2 and T4 test agents each used one heredoc/compound command (preamble rule); the content is in their commits.
- **Deferred findings (final fix round):** none yet.
- Branch `members-hub`, spec `792044ce`, plan: this commit.

## Plan rulings (mechanical, listed for the user)

Items marked **USER DECISION** need the user's answer (options and a recommendation are given); the rest are rulings with their cost if wrong.

- **P1 Effective types now, teams empty (the user's ask).** `chelabase-types` gains `GrantSources` (private fields, `teams: Vec<Grant>`) and `Effective` (platform level, `RoleSet`, a private per-role level map; `RoleSet::All { plus }` keeps named data API roles, MH-3). Every grant reader goes through `GrantSources::effective()`: `authorize`, the worker reconcile, `/me`, the project list, realtime, the SQL editor, the console, data keys and validation reads. The metadata builds `GrantSources` with `teams: Vec::new()`. Step 2 changes only the metadata SQL that fills `teams`, the team writes, and `chelabase_member_level` (P2); no type or caller signature changes. Write-path validation (`effective_on`/`check_effective`, `services/orchestrator/src/members.rs:1021-1194`) stays on the own grant being written in step 1 (MH-4 is step 2). Cost if wrong: step 2 reworks the types; low, since the shapes are pinned below from MH-1–3.
- **P2 One `qualifies`, two mirrors.** SQL function `chelabase_member_level(member, database)` plus `chelabase_personal_login_qualifies(member, database)` (migration, T3) and the Rust `access::qualifies` (T2). A live exhaustive test checks that they agree on every small state (T3). Step 2 replaces only the SQL function body and `GrantSources`' team input. Cost if wrong: two predicates drift; the agreement test catches that.
- **P3 Revocation by rule, not by trigger list.** Every metadata function that writes `project_grants`, `database_grant_overrides`, `members.status` or `members.tenant_admin` calls `revoke_unqualified_logins(tx, member)` before its commit. Remove, suspend, revoke-now and offboarding revoke unconditionally. A property test (random write sequences) checks that no unrevoked login of a non-qualifying member survives any write (T3). Cost if wrong: a forgotten writer; the property test enumerates the writers.
- **P4 Proxy cache.** The proxy's role-secret cache is cleared by a `PUBLISH` of `/password_updated` on `neondb-proxy-ws-updates` (`crates/chelabase-neon/src/proxy_cache.rs:11-21`, `:92-121`). The orchestrator's internal state already holds one as `cache: Arc<dyn CachePort>` (`services/orchestrator/src/main.rs:94-97`, `src/internal/ddl.rs:39-56`), and the worker holds `CacheNotifier` (`services/worker/src/ports.rs:1704-1732`). T8 adds `proxy_cache` to the public `AppState` (the same `Arc`) and publishes now and again 1 s later for every personal row of every affected member (`internal/ddl.rs:1201-1225`, made `pub(crate)`). The metadata verifier is already NULL in the same transaction, so any new lookup refuses even if Valkey is down. Cost if wrong (Valkey down): a proxy that cached the secret before the revoke accepts it until its cache TTL; the compute-side drop and `VALID UNTIL` still apply.
- **P5 `personal_logins.scram_verifier` is plain**, not the spec's `verifier_enc`. The proxy's copy in `neon_roles` is plain by design (`neon.rs:747-764`), so sealing adds nothing. It is NULL once revoked (a CHECK). Cost if wrong: a later migration seals it.
- **P6 Revoked records stay** (status "revoked", with their reason) until deleted. `PUT` on a database whose record is revoked or expired replaces it (new name allowed). Only a live record gives 409 `personal_login_exists`. Cost if wrong: one extra Delete click if the user wanted 409 there.
- **P7 Create converges through the reconciler.** `PUT` writes the record (with the verifier), then asks for one synchronous member-role reconcile of the main branch (the existing RPC, `services/orchestrator/src/lib.rs:1432-1437`, bounded by `MEMBER_ROLES_DEADLINE`, `services/worker/src/member_roles.rs:88-90`) and enqueues the other branches. The answer doesn't wait for other branches. If the main branch isn't ready or its reconcile fails, the answer is still 201 with `ready_on_main: false`. Cost if wrong: on a child branch the login appears seconds later.
- **P8 Reset is immediate.** The reset transaction writes the new verifier into the record and every `neon_roles` personal row of it, and the proxy cache is cleared. The old password is refused at once, and the new one works on a branch once its reconcile set it (the main one synchronously). Cost if wrong: a few seconds of "password refused" on child branches after a reset.
- **P9 All rows is its own op**, `set_all_rows { table, role, enabled }`, not `create_policy`/`drop_policy` as spec Part 4 first said. MH-26's comment marker, shape identification and exposure refusal need fields the generic ops don't carry. Cost if wrong: one more op variant.
- **P10 ACL staleness per object.** The read gives each object an `acl_hash` (sha256 over its whole ACL, all grantees, canonical order). `SchemaChangeRequest` gains `expect_acl: Vec<ExpectedAcl>`. A mismatch is 409 `privileges_changed_since_read` naming the objects. Cost if wrong: none beyond the field.
- **P11 Functions by OID.** The server resolves `oid::regprocedure` in the apply transaction and renders only that text, from `RenderContext.resolved_functions`. If the identity changed it answers 409 `privileges_changed_since_read`; if the function is gone, 404 with `op_index`.
- **P12 (user, 2026-10-23: (a) transient; T1 confirms it on PG17) How "run as the object's owner" (D6) gets SET.**
  - **(a) Recommended: transient.** Inside the apply transaction, for each distinct object owner X ≠ the database owner on which the owner holds ADMIN but not SET: `GRANT X TO <owner> WITH INHERIT FALSE, SET TRUE`, then the statements under `SET LOCAL ROLE X` … `RESET ROLE`, then `REVOKE SET OPTION FOR X FROM <owner>`, all in the same transaction. The owner's standing memberships never change, and neither does the member-role safety walk.
  - **(b) Persistent, as spec D6 words it:** the resync's owner-ADMIN invariant (`services/worker/src/resync.rs:788-797`) and `create_role_sql` grant `SET TRUE`.
  - T1 checks both on PG17. The plan pins (a) unless T1 shows Postgres refuses it. Cost if wrong: (b) touches the resync and every API-made role on every branch.
- **P13 `privileges_changed` is written after the compute's COMMIT** (a separate metadata write; no transaction spans both databases). A failed write is logged at `error!` with the request id. Cost if wrong: an applied change without its event when the metadata write fails.
- **P14 Suspended callers** get 403 `member_suspended` on every route except `GET /v1/me`, which answers with `status: "suspended"`. A suspended member's Zitadel sign-in is locked anyway.
- **P15 Suspend refuses** the caller themself (409 `conflict`) and the last active tenant admin (409 `last_tenant_admin`).
- **P16 (user, 2026-10-23: (b)) "Revoke Postgres access now" scope:** revoke the member's personal logins **and** rotate their `cb_m_*` passwords on every branch of every project they reach, ending those sessions (open SQL-editor and console sessions drop and reconnect with the new password). T8 adds it.
  - The rotation reuses the member-role reconcile's existing "fresh password, sessions end" step (`services/worker/src/member_roles.rs:30-33`, `:1309-1342`: a row that won't open gets a fresh password, `rotate` when the role exists, and `plan_member_roles` ends its sessions). Seam pass S2 pins the trigger: T3's `revoke_postgres_access` and `suspend_member` run `UPDATE member_branch_roles SET password_enc = '' WHERE member_id = $1` in their transaction (the row stays, so R31's `last_acted_role` survives; a `DELETE` would lose it, `members.rs:2606-2621`). T8 then reconciles every branch of every touched project. The SQL editor's next run finds the row unopenable and asks for the member-filtered reconcile itself (`sql_editor/routes.rs:466-491`).
  - Suspend does the same (a suspended member's roles are dropped anyway; the marker makes a reinstated member start with fresh passwords).
  - Live test (end-of-step pass): after revoke-now, an SQL-editor session opened before it is ended, and the next SQL-editor run succeeds with the new password.
- **P17 Webhook revocation is retried by Zitadel.** If the revocation write fails, the webhook answers 500 (Zitadel redelivers), unlike W18's best-effort realtime event (`services/orchestrator/src/internal/app.rs:551-567`).
- **P18 The migration deletes leftover `origin='personal'` `neon_roles` rows** (round 4 made the origin, no API ever created one; only test databases hold any). The reconciler drops their marked roles as orphans.
- **P19 One migration, owned by T3.** No later task adds a migration. A need for one is a deviation reported to the coordinator.
- **P20 Studio routes are S1's.** S1 creates every hub route file and commits `routeTree.gen.ts`. S2–S4 fill components only.
- **P21 OpenAPI and generated clients.** `just api-gen` (`justfile:99-102`) blesses `openapi/management.json` through each service's `document_is_current` test and regenerates `@chelabase/management-client` (and Studio's login client). `just check` refuses stale output (`justfile:127-129`).
  - Every task that runs `cargo test -p orchestrator` runs `just --justfile <wt>/justfile --working-directory <wt> api-gen` as its last step and commits the generated files with its code: T2, T3, T9, T5, T8, in that order of waves.
  - T4 changes `Operation` (in the document) without regenerating; T3 regenerates after it.
  - Two regenerating tasks never share a wave. A merge conflict in generated files is never hand-merged: the coordinator stops the merge, and the later task's coder reruns `api-gen` on the merged base.
- **P22 Live runs.**
  - Scratch-database live tests (metadata `members_live`/`live_db`, schema `catalog_live`/`privileges_live`, orchestrator api `changes_app`-style tests on the metadata Postgres) run inside their task, by name: no shared state.
  - T1's probe runs on the stack (its own database).
  - Everything that needs the step's own orchestrator, worker or gateway on the stack is deferred to the end-of-step live pass, after one app restart from the merged branch: the worker personal-login suites, the proxy refusal and the e2e scripts. Running them earlier would race the stack's older worker (its resync and sweep) on the same databases.
  - There is no mid-step stack refresh.
- **P23 The connection string** in a create or reset answer is for the main branch's default database. The member page says the same login works on every branch where the member qualifies, with that branch's host.
- **P24 (user, 2026-10-23: (a)) R3's "Personal logins on this branch: N" link:** a new `GET /v1/databases/{id}/personal-logins` (project Admin and tenant admins) lists whose logins exist, with names and expiry. It feeds the Roles tab's count and a list linking to each member's page (T8 adds the route, S3 the UI).
- **P25 Role events** (`role_created`, `role_updated`, `role_deleted`) are written by the orchestrator's role routes after the worker answers `Done`, with `branch_id`. Roles changed in SQL never reach the log, and the Activity tab says so (MH-37).

### T1's rulings (PG 17.11, report `docs/superpowers/notes/2026-10-23-members-hub-d6-feasibility.md`, `d7508299`)

These supersede P12's sequence and `owner_settable` wherever they differ. T5's brief carries them.

- **P26 Run-as sequence (corrects P12 (a)).** The owner has `createrole_self_grant = 'set, inherit'`, so API-made roles carry a second, self-granted row (INHERIT, SET) next to the ADMIN row. P12's `GRANT … INHERIT FALSE, SET TRUE` + `REVOKE SET OPTION` would permanently strip it. For each object owner X:
  1. If `pg_has_role(owner, X, 'SET')`: just `SET LOCAL ROLE X`.
  2. Else, if a membership row granted by the owner exists: `GRANT X TO owner WITH SET TRUE` … `REVOKE SET OPTION FOR X FROM owner`, which restores that row exactly.
  3. Else: `GRANT X TO owner WITH INHERIT FALSE, SET TRUE` … `REVOKE X FROM owner`, which leaves nothing.
  4. In every case, compare X's `pg_auth_members` rows before and after in the same transaction, and roll back on any difference.
- **P27 `owner_settable`** = `o = current_user OR pg_has_role(current_user, o, 'SET') OR pg_has_role(current_user, o, 'MEMBER WITH ADMIN OPTION')`. It matched all five probed cases. Roles made by another CREATEROLE role or by `cloud_admin` aren't settable and stay read-only. T5 edits `privileges_sql.sql`.
- **P28 Default privileges:** `ALTER DEFAULT PRIVILEGES FOR ROLE X` runs as X (T4's `run_as = for_role`), where SET suffices. Run as the owner it would need INHERIT.
- **P29 Verify-after matching:** `01007`/`01006` arrive as WARNINGs, and a relation-level `REVOKE` emits one per column (system columns included). T5 treats any such warning as failure per statement (not by counting warnings) and relies on the ACL re-read. Revoking another grantor's grant gives no warning and leaves it, so the re-read is what catches it.
- **P30 (user, 2026-10-23) `neon_superuser`-granted entries:** on objects `cloud_admin` created (which carry `neon_superuser` ALL WITH GRANT OPTION), the owner's grants are recorded with grantor `neon_superuser`. The editor revokes them **as the owner, without `run_as`**, with the same verify-after check. Any Admin can do it through the hub, since grantors are roles, not people.
- Confirmed: grants under `SET ROLE X` record grantor X. RLS, policy DDL and REVOKE work. A guarded (locked) Developer under `SET ROLE` is refused GRANT, CREATE POLICY and ALTER DEFAULT PRIVILEGES, while the owner run is allowed on protected branches. The safety walk and the reconcile are unaffected. The owner reads `pg_db_role_setting`.

- **P31 (user, 2026-10-23) "Exposes to the data API" flag:** the review step flags any grant (default privileges included) to `anon`/`authenticated` **or to a role either of them inherits** (over inherit-membership chains, as MH-26's check walks them). T5 computes it server-side in the dry run's answer; S3 shows it (comp 09).
- **P32 (user, 2026-10-23) Comps approved:** `docs/superpowers/designs/2026-10-23-members-hub/` (16 screens, `ac6aa91c`). S1–S4 build to them; the surface briefs in `apps/studio/.impeccable/surfaces/` (member page, roles, People-tab amendment) carry the confirm copy. Dark only (Studio has no light theme); phone width per DESIGN.md's rail-to-menu direction.

## Task table (binding)

| Task | Scope | Test | Coder | Audit (review) | Wave | Depends |
|---|---|---|---|---|---|---|
| T1 | D6 feasibility probe on the PG17 compute: committed live probe `d6_feasibility_live.rs` + report + rulings | opus (one agent: probe, run, report) | — | none (no review of its own; the coordinator reads the report; its rulings feed T4/T5) | W1 | stack up |
| T2 | Effective-access types (`GrantSources`, `Effective`, `RoleSet::All{plus}`, per-role `decide`, `qualifies`, Suspended facts/deny, new `ErrorCode`s); metadata `grant_sources` reads; every caller switched | opus | opus | opus | W1 | — |
| T4 | Schema crate: privilege ops (types + render), `set_all_rows`, privileges read SQL + parse, ACL compare and hash, exposure check, catalog CTEs exclude marked roles | opus | opus | opus | W1 | — (T1 rulings applied in T5) |
| T3 | Metadata: migration, `personal_logins` module, `chelabase_member_level`, revocation in every access write, suspend/reinstate/revoke-now/offboard writes, roster read, events (`branch_id`, new actions, `member_id` filter), `routable_role_secret`; `MemberStatus::Suspended` and its match sites; api-gen | opus | opus | opus | W2 | T2 |
| D | Comps with `/impeccable` (Operate mode, Basalt): the screens of spec Part 7 "Design" | opus (one agent, invokes the `impeccable` skill) | — | **the user approves the comps (pause)** | W2 | — |
| T6 | Worker personal logins: resync leaves marked roles alone, catalog reads them, generalised planner (create/rotate/expiry/lock/memberships/cleanup/drop), `neon_roles` personal rows, protected children, data keys, new-branch reconcile | opus | opus | opus | W3 | T2, T3 |
| T9 | Orchestrator roster and log: `access` on `GET /v1/members` (R1, MH-35), `suspended` listed, `member_id` filter and `branch_id` on events, role events in role routes; api-gen | sonnet | sonnet | opus | W3 | T3 |
| T5 | Orchestrator privileges: read route, run-as-owner (P12), verify-after (MH-24), `expect_acl`, all-rows exposure, allow-lists, `privileges_changed`, dry-run rule; api-gen | opus | opus | opus | W4 | T1, T3, T4 |
| S1 | Studio foundation and roster: hub routes and search params (P20), shared components (role checklist, grant card, level badge, view toggle), grouped People tab (cards/list), multi-email invite | sonnet | sonnet | sonnet | W4 | T9, D approved |
| T8 | Orchestrator personal logins and revocation: personal-login API, `AppState.proxy_cache` and `access_changed_for`, suspend/reinstate/revoke-now (+ Zitadel lock/unlock), webhook revocation, generic role routes hide personal rows, act-as/`check_names`/key refusals, `personal-logins-e2e.sh`; api-gen | opus | opus | opus | W5 | T3, T9 (files), T5 (files) |
| T7 | Worker identity sweep (every 10 min, Zitadel state) | sonnet | sonnet | none (leaf; end-of-step review) | W5 | T6 |
| S2 | Studio member page: project access cards, personal logins (reveal dialog), suspend/reinstate/revoke-now, activity | sonnet | sonnet | none (leaf; end-of-step review + Impeccable finish review) | W6 | S1, T8 |
| S3 | Studio Roles tab, role page, privilege editor (staged review/apply/conflict), create-role dialog | sonnet | opus | none (leaf; end-of-step review + Impeccable finish review) | W6 | S1, T5, T8 (P24's route, S8) |
| S4 | Studio project Access tab and branch Roles panel rebuilt on the shared components, "Manage in Members" links | sonnet | sonnet | none (leaf; end-of-step review) | W7 | S1, S2, S3 |

Reviews of their own: T2, T4, T3, T6, T9, T5, S1, T8 (the audit stage). None: T1 (probe; its report is read by the coordinator and its facts are asserted by its committed test), D (the user approves), T7, S2, S3, S4 (leaves). The end-of-step review (opus) covers all of them.

### Waves (at most 2 pipelines and 3 agents at once; at most 3 builds or suites)

| Wave | Lane A | Lane B | Lane C | Claude time |
|---|---|---|---|---|
| W1 | T2 | T4 | T1 (single agent, live on the stack) | 4.5 h |
| W2 | T3 | D (single agent), then **pause: the user approves the comps** | — | 4 h |
| W3 | T6 | T9 | — | 4.5 h |
| W4 | T5 | S1 (after the comps are approved) | — | 4 h |
| W5 | T8 | T7 | — | 4 h |
| W6 | S2 | S3 | — | 3.5 h |
| W7 | S4 | — | — | 2 h |
| End | review 1.5 h; final fix round 3 h; `just check` 0.7 h; app restart + live pass 3.5 h; browser pass + Impeccable finish review 1 h | | | ≈ 10 h |

Total ≈ 37 h of Claude time. Each task runs in its own worktree, `just wt-add members-hub-tN members-hub` (records the plan's stack in `.workbench-stack`), is merged back `--no-ff` and removed.

## Shared interfaces (pinned)

Implementers write these exactly. A change goes back to the coordinator.

### Access types (`crates/chelabase-types/src/access.rs`, T2)

```rust
pub enum RoleSet {
    /// An Admin's every role. `plus` holds only data API names (`anon`, `authenticated`) a
    /// source named (MH-3); `may_act_as` honours them. Always empty in step 1 except when an
    /// override names them.
    All { plus: BTreeSet<String> },
    Only(BTreeSet<PgRole>),
}

/// One source of access (a project grant; a team grant in step 2).
pub struct Grant { pub platform_role: PlatformRole, pub roles: RoleSet }

pub struct GrantSources { own: Option<Grant>, teams: Vec<Grant>, database: Option<Narrowing> }
impl GrantSources {
    pub fn new(own: Option<Grant>, teams: Vec<Grant>, database: Option<Narrowing>) -> Self;
    /// Union then narrowing (D1): `None` with no source. Per role: the highest level among the
    /// sources naming it (an Admin source names every non-data-API role at Admin), capped by
    /// the override's level; the override's role list intersects. Admin from any source and
    /// no narrowing below Admin gives `All { plus }`.
    pub fn effective(&self) -> Option<Effective>;
}

pub struct Effective {
    pub platform_role: PlatformRole,
    pub roles: RoleSet,
    role_levels: BTreeMap<String, PlatformRole>, // private
}
impl Effective {
    /// The level the member holds `name` at (`All`: the platform role for any name `may_act_as`
    /// accepts); `None` when not held.
    pub fn level_of(&self, name: &str) -> Option<PlatformRole>;
    /// The roles held at `min` or above (personal logins: `Developer`, D1).
    pub fn roles_at_least(&self, min: PlatformRole) -> RoleSet;
}
pub fn tenant_admin_effective() -> Effective; // replaces tenant_admin_grant() for decisions
/// MH-17: active (not suspended) and effective level ≥ Developer (tenant admins included).
pub fn qualifies(active: bool, effective: Option<&Effective>) -> bool;

pub enum MemberFacts { /* existing … */ Suspended }
pub enum Deny { /* existing … */ Suspended }
```

- `decide(…, grant: Option<&Effective>, …)`. An act-as of `Role(name)` passes only when `grant.roles.may_act_as(name)` and `grant.level_of(name) >= min`; the owner needs `platform_role == Admin` (unchanged).
- `MemberFacts::Suspended` → `Deny::Suspended`, checked where `Removed`/`Pending` are (`access.rs:335-345`).
- The old free function `effective(&Grant, Option<&Narrowing>)` becomes private.
- `Authorized.grant: Option<Effective>` (`services/orchestrator/src/access.rs:95`).
- New `ErrorCode`s (`crates/chelabase-types/src/api_error.rs`, added to `ALL`), all in T2:

| Code | Status | Meaning |
|---|---|---|
| `privileges_not_applied` | 409 | The re-read ACL differs from the requested end state |
| `privileges_changed_since_read` | 409 | `expect_acl` mismatch, or a function's identity changed |
| `exposes_data_api` | 422 | All rows refused, with `extra.chain` |
| `personal_login_exists` | 409 | A live login exists on that database |
| `member_suspended` | 403 | The caller is suspended |

### Metadata (`crates/chelabase-metadata`, T2 then T3)

T2:
- `MetadataClient::grant_sources(member, project, database: Option<Uuid>) -> Result<GrantSources, MetadataError>` replaces `grant_rows` (`members.rs:1190`).
- `database_grant_sources(project, database) -> Result<Vec<(Uuid, GrantSources)>, MetadataError>` replaces `database_grant_rows` (`members.rs:1233`).
- `GrantRows` is removed.
- Ports follow: orchestrator `MetadataPort::grant_sources` (`lib.rs:615`, `:1125`; `openapi.rs:615`; `tests/common/fakes.rs`); worker `WorkerMetadata::database_grant_sources` (`ports.rs:431`, `:1129`; `rt_events/delegate.rs:626`; `fakes.rs:2222`).

Migration `crates/chelabase-metadata/migrations/20261023000001_members_hub.sql` (T3; small snippet, the coder completes the comments):

```sql
-- Members hub step 1: suspended members, personal logins per (member, database), the log's branch.
ALTER TABLE members DROP CONSTRAINT members_status_check;
ALTER TABLE members ADD CONSTRAINT members_status_check
  CHECK (status IN ('invited','pending','active','suspended','removed'));
ALTER TABLE access_events ADD COLUMN branch_id uuid;
CREATE INDEX access_events_target ON access_events (tenant_id, target_member_id, id DESC);
CREATE INDEX access_events_actor ON access_events (tenant_id, actor_member_id, id DESC);
CREATE TABLE personal_logins (
  member_id uuid NOT NULL REFERENCES members(id),
  database_id uuid NOT NULL REFERENCES databases(id) ON DELETE CASCADE,
  role_name text NOT NULL CHECK (role_name ~ '^[a-z_][a-z0-9_]{0,62}$'),
  scram_verifier text,
  valid_until timestamptz NOT NULL,
  revoked_at timestamptz,
  revoked_reason text CHECK (revoked_reason IN
    ('access_lost','suspended','revoked_now','identity_offboarded','member_removed')),
  created_at timestamptz NOT NULL DEFAULT now(),
  updated_at timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (member_id, database_id),
  UNIQUE (database_id, role_name),
  UNIQUE (member_id, database_id, role_name),
  CHECK ((revoked_at IS NULL) = (scram_verifier IS NOT NULL)),
  CHECK ((revoked_at IS NULL) = (revoked_reason IS NULL)));
DELETE FROM neon_roles WHERE origin = 'personal';  -- P18
ALTER TABLE neon_roles
  ADD COLUMN member_id uuid,
  ADD COLUMN personal_database_id uuid,
  ADD CONSTRAINT neon_roles_personal_member
    CHECK ((origin = 'personal') = (member_id IS NOT NULL AND personal_database_id IS NOT NULL)),
  ADD CONSTRAINT neon_roles_personal_unsealed CHECK (origin <> 'personal' OR password_enc IS NULL),
  ADD CONSTRAINT neon_roles_personal_login
    FOREIGN KEY (member_id, personal_database_id, name)
    REFERENCES personal_logins (member_id, database_id, role_name) ON DELETE CASCADE;
-- P2: the one level predicate (step 2 replaces the body to add team grants).
CREATE FUNCTION chelabase_member_level(p_member uuid, p_database uuid) RETURNS text
LANGUAGE sql STABLE AS $$
  SELECT CASE
    WHEN m.tenant_admin THEN 'admin'
    WHEN g.platform_role IS NULL THEN NULL
    ELSE (ARRAY['viewer','developer','admin'])[least(
           array_position(ARRAY['viewer','developer','admin'], g.platform_role),
           coalesce(array_position(ARRAY['viewer','developer','admin'], o.platform_role), 3))]
  END
  FROM members m
  JOIN databases d ON d.id = p_database AND d.status <> 'deleted'
  JOIN projects p ON p.id = d.project_id AND p.tenant_id = m.tenant_id
  LEFT JOIN project_grants g ON g.member_id = m.id AND g.project_id = d.project_id
  LEFT JOIN database_grant_overrides o ON o.member_id = m.id AND o.database_id = d.id
  WHERE m.id = p_member AND m.status = 'active'
$$;
CREATE FUNCTION chelabase_personal_login_qualifies(p_member uuid, p_database uuid) RETURNS boolean
LANGUAGE sql STABLE AS $$
  SELECT coalesce(chelabase_member_level(p_member, p_database) IN ('developer','admin'), false)
$$;
```

New module `crates/chelabase-metadata/src/personal_logins.rs` (T3):

```rust
pub enum RevokeReason { AccessLost, Suspended, RevokedNow, IdentityOffboarded, MemberRemoved } // as_str: snake_case
pub enum LoginStatus { Active, Expired, Revoked }
pub struct PersonalLogin { pub member_id: Uuid, pub database_id: Uuid, pub project_id: Uuid,
    pub database_name: String, pub role_name: String, pub valid_until: DateTime<Utc>,
    pub created_at: DateTime<Utc>, pub revoked_at: Option<DateTime<Utc>>,
    pub revoked_reason: Option<RevokeReason>, pub status: LoginStatus }
/// The worker's read: `qualifies` from `chelabase_personal_login_qualifies` in the same query.
pub struct PersonalLoginRecord { pub member_id: Uuid, pub database_id: Uuid, pub role_name: String,
    pub scram_verifier: Option<String>, pub valid_until: DateTime<Utc>,
    pub revoked: bool, pub qualifies: bool }
/// Where a login's proxy rows are: one per branch endpoint holding a personal `neon_roles` row.
pub struct LoginEndpoint { pub branch_id: Uuid, pub endpoint_id: String, pub role_name: String }
pub enum LoginWriteError { NotQualified, Exists, NameTaken, NotFound, Access(AccessWriteError), Db(sqlx::Error) }

impl MetadataClient {
    /// Under the member's lock: qualifies (SQL fn) or NotQualified; a live record → Exists; the
    /// name used by another login of the database or by any `neon_roles` row of any live branch
    /// endpoint of the database → NameTaken, except rows of the member's own record on that
    /// database, which is being replaced (S10); a revoked/expired record is replaced (P6) by
    /// DELETE (its personal rows cascade) then INSERT in the same transaction, never an UPDATE
    /// of `role_name` (the FK has no ON UPDATE). Event `personal_login_created` (actor = target
    /// = member; after: role_name, valid_until, level).
    pub async fn create_personal_login(&self, member: Uuid, database: Uuid, role_name: &str,
        scram_verifier: &str, valid_until: DateTime<Utc>) -> Result<PersonalLogin, LoginWriteError>;
    /// The live record's verifier and valid_until replaced, and every personal `neon_roles` row
    /// of it set to the new verifier (P8). NotFound without a live record. Event `personal_login_reset`.
    pub async fn reset_personal_login(&self, member: Uuid, database: Uuid, scram_verifier: &str,
        valid_until: DateTime<Utc>) -> Result<(PersonalLogin, Vec<LoginEndpoint>), LoginWriteError>;
    /// Deletes the record (its `neon_roles` rows cascade); returns where they were. Event
    /// `personal_login_deleted`.
    pub async fn delete_personal_login(&self, actor: Uuid, member: Uuid, database: Uuid)
        -> Result<Vec<LoginEndpoint>, LoginWriteError>;
    pub async fn member_personal_logins(&self, member: Uuid) -> Result<Vec<PersonalLogin>, MetadataError>;
    pub async fn database_personal_logins(&self, database: Uuid) -> Result<Vec<PersonalLogin>, MetadataError>;
    pub async fn personal_login_records(&self, database: Uuid) -> Result<Vec<PersonalLoginRecord>, MetadataError>;
    /// Whether `name` is a personal login of `database` (any status).
    pub async fn is_personal_login_name(&self, database: Uuid, name: &str) -> Result<bool, MetadataError>;
    /// S12: every personal-login name (any status) of every database of `project` (a project
    /// grant's `pg_roles` apply to all of them; `check_names` reads this).
    pub async fn personal_login_names(&self, project: Uuid) -> Result<BTreeSet<String>, MetadataError>;
    /// Every personal `neon_roles` row of the member, whatever its record's status (revoked
    /// records' rows stay until the worker drops the role, and the cache must still be told).
    pub async fn member_login_endpoints(&self, member: Uuid) -> Result<Vec<LoginEndpoint>, MetadataError>;
    /// The worker's write: a personal row for `record` on `endpoint_id`. The verifier is stored
    /// only when the record is live in the same statement (never written back for a revoked
    /// record, MH-9); otherwise NULL.
    pub async fn upsert_personal_role_row(&self, endpoint_id: &str, member: Uuid, database: Uuid,
        role_name: &str, scram_verifier: &str) -> Result<(), MetadataError>;
    /// Deletes only a row whose origin is `personal`.
    pub async fn delete_personal_role_row(&self, endpoint_id: &str, role_name: &str) -> Result<(), MetadataError>;
    /// An active member → suspended: every live login revoked (`suspended`), member roles
    /// marked for rotation (S2: `member_branch_roles.password_enc = ''`), event
    /// `member_suspended`. Returns the touched projects (the `remove_member` union,
    /// `members.rs:1784-1797`, plus the projects of the member's personal logins). Already
    /// suspended → `Ok` with the touched projects and no write (S7). Not active → `Conflict`;
    /// the last active tenant admin → `LastTenantAdmin`.
    pub async fn suspend_member(&self, actor: Uuid, member: Uuid) -> Result<Vec<Uuid>, AccessWriteError>;
    /// Suspended → active (logins stay revoked). Already active → `Ok` with no write (S7);
    /// invited, pending or removed → `Conflict`.
    pub async fn reinstate_member(&self, actor: Uuid, member: Uuid) -> Result<(), AccessWriteError>;
    /// "Revoke Postgres access now" (P16 b, S2): every live login revoked (`revoked_now`) and
    /// member roles marked for rotation, in one transaction; event `personal_logins_revoked`
    /// (`after: {count, reason, rotated_branches}`). Returns the touched projects.
    pub async fn revoke_postgres_access(&self, actor: Uuid, member: Uuid) -> Result<Vec<Uuid>, AccessWriteError>;
    /// The webhook's and the sweep's write: the tenant member with this Zitadel user, all live
    /// logins revoked (`identity_offboarded`); unknown user → empty.
    pub async fn revoke_logins_for_zitadel_user(&self, tenant: TenantId, zitadel_user_id: &str)
        -> Result<Vec<LoginEndpoint>, MetadataError>;
    /// `(tenant, member, zitadel_user_id)` of members holding a live login (the sweep).
    pub async fn members_with_live_logins(&self) -> Result<Vec<(TenantId, Uuid, String)>, MetadataError>;
    /// Every own grant of the tenant's members: `(member, project, platform_role)` (the roster).
    pub async fn member_project_roles(&self, tenant: TenantId)
        -> Result<Vec<(Uuid, Uuid, PlatformRole)>, MetadataError>;
    pub async fn record_access_event(&self, e: &AccessEventInput<'_>) -> Result<(), MetadataError>;
}
/// In the caller's transaction: every live login of `member` whose
/// `chelabase_personal_login_qualifies` is false is revoked (`access_lost`): revoked_at,
/// reason, verifier NULL; its `neon_roles` rows' verifier NULL; one `personal_logins_revoked`
/// event, written only when the count is above 0 (S18). Returns how many.
pub(crate) async fn revoke_unqualified_logins(tx: &mut Tx, member: Uuid) -> Result<usize, sqlx::Error>;

pub struct AccessEventInput<'a> { pub tenant: TenantId, pub actor: Option<Uuid>, pub action: AccessAction,
    pub target: Option<Uuid>, pub project: Option<Uuid>, pub database: Option<Uuid>,
    pub branch: Option<Uuid>, pub before: Option<&'a Value>, pub after: Option<&'a Value> }
```

- `access_events(tenant, project, member: Option<Uuid>, before_id, limit)` gains `member`: target or actor (`members.rs:2462`). T3 updates the one caller, `MembersPort for MetadataClient` (`services/orchestrator/src/members.rs:312-320`), to pass `None`; T9 threads the query parameter (S13).
- `AccessEvent` gains `branch_id: Option<Uuid>` (T3 also fixes the literal in `services/orchestrator/tests/common/fakes.rs:2785`).
- `list_member_listings` and `member_listing` already include `suspended` members (their filter is `status <> 'removed'`, `members.rs:1354`, `:1371`); a test pins it.
- `remove_member`'s `touched` adds the projects of the member's personal logins (`members.rs:1784-1797`).
- `routable_role_secret` (`neon.rs:739-764`, `role_secret_row`) answers no verifier for a personal row whose record is revoked or past `valid_until` (a join on `personal_logins` in the same statement).
- `neon_roles`/`neon_roles_versioned` (`neon.rs:767-797`) never return personal rows (they read `origin = 'owner'` only; a test pins it).
- **S1: no generic writer touches a personal row.** The migration's CHECK `neon_roles_personal_member` and FK make any personal row written without its record fail, and today three paths write them:
  - `apply_resync` (`neon.rs:1774-1830`) flips `personal` → `sql` (violates the CHECK, failing the whole resync transaction) and writes the catalog verifier back (MH-9). T3: its upsert's `WHERE` becomes `r.origin NOT IN ('owner', 'personal')`, the flip is deleted, and an upsert whose `origin` is `personal` is skipped; removals already skip personal rows (`:1855-1858`).
  - The branch copy (`branches.rs:1001-1025`) inserts every parent row with its origin. T3 skips `origin = 'personal'` copies there, and T6 filters them out of `child_role_copies` (`services/worker/src/branch_create.rs:294-306`, `:351`): the child's personal rows come from its reconcile.
  - The by-name writers (`ddl.rs:213-317`; `neon.rs:1248`, `:1391`, `:1462`, `:1500`, `:1543-1558`, `:1588`, `:1660`, `:1720`, `:1903`) gain `AND origin <> 'personal'` (or skip a personal row), so the DDL receiver, role routes and renames never change one.
  - Only `upsert_personal_role_row`, `delete_personal_role_row`, the revocation writes and the reset write personal rows.
- `MemberStatus::Suspended` (`members.rs:41-69`). **S6:** `ensure_member` returns a suspended member as is in both of its matches (`members.rs:1057` and `:1084`; the second one's catch-all `Some(m) =>` arm would otherwise activate a suspended member that has an open invite), and so does the orchestrator's `ensure` (`services/orchestrator/src/lib.rs:1765`). `bootstrap_admin` (`members.rs:2499-2530`) keeps reactivating (the operator's recovery path) and its doc says so.

`AccessAction` gains (strings exactly). T3 adds every variant below to the enum, `as_str` and `ALL` (`members.rs:252-296`), so T9 and T5 only write them (S13); T9 also updates the action list in the API's `AccessEvent` doc (`services/orchestrator/src/members.rs:783-786`):

| Action | Written by | Notes |
|---|---|---|
| `member_suspended`, `member_reinstated` | T3 | |
| `personal_logins_revoked` | T3 | `after: {count, reason}` |
| `personal_login_created`, `personal_login_reset`, `personal_login_deleted` | T3 | `database_id` set; `after`/`before` carry `role_name` only, never a secret |
| `role_created`, `role_updated`, `role_deleted` | T9, through `record_access_event` | `project_id`, `database_id`, `branch_id`; `after: {role, login, attributes}` |
| `privileges_changed` | T5 | `after: {pg_database, acted_as, ops: [{op, object}]}`, never SQL |

### Schema crate and ops (`crates/chelabase-types/src/table_api.rs`, `crates/chelabase-schema`, T4)

New `Operation` variants (`serde(rename_all = "snake_case")`, `deny_unknown_fields`):

```rust
GrantSchema { schema: String, grantee: Grantee, privileges: Vec<SchemaPrivilege> },
RevokeSchema { schema: String, grantee: Grantee, privileges: Vec<SchemaPrivilege>, #[serde(default)] grant_option_only: bool },
GrantSequence { sequence: TableRef, grantee: Grantee, privileges: Vec<SequencePrivilege> },
RevokeSequence { sequence: TableRef, grantee: Grantee, privileges: Vec<SequencePrivilege>, #[serde(default)] grant_option_only: bool },
GrantFunction { function: FunctionRef, grantee: Grantee },            // EXECUTE
RevokeFunction { function: FunctionRef, grantee: Grantee, #[serde(default)] grant_option_only: bool },
AlterDefaultPrivileges { for_role: String, schema: String, object: DefaultObject, grant: bool,
                         grantee: Grantee, privileges: Vec<PrivilegeName> },
SetAllRows { table: TableRef, role: String, enabled: bool },
```

```rust
pub enum SchemaPrivilege { Usage, Create }
pub enum SequencePrivilege { Usage, Select, Update }
pub enum DefaultObject { Tables, Sequences, Functions }
pub enum PrivilegeName { Select, Insert, Update, Delete, Truncate, References, Trigger, Maintain, Usage, Execute }
/// `oid` from the privileges read; `identity` its `regprocedure` text then (P11).
pub struct FunctionRef { pub oid: u32, pub identity: String }
pub struct ExpectedAcl { pub object: PrivilegeTarget, pub acl_hash: String }
pub enum PrivilegeTarget { Schema { schema: String }, Relation { schema: String, name: String },
    Sequence { schema: String, name: String }, Function { oid: u32 },
    DefaultAcl { for_role: String, schema: String, object: DefaultObject } }
// SchemaChangeRequest gains: #[serde(default)] pub expect_acl: Vec<ExpectedAcl>
```

- None of the new ops has a grant option (MH-29). `grant_option_only` renders `REVOKE GRANT OPTION FOR …`. (The existing table `Grant` refuses `with_grant_option: true` with 400: S21, user.)
- `RenderContext` gains:
  - `resolved_functions: BTreeMap<u32, String>`: the regprocedure text per OID, filled by the orchestrator in the transaction; render refuses a function op whose OID is missing;
  - `owners: BTreeMap<PrivilegeTarget, String>`: object owners, filled in the transaction (empty for a role run);
  - `session_role: String`: the database owner for an owner run, the acting role for a role run;
  - `server_version_num: i32`: `Maintain` is refused below 170000.
- `RenderedStatement` gains `run_as: Option<String>` (S4):
  - `Some(owner)` only in an owner run (`session_role` is the database owner), when the object's owner from `owners` isn't the session role;
  - it applies to the existing table `Grant`/`Revoke` (target `Relation`), every new op, and `SetAllRows` (CREATE/DROP/COMMENT ON POLICY need the table's owner);
  - for `AlterDefaultPrivileges` the "owner" is `for_role`: with `for_role` ≠ the session role it runs as `for_role`, so `FOR ROLE` names the current role and needs nothing beyond SET (T1 item 4 confirms);
  - a role run (`owners` empty) never sets it: Postgres decides what that role may do.
- `ddl::privilege_grantee(name: &str, owner: &str) -> R<&str>` refuses the owner, `act_as_reserved` names (`anon`/`authenticated` allowed), `authenticator` and PUBLIC (PUBLIC only in revokes, MH-28). **S5:** it applies to the existing table `Grant`/`Revoke` ops too (the privilege editor's table and column privileges are those ops, so MH-28 would otherwise be bypassed through them), and to `SetAllRows.role`. Personal logins are refused in the orchestrator (they need the metadata).
- Render refusals are `DdlError`s with new `DdlErrorKind`s (`GranteeNotAllowed(String)`, `ForRoleNotAllowed(String)`, `PrivilegeNotSupported(&'static str)` for `Maintain` below PG17, `UnresolvedFunction(u32)`), answered as today's 400 `invalid_request` with `op_index` (`services/orchestrator/src/table_api/changes.rs:86-88`). T5's personal-login grantee refusal answers the same. Only the owner-not-settable refusal is 422 `invalid_request` with `extra.owner` (T5, in the transaction).
- `crates/chelabase-schema/src/privileges.rs`:

```rust
pub const ALL_ROWS_MARKER: &str = "chelabase:all_rows"; // COMMENT ON POLICY text
/// One schema page of `role`'s privileges, every grantor (MH-24), marked roles and `cb_m_*`
/// excluded (MH-13, MH-32). `schema: None` → the schema list and default ACLs only. Every
/// object of the schema is listed (paged), with `role`'s entries; `acl_hash` and
/// `owner_settable` are computed in this SQL (S3, S4).
pub fn privileges_sql() -> &'static str; // params: $1 role, $2 schema (nullable), $3 after (nullable), $4 limit
pub fn parse_privileges(json: &str) -> Result<PrivilegesPage, CatalogError>;
/// S3: the current `acl_hash` of each target, same expression as `privileges_sql`.
/// params: $1 jsonb array of `PrivilegeTarget` (their serde form); returns one jsonb array
/// `[{object, acl_hash}]`, a missing object left out.
pub fn acl_hashes_sql() -> &'static str;
/// MH-24: the end state `ops` ask for, against the re-read pages (one per (grantee, schema)
/// the ops touch, plus the `schema: None` page for schema and default-privilege ops, each read
/// to its last page); every difference.
pub fn verify_privileges(ops: &[Operation], after: &[PrivilegesPage]) -> Result<(), Vec<PrivilegeMismatch>>;
pub struct PrivilegeMismatch { pub object: PrivilegeTarget, pub privilege: String,
    pub grantee: String, pub still_granted_by: Option<String>, pub wanted: bool }
/// MH-26: the membership chain by which `role` is, or is inherited by, `anon`, `authenticated`,
/// `authenticator`, one of `key_roles`, or PUBLIC; `None` when it's safe. `edges`: member → roles
/// it is a member of (any option).
pub fn all_rows_exposure(edges: &BTreeMap<String, BTreeSet<String>>, role: &str, key_roles: &BTreeSet<String>) -> Option<Vec<String>>;
pub fn all_rows_policy_name(role: &str, taken: &BTreeSet<String>) -> String; // `<role>_all_rows`, ≤63 bytes, 8-hex suffix on a clash
```

- `PrivilegesPage` is the read's JSON body, below.
- **S3, the hash.** `acl_hash` is hex sha256 over the object's **whole** ACL (every grantee, not only `?role=`'s entries), as sorted `(grantee, privilege, grantor, grantable)` rows from `aclexplode` by name; a relation's hash also covers every column's `attacl` (a column grant changes it); an empty or NULL ACL hashes the empty list. The page carries only `role`'s entries, so the hash is computed in SQL, not from them (the earlier `acl_hash(entries)` is dropped). T5 compares `expect_acl` against `acl_hashes_sql` in the apply transaction, before the statements.
- Serde forms the client sends back: `PrivilegeTarget` and `DefaultObject` are `rename_all = "snake_case"`, externally tagged (`{"relation": {"schema": "public", "name": "t"}}`, `{"function": {"oid": 16384}}`); `FunctionRef` is `{oid, identity}`. `model::Grant` is `{grantor: {oid, name}, grantee: "public" | {"role": {oid, name}}, privilege: "SELECT", grantable}` (upper case, `model.rs:55-87`); the ops' privileges are lower snake case, so the UI maps `privilege.toLowerCase()`. `PrivilegeMismatch.privilege` is upper case like `model::Grant`; `grantee` and `still_granted_by` are role names.
- The catalog `reserved` CTE (`catalog_sql.sql:4-11`) and `hidden` CTE (`catalog_parts/head.sql:57-63`) also exclude marked roles, with the round-4 predicate (`pg_db_role_setting`, `setdatabase = 0`, a `chelabase.managed_role=` entry; `crates/chelabase-types/src/role_name.rs:263-286`).

### Orchestrator routes (exact)

| Endpoint const (`chelabase-routes`) | Method, path | Access | `operationId` (S14) | Task |
|---|---|---|---|---|
| `GET_PRIVILEGES` | GET `/v1/databases/{id}/branches/{branch_id}/pg-databases/{name}/privileges` | `Project(Viewer)` | `getPrivileges` | T5 |
| `LIST_MEMBER_PERSONAL_LOGINS` | GET `/v1/members/{member_id}/personal-logins` | `TenantMember` (handler filter, MH-22) | `listMemberPersonalLogins` | T8 |
| `LIST_DATABASE_PERSONAL_LOGINS` | GET `/v1/databases/{id}/personal-logins` | `Project(Admin)` (P24a) | `listDatabasePersonalLogins` | T8 |
| `PUT_PERSONAL_LOGIN` | PUT `/v1/databases/{id}/personal-login` | `Project(Developer)` (= qualifies on that database, overrides included) | `createPersonalLogin` | T8 |
| `RESET_PERSONAL_LOGIN` | POST `/v1/databases/{id}/personal-login/reset-password` | `Project(Developer)` | `resetPersonalLoginPassword` | T8 |
| `DELETE_PERSONAL_LOGIN` | DELETE `/v1/databases/{id}/personal-logins/{member_id}` | `TenantMember` (handler: self, tenant admin, or effective Admin on the database; else 404) | `deletePersonalLogin` | T8 |
| `SUSPEND_MEMBER` | POST `/v1/members/{member_id}/suspend` | `TenantAdmin` | `suspendMember` | T8 |
| `REINSTATE_MEMBER` | POST `/v1/members/{member_id}/reinstate` | `TenantAdmin` | `reinstateMember` | T8 |
| `REVOKE_MEMBER_POSTGRES_ACCESS` | POST `/v1/members/{member_id}/revoke-postgres-access` | `TenantAdmin` | `revokeMemberPostgresAccess` | T8 |

Each new endpoint also gets its row in `chelabase-routes`' `spec_table` test (`crates/chelabase-routes/src/lib.rs:1749-1915`, which asserts every endpoint has one) and its `operationId` in `operation_ids_are_the_contract` (`services/orchestrator/src/openapi.rs:962`), in the commit that routes it.

Changed:
- `GET /v1/members` gains `access` and lists suspended members (T9).
- `GET /v1/access-events` and `GET /v1/projects/{id}/access-events` gain `?member_id=` and `branch_id` in each event (T9).
- `POST …/schema-changes` gains the new ops and `expect_acl` (T5).
- `MemberStatusView` gains `suspended` (T3).

Shapes (JSON field names exact):

```text
Member.access: { level: "tenant_admin"|"admin"|"developer"|"viewer"|"other"|"none",
                 projects: [{ project_id, platform_role, via: "own" }] }   // R1 + MH-35; via always "own" in step 1

PersonalLogin: { member_id, database_id, database_name, project_id, role_name, valid_until,
                 created_at, status: "active"|"expired"|"revoked", revoked_at, revoked_reason }
PersonalLoginList: { logins: [PersonalLogin] }                     // never 403 for the filter: an empty list (MH-22);
                                                                    // a suspended caller still gets 403 member_suspended (P14)
   // the database list (P24) returns every record of the database (any status); task S3 counts status "active"
PutPersonalLoginRequest: { name }                                   // required; role_name::check_login_role_name (role_name.rs:193)
PersonalLoginSecret: { login: PersonalLogin, password, connection_string, ready_on_main: bool }
   // 201 on PUT, 200 on reset; Cache-Control: no-store; password ≥128 bits (credentials::generate_password)
   // S2 renew = reset (spec Part 6: "renewed by a reset"); there is no renew route
RevokeResult: { revoked: n, rotated_branches: n }                   // revoke-postgres-access (S2)

PUT errors:   403 forbidden (extra.required "developer") | 403 member_suspended | 404 not_found (database)
              | 409 personal_login_exists | 409 role_exists (name taken, P-D3) | 422 invalid_role_name | 422 reserved_role_name
Reset errors: 404 not_found (no live login) | 403 as PUT
S15 LoginWriteError → answer: NotQualified → 403 forbidden {required: "developer"}; Exists → 409 personal_login_exists;
              NameTaken → 409 role_exists; NotFound → 404 not_found; Access(e) → members.rs `write_failed` (:841-861); Db → 500
Suspend:      200 Member | 409 conflict (invited, pending or removed, or self) | 409 last_tenant_admin
              | 503 unavailable (Zitadel lock failed; Chelabase side done). S7: suspending a suspended member is 200 and
              redoes only the Zitadel lock and session delete, so the retry the 503 asks for works.
Reinstate:    200 Member | 409 conflict (invited, pending or removed) | 503 unavailable (unlock failed). S7: reinstating
              an active member is 200 and redoes only the unlock.

Privileges read (?role= required; ?schema=; ?after=; ?limit= default 200, max 500):
{ role, server_version_num, offered: { table: [...], sequence: [...], schema: [...], function: ["execute"] },
  schemas: [{ name, owner, owner_settable, grants: [Grant], acl_hash }],          // schema-level entries
  default_privileges: [{ for_role, schema, object, grants: [Grant], acl_hash }],
  objects: [{ kind: "table"|"view"|"materialized_view"|"partitioned_table"|"foreign_table"|"sequence"|"function"|"procedure",
              schema, name, oid, identity, owner, owner_settable, rls_enabled,
              grants: [Grant], columns: [{ name, grants: [Grant] }],
              all_rows: null | { policy }, acl_hash }],                            // objects only with ?schema=
  next_after: null | string }
  Grant = chelabase_schema::model::Grant { grantor, grantee, privilege, grantable } (grantee = role only)
  owner_settable: the owner, or (P12) a role the owner holds ADMIN or SET on; else the UI greys it.
    S4: computed in privileges_sql (T4) as `owner = <database owner>` OR a direct pg_auth_members
    row (roleid = owner, member = the database owner) with admin_option OR set_option. The database
    owner is `current_user` of the owner connection the read runs on. T1's report may narrow it
    (item 6); T5 then changes privileges_sql.sql (in T5's files for that reason).
Errors: 404 not_found (role not on the branch, or pg database not in branch_databases)

schema-changes additions:
  409 privileges_not_applied   extra.mismatches: [{object, privilege, grantee, still_granted_by, wanted}]
  409 privileges_changed_since_read extra.objects: [PrivilegeTarget]
  422 exposes_data_api         extra.chain: [role…]
  422 invalid_request          extra.owner (an owner run on an object whose owner isn't settable, D6)
```

### Revocation and its side effects

- **Metadata (T3):** `revoke_unqualified_logins` runs inside every writer named in P3. The explicit writes (`suspend_member`, `revoke_postgres_access`, `revoke_logins_for_zitadel_user`, `remove_member`) revoke every live login of the member; `suspend_member` and `revoke_postgres_access` also mark the member's `member_branch_roles` rows for rotation (S2).
- **Orchestrator (T8):**
  - `pub(crate) async fn access_changed_for(state, tenant: TenantId, members: &[(Uuid, String)], projects: Vec<Uuid>)`. The members are `(member id, Zitadel sub)` pairs. It does three things, in this order:
    1. `access_changed` per member (`members.rs:937-941`);
    2. `invalidate_member_logins` per member: `member_login_endpoints` → `publish_password_updated`, grouped by endpoint;
    3. `reconcile_projects(projects)` (`members.rs:943-964`).
  - Every grant, override, tenant-admin, remove, suspend, reinstate and revoke-now handler calls it. Suspend and revoke-now pass the touched projects their metadata write returns, so every branch of every project the member reaches is reconciled and the rotation (S2) happens there. Step 2's team writes will call the same helper (MH-5).
  - `crate::internal::ddl::publish_password_updated(cache: Arc<dyn CachePort>, endpoint_id: String, roles: Vec<String>)` is today's private `publish` (`internal/ddl.rs:1201-1225`), renamed and made `pub(crate)`. It stays `tokio::spawn`ed (two rounds, 1 s apart), so tests wait on the fake `CachePort`'s record (a `Notify` or channel), never on a sleep (S19).
  - `main.rs` builds the `ProxyCache` inline inside `InternalState` (`services/orchestrator/src/main.rs:95-97`); T8 hoists it to one `Arc` given to both states.
- **Webhook (T8):** `user.deactivated`, `user.locked` and `user.removed` for a tenant org (`chelabase-zitadel/src/actions.rs:84-93`: `Gone | Deactivated`) → `revoke_logins_for_zitadel_user` (on the internal `NeonMetadataPort`, `internal/mod.rs:46`) → publish for each endpoint → `ResyncPort::reconcile_member_roles(branch)` per branch (`internal/ddl.rs:61-86`) → then today's realtime access event (P17). Suspend's own Zitadel lock fires `user.locked` back at us: the member then has no live login, so the write revokes nothing and writes no event (S18).
- **Sweep (T7):** `Worker::sweep_identities_once() -> (checked, revoked)`. For each `members_with_live_logins` row, `Zitadel::get_user`: `None`, `Inactive` or `Locked` → `revoke_logins_for_zitadel_user` → `CacheNotifier::password_updated` + `password_updated_later` → `MemberRoleRequests::request(branch)`. The loop runs every `CHELABASE_IDENTITY_SWEEP_SECS` (default 600; `0 < s ≤ 86400`). Without Zitadel (`config.rs:338-358`) it logs once and skips.

### Worker (T6)

- `MemberCatalog` gains `marked: BTreeSet<String>` and `personal: BTreeMap<String, ActualMemberRole>` (`member_roles.rs:549-588`). `member_catalog_sql()` reads the marker with the round-4 predicate.
- `DesiredMemberRole` gains `kind: RoleKind`, with `pub enum RoleKind { Member, Personal { valid_until: DateTime<Utc> } }`.
- `MemberRoleStep` gains:
  - `CreatePersonal { role, verifier, valid_until, lock }`: one transaction, as `cloud_admin`. It renders `CREATE ROLE … LOGIN NOINHERIT NOCREATEDB NOCREATEROLE NOBYPASSRLS NOREPLICATION VALID UNTIL '<ts>' PASSWORD '<verifier>'`, then `mark_personal_login_sql`, then (when `lock`) `GRANT chelabase_schema_locked TO <role>`.
  - `SetValidUntil { role, valid_until }`.
- `plan_member_roles` (`member_roles.rs:236`) plans personal logins with the same revoke → create/password/lock → grant → drop order. A personal `Drop` runs the K6-H5 order: lock-out, sessions ended, `REASSIGN OWNED`/`DROP OWNED` in every database, `DROP ROLE`.
- `WorkerMetadata` (`ports.rs`) gains `personal_login_records`, `upsert_personal_role_row` and `delete_personal_role_row` (T3 signatures).
- `plan_key_roles(keys, catalog, timeline, marked: &BTreeSet<String>)` (`resync.rs:123`) revokes any live key bound to a marked name.
- **S9, member-filtered runs.** A reconcile with `member_id: Some(m)` (`MemberRolesReconcileRequest`; T8's create asks for one on the main branch) plans `m`'s personal logins too: their desired and actual sets are restricted to `m`'s records and the marked roles named by them. A filtered run never drops an orphaned marked role (it can't be attributed); the unfiltered runs and the sweep do.
- **S16:** every personal row the worker upserts or deletes gets `CacheNotifier::password_updated` + `password_updated_later` (`ports.rs:1704-1732`), as the resync does for its rows (`resync.rs:1325`). Memberships use `Effective::roles_at_least(Developer)`, with a tenant admin's from `tenant_admin_effective()` (the reconcile reads tenant admins from the member list, `member_roles.rs:1269-1285`, not from `database_grant_sources`, which holds only grant and override rows, `members.rs:1228-1265`).
- **S1:** `child_role_copies` (`branch_create.rs:351`) leaves out `origin = personal` rows; the child's personal rows come from its reconcile.

### Studio (S1, consumed by S2–S4)

- Search params, each parsed against its enum (an unknown value is dropped):
  - `/app/members`: `tab: "people"|"roles"|"activity"|"instance"`, `project: uuid`, `view: "cards"|"list"`, `q: string`; on the Roles tab also `database: uuid`, `branch: uuid`, `pg_database: string`.
  - `/app/members/$memberId` (file `app.members_.$memberId.tsx`).
  - `/app/members/roles/$databaseId/$branchId/$role` (`app.members_.roles.$databaseId.$branchId.$role.tsx`), with `?pg_database=`.
- `components/members/hub/` exports:
  - `RoleChecklist({ choices, checked, locked?, onChange })`;
  - `GrantCard({ projectId, member, grant, roleSources, onSaved })`, which sends the own grant only (MH-38a);
  - `LevelBadge({ level })`;
  - `ViewToggle({ value, onChange })`;
  - `useViewPreference(scope: "roster" | "roles")`: localStorage key `chelabase.members.view.<scope>`, every access in try/catch, falling back to `"list"`;
  - `groupRoster(members, { projectId? }) -> Group[]`, pure, in the order of MH-35: Tenant admins, Project admins, Developers, Viewers, Other projects, No access yet, Invited, Suspended; empty groups hidden. S17: status decides first (`invited` → Invited, `suspended` → Suspended), then `access.level`; `pending` members are not grouped, they stay in the Requests block above the roster (spec Part 2 "Groups"; `people-tab.tsx:70-77`, `:285`);
  - `escapeName(name)`: non-ASCII and control characters shown escaped (MH-38d).
- `roleChoices` skips `origin === "personal"` (`components/members/role-picker.tsx:65`).
- S17: `lib/members.ts` (task S1) gains a sentence for each new action (`eventSentence`'s `switch`, `:94-103`), `suspended` in `STATUS_WORDS` (`:172`) and in `memberStatus` (`:45`); `$memberId` is parsed as a UUID (not a UUID → the not-found state).
- Generated client functions (S14): task S2 calls `listMemberPersonalLogins`, `createPersonalLogin`, `resetPersonalLoginPassword` (also "Renew"), `deletePersonalLogin`, `suspendMember`, `reinstateMember`, `revokeMemberPostgresAccess`, `listAccessEvents`/`listProjectAccessEvents` with `member_id`; task S3 calls `getPrivileges`, `applySchemaChanges`, `listDatabasePersonalLogins` (Admins only), `listMembers` (names for the logins list).

## Pre-flight scan (2026-10-23)

**Files per wave (disjoint lanes):**
- W1: T2 has types `access.rs`, `api_error.rs`; metadata `members.rs` (reads only); orchestrator `access.rs`, `members.rs` (effective callers), `lib.rs`, `projects.rs`, `internal/realtime.rs`, `console.rs`, `sql_editor/*`, `data_keys.rs`, `table_api/*` (callers of `grant.roles`); worker `member_roles.rs`, `ports.rs`, `fakes.rs`, `rt_events/delegate.rs`; and their tests. T4 has types `table_api.rs`, schema `ddl/*`, catalog SQL, new `privileges.rs`. T4 doesn't touch the orchestrator: P21 means it neither regenerates nor runs orchestrator tests, and T2 never edits `table_api.rs`. T1 adds one new worker test file. Disjoint.
- W2: T3 has the metadata crate, its migration and tests, plus orchestrator `access.rs` (`member_facts`), `lib.rs` (`MemberStatusView`, `ensure` path) and `openapi.rs`. D writes only `docs/superpowers/designs/2026-10-23-members-hub/` and `apps/studio/.impeccable/surfaces/*members*`.
- W3: T6 is worker-only. T9 has orchestrator `members.rs`, `roles.rs`, `lib.rs`, `openapi.rs`, routes, tests. Disjoint.
- W4: T5 has orchestrator `table_api/*`, new `privileges.rs`, routes, `openapi.rs`, tests. S1 is `apps/studio` only. Disjoint.
- W5: T8 is orchestrator, `chelabase-zitadel`, `scripts/`, `justfile`. T7 has worker `identity_sweep.rs`, `main.rs`, `config.rs`, `ports.rs`, `fakes.rs`, `lib.rs`. Disjoint.
- W6: S2 has `components/members/member-page/*`. S3 has `components/members/roles/*`. Disjoint (routes are S1's, P20).

**Match sites a new variant breaks:**
- `Operation` (T4): `crates/chelabase-schema/src/ddl/mod.rs:131-175, :231, :270-296, :365-393, :425-480`, `ddl/render.rs`, `ddl/dependents.rs`. No match in services (`realtime/src/lease.rs` is a NATS `Operation`).
- `MemberStatus::Suspended` (T3): `services/orchestrator/src/access.rs:196-205`, `lib.rs:1765` (returned as is, like `Removed`/`Pending`), `lib.rs:2186-2193` (`MemberStatusView`). Status filters `== Active` need no change (`internal/realtime.rs:280, :397`, `worker/src/member_roles.rs:1271`).
- `MemberFacts`/`Deny` (T2): `services/orchestrator/src/access.rs:146-169` (`deny_to_api`), `:193-206`, `:242-265`.
- `RoleSet::All` → `All { plus }` (T2): 29 non-test sites in `crates/chelabase-types/src/access.rs`, `crates/chelabase-metadata/src/members.rs`, `services/orchestrator/src/lib.rs`, `table_api/read.rs`, `services/worker/src/member_roles.rs`.
- `AccessAction::ALL` (T3): `crates/chelabase-metadata/src/members.rs:277-296`.
- `ErrorCode::ALL` (T2): `crates/chelabase-types/src/api_error.rs:142`, `:522`, `:546`.

**Existing helpers to reuse:**
- `credentials::generate_password` and SCRAM `chelabase_neon::scram::verifier` (as `member_roles.rs:1329-1341`).
- `mark_personal_login_sql`/`is_marked_sql` (`role_name.rs:263-286`).
- `cleanup_script`/`drop_role_sql`/`terminate_sql` (`member_roles.rs:399-468`).
- `safe_roles`/`granted_roles` (`:913-969`).
- `ProxyCache::password_updated` (P4).
- `NewEvent` (`members.rs:600-672`, behind `record_access_event`).
- `Notices` (`services/orchestrator/src/table_api/changes.rs:163-220`, which keeps each notice's `DbError`; `ddl/notices.rs` only parses cascade notices). SQLSTATE `01007` "privilege not granted" and `01006` "privilege not revoked" are MH-24 failures, matched by `DbError::code()`, never by message text (S19).
- `ScratchCompute` (`services/orchestrator/tests/api.rs:4828`), `owner_console_app` (`:4919`) and `changes_app` (`:12335`) for T5's live route tests.
- The worker live `Fx` (`services/worker/tests/member_roles_live.rs:67-520`) for T1/T6.
- `Zitadel::get_user` (`chelabase-zitadel/src/client.rs:483-492`) and its `mock` feature.
- `CopyButton` (`apps/studio/src/components/copy-button.tsx`).

**Traps:**
- Declared-but-unrouted endpoints fail three tests: `every_declared_orchestrator_endpoint_is_routed` (`services/orchestrator/tests/api.rs:919`), `document_matches_the_registry` (`src/openapi.rs:927`) and `access_walk.rs:51`. So each task declares only the endpoints it routes, in the same commit. Two more tests list every endpoint: `spec_table` (`crates/chelabase-routes/src/lib.rs:1749`) and `operation_ids_are_the_contract` (`src/openapi.rs:962`) (S14).
- Fakes implement every port: orchestrator `tests/common/fakes.rs` (FakeMetadata, `AppState` at `:2501`; it has no fake `CachePort` yet, T8 adds one), `src/openapi.rs:600-740` (the document's own stub), `tests/internal.rs:613` (NeonMetadataPort fake), worker `src/fakes.rs`, and `rt_events/delegate.rs` (a delegating impl).
- **Port ownership (S13):** the task that first calls a metadata function adds it to its port (trait, `MetadataClient` impl, `openapi.rs` stub, fakes): T9 adds `record_access_event` and `member_project_roles` to `MembersPort` (`services/orchestrator/src/members.rs:57`), and T5 reuses `record_access_event`; T5 adds `is_personal_login_name`; T8 adds the personal-login, suspend, reinstate, revoke-now and `personal_login_names` methods to `MembersPort` and `revoke_logins_for_zitadel_user` to `NeonMetadataPort` (`internal/mod.rs:46`); T6 and T7 add theirs to `WorkerMetadata`, `fakes.rs` and `rt_events/delegate.rs`. `MetadataPort` already requires `MembersPort` (`lib.rs:122`).
- The marker guard refuses non-superuser changes to marked roles, so every personal-login statement runs through `admin_sql` (`cloud_admin`, forwarding off, `crates/chelabase-neon/src/backend.rs:329-335`).
- `DROP OWNED BY` or `REVOKE … CASCADE` run by a role recorded as grantor can strip a marked role's memberships (round 4 K6 H2). Grant personal memberships as the superuser only.
- A protected parent's child gets fresh passwords on every role with a password (`resync.rs:956-1014`, `FreshPasswords`). Marked roles must be skipped there, and the reconciler sets them back to the record's verifier (D2).
- Today the resync recreates and records personal logins (`resync.rs:816-871`, `:907-946`). T6 removes this deliberately. Existing tests that encode it change deliberately: `resync.rs:3307-3410`, `src/c3_tests.rs`, `tests/compute_fork_live.rs:735` (`a_recreated_personal_login_is_marked_again`), and `crates/chelabase-metadata/tests/live_db.rs:3001` (`a_resync_never_removes_a_personal_row`). So does `services/orchestrator/tests/api.rs:2790` (`list_roles_shows_a_personal_login_with_its_origin`, T8: now hidden).
- `access_events` is append-only (triggers, `20261014000001_members.sql:52-60`). The migration only adds a column and indexes.
- `ensure_member` must not reactivate a suspended member: the orchestrator's `ensure` (`lib.rs:1763-1766`) and both matches of the metadata `ensure_member` (`crates/chelabase-metadata/src/members.rs:1057`, `:1084`) (S6).
- The gateway routes by `chelabase-routes::ENDPOINTS`, so new routes need the gateway rebuilt: the end-of-step restart includes it.
- Every Base UI modal hides the page behind it from role queries (preamble, Studio tests).

## Seam pass (2026-10-23)

One opus agent read every pinned interface from both sides against the code at `3c1af85d`. Each finding below is applied to "Shared interfaces" and the task sections (rewritten in place); this list is the record. Severity: 1 critical, 10 important, 9 minor, 1 USER DECISION. Findings are S1–S21; the Studio tasks are written "task S1"–"task S4" where both could be meant, and a bare "(Sn)" in a task section is a finding.

- **S1 (critical) Personal `neon_roles` rows vs every generic writer (T3 ↔ T6, T3 ↔ the existing resync and branch copy).** The migration's CHECK `neon_roles_personal_member` and FK fail any personal row written without its record. Today `apply_resync`'s upsert flips `personal` → `sql` and writes the catalog verifier back (`crates/chelabase-metadata/src/neon.rs:1774-1830`): the flip violates the CHECK and fails the whole resync transaction of that endpoint, and the verifier write is MH-9's write-back. The branch copy inserts every parent row with its origin (`branches.rs:1001-1025`, fed by `services/worker/src/branch_create.rs:294-306`): a parent with a personal login makes every child branch's creation fail. **Ruling:** only T3's personal functions write personal rows; `apply_resync`, the branch copy and every by-name writer leave them alone (list in "Shared interfaces → Metadata"); T6 filters them out of `child_role_copies`. Briefs: T3 (files `branches.rs`, `ddl.rs`, `neon.rs`; RED `no_generic_writer_touches_a_personal_row`, `a_branch_copy_skips_personal_rows`), T6.
- **S2 (important) P16 (b) had no wiring.** T8's behaviour and RED list never mentioned the rotation, and T3's revoke write returned endpoints, not the projects T8 must reconcile. The reconcile already rotates a role whose row "won't open" and ends its sessions (`services/worker/src/member_roles.rs:1296-1342`, `:264`); a `DELETE` of the row would also lose R31's `last_acted_role` (`members.rs:2588-2621`). **Ruling:** `revoke_personal_logins` is replaced by `revoke_postgres_access(actor, member) -> Vec<Uuid>` (touched projects); it and `suspend_member` (now also `-> Vec<Uuid>`) set `member_branch_roles.password_enc = ''` for the member in their transaction; T8 passes the projects to `access_changed_for`. Pinned interface changed (T3, T8). Tests: T3 `revoke_now_and_suspend_mark_member_roles_for_rotation`; T8 `revoke_now_revokes_logins_and_reconciles_every_reached_project`; the end-of-step live check stays (P16).
- **S3 (important) `acl_hash` can't be computed from the page.** P10 hashes the object's whole ACL, but the read returns only `?role=`'s entries, so `acl_hash(entries)` (T4) would hash a subset and miss another grantee's change; a relation's column ACLs weren't covered; and `verify_privileges` took one page while ops can span grantees and schemas, and pages are capped. **Ruling:** the hash is computed in SQL over the whole ACL (columns included); T4 adds `acl_hashes_sql()` for T5's in-transaction check; `verify_privileges` takes `&[PrivilegesPage]`, read to the last page per touched (grantee, schema). Pinned interface changed (T4, T5).
- **S4 (important) T1's outcomes had no landing place in T4 (W1, parallel with T1).** `owner_settable` and `run_as` live in T4's SQL and renderer, before T1 reports. **Ruling:** T4 implements the predicate written in the read shape (direct ADMIN or SET) and `run_as` for owner runs only (table `Grant`/`Revoke`, new ops, `SetAllRows`; `AlterDefaultPrivileges` runs as `for_role`); T1's report table (T1 section) says, per item, what T5 changes or when the coordinator stops; `privileges_sql.sql` joins T5's files for that case.
- **S5 (important) MH-28's allow-list skipped the existing table ops.** The privilege editor's table and column privileges are the existing `Grant`/`Revoke` ops (spec Part 4; no other op exists), which have no grantee check (`crates/chelabase-schema/src/ddl/render.rs:304-310`, `:785`). **Ruling:** `privilege_grantee` applies to them and to `SetAllRows.role`; refusals are 400 `invalid_request` with `op_index` like every `DdlError` (`changes.rs:86-88`); owner-not-settable stays 422. Cost if wrong: an API caller that granted to PUBLIC, `authenticator` or the owner through the table op gets 400 (Studio sends none: its table editor only displays grants, `structure-view.tsx:293-329`). Briefs: T4, T5.
- **S6 (important) A suspended member could be activated by `ensure_member`.** Its second match's catch-all arm treats any other status as invited and activates the member when an open invite exists (`members.rs:1084-1100`); the plan named only the orchestrator's check. **Ruling:** both metadata matches return a suspended member as is. Brief: T3 (RED `a_suspended_member_is_never_reactivated_by_sign_in`).
- **S7 (important) Suspend's "retry is idempotent" contradicted its 409.** After a 503 (Zitadel lock failed, Chelabase side done), the retry hit "not active → 409", so the lock could never be completed; same for reinstate's unlock. **Ruling:** suspend of a suspended member and reinstate of an active one answer 200 and redo only the Zitadel step. Briefs: T3, T8 (RED `a_failed_lock_is_completed_by_the_retry`).
- **S8 (important) P24's route had no test and task S3 didn't depend on it.** `LIST_DATABASE_PERSONAL_LOGINS` is T8's (W5) but task S3 listed only tasks S1/T5, and neither RED list covered it. **Ruling:** T8 RED `database_personal_logins_are_project_admin_only`; task S3 RED `personal_login_count_is_admin_only_and_links_to_member_pages`; task S3 depends on T8 (already true by wave). The list returns every record; task S3 counts `active` and names members from `listMembers`.
- **S9 (important) Create's synchronous reconcile vs the member filter.** T8 asks for a member-filtered run (`member_id: Some`), whose actual set today is filtered to the member role (`member_roles.rs:1361-1367`); nothing said personal logins are planned there, nor how T8 learns `ready_on_main` (`MemberRolesReply::Done` carries only member roles), and the shared helper `reconcile_member_role` turns a timeout into 503 (`sql_editor/routes.rs:388-411`), contradicting P7's 201. **Ruling:** filtered runs plan that member's personal logins and never drop orphans (T6); T8 calls `state.js.reconcile_member_roles` directly, `ready_on_main` = `Done` and `member_login_endpoints` lists the main branch's endpoint for that role; any other outcome is `false`, never an error. Briefs: T6 (RED `a_member_filtered_run_creates_that_members_login_and_drops_no_orphan`), T8.
- **S10 (important) Re-creating a revoked login under the same name was refused by its own leftovers.** A revoked record's `neon_roles` rows stay (verifier NULL) until the worker drops the role, and the name check counts them, so P6's replace answered 409 `role_exists`. **Ruling:** the check ignores the replaced record's own rows; replace = DELETE + INSERT (the FK has no ON UPDATE). Brief: T3 (RED extends `create_replaces_a_revoked_record_and_refuses_a_live_one` with the same name).
- **S11 (important) D3 holds for the API's own role names too.** `CREATE_ROLE` and rename take any free name on the branch, so an API role made where the login isn't yet (a child before its reconcile) blocks it forever (the reconciler never adopts an unmarked role). **Ruling:** T8's create and rename role routes refuse a personal-login name of the database with 409 `role_exists`. Brief: T8 (RED `create_role_refuses_a_personal_login_name`).
- **S12 (minor) `check_names` for a project grant needs every database.** `is_personal_login_name` is per database; a project grant's roles apply to all. **Ruling:** T3 adds `personal_login_names(project)`; T8's `check_names` uses it for grants and `is_personal_login_name` for overrides.
- **S13 (minor) Who adds port methods and enum variants.** T3 changes `access_events`' signature, whose only caller is the orchestrator's `MembersPort` impl (`services/orchestrator/src/members.rs:312-320`, not in T3's files), and `AccessEvent`'s literal in the fakes (`:2785`); `AccessAction` variants written by T9/T5 had no owner. **Ruling:** T3 adds all variants and passes `None` in the impl; port ownership as in the pre-flight traps; T7's files gain `rt_events/delegate.rs`.
- **S14 (minor) `operationId`s weren't pinned** though Studio calls them by name and `operation_ids_are_the_contract` lists them. **Ruling:** the routes table pins them; each routing commit also updates `spec_table`.
- **S15 (minor) `LoginWriteError` had no HTTP mapping.** **Ruling:** in the shapes block.
- **S16 (minor) The proxy cache on the worker's writes; `member_login_endpoints` scope.** Nothing told T6 to clear the cache for rows it writes, and whether `member_login_endpoints` includes revoked records' rows was open. **Ruling:** T6 publishes for each personal row it writes or deletes; `member_login_endpoints` returns every personal row of the member; `delete_personal_role_row` deletes only personal rows.
- **S17 (minor) Studio seams.** `groupRoster` had no rule for `pending` (spec Part 2 keeps them in the Requests block) or for status vs level; `lib/members.ts` renders events by action (`eventSentence`, `:94`) and statuses (`:45`, `:172`) and no task owned the new ones; "Renew" had no route (spec Part 6: a reset renews). **Ruling:** in "Shared interfaces → Studio"; tasks S1 and S2.
- **S18 (minor) The Zitadel mock and our own lock's webhook.** The mock has `/deactivate` and `/reactivate` only (`crates/chelabase-zitadel/src/mock.rs:139-140`), so T8's `lock_and_unlock_are_idempotent` needs `/lock` and `/unlock` there (T8 files gain `mock.rs`); suspend's lock fires `user.locked` back, and a 0-count revocation writes no event.
- **S19 (minor) Citations and test hazards.** Fakes' `AppState` is at `tests/common/fakes.rs:2501`; `ScratchCompute`/`owner_console_app`/`changes_app` at `api.rs:4828`/`:4919`/`:12335`; `Notices` is in `table_api/changes.rs:163-220` and matches by SQLSTATE; `publish` is spawned, so T8's cache tests wait on the fake; the internal `ProxyCache` is built inline (`main.rs:95-97`) and is hoisted.
- **S20 (minor) T2/T3 notes.** `RoleSet::All { plus }`: an override intersects to `Only` (`access.rs:104-112`, D1), so in step 1 `plus` is always empty and `/me` keeps `"all"` (`lib.rs:2223-2235`); T2's exhaustive test asserts it. T3's `the_level_function_agrees_with_qualifies_exhaustively` feeds `tenant_admin_effective()` for tenant admins (as `authorize` does, `access.rs:316-322`) and adds a deleted-database case where SQL answers false whatever the grant.
- **S21 (user, 2026-10-23: refuse) the existing table `Grant`'s `with_grant_option`:** `Grant` and `CreateTable.grants` with `with_grant_option: true` answer 400 `invalid_request`; on `Revoke` it keeps meaning "revoke the grant option only". T4 adds it with tests. MH-29 removes grant options from the new ops, but `Operation::Grant { grant: TableGrant { with_grant_option } }` (`crates/chelabase-types/src/table_api.rs:328-337`) still grants `WITH GRANT OPTION` on tables and columns, which is MH-29's attack (a group holding a grant option lets every Developer acting as it re-grant to `anon`, with a grantor the owner's REVOKE can't remove). Studio never sends it (no `with_grant_option` in `apps/studio/src`), and merges don't use the op (`render.rs:304` is its only renderer).
  - Applied to T4's brief (T5 only maps the error). This closes MH-29 for tables too.

---

## T1: D6 feasibility probe (opus; no review of its own)

**Worktree:** `.worktrees/members-hub-t1` from `members-hub`.

**Files:** new `services/worker/tests/d6_feasibility_live.rs`; new `docs/superpowers/notes/2026-10-23-members-hub-d6-feasibility.md` (report).

**Behaviour:** on a fresh database of the plan's stack (the worker live harness, `common::setup`), establish with assertions:
1. API-made roles (made through the worker's `create_role` path, `SET ROLE owner; CREATE ROLE`, `resync.rs:1405-1422`): the owner holds ADMIN, `INHERIT FALSE, SET FALSE`. Can the owner `GRANT r TO <owner> WITH INHERIT FALSE, SET TRUE` itself, then `SET LOCAL ROLE r`, inside a transaction, and `REVOKE SET OPTION FOR r FROM <owner>` there (P12a)? Can `cloud_admin` grant it persistently (P12b)?
2. Under `SET ROLE r` from the owner session, `GRANT SELECT ON <r's table>` and `CREATE POLICY`/`COMMENT ON POLICY` on r's table work. The grantor recorded is r.
3. The protected-branch guard (PB, `schema_guard.rs`):
   - an owner session running (1)–(2) on a protected branch is allowed;
   - a locked member role (`cb_m_*` holding `chelabase_schema_locked`) that `SET ROLE`s into r and runs a `GRANT` is refused.
4. `ALTER DEFAULT PRIVILEGES FOR ROLE r IN SCHEMA s GRANT …` from the owner: is it refused without membership, and does it work with SET, or does it need INHERIT? Record the exact requirement on PG17.
5. Member-role safety walk: `member_catalog_sql` + `safe_roles` on the branch where the owner is a member of group `g` (SET TRUE). Is `g` still granted to members, i.e. does `unsafe_node` ignore the owner's own membership in `g`? Run both P12a (transient, nothing left) and P12b (persistent).
6. Roles made in SQL:
   - by the owner: the owner has ADMIN through `createrole_self_grant`'s default; settable?
   - by another CREATEROLE role, and by `cloud_admin`: the owner has no ADMIN, so not settable, and the UI greys them.
7. The owner's catalog session reads `pg_db_role_setting` (the marker predicate, MH-13 checklist).
8. A `REVOKE` the owner can't fully perform raises SQLSTATE `01006` as a notice (not an error), and a grant `01007`. Record the exact notice text for the verify (MH-24).

**Report:** for each item, the observed behaviour, the PG17 version, and the ruling it implies for T4/T5 (P12's choice; `owner_settable`'s exact predicate; the `FOR ROLE` membership requirement). It also notes whether spec Part 4's D6 is feasible as written. If an item rules out D6 entirely, the coordinator stops and asks the user before T5.

**Outcomes and where each lands (S4).** T4 (W1, in parallel) implements the expected column; T5 absorbs a different answer, or the coordinator stops:

| Item | Expected (T4 builds on it) | If different |
|---|---|---|
| 1 (a) | the owner can grant itself SET on r, `SET LOCAL ROLE r`, and revoke the option, in one transaction | (b) works instead: **stop, ask the user** (P12 was decided as (a); (b) puts the resync invariant and `create_role_sql` into T6's brief, and T5 drops the transient grant). Neither works: D6 is out, stop. |
| 2 | grants and policies work under `SET ROLE r`; the grantor recorded is r | another grantor: T5's verify expects the observed grantor (verify is per grantor, MH-24) |
| 3 | owner session allowed on a protected branch; a locked member role under `SET ROLE` refused | owner refused: owner runs are impossible on protected branches, **stop**. Locked role allowed: a guard gap outside this plan, **stop** |
| 4 | `ALTER DEFAULT PRIVILEGES FOR ROLE r` works with SET (T4 runs it as `for_role`) | needs INHERIT: T5's transient grant uses `INHERIT TRUE` for those statements only |
| 5 | (a) leaves no membership, so the safety walk is untouched | under (a) nothing to do; recorded for (b) only |
| 6 | owner-made SQL roles: ADMIN via PG16+'s implicit grant, so settable; others not | T5 changes `owner_settable` in `privileges_sql.sql` to the observed predicate |
| 7 | the owner reads `pg_db_role_setting` | unreadable: T5 passes the database's marked names (`personal_login_records`) as a parameter to the catalog and privileges reads, T4's SQL takes it (T5 edits it) |
| 8 | `01006`/`01007` arrive as notices | recorded; T5 matches the SQLSTATE, never the text |

**RED tests:** none (a probe). The committed test file asserts the observed facts, so the end-of-step live pass re-checks them.

**Gotchas:**
- `common::setup` builds an in-process `RealWorker` against the stack. The probe uses only its `admin_sql`/owner connections, never its reconcile, so the stack's own worker can't race it (it only sees an unprotected-then-protected database it made).
- Protect the branch through the metadata as `member_roles_live` does.

**Verify:** `cargo test -p worker --test d6_feasibility_live --no-run`.

**Live runs:** `just --justfile <wt>/justfile --working-directory <wt> workbench test -p worker --test d6_feasibility_live -- --run-ignored only`, once, in W1. It runs alone on the stack (T2/T4 don't touch the stack) and uses its own database.

---

## T2: effective-access types and the one read (opus / opus / opus)

**Worktree:** `.worktrees/members-hub-t2`.

**Files:**
- `crates/chelabase-types/src/access.rs`, `api_error.rs`, `tests/project_admin_all.rs`, new `tests/effective_sources.rs`;
- `crates/chelabase-metadata/src/members.rs` (`grant_sources`, `database_grant_sources`, `GrantRow::to_grant`);
- orchestrator: `access.rs`, `members.rs` (the `effective`/`Authorized.grant` readers), `lib.rs` (`/me`, `MetadataPort`), `projects.rs`, `internal/realtime.rs`, `console.rs`, `sql_editor/*`, `data_keys.rs`, `merges.rs`, `branches.rs`, `protection.rs`, `data_api_databases.rs`, `table_api/{read,rows}.rs`, `openapi.rs` (stub port), `tests/common/fakes.rs`, `tests/role_rules_agree.rs`;
- worker: `member_roles.rs`, `member_roles/tests.rs`, `ports.rs`, `fakes.rs`, `rt_events/delegate.rs`, `rt1_tests/exclusions.rs`;
- `openapi/management.json` and generated clients (P21).

**Behaviour:** spec Part 5 "Effective access" and "Types" (MH-1–3, D1) as pinned in "Shared interfaces"; `qualifies` (MH-17); `Deny::Suspended`. Behaviour-preserving for today's single own grant, except that `decide` now checks the requested role's own level (equal to the platform level with one source). The worker grants member roles from `Effective::roles` as before. Personal logins (T6) will use `roles_at_least(Developer)`.

**Checklist (red-team "Teams / access union", the step-1 part):**
- an empty source list is `None`, never a default;
- per-role levels; the exhaustive test asserts "per role, never above the best source naming it, nor the override's cap";
- `All` ∪ `Only(S)` keeps S's `anon`/`authenticated`;
- `effective`'s every-role test accepts `All { plus }`; in step 1 `plus` is always empty (an override intersects to `Only`) and `/me` keeps `"all"` (S20);
- no public own-grant field: `GrantSources::effective()` is the only way to a grant, and every reader in MH-2's list is switched (admission `access.rs:247-253` stays on `member_projects`, which lists own grants, unchanged in step 1; its team half is step 2's);
- a tenant admin stays `tenant_admin_effective()` and is never narrowed.

**RED tests (test agent):**
- `crates/chelabase-types/tests/effective_sources.rs`:
  - `effective_matches_a_model_exhaustively`: every own ∈ {none, V/D/A × subsets of {a, b, anon}}, teams ∈ {[], [one such grant], [two]}, override ∈ {none, level × subset}, compared with a reference model written in the test. First failure: `GrantSources` unresolved (the item under test).
  - `union_is_commutative_and_idempotent_over_teams`.
  - `a_roles_level_never_exceeds_its_best_source_or_the_cap`.
  - `admin_from_any_source_is_all_with_named_data_api_roles`: `may_act_as("anon")` true only when a source named it.
  - `no_source_is_none`.
  - `qualifies_needs_active_and_developer`: exhaustive over status × level.
- `crates/chelabase-types/src/access.rs` tests:
  - `decide_checks_the_requested_roles_own_level`: an `Effective` at Developer whose role `x` is held at Viewer; `ActAs(Developer)` as `x` → `Forbidden{"role:x"}`; first failure: compile, `decide` takes `&Grant`.
  - `a_suspended_member_is_refused_member_suspended`.
- `api_error.rs`: `every_code_serialises_snake_case` and the `ALL` listing gain the five codes; first failure: unknown variant.
- `services/orchestrator/tests/api.rs`: `act_as_uses_the_roles_level_not_the_members` with a fake whose `grant_sources` gives a Developer own grant. First failure: compile (`grant_sources` missing on the fake port).
- `services/worker/src/member_roles/tests.rs`: `reconcile_reads_grant_sources`. The fake's `database_grant_sources` gives a member a grant; the desired set is unchanged from today's. First failure: compile.

**Gotchas:**
- `GrantRow::to_grant` (`members.rs:150-158`) builds `RoleSet::All` for Admin rows.
- `tenant_admin_grant` callers (`grep`) switch to `tenant_admin_effective`.
- `role_rules_agree.rs` compares Studio's TS rules with Rust: keep it green.

**Verify:**
- `cargo test -p chelabase-types -p chelabase-metadata --lib`;
- `cargo test -p chelabase-types --test effective_sources --test project_admin_all`;
- `cargo test -p orchestrator`, `cargo test -p worker --lib`;
- `cargo clippy -p chelabase-types -p chelabase-metadata -p orchestrator -p worker --all-targets -- -D warnings`;
- `just api-gen` last (P21);
- live targets `--no-run`: `cargo test -p chelabase-metadata --test members_live --no-run`.

**Live runs:** none of its own (scratch-DB reads keep their shape; the end-of-step pass runs `members_live`).

---

## T4: schema crate privilege ops and the privileges read (opus / opus / opus)

**Worktree:** `.worktrees/members-hub-t4`.

**Files:**
- `crates/chelabase-types/src/table_api.rs` (ops, `PrivilegeTarget`, `ExpectedAcl`, `SchemaChangeRequest.expect_acl`);
- `crates/chelabase-schema/src/ddl/{mod.rs,render.rs,dependents.rs}`;
- new `crates/chelabase-schema/src/privileges.rs` and `src/privileges_sql.sql`;
- `src/catalog_sql.sql`, `src/catalog_parts/head.sql`, `src/lib.rs`;
- tests: new `tests/ddl_privileges.rs`, new `tests/privileges_live.rs`, `tests/catalog_live.rs` (marked roles).

**Behaviour:** spec Part 4 "Backend: new schema-change operations", "a privileges read", "After the red-team" (MH-24, 26–29, 31–33, 13), as pinned.

**Checklist (red-team "Privilege ops", schema part):**
- the catalog `reserved`/`hidden` CTEs exclude marked roles, with merge-plan tests: a personal login on both sides, on one side, and a same-name group (`plan/diff/mod.rs:1237-1270` never pairs a marked role);
- no grant option on the new ops (the table `Grant` refuses it too, S21);
- functions by OID → server `regprocedure` text only;
- grantee and `FOR ROLE` allow-lists (`privilege_grantee`), on the existing table `Grant`/`Revoke` and `SetAllRows.role` too (S5);
- `run_as` only in owner runs, per object owner; `AlterDefaultPrivileges` as `for_role` (S4);
- `acl_hash` in SQL over the whole ACL, columns included; `acl_hashes_sql` with the same expression (S3);
- `owner_settable` in SQL, direct ADMIN or SET (S4);
- `verify_privileges` with grantors, over several pages (S3);
- all-rows identified by shape (permissive, `FOR ALL`, roles `{role}`, `USING (true)`, `WITH CHECK (true)`) plus the comment marker; off drops only that shape;
- exposure chains through any membership option;
- `Maintain` by server version;
- rendering tests with hostile identifiers, each statement exactly one statement (`pg_query`, as `tests/ddl_objects.rs:300-310`).

**RED tests (test agent):**
- `tests/ddl_privileges.rs`. First failure for each: compile (the ops missing).
  - `schema_ops_render_quoted_one_statement_each`;
  - `revoke_grant_option_only_renders_grant_option_for`;
  - `sequence_ops_render`;
  - `function_ops_render_only_the_resolved_identity` (missing OID → error);
  - `default_privileges_render_for_role_in_schema`;
  - `public_is_refused_as_a_grantee_except_in_revokes`;
  - `reserved_owner_and_authenticator_are_refused_as_grantees`;
  - `maintain_is_refused_below_pg17`;
  - `set_all_rows_renders_the_marked_permissive_policy`;
  - `hostile_identifiers_render_one_statement_each` (proptest over names with `"`, `;`, `--`, newlines and bidi characters);
  - `run_as_is_set_when_the_object_owner_is_not_the_session_role` (table `Grant`, a new op, `SetAllRows`; none in a role run);
  - `default_privileges_for_another_role_run_as_that_role` (S4);
  - `table_grant_and_revoke_check_the_grantee` (S5: owner, `authenticator`, a reserved name, PUBLIC in a grant refused; PUBLIC in a revoke allowed).
- `src/privileges.rs` unit tests:
  - `verify_reads_several_pages` (an op set spanning two schemas and a default privilege, S3);
  - `verify_flags_a_revoke_another_grantor_still_holds`;
  - `verify_flags_a_grant_that_did_not_land`;
  - `exposure_finds_anon_through_any_membership_chain`;
  - `exposure_finds_a_key_role`;
  - `all_rows_policy_name_fits_63_bytes_and_avoids_clashes`.
- `tests/privileges_live.rs` (scratch DB on the metadata Postgres, like `catalog_live.rs`):
  - `read_lists_one_roles_entries_with_every_grantor`;
  - `read_pages_per_schema_and_skips_marked_and_member_roles`;
  - `a_revoke_by_another_grantor_is_seen_by_verify`;
  - `the_hub_policy_is_found_by_shape_and_marker_not_name`;
  - `acl_hash_covers_every_grantee_and_column_and_ignores_order` (S3: another grantee's grant changes it, a column grant changes the relation's, the same ACL built in another order hashes the same; `acl_hashes_sql` agrees with the page);
  - `owner_settable_follows_direct_admin_or_set` (S4).
  - First failure: compile.
- `tests/catalog_live.rs`: `a_marked_role_never_appears_as_role_or_grantee`. First failure: the marked role is listed.

**Gotchas:**
- Existing grant rendering emits one statement per privilege and column (`tests/ddl_objects.rs:108`). Keep that for the new ops, since verify matches per privilege.
- The metadata Postgres is vanilla PG17 (`infra/compose/docker-compose.yml:160`). The marker there is a plain `ALTER ROLE … SET chelabase.managed_role = 'personal'` (no guard), which is enough for read tests.
- `PrivilegeTarget` must be `Ord` (a map key).
- S5 changes existing tests deliberately: `tests/ddl_objects.rs:117` (a table `Grant` to PUBLIC) and `:177` (the lock test's grant to PUBLIC) take a plain role; `:138` keeps its `Empty("privileges")` error (the empty check runs before the grantee check). `:118`'s `with_grant_option: true` waits for S21.

**Verify:**
- `cargo test -p chelabase-schema`, `cargo test -p chelabase-types`;
- `cargo clippy -p chelabase-schema -p chelabase-types --all-targets -- -D warnings`;
- `cargo test -p chelabase-schema --test privileges_live --test catalog_live --no-run`.
- No `api-gen` (P21).

**Live runs:** `workbench test -p chelabase-schema --test privileges_live -- --run-ignored only` and `--test catalog_live -- --run-ignored only a_marked_role_never_appears_as_role_or_grantee`, by the coder once green. Scratch DBs: no conflict with T1.

---

## T3: metadata for personal logins, suspension, revocation and the log (opus / opus / opus)

**Worktree:** `.worktrees/members-hub-t3` from `members-hub` after T2 and T4 merged.

**Files:**
- `crates/chelabase-metadata/migrations/20261023000001_members_hub.sql`;
- `crates/chelabase-metadata/src/{personal_logins.rs (new), members.rs, neon.rs, branches.rs, ddl.rs, lib.rs}` (S1: `branches.rs` and `ddl.rs` for the personal-row guards);
- `crates/chelabase-metadata/tests/{members_live.rs, live_db.rs}`;
- orchestrator `access.rs` (`member_facts`), `lib.rs` (`MemberStatusView::Suspended`, `ensure` path), `members.rs` (only the `access_events` call, `:312-320`, passing `None`, S13), `openapi.rs`, `tests/common/fakes.rs` (`AccessEvent` literal, `:2785`);
- generated files (P21).

**Behaviour:** spec Part 6 "Data", "Rules", "Revocation is immediate", "Suspend and immediate revoke", plus Part 4 MH-37 and Part 3's `member_id` filter, as pinned (P2, P3, P5, P6, P8, P18).

**Checklist (red-team "Personal logins, API side", metadata part, and "Roster / member API", metadata part):**
- one record linking member to role rows, with FKs; CHECK `origin='personal'` ⇒ `password_enc IS NULL`;
- every access-reducing write revokes in the same transaction;
- the proxy answers no secret for a revoked or expired login;
- `qualifies` (SQL) agrees with `access::qualifies`;
- name uniqueness across the database's branches and logins (an exact indexed lookup, D3);
- `access_events.branch_id`, with the append-only triggers unchanged;
- `member_id` matches target or actor;
- `remove_member` touched includes personal-login projects.

Plus:
- a suspended member is never reactivated by `ensure_member` (both matches, `members.rs:1057`, `:1084`, S6);
- `reinstate` doesn't recreate logins;
- the last active tenant admin can't be suspended; suspending a suspended member and reinstating an active one write nothing and succeed (S7);
- the verifier never appears in any event `before`/`after`;
- `upsert_personal_role_row` never writes a verifier for a revoked record (one statement);
- a database deleted (status `deleted`) ⇒ not qualifying;
- no generic `neon_roles` writer touches a personal row (S1, the list in "Shared interfaces → Metadata");
- `suspend_member` and `revoke_postgres_access` set the member's `member_branch_roles.password_enc = ''` (rows kept, `last_acted_role` kept) and return the touched projects (S2);
- the name check ignores the replaced record's own rows; replace is DELETE + INSERT (S10);
- a revocation of 0 logins writes no event (S18).

**RED tests (test agent).** Live on scratch DBs, in `members_live.rs` unless noted; first failure is a compile error (the API is missing) unless noted:
- `the_level_function_agrees_with_qualifies_exhaustively`: every status × tenant_admin × grant level (none/V/D/A) × override level (none/V/D/A), compared with `access::qualifies(GrantSources::effective())` (`tenant_admin_effective()` for a tenant admin), plus a deleted database where SQL answers false (S20).
- `every_access_write_revokes_unqualified_logins` (P3; invariant): a proptest over sequences of every writer (put/delete grant, put/delete override, set/unset tenant admin, remove, suspend, invites with grants, approve). After each step, no live login of a non-qualifying member exists, and its `neon_roles` rows have no verifier.
- `create_refuses_a_name_taken_on_any_branch_or_by_another_login`.
- `create_replaces_a_revoked_record_and_refuses_a_live_one` (the replacement reuses the revoked login's own name while its rows remain, S10).
- `reset_sets_every_proxy_row_to_the_new_verifier`.
- `delete_cascades_the_proxy_rows_and_returns_their_endpoints`.
- `revoked_record_never_gets_a_verifier_back`.
- `suspend_revokes_and_reinstate_keeps_them_revoked`.
- `the_last_active_tenant_admin_cannot_be_suspended`.
- `suspend_and_reinstate_are_idempotent` (S7).
- `revoke_now_and_suspend_mark_member_roles_for_rotation` (S2: rows kept with `password_enc = ''`, `last_acted_role` kept, the touched projects returned).
- `a_suspended_member_is_never_reactivated_by_sign_in` (S6: with an open invite too).
- `personal_login_names_cover_every_database_of_the_project` (S12).
- `events_carry_branch_and_never_a_secret`.
- `member_filter_matches_target_or_actor`.
- `remove_member_touches_personal_login_projects`.
- `zitadel_offboarding_revokes_by_user`.
- `live_db.rs`:
  - `the_proxy_gets_no_secret_for_a_revoked_or_expired_login` (`routable_role_secret`);
  - `a_personal_row_needs_its_record` (FK; the insert fails);
  - `no_generic_writer_touches_a_personal_row` (S1: `apply_resync` with an upsert and a removal of the name, the by-name writers; the row, its origin and its verifier unchanged, and the resync's other rows still written);
  - `a_branch_copy_skips_personal_rows` (S1).
- Changed deliberately: `a_resync_never_removes_a_personal_row` (`live_db.rs:3001`) → personal rows are the reconciler's, gone with their record.
- orchestrator `tests/api.rs`: `a_suspended_caller_gets_member_suspended_and_me_says_so`. First failure: compile (`MemberStatus::Suspended`).

**Gotchas:**
- Lock order is tenant, then member (`members.rs` module docs).
- `revoke_unqualified_logins` reads `chelabase_personal_login_qualifies` after the write in the same transaction, so it sees the new state.
- `neon_roles` rows live per endpoint: join `branches.endpoint_id`.

**Verify:**
- `cargo test -p chelabase-metadata --lib`, `cargo test -p orchestrator`;
- clippy on both;
- `cargo test -p chelabase-metadata --test members_live --test live_db --no-run`;
- `just api-gen` last.

**Live runs:** the new `members_live` and `live_db` tests by name, once green (scratch DBs; no conflict with D).

---

## D: comps with `/impeccable` (opus; the user approves)

**Worktree:** `.worktrees/members-hub-d`.

**Files:**
- `docs/superpowers/designs/2026-10-23-members-hub/`: one static HTML comp per screen built on Studio's tokens (`apps/studio/src/styles.css`, `apps/studio/DESIGN.md`), captures at 1440×900 and 390×844, and `index.html` linking them;
- `apps/studio/.impeccable/surfaces/` briefs for the new surfaces.

**Behaviour:** the agent invokes the `impeccable` skill in Operate mode, inside Studio's Basalt system (no new visual world). Screens (spec Part 7 "Design"):
- the roster in cards and in list, grouped (MH-35 groups), and filtered by a project;
- the member page (project access cards with the role checklist and "also gives", personal logins with expiry and Renew, Suspend/Reinstate/Revoke Postgres access now, activity);
- the Roles tab (picker, role cards and list);
- the role page with the privilege matrix (schema blocks, the column drill-in, all-rows with its warning, sequences, functions, future objects, greyed "owned by X" rows, the review-changes diff with the data API exposure flag, the conflict state);
- the create-role dialog;
- the personal-login reveal dialog ("You won't see this password again");
- the multi-email invite with per-email results.

**Checklist (Studio, MH-38):** names render escaped; locked rows (step 2's "via team") have a visual slot; holders and member counts are shown to Admins only (MH-34).

**Verify:** the coordinator publishes the index for the user (Artifact) and **pauses for the user's approval**. S1 starts only after a yes; requested changes go back to this agent.

---

## T6: worker personal-login convergence (opus / opus / opus)

**Worktree:** `.worktrees/members-hub-t6` from `members-hub` after T3.

**Files:**
- `services/worker/src/`: `member_roles.rs`, `member_roles/tests.rs`, new `personal_logins.rs` (planning and rendering of personal steps), `resync.rs`, `roles.rs` (remove `create_personal_login`, `:532-567`, or route it through the planner), `data_api.rs` (key-role check), `ports.rs`, `fakes.rs`, `rt_events/delegate.rs`, `c3_tests.rs`, and `branch_create.rs` (a member-role reconcile request after the new branch's resync, if absent);
- tests: new `services/worker/tests/personal_logins_live.rs`, `tests/compute_fork_live.rs` (deliberate change at `:735`).

**Behaviour:** spec Part 6 "Convergence", "Memberships", "No privileges of its own", "Hidden elsewhere" (worker part), D2, D4's drop half, R10, MH-10, 12, 16–18, 23.
- The desired set on a branch is the database's records where `qualifies && !revoked && valid_until > now()`.
- The actual set is the catalog's marked roles.
- A marked role without such a record is dropped. Until then, its `neon_roles` row (if any) carries no verifier.
- The record's verifier is set back wherever the catalog differs: copy, restore or protected child.
- Memberships are the safe set of `Effective::roles_at_least(Developer)` (`INHERIT FALSE, SET TRUE`), never the owner or an owner-level role.
- The lock marker goes in the create transaction on a protected branch for a non-Admin, and `Lock` comes before any grant.
- The R31-style cleanup runs before memberships.
- A drop or rotation ends sessions.
- The resync never records, recreates or repairs a marked role or a personal row.
- `plan_key_roles` revokes keys bound to marked names, and the data API's key-role check refuses them.

**Checklist (red-team "Personal logins, worker side", copied):**
- the reconcile's desired set comes from qualifying records; non-qualifying or orphaned marked roles are dropped as `cloud_admin` with sessions ended (MH-10, MH-17);
- the resync never records a marked role as `sql` and never writes a verifier back for a revoked record (MH-9, MH-10);
- memberships come from the member-role safe set, `INHERIT FALSE, SET TRUE` (R10), never the owner (MH-23); the lock marker comes before any grant on a protected branch (MH-16);
- a revoke ends the login's sessions (MH-17); a reset ends them too;
- R31-style cleanup of direct privileges and ownership on personal logins before memberships (MH-18);
- `plan_key_roles` revokes keys bound to a marked role; delete never waits on keys (MH-12);
- branch copies, restores, resets and protected children converge per D2; worker unit tests for each orphan origin; live tests for a restore before deletion and a removal while suspended;
- forwarding stays off for every personal-login statement (`admin_sql` as `cloud_admin`, `crates/chelabase-neon/src/backend.rs:329-335`).

Plus:
- K6 H2: memberships are granted by the superuser only;
- K6 H5: `REASSIGN`/`DROP OWNED` before `DROP ROLE`;
- an unmarked role holding the record's name is never adopted (the create fails, logged, the record kept);
- `FreshPasswords::assign` skips marked roles;
- `guard_logins` on guards every personal login, Admins' included (no exemption by default);
- a branch not ready → `failed` (the sweep retries);
- `child_role_copies` leaves out personal rows (S1);
- a member-filtered run plans that member's personal logins and never drops an orphan (S9);
- every personal row written or deleted clears the proxy cache, now and after `REPUBLISH_DELAY` (S16);
- a tenant admin's memberships come from `tenant_admin_effective()` (S16).

**RED tests (test agent):**
- Unit, `member_roles/tests.rs` and `personal_logins.rs`:
  - `plan_creates_marks_and_locks_in_one_transaction` (first failure: compile, `CreatePersonal`);
  - `plan_drops_an_orphaned_marked_role_after_cleanup`;
  - `plan_drops_a_login_whose_member_no_longer_qualifies`;
  - `plan_drops_an_expired_login`;
  - `plan_sets_the_record_verifier_back_after_a_copy`;
  - `plan_grants_only_developer_level_safe_roles`;
  - `plan_never_grants_the_owner_or_an_owner_level_role`;
  - `a_rotation_or_drop_ends_sessions`;
  - `valid_until_drift_is_repaired`;
  - `a_member_filtered_run_creates_that_members_login_and_drops_no_orphan` (S9);
  - `personal_row_writes_clear_the_proxy_cache_twice` (S16);
  - `a_child_branch_copies_no_personal_row` (`branch_create.rs`, S1).
  - Orphan origins, one test each: `orphan_from_a_restore`, `orphan_from_a_reset_to_parent`, `orphan_from_a_lost_create_answer`, `orphan_from_a_deleted_record`.
- `resync.rs` tests:
  - `resync_leaves_marked_roles_and_personal_rows_alone` (replaces `:3307-3410` deliberately);
  - `fresh_passwords_skip_marked_roles`;
  - `key_roles_bound_to_a_marked_role_are_revoked`.
- `data_api.rs`: `a_key_cannot_use_a_marked_role`.
- Live, `tests/personal_logins_live.rs`, traced by reading and compiled `--no-run`:
  - `a_personal_login_reaches_only_its_safe_set_through_set_role`;
  - `a_new_branch_gets_the_login_through_the_reconcile`;
  - `a_restore_before_deletion_brings_no_login_back`;
  - `a_removed_member_loses_the_login_on_every_branch`;
  - `a_guarded_login_is_refused_ddl_on_a_protected_branch`;
  - `a_reset_password_works_and_the_old_one_is_refused_on_every_branch`.
- `compute_fork_live.rs:735` changes deliberately: a recreated personal login comes from the reconciler.

**Gotchas:**
- The worker live harness runs an in-process `RealWorker` beside the stack's own worker. That's why these tests are deferred (P22).
- Personal rows' `neon_roles` writes go through T3's guarded functions only.
- The existing `Drop` path's per-database `DROP OWNED` (`member_roles.rs:1661-1693`) needs a reassign target: the owner.

**Verify:**
- `cargo test -p worker`;
- `cargo clippy -p worker --all-targets -- -D warnings`;
- `cargo test -p worker --test personal_logins_live --test compute_fork_live --test member_roles_live --no-run`.

**Live runs:** none in the task (P22). `personal_logins_live`, `compute_fork_live` and `member_roles_live` run at End of step 5.

---

## T9: roster access and the log (sonnet / sonnet / opus)

**Worktree:** `.worktrees/members-hub-t9` from `members-hub` after T3.

**Files:**
- `services/orchestrator/src/{members.rs, roles.rs, lib.rs, openapi.rs}` (`members.rs` gains `MembersPort::record_access_event` and `member_project_roles`, S13), `crates/chelabase-routes/src/lib.rs` (only if a query param is documented there);
- `services/orchestrator/tests/{api.rs, common/fakes.rs}`;
- generated files.

**Behaviour:**
- spec Part 2 API with R1 and MH-35 (`level` over visible projects; `other`; `none` only when no grant anywhere; `projects` filtered);
- `MemberStatusView::Suspended` listed;
- `GET /v1/access-events?member_id=` and the project route's;
- `branch_id` on `AccessEvent`;
- role events from `create_role`/`update_role`/`delete_role` after `Done` (P25), with `branch_id` and `database_id`;
- `access` is always present (never `null`), whatever the status; Studio groups by status first (S17);
- `has_access` kept as is.

**Checklist (red-team "Roster / member API", copied):**
- `access.projects` and `level` cover visible projects only (R1); the group label per MH-35;
- member counts and holders only for Admins; no widening of `LIST_PROJECT_GRANTS` (MH-34);
- `access_events.branch_id` migration; the `member_id` filter matches team-affected members (MH-7, MH-37; step 1: target or actor);
- Part 7's table matches the routes: project invites and project Activity for project Admins (MH-36);
- `remove_member` touched includes team and personal-login projects (MH-5; T3 did the metadata half).

Plus: a Developer sees others' platform role per visible project only, never `pg_roles` or overrides; a tenant-admin member's level is `tenant_admin` whatever their grants.

**RED tests (test agent).** In `tests/api.rs`, with fakes; first failure: the `access` field is missing (JSON `null`) unless noted:
- `roster_level_is_over_the_callers_visible_projects`;
- `a_member_only_on_hidden_projects_is_other_and_one_without_grants_is_none`;
- `a_tenant_admin_sees_every_project`;
- `roster_lists_suspended_members`;
- `events_filter_by_member_as_target_or_actor` (first failure: the param is ignored);
- `events_carry_branch_id`;
- `role_create_update_delete_write_events_with_the_branch`.

**Verify:** `cargo test -p orchestrator`; clippy; `just api-gen` last.

**Live runs:** none (fakes). End of step 5 runs `api`.

---

## T5: privileges read and apply (opus / opus / opus)

**Worktree:** `.worktrees/members-hub-t5` from `members-hub` after T1, T3, T4 and T9 merged.

**Files:**
- `services/orchestrator/src/table_api/{changes.rs, mod.rs}`, new `src/privileges.rs` (the read route), `src/lib.rs` (route wiring), `src/openapi.rs`, `src/members.rs` (`MembersPort::is_personal_login_name`, S13);
- `crates/chelabase-routes/src/lib.rs` (`GET_PRIVILEGES`);
- `crates/chelabase-schema/src/privileges_sql.sql` and `catalog_sql.sql`/`catalog_parts/head.sql` only when T1's report requires it (items 6 and 7, S4);
- `services/orchestrator/tests/{api.rs, common/fakes.rs}`;
- generated files.

**Behaviour:** spec Part 4 "After the red-team" with P10–P13 and T1's rulings.
- **Read:** resolves `pg_database` through `branch_databases` only (`lib.rs:2947` pattern), connects as the owner (console pool), runs `privileges_sql`, and marks `owner_settable` per P12.
- **Apply** (`apply_schema_changes`, `changes.rs:1070`). When the request holds a privilege op (`Grant`, `Revoke` or any new op):
  - check `expect_acl` against `acl_hashes_sql` in the transaction, before the statements (S3);
  - resolve functions and owners in the transaction (`owners` only in an owner run, S4);
  - refuse personal-login grantees (`is_personal_login_name`; 400 `invalid_request` with `op_index`, S5) and owner-unreachable objects (422 `invalid_request`, `extra.owner`);
  - run each `run_as` statement as P12 (a): transient `GRANT X TO <owner> WITH INHERIT FALSE, SET TRUE`, `SET LOCAL ROLE X`, the statement, `RESET ROLE`, `REVOKE SET OPTION FOR X FROM <owner>`, adjusted by T1's report (S4's table);
  - treat notices `01006`/`01007` as failures, by `DbError::code()` (the `Notices` sink, `changes.rs:163-220`);
  - re-read every touched (grantee, schema) page to its last page, plus the `schema: None` page for schema and default-privilege ops, and `verify_privileges` → 409 (S3);
  - for `SetAllRows`, run the exposure check over the catalog memberships plus the branch's live key roles (metadata) → 422;
  - commit, then write `privileges_changed` (P13).
- A dry run follows the apply's rules and access (MH-31).
- An owner run is Admin only (the route's existing `ActAsBranch` + owner rule, R6/MH-30). A Developer's run as a role does what Postgres allows that role, and logs too.

**Checklist (red-team "Privilege ops", route part, copied):**
- in-transaction verify-after-apply; `no privileges` warnings are failures (MH-24);
- the owner-ownership scope per D6 and P12;
- all-rows refused when the target is, or is inherited by, `anon`/`authenticated`/`authenticator`/a key role/PUBLIC (MH-26);
- `pg_database` only via `branch_databases`; `MAINTAIN` by server version; the dry run under the apply's access rule (MH-31);
- apply carries the read's ACL hash (MH-33);
- every privilege op logs `privileges_changed` whatever the route (MH-30).

Plus:
- the read is capped and paged (MH-32);
- no SQL text in events or logs;
- `RESET ROLE` after every `run_as` statement, so a failing statement can't leave the session as X (the transaction rolls back anyway);
- `expect_acl` naming an object the request doesn't touch is ignored.

**RED tests (test agent).** Live-on-scratch (`changes_app`, `api.rs:12334`) unless noted; first failure: compile, or 404 for the new route:
- `privileges_read_lists_grants_with_grantors_and_hashes`;
- `privileges_read_pages_per_schema`;
- `privileges_read_refuses_a_pg_database_outside_branch_databases`;
- `owner_run_grants_on_a_table_owned_by_a_settable_role`;
- `owner_run_refuses_an_object_whose_owner_is_unreachable`;
- `a_revoke_another_grantor_still_holds_is_privileges_not_applied` (rolled back);
- `a_stale_acl_hash_is_privileges_changed_since_read`;
- `a_changed_function_identity_is_privileges_changed_since_read`;
- `all_rows_for_a_role_anon_inherits_is_exposes_data_api`;
- `all_rows_toggle_off_drops_only_the_marked_policy`;
- `a_personal_login_is_refused_as_grantee`;
- `a_stale_hash_from_another_grantees_change_is_detected` (S3: the request's role untouched, another grantee's grant added since the read);
- `default_privileges_for_another_role_apply_as_that_role` (S4);
- `the_dry_run_returns_the_same_sql_and_changes_nothing`.
- Fakes: `every_privilege_apply_writes_privileges_changed_without_sql`; `a_developer_run_as_a_role_is_logged_too`.

**Verify:**
- `cargo test -p orchestrator`; clippy;
- `cargo test -p orchestrator --test api --no-run`;
- `just api-gen` last.

**Live runs:** the new ignored `api` tests above by name, once green (scratch DBs on the metadata Postgres; no conflict with S1).

---

## S1: Studio foundation and roster (sonnet / sonnet / sonnet)

**Worktree:** `.worktrees/members-hub-s1` from `members-hub` after T9 merged and the comps approved.

**Files:**
- `apps/studio/src/routes/{app.members.tsx, app.members_.$memberId.tsx (new; a stub page S2 fills), app.members_.roles.$databaseId.$branchId.$role.tsx (new; a stub S3 fills)}`, `src/routeTree.gen.ts`;
- `src/components/members/hub/*` (new; exports pinned);
- `src/components/members/{members-page.tsx, people-tab.tsx, invite-dialog.tsx, grant-rows.tsx, role-picker.tsx, access-event-list.tsx}` and their tests;
- `src/lib/members.ts`.

**Behaviour:**
- spec Parts 1, 2 and 7 (Studio rules), and Part 5's invite paragraph minus teams (R8: one `POST /v1/invites` per email, in sequence, at most 50, a result per email, link invites listed together);
- back links carry tab, project, view and q;
- the People tab's row menu keeps the quick actions; tenant-admin changes move to the member page;
- per the approved comps.

**Checklist (red-team "Studio", copied, S1's part):**
- `roleChoices` skips `origin === "personal"`; the branch Roles panel stops listing personal logins (MH-11; the panel is S4's);
- the grant editor's PUT body is the own grant only (MH-38a);
- search params parsed against enums; localStorage reads and writes in try/catch with a list fallback (MH-38b);
- role names rendered escaped (MH-38d);
- member counts hidden for non-Admins (MH-34);
- `pending` members stay in the Requests block, never in a group; status decides before level (S17);
- every new action has a sentence and `suspended` a status word (S17).

**RED tests (test agent).** Vitest, `*.test.tsx` beside each component; first failure: the module is missing:
- `groupRoster_orders_groups_and_hides_empty_ones` (pending left out, invited and suspended by status, S17);
- `new_actions_and_the_suspended_status_have_words` (`lib/members.ts`, S17);
- `project_filter_regroups_by_that_projects_role`;
- `view_toggle_persists_and_falls_back_to_list_when_storage_throws`;
- `unknown_search_params_are_dropped`;
- `role_checklist_filters_above_8_and_shows_also_gives`;
- `role_checklist_escapes_control_and_bidi_characters`;
- `personal_logins_are_never_role_choices`;
- `grant_card_sends_the_own_grant_only`;
- `multi_email_invite_sends_one_request_per_email_and_shows_each_result`;
- `invalid_emails_are_flagged_per_chip_and_not_sent`;
- `back_link_restores_tab_filter_and_view`.

**Verify:** `pnpm --filter @chelabase/studio typecheck`, `pnpm --filter @chelabase/studio test`, `pnpm --filter @chelabase/studio generate-routes` (committed output).

**Live runs:** none (the browser pass is End of step 5).

---

## T8: personal-login API, revocation side effects, suspension, webhook (opus / opus / opus)

**Worktree:** `.worktrees/members-hub-t8` from `members-hub` after T5 merged.

**Files:**
- `services/orchestrator/src/`: new `personal_logins.rs`, `members.rs` (`access_changed_for` and its callers; `check_names` refuses personal names, through `personal_login_names(project)` for grants and `is_personal_login_name` for overrides, S12; the `MembersPort` methods, S13), `access.rs` (act-as refusal of a personal login for everyone, MH-21), `roles.rs` (generic routes treat personal rows as absent, MH-11; create and rename refuse a personal-login name, S11), `data_keys.rs` (refuse a personal login, MH-12), `internal/app.rs` (webhook revocation, P17), `internal/ddl.rs` (`publish_password_updated`), `internal/mod.rs` (port methods), `lib.rs` (`AppState.proxy_cache`, routes), `main.rs` (one `ProxyCache` `Arc` for both states, S19), `openapi.rs`;
- `crates/chelabase-routes/src/lib.rs` (the eight T8 endpoints, with their `spec_table` rows, S14);
- `crates/chelabase-zitadel/src/client.rs` (`lock_user`, `unlock_user`: `POST /v2/users/{id}/lock` and `/unlock`; already locked or unlocked is fine, like `deactivate_user`'s 400/404/412 handling, `members.rs:2037-2045`) and `src/mock.rs` (the `/lock` and `/unlock` routes and the `Locked` state, S18);
- `services/orchestrator/tests/{api.rs, internal.rs, common/fakes.rs}`;
- new `scripts/personal-logins-e2e.sh`, `justfile` (`personal-logins-e2e`), `scripts/tests/workbench-recipes.sh` (the e2e list);
- generated files.

**Behaviour:** spec Part 6 "API", "Rules" (qualifying, name), "Hidden elsewhere" (orchestrator part), "Revocation is immediate" (orchestrator part), "Identity-provider offboarding" (webhook), "Suspend and immediate revoke"; P4, P6–P8, P14–P17, P23, P24.

Create runs as follows:
1. `generate_password` → SCRAM verifier.
2. `create_personal_login`.
3. A synchronous member-filtered reconcile of the main branch (P7), through `state.js.reconcile_member_roles` directly, not `reconcile_member_role` (which answers 503 on a timeout, `sql_editor/routes.rs:388-411`). `ready_on_main` is true only when the reply is `Done` and `member_login_endpoints` lists the main branch's endpoint for this role; any other outcome is `false`, never an error (S9).
4. Enqueue the other branches.
5. Answer once with password and connection string, `no-store`, never logged.

Suspend: the metadata write (returns the touched projects), then `access_changed_for(projects)`, then Zitadel `lock_user` + `delete_user_sessions`. A suspended member's suspend redoes only the Zitadel step (S7).

Reinstate: the metadata write, then `unlock_user`. An active member's reinstate redoes only the unlock (S7).

Revoke Postgres access now (P16 b, S2): `revoke_postgres_access` (logins revoked, member roles marked for rotation, touched projects returned), then `access_changed_for(projects)`: the cache is cleared and every branch of every touched project reconciles, rotating the member's `cb_m_*` passwords and ending their sessions. Answer `RevokeResult`.

**Checklist (red-team "Personal logins, API side", copied, orchestrator part):**
- create: pending row → SQL create plus marker (plus the lock marker on a protected branch, MH-16) → confirm, under the endpoint lock; `PUT` on an existing login is 409; the password is server-generated and never logged or stored sealed. In step 1 the record is first and the worker's reconcile does the SQL (P7);
- every access-reducing write also revokes the affected logins' proxy verifiers in the same transaction (MH-9; T3), and here the cache is cleared;
- generic role routes (list, reset, connection, update, delete, rename, sessions) treat personal rows as absent; the c3 list test changes deliberately (MH-11);
- data keys refuse personal logins (orchestrator and worker) (MH-12);
- `authorize` refuses act-as of a personal login for everyone; `check_names` refuses personal-login names in grants and overrides (MH-21);
- `GET /v1/members/{id}/personal-logins` is filtered by caller (MH-22);
- name rule per D3; expiry per D4 (`valid_until = now + 90 days` on create and reset);
- `qualifies(member, database)` = active and effective level ≥ Developer (tenant admins included) (MH-17); route tests for each Part 7 row, allow and refuse.

Plus:
- a Viewer is 403 `forbidden` (required `developer`);
- a suspended caller is 403 `member_suspended`;
- deleting another member's login as a Developer is 404;
- the webhook revocation runs before today's realtime event and answers 500 on a metadata error;
- the default name is computed client-side (task S2);
- nothing in a log line or event carries a password or verifier;
- `LoginWriteError` maps as pinned (S15);
- cache tests wait on the fake `CachePort`'s record, never sleep (S19).

**RED tests (test agent).** `tests/api.rs` with fakes unless noted; first failure: 404 (route missing) or compile:
- One row-by-row test per Part 7 table row: `personal_login_create_is_own_and_developer_plus`, `personal_login_delete_by_self_tenant_admin_or_project_admin`, `personal_login_list_is_filtered_by_caller`, `suspend_reinstate_and_revoke_are_tenant_admin_only`.
- Others:
  - `create_answers_the_password_once_with_no_store`;
  - `create_twice_is_personal_login_exists`;
  - `a_name_taken_on_another_branch_is_role_exists`;
  - `create_asks_main_for_a_synchronous_reconcile_and_enqueues_the_others`;
  - `reset_renews_valid_until_and_clears_the_cache`;
  - `every_access_write_clears_the_proxy_cache_for_the_members_logins` (grant put/delete, override, tenant admin, remove, suspend, revoke-now);
  - `suspending_yourself_or_the_last_tenant_admin_is_refused`;
  - `suspend_locks_the_zitadel_user_and_reinstate_unlocks_it`;
  - `generic_role_routes_hide_personal_logins` (list, reset, delete, update, sessions, connection: 404);
  - `act_as_a_personal_login_is_forbidden_even_for_a_tenant_admin`;
  - `grants_and_overrides_refuse_a_personal_login_name` (a project grant naming a login of any of its databases, S12);
  - `a_key_cannot_be_bound_to_a_personal_login`;
  - `database_personal_logins_are_project_admin_only` (P24, S8: Admin and tenant admin get every record; Developer 403);
  - `revoke_now_revokes_logins_and_reconciles_every_reached_project` (P16 b, S2);
  - `a_failed_lock_is_completed_by_the_retry` (S7: suspend → 503 when the mock refuses the lock; the retry is 200 and locks);
  - `create_reports_ready_on_main_false_when_the_reconcile_times_out` (S9: 201, not 503);
  - `create_role_refuses_a_personal_login_name` (create and rename, S11).
- `tests/internal.rs`: `a_deactivated_zitadel_user_loses_their_logins_and_the_cache_is_cleared`; `a_failed_revocation_is_500_so_zitadel_retries`.
- `chelabase-zitadel` mock: `lock_and_unlock_are_idempotent`.
- `scripts/personal-logins-e2e.sh` (traced by reading; the RED is that the recipe is missing). On the stack, it:
  - creates a project, a database, a group role with a table, and a member with a grant;
  - the member creates a login and connects with `psql` through the proxy, where `SET ROLE` works only into the safe set;
  - a child branch gets the login;
  - reset: the old password is refused and the new one works;
  - grant deleted while the compute is suspended → the proxy refuses before waking the compute (MH-9);
  - re-grant, new login, suspend → refused; reinstate → still refused until a new login;
  - revoke-now → refused;
  - Zitadel deactivate (real webhook) → refused;
  - an expired `VALID UNTIL` (set short through the metadata) → refused;
  - everything is cleaned on exit.
- Changed deliberately: `list_roles_shows_a_personal_login_with_its_origin` (`api.rs:2790`) → `list_roles_hides_personal_logins`.

**Gotchas:**
- `AppState` is built in `main.rs` and `tests/common/fakes.rs:2528`. The fake `CachePort` records publishes.
- The internal webhook path holds `InternalState.cache` and `resync`.
- `workbench-recipes.sh` checks each e2e recipe runs `workbench run scripts/<name>.sh`.

**Verify:**
- `cargo test -p orchestrator -p chelabase-zitadel`; clippy;
- `cargo test -p orchestrator --test api --test internal --no-run`;
- `bash -n scripts/personal-logins-e2e.sh`, `scripts/tests/workbench-recipes.sh`;
- `just api-gen` last.

**Live runs:** none in the task (P22). `just personal-logins-e2e` runs at End of step 5.

---

## T7: identity sweep (sonnet / sonnet / none, leaf)

**Worktree:** `.worktrees/members-hub-t7` from `members-hub` after T6.

**Files:** `services/worker/src/{identity_sweep.rs (new), lib.rs, main.rs, config.rs, ports.rs, fakes.rs, rt_events/delegate.rs}` and tests (`delegate.rs` implements `WorkerMetadata` too, S13; the port gains `members_with_live_logins` and `revoke_logins_for_zitadel_user`).

**Behaviour:** spec Part 6 "Identity-provider offboarding" backstop, as pinned under "Sweep". Each revoked member's branches get a reconcile request, and `/password_updated` goes out now and again after `REPUBLISH_DELAY` (`resync.rs:172`).

**Checklist:**
- Zitadel errors leave the member alone and are counted, not revoked (fail-safe: no revocation on an unreadable state, logged);
- a `None` user (404) is offboarded;
- `Inactive` and `Locked` are offboarded, `Active` and `Other` are kept;
- without a Zitadel client the loop logs once and idles;
- `CHELABASE_IDENTITY_SWEEP_SECS` is parsed with bounds; a bad value fails startup with the variable named.

**RED tests (test agent).** Unit, with the Zitadel mock and worker fakes; first failure: compile:
- `inactive_locked_or_missing_users_are_offboarded`;
- `active_users_and_unreadable_states_are_kept`;
- `a_revocation_clears_the_cache_twice_and_requests_reconciles`;
- `the_interval_is_bounded`.

**Verify:** `cargo test -p worker`; clippy.

**Live runs:** none.

---

## S2: member page (sonnet / sonnet / none, leaf)

**Worktree:** `.worktrees/members-hub-s2` from `members-hub` after S1 and T8.

**Files:** `apps/studio/src/components/members/member-page/*` (new) and tests, `src/routes/app.members_.$memberId.tsx` (body only, P20).

**Behaviour:**
- spec Part 3 sections 1, 2 and 4 (no teams), and the header actions by power (Part 7);
- Suspend / Reinstate / Revoke Postgres access now (tenant admin; confirm copy in the comps);
- personal logins: list, New login (project → database → name, defaulted from the email's local part sanitized to `^[a-z_][a-z0-9_]{0,62}$`), the reveal dialog with `CopyButton` (state in the dialog only, cleared on close), Reset, Delete, Renew within 14 days (Renew is `resetPersonalLoginPassword`: a new password and 90 more days, through the same reveal dialog, S17);
- the activity section reads `listAccessEvents` with `member_id` for a tenant admin and `listProjectAccessEvents` with `member_id` per visible project for a project Admin (both routes' access, `crates/chelabase-routes/src/lib.rs:777-791`); others see none;
- self view read-only except own logins;
- the unsaved-cards router blocker (R2), as the table editor's;
- per the approved comps.

**Checklist (Studio):**
- the password reveal lives only in the dialog's state (MH-38c), never in the URL, router state, the query cache or storage;
- the PUT body is the own grant only;
- a Developer viewing another member sees platform roles per visible project only.

**RED tests (test agent).** Vitest:
- `the_password_is_shown_once_and_gone_after_close`;
- `the_password_never_reaches_the_query_cache_or_storage`;
- `default_login_name_is_sanitized_from_the_email`;
- `renew_appears_within_14_days_and_resets_through_the_reveal_dialog`;
- `suspend_and_revoke_are_tenant_admin_only`;
- `own_page_is_read_only_except_personal_logins`;
- `leaving_with_unsaved_cards_asks_first`.

**Verify:** Studio typecheck + test.

**Live runs:** none.

---

## S3: Roles tab, role page, privilege editor (sonnet / opus / none, leaf)

**Worktree:** `.worktrees/members-hub-s3` from `members-hub` after task S1, T5 and T8 (P24's route and its generated client, S8).

**Files:** `apps/studio/src/components/members/roles/*` (new) and tests, `src/routes/app.members_.roles.$databaseId.$branchId.$role.tsx` (body only).

**Behaviour:**
- spec Part 4: the picker, the role list (cards/list, owner/reserved/personal hidden, "Personal logins on this database: N" per P24, for project Admins and tenant admins only: `listDatabasePersonalLogins` counting `status === "active"`, names joined from `listMembers`, each linking to `/app/members/$memberId`; S8), the create-role dialog, and the role page;
- the staged privilege editor: checkboxes → Review changes (dry-run SQL, the "exposes to the data API" flag) → Apply with `expect_acl` (each touched object's `acl_hash` from the read, its `PrivilegeTarget` in serde form, S3); 409 `privileges_changed_since_read` → reload prompt; 409 `privileges_not_applied` → names object, privilege and grantor; 422 `exposes_data_api` → shows the chain; table and column privileges use the existing `grant`/`revoke` ops (`model::Grant.privilege` is upper case, the ops' lower case);
- greyed "owned by X: change in SQL as X" rows;
- revoke-grant-option only (MH-29);
- holders and member counts only for Admins (MH-34);
- memberships editing through `PATCH …/roles/{role}`;
- the danger zone;
- per the approved comps.

**Checklist (red-team "Studio", copied, S3's part):** the privilege editor shows grantors, refuses all-rows with the server's chain message, flags data API exposure in review, and greys objects per MH-25 (D6: per `owner_settable`).

**RED tests (test agent).** Vitest:
- `checkboxes_stage_and_review_shows_the_dry_run_sql`;
- `apply_sends_expect_acl_for_touched_objects`;
- `a_stale_read_prompts_a_reload`;
- `privileges_not_applied_names_the_grantor`;
- `exposes_data_api_shows_the_chain`;
- `unsettable_owners_grey_the_row`;
- `grant_option_is_revoke_only`;
- `holders_are_admin_only`;
- `personal_login_count_is_admin_only_and_links_to_member_pages` (S8);
- `create_role_goes_to_the_new_roles_page`.

**Verify:** Studio typecheck + test.

**Live runs:** none.

---

## S4: project Access tab and branch Roles panel on the shared components (sonnet / sonnet / none, leaf)

**Worktree:** `.worktrees/members-hub-s4` from `members-hub` after S2 and S3.

**Files:** `apps/studio/src/components/project-access/*`, `src/components/branch-roles.tsx` and their tests.

**Behaviour:** H10: both stay, rebuilt from `GrantCard`, `RoleChecklist` and the role editor, each with "Manage in Members". The branch Roles panel stops listing personal logins (MH-11, `branch-roles.tsx:35`). Owner-level role creation stays in the panel only (Part 4).

**RED tests (test agent):**
- `access_tab_uses_the_shared_grant_card_and_links_to_members`;
- `branch_roles_panel_hides_personal_logins_and_links_to_the_role_page`.
- Existing `access-tab.test.tsx` and `branch-roles.test.tsx` are kept green.

**Verify:** Studio typecheck + test.

**Live runs:** none.

---

## End of step

0. Before this plan: the spec's red-team (opus, MH-1–38) is done. At the step's start, `main` is green (hardening merged `0aa8f002`, `just check` green there). Before W1: the user's approval of this plan (done), the **seam pass** (done: S1–S21 in "Seam pass"), and the stack up.
1. Whole-branch review (opus) on `main..members-hub`. It covers the leaf tasks (T1's probe, T7, S2, S3, S4) and checks the comps were followed.
2. The one final fix round: the review's findings plus every deferred task finding (Status), batched by area into pipelines (test agent → coder → audit, models by area as in the task table). The coordinator verifies each fix diff.
3. `just --justfile <wt>/justfile --working-directory <wt> check`, once.
4. No fork change: no gate or image build. The live pass runs on the pinned images. First the apps are restarted from the merged branch, once: `just --justfile <wt>/justfile --working-directory <wt> workbench restart orchestrator worker gateway rest realtime auth`. This applies the migration and rebuilds the gateway's route registry.
5. Live pass on the plan's stack, one suite at a time, whole files, once, in this order. No two overlap and nothing restarts an app after step 4. `W` stands for `just --justfile <wt>/justfile --working-directory <wt> workbench test`.
   1. `W -p chelabase-metadata --test members_live -- --run-ignored only`, then `--test live_db`.
   2. `W -p chelabase-schema --test privileges_live -- --run-ignored only`, then `--test catalog_live`, `--test review_live`.
   3. `W -p orchestrator --test api -- --run-ignored only`, then `--test sql_run_live`.
   4. `W -p worker --test d6_feasibility_live -- --run-ignored only`, then `--test personal_logins_live`, `--test member_roles_live`, `--test compute_fork_live`, `--test roles_live`, `--test branch_live`, `--test merge_live`.
   5. e2e, in this order:
      - `just personal-logins-e2e` (new; covers the proxy refusals, MH-9);
      - `just members-e2e`;
      - `just role-e2e`;
      - `just sql-editor-e2e`;
      - `just merge-e2e` (the catalog's marked-role exclusion);
      - `just data-e2e` (keys);
      - `just app-users-e2e` (the webhook path).
   6. Browser pass (Playwright MCP, `http://studio.members-hub.chelabase.localhost:<port>` from `workbench status`), as a tenant admin and as a Developer in isolated contexts. The path is roster (cards, list, filter, back links) → member page (grant edit, personal login create and reveal, reset, delete, suspend/reinstate) → Roles tab → role page (privilege edit, review, apply, a stale-read conflict, all-rows refusal) → project Access tab and branch Roles panel. Then the Impeccable finish reviewer on the hub surfaces against the approved comps.

   A failure goes to a debug agent (sonnet; opus for auth, revocation, the proxy or concurrency causes). The pass resumes from the failed suite (Status).
6. `docs/landing-highlights.md` on `main`:
   - what the step solves: one place for access; personal Postgres logins that are revoked the instant access goes (proxy refusal before any compute work), with an IdP backstop; privileges editable as the object's owner, with checked results and no silent no-op revokes;
   - what it unlocks: step 2 teams on the same effective types;
   - evidence: the suites above;
   - the original Neon base vs ours.

   Then update the plan Status and the roadmap's "Resume here", and **pause for the user** before merging.
