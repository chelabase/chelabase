# Cheladb

Local-only DBaaS control-plane monorepo.

Apache-2.0. Tooling: [proto](https://moonrepo.dev/proto) + [moon](https://moonrepo.dev/moon) + [just](https://github.com/casey/just). Rust workspace for the control plane; pnpm workspace for Studio and shared TS packages. Use **`just`** for day-to-day lifecycle.

## Layout

```
apps/
  orchestrator/   Axum API (create DB → JetStream; JWKS auth)
  worker/         JetStream consumer + Docker Postgres provisioner
  gateway/        TCP stub (Pingora / wire routing later)
  cheladb/        Local CLI (auth bootstrap / token mint; future control-plane commands)
  studio/         Vite + TanStack Router + Tailwind + shadcn (Base UI) + Zustand
crates/
  cheladb-types/
  cheladb-metadata/   sqlx migrations + client
  cheladb-nats/       JetStream helpers
  cheladb-auth/       Zitadel JWKS JWT verify
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
| `just create-db-demo` | Authenticated `POST /v1/databases` with seed tenant/project |
| `just sweep-dbs` | Remove worker-provisioned Postgres containers (+ metadata rows if deps up) |
| `just check` | Rust + TS typechecks |
| `just orchestrator` / `worker` / `gateway` / `studio` | Foreground one process |

```bash
just dev                 # deps + IAM bootstrap + background processes
just create-db-demo      # → 202 + database_id (mints JWT via cheladb CLI)
# poll (with minted token):
#   TOKEN=$(cargo run -q -p cheladb -- auth token)
#   curl -sS -H "Authorization: Bearer $TOKEN" \
#     http://127.0.0.1:8081/v1/databases/<database_id>
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

- Orchestrator `/v1/*` requires `Authorization: Bearer <JWT>` (JWKS-verified access token).
- Soft-bind: claim `urn:zitadel:iam:org:id` or `urn:zitadel:iam:user:resourceowner:id` must map to the request’s `tenant_id`.
- `/health` is public.
- Local demo: `cargo run -p cheladb -- auth bootstrap` / `auth token` (machine key + JWT bearer grant).

Design notes:

- [create-database vertical slice](docs/superpowers/specs/2026-09-27-create-database-vertical-slice-design.md)
- [orchestrator auth](docs/superpowers/specs/2026-09-27-orchestrator-auth-design.md)

## Create-database smoke test

1. `just dev` (needs Docker; worker talks to the Docker socket)
2. `just create-db-demo name=demo1`
3. Poll `GET /v1/databases/<id>` with a minted Bearer token until `status` is `ready`
4. `psql` using `host`, `port`, user `cheladb`, password from `admin_password` (dev-only field)
5. `just sweep-dbs` to prune tenant containers (not removed by `just stop`)

## Local dependencies

| Service | Host port | Notes |
| --- | --- | --- |
| metadata Postgres | 5433 | Control-plane metadata |
| NATS (+ JetStream) | 4222 (monitor 8222) | Provision queue |
| Zitadel | 8080 | Console: http://localhost:8080/ui/console |
| Zitadel Postgres | 5434 | Zitadel’s own DB |
| Orchestrator | 8081 | HTTP API (auth on `/v1/*`) |
| Worker health | 8082 | `/health` |
| Gateway stub | 15432 | Not used for create path |

Copy [`infra/compose/.env.example`](infra/compose/.env.example) to a root `.env` for dotenv auto-load. Prefer values from `.cheladb/zitadel/demo.env` after bootstrap.

## Still out of scope

Gateway Postgres wire routing, Studio OIDC UI, API keys, protocol-admin RBAC, delete/reconcile, CI, git remote. Next: gateway → studio.
