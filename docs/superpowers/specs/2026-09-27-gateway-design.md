# API Gateway — Design Spec

**Date:** 2026-09-27  
**Status:** Draft  
**License:** Apache-2.0  
**Depends on:** [2026-09-27-orchestrator-auth-design.md](./2026-09-27-orchestrator-auth-design.md)

## Intent

Replace the TCP stub in `apps/gateway` with the HTTP front door for Cheladb. It does the job Kong does in the Supabase stack, but not the way Kong does it. Kong is generic because it knows nothing about what sits behind it. This gateway fronts a small, known set of Cheladb services, so it is built around that knowledge:

1. **Routes are code, shared with the services.** A new `cheladb-routes` crate declares every public endpoint and its policy. The orchestrator builds its axum router from the same declarations, so the gateway and services can't drift. There is no route config file.
2. **Policy is typed.** Each route carries a `Policy` value (auth mode, CORS mode). Invalid combinations and ordering mistakes don't compile.
3. **The gateway translates identity.** It verifies the external credential (a Zitadel JWT today; API keys and project-user tokens later) and forwards a short-lived **internal identity token** it signs itself. Services verify one format with one public key and never talk to Zitadel.
4. **What changes at runtime is data, not routes.** Hostname → scope resolution lives in a `Registry` behind an atomic swap. It's static this phase; in hosted mode it's filled from metadata.

Engine: Pingora. Only the glue layer imports it.

## Non-goals (this phase)

