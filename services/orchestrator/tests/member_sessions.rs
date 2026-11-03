//! Review fix 1 (auth-replacement whole-epic review, should-fix 1): a member token stops
//! working once its identity session ends (logout, a password reset), within the session-check
//! window; a live session passes; identity being down answers 503 (fails closed); and the
//! cache bounds identity reads. The same check guards realtime's `/internal/realtime/*` routes.
#![allow(dead_code, unused_imports)]

include!("common/fakes.rs");

use chelabase_auth::test_support::MemberKeys;
use chelabase_identity::Pool;
use chelabase_identity::mock::FakeIdentity;
use orchestrator::member_sessions::MemberSessions;
use std::time::Duration;

const PW: &str = "password-member-1";

struct World {
    app: TestApp,
    identity: FakeIdentity,
    keys: MemberKeys,
    tenant: TenantId,
    /// The member's identity user id (their `sub`).
    user: String,
    /// A live identity session of theirs.
    sid: String,
}

/// org-1's tenant with one active admin member, a live identity session of theirs, and an
/// orchestrator whose session answers live for `window`.
async fn world(window: Duration) -> World {
    let metadata = FakeMetadata::default();
    let tenant = TenantId::new();
    metadata.bind_org("org-1", tenant);
    let identity = FakeIdentity::start().await;
    identity.with_pool(&Pool::Members);
    let user = identity.with_user(&Pool::Members, "ana@example.com", PW);
    let (sid, _) = identity.with_session(&Pool::Members, &user);
    metadata.add_member(tenant, &user, MemberStatus::Active, true);
    let keys = MemberKeys::new();
    let app = spawn_app_core_with(
        metadata,
        FakeJobs::default(),
        "http://127.0.0.1:9",
        Mode::SelfHosted,
        10,
        Arc::new(chelabase_query::app_users::PgUsersDb::new()),
        None,
        None,
        Some(keys.verifier()),
        Some(identity.client()),
        Some(Arc::new(MemberSessions::new(
            Some(identity.client()),
            window,
        ))),
    )
    .await;
    World {
        app,
        identity,
        keys,
        tenant,
        user,
        sid,
    }
}

impl World {
    /// The gateway's identity for a member token of `user` naming session `sid`.
    fn header(&self, sid: &str) -> String {
        self.app
            .signer
            .sign_for_tenant(
                Service::Orchestrator.name(),
                &identity_of(&self.user),
                self.tenant.as_uuid(),
                sid,
            )
            .unwrap()
    }

    #[allow(clippy::disallowed_methods, clippy::disallowed_types)] // test code
    async fn me(&self, sid: &str) -> (u16, Value) {
        let resp = reqwest::Client::new()
            .get(format!("{}/v1/me", self.app.base))
            .header(IDENTITY_HEADER, self.header(sid))
            .send()
            .await
            .unwrap();
        let status = resp.status().as_u16();
        (status, resp.json().await.unwrap_or(Value::Null))
    }

    /// Identity's reads of session `sid`.
    fn lookups(&self, sid: &str) -> usize {
        let want = format!("GET /internal/v1/pools/members/sessions/{sid}");
        self.identity.calls().iter().filter(|c| **c == want).count()
    }

    /// A member token of the member naming session `sid`.
    fn member_token(&self, sid: &str) -> String {
        let mut claims = self.keys.claims(&self.user, self.tenant.as_uuid(), 300);
        claims.sid = sid.into();
        self.keys.sign(&claims)
    }

    #[allow(clippy::disallowed_methods, clippy::disallowed_types)] // test code
    async fn realtime(&self, path: &str, body: Value) -> (u16, Value) {
        let resp = reqwest::Client::new()
            .post(format!("{}{path}", self.app.base))
            .bearer_auth(REALTIME_TOKEN)
            .json(&body)
            .send()
            .await
            .unwrap();
        let status = resp.status().as_u16();
        (status, resp.json().await.unwrap_or(Value::Null))
    }
}

fn code(body: &Value) -> &str {
    body["error"]["code"].as_str().unwrap_or("")
}

// Logout ends the identity session: the token minted before it is refused.
#[tokio::test]
async fn logout_ends_the_member_token() {
    let w = world(Duration::ZERO).await;
    let (status, body) = w.me(&w.sid).await;
    assert_eq!(status, 200, "{body}");
    w.identity
        .client()
        .revoke_session(&Pool::Members, &w.sid)
        .await
        .unwrap();
    let (status, body) = w.me(&w.sid).await;
    assert_eq!(status, 401, "{body}");
    assert_eq!(code(&body), "unauthorized", "{body}");
}

// A password reset ends every session of the person (identity's setPasswordAfterCode).
#[tokio::test]
async fn reset_ends_the_member_token() {
    let w = world(Duration::ZERO).await;
    let (other, _) = w.identity.with_session(&Pool::Members, &w.user);
    assert_eq!(w.me(&w.sid).await.0, 200);
    assert_eq!(w.me(&other).await.0, 200);
    w.identity
        .client()
        .revoke_user_sessions(&Pool::Members, &w.user)
        .await
        .unwrap();
    assert_eq!(w.me(&w.sid).await.0, 401);
    assert_eq!(w.me(&other).await.0, 401);
}

// A live session passes; another person's session, or one that never existed, doesn't.
#[tokio::test]
async fn only_the_members_own_live_session_passes() {
    let w = world(Duration::ZERO).await;
    assert_eq!(w.me(&w.sid).await.0, 200);
    let someone = w.identity.with_user(&Pool::Members, "bo@example.com", PW);
    let (theirs, _) = w.identity.with_session(&Pool::Members, &someone);
    assert_eq!(w.me(&theirs).await.0, 401);
    assert_eq!(w.me(&uuid::Uuid::new_v4().to_string()).await.0, 401);
}

