//! RT1 T7: `auth` frames (R9, S2, S14): `sub` and `org_id` must be the ticket's, `exp ≤ now`
//! is expired (no leeway), and the OIDC verifier's answers (expiry, a bad token, JWKS down).

use std::time::{SystemTime, UNIX_EPOCH};

use chelabase_auth::CLAIM_ORG_ID;
use chelabase_auth::ZitadelAuthConfig;
use chelabase_auth::internal::AuthMethod;
use chelabase_auth::test_support::{BrokenJwks, MockJwks};
use realtime::auth::fake::FakeVerifier;
use realtime::auth::{
    AuthCheck, MemberToken, OidcVerifier, TokenError, TokenVerifier, check_auth_frame,
};
use realtime::ticket::TicketIdentity;
use serde_json::json;

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn ticket() -> TicketIdentity {
    TicketIdentity {
        sub: "ana".into(),
        org_id: "org-1".into(),
        auth: AuthMethod::ManagementJwt,
        email: None,
        name: None,
        data: None,
    }
}

fn token(sub: &str, org: &str, exp: u64) -> MemberToken {
    MemberToken {
        sub: sub.into(),
        org_id: org.into(),
        exp,
        name: None,
        email: None,
    }
}

#[test]
fn a_matching_unexpired_token_is_ok() {
    assert_eq!(
        check_auth_frame(&ticket(), &token("ana", "org-1", 1_000), 999),
        AuthCheck::Ok
    );
}

#[test]
fn exp_at_or_before_now_is_expired() {
    assert_eq!(
        check_auth_frame(&ticket(), &token("ana", "org-1", 1_000), 1_000),
        AuthCheck::Expired,
        "at exp exactly"
    );
    assert_eq!(
        check_auth_frame(&ticket(), &token("ana", "org-1", 1_000), 1_030),
        AuthCheck::Expired,
        "no leeway"
    );
}

#[test]
fn another_sub_or_org_is_a_mismatch() {
    assert_eq!(
        check_auth_frame(&ticket(), &token("bob", "org-1", 2_000), 1_000),
        AuthCheck::Mismatch
    );
    assert_eq!(
        check_auth_frame(&ticket(), &token("ana", "org-2", 2_000), 1_000),
        AuthCheck::Mismatch
    );
    assert_eq!(
        check_auth_frame(&ticket(), &token("bob", "org-1", 500), 1_000),
        AuthCheck::Mismatch,
        "a mismatch wins over expiry"
    );
}

#[tokio::test]
async fn the_fake_verifier_round_trips_its_tokens() {
    let v = FakeVerifier::new();
    let t = token("ana", "org-1", 5);
    let text = FakeVerifier::token(&t);
    assert!(text.starts_with("fake."), "{text}");
    assert_eq!(FakeVerifier::decode(&text), Some(t.clone()));
    assert_eq!(v.verify(&text).await, Ok(t), "whatever its exp");
    assert_eq!(v.verify("garbage").await, Err(TokenError::Invalid));
    v.set_unavailable(true);
    assert_eq!(v.verify(&text).await, Err(TokenError::Unavailable));
}

fn claims(jwks: &MockJwks, exp: i64) -> serde_json::Value {
    json!({"sub": "ana", "iss": jwks.issuer(), "iat": now() as i64 - 600, "exp": exp,
        CLAIM_ORG_ID: "org-1", "name": "Ana", "email": "ana@example.com"})
}

#[tokio::test]
async fn the_oidc_verifier_returns_the_members_exp() {
    let jwks = MockJwks::start().await;
    let v = OidcVerifier::new(jwks.config(vec![]));
    let exp = now() as i64 + 300;
    let got = v.verify(&jwks.sign(claims(&jwks, exp))).await.unwrap();
    assert_eq!(
        got,
        MemberToken {
            sub: "ana".into(),
            org_id: "org-1".into(),
            exp: exp as u64,
            name: Some("Ana".into()),
            email: Some("ana@example.com".into()),
        }
    );
}

