//! #119: Studio sockets with member tokens signed by auth (`auth/member-jwt.key`). The gateway's
//! identity names the tenant (`tenant_id`); the ticket keeps it; the orchestrator's member check
//! gets it; every `auth` frame's member token is verified with the member key
//! and must name the ticket's person and tenant.

mod common;

use std::sync::Arc;

use chelabase_auth::member_token::{
    MEMBER_AUDIENCE, MemberClaims, MemberTokenSigner, MemberTokenVerifier,
};
use common::*;
use realtime::auth::{MemberVerifier, TokenError, TokenVerifier};
use serde_json::json;
use uuid::Uuid;

const ISSUER: &str = "http://auth.test";

struct Keys {
    signer: MemberTokenSigner,
    public: String,
}

fn keys() -> Keys {
    let (private, public) = chelabase_auth::internal::generate_keypair_pem();
    Keys {
        signer: MemberTokenSigner::from_pem(private.as_bytes(), public.as_bytes(), ISSUER).unwrap(),
        public,
    }
}

fn claims(sub: &str, tid: Uuid, exp_in: i64) -> MemberClaims {
    let iat = now() - 1;
    MemberClaims {
        iss: ISSUER.into(),
        aud: MEMBER_AUDIENCE.into(),
        sub: sub.into(),
        tid,
        sid: "sess".into(),
        email: format!("{sub}@example.com"),
        name: format!("Name {sub}"),
        iat,
        exp: (now() as i64 + exp_in) as u64,
    }
}

/// The production verifier: member tokens checked with `public`.
async fn verifier(public: Option<&str>) -> (MemberVerifier, ()) {
    let member = public.map(|p| MemberTokenVerifier::from_pem(p.as_bytes(), ISSUER).unwrap());
    (MemberVerifier::new(member), ())
}

// AC 1 on realtime: a member-token ticket asks the orchestrator with the tenant, the
// socket's `auth` frame with the real member token is accepted, and the member subscribes.
#[tokio::test]
async fn member_token_subscribes() {
    let k = keys();
    let (v, _jwks) = verifier(Some(&k.public)).await;
    let h = Harness::start_verifying(Arc::new(v)).await;
    h.orch.on_authorize(|_| allowed("ana"));
    let tid = Uuid::new_v4();

    let (status, _, body) = h
        .post_ticket(h.rt(), Some(&h.identity("ana", tid)))
        .await;
    assert_eq!(status, 201, "{body}");
    let asked = h.orch.member_calls();
    assert_eq!(asked.len(), 1);
    assert_eq!(asked[0]["sub"], "ana");
    assert_eq!(asked[0]["tenant_id"], json!(tid), "{}", asked[0]);
    assert!(asked[0].get("org_id").is_none(), "{}", asked[0]);

    let ticket = body["ticket"].as_str().unwrap();
    let mut ws = Harness::open(h.rt(), ticket).await;
    let token = k.signer.sign(&claims("ana", tid, 300)).unwrap();
    ws.send(json!({"type": "auth", "token": token})).await;
    let joined = ws.join_table("1", "items", None).await;
    assert_eq!(joined["type"], "joined", "{joined}");
    // The orchestrator verified the same raw member token.
    assert_eq!(h.orch.authorize_calls()[0]["token"], json!(token));
}

// A member token naming another tenant than the ticket's is another person: 4003.
#[tokio::test]
async fn member_token_for_another_tenant_is_a_mismatch() {
    let k = keys();
    let (v, _jwks) = verifier(Some(&k.public)).await;
    let h = Harness::start_verifying(Arc::new(v)).await;
    let tid = Uuid::new_v4();
    let (status, _, body) = h
        .post_ticket(h.rt(), Some(&h.identity("ana", tid)))
        .await;
    assert_eq!(status, 201, "{body}");
    let mut ws = Harness::open(h.rt(), body["ticket"].as_str().unwrap()).await;
    let other = k.signer.sign(&claims("ana", Uuid::new_v4(), 300)).unwrap();
    ws.send(json!({"type": "auth", "token": other})).await;
    assert_eq!(ws.closed().await, 4003);
}

// The verifier: a member token is decided by the member key alone.
#[tokio::test]
async fn the_verifier_decides_member_tokens_with_the_member_key() {
    let k = keys();
    let (v, _jwks) = verifier(Some(&k.public)).await;
    let tid = Uuid::new_v4();
    let ok = v
        .verify(&k.signer.sign(&claims("ana", tid, 300)).unwrap())
        .await
        .unwrap();
    assert_eq!(
        (ok.sub.as_str(), ok.tenant_id),
        ("ana", Some(tid))
    );
    assert_eq!(ok.email.as_deref(), Some("ana@example.com"));

    let expired = k.signer.sign(&claims("ana", tid, -31)).unwrap();
    assert_eq!(v.verify(&expired).await, Err(TokenError::Expired));
    let foreign = keys().signer.sign(&claims("ana", tid, 300)).unwrap();
    assert_eq!(
        v.verify(&foreign).await,
        Err(TokenError::Invalid),
        "another key"
    );

    // Without the member key, every member token is refused.
    let (none, _jwks2) = verifier(None).await;
    let good = k.signer.sign(&claims("ana", tid, 300)).unwrap();
    assert_eq!(none.verify(&good).await, Err(TokenError::Invalid));
}
