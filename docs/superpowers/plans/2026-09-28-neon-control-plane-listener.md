# Internal Control-Plane Listener and Neon's Proxy: Implementation Plan (roadmap step 3)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give Neon's proxy, `compute_ctl` and the storage controller a real control plane to call: an internal listener in the orchestrator. Add Valkey, so the proxy can cache role secrets (cleared on every password change) and cancel queries. Then prove the whole path with a hand-made compute that answers through the proxy, over Postgres wire, SQL over HTTP and the serverless driver.

**Architecture:**
- A second axum router in the orchestrator, on `127.0.0.1:8083`. Each caller has its own bearer token; the compute's token is checked by its SHA-256.
- The router depends on two ports, so tests can use fakes: `NeonMetadataPort` (endpoints, roles and databases in three new metadata tables) and `ComputeLocator` (Docker inspect through bollard).
- The storage-controller client already exists; the tests run it against an in-process mock.
- Compose drops the `storcon-hook` stand-in and adds `valkey` (base profile), `proxy-tls` and `proxy`.
- The proxy's role-secret cache stays on (TTL 1 min). A password change publishes `/password_updated` to Valkey, after the compute has been reconfigured.

**Tech Stack:** Rust 2024: axum 0.8, bollard 0.21.1, redis 1.7.1 (for Valkey), reqwest 0.13, sqlx, jsonwebtoken. Valkey 9.1.2. Neon proxy, storage controller and compute image at `infra/neon/NEON_REF`. Bash, psql and curl for the live scripts, and Node 24 with `@neondatabase/serverless`.

**Spec:** `docs/superpowers/specs/2026-09-28-neon-control-plane-listener-design.md`

## Global Constraints

- Internal listener: `CHELADB_INTERNAL_ADDR`, default `127.0.0.1:8083`. It never goes through the gateway.
- Proxy ports: `127.0.0.1:4432` (Postgres wire) and `127.0.0.1:4444` (WebSocket and SQL over HTTP). Valkey: `127.0.0.1:6379`. Every published port is on `127.0.0.1`; `just neon-test` enforces this.
- Proxy flags include:
  - `--project-info-cache=size=10000,ttl=1m,max_roles=10,gc_interval=60m`;
  - `--redis-auth-type=plain` and `--redis-plain=redis://valkey:6379`.
- A password change goes in this order: metadata first, then reconfigure the computes, then publish `/password_updated`. When `PUBLISH` reaches 0 subscribers, that's a failure.
- Valkey:
  - image `valkey/valkey:9.1.2-alpine@sha256:48332870af354a799964c0012ae1194a0bf2bf894eb508f945810596dc2d8d11`;
  - base Compose profile;
  - no auth in local dev;
  - `CHELADB_VALKEY_URL=redis://127.0.0.1:6379`.
- Tokens, local-dev defaults:
  - `CHELADB_PROXY_API_TOKEN=cheladb-dev-proxy-api`
  - `CHELADB_STORCON_HOOK_TOKEN=cheladb-dev-storcon-hook`
  - the compute's token is per endpoint, and only its SHA-256 (hex) is stored.
- Other settings: `CHELADB_STORCON_URL=http://127.0.0.1:1234`, `CHELADB_COMPUTE_JWT_DIR=.cheladb/neon`, `CHELADB_COMPUTE_NETWORK=cheladb_default`.
- Status codes:
  - Hooks answer 200 or 503, never 400/401/403 for our own failures. The storage controller treats those as fatal; 401 is only for a bad token.
  - `wake_compute` answers 503 with `retry_ms: 1000` while the compute isn't running.
  - An unknown role gets 200 with an empty `role_secret`.
- Dependencies:
  - bollard is the workspace pin, **0.21.1**.
  - `redis` is **1.7.1**, the latest, added to the workspace with `default-features = false, features = ["tokio-comp"]`.
  - `@neondatabase/serverless` is **^1.1.0**, the latest (released 2026-04-17, which clears pnpm's minimum release age).
  - Use pnpm, never npx.
- Repo rules:
  - Commit with `git commit`, ending the message with the `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>` and `Claude-Session: https://claude.ai/code/session_01KESSEqEbiuKm7pMwCPe2Wg` trailers.
  - Work on branch `neon-control-plane-listener`; don't merge until the user says so.
  - Never run `cargo fmt` on the workspace; `rustfmt --edition 2024 <file>` only on files this plan creates.
  - Never run `just sweep-dbs`, and never touch databases demo1, demo2 or demo3.
  - Never overwrite `.cheladb/secrets/secrets.key`, the gateway key pair, the cookie key or the new compute-JWT key pair.
- Environment: WSL's ephemeral port range is small. If tests fail binding `127.0.0.1:0`, wait a minute and rerun; it's TIME-WAIT exhaustion, not flakiness.

## Review Focus

1. **The proxy's query strings carry extra parameters** (`session_id`, `application_name`, `options`). Access control and wake must ignore them and answer normally. Test in Task 5.
2. **A hook arrives while the compute is still booting.** `compute_ctl` answers `/configure` with 412. The hook must answer 503 so the storage controller retries, never 4xx. Test in Task 7.
3. **One of several endpoints on a tenant fails to reconfigure.** The others are still pushed, and the hook answers 503. Test in Task 7.
4. **The orchestrator starts before bootstrap has written the compute-JWT key pair.** Startup fails with a message that names `just zitadel-bootstrap`. Test in Task 5.
5. **A password changes while no proxy is subscribed to Valkey** (the proxy is restarting, or Valkey was just recreated). `cheladb neon password-updated` must exit non-zero and say so, rather than report success while the proxy keeps the old secret. Test in Task 4.

---

## File Structure

| File | Responsibility |
|---|---|
| `crates/cheladb-neon/src/jwt.rs` (modify) | `generate_keypair()`, key file names |
| `crates/cheladb-neon/src/compute.rs` (create) | container name, ports, `compute_ctl_args`, `ComputeLocator`, inspect parsing, `DockerLocator` |
| `crates/cheladb-neon/src/proxy_api.rs` (create) | proxy response and error bodies |
| `crates/cheladb-neon/src/reconfigure.rs` (create) | `/configure` body and push |
| `crates/cheladb-neon/src/proxy_cache.rs`, `tests/proxy_cache_live.rs` (create) | `/password_updated` message and publish |
| `crates/cheladb-neon/tests/reconfigure_mock.rs` (create) | push against a mock compute |
| `crates/cheladb-metadata/migrations/20260928000003_neon_endpoints.sql` (create) | three tables |
| `crates/cheladb-metadata/src/neon.rs` (create) | read queries |
| `crates/cheladb-metadata/tests/neon_live.rs` (create) | live queries and cascade |
| `tools/cli/src/neon/mod.rs`, `compute_jwt.rs` (create) | key pair, `neon verifier`, `neon compute-args`, `neon password-updated` |
| `tools/cli/src/{main.rs,paths.rs,dev/bootstrap.rs,dev/env_files.rs,dev/internal_keys.rs}` (modify) | wiring |
| `services/orchestrator/src/internal/{mod.rs,config.rs,proxy.rs,spec.rs,hooks.rs}` (create) | the internal listener |
| `services/orchestrator/src/{lib.rs,main.rs}` (modify) | module and second listener |
| `services/orchestrator/tests/internal.rs` (create) | route tests with fakes |
| `infra/compose/docker-compose.yml`, `.env.example`, `justfile`, `infra/neon/tests/run.sh` (modify) | proxy, hooks, recipes, offline checks |
| `scripts/neon-compute-demo.sh`, `scripts/neon-e2e.sh` (create) | live demo and e2e |
| `packages/neon-e2e/{package.json,check.mjs}` (create) | serverless-driver check |
| `README.md`, `docs/roadmap/2026-09-28-neon-and-data-api.md`, `scripts/neon-smoke.sh` (modify) | docs |

---

### Task 1: `cheladb-neon`: key generation, compute basics, proxy bodies

**Files:**
- Modify: `crates/cheladb-neon/Cargo.toml`, `crates/cheladb-neon/src/lib.rs`, `crates/cheladb-neon/src/jwt.rs`
- Create: `crates/cheladb-neon/src/compute.rs`, `crates/cheladb-neon/src/proxy_api.rs`

**Interfaces:**
- Produces:
  - `cheladb_neon::jwt::{generate_keypair() -> (String, String), PRIVATE_KEY_FILE: &str = "compute-jwt.pem", PUBLIC_KEY_FILE: &str = "compute-jwt.pub"}`
  - `cheladb_neon::compute::{POSTGRES_PORT: u16, COMPUTE_CTL_PORT: u16, COMPUTE_TOKEN_ENV: &str, ENDPOINT_LABEL: &str, container_name(&str) -> String, compute_ctl_args(endpoint_id: &str, control_plane_uri: &str) -> Vec<String>}`
  - `cheladb_neon::proxy_api::{Reason, access_control(project_id: &str, role_secret: &str) -> Value, wake_compute(ip: IpAddr, endpoint_id: &str, timeline_id: &str) -> Value, error(reason: Reason, message: &str, retry_ms: Option<u64>) -> Value}`

- [x] **Step 1: Move `ed25519-dalek` to `[dependencies]`**

In `crates/cheladb-neon/Cargo.toml`:
- add `ed25519-dalek = { workspace = true }` under `[dependencies]`, in alphabetical order after `base64`;
- remove it from `[dev-dependencies]`.

- [x] **Step 2: Write the failing tests**

In `crates/cheladb-neon/src/jwt.rs`, replace the test helper `test_keypair()` and its imports:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signs_tokens_verifiable_with_the_published_jwk() {
        let (pem, x) = generate_keypair();
```

Then change every other `test_keypair()` call in that module to `generate_keypair()`, and add:

```rust
    #[test]
    fn generated_key_pairs_are_fresh_and_work() {
        let (pem_a, x_a) = generate_keypair();
        let (_, x_b) = generate_keypair();
        assert_ne!(x_a, x_b);
        assert_eq!(x_a.len(), 43, "32 bytes, base64url without padding");
        assert!(ComputeSigner::new(&pem_a, &x_a).is_ok());
    }
```

Create `crates/cheladb-neon/src/compute.rs`:

```rust
//! One compute (Postgres plus compute_ctl) per endpoint, run as a Docker container.

/// Postgres inside a compute.
pub const POSTGRES_PORT: u16 = 5432;
/// compute_ctl's external HTTP API (`/configure`).
pub const COMPUTE_CTL_PORT: u16 = 3080;
/// compute_ctl presents this variable's value when it fetches its spec.
pub const COMPUTE_TOKEN_ENV: &str = "NEON_CONTROL_PLANE_TOKEN";
/// Container label naming the endpoint a compute belongs to.
pub const ENDPOINT_LABEL: &str = "cheladb.dev/endpoint-id";

const NAME_PREFIX: &str = "cheladb-compute-";

/// Container names are global on a Docker host; the prefix keeps ours recognisable.
pub fn container_name(endpoint_id: &str) -> String {
    format!("{NAME_PREFIX}{endpoint_id}")
}

/// compute_ctl's arguments. It pulls its spec from
/// `{control_plane_uri}/compute/api/v2/computes/{endpoint_id}/spec`, presenting
/// [`COMPUTE_TOKEN_ENV`].
pub fn compute_ctl_args(endpoint_id: &str, control_plane_uri: &str) -> Vec<String> {
    vec![
        "--pgbin=/usr/local/bin/postgres".to_string(),
        "--pgdata=/var/db/postgres/compute".to_string(),
        format!("--connstr=postgresql://cloud_admin@localhost:{POSTGRES_PORT}/postgres"),
        format!("--compute-id={endpoint_id}"),
        format!("--control-plane-uri={control_plane_uri}"),
        format!("--external-http-port={COMPUTE_CTL_PORT}"),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn containers_are_named_after_their_endpoint() {
        assert_eq!(container_name("ep-abc123"), "cheladb-compute-ep-abc123");
    }

    #[test]
    fn compute_ctl_pulls_its_spec_from_the_control_plane() {
        let args = compute_ctl_args("ep-abc123", "http://host.docker.internal:8083");
        assert!(args.contains(&"--compute-id=ep-abc123".to_string()));
        assert!(args.contains(&"--control-plane-uri=http://host.docker.internal:8083".to_string()));
        assert!(args.contains(&"--external-http-port=3080".to_string()));
        assert!(args.contains(&"--pgdata=/var/db/postgres/compute".to_string()));
    }
}
```

Create `crates/cheladb-neon/src/proxy_api.rs` with the tests first and the bodies left `todo!()`:

```rust
//! Bodies for the control-plane API that Neon's proxy calls with `--auth-backend=control-plane`.
//! The shapes follow neon `proxy/src/control_plane/messages.rs` at `infra/neon/NEON_REF`:
//! `GetEndpointAccessControl`, `WakeCompute` and `ControlPlaneErrorMessage`.

use std::net::{IpAddr, SocketAddr};

use serde_json::{Value, json};

use crate::compute::POSTGRES_PORT;

/// `reason` values we send; the proxy reads any value it doesn't know as `UNKNOWN`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    EndpointNotFound,
    Unknown,
}

impl Reason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::EndpointNotFound => "ENDPOINT_NOT_FOUND",
            Self::Unknown => "UNKNOWN",
        }
    }
}

/// `GET get_endpoint_access_control`. With an empty `role_secret`, the proxy fails the login
/// exactly as it does for a wrong password, so role names can't be probed.
pub fn access_control(project_id: &str, role_secret: &str) -> Value {
    todo!()
}

/// `GET wake_compute`. Until step 4 has real ids, `project_id` carries the endpoint id and
/// `branch_id` the timeline id.
pub fn wake_compute(ip: IpAddr, endpoint_id: &str, timeline_id: &str) -> Value {
    todo!()
}

/// An error body. With `retry_ms`, the proxy retries after that delay instead of failing
/// the connection.
pub fn error(reason: Reason, message: &str, retry_ms: Option<u64>) -> Value {
    todo!()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn access_control_carries_the_fields_the_proxy_reads() {
        let v = access_control("ep-a", "SCRAM-SHA-256$x");
        assert_eq!(v["role_secret"], "SCRAM-SHA-256$x");
        assert_eq!(v["project_id"], "ep-a");
        for key in [
            "account_id",
            "allowed_ips",
            "allowed_vpc_endpoint_ids",
            "block_public_connections",
            "block_vpc_connections",
        ] {
            assert!(v[key].is_null(), "{key}");
            assert!(v.as_object().unwrap().contains_key(key), "{key} present");
        }
    }

    #[test]
    fn wake_compute_points_at_postgres_on_the_compute() {
        let v = wake_compute("172.20.0.9".parse().unwrap(), "ep-a", "0123");
        assert_eq!(v["address"], "172.20.0.9:5432");
        assert!(v["server_name"].is_null());
        assert_eq!(v["aux"]["endpoint_id"], "ep-a");
        assert_eq!(v["aux"]["project_id"], "ep-a");
        assert_eq!(v["aux"]["branch_id"], "0123");
        assert_eq!(v["aux"]["compute_id"], "ep-a");
        assert_eq!(v["aux"]["cold_start_info"], "warm");
    }

    #[test]
    fn errors_carry_a_reason_and_an_optional_retry() {
        let v = error(Reason::Unknown, "compute is starting", Some(1000));
        assert_eq!(v["error"], "compute is starting");
        assert_eq!(v["status"]["details"]["error_info"]["reason"], "UNKNOWN");
        assert_eq!(v["status"]["details"]["retry_info"]["retry_delay_ms"], 1000);
        assert_eq!(
            v["status"]["details"]["user_facing_message"]["message"],
            "compute is starting"
        );
        assert!(v["status"]["code"].is_string());
        assert!(v["status"]["message"].is_string());

        let v = error(Reason::EndpointNotFound, "gone", None);
        assert_eq!(v["status"]["details"]["error_info"]["reason"], "ENDPOINT_NOT_FOUND");
        assert!(v["status"]["details"]["retry_info"].is_null());
    }
}
```

In `crates/cheladb-neon/src/lib.rs`, add `pub mod compute;` and `pub mod proxy_api;` (alphabetical among the existing `pub mod` lines), and change the crate doc's first line to:

```rust
//! Neon-facing building blocks for the Cheladb control plane: the storage-controller
//! client, the compute spec compute_ctl pulls, the proxy API bodies, compute containers,
//! SCRAM verifiers, LSNs, compute JWTs and ids.
```

- [x] **Step 3: Run the tests to verify they fail**

Run: `cargo test -p cheladb-neon --lib`
Expected: compile error, `cannot find function generate_keypair`. Once that's added, the proxy_api tests panic at `todo!()`.

- [x] **Step 4: Implement**

In `jwt.rs`, after the `TOKEN_TTL_SECS` const, add:

```rust
/// File names of the key pair `cheladb dev bootstrap` writes to `.cheladb/neon/`.
pub const PRIVATE_KEY_FILE: &str = "compute-jwt.pem";
pub const PUBLIC_KEY_FILE: &str = "compute-jwt.pub";
```

After `impl ComputeSigner { … }`, add:

```rust
/// A fresh Ed25519 key pair for [`ComputeSigner::new`]: the PKCS#8 PEM private key and the
/// base64url (no padding) public key.
pub fn generate_keypair() -> (String, String) {
    use base64::Engine;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use ed25519_dalek::SigningKey;
    use ed25519_dalek::pkcs8::EncodePrivateKey;
    use ed25519_dalek::pkcs8::spki::der::pem::LineEnding;

    let mut seed = [0u8; 32];
    getrandom::fill(&mut seed).expect("OS random number generator");
    let key = SigningKey::from_bytes(&seed);
    let pem = key
        .to_pkcs8_pem(LineEnding::LF)
        .expect("PKCS#8 PEM")
        .to_string();
    (pem, URL_SAFE_NO_PAD.encode(key.verifying_key().to_bytes()))
}
```

In `proxy_api.rs`, replace the three `todo!()` bodies:

```rust
pub fn access_control(project_id: &str, role_secret: &str) -> Value {
    json!({
        "role_secret": role_secret,
        "project_id": project_id,
        "account_id": null,
        "allowed_ips": null,
        "allowed_vpc_endpoint_ids": null,
        "block_public_connections": null,
        "block_vpc_connections": null,
    })
}

pub fn wake_compute(ip: IpAddr, endpoint_id: &str, timeline_id: &str) -> Value {
    json!({
        "address": SocketAddr::new(ip, POSTGRES_PORT).to_string(),
        "server_name": null,
        "aux": {
            "endpoint_id": endpoint_id,
            "project_id": endpoint_id,
            "branch_id": timeline_id,
            "compute_id": endpoint_id,
            "cold_start_info": "warm",
        },
    })
}

