//! RT1 T1, user decision "storage auth": the pageserver and safekeeper require NeonJWT on libpq
//! and HTTP. Tokens are signed with the existing compute-JWT key pair (Ed25519, never
//! regenerated) and verified by the Neon binaries against its public key as SPKI PEM.
//!
//! What Neon checks (the fork at infra/neon/NEON_REF):
//! - `libs/utils/src/auth.rs`: EdDSA only; `Claims { tenant_id: Option<TenantId>, scope }`,
//!   scope names lowercase (`tenant`, `pageserverapi`, `safekeeperdata`, `admin`, `infra`) and
//!   `generations_api`; no claim is required (no `exp`); keys are read with
//!   `DecodingKey::from_ed_pem`.
//! - `pageserver/src/auth.rs`, `safekeeper/src/auth.rs`: a `tenant` token opens only its own
//!   tenant (and `unwrap`s `tenant_id`, so a tenant token must always carry one); the pageserver
//!   takes `pageserverapi`, the safekeeper `safekeeperdata`.
//! - `storage_controller/src/http.rs`: `admin` opens every route; pageserver upcalls use
//!   `generations_api`.
//! - compute_ctl (`compute_tools/src/http/middleware/authorize.rs`) verifies the same key's JWKS:
//!   it must never accept a storage token (no `compute_id`, no `aud`), and storage services must
//!   never accept a compute_ctl token (no `scope`).
//! - `libs/compute_api/src/spec.rs:153`: the compute gets its token as `spec.storage_auth_token`
//!   (compute_ctl passes it as the libpq password and `NEON_AUTH_TOKEN`).

use axum::Router;
use axum::extract::{Request, State};
use axum::response::{IntoResponse, Response};
use chelabase_neon::jwt::{PUBLIC_PEM_FILE, StorageScope, generate_keypair};
use chelabase_neon::safekeeper::SafekeeperClient;
use chelabase_neon::spec::{
    ComputeMode, DatabaseSpec, Endpoint, MemberLogins, Placement, RoleSpec, SpecInputs,
    config_response, endpoint_config,
};
use chelabase_neon::{ComputeSigner, NeonId, StorconClient, StorconError};
use jsonwebtoken::{Algorithm, DecodingKey, Validation};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use uuid::Uuid;

fn signer() -> ComputeSigner {
    let (pem, x) = generate_keypair();
    ComputeSigner::new(&pem, &x).unwrap()
}

/// Decodes the way the pageserver, safekeeper and storage controller do (`JwtAuth::new`).
fn neon_decode(signer: &ComputeSigner, token: &str) -> Value {
    let key = DecodingKey::from_ed_pem(signer.public_key_pem().as_bytes())
        .expect("an SPKI PEM Ed25519 key");
    let mut validation = Validation::new(Algorithm::EdDSA);
    validation.required_spec_claims = Default::default();
    jsonwebtoken::decode::<Value>(token, &key, &validation)
        .expect("Neon accepts the token")
        .claims
}

fn tenant() -> NeonId {
    NeonId::from(Uuid::from_u128(0xabc))
}

#[test]
fn the_public_key_pem_is_spki_for_the_same_key() {
    let s = signer();
    let pem = s.public_key_pem();
    assert!(pem.starts_with("-----BEGIN PUBLIC KEY-----\n"), "{pem}");
    assert!(
        pem.trim_end().ends_with("-----END PUBLIC KEY-----"),
        "{pem}"
    );
    // A token from this signer verifies with it; one from another key pair doesn't.
    let token = s.storage_token(&StorageScope::Admin).unwrap();
    neon_decode(&s, &token);
    let other = signer();
    let key = DecodingKey::from_ed_pem(other.public_key_pem().as_bytes()).unwrap();
    let mut v = Validation::new(Algorithm::EdDSA);
    v.required_spec_claims = Default::default();
    assert!(jsonwebtoken::decode::<Value>(&token, &key, &v).is_err());
    assert_eq!(PUBLIC_PEM_FILE, "compute-jwt.pub.pem");
}

#[test]
fn each_scope_has_neons_claim_shape() {
    let s = signer();
    let t = tenant();
    let cases = [
        (
            StorageScope::Tenant(t.clone()),
            json!({"tenant_id": t.to_string(), "scope": "tenant"}),
        ),
        (
            StorageScope::PageServerApi,
            json!({"scope": "pageserverapi"}),
        ),
        (
            StorageScope::SafekeeperData,
            json!({"scope": "safekeeperdata"}),
        ),
        (
            StorageScope::GenerationsApi,
            json!({"scope": "generations_api"}),
        ),
        (StorageScope::Admin, json!({"scope": "admin"})),
        (StorageScope::Infra, json!({"scope": "infra"})),
    ];
    for (scope, want) in cases {
        let claims = neon_decode(&s, &s.storage_token(&scope).unwrap());
        assert_eq!(claims["scope"], want["scope"], "{claims}");
        assert_eq!(
            claims["tenant_id"],
            want.get("tenant_id").cloned().unwrap_or(Value::Null),
            "{claims}"
        );
        // Neon requires no exp, and a compute holds its token for its whole life (it is the
        // libpq password on every reconnect): none is set.
        assert!(claims.get("exp").is_none(), "{claims}");
        // compute_ctl must never take a storage token (it needs `compute_id`, or `aud` for admin).
        assert!(
            claims.get("compute_id").is_none() && claims.get("aud").is_none(),
            "{claims}"
        );
    }
    assert_eq!(
        t.to_string().len(),
        32,
        "Neon's TenantId is 32 hex characters"
    );
}

