//! RT1 T1 edge cases beyond the RED set: a bearer survives a later timeout change, a Valkey URL
//! with credentials or a half-set ACL user is refused, and storage tokens never expire nor depend
//! on when they are signed.

use axum::Router;
use axum::extract::{Request, State};
use axum::response::{IntoResponse, Response};
use chelabase_neon::StorconClient;
use chelabase_neon::jwt::{StorageScope, generate_keypair};
use chelabase_neon::proxy_cache::ProxyCache;
use std::sync::{Arc, Mutex};
use std::time::Duration;

async fn recording_server() -> (String, Arc<Mutex<Vec<Option<String>>>>) {
    let seen: Arc<Mutex<Vec<Option<String>>>> = Arc::default();
    async fn answer(State(seen): State<Arc<Mutex<Vec<Option<String>>>>>, req: Request) -> Response {
        let auth = req
            .headers()
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        seen.lock().unwrap().push(auth);
        ([("content-type", "application/json")], "[]").into_response()
    }
    let app = Router::new().fallback(answer).with_state(seen.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), seen)
}

#[tokio::test]
async fn the_bearer_survives_a_timeout_set_after_it() {
    let (base, seen) = recording_server().await;
    StorconClient::new(&base, 0)
        .with_token("tok-1".into())
        .with_timeout(Duration::from_secs(3))
        .active_safekeepers()
        .await
        .unwrap();
    assert_eq!(seen.lock().unwrap()[0].as_deref(), Some("Bearer tok-1"));
}

#[test]
fn a_valkey_url_with_credentials_or_a_half_set_user_is_refused() {
    assert!(ProxyCache::with_auth("redis://u:p@valkey:6379", "control", "pw").is_err());
    let only_user = |n: &str| (n == "CHELABASE_VALKEY_USER").then(|| "control".to_string());
    assert!(ProxyCache::from_lookup("redis://valkey:6379", only_user).is_err());
    let only_file =
        |n: &str| (n == "CHELABASE_VALKEY_PASSWORD_FILE").then(|| "/nonexistent".to_string());
    assert!(ProxyCache::from_lookup("redis://valkey:6379", only_file).is_err());
    assert!(ProxyCache::from_lookup("redis://valkey:6379", |_| None).is_ok());
}

#[test]
fn a_storage_token_is_the_same_whenever_it_is_signed() {
    let (pem, x) = generate_keypair();
    let s = chelabase_neon::ComputeSigner::new(&pem, &x).unwrap();
    let a = s.storage_token(&StorageScope::SafekeeperData).unwrap();
    std::thread::sleep(Duration::from_millis(1100));
    let b = s.storage_token(&StorageScope::SafekeeperData).unwrap();
    assert_eq!(a, b, "no iat/exp: a running service's token never changes");
    assert_ne!(a, s.storage_token(&StorageScope::Admin).unwrap());
}
