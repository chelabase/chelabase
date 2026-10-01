# Cheladb

Local-only DBaaS control-plane monorepo.

Apache-2.0. Tooling: [proto](https://moonrepo.dev/proto) + [moon](https://moonrepo.dev/moon) + [just](https://github.com/casey/just). Rust workspace for the control plane; pnpm workspace for Studio and shared TS packages. Use **`just`** for day-to-day lifecycle.

## Repository layout

```
apps/studio          the only user-facing app
services/            gateway, orchestrator, worker, rest, auth (was ui-auth)
tools/cli            the `cheladb` binary (binary and package name unchanged)
crates/, packages/   shared Rust and TypeScript libraries, unchanged
infra/compose/       metadata Postgres, NATS JetStream, Zitadel
```

## Prerequisites

- [proto](https://moonrepo.dev/docs/proto)
- Docker (Compose deps **and** Docker socket for the worker)
- Rust toolchain (edition 2021; rustc 1.98+ works)
- cmake (Pingora builds zlib-ng): `sudo apt-get install -y cmake`

```bash
proto install   # moon, node, pnpm, just from .prototools
pnpm install
```

## Lifecycle (`just`)

| Command | What it does |
| --- | --- |
| `just` | List recipes |
| `just deps-up` / `up` | Compose up `-d` |
| `just deps-down` / `down` | Compose down **only** |
| `just zitadel-bootstrap` | `cargo run -p cheladb -- dev bootstrap` |
| `just auth-token` | Mint demo JWT access token (stdout) |
| `just zitadel-reset` | Wipe **only** Zitadel volume so FirstInstance rewrites `admin.pat` |
| `just dev` | Compose up + bootstrap + **detached** apps; prints seed IDs |
| `just dev-down` | Stop host processes from `just dev` (Compose stays up) |
| `just stop` | `dev-down` then `down` |
| `just create-db-demo` | Authenticated `POST /v1/databases` through the gateway (:8000) |
| `just sweep-dbs` | Dev reset: remove every worker-provisioned container and data volume (+ all metadata rows if deps up) |
| `just check` | Rust + TS typechecks |
| `just test` | Rust tests with nextest, then doctests |
| `just sweep-builds` | Delete build artifacts older than 14 days (`days=N` to change) from both target dirs |
| `just wt-add <branch> [base]` / `wt-remove <branch>` | Create or remove a worktree under `.worktrees/` |
| `just orchestrator` / `worker` / `gateway` / `rest` / `auth` / `studio` | Foreground one process |
| `just cleanup` | Fresh start: stops dev apps, deletes tenant databases, every Zitadel account and all Mailpit email, then re-bootstraps (keeps keys and the seeded tenant). Asks first |
| `just auth-e2e` | Live sign-up, sign-in, reset and logout through the gateway (needs `just dev`) |
| `just data-e2e` | Live data API story: keys, queries, generated types, RLS, revoke (needs `just deps-up` and `just dev`) |

```bash
just dev                 # deps + IAM bootstrap + background processes
just create-db-demo      # → 202 + database_id (mints JWT via cheladb CLI)
# poll (with minted token):
#   TOKEN=$(cargo run -q -p cheladb -- dev token)
#   curl -sS -H "Authorization: Bearer $TOKEN" \
#     http://127.0.0.1:8000/v1/databases/<database_id>
just stop
```

If bootstrap fails with missing `admin.pat` (old Zitadel volume before FirstInstance machine):

```bash
just zitadel-reset && just zitadel-bootstrap
```

### Builds and worktrees

`just test` runs the Rust tests with [cargo-nextest](https://nexte.st) (`cargo install cargo-nextest --locked`) and then the doctests. Dev builds carry line tables only (no full debug info), which keeps test binaries small. `just sweep-builds` (needs `cargo install cargo-sweep --locked`) drops build artifacts older than 14 days from `target/` and `.worktrees/target`; `just cleanup` runs it too. `just wt-add <branch>` makes a worktree in `.worktrees/<branch>` with `.cheladb` and `apps/studio/.env.local` linked from the main checkout and dependencies installed; `just wt-remove <branch>` removes it and keeps the branch. All worktrees share one Cargo target dir, `.worktrees/target` (set in `.worktrees/.cargo/config.toml`), so dependencies build once. The main checkout stays on `main`.

Logs for detached processes: `.cheladb/dev/*.log`. IAM artifacts: `.cheladb/zitadel/` (gitignored). Zitadel PATs are opaque — demo calls use **JWT profile** access tokens from `cheladb dev token`.

## Seed tenant / project (local demo)

| Entity | Slug | UUID |
| --- | --- | --- |
| Tenant | `demo` | `00000000-0000-4000-8000-000000000001` |
| Project | `default` | `00000000-0000-4000-8000-000000000002` |

Bootstrap sets `tenants.zitadel_org_id` for the demo tenant from the local Zitadel org.

## Auth

- All API traffic goes through the gateway on `:8000`. Endpoints and their policy are declared once in `crates/cheladb-routes`.
- Management API endpoints (`/v1/*`) require `Authorization: Bearer <Zitadel JWT>`. The gateway verifies it via JWKS, then forwards a 60-second EdDSA **internal identity token** (`x-cheladb-identity`) signed for the target service. It never forwards `Authorization` or a client-supplied identity header.
- The orchestrator trusts only the internal token (`CHELADB_INTERNAL_VERIFY_KEY`) and maps its `org_id` to a tenant (soft-bind via `tenants.zitadel_org_id`). Calling `:8081` directly returns `401`.
- `cheladb dev bootstrap` creates the gateway keypair in `.cheladb/gateway/` (once; never overwritten) and writes both key paths into `.cheladb/zitadel/demo.env`.
- `GET /health` is public; `GET /_gateway/health` is the gateway's own liveness check.
- Forwarding headers: the gateway appends the client IP to `X-Forwarded-For` (never trust it for authorization) and sets `X-Forwarded-Proto: http` only when a TLS-terminating proxy in front hasn't already set it.
- Gateway env: `CHELADB_GATEWAY_ADDR` (default `127.0.0.1:8000`), `CHELADB_ORCHESTRATOR_URL` (default `http://127.0.0.1:8081`), `CHELADB_MANAGEMENT_HOSTS` (comma-separated; empty = any host), `CHELADB_STUDIO_ORIGINS` (default `http://localhost:3000`), `CHELADB_GATEWAY_UPSTREAM_TIMEOUT_SECS` (default `60`).

Design notes:

- [create-database vertical slice](docs/superpowers/specs/2026-09-27-create-database-vertical-slice-design.md)
- [orchestrator auth](docs/superpowers/specs/2026-09-27-orchestrator-auth-design.md)
- [API gateway](docs/superpowers/specs/2026-09-27-gateway-design.md)
- [Studio auth](docs/superpowers/specs/2026-09-27-studio-auth-design.md)

## Studio sign-in and data fetching

Studio has its own sign-in pages (`/login`, `/register`, `/verify`, `/password-reset`, `/logout`). Zitadel v4 sends every sign-in there; the pages call `auth` through the gateway (`/auth/v1/*`), which talks to Zitadel's Session, User and OIDC v2 APIs with the login-client token and keeps step state in encrypted HttpOnly cookies. In platform mode, answers don't reveal whether an account exists (with one known limit: signing up and then signing in with the same email and password tells a new email from a taken one). In self-hosted mode, a sign-up for a taken email says so and sends you to sign in.

Studio uses TanStack Query for all data fetching. Lists poll every 2 seconds while a database or branch is pending, provisioning, resetting or deleting; polling stops when the status settles. Writes invalidate exactly the queries they change (not whole cache keys, only the affected entities). Secrets—connection details, passwords, created or rotated API keys—never enter the query or mutation cache; they live in component state and disappear when the panel closes. In dev builds, TanStack Query DevTools are available for inspection.

1. `just dev`, then open http://localhost:3000 and choose **Create account** (or **Sign in**).
2. You land in Studio straight away (email verification is off in self-hosted mode). Emails such as password resets land in **Mailpit**: http://localhost:8025.
3. After verifying you land on `/app`: your tenant and project, their databases, and a form to create one. Everything goes through the gateway on `:8000` with your Zitadel JWT.

How it's wired:

- `cheladb dev bootstrap` (idempotent) registers the `cheladb-studio` OIDC app (PKCE, JWT access tokens; drifted redirect settings are corrected in place), points Zitadel's SMTP at Mailpit, creates `.cheladb/auth/cookie.key`, and writes `apps/studio/.env.local` (gitignored). It needs `.cheladb/zitadel/login-client.pat`, which Zitadel writes only on a fresh instance (`just zitadel-reset`).
- `CHELADB_MODE` is `self-hosted` (the default, the dev mode) or `platform` (the hosted cloud, not built yet). Email verification follows it: off in self-hosted mode (sign-ups are created with the email already verified), required in platform mode. `CHELADB_REQUIRE_EMAIL_VERIFICATION=true` or `=false` overrides it; codes land in Mailpit.
- Everyone who signs up joins the same organization, tenant and project (a shared team workspace, by design for self-hosting). Per-signup isolation is the "one org per signup" option in the spec.
- Open Studio at `http://localhost:3000`; visits to `127.0.0.1:3000` are redirected there, since the Zitadel redirect URI and gateway CORS are registered for `localhost`.
- New signups join the bootstrapped org, which is bound to the demo tenant. That pin is one scope in `apps/studio/src/lib/auth.ts`. Moving to one organization per signup is sketched in the [Studio auth spec](docs/superpowers/specs/2026-09-27-studio-auth-design.md).
- `GET /v1/me` returns the caller plus their tenant, projects and databases, or `tenant: null` when the org has no tenant.

## Create-database smoke test

1. `just dev` (needs Docker; the worker starts each database's compute container)
2. `just create-db-demo demo1` (positional; `name=demo1` would become the literal name)
3. Poll `GET http://127.0.0.1:8000/v1/databases/<id>` with a minted Bearer token until `status` is `ready`
4. `GET http://127.0.0.1:8000/v1/databases/<id>/connection` (same token) returns the owner credentials and a `postgresql://postgres:…@<ep>.localtest.me:4432/<db>?sslmode=verify-full` URI. Connect with the proxy's CA: `psql "<uri>&sslrootcert=.cheladb/neon/proxy-ca.crt"`, adding `&hostaddr=127.0.0.1` if `localtest.me` resolves to `::1` first
5. `DELETE http://127.0.0.1:8000/v1/databases/<id>` removes it (or use Studio). `just sweep-dbs` is the dev-only "remove everything" reset.

## Database lifecycle

Each database gets a Neon tenant and timeline on the storage controller, plus one compute container (`cheladb-compute-<ep>`). The `postgres` role is a member of `neon_superuser`, and a real superuser only when `CHELADB_DB_OWNER_SUPERUSER` is on and `CHELADB_MODE` is self-hosted. Clients connect through Neon's proxy: Postgres wire at `<ep>.localtest.me:4432`; SQL over HTTP and WebSocket at `https://<ep>.localtest.me:4444/sql`. Use `sslrootcert=.cheladb/neon/proxy-ca.crt`; if `localtest.me` resolves to `::1` first, also add `hostaddr=127.0.0.1`.

The data API roles `anon` (NOLOGIN), `authenticated` (NOLOGIN) and `authenticator` (LOGIN NOINHERIT, with its own encrypted password per branch) are created in every database. The `cheladb_platform` role is gone.

- **Connection details** (`GET /v1/databases/{id}/connection`, Studio → *Connection details*) can be revealed any number of times. Passwords are stored encrypted (AES-256-GCM, bound to the database id) with the key at `CHELADB_SECRETS_KEY` (`.cheladb/secrets/secrets.key`, created by bootstrap). Keep that file: without it stored passwords can't be decrypted. The orchestrator and worker check it at startup and refuse to run with a different key while encrypted credentials exist (after `just sweep-dbs` a new key is simply adopted). Databases from before Neon provisioning became `deleted` tombstones when its migration ran; recreate them.
- **Reset password** (`POST …/reset-password`) sets a new owner password. Existing sessions keep working until they reconnect. If the worker isn't running you get `503`. A `503` saying the password "was changed but not yet applied" means the new password is stored (reveal shows it; Studio shows it with the error) and the worker's reconcile applies it.
- **Delete** (`DELETE /v1/databases/{id}`, Studio asks you to type the name) removes the compute and the tenant, with all its data. The row stays as a `deleted` tombstone (hidden from the API) and the name can be reused.
- **Reconcile**: every 30 s the worker:
  - fails databases stuck provisioning (> 10 min) and tears down what they built;
  - retries deletes stuck for more than a minute;
  - starts a new compute for a ready database whose container is gone (an exited container is left to Docker's restart policy);
  - applies a password reset that was stored but not applied;
  - removes compute containers that have no endpoint row;
  - retries the teardown of an endpoint whose database is failed or deleted.

  A storage-controller tenant with no endpoint row, or an endpoint row with no database, is only reported in the worker log, never removed: it may hold the only copy of someone's data. In local dev, `just sweep-dbs` removes every database, endpoint and endpoint tenant; delete an unknown tenant by hand.
- `CHELADB_DB_OWNER_SUPERUSER=true` makes the `postgres` role of *new* databases a real superuser (self-hosted only; ignored in platform mode). Users can then alter `cloud_admin` or `neon_superuser` and break the compute's reconfigure and the worker's admin SQL for that database.
- Connections go through Neon's proxy (above), never straight to a compute.
- **Upgrading from before this feature is one-way.** The migration drops the old plaintext `admin_password` column, so older binaries can't run against the upgraded metadata database (and sqlx refuses to start with a migration it doesn't know). Existing databases keep running; recreate them to get revealable credentials.
- Metadata tests that need a real database are opt-in: `cargo test -p cheladb-metadata -- --ignored --test-threads=1` (with `just deps-up`).

## Branches

Every database can have branches: copies of another branch's data, each with its own Neon timeline, endpoint, compute container and `postgres` password. Main is itself a branch, created when the database is provisioned. Branches form a tree—a branch can be created from any other branch.

- **Cap:** A database has at most `max_branches` live normal branches (default 5, range 0–20), not counting main or backup branches. Set it with `PATCH /v1/databases/{id}` in Studio's database settings. The cap is checked when a new branch is created; if reached, the create is refused.
- **History window:** Each database has a `history_window` (default 7 days, range 1 hour to 30 days) that controls how far back you can restore or create a branch from. The window is sent to Neon's storage controller and affects how much history is kept. Shrinking the window lets the pageserver remove older history. Set it with `PATCH /v1/databases/{id}` in Studio's database settings.
- **Creating a branch** (`POST /v1/databases/{id}/branches`): specify a name, an optional parent (defaults to main), and an optional point in time (defaults to now). If you specify a time, the branch starts at that LSN. The create is async: the branch appears immediately with `pending` status, and its status moves to `provisioning`, then `ready` when the compute is up. If the point is outside the history window, the branch fails with `outside the history window; the oldest available point is <time>`: the time of the oldest commit Neon still keeps, or the source branch's creation time when there's no commit there yet (a database younger than its window). If the point is before the source branch's own history begins (it starts where the source was branched from its parent), the branch fails with `before <source>'s history begins (<time>); branch from <its parent> instead`, or, for a restore backup, `before the history of <backup> begins (<time>)`. Restoring a branch to such a point fails the same way. Studio's time pickers start at the later of the history window's start and the branch's `history_start`, which the API returns (null when unknown). A storage-controller failure that may pass (a 5xx, no answer, or a timeout) is retried: the job is delivered again, 5 times in all and 30 s apart (about 2 minutes, enough for a storage-controller restart), before the branch fails with it; a refusal (4xx) fails at once. Reset to parent and restore retry the same way before they switch timelines. Creating a timeline may take up to 2 minutes and a restore's detach up to 10; a detach that times out is never undone while it may still be running: the job is retried, and the retry finishes the detach. Each retry records its cause on the branch; if the detach is still unresolved 20 minutes after the restore made its timeline, the branch fails with that cause, and nothing is deleted (the branch keeps its old timeline, and reconcile reports the kept pending timeline). A branch that fails stays listed as `failed` with its error; it keeps its name and counts toward the cap until you delete it. A failed main shows on the branch, not on the database: `databases.status` stays the database's own lifecycle. A restore that stays stuck fails the branch with a message ending "delete the branch to remove it"; such a branch can only be deleted.
- **Restore:** `POST /v1/databases/{id}/branches/{bid}/restore` takes a branch back to a point in time, creating a new timeline. The branch keeps its connection string and its `postgres` password. Restore creates a backup branch (`<name>-before-restore-<yyyymmddThhmmssZ>`) that holds the old timeline for recovery, with no compute. To connect to a backup, create a branch from it.
- **Reset to parent** (`POST /v1/databases/{id}/branches/{bid}/reset-to-parent`): moves a branch's timeline to the parent's latest state, keeping the same connection string and password.
- **Children rule:** Delete, reset-to-parent and restore are refused if the branch has live normal child branches. Each child's timeline branches from its parent's, and Neon refuses (412) to delete a timeline that others branch from; v1 doesn't re-parent children onto another timeline, so the children go first. Backup branches never count as children; a branch's backups are deleted when the branch itself is deleted.
- **`just branch-e2e`** runs the story end to end through the gateway, checking the data with psql through the proxy: a database whose main gets rows A and then B; a branch at a time between the two (it has only A); C on main and a reset to parent (the branch has A, B and C); a refused restore of main while it has that child, then deleting the branch; a restore of main to the noted time (main has A; the backup has no compute, and a branch of it has A, B and C); a 1 hour window and a point 2 hours back, which fails with the oldest available point; and finally deleting the database, checking that every compute, the tenant, the endpoint rows and the branch rows are gone. It takes about 40 s.

## Management API

OpenAPI documents are generated from the Rust route handlers in `crates/cheladb-routes` using utoipa 6 and utoipa-axum. Two documents are maintained: `openapi/management.json` (the Management API, the orchestrator's `/v1`, meant to go public later) and `openapi/login.json` (the auth `/auth/v1` login API for Studio only). The route tests fail when the documents are stale or when a route isn't documented.

After changing any management or login route, its request/response structs, or error codes, run `just api-gen`. This command:
- Rewrites both OpenAPI documents with `CHELADB_BLESS=1` on the document tests
- Runs orval 8 to regenerate the TypeScript clients
- Syncs the Scalar API reference UI bundle with `just scalar-sync`
- Commit the result; `just check` will fail if the documents, generated code or Scalar bundle are out of date.

**Generated clients.** Two packages are generated from the documents using orval 8:
- `packages/management-client`: `@cheladb/management-client` (private for now) with TanStack Query hooks (`useGetDatabase`, `useCreateBranch`), plain functions (`getDatabase(id)`), and query key helpers (`getGetDatabaseQueryKey(id)`). Configure it once with `configureManagementClient({ baseUrl, getToken, onUnauthorized })`.
- `apps/studio/src/lib/login-client`: Studio's internal login client, generated the same way.

Generated files are never edited by hand. Errors from these clients are thrown as `ApiError { status, code, message, extra }`, so clients can switch on the stable `code` field rather than the message.

**Error shape.** All management, login and gateway errors use one shape: `{"error": {"code": "<code>", "message": "<sentence>", ...}}`. The `code` is stable snake_case (e.g. `not_found`, `branch_not_ready`, `has_children`, `name_taken`, `tenant_not_bound`, `invalid_request`, `too_large`, `rate_limited`, `unavailable`, `worker_unavailable`, `not_applied`, `sign_in_expired`, `invalid_credentials`). The full list is in the `ErrorCode` enum (`crates/cheladb-types/src/api_error.rs`) and the OpenAPI documents. Another tenant's database answers 404 `not_found`, exactly like a missing one. The data API keeps its own codes and extra fields (`detail`, `hint`, `sqlstate`).

**API reference.** Scalar provides a self-hosted API reference UI at `GET /v1/docs` behind the gateway (public; the orchestrator serves the page plus `/v1/docs/scalar.js`, the embedded bundle) and queries `GET /v1/openapi.json`. Studio has an **API** page (`/app/api`, linked next to Sign out) rendering the same document with "Try it out" sending the signed-in user's token automatically. Scalar is configured to never contact scalar.com: telemetry off, default fonts off, no request proxy (`proxyUrl: ""`), external URLs empty, agent/MCP off, developer tools never. Scalar is versioned via pnpm pinning: root `@scalar/api-reference` (exact) and Studio `@scalar/api-reference-react` (exact). To update: bump both with pnpm (minimum release age applies), run `just scalar-sync` to copy `dist/browser/standalone.js` into `services/orchestrator/assets/scalar/` with a `VERSION` file, then commit. The orchestrator embeds Scalar at compile time, so Rust builds need no Node.

## Local dependencies

| Service | Host port | Notes |
| --- | --- | --- |
| metadata Postgres | 5433 | Control-plane metadata |
| NATS (+ JetStream) | 4222 (monitor 8222) | Provision queue |
| Zitadel | 8080 | Console: http://localhost:8080/ui/console |
| Zitadel Postgres | 5434 | Zitadel’s own DB |
| Mailpit | 8025 | Local email inbox (signup verification codes) |
| Orchestrator | 8081 | Internal HTTP API (via gateway) |
| Worker health | 8082 | `/health` |
| Gateway | 8000 | HTTP front door (Pingora) |
| auth | 8084 | Sign-in backend (via gateway at `/auth/v1`) |
| rest | 8085 | Data API engine (via gateway at `/data/v1`) |
| rest internal | 8086 | Orchestrator's schema calls to rest |
| Neon storage controller | 1234 | Tenants and timelines |
| Neon proxy | 4432, 4444 | Postgres wire; WebSocket and SQL over HTTP |
| Valkey | 6379 | Proxy cache invalidation and query cancel |

Copy [`infra/compose/.env.example`](infra/compose/.env.example) to a root `.env` for dotenv auto-load. Prefer values from `.cheladb/zitadel/demo.env` after bootstrap.

## Data API

Apps read and write a branch's tables over `POST /data/v1/query` with per-branch keys. The gateway checks the key; the `rest` service (`services/rest`, `just rest`) turns the request into SQL and runs it.

### Request flow

```
POST /data/v1/query, Authorization: Bearer <key>  →  gateway
  ↓ sha256(key) → cached internal listener
  ↓ branch_id, role, key_id from the key
  ↓ mint internal identity token (aud: "rest", branch_id, role, key_id)
  ↓ rest service, verified identity
  ↓ GET internal listener for branch connection (cached 60 s)
  ↓ pool for the branch (sqlx, as `authenticator`)
  ↓ BEGIN
  ↓ one round trip: set_config for role, request.jwt.claims, statement_timeout and
  ↓   transaction_timeout; read cheladb.schema_version
  ↓ [introspect if the schema version is new]
  ↓ parse query → shape cache → generate SQL → execute
  ↓ COMMIT
  ↓ JSON response
```

Keys authenticate with the `Authorization: Bearer` header. The gateway resolves the key (cached 30 s) and mints an internal token that `rest` verifies. Unknown keys are also cached 30 s, and cache misses (lookups reaching the internal listener) are limited to 20 per second per client IP; over the limit gets 429.

`rest` listens on `127.0.0.1:8085` (internal listener on `:8086`). Its env:
- `CHELADB_REST_ADDR` and `CHELADB_REST_INTERNAL_ADDR` (the defaults above);
- `CHELADB_REST_MAX_CONNECTIONS` (default 200): permits shared by every branch's pool, on top of each branch's own `max_connections` setting;
- `CHELADB_REST_MAX_RESPONSE_BYTES` (default 16 MiB): the largest response. Each operation's result is counted as it arrives; the first one past the cap rolls the request back and answers 413 `too_large`.

The gateway answers its own 404 and 405 under `/data/` with the data API's error body (code `invalid_query`).

### Keys

Each branch has exactly one live **public key** (role `anon`, safe for browsers) and zero or more **secret keys** (chosen role, server only). Both start with a prefix (`chk_pub_` or `chk_sec_`) and are 51 characters total.

- **What is stored:** the SHA-256 of every key and its first 12 characters (shown in lists). A secret key is returned once and Cheladb keeps no copy (Studio shows it in a copy-once box). The public key is also stored encrypted, so Studio can show it again.
- **Public key:** created when the branch first becomes ready. It can't be revoked (409: rotate it instead). Rotating creates a new key with the same role and name and revokes the old one in one transaction; the old key keeps working until the gateway's 30-second cache expires.
- **Secret key:** choose a role when creating one (`POST /v1/databases/{id}/branches/{bid}/keys`; the default is `authenticated`). The role is checked and granted to `authenticator` at creation; a refused role is a 422 saying why. A role is refused when it:
  - isn't a valid role name (a letter or `_`, then letters, digits, `_` or `$`; at most 63 characters);
  - is `authenticator` or `cheladb_admin`, or any `pg_*` role;
  - doesn't exist in the branch's database;
  - is a superuser, or a member (at any depth) of a superuser role, `neon_superuser`, `cheladb_admin`, `pg_execute_server_program`, `pg_read_server_files` or `pg_write_server_files`.
- **Rotating a secret key** keeps its role and doesn't run the check or the grant again. A reset or restore rewinds the grant, so the worker's bootstrap grants each live secret key's role again (a role that's gone or now refused is skipped with a warning, and its key stops working).
- **Revocation:** effective within 30 seconds (the gateway cache).
- **Lifecycle:** keys belong to the branch, not its timeline, so reset and restore keep them. Deleting a branch deletes its keys.

### Schema model

The `cheladb` schema holds machinery only, reserved and never exposed. Other reserved schemas: `pg_catalog`, `information_schema`, `pg_toast`, `neon`, `neon_migration` and temp schemas. An object is reachable when the key's role has `USAGE` on its schema and any privilege on the object: any column-level `SELECT`, `INSERT` or `UPDATE`, or table-level `DELETE`, for tables, views, materialized views and foreign tables, and `EXECUTE` for functions. Anything else answers like a name that doesn't exist (`unknown_table`, `unknown_column`, `unknown_function`).

**Default privileges.** The worker's bootstrap runs on every compute (re)creation and from reconcile, and is idempotent. It sets, for objects the owner role (`postgres`) creates:
- `anon`: `SELECT` on tables and views in `public`, and `USAGE` on the `public` schema.
- `authenticated`: `SELECT, INSERT, UPDATE, DELETE` on tables and views in `public`; `USAGE, SELECT` on sequences in `public`; `USAGE` on the `public` schema.
- Functions: `EXECUTE` is revoked from `PUBLIC` for every function the owner creates (any schema) and granted to `authenticated` in `public`. `anon` gets no function by default.

The first bootstrap of a database also grants the same on what already exists in `public` (tables, sequences, functions the owner made; extension functions are skipped). Later bootstraps don't repeat it.

**Row-level security (RLS) is off by default.** Studio warns about every reachable object without it:
- a table: `ALTER TABLE … ENABLE ROW LEVEL SECURITY; CREATE POLICY …`;
- a view runs with its owner's rights, so RLS on the tables it reads doesn't apply: `ALTER VIEW … SET (security_invoker = true)`;
- a materialized view or a foreign table can't have RLS: `REVOKE ALL … FROM anon, authenticated`.

### Query language

One request is one operation: a JSON tree. `"v": 1` is required at the top, and unknown fields are rejected anywhere. A table or function name is `"name"` (in the database's `default_schema`) or `"schema.name"`. These examples are the contract fixtures in `packages/client/fixtures`.

- **`select`**: `from`, optional `select` (a list of column names or `{"col", "as"}`; omitted means every column), `where`, `orderBy`, `limit`, `offset`.
  ```json
  {
    "v": 1,
    "op": "select",
    "from": "orders",
    "select": ["id", "total", { "col": "customer_name", "as": "customer" }],
    "where": { "and": [{ "col": "status", "eq": "paid" }, { "col": "total", "gte": "100" }] },
    "orderBy": [{ "col": "created_at", "dir": "desc", "nulls": "last" }],
    "limit": 50
  }
  ```
  `dir` is `asc` or `desc`, `nulls` is `first` or `last`; both are optional.
- **`insert`**: `into`, `values` (a non-empty array of objects with the same keys), optional `returning` (`"*"` or a list of columns).
  ```json
  { "v": 1, "op": "insert", "into": "orders", "values": [{ "total": "12.50", "region": "eu" }], "returning": ["id", "total"] }
  ```
- **`update`**: `table`, `set`, and `where` or `"all": true`; optional `returning`.
  ```json
  { "v": 1, "op": "update", "table": "orders", "set": { "status": "shipped" }, "where": { "col": "id", "eq": 7 }, "returning": "*" }
  ```
- **`delete`**: `from`, and `where` or `"all": true`; optional `returning`.
  ```json
  { "v": 1, "op": "delete", "from": "orders", "where": { "col": "status", "eq": "pending" } }
  ```
  An update or delete with neither `where` nor `"all": true` is refused (`invalid_query`). To touch every row, say so: `{ "v": 1, "op": "delete", "from": "billing.invoices", "all": true }`.
- **`call`**: `fn` and named `args`, returning a scalar or rows.
  ```json
  { "v": 1, "op": "call", "fn": "add_points", "args": { "user_id": "7c9e6679-7425-40de-944b-e07fc1f90ae7", "n": 5 } }
  ```
- **`batch`**: `ops`, up to 50 operations, all or nothing in one transaction. Batches don't nest; an operation inside may leave out `"v"`.
  ```json
  {
    "v": 1,
    "op": "batch",
    "ops": [
      { "op": "insert", "into": "orders", "values": [{ "total": "1.00", "region": "eu" }], "returning": ["id"] },
      { "op": "update", "table": "events", "set": { "label": "x" }, "where": { "col": "id", "eq": 1 } },
      { "op": "call", "fn": "add_points", "args": { "user_id": "u" } }
    ]
  }
  ```

**Filters.** A condition is `{ "col": "<column>", "<operator>": value }` with exactly one operator: `eq`, `neq`, `lt`, `lte`, `gt`, `gte`, `in` (an array), `is` (`null`, `true` or `false`), `like`, `ilike` (a string pattern). Combine with `{ "and": [...] }`, `{ "or": [...] }` and `{ "not": filter }`; a filter holds one of them and nothing beside it.
```json
{ "or": [{ "col": "region", "in": ["eu"] }, { "and": [{ "col": "vip", "eq": true }, { "col": "qty", "gt": 2 }] }] }
```

**Numbers keep every digit in a request.** A big JSON number in a body (say a 30-digit `numeric`) reaches Postgres exactly.

### SDK and type generation

`@cheladb/client` is a typed TypeScript SDK:

```ts
import { createClient } from "@cheladb/client";
import { type Database, types } from "./db"; // from `cheladb gen types`

const db = createClient<Database>({ url: "http://localhost:8000", key, types });
await db.from("orders").select("id", "total").where({ status: "paid" }).limit(10);
await db.from("orders").insert([{ total: "19.50" }]).returning("id");
await db.from("orders").update({ status: "shipped" }).where({ id });
await db.from("orders").delete().where({ id });
await db.call("add_points", { user_id: 10, n: 5 });
const [order, points] = await db.batch((tx) => [
  tx.from("orders").insert({ total: "1.00", region: "eu" }).returning("id"),
  tx.call("add_points", { user_id: 10, n: 5 }),
]);
```

An `update` or `delete` needs `.where()` or `.all()` before it compiles.

**A typed client requires `types`.** `createClient<Database>({ url, key })` without `types` is a compile error: without the map the client can't tell an `int8` or a `numeric` from any other number. The untyped `createClient({ url, key })` needs no `types`.

**Type generation:** `cheladb gen types --url <gateway-url> --key <secret-key> --out db.ts` writes a typed `Database` type and a `types` constant. Pass the key through the environment variable `CHELADB_DATA_KEY` instead of a flag, so it stays out of the process list.

**Big numbers:** `int8` and `numeric` columns are read exactly from JSON source text. The default type is `number` for `int8` (throws `UnsafeIntegerError` outside the safe range ±2^53) and `string` for `numeric`. Use `bigint: true` together with `types` to get `bigint` for all `int8` columns:

```ts
const db = createClient<Database>({ url, key, types, bigint: true });
```

See `packages/client/README.md` for details and runtime requirements.

### Settings

| Setting | Default | Min | Max |
|---|---|---|---|
| `max_rows` | 1000 | 1 | 100000 |
| `statement_timeout_ms` | 8000 | 100 | 120000 |
| `max_connections` | 5 | 1 | 50 |
| `default_schema` | `public` | — | must match `^[a-z_][a-z0-9_]{0,62}$` |

Changed with `PATCH /v1/databases/{id}` in Studio's database settings. Every setting takes up to 60 seconds to take effect (the connection-info cache). `max_connections` is per branch: the size of that branch's connection pool. `statement_timeout_ms` also sets Postgres's `transaction_timeout`, so it bounds the whole request: a batch of 50 operations shares one timeout, not 50.

### Error responses

The response is `{"error": {"code", "message", "detail"?, "hint"?, "sqlstate"?}}` with the status and code from this table.

| Status | Code | When |
|---|---|---|
| 400 | `invalid_query` | Syntax error, bad filter, wrong types, empty `select`/`returning`, an update or delete with neither `where` nor `all`. |
| 400 | `unknown_column` | A column that doesn't exist or that the key's role can't reach. |
| 400 | `invalid_value` | Not-null (23502) or check (23514) violation, or any type or value error (class 22). |
| 400 | `raised` | `RAISE EXCEPTION` in SQL (P0001). |
| 401 | `unauthorized` | Key unknown or revoked, or the branch is deleted. |
| 403 | `permission_denied` | Postgres permission denied (42501). |
| 404 | `unknown_table` | A table or view that doesn't exist or that the key's role can't reach. |
| 404 | `unknown_function` | A function that doesn't exist, can't be called through the API, or that the key's role can't execute. |
| 409 | `conflict` | Unique (23505), foreign key (23503) or exclusion (23P01) violation. |
| 409 | `retry` | Serialization failure or deadlock (40001, 40P01). Safe to retry. |
| 413 | `too_large` | Request body over 1 MiB, or a response over `CHELADB_REST_MAX_RESPONSE_BYTES`. |
| 429 | `rate_limited` | Cache misses over 20 per second from this IP address. |
| 500 | `internal` | Unexpected server error. The message includes a request id; check the logs. |
| 503 | `unavailable` | See below. |
| 504 | `timeout` | Query timeout (57014) or transaction timeout (25P04): the request ran past `statement_timeout_ms`. |

A 503 comes with `Retry-After`: `1` when the gateway answers it, `2` when `rest` does. Causes:
- the branch's database can't be reached, refuses the connection, or is out of connections or resources (Postgres classes 08, 28 and 53, and 57P01–57P03);
- no connection to the branch in time, or too many connections in use;
- `rest` can't look up the branch (the orchestrator is unreachable), or the branch isn't ready;
- the key's role can't be used right now (dropped, or its grant is gone until the bootstrap runs again);
- the data API's schema is missing; it will be reinstalled (by the worker's reconcile).

The 503 message is fixed text; what happened goes to the log with the request id.

A non-2xx response without this error body (a proxy error page) gets its code from status alone: 401 `unauthorized`, 413 `too_large`, 429 `rate_limited`, 502/503 `unavailable`, 504 `timeout`, else `internal`.

### Testing

`just data-e2e` (needs `just deps-up` and `just dev`) runs a live story through the gateway and the proxy:
1. Provision a database, list its public key, create a secret key (role `authenticated`) and a table.
2. Through the SDK: the secret key inserts, the public key reads, the public key's insert is refused (403), and a batch rolls back on error.
3. Generate types with `cheladb gen types` and typecheck a sample: a valid call compiles, a wrong column doesn't.
4. Add a row-level security policy: it hides a row from the public key (`anon`) but not from the secret key's role.
5. Revoke the secret key and check it gets 401 within 35 seconds.

`KEEP=1` leaves the database in place.

### Known limits

- **Default privileges:** cover only objects the owner role (`postgres`) creates. Objects created by any other role (a role you made, or a superuser) get no default grants; grant on them yourself.
- **Settings and schema changes:** up to 60 seconds to take effect (connection-info cache lifetime).
- **Revocation lag:** per gateway instance, up to 30 seconds.
- **IPv6 rate limiting:** the limiter groups IPv6 clients by `/64` prefix; a load balancer in front collapses all clients into one address.
- **Limiter size:** it remembers at most 100,000 addresses (idle ones are forgotten after 60 seconds). Past that it evicts entries, and an evicted address starts again with a full allowance.

## Neon

The storage stack starts with `just deps-up` and backs every database.

### Proxy and computes (step 3)

- **Ports.**
  - The orchestrator's internal listener is on `127.0.0.1:8083`, for the proxy, compute_ctl and the storage controller. Each has its own token, and nothing goes through the gateway.
  - Neon's proxy is on `127.0.0.1:4432` (Postgres) and `127.0.0.1:4444` (WebSocket and SQL over HTTP), for `<endpoint>.localtest.me`.
- **`just neon-e2e`** goes through the gateway API to create a test database, then proves that it answers through the proxy over Postgres wire, SQL over HTTP and the serverless driver; that a wrong password is refused; that query cancel works; and that a changed password works at once.
- **Valkey** (`127.0.0.1:6379`, base profile):
  - it's what makes query cancel work through the proxy;
  - the proxy caches role secrets for at most a minute, so a password change must publish `/password_updated` (`cheladb neon password-updated`) after reconfiguring the compute;
  - the roadmap's "Valkey: where else it helps" lists later uses.
- **Native Linux Docker.** `host.docker.internal` resolves to the bridge gateway, which can't reach a listener on `127.0.0.1`. There, set `CHELADB_INTERNAL_ADDR` to the bridge address (for example `172.17.0.1:8083`). Only the host part may change: the port stays 8083, because Compose and the compute use it. The scripts derive their internal URL from the same variable. Docker Desktop (WSL2) works as is.
- **`just neon-smoke`** now needs the orchestrator running (`just dev`).

## Still out of scope

Postgres wire proxy (`services/pgproxy` later), gateway TLS, per-signup organizations, API keys, protocol-admin RBAC, delete/reconcile, CI.