#[tokio::test]
async fn a_token_within_the_leeway_verifies_and_the_socket_check_expires_it() {
    // The verifier keeps the gateway's 60 s leeway; S14's "exp ≤ now closes 4001" is realtime's.
    let jwks = MockJwks::start().await;
    let v = OidcVerifier::new(jwks.config(vec![]));
    let exp = now() - 20;
    let got = v
        .verify(&jwks.sign(claims(&jwks, exp as i64)))
        .await
        .unwrap();
    assert_eq!(got.exp, exp);
    assert_eq!(check_auth_frame(&ticket(), &got, now()), AuthCheck::Expired);
}

#[tokio::test]
async fn an_expired_token_is_expired() {
    let jwks = MockJwks::start().await;
    let v = OidcVerifier::new(jwks.config(vec![]));
    let t = jwks.sign(claims(&jwks, now() as i64 - 3_600));
    assert_eq!(v.verify(&t).await, Err(TokenError::Expired));
}

#[tokio::test]
async fn bad_tokens_are_invalid() {
    let jwks = MockJwks::start().await;
    let v = OidcVerifier::new(jwks.config(vec![]));
    assert_eq!(v.verify("not-a-jwt").await, Err(TokenError::Invalid));
    assert_eq!(v.verify("").await, Err(TokenError::Invalid));
    let mut other = claims(&jwks, now() as i64 + 300);
    other["iss"] = json!("http://someone-else");
    assert_eq!(
        v.verify(&jwks.sign(other)).await,
        Err(TokenError::Invalid),
        "another issuer"
    );
    let mut no_org = claims(&jwks, now() as i64 + 300);
    no_org.as_object_mut().unwrap().remove(CLAIM_ORG_ID);
    assert_eq!(
        v.verify(&jwks.sign(no_org)).await,
        Err(TokenError::Invalid),
        "no org"
    );
}

#[tokio::test]
async fn jwks_down_is_unavailable() {
    let broken = BrokenJwks::start().await;
    let jwks = MockJwks::start().await;
    let config = ZitadelAuthConfig {
        issuer: jwks.issuer().into(),
        jwks_url: broken.jwks_url().into(),
        audiences: vec![chelabase_auth::test_support::MOCK_AUDIENCE.to_string()].into(),
    };
    let v = OidcVerifier::new(config);
    assert_eq!(
        v.verify(&jwks.sign(claims(&jwks, now() as i64 + 300)))
            .await,
        Err(TokenError::Unavailable)
    );
}

// #22 AC 3: realtime starts before the bootstrap has written the audiences. Until both files
// are there, an `auth` frame's token is "can't be checked now" (never accepted, whatever its
// audience) and the verifier isn't ready; once they appear, the right audience passes and
// another is invalid, without a restart.
#[tokio::test]
async fn oidc_refuses_until_audiences() {
    use chelabase_auth::Audiences;
    use chelabase_types::file_value::{FileValue, Sensitivity};
    use std::os::unix::fs::PermissionsExt;
    let jwks = MockJwks::start().await;
    let dir = tempfile::tempdir().unwrap();
    let (project, client) = (dir.path().join("project-id"), dir.path().join("api-client-id"));
    let mut config = jwks.config(vec![]);
    config.audiences = Audiences::Ids(vec![
        FileValue::file("CHELABASE_ZITADEL_PROJECT_ID_FILE", &project, Sensitivity::Public),
        FileValue::file("CHELABASE_ZITADEL_API_CLIENT_ID_FILE", &client, Sensitivity::Public),
    ]);
    let v = OidcVerifier::new(config);
    let signed = |aud: &str| {
        let mut c = claims(&jwks, now() as i64 + 300);
        c["aud"] = json!(aud);
        jwks.sign(c)
    };
    assert!(!v.is_ready());
    for aud in ["p1", "anything"] {
        assert_eq!(v.verify(&signed(aud)).await, Err(TokenError::Unavailable), "{aud}");
    }
    for (path, value) in [(&project, "p1\n"), (&client, "c-api\n")] {
        std::fs::write(path, value).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644)).unwrap();
    }
    assert!(v.is_ready());
    assert!(v.verify(&signed("p1")).await.is_ok());
    assert_eq!(v.verify(&signed("someone-else")).await, Err(TokenError::Invalid));
}
