# Members and roles: design

**Date:** 2026-10-09
**Status:** Design approved in conversation (parts 1–7). This spec awaits the user's review.
**Branch:** `members` (`.worktrees/members`)
**Roadmap:** step 8, before the table editor (TE-A, TE-B). Decisions M1–M15 are in the roadmap's "Resume here".

## Why

The user wants departments to edit their own things. For example, the owner creates a `billing` table, and the billing department edits it freely while nobody else can. Postgres can already enforce this: a group role owns its tables and schemas, and grants control rows. What's missing is the link between a Studio person and the Postgres roles that person may act as, plus platform powers per person. Today every route checks only that the caller's org maps to the tenant (`load_owned`), and every Studio sign-up joins the single configured org. So anyone who signs up can open the SQL editor as the owner, reveal the owner password and delete databases.

The table editor (TE-A) builds its DDL and row authorization on this step.

## Goals

- People are **members** of a tenant. They are invited, or they sign up when the instance allows it. Each person's access is explicit.
- Per project, a member has a **platform role** (Admin / Developer / Viewer) and a set of **Postgres roles** they may act as, optionally narrowed per database.
- The boundary is **real**: it covers every route that acts as a Postgres role or exposes one, and Postgres enforces it too, inside user SQL.
- There's a **clear boundary between self-hosted and platform mode**, held in one place.

## Non-goals (later steps)

- Machine accounts and service tokens. A token acts as the person who minted it.
- Platform-mode sign-up (one tenant and org per customer) and operator pages. Platform mode fails closed here.
- Per-branch grants. The scope is per project with per-database narrowing.

## Decisions (from the question round)

| # | Decision |
|---|---|
| M1 | **Amended 2026-10-09 (R34 revised, R35, R39, R40):** invites first. Self-hosted: sign-up off unless a tenant admin turns on open sign-up; sign-ups then wait as `pending` until a tenant admin approves (with project access) or declines. Zitadel's own registration is off. Invites go by email or, without SMTP, by a one-time link. Platform-mode sign-up is designed later and is refused until then. |
| M2 | Platform roles plus Postgres roles. |
| M3 | Per project: Admin / Developer / Viewer (matrix in "Access per route"). **Admin implies the database owner (R7, user 2026-10-09);** Developer and Viewer can never act as or reach the owner. |
| M4 | One grant per (member, project), plus an optional override per Chelabase database (its own cluster) that only narrows. **R17 (user, 2026-10-09):** per-Postgres-database overrides were dropped to avoid a fork change; restricting a role to one Postgres database is done with Postgres privileges (rights and `CONNECT` only there). Platform-level per-Postgres-database overrides may return with a future fork round. Schema isolation inside a database comes from Postgres roles. |
| M5 | Per-member hidden login roles in Postgres. User SQL runs on a session that can only become the member's allowed roles. |
| M6 | Tenant admins manage people, projects and the sign-up switch, and are Admin on every project. Project Admins may also invite new people straight into their project. |
| M7 | Viewers get generated reads only (console reads, structure reads). They get no SQL editor, writes, connection strings or keys. |
| M8 | Invites use Zitadel invite codes and a new `/accept-invite` page. |
| M9 | The first admin comes from the bootstrap, or is the first sign-in on a tenant with no members (an atomic claim). Existing users get no automatic access. |
| M10 | App auth settings: Admin. App user management: Developer. App user list: Viewer. |
| M11 | Access changes go to an append-only `access_events` table, shown in Studio, plus an `info` log line. |
| M12 | Machine access comes later. |
| M13 | Studio gets a "Members" rail item, a project "Access" tab and a "no access yet" screen. |
| M14 | An Instance scope sits above tenant, and one `ModePolicy` holds every mode-dependent rule. |
| M15 | Platform mode fails closed in this step. |

## 1. Data model (metadata migrations)

- **`members`**:
  - Columns: `id uuid`, `tenant_id`, `zitadel_user_id text`, `email text`, `display_name text`, `tenant_admin bool`, `status` (`invited | active | removed`), `invited_by uuid NULL`, `created_at`, `updated_at`, `accepted_at NULL`.
  - Unique on (`tenant_id`, `zitadel_user_id`).
