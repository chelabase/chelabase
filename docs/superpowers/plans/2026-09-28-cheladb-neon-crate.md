# `cheladb-neon` crate Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Move the workspace to edition 2024, then add `crates/cheladb-neon`: Neon ids, LSNs, SCRAM verifiers, the compute spec, the compute JWT signer and a tested storage-controller client, ported from the old repo.

**Architecture:** One library crate, one module per concern, no callers yet. The pure modules come over from `<repo root>/services/controlplane/src` almost verbatim. The storage-controller client gains typed ids, `describe_tenant`, timeouts and an injectable delete poll. It is tested against an in-process axum mock and, opt-in, against the real `just neon-up` stack.

**Tech Stack:** Rust (edition 2024, rustc 1.98.1), reqwest 0.13, jsonwebtoken 11, RustCrypto sha2 0.11 / hmac 0.13 / pbkdf2 0.13, getrandom 0.4, base64 0.23; tests use axum 0.8, tokio and ed25519-dalek 3.

**Spec:** `docs/superpowers/specs/2026-09-28-cheladb-neon-crate-design.md`

## Global Constraints

- Branch `neon-crate`. No push, no merge until the user says.
- Commit as `git commit`, message ending with a blank line and:
  ```
  Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
  Claude-Session: https://claude.ai/code/session_01KESSEqEbiuKm7pMwCPe2Wg
  ```
- **Never run `cargo fmt`** (workspace-wide or per package). Format only the new files, one by one, with `rustfmt --edition 2024 <file>`.
- Never run `just sweep-dbs`, touch demo1/demo2/demo3, `.cheladb/secrets/secrets.key` or the gateway key pair.
- `cheladb-neon` dependency versions are declared in its own `Cargo.toml`, at exactly these versions: reqwest `0.13.5` (default off; `json`, `rustls`), jsonwebtoken `11.1.0` (`use_pem`, `rust_crypto`), base64 `0.23.1`, sha2 `0.11.0`, hmac `0.13.0`, pbkdf2 `0.13.0`, getrandom `0.4.3`, hex `0.4.3`, uuid `1.26.1` (`v4`), serde `1.0.229` (`derive`), serde_json `1.0.151`, thiserror `2.0.21`, anyhow `1.0.104`, tokio `1.53.1` (`time`). Dev: tokio `1.53.1` (`macros`, `rt-multi-thread`, `net`), axum `0.8.9`, ed25519-dalek `3.0.0` (`pem`).
- The workspace pins (`reqwest 0.12`, `jsonwebtoken 9`, `base64 0.22`, …) stay unchanged.
- No HTTP routes, no Docker backend, no DB schema in this plan.
- Storage-controller defaults: request timeout 30 s; delete poll 120 attempts × 500 ms.

## Review Focus

1. **Storage controller not running** (connection refused): every call returns `StorconError::Other`, never panics. Test in Task 6 (`unreachable_server_is_other`).
2. **Base URL with a trailing slash** (`http://127.0.0.1:1234/`): requests go to `/v1/...`, not `//v1/...`. Test in Task 6 (`trailing_slash_in_base_is_ignored`).
3. **A 2xx answer whose body doesn't parse** (bad LSN, missing field): `Other`, not a panic or a default value. Test in Task 6 (`malformed_success_body_is_other`).
4. **A storage controller that accepts but never answers**: the call fails after the timeout instead of hanging. Test in Task 6 (`hanging_server_times_out`) using `with_timeout`.
5. **`last_error` sent as `null`, and a root timeline's `ancestor_lsn` as `null`**: both parse (`""` and `None`). Tests in Task 6 (`describe_tenant_parses_shards`, `create_timeline_fresh_and_branch`).

---

## File structure

| File | Responsibility |
|---|---|
| `Cargo.toml` (workspace) | edition 2024, resolver 3, new member |
| `crates/cheladb-neon/Cargo.toml` | crate manifest with its own dependency versions |
| `crates/cheladb-neon/src/lib.rs` | module list and re-exports |
| `crates/cheladb-neon/src/id.rs` | `NeonId` |
| `crates/cheladb-neon/src/lsn.rs` | `Lsn` |
| `crates/cheladb-neon/src/scram.rs` | SCRAM-SHA-256 verifiers |
| `crates/cheladb-neon/src/jwt.rs` | `ComputeSigner` (Ed25519 tokens + JWKS for compute_ctl) |
| `crates/cheladb-neon/src/spec.rs` | compute spec JSON for compute_ctl |
| `crates/cheladb-neon/src/storcon.rs` | `StorconClient`, `StorconError`, response types |
| `crates/cheladb-neon/tests/storcon_mock.rs` | client tests against an in-process mock |
| `crates/cheladb-neon/tests/storcon_live.rs` | `#[ignore]`d round trip against `just neon-up` |
| `justfile` | `neon-test-live` recipe |
| `docs/roadmap/2026-09-28-neon-and-data-api.md` | step 2 done, findings, dependency follow-up |

---

### Task 1: Workspace on edition 2024

**Files:**
- Modify: `Cargo.toml:2` (resolver) and `Cargo.toml:18` (edition)
- Modify: whatever `cargo fix --edition` rewrites under `apps/` and `crates/`

**Interfaces:**
- Consumes: nothing.
- Produces: `[workspace.package] edition = "2024"`, which every later crate inherits with `edition.workspace = true`.

- [ ] **Step 1: Record the baseline**

The tree must be clean (`git status --short` prints nothing). Then:

```bash
S=<scratch dir>
cargo test --workspace 2>&1 | grep -E '^test result|FAILED|panicked' > $S/baseline-tests.txt; cat $S/baseline-tests.txt
cargo clippy --workspace --all-targets 2>&1 | grep -cE '^(warning|error)' > $S/baseline-clippy.txt; cat $S/baseline-clippy.txt
```

Expected: a list of `test result: ok.` lines (live tests show as ignored) and a warning count. Keep both files; Step 5 compares against them.

- [ ] **Step 2: Let cargo migrate the code, still on 2021**

`cargo fix --edition` must run while the manifest still says 2021.

```bash
cargo fix --edition --workspace --all-targets
git diff --stat
```

Expected: exit 0. The diff is empty or small: `use<..>` bounds on `impl Trait` returns, or `if let` rewritten to `match` for temporary scope. Read the whole diff. If `cargo fix` reports it couldn't fix something, stop and fix it by hand the way the compiler message says.

- [ ] **Step 3: Switch the edition and resolver**

In `Cargo.toml`, change `resolver = "2"` to `resolver = "3"` and, under `[workspace.package]`, `edition = "2021"` to `edition = "2024"`. (Resolver 3 is the 2024 default; with no `rust-version` set anywhere it changes no resolution.)

- [ ] **Step 4: Build and lint**

```bash
cargo check --workspace --all-targets
cargo clippy --workspace --all-targets 2>&1 | grep -cE '^(warning|error)'
```

Expected: check passes; the clippy count is no higher than `baseline-clippy.txt`. New edition-2024 lints (for example `tail_expr_drop_order`, `if_let_rescope`, `impl_trait_overcaptures`) that appear only now get fixed in code, not allowed.

- [ ] **Step 5: Tests as before**

```bash
cargo test --workspace 2>&1 | grep -E '^test result|FAILED|panicked' > $S/after-tests.txt
diff $S/baseline-tests.txt $S/after-tests.txt
```

Expected: the only differences are timings (`finished in …`). Same pass, fail and ignored counts.

- [ ] **Step 6: Confirm no formatting churn**

```bash
git diff --stat
```

Expected: `Cargo.toml` plus only the files Step 2 changed. No file shows whitespace-only or import-reordering changes (nothing ran rustfmt).

- [ ] **Step 7: Commit**

```bash
git add -A Cargo.toml Cargo.lock apps crates
git commit -F- <<'EOF'
Workspace: move to edition 2024 and resolver 3

cargo fix --edition first, then the switch; no rustfmt run.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01KESSEqEbiuKm7pMwCPe2Wg
EOF
```

---

### Task 2: Crate skeleton, `NeonId` and `Lsn`

**Files:**
- Modify: `Cargo.toml` (workspace members, workspace dependency entry)
- Create: `crates/cheladb-neon/Cargo.toml`
- Create: `crates/cheladb-neon/src/lib.rs`
- Create: `crates/cheladb-neon/src/id.rs`
- Create: `crates/cheladb-neon/src/lsn.rs`

**Interfaces:**
- Consumes: edition 2024 from Task 1.
- Produces:
  - `cheladb_neon::NeonId`: `NeonId::new() -> NeonId`, `as_str(&self) -> &str`, `From<uuid::Uuid>`, `FromStr<Err = InvalidNeonId>`, `Display`, `Serialize`/`Deserialize` (string, validated), `Clone + Debug + PartialEq + Eq + Hash + Default`.
  - `cheladb_neon::Lsn(pub u64)`: `FromStr<Err = String>`, `Display` (`0/1F98488`, uppercase hex), `Ord`, `Copy`, `Serialize`/`Deserialize` as the text form.

