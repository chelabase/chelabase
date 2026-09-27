# Create-Database Vertical Slice — Design Spec

**Date:** 2026-09-27  
**Status:** Implemented  
**License:** Apache-2.0  
**Depends on:** [2026-09-27-cheladb-scaffold-design.md](./2026-09-27-cheladb-scaffold-design.md)

## Intent

Thin vertical slice for async database provisioning on localhost. Prove control-plane flow without gateway routing or auth.

## Flow

1. Client `POST /v1/databases` with `tenant_id`, `project_id`, `name`
2. Orchestrator validates tenant/project, inserts `databases` row as `pending`, publishes JetStream job on `cheladb.provision`
3. Returns `202` with `job_id` + `database_id`
4. Worker consumes job, marks `provisioning`, runs `postgres:17` via Docker (bollard) with host port published on `127.0.0.1`
5. After TCP readiness, updates metadata to `ready` (container id, host, port, admin password)
6. Client `GET /v1/databases/{id}` polls status; when ready, can `psql` to host/port

## Seed data

Fixed UUIDs (migration `20260927000002_seed_demo.sql`):

| Entity | Slug | UUID |
| --- | --- | --- |
| Tenant | `demo` | `00000000-0000-4000-8000-000000000001` |
| Project | `default` | `00000000-0000-4000-8000-000000000002` |

## Lifecycle (`just`)

| Recipe | Behavior |
| --- | --- |
| `just dev` | Compose up + detached orchestrator/worker/gateway/studio; prints seed IDs |
| `just dev-down` | Kill host PIDs under `.cheladb/dev/` |
| `just down` | Compose down only |
| `just stop` | `dev-down` then `down` |
| `just create-db-demo` | Curl create with seed IDs |
| `just sweep-dbs` | Remove managed tenant containers (+ metadata rows if deps up) |

Tenant Postgres containers labeled `cheladb.managed=true` are **not** removed by `dev-down`/`down`. Use `just sweep-dbs` to stop/remove them (and `DELETE FROM databases` when metadata Postgres is reachable).

## Out of scope

Gateway wire routing, JWKS, API keys, reconcile consumer, delete API, Studio create UI.