// A compute_ctl token has no `scope`, so Neon's `Claims` (scope required) refuses it.
#[test]
fn a_compute_ctl_token_is_no_storage_token() {
    let s = signer();
    let claims = neon_decode(&s, &s.token_for("ep-a").unwrap());
    assert!(claims.get("scope").is_none(), "{claims}");
}

// --- the compute spec carries its tenant's token -------------------------------------------

struct FakePlacement;

impl Placement for FakePlacement {
    async fn pageserver_connstring(&self, _tenant: &NeonId) -> Result<String, StorconError> {
        Ok("postgresql://no_user@pageserver:6400".into())
    }
    async fn active_safekeepers(&self) -> Result<Vec<String>, StorconError> {
        Ok(vec!["safekeeper1:5454".into()])
    }
}

const TIMELINE: &str = "00000000000000000000000000000002";

fn logins() -> MemberLogins {
    MemberLogins::orchestrator(vec!["10.213.0.10/32".into()])
}

#[test]
fn config_response_serves_the_storage_auth_token() {
    let t = tenant();
    let timeline: NeonId = TIMELINE.parse().unwrap();
    let v = config_response(&SpecInputs {
        endpoint_id: "ep-a",
        tenant_id: &t,
        timeline_id: &timeline,
        pageserver_connstring: "postgresql://no_user@pageserver:6400".into(),
        safekeeper_connstrings: vec!["safekeeper1:5454".into()],
        roles: vec![],
        databases: vec![],
        jwks: json!({"keys": []}),
        console_url: None,
        mode: ComputeMode::Primary,
        member_logins: logins(),
        branch_protected: false,
        storage_auth_token: "tok-for-ep-a".into(),
    });
    assert_eq!(v["spec"]["storage_auth_token"], "tok-for-ep-a");
}

#[tokio::test]
async fn each_compute_gets_a_token_for_its_own_tenant_only() {
    let s = signer();
    let mut seen = Vec::new();
    for n in [1u128, 2] {
        let tenant_hex = NeonId::from(Uuid::from_u128(0x1000 + n)).to_string();
        let v = endpoint_config(
            &FakePlacement,
            Endpoint {
                id: "ep-a",
                tenant_id: &tenant_hex,
                timeline_id: TIMELINE,
                mode: ComputeMode::Primary,
            },
            vec![RoleSpec {
                name: "app".into(),
                scram_verifier: "SCRAM-SHA-256$x".into(),
            }],
            vec![DatabaseSpec {
                name: "app".into(),
                owner: "app".into(),
            }],
            s.jwks(),
            None,
            &logins(),
            false,
            &s,
        )
        .await
        .unwrap();
        let token = v["spec"]["storage_auth_token"]
            .as_str()
            .expect("the spec carries a storage token");
        let claims = neon_decode(&s, token);
        assert_eq!(claims["scope"], "tenant", "{claims}");
        assert_eq!(claims["tenant_id"], tenant_hex.as_str(), "{claims}");
        seen.push(token.to_string());
    }
    assert_ne!(seen[0], seen[1]);
}

// --- the control plane's clients present their tokens --------------------------------------

async fn recording_server() -> (String, Arc<Mutex<Vec<Option<String>>>>) {
    let seen: Arc<Mutex<Vec<Option<String>>>> = Arc::default();
    async fn answer(State(seen): State<Arc<Mutex<Vec<Option<String>>>>>, req: Request) -> Response {
        let auth = req
            .headers()
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        seen.lock().unwrap().push(auth);
        let body = if req.uri().path() == "/control/v1/safekeeper" {
            r#"[{"id":1,"host":"safekeeper1","port":5454,"scheduling_policy":"Active"}]"#
        } else {
            r#"{"commit_lsn":"0/16B3748"}"#
        };
        ([("content-type", "application/json")], body).into_response()
    }
    let app = Router::new().fallback(answer).with_state(seen.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), seen)
}

#[tokio::test]
async fn the_storcon_client_sends_its_bearer() {
    let (base, seen) = recording_server().await;
    StorconClient::new(&base, 0)
        .with_token("storcon-admin-jwt".into())
        .active_safekeepers()
        .await
        .unwrap();
    assert_eq!(
        seen.lock().unwrap()[0].as_deref(),
        Some("Bearer storcon-admin-jwt")
    );
}

#[tokio::test]
async fn the_safekeeper_client_sends_its_bearer() {
    let (base, seen) = recording_server().await;
    SafekeeperClient::new(base.parse().unwrap())
        .with_token("sk-data-jwt".into())
        .commit_lsn(&tenant(), &TIMELINE.parse().unwrap())
        .await
        .unwrap();
    assert_eq!(
        seen.lock().unwrap()[0].as_deref(),
        Some("Bearer sk-data-jwt")
    );
}