- [ ] **Step 1: Register the crate**

In the workspace `Cargo.toml`, add `"crates/cheladb-neon",` to `members` after `"crates/cheladb-secrets",`, and under `[workspace.dependencies]` add after the `cheladb-secrets` line:

```toml
cheladb-neon = { path = "crates/cheladb-neon" }
```

Create `crates/cheladb-neon/Cargo.toml`:

```toml
[package]
name = "cheladb-neon"
version.workspace = true
edition.workspace = true
license.workspace = true
description = "Neon storage-controller client, compute spec, SCRAM verifiers and compute JWTs for Cheladb"

# Latest releases as of 2026-09-28, declared here rather than as workspace pins: other
# crates still use reqwest 0.12, jsonwebtoken 9 and base64 0.22.
[dependencies]
serde = { version = "1.0.229", features = ["derive"] }
thiserror = "2.0.21"
uuid = { version = "1.26.1", features = ["v4"] }

[dev-dependencies]
serde_json = "1.0.151"
```

Create `crates/cheladb-neon/src/lib.rs`:

```rust
//! Neon-facing building blocks for the Cheladb control plane: the storage-controller
//! client, the compute spec compute_ctl pulls, SCRAM verifiers, LSNs, compute JWTs and ids.

pub mod id;
pub mod lsn;

pub use id::NeonId;
pub use lsn::Lsn;
```

- [ ] **Step 2: Write the failing tests**

Create `crates/cheladb-neon/src/id.rs` with only the tests for now:

```rust
//! Neon tenant and timeline ids: 32 lowercase hex characters, no dashes.

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[test]
    fn random_ids_are_32_lowercase_hex() {
        let (a, b) = (NeonId::new(), NeonId::new());
        assert_ne!(a, b);
        assert_eq!(a.as_str().len(), 32);
        assert!(a.as_str().parse::<NeonId>().is_ok());
    }

    #[test]
    fn a_uuid_maps_to_its_simple_form() {
        let u: Uuid = "67e55044-10b1-426f-9247-bb680e5fe0c8".parse().unwrap();
        assert_eq!(NeonId::from(u).as_str(), "67e5504410b1426f9247bb680e5fe0c8");
        assert_eq!(NeonId::from(u), NeonId::from(u));
        assert_eq!(NeonId::from(u).to_string(), "67e5504410b1426f9247bb680e5fe0c8");
    }

    #[test]
    fn rejects_malformed() {
        for bad in [
            "",
            "67e5504410b1426f9247bb680e5fe0c",
            "67e5504410b1426f9247bb680e5fe0c8a",
            "67E5504410B1426F9247BB680E5FE0C8",
            "67e55044-10b1-426f-9247-bb680e5fe0c8",
            "g7e5504410b1426f9247bb680e5fe0c8",
        ] {
            assert!(bad.parse::<NeonId>().is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn serde_round_trips_and_validates() {
        let id = NeonId::from(Uuid::nil());
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, "\"00000000000000000000000000000000\"");
        assert_eq!(serde_json::from_str::<NeonId>(&json).unwrap(), id);
        assert!(serde_json::from_str::<NeonId>("\"ABC\"").is_err());
    }
}
```

