# Internal control-plane listener and Neon's proxy (roadmap step 3)

**Date:** 2026-09-28
**Status:** Approved. Amended after the Zitadel v4 merge (ports, `CHELADB_MODE`), then for Valkey: the proxy's cache on with invalidation, and query cancel.
**Roadmap:** step 3 of `docs/roadmap/2026-09-28-neon-and-data-api.md`.

## Goal

Neon's services call our control plane machine-to-machine. This step gives them
something real to call, and puts Neon's proxy in front of computes:

- an internal listener in the orchestrator that serves the proxy, `compute_ctl` and the
  storage controller, each with its own credential
- Neon's proxy in Compose, for Postgres wire **and** its serverless side (WebSocket and
  SQL-over-HTTP, password auth)
- the `storcon-hook` stand-in removed; the storage controller calls the orchestrator
- a hand-made compute, registered in metadata and run with `docker run`, to prove the path

Provisioning still uses plain `postgres:17`. Moving it to Neon is step 4.

**Done when** `just neon-e2e` passes: `psql` through `<ep>.localtest.me:4432`, SQL-over-HTTP
and the `@neondatabase/serverless` driver through `<ep>.localtest.me:4444`, and a
password change pushed to the running compute by a storage-controller hook.

## Decisions

| Question | Decision |
|---|---|
| Where the listener runs | **A second listener in the orchestrator**, `CHELADB_INTERNAL_ADDR` (default `127.0.0.1:8083`). It shares the metadata pool, and step 4's reset password can reconfigure computes directly. It never goes through the gateway. |
| Bind address | **Loopback.** A probe on this Docker Desktop (WSL2) host showed containers reach a host listener on `127.0.0.1` through `host.docker.internal`. Compose still sets `extra_hosts: host.docker.internal:host-gateway`, but on plain Linux Docker that address is the bridge gateway, which a loopback-only listener doesn't answer; there the listener must bind the bridge address instead (see the README). |
| Where endpoint records live | **Metadata tables now** (`neon_endpoints`, `neon_roles`, `neon_databases`). Step 4 adds `database_id` and provisions into the same tables. |
| Reconfigure | **Built in full now.** The host can't route to container IPs on Docker Desktop, so a compute publishes `compute_ctl`'s port 3080 on a random loopback port, and the orchestrator finds it with `docker inspect` (bollard). |
| Compute address for the proxy | The container's **IP on `cheladb_default`**, from the same inspect. |
| Proxy serverless side | **On, password mode** (`--wss`). The roadmap had kept it off; reversed so Neon's serverless driver works against Cheladb. |
| Proxy JWT mode (`--is-auth-broker`) | **Off until step 6.** At the pinned ref it replaces password auth on HTTP rather than adding to it, and it needs the `/jwks` route, `local_proxy` on the compute and `pg_session_jwt`. It depends on the data API's end-user identity decision. |
| Proxy REST broker (`--is-rest-broker`) | **Off for good.** Its engine, `subzero-core`, is a stub in the public source and the real crate isn't published anywhere. The data API stays `cheladb-rest` behind Pingora; `proxy/src/serverless/rest.rs` is a reference for it. |
| `/endpoints/{id}/jwks` route | **Not served.** Only the JWT mode above calls it (`proxy/src/serverless/backend.rs`, `authenticate_with_jwt`). |
| Proxy role-secret cache | **On, cleared through Valkey** (decided 2026-09-28: turning it on later is easy to forget). Flags: `--project-info-cache=size=10000,ttl=1m,max_roles=10,gc_interval=60m` plus `--redis-auth-type=plain --redis-plain=redis://valkey:6379`. On a password change, the control plane publishes `{"topic":"/password_updated","data":"{\"project_id\":\"<ep>\",\"role_name\":\"<role>\"}"}` on `neondb-proxy-ws-updates`. The proxy drops that role at once, and again 20 s later in case a lookup was in flight (`proxy/src/redis/notifications.rs`). `data` is a JSON string, and `project_id` is the endpoint id we return from access control. `PUBLISH` returns how many subscribers got the message; 0 means no proxy is listening, and the caller treats that as a failure. The 1-minute TTL (Neon's default is 4 minutes) only matters if a message is lost. |
| Password-change order | **Metadata, then computes, then the proxy.** Write the new verifier, reconfigure the running computes, then publish `/password_updated`. Publishing first would let the proxy accept the new password before the compute knows it. |
| Valkey | **Valkey 9.1.2** (BSD-3-Clause), not Redis: Redis 8 is RSALv2/SSPLv1/AGPLv3, which AGPL-averse self-hosters would have to clear. It goes in the base Compose profile, not `neon`, because later uses (see the roadmap's Valkey note) aren't Neon-specific. It listens on `127.0.0.1:6379`, with no auth in local dev; platform mode needs auth and TLS. It also makes **query cancel** work through the proxy: without Redis, the proxy fails every cancel request ("cancellation handler is not available"). |
| Other proxy caches | **Defaults.** The wake-compute cache (`size=4000,idle_ttl=4m`) holds only the compute address and is dropped when a connect fails (`proxy/connect_compute.rs`). The PBKDF2 cache is checked against the current secret's stored key, so it never accepts an old password (`proxy/src/scram/cache.rs`). |
| `CHELADB_MODE` | **No effect on this step.** The listener, the proxy and password auth behave the same in self-hosted and platform modes. |
| Ports | The listener takes **8083** (auth has 8084). The proxy publishes **4432** and **4444**; Valkey **6379**. All on `127.0.0.1` only; `just neon-test` already fails on any published port that isn't. The listener runs inside the orchestrator, so `just stop` and `just cleanup` need no new port. |

## Components

### Orchestrator: internal listener

A separate axum router, bound next to the public one in `main.rs`. It starts whether or
not the storage stack is up; handlers answer 503 while it's down.

| Route | Caller | Credential |
|---|---|---|
| `GET /proxy/api/v1/get_endpoint_access_control?endpointish=&role=` | proxy | `CHELADB_PROXY_API_TOKEN` |
| `GET /proxy/api/v1/wake_compute?endpointish=` | proxy | `CHELADB_PROXY_API_TOKEN` |
| `GET /compute/api/v2/computes/{id}/spec` | `compute_ctl` | per-endpoint token; only its SHA-256 is stored |
| `PUT /storcon/notify-attach` | storage controller | `CHELADB_STORCON_HOOK_TOKEN` |
| `PUT /storcon/notify-safekeepers` | storage controller | `CHELADB_STORCON_HOOK_TOKEN` |
| `GET /health` | checks | none |

- Tokens are checked with a constant-time compare (ported from the old `auth.rs`) in
  middleware, before the body is parsed. Each token opens only its own routes.
- The handlers depend on traits (metadata queries, `ComputeLocator`, the storage
  controller, the reconfigure HTTP call) so tests can use fakes.

### `crates/cheladb-neon` additions

- `proxy_api`: the `get_endpoint_access_control` and `wake_compute` response bodies and
  the `ControlPlaneErrorMessage` error body (`reason`, optional `retry_ms`), as pure
  functions. Ported from the old `proxy_api.rs`.
- `compute`:
  - `container_name(ep) = "cheladb-compute-<ep>"`, the port constants (Postgres 5432,
    `compute_ctl` 3080) and `compute_ctl_args` (ported from the old `backend.rs`).
  - `ComputeLocator` trait: `address(ep) -> Option<IpAddr>` (IP on the compute network,
    for the proxy) and `control_addr(ep) -> Option<SocketAddr>` (`127.0.0.1:<published
    3080>`, for the host). Both `None` when the container is missing or not running.
  - `DockerLocator`: the bollard implementation, parsing `docker inspect`.
- `proxy_cache`: `password_updated_message(endpoint, role)` (pure), and `ProxyCache::password_updated`, which publishes it and returns the subscriber count. It uses the `redis` crate (1.7.1, the latest; `tokio-comp`), which speaks Valkey's protocol.
- `reconfigure`: builds `{spec, compute_ctl_config}` and `POST`s it to
  `http://<control_addr>/configure` with a `ComputeSigner` token for that compute.

bollard is added to the crate from the workspace pin, **0.21.1** (the latest release since
the dependency refresh; the worker uses it too). The old repo's `docker.rs` is already
written against 0.21.

### Metadata migration

```sql
CREATE TABLE neon_endpoints (
    id                   text PRIMARY KEY,           -- 'ep-' + 12 hex
    tenant_id            text NOT NULL,              -- 32 hex
    timeline_id          text NOT NULL,              -- 32 hex
    compute_token_sha256 text NOT NULL,
    created_at           timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX ON neon_endpoints (tenant_id);
CREATE INDEX ON neon_endpoints (timeline_id);

CREATE TABLE neon_roles (
    endpoint_id    text NOT NULL REFERENCES neon_endpoints (id) ON DELETE CASCADE,
    name           text NOT NULL,
    scram_verifier text NOT NULL,
    PRIMARY KEY (endpoint_id, name)
);

CREATE TABLE neon_databases (
    endpoint_id text NOT NULL REFERENCES neon_endpoints (id) ON DELETE CASCADE,
    name        text NOT NULL,
    owner       text NOT NULL,
    PRIMARY KEY (endpoint_id, name),
    FOREIGN KEY (endpoint_id, owner) REFERENCES neon_roles (endpoint_id, name)
);
```

No link to `databases` yet; step 4 adds `database_id`. Roles are keyed by endpoint, not
branch: each branch gets one endpoint, so step 5 can copy rows when it branches.

### Keys and config

- `cheladb dev bootstrap` also creates the compute-JWT key pair in `.cheladb/neon/`:
  `compute-jwt.pem` (PKCS#8 Ed25519) and `compute-jwt.pub` (the base64url `x`). Like the
  gateway pair, it's created only when neither file exists, never overwritten, and
  refused when only one exists.
- New orchestrator settings, all with local-dev defaults:

  | Variable | Default |
  |---|---|
  | `CHELADB_INTERNAL_ADDR` | `127.0.0.1:8083` |
  | `CHELADB_PROXY_API_TOKEN` | `cheladb-dev-proxy-api` |
  | `CHELADB_STORCON_HOOK_TOKEN` | `cheladb-dev-storcon-hook` (already read by Compose) |
  | `CHELADB_STORCON_URL` | `http://127.0.0.1:1234` |
  | `CHELADB_COMPUTE_JWT_DIR` | `.cheladb/neon` |
  | `CHELADB_COMPUTE_NETWORK` | `cheladb_default` |
  | `CHELADB_VALKEY_URL` | `redis://127.0.0.1:6379` (read by `cheladb neon password-updated` in this step; by the orchestrator from step 4) |

### Compose

- **`valkey`** (base profile): `valkey/valkey:9.1.2-alpine`, pinned by digest, on `127.0.0.1:6379`, with a `valkey-cli ping` healthcheck and a volume.

**`neon` profile:**


- **Removed:** `storcon-hook`, and the storage controller's `depends_on` on it.
- **Storage controller:** `--control-plane-url=http://host.docker.internal:8083/storcon`,
  plus `extra_hosts`.
- **`proxy-tls`** (ported): a one-shot that writes a local CA and a `*.localtest.me`
  certificate to the `proxy-tls` volume once, and keeps them afterwards.
- **`proxy`** (ported, extended):
  - `--auth-backend=control-plane`,
    `--auth-endpoint=http://host.docker.internal:8083/proxy/api/v1`,
    `NEON_PROXY_TO_CONTROLPLANE_TOKEN=${CHELADB_PROXY_API_TOKEN:-cheladb-dev-proxy-api}`
  - `--proxy=0.0.0.0:4432` and `--wss=0.0.0.0:4444`, both with the proxy-tls certificate
  - `--project-info-cache=size=10000,ttl=1m,max_roles=10,gc_interval=60m`, `--redis-auth-type=plain`, `--redis-plain=redis://valkey:6379` (see Decisions); `depends_on` Valkey being healthy, because the proxy exits if it can't reach Redis at startup
  - ports `127.0.0.1:4432:4432` and `127.0.0.1:4444:4444`; `extra_hosts`
  - on `cheladb_default`, so it reaches compute IPs
- `neon_services` / `neon_daemons` in the justfile swap `storcon-hook` for `proxy-tls` and
  `proxy`.

## Behaviour

### `get_endpoint_access_control`

- Endpoint and role found: `200 {role_secret, project_id: <endpoint id>, account_id: null,
  allowed_ips: null, allowed_vpc_endpoint_ids: null, block_public_connections: null,
  block_vpc_connections: null}`.
- Unknown role: the same 200 with an empty `role_secret`. The proxy then fails exactly as
  for a wrong password, so role names can't be probed.
- Unknown endpoint: 404, reason `ENDPOINT_NOT_FOUND`.
- Metadata error: 500, reason `UNKNOWN`, `retry_ms: 1000`.

### `wake_compute`

- Container running: `200 {address: "<ip>:5432", server_name: null, aux: {endpoint_id,
  project_id, branch_id, compute_id, cold_start_info: "warm"}}`. `project_id` and
  `branch_id` carry the endpoint id and timeline id until step 4 has real ones.
- Endpoint unknown: 404 `ENDPOINT_NOT_FOUND`.
- Container missing or not running: 503, `retry_ms: 1000`, so the proxy retries while
  the compute boots. Step 7's scale to zero starts computes here.

### `GET /compute/api/v2/computes/{id}/spec`

- The bearer token's SHA-256 must equal `compute_token_sha256`. Unknown id and bad token
  both get 401.
- The body is `spec::config_response` with the storage controller's
  `pageserver_connstring(tenant)` and `active_safekeepers()`, the endpoint's roles and
  databases, and `ComputeSigner::jwks()`.
- Storage controller unreachable or erroring: 503; `compute_ctl` retries.

### `notify-attach` and `notify-safekeepers`

- `notify-attach` maps `tenant_id` to its endpoints; `notify-safekeepers` maps
  `timeline_id`.
- Each endpoint is reconfigured: `control_addr` is `None` → skipped (it pulls the current
  spec when it boots); otherwise the spec is built and `POST`ed to `/configure`.
- 200 when every endpoint was reconfigured or skipped, including when there are none.
  That's the case on every tenant create, and answering at once fixes the ~15 s stall
  step 1 saw.
- 503 when a lookup or any reconfigure failed; the storage controller retries 503. Never
  400/401/403 for our own failures, since the storage controller treats those as fatal.

### Hand-made compute: `scripts/neon-compute-demo.sh` (`just neon-compute-demo`)

Needs `just neon-up` and a running orchestrator.

1. Create a tenant and a `main` timeline through the storage controller at
   `127.0.0.1:1234`.
2. Generate an endpoint id, a password and a compute token. Insert the endpoint (token
   SHA-256 only), the `postgres` role and the `postgres` database into metadata. The
   verifier comes from a new `cheladb neon verifier` subcommand (reads the password on
   stdin, prints `scram::verifier`).
3. `docker run -d` the compute image as `cheladb-compute-<ep>` on `cheladb_default` with
   `-p 127.0.0.1::3080`, `NEON_CONTROL_PLANE_TOKEN`, and `compute_ctl_args` with
   `--control-plane-uri=http://host.docker.internal:8083` and `extra_hosts`.
4. Write the endpoint id and password to `.cheladb/neon/demo-compute.env` and print the
   `psql` and serverless connection strings.

`just neon-compute-demo --down` removes the container, the metadata rows (cascade) and
the tenant.

## Testing

**Unit tests (`cargo test`):**

- `cheladb-neon`:
  - proxy bodies carry the fields and error shape the pinned proxy parses
    (`control_plane/messages.rs`)
  - the `/password_updated` message parses as the pinned proxy's `Notification` (topic, and
    `data` as a JSON string); a live, ignored test publishes to Valkey and a subscriber gets it
  - `DockerLocator` inspect parsing: IP on the named network only, the published 3080
    port, and `None` for a stopped or missing container
  - `compute_ctl_args` and the reconfigure request (URL, bearer, body)
- `services/orchestrator/tests/internal.rs`, with fakes for metadata, `ComputeLocator`, the
  storage controller and a compute (in-process axum):
  - every route refuses a missing or wrong token; one caller's token doesn't open another
    caller's routes
  - access control: found, unknown role (empty secret), unknown endpoint (404)
  - `wake_compute`: running (address), not running (503 + `retry_ms`)
  - spec: correct token (body), bad token and unknown id (401), storage controller down
    (503)
  - hooks: no endpoints (200, no compute contacted), compute not running (200, skipped),
    reconfigure fails (503), reconfigure succeeds (200, spec `POST`ed with a valid token)
- `crates/cheladb-metadata/tests/live_db.rs`: the new queries, including cascade delete.
- `tools/cli`: the compute-JWT key pair is created once, reused, and refused when half
  of it exists.

**Offline Compose checks (`just neon-test`):** `storcon-hook` is gone; the storage
controller's control-plane URL and `extra_hosts`; `proxy-tls` and `proxy` exist, with
both ports bound to `127.0.0.1`; the proxy's cache flags and its Valkey URL; `valkey` in the base profile on `127.0.0.1:6379`.

**Live (`just neon-e2e`, needs `just neon-up` and the orchestrator):**

1. `neon-compute-demo` creates the endpoint, then waits until the compute answers
   through the proxy.
2. `psql "host=<ep>.localtest.me port=4432 sslmode=verify-full sslrootcert=<ca>"` creates
   a table and inserts rows.
3. `curl POST https://<ep>.localtest.me:4444/sql` with `Neon-Connection-String` reads
   them back.
4. A Node script using `@neondatabase/serverless` over WebSocket reads them back.
5. A wrong password is refused.
6. Query cancel: `SELECT pg_sleep(30)` through the proxy, `psql` gets SIGINT after 2 s, and
   the statement ends with "canceling statement due to user request" within a few seconds.
7. Password change, in the order above:
   - replace the role's verifier in metadata; the old password **still** connects, which
     proves the cache is on;
   - `PUT /storcon/notify-attach` with the storage-controller token;
   - `cheladb neon password-updated` reports at least one subscriber;
   - then, **with no wait**, the new password connects and the old one is refused, both on
     4432 and on `/sql`.
8. `neon-compute-demo --down`; the tenant is gone from the storage controller.

`neon-smoke` keeps its "no pending compute notification" check, now answered by the
orchestrator; it needs the orchestrator running.

## Out of scope

- The Docker `ComputeBackend` (create, delete, lifecycle) and provisioning: step 4.
- Proxy JWT mode, `/jwks`, `local_proxy`: step 6.
- Step 4: reset password must follow the password-change order above. It uses
  `ProxyCache::password_updated`, and fails the reset (or retries) when no proxy received
  the message.
- Proxy TLS for anything but `*.localtest.me`, rate-limit tuning: later. (The role-secret cache and its Valkey invalidation are in scope; see Decisions.)

## Risks

- **Proxy flags at the pinned ref.** The old repo ran the proxy without `--wss`. If the
  serverless side needs more flags (for example its own TLS or `--http` settings), the
  plan adds them; the e2e steps 3 and 4 catch it.
- **The driver's hostname rules.** `@neondatabase/serverless` builds its WebSocket URL
  from the connection string's host. If it insists on port 443, the Node check sets the
  driver's `wsProxy` option to `<ep>.localtest.me:4444/v2`; the check still proves the
  proxy's WebSocket path.
- **CA trust for Node.** The Node check passes the proxy CA with `NODE_EXTRA_CA_CERTS`.
- **Reaching the internal listener from a compute.** On Docker Desktop (WSL2),
  `host.docker.internal` reaches the host's 127.0.0.1 (verified live in Task 9). On native
  Linux Docker, `host-gateway` is the bridge IP, so the internal listener must bind an
  address the bridge can reach (set `CHELADB_INTERNAL_ADDR`).
- **What a refusal looks like (verified live in Task 10).** A wrong password gets
  `ERROR: password authentication failed for user 'postgres'` on 4432, and HTTP 400 with
  `{"message":"password authentication failed for user 'postgres'",…}` on `/sql`. An
  unknown endpoint answers the same way (its role secret is empty), so `just neon-e2e`
  asserts that message rather than just "not 200", which a downed proxy would also pass.
- **`localtest.me` resolves to `::1` too.** The proxy publishes on 127.0.0.1 only, so psql's
  first attempt is refused before it falls back. The e2e sets `hostaddr=127.0.0.1` (the
  hostname still drives SNI and certificate checks).
- **The driver needed no workaround beyond the plan.** `@neondatabase/serverless` 1.1.0
  works with `wsProxy = host:4444/v2` and `fetchEndpoint = https://host:4444/sql`, the proxy
  CA via `NODE_EXTRA_CA_CERTS`, and Node 24's built-in `WebSocket`; no proxy flags were added.
- **The cache check depends on timing.** The step that proves the proxy still serves the
  cached secret must run within the cache TTL (1 minute) of the first connect. So
  `just neon-e2e` builds `cheladb` and computes the new password's verifier before the
  compute is created; nothing slow sits between the first connect and that check, and a
  full run takes about 10 s after the build.
