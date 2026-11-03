//! RT1 T7: `auth` frames (R9, S2, S14): `sub` and the tenant must be the ticket's, `exp ≤ now`
//! is expired (no leeway), and the member verifier's answers (expiry, a bad token, no key).

use std::time::{SystemTime, UNIX_EPOCH};

use chelabase_auth::internal::AuthMethod;
use chelabase_auth::test_support::MemberKeys;
use realtime::auth::fake::FakeVerifier;
use realtime::auth::{
    AuthCheck, MemberToken, MemberVerifier, TokenError, TokenVerifier, check_auth_frame,
};
use realtime::ticket::TicketIdentity;
use uuid::Uuid;

const T1: Uuid = Uuid::from_u128(1);
const T2: Uuid = Uuid::from_u128(2);

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn ticket() -> TicketIdentity {
    TicketIdentity {
        sub: "ana".into(),
        tenant_id: Some(T1),
        auth: AuthMethod::ManagementJwt,
        email: None,
        name: None,
        data: None,
    }
}

fn token(sub: &str, tenant: Uuid, exp: u64) -> MemberToken {
    MemberToken {
        sub: sub.into(),
        tenant_id: Some(tenant),
        sid: format!("sid-{sub}"),
        exp,
        name: None,
        email: None,
    }
}

#[test]
fn a_matching_unexpired_token_is_ok() {
    assert_eq!(
        check_auth_frame(&ticket(), &token("ana", T1, 1_000), 999),
        AuthCheck::Ok
    );
}

#[test]
fn exp_at_or_before_now_is_expired() {
    assert_eq!(
        check_auth_frame(&ticket(), &token("ana", T1, 1_000), 1_000),
        AuthCheck::Expired,
        "at exp exactly"
    );
    assert_eq!(
        check_auth_frame(&ticket(), &token("ana", T1, 1_000), 1_030),
        AuthCheck::Expired,
        "no leeway"
    );
}

#[test]
fn another_sub_or_tenant_is_a_mismatch() {
    assert_eq!(
        check_auth_frame(&ticket(), &token("bob", T1, 2_000), 1_000),
        AuthCheck::Mismatch
    );
    assert_eq!(
        check_auth_frame(&ticket(), &token("ana", T2, 2_000), 1_000),
        AuthCheck::Mismatch
    );
    assert_eq!(
        check_auth_frame(&ticket(), &token("bob", T1, 500), 1_000),
        AuthCheck::Mismatch,
        "a mismatch wins over expiry"
    );
}

#[tokio::test]
async fn the_fake_verifier_round_trips_its_tokens() {
    let v = FakeVerifier::new();
    let t = token("ana", T1, 5);
    let text = FakeVerifier::token(&t);
    assert!(text.starts_with("fake."), "{text}");
    assert_eq!(FakeVerifier::decode(&text), Some(t.clone()));
    assert_eq!(v.verify(&text).await, Ok(t), "whatever its exp");
    assert_eq!(v.verify("garbage").await, Err(TokenError::Invalid));
    v.set_unavailable(true);
    assert_eq!(v.verify(&text).await, Err(TokenError::Unavailable));
}

#[tokio::test]
async fn the_member_verifier_returns_the_members_exp() {
    let keys = MemberKeys::new();
    let v = MemberVerifier::new(Some(keys.verifier()));
    let mut claims = keys.claims("ana", T1, 300);
    claims.name = "Ana".into();
    claims.email = "ana@example.com".into();
    let got = v.verify(&keys.sign(&claims)).await.unwrap();
    assert_eq!(
        got,
        MemberToken {
            sub: "ana".into(),
            tenant_id: Some(T1),
            sid: claims.sid.clone(),
            exp: claims.exp,
            name: Some("Ana".into()),
            email: Some("ana@example.com".into()),
        }
    );
}

#[tokio::test]
async fn a_token_within_the_leeway_verifies_and_the_socket_check_expires_it() {
    // The verifier keeps the member tokens' 30 s leeway; S14's "exp ≤ now closes 4001" is
    // realtime's.
    let keys = MemberKeys::new();
    let v = MemberVerifier::new(Some(keys.verifier()));
    let mut claims = keys.claims("ana", T1, 300);
    claims.iat = now() - 300;
    claims.exp = now() - 20;
    let got = v.verify(&keys.sign(&claims)).await.unwrap();
    assert_eq!(got.exp, claims.exp);
    assert_eq!(check_auth_frame(&ticket(), &got, now()), AuthCheck::Expired);
}

#[tokio::test]
async fn an_expired_token_is_expired() {
    let keys = MemberKeys::new();
    let v = MemberVerifier::new(Some(keys.verifier()));
    let mut claims = keys.claims("ana", T1, 300);
    claims.iat = now() - 3_900;
    claims.exp = now() - 3_600;
    assert_eq!(v.verify(&keys.sign(&claims)).await, Err(TokenError::Expired));
}

#[tokio::test]
async fn bad_tokens_are_invalid() {
    let keys = MemberKeys::new();
    let v = MemberVerifier::new(Some(keys.verifier()));
    assert_eq!(v.verify("not-a-jwt").await, Err(TokenError::Invalid));
    assert_eq!(v.verify("").await, Err(TokenError::Invalid));
    let mut other = keys.claims("ana", T1, 300);
    other.iss = "http://someone-else".into();
    assert_eq!(
        v.verify(&keys.sign(&other)).await,
        Err(TokenError::Invalid),
        "another issuer"
    );
    assert_eq!(
        v.verify(&keys.sign(&keys.claims("ana", Uuid::nil(), 300))).await,
        Err(TokenError::Invalid),
        "the nil tenant"
    );
    assert_eq!(
        v.verify(&MemberKeys::new().token("ana", T1, 300)).await,
        Err(TokenError::Invalid),
        "another key"
    );
}

// Without the member key every token is refused and the verifier isn't ready.
#[tokio::test]
async fn without_the_key_nothing_verifies_and_it_is_not_ready() {
    let keys = MemberKeys::new();
    let none = MemberVerifier::new(None);
    assert!(!none.is_ready());
    assert_eq!(none.verify(&keys.token("ana", T1, 300)).await, Err(TokenError::Invalid));
    assert!(MemberVerifier::new(Some(keys.verifier())).is_ready());
}
