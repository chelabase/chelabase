//! RT1 T7: the client of the orchestrator's `/internal/realtime/*` routes (P13, S13, S14, S15),
//! against the answers T6's handlers give (`services/orchestrator/src/internal/realtime.rs`).

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Router;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use realtime::orchestrator::{
    AUTHORIZE_TIMEOUT, AuthTarget, AuthorizeOutcome, MemberInfo, OrchestratorClient,
    PROJECTS_TIMEOUT, ProjectsError, VISIBLE_TIMEOUT, authorize_outcome, projects_result,
};
use serde_json::{Value, json};
use uuid::Uuid;

const TENANT: &str = "00000000-0000-4000-8000-000000000001";

fn body(v: Value) -> Vec<u8> {
    v.to_string().into_bytes()
}

#[test]
fn s14_timeouts() {
    assert_eq!(AUTHORIZE_TIMEOUT, Duration::from_secs(20));
    assert_eq!(PROJECTS_TIMEOUT, Duration::from_secs(20));
    assert_eq!(VISIBLE_TIMEOUT, Duration::from_secs(5));
}

#[test]
fn an_allowed_answer_carries_the_tenant_and_the_member() {
    let answer = json!({"allowed": true, "tenant_id": TENANT,
        "member": {"sub": "ana", "name": "Ana", "email": null}});
    assert_eq!(
        authorize_outcome(200, &body(answer)),
        AuthorizeOutcome::Allowed {
            tenant_id: TENANT.parse().unwrap(),
            member: MemberInfo {
                sub: "ana".into(),
                name: Some("Ana".into()),
                email: None
            },
        }
    );
}

#[test]
fn refusal_reasons_map_one_to_one() {
    for (reason, want) in [
        ("forbidden", AuthorizeOutcome::Forbidden),
        ("not_found", AuthorizeOutcome::NotFound),
        ("token_expired", AuthorizeOutcome::TokenExpired),
        ("unavailable", AuthorizeOutcome::Unavailable),
    ] {
        assert_eq!(
            authorize_outcome(200, &body(json!({"allowed": false, "reason": reason}))),
            want,
            "{reason}"
        );
    }
}

#[test]
fn an_unknown_or_missing_reason_fails_closed() {
    assert_eq!(
        authorize_outcome(
            200,
            &body(json!({"allowed": false, "reason": "something_new"}))
        ),
        AuthorizeOutcome::Forbidden
    );
    assert_eq!(
        authorize_outcome(200, &body(json!({"allowed": false}))),
        AuthorizeOutcome::Forbidden
    );
}

#[test]
fn http_failures_and_malformed_answers_are_unavailable() {
    // S13: 400/401/5xx, a timeout (tested below) and anything that isn't the route's JSON.
    for status in [400, 401, 404, 500, 502, 503, 504] {
        assert_eq!(
            authorize_outcome(status, b""),
            AuthorizeOutcome::Unavailable,
            "{status}"
        );
    }
    assert_eq!(
        authorize_outcome(200, b"not json"),
        AuthorizeOutcome::Unavailable
    );
    assert_eq!(
        authorize_outcome(
            200,
            &body(json!({"allowed": true, "member": {"sub": "ana"}}))
        ),
        AuthorizeOutcome::Unavailable,
        "allowed without tenant_id"
    );
    assert_eq!(
        authorize_outcome(200, &body(json!({"allowed": true, "tenant_id": TENANT}))),
        AuthorizeOutcome::Unavailable,
        "allowed without member"
    );
    assert_eq!(
        authorize_outcome(200, &body(json!({"allowed": "yes"}))),
        AuthorizeOutcome::Unavailable
    );
}

#[test]
fn projects_answers() {
    let (p, d) = (Uuid::from_u128(10), Uuid::from_u128(20));
    let ok = json!({"tenant_id": TENANT, "projects": [{"project_id": p, "database_ids": [d]}]});
    let view = projects_result(200, &body(ok)).unwrap();
    assert_eq!(view.tenant_id, TENANT.parse::<Uuid>().unwrap());
    assert!(view.projects[&p].contains(&d));
    assert_eq!(
        projects_result(403, &body(json!({"error": {"code": "forbidden"}}))),
        Err(ProjectsError::Forbidden)
    );
    assert_eq!(
        projects_result(404, &body(json!({"error": {"code": "not_found"}}))),
        Err(ProjectsError::Forbidden)
    );
    assert_eq!(
        projects_result(401, &body(json!({"error": {"code": "token_expired"}}))),
        Err(ProjectsError::TokenExpired)
    );
    // A bare 401 is realtime's own bearer refused: a configuration error, not the member's.
    assert_eq!(projects_result(401, b""), Err(ProjectsError::Unavailable));
    assert_eq!(
        projects_result(503, &body(json!({"error": {"code": "unavailable"}}))),
        Err(ProjectsError::Unavailable)
    );
    assert_eq!(projects_result(400, b""), Err(ProjectsError::Unavailable));
    assert_eq!(projects_result(200, b"{"), Err(ProjectsError::Unavailable));
}

/// What the fake orchestrator saw.
#[derive(Default)]
struct Seen {
    calls: Vec<(String, Option<String>, Value)>,
}

