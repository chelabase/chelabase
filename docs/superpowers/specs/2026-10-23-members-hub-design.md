# Members hub: one place for people, roles, privileges and teams (design)

Date: 2026-10-23. Branch: `members-hub`. Builds on the members-and-roles design (`docs/superpowers/specs/2026-10-09-members-and-roles-design.md`, rulings R1–R51) and fork round 4's personal-login marker (`docs/superpowers/specs/2026-10-21-neon-fork-round-4-design.md` §12).

## Why

Today `/app/members` lists people, and each row's menu can only make someone a tenant admin or remove them (`apps/studio/src/components/members/people-tab.tsx`). A person's real access is set somewhere else: per project in the project's Access tab (`project-access/`), and Postgres roles per branch in the branch Roles panel (`branch-roles.tsx`). The user wants Members to be the one place for everything about permissions and roles. People should be organized by level, each person should have a page where their platform and Postgres roles are managed, adding people should come with pre-assigned access, and there should be a place to create NOLOGIN roles and the privileges members receive through them. The UI is designed with `/impeccable` in Operate mode.

## Decisions (user, 2026-10-23)

| # | Decision |
|---|---|
| H1 | "Auth roles" means both platform roles and app-user roles. They're split into two subsystems: **A, the Members hub** (this spec) and **B, app-user roles** (end users' roles mapping to Postgres roles, its own spec next). A's Roles tab leaves room for B as a second kind of role. |
| H2 | The roster is grouped by each person's **highest level** (Tenant admins, Project admins, Developers, Viewers, No access yet, Invited). A **project filter** regroups everyone by their role on that one project. |
| H3 | A **card / list toggle** on the roster and on the Roles tab's role list. |
| H4 | The Roles tab works on **any branch, picked**: project → database → branch, then that branch's roles and privileges. |
| H5 | The **full privilege set**: tables and columns, schemas, default privileges, sequences, functions, and the "all rows" RLS toggle (members follow-up b). |
| H6 | Postgres roles are assigned with a **filterable checklist** that shows each role's "also gives" chain (R21). |
| H7 | Adding people: **teams** plus **several emails** per invite. |
| H8 | Teams are **live-linked**: effective access = own grants ∪ the member's teams' grants, and editing a team changes everyone in it. |
| H9 | **Personal Postgres logins** (members follow-up a) are in scope, on the member page. |
| H10 | The project Access tab and the branch Roles panel **stay**, rebuilt from the hub's shared components, each with a "Manage in Members" link. |
| H11 | Who edits is **scoped by existing powers**: tenant admins edit everything, project Admins edit within their projects, everyone else gets a read-only roster plus their own page. |
| H12 | **One spec, two steps**: step 1 is the hub, roles and privileges, and personal logins; step 2 is teams. Each step has its own plan, review and merge. |
| D1 | (after the red-team, MH-1) Teams combine with **per-role levels**: each Postgres role keeps the highest level among the sources naming it, and act-as checks that role's level (Part 5). |
| D2 | (MH-14) One personal login per **(member, Chelabase database)**, on every branch where the member qualifies; it replaces R11 (Part 6). |
| D3 | (MH-19) Personal-login names are **free and unique** across the database's branches and roles. Bloom filters were considered: an exact indexed lookup is cheaper, so none is used. |
| D4 | (MH-20) **Immediate revocation** through the Zitadel Actions webhook, plus a 10-minute Zitadel sweep and a 90-day `VALID UNTIL` as backstops (Part 6). |
| D5 | (user, on D4) A Chelabase-side **Suspend** (reversible) and **Revoke Postgres access now**, both immediate (Part 6). |
| D6 | (MH-25) Privilege edits on objects the owner doesn't own **run as the object's owner**, after a PG17 feasibility check (Part 4). |
| H13 | Part 1 (structure and routes) was approved in chat. Parts 2–7 are reviewed here: every choice made without a user answer is marked **Ruling** with what it costs if wrong. |

Out of scope: subsystem B (app-user roles), machine accounts (M12), platform sign-up (M15), per-Postgres-database grant overrides (R17 still holds).

## Part 1: Structure and routes (approved)

Members keeps its nav entry and becomes the hub, with five tabs:

| Tab | Who sees it | Holds |
|---|---|---|
| People | everyone | the grouped roster (Part 2), invites, pending sign-up requests |
| Roles | members with any project grant (read); edit per Part 7 | Postgres roles on a picked branch and their privileges (Part 4). Subsystem B later adds a "Postgres roles / App-user roles" switch at the top. |
| Teams | everyone (read); tenant admins edit | live-linked access bundles (Part 5, step 2) |
| Activity | tenant admins | the access log, plus the new role, privilege, personal-login and team events |
| Instance | instance admins, when switchable | unchanged |

Routes, all under `/app/members` (the nav stays on Members):
- `/app/members?tab=…&project=…&view=cards|list&q=…`: the tabs; the Roles tab adds `database`, `branch` and `pg_database`.
- `/app/members/$memberId`: a member's page (Part 3).
- `/app/members/roles/$databaseId/$branchId/$role?pg_database=…`: a role's page (Part 4).
- `/app/members/teams/$teamId`: a team's page (Part 5).

Every sub-page has a back link ("← Members", "← Roles", "← Teams"). It returns to the tab, filter and view the user left, because that state lives in the search params and the link carries them. The card/list choice is also the per-browser default (localStorage, every access in try/catch; a blocked store falls back to list).

The project Access tab and the branch Roles panel are rebuilt from the same components (grant editor, role checklist, role editor, privilege editor) and link to the matching hub page.

## Part 2: Roster

**API.** `GET /v1/members` gains `access` on each member:

```
access: {
  level: "tenant_admin" | "admin" | "developer" | "viewer" | "none",
  projects: [{ project_id, platform_role, via: "own" | "team" | "both" }]   // via: step 2; step 1 always "own"
}
```

- **Ruling R1:** `projects` lists only projects the caller may see: every project for a tenant admin, otherwise those the caller holds a grant on. `level` is computed over that same visible list, so a non-admin never learns of grants on projects they can't see. Cost if wrong: a non-admin sees others' levels as lower than they are; it doesn't leak anything.
- The effective platform role per project is the one `access::effective` gives without a database override (overrides only narrow per database; the roster shows project level).
- **MH-35:** `level: "none"` means no access anywhere; the server knows this, and saying so reveals nothing more than "has some access". A member with access only on projects the caller can't see gets `level: "other"`, shown in an **Other projects** group. The groups are Tenant admins, Project admins, Developers, Viewers, Other projects, No access yet, Invited and Suspended. A Developer viewing someone's page sees the platform role per visible project only, never another member's `pg_roles` or overrides.

**Groups.** The groups, in order, are Tenant admins, Project admins, Developers, Viewers, No access yet and Invited. Each heading carries a count, and empty groups are hidden. A person sits in the group of their highest visible level. Pending sign-up requests keep their own block above the roster (`requests.tsx`). With a project filter, the groups become that project's Admins, Developers, Viewers and "No access on <project>". Tenant admins sit in Admins, marked "Tenant admin".

**Views.**
- **Cards:** an initials avatar, the name, email, a level badge, "N projects", and the status when it isn't active. The whole card links to the person's page.
- **List:** a table with Person (a link), Level, Projects, Status, Joined and the row menu. Below 40rem the list stacks, as today.

The row menu keeps the quick actions (resend/new link/revoke invite, remove from tenant). Tenant-admin changes move to the member page. The name/email filter stays and applies within the groups.

## Part 3: Member page (`/app/members/$memberId`)

**Header:** the name, "(you)", email, status, joined date, and a level badge. Actions, by power (Part 7): make or remove tenant admin, step down, remove from tenant, and resend or revoke an invite. The confirm copy is today's (`people-tab.tsx` `copyFor`).

**Sections, in order:**

1. **Project access**: one card per visible project.
   - The platform role is a segmented control: No access / Viewer / Developer / Admin.
   - **Postgres roles** use the filterable checklist (H6). The candidates are the union of each ready database's main-branch roles, as `use-role-sources.ts` reads them today, run through `roleChoices`' existing safety filter. A filter box appears above 8 roles, checked roles come first, and each role shows "also gives: …" (its `SET ROLE` chain).
   - In step 2, roles and levels that come from a team show as checked and locked, labelled "via <team>" with a link, and can't be unchecked here.
   - **Narrow on a database** (collapsed) holds today's `database-narrowing.tsx`.
   - An **Effective** line sums it up: "Developer · billing_editor, billing_reader (+ via Billing team)".
   - A tenant admin's cards read "Admin on every project, as tenant admin" and are read-only.
   - **Ruling R2:** each card has an explicit **Save** (one `PUT /v1/projects/{id}/grants/{member_id}`), not autosave. Leaving with unsaved cards asks first (the router blocker the table editor uses). Cost if wrong: one extra click per card.
   - Projects the member has no grant on are listed under "Add access to a project", with a project picker.
