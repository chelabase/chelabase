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
    let uid = f.with_invited_user(&pool, "t@example.com");
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
    // The first invitee is established now; invite another.
    let uid = f.with_invited_user(&pool, "t2@example.com");
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
    let uid = f.with_invited_user(&pool, "c@example.com");
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

#[tokio::test]
async fn email_change_kills_codes() {
    let (_f, id, p) = setup(1).await;
    common::email_change_kills_codes(&id, &p[0]).await;
}

#[tokio::test]
async fn invite_refused_for_established_account() {
    let (_f, id, p) = setup(1).await;
    common::invite_refused_for_established_account(&id, &p[0]).await;
}

#[tokio::test]
async fn invite_works_for_preregistered_account() {
    let (_f, id, p) = setup(1).await;
    common::invite_works_for_preregistered_account(&id, &p[0]).await;
}

/// The fake's ids are UUIDs like the service's, and `search_users` matches part of the email,
/// ignoring case.
#[tokio::test]
async fn ids_are_uuids_and_search_matches_part_of_the_email() {
    let f = FakeIdentity::start().await;
    let id = f.client();
    let pool = Pool::App(uuid::Uuid::new_v4());
    id.create_pool(&pool).await.unwrap();
    let a = id
        .create_user(
            &pool,
            &chelabase_identity::NewUser {
                email: "Find.Me@x.io",
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let b = f.with_user(&pool, "findme+2@x.io", PW);
    f.with_user(&pool, "other@x.io", PW);
    assert!(uuid::Uuid::parse_str(&a.id).is_ok(), "{}", a.id);
    assert!(uuid::Uuid::parse_str(&b).is_ok(), "{b}");
    let (sid, _) = f.with_session(&pool, &b);
    assert!(uuid::Uuid::parse_str(&sid).is_ok(), "{sid}");

    let page = id.search_users(&pool, "FIND", 50, 0).await.unwrap();
    let ids: Vec<_> = page.users.iter().map(|u| u.id.clone()).collect();
    assert_eq!((ids, page.total), (vec![a.id.clone(), b.clone()], 2));
    let page = id.search_users(&pool, "me+2@", 50, 0).await.unwrap();
    assert_eq!(
        page.users.iter().map(|u| u.id.as_str()).collect::<Vec<_>>(),
        [b.as_str()]
    );
    assert_eq!(id.list_users(&pool, 50, 0).await.unwrap().total, 3);
    assert!(
        f.calls()
            .iter()
            .any(|c| c.starts_with("GET ") && c.contains("/users")),
        "{:?}",
        f.calls()
    );
}

#[tokio::test]
async fn invite_notice_sends_no_code() {
    let f = FakeIdentity::start().await;
    let id = f.client();
    let uid = f.with_user(&Pool::Members, "ann@x.io", "password-ann-1");
    let link = "https://studio.test/accept-invite?t=abc";
    id.send_invite_notice(&Pool::Members, &uid, Some("Acme"), link)
        .await
        .unwrap();
    assert_eq!(f.notices_sent().len(), 1);
    assert_eq!(f.notices_sent()[0].user_id, uid);
    assert_eq!(f.notices_sent()[0].link, link);
    assert!(f.codes_sent().is_empty(), "a notice is not a code");

    // As the service: a link with a code, no SMTP, an app pool, an unknown user.
    let coded = id
        .send_invite_notice(&Pool::Members, &uid, None, "https://s.test/a?t=1&code=123456")
        .await;
    assert!(matches!(coded, Err(IdentityError::Invalid { .. })), "{coded:?}");
    let unknown = id
        .send_invite_notice(&Pool::Members, "00000000-0000-4000-8000-000000000000", None, link)
        .await;
    assert_eq!(unknown, Err(IdentityError::NotFound(Missing::User)));
    let app = app_pool();
    f.with_pool(&app);
    let wrong_pool = id.send_invite_notice(&app, &uid, None, link).await;
    assert!(matches!(wrong_pool, Err(IdentityError::Invalid { .. })), "{wrong_pool:?}");
    f.set_email_configured(false);
    assert_eq!(
        id.send_invite_notice(&Pool::Members, &uid, None, link).await,
        Err(IdentityError::EmailNotConfigured)
    );
    assert_eq!(f.notices_sent().len(), 1);
}

#[tokio::test]
async fn user_detail_tells_password_and_ban_reason() {
    let f = FakeIdentity::start().await;
    let id = f.client();
    let with = f.with_user(&Pool::Members, "ann@x.io", "password-ann-1");
    let pre = f.with_unverified_user(&Pool::Members, "pre@x.io", "password-pre-1");
    let without = f.with_invited_user(&Pool::Members, "new@x.io");
    assert!(id.user_detail(&Pool::Members, &with).await.unwrap().has_password);
    let detail = id.user_detail(&Pool::Members, &pre).await.unwrap();
    assert!(detail.has_password && !detail.user.email_verified);
    assert!(!id.user_detail(&Pool::Members, &without).await.unwrap().has_password);

    id.ban(&Pool::Members, &with, Some("ours")).await.unwrap();
    let banned = id.user_detail(&Pool::Members, &with).await.unwrap();
    assert!(banned.user.banned);
    assert_eq!(banned.ban_reason.as_deref(), Some("ours"));
    id.unban(&Pool::Members, &with).await.unwrap();
    assert_eq!(id.user_detail(&Pool::Members, &with).await.unwrap().ban_reason, None);
}

// As the service: members' sessions last 30 days, app users' 90.
#[tokio::test]
async fn session_lifetime_per_pool() {
    let f = FakeIdentity::start().await;
    let id = f.client();
    let app = app_pool();
    let day = |n: u64| Duration::from_secs(n * 24 * 3600);
    f.with_user(&Pool::Members, "m@x.io", "password-m-1");
    let a = f.with_user(&app, "a@x.io", "password-a-1");
    let member = id.sign_in(&Pool::Members, "m@x.io", "password-m-1").await.unwrap();
    let (app_session, _) = f.with_session(&app, &a);
    f.advance(day(29));
    assert!(id.session_by_id(&Pool::Members, &member.session_id).await.is_ok());
    f.advance(day(2));
    assert!(id.session_by_id(&Pool::Members, &member.session_id).await.is_err(), "past 30 days");
    assert!(id.session_by_id(&app, &app_session).await.is_ok());
    f.advance(day(60));
    assert!(id.session_by_id(&app, &app_session).await.is_err(), "past 90 days");
}

/// `find_user_by_email` is one exact lookup: the user with that address (any case), or `None`
/// when only longer addresses contain it. One `GET …/users` call either way.
#[tokio::test]
async fn find_user_by_email_is_one_exact_lookup() {
    let f = FakeIdentity::start().await;
    let id = f.client();
    let pool = Pool::App(uuid::Uuid::new_v4());
    id.create_pool(&pool).await.unwrap();
    let a = f.with_user(&pool, "exact@x.io", PW);
    for i in 0..250 {
        f.with_user(&pool, &format!("u{i}exact@x.io"), PW);
    }
    let list = format!("GET /internal/v1/pools/{pool}/users");
    let lists = || f.calls().iter().filter(|c| c.starts_with(&list)).count();

    let before = lists();
    let got = id.find_user_by_email(&pool, "EXACT@x.io").await.unwrap();
    assert_eq!(got.map(|u| u.id), Some(a));
    assert_eq!(lists() - before, 1);

    let before = lists();
    assert!(id.find_user_by_email(&pool, "xact@x.io").await.unwrap().is_none());
    assert!(id.find_user_by_email(&pool, "%@x.io").await.unwrap().is_none());
    assert_eq!(lists() - before, 2);
}
