# Database Lifecycle Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Databases get a platform superuser and a revealable, resettable owner role, both with passwords encrypted at rest. They can be deleted as tombstones, and a reconcile loop cleans up stale and orphaned resources. Studio exposes connection details, reset password and delete.

**Architecture:**
- **`cheladb-secrets`** (new crate): AES-256-GCM with the database id bound in, and a key file created by bootstrap.
- **Metadata:** a migration replaces the plaintext `admin_password` with encrypted columns, adds the tombstone status, and makes names unique among live rows only.
- **Worker** (split into a lib with pure, tested modules):
  - provisions with a named volume, the platform role and an owner role
  - handles durable delete jobs and a reset-password RPC
  - runs a reconcile loop
- **Orchestrator:** adds reveal, reset and delete endpoints, declared in `cheladb-routes`.
- **Studio:** adds per-database actions.

**Tech Stack:** Rust (axum, sqlx, bollard 0.18, async-nats 0.41, tokio-postgres 0.7, aes-gcm 0.10), React + Vitest, Postgres 17, just.

**Spec:** `docs/superpowers/specs/2026-09-28-db-lifecycle-design.md`

## Global Constraints

- **Branch** `feat/db-lifecycle`. Commit with `git commit …` and end every message with:
  ```
  Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
  Claude-Session: https://claude.ai/code/session_01KESSEqEbiuKm7pMwCPe2Wg
  ```
- **Roles:** platform role `cheladb_platform` (SUPERUSER, never returned by any API); owner role `postgres`.
- **Passwords:** 32 alphanumeric characters.
- **Superuser switch:** `CHELADB_DB_OWNER_SUPERUSER=true` makes the owner `SUPERUSER`; it applies to newly created databases only.
- **Secrets key:** a file named by `CHELADB_SECRETS_KEY`, containing base64 of 32 bytes.
  - Stored format: `v1:` + base64(nonce12 ‖ ciphertext).
  - Associated data: the database id as a string.
- **Volume:** `cheladb-db-<database_id>`, with labels `cheladb.managed=true` and `cheladb.database_id=<id>`.
- **Messaging:**
  - delete: JetStream subject `cheladb.delete`, durable consumer `cheladb-worker-delete`
  - reset password: core request/reply `cheladb.rpc.reset_password`, with a 15 s timeout in the orchestrator
- **Reconcile:** every 30 s. Stale provisioning is anything older than 600 s; a stale delete is `deleting` for longer than 60 s.
- **No secrets anywhere else:** no API response except `GET /v1/databases/{id}/connection` includes a password, and no log line ever does.
- **`cargo clippy --workspace --all-targets` stays warning-free.**

## Review Focus

1. **Deleting while provisioning is still running:** the worker must not mark the database `ready` after the delete. It must tear down instead. (Task 4: `should_abort_provision` test, and the end-to-end run)
2. **Reveal for a database created before this change** (no encrypted password): `409` "credentials unavailable; recreate this database", not `500`. (Task 5)
3. **Name reuse after delete, and the duplicate check among live rows:** creating `web1` again after deleting it succeeds; two live `web1`s still get `409`. (Task 2 migration plus the end-to-end run)
4. **Orphan cleanup must never touch a live database's container or volume**, including one mid-provisioning (row `pending`, container just created). (Task 3 reconcile tests)
5. **The reset RPC with the worker down:** `504` within about 15 s, and the stored password unchanged. (Task 5: fake timeout test)

---

## File Structure

| Path | Status | Responsibility |
| --- | --- | --- |
| `crates/cheladb-secrets/` | create | `SecretKey` (load / generate), `seal`, `open` |
| `apps/cheladb/src/auth/secrets_key.rs`, `bootstrap.rs`, `paths.rs` | create/modify | Create the key file once; write `CHELADB_SECRETS_KEY` into `demo.env` |
| `crates/cheladb-metadata/migrations/20260928000001_db_lifecycle.sql` | create | Encrypted columns, `deleted_at`, drop `admin_password`, partial unique index |
| `crates/cheladb-metadata/src/lib.rs` | modify | Record fields, `Deleted` status, `set_ready` (new signature), `begin_delete`, `mark_deleted`, `set_owner_password_enc`, `list_live_for_reconcile`; overview hides deleted |
| `crates/cheladb-nats/src/lib.rs` | modify | Delete subject, consumer and publish; stream subject update; reset-password RPC types |
| `apps/worker/src/{lib.rs,names.rs,roles.rs,reconcile.rs,credentials.rs}` | create | Pure logic: name sanitizing, role SQL, reconcile planning, password generation |
| `apps/worker/src/{docker_ops.rs,main.rs}` | create/modify | Container and volume ops; provision, delete, reset, reconcile wiring |
| `apps/orchestrator/src/lib.rs`, `main.rs`, `tests/api.rs` | modify | Reveal, reset, delete; `JobPort` delete and reset; secret key in state |
| `crates/cheladb-routes/src/lib.rs` | modify | `DATABASE_CONNECTION`, `RESET_PASSWORD`, `DELETE_DATABASE` |
| `packages/types/src/index.ts` | modify | `ConnectionDetails`; `deleted` status |
| `apps/studio/src/lib/api.ts`, `components/database-actions.tsx`, `components/dashboard.tsx` (+ tests) | create/modify | Actions UI, and polling while databases are transitional |
| `justfile`, `README.md`, `infra/compose/.env.example` | modify | `worker` sources `demo.env`; `sweep-dbs` removes volumes; docs |

---

### Task 1: `cheladb-secrets` crate + bootstrap key

**Files:**
- Create: `crates/cheladb-secrets/{Cargo.toml,moon.yml,src/lib.rs}`, `apps/cheladb/src/auth/secrets_key.rs`
- Modify: root `Cargo.toml`, `apps/cheladb/{Cargo.toml,src/auth/mod.rs,src/auth/bootstrap.rs,src/paths.rs}`

**Interfaces (produced):**
- `pub struct SecretKey` (Clone):
  - `SecretKey::generate() -> SecretKey`
  - `to_base64(&self) -> String`
  - `from_base64(&str) -> Result<SecretKey, SecretsError>`
  - `from_file(&Path) -> Result<SecretKey, SecretsError>`
  - `from_env() -> Result<SecretKey, SecretsError>` (reads `CHELADB_SECRETS_KEY`)