Create `crates/cheladb-neon/src/lsn.rs` with only the tests for now (the first two are the old repo's, unchanged):

```rust
//! Postgres log sequence numbers in their text form, e.g. `0/1F98488`.

#[cfg(test)]
mod tests {
    use super::Lsn;

    #[test]
    fn parses_and_orders() {
        let a: Lsn = "0/1F98488".parse().unwrap();
        let b: Lsn = "1/0".parse().unwrap();
        assert_eq!(a.0, 0x1F98488);
        assert_eq!(b.0, 1 << 32);
        assert!(a < b);
        assert_eq!(a.to_string(), "0/1F98488");
        assert_eq!(
            "ff/ffffffff".parse::<Lsn>().unwrap().to_string(),
            "FF/FFFFFFFF"
        );
    }

    #[test]
    fn rejects_malformed() {
        for bad in ["", "0", "/1", "0/", "x/1", "0/1/2", "123456789/0", "-1/0"] {
            assert!(bad.parse::<Lsn>().is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn serde_uses_the_text_form() {
        let lsn: Lsn = serde_json::from_str("\"0/14EE0B8\"").unwrap();
        assert_eq!(lsn.0, 0x14EE0B8);
        assert_eq!(serde_json::to_string(&lsn).unwrap(), "\"0/14EE0B8\"");
        assert_eq!(serde_json::from_str::<Option<Lsn>>("null").unwrap(), None);
        assert!(serde_json::from_str::<Lsn>("\"nope\"").is_err());
    }
}
```

- [ ] **Step 3: Run the tests to see them fail**

Run: `cargo test -p cheladb-neon`
Expected: compile errors: `cannot find type NeonId` and `unresolved import super::Lsn`.

- [ ] **Step 4: Implement `NeonId`**

In `crates/cheladb-neon/src/id.rs`, between the module doc comment and `#[cfg(test)]`, insert:

```rust
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use uuid::Uuid;

/// Always 32 lowercase hex characters: the form the storage controller accepts.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct NeonId(String);

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid Neon id {0:?}; expected 32 lowercase hex characters")]
pub struct InvalidNeonId(String);

impl NeonId {
    /// A random id.
    pub fn new() -> Self {
        Self::from(Uuid::new_v4())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A random id, like [`NeonId::new`].
impl Default for NeonId {
    fn default() -> Self {
        Self::new()
    }
}

/// The uuid's simple form, so one of our ids always maps to the same Neon id.
impl From<Uuid> for NeonId {
    fn from(id: Uuid) -> Self {
        Self(id.simple().to_string())
    }
}

impl FromStr for NeonId {
    type Err = InvalidNeonId;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.len() == 32 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
            Ok(Self(s.to_string()))
        } else {
            Err(InvalidNeonId(s.to_string()))
        }
    }
}

impl fmt::Display for NeonId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for NeonId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for NeonId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}
```

In the tests module, `use uuid::Uuid;` is now also imported by `super::*`; remove the explicit `use uuid::Uuid;` line from the tests if the compiler warns about a redundant import.

- [ ] **Step 5: Implement `Lsn`**

Port `<repo root>/services/controlplane/src/lsn.rs` and add serde. In `crates/cheladb-neon/src/lsn.rs`, between the doc comment and `#[cfg(test)]`, insert:

```rust
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Lsn(pub u64);

impl FromStr for Lsn {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let invalid = || format!("invalid LSN {s:?}; expected the form 0/1F98488");
        let (hi, lo) = s.split_once('/').ok_or_else(invalid)?;
        let part = |p: &str| {
            if p.is_empty() || p.len() > 8 {
                return Err(invalid());
            }
            u32::from_str_radix(p, 16).map_err(|_| invalid())
        };
        Ok(Self((u64::from(part(hi)?) << 32) | u64::from(part(lo)?)))
    }
}

impl fmt::Display for Lsn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:X}/{:X}", self.0 >> 32, self.0 & 0xffff_ffff)
    }
}

/// As the text form, the way Neon's APIs send and accept LSNs.
impl Serialize for Lsn {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Lsn {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}
```

- [ ] **Step 6: Run the tests to see them pass**

Run: `cargo test -p cheladb-neon`
Expected: 7 passed.

- [ ] **Step 7: Format, lint, commit**

```bash
for f in crates/cheladb-neon/src/*.rs; do rustfmt --edition 2024 "$f"; done
cargo clippy -p cheladb-neon --all-targets -- -D warnings
git add Cargo.toml Cargo.lock crates/cheladb-neon
git commit -F- <<'EOF'
cheladb-neon: crate with NeonId and Lsn

NeonId maps our uuids to Neon's 32-hex form. Lsn is the old repo's, plus serde as text.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01KESSEqEbiuKm7pMwCPe2Wg
EOF
```

Expected: clippy clean.

---

### Task 3: `scram`

**Files:**
- Modify: `crates/cheladb-neon/Cargo.toml`, `crates/cheladb-neon/src/lib.rs`
- Create: `crates/cheladb-neon/src/scram.rs`

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces: `cheladb_neon::scram::verifier(password: &str) -> String` and `cheladb_neon::scram::verifier_with_salt(password: &str, salt: &[u8], iterations: u32) -> String`, output `SCRAM-SHA-256$<iterations>:<salt b64>$<StoredKey b64>:<ServerKey b64>`.

- [ ] **Step 1: Dependencies and module**

In `crates/cheladb-neon/Cargo.toml` `[dependencies]`, add (keep alphabetical order):

```toml
base64 = "0.23.1"
getrandom = "0.4.3"
hmac = "0.13.0"
pbkdf2 = "0.13.0"
sha2 = "0.11.0"
```

In `src/lib.rs`, add `pub mod scram;` after `pub mod lsn;`.

- [ ] **Step 2: Write the failing tests**

Create `crates/cheladb-neon/src/scram.rs`:

```rust
//! SCRAM-SHA-256 password verifiers in the format Postgres stores (RFC 5802 / RFC 7677).

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;

    /// Test vector from RFC 7677 section 3 (user "user", password "pencil").
    #[test]
    fn matches_rfc7677_vector() {
        let salt = STANDARD.decode("W22ZaJ0SNY7soEsUEjb6gQ==").unwrap();
        let v = verifier_with_salt("pencil", &salt, 4096);
        let (_, keys) = v.rsplit_once('$').unwrap();
        let (stored, server) = keys.split_once(':').unwrap();
        // ServerKey from the RFC's server signature derivation; StoredKey = H(ClientKey).
        assert_eq!(server, "wfPLwcE6nTWhTAmQ7tl2KeoiWGPlZqQxSrmfPwDl2dU=");
        assert_eq!(stored, "WG5d8oPm3OtcPnkdi4Uo7BkeZkBFzpcXkuLmtbsT4qY=");
        assert!(v.starts_with("SCRAM-SHA-256$4096:W22ZaJ0SNY7soEsUEjb6gQ==$"));
    }

    #[test]
    fn each_verifier_gets_a_fresh_16_byte_salt() {
        let (a, b) = (verifier("pw"), verifier("pw"));
        assert_ne!(a, b);
        assert!(a.starts_with("SCRAM-SHA-256$4096:"));
        let salt = a.split('$').nth(1).unwrap().split_once(':').unwrap().1;
        assert_eq!(STANDARD.decode(salt).unwrap().len(), 16);
    }
}
```

- [ ] **Step 3: Run the tests to see them fail**

Run: `cargo test -p cheladb-neon scram`
Expected: compile errors: `cannot find function verifier_with_salt` / `verifier`.

- [ ] **Step 4: Implement**

Port the old `scram.rs` without `generate_password`, making `verifier_with_salt` public. Insert between the doc comment and `#[cfg(test)]`:

```rust
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use hmac::{Hmac, KeyInit, Mac};
use sha2::{Digest, Sha256};

const ITERATIONS: u32 = 4096;

/// `SCRAM-SHA-256$<iterations>:<salt>$<StoredKey>:<ServerKey>`, with a random salt.
pub fn verifier(password: &str) -> String {
    let mut salt = [0u8; 16];
    getrandom::fill(&mut salt).expect("OS random number generator");
    verifier_with_salt(password, &salt, ITERATIONS)
}

pub fn verifier_with_salt(password: &str, salt: &[u8], iterations: u32) -> String {
    let mut salted = [0u8; 32];
    pbkdf2::pbkdf2_hmac::<Sha256>(password.as_bytes(), salt, iterations, &mut salted);
    let client_key = hmac(&salted, b"Client Key");
    let server_key = hmac(&salted, b"Server Key");
    let stored_key = Sha256::digest(client_key);
    format!(
        "SCRAM-SHA-256${iterations}:{}${}:{}",
        STANDARD.encode(salt),
        STANDARD.encode(stored_key),
        STANDARD.encode(server_key)
    )
}

fn hmac(key: &[u8], msg: &[u8]) -> Vec<u8> {
    let mut mac =
        <Hmac<Sha256> as KeyInit>::new_from_slice(key).expect("HMAC accepts any key size");
    mac.update(msg);
    mac.finalize().into_bytes().to_vec()
}
```

The tests module's own `use base64::Engine; use ...STANDARD;` now duplicate `super::*`; delete those two lines from the tests if the compiler flags them.

- [ ] **Step 5: Run the tests to see them pass**

Run: `cargo test -p cheladb-neon scram`
Expected: 2 passed.

- [ ] **Step 6: Format, lint, commit**

```bash
rustfmt --edition 2024 crates/cheladb-neon/src/scram.rs crates/cheladb-neon/src/lib.rs
cargo clippy -p cheladb-neon --all-targets -- -D warnings
git add Cargo.lock crates/cheladb-neon
git commit -F- <<'EOF'
cheladb-neon: SCRAM-SHA-256 verifiers

Ported from the old control plane, with its RFC 7677 test vector.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01KESSEqEbiuKm7pMwCPe2Wg
EOF
```

---

### Task 4: `jwt` (compute_ctl signer and JWKS)

**Files:**
- Modify: `crates/cheladb-neon/Cargo.toml`, `crates/cheladb-neon/src/lib.rs`
- Create: `crates/cheladb-neon/src/jwt.rs`

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces: `cheladb_neon::ComputeSigner` with `new(private_pem: &str, public_x: &str) -> anyhow::Result<ComputeSigner>`, `jwks(&self) -> serde_json::Value`, `token_for(&self, compute_id: &str) -> anyhow::Result<String>`.

- [ ] **Step 1: Dependencies and module**

`crates/cheladb-neon/Cargo.toml` `[dependencies]`, add:

```toml
anyhow = "1.0.104"
hex = "0.4.3"
jsonwebtoken = { version = "11.1.0", features = ["use_pem", "rust_crypto"] }
serde_json = "1.0.151"
```

Remove `serde_json` from `[dev-dependencies]` (it is a normal dependency now), and add to `[dev-dependencies]`:

```toml
ed25519-dalek = { version = "3.0.0", features = ["pem"] }
```

`src/lib.rs`: add `pub mod jwt;` (keep the module list alphabetical: `id`, `jwt`, `lsn`, `scram`) and `pub use jwt::ComputeSigner;`.

- [ ] **Step 2: Write the failing tests**

Create `crates/cheladb-neon/src/jwt.rs`:

```rust
//! Tokens the control plane presents to compute_ctl (e.g. on `POST /configure`).
//!
//! compute_ctl verifies EdDSA tokens against the JWKS in its `compute_ctl_config`
//! (neon `compute_tools/src/http/middleware/authorize.rs`): `exp` is checked, and a
//! token without the admin scope must carry the compute's own `compute_id`.

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use ed25519_dalek::SigningKey;
    use ed25519_dalek::pkcs8::EncodePrivateKey;
    use ed25519_dalek::pkcs8::spki::der::pem::LineEnding;

    /// A throwaway Ed25519 key pair: PKCS#8 PEM and the base64url public key.
    fn test_keypair() -> (String, String) {
        let mut seed = [0u8; 32];
        getrandom::fill(&mut seed).expect("OS random number generator");
        let key = SigningKey::from_bytes(&seed);
        let pem = key.to_pkcs8_pem(LineEnding::LF).expect("PKCS#8 PEM").to_string();
        let x = URL_SAFE_NO_PAD.encode(key.verifying_key().to_bytes());
        (pem, x)
    }

    #[test]
    fn signs_tokens_verifiable_with_the_published_jwk() {
        let (pem, x) = test_keypair();
        let signer = ComputeSigner::new(&pem, &x).unwrap();
        let token = signer.token_for("ep-abc").unwrap();
        // Verify the way compute_ctl does: a DecodingKey built from the JWK itself.
        let jwks: jsonwebtoken::jwk::JwkSet = serde_json::from_value(signer.jwks()).unwrap();
        let key = DecodingKey::from_jwk(&jwks.keys[0]).unwrap();
        let data =
            jsonwebtoken::decode::<Claims>(&token, &key, &Validation::new(Algorithm::EdDSA))
                .unwrap();
        assert_eq!(data.claims.compute_id, "ep-abc");
        assert_eq!(
            jsonwebtoken::decode_header(&token).unwrap().kid.as_deref(),
            jwks.keys[0].common.key_id.as_deref()
        );
    }

    #[test]
    fn rejects_a_mismatched_public_key() {
        let (pem, _) = test_keypair();
        let (_, other_x) = test_keypair();
        assert!(ComputeSigner::new(&pem, &other_x).is_err());
    }

    #[test]
    fn rejects_a_malformed_private_key() {
        let (_, x) = test_keypair();
        assert!(ComputeSigner::new("not a pem", &x).is_err());
    }
}
```

- [ ] **Step 3: Run the tests to see them fail**

Run: `cargo test -p cheladb-neon jwt`
Expected: compile errors: `cannot find type ComputeSigner`, `Claims`, `DecodingKey`.

- [ ] **Step 4: Implement**

Port `<repo root>/services/controlplane/src/jwt.rs` unchanged apart from the tests (it compiles as-is against jsonwebtoken 11.1.0 / sha2 0.11 / hex 0.4.3; checked in a scratch crate). Insert between the doc comment and `#[cfg(test)]`:

```rust
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Context;
use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const TOKEN_TTL_SECS: u64 = 300;

#[derive(Debug, Serialize, Deserialize)]
struct Claims {
    compute_id: String,
    exp: u64,
}

pub struct ComputeSigner {
    key: EncodingKey,
    /// Base64url (no padding) Ed25519 public key: the JWK `x` parameter.
    public_x: String,
    kid: String,
}

impl ComputeSigner {
    /// `private_pem`: PKCS#8 Ed25519 key. `public_x`: its public key, base64url.
    /// Fails if the two don't belong together.
    pub fn new(private_pem: &str, public_x: &str) -> anyhow::Result<Self> {
        let key = EncodingKey::from_ed_pem(private_pem.as_bytes())
            .context("parsing Ed25519 private key PEM")?;
        let kid = hex::encode(&Sha256::digest(public_x.as_bytes())[..8]);
        let signer = Self {
            key,
            public_x: public_x.trim().to_string(),
            kid,
        };
        let token = signer.token_for("self-test")?;
        signer
            .verify(&token)
            .context("JWT private key does not match the configured public key")?;
        Ok(signer)
    }

    /// The JWKS to embed in `compute_ctl_config.jwks`.
    pub fn jwks(&self) -> Value {
        json!({ "keys": [{
            "kty": "OKP",
            "crv": "Ed25519",
            "x": self.public_x,
            "kid": self.kid,
            "alg": "EdDSA",
            "use": "sig",
        }]})
    }

    /// A short-lived token only the given compute accepts.
    pub fn token_for(&self, compute_id: &str) -> anyhow::Result<String> {
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
        let mut header = Header::new(Algorithm::EdDSA);
        header.kid = Some(self.kid.clone());
        let claims = Claims {
            compute_id: compute_id.to_string(),
            exp: now + TOKEN_TTL_SECS,
        };
        Ok(jsonwebtoken::encode(&header, &claims, &self.key)?)
    }

    fn verify(&self, token: &str) -> anyhow::Result<String> {
        let key = DecodingKey::from_ed_components(&self.public_x)?;
        let data = jsonwebtoken::decode::<Claims>(token, &key, &Validation::new(Algorithm::EdDSA))?;
        Ok(data.claims.compute_id)
    }
}
```

- [ ] **Step 5: Run the tests to see them pass**

Run: `cargo test -p cheladb-neon jwt`
Expected: 3 passed.

- [ ] **Step 6: Format, lint, commit**

```bash
rustfmt --edition 2024 crates/cheladb-neon/src/jwt.rs crates/cheladb-neon/src/lib.rs
cargo clippy -p cheladb-neon --all-targets -- -D warnings
git add Cargo.lock crates/cheladb-neon
git commit -F- <<'EOF'
cheladb-neon: compute_ctl token signer and JWKS

Ported from the old control plane; tests make keys with ed25519-dalek, not openssl.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01KESSEqEbiuKm7pMwCPe2Wg
EOF
```

---

### Task 5: `spec` (compute spec for compute_ctl)

**Files:**
- Modify: `crates/cheladb-neon/src/lib.rs`
- Create: `crates/cheladb-neon/src/spec.rs`

**Interfaces:**
- Consumes: `NeonId` (Task 2).
- Produces: in `cheladb_neon::spec`:
  - `pub struct RoleSpec { pub name: String, pub scram_verifier: String }`
  - `pub struct DatabaseSpec { pub name: String, pub owner: String }`
  - `pub struct SpecInputs<'a> { pub endpoint_id: &'a str, pub tenant_id: &'a NeonId, pub timeline_id: &'a NeonId, pub pageserver_connstring: String, pub safekeeper_connstrings: Vec<String>, pub roles: Vec<RoleSpec>, pub databases: Vec<DatabaseSpec>, pub jwks: serde_json::Value }`
  - `pub fn config_response(inputs: &SpecInputs<'_>) -> serde_json::Value`

- [ ] **Step 1: Module**

`src/lib.rs`: add `pub mod spec;` after `pub mod scram;`.

- [ ] **Step 2: Write the failing test**

Create `crates/cheladb-neon/src/spec.rs`:

```rust
//! The compute spec served to compute_ctl (`GET /compute/api/v2/computes/{id}/spec`).
//!
//! Field names follow neon's `compute_api::spec::ComputeSpec` and
//! `compute_api::responses::ControlPlaneConfigResponse`.

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[test]
    fn builds_an_attached_spec() {
        let tenant = NeonId::from(Uuid::from_u128(1));
        let timeline = NeonId::from(Uuid::from_u128(2));
        let v = config_response(&SpecInputs {
            endpoint_id: "ep-a",
            tenant_id: &tenant,
            timeline_id: &timeline,
            pageserver_connstring: "postgresql://no_user@ps:6400".into(),
            safekeeper_connstrings: vec!["sk-0:5454".into(), "sk-1:5454".into()],
            roles: vec![RoleSpec {
                name: "app".into(),
                scram_verifier: "SCRAM-SHA-256$x".into(),
            }],
            databases: vec![DatabaseSpec {
                name: "app".into(),
                owner: "app".into(),
            }],
            jwks: json!({ "keys": [{ "kid": "k" }] }),
        });
        assert_eq!(v["status"], "attached");
        assert_eq!(v["spec"]["tenant_id"], "00000000000000000000000000000001");
        assert_eq!(v["spec"]["timeline_id"], "00000000000000000000000000000002");
        assert_eq!(v["spec"]["pageserver_connstring"], "postgresql://no_user@ps:6400");
        assert_eq!(v["spec"]["safekeeper_connstrings"][1], "sk-1:5454");
        assert_eq!(v["spec"]["cluster"]["cluster_id"], "ep-a");
        assert_eq!(
            v["spec"]["cluster"]["roles"][0]["encrypted_password"],
            "SCRAM-SHA-256$x"
        );
        assert_eq!(v["spec"]["cluster"]["databases"][0]["owner"], "app");
        assert_eq!(v["compute_ctl_config"]["jwks"]["keys"][0]["kid"], "k");
        let settings = v["spec"]["cluster"]["settings"].as_array().unwrap();
        let get = |name: &str| {
            settings
                .iter()
                .find(|s| s["name"] == name)
                .map(|s| s["value"].as_str().unwrap().to_string())
        };
        assert_eq!(get("shared_preload_libraries").as_deref(), Some("neon"));
        assert_eq!(get("synchronous_standby_names").as_deref(), Some("walproposer"));
        assert_eq!(get("password_encryption").as_deref(), Some("scram-sha-256"));
    }
}
```

- [ ] **Step 3: Run the test to see it fail**

Run: `cargo test -p cheladb-neon spec`
Expected: compile errors: `cannot find function config_response`, `SpecInputs`, `json`.

- [ ] **Step 4: Implement**

Port the old `compute.rs` with typed Neon ids. Insert between the doc comment and `#[cfg(test)]`:

```rust
use serde_json::{Value, json};

use crate::id::NeonId;

pub struct RoleSpec {
    pub name: String,
    pub scram_verifier: String,
}

pub struct DatabaseSpec {
    pub name: String,
    pub owner: String,
}

pub struct SpecInputs<'a> {
    /// Our endpoint id; compute_ctl uses it as the cluster id and name.
    pub endpoint_id: &'a str,
    pub tenant_id: &'a NeonId,
    pub timeline_id: &'a NeonId,
    /// `postgresql://no_user@host:port` of the attached pageserver.
    pub pageserver_connstring: String,
    /// `host:port` of each safekeeper.
    pub safekeeper_connstrings: Vec<String>,
    pub roles: Vec<RoleSpec>,
    pub databases: Vec<DatabaseSpec>,
    /// Keys compute_ctl accepts control-plane tokens from.
    pub jwks: Value,
}

