//! m1 and m2 scenarios against `FakeIdentity`, plus the helpers callers port tests with.

mod common;

/// A fake, its client, and `n` fresh app pools that exist.
async fn setup(n: usize) -> (FakeIdentity, chelabase_identity::Identity, Vec<Pool>) {
    let f = FakeIdentity::start().await;
    let pools: Vec<Pool> = (0..n).map(|_| app_pool()).collect();
    for p in &pools {
        f.with_pool(p);
    }
    let id = f.client();
    (f, id, pools)
}

use chelabase_identity::mock::FakeIdentity;
use chelabase_identity::{CodeKind, CodeRequest, Delivery, IdentityError, Missing, Pool};
use common::*;
use std::time::Duration;

#[tokio::test]
async fn same_email_two_pools() {
    let (_f, id, p) = setup(2).await;
    common::same_email_two_pools(&id, &p[0], &p[1]).await;
}

#[tokio::test]
async fn ban_blocks_sign_in_and_kills_sessions() {
    let (_f, id, p) = setup(1).await;
    common::ban_blocks_sign_in_and_kills_sessions(&id, &p[0]).await;
}

#[tokio::test]
async fn revoke_all() {
    let (_f, id, p) = setup(1).await;
    common::revoke_all(&id, &p[0]).await;
}

#[tokio::test]
async fn drop_pool_isolated() {
    let (_f, id, p) = setup(2).await;
    common::drop_pool_isolated(&id, &id, &p[0], &p[1]).await;
}

#[tokio::test]
async fn user_lifecycle() {
    let (f, id, _) = setup(0).await;
    f.with_pool(&Pool::Members);
    common::user_lifecycle(&id, &Pool::Members).await;
}

#[tokio::test]
async fn code_single_use() {
    let (_f, id, p) = setup(1).await;
    common::code_single_use(&id, &p[0]).await;
}

#[tokio::test]
async fn code_lockout() {
    let (_f, id, p) = setup(1).await;
    common::code_lockout(&id, &p[0]).await;
}

#[tokio::test]
async fn code_cross_pool() {
    let (_f, id, p) = setup(2).await;
    common::code_cross_pool(&id, &p[0], &p[1]).await;
}

#[tokio::test]
async fn reset_revokes_sessions() {
    let (_f, id, p) = setup(1).await;
    common::reset_revokes_sessions(&id, &p[0]).await;
}

