# Members hub: spec red-team

Date: 2026-10-23. Spec: `docs/superpowers/specs/2026-10-23-members-hub-design.md` (Parts 1–7, rulings R1–R11). Builds on the members-and-roles design (R1–R51) and fork round 4 §12 (the personal-login marker, RT4-4).

Scope: the spec text, and how its changes interact with the subsystems they touch: the access check (`authorize`/`decide`), the member-role reconciler, the resync, the proxy's secret lookup, the DDL guard (`chelabase.managed_role`, `chelabase_schema_locked`, `guard_logins`), branch copies, restores and resets, the 6g merge and the catalog read, data keys and the data API's grants, realtime, and Zitadel. Every claim cites the code at the `members-hub` branch head (`3c6dfe69`).

Counts: **5 critical, 18 important, 15 minor** (38). Five findings need a user decision (marked **USER DECISION**); everything else has a mechanical ruling.

---

## A. Teams and `access::union` (Part 5, step 2)

### MH-1 (critical) — the flat union mixes levels and exceeds every single source — USER DECISION

**Scenario.** Alice has her own grant `Developer {app}` on P. She is in the "Auditors" team, which has `Viewer {billing_writer}` on P (auditors read billing through that role; Viewer routes are reads). Part 5's union takes "the highest platform role and the union of role sets": `Developer {app, billing_writer}`. Alice can now write through `billing_writer` on every Developer route (table API writes, SQL editor), which neither source allowed. Through her personal login (Part 6, Developer+) she can `SET ROLE billing_writer` in `psql` and write without any route check at all: the reconciler grants every role of the effective set regardless of level (`services/worker/src/member_roles.rs:1274-1289`, `granted_roles` at `:937-969`).

The spec's own test promise (Part 7 Testing: "union then narrowing never exceeds what any single source or the override allows") is false for the algebra Part 5 defines: `Developer + billing_writer` is allowed by no single source.

**Evidence.** `decide` checks the level once, then the role set (`crates/chelabase-types/src/access.rs:370-397`); `Grant` holds one `platform_role` for all roles (`:113-117`). Today a single grant is uniform, so the problem only appears with a union.

**USER DECISION.**
1. **(Recommended) Per-role levels.** The union keeps, for each role, the highest level among the sources that name it (`Grant { platform_role, roles: RoleSet, role_levels: BTreeMap<String, PlatformRole> }` or an equivalent). `decide` checks the route minimum against the requested role's own level; the reconciler and personal logins grant a role only when its level is at least Developer (a Viewer-sourced role stays read-only, which in Postgres means it is not granted to the personal login). The exhaustive test checks "per role, never above the best single source".
2. **Flat union (the spec as written).** Accept the combination and rewrite the test property to "≤ the join of the sources". Cheapest; documents an escalation that teams make easy.
3. **Teams carry no roles below Developer.** Team grants are Developer or Admin only (a Viewer team is level-only, no roles). The flat union is then safe for role sets; levels still mix (a Viewer team plus own Developer is just Developer).

### MH-2 (important) — `union(&[Grant]) -> Grant` and `GrantRows.project` invite own-only and empty-union mistakes

**Scenario.** (a) `union(&[])` must mean "no access", but a `-> Grant` signature forces a value: any default (`Viewer`, no roles) turns "no grant" into Viewer access (fail-open). (b) Today every caller reads `GrantRows.project: Option<GrantRow>` and maps `None` to "no access" (`services/orchestrator/src/access.rs:333-338`, `member_roles.rs:1277-1284`). A caller left on that field after step 2 silently ignores team grants: a team-only member is "no access yet" there while other paths grant access (fail-closed in one place, open in another).

**Evidence.** The spec's caller list (Part 5) misses several readers of grants: `authorize`'s admission (`access.rs:247-253`, `member_projects`), the realtime project filter (`services/orchestrator/src/internal/realtime.rs:408-414`), the project list (`services/orchestrator/src/projects.rs:199`), `/v1/me` (`services/orchestrator/src/lib.rs:2441-2479`), the roster's `has_access` (`crates/chelabase-metadata/src/members.rs:565-567`), grant validation `effective_on` / `check_effective` (`services/orchestrator/src/members.rs:1021-1037`, `:1154-1194`), the I4 observe (`members.rs:1200-1214`, `project_access`), and `remove_member`'s `touched` (`crates/chelabase-metadata/src/members.rs:1784-1797`).

**Ruling.** `access::union(&[Grant]) -> Option<Grant>` (`None` for an empty slice). The metadata replaces `GrantRows { project, database }` with a type that has no public own-grant field (`GrantSources { own, teams, database }` with one method `effective(&self) -> Option<Grant>` doing union then narrowing), so a caller that reads only the own grant doesn't compile. Every query listed above reads team grants too (one SQL shape, shared). The plan lists each caller by file and a route test per caller with a team-only member.

### MH-3 (important) — union with `All` drops the data API roles a grant named (R18)

**Scenario.** Own grant `Developer {anon}` (Alice tests the data API as `anon`); a team gives Admin on P. The union is `Admin, RoleSet::All` (Part 5: "Admin means `RoleSet::All`"), and `All` never holds `anon`/`authenticated` (`access.rs:93-101`, R18). Alice loses `anon` the moment she gains Admin. Today the shape can't arise, because an Admin row stores no roles (`crates/chelabase-metadata/src/members.rs:139-145`).