2. **Personal Postgres logins** (Part 6).
3. **Teams** (step 2): chips with links; tenant admins add or remove.
4. **Activity**: this member's access events. `GET /v1/access-events` gains `member_id` (target or actor). Tenant admins only, as today.

A member viewing their own page sees sections 1–3 read-only (except their personal logins), with "Ask a tenant admin or project Admin to change your access."

## Part 4: Roles tab and privileges

**Picker:** project → database → branch, defaulting to the project's first database and its main branch. A Postgres database select (default: the branch's default database) applies to privileges only, because roles are cluster-wide and privileges are per Postgres database. The choice is held in the search params. A protected branch shows today's protected banner and its guard rules.

**Role list** (cards/list, H3), built from the branch's `GET …/roles`:
- **Cards:** the name, NOLOGIN/LOGIN, "owner-level" in the hazard tone when set, member count, and a privilege summary ("SELECT, INSERT, UPDATE on 4 tables in billing").
- **List:** the same data as columns.
- Owner and reserved roles are hidden as today.
- **Ruling R3:** personal logins are excluded from the role list and from every act-as picker and grant check (fork round 4 §12's handoff). The member page and the Activity log show them. Cost if wrong: admins look for them on the branch and don't find them; a link "Personal logins on this branch: N" under the list shows where they are.
- **Ruling R4:** the member count is computed from the project's grants (and team grants in step 2) as names. "Holds it" means the role is in the member's effective set, chain included. That makes it a client-side join over `GET /v1/projects/{id}/grants` and needs no new endpoint. Cost if wrong: counts that include people whose grant names a role missing on this branch. The role page lists them with "not on this branch".

**Create role** (a dialog): name (the API's rule), NOLOGIN by default. Under **Advanced**: can log in, member of, inherit, bypass RLS, connection limit and valid until (today's `CreateRoleRequest`); owner-level stays in the branch Roles panel only. After creating, the dialog goes to the new role's page.

**Role page** (`/app/members/roles/$databaseId/$branchId/$role`):
- **Header:** the name, its attributes (today's `RoleAttributes`), and "This branch only: to have it elsewhere, merge or reset the other branch" (H4).
- **Privileges** (the H5 editor), grouped by schema. Each schema block holds:
  - **Schema**: USAGE, CREATE checkboxes.
  - **Tables and views**: a matrix of objects × SELECT, INSERT, UPDATE, DELETE, TRUNCATE, REFERENCES, TRIGGER, MAINTAIN, with a column drill-in per table for column privileges, and "all" toggles on each row and column.
  - **All rows**: on tables with RLS on, a toggle. It creates a permissive policy `FOR ALL TO <role> USING (true) WITH CHECK (true)` named `<role>_all_rows`, truncated to 63 bytes with a hash suffix on a clash. The block also offers "every RLS table in this schema".
  - **Sequences**: USAGE, SELECT, UPDATE.
  - **Functions and procedures** (by signature): EXECUTE.
  - **Future objects**: default privileges for new tables, sequences and functions, `FOR ROLE <creator>` (a select of the roles that own objects in the schema, the owner first) `IN SCHEMA <schema>`.
- **Ruling R5:** editing is staged. Checkboxes collect changes, and **Review changes** shows the SQL from a dry run (the schema-changes `dry_run`). **Apply** runs every change in one transaction. This is the table editor's model, and it costs one extra click.
- **Members:** who holds the role: directly, via a team (step 2), or via another role's chain. Each person links to their page.
- **Memberships:** which roles this one is in, and which are in it; edits go through today's `PATCH …/roles/{role}` `member_of`.
- **Danger zone:** delete the role (today's delete, with its in-use-key handling).

**Backend: new schema-change operations** (in `Operation`, rendered and run like `grant`/`revoke`):
- `grant_schema` / `revoke_schema` `{ schema, grantee, privileges: [usage|create], with_grant_option }`
- `grant_sequence` / `revoke_sequence` `{ sequence: ObjectRef, grantee, privileges: [usage|select|update] }`
- `grant_function` / `revoke_function` `{ function: { schema, name, args: [type] }, grantee, privileges: [execute] }`
- `alter_default_privileges` `{ for_role, schema, object: tables|sequences|functions, grant: bool, grantee, privileges }`
- The "all rows" toggle uses the existing `create_policy` / `drop_policy`. It needs no new op.

**Backend: a privileges read.** `GET /v1/databases/{id}/branches/{branch_id}/pg-databases/{name}/privileges?role=<name>` returns that grantee's schema, table, column, sequence, function and default ACL entries, plus the `_all_rows` policies. It comes from `chelabase-schema`'s catalog model, which already reads `nspacl`, `proacl` and `pg_default_acl` (`crates/chelabase-schema/src/catalog_sql.sql`). Viewer level: ACLs are readable by any catalog reader.

**Ruling R6 (restated, MH-30):** owner-run privilege edits are Admin-only, the route's rule for an owner run (M3: "Postgres role management" is Admin). A Developer's privilege op acting as a role does what Postgres allows that role. Developers and Viewers read.

**After the red-team (supersedes the op shapes above where they differ):**
- **Run as the object's owner (D6, MH-25):** each statement on an object the database owner doesn't own runs after `SET ROLE <object owner>`. The database owner already holds ADMIN on every API-made role (`services/worker/src/resync.rs:790-795`, `WITH ADMIN OPTION, INHERIT FALSE, SET FALSE`), so it can already grant itself membership; this turns that into `SET TRUE` and adds no real power.
  - **Step 1's first task is a PG17 feasibility check:** SET on API-made roles, the protected-branch guard under `SET ROLE`, `ALTER DEFAULT PRIVILEGES FOR ROLE`, and the member-role safety walk (`unsafe_node`) with the owner as a member of groups.
  - Objects owned by roles the owner can't SET to (SQL-made roles without the owner's ADMIN, personal logins, reserved roles) are shown read-only: "owned by X: change in SQL as X".
- **Checked results (MH-24):** inside the apply transaction, after the statements, the server re-reads the grantee's ACL for every touched object and compares it with the requested end state. Any difference, or any `no privileges` / `not all privileges` warning, rolls back with 409 `privileges_not_applied`, naming the object, the privilege and the grantor that still holds it. The read returns each entry's grantor, so the UI can say "granted by X".
- **All rows (MH-26):** refused (422 `exposes_data_api`, naming the chain) when the target is, or is inherited by, `anon`, `authenticated`, `authenticator`, a role bound to a live secret key on the branch, or PUBLIC. The check is made on the catalog in the apply transaction, over inherit chains.
  - The hub identifies its policy by shape plus a `COMMENT ON POLICY` marker, never by name. It is permissive, `FOR ALL`, roles `{role}`, `USING (true)`, `WITH CHECK (true)`. Toggling off drops only such a policy.
  - The UI says that restrictive policies still apply and that a role without SELECT sees nothing.
- **Functions (MH-27):** function ops take the function's OID from the privileges read. The server resolves it in the apply transaction (`oid::regprocedure`; refused if it's gone or its identity changed) and renders only that server-produced text. Sequences and schemas are referenced as `ObjectRef`/identifiers.
- **Grantees and `FOR ROLE` (MH-28):**
  - Grantees: any role on the branch except the owner, `act_as_reserved` names (`anon`/`authenticated` allowed), `authenticator` and personal logins. PUBLIC only for REVOKE.
  - `FOR ROLE`: the owner, or a role the owner can SET to (D6); never reserved roles or personal logins.
  - The review step flags any grant to `anon`/`authenticated` (default privileges included) as "exposes to the data API".
- **No grant option (MH-29):** the new ops have no `with_grant_option`. The read shows existing grant options, and the UI offers only "revoke grant option".
- **Edges (MH-31–MH-33):**
  - `pg_database` resolves only through `branch_databases`, and `MAINTAIN` is offered by server version.
  - The dry run follows the apply's access rule.
  - The privileges read is project-level (Viewer, like `LIST_OBJECTS`), paged per schema, and excludes marked and `cb_m_*` entries.
  - Apply carries the read's ACL hash for the touched objects; a mismatch is 409 `privileges_changed_since_read`.
- **Holders (MH-34, replaces R4):** member counts and the role page's Members section are shown to project Admins and tenant admins only (the grants list is Admin-only). Others see roles and privileges without holders.
- **Log (MH-30, MH-37):** every privilege op, whatever its route, writes `privileges_changed` (actor, branch, Postgres database, ops; never SQL text). `access_events` gains a nullable `branch_id` (the append-only triggers unchanged). Activity says "changes made through Chelabase; SQL changes appear in the branch's history".

## Part 5: Teams and invites (step 2)

**Data** (metadata migration):
- `teams (id, tenant_id, name, description, created_by, created_at, updated_at, UNIQUE (tenant_id, lower(name)))`
- `team_grants (team_id, project_id, platform_role, pg_roles text[], …, PRIMARY KEY (team_id, project_id))`, with the same checks as `project_grants`
- `team_members (team_id, member_id, added_by, added_at, PRIMARY KEY (team_id, member_id))`
- **Ruling R7:** teams carry no per-database overrides. A member's own overrides narrow the union, so a team can be cut down for one person on one database. Cost if wrong: a later migration adds `team_grant_overrides`.

**Effective access** (the one check), with **per-role levels (D1, MH-1)**:
- The member's platform level on a project is the highest among their own grant and their teams' grants.
- **Each Postgres role keeps its own level:** the highest level among the sources that name it. A role a Viewer team gives stays a Viewer-level role even when the member is Developer through their own grant.
- Act-as checks a route's minimum level against **that role's level**, not the member's top level. Personal logins get only roles held at Developer or above.
- Admin from any source still means every role (R17).
- The member's database override narrows afterwards: it caps the platform level and every role's level, and its role list intersects the role set (as today, `crates/chelabase-types/src/access.rs:144`).
- A tenant admin is unchanged.
- **Types (MH-2, MH-3):** the metadata exposes `GrantSources { own, teams, database }` and no public own-grant field. Its one method `effective(&self) -> Option<Effective>` (`None` with no source) does union then narrowing, so a caller that reads only the own grant doesn't compile.
  - `Effective` carries the platform level and a role → level map (or `All` with named data API extras: the union of `All` with a set naming `anon`/`authenticated` keeps them, R18).
  - Every caller is fed by one shared SQL shape that returns own and team grants together, and each gets a route test with a team-only member: orchestrator `members.rs` `effective_on`, the table API, console, SQL editor, merges, data keys, owner reveal/reset, realtime projects, the project list, `/me`, admission, `has_access`, validation, `remove_member`, and the worker's member-role reconcile. The plan lists each by file.
- **Writes (MH-4, MH-5):** every team write (grant put/delete, member add/remove, team delete) runs `check_effective` on the union for every affected member and database, with their overrides (422 naming the member and database). It takes the tenant lock, then each affected member's lock in id order. Override writes validate against the union. One helper, `access_changed_for(members, projects)`, runs every grant, override and team write's side effects: `access_changed` per member, `reconcile_projects`, and personal-login revocation (Part 6). `remove_member` also touches the projects of the member's team grants and personal logins.
- **Visibility (MH-6, MH-8):** team reads apply R1, so a non-tenant-admin sees a team's members and only the project cards of projects they hold a grant on. `team_ids` is accepted only on the tenant invite route; the project invite refuses it (422).
- **Log (MH-7):** team events carry `affected_member_ids` in `after` (and in `before` for removals and deletes), and the member page's `member_id` filter matches them.

**Triggers:** changing a team's grants, adding or removing a member, or deleting a team runs the member-role reconcile for the affected members on every branch of the affected projects, through the same path a grant change takes. Removing a member from the tenant drops their team memberships.

**Access log:** `team_created`, `team_updated`, `team_deleted`, `team_grant_put`, `team_grant_deleted`, `team_member_added`, `team_member_removed`, with `before`/`after`.

**Teams tab:** cards/list of teams (name, member count, projects). A team's page has a name and description, its **Members** (a filterable checklist of people), and **Project access** cards using the member page's grant editor without narrowing. Deleting a team asks first and names how many people lose access through it.

**Invites (step 2 for teams; step 1 for several emails):**
- An **Emails** field takes pasted or typed addresses separated by commas, spaces or newlines, shown as chips. Invalid addresses are flagged per chip.
- **Access** offers teams (a checklist, step 2), **Copy from a member** (one-time: fills custom rows from that member's own grants, not their teams), and custom per-project rows (today's `grant-rows.tsx`). Teams and custom rows combine.
- **Ruling R8:** Studio sends one `POST /v1/invites` per email, in sequence, and shows a result per email (sent / link made / failed, with the reason). There's no batch endpoint, at most 50 emails per send, and link invites are listed together, each shown once. `POST /v1/invites` gains `team_ids`. Cost if wrong: slower large invites, roughly 50 sequential requests.
- Approving a pending sign-up offers the same access fields.

## Part 6: Personal Postgres logins (step 1)

A member's own LOGIN role in a Chelabase database, so they can connect with `psql` as themselves through the proxy (on any branch), with exactly the Postgres roles their access gives them. Revised after the red-team (D2–D4, MH-9–MH-23).

**Unit (D2):** one login per **(member, Chelabase database)**, with one name and one stored verifier, present on every branch of the database where the member qualifies. Create, reset, renew and revoke act on all branches at once. It replaces the per-branch design and R11.

**Data:**
- `personal_logins (member_id, database_id, role_name, verifier_enc, valid_until, revoked_at, created_at, PRIMARY KEY (member_id, database_id), UNIQUE (database_id, role_name))`.
- Each branch's `neon_roles` row of origin `personal` gains `member_id`, non-null exactly when `origin = 'personal'`. FKs make sure neither row exists alone (MH-15), and a CHECK: `origin = 'personal'` ⇒ `password_enc IS NULL` (MH-11).
- The proxy reads the verifier through `routable_role_secret` (`crates/chelabase-metadata/src/neon.rs:739-764`). A revoked or expired login has no verifier there (MH-9).

**API:**
- `GET /v1/members/{id}/personal-logins` lists a member's logins. Self sees all of their own, a tenant admin all, a project Admin those on their projects, everyone else an empty list (MH-22).
- `PUT /v1/databases/{id}/personal-login` `{ name }` creates the caller's own login (409 `personal_login_exists` if one exists; renaming means delete and create).
- `POST /v1/databases/{id}/personal-login/reset-password` resets it, renewing `valid_until`.
- `DELETE /v1/databases/{id}/personal-logins/{member_id}` deletes one (own, or by a tenant admin or a project Admin of the database's project).
- Create and reset return the server-generated password (at least 128 bits, `credentials::generate_password`) once, with a connection string. It's never readable again, never logged, never in an access event (MH-15).

**Rules:**
- **Qualifying (MH-17):** one predicate, `qualifies(member, database)`: an active, not suspended member whose effective level on that database is at least Developer (tenant admins included). Only a qualifying member creates or resets, and only their own. **R9:** Viewers get no personal login.
- **Name (D3):** free. It defaults to the email's local part sanitized to `^[a-z_][a-z0-9_]{0,62}$`, and the usual reserved-name rules apply. It must be unique across every branch of the database and against every existing role there, checked by an exact indexed lookup on `personal_logins` and `neon_roles`. Bloom filters were considered (user's suggestion) and not used: the indexed lookup is already exact and O(log n), and a filter would add false positives that still need it, plus staleness. A later `CREATE ROLE` of that name in SQL simply fails.
- **Create order (MH-15):** under the endpoint's lock, as `create_role` does: (1) the pending record with its verifier; (2) the worker's `create_personal_login`: `CREATE ROLE` (it fails if the name exists, never adopting a customer's role), the marker, and on a protected branch, for a non-Admin, the grant of `chelabase_schema_locked`, all in one transaction (fail closed, MH-16); (3) the record confirmed. A failure after (1) is completed or removed by the next resync, never adopted as `sql`.
- **Convergence (MH-10, MH-17):** the desired state is "records whose member qualifies, unrevoked and unexpired", run by the resync and the member-role reconcile alike, on every branch of the database. A marked role is never recorded as `sql`. A marked role without such a record is dropped as `cloud_admin` (sessions ended), and until then its row carries no verifier. New branches get the login through this reconcile, not through the catalog copy alone.
- **Memberships:** the member-role reconciler manages them as it does the member's `cb_m_*` role. It uses the effective safe set on the branch, **only roles held at Developer or above** (D1), and R21 chains. The lock marker comes before any grant on protected branches, and with `guard_logins` on every personal login is guarded unless exempted, Admins' included (MH-16). It never reaches the owner or any owner-level role, so Admins use the owner credential for owner work (MH-23). **R10:** `INHERIT FALSE, SET TRUE`, as member roles (in `psql`: `SET ROLE billing_editor`).
- **No privileges of its own (MH-18, amended by the user 2026-10-23 after T6's review):** the privilege ops refuse personal logins as grantees. The reconcile runs Postgres' canonical role-retirement procedure before applying memberships (and when dropping a login): `REASSIGN OWNED BY <login> TO chelabase_orphaned`, then `DROP OWNED BY <login>`, in every database.
  - `chelabase_orphaned` is a fixed, reserved NOLOGIN role with no members and no privileges, created when missing. Reassigning to the database owner would let a planted `SECURITY DEFINER` function or view run as the owner, so it isn't used.
  - EXECUTE is revoked from PUBLIC on every function reassigned to it.
  - Nothing is lost: an Admin reassigns or drops the quarantined objects later in SQL.
- **Hidden elsewhere (MH-11–13, MH-21):** every generic role route treats a personal row as absent (404). `authorize` refuses act-as of a personal login for everyone, tenant admins included. `check_names` refuses one in grants and overrides. Key creation and the worker's key-role check refuse one, and `plan_key_roles` revokes any key bound to one (deletion never waits on keys). `catalog_sql.sql` and the 6g merge exclude marked roles (the round-4 predicate, `role_name.rs:276`). Studio's `roleChoices` skips `origin === "personal"`.
- **Revocation is immediate (MH-9, D4):** every metadata write that can make `qualifies` false marks the affected logins `revoked_at` and clears their verifiers in the same transaction, so the proxy refuses them at once. Those writes are: grant delete, override below Developer, team change, tenant admin cleared, member removed, suspend, and "Revoke Postgres access now". The write also invalidates the proxy's role-secret cache. The worker then drops the role on running computes and ends its sessions; a suspended compute needs nothing, because logins go through the proxy. Nothing may write a verifier back for a revoked record.
- **Identity-provider offboarding (D4):** the Zitadel Actions v2 webhook the orchestrator already receives for tenant members (`services/orchestrator/src/internal/app.rs:551-566`, W18) revokes a member's logins on `user.deactivated`, `user.locked` or removal, through the same write. Two backstops cover a missed webhook (Zitadel down, or no signing key): a worker sweep every 10 minutes that checks Zitadel user state, and `VALID UNTIL` 90 days on every login, renewed by a reset and shown on the member page.

**UI (member page section 2):** a list of the member's logins (database / name / expires / created) with Reset password and Delete, plus **New login**: project → database, then a name, then a dialog that shows the password and the connection string once (today's `CopyButton`, state held in the dialog only and cleared on close), with "You won't see this password again." Expiry within 14 days shows a "Renew" prompt.

**Suspend and immediate revoke (D5):** two tenant-admin actions on the member page header, both immediate, both on the revoke path above.
- **Suspend:** a `suspended` member status. It ends the member's Studio sessions and revokes their personal logins, ends their member-role sessions, and locks the user in Zitadel. Grants and teams are kept, so **Reinstate** unlocks the user and restores access; personal logins are then made again by the member.
- **Revoke Postgres access now:** revokes the member's personal logins and ends their live sessions, for a suspected leaked password. It also rotates the member's hidden `cb_m_*` passwords on every branch, so their SQL-editor and console sessions end and reconnect (user, 2026-10-23, plan P16). Studio access is otherwise untouched, and the member can create new logins.
- Remove from tenant stays the permanent option.

## Part 7: Who can do what, the step split, testing, and design

**Powers (H11):**

| Action | Tenant admin | Project Admin | Developer / Viewer | Self |
|---|---|---|---|---|
| See roster, groups, cards | ✓ | ✓ (levels over visible projects, R1) | ✓ (same) | ✓ |
| Open a member's page | ✓ | ✓ (their projects' cards only) | ✓ read-only | ✓ |
| Tenant admin on/off, remove from tenant, invites | ✓ | — | — | step down |
| Edit a member's grant on project P | ✓ | ✓ on P (today's grant API rules) | — | — |
| Roles tab: read | ✓ | ✓ | ✓ on their projects | — |
| Create/edit/delete roles, privileges (R6) | ✓ | ✓ on P | — | — |
| Teams: create/edit/members (step 2) | ✓ | — | — | — |
| Personal login: create/reset | own | own | own (Developer+, R9) | ✓ |
| Personal login: delete | any | on P | — | own |
| Suspend / reinstate, revoke Postgres access now (D5) | ✓ | — | — | — |
| Invite into P (rows on P only, no teams; MH-8, MH-36) | ✓ | ✓ on P | — | — |
| P's Activity (MH-36) | ✓ | ✓ on P | — | — |

