//! Auth's refusal mapping against the real stack (#61, #78): the stack's auth, orchestrator and
//! Zitadel v4.19.1, through the gateway as Studio and an app reach them. The unit tests prove
//! the arms against the mock; these prove the real Zitadel's answers reach them.
//!
//! Runs in the workbench runner, which has `demo.env` (the issuer, the admin PAT), the gateway's
//! entry URLs, `CHELABASE_CLI` (`chelabase dev token`: the demo tenant admin) and the metadata
//! database's URL:
//!
//! ```text
//! just workbench test -p auth -- --run-ignored only
//! ```
//!
//! Every test makes its own invitee (or project) and removes it afterwards, pass or fail. They
//! share the runner's address, whose accept-invite failures auth counts (20 an hour): a run
//! spends three. They run one at a time (`stack-serial`).

// Test code: plain clients against the stack (clippy.toml allows tests only).
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::future::Future;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use chelabase_zitadel::client::{UserState, Zitadel};
use reqwest::Method;
use serde_json::{Value, json};
use uuid::Uuid;

const DEMO_TENANT_ID: &str = "00000000-0000-4000-8000-000000000001";

/// Passes Zitadel's default password policy and auth's own checks.
const STRONG: &str = "Live-pass-12!";
/// Passes auth's own checks (a Unicode lowercase letter counts there) but not Zitadel's default
/// policy, which wants an ASCII one: the answer is Zitadel's refusal (`DOMAIN-co3Xw` on a set,
/// `COMMA-co3Xw` on a create), which auth maps by its id.
const ZITADEL_WEAK: &str = "ÉCOLE#2026é";

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

fn need(name: &str) -> String {
    env(name).unwrap_or_else(|| panic!("{name} is unset (run through just workbench test)"))
}

/// The gateway's API entry: the management API, and auth's `/auth/v1/*` as Studio calls it.
fn gateway() -> String {
    need("CHELABASE_ENTRY_API_URL")
        .trim_end_matches('/')
        .to_string()
}

fn workspace_root() -> PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// The `chelabase` CLI: `CHELABASE_CLI` (the runner sets it), else the build next to this test.
fn chelabase_cli() -> PathBuf {
    if let Some(cli) = env("CHELABASE_CLI") {
        return cli.into();
    }
    let exe = std::env::current_exe().unwrap();
    exe.parent().unwrap().parent().unwrap().join("chelabase")
}

