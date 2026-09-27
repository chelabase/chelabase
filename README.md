# Cheladb

Local-only DBaaS control-plane monorepo.

Apache-2.0. Tooling: [proto](https://moonrepo.dev/proto) + [moon](https://moonrepo.dev/moon) + [just](https://github.com/casey/just). Rust workspace for the control plane; pnpm workspace for Studio and shared TS packages. Use **`just`** for day-to-day lifecycle.

## Layout

```
apps/
  orchestrator/   Axum API (create DB → JetStream; JWKS auth)
  worker/         JetStream consumer + Docker Postgres provisioner
  gateway/        HTTP API gateway (Pingora): routing, Zitadel JWT → internal identity
  cheladb/        Local CLI (auth bootstrap / token mint; future control-plane commands)
  studio/         Vite + TanStack Router + Tailwind + shadcn (Base UI) + Zustand
crates/
  cheladb-types/
  cheladb-metadata/   sqlx migrations + client
  cheladb-nats/       JetStream helpers
  cheladb-auth/       Zitadel JWKS JWT verify + internal identity tokens
  cheladb-routes/     Public endpoint declarations shared by gateway + services
packages/
  types/          @cheladb/types
  zitadel/        @cheladb/zitadel (OpenAPI → orval, pin v2.71.11)
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
| `just sweep-dbs` | Remove worker-provisioned Postgres containers (+ metadata rows if deps up) |
| `just check` | Rust + TS typechecks |
| `just orchestrator` / `worker` / `gateway` / `studio` | Foreground one process |

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

## Create-database smoke test

1. `just dev` (needs Docker; worker talks to the Docker socket)
2. `just create-db-demo demo1` (positional; `name=demo1` would become the literal name)
3. Poll `GET http://127.0.0.1:8000/v1/databases/<id>` with a minted Bearer token until `status` is `ready`
4. `psql` using `host`, `port`, user `cheladb`, password from `admin_password` (dev-only field)
5. `just sweep-dbs` to prune tenant containers (not removed by `just stop`)

## Local dependencies

| Service | Host port | Notes |
| --- | --- | --- |
| metadata Postgres | 5433 | Control-plane metadata |
| NATS (+ JetStream) | 4222 (monitor 8222) | Provision queue |
| Zitadel | 8080 | Console: http://localhost:8080/ui/console |
| Zitadel Postgres | 5434 | Zitadel’s own DB |
| Orchestrator | 8081 | Internal HTTP API (via gateway) |
| Worker health | 8082 | `/health` |
| Gateway | 8000 | HTTP front door (Pingora) |

Copy [`infra/compose/.env.example`](infra/compose/.env.example) to a root `.env` for dotenv auto-load. Prefer values from `.cheladb/zitadel/demo.env` after bootstrap.

## Still out of scope

Postgres wire proxy (apps/pgproxy), gateway TLS, Studio OIDC UI, API keys, protocol-admin RBAC, delete/reconcile, CI, git remote. Next: studio.
