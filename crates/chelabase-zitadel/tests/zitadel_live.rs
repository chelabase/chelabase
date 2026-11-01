//! The client against the stack's real Zitadel: what "no such user" looks like (#57), and the
//! refusals `chelabase_zitadel::refusals` records (#60), each asserted against its constant.
//!
//! Runs in the workbench runner, which has `demo.env` (the issuer and the admin PAT):
//!
//! ```text
//! just workbench test -p chelabase-zitadel --test zitadel_live -- --run-ignored all
//! ```
//!
//! Calls go to `CHELABASE_ZITADEL_INTERNAL_URL` (else the `zitadel` service) with the issuer's
//! host as `Host`, as the apps send them.

// Test code: plain clients against the stack (clippy.toml allows tests only).
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use chelabase_zitadel::client::{NewUser, UserState, ZError, Zitadel, classify};
use chelabase_zitadel::refusals::{self, Refusal};
use reqwest::Method;
use serde_json::{Value, json};
use std::future::Future;
use std::path::PathBuf;

#[derive(Clone)]
struct Live {
    base: String,
    host: String,
    pat: String,
}

fn live() -> Live {
    let issuer = std::env::var("CHELABASE_ZITADEL_ISSUER")
        .expect("CHELABASE_ZITADEL_ISSUER is unset (source demo.env)");
    let host = issuer
        .split_once("://")
        .map(|(_, rest)| rest.trim_end_matches('/').to_string())
        .expect("the issuer has a host");
    let base = std::env::var("CHELABASE_ZITADEL_INTERNAL_URL")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "http://zitadel:8080".into());
    let file = PathBuf::from(
        std::env::var("CHELABASE_ZITADEL_ADMIN_PAT_FILE")
            .expect("CHELABASE_ZITADEL_ADMIN_PAT_FILE is unset (source demo.env)"),
    );
    let file = if file.is_relative() {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join(file)
    } else {
        file
    };
    let pat = std::fs::read_to_string(file).unwrap().trim().to_string();
    Live {
        base: base.trim_end_matches('/').to_string(),
        host,
        pat,
    }
}

impl Live {
    fn client(&self) -> Zitadel {
        Zitadel::new(&self.base, &self.pat).with_host(Some(&self.host))
    }

    /// The raw answer to `GET /v2/users/{id}`, to show what Zitadel's not-found looks like.
    async fn raw_get_user(&self, id: &str) -> (u16, String) {
        self.raw(Method::GET, &format!("/v2/users/{id}"), None).await
    }

    /// The raw answer to one call, with the stack's `Host` and the admin PAT.
    async fn raw(&self, method: Method, path: &str, body: Option<Value>) -> (u16, String) {
        self.raw_as(&self.host, &self.pat, method, path, body).await
    }

    async fn raw_as(
        &self,
        host: &str,
        token: &str,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> (u16, String) {
        let mut request = reqwest::Client::new()
            .request(method, format!("{}{path}", self.base))
            .header(reqwest::header::HOST, host)
            .bearer_auth(token);
        if let Some(body) = body {
            request = request.json(&body);
        }
        let resp = request.send().await.unwrap();
        let status = resp.status().as_u16();
        (status, resp.text().await.unwrap())
    }

    /// Asserts that the raw answer to one call is exactly the captured refusal.
    async fn refused(&self, method: Method, path: &str, body: Option<Value>, want: Refusal) {
        let (status, text) = self.raw(method.clone(), path, body).await;
        eprintln!("{method} {path}: {status} {text}");
        let got = classify(status, &text);
        assert!(got.is(want), "{method} {path}: want {want:?}, got {got:?}");
    }
}

/// Asserts that a client call failed with exactly the captured refusal.
fn assert_refused<T: std::fmt::Debug>(what: &str, got: Result<T, ZError>, want: Refusal) {
    eprintln!("{what}: {got:?}");
    match got {
        Err(e) if e.is(want) => {}
        other => panic!("{what}: want {want:?}, got {other:?}"),
    }
}

fn tag() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos()
}

/// Runs `checks` with a fresh org of its own, then deletes the org, even when a check fails.
async fn in_org<F, Fut>(name: &str, checks: F)
where
    F: FnOnce(Live, String) -> Fut,
    Fut: Future<Output = ()> + Send + 'static,
{
    let live = live();
    let z = live.client();
    let org = format!("live-{name}-{}", tag());
    z.ensure_org(&org, &org).await.unwrap();
    let outcome = tokio::spawn(checks(live, org.clone())).await;
    z.delete_org(&org).await.unwrap();
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic.into_panic());
    }
}