**Studio rules with security weight (MH-38):**
- A grant PUT body carries the own grant only, with a vitest that a locked team role is never sent.
- Search params are parsed against their enums.
- A shown password lives only in the dialog's state and is cleared on close.
- Role and member names render as text, with non-ASCII or control characters shown escaped in the checklist.

**Red-team:** `docs/superpowers/specs/2026-10-23-members-hub-red-team.md` (MH-1–MH-38) holds the findings and the per-area checklist for the implementer briefs. Every proposed ruling there is adopted except where D1–D6 decided otherwise: MH-14/15/17 are adapted to the per-database login, MH-28's `FOR ROLE` follows D6, and MH-35 takes the split-group option.

**Step 1** (its own plan, review and merge):
- the roster API (`access`) and grouped roster with cards/list;
- the member page, without teams;
- the Roles tab, role page and privilege editor, with the new ops and the privileges read;
- personal logins (API, worker wiring, exclusions, immediate revocation, the webhook path, the sweep, `VALID UNTIL`), plus Suspend / Reinstate and "Revoke Postgres access now" (D5; events `member_suspended`, `member_reinstated`, `personal_logins_revoked`);
- the privilege-ops feasibility check (D6) as the first task;
- several emails per invite;
- the Access tab and branch Roles panel rebuilt on the shared components;
- the Activity log's new events (`role_created`, `role_updated`, `role_deleted`, `privileges_changed`, `personal_login_created`, `personal_login_reset`, `personal_login_deleted`).

