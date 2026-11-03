// Test code: plain clients against local test servers (clippy.toml allows tests only).
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

mod support;

use chelabase_secrets::SecretKey;
use reqwest::StatusCode;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::{Duration, Instant};
use support::*;
use auth::app::{AppState, messages, router};
use auth::config::Config;
use auth::guard::RateLimiter;
use auth::zitadel::Zitadel;
use chelabase_auth::member_token::{MemberClaims, MemberTokenVerifier};
use chelabase_identity::Pool;
use chelabase_identity::mock::FakeIdentity;
use uuid::Uuid;

pub struct Harness {
    pub internal: support::internal::FakeInternal,
    pub cookie_key: SecretKey,
    pub mock: MockZitadel,
    pub identity: FakeIdentity,
    pub clock: auth::session::Clock,
    /// The member tokens' public key.
    pub member_public: String,
    pub _keys: tempfile::TempDir,
    pub base: String,
    pub http: reqwest::Client,
}

pub async fn harness_with(skip: bool, min_ms: u64) -> Harness {
    harness_mode(skip, min_ms, auth::config::Mode::SelfHosted).await
}

pub async fn harness_mode(skip: bool, min_ms: u64, mode: auth::config::Mode) -> Harness {
    harness_full(skip, min_ms, mode, true).await
}

/// `admin`: whether the service has the admin Zitadel client. The mock refuses the login
/// token on the calls accept-invite makes, so only the admin client can complete them.
pub async fn harness_full(
    skip: bool,
    min_ms: u64,
    mode: auth::config::Mode,
    admin: bool,
) -> Harness {
    harness_tweaked(skip, min_ms, mode, admin, |_| {}).await
}

/// [`harness_full`] with the config changed by `tweak` (m5: ids from files, Studio's config).
pub async fn harness_tweaked(
    skip: bool,
    min_ms: u64,
    mode: auth::config::Mode,
    admin: bool,
    tweak: impl FnOnce(&mut Config),
) -> Harness {
    harness_on_identity(skip, min_ms, mode, admin, tweak, None).await
}