**Ruling.** The union of `All` with `Only(S)` keeps S's data API names: `RoleSet::All` gains the named extras (`All { plus: BTreeSet<PgRole> }`, holding only `anon`/`authenticated`), `may_act_as` honours them, and `effective`'s `every` test (`access.rs:146`) accepts the new shape. The exhaustive test covers it.

### MH-4 (important) — team writes must keep the grant invariant and the I4 check-and-write

**Scenario.** (a) Alice's own grant is `Developer {a}`; an override on database D says `{platform_role: Viewer, roles: {x}}` where `x` is a LOGIN role. Today that passes validation because `effective_on` intersects `{a} ∩ {x} = ∅` and checks nothing (`members.rs:1028-1033`). A team then gives Admin on P: the union is Admin+All, and `effective` keeps the override's roles verbatim for an every-role grant (`access.rs:156-158`), so Alice is `Viewer {x}` on D, a state the T7 invariant (`members.rs:1008-1012`) forbids. The act-as chain check (`access.rs:384-395`) and the reconciler's `safe_roles` (`member_roles.rs:913-920`) still refuse `x`, so it's not exploitable today, but the stored state breaks the invariant every other check relies on. (b) The I4 checked write (`put_project_grants_checked`, `crates/chelabase-metadata/src/members.rs:1879`) validates under the member's lock; a concurrent team change for that member doesn't take that lock, so the check validates a state that no longer exists.

**Ruling.** Every team write (grant put/delete, member add/remove, team delete) runs `check_effective` on the union for every affected member and every database (with their overrides), refused with 422 naming the member and the database, and takes the tenant lock, then each affected member's lock in id order, before writing. Override writes validate against the union. One property test: after any sequence of grant, override and team writes, every member's effective grant passes `check_effective`.

### MH-5 (important) — team changes must fire every side effect a grant change fires