- `pub fn seal(key: &SecretKey, aad: &str, plaintext: &str) -> String`
- `pub fn open(key: &SecretKey, aad: &str, sealed: &str) -> Result<String, SecretsError>`
- `pub enum SecretsError { Config(String), Malformed, Decrypt }` (thiserror)
- CLI: `secrets_key::ensure(dir: &Path) -> anyhow::Result<PathBuf>`; `paths::secrets_dir()`

- [ ] **Step 1: Scaffold.**
  - Root `Cargo.toml`:
    - add `"crates/cheladb-secrets"` to `members`
    - add `cheladb-secrets = { path = "crates/cheladb-secrets" }`, `aes-gcm = "0.10"` and `tokio-postgres = "0.7"` to `[workspace.dependencies]`
  - Crate manifest:
    - dependencies: `aes-gcm`, `base64`, `thiserror`
    - dev-dependency: `tempfile`
  - `moon.yml` follows `crates/cheladb-routes/moon.yml`, with `-p cheladb-secrets`.

- [ ] **Step 2: Failing tests** (`src/lib.rs`, with stub bodies `todo!()` and the signatures above):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_for_the_same_database() {
        let key = SecretKey::generate();
        let sealed = seal(&key, "db-1", "hunter2hunter2");
        assert!(sealed.starts_with("v1:"));
        assert_eq!(open(&key, "db-1", &sealed).unwrap(), "hunter2hunter2");
    }

    #[test]
    fn each_seal_uses_a_fresh_nonce() {
        let key = SecretKey::generate();
        assert_ne!(seal(&key, "db-1", "same"), seal(&key, "db-1", "same"));
    }

    #[test]
    fn a_sealed_value_only_opens_for_its_own_database() {
        let key = SecretKey::generate();
        let sealed = seal(&key, "db-1", "pw");
        assert!(matches!(open(&key, "db-2", &sealed), Err(SecretsError::Decrypt)));
    }

    #[test]
    fn the_wrong_key_is_rejected() {
        let sealed = seal(&SecretKey::generate(), "db-1", "pw");
        assert!(matches!(open(&SecretKey::generate(), "db-1", &sealed), Err(SecretsError::Decrypt)));
    }

    #[test]
    fn tampering_is_rejected() {
        let key = SecretKey::generate();
        let mut sealed = seal(&key, "db-1", "pw");
        let last = sealed.pop().unwrap();
        sealed.push(if last == 'A' { 'B' } else { 'A' });
        assert!(open(&key, "db-1", &sealed).is_err());
    }

    #[test]
    fn malformed_values_are_rejected() {
        let key = SecretKey::generate();
        for bad in ["", "v2:abc", "v1:not-base64!", "v1:AAAA"] {
            assert!(matches!(open(&key, "db-1", bad), Err(SecretsError::Malformed)), "{bad}");
        }
    }

    #[test]
    fn keys_survive_base64_and_files() {
        let key = SecretKey::generate();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("k");
        std::fs::write(&path, key.to_base64()).unwrap();
        let loaded = SecretKey::from_file(&path).unwrap();
        let sealed = seal(&key, "db-1", "pw");
        assert_eq!(open(&loaded, "db-1", &sealed).unwrap(), "pw");
        assert!(matches!(SecretKey::from_base64("short"), Err(SecretsError::Config(_))));
    }
}
```

- [ ] **Step 3: Run.** `cargo test -p cheladb-secrets` fails; the stubs panic with `not yet implemented`.

- [ ] **Step 4: Implement.**

```rust
//! Encryption at rest for control-plane secrets (database passwords).
//!
//! AES-256-GCM with a random 96-bit nonce. The database id is authenticated
//! associated data, so a ciphertext only opens for the row it was made for.
//! Stored as `v1:` + base64(nonce ‖ ciphertext).

use aes_gcm::aead::{Aead, AeadCore, KeyInit, OsRng, Payload};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use std::path::Path;
use thiserror::Error;

const PREFIX: &str = "v1:";
const NONCE_LEN: usize = 12;

#[derive(Debug, Error)]
pub enum SecretsError {
    #[error("secrets key: {0}")]
    Config(String),
    #[error("malformed sealed value")]
    Malformed,
    #[error("could not decrypt sealed value (wrong key or row)")]
    Decrypt,
}

#[derive(Clone)]
pub struct SecretKey(Key<Aes256Gcm>);

impl SecretKey {
    pub fn generate() -> Self {
        Self(Aes256Gcm::generate_key(&mut OsRng))
    }

    pub fn to_base64(&self) -> String {
        STANDARD.encode(self.0.as_slice())
    }

    pub fn from_base64(encoded: &str) -> Result<Self, SecretsError> {
        let bytes = STANDARD
            .decode(encoded.trim())
            .map_err(|e| SecretsError::Config(format!("not base64: {e}")))?;
        if bytes.len() != 32 {
            return Err(SecretsError::Config(format!("expected 32 bytes, got {}", bytes.len())));
        }
        Ok(Self(*Key::<Aes256Gcm>::from_slice(&bytes)))
    }

    pub fn from_file(path: &Path) -> Result<Self, SecretsError> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| SecretsError::Config(format!("read {}: {e}", path.display())))?;
        Self::from_base64(&text)
    }

    /// From the file named by `CHELADB_SECRETS_KEY`.
    pub fn from_env() -> Result<Self, SecretsError> {
        let path = std::env::var("CHELADB_SECRETS_KEY").map_err(|_| {
            SecretsError::Config(
                "CHELADB_SECRETS_KEY is not set; run `just zitadel-bootstrap` and load .cheladb/zitadel/demo.env".into(),
            )
        })?;
        Self::from_file(Path::new(&path))
    }
}

impl std::fmt::Debug for SecretKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SecretKey(..)")
    }
}

pub fn seal(key: &SecretKey, aad: &str, plaintext: &str) -> String {
    let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
    let ciphertext = Aes256Gcm::new(&key.0)
        .encrypt(&nonce, Payload { msg: plaintext.as_bytes(), aad: aad.as_bytes() })
        .expect("AES-GCM encryption cannot fail for in-memory input");
    let mut bytes = nonce.to_vec();
    bytes.extend_from_slice(&ciphertext);
    format!("{PREFIX}{}", STANDARD.encode(bytes))
}