pub fn error(reason: Reason, message: &str, retry_ms: Option<u64>) -> Value {
    json!({
        "error": message,
        "status": {
            "code": reason.as_str(),
            "message": message,
            "details": {
                "error_info": { "reason": reason.as_str() },
                "retry_info": retry_ms.map(|ms| json!({ "retry_delay_ms": ms })),
                "user_facing_message": { "message": message },
            },
        },
    })
}
```

- [x] **Step 5: Run the tests and format the new files**

Run: `cargo test -p cheladb-neon --lib && rustfmt --edition 2024 crates/cheladb-neon/src/compute.rs crates/cheladb-neon/src/proxy_api.rs`
Expected: all pass.

- [x] **Step 6: Commit**

```bash
git add crates/cheladb-neon
git commit -m "cheladb-neon: compute key generation, compute basics, proxy API bodies

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01KESSEqEbiuKm7pMwCPe2Wg"
```

---

### Task 2: `cheladb-neon`: compute locator (bollard), reconfigure, proxy cache

**Files:**
- Modify: `crates/cheladb-neon/Cargo.toml`, `crates/cheladb-neon/src/compute.rs`, `crates/cheladb-neon/src/lib.rs`
- Create: `crates/cheladb-neon/src/reconfigure.rs`, `crates/cheladb-neon/tests/reconfigure_mock.rs`, `crates/cheladb-neon/src/proxy_cache.rs`, `crates/cheladb-neon/tests/proxy_cache_live.rs`
- Modify: root `Cargo.toml` (the `redis` pin)

**Interfaces:**
- Consumes: `compute::{COMPUTE_CTL_PORT, container_name}`, `jwt::ComputeSigner` (Task 1)
- Produces:
  - `cheladb_neon::compute::ComputeLocator`, a trait with two methods:
    - `fn address(&self, endpoint_id: &str) -> impl Future<Output = anyhow::Result<Option<IpAddr>>> + Send`
    - `fn control_addr(&self, endpoint_id: &str) -> impl Future<Output = anyhow::Result<Option<SocketAddr>>> + Send`
  - `cheladb_neon::compute::{ip_on_network, published_control_addr}`
  - `cheladb_neon::compute::DockerLocator::connect(network: &str) -> anyhow::Result<DockerLocator>`
  - `cheladb_neon::reconfigure::{configure_body(&Value) -> Value, push(http: &reqwest::Client, control_addr: SocketAddr, signer: &ComputeSigner, endpoint_id: &str, config_response: &Value) -> anyhow::Result<()>}`
  - `cheladb_neon::proxy_cache::{CHANNEL, password_updated_message(endpoint_id: &str, role: &str) -> String, ProxyCache::new(url: &str) -> anyhow::Result<ProxyCache>, ProxyCache::password_updated(&self, endpoint_id: &str, role: &str) -> anyhow::Result<u64>}`

- [x] **Step 1: Add bollard and redis**

- In the root `Cargo.toml` `[workspace.dependencies]`, add `redis = { version = "1.7.1", default-features = false, features = ["tokio-comp"] }`, in alphabetical order.
- In `crates/cheladb-neon/Cargo.toml` `[dependencies]`, add `bollard = { workspace = true }` after `base64`, and `redis = { workspace = true }` after `pbkdf2`.
- In `crates/cheladb-neon/Cargo.toml` `[dev-dependencies]`, add `futures-util = { workspace = true }`.

- [x] **Step 2: Write the failing tests**

Append to the `tests` module in `compute.rs`:

```rust
    use bollard::models::{
        ContainerInspectResponse, ContainerState, ContainerStateStatusEnum, EndpointSettings,
        NetworkSettings, PortBinding,
    };
    use std::collections::HashMap;

    fn inspect(status: ContainerStateStatusEnum, network: &str, ip: &str) -> ContainerInspectResponse {
        ContainerInspectResponse {
            state: Some(ContainerState {
                status: Some(status),
                ..Default::default()
            }),
            network_settings: Some(NetworkSettings {
                networks: Some(HashMap::from([(
                    network.to_string(),
                    EndpointSettings {
                        ip_address: Some(ip.to_string()),
                        ..Default::default()
                    },
                )])),
                ports: Some(HashMap::from([(
                    "3080/tcp".to_string(),
                    Some(vec![PortBinding {
                        host_ip: Some("127.0.0.1".to_string()),
                        host_port: Some("49153".to_string()),
                    }]),
                )])),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn a_running_compute_has_an_ip_on_the_compute_network() {
        let running = inspect(ContainerStateStatusEnum::RUNNING, "cheladb_default", "172.20.0.9");
        assert_eq!(
            ip_on_network(&running, "cheladb_default"),
            Some("172.20.0.9".parse().unwrap())
        );
        assert_eq!(ip_on_network(&running, "bridge"), None, "only the named network");
    }

    #[test]
    fn a_running_compute_publishes_compute_ctl_on_loopback() {
        let running = inspect(ContainerStateStatusEnum::RUNNING, "cheladb_default", "172.20.0.9");
        assert_eq!(
            published_control_addr(&running),
            Some("127.0.0.1:49153".parse().unwrap())
        );
    }

    #[test]
    fn a_stopped_compute_has_neither() {
        let exited = inspect(ContainerStateStatusEnum::EXITED, "cheladb_default", "172.20.0.9");
        assert_eq!(ip_on_network(&exited, "cheladb_default"), None);
        assert_eq!(published_control_addr(&exited), None);
        let blank = inspect(ContainerStateStatusEnum::RUNNING, "cheladb_default", "");
        assert_eq!(ip_on_network(&blank, "cheladb_default"), None, "an empty IP is no IP");
    }

    #[test]
    fn a_binding_on_another_interface_is_not_used() {
        let mut running = inspect(ContainerStateStatusEnum::RUNNING, "n", "172.20.0.9");
        let ports = running.network_settings.as_mut().unwrap().ports.as_mut().unwrap();
        ports.insert(
            "3080/tcp".to_string(),
            Some(vec![PortBinding {
                host_ip: Some("192.168.1.5".to_string()),
                host_port: Some("49153".to_string()),
            }]),
        );
        assert_eq!(published_control_addr(&running), None);
    }
```

Create `crates/cheladb-neon/tests/reconfigure_mock.rs`:

```rust
//! `reconfigure::push` against an in-process compute_ctl stand-in.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use cheladb_neon::ComputeSigner;
use cheladb_neon::jwt::generate_keypair;
use cheladb_neon::reconfigure::{configure_body, push};
use jsonwebtoken::{Algorithm, DecodingKey, Validation};
use serde_json::{Value, json};

#[derive(Default)]
struct Seen {
    auth: Mutex<Option<String>>,
    body: Mutex<Option<Value>>,
}

async fn compute(status: u16) -> (SocketAddr, Arc<Seen>) {
    let seen = Arc::new(Seen::default());
    let app = Router::new()
        .route(
            "/configure",
            post(
                move |State(seen): State<Arc<Seen>>, headers: HeaderMap, body: axum::Json<Value>| async move {
                    *seen.auth.lock().unwrap() = headers
                        .get("authorization")
                        .map(|v| v.to_str().unwrap().to_string());
                    *seen.body.lock().unwrap() = Some(body.0);
                    (StatusCode::from_u16(status).unwrap(), "{}")
                },
            ),
        )
        .with_state(seen.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (addr, seen)
}

fn config() -> Value {
    json!({
        "status": "attached",
        "spec": { "cluster": { "cluster_id": "ep-a" } },
        "compute_ctl_config": { "jwks": { "keys": [] } },
    })
}

#[test]
fn the_body_is_the_spec_and_the_compute_ctl_config() {
    let body = configure_body(&config());
    assert_eq!(body["spec"]["cluster"]["cluster_id"], "ep-a");
    assert!(body["compute_ctl_config"]["jwks"]["keys"].is_array());
    assert!(body.get("status").is_none());
}

#[tokio::test]
async fn pushes_the_spec_with_a_token_only_that_compute_accepts() {
    let (pem, x) = generate_keypair();
    let signer = ComputeSigner::new(&pem, &x).unwrap();
    let (addr, seen) = compute(200).await;
    push(&reqwest::Client::new(), addr, &signer, "ep-a", &config())
        .await
        .unwrap();

    let body = seen.body.lock().unwrap().clone().unwrap();
    assert_eq!(body, configure_body(&config()));
    let auth = seen.auth.lock().unwrap().clone().unwrap();
    let token = auth.strip_prefix("Bearer ").expect("bearer token");
    let jwks: jsonwebtoken::jwk::JwkSet = serde_json::from_value(signer.jwks()).unwrap();
    let key = DecodingKey::from_jwk(&jwks.keys[0]).unwrap();
    let claims = jsonwebtoken::decode::<Value>(token, &key, &Validation::new(Algorithm::EdDSA))
        .unwrap()
        .claims;
    assert_eq!(claims["compute_id"], "ep-a");
}

// compute_ctl answers 412 while it is still starting; that must be an error the caller sees.
#[tokio::test]
async fn a_compute_that_refuses_is_an_error() {
    let (pem, x) = generate_keypair();
    let signer = ComputeSigner::new(&pem, &x).unwrap();
    let (addr, _) = compute(412).await;
    let err = push(&reqwest::Client::new(), addr, &signer, "ep-a", &config())
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("412"), "{err}");
}
```

Create `crates/cheladb-neon/src/proxy_cache.rs`, with the test first and `todo!()` bodies:

```rust
//! Clearing Neon's proxy role-secret cache. The control plane publishes on the channel every
//! proxy subscribes to (neon `proxy/src/redis/notifications.rs` at `infra/neon/NEON_REF`).
//! The proxy drops the role at once, and again 20 s later in case a lookup was in flight.

use anyhow::Context;
use serde_json::json;

/// The channel the proxy subscribes to.
pub const CHANNEL: &str = "neondb-proxy-ws-updates";

/// `/password_updated` for one role. `data` is itself a JSON string, and `project_id` is the
/// endpoint id our access-control answer gives the proxy.
pub fn password_updated_message(endpoint_id: &str, role: &str) -> String {
    todo!()
}

/// Publishes cache-clearing messages to Valkey (any Redis-protocol server).
#[derive(Clone)]
pub struct ProxyCache {
    client: redis::Client,
}

impl ProxyCache {
    /// `redis://host:port`. Nothing is contacted yet.
    pub fn new(url: &str) -> anyhow::Result<Self> {
        todo!()
    }

    /// Tells every proxy to drop its cached secret for `role`. Returns how many subscribers
    /// got the message; 0 means no proxy is listening, and the old secret stays cached until
    /// its TTL runs out.
    pub async fn password_updated(&self, endpoint_id: &str, role: &str) -> anyhow::Result<u64> {
        todo!()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    // The shape neon's `Notification` parses: tag `topic`, and `data` as a JSON string.
    #[test]
    fn the_message_is_what_the_proxy_parses() {
        let v: Value = serde_json::from_str(&password_updated_message("ep-a", "app")).unwrap();
        assert_eq!(v["topic"], "/password_updated");
        let data: Value = serde_json::from_str(v["data"].as_str().expect("data is a string")).unwrap();
        assert_eq!(data, json!({ "project_id": "ep-a", "role_name": "app" }));
    }

    #[test]
    fn a_bad_url_is_refused() {
        assert!(ProxyCache::new("not a url").is_err());
    }
}
```

Create `crates/cheladb-neon/tests/proxy_cache_live.rs`:

```rust
//! Against a real Valkey (`just deps-up`):
//!
//! ```sh
//! cargo test -p cheladb-neon --test proxy_cache_live -- --ignored
//! ```

use cheladb_neon::proxy_cache::{CHANNEL, ProxyCache, password_updated_message};
use futures_util::StreamExt;

fn url() -> String {
    std::env::var("CHELADB_VALKEY_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379".into())
}

#[tokio::test]
#[ignore = "needs Valkey (just deps-up)"]
async fn a_subscriber_gets_the_message_and_is_counted() {
    let client = redis::Client::open(url()).unwrap();
    let mut sub = client.get_async_pubsub().await.expect("Valkey reachable (just deps-up)");
    sub.subscribe(CHANNEL).await.unwrap();

    let receivers = ProxyCache::new(&url())
        .unwrap()
        .password_updated("ep-live", "app")
        .await
        .unwrap();
    assert!(receivers >= 1, "at least this test's subscriber");

    let msg = tokio::time::timeout(std::time::Duration::from_secs(5), sub.on_message().next())
        .await
        .expect("a message within 5 s")
        .unwrap();
    let payload: String = msg.get_payload().unwrap();
    assert_eq!(payload, password_updated_message("ep-live", "app"));
}
```

Only the proxy and this test subscribe to `CHANNEL`. The endpoint `ep-live` doesn't exist, so the proxy ignores the message.

- [x] **Step 3: Run the tests to verify they fail**

Run: `cargo test -p cheladb-neon`
Expected: compile errors: `ip_on_network`, `published_control_addr` and modules `reconfigure` and `proxy_cache` not found. After they're declared, the `proxy_cache` tests panic at `todo!()`.

- [x] **Step 4: Implement**

At the top of `compute.rs`, below the module doc, add:

```rust
use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use anyhow::Context;
use bollard::Docker;
use bollard::errors::Error as DockerError;
use bollard::models::{ContainerInspectResponse, ContainerStateStatusEnum};
use bollard::query_parameters::InspectContainerOptions;
```

Before `#[cfg(test)]`, add:

```rust
/// Where an endpoint's compute can be reached. Both lookups give `None` when the container is
/// missing or not running.
pub trait ComputeLocator: Send + Sync + 'static {
    /// The compute's IP on the compute network: where the proxy connects.
    fn address(
        &self,
        endpoint_id: &str,
    ) -> impl Future<Output = anyhow::Result<Option<IpAddr>>> + Send;

    /// `127.0.0.1:<published 3080>`: where the host reaches compute_ctl. The host can't
    /// route to container IPs on Docker Desktop.
    fn control_addr(
        &self,
        endpoint_id: &str,
    ) -> impl Future<Output = anyhow::Result<Option<SocketAddr>>> + Send;
}

fn running(inspect: &ContainerInspectResponse) -> bool {
    inspect.state.as_ref().and_then(|s| s.status) == Some(ContainerStateStatusEnum::RUNNING)
}

/// The container's IP on `network`, if it's running.
pub fn ip_on_network(inspect: &ContainerInspectResponse, network: &str) -> Option<IpAddr> {
    if !running(inspect) {
        return None;
    }
    inspect
        .network_settings
        .as_ref()?
        .networks
        .as_ref()?
        .get(network)?
        .ip_address
        .as_deref()?
        .parse()
        .ok()
}

/// The loopback address compute_ctl's port is published on (`-p 127.0.0.1::3080`), if the
/// container is running. An all-interfaces binding is reachable on loopback too.
pub fn published_control_addr(inspect: &ContainerInspectResponse) -> Option<SocketAddr> {
    if !running(inspect) {
        return None;
    }
    let bindings = inspect
        .network_settings
        .as_ref()?
        .ports
        .as_ref()?
        .get(&format!("{COMPUTE_CTL_PORT}/tcp"))?
        .as_ref()?;
    bindings.iter().find_map(|binding| {
        match binding.host_ip.as_deref() {
            None | Some("" | "127.0.0.1" | "0.0.0.0") => {}
            Some(_) => return None,
        }
        let port = binding.host_port.as_deref()?.parse().ok()?;
        Some(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port))
    })
}

/// [`ComputeLocator`] over the Docker Engine API.
pub struct DockerLocator {
    docker: Docker,
    network: String,
}

impl DockerLocator {
    /// `DOCKER_HOST` or the local socket, like the worker. Nothing is contacted yet.
    pub fn connect(network: &str) -> anyhow::Result<Self> {
        Ok(Self {
            docker: Docker::connect_with_local_defaults().context("configuring the Docker client")?,
            network: network.to_string(),
        })
    }

    async fn inspect(&self, endpoint_id: &str) -> anyhow::Result<Option<ContainerInspectResponse>> {
        match self
            .docker
            .inspect_container(&container_name(endpoint_id), None::<InspectContainerOptions>)
            .await
        {
            Ok(inspect) => Ok(Some(inspect)),
            Err(DockerError::DockerResponseServerError {
                status_code: 404, ..
            }) => Ok(None),
            Err(e) => Err(anyhow::Error::new(e).context(format!("inspecting compute {endpoint_id}"))),
        }
    }
}

impl ComputeLocator for DockerLocator {
    async fn address(&self, endpoint_id: &str) -> anyhow::Result<Option<IpAddr>> {
        Ok(self
            .inspect(endpoint_id)
            .await?
            .and_then(|inspect| ip_on_network(&inspect, &self.network)))
    }

    async fn control_addr(&self, endpoint_id: &str) -> anyhow::Result<Option<SocketAddr>> {
        Ok(self
            .inspect(endpoint_id)
            .await?
            .and_then(|inspect| published_control_addr(&inspect)))
    }
}
```

Create `crates/cheladb-neon/src/reconfigure.rs`:

```rust
//! Pushing a new spec to a running compute: `POST http://<control_addr>/configure`
//! (neon `compute_tools/src/http/routes/configure.rs`), with a [`ComputeSigner`] token only
//! that compute accepts.

use std::net::SocketAddr;

use anyhow::Context;
use serde_json::{Value, json};

use crate::jwt::ComputeSigner;

/// The `/configure` body (`ConfigurationRequest`) from a [`crate::spec::config_response`].
pub fn configure_body(config_response: &Value) -> Value {
    json!({
        "spec": config_response["spec"],
        "compute_ctl_config": config_response["compute_ctl_config"],
    })
}

/// compute_ctl answers once the compute has applied the spec. Anything but 2xx is an error:
/// for example 412 while the compute is still starting.
pub async fn push(
    http: &reqwest::Client,
    control_addr: SocketAddr,
    signer: &ComputeSigner,
    endpoint_id: &str,
    config_response: &Value,
) -> anyhow::Result<()> {
    let resp = http
        .post(format!("http://{control_addr}/configure"))
        .bearer_auth(signer.token_for(endpoint_id)?)
        .json(&configure_body(config_response))
        .send()
        .await
        .with_context(|| format!("POST /configure to compute {endpoint_id}"))?;
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        anyhow::bail!("compute {endpoint_id} /configure returned {status}: {text}");
    }
    Ok(())
}
```

`proxy_cache.rs` bodies:

```rust
pub fn password_updated_message(endpoint_id: &str, role: &str) -> String {
    json!({
        "topic": "/password_updated",
        "data": json!({ "project_id": endpoint_id, "role_name": role }).to_string(),
    })
    .to_string()
}
```

```rust
    pub fn new(url: &str) -> anyhow::Result<Self> {
        Ok(Self {
            client: redis::Client::open(url).context("Valkey URL")?,
        })
    }

    pub async fn password_updated(&self, endpoint_id: &str, role: &str) -> anyhow::Result<u64> {
        let mut conn = self
            .client
            .get_multiplexed_async_connection()
            .await
            .context("connecting to Valkey")?;
        let receivers: u64 = redis::cmd("PUBLISH")
            .arg(CHANNEL)
            .arg(password_updated_message(endpoint_id, role))
            .query_async(&mut conn)
            .await
            .context("PUBLISH to Valkey")?;
        Ok(receivers)
    }
```

If redis 1.7's async API names differ (`get_multiplexed_async_connection`, `get_async_pubsub`, `on_message`), use the 1.7 equivalents from its docs; the behaviour stays the same.

In `lib.rs`, add `pub mod proxy_cache;` and `pub mod reconfigure;`, and `pub use compute::{ComputeLocator, DockerLocator};` next to the other `pub use` lines.

- [x] **Step 5: Run the tests, clippy and rustfmt**

Run: `cargo test -p cheladb-neon && cargo clippy -p cheladb-neon --all-targets -- -D warnings && rustfmt --edition 2024 crates/cheladb-neon/src/compute.rs crates/cheladb-neon/src/reconfigure.rs crates/cheladb-neon/src/proxy_cache.rs crates/cheladb-neon/tests/reconfigure_mock.rs crates/cheladb-neon/tests/proxy_cache_live.rs`
Expected: all pass, no warnings. `storcon_live` and `proxy_cache_live` stay ignored. Valkey isn't in Compose until Task 8; run `proxy_cache_live` there.

- [x] **Step 6: Commit**

```bash
git add Cargo.toml crates/cheladb-neon Cargo.lock
git commit -m "cheladb-neon: locate computes through Docker; push specs; clear the proxy's cache

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01KESSEqEbiuKm7pMwCPe2Wg"
```

---

### Task 3: Metadata: Neon endpoint tables and queries

**Files:**
- Create: `crates/cheladb-metadata/migrations/20260928000003_neon_endpoints.sql`, `crates/cheladb-metadata/src/neon.rs`, `crates/cheladb-metadata/tests/neon_live.rs`
- Modify: `crates/cheladb-metadata/src/lib.rs`

**Interfaces:**
- Produces:
  - `cheladb_metadata::{NeonEndpoint { id, tenant_id, timeline_id, compute_token_sha256: String }, NeonRole { name, scram_verifier: String }, NeonDatabase { name, owner: String }}`
  - Methods on `MetadataClient`:
    - `neon_endpoint(&self, id: &str) -> Result<Option<NeonEndpoint>, MetadataError>`
    - `neon_role_secret(&self, endpoint_id: &str, role: &str) -> Result<Option<Option<String>>, MetadataError>`. The outer `None` means an unknown endpoint; `Some(None)` means an unknown role.
    - `neon_roles(&self, endpoint_id: &str) -> Result<Vec<NeonRole>, MetadataError>`
    - `neon_databases(&self, endpoint_id: &str) -> Result<Vec<NeonDatabase>, MetadataError>`
    - `neon_endpoints_for_tenant(&self, tenant_id: &str) -> Result<Vec<String>, MetadataError>`
    - `neon_endpoints_for_timeline(&self, timeline_id: &str) -> Result<Vec<String>, MetadataError>`

- [x] **Step 1: Write the migration**

`crates/cheladb-metadata/migrations/20260928000003_neon_endpoints.sql`:

```sql
-- Neon endpoints (roadmap step 3). The proxy, compute_ctl and the storage controller ask the
-- orchestrator's internal listener about these. Step 4 links them to `databases`.
CREATE TABLE neon_endpoints (
    id                   text PRIMARY KEY,           -- 'ep-' + 12 hex
    tenant_id            text NOT NULL,              -- 32 hex
    timeline_id          text NOT NULL,              -- 32 hex
    compute_token_sha256 text NOT NULL,              -- hex SHA-256 of compute_ctl's token
    created_at           timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX ON neon_endpoints (tenant_id);
CREATE INDEX ON neon_endpoints (timeline_id);

-- Keyed by endpoint, not branch: each branch gets one endpoint, so step 5 copies rows.
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

- [x] **Step 2: Write the failing live tests**

`crates/cheladb-metadata/tests/neon_live.rs`:

```rust
//! Opt-in tests of the Neon endpoint queries against a real metadata Postgres
//! (`just deps-up` first):
//!
//! ```sh
//! cargo test -p cheladb-metadata --test neon_live -- --ignored --test-threads=1
//! ```
//!
//! Each test creates its own endpoint and deletes it.

use cheladb_metadata::{MetadataClient, MetadataConfig, NeonDatabase, NeonEndpoint, NeonRole};

async fn client() -> MetadataClient {
    let database_url = std::env::var("CHELADB_METADATA_DATABASE_URL").unwrap_or_else(|_| {
        "postgres://cheladb:cheladb@127.0.0.1:5433/cheladb_metadata".to_string()
    });
    let client = MetadataClient::connect(&MetadataConfig { database_url })
        .await
        .expect("metadata Postgres reachable (just deps-up)");
    client.migrate().await.unwrap();
    client
}

fn hex32() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// An endpoint with roles `app` and `ro`, and database `app` owned by `app`.
async fn endpoint(client: &MetadataClient) -> NeonEndpoint {
    let ep = NeonEndpoint {
        id: format!("ep-{}", &hex32()[..12]),
        tenant_id: hex32(),
        timeline_id: hex32(),
        compute_token_sha256: "ab".repeat(32),
    };
    let pool = client.pool();
    sqlx::query("INSERT INTO neon_endpoints (id, tenant_id, timeline_id, compute_token_sha256) VALUES ($1, $2, $3, $4)")
        .bind(&ep.id).bind(&ep.tenant_id).bind(&ep.timeline_id).bind(&ep.compute_token_sha256)
        .execute(pool).await.unwrap();
    for (name, verifier) in [("app", "SCRAM-SHA-256$app"), ("ro", "SCRAM-SHA-256$ro")] {
        sqlx::query("INSERT INTO neon_roles (endpoint_id, name, scram_verifier) VALUES ($1, $2, $3)")
            .bind(&ep.id).bind(name).bind(verifier)
            .execute(pool).await.unwrap();
    }
    sqlx::query("INSERT INTO neon_databases (endpoint_id, name, owner) VALUES ($1, 'app', 'app')")
        .bind(&ep.id)
        .execute(pool).await.unwrap();
    ep
}

async fn delete(client: &MetadataClient, id: &str) {
    sqlx::query("DELETE FROM neon_endpoints WHERE id = $1")
        .bind(id)
        .execute(client.pool())
        .await
        .unwrap();
}

#[tokio::test]
#[ignore = "needs the metadata Postgres (just deps-up)"]
async fn finds_an_endpoint_its_roles_and_databases() {
    let client = client().await;
    let ep = endpoint(&client).await;

    assert_eq!(client.neon_endpoint(&ep.id).await.unwrap(), Some(ep.clone()));
    assert_eq!(client.neon_endpoint("ep-missing").await.unwrap(), None);
    assert_eq!(
        client.neon_roles(&ep.id).await.unwrap(),
        vec![
            NeonRole { name: "app".into(), scram_verifier: "SCRAM-SHA-256$app".into() },
            NeonRole { name: "ro".into(), scram_verifier: "SCRAM-SHA-256$ro".into() },
        ]
    );
    assert_eq!(
        client.neon_databases(&ep.id).await.unwrap(),
        vec![NeonDatabase { name: "app".into(), owner: "app".into() }]
    );
    assert_eq!(client.neon_endpoints_for_tenant(&ep.tenant_id).await.unwrap(), vec![ep.id.clone()]);
    assert_eq!(client.neon_endpoints_for_timeline(&ep.timeline_id).await.unwrap(), vec![ep.id.clone()]);
    assert!(client.neon_endpoints_for_tenant(&hex32()).await.unwrap().is_empty());

    delete(&client, &ep.id).await;
}

#[tokio::test]
#[ignore = "needs the metadata Postgres (just deps-up)"]
async fn role_secrets_tell_unknown_endpoints_from_unknown_roles() {
    let client = client().await;
    let ep = endpoint(&client).await;

    assert_eq!(
        client.neon_role_secret(&ep.id, "app").await.unwrap(),
        Some(Some("SCRAM-SHA-256$app".into()))
    );
    assert_eq!(client.neon_role_secret(&ep.id, "nobody").await.unwrap(), Some(None));
    assert_eq!(client.neon_role_secret("ep-missing", "app").await.unwrap(), None);

    delete(&client, &ep.id).await;
}

#[tokio::test]
#[ignore = "needs the metadata Postgres (just deps-up)"]
async fn deleting_an_endpoint_deletes_its_roles_and_databases() {
    let client = client().await;
    let ep = endpoint(&client).await;
    delete(&client, &ep.id).await;

    assert!(client.neon_roles(&ep.id).await.unwrap().is_empty());
    assert!(client.neon_databases(&ep.id).await.unwrap().is_empty());
    let (left,): (i64,) = sqlx::query_as("SELECT count(*) FROM neon_roles WHERE endpoint_id = $1")
        .bind(&ep.id)
        .fetch_one(client.pool())
        .await
        .unwrap();
    assert_eq!(left, 0);
}
```

- [x] **Step 3: Run the tests to verify they fail**

Run: `cargo test -p cheladb-metadata --test neon_live -- --ignored --test-threads=1`
Expected: compile error, `NeonEndpoint` not found.

- [x] **Step 4: Implement the queries**

`crates/cheladb-metadata/src/neon.rs`:

```rust
//! Neon endpoints, roles and databases: what the proxy, compute_ctl and the storage
//! controller ask the orchestrator's internal listener about.

use crate::{MetadataClient, MetadataError};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NeonEndpoint {
    pub id: String,
    pub tenant_id: String,
    pub timeline_id: String,
    pub compute_token_sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NeonRole {
    pub name: String,
    pub scram_verifier: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NeonDatabase {
    pub name: String,
    pub owner: String,
}

impl MetadataClient {
    pub async fn neon_endpoint(&self, id: &str) -> Result<Option<NeonEndpoint>, MetadataError> {
        let row: Option<(String, String, String, String)> = sqlx::query_as(
            "SELECT id, tenant_id, timeline_id, compute_token_sha256 FROM neon_endpoints WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|(id, tenant_id, timeline_id, compute_token_sha256)| NeonEndpoint {
            id,
            tenant_id,
            timeline_id,
            compute_token_sha256,
        }))
    }

    /// `None` for an unknown endpoint; `Some(None)` for a known endpoint without that role.
    pub async fn neon_role_secret(
        &self,
        endpoint_id: &str,
        role: &str,
    ) -> Result<Option<Option<String>>, MetadataError> {
        let row: Option<(Option<String>,)> = sqlx::query_as(
            "SELECT r.scram_verifier FROM neon_endpoints e
             LEFT JOIN neon_roles r ON r.endpoint_id = e.id AND r.name = $2
             WHERE e.id = $1",
        )
        .bind(endpoint_id)
        .bind(role)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|(verifier,)| verifier))
    }

    pub async fn neon_roles(&self, endpoint_id: &str) -> Result<Vec<NeonRole>, MetadataError> {
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT name, scram_verifier FROM neon_roles WHERE endpoint_id = $1 ORDER BY name",
        )
        .bind(endpoint_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(name, scram_verifier)| NeonRole { name, scram_verifier })
            .collect())
    }

    pub async fn neon_databases(&self, endpoint_id: &str) -> Result<Vec<NeonDatabase>, MetadataError> {
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT name, owner FROM neon_databases WHERE endpoint_id = $1 ORDER BY name",
        )
        .bind(endpoint_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(name, owner)| NeonDatabase { name, owner })
            .collect())
    }

    pub async fn neon_endpoints_for_tenant(&self, tenant_id: &str) -> Result<Vec<String>, MetadataError> {
        Ok(sqlx::query_scalar("SELECT id FROM neon_endpoints WHERE tenant_id = $1 ORDER BY id")
            .bind(tenant_id)
            .fetch_all(&self.pool)
            .await?)
    }

    pub async fn neon_endpoints_for_timeline(
        &self,
        timeline_id: &str,
    ) -> Result<Vec<String>, MetadataError> {
        Ok(sqlx::query_scalar("SELECT id FROM neon_endpoints WHERE timeline_id = $1 ORDER BY id")
            .bind(timeline_id)
            .fetch_all(&self.pool)
            .await?)
    }
}
```

In `crates/cheladb-metadata/src/lib.rs`, after the `use` lines, add:

```rust
mod neon;
pub use neon::{NeonDatabase, NeonEndpoint, NeonRole};
```

`neon` is a child module, so it can read `MetadataClient`'s private `pool` field.

- [x] **Step 5: Run the tests**

Run: `just deps-up` (if needed), then `cargo test -p cheladb-metadata && cargo test -p cheladb-metadata --test neon_live -- --ignored --test-threads=1 && rustfmt --edition 2024 crates/cheladb-metadata/src/neon.rs crates/cheladb-metadata/tests/neon_live.rs`
Expected: all pass. The migration runs on the local metadata database; it only adds tables.

- [x] **Step 6: Commit**

```bash
git add crates/cheladb-metadata
git commit -m "metadata: Neon endpoints, roles and databases

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01KESSEqEbiuKm7pMwCPe2Wg"
```

---

### Task 4: `cheladb` CLI: compute-JWT key pair, `neon verifier`, `neon compute-args`, `neon password-updated`

**Files:**
- Create: `tools/cli/src/neon/mod.rs`, `tools/cli/src/neon/compute_jwt.rs`
- Modify: `tools/cli/Cargo.toml`, `tools/cli/src/main.rs`, `tools/cli/src/paths.rs`, `tools/cli/src/dev/bootstrap.rs`, `tools/cli/src/dev/env_files.rs`, `tools/cli/src/dev/internal_keys.rs`

**Interfaces:**
- Consumes: `cheladb_neon::jwt::{generate_keypair, PRIVATE_KEY_FILE, PUBLIC_KEY_FILE, ComputeSigner}`, `cheladb_neon::scram::verifier`, `cheladb_neon::compute::compute_ctl_args`, `cheladb_neon::proxy_cache::ProxyCache` (Task 2)
- Produces:
  - `cheladb neon verifier`: the password on stdin, one `SCRAM-SHA-256$…` line on stdout.
  - `cheladb neon compute-args --endpoint <ep> --control-plane-uri <uri>`: one argument per line.
  - `cheladb neon password-updated --endpoint <ep> --role <role>`: publishes to `CHELADB_VALKEY_URL` (default `redis://127.0.0.1:6379`) and prints the subscriber count. It exits non-zero when that count is 0.
  - `.cheladb/neon/compute-jwt.{pem,pub}`.
  - `demo.env` gains `CHELADB_COMPUTE_JWT_DIR=<absolute dir>`.

