mod support;

use cheladb_secrets::SecretKey;
use reqwest::StatusCode;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::{Duration, Instant};
use support::*;
use ui_auth::app::{AppState, messages, router};
use ui_auth::config::Config;
use ui_auth::guard::RateLimiter;
use ui_auth::zitadel::Zitadel;

pub struct Harness {
    pub mock: MockZitadel,
    pub base: String,
    pub http: reqwest::Client,
}

pub async fn harness_with(skip: bool, min_ms: u64) -> Harness {
    let mock = MockZitadel::start().await;
    let config = Config {
        listen: String::new(),
        issuer: mock.base.clone(),
        token: TOKEN.into(),
        cookie_key: SecretKey::generate(),
        org_id: "o1".into(),
        studio_url: "http://localhost:3000".into(),
        skip_email_verification: skip,
        secure_cookies: false,
        min_response: Duration::from_millis(min_ms),
        mode: ui_auth::config::Mode::SelfHosted,
    };
    let state = Arc::new(AppState {
        zitadel: Zitadel::new(&mock.base, TOKEN),
        config,
        limiter: RateLimiter::default(),
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router(state)).await.unwrap() });
    Harness {
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
        (StatusCode::OK, json!({"signUp": true}))
    );
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
        (StatusCode::GONE, json!({"error": messages::GONE}))
    );
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
            .starts_with("http://localhost:3000/callback?code=")
    );
    assert!(a.cookie("cheladb_session").is_some());
    assert!(a.cookies.iter().any(|c| c.starts_with("cheladb_session=")
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
            .all(|b| *b == format!(r#"{{"error":"{}"}}"#, messages::BAD_CREDENTIALS)),
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
    let login = a.cookie("cheladb_login").unwrap();
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
            .any(|c| c.starts_with("cheladb_login=;") && c.contains("Max-Age=0"))
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
    let login = a.cookie("cheladb_login").unwrap();
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
            json!({"error": messages::BAD_CODE})
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
    let decoy = taken.cookie("cheladb_login").unwrap();
    assert_eq!(decoy.len(), fresh.cookie("cheladb_login").unwrap().len());
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
            json!({"error": messages::BAD_CODE})
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
            json!({"error": messages::WEAK_PASSWORD})
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
            json!({"error": messages::TOO_MANY})
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
    let login = a.cookie("cheladb_login").unwrap();
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
        (StatusCode::GONE, json!({"error": messages::GONE}))
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
            json!({"error": messages::UNAVAILABLE})
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
            json!({"error": messages::BAD_CODE})
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
            json!({"error": messages::WEAK_PASSWORD})
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
            Some(&a.cookie("cheladb_session").unwrap()),
            "1.1.1.1",
        )
        .await;
    assert_eq!((out.status, out.body), (StatusCode::OK, json!({})));
    assert_eq!(h.mock.session_count(), 0);
    for name in ["cheladb_login=;", "cheladb_session=;"] {
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
        (StatusCode::GONE, json!({"error": messages::GONE}))
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
            json!({"error": messages::BAD_CODE})
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
            Some(&taken.cookie("cheladb_login").unwrap()),
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
    let decoy = taken.cookie("cheladb_login").unwrap();
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
            json!({"error": messages::WEAK_PASSWORD})
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
            json!({"error": messages::WEAK_PASSWORD})
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
            json!({"error": messages::ACCOUNT_EXISTS})
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
        (StatusCode::GONE, json!({"error": messages::GONE}))
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
