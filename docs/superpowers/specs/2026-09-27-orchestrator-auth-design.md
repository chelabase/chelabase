# Orchestrator Auth (JWKS + Soft Binding) — Design Spec

**Date:** 2026-09-27  
**Status:** Implemented  
**License:** Apache-2.0  
**Depends on:** [2026-09-27-create-database-vertical-slice-design.md](./2026-09-27-create-database-vertical-slice-design.md)

## Intent

Protect orchestrator `/v1/*` with local Zitadel JWKS JWT verification and org→tenant soft binding. Keep local IAM repeatable via an idempotent Rust CLI (`apps/cheladb`). Gateway JWT and Studio login are out of scope (next: gateway → studio).

## Flow

1. Client sends `Authorization: Bearer <JWT access token>` to `POST/GET /v1/databases…`
2. `cheladb-auth` verifies signature via JWKS cache, `iss`, `exp`; optional `aud` when present
3. Extract org from `urn:zitadel:iam:org:id` or `urn:zitadel:iam:user:resourceowner:id`
4. Resolve metadata tenant by `tenants.zitadel_org_id`
5. Soft-bind: request `tenant_id` (create) or row `tenant_id` (get) must match resolved tenant
6. `/health` remains public

## Soft binding

| Column | Purpose |
| --- | --- |
| `tenants.zitadel_org_id` | Unique Zitadel org id from JWT org/resourceowner claim |

Unset until `cheladb auth bootstrap` runs.

## Local IAM bootstrap

| Piece | Role |
| --- | --- |
| Compose FirstInstance | Machine `cheladb-bootstrap` + opaque admin PAT → `.cheladb/zitadel/admin.pat` (fresh volume only; for Management API) |
| `cheladb auth bootstrap` | Idempotent ensure: project, API app, demo machine + JWT profile key; bind demo tenant; write `demo.env` |
| `cheladb auth token` | Mint short-lived access token (JWT profile + resourceowner scope) for orchestrator calls |
| `scripts/zitadel-reset.sh` | Wipe Zitadel volume only when admin PAT missing |
| `just zitadel-bootstrap` / `auth-token` / `zitadel-reset` | Thin wrappers |
| `just dev` | deps-up → bootstrap → start apps |
| `just create-db-demo` | Mint token then Bearer POST |

Note: Zitadel personal access tokens are opaque and cannot be JWKS-verified. Demo API calls use JWT-profile access tokens from the Rust CLI.

## Out of scope

Gateway wire routing, Studio OIDC UI, token introspection, API keys, protocol-admin RBAC. Fuller CLI commands beyond auth are deferred.
