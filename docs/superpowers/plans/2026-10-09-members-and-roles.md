# Members and roles Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Studio people become tenant members. Each member has a per-project platform role (Admin / Developer / Viewer) and a set of Postgres roles they may act as, narrowed per Chelabase database and per Postgres database. One policy table on every route enforces this, and Postgres enforces it too through hidden per-member login roles, with a single self-hosted/platform boundary (`ModePolicy`).

**Architecture:**
- **Policy:** a required `access` field on every `chelabase_routes::Endpoint`, a pure `chelabase_types::access::decide`, and an orchestrator `authorize` that replaces `load_owned`.
- **Metadata:** members, grants, overrides, invites, member roles and `access_events`, with the last-admin and first-claim invariants enforced in SQL transactions.
- **Member roles:** a worker reconciler keeps `cb_m_*` roles on each branch equal to metadata. User-SQL paths log in as them, then `SET ROLE`.
- **Identity:** invites use Zitadel invite codes. The auth service gates sign-up through the orchestrator's internal listener.
- **Studio:** a Members rail item, a project Access tab, a "no access yet" screen and `/accept-invite`, designed through an Impeccable shape pass first.

**Tech Stack:** Rust (axum, utoipa, sqlx runtime queries, async-nats, tokio-postgres for live tests), proptest 1.11.0 (new workspace dev-dependency), Zitadel v4.19.1 v2 API, React 19 with TanStack Router, `@chelabase/management-client` (orval), `packages/lagoon`, Vitest 5 and Testing Library.

**Spec:** `docs/superpowers/specs/2026-10-09-members-and-roles-design.md` (M1–M15, parts 1–7). Implementers read the spec part their task names.

## Status

- **RESUME HERE (2026-10-09):** `members` holds T1–T9, T4b, T10, T11, T12a, T12x and T12y (T7 merged at `4463812`, T12a at `866bc2d`, T9 at `83093da`, T12x at `72b9ee0`, T12y at `1b44533`). No task worktree is open. Briefs are in `.superpowers/sdd/2026-10-09-members-and-roles/task-TN-brief.md` (git-ignored scratch); T12x and T12y also have their own task sections in this plan. If a session dies, check the worktree's commits before restarting anything. **Next:**
  1. **Done: T12x** (sonnet/sonnet), merged `72b9ee0` (fixes `ff9635d..7223e16`, verified by the coordinator; api 375, internal 106, access_walk 7 pass after the merge).
     - Live tests: `members_live::{access_events_join_names, member_listings_report_has_access, invite_description_follows_the_open_invite}`.
  2. **Done: T12y** (sonnet/opus), merged `1b44533`. It went through three rounds: `6543827..02b91c8`, then `..7ce59c4` (the rework, R34 revised to R40), then `..ebfc34b` (the re-review fixes: the status × write matrix, `lock_writable_member`, the demo user made tenant admin in dev, the org login policy, link invites refused for any auth method). The coordinator verified the last diff. Live: `members_live::{member_status_write_matrix, pending_member_lifecycle, bootstrap_names_the_tenant_once, tenant_display_name_round_trip}`, `just live auth-e2e`.
  3. **Done: T12b** (opus/sonnet), merged `d7408b1`; fixes `38dce06..f53d157` verified by the coordinator; Studio 930 tests. (Built `44049dc..38dce06`; review (sonnet): 4 important, ~14 minor; **fix round running**, which may touch Rust for three API gaps: new 409 codes `link_not_allowed`, `signup_pending`, `email_ambiguous`, `email_in_use_elsewhere`, `invite_closed`; `member.id` on `/v1/me`; `extra.project_id` on grant refusals. Capability hiding gets the invariant "every route-backed control is shown iff the caller passes that route's Access", proven by a table-driven test. The live browser pass must look at narrow widths, modal/popup z-order, the badge and focus return); **T13 done**, merged `620cea9` (`44049dc..7008d98`; new Using pages, generated access and mode tables with a staleness check in `just check`, `every_error_code_is_documented`; UI labels to revisit once T12b lands) (the user said yes on 2026-10-09). T12b first commits the brief amendments, which T12b applies to the brief files first. Members page: an Instance tab with the open sign-up switch (TenantAdmin, platform: hidden), a Requests section (pending sign-ups, approve with project access, decline) with a badge on Members, the tenant display-name field, email or link delivery in the invite dialog (`/v1/invites/delivery`, link shown once, refused for users with any auth method). A "Waiting for approval" screen for pending members. Sign-in shows "Create account" only when `signUpAllowed`. Studio's landing lost "Create account". T13 (sonnet, no review) runs in parallel with T12b.
  4. **Done: T12c** (sonnet/sonnet), merged `1c6b059` (`d7408b1..4eddd587`; review: 4 important, all fixed with RED shown; Studio 988 tests). **Done: T14** (sonnet, no review), merged `fbd4aab` (`scripts/members-e2e.sh`, `just members-e2e`; not yet run). **End-of-step review (opus) done** on `da62614a..fbd4aab4`: 0 critical, 6 important (I1 an expired invite still activates; I2 act-as follows SET FALSE memberships; I3 Studio ignores per-database overrides; I4 the T7 check-then-write race; I5 the e2e R31 step and revoke step; I6 doc mismatches), 16 minor. **Fixes done and merged** (docs/e2e `cf06318..b5f9dae`, merged `0906865`; code `cf06318..e4bc3bf6`, merged `4dcc017`; coordinator checked the reports, RED partly by mutation). New rulings in the code fix: R42 the I4 check passes the observed grant and overrides, a stale one gives `Changed`, it retries once, then 409 (cost if wrong: unrelated concurrent edits of one member retry); R43 the R41 race loser is recognised by an `admin_claimed` event within 1 hour (cost if wrong: an org user first calling within an hour of a fresh claim becomes a pending request); R44 a failed delivery is logged as `invite_revoked` with `reason: delivery_failed`; R45 the CLI keeps hand-written admin SQL (`tools/cli/src/dev/tenant_admin.sql`), live-tested against `bootstrap_admin`. To check: the docs test `badge_signature_is_on_operation_and_sidebar_badges` failed in the fix worktree (it may have been failing before). Live tests added: `members_live::{expired_invite_does_not_activate, grant_and_override_race_keeps_the_invariant, bootstrap_admin_sql_matches_the_metadata_crate, admin_claimed_since_reads_the_claim_event}`, `member_roles_live::remove_member_terminates_member_sessions`, and the `live_db` role-attribute tests. Check in the live pass that real Zitadel lets a deactivated user verify an invite code and have a password set (I1). Was: two parallel worktrees, `members-fix` (code; it carries the role-rule invariant that orchestrator, worker and Studio agree on reach (SET edges) and refusal (every edge, ADMIN included), proven by property tests and a shared fixture) and `members-fixdocs` (docs and `members-e2e.sh`). Rulings: the R41 race loser is admitted as `pending`; describe-invite becomes POST; R30 is superseded, so ADMIN chains are refused at grant time once 6a records membership options; platform mode refuses the default internal bearer, then the end-of-step review (opus) and its fixes (sonnet), then `just check`, then the live pass.
- **R34 (user, 2026-10-09; supersedes M1's "open sign-up is a self-hosted opt-in" and T7's `open_signup` switch):** self-hosted mode has no self-sign-up at all. Anyone who finds an exposed login page could otherwise spam it with fake sign-ups. People join only by invite from the `.env` bootstrap admin or another admin. Platform-mode sign-up gets its own design later, and stays refused until then. Cost if wrong: re-adding a sign-up path later.
- **`just check` passes on `members`** (2026-10-09, after `81badea`). It failed once: T13's "access" page broke the docs badge test, which read the Errors page. The test now reads the Access page. **Live pass running** via `.superpowers` scratch `live-pass.sh`. It runs, in order: the bootstrap twice; the live suites `members_live`, `live_db`, `catalog_live`, `member_roles_live`, `roles_live`, worker `app_users_live`, `sql_run_live` and `sql_schema_live`; then the e2e suites `auth`, `members`, `console`, `sql-editor`, `merge`, `role`, `branch`, `data` and `app-users`. After it comes a browser pass of the new screens. If it is interrupted, rerun the remaining items with `just live <item>`. **First live run, 2026-10-09:**
  - Green: bootstrap ×2, `sql_schema_live`, and the e2e suites `auth`, `console`, `sql-editor`, `merge`, `branch`, `data` and `app-users`.
  - Red:
    - the worker live suites: `CHELABASE_COMPUTE_IMAGE` wasn't set in the run environment;
    - `live_db`: 143 failed, mostly the `make_ready` helper;
    - `members_live`: 3;
    - `catalog_live`: 4;
    - `sql_run_live`: 3;
    - `members-e2e`: describe-invite as the demo machine user;
    - `role-e2e`: a key still answers after BYPASSRLS.
  - **Live debugging done** (opus, `be159fba..7c992b4c`). **The live pass is green** (logs in the session scratchpad `live-final*`):
    - members_live 34/34, live_db 184/184 (`--test-threads=1`), catalog_live 9/9 (`--test-threads=1`), member_roles_live 18/18, roles_live 42/42, worker app_users_live 13/13, sql_run_live 22/22, sql_schema_live 1/1, bootstrap ×2 (no new `admin_set`);
    - the e2e suites auth, console, sql-editor, merge, role, branch, data and app-users all pass. members-e2e passed after the rate-limit fix when run right after auth-e2e, but the full pass was not rerun after that fix.
    - The worker suites now run via `just live worker-live <test> [filter]`.
  - Real bugs it found:
    - the hand-over SQL alias `d` clashed with the DO block variable, so nothing was ever reassigned;
    - `member_role_branches` read the non-existent `d.tenant_id`, so the periodic sweep never worked;
    - the Zitadel auth-methods query was rejected by real Zitadel;
    - real Zitadel refuses invite codes for users with a password, so a re-invite of a removed member failed. **R46:** such a re-invite now sends Zitadel's password-reset email with the same `/accept-invite` link plus `reset=1` (cost if wrong: the email reads as a password reset).
    - **R47:** an inviter with no name or email shows as "An admin". **R48:** a removed member's `/v1/me` is 200 with `removed`.
    - role-e2e now escalates with CREATEROLE (BYPASSRLS keys have been allowed since 6g E33).
  - Pre-existing, not from this step: a cancelled or timed-out SQL-editor run can report `cancelled` after its COMMIT already happened. Follow-up.
  - `just check` is green. The stack is back on the main checkout. **R46 kept by the user.**
  - **Browser pass done** (opus): 6 small fixes in `e83b7133`. It found 3 important problems:
    - accept-invite doesn't sign the person in, because the page never sends an `authRequest`;
    - an expired invite lands on "This tenant has no projects yet";
    - **the resync drops password-less NOLOGIN roles, so SQL-made group roles never reach the picker**.
    
    It also found about 15 minor ones.
  - **R49 (user):** an expired invite shows a dedicated "Your invite expired" screen, with the admins listed and sign-out.
  - **Fix round done** (opus, `833aa2d2..1a3330f3`, 9 commits; RED/GREEN shown). Studio 1119 tests and `just check` pass; cargo 1345. Live re-check green: live_db 185/185, member_roles_live 18/18, roles_live 42/42, auth-e2e and members-e2e pass. The browser re-checked all three important fixes and the minor ones at 1280 and 375. **R50:** below 40rem, "Expanded" behaves like "Expand on hover". **R51:** a no-password `sql` role keeps a row with `neon_roles.no_password` (migration `20261014000008`). Wording choices are in the agent report and listed for the user at the pause. **PAUSED for the user's merge decision** (members → main). Then: landing highlights, delete branches, and bring the stack back up on `main`.
  - Then the user's pause before merging into `main`.
  - Note: the dev stack on `main` can't start the orchestrator until `members` merges, because the metadata DB already holds the members migrations.
