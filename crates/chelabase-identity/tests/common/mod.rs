//! The m1 and m2 acceptance scenarios, written against the real client so the same code runs
//! against `FakeIdentity` (tests/fake.rs) and the live service (tests/identity_live.rs).
#![allow(dead_code)]

use chelabase_identity::{CodeKind, CodeRequest, Delivery, Identity, IdentityError, NewUser, Pool};
use uuid::Uuid;

pub const PW: &str = "correct horse 9";

pub fn email(tag: &str) -> String {
    format!("{tag}-{}@example.com", Uuid::new_v4().simple())
}

pub fn app_pool() -> Pool {
    Pool::App(Uuid::new_v4())
}

pub async fn user(id: &Identity, pool: &Pool, email: &str) -> String {
    id.create_user(
        pool,
        &NewUser {
            email,
            password: Some(PW),
            email_verified: true,
            ..Default::default()
        },
    )
    .await
    .unwrap()
    .id
}

/// Scenarios expect their pools to exist already.
///
/// m1 AC 1: the same email lives in two pools, each with its own password.
pub async fn same_email_two_pools(id: &Identity, a: &Pool, b: &Pool) {
    let mail = email("same");
    let ua = user(id, a, &mail).await;
    let ub = id
        .create_user(
            b,
            &NewUser {
                email: &mail,
                password: Some("another pass 7"),
                ..Default::default()
            },
        )
        .await
        .unwrap()
        .id;
    assert_ne!(ua, ub);
    assert_eq!(id.sign_in(a, &mail, PW).await.unwrap().user.id, ua);
    assert_eq!(
        id.sign_in(b, &mail, "another pass 7")
            .await
            .unwrap()
            .user
            .id,
        ub
    );
    // A password from the other pool is refused.
    assert_eq!(
        id.sign_in(a, &mail, "another pass 7").await.unwrap_err(),
        IdentityError::BadCredentials
    );
    // The same email twice in one pool is a conflict.
    let dup = id
        .create_user(
            a,
            &NewUser {
                email: &mail,
                password: Some(PW),
                ..Default::default()
            },
        )
        .await;
    assert!(matches!(dup, Err(IdentityError::Conflict(_))), "{dup:?}");
    // A user of one pool is not in the other.
    assert_eq!(
        id.get_user(b, &ua).await.unwrap_err(),
        IdentityError::NotFound(chelabase_identity::Missing::User)
    );
}

/// m1 AC 3: a ban refuses sign-in and kills the user's sessions.
pub async fn ban_blocks_sign_in_and_kills_sessions(id: &Identity, pool: &Pool) {
    let mail = email("ban");
    let uid = user(id, pool, &mail).await;
    let s = id.sign_in(pool, &mail, PW).await.unwrap();
    assert_eq!(
        id.session_by_token(pool, &s.session_token)
            .await
            .unwrap()
            .user
            .id,
        uid
    );
    let banned = id.ban(pool, &uid, Some("spam")).await.unwrap();
    assert!(banned.banned);
    assert_eq!(
        id.sign_in(pool, &mail, PW).await.unwrap_err(),
        IdentityError::Banned
    );
    assert_eq!(
        id.session_by_token(pool, &s.session_token)
            .await
            .unwrap_err(),
        IdentityError::NotFound(chelabase_identity::Missing::Session)
    );
    assert!(!id.unban(pool, &uid).await.unwrap().banned);
    id.sign_in(pool, &mail, PW).await.unwrap();
}

/// m1 AC 4: revoking all of a user's sessions leaves other users' sessions alone.
pub async fn revoke_all(id: &Identity, pool: &Pool) {
    let (m1, m2) = (email("rev-a"), email("rev-b"));
    let (u1, _u2) = (user(id, pool, &m1).await, user(id, pool, &m2).await);
    let a1 = id.sign_in(pool, &m1, PW).await.unwrap();
    let a2 = id.sign_in(pool, &m1, PW).await.unwrap();
    let other = id.sign_in(pool, &m2, PW).await.unwrap();
    id.revoke_user_sessions(pool, &u1).await.unwrap();
    for t in [&a1, &a2] {
        assert!(id.session_by_token(pool, &t.session_token).await.is_err());
        assert!(id.session_by_id(pool, &t.session_id).await.is_err());
    }
    id.session_by_token(pool, &other.session_token)
        .await
        .unwrap();
    // One session by id.
    id.revoke_session(pool, &other.session_id).await.unwrap();
    assert!(id.revoke_session(pool, &other.session_id).await.is_err());
}

