# Switch provisioning to Neon (roadmap step 4)

**Date:** 2026-09-29
**Status:** Approved 2026-09-29. Plan: `docs/superpowers/plans/2026-09-29-neon-provisioning.md`.
**Roadmap:** `docs/roadmap/2026-09-28-neon-and-data-api.md`, step 4.
**Branch:** `neon-provisioning`.

## Goal

Creating a database gives a Neon-backed database, reachable through Neon's proxy at `<ep>.<proxy domain>:4432` (Postgres wire) and `:4444` (SQL over HTTP and WebSocket). The plain `postgres:17` provisioning and the `cheladb_platform` role go away.

**Done when:** create, connect (psql and the serverless driver), reveal, reset (the new password works at once, the old one is refused), and delete all work end to end through the platform API and Studio, and nothing in the code still refers to `cheladb_platform` or `postgres:17` tenant containers.

## Decisions

| Question | Decision | Why |
|---|---|---|
| Databases already on `postgres:17` | **Dropped, not migrated.** The old code is deleted. A migration turns any leftover row without an endpoint into a `deleted` tombstone. | They're obsolete. On 2026-09-29 this machine had none (no managed containers or volumes, an empty `databases` table); the migration covers the other computer's metadata database. |
| `CHELADB_DB_OWNER_SUPERUSER` | **Kept, self-hosted only.** Still off by default. When on and `CHELADB_MODE` is self-hosted, provisioning runs `ALTER ROLE postgres SUPERUSER`. In platform mode it's ignored, with a warning at startup. | The owner is a `neon_superuser` member already (CREATEDB, CREATEROLE, BYPASSRLS, REPLICATION); true superuser stays a local-dev convenience. |
| How the platform runs superuser SQL in a compute | **`ComputeBackend::admin_sql`: `docker exec` psql as `cloud_admin` over the compute's own localhost.** A later Kubernetes backend would use pods/exec. | No superuser credential exists anywhere and nothing superuser is reachable over the network. See "Findings" for why `compute_ctl` can't do it. |
| Who runs the lifecycle | **The worker**: provision, delete, reset, reconcile. The orchestrator's internal listener stays as step 3 built it (spec, proxy API, hooks). | The worker already consumes those jobs and holds the Docker socket. |
| Data API roles | Created in step 4 through `admin_sql`, **not** through the compute spec. | Spec roles are elevated (see "Findings"). |

### Findings: what `compute_ctl` can and can't do (Neon at `NEON_REF` `fa504217`)

This settles the roadmap's condition for dropping `cheladb_platform` ("Resume here", item 7).

- **Spec roles are elevated.** `compute_tools/src/spec_apply.rs` creates every spec role as `CREATE ROLE <name> INHERIT CREATEROLE CREATEDB BYPASSRLS REPLICATION IN ROLE neon_superuser … LOGIN PASSWORD …`. The only exception is a role named in `local_proxy_config.jwks[].role_names`, which gets `CREATE ROLE <name> … LOGIN`. `LOGIN` is always appended (`pg_helpers.rs`, `to_pg_options`). So `anon`, `authenticated` and `authenticator` can't go in the spec: they would bypass row-level security, or at least be able to log in.
- **No general SQL hook.** The spec has no field for extra SQL. `/grants` (`SetRoleGrantsRequest`: schema privileges only) and `/extensions` are on compute_ctl's **internal** router (localhost-only). The external router (`/configure`, `/status`, `/terminate`, …) has nothing that runs SQL.
- **A superuser is needed in step 6.** `CREATE EVENT TRIGGER` needs superuser in Postgres 17, and the users table's protection is an event trigger. `neon_superuser` isn't a superuser.
- **The exec path works on our image.** `ghcr.io/chelabase/compute-node-v17:0.1.0` ships `/usr/local/bin/psql` and runs as uid 1000 (`postgres`). `compute_ctl` itself connects as `postgresql://cloud_admin@localhost:5432/postgres` without a password, so `cloud_admin` is trusted from inside the container. The first implementation task confirms this against a live compute before anything is built on it.
- Catalog changes live in Neon storage, so the bootstrap SQL runs once per provision, not on every compute start.

## Architecture

