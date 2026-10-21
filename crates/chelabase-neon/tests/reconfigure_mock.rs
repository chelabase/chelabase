//! `reconfigure::push` against an in-process compute_ctl stand-in.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use chelabase_neon::ComputeSigner;
use chelabase_neon::jwt::generate_keypair;
use chelabase_neon::reconfigure::{configure_body, push};
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
                move |State(seen): State<Arc<Seen>>,
                      headers: HeaderMap,
                      body: axum::Json<Value>| async move {
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

async fn status_compute(body: Value) -> (SocketAddr, Arc<Seen>) {
    let seen = Arc::new(Seen::default());
    let app = Router::new()
        .route(
            "/status",
            axum::routing::get(move |State(seen): State<Arc<Seen>>, headers: HeaderMap| {
                let body = body.clone();
                async move {
                    *seen.auth.lock().unwrap() =
                        headers.get("authorization").map(|v| v.to_str().unwrap().to_string());
                    (StatusCode::OK, axum::Json(body))
                }
            }),
        )
        .with_state(seen.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (addr, seen)
}

#[tokio::test]
async fn status_reads_compute_ctl_with_its_token() {
    use chelabase_neon::reconfigure::{ComputeCtlStatus, status};
    let (pem, x) = generate_keypair();
    let signer = ComputeSigner::new(&pem, &x).unwrap();
    let base = json!({"start_time": "2026-10-15T00:00:00Z", "tenant": null, "timeline": null,
                      "last_active": null, "error": null});
    let with = |s: &str, e: Option<&str>| {
        let mut b = base.clone();
        b["status"] = json!(s);
        b["error"] = json!(e);
        b
    };
    for (body, want) in [
        (with("running", None), ComputeCtlStatus::Running),
        (with("failed", Some("migration 2 failed")), ComputeCtlStatus::Failed("migration 2 failed".into())),
        (with("init", None), ComputeCtlStatus::Other("init".into())),
        (with("configuration_pending", None), ComputeCtlStatus::Other("configuration_pending".into())),
    ] {
        let (addr, seen) = status_compute(body).await;
        let got = status(&reqwest::Client::new(), addr, &signer, "ep-a").await.unwrap();
        assert_eq!(got, want);
        assert!(seen.auth.lock().unwrap().clone().unwrap().starts_with("Bearer "));
    }
}

// --- round 4 (C1): capabilities in /status, POST /storage_token, the header reader ------------

use chelabase_neon::reconfigure::{
    CAPABILITIES_HEADER, ComputeCtlReport, ComputeCtlStatus, STORAGE_TOKEN_REFRESH,
    advertises_storage_token_refresh, push_storage_token, status_report,
};

fn signer() -> ComputeSigner {
    let (pem, x) = generate_keypair();
    ComputeSigner::new(&pem, &x).unwrap()
}

fn bearer_claims(signer: &ComputeSigner, auth: &str) -> Value {
    let token = auth.strip_prefix("Bearer ").expect("bearer token");
    let jwks: jsonwebtoken::jwk::JwkSet = serde_json::from_value(signer.jwks()).unwrap();
    let key = DecodingKey::from_jwk(&jwks.keys[0]).unwrap();
    jsonwebtoken::decode::<Value>(token, &key, &Validation::new(Algorithm::EdDSA))
        .unwrap()
        .claims
}

fn status_body(status: &str, capabilities: Option<Value>) -> Value {
    let mut body = json!({"start_time": "2026-10-21T00:00:00Z", "tenant": null, "timeline": null,
                          "last_active": null, "error": null, "status": status});
    if let Some(c) = capabilities {
        body["capabilities"] = c;
    }
    body
}

#[test]
fn the_capability_names_are_the_wire_names() {
    assert_eq!(CAPABILITIES_HEADER, "Compute-Capabilities");
    assert_eq!(STORAGE_TOKEN_REFRESH, "storage_token_refresh");
}

#[tokio::test]
async fn status_report_reads_the_status_and_the_refresh_capability() {
    let signer = signer();
    let running = ComputeCtlStatus::Running;
    for (capabilities, refresh) in [
        (Some(json!(["storage_token_refresh"])), true),
        (Some(json!(["something_new", "storage_token_refresh"])), true),
        (Some(json!([])), false),
        // An old compute_ctl sends no `capabilities` at all: empty.
        (None, false),
        (Some(json!(["something_new"])), false),
        // Exact, case-sensitive names (a near miss isn't the capability).
        (Some(json!(["Storage_Token_Refresh"])), false),
        (Some(json!(["storage_token_refresh2"])), false),
        (Some(json!(["storage_token"])), false),
        (Some(json!([" storage_token_refresh"])), false),
    ] {
        let (addr, seen) = status_compute(status_body("running", capabilities.clone())).await;
        let report = status_report(&reqwest::Client::new(), addr, &signer, "ep-a")
            .await
            .unwrap();
        assert_eq!(
            report,
            ComputeCtlReport { status: running.clone(), storage_token_refresh: refresh },
            "{capabilities:?}"
        );
        let auth = seen.auth.lock().unwrap().clone().unwrap();
        assert_eq!(bearer_claims(&signer, &auth)["compute_id"], "ep-a");
    }
}

#[tokio::test]
async fn status_report_keeps_every_status_mapping() {
    let signer = signer();
    for (body, want) in [
        (
            status_body("failed", None).as_object().cloned().map(|mut b| {
                b.insert("error".into(), json!("migration 2 failed"));
                Value::Object(b)
            }).unwrap(),
            ComputeCtlStatus::Failed("migration 2 failed".into()),
        ),
        (status_body("init", None), ComputeCtlStatus::Other("init".into())),
        (
            status_body("configuration_pending", Some(json!(["storage_token_refresh"]))),
            ComputeCtlStatus::Other("configuration_pending".into()),
        ),
    ] {
        let (addr, _) = status_compute(body).await;
        let report = status_report(&reqwest::Client::new(), addr, &signer, "ep-a").await.unwrap();
        assert_eq!(report.status, want);
    }
}

#[tokio::test]
async fn status_still_works_on_a_body_with_capabilities() {
    // The old `status()` keeps its callers (P11): it ignores the new field.
    let (addr, _) = status_compute(status_body("running", Some(json!(["storage_token_refresh"])))).await;
    let got = chelabase_neon::reconfigure::status(&reqwest::Client::new(), addr, &signer(), "ep-a")
        .await
        .unwrap();
    assert_eq!(got, ComputeCtlStatus::Running);
}

#[tokio::test]
async fn status_report_errors_carry_the_status_and_compute_ctls_body() {
    let signer = signer();
    let app = Router::new().route(
        "/status",
        axum::routing::get(|| async { (StatusCode::UNAUTHORIZED, "no good token here") }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let err = status_report(&reqwest::Client::new(), addr, &signer, "ep-a")
        .await
        .unwrap_err();
    let text = format!("{err:#}");
    assert!(text.contains("401") && text.contains("no good token here"), "{text}");

    // An unreachable compute_ctl and a body that isn't a status are errors too, never a
    // silent "no capability" (SP2).
    let dead = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap()
    };
    assert!(status_report(&reqwest::Client::new(), dead, &signer, "ep-a").await.is_err());
    let (addr, _) = status_compute(json!({"nonsense": true})).await;
    assert!(status_report(&reqwest::Client::new(), addr, &signer, "ep-a").await.is_err());
}

struct Refresh {
    auth: Option<String>,
    body: Value,
}

async fn storage_token_compute(status: u16, answer: &'static str) -> (SocketAddr, Arc<Mutex<Vec<Refresh>>>) {
    let seen: Arc<Mutex<Vec<Refresh>>> = Arc::default();
    let app = Router::new()
        .route(
            "/storage_token",
            post(
                move |State(seen): State<Arc<Mutex<Vec<Refresh>>>>,
                      headers: HeaderMap,
                      body: axum::Json<Value>| async move {
                    seen.lock().unwrap().push(Refresh {
                        auth: headers
                            .get("authorization")
                            .map(|v| v.to_str().unwrap().to_string()),
                        body: body.0,
                    });
                    (StatusCode::from_u16(status).unwrap(), answer)
                },
            ),
        )
        .with_state(seen.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (addr, seen)
}

#[tokio::test]
async fn push_storage_token_posts_the_token_with_the_computes_own_bearer() {
    let signer = signer();
    let (addr, seen) = storage_token_compute(204, "").await;
    push_storage_token(&reqwest::Client::new(), addr, &signer, "ep-a", "the.new.jwt")
        .await
        .unwrap();
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].body, json!({"token": "the.new.jwt"}), "exactly the pinned body");
    assert_eq!(bearer_claims(&signer, seen[0].auth.as_deref().unwrap())["compute_id"], "ep-a");
}

#[tokio::test]
async fn push_storage_token_accepts_any_2xx() {
    for code in [200, 204] {
        let (addr, _) = storage_token_compute(code, "").await;
        push_storage_token(&reqwest::Client::new(), addr, &signer(), "ep-a", "t")
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn push_storage_token_non_2xx_is_an_error_with_status_and_body_but_never_the_token() {
    for (code, answer) in [
        (400u16, r#"{"error":"the storage token is empty"}"#),
        (401, r#"{"error":"bad bearer"}"#),
        (412, "compute is not running"),
        (500, "disk full"),
    ] {
        let (addr, _) = storage_token_compute(code, answer).await;
        let err = push_storage_token(&reqwest::Client::new(), addr, &signer(), "ep-a", "SECRET.JWT.VALUE")
            .await
            .unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains(&code.to_string()), "{text}");
        assert!(text.contains(answer), "{text}");
        assert!(text.contains("ep-a"), "{text}");
        assert!(!text.contains("SECRET.JWT.VALUE"), "the token never reaches an error: {text}");
    }
}

#[tokio::test]
async fn push_storage_token_to_nobody_is_an_error() {
    let dead = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap()
    };
    let err = push_storage_token(&reqwest::Client::new(), dead, &signer(), "ep-a", "SECRET.JWT.VALUE")
        .await
        .unwrap_err();
    assert!(!format!("{err:#}").contains("SECRET.JWT.VALUE"));
}

#[test]
fn the_capabilities_header_reader_is_exact() {
    // SP23: the joined values of every `Compute-Capabilities` header; split on `,`, trimmed,
    // compared exactly (case-sensitive).
    for (raw, want) in [
        (None, false),
        (Some(""), false),
        (Some("  "), false),
        (Some(","), false),
        (Some("storage_token_refresh"), true),
        (Some(" storage_token_refresh , x"), true),
        (Some("x,storage_token_refresh"), true),
        (Some("x, storage_token_refresh ,y"), true),
        (Some("\tstorage_token_refresh\t"), true),
        (Some("storage_token_refresh,storage_token_refresh"), true),
        // Near misses.
        (Some("x"), false),
        (Some("storage_token"), false),
        (Some("storage_token_refresh2"), false),
        (Some("xstorage_token_refresh"), false),
        (Some("storage_token_refresh_"), false),
        (Some("storage-token-refresh"), false),
        (Some("storage_token_refresh x"), false),
        (Some("storage_token_refresh;x"), false),
        // Case-sensitive.
        (Some("Storage_Token_Refresh"), false),
        (Some("STORAGE_TOKEN_REFRESH"), false),
        (Some("x, STORAGE_TOKEN_REFRESH"), false),
    ] {
        assert_eq!(advertises_storage_token_refresh(raw), want, "{raw:?}");
    }
}