fn setting(name: &str, value: &str, vartype: &str) -> Value {
    json!({ "name": name, "value": value, "vartype": vartype })
}

/// `{spec, status, compute_ctl_config}` for an attached compute.
pub fn config_response(inputs: &SpecInputs<'_>) -> Value {
    let roles: Vec<Value> = inputs
        .roles
        .iter()
        .map(|r| json!({ "name": r.name, "encrypted_password": r.scram_verifier, "options": null }))
        .collect();
    let databases: Vec<Value> = inputs
        .databases
        .iter()
        .map(|d| json!({ "name": d.name, "owner": d.owner, "options": null }))
        .collect();
    json!({
        "status": "attached",
        "spec": {
            "format_version": 1.0,
            "suspend_timeout_seconds": -1,
            "tenant_id": inputs.tenant_id,
            "timeline_id": inputs.timeline_id,
            "pageserver_connstring": inputs.pageserver_connstring,
            "safekeeper_connstrings": inputs.safekeeper_connstrings,
            "cluster": {
                "cluster_id": inputs.endpoint_id,
                "name": inputs.endpoint_id,
                "state": "restarted",
                "roles": roles,
                "databases": databases,
                "settings": [
                    setting("port", "5432", "integer"),
                    setting("listen_addresses", "0.0.0.0", "string"),
                    setting("shared_preload_libraries", "neon", "string"),
                    setting("wal_level", "logical", "enum"),
                    setting("wal_log_hints", "on", "bool"),
                    setting("max_connections", "100", "integer"),
                    setting("max_wal_senders", "10", "integer"),
                    setting("max_replication_slots", "10", "integer"),
                    setting("wal_keep_size", "0", "integer"),
                    setting("restart_after_crash", "off", "bool"),
                    setting("synchronous_standby_names", "walproposer", "string"),
                    setting("password_encryption", "scram-sha-256", "enum"),
                ],
            },
            "delta_operations": [],
        },
        "compute_ctl_config": { "jwks": inputs.jwks },
    })
}
```

- [ ] **Step 5: Run the test to see it pass**

Run: `cargo test -p cheladb-neon spec`
Expected: 1 passed.

- [ ] **Step 6: Format, lint, commit**

```bash
rustfmt --edition 2024 crates/cheladb-neon/src/spec.rs crates/cheladb-neon/src/lib.rs
cargo clippy -p cheladb-neon --all-targets -- -D warnings
git add crates/cheladb-neon
git commit -F- <<'EOF'
cheladb-neon: compute spec for compute_ctl