**Scenario.** A grant change calls `access_changed` (realtime re-checks the member's subscriptions, `members.rs:939-941`, `internal/realtime.rs:114-119`) and `reconcile_projects` (`members.rs:945-964`). The spec's team triggers name only the reconcile. A member removed from a team keeps live realtime subscriptions on the team's projects. Also `remove_member`'s `touched` lists only own grants, overrides and member-role rows (`crates/chelabase-metadata/src/members.rs:1784-1797`), so the team-only projects of a removed member, and branches where they hold only a personal login, aren't reconciled until the sweep.

**Ruling.** One helper, `access_changed_for(members, projects)`, called by every grant, override and team write: `access_changed` per affected member, then `reconcile_projects` over the affected projects. `remove_member`'s `touched` adds the projects of the member's team grants and of their personal logins.

### MH-6 (minor) — the Teams tab leaks grants on projects a reader can't see

**Scenario.** Part 1: Teams "everyone (read)"; a team page shows "Project access cards". A Viewer on project A reads the team page and learns project B exists and which Postgres roles the team holds there. R1 hides exactly this on the roster.

**Ruling.** Team reads apply R1: a non-tenant-admin sees a team's members and only the project cards of projects they hold a grant on; counts ("N projects") are over the visible ones.

### MH-7 (minor) — the member page's Activity misses team-driven access changes

**Scenario.** `GET /v1/access-events?member_id=` (Part 3) matches target or actor. A `team_grant_put` changes ten members' access but names none of them as target, so each member's Activity shows nothing.

**Ruling.** Team events carry `affected_member_ids` in `after` (and `before` for removals and deletes), and the `member_id` filter also matches those.

### MH-8 (important) — who may attach a team to an invite

**Scenario.** Part 5 adds `team_ids` to `POST /v1/invites` (TenantAdmin, `crates/chelabase-routes/src/lib.rs:712-713`). A project Admin can also invite today (`CREATE_PROJECT_INVITE`, `Project(Admin)`, `:732-737`). If the shared invite form also sends `team_ids` there, a project Admin can invite someone into a team whose grants cover projects they don't administer. Part 7's table says "invites: tenant admin only", which contradicts today's project invite.

**Ruling.** `team_ids` only on the tenant route; the project invite refuses the field (422). Part 7's table gets a row: project Admin invites into P (rows on P only, no teams), as today.

---

## B. Personal logins (Part 6, step 1)

### MH-9 (critical) — losing access doesn't revoke the proxy credential until a compute-side drop succeeds

**Scenario.** Bob is removed (or his grant is deleted) while his branch's compute is suspended, the branch is resetting, or a data key is bound to his login. The proxy authenticates from the `neon_roles` verifier alone (`crates/chelabase-metadata/src/neon.rs:747-763`, `services/orchestrator/src/internal/proxy.rs:61-78`), and `wake_compute` starts the compute for him (`proxy.rs:80-102`). The worker can only drop the role on a ready normal branch (`member_roles.rs:1160-1163`), and `delete_role` refuses while a live secret key uses the role (`services/worker/src/roles.rs:1016-1029`). Until the drop succeeds, Bob's `psql` keeps working: for minutes (the sweep), or indefinitely (the key, or a branch that stays unready).

**Ruling.** The metadata transaction that removes access (grant delete, override below Developer, member removed, tenant admin cleared, team change) also marks every affected personal login revoked (`revoked_at`) and clears its proxy verifier in the same transaction, so `routable_role_secret` answers no secret at once. The worker then drops the role (sessions ended, `roles.rs:1231-1275`), and nothing may write a verifier back for a revoked row (the resync records the catalog's verifier for a personal row today, `services/worker/src/resync.rs:931-946`). Live test: remove a member while the compute is suspended, and the proxy refuses the login before any compute work.

### MH-10 (critical) — the resync adopts an orphaned personal login as an ordinary SQL login, password included

**Scenario.** A marked role whose personal row is missing gets recorded by the resync as an `sql` role with its catalog verifier (`resync.rs:899-946`; `personal` needs both the row and the marker, `:926-937`). Such roles appear in many ways: a restore or PITR child of a point before Bob's login was deleted; a branch reset to a parent that still has someone's login; R11's child copies if the worker's `personal_logins` write is lost; and a create whose API answer is lost after `CREATE ROLE` ran (MH-15). The result is a LOGIN role with a working password for a person who may be removed. The proxy accepts it (any row with a verifier), it's listed, and an Admin can reset it and reveal the new password (MH-11). The owner can't drop it, because the marker guard refuses `DROP ROLE` on a marked role (round 4 §12).

**Ruling.** A marked role is never recorded as `sql`. The convergence rule, run by the resync and the member-role reconcile alike: a marked role on a branch is kept only when a live, unrevoked personal-login record exists for it and its member qualifies there (MH-17). Otherwise it's dropped as `cloud_admin` (sessions ended), and until then its row carries no verifier. Worker unit tests run this for each origin of an orphan, plus one live restore test.

### MH-11 (important) — the generic role endpoints act on personal logins

**Scenario.** A project Admin calls `POST …/roles/alice/reset-password` on Alice's personal login. `reset_role` accepts "any non-owner row" (`roles.rs:923-984`), runs as `cloud_admin` (superuser, which the marker guard skips), and stores a sealed password. `GET …/branches/{id}/connection?role=alice` (Admin, `crates/chelabase-routes/src/lib.rs:191-198`) then hands it out. The Admin can now log in as Alice, and the Postgres logs and `pg_stat_activity` attribute their actions to her. `DELETE …/roles/alice` (`roles.rs:990-1044`) and `PATCH …/roles/alice` likewise bypass the personal-login bookkeeping and the access log. `GET …/roles` lists personal logins today, with a test that asserts it (`services/orchestrator/tests/api.rs:2788-2806`), and Studio labels them (`apps/studio/src/components/branch-roles.tsx:35`).

**Ruling.** Every generic role route (list, reset, connection/reveal, update, delete, rename, sessions) treats a personal row as absent (404), and Studio's `roleChoices` skips `origin === "personal"` (`apps/studio/src/components/members/role-picker.tsx:65`). A metadata CHECK: `origin = 'personal'` ⇒ `password_enc IS NULL`. Personal logins are changed only through the personal-login API. The plan lists the existing c3 test (`api.rs:2788`) as a deliberate behaviour change. R3's "Personal logins on this branch: N" link reads the personal-login API.

### MH-12 (important) — a data key can be bound to a personal login, which also blocks its deletion

**Scenario.** An Admin creates a secret key bound to `alice` (Alice's personal login). `authorize_key_role` lets any Admin through (`services/orchestrator/src/access.rs:615-617`), and the worker runs `GRANT alice TO authenticator` as `cloud_admin` (`services/worker/src/data_api.rs:1117-1118`), which the marker guard doesn't cover (superusers are untouched). The data API now acts as Alice's personal identity. When Alice is later removed, `delete_role` refuses because the key uses the role (`roles.rs:1027-1029`), so the login outlives her access (MH-9).

**Ruling.** Key creation and the worker's key-role check refuse a personal login (origin `personal` or marked in the catalog). `plan_key_roles` revokes any live key bound to one. The personal-login delete path never waits on keys.

### MH-13 (important) — the catalog read and the 6g merge treat personal logins as ordinary roles

**Scenario.** The catalog read's `reserved` CTE (roles) and `hidden` CTE (ACL grantees) exclude `cb_m_*` and `chelabase_*` but not marked roles (`crates/chelabase-schema/src/catalog_sql.sql:4-11`, `catalog_parts/head.sql:59-63`). So a merge plans role operations for personal logins: a `Create` of Alice's child-only login on the parent (an unmarked, owner-administered LOGIN role under her name, which squats the name there), an `Update` when the member-role reconciler gave the login different memberships on each branch (refused by the guard, so the merge fails), and name-pairing (`crates/chelabase-schema/src/plan/diff/mod.rs:1237-1243`) can pair Alice's personal login `billing_admin` on the source with a group `billing_admin` on the target and plan `ALTER ROLE … LOGIN` on the group (`:1268-1270`). ACL entries naming a personal login are replayed as `GRANT … TO alice` on a target where the role may not exist, or is someone else.

**Ruling.** Both CTEs exclude marked roles, using the round-4 predicate (`pg_db_role_setting`, `setdatabase = 0`, a `chelabase.managed_role=` entry; `crates/chelabase-types/src/role_name.rs:276`, `is_marked_sql`). Merges, schema diffs and the privileges read never see them. Checklist item: confirm the owner's catalog session can read `pg_db_role_setting` (it's a shared catalog), and add a catalog unit test with a marked role in an ACL.

### MH-14 (important) — R11's shared lineage password: a reset on one branch leaves the old password valid on every other — USER DECISION

**Scenario.** Alice's password leaks; she resets it on `main`. Her login on the fifteen branches created since still accepts the leaked password, because R11 copies the catalog with the old verifier and reset acts on one branch. Restores and PITR children bring back old verifiers too (MH-10). Meanwhile a child of a protected parent gets a fresh password on every role at the resync (`resync.rs:956-1000`, `FreshPasswords`), so on those children her login exists with a password nobody knows. That's three behaviours for one credential.

**USER DECISION.**
1. **(Recommended) One login per (member, Chelabase database).** One name and one stored verifier in the metadata. The reconciler converges every ready branch of the database: the login is created (marked, with the stored verifier) where the member qualifies, dropped where they don't, and set back to the stored verifier where a copy, restore or protected child changed it. Create, reset and delete act on all branches at once, and the API takes a database, not a branch. This removes the lineage problem, the restore orphans and the protected-child case, and fits MH-19's database-wide name. Cost: a different data model and API than Part 6 (`personal_logins (member_id, database_id, role_name, verifier…)`), and each convergence touches every branch.
2. **Per branch, carried (R11 as written),** but reset and delete fan out to every branch of the database where the same member's login exists, and the protected-child fresh password is skipped for personal rows. Smaller change; the fan-out has to track lineage.
3. **Per branch, dropped on branch create.** A child never carries a login; the member creates one per branch. Simplest and strongest; costs a create on every new branch.

### MH-15 (important) — create ordering and two sources of truth

**Scenario.** Part 6 keeps `personal_logins (member_id, branch_id, role_name)` beside a `neon_roles` row of origin `personal`. The two can disagree (one written, the other not), and every reader then has to pick one. `create_personal_login` runs `CREATE ROLE` and the marker as `cloud_admin`, writes no metadata, and takes no endpoint lock (`roles.rs:532-567`). If the API writes the rows after the SQL and its request dies in between, the role exists with no row (MH-10), and the retry fails because `CREATE ROLE` refuses an existing name. The `PUT` semantics on an existing login (rename? no-op?) aren't specified.

**Ruling.** One record: the personal-login row references the `neon_roles` row (or the `neon_roles` row gains a `member_id`, non-null exactly when `origin = 'personal'`), with FKs so neither exists alone. Order under the endpoint's lock, as `create_role` does with its pending row: (1) the pending personal row with its verifier; (2) the SQL create and marker; (3) the row confirmed. A failure after (1) is completed or removed by the next resync, never adopted as `sql`. `PUT` on an existing login is 409 `personal_login_exists`; renaming means delete and create. The password is server-generated (at least 128 bits, as `credentials::generate_password`), never chosen by the client, never logged, and never written to an access event.

### MH-16 (important) — a new personal login on a protected branch is unlocked until the next reconcile

**Scenario.** On a protected branch with `guard_logins` off, the DDL guard locks a session only when its `session_user` holds `chelabase_schema_locked` (`services/worker/src/schema_guard.rs:7-10`, `:220-228`). `create_personal_login` grants no memberships (`roles.rs:556-561`), and the marker grant comes at the next reconcile (`member_roles.rs:33-38`). Between the two, a Developer's fresh login runs DDL wherever PUBLIC has CREATE (an owner may have granted CREATE on a schema to PUBLIC). With `guard_logins` on, an Admin's personal login is locked although the Admin's `cb_m_*` role is not (`schema_guard.rs:225-226`: only the owner, `cb_m_` and exemptions pass).

**Ruling.** On a protected branch, the create script grants `chelabase_schema_locked` in the same transaction for a member who isn't Admin there (fail closed: no lock, no login). The reconciler treats the personal login's lock exactly like the member role's (`Lock` before any grant). The spec states that with `guard_logins` on, every personal login is guarded unless exempted, Admins' included.

### MH-17 (important) — "losing access" needs one rule, evaluated as desired state, and revokes must end sessions

**Scenario.** (a) Part 6 names "a grant deleted, the member removed". Not covered: a downgrade to Viewer (R9 says Viewers get no login, but nothing deletes an existing one); an override that takes one database below Developer; a tenant admin cleared (or stepping down) with no project grant; a project or database deleted; a team change (step 2). (b) "Losing a role shrinks the memberships at the next reconcile": Postgres checks membership only at `SET ROLE`, so a `psql` session that already ran `SET ROLE billing_editor` keeps it after the `REVOKE`. The member roles end their sessions on a revoke (`member_roles.rs:43-45`); Part 6 says nothing about personal logins.

**Ruling.** One predicate, `qualifies(member, database)`: an active member whose effective level on that database is at least Developer (tenant admins included). The reconcile's desired set for personal logins is "records whose member qualifies", so every path above, and the sweep, converges without a per-trigger list. MH-9's immediate verifier revocation applies on every write that can make the predicate false. A personal login's revoke step ends its sessions, as a member role's does.

### MH-18 (important) — direct object privileges on a personal login escape the access model

**Scenario.** The marker guard leaves object privileges and ownership outside it (round 4 §12, S16). An owner, or a Developer acting as a role that owns tables, runs `GRANT SELECT ON payroll TO alice`. Alice's login then holds privileges directly, with no `SET ROLE`, outside "exactly the Postgres roles their access gives them" (Part 6), and they survive any shrink of her memberships. The hub's own privilege editor (Part 4) could also target her if the grantee isn't restricted (MH-28).

**Ruling.** Personal logins hold no privileges of their own. The privilege ops refuse them as grantees (MH-28). The reconcile runs the R31-style cleanup for personal logins: wherever `pg_shdepend` shows an ACL entry or ownership for the login, objects are reassigned to the owner, then `DROP OWNED` in every database, before the memberships are applied, as `cleanup_script` does for member roles (`member_roles.rs:416-425`).

### MH-19 (minor) — name squatting and look-alike names — USER DECISION

**Scenario.** A member names their login `billing_admin` on a branch where that group doesn't exist yet. The owner can then never create the group there (the name is taken), merges pair the names (MH-13), and audit logs show `billing_admin` doing things as a person. Uniqueness is only per branch (`UNIQUE (branch_id, role_name)`).

**USER DECISION.**
1. **(Recommended) Free name, unique across the database.** At create, the name is refused when any branch of the Chelabase database has a role of that name (rows in `neon_roles` for every endpoint of the database) or another member's login. The owner's later `CREATE ROLE` of that name fails on that branch, as any taken name does.
2. **A fixed shape**, e.g. `<local>_p` or a reserved prefix such as `u_<local>`, refused for owner-made roles. Squatting becomes impossible by construction, and logs read as people. Costs a less natural `psql -U` name, plus the prefix in the reserved lists and the guard.
3. **Per branch (as written).** Accept squatting; the merge fix (MH-13) still applies.

### MH-20 (minor) — a personal login outlives an IdP-side deactivation — USER DECISION

**Scenario.** Chelabase deactivates Zitadel when it removes a member (`members.rs:2036-2048`), but not the reverse: an IdP admin who deactivates Bob in Zitadel (offboarding done in the IdP) leaves Bob `active` in Chelabase. His platform session ends, but his personal-login password works through the proxy indefinitely.

**USER DECISION.**
1. **(Recommended) Expiry.** Logins are created `VALID UNTIL now() + 90 days` (an attribute the marker guard protects); a reset renews it; the member page shows the expiry.
2. **No expiry (as written).** Offboarding must go through Chelabase's "remove".
3. **A Zitadel state check** in the worker sweep that revokes the logins of users who are no longer active. Exact, but adds Zitadel calls to the worker.

### MH-21 (minor) — act-as and grants can name a personal login

**Scenario.** For an Admin's `All`, `may_act_as` accepts any non-reserved name (`access.rs:93-101`), and the R16/R22 chain check runs only below Admin (`services/orchestrator/src/access.rs:384-395`). An Admin's console request "act as alice" passes the API and fails in Postgres, because the owner holds no membership in a role `cloud_admin` created. An Admin grant's override may name `alice` (its roles aren't checked while still Admin, `members.rs:1162-1164`).

**Ruling.** `authorize` refuses act-as of a personal login for everyone, tenant admins included, with `Forbidden{"role:<n>"}`, and `check_names` (`members.rs:1121-1146`) refuses a personal-login name in any grant or override. R3's "every act-as picker and grant check" becomes these two server checks.

### MH-22 (minor) — who may read a member's personal logins

**Scenario.** `GET /v1/members/{id}/personal-logins` has no stated access. Opened to everyone (as the member page is), it reveals database and branch names on projects the caller can't see.

**Ruling.** Self sees all of their own. A tenant admin sees all. A project Admin sees those on their projects. Everyone else sees none (empty list, not 403). The endpoint's route policy is `TenantMember`, with the filter in the handler, tested row by row against Part 7's table.

### MH-23 (minor) — what an Admin's personal login holds

**Scenario.** "Exactly the Postgres roles their access gives them": an Admin's access includes the owner, but the reconciler never grants the owner (`member_roles.rs:932-951`). The spec doesn't say whether an Admin's login reaches the owner.

**Ruling.** State it: a personal login never reaches the owner or any owner-level role (the member-role safe set); Admins use the owner credential for owner work. The access log's `personal_login_created` records the member's level at creation.

---

## C. Privilege operations (Part 4, step 1)

### MH-24 (critical) — revokes (and grants) the owner can't perform succeed silently

**Scenario.** An Admin unticks SELECT for `intern` on `billing.invoices`, which `billing_owner` owns and which `billing_owner` granted to `intern`. The op renders `REVOKE SELECT ON billing.invoices FROM intern` (`crates/chelabase-schema/src/plan/ddl.rs:483-496`: no `GRANTED BY`, no `CASCADE`) and runs it as the database owner. A REVOKE removes only the privileges the current role granted. If the owner holds some privilege on the table, Postgres answers `WARNING: no privileges could be revoked` instead of an error. The transaction commits, Apply reports success, and `intern` keeps SELECT. Grants have the same shape (`WARNING: no privileges were granted`, or "not all privileges were granted"). A privilege granted by a second grantor through WITH GRANT OPTION also survives the owner's REVOKE.

**Ruling.** Apply (and the dry run) re-read the grantee's ACL for every touched object inside the same transaction, after the statements, and compare it with the requested end state. Any difference rolls back with 409 `privileges_not_applied`, naming the object, the privilege and the grantor that still holds it. Any `no privileges` / `not all privileges` warning is a failure even before that check. The read endpoint returns the grantor of each entry so the UI can say "granted by X".

### MH-25 (important) — objects not owned by the database owner — USER DECISION

**Scenario.** Tables created through the SQL editor as `billing_editor` are owned by `billing_editor` (R31 reassigns member-role leftovers to the role last acted as). The owner holds ADMIN on API-made roles but `INHERIT FALSE, SET FALSE` (`resync.rs:788-797`), and no membership in SQL-made ones. So, as the owner, `GRANT … ON` those tables, `CREATE POLICY` on them (it needs table ownership), and `ALTER DEFAULT PRIVILEGES FOR ROLE billing_editor` fail. The privilege editor would be read-only for much of a real database, and Part 4's "`FOR ROLE <creator>` (a select of the roles that own objects in the schema)" offers choices that can't run.

**USER DECISION.**
1. **(Recommended) Owner-only edits in step 1.** The editor enables changes only on objects the owner owns (and `FOR ROLE` the owner only). Other objects show "owned by X: change it in SQL as X". A later step can widen this. Cheapest and honest; the hub covers less.
2. **Run each statement as the object's owner.** The owner session `SET ROLE`s to the object owner for that statement, which needs the owner to hold SET on every non-member role. That's a change to how roles are made (`createrole_self_grant` or an explicit `GRANT … WITH SET TRUE` to the owner), and it widens what the owner credential reaches; feasibility on our Postgres versions must be checked first.
3. **Run the privilege ops as `cloud_admin`** in the worker. Everything works, but it bypasses the protected-branch guard (which skips superusers, `schema_guard.rs:220`) and any owner-level check, so every op needs its own policy re-implemented. Not recommended.

### MH-26 (critical) — the "all rows" policy can switch off RLS for the data API

**Scenario.** The data API grants `anon` SELECT, and `authenticated` SELECT/INSERT/UPDATE/DELETE, on every table of an exposed schema, existing and future (`services/worker/src/data_api.rs:398-409`), so RLS policies are the only fence for internet callers. A policy `TO billing_reader` applies to every role that has billing_reader's privileges. If `authenticated` (or `anon`, or `authenticator`, or a secret key's bound role) is a member of `billing_reader` (an owner's `GRANT billing_reader TO authenticated` in SQL is enough), then ticking "All rows" for `billing_reader` exposes every row to every signed-in app user. "Every RLS table in this schema" does it to a whole schema in one click. Targeting `anon`/`authenticated` directly is hidden only because they're reserved (`crates/chelabase-types/src/role_name.rs:6-18`), not refused by the op.

**Ruling.** The all-rows op is refused (422 `exposes_data_api`, naming the chain) when the target is, or is inherited by, `anon`, `authenticated`, `authenticator`, any role bound to a live secret key on the branch, or PUBLIC. The check runs on the catalog inside the apply transaction, over inherit-membership chains. The hub identifies its policies by shape, never by name: permissive, `FOR ALL`, roles `{role}`, `USING (true)`, `WITH CHECK (true)`, plus a `COMMENT ON POLICY` marker. Toggling off drops only a policy of that shape, so a user's own `x_all_rows` policy is never shown as the toggle or dropped. The UI says that restrictive policies still apply and that a role without SELECT sees nothing.

### MH-27 (important) — function signatures are free text

**Scenario.** `grant_function { function: { schema, name, args: [type] } }` renders `GRANT EXECUTE ON FUNCTION s.f(<args>)`. Identifiers are quoted (`render.rs:395-408`), but types are type expressions, not identifiers: `args: ["int) TO PUBLIC; GRANT ALL ON ALL TABLES IN SCHEMA public TO anon; --"]` is SQL injection unless the types are validated.

**Ruling.** Function ops take the function's OID from the privileges read (plus its schema-qualified identity for the error text). The server resolves it in the apply transaction (`oid::regprocedure`, refused if it's gone or its identity changed) and renders only the server-produced `regprocedure` text. No client type text reaches SQL. Sequences and schemas are referenced by `ObjectRef`/identifier only. A unit test per op renders hostile names and checks every statement parses as exactly one statement.

