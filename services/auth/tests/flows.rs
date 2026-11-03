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
        json!({"email": "nobody@example.com", "code": "123456", "password": "New-pass-12"}),
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

// T12x (R32): the accept page describes an invite by (Zitadel user, link token) alone.
fn token_hash_hex(token: &str) -> String {
    use sha2::Digest;
    hex::encode(sha2::Sha256::digest(token.as_bytes()))
}

/// An identity user id.
const U1: &str = "0195b3e6-7a10-7c3e-9d50-6f8a3b1c2d4e";

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
    h.internal.set_invite(U1, &token_hash_hex(INVITE_TOKEN), want.clone());
    let a = h.describe(json!({"user": U1, "t": INVITE_TOKEN}), "1.1.1.1").await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.body);
    assert_eq!(a.body["inviter_name"], "Alice");
    assert_eq!(a.body["inviter_email"], "alice@x.com");
    assert_eq!(a.body["tenant_name"], "acme");
    assert_eq!(a.body["_cache_control"], "no-store");
    // The old GET is gone: a link's secrets never go in a URL.
    let old = h.get(&format!("/auth/v1/invite?user={U1}&t={INVITE_TOKEN}"), "1.1.1.1").await;
    assert_eq!(old.status, StatusCode::METHOD_NOT_ALLOWED);
    // The orchestrator was asked with the hash, never the token itself, and not in the URL.
    let bodies = h.internal.bodies("POST /internal/invites/describe");
    assert_eq!(bodies, vec![json!({"user": U1, "token_hash": token_hash_hex(INVITE_TOKEN)})]);
    assert!(!bodies[0].to_string().contains(INVITE_TOKEN));
    assert_eq!(h.internal.queries("POST /internal/invites/describe"), vec![String::new()]);
}