Ported from the old control plane; tenant and timeline ids are NeonIds.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01KESSEqEbiuKm7pMwCPe2Wg
EOF
```

---

### Task 6: `storcon` client with mock tests

**Files:**
- Modify: `crates/cheladb-neon/Cargo.toml`, `crates/cheladb-neon/src/lib.rs`
- Create: `crates/cheladb-neon/src/storcon.rs`
- Test: `crates/cheladb-neon/tests/storcon_mock.rs`

**Interfaces:**
- Consumes: `NeonId` (`as_str`, `Serialize`), `Lsn` (`Serialize`/`Deserialize`) from Task 2.
- Produces, in `cheladb_neon::storcon` (with `StorconClient` and `StorconError` re-exported at the crate root):
  ```rust
  pub enum StorconError { Rejected { status: reqwest::StatusCode, message: String }, Other(anyhow::Error) }
  pub struct TimelineInfo { pub ancestor_lsn: Option<Lsn>, pub last_record_lsn: Lsn }
  pub struct TenantDescribe { pub shards: Vec<ShardStatus> }
  pub struct ShardStatus { pub is_pending_compute_notification: bool, pub is_reconciling: bool, pub last_error: String }
  impl StorconClient {
      pub fn new(base: &str, tenant_secondaries: u32) -> Self;
      pub fn with_timeout(self, timeout: Duration) -> Self;
      pub fn with_delete_poll(self, interval: Duration, attempts: u32) -> Self;
      pub async fn create_tenant(&self, tenant: &NeonId) -> Result<(), StorconError>;
      pub async fn describe_tenant(&self, tenant: &NeonId) -> Result<TenantDescribe, StorconError>;
      pub async fn delete_tenant(&self, tenant: &NeonId) -> Result<(), StorconError>;
      pub async fn create_timeline(&self, tenant: &NeonId, timeline: &NeonId, pg_version: u32, ancestor: Option<(&NeonId, Option<Lsn>)>) -> Result<TimelineInfo, StorconError>;
      pub async fn get_timeline(&self, tenant: &NeonId, timeline: &NeonId) -> Result<TimelineInfo, StorconError>;
      pub async fn delete_timeline(&self, tenant: &NeonId, timeline: &NeonId) -> Result<(), StorconError>;
      pub async fn pageserver_connstring(&self, tenant: &NeonId) -> Result<String, StorconError>;
      pub async fn active_safekeepers(&self) -> Result<Vec<String>, StorconError>;
  }
  ```
  `with_timeout` is a small addition to the spec's API, needed to test Review Focus item 4 without waiting 30 s.

- [ ] **Step 1: Dependencies and module**

`crates/cheladb-neon/Cargo.toml` `[dependencies]`, add:

```toml
reqwest = { version = "0.13.5", default-features = false, features = ["json", "rustls"] }
tokio = { version = "1.53.1", features = ["time"] }
```

`[dev-dependencies]`, add:

```toml
axum = "0.8.9"
tokio = { version = "1.53.1", features = ["macros", "rt-multi-thread", "net"] }
```

`src/lib.rs`: add `pub mod storcon;` after `pub mod spec;`, and `pub use storcon::{StorconClient, StorconError};`.

Create `crates/cheladb-neon/src/storcon.rs` containing only its doc comment for now, so the crate builds:

```rust
//! Client for the Neon storage controller's tenant, timeline and placement API.
```

- [ ] **Step 2: Write the failing tests**

Create `crates/cheladb-neon/tests/storcon_mock.rs`:

```rust
//! `StorconClient` against an in-process mock storage controller that answers from a
//! script and records every request.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Router;
use axum::extract::{Request, State};
use axum::http::StatusCode as AxumStatus;
use axum::response::{IntoResponse, Response};
use cheladb_neon::storcon::TimelineInfo;
use cheladb_neon::{Lsn, NeonId, StorconClient, StorconError};
use reqwest::StatusCode;
use serde_json::{Value, json};

#[derive(Debug, Clone)]
struct Seen {
    method: String,
    path: String,
    body: Value,
}

