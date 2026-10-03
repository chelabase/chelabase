# Step 6b: projects and app users (design)

Status: brainstormed with the user on 2026-10-03. Part 1 (projects) was approved; the user then said "just build it", so parts 2–6 were decided by Claude. Each such decision is marked **(R)** and listed under "Rulings" with its cost if wrong.

Roadmap context: `docs/roadmap/2026-09-28-neon-and-data-api.md`, "App users: Zitadel identity, an app-owned users table". Zitadel facts below were verified live on v4.19.1 (2026-10-03).

## User decisions (question rounds)

1. One step, one spec: projects become a real resource, and app users are built on them.
2. Projects get full lifecycle: create, list, rename, and **cascading delete** (databases, branches, computes, S3 data and the Zitadel org).
3. App users is a per-project switch, off by default. Enabling it creates the project's Zitadel org and app. Disabling it is refused while users exist.
4. The users table lives in **one chosen auth database**: a Cheladb database, a Postgres database and a schema (default `public.users`). It exists on every branch of that database. The project's other databases accept the same user tokens but have no users table.
5. Branches share one user pool. A sign-up inserts on the branch the key names, and sync copies the row to the other branches.
6. **Cheladb issues the tokens.** `auth` checks the password through a Zitadel session, then issues a short-lived EdDSA access JWT plus a rotating refresh token bound to that Zitadel session.
7. A request carries `Authorization: Bearer <public key>` plus `Cheladb-User: <access JWT>`.
8. Flows: signUp, signIn, signOut, refresh, getUser, verifyEmail/resendCode (verification is an opt-in per project, off by default in self-hosted mode), password reset, change email and change password.
9. Sync runs three ways: write-through from our flows, Zitadel Actions v2 webhooks, and a periodic reconcile.
10. Studio: project settings plus full user admin (list, search, create, edit email, set password, sessions, sign out everywhere, deactivate/reactivate, delete).
11. The SDK keeps the refresh token in a pluggable storage (localStorage by default in browsers), with rotation and reuse detection.
12. `auth` owns the db.auth flows, including the sign-up transaction on the compute.

Out of scope: social/OIDC identity providers, MFA, bring-your-own IdP, custom roles beyond `authenticated`, moving app users to another auth database (disable then enable instead), per-project email templates.

## Part 1: Projects (approved)

**Data model.** A migration adds these columns to `projects`:
- `name TEXT NOT NULL`: display name, renamable. Backfilled from `slug`.
- `status TEXT NOT NULL DEFAULT 'active'`: `active` or `deleting`.
- `error TEXT`.
- `updated_at TIMESTAMPTZ NOT NULL DEFAULT now()`.

`slug` stays unique per tenant and is fixed at create. It is derived from the name: lowercase, `[a-z0-9-]`, 1–40 characters, with `-2`, `-3`… appended on collision. Renaming changes `name` only.