- [ ] **Step 1: Add the dependency, and share `write_private`**

- In `tools/cli/Cargo.toml` `[dependencies]`, add `cheladb-neon = { workspace = true }`, after `cheladb-auth`. The workspace entry already exists.
- In `tools/cli/src/dev/internal_keys.rs`, change `fn write_private(path: &Path, pem: &str)` to `pub(crate) fn write_private(path: &Path, pem: &str)`.

- [ ] **Step 2: Write the failing tests**

`tools/cli/src/neon/compute_jwt.rs`:

```rust
//! The key pair the control plane signs compute_ctl tokens with (`ComputeSigner`).

use anyhow::{Context, Result};
use cheladb_neon::jwt::{PRIVATE_KEY_FILE, PUBLIC_KEY_FILE, generate_keypair};
use std::path::{Path, PathBuf};

use crate::dev::internal_keys::write_private;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComputeJwtPaths {
    pub dir: PathBuf,
    pub private: PathBuf,
    pub public: PathBuf,
}

/// Create the key pair in `dir` if neither file exists. Never overwrites: running computes
/// trust the public key they were started with.
pub fn ensure(dir: &Path) -> Result<ComputeJwtPaths> {
    todo!()
}

#[cfg(test)]
mod tests {
    use super::*;
    use cheladb_neon::ComputeSigner;

    fn signer(paths: &ComputeJwtPaths) -> anyhow::Result<ComputeSigner> {
        ComputeSigner::new(
            &std::fs::read_to_string(&paths.private)?,
            &std::fs::read_to_string(&paths.public)?,
        )
    }

    #[test]
    fn creates_a_key_pair_the_signer_accepts() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ensure(dir.path()).unwrap();
        assert!(paths.dir.is_absolute());
        assert!(signer(&paths).is_ok());
    }

    #[test]
    fn a_second_run_keeps_the_keys() {
        let dir = tempfile::tempdir().unwrap();
        let first = ensure(dir.path()).unwrap();
        let before = std::fs::read(&first.private).unwrap();
        let second = ensure(dir.path()).unwrap();
        assert_eq!(first, second);
        assert_eq!(std::fs::read(&second.private).unwrap(), before);
    }

    #[test]
    fn half_a_key_pair_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ensure(dir.path()).unwrap();
        std::fs::remove_file(&paths.public).unwrap();
        let err = ensure(dir.path()).unwrap_err().to_string();
        assert!(err.contains("delete both"), "{err}");
        assert!(!paths.public.exists(), "must not regenerate half a key pair");
    }

    #[cfg(unix)]
    #[test]
    fn the_private_key_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let paths = ensure(dir.path()).unwrap();
        let mode = std::fs::metadata(&paths.private).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
```

`tools/cli/src/neon/mod.rs`:

```rust
//! `cheladb neon …`: helpers for the Neon compute path (roadmap step 3).

pub mod compute_jwt;

use anyhow::Result;
use std::io::Read;

/// The password from stdin. One trailing newline (from `echo` or a heredoc) is dropped;
/// everything else, spaces included, is part of the password.
pub fn password_from(mut input: impl Read) -> Result<String> {
    todo!()
}

/// `cheladb neon verifier`: prints the SCRAM verifier for the password on stdin.
pub fn verifier() -> Result<()> {
    let password = password_from(std::io::stdin().lock())?;
    println!("{}", cheladb_neon::scram::verifier(&password));
    Ok(())
}

/// Review Focus 5: nobody receiving the message is a failure, not a success.
pub fn check_receivers(receivers: u64) -> Result<()> {
    todo!()
}

/// `cheladb neon password-updated`: tells the proxies to drop a role's cached secret.
pub async fn password_updated(endpoint: &str, role: &str) -> Result<()> {
    let url = std::env::var("CHELADB_VALKEY_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379".into());
    let receivers = cheladb_neon::proxy_cache::ProxyCache::new(&url)?
        .password_updated(endpoint, role)
        .await?;
    check_receivers(receivers)?;
    println!("{receivers}");
    Ok(())
}

/// `cheladb neon compute-args`: compute_ctl's arguments, one per line.
pub fn compute_args(endpoint: &str, control_plane_uri: &str) {
    for arg in cheladb_neon::compute::compute_ctl_args(endpoint, control_plane_uri) {
        println!("{arg}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_trailing_newline_is_dropped() {
        assert_eq!(password_from("pw\n".as_bytes()).unwrap(), "pw");
        assert_eq!(password_from("pw\r\n".as_bytes()).unwrap(), "pw");
        assert_eq!(password_from("pw".as_bytes()).unwrap(), "pw");
        assert_eq!(password_from("pw\n\n".as_bytes()).unwrap(), "pw\n");
    }

    #[test]
    fn spaces_are_part_of_the_password() {
        assert_eq!(password_from(" p w \n".as_bytes()).unwrap(), " p w ");
    }

    #[test]
    fn nobody_listening_is_an_error_that_says_why() {
        let err = check_receivers(0).unwrap_err().to_string();
        assert!(err.contains("no proxy"), "{err}");
        assert!(check_receivers(1).is_ok());
        assert!(check_receivers(3).is_ok());
    }

    #[test]
    fn an_empty_password_is_refused() {
        assert!(password_from("".as_bytes()).is_err());
        assert!(password_from("\n".as_bytes()).is_err());
    }
}
```