- TLS, ACME, Caddy (the operator's reverse proxy or k8s ingress handles TLS)
- Postgres wire proxying (future `apps/pgproxy`)
- API keys, project-user auth, project data services
- Registry loaded from metadata, NATS-driven registry reload, file-watch reload
- Rate limiting, usage/metering events
- Self-hoster-defined extra upstreams (possible later as an explicit exception)
- Dockerfile / Compose service, Kubernetes

## Architecture

```
client ──HTTP :8000──► gateway ─────────────────────────────► orchestrator :8081
   Authorization: Bearer <Zitadel JWT>        X-Cheladb-Identity: <internal token>
                          │
      request-id → scope (Registry) → route (cheladb-routes) → CORS → auth → mint identity → proxy
```

Fixed pipeline order, in code. Nothing is configurable per route except the `Policy` fields.

## Crates and layout

```
crates/cheladb-routes/        NEW — endpoint declarations + matcher (no dependencies)
crates/cheladb-auth/          + internal identity module (signer / verifier)
apps/gateway/
  src/main.rs                 env, build Pingora server
  src/lib.rs
  src/registry.rs             Registry, ArcSwap holder, host → Scope
  src/pipeline.rs             request-id, CORS, auth, identity minting (no Pingora types)
  src/upstreams.rs            Service → address from env
  src/proxy.rs                ProxyHttp glue (only file besides main.rs importing Pingora)
  tests/
apps/orchestrator/            router built from cheladb-routes; auth switched to internal identity
apps/cheladb/                 bootstrap also generates the internal signing keypair
```

## `cheladb-routes`

```rust
pub enum Method  { Get, Post, Put, Patch, Delete }  // own type: keeps the crate dependency-free
pub enum Service { Orchestrator }            // later: Rest, Auth, Realtime, Storage
pub enum Scope   { Platform }                // later: Project
pub enum Auth    { Public, Platform }        // later: ProjectKey, ProjectUser
pub enum Cors    { None, Studio }

pub struct Policy { pub auth: Auth, pub cors: Cors }

pub struct Endpoint {
    pub name: &'static str,                  // used in logs
    pub method: Method,
    pub path: &'static str,                  // axum syntax: "/v1/databases/{id}"
    pub scope: Scope,
    pub service: Service,
    pub policy: Policy,
}

pub mod orchestrator {
    pub const HEALTH: Endpoint = /* GET /health, Public, Cors::None */;
    pub const CREATE_DATABASE: Endpoint = /* POST /v1/databases, Platform, Studio */;
    pub const GET_DATABASE: Endpoint = /* GET /v1/databases/{id}, Platform, Studio */;
}

pub const ENDPOINTS: &[Endpoint] = &[/* all of the above */];

pub fn match_endpoint(scope: Scope, method: &str, path: &str) -> Match; // Found(&Endpoint) | MethodNotAllowed(Vec<Method>) | NotFound
```

- Path templates use axum's `{param}` syntax, so the orchestrator passes `CREATE_DATABASE.path` straight to `.route(...)`.
- Matching compares whole segments; `{param}` matches exactly one non-empty segment. Literal segments beat params when both could match.
- A template matching the path under a different method → `MethodNotAllowed` (the gateway returns `405` with `Allow`).
- The crate has no dependencies. It declares its own `Method`; unknown method tokens (e.g. `OPTIONS`, `HEAD`) never match an endpoint.
- The gateway exposes only declared endpoints. An undeclared path is `404` at the gateway even if the upstream has a handler for it.

**Drift test (in the orchestrator):** for every `ENDPOINTS` entry with `service == Orchestrator`, send the method and path (params filled with a dummy UUID) to the orchestrator router built with fakes, and assert the response is neither `404` nor `405`. Auth-protected endpoints return `401`, which still proves the route exists.

## Registry and host scoping

```rust
pub struct Registry { platform_hosts: HashSet<String> }  // later: project hosts → project id
fn scope_for(&self, host: &str) -> Option<Scope>;
```

- Host is taken from `Host` / `:authority`, lowercased, with the port stripped.
- `CHELADB_PLATFORM_HOSTS` (comma-separated, optional). Empty means every host is Platform, which is the dev default: `localhost:8000` just works.
- Non-empty and the host isn't listed → `404 {"error":"unknown host"}`.
- Held in `ArcSwap<Registry>`. Each request loads it once. This phase builds it from env at startup and never swaps it. The swap exists so a metadata-backed source (hosted mode, `*.data.<domain>` project hosts, testable locally as `acme.localhost`) plugs in without touching the request path.

## Pipeline

All steps are plain functions over request parts plus a per-request context; no Pingora types.

1. **Request id.** Reuse the incoming `X-Request-Id` if it is ≤ 128 chars of `[A-Za-z0-9._-]`, else generate a UUIDv4. Sent upstream and returned on the response.
2. **Scope.** From the Registry (see above).
3. **Route.** `match_endpoint` → `404 {"error":"no route"}` / `405`.
4. **CORS** (when `Cors::Studio`):
   - Allowed origins from `CHELADB_STUDIO_ORIGINS` (default `http://localhost:3000`, the Studio dev server).
   - Preflight (`OPTIONS` + `Origin` + `Access-Control-Request-Method`) matching a declared endpoint's path → `204` with CORS headers, not proxied, no auth. The preflight is matched against the method in `Access-Control-Request-Method`, not `OPTIONS`.
   - Other requests: echo an allowed `Origin` in `Access-Control-Allow-Origin` with `Vary: Origin`. Disallowed origins get no CORS headers.
5. **Auth.**
   - Always remove any client-supplied `X-Cheladb-Identity` header first, for every endpoint, so it can't be spoofed.
   - `Auth::Public`: continue, no identity header.
   - `Auth::Platform`: require `Authorization: Bearer <JWT>` and verify it with `cheladb-auth`'s `JwtVerifier` (JWKS, `iss`, `exp`, optional `aud`). Failure → `401`; JWKS unavailable → `503`. On success, mint an internal token (step 6).
   - `Authorization` is removed before proxying on every endpoint. Upstreams only ever see the internal identity.
   - Gateway-generated `401`/`503` responses on `Cors::Studio` endpoints carry the same CORS headers as a proxied response, so browsers can read the error.
6. **Mint identity.** See the next section.
7. **Proxy.** `Service` → address from env (`CHELADB_ORCHESTRATOR_URL`, default `http://127.0.0.1:8081`). Adds `X-Forwarded-For/Proto/Host`. Streams bodies and passes websocket upgrades through. Connect failure → `502`, timeout → `504`.

Gateway liveness: `GET /_gateway/health` → `200 {"status":"ok"}`, answered before step 2.

Errors are JSON `{"error":"..."}`, matching the orchestrator.

## Internal identity token

A compact JWT signed by the gateway with **EdDSA (Ed25519)**. Asymmetric, so services can verify but can't mint.

| Claim | Value |
| --- | --- |
| `iss` | `cheladb-gateway` |
| `aud` | target service (`orchestrator`), so a token can't be replayed to another service |
| `iat` / `exp` | now / now + 60 s |
| `sub` | external subject (Zitadel `sub`) |
| `org_id` | Zitadel org id (from the same claims the orchestrator reads today) |
| `auth` | `platform_jwt` (later `project_key`, `project_user`) |
| `rid` | request id |

- Code lives in `cheladb-auth::internal`: `InternalSigner` (gateway) and `InternalVerifier` (services), with `InternalClaims`. Verification checks signature, `iss`, `aud`, `exp` with 5 s leeway.
- The gateway doesn't resolve tenants. `org_id` → tenant stays in the orchestrator, which owns that data. Moving resolution into the gateway can happen later with the metadata-backed Registry.
- Keys: `CHELADB_INTERNAL_SIGNING_KEY` (path to PKCS#8 PEM, gateway only) and `CHELADB_INTERNAL_VERIFY_KEY` (path to public PEM, services).
- `cheladb auth bootstrap` generates the keypair into `.cheladb/gateway/` if missing (idempotent, never overwrites) and adds both paths to `demo.env`.
- Rotation is out of scope; the design allows a `kid` header plus a set of verify keys later.

## Orchestrator changes

- Build the router from `cheladb_routes::orchestrator::*` paths.
- The `AuthPrincipal` extractor reads `X-Cheladb-Identity` and verifies it with `InternalVerifier` (`aud = "orchestrator"`). `Authorization` is ignored, and `JwtVerifier` / Zitadel config are removed from orchestrator state.
- Soft-binding is unchanged: the claim `org_id` maps to `tenants.zitadel_org_id`.
- Existing tests switch from `MockJwks` tokens to internal tokens minted with an `InternalSigner` test key. The 401/403/202/409 cases keep their meaning. The JWKS-unreachable `503` case moves to the gateway, since the orchestrator no longer fetches JWKS.
- The orchestrator logs the `rid` claim (request id) on accepted creates, so a request can be followed from gateway to orchestrator.
- Consequence: calling the orchestrator directly on `:8081` without the gateway returns `401`. All external traffic goes through `:8000`.

## Dependencies

- `pingora-core`, `pingora-proxy`, `pingora-http`, `pingora-error` 0.9 with default features (no TLS backend compiled in).
- **Build prerequisite:** Pingora always compiles zlib-ng, which needs `cmake` on `PATH` (`sudo apt-get install -y cmake`). Document it in the README.
- `arc-swap`, `http`, `bytes`, `async-trait` in the gateway. `jsonwebtoken` already supports EdDSA. Keypair generation uses `ed25519-dalek` (`pkcs8`, `pem`, `rand_core`) behind a `keygen` feature of `cheladb-auth`, used by the CLI and tests.
- The gateway drops its `cheladb-metadata` dependency.

## Dev workflow

- `just gateway` runs the gateway on `:8000`. `just dev` starts it after the orchestrator.
- `just create-db-demo` and the README curl examples use `http://127.0.0.1:8000`.
- README port table: gateway `8000` (HTTP front door). Remove the `15432` stub row. Note that the orchestrator isn't meant to be called directly.

## Testing

**`cheladb-routes` unit:** segment matching, literal-over-param precedence, `NotFound` vs `MethodNotAllowed`, no duplicate (scope, method, path) in `ENDPOINTS`.

**`cheladb-auth::internal` unit:** round trip; wrong `aud`, expired, wrong key, and tampered token are rejected.

**Gateway pipeline unit (no Pingora):** request-id accept/replace/generate; scope with empty and non-empty platform hosts; CORS preflight/allowed/disallowed/`Vary`; auth public/missing/invalid/valid (`MockJwks`); client-supplied `X-Cheladb-Identity` removed; `Authorization` removed on platform routes.

**Gateway integration (real gateway, axum mock upstream):** the internal token arrives upstream and verifies with the public key; `404` undeclared path; `405` wrong method; `401` without a token; public `/health` passes without one; preflight isn't proxied; upstream down → `502`.

**Orchestrator:** drift test; existing auth tests ported to internal tokens.

Integration tests spawn the real gateway binary (`CARGO_BIN_EXE_gateway`) on a pre-picked free port, because Pingora's `Server::run_forever` never returns. The gateway sets Pingora's grace period to 0 and its graceful-shutdown timeout to 5 s, so it exits promptly on SIGTERM (checked with a probe: about 100 ms).

## Success criteria

1. `cargo test --workspace` passes.
2. `just dev` then `just create-db-demo` returns `202` through `:8000`, and the orchestrator log shows the request id the gateway assigned.
3. Adding an endpoint to `cheladb-routes` without an orchestrator handler fails the drift test.
4. Only `proxy.rs` and `main.rs` in the gateway import Pingora.

## Later (not this spec)

`Auth::ProjectKey` / `Auth::ProjectUser` and project data services; `Scope::Project` with a metadata-backed Registry refreshed over NATS; tenant resolution in the gateway; rate limits; usage events; signing-key rotation; gateway TLS; explicit extra-upstream config for self-hosters; `apps/pgproxy`.