- **`project_grants`**:
  - Columns: primary key (`member_id`, `project_id`), `platform_role` (`admin | developer | viewer`), `pg_roles text[]`, `granted_by`, timestamps.
  - CHECK: `pg_roles` never contains `owner` (R7: owner is implied by Admin and never stored). The same CHECK is on overrides.
  - Every `pg_roles` entry is a Postgres role name. Reserved names (`owner`, the database's real owner role, `cb_m_*`, `chelabase_*`, the platform roles) are refused at the API.
- **`database_grant_overrides`**:
  - Columns: `member_id`, `database_id` (a Chelabase database: its own cluster), `platform_role NULL`, `pg_roles text[] NULL`, `set_by`, timestamps (R17: no `pg_database` column).
  - Primary key (`member_id`, `database_id`).
- **Effective grant:**
  - **Effective grant = project grant ∩ Chelabase-database override.** The platform role is the lower of the two, and the role set is their intersection; a missing override doesn't narrow. Overrides can never widen access, including after the project grant changes later.
  - **Admin implies owner (R7):** an effective platform role of Admin always includes the database owner; a narrowing override to Developer or Viewer removes it in that database. A tenant admin's effective grant on every project is Admin with every role (including the owner).
- **`invites`**: `id`, `member_id` (the `invited` member), `zitadel_user_id`, `invited_by`, `expires_at`, `revoked_at NULL`, `accepted_at NULL`. Grants for an invited member are stored up front and only count once the member is `active`.
- **`tenants.open_signup bool NOT NULL DEFAULT false`.**
- **`member_branch_roles`** (reconciler state):
  - Columns: primary key (`member_id`, `branch_id`, `database_name`), `role_name`, `password_enc`, `applied_roles text[]`, `applied_at`.
  - The password is sealed with a new `Purpose::MemberRole`, with AAD = branch + role name.
- **`access_events`**:
  - Columns: `id bigserial`, `tenant_id`, `actor_member_id`, `action`, `target_member_id NULL`, `project_id NULL`, `database_id NULL`, `before jsonb`, `after jsonb`, `at`.
  - A trigger refuses UPDATE and DELETE.
  - Each access change writes exactly one event, in the same transaction as the change.
- **Invariant: a tenant always has at least one active tenant admin.** Every change that could break it (demoting or removing a member) locks the tenant row and re-checks in the same transaction. A violation returns 409 `LastTenantAdmin`.

## 2. Policy and the mode boundary

### `ModePolicy` (chelabase-types)

- One struct built from `Mode` holds every mode-dependent rule:
  - who is an instance admin;
  - what sign-up does;
  - whether open sign-up can be switched on;
  - the existing defaults: secure cookies, email verification, password rules, the owner-superuser refusal, the CA file on reveal.
- Today's scattered `mode ==` checks move into it.
- Each rule has a per-mode test, so the full self-hosted vs platform table lives in one file.
- The docs page "Self-hosted vs platform" is generated from it.

### Scopes and `Access`

- Scopes nest as **instance → tenant → project**.
- `chelabase_routes::Endpoint` gains a required `access: Access` field, so a route without a policy doesn't compile. `Access` has these variants:

| Variant | Meaning |
|---|---|
| `Public` | No identity (health, openapi, accept-invite by code). |
| `SignedIn` | Any verified identity (`/v1/me`). |
| `Instance` | Self-hosted: an active tenant admin of the single tenant. Platform: a member of `CHELABASE_OPERATOR_ORG_ID`, whatever their tenant role; with it unset, nobody passes. |
| `TenantMember` | Any active member of the caller's tenant. |
| `TenantAdmin` | An active tenant admin. |
| `Project(min)` | Effective platform role ≥ `min`. |
| `Branch(min_unprotected, min_protected)` | Picks the minimum by the target branch's protection. For merges, the target is the branch merged into. |
| `ActAs(min)` | Effective platform role ≥ `min`, and the requested Postgres role is in the effective set. No role (owner) needs Admin. |

### Decision and resolution

- `access::decide(policy, caller, effective_grant, branch_protected, requested_role, access) -> Allow | Deny(reason)` is pure.
- `access::authorize(state, principal, access, path ids)` works in this order:
  1. It resolves project ← database ← branch.
  2. It loads the member and their effective grant in one indexed query, with no cache, so a revoke takes effect on the next request.
  3. It calls `decide`.
- `authorize` **replaces `load_owned`**. Handlers receive `Authorized { record, grant }`, and no other function loads a database for a caller.

### Answers

| Case | Status | Code |
|---|---|---|
| No grant on the project (existence isn't revealed) | 404 | `NotFound` |
| Grant too low, or role not allowed | 403 | `Forbidden` (`extra.required`) |
| Signed in, member, no grants anywhere | 403 | `NoAccessYet` |
| Feature not designed for platform mode | 403 | `NotAvailableInPlatformMode` |

`/v1/me` returns the caller's capabilities: instance admin, tenant admin, and per project the role and allowed Postgres roles. Studio only hides things; the API decides.

## 3. Member roles in Postgres

- **One login role per (member, branch):** `cb_m_<member uuid hex32>` (37 characters). Each Chelabase database is its own cluster, so roles never cross Chelabase databases (R17).
- **Attributes:** `LOGIN NOINHERIT NOCREATEDB NOCREATEROLE NOBYPASSRLS NOREPLICATION`.
- **Memberships:**
  - `GRANT <r> TO <member role> WITH INHERIT FALSE, SET TRUE, ADMIN FALSE` for each role in the effective set that exists on the branch.
  - The member role holds no rights of its own and can only become those roles.
  - It is **never** granted the owner. An Admin acting as owner uses the owner connection, as today.
  - **R16 (user, 2026-10-09): owner-level roles are Admin-only, like the owner.** A role that can reach the owner, `neon_superuser` or `cloud_admin` through memberships, or has SUPERUSER, REPLICATION, CREATEROLE or BYPASSRLS, is never granted to a member role. Only Admins act as it, through the owner connection. Grants naming it are refused for non-Admins. (A later table-editor toggle may give a Developer's role "all rows" through permissive `USING (true)` policies on every RLS table, which needs no owner-level attribute.)
  - **R21 (user, 2026-10-09): a granted role includes its chain.** A member granted `billing_admin`, which can `SET ROLE billing`, may act as both. A granted role whose chain reaches anything owner-level is refused at grant time and never applied. Studio shows the expanded set.
  - **R22 (user, 2026-10-09): Postgres convention.** Permissions go to NOLOGIN group roles, and each person is a LOGIN role (their member role) made a member of those groups. Grants name NOLOGIN roles only. A granted role whose chain reaches a LOGIN role, or that holds ADMIN on any role, is excluded. Acting as a LOGIN role (an app's identity) is Admin-only, as are login-role connection strings and keys bound to LOGIN roles. A personal login per member, so a person can connect `psql` as themselves, is a follow-up.
  - **R23:** `CREATEDB` counts as owner-level (as in 6a). A chain through any reserved role excludes the granted role.
- **Applied out of band:**
  - The worker creates and changes member roles through `admin_sql` (`cloud_admin`, `neon.forward_ddl=off`), so they're never forwarded to the DDL receiver.
  - They have no `neon_roles` row, so they **can't log in through the public proxy**. Only the orchestrator, connecting directly to the compute, can use them.
- **Passwords:** a fresh password per role and branch, with the SCRAM verifier in `pg_authid`. The sealed password goes in `member_branch_roles`.
- **Reconciler:**
  - The worker job `reconcile_member_roles(branch_id)` makes the branch's member roles exactly match metadata: create, grant, revoke, drop.
  - It is enqueued on:
    - grant or override changes (every branch of the project);
    - member removal;
    - branch creation (a child inherits the parent's catalog, so the job rotates passwords and fixes memberships);
    - role create or delete through 6a;
    - (R17: no reconcile on Postgres database create or drop; member roles are cluster-wide.)
  - It is idempotent.
- **Lazy check:**
  - Before an act-as connection, the orchestrator compares `applied_roles` with the effective set.
  - If they differ, it runs the reconcile synchronously (a worker RPC, as `create_role` does).
  - If that fails, it returns 503 `MemberRoleReconcileFailed` with `retry_after`.
- **Revocation:** the API refuses as soon as metadata changes. The reconcile then revokes the memberships and runs `pg_terminate_backend` on that member role's sessions.
- **Removal:** `REASSIGN OWNED BY <member role> TO <owner>`, then `DROP OWNED BY` (privileges only, once ownership has moved), then `DROP ROLE`.
- **Hidden everywhere** (the `cb_m_` prefix is reserved):
  - `chelabase_types::role_name::is_reserved_for` (DDL receiver, role routes, worker delete). **This also fixes an existing gap: the `chelabase_` prefix is reserved too.** Today a user can `CREATE ROLE chelabase_x` in SQL.
  - `is_membership_reserved`.
  - `RESYNC_SQL` (`services/worker/src/resync.rs`).
  - `crates/chelabase-schema`: `exclusions.rs`, the `reserved` CTE and the ACL-grantee filter in `catalog_sql.sql`. 6g merges therefore skip member roles and any grants to them.
  - The pinned tests `catalog_sql_names_every_exclusion` and `exclusion_lists_match_worker_constants` are updated.
- **Connections:**
  - Act-as runs use one connection per run, as the SQL editor does today.
  - The console's generated reads and writes as a named role keep rest's `authenticator` + `SET ROLE` path. It runs no user SQL, so it's safe.

## 4. Access per route

"Act-as" means the role must be in the caller's effective set, and the owner needs Admin. Every route below exists today unless it is marked new.

| Area | Routes | Access |
|---|---|---|
| Console | query, schema | `ActAs(Viewer)`. Viewer: select only (writes are refused with 403 before reaching rest). |
| SQL editor | run, schema | `ActAs(Developer)`. A named role connects as the member role, then `SET ROLE r`, and the schema read follows that role's USAGE. The owner uses today's owner connection. The `grant_to_owner` workaround for `anon`/`authenticated` is removed. |
| | cancel | The run's starter, or Admin. |
| Saved queries | list, read (shared within the project, R24) | `Project(Developer)` |
| | edit, delete | The author, or Admin. |
| Connections | `GET /v1/databases/{id}/connection`, `POST …/reset-password` | `Project(Admin)` |
| | `GET …/branches/{b}/connection?role=` | `Project(Admin)` (R22: login-role credentials are identities, not group permissions). |
| | `POST …/branches/{b}/reset-password` | `Project(Admin)` |
| Roles (6a) | list | `Project(Viewer)`. Names aren't secret; each role is marked "you may use". |
| | create, update, delete, reset password, sessions | `Project(Admin)` |
| Data keys | list, `data-api/schema` | `Project(Viewer)` |
| | create, rotate, patch, delete | `Project(Developer)` for keys bound to an allowed role. Admin for any key. |
| Data API per database | enable, disable, patch | `Project(Admin)` |
| Branches | list, `…/databases` | `Project(Viewer)` |
| | create | `Project(Developer)`. The new branch is unprotected. |
| | reset, restore, delete, rename | `Branch(Developer, Admin)` |
| | protect, unprotect | `Project(Admin)` |
| Databases | get | `Project(Viewer)` |
| | create (now under a project), update, delete | `Project(Admin)` |
| Merges | list, get, `plan.sql` | `Project(Viewer)` |
| | create, rehearse, recompute, retry, cancel, abandon, apply, finish | `Branch(Developer, Admin)` by the target branch. A plan with security steps (BYPASSRLS etc.) needs Admin to acknowledge. |
| Projects | list | `TenantMember`, filtered to granted projects. Tenant admins see all. |
| | create | `TenantAdmin` |
| | get | `Project(Viewer)` |
| | update, delete | `Project(Admin)` |
| App users (6b) | app auth enable, disable, settings | `Project(Admin)` |
| | create, block, delete users | `Project(Developer)` |
| | list | `Project(Viewer)` |
| Members (new) | `GET /v1/members` | `TenantMember` (names and emails, for grant pickers). |
| | `PATCH /v1/members/{id}` (admin flag), `DELETE` | `TenantAdmin` |
| | `POST /v1/invites`, resend, revoke | `TenantAdmin` |
| | `GET/PUT/DELETE /v1/projects/{id}/grants[/{member}]`, overrides | `Project(Admin)` |
| | `POST /v1/projects/{id}/invites` | `Project(Admin)` |
| | `GET /v1/access-events` | `TenantAdmin` |
| | `GET /v1/projects/{id}/access-events` | `Project(Admin)` |
| | `GET/PATCH /v1/instance/settings` | `Instance` |
| | `/v1/me` | `SignedIn` |
| Auth service (new) | `POST /auth/v1/accept-invite` | Public, by code. |

Exact paths and `operationId`s are fixed in the plan. They follow `operation_ids_are_the_contract`.

## 5. Identity flows

- **Resolving the caller:**
  - The tenant comes from the org, as today. The member comes from (tenant, `sub`).
  - A person in the tenant's org without a member row gets one on their first request (`active`, no grants: "no access yet"). Being in the org already means sign-up, an invite or a Zitadel admin let them in.
  - The email and name are refreshed from the token claims.
- **First admin claim:** the member-creating insert sets `tenant_admin = NOT EXISTS (an active admin)` under a lock on the tenant row. It is atomic, so one person wins.
- **Bootstrap:**
  - `chelabase dev bootstrap --admin-email <e>` (default: the dev user) upserts that member as tenant admin.
  - `just zitadel-bootstrap` passes the flag, and the self-hosted setup uses the same command.
- **Sign-up gate:**
  - `/auth/v1/sign-up` asks the orchestrator's internal listener `may_sign_up(org)`. It is true when `open_signup` is on, or the tenant has no members. Otherwise the answer is `SignUpClosed`.
  - In platform mode, `ModePolicy` refuses with `NotAvailableInPlatformMode`.
- **Invites:**
  - The body is `{email, grants}`. A tenant invite may carry grants on several projects; a project invite carries one.
  - The orchestrator handles the email in this order:
    1. A member with that email just gets the grants.
    2. A Zitadel user in the org with that email gets a member row and the grants.
    3. Otherwise, using its existing admin-PAT Zitadel client, it:
       - creates a passwordless Zitadel user in the tenant's org;
       - requests an invite code with the URL template `{studio}/accept-invite?user={{.UserID}}&code={{.Code}}` (Zitadel sends the email; SMTP is Mailpit in dev);
       - stores an `invited` member, its grants (inert until the member is active) and an `invites` row.
  - **Resend:** a new code. **Revoke:** delete the never-activated Zitadel user, mark the member `removed` and delete its grants.
- **Accept:**
  1. Studio's `/accept-invite` page calls `POST /auth/v1/accept-invite {user, code, password}`.
  2. The auth service verifies the code with Zitadel, sets the password, marks the email verified and finishes sign-in.
  3. On the member's first authenticated request, the orchestrator flips the member to `active` and sets `accepted_at`. No cross-service write is needed.
  - An expired or revoked code gets `InviteExpired` or `InviteRevoked`.
- **Removal:**
  1. The member is marked `removed`, and its grants and overrides are deleted.
  2. The member-role reconcile runs on every branch of the member's projects.
  3. The Zitadel user is deactivated and its sessions deleted.
  - A still-valid JWT is refused at once, because `authorize` checks the status.

## 6. Studio

- **Process:** before any UI task, an Impeccable pass in Operate mode:
  - `context --target` on the new routes;
  - `shape` for these surfaces;
  - read `craft-floor.md`.

  The pass respects `PRODUCT.md`, `apps/studio/DESIGN.md`, the app shell and `packages/lagoon` (tokens only, dark only, `components/ui/*`). The layouts below are scope, not looks. The `shape` brief goes to the user before any UI task.
- **Rail:** "Members" replaces `placeholder-2`. It gets a new `RailLink` id and route, its own `isRailItemActive` case, and is shown to every tenant member. Tabs:
  - **People:** active, invited and "no access yet" members. Tenant admins can invite (email plus grants), resend, revoke, remove and toggle admin. Everyone else sees a read-only list.
  - **Activity:** `access_events`, for tenant admins.
  - **Instance:** the open sign-up switch, shown only with the `Instance` capability.
- **Project "Access" tab:**
  - A grants table: member, platform role and allowed Postgres roles. Roles are picked from the branch's 6a role list. Admin is labelled "includes the database owner"; there is no separate owner entry.
  - Overrides per Chelabase database, shown as narrowing chips.
  - Invite into this project, and the project's activity, both for project Admins.
- **"No access yet" screen:** shown when `/v1/me` has no projects and the caller isn't a tenant admin. It says an admin must grant access, and names the tenant admins.
- **`/accept-invite` page:** built on the existing auth page frame. It sets a password, then signs in.
- **Capability-driven UI:**
  - Actions the caller can't perform are hidden or disabled. Role pickers list only allowed roles, the SQL editor is hidden for Viewers, and owner actions are Admin-only.
  - A 403 still shows a clear message.
- **Missing components** (a role multi-select or combobox, perhaps an invite dialog form): each is proposed to the user with a question on whether it belongs in Studio or in lagoon.

## 7. Errors and testing

### New `ErrorCode`s

The new codes are listed below. Each gets a docs entry.

- `Forbidden` (with `extra.required`)
- `NoAccessYet`
- `NotAvailableInPlatformMode`
- `SignUpClosed`
- `LastTenantAdmin`
- `InviteExpired`
- `InviteRevoked`
- `MemberRoleReconcileFailed` (retryable)

### Invariant tests (from the start)

1. **Decision matrix:** an exhaustive test of `access::decide` over every `Access` variant × mode × caller kind (operator, tenant admin, Admin, Developer, Viewer, no grant, non-member) × branch protection × requested role (owner, allowed, other). It checks against a matrix written independently as data.
2. **Route walk:** every `ENDPOINTS` entry is called as a non-member, an ungranted member and a member one level too low, in both modes, through the fake-backed API (`tests/api.rs` style). Each call must be refused. A handler that skips `authorize` fails here.
3. **Grant algebra (property test, proptest at its latest version):**
   - the effective grant is never wider than the project grant or the override;
   - it equals the intersection of the levels present;
   - `owner` is never effective without Admin.
4. **`ModePolicy`:** every rule is tested per mode, and platform mode fails closed.
5. **Last admin (model test on the real metadata Postgres):** random concurrent sequences of add, demote, remove and claim. A tenant never ends up without an active admin, and at most one first claim wins.
6. **Reconciler convergence (live):** random grant, override, role and branch operations, followed by a reconcile. Afterwards the branch's `cb_m_*` roles and memberships equal the metadata model exactly, and a second run changes nothing.
7. **Escape attempts (live):** each of these fails, as a member role acting as `r`:
   - `RESET ROLE`;
   - `SET ROLE <owner>`;
   - `set_config('role', …)`;
   - a direct login as `cb_m_*` through the proxy;
   - `dblink` without a password.

   A revoked membership also ends the open session.
8. **Exclusions:** `cb_m_*` and `chelabase_*` are refused by `is_reserved_for`, and are skipped by resync, by `catalog_sql.sql` (roles and ACL grantees) and by the 6g merge.

### Other tests

- Worker unit tests for the reconciler's SQL.
- Auth service flows:
  - sign-up, both closed and open;
  - accept-invite with a valid, an expired and a revoked code.
- Metadata migration tests: the append-only trigger and the CHECKs.
- Studio: Vitest and Testing Library next to each component. They cover capability hiding, invite form validation, the grant editor (Admin labelled as including the owner, overrides that only narrow) and the no-access screen.

### Live suites (`just live`)

- New `members-e2e`, in this order:
  1. The bootstrap admin invites someone.
  2. The invite is accepted through Mailpit.
  3. The new member becomes a Developer with `billing`.
  4. The SQL editor touches only `billing`'s schema.
  5. The member is revoked, and the next request is refused.
- These run again under the new access check, as the bootstrap admin: `console-e2e`, `sql-editor-e2e`, `merge-e2e`, `role-e2e`, `branch-e2e`, `data-e2e`, `app-users-e2e`.

### Docs (`apps/docs`)

- Using: members and invites; roles and schema isolation (the `billing` example); self-hosted vs platform (generated from `ModePolicy`).
- Reference: access per route (generated from `ENDPOINTS`) and the new error codes.

## Execution notes

- No fork changes are expected, so there is no image build. The live pass runs on the pinned images. The dev stack is torn down, so it needs Zitadel's first-instance setup and `just zitadel-bootstrap` (with `--admin-email`) before the first live run.
- Migration: existing Zitadel users get no automatic access (M9). The dev stack's user becomes admin through the bootstrap.