#[tokio::test]
async fn describe_invite_wrong_anything_is_the_same_404() {
    let h = harness().await;
    h.internal.set_invite(U1, &token_hash_hex(INVITE_TOKEN), json!({"inviter_name": "A", "inviter_email": null, "tenant_name": "acme"}));
    let other = "ZYXWVUTSRQPONMLKJIHGFEDCBAzyxwvutsrqponml-_";
    assert_eq!(other.len(), 43);
    let short = &INVITE_TOKEN[..42];
    let mut answers = Vec::new();
    for body in [
        json!({"user": U1, "t": other}),
        json!({"user": "0195b3e6-7a10-7c3e-9d50-6f8a3b1c2d4f", "t": INVITE_TOKEN}),
        json!({"user": U1, "t": short}),
        json!({"user": U1}),
        json!({}),
        json!({"user": "0195b3e6-7a10-7c3e-9d50-6f8a3b1c2d4x", "t": INVITE_TOKEN}),
        json!({"user": U1, "t": format!("{}+", &INVITE_TOKEN[..42])}),
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
    let body = json!({"user": U1, "t": INVITE_TOKEN});
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
    assert_eq!((a.status, a.body["setupRequired"].clone()), (StatusCode::OK, json!(false)));
    h.internal.set_setup_code(SETUP_CODE);
    let a = h.get("/auth/v1/instance", "1.1.1.1").await;
    assert_eq!((a.status, a.body["setupRequired"].clone()), (StatusCode::OK, json!(true)));
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
    assert_eq!((a.status, a.body), (StatusCode::OK, json!({"setupRequired": false, "signUpAllowed": false})));
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
    // A wrong code: never a session.
    let done = h.post("/auth/v1/verify-email", json!({"code": "000000"}), Some(&b.cookie("chelabase_login").unwrap()), "1.1.1.1").await;
    assert_eq!((done.status, done.body), (StatusCode::BAD_REQUEST, err("invalid_code", messages::BAD_CODE)));
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

// ---- #124 (m11): invites, verification and resets on the identity service ---------------------

use chelabase_identity::rules::MEMBER_REMOVED_BAN_REASON;
use chelabase_identity::{CodeKind, CodeRequest, Delivery};

const NEW_PW: &str = "Fresh-pass-12";

/// A link token (43 base64url characters), distinct per `n`.
fn link_token(n: u32) -> String {
    format!("tok{n:0>40}")
}

fn accept_code(user: &str, t: &str, code: &str, pw: &str) -> Value {
    json!({"user": user, "t": t, "code": code, "password": pw})
}

fn accept_notice(t: &str, email: &str, pw: &str) -> Value {
    json!({"t": t, "email": email, "password": pw})
}

impl Harness {
    /// An open invite of T1 for `user`, whose link token is `t`.
    fn open_invite_for(&self, user: &str, t: &str) {
        self.internal.set_signup_tenant(T1);
        self.internal.set_invite(
            user,
            &token_hash_hex(t),
            json!({"inviter_name": "Alice", "inviter_email": "alice@x.com", "tenant_name": "acme"}),
        );
        self.internal.set_open_invite(user, true);
    }

    /// A code of `kind` for `user`, handed back as a link delivery would.
    async fn code_for(&self, user: &str, kind: CodeKind) -> String {
        self.identity
            .client()
            .create_code(&Pool::Members, &CodeRequest::new(user, kind, Delivery::Return))
            .await
            .unwrap()
            .code
            .unwrap()
    }

    /// A new person invited to T1 with link token `t`: their user id and invite code.
    async fn invitee(&self, email: &str, t: &str) -> (String, String) {
        let id = self.identity.with_invited_user(&Pool::Members, email);
        self.open_invite_for(&id, t);
        let code = self.code_for(&id, CodeKind::Invite).await;
        (id, code)
    }

    async fn accept(&self, body: Value, ip: &str) -> Answer {
        self.post("/auth/v1/accept-invite", body, None, ip).await
    }

    /// Whether `email` signs in to identity with `pw`.
    async fn signs_in(&self, email: &str, pw: &str) -> bool {
        self.identity.client().sign_in(&Pool::Members, email, pw).await.is_ok()
    }

    /// The latest mailed code of `kind` for `user`.
    fn mailed(&self, user: &str, kind: CodeKind) -> Option<String> {
        self.identity
            .codes_sent()
            .into_iter()
            .rev()
            .find(|c| c.user_id == user && c.kind == kind)
            .map(|c| c.code)
    }

    /// Waits for a background send (a reset) to land: up to a second.
    async fn mailed_eventually(&self, user: &str, kind: CodeKind) -> Option<String> {
        for _ in 0..100 {
            if let Some(code) = self.mailed(user, kind) {
                return Some(code);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        None
    }
}

fn session_set(a: &Answer) -> bool {
    a.cookies.iter().any(|c| c.starts_with("chelabase_session=") && !c.contains("Max-Age=0"))
}

// AC 1: the code sets the first password and verifies the email; the member row is activated
// by the orchestrator (member tokens make no row), and the answer is a signed-in session.
#[tokio::test]
async fn accept_invite_signs_in_and_verifies() {
    let h = harness().await;
    let t = link_token(1);
    let (id, code) = h.invitee("bob@example.com", &t).await;
    let a = h.accept(accept_code(&id, &t, &code, NEW_PW), "1.1.1.1").await;
    assert_eq!((a.status, &a.body), (StatusCode::OK, &json!({"next": "studio"})), "{}", a.body);
    assert!(session_set(&a));
    let claims = h.claims(&h.token(&a.cookie("chelabase_session").unwrap()).await);
    assert_eq!((claims.sub.as_str(), claims.tid), (id.as_str(), T1));
    let user = h.identity.user(&Pool::Members, &id).unwrap();
    assert!(user.verified && user.has_password && !user.banned);
    assert!(h.signs_in("bob@example.com", NEW_PW).await);
    assert_eq!(
        h.internal.bodies("POST /internal/invites/accept"),
        vec![json!({"org": "o1", "user": id, "token_hash": token_hash_hex(&t), "email": "bob@example.com", "name": ""})]
    );
}

// AC 1, failure path: the code is single use, and so is a retry ticket. A step after the code
// fails hands back a ticket; the ticket finishes once, then never again.
#[tokio::test]
async fn accept_invite_ticket_single_use() {
    let h = harness().await;
    let t = link_token(2);
    let (id, code) = h.invitee("bob@example.com", &t).await;
    h.identity.fail("POST /internal/v1/pools/members/sessions", 503, "unavailable", 1);
    let a = h.accept(accept_code(&id, &t, &code, NEW_PW), "1.1.1.1").await;
    assert_eq!((a.status, a.body["error"]["code"].as_str()), (StatusCode::SERVICE_UNAVAILABLE, Some("invite_incomplete")), "{}", a.body);
    assert!(!session_set(&a));
    let ticket = a.body["error"]["accept_ticket"].as_str().expect("a ticket").to_string();
    // The code is spent.
    let again = h.accept(accept_code(&id, &t, &code, NEW_PW), "1.1.1.2").await;
    assert_eq!(again.status, StatusCode::BAD_REQUEST, "{}", again.body);
    let with_ticket = json!({"user": id, "t": t, "acceptTicket": ticket, "password": NEW_PW});
    let done = h.accept(with_ticket.clone(), "1.1.1.3").await;
    assert_eq!((done.status, &done.body), (StatusCode::OK, &json!({"next": "studio"})), "{}", done.body);
    assert!(session_set(&done));
    let replay = h.accept(with_ticket, "1.1.1.4").await;
    assert_eq!((replay.status, replay.body), (StatusCode::BAD_REQUEST, err("invalid_request", messages::BAD_INVITE)));
}

// A ticket proves the code, not the password: a retry with another password is refused, with
// no new ticket, and sets nothing.
#[tokio::test]
async fn accept_ticket_needs_the_password_the_code_set() {
    let h = harness().await;
    let t = link_token(3);
    let (id, code) = h.invitee("bob@example.com", &t).await;
    h.internal.set_accept_refusal(Some((503, "unavailable")));
    let a = h.accept(accept_code(&id, &t, &code, NEW_PW), "1.1.1.1").await;
    let ticket = a.body["error"]["accept_ticket"].as_str().expect("a ticket").to_string();
    h.internal.set_accept_refusal(None);
    let wrong = h.accept(json!({"user": id, "t": t, "acceptTicket": ticket, "password": "Other-pass-34"}), "1.1.1.1").await;
    assert_eq!((wrong.status, wrong.body.clone()), (StatusCode::BAD_REQUEST, err("invalid_request", messages::BAD_INVITE)));
    assert!(!session_set(&wrong));
    assert!(h.signs_in("bob@example.com", NEW_PW).await, "the code's password stands");
}

// AC 2, adapted: m9's suspend ends sessions and the access check refuses the member; it bans
// no account. What accept-invite meets instead:
// - an account banned for any reason but ours (an abuse ban outside Chelabase) is refused
//   with 403 `member_suspended` before the code is spent, and stays banned;
// - a member row the orchestrator holds suspended is 403 `member_suspended`, with no session.
#[tokio::test]
async fn accept_invite_suspended_403() {
    let h = harness().await;
    let t = link_token(4);
    let (id, code) = h.invitee("bob@example.com", &t).await;
    h.identity.client().ban(&Pool::Members, &id, Some("abuse")).await.unwrap();
    let a = h.accept(accept_code(&id, &t, &code, NEW_PW), "1.1.1.1").await;
    assert_eq!((a.status, a.body.clone()), (StatusCode::FORBIDDEN, err("member_suspended", messages::MEMBER_SUSPENDED)));
    assert!(!session_set(&a));
    let user = h.identity.user(&Pool::Members, &id).unwrap();
    assert_eq!((user.banned, user.ban_reason.as_deref(), user.has_password), (true, Some("abuse"), false));
    assert!(h.internal.bodies("POST /internal/invites/accept").is_empty());
    // The code wasn't spent.
    h.identity.client().unban(&Pool::Members, &id).await.unwrap();
    h.identity.client().verify_code(&Pool::Members, &id, CodeKind::Invite, &code, Some(NEW_PW)).await.expect("the code still works");

    let h = harness().await;
    let (id, code) = h.invitee("carol@example.com", &t).await;
    h.internal.set_accept_refusal(Some((403, "member_suspended")));
    let a = h.accept(accept_code(&id, &t, &code, NEW_PW), "1.1.1.1").await;
    assert_eq!((a.status, a.body.clone()), (StatusCode::FORBIDDEN, err("member_suspended", messages::MEMBER_SUSPENDED)));
    assert!(!session_set(&a));
    assert_eq!(h.sessions(), 0);
}

// AC 3: a removed member (banned with our reason) with a fresh open invite: the ban is lifted
// and they are signed in, by a code (no password yet) or by their own credentials (an
// established account, which got the code-less notice).
#[tokio::test]
async fn accept_invite_reopens_removed() {
    let h = harness().await;
    let t = link_token(5);
    let (id, code) = h.invitee("bob@example.com", &t).await;
    h.identity.client().ban(&Pool::Members, &id, Some(MEMBER_REMOVED_BAN_REASON)).await.unwrap();
    let a = h.accept(accept_code(&id, &t, &code, NEW_PW), "1.1.1.1").await;
    assert_eq!((a.status, &a.body), (StatusCode::OK, &json!({"next": "studio"})), "{}", a.body);
    assert!(!h.identity.user(&Pool::Members, &id).unwrap().banned);
    assert_eq!(h.claims(&h.token(&a.cookie("chelabase_session").unwrap()).await).tid, T1);

    let id = h.identity.with_user(&Pool::Members, "ann@example.com", PW);
    let t = link_token(6);
    h.open_invite_for(&id, &t);
    h.identity.client().ban(&Pool::Members, &id, Some(MEMBER_REMOVED_BAN_REASON)).await.unwrap();
    let a = h.accept(accept_notice(&t, "Ann@Example.com", PW), "1.1.1.2").await;
    assert_eq!((a.status, &a.body), (StatusCode::OK, &json!({"next": "studio"})), "{}", a.body);
    assert!(!h.identity.user(&Pool::Members, &id).unwrap().banned);
    assert!(h.signs_in("ann@example.com", PW).await, "their own password");
}

// Our ban is lifted only with an open invite that matches the link: a removed member's old
// link, or a wrong token, leaves them banned, and a wrong password on the notice path lifts
// nothing either. A banned account answers each of these as a wrong password, as sign-in does,
// so the notice path tells nobody a banned account's password is right; a foreign ban too.
#[tokio::test]
async fn our_ban_stays_without_a_matching_open_invite() {
    let h = harness().await;
    let id = h.identity.with_user(&Pool::Members, "ann@example.com", PW);
    h.identity.client().ban(&Pool::Members, &id, Some(MEMBER_REMOVED_BAN_REASON)).await.unwrap();
    let wrong = (StatusCode::UNAUTHORIZED, err("invalid_credentials", messages::BAD_CREDENTIALS));
    let t = link_token(7);
    let a = h.accept(accept_notice(&t, "ann@example.com", PW), "1.1.1.1").await;
    assert_eq!((a.status, a.body), wrong.clone(), "no open invite");
    h.open_invite_for(&id, &t);
    let a = h.accept(accept_notice(&link_token(8), "ann@example.com", PW), "1.1.1.2").await;
    assert_eq!((a.status, a.body), wrong.clone(), "another link's token");
    let a = h.accept(accept_notice(&t, "ann@example.com", "Wrong-pass-1"), "1.1.1.3").await;
    assert_eq!((a.status, a.body), wrong.clone(), "a wrong password");
    let user = h.identity.user(&Pool::Members, &id).unwrap();
    assert_eq!((user.banned, user.ban_reason.as_deref()), (true, Some(MEMBER_REMOVED_BAN_REASON)));
    h.identity.client().ban(&Pool::Members, &id, Some("abuse")).await.unwrap();
    let a = h.accept(accept_notice(&t, "ann@example.com", PW), "1.1.1.4").await;
    assert_eq!((a.status, a.body), wrong, "a foreign ban");
    assert_eq!(h.identity.user(&Pool::Members, &id).unwrap().ban_reason.as_deref(), Some("abuse"));
    assert!(h.internal.bodies("POST /internal/invites/accept").is_empty());
}

// AC 8: an established account accepts by signing in as themselves; no password is set and no
// code is used. A wrong password joins nothing.
#[tokio::test]
async fn accept_existing_account_sets_no_password() {
    let h = harness().await;
    let id = h.identity.with_user(&Pool::Members, "ann@example.com", PW);
    let t = link_token(9);
    h.open_invite_for(&id, &t);
    let wrong = h.accept(accept_notice(&t, "ann@example.com", NEW_PW), "1.1.1.1").await;
    assert_eq!(wrong.status, StatusCode::UNAUTHORIZED);
    assert!(h.internal.bodies("POST /internal/invites/accept").is_empty());
    let a = h.accept(accept_notice(&t, "ann@example.com", PW), "1.1.1.2").await;
    assert_eq!((a.status, &a.body), (StatusCode::OK, &json!({"next": "studio"})), "{}", a.body);
    assert_eq!(h.claims(&h.token(&a.cookie("chelabase_session").unwrap()).await).sub, id);
    assert!(h.signs_in("ann@example.com", PW).await && !h.signs_in("ann@example.com", NEW_PW).await);
    assert!(h.identity.calls().iter().all(|c| !c.contains("/codes") && !c.ends_with("/password")), "{:?}", h.identity.calls());
}

// AC 9: an account someone else pre-registered (a password, an unverified email) gets an
// invite code; accepting replaces the old password and ends every session of it.
#[tokio::test]
async fn accept_preregistered_account_drops_old_password() {
    let h = harness().await;
    let id = h.identity.with_unverified_user(&Pool::Members, "bob@example.com", "Squatter-pw1");
    let (squatter, _) = h.identity.with_session(&Pool::Members, &id);
    let t = link_token(10);
    h.open_invite_for(&id, &t);
    let code = h.code_for(&id, CodeKind::Invite).await;
    let a = h.accept(accept_code(&id, &t, &code, NEW_PW), "1.1.1.1").await;
    assert_eq!((a.status, &a.body), (StatusCode::OK, &json!({"next": "studio"})), "{}", a.body);
    assert!(!h.signs_in("bob@example.com", "Squatter-pw1").await);
    assert!(h.identity.client().session_by_id(&Pool::Members, &squatter).await.is_err(), "the squatter's session is gone");
    assert!(h.identity.user(&Pool::Members, &id).unwrap().verified);
}

// Live gate (ported): an invitee whose identity user is gone gets 410 `invite_revoked`.
#[tokio::test]
async fn accept_invite_for_a_deleted_user_is_revoked() {
    let h = harness().await;
    let t = link_token(11);
    let (id, code) = h.invitee("bob@example.com", &t).await;
    h.identity.client().delete_user(&Pool::Members, &id).await.unwrap();
    let a = h.accept(accept_code(&id, &t, &code, NEW_PW), "1.1.1.1").await;
    assert_eq!((a.status, a.body), (StatusCode::GONE, err("invite_revoked", messages::INVITE_REVOKED)));
}

// Live gate (ported): a link whose stored invite expired is 410 `invite_expired`; with the
// invite open, a wrong code is 400 and the right one still works.
#[tokio::test]
async fn a_wrong_code_is_expired_only_when_the_stored_invite_expired() {
    let h = harness().await;
    let t = link_token(12);
    let id = h.identity.with_invited_user(&Pool::Members, "bob@example.com");
    let code = h.code_for(&id, CodeKind::Invite).await;
    h.internal.set_open_invite(&id, false);
    let a = h.accept(accept_code(&id, &t, "000000", NEW_PW), "1.1.1.1").await;
    assert_eq!((a.status, a.body), (StatusCode::GONE, err("invite_expired", messages::INVITE_EXPIRED)));
    h.open_invite_for(&id, &t);
    let a = h.accept(accept_code(&id, &t, "000000", NEW_PW), "1.1.1.1").await;
    assert_eq!((a.status, a.body), (StatusCode::BAD_REQUEST, err("invalid_request", messages::BAD_INVITE)));
    let a = h.accept(accept_code(&id, &t, &code, NEW_PW), "1.1.1.1").await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.body);
}

// AC 6: a weak password is refused before the code is used, on accept and on confirm, by
// auth's own rule or by identity's: the code still works afterwards.
#[tokio::test]
async fn weak_password_keeps_code() {
    let h = harness().await;
    let t = link_token(13);
    let (id, code) = h.invitee("bob@example.com", &t).await;
    let a = h.accept(accept_code(&id, &t, &code, "short"), "1.1.1.1").await;
    assert_eq!((a.status, a.body), (StatusCode::BAD_REQUEST, err("weak_password", messages::WEAK_PASSWORD)));
    h.identity.fail("POST /internal/v1/pools/members/codes/verify", 400, "weak_password", 1);
    let a = h.accept(accept_code(&id, &t, &code, NEW_PW), "1.1.1.1").await;
    assert_eq!((a.status, a.body.clone()), (StatusCode::BAD_REQUEST, err("weak_password", messages::WEAK_PASSWORD)));
    assert!(a.body["error"].get("accept_ticket").is_none(), "nothing was spent");
    let a = h.accept(accept_code(&id, &t, &code, NEW_PW), "1.1.1.1").await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.body);

    let id = h.member("ann@example.com", &[T1]);
    let code = h.code_for(&id, CodeKind::Reset).await;
    let confirm = |pw: &str| json!({"email": "ann@example.com", "code": code, "password": pw});
    let a = h.post("/auth/v1/password-reset/confirm", confirm("alllowercase1!"), None, "2.2.2.2").await;
    assert_eq!((a.status, a.body), (StatusCode::BAD_REQUEST, err("weak_password", messages::WEAK_PASSWORD)));
    h.identity.fail("POST /internal/v1/pools/members/codes/verify", 400, "weak_password", 1);
    let a = h.post("/auth/v1/password-reset/confirm", confirm(NEW_PW), None, "2.2.2.2").await;
    assert_eq!((a.status, a.body), (StatusCode::BAD_REQUEST, err("weak_password", messages::WEAK_PASSWORD)));
    let a = h.post("/auth/v1/password-reset/confirm", confirm(NEW_PW), None, "2.2.2.2").await;
    assert_eq!((a.status, a.body), (StatusCode::OK, json!({})));
    assert!(h.signs_in("ann@example.com", NEW_PW).await);
}

// AC 4: a confirmed reset ends every session of the user: an earlier session's `/token` is 401.
#[tokio::test]
async fn reset_confirm_revokes_sessions() {
    let h = harness().await;
    let id = h.member("ann@example.com", &[T1]);
    let cookie = h.session("", "ann@example.com", "1.1.1.1").await;
    assert_eq!(h.token(&cookie).await.status, StatusCode::OK);
    let a = h.post("/auth/v1/password-reset", json!({"email": "ANN@example.com"}), None, "1.1.1.1").await;
    assert_eq!((a.status, a.body), (StatusCode::OK, json!({})));
    let code = h.mailed_eventually(&id, CodeKind::Reset).await.expect("a reset code is mailed");
    let link = h.identity.codes_sent().last().unwrap().link.clone().unwrap();
    assert_eq!(link, "http://studio.chelabase.localhost:8000/password-reset", "no id or code in the link");
    let bad = h.post("/auth/v1/password-reset/confirm", json!({"email": "ann@example.com", "code": "000000", "password": NEW_PW}), None, "1.1.1.1").await;
    assert_eq!((bad.status, bad.body), (StatusCode::BAD_REQUEST, err("invalid_code", messages::BAD_CODE)));
    let ok = h.post("/auth/v1/password-reset/confirm", json!({"email": "ann@example.com", "code": code, "password": NEW_PW}), None, "1.1.1.1").await;
    assert_eq!((ok.status, ok.body), (StatusCode::OK, json!({})));
    assert_eq!(h.token(&cookie).await.status, StatusCode::UNAUTHORIZED);
    assert_eq!(h.sessions(), 0);
    assert!(h.signs_in("ann@example.com", NEW_PW).await && !h.signs_in("ann@example.com", PW).await);
}

// AC 5: a reset for an unknown email is answered like a known one: status, body and the padded
// time; only the known one gets a code. A confirm for an unknown email is a bad code.
#[tokio::test]
async fn reset_unknown_email_indistinguishable() {
    const PAD_MS: u64 = 300;
    let h = harness_with(false, PAD_MS).await;
    let id = h.member("ann@example.com", &[T1]);
    let mut answers = Vec::new();
    for email in ["ann@example.com", "nobody@example.com"] {
        let start = Instant::now();
        let a = h.post("/auth/v1/password-reset", json!({"email": email}), None, "1.1.1.1").await;
        let took = start.elapsed();
        assert!(took >= Duration::from_millis(PAD_MS) && took < Duration::from_millis(PAD_MS + 250), "{email}: {took:?}");
        answers.push((a.status, a.body, a.cookies));
    }
    assert_eq!(answers[0], answers[1]);
    assert!(h.mailed_eventually(&id, CodeKind::Reset).await.is_some());
    assert_eq!(h.identity.codes_sent().len(), 1);
    let a = h.post("/auth/v1/password-reset/confirm", json!({"email": "nobody@example.com", "code": "123456", "password": NEW_PW}), None, "1.1.1.1").await;
    assert_eq!((a.status, a.body), (StatusCode::BAD_REQUEST, err("invalid_code", messages::BAD_CODE)));
}

// A removed member (banned with our reason) can reset their password, which anyone can, but
// that lifts nothing: no session, the ban stays, and a reset code never accepts an invite.
#[tokio::test]
async fn a_removed_members_own_reset_code_never_reactivates() {
    let h = harness().await;
    let id = h.member("ann@example.com", &[]);
    h.identity.client().ban(&Pool::Members, &id, Some(MEMBER_REMOVED_BAN_REASON)).await.unwrap();
    let t = link_token(14);
    h.open_invite_for(&id, &t);
    let code = h.code_for(&id, CodeKind::Reset).await;
    let a = h.accept(accept_code(&id, &t, &code, NEW_PW), "1.1.1.1").await;
    assert_eq!(a.status, StatusCode::BAD_REQUEST, "a reset code is no invite code: {}", a.body);
    assert!(h.identity.user(&Pool::Members, &id).unwrap().banned);
    let a = h.post("/auth/v1/password-reset/confirm", json!({"email": "ann@example.com", "code": code, "password": NEW_PW}), None, "1.1.1.1").await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.body);
    let s = h.sign_in("", "ann@example.com", NEW_PW, "1.1.1.1").await;
    assert_eq!(s.status, StatusCode::UNAUTHORIZED);
    assert!(!session_set(&s));
    let user = h.identity.user(&Pool::Members, &id).unwrap();
    assert_eq!((user.banned, user.ban_reason.as_deref()), (true, Some(MEMBER_REMOVED_BAN_REASON)));
}

// AC 7: the limits stay: accept-invite (20 failures an hour per address), verify (10 a
// minute), resend (20 an hour per address), reset (5 an hour per email), confirm (10 an hour
// per email across addresses); and codes go out at most 5 an hour per user and kind, however
// many addresses or flows ask (resend, reset, the decoy's reset).
#[tokio::test]
async fn member_code_limits_kept() {
    let h = harness().await;
    // Failures for 20 different invitees spend the address's budget; one invitee alone is
    // stopped at 5.
    let (id, _) = h.invitee("bob@example.com", &link_token(15)).await;
    for i in 0..5 {
        let a = h.accept(accept_code(&id, &link_token(15), "000000", NEW_PW), &format!("1.1.2.{i}")).await;
        assert_eq!(a.status, StatusCode::BAD_REQUEST, "{i}");
    }
    assert_eq!(h.accept(accept_code(&id, &link_token(15), "000000", NEW_PW), "1.1.2.9").await.status, StatusCode::TOO_MANY_REQUESTS);
    for i in 0..20 {
        let a = h.accept(accept_code(&Uuid::new_v4().to_string(), &link_token(15), "000000", NEW_PW), "1.1.1.1").await;
        assert_eq!(a.status, StatusCode::BAD_REQUEST, "{i}");
    }
    let a = h.accept(accept_code(&Uuid::new_v4().to_string(), &link_token(15), "000000", NEW_PW), "1.1.1.1").await;
    assert_eq!(a.status, StatusCode::TOO_MANY_REQUESTS);

    for i in 0..10 {
        assert_eq!(h.post("/auth/v1/verify-email", json!({"code": "000000"}), None, "2.2.2.2").await.status, StatusCode::GONE, "{i}");
    }
    assert_eq!(h.post("/auth/v1/verify-email", json!({"code": "000000"}), None, "2.2.2.2").await.status, StatusCode::TOO_MANY_REQUESTS);
    for i in 0..20 {
        assert_eq!(h.post("/auth/v1/resend-code", json!({}), None, "3.3.3.3").await.status, StatusCode::OK, "{i}");
    }
    assert_eq!(h.post("/auth/v1/resend-code", json!({}), None, "3.3.3.3").await.status, StatusCode::TOO_MANY_REQUESTS);

    let ann = h.member("ann@example.com", &[T1]);
    for i in 0..5 {
        let a = h.post("/auth/v1/password-reset", json!({"email": "ann@example.com"}), None, &format!("4.4.4.{i}")).await;
        assert_eq!(a.status, StatusCode::OK, "{i}");
    }
    let a = h.post("/auth/v1/password-reset", json!({"email": "ann@example.com"}), None, "4.4.5.5").await;
    assert_eq!(a.status, StatusCode::TOO_MANY_REQUESTS);
    // The decoy's reset (a sign-up for her email) shares her per-user code budget.
    h.post("/auth/v1/sign-up", signup("ann@example.com", PW), None, "4.4.6.6").await;
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let resets = h.identity.codes_sent().into_iter().filter(|c| c.user_id == ann && c.kind == CodeKind::Reset).count();
    assert_eq!(resets, 5, "capped per user and kind");

    for i in 0..10 {
        let a = h.post("/auth/v1/password-reset/confirm", json!({"email": "ann@example.com", "code": "000000", "password": NEW_PW}), None, &format!("5.5.5.{i}")).await;
        assert_eq!(a.status, StatusCode::BAD_REQUEST, "{i}");
    }
    let a = h.post("/auth/v1/password-reset/confirm", json!({"email": "ann@example.com", "code": "000000", "password": NEW_PW}), None, "5.5.6.6").await;
    assert_eq!(a.status, StatusCode::TOO_MANY_REQUESTS);
}

// Resend issues within the per-user code budget too: a sign-up's code plus 6 resends from
// fresh addresses is 5 codes.
#[tokio::test]
async fn resend_codes_are_limited_per_user() {
    let h = harness().await;
    let a = h.post("/auth/v1/sign-up", signup("new@example.com", PW), None, "1.1.1.1").await;
    let login = a.cookie("chelabase_login").unwrap();
    for i in 0..6 {
        h.post("/auth/v1/resend-code", json!({}), Some(&login), &format!("6.6.6.{i}")).await;
    }
    let sent = h.identity.codes_sent();
    assert_eq!(sent.len(), 5, "{sent:?}");
    assert!(sent.iter().all(|c| c.kind == CodeKind::Verify && c.link.as_deref() == Some("http://studio.chelabase.localhost:8000/verify")));
}

// #121 TODO: with verification on, an open sign-up joins once its email is verified, as a
// pending member; the answer is `pending`.
#[tokio::test]
async fn verify_email_joins_an_open_sign_up() {
    let h = harness().await;
    h.internal.set_signup_tenant(T1);
    let a = h.post("/auth/v1/sign-up", signup("new@example.com", PW), None, "1.1.1.1").await;
    let login = a.cookie("chelabase_login").unwrap();
    let id = h.identity.user_by_email(&Pool::Members, "new@example.com").unwrap();
    let code = h.mailed(&id, CodeKind::Verify).unwrap();
    let done = h.post("/auth/v1/verify-email", json!({"code": code}), Some(&login), "1.1.1.1").await;
    assert_eq!((done.status, &done.body), (StatusCode::OK, &json!({"next": "pending"})), "{}", done.body);
    assert!(h.identity.user(&Pool::Members, &id).unwrap().verified);
    assert_eq!(h.internal.bodies("POST /internal/signup/joined"), vec![json!({"org": "o1", "user_id": id})]);
    assert!(done.cookies.iter().any(|c| c.starts_with("chelabase_login=;")));
    assert!(!session_set(&done));
}

// A member who already has a row (the fresh-install admin, an approved member) verifies and
// lands in Studio: no join is asked, whatever sign-up's state.
#[tokio::test]
async fn verify_email_of_a_member_signs_in() {
    let h = harness().await;
    let id = h.identity.with_unverified_user(&Pool::Members, "ann@example.com", PW);
    h.internal.set_memberships(&id, &[T1], &[]);
    h.internal.set_joined_closed();
    let a = h.sign_in("", "ann@example.com", PW, "1.1.1.1").await;
    assert_eq!(a.body, json!({"next": "verify"}));
    let code = h.mailed(&id, CodeKind::Verify).unwrap();
    let done = h.post("/auth/v1/verify-email", json!({"code": code}), Some(&a.cookie("chelabase_login").unwrap()), "1.1.1.1").await;
    assert_eq!((done.status, &done.body), (StatusCode::OK, &json!({"next": "studio"})), "{}", done.body);
    assert_eq!(h.claims(&h.token(&done.cookie("chelabase_session").unwrap()).await).tid, T1);
    assert!(h.internal.bodies("POST /internal/signup/joined").is_empty());
}

// Sign-up closed between the sign-up and its code: the visitor is told sign-up is closed
// (403 `sign_up_closed`), the session and the step end, and the account is deleted, so a
// sign-up once it reopens starts afresh.
#[tokio::test]
async fn verify_email_after_sign_up_closed() {
    let h = harness().await;
    let a = h.post("/auth/v1/sign-up", signup("new@example.com", PW), None, "1.1.1.1").await;
    let login = a.cookie("chelabase_login").unwrap();
    let id = h.identity.user_by_email(&Pool::Members, "new@example.com").unwrap();
    let code = h.mailed(&id, CodeKind::Verify).unwrap();
    h.internal.set_joined_closed();
    let done = h.post("/auth/v1/verify-email", json!({"code": code}), Some(&login), "1.1.1.1").await;
    assert_eq!((done.status, done.body.clone()), (StatusCode::FORBIDDEN, err("sign_up_closed", messages::SIGN_UP_CLOSED)));
    assert!(!session_set(&done));
    assert!(done.cookies.iter().any(|c| c.starts_with("chelabase_login=;")), "the step ends");
    assert_eq!(h.users_with("new@example.com").await, 0);
    assert_eq!(h.sessions(), 0);
}

// The join fails after the code is used: the step stays, and a retry with the same (spent)
// code finishes, because the email is verified already.
#[tokio::test]
async fn verify_email_retry_after_a_failed_join_finishes() {
    let h = harness().await;
    h.internal.set_signup_tenant(T1);
    let a = h.post("/auth/v1/sign-up", signup("new@example.com", PW), None, "1.1.1.1").await;
    let login = a.cookie("chelabase_login").unwrap();
    let id = h.identity.user_by_email(&Pool::Members, "new@example.com").unwrap();
    let code = h.mailed(&id, CodeKind::Verify).unwrap();
    h.internal.set_joined_status(Some(500));
    let failed = h.post("/auth/v1/verify-email", json!({"code": code}), Some(&login), "1.1.1.1").await;
    assert_eq!((failed.status, failed.body.clone()), (StatusCode::SERVICE_UNAVAILABLE, err("unavailable", messages::UNAVAILABLE)));
    assert!(failed.cookies.is_empty(), "the step stays");
    assert_eq!(h.users_with("new@example.com").await, 1, "the account stays");
    h.internal.set_joined_status(None);
    let done = h.post("/auth/v1/verify-email", json!({"code": code}), Some(&login), "1.1.1.1").await;
    assert_eq!((done.status, &done.body), (StatusCode::OK, &json!({"next": "pending"})), "{}", done.body);
    // An unverified user's wrong code is still refused.
    let b = h.post("/auth/v1/sign-up", signup("other@example.com", PW), None, "1.1.1.2").await;
    let wrong = h.post("/auth/v1/verify-email", json!({"code": "000000"}), Some(&b.cookie("chelabase_login").unwrap()), "1.1.1.2").await;
    assert_eq!((wrong.status, wrong.body), (StatusCode::BAD_REQUEST, err("invalid_code", messages::BAD_CODE)));
}

// m12: the sign-in page hides "Create account" unless sign-up is open (or the instance waits
// for its first account).
#[tokio::test]
async fn instance_says_whether_sign_up_is_allowed() {
    let h = harness().await;
    for (allowed, reason, want) in [(true, "open", true), (false, "closed", false)] {
        h.internal.set_signup(allowed, reason);
        let a = h.get("/auth/v1/instance", "1.1.1.1").await;
        assert_eq!((a.status, a.body["signUpAllowed"].clone()), (StatusCode::OK, json!(want)), "{reason}");
    }
    h.internal.set_setup_code(SETUP_CODE);
    let a = h.get("/auth/v1/instance", "1.1.1.1").await;
    assert_eq!(a.body, json!({"setupRequired": true, "signUpAllowed": true}));
    let p = harness_mode(false, 1, auth::config::Mode::Platform).await;
    assert_eq!(p.get("/auth/v1/instance", "1.1.1.1").await.body, json!({"setupRequired": false, "signUpAllowed": false}));
}

// The emailed invite link carries no code, so the page sends the code typed from the mail;
// malformed input is refused before any call, and the answer is never cached.
#[tokio::test]
async fn accept_invite_rejects_malformed_input() {
    let h = harness().await;
    let t = link_token(16);
    for body in [
        json!({"user": "123", "t": t, "code": "123456", "password": NEW_PW}),
        json!({"user": U1, "t": "short", "code": "123456", "password": NEW_PW}),
        json!({"user": U1, "t": t, "code": "12345x", "password": NEW_PW}),
        json!({"user": U1, "t": t, "password": NEW_PW}),
        json!({"user": U1, "t": t, "code": "123456", "acceptTicket": "x", "password": NEW_PW}),
        json!({"t": t, "password": NEW_PW}),
        json!({"t": t, "email": "a@example.com", "code": "123456", "password": NEW_PW}),
    ] {
        let a = h.accept(body.clone(), "1.1.1.1").await;
        assert_eq!((a.status, a.body), (StatusCode::BAD_REQUEST, err("invalid_request", messages::BAD_INVITE)), "{body}");
    }
    assert!(h.identity.calls().is_empty(), "{:?}", h.identity.calls());
    assert!(h.internal.bodies("POST /internal/invites/describe").is_empty());
    let resp = h.http.post(format!("{}/auth/v1/accept-invite", h.base)).json(&json!({"t": t, "password": "x"})).send().await.unwrap();
    assert_eq!(resp.headers()["cache-control"], "no-store");
}

// #124 audit B1: a confirmed reset ends every session of the account, so a step cookie made
// before it is dead. A wrong code with it is 410 `sign_in_expired` and the step ends; nothing
// happens to the account: with sign-up closed it isn't deleted, and with it open it doesn't
// join. (A squatter pre-registers the owner's email; the owner takes it back with a reset.)
#[tokio::test]
async fn a_step_cookie_from_before_a_reset_changes_nothing() {
    for open in [false, true] {
        let h = harness().await;
        let id = h.identity.with_unverified_user(&Pool::Members, "own@example.com", "Squatter-pw1");
        let s = h.sign_in("", "own@example.com", "Squatter-pw1", "1.1.1.1").await;
        assert_eq!(s.body, json!({"next": "verify"}));
        let step = s.cookie("chelabase_login").unwrap();
        let code = h.code_for(&id, CodeKind::Reset).await;
        let ok = h
            .post("/auth/v1/password-reset/confirm", json!({"email": "own@example.com", "code": code, "password": NEW_PW}), None, "2.2.2.2")
            .await;
        assert_eq!(ok.status, StatusCode::OK, "{}", ok.body);
        assert_eq!(h.sessions(), 0);
        if open {
            h.internal.set_signup_tenant(T1);
        } else {
            h.internal.set_joined_closed();
        }
        let v = h.post("/auth/v1/verify-email", json!({"code": "000000"}), Some(&step), "1.1.1.3").await;
        assert_eq!((v.status, v.body.clone()), (StatusCode::GONE, err("sign_in_expired", messages::GONE)), "open: {open}");
        assert!(v.cookies.iter().any(|c| c.starts_with("chelabase_login=;")), "the step ends");
        assert!(!session_set(&v));
        assert!(h.identity.user(&Pool::Members, &id).is_some(), "the owner's reclaimed account stays");
        assert!(h.internal.bodies("POST /internal/signup/joined").is_empty(), "no join");
        assert!(h.signs_in("own@example.com", NEW_PW).await);
    }
}

// #124 audit B1: accepting an invite ends every session of a pre-registered account, so the
// squatter's step cookie is never answered `studio` with a session cookie for a dead session.
#[tokio::test]
async fn a_step_cookie_from_before_an_accepted_invite_is_expired() {
    let h = harness().await;
    let id = h.identity.with_unverified_user(&Pool::Members, "bob@example.com", "Squatter-pw1");
    let s = h.sign_in("", "bob@example.com", "Squatter-pw1", "1.1.1.1").await;
    assert_eq!(s.body, json!({"next": "verify"}));
    let step = s.cookie("chelabase_login").unwrap();
    let t = link_token(17);
    h.open_invite_for(&id, &t);
    let code = h.code_for(&id, CodeKind::Invite).await;
    let a = h.accept(accept_code(&id, &t, &code, NEW_PW), "2.2.2.2").await;
    assert_eq!((a.status, &a.body), (StatusCode::OK, &json!({"next": "studio"})), "{}", a.body);
    let v = h.post("/auth/v1/verify-email", json!({"code": "000000"}), Some(&step), "1.1.1.3").await;
    assert_eq!((v.status, v.body.clone()), (StatusCode::GONE, err("sign_in_expired", messages::GONE)));
    assert!(!session_set(&v), "no cookie for a dead session");
    assert!(v.cookies.iter().any(|c| c.starts_with("chelabase_login=;")), "the step ends");
    // The owner's own session is untouched.
    assert_eq!(h.token(&a.cookie("chelabase_session").unwrap()).await.status, StatusCode::OK);
}

// #124 audit S1: the account's own password with a link token that matches no open invite of
// theirs is refused before the orchestrator is asked, even for an active member: no session,
// and the offboarded mark isn't touched.
#[tokio::test]
async fn an_active_member_with_a_foreign_token_is_refused() {
    let h = harness().await;
    h.member("ann@example.com", &[T1]);
    h.internal.set_signup_tenant(T1);
    let a = h.accept(accept_notice(&link_token(18), "ann@example.com", PW), "1.1.1.1").await;
    assert_eq!((a.status, a.body.clone()), (StatusCode::BAD_REQUEST, err("invalid_request", messages::BAD_INVITE)));
    assert!(!session_set(&a));
    assert_eq!(h.sessions(), 0, "the sign-in's session is dropped");
    assert!(h.internal.bodies("POST /internal/invites/accept").is_empty());
    // Another user's open invite and token don't help either.
    let other = h.identity.with_invited_user(&Pool::Members, "bob@example.com");
    let t = link_token(19);
    h.open_invite_for(&other, &t);
    let a = h.accept(accept_notice(&t, "ann@example.com", PW), "1.1.1.2").await;
    assert_eq!(a.status, StatusCode::BAD_REQUEST, "{}", a.body);
    assert!(h.internal.bodies("POST /internal/invites/accept").is_empty());
}