/// m1 AC 5: dropping a pool leaves the others alone; its calls answer pool_not_found.
/// `admin` is the caller allowed to drop pools.
pub async fn drop_pool_isolated(id: &Identity, admin: &Identity, a: &Pool, b: &Pool) {
    let mail = email("drop");
    user(id, a, &mail).await;
    let ub = user(id, b, &mail).await;
    admin.drop_pool(a).await.unwrap();
    assert_eq!(
        id.sign_in(a, &mail, PW).await.unwrap_err(),
        IdentityError::NotFound(chelabase_identity::Missing::Pool)
    );
    assert_eq!(id.get_user(b, &ub).await.unwrap().email, mail);
}

/// User management round trip: list, set email, set password, verify, delete.
pub async fn user_lifecycle(id: &Identity, pool: &Pool) {
    let mail = email("life");
    let u = id
        .create_user(
            pool,
            &NewUser {
                email: &mail,
                password: Some(PW),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(!u.email_verified);
    assert!(
        id.mark_email_verified(pool, &u.id)
            .await
            .unwrap()
            .email_verified
    );
    let page = id.list_users(pool, 50, 0).await.unwrap();
    assert!(page.users.iter().any(|x| x.id == u.id));
    assert_eq!(page.total, page.users.len() as u64);
    let new_mail = email("life2");
    let moved = id.set_email(pool, &u.id, &new_mail, false).await.unwrap();
    assert_eq!(moved.email, new_mail);
    assert!(!moved.email_verified);
    id.set_password(pool, &u.id, "a brand new pw 1")
        .await
        .unwrap();
    assert!(id.sign_in(pool, &new_mail, PW).await.is_err());
    id.sign_in(pool, &new_mail, "a brand new pw 1")
        .await
        .unwrap();
    let weak = id.set_password(pool, &u.id, "short").await.unwrap_err();
    assert!(
        matches!(weak, IdentityError::Invalid { ref code, .. } if code == "weak_password"),
        "{weak:?}"
    );
    id.delete_user(pool, &u.id).await.unwrap();
    assert_eq!(
        id.get_user(pool, &u.id).await.unwrap_err(),
        IdentityError::NotFound(chelabase_identity::Missing::User)
    );
}

/// m2 AC 1 and 5: a verify code works once; `return` delivery hands the code back.
pub async fn code_single_use(id: &Identity, pool: &Pool) {
    let mail = email("code");
    let u = id
        .create_user(
            pool,
            &NewUser {
                email: &mail,
                password: Some(PW),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let issued = id
        .create_code(
            pool,
            &CodeRequest::new(&u.id, CodeKind::Verify, Delivery::Return),
        )
        .await
        .unwrap();
    assert_eq!(
        (issued.kind, issued.delivery),
        (CodeKind::Verify, Delivery::Return)
    );
    let code = issued.code.expect("return delivery gives the code back");
    let verified = id
        .verify_code(pool, &u.id, CodeKind::Verify, &code, None)
        .await
        .unwrap();
    assert!(verified.email_verified);
    assert!(id.get_user(pool, &u.id).await.unwrap().email_verified);
    // Reusing it fails.
    assert_eq!(
        id.verify_code(pool, &u.id, CodeKind::Verify, &code, None)
            .await
            .unwrap_err(),
        IdentityError::CodeInvalid
    );
}

/// m2 AC 2: five wrong codes lock the code; the right one is then refused, like any wrong one.
pub async fn code_lockout(id: &Identity, pool: &Pool) {
    let u = id
        .create_user(
            pool,
            &NewUser {
                email: &email("lock"),
                password: Some(PW),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let code = issue(id, pool, &u.id, CodeKind::Reset).await;
    let wrong = if code == "000000" { "111111" } else { "000000" };
    for _ in 0..5 {
        let e = id
            .verify_code(pool, &u.id, CodeKind::Reset, wrong, Some("n3w password!"))
            .await
            .unwrap_err();
        assert_eq!(e, IdentityError::CodeInvalid);
    }
    assert_eq!(
        id.verify_code(pool, &u.id, CodeKind::Reset, &code, Some("n3w password!"))
            .await
            .unwrap_err(),
        IdentityError::CodeInvalid
    );
    // A new code is required, and works (the counter starts again).
    let fresh = issue(id, pool, &u.id, CodeKind::Reset).await;
    id.verify_code(pool, &u.id, CodeKind::Reset, &fresh, Some("n3w password!"))
        .await
        .unwrap();
}

/// A weak password costs no attempt; a code of another kind never works; verify takes no password.
pub async fn code_strictness(id: &Identity, pool: &Pool) {
    let uid = user(id, pool, &email("strict")).await;
    let code = issue(id, pool, &uid, CodeKind::Reset).await;
    for _ in 0..6 {
        let e = id
            .verify_code(pool, &uid, CodeKind::Reset, &code, Some("short"))
            .await
            .unwrap_err();
        assert!(
            matches!(e, IdentityError::Invalid { ref code, .. } if code == "weak_password"),
            "{e:?}"
        );
    }
    // Reset needs a password; verify refuses one.
    let e = id
        .verify_code(pool, &uid, CodeKind::Reset, &code, None)
        .await
        .unwrap_err();
    assert!(
        matches!(e, IdentityError::Invalid { ref code, .. } if code == "invalid_request"),
        "{e:?}"
    );
    // The reset code is not an invite code or a verify code.
    let e = id
        .verify_code(pool, &uid, CodeKind::Invite, &code, Some("n3w password!"))
        .await
        .unwrap_err();
    assert_eq!(e, IdentityError::CodeInvalid);
    let e = id
        .verify_code(pool, &uid, CodeKind::Verify, &code, None)
        .await
        .unwrap_err();
    assert_eq!(e, IdentityError::CodeInvalid);
    let vcode = issue(id, pool, &uid, CodeKind::Verify).await;
    let e = id
        .verify_code(pool, &uid, CodeKind::Verify, &vcode, Some("n3w password!"))
        .await
        .unwrap_err();
    assert!(
        matches!(e, IdentityError::Invalid { ref code, .. } if code == "invalid_request"),
        "{e:?}"
    );
    // The code survived all of that.
    id.verify_code(pool, &uid, CodeKind::Reset, &code, Some("n3w password!"))
        .await
        .unwrap();
    // A new code replaces the old one.
    let a = issue(id, pool, &uid, CodeKind::Invite).await;
    let b = issue(id, pool, &uid, CodeKind::Invite).await;
    if a != b {
        assert_eq!(
            id.verify_code(pool, &uid, CodeKind::Invite, &a, Some("n3w password!"))
                .await
                .unwrap_err(),
            IdentityError::CodeInvalid
        );
    }
}

/// A code, delivered by `return`.
pub async fn issue(id: &Identity, pool: &Pool, user_id: &str, kind: CodeKind) -> String {
    id.create_code(pool, &CodeRequest::new(user_id, kind, Delivery::Return))
        .await
        .unwrap()
        .code
        .unwrap()
}

/// m2 AC 4: a code made in one pool is refused in another, even for the same email.
pub async fn code_cross_pool(id: &Identity, a: &Pool, b: &Pool) {
    let mail = email("xpool");
    let ua = user(id, a, &mail).await;
    let ub = user(id, b, &mail).await;
    let code = id
        .create_code(a, &CodeRequest::new(&ua, CodeKind::Reset, Delivery::Return))
        .await
        .unwrap()
        .code
        .unwrap();
    let e = id
        .verify_code(b, &ub, CodeKind::Reset, &code, Some("n3w password!"))
        .await
        .unwrap_err();
    assert!(
        matches!(e, IdentityError::CodeInvalid | IdentityError::NotFound(_)),
        "{e:?}"
    );
    // Nor does user B's id work with it in pool A.
    assert!(
        id.verify_code(a, &ub, CodeKind::Reset, &code, Some("n3w password!"))
            .await
            .is_err()
    );
    id.verify_code(a, &ua, CodeKind::Reset, &code, Some("n3w password!"))
        .await
        .unwrap();
}

/// m2 AC 7: finishing a reset revokes the user's sessions and sets the password.
pub async fn reset_revokes_sessions(id: &Identity, pool: &Pool) {
    let mail = email("reset");
    let uid = user(id, pool, &mail).await;
    let s = id.sign_in(pool, &mail, PW).await.unwrap();
    let code = id
        .create_code(
            pool,
            &CodeRequest::new(&uid, CodeKind::Reset, Delivery::Return),
        )
        .await
        .unwrap()
        .code
        .unwrap();
    id.verify_code(pool, &uid, CodeKind::Reset, &code, Some("n3w password!"))
        .await
        .unwrap();
    assert!(id.session_by_token(pool, &s.session_token).await.is_err());
    id.sign_in(pool, &mail, "n3w password!").await.unwrap();
}