const PASSWORD: &str = "Str0ng!pass-live";

/// A user with a password and a verified email in `org`.
async fn new_user(z: &Zitadel, org: &str, name: &str) -> String {
    let email = format!("{name}-{}@example.com", tag());
    z.create_user(&NewUser {
        org_id: org,
        email: &email,
        given_name: "Live",
        family_name: "User",
        password: Some(PASSWORD),
        user_id: None,
        username: Some(&email),
        send_code: false,
        verified: true,
        url_template: "",
    })
    .await
    .unwrap()
}

/// A session for a new user: Zitadel's projections may not show the user to sessions yet
/// (`SESSION_USER_NOT_FOUND`), as the auth service's own retries allow for.
async fn new_session(z: &Zitadel, user: &str) -> chelabase_zitadel::client::Session {
    for _ in 0..20 {
        match z.create_session(user, PASSWORD).await {
            Err(e) if e.is(refusals::SESSION_USER_NOT_FOUND) => {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            other => return other.unwrap(),
        }
    }
    panic!("the new user never became visible to sessions");
}

async fn state(z: &Zitadel, id: &str) -> UserState {
    z.get_user(id).await.unwrap().unwrap().state
}

/// A deleted user and a never-existing id are both "no such user" (`Ok(None)`, gone), so the
/// not-found match in `get_user` matches the answer the real Zitadel gives.
#[tokio::test]
#[ignore = "needs the workbench stack and demo.env (Zitadel admin)"]
async fn a_deleted_or_unknown_user_is_not_found() {
    let live = live();
    let z = live.client();
    let tag = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let org = format!("live-zitadel-{tag}");
    z.ensure_org(&org, &org).await.unwrap();
    let email = format!("gone-{tag}@example.com");
    let created = z
        .create_user(&NewUser {
            org_id: &org,
            email: &email,
            given_name: "Gone",
            family_name: "User",
            password: Some("Str0ng!pass-live"),
            user_id: None,
            username: Some(&email),
            send_code: false,
            verified: true,
            url_template: "",
        })
        .await;
    let checks = live.clone();
    let checked = async move {
        let live = checks;
        let z = live.client();
        let id = created.unwrap();
        assert_eq!(z.get_user(&id).await.unwrap().unwrap().id, id);
        assert!(!z.user_is_gone(&id).await.unwrap());
        z.delete_user(&id).await.unwrap();
        let (status, body) = live.raw_get_user(&id).await;
        eprintln!("deleted user: {status} {body}");
        assert!(classify(status, &body).is(refusals::GET_USER_NOT_FOUND));
        assert_eq!(z.get_user(&id).await.unwrap(), None);
        assert!(z.user_is_gone(&id).await.unwrap());
        let never = format!("{tag}");
        let (status, body) = live.raw_get_user(&never).await;
        eprintln!("never-existing user: {status} {body}");
        assert!(classify(status, &body).is(refusals::GET_USER_NOT_FOUND));
        assert_eq!(z.get_user(&never).await.unwrap(), None);
        assert!(z.user_is_gone(&never).await.unwrap());
    };
    let outcome = tokio::spawn(checked).await;
    z.delete_org(&org).await.unwrap();
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic.into_panic());
    }
}


/// Each object deleted twice: the second delete succeeds. A user deleted before and one that
/// never existed answer the same captured not-found; an org and a session answer 200 for both,
/// so their deletes have no not-found to accept.
#[tokio::test]
#[ignore = "needs the workbench stack and demo.env (Zitadel admin)"]
async fn deleting_twice_succeeds_on_the_captured_answers() {
    in_org("deletes", |live, org| async move {
        let z = live.client();
        let never = format!("{}", tag());
        // User.
        let user = new_user(&z, &org, "del").await;
        z.delete_user(&user).await.unwrap();
        z.delete_user(&user).await.unwrap();
        let path = format!("/v2/users/{user}");
        live.refused(Method::DELETE, &path, None, refusals::DELETE_USER_NOT_FOUND).await;
        let path = format!("/v2/users/{never}");
        live.refused(Method::DELETE, &path, None, refusals::DELETE_USER_NOT_FOUND).await;
        z.delete_user(&never).await.unwrap();
        // Session.
        let holder = new_user(&z, &org, "ses").await;
        let session = new_session(&z, &holder).await;
        z.delete_session_admin(&session.id).await.unwrap();
        z.delete_session_admin(&session.id).await.unwrap();
        for id in [session.id.as_str(), never.as_str()] {
            let (status, body) = live.raw(Method::DELETE, &format!("/v2/sessions/{id}"), None).await;
            eprintln!("delete session {id} again: {status} {body}");
            assert_eq!(status, 200, "{body}");
        }
        // Org (another one: `in_org` deletes its own).
        let other = format!("live-delorg-{}", tag());
        z.ensure_org(&other, &other).await.unwrap();
        z.delete_org(&other).await.unwrap();
        z.delete_org(&other).await.unwrap();
        for id in [other, format!("never-{never}")] {
            let (status, body) = live.raw(Method::DELETE, &format!("/v2/organizations/{id}"), None).await;
            eprintln!("delete org {id} again: {status} {body}");
            assert_eq!(status, 200, "{body}");
        }
    })
    .await;
}

