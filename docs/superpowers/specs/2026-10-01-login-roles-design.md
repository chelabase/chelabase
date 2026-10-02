# Step 6a: login roles — design (revision 2: roles and databases managed from SQL)

Status: **approved by the user 2026-10-02.** Revision 1 (the API-only design) was approved, built and verified on branch `login-roles`, then re-specced by the user's decision before merging; it stays in git history. Brainstorm record (every answer and approved design part): `2026-10-01-login-roles-v2-brainstorm.md`. Research: `2026-10-01-sql-roles-research.md`.

## Goal

Projects run their existing migrations and setup scripts unchanged, and they work through Cheladb's proxy: `CREATE ROLE … LOGIN PASSWORD`, `ALTER ROLE … PASSWORD`, `ALTER ROLE … RENAME`, `DROP ROLE`, `GRANT role TO role`, `CREATE DATABASE` and friends. What power users do in raw SQL applies firmly, as long as it doesn't violate the system. Studio and the API manage the same roles.

Why it fails today: every connection goes through Neon's proxy, which gets each role's SCRAM secret from our control plane (`neon_roles`), and the proxy passes SCRAM keys through to the compute, which checks them again, so the two copies must be byte-identical. A role made in SQL has no control-plane copy, and an owner's SQL password change makes the copies diverge (the owner is locked out).

## Decisions (user, 2026-10-01)

| # | Topic | Decision |
|---|---|---|
| 1 | Restore / branch from the past | **Split by origin.** SQL-made roles follow the database's history; Studio/API-made roles keep their current existence and password. |
| 2 | Password strength | Off in self-hosted mode (opt-in switch), on in platform mode. |
| 3 | Pre-hashed passwords | SCRAM hashes accepted in self-hosted mode only; platform refuses every pre-hashed password. MD5 refused in both. |
| 4 | Revealing SQL passwords | Not revealable: only the hash is kept. "Reset password" gives a revealable one. |
| 5 | Shared roles | One list managed from SQL and Studio alike; guards apply to both. Origin is fixed at creation. |
| 6 | Child branch passwords | Copied roles keep the parent's passwords (like Neon). **The owner `postgres` keeps step 5's rule: its own password per branch.** |
| 7 | Cap | 50 login roles per branch; group roles without a password don't count. |
| 8 | Control plane unreachable | The DDL's transaction fails ("try again"). |
| 9 | Privileges given in SQL | Stick. The data API refuses a secret key whose role has become owner-level. |
| 10 | Databases made in SQL | Tracked and shown (Studio list, connection picker). |
| 11 | Protected branch | New flag: children get fresh passwords for every copied login role; delete and reset-to-parent refused; restore allowed. Deleting a Cheladb database with a protected branch is refused too. |
| — | Database limit | Per-branch limit on Postgres databases (a setting; plans set it later) plus the existing branch cap. Note: a per-project cap may follow if needed. |
| — | Approach | **C:** Neon's DDL forwarding into the control plane, plus a resync from the database's catalog. No fork task. |
| — | Data API across databases and schemas | Its own step **6c**, right after 6a and before 6b, including schema support and database/schema switching in the UI. 6a only tracks and shows databases. |
| — | Docs | The product documentation site must clearly highlight the branch password rule (row 6 and the protected flag) so nobody is blindsided. |

Names: a **Cheladb database** is the thing with branches (`/v1/databases/{id}`); a **Postgres database** is what `CREATE DATABASE` makes inside a branch.

## Part 1: where roles live

- **The branch's database (`pg_authid`) is the source of truth for every login role except the owner.** Roles are ordinary Postgres roles managed by `postgres`, which holds ADMIN on all of them (PG16+ gives the creator ADMIN; Studio-made roles are created as `postgres`).
- **The compute spec lists only the owner**, as before 6a. `compute_ctl` touches only spec roles (`spec_apply.rs:786-828`), so it never re-creates, re-passwords or re-LOGINs user roles, and SQL changes stick. The fork's `privileged` field is no longer used by Cheladb.
- **`neon_roles` is the proxy's copy** of each login role's SCRAM hash, plus bookkeeping:
  - `origin` `sql | api` (the owner row: `owner`), fixed at creation;
  - `password_enc` (sealed, `Purpose::Role`, AAD `"{branch_id}:role:{name}"`) only when we generated the password;
  - `login` and `level` (`plain | owner_level`) as last observed in the database;
  - `pending` and the endpoint's `roles_version` guard (kept from revision 1).
- **Studio-made roles** are created by the worker through `admin_sql` (forwarding off) as `SET ROLE postgres; CREATE ROLE "<r>" LOGIN PASSWORD '<scram>'`; an owner-level one is created with CREATEROLE, CREATEDB, BYPASSRLS and REPLICATION and granted `neon_superuser` (as `cloud_admin`).
- **`branch_databases`** (per branch: `name`, `owner`, `origin` `sql | main`) mirrors the branch's Postgres databases.
- **`branches.protected boolean NOT NULL DEFAULT false`.**