### MH-28 (important) — grantee and `FOR ROLE` allow-lists

**Scenario.** The renderer refuses only `public`, `none`, `current_user`, `session_user` and `current_role` (`render.rs:395-408`). The new ops would accept `cb_m_*` (direct privileges on a member role; R31 cleans them up only later), `chelabase_*` (`chelabase_schema_locked`, the realtime role), `pg_*`, `authenticator`, a personal login (MH-18) and the owner itself as grantees. `alter_default_privileges` would accept any `FOR ROLE`, including `postgres` with grantee `anon`, which publishes every future table to the internet.

**Ruling.** Grantees: any role on the branch except the owner, `act_as_reserved` names (`access.rs:192-197`; `anon` and `authenticated` are allowed, with the MH-26 check for all-rows), `authenticator` and personal logins; PUBLIC only for REVOKE. `FOR ROLE`: the owner, or (with MH-25 option 2) a role the session can SET to; never reserved roles or personal logins. A grant to `anon` or `authenticated` (default privileges included) is flagged in the review step as "exposes to the data API".

### MH-29 (important) — WITH GRANT OPTION creates privilege paths the hub can't follow

**Scenario.** `grant_schema … with_grant_option` (Part 4). A group holding a grant option lets every Developer who acts as it re-grant to anyone, `anon` and `authenticated` included. Each re-grant has a grantor other than the owner, which the owner's REVOKE can't remove (MH-24).