/// A wrong `Host` gets Zitadel's "instance not found": gRPC NOT_FOUND but no error detail. It is
/// never "gone": every delete, lookup and settle is `Unavailable`.
#[tokio::test]
#[ignore = "needs the workbench stack and demo.env (Zitadel admin)"]
async fn a_wrong_host_is_never_gone() {
    in_org("host", |live, org| async move {
        let z = live.client();
        let user = new_user(&z, &org, "host").await;
        let wrong = Zitadel::new(&live.base, &live.pat).with_host(Some("wrong.example.com"));
        let (status, body) = live
            .raw_as("wrong.example.com", &live.pat, Method::DELETE, &format!("/v2/users/{user}"), None)
            .await;
        eprintln!("wrong host: {status} {body}");
        assert_eq!(status, 404);
        assert!(matches!(classify(status, &body), ZError::Unavailable(_)), "{body}");
        let unavailable = |what: &str, got: Result<(), ZError>| {
            assert!(matches!(got, Err(ZError::Unavailable(_))), "{what}: {got:?}");
        };
        unavailable("delete user", wrong.delete_user(&user).await);
        unavailable("delete org", wrong.delete_org(&org).await);
        unavailable("delete session", wrong.delete_session_admin("1").await);
        unavailable("get user", wrong.get_user(&user).await.map(|_| ()));
        unavailable("lock", wrong.lock_user(&user).await);
        unavailable("unlock", wrong.unlock_user(&user).await);
        unavailable("deactivate", wrong.deactivate_user_settled(&user).await);
        unavailable("reactivate", wrong.reactivate_user_settled(&user).await);
        assert_eq!(state(&z, &user).await, UserState::Active, "the user is untouched");
    })
    .await;
}

/// Lock, unlock, deactivate and reactivate: the captured precondition refusals, and the
/// settled helpers on them. Locking a deactivated user gets the same refusal as locking a locked
/// one, and is never settled as locked.
#[tokio::test]
#[ignore = "needs the workbench stack and demo.env (Zitadel admin)"]
async fn state_changes_answer_the_captured_refusals() {
    in_org("states", |live, org| async move {
        let z = live.client();
        let user = new_user(&z, &org, "state").await;
        let path = |verb: &str| format!("/v2/users/{user}/{verb}");
        let empty = || Some(json!({}));
        // Active.
        live.refused(Method::POST, &path("unlock"), empty(), refusals::UNLOCK_NOT_LOCKED).await;
        assert_refused("reactivate active", z.reactivate_user(&user).await, refusals::REACTIVATE_NOT_INACTIVE);
        z.unlock_user(&user).await.unwrap();
        z.reactivate_user_settled(&user).await.unwrap();
        // Locked.
        z.lock_user(&user).await.unwrap();
        live.refused(Method::POST, &path("lock"), empty(), refusals::LOCK_NOT_ACTIVE).await;
        z.lock_user(&user).await.unwrap();
        assert_eq!(state(&z, &user).await, UserState::Locked);
        z.unlock_user(&user).await.unwrap();
        assert_eq!(state(&z, &user).await, UserState::Active);
        // Inactive.
        z.deactivate_user_settled(&user).await.unwrap();
        assert_refused("deactivate inactive", z.deactivate_user(&user).await, refusals::DEACTIVATE_ALREADY_INACTIVE);
        z.deactivate_user_settled(&user).await.unwrap();
        live.refused(Method::POST, &path("lock"), empty(), refusals::LOCK_NOT_ACTIVE).await;
        assert_refused("lock inactive", z.lock_user(&user).await, refusals::LOCK_NOT_ACTIVE);
        assert_eq!(state(&z, &user).await, UserState::Inactive, "never reported as locked");
        live.refused(Method::POST, &path("unlock"), empty(), refusals::UNLOCK_NOT_LOCKED).await;
        z.unlock_user(&user).await.unwrap();
        z.reactivate_user_settled(&user).await.unwrap();
        assert_eq!(state(&z, &user).await, UserState::Active);
        // Deactivating a locked user succeeds: it is then inactive.
        z.lock_user(&user).await.unwrap();
        z.deactivate_user_settled(&user).await.unwrap();
        assert_eq!(state(&z, &user).await, UserState::Inactive);
        z.reactivate_user_settled(&user).await.unwrap();
        // Gone: each endpoint's own not-found, settled.
        z.delete_user(&user).await.unwrap();
        for (verb, want) in [
            ("lock", refusals::LOCK_USER_NOT_FOUND),
            ("unlock", refusals::UNLOCK_USER_NOT_FOUND),
            ("deactivate", refusals::DEACTIVATE_USER_NOT_FOUND),
            ("reactivate", refusals::REACTIVATE_USER_NOT_FOUND),
        ] {
            live.refused(Method::POST, &path(verb), empty(), want).await;
        }
        z.lock_user(&user).await.unwrap();
        z.unlock_user(&user).await.unwrap();
        z.deactivate_user_settled(&user).await.unwrap();
        z.reactivate_user_settled(&user).await.unwrap();
    })
    .await;
}