In `tools/cli/src/dev/env_files.rs`'s `tests` module, set `compute_jwt_dir: "/abs/neon"` in every existing `DemoEnv { … }` literal, and add:

```rust
    #[test]
    fn demo_env_names_the_compute_jwt_dir() {
        let text = render_demo_env(&DemoEnv {
            issuer: "http://localhost:8080",
            audiences: "p1,c1",
            org_id: "o",
            project_id: "p1",
            api_client_id: "c1",
            demo_user_id: "u",
            demo_tenant: "t",
            signing_key: "/s",
            verify_key: "/v",
            secrets_key: "/k",
            auth_token_file: "/tok",
            auth_cookie_key: "/cookie",
            compute_jwt_dir: "/abs/neon",
        });
        assert!(text.contains("\nCHELADB_COMPUTE_JWT_DIR=/abs/neon\n"), "{text}");
    }
```

Match the field list to the actual `DemoEnv` struct when you edit it.

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test -p cheladb`
Expected: compile errors. The `neon` module isn't declared yet, and `DemoEnv` has no `compute_jwt_dir`.

- [ ] **Step 4: Implement**

`check_receivers`:

```rust
pub fn check_receivers(receivers: u64) -> Result<()> {
    anyhow::ensure!(
        receivers > 0,
        "no proxy is subscribed to Valkey, so none dropped the old secret (is `just neon-up` running?)"
    );
    Ok(())
}
```

`compute_jwt::ensure`:

```rust
pub fn ensure(dir: &Path) -> Result<ComputeJwtPaths> {
    std::fs::create_dir_all(dir).with_context(|| format!("mkdir {}", dir.display()))?;
    let dir = dir
        .canonicalize()
        .with_context(|| format!("resolve {}", dir.display()))?;
    let paths = ComputeJwtPaths {
        private: dir.join(PRIVATE_KEY_FILE),
        public: dir.join(PUBLIC_KEY_FILE),
        dir: dir.clone(),
    };
    match (paths.private.is_file(), paths.public.is_file()) {
        (true, true) => tracing::info!("reusing the compute-JWT key pair in {}", dir.display()),
        (false, false) => {
            let (pem, x) = generate_keypair();
            write_private(&paths.private, &pem)?;
            std::fs::write(&paths.public, format!("{x}\n"))
                .with_context(|| format!("write {}", paths.public.display()))?;
            tracing::info!("wrote the compute-JWT key pair to {}", dir.display());
        }
        _ => anyhow::bail!(
            "only one of {PRIVATE_KEY_FILE} / {PUBLIC_KEY_FILE} exists in {}; delete both to regenerate",
            dir.display()
        ),
    }
    Ok(paths)
}
```

`password_from`:

```rust
pub fn password_from(mut input: impl Read) -> Result<String> {
    let mut text = String::new();
    input.read_to_string(&mut text)?;
    let password = match text.strip_suffix('\n') {
        Some(rest) => rest.strip_suffix('\r').unwrap_or(rest),
        None => &text,
    };
    anyhow::ensure!(!password.is_empty(), "no password on stdin");
    Ok(password.to_string())
}
```

In `paths.rs`, add after `auth_dir`:

```rust
/// Where `cheladb dev bootstrap` keeps the compute-JWT key pair and the step 3 demo files.
pub fn neon_dir() -> PathBuf {
    if let Ok(dir) = env::var("CHELADB_NEON_DIR") {
        return PathBuf::from(dir);
    }
    repo_root().join(".cheladb/neon")
}
```

In `env_files.rs`:
- add `pub compute_jwt_dir: &'a str,` to `DemoEnv`, after `auth_cookie_key`;
- in `render_demo_env`, change the end of the format string from `CHELADB_AUTH_COOKIE_KEY={}\n",` to `CHELADB_AUTH_COOKIE_KEY={}\nCHELADB_COMPUTE_JWT_DIR={}\n",`, and add `e.compute_jwt_dir` after `e.auth_cookie_key`.

In `bootstrap.rs`:
- import `neon_dir` from `crate::paths`, and `crate::neon::compute_jwt`;
- after the `cookie_key` line, add `let compute_jwt = compute_jwt::ensure(&neon_dir())?;`;
- in the `DemoEnv` literal, add `compute_jwt_dir: &path(&compute_jwt.dir),`.

In `main.rs`:
- add `mod neon;` after `mod dev;`;
- add a `Neon` subcommand:

```rust
    /// Neon compute helpers (roadmap step 3)
    Neon {
        #[command(subcommand)]
        command: NeonCommands,
    },
```

```rust
#[derive(Subcommand, Debug)]
enum NeonCommands {
    /// Print the SCRAM-SHA-256 verifier for the password on stdin
    Verifier,
    /// Print compute_ctl's arguments for an endpoint, one per line
    ComputeArgs {
        #[arg(long)]
        endpoint: String,
        #[arg(long)]
        control_plane_uri: String,
    },
    /// Tell the proxies to drop a role's cached secret (after a password change)
    PasswordUpdated {
        #[arg(long)]
        endpoint: String,
        #[arg(long)]
        role: String,
    },
}
```

and in `main`:

```rust
        Commands::Neon { command } => match command {
            NeonCommands::Verifier => neon::verifier(),
            NeonCommands::ComputeArgs {
                endpoint,
                control_plane_uri,
            } => {
                neon::compute_args(&endpoint, &control_plane_uri);
                Ok(())
            }
            NeonCommands::PasswordUpdated { endpoint, role } => {
                neon::password_updated(&endpoint, &role).await
            }
        },
```

- [ ] **Step 5: Run the tests, clippy and a manual check**

Run:
```bash
cargo test -p cheladb && cargo clippy -p cheladb --all-targets -- -D warnings
echo 'pencil' | cargo run -q -p cheladb -- neon verifier
cargo run -q -p cheladb -- neon compute-args --endpoint ep-x --control-plane-uri http://h:8083
rustfmt --edition 2024 tools/cli/src/neon/mod.rs tools/cli/src/neon/compute_jwt.rs
```
Expected: tests pass; one line starting `SCRAM-SHA-256$4096:`; six `--…` lines.

- [ ] **Step 6: Commit**

```bash
git add tools/cli Cargo.lock
git commit -m "cheladb: compute-JWT key pair in bootstrap; neon verifier, compute-args, password-updated

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01KESSEqEbiuKm7pMwCPe2Wg"
```

---

### Task 5: Orchestrator: internal listener, tokens, proxy routes

**Files:**
- Create: `services/orchestrator/src/internal/mod.rs`, `services/orchestrator/src/internal/config.rs`, `services/orchestrator/src/internal/proxy.rs`, `services/orchestrator/tests/internal.rs`
- Modify: `services/orchestrator/Cargo.toml`, `services/orchestrator/src/lib.rs`, `services/orchestrator/src/main.rs`

**Interfaces:**
- Consumes:
  - `cheladb_metadata::{NeonEndpoint, NeonRole, NeonDatabase, MetadataClient, MetadataError}` (Task 3)
  - `cheladb_neon::{ComputeLocator, DockerLocator, ComputeSigner, StorconClient, proxy_api}` (Tasks 1–2)
- Produces:
  - `orchestrator::internal::NeonMetadataPort`: the trait of the six Task 3 queries, with the same signatures, returning `impl Future<…> + Send`.
  - `orchestrator::internal::InternalState<M, L> { metadata, locator, storcon: StorconClient, signer: ComputeSigner, http: reqwest::Client, proxy_token: String, storcon_token: String }`
  - `orchestrator::internal::build_internal_router<M: NeonMetadataPort, L: ComputeLocator>(Arc<InternalState<M, L>>) -> Router`
  - `orchestrator::internal::config::InternalConfig { addr, proxy_token, storcon_token, storcon_url, compute_jwt_dir, compute_network }`, with `from_env()`, `from_lookup(impl Fn(&str) -> Option<String>)` and `load_signer()`.
  - `pub(crate) fn constant_time_eq(&[u8], &[u8]) -> bool` in `internal/mod.rs`.

- [x] **Step 1: Dependencies**

In `services/orchestrator/Cargo.toml`:
- `[dependencies]`: add `cheladb-neon`, `hex`, `reqwest` and `sha2`, each `{ workspace = true }`;
- `[dev-dependencies]`: add `tempfile = { workspace = true }`.

- [x] **Step 2: Write the failing tests**

`services/orchestrator/src/internal/config.rs`, with the tests first and `todo!()` bodies:

```rust
//! Settings for the internal listener, all with local-dev defaults.

use anyhow::{Context, Result};
use cheladb_neon::ComputeSigner;
use cheladb_neon::jwt::{PRIVATE_KEY_FILE, PUBLIC_KEY_FILE};
use std::net::SocketAddr;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct InternalConfig {
    pub addr: SocketAddr,
    pub proxy_token: String,
    pub storcon_token: String,
    pub storcon_url: String,
    pub compute_jwt_dir: PathBuf,
    pub compute_network: String,
}

impl InternalConfig {
    pub fn from_env() -> Result<Self> {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    /// Blank values count as unset.
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Self> {
        todo!()
    }

    /// The key pair `cheladb dev bootstrap` writes.
    pub fn load_signer(&self) -> Result<ComputeSigner> {
        todo!()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_for_local_dev() {
        let c = InternalConfig::from_lookup(|_| None).unwrap();
        assert_eq!(c.addr, "127.0.0.1:8083".parse().unwrap());
        assert_eq!(c.proxy_token, "cheladb-dev-proxy-api");
        assert_eq!(c.storcon_token, "cheladb-dev-storcon-hook");
        assert_eq!(c.storcon_url, "http://127.0.0.1:1234");
        assert_eq!(c.compute_jwt_dir, PathBuf::from(".cheladb/neon"));
        assert_eq!(c.compute_network, "cheladb_default");
    }

    #[test]
    fn set_values_win_and_blank_ones_do_not() {
        let c = InternalConfig::from_lookup(|name| match name {
            "CHELADB_INTERNAL_ADDR" => Some("127.0.0.1:9083".into()),
            "CHELADB_PROXY_API_TOKEN" => Some("  ".into()),
            _ => None,
        })
        .unwrap();
        assert_eq!(c.addr.port(), 9083);
        assert_eq!(c.proxy_token, "cheladb-dev-proxy-api");
    }

    #[test]
    fn a_bad_address_names_the_variable() {
        let err = InternalConfig::from_lookup(|name| {
            (name == "CHELADB_INTERNAL_ADDR").then(|| "nope".to_string())
        })
        .unwrap_err()
        .to_string();
        assert!(err.contains("CHELADB_INTERNAL_ADDR"), "{err}");
    }

    // Review Focus 4: an orchestrator started before bootstrap says what to run.
    #[test]
    fn missing_keys_say_to_run_the_bootstrap() {
        let dir = tempfile::tempdir().unwrap();
        let c = InternalConfig {
            compute_jwt_dir: dir.path().to_path_buf(),
            ..InternalConfig::from_lookup(|_| None).unwrap()
        };
        let err = format!("{:#}", c.load_signer().err().expect("no keys"));
        assert!(err.contains("just zitadel-bootstrap"), "{err}");
    }

    #[test]
    fn keys_from_the_bootstrap_load() {
        let dir = tempfile::tempdir().unwrap();
        let (pem, x) = cheladb_neon::jwt::generate_keypair();
        std::fs::write(dir.path().join(PRIVATE_KEY_FILE), pem).unwrap();
        std::fs::write(dir.path().join(PUBLIC_KEY_FILE), format!("{x}\n")).unwrap();
        let c = InternalConfig {
            compute_jwt_dir: dir.path().to_path_buf(),
            ..InternalConfig::from_lookup(|_| None).unwrap()
        };
        assert!(c.load_signer().is_ok());
    }
}
```

`services/orchestrator/tests/internal.rs`, the harness plus Task 5's tests:

