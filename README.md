# Cheladb

Local-only DBaaS control-plane monorepo.

Apache-2.0. Tooling: [proto](https://moonrepo.dev/proto) + [moon](https://moonrepo.dev/moon) + [just](https://github.com/casey/just). Rust workspace for the control plane; pnpm workspace for Studio and shared TS packages. Use **`just`** for day-to-day lifecycle.

## Layout

```
apps/
  orchestrator/   Axum API (create DB → JetStream; JWKS auth)
  worker/         JetStream consumer + Docker Postgres provisioner
  gateway/        HTTP API gateway (Pingora): routing, Zitadel JWT → internal identity
  ui-auth/        Sign-in backend for Studio's login pages; holds Zitadel's login-client token
  cheladb/        Local CLI (auth bootstrap / token mint; future control-plane commands)
  studio/         Vite + TanStack Router + Tailwind + shadcn (Base UI); Zitadel sign-in (oidc-client-ts)
crates/
  cheladb-types/
  cheladb-metadata/   sqlx migrations + client
  cheladb-nats/       JetStream helpers
  cheladb-auth/       Zitadel JWKS JWT verify + internal identity tokens
  cheladb-routes/     Public endpoint declarations shared by gateway + services
packages/
  types/          @cheladb/types
infra/compose/    metadata Postgres, NATS JetStream, Zitadel
scripts/          zitadel-reset (Compose volume escape hatch only)
docs/superpowers/specs/
justfile
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
| `just zitadel-bootstrap` | `cargo run -p cheladb -- auth bootstrap` |
| `just auth-token` | Mint demo JWT access token (stdout) |
| `just zitadel-reset` | Wipe **only** Zitadel volume so FirstInstance rewrites `admin.pat` |
| `just dev` | Compose up + bootstrap + **detached** apps; prints seed IDs |
| `just dev-down` | Stop host processes from `just dev` (Compose stays up) |
| `just stop` | `dev-down` then `down` |
| `just create-db-demo` | Authenticated `POST /v1/databases` through the gateway (:8000) |
| `just sweep-dbs` | Dev reset: remove every worker-provisioned container and data volume (+ all metadata rows if deps up) |
| `just check` | Rust + TS typechecks |
| `just orchestrator` / `worker` / `gateway` / `ui-auth` / `studio` | Foreground one process |
| `just cleanup` | Fresh start: stops dev apps, deletes tenant databases, every Zitadel account and all Mailpit email, then re-bootstraps (keeps keys and the seeded tenant). Asks first |
| `just auth-e2e` | Live sign-up, sign-in, reset and logout through the gateway (needs `just dev`) |

```bash
just dev                 # deps + IAM bootstrap + background processes
just create-db-demo      # → 202 + database_id (mints JWT via cheladb CLI)
# poll (with minted token):
#   TOKEN=$(cargo run -q -p cheladb -- auth token)
#   curl -sS -H "Authorization: Bearer $TOKEN" \
#     http://127.0.0.1:8000/v1/databases/<database_id>
just stop
```

If bootstrap fails with missing `admin.pat` (old Zitadel volume before FirstInstance machine):

```bash
just zitadel-reset && just zitadel-bootstrap
```

Logs for detached processes: `.cheladb/dev/*.log`. IAM artifacts: `.cheladb/zitadel/` (gitignored). Zitadel PATs are opaque — demo calls use **JWT profile** access tokens from `cheladb auth token`.

## Seed tenant / project (local demo)

| Entity | Slug | UUID |
| --- | --- | --- |
| Tenant | `demo` | `00000000-0000-4000-8000-000000000001` |
| Project | `default` | `00000000-0000-4000-8000-000000000002` |

Bootstrap sets `tenants.zitadel_org_id` for the demo tenant from the local Zitadel org.

## Auth

- All API traffic goes through the gateway on `:8000`. Endpoints and their policy are declared once in `crates/cheladb-routes`.
- Platform endpoints (`/v1/*`) require `Authorization: Bearer <Zitadel JWT>`. The gateway verifies it via JWKS, then forwards a 60-second EdDSA **internal identity token** (`x-cheladb-identity`) signed for the target service. It never forwards `Authorization` or a client-supplied identity header.
- The orchestrator trusts only the internal token (`CHELADB_INTERNAL_VERIFY_KEY`) and maps its `org_id` to a tenant (soft-bind via `tenants.zitadel_org_id`). Calling `:8081` directly returns `401`.
- `cheladb auth bootstrap` creates the gateway keypair in `.cheladb/gateway/` (once; never overwritten) and writes both key paths into `.cheladb/zitadel/demo.env`.
- `GET /health` is public; `GET /_gateway/health` is the gateway's own liveness check.
- Forwarding headers: the gateway appends the client IP to `X-Forwarded-For` (never trust it for authorization) and sets `X-Forwarded-Proto: http` only when a TLS-terminating proxy in front hasn't already set it.
- Gateway env: `CHELADB_GATEWAY_ADDR` (default `127.0.0.1:8000`), `CHELADB_ORCHESTRATOR_URL` (default `http://127.0.0.1:8081`), `CHELADB_PLATFORM_HOSTS` (comma-separated; empty = any host), `CHELADB_STUDIO_ORIGINS` (default `http://localhost:3000`), `CHELADB_GATEWAY_UPSTREAM_TIMEOUT_SECS` (default `60`).

Design notes:

- [create-database vertical slice](docs/superpowers/specs/2026-09-27-create-database-vertical-slice-design.md)
- [orchestrator auth](docs/superpowers/specs/2026-09-27-orchestrator-auth-design.md)
- [API gateway](docs/superpowers/specs/2026-09-27-gateway-design.md)
- [Studio auth](docs/superpowers/specs/2026-09-27-studio-auth-design.md)

## Studio sign-in

Studio has its own sign-in pages (`/login`, `/register`, `/verify`, `/password-reset`, `/logout`). Zitadel v4 sends every sign-in there; the pages call `ui-auth` through the gateway (`/auth/v1/*`), which talks to Zitadel's Session, User and OIDC v2 APIs with the login-client token and keeps step state in encrypted HttpOnly cookies. In platform mode, answers don't reveal whether an account exists (with one known limit: signing up and then signing in with the same email and password tells a new email from a taken one). In self-hosted mode, a sign-up for a taken email says so and sends you to sign in.

1. `just dev`, then open http://localhost:3000 and choose **Create account** (or **Sign in**).
2. You land in Studio straight away (email verification is off in self-hosted mode). Emails such as password resets land in **Mailpit**: http://localhost:8025.
3. After verifying you land on `/app`: your tenant and project, their databases, and a form to create one. Everything goes through the gateway on `:8000` with your Zitadel JWT.

How it's wired:

- `cheladb auth bootstrap` (idempotent) registers the `cheladb-studio` OIDC app (PKCE, JWT access tokens; drifted redirect settings are corrected in place), points Zitadel's SMTP at Mailpit, creates `.cheladb/ui-auth/cookie.key`, and writes `apps/studio/.env.local` (gitignored). It needs `.cheladb/zitadel/login-client.pat`, which Zitadel writes only on a fresh instance (`just zitadel-reset`).
- `CHELADB_MODE` is `self-hosted` (the default, the dev mode) or `platform` (the hosted cloud, not built yet). Email verification follows it: off in self-hosted mode (sign-ups are created with the email already verified), required in platform mode. `CHELADB_REQUIRE_EMAIL_VERIFICATION=true` or `=false` overrides it; codes land in Mailpit.
- Everyone who signs up joins the same organization, tenant and project (a shared team workspace, by design for self-hosting). Per-signup isolation is the "one org per signup" option in the spec.
- Open Studio at `http://localhost:3000`; visits to `127.0.0.1:3000` are redirected there, since the Zitadel redirect URI and gateway CORS are registered for `localhost`.
- New signups join the bootstrapped org, which is bound to the demo tenant. That pin is one scope in `apps/studio/src/lib/auth.ts`. Moving to one organization per signup is sketched in the [Studio auth spec](docs/superpowers/specs/2026-09-27-studio-auth-design.md).
- `GET /v1/me` returns the caller plus their tenant, projects and databases, or `tenant: null` when the org has no tenant.

## Create-database smoke test

1. `just dev` (needs Docker; worker talks to the Docker socket)
2. `just create-db-demo demo1` (positional; `name=demo1` would become the literal name)
3. Poll `GET http://127.0.0.1:8000/v1/databases/<id>` with a minted Bearer token until `status` is `ready`
4. `GET http://127.0.0.1:8000/v1/databases/<id>/connection` (same token) returns the owner credentials and a `postgres://postgres:…` URI; `psql "<uri>"`
5. `DELETE http://127.0.0.1:8000/v1/databases/<id>` removes it (or use Studio). `just sweep-dbs` is the dev-only "remove everything" reset.

## Database lifecycle

Each database is its own `postgres:17` container with a named data volume (`cheladb-db-<id>`) and two roles:

| Role | Who uses it | Privileges |
| --- | --- | --- |
| `postgres` | You (revealed in Studio / `…/connection`) | Owns the database and `public` schema; `CREATEDB`, `CREATEROLE`, `BYPASSRLS`; `pg_read_all_data`, `pg_write_all_data`, `pg_monitor`, `pg_signal_backend`; installs trusted extensions. **Not** a superuser. |
| `cheladb_platform` | The platform only (reset, delete, reconcile, later the Postgres proxy) | `SUPERUSER`. Never revealed. |

This is the Supabase/Neon pattern: near-full control for you, while the platform keeps access you can't break. Postgres 17 only lets `CREATEROLE` manage roles it created, so `postgres` can't alter or drop `cheladb_platform`.

- **Connection details** (`GET /v1/databases/{id}/connection`, Studio → *Connection details*) can be revealed any number of times. Passwords are stored encrypted (AES-256-GCM, bound to the database id) with the key at `CHELADB_SECRETS_KEY` (`.cheladb/secrets/secrets.key`, created by bootstrap). Keep that file: without it stored passwords can't be decrypted. The orchestrator and worker check it at startup and refuse to run with a different key while encrypted credentials exist (after `just sweep-dbs` a new key is simply adopted). Databases created before this existed report "credentials unavailable; recreate this database".
- **Reset password** (`POST …/reset-password`) sets a new owner password. Existing sessions keep working until they reconnect. If the worker isn't running you get `503`.
- **Delete** (`DELETE /v1/databases/{id}`, Studio asks you to type the name) removes the container and its data. The row stays as a `deleted` tombstone (hidden from the API) and the name can be reused.
- **Reconcile**: every 30 s the worker fails databases stuck provisioning (> 10 min) or whose container vanished, retries lost deletes, and removes the container/volume of deleted databases. A managed container or volume with *no* metadata row at all (e.g. after a metadata reset or restoring an old backup) is only reported in the worker log, never removed: it may hold the only copy of someone's data. Clear those deliberately with `just sweep-dbs`.
- `CHELADB_DB_OWNER_SUPERUSER=true` makes the `postgres` role of *new* databases a real superuser (self-hosted opt-in). Users can then break reset/delete/reconcile for that database.
- Connections currently go straight to `127.0.0.1:<port>`. A Postgres proxy (`pgproxy`) will replace that; it's a separate front door from the HTTP gateway because Postgres clients don't speak HTTP.
- **Upgrading from before this feature is one-way.** The migration drops the old plaintext `admin_password` column, so older binaries can't run against the upgraded metadata database (and sqlx refuses to start with a migration it doesn't know). Existing databases keep running; recreate them to get revealable credentials.
- **Security notes.** The password Postgres is initialised with (visible via `docker inspect`) is replaced right after init; only the encrypted replacement is stored. Because `postgres` has `pg_read_all_data`, it can read the platform role's SCRAM verifier in `pg_authid`; with 32 random characters that isn't practically crackable, but treat it as sensitive.
- Metadata tests that need a real database are opt-in: `cargo test -p cheladb-metadata --test live_db -- --ignored --test-threads=1` (with `just deps-up`).

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
| ui-auth | 8084 | Sign-in backend (via gateway at `/auth/v1`) |

Copy [`infra/compose/.env.example`](infra/compose/.env.example) to a root `.env` for dotenv auto-load. Prefer values from `.cheladb/zitadel/demo.env` after bootstrap.

## Still out of scope

Postgres wire proxy (apps/pgproxy), gateway TLS, per-signup organizations, API keys, protocol-admin RBAC, delete/reconcile, CI, git remote.