/// The refusals the auth service (m2) and the orchestrator's app-user admin (m4) map: invite
/// codes, password policy, sign-in, email, existing users and orgs.
#[tokio::test]
#[ignore = "needs the workbench stack and demo.env (Zitadel admin)"]
async fn the_refusals_callers_map_are_the_captured_ones() {
    in_org("refusals", |live, org| async move {
        let z = live.client();
        let user = new_user(&z, &org, "ref").await;
        let email = z.get_user(&user).await.unwrap().unwrap().email;
        // Invite codes.
        let invitee = z.create_user_without_password(&org, &format!("inv-{}@example.com", tag())).await.unwrap();
        let code = z.return_invite_code(&invitee).await.unwrap();
        assert_refused("wrong invite code", z.verify_invite_code(&invitee, "WRONG1").await, refusals::INVITE_CODE_INVALID);
        assert_refused("no pending invite code", z.verify_invite_code(&user, "WRONG1").await, refusals::INVITE_CODE_INVALID);
        assert_refused("invite for a user who can sign in", z.return_invite_code(&user).await, refusals::INVITE_USER_INITIALIZED);
        z.delete_user(&invitee).await.unwrap();
        assert_refused("invite of a deleted user", z.verify_invite_code(&invitee, &code).await, refusals::INVITE_USER_NOT_FOUND);
        assert_refused("invite of an unknown user", z.verify_invite_code(&format!("{}", tag()), "WRONG1").await, refusals::INVITE_USER_NOT_FOUND);
        // Password policy (the instance's default).
        for (password, want) in [
            ("Sh0rt!", refusals::PASSWORD_TOO_SHORT),
            ("abcdefgh1!", refusals::PASSWORD_NO_UPPER),
            ("ABCDEFGH1!", refusals::PASSWORD_NO_LOWER),
            ("Abcdefgh!!", refusals::PASSWORD_NO_NUMBER),
            ("Abcdefgh12", refusals::PASSWORD_NO_SYMBOL),
        ] {
            assert_refused(password, z.set_password_admin(&user, password).await, want);
        }
        let weak = format!("weak-{}@example.com", tag());
        let created = z
            .create_user(&NewUser {
                org_id: &org,
                email: &weak,
                given_name: "Weak",
                family_name: "User",
                password: Some("Sh0rt!"),
                user_id: None,
                username: Some(&weak),
                send_code: false,
                verified: true,
                url_template: "",
            })
            .await;
        assert_refused("new user, short password", created, refusals::CREATE_USER_PASSWORD_TOO_SHORT);
        assert_refused("wrong current password", z.change_password(&user, "wrong", "An0ther!pass").await, refusals::PASSWORD_INVALID);
        assert_refused("wrong reset code", z.set_password(&user, "NOPE12", "An0ther!pass").await, refusals::PASSWORD_CODE_NOT_FOUND);
        assert_refused("wrong email code", z.verify_email(&user, "NOPE12").await, refusals::EMAIL_CODE_INVALID);
        // App-user admin: unchanged email, existing user, missing org.
        assert_refused("same email", z.set_email(&user, &email, true, None).await, refusals::EMAIL_NOT_CHANGED);
        let again = z
            .create_user(&NewUser {
                org_id: &org,
                email: &email,
                given_name: "Again",
                family_name: "User",
                password: None,
                user_id: None,
                username: Some(&email),
                send_code: false,
                verified: true,
                url_template: "",
            })
            .await;
        assert_refused("existing username", again, refusals::USER_ALREADY_EXISTS);
        assert_refused("org missing", z.create_user_without_password(&format!("missing-{}", tag()), "x@example.com").await, refusals::CREATE_USER_ORG_NOT_FOUND);
        live.refused(Method::POST, "/v2/organizations", Some(json!({"name": org, "organizationId": org})), refusals::ORG_ALREADY_EXISTS).await;
        // Sign-in (once the user is visible to sessions).
        new_session(&z, &user).await;
        assert_refused("wrong password", z.create_session(&user, "wrong").await, refusals::PASSWORD_INVALID);
        assert_refused("unknown user", z.create_session(&format!("{}", tag()), PASSWORD).await, refusals::SESSION_USER_NOT_FOUND);
        assert_refused("unknown auth request", z.auth_request("V2_0").await, refusals::AUTH_REQUEST_NOT_FOUND);
        let bogus = Zitadel::new(&live.base, "not-a-token").with_host(Some(&live.host));
        assert_refused("a token Zitadel doesn't accept", bogus.get_user(&user).await, refusals::TOKEN_INVALID);
        z.lock_user(&user).await.unwrap();
        let locked = z.create_session(&user, PASSWORD).await;
        eprintln!("locked user: {locked:?}");
        assert!(
            locked.as_ref().is_err_and(|e| e.is_any(&[refusals::SESSION_USER_NOT_ACTIVE, refusals::SESSION_USER_LOCKED])),
            "locked user: {locked:?}"
        );
    })
    .await;
}

