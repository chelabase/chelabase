# `crates/cheladb-neon` (roadmap step 2)

**Date:** 2026-09-28
**Status:** Design approved in conversation; spec awaiting review.
**Roadmap:** step 2 of `docs/roadmap/2026-09-28-neon-and-data-api.md`.

## Goal

A library crate with the Neon-facing building blocks the control plane needs in steps 3
and 4, ported from the old repo with tests:

- a storage-controller client
- the compute spec that `compute_ctl` pulls
- SCRAM-SHA-256 verifiers for the proxy and the spec
- Postgres LSNs
- the Ed25519 signer and JWKS for `compute_ctl`
- Neon ids

Nothing calls the crate yet. HTTP routes, the Docker compute backend and any database
schema stay out; they come in steps 3 and 4.

Before the crate, the workspace moves to edition 2024 in a commit of its own.

## Decisions

| Question | Decision |
|---|---|
| Crate layout | **One crate**, one module per concern. A pure/HTTP split buys nothing: every caller already has `reqwest`. |
| Dependency versions | **Latest releases**, declared in the crate's own `Cargo.toml`. The workspace pins other crates use (`reqwest 0.12`, `jsonwebtoken 9`, `base64 0.22`) stay as they are; moving them is a separate change. |
| Edition | **2024 for the whole workspace**, first commit on the branch. The crate uses `edition.workspace = true`. There is no edition 2026; rustc 1.98.1 rejects it. |
| Storage-controller auth | None. The Compose storage controller runs with `--dev`. |
| Storcon tests | **In-process mock server** for request and error handling, plus an **`#[ignore]`d live test** against `just neon-up`. |
| Our ids to Neon ids | `NeonId::from(Uuid)` gives the uuid's 32-hex form, so a database's tenant id comes from its id and needs no extra column. |

## Edition 2024 (first commit)

- `Cargo.toml`: `[workspace.package] edition = "2024"`.
- Run `cargo fix --edition --allow-dirty -p <crate>` for each member **before** switching
  the edition, then switch, then `cargo check --workspace --all-targets`.
- A search found none of the constructs edition 2024 breaks outright (`env::set_var`,
  `env::remove_var`, `extern` blocks, `#[no_mangle]`, `gen` as an identifier). Expect at
  most `cargo fix` edits for `impl Trait` lifetime capture and `if let`/tail-expression
  temporary scope.
- **No `cargo fmt`.** Edition 2024 changes rustfmt's default style edition, but existing
  code stays as formatted. New files in `cheladb-neon` may be formatted individually with
  `rustfmt --edition 2024 <file>`.
- Done when `cargo test --workspace` passes as it did before the change. Tests that need
  services (`just test` prerequisites) run the same way as before.

## What is ported from the old repo

Source: `<repo root>/services/controlplane/src` (edition 2024, same
dependency majors as below, so most code comes over nearly verbatim).

| Old path | New path | Changes |
|---|---|---|
| `ids.rs` | `src/id.rs` | `neon_id()` becomes the `NeonId` newtype. `prefixed()` is dropped; the scaffold has its own ids. |
| `lsn.rs` | `src/lsn.rs` | none |
| `scram.rs` | `src/scram.rs` | `generate_password()` is dropped; the scaffold already generates owner passwords. `verifier_with_salt` becomes `pub`. |
| `compute.rs` | `src/spec.rs` | ids typed as `NeonId` where they are Neon ids |
| `jwt.rs` | `src/jwt.rs` | test keys come from `ed25519-dalek` instead of shelling out to `openssl` |
| `storcon.rs` | `src/storcon.rs` | typed ids, `describe_tenant`, request timeout, injectable delete-poll interval |

Not ported here: `api/*`, `backend.rs`, `docker.rs`, `k8s.rs`, `config.rs`, `error.rs`
(`ApiError` is HTTP-layer; step 3 maps `StorconError` into its own error type),
`reconcile.rs` and the migrations.

## Crate: `crates/cheladb-neon`

`src/lib.rs` declares `pub mod id; pub mod jwt; pub mod lsn; pub mod scram; pub mod spec;
pub mod storcon;` and re-exports `NeonId`, `Lsn`, `ComputeSigner`, `StorconClient` and
`StorconError`.

### `id`

```rust
pub struct NeonId(String);            // always 32 lowercase hex chars

impl NeonId {
    pub fn new() -> Self;             // random (uuid v4, simple form)
    pub fn as_str(&self) -> &str;
}
impl From<Uuid> for NeonId;           // uuid.simple()
impl FromStr for NeonId;              // rejects anything but 32 lowercase hex chars
impl Display for NeonId;
// Serialize / Deserialize as a string; Deserialize validates like FromStr.
// Clone, Debug, PartialEq, Eq, Hash.
```