```rust
//! The internal listener (proxy, compute_ctl and storage-controller routes) with fake
//! metadata, a fake compute locator, and in-process stand-ins for the storage controller and
//! computes.

use axum::Router;
use axum::http::StatusCode as AxumStatus;
use axum::routing::get;
use cheladb_metadata::{MetadataError, NeonDatabase, NeonEndpoint, NeonRole};
use cheladb_neon::jwt::generate_keypair;
use cheladb_neon::{ComputeLocator, ComputeSigner, StorconClient};
use orchestrator::internal::{InternalState, NeonMetadataPort, build_internal_router};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use tokio::net::TcpListener;

const PROXY_TOKEN: &str = "proxy-token";
const STORCON_TOKEN: &str = "storcon-token";
/// The compute token for every endpoint `FakeNeon::add` creates.
const COMPUTE_TOKEN: &str = "compute-token";

fn sha256_hex(s: &str) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(s.as_bytes()))
}

#[derive(Clone, Default)]
struct FakeNeon {
    endpoints: Arc<Mutex<Vec<NeonEndpoint>>>,
    /// (endpoint id, role)
    roles: Arc<Mutex<Vec<(String, NeonRole)>>>,
    fail: Arc<Mutex<bool>>,
}

impl FakeNeon {
    /// An endpoint with role and database `app`.
    fn add(&self, id: &str, tenant: &str, timeline: &str) {
        self.endpoints.lock().unwrap().push(NeonEndpoint {
            id: id.into(),
            tenant_id: tenant.into(),
            timeline_id: timeline.into(),
            compute_token_sha256: sha256_hex(COMPUTE_TOKEN),
        });
        self.roles.lock().unwrap().push((
            id.into(),
            NeonRole { name: "app".into(), scram_verifier: format!("SCRAM-SHA-256$verifier-of-{id}") },
        ));
    }

    fn break_it(&self) {
        *self.fail.lock().unwrap() = true;
    }

    fn check(&self) -> Result<(), MetadataError> {
        if *self.fail.lock().unwrap() {
            return Err(MetadataError::Config("metadata is down".into()));
        }
        Ok(())
    }
}

impl NeonMetadataPort for FakeNeon {
    async fn neon_endpoint(&self, id: &str) -> Result<Option<NeonEndpoint>, MetadataError> {
        self.check()?;
        Ok(self.endpoints.lock().unwrap().iter().find(|e| e.id == id).cloned())
    }

    async fn neon_role_secret(&self, endpoint_id: &str, role: &str) -> Result<Option<Option<String>>, MetadataError> {
        if self.neon_endpoint(endpoint_id).await?.is_none() {
            return Ok(None);
        }
        Ok(Some(
            self.roles.lock().unwrap().iter()
                .find(|(ep, r)| ep == endpoint_id && r.name == role)
                .map(|(_, r)| r.scram_verifier.clone()),
        ))
    }

    async fn neon_roles(&self, endpoint_id: &str) -> Result<Vec<NeonRole>, MetadataError> {
        self.check()?;
        Ok(self.roles.lock().unwrap().iter().filter(|(ep, _)| ep == endpoint_id).map(|(_, r)| r.clone()).collect())
    }

    async fn neon_databases(&self, endpoint_id: &str) -> Result<Vec<NeonDatabase>, MetadataError> {
        self.check()?;
        Ok(if self.neon_endpoint(endpoint_id).await?.is_some() {
            vec![NeonDatabase { name: "app".into(), owner: "app".into() }]
        } else {
            vec![]
        })
    }

    async fn neon_endpoints_for_tenant(&self, tenant_id: &str) -> Result<Vec<String>, MetadataError> {
        self.check()?;
        Ok(self.endpoints.lock().unwrap().iter().filter(|e| e.tenant_id == tenant_id).map(|e| e.id.clone()).collect())
    }

    async fn neon_endpoints_for_timeline(&self, timeline_id: &str) -> Result<Vec<String>, MetadataError> {
        self.check()?;
        Ok(self.endpoints.lock().unwrap().iter().filter(|e| e.timeline_id == timeline_id).map(|e| e.id.clone()).collect())
    }
}

#[derive(Clone, Default)]
struct FakeLocator {
    ips: Arc<Mutex<HashMap<String, IpAddr>>>,
    controls: Arc<Mutex<HashMap<String, SocketAddr>>>,
    broken: Arc<Mutex<bool>>,
}

impl FakeLocator {
    fn running(&self, ep: &str, ip: &str, control: Option<SocketAddr>) {
        self.ips.lock().unwrap().insert(ep.into(), ip.parse().unwrap());
        if let Some(addr) = control {
            self.controls.lock().unwrap().insert(ep.into(), addr);
        }
    }
}

impl ComputeLocator for FakeLocator {
    async fn address(&self, ep: &str) -> anyhow::Result<Option<IpAddr>> {
        anyhow::ensure!(!*self.broken.lock().unwrap(), "docker is down");
        Ok(self.ips.lock().unwrap().get(ep).copied())
    }

    async fn control_addr(&self, ep: &str) -> anyhow::Result<Option<SocketAddr>> {
        anyhow::ensure!(!*self.broken.lock().unwrap(), "docker is down");
        Ok(self.controls.lock().unwrap().get(ep).copied())
    }
}

async fn serve(app: Router) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    addr
}

/// A storage-controller stand-in: tenants on pageserver `ps:6400`, safekeeper `sk1:5454`
/// active and `sk2:5454` paused. With `healthy = false`, everything answers 500.
async fn storcon(healthy: bool) -> String {
    let app = if healthy {
        Router::new()
            .route("/debug/v1/tenant/{tenant}/locate", get(|| async {
                axum::Json(json!({ "shards": [{ "listen_pg_addr": "ps", "listen_pg_port": 6400 }] }))
            }))
            .route("/control/v1/safekeeper", get(|| async {
                axum::Json(json!([
                    { "host": "sk1", "port": 5454, "scheduling_policy": "Active" },
                    { "host": "sk2", "port": 5454, "scheduling_policy": "Pause" },
                ]))
            }))
    } else {
        Router::new().fallback(|| async { (AxumStatus::INTERNAL_SERVER_ERROR, "boom") })
    };
    format!("http://{}", serve(app).await)
}

struct Internal {
    base: String,
    signer: ComputeSigner,
    http: reqwest::Client,
}

impl Internal {
    async fn get(&self, path: &str, token: Option<&str>) -> (u16, Value) {
        let mut req = self.http.get(format!("{}{path}", self.base));
        if let Some(token) = token {
            req = req.bearer_auth(token);
        }
        let resp = req.send().await.unwrap();
        let status = resp.status().as_u16();
        (status, resp.json().await.unwrap_or(Value::Null))
    }

    async fn put(&self, path: &str, token: Option<&str>, body: &str) -> u16 {
        let mut req = self.http.put(format!("{}{path}", self.base))
            .header("content-type", "application/json")
            .body(body.to_string());
        if let Some(token) = token {
            req = req.bearer_auth(token);
        }
        req.send().await.unwrap().status().as_u16()
    }
}

async fn spawn_internal(neon: FakeNeon, locator: FakeLocator, storcon_base: &str) -> Internal {
    let (pem, x) = generate_keypair();
    let state = Arc::new(InternalState {
        metadata: neon,
        locator,
        storcon: StorconClient::new(storcon_base, 0),
        signer: ComputeSigner::new(&pem, &x).unwrap(),
        http: reqwest::Client::new(),
        proxy_token: PROXY_TOKEN.into(),
        storcon_token: STORCON_TOKEN.into(),
    });
    let addr = serve(build_internal_router(state)).await;
    Internal {
        base: format!("http://{addr}"),
        signer: ComputeSigner::new(&pem, &x).unwrap(),
        http: reqwest::Client::new(),
    }
}

const TENANT: &str = "11111111111111111111111111111111";
const TIMELINE: &str = "22222222222222222222222222222222";
const ACCESS: &str = "/proxy/api/v1/get_endpoint_access_control";
const WAKE: &str = "/proxy/api/v1/wake_compute";

#[tokio::test]
async fn health_needs_no_token() {
    let app = spawn_internal(FakeNeon::default(), FakeLocator::default(), "http://127.0.0.1:9").await;
    assert_eq!(app.get("/health", None).await.0, 200);
}

#[tokio::test]
async fn proxy_routes_need_the_proxy_token() {
    let neon = FakeNeon::default();
    neon.add("ep-a", TENANT, TIMELINE);
    let app = spawn_internal(neon, FakeLocator::default(), "http://127.0.0.1:9").await;
    let q = format!("{ACCESS}?endpointish=ep-a&role=app");
    assert_eq!(app.get(&q, None).await.0, 401);
    assert_eq!(app.get(&q, Some("wrong")).await.0, 401);
    assert_eq!(app.get(&q, Some(STORCON_TOKEN)).await.0, 401, "another caller's token");
    assert_eq!(app.get(&q, Some(COMPUTE_TOKEN)).await.0, 401);
    assert_eq!(app.get(&format!("{WAKE}?endpointish=ep-a"), Some(STORCON_TOKEN)).await.0, 401);
}

#[tokio::test]
async fn access_control_gives_the_role_secret() {
    let neon = FakeNeon::default();
    neon.add("ep-a", TENANT, TIMELINE);
    let app = spawn_internal(neon, FakeLocator::default(), "http://127.0.0.1:9").await;
    let (status, body) = app.get(&format!("{ACCESS}?endpointish=ep-a&role=app"), Some(PROXY_TOKEN)).await;
    assert_eq!(status, 200);
    assert_eq!(body["role_secret"], "SCRAM-SHA-256$verifier-of-ep-a");
    assert_eq!(body["project_id"], "ep-a");
}

// Review Focus 1: the proxy adds session_id, application_name and options.
#[tokio::test]
async fn extra_proxy_parameters_are_ignored() {
    let neon = FakeNeon::default();
    neon.add("ep-a", TENANT, TIMELINE);
    let locator = FakeLocator::default();
    locator.running("ep-a", "172.20.0.9", None);
    let app = spawn_internal(neon, locator, "http://127.0.0.1:9").await;
    let extra = "session_id=00000000-0000-0000-0000-000000000000&application_name=psql&options=-c%20x%3D1";
    let (status, _) = app.get(&format!("{ACCESS}?{extra}&endpointish=ep-a&role=app"), Some(PROXY_TOKEN)).await;
    assert_eq!(status, 200);
    let (status, _) = app.get(&format!("{WAKE}?{extra}&endpointish=ep-a"), Some(PROXY_TOKEN)).await;
    assert_eq!(status, 200);
}

#[tokio::test]
async fn an_unknown_role_gets_an_empty_secret() {
    let neon = FakeNeon::default();
    neon.add("ep-a", TENANT, TIMELINE);
    let app = spawn_internal(neon, FakeLocator::default(), "http://127.0.0.1:9").await;
    let (status, body) = app.get(&format!("{ACCESS}?endpointish=ep-a&role=nobody"), Some(PROXY_TOKEN)).await;
    assert_eq!(status, 200);
    assert_eq!(body["role_secret"], "");
}

#[tokio::test]
async fn an_unknown_endpoint_is_not_found() {
    let app = spawn_internal(FakeNeon::default(), FakeLocator::default(), "http://127.0.0.1:9").await;
    for path in [format!("{ACCESS}?endpointish=ep-x&role=app"), format!("{WAKE}?endpointish=ep-x")] {
        let (status, body) = app.get(&path, Some(PROXY_TOKEN)).await;
        assert_eq!(status, 404, "{path}");
        assert_eq!(body["status"]["details"]["error_info"]["reason"], "ENDPOINT_NOT_FOUND");
    }
}

#[tokio::test]
async fn a_metadata_failure_asks_the_proxy_to_retry() {
    let neon = FakeNeon::default();
    neon.break_it();
    let app = spawn_internal(neon, FakeLocator::default(), "http://127.0.0.1:9").await;
    let (status, body) = app.get(&format!("{ACCESS}?endpointish=ep-a&role=app"), Some(PROXY_TOKEN)).await;
    assert_eq!(status, 500);
    assert_eq!(body["status"]["details"]["retry_info"]["retry_delay_ms"], 1000);
}

#[tokio::test]
async fn wake_compute_gives_the_running_compute() {
    let neon = FakeNeon::default();
    neon.add("ep-a", TENANT, TIMELINE);
    let locator = FakeLocator::default();
    locator.running("ep-a", "172.20.0.9", None);
    let app = spawn_internal(neon, locator, "http://127.0.0.1:9").await;
    let (status, body) = app.get(&format!("{WAKE}?endpointish=ep-a"), Some(PROXY_TOKEN)).await;
    assert_eq!(status, 200);
    assert_eq!(body["address"], "172.20.0.9:5432");
    assert_eq!(body["aux"]["branch_id"], TIMELINE);
}

#[tokio::test]
async fn a_compute_that_is_not_running_is_retried() {
    let neon = FakeNeon::default();
    neon.add("ep-a", TENANT, TIMELINE);
    let app = spawn_internal(neon, FakeLocator::default(), "http://127.0.0.1:9").await;
    let (status, body) = app.get(&format!("{WAKE}?endpointish=ep-a"), Some(PROXY_TOKEN)).await;
    assert_eq!(status, 503);
    assert_eq!(body["status"]["details"]["retry_info"]["retry_delay_ms"], 1000);
}

#[tokio::test]
async fn docker_being_down_is_retried_too() {
    let neon = FakeNeon::default();
    neon.add("ep-a", TENANT, TIMELINE);
    let locator = FakeLocator::default();
    *locator.broken.lock().unwrap() = true;
    let app = spawn_internal(neon, locator, "http://127.0.0.1:9").await;
    let (status, body) = app.get(&format!("{WAKE}?endpointish=ep-a"), Some(PROXY_TOKEN)).await;
    assert_eq!(status, 503);
    assert_eq!(body["status"]["details"]["retry_info"]["retry_delay_ms"], 1000);
}
```

The harness already includes `storcon()`, `Internal::put` and the `Internal::signer` field, which Tasks 6 and 7 use. Until then, mark them `#[allow(dead_code)]`, and remove the attribute in Task 7.

- [x] **Step 3: Run the tests to verify they fail**

Run: `cargo test -p orchestrator --test internal`
Expected: compile error, `orchestrator::internal` not found.

- [x] **Step 4: Implement `internal/mod.rs`**

```rust
//! The internal listener: machine-to-machine routes for Neon's proxy, compute_ctl and the
//! storage controller. It binds `CHELADB_INTERNAL_ADDR` (loopback by default) and never goes
//! through the gateway. Each caller has its own bearer token and can open only its own routes.

pub mod config;
mod proxy;

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::http::header::AUTHORIZATION;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use cheladb_metadata::{MetadataClient, MetadataError, NeonDatabase, NeonEndpoint, NeonRole};
use cheladb_neon::{ComputeLocator, ComputeSigner, StorconClient};
use std::future::Future;
use std::sync::Arc;
use tower_http::trace::TraceLayer;

/// The metadata the internal routes read. Abstracted so tests can use an in-memory fake.
pub trait NeonMetadataPort: Send + Sync + 'static {
    fn neon_endpoint(&self, id: &str)
    -> impl Future<Output = Result<Option<NeonEndpoint>, MetadataError>> + Send;
    fn neon_role_secret(
        &self,
        endpoint_id: &str,
        role: &str,
    ) -> impl Future<Output = Result<Option<Option<String>>, MetadataError>> + Send;
    fn neon_roles(&self, endpoint_id: &str)
    -> impl Future<Output = Result<Vec<NeonRole>, MetadataError>> + Send;
    fn neon_databases(
        &self,
        endpoint_id: &str,
    ) -> impl Future<Output = Result<Vec<NeonDatabase>, MetadataError>> + Send;
    fn neon_endpoints_for_tenant(
        &self,
        tenant_id: &str,
    ) -> impl Future<Output = Result<Vec<String>, MetadataError>> + Send;
    fn neon_endpoints_for_timeline(
        &self,
        timeline_id: &str,
    ) -> impl Future<Output = Result<Vec<String>, MetadataError>> + Send;
}

// As for `MetadataPort`: each method must call `MetadataClient`'s inherent method by its full
// path, or a rename would make it call itself.
impl NeonMetadataPort for MetadataClient {
    async fn neon_endpoint(&self, id: &str) -> Result<Option<NeonEndpoint>, MetadataError> {
        MetadataClient::neon_endpoint(self, id).await
    }
    async fn neon_role_secret(
        &self,
        endpoint_id: &str,
        role: &str,
    ) -> Result<Option<Option<String>>, MetadataError> {
        MetadataClient::neon_role_secret(self, endpoint_id, role).await
    }
    async fn neon_roles(&self, endpoint_id: &str) -> Result<Vec<NeonRole>, MetadataError> {
        MetadataClient::neon_roles(self, endpoint_id).await
    }
    async fn neon_databases(&self, endpoint_id: &str) -> Result<Vec<NeonDatabase>, MetadataError> {
        MetadataClient::neon_databases(self, endpoint_id).await
    }
    async fn neon_endpoints_for_tenant(&self, tenant_id: &str) -> Result<Vec<String>, MetadataError> {
        MetadataClient::neon_endpoints_for_tenant(self, tenant_id).await
    }
    async fn neon_endpoints_for_timeline(
        &self,
        timeline_id: &str,
    ) -> Result<Vec<String>, MetadataError> {
        MetadataClient::neon_endpoints_for_timeline(self, timeline_id).await
    }
}

pub struct InternalState<M, L> {
    pub metadata: M,
    pub locator: L,
    pub storcon: StorconClient,
    /// Signs the tokens compute_ctl accepts, and publishes their JWKS in each spec.
    pub signer: ComputeSigner,
    /// For compute_ctl's `/configure`.
    pub http: reqwest::Client,
    pub proxy_token: String,
    pub storcon_token: String,
}

pub fn build_internal_router<M, L>(state: Arc<InternalState<M, L>>) -> Router
where
    M: NeonMetadataPort,
    L: ComputeLocator,
{
    let proxy_token: Arc<str> = state.proxy_token.as_str().into();
    let proxy = Router::new()
        .route(
            "/proxy/api/v1/get_endpoint_access_control",
            get(proxy::access_control::<M, L>),
        )
        .route("/proxy/api/v1/wake_compute", get(proxy::wake_compute::<M, L>))
        .route_layer(middleware::from_fn_with_state(proxy_token, require_bearer));
    Router::new()
        .route("/health", get(health))
        .merge(proxy)
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

async fn health() -> impl IntoResponse {
    Json(serde_json::json!({ "status": "ok", "service": "orchestrator-internal" }))
}

/// The bearer token from `Authorization`, if any.
pub(crate) fn bearer(headers: &axum::http::HeaderMap) -> Option<&str> {
    headers
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
}

/// Runs before extractors, so a caller without the token learns nothing about the body format.
async fn require_bearer(State(token): State<Arc<str>>, request: Request, next: Next) -> Response {
    match bearer(request.headers()) {
        Some(t) if constant_time_eq(t.as_bytes(), token.as_bytes()) => next.run(request).await,
        _ => StatusCode::UNAUTHORIZED.into_response(),
    }
}

/// Compare secrets without an early exit, so timing doesn't show how much matched.
pub(crate) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::constant_time_eq;

    #[test]
    fn compares_secrets() {
        assert!(constant_time_eq(b"secret", b"secret"));
        assert!(!constant_time_eq(b"secret", b"secreT"));
        assert!(!constant_time_eq(b"secret", b"secret-longer"));
        assert!(!constant_time_eq(b"", b"x"));
    }
}
```

`services/orchestrator/src/internal/proxy.rs`:

```rust
//! What Neon's proxy calls (`--auth-backend=control-plane`, neon
//! `proxy/src/control_plane/client/cplane_proxy_v1.rs`): the role's SCRAM secret, and
//! the address of the endpoint's compute.

use super::{InternalState, NeonMetadataPort};
use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use cheladb_neon::ComputeLocator;
use cheladb_neon::proxy_api::{self, Reason};
use serde::Deserialize;
use std::sync::Arc;
use tracing::{error, warn};

/// The proxy also sends `session_id`, `application_name` and `options`; they're ignored.
#[derive(Debug, Deserialize)]
pub struct AccessControlQuery {
    endpointish: String,
    role: String,
}

#[derive(Debug, Deserialize)]
pub struct WakeQuery {
    endpointish: String,
}

const RETRY_MS: u64 = 1000;

fn reply(status: StatusCode, body: serde_json::Value) -> Response {
    (status, Json(body)).into_response()
}

fn not_found(endpoint: &str) -> Response {
    reply(
        StatusCode::NOT_FOUND,
        proxy_api::error(Reason::EndpointNotFound, &format!("endpoint {endpoint} not found"), None),
    )
}

fn internal(err: impl std::fmt::Display) -> Response {
    error!("proxy API: {err}");
    reply(
        StatusCode::INTERNAL_SERVER_ERROR,
        proxy_api::error(Reason::Unknown, "internal error", Some(RETRY_MS)),
    )
}

/// 503 with `retry_ms`: the proxy retries, for example while a compute boots.
fn retry_later(message: &str) -> Response {
    reply(
        StatusCode::SERVICE_UNAVAILABLE,
        proxy_api::error(Reason::Unknown, message, Some(RETRY_MS)),
    )
}

pub async fn access_control<M: NeonMetadataPort, L: ComputeLocator>(
    State(state): State<Arc<InternalState<M, L>>>,
    Query(q): Query<AccessControlQuery>,
) -> Response {
    match state.metadata.neon_role_secret(&q.endpointish, &q.role).await {
        Err(e) => internal(e),
        Ok(None) => not_found(&q.endpointish),
        Ok(Some(secret)) => Json(proxy_api::access_control(
            &q.endpointish,
            secret.as_deref().unwrap_or(""),
        ))
        .into_response(),
    }
}

pub async fn wake_compute<M: NeonMetadataPort, L: ComputeLocator>(
    State(state): State<Arc<InternalState<M, L>>>,
    Query(q): Query<WakeQuery>,
) -> Response {
    let endpoint = match state.metadata.neon_endpoint(&q.endpointish).await {
        Err(e) => return internal(e),
        Ok(None) => return not_found(&q.endpointish),
        Ok(Some(endpoint)) => endpoint,
    };
    match state.locator.address(&endpoint.id).await {
        Ok(Some(ip)) => {
            Json(proxy_api::wake_compute(ip, &endpoint.id, &endpoint.timeline_id)).into_response()
        }
        Ok(None) => retry_later("compute is starting"),
        Err(e) => {
            warn!(endpoint = %endpoint.id, "locating compute: {e:#}");
            retry_later("compute is unavailable")
        }
    }
}
```

`config.rs` bodies:

```rust
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let or = |name: &str, default: &str| {
            get(name)
                .filter(|v| !v.trim().is_empty())
                .unwrap_or_else(|| default.to_string())
        };
        Ok(Self {
            addr: or("CHELADB_INTERNAL_ADDR", "127.0.0.1:8083")
                .parse()
                .context("CHELADB_INTERNAL_ADDR is not a socket address")?,
            proxy_token: or("CHELADB_PROXY_API_TOKEN", "cheladb-dev-proxy-api"),
            storcon_token: or("CHELADB_STORCON_HOOK_TOKEN", "cheladb-dev-storcon-hook"),
            storcon_url: or("CHELADB_STORCON_URL", "http://127.0.0.1:1234"),
            compute_jwt_dir: PathBuf::from(or("CHELADB_COMPUTE_JWT_DIR", ".cheladb/neon")),
            compute_network: or("CHELADB_COMPUTE_NETWORK", "cheladb_default"),
        })
    }

    pub fn load_signer(&self) -> Result<ComputeSigner> {
        let read = |name: &str| {
            let path = self.compute_jwt_dir.join(name);
            std::fs::read_to_string(&path).with_context(|| {
                format!(
                    "read {} (the compute-JWT key pair; run just zitadel-bootstrap)",
                    path.display()
                )
            })
        };
        ComputeSigner::new(&read(PRIVATE_KEY_FILE)?, &read(PUBLIC_KEY_FILE)?)
    }
```

In `services/orchestrator/src/lib.rs`, add `pub mod internal;` after the `use` block.

In `services/orchestrator/src/main.rs`:
- imports: `use orchestrator::internal::{build_internal_router, config::InternalConfig, InternalState};`, `use cheladb_neon::{DockerLocator, StorconClient};` and `use std::time::Duration;`;
- read the config and load the signer before the metadata connects, so a missing key fails fast;
- keep a clone of the metadata client before moving it into `AppState`;
- replace the single serve with two.