**Ruling.** The hub ops never set the grant option (the field is dropped from the new ops). The read shows existing grant options and offers "revoke grant option" only.

### MH-30 (minor) — R6 is a Studio rule unless the server enforces it, and the log must see every route

**Scenario.** The new ops live in `Operation` and run through `POST …/schema-changes`, which is `ActAsBranch { unprotected: Developer, protected: Admin }` (`crates/chelabase-routes/src/lib.rs:845-850`). A Developer acting as a role they hold can call `grant_schema`/`alter_default_privileges` through the API on objects that role owns. That's legitimate in Postgres terms, but it contradicts R6's "Admin-only". And `privileges_changed` would be written only by the hub's flow.

**Ruling.** State R6 precisely: owner-run privilege edits are Admin-only (the route's rule for an owner run). A Developer's privilege op acting as a role is what Postgres allows that role. Every privilege op, whatever the route or role, writes `privileges_changed` (actor, branch, Postgres database, ops; never SQL text).

### MH-31 (minor) — Postgres database, version and dry-run edges

**Scenario.** (a) `pg_database` in the read and the ops must resolve through the branch's recorded databases (`branch_databases`, as the table API does, `services/orchestrator/src/lib.rs:2947`). Privileges set in `template1` are copied into every future database. (b) `MAINTAIN` exists only on Postgres 17; a compute on an older major refuses it. (c) A dry run rolls back, but event triggers (the protected-branch guard) fire in it; on a protected branch a non-Admin dry run is refused like the real one.

**Ruling.** Resolve `pg_database` only through `branch_databases` (never a template or `postgres`); offer `MAINTAIN` by server version; the dry run follows the same access rule as the apply.

### MH-32 (minor) — the Viewer-level privileges read

**Scenario.** The read connects as the platform, not as the Viewer. It reveals ACLs and policy names of a Postgres database the Viewer couldn't connect to (owner revoked CONNECT, or born closed). That's not a privilege, but it's more than "any catalog reader" (Part 4) can see.

**Ruling.** State that the read is project-level (Viewer, like `LIST_OBJECTS`, `crates/chelabase-routes/src/lib.rs:821-826`). Cap the response (objects per schema, with paging), and exclude marked roles and `cb_m_*` entries (MH-13).

### MH-33 (minor) — a stale editor applies over someone else's change

**Scenario.** Two Admins edit the same role. The second Apply's ops were computed against an older read; GRANT/REVOKE are idempotent, but the review screen showed a diff that is no longer the change being made.

**Ruling.** Apply carries the read's ACL hash for the touched objects; a mismatch is 409 `privileges_changed_since_read` (the `dependents_hash` pattern of the schema-changes route).

---

## D. Roster, member page and powers (Parts 2, 3, 7)

### MH-34 (important) — R4's member counts read an Admin-only endpoint

**Scenario.** R4 computes "Members" counts on the Roles tab and role page client-side from `GET /v1/projects/{id}/grants`. That endpoint is `Project(Admin)` (`crates/chelabase-routes/src/lib.rs:740-745`), while the Roles tab is readable by Developers and Viewers (Part 7). Either the counts fail with 403 for them, or the endpoint gets widened and every Developer sees everyone's grants and overrides.

**Ruling.** Member counts and the role page's Members section are shown to Admins only (and to tenant admins). Others see the roles and privileges without holders. No widening of the grants endpoint.

### MH-35 (minor) — R1's "No access yet" group mislabels people

**Scenario.** A member with grants only on projects the caller can't see has `level: none` under R1 and sits in "No access yet", which is false and leads callers to "fix" it. A Developer opening that member's page sees nothing at all.

**Ruling.** Split the group: `none` (no grants anywhere: the server knows, and this reveals nothing beyond "has some access") versus "No access you can see", shown as "Other projects". Or keep R1 and label the group "No shared projects" for non-admins. The member page for a Developer shows the platform role per visible project only, never another member's `pg_roles` or overrides.

### MH-36 (minor) — Part 7's table against today's routes

**Scenario.** Today a project Admin may invite into P (`CREATE_PROJECT_INVITE`) and read P's access log (`LIST_PROJECT_ACCESS_EVENTS`, `Project(Admin)`, `crates/chelabase-routes/src/lib.rs:785-790`). Part 7 lists invites and Activity as tenant-admin only.

**Ruling.** Part 7's table gains rows for project invites (P only, no teams, MH-8) and P's Activity (project Admin), so the hub doesn't remove powers the API keeps.

### MH-37 (minor) — the new events need a branch, and the log only sees API actions

**Scenario.** `access_events` has `project_id` and `database_id` but no branch (`crates/chelabase-metadata/migrations/20261014000001_members.sql:44-48`); role and personal-login events are per branch. Roles and privileges changed in SQL never reach the log.

**Ruling.** Add a nullable `branch_id` column (a migration, the append-only triggers unchanged) and a `pg_database` field in `after`. Activity states "changes made through Chelabase; SQL changes appear in the branch's history, not here".

---

## E. Studio

### MH-38 (minor) — Studio edges that carry security weight

**Scenario.** (a) Step 2: the member page's grant editor shows team roles as checked and locked; if the PUT body is built from the checked set, the team's roles get baked into the own grant and survive leaving the team. (b) The back links carry `tab`, `project`, `view` and `q`; values outside their enums must be ignored, and no URL target may be read from the query. (c) The password reveal must not reach the router state, the URL, the React Query cache persistence or localStorage. (d) Role names from the catalog may hold spaces, quotes or bidi characters, and the "also gives" chains display them.

**Ruling.** (a) The PUT body is the own grant only, with a vitest that a locked team role is never sent. (b) Search params are parsed against their enums. (c) The password is held in component state of the dialog only and cleared on close. (d) Names render as text, with non-ASCII or control characters shown escaped in the checklist.

---

## Per-area checklist for implementer briefs

**Teams / access union (`crates/chelabase-types/src/access.rs`, `crates/chelabase-metadata/src/members.rs`, `services/orchestrator/src/access.rs`, `members.rs`, `lib.rs` `/me`, `projects.rs`, `internal/realtime.rs`, `services/worker/src/member_roles.rs`)**
- `union(&[Grant]) -> Option<Grant>`; an empty slice is `None` (MH-2).
- Per-role levels or the chosen MH-1 option; the exhaustive test asserts the chosen property, never "the union exceeds no source" unless that's true.
- `All` ∪ `Only(S)` keeps S's `anon`/`authenticated` (MH-3); `effective`'s every-role test accepts the new shape.
- No public own-grant field: `GrantSources::effective()` is the only way to a grant; every caller in MH-2's list is switched, with a team-only-member route test each (admission, realtime projects, project list, `/me`, roster `has_access`, validation, I4 observe, `remove_member` touched, worker reconcile).
- Team writes: tenant lock, then member locks in id order; `check_effective` on union ∩ override for every affected member and database; 422 names the member (MH-4).
- Every team write fires `access_changed` per affected member and `reconcile_projects` (MH-5).
- Team reads apply R1 visibility (MH-6); team events list affected members (MH-7).
- `team_ids` refused on the project invite route (MH-8).
- Property test: any sequence of grant, override and team writes leaves every member's effective grant passing `check_effective`.

**Personal logins, API side (`services/orchestrator`, `crates/chelabase-metadata`, migrations)**
- One record linking member to role row, with FKs; CHECK `origin='personal'` ⇒ `password_enc IS NULL` (MH-11, MH-15).
- Create: pending row → SQL create plus marker (plus the lock marker on a protected branch, MH-16) → confirm, under the endpoint lock; `PUT` on an existing login is 409; the password is server-generated and never logged or stored sealed.
- Every access-reducing write also revokes the affected logins' proxy verifiers in the same transaction (MH-9); the proxy answers no secret for a revoked login.
- Generic role routes (list, reset, connection, update, delete, rename, sessions) treat personal rows as absent; the c3 list test changes deliberately (MH-11).
- Data keys refuse personal logins (orchestrator and worker) (MH-12).
- `authorize` refuses act-as of a personal login for everyone; `check_names` refuses personal-login names in grants and overrides (MH-21).
- `GET /v1/members/{id}/personal-logins` is filtered by caller (MH-22).
- Name rule per MH-19's decision; expiry per MH-20's decision.
- `qualifies(member, database)` = active and effective level ≥ Developer (tenant admins included) (MH-17); route tests for each Part 7 row, allow and refuse.

**Personal logins, worker side (`services/worker/src/member_roles.rs`, `resync.rs`, `roles.rs`, `data_api.rs`)**
- The reconcile's desired set for personal logins comes from qualifying records; non-qualifying or orphaned marked roles are dropped as `cloud_admin` with sessions ended (MH-10, MH-17).
- The resync never records a marked role as `sql` and never writes a verifier back for a revoked record (MH-9, MH-10).
- Memberships: the member-role safe set, `INHERIT FALSE, SET TRUE` (R10), never the owner (MH-23); the lock marker before any grant on a protected branch (MH-16).
- A revoke ends the login's sessions (MH-17); reset ends them too.
- R31-style cleanup of direct privileges and ownership on personal logins before memberships (MH-18).
- `plan_key_roles` revokes keys bound to a marked role; delete never waits on keys (MH-12).
- Branch copies, restores, resets, protected children: converge per MH-14's decision. Worker unit tests for each orphan origin; live tests: a restore before deletion, and a removal while suspended.
- Forwarding stays off for every personal-login statement (`admin_sql` as `cloud_admin`, `crates/chelabase-neon/src/backend.rs:329-335`).

**Privilege ops (`crates/chelabase-schema` render, catalog, `services/orchestrator/src/table_api/changes.rs`)**
- The catalog `reserved` and `hidden` CTEs exclude marked roles (MH-13); a merge-plan test with a personal login on both sides, on one side, and a same-name group.
- New ops: no grant option (MH-29); functions by OID → `regprocedure` (MH-27); grantee and `FOR ROLE` allow-lists (MH-28).
- In-transaction verify-after-apply; `no privileges` warnings are failures (MH-24).
- The owner-ownership scope per MH-25's decision.
- All-rows: refused when the target is, or is inherited by, `anon`/`authenticated`/`authenticator`/a key role/PUBLIC; identified by shape plus a comment marker; off drops only that shape (MH-26).
- `pg_database` only via `branch_databases`; `MAINTAIN` by server version; the dry run under the apply's access rule (MH-31).
- Apply carries the read's ACL hash (MH-33); every privilege op logs `privileges_changed` whatever the route (MH-30).
- Rendering tests per op with hostile identifiers: each statement is exactly one statement.

**Roster / member API (`services/orchestrator/src/members.rs`, metadata)**
- `access.projects` and `level` over visible projects only (R1); the group label per MH-35.
- Member counts and holders only for Admins; no widening of `LIST_PROJECT_GRANTS` (MH-34).
- `access_events.branch_id` migration; `member_id` filter matches team-affected members (MH-7, MH-37).
- Part 7's table matches the routes: project invites and project Activity for project Admins (MH-36).
- `remove_member` touched includes team and personal-login projects (MH-5).

**Studio (`apps/studio/src/components/members/*`, `project-access/*`, `branch-roles.tsx`)**
- `roleChoices` skips `origin === "personal"`; the branch Roles panel stops listing personal logins (MH-11).
- The grant editor's PUT body is the own grant only (MH-38a).
- Search params parsed against enums; localStorage reads and writes in try/catch with a list fallback (MH-38b).
- The password reveal lives only in the dialog's state (MH-38c); role names rendered escaped (MH-38d).
- The privilege editor shows grantors, refuses all-rows with the server's chain message, flags data API exposure in review, and greys objects per MH-25.
- Member counts hidden for non-Admins (MH-34).
