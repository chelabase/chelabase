# Switch Provisioning to Neon: Implementation Plan (roadmap step 4)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.
>
> **Plan style:** this plan pins interfaces, behavior, tests and gotchas; the implementer writes the code. Literal code appears only where its exact form matters. Where a signature is given, use it exactly: other tasks depend on it.

**Goal:** Creating a database gives a Neon-backed database behind Neon's proxy (`<ep>.<domain>:4432` and `:4444`). The `postgres:17` provisioning and the `cheladb_platform` role go away.

**Architecture:** The worker owns the lifecycle (provision, delete, reset, reconcile) through four ports: metadata, the storage controller, a `ComputeBackend` (Docker) and the proxy cache, plus a spec pusher. The orchestrator's internal listener is unchanged, except that it now shares the spec builder with the worker. Superuser SQL runs through `ComputeBackend::admin_sql` (`docker exec` psql as `cloud_admin`).

**Tech Stack:** Rust 2024, bollard 0.21.1 (exec API), sqlx, reqwest 0.13, redis 1.7.1, axum 0.8. Neon at `infra/neon/NEON_REF` (`fa504217`). Bash, psql, curl and jq for the scripts. React and vitest for Studio.

**Spec:** `docs/superpowers/specs/2026-09-29-neon-provisioning-design.md`

## Status

- **Current:** step 4 complete, verified and merged into local `main` (2026-09-30). Nothing running; the deps stack (Neon included) is up, the dev stack is stopped, no computes are left.
- **End-of-step:** review (opus) returned with fixes: 0 Critical, 3 Important. Fix batch (opus): `7b8a2b0` (per-endpoint `pg_try_advisory_xact_lock` around `apply_pending` on a separate lazy 4-connection pool; the endpoint re-check after `create_timeline`; `{e:#}`; host/port only when ready; Studio reveals after a 503; `deleted_among` removed; dead deps dropped; shared `stored_endpoint_config`) and `88aba48` (README, `.env.example`, `infra/neon/README.md`). Scoped re-review (opus): every item addressed, and the I1 RED was a genuine interleaving failure.
- **Full verification run (2026-09-30, sonnet):**
  - `cargo test --workspace`: 427 passed, 23 ignored;
  - clippy clean;
  - live: metadata 18, `proxy_cache_live` 1, worker `neon_live` 1 (provision 6.0 s, reset 238 ms), `docker_backend_live` 2, `storcon_live` (with `just dev`);
  - `just neon-test` 58 ok, `neon-smoke`, `neon-e2e`, `auth-e2e`;
  - Studio 77/77, typechecks, build.
  - `storcon_live`'s precondition now says it also needs `just dev`: the storage controller calls the internal listener on attach.
- **Minors fixed before merge (user, 2026-09-30):** `aae92e4`: the endpoint lock is `SET LOCAL lock_timeout` 5 s plus a blocking `pg_advisory_xact_lock` (FIFO, bounded; 55P03 → "another password change for <ep> is still being applied"); the lock pool grew to 8 with a 5 s acquire; `control_addr` gets a 5 s timeout inside the lock; `EndpointGone` marks deleted only for `deleting`/`deleted`/missing rows; Studio shows one "stored but not yet applied" notice after a 503. Scoped re-review (opus): all addressed, ready to merge. Remaining notes (not defects): more than 8 concurrent resets on one endpoint fail with a pool timeout rather than the lock message; a push already in flight when `RESET_DEADLINE` fires can in theory be overtaken (pre-existing); the fairness test could pass by luck about 3% of the time under polling.
- **History, the fix list as dispatched:**
  - **I1** `services/worker/src/reset.rs` (~67/117/138): `apply_pending` isn't serialized per endpoint, so a stale push (reconcile holding VA) can land after a newer reset's push of VB; B clears the flag, and the compute keeps VA, so password B is refused and nothing retries. Serialize per endpoint across read → push → clear (`pg_advisory_lock(hashtext(ep))`, which also covers multiple workers, or a per-endpoint mutex). Test: `on_push` runs a second `apply_pending` with a newer verifier; assert the last pushed config carries the stored verifier.
  - **I2** README: `:129` anon is NOLOGIN; `:130` owner is a *member* of `neon_superuser`; `:131` legacy rows are tombstoned, not "credentials unavailable"; `:134` reconcile now recreates computes, removes computes with no endpoint row, and only reports tenants; `:136` connections go through Neon's proxy (no pgproxy/127.0.0.1:<port>); `:138` drop the init-password/platform-role security notes; `:122` smoke test needs `sslrootcert` (and `hostaddr`); `:159` drop "since step 4". `infra/compose/.env.example:39-40`: superuser switch is self-hosted only; a superuser owner can break `cloud_admin`/`neon_superuser` (compute_ctl, `admin_sql`). `infra/neon/README.md`: `neon-ps`, not `deps-ps`; add storcon 1234, proxy 4432/4444, Valkey 6379 to the deps table.
  - **I3** `{e:#}` for storage errors: `provision.rs:215,219`, `delete.rs:52`.
  - **Ruling 8** re-check after `create_timeline`: check that the endpoint row (`neon_endpoint(ep)`) still exists, or that the database still links it (not only the status). On a miss, tear down (the tenant especially), don't `set_failed`; fake-driven test.
  - Deferred minors marked fix-before-merge: remove `deleted_among` and its live test; the live_db comment "the endpoint is gone"; `rustfmt --edition 2024 services/worker/src/main.rs`; the retried-provision message (`provision.rs:200-203`: "a previous attempt was interrupted; its resources will be cleaned up"); tests for a teardown failure inside `abandon`.
  - Cheap minors to batch: drop the unused worker deps `bollard`/`uuid`/`tokio-postgres`; gate `host`/`port` on `status == Ready` (`orchestrator/src/lib.rs:357,427`); fix the split header comment in `scripts/lib/neon-common.sh:2-3`; roadmap `:19` "Nothing calls it yet"; the `ACK_WAIT` comment (bollard has no timeout, prefetch timers, a redelivered failed job re-provisions); reconcile report warnings mention `just sweep-dbs`; Studio reveals the new password after a 503 `RESET_NOT_APPLIED`; one shared `NeonRole`/`NeonDatabase` → spec mapping helper (orchestrator `internal/spec.rs` and `worker/reset.rs`); a comment on the recreate-then-push self-heal.
  - Ruling 7's text is wrong: `sweep-dbs` *still* removes `cheladb.managed=true` containers and volumes, and should keep doing so (the other computer). Fix the text, not the recipe.
  - Leave: every other deferred minor (reviewer triage agrees). Plan approved 2026-09-29. The branch lives in `.worktrees/neon-provisioning`, and implementers work there. `main` (with the build-speed chore, `bd61377`) was merged in at `14a32a4`, and `just test` passes (385 tests).