### `lsn`

`Lsn(pub u64)` with `FromStr` (`0/1F98488`), `Display` (uppercase hex), ordering.
Unchanged.

### `scram`

```rust
pub fn verifier(password: &str) -> String;   // random 16-byte salt, 4096 iterations
pub fn verifier_with_salt(password: &str, salt: &[u8], iterations: u32) -> String;
```

Output: `SCRAM-SHA-256$<iterations>:<salt>$<StoredKey>:<ServerKey>`, standard base64.
Salt from `getrandom`.

### `spec`

`RoleSpec { name, scram_verifier }`, `DatabaseSpec { name, owner }`, and
`SpecInputs { endpoint_id: &str, tenant_id: &NeonId, timeline_id: &NeonId,
pageserver_connstring, safekeeper_connstrings, roles, databases, jwks }`.

`config_response(&SpecInputs) -> serde_json::Value` returns
`{status: "attached", spec: {...}, compute_ctl_config: {jwks}}` with the same settings
as the old repo (port 5432, `shared_preload_libraries=neon`, `wal_level=logical`,
`synchronous_standby_names=walproposer`, `password_encryption=scram-sha-256`, …).
`endpoint_id` stays a plain string: endpoints use our own ids, not Neon ids.

### `jwt`

`ComputeSigner::new(private_pem, public_x)` parses a PKCS#8 Ed25519 key, derives `kid` =
hex of the first 8 bytes of `sha256(public_x)`, and fails unless a self-signed token
verifies against `public_x`. `jwks()` returns the one-key JWKS for `compute_ctl_config`.
`token_for(compute_id)` signs `{compute_id, exp: now + 300}`.

### `storcon`

```rust
pub enum StorconError {
    Rejected { status: StatusCode, message: String },  // 4xx, message from {"msg"} or body
    Other(anyhow::Error),                              // transport, 5xx, bad JSON, timeouts
}

pub struct StorconClient { /* reqwest::Client, base, tenant_secondaries, delete_poll */ }

impl StorconClient {
    pub fn new(base: &str, tenant_secondaries: u32) -> Self;
    pub fn with_delete_poll(self, interval: Duration, attempts: u32) -> Self; // tests

    pub async fn create_tenant(&self, tenant: &NeonId) -> Result<(), StorconError>;
    pub async fn describe_tenant(&self, tenant: &NeonId) -> Result<TenantDescribe, StorconError>;
    pub async fn delete_tenant(&self, tenant: &NeonId) -> Result<(), StorconError>;

    pub async fn create_timeline(&self, tenant: &NeonId, timeline: &NeonId, pg_version: u32,
        ancestor: Option<(&NeonId, Option<Lsn>)>) -> Result<TimelineInfo, StorconError>;
    pub async fn get_timeline(&self, tenant: &NeonId, timeline: &NeonId)
        -> Result<TimelineInfo, StorconError>;
    pub async fn delete_timeline(&self, tenant: &NeonId, timeline: &NeonId)
        -> Result<(), StorconError>;

    pub async fn pageserver_connstring(&self, tenant: &NeonId) -> Result<String, StorconError>;
    pub async fn active_safekeepers(&self) -> Result<Vec<String>, StorconError>;
}

pub struct TimelineInfo { pub ancestor_lsn: Option<Lsn>, pub last_record_lsn: Lsn }

pub struct TenantDescribe { pub shards: Vec<ShardStatus> }
pub struct ShardStatus {
    pub is_pending_compute_notification: bool,
    pub is_reconciling: bool,
    pub last_error: String,              // "" when there is none
}
```

Endpoints, as in the old repo and `scripts/neon-smoke.sh`:

| Method | Path |
|---|---|
| `create_tenant` | `POST /v1/tenant` `{new_tenant_id, placement_policy: {Attached: n}}` |
| `describe_tenant` | `GET /control/v1/tenant/{id}` |
| `delete_tenant` | `DELETE /v1/tenant/{id}` |
| `create_timeline` | `POST /v1/tenant/{id}/timeline` |
| `get_timeline` | `GET /v1/tenant/{id}/timeline/{tl}` |
| `delete_timeline` | `DELETE /v1/tenant/{id}/timeline/{tl}` |
| `pageserver_connstring` | `GET /debug/v1/tenant/{id}/locate` → `postgresql://no_user@host:port` of the first shard |
| `active_safekeepers` | `GET /control/v1/safekeeper`, `scheduling_policy == "Active"`, as `host:port` |