async fn serve(answer: (StatusCode, Value), delay: Duration) -> (String, Arc<Mutex<Seen>>) {
    let seen = Arc::new(Mutex::new(Seen::default()));
    let record = |path: &'static str| {
        let seen = seen.clone();
        let answer = answer.clone();
        move |headers: HeaderMap, body: String| {
            let seen = seen.clone();
            let answer = answer.clone();
            async move {
                let bearer = headers
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string);
                seen.lock().unwrap().calls.push((
                    path.to_string(),
                    bearer,
                    serde_json::from_str(&body).unwrap(),
                ));
                tokio::time::sleep(delay).await;
                (answer.0, axum::Json(answer.1))
            }
        }
    };
    let app = Router::new()
        .route(
            "/internal/realtime/authorize",
            post(record("/internal/realtime/authorize")),
        )
        .route(
            "/internal/realtime/projects",
            post(record("/internal/realtime/projects")),
        );
    let listener = tokio::net::TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), seen)
}

fn target() -> AuthTarget {
    AuthTarget {
        database_id: Uuid::from_u128(1),
        branch_id: Uuid::from_u128(2),
        pg_database: "app".into(),
        role: None,
    }
}

#[tokio::test]
async fn authorize_posts_the_token_and_target_with_realtimes_bearer() {
    let answer = json!({"allowed": true, "tenant_id": TENANT, "member": {"sub": "ana", "name": null, "email": null}});
    let (url, seen) = serve((StatusCode::OK, answer), Duration::ZERO).await;
    let client = OrchestratorClient::new(&url, "http://internal.invalid", "rt-secret");
    let out = client.authorize("member-jwt", &target()).await;
    assert!(matches!(out, AuthorizeOutcome::Allowed { .. }), "{out:?}");
    let seen = seen.lock().unwrap();
    let (path, bearer, sent) = &seen.calls[0];
    assert_eq!(
        path, "/internal/realtime/authorize",
        "at CHELABASE_ORCHESTRATOR_URL (S15)"
    );
    assert_eq!(bearer.as_deref(), Some("Bearer rt-secret"));
    assert_eq!(
        sent,
        &json!({"token": "member-jwt", "target": {
            "database_id": Uuid::from_u128(1), "branch_id": Uuid::from_u128(2), "pg_database": "app", "role": null}}),
        "T6's body, deny_unknown_fields: nothing else; the owner is role null"
    );
}

#[tokio::test]
async fn a_named_role_is_sent_as_a_string() {
    let (url, seen) = serve(
        (
            StatusCode::OK,
            json!({"allowed": false, "reason": "forbidden"}),
        ),
        Duration::ZERO,
    )
    .await;
    let client = OrchestratorClient::new(&url, "http://internal.invalid", "rt-secret");
    let mut t = target();
    t.role = Some("billing".into());
    assert_eq!(
        client.authorize("jwt", &t).await,
        AuthorizeOutcome::Forbidden
    );
    assert_eq!(seen.lock().unwrap().calls[0].2["target"]["role"], "billing");
}

#[tokio::test]
async fn projects_posts_the_token() {
    let p = Uuid::from_u128(10);
    let answer = json!({"tenant_id": TENANT, "projects": [{"project_id": p, "database_ids": []}]});
    let (url, seen) = serve((StatusCode::OK, answer), Duration::ZERO).await;
    let client = OrchestratorClient::new(&url, "http://internal.invalid", "rt-secret");
    let view = client.projects("member-jwt").await.unwrap();
    assert!(view.has_project(p));
    let seen = seen.lock().unwrap();
    assert_eq!(seen.calls[0].0, "/internal/realtime/projects");
    assert_eq!(seen.calls[0].1.as_deref(), Some("Bearer rt-secret"));
    assert_eq!(seen.calls[0].2, json!({"token": "member-jwt"}));
}

#[tokio::test]
async fn a_slow_orchestrator_is_unavailable_after_the_timeout() {
    let answer = json!({"allowed": true, "tenant_id": TENANT, "member": {"sub": "ana", "name": null, "email": null}});
    let (url, _) = serve((StatusCode::OK, answer), Duration::from_secs(30)).await;
    let client = OrchestratorClient::new(&url, "http://internal.invalid", "rt-secret")
        .with_timeouts(Duration::from_millis(300), Duration::from_millis(300));
    let started = Instant::now();
    assert_eq!(
        client.authorize("jwt", &target()).await,
        AuthorizeOutcome::Unavailable
    );
    assert_eq!(
        client.projects("jwt").await.err(),
        Some(ProjectsError::Unavailable)
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn an_unreachable_orchestrator_is_unavailable() {
    // A port nothing listens on: bind one, then drop it.
    let addr = {
        let l = std::net::TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).unwrap();
        l.local_addr().unwrap()
    };
    let client = OrchestratorClient::new(&format!("http://{addr}"), "http://internal.invalid", "t");
    assert_eq!(
        client.authorize("jwt", &target()).await,
        AuthorizeOutcome::Unavailable
    );
    assert_eq!(
        client.projects("jwt").await.err(),
        Some(ProjectsError::Unavailable)
    );
}