- **Done:**
  - Task 1 (`17da720`): `ComputeBackend`, `DockerBackend`, `admin_sql`, `FakeBackend`. 36 unit tests, both `docker_backend_live` tests and clippy pass. The stdin half-close works, so SQL stays on stdin (no `-c` switch); the live `admin_sql` test took about 4 s. `DockerLocator.docker` and `DockerLocator::inspect` are now `pub(crate)`.
  - Task 2 (`72c6699`): `Placement`, `Endpoint`, `endpoint_config`, `jwt::load_signer` and `StorconClient::list_tenant_ids` in `cheladb-neon`. The orchestrator's `endpoint_config` and `InternalConfig::load_signer` are now thin wrappers, and its tests are unchanged. `cargo test -p cheladb-neon -p orchestrator` and clippy pass. Extra test: `a_bad_stored_id_is_named`.
  - Task 3 (`6d64a7a`, fix `edd1951`): migration `20260929000001_neon_provisioning.sql` (applied to the local metadata DB), the new metadata queries, `RESET_NOT_APPLIED`, `Purpose::Platform` removed, orchestrator reveals `<ep>.<domain>:4432` and `http_url`, 409 without an endpoint, 503 on `RESET_NOT_APPLIED`. The worker's `provision`/`reset_password` return "not wired up yet". Review: spec ✅. One Important finding was fixed in round 1: `clear_needs_reconfigure` compared the verifier against a stale snapshot under READ COMMITTED, so a concurrent reset's flag could be cleared. It now locks the role row `FOR SHARE` in a CTE, and the live test `clear_needs_reconfigure_rechecks_the_verifier_after_a_lock_wait` went red first. `cargo test --workspace` and clippy passed at `6d64a7a`; the fix round re-ran the metadata crate (live_db 12, neon_live 6) and its clippy. `deleted_among` was kept (legacy reconcile used it; dead since Task 5); `set_owner_password_enc` was removed.
  - Task 4 (`59c1219`): `ports.rs` (`WorkerMetadata`, `Storage`, `CacheNotifier`, `SpecPusher`, `HttpPusher`), `fakes.rs` (one shared journal), `provision.rs`, `delete.rs`; `Worker` in `lib.rs`. `docker_ops.rs` and `tests/docker_live.rs` deleted early (allowed by a ruling); `roles.rs`/`reconcile.rs` stay for Task 5/6. No new `cheladb-neon` interfaces were needed. `cargo test --workspace` 408 passed; clippy clean; RED was 15 failing on `todo!()` stubs. Review (opus): spec ✅, approved, minors only.
  - Task 5 (`a034d56`): `reset.rs` (`reset_password`, `apply_pending`), `reconcile.rs` rewritten (`Snapshot`, `Action`, `plan`, `reconcile_once`); the legacy `main.rs` reconcile is a no-op until Task 6. `cargo test --workspace` 423 passed; clippy clean; RED was 23 failing on `todo!()` stubs. The Review Focus 1 test was mutation-checked (re-reading the verifier after the push makes it fail). The implementer stopped once when the auto-mode safety check returned no verdict (degraded service) and was resumed. Review (opus): spec ✅, approved, minors only.
  - Task 6 (`159137d`): `config.rs` (`WorkerConfig`, `check_control_plane_uri`, `build`), `main.rs` rewritten on `Arc<RealWorker>` (the 30 s loop calls `Worker::reconcile_once`; `RESET_DEADLINE` timeout replies `RESET_NOT_APPLIED`), `roles.rs` deleted, `tests/neon_live.rs`. Live run: provision 7.06 s, **reset 114 ms** (so `RESET_DEADLINE`/`RESET_TIMEOUT` unchanged), delete leaves no compute, tenant 404, no endpoint row. Missing image or JWT keys exit naming `scripts/neon-images.sh` / `just zitadel-bootstrap`. Added `MetadataClient::connect_lazy`. `cargo test --workspace` and clippy clean. Gated by the live test; no per-task review.
  - Task 7 (`053cc1a`): the Neon services start with `deps-up` (no `neon` profile; waits on `register-safekeepers`, copies the proxy CA); `neon-up`/`neon-down`/`neon-compute-demo` and `scripts/neon-compute-demo.sh` removed; `just dev`/`just worker` export `CHELADB_COMPUTE_IMAGE`; `sweep-dbs` removes compute containers, storage-controller tenants and endpoint rows; `neon-e2e.sh` goes through the gateway API. Gates: `just neon-test` (58 ok, RED first), `neon-smoke`, `neon-e2e`, `auth-e2e` all passed after `dev-down && down && deps-up && dev`. `sweep-dbs` verified live.
  - Task 8 (`4313f44`): `ConnectionDetails.http_url`; the panel shows the HTTP URL with its own copy button and the local-dev note. Studio 75/75 (RED first: 4 failing), types and studio typecheck, build. Playwright against `just dev`: sign up → create → ready → reveal (proxy host, 4432, HTTP URL, `verify-full` URI) → reset (new password) → delete (list empty). The run left a dev Zitadel user `task8-e2e@example.com`.
  - **Carry into Task 9:** stale `just neon-up`/`neon-compute-demo` mentions in `README.md`, `infra/neon/README.md`, the `tools/cli/src/neon/mod.rs:33` message, and the doc comments and ignore reasons in `crates/cheladb-neon/tests/{storcon_live,docker_backend_live}.rs` and `services/worker/tests/neon_live.rs`. `docker_backend_live`'s `CHELADB_LIVE_ENDPOINT` now comes from a database created through the API.
  - **Carried into Tasks 4-6 (all resolved by Task 6):** `reconcile_once` treats `Action::FailMissingContainer` as a no-op (otherwise every Neon database would be failed as "container missing"); the legacy `handle_delete` doesn't call `delete_endpoint`; the worker's `Ctx.key`/`owner_superuser` have `#[allow(dead_code)]`. Task 6 removes all three with the legacy modules.
- **Rulings (made while planning; cost if wrong):**
  1. Readiness polls `admin_sql(ep, <new database>, "SELECT 1")`, not the `postgres` database. The new database exists only once compute_ctl has applied the spec, so the bootstrap can't race it. *Cost if wrong: small; switch the database argument.*
  2. `CHELADB_COMPUTE_IMAGE` has no default: the worker refuses to start without it, and `just dev`/`just worker` set it from `scripts/neon-images.sh`. This avoids a second copy of the image digest in Rust. *Cost: none.*
  3. A reset that stored the new password but couldn't apply it replies `RESET_NOT_APPLIED`. The orchestrator answers that with 503 and the message, not a generic 500. *Cost: none.*
  4. `databases.endpoint_id` is `ON DELETE SET NULL`, so deleting an endpoint row never fails on the link. *Cost: none.*
  5. `needs_reconfigure` is cleared only if the owner's verifier still equals the one that was pushed, so a newer concurrent reset keeps its flag. *Cost: none.*
  6. `/v1/me` and `GET /v1/databases/{id}` keep their `host`/`port` fields, now `<ep>.<domain>` and 4432 (only once the database is ready), so Studio's list needs no change.
  7. `just sweep-dbs` still removes `cheladb.managed=true` containers and volumes (the postgres:17 era), and keeps doing so: the other computer may still have some.
  8. *(Task 4 review)* A delete that lands between provision step 2 and `create_tenant`, followed by a crash or failed teardown, leaves a tenant that no endpoint row records; reconcile only logs it. Fix in the end-of-step batch: provision re-checks its endpoint row after `create_timeline` and tears down if it's gone. *Cost if wrong: a rare orphan tenant, visible in reconcile logs, removed by hand.*
  9. *(Task 6)* A `RESET_DEADLINE` timeout replies `RESET_NOT_APPLIED`: the metadata transaction is quick, so a timeout almost always lands in the push or publish after the new secret committed. *Cost if wrong: rarely, the user is told a new password is pending when it wasn't stored; reveal still shows the truth.*
  10. *(Task 6)* NATS `ACK_WAIT` raised from 120 s to 300 s for the provision and delete consumers (worst-case provision plus teardown is about 180 s). *Cost if wrong: a crashed job waits 5 minutes before redelivery.*