#[derive(Default)]
struct Script {
    replies: Mutex<VecDeque<(u16, &'static str)>>,
    seen: Mutex<Vec<Seen>>,
}

struct Mock {
    base: String,
    script: Arc<Script>,
}

impl Mock {
    fn seen(&self) -> Vec<Seen> {
        self.script.seen.lock().unwrap().clone()
    }
}

async fn serve(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

/// Answers requests in order with `replies` (status, JSON or text body); anything past the
/// end gets 500 "unexpected request".
async fn mock(replies: Vec<(u16, &'static str)>) -> Mock {
    let script = Arc::new(Script {
        replies: Mutex::new(replies.into()),
        seen: Mutex::default(),
    });
    let app = Router::new().fallback(answer).with_state(script.clone());
    Mock {
        base: serve(app).await,
        script,
    }
}

async fn answer(State(script): State<Arc<Script>>, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    script.seen.lock().unwrap().push(Seen {
        method: parts.method.to_string(),
        path: parts.uri.path().to_string(),
        body,
    });
    let (status, reply) = script
        .replies
        .lock()
        .unwrap()
        .pop_front()
        .unwrap_or((500, "unexpected request"));
    (
        AxumStatus::from_u16(status).unwrap(),
        [("content-type", "application/json")],
        reply,
    )
        .into_response()
}

fn fast_deletes(client: StorconClient, attempts: u32) -> StorconClient {
    client.with_delete_poll(Duration::from_millis(1), attempts)
}

fn id(n: u128) -> NeonId {
    NeonId::from(uuid::Uuid::from_u128(n))
}

fn lsn(s: &str) -> Lsn {
    s.parse().unwrap()
}

#[tokio::test]
async fn create_tenant_sends_id_and_placement() {
    let m = mock(vec![(201, "{}")]).await;
    let tenant = id(1);
    StorconClient::new(&m.base, 1)
        .create_tenant(&tenant)
        .await
        .unwrap();
    let seen = m.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].method, "POST");
    assert_eq!(seen[0].path, "/v1/tenant");
    assert_eq!(
        seen[0].body,
        json!({ "new_tenant_id": tenant.as_str(), "placement_policy": { "Attached": 1 } })
    );
}

#[tokio::test]
async fn create_timeline_fresh_and_branch() {
    let m = mock(vec![
        (201, r#"{"timeline_id":"x","ancestor_lsn":null,"last_record_lsn":"0/14EE0B8"}"#),
        (201, r#"{"ancestor_lsn":"0/14EE0B8","last_record_lsn":"0/14EE0B8"}"#),
        (201, r#"{"ancestor_lsn":"0/14EE0B8","last_record_lsn":"0/14EE0B8"}"#),
    ])
    .await;
    let client = StorconClient::new(&m.base, 0);
    let (tenant, main, dev, head) = (id(1), id(2), id(3), id(4));

    let fresh = client.create_timeline(&tenant, &main, 17, None).await.unwrap();
    assert_eq!(
        fresh,
        TimelineInfo {
            ancestor_lsn: None,
            last_record_lsn: lsn("0/14EE0B8"),
        }
    );
    let branch = client
        .create_timeline(&tenant, &dev, 17, Some((&main, Some(lsn("0/14EE0B8")))))
        .await
        .unwrap();
    assert_eq!(branch.ancestor_lsn, Some(lsn("0/14EE0B8")));
    client
        .create_timeline(&tenant, &head, 17, Some((&main, None)))
        .await
        .unwrap();

    let seen = m.seen();
    let path = format!("/v1/tenant/{tenant}/timeline");
    assert!(seen.iter().all(|s| s.method == "POST" && s.path == path));
    assert_eq!(
        seen[0].body,
        json!({ "new_timeline_id": main.as_str(), "pg_version": 17 })
    );
    assert_eq!(
        seen[1].body,
        json!({
            "new_timeline_id": dev.as_str(),
            "ancestor_timeline_id": main.as_str(),
            "ancestor_start_lsn": "0/14EE0B8",
            "pg_version": 17,
        })
    );
    assert_eq!(seen[2].body["ancestor_start_lsn"], Value::Null);
}

#[tokio::test]
async fn get_timeline_reads_it_back() {
    let m = mock(vec![(200, r#"{"ancestor_lsn":null,"last_record_lsn":"0/1F98488"}"#)]).await;
    let (tenant, timeline) = (id(1), id(2));
    let info = StorconClient::new(&m.base, 0)
        .get_timeline(&tenant, &timeline)
        .await
        .unwrap();
    assert_eq!(info.last_record_lsn, lsn("0/1F98488"));
    let seen = m.seen();
    assert_eq!(seen[0].method, "GET");
    assert_eq!(seen[0].path, format!("/v1/tenant/{tenant}/timeline/{timeline}"));
}

#[tokio::test]
async fn a_4xx_with_msg_is_rejected_with_that_message() {
    let m = mock(vec![(409, r#"{"msg":"tenant exists"}"#)]).await;
    let err = StorconClient::new(&m.base, 0)
        .create_tenant(&id(1))
        .await
        .unwrap_err();
    match err {
        StorconError::Rejected { status, message } => {
            assert_eq!(status, StatusCode::CONFLICT);
            assert_eq!(message, "tenant exists");
        }
        other => panic!("expected Rejected, got {other:?}"),
    }
}

#[tokio::test]
async fn a_4xx_with_a_plain_body_keeps_the_body() {
    let m = mock(vec![(400, "bad tenant id")]).await;
    let err = StorconClient::new(&m.base, 0)
        .create_tenant(&id(1))
        .await
        .unwrap_err();
    assert!(
        matches!(&err, StorconError::Rejected { status, message }
            if *status == StatusCode::BAD_REQUEST && message == "bad tenant id"),
        "{err:?}"
    );
}

#[tokio::test]
async fn a_5xx_is_other_and_names_status_and_message() {
    let m = mock(vec![(500, r#"{"msg":"boom"}"#)]).await;
    let err = StorconClient::new(&m.base, 0)
        .create_tenant(&id(1))
        .await
        .unwrap_err();
    assert!(matches!(err, StorconError::Other(_)), "{err:?}");
    let text = err.to_string();
    assert!(text.contains("500") && text.contains("boom"), "{text}");
}

#[tokio::test]
async fn delete_polls_while_accepted() {
    let m = mock(vec![(202, ""), (202, ""), (200, "")]).await;
    let tenant = id(1);
    fast_deletes(StorconClient::new(&m.base, 0), 5)
        .delete_tenant(&tenant)
        .await
        .unwrap();
    let seen = m.seen();
    assert_eq!(seen.len(), 3);
    let path = format!("/v1/tenant/{tenant}");
    assert!(seen.iter().all(|s| s.method == "DELETE" && s.path == path));
}

#[tokio::test]
async fn delete_of_a_missing_object_succeeds_at_once() {
    let m = mock(vec![(404, r#"{"msg":"not found"}"#)]).await;
    let (tenant, timeline) = (id(1), id(2));
    fast_deletes(StorconClient::new(&m.base, 0), 5)
        .delete_timeline(&tenant, &timeline)
        .await
        .unwrap();
    let seen = m.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].path, format!("/v1/tenant/{tenant}/timeline/{timeline}"));
}

#[tokio::test]
async fn delete_gives_up_after_the_attempts() {
    let m = mock(vec![(202, ""), (202, ""), (202, "")]).await;
    let err = fast_deletes(StorconClient::new(&m.base, 0), 3)
        .delete_tenant(&id(1))
        .await
        .unwrap_err();
    assert!(matches!(err, StorconError::Other(_)), "{err:?}");
    assert!(err.to_string().contains("timed out"), "{err}");
    assert_eq!(m.seen().len(), 3);
}

#[tokio::test]
async fn delete_maps_other_statuses_as_errors() {
    let m = mock(vec![(409, r#"{"msg":"has children"}"#)]).await;
    let err = fast_deletes(StorconClient::new(&m.base, 0), 5)
        .delete_timeline(&id(1), &id(2))
        .await
        .unwrap_err();
    assert!(
        matches!(&err, StorconError::Rejected { message, .. } if message == "has children"),
        "{err:?}"
    );
}

#[tokio::test]
async fn pageserver_connstring_uses_the_first_shard() {
    let m = mock(vec![
        (
            200,
            r#"{"shards":[{"shard_id":"s","node_id":1,"listen_pg_addr":"pageserver","listen_pg_port":6400,"listen_http_addr":"pageserver","listen_http_port":9898}]}"#,
        ),
        (200, r#"{"shards":[]}"#),
    ])
    .await;
    let client = StorconClient::new(&m.base, 0);
    let tenant = id(1);
    assert_eq!(
        client.pageserver_connstring(&tenant).await.unwrap(),
        "postgresql://no_user@pageserver:6400"
    );
    assert_eq!(m.seen()[0].path, format!("/debug/v1/tenant/{tenant}/locate"));
    let err = client.pageserver_connstring(&tenant).await.unwrap_err();
    assert!(matches!(err, StorconError::Other(_)), "{err:?}");
}

#[tokio::test]
async fn active_safekeepers_skips_the_rest() {
    let m = mock(vec![(
        200,
        r#"[{"id":1,"host":"safekeeper1","port":5454,"scheduling_policy":"Active"},
            {"id":2,"host":"safekeeper2","port":5454,"scheduling_policy":"Pause"}]"#,
    )])
    .await;
    let sks = StorconClient::new(&m.base, 0).active_safekeepers().await.unwrap();
    assert_eq!(sks, vec!["safekeeper1:5454".to_string()]);
    assert_eq!(m.seen()[0].path, "/control/v1/safekeeper");
}

#[tokio::test]
async fn describe_tenant_parses_shards() {
    let m = mock(vec![(
        200,
        r#"{"tenant_id":"t","shards":[
            {"tenant_shard_id":"t","is_pending_compute_notification":true,"is_reconciling":false,"last_error":"hook failed"},
            {"tenant_shard_id":"t","is_pending_compute_notification":false,"is_reconciling":true,"last_error":null}]}"#,
    )])
    .await;
    let tenant = id(1);
    let d = StorconClient::new(&m.base, 0)
        .describe_tenant(&tenant)
        .await
        .unwrap();
    assert_eq!(d.shards.len(), 2);
    assert!(d.shards[0].is_pending_compute_notification);
    assert!(!d.shards[0].is_reconciling);
    assert_eq!(d.shards[0].last_error, "hook failed");
    assert!(d.shards[1].is_reconciling);
    assert_eq!(d.shards[1].last_error, "");
    assert_eq!(m.seen()[0].path, format!("/control/v1/tenant/{tenant}"));
}

#[tokio::test]
async fn unreachable_server_is_other() {
    // Port 1 on loopback: nothing listens, so the connection is refused.
    let err = StorconClient::new("http://127.0.0.1:1", 0)
        .active_safekeepers()
        .await
        .unwrap_err();
    assert!(matches!(err, StorconError::Other(_)), "{err:?}");
}

#[tokio::test]
async fn trailing_slash_in_base_is_ignored() {
    let m = mock(vec![(200, "[]")]).await;
    StorconClient::new(&format!("{}/", m.base), 0)
        .active_safekeepers()
        .await
        .unwrap();
    assert_eq!(m.seen()[0].path, "/control/v1/safekeeper");
}

#[tokio::test]
async fn malformed_success_body_is_other() {
    let m = mock(vec![(200, r#"{"last_record_lsn":"nope"}"#), (200, "{}")]).await;
    let client = StorconClient::new(&m.base, 0);
    for _ in 0..2 {
        let err = client.get_timeline(&id(1), &id(2)).await.unwrap_err();
        assert!(matches!(err, StorconError::Other(_)), "{err:?}");
    }
}

#[tokio::test]
async fn hanging_server_times_out() {
    let app = Router::new().fallback(|| async {
        tokio::time::sleep(Duration::from_secs(60)).await;
        "late"
    });
    let base = serve(app).await;
    let started = Instant::now();
    let err = StorconClient::new(&base, 0)
        .with_timeout(Duration::from_millis(200))
        .active_safekeepers()
        .await
        .unwrap_err();
    assert!(matches!(err, StorconError::Other(_)), "{err:?}");
    assert!(started.elapsed() < Duration::from_secs(5));
}
```

`uuid` (used by `id()`), `serde_json` and `reqwest` are normal dependencies of the crate, so the integration test can use them without dev-dependency entries.

For `assert_eq!(fresh, TimelineInfo { .. })` to compile, `TimelineInfo` derives `PartialEq` (Step 4 does).

- [ ] **Step 3: Run the tests to see them fail**

Run: `cargo test -p cheladb-neon --test storcon_mock`
Expected: compile errors: `unresolved import cheladb_neon::storcon::TimelineInfo` and `cheladb_neon::StorconClient`.

- [ ] **Step 4: Implement the client**

Replace `crates/cheladb-neon/src/storcon.rs` with:

```rust
//! Client for the Neon storage controller's tenant, timeline and placement API.
//!
//! The Compose storage controller runs with `--dev`, so requests carry no token.

use std::time::Duration;

use reqwest::StatusCode;
use serde::{Deserialize, Deserializer};
use serde_json::json;

use crate::id::NeonId;
use crate::lsn::Lsn;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const DELETE_POLL_INTERVAL: Duration = Duration::from_millis(500);
const DELETE_POLL_ATTEMPTS: u32 = 120;

#[derive(Debug, thiserror::Error)]
pub enum StorconError {
    /// The storage controller understood the request and refused it (4xx).
    #[error("storage controller rejected the request ({status}): {message}")]
    Rejected { status: StatusCode, message: String },
    /// No answer, a timeout, a 5xx, or an answer we couldn't parse.
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl From<reqwest::Error> for StorconError {
    fn from(e: reqwest::Error) -> Self {
        Self::Other(e.into())
    }
}

/// The subset of the storage controller's timeline description we use.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct TimelineInfo {
    pub ancestor_lsn: Option<Lsn>,
    pub last_record_lsn: Lsn,
}

/// `GET /control/v1/tenant/{id}`: per-shard reconcile state.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct TenantDescribe {
    pub shards: Vec<ShardStatus>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ShardStatus {
    pub is_pending_compute_notification: bool,
    pub is_reconciling: bool,
    /// Empty when there is none.
    #[serde(default, deserialize_with = "null_as_empty")]
    pub last_error: String,
}

fn null_as_empty<'de, D: Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    Ok(Option::<String>::deserialize(deserializer)?.unwrap_or_default())
}

#[derive(Debug, Deserialize)]
struct LocateResponse {
    shards: Vec<LocateShard>,
}

#[derive(Debug, Deserialize)]
struct LocateShard {
    listen_pg_addr: String,
    listen_pg_port: u16,
}

#[derive(Debug, Deserialize)]
struct Safekeeper {
    host: String,
    port: u16,
    scheduling_policy: String,
}

#[derive(Clone)]
pub struct StorconClient {
    http: reqwest::Client,
    base: String,
    /// Warm secondary locations per tenant (`PlacementPolicy::Attached(n)`). With at least
    /// one, the storage controller can move a tenant off a draining or failed pageserver.
    tenant_secondaries: u32,
    delete_poll_interval: Duration,
    delete_poll_attempts: u32,
}

fn http_client(timeout: Duration) -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(timeout)
        .build()
        .expect("reqwest client with the default TLS setup")
}

impl StorconClient {
    pub fn new(base: &str, tenant_secondaries: u32) -> Self {
        Self {
            http: http_client(REQUEST_TIMEOUT),
            base: base.trim_end_matches('/').to_string(),
            tenant_secondaries,
            delete_poll_interval: DELETE_POLL_INTERVAL,
            delete_poll_attempts: DELETE_POLL_ATTEMPTS,
        }
    }

    /// Per-request timeout (default 30 s).
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.http = http_client(timeout);
        self
    }

    /// How deletes wait for the storage controller (default 120 × 500 ms).
    pub fn with_delete_poll(mut self, interval: Duration, attempts: u32) -> Self {
        self.delete_poll_interval = interval;
        self.delete_poll_attempts = attempts;
        self
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    pub async fn create_tenant(&self, tenant: &NeonId) -> Result<(), StorconError> {
        let resp = self
            .http
            .post(self.url("/v1/tenant"))
            .json(&json!({
                "new_tenant_id": tenant,
                "placement_policy": { "Attached": self.tenant_secondaries },
            }))
            .send()
            .await?;
        check(resp).await.map(drop)
    }

    pub async fn describe_tenant(&self, tenant: &NeonId) -> Result<TenantDescribe, StorconError> {
        let resp = self
            .http
            .get(self.url(&format!("/control/v1/tenant/{tenant}")))
            .send()
            .await?;
        Ok(check(resp).await?.json().await?)
    }

    pub async fn delete_tenant(&self, tenant: &NeonId) -> Result<(), StorconError> {
        self.delete_until_gone(self.url(&format!("/v1/tenant/{tenant}")))
            .await
    }

    /// Create a timeline: a fresh one (`ancestor` is None) or a branch of `ancestor`,
    /// optionally at a specific LSN.
    pub async fn create_timeline(
        &self,
        tenant: &NeonId,
        timeline: &NeonId,
        pg_version: u32,
        ancestor: Option<(&NeonId, Option<Lsn>)>,
    ) -> Result<TimelineInfo, StorconError> {
        let body = match ancestor {
            None => json!({ "new_timeline_id": timeline, "pg_version": pg_version }),
            Some((ancestor_timeline, lsn)) => json!({
                "new_timeline_id": timeline,
                "ancestor_timeline_id": ancestor_timeline,
                "ancestor_start_lsn": lsn,
                "pg_version": pg_version,
            }),
        };
        let resp = self
            .http
            .post(self.url(&format!("/v1/tenant/{tenant}/timeline")))
            .json(&body)
            .send()
            .await?;
        Ok(check(resp).await?.json().await?)
    }

    pub async fn get_timeline(
        &self,
        tenant: &NeonId,
        timeline: &NeonId,
    ) -> Result<TimelineInfo, StorconError> {
        let resp = self
            .http
            .get(self.url(&format!("/v1/tenant/{tenant}/timeline/{timeline}")))
            .send()
            .await?;
        Ok(check(resp).await?.json().await?)
    }

    pub async fn delete_timeline(
        &self,
        tenant: &NeonId,
        timeline: &NeonId,
    ) -> Result<(), StorconError> {
        self.delete_until_gone(self.url(&format!("/v1/tenant/{tenant}/timeline/{timeline}")))
            .await
    }

    /// `postgresql://no_user@host:port` of the pageserver an (unsharded) tenant is attached to.
    pub async fn pageserver_connstring(&self, tenant: &NeonId) -> Result<String, StorconError> {
        let resp = self
            .http
            .get(self.url(&format!("/debug/v1/tenant/{tenant}/locate")))
            .send()
            .await?;
        let located: LocateResponse = check(resp).await?.json().await?;
        let shard = located
            .shards
            .first()
            .ok_or_else(|| anyhow::anyhow!("tenant {tenant} has no attached shards"))?;
        Ok(format!(
            "postgresql://no_user@{}:{}",
            shard.listen_pg_addr, shard.listen_pg_port
        ))
    }

    /// `host:port` of every active safekeeper.
    pub async fn active_safekeepers(&self) -> Result<Vec<String>, StorconError> {
        let resp = self
            .http
            .get(self.url("/control/v1/safekeeper"))
            .send()
            .await?;
        let all: Vec<Safekeeper> = check(resp).await?.json().await?;
        Ok(all
            .into_iter()
            .filter(|sk| sk.scheduling_policy == "Active")
            .map(|sk| format!("{}:{}", sk.host, sk.port))
            .collect())
    }

    /// Deletion is asynchronous: the storage controller answers 202 while it is in
    /// progress and 200 once the object is gone (404 also means gone).
    async fn delete_until_gone(&self, url: String) -> Result<(), StorconError> {
        for attempt in 1..=self.delete_poll_attempts {
            let resp = self.http.delete(&url).send().await?;
            match resp.status() {
                StatusCode::OK | StatusCode::NOT_FOUND => return Ok(()),
                StatusCode::ACCEPTED => {
                    if attempt < self.delete_poll_attempts {
                        tokio::time::sleep(self.delete_poll_interval).await;
                    }
                }
                _ => {
                    check(resp).await?;
                }
            }
        }
        Err(anyhow::anyhow!("timed out waiting for deletion of {url}").into())
    }
}

async fn check(resp: reqwest::Response) -> Result<reqwest::Response, StorconError> {
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    let body = resp.text().await.unwrap_or_default();
    // Errors come back as {"msg": "..."}; fall back to the raw body.
    let message = serde_json::from_str::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| v.get("msg").and_then(|m| m.as_str()).map(str::to_string))
        .unwrap_or(body);
    if status.is_client_error() {
        Err(StorconError::Rejected { status, message })
    } else {
        Err(anyhow::anyhow!("storage controller returned {status}: {message}").into())
    }
}
```

Note on `delete_until_gone`'s `_ =>` arm: a 2xx other than 200/202 (e.g. 204) passes `check` and the loop continues polling; that matches the old behaviour.

- [ ] **Step 5: Run the tests to see them pass**

Run: `cargo test -p cheladb-neon --test storcon_mock`
Expected: 17 passed.

- [ ] **Step 6: Whole crate, format, lint, commit**

```bash
cargo test -p cheladb-neon
rustfmt --edition 2024 crates/cheladb-neon/src/storcon.rs crates/cheladb-neon/src/lib.rs crates/cheladb-neon/tests/storcon_mock.rs
cargo clippy -p cheladb-neon --all-targets -- -D warnings
git add Cargo.lock crates/cheladb-neon
git commit -F- <<'EOF'
cheladb-neon: storage-controller client with mock tests

Ported from the old control plane, with typed ids, describe_tenant, a request timeout and
an injectable delete poll. Tested against an in-process mock.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01KESSEqEbiuKm7pMwCPe2Wg
EOF
```

Expected: all unit and mock tests pass (30 in total: 7 id/lsn, 2 scram, 3 jwt, 1 spec, 17 mock); clippy clean.

---

### Task 7: Live round trip, recipe and roadmap

**Files:**
- Create: `crates/cheladb-neon/tests/storcon_live.rs`
- Modify: `justfile` (after the `neon-test` recipe, around line 292)
- Modify: `docs/roadmap/2026-09-28-neon-and-data-api.md`

**Interfaces:**
- Consumes: `StorconClient`, `StorconError`, `NeonId` (Tasks 2 and 6).
- Produces: `just neon-test-live`.

- [ ] **Step 1: Write the live test**

Create `crates/cheladb-neon/tests/storcon_live.rs`:

```rust
//! Opt-in round trip through `StorconClient` against a running storage controller
//! (`just neon-up` first):
//!
//! ```sh
//! just neon-test-live
//! ```
//!
//! It creates a throwaway tenant and deletes it, even when an assertion fails.

use cheladb_neon::{NeonId, StorconClient};

fn client() -> StorconClient {
    let url = std::env::var("CHELADB_STORCON_URL")
        .unwrap_or_else(|_| "http://127.0.0.1:1234".to_string());
    StorconClient::new(&url, 0)
}

#[tokio::test]
#[ignore = "needs the Neon storage stack: just neon-up"]
async fn tenant_and_timeline_round_trip() {
    let client = client();
    let tenant = NeonId::new();
    let result = tokio::spawn(round_trip(client.clone(), tenant.clone())).await;
    if let Err(panic) = result {
        // The round trip stopped part-way; don't leave the tenant behind.
        if let Err(e) = client.delete_tenant(&tenant).await {
            eprintln!("could not delete tenant {tenant}: {e}");
        }
        std::panic::resume_unwind(panic.into_panic());
    }
}

async fn round_trip(client: StorconClient, tenant: NeonId) {
    let safekeepers = client
        .active_safekeepers()
        .await
        .expect("storage controller reachable (just neon-up)");
    assert!(!safekeepers.is_empty(), "no Active safekeeper");

    client.create_tenant(&tenant).await.unwrap();
    let described = client.describe_tenant(&tenant).await.unwrap();
    assert!(!described.shards.is_empty());
    for shard in &described.shards {
        assert!(
            !shard.is_pending_compute_notification,
            "compute notification pending (is storcon-hook answering?): {shard:?}"
        );
        assert_eq!(shard.last_error, "", "{shard:?}");
    }

    let main = NeonId::new();
    let created = client.create_timeline(&tenant, &main, 17, None).await.unwrap();
    let fetched = client.get_timeline(&tenant, &main).await.unwrap();
    assert!(fetched.last_record_lsn >= created.last_record_lsn);

    let branch = NeonId::new();
    let at = fetched.last_record_lsn;
    let branched = client
        .create_timeline(&tenant, &branch, 17, Some((&main, Some(at))))
        .await
        .unwrap();
    assert_eq!(branched.ancestor_lsn, Some(at));

    assert_eq!(
        client.pageserver_connstring(&tenant).await.unwrap(),
        "postgresql://no_user@pageserver:6400"
    );

    client.delete_timeline(&tenant, &branch).await.unwrap();
    client.delete_timeline(&tenant, &main).await.unwrap();
    client.delete_tenant(&tenant).await.unwrap();
}
```

- [ ] **Step 2: Check it's skipped by default**

Run: `cargo test -p cheladb-neon --test storcon_live`
Expected: `1 ignored`, 0 run.

- [ ] **Step 3: Add the recipe**

In `justfile`, after the `neon-test` recipe (`infra/neon/tests/run.sh`), add:

```just

# Round trip through the Rust storage-controller client (needs just neon-up)
neon-test-live:
    cargo test -p cheladb-neon --test storcon_live -- --ignored
```

- [ ] **Step 4: Run it against the real stack**

```bash
just neon-up
just neon-test-live
```

Expected: `1 passed`. If `neon-up` fails on the image pull, the GHCR packages are private: ask the user to run `! docker login ghcr.io` and retry. Do not skip this step; if the stack can't be started, stop and report.

If an assertion fails, the failure message says which Neon response differed. Fix the client or the test to match what the storage controller actually sends (for example a field's type), rerun the mock tests, and note the finding for Step 6.

Then check that nothing was left behind:

```bash
curl -s http://127.0.0.1:1234/v1/tenant | jq length
```

Expected: the same count as before the run (`0` on a fresh stack).

- [ ] **Step 5: Full verification**

```bash
cargo test -p cheladb-neon
cargo clippy -p cheladb-neon --all-targets -- -D warnings
cargo test --workspace 2>&1 | grep -E '^test result|FAILED|panicked'
just neon-test
```

Expected: 30 passed, 1 ignored in `cheladb-neon`; clippy clean; the workspace results match Task 1's baseline plus the new crate; `neon-test` passes.

- [ ] **Step 6: Update the roadmap**

In `docs/roadmap/2026-09-28-neon-and-data-api.md`:

1. Under "Where we are", after the Neon storage stack bullet, add:
   ```markdown
   - **`crates/cheladb-neon`** (step 2, done): the storage-controller client, compute spec, SCRAM, LSNs, compute JWTs and Neon ids. Nothing calls it yet.
   ```
2. Replace step 2 in "Next steps, in order" with:
   ```markdown
   2. **`crates/cheladb-neon`.** ✅ Done 2026-09-28; see "Progress".
      - A straight port with its tests: storcon client, compute-spec builder, `scram`, `lsn`, compute JWKS.
   ```
3. Under "Can be done at any point", add a line:
   ```markdown
   **Dependency follow-up:** `cheladb-neon` uses reqwest 0.13, jsonwebtoken 11 and base64 0.23; the other crates still use 0.12, 9 and 0.22, so `Cargo.lock` carries both. Move the rest of the workspace up and make them workspace pins.
   ```
4. Under "Progress", after step 1's subsection, add:
   ```markdown
   ### Step 2: `crates/cheladb-neon` (done 2026-09-28)

   Spec: `docs/superpowers/specs/2026-09-28-cheladb-neon-crate-design.md`. Plan: `docs/superpowers/plans/2026-09-28-cheladb-neon-crate.md`.

   **What exists now:**
   - The workspace is on edition 2024 (resolver 3). No rustfmt run; existing files keep their formatting.
   - `crates/cheladb-neon`, at the latest crate versions, declared in its own `Cargo.toml`:
     - `NeonId`: 32 lowercase hex; `NeonId::from(uuid)` maps one of our ids to its tenant id.
     - `Lsn`, with serde as the text form.
     - `scram::verifier`, with the RFC 7677 vector.
     - `spec::config_response` for compute_ctl.
     - `ComputeSigner`: Ed25519 tokens and the JWKS for `compute_ctl_config`.
     - `StorconClient`: tenants (create, describe, delete), timelines (create, branch, get, delete), locate, active safekeepers. 30 s timeout; deletes poll 120 × 500 ms.
   - Tests: unit tests, `tests/storcon_mock.rs` (in-process mock), and `tests/storcon_live.rs`, run by `just neon-test-live` after `just neon-up`.

   **Findings to carry forward:**
   - The old `jwt.rs` and `scram.rs` compile unchanged against jsonwebtoken 11 and the 0.11/0.13 RustCrypto crates.
   ```
   Add to the findings list anything Task 1 or Step 4 turned up (edits `cargo fix` made, a response shape that differed from the mock). If nothing else turned up, leave the list as above.

- [ ] **Step 7: Format, commit**

```bash
rustfmt --edition 2024 crates/cheladb-neon/tests/storcon_live.rs
git add crates/cheladb-neon/tests/storcon_live.rs justfile docs/roadmap/2026-09-28-neon-and-data-api.md
git commit -F- <<'EOF'
cheladb-neon: live storcon round trip; roadmap: step 2 done

just neon-test-live runs the client against just neon-up: tenant, describe, timeline,
branch, locate, safekeepers, deletes.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01KESSEqEbiuKm7pMwCPe2Wg
EOF
```