**Step 2:** teams (data, `access::union`, the one query, reconcile triggers, log events, the Teams tab and team page), team pickers in invites and approvals, and "via team" on the member and role pages.

**Process:**
- Step 1's security core is the privilege ops (run as the owner) and personal logins (a new proxy-reachable credential). Step 2's is the union in the one access check.
- One opus red-team attacks this whole spec now, including the interactions with the member-role reconciler, the protected-branch guard, the proxy and branch copies. Its findings become spec rulings, and a per-area checklist goes into each step's implementer briefs.
- Each plan gets its seam pass.

**Design (`/impeccable`, Operate mode):**
- It works inside Studio's existing Basalt system; there is no new visual world.
- Before step 1's Studio tasks, a shaping pass produces comps of the roster (cards and list, grouped, filtered), the member page, the Roles tab and role page with the privilege matrix, the create-role dialog, the personal-login reveal and the multi-email invite. Step 2 adds the team page and team pickers.
- The user approves the comps before the Studio tasks start. The Studio tasks build to them, and the end-of-step review includes Impeccable's finish review.

**Testing (by layer; the plans pin the names):**
- **Unit, `chelabase-types`:** `GrantSources::effective` is checked against a model by an exhaustive test over small grant sets. For every role, its effective level never exceeds the highest level of a source naming it, nor the override's cap. Admin from any source gives `All` (keeping named data API extras). The result is commutative and idempotent. A property test: after any sequence of grant, override and team writes, every member's effective grant passes `check_effective` (MH-4).
- **Unit, schema crate:** each new op's SQL rendering, quoting included.
- **Orchestrator:** route tests for each new endpoint's powers (the table above, row by row, for allow and refuse); R1's visibility filter; the personal-login API's self-only rules.
- **Worker:** the reconciler covers personal logins: the desired set over every branch of the database, the lock marker, shrink, the R31-style cleanup, drop on lost access, and a new branch getting the login. The resync never adopts a marked role as `sql`; there is a unit test for each origin of an orphan, plus one live restore test (MH-10). These extend the live convergence property test (members spec test 6) to personal logins and team grants.
- **Live, revocation:** remove a member while the compute is suspended, and the proxy refuses the login before any compute work (MH-9). The same holds for suspend, for "revoke Postgres access now", and for a Zitadel `user.deactivated` webhook. An expired `VALID UNTIL` is refused. The sweep revokes when the webhook was missed (fake Zitadel).
- **Live:** a personal login connects through the proxy and can `SET ROLE` only into its safe set; it's refused on a protected branch's DDL as a guarded login; a privilege set from the role page is visible to that role in SQL; the all-rows policy lets a role see rows under RLS.
- **Studio (vitest):** grouping and the filter, the card/list toggle and its persistence fallback, the checklist (filter, chains, locked team rows), the staged privilege editor (review/apply/conflict), invite results per email, the back links restoring state.
- **e2e/Playwright:** one pass through roster → member → grant → role → privileges → personal login, and in step 2 team → member effective access.