```rust
    let internal_config = InternalConfig::from_env()?;
    let signer = internal_config.load_signer()?;
    // … existing metadata/nats/secrets setup …
    let internal_state = Arc::new(InternalState {
        metadata: metadata.clone(),
        locator: DockerLocator::connect(&internal_config.compute_network)?,
        storcon: StorconClient::new(&internal_config.storcon_url, 0)
            .with_timeout(Duration::from_secs(10)),
        signer,
        http: reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()?,
        proxy_token: internal_config.proxy_token.clone(),
        storcon_token: internal_config.storcon_token.clone(),
    });
    // … existing `state` and `app` …
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let internal_listener = tokio::net::TcpListener::bind(internal_config.addr).await?;
    info!("orchestrator listening on {addr}");
    info!(
        "internal listener (proxy, compute_ctl, storage controller) on {}",
        internal_config.addr
    );
    tokio::try_join!(
        async { axum::serve(listener, app).await },
        async { axum::serve(internal_listener, build_internal_router(internal_state)).await },
    )?;
    Ok(())
```

Remove the old `let listener …; axum::serve(listener, app).await?;` lines. `metadata.clone()` must come before `metadata` moves into `AppState`.

- [x] **Step 5: Run the tests and clippy**

Run: `cargo test -p orchestrator && cargo clippy -p orchestrator --all-targets -- -D warnings && rustfmt --edition 2024 services/orchestrator/src/internal/*.rs services/orchestrator/tests/internal.rs`
Expected: all pass, including the existing `api.rs`.

- [x] **Step 6: Commit**

```bash
git add services/orchestrator Cargo.lock
git commit -m "orchestrator: internal listener with the proxy's access-control and wake routes

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01KESSEqEbiuKm7pMwCPe2Wg"
```

---

### Task 6: Orchestrator: compute spec route

**Files:**
- Create: `services/orchestrator/src/internal/spec.rs`
- Modify: `services/orchestrator/src/internal/mod.rs`, `services/orchestrator/tests/internal.rs`

**Interfaces:**
- Consumes: `InternalState`, `NeonMetadataPort`, `bearer` and `constant_time_eq` (Task 5); `cheladb_neon::spec::{config_response, SpecInputs, RoleSpec, DatabaseSpec}`, `NeonId`
- Produces: `pub(crate) async fn endpoint_config<M, L>(state: &InternalState<M, L>, endpoint: &NeonEndpoint) -> anyhow::Result<serde_json::Value>`, which Task 7 uses.

- [ ] **Step 1: Write the failing tests**

Append to `services/orchestrator/tests/internal.rs`:

```rust
fn spec_path(ep: &str) -> String {
    format!("/compute/api/v2/computes/{ep}/spec")
}

#[tokio::test]
async fn the_spec_goes_to_the_compute_with_its_token() {
    let neon = FakeNeon::default();
    neon.add("ep-a", TENANT, TIMELINE);
    let app = spawn_internal(neon, FakeLocator::default(), &storcon(true).await).await;
    let (status, body) = app.get(&spec_path("ep-a"), Some(COMPUTE_TOKEN)).await;
    assert_eq!(status, 200);
    assert_eq!(body["status"], "attached");
    assert_eq!(body["spec"]["tenant_id"], TENANT);
    assert_eq!(body["spec"]["timeline_id"], TIMELINE);
    assert_eq!(body["spec"]["pageserver_connstring"], "postgresql://no_user@ps:6400");
    assert_eq!(body["spec"]["safekeeper_connstrings"], json!(["sk1:5454"]));
    assert_eq!(body["spec"]["cluster"]["cluster_id"], "ep-a");
    assert_eq!(
        body["spec"]["cluster"]["roles"][0]["encrypted_password"],
        "SCRAM-SHA-256$verifier-of-ep-a"
    );
    assert_eq!(body["spec"]["cluster"]["databases"][0]["owner"], "app");
    assert_eq!(body["compute_ctl_config"]["jwks"], app.signer.jwks());
}

#[tokio::test]
async fn the_spec_needs_that_computes_token() {
    let neon = FakeNeon::default();
    neon.add("ep-a", TENANT, TIMELINE);
    let app = spawn_internal(neon, FakeLocator::default(), &storcon(true).await).await;
    assert_eq!(app.get(&spec_path("ep-a"), None).await.0, 401);
    assert_eq!(app.get(&spec_path("ep-a"), Some("wrong")).await.0, 401);
    assert_eq!(app.get(&spec_path("ep-a"), Some(PROXY_TOKEN)).await.0, 401, "another caller's token");
    assert_eq!(app.get(&spec_path("ep-a"), Some(STORCON_TOKEN)).await.0, 401);
    assert_eq!(
        app.get(&spec_path("ep-missing"), Some(COMPUTE_TOKEN)).await.0,
        401,
        "an unknown id looks like a bad token"
    );
}

#[tokio::test]
async fn the_spec_waits_for_the_storage_controller() {
    let neon = FakeNeon::default();
    neon.add("ep-a", TENANT, TIMELINE);
    let app = spawn_internal(neon, FakeLocator::default(), &storcon(false).await).await;
    assert_eq!(app.get(&spec_path("ep-a"), Some(COMPUTE_TOKEN)).await.0, 503);
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p orchestrator --test internal spec`
Expected: FAIL, 404 instead of 200 or 401, because the route doesn't exist.

- [ ] **Step 3: Implement**

`services/orchestrator/src/internal/spec.rs`:

```rust
//! `GET /compute/api/v2/computes/{id}/spec`: compute_ctl pulls its spec here when it boots
//! (neon `compute_tools/src/spec.rs`). The bearer token is the compute's own; only its SHA-256
//! is stored. compute_ctl retries 503.

use super::{InternalState, NeonMetadataPort, bearer, constant_time_eq};
use anyhow::Context;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use cheladb_metadata::NeonEndpoint;
use cheladb_neon::ComputeLocator;
use cheladb_neon::NeonId;
use cheladb_neon::spec::{DatabaseSpec, RoleSpec, SpecInputs, config_response};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::sync::Arc;
use tracing::warn;

/// `spec::config_response` for an endpoint: storage placement from the storage controller;
/// roles and databases from metadata.
pub(crate) async fn endpoint_config<M: NeonMetadataPort, L: ComputeLocator>(
    state: &InternalState<M, L>,
    endpoint: &NeonEndpoint,
) -> anyhow::Result<Value> {
    let tenant: NeonId = endpoint.tenant_id.parse().context("stored tenant id")?;
    let timeline: NeonId = endpoint.timeline_id.parse().context("stored timeline id")?;
    let pageserver_connstring = state.storcon.pageserver_connstring(&tenant).await?;
    let safekeeper_connstrings = state.storcon.active_safekeepers().await?;
    let roles = state.metadata.neon_roles(&endpoint.id).await?;
    let databases = state.metadata.neon_databases(&endpoint.id).await?;
    Ok(config_response(&SpecInputs {
        endpoint_id: &endpoint.id,
        tenant_id: &tenant,
        timeline_id: &timeline,
        pageserver_connstring,
        safekeeper_connstrings,
        roles: roles
            .into_iter()
            .map(|r| RoleSpec { name: r.name, scram_verifier: r.scram_verifier })
            .collect(),
        databases: databases
            .into_iter()
            .map(|d| DatabaseSpec { name: d.name, owner: d.owner })
            .collect(),
        jwks: state.signer.jwks(),
    }))
}

pub async fn compute_spec<M: NeonMetadataPort, L: ComputeLocator>(
    State(state): State<Arc<InternalState<M, L>>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let endpoint = match state.metadata.neon_endpoint(&id).await {
        Err(e) => {
            warn!(endpoint = %id, "spec: {e}");
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
        Ok(None) => return StatusCode::UNAUTHORIZED.into_response(),
        Ok(Some(endpoint)) => endpoint,
    };
    let presented = bearer(&headers).map(|t| hex::encode(Sha256::digest(t.as_bytes())));
    match presented {
        Some(digest)
            if constant_time_eq(digest.as_bytes(), endpoint.compute_token_sha256.as_bytes()) => {}
        _ => return StatusCode::UNAUTHORIZED.into_response(),
    }
    match endpoint_config(&state, &endpoint).await {
        Ok(config) => Json(config).into_response(),
        Err(e) => {
            warn!(endpoint = %id, "spec: {e:#}");
            StatusCode::SERVICE_UNAVAILABLE.into_response()
        }
    }
}
```

In `internal/mod.rs`, add `mod spec;`, and in `build_internal_router` add this before `.merge(proxy)`:

```rust
        .route(
            "/compute/api/v2/computes/{id}/spec",
            get(spec::compute_spec::<M, L>),
        )
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p orchestrator --test internal && cargo clippy -p orchestrator --all-targets -- -D warnings && rustfmt --edition 2024 services/orchestrator/src/internal/spec.rs`
Expected: all pass.

- [ ] **Step 5: Commit**

```bash
git add services/orchestrator
git commit -m "orchestrator: serve compute specs on the internal listener

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01KESSEqEbiuKm7pMwCPe2Wg"
```

---

### Task 7: Orchestrator: storage-controller hooks

**Files:**
- Create: `services/orchestrator/src/internal/hooks.rs`
- Modify: `services/orchestrator/src/internal/mod.rs`, `services/orchestrator/tests/internal.rs`

**Interfaces:**
- Consumes: `endpoint_config` (Task 6), `cheladb_neon::reconfigure::push` (Task 2), `InternalState.storcon_token`
- Produces: `PUT /storcon/notify-attach` and `PUT /storcon/notify-safekeepers`.

- [x] **Step 1: Write the failing tests**

Append to `services/orchestrator/tests/internal.rs`:

```rust
/// A compute_ctl stand-in answering `/configure` with `status`, recording each request's
/// bearer token and body.
#[derive(Clone, Default)]
struct Configured(Arc<Mutex<Vec<(String, Value)>>>);

async fn compute(status: u16) -> (SocketAddr, Configured) {
    use axum::extract::State;
    let seen = Configured::default();
    let app = Router::new()
        .route(
            "/configure",
            axum::routing::post(
                move |State(seen): State<Configured>, headers: axum::http::HeaderMap, body: axum::Json<Value>| async move {
                    let auth = headers.get("authorization").unwrap().to_str().unwrap().to_string();
                    seen.0.lock().unwrap().push((auth, body.0));
                    (AxumStatus::from_u16(status).unwrap(), "{}")
                },
            ),
        )
        .with_state(seen.clone());
    (serve(app).await, seen)
}

fn attach(tenant: &str) -> String {
    json!({ "tenant_id": tenant, "preferred_az": null, "stripe_size": null,
            "shards": [{ "node_id": 1, "shard_number": 0 }] }).to_string()
}

const ATTACH: &str = "/storcon/notify-attach";
const SAFEKEEPERS: &str = "/storcon/notify-safekeepers";

#[tokio::test]
async fn hooks_need_the_storage_controller_token_before_the_body() {
    let app = spawn_internal(FakeNeon::default(), FakeLocator::default(), &storcon(true).await).await;
    assert_eq!(app.put(ATTACH, None, "not json").await, 401);
    assert_eq!(app.put(ATTACH, Some(PROXY_TOKEN), &attach(TENANT)).await, 401, "another caller's token");
    assert_eq!(app.put(SAFEKEEPERS, Some(COMPUTE_TOKEN), "{}").await, 401);
}

#[tokio::test]
async fn a_tenant_without_endpoints_is_answered_at_once() {
    let app = spawn_internal(FakeNeon::default(), FakeLocator::default(), &storcon(true).await).await;
    assert_eq!(app.put(ATTACH, Some(STORCON_TOKEN), &attach(TENANT)).await, 200);
}

#[tokio::test]
async fn a_compute_that_is_not_running_is_skipped() {
    let neon = FakeNeon::default();
    neon.add("ep-a", TENANT, TIMELINE);
    let app = spawn_internal(neon, FakeLocator::default(), &storcon(true).await).await;
    assert_eq!(app.put(ATTACH, Some(STORCON_TOKEN), &attach(TENANT)).await, 200);
}

#[tokio::test]
async fn a_running_compute_gets_the_new_spec() {
    let neon = FakeNeon::default();
    neon.add("ep-a", TENANT, TIMELINE);
    let (addr, seen) = compute(200).await;
    let locator = FakeLocator::default();
    locator.running("ep-a", "172.20.0.9", Some(addr));
    let app = spawn_internal(neon, locator, &storcon(true).await).await;
    assert_eq!(app.put(ATTACH, Some(STORCON_TOKEN), &attach(TENANT)).await, 200);

    let seen = seen.0.lock().unwrap().clone();
    assert_eq!(seen.len(), 1);
    let (auth, body) = &seen[0];
    assert_eq!(body["spec"]["cluster"]["roles"][0]["encrypted_password"], "SCRAM-SHA-256$verifier-of-ep-a");
    assert_eq!(body["compute_ctl_config"]["jwks"], app.signer.jwks());
    // The token verifies against the published JWKS and names this compute.
    let token = auth.strip_prefix("Bearer ").unwrap();
    let jwks: jsonwebtoken::jwk::JwkSet = serde_json::from_value(app.signer.jwks()).unwrap();
    let key = jsonwebtoken::DecodingKey::from_jwk(&jwks.keys[0]).unwrap();
    let claims = jsonwebtoken::decode::<Value>(
        token,
        &key,
        &jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::EdDSA),
    )
    .unwrap()
    .claims;
    assert_eq!(claims["compute_id"], "ep-a");
}

#[tokio::test]
async fn notify_safekeepers_reconfigures_the_timelines_endpoints() {
    let neon = FakeNeon::default();
    neon.add("ep-a", TENANT, TIMELINE);
    let (addr, seen) = compute(200).await;
    let locator = FakeLocator::default();
    locator.running("ep-a", "172.20.0.9", Some(addr));
    let app = spawn_internal(neon, locator, &storcon(true).await).await;
    let body = json!({ "tenant_id": TENANT, "timeline_id": TIMELINE, "generation": 3,
                       "safekeepers": [{ "id": 1, "hostname": "sk1" }] }).to_string();
    assert_eq!(app.put(SAFEKEEPERS, Some(STORCON_TOKEN), &body).await, 200);
    assert_eq!(seen.0.lock().unwrap().len(), 1);
}

// Review Focus 2: compute_ctl answers 412 while it is still starting. 503 makes the storage
// controller retry; 4xx would fail the reconcile.
#[tokio::test]
async fn a_compute_still_starting_is_retried() {
    let neon = FakeNeon::default();
    neon.add("ep-a", TENANT, TIMELINE);
    let (addr, _) = compute(412).await;
    let locator = FakeLocator::default();
    locator.running("ep-a", "172.20.0.9", Some(addr));
    let app = spawn_internal(neon, locator, &storcon(true).await).await;
    assert_eq!(app.put(ATTACH, Some(STORCON_TOKEN), &attach(TENANT)).await, 503);
}

// Review Focus 3: one failing compute doesn't stop the others from being reconfigured.
#[tokio::test]
async fn one_failing_compute_does_not_stop_the_others() {
    let neon = FakeNeon::default();
    neon.add("ep-a", TENANT, TIMELINE);
    neon.add("ep-b", TENANT, "33333333333333333333333333333333");
    let (bad, _) = compute(500).await;
    let (good, seen) = compute(200).await;
    let locator = FakeLocator::default();
    locator.running("ep-a", "172.20.0.9", Some(bad));
    locator.running("ep-b", "172.20.0.10", Some(good));
    let app = spawn_internal(neon, locator, &storcon(true).await).await;
    assert_eq!(app.put(ATTACH, Some(STORCON_TOKEN), &attach(TENANT)).await, 503);
    assert_eq!(seen.0.lock().unwrap().len(), 1, "ep-b was still reconfigured");
}

#[tokio::test]
async fn a_metadata_failure_is_retried() {
    let neon = FakeNeon::default();
    neon.break_it();
    let app = spawn_internal(neon, FakeLocator::default(), &storcon(true).await).await;
    assert_eq!(app.put(ATTACH, Some(STORCON_TOKEN), &attach(TENANT)).await, 503);
}
```

Remove the `#[allow(dead_code)]` attributes added in Task 5. `orchestrator`'s dev-dependencies need `jsonwebtoken = { workspace = true }`; add it.

- [x] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p orchestrator --test internal -- hook compute notify one_failing metadata_failure_is`
Expected: FAIL, 404 or 405, because the routes don't exist.

- [x] **Step 3: Implement**

`services/orchestrator/src/internal/hooks.rs`:

```rust
//! Hooks the storage controller calls when storage placement changes
//! (`PUT {control-plane-url}/notify-attach` and `/notify-safekeepers`, neon
//! `storage_controller/src/compute_hook.rs`). Each running compute on the tenant or timeline
//! gets a fresh spec. The storage controller retries 503 and treats 400/401/403 as fatal, so
//! our own failures are always 503.

use super::spec::endpoint_config;
use super::{InternalState, NeonMetadataPort};
use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use cheladb_metadata::MetadataError;
use cheladb_neon::ComputeLocator;
use cheladb_neon::reconfigure;
use serde::Deserialize;
use std::sync::Arc;
use tracing::{error, info, warn};

/// Other fields (`shards`, `preferred_az`, `stripe_size`) aren't needed.
#[derive(Debug, Deserialize)]
pub struct NotifyAttach {
    tenant_id: String,
}

/// Other fields (`generation`, `safekeepers`) aren't needed: the spec lists the safekeepers.
#[derive(Debug, Deserialize)]
pub struct NotifySafekeepers {
    tenant_id: String,
    timeline_id: String,
}

pub async fn notify_attach<M: NeonMetadataPort, L: ComputeLocator>(
    State(state): State<Arc<InternalState<M, L>>>,
    Json(req): Json<NotifyAttach>,
) -> StatusCode {
    info!(tenant = %req.tenant_id, "notify-attach");
    let endpoints = state.metadata.neon_endpoints_for_tenant(&req.tenant_id).await;
    reconfigure_all(&state, endpoints).await
}

pub async fn notify_safekeepers<M: NeonMetadataPort, L: ComputeLocator>(
    State(state): State<Arc<InternalState<M, L>>>,
    Json(req): Json<NotifySafekeepers>,
) -> StatusCode {
    info!(tenant = %req.tenant_id, timeline = %req.timeline_id, "notify-safekeepers");
    let endpoints = state.metadata.neon_endpoints_for_timeline(&req.timeline_id).await;
    reconfigure_all(&state, endpoints).await
}