/// The invite code generator, set for the expired-code case and restored.
const INVITE_GENERATOR: &str = "/admin/v1/secretgenerators/SECRET_GENERATOR_TYPE_INVITE_CODE";

/// An expired invite code is refused exactly as a wrong one (`INVITE_CODE_INVALID`).
///
/// Zitadel has no per-org code expiry, so this shortens the instance's invite code expiry for
/// the one call that makes the code (a code keeps the expiry it was made with), and restores
/// it before anything is asserted. Other tests that make an invite code in that window would
/// get a short-lived one, so this binary runs in the `stack-serial` group
/// (`.config/nextest.toml`).
#[tokio::test]
#[ignore = "needs the workbench stack and demo.env (Zitadel admin)"]
async fn an_expired_invite_code_is_refused_as_invalid() {
    in_org("expiry", |live, org| async move {
        let z = live.client();
        let (status, text) = live.raw(Method::GET, INVITE_GENERATOR, None).await;
        assert_eq!(status, 200, "{text}");
        let before: Value = serde_json::from_str(&text).unwrap();
        let generator = &before["secretGenerator"];
        let setting = |expiry: &Value| {
            let mut body = json!({"expiry": expiry});
            for key in ["length", "includeLowerLetters", "includeUpperLetters", "includeDigits", "includeSymbols"] {
                body[key] = generator.get(key).cloned().unwrap_or(json!(false));
            }
            body
        };
        let restore = setting(&generator["expiry"]);
        let invitee = z.create_user_without_password(&org, &format!("exp-{}@example.com", tag())).await.unwrap();
        let (status, text) = live.raw(Method::PUT, INVITE_GENERATOR, Some(setting(&json!("0.001s")))).await;
        let code = z.return_invite_code(&invitee).await;
        let (restored, restored_text) = live.raw(Method::PUT, INVITE_GENERATOR, Some(restore)).await;
        assert_eq!(restored, 200, "restoring the invite code expiry: {restored_text}");
        let (_, after) = live.raw(Method::GET, INVITE_GENERATOR, None).await;
        let after: Value = serde_json::from_str(&after).unwrap();
        assert_eq!(after["secretGenerator"]["expiry"], generator["expiry"], "the expiry is restored");
        assert_eq!(status, 200, "shortening the invite code expiry: {text}");
        let code = code.unwrap();
        assert_refused("expired invite code", z.verify_invite_code(&invitee, &code).await, refusals::INVITE_CODE_INVALID);
        // The same steps with the instance's expiry: the right code verifies.
        let fresh = z.create_user_without_password(&org, &format!("fresh-{}@example.com", tag())).await.unwrap();
        let code = z.return_invite_code(&fresh).await.unwrap();
        z.verify_invite_code(&fresh, &code).await.unwrap();
    })
    .await;
}