// Fails closed: identity down is 503, never a pass, and a later answer is asked again.
#[tokio::test]
async fn identity_down_is_503() {
    let w = world(Duration::from_secs(30)).await;
    w.identity.set_down(true);
    let (status, body) = w.me(&w.sid).await;
    assert_eq!(status, 503, "{body}");
    assert_eq!(code(&body), "identity_unavailable", "{body}");
    // An unavailable answer isn't cached.
    w.identity.set_down(false);
    assert_eq!(w.me(&w.sid).await.0, 200);
}

// No identity service configured: every member token is refused as unavailable.
#[tokio::test]
async fn no_identity_service_is_503() {
    let metadata = FakeMetadata::default();
    let tenant = TenantId::new();
    metadata.bind_org("org-1", tenant);
    metadata.add_member(tenant, DEFAULT_SUB, MemberStatus::Active, true);
    let app = spawn_app_core_with(
        metadata,
        FakeJobs::default(),
        "http://127.0.0.1:9",
        Mode::SelfHosted,
        10,
        Arc::new(chelabase_query::app_users::PgUsersDb::new()),
        None,
        None,
        None,
        None,
        Some(Arc::new(MemberSessions::new(None, Duration::from_secs(15)))),
    )
    .await;
    #[allow(clippy::disallowed_methods, clippy::disallowed_types)] // test code
    let resp = reqwest::Client::new()
        .get(format!("{}/v1/me", app.base))
        .header(IDENTITY_HEADER, app.token_for_org("org-1"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 503);
}

// The cache: one identity read per session per window, live and ended alike. Within the
// window a just-ended session still passes (the window is the bound on how long).
#[tokio::test]
async fn the_cache_bounds_identity_calls() {
    let w = world(Duration::from_secs(30)).await;
    for _ in 0..5 {
        assert_eq!(w.me(&w.sid).await.0, 200);
    }
    assert_eq!(w.lookups(&w.sid), 1, "{:?}", w.identity.calls());
    w.identity
        .client()
        .revoke_session(&Pool::Members, &w.sid)
        .await
        .unwrap();
    assert_eq!(w.me(&w.sid).await.0, 200, "inside the window");
    assert_eq!(w.lookups(&w.sid), 1);
    let gone = uuid::Uuid::new_v4().to_string();
    for _ in 0..3 {
        assert_eq!(w.me(&gone).await.0, 401);
    }
    assert_eq!(w.lookups(&gone), 1, "an ended answer is cached too");
}

// Realtime's checks go through the same session check: once the session ends, `/authorize`
// and `/projects` refuse (an open socket's channels leave at their next check), and so does
// the ticket check `/member`.
#[tokio::test]
async fn realtime_checks_refuse_an_ended_session() {
    let w = world(Duration::ZERO).await;
    let token = w.member_token(&w.sid);
    let (status, body) = w
        .realtime("/internal/realtime/projects", json!({ "token": token }))
        .await;
    assert_eq!(status, 200, "{body}");
    let member = json!({ "sub": w.user, "tenant_id": w.tenant.as_uuid(), "sid": w.sid });
    let (status, body) = w
        .realtime("/internal/realtime/member", member.clone())
        .await;
    assert_eq!(
        (status, body["allowed"].clone()),
        (200, json!(true)),
        "{body}"
    );

    w.identity
        .client()
        .revoke_session(&Pool::Members, &w.sid)
        .await
        .unwrap();
    let (status, body) = w
        .realtime("/internal/realtime/projects", json!({ "token": token }))
        .await;
    assert_eq!(
        (status, body["error"]["code"].clone()),
        (403, json!("forbidden")),
        "{body}"
    );
    let target = json!({ "database_id": uuid::Uuid::new_v4(), "branch_id": uuid::Uuid::new_v4(), "pg_database": "web1" });
    let (status, body) = w
        .realtime(
            "/internal/realtime/authorize",
            json!({ "token": token, "target": target }),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body, json!({ "allowed": false, "reason": "forbidden" }));
    let (status, body) = w.realtime("/internal/realtime/member", member).await;
    assert_eq!(
        body,
        json!({ "allowed": false, "reason": "forbidden" }),
        "{status}"
    );
    // A ticket check that names no session is refused.
    let (_, body) = w
        .realtime(
            "/internal/realtime/member",
            json!({ "sub": w.user, "tenant_id": w.tenant.as_uuid() }),
        )
        .await;
    assert_eq!(body["allowed"], json!(false), "{body}");
}

// Realtime's checks fail closed too: identity down is `unavailable`.
#[tokio::test]
async fn realtime_checks_answer_unavailable_when_identity_is_down() {
    let w = world(Duration::ZERO).await;
    let token = w.member_token(&w.sid);
    w.identity.set_down(true);
    let (status, body) = w
        .realtime("/internal/realtime/projects", json!({ "token": token }))
        .await;
    assert_eq!(
        (status, body["error"]["code"].clone()),
        (503, json!("unavailable")),
        "{body}"
    );
    let target = json!({ "database_id": uuid::Uuid::new_v4(), "branch_id": uuid::Uuid::new_v4(), "pg_database": "web1" });
    let (_, body) = w
        .realtime(
            "/internal/realtime/authorize",
            json!({ "token": token, "target": target }),
        )
        .await;
    assert_eq!(body, json!({ "allowed": false, "reason": "unavailable" }));
    let member = json!({ "sub": w.user, "tenant_id": w.tenant.as_uuid(), "sid": w.sid });
    let (status, _) = w.realtime("/internal/realtime/member", member).await;
    assert_eq!(status, 503);
}
