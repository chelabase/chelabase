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

pub struct Harness {
    pub internal: support::internal::FakeInternal,
    pub cookie_key: SecretKey,
    pub mock: MockZitadel,
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
    let internal = support::internal::FakeInternal::start().await;
    let mock = MockZitadel::start().await;
    mock.set_strict_login(true);
    let cookie_key = SecretKey::generate();
    let config = Config {
        listen: String::new(),
        issuer: mock.base.clone(),
        token: TOKEN.into(),
        cookie_key: cookie_key.clone(),
        org_id: "o1".into(),
        studio_url: "http://studio.chelabase.localhost:8000".into(),
        skip_email_verification: skip,
        secure_cookies: false,
        min_response: Duration::from_millis(min_ms),
        mode,
    };
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
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router(state)).await.unwrap() });
    Harness {
        internal,
        cookie_key,
        mock,
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

fn signin(ar: &str, email: &str, pw: &str) -> Value {
    json!({"authRequest": ar, "email": email, "password": pw})
}
fn signup(ar: &str, email: &str, pw: &str) -> Value {
    json!({"authRequest": ar, "email": email, "password": pw, "givenName": "A", "familyName": "B"})
}

#[tokio::test]
async fn auth_request_says_whether_to_show_sign_up() {
    let h = harness().await;
    let yes = h
        .post(
            "/auth/v1/auth-request",
            json!({"authRequest": h.mock.add_auth_request(true)}),
            None,
            "1.1.1.1",
        )
        .await;
    assert_eq!(
        (yes.status, yes.body.clone()),
        (StatusCode::OK, json!({"signUp": true, "signUpAllowed": true}))
    );
    let plain = h
        .post(
            "/auth/v1/auth-request",
            json!({"authRequest": h.mock.add_auth_request(false)}),
            None,
            "1.1.1.1",
        )
        .await;
    assert_eq!(plain.body, json!({"signUp": false, "signUpAllowed": true}));
    let gone = h
        .post(
            "/auth/v1/auth-request",
            json!({"authRequest": "V2_nope"}),
            None,
            "1.1.1.1",
        )
        .await;
    assert_eq!(
        (gone.status, gone.body),
        (StatusCode::GONE, err("sign_in_expired", messages::GONE))
    );
}

// R37: a closed instance never offers sign-up, even to a request that asked for it.
#[tokio::test]
async fn auth_request_never_offers_a_closed_sign_up() {
    let h = harness().await;
    h.internal.set_signup(false, "closed");
    let asked = h
        .post("/auth/v1/auth-request", json!({"authRequest": h.mock.add_auth_request(true)}), None, "1.1.1.1")
        .await;
    assert_eq!(asked.body, json!({"signUp": false, "signUpAllowed": false}));
    // A gate that can't answer is closed too, and doesn't fail the sign-in page.
    h.internal.set_signup_status(Some(503));
    let unsure = h
        .post("/auth/v1/auth-request", json!({"authRequest": h.mock.add_auth_request(true)}), None, "1.1.1.2")
        .await;
    assert_eq!((unsure.status, unsure.body), (StatusCode::OK, json!({"signUp": false, "signUpAllowed": false})));
    // Platform mode: closed without asking.
    let p = harness_mode(false, 1, auth::config::Mode::Platform).await;
    let platform = p
        .post("/auth/v1/auth-request", json!({"authRequest": p.mock.add_auth_request(true)}), None, "1.1.1.1")
        .await;
    assert_eq!(platform.body, json!({"signUp": false, "signUpAllowed": false}));
    assert!(p.internal.queries("GET /internal/signup/allowed").is_empty());
}

#[tokio::test]
async fn sign_in_finishes_and_sets_the_session_cookie() {
    let h = harness().await;
    h.mock.add_user("ada@example.com", "Secret-pass1", true);
    let ar = h.mock.add_auth_request(false);
    let a = h
        .post(
            "/auth/v1/sign-in",
            signin(&ar, " ADA@example.com ", "Secret-pass1"),
            None,
            "1.1.1.1",
        )
        .await;
    assert_eq!(a.status, StatusCode::OK);
    assert_eq!(a.body["next"], "done");
    assert!(
        a.body["redirect"]
            .as_str()
            .unwrap()
            .starts_with("http://studio.chelabase.localhost:8000/callback?code=")
    );
    assert!(a.cookie("chelabase_session").is_some());
    assert!(a.cookies.iter().any(|c| c.starts_with("chelabase_session=")
        && c.contains("HttpOnly; SameSite=Strict; Path=/auth/v1")));
}

#[tokio::test]
async fn unknown_user_wrong_password_and_locked_give_identical_401s() {
    let h = harness().await;
    h.mock.add_user("ada@example.com", "Secret-pass1", true);
    let locked = h.mock.add_user("lee@example.com", "Secret-pass1", true);
    h.mock.lock(&locked);
    let mut bodies = Vec::new();
    for (email, pw) in [
        ("nobody@example.com", "x"),
        ("ada@example.com", "wrong"),
        ("lee@example.com", "Secret-pass1"),
    ] {
        let a = h
            .post(
                "/auth/v1/sign-in",
                signin(&h.mock.add_auth_request(false), email, pw),
                None,
                "1.1.1.1",
            )
            .await;
        assert_eq!(a.status, StatusCode::UNAUTHORIZED, "{email}");
        assert!(a.cookies.is_empty(), "{email}");
        bodies.push(serde_json::to_string(&a.body).unwrap());
    }
    assert!(
        bodies
            .iter()
            .all(|b| *b == serde_json::to_string(&err("invalid_credentials", messages::BAD_CREDENTIALS)).unwrap()),
        "{bodies:?}"
    );
}

#[tokio::test]
async fn sign_in_with_an_unverified_email_asks_for_the_code() {
    let h = harness().await;
    let id = h.mock.add_user("ada@example.com", "Secret-pass1", false);
    let ar = h.mock.add_auth_request(false);
    let a = h
        .post(
            "/auth/v1/sign-in",
            signin(&ar, "ada@example.com", "Secret-pass1"),
            None,
            "1.1.1.1",
        )
        .await;
    assert_eq!(
        (a.status, a.body.clone()),
        (StatusCode::OK, json!({"next": "verify"}))
    );
    assert_eq!(h.mock.emails(), vec![(id, "verify".to_string())]);
    let login = a.cookie("chelabase_login").unwrap();
    let done = h
        .post(
            "/auth/v1/verify-email",
            json!({"code": EMAIL_CODE}),
            Some(&login),
            "1.1.1.1",
        )
        .await;
    assert_eq!(done.body["next"], "done");
    assert!(
        done.cookies
            .iter()
            .any(|c| c.starts_with("chelabase_login=;") && c.contains("Max-Age=0"))
    );
}

#[tokio::test]
async fn sign_up_then_verify_finishes() {
    let h = harness().await;
    let ar = h.mock.add_auth_request(true);
    let a = h
        .post(
            "/auth/v1/sign-up",
            signup(&ar, "new@example.com", "Secret-pass1"),
            None,
            "1.1.1.1",
        )
        .await;
    assert_eq!(
        (a.status, a.body.clone()),
        (StatusCode::OK, json!({"next": "verify"}))
    );
    let login = a.cookie("chelabase_login").unwrap();
    let bad = h
        .post(
            "/auth/v1/verify-email",
            json!({"code": "000000"}),
            Some(&login),
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
    let done = h
        .post(
            "/auth/v1/verify-email",
            json!({"code": EMAIL_CODE}),
            Some(&login),
            "1.1.1.1",
        )
        .await;
    assert_eq!(done.body["next"], "done");
    assert!(h.mock.user("new@example.com").unwrap().verified);
}

#[tokio::test]
async fn a_taken_email_looks_the_same_and_its_code_never_works() {
    let h = harness().await;
    h.mock.add_user("ada@example.com", "Secret-pass1", true);
    let fresh = h
        .post(
            "/auth/v1/sign-up",
            signup(
                &h.mock.add_auth_request(true),
                "new@example.com",
                "Secret-pass1",
            ),
            None,
            "1.1.1.1",
        )
        .await;
    let taken = h
        .post(
            "/auth/v1/sign-up",
            signup(
                &h.mock.add_auth_request(true),
                "ada@example.com",
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
            signup(&h.mock.add_auth_request(true), "new@example.com", "short"),
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
            signup(
                &h.mock.add_auth_request(true),
                "dev@example.com",
                "Secret-pass1",
            ),
            None,
            "1.1.1.1",
        )
        .await;
    assert_eq!(a.body["next"], "done");
    assert!(h.mock.user("dev@example.com").unwrap().verified);
    assert!(h.mock.emails().is_empty());
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
        signin(&h.mock.add_auth_request(false), "nobody@example.com", "x"),
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
                signin(&h.mock.add_auth_request(false), "ada@example.com", "x"),
                None,
                &format!("1.1.1.{i}"),
            )
            .await;
        assert_eq!(a.status, StatusCode::UNAUTHORIZED);
    }
    let a = h
        .post(
            "/auth/v1/sign-in",
            signin(&h.mock.add_auth_request(false), "ada@example.com", "x"),
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
            signup(
                &h.mock.add_auth_request(true),
                "new@example.com",
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
async fn zitadel_down_is_503_and_an_expired_sign_in_is_410() {
    let h = harness().await;
    let a = h
        .post(
            "/auth/v1/sign-in",
            signin("V2_nope", "ada@example.com", "x"),
            None,
            "1.1.1.1",
        )
        .await;
    assert_eq!(
        (a.status, a.body),
        (StatusCode::GONE, err("sign_in_expired", messages::GONE))
    );
    h.mock.set_down(true);
    let a = h
        .post(
            "/auth/v1/sign-in",
            signin("V2_1", "ada@example.com", "x"),
            None,
            "1.1.1.1",
        )
        .await;
    assert_eq!(
        (a.status, a.body),
        (
            StatusCode::SERVICE_UNAVAILABLE,
            err("unavailable", messages::UNAVAILABLE)
        )
    );
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
    h.mock.add_user("ada@example.com", "Secret-pass1", true);
    let a = h
        .post(
            "/auth/v1/sign-in",
            signin(
                &h.mock.add_auth_request(false),
                "ada@example.com",
                "Secret-pass1",
            ),
            None,
            "1.1.1.1",
        )
        .await;
    assert_eq!(h.mock.session_count(), 1);
    let out = h
        .post(
            "/auth/v1/logout",
            json!({}),
            Some(&a.cookie("chelabase_session").unwrap()),
            "1.1.1.1",
        )
        .await;
    assert_eq!((out.status, out.body), (StatusCode::OK, json!({})));
    assert_eq!(h.mock.session_count(), 0);
    for name in ["chelabase_login=;", "chelabase_session=;"] {
        assert!(
            out.cookies
                .iter()
                .any(|c| c.starts_with(name) && c.contains("Max-Age=0")),
            "{name}"
        );
    }
    let again = h.post("/auth/v1/logout", json!({}), None, "1.1.1.1").await;
    assert_eq!(again.status, StatusCode::OK);
}

// --- Security review fixes -------------------------------------------------------------

// Live finding: Zitadel's session API can't see a user for a moment after CreateUser.
#[tokio::test]
async fn sign_up_waits_for_zitadel_to_see_the_new_user() {
    let h = harness().await;
    h.mock.set_new_user_lag(2);
    let a = h
        .post(
            "/auth/v1/sign-up",
            signup(
                &h.mock.add_auth_request(true),
                "new@example.com",
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
}

// C1: ids go into Zitadel URL paths, so only Zitadel-shaped ids are accepted.
#[tokio::test]
async fn ids_that_are_not_zitadel_ids_never_reach_zitadel() {
    let h = harness().await;
    let id = h.mock.add_user("ada@example.com", "Old-pass-1", true);
    let probe = h
        .post(
            "/auth/v1/auth-request",
            json!({"authRequest": format!("../../users/{id}")}),
            None,
            "1.1.1.1",
        )
        .await;
    assert_eq!(
        (probe.status, probe.body),
        (StatusCode::GONE, err("sign_in_expired", messages::GONE))
    );
    let sign_in = h
        .post(
            "/auth/v1/sign-in",
            signin("V2_1/../../users", "ada@example.com", "Old-pass-1"),
            None,
            "1.1.1.1",
        )
        .await;
    assert_eq!(sign_in.status, StatusCode::GONE);
    let confirm = h.post("/auth/v1/password-reset/confirm", json!({"userId": format!("{id}/lock?x="), "code": RESET_CODE, "password": "New-pass-12"}), None, "1.1.1.1").await;
    assert_eq!(
        (confirm.status, confirm.body),
        (
            StatusCode::BAD_REQUEST,
            err("invalid_code", messages::BAD_CODE)
        )
    );
    assert_eq!(
        h.mock.user("ada@example.com").unwrap().password,
        "Old-pass-1"
    );
}

// I1: a cookieless resend must not share a rate-limit bucket with decoy cookies.
#[tokio::test]
async fn resend_limits_do_not_reveal_a_decoy() {
    let h = harness().await;
    h.mock.add_user("ada@example.com", "Secret-pass1", true);
    for _ in 0..5 {
        h.post("/auth/v1/resend-code", json!({}), None, "4.4.4.4")
            .await;
    }
    let taken = h
        .post(
            "/auth/v1/sign-up",
            signup(
                &h.mock.add_auth_request(true),
                "ada@example.com",
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
    h.mock.add_user("ada@example.com", "Secret-pass1", true);
    let taken = h
        .post(
            "/auth/v1/sign-up",
            signup(
                &h.mock.add_auth_request(true),
                "ada@example.com",
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
            signin(
                &h.mock.add_auth_request(false),
                &format!("x{i}@example.com"),
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
                signin(&h.mock.add_auth_request(false), "z@example.com", "x"),
                None,
                "6.6.6.6",
            )
            .await;
        assert_eq!(a.status, StatusCode::TOO_MANY_REQUESTS);
    }
    let a = h
        .post(
            "/auth/v1/sign-in",
            signin(&h.mock.add_auth_request(false), "z@example.com", "x"),
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
            signin("V2_1", &email, "x"),
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
    h.mock.allow_duplicate_emails();
    h.mock.add_user("ada@example.com", "Secret-pass1", true);
    let a = h
        .post(
            "/auth/v1/sign-up",
            signup(
                &h.mock.add_auth_request(true),
                "Ada@Example.com",
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
    assert_eq!(h.mock.users_with_email("ada@example.com"), 1);
}

// N2: two accounts already sharing an email (made outside these checks) still mean "taken".
#[tokio::test]
async fn sign_up_never_adds_to_an_ambiguous_email() {
    let h = harness().await;
    h.mock.allow_duplicate_emails();
    h.mock.add_user("ada@example.com", "Secret-pass1", true);
    h.mock.add_user("ADA@example.com", "Secret-pass1", true);
    let a = h
        .post(
            "/auth/v1/sign-up",
            signup(
                &h.mock.add_auth_request(true),
                "ada@example.com",
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
    assert_eq!(h.mock.users_with_email("ada@example.com"), 2);
}

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
            signup(
                &h.mock.add_auth_request(true),
                "new@example.com",
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
    h.mock.add_user("ada@example.com", "Secret-pass1", true);
    let a = h
        .post(
            "/auth/v1/sign-up",
            signup(
                &h.mock.add_auth_request(true),
                "Ada@Example.com",
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
    assert_eq!(
        h.mock.session_count(),
        0,
        "no sign-in from the sign-up form"
    );
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

#[tokio::test]
async fn auth_request_is_rate_limited_per_ip() {
    let h = harness().await;
    for _ in 0..60 {
        h.post(
            "/auth/v1/auth-request",
            json!({"authRequest": "V2_1"}),
            None,
            "2.3.4.5",
        )
        .await;
    }
    let a = h
        .post(
            "/auth/v1/auth-request",
            json!({"authRequest": "V2_1"}),
            None,
            "2.3.4.5",
        )
        .await;
    assert_eq!(a.status, StatusCode::TOO_MANY_REQUESTS);
}

// Successful sign-ins don't use up the per-email limit, so nobody can lock a known account
// out by signing in as it... they'd need its password.
#[tokio::test]
async fn only_failed_sign_ins_count_against_an_email() {
    let h = harness().await;
    h.mock.add_user("ada@example.com", "Secret-pass1", true);
    for i in 0..7 {
        let a = h
            .post(
                "/auth/v1/sign-in",
                signin(
                    &h.mock.add_auth_request(false),
                    "ada@example.com",
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
        signup(
            &h.mock.add_auth_request(true),
            "new@example.com",
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
    let id = h.mock.add_user("ada@example.com", "Secret-pass1", true);
    h.post(
        "/auth/v1/sign-up",
        signup(
            &h.mock.add_auth_request(true),
            "ada@example.com",
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

const SIGN_UP_CALL: &str = "GET /internal/signup/allowed";

#[tokio::test]
async fn sign_up_refused_when_closed() {
    let h = harness().await;
    h.internal.set_signup(false, "closed");
    let a = h
        .post(
            "/auth/v1/sign-up",
            signup(&h.mock.add_auth_request(true), "new@example.com", "Secret-pass1"),
            None,
            "1.1.1.1",
        )
        .await;
    assert_eq!(a.status, StatusCode::FORBIDDEN);
    assert_eq!(a.body["error"]["code"], "sign_up_closed");
    assert_eq!(h.mock.users_with_email("new@example.com"), 0);
    let asked = h.internal.queries(SIGN_UP_CALL);
    assert_eq!(asked, vec!["org=o1".to_string()]);
}

#[tokio::test]
async fn sign_up_allowed_when_open() {
    let h = harness().await;
    h.internal.set_signup(true, "open");
    let a = h
        .post(
            "/auth/v1/sign-up",
            signup(&h.mock.add_auth_request(true), "new@example.com", "Secret-pass1"),
            None,
            "1.1.1.1",
        )
        .await;
    assert_eq!((a.status, a.body), (StatusCode::OK, json!({"next": "verify"})));
    assert_eq!(h.mock.users_with_email("new@example.com"), 1);
}

#[tokio::test]
async fn first_member_may_sign_up() {
    let h = harness().await;
    h.internal.set_signup(true, "first_member");
    let a = h
        .post(
            "/auth/v1/sign-up",
            signup(&h.mock.add_auth_request(true), "first@example.com", "Secret-pass1"),
            None,
            "1.1.1.1",
        )
        .await;
    assert_eq!(a.status, StatusCode::OK);
    assert_eq!(h.mock.users_with_email("first@example.com"), 1);
}

#[tokio::test]
async fn sign_up_fails_closed_when_the_gate_is_unreachable() {
    let h = harness().await;
    h.internal.set_signup_status(Some(503));
    let a = h
        .post(
            "/auth/v1/sign-up",
            signup(&h.mock.add_auth_request(true), "new@example.com", "Secret-pass1"),
            None,
            "1.1.1.1",
        )
        .await;
    assert_eq!(a.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(h.mock.users_with_email("new@example.com"), 0);
}

#[tokio::test]
async fn platform_sign_up_refused() {
    let h = harness_mode(false, 1, auth::config::Mode::Platform).await;
    let a = h
        .post(
            "/auth/v1/sign-up",
            signup(&h.mock.add_auth_request(true), "new@example.com", "Secret-pass1"),
            None,
            "1.1.1.1",
        )
        .await;
    assert_eq!(a.status, StatusCode::FORBIDDEN);
    assert_eq!(a.body["error"]["code"], "not_available_in_platform_mode");
    assert_eq!(h.mock.users_with_email("new@example.com"), 0);
    assert!(h.internal.queries(SIGN_UP_CALL).is_empty());
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
async fn verify_errors_map_by_zitadel_error_key() {
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
    // A 412 with the expired key (a localized message) is an expired invite; the user-not-found
    // key is a revoked one.
    h.mock.fail("/invite_code/verify", 412, "Errors.User.Code.Expired", 1);
    let b = h
        .post(
            "/auth/v1/accept-invite",
            accept(&h.mock.add_auth_request(false), &id, &code, "Secret-pass1"),
            None,
            "1.1.1.1",
        )
        .await;
    assert_eq!(b.body["error"]["code"], "invite_expired");
    h.mock.fail("/invite_code/verify", 404, "Errors.User.NotFound", 1);
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
    let body = |h: &Harness| signup(&h.mock.add_auth_request(true), "new@example.com", "Secret-pass1");
    // An odd reason with allowed:true is still allowed.
    h.internal.set_signup(true, "something_new");
    assert_eq!(
        h.post("/auth/v1/sign-up", body(&h), None, "1.1.1.1").await.status,
        StatusCode::OK
    );
    // The orchestrator saying "platform" is the platform error.
    h.internal.set_signup(false, "platform");
    let a = h.post("/auth/v1/sign-up", body(&h), None, "1.1.1.2").await;
    assert_eq!(a.body["error"]["code"], "not_available_in_platform_mode");
    // A 4xx from the gate (a wrong token) is unavailability, not an open door.
    h.internal.set_signup_status(Some(401));
    let b = h.post("/auth/v1/sign-up", body(&h), None, "1.1.1.3").await;
    assert_eq!(b.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(h.mock.users_with_email("new@example.com"), 1);
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
            signup(&h.mock.add_auth_request(true), "new@example.com", "Secret-pass1"),
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