- **Deferred minors:**
  - Task 1: the commit's trailer says `Claude Sonnet 5.5` (the implementer's model) instead of the plan's `Claude Opus 5.5`. Harmless; left as is.
  - Task 1: TDD wasn't strictly red-first; the implementer checked the tests by mutating the code afterwards (2 tests failed as expected).
  - Task 2: the RED run failed to compile on the missing items, so the two `list_tenant_ids` tests never ran red. The coordinator stubbed the function afterwards, and both failed. The `jwt.rs` `load_signer_*` tests duplicate the orchestrator's three signer tests. Only `storcon.rs` was rustfmt-checked.
  - Task 3: `migration_tombstones_rows_without_an_endpoint` re-runs the migration's `UPDATE`, which tombstones every unlinked live row in the shared dev metadata DB, not just its own row.
  - Task 3: no unit test for `proxy_domain_from_env` with blank or unset values.
  - Task 3: the race test sleeps 500 ms before committing instead of polling `pg_locks`, so it can pass without exercising the wait (it can't fail spuriously).
  - Task 4: storage-controller errors are formatted with `{e}`, not `{e:#}` (`provision.rs` create tenant/timeline, `delete.rs` delete tenant), so the cause chain is lost from `databases.error`.
  - Task 4: after a `set_ready` error, the retried job fails with "recording the endpoint: … already has an endpoint"; it should say a previous attempt left an endpoint.
  - Task 4: untested branches: the step-7 `get_database` error, the `set_ready` error, a teardown failure inside `abandon`, a `delete_tenant` failure during provision teardown, and `create_endpoint_for_database` returning false; `metadata_trouble_is_retried` asserts only `is_err()`.
  - Task 4: `FakeMetadata.deleting_on_get` counts `get_database` calls across all ids.
  - Task 6: check the NATS `ack_wait` against the worst-case provision time (60 s readiness plus storage-controller calls); a redelivery during a live run shows a transient `failed`.
  - Task 5: when a ready database lost its compute and has a pending reset, the same pass recreates the compute and pushes before `compute_ctl` listens; it heals on the next pass. A comment at most.
  - Task 5: a `clear_needs_reconfigure` error after a successful push and publish still replies `RESET_NOT_APPLIED` (harmless; reconcile clears it).
  - Task 5: `deleted_among` has no production caller any more; remove it and its live test.
  - Task 6: `check_control_plane_uri` refuses `http://h:80`, because the URL parser hides default ports.
  - Task 6: `main.rs` (rewritten) wasn't run through rustfmt.
  - Task 7: the `neon-e2e` failure trap (delete the database on failure) wasn't exercised live.
  - Task 8: Studio's table header cells run together ("NameStatusEndpoint") while the connection panel is open; cosmetic, probably older than this step.
- **Left running:** the deps stack (Neon included, now part of `deps-up`). No compute containers; the dev stack is stopped.

## Global Constraints

- Branch `neon-provisioning`. Commit with `git commit`, ending with the `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>` and `Claude-Session: https://claude.ai/code/session_01SUiNQSh3VyeaGC5pkLP934` trailers. Don't merge or push.
- Never run `cargo fmt` on the workspace; `rustfmt --edition 2024 <file>` only on files you create.
- Never overwrite `.cheladb/secrets/secrets.key`, the gateway key pair, the cookie key or the compute-JWT key pair.
- Ids: tenant and timeline are `NeonId` (32 hex); endpoint is `ep-` + 12 lowercase hex; compute token is 32 random bytes as hex, and only its SHA-256 (hex) is stored; owner password is `credentials::generate_password()`.
- Owner role `postgres`. The data API roles `anon`, `authenticated` (NOLOGIN) and `authenticator` (NOLOGIN NOINHERIT, granted both) are created through `admin_sql`, never through the spec.
- `CHELADB_DB_OWNER_SUPERUSER` takes effect only when `CHELADB_MODE` is self-hosted. In platform mode it's ignored, with a startup warning.
- Reset order: metadata (sealed password, verifier, `needs_reconfigure = true`) in one transaction → push the spec if the compute runs → `ProxyCache::password_updated` (0 receivers is an error) → clear the flag. `RESET_DEADLINE` (8 s) stays under the orchestrator's `RESET_TIMEOUT` (15 s).
- Reveal: `uri = postgresql://postgres:<pw>@<ep>.<domain>:4432/<database_name>?sslmode=verify-full`, `http_url = https://<ep>.<domain>:4444/sql`, `host = <ep>.<domain>`, `port = 4432`. `<domain>` is `CHELADB_PROXY_DOMAIN`, default `localtest.me`.
- Worker env, with local-dev defaults: `CHELADB_STORCON_URL=http://127.0.0.1:1234`, `CHELADB_COMPUTE_JWT_DIR=.cheladb/neon`, `CHELADB_COMPUTE_NETWORK=cheladb_default`, `CHELADB_CONTROL_PLANE_URI=http://host.docker.internal:8083`, `CHELADB_VALKEY_URL=redis://127.0.0.1:6379`. `CHELADB_COMPUTE_IMAGE` is required. Blank counts as unset.
- Compute container: `cheladb-compute-<ep>`, entrypoint `/usr/local/bin/compute_ctl`, args `compute_ctl_args(ep, control_plane_uri)`, token in env `NEON_CONTROL_PLANE_TOKEN` (never argv), network `CHELADB_COMPUTE_NETWORK`, `host.docker.internal:host-gateway`, label `cheladb.dev/endpoint-id=<ep>`, restart `unless-stopped`, 1 CPU (`nano_cpus = 1_000_000_000`) / 2 GiB, `3080/tcp` published on `127.0.0.1` with a random host port.
- Reconcile never deletes a storage-controller tenant that has no endpoint row: it logs only.
- WSL's ephemeral port range is small. If tests fail binding `127.0.0.1:0`, wait a minute and rerun.

## Review Focus

1. **Two resets of the same database overlap.** The first one to finish must not clear `needs_reconfigure` for the second. Test in Task 5 (`apply_pending_keeps_the_flag_when_a_newer_reset_landed`).
2. **A delete lands while a provision is in flight.** Everything the provision created is removed, and the row ends `deleted`, never `ready` or `failed`. Test in Task 4 (both the step 7 re-check and `set_ready` returning false).
3. **A teardown fails halfway** (for example, the storage controller is down). The endpoint row stays, so reconcile can retry it; the database is `failed`, or stays `deleting`. Tests in Tasks 4 and 5.
4. **`admin_sql` gets a database name that isn't a plain identifier** (`x -h evil`, `a=b`, empty, 64 chars). It's refused before any exec. Test in Task 1.
5. **The worker starts before `cheladb dev bootstrap`, or without `CHELADB_COMPUTE_IMAGE`.** It exits with a message naming what to run or set. Test in Task 6.

---

## File Structure

| File | Responsibility |
|---|---|
| `crates/cheladb-neon/src/backend.rs` (create) | `ComputeBackend`, `ComputeStatus`, `ComputeSettings`, `DockerBackend`, container body, psql command, `fake` module |
| `crates/cheladb-neon/src/compute.rs` (modify) | `DockerLocator` exposes its client and inspect to `backend.rs` |
| `crates/cheladb-neon/src/spec.rs` (modify) | `Placement` trait, `Endpoint`, shared `endpoint_config` |
| `crates/cheladb-neon/src/jwt.rs` (modify) | `load_signer(dir)` |
| `crates/cheladb-neon/src/storcon.rs` (modify) | `list_tenant_ids` |
| `crates/cheladb-metadata/migrations/20260929000001_neon_provisioning.sql` (create) | the one migration |
| `crates/cheladb-metadata/src/{lib.rs,neon.rs}` (modify) | record changes, new queries |
| `crates/cheladb-nats/src/lib.rs` (modify) | `RESET_NOT_APPLIED` |
| `crates/cheladb-secrets/src/lib.rs` (modify) | drop `Purpose::Platform` |
| `services/orchestrator/src/{lib.rs,main.rs,internal/spec.rs,internal/config.rs}` (modify) | reveal, summaries, reset, shared spec builder |
| `services/worker/src/{ports.rs,provision.rs,delete.rs,reset.rs,reconcile.rs,config.rs,fakes.rs,lib.rs,main.rs}` | the worker (Tasks 3–6) |
| `services/worker/src/{roles.rs,docker_ops.rs}`, `tests/docker_live.rs` (delete) | legacy |
| `infra/compose/docker-compose.yml`, `justfile`, `scripts/*`, `infra/neon/tests/run.sh` | Task 7 |
| `packages/types/src/index.ts`, `apps/studio/src/components/database-actions.{tsx,test.tsx}` | Task 8 |
| `README.md`, `docs/roadmap/2026-09-28-neon-and-data-api.md` | Task 9 |

Models (implementer / per-task reviewer). A task gets its own review only when later tasks build on it or nothing else in the plan would catch a mistake; the rest are covered by the end-of-step review.

| Task | Implementer | Reviewer |
|---|---|---|
| 1 ComputeBackend | sonnet | none (its live test is a hard gate) |
| 2 Shared spec builder | sonnet | none (moved code, existing tests unchanged) |
| 3 Metadata, orchestrator API, legacy removal | sonnet | sonnet (migration can't be undone) |
| 4 Provision and delete | opus | opus |
| 5 Reset and reconcile | opus | opus |
| 6 Worker wiring and live test | sonnet | none (live test) |
| 7 Compose, just, scripts | sonnet | none (neon-test, neon-e2e, auth-e2e) |
| 8 Studio panel | sonnet | none (unit tests, Playwright) |
| 9 Docs and verification | haiku | none |
| End-of-step whole-branch review | | opus |

---

### Task 1: `ComputeBackend` and `DockerBackend` (`cheladb-neon`)

**Files:** create `crates/cheladb-neon/src/backend.rs` and `crates/cheladb-neon/tests/docker_backend_live.rs`; modify `compute.rs`, `lib.rs` and `Cargo.toml` (add `[features] fake = []`; add `futures-util` to `[dependencies]` if it isn't there).

**Produces (exact):**

```rust
pub enum ComputeStatus { Running, NotRunning(String) }   // String: Docker's state, e.g. "exited"
pub struct ComputeSettings { pub image: String, pub network: String, pub control_plane_uri: String }

pub trait ComputeBackend: ComputeLocator {
    fn create(&self, endpoint_id: &str, compute_token: &str) -> impl Future<Output = anyhow::Result<()>> + Send;
    fn delete(&self, endpoint_id: &str) -> impl Future<Output = anyhow::Result<()>> + Send;
    fn status(&self, endpoint_id: &str) -> impl Future<Output = anyhow::Result<Option<ComputeStatus>>> + Send;
    /// Endpoint ids of every container carrying ENDPOINT_LABEL, running or not.
    fn list(&self) -> impl Future<Output = anyhow::Result<Vec<String>>> + Send;
    /// Runs `sql` (stdin) with psql as cloud_admin inside the compute; returns stdout.
    fn admin_sql(&self, endpoint_id: &str, database: &str, sql: &str) -> impl Future<Output = anyhow::Result<String>> + Send;
}

pub struct DockerBackend { /* DockerLocator + ComputeSettings */ }
impl DockerBackend { pub fn connect(settings: ComputeSettings) -> anyhow::Result<Self>; }
// DockerBackend implements ComputeLocator (by delegating to its DockerLocator) and ComputeBackend.

pub fn create_body(endpoint_id: &str, compute_token: &str, settings: &ComputeSettings) -> bollard::models::ContainerCreateBody;
pub fn admin_psql_command(database: &str) -> anyhow::Result<Vec<String>>;

#[cfg(any(test, feature = "fake"))]
pub mod fake {
    pub type Journal = std::sync::Arc<std::sync::Mutex<Vec<String>>>;
    pub struct FakeBackend { /* see below */ }
}
```

Re-export `ComputeBackend`, `ComputeStatus`, `ComputeSettings` and `DockerBackend` from `lib.rs`. Keep `DockerLocator` as is for the orchestrator.

**Behavior:**
- `create`: build the body from the Global Constraints. Publish 3080 with `PortBinding { host_ip: "127.0.0.1", host_port: "0" }` (the old worker's proven form) and `exposed_ports: vec!["3080/tcp"]`. On 409 (name taken), force-remove the old container and create again. Then start. Errors carry `creating container <name>` or `starting container <name>`. The image isn't pulled (`neon-images.sh` already made sure it's present).
- `delete`: `remove_container(force: true, v: true)`; a 404 is fine.
- `status`: inspect; a 404 gives `None`; `RUNNING` gives `Running`; anything else gives `NotRunning(state)`.
- `list`: `list_containers(all: true, filters: label=ENDPOINT_LABEL)`, returning the label values.
- `admin_sql`: the command is `["psql","-X","-q","-A","-t","-v","ON_ERROR_STOP=1","-h","localhost","-U","cloud_admin","-d",<database>]`. `admin_psql_command` refuses any `database` that doesn't match `^[A-Za-z0-9_]{1,63}$` (psql would read `-d a=b` as a conninfo string). Run `create_exec` with user `postgres`, `attach_stdin/stdout/stderr`, env `PGCONNECT_TIMEOUT=5`. Then `start_exec`, write the SQL to `input`, then `shutdown()` and drop it, so psql sees EOF. Collect stdout and stderr from `output`, then `inspect_exec` for `exit_code`. Exit code 0 returns stdout; anything else is an error: `psql in compute <ep> exited with <code>: <stderr trimmed>`. The whole call is bounded by a 30 s `tokio::time::timeout`.
  - **Gotcha:** if the stdin half-close doesn't reach psql (the live test hangs until the timeout), pass the SQL as `-c <sql>` instead. None of our SQL carries a secret. Record the switch in the plan's Status as a ruling.
- `FakeBackend` (in-memory, `Default`, plus `with_journal(Journal)`):
  - state is a map `ep → { token: String, running: bool }`, and `sql: Vec<(ep, database, sql)>`;
  - knobs (all `Mutex`): `fail_create: Option<String>`; `sql_failures_left: u32` (each call fails with "not ready" and decrements while > 0); `fail_sql_containing: Option<(String, String)>` (fails when the SQL contains the first string, with the second as message); `fail_delete: Option<String>`;
  - helpers: `token_of(ep) -> Option<String>`, `set_running(ep, bool)`, `insert(ep)` (a running compute), `remove(ep)` (vanished);
  - `create` inserts a running compute (replacing any); `address` gives `172.20.0.2` and `control_addr` gives `127.0.0.1:3080` while running;
  - every call appends to the journal: `compute.create <ep>`, `compute.delete <ep>`, `compute.sql <ep> <database> <first line of sql>`.

**Tests (unit, in `backend.rs`):**
- `container_body_runs_compute_ctl_with_the_token_in_env`: image, entrypoint, cmd equal to `compute_ctl_args`, env is exactly `["NEON_CONTROL_PLANE_TOKEN=<t>"]`, label, and the token never appears in cmd.
- `container_body_limits_restarts_and_networks`: `network_mode`, `unless-stopped`, memory 2 GiB, `nano_cpus`, `extra_hosts` contains `host.docker.internal:host-gateway`.
- `compute_ctl_is_published_on_loopback_only`: the 3080 binding's `host_ip` is `127.0.0.1`.
- `psql_command_uses_cloud_admin_on_localhost` (exact vector).
- `psql_command_refuses_non_identifiers` (Review Focus 4): `""`, `"a=b"`, `"x -h evil"`, `"-c"`, 64 × `a`; accepts `web_1`, `1abc`.
- The fake: `create` then `status` is `Running`, `list` has it, and the journal records the calls; `sql_failures_left` counts down.

**Live tests** (`tests/docker_backend_live.rs`, `#[ignore]`):
- `lifecycle`: needs Docker and the compute image (`COMPUTE_NODE_IMAGE`, else `scripts/neon-images.sh` output). Network `bridge`, control-plane URI `http://127.0.0.1:9`. Steps: create → `list` contains it → `status` is `Running` → create again (replaced) → delete → `status` is `None` → delete again succeeds.
- `admin_sql_on_a_running_compute`: needs `CHELADB_LIVE_ENDPOINT` (a compute made by `just neon-compute-demo`, which still exists at this point).
  - `SELECT current_user` gives `cloud_admin`;
  - `SELECT rolsuper FROM pg_roles WHERE rolname = current_user` gives `t`;
  - a guarded `DO $$ … CREATE ROLE cheladb_live_probe NOLOGIN … $$` then `DROP ROLE cheladb_live_probe` both succeed;
  - `SELEC 1` fails with an error containing `syntax error`;
  - `admin_sql(ep, "a=b", …)` fails without an exec.
  - This is the spec's "confirm the exec path first". If it fails, stop and report; nothing after this task works without it.

- [ ] Write the unit tests and the fake; watch them fail, then implement `backend.rs`; `cargo test -p cheladb-neon` passes.
- [ ] Bring up the stack (`just neon-up`, `just dev`, `just neon-compute-demo`), run both live tests (`cargo test -p cheladb-neon --test docker_backend_live -- --ignored --test-threads=1`), then `just neon-compute-demo --down`.
- [ ] `cargo clippy -p cheladb-neon --all-targets --features fake -- -D warnings`; commit `cheladb-neon: ComputeBackend with a Docker implementation, admin_sql and a fake`.

---

### Task 2: Shared spec builder, signer loader, tenant list (`cheladb-neon` + orchestrator)

**Files:** modify `crates/cheladb-neon/src/{spec.rs,jwt.rs,storcon.rs}`, `tests/storcon_mock.rs`, `Cargo.toml` (dev-dep `tempfile`); `services/orchestrator/src/internal/{spec.rs,config.rs}`.

**Produces (exact):**

```rust
// spec.rs
pub trait Placement: Send + Sync {
    fn pageserver_connstring(&self, tenant: &NeonId) -> impl Future<Output = Result<String, StorconError>> + Send;
    fn active_safekeepers(&self) -> impl Future<Output = Result<Vec<String>, StorconError>> + Send;
}
impl Placement for StorconClient { /* call the inherent methods by full path */ }
pub struct Endpoint<'a> { pub id: &'a str, pub tenant_id: &'a str, pub timeline_id: &'a str }
pub async fn endpoint_config(placement: &impl Placement, endpoint: Endpoint<'_>, roles: Vec<RoleSpec>,
                             databases: Vec<DatabaseSpec>, jwks: serde_json::Value) -> anyhow::Result<serde_json::Value>;
// jwt.rs
pub fn load_signer(dir: &std::path::Path) -> anyhow::Result<ComputeSigner>;
// storcon.rs
impl StorconClient { pub async fn list_tenant_ids(&self) -> Result<Vec<NeonId>, StorconError>; }
```

**Behavior:**
- `endpoint_config`: parse the stored tenant and timeline ids (`context("stored tenant id")` and so on), ask the placement, and call `config_response`. This is exactly today's `services/orchestrator/src/internal/spec.rs::endpoint_config`, moved here.
- The orchestrator's `endpoint_config` becomes a thin wrapper: it reads roles and databases from `NeonMetadataPort`, maps them to `RoleSpec`/`DatabaseSpec`, and calls the shared function with `&state.storcon` and `state.signer.jwks()`. The existing orchestrator tests must pass unchanged.
- `load_signer`: move `InternalConfig::load_signer`'s body here, with the same messages (`just zitadel-bootstrap`, `corrupt`). `InternalConfig::load_signer` calls it, and its tests stay where they are.
- `list_tenant_ids`: `GET /control/v1/tenant` with no query returns every tenant (neon `storage_controller/src/http.rs:920`, `handle_tenant_list`). Parse `[{ "tenant_id": "<32 hex>", … }]`, ignoring the other fields.

**Tests:**
- `spec.rs`: a fake `Placement` gives `endpoint_config_uses_the_placement` (pageserver and safekeepers land in the spec); a placement error propagates.
- `jwt.rs`: move or copy the three signer-file tests (missing, corrupt, valid) to `load_signer`.
- `storcon_mock.rs`: `list_tenant_ids_reads_every_tenant` (GET path `/control/v1/tenant`, two ids parsed), and a malformed id is an error.

- [ ] Tests first; implement; `cargo test -p cheladb-neon -p orchestrator` passes; clippy clean.
- [ ] Commit `cheladb-neon: shared spec builder, signer loader and tenant list; orchestrator uses them`.

---

### Task 3: Metadata migration, the orchestrator's API, and legacy removal

**Files:** create `crates/cheladb-metadata/migrations/20260929000001_neon_provisioning.sql`; modify `crates/cheladb-metadata/src/{lib.rs,neon.rs}`, `tests/live_db.rs`, `tests/neon_live.rs`, `crates/cheladb-nats/src/lib.rs`, `crates/cheladb-secrets/src/lib.rs`, `services/orchestrator/src/{lib.rs,main.rs}`, `services/orchestrator/tests/api.rs`, `services/worker/src/main.rs`.

**The migration (exact):**

```sql
-- Roadmap step 4: every database runs on Neon. Rows from the postgres:17 era can't be served
-- any more; they become tombstones before their columns go.
ALTER TABLE databases ADD COLUMN endpoint_id text REFERENCES neon_endpoints (id) ON DELETE SET NULL;
UPDATE databases
   SET status = 'deleted', deleted_at = now(), updated_at = now(), owner_password_enc = NULL
 WHERE endpoint_id IS NULL AND status <> 'deleted';
ALTER TABLE databases
    DROP COLUMN container_id,
    DROP COLUMN host,
    DROP COLUMN port,
    DROP COLUMN platform_password_enc;
CREATE UNIQUE INDEX databases_endpoint_id_key ON databases (endpoint_id) WHERE endpoint_id IS NOT NULL;
ALTER TABLE neon_endpoints ADD COLUMN needs_reconfigure boolean NOT NULL DEFAULT false;
```

**Produces (exact types and signatures, all in `cheladb_metadata`):**

```rust
// DatabaseRecord: remove container_id, host, port, platform_password_enc; add
pub endpoint_id: Option<String>,
// ReadyDatabase<'a> { database_name: &'a str, owner_role: &'a str, owner_password_enc: &'a str }
pub async fn set_ready(&self, id: DatabaseId, endpoint_id: &str, db: ReadyDatabase<'_>) -> Result<bool, MetadataError>;
// LiveDatabase gains `pub endpoint_id: Option<String>`
pub struct NewEndpoint<'a> { pub id: &'a str, pub tenant_id: &'a str, pub timeline_id: &'a str,
    pub compute_token_sha256: &'a str, pub owner_role: &'a str, pub owner_verifier: &'a str, pub database_name: &'a str }
pub struct EndpointState { pub id: String, pub tenant_id: String, pub database_id: Option<DatabaseId>,
    pub database_status: Option<DatabaseStatus>, pub needs_reconfigure: bool }
impl MetadataClient {
    /// One transaction: endpoint, role, database, then link. False (and nothing written) when the
    /// database is deleting/deleted/missing or already linked.
    pub async fn create_endpoint_for_database(&self, id: DatabaseId, ep: &NewEndpoint<'_>) -> Result<bool, MetadataError>;
    /// Roles and databases cascade; databases.endpoint_id becomes NULL. Missing is fine.
    pub async fn delete_endpoint(&self, endpoint_id: &str) -> Result<(), MetadataError>;
    /// One transaction: sealed password (only while ready and linked to endpoint_id), the role's
    /// verifier, needs_reconfigure = true. False (nothing written) otherwise.
    pub async fn replace_owner_secret(&self, id: DatabaseId, endpoint_id: &str, role: &str,
        verifier: &str, sealed_password: &str) -> Result<bool, MetadataError>;
    /// Clears the flag only while `role`'s verifier still equals `applied_verifier`. True when cleared.
    pub async fn clear_needs_reconfigure(&self, endpoint_id: &str, role: &str, applied_verifier: &str) -> Result<bool, MetadataError>;
    pub async fn replace_compute_token(&self, endpoint_id: &str, sha256_hex: &str) -> Result<(), MetadataError>;
    /// Every endpoint, left-joined to the database linked to it.
    pub async fn list_endpoints_for_reconcile(&self) -> Result<Vec<EndpointState>, MetadataError>;
}
// cheladb-nats
pub const RESET_NOT_APPLIED: &str = "the password was changed but not yet applied; it will be retried";
```

Also:
- `mark_deleted` clears `owner_password_enc` only.
- The sealed-rows count in `ensure_key_check` uses `owner_password_enc` only.
- `set_owner_password_enc` and `deleted_among` are removed if nothing uses them after this task (the legacy worker code below goes).
- `Purpose::Platform` is removed from `cheladb-secrets`, along with its test.

**Orchestrator:**
- `AppState` gains `pub proxy_domain: String`; `main.rs` reads `CHELADB_PROXY_DOMAIN` (blank or unset means `localtest.me`).
- Add constants `PROXY_PG_PORT = 4432` and `PROXY_HTTP_PORT = 4444`.
- `DatabaseResponse` and `DatabaseSummary` keep `host`/`port`: `Some("<ep>.<domain>")` and `Some(4432)` when `endpoint_id` is set, else `None`. The `From` impls become functions that take the domain.
- Reveal: `ConnectionDetails` gains `http_url`, formatted as in the Global Constraints. A ready record without `endpoint_id`, `database_name` or sealed password keeps today's 409 "credentials unavailable; recreate this database".
- The reset precheck tests `endpoint_id.is_none()` instead of the platform password. A reply error equal to `RESET_NOT_APPLIED` becomes 503 with that message.

**Worker (legacy removal only; Tasks 4–6 rebuild it):**
- In `main.rs`, `provision()` and `reset_password()` lose their bodies. `provision` returns `Err("provisioning on Neon is not wired up yet")`, and `reset_password` returns `Err("password reset on Neon is not wired up yet")`.
- Drop every use of the removed fields and of `Purpose::Platform`.
- Leave `roles.rs`, `docker_ops.rs` and the reconcile module for Task 6 to delete, unless they no longer compile. The workspace must build and every test must pass at the end of this task.

**Tests:**
- `api.rs`:
  - update `sample_database`/`ready_database` (set `endpoint_id: Some("ep-0123456789ab")`, drop the removed fields);
  - `reveal_ready_database_returns_decrypted_owner_credentials` asserts host `ep-0123456789ab.localtest.me`, port 4432, the exact `uri` and `http_url`;
  - `summaries_show_the_proxy_host` (`/v1/me` and GET);
  - `reveal_without_an_endpoint_is_409`;
  - `reset_without_an_endpoint_is_409`;
  - `reset_not_applied_is_503_with_the_message` (add a `FakeReset::NotApplied`);
  - remove the tests about the platform credential; `me_never_exposes_database_secrets` keeps its `owner_password_enc` check.
- `live_db.rs` and `neon_live.rs` (ignored; run them):
  - `migration_tombstones_rows_without_an_endpoint`: insert a legacy-shaped row (pending, no endpoint), re-run the migration's `UPDATE` by hand (the migration already ran), and assert it's `deleted`;
  - `create_endpoint_links_and_refuses_a_deleting_database`;
  - `delete_endpoint_cascades_and_unlinks`;
  - `replace_owner_secret_is_atomic_and_needs_ready` (not ready gives false and nothing changes);
  - `clear_needs_reconfigure_only_for_the_applied_verifier`;
  - `list_endpoints_for_reconcile_joins_status`;
  - update the existing `set_ready` tests to the new signature.

- [ ] Write or adjust the tests; implement; `cargo test --workspace` passes; run the ignored metadata tests with `just deps-up` running (`--test-threads=1`); clippy clean.
- [ ] Commit `metadata: link databases to Neon endpoints; orchestrator reveals proxy URLs; remove postgres:17 provisioning`.

---

### Task 4: Worker ports, fakes, provision and delete

**Files:** create `services/worker/src/{ports.rs,provision.rs,delete.rs,fakes.rs}`; modify `lib.rs`, `names.rs` (keep only `sanitize_db_name`) and `Cargo.toml`:
- add `cheladb-neon`, `reqwest`, `sha2`, `hex` and `serde_json` if missing;
- dev-dependency `cheladb-neon = { workspace = true, features = ["fake"] }`.

**Produces (exact):**

```rust
// ports.rs
pub trait WorkerMetadata: Send + Sync + 'static {
    fn get_database(&self, id: DatabaseId) -> impl Future<Output = Result<Option<DatabaseRecord>, MetadataError>> + Send;
    fn set_status(&self, id: DatabaseId, status: DatabaseStatus) -> impl Future<Output = Result<bool, MetadataError>> + Send;
    fn create_endpoint_for_database(&self, id: DatabaseId, ep: &NewEndpoint<'_>) -> impl Future<Output = Result<bool, MetadataError>> + Send;
    fn neon_endpoint(&self, endpoint_id: &str) -> impl Future<Output = Result<Option<NeonEndpoint>, MetadataError>> + Send;
    fn neon_roles(&self, endpoint_id: &str) -> impl Future<Output = Result<Vec<NeonRole>, MetadataError>> + Send;
    fn neon_databases(&self, endpoint_id: &str) -> impl Future<Output = Result<Vec<NeonDatabase>, MetadataError>> + Send;
    fn set_ready(&self, id: DatabaseId, endpoint_id: &str, db: ReadyDatabase<'_>) -> impl Future<Output = Result<bool, MetadataError>> + Send;
    fn set_failed(&self, id: DatabaseId, error: &str) -> impl Future<Output = Result<(), MetadataError>> + Send;
    fn mark_deleted(&self, id: DatabaseId) -> impl Future<Output = Result<(), MetadataError>> + Send;
    fn delete_endpoint(&self, endpoint_id: &str) -> impl Future<Output = Result<(), MetadataError>> + Send;
    fn replace_owner_secret(&self, id: DatabaseId, endpoint_id: &str, role: &str, verifier: &str, sealed: &str) -> impl Future<Output = Result<bool, MetadataError>> + Send;
    fn clear_needs_reconfigure(&self, endpoint_id: &str, role: &str, applied_verifier: &str) -> impl Future<Output = Result<bool, MetadataError>> + Send;
    fn replace_compute_token(&self, endpoint_id: &str, sha256_hex: &str) -> impl Future<Output = Result<(), MetadataError>> + Send;
    fn fail_if_stale(&self, id: DatabaseId, max_age_secs: i64) -> impl Future<Output = Result<bool, MetadataError>> + Send;
    fn list_live_for_reconcile(&self) -> impl Future<Output = Result<Vec<LiveDatabase>, MetadataError>> + Send;
    fn list_endpoints_for_reconcile(&self) -> impl Future<Output = Result<Vec<EndpointState>, MetadataError>> + Send;
}
// impl for MetadataClient: each method calls MetadataClient::<method>(self, …) by full path
// (a bare self.method() would recurse after a rename; same rule as the orchestrator's ports).

pub trait Storage: Placement + 'static {
    fn create_tenant(&self, tenant: &NeonId) -> impl Future<Output = Result<(), StorconError>> + Send;
    fn create_timeline(&self, tenant: &NeonId, timeline: &NeonId) -> impl Future<Output = Result<(), StorconError>> + Send; // pg 17, fresh
    fn delete_tenant(&self, tenant: &NeonId) -> impl Future<Output = Result<(), StorconError>> + Send;
    fn list_tenant_ids(&self) -> impl Future<Output = Result<Vec<NeonId>, StorconError>> + Send;
}
pub trait CacheNotifier: Send + Sync + 'static {
    fn password_updated(&self, endpoint_id: &str, role: &str) -> impl Future<Output = anyhow::Result<u64>> + Send;
}
pub trait SpecPusher: Send + Sync + 'static {
    fn jwks(&self) -> serde_json::Value;
    fn push(&self, control_addr: SocketAddr, endpoint_id: &str, config: &serde_json::Value) -> impl Future<Output = anyhow::Result<()>> + Send;
}
pub struct HttpPusher { pub http: reqwest::Client, pub signer: ComputeSigner } // push = reconfigure::push
// impls: Storage for StorconClient, CacheNotifier for ProxyCache, SpecPusher for HttpPusher.

// lib.rs
pub struct Worker<M, S, C, N, P> {
    pub metadata: M, pub storage: S, pub computes: C, pub cache: N, pub pusher: P,
    pub key: SecretKey, pub owner_superuser: bool,
    pub ready_poll: Duration,    // 500 ms in production
    pub ready_timeout: Duration, // 60 s in production
}
pub const OWNER_ROLE: &str = "postgres";

// provision.rs (impl<M: WorkerMetadata, S: Storage, C: ComputeBackend, N: CacheNotifier, P: SpecPusher> Worker<…>)
pub async fn handle_provision(&self, id: DatabaseId, name: &str) -> Result<(), String>; // Err only for metadata trouble (job retried)
pub fn bootstrap_sql(owner_superuser: bool) -> String;
pub fn new_endpoint_id() -> String;
pub fn new_compute_token() -> String;
pub fn sha256_hex(s: &str) -> String;
// delete.rs
pub async fn handle_delete(&self, id: DatabaseId) -> Result<(), String>;
pub(crate) async fn teardown(&self, endpoint_id: &str, tenant_id: &str) -> Result<(), String>;
```

**Behavior (provision):** follow the spec's "Provision" section. Specifics:
- The skip checks and the claim are as in today's `main.rs::handle_provision`. A name `sanitize_db_name` rejects means `set_failed` and `Ok`.
- Steps and error prefixes:
  - step 2: `create_endpoint_for_database`; an error or `false` fails with `recording the endpoint: …`, and there's nothing to clean;
  - `creating tenant: …`;
  - `creating timeline: …`;
  - `starting compute: …`;
  - readiness polls `admin_sql(ep, database_name, "SELECT 1")` every `ready_poll` until `ready_timeout`, else `waiting for the compute: <last error>`;
  - `bootstrapping roles: …`.
- After step 2, remember `(endpoint_id, tenant_id)`. On any later failure, run `teardown` (a teardown error is logged, and reconcile finishes it), then `set_failed(id, err)`, then `Ok`.
- Step 7: `get_database` again. If `should_abort_provision(status)`, teardown and `mark_deleted`. Otherwise seal the password (`Purpose::Owner`, AAD the database id) and `set_ready`. If it returns `false`, teardown and `mark_deleted`.
- `bootstrap_sql` (exact form):

```sql
DO $$
BEGIN
  IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname = 'anon') THEN
    CREATE ROLE anon NOLOGIN;
  END IF;
  IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname = 'authenticated') THEN
    CREATE ROLE authenticated NOLOGIN;
  END IF;
  IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname = 'authenticator') THEN
    CREATE ROLE authenticator NOLOGIN NOINHERIT;
  END IF;
END
$$;
GRANT anon, authenticated TO authenticator;
```

  With `owner_superuser`, append `ALTER ROLE postgres SUPERUSER;`. compute_ctl never drops roles that aren't in the spec, and on reconfigure it only runs `ALTER ROLE … LOGIN PASSWORD` for spec roles whose verifier changed (neon `compute_tools/src/spec_apply.rs:786-835`). So these roles and the SUPERUSER bit survive later reconfigures.

**Behavior (delete):** load the record. If it has an `endpoint_id` whose endpoint row exists, `teardown(ep, tenant)`: `computes.delete` → `storage.delete_tenant` (404 already counts as done) → `metadata.delete_endpoint`, stopping at the first error with prefixes `removing compute:`, `deleting tenant:`, `deleting endpoint:`. Then `mark_deleted`. A teardown error returns `Err`, so the job is retried and the row stays `deleting`.

**Fakes (`fakes.rs`, `#[cfg(test)]`):**
- `FakeMetadata { state: Arc<Mutex<FakeState>>, journal: Journal }`, holding databases (by id), endpoints (id → `NeonEndpoint` + roles + databases + `needs_reconfigure`), ages, and knobs:
  - `fail: Option<&'static str>` (the method name that errors);
  - `deleting_on_get: Option<u32>` (the Nth `get_database` call first flips the row to `deleting`);
  - `deleting_before_set_ready: bool`.
- Every write method journals `metadata.<method>`. Semantics mirror the SQL guards above (status checks, cascade, verifier-guarded clear).
- `FakeStorage` (tenants set, `fail: Option<&'static str>`, journals `storage.<method>`; `Placement` gives `postgresql://no_user@pageserver:6400` and `["safekeeper1:5454"]`).
- `FakeCache` (`receivers: u64` default 1, `fail: Option<String>`, journals `cache.password_updated <ep>`).
- `FakePusher` (`fail: Option<String>`, `on_push: Option<Box<dyn FnOnce() + Send>>`, records the pushed configs, journals `pusher.push <ep>`).
- `fn worker() -> (Worker<FakeMetadata, FakeStorage, FakeBackend, FakeCache, FakePusher>, Journal)`: one shared journal, `ready_poll` 1 ms, `ready_timeout` 20 ms, a fresh `SecretKey::generate()`.

**Tests (in `provision.rs` and `delete.rs`):**
- `provision_happy_path`: status `ready`; one endpoint with role `postgres` (verifier starts with `SCRAM-SHA-256$`) and database `<sanitized>` owned by `postgres`; the tenant exists; the compute token's `sha256_hex` equals the stored hash; the bootstrap ran on the new database, contains `CREATE ROLE anon NOLOGIN` and no `SUPERUSER`; the sealed password opens to 32 alphanumerics; the journal order is create_endpoint < create_tenant < create_timeline < compute.create < sql SELECT 1 < sql DO < set_ready.
- `superuser_switch_appends_alter_role`.
- `waits_for_readiness`: `sql_failures_left = 3` still ends `ready`.
- `a_failure_at_each_step_cleans_up_and_names_the_step`, one case per step:
  - create_endpoint: nothing torn down;
  - create_tenant, create_timeline, compute create, readiness (`sql_failures_left = u32::MAX`), bootstrap (`fail_sql_containing("DO $$")`): each gives compute.delete, delete_tenant and delete_endpoint in the journal, no endpoint left, and `failed` with the error starting with the step's prefix.
- `a_teardown_failure_leaves_the_endpoint_for_reconcile` (Review Focus 3).
- `a_delete_seen_at_the_recheck_tears_down_and_marks_deleted` and `a_delete_racing_set_ready_tears_down` (Review Focus 2).
- `already_ready_deleted_or_missing_rows_are_skipped`; `a_reserved_name_fails_without_creating_anything`.
- `delete_tears_down_then_marks_deleted` (order: compute.delete < delete_tenant < delete_endpoint < mark_deleted); `delete_without_an_endpoint_marks_deleted`; `a_failed_teardown_keeps_the_row_deleting_and_errs`.

- [ ] Fakes and tests first; implement; `cargo test -p worker` passes; clippy clean.
- [ ] Commit `worker: ports, fakes, Neon provision and delete`.

---

### Task 5: Worker reset and reconcile

**Files:** create `services/worker/src/reset.rs`; rewrite `services/worker/src/reconcile.rs`.

**Produces (exact):**

```rust
// reset.rs (same impl bounds as Task 4)
pub async fn reset_password(&self, id: DatabaseId) -> Result<(), String>;
/// Reset steps 2–4 for one endpoint; reconcile calls it too.
pub async fn apply_pending(&self, endpoint_id: &str) -> anyhow::Result<()>;
// reconcile.rs
pub const STALE_PROVISION_SECS: i64 = 600;
pub const STALE_DELETE_SECS: i64 = 60;
pub struct Snapshot { pub databases: Vec<LiveDatabase>, pub endpoints: Vec<EndpointState>,
                      pub computes: HashSet<String>, pub tenants: HashSet<String> }
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    FailStale(DatabaseId), RetryDelete(DatabaseId),
    RecreateCompute(String), ApplyPending(String), RemoveCompute(String), RetryTeardown(String),
    ReportUnknownTenant(String), ReportEndpointWithoutDatabase(String),
}
pub fn plan(s: &Snapshot) -> Vec<Action>;
pub fn should_abort_provision(status: DatabaseStatus) -> bool; // unchanged
pub async fn reconcile_once(&self) -> Result<(), String>;       // on Worker
```

**Behavior (reset):**
- Missing gives `"not found"`; not ready gives `"not ready"`; no endpoint gives `"credentials unavailable"`.
- Then new password → `scram::verifier` → seal → `replace_owner_secret`; `false` gives `"not ready"`.
- Then `apply_pending(ep)`; any error is logged with its cause and returns `Err(RESET_NOT_APPLIED)`.

**Behavior (`apply_pending`):**
- A missing endpoint row is `Ok` (deleted meanwhile).
- Read the roles, and remember the `postgres` verifier.
- If `computes.control_addr(ep)` is `Some`, build the config with `cheladb_neon::spec::endpoint_config(&self.storage, …, self.pusher.jwks())` and `push` it.
- Then `cache.password_updated(ep, "postgres")`. 0 receivers is an error: `no proxy received /password_updated; the old password stays cached until its TTL runs out`.
- Then `clear_needs_reconfigure(ep, "postgres", remembered_verifier)`.

**Behavior (reconcile):** `reconcile_once` observes in this order: `computes.list()`, `storage.list_tenant_ids()`, `list_live_for_reconcile()`, `list_endpoints_for_reconcile()`. Rows are always written before their container or tenant exists, so a fresh resource never looks orphaned. `plan` rules, one action each (sort the output for stable tests):

| Condition | Action |
|---|---|
| database pending/provisioning, age > 600 | `FailStale(id)` |
| database deleting, age > 60 | `RetryDelete(id)` |
| database ready, `endpoint_id` set, not in `computes` | `RecreateCompute(ep)` |
| endpoint `needs_reconfigure` and its database ready | `ApplyPending(ep)` |
| endpoint whose database is failed or deleted | `RetryTeardown(ep)` |
| endpoint with no database | `ReportEndpointWithoutDatabase(ep)` (log only) |
| compute in `computes` with no endpoint row | `RemoveCompute(ep)` |
| tenant not among the endpoints' tenant ids | `ReportUnknownTenant(t)` (log only, never delete) |

A running or exited container of a live endpoint gives nothing (Docker's `unless-stopped` owns restarts). Execution:
- `FailStale`: `fail_if_stale`; if true and the record has an endpoint, `teardown`.
- `RetryDelete`: `handle_delete`.
- `RecreateCompute`: new token → `replace_compute_token(ep, sha)` **first** → `computes.create(ep, token)`.
- `ApplyPending`: `apply_pending`.
- `RemoveCompute`: `computes.delete`.
- `RetryTeardown`: load the endpoint row, then `teardown`.
- Reports: `warn!`.

Each action's error is logged, and the next action still runs.

**Tests:**
- `reset_runs_in_order`: journal replace_owner_secret < pusher.push < cache.password_updated < clear_needs_reconfigure; flag false; the verifier and sealed password changed and the sealed one opens to the password whose verifier is stored (check with `scram::verifier_with_salt` against the stored salt, or just that it differs from before and opens).
- `reset_without_a_running_compute_publishes_without_pushing`.
- `a_failed_push_or_publish_keeps_the_flag_and_the_new_password`, three cases: push error, publish error, 0 receivers. Each gives `Err(RESET_NOT_APPLIED)`, flag true, and metadata holding the new secret.
- `reset_refuses_missing_not_ready_and_unlinked`.
- `apply_pending_keeps_the_flag_when_a_newer_reset_landed` (Review Focus 1): `on_push` changes the stored verifier; afterwards the flag is still true.
- `plan`: one test per table row, plus `a_live_endpoint_with_its_compute_needs_nothing`, `a_fresh_provision_is_left_alone`, and `unknown_tenants_are_reported_never_removed`.
- `reconcile_recreates_a_missing_compute_with_a_fresh_token`: the journal has replace_compute_token before compute.create, and the fake's token hashes to the stored hash.
- `reconcile_retries_a_failed_teardown` (Review Focus 3).
- `one_failing_action_does_not_stop_the_rest`.

- [ ] Tests first; implement; `cargo test -p worker` passes; clippy clean.
- [ ] Commit `worker: reset in step 3's order and reconcile over endpoints and computes`.

---

### Task 6: Worker wiring, config and live test

**Files:** create `services/worker/src/config.rs` and `services/worker/tests/neon_live.rs`; rewrite `services/worker/src/main.rs`; delete `roles.rs`, `docker_ops.rs` and `tests/docker_live.rs`; update `lib.rs`.

**Produces (exact):**

```rust
pub struct WorkerConfig { pub storcon_url: String, pub compute_jwt_dir: PathBuf, pub compute_network: String,
    pub control_plane_uri: String, pub valkey_url: String, pub compute_image: String,
    pub mode: cheladb_types::mode::Mode, pub owner_superuser_requested: bool }
impl WorkerConfig {
    pub fn from_env() -> anyhow::Result<Self>;
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> anyhow::Result<Self>;
    pub fn owner_superuser(&self) -> bool; // requested && mode == SelfHosted
}
pub fn check_control_plane_uri(uri: &str) -> anyhow::Result<()>;
pub type RealWorker = Worker<MetadataClient, StorconClient, DockerBackend, ProxyCache, HttpPusher>;
pub fn build(config: &WorkerConfig, metadata: MetadataClient, key: SecretKey) -> anyhow::Result<RealWorker>;
```

**Behavior:**
- `from_lookup`:
  - defaults from the Global Constraints;
  - a missing `CHELADB_COMPUTE_IMAGE` fails with `CHELADB_COMPUTE_IMAGE is not set; just dev and just worker set it from scripts/neon-images.sh`;
  - `CHELADB_DB_OWNER_SUPERUSER` counts only explicit `true|1|yes` (the old `owner_superuser_from_env` rule, moved here);
  - `CHELADB_MODE` is parsed with `Mode::parse`;
  - `Debug` is derived (no secrets in it).
- `check_control_plane_uri`: parses with `reqwest::Url`; needs scheme `http` or `https`, a host, and an explicit port; the error names `CHELADB_CONTROL_PLANE_URI`.
- `build`:
  - `StorconClient::new(url, 0).with_timeout(10 s)`;
  - `DockerBackend::connect(ComputeSettings { image, network, control_plane_uri })`;
  - `ProxyCache::new(valkey_url)`;
  - `HttpPusher` with a 5 s reqwest client and `cheladb_neon::jwt::load_signer(&config.compute_jwt_dir)` (a missing key names `just zitadel-bootstrap`: Review Focus 5);
  - `ready_poll` 500 ms, `ready_timeout` 60 s, `owner_superuser = config.owner_superuser()`.
- `main.rs`:
  - keep the health server, secrets-key check, NATS consumers and reset-password server (queue group, per-request task, `RESET_DEADLINE`), and the 30 s reconcile loop (first pass after one period);
  - all of them now call an `Arc<RealWorker>`;
  - log a warning when the superuser switch is requested in platform mode, and when it's on in self-hosted mode (today's message).

**Tests:**
- `config.rs`: `defaults_are_for_local_dev` (with the image set); `a_missing_image_names_the_recipe`; `blank_values_count_as_unset`; `superuser_only_in_self_hosted_mode` (4 combinations); `control_plane_uri_needs_scheme_host_and_port` (`host.docker.internal:8083` no scheme, `http://h` no port, and `ftp://h:1` are refused; `http://172.17.0.1:8083` is accepted).
- `build_without_jwt_keys_names_the_bootstrap` (tempdir; needs no Docker connection, since `connect_with_local_defaults` doesn't contact anything).
- `tests/neon_live.rs` (`#[ignore]`; needs `just dev` running, since computes pull their spec from the internal listener; run with `--test-threads=1`):
  - insert a pending database on the demo tenant/project, then `handle_provision` gives `ready` within 90 s;
  - `admin_sql` shows the three roles, with `anon` not able to log in or bypass RLS;
  - `reset_password` succeeds; **print the elapsed time**, the flag is false, the verifier changed;
  - `handle_delete`: the container is gone (`status` is `None`), `describe_tenant` is rejected with 404, there's no endpoint row, and the database is `deleted`;
  - on a panic, clean up best-effort (teardown and delete the row).

- [ ] Tests first; implement; `cargo test -p worker`; clippy clean on the workspace.
- [ ] Live: with the stack up (the Neon services via `just neon-up` still work at this point) and `CHELADB_COMPUTE_IMAGE` exported, `cargo test -p worker --test neon_live -- --ignored --test-threads=1`. Record the reset time in the Status. If a reset takes more than about 6 s, raise `RESET_DEADLINE` and the orchestrator's `RESET_TIMEOUT` together, keeping the deadline under the timeout by at least 5 s.
- [ ] Confirm `grep -rn "cheladb_platform\|postgres:17\|PLATFORM_ROLE" services crates tools` is empty (the migrations' history aside).
- [ ] Commit `worker: wire Neon provisioning, config and a live lifecycle test; remove the platform role`.

---

### Task 7: Compose, justfile and scripts

**Files:** `infra/compose/docker-compose.yml`, `justfile`, `scripts/neon-e2e.sh` (rewrite), `scripts/lib/neon-common.sh`, `infra/neon/tests/run.sh`; delete `scripts/neon-compute-demo.sh`.

**Behavior:**
- **Compose:** remove every `profiles: ["neon"]`, and update the comments that say "profile neon" or name `neon-compute-demo`.
- **`deps-up`:**
  1. export `$(scripts/neon-images.sh)`;
  2. `compose up -d`;
  3. `compose wait register-safekeepers` (fail if it exits non-zero: provisioning needs an active safekeeper);
  4. `mkdir -p .cheladb/neon` and `compose cp proxy:/tls/ca.crt .cheladb/neon/proxy-ca.crt`, showing `cp`'s stderr only on failure (the demo script's pattern).
- **`deps-down`:** remove `cheladb-compute-*` containers, then `compose down` (no profile flag).
- Remove the `neon-up`, `neon-down` and `neon-compute-demo` recipes. `neon-ps` drops `--profile neon`. Recipe comments that say "needs just neon-up" now say `just deps-up`.
- **`just dev` and `just worker`:** `eval "$(scripts/neon-images.sh)"` and `export CHELADB_COMPUTE_IMAGE="$COMPUTE_NODE_IMAGE"` before starting the worker.
- **`sweep-dbs`:**
  1. `docker rm -f` every container with label `cheladb.dev/endpoint-id`;
  2. if metadata Postgres is up: for each `tenant_id` in `neon_endpoints`, delete it from the storage controller with a new `delete_tenant <id>` helper in `neon-common.sh` (the demo script's 202-poll loop, moved: 404 or 2xx is done, 60 s cap);
  3. then `DELETE FROM neon_endpoints; DELETE FROM databases;` and print counts.

  Keep the messages' style.
- **`neon-e2e.sh`** goes through the API (needs `just dev`). Keep `pass`/`die`, the `sql`/`http_sql`/`refused`/`http_refused` helpers and the up-front `cargo build`.
  1. Source `.cheladb/zitadel/demo.env`; `TOKEN=$(cargo run -q -p cheladb -- dev token)`.
  2. POST `/v1/databases` on the gateway (`127.0.0.1:8000`) with the demo tenant and project ids and name `e2e_<hex 4>`, then poll GET until `ready` (120 s; `failed` means die with its `error`).
  3. GET `/connection` gives password, host (derive the `ep-…` id from it), database, `http_url` and `uri`.
  4. From here, the step 3 script's checks 1–6: psql retried up to 90 s; create and insert; SQL over HTTP; the serverless driver; a wrong password refused; cancel.
  5. Connect once with the old password (primes the cache). POST `/reset-password` gives 200; GET `/connection` gives the new password. The new one connects at once, and the old one is refused on 4432 and `/sql`.
  6. Read `tenant_id` from `neon_endpoints` (`metadata_sql`). DELETE gives 202; poll GET until 404 (60 s). Then: no `cheladb-compute-<ep>` container; storage controller `GET /control/v1/tenant/<t>` is 404; `SELECT count(*) FROM neon_endpoints WHERE id = :'ep'` is 0.
  - A trap deletes the database through the API on failure, unless `KEEP=1`.
- **`infra/neon/tests/run.sh`:**
  - the service-list check becomes one check that the base config has every service;
  - the published-ports check applies to the Neon services by name instead of by profile;
  - drop the two `neon-down`/`neon-compute-demo` checks and the loop over `neon-compute-demo`;
  - add `deps-up copies the proxy CA` and `dev exports CHELADB_COMPUTE_IMAGE` (via `just --show`).

- [ ] Make the changes; `just neon-test` passes; `bash -n` and `shellcheck` (if installed) on changed scripts.
- [ ] Live: `just dev-down && just down && just deps-up && just dev`, then `just neon-smoke`, `just neon-e2e` and `just auth-e2e` (wait a minute after any earlier auth run). Record the results in the Status.
- [ ] Commit `stack: Neon services start with deps-up; neon-e2e goes through the API; sweep-dbs clears endpoints and tenants`.

---

### Task 8: Studio connection panel

**Files:** `packages/types/src/index.ts` (`ConnectionDetails.http_url: string`), `apps/studio/src/components/database-actions.tsx` and its test.

**Behavior:**
- The panel shows Host, Port, Database, User and Password (masked) as today.
- Add **HTTP URL** (`http_url`, always shown, with its own copy button). The URI is still shown only with the password.
- Below the fields, a short muted note:

  > Local dev: pass the proxy's CA with `sslrootcert=.cheladb/neon/proxy-ca.crt`. If `localtest.me` resolves to `::1` first on your machine, add `hostaddr=127.0.0.1`.

**Tests:**
- Update the `details()` fixture to the proxy form (`ep-0123456789ab.localtest.me`, 4432, the `postgresql://…?sslmode=verify-full` URI, `http_url`).
- `shows_the_http_url_without_the_password`, `copies_the_http_url`, `shows_the_local_dev_note`.
- Existing tests keep passing.

- [ ] Tests first; implement; `pnpm --filter @cheladb/types typecheck`, `pnpm --filter @cheladb/studio typecheck`, `pnpm --filter @cheladb/studio test` and `pnpm --filter @cheladb/studio build` pass.
- [ ] Playwright MCP pass against `just dev`: sign in → create a database → it turns ready → reveal (URI and HTTP URL shown) → reset (a new password is shown) → delete (it leaves the list). Take a screenshot per step into the scratchpad.
- [ ] Commit `studio: proxy connection details, HTTP URL and a local-dev note`.

---

### Task 9: Docs and full verification (docs only; covered by the end-of-step review)

- [ ] **README:** replace the "Each database is its own `postgres:17` container…" section and the roles table with what happens now: Neon storage, one compute per database, reached through the proxy on 4432/4444, the owner role `postgres` (a `neon_superuser` member; superuser only with the self-hosted switch), and the data API roles. Drop `neon-up`/`neon-down` and `neon-compute-demo`; `deps-up` starts everything. Mention the CA file and `hostaddr`.
- [ ] **Roadmap:** update "Where we are", step 4 progress, and "Resume here". Drop the "never run `just sweep-dbs` / touch demo1–3" rule.
- [ ] **Full run:** `cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D warnings`, the ignored live tests (`neon_live`, `live_db`, `docker_backend_live`, worker `neon_live`, `proxy_cache_live`), `just neon-test`, `just neon-smoke`, `just neon-e2e`, `just auth-e2e`, Studio test, typecheck and build. Record the counts in the Status.
- [ ] Commit `docs: step 4 — provisioning on Neon`.

Then run the end-of-step review (whole branch, most capable model), one fix batch with a scoped re-review, the full run again, and pause for the merge go-ahead.
