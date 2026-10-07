//! `SafekeeperClient` against an in-process mock safekeeper HTTP API.

use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::{Request, State};
use axum::http::StatusCode as AxumStatus;
use axum::response::{IntoResponse, Response};
use chelabase_neon::safekeeper::SafekeeperClient;
use chelabase_neon::{Lsn, NeonId};

/// Recorded (method, path) pairs.
type Seen = Arc<Mutex<Vec<(String, String)>>>;

struct Mock {
    base: reqwest::Url,
    seen: Seen,
}

/// Answers every request with `status` and `body`, recording (method, path).
async fn mock(status: u16, body: &'static str) -> Mock {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let state = seen.clone();
    let app = Router::new()
        .fallback(move |State(seen): State<Seen>, req: Request| async move {
            seen.lock()
                .unwrap()
                .push((req.method().to_string(), req.uri().path().to_string()));
            (
                AxumStatus::from_u16(status).unwrap(),
                [("content-type", "application/json")],
                body,
            )
                .into_response() as Response
        })
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Mock {
        base: format!("http://{addr}").parse().unwrap(),
        seen,
    }
}

fn id(n: u128) -> NeonId {
    NeonId::from(uuid::Uuid::from_u128(n))
}

#[tokio::test]
async fn commit_lsn_parsed() {
    // The safekeeper's status carries many more fields; only commit_lsn matters.
    let m = mock(
        200,
        r#"{"tenant_id":"t","flush_lsn":"0/16B9188","commit_lsn":"0/16B3748","backup_lsn":"0/0"}"#,
    )
    .await;
    let (tenant, timeline) = (id(1), id(2));
    let got = SafekeeperClient::new(m.base.clone())
        .commit_lsn(&tenant, &timeline)
        .await
        .unwrap();
    assert_eq!(got, Some("0/16B3748".parse::<Lsn>().unwrap()));
    assert_eq!(
        m.seen.lock().unwrap().clone(),
        vec![(
            "GET".to_string(),
            format!("/v1/tenant/{tenant}/timeline/{timeline}")
        )]
    );
}

#[tokio::test]
async fn commit_lsn_404_is_none() {
    let m = mock(404, r#"{"msg":"timeline not found"}"#).await;
    let got = SafekeeperClient::new(m.base.clone())
        .commit_lsn(&id(1), &id(2))
        .await
        .unwrap();
    assert_eq!(got, None);
}

#[tokio::test]
async fn commit_lsn_error_is_err() {
    let m = mock(500, r#"{"msg":"boom"}"#).await;
    let err = SafekeeperClient::new(m.base.clone())
        .commit_lsn(&id(1), &id(2))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("500"), "{err}");

    let m = mock(200, r#"{"commit_lsn":"nope"}"#).await;
    let err = SafekeeperClient::new(m.base.clone())
        .commit_lsn(&id(1), &id(2))
        .await
        .unwrap_err();
    assert!(!err.to_string().is_empty(), "{err:?}");
}
