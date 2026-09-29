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