/// A fresh access token of the demo tenant's admin.
async fn dev_token() -> String {
    let checkout = env("CHELABASE_CHECKOUT").map_or_else(workspace_root, Into::into);
    let out = tokio::process::Command::new(chelabase_cli())
        .args(["dev", "token"])
        .current_dir(checkout)
        .stdin(Stdio::null())
        .output()
        .await
        .expect("chelabase dev token");
    assert!(
        out.status.success(),
        "chelabase dev token: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

async fn send(req: reqwest::RequestBuilder) -> (u16, Value) {
    let resp = req
        .timeout(Duration::from_secs(60))
        .send()
        .await
        .expect("the gateway answers");
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(Value::Null))
}

/// A management API call as the demo tenant admin: (status, body).
async fn api(method: Method, path: &str, body: Option<Value>) -> (u16, Value) {
    let mut req = reqwest::Client::new()
        .request(method, format!("{}{path}", gateway()))
        .bearer_auth(dev_token().await);
    if let Some(body) = body {
        req = req.json(&body);
    }
    send(req).await
}

/// One of Studio's sign-in pages' calls (`POST /auth/v1/<path>`), with Studio's origin.
async fn ui(path: &str, body: Value) -> (u16, Value) {
    send(
        reqwest::Client::new()
            .post(format!("{}/auth/v1/{path}", gateway()))
            .header("origin", need("CHELABASE_ENTRY_STUDIO_URL"))
            .json(&body),
    )
    .await
}

/// An app-user call (`POST /auth/v1/app/<path>`) with the project's public key, as the SDK sends it.
async fn app(key: &str, path: &str, body: Value) -> (u16, Value) {
    send(
        reqwest::Client::new()
            .post(format!("{}/auth/v1/app/{path}", gateway()))
            .bearer_auth(key)
            .json(&body),
    )
    .await
}

fn code_of(body: &Value) -> &str {
    body["error"]["code"].as_str().unwrap_or("")
}

/// Zitadel with the admin PAT, at `CHELABASE_ZITADEL_INTERNAL_URL` (else the `zitadel` service)
/// with the issuer's host as `Host`, as the apps call it.
struct Admin {
    base: String,
    host: String,
    pat: String,
}

fn admin() -> Admin {
    let issuer = need("CHELABASE_ZITADEL_ISSUER");
    let host = issuer
        .split_once("://")
        .map(|(_, rest)| rest.trim_end_matches('/').to_string())
        .expect("the issuer has a host");
    let base =
        env("CHELABASE_ZITADEL_INTERNAL_URL").unwrap_or_else(|| "http://zitadel:8080".into());
    let file = PathBuf::from(need("CHELABASE_ZITADEL_ADMIN_PAT_FILE"));
    let file = if file.is_relative() {
        workspace_root().join(file)
    } else {
        file
    };
    Admin {
        base: base.trim_end_matches('/').to_string(),
        host,
        pat: std::fs::read_to_string(file).unwrap().trim().to_string(),
    }
}

impl Admin {
    fn client(&self) -> Zitadel {
        Zitadel::new(&self.base, &self.pat).with_host(Some(&self.host))
    }

    /// A password-reset code for the user, returned instead of mailed (anyone can have one
    /// mailed, a deactivated user too).
    async fn reset_code(&self, user: &str) -> String {
        let resp = reqwest::Client::new()
            .post(format!("{}/v2/users/{user}/password_reset", self.base))
            .header(reqwest::header::HOST, &self.host)
            .bearer_auth(&self.pat)
            .json(&json!({"returnCode": {}}))
            .send()
            .await
            .unwrap();
        let status = resp.status().as_u16();
        let body: Value = resp.json().await.unwrap_or(Value::Null);
        assert_eq!(status, 200, "a reset code for {user}: {body}");
        body["verificationCode"]
            .as_str()
            .expect("verificationCode")
            .to_string()
    }

    async fn state(&self, user: &str) -> UserState {
        self.client()
            .get_user(user)
            .await
            .unwrap()
            .expect("the user exists")
            .state
    }
}

/// One statement against the metadata database, as the e2e scripts' `metadata_sql` runs it.
async fn metadata_sql(sql: &str) -> String {
    let out = tokio::process::Command::new("psql")
        .arg(need("CHELABASE_METADATA_DATABASE_URL"))
        .args(["-v", "ON_ERROR_STOP=1", "-qtA", "-c", sql])
        .stdin(Stdio::null())
        .output()
        .await
        .expect("psql");
    assert!(
        out.status.success(),
        "{sql}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A person invited by link into the demo tenant.
#[derive(Clone)]
struct Invited {
    member: String,
    invite: Uuid,
    user: String,
    code: String,
}

fn query_param(url: &str, name: &str) -> String {
    let query = url.split_once('?').expect("the link has a query").1;
    query
        .split('&')
        .find_map(|kv| kv.strip_prefix(&format!("{name}=")))
        .unwrap_or_else(|| panic!("no {name} in {url}"))
        .to_string()
}

/// Invites a fresh person by link, runs `checks`, then removes the member and deletes their
/// Zitadel user, pass or fail.
async fn with_invitee<F, Fut>(name: &str, checks: F)
where
    F: FnOnce(Invited) -> Fut,
    Fut: Future<Output = ()> + Send + 'static,
{
    let email = format!(
        "auth-live-{name}-{}@example.com",
        &Uuid::new_v4().simple().to_string()[..12]
    );
    let (status, body) = api(
        Method::POST,
        "/v1/invites",
        Some(json!({"email": email, "grants": [], "delivery": "link"})),
    )
    .await;
    assert_eq!(status, 201, "invite {email}: {body}");
    let url = body["invite_url"]
        .as_str()
        .unwrap_or_else(|| panic!("no invite_url: {body}"));
    let invited = Invited {
        member: body["member"]["id"].as_str().unwrap().to_string(),
        invite: body["member"]["invite"]["id"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap(),
        user: query_param(url, "user"),
        code: query_param(url, "code"),
    };
    let outcome = tokio::spawn(checks(invited.clone())).await;
    let _ = api(
        Method::DELETE,
        &format!("/v1/members/{}", invited.member),
        None,
    )
    .await;
    let _ = admin().client().delete_user(&invited.user).await;
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic.into_panic());
    }
}

/// `POST /auth/v1/<path>` as Studio's session calls make it, with `cookie`: the status, the body
/// and the `name=value` of `chelabase_session` if it was set.
async fn session_call(path: &str, cookie: Option<&str>, body: Option<Value>) -> (u16, Value, Option<String>) {
    let mut req = reqwest::Client::new()
        .post(format!("{}/auth/v1/{path}", gateway()))
        .header("origin", need("CHELABASE_ENTRY_STUDIO_URL"))
        .header("x-chelabase-studio", "1");
    if let Some(cookie) = cookie {
        req = req.header("cookie", cookie);
    }
    if let Some(body) = body {
        req = req.json(&body);
    }
    let resp = req.send().await.expect("the gateway answers");
    let status = resp.status().as_u16();
    let set = resp
        .headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .filter(|c| c.starts_with("chelabase_session=") && !c.starts_with("chelabase_session=;"))
        .map(|c| c.split(';').next().unwrap().to_string())
        .next();
    let body = resp.json().await.unwrap_or(Value::Null);
    (status, body, set)
}

/// #121 AC 1 against the stack: an open sign-up waits as pending, and once active the member signs
/// in, gets a token the gateway and orchestrator accept, and loses it at logout. Open sign-up is
/// switched on for the demo tenant meanwhile; the row and the user go afterwards, pass or fail.
#[tokio::test]
#[ignore = "needs the workbench stack and the metadata database"]
async fn member_sign_in_live() {
    let email = format!("auth-live-member-{}@example.com", &Uuid::new_v4().simple().to_string()[..12]);
    let was_open = metadata_sql(&format!("SELECT open_signup FROM tenants WHERE id = '{DEMO_TENANT_ID}'")).await;
    metadata_sql(&format!("UPDATE tenants SET open_signup = true WHERE id = '{DEMO_TENANT_ID}'")).await;
    let run = email.clone();
    let outcome = tokio::spawn(async move {
        let email = run;
        let creds = json!({"email": email, "password": STRONG});
        let mut sign_up = creds.clone();
        sign_up["givenName"] = json!("Live");
        sign_up["familyName"] = json!("Member");
        let (status, body, _) = session_call("sign-up", None, Some(sign_up)).await;
        assert_eq!((status, &body), (200, &json!({"next": "pending"})), "sign-up");
        let status_of = |email: String| async move {
            metadata_sql(&format!("SELECT status FROM members WHERE tenant_id = '{DEMO_TENANT_ID}' AND email = '{email}'")).await
        };
        assert_eq!(status_of(email.clone()).await, "pending");
        let (_, body, _) = session_call("sign-in", None, Some(creds.clone())).await;
        assert_eq!(body, json!({"next": "pending"}), "before approval");

        metadata_sql(&format!("UPDATE members SET status = 'active' WHERE tenant_id = '{DEMO_TENANT_ID}' AND email = '{email}'")).await;
        let (status, body, cookie) = session_call("sign-in", None, Some(creds.clone())).await;
        assert_eq!((status, &body), (200, &json!({"next": "studio"})), "sign-in");
        let cookie = cookie.expect("the session cookie");
        let (status, body, _) = session_call("token", Some(&cookie), None).await;
        assert_eq!(status, 200, "token: {body}");
        let token = body["access_token"].as_str().expect("a token").to_string();
        let (status, me) = send(reqwest::Client::new().get(format!("{}/v1/me", gateway())).bearer_auth(&token)).await;
        assert_eq!(status, 200, "/v1/me: {me}");
        assert_eq!(me["tenant_id"], DEMO_TENANT_ID);
        assert_eq!(me["member"]["status"], "active");

        // Suspended: `/token` stops at once, whatever the session.
        metadata_sql(&format!("UPDATE members SET status = 'suspended' WHERE tenant_id = '{DEMO_TENANT_ID}' AND email = '{email}'")).await;
        assert_eq!(session_call("token", Some(&cookie), None).await.0, 401, "suspended");
        metadata_sql(&format!("UPDATE members SET status = 'active' WHERE tenant_id = '{DEMO_TENANT_ID}' AND email = '{email}'")).await;
        assert_eq!(session_call("token", Some(&cookie), None).await.0, 200);

        // Without Studio's header, and after logout with the same bytes.
        let bare = reqwest::Client::new()
            .post(format!("{}/auth/v1/token", gateway()))
            .header("origin", need("CHELABASE_ENTRY_STUDIO_URL"))
            .header("cookie", &cookie);
        assert_eq!(send(bare).await.0, 403);
        assert_eq!(session_call("logout", Some(&cookie), Some(json!({}))).await.0, 200);
        assert_eq!(session_call("token", Some(&cookie), None).await.0, 401, "replayed after logout");
    })
    .await;
    metadata_sql(&format!("UPDATE tenants SET open_signup = {was_open} WHERE id = '{DEMO_TENANT_ID}'")).await;
    let _ = metadata_sql(&format!("DELETE FROM members WHERE tenant_id = '{DEMO_TENANT_ID}' AND email = '{email}'")).await;
    let _ = metadata_sql(&format!(r#"DELETE FROM identity_members."user" WHERE email = '{email}'"#)).await;
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic.into_panic());
    }
}

fn accept(user: &str, code: &str, password: &str) -> Value {
    json!({"user": user, "code": code, "reset": false, "password": password})
}

fn reset_accept(user: &str, code: &str, password: &str) -> Value {
    json!({"user": user, "code": code, "reset": true, "password": password})
}

/// An invitee whose Zitadel user was deleted gets 410 `invite_revoked` (AC 1).
#[tokio::test]
#[ignore = "needs the workbench stack and demo.env (Zitadel admin)"]
async fn accept_invite_for_a_deleted_user_is_revoked() {
    with_invitee("deleted", |inv| async move {
        admin().client().delete_user(&inv.user).await.unwrap();
        let (status, body) = ui("accept-invite", accept(&inv.user, &inv.code, STRONG)).await;
        assert_eq!((status, code_of(&body)), (410, "invite_revoked"), "{body}");
    })
    .await;
}

/// Zitadel answers a wrong and an expired code alike (`COMMAND-Wgn4q`): the stored invite tells
/// them apart. A wrong code on an invite whose stored expiry passed is 410 `invite_expired`; the
/// same code once it is open again is 400 `invalid_request`; the right code still works (AC 2).
#[tokio::test]
#[ignore = "needs the workbench stack and demo.env (Zitadel admin)"]
async fn a_wrong_code_is_expired_only_when_the_stored_invite_expired() {
    with_invitee("expiry", |inv| async move {
        let set_expiry = |interval: &'static str| {
            let invite = inv.invite;
            async move {
                metadata_sql(&format!("UPDATE invites SET expires_at = now() {interval} WHERE id = '{invite}' RETURNING id"))
                    .await
            }
        };
        assert_eq!(set_expiry("- interval '1 minute'").await, inv.invite.to_string());
        let (status, body) = ui("accept-invite", accept(&inv.user, "WRONG1", STRONG)).await;
        assert_eq!((status, code_of(&body)), (410, "invite_expired"), "{body}");
        set_expiry("+ interval '1 day'").await;
        let (status, body) = ui("accept-invite", accept(&inv.user, "WRONG1", STRONG)).await;
        assert_eq!((status, code_of(&body)), (400, "invalid_request"), "{body}");
        let (status, body) = ui("accept-invite", accept(&inv.user, &inv.code, STRONG)).await;
        assert_eq!(status, 200, "the right code: {body}");
    })
    .await;
}

/// A password Zitadel's policy refuses (by `DOMAIN-` id) is `weak_password` in accept-invite:
/// after the code (with a ticket to finish), and in a reset before anything changes (AC 1).
#[tokio::test]
#[ignore = "needs the workbench stack and demo.env (Zitadel admin)"]
async fn zitadel_policy_refusals_are_weak_passwords_in_accept_invite() {
    with_invitee("weak", |inv| async move {
        let (status, body) = ui("accept-invite", accept(&inv.user, &inv.code, ZITADEL_WEAK)).await;
        assert_eq!((status, code_of(&body)), (400, "weak_password"), "{body}");
        let ticket = body["error"]["accept_ticket"]
            .as_str()
            .unwrap_or_else(|| panic!("no ticket: {body}"));
        let (status, body) = ui(
            "accept-invite",
            json!({"user": inv.user, "acceptTicket": ticket, "reset": false, "password": STRONG}),
        )
        .await;
        assert_eq!(status, 200, "finishing with the ticket: {body}");
        // A reset code: the policy refuses the password, and the old one still signs in.
        let code = admin().reset_code(&inv.user).await;
        let (status, body) = ui(
            "accept-invite",
            reset_accept(&inv.user, &code, ZITADEL_WEAK),
        )
        .await;
        assert_eq!((status, code_of(&body)), (400, "weak_password"), "{body}");
        admin()
            .client()
            .create_session(&inv.user, STRONG)
            .await
            .expect("the password is unchanged");
    })
    .await;
}

/// #78: a removed member asks for a reset code (anyone can) and uses it on accept-invite. The
/// code checks out, but with no open invite they stay inactive and get no session.
#[tokio::test]
#[ignore = "needs the workbench stack and demo.env (Zitadel admin)"]
async fn a_removed_members_own_reset_code_never_reactivates() {
    with_invitee("removed", |inv| async move {
        let (status, body) = ui("accept-invite", accept(&inv.user, &inv.code, STRONG)).await;
        assert_eq!(status, 200, "accept: {body}");
        let (status, body) =
            api(Method::DELETE, &format!("/v1/members/{}", inv.member), None).await;
        assert!(status == 200 || status == 204, "remove: {status} {body}");
        let z = admin();
        assert_eq!(
            z.state(&inv.user).await,
            UserState::Inactive,
            "removal deactivates"
        );
        let code = z.reset_code(&inv.user).await;
        let resp = reqwest::Client::new()
            .post(format!("{}/auth/v1/accept-invite", gateway()))
            .header("origin", need("CHELABASE_ENTRY_STUDIO_URL"))
            .json(&reset_accept(&inv.user, &code, "Other-pass-34!"))
            .timeout(Duration::from_secs(60))
            .send()
            .await
            .unwrap();
        let status = resp.status().as_u16();
        let cookies: Vec<String> = resp
            .headers()
            .get_all(reqwest::header::SET_COOKIE)
            .iter()
            .map(|v| v.to_str().unwrap_or("").to_string())
            .collect();
        let body: Value = resp.json().await.unwrap_or(Value::Null);
        assert_eq!((status, code_of(&body)), (400, "invalid_request"), "{body}");
        assert!(
            !cookies
                .iter()
                .any(|c| c.starts_with("chelabase_session=") && !c.contains("Max-Age=0")),
            "{cookies:?}"
        );
        assert_eq!(
            z.state(&inv.user).await,
            UserState::Inactive,
            "still inactive"
        );
    })
    .await;
}

/// A project of the demo tenant with a ready database and app users enabled: runs `checks`
/// with the public key, then deletes the database and the project, pass or fail.
async fn with_app_users<F, Fut>(checks: F)
where
    F: FnOnce(String) -> Fut,
    Fut: Future<Output = ()> + Send + 'static,
{
    let tag = &Uuid::new_v4().simple().to_string()[..8];
    let (status, project) = api(
        Method::POST,
        "/v1/projects",
        Some(json!({"name": format!("auth-live-{tag}")})),
    )
    .await;
    assert_eq!(status, 201, "project: {project}");
    let project_id = project["id"].as_str().unwrap().to_string();
    let slug = project["slug"].as_str().unwrap().to_string();
    let (status, created) = api(
        Method::POST,
        "/v1/databases",
        Some(json!({"tenant_id": DEMO_TENANT_ID, "project_id": project_id, "name": format!("users_{tag}")})),
    )
    .await;
    let db = created["database_id"].as_str().map(str::to_string);
    let setup = async {
        if status != 201 && status != 202 {
            return Err(format!("database: {status} {created}"));
        }
        let db = db.clone().unwrap();
        let mut ready = false;
        for _ in 0..180 {
            let (_, d) = api(Method::GET, &format!("/v1/databases/{db}"), None).await;
            match d["status"].as_str() {
                Some("ready") => {
                    ready = true;
                    break;
                }
                Some("failed") => return Err(format!("database failed: {d}")),
                _ => tokio::time::sleep(Duration::from_secs(1)).await,
            }
        }
        if !ready {
            return Err("the database isn't ready after 180 s".into());
        }
        let (_, keys) = api(
            Method::GET,
            &format!("/v1/databases/{db}/branches/{db}/keys"),
            None,
        )
        .await;
        let key = keys
            .as_array()
            .and_then(|k| k.iter().find(|k| k["kind"] == "public"))
            .and_then(|k| k["key"].as_str())
            .ok_or_else(|| format!("no public key: {keys}"))?
            .to_string();
        let (status, body) = api(
            Method::PUT,
            &format!("/v1/projects/{project_id}/app-users"),
            // Sign-up signs in at once: the live tests read no mail.
            Some(json!({"database_id": db, "email_verification": false})),
        )
        .await;
        if status != 202 {
            return Err(format!("enable app users: {status} {body}"));
        }
        for _ in 0..120 {
            let (_, a) = api(
                Method::GET,
                &format!("/v1/projects/{project_id}/app-users"),
                None,
            )
            .await;
            match a["status"].as_str() {
                Some("enabled") => return Ok(key),
                Some("failed") => return Err(format!("enabling app users failed: {a}")),
                _ => tokio::time::sleep(Duration::from_secs(1)).await,
            }
        }
        Err("app users aren't enabled after 120 s".into())
    }
    .await;
    let outcome = match setup {
        Ok(key) => Ok(tokio::spawn(checks(key)).await),
        Err(e) => Err(e),
    };
    if let Some(db) = &db {
        let _ = api(Method::DELETE, &format!("/v1/databases/{db}"), None).await;
    }
    let _ = api(
        Method::DELETE,
        &format!("/v1/projects/{project_id}"),
        Some(json!({"confirm": slug})),
    )
    .await;
    match outcome {
        Ok(Ok(())) => {}
        Ok(Err(panic)) => std::panic::resume_unwind(panic.into_panic()),
        Err(e) => panic!("{e}"),
    }
}

/// An app-user call made as a signed-in user: the `Chelabase-User` header carries the access
/// token, as the SDK sends it.
async fn app_as(key: &str, access: &str, path: &str, body: Value) -> (u16, Value) {
    send(
        reqwest::Client::new()
            .post(format!("{}/auth/v1/app/{path}", gateway()))
            .bearer_auth(key)
            .header("chelabase-user", access)
            .json(&body),
    )
    .await
}

/// #123: app users live in the project's identity pool. Sign-up, sign-in and refresh run there
/// (refresh checks the identity session behind the app session), and a password change on one
/// device ends the other's session at its next refresh. Retires
/// `zitadel_policy_refusals_are_weak_passwords_for_app_users`: identity has no Zitadel policy,
/// and auth's own password rule answers first.
#[tokio::test]
#[ignore = "needs the workbench stack and CHELABASE_COMPUTE_IMAGE"]
async fn app_user_sessions_live_in_the_identity_pool() {
    with_app_users(|key| async move {
        let tag = &Uuid::new_v4().simple().to_string()[..8];
        let email = format!("user-{tag}@example.com");
        let (status, one) = app(&key, "sign-up", json!({"email": email, "password": STRONG})).await;
        assert_eq!(status, 201, "sign-up: {one}");
        let one_access = one["access_token"].as_str().unwrap_or_else(|| panic!("no tokens: {one}"));
        let (status, two) = app(&key, "sign-in", json!({"email": email, "password": STRONG})).await;
        assert_eq!(status, 200, "sign-in: {two}");
        let (status, two) = app(&key, "refresh", json!({"refresh_token": two["refresh_token"]})).await;
        assert_eq!(status, 200, "refresh: {two}");
        let (status, wrong) = app(&key, "sign-in", json!({"email": email, "password": "Wrong-pass-12!"})).await;
        assert_eq!((status, code_of(&wrong)), (401, "invalid_credentials"), "{wrong}");
        // Device one changes the password: device two's next refresh is refused.
        let new_password = "Other-pass-34!";
        let (status, changed) = app_as(
            &key,
            one_access,
            "change-password",
            json!({"current_password": STRONG, "password": new_password}),
        )
        .await;
        assert_eq!(status, 200, "change-password: {changed}");
        let (status, body) = app(&key, "refresh", json!({"refresh_token": two["refresh_token"]})).await;
        assert_eq!(status, 401, "the other device's refresh: {body}");
        let (status, body) = app(&key, "refresh", json!({"refresh_token": changed["refresh_token"]})).await;
        assert_eq!(status, 200, "the new session's refresh: {body}");
        let (status, _) = app(&key, "sign-in", json!({"email": email, "password": STRONG})).await;
        assert_eq!(status, 401, "the old password");
        let (status, body) = app(&key, "sign-in", json!({"email": email, "password": new_password})).await;
        assert_eq!(status, 200, "the new password: {body}");
    })
    .await;
}