/// Every endpoint is tried, even after one fails.
async fn reconfigure_all<M: NeonMetadataPort, L: ComputeLocator>(
    state: &InternalState<M, L>,
    endpoints: Result<Vec<String>, MetadataError>,
) -> StatusCode {
    let endpoints = match endpoints {
        Ok(ids) => ids,
        Err(e) => {
            error!("looking up endpoints: {e}");
            return StatusCode::SERVICE_UNAVAILABLE;
        }
    };
    let mut ok = true;
    for id in endpoints {
        if let Err(e) = reconfigure_endpoint(state, &id).await {
            warn!(endpoint = %id, "reconfigure failed; the storage controller will retry: {e:#}");
            ok = false;
        }
    }
    if ok {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    }
}

/// Push the current spec to a running compute. A compute that isn't running pulls the current
/// spec when it boots, so there's nothing to do for it.
async fn reconfigure_endpoint<M: NeonMetadataPort, L: ComputeLocator>(
    state: &InternalState<M, L>,
    endpoint_id: &str,
) -> anyhow::Result<()> {
    let Some(control_addr) = state.locator.control_addr(endpoint_id).await? else {
        info!(endpoint = %endpoint_id, "compute not running; skipping reconfigure");
        return Ok(());
    };
    let endpoint = state
        .metadata
        .neon_endpoint(endpoint_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("endpoint {endpoint_id} is gone"))?;
    let config = endpoint_config(state, &endpoint).await?;
    reconfigure::push(&state.http, control_addr, &state.signer, endpoint_id, &config).await?;
    info!(endpoint = %endpoint_id, "reconfigured compute");
    Ok(())
}
```

In `internal/mod.rs`, add `mod hooks;`, and in `build_internal_router`:

```rust
    let storcon_token: Arc<str> = state.storcon_token.as_str().into();
    let storcon = Router::new()
        .route("/storcon/notify-attach", put(hooks::notify_attach::<M, L>))
        .route("/storcon/notify-safekeepers", put(hooks::notify_safekeepers::<M, L>))
        .route_layer(middleware::from_fn_with_state(storcon_token, require_bearer));
```

then `.merge(storcon)` after `.merge(proxy)`, and import `put` from `axum::routing`.

- [x] **Step 4: Run the tests**

Run: `cargo test -p orchestrator && cargo clippy -p orchestrator --all-targets -- -D warnings && rustfmt --edition 2024 services/orchestrator/src/internal/hooks.rs services/orchestrator/tests/internal.rs`
Expected: all pass.

- [x] **Step 5: Commit**

```bash
git add services/orchestrator Cargo.lock
git commit -m "orchestrator: storage-controller hooks push new specs to running computes

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01KESSEqEbiuKm7pMwCPe2Wg"
```

---

### Task 8: Compose, justfile and offline checks

**Files:**
- Modify: `infra/compose/docker-compose.yml`, `infra/compose/.env.example`, `justfile`, `infra/neon/tests/run.sh`

- [ ] **Step 1: Update the offline checks first**

In `infra/neon/tests/run.sh`, inside `if command -v docker`:
- Change the neon-profile services check to:

```bash
  check "the neon profile adds the storage services and the proxy" \
    test "$(services --profile neon)" = "create-bucket mailpit metadata-db nats pageserver proxy proxy-tls register-safekeepers safekeeper1 seaweedfs storage-broker storage-controller storcon-db valkey zitadel zitadel-db "
```

- Change the base-profile services check (`without the profile`) to expect `mailpit metadata-db nats valkey zitadel zitadel-db `.
- Change the published-ports check to:

```bash
  check "neon publishes only 1234, 4432, 4444 and 9898, on 127.0.0.1" \
    test "$published" = "127.0.0.1:1234 127.0.0.1:4432 127.0.0.1:4444 127.0.0.1:9898"
```

- After the hook-token check, add:

```bash
  neon_json="$("${compose[@]}" --profile neon config --format json)"
  storcon="$(jq -r '.services["storage-controller"]' <<<"$neon_json")"
  check "the storage controller calls the orchestrator's internal listener" \
    grep -qx -- '--control-plane-url=http://host.docker.internal:8083/storcon' <(jq -r '.command[]' <<<"$storcon")
  check "the storage controller can resolve host.docker.internal" \
    grep -q 'host-gateway' <<<"$(jq -c '.extra_hosts' <<<"$storcon")"
  proxy="$(jq -r '.services.proxy' <<<"$neon_json")"
  proxy_args="$(jq -r '.command[]' <<<"$proxy")"
  check "the proxy asks the orchestrator's internal listener" \
    grep -qx -- '--auth-endpoint=http://host.docker.internal:8083/proxy/api/v1' <<<"$proxy_args"
  check "the proxy serves Postgres wire and serverless" \
    bash -c "grep -qx -- '--proxy=0.0.0.0:4432' <<<\"\$1\" && grep -qx -- '--wss=0.0.0.0:4444' <<<\"\$1\"" _ "$proxy_args"
  check "the proxy caches role secrets for at most a minute" \
    grep -qx -- '--project-info-cache=size=10000,ttl=1m,max_roles=10,gc_interval=60m' <<<"$proxy_args"
  check "the proxy uses Valkey (cache clearing, query cancel)" \
    bash -c "grep -qx -- '--redis-auth-type=plain' <<<\"\$1\" && grep -qx -- '--redis-plain=redis://valkey:6379' <<<\"\$1\"" _ "$proxy_args"
  valkey="$(jq -r '.services.valkey' <<<"$neon_json")"
  check "valkey is 9.1.2, pinned by digest" \
    grep -Eq '"valkey/valkey:9\.1\.2-alpine@sha256:[0-9a-f]{64}"' <<<"$valkey"
  check "valkey is published on 127.0.0.1:6379 only" \
    test "$(jq -r '[.ports[] | "\(.host_ip):\(.published)"] | join(" ")' <<<"$valkey")" = "127.0.0.1:6379"
  check "the proxy can resolve host.docker.internal" \
    grep -q 'host-gateway' <<<"$(jq -c '.extra_hosts' <<<"$proxy")"
  ptoken="$(CHELADB_PROXY_API_TOKEN=from-env "${compose[@]}" --profile neon config --format json |
    jq -r '.services.proxy.environment.NEON_PROXY_TO_CONTROLPLANE_TOKEN')"
  check "the proxy's token comes from CHELADB_PROXY_API_TOKEN" test "$ptoken" = "from-env"
```

- In the justfile section at the end, add:

```bash
check "neon-down also removes demo computes" bash -c "just --justfile '$root/justfile' --show neon-down | grep -q 'cheladb-compute-'"
```

- [ ] **Step 2: Run the checks to verify they fail**

Run: `just neon-test`
Expected: FAIL on the services, ports, storage controller, proxy and neon-down checks.

- [ ] **Step 3: Compose**

In `infra/compose/docker-compose.yml`, change the storage controller's command comment and URL:

```yaml
      # Attachment changes (every tenant create, too) are pushed to the orchestrator's
      # internal listener on the host; a tenant's reconcile waits until it answers. The
      # token is local dev only.
      - --control-plane-url=http://host.docker.internal:8083/storcon
```

Then:
- add `extra_hosts: ["host.docker.internal:host-gateway"]` to `storage-controller`, after `ports`;
- remove the `storcon-hook:` entry from its `depends_on`;
- delete the `storcon-hook` service and the comment above it.

Add, next to `nats` (base profile, no `profiles:` key):

```yaml
  # Valkey (BSD-3-Clause, speaks the Redis protocol). Neon's proxy uses it for query cancel and
  # for the cache-clearing messages the control plane publishes on a password change. Local dev:
  # no auth, loopback only. Platform mode needs auth and TLS.
  valkey:
    image: valkey/valkey:9.1.2-alpine@sha256:48332870af354a799964c0012ae1194a0bf2bf894eb508f945810596dc2d8d11
    restart: unless-stopped
    ports:
      - "127.0.0.1:6379:6379"
    volumes: ["valkey_data:/data"]
    healthcheck:
      test: ["CMD", "valkey-cli", "ping"]
      interval: 2s
      retries: 30
```

Add `valkey_data:` under `volumes:`. `just deps-up` runs `docker compose up -d` without naming services, so it starts Valkey with no recipe change.

Add after `register-safekeepers`:

```yaml
  # Writes the proxy's TLS certificate once: a local CA and a *.localtest.me certificate signed
  # by it. Clients verify against the CA; `just neon-compute-demo` copies it to
  # .cheladb/neon/proxy-ca.crt.
  proxy-tls:
    image: *neon-image
    profiles: ["neon"]
    restart: "no"
    user: "0"
    entrypoint: ["/bin/sh", "-c"]
    command:
      - |
        set -e
        cd /tls
        if [ -s tls.crt ] && openssl x509 -in tls.crt -noout -subject | grep -qF '*.localtest.me'; then
          echo "certificate for *.localtest.me exists"; exit 0
        fi
        [ -s ca.key ] || openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes \
          -keyout ca.key -out ca.crt -days 3650 -subj "/CN=Cheladb local CA"
        openssl req -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -keyout tls.key -out tls.csr \
          -subj "/CN=*.localtest.me"
        printf 'subjectAltName=DNS:*.localtest.me\n' > ext.cnf
        openssl x509 -req -in tls.csr -CA ca.crt -CAkey ca.key -CAcreateserial -days 825 \
          -extfile ext.cnf -out tls.crt
        rm -f tls.csr ext.cnf
        chown 1000:1000 tls.key tls.crt ca.crt
        chmod 600 ca.key tls.key
        echo "issued a certificate for *.localtest.me"
    volumes: ["neon_proxy_tls:/tls"]
    network_mode: none

  # Neon's proxy. Clients connect to <endpoint-id>.localtest.me (which resolves to 127.0.0.1):
  # Postgres wire on 4432; WebSocket and SQL over HTTP on 4444. It asks the orchestrator's
  # internal listener for role secrets and compute addresses, and reaches computes on this
  # network. Role secrets are cached for at most a minute, and dropped at once when the
  # control plane publishes /password_updated to Valkey.
  proxy:
    image: *neon-image
    profiles: ["neon"]
    restart: unless-stopped
    command:
      - proxy
      - --auth-backend=control-plane
      - --auth-endpoint=http://host.docker.internal:8083/proxy/api/v1
      - --proxy=0.0.0.0:4432
      - --wss=0.0.0.0:4444
      # The proxy takes the endpoint domain from the certificate's CN.
      - --tls-cert=/tls/tls.crt
      - --tls-key=/tls/tls.key
      - --project-info-cache=size=10000,ttl=1m,max_roles=10,gc_interval=60m
      - --redis-auth-type=plain
      - --redis-plain=redis://valkey:6379
    environment:
      NEON_PROXY_TO_CONTROLPLANE_TOKEN: ${CHELADB_PROXY_API_TOKEN:-cheladb-dev-proxy-api}
    ports:
      - "127.0.0.1:4432:4432"
      - "127.0.0.1:4444:4444"
    extra_hosts: ["host.docker.internal:host-gateway"]
    volumes: ["neon_proxy_tls:/tls:ro"]
    healthcheck:
      test: ["CMD", "bash", "-c", "</dev/tcp/127.0.0.1/4432"]
      interval: 2s
      retries: 60
    depends_on:
      proxy-tls:
        condition: service_completed_successfully
      # The proxy exits if it can't reach Redis at startup.
      valkey:
        condition: service_healthy
```

Add `neon_proxy_tls:` under `volumes:`.

In `infra/compose/.env.example`, replace the storcon comment block with:

```bash
# --- Neon storage and proxy (just neon-up; local dev only) ---
# The storage controller and the proxy call the orchestrator's internal listener
# (127.0.0.1:8083 on the host) with these bearer tokens. Compose and the orchestrator both
# read them; these are the defaults.
# CHELADB_STORCON_HOOK_TOKEN=cheladb-dev-storcon-hook
# CHELADB_PROXY_API_TOKEN=cheladb-dev-proxy-api
```

- [ ] **Step 4: justfile**

```
neon_services := "seaweedfs create-bucket storage-broker safekeeper1 storcon-db storage-controller pageserver register-safekeepers proxy-tls proxy"
neon_daemons := "seaweedfs storage-broker safekeeper1 storcon-db storage-controller pageserver proxy"
```

Change `neon-down` to also remove hand-made computes:

```
# Stop and remove the Neon services (volumes kept) and any demo compute containers
neon-down:
    docker ps -aq --filter name=^cheladb-compute- | xargs -r docker rm -f
    docker compose -f {{compose_file}} --env-file {{compose_env}} --profile neon rm -sf {{neon_services}}
```

Keep the existing recipe comment's wording if it differs; only add the compute part.

`deps-down` runs `compose down`, which can't remove the `cheladb_default` network while a demo compute is still attached. Add the same first line to it:

```
deps-down:
    docker ps -aq --filter name=^cheladb-compute- | xargs -r docker rm -f
    docker compose -f {{compose_file}} --env-file {{compose_env}} --profile neon down
```

- [ ] **Step 5: Run the checks**

Run: `just neon-test`
Expected: `all passed`.

- [ ] **Step 6: Bring the stack up**

Run: `just deps-up && just neon-up && just neon-ps && cargo test -p cheladb-neon --test proxy_cache_live -- --ignored`
Expected:
- `proxy-tls` exited 0, and `proxy` is healthy;
- no `storcon-hook`;
- `valkey` is healthy, `proxy_cache_live` passes, and the proxy log shows `subscribing to a channel` and `Connected to Redis KV client`;
- `docker compose … logs proxy-tls` shows `issued a certificate for *.localtest.me`.

If `proxy` restarts, read `docker compose -f infra/compose/docker-compose.yml --profile neon logs proxy`. Add any flag the pinned proxy demands, and record it in the spec's Risks section.

- [ ] **Step 7: Commit**

```bash
git add infra justfile
git commit -m "compose: Valkey, Neon's proxy (wire, WebSocket, HTTP), hooks to the orchestrator

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01KESSEqEbiuKm7pMwCPe2Wg"
```

---

### Task 9: Hand-made compute: `scripts/neon-compute-demo.sh`

**Files:**
- Create: `scripts/neon-compute-demo.sh` (executable)
- Modify: `justfile`

**Interfaces:**
- Consumes: `cheladb neon verifier` and `cheladb neon compute-args` (Task 4), the internal listener (Tasks 5–7), Compose `proxy` (Task 8), and `scripts/neon-images.sh`.
- Produces:
  - `.cheladb/neon/demo-compute.env`, with `DEMO_ENDPOINT`, `DEMO_PASSWORD`, `DEMO_TENANT` and `DEMO_TIMELINE`;
  - `.cheladb/neon/proxy-ca.crt`;
  - `scripts/neon-compute-demo.sh --down`.

- [x] **Step 1: Write the script**

```bash
#!/usr/bin/env bash
# A hand-made Neon compute behind the proxy (roadmap step 3):
# - a tenant and a timeline in the storage controller;
# - an endpoint with a `postgres` role and database in metadata;
# - a compute container that pulls its spec from the orchestrator's internal listener.
#
#   scripts/neon-compute-demo.sh          create (needs just neon-up and a running orchestrator)
#   scripts/neon-compute-demo.sh --down   remove the container, the metadata rows and the tenant
#
# Writes .cheladb/neon/demo-compute.env (the password is in it) and .cheladb/neon/proxy-ca.crt.
# Local dev only.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"
STORCON="${STORCON_URL:-http://127.0.0.1:1234}"
INTERNAL="${CHELADB_INTERNAL_URL:-http://127.0.0.1:8083}"
NETWORK="${CHELADB_COMPUTE_NETWORK:-cheladb_default}"
state_dir=.cheladb/neon
env_file="$state_dir/demo-compute.env"
compose=(docker compose -f infra/compose/docker-compose.yml --env-file infra/compose/.env.example)

say() { echo "[neon-compute-demo] $*"; }
die() {
  echo "[neon-compute-demo] FAIL: $*" >&2
  exit 1
}
hex() { od -An -N"$1" -tx1 /dev/urandom | tr -d ' \n'; }
# SQL on stdin; values are passed as psql variables (-v name=value, used as :'name').
metadata_sql() { "${compose[@]}" exec -T metadata-db psql -U cheladb -d cheladb_metadata -v ON_ERROR_STOP=1 -qtA "$@"; }

# call <method> <url> [json body]: dies on no answer or non-2xx.
call() {
  local method="$1" url="$2" body="${3:-}" out status
  local args=(-sS --max-time 60 -o /dev/stdout -w '\n%{http_code}' -X "$method")
  if [[ -n "$body" ]]; then
    args+=(-H 'content-type: application/json' -d "$body")
  fi
  out="$(curl "${args[@]}" "$url" 2>&1)" || die "$method $url: no answer (${out%%$'\n'*})"
  status="${out##*$'\n'}"
  [[ "$status" == 2* ]] || die "$method $url: $status ${out%$'\n'*}"
}

down() {
  if [[ ! -f "$env_file" ]]; then
    say "nothing to remove ($env_file doesn't exist)"
    return 0
  fi
  # shellcheck disable=SC1090
  source "$env_file"
  docker rm -f "cheladb-compute-$DEMO_ENDPOINT" >/dev/null 2>&1 || true
  metadata_sql -v ep="$DEMO_ENDPOINT" <<<"DELETE FROM neon_endpoints WHERE id = :'ep';"
  local status=""
  for _ in $(seq 120); do
    status="$(curl -sS --max-time 60 -o /dev/null -w '%{http_code}' -X DELETE "$STORCON/v1/tenant/$DEMO_TENANT")" ||
      die "DELETE tenant $DEMO_TENANT: no answer"
    case "$status" in
      202) sleep 0.5 ;;
      2* | 404) break ;;
      *) die "DELETE tenant $DEMO_TENANT: HTTP $status" ;;
    esac
  done
  [[ "$status" != 202 ]] || die "tenant $DEMO_TENANT is still being deleted after 60 s"
  rm -f "$env_file"
  say "removed $DEMO_ENDPOINT and tenant $DEMO_TENANT"
}