/// [`harness_tweaked`], talking to the identity service at `identity_base` when given (a stub
/// of a test's own) instead of its [`FakeIdentity`].
pub async fn harness_on_identity(
    skip: bool,
    min_ms: u64,
    mode: auth::config::Mode,
    admin: bool,
    tweak: impl FnOnce(&mut Config),
    identity_base: Option<String>,
) -> Harness {
    let internal = support::internal::FakeInternal::start().await;
    let identity = FakeIdentity::start().await;
    identity.with_pool(&Pool::Members);
    internal.set_signup_tenant(T1);
    let client = match identity_base {
        Some(base) => chelabase_identity::Identity::new(&base, chelabase_identity::mock::TOKEN),
        None => identity.client(),
    };
    let clock = auth::session::Clock::default();
    let keys = tempfile::tempdir().unwrap();
    let (private, member_public) = chelabase_auth::internal::generate_keypair_pem();
    let member_config = auth::config::MemberTokenConfig {
        private_key: keys.path().join("member-jwt.key"),
        public_key: keys.path().join("member-jwt.pub"),
        ttl: Duration::from_secs(300),
    };
    std::fs::write(&member_config.private_key, &private).unwrap();
    std::fs::write(&member_config.public_key, &member_public).unwrap();
    let mock = MockZitadel::start().await;
    mock.set_strict_login(true);
    let cookie_key = SecretKey::generate();
    let mut config = Config {
        listen: String::new(),
        issuer: mock.base.clone(),
        zitadel: chelabase_types::entry::ZitadelAt { base: mock.base.clone(), host: None },
        token: TOKEN.into(),
        cookie_key: cookie_key.clone(),
        org_id: "o1".into(),
        studio_url: "http://studio.chelabase.localhost:8000".into(),
        studio: Default::default(),
        skip_email_verification: skip,
        secure_cookies: false,
        min_response: Duration::from_millis(min_ms),
        mode,
        // The test client connects from loopback: it stands in for the gateway.
        trusted_proxies: auth::guard::TrustedProxies::parse("127.0.0.1,::1").unwrap(),
    };
    tweak(&mut config);
    let tokens = auth::member_token::MemberTokens::from_config(&member_config, &config.issuer).unwrap();
    let state = Arc::new(AppState {
        zitadel: Zitadel::new(&mock.base, TOKEN),
        config,
        limiter: RateLimiter::default(),
        app: None,
        signup_gate: Some(auth::app_users::InternalClient::new(
            &internal.base,
            support::internal::INTERNAL_TOKEN,
        )),
        used_tickets: Default::default(),
        admin: admin.then(|| Zitadel::new(&mock.base, ADMIN_TOKEN)),
        member_tokens: tokens,
        identity: client,
        clock: clock.clone(),
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(
            listener,
            router(state).into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap()
    });
    Harness {
        internal,
        cookie_key,
        mock,
        identity,
        clock,
        member_public,
        _keys: keys,
        base: format!("http://{addr}"),
        http: reqwest::Client::new(),
    }
}

pub async fn harness() -> Harness {
    harness_with(false, 1).await
}

pub struct Answer {
    pub status: StatusCode,
    pub body: Value,
    pub cookies: Vec<String>,
}

impl Answer {
    /// `name=value` of a cookie this answer set, for sending back.
    pub fn cookie(&self, name: &str) -> Option<String> {
        self.cookies
            .iter()
            .find(|c| c.starts_with(&format!("{name}=")))
            .map(|c| c.split(';').next().unwrap().to_string())
    }
}

impl Harness {
    pub async fn get(&self, path_and_query: &str, ip: &str) -> Answer {
        let resp = self
            .http
            .get(format!("{}{path_and_query}", self.base))
            .header("x-forwarded-for", format!("9.9.9.9, {ip}"))
            .send()
            .await
            .unwrap();
        let status = resp.status();
        let cache = resp.headers().get("cache-control").map(|v| v.to_str().unwrap().to_string());
        let text = resp.text().await.unwrap();
        let mut body: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        if let (Some(cache), Value::Object(map)) = (cache, &mut body) {
            map.insert("_cache_control".into(), json!(cache));
        }
        Answer { status, body, cookies: Vec::new() }
    }

    pub async fn post(&self, path: &str, body: Value, cookie: Option<&str>, ip: &str) -> Answer {
        let mut req = self
            .http
            .post(format!("{}{path}", self.base))
            .json(&body)
            .header("x-forwarded-for", format!("9.9.9.9, {ip}"));
        if let Some(c) = cookie {
            req = req.header("cookie", c);
        }
        let resp = req.send().await.unwrap();
        let status = resp.status();
        let cookies = resp
            .headers()
            .get_all("set-cookie")
            .iter()
            .map(|v| v.to_str().unwrap().to_string())
            .collect();
        let text = resp.text().await.unwrap();
        Answer {
            status,
            body: serde_json::from_str(&text).unwrap_or(Value::Null),
            cookies,
        }
    }
}

fn err(code: &str, message: &str) -> Value {
    json!({"error": {"code": code, "message": message}})
}

fn signin(email: &str, pw: &str) -> Value {
    json!({"email": email, "password": pw})
}
fn signup(email: &str, pw: &str) -> Value {
    json!({"email": email, "password": pw, "givenName": "A", "familyName": "B"})
}

// R37: a closed instance never offers sign-up, even to a request that asked for it.
// Until m11, verify-email still finishes a Zitadel step (an identity user's code is refused).
#[tokio::test]
async fn verify_email_finishes_a_zitadel_step() {
    let h = harness().await;
    let id = h.mock.add_user("ada@example.com", "Secret-pass1", false);
    let session = Zitadel::new(&h.mock.base, TOKEN).create_session(&id, "Secret-pass1").await.unwrap();
    let state = auth::cookie::LoginState {
        auth_request: h.mock.add_auth_request(false),
        user_id: Some(id),
        session_id: Some(session.id),
        session_token: Some(session.token),
        step: auth::cookie::Step::Verify,
        decoy: false,
        exp: auth::cookie::now_secs() + 60,
    };
    let login = format!("chelabase_login={}", auth::cookie::seal_login(&h.cookie_key, &state));
    let bad = h.post("/auth/v1/verify-email", json!({"code": "000000"}), Some(&login), "1.1.1.1").await;
    assert_eq!((bad.status, bad.body), (StatusCode::BAD_REQUEST, err("invalid_code", messages::BAD_CODE)));
    let done = h.post("/auth/v1/verify-email", json!({"code": EMAIL_CODE}), Some(&login), "1.1.1.1").await;
    assert_eq!(done.body["next"], "done");
    assert!(done.cookies.iter().any(|c| c.starts_with("chelabase_login=;") && c.contains("Max-Age=0")));
    assert!(h.mock.user("ada@example.com").unwrap().verified);
}

// #79: only Zitadel's password-policy refusals on create are `weak_password`; another 400 (a
// missing org: the instance is misconfigured) or a refusal nobody captured is 503 `unavailable`.
#[tokio::test]
async fn sign_up_answers_weak_password_only_for_a_policy_refusal() {
    let h = harness().await;
    for (status_in, key, status, code) in [
        (400, "weak_password", StatusCode::BAD_REQUEST, "weak_password"),
        (400, "invalid_email", StatusCode::BAD_REQUEST, "invalid_request"),
        (404, "pool_not_found", StatusCode::SERVICE_UNAVAILABLE, "unavailable"),
        (500, "internal", StatusCode::SERVICE_UNAVAILABLE, "unavailable"),
    ] {
        h.identity.fail("POST /internal/v1/pools/members/users", status_in, key, 1);
        let a = h.post("/auth/v1/sign-up", signup("new@example.com", "Secret-pass1"), None, "1.1.1.1").await;
        assert_eq!((a.status, a.body["error"]["code"].as_str()), (status, Some(code)), "{key}: {}", a.body);
    }
    assert_eq!(h.users_with("new@example.com").await, 0);
}

/// A name Zitadel would refuse is refused before it is called, and a validation refusal that
/// still comes back (code 3, no id) is the visitor's input, never a weak password (#79).
/// A bad name is refused before identity is called, and a refusal that still comes back is the
/// visitor's input, never a weak password (#79).
#[tokio::test]
async fn sign_up_never_blames_the_password_for_a_bad_name() {
    let h = harness().await;
    for given in ["   ", &"x".repeat(201)] {
        let mut body = signup("new@example.com", "Secret-pass1");
        body["givenName"] = json!(given);
        let a = h.post("/auth/v1/sign-up", body, None, "1.1.1.1").await;
        assert_eq!((a.status, a.body["error"]["code"].as_str()), (StatusCode::BAD_REQUEST, Some("invalid_request")), "{given:?}: {}", a.body);
    }
    assert!(h.identity.calls().iter().all(|c| c != "POST /internal/v1/pools/members/users"), "a bad name never reaches identity");
    h.identity.fail("POST /internal/v1/pools/members/users", 400, "invalid_request", 1);
    let a = h.post("/auth/v1/sign-up", signup("new@example.com", "Secret-pass1"), None, "2.2.2.2").await;
    assert_eq!((a.status, a.body["error"]["code"].as_str()), (StatusCode::BAD_REQUEST, Some("invalid_request")), "{}", a.body);
}

#[tokio::test]
async fn a_taken_email_looks_the_same_and_its_code_never_works() {
    let h = harness().await;
    h.identity.with_user(&Pool::Members, "ada@example.com", "Secret-pass1");
    let fresh = h
        .post(
            "/auth/v1/sign-up",
            signup("new@example.com",
                "Secret-pass1",
            ),
            None,
            "1.1.1.1",
        )
        .await;
    let taken = h
        .post(
            "/auth/v1/sign-up",
            signup("ada@example.com",
                "Secret-pass1",
            ),
            None,
            "2.2.2.2",
        )
        .await;
    assert_eq!(
        (taken.status, taken.body.clone()),
        (fresh.status, fresh.body.clone())
    );
    let decoy = taken.cookie("chelabase_login").unwrap();
    assert_eq!(decoy.len(), fresh.cookie("chelabase_login").unwrap().len());
    let a = h
        .post(
            "/auth/v1/verify-email",
            json!({"code": EMAIL_CODE}),
            Some(&decoy),
            "2.2.2.2",
        )
        .await;
    assert_eq!(
        (a.status, a.body),
        (
            StatusCode::BAD_REQUEST,
            err("invalid_code", messages::BAD_CODE)
        )
    );
}

#[tokio::test]
async fn a_weak_password_at_sign_up_says_so() {
    let h = harness().await;
    let a = h
        .post(
            "/auth/v1/sign-up",
            signup("new@example.com", "short"),
            None,
            "1.1.1.1",
        )
        .await;
    assert_eq!(
        (a.status, a.body),
        (
            StatusCode::BAD_REQUEST,
            err("weak_password", messages::WEAK_PASSWORD)
        )
    );
}

#[tokio::test]
async fn the_dev_flag_signs_up_already_verified() {
    let h = harness_with(true, 1).await;
    let a = h
        .post(
            "/auth/v1/sign-up",
            signup("dev@example.com",
                "Secret-pass1",
            ),
            None,
            "1.1.1.1",
        )
        .await;
    assert_eq!(a.body["next"], "pending");
    let user = h.identity.client().search_users(&Pool::Members, "dev@example.com", 10, 0).await.unwrap().users.remove(0);
    assert!(user.email_verified);
    assert!(h.identity.codes_sent().is_empty());
}

#[tokio::test]
async fn resend_always_answers_ok() {
    let h = harness().await;
    let a = h
        .post("/auth/v1/resend-code", json!({}), None, "1.1.1.1")
        .await;
    assert_eq!((a.status, a.body), (StatusCode::OK, json!({})));
}

#[tokio::test]
async fn sign_in_answers_take_the_minimum_time() {
    let h = harness_with(false, 300).await;
    let start = Instant::now();
    h.post(
        "/auth/v1/sign-in",
        signin("nobody@example.com", "x"),
        None,
        "1.1.1.1",
    )
    .await;
    assert!(start.elapsed() >= Duration::from_millis(300));
}

#[tokio::test]
async fn the_sixth_sign_in_per_email_per_minute_is_refused_even_from_new_ips() {
    let h = harness().await;
    for i in 0..5 {
        let a = h
            .post(
                "/auth/v1/sign-in",
                signin("ada@example.com", "x"),
                None,
                &format!("1.1.1.{i}"),
            )
            .await;
        assert_eq!(a.status, StatusCode::UNAUTHORIZED);
    }
    let a = h
        .post(
            "/auth/v1/sign-in",
            signin("ada@example.com", "x"),
            None,
            "7.7.7.7",
        )
        .await;
    assert_eq!(
        (a.status, a.body),
        (
            StatusCode::TOO_MANY_REQUESTS,
            err("rate_limited", messages::TOO_MANY)
        )
    );
}

// Review Focus 5.
#[tokio::test]
async fn verify_email_is_rate_limited() {
    let h = harness().await;
    let a = h
        .post(
            "/auth/v1/sign-up",
            signup("new@example.com",
                "Secret-pass1",
            ),
            None,
            "3.3.3.3",
        )
        .await;
    let login = a.cookie("chelabase_login").unwrap();
    for _ in 0..10 {
        h.post(
            "/auth/v1/verify-email",
            json!({"code": "000000"}),
            Some(&login),
            "3.3.3.3",
        )
        .await;
    }
    let a = h
        .post(
            "/auth/v1/verify-email",
            json!({"code": EMAIL_CODE}),
            Some(&login),
            "3.3.3.3",
        )
        .await;
    assert_eq!(a.status, StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn a_non_json_request_is_refused() {
    let h = harness().await;
    let resp = h
        .http
        .post(format!("{}/auth/v1/sign-in", h.base))
        .body("email=a")
        .header("content-type", "application/x-www-form-urlencoded")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "invalid_request");
}

#[tokio::test]
async fn password_reset_answers_the_same_whether_or_not_the_user_exists() {
    let h = harness().await;
    let id = h.mock.add_user("ada@example.com", "Old-pass-1", true);
    let known = h
        .post(
            "/auth/v1/password-reset",
            json!({"email": "ADA@example.com"}),
            None,
            "1.1.1.1",
        )
        .await;
    let unknown = h
        .post(
            "/auth/v1/password-reset",
            json!({"email": "nobody@example.com"}),
            None,
            "1.1.1.1",
        )
        .await;
    assert_eq!(
        (known.status, known.body.clone()),
        (StatusCode::OK, json!({}))
    );
    assert_eq!((unknown.status, unknown.body), (known.status, known.body));
    // The email goes out in the background.
    for _ in 0..50 {
        if !h.mock.emails().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(h.mock.emails(), vec![(id, "reset".to_string())]);
}

#[tokio::test]
async fn confirm_sets_the_new_password() {
    let h = harness().await;
    let id = h.mock.add_user("ada@example.com", "Old-pass-1", true);
    let bad = h
        .post(
            "/auth/v1/password-reset/confirm",
            json!({"userId": id, "code": "000000", "password": "New-pass-12"}),
            None,
            "1.1.1.1",
        )
        .await;
    assert_eq!(
        (bad.status, bad.body),
        (
            StatusCode::BAD_REQUEST,
            err("invalid_code", messages::BAD_CODE)
        )
    );
    let ok = h
        .post(
            "/auth/v1/password-reset/confirm",
            json!({"userId": id, "code": RESET_CODE, "password": "New-pass-12"}),
            None,
            "1.1.1.1",
        )
        .await;
    assert_eq!((ok.status, ok.body), (StatusCode::OK, json!({})));
    assert_eq!(
        h.mock.user("ada@example.com").unwrap().password,
        "New-pass-12"
    );
}

// Zitadel hashes with bcrypt (72 bytes at most): platform sign-up and reset confirm refuse a
// longer password before Zitadel sees it, counted in bytes (39 characters can be 74), and take
// one of exactly 72.
#[tokio::test]
async fn platform_sign_up_and_confirm_refuse_a_password_over_72_bytes_before_zitadel() {
    let h = harness().await;
    let long = format!("Aa1!{}", "é".repeat(35));
    let max = format!("Aa1!{}", "é".repeat(34));
    assert_eq!((long.chars().count(), long.len(), max.len()), (39, 74, 72));
    let a = h
        .post("/auth/v1/sign-up", signup("new@example.com", &long), None, "1.1.1.1")
        .await;
    assert_eq!((a.status, a.body), (StatusCode::BAD_REQUEST, err("weak_password", messages::WEAK_PASSWORD)));
    assert!(!h.identity.calls().iter().any(|c| *c == "POST /internal/v1/pools/members/users"), "{:?}", h.identity.calls());
    let id = h.mock.add_user("ada@example.com", "Old-pass-1", true);
    let a = h
        .post("/auth/v1/password-reset/confirm", json!({"userId": id, "code": RESET_CODE, "password": long}), None, "1.1.1.1")
        .await;
    assert_eq!((a.status, a.body), (StatusCode::BAD_REQUEST, err("weak_password", messages::WEAK_PASSWORD)));
    assert!(!h.mock.calls().iter().any(|c| c.ends_with("/password")), "{:?}", h.mock.calls());
    assert_eq!(h.mock.user("ada@example.com").unwrap().password, "Old-pass-1");
    // 72 bytes is fine on both.
    let a = h
        .post("/auth/v1/sign-up", signup("max@example.com", &max), None, "1.1.1.1")
        .await;
    assert_eq!((a.status, a.body), (StatusCode::OK, json!({"next": "verify"})));
    let a = h
        .post("/auth/v1/password-reset/confirm", json!({"userId": id, "code": RESET_CODE, "password": max}), None, "1.1.1.1")
        .await;
    assert_eq!((a.status, a.body), (StatusCode::OK, json!({})));
    assert_eq!(h.mock.user("ada@example.com").unwrap().password, max);
}

// Review Focus 3.
#[tokio::test]
async fn confirm_with_a_weak_password_says_so() {
    let h = harness().await;
    let id = h.mock.add_user("ada@example.com", "Old-pass-1", true);
    let a = h
        .post(
            "/auth/v1/password-reset/confirm",
            json!({"userId": id, "code": RESET_CODE, "password": "short"}),
            None,
            "1.1.1.1",
        )
        .await;
    assert_eq!(
        (a.status, a.body),
        (
            StatusCode::BAD_REQUEST,
            err("weak_password", messages::WEAK_PASSWORD)
        )
    );
}

#[tokio::test]
async fn logout_ends_the_session_and_clears_both_cookies() {
    let h = harness().await;
    h.member("ada@example.com", &[T1]);
    let cookie = h.session("", "ada@example.com", "1.1.1.1").await;
    assert_eq!(h.sessions(), 1);
    let out = h.post("/auth/v1/logout", json!({}), Some(&cookie), "1.1.1.1").await;
    assert_eq!((out.status, out.body), (StatusCode::OK, json!({})));
    assert_eq!(h.sessions(), 0);
    for name in ["chelabase_login=;", "chelabase_session=;"] {
        assert!(out.cookies.iter().any(|c| c.starts_with(name) && c.contains("Max-Age=0")), "{name}");
    }
    let again = h.post("/auth/v1/logout", json!({}), None, "1.1.1.1").await;
    assert_eq!(again.status, StatusCode::OK);
}

// --- Security review fixes -------------------------------------------------------------

// Live finding: Zitadel's session API can't see a user for a moment after CreateUser.
// C1: ids go into Zitadel URL paths, so only Zitadel-shaped ids are accepted.
#[tokio::test]
async fn ids_that_are_not_zitadel_ids_never_reach_zitadel() {
    let h = harness().await;
    let id = h.mock.add_user("ada@example.com", "Old-pass-1", true);
    let confirm = h.post("/auth/v1/password-reset/confirm", json!({"userId": format!("{id}/lock?x="), "code": RESET_CODE, "password": "New-pass-12"}), None, "1.1.1.1").await;
    assert_eq!((confirm.status, confirm.body), (StatusCode::BAD_REQUEST, err("invalid_code", messages::BAD_CODE)));
    assert_eq!(h.mock.user("ada@example.com").unwrap().password, "Old-pass-1");
}

// I1: a cookieless resend must not share a rate-limit bucket with decoy cookies.
#[tokio::test]
async fn resend_limits_do_not_reveal_a_decoy() {
    let h = harness().await;
    h.identity.with_user(&Pool::Members, "ada@example.com", "Secret-pass1");
    for _ in 0..5 {
        h.post("/auth/v1/resend-code", json!({}), None, "4.4.4.4")
            .await;
    }
    let taken = h
        .post(
            "/auth/v1/sign-up",
            signup("ada@example.com",
                "Secret-pass1",
            ),
            None,
            "4.4.4.4",
        )
        .await;
    let a = h
        .post(
            "/auth/v1/resend-code",
            json!({}),
            Some(&taken.cookie("chelabase_login").unwrap()),
            "4.4.4.4",
        )
        .await;
    assert_eq!(a.status, StatusCode::OK);
}

// I1, I2: code endpoints take the minimum time too, so a decoy isn't faster.
#[tokio::test]
async fn code_endpoints_take_the_minimum_time() {
    let h = harness_with(false, 300).await;
    h.identity.with_user(&Pool::Members, "ada@example.com", "Secret-pass1");
    let taken = h
        .post(
            "/auth/v1/sign-up",
            signup("ada@example.com",
                "Secret-pass1",
            ),
            None,
            "5.5.5.5",
        )
        .await;
    let decoy = taken.cookie("chelabase_login").unwrap();
    for (path, body) in [
        ("/auth/v1/verify-email", json!({"code": "000000"})),
        ("/auth/v1/resend-code", json!({})),
    ] {
        let start = Instant::now();
        h.post(path, body, Some(&decoy), "5.5.5.5").await;
        assert!(start.elapsed() >= Duration::from_millis(300), "{path}");
    }
    let start = Instant::now();
    h.post(
        "/auth/v1/password-reset/confirm",
        json!({"userId": "3000999", "code": "x", "password": "New-pass-12"}),
        None,
        "5.5.5.5",
    )
    .await;
    assert!(start.elapsed() >= Duration::from_millis(300), "confirm");
}

// I3: an IP over its limit doesn't spend (or create) per-email buckets.
#[tokio::test]
async fn an_ip_over_its_limit_does_not_use_up_an_email_bucket() {
    let h = harness().await;
    for i in 0..10 {
        h.post(
            "/auth/v1/sign-in",
            signin(&format!("x{i}@example.com"),
                "x",
            ),
            None,
            "6.6.6.6",
        )
        .await;
    }
    for _ in 0..5 {
        let a = h
            .post(
                "/auth/v1/sign-in",
                signin("z@example.com", "x"),
                None,
                "6.6.6.6",
            )
            .await;
        assert_eq!(a.status, StatusCode::TOO_MANY_REQUESTS);
    }
    let a = h
        .post(
            "/auth/v1/sign-in",
            signin("z@example.com", "x"),
            None,
            "6.6.6.7",
        )
        .await;
    assert_eq!(a.status, StatusCode::UNAUTHORIZED);
}

// I3: huge bodies (and so huge rate-limit keys) are refused up front.
#[tokio::test]
async fn oversized_bodies_are_refused() {
    let h = harness().await;
    let email = format!("{}@example.com", "a".repeat(20_000));
    let a = h
        .post(
            "/auth/v1/sign-in",
            signin(&email, "x"),
            None,
            "1.1.1.1",
        )
        .await;
    assert_eq!(a.status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(a.body["error"]["code"], "too_large");
}

// I4: an email that already has an account never gets a second one.
#[tokio::test]
async fn sign_up_never_duplicates_an_existing_email() {
    let h = harness().await;
    h.identity.with_user(&Pool::Members, "ada@example.com", "Secret-pass1");
    let a = h
        .post(
            "/auth/v1/sign-up",
            signup("Ada@Example.com",
                "Secret-pass1",
            ),
            None,
            "1.1.1.1",
        )
        .await;
    assert_eq!(
        (a.status, a.body),
        (StatusCode::OK, json!({"next": "verify"}))
    );
    assert_eq!(h.users_with("ada@example.com").await, 1);
}

// N2: two accounts already sharing an email (made outside these checks) still mean "taken".
// I5: the weak-password answer doesn't depend on whether the user id exists.
#[tokio::test]
async fn a_weak_password_is_refused_before_the_user_is_looked_up() {
    let h = harness().await;
    let a = h
        .post(
            "/auth/v1/password-reset/confirm",
            json!({"userId": "3000999", "code": RESET_CODE, "password": "alllowercase1!"}),
            None,
            "1.1.1.1",
        )
        .await;
    assert_eq!(
        (a.status, a.body),
        (
            StatusCode::BAD_REQUEST,
            err("weak_password", messages::WEAK_PASSWORD)
        )
    );
    let b = h
        .post(
            "/auth/v1/sign-up",
            signup("new@example.com",
                "NoSymbol123",
            ),
            None,
            "1.1.1.1",
        )
        .await;
    assert_eq!(
        (b.status, b.body),
        (
            StatusCode::BAD_REQUEST,
            err("weak_password", messages::WEAK_PASSWORD)
        )
    );
}

// I6: codes are limited per account too, not only per IP.
#[tokio::test]
async fn confirm_is_limited_per_user_across_ips() {
    let h = harness().await;
    let id = h.mock.add_user("ada@example.com", "Old-pass-1", true);
    for i in 0..10 {
        h.post(
            "/auth/v1/password-reset/confirm",
            json!({"userId": id, "code": "000000", "password": "New-pass-12"}),
            None,
            &format!("8.8.8.{i}"),
        )
        .await;
    }
    let a = h
        .post(
            "/auth/v1/password-reset/confirm",
            json!({"userId": id, "code": RESET_CODE, "password": "New-pass-12"}),
            None,
            "8.8.9.9",
        )
        .await;
    assert_eq!(a.status, StatusCode::TOO_MANY_REQUESTS);
}

// Self-hosted (verification off): a taken email is told so; there's no code step to hide behind.
#[tokio::test]
async fn a_taken_email_without_verification_says_it_exists() {
    let h = harness_with(true, 1).await;
    h.identity.with_user(&Pool::Members, "ada@example.com", "Secret-pass1");
    let a = h
        .post(
            "/auth/v1/sign-up",
            signup("Ada@Example.com",
                "Secret-pass1",
            ),
            None,
            "1.1.1.1",
        )
        .await;
    assert_eq!(
        (a.status, a.body),
        (
            StatusCode::CONFLICT,
            err("email_taken", messages::ACCOUNT_EXISTS)
        )
    );
    assert!(a.cookies.is_empty());
    assert_eq!(h.sessions(), 0, "no sign-in from the sign-up form");
}

// --- Minor findings -----------------------------------------------------------------

// An expired or missing step cookie is a dead sign-in, not a bad code: Studio offers "Start again".
#[tokio::test]
async fn verify_without_a_step_cookie_is_gone() {
    let h = harness().await;
    let a = h
        .post(
            "/auth/v1/verify-email",
            json!({"code": "123456"}),
            None,
            "1.1.1.1",
        )
        .await;
    assert_eq!(
        (a.status, a.body),
        (StatusCode::GONE, err("sign_in_expired", messages::GONE))
    );
}

// Successful sign-ins don't use up the per-email limit, so nobody can lock a known account
// out by signing in as it... they'd need its password.
#[tokio::test]
async fn only_failed_sign_ins_count_against_an_email() {
    let h = harness().await;
    h.member("ada@example.com", &[T1]);
    for i in 0..7 {
        let a = h
            .post(
                "/auth/v1/sign-in",
                signin("ada@example.com",
                    "Secret-pass1",
                ),
                None,
                &format!("9.8.7.{i}"),
            )
            .await;
        assert_eq!(a.status, StatusCode::OK, "attempt {i}");
    }
}

// Sign-up waits out Zitadel's projection lag, so its floor sits above sign-in's.
#[tokio::test]
async fn sign_up_answers_take_the_longer_minimum() {
    let h = harness_with(false, 300).await;
    let start = Instant::now();
    h.post(
        "/auth/v1/sign-up",
        signup("new@example.com",
            "Secret-pass1",
        ),
        None,
        "1.1.1.1",
    )
    .await;
    assert!(start.elapsed() >= Duration::from_millis(1300));
}

// Platform mode: the real owner of a taken email hears about the attempt (a reset link).
#[tokio::test]
async fn a_decoy_sign_up_emails_the_owner_a_reset_link() {
    let h = harness().await;
    // The reset stays on Zitadel until m11.
    let id = h.mock.add_user("ada@example.com", "Secret-pass1", true);
    h.identity.with_user(&Pool::Members, "ada@example.com", "Secret-pass1");
    h.post(
        "/auth/v1/sign-up",
        signup("ada@example.com",
            "Secret-pass1",
        ),
        None,
        "1.1.1.1",
    )
    .await;
    for _ in 0..50 {
        if !h.mock.emails().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(h.mock.emails(), vec![(id, "reset".to_string())]);
}

const SIGN_UP_CALL: &str = "POST /internal/signup/check";

#[tokio::test]
async fn sign_up_refused_when_closed() {
    let h = harness().await;
    h.internal.set_signup(false, "closed");
    let a = h
        .post(
            "/auth/v1/sign-up",
            signup("new@example.com", "Secret-pass1"),
            None,
            "1.1.1.1",
        )
        .await;
    assert_eq!(a.status, StatusCode::FORBIDDEN);
    assert_eq!(a.body["error"]["code"], "sign_up_closed");
    assert_eq!(h.users_with("new@example.com").await, 0);
    let asked = h.internal.bodies(SIGN_UP_CALL);
    assert_eq!(asked, vec![json!({"org": "o1"})]);
}

#[tokio::test]
async fn sign_up_allowed_when_open() {
    let h = harness().await;
    h.internal.set_signup(true, "open");
    let a = h
        .post(
            "/auth/v1/sign-up",
            signup("new@example.com", "Secret-pass1"),
            None,
            "1.1.1.1",
        )
        .await;
    assert_eq!((a.status, a.body), (StatusCode::OK, json!({"next": "verify"})));
    assert_eq!(h.users_with("new@example.com").await, 1);
}

#[tokio::test]
async fn first_member_may_sign_up() {
    let h = harness().await;
    h.internal.set_signup(true, "first_member");
    let a = h
        .post(
            "/auth/v1/sign-up",
            signup("first@example.com", "Secret-pass1"),
            None,
            "1.1.1.1",
        )
        .await;
    assert_eq!(a.status, StatusCode::OK);
    assert_eq!(h.users_with("first@example.com").await, 1);
}

#[tokio::test]
async fn sign_up_fails_closed_when_the_gate_is_unreachable() {
    let h = harness().await;
    h.internal.set_signup_status(Some(503));
    let a = h
        .post(
            "/auth/v1/sign-up",
            signup("new@example.com", "Secret-pass1"),
            None,
            "1.1.1.1",
        )
        .await;
    assert_eq!(a.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(h.users_with("new@example.com").await, 0);
}

#[tokio::test]
async fn platform_sign_up_refused() {
    let h = harness_mode(false, 1, auth::config::Mode::Platform).await;
    let a = h
        .post(
            "/auth/v1/sign-up",
            signup("new@example.com", "Secret-pass1"),
            None,
            "1.1.1.1",
        )
        .await;
    assert_eq!(a.status, StatusCode::FORBIDDEN);
    assert_eq!(a.body["error"]["code"], "not_available_in_platform_mode");
    assert_eq!(h.users_with("new@example.com").await, 0);
    assert!(h.internal.bodies(SIGN_UP_CALL).is_empty());
}

fn accept(ar: &str, user: &str, code: &str, pw: &str) -> Value {
    json!({"authRequest": ar, "user": user, "code": code, "password": pw})
}

// M4: after the password is set, Zitadel's session API may not see the user yet (a 404 for a
// moment): accept-invite waits for it instead of failing with a spent code.
#[tokio::test]
async fn accept_invite_waits_for_zitadel_to_see_the_user() {
    let h = harness().await;
    let (id, code) = h.mock.add_invited_user("lag@example.com");
    h.mock.hide_user(&id, 3);
    let a = h
        .post(
            "/auth/v1/accept-invite",
            accept(&h.mock.add_auth_request(false), &id, &code, "Secret-pass1"),
            None,
            "1.1.1.1",
        )
        .await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.body);
    assert_eq!(a.body["next"], "done");
    assert!(a.cookie("chelabase_session").is_some());
}

// M4: accept-invite may wait out that lag, so its floor sits above sign-in's (SIGN_UP_EXTRA), or
// an answer after a lag would be slower than one without.
#[tokio::test]
async fn accept_invite_answers_take_the_longer_minimum() {
    let h = harness_with(false, 300).await;
    let (id, code) = h.mock.add_invited_user("slow@example.com");
    let start = Instant::now();
    h.post(
        "/auth/v1/accept-invite",
        accept(&h.mock.add_auth_request(false), &id, &code, "Secret-pass1"),
        None,
        "1.1.1.1",
    )
    .await;
    assert!(start.elapsed() >= Duration::from_millis(300) + Duration::from_millis(1000));
    // A refused one (a wrong code) is held as long.
    let start = Instant::now();
    let bad = h
        .post(
            "/auth/v1/accept-invite",
            accept(&h.mock.add_auth_request(false), &id, "wrong", "Secret-pass1"),
            None,
            "1.1.1.2",
        )
        .await;
    assert_eq!(bad.status, StatusCode::BAD_REQUEST);
    assert!(start.elapsed() >= Duration::from_millis(300) + Duration::from_millis(1000));
}

#[tokio::test]
async fn accept_invite_sets_password_and_signs_in() {
    let h = harness().await;
    let (id, code) = h.mock.add_invited_user("bob@example.com");
    let ar = h.mock.add_auth_request(false);
    let a = h
        .post(
            "/auth/v1/accept-invite",
            accept(&ar, &id, &code, "Secret-pass1"),
            None,
            "1.1.1.1",
        )
        .await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.body);
    assert_eq!(a.body["next"], "done");
    assert!(a.body["redirect"].as_str().unwrap().contains("callback"));
    assert!(a.cookie("chelabase_session").is_some());
    let user = h.mock.user_by_id(&id).unwrap();
    assert_eq!(user.password, "Secret-pass1");
    assert!(user.verified);
    // The code is single use.
    let again = h
        .post(
            "/auth/v1/accept-invite",
            accept(&h.mock.add_auth_request(false), &id, &code, "Other-pass1"),
            None,
            "1.1.1.1",
        )
        .await;
    assert_eq!(again.status, StatusCode::BAD_REQUEST);
    assert_eq!(h.mock.user_by_id(&id).unwrap().password, "Secret-pass1");
}

// I1: a deactivated user (a removed member invited again) is reactivated only once the code is
// verified; a wrong code leaves them deactivated.
#[tokio::test]
async fn accept_invite_reactivates_a_deactivated_user_after_the_code() {
    let h = harness().await;
    let (id, code) = h.mock.add_invited_user("back@example.com");
    h.mock.set_inactive(&id, true);
    h.internal.set_open_invite(&id, true);
    let wrong = h
        .post(
            "/auth/v1/accept-invite",
            accept(&h.mock.add_auth_request(false), &id, "WRONG", "Secret-pass1"),
            None,
            "1.1.1.1",
        )
        .await;
    assert_eq!(wrong.status, StatusCode::BAD_REQUEST);
    assert!(h.mock.user_by_id(&id).unwrap().inactive, "a wrong code reactivates nothing");
    let ok = h
        .post(
            "/auth/v1/accept-invite",
            accept(&h.mock.add_auth_request(false), &id, &code, "Secret-pass1"),
            None,
            "1.1.1.1",
        )
        .await;
    assert_eq!(ok.status, StatusCode::OK, "{}", ok.body);
    assert!(!h.mock.user_by_id(&id).unwrap().inactive, "reactivated by the accept");
}

// Live finding: Zitadel makes no invite code for a user who has a password ("User is already
// initialized"), so a removed member invited again gets a password-reset code, behind the
// accept-invite link with `reset: true`. The code sets the password (a wrong one sets nothing
// and reactivates nothing); only then is the user reactivated.
#[tokio::test]
async fn accept_invite_with_a_reset_code_sets_the_password_then_reactivates() {
    let h = harness().await;
    let id = h.mock.add_user("again@example.com", "Old-pass-12", true);
    h.mock.set_inactive(&id, true);
    h.internal.set_open_invite(&id, true);
    let reset = |code: &str, pw: &str| {
        let mut body = accept(&h.mock.add_auth_request(false), &id, code, pw);
        body["reset"] = json!(true);
        body
    };
    let wrong = h.post("/auth/v1/accept-invite", reset("000000", "Secret-pass1"), None, "1.1.1.1").await;
    assert_eq!(wrong.status, StatusCode::BAD_REQUEST, "{}", wrong.body);
    let user = h.mock.user_by_id(&id).unwrap();
    assert!(user.inactive, "a wrong code reactivates nothing");
    assert_eq!(user.password, "Old-pass-12", "a wrong code sets nothing");
    assert!(!h.mock.calls().iter().any(|c| c.contains("invite_code")), "no invite code is checked");
    let ok = h.post("/auth/v1/accept-invite", reset(RESET_CODE, "Secret-pass1"), None, "1.1.1.1").await;
    assert_eq!(ok.status, StatusCode::OK, "{}", ok.body);
    assert_eq!(ok.body["next"], "done");
    let user = h.mock.user_by_id(&id).unwrap();
    assert!(!user.inactive, "reactivated by the accept");
    assert_eq!(user.password, "Secret-pass1");
    assert!(user.verified);
}

#[tokio::test]
async fn accept_invite_weak_password_keeps_the_code() {
    let h = harness().await;
    let (id, code) = h.mock.add_invited_user("bob@example.com");
    let ar = h.mock.add_auth_request(false);
    let weak = h
        .post("/auth/v1/accept-invite", accept(&ar, &id, &code, "weak"), None, "1.1.1.1")
        .await;
    assert_eq!(weak.status, StatusCode::BAD_REQUEST);
    assert_eq!(weak.body["error"]["code"], "weak_password");
    let ok = h
        .post("/auth/v1/accept-invite", accept(&ar, &id, &code, "Secret-pass1"), None, "1.1.1.1")
        .await;
    assert_eq!(ok.status, StatusCode::OK, "{}", ok.body);
}

#[tokio::test]
async fn accept_invite_expired() {
    let h = harness().await;
    let (id, code) = h.mock.add_invited_user("bob@example.com");
    h.mock.expire_invite(&id);
    // Zitadel answers an expired code like a wrong one; the stored expiry tells them apart.
    h.internal.set_open_invite(&id, false);
    let a = h
        .post(
            "/auth/v1/accept-invite",
            accept(&h.mock.add_auth_request(false), &id, &code, "Secret-pass1"),
            None,
            "1.1.1.1",
        )
        .await;
    assert_eq!(a.status, StatusCode::GONE);
    assert_eq!(a.body["error"]["code"], "invite_expired");
    assert_eq!(h.mock.user_by_id(&id).unwrap().password, "");
}

#[tokio::test]
async fn accept_invite_revoked() {
    let h = harness().await;
    let (id, code) = h.mock.add_invited_user("bob@example.com");
    h.mock.remove_user(&id);
    let a = h
        .post(
            "/auth/v1/accept-invite",
            accept(&h.mock.add_auth_request(false), &id, &code, "Secret-pass1"),
            None,
            "1.1.1.1",
        )
        .await;
    assert_eq!(a.status, StatusCode::GONE);
    assert_eq!(a.body["error"]["code"], "invite_revoked");
}

#[tokio::test]
async fn accept_invite_rejects_malformed_input() {
    let h = harness().await;
    for body in [
        accept("V2_1", "not-a-number", "ABC", "Secret-pass1"),
        accept("V2_1", "30001", "", "Secret-pass1"),
    ] {
        let a = h.post("/auth/v1/accept-invite", body, None, "1.1.1.1").await;
        assert_eq!(a.status, StatusCode::BAD_REQUEST);
        assert_eq!(a.body["error"]["code"], "invalid_request");
    }
}

#[tokio::test]
async fn accept_invite_is_rate_limited_like_sign_up() {
    let h = harness().await;
    let (id, _) = h.mock.add_invited_user("bob@example.com");
    let mut last = StatusCode::OK;
    for _ in 0..6 {
        last = h
            .post(
                "/auth/v1/accept-invite",
                accept(&h.mock.add_auth_request(false), &id, "WRONG", "Secret-pass1"),
                None,
                "1.1.1.1",
            )
            .await
            .status;
    }
    assert_eq!(last, StatusCode::TOO_MANY_REQUESTS);
}

// A turned-away attempt spends nothing: after a burst over the budget in which no attempt
// failed, the address is not locked.
#[tokio::test(flavor = "multi_thread", worker_threads = 64)]
async fn accept_invite_concurrent_burst_of_valid_accepts_does_not_lock_the_ip() {
    let h = Arc::new(harness().await);
    h.mock.on_request("/invite_code/verify", || std::thread::sleep(Duration::from_millis(300)));
    let mut tasks = Vec::new();
    for i in 0..40 {
        let (id, code) = h.mock.add_invited_user(&format!("b{i}@example.com"));
        let ar = h.mock.add_auth_request(false);
        let h = h.clone();
        tasks.push(tokio::spawn(async move {
            h.post("/auth/v1/accept-invite", accept(&ar, &id, &code, "Secret-pass1"), None, "1.1.1.1")
                .await
                .status
        }));
    }
    let mut ok = 0;
    let mut limited = 0;
    for t in tasks {
        match t.await.unwrap() {
            StatusCode::OK => ok += 1,
            StatusCode::TOO_MANY_REQUESTS => limited += 1,
            other => panic!("unexpected {other}"),
        }
    }
    h.mock.clear_on_request();
    // The burst is over and not one attempt failed: a later valid accept must go through.
    let (id, code) = h.mock.add_invited_user("after@example.com");
    let a = h
        .post("/auth/v1/accept-invite", accept(&h.mock.add_auth_request(false), &id, &code, "Secret-pass1"), None, "1.1.1.1")
        .await;
    // The burst must actually reach the limit, or the check below proves nothing.
    assert!(limited > 0, "burst: {ok} ok, none got 429: the requests didn't overlap");
    assert_eq!(a.status, StatusCode::OK, "burst: {ok} ok, {limited} got 429; afterwards: {}", a.body);
}

// Only failed attempts spend the per-IP budget: a success needs a valid code for that user, so
// an office behind one NAT can accept many invites in a minute.
#[tokio::test]
async fn accept_invite_successes_do_not_spend_the_ip_budget() {
    let h = harness().await;
    for i in 0..25 {
        let (id, code) = h.mock.add_invited_user(&format!("u{i}@example.com"));
        let a = h
            .post(
                "/auth/v1/accept-invite",
                accept(&h.mock.add_auth_request(false), &id, &code, "Secret-pass1"),
                None,
                "1.1.1.1",
            )
            .await;
        assert_eq!(a.status, StatusCode::OK, "accept {i}: {}", a.body);
    }
}

#[tokio::test]
async fn accept_invite_failed_codes_exhaust_the_ip_budget() {
    let h = harness().await;
    for i in 0..20 {
        let (id, _) = h.mock.add_invited_user(&format!("u{i}@example.com"));
        let a = h
            .post(
                "/auth/v1/accept-invite",
                accept(&h.mock.add_auth_request(false), &id, "WRONG", "Secret-pass1"),
                None,
                "1.1.1.1",
            )
            .await;
        assert_eq!(a.status, StatusCode::BAD_REQUEST, "guess {i}");
    }
    let (id, code) = h.mock.add_invited_user("late@example.com");
    let late = h
        .post(
            "/auth/v1/accept-invite",
            accept(&h.mock.add_auth_request(false), &id, &code, "Secret-pass1"),
            None,
            "1.1.1.1",
        )
        .await;
    assert_eq!(late.status, StatusCode::TOO_MANY_REQUESTS);
    // Another address is untouched.
    let other = h
        .post(
            "/auth/v1/accept-invite",
            accept(&h.mock.add_auth_request(false), &id, &code, "Secret-pass1"),
            None,
            "2.2.2.2",
        )
        .await;
    assert_eq!(other.status, StatusCode::OK, "{}", other.body);
}

#[tokio::test]
async fn accept_invite_unknown_users_and_bad_bodies_count() {
    for malformed in [false, true] {
        let h = harness().await;
        for i in 0..20 {
            let body = if malformed {
                accept("V2_1", "not-a-number", "ABC", "Secret-pass1")
            } else {
                accept(&h.mock.add_auth_request(false), &format!("9000{i}"), "ABC", "Secret-pass1")
            };
            let a = h.post("/auth/v1/accept-invite", body, None, "1.1.1.1").await;
            let want = if malformed { StatusCode::BAD_REQUEST } else { StatusCode::GONE };
            assert_eq!(a.status, want, "attempt {i}: {}", a.body);
        }
        let (id, code) = h.mock.add_invited_user("late@example.com");
        let late = h
            .post(
                "/auth/v1/accept-invite",
                accept(&h.mock.add_auth_request(false), &id, &code, "Secret-pass1"),
                None,
                "1.1.1.1",
            )
            .await;
        assert_eq!(late.status, StatusCode::TOO_MANY_REQUESTS, "malformed: {malformed}");
    }
}

#[tokio::test]
async fn accept_invite_weak_password_and_outage_do_not_count() {
    // One more attempt than the budget (21) per case, each from its own address. Answers are
    // padded to about a second, so the cases that don't share a mock failure rule run side by
    // side. A valid accept from each address at the end proves its budget is untouched.
    let h = harness().await;
    let (id, code) = h.mock.add_invited_user("bob@example.com");
    let again = h.mock.add_user("again@example.com", "Old-pass-12", true);
    h.mock.fail("/invite_code/verify", 500, "Errors.Internal", 21);
    h.mock.fail("/password", 400, "Errors.User.PasswordComplexityPolicy.HasSymbol", 21);
    let weak = async {
        for i in 0..21 {
            let a = h
                .post(
                    "/auth/v1/accept-invite",
                    accept(&h.mock.add_auth_request(false), &id, &code, "weak"),
                    None,
                    "1.1.1.1",
                )
                .await;
            assert_eq!(a.body["error"]["code"], "weak_password", "weak {i}");
        }
    };
    let verify_outage = async {
        for i in 0..21 {
            let a = h
                .post(
                    "/auth/v1/accept-invite",
                    accept(&h.mock.add_auth_request(false), &id, &code, "Secret-pass1"),
                    None,
                    "2.2.2.2",
                )
                .await;
            assert_eq!(a.status, StatusCode::SERVICE_UNAVAILABLE, "outage {i}: {}", a.body);
        }
    };
    // A reset code with a password Zitadel refuses: the code isn't spent and nothing is guessed.
    let refused = async {
        for i in 0..21 {
            let mut body = accept(&h.mock.add_auth_request(false), &again, RESET_CODE, "Secret-pass1");
            body["reset"] = json!(true);
            let a = h.post("/auth/v1/accept-invite", body, None, "3.3.3.3").await;
            assert_eq!(a.body["error"]["code"], "weak_password", "refused {i}: {}", a.body);
        }
    };
    tokio::join!(weak, verify_outage, refused);
    // The user lookup fails for everyone, so this case runs alone.
    h.mock.fail("GET /v2/users/", 500, "Errors.Internal", 21);
    for i in 0..21 {
        let a = h
            .post(
                "/auth/v1/accept-invite",
                accept(&h.mock.add_auth_request(false), &id, &code, "Secret-pass1"),
                None,
                "4.4.4.4",
            )
            .await;
        assert_eq!(a.status, StatusCode::SERVICE_UNAVAILABLE, "get_user outage {i}: {}", a.body);
    }
    for ip in ["1.1.1.1", "2.2.2.2", "3.3.3.3", "4.4.4.4"] {
        let (id, code) = h.mock.add_invited_user(&format!("last-{ip}@example.com"));
        let ok = h
            .post(
                "/auth/v1/accept-invite",
                accept(&h.mock.add_auth_request(false), &id, &code, "Secret-pass1"),
                None,
                ip,
            )
            .await;
        assert_eq!(ok.status, StatusCode::OK, "{ip}: {}", ok.body);
    }
}

/// A user to accept for: an invitee, or for `reset` a user with a pending reset code.
fn code_holder(h: &Harness, email: &str, reset: bool) -> (String, String) {
    if reset {
        (h.mock.add_user(email, "Old-pass-12", true), RESET_CODE.to_string())
    } else {
        h.mock.add_invited_user(email)
    }
}

fn accept_or_reset(h: &Harness, id: &str, code: &str, reset: bool) -> Value {
    let mut body = accept(&h.mock.add_auth_request(false), id, code, "Secret-pass1");
    if reset {
        body["reset"] = json!(true);
    }
    body
}

#[tokio::test]
async fn accept_invite_any_rejected_code_check_counts_against_the_ip() {
    // Zitadel answering about the code with a status that isn't 400/412 (here 403) is still a
    // refused guess, on the invite path and on the reset path. Five per user (their own limit),
    // four users: the address's 20.
    for reset in [false, true] {
        let h = harness().await;
        h.mock.fail(if reset { "/password" } else { "/invite_code/verify" }, 403, "Errors.Forbidden", 20);
        for u in 0..4 {
            let (id, code) = code_holder(&h, &format!("bob{u}@example.com"), reset);
            for i in 0..5 {
                let a = h.post("/auth/v1/accept-invite", accept_or_reset(&h, &id, &code, reset), None, "1.1.1.1").await;
                assert_eq!(a.status, StatusCode::SERVICE_UNAVAILABLE, "reset {reset}, user {u}, attempt {i}: {}", a.body);
            }
        }
        let (id, code) = code_holder(&h, "eve@example.com", reset);
        let late = h.post("/auth/v1/accept-invite", accept_or_reset(&h, &id, &code, reset), None, "1.1.1.1").await;
        assert_eq!(late.status, StatusCode::TOO_MANY_REQUESTS, "reset {reset}");
    }
}

// Epic review (5): a refusal of the code check that isn't a wrong code by its captured id never
// hands the invitee's attempt back, so from new addresses a sixth is still refused (fail closed).
#[tokio::test]
async fn accept_invite_any_rejected_code_check_counts_against_the_user() {
    for reset in [false, true] {
        let h = harness().await;
        let (id, code) = code_holder(&h, "bob@example.com", reset);
        h.mock.fail(if reset { "/password" } else { "/invite_code/verify" }, 403, "Errors.Forbidden", 20);
        for i in 0..5 {
            let ip = format!("1.1.1.{i}");
            let a = h.post("/auth/v1/accept-invite", accept_or_reset(&h, &id, &code, reset), None, &ip).await;
            assert_eq!(a.status, StatusCode::SERVICE_UNAVAILABLE, "reset {reset}, attempt {i}: {}", a.body);
        }
        let sixth = h.post("/auth/v1/accept-invite", accept_or_reset(&h, &id, &code, reset), None, "2.2.2.2").await;
        assert_eq!(sixth.status, StatusCode::TOO_MANY_REQUESTS, "reset {reset}: {}", sixth.body);
    }
}

#[tokio::test]
async fn accept_invite_user_deleted_after_lookup_counts_against_the_ip() {
    // get_user finds the user but /invite_code/verify says Errors.User.NotFound (deleted in
    // between): 410 and a counted failure.
    let h = harness().await;
    let (id, code) = h.mock.add_invited_user("bob@example.com");
    h.mock.fail("/invite_code/verify", 400, "Errors.User.NotFound", 20);
    for i in 0..20 {
        let a = h
            .post(
                "/auth/v1/accept-invite",
                accept(&h.mock.add_auth_request(false), &id, &code, "Secret-pass1"),
                None,
                "1.1.1.1",
            )
            .await;
        assert_eq!(a.status, StatusCode::GONE, "attempt {i}: {}", a.body);
    }
    let late = h
        .post(
            "/auth/v1/accept-invite",
            accept(&h.mock.add_auth_request(false), &id, &code, "Secret-pass1"),
            None,
            "1.1.1.1",
        )
        .await;
    assert_eq!(late.status, StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn accept_invite_reset_user_deleted_after_lookup_is_revoked_and_counts_against_the_ip() {
    // get_user finds the user but the reset's POST /password answers Zitadel's captured
    // not-found (deleted in between): 410 invite_revoked, the invitee's attempt handed back (no
    // 429 after five), the address's counted (the 21st is 429).
    let h = harness().await;
    let id = h.mock.add_user("bob@example.com", "Old-pass-12", true);
    h.mock.fail("/password", 400, "Errors.User.NotFound", 20);
    let reset = || reset_accept(&h.mock.add_auth_request(false), &id, RESET_CODE, "Secret-pass1");
    for i in 0..20 {
        let a = h.post("/auth/v1/accept-invite", reset(), None, "1.1.1.1").await;
        assert_eq!(a.status, StatusCode::GONE, "attempt {i}: {}", a.body);
        assert_eq!(a.body["error"]["code"], "invite_revoked", "attempt {i}");
    }
    let late = h.post("/auth/v1/accept-invite", reset(), None, "1.1.1.1").await;
    assert_eq!(late.status, StatusCode::TOO_MANY_REQUESTS, "{}", late.body);
}

#[tokio::test]
async fn accept_invite_concurrent_failures_cannot_exceed_the_ip_budget() {
    let h = Arc::new(harness().await);
    let mut tasks = Vec::new();
    for i in 0..60 {
        let h = h.clone();
        tasks.push(tokio::spawn(async move {
            h.post(
                "/auth/v1/accept-invite",
                accept("V2_1", &format!("8000{i}"), "WRONG", "Secret-pass1"),
                None,
                "6.6.6.6",
            )
            .await
            .status
        }));
    }
    let mut answered = 0;
    for t in tasks {
        if t.await.unwrap() != StatusCode::TOO_MANY_REQUESTS {
            answered += 1;
        }
    }
    assert!(answered <= 20, "{answered} of 60 concurrent failed attempts from one IP were answered");
}

fn ticket_of(a: &Answer) -> String {
    a.body["error"]["accept_ticket"].as_str().unwrap().to_string()
}

fn with_ticket(ar: &str, user: &str, ticket: &str, pw: &str) -> Value {
    json!({"authRequest": ar, "user": user, "acceptTicket": ticket, "password": pw})
}

#[tokio::test]
async fn accept_invite_failure_after_verify_returns_ticket() {
    let h = harness().await;
    let (id, code) = h.mock.add_invited_user("bob@example.com");
    h.mock.fail("/password", 500, "Errors.Internal", 1);
    let a = h
        .post(
            "/auth/v1/accept-invite",
            accept(&h.mock.add_auth_request(false), &id, &code, "Secret-pass1"),
            None,
            "1.1.1.1",
        )
        .await;
    assert_eq!(a.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(a.body["error"]["code"], "invite_incomplete");
    assert!(!ticket_of(&a).is_empty());
    // The code is spent: retrying with it alone is refused.
    let again = h
        .post(
            "/auth/v1/accept-invite",
            accept(&h.mock.add_auth_request(false), &id, &code, "Secret-pass1"),
            None,
            "1.1.1.1",
        )
        .await;
    assert_eq!(again.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn accept_invite_retry_with_ticket_completes() {
    let h = harness().await;
    let (id, code) = h.mock.add_invited_user("bob@example.com");
    h.mock.fail("PATCH /v2/users/", 500, "Errors.Internal", 1);
    let a = h
        .post(
            "/auth/v1/accept-invite",
            accept(&h.mock.add_auth_request(false), &id, &code, "Secret-pass1"),
            None,
            "1.1.1.1",
        )
        .await;
    assert_eq!(a.body["error"]["code"], "invite_incomplete");
    let ticket = ticket_of(&a);
    let ok = h
        .post(
            "/auth/v1/accept-invite",
            with_ticket(&h.mock.add_auth_request(false), &id, &ticket, "Secret-pass1"),
            None,
            "1.1.1.1",
        )
        .await;
    assert_eq!(ok.status, StatusCode::OK, "{}", ok.body);
    assert_eq!(ok.body["next"], "done");
    assert!(ok.cookie("chelabase_session").is_some());
    let user = h.mock.user_by_id(&id).unwrap();
    assert_eq!((user.password.as_str(), user.verified), ("Secret-pass1", true));
}

#[tokio::test]
async fn ticket_for_other_user_refused() {
    let h = harness().await;
    let (a_id, a_code) = h.mock.add_invited_user("a@example.com");
    let (b_id, _) = h.mock.add_invited_user("b@example.com");
    h.mock.fail("/password", 500, "Errors.Internal", 1);
    let failed = h
        .post(
            "/auth/v1/accept-invite",
            accept(&h.mock.add_auth_request(false), &a_id, &a_code, "Secret-pass1"),
            None,
            "1.1.1.1",
        )
        .await;
    let ticket = ticket_of(&failed);
    let a = h
        .post(
            "/auth/v1/accept-invite",
            with_ticket(&h.mock.add_auth_request(false), &b_id, &ticket, "Secret-pass1"),
            None,
            "1.1.1.1",
        )
        .await;
    assert_eq!(a.status, StatusCode::BAD_REQUEST);
    assert_eq!(a.body["error"]["code"], "invalid_request");
    assert_eq!(h.mock.user_by_id(&b_id).unwrap().password, "");
    assert!(a.cookie("chelabase_session").is_none());
}

#[tokio::test]
async fn expired_ticket_refused() {
    let h = harness().await;
    let (id, _) = h.mock.add_invited_user("bob@example.com");
    let old = auth::cookie::seal_ticket(
        &h.cookie_key,
        &auth::cookie::AcceptTicket { user_id: id.clone(), exp: auth::cookie::now_secs() - 1, nonce: "n1".into() },
    );
    let a = h
        .post(
            "/auth/v1/accept-invite",
            with_ticket(&h.mock.add_auth_request(false), &id, &old, "Secret-pass1"),
            None,
            "1.1.1.1",
        )
        .await;
    assert_eq!(a.status, StatusCode::BAD_REQUEST);
    assert_eq!(h.mock.user_by_id(&id).unwrap().password, "");
    // A ticket sealed with another key is refused alike, and a code together with a ticket.
    let foreign = auth::cookie::seal_ticket(
        &SecretKey::generate(),
        &auth::cookie::AcceptTicket { user_id: id.clone(), exp: auth::cookie::now_secs() + 600, nonce: "n2".into() },
    );
    let b = h
        .post(
            "/auth/v1/accept-invite",
            with_ticket(&h.mock.add_auth_request(false), &id, &foreign, "Secret-pass1"),
            None,
            "1.1.1.1",
        )
        .await;
    assert_eq!(b.status, StatusCode::BAD_REQUEST);
    let both = h
        .post(
            "/auth/v1/accept-invite",
            json!({"user": id, "code": "X", "acceptTicket": foreign, "password": "Secret-pass1"}),
            None,
            "1.1.1.1",
        )
        .await;
    assert_eq!(both.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn accept_invite_without_the_admin_client_is_503_and_spends_no_code() {
    let h = harness_full(false, 1, auth::config::Mode::SelfHosted, false).await;
    let (id, code) = h.mock.add_invited_user("bob@example.com");
    let a = h
        .post(
            "/auth/v1/accept-invite",
            accept(&h.mock.add_auth_request(false), &id, &code, "Secret-pass1"),
            None,
            "1.1.1.1",
        )
        .await;
    assert_eq!(a.status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(
        !h.mock.calls().iter().any(|c| c.contains("invite_code/verify")),
        "{:?}",
        h.mock.calls()
    );
    assert!(h.mock.inner.lock().unwrap().invites.contains_key(&id));
}

#[tokio::test]
async fn verify_errors_map_by_the_captured_refusal() {
    let h = harness().await;
    // 401, 403, 429, a missing route and a 5xx are ours or transient: 503, never "invalid link".
    for (status, key) in [
        (401, "Errors.Token.Invalid"),
        (403, "Errors.PermissionDenied"),
        (429, "Errors.RateLimit"),
        (404, "Errors.Route.NotFound"),
        (500, "Errors.Internal"),
    ] {
        let (id, code) = h.mock.add_invited_user(&format!("u{status}@example.com"));
        h.mock.fail("/invite_code/verify", status, key, 1);
        let a = h
            .post(
                "/auth/v1/accept-invite",
                accept(&h.mock.add_auth_request(false), &id, &code, "Secret-pass1"),
                None,
                "1.1.1.1",
            )
            .await;
        assert_eq!(a.status, StatusCode::SERVICE_UNAVAILABLE, "{status}: {}", a.body);
        assert_eq!(a.body["error"]["code"], "unavailable", "{status}");
        assert_eq!(h.mock.user_by_id(&id).unwrap().password, "", "{status}");
    }
    // Any other 400 is a bad link.
    let (id, code) = h.mock.add_invited_user("other@example.com");
    h.mock.fail("/invite_code/verify", 400, "Errors.Something.Else", 1);
    let a = h
        .post(
            "/auth/v1/accept-invite",
            accept(&h.mock.add_auth_request(false), &id, &code, "Secret-pass1"),
            None,
            "1.1.1.1",
        )
        .await;
    assert_eq!((a.status, a.body["error"]["code"].clone()), (StatusCode::BAD_REQUEST, json!("invalid_request")));
    // Zitadel's invalid-code refusal (`COMMAND-Wgn4q`, a wrong or an expired code alike) is an
    // expired invite when the stored invite has expired; its user-not-found (`COMMAND-F5g2h`, a
    // 400) is a revoked one.
    h.internal.set_open_invite(&id, false);
    h.mock.fail("/invite_code/verify", 400, "Errors.User.Code.Expired", 1);
    let b = h
        .post(
            "/auth/v1/accept-invite",
            accept(&h.mock.add_auth_request(false), &id, &code, "Secret-pass1"),
            None,
            "1.1.1.1",
        )
        .await;
    assert_eq!(b.body["error"]["code"], "invite_expired");
    h.mock.fail("/invite_code/verify", 400, "Errors.User.NotFound", 1);
    let c = h
        .post(
            "/auth/v1/accept-invite",
            accept(&h.mock.add_auth_request(false), &id, &code, "Secret-pass1"),
            None,
            "1.1.1.1",
        )
        .await;
    assert_eq!(c.body["error"]["code"], "invite_revoked");
}

#[tokio::test]
async fn set_password_maps_only_the_policy_key_to_weak_password() {
    let h = harness().await;
    let (id, code) = h.mock.add_invited_user("bob@example.com");
    h.mock.fail("/password", 400, "Errors.User.PasswordComplexityPolicy.HasSymbol", 1);
    let weak = h
        .post(
            "/auth/v1/accept-invite",
            accept(&h.mock.add_auth_request(false), &id, &code, "Secret-pass1"),
            None,
            "1.1.1.1",
        )
        .await;
    assert_eq!(weak.status, StatusCode::BAD_REQUEST);
    assert_eq!(weak.body["error"]["code"], "weak_password");
    let ticket = ticket_of(&weak);
    // Another 400 is not the password's fault.
    h.mock.fail("/password", 400, "Errors.Something.Else", 1);
    let other = h
        .post(
            "/auth/v1/accept-invite",
            with_ticket(&h.mock.add_auth_request(false), &id, &ticket, "Secret-pass1"),
            None,
            "1.1.1.1",
        )
        .await;
    assert_eq!(other.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(other.body["error"]["code"], "invite_incomplete");
    let ok = h
        .post(
            "/auth/v1/accept-invite",
            with_ticket(&h.mock.add_auth_request(false), &id, &ticket_of(&other), "Better-pass1!"),
            None,
            "1.1.1.1",
        )
        .await;
    assert_eq!(ok.status, StatusCode::OK, "{}", ok.body);
}

#[tokio::test]
async fn an_expired_sign_in_still_completes_the_accept() {
    let h = harness().await;
    let (id, code) = h.mock.add_invited_user("bob@example.com");
    let a = h
        .post(
            "/auth/v1/accept-invite",
            accept("V2_999999", &id, &code, "Secret-pass1"),
            None,
            "1.1.1.1",
        )
        .await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.body);
    assert_eq!(a.body, json!({"next": "done", "redirect": "http://studio.chelabase.localhost:8000"}));
    assert!(a.cookie("chelabase_session").is_some());
    assert_eq!(h.mock.user_by_id(&id).unwrap().password, "Secret-pass1");
}

#[tokio::test]
async fn accept_invite_for_another_orgs_user_is_refused_and_keeps_the_code() {
    let h = harness().await;
    let id = h.mock.add_user_in("o2", "app@example.com", "", false);
    h.mock
        .inner
        .lock()
        .unwrap()
        .invites
        .insert(id.clone(), ("CODE1".into(), false));
    let a = h
        .post(
            "/auth/v1/accept-invite",
            accept(&h.mock.add_auth_request(false), &id, "CODE1", "Secret-pass1"),
            None,
            "1.1.1.1",
        )
        .await;
    assert_eq!(a.status, StatusCode::BAD_REQUEST);
    assert_eq!(h.mock.user_by_id(&id).unwrap().password, "");
    assert!(h.mock.inner.lock().unwrap().invites.contains_key(&id));
}

#[tokio::test]
async fn accept_invite_never_touches_an_active_user_or_another_users_code() {
    let h = harness().await;
    let active = h.mock.add_user("active@example.com", "Original-pass1", true);
    let a = h
        .post(
            "/auth/v1/accept-invite",
            accept(&h.mock.add_auth_request(false), &active, "ANYCODE", "Hijack-pass1"),
            None,
            "1.1.1.1",
        )
        .await;
    assert_eq!(a.status, StatusCode::BAD_REQUEST);
    assert_eq!(h.mock.user_by_id(&active).unwrap().password, "Original-pass1");
    assert!(a.cookie("chelabase_session").is_none());
    // A's code against B's id.
    let (a_id, a_code) = h.mock.add_invited_user("a@example.com");
    let (b_id, _) = h.mock.add_invited_user("b@example.com");
    let cross = h
        .post(
            "/auth/v1/accept-invite",
            accept(&h.mock.add_auth_request(false), &b_id, &a_code, "Secret-pass1"),
            None,
            "1.1.1.1",
        )
        .await;
    assert_eq!(cross.status, StatusCode::BAD_REQUEST);
    assert_eq!(h.mock.user_by_id(&b_id).unwrap().password, "");
    assert!(h.mock.inner.lock().unwrap().invites.contains_key(&a_id));
}

#[tokio::test]
async fn accept_invite_is_limited_per_ip_across_users() {
    let h = harness().await;
    let mut statuses = Vec::new();
    for i in 0..21 {
        statuses.push(
            h.post(
                "/auth/v1/accept-invite",
                accept("V2_1", &format!("4000{i}"), "WRONG", "Secret-pass1"),
                None,
                "2.2.2.2",
            )
            .await
            .status,
        );
    }
    assert!(statuses[..20].iter().all(|s| *s != StatusCode::TOO_MANY_REQUESTS));
    assert_eq!(statuses[20], StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn only_failed_codes_count_against_an_invitee() {
    let h = harness().await;
    let (id, code) = h.mock.add_invited_user("bob@example.com");
    // Weak-password typos don't count.
    for _ in 0..7 {
        let a = h
            .post(
                "/auth/v1/accept-invite",
                accept("V2_1", &id, &code, "weak"),
                None,
                "3.3.3.3",
            )
            .await;
        assert_eq!(a.body["error"]["code"], "weak_password");
    }
    let ok = h
        .post(
            "/auth/v1/accept-invite",
            accept(&h.mock.add_auth_request(false), &id, &code, "Secret-pass1"),
            None,
            "3.3.3.3",
        )
        .await;
    assert_eq!(ok.status, StatusCode::OK, "{}", ok.body);
}

#[tokio::test]
async fn the_gate_refuses_what_it_does_not_clearly_allow() {
    let h = harness().await;
    let body = || signup("new@example.com", "Secret-pass1");
    // An odd reason with allowed:true is still allowed.
    h.internal.set_signup(true, "something_new");
    assert_eq!(
        h.post("/auth/v1/sign-up", body(), None, "1.1.1.1").await.status,
        StatusCode::OK
    );
    // The orchestrator saying "platform" is the platform error.
    h.internal.set_signup(false, "platform");
    let a = h.post("/auth/v1/sign-up", body(), None, "1.1.1.2").await;
    assert_eq!(a.body["error"]["code"], "not_available_in_platform_mode");
    // A 4xx from the gate (a wrong token) is unavailability, not an open door.
    h.internal.set_signup_status(Some(401));
    let b = h.post("/auth/v1/sign-up", body(), None, "1.1.1.3").await;
    assert_eq!(b.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(h.users_with("new@example.com").await, 1);
}

#[tokio::test]
async fn ticket_replay_after_success_refused() {
    let h = harness().await;
    let (id, code) = h.mock.add_invited_user("bob@example.com");
    h.mock.fail("/password", 500, "Errors.Internal", 1);
    let failed = h
        .post(
            "/auth/v1/accept-invite",
            accept(&h.mock.add_auth_request(false), &id, &code, "Secret-pass1"),
            None,
            "1.1.1.1",
        )
        .await;
    let ticket = ticket_of(&failed);
    let ok = h
        .post(
            "/auth/v1/accept-invite",
            with_ticket(&h.mock.add_auth_request(false), &id, &ticket, "Secret-pass1"),
            None,
            "1.1.1.1",
        )
        .await;
    assert_eq!(ok.status, StatusCode::OK, "{}", ok.body);
    // The same ticket can't set the password again.
    let replay = h
        .post(
            "/auth/v1/accept-invite",
            with_ticket(&h.mock.add_auth_request(false), &id, &ticket, "Attacker-pass1"),
            None,
            "1.1.1.1",
        )
        .await;
    assert_eq!(replay.status, StatusCode::BAD_REQUEST);
    assert_eq!(replay.body["error"]["code"], "invalid_request");
    assert!(replay.cookie("chelabase_session").is_none());
    assert_eq!(h.mock.user_by_id(&id).unwrap().password, "Secret-pass1");
}

#[tokio::test]
async fn accept_invite_answers_are_never_cached() {
    let h = harness().await;
    let (id, code) = h.mock.add_invited_user("bob@example.com");
    h.mock.fail("/password", 500, "Errors.Internal", 1);
    let post = |body: Value| {
        h.http
            .post(format!("{}/auth/v1/accept-invite", h.base))
            .json(&body)
            .header("x-forwarded-for", "9.9.9.9, 1.1.1.1")
            .send()
    };
    let failed = post(accept("V2_1", &id, &code, "Secret-pass1")).await.unwrap();
    assert_eq!(failed.headers().get("cache-control").unwrap(), "no-store");
    let failed: Value = failed.json().await.unwrap();
    let ticket = failed["error"]["accept_ticket"].as_str().unwrap().to_string();
    let ok = post(with_ticket(&h.mock.add_auth_request(false), &id, &ticket, "Secret-pass1"))
        .await
        .unwrap();
    assert_eq!(ok.status(), StatusCode::OK);
    assert_eq!(ok.headers().get("cache-control").unwrap(), "no-store");
    let bad = post(accept("V2_1", "not-a-number", "X", "Secret-pass1")).await.unwrap();
    assert_eq!(bad.headers().get("cache-control").unwrap(), "no-store");
}

fn cookie_value(a: &Answer, name: &str) -> String {
    a.cookie(name).unwrap().split_once('=').unwrap().1.to_string()
}

#[tokio::test]
async fn tickets_and_cookies_are_not_interchangeable() {
    let h = harness().await;
    // A real session cookie (from an accept) and a real login cookie (from a sign-up).
    let (id, code) = h.mock.add_invited_user("bob@example.com");
    let done = h
        .post(
            "/auth/v1/accept-invite",
            accept(&h.mock.add_auth_request(false), &id, &code, "Secret-pass1"),
            None,
            "1.1.1.1",
        )
        .await;
    let session = cookie_value(&done, "chelabase_session");
    let up = h
        .post(
            "/auth/v1/sign-up",
            signup("new@example.com", "Secret-pass1"),
            None,
            "1.1.1.2",
        )
        .await;
    let login = cookie_value(&up, "chelabase_login");
    let (other, _) = h.mock.add_invited_user("carol@example.com");
    for value in [session, login] {
        let a = h
            .post(
                "/auth/v1/accept-invite",
                with_ticket(&h.mock.add_auth_request(false), &other, &value, "Secret-pass1"),
                None,
                "1.1.1.3",
            )
            .await;
        assert_eq!(a.status, StatusCode::BAD_REQUEST);
        assert_eq!(h.mock.user_by_id(&other).unwrap().password, "");
    }
    // A ticket presented as a cookie opens nothing.
    h.mock.fail("/password", 500, "Errors.Internal", 1);
    let (id2, code2) = h.mock.add_invited_user("dan@example.com");
    let failed = h
        .post(
            "/auth/v1/accept-invite",
            accept(&h.mock.add_auth_request(false), &id2, &code2, "Secret-pass1"),
            None,
            "1.1.1.4",
        )
        .await;
    let ticket = ticket_of(&failed);
    let as_login = h
        .post(
            "/auth/v1/verify-email",
            json!({"code": "123456"}),
            Some(&format!("chelabase_login={ticket}")),
            "1.1.1.4",
        )
        .await;
    assert_eq!(as_login.status, StatusCode::GONE);
    let before = h.mock.calls().iter().filter(|c| c.starts_with("DELETE /v2/sessions")).count();
    h.post(
        "/auth/v1/logout",
        json!({}),
        Some(&format!("chelabase_session={ticket}")),
        "1.1.1.4",
    )
    .await;
    let after = h.mock.calls().iter().filter(|c| c.starts_with("DELETE /v2/sessions")).count();
    assert_eq!(before, after);
    // Control: a real session cookie does end its session.
    h.post(
        "/auth/v1/logout",
        json!({}),
        Some(&format!("chelabase_session={}", cookie_value(&done, "chelabase_session"))),
        "1.1.1.4",
    )
    .await;
    let ended = h.mock.calls().iter().filter(|c| c.starts_with("DELETE /v2/sessions")).count();
    assert_eq!(ended, after + 1);
}

#[tokio::test]
async fn zitadel_error_keys_match_exactly() {
    let h = harness().await;
    // Keys that merely contain a known key are not that key.
    let (id, code) = h.mock.add_invited_user("bob@example.com");
    for (key, expect) in [
        ("Errors.User.Code.ExpiredToken", "invalid_request"),
        ("Errors.User.NotFound.Org", "invalid_request"),
    ] {
        h.mock.fail("/invite_code/verify", 400, key, 1);
        let a = h
            .post(
                "/auth/v1/accept-invite",
                accept(&h.mock.add_auth_request(false), &id, &code, "Secret-pass1"),
                None,
                "1.1.1.1",
            )
            .await;
        assert_eq!(a.body["error"]["code"], expect, "{key}");
    }
    h.mock.fail("/password", 400, "Errors.Org.PasswordComplexityPolicy.Other", 1);
    let a = h
        .post(
            "/auth/v1/accept-invite",
            accept(&h.mock.add_auth_request(false), &id, &code, "Secret-pass1"),
            None,
            "1.1.1.1",
        )
        .await;
    assert_eq!(a.body["error"]["code"], "invite_incomplete");
}

// T12x (R32): the accept page describes an invite by (Zitadel user, link token) alone.
fn token_hash_hex(token: &str) -> String {
    use sha2::Digest;
    hex::encode(sha2::Sha256::digest(token.as_bytes()))
}

const INVITE_TOKEN: &str = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNO-_";

/// M8: the description is asked for with a POST body, so neither the user id nor the token
/// is in a URL (access logs, browser history, referrers). The answer carries its cache header
/// as `_cache_control`.
impl Harness {
    async fn describe(&self, body: Value, ip: &str) -> Answer {
        let resp = self
            .http
            .post(format!("{}/auth/v1/invite", self.base))
            .json(&body)
            .header("x-forwarded-for", format!("9.9.9.9, {ip}"))
            .send()
            .await
            .unwrap();
        let status = resp.status();
        let cache = resp.headers().get("cache-control").map(|v| v.to_str().unwrap().to_string());
        let mut body: Value = serde_json::from_str(&resp.text().await.unwrap()).unwrap_or(Value::Null);
        if let (Some(cache), Value::Object(map)) = (cache, &mut body) {
            map.insert("_cache_control".into(), json!(cache));
        }
        Answer { status, body, cookies: Vec::new() }
    }
}

#[tokio::test]
async fn describe_invite_returns_the_description_for_the_link_token() {
    assert_eq!(INVITE_TOKEN.len(), 43);
    let h = harness().await;
    let want = json!({"inviter_name": "Alice", "inviter_email": "alice@x.com", "tenant_name": "acme"});
    h.internal.set_invite("123", &token_hash_hex(INVITE_TOKEN), want.clone());
    let a = h.describe(json!({"user": "123", "t": INVITE_TOKEN}), "1.1.1.1").await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.body);
    assert_eq!(a.body["inviter_name"], "Alice");
    assert_eq!(a.body["inviter_email"], "alice@x.com");
    assert_eq!(a.body["tenant_name"], "acme");
    assert_eq!(a.body["_cache_control"], "no-store");
    // The old GET is gone: a link's secrets never go in a URL.
    let old = h.get(&format!("/auth/v1/invite?user=123&t={INVITE_TOKEN}"), "1.1.1.1").await;
    assert_eq!(old.status, StatusCode::METHOD_NOT_ALLOWED);
    // The orchestrator was asked with the hash, never the token itself, and not in the URL.
    let bodies = h.internal.bodies("POST /internal/invites/describe");
    assert_eq!(bodies, vec![json!({"user": "123", "token_hash": token_hash_hex(INVITE_TOKEN)})]);
    assert!(!bodies[0].to_string().contains(INVITE_TOKEN));
    assert_eq!(h.internal.queries("POST /internal/invites/describe"), vec![String::new()]);
}

#[tokio::test]
async fn describe_invite_wrong_anything_is_the_same_404() {
    let h = harness().await;
    h.internal.set_invite("123", &token_hash_hex(INVITE_TOKEN), json!({"inviter_name": "A", "inviter_email": null, "tenant_name": "acme"}));
    let other = "ZYXWVUTSRQPONMLKJIHGFEDCBAzyxwvutsrqponml-_";
    assert_eq!(other.len(), 43);
    let short = &INVITE_TOKEN[..42];
    let mut answers = Vec::new();
    for body in [
        json!({"user": "123", "t": other}),
        json!({"user": "124", "t": INVITE_TOKEN}),
        json!({"user": "123", "t": short}),
        json!({"user": "123"}),
        json!({}),
        json!({"user": "12x", "t": INVITE_TOKEN}),
        json!({"user": "123", "t": format!("{}+", &INVITE_TOKEN[..42])}),
    ] {
        let a = h.describe(body.clone(), "1.1.1.1").await;
        assert_eq!(a.status, StatusCode::NOT_FOUND, "{body}: {}", a.body);
        answers.push(a.body);
    }
    assert!(answers.windows(2).all(|w| w[0]["error"] == w[1]["error"]), "{answers:?}");
    assert_eq!(answers[0]["error"]["code"], "not_found");
    // Only the two well-formed lookups reached the orchestrator.
    assert_eq!(h.internal.bodies("POST /internal/invites/describe").len(), 2);
}

#[tokio::test]
async fn describe_invite_is_rate_limited_and_fails_closed() {
    let h = harness().await;
    let body = json!({"user": "123", "t": INVITE_TOKEN});
    for _ in 0..60 {
        let a = h.describe(body.clone(), "2.2.2.2").await;
        assert_eq!(a.status, StatusCode::NOT_FOUND);
    }
    let limited = h.describe(body.clone(), "2.2.2.2").await;
    assert_eq!(limited.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(limited.body["error"]["code"], "rate_limited");
    // Another address is counted on its own; an orchestrator failure is 503, not a 404.
    h.internal.set_describe_status(Some(500));
    let down = h.describe(body, "3.3.3.3").await;
    assert_eq!(down.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(down.body["error"]["code"], "unavailable");
}

// Selfhost-install m3: an unclaimed instance's sign-up needs the setup code, and the account
// it creates is bound to the fresh-install claim (or deleted when that fails).

const SETUP_CODE: &str = "ABCD-EFGH-JKMN-PQRS-TUVW";
const CLAIMED_CALL: &str = "POST /internal/signup/claimed";

fn signup_with_code(email: &str, code: Option<&str>) -> Value {
    let mut body = signup(email, "Secret-pass1");
    if let Some(code) = code {
        body["setupCode"] = json!(code);
    }
    body
}

// AC 1: no or a wrong code: 403 `setup_code_invalid`, no Zitadel user, no claim.
#[tokio::test]
async fn sign_up_with_bad_code_creates_no_user() {
    let h = harness().await;
    h.internal.set_setup_code(SETUP_CODE);
    for (i, code) in [None, Some(""), Some("WRONG-CODE")].into_iter().enumerate() {
        let a = h
            .post("/auth/v1/sign-up", signup_with_code("first@example.com", code), None, &format!("1.1.1.{i}"))
            .await;
        assert_eq!(
            (a.status, a.body),
            (StatusCode::FORBIDDEN, err("setup_code_invalid", messages::SETUP_CODE_INVALID)),
            "{code:?}"
        );
    }
    assert_eq!(h.users_with("first@example.com").await, 0);
    assert!(h.identity.calls().iter().all(|c| c != "POST /internal/v1/pools/members/users"), "no create call");
    assert!(h.internal.bodies(CLAIMED_CALL).is_empty());
    // The code goes in the gate's body; a blank one is not sent at all.
    let asked = h.internal.bodies(SIGN_UP_CALL);
    assert_eq!(asked[0], json!({"org": "o1"}));
    assert_eq!(asked[1], json!({"org": "o1"}));
    assert_eq!(asked[2], json!({"org": "o1", "setup_code": "WRONG-CODE"}));
    assert!(h.internal.queries(SIGN_UP_CALL).iter().all(String::is_empty), "nothing in a URL");
}

// AC 2 (auth side): the right code creates the account and binds the claim to it with the
// gate's nonce; the sign-in page offers sign-up while the instance waits for its code.
#[tokio::test]
async fn sign_up_with_the_code_binds_the_claim() {
    let h = harness().await;
    h.internal.set_setup_code(SETUP_CODE);
    let a = h
        .post("/auth/v1/sign-up", signup_with_code("first@example.com", Some(SETUP_CODE)), None, "1.1.1.1")
        .await;
    assert_eq!((a.status, a.body), (StatusCode::OK, json!({"next": "verify"})));
    let user = h.identity.client().search_users(&Pool::Members, "first@example.com", 10, 0).await.unwrap().users.remove(0);
    assert_eq!(
        h.internal.bodies(CLAIMED_CALL),
        vec![json!({"org": "o1", "user_id": user.id, "nonce": support::internal::CLAIM_NONCE})]
    );
}

// AC 6 (auth side): the instance was claimed meanwhile (`bound: false`): the account stays.
#[tokio::test]
async fn an_unbound_claim_keeps_the_account() {
    let h = harness().await;
    h.internal.set_setup_code(SETUP_CODE);
    h.internal.set_claimed_unbound();
    let a = h
        .post("/auth/v1/sign-up", signup_with_code("second@example.com", Some(SETUP_CODE)), None, "1.1.1.1")
        .await;
    assert_eq!(a.status, StatusCode::OK);
    assert_eq!(h.users_with("second@example.com").await, 1);
}

// AC 10: `claimed` fails after the account was created: the account is deleted, and a retry
// with the same email succeeds.
// Without the admin client a failed claim couldn't be undone: no setup-code sign-up at all.
// The register page's question: `setupRequired` while the instance waits for its code.
#[tokio::test]
async fn instance_says_whether_setup_is_required() {
    let h = harness().await;
    let a = h.get("/auth/v1/instance", "1.1.1.1").await;
    assert_eq!((a.status, a.body), (StatusCode::OK, json!({"setupRequired": false})));
    h.internal.set_setup_code(SETUP_CODE);
    let a = h.get("/auth/v1/instance", "1.1.1.1").await;
    assert_eq!((a.status, a.body), (StatusCode::OK, json!({"setupRequired": true})));
    // The question carries no code.
    assert!(h.internal.bodies(SIGN_UP_CALL).iter().all(|b| *b == json!({"org": "o1"})));
    h.internal.set_signup_status(Some(503));
    assert_eq!(h.get("/auth/v1/instance", "1.1.1.1").await.status, StatusCode::SERVICE_UNAVAILABLE);
    // Rate-limited per client.
    h.internal.set_signup_status(None);
    for _ in 0..57 {
        assert_eq!(h.get("/auth/v1/instance", "1.1.1.1").await.status, StatusCode::OK);
    }
    assert_eq!(h.get("/auth/v1/instance", "1.1.1.1").await.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(h.get("/auth/v1/instance", "1.1.1.2").await.status, StatusCode::OK);
    // Platform mode: never, without asking.
    let p = harness_mode(false, 1, auth::config::Mode::Platform).await;
    let a = p.get("/auth/v1/instance", "1.1.1.1").await;
    assert_eq!((a.status, a.body), (StatusCode::OK, json!({"setupRequired": false})));
    assert!(p.internal.bodies(SIGN_UP_CALL).is_empty());
}

// B1 (m3 audit): there is no global setup-code bucket to drain. 300 junk sign-ups with a code
// from 15 addresses (each under its own limit) leave the operator's right-code sign-up alone;
// the per-address and per-email limits still apply. The code's ~98 bits need no global cap.
#[tokio::test]
async fn junk_setup_sign_ups_never_lock_out_the_code() {
    let h = harness().await;
    h.internal.set_setup_code(SETUP_CODE);
    let mut set = tokio::task::JoinSet::new();
    for i in 0..300 {
        let (http, base) = (h.http.clone(), h.base.clone());
        let body = signup_with_code(&format!("junk{i}@example.com"), Some("X"));
        set.spawn(async move {
            http.post(format!("{base}/auth/v1/sign-up"))
                .json(&body)
                .header("x-forwarded-for", format!("9.9.9.9, 10.0.1.{}", i % 15))
                .send()
                .await
                .unwrap()
                .status()
        });
    }
    let statuses = set.join_all().await;
    assert!(statuses.iter().all(|s| *s != StatusCode::TOO_MANY_REQUESTS), "{statuses:?}");
    let a = h
        .post("/auth/v1/sign-up", signup_with_code("owner@example.com", Some(SETUP_CODE)), None, "7.7.7.7")
        .await;
    assert_eq!(a.status, StatusCode::OK, "the operator's right-code sign-up: {}", a.body);
    // The per-address limit still holds for code guesses.
    for i in 0..20 {
        let a = h.post("/auth/v1/sign-up", signup_with_code(&format!("g{i}@example.com"), Some("X")), None, "8.8.8.8").await;
        assert_eq!(a.status, StatusCode::FORBIDDEN, "{i}");
    }
    let a = h.post("/auth/v1/sign-up", signup_with_code("g20@example.com", Some("X")), None, "8.8.8.8").await;
    assert_eq!(a.status, StatusCode::TOO_MANY_REQUESTS);
}

/// Every log line written on this thread while the guard lives (the test's current-thread
/// runtime runs the server's tasks here too).
#[derive(Clone, Default)]
struct LogSink(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for LogSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

// The code and the nonce never show in auth's logs or answers.
#[tokio::test]
async fn log_and_error_scrub() {
    let sink = LogSink::default();
    let writer = sink.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    let h = harness().await;
    h.internal.set_setup_code(SETUP_CODE);
    let wrong = "ZZZZ-YYYY-XXXX-WWWW-VVVV";
    let mut bodies = Vec::new();
    for (i, code) in [wrong, SETUP_CODE].into_iter().enumerate() {
        bodies.push(
            h.post("/auth/v1/sign-up", signup_with_code(&format!("u{i}@example.com"), Some(code)), None, "1.1.1.1")
                .await
                .body,
        );
    }
    h.internal.set_claimed_status(Some(403));
    bodies.push(
        h.post("/auth/v1/sign-up", signup_with_code("u9@example.com", Some(SETUP_CODE)), None, "1.1.1.1")
            .await
            .body,
    );
    bodies.push(h.get("/auth/v1/instance", "1.1.1.1").await.body);
    let logs = String::from_utf8(sink.0.lock().unwrap().clone()).unwrap();
    assert!(logs.contains("enrolling the new account failed"), "{logs}");
    for secret in [SETUP_CODE, wrong, support::internal::CLAIM_NONCE, "ABCDEFGHJKMNPQRSTUVW"] {
        assert!(!logs.to_lowercase().contains(&secret.to_lowercase()), "{secret} in the logs");
        for body in &bodies {
            assert!(!body.to_string().contains(secret), "{secret} in {body}");
        }
    }
}

// ---- Selfhost-install m5: the bootstrap's outputs from files ----

fn write_id(path: &std::path::Path, value: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, value).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644)).unwrap();
}

fn org_file(path: &std::path::Path) -> chelabase_types::file_value::FileValue {
    chelabase_types::file_value::FileValue::file(
        "CHELABASE_ZITADEL_ORG_ID_FILE",
        path,
        chelabase_types::file_value::Sensitivity::Public,
    )
}

// AC 1: auth starts before the bootstrap has written the org id. Until it is there, sign-in,
// sign-up (before any gate call or account), the instance probe and password reset answer
// 503 `unavailable`, and the health check reports not-ready. Once the file appears the next
// request succeeds, without a restart.
#[tokio::test]
async fn sign_up_503_until_org_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("org-id");
    let p = path.clone();
    let h = harness_tweaked(false, 1, auth::config::Mode::SelfHosted, true, move |c| {
        c.org_id = org_file(&p)
    })
    .await;
    h.internal.set_signup(true, "open");
    let unavailable = || err("unavailable", messages::UNAVAILABLE);
    for round in ["absent", "empty"] {
        if round == "empty" {
            write_id(&path, "\n");
        }
        assert_eq!(h.get("/health", "1.1.1.1").await.status, StatusCode::SERVICE_UNAVAILABLE, "{round}");
        let a = h.post("/auth/v1/sign-up", signup("new@example.com", "Secret-pass1"), None, "1.1.1.2").await;
        assert_eq!((a.status, a.body), (StatusCode::SERVICE_UNAVAILABLE, unavailable()), "{round}");
        let a = h.get("/auth/v1/instance", "1.1.1.3").await;
        assert_eq!(a.status, StatusCode::SERVICE_UNAVAILABLE, "{round}");
        let a = h.post("/auth/v1/password-reset", json!({"email": "ada@example.com"}), None, "1.1.1.4").await;
        assert_eq!((a.status, a.body), (StatusCode::SERVICE_UNAVAILABLE, unavailable()), "{round}");
    }
    assert_eq!(h.users_with("new@example.com").await, 0, "no account was made");
    assert!(h.internal.bodies(SIGN_UP_CALL).is_empty(), "the gate was never asked");

    write_id(&path, "o1\n");
    assert_eq!(h.get("/health", "1.1.1.1").await.status, StatusCode::OK);
    let a = h
        .post("/auth/v1/sign-up", signup("new@example.com", "Secret-pass1"), None, "1.1.1.2")
        .await;
    assert_eq!((a.status, a.body), (StatusCode::OK, json!({"next": "verify"})));
    assert_eq!(h.internal.bodies(SIGN_UP_CALL), vec![json!({"org": "o1"})]);
}

// A refused org file (a symlink here) is 503 too, never a sign-in against no org.
#[tokio::test]
async fn a_refused_org_file_is_503() {
    let dir = tempfile::tempdir().unwrap();
    let real = dir.path().join("real");
    write_id(&real, "o1\n");
    let link = dir.path().join("org-id");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let h = harness_tweaked(false, 1, auth::config::Mode::SelfHosted, true, move |c| {
        c.org_id = org_file(&link)
    })
    .await;
    let a = h.post("/auth/v1/sign-up", signup("new@example.com", "Secret-pass1"), None, "1.1.1.1").await;
    assert_eq!(a.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(h.get("/health", "1.1.1.1").await.status, StatusCode::SERVICE_UNAVAILABLE);
}

// AC 5: `GET /auth/v1/studio-config` is exactly the six public values Studio needs, cacheable
// for 60 s. Not configured here (the workbench) it is 404, so Studio uses its build values;
// configured but not written yet it is 503, uncached, and auth isn't ready.
#[tokio::test]
async fn studio_config_shape() {
    use chelabase_types::file_value::{FileValue, Sensitivity};
    let h = harness_tweaked(false, 1, auth::config::Mode::SelfHosted, true, |c| {
        c.studio = auth::config::StudioSettings {
            client_id: Some("c-studio".into()),
            project_id: Some("p1".into()),
            api_url: "http://api.chelabase.localhost:8000".into(),
            studio_host: "studio.chelabase.localhost:8000".into(),
        };
    })
    .await;
    let mut a = h.get("/auth/v1/studio-config", "1.1.1.1").await;
    assert_eq!(a.status, StatusCode::OK);
    let cache = a.body.as_object_mut().unwrap().remove("_cache_control");
    assert_eq!(cache, Some(json!("public, max-age=60")));
    assert_eq!(
        a.body,
        json!({
            "apiUrl": "http://api.chelabase.localhost:8000",
            "studioHost": "studio.chelabase.localhost:8000",
        })
    );
    assert_eq!(h.get("/health", "1.1.1.1").await.status, StatusCode::OK);

    let plain = harness().await;
    let a = plain.get("/auth/v1/studio-config", "1.1.1.1").await;
    assert_eq!(a.status, StatusCode::NOT_FOUND);
    assert_eq!(a.body["error"]["code"], "not_found");
    assert_eq!(plain.get("/health", "1.1.1.1").await.status, StatusCode::OK, "nothing to wait for");

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("studio-client-id");
    let p = path.clone();
    let late = harness_tweaked(false, 1, auth::config::Mode::SelfHosted, true, move |c| {
        c.studio = auth::config::StudioSettings {
            client_id: Some(FileValue::file("CHELABASE_ZITADEL_STUDIO_CLIENT_ID_FILE", &p, Sensitivity::Public)),
            project_id: Some("p1".into()),
            api_url: "http://api.chelabase.localhost:8000".into(),
            studio_host: "studio.chelabase.localhost:8000".into(),
        };
    })
    .await;
    let a = late.get("/auth/v1/studio-config", "1.1.1.1").await;
    assert_eq!(a.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(a.body["error"]["code"], "unavailable");
    assert_eq!(a.body["_cache_control"], "no-store");
    assert_eq!(late.get("/health", "1.1.1.1").await.status, StatusCode::SERVICE_UNAVAILABLE);
    write_id(&path, "c-late\n");
    let a = late.get("/auth/v1/studio-config", "1.1.1.1").await;
    assert_eq!((a.status, a.body["apiUrl"].clone()), (StatusCode::OK, json!("http://api.chelabase.localhost:8000")));
    assert_eq!(late.get("/health", "1.1.1.1").await.status, StatusCode::OK);
}

/// Sends an accept-invite as a client that gives up after 100 ms, from `ip`. Ok(status) when
/// the answer beat the timeout (a 429 does), Err when the client hung up first.
async fn hang_up_accept(h: &Harness, body: Value, ip: &str) -> Result<StatusCode, reqwest::Error> {
    let impatient = reqwest::Client::builder().timeout(Duration::from_millis(100)).build().unwrap();
    impatient
        .post(format!("{}/auth/v1/accept-invite", h.base))
        .json(&body)
        .header("x-forwarded-for", format!("9.9.9.9, {ip}"))
        .send()
        .await
        .map(|r| r.status())
}

// A client that hangs up while Zitadel checks the code still spends the attempt: the check ran
// (a right reset code even set the password), so a dropped request can't be a free guess.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn accept_invite_cancelled_code_checks_still_count_against_the_ip() {
    for reset in [false, true] {
        let h = harness().await;
        let (victim, right, path) = if reset {
            (h.mock.add_user("victim@example.com", "Old-pass-12", true), RESET_CODE.to_string(), "/password")
        } else {
            let (id, code) = h.mock.add_invited_user("victim@example.com");
            (id, code, "/invite_code/verify")
        };
        // Zitadel takes 300 ms to answer the code check; the attacker hangs up at 100 ms.
        h.mock.on_request(path, || std::thread::sleep(Duration::from_millis(300)));
        let mut guesses: Vec<String> = (0..30).map(|i| format!("{:06}", 100000 + i)).collect();
        guesses.push(right); // the 31st guess is the right one
        for (i, code) in guesses.iter().enumerate() {
            let mut body = accept(&h.mock.add_auth_request(false), &victim, code, "Attacker-pw-1");
            body["reset"] = json!(reset);
            match hang_up_accept(&h, body, "7.7.7.7").await {
                Err(e) => assert!(e.is_timeout(), "reset {reset} guess {i}: {e}"),
                Ok(status) => assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "reset {reset} guess {i}"),
            }
        }
        tokio::time::sleep(Duration::from_millis(600)).await;
        h.mock.clear_on_request();
        let checks = h.mock.calls().iter().filter(|c| c.ends_with(path)).count();
        assert!(checks <= 20, "reset {reset}: {checks} code checks from one IP reached Zitadel");
        let pw = h.mock.user_by_id(&victim).unwrap().password;
        assert_ne!(pw, "Attacker-pw-1", "reset {reset}: the attacker set the victim's password");
        let mut body = accept(&h.mock.add_auth_request(false), &victim, "999999", "Attacker-pw-1");
        body["reset"] = json!(reset);
        let next = h.post("/auth/v1/accept-invite", body, None, "7.7.7.7").await;
        assert_eq!(next.status, StatusCode::TOO_MANY_REQUESTS, "reset {reset}: {}", next.body);
    }
}

// The same for the per-user budget: hung-up code checks on one user from five addresses spend
// its five attempts.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn accept_invite_cancelled_code_checks_still_count_against_the_user() {
    for reset in [false, true] {
        let h = harness().await;
        let (victim, path) = if reset {
            (h.mock.add_user("victim@example.com", "Old-pass-12", true), "/password")
        } else {
            (h.mock.add_invited_user("victim@example.com").0, "/invite_code/verify")
        };
        h.mock.on_request(path, || std::thread::sleep(Duration::from_millis(300)));
        for i in 0..5 {
            let mut body = accept(&h.mock.add_auth_request(false), &victim, "999999", "Attacker-pw-1");
            body["reset"] = json!(reset);
            let r = hang_up_accept(&h, body, &format!("8.8.8.{i}")).await;
            assert!(r.as_ref().is_err_and(|e| e.is_timeout()), "reset {reset} attempt {i}: {r:?}");
        }
        tokio::time::sleep(Duration::from_millis(600)).await;
        h.mock.clear_on_request();
        let mut body = accept(&h.mock.add_auth_request(false), &victim, "999999", "Attacker-pw-1");
        body["reset"] = json!(reset);
        let next = h.post("/auth/v1/accept-invite", body, None, "8.8.8.9").await;
        assert_eq!(next.status, StatusCode::TOO_MANY_REQUESTS, "reset {reset}: {}", next.body);
    }
}

// Zitadel refusing the user lookup (a 4xx that isn't its 404) is an answer, not an outage, so
// it counts against the address; only transport errors and 5xx are free.
#[tokio::test]
async fn accept_invite_refused_user_lookup_counts_against_the_ip() {
    let h = harness().await;
    let (id, code) = h.mock.add_invited_user("bob@example.com");
    h.mock.fail("GET /v2/users/", 403, "Errors.Permission", 20);
    for i in 0..20 {
        let a = h
            .post("/auth/v1/accept-invite", accept(&h.mock.add_auth_request(false), &id, &code, "Secret-pass1"), None, "6.6.6.7")
            .await;
        assert_eq!(a.status, StatusCode::SERVICE_UNAVAILABLE, "lookup {i}: {}", a.body);
    }
    let a = h
        .post("/auth/v1/accept-invite", accept(&h.mock.add_auth_request(false), &id, &code, "Secret-pass1"), None, "6.6.6.7")
        .await;
    assert_eq!(a.status, StatusCode::TOO_MANY_REQUESTS, "{}", a.body);
}

// A missing admin token or org id is our outage: 21 such answers leave the address's budget
// untouched.
#[tokio::test]
async fn accept_invite_unconfigured_503s_do_not_count() {
    // Answers are padded to about a second, so the two cases run side by side.
    let no_admin = async {
        let h = harness_full(false, 1, auth::config::Mode::SelfHosted, false).await;
        let (id, code) = h.mock.add_invited_user("bob@example.com");
        for i in 0..21 {
            let a = h
                .post("/auth/v1/accept-invite", accept(&h.mock.add_auth_request(false), &id, &code, "Secret-pass1"), None, "5.5.5.5")
                .await;
            assert_eq!(a.status, StatusCode::SERVICE_UNAVAILABLE, "no admin {i}: {}", a.body);
        }
        // A bad body is checked before the admin token, so it's answered (not 429) only if the
        // budget is still there.
        let a = h
            .post("/auth/v1/accept-invite", accept(&h.mock.add_auth_request(false), "not-an-id", &code, "Secret-pass1"), None, "5.5.5.5")
            .await;
        assert_eq!(a.status, StatusCode::BAD_REQUEST, "no admin: {}", a.body);
    };
    let no_org = async {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("org-id");
        let p = path.clone();
        let h = harness_tweaked(false, 1, auth::config::Mode::SelfHosted, true, move |c| c.org_id = org_file(&p)).await;
        let (id, code) = h.mock.add_invited_user("bob@example.com");
        for i in 0..21 {
            let a = h
                .post("/auth/v1/accept-invite", accept(&h.mock.add_auth_request(false), &id, &code, "Secret-pass1"), None, "5.5.5.6")
                .await;
            assert_eq!(a.status, StatusCode::SERVICE_UNAVAILABLE, "no org {i}: {}", a.body);
        }
        write_id(&path, "o1\n");
        let ok = h
            .post("/auth/v1/accept-invite", accept(&h.mock.add_auth_request(false), &id, &code, "Secret-pass1"), None, "5.5.5.6")
            .await;
        assert_eq!(ok.status, StatusCode::OK, "no org: {}", ok.body);
    };
    tokio::join!(no_admin, no_org);
}

// ---- #74: the sign-in email limit under concurrency and hang-ups ------------------------------

/// An identity service whose every password check is refused, slowly: after `delay`, or
/// never without one (the check is still running when the client hangs up).
struct PasswordStub {
    base: String,
    checks: Arc<std::sync::atomic::AtomicUsize>,
    started: Arc<tokio::sync::Notify>,
}

impl PasswordStub {
    async fn start(delay: Option<Duration>) -> Self {
        let checks = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let started = Arc::new(tokio::sync::Notify::new());
        let (c, n) = (checks.clone(), started.clone());
        let app = axum::Router::new().route(
            "/internal/v1/pools/members/sessions",
            axum::routing::post(move || {
                let (c, n) = (c.clone(), n.clone());
                async move {
                    c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    n.notify_one();
                    match delay {
                        Some(delay) => tokio::time::sleep(delay).await,
                        None => std::future::pending().await,
                    }
                    (axum::http::StatusCode::UNAUTHORIZED, axum::Json(json!({"error": "bad_credentials", "message": "no"})))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self { base: format!("http://{addr}"), checks, started }
    }

    fn checks(&self) -> usize {
        self.checks.load(std::sync::atomic::Ordering::SeqCst)
    }
}

/// Sends a sign-in from `ip` and hangs up once identity is checking the password.
async fn hang_up_sign_in(h: &Harness, stub: &PasswordStub, body: Value, ip: &str) {
    let send = reqwest::Client::new()
        .post(format!("{}/auth/v1/sign-in", h.base))
        .json(&body)
        .header("x-forwarded-for", format!("9.9.9.9, {ip}"))
        .send();
    tokio::select! {
        r = send => panic!("{ip}: answered before the password check reached identity: {r:?}"),
        () = stub.started.notified() => {}
    }
}

/// Sends `n` wrong-password sign-ins for ada one at a time, each from its own address, and
/// returns their statuses.
async fn wrong_sign_ins(h: &Harness, n: usize, tag: &str) -> Vec<StatusCode> {
    let mut out = Vec::new();
    for i in 0..n {
        let body = signin("ada@example.com", "Wrong-pass1");
        out.push(h.post("/auth/v1/sign-in", body, None, &format!("6.{tag}.{i}.1")).await.status);
    }
    out
}

/// The budget is whole: five wrong passwords are checked (401), and the sixth is refused.
async fn assert_budget_unspent(h: &Harness, tag: &str) {
    let got = wrong_sign_ins(h, 6, tag).await;
    assert!(got[..5].iter().all(|s| *s == StatusCode::UNAUTHORIZED), "{tag}: {got:?}");
    assert_eq!(got[5], StatusCode::TOO_MANY_REQUESTS, "{tag}: {got:?}");
}

// AC 1: 30 wrong passwords at once from 30 addresses get at most 5 password checks.
// AC 1: 30 wrong passwords at once from 30 addresses get at most 5 password checks.
#[tokio::test(flavor = "multi_thread", worker_threads = 64)]
async fn sign_in_email_limit_holds_under_concurrency() {
    // A slow password check, so every request is in flight before the first answer.
    let stub = PasswordStub::start(Some(Duration::from_millis(300))).await;
    let h = Arc::new(harness_on_identity(false, 1, auth::config::Mode::SelfHosted, true, |_| {}, Some(stub.base.clone())).await);
    let mut tasks = Vec::new();
    for i in 0..30 {
        let h = h.clone();
        tasks.push(tokio::spawn(async move {
            let body = signin("ada@example.com", "Wrong-pass1");
            h.post("/auth/v1/sign-in", body, None, &format!("1.2.{i}.1")).await.status
        }));
    }
    let (mut refused, mut limited) = (0, 0);
    for t in tasks {
        match t.await.unwrap() {
            StatusCode::UNAUTHORIZED => refused += 1,
            StatusCode::TOO_MANY_REQUESTS => limited += 1,
            other => panic!("unexpected {other}"),
        }
    }
    let checks = stub.checks();
    assert!(checks <= 5, "{checks} password checks reached identity ({refused} got 401, {limited} got 429)");
    assert_eq!((refused, limited), (5, 25));
}

// AC 3: a client that hangs up while Zitadel checks the password still spends the attempt.
// AC 3: a client that hangs up while identity checks the password still spends the attempt.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn sign_in_cancelled_password_checks_still_count() {
    // Identity holds each password check, so each client hangs up in the middle of it.
    let stub = PasswordStub::start(None).await;
    let h = harness_on_identity(false, 1, auth::config::Mode::SelfHosted, true, |_| {}, Some(stub.base.clone())).await;
    for i in 0..5 {
        hang_up_sign_in(&h, &stub, signin("ada@example.com", "Wrong-pass1"), &format!("1.3.{i}.1")).await;
    }
    // A patient client (no timeout).
    let a = h.post("/auth/v1/sign-in", signin("ada@example.com", "Wrong-pass1"), None, "1.3.9.1").await;
    assert_eq!(a.status, StatusCode::TOO_MANY_REQUESTS, "{}", a.body);
    assert_eq!(stub.checks(), 5);
}

// AC 7: correct passwords at once all sign in and spend nothing.
// AC 7: correct passwords at once all sign in and spend nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 16)]
async fn concurrent_correct_sign_ins_do_not_spend_the_email_budget() {
    let h = Arc::new(harness().await);
    h.member("ada@example.com", &[T1]);
    let mut tasks = Vec::new();
    for i in 0..5 {
        let h = h.clone();
        tasks.push(tokio::spawn(async move {
            let a = h.post("/auth/v1/sign-in", signin("ada@example.com", "Secret-pass1"), None, &format!("1.4.{i}.1")).await;
            (a.status, a.body)
        }));
    }
    for t in tasks {
        let (status, body) = t.await.unwrap();
        assert_eq!(status, StatusCode::OK, "{body}");
    }
    assert_budget_unspent(&h, "1").await;
}

// AC 8: outages and sign-ins that are dead before the password check spend nothing; Zitadel's
// own refusals of the input (its request validation) count like a wrong password.
// AC 8: outages spend nothing; identity's own refusals of the input count like a wrong password.
#[tokio::test]
async fn sign_in_outages_do_not_spend_the_email_budget() {
    let h = harness().await;
    h.member("ada@example.com", &[T1]);
    let mut ip = 0;
    let mut sign_in = async |password: &str, why: &str| {
        ip += 1;
        let a = h.post("/auth/v1/sign-in", signin("ada@example.com", password), None, &format!("2.4.{ip}.1")).await;
        assert_eq!(a.status, StatusCode::SERVICE_UNAVAILABLE, "{why}: {}", a.body);
    };
    h.identity.set_down(true);
    for _ in 0..6 {
        sign_in("Wrong-pass1", "down").await;
    }
    h.identity.set_down(false);
    // The password check fails: a 5xx, and a 4xx that isn't the service's own answer.
    h.identity.fail("POST /internal/v1/pools/members/sessions", 503, "internal", 3);
    h.identity.fail("POST /internal/v1/pools/members/sessions", 404, "pool_not_found", 3);
    for _ in 0..6 {
        sign_in("Wrong-pass1", "check down").await;
    }
    // The memberships can't be read.
    h.internal.set_members_status(Some(500));
    for _ in 0..6 {
        sign_in("Secret-pass1", "orchestrator down").await;
    }
    h.internal.set_members_status(None);
    assert_eq!(h.sessions(), 0, "no session outlives an outage");
    assert_budget_unspent(&h, "2").await;

    // Identity's validation (a password over its cap): the caller's input counts.
    let h = harness().await;
    h.member("ada@example.com", &[T1]);
    h.identity.fail("POST /internal/v1/pools/members/sessions", 400, "invalid_request", 5);
    let got = wrong_sign_ins(&h, 6, "3").await;
    assert!(got[..5].iter().all(|s| *s == StatusCode::UNAUTHORIZED), "{got:?}");
    assert_eq!(got[5], StatusCode::TOO_MANY_REQUESTS, "{got:?}");
}

fn reset_accept(ar: &str, user: &str, code: &str, pw: &str) -> Value {
    let mut body = accept(ar, user, code, pw);
    body["reset"] = json!(true);
    body
}

// #78: Zitadel makes a reset code for an inactive user too, and anyone can ask for one. A removed
// member's own reset code checks out, but without an open invite it reactivates nothing and signs
// nobody in.
#[tokio::test]
async fn a_reset_code_never_reactivates_a_member_without_an_open_invite() {
    let h = harness().await;
    let id = h.mock.add_user("gone@example.com", "Old-pass-12", true);
    h.mock.set_inactive(&id, true);
    // No invite at all, then one that expired.
    for expired in [false, true] {
        if expired {
            h.internal.set_open_invite(&id, false);
        }
        let ar = h.mock.add_auth_request(false);
        let a = h.post("/auth/v1/accept-invite", reset_accept(&ar, &id, RESET_CODE, "Secret-pass1"), None, "1.1.1.1").await;
        assert_eq!(a.status, StatusCode::BAD_REQUEST, "expired {expired}: {}", a.body);
        assert_eq!(a.body["error"]["code"], "invalid_request", "expired {expired}");
        assert!(a.cookie("chelabase_session").is_none(), "expired {expired}");
        assert!(h.mock.user_by_id(&id).unwrap().inactive, "expired {expired}: still inactive");
    }
    assert!(!h.mock.calls().iter().any(|c| c.ends_with("/reactivate")), "{:?}", h.mock.calls());
    assert!(!h.mock.calls().iter().any(|c| c == "POST /v2/sessions"), "{:?}", h.mock.calls());
    assert_eq!(h.internal.bodies("POST /internal/invites/open"), vec![json!({"user": id}); 2]);
    // Invited again: the same reset code reactivates and signs in.
    h.internal.set_open_invite(&id, true);
    let ar = h.mock.add_auth_request(false);
    let ok = h.post("/auth/v1/accept-invite", reset_accept(&ar, &id, RESET_CODE, "Secret-pass1"), None, "1.1.1.1").await;
    assert_eq!(ok.status, StatusCode::OK, "{}", ok.body);
    assert!(!h.mock.user_by_id(&id).unwrap().inactive);
}

// #78: when the orchestrator can't say whether an invite is open, nobody is reactivated (fail
// closed): 503 with a ticket, and the retry completes once it answers.
#[tokio::test]
async fn an_orchestrator_outage_never_reactivates() {
    for status in [500, 401] {
        let h = harness().await;
        let (id, code) = h.mock.add_invited_user("back@example.com");
        h.mock.set_inactive(&id, true);
        h.internal.set_open_invite(&id, true);
        h.internal.set_open_status(Some(status));
        let a = h
            .post("/auth/v1/accept-invite", accept(&h.mock.add_auth_request(false), &id, &code, "Secret-pass1"), None, "1.1.1.1")
            .await;
        assert_eq!(a.status, StatusCode::SERVICE_UNAVAILABLE, "{status}: {}", a.body);
        assert_eq!(a.body["error"]["code"], "invite_incomplete", "{status}");
        assert!(a.cookie("chelabase_session").is_none(), "{status}");
        assert!(h.mock.user_by_id(&id).unwrap().inactive, "{status}: still inactive");
        assert!(!h.mock.calls().iter().any(|c| c.ends_with("/reactivate")), "{status}");
        h.internal.set_open_status(None);
        let ok = h
            .post(
                "/auth/v1/accept-invite",
                with_ticket(&h.mock.add_auth_request(false), &id, &ticket_of(&a), "Secret-pass1"),
                None,
                "1.1.1.1",
            )
            .await;
        assert_eq!(ok.status, StatusCode::OK, "{status}: {}", ok.body);
        assert!(!h.mock.user_by_id(&id).unwrap().inactive, "{status}");
    }
}

// A suspended member's Zitadel user is active and locked (#76). A reset code and an open invite
// still give no session: accept-invite never unlocks, and Zitadel refuses the locked user's
// session (`SESSION-Gj4ko` or `COMMAND-JLK35`), so the answer is `invite_incomplete`.
#[tokio::test]
async fn accept_invite_never_signs_in_a_suspended_member() {
    // Zitadel's user check (`SESSION-Gj4ko`) or its password check (`COMMAND-JLK35`).
    for by_password_check in [false, true] {
        let h = harness().await;
        h.mock.set_lock_seen_by_password_check(by_password_check);
        let id = h.mock.add_user("held@example.com", "Old-pass-12", true);
        h.mock.lock(&id);
        h.internal.set_open_invite(&id, true);
        let ar = h.mock.add_auth_request(false);
        let a = h.post("/auth/v1/accept-invite", reset_accept(&ar, &id, RESET_CODE, "Secret-pass1"), None, "1.1.1.1").await;
        let why = format!("by the password check: {by_password_check}");
        // Suspended, not unfinished: a retry can never succeed, so no ticket invites one.
        assert_eq!(a.status, StatusCode::FORBIDDEN, "{why}: {}", a.body);
        assert_eq!(a.body["error"]["code"], "member_suspended", "{why}");
        assert!(a.body["error"]["accept_ticket"].is_null(), "{why}: {}", a.body);
        assert!(a.cookie("chelabase_session").is_none(), "{why}");
        let user = h.mock.user_by_id(&id).unwrap();
        assert!(user.locked && !user.inactive, "{why}: still locked");
        assert!(!h.mock.calls().iter().any(|c| c.ends_with("/unlock") || c.ends_with("/reactivate")), "{why}");
    }
}

// Zitadel answers a wrong and an expired invite code alike (`COMMAND-Wgn4q`). Only then does auth
// read the stored invite: an expired one is `invite_expired`, anything else the invalid link.
#[tokio::test]
async fn a_refused_code_is_expired_only_when_the_stored_invite_expired() {
    let h = harness().await;
    // A right code never asks.
    let (other, other_code) = h.mock.add_invited_user("other@example.com");
    let ok = h
        .post("/auth/v1/accept-invite", accept(&h.mock.add_auth_request(false), &other, &other_code, "Secret-pass1"), None, "1.1.1.9")
        .await;
    assert_eq!(ok.status, StatusCode::OK, "{}", ok.body);
    assert!(h.internal.bodies("POST /internal/invites/open").is_empty());
    let (id, _) = h.mock.add_invited_user("bob@example.com");
    let h = &h;
    let wrong = |ip: &'static str| {
        let body = accept(&h.mock.add_auth_request(false), &id, "WRONG", "Secret-pass1");
        async move { h.post("/auth/v1/accept-invite", body, None, ip).await }
    };
    // No invite stored, an open one, an expired one, and the orchestrator down.
    let a = wrong("1.1.1.1").await;
    assert_eq!((a.status, a.body["error"]["code"].clone()), (StatusCode::BAD_REQUEST, json!("invalid_request")));
    h.internal.set_open_invite(&id, true);
    let a = wrong("1.1.1.2").await;
    assert_eq!((a.status, a.body["error"]["code"].clone()), (StatusCode::BAD_REQUEST, json!("invalid_request")));
    h.internal.set_open_invite(&id, false);
    let a = wrong("1.1.1.3").await;
    assert_eq!((a.status, a.body["error"]["code"].clone()), (StatusCode::GONE, json!("invite_expired")));
    h.internal.set_open_status(Some(500));
    let a = wrong("1.1.1.4").await;
    assert_eq!((a.status, a.body["error"]["code"].clone()), (StatusCode::BAD_REQUEST, json!("invalid_request")));
    h.internal.set_open_status(None);
    assert_eq!(h.internal.bodies("POST /internal/invites/open"), vec![json!({"user": id}); 4]);
    // Every one of them counted against the invitee: the fifth still answers, the sixth is 429.
    assert_eq!(wrong("1.1.1.5").await.body["error"]["code"], "invite_expired");
    assert_eq!(wrong("1.1.1.6").await.status, StatusCode::TOO_MANY_REQUESTS);
}

// Every arm that hands an attempt back matches one exact captured refusal, and a wrong code is
// never one of them, whatever the invite's state: per case, the sixth wrong code for one user is
// 429 on the invitee's key, and the address has spent five of its 20.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn wrong_codes_never_reach_a_refunding_arm() {
    // Answers are padded to about a second, so the cases run side by side.
    let cases = ["invite", "reset", "reset-inactive", "no-invite", "open", "expired", "lookup-down"];
    let runs: Vec<_> = cases.into_iter().map(|case| tokio::spawn(wrong_codes_case(case))).collect();
    for run in runs {
        run.await.unwrap();
    }
}

async fn wrong_codes_case(case: &'static str) {
    let h = harness().await;
    let reset = case.starts_with("reset");
    let id = if reset {
        h.mock.add_user("bob@example.com", "Old-pass-12", true)
    } else {
        h.mock.add_invited_user("bob@example.com").0
    };
    match case {
        "reset-inactive" => h.mock.set_inactive(&id, true),
        "no-invite" => h.mock.inner.lock().unwrap().invites.clear(),
        "open" => h.internal.set_open_invite(&id, true),
        "expired" => h.internal.set_open_invite(&id, false),
        "lookup-down" => h.internal.set_open_status(Some(500)),
        _ => {}
    }
    let wrong = || {
        let ar = h.mock.add_auth_request(false);
        if reset { reset_accept(&ar, &id, "000000", "Secret-pass1") } else { accept(&ar, &id, "WRONG", "Secret-pass1") }
    };
    for i in 0..5 {
        let a = h.post("/auth/v1/accept-invite", wrong(), None, "1.1.1.1").await;
        assert!(
            matches!(a.status, StatusCode::BAD_REQUEST | StatusCode::GONE),
            "{case} {i}: {} {}",
            a.status,
            a.body
        );
    }
    let sixth = h.post("/auth/v1/accept-invite", wrong(), None, "1.1.1.1").await;
    assert_eq!(sixth.status, StatusCode::TOO_MANY_REQUESTS, "{case}: {}", sixth.body);
    // The address counted the five: 15 more failures, then even a right code is 429.
    for i in 0..15 {
        let (other, _) = h.mock.add_invited_user(&format!("u{i}@example.com"));
        let a = h
            .post("/auth/v1/accept-invite", accept(&h.mock.add_auth_request(false), &other, "WRONG", "Secret-pass1"), None, "1.1.1.1")
            .await;
        assert_ne!(a.status, StatusCode::TOO_MANY_REQUESTS, "{case}: failure {i} after the five");
    }
    let (late, code) = h.mock.add_invited_user("late@example.com");
    let a = h
        .post("/auth/v1/accept-invite", accept(&h.mock.add_auth_request(false), &late, &code, "Secret-pass1"), None, "1.1.1.1")
        .await;
    assert_eq!(a.status, StatusCode::TOO_MANY_REQUESTS, "{case}: the address spent 20");
}

// Zitadel hashes passwords with bcrypt, which takes 72 bytes at most: a longer one would pass its
// validation and fail with a 500. Counted in bytes: 39 characters can be 74 bytes.
#[tokio::test]
async fn accept_invite_refuses_a_password_over_72_bytes_before_zitadel() {
    let h = harness().await;
    let long = format!("Aa1!{}", "é".repeat(35));
    assert_eq!((long.chars().count(), long.len()), (39, 74));
    let (id, code) = h.mock.add_invited_user("bob@example.com");
    let a = h
        .post("/auth/v1/accept-invite", accept(&h.mock.add_auth_request(false), &id, &code, &long), None, "1.1.1.1")
        .await;
    assert_eq!(a.status, StatusCode::BAD_REQUEST, "{}", a.body);
    assert_eq!(a.body["error"]["code"], "weak_password");
    assert!(!h.mock.calls().iter().any(|c| c.contains("/invite_code/verify")), "{:?}", h.mock.calls());
    // 72 bytes is fine, and the code wasn't spent.
    let max = format!("Aa1!{}", "é".repeat(34));
    assert_eq!(max.len(), 72);
    let ok = h
        .post("/auth/v1/accept-invite", accept(&h.mock.add_auth_request(false), &id, &code, &max), None, "1.1.1.1")
        .await;
    assert_eq!(ok.status, StatusCode::OK, "{}", ok.body);
}

// Epic review (10): only Zitadel's captured "no such auth request" (`QUERY-Thee9` on the read,
// `COMMAND-jae5P` on finishing) is an expired sign-in; any other refusal is unavailability.
// Epic review (10): after the code is spent, a refusal of finishing the sign-in that isn't the
// captured "no such auth request" is a step to retry (a ticket), not a finished sign-in.
#[tokio::test]
async fn accept_invite_finish_refused_otherwise_hands_back_a_ticket() {
    let h = harness().await;
    let (id, code) = h.mock.add_invited_user("bob@example.com");
    h.mock.fail("POST /v2/oidc/auth_requests/", 400, "Errors.Something.Else", 1);
    let a = h
        .post("/auth/v1/accept-invite", accept(&h.mock.add_auth_request(false), &id, &code, "Secret-pass1"), None, "1.1.1.1")
        .await;
    assert_eq!(a.status, StatusCode::SERVICE_UNAVAILABLE, "{}", a.body);
    assert_eq!(a.body["error"]["code"], "invite_incomplete");
    let ok = h
        .post("/auth/v1/accept-invite", with_ticket(&h.mock.add_auth_request(false), &id, &ticket_of(&a), "Secret-pass1"), None, "1.1.1.1")
        .await;
    assert_eq!(ok.status, StatusCode::OK, "{}", ok.body);
    assert_eq!(ok.body["next"], "done");
}

// Epic review (9): only identity's `conflict` is a taken email at sign-up; another 409 is
// unavailability.
#[tokio::test]
async fn sign_up_reads_only_the_captured_conflict_as_taken() {
    let h = harness_with(true, 0).await;
    h.identity.fail("POST /internal/v1/pools/members/users", 409, "conflict", 1);
    let a = h.post("/auth/v1/sign-up", signup("new@example.com", "Secret-pass1"), None, "1.1.1.1").await;
    assert_eq!((a.status, a.body["error"]["code"].clone()), (StatusCode::CONFLICT, json!("email_taken")), "{}", a.body);
    h.identity.fail("POST /internal/v1/pools/members/users", 409, "something_else", 1);
    let a = h.post("/auth/v1/sign-up", signup("new@example.com", "Secret-pass1"), None, "1.1.1.2").await;
    assert_eq!((a.status, a.body), (StatusCode::SERVICE_UNAVAILABLE, err("unavailable", messages::UNAVAILABLE)));
}

// Epic review (9): the session API's lag is waited out only on its captured "no such user"
// (`QUERY-Dfbg2`); another 404 is not retried.
#[tokio::test]
async fn accept_invite_retries_only_the_captured_missing_user() {
    let h = harness().await;
    let (id, code) = h.mock.add_invited_user("bob@example.com");
    h.mock.fail("POST /v2/sessions", 404, "Errors.Something.Else", 1);
    let a = h
        .post("/auth/v1/accept-invite", accept(&h.mock.add_auth_request(false), &id, &code, "Secret-pass1"), None, "1.1.1.1")
        .await;
    assert_eq!(a.status, StatusCode::SERVICE_UNAVAILABLE, "{}", a.body);
    assert_eq!(a.body["error"]["code"], "invite_incomplete");
    assert_eq!(h.mock.calls().iter().filter(|c| *c == "POST /v2/sessions").count(), 1);
}

// ---- #121: members sign in, up and out against the identity service --------------------------

const PW: &str = "Secret-pass1";
const STUDIO_ORIGIN: &str = "http://studio.chelabase.localhost:8000";
const T1: Uuid = Uuid::from_u128(0xa1);
const T2: Uuid = Uuid::from_u128(0xa2);
const T3: Uuid = Uuid::from_u128(0xa3);

impl Harness {
    /// A verified member of `tenants`.
    fn member(&self, email: &str, tenants: &[Uuid]) -> String {
        let id = self.identity.with_user(&Pool::Members, email, PW);
        self.internal.set_memberships(&id, tenants, &[]);
        id
    }

    async fn sign_in(&self, query: &str, email: &str, password: &str, ip: &str) -> Answer {
        self.post(&format!("/auth/v1/sign-in{query}"), json!({"email": email, "password": password}), None, ip).await
    }

    /// A sign-in that has to work: the `chelabase_session` cookie.
    async fn session(&self, query: &str, email: &str, ip: &str) -> String {
        let a = self.sign_in(query, email, PW, ip).await;
        assert_eq!((a.status, &a.body), (StatusCode::OK, &json!({"next": "studio"})), "sign-in");
        a.cookie("chelabase_session").expect("the session cookie")
    }

    async fn token_as(&self, path: &str, cookie: Option<&str>, headers: &[(&str, &str)]) -> Answer {
        let mut req = self.http.post(format!("{}{path}", self.base));
        if let Some(c) = cookie {
            req = req.header("cookie", c);
        }
        for (name, value) in headers {
            req = req.header(*name, *value);
        }
        let resp = req.send().await.unwrap();
        let status = resp.status();
        let cookies = resp.headers().get_all("set-cookie").iter().map(|v| v.to_str().unwrap().to_string()).collect();
        let cache = resp.headers().get("cache-control").map(|v| v.to_str().unwrap().to_string());
        let mut body: Value = serde_json::from_str(&resp.text().await.unwrap()).unwrap_or(Value::Null);
        if let (Some(cache), Value::Object(map)) = (cache, &mut body) {
            map.insert("_cache_control".into(), json!(cache));
        }
        Answer { status, body, cookies }
    }

    /// `/auth/v1/token` as Studio calls it.
    async fn token(&self, cookie: &str) -> Answer {
        self.token_as("/auth/v1/token", Some(cookie), &[("origin", STUDIO_ORIGIN), ("x-chelabase-studio", "1")]).await
    }

    fn claims(&self, a: &Answer) -> MemberClaims {
        assert_eq!(a.status, StatusCode::OK, "{}", a.body);
        MemberTokenVerifier::from_pem(self.member_public.as_bytes(), &self.mock.base)
            .unwrap()
            .verify(a.body["access_token"].as_str().unwrap())
            .expect("the gateway's verifier accepts it")
    }

    fn sessions(&self) -> usize {
        self.identity.session_count(&Pool::Members)
    }

    async fn users_with(&self, email: &str) -> u64 {
        self.identity.client().search_users(&Pool::Members, email, 10, 0).await.unwrap().total
    }
}

// AC 1.
#[tokio::test]
async fn member_sign_in_token_roundtrip() {
    let h = harness().await;
    let id = h.member("ada@example.com", &[T1]);
    let a = h.sign_in("", " ADA@example.com ", PW, "1.1.1.1").await;
    assert_eq!((a.status, &a.body), (StatusCode::OK, &json!({"next": "studio"})));
    let set = a.cookies.iter().find(|c| c.starts_with("chelabase_session=")).expect("set");
    assert!(set.contains("HttpOnly; SameSite=Strict; Path=/auth/v1"), "{set}");
    let t = h.token(&a.cookie("chelabase_session").unwrap()).await;
    let claims = h.claims(&t);
    assert_eq!((claims.sub.as_str(), claims.tid, claims.email.as_str()), (id.as_str(), T1, "ada@example.com"));
    assert_eq!(t.body["expires_in"], 300);
    assert_eq!(t.body["_cache_control"], "no-store");
    let token = h.identity.client().sign_in(&Pool::Members, "ada@example.com", PW).await.unwrap().session_token;
    assert!(a.cookies.iter().all(|c| !c.contains(&token)), "the session token is sealed");
    // Without identity's Secure flag in dev; on for HTTPS deployments.
    let secure = harness_tweaked(false, 1, auth::config::Mode::SelfHosted, true, |c| c.secure_cookies = true).await;
    secure.member("ada@example.com", &[T1]);
    let a = secure.sign_in("", "ada@example.com", PW, "1.1.1.1").await;
    assert!(a.cookies.iter().any(|c| c.starts_with("chelabase_session=") && c.contains("; Secure")));
}

// AC 2, with the padding bound measured: a refused sign-in looks the same whichever way it
// failed, and leaves no session behind.
#[tokio::test]
async fn sign_in_failures_indistinguishable() {
    const PAD_MS: u64 = 300;
    let h = harness_with(false, PAD_MS).await;
    h.member("ada@example.com", &[T1]);
    h.member("none@example.com", &[]);
    let banned = h.member("banned@example.com", &[T1]);
    h.identity.ban(&Pool::Members, &banned);
    let mut seen = Vec::new();
    for (i, (query, email, pw)) in [
        ("", "ada@example.com", "Wrong-pass1"),
        ("", "nobody@example.com", PW),
        ("", "none@example.com", PW),
        ("", "banned@example.com", PW),
        ("?tenant=not-a-uuid", "ada@example.com", PW),
    ]
    .into_iter()
    .enumerate()
    {
        let start = Instant::now();
        let a = h.sign_in(query, email, pw, &format!("3.3.3.{i}")).await;
        let took = start.elapsed();
        assert!(took >= Duration::from_millis(PAD_MS), "{email}: {took:?}");
        assert!(took < Duration::from_millis(PAD_MS + 250), "{email}: {took:?}");
        assert!(a.cookies.is_empty(), "{email}: {:?}", a.cookies);
        seen.push((a.status, a.body));
    }
    let want = (StatusCode::UNAUTHORIZED, err("invalid_credentials", messages::BAD_CREDENTIALS));
    assert!(seen.iter().all(|s| *s == want), "{seen:?}");
    assert_eq!(h.sessions(), 0, "a refused sign-in leaves no session");
}

// AC 3.
#[tokio::test]
async fn logout_kills_token_minting() {
    let h = harness().await;
    h.member("ada@example.com", &[T1]);
    let cookie = h.session("", "ada@example.com", "1.1.1.1").await;
    assert_eq!(h.token(&cookie).await.status, StatusCode::OK);
    let out = h.post("/auth/v1/logout", json!({}), Some(&cookie), "1.1.1.1").await;
    assert_eq!(out.status, StatusCode::OK);
    assert_eq!(h.sessions(), 0);
    assert!(out.cookies.iter().any(|c| c.starts_with("chelabase_session=;")));
    // The same cookie bytes, replayed.
    let replay = h.token(&cookie).await;
    assert_eq!(replay.status, StatusCode::UNAUTHORIZED);
    assert!(replay.body.get("access_token").is_none());
}

// A logout that couldn't reach identity must not claim the session ended: the cookie stays so
// the client can try again.
#[tokio::test]
async fn logout_outage_is_not_a_logout() {
    let h = harness().await;
    h.member("ada@example.com", &[T1]);
    let cookie = h.session("", "ada@example.com", "1.1.1.1").await;
    h.identity.set_down(true);
    let out = h.post("/auth/v1/logout", json!({}), Some(&cookie), "1.1.1.1").await;
    assert_eq!(out.status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(out.cookies.is_empty());
    h.identity.set_down(false);
    let out = h.post("/auth/v1/logout", json!({}), Some(&cookie), "1.1.1.1").await;
    assert_eq!(out.status, StatusCode::OK);
    // Already gone is still a logout.
    let out = h.post("/auth/v1/logout", json!({}), Some(&cookie), "1.1.1.1").await;
    assert_eq!(out.status, StatusCode::OK);
}

// AC 4, and the session being revoked out of band answers at once.
#[tokio::test]
async fn suspended_member_no_token() {
    let h = harness().await;
    let id = h.member("ada@example.com", &[T1]);
    let cookie = h.session("", "ada@example.com", "1.1.1.1").await;
    assert_eq!(h.token(&cookie).await.status, StatusCode::OK);
    // Suspended: no longer active in T1.
    h.internal.set_memberships(&id, &[], &[]);
    let a = h.token(&cookie).await;
    assert_eq!(a.status, StatusCode::UNAUTHORIZED, "{}", a.body);
    assert!(a.body.get("access_token").is_none());
    // Can't tell: no token either.
    h.internal.set_members_status(Some(500));
    assert_eq!(h.token(&cookie).await.status, StatusCode::SERVICE_UNAVAILABLE);
    h.internal.set_members_status(None);
    // Reinstated, then the identity session is revoked.
    h.internal.set_memberships(&id, &[T1], &[]);
    assert_eq!(h.token(&cookie).await.status, StatusCode::OK);
    h.identity.client().revoke_user_sessions(&Pool::Members, &id).await.unwrap();
    assert_eq!(h.token(&cookie).await.status, StatusCode::UNAUTHORIZED);
}

// AC 5.
#[tokio::test]
async fn token_csrf_guards() {
    let h = harness().await;
    h.member("ada@example.com", &[T1]);
    let cookie = h.session("", "ada@example.com", "1.1.1.1").await;
    for headers in [
        vec![("origin", STUDIO_ORIGIN)],
        vec![("origin", STUDIO_ORIGIN), ("x-chelabase-studio", "0")],
        vec![("x-chelabase-studio", "1")],
        vec![("origin", "http://evil.example"), ("x-chelabase-studio", "1")],
        vec![("origin", "http://studio.chelabase.localhost:8000.evil.example"), ("x-chelabase-studio", "1")],
        vec![("origin", "null"), ("x-chelabase-studio", "1")],
    ] {
        let a = h.token_as("/auth/v1/token", Some(&cookie), &headers).await;
        assert_eq!((a.status, a.body["error"]["code"].as_str()), (StatusCode::FORBIDDEN, Some("forbidden")), "{headers:?}");
    }
    // GET has no route.
    let got = h.http.get(format!("{}/auth/v1/token", h.base)).header("cookie", &cookie).send().await.unwrap();
    assert_eq!(got.status(), reqwest::StatusCode::METHOD_NOT_ALLOWED);
    // No or a junk cookie.
    let studio = [("origin", STUDIO_ORIGIN), ("x-chelabase-studio", "1")];
    assert_eq!(h.token_as("/auth/v1/token", None, &studio).await.status, StatusCode::UNAUTHORIZED);
    assert_eq!(h.token_as("/auth/v1/token", Some("chelabase_session=junk"), &studio).await.status, StatusCode::UNAUTHORIZED);
}

// The route's own limit: 120 a minute per session.
#[tokio::test]
async fn token_is_limited_per_session() {
    let h = harness().await;
    h.member("ada@example.com", &[T1]);
    h.member("bob@example.com", &[T1]);
    let cookie = h.session("", "ada@example.com", "1.1.1.1").await;
    for _ in 0..120 {
        assert_eq!(h.token(&cookie).await.status, StatusCode::OK);
    }
    assert_eq!(h.token(&cookie).await.status, StatusCode::TOO_MANY_REQUESTS);
    let other = h.session("", "bob@example.com", "1.1.1.1").await;
    assert_eq!(h.token(&other).await.status, StatusCode::OK);
}

// AC 6.
#[tokio::test]
async fn tenant_choice_validated() {
    let h = harness().await;
    h.member("ada@example.com", &[T1, T2]);
    let cookie = h.session(&format!("?tenant={T2}"), "ada@example.com", "1.1.1.1").await;
    assert_eq!(h.claims(&h.token(&cookie).await).tid, T2);
    let a = h.sign_in(&format!("?tenant={T3}"), "ada@example.com", PW, "1.1.1.1").await;
    assert_eq!((a.status, a.body), (StatusCode::UNAUTHORIZED, err("invalid_credentials", messages::BAD_CREDENTIALS)));
    assert!(a.cookies.is_empty());
    assert_eq!(h.sessions(), 1, "only the T2 session lives");
}

// Several memberships and no choice: which ones, only to whoever has the password; no session.
#[tokio::test]
async fn several_tenants_need_a_choice() {
    let h = harness().await;
    h.member("ada@example.com", &[T1, T2]);
    let a = h.sign_in("", "ada@example.com", PW, "1.1.1.1").await;
    assert_eq!(a.status, StatusCode::CONFLICT);
    assert_eq!(a.body["error"]["code"], "tenant_required");
    assert_eq!(a.body["error"]["tenants"], json!([T1, T2]));
    assert!(a.cookies.is_empty());
    assert_eq!(h.sessions(), 0, "the retry with ?tenant= makes its own session");
    let wrong = h.sign_in("", "ada@example.com", "Wrong-pass1", "1.1.1.1").await;
    assert_eq!(wrong.status, StatusCode::UNAUTHORIZED);
    assert!(wrong.body["error"].get("tenants").is_none());
}

// A pending person is told so, once their password is right, and gets no session.
#[tokio::test]
async fn pending_member_is_told_so() {
    let h = harness().await;
    let id = h.member("ada@example.com", &[]);
    h.internal.set_memberships(&id, &[], &[T1]);
    let a = h.sign_in("", "ada@example.com", PW, "1.1.1.1").await;
    assert_eq!((a.status, a.body), (StatusCode::OK, json!({"next": "pending"})));
    assert!(a.cookies.is_empty());
    assert_eq!(h.sessions(), 0);
    let wrong = h.sign_in("", "ada@example.com", "Wrong-pass1", "1.1.1.1").await;
    assert_eq!(wrong.status, StatusCode::UNAUTHORIZED);
    // Pending elsewhere is no reason to tell a person with an active tenant so.
    h.internal.set_memberships(&id, &[T2], &[T1]);
    assert_eq!(h.claims(&h.token(&h.session("", "ada@example.com", "1.1.1.2").await).await).tid, T2);
}

// An unverified member gets a code and the step cookie, never a session cookie.
#[tokio::test]
async fn unverified_sign_in_sends_a_code() {
    let h = harness().await;
    h.internal.set_signup_tenant(T1);
    let a = h.post("/auth/v1/sign-up", json!({"email": "new@example.com", "password": PW, "givenName": "A", "familyName": "B"}), None, "1.1.1.1").await;
    assert_eq!((a.status, &a.body), (StatusCode::OK, &json!({"next": "verify"})));
    assert!(a.cookie("chelabase_login").is_some() && a.cookie("chelabase_session").is_none());
    let b = h.sign_in("", "new@example.com", PW, "1.1.1.1").await;
    assert_eq!((b.status, &b.body), (StatusCode::OK, &json!({"next": "verify"})));
    assert!(b.cookie("chelabase_login").is_some() && b.cookie("chelabase_session").is_none());
    // Until m11 the code step is Zitadel's, so it refuses an identity user: never a session.
    let done = h.post("/auth/v1/verify-email", json!({"code": "000000"}), Some(&b.cookie("chelabase_login").unwrap()), "1.1.1.1").await;
    assert_eq!(done.status, StatusCode::BAD_REQUEST);
    assert!(done.cookies.is_empty());
    let sent = h.identity.codes_sent();
    assert_eq!(sent.len(), 2);
    assert!(sent.iter().all(|c| c.kind == chelabase_identity::CodeKind::Verify && c.email == "new@example.com"));
}

// m2's audit: a new code resets the guess counter, so issuing has to be capped per user and
// kind, however many addresses ask. 5 codes × 5 guesses = at most 25 guesses an hour.
#[tokio::test]
async fn code_issuance_is_limited_per_user_and_kind() {
    let h = harness().await;
    h.internal.set_signup_tenant(T1);
    let a = h.post("/auth/v1/sign-up", json!({"email": "new@example.com", "password": PW, "givenName": "A", "familyName": "B"}), None, "1.1.1.1").await;
    assert_eq!(a.status, StatusCode::OK);
    for i in 0..9 {
        let b = h.sign_in("", "new@example.com", PW, &format!("4.4.4.{i}")).await;
        assert_eq!((b.status, b.body), (StatusCode::OK, json!({"next": "verify"})), "same answer once capped");
    }
    assert_eq!(h.identity.codes_sent().len(), 5, "capped whatever the address");
}

// AC 7.
#[tokio::test]
async fn setup_claim_binds_identity_user() {
    let h = harness_with(true, 1).await;
    h.internal.set_setup_code(SETUP_CODE);
    h.internal.set_signup_tenant(T1);
    let body = json!({"email": "first@example.com", "password": PW, "givenName": "Ann", "familyName": "Lee", "setupCode": SETUP_CODE});
    let a = h.post("/auth/v1/sign-up", body, None, "1.1.1.1").await;
    assert_eq!((a.status, &a.body), (StatusCode::OK, &json!({"next": "studio"})));
    let user = h.identity.client().search_users(&Pool::Members, "first@example.com", 10, 0).await.unwrap().users.remove(0);
    assert_eq!(user.name, "Ann Lee");
    assert!(user.email_verified, "verification is off here");
    assert_eq!(
        h.internal.bodies(CLAIMED_CALL),
        vec![json!({"org": "o1", "user_id": user.id, "nonce": support::internal::CLAIM_NONCE})]
    );
    let claims = h.claims(&h.token(&a.cookie("chelabase_session").unwrap()).await);
    assert_eq!((claims.sub, claims.tid), (user.id, T1));
}

// AC 7: a failed bind deletes the identity user, and a retry with the same email works.
#[tokio::test]
async fn setup_claim_bind_failure_deletes_user() {
    let h = harness_with(true, 1).await;
    h.internal.set_setup_code(SETUP_CODE);
    h.internal.set_signup_tenant(T1);
    let body = json!({"email": "first@example.com", "password": PW, "givenName": "A", "familyName": "B", "setupCode": SETUP_CODE});
    for (status, want, code) in [
        (500, StatusCode::SERVICE_UNAVAILABLE, "unavailable"),
        (403, StatusCode::FORBIDDEN, "setup_code_invalid"),
    ] {
        h.internal.set_claimed_status(Some(status));
        let a = h.post("/auth/v1/sign-up", body.clone(), None, &format!("2.2.2.{status}")).await;
        assert_eq!((a.status, a.body["error"]["code"].as_str()), (want, Some(code)), "{status}");
        assert_eq!(h.users_with("first@example.com").await, 0, "deleted after a {status}");
        assert_eq!(h.sessions(), 0);
    }
    h.internal.set_claimed_status(None);
    let a = h.post("/auth/v1/sign-up", body, None, "2.2.2.9").await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.body);
    assert_eq!(h.users_with("first@example.com").await, 1);
}

// #121 audit S2: the bind failed and so did deleting the account. The claimant's retry with
// the code and the same password heals the claim (`claimed` again for that user writes the
// row), so the install keeps its admin. A wrong password heals nothing.
#[tokio::test]
async fn setup_claim_double_failure_heals_on_retry() {
    let h = harness_with(true, 1).await;
    h.internal.set_setup_code(SETUP_CODE);
    h.internal.set_signup_tenant(T1);
    let body = json!({"email": "first@example.com", "password": PW, "givenName": "A", "familyName": "B", "setupCode": SETUP_CODE});
    h.internal.set_claimed_status(Some(500));
    h.identity.fail("DELETE /internal/v1/pools/members/users/", 503, "unavailable", 1);
    let a = h.post("/auth/v1/sign-up", body.clone(), None, "2.2.3.1").await;
    assert_eq!(a.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(h.users_with("first@example.com").await, 1, "the delete failed: the account stays");
    h.internal.set_claimed_status(None);
    let mut wrong = body.clone();
    wrong["password"] = json!("Another-pass1");
    let a = h.post("/auth/v1/sign-up", wrong, None, "2.2.3.2").await;
    assert_eq!((a.status, a.body["error"]["code"].as_str()), (StatusCode::CONFLICT, Some("email_taken")));
    assert_eq!(h.internal.bodies(CLAIMED_CALL).len(), 1, "a wrong password claims nothing");
    assert_eq!(h.sessions(), 0);
    let a = h.post("/auth/v1/sign-up", body, None, "2.2.3.3").await;
    assert_eq!((a.status, &a.body), (StatusCode::OK, &json!({"next": "studio"})), "the retry heals");
    let user = h.identity.client().search_users(&Pool::Members, "first@example.com", 10, 0).await.unwrap().users.remove(0);
    assert_eq!(h.internal.bodies(CLAIMED_CALL).last().unwrap()["user_id"], json!(user.id));
    let claims = h.claims(&h.token(&a.cookie("chelabase_session").unwrap()).await);
    assert_eq!((claims.sub, claims.tid), (user.id, T1));
}

// #121 audit S1: with verification on, an open sign-up writes no row for an address nobody
// has proven. The join waits for the verified email (m11's verify-email).
#[tokio::test]
async fn unverified_sign_up_writes_no_member_row() {
    let h = harness().await; // verification on
    h.internal.set_signup_tenant(T1);
    let a = h.post("/auth/v1/sign-up", json!({"email": "victim@example.com", "password": PW, "givenName": "V", "familyName": "W"}), None, "1.1.1.1").await;
    assert_eq!((a.status, &a.body), (StatusCode::OK, &json!({"next": "verify"})));
    assert_eq!(h.internal.bodies("POST /internal/signup/joined"), Vec::<Value>::new(), "a pending row for an unverified address");
    assert_eq!(h.users_with("victim@example.com").await, 1, "the account stays, waiting for its code");
}

// The loser of the claim race, and an open sign-up, wait as pending: the row is written at
// sign-up, because a member token never makes one.
#[tokio::test]
async fn sign_ups_without_the_claim_wait_as_pending() {
    let h = harness_with(true, 1).await;
    h.internal.set_signup_tenant(T1);
    h.internal.set_setup_code(SETUP_CODE);
    h.internal.set_claimed_unbound();
    let body = json!({"email": "second@example.com", "password": PW, "givenName": "A", "familyName": "B", "setupCode": SETUP_CODE});
    let a = h.post("/auth/v1/sign-up", body, None, "2.2.2.1").await;
    assert_eq!((a.status, &a.body), (StatusCode::OK, &json!({"next": "pending"})), "race loser");
    assert!(a.cookies.iter().all(|c| !c.starts_with("chelabase_session=v")));

    let h = harness_with(true, 1).await;
    h.internal.set_signup_tenant(T1);
    let body = json!({"email": "open@example.com", "password": PW, "givenName": "A", "familyName": "B"});
    let a = h.post("/auth/v1/sign-up", body, None, "2.2.2.2").await;
    assert_eq!((a.status, &a.body), (StatusCode::OK, &json!({"next": "pending"})), "open sign-up");
    let user = h.identity.client().search_users(&Pool::Members, "open@example.com", 10, 0).await.unwrap().users.remove(0);
    assert_eq!(h.internal.bodies("POST /internal/signup/joined"), vec![json!({"org": "o1", "user_id": user.id})]);
    assert_eq!(h.sessions(), 0);
}

// A join that didn't happen undoes the account, as a failed bind does.
#[tokio::test]
async fn a_failed_join_deletes_the_user() {
    let h = harness_with(true, 1).await;
    let body = json!({"email": "open@example.com", "password": PW, "givenName": "A", "familyName": "B"});
    h.internal.set_joined_status(Some(500));
    let a = h.post("/auth/v1/sign-up", body.clone(), None, "2.2.2.1").await;
    assert_eq!(a.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(h.users_with("open@example.com").await, 0);
    h.internal.set_joined_status(None);
    h.internal.set_joined_closed();
    let a = h.post("/auth/v1/sign-up", body, None, "2.2.2.2").await;
    assert_eq!((a.status, a.body["error"]["code"].as_str()), (StatusCode::FORBIDDEN, Some("sign_up_closed")));
    assert_eq!(h.users_with("open@example.com").await, 0);
}

// AC 8.
#[tokio::test]
async fn sign_in_rate_limits_kept() {
    let h = harness().await;
    h.member("ada@example.com", &[T1]);
    for i in 0..10 {
        let a = h.sign_in("", &format!("u{i}@example.com"), "Wrong-pass1", "5.5.5.5").await;
        assert_eq!(a.status, StatusCode::UNAUTHORIZED);
    }
    let a = h.sign_in("", "ada@example.com", PW, "5.5.5.5").await;
    assert_eq!((a.status, a.body), (StatusCode::TOO_MANY_REQUESTS, err("rate_limited", messages::TOO_MANY)));
    // Per email, across addresses; a no-membership sign-in counts like a wrong password.
    h.member("none@example.com", &[]);
    for i in 0..5 {
        let a = h.sign_in("", "none@example.com", PW, &format!("5.5.6.{i}")).await;
        assert_eq!(a.status, StatusCode::UNAUTHORIZED);
    }
    assert_eq!(h.sign_in("", "none@example.com", PW, "5.5.6.9").await.status, StatusCode::TOO_MANY_REQUESTS);
    // Right passwords spend nothing, and neither does an outage.
    h.member("bob@example.com", &[T1]);
    for i in 0..6 {
        assert_eq!(h.sign_in("", "bob@example.com", PW, &format!("5.5.7.{i}")).await.status, StatusCode::OK);
    }
    h.identity.set_down(true);
    for i in 0..6 {
        let a = h.sign_in("", "cat@example.com", "Wrong-pass1", &format!("5.5.8.{i}")).await;
        assert_eq!(a.status, StatusCode::SERVICE_UNAVAILABLE);
    }
    h.identity.set_down(false);
    assert_eq!(h.sign_in("", "cat@example.com", "Wrong-pass1", "5.5.9.1").await.status, StatusCode::UNAUTHORIZED);
}

// AC 9, on auth's clock: identity's session is still live, but older than the lifetime.
#[tokio::test]
async fn token_refused_after_absolute_lifetime() {
    let h = harness().await;
    h.member("ada@example.com", &[T1]);
    let cookie = h.session("", "ada@example.com", "1.1.1.1").await;
    h.clock.advance(Duration::from_secs(29 * 24 * 3600));
    assert_eq!(h.token(&cookie).await.status, StatusCode::OK);
    h.clock.advance(Duration::from_secs(2 * 24 * 3600));
    let a = h.token(&cookie).await;
    assert_eq!(a.status, StatusCode::UNAUTHORIZED);
    assert_eq!(h.sessions(), 0, "revoked once found too old");
    // `/token` reads the session without extending it: by id, never by token.
    let calls = h.identity.calls();
    assert!(calls.iter().any(|c| c.starts_with("GET /internal/v1/pools/members/sessions/")), "{calls:?}");
    assert!(!calls.iter().any(|c| c.ends_with("/sessions/lookup")), "{calls:?}");
}

// AC 10.
#[tokio::test]
async fn token_tid_bound_to_sealed_tenant() {
    let h = harness().await;
    h.member("ada@example.com", &[T1, T2]);
    let cookie = h.session(&format!("?tenant={T1}"), "ada@example.com", "1.1.1.1").await;
    let studio = [("origin", STUDIO_ORIGIN), ("x-chelabase-studio", "1")];
    let a = h.token_as(&format!("/auth/v1/token?tenant={T2}"), Some(&cookie), &studio).await;
    assert_eq!(a.status, StatusCode::UNAUTHORIZED, "another tenant, though a member of it");
    assert!(a.body.get("access_token").is_none());
    let same = h.token_as(&format!("/auth/v1/token?tenant={T1}"), Some(&cookie), &studio).await;
    assert_eq!(h.claims(&same).tid, T1);
}

// The Zitadel-era session (accept-invite, until m11) mints nothing.
#[tokio::test]
async fn a_zitadel_session_cookie_mints_nothing() {
    let h = harness().await;
    let sealed = auth::cookie::seal_session(
        &h.cookie_key,
        &auth::cookie::SessionState::Zitadel {
            session_id: "1".into(),
            session_token: "t".into(),
            exp: auth::cookie::now_secs() + 60,
        },
    );
    assert_eq!(h.token(&format!("chelabase_session={sealed}")).await.status, StatusCode::UNAUTHORIZED);
}
