# Auth + Soft-Bind + Orchestrator Test Coverage Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add focused Rust unit/integration tests covering JWKS-based JWT verification (`cheladb-auth`), org→tenant soft-bind resolution, and orchestrator `/v1/databases` status codes (401/403/202), with all JWKS/Zitadel interaction mocked in-process and no dependency on live Zitadel, Postgres, NATS, or Docker.

**Architecture:** Add a reusable `test_support` module (feature-gated) to `cheladb-auth` that spins up a real in-process axum server serving a JWKS document for a freshly generated RSA keypair, and can sign JWTs against that key. Split `cheladb-auth::JwtVerifier::verify` so claim/audience extraction is a pure, directly-testable function separate from network/signature verification. Extract the orchestrator's `AppState`, `AuthPrincipal` extractor, and handlers out of `main.rs` into `lib.rs`, generic over two small local traits (`MetadataPort`, `JobPort`) so tests can inject in-memory fakes instead of a real Postgres/NATS connection. `main.rs` becomes a thin binary that wires the real `MetadataClient`/`JetStreamContext` implementations. Orchestrator integration tests then boot the real router (via the fakes) on an ephemeral port, hit it with `reqwest`, and authenticate using tokens signed by the same JWKS mock.

**Tech Stack:** Rust 1.98, axum 0.8, tokio, jsonwebtoken 9, reqwest (workspace, rustls-tls), `rsa` 0.9 (new, feature-gated dev-only dependency of `cheladb-auth`), `base64` (workspace, already present).

**Spec:** `docs/superpowers/specs/2026-09-27-orchestrator-auth-design.md`

## Global Constraints

- No test may require a live Zitadel instance, Docker, Postgres, or NATS — all such dependencies are replaced with in-process fakes/mocks.
- Do not modify `apps/gateway` (it is an intentional stub; out of scope per the design spec).
- Do not modify `apps/worker` or `apps/cheladb` (the Zitadel bootstrap/token CLI is pure live-Zitadel HTTP calls with no pure logic worth unit testing, and is explicitly out of scope for this pass).
- Keep the orchestrator's external HTTP behavior byte-for-byte identical to today (same routes, same status codes, same JSON shapes) — this is a refactor for testability, not a behavior change.
- New crate dependencies (`rsa`) must be added only to `cheladb-auth`, gated behind an off-by-default `test-support` feature so production builds are unaffected.
- Use native Rust async-fn-in-trait (RPITIT, stable since 1.75) for `MetadataPort`/`JobPort` — no `async-trait` or `dyn` dispatch needed since call sites are generic.

## Review Focus

- **Org claim fallback:** a token with no `urn:zitadel:iam:org:id` but a valid `urn:zitadel:iam:user:resourceowner:id` must still verify successfully (spec explicitly allows this fallback) — covered in Task 2.
- **Unbound org still 403s cleanly:** a syntactically valid, correctly signed token whose org has no matching `tenants.zitadel_org_id` row must produce 403 with the "run just zitadel-bootstrap" message, not 500 — covered in Task 5.
- **Soft-bind body/principal mismatch:** an authenticated caller who passes a `tenant_id` in the POST body that differs from their resolved tenant must get 403, even though the token itself is perfectly valid — covered in Task 5.
- **JWKS cache/kid miss:** a token signed with a `kid` not present in the mocked JWKS response must fail verification (not panic, not silently pass) — covered in Task 3.
- **Expired token is rejected, not merely logged:** `exp` in the past must produce `AuthError::InvalidToken`, propagated by the orchestrator as 401, not treated as a soft warning — covered in Tasks 3 and 5.

---

## Task 1: Add `test_support` JWKS mock helper to `cheladb-auth`

**Files:**
- Modify: `crates/cheladb-auth/Cargo.toml`
- Modify: `crates/cheladb-auth/src/lib.rs`

**Interfaces:**
- Produces: `cheladb_auth::test_support::MockJwks` with:
  - `async fn start() -> MockJwks`
  - `fn config(&self, audiences: Vec<String>) -> ZitadelAuthConfig`
  - `fn sign(&self, claims: serde_json::Value) -> String`
  - `fn token_for_org(&self, sub: &str, org_id: &str, ttl_secs: i64) -> String`
  - `fn issuer(&self) -> &str`
  - `fn jwks_url(&self) -> &str`
  - `fn kid(&self) -> &str`