pub fn open(key: &SecretKey, aad: &str, sealed: &str) -> Result<String, SecretsError> {
    let encoded = sealed.strip_prefix(PREFIX).ok_or(SecretsError::Malformed)?;
    let bytes = STANDARD.decode(encoded).map_err(|_| SecretsError::Malformed)?;
    if bytes.len() <= NONCE_LEN {
        return Err(SecretsError::Malformed);
    }
    let (nonce, ciphertext) = bytes.split_at(NONCE_LEN);
    let plaintext = Aes256Gcm::new(&key.0)
        .decrypt(Nonce::from_slice(nonce), Payload { msg: ciphertext, aad: aad.as_bytes() })
        .map_err(|_| SecretsError::Decrypt)?;
    String::from_utf8(plaintext).map_err(|_| SecretsError::Malformed)
}
```

`"v1:AAAA"` decodes to 3 bytes, which is ≤ 12, so it is `Malformed` as the test expects.

- [ ] **Step 5: Bootstrap key.**
  - `apps/cheladb/src/auth/secrets_key.rs` provides `ensure(dir) -> PathBuf`: it creates `dir/secrets.key` with `create_new` + mode 0600, containing `SecretKey::generate().to_base64()`, reuses it if present, and returns the canonical path. Tests:
    - creates a loadable key (`SecretKey::from_file` works)
    - a second call keeps the same bytes
    - the file is 0600
  - `paths::secrets_dir()` returns `CHELADB_SECRETS_DIR` or `.cheladb/secrets`.
  - In bootstrap `run()`, next to `internal_keys::ensure`, call `let secrets_key = super::secrets_key::ensure(&secrets_dir())?;`.
  - Append `CHELADB_SECRETS_KEY={}` (the path) to the `demo.env` format.
  - `apps/cheladb/Cargo.toml`: add `cheladb-secrets`.
- [ ] **Step 6: Run.** `cargo test -p cheladb-secrets -p cheladb` should pass (7 secrets tests plus 3 key tests).
- [ ] **Step 7: Commit.** Message: `Add cheladb-secrets (AES-256-GCM, row-bound) and bootstrap secrets key`.

---

### Task 2: Metadata — migration and credential/tombstone API

**Files:** Create `crates/cheladb-metadata/migrations/20260928000001_db_lifecycle.sql`. Modify `crates/cheladb-metadata/src/lib.rs`, plus the minimal compile fixes in `apps/orchestrator/src/lib.rs`, `apps/orchestrator/tests/api.rs` and `apps/worker/src/main.rs`.

**Interfaces (produced):**
- `DatabaseStatus::Deleted` (`"deleted"`).
- `DatabaseRecord`: `admin_password` is removed; it gains `database_name: Option<String>`, `owner_role: Option<String>`, `owner_password_enc: Option<String>`, `platform_password_enc: Option<String>`.
- `MetadataClient`:
  - `set_ready(&self, id, ReadyDatabase<'_>)`
  - `begin_delete(&self, id) -> Result<bool>`: `true` if moved to `deleting`; `false` if already `deleting`, `deleted` or missing.
  - `mark_deleted(&self, id)`
  - `set_owner_password_enc(&self, id, &str)`
  - `list_live_for_reconcile(&self) -> Result<Vec<LiveDatabase>>`
- `pub struct ReadyDatabase<'a> { container_id, host, port: u16, database_name, owner_role, owner_password_enc, platform_password_enc }` (all `&'a str` except `port`).
- `pub struct LiveDatabase { id: DatabaseId, status: DatabaseStatus, age_secs: i64 }`, where `age_secs` is the time since `updated_at`.
- `tenant_overview` excludes `deleted` rows. `get_database` still returns them (the API maps them to `404`).
- The duplicate-name check uses constraint `databases_live_name_key`.

- [ ] **Step 1: Migration.**

```sql
-- Credentials: encrypted at rest (cheladb-secrets), revealed via the API.
ALTER TABLE databases
    ADD COLUMN database_name TEXT,
    ADD COLUMN owner_role TEXT,
    ADD COLUMN owner_password_enc TEXT,
    ADD COLUMN platform_password_enc TEXT,
    ADD COLUMN deleted_at TIMESTAMPTZ,
    DROP COLUMN admin_password;

-- Tombstones: names are unique among databases that aren't deleted.
ALTER TABLE databases DROP CONSTRAINT databases_project_id_name_key;
CREATE UNIQUE INDEX databases_live_name_key ON databases (project_id, name) WHERE status <> 'deleted';
```

- [ ] **Step 2: Failing tests** (append to the metadata `tests` module; they are pure):

```rust
    #[test]
    fn deleted_status_round_trips() {
        assert_eq!(DatabaseStatus::Deleted.as_str(), "deleted");
        assert_eq!(DatabaseStatus::parse("deleted").unwrap(), DatabaseStatus::Deleted);
    }
```

Run `cargo test -p cheladb-metadata`: it fails to compile, because there is no variant `Deleted`.

- [ ] **Step 3: Implement.**
  1. **`Deleted` variant:** add it to `as_str` and `parse`.
  2. **`DatabaseRecord`:** change the fields as listed above. `database_from_row` reads the new columns, and the `SELECT` lists in `get_database` and `tenant_overview` swap `d.admin_password` for `d.database_name, d.owner_role, d.owner_password_enc, d.platform_password_enc`. `tenant_overview` adds `AND d.status <> 'deleted'`.
  3. **Duplicate constraint:** `insert_database_pending` matches `Some("databases_live_name_key")`.
  4. **`set_ready`:**
     ```rust
     pub async fn set_ready(&self, id: DatabaseId, db: ReadyDatabase<'_>) -> Result<(), MetadataError> {
         sqlx::query(r#"
             UPDATE databases
             SET status = 'ready', container_id = $2, host = $3, port = $4, database_name = $5,
                 owner_role = $6, owner_password_enc = $7, platform_password_enc = $8,
                 error = NULL, updated_at = now()
             WHERE id = $1"#)
             .bind(id.as_uuid()).bind(db.container_id).bind(db.host).bind(i32::from(db.port))
             .bind(db.database_name).bind(db.owner_role).bind(db.owner_password_enc).bind(db.platform_password_enc)
             .execute(&self.pool).await?;
         Ok(())
     }
     ```
  5. **`begin_delete`:**
     ```rust
     pub async fn begin_delete(&self, id: DatabaseId) -> Result<bool, MetadataError> {
         let moved = sqlx::query(
             "UPDATE databases SET status = 'deleting', updated_at = now()
              WHERE id = $1 AND status NOT IN ('deleting', 'deleted')")
             .bind(id.as_uuid()).execute(&self.pool).await?;
         Ok(moved.rows_affected() == 1)
     }
     ```
  6. **`mark_deleted`:**
     ```rust
     pub async fn mark_deleted(&self, id: DatabaseId) -> Result<(), MetadataError> {
         sqlx::query(
             "UPDATE databases SET status = 'deleted', deleted_at = now(), updated_at = now(),
                     container_id = NULL, host = NULL, port = NULL,
                     owner_password_enc = NULL, platform_password_enc = NULL
              WHERE id = $1")
             .bind(id.as_uuid()).execute(&self.pool).await?;
         Ok(())
     }
     ```
  7. **`set_owner_password_enc`:** `UPDATE databases SET owner_password_enc = $2, updated_at = now() WHERE id = $1`.
  8. **`list_live_for_reconcile`:** `SELECT id, status, EXTRACT(EPOCH FROM now() - updated_at)::BIGINT AS age_secs FROM databases WHERE status <> 'deleted'`, mapped to `LiveDatabase`.
  9. **Compile fixes elsewhere:**
     - Orchestrator: drop `admin_password` and `container_id` from `DatabaseResponse`. `get_database` returns `404 "database not found"` when `record.status == DatabaseStatus::Deleted`.
     - Orchestrator tests: `sample_database` builds the new fields (`None`); `overview_with_database` puts a sealed-looking value in `owner_password_enc` instead of `admin_password`; `me_never_exposes_database_secrets` asserts that `"owner_password_enc"`, `"platform_password_enc"`, `"container_id"` and that value never appear.
     - Worker: the temporary call keeps today's behavior. `set_ready(id, ReadyDatabase { container_id, host: "127.0.0.1", port, database_name: &sanitize_db_name(name), owner_role: "cheladb", owner_password_enc: "", platform_password_enc: "" })`. Task 4 replaces it.
  10. **New orchestrator test** (Review Focus 3 at the API level, `404` for tombstones):
      ```rust
      #[tokio::test]
      async fn get_deleted_database_is_404() {
          let metadata = FakeMetadata::default();
          let tenant = TenantId::new();
          metadata.bind_org("org-1", tenant);
          let mut record = sample_database(tenant);
          record.status = DatabaseStatus::Deleted;
          let id = record.database_id;
          metadata.insert_database(record);
          let app = spawn_app(metadata, FakeJobs::default()).await;
          let resp = reqwest::Client::new()
              .get(format!("{}/v1/databases/{}", app.base, id.as_uuid()))
              .header(IDENTITY_HEADER, app.token_for_org("org-1"))
              .send().await.unwrap();
          assert_eq!(resp.status(), 404);
      }
      ```
- [ ] **Step 4: Run.** `cargo test --workspace` is green. `cargo run -q -p orchestrator` against Compose applies the migration once; check it with `psql … -c '\d databases'` showing the new columns and `databases_live_name_key`. Stop it afterwards.
- [ ] **Step 5: Commit.** Message: `Metadata: encrypted credential columns, deleted tombstones, live-name uniqueness`.

---

### Task 3: Worker lib — pure logic (names, roles, passwords, reconcile)

**Files:** Create `apps/worker/src/{lib.rs,names.rs,roles.rs,credentials.rs,reconcile.rs}`. Modify `apps/worker/Cargo.toml` (add a `[lib]`, `tokio-postgres`, `cheladb-secrets`) and `apps/worker/src/main.rs` (use `worker::names::sanitize_db_name`).

**Interfaces (produced):**
- `names::sanitize_db_name(&str) -> String` (moved out of `main.rs`, same behavior), `names::volume_name(DatabaseId) -> String` (`cheladb-db-<uuid>`), `names::database_id_from_volume(&str) -> Option<DatabaseId>`.
- `credentials::generate_password() -> String` (32 alphanumeric characters).
- `roles::PLATFORM_ROLE = "cheladb_platform"`, `roles::OWNER_ROLE = "postgres"`.
- `roles::owner_setup_sql(database_name: &str, owner_password: &str, superuser: bool) -> Vec<String>`: statements run while connected to `postgres`.
- `roles::owner_schema_sql() -> Vec<String>`: statements run while connected to the new database.
- `roles::reset_password_sql(password: &str) -> String`.
- `roles::owner_superuser_from_env(raw: Option<&str>) -> bool` (`true`, `1`, `yes`).
- `reconcile::{Observed, Action, plan(rows: &[LiveDatabase], observed: &Observed) -> Vec<Action>}`, where:
  - `Observed { containers: HashSet<DatabaseId>, volumes: HashSet<DatabaseId> }`
  - `Action::{FailStale(DatabaseId), FailMissingContainer(DatabaseId), RetryDelete(DatabaseId), RemoveOrphan(DatabaseId)}`
- `reconcile::should_abort_provision(status: DatabaseStatus) -> bool` (Review Focus 1).

- [ ] **Step 1: Failing tests** (one `#[cfg(test)]` module per file):

```rust
// names.rs
#[test] fn sanitizes_like_before() {
    assert_eq!(sanitize_db_name("web-1 prod"), "web_1_prod");
    assert_eq!(sanitize_db_name("---"), "___");
    assert_eq!(sanitize_db_name(""), "cheladb");
}
#[test] fn volume_names_round_trip() {
    let id = DatabaseId::new();
    assert_eq!(database_id_from_volume(&volume_name(id)), Some(id));
    assert_eq!(database_id_from_volume("cheladb-db-not-a-uuid"), None);
    assert_eq!(database_id_from_volume("other-volume"), None);
}

// credentials.rs
#[test] fn passwords_are_32_alphanumerics_and_differ() {
    let (a, b) = (generate_password(), generate_password());
    assert_eq!(a.len(), 32);
    assert!(a.chars().all(|c| c.is_ascii_alphanumeric()));
    assert_ne!(a, b);
}

// roles.rs
#[test] fn owner_is_elevated_but_not_superuser_by_default() {
    let sql = owner_setup_sql("web1", "pw123", false).join("\n");
    assert!(sql.contains("CREATE ROLE postgres LOGIN PASSWORD 'pw123' CREATEDB CREATEROLE BYPASSRLS"));
    assert!(!sql.contains("SUPERUSER"));
    assert!(sql.contains("GRANT pg_read_all_data, pg_write_all_data, pg_monitor, pg_signal_backend TO postgres"));
    assert!(sql.contains(r#"ALTER DATABASE "web1" OWNER TO postgres"#));
}
#[test] fn superuser_switch_makes_the_owner_superuser() {
    assert!(owner_setup_sql("web1", "pw", true)[0].ends_with(" SUPERUSER"));
}
#[test] fn schema_is_handed_to_the_owner() {
    assert_eq!(owner_schema_sql(), vec!["ALTER SCHEMA public OWNER TO postgres".to_string()]);
}
#[test] fn reset_sql_targets_the_owner_role() {
    assert_eq!(reset_password_sql("newpw"), "ALTER ROLE postgres PASSWORD 'newpw'");
}
#[test] fn superuser_switch_parses_explicit_true_only() {
    for on in ["true", "1", "YES"] { assert!(owner_superuser_from_env(Some(on))); }
    for off in ["false", "", "nope"] { assert!(!owner_superuser_from_env(Some(off))); }
    assert!(!owner_superuser_from_env(None));
}

// reconcile.rs
fn live(id: DatabaseId, status: DatabaseStatus, age_secs: i64) -> LiveDatabase { LiveDatabase { id, status, age_secs } }

#[test] fn fresh_provisioning_is_left_alone_even_with_its_container() {
    let id = DatabaseId::new();
    let observed = Observed { containers: [id].into(), volumes: [id].into() };
    assert!(plan(&[live(id, DatabaseStatus::Pending, 5)], &observed).is_empty());
}
#[test] fn stale_provisioning_fails() {
    let id = DatabaseId::new();
    assert_eq!(plan(&[live(id, DatabaseStatus::Provisioning, 601)], &Observed::default()), vec![Action::FailStale(id)]);
}
#[test] fn ready_without_container_fails() {
    let id = DatabaseId::new();
    let observed = Observed { containers: HashSet::new(), volumes: [id].into() };
    assert_eq!(plan(&[live(id, DatabaseStatus::Ready, 3600)], &observed), vec![Action::FailMissingContainer(id)]);
}
#[test] fn ready_with_container_is_fine() {
    let id = DatabaseId::new();
    let observed = Observed { containers: [id].into(), volumes: [id].into() };
    assert!(plan(&[live(id, DatabaseStatus::Ready, 3600)], &observed).is_empty());
}
#[test] fn lost_delete_is_retried_after_a_minute() {
    let id = DatabaseId::new();
    assert!(plan(&[live(id, DatabaseStatus::Deleting, 30)], &Observed::default()).is_empty());
    assert_eq!(plan(&[live(id, DatabaseStatus::Deleting, 61)], &Observed::default()), vec![Action::RetryDelete(id)]);
}
// Review Focus 4.
#[test] fn only_resources_without_a_live_row_are_orphans() {
    let (live_id, orphan_c, orphan_v) = (DatabaseId::new(), DatabaseId::new(), DatabaseId::new());
    let observed = Observed { containers: [live_id, orphan_c].into(), volumes: [live_id, orphan_v].into() };
    let mut actions = plan(&[live(live_id, DatabaseStatus::Pending, 1)], &observed);
    actions.sort_by_key(|a| format!("{a:?}"));
    let mut expected = vec![Action::RemoveOrphan(orphan_c), Action::RemoveOrphan(orphan_v)];
    expected.sort_by_key(|a| format!("{a:?}"));
    assert_eq!(actions, expected);
}
#[test] fn an_id_with_both_orphan_container_and_volume_is_removed_once() {
    let id = DatabaseId::new();
    let observed = Observed { containers: [id].into(), volumes: [id].into() };
    assert_eq!(plan(&[], &observed), vec![Action::RemoveOrphan(id)]);
}
// Review Focus 1.
#[test] fn provisioning_aborts_once_delete_has_started() {
    assert!(should_abort_provision(DatabaseStatus::Deleting));
    assert!(should_abort_provision(DatabaseStatus::Deleted));
    assert!(!should_abort_provision(DatabaseStatus::Provisioning));
}
```

- [ ] **Step 2: Run.** `cargo test -p worker --lib` fails to compile until the stubs exist, then the stub-based tests fail.

- [ ] **Step 3: Implement.**

```rust
// roles.rs
pub const PLATFORM_ROLE: &str = "cheladb_platform";
pub const OWNER_ROLE: &str = "postgres";

/// Statements run as the platform role while connected to database `postgres`.
/// Passwords are alphanumeric (credentials::generate_password), so a plain
/// single-quoted literal is safe.
pub fn owner_setup_sql(database_name: &str, owner_password: &str, superuser: bool) -> Vec<String> {
    let mut create = format!(
        "CREATE ROLE {OWNER_ROLE} LOGIN PASSWORD '{owner_password}' CREATEDB CREATEROLE BYPASSRLS"
    );
    if superuser {
        create.push_str(" SUPERUSER");
    }
    vec![
        create,
        format!("GRANT pg_read_all_data, pg_write_all_data, pg_monitor, pg_signal_backend TO {OWNER_ROLE}"),
        format!(r#"ALTER DATABASE "{database_name}" OWNER TO {OWNER_ROLE}"#),
    ]
}

/// Statements run as the platform role while connected to the new database.
pub fn owner_schema_sql() -> Vec<String> {
    vec![format!("ALTER SCHEMA public OWNER TO {OWNER_ROLE}")]
}

pub fn reset_password_sql(password: &str) -> String {
    format!("ALTER ROLE {OWNER_ROLE} PASSWORD '{password}'")
}

pub fn owner_superuser_from_env(raw: Option<&str>) -> bool {
    matches!(raw.map(|v| v.trim().to_ascii_lowercase()).as_deref(), Some("true" | "1" | "yes"))
}
```

`sanitize_db_name` only emits `[A-Za-z0-9_]`, so the quoted identifier is safe.

```rust
// reconcile.rs
use cheladb_metadata::{DatabaseStatus, LiveDatabase};
use cheladb_types::DatabaseId;
use std::collections::{BTreeSet, HashSet};

pub const STALE_PROVISION_SECS: i64 = 600;
pub const STALE_DELETE_SECS: i64 = 60;

#[derive(Debug, Default)]
pub struct Observed {
    pub containers: HashSet<DatabaseId>,
    pub volumes: HashSet<DatabaseId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    FailStale(DatabaseId),
    FailMissingContainer(DatabaseId),
    RetryDelete(DatabaseId),
    /// Container and/or volume whose database has no live row.
    RemoveOrphan(DatabaseId),
}

pub fn plan(rows: &[LiveDatabase], observed: &Observed) -> Vec<Action> {
    let mut actions = Vec::new();
    for row in rows {
        match row.status {
            DatabaseStatus::Pending | DatabaseStatus::Provisioning if row.age_secs > STALE_PROVISION_SECS => {
                actions.push(Action::FailStale(row.id))
            }
            DatabaseStatus::Ready if !observed.containers.contains(&row.id) => {
                actions.push(Action::FailMissingContainer(row.id))
            }
            DatabaseStatus::Deleting if row.age_secs > STALE_DELETE_SECS => {
                actions.push(Action::RetryDelete(row.id))
            }
            _ => {}
        }
    }
    let live: HashSet<DatabaseId> = rows.iter().map(|r| r.id).collect();
    let orphans: BTreeSet<String> = observed
        .containers
        .iter()
        .chain(&observed.volumes)
        .filter(|id| !live.contains(id))
        .map(|id| id.to_string())
        .collect();
    actions.extend(orphans.into_iter().filter_map(|id| id.parse().ok().map(Action::RemoveOrphan)));
    actions
}

pub fn should_abort_provision(status: DatabaseStatus) -> bool {
    matches!(status, DatabaseStatus::Deleting | DatabaseStatus::Deleted)
}
```

If `DatabaseId` isn't `Ord`, the `BTreeSet<String>` dedupes via the display form. `DatabaseId: FromStr` is needed for `parse`; if it's missing, use `Uuid::parse_str` and `DatabaseId(uuid)`. `names.rs`, `credentials.rs` (`rand::distributions::Alphanumeric`, 32 chars) and `lib.rs` (`pub mod names; pub mod roles; pub mod credentials; pub mod reconcile;`) are straightforward.

- [ ] **Step 4: Run.** `cargo test -p worker` passes 16 tests.
- [ ] **Step 5: Commit.** Message: `Worker: pure role SQL, password, naming and reconcile planning`.

---

### Task 4: NATS plumbing and worker I/O (provision, delete, reset RPC, reconcile)

**Files:**
- Modify `crates/cheladb-nats/src/lib.rs` and `apps/worker/src/main.rs`.
- Create `apps/worker/src/docker_ops.rs`.

**Interfaces (produced):**
- `cheladb-nats`:
  - `SUBJECT_DELETE = "cheladb.delete"`, `CONSUMER_DELETE = "cheladb-worker-delete"`, `SUBJECT_RESET_PASSWORD = "cheladb.rpc.reset_password"`
  - `DeleteDatabaseJob { database_id }`, `ResetPasswordRequest { database_id }`, `ResetPasswordReply { ok: bool, error: Option<String> }`
  - `publish_delete(js, &JobEnvelope<DeleteDatabaseJob>)`
  - `run_delete_consumer(js, handler)`: the same loop as provision, generalized. Refactor both into a private generic `run_consumer<T>(js, durable, subject, handler)`.
  - `ensure_control_stream` also **updates** an existing stream so its subjects include provision, reconcile and delete (`get_stream`, compare config subjects, `update_stream` if one is missing).
- Worker I/O in `docker_ops.rs`:
  - `ensure_volume(&Docker, DatabaseId)`
  - `remove_database_resources(&Docker, DatabaseId)`: stops and removes container `cheladb-db-<id>` (ignoring not-found) and removes the volume (`force`, ignoring not-found)
  - `observe(&Docker) -> Observed`: containers and volumes listed with `label=cheladb.managed=true`; ids come from the `cheladb.database_id` label, falling back to the volume name

- [ ] **Step 1: Failing test for the stream subjects** (a pure helper in `cheladb-nats`):

```rust
#[test]
fn stream_subjects_include_delete() {
    assert!(control_subjects().contains(&SUBJECT_DELETE.to_string()));
    assert_eq!(missing_subjects(&["cheladb.provision".into()]), vec![SUBJECT_RECONCILE.to_string(), SUBJECT_DELETE.to_string()]);
}
```

Here `control_subjects() -> Vec<String>` and `missing_subjects(existing: &[String]) -> Vec<String>`. Run `cargo test -p cheladb-nats`: red.

- [ ] **Step 2: Implement `cheladb-nats`.**
  - The helpers above, and `ensure_control_stream` using them to create or update the stream.
  - The delete and RPC types.
  - The generic consumer. `run_provision_consumer` and `run_delete_consumer` become thin wrappers over it.
  - `cargo test -p cheladb-nats` is green.

- [ ] **Step 3: Worker I/O.**
  - **Load settings once at startup:** `let key = SecretKey::from_env()?;` and `let owner_superuser = owner_superuser_from_env(env::var("CHELADB_DB_OWNER_SUPERUSER").ok().as_deref());`.
  - **Provision (`handle_provision`):**
    1. If the row is already `ready`, skip. If `should_abort_provision(status)`, ack and return.
    2. Set the status to `provisioning`.
    3. Generate `platform_pw` and `owner_pw`, then call `ensure_volume`.
    4. Create the container with `POSTGRES_USER=cheladb_platform`, `POSTGRES_PASSWORD=platform_pw` and `POSTGRES_DB=<sanitized>`, the volume mounted with `HostConfig.binds = vec!["cheladb-db-<id>:/var/lib/postgresql/data"]`, and the existing labels and port binding.
    5. Wait for the TCP port, then retry `tokio_postgres::connect("host=127.0.0.1 port=<port> user=cheladb_platform password=<pw> dbname=postgres", NoTls)` for up to 60 s, since the entrypoint restarts Postgres once.
    6. Run `owner_setup_sql`. Connect to the new database and run `owner_schema_sql`.
    7. Re-read the row. If `should_abort_provision`, call `remove_database_resources` and `mark_deleted`, and return.
    8. Otherwise `set_ready(… owner_role: "postgres", owner_password_enc: seal(&key, &id.to_string(), &owner_pw), platform_password_enc: seal(&key, &id.to_string(), &platform_pw) …)`.
  - **Delete (`handle_delete`):** `remove_database_resources` then `mark_deleted`. It must succeed when resources are already gone.
  - **Reset password:**
    - `client.subscribe(SUBJECT_RESET_PASSWORD)` runs in a spawned task.
    - For each message it parses `ResetPasswordRequest` and loads the row. If the row isn't `ready`, the reply is `ok: false`, error `not ready`.
    - Otherwise it opens the platform password, connects to `127.0.0.1:<port>` as `cheladb_platform`, runs `reset_password_sql(new)`, calls `set_owner_password_enc(seal(...))`, and replies `ok: true`.
    - On any error it replies `ok: false` with the error text, never containing the password.
    - Replies go through `client.publish(reply_subject, json)`.
  - **Reconcile:** a spawned task runs every 30 s.
    1. `rows = list_live_for_reconcile()`, `observed = observe(&docker)`.
    2. For each `plan(...)` action:
       - `FailStale`: `remove_database_resources` + `set_failed("provisioning timed out")`
       - `FailMissingContainer`: `set_failed("container missing")`
       - `RetryDelete`: `handle_delete`
       - `RemoveOrphan`: `remove_database_resources`
    3. Log each action at `info` with the id.
  - The worker holds the core `async_nats::Client` (from `connect`) as well as the JetStream context.
- [ ] **Step 4: Run.** `cargo test --workspace` and `cargo clippy --workspace --all-targets` are clean.
- [ ] **Step 5: Live check (Compose running).**
  - `just deps-up`, then source `.cheladb/zitadel/demo.env` (after Task 1 has re-run bootstrap) and start `cargo run -p worker`. `CHELADB_SECRETS_KEY` must be set.
  - Create a database via `just create-db-demo lc1`. It reaches `ready`. Then:
    - `docker volume ls` shows `cheladb-db-<id>`
    - the row has non-null `owner_password_enc` and `platform_password_enc` starting with `v1:`
  - Decrypt the owner password with a throwaway `cargo run -q -p cheladb --example …`, or by temporarily logging it at `debug` in a local, never-committed run. Better: wait for Task 5's reveal endpoint and do this check in Task 8. Here, it's enough that the row has encrypted values and `psql -U cheladb_platform` works with the platform password taken from the container env (`docker exec <c> printenv POSTGRES_PASSWORD`).
  - In that `psql` session, `\du` shows `postgres` with `Create role, Create DB, Bypass RLS` and without `Superuser`, and `cheladb_platform` with `Superuser`.
  - Stop the worker.
- [ ] **Step 6: Commit.** Message: `Worker: two-role provisioning with named volumes, delete jobs, reset RPC, reconcile loop`.

---

### Task 5: Orchestrator endpoints (reveal, reset, delete)

**Files:** Modify `crates/cheladb-routes/src/lib.rs`, `apps/orchestrator/{Cargo.toml,src/lib.rs,src/main.rs,tests/api.rs}`.

**Interfaces (produced):**
- **Routes** (all `Auth::Platform`, `Cors::Studio`, added to `ENDPOINTS`):
  - `orchestrator::DATABASE_CONNECTION`: GET `/v1/databases/{id}/connection`
  - `orchestrator::RESET_PASSWORD`: POST `/v1/databases/{id}/reset-password`
  - `orchestrator::DELETE_DATABASE`: DELETE `/v1/databases/{id}`
- **`JobPort` additions:**
  - `publish_delete(&JobEnvelope<DeleteDatabaseJob>) -> Result<(), BoxErr>`
  - `reset_password(DatabaseId) -> Result<ResetPasswordReply, ResetError>`, where `enum ResetError { Timeout, Transport(String) }`
- **`MetadataPort` addition:** `begin_delete(DatabaseId) -> Result<bool, MetadataError>`.
- **`AppState` gains** `secrets: SecretKey`.
- **Production `JobPort`:** `NatsPorts { js, client }`.
  - `reset_password` uses `tokio::time::timeout(15s, client.request(SUBJECT_RESET_PASSWORD, json))`.
  - It replaces `impl JobPort for JetStreamContext`; `main.rs` builds `NatsPorts`.
- **Connection response:** `ConnectionDetails { host, port, database, user, password, uri }`.

- [ ] **Step 1: Failing tests.** Extend `FakeJobs` with `deleted: Arc<Mutex<Vec<DatabaseId>>>` and a `reset: Arc<Mutex<FakeReset>>`, where `enum FakeReset { Ok, NotReady, Timeout }`. Add `begin_delete` to `FakeMetadata`: it moves a record to `Deleting` unless it is already `Deleting` or `Deleted`, and returns whether it moved. `spawn_app` creates a `SecretKey::generate()`, which `TestApp` exposes as `key`. Tests:
  - `reveal_ready_database_returns_decrypted_owner_credentials`
    - set up with `owner_password_enc = seal(&app.key, &id.to_string(), "s3cret")`, `database_name = "web1"`, `owner_role = "postgres"`, host and port set, status `Ready`
    - expect `200`, `password == "s3cret"`, `user == "postgres"`, `uri == "postgres://postgres:s3cret@127.0.0.1:49153/web1"`, and a `cache-control: no-store` header
  - `reveal_not_ready_is_409`
  - `reveal_without_stored_credentials_is_409_with_recreate_hint` (Review Focus 2): the body mentions `recreate`
  - `reveal_other_tenant_is_403`
  - `reveal_deleted_is_404`
  - `reset_password_ok_is_200`
  - `reset_password_not_ready_is_409` (the row isn't ready, so no RPC is sent)
  - `reset_password_worker_timeout_is_504` (Review Focus 5)
  - `delete_ready_database_is_202_and_publishes_job`
  - `delete_twice_is_409`
  - `delete_deleted_is_404`
  - `delete_other_tenant_is_403`
  - The drift test covers the three new routes automatically.

  Run `cargo test -p orchestrator`: red.

- [ ] **Step 2: Implement.**
  - **Handlers:** each resolves `AuthPrincipal`, loads the row and applies the tenant check. A deleted or missing row gives `404`.
  - **`connection`:**
    - Not `ready` → `409 {"error":"database is <status>; connection details are available once it is ready"}`.
    - Credentials missing (`owner_password_enc`, `database_name`, host or port is `None`, or `owner_password_enc` is empty) → `409 {"error":"credentials unavailable; recreate this database"}`.
    - Otherwise `open(&state.secrets, &id.to_string(), enc)`. Failure → `500` with a log line that contains no password.
    - Success → `200` with the header `Cache-Control: no-store`, logging `info!(request_id, sub, database_id, "revealed connection details")`.
  - **`reset_password`:**
    - Not ready → `409`.
    - `state.js.reset_password(id)` returns:
      - `Ok(reply)` with `reply.ok` → `200 {}`; with `!reply.ok` → `409` or `500` depending on `reply.error` (`not ready` → `409`, else `500`)
      - `Err(Timeout)` → `504 {"error":"worker did not respond; try again"}`
      - `Err(Transport)` → `503`
  - **`delete`:**
    - `begin_delete(id)` returns `false`: if the row is `Deleted` → `404`, else `409 {"error":"database is already being deleted"}`.
    - `true` → publish the delete job. On publish failure, log it, leave the row `deleting` (reconcile retries it) and still return `202`.
    - Response: `202 {"status":"deleting"}`.
  - **`main.rs`:** `SecretKey::from_env().expect("secrets key")`; the `NatsPorts` wiring.
  - **`Cargo.toml`:** add `cheladb-secrets` and `tokio`, already present via axum.
- [ ] **Step 3: Run.** `cargo test --workspace` is green with the drift test passing; clippy is clean.
- [ ] **Step 4: Commit.** Message: `API: reveal connection details, reset password, delete (tombstone)`.

---

### Task 6: Dev workflow wiring

**Files:** Modify `justfile`, `infra/compose/.env.example`.

- [ ] **Step 1: `justfile`.**
  - `worker:` sources `.cheladb/zitadel/demo.env`, like `orchestrator:` does.
  - `sweep-dbs` also removes managed volumes: `docker volume ls -q --filter label=cheladb.managed=true | xargs -r docker volume rm -f`, with a count message.
  - The `dev` summary is unchanged.
- [ ] **Step 2: `.env.example`.** Document `CHELADB_SECRETS_KEY` (comes from `demo.env`) and a commented `# CHELADB_DB_OWNER_SUPERUSER=false` line with a one-line warning.
- [ ] **Step 3: Verify.** `just --list`; then `just sweep-dbs` on the running Compose removes the Task 4 test database's container and volume (`docker volume ls --filter label=cheladb.managed=true` is empty).
- [ ] **Step 4: Commit.** Message: `Dev workflow: worker loads demo.env; sweep-dbs removes data volumes`.

---

### Task 7: Studio actions

**Files:**
- Modify `packages/types/src/index.ts`, `apps/studio/src/lib/api.ts` (+ test), `apps/studio/src/components/dashboard.tsx` (+ test).
- Create `apps/studio/src/components/database-actions.tsx` (+ test).

**Interfaces (produced):**
- Types: `DatabaseStatus` adds `"deleted"`; `ConnectionDetails { host: string; port: number; database: string; user: string; password: string; uri: string }`.
- `ApiClient` adds `getConnection(id): Promise<ConnectionDetails>`, `resetPassword(id): Promise<void>` and `deleteDatabase(id): Promise<void>`. `request()` handles `202` and `204` by skipping JSON parsing when the body is empty.
- `<DatabaseActions api database onChanged />`.
- `Dashboard` re-polls `me()` every `pollIntervalMs` (default 2000) while any database is `pending`, `provisioning` or `deleting`.

- [ ] **Step 1: Failing tests.**
  - **`api.test.ts`:**
    - `getConnection` GETs `/v1/databases/d1/connection`
    - `resetPassword` POSTs `/v1/databases/d1/reset-password`
    - `deleteDatabase` sends DELETE to `/v1/databases/d1` and resolves on `202` with a JSON body
  - **`database-actions.test.tsx`:**
    - "Connection details" reveals host, port, database and user, with the password masked (`••••`) until "Show password" is clicked. The URI and password are shown only after that.
    - Copy buttons call `navigator.clipboard.writeText` (stubbed) with the URI.
    - The button is disabled unless `status === "ready"`.
    - "Reset password" needs a second click on "Confirm reset", then calls `resetPassword` and refetches the connection, showing the new password masked. A `504` error is shown in an alert.
    - "Delete" opens a confirm row with a text field. The "Delete permanently" button stays disabled until the typed text equals the database name. It then calls `deleteDatabase` and `onChanged`.
  - **`dashboard.test.tsx`:** with a `deleting` database the dashboard polls again (`pollIntervalMs={0}`) and stops once the database is gone. Assert `me` was called at least twice and the row disappears.
- [ ] **Step 2: Run.** `pnpm --filter @cheladb/studio test` is red.
- [ ] **Step 3: Implement.**
  - `DatabaseActions` sits in a new "Actions" column of `DatabasesTable`. Keep the markup small and use the existing `Button`.
  - The reveal panel renders under the row as a `<dl>`. The URI is shown only after "Show password", since it contains the password.
  - `navigator.clipboard.writeText` is guarded by try/catch, with the message "Copy failed — select and copy manually".
  - Dashboard polling: `useEffect` on `state`; when transitional statuses exist, schedule `setTimeout(load, pollIntervalMs)` and clean up on change.
- [ ] **Step 4: Run.** `pnpm --filter @cheladb/studio test`, `typecheck` and `build` are all green.
- [ ] **Step 5: Commit.** Message: `Studio: connection details, reset password, delete with type-to-confirm`.

---

### Task 8: End to end, docs

**Files:** Modify `README.md`.

- [ ] **Step 1: README.**
  - A new "Database lifecycle" section: the two roles (and why), the superuser switch and its risk, reveal/reset/delete, tombstones and name reuse, the reconcile loop, and the secrets key.
  - The "Create-database smoke test" uses the reveal endpoint instead of the removed `admin_password` field.
- [ ] **Step 2: Restart the stack.** `just dev-down; just zitadel-bootstrap; just dev` (bootstrap writes `CHELADB_SECRETS_KEY`).
- [ ] **Step 3: End to end with the demo token through `:8000`.**
  1. Create `e2e1` and poll until `ready`.
  2. `GET …/connection` returns `200` with a `postgres://postgres:…@127.0.0.1:<port>/e2e1` URI.
  3. `psql "$URI" -c "select rolsuper from pg_roles where rolname = current_user"` returns `f`.
  4. `psql "$URI" -c "create table t(id int); create extension pgcrypto;"` succeeds.
  5. `POST …/reset-password` returns `200`, then:
     - `psql` with the OLD URI fails authentication
     - a fresh `GET …/connection` gives a URI that works
  6. `DELETE …` returns `202`; poll `GET …` until `404`. Then:
     - `docker ps -a` and `docker volume ls` show nothing for that id
     - the row has `status = 'deleted'`
  7. Create `e2e1` again: `202` (the name is reusable).
  8. Delete it, then delete it again: the second answer is `404`.
  9. **Reconcile:** `docker volume create --label cheladb.managed=true --label cheladb.database_id=<random-uuid> cheladb-db-<random-uuid>`. Within about 35 s it's gone, and the worker log says `RemoveOrphan`.
  10. **Review Focus 1:** create `race1` and immediately `DELETE`. It ends `deleted` with no container or volume left.
  11. **Review Focus 5:** stop the worker only (`kill` its process group), then `POST …/reset-password` on a ready database returns `504` in about 15 s. Restart with `just dev-down && just dev`.
- [ ] **Step 4: Full checks.** `cargo test --workspace`, `just check`, `cargo clippy --workspace --all-targets` (no warnings).
- [ ] **Step 5: Commit.** Message: `Docs: database lifecycle; e2e verified`.
- [ ] **Step 6: Hand the browser check to the user:** Studio → Connection details → Show password → copy the URI into `psql`, then Reset password, then Delete (type the name).