create() {
  [[ ! -f "$env_file" ]] || die "$env_file exists: run $0 --down first"
  curl -fsS --max-time 5 "$INTERNAL/health" >/dev/null ||
    die "the orchestrator's internal listener isn't answering at $INTERNAL (run just dev)"
  eval "$(scripts/neon-images.sh)"

  local tenant timeline ep password token
  tenant="$(hex 16)"
  timeline="$(hex 16)"
  ep="ep-$(hex 6)"
  password="$(hex 16)"
  token="$(hex 32)"
  # Written first, so --down can clean up after a failure anywhere below.
  mkdir -p "$state_dir"
  (
    umask 077
    printf 'DEMO_ENDPOINT=%s\nDEMO_PASSWORD=%s\nDEMO_TENANT=%s\nDEMO_TIMELINE=%s\n' \
      "$ep" "$password" "$tenant" "$timeline" >"$env_file"
  )

  call POST "$STORCON/v1/tenant" "{\"new_tenant_id\":\"$tenant\",\"placement_policy\":{\"Attached\":0}}"
  call POST "$STORCON/v1/tenant/$tenant/timeline" "{\"new_timeline_id\":\"$timeline\",\"pg_version\":17}"
  say "tenant $tenant, timeline $timeline"

  local verifier token_sha
  verifier="$(printf '%s' "$password" | cargo run -q -p cheladb -- neon verifier)"
  token_sha="$(printf '%s' "$token" | sha256sum | cut -d' ' -f1)"
  metadata_sql -v ep="$ep" -v tenant="$tenant" -v timeline="$timeline" -v sha="$token_sha" -v verifier="$verifier" <<'SQL'
INSERT INTO neon_endpoints (id, tenant_id, timeline_id, compute_token_sha256)
  VALUES (:'ep', :'tenant', :'timeline', :'sha');
INSERT INTO neon_roles (endpoint_id, name, scram_verifier) VALUES (:'ep', 'postgres', :'verifier');
INSERT INTO neon_databases (endpoint_id, name, owner) VALUES (:'ep', 'postgres', 'postgres');
SQL
  say "endpoint $ep in metadata"

  local args
  mapfile -t args < <(cargo run -q -p cheladb -- neon compute-args --endpoint "$ep" \
    --control-plane-uri http://host.docker.internal:8083)
  # compute_ctl tries its spec three times, then exits; Docker restarts it.
  docker run -d --name "cheladb-compute-$ep" --network "$NETWORK" --restart unless-stopped \
    -p 127.0.0.1::3080 --add-host host.docker.internal:host-gateway \
    --label "cheladb.dev/endpoint-id=$ep" -e "NEON_CONTROL_PLANE_TOKEN=$token" \
    --entrypoint /usr/local/bin/compute_ctl "$COMPUTE_NODE_IMAGE" "${args[@]}" >/dev/null
  "${compose[@]}" --profile neon cp proxy:/tls/ca.crt "$state_dir/proxy-ca.crt" >/dev/null
  say "compute cheladb-compute-$ep started"
  echo ""
  echo "psql (password in $env_file):"
  echo "  psql \"host=$ep.localtest.me port=4432 user=postgres dbname=postgres sslmode=verify-full sslrootcert=$state_dir/proxy-ca.crt\""
  echo "SQL over HTTP and WebSocket: https://$ep.localtest.me:4444/sql, wss://$ep.localtest.me:4444/v2"
  echo "Remove: just neon-compute-demo --down"
}

case "${1:-}" in
  --down) down ;;
  "") create ;;
  *) die "usage: $0 [--down]" ;;
esac
```

`chmod +x scripts/neon-compute-demo.sh`.

- [x] **Step 2: Add the justfile recipe**

After `neon-smoke`:

```
# A hand-made compute behind the proxy (needs just neon-up and just dev); --down removes it
neon-compute-demo *args:
    scripts/neon-compute-demo.sh {{args}}
```

`.cheladb/` is already git-ignored, so the env file and CA never get committed.

- [x] **Step 3: Check the script with shellcheck, if installed**

Run: `command -v shellcheck && shellcheck scripts/neon-compute-demo.sh`
Expected: no findings, or none beyond the documented `SC1090`.

- [x] **Step 4: Live run**

With `just neon-up` and `just dev` running (restart `just dev` if the orchestrator predates Task 5, so it has the internal listener):

```bash
just neon-compute-demo
source .cheladb/neon/demo-compute.env
for i in $(seq 60); do
  PGPASSWORD="$DEMO_PASSWORD" psql "host=$DEMO_ENDPOINT.localtest.me port=4432 user=postgres dbname=postgres sslmode=verify-full sslrootcert=.cheladb/neon/proxy-ca.crt" -c 'select 1' && break
  sleep 2
done
docker logs --tail 40 "cheladb-compute-$DEMO_ENDPOINT"
just neon-compute-demo --down
```

Expected:
- `select 1` succeeds within about two minutes;
- the compute log shows the spec fetched and Postgres running;
- `--down` reports the endpoint and tenant removed.

If it doesn't connect:
- check `.cheladb/dev/orchestrator.log` for spec or wake calls;
- check `docker compose … logs proxy`.

Fix before going on. A failure here is what the spec's Risks section predicted, so record anything learned there.

- [x] **Step 5: Commit**

```bash
git add scripts/neon-compute-demo.sh justfile
git commit -m "scripts: neon-compute-demo, a hand-made compute behind the proxy

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01KESSEqEbiuKm7pMwCPe2Wg"
```

---

### Task 10: Live end-to-end check, `just neon-e2e`

**Files:**
- Create: `scripts/neon-e2e.sh` (executable), `packages/neon-e2e/package.json`, `packages/neon-e2e/check.mjs`
- Modify: `justfile`, `pnpm-lock.yaml` (via `pnpm install`)

- [x] **Step 1: The serverless-driver check**

`packages/neon-e2e/package.json`:

```json
{
  "name": "@cheladb/neon-e2e",
  "private": true,
  "type": "module",
  "scripts": {
    "check": "node check.mjs"
  },
  "dependencies": {
    "@neondatabase/serverless": "^1.1.0"
  }
}
```

`packages/neon-e2e/check.mjs`:

```js
// Reads the e2e table back through Neon's proxy with @neondatabase/serverless: once over
// WebSocket (Pool) and once over SQL over HTTP (neon()). The proxy's serverless side listens
// on 4444, not 443, so both URLs point there.
// Env: DATABASE_URL, EXPECT (the row count), NODE_EXTRA_CA_CERTS (the proxy's CA).
import { Pool, neon, neonConfig } from "@neondatabase/serverless";

const url = process.env.DATABASE_URL;
const expected = Number(process.env.EXPECT);

neonConfig.webSocketConstructor = WebSocket;
neonConfig.wsProxy = (host) => `${host}:4444/v2`;
neonConfig.fetchEndpoint = (host) => `https://${host}:4444/sql`;

const pool = new Pool({ connectionString: url });
const { rows } = await pool.query("SELECT count(*)::int AS n FROM e2e");
await pool.end();
if (rows[0].n !== expected) {
  throw new Error(`WebSocket: expected ${expected} rows, got ${rows[0].n}`);
}

const sql = neon(url);
const viaHttp = await sql`SELECT count(*)::int AS n FROM e2e`;
if (viaHttp[0].n !== expected) {
  throw new Error(`HTTP: expected ${expected} rows, got ${viaHttp[0].n}`);
}
console.log(`ok   @neondatabase/serverless read ${expected} rows over WebSocket and over HTTP`);
```

Run `pnpm install` at the repo root. It must resolve `@neondatabase/serverless` 1.1.x within the minimum release age, without any exemption.

- [x] **Step 2: The e2e script**

`scripts/neon-e2e.sh`:

```bash
#!/usr/bin/env bash
# Live check of roadmap step 3 (needs just neon-up and just dev). A hand-made compute:
# - answers through Neon's proxy over Postgres wire, SQL over HTTP and the serverless driver;
# - refuses a wrong password;
# - cancels a running query;
# - takes a changed password at once (metadata, then notify-attach, then /password_updated).
# KEEP=1 leaves the compute running on failure.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"
INTERNAL="${CHELADB_INTERNAL_URL:-http://127.0.0.1:8083}"
STORCON="${STORCON_URL:-http://127.0.0.1:1234}"
HOOK_TOKEN="${CHELADB_STORCON_HOOK_TOKEN:-cheladb-dev-storcon-hook}"
compose=(docker compose -f infra/compose/docker-compose.yml --env-file infra/compose/.env.example)
tmp="$(mktemp -d)"

pass() { echo "ok   $*"; }
die() {
  echo "FAIL $*" >&2
  exit 1
}
metadata_sql() { "${compose[@]}" exec -T metadata-db psql -U cheladb -d cheladb_metadata -v ON_ERROR_STOP=1 -qtA "$@"; }
cleanup() {
  if [[ "${KEEP:-}" != 1 ]]; then scripts/neon-compute-demo.sh --down >/dev/null || true; fi
  rm -rf "$tmp"
}

scripts/neon-compute-demo.sh >/dev/null
trap cleanup EXIT
# shellcheck disable=SC1091
source .cheladb/neon/demo-compute.env
ca="$root/.cheladb/neon/proxy-ca.crt"
host="$DEMO_ENDPOINT.localtest.me"

# sql <password> <psql args…>
sql() {
  local pw="$1"
  shift
  PGPASSWORD="$pw" PGCONNECT_TIMEOUT=10 psql \
    "host=$host port=4432 user=postgres dbname=postgres sslmode=verify-full sslrootcert=$ca" \
    -v ON_ERROR_STOP=1 -qtA "$@"
}
# http_sql <password> <query>: prints the HTTP status; the body goes to $tmp/body.
http_sql() {
  curl -sS --max-time 30 --cacert "$ca" -o "$tmp/body" -w '%{http_code}' -X POST "https://$host:4444/sql" \
    -H "Neon-Connection-String: postgresql://postgres:$1@$host/postgres" \
    -H 'content-type: application/json' \
    -d "$(jq -nc --arg q "$2" '{query: $q, params: []}')"
}

# 1. The compute pulls its spec and answers through the proxy.
for i in $(seq 90); do
  if sql "$DEMO_PASSWORD" -c 'select 1' >/dev/null 2>"$tmp/err"; then break; fi
  [[ $i -lt 90 ]] || die "the compute never answered through the proxy: $(cat "$tmp/err")"
  sleep 2
done
pass "the compute answers through the proxy on 4432"

# 2. Postgres wire.
sql "$DEMO_PASSWORD" -c 'CREATE TABLE e2e (n int); INSERT INTO e2e VALUES (1), (2), (3);'
pass "psql created a table and inserted 3 rows"

# 3. SQL over HTTP.
status="$(http_sql "$DEMO_PASSWORD" 'SELECT count(*)::int AS n FROM e2e')"
[[ "$status" == 200 && "$(jq -r '.rows[0].n' "$tmp/body")" == 3 ]] ||
  die "SQL over HTTP: $status $(cat "$tmp/body")"
pass "SQL over HTTP on 4444 read the rows back"

# 4. The serverless driver, over WebSocket and HTTP.
DATABASE_URL="postgresql://postgres:$DEMO_PASSWORD@$host/postgres" NODE_EXTRA_CA_CERTS="$ca" EXPECT=3 \
  pnpm --filter @cheladb/neon-e2e --silent check || die "@neondatabase/serverless"

# 5. A wrong password is refused.
if sql wrong-password -c 'select 1' >/dev/null 2>"$tmp/err"; then die "a wrong password connected"; fi
grep -qi 'password authentication failed' "$tmp/err" || die "unexpected refusal: $(cat "$tmp/err")"
pass "a wrong password is refused"

# 6. Query cancel goes through the proxy (it keeps cancel keys in Valkey).
started=$SECONDS
PGPASSWORD="$DEMO_PASSWORD" timeout -s INT 2 psql \
  "host=$host port=4432 user=postgres dbname=postgres sslmode=verify-full sslrootcert=$ca" \
  -c 'SELECT pg_sleep(30)' >/dev/null 2>"$tmp/err" || true
[[ $((SECONDS - started)) -lt 10 ]] || die "cancel took $((SECONDS - started)) s"
grep -q 'canceling statement due to user request' "$tmp/err" || die "query cancel: $(cat "$tmp/err")"
pass "query cancel works through the proxy"

# 7. A changed password works at once. Order: metadata, computes, then the proxy's cache.
new="$(od -An -N16 -tx1 /dev/urandom | tr -d ' \n')"
verifier="$(printf '%s' "$new" | cargo run -q -p cheladb -- neon verifier)"
metadata_sql -v ep="$DEMO_ENDPOINT" -v v="$verifier" \
  <<<"UPDATE neon_roles SET scram_verifier = :'v' WHERE endpoint_id = :'ep' AND name = 'postgres';"
# The proxy still has the old secret cached: this proves the cache is on.
sql "$DEMO_PASSWORD" -c 'select 1' >/dev/null 2>"$tmp/err" ||
  die "the old password stopped working before the cache was cleared (is the cache on?): $(cat "$tmp/err")"
pass "the proxy serves the cached secret until it's told otherwise"
status="$(curl -sS --max-time 60 -o "$tmp/body" -w '%{http_code}' -X PUT "$INTERNAL/storcon/notify-attach" \
  -H "Authorization: Bearer $HOOK_TOKEN" -H 'content-type: application/json' \
  -d "{\"tenant_id\":\"$DEMO_TENANT\",\"preferred_az\":null,\"stripe_size\":null,\"shards\":[]}")"
[[ "$status" == 200 ]] || die "notify-attach: $status $(cat "$tmp/body")"
receivers="$(cargo run -q -p cheladb -- neon password-updated --endpoint "$DEMO_ENDPOINT" --role postgres)" ||
  die "password-updated"
sql "$new" -c 'select 1' >/dev/null 2>"$tmp/err" || die "the new password was refused right after the change: $(cat "$tmp/err")"
if sql "$DEMO_PASSWORD" -c 'select 1' >/dev/null 2>&1; then die "the old password still connects on 4432"; fi
[[ "$(http_sql "$new" 'select 1')" == 200 ]] || die "the new password over HTTP: $(cat "$tmp/body")"
[[ "$(http_sql "$DEMO_PASSWORD" 'select 1')" != 200 ]] || die "the old password still works over HTTP"
pass "a changed password works at once ($receivers proxy notified); the old one is refused (4432 and /sql)"

# 8. Clean up; the tenant is gone.
trap 'rm -rf "$tmp"' EXIT
scripts/neon-compute-demo.sh --down >/dev/null
status="$(curl -sS --max-time 30 -o /dev/null -w '%{http_code}' "$STORCON/control/v1/tenant/$DEMO_TENANT")"
[[ "$status" == 404 ]] || die "tenant $DEMO_TENANT still exists (HTTP $status)"
pass "neon-compute-demo --down removed the compute and the tenant"
echo "all passed"
```

`chmod +x scripts/neon-e2e.sh`.

justfile, after `neon-compute-demo`:

```
# Live check of step 3: psql, SQL over HTTP and the serverless driver through the proxy, query
# cancel, and a password change that works at once (needs just neon-up and just dev)
neon-e2e:
    scripts/neon-e2e.sh
```

- [x] **Step 3: Run it live**

Run: `just neon-e2e`
Expected: every check prints `ok`, then `all passed`.

On failure, rerun with `KEEP=1 just neon-e2e`, then inspect:
- the compute's `docker logs`;
- `docker compose … logs proxy`;
- `.cheladb/dev/orchestrator.log`.

Known risks, from the spec:
- **The driver's WebSocket URL.** If the proxy refuses the path, check `@neondatabase/serverless`'s `wsProxy` docs for 1.1 and adjust `check.mjs`.
- **Node and the CA.** `NODE_EXTRA_CA_CERTS` must be an absolute path; it is.

Remove the demo with `just neon-compute-demo --down` when done.

- [x] **Step 4: Commit**

```bash
git add scripts/neon-e2e.sh packages/neon-e2e justfile pnpm-lock.yaml
git commit -m "neon-e2e: psql, SQL over HTTP and the serverless driver through the proxy

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01KESSEqEbiuKm7pMwCPe2Wg"
```

---

### Task 11: Docs, smoke hint, full verification

**Files:**
- Modify: `README.md`, `docs/roadmap/2026-09-28-neon-and-data-api.md`, `scripts/neon-smoke.sh`

- [ ] **Step 1: neon-smoke's hint**

In `scripts/neon-smoke.sh`, change the pending-notification line:
- the comment above it becomes `# The orchestrator's internal listener answers the hook; without it the notification stays pending.`
- the `die` message becomes `"tenant $tenant's compute notification is still pending (is the orchestrator running? just dev): $shard"`.

- [ ] **Step 2: README**

In the Neon section of `README.md`, add a subsection **"Proxy and computes (step 3)"** covering:
- **Ports:**
  - the orchestrator's internal listener on `127.0.0.1:8083`, for the proxy, compute_ctl and the storage controller; each has its own token, and nothing goes through the gateway;
  - the proxy on `127.0.0.1:4432` (Postgres) and `127.0.0.1:4444` (WebSocket and SQL over HTTP), for `<endpoint>.localtest.me`.
- **`just neon-compute-demo`:** what it creates, where the password and CA go, and `--down`.
- **`just neon-e2e`:** what it proves.
- **Valkey** (`127.0.0.1:6379`, base profile):
  - it's what makes query cancel work through the proxy;
  - the proxy caches role secrets for at most a minute, and a password change must publish `/password_updated` (`cheladb neon password-updated`) after reconfiguring the compute;
  - the roadmap's "Valkey: where else it helps" lists later uses.
- **Native Linux Docker:** `host.docker.internal` resolves to the bridge gateway, which can't reach a listener on `127.0.0.1`. There, set `CHELADB_INTERNAL_ADDR` to the bridge address (e.g. `172.17.0.1:8083`). Docker Desktop (WSL2) works as is.
- **`just neon-smoke`** now needs the orchestrator running.

- [ ] **Step 3: Roadmap**

In `docs/roadmap/2026-09-28-neon-and-data-api.md`:
- Mark step 3 done in the step list, with a one-line summary.
- Replace the "Resume here (handoff, 2026-09-28)" section's "Next session" list with step 4's starting points:
  - provisioning into `neon_endpoints`;
  - the Docker `ComputeBackend`;
  - reset password in the step 3 order: rewrite `neon_roles`, reconfigure, then `ProxyCache::password_updated`, failing when no proxy received it.
- Keep the environment findings and the "Valkey: where else it helps" section.
- Add any new findings from Tasks 8–10 (proxy flags, driver URL, timing).

- [ ] **Step 4: Full verification**

Run, in order:

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo test -p cheladb-metadata --test neon_live -- --ignored --test-threads=1
cargo test -p cheladb-neon --test proxy_cache_live -- --ignored
just neon-test
just neon-smoke
just neon-e2e
just auth-e2e
```

Expected: all green. `just auth-e2e` checks that the orchestrator's second listener didn't disturb sign-in through the gateway. Wait a minute between `auth-e2e` runs for its rate limit.

If `cargo test --workspace` fails binding `127.0.0.1:0`, wait a minute and rerun; see Global Constraints.

- [ ] **Step 5: Commit**

```bash
git add README.md docs scripts/neon-smoke.sh
git commit -m "docs: step 3 (proxy, internal listener, neon-e2e); roadmap handoff to step 4

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01KESSEqEbiuKm7pMwCPe2Wg"
```

- [ ] **Step 6: Whole-branch review**

Dispatch one reviewer on the most capable model over `git diff main...neon-control-plane-listener`, with the spec and this plan. It should focus on:
- token separation;
- status codes the storage controller treats as fatal;
- secrets in logs;
- the Review Focus list.

Fix what it finds, rerun Step 4, and report to the user. Don't merge until the user says so.