This is consumed by Tasks 2/3 (this crate's own tests) and Task 5 (`apps/orchestrator` integration tests).

- [ ] **Step 1: Add dependencies**

Edit `crates/cheladb-auth/Cargo.toml` to:

```toml
[package]
name = "cheladb-auth"
version.workspace = true
edition.workspace = true
license.workspace = true
description = "Zitadel JWKS JWT verification for Cheladb"

[dependencies]
serde = { workspace = true }
serde_json = { workspace = true }
thiserror = { workspace = true }
jsonwebtoken = { workspace = true }
reqwest = { workspace = true }
parking_lot = { workspace = true }
tracing = { workspace = true }
tokio = { workspace = true }
axum = { workspace = true, optional = true }
rsa = { version = "0.9", features = ["pem"], optional = true }
base64 = { workspace = true, optional = true }
rand = { workspace = true, optional = true }

[dev-dependencies]
cheladb-auth = { path = ".", features = ["test-support"] }

[features]
test-support = ["dep:axum", "dep:rsa", "dep:base64", "dep:rand"]
```

(The self-referential `[dev-dependencies]` entry is what makes `cargo test -p cheladb-auth` build this crate's own `tests/` with the feature enabled, without turning it on for normal library consumers like `apps/orchestrator`, which will opt in explicitly in Task 4.)

- [ ] **Step 2: Add the `test_support` module**

Append to `crates/cheladb-auth/src/lib.rs`:

```rust
#[cfg(feature = "test-support")]
pub mod test_support {
    //! In-process JWKS mock for tests. No network calls beyond localhost.

    use crate::{ZitadelAuthConfig, CLAIM_ORG_ID};
    use axum::{routing::get, Json, Router};
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
    use jsonwebtoken::{encode, EncodingKey, Header};
    use rsa::pkcs1::EncodeRsaPrivateKey;
    use rsa::traits::PublicKeyParts;
    use rsa::RsaPrivateKey;
    use serde_json::{json, Value};
    use std::time::{SystemTime, UNIX_EPOCH};
    use tokio::net::TcpListener;
    use tokio::sync::oneshot;

    /// A locally-hosted JWKS endpoint backed by a freshly generated RSA keypair,
    /// plus a signer for minting test JWTs against that same key.
    pub struct MockJwks {
        issuer: String,
        jwks_url: String,
        kid: String,
        encoding_key: EncodingKey,
        _shutdown: oneshot::Sender<()>,
    }

    impl MockJwks {
        /// Start the mock JWKS server on an ephemeral localhost port.
        pub async fn start() -> Self {
            let mut rng = rand::thread_rng();
            let private_key = RsaPrivateKey::new(&mut rng, 2048).expect("rsa keygen");
            let public_key = private_key.to_public_key();
            let n = URL_SAFE_NO_PAD.encode(public_key.n().to_bytes_be());
            let e = URL_SAFE_NO_PAD.encode(public_key.e().to_bytes_be());
            let kid = "test-key-1".to_string();

            let pem = private_key
                .to_pkcs1_pem(rsa::pkcs8::LineEnding::LF)
                .expect("encode rsa pem");
            let encoding_key =
                EncodingKey::from_rsa_pem(pem.as_bytes()).expect("build jsonwebtoken encoding key");

            let jwks_body = json!({
                "keys": [{
                    "kty": "RSA",
                    "use": "sig",
                    "alg": "RS256",
                    "kid": kid,
                    "n": n,
                    "e": e,
                }]
            });

            let app = Router::new().route(
                "/oauth/v2/keys",
                get(move || {
                    let body = jwks_body.clone();
                    async move { Json(body) }
                }),
            );

            let listener = TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind mock jwks listener");
            let addr = listener.local_addr().expect("local addr");

            let (shutdown_tx, shutdown_rx) = oneshot::channel();
            tokio::spawn(async move {
                let _ = axum::serve(listener, app)
                    .with_graceful_shutdown(async {
                        let _ = shutdown_rx.await;
                    })
                    .await;
            });

            let issuer = format!("http://{addr}");
            let jwks_url = format!("http://{addr}/oauth/v2/keys");

            Self {
                issuer,
                jwks_url,
                kid,
                encoding_key,
                _shutdown: shutdown_tx,
            }
        }

        pub fn issuer(&self) -> &str {
            &self.issuer
        }

        pub fn jwks_url(&self) -> &str {
            &self.jwks_url
        }

        pub fn kid(&self) -> &str {
            &self.kid
        }

        /// Build a `ZitadelAuthConfig` pointing at this mock's issuer/JWKS URL.
        pub fn config(&self, audiences: Vec<String>) -> ZitadelAuthConfig {
            ZitadelAuthConfig {
                issuer: self.issuer.clone(),
                jwks_url: self.jwks_url.clone(),
                audiences,
            }
        }

        /// Sign arbitrary claims with this mock's key, using the correct `kid` header.
        /// Callers control every claim, including `iss`/`exp`, for negative-path tests.
        pub fn sign(&self, claims: Value) -> String {
            let mut header = Header::new(jsonwebtoken::Algorithm::RS256);
            header.kid = Some(self.kid.clone());
            encode(&header, &claims, &self.encoding_key).expect("sign test jwt")
        }

        /// Sign a well-formed, currently-valid token for the given subject/org.
        pub fn token_for_org(&self, sub: &str, org_id: &str, ttl_secs: i64) -> String {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system time")
                .as_secs() as i64;
            let claims = json!({
                "sub": sub,
                "iss": self.issuer,
                "iat": now,
                "exp": now + ttl_secs,
                CLAIM_ORG_ID: org_id,
            });
            self.sign(claims)
        }
    }
}
```

- [ ] **Step 3: Confirm it compiles**

Run: `cargo check -p cheladb-auth --features test-support`
Expected: compiles with no errors (warnings about unused items are fine at this point since nothing calls `MockJwks` yet).

- [ ] **Step 4: Commit**

```bash
git add crates/cheladb-auth/Cargo.toml crates/cheladb-auth/src/lib.rs
git commit -m "test: add in-process JWKS mock helper to cheladb-auth"
```

---

## Task 2: Extract pure claim-extraction logic and unit test it

**Files:**
- Modify: `crates/cheladb-auth/src/lib.rs`

**Interfaces:**
- Produces: private `fn build_claims(claims: &serde_json::Value, config: &ZitadelAuthConfig) -> Result<VerifiedClaims, AuthError>` (crate-internal, exercised via `#[cfg(test)] mod tests` in the same file — no need to make it `pub`).
- Consumes: nothing new; reuses `VerifiedClaims`, `AuthError`, `ZitadelAuthConfig`, `CLAIM_ORG_ID`, `CLAIM_RESOURCE_OWNER_ID` already defined in this file.

This isolates the sub/org/audience extraction logic (today inlined in `JwtVerifier::verify`) so it can be tested without any signing or networking, and reused by `verify()` unchanged.

- [ ] **Step 1: Write the failing tests**

Add to the bottom of `crates/cheladb-auth/src/lib.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn config(audiences: Vec<String>) -> ZitadelAuthConfig {
        ZitadelAuthConfig {
            issuer: "http://issuer.test".into(),
            jwks_url: "http://issuer.test/keys".into(),
            audiences,
        }
    }

    #[test]
    fn build_claims_uses_org_id_claim_when_present() {
        let claims = json!({
            "sub": "user-1",
            CLAIM_ORG_ID: "org-1",
            CLAIM_RESOURCE_OWNER_ID: "org-2",
        });
        let result = build_claims(&claims, &config(vec![])).unwrap();
        assert_eq!(result.sub, "user-1");
        assert_eq!(result.org_id, "org-1");
    }

    #[test]
    fn build_claims_falls_back_to_resource_owner_claim() {
        let claims = json!({
            "sub": "user-1",
            CLAIM_RESOURCE_OWNER_ID: "org-2",
        });
        let result = build_claims(&claims, &config(vec![])).unwrap();
        assert_eq!(result.org_id, "org-2");
    }

    #[test]
    fn build_claims_errors_when_no_org_claim_present() {
        let claims = json!({ "sub": "user-1" });
        let err = build_claims(&claims, &config(vec![])).unwrap_err();
        assert!(matches!(err, AuthError::InvalidToken(_)));
    }

    #[test]
    fn build_claims_errors_when_sub_missing() {
        let claims = json!({ CLAIM_ORG_ID: "org-1" });
        let err = build_claims(&claims, &config(vec![])).unwrap_err();
        assert!(matches!(err, AuthError::InvalidToken(_)));
    }

    #[test]
    fn build_claims_errors_when_sub_empty() {
        let claims = json!({ "sub": "", CLAIM_ORG_ID: "org-1" });
        let err = build_claims(&claims, &config(vec![])).unwrap_err();
        assert!(matches!(err, AuthError::InvalidToken(_)));
    }

    #[test]
    fn build_claims_skips_audience_check_when_config_has_none() {
        let claims = json!({ "sub": "user-1", CLAIM_ORG_ID: "org-1", "aud": "someone-else" });
        assert!(build_claims(&claims, &config(vec![])).is_ok());
    }

    #[test]
    fn build_claims_skips_audience_check_when_token_has_no_aud() {
        let claims = json!({ "sub": "user-1", CLAIM_ORG_ID: "org-1" });
        assert!(build_claims(&claims, &config(vec!["expected-aud".into()])).is_ok());
    }

    #[test]
    fn build_claims_accepts_matching_audience() {
        let claims = json!({ "sub": "user-1", CLAIM_ORG_ID: "org-1", "aud": "expected-aud" });
        assert!(build_claims(&claims, &config(vec!["expected-aud".into()])).is_ok());
    }

    #[test]
    fn build_claims_accepts_matching_audience_in_array() {
        let claims = json!({
            "sub": "user-1",
            CLAIM_ORG_ID: "org-1",
            "aud": ["other-aud", "expected-aud"],
        });
        assert!(build_claims(&claims, &config(vec!["expected-aud".into()])).is_ok());
    }

    #[test]
    fn build_claims_rejects_mismatched_audience() {
        let claims = json!({ "sub": "user-1", CLAIM_ORG_ID: "org-1", "aud": "wrong-aud" });
        let err = build_claims(&claims, &config(vec!["expected-aud".into()])).unwrap_err();
        assert!(matches!(err, AuthError::InvalidToken(_)));
    }
}
```

- [ ] **Step 2: Run tests to verify they fail to compile (function doesn't exist yet)**

Run: `cargo test -p cheladb-auth --lib`
Expected: compile error, `cannot find function 'build_claims' in this scope`.

- [ ] **Step 3: Extract `build_claims` and call it from `verify`**

In `crates/cheladb-auth/src/lib.rs`, replace the body of `JwtVerifier::verify` from `self.check_audience(&claims)?;` through the end of the function, and remove `check_audience` as a method (moving its logic into the new free function). Full replacement of the `verify` method and the `check_audience` method:

```rust
    /// Verify a raw Bearer token (without the `Bearer ` prefix).
    pub async fn verify(&self, token: &str) -> Result<VerifiedClaims, AuthError> {
        let token = token.trim();
        if token.is_empty() {
            return Err(AuthError::Unauthorized);
        }

        let header = decode_header(token)
            .map_err(|e| AuthError::InvalidToken(format!("header: {e}")))?;
        let kid = header
            .kid
            .ok_or_else(|| AuthError::InvalidToken("missing kid".into()))?;

        let key = self.decoding_key_for_kid(&kid).await?;

        let mut validation = Validation::new(Algorithm::RS256);
        validation.set_issuer(&[self.config.issuer.clone()]);
        validation.validate_aud = false;
        validation.leeway = 60;

        let data = decode::<Value>(token, &key, &validation)
            .map_err(|e| AuthError::InvalidToken(e.to_string()))?;

        build_claims(&data.claims, &self.config)
    }
```

Then add the extracted free function near the bottom of the file (above the `#[cfg(test)]` module, below `impl JwtVerifier`):

```rust
/// Extract and validate `sub`/org/audience from already-signature-verified claims.
fn build_claims(claims: &Value, config: &ZitadelAuthConfig) -> Result<VerifiedClaims, AuthError> {
    check_audience(claims, config)?;

    let sub = claims
        .get("sub")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AuthError::InvalidToken("missing sub".into()))?
        .to_string();

    let org_id = claims
        .get(CLAIM_ORG_ID)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            claims
                .get(CLAIM_RESOURCE_OWNER_ID)
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
        })
        .ok_or_else(|| {
            AuthError::InvalidToken(format!(
                "missing org claim ({CLAIM_ORG_ID} or {CLAIM_RESOURCE_OWNER_ID})"
            ))
        })?
        .to_string();

    Ok(VerifiedClaims { sub, org_id })
}

fn check_audience(claims: &Value, config: &ZitadelAuthConfig) -> Result<(), AuthError> {
    if config.audiences.is_empty() {
        return Ok(());
    }

    let Some(aud_value) = claims.get("aud") else {
        // Local Zitadel PATs often omit aud; soft-bind is the gate.
        debug!("token has no aud claim; skipping audience check");
        return Ok(());
    };

    let token_auds: Vec<String> = match aud_value {
        Value::String(s) if !s.is_empty() => vec![s.clone()],
        Value::Array(arr) => arr
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    };

    if token_auds.is_empty() {
        debug!("token aud empty; skipping audience check");
        return Ok(());
    }

    let ok = token_auds
        .iter()
        .any(|a| config.audiences.iter().any(|e| e == a));
    if !ok {
        return Err(AuthError::InvalidToken(
            "token audience does not match CHELADB_ZITADEL_AUDIENCES".into(),
        ));
    }
    Ok(())
}
```

Delete the now-unused `JwtVerifier::check_audience` method (its body has moved to the free function above).

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p cheladb-auth --lib`
Expected: all `build_claims_*` tests PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/cheladb-auth/src/lib.rs
git commit -m "refactor: extract pure claim/audience validation for unit testing"
```

---

## Task 3: Integration tests for `JwtVerifier::verify` against the mock JWKS server

**Files:**
- Create: `crates/cheladb-auth/tests/verify.rs`

**Interfaces:**
- Consumes: `cheladb_auth::{JwtVerifier, AuthError}` and `cheladb_auth::test_support::MockJwks` (from Task 1).

- [ ] **Step 1: Write the tests**

```rust
use cheladb_auth::test_support::MockJwks;
use cheladb_auth::{AuthError, JwtVerifier};
use serde_json::json;
use std::time::{SystemTime, UNIX_EPOCH};

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

#[tokio::test]
async fn valid_token_verifies_successfully() {
    let mock = MockJwks::start().await;
    let verifier = JwtVerifier::new(mock.config(vec![]));
    let token = mock.token_for_org("user-1", "org-1", 300);

    let claims = verifier.verify(&token).await.expect("should verify");
    assert_eq!(claims.sub, "user-1");
    assert_eq!(claims.org_id, "org-1");
}

#[tokio::test]
async fn expired_token_is_rejected() {
    let mock = MockJwks::start().await;
    let verifier = JwtVerifier::new(mock.config(vec![]));
    let token = mock.sign(json!({
        "sub": "user-1",
        "iss": mock.issuer(),
        "exp": now() - 3600,
        cheladb_auth::CLAIM_ORG_ID: "org-1",
    }));

    let err = verifier.verify(&token).await.unwrap_err();
    assert!(matches!(err, AuthError::InvalidToken(_)), "got {err:?}");
}

#[tokio::test]
async fn wrong_issuer_is_rejected() {
    let mock = MockJwks::start().await;
    let verifier = JwtVerifier::new(mock.config(vec![]));
    let token = mock.sign(json!({
        "sub": "user-1",
        "iss": "http://not-the-configured-issuer",
        "exp": now() + 300,
        cheladb_auth::CLAIM_ORG_ID: "org-1",
    }));

    let err = verifier.verify(&token).await.unwrap_err();
    assert!(matches!(err, AuthError::InvalidToken(_)), "got {err:?}");
}

#[tokio::test]
async fn unknown_kid_is_rejected() {
    let mock = MockJwks::start().await;
    let verifier = JwtVerifier::new(mock.config(vec![]));

    // Sign with the mock's real key but claim a kid that isn't in the JWKS response.
    let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256);
    header.kid = Some("no-such-kid".into());
    // We don't have direct access to the encoding key from outside test_support,
    // so instead assert against a token whose kid the verifier will fail to resolve
    // by mutating a validly-shaped but bogus JWT header via sign() + swapping kid
    // is not supported; use a syntactically valid but foreign token instead.
    let token = mock.token_for_org("user-1", "org-1", 300);
    let tampered = tamper_kid(&token, "no-such-kid");

    let err = verifier.verify(&tampered).await.unwrap_err();
    assert!(matches!(err, AuthError::InvalidToken(_)), "got {err:?}");
}

#[tokio::test]
async fn missing_kid_header_is_rejected() {
    let mock = MockJwks::start().await;
    let verifier = JwtVerifier::new(mock.config(vec![]));
    let tampered = tamper_kid_removed(&mock.token_for_org("user-1", "org-1", 300));

    let err = verifier.verify(&tampered).await.unwrap_err();
    assert!(matches!(err, AuthError::InvalidToken(_)), "got {err:?}");
}

#[tokio::test]
async fn malformed_token_is_rejected() {
    let mock = MockJwks::start().await;
    let verifier = JwtVerifier::new(mock.config(vec![]));

    let err = verifier.verify("not-a-jwt").await.unwrap_err();
    assert!(matches!(err, AuthError::InvalidToken(_)), "got {err:?}");
}

#[tokio::test]
async fn empty_token_is_unauthorized() {
    let mock = MockJwks::start().await;
    let verifier = JwtVerifier::new(mock.config(vec![]));

    let err = verifier.verify("   ").await.unwrap_err();
    assert!(matches!(err, AuthError::Unauthorized), "got {err:?}");
}

#[tokio::test]
async fn audience_mismatch_is_rejected_end_to_end() {
    let mock = MockJwks::start().await;
    let verifier = JwtVerifier::new(mock.config(vec!["expected-aud".into()]));
    let token = mock.sign(json!({
        "sub": "user-1",
        "iss": mock.issuer(),
        "exp": now() + 300,
        "aud": "wrong-aud",
        cheladb_auth::CLAIM_ORG_ID: "org-1",
    }));

    let err = verifier.verify(&token).await.unwrap_err();
    assert!(matches!(err, AuthError::InvalidToken(_)), "got {err:?}");
}

#[tokio::test]
async fn jwks_endpoint_unreachable_surfaces_jwks_error() {
    let config = cheladb_auth::ZitadelAuthConfig {
        issuer: "http://127.0.0.1:1".into(),
        jwks_url: "http://127.0.0.1:1/oauth/v2/keys".into(),
        audiences: vec![],
    };
    let verifier = JwtVerifier::new(config);

    // Any syntactically valid header with a kid triggers a JWKS fetch attempt.
    let mock = MockJwks::start().await;
    let token = mock.token_for_org("user-1", "org-1", 300);

    let err = verifier.verify(&token).await.unwrap_err();
    assert!(matches!(err, AuthError::Jwks(_)), "got {err:?}");
}

/// Re-signs nothing; swaps the base64url header segment's `kid` field for negative tests.
/// Tokens are `header.payload.signature`; the signature will no longer validate for a
/// *different key*, but here we're only exercising the "kid not found" path, and
/// jsonwebtoken checks the header/kid lookup before signature validation in this crate's
/// call order (decode_header runs first, then decoding_key_for_kid looks up by kid).
fn tamper_kid(token: &str, new_kid: &str) -> String {
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
    let mut parts = token.splitn(3, '.');
    let header_b64 = parts.next().unwrap();
    let rest: Vec<&str> = parts.collect();
    let mut header: serde_json::Value =
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(header_b64).unwrap()).unwrap();
    header["kid"] = serde_json::Value::String(new_kid.to_string());
    let new_header_b64 = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&header).unwrap());
    format!("{new_header_b64}.{}", rest.join("."))
}

fn tamper_kid_removed(token: &str) -> String {
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
    let mut parts = token.splitn(3, '.');
    let header_b64 = parts.next().unwrap();
    let rest: Vec<&str> = parts.collect();
    let mut header: serde_json::Value =
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(header_b64).unwrap()).unwrap();
    header.as_object_mut().unwrap().remove("kid");
    let new_header_b64 = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&header).unwrap());
    format!("{new_header_b64}.{}", rest.join("."))
}
```

This test file needs `base64` directly (not just via the feature-gated re-export), so it must be declared as a dev-dependency of `cheladb-auth` too.

- [ ] **Step 2: Add `base64` as a plain dev-dependency**

In `crates/cheladb-auth/Cargo.toml`, add under `[dev-dependencies]` (alongside the existing self-reference from Task 1):

```toml
[dev-dependencies]
cheladb-auth = { path = ".", features = ["test-support"] }
base64 = { workspace = true }
serde_json = { workspace = true }
```

(`serde_json` is already a normal dependency, but listing it in `[dev-dependencies]` too is harmless and keeps the test file's imports self-evident; skip it if `cargo check` complains about a duplicate — normal deps are already visible to integration tests.)

- [ ] **Step 3: Run tests to verify they fail first (file doesn't exist yet until Step 1 lands, so this really just confirms compile+run after adding)**

Run: `cargo test -p cheladb-auth --test verify --features test-support`
Expected: all tests PASS. If `jwks_endpoint_unreachable_surfaces_jwks_error` hangs or times out (connection to `127.0.0.1:1` may hang rather than refuse on some systems), replace port `1` with an address you've confirmed refuses immediately in this sandbox (e.g. bind and immediately drop a `TcpListener` on `127.0.0.1:0` to get a real closed port number) — adjust the test to obtain such a port dynamically:

```rust
async fn closed_port_url() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    format!("http://{addr}")
}
```

and use `closed_port_url().await` in place of the hardcoded `http://127.0.0.1:1` if the hardcoded version proves flaky.

- [ ] **Step 4: Commit**

```bash
git add crates/cheladb-auth/Cargo.toml crates/cheladb-auth/tests/verify.rs
git commit -m "test: cover JwtVerifier::verify against a mock JWKS server"
```

---

## Task 4: Extract orchestrator into a testable lib (traits + generic AppState)

**Files:**
- Modify: `apps/orchestrator/Cargo.toml`
- Create: `apps/orchestrator/src/lib.rs`
- Modify: `apps/orchestrator/src/main.rs`

**Interfaces:**
- Produces (all `pub`, in `orchestrator` lib crate):
  - `trait MetadataPort: Send + Sync + 'static` with methods `get_tenant_by_zitadel_org_id`, `insert_database_pending`, `set_failed`, `get_database` (signatures match `cheladb_metadata::MetadataClient`'s existing inherent methods).
  - `trait JobPort: Send + Sync + 'static` with method `publish_provision(&self, job: &JobEnvelope<ProvisionDatabaseJob>) -> Result<(), String>`.
  - `impl MetadataPort for cheladb_metadata::MetadataClient`
  - `impl JobPort for cheladb_nats::JetStreamContext`
  - `struct AppState<M, J> { metadata: M, js: J, verifier: JwtVerifier }`
  - `fn build_router<M: MetadataPort, J: JobPort>(state: std::sync::Arc<AppState<M, J>>) -> axum::Router`
  - `struct AuthPrincipal { sub: String, org_id: String, tenant_id: TenantId }` (fields `pub(crate)` is fine; tests only need the router's HTTP responses, not this struct directly)
  - `struct CreateDatabaseRequest { tenant_id: TenantId, project_id: ProjectId, name: String }` (`pub`, `Deserialize`)
  - `struct CreateDatabaseAccepted { job_id: Uuid, database_id: DatabaseId, status: DatabaseStatus }` (`pub`, `Serialize`)
- Consumes: nothing new; this task moves existing code, it doesn't add logic.

This task is a pure refactor: after it, `cargo run -p orchestrator` must behave identically to before. There is no new test in this task — Task 5 adds the tests that prove the refactor preserved behavior.

- [ ] **Step 1: Update `apps/orchestrator/Cargo.toml` to declare both a lib and a bin target**

Replace the `[[bin]]` section and add a `[lib]` section, and add `[dev-dependencies]`:

```toml
[package]
name = "orchestrator"
version.workspace = true
edition.workspace = true
license.workspace = true
description = "Cheladb control-plane orchestrator API"

[lib]
name = "orchestrator"
path = "src/lib.rs"

[[bin]]
name = "orchestrator"
path = "src/main.rs"

[dependencies]
cheladb-auth = { workspace = true }
cheladb-metadata = { workspace = true }
cheladb-nats = { workspace = true }
cheladb-types = { workspace = true }
axum = { workspace = true }
tokio = { workspace = true }
tower-http = { workspace = true }
tracing = { workspace = true }
tracing-subscriber = { workspace = true }
serde = { workspace = true }
serde_json = { workspace = true }
uuid = { workspace = true }
dotenvy = { workspace = true }
anyhow = { workspace = true }

[dev-dependencies]
cheladb-auth = { workspace = true, features = ["test-support"] }
reqwest = { workspace = true }
tokio = { workspace = true, features = ["full", "test-util"] }
```

- [ ] **Step 2: Create `apps/orchestrator/src/lib.rs`**

This is the entire contents of today's `apps/orchestrator/src/main.rs` (lines 1–334, i.e. everything up to but not including the `#[tokio::main] async fn main()` function and the trailing `health`/`create_database`/`get_database`/`map_metadata_error` functions — all of those move here too), restructured with the two new traits and generics:

```rust
use axum::{
    extract::{FromRequestParts, Path, State},
    http::{header::AUTHORIZATION, request::Parts, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use cheladb_auth::{AuthError, JwtVerifier};
use cheladb_metadata::{
    DatabaseRecord, DatabaseStatus, MetadataClient, MetadataError,
};
use cheladb_nats::{JobEnvelope, JetStreamContext, ProvisionDatabaseJob};
use cheladb_types::{DatabaseId, ProjectId, TenantId};
use serde::{Deserialize, Serialize};
use std::future::Future;
use std::sync::Arc;
use tower_http::trace::TraceLayer;
use tracing::{error, info};
use uuid::Uuid;

/// Abstraction over tenant/database metadata lookups, so tests can inject an
/// in-memory fake instead of a real Postgres-backed `MetadataClient`.
pub trait MetadataPort: Send + Sync + 'static {
    fn get_tenant_by_zitadel_org_id(
        &self,
        zitadel_org_id: &str,
    ) -> impl Future<Output = Result<Option<TenantId>, MetadataError>> + Send;

    fn insert_database_pending(
        &self,
        database_id: DatabaseId,
        tenant_id: TenantId,
        project_id: ProjectId,
        name: &str,
    ) -> impl Future<Output = Result<(), MetadataError>> + Send;

    fn set_failed(
        &self,
        database_id: DatabaseId,
        error: &str,
    ) -> impl Future<Output = Result<(), MetadataError>> + Send;

    fn get_database(
        &self,
        database_id: DatabaseId,
    ) -> impl Future<Output = Result<Option<DatabaseRecord>, MetadataError>> + Send;
}

impl MetadataPort for MetadataClient {
    async fn get_tenant_by_zitadel_org_id(
        &self,
        zitadel_org_id: &str,
    ) -> Result<Option<TenantId>, MetadataError> {
        MetadataClient::get_tenant_by_zitadel_org_id(self, zitadel_org_id).await
    }

    async fn insert_database_pending(
        &self,
        database_id: DatabaseId,
        tenant_id: TenantId,
        project_id: ProjectId,
        name: &str,
    ) -> Result<(), MetadataError> {
        MetadataClient::insert_database_pending(self, database_id, tenant_id, project_id, name)
            .await
    }

    async fn set_failed(&self, database_id: DatabaseId, error: &str) -> Result<(), MetadataError> {
        MetadataClient::set_failed(self, database_id, error).await
    }

    async fn get_database(
        &self,
        database_id: DatabaseId,
    ) -> Result<Option<DatabaseRecord>, MetadataError> {
        MetadataClient::get_database(self, database_id).await
    }
}

/// Abstraction over enqueuing a provision job, so tests can inject an
/// in-memory fake instead of a real NATS JetStream context.
pub trait JobPort: Send + Sync + 'static {
    fn publish_provision(
        &self,
        job: &JobEnvelope<ProvisionDatabaseJob>,
    ) -> impl Future<Output = Result<(), String>> + Send;
}

impl JobPort for JetStreamContext {
    async fn publish_provision(&self, job: &JobEnvelope<ProvisionDatabaseJob>) -> Result<(), String> {
        cheladb_nats::publish_provision(self, job)
            .await
            .map_err(|e| e.to_string())
    }
}

pub struct AppState<M, J> {
    pub metadata: M,
    pub js: J,
    pub verifier: JwtVerifier,
}

/// Authenticated caller after JWKS verify + org→tenant soft-bind resolution.
#[derive(Debug, Clone)]
struct AuthPrincipal {
    #[allow(dead_code)]
    sub: String,
    #[allow(dead_code)]
    org_id: String,
    tenant_id: TenantId,
}

impl<M, J> FromRequestParts<Arc<AppState<M, J>>> for AuthPrincipal
where
    M: MetadataPort,
    J: JobPort,
{
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &Arc<AppState<M, J>>,
    ) -> Result<Self, Self::Rejection> {
        let header = parts
            .headers
            .get(AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| auth_error_response(AuthError::Unauthorized))?;

        let token = header
            .strip_prefix("Bearer ")
            .or_else(|| header.strip_prefix("bearer "))
            .ok_or_else(|| auth_error_response(AuthError::Unauthorized))?;

        let claims = state
            .verifier
            .verify(token)
            .await
            .map_err(auth_error_response)?;

        let tenant_id = state
            .metadata
            .get_tenant_by_zitadel_org_id(&claims.org_id)
            .await
            .map_err(|err| {
                error!("tenant lookup by zitadel org: {err}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({ "error": "internal error" })),
                )
                    .into_response()
            })?
            .ok_or_else(|| {
                (
                    StatusCode::FORBIDDEN,
                    Json(serde_json::json!({
                        "error": "no Cheladb tenant bound to this Zitadel org; run just zitadel-bootstrap"
                    })),
                )
                    .into_response()
            })?;

        Ok(AuthPrincipal {
            sub: claims.sub,
            org_id: claims.org_id,
            tenant_id,
        })
    }
}

fn auth_error_response(err: AuthError) -> Response {
    match err {
        AuthError::Unauthorized | AuthError::InvalidToken(_) => (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({ "error": err.to_string() })),
        )
            .into_response(),
        AuthError::Jwks(_) | AuthError::Config(_) => {
            error!("auth infrastructure error: {err}");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({ "error": "auth unavailable" })),
            )
                .into_response()
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct CreateDatabaseRequest {
    pub tenant_id: TenantId,
    pub project_id: ProjectId,
    pub name: String,
}

#[derive(Debug, Serialize)]
pub struct CreateDatabaseAccepted {
    pub job_id: Uuid,
    pub database_id: DatabaseId,
    pub status: DatabaseStatus,
}

#[derive(Debug, Serialize)]
struct DatabaseResponse {
    database_id: DatabaseId,
    project_id: ProjectId,
    tenant_id: TenantId,
    name: String,
    status: DatabaseStatus,
    host: Option<String>,
    port: Option<u16>,
    /// Dev-only; present when ready for local poking.
    admin_password: Option<String>,
    error: Option<String>,
}

impl From<DatabaseRecord> for DatabaseResponse {
    fn from(r: DatabaseRecord) -> Self {
        Self {
            database_id: r.database_id,
            project_id: r.project_id,
            tenant_id: r.tenant_id,
            name: r.name,
            status: r.status,
            host: r.host,
            port: r.port,
            admin_password: r.admin_password,
            error: r.error,
        }
    }
}

/// Build the orchestrator's axum router over the given (possibly-fake) state.
pub fn build_router<M, J>(state: Arc<AppState<M, J>>) -> Router
where
    M: MetadataPort,
    J: JobPort,
{
    Router::new()
        .route("/health", get(health))
        .route("/v1/databases", post(create_database::<M, J>))
        .route("/v1/databases/{id}", get(get_database::<M, J>))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

async fn health() -> impl IntoResponse {
    Json(serde_json::json!({ "status": "ok", "service": "orchestrator" }))
}

async fn create_database<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    Json(body): Json<CreateDatabaseRequest>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    if body.tenant_id != principal.tenant_id {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({
                "error": "tenant_id does not match authenticated Zitadel org binding"
            })),
        )
            .into_response();
    }

    if body.name.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "name is required" })),
        )
            .into_response();
    }

    let job_id = Uuid::new_v4();
    let database_id = DatabaseId::new();

    if let Err(err) = state
        .metadata
        .insert_database_pending(database_id, body.tenant_id, body.project_id, &body.name)
        .await
    {
        return map_metadata_error(err);
    }

    let job = JobEnvelope {
        job_id,
        payload: ProvisionDatabaseJob {
            database_id,
            project_id: body.project_id,
            tenant_id: body.tenant_id,
            name: body.name.clone(),
        },
    };

    if let Err(err) = state.js.publish_provision(&job).await {
        error!("publish failed: {err}");
        let _ = state
            .metadata
            .set_failed(database_id, &format!("publish failed: {err}"))
            .await;
        return (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({ "error": "failed to enqueue provision job" })),
        )
            .into_response();
    }

    info!(%job_id, %database_id, name = %body.name, "accepted create database");

    (
        StatusCode::ACCEPTED,
        Json(CreateDatabaseAccepted {
            job_id,
            database_id,
            status: DatabaseStatus::Pending,
        }),
    )
        .into_response()
}

async fn get_database<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    Path(id): Path<Uuid>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    let database_id = DatabaseId(id);
    match state.metadata.get_database(database_id).await {
        Ok(Some(record)) => {
            if record.tenant_id != principal.tenant_id {
                return (
                    StatusCode::FORBIDDEN,
                    Json(serde_json::json!({ "error": "forbidden" })),
                )
                    .into_response();
            }
            (StatusCode::OK, Json(DatabaseResponse::from(record))).into_response()
        }
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "database not found" })),
        )
            .into_response(),
        Err(err) => {
            error!("get_database: {err}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "internal error" })),
            )
                .into_response()
        }
    }
}

fn map_metadata_error(err: MetadataError) -> Response {
    match err {
        MetadataError::TenantNotFound(_) | MetadataError::ProjectNotFound(_) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": err.to_string() })),
        )
            .into_response(),
        MetadataError::ProjectTenantMismatch { .. } => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": err.to_string() })),
        )
            .into_response(),
        MetadataError::DuplicateDatabaseName => (
            StatusCode::CONFLICT,
            Json(serde_json::json!({ "error": "database name already exists in project" })),
        )
            .into_response(),
        other => {
            error!("metadata error: {other}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "internal error" })),
            )
                .into_response()
        }
    }
}
```

- [ ] **Step 3: Replace `apps/orchestrator/src/main.rs`**

```rust
use cheladb_auth::JwtVerifier;
use cheladb_metadata::{MetadataClient, MetadataConfig};
use cheladb_nats::{connect, ensure_control_stream, NatsConfig};
use orchestrator::{build_router, AppState};
use std::net::SocketAddr;
use std::sync::Arc;
use tracing::info;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "orchestrator=info,tower_http=info,cheladb_auth=info".into()),
        )
        .init();

    let metadata_config = MetadataConfig::from_env().unwrap_or_else(|_| MetadataConfig {
        database_url: "postgres://cheladb:cheladb@127.0.0.1:5433/cheladb_metadata".into(),
    });
    let nats_config = NatsConfig::from_env().expect("nats config");
    let verifier = JwtVerifier::from_env().expect("zitadel auth config");

    let metadata = MetadataClient::connect(&metadata_config).await?;
    metadata.migrate().await?;

    let nats = connect(&nats_config).await?;
    let js = ensure_control_stream(&nats).await?;

    let state = AppState {
        metadata,
        js,
        verifier,
    };

    let app = build_router(Arc::new(state));

    let addr: SocketAddr = std::env::var("CHELADB_ORCHESTRATOR_ADDR")
        .unwrap_or_else(|_| "127.0.0.1:8081".into())
        .parse()?;
    info!("orchestrator listening on {addr}");
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}
```

- [ ] **Step 4: Verify the refactor compiles and preserves behavior**

Run: `cargo build -p orchestrator`
Expected: builds successfully (both `lib` and `bin` targets).

Run: `cargo clippy -p orchestrator -- -D warnings`
Expected: no warnings (fix any that surface from the refactor, e.g. unused imports left behind in `main.rs`).

- [ ] **Step 5: Commit**

```bash
git add apps/orchestrator/Cargo.toml apps/orchestrator/src/lib.rs apps/orchestrator/src/main.rs
git commit -m "refactor: extract orchestrator into a testable lib with MetadataPort/JobPort traits"
```

---

## Task 5: Orchestrator integration tests — 401 / 403 / 202 (and soft-bind edge cases)

**Files:**
- Create: `apps/orchestrator/tests/api.rs`

**Interfaces:**
- Consumes: `orchestrator::{AppState, MetadataPort, JobPort, build_router, CreateDatabaseAccepted}`, `cheladb_auth::{JwtVerifier, test_support::MockJwks}`, `cheladb_metadata::{DatabaseRecord, DatabaseStatus, MetadataError}`, `cheladb_nats::{JobEnvelope, ProvisionDatabaseJob}`, `cheladb_types::{DatabaseId, ProjectId, TenantId}`.

- [ ] **Step 1: Write the test file**

```rust
use axum::Router;
use cheladb_auth::test_support::MockJwks;
use cheladb_auth::JwtVerifier;
use cheladb_metadata::{DatabaseRecord, DatabaseStatus, MetadataError};
use cheladb_nats::{JobEnvelope, ProvisionDatabaseJob};
use cheladb_types::{DatabaseId, ProjectId, TenantId};
use orchestrator::{build_router, AppState, JobPort, MetadataPort};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::net::TcpListener;

#[derive(Clone, Default)]
struct FakeMetadata {
    tenants_by_org: Arc<Mutex<HashMap<String, TenantId>>>,
    fail_insert: Arc<Mutex<bool>>,
}

impl FakeMetadata {
    fn bind_org(&self, org_id: &str, tenant_id: TenantId) {
        self.tenants_by_org
            .lock()
            .unwrap()
            .insert(org_id.to_string(), tenant_id);
    }
}

impl MetadataPort for FakeMetadata {
    async fn get_tenant_by_zitadel_org_id(
        &self,
        zitadel_org_id: &str,
    ) -> Result<Option<TenantId>, MetadataError> {
        Ok(self.tenants_by_org.lock().unwrap().get(zitadel_org_id).copied())
    }

    async fn insert_database_pending(
        &self,
        _database_id: DatabaseId,
        _tenant_id: TenantId,
        _project_id: ProjectId,
        _name: &str,
    ) -> Result<(), MetadataError> {
        if *self.fail_insert.lock().unwrap() {
            return Err(MetadataError::DuplicateDatabaseName);
        }
        Ok(())
    }

    async fn set_failed(&self, _database_id: DatabaseId, _error: &str) -> Result<(), MetadataError> {
        Ok(())
    }

    async fn get_database(
        &self,
        _database_id: DatabaseId,
    ) -> Result<Option<DatabaseRecord>, MetadataError> {
        Ok(None)
    }
}

#[derive(Clone, Default)]
struct FakeJobs {
    published: Arc<Mutex<Vec<JobEnvelope<ProvisionDatabaseJob>>>>,
}

impl JobPort for FakeJobs {
    async fn publish_provision(&self, job: &JobEnvelope<ProvisionDatabaseJob>) -> Result<(), String> {
        self.published.lock().unwrap().push(job.clone());
        Ok(())
    }
}

/// Boots the real orchestrator router (fake metadata/jobs, mock JWKS) on an
/// ephemeral port and returns its base URL plus the mock JWKS handle for signing.
async fn spawn_app(metadata: FakeMetadata, jobs: FakeJobs) -> (String, MockJwks) {
    let mock = MockJwks::start().await;
    let verifier = JwtVerifier::new(mock.config(vec![]));
    let state = Arc::new(AppState {
        metadata,
        js: jobs,
        verifier,
    });
    let app: Router = build_router(state);

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    (format!("http://{addr}"), mock)
}

#[tokio::test]
async fn health_is_public() {
    let (base, _mock) = spawn_app(FakeMetadata::default(), FakeJobs::default()).await;
    let resp = reqwest::get(format!("{base}/health")).await.unwrap();
    assert_eq!(resp.status(), 200);
}

#[tokio::test]
async fn missing_bearer_token_is_401() {
    let (base, _mock) = spawn_app(FakeMetadata::default(), FakeJobs::default()).await;
    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/databases"))
        .json(&serde_json::json!({
            "tenant_id": TenantId::new(),
            "project_id": ProjectId::new(),
            "name": "demo",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
}

#[tokio::test]
async fn invalid_bearer_token_is_401() {
    let (base, _mock) = spawn_app(FakeMetadata::default(), FakeJobs::default()).await;
    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/databases"))
        .bearer_auth("not-a-real-jwt")
        .json(&serde_json::json!({
            "tenant_id": TenantId::new(),
            "project_id": ProjectId::new(),
            "name": "demo",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
}

#[tokio::test]
async fn expired_bearer_token_is_401() {
    let metadata = FakeMetadata::default();
    let (base, mock) = spawn_app(metadata.clone(), FakeJobs::default()).await;
    let token = mock.sign(serde_json::json!({
        "sub": "user-1",
        "iss": mock.issuer(),
        "exp": 1,
        cheladb_auth::CLAIM_ORG_ID: "org-1",
    }));

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/databases"))
        .bearer_auth(token)
        .json(&serde_json::json!({
            "tenant_id": TenantId::new(),
            "project_id": ProjectId::new(),
            "name": "demo",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
}

#[tokio::test]
async fn unbound_org_is_403_with_bootstrap_hint() {
    let (base, mock) = spawn_app(FakeMetadata::default(), FakeJobs::default()).await;
    let token = mock.token_for_org("user-1", "org-not-bound", 300);

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/databases"))
        .bearer_auth(token)
        .json(&serde_json::json!({
            "tenant_id": TenantId::new(),
            "project_id": ProjectId::new(),
            "name": "demo",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 403);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(body["error"].as_str().unwrap().contains("zitadel-bootstrap"));
}

#[tokio::test]
async fn body_tenant_mismatch_is_403() {
    let metadata = FakeMetadata::default();
    let bound_tenant = TenantId::new();
    metadata.bind_org("org-1", bound_tenant);
    let (base, mock) = spawn_app(metadata, FakeJobs::default()).await;
    let token = mock.token_for_org("user-1", "org-1", 300);

    let other_tenant = TenantId::new();
    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/databases"))
        .bearer_auth(token)
        .json(&serde_json::json!({
            "tenant_id": other_tenant,
            "project_id": ProjectId::new(),
            "name": "demo",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 403);
}

#[tokio::test]
async fn valid_request_is_202_accepted_and_publishes_job() {
    let metadata = FakeMetadata::default();
    let bound_tenant = TenantId::new();
    metadata.bind_org("org-1", bound_tenant);
    let jobs = FakeJobs::default();
    let (base, mock) = spawn_app(metadata, jobs.clone()).await;
    let token = mock.token_for_org("user-1", "org-1", 300);

    let project_id = ProjectId::new();
    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/databases"))
        .bearer_auth(token)
        .json(&serde_json::json!({
            "tenant_id": bound_tenant,
            "project_id": project_id,
            "name": "demo-db",
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 202);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["status"], "pending");
    assert!(body["job_id"].is_string());
    assert!(body["database_id"].is_string());

    let published = jobs.published.lock().unwrap();
    assert_eq!(published.len(), 1);
    assert_eq!(published[0].payload.tenant_id, bound_tenant);
    assert_eq!(published[0].payload.project_id, project_id);
    assert_eq!(published[0].payload.name, "demo-db");
}

#[tokio::test]
async fn empty_name_is_400() {
    let metadata = FakeMetadata::default();
    let bound_tenant = TenantId::new();
    metadata.bind_org("org-1", bound_tenant);
    let (base, mock) = spawn_app(metadata, FakeJobs::default()).await;
    let token = mock.token_for_org("user-1", "org-1", 300);

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/databases"))
        .bearer_auth(token)
        .json(&serde_json::json!({
            "tenant_id": bound_tenant,
            "project_id": ProjectId::new(),
            "name": "   ",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
}
```

Note: `JobEnvelope<ProvisionDatabaseJob>` needs `Clone` for `FakeJobs::published` to store copies — check `cheladb_nats::JobEnvelope` derives `Clone` (it does, per `crates/cheladb-nats/src/lib.rs:22`), so no change needed there.

- [ ] **Step 2: Run the tests**

Run: `cargo test -p orchestrator --test api`
Expected: all tests PASS, no attempt to connect to Postgres, NATS, or a real Zitadel — confirm by running with no local services up (`docker ps` showing nothing related, or simply trusting that `FakeMetadata`/`FakeJobs` never perform I/O beyond the in-process mutex).

- [ ] **Step 3: Run the full focused suite together**

Run: `cargo test -p cheladb-auth -p orchestrator --features cheladb-auth/test-support`
Expected: all tests across both crates PASS.

Run: `cargo test --workspace` (this will also attempt to compile/run `cheladb-types`, `cheladb-metadata`, `cheladb-nats`, `apps/gateway`, `apps/worker`, `apps/cheladb` — none of these have tests requiring live infra today, so this should pass or no-op cleanly; if `cheladb-metadata`, `worker`, or `gateway` fail to even *compile* tests due to requiring env vars at test time, that's a pre-existing condition unrelated to this change — verify by checking `git status`/`git stash` behavior isn't implicated, i.e. confirm the failure also exists on `main` before this branch's changes).
Expected: PASS (or pre-existing, unrelated failures only).

- [ ] **Step 4: Commit**

```bash
git add apps/orchestrator/tests/api.rs
git commit -m "test: cover orchestrator 401/403/202 and soft-bind via fake metadata/jobs + mock JWKS"
```

---

## Task 6: Final full-suite verification

**Files:** none (verification only)

- [ ] **Step 1: Run the complete focused test surface one more time from a clean build**

```bash
cargo test -p cheladb-auth --features test-support
cargo test -p orchestrator
```

Expected: all PASS, total run time low (seconds, not minutes) since nothing touches the network beyond localhost loopback and nothing waits on external services.

- [ ] **Step 2: Confirm no live-infra dependency was introduced**

```bash
grep -rn "5433\|4222\|localhost:8080\|127.0.0.1:8080" apps/orchestrator/tests crates/cheladb-auth/tests crates/cheladb-auth/src/lib.rs
```

Expected: no matches (those are the real Postgres/NATS/Zitadel ports from `.env`/defaults; test code must never reference them — everything should bind to `127.0.0.1:0` and read the resulting ephemeral address back).

- [ ] **Step 3: Confirm `apps/gateway` is untouched**

```bash
git diff --stat main -- apps/gateway
```

Expected: empty output.

- [ ] **Step 4: Commit (if Step 1/2/3 required any fixes)**

Only commit if changes were needed to satisfy Steps 1–3; otherwise this task is verification-only and requires no commit.
