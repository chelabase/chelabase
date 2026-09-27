# Cheladb Control-Plane Skeleton — Design Spec

**Date:** 2026-09-27  
**Status:** Approved for implementation  
**License:** Apache-2.0  

## Intent

Scaffold a local-only DBaaS control-plane monorepo for fast prototyping. Structure must support later extraction into separate service repos. No real database provisioning, JWT enforcement, or Postgres wire routing in this phase — compiling stubs and local infrastructure only.

## Goals

- moon + proto monorepo with a single Cargo workspace and a pnpm workspace for TypeScript
- Separate binaries: orchestrator, worker, gateway; UI app: studio
- Shared Rust crates and shared TS packages with clear boundaries
- Docker Compose for metadata Postgres, NATS JetStream, and self-hosted Zitadel
- Studio: Vite + TanStack Router + Tailwind + shadcn (Base UI) + Zustand
- Free/self-hosted stack (Apache-2.0 / MIT components)

## Non-goals

- Docker SDK provisioning of tenant Postgres
- Live NATS publish/consume logic
- Live JWT/JWKS middleware
- API keys implementation
- Token introspection on the hot path
- Pingora Postgres wire protocol parsing
- Studio product pages / auth UI
- Orchestrator → TypeScript OpenAPI codegen
- Rust Zitadel OpenAPI clients
- CI, git remote

## Architecture

```
Studio ──► (future) Orchestrator ──► Metadata Postgres
                │                         ▲
                │ (future publish)        │
                ▼                         │
              NATS JetStream ──► Worker ──┘
                                          │
Gateway ──► Metadata lookup (future) ─────┘
                │
                └── JWKS verify (future) ◄── Zitadel
Studio ──► @cheladb/zitadel (OpenAPI codegen) ──► Zitadel Management APIs
```

### Services

| App | Role | Skeleton behavior |
| --- | --- | --- |
| `orchestrator` | Axum HTTP control API | `GET /health`; `POST /v1/databases` → 202 + stub job id |
| `worker` | NATS JetStream consumer | Config load + TODO loop + health endpoint |
| `gateway` | Pingora (or TCP stub) | Bind port; unimplemented response |
| `studio` | Operator/org UI | Placeholder index route only |

### Shared libraries

**Rust (`crates/`):**

- `cheladb-types` — `TenantId`, `ProjectId`, `DatabaseId` newtypes
- `cheladb-metadata` — sqlx stub + empty migrations folder
- `cheladb-nats` — subject constants + message envelope stub
- `cheladb-auth` — Zitadel issuer/JWKS config only

**TypeScript (`packages/`):**

- `@cheladb/types` — hand-written domain + control-plane API stubs
- `@cheladb/zitadel` — OpenAPI-generated client from pinned Zitadel spec

## Auth direction (future)

- **Default:** JWKS local JWT verification for org users and protocol admins
- Distinguish personas via Zitadel applications/roles/claims + metadata RBAC
- Machine access via API keys stored in metadata (local hash check)
- **Defer** token introspection (not on gateway hot path)

## Local infrastructure

Compose services:

- `metadata-db` — Postgres 17 (control-plane metadata)
- `nats` — NATS with JetStream
- `zitadel` + `zitadel-db` — self-hosted Zitadel and its database

Rust apps and Studio run on the host against Compose. Tenant databases are not Compose services (future Docker SDK).

## Tooling

- proto pins: moon, rust, node, pnpm
- moon tasks per project
- Git: local only, no remote in this phase
- License: Apache-2.0

## Success criteria

1. `cargo check` succeeds for the workspace
2. Compose file is valid and documents env via `.env.example`
3. Studio starts via pnpm/moon with wired `@cheladb/types` and `@cheladb/zitadel`
4. README documents setup and run commands
5. Layout allows lifting an `apps/*` binary + needed crates/packages into its own repo later