- **R34 revised (user, 2026-10-09; supersedes the R34 line below where they differ).** Sign-up is **kept**, decided by mode. Self-hosted: off unless the tenant admin turns on the `open_signup` Studio switch (default off; spam is the org's call). Platform: refused until platform sign-up is designed. The coordinator's T12y brief wrongly said to remove sign-up end to end; T12y removed it, and the fix round restores it. Cost if wrong: none; this is the user's ruling.
- **R38:** bootstrap turns off Zitadel's own registration (`allowRegister: false`, review C1).
- **R39 (user):** an uninvited org user gets a member row on sign-in only when open sign-up is on, and then as `pending`.
- **R40 (user):** pending members wait for approval. They see "Waiting for approval". Tenant admins approve them (with project access) or decline them (removed and deactivated) from a Requests list with a badge on Members; both actions are logged. Zitadel has no built-in approval step, so the state lives in our member table. Cost if wrong: an extra status and 2 endpoints.
- **R41 (user):** the fresh-install exception stays: self-hosted sign-up is open while the tenant has no member yet, so the first person in can claim admin. Leaving that exposed without a `.env` admin is the self-hoster's misconfiguration. The docs (quickstart, security) must warn about it and say to set the bootstrap admin before exposing Studio. Cost if wrong: none; the user chose to document this rather than guard it.
- **T12y** implemented `6543827..02b91c8`. The review (opus) found:
  - 1 critical: Zitadel's own registration was open, plus auto-create on sign-in.
  - 2 important: a link invite to an existing user hands the inviter a takeover; the email-verification env var was dead.
  - 6 minor.
  
  - Fix round `02b91c8..7ce59c4`: it restored sign-up exactly as at `6543827`, plus `signUpAllowed`. It covers R38–R40 and every finding.
  - Scoped re-review (opus): 2 important, 5 minor.
    - I-A: grants can be put on pending members. The invariant gets an exhaustive status × write test.
    - I-B: `dev token` scripts get 403 under R39. `dev bootstrap` makes the demo machine user a tenant admin (dev only).
  - The final fix round is running (sonnet). The coordinator verifies it, then merges.
  - Live: `members_live::{pending_member_lifecycle, bootstrap_names_the_tenant_once, tenant_display_name_round_trip}` and `just live auth-e2e`.
  - A metadata DB that applied the old `20261014000005` needs a reset.
  - Studio's landing lost "Create account"; `/login` shows it when sign-up is allowed. Listed for the user with the T12b brief. Its first round removed `Access::Instance`, the operator-org plumbing and `instance_admin`; the rework restored all three (R34 revised). **R37 (user):** an unknown email on sign-in keeps the generic "Email or password is incorrect" plus a standing "No account? Ask your admin for an invite." line (no account enumeration). **Follow-up outside this step (user):** the landing page's "Start free" (`Studio/?signup`) becomes a platform **waitlist**; it needs a waitlist destination first (landing-page work, not Members).
- **R35 (user):** without SMTP, an admin invites by link. Studio detects whether Zitadel has an active SMTP provider: with SMTP, email is the default and the link option stays available; without it, link only. The one-time `/accept-invite` link is shown once and never stored, and the invitee sets their own password. Cost if wrong: an extra API field and a dialog option.
- **T12a answers (user, 2026-10-09):**
  - People: a "No access yet" label from `has_access`.
  - Activity names come from the API.
  - The role picker merges roles from each ready database's main branch.
  - A tenant admin can't remove themselves.
  - The role is called "Tenant admin".
  - **Members is last in the rail.**
  - A project Admin = owner + the listed roles, explained in the editor.
  - **The Access tab is shown to everyone; non-Admins see only "Your access".**
  - The no-access screen replaces only the Projects section.
  - **/accept-invite names the inviter and the tenant (R32).**
  - All new components live in Studio.
  - The modal is restyled now (no shadow, square, hairline) and gets a wide size.
- **R32 (user):** the describe-invite lookup is keyed by a Chelabase token in the link, not by the Zitadel user id. User ids aren't secret and are time-ordered. Cost if wrong: one invite column.
- **R33 (user):** data keys follow `refusal()` like grants and act-as: below Admin, no key bound to a role whose chain reaches a LOGIN, owner-level or reserved role. Cost if wrong: Developers need an Admin for such keys.
- **T9 detail:**
  - The first review (opus) found 3 important and 6 minor. It was the second finding in the escape area, so it got an invariant: after any act-as run ends, the member role owns nothing, appears in no ACL, default ACL or policy, and can `SET ROLE` only into its reconciled safe set. It is proven by `sql_run_live::member_role_holds_nothing_after_any_run`.
  - The hand-over is a worker RPC (`chelabase.rpc.member_roles.hand_over`): superuser, it terminates the run's backend, then `REASSIGN OWNED BY m TO acted; DROP OWNED BY m`.
  - **R26 (superseded by the hand-over RPC):** `REASSIGN OWNED` from the run's own session fails in PG17 (it needs the target role's privileges).
  - **R27:** the lazy check reconciles on a missing row, a failed password (28P01 or 28000 → one reconcile and retry), an ungranted or now-unsafe held role, or an unreachable role. Cost if wrong: a stale grant is used until the next reconcile.
  - **R28:** an effective Admin falls back to the owner connection plus `SET ROLE` for roles the member role can't reach (roles in 6a, plus `anon`/`authenticated`). Cost if wrong: Admins use the owner connection more often.
  - **R31 (user; supersedes the spec's "reassign to the owner"):** leftovers go to the member's last acted role (`member_branch_roles.last_acted_role`, migration `20261014000003`) if the member could still `SET ROLE` to it, then `DROP OWNED`; never to the owner. Cost if wrong: leftovers are lost when the acted role is gone.
  - Live tests: `sql_run_live::{member_role_holds_nothing_after_any_run, script_cannot_reset_to_owner, objects_created_after_reset_role_end_up_owned_by_the_acted_role, schema_read_follows_role_usage}`, `api::{developer_cancels_their_own_run_over_http, member_login_refused_forces_one_reconcile}`, `member_roles_live::a_reconcile_hands_leftovers_to_the_last_acted_role_then_drops_the_rest`, `members_live::branch_roles_and_project_branches`.
  - Studio must send `?role=` on schema reads (→ T12b).
- Done: **T7** `813ea14..ea2a016`, merged `4463812`.
  - The Zitadel v4.19.1 invite-code endpoints were verified present.
  - Review (opus): 4 important and 8 minor, all fixed (`cdeac2c..ea2a016`, verified by the coordinator).
  - Property test: `api::grant_invariant::effective_grants_always_pass_the_role_check`.
  - Live tests: `members_live::{member_api_reads, existing_user_is_added_active, put_grants_is_all_or_nothing, promoting_an_invited_member_conflicts, project_events_include_listed_grants}`.
  - For the end review: grant and override writes check and then write in separate statements, so a concurrent grant + override pair could each pass. The reconciler is the backstop.
  - **R29 (user):** any inviter may bring back a removed or deactivated person, always as `invited` with a fresh code. The log records the inviter. Cost if wrong: a project Admin can undo a removal, and it is visible in the log.
  - **R30 (superseded 2026-10-09 by the end-review fix I2):** 6a now records membership options (`neon_roles.member_no_set`, `member_admin`), so ADMIN-holding roles and chains are refused at grant time below Admin, like the other unsafe roles.
- Done: **T8** `8be8713..54ceecd`, merged `b231b6c` (review opus: 2 critical, 6 important, 7 minor, all fixed; scoped re-review: all 15 addressed, plus new N1 (critical), N2, N3 and 5 minors, fixed under R22, R23 and R21).
  - Invariants proven by property tests: **reach** (a member role's SET-closure equals the closure of its included granted roles, and contains no owner-level, LOGIN, ADMIN-holding or reserved role) and **staleness** (reset and restore forget rows and enqueue; the model checks passwords; 5-minute sweep plus retries).
  - Live tests (14, `services/worker/tests/member_roles_live.rs`): `an_owner_chain_made_in_sql_is_revoked_by_the_hook` needs the stack's worker running this branch.
  - Sweep env: `CHELABASE_MEMBER_ROLES_SWEEP_SECS`.
- Done: **T6** `b1fedcc..1df1185`, merged `0af35b1` (review opus: 5 important and 8 minor, all fixed under R21–R25).
  - Live: `sql_run_live::developer_cancels_their_own_run_over_http`, metadata `saved_query_round_trip`.
  - `branch_connection?role=` is now `Project(Admin)` (R22).
- Done: **T4b** `acd3abc`, merged `da6f209`.
- Done: **T10** `8be8713..dcbb1b5`, merged `7c5dde3` (review sonnet: 4 important and 9 minor, fixed under R13 and R14; scoped re-review: all addressed; R15 follow-up: one-use tickets, `no-store`, exact error keys).
- Done: **T11** `4eb0e20`, merged `45c6245` (no review of its own). Live check: run `just zitadel-bootstrap` twice; the second run logs no new `admin_set`.
- Done: **T5** `8be8713..473207f`, merged `b1fedcc` (review opus: 3 important and 8 minor, all fixed, including R16–R19). Live: `sql_run_live`, `sql_schema_live`. For T7: `/v1/instance/settings` uses the `InstancePrincipal` extractor. For T14: assert the dev user's member row has an email.
- **R20 (→ T6):** app-user routes apply the auth database's override.
- **R21 (user, 2026-10-09):** a granted role includes its chain; a chain reaching owner-level refuses that grant (T7) or excludes it (T8); act-as accepts roles reachable through the chain (T9). Cost if wrong: grants give more roles than they list; Studio shows the expanded set.
- **R22–R25 (2026-10-09):** the Postgres convention (user): NOLOGIN groups only. `CREATEDB` counts as owner-level. Saved queries are shared within the project. Merge retries with security steps need Admin. Follow-up step: a personal login per member, for `psql` as themselves. Cost if wrong: Developers lose login-role connection strings (by design).
  - T8 must also prove two invariants with property tests: *reach* (a member role can only become its desired, non-owner-level roles) and *staleness* (reset and restore enqueue a reconcile; the model checks passwords).
  - **R18:** when an access token lacks `email` or `name`, the orchestrator fetches them once from Zitadel's admin API. T7 and T11 match on the Zitadel user id first.
  - **R19:** `Instance` routes are authorized before tenant binding, so platform operators without a tenant can reach them.
  - **The `members` branch must not merge before T9:** until then a Developer's `run_sql` can `RESET ROLE` to the owner. T9 also adds `?role=` to the SQL schema route.
- **R16 (user, 2026-10-09; T8 review C1):** owner-level roles are Admin-only. Cost if wrong: a Developer can't be given a BYPASSRLS role. Follow-up (TE-A/TE-B): a "grant all rows" toggle for a role, using permissive `USING (true)` policies on every RLS table, with no owner-level attribute.
- **R17 (user, 2026-10-09; T8 review I4, "no fork change now"):** per-Postgres-database overrides are dropped. Per-database restriction is done with Postgres privileges. Follow-up for the next Neon fork round: a neon-extension hook that refuses `ALTER ROLE` on `cb_m_*` roles except by `cloud_admin`, and `cb_m_*` logins not from the orchestrator; then platform-level per-Postgres-database overrides can return. New task **T4b** (sonnet / sonnet) cleans up the merged T3 and T4 code. Cost if wrong: no platform-level per-Postgres-database narrowing.
- **R15:** the re-review's 5 new minors were fixed in one follow-up verified by the coordinator.
- Done: **T3** `c688c6f..791e0e7`, merged `8be8713` (review opus: 2 important and 9 minor, all fixed, including R7 and R8; the duplicate `proptest` workspace dependency was resolved in the merge). Live tests for the live pass: `members_live` (17).
- **R8:** the newest email claim wins. `members.email` is nullable, and other live holders are cleared under the tenant lock. Cost if wrong: an old member shows no email until their next sign-in.
- **R9 (→ T7):** promoting a non-active member to tenant admin is refused with `conflict`. Cost if wrong: an extra step for admins.
- **R13 (T10):** a signed 15-min accept ticket, returned with `invite_incomplete` after verification succeeded, makes the later steps retryable without the spent code. T12b's page retries with it. Cost if wrong: an extra request shape.
- **R14 (T10):** accept-invite requires the admin Zitadel client (admin PAT); without it, it answers 503 before verifying, with no silent fallback. Cost if wrong: self-hosters must configure the admin PAT, which the bootstrap already does.
- Done: **T4** `c688c6f..d44e88a`, merged `1a43c52` (review opus: 4 important and 6 minor, all fixed, including R6 and R7; verified by the coordinator).
  - For T5: `decide` takes `owner_role: Option<&str>` (pass the database owner on `ActAs` routes) and offers `Requested::from_name` and `act_as_reserved`.
  - **R10:** `anon` and `authenticated` stay usable by name; they still need a grant.
  - **R11:** `Role("postgres")` with no known owner is refused.
  - **R12:** non-active callers keep their membership answers on `Instance` routes.
- **R6:** `get_app_users` (a settings read) is `Project(Viewer)`. Cost if wrong: Viewers see the app-auth settings, which hold no secrets.
- **R7 (user, 2026-10-09: "Admin implies owner, as long as it still makes solid boundaries"):**
  - Effective Admin ⇒ owner.
  - `owner` is never stored in grants, and is a reserved role name, as is the database's real owner role.
  - `Requested::from_name` maps both to `Owner`.
  - `decide` refuses reserved names as roles.
  - A narrowing override removes owner.
  - Spec (M3, parts 1 and 6) and plan (Global Constraints, T3 CHECK, T7 validation, T12c test) amended.
  - Cost if wrong: there's no "Admin without owner" tier; the user chose that.
- Done: **T2** `ca90dbc..8970981`, merged `4f3ca66` (review opus: 3 important and 6 minor, fixed under R4b; minor 7 moved to T9 per R5; verified by the coordinator). Live test for the live pass: `catalog_live::member_roles_and_their_grants_are_invisible`.
- Done: **T1** `ca90dbc..57c89c8`, merged `c688c6f` (review sonnet: approve, 6 minors fixed, verified by the coordinator).
  - Deviations: `Forbidden` already existed, so there are 7 new codes (`ALL` = 82).
  - `AppState.mode` is replaced by `policy`.
  - `dev_login_redirects` was dropped because no decision uses it (R3).
  - `prehashed_passwords_allowed` is its own rule (R2).
- Rulings:
  - **R2:** pre-hashed passwords get their own `ModePolicy` rule. Cost if wrong: a rename.
  - **R3:** `dev_login_redirects` was deleted, since the CLI sets `developmentMode` in both modes. Cost if wrong: re-adding one method.
  - **R4b (T2; supersedes R4 after the user said every existing role and database is throwaway dev scratch):** both prefixes are reserved outright everywhere (create, rename, membership, delete, drop). Resync and the catalog hide both. There are no legacy allowances. Cost if wrong: none known.
  - **R5 (→ T9):** after each act-as run, `RESET ROLE; REASSIGN OWNED BY CURRENT_USER TO <acted role>`. Cost if wrong: objects stay owned by `cb_m_*` until the role is dropped.
- Ruling R1: T1 adds `policy: ModePolicy` to the orchestrator `AppState`; T5 adds only `operator_org` (cost if wrong: a trivial merge adjustment in T5).
- Pre-flight (checked on `members` at `38d5b39`):
  - `chelabase_routes::Endpoint` (`crates/chelabase-routes/src/lib.rs:98`) already has `policy: Policy { auth: Auth, cors: Cors }` (:93). `access` goes **into `Policy`**, not onto `Endpoint` directly. There are 94 `ENDPOINTS`: 72 orchestrator, 2 rest, 20 auth. The gateway decides JWT checks from `policy.auth` (`services/gateway/src/pipeline.rs:183`).
  - `load_owned` has 42 call sites: merges 12, branches 10, data_keys 6, saved 5, lib 2, console 2, roles 2, data_api_databases 1, sql_editor/routes 1.
  - `MetadataPort` (lib.rs:113) has 3 impls: real lib.rs:526, the doc stub openapi.rs:88 and the fake tests/api.rs:282. `JobPort` (lib.rs:954) also has 3 impls. A new method goes into all three.
  - `ErrorCode` (`crates/chelabase-types/src/api_error.rs:11`): add the variant, an `as_str` arm (:93) and an entry in the `ALL` test array (:272, count 75 → 83).
  - `Mode` call sites outside tests (they move into `ModePolicy`): auth `config.rs:50-62` (email verification, secure cookies); worker `config.rs:115-117` and `main.rs:67` (owner superuser); orchestrator `main.rs:35` and worker `config.rs:87` (`RoleSettings` password strength); CLI `dev/requests.rs:53-165` (dev redirects) and `dev/steps.rs:116` (platform leaves the Studio login alone); orchestrator lib.rs:2309 (CA file on reveal).
  - Metadata Postgres is `postgres:17.11-alpine`, so `NULLS NOT DISTINCT` is available. Migrations use runtime `sqlx::query` and are named `YYYYMMDD00000N_*.sql`; the latest is `20261013000001_merge_failed_online.sql`, so new ones start at `20261014000001`.
  - `Purpose` (`crates/chelabase-secrets/src/lib.rs:94`) is `{Owner, Authenticator, DataKey, Role}`. `seal_role_password` uses AAD `"{branch_id}:role:{role}"` (:156).
  - Worker RPCs: `role_rpc` (lib.rs:1089) on subjects `chelabase.rpc.<area>.<verb>` (`crates/chelabase-nats/src/lib.rs:303`), served by `queue_subscribe` (`services/worker/src/main.rs:93`). Resync is a core-NATS publish (`chelabase.resync.branch`, :274), consumed at `services/worker/src/main.rs:583`.
  - Hooks:
    - branch create finishes in `finish_branch` (`services/worker/src/branch_create.rs:567`, after :602);
    - 6c Postgres database upserts and removals arrive at `services/orchestrator/src/internal/ddl.rs` (`Action::UpsertDatabase` :923) and at the resync (`resync.rs:385-391`).
  - Auth: `sign_up_inner` (`services/auth/src/app.rs:386`); the internal client is `services/auth/src/app_users/internal.rs:92`; the internal router is `services/orchestrator/src/internal/mod.rs:360` (bearer `app_auth_token`); the auth page frame is `LoginLayout` (`apps/studio/src/components/login/sign-in-form.tsx:52`).
  - CLI: `DevCommands::Bootstrap` is a unit variant (`tools/cli/src/main.rs:61`). The tenant bind runs raw psql (`tools/cli/src/dev/tenant.rs:80`). `just zitadel-bootstrap` is at justfile:158.
  - Studio: `RailLink.id` is the union `"projects" | "api"`, with `placeholder-1` and `placeholder-2`. `ProjectTab` is `"databases" | "users" | "settings"` (`project-page.tsx:27`), and the tab lives in the URL via `lib/project-search.ts`.
  - **Not verified:** Zitadel v4.19.1's `POST /v2/users/{id}/invite_code` and `/invite_code/verify` (there's no proto in the repo). T7's first step verifies them against the v4.19.1 API docs or proto at that tag. **If they're missing, stop and ask the user.**
  - proptest 1.11.0 is the latest on crates.io (checked 2026-10-09). It's not yet a dependency.
- Pending fork changes: none (no fork is touched, so there's no image build).
- **Timing (user, 2026-10-09: record the estimate against reality):**
  - **Estimate, wall clock:** 12–17 h from dispatching T1 to the end-of-step pause. That's the critical path through the waves, excluding the user's T12a brief review.
  - The earlier "14–20 h" was summed agent time across tasks (parallel tasks counted twice), not wall clock.
  - **Start:** 2026-10-09T10:49:33+07:00 (T1 and T2 dispatched).
  - Wave 3 + T6 + T4b (T5, T8, T10, T11, T6): 11:31–13:29, about 2 h, including user decisions R16–R22 that re-opened T3, T4, T5 and T8.
  - **Pauses:** a /clear checkpoint at 13:30 (the user resumes; the gap until the next dispatch is subtracted).
  - **Finish:** _(at the end-of-step pause)_.
  - **Actual, wall clock minus pauses:** _(computed at finish)_.
  - **Per wave:** wave 1 (T1 ∥ T2) 10:49 → T1 done about 11:00, T2 in its fix round at 11:07 (about 15–20 min against about 60 min budgeted).
  - Wave 2 (T3 ∥ T4): 10:59–11:30, about 31 min including reviews and fix rounds.
  - **Revised estimate (11:07):** 4–7 h wall clock to the end-of-step pause, excluding the T12a brief review. The original 12–17 h ran about 3× high on wave 1. Most of the remaining risk is in the opus tasks (T5, T8, T7, T9) and the live pass.

## Task table (binding)

| Task | What | Implementer | Reviewer | Runs |
|---|---|---|---|---|
| T1 | `ModePolicy` (chelabase-types), with every `Mode` call site moved to it; the 8 new `ErrorCode`s; `chelabase_types::access::PlatformRole` (only this enum; T4 fills in the module) | sonnet | sonnet | wave 1, parallel with T2 |
| T2 | Reserved prefixes `cb_m_` and `chelabase_` in `role_name`, `RESYNC_SQL`, chelabase-schema `exclusions.rs`, `catalog_sql.sql` (roles and ACL grantees); the pinned list tests | sonnet | opus | wave 1, parallel with T1 |
| T3 | Metadata migrations and `chelabase-metadata` API (members, grants, overrides, invites, member_branch_roles, access_events, `tenants.open_signup`); last-admin and first-claim invariants; the concurrent model test | opus | opus | wave 2, after T1 merges, parallel with T4 |
| T4 | `chelabase_types::access` (`PgRole`, `RoleSet`, `Grant`, `Narrowing`, `effective`, `Access`, `decide`); `Policy.access` on all 94 endpoints; the exhaustive decision test; the grant-algebra proptest | opus | opus | wave 2, after T1 merges, parallel with T3 |
| T8 | Worker member-role reconciler (RPC and async subject), `Purpose::MemberRole`, hooks (branch finish, 6a role create/delete, 6c database upsert/remove), the live convergence and escape tests | opus | opus | wave 3, parallel with T5 and T10 |
| T5 | Orchestrator `AuthPrincipal` → member resolution; `authorize` + `Authorized` replace all 42 `load_owned` calls; `/v1/me` capabilities; the route-walk test | opus | opus | wave 3, parallel with T8 and T10 |
| T4b | R17 cleanup in the merged code: drop `pg_database` from overrides and `member_branch_roles` (edit migration `20261014000001` in place) and from `chelabase-metadata` members; `effective` takes two levels (`chelabase_types::access`), with tests updated | sonnet | sonnet (coordinator verifies if the diff is mechanical) | before the T5 and T8 fix rounds |
| T10 | Auth service: sign-up gate (`may_sign_up` internal route) and `POST /auth/v1/accept-invite`; login OpenAPI and login client regenerated | sonnet | sonnet | wave 3, parallel with T5 and T8 |
| T11 | CLI `dev bootstrap --admin-email`; `just zitadel-bootstrap` passes it | sonnet | none (end-of-step review) | after T3, whenever a build slot is free |
| T6 | Route-specific rules: console Viewer select-only, SQL-editor cancel by starter, saved-query author rule, keys bound to allowed roles, merge security acknowledgement needs Admin, project list filter, database create under a project | sonnet | opus | wave 4, after T5 |
| T7 | Member management API: members, grants, overrides, invites (Zitadel invite codes, plus the mock), access events, instance settings; reconcile enqueued on changes; `just api-gen` | opus | opus | wave 5, parallel with T9 |
| T9 | Act-as connections: the SQL editor runs and schema reads go through the member role (lazy reconcile, `MemberRoleReconcileFailed`); `grant_to_owner` removed | opus | opus | wave 5, parallel with T7 |
| T12a | Impeccable Operate-mode `context` and `shape` for the Members surfaces → brief to the user (pause), plus the component questions (Studio or lagoon) | opus | none (the user reviews the brief) | after T7 |
| T12x | (added 2026-10-09, user) API for the Members UI: `Member.has_access`/`created_at`, names on access events, describe-invite keyed by a Chelabase token (R32) | sonnet | sonnet | after T12a, parallel with T9's last fix round |
| T12y | (added 2026-10-09, user) R34 self-hosted is invite-only (sign-up removed end to end, incl. Studio's `/register`); R35 link invites (`delivery`, `invite_url`) and SMTP detection; R36 tenant display names (backend) | sonnet | opus | after T12x merges, before T12b |
| T12b | Studio: the Members rail item and page (People, Activity, Instance), the "no access yet" screen, `/accept-invite`, capability hiding across Studio | opus | sonnet | after the user approves the brief |
| T12c | Studio: the project Access tab (grants, overrides, project invites, project activity) | sonnet | sonnet | after T12b |
| T13 | Docs: Using pages, plus Reference pages generated from `ENDPOINTS` and `ModePolicy`; the new error codes | sonnet | none (end-of-step review) | parallel with T12b |
| T14 | `scripts/members-e2e.sh` and `just members-e2e`; the existing e2e scripts sign in as the bootstrap admin | sonnet | none (the live pass exercises it) | after T12c |
| End | Whole-branch review | — | opus | after T1–T14 |
| Fixes | End-of-step review fixes | sonnet | coordinator verifies the diff | after the review |

Then: the integration, `just check` once, no image build, and one live pass through `just live`:
- Zitadel's first-instance setup and `just zitadel-bootstrap --admin-email` first;
- then the orchestrator, worker and auth ignored live tests, `members-e2e`, and `console-e2e`, `sql-editor-e2e`, `merge-e2e`, `role-e2e`, `branch-e2e`, `data-e2e`, `app-users-e2e`;
- move the stack back afterwards;

and then the user's pause.

**Parallel worktrees:** each wave's tasks get `just wt-add members-tN members`, are reviewed there, merged back `--no-ff`, and removed. At most 3 builds at once.
- **Expected conflicts:** `MetadataPort` and its fakes (T5/T6/T7/T9), the `ErrorCode` list, `Cargo.lock`.
- **Merging:** the second merge resolves them by keeping both sides, then runs `cargo check --workspace` before committing.

**Claude-time estimate:**
- tasks: about 14–20 h of agent time (opus tasks 1–2 h each including review and fixes; sonnet tasks 30–60 min);
- end-of-step review and fixes: about 2 h;
- `just check` and the live pass: about 1.5 h.

## Global Constraints

- **Amendments R16 and R17 override any earlier text in this plan:**
  - **R16:** owner-level roles are Admin-only. A role that reaches the owner, `neon_superuser` or `cloud_admin` through memberships, or has SUPERUSER, REPLICATION, CREATEROLE or BYPASSRLS, is never granted to a member role. Act-as into it needs Admin. Grants naming it are refused unless the grant is Admin.
  - **R17:** no per-Postgres-database overrides. `database_grant_overrides` has no `pg_database`. `effective(project, database)` takes two levels. Member roles are one per (member, branch): `cb_m_<member uuid hex32>`. `member_branch_roles` has no `pg_database`. There is no reconcile on Postgres database create, drop or rename.
  - **R21 (user, 2026-10-09):** granting a role includes everything it can `SET ROLE` into (its chain). A granted role whose chain reaches anything owner-level is refused at grant time (T7: `invalid_request`, naming the role and the chain) and excluded by the reconciler (T8). Act-as (T9, and the console's named-role path) accepts a role reachable through a granted role's chain; it uses the 6a metadata `member_of` closure, and Postgres enforces the same at `SET ROLE`.
  - **R22 (user):** grants and act-as for non-Admins involve NOLOGIN group roles only. LOGIN roles, and chains reaching LOGIN roles or holding ADMIN, are excluded or refused. Login-role connection strings (`branch_connection?role=`) are `Project(Admin)`. Keys bound to LOGIN roles need Admin.
  - **R23:** `CREATEDB` counts as owner-level. A chain through a reserved role excludes the granted role.
  - **R24:** saved queries are shared within the project. Edit and delete belong to the author or an Admin; anyone else gets 403.
  - **R25:** a merge retry that re-runs security steps needs Admin. The Admin's apply is the acknowledgement. Patching acknowledgements is Admin-only.
- **Names:**
  - member roles: `cb_m_<member uuid simple hex, 32>_<first 8 hex of sha256(pg database name)>`;
  - reserved prefixes: `cb_m_` and `chelabase_`, case-insensitive;
  - **R7: Admin implies the database owner.** `owner` is never stored in grants; `owner` and the database's real owner role name are reserved role names, resolve to `Requested::Owner` (`Requested::from_name`) and need effective Admin.
- **Platform roles are ordered** Viewer < Developer < Admin. A tenant admin's effective grant on every project is Admin with every role (and so the owner).
- **Effective grant = project ∩ Chelabase-database override (R17: no Postgres-database level).** The platform role is the lower of the two, and the role set is their intersection. A missing override doesn't narrow. Owner is effective exactly when the effective role is Admin.
- **Answers:**
  - no grant: 404 `not_found`;
  - too low or role not allowed: 403 `forbidden` with `extra.required`;
  - member with no grants anywhere: 403 `no_access_yet`;
  - platform gap: 403 `not_available_in_platform_mode`.
- **New `ErrorCode`s**, exactly these snake_case names: `forbidden`, `no_access_yet`, `not_available_in_platform_mode`, `sign_up_closed`, `last_tenant_admin`, `invite_expired`, `invite_revoked`, `member_role_reconcile_failed`.
- **Platform mode fails closed.** Self-sign-up is refused, and `Instance` routes need membership in `CHELABASE_OPERATOR_ORG_ID`; when that's unset, nobody passes.
- **Member roles:**
  - attributes `LOGIN NOINHERIT NOCREATEDB NOCREATEROLE NOBYPASSRLS NOREPLICATION`;
  - memberships `WITH INHERIT FALSE, SET TRUE, ADMIN FALSE`;
  - never a member of the owner;
  - no `neon_roles` row;
  - applied only via `admin_sql` (`forward_ddl=off`).
- **Every access change writes exactly one `access_events` row in the same transaction.** `access_events` refuses UPDATE and DELETE.
- **No caching of grants:** every request reads them.
- **Strict TDD** (the preamble). Never `cargo fmt` the workspace. Implementers never run the dev stack; live tests compile with `--no-run` and are listed in the report.
- **No Supabase mirroring:** the shapes here are ours.

## Review Focus

1. **A member removed while a SQL-editor run is open:** the run's session is terminated, and the next request gets 404/403. *Test:* T8 `revoke_terminates_member_sessions` (live), plus T5 `removed_member_is_refused_with_a_valid_token`.
2. **A grant naming a Postgres role that doesn't exist on a branch** (dropped, or never created there): the reconcile skips it without failing, and act-as as that role returns 403 `forbidden`. *Tests:* T8 `reconcile_skips_roles_missing_on_the_branch`, T9 `act_as_missing_role_is_forbidden`.
3. **An invite email with different case or whitespace than an existing member** (`" Bob@X.com"` vs `bob@x.com`): it matches the existing member (emails stored trimmed and lowercased) rather than creating a second Zitadel user. *Test:* T7 `invite_matches_existing_member_case_insensitively`.
4. **Two tenant admins demoting each other at the same moment:** exactly one succeeds, and the other gets `last_tenant_admin`. *Test:* T3 `concurrent_demotions_keep_an_admin` (part of the model test).
5. **A child branch created before any reconcile ran:** it inherits the parent's `cb_m_*` roles and passwords. Its roles get fresh passwords and memberships before first use, and the parent's sealed password never opens a child session. *Test:* T8 `child_branch_gets_fresh_member_passwords` (live).

---

### Task T1: `ModePolicy` and the new error codes

**Spec:** part 2 ("`ModePolicy`"), part 7 ("New `ErrorCode`s").

**Files:**
- Create: `crates/chelabase-types/src/mode_policy.rs` (+ `pub mod` in `lib.rs`).
- Modify: `crates/chelabase-types/src/api_error.rs`, `services/auth/src/config.rs`, `services/worker/src/config.rs`, `services/worker/src/main.rs`, `services/orchestrator/src/main.rs`, `services/orchestrator/src/lib.rs:2309`, `crates/chelabase-types/src/role_settings.rs`, `tools/cli/src/dev/requests.rs`, `tools/cli/src/dev/steps.rs`.

**Interfaces (produces):**
```rust
pub enum SignUpRule { GatedByTenant, Refused }   // self-hosted: GatedByTenant; platform: Refused (M15)
#[derive(Clone, Copy, Debug)]
pub struct ModePolicy { mode: Mode }
impl ModePolicy {
    pub fn new(mode: Mode) -> Self;
    pub fn mode(&self) -> Mode;
    pub fn instance_admin(&self, active_tenant_admin: bool, in_operator_org: bool) -> bool; // SH: active_tenant_admin; P: in_operator_org
    pub fn sign_up(&self) -> SignUpRule;
    pub fn open_signup_switchable(&self) -> bool;            // SH: true; P: false
    pub fn email_verification_default(&self) -> bool;        // SH: false; P: true
    pub fn secure_cookies_default(&self) -> bool;            // SH: false; P: true
    pub fn owner_superuser_allowed(&self) -> bool;           // SH: true; P: false
    pub fn password_strength_forced(&self) -> bool;          // SH: false; P: true
    pub fn ca_file_on_reveal(&self) -> bool;                 // SH: true; P: false
    pub fn dev_login_redirects(&self) -> bool;               // SH: true; P: false
    pub fn cli_manages_studio_login(&self) -> bool;          // SH: true; P: false
    pub const RULES: &'static [&'static str];                // rule names, in order, for the docs table (T13)
    pub fn describe(&self) -> Vec<(&'static str, String)>;   // (rule, this mode's value) for the docs table
}
```
New `ErrorCode` variants: `Forbidden`, `NoAccessYet`, `NotAvailableInPlatformMode`, `SignUpClosed`, `LastTenantAdmin`, `InviteExpired`, `InviteRevoked`, `MemberRoleReconcileFailed`.

Also create `crates/chelabase-types/src/access.rs` with only:
```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]   // match how ErrorCode gates ToSchema
#[serde(rename_all = "snake_case")]
pub enum PlatformRole { Viewer, Developer, Admin }
impl PlatformRole { pub fn as_str(&self) -> &'static str; pub fn parse(s: &str) -> Option<Self>; }
```
Test: `access::tests::platform_role_order_and_round_trip` (Viewer < Developer < Admin; `parse(as_str(r)) == Some(r)`; serde snake_case).

**Behaviour:**
- Every existing `mode == Mode::Platform` / `SelfHosted` decision outside tests calls the matching `ModePolicy` method. Behaviour is unchanged, including the env overrides in auth `config.rs`, which still win over the default.
- After the change, `rg "Mode::(Platform|SelfHosted)" --type rust` outside `mode.rs`, `mode_policy.rs` and tests returns nothing.

**Tests (write first):**
- `mode_policy::tests::every_rule_per_mode`: a table of (rule, self-hosted value, platform value) covering every method, checked for both modes.
- `mode_policy::tests::describe_lists_every_rule`: `describe()` names equal `RULES`, with no duplicates.
- `mode_policy::tests::platform_fails_closed`: platform `sign_up() == Refused`, and `instance_admin(true, false) == false`.
- `api_error` `ALL` updated (83 entries); the serde round trip covers the new codes.
- The existing auth `config.rs` tests still pass unchanged.

**Verify:** `cargo test -p chelabase-types -p auth -p worker -p orchestrator --lib`, `cargo check --workspace`.

- [ ] RED: the new tests fail. GREEN. Commit `members T1: ModePolicy and access error codes`. Push.

### Task T2: Reserved member-role and `chelabase_` prefixes

**Spec:** part 3 ("Hidden everywhere").

**Files:**
- `crates/chelabase-types/src/role_name.rs`
- `services/worker/src/resync.rs` (`RESYNC_SQL` :375-383 and its test :1970)
- `crates/chelabase-schema/src/exclusions.rs`
- `crates/chelabase-schema/src/catalog_sql.sql` (the `reserved` CTE :33-39, the ACL-grantee filters around :57-87 and :239)
- `crates/chelabase-schema/src/catalog.rs` (test `catalog_sql_names_every_exclusion` :906)
- `services/worker/src/schema_base.rs` (test `exclusion_lists_match_worker_constants` :1633)

**Interfaces (produces):** `pub const MEMBER_ROLE_PREFIX: &str = "cb_m_";` and `pub fn is_member_role(name: &str) -> bool` in `chelabase_types::role_name`. `exclusions.rs` gets a `MEMBER_ROLE_PREFIX` constant equal to the types one.

**Behaviour:**
- `is_reserved_for`, `is_reserved_target` and `is_membership_reserved` all refuse names starting with `cb_m_` or `chelabase_`, case-insensitively. **The `chelabase_` prefix in `is_reserved_for` is the bug fix:** today SQL `CREATE ROLE chelabase_x` passes.
- `RESYNC_SQL` adds `AND a.rolname !~* '^(cb_m_|chelabase_)'`.
- In `catalog_sql.sql`:
  - the `reserved` CTE adds `lower(rolname) LIKE 'cb\_m\_%'`;
  - every aclexplode grantee filter drops grantees matching either prefix, so the 6g merge never sees member roles or grants to them.

**Tests (write first):**
- `role_name::tests::reserved_prefixes_exhaustive`: for each of the three predicates × {`cb_m_x`, `CB_M_x`, `chelabase_x`, `Chelabase_X`, `cbm_x`, `chelabasex`, `billing`}, expect refused / refused / refused / refused / allowed / allowed / allowed.
- `resync::tests::resync_sql_skips_reserved_prefixes`: the SQL text contains the prefix rule.
- `catalog_sql_names_every_exclusion` and `exclusion_lists_match_worker_constants` extended to the new prefix.
- Live, `#[ignore]`, compile only: `crates/chelabase-schema/tests/catalog_live.rs::member_roles_and_their_grants_are_invisible`. It creates `cb_m_test_x`, grants it a membership and a table SELECT, reads the catalog, and expects neither the role nor the grant.

**Verify:** `cargo test -p chelabase-types -p chelabase-schema -p worker --lib`, then `cargo test -p chelabase-schema --test catalog_live --no-run`.

- [ ] RED, GREEN, commit `members T2: reserve cb_m_ and chelabase_ role prefixes`, push.

### Task T3: Metadata schema and API

**Spec:** part 1 (all), part 5 (the first-admin claim, invited → active).

**Files:**
- Create: `crates/chelabase-metadata/migrations/20261014000001_members.sql`, `crates/chelabase-metadata/src/members.rs` (+ `mod` in `lib.rs`).
- Test: `crates/chelabase-metadata/tests/members_live.rs` (`#[ignore = "needs the metadata Postgres (just deps-up)"]`, scratch database as in the existing live tests).

**Migration (pinned):**
```sql
ALTER TABLE tenants ADD COLUMN open_signup boolean NOT NULL DEFAULT false;
CREATE TABLE members (
  id uuid PRIMARY KEY, tenant_id uuid NOT NULL REFERENCES tenants(id),
  zitadel_user_id text NOT NULL, email text NOT NULL CHECK (email = lower(btrim(email))),
  display_name text NOT NULL DEFAULT '', tenant_admin boolean NOT NULL DEFAULT false,
  status text NOT NULL CHECK (status IN ('invited','active','removed')),
  invited_by uuid REFERENCES members(id), accepted_at timestamptz,
  created_at timestamptz NOT NULL DEFAULT now(), updated_at timestamptz NOT NULL DEFAULT now(),
  UNIQUE (tenant_id, zitadel_user_id));
CREATE UNIQUE INDEX members_live_email ON members (tenant_id, email) WHERE status <> 'removed';
CREATE TABLE project_grants (
  member_id uuid NOT NULL REFERENCES members(id), project_id uuid NOT NULL REFERENCES projects(id),
  platform_role text NOT NULL CHECK (platform_role IN ('viewer','developer','admin')),
  pg_roles text[] NOT NULL DEFAULT '{}',
  CHECK (NOT ('owner' = ANY(pg_roles))),   -- R7: owner is implied by Admin, never stored (same CHECK on database_grant_overrides.pg_roles)
  granted_by uuid REFERENCES members(id), created_at timestamptz NOT NULL DEFAULT now(), updated_at timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (member_id, project_id));
CREATE TABLE database_grant_overrides (
  member_id uuid NOT NULL REFERENCES members(id), database_id uuid NOT NULL REFERENCES databases(id),
  pg_database text, platform_role text CHECK (platform_role IN ('viewer','developer','admin')),
  pg_roles text[], set_by uuid REFERENCES members(id),
  created_at timestamptz NOT NULL DEFAULT now(), updated_at timestamptz NOT NULL DEFAULT now(),
  CHECK (platform_role IS NOT NULL OR pg_roles IS NOT NULL),
  UNIQUE NULLS NOT DISTINCT (member_id, database_id, pg_database));
CREATE TABLE invites (
  id uuid PRIMARY KEY, member_id uuid NOT NULL REFERENCES members(id), zitadel_user_id text NOT NULL,
  invited_by uuid NOT NULL REFERENCES members(id), expires_at timestamptz NOT NULL,
  revoked_at timestamptz, accepted_at timestamptz, created_at timestamptz NOT NULL DEFAULT now());
CREATE TABLE member_branch_roles (
  member_id uuid NOT NULL REFERENCES members(id), branch_id uuid NOT NULL REFERENCES branches(id) ON DELETE CASCADE,
  pg_database text NOT NULL, role_name text NOT NULL, password_enc text NOT NULL,
  applied_roles text[] NOT NULL DEFAULT '{}', applied_at timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (member_id, branch_id, pg_database), UNIQUE (branch_id, role_name));
CREATE TABLE access_events (
  id bigserial PRIMARY KEY, tenant_id uuid NOT NULL REFERENCES tenants(id),
  actor_member_id uuid REFERENCES members(id), action text NOT NULL,
  target_member_id uuid REFERENCES members(id), project_id uuid, database_id uuid,
  before jsonb, after jsonb, at timestamptz NOT NULL DEFAULT now());
CREATE INDEX access_events_tenant_at ON access_events (tenant_id, at DESC);
CREATE FUNCTION access_events_append_only() RETURNS trigger LANGUAGE plpgsql AS
  $$BEGIN RAISE EXCEPTION 'access_events is append-only'; END$$;
CREATE TRIGGER access_events_no_change BEFORE UPDATE OR DELETE ON access_events
  FOR EACH ROW EXECUTE FUNCTION access_events_append_only();
```
`actor_member_id` is NULL only for system actions (the bootstrap, the first claim). `action` values: `member_joined`, `admin_claimed`, `admin_set`, `admin_unset`, `member_removed`, `grant_put`, `grant_deleted`, `override_put`, `override_deleted`, `invite_created`, `invite_resent`, `invite_revoked`, `invite_accepted`, `open_signup_set`.

**Interfaces (produces, in `chelabase_metadata::members`; all `async`, `&self` on `MetadataClient`):**
```rust
pub struct MemberRecord { pub id: Uuid, pub tenant_id: TenantId, pub zitadel_user_id: String, pub email: String,
  pub display_name: String, pub tenant_admin: bool, pub status: MemberStatus, pub accepted_at: Option<DateTime<Utc>> }
pub enum MemberStatus { Invited, Active, Removed }
pub struct GrantRow { pub platform_role: PlatformRole, pub pg_roles: Vec<String> }        // PlatformRole from chelabase_types::access (T1)
pub struct OverrideRow { pub platform_role: Option<PlatformRole>, pub pg_roles: Option<Vec<String>> }
pub struct GrantRows { pub project: Option<GrantRow>, pub database: Option<OverrideRow>, pub pg_database: Option<OverrideRow> }
pub struct MemberBranchRole { pub member_id: Uuid, pub branch_id: Uuid, pub pg_database: String, pub role_name: String,
  pub password_enc: String, pub applied_roles: Vec<String> }
pub enum AccessWriteError { LastTenantAdmin, NotFound, Conflict(String), Db(sqlx::Error) }

fn member_by_zitadel(tenant: TenantId, zitadel_user_id: &str) -> Result<Option<MemberRecord>>;
fn ensure_member(tenant: TenantId, zitadel_user_id: &str, email: &str, name: &str) -> Result<MemberRecord>;
   // one tx, tenant row locked FOR UPDATE: insert active member if none (tenant_admin = no other active admin → event
   // admin_claimed, else member_joined); invited → active + accepted_at (+ invites.accepted_at, event invite_accepted);
   // removed stays removed (returned as is); refreshes email/name
fn grant_rows(member: Uuid, project: Uuid, database: Option<Uuid>, pg_database: Option<&str>) -> Result<GrantRows>; // one query
fn member_projects(member: Uuid) -> Result<Vec<(Uuid, GrantRow)>>;
fn list_members(tenant: TenantId) -> Result<Vec<MemberRecord>>;
fn active_tenant_admins(tenant: TenantId) -> Result<Vec<MemberRecord>>;
fn tenant_has_members(tenant: TenantId) -> Result<bool>;
fn set_tenant_admin(actor: Option<Uuid>, member: Uuid, admin: bool) -> Result<(), AccessWriteError>;   // locks tenant; LastTenantAdmin
fn remove_member(actor: Uuid, member: Uuid) -> Result<Vec<Uuid /*project ids touched*/>, AccessWriteError>; // locks tenant; deletes grants+overrides
fn put_project_grant(actor: Uuid, member: Uuid, project: Uuid, grant: &GrantRow) -> Result<(), AccessWriteError>;
fn delete_project_grant(actor: Uuid, member: Uuid, project: Uuid) -> Result<(), AccessWriteError>;
fn put_override(actor: Uuid, member: Uuid, database: Uuid, pg_database: Option<&str>, o: &OverrideRow) -> Result<(), AccessWriteError>;
fn delete_override(actor: Uuid, member: Uuid, database: Uuid, pg_database: Option<&str>) -> Result<(), AccessWriteError>;
fn create_invited_member(actor: Uuid, tenant: TenantId, zitadel_user_id: &str, email: &str, expires_at: DateTime<Utc>,
   grants: &[(Uuid, GrantRow)]) -> Result<(MemberRecord, Uuid /*invite id*/), AccessWriteError>;
fn invite(invite: Uuid) -> Result<Option<(Uuid /*member*/, DateTime<Utc>, Option<DateTime<Utc>> /*revoked*/, Option<DateTime<Utc>> /*accepted*/)>>;
fn resend_invite(actor: Uuid, invite: Uuid, expires_at: DateTime<Utc>) -> Result<(), AccessWriteError>;
fn revoke_invite(actor: Uuid, invite: Uuid) -> Result<String /*zitadel user id to delete*/, AccessWriteError>; // member → removed
fn open_signup(tenant: TenantId) -> Result<bool>;
fn set_open_signup(actor: Uuid, tenant: TenantId, on: bool) -> Result<(), AccessWriteError>;
fn access_events(tenant: TenantId, project: Option<Uuid>, before_id: Option<i64>, limit: u32) -> Result<Vec<AccessEvent>>;
fn bootstrap_admin(tenant: TenantId, zitadel_user_id: &str, email: &str) -> Result<MemberRecord>; // upsert active + admin; event admin_set, actor NULL
fn member_branch_roles(branch: Uuid) -> Result<Vec<MemberBranchRole>>;
fn upsert_member_branch_role(r: &MemberBranchRole) -> Result<()>;
fn delete_member_branch_role(member: Uuid, branch: Uuid, pg_database: &str) -> Result<()>;
fn project_branches(project: Uuid) -> Result<Vec<Uuid>>;                     // every live branch of every database of the project
```
`AccessEvent` mirrors the table's columns.

**Behaviour:**
- Emails are stored `lower(btrim(...))`.
- `ensure_member` is called on every request (T5), so its fast path is a single read with no lock and no write when the member is active and the email and name are unchanged. The tenant row is locked only to insert a member or activate an invited one. Changed claims are refreshed with a plain UPDATE that writes no event.
- A grant or override whose `pg_roles` contains `owner` returns `Conflict`, not a raw CHECK error (R7).
- Every write function inserts its `access_events` row in its transaction. `before` and `after` are JSON of the grant, override or flag, never secrets.

**Tests (write first):**
- Unit tests for row ↔ struct mapping.
- Live (`--no-run` for the implementer):
  - `members_live::first_claim_is_atomic`: 20 concurrent `ensure_member` calls for different users on an empty tenant; exactly one is `tenant_admin`.
  - `members_live::concurrent_demotions_keep_an_admin`: two admins demote each other concurrently; one succeeds, the other gets `LastTenantAdmin`.
  - `members_live::last_admin_model`: a proptest (64 cases) of random sequences over {add, demote, promote, remove, claim} with 3 concurrent workers per step. After every step there's ≥ 1 active admin whenever any member is active, and every failure is `LastTenantAdmin`.
  - `members_live::access_events_are_append_only`: UPDATE and DELETE raise.
  - `members_live::owner_needs_admin`: putting `owner` with developer returns `Conflict`.
  - `members_live::invited_becomes_active_on_ensure`, `members_live::removed_stays_removed`.
  - `members_live::override_unique_with_null_pg_database`: two NULL-`pg_database` overrides for the same (member, database) conflict.
  - `members_live::every_write_logs_one_event`: each write function adds exactly one event.

**Verify:** `cargo test -p chelabase-metadata --lib`, then `cargo test -p chelabase-metadata --test members_live --no-run`.

- [ ] RED, GREEN, commit `members T3: metadata for members, grants, invites and access events`, push.

### Task T4: The access model and the route policy table

**Spec:** part 2 (all), part 4 (the table), part 7 (invariant tests 1 and 3).

**Files:**
- Modify: `crates/chelabase-types/src/access.rs` (created by T1 with `PlatformRole` only).
- Modify: `crates/chelabase-routes/src/lib.rs` (`Policy` gains `access`, and all 94 `ENDPOINTS` are annotated), `crates/chelabase-routes/Cargo.toml` (depends on chelabase-types, if not already), workspace `Cargo.toml` (`proptest = "1.11.0"` in `[workspace.dependencies]`).

**Interfaces (produces):**
```rust
// PlatformRole already exists (T1).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PgRole { Owner, Named(String) }          // "owner" ↔ Owner when parsing grant rows
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RoleSet { All, Only(BTreeSet<PgRole>) }  // All: a tenant admin may act as any role
impl RoleSet { pub fn contains(&self, r: &PgRole) -> bool; pub fn intersect(&self, other: &RoleSet) -> RoleSet; }
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Grant { pub platform_role: PlatformRole, pub roles: RoleSet }
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Narrowing { pub platform_role: Option<PlatformRole>, pub roles: Option<BTreeSet<PgRole>> }
pub fn effective(project: &Grant, database: Option<&Narrowing>, pg_database: Option<&Narrowing>) -> Grant; // drops Owner unless Admin
pub fn tenant_admin_grant() -> Grant;              // Admin + RoleSet::All
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access { Public, DataKey, SignedIn, Instance, TenantMember, TenantAdmin,
  Project(PlatformRole), Branch { unprotected: PlatformRole, protected: PlatformRole }, ActAs(PlatformRole) }
pub enum MemberFacts { NotMember, Invited, Removed, Active { tenant_admin: bool, has_any_grant: bool } }
pub struct CallerFacts { pub member: MemberFacts, pub in_operator_org: bool }
pub enum Requested<'a> { Owner, Role(&'a str) }
pub enum Deny { NotFound, Forbidden { required: String }, NoAccessYet, NotAvailableInPlatformMode }
pub fn decide(policy: &ModePolicy, caller: &CallerFacts, grant: Option<&Grant>, branch_protected: Option<bool>,
              requested: Option<Requested<'_>>, access: Access) -> Result<(), Deny>;
```
**`RoleSet` rules:**
- `All ∩ X = X`.
- `RoleSet::All.contains(Owner)` is true only when the grant is Admin, which `effective` guarantees because only Admin grants are `All`.
- T5, T8 and T9 use these exact types.

**`decide` rules (normative):**
- `Public` and `DataKey`: always `Ok`. The gateway authenticated keys already.
- `SignedIn`: `Ok`.
- If the caller isn't an active member, every other variant returns: `Removed` → `NotFound`; `NotMember`/`Invited` → `NoAccessYet`, except in platform mode → `NotAvailableInPlatformMode`.
- `Instance`: `policy.instance_admin(tenant_admin, in_operator_org)`, else `Forbidden{"instance admin"}`.
- `TenantMember`: `Ok`. `TenantAdmin`: requires `tenant_admin`.
- `Project(min)`, `Branch{..}` and `ActAs(min)` with `grant == None`: `NotFound`, or `NoAccessYet` if `!has_any_grant`.
- `Branch`: `min = if branch_protected.unwrap_or(true) { protected } else { unprotected }`.
- `ActAs(min)`: also `requested` must be in `grant.roles`. `Owner` requires Admin. A missing `requested` means `Owner`.
- `Forbidden.required` strings: `"viewer"`, `"developer"`, `"admin"`, `"role:<name>"`, `"owner"`, `"tenant admin"`, `"instance admin"`.

**Endpoint annotation:**
- Every orchestrator endpoint gets the `Access` from the spec's part 4 table.
- Auth endpoints: `Public`.
- rest endpoints: `DataKey`.
- Routes whose rule refines inside the handler (cancel by starter, saved-query author, keys bound to a role) use the table's base level; T6 adds the refinement.
- New routes (T7, T10) are added in those tasks with their `Access`.

**Tests (write first):**
- `access::tests::decide_matrix_exhaustive`: iterates every `Access` value (with each `PlatformRole` inside) × {SelfHosted, Platform} × caller kinds {operator-only, tenant admin, Admin, Developer, Viewer, active without grant, active with no grants anywhere, invited, removed, non-member} × `branch_protected` ∈ {None, Some(false), Some(true)} × requested ∈ {None, Owner, allowed `billing`, other `sales`}. Expected results come from an independent `expected()` written as a lookup table in the test, **not** by calling into `decide`'s helpers.
- `access::tests::grant_algebra` (proptest): for random `Grant`s and optional `Narrowing`s, `effective`:
  - never has a platform role above any present level;
  - its roles ⊆ each present level's roles;
  - equals the intersection;
  - contains `Owner` only with Admin;
  - is idempotent (`effective(effective(..)) == effective(..)`).
- `chelabase_routes::tests::auth_and_access_agree`: `Auth::Public` ⇔ `Access::Public`; `Auth::*DataKey` ⇔ `Access::DataKey`; `Auth::Management` ⇔ every other variant.
- `chelabase_routes::tests::spec_table`: a fixed list of (endpoint name → `Access`) copied from the spec's part 4 table, checked against `ENDPOINTS`.

**Verify:** `cargo test -p chelabase-types -p chelabase-routes`, `cargo check --workspace`.

- [ ] RED, GREEN, commit `members T4: access model and route policy`, push.

### Task T8: Member-role reconciler (worker)

**Spec:** part 3 (all), part 7 (invariant tests 6 and 7).

**Files:**
- Create: `services/worker/src/member_roles.rs`.
- Modify: `crates/chelabase-nats/src/lib.rs`, `crates/chelabase-secrets/src/lib.rs`, `services/worker/src/main.rs`, `services/worker/src/branch_create.rs` (after :602), `services/worker/src/roles.rs` (after a successful create/delete), `services/worker/src/resync.rs` (when a `pg_database` appears or disappears), `services/orchestrator/src/internal/ddl.rs` (enqueue after `UpsertDatabase`/`RemoveDatabase`).
- Test: `services/worker/tests/member_roles_live.rs` (`#[ignore]`).

**Interfaces (produces):**
```rust
// chelabase-nats
pub const SUBJECT_MEMBER_ROLES_RECONCILE: &str = "chelabase.rpc.member_roles.reconcile";   // sync RPC
pub const SUBJECT_MEMBER_ROLES_ENQUEUE: &str = "chelabase.member_roles.reconcile";        // core publish, debounced like resync
#[derive(Serialize, Deserialize)] pub struct MemberRolesReconcileRequest { pub branch_id: Uuid, pub member_id: Option<Uuid> }
#[derive(Serialize, Deserialize)] pub enum MemberRolesReply { Done { roles: Vec<MemberRoleApplied> }, Failed { message: String } }
#[derive(Serialize, Deserialize)] pub struct MemberRoleApplied { pub member_id: Uuid, pub pg_database: String, pub role_name: String, pub applied_roles: Vec<String> }
// chelabase-secrets
Purpose::MemberRole  // as_str "member_role"
pub fn seal_member_role_password(key, branch_id: Uuid, role: &str, pw: &str) -> Result<String>;   // AAD "{branch_id}:member_role:{role}"
pub fn open_member_role_password(key, branch_id: Uuid, role: &str, sealed: &str) -> Result<String>;
// worker
pub fn member_role_name(member: Uuid, pg_database: &str) -> String;
pub fn plan_member_roles(desired: &[DesiredMemberRole], actual: &[ActualMemberRole]) -> Vec<MemberRoleStep>; // pure
pub fn render_step(step: &MemberRoleStep) -> String;                                                        // pure, uses chelabase_schema::sql::{ident, literal}
```

**Behaviour:**
- **Desired state:** for each active member with an effective grant on the branch's project, and each Postgres database on the branch (`branch_databases`), the desired role set is `effective(...)` minus `Owner`, intersected with the roles that exist in that database's cluster. A tenant admin's `All` = every non-reserved, non-owner role on the branch.
- **Desired = empty:** no `cb_m_*` role exists for that (member, database). Admins acting as owner use the owner connection.
- **Actual state:** read with `admin_sql` from `pg_roles` and `pg_auth_members`, filtered to `cb_m_%`.
- **Steps, in order:**
  1. create (`CREATE ROLE … LOGIN NOINHERIT NOCREATEDB NOCREATEROLE NOBYPASSRLS NOREPLICATION PASSWORD '<scram verifier>'`);
  2. grant (`GRANT r TO m WITH INHERIT FALSE, SET TRUE, ADMIN FALSE`);
  3. revoke (`REVOKE r FROM m`, then `SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE usename = m`);
  4. drop (terminate, `REASSIGN OWNED BY m TO <owner>`, `DROP OWNED BY m`, `DROP ROLE m`; the last three run in each Postgres database of the branch).
- **Passwords:**
  - A role with no `member_branch_roles` row on this branch is a role this branch inherited from its parent, or a new one. In both cases it gets a fresh password (`ALTER ROLE … PASSWORD`) before its row is written.
  - Passwords travel only as SCRAM verifiers (`chelabase_neon::scram::verifier`). They never appear in argv or logs.
- **Missing roles:** a role named in a grant that doesn't exist on the branch is skipped and logged at `debug`.
- **Collisions:** a hex8 collision between two databases returns `Failed`.
- **Idempotent:** a second run with nothing changed renders zero steps.
- **Hooks:**
  - `finish_branch` publishes an enqueue for the new branch;
  - 6a role create/delete enqueues for that branch;
  - 6c database upsert/remove enqueues;
  - the RPC serves synchronous calls (T9) and enqueues from the orchestrator (T7).

**Tests (write first):**
- Unit:
  - `plan_member_roles_creates_grants_revokes_drops`;
  - `plan_is_empty_when_converged`;
  - `render_quotes_names_and_never_contains_plaintext_passwords` (a property over random role names, including quotes, spaces and Unicode);
  - `member_role_name_is_46_chars_and_stable`;
  - `owner_is_never_granted`.
- Live (`#[ignore]`, `--no-run`):
  - `reconcile_converges` (proptest, 16 cases): random grant, override, role, database and branch operations, then reconcile. `pg_roles`/`pg_auth_members` for `cb_m_%` equal the model, and a second reconcile changes nothing.
  - `revoke_terminates_member_sessions`.
  - `child_branch_gets_fresh_member_passwords`.
  - `reconcile_skips_roles_missing_on_the_branch`.
  - `escape_attempts_fail`: connected as a member role, then `SET ROLE billing`; these must all fail:
    - `RESET ROLE; SET ROLE <owner>`;
    - `SELECT set_config('role', '<owner>', false)`;
    - `SET ROLE <a role allowed only in another pg database of the branch>`;
    - a login as `cb_m_*` through the proxy port 4432;
    - `dblink` to the same database without a password.

**Verify:** `cargo test -p worker -p chelabase-secrets -p chelabase-nats --lib`, then `cargo test -p worker --test member_roles_live --no-run`.

- [ ] RED, GREEN, commit `members T8: member-role reconciler`, push.


### Task T4b: R17 cleanup (no per-Postgres-database overrides)

**Spec:** M4 and parts 1 and 3 as amended by R17.

**Files:**
- `crates/chelabase-metadata/migrations/20261014000001_members.sql` (edit in place: the branch is unmerged and dev data is throwaway)
- `crates/chelabase-metadata/src/members.rs` and `tests/members_live.rs`
- `crates/chelabase-types/src/access.rs`

**Behaviour:**
- `database_grant_overrides` drops `pg_database`; its primary key becomes (`member_id`, `database_id`).
- `member_branch_roles` drops `pg_database`; its primary key becomes (`member_id`, `branch_id`).
- `put_override`, `delete_override`, `grant_rows` and `GrantRows` lose the `pg_database` parameter and field.
- `MemberBranchRole` loses `pg_database`, and `delete_member_branch_role(member, branch)` follows.
- `effective(project: &Grant, database: Option<&Narrowing>) -> Grant`.
- Every test is updated. The grant-algebra proptest covers two levels.
- Any unique and null tests about `pg_database` are removed or replaced.

**Not here:** the orchestrator (T5's fix round) and the worker (T8's fix round) adapt to these signatures after this merges.

**Verify:** `cargo test -p chelabase-types -p chelabase-metadata --lib`, then `cargo test -p chelabase-metadata --test members_live --no-run`. The orchestrator and worker may fail to compile until their fix rounds, so `cargo check -p chelabase-types -p chelabase-metadata` is enough.

- [ ] RED, GREEN, commit `members T4b: drop per-Postgres-database overrides (R17)`, push.
### Task T5: Orchestrator authorization

**Spec:** part 2 ("Decision and resolution", "Answers"), part 5 ("Resolving the caller"), part 7 (invariant test 2).

**Files:**
- Create: `services/orchestrator/src/access.rs`.
- Modify: `services/orchestrator/src/lib.rs` (`AuthPrincipal`, `MetadataPort` + its real impl, `load_owned` removed, `/v1/me`), `openapi.rs` (Doc impl), every file with a `load_owned` call, `tests/api.rs` (FakeMetadata: the members, grants and overrides state).
- Test: `services/orchestrator/tests/access_walk.rs` (new; reuses `tests/api.rs` helpers via a shared `tests/common/mod.rs` if needed).

**Interfaces:**
- Consumes: T3's metadata functions (exposed through new `MetadataPort` methods of the same names and signatures) and T4's `decide`, `effective`, `Access`, `RoleSet`.
- Produces:
```rust
pub struct AuthPrincipal { pub sub: String, pub org_id: String, pub tenant_id: TenantId, pub request_id: String,
                           pub member: Option<MemberRecord>, pub in_operator_org: bool }
pub struct Authorized { pub record: DatabaseRecord, pub project_id: Uuid, pub grant: Option<Grant>, pub branch_protected: Option<bool> }
pub async fn authorize<M: MetadataPort, J: JobPort>(state: &AppState<M, J>, principal: &AuthPrincipal, access: Access,
    target: Target<'_>) -> Result<Authorized, ApiError>;
pub enum Target<'a> { None, Project(Uuid), Database(Uuid), Branch { database: Uuid, branch: Uuid },
                      PgDatabase { database: Uuid, branch: Uuid, pg_database: &'a str }, Role { database: Uuid, branch: Uuid, pg_database: &'a str, role: Requested<'a> } }
pub fn deny_to_api(d: Deny) -> ApiError;    // NotFound→404 not_found, Forbidden→403 forbidden + extra.required, NoAccessYet→403, NotAvailable…→403
```
- `AppState` gains `policy: ModePolicy` and `operator_org: Option<String>` (env `CHELABASE_OPERATOR_ORG_ID`).

**Behaviour:**
- **The principal:** `AuthPrincipal` extraction calls `ensure_member` (which creates or activates the member and refreshes the email and name from the identity claims; it extends the gateway's identity token with `email` and `name` if they're absent, checked in `services/gateway/src/pipeline.rs:369-390`). It skips `ensure_member` when the policy is platform and no member exists, so platform never creates members.
- **`authorize`:**
  - looks up the endpoint's `Access` from the handler's own constant (each handler passes `ENDPOINT.policy.access`, so the table is the single source);
  - resolves the target to project, database and branch (protection), loads `grant_rows`, computes `effective` (or `tenant_admin_grant()`), and calls `decide`;
  - checks that the database belongs to the caller's tenant (as `load_owned` did) before anything else.
- **The 42 `load_owned` sites:** each becomes `authorize(&state, &principal, ENDPOINT_X.policy.access, Target::…)`. `load_owned` is deleted.
- **`/v1/me`** returns:
```json
{ "sub", "email", "name", "tenant_id", "member": {status, tenant_admin} | null, "instance_admin": bool,
  "projects": [{ "project_id", "platform_role", "roles": [..] | "all" }] }
```
  It's a utoipa schema `MeResponse`.

**Tests (write first):**
- `access_walk::every_route_refuses_non_members`, `…_ungranted_members`, `…_one_level_too_low`, in both modes. Each iterates `chelabase_routes::ENDPOINTS` filtered to `Service::Orchestrator`, builds the path with fake ids for a database the fake owns, sends the method with an empty or minimal JSON body, and expects a 403/404 envelope. For "one level too low", the expected `required` is derived from the endpoint's `Access`.
- `access_walk::walk_covers_every_orchestrator_endpoint`: the walked count equals the orchestrator endpoint count.
- `api::removed_member_is_refused_with_a_valid_token`, `api::invited_member_activates_on_first_request`, `api::platform_mode_creates_no_members`, `api::me_lists_capabilities`, `api::tenant_admin_is_admin_on_every_project`.
- All existing `tests/api.rs` tests pass. The fake's default caller becomes a tenant admin so they keep their meaning.

**Verify:** `cargo test -p orchestrator`, `just api-gen` (the `/v1/me` schema changed), `cargo check --workspace`.

- [ ] RED, GREEN, commit `members T5: authorize every orchestrator route`, push.

### Task T10: Auth service, sign-up gate and invite acceptance

**Spec:** part 5 ("Sign-up gate", "Accept").

**Files:**
- `services/auth/src/app.rs`
- `services/auth/src/app_users/internal.rs` (or a new `services/auth/src/internal.rs` if the client is app-users-specific)
- `services/orchestrator/src/internal/` (new `signup.rs` with `GET /internal/signup/allowed?org=<id>`, bearer as `/internal/app/*`)
- `crates/chelabase-zitadel/src/client.rs` + `mock.rs` (`verify_invite_code(user_id, code)`)
- `crates/chelabase-routes/src/lib.rs` (new auth endpoint `ACCEPT_INVITE` with `Access::Public`)
- `openapi/login.json`
- `apps/studio/src/lib/login-client/generated`

**Interfaces (produces):**
- `POST /auth/v1/accept-invite`, body `{ user: String, code: String, password: String }` → the same success shape as sign-in (session plus the redirect continuation).
- Errors: `invite_expired`, `invite_revoked`, `invalid_request` (a weak password uses the existing strength error).
- Internal: `GET /internal/signup/allowed?org=` → `{ "allowed": bool, "reason": "open" | "first_member" | "closed" | "platform" }`.

**Behaviour:**
- **Sign-up:** `sign_up_inner` calls the internal route before `create_user`. `closed` gives `sign_up_closed`; `platform` gives `not_available_in_platform_mode`.
- **Accept-invite:**
  1. verify the code with Zitadel (expired → `invite_expired`);
  2. set the password;
  3. mark the email verified;
  4. create the session;
  5. finish the auth request.

  A revoked invite's Zitadel user is deleted (T7), so Zitadel answers "not found"; that maps to `invite_revoked`.
- **Rate limit:** the same as sign-up.

**Tests (write first):** auth `tests/flows.rs`:
- `sign_up_refused_when_closed`;
- `sign_up_allowed_when_open`;
- `first_member_may_sign_up`;
- `platform_sign_up_refused`;
- `accept_invite_sets_password_and_signs_in`;
- `accept_invite_expired`;
- `accept_invite_revoked`.

Orchestrator internal test: `signup_allowed_reasons`.

**Verify:** `cargo test -p auth -p orchestrator`, `just api-gen`.

- [ ] RED, GREEN, commit `members T10: sign-up gate and invite acceptance`, push.

### Task T11: CLI bootstrap admin

**Spec:** part 5 ("Bootstrap").

**Files:** `tools/cli/src/main.rs` (`Bootstrap { #[arg(long)] admin_email: Option<String> }`), `tools/cli/src/dev/bootstrap.rs`, `tools/cli/src/dev/tenant.rs`, `justfile:158` (`zitadel-bootstrap admin_email="<dev user email>":` passes `--admin-email {{admin_email}}`).

**Behaviour:**
- After binding the tenant, find the Zitadel user with that email in the org (defaulting to the dev user the bootstrap creates). Then upsert the member as an active tenant admin through the same SQL as `bootstrap_admin`: psql `INSERT … ON CONFLICT`, plus an `access_events` row with `admin_set` and a NULL actor.
- Idempotent.

**Tests (write first):**
- `bootstrap::tests::admin_email_defaults_to_dev_user`;
- `bootstrap::tests::admin_sql_is_idempotent_and_logs_event` (asserts on the SQL text);
- clap parse test `bootstrap_accepts_admin_email`.

**Verify:** `cargo test -p chelabase`.

- [ ] RED, GREEN, commit `members T11: bootstrap admin`, push.

### Task T6: Route-specific refinements

**Spec:** part 4 (the per-route notes).

**Files:**
- `services/orchestrator/src/console.rs`
- `sql_editor/routes.rs` (cancel)
- `sql_editor/saved.rs`
- `data_keys.rs`
- `merges.rs`
- `projects.rs`
- `lib.rs` (`POST /v1/databases` takes `project_id`; `body.tenant_id` is dropped)
- `tests/api.rs`

**Behaviour:**
- **Console:** a Viewer may only send `select` trees (a batch containing any write is also refused) → `forbidden{required:"developer"}`.
- **SQL-editor cancel:** allowed for the run's starter (the run registry stores the member id) or an Admin.
- **Saved queries:** edit and delete are allowed for the author (a new `author_member_id` column via the T3-style migration `20261014000002_saved_query_author.sql`; existing rows are NULL, which means Admin-only) or an Admin.
- **Data keys:** create, rotate, patch and delete need the key's bound role to be in the caller's effective roles, unless the caller is Admin.
- **Merges:** `apply` of a plan with security steps needs Admin (`forbidden{required:"admin"}`), even on unprotected targets.
- **Projects:** `GET /v1/projects` lists only projects with a grant (all for tenant admins).
- **Databases:** `POST /v1/databases` requires `Project(Admin)` on the given `project_id`.
- **`openapi`:** every changed request body is regenerated (`just api-gen`).
- **R20:** app-user routes act on the project's auth database (`project_auth.database_id`), so their access check applies that Chelabase database's override (fail closed). *Test:* `api::app_user_routes_apply_the_auth_database_override`.
- Fix the clippy warning in `crates/chelabase-metadata/src/members.rs:622` (left by T3).

**Tests (write first):** `api::viewer_console_select_ok_write_forbidden`, `api::viewer_batch_with_write_forbidden`, `api::cancel_by_other_developer_forbidden`, `api::saved_query_edit_by_author_or_admin`, `api::key_for_disallowed_role_forbidden`, `api::merge_with_security_steps_needs_admin`, `api::project_list_filtered_by_grant`, `api::create_database_needs_project_admin`.

**Verify:** `cargo test -p orchestrator`, `just api-gen`.

- [ ] RED, GREEN, commit `members T6: route-specific access rules`, push.

### Task T7: Member management API

**Spec:** part 4 ("Members (new)" rows), part 5 ("Invites", "Removal").

**First step:** verify Zitadel v4.19.1's `POST /v2/users/{userId}/invite_code` (body `{ "sendCode": { "urlTemplate", "applicationName" } }`) and `POST /v2/users/{userId}/invite_code/verify` (`{ "verificationCode" }`) against the v4.19.1 tag's `proto/zitadel/user/v2/user_service.proto`. Report the exact shapes. **If they're absent, stop and report.**

**Files:**
- Create: `services/orchestrator/src/members.rs`.
- Modify:
  - `crates/chelabase-zitadel/src/client.rs` + `mock.rs` (`create_invite_code(user_id, url_template)`, `create_user_without_password(org, email)`)
  - `crates/chelabase-routes/src/lib.rs` (the new endpoints with `Access`)
  - `services/orchestrator/src/lib.rs` (routes merged, `MetadataPort`)
  - `openapi.rs`
  - `tests/api.rs`
  - `openapi/management.json`
  - `packages/management-client` (regenerated)

**Endpoints (operationIds pinned):**

| Method path | operationId | Access |
|---|---|---|
| `GET /v1/members` | `listMembers` | TenantMember |
| `PATCH /v1/members/{member_id}` `{tenant_admin}` | `updateMember` | TenantAdmin |
| `DELETE /v1/members/{member_id}` | `removeMember` | TenantAdmin |
| `POST /v1/invites` `{email, grants:[{project_id, platform_role, pg_roles}]}` | `createInvite` | TenantAdmin |
| `POST /v1/invites/{invite_id}/resend` | `resendInvite` | TenantAdmin |
| `DELETE /v1/invites/{invite_id}` | `revokeInvite` | TenantAdmin |
| `POST /v1/projects/{id}/invites` `{email, platform_role, pg_roles}` | `createProjectInvite` | Project(Admin) |
| `GET /v1/projects/{id}/grants` | `listProjectGrants` | Project(Admin) |
| `PUT /v1/projects/{id}/grants/{member_id}` `{platform_role, pg_roles}` | `putProjectGrant` | Project(Admin) |
| `DELETE /v1/projects/{id}/grants/{member_id}` | `deleteProjectGrant` | Project(Admin) |
| `PUT /v1/projects/{id}/grants/{member_id}/overrides` `{database_id, pg_database?, platform_role?, pg_roles?}` | `putGrantOverride` | Project(Admin) |
| `DELETE /v1/projects/{id}/grants/{member_id}/overrides` `?database_id&pg_database` | `deleteGrantOverride` | Project(Admin) |
| `GET /v1/access-events` `?before&limit` | `listAccessEvents` | TenantAdmin |
| `GET /v1/projects/{id}/access-events` `?before&limit` | `listProjectAccessEvents` | Project(Admin) |
| `GET /v1/instance/settings` | `getInstanceSettings` | Instance |
| `PATCH /v1/instance/settings` `{open_signup}` | `updateInstanceSettings` | Instance |

**Behaviour:**
- **Invite matching:** order and Zitadel calls as in spec part 5. Emails are trimmed and lowercased. The URL template comes from config `CHELABASE_STUDIO_URL` + `/accept-invite?user={{.UserID}}&code={{.Code}}`, and expiry is Zitadel's code lifetime (store `now + 72h`, matching the configured lifetime).
- **Project invites:** a project Admin may grant only on their own project.
- **Override validation:** an override may only narrow: a role not in the project grant, or a level above it, gives `invalid_request`.
- **Reserved names (R7):** grants, overrides and invites refuse any reserved role name in `pg_roles` (`owner`, the database's owner role, `cb_m_*`, `chelabase_*`, the platform roles), with `invalid_request`. *Test:* `api::grant_with_reserved_role_name_is_refused`.
- **R9:** promoting a member who isn't `active` (for example, invited) to tenant admin is refused with `conflict`. **R8:** `MemberRecord.email` is `Option<String>` (the newest claim wins); invite matching compares only non-NULL emails. *Test:* `api::promote_invited_member_conflicts`.
- **`open_signup`:** in platform mode, setting it gives `not_available_in_platform_mode`.
- **Reconcile:** after every grant, override or removal change, publish `SUBJECT_MEMBER_ROLES_ENQUEUE` for each of `project_branches(project)` (on removal, for all of the member's projects).
- **Removal:** additionally deactivate the Zitadel user and delete their sessions (the existing admin-PAT client).

**Tests (write first):**
- `api::invite_creates_zitadel_user_and_invited_member`;
- `api::invite_matches_existing_member_case_insensitively`;
- `api::invite_existing_org_user_gets_member_row`;
- `api::project_admin_invites_into_own_project_only`;
- `api::override_cannot_widen`;
- `api::grant_change_enqueues_reconcile_for_every_branch`;
- `api::remove_member_deactivates_and_enqueues`;
- `api::last_admin_demotion_conflicts`;
- `api::open_signup_platform_refused`;
- `api::revoke_invite_deletes_zitadel_user`;
- Zitadel mock tests for the two new calls.

**Verify:** `cargo test -p orchestrator -p chelabase-zitadel`, `just api-gen`.

- [ ] RED, GREEN, commit `members T7: member management API`, push.

### Task T9: Act-as connections through member roles

**Spec:** part 3 ("Lazy check", "Connections"), part 4 (SQL editor row).

**Files:** `services/orchestrator/src/sql_editor/routes.rs` (`connect_as_owner` :237, `resolve_role` :165, `grant_to_owner` :198 removed), `sql_editor/run.rs`, `sql_editor/schema.rs`, `services/orchestrator/src/lib.rs` (a `JobPort::reconcile_member_roles(req) -> Result<MemberRolesReply, ResetError>` RPC method + its 3 impls), `tests/api.rs`.
- Test (live): `services/orchestrator/tests/sql_run_live.rs` additions (`#[ignore]`).

**Interfaces (produces):**
```rust
pub async fn member_session_target<M, J>(state: &AppState<M, J>, auth: &Authorized, principal: &AuthPrincipal,
    branch: Uuid, pg_database: &str, role: &str) -> Result<PgTarget, ApiError>;   // role name ≠ owner; lazy reconcile; opens the sealed password
```

**Behaviour:**
- **Named role:**
  1. Read `member_branch_roles` for (member, branch, pg_database).
  2. If the row is missing, or `applied_roles` doesn't equal the effective set ∩ the branch's existing roles, call the reconcile RPC with `member_id`. A failure or timeout gives 503 `member_role_reconcile_failed` with `retry_after: 2`.
  3. Connect as the member role, then `SET ROLE <role>`. A role that's missing on the branch gives 403 `forbidden{required:"role:<name>"}`.
- **Owner:** the owner connection, as today. `authorize` already required Admin.
- **Schema reads:** the SQL-editor schema read uses the same session.
- **Gone:** `grant_to_owner` and its tests. `anon`/`authenticated` now work like any other role through membership.
- **Unchanged:** the console's named-role path through rest.
- **Ruling R5:** a script can `RESET ROLE` back to the member role and create objects, wherever PUBLIC has CREATE, or as temp objects. So after every act-as run (success or error), the same connection runs `RESET ROLE; REASSIGN OWNED BY CURRENT_USER TO <acted role>` before closing. *Test:* `sql_run_live::objects_created_after_reset_role_end_up_owned_by_the_acted_role`.

**Tests (write first):**
- Unit and fake: `api::sql_run_as_role_uses_member_session`, `api::sql_run_reconciles_when_stale`, `api::sql_run_reconcile_failure_is_503`, `api::act_as_missing_role_is_forbidden`.
- Live (`--no-run`): `sql_run_live::script_cannot_reset_to_owner`, `sql_run_live::schema_read_follows_role_usage`.

**Verify:** `cargo test -p orchestrator`, then `cargo test -p orchestrator --test sql_run_live --no-run`.

- [ ] RED, GREEN, commit `members T9: act-as through member roles`, push.

### Task T12a: Impeccable shape for the Members surfaces (pause)

**Spec:** part 6.

- Run Impeccable in Operate mode:
  - `context --target` on `apps/studio/src/routes/app.members.tsx` (new), the project Access tab, the no-access screen and `/accept-invite`;
  - then `shape` for these surfaces;
  - read `craft-floor.md`, `PRODUCT.md`, `apps/studio/DESIGN.md`, `components/app-shell/*`, the `.impeccable/` surface briefs and `packages/lagoon/README.md`.
- The output is a brief committed under `apps/studio/.impeccable/surfaces/` for each surface (layout, states, copy, components used).
- List the missing components (a role multi-select or combobox, perhaps a dialog form), each with a recommendation: Studio or lagoon.
- **No code.** The coordinator shows the user the brief and asks the component questions. T12b starts only after a yes.

### Task T12x: API additions for the Members UI (user, 2026-10-09; added after T12a)

Implementer sonnet, reviewer sonnet. Runs before T12b. Global Constraints in `task-T7-brief.md` apply (exactly one `access_events` row per access change, no grant caching, strict TDD, never `cargo fmt` the workspace, never run the dev stack; live tests `#[ignore]` and compiled `--no-run`).

**1. `Member` gains `has_access: bool` and `created_at`** (`listMembers`, and anywhere else `Member` is returned).
- `has_access` is true when the member is a tenant admin or holds at least one project grant (an override alone doesn't count). One query, no N+1.
- Test: `api::list_members_reports_has_access_and_created_at`.

**2. Access events carry names:** `actor_email`, `target_email` and `project_name`, all `Option<String>`, on both `listAccessEvents` and `listProjectAccessEvents`.
- Joined at read time. Member rows are kept when people are removed (only marked removed), so removed people still resolve. A deleted project resolves to its last name if the row still exists, else `None`.
- Test: `api::access_events_name_removed_members_and_projects`; plus a metadata live test `members_live::access_events_join_names`.

**3. Describe-invite, keyed by a Chelabase token (R32).**
- Invites get a random 32-byte token (base64url), stored only as its SHA-256 on the invite row (new migration; `members` is at `20261014000002`, and T9 adds `20261014000003`, so use `20261014000004`).
- The invite URL template becomes `CHELABASE_STUDIO_URL + /accept-invite?user={{.UserID}}&code={{.Code}}&t=<token>`. The token is minted per send, so `resendInvite` and the R29 re-invite replace it.
- New auth-service route `GET /auth/v1/invite?user&t` (public, rate-limited like the other public auth routes). It answers `{inviter_name, inviter_email, tenant_name}` only for an open (not accepted, not revoked, not expired) invite whose user id and token hash match, comparing the hash in constant time. Anything else answers the same 404 `not_found`, so callers can't tell which part was wrong.
- It goes through an internal orchestrator route (same bearer as `may_sign_up`).
- Add it to `ENDPOINTS` with the right `Policy`, and regenerate the login OpenAPI and client (`just api-gen`).
- Tests: `describe_invite_needs_the_token`, `describe_invite_closed_is_404`, `resend_rotates_the_token`, and a Zitadel-mock test that the URL template carries `t=`.

**Verify:** `cargo test -p orchestrator -p chelabase-metadata -p auth` (use the real package names), `just api-gen`; the live tests compiled with `--no-run`.

- [ ] RED, GREEN, commit `members T12x: API additions for the Members UI`, push.

### Task T12y: Invite-only self-hosted mode, link invites (user, 2026-10-09; R34, R35)

Implementer sonnet, reviewer opus (it removes an entry path and handles one-time links). Runs after T12x merges, before T12b. The Global Constraints apply.

**R34 (user):** self-hosted mode has no self-sign-up at all. People join only by invite from the bootstrap admin (`.env`) or another admin. Platform-mode sign-up gets its own design later; until then it stays refused, as it already is.
- Remove self-sign-up end to end:
  - the auth sign-up route and its `may_sign_up` internal route and client;
  - `tenants.open_signup` (a new migration drops the column);
  - `GET`/`PATCH /v1/instance/settings` and `InstancePrincipal`, if nothing else uses it (check; T5/R19 introduced it). Keep `Access::Instance` only if other routes use it.
  - the `ModePolicy` sign-up rule;
  - the `sign_up_closed` error code, if nothing else uses it (check the docs and the pinned `ALL` list).
- The sign-in lookup no longer offers sign-up for an unknown email. It answers the same as today for an unknown account, without revealing whether the email exists (check the current lookup's enumeration behaviour and keep it), and the login API no longer has a sign-up flag.
- Regenerate the login and management OpenAPI and clients (`just api-gen`). Update the `ENDPOINTS` counts, the routes tests and the access walk.
- **Studio part:** delete `routes/register.tsx`, `sign-up-form.tsx` and the `toRegister` path in `sign-in-form.tsx`, with their tests. An unknown email shows "No account for this email. Ask your admin for an invite." (wording to match the existing login copy).

**R35 (user): invites without SMTP.**
- `createInvite`, `createProjectInvite` and `resendInvite` take `delivery: "email" | "link"` (default `"email"`).
- With `"link"`, the orchestrator asks Zitadel for the code with `returnCode` instead of `sendCode` and answers with `invite_url` (the full `/accept-invite?user&code&t` link). It is shown once and never stored or logged; the response has `Cache-Control: no-store`.
- The email counts as verified when the invitee accepts (as with email delivery).
- New `GET /v1/invites/delivery` → `{ email_available: bool }`, Access `TenantMember`. The orchestrator asks Zitadel's admin API whether an SMTP provider is active. Verify the exact v4.19.1 call against the tag's protos and report it; if there is none, stop and report. A Zitadel failure answers `email_available: false`, plus a `reason`.
- Tests:
  - `api::link_invite_returns_the_url_once_and_sends_no_mail`
  - `api::link_resend_rotates_code_and_token`
  - `api::delivery_reports_smtp_state`
  - `api::sign_up_route_is_gone`
  - the auth flow tests updated
  - Zitadel mock tests for `returnCode` and the SMTP query

**R36 (user): tenant display names.**
- A nullable `tenants.display_name`, at most 80 characters, trimmed; an empty value is stored as NULL. Readers fall back to the slug.
- `/v1/me` returns `tenant: { id, slug, display_name }`, and describe-invite's `tenant_name` uses the display name, falling back to the slug.
- New `PATCH /v1/tenant { display_name }` (operationId `updateTenant`, Access `TenantAdmin`). It writes one `access_events` row (`tenant_renamed`) in the same transaction.
- `dev bootstrap` fills it from the Zitadel org's name when it is NULL.
- Tests: `api::tenant_admin_renames_tenant`, `api::rename_needs_tenant_admin`, `internal::describe_invite_uses_display_name`, a metadata live test `members_live::tenant_display_name_round_trip`.
- The Studio edit field is T12b's.

**Verify:** `cargo test` for orchestrator, auth, metadata, routes, zitadel and types; `pnpm --filter studio test` and the typecheck; `just api-gen`.

- [ ] RED, GREEN, commit `members T12y: invite-only self-hosted mode, link invites`, push.

### Task T12b: Studio, Members page, no-access screen, accept-invite, capability hiding

**Spec:** part 6.

**Files:**
- `apps/studio/src/components/app-shell/rail-items.ts`: `RailLink.id` adds `"members"`, `to` adds `"/app/members"`; `placeholder-2` is replaced; `isRailItemActive` gets a `members` case.
- `src/routes/app.members.tsx`.
- `src/components/members/*`.
- `src/routes/accept-invite.tsx`, using `LoginLayout` and `LoginProviders`.
- `src/components/no-access.tsx`, shown from `app.tsx` when `me.projects` is empty and the caller isn't a tenant admin.
- Capability hiding in `project-page.tsx`, the SQL editor entry points, role pickers and owner actions.
- Plus the components the user approved in T12a.

**Behaviour:** as in the approved brief.
- Lagoon only: tokens through Tailwind, dark only, no colour literals, fonts only from lagoon, `components/ui/*`.
- A 403 envelope shows a clear message.

**Tests (write first, Vitest and Testing Library next to each component):**
- `members-page`:
  - `lists_people_with_status`;
  - `invite_form_validates_email_and_grants`;
  - `non_admin_sees_read_only`;
  - `instance_tab_only_with_capability`.
- `no-access`: `names_tenant_admins`.
- `accept-invite`:
  - `sets_password_and_signs_in`;
  - `shows_expired_and_revoked`.
  - `retries_with_accept_ticket` (R13: an `invite_incomplete` **or `weak_password`** answer may carry `accept_ticket`; the page keeps it and retries with the ticket and a new password, never the spent code; tickets are one-use).
- `rail-items`: `members_item_active_on_members_routes`.
- `capabilities`:
  - `viewer_has_no_sql_editor`;
  - `owner_actions_admin_only`;
  - `role_picker_lists_allowed_roles`.

**Verify:** `pnpm --filter studio typecheck`, `pnpm --filter studio test`, and lagoon's single-source font test.

- [ ] RED, GREEN, commit `members T12b: Studio members, no-access, accept-invite`, push.

### Task T12c: Studio project Access tab

**Spec:** part 6.

**Files:** `project-page.tsx` (`ProjectTab` adds `"access"`), `lib/project-search.ts`, `src/components/project-access/*`.

**Behaviour:**
- **Grants table:** member, platform role and allowed roles. Roles are picked from the branch's 6a role list (`listRoles`). There is no owner entry: Admin is labelled "includes the database owner" (R7).
- **Overrides:** per Chelabase database and per Postgres database, shown as narrowing chips. The UI offers only narrower choices, and the API remains the judge.
- **Invite into this project** and the project's activity list.

**Tests (write first):**
- `access-tab`:
  - `admin_is_labelled_as_owner_and_no_owner_chip`;
  - `override_offers_only_narrower_choices`;
  - `invite_into_project`;
  - `activity_lists_events`.
- `project-search`: `access_tab_round_trips`.

**Verify:** `pnpm --filter studio typecheck`, `pnpm --filter studio test`.

- [ ] RED, GREEN, commit `members T12c: project Access tab`, push.

### Task T13: Docs

**Spec:** part 7 ("Docs").

**Files:**
- `apps/docs` content: Using → "Members and invites", "Roles and schema isolation" (the `billing` example: a group role owns schema `billing`; a Developer allowed `billing` edits it, and others get `forbidden`), "Self-hosted vs platform".
- Reference → "Access per route", "Error codes".
- A small generator binary or test (`crates/chelabase-routes/tests/docs_tables.rs`, run with `CHELABASE_BLESS=1`) that writes the two generated tables from `ENDPOINTS` and `ModePolicy::describe()`. A staleness check is added to `just check`.

**Tests (write first):** `docs_tables::access_table_is_current`, `docs_tables::mode_table_is_current`.

**Verify:** `cargo test -p chelabase-routes`, `pnpm --filter docs build`.

- [ ] RED, GREEN, commit `members T13: docs`, push.

### Task T14: Live e2e

**Spec:** part 7 ("Live suites").

**Files:**
- `scripts/members-e2e.sh` and a `justfile` recipe `members-e2e: _live-check`, after `role-e2e`.
- Existing `scripts/*-e2e.sh`: they now sign in as the bootstrap admin (the shared helper they use for tokens).

**Flow:**
1. bootstrap admin;
2. create a project with a database, plus a group role `billing` that owns schema `billing` with a table;
3. invite `dev@…`, fetch the invite link from Mailpit's API and accept it;
4. the admin grants `dev` Developer with `billing`;
5. as `dev`, the SQL editor can `INSERT` into `billing.t` but gets `forbidden`/permission denied on `public` and on another schema, and the console as owner is forbidden;
6. revoke, and the next request is refused;
7. sign-up is closed: the `sign-up` call returns `sign_up_closed`.

**Verify:** `bash -n scripts/members-e2e.sh`. The coordinator runs it through `just live members-e2e`.

- [ ] Commit `members T14: members e2e`, push.