```
Studio ──► gateway ──► orchestrator (platform API) ──NATS──► worker
                                                              │
                        ┌─────────────────────────────────────┼──────────────────────┐
                        ▼                                     ▼                      ▼
              storage controller                     ComputeBackend (Docker)     Valkey
              (tenant, timeline)                     create / delete / status    /password_updated
                                                     admin_sql (docker exec)
                        │                                     │
                        ▼                                     ▼
                   pageserver, safekeepers ◄────────── compute container ──spec──► orchestrator
                                                              ▲                   internal :8083
                                            proxy :4432/:4444 ┘ (wake_compute, role secret)
```

### `ComputeBackend` (in `crates/cheladb-neon`)

Ported from the old repo (`services/controlplane/src/backend.rs`, `docker.rs`) and placed next to step 3's `ComputeLocator` in `compute.rs` (or a new `backend.rs`). `DockerBackend` implements both traits.

| Method | Behaviour |
|---|---|
| `create(endpoint_id, compute_token)` | Creates and starts `cheladb-compute-<ep>` with the compute image (`scripts/neon-images.sh`'s `COMPUTE_NODE_IMAGE`, passed in as config) and `compute_ctl_args(endpoint_id, control_plane_uri)`. The token is passed in `NEON_CONTROL_PLANE_TOKEN` (env, never argv). The container is on the compute network, has `host.docker.internal:host-gateway`, the endpoint label, `unless-stopped`, 1 CPU / 2 GiB, and compute_ctl's 3080 published on `127.0.0.1` only. A leftover container of the same name is replaced. |
| `delete(endpoint_id)` | Force-removes the container. A missing one isn't an error. |
| `status(endpoint_id)` | `None` if missing, else the container state. |
| `list()` | Endpoint ids of every container with our endpoint label (for reconcile). |
| `address`, `control_addr` | Unchanged from step 3's `ComputeLocator`. |
| `admin_sql(endpoint_id, database, sql)` | `docker exec -i` as uid `postgres`: `psql -X -q -v ON_ERROR_STOP=1 -h localhost -U cloud_admin -d <database>`, with the SQL on stdin. It returns stdout, or an error carrying psql's exit code and stderr. `database` is validated as a plain identifier. |

An in-memory fake of the trait lives behind `#[cfg(any(test, feature = "fake"))]` for the worker's tests.

### Metadata (one migration)

- `databases`: add `endpoint_id text REFERENCES neon_endpoints (id)`; drop `container_id`, `host`, `port`, `platform_password_enc`. `database_name`, `owner_role` and `owner_password_enc` stay.
- `neon_endpoints`: add `needs_reconfigure boolean NOT NULL DEFAULT false`.
- Leftover rows: `UPDATE databases SET status = 'deleted', deleted_at = now() WHERE endpoint_id IS NULL AND status <> 'deleted'` runs before the columns are dropped.
- `ReadyDatabase`, `DatabaseRecord` and the metadata client follow: `set_ready` takes the endpoint id. New calls: create an endpoint with its role and database; link it; replace its verifier and sealed password in one transaction; set and clear `needs_reconfigure`; replace the compute token hash; delete an endpoint; list endpoints needing reconfigure.

### Ids and secrets

- Tenant and timeline: `NeonId` (32 hex). Endpoint: `ep-` + 12 hex. Owner password: `credentials::generate_password` (alphanumeric, unchanged).
- Compute token: 32 random bytes as hex. Only its SHA-256 goes into `neon_endpoints`; the plaintext exists only in the container's env.
- The owner's SCRAM verifier comes from `cheladb_neon::scram` and goes into `neon_roles`. The sealed password stays in `databases.owner_password_enc` (purpose `Owner`, AAD the database id), so reveal is unchanged.

## Data flow

### Provision (worker, NATS job)

Each step records what it created before the next one starts, so every failure can be cleaned up:

1. Skip checks and the `provisioning` claim, as today.
2. Generate the ids, password, verifier and token. In one transaction: insert `neon_endpoints`, `neon_roles(postgres)`, `neon_databases(<database_name>, owner postgres)`, and set `databases.endpoint_id`.
3. Storage controller: `create_tenant`, then `create_timeline` (Postgres 17). Then re-check that the endpoint row still exists: a delete that finished meanwhile found no tenant to delete. If it's gone, run the teardown and mark the row `deleted` (not `failed`).
4. `ComputeBackend::create`.
5. Wait until ready: `admin_sql(…, "postgres", "SELECT 1")` every 500 ms, for up to 60 s.
6. Bootstrap through `admin_sql` on the new database: one idempotent script.
   - `anon` and `authenticated`: `NOLOGIN`.
   - `authenticator`: `NOLOGIN NOINHERIT`, granted `anon` and `authenticated` (step 6 gives it a login).
   - With the self-hosted owner-superuser switch on: `ALTER ROLE postgres SUPERUSER`.
   - Each `CREATE ROLE` is guarded by a `pg_roles` check in a `DO` block. The script contains no secrets.
7. Re-check for a delete that landed meanwhile (kept from today), then `set_ready` with the sealed password.

**On failure at any step:** remove the compute, delete the tenant (a 404 is fine), delete the endpoint row (roles and databases cascade), then `set_failed` with the step named in the error, for example `creating timeline: …`. A teardown failure is logged, and reconcile finishes it (see below). A delete found at step 7 runs the same teardown and marks the row `deleted`.

### Reveal (orchestrator)

The sealed password is opened as today. The response carries:
- `uri`: `postgresql://postgres:<pw>@<ep>.<domain>:4432/<database_name>?sslmode=verify-full`;
- `http_url`: `https://<ep>.<domain>:4444/sql`;
- `host`, `port`, `database`, `user`.

`<domain>` is `CHELADB_PROXY_DOMAIN` (default `localtest.me`). A database that isn't ready, or has no endpoint, answers as today for a database without credentials.

**Studio** shows the connection string and the HTTP URL, with a short note: in local dev, pass the CA at `.cheladb/neon/proxy-ca.crt` (`sslrootcert=`), and on hosts where `localtest.me` resolves to `::1` first, add `hostaddr=127.0.0.1` (findings from step 3).

### Reset (worker, NATS request/reply), in step 3's order

1. In one transaction: the new sealed password, the new verifier in `neon_roles`, and `needs_reconfigure = true`.
2. If the compute is running (`control_addr`), push the current spec with `reconfigure::push`. A compute that isn't running pulls the current spec when it boots.
3. `ProxyCache::password_updated(ep, "postgres")`. Zero receivers is an error.
4. Clear `needs_reconfigure` and reply with success.

If step 2 or 3 fails, the reply is an error ("the password was changed but not yet applied; it will be retried"). Metadata then holds the new password, so reveal shows the one that will work, and reconcile retries steps 2–4. Reconfigure plus publish must fit inside `RESET_DEADLINE` (8 s), which in turn must stay under the orchestrator's `RESET_TIMEOUT` (15 s). The plan measures one live reset and raises both together if needed.

Steps 2–4 run under a per-endpoint lock (a Postgres advisory lock, so it holds across workers), taken after step 1 commits. Reconcile's retry takes it too, so an older push can't land after a newer one.

**The worker needs**, for reset: the compute JWT signer (`CHELADB_COMPUTE_JWT_DIR`, the same key files the internal listener loads), the storage-controller client (for the spec's pageserver and safekeepers) and a `ProxyCache`. Building the spec is currently in `services/orchestrator/src/internal/spec.rs`. It moves into `cheladb-neon` (a function over a small metadata/storcon port), so the worker and the listener build identical specs.

### Delete (worker, NATS job)

The tombstone flow is unchanged. The worker:
1. removes the compute;
2. deletes the tenant (`StorconClient::delete_tenant` already polls 202 and treats 404 as done);
3. deletes the endpoint row;
4. marks the database `deleted`.

A row with no endpoint (a failed provision after cleanup) goes straight to `deleted`.

## Reconcile (worker, every 30 s)

| Observation | Action |
|---|---|
| Ready database, compute container missing | New compute token (hash replaced first), then `create`. |
| Compute container exists but exited | Nothing: Docker's `unless-stopped` owns restarts. |
| Endpoint with `needs_reconfigure` | Reset steps 2–4. |
| Container with our endpoint label but no endpoint row, or its database is deleted | `delete` the container. |
| `provisioning` for longer than `STALE_PROVISION_SECS` | Provision's teardown, then `failed` (as today). |
| `deleting` | Retry delete (as today). |
| Endpoint row whose database is `failed` or `deleted` (a teardown that failed) | Retry the teardown. |
| Tenant in the storage controller with no endpoint row | **Log only, never delete.** Deleting storage on a guess can't be undone. |

`services/worker/src/reconcile.rs` keeps its shape: a pure `plan(observed) -> Vec<Action>`, with the observation widened to endpoints and compute containers.

## Stack and tooling

- **Compose:** the `profiles: ["neon"]` lines go, so `just deps-up` and `just dev` start the storage stack, the proxy and Valkey. `just neon-up` and `just neon-down` are removed; the justfile and README point at `deps-up`.
- **Worker configuration** (with local-dev defaults, the same style as `InternalConfig`):
  - `CHELADB_STORCON_URL`;
  - `CHELADB_COMPUTE_JWT_DIR`;
  - `CHELADB_COMPUTE_NETWORK`;
  - `CHELADB_CONTROL_PLANE_URI`: the internal listener as computes see it, default `http://host.docker.internal:8083`, checked like the demo script's `check_internal_addr`;
  - `CHELADB_VALKEY_URL`;
  - `CHELADB_COMPUTE_IMAGE`: from `scripts/neon-images.sh` via `just dev`;
  - `CHELADB_PROXY_DOMAIN`, read by the orchestrator too.
- **Removed:** `scripts/neon-compute-demo.sh` and its recipe, `services/worker/src/roles.rs`, `docker_ops.rs` and `names.rs`'s volume helpers (the name sanitizer stays), `POSTGRES_IMAGE`, and every `cheladb_platform` reference.
- **`just neon-e2e`**, rewritten to go through the API. It creates a database with `cheladb dev token` and waits for `ready`. Then:
  - psql through `:4432`, and SQL over HTTP and the serverless driver through `:4444`;
  - query cancel;
  - reset, then: the old password is refused and the new one works within the cache TTL;
  - delete, then: the container is gone, the tenant is gone (404), and there's no endpoint row.
- **`just sweep-dbs`**, rewritten: remove every labelled compute container, delete every tenant that belongs to an endpoint row, then clear `neon_endpoints` and `databases`. The roadmap's "never run `just sweep-dbs` / touch demo1–3" rule is dropped.
- **Docs:** the README's local-dev section, and the roadmap's "Where we are", "Resume here" and step 4 progress.

## Error handling

- Storage-controller and Docker errors carry the step in their context (`anyhow::Context`) and end up in `databases.error`.
- `admin_sql` errors carry psql's stderr. No SQL we send contains a secret, so the errors are safe to log and store.
- The storage controller uses the existing client timeouts. Compute readiness: 60 s. The per-provision cap stays under `STALE_PROVISION_SECS` (600 s).
- The worker refuses to start if the JWT key files are missing, with the same message the internal listener gives (`cheladb dev bootstrap`).

## Testing

- **Unit** (worker, with a fake `ComputeBackend` and fake storage-controller and proxy-cache ports):
  - provision's happy path;
  - a failure at each step, and its cleanup;
  - a delete during provision;
  - reset's order, with each failure leaving `needs_reconfigure` set;
  - delete;
  - every reconcile row in the table above.
- **Unit** (`cheladb-neon`): the container body (args, env, labels, limits, port binding), `admin_sql`'s psql command line and identifier check, and the shared spec builder.
- **Metadata:** the migration against a database holding a legacy row, and the new queries (the existing sqlx test setup).
- **Ignored live tests:**
  - `DockerBackend` against a real compute: create, `admin_sql` `SELECT 1` and a `CREATE ROLE`, then delete;
  - a full provision → reset → delete through the worker's functions against the running stack.
- **End to end:** `just neon-e2e`, `just auth-e2e`, `just neon-test` (the compose checks, updated for the base profile) and `just neon-smoke`.
- **Studio:** unit tests (connection panel), typecheck, build, and a Playwright pass over create, reveal, reset and delete.

## Out of scope

- Branching (step 5).
- The data API's login for `authenticator`, grants, the users table and its event trigger (step 6). Step 4 only creates the three roles and the `admin_sql` path they'll use.
- Scale to zero and resizing (step 7).
- Extensions not in the current compute image, among them pgvector and pgsodium: roadmap step 4a. Step 4 keeps the image as it is.
- Using the notify body's shard and node data in the hooks ("Resume here", item 5): stays a note.
- Concurrent or shorter-timeout reconfigure pushes in the hooks ("Resume here", item 6): stays a note unless the reset measurement shows it's needed.