## Part 2: the DDL forwarding receiver

- **Wiring.** Every compute's spec sets `neon.console_url = http://<internal listener>/compute/api/v2/computes/{endpoint_id}/ddl` (a postmaster setting, so computes restart once at rollout). The fork's handler (`pgxn/neon/neon_ddl_handler.c`) sends one `PATCH` per transaction at pre-commit with `Authorization: Bearer $NEON_CONTROL_PLANE_TOKEN`, which Cheladb already sets to the compute's token; the route checks it against `neon_endpoints.compute_token_sha256` like the spec fetch. Every worker `admin_sql` session runs `SET neon.forward_ddl = off` first.
- **Handling.** One metadata transaction under the endpoint row lock; role renames first, then sets, then deletes (Neon's order), then databases. Idempotent: a timed-out request is retried by the compute, and `DROP ROLE IF EXISTS` of a missing role still arrives. **200** accepts; any refusal is a **4xx with a one-line plain-text body**, which fails the user's transaction with that body as its message (fork F2; an empty body reads `role and database changes were refused (HTTP <code>)`).
- **Role `set` (create, password change):**
  - reserved names (`postgres` excepted below, `cloud_admin`, `neon_superuser`, `authenticator`, `anon`, `authenticated`, `cheladb_admin`, `pg_*`) → refused;
  - **`postgres`** re-passworded in SQL → accepted; its copy is updated and its sealed password cleared (reveal then says the password was set in SQL);
  - `encrypted_password` must be SCRAM; MD5 → refused ("MD5 passwords can't be used through Cheladb; set a plain password");
  - a pre-hashed `password` (the literal is a SCRAM verifier) → accepted in self-hosted mode, refused in platform mode;
  - strength (platform mode, or self-hosted with `CHELADB_PASSWORD_STRENGTH=on`): at least 12 characters and an estimated 60 bits of entropy (character classes × length); the error names the rule;
  - no password (a group role) → no copy; `PASSWORD NULL` removes an existing copy;
  - a new copy past the cap (`CHELADB_BRANCH_ROLE_CAP`, default 50, 0..=500) → refused, naming the setting;
  - a new role gets `origin = sql`; an existing row keeps its origin.
- **Role `del`:** refused while a live (non-revoked) secret data API key uses the role, naming the key(s); otherwise the copy is removed, whatever its origin.
- **Role rename:** the copy moves (a SCRAM hash doesn't depend on the name); an API role's sealed password is re-sealed under the new name; data API keys using the role follow the rename.
- **Databases:** `set`/rename/owner/`del` are mirrored into `branch_databases`; a new database past `CHELADB_BRANCH_DATABASE_CAP` (default 20) is refused; dropping or renaming the branch's main database is refused (the compute refuses a non-superuser drop before it runs: every spec sets `neon.protected_databases` to the main database, fork F1).
- **Settings:** the orchestrator (receiver) and the worker (API creates) both read `CHELADB_BRANCH_ROLE_CAP`, `CHELADB_BRANCH_DATABASE_CAP` and `CHELADB_PASSWORD_STRENGTH`; platform mode forces strength on and refuses pre-hashed passwords whatever the setting. The proxy's `--project-info-cache … max_roles` is raised from 10 to cover the cap (owner and system roles included), so up to 50 login roles don't churn its cache.
- **Proxy cache:** each changed role gets `password_updated` at once and again about 1 s later (the receiver answers before the user's commit; a login in between can only be refused, never wrongly accepted, since the compute re-checks).
- **Control plane down:** the compute's request fails, so the user's transaction fails (decision 8) with `role and database changes are unavailable right now; try again` (SQLSTATE 55000, fork F2).
- **Known gap (Neon's own):** a transaction can abort after it forwarded (rare); the resync repairs the copy.

## Part 3: the resync

- **When:** under the endpoint lock after a restore, reset-to-parent, branch create (on the child, before it's shown ready), any compute re-creation, and periodically in reconcile (the data API health-check batch).
- **Read:** one `admin_sql` query (forwarding off; names hex-encoded) returning, per non-system role, name, `rolcanlogin`, `rolpassword`, and owner-level (superuser, `CREATEROLE`/`CREATEDB`/`BYPASSRLS`/`REPLICATION`, or `neon_superuser` membership at any depth); plus databases and owners.
- **SQL-made roles — the database wins:** a role with a SCRAM password and no copy is added (`origin = sql`); a copy whose role is gone is removed; a different hash replaces the copy's; `login` and `level` are refreshed.
- **Studio-made roles — the control plane wins for existence and password:** a missing role is re-created as `postgres` with the stored hash (if created owner-level: created with CREATEROLE, CREATEDB, BYPASSRLS and REPLICATION and granted `neon_superuser`; an existing one missing any of them gets them back); a differing hash is set back; `login` and `level` are refreshed; grants and ownership follow the data.
- MD5-hashed and password-less roles get no copy (Studio flags MD5). Nothing is removed for exceeding the cap. `branch_databases` is set to the database's list.
- Changed roles get `password_updated`.
- **Failure:** fails restore, reset-to-parent, branch create and re-creation with `syncing login roles: …` (the branch shows it; retryable); in reconcile it's logged and retried.

## Part 4: branches, restore, protected

- **Branch create (now or from a past point):** the child's catalog is a copy, so every role and password is the parent's (decision 6); the resync fills the child's copies before ready. API roles keep `origin = api`, sealed passwords re-sealed for the child. From a past point: that time's SQL roles plus the parent's current API roles (re-created if needed).
- **The owner `postgres` gets its own password on every branch** (step 5's rule, unchanged).
- **Protected:** `PATCH /v1/databases/{id}/branches/{branch_id}` with `{"protected": bool}`; children of a protected branch get a fresh generated (revealable) password for every copied login role, SQL-made ones included (they become revealable); delete and reset-to-parent of a protected branch → 409 `branch_protected`; deleting a Cheladb database with any protected branch → 409 `branch_protected`; restore allowed.

## Part 5: data API safety and system roles

- Key creation keeps refusing an owner-level role (`check_data_role`).
- **`rest` checks before switching to a secret key's role** whether it is owner-level (superuser, `BYPASSRLS`, or `neon_superuser` member at any depth), cached 30 s per branch and role; if so: data API error `role_not_allowed` (403): "the role `<name>` of this key can bypass row-level security; make it plain or use another role". Studio shows the key as **Blocked** from the resync's observed `level`.
- `REVOKE <role> FROM authenticator` in SQL applies; the key then fails with the existing permission error.
- System roles are protected twice: Postgres refuses the owner (no ADMIN on roles `cloud_admin` created) and the receiver refuses them. Table grants to `anon`/`authenticated` work as today.

## Part 6: API and Studio

- `GET …/branches/{branch_id}/roles` → owner first, then every login role: `name`, `origin`, `level`, `login`, `password` (`generated | set_in_sql | md5_unusable`), `blocked_keys`, `pending`, `created_at`.
- `POST …/roles` `{ name, owner_level?: false }` → 201 role row (no password). `POST …/roles/{role}/reset-password` → 204 on any login role (a new generated, revealable password). `DELETE …/roles/{role}` → 204 on any login role but the owner (Part 2's guards).
- `GET …/connection?role=&database=` → the password when revealable; otherwise no password and `password: "set_in_sql"`.
- `GET …/branches/{branch_id}/databases` → `[{ name, owner, origin, main }]`. No database create/delete through the API in 6a.
- `PATCH …/branches/{branch_id}` `{ protected }`; branch responses carry `protected`; new `ErrorCode` `branch_protected` (409). Revision 1's role codes stay.
- utoipa annotations and the regenerated client, as revision 1.
- **Studio:** Roles panel with origin and level badges, "Password set in SQL — reset to get a revealable one", an MD5 warning, blocked-key notes; reset on every role, delete on every role but the owner. Connection panel: role and database pickers. Branch actions: a Protect toggle with a confirm explaining it; Delete and Reset-to-parent disabled with the reason when protected; the Cheladb database's delete disabled while a branch is protected. Data API keys: **Blocked** with the reason.

## Part 7: rollout, tests, docs

- **Migration on top of revision 1's** (already applied on dev machines): `neon_roles` gains `origin`, `login`, `level` and loses revision 1's sealed/privileged constraints; `branch_databases`; `branches.protected`.
- **Rollout:** reconcile re-creates every compute once (`neon.console_url`). Roles made in SQL before this change become able to log in after the first resync (README note).
- **Tests:** unit (the receiver's rules table-driven, the resync per origin, protected refusals, `rest`'s check, routes, Studio vitest); live (a real migration script through the proxy: create, log in, re-password, rename, grant, create database and connect, drop; restore to before it and re-run it; child and protected-child passwords, owner per branch; MD5 refused; a key's role drop refused; control plane stopped → the DDL fails; forwarding off → the resync picks the role up; `GRANT neon_superuser` → `role_not_allowed`); `just role-e2e` rewritten around a `psql` migration file; `neon-e2e`, `data-e2e`, `branch-e2e`, `rest_live`; a Playwright pass.
- **Docs:** README "Roles and migrations" (what works in SQL, password rules per mode, caps, protected branches, MD5, the branch password rule); roadmap: 6c, the per-project cap note, and a note that the docs site must highlight the branch password rule.

## Kept from revision 1

Name rules and reserved names; sealed passwords for generated ones; routes, RPCs and 201/204 replies; the version guard and `pending`; delete across every connectable database (quoted conninfo, checked before `NOLOGIN`); the error codes; the Studio Roles panel and role picker; the live-test setup. **Dropped:** spec-driven login roles and `privileged`, privilege fixed at create, the demotion check, fresh passwords per child.