#[tokio::test]
async fn unknown_pool_and_bad_pool_name() {
    let f = FakeIdentity::start().await;
    let id = f.client();
    assert_eq!(
        id.get_user(&Pool::Members, "u1").await.unwrap_err(),
        IdentityError::NotFound(Missing::Pool)
    );
    // The client's Pool type cannot spell a bad name, so go around it.
    let http = chelabase_http::client_builder().build().unwrap();
    let r = http
        .get(format!("{}/internal/v1/pools/Members/users", f.base))
        .bearer_auth(chelabase_identity::mock::TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
}

#[tokio::test]
async fn pool_names() {
    let u = uuid::Uuid::parse_str("0b1f6f3a-9d0e-4f5e-8a53-1c2d3e4f5a6b").unwrap();
    assert_eq!(Pool::Members.to_string(), "members");
    assert_eq!(
        Pool::App(u).to_string(),
        "app-0b1f6f3a-9d0e-4f5e-8a53-1c2d3e4f5a6b"
    );
}

#[tokio::test]
async fn builder_helpers_port_mechanically() {
    let f = FakeIdentity::start().await;
    let id = f.client();
    let uid = f.with_user(&Pool::Members, "Ada@Example.com", PW);
    assert_eq!(
        id.get_user(&Pool::Members, &uid).await.unwrap().email,
        "ada@example.com"
    );
    let (sid, token) = f.with_session(&Pool::Members, &uid);
    assert_eq!(
        id.session_by_token(&Pool::Members, &token)
            .await
            .unwrap()
            .session
            .id,
        sid
    );
    f.ban(&Pool::Members, &uid);
    assert!(id.session_by_id(&Pool::Members, &sid).await.is_err());
    assert_eq!(
        id.sign_in(&Pool::Members, "ada@example.com", PW)
            .await
            .unwrap_err(),
        IdentityError::Banned
    );
    assert_eq!(
        f.calls().last().unwrap(),
        "POST /internal/v1/pools/members/sessions"
    );
}

#[tokio::test]
async fn return_vs_email_delivery() {
    let f = FakeIdentity::start().await;
    let id = f.client();
    let pool = app_pool();
    let uid = f.with_user(&pool, "m@example.com", PW);
    let returned = id
        .create_code(
            &pool,
            &CodeRequest::new(&uid, CodeKind::Verify, Delivery::Return),
        )
        .await
        .unwrap();
    assert!(returned.code.is_some());
    assert!(f.codes_sent().is_empty(), "return delivery sends nothing");
    let mailed = id
        .create_code(
            &pool,
            &CodeRequest::new(&uid, CodeKind::Verify, Delivery::Email),
        )
        .await
        .unwrap();
    assert_eq!(
        mailed.code, None,
        "the code never appears in an email-delivery response"
    );
    let sent = f.codes_sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(
        (sent[0].email.as_str(), sent[0].kind),
        ("m@example.com", CodeKind::Verify)
    );
    id.verify_code(&pool, &uid, CodeKind::Verify, &sent[0].code, None)
        .await
        .unwrap();
}

#[tokio::test]
async fn no_smtp_409() {
    let f = FakeIdentity::start().await;
    let id = f.client();
    assert!(id.email_status().await.unwrap().configured);
    f.set_email_configured(false);
    assert!(!id.email_status().await.unwrap().configured);
    let pool = app_pool();
    let uid = f.with_user(&pool, "m@example.com", PW);
    assert_eq!(
        id.create_code(
            &pool,
            &CodeRequest::new(&uid, CodeKind::Verify, Delivery::Email)
        )
        .await
        .unwrap_err(),
        IdentityError::EmailNotConfigured
    );
    // `return` still works without SMTP.
    assert!(
        id.create_code(
            &pool,
            &CodeRequest::new(&uid, CodeKind::Verify, Delivery::Return)
        )
        .await
        .unwrap()
        .code
        .is_some()
    );
}

#[tokio::test]
async fn ttl_per_kind() {
    let f = FakeIdentity::start().await;
    let id = f.client();
    let pool = app_pool();
    let uid = f.with_user(&pool, "t@example.com", PW);
    let reset = id
        .create_code(
            &pool,
            &CodeRequest::new(&uid, CodeKind::Reset, Delivery::Return),
        )
        .await
        .unwrap()
        .code
        .unwrap();
    let invite = id
        .create_code(
            &pool,
            &CodeRequest::new(&uid, CodeKind::Invite, Delivery::Return),
        )
        .await
        .unwrap()
        .code
        .unwrap();
    f.advance(Duration::from_secs(16 * 60));
    assert_eq!(
        id.verify_code(&pool, &uid, CodeKind::Reset, &reset, Some("n3w password!"))
            .await
            .unwrap_err(),
        IdentityError::CodeInvalid,
        "a reset code lasts 15 minutes"
    );
    f.advance(Duration::from_secs(6 * 24 * 3600));
    id.verify_code(
        &pool,
        &uid,
        CodeKind::Invite,
        &invite,
        Some("n3w password!"),
    )
    .await
    .unwrap();
    let again = id
        .create_code(
            &pool,
            &CodeRequest::new(&uid, CodeKind::Invite, Delivery::Return),
        )
        .await
        .unwrap()
        .code
        .unwrap();
    f.advance(Duration::from_secs(8 * 24 * 3600));
    assert!(
        id.verify_code(&pool, &uid, CodeKind::Invite, &again, Some("n3w password!"))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn reset_does_not_clobber_invite() {
    let f = FakeIdentity::start().await;
    let id = f.client();
    let pool = app_pool();
    let uid = f.with_user(&pool, "c@example.com", PW);
    let invite = id
        .create_code(
            &pool,
            &CodeRequest::new(&uid, CodeKind::Invite, Delivery::Return),
        )
        .await
        .unwrap()
        .code
        .unwrap();
    id.create_code(
        &pool,
        &CodeRequest::new(&uid, CodeKind::Reset, Delivery::Return),
    )
    .await
    .unwrap();
    id.verify_code(
        &pool,
        &uid,
        CodeKind::Invite,
        &invite,
        Some("n3w password!"),
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn a_code_is_bound_to_its_kind() {
    let f = FakeIdentity::start().await;
    let id = f.client();
    let pool = app_pool();
    let uid = f.with_user(&pool, "k@example.com", PW);
    let verify = id
        .create_code(
            &pool,
            &CodeRequest::new(&uid, CodeKind::Verify, Delivery::Return),
        )
        .await
        .unwrap()
        .code
        .unwrap();
    assert_eq!(
        id.verify_code(&pool, &uid, CodeKind::Reset, &verify, Some("n3w password!"))
            .await
            .unwrap_err(),
        IdentityError::CodeInvalid
    );
    id.verify_code(&pool, &uid, CodeKind::Verify, &verify, None)
        .await
        .unwrap();
}

#[tokio::test]
async fn code_for_unknown_user_is_not_found() {
    let f = FakeIdentity::start().await;
    let pool = app_pool();
    f.with_pool(&pool);
    assert_eq!(
        f.client()
            .create_code(
                &pool,
                &CodeRequest::new("nobody", CodeKind::Verify, Delivery::Return)
            )
            .await
            .unwrap_err(),
        IdentityError::NotFound(Missing::User)
    );
}

#[tokio::test]
async fn code_strictness() {
    let (_f, id, p) = setup(1).await;
    common::code_strictness(&id, &p[0]).await;
}

#[tokio::test]
async fn email_send_failure_is_its_own_refusal() {
    let (f, id, p) = setup(1).await;
    let uid = f.with_user(&p[0], "s@example.com", PW);
    f.fail("/codes", 502, "email_send_failed", 1);
    let e = id
        .create_code(
            &p[0],
            &CodeRequest::new(&uid, CodeKind::Reset, Delivery::Email),
        )
        .await
        .unwrap_err();
    assert!(matches!(e, IdentityError::EmailSendFailed(_)), "{e:?}");
    assert!(!e.is_retryable());
}

#[tokio::test]
async fn a_new_code_resets_the_attempts() {
    let (f, id, p) = setup(1).await;
    let uid = f.with_user(&p[0], "r@example.com", PW);
    let first = common::issue(&id, &p[0], &uid, CodeKind::Reset).await;
    let wrong = if first == "000000" {
        "111111"
    } else {
        "000000"
    };
    for _ in 0..4 {
        id.verify_code(&p[0], &uid, CodeKind::Reset, wrong, Some("n3w password!"))
            .await
            .unwrap_err();
    }
    let second = common::issue(&id, &p[0], &uid, CodeKind::Reset).await;
    let wrong = if second == "000000" {
        "111111"
    } else {
        "000000"
    };
    for _ in 0..4 {
        id.verify_code(&p[0], &uid, CodeKind::Reset, wrong, Some("n3w password!"))
            .await
            .unwrap_err();
    }
    id.verify_code(&p[0], &uid, CodeKind::Reset, &second, Some("n3w password!"))
        .await
        .unwrap();
}