**API** (orchestrator, caller's tenant as today, utoipa/OpenAPI):
- `GET /v1/projects`.
- `POST /v1/projects {name}`: 201.
- `GET /v1/projects/{id}`.
- `PATCH /v1/projects/{id} {name}`.
- `DELETE /v1/projects/{id} {confirm}`: 202. `confirm` must equal the slug, otherwise 422.
- Another tenant's project returns 404.
- `name` is 1–64 characters after trim, otherwise 422.
- A `deleting` project refuses every change with 409 `project_deleting`. `POST /v1/databases` refuses it too.
- `/v1/me` keeps listing projects, now with `name` and `status`.

**Cascading delete:**
1. One transaction sets the project to `deleting`, sets each live database to `deleting`, and publishes the existing database-delete job for each.
2. A new worker job `project_delete {project_id}` (new NATS subject) waits until none of the project's databases is left. It retries with backoff, and records a database's delete error on `projects.error`.
3. It then revokes every app session of the project, deletes the Zitadel org if one exists (admin PAT, `DELETE /v2/organizations/{id}`; a 404 counts as done) and deletes the `project_auth` row and the project row.
4. Failures leave the project `deleting` with `error` set, and the job retries.
5. Deleting the last project is allowed.

**Studio:**
- The dashboard lists projects and has a "New project" dialog.
- The project page (`/app/projects/$projectId`) has two tabs:
  - **Databases:** the existing database list and create form move here.
  - **Settings:** rename, App users (part 2) and a danger zone. The delete dialog requires typing the slug and lists the databases that will be destroyed.
- A project that is `deleting` shows a banner and disables all actions.

## Part 2: Enabling app users, and the database machinery

**`project_auth` table** (metadata, one row per project with app users):
- `project_id UUID PK REFERENCES projects`.
- `status`: `enabling`, `enabled`, `disabling` or `failed`.
- `error TEXT`.
- `zitadel_org_id TEXT UNIQUE`.
- `database_id UUID REFERENCES databases`: the auth database.
- `pg_database TEXT`: default is the branch's main Postgres database.
- `schema_name TEXT DEFAULT 'public'` and `table_name TEXT DEFAULT 'users'`, both validated as identifiers.
- `email_verification BOOL NOT NULL DEFAULT false`. **(R)** The default is false in self-hosted mode; platform mode decides later.
- `on_delete TEXT NOT NULL DEFAULT 'delete'`: `delete` or `mark`.
- `created_at` and `updated_at`.

**API:**
- `GET /v1/projects/{id}/app-users`: the settings, or 404 `app_users_disabled`.
- `PUT /v1/projects/{id}/app-users {database_id, pg_database?, schema?, table?, email_verification?, on_delete?}`:
  - **Enable** when there is no row (202).
  - Otherwise **update** `email_verification` and `on_delete` only. Changing the location is refused with 409 `app_users_location_fixed`.
- `DELETE /v1/projects/{id}/app-users`: **disable**.
  - Refused with 409 `app_users_has_users` while the org has any user.
  - Otherwise 202: the worker removes the guard machinery (the table stays as an ordinary table), deletes the org and deletes the row.

**Enable preconditions** (409 with a code when not met):
- The database belongs to the project and is `ready`.
- The data API is enabled on that Postgres database on **every** live branch (`data_api_not_enabled`). The guard relies on its roles.
- The target table doesn't exist on any branch (`app_users_table_exists`).

**Worker job `app_users_enable {project_id}`:**
1. Creates the org with `POST /v2/organizations`, using `organizationId = project uuid` and name `cheladb-app-<project uuid>`. The call is idempotent: if the org already exists, it reads it.
2. On every live branch of the auth database, runs the **users bootstrap** with `admin_sql` in the chosen Postgres database.
3. On every data-API-enabled Postgres database of every branch of every database in the project, re-runs the **claim helpers** install (idempotent).
4. Sets `enabled`. On error it sets `failed` with `error`. Re-running `PUT` with the same body retries; `DELETE` cleans up.

**Users bootstrap SQL** (pinned in the plan; all of it idempotent):
- **Login role `cheladb_auth_sync`:**
  - `LOGIN NOINHERIT BYPASSRLS`; not a superuser.
  - Its per-branch password is generated by the worker, sealed in metadata (`branch_auth_sync` rows: `branch_id`, `pg_database`, `password_sealed`) and set with `ALTER ROLE … PASSWORD`.
  - Copied branches get a fresh password. `branch_create` runs the users bootstrap on the new branch when the project has app users.
  - **(R)** The role is per branch, not in the compute spec, like `authenticator`.
- **Table** `<schema>.<table>`:
  - `id uuid PRIMARY KEY`.
  - `email text NOT NULL`, with a unique index on `lower(email)`.
  - `email_verified boolean NOT NULL DEFAULT false`.
  - `created_at timestamptz NOT NULL DEFAULT now()`.
  - `deleted_at timestamptz`: managed, used by `on_delete = mark`.
  - Owned by the database owner (`postgres`), so developers alter it with normal migrations. `cheladb_auth_sync` gets `SELECT, INSERT, UPDATE, DELETE`.
- **Guard trigger** `cheladb_users_guard` (BEFORE INSERT OR UPDATE OR DELETE, FOR EACH ROW; function `cheladb.users_guard()`, `SECURITY INVOKER`). Unless `current_user = 'cheladb_auth_sync'`, it:
  - refuses INSERT and DELETE, with `42501` and the message "rows of <table> are created and deleted by Cheladb app users";
  - refuses an UPDATE that changes any managed column, with `42501` and the message "column <c> is managed by Cheladb app users".
  - **(R)** It uses a trigger, not column privileges. Postgres can't revoke a column from a table-level grant, and the trigger also covers the owner.
- **Event trigger** `cheladb_users_protect` (on `sql_drop` and `ddl_command_end`) refuses:
  - dropping or renaming the table, a managed column or the guard trigger;
  - changing a managed column's type;
  - `ALTER TABLE … DISABLE TRIGGER` on the guard.

  It reads `cheladb.app_users_table` (a one-row table with schema and table name) to know which table to protect. **(R)** A superuser can still bypass it; that is the owner's responsibility.
- **Grants** come from the data API defaults: `authenticated` has DML and `anon` SELECT. The guard trigger limits what DML can do. RLS stays off by default, and Studio warns about it on this table.

**Claim helpers** (in schema `cheladb`, `STABLE`, `EXECUTE` granted to `anon` and `authenticated`; also added to the data API bootstrap so new enables get them):
- `cheladb.claims() returns jsonb`: `current_setting('request.jwt.claims', true)::jsonb`, or `'{}'`.
- `cheladb.user_id() returns uuid`: `(claims ->> 'sub')::uuid`, or NULL when absent.
- `cheladb.user_email() returns text`.

## Part 3: Tokens and the request path

**Signing key:**
- An Ed25519 key pair for app tokens, at `.cheladb/auth/app-jwt.{key,pub}`. `cheladb dev bootstrap` generates it when missing and never overwrites it.
- `auth` signs; the gateway verifies with the public key. Each token's `kid` is the first 16 hex characters of the SHA-256 of the public key.
- Public JWKS: `GET /auth/v1/app/jwks`, so apps' own servers can verify tokens.

**Access JWT** (`EdDSA`, TTL `CHELADB_APP_ACCESS_TTL_SECS`, default 900):
- `iss = "cheladb-auth"`.
- `aud = "cheladb-project:<project_id>"`.
- `sub` (user id), `sid` (app session id) and `project`.
- `email` and `email_verified`.
- `role = "authenticated"`.
- `iat` and `exp`.

**App sessions** (metadata table `app_sessions`):
- `id uuid`, `project_id` and `user_id`.
- `zitadel_session_id`.
- `refresh_hash` (SHA-256), `prev_refresh_hash`.
- `created_at`, `last_used_at`, `expires_at` and `revoked_at`.

Refresh tokens:
- Format: `<session id>.<43 characters, 32 random bytes base64url>`.
- Sliding expiry `CHELADB_APP_REFRESH_TTL_SECS` (default 30 days), with an absolute cap of 90 days from `created_at`.
- **Rotation:** every refresh issues a new token, moves the old hash to `prev_refresh_hash` and returns a fresh access JWT.
- **Reuse detection:** presenting `prev_refresh_hash` (or any stale token for a live session) revokes the session and deletes its Zitadel session. The response is 401 `refresh_token_reused`.
- A concurrent-refresh grace of 10 s: within 10 s of a rotation, the previous token returns the **same** new pair. That needs the new token stored sealed for 10 s (`grace_refresh_sealed`, `grace_until`). **(R)** The grace exists so two tabs refreshing at once don't sign the user out.

**Internal endpoints** (orchestrator internal listener, new `app-auth` bearer, used by `auth` and the gateway; `auth` stays without a metadata connection, like rest):
- `GET /internal/app/branches/{branch_id}`: the project, its app-user settings, the auth database, whether this branch belongs to it (else the auth database's main branch id), and the sync connection (host, port, database, `cheladb_auth_sync` password) for the target branch.
- `POST /internal/app/sessions`: create a session.
- `POST /internal/app/sessions/refresh`: rotate a refresh token.
- `POST /internal/app/sessions/{id}/revoke`.
- `POST /internal/app/users/{user_id}/revoke-sessions` (optionally keeping one session).
- `GET /internal/app/sessions/{id}`: liveness, used by the gateway.
- `POST /internal/app/sync {project_id, user_id, data?, origin_branch_id?}`: publishes the sync job.

**Gateway:**
- On `/data/v1/*` and `/auth/v1/app/*`, the data-key step resolves the key. The internal key response now also returns `project_id`.
- With a **public** key and a `Cheladb-User` header:
  1. verify the JWT (signature, `exp` with 30 s leeway, `iss`, and `aud` equal to `cheladb-project:<key's project>`);
  2. check the session is live through `GET /internal/app/sessions/{sid}`, cached for 30 s (revocation therefore takes effect within 30 s);
  3. mint the internal identity with `role = "authenticated"` and `claims = {sub, sid, project, email, email_verified, role, exp}`.
- An invalid token returns 401 `user_token_invalid`, an expired one 401 `user_token_expired`, and a revoked session 401 `user_session_revoked`. The request **never falls back to `anon`**.
- A **secret** key with `Cheladb-User` returns 400 `user_token_with_secret_key`.
- Rest needs no change: `PREAMBLE_SQL` already sets `role` and `request.jwt.claims`.
- On `/auth/v1/app/*`, the identity is forwarded to `auth` (branch, project). These routes accept public keys only.

## Part 4: db.auth flows in `auth`

All the flows live under `/auth/v1/app/` (gateway scope `Scope::Api`). They are JSON, padded like the platform flows (`CHELADB_AUTH_MIN_RESPONSE_MS`), and rate-limited per project and IP, and per project and email. They need app users `enabled` (otherwise 409 `app_users_disabled`). Zitadel calls use the `login-client` PAT, which works in any org.

Password rules are the platform's. Every app user is created with **`username = userId`**, because Zitadel usernames are unique across the whole instance. Lookups by email go through `POST /v2/users` with an org filter, and sessions use the `userId` check, never `loginName`.

- **`POST sign-up {email, password, data?}`:**
  1. Validate the input.
  2. Look up the email in the org. If it is taken, send the decoy response (identical shape and timing) and send the owner a "someone tried to sign up" notice through a password-reset email, like the platform flow.
  3. Choose the **sign-up branch**: the key's branch if it belongs to the auth database, otherwise the auth database's main branch.
  4. Generate the uuid.
  5. Connect as `cheladb_auth_sync`, `BEGIN`, and `INSERT` the managed columns plus `data`. Each `data` key must be a non-managed column, checked against `information_schema` in the same transaction; otherwise 422 `unknown_column`. Values are bound as JSON through `json_populate_record`, so the app's constraints and types validate them. A constraint error returns 422 `constraint_violation` with the Postgres message and the constraint name.
  6. Create the Zitadel user (`userId`, `username = userId`, the org, email, password, `isVerified = !email_verification`; with verification on, `sendCode`).
  7. `COMMIT`. If the commit fails, delete the Zitadel user.
  8. Publish the sync (with `data` and the origin branch) so the other branches get the row.
  9. With verification off, sign in at once (as in `sign-in` below). With it on, return `{user, verification_required: true}` and no tokens.
- **`POST sign-in {email, password}`:**
  1. Look up the user in the org.
  2. Create a Zitadel session with the `userId` and password checks.
  3. Refuse with 403 `email_not_verified` when verification is on and the email isn't verified.
  4. Create the app session.
  5. Return `{access_token, token_type: "bearer", expires_in, refresh_token, user}`.

  An unknown email and a wrong password both return 401 `invalid_credentials`, padded. A locked or deactivated user returns 403 `user_inactive`.
- **`POST refresh {refresh_token}`:** returns the same shape as sign-in. On refresh, `email` and `email_verified` are re-read from Zitadel, so claims stay current.
- **`POST sign-out`:** uses the `Cheladb-User` header (the session from `sid`) or a `{refresh_token}` body. It revokes the app session and deletes the Zitadel session. 204.
- **`GET user`:** uses `Cheladb-User`. Returns `{id, email, email_verified, created_at}` from Zitadel.
- **`POST verify-email {email, code}`** marks the email verified, writes it through the sync, and returns 204.
- **`POST resend-code {email}`** always returns 204, padded.
- **`POST password-reset {email}`:** Zitadel sends the code through `sendCode`. It always returns 204, padded. **(R)** It sends a code, not a link, since there are no per-project templates in 6b.
- **`POST password-reset-confirm {email, code, password}`:** sets the password with the verification code and revokes all of the user's sessions. 204.
- **`POST change-email {email}`** (needs `Cheladb-User`):
  - with verification on, Zitadel sends a code and the email shows as unverified until `verify-email`;
  - with it off, the email is set verified.

  The change is written through the sync. 204.
- **`POST change-password {current_password, password}`** (needs `Cheladb-User`) sets the password with the current one and revokes the user's other sessions. 204.

## Part 5: Sync

**Worker job `app_users_sync {project_id, user_id, data?, origin_branch_id?}`:**
- Reads the user from Zitadel (the source of truth) and applies its state to every live branch of the auth database, connecting as `cheladb_auth_sync`:
  - **exists:** upsert the managed columns (`INSERT … ON CONFLICT (id) DO UPDATE` for email and email_verified only; `created_at` from Zitadel's creation date). When the row is new and `data` is present, the `data` columns that exist on that branch are inserted too. If the insert fails because of an app constraint, it is retried with the managed columns only. If that fails too (for example a NOT NULL app column without a default), the error is logged with the branch, and the reconcile retries later.
  - **gone (404):** `on_delete = delete` deletes the row (app foreign keys cascade as the app defined them); `mark` sets `deleted_at = now()`.
  - **deactivated or locked:** revoke the user's app sessions. Nothing changes in the table.
- The job is idempotent and state-based, so duplicate or out-of-order deliveries are harmless.
- The origin branch is skipped for the insert, but still gets the upsert.

**Webhooks (Zitadel Actions v2):**
- `cheladb dev bootstrap` creates one instance-wide `restAsync` target pointing at the orchestrator's internal listener (`POST /internal/zitadel/events`), plus an execution on the `user` event group.
- The signing key is written to `.cheladb/zitadel/actions.env` (`CHELADB_ZITADEL_ACTIONS_SIGNING_KEY`), and the orchestrator loads it.
- Compose overrides `ZITADEL_HTTPCLIENT_DENYLIST` so Zitadel may call the host's internal listener. That applies in self-hosted mode only.
- The handler verifies `ZITADEL-Signature: t=…,v1=…`: HMAC-SHA256 of `"<t>.<body>"`, 300 s tolerance, any `v1` matching, constant-time compare.
- It then maps `resourceOwner` to a project through `project_auth.zitadel_org_id`, ignores other orgs, and for the event types listed in the roadmap (plus `user.human.password.changed`, which revokes sessions) publishes `app_users_sync`.
- It answers 200 quickly. A bad signature returns 401.

**Reconcile:**
- Every `CHELADB_APP_USERS_RECONCILE_SECS` (default 300) per enabled project, and right after a branch of the auth database is created, the worker:
  1. pages through the org's users in Zitadel;
  2. reads `id, email, email_verified, deleted_at` from each branch's table;
  3. publishes `app_users_sync` for every difference: missing rows, stale columns, and rows whose user is gone.
- Rows not in Zitadel are handled per `on_delete`.

## Part 6: SDK, platform user admin, Studio

**SDK (`@cheladb/client`):**
- `createClient({url, key, auth?: {storage?, autoRefresh?: true}})`.
- `db.auth` methods: `signUp`, `signIn`, `signOut`, `refresh`, `getUser`, `verifyEmail`, `resendCode`, `requestPasswordReset`, `confirmPasswordReset`, `changeEmail`, `changePassword`, and `onChange(cb) → unsubscribe` (events: `signed_in`, `signed_out`, `refreshed`).
- **Storage:** a `{get, set, remove}` adapter, sync or async. The default is `localStorage` when present, otherwise memory. Only the refresh token is stored (key `cheladb.<project-key-prefix>.refresh`); the access token stays in memory.
- **Every data request** gets `Cheladb-User` when signed in.
- **Refresh:**
  - automatic, 60 s before `exp`, single-flight (one in-flight refresh shared);
  - on `401 user_token_expired`: one refresh and one retry;
  - on `refresh_token_reused` or `user_session_revoked`: sign out locally and emit `signed_out`.
- `cheladb gen types` marks the auth table. `signUp`'s `data` is typed as the table's insertable non-managed columns.

**Platform API** (orchestrator; calls Zitadel with the admin PAT through the new shared crate):
- `GET /v1/projects/{id}/users?search=&page_token=` (email search, 50 per page). Each user: id, email, email_verified, state (`active`, `inactive` or `locked`), created_at.
- `POST /v1/projects/{id}/users {email, password, email_verified?}`: inserts on the auth database's main branch first, then the sync fans out.
- `GET /v1/projects/{id}/users/{user_id}`.
- `PATCH /v1/projects/{id}/users/{user_id} {email?, email_verified?, password?}`. A changed password revokes sessions.
- `POST …/{user_id}/deactivate` and `POST …/{user_id}/reactivate`. Deactivate revokes sessions.
- `GET …/{user_id}/sessions` (app sessions) and `DELETE …/{user_id}/sessions` (sign out everywhere).
- `DELETE …/{user_id}`: deletes the Zitadel user, revokes sessions and syncs the row per `on_delete`.

**Shared crate `crates/cheladb-zitadel`:** the v2 REST client: users, sessions, orgs, codes, Actions verification and event types. It moves out of `services/auth/src/zitadel.rs`, which then uses the crate. `auth`, the orchestrator and the worker use it. `tools/cli` keeps its bootstrap steps but may reuse it.

**Studio:**
- The project Settings tab gets an **App users** card: an enable form (database, Postgres database, schema and table, verification switch, on-delete choice), status with error, the settings when enabled, and disable (refused with a message while users exist).
- A new **Users** tab on the project page (visible when app users are enabled):
  - a searchable, paginated table;
  - a "Create user" dialog;
  - a user drawer: email (edit), verified (toggle), set password, deactivate/reactivate, a session list with "Sign out everywhere", and delete with confirmation.
- The data-API RLS warning also covers the users table.

## Errors

New codes in the shared error shape: `project_deleting`, `app_users_disabled`, `app_users_location_fixed`, `app_users_has_users`, `app_users_table_exists`, `data_api_not_enabled`, `unknown_column`, `constraint_violation`, `invalid_credentials`, `email_not_verified`, `user_inactive`, `refresh_token_invalid`, `refresh_token_reused`, `user_token_invalid`, `user_token_expired`, `user_session_revoked`, `user_token_with_secret_key`. Invalid input returns 422, as everywhere else.

## Testing

- **Unit:**
  - slug generation;
  - JWT sign/verify (aud mismatch, expiry, kid);
  - refresh rotation, the grace window and reuse;
  - signature verification against Zitadel's algorithm (a fixed vector built from the source's format);
  - the event-to-project mapping;
  - the sync state machine with fakes;
  - the gateway user-token step (header combinations).
- **Mock Zitadel:** `auth` integration tests for every flow, including decoy sign-up, commit-failure rollback (the Zitadel user is deleted) and a constraint failure (Zitadel is never called).
- **Live** (through `just live`):
  - users bootstrap and guard/event-trigger behaviour on a real compute (owner refused, sync role allowed, drop refused);
  - helpers under the data API;
  - an `app_users_live` worker test for enable, sync fan-out to a second branch and disable.
- **E2E `just app-users-e2e`:**
  1. create a project and a database, enable the data API and app users;
  2. SDK sign-up with `data`;
  3. an RLS policy `USING (id = cheladb.user_id())`, where a user reads only their own row;
  4. the row appears on a second branch;
  5. a console change in Zitadel (via API) arrives through the webhook;
  6. refresh rotation and reuse revoking the session;
  7. delete the user (the row goes away);
  8. delete the project cascades (the databases are gone, and the org is gone).
- **Studio:** vitest for new components, then a Playwright pass over projects, app-user settings and user admin.

## Rulings (decided by Claude after "just build it"; cost if wrong)

- R1: a trigger guards the managed columns instead of column privileges. If wrong, switch to column grants plus an event trigger that grants new columns: medium rework of the bootstrap SQL.
- R2: `cheladb_auth_sync` is a per-branch login role with a sealed password, and `auth` connects directly. If wrong, route the sign-up transaction through rest or the orchestrator: large.
- R3: Cheladb's own key pair in `.cheladb/auth/`, with a 15-minute access TTL, a 30-day sliding refresh and a 90-day cap. TTLs are env settings, so changing them is cheap.
- R4: the gateway checks session liveness through a 30 s cache, so revocation lands within 30 s. If no check is wanted, removing it is cheap.
- R5: sign-up from a non-auth database's key inserts on the auth database's main branch. Alternative: refuse it. Cheap to change.
- R6: password reset and verification send codes (Zitadel emails them), not links. Links need per-project URL templates later. Cheap.
- R7: app users requires the data API enabled on every branch of the auth Postgres database. Relaxing that needs the role bootstrap split out: medium.
- R8: an always-present `deleted_at` managed column. Cheap.
- R9: refresh tokens live in the SDK's storage (localStorage by default), with a 10 s concurrent-refresh grace. Removing the grace is cheap.
- R10: the Zitadel org id equals the project uuid, which keeps the mapping trivial. Cheap.
- R11: the Actions v2 webhook target is instance-wide, created by `cheladb dev bootstrap`, and Compose overrides Zitadel's deny list (self-hosted only). Platform mode needs its own decision.