Behaviour:
- **Timeout:** 30 s per request.
- **Deletes are asynchronous:** poll while the answer is 202; 200 or 404 means gone.
  Default 120 attempts × 500 ms, then `Other("timed out …")`. Any other status goes
  through the normal error mapping.
- `Lsn` fields parse on deserialize; a malformed LSN is `Other`.
- Unknown JSON fields are ignored, so upstream additions don't break us.

### Dependencies

Declared in `crates/cheladb-neon/Cargo.toml` (versions current on crates.io, 2026-09-28):

| Crate | Version | Features |
|---|---|---|
| reqwest | 0.13.5 | default off; `json`, `rustls` |
| jsonwebtoken | 11.1.0 | `use_pem`, `rust_crypto` |
| base64 | 0.23.1 | |
| sha2 / hmac / pbkdf2 | 0.11.0 / 0.13.0 / 0.13.0 | |
| getrandom | 0.4.3 | |
| hex | 0.4.3 | |
| uuid | 1.26 | `v4` |
| serde, serde_json, thiserror, anyhow | latest 1.x / 2.x | serde `derive` |
| tokio | 1.53 | `time` |
| dev: tokio | 1.53 | `macros`, `rt-multi-thread`, `net` |
| dev: axum | 0.8.9 | |
| dev: ed25519-dalek | 3.0.0 | `pkcs8`, `pem`, and what key generation needs |

The exact reqwest TLS feature name and the ed25519-dalek key-generation features get
checked against the published crates when the plan is written.

`Cargo.lock` then holds two versions of reqwest, jsonwebtoken and base64 until the other
crates move up. That's recorded as a follow-up in the roadmap.

## Testing

**Unit tests** (in the modules, as in the old repo):
- `id`: random ids are 32 lowercase hex; `From<Uuid>` matches `simple()`; `FromStr` and
  `Deserialize` reject uppercase, dashes, wrong length.
- `lsn`: parse, order, display; malformed inputs rejected.
- `scram`: the RFC 7677 vector (`pencil`, salt `W22ZaJ0SNY7soEsUEjb6gQ==`); two calls to
  `verifier` give different salts.
- `spec`: the attached-spec test from the old repo.
- `jwt`: a token verifies against a `DecodingKey` built from the published JWK; a
  mismatched public key is refused by `new`.

**`tests/storcon_mock.rs`**: an axum server on `127.0.0.1:0`, recording requests:
- `create_tenant` sends `new_tenant_id` and `{"Attached": n}`.
- `create_timeline` with and without an ancestor sends the expected body; the response
  parses into `TimelineInfo` with `Lsn`s.
- A 409 with `{"msg": "exists"}` becomes `Rejected { 409, "exists" }`; a 400 with a
  plain-text body keeps the body as the message; a 500 becomes `Other`.
- Delete: 202, 202, then 200 succeeds after three calls; 404 succeeds at once; always
  202 times out (with a 1 ms interval and 3 attempts).
- `pageserver_connstring` formats the first shard; no shards is `Other`.
- `active_safekeepers` keeps only `Active`.
- `describe_tenant` parses the shard fields.

**`tests/storcon_live.rs`**, `#[ignore]`: against `CHELADB_STORCON_URL`
(default `http://127.0.0.1:1234`) with `tenant_secondaries = 0`, the same round trip as
`just neon-smoke`:
create tenant → describe (no shard pending a compute notification, no `last_error`) →
create `main` timeline (pg 17) → get timeline → `pageserver_connstring` is
`postgresql://no_user@pageserver:6400` → `active_safekeepers` is non-empty →
delete timeline → delete tenant. Cleans up the tenant even when an assertion fails.

**Recipes:** `just neon-test-live` runs `cargo test -p cheladb-neon -- --ignored`
(needs `just neon-up`). The ordinary suite runs without Docker.

## Done when

- The edition commit is in and `cargo test --workspace` passes as before.
- `cargo test -p cheladb-neon` passes without Docker.
- `just neon-test-live` passes against `just neon-up`.
- `cargo clippy -p cheladb-neon --all-targets` is clean.
- The roadmap records step 2 as done, with findings and the dependency follow-up.

## Working rules

As in the roadmap: feature branch `neon-crate`, no push, no merge until the user says;
commit as `<maintainer>`; no workspace-wide `cargo fmt`; don't touch the demo databases,
`secrets.key` or the gateway key pair.
