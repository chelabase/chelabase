//! Auth against the real stack: the stack's auth, orchestrator and identity service, through the
//! gateway as Studio and an app reach them. The unit tests prove the arms against the fakes;
//! these prove the real services' answers reach them.
//!
//! Runs in the workbench runner, which has the gateway's entry URLs, `CHELABASE_CLI`
//! (`chelabase dev token`: the demo tenant admin), the identity service's URL and auth's token
//! for it (`CHELABASE_IDENTITY_URL`, `CHELABASE_IDENTITY_TOKEN_FILE`) and the metadata
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

use chelabase_identity::rules::MEMBER_REMOVED_BAN_REASON;
use chelabase_identity::{CodeKind, CodeRequest, Delivery, Identity, Pool};
use reqwest::Method;
use serde_json::{Value, json};
use uuid::Uuid;

const DEMO_TENANT_ID: &str = "00000000-0000-4000-8000-000000000001";

/// Passes auth's password rule.
const STRONG: &str = "Live-pass-12!";

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

/// The identity service as auth calls it (auth's token): codes returned instead of mailed, user
/// reads and deletes.
fn identity() -> Identity {
    let file = PathBuf::from(need("CHELABASE_IDENTITY_TOKEN_FILE"));
    let file = if file.is_relative() { workspace_root().join(file) } else { file };
    Identity::from_token_file(&need("CHELABASE_IDENTITY_URL"), &file).expect("auth's identity token")
}

/// A code of `kind` for the member `user`, handed back instead of mailed.
async fn code_for(user: &str, kind: CodeKind) -> String {
    identity()
        .create_code(&Pool::Members, &CodeRequest::new(user, kind, Delivery::Return))
        .await
        .unwrap_or_else(|e| panic!("a {kind:?} code for {user}: {e}"))
        .code
        .expect("a returned code")
}

/// One statement against the metadata database, as the e2e scripts' `metadata_sql` runs it.
/// [`metadata_sql`] that reports a failure instead of panicking, for cleanups.
async fn try_metadata_sql(sql: &str) -> Result<String, String> {
    let out = tokio::process::Command::new("psql")
        .arg(need("CHELABASE_METADATA_DATABASE_URL"))
        .args(["-v", "ON_ERROR_STOP=1", "-qtA", "-c", sql])
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|e| format!("{sql}: {e}"))?;
    if !out.status.success() {
        return Err(format!("{sql}: {}", String::from_utf8_lossy(&out.stderr)));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

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
    email: String,
    user: String,
    code: String,
    t: String,
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
/// identity user, pass or fail.
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
        email: email.clone(),
        user: query_param(url, "user"),
        code: query_param(url, "code"),
        t: query_param(url, "t"),
    };
    let outcome = tokio::spawn(checks(invited.clone())).await;
    let _ = api(
        Method::DELETE,
        &format!("/v1/members/{}", invited.member),
        None,
    )
    .await;
    let _ = identity().delete_user(&Pool::Members, &invited.user).await;
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

/// Every person `member_sign_in_live` signs up, this run's and any an earlier run left behind.
const LIVE_MEMBERS: &str = "auth-live-member-%@example.com";

/// Undoes what `member_sign_in_live` changes, whatever state a run stopped in, and never panics
/// midway: the demo tenant's open sign-up goes back first, then each member is removed through the
/// API as an admin would (`access_events` is append-only and names the row, so it is never
/// deleted), then their identity users go. Returns what failed.
///
/// Open sign-up goes back to the demo tenant's baseline, `false` (the column's default; the
/// seed never sets it), never to a value read at the start: a run killed after switching it on
/// would otherwise make `true` the next run's "original".
async fn member_sign_in_cleanup() -> Vec<String> {
    let mut failed = Vec::new();
    if let Err(e) = try_metadata_sql(&format!("UPDATE tenants SET open_signup = false WHERE id = '{DEMO_TENANT_ID}'")).await {
        failed.push(e);
    }
    let ids = try_metadata_sql(&format!(
        "SELECT id FROM members WHERE tenant_id = '{DEMO_TENANT_ID}' AND email LIKE '{LIVE_MEMBERS}' AND status <> 'removed'"
    ))
    .await;
    match ids {
        Ok(ids) => {
            for id in ids.lines().filter(|l| !l.is_empty()) {
                let path = format!("/v1/members/{id}");
                match tokio::spawn(async move { api(Method::DELETE, &path, None).await }).await {
                    // 503: removed here, only identity's follow-up steps failed.
                    Ok((204 | 503, _)) => {}
                    Ok((status, body)) => failed.push(format!("removing member {id}: {status} {body}")),
                    Err(e) => failed.push(format!("removing member {id}: {e}")),
                }
            }
        }
        Err(e) => failed.push(e),
    }
    if let Err(e) = try_metadata_sql(&format!(r#"DELETE FROM identity_members."user" WHERE email LIKE '{LIVE_MEMBERS}'"#)).await {
        failed.push(e);
    }
    failed
}

/// #121 AC 1 against the stack: an open sign-up waits as pending, and once active the member signs
/// in, gets a token the gateway and orchestrator accept, and loses it at logout. Open sign-up is
/// switched on for the demo tenant meanwhile; [`member_sign_in_cleanup`] runs afterwards, pass or
/// fail.
#[tokio::test]
#[ignore = "needs the workbench stack and the metadata database"]
async fn member_sign_in_live() {
    let email = format!("auth-live-member-{}@example.com", &Uuid::new_v4().simple().to_string()[..12]);
    // Leftovers of an earlier run first: their pending rows and users would get in the way.
    let stale = member_sign_in_cleanup().await;
    assert!(stale.is_empty(), "cleaning up an earlier run: {stale:?}");
    let run = email.clone();
    let outcome = tokio::spawn(async move {
        let email = run;
        metadata_sql(&format!("UPDATE tenants SET open_signup = true WHERE id = '{DEMO_TENANT_ID}'")).await;
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
        metadata_sql(&format!("UPDATE members SET status = 'suspended', suspended_at = now() WHERE tenant_id = '{DEMO_TENANT_ID}' AND email = '{email}'")).await;
        assert_eq!(session_call("token", Some(&cookie), None).await.0, 401, "suspended");
        metadata_sql(&format!("UPDATE members SET status = 'active', suspended_at = NULL WHERE tenant_id = '{DEMO_TENANT_ID}' AND email = '{email}'")).await;
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
    let failed = member_sign_in_cleanup().await;
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic.into_panic());
    }
    assert!(failed.is_empty(), "cleanup: {failed:?}");
}

fn accept(inv: &Invited, code: &str, password: &str) -> Value {
    json!({"user": inv.user, "t": inv.t, "code": code, "password": password})
}

/// `POST /auth/v1/accept-invite` as Studio sends it: the status, the body and the session cookie
/// if one was set.
async fn accept_call(body: Value) -> (u16, Value, Option<String>) {
    session_call("accept-invite", None, Some(body)).await
}

/// #124 AC 1 against the stack: the link's code sets the first password and verifies the email,
/// the member row turns active, and the session cookie mints a token for the demo tenant. The
/// code is single use.
#[tokio::test]
#[ignore = "needs the workbench stack and the identity service"]
async fn invite_accept_live() {
    with_invitee("accept", |inv| async move {
        let (status, body, cookie) = accept_call(accept(&inv, &inv.code, STRONG)).await;
        assert_eq!((status, &body), (200, &json!({"next": "studio"})), "accept");
        let cookie = cookie.expect("the session cookie");
        let (status, body, _) = session_call("token", Some(&cookie), None).await;
        assert_eq!(status, 200, "token: {body}");
        let user = identity().get_user(&Pool::Members, &inv.user).await.unwrap();
        assert!(user.email_verified, "the code verified the email");
        let row = metadata_sql(&format!("SELECT status FROM members WHERE id = '{}'", inv.member)).await;
        assert_eq!(row, "active");
        let (status, body, cookie) = accept_call(accept(&inv, &inv.code, STRONG)).await;
        assert_eq!((status, code_of(&body), cookie), (400, "invalid_request", None), "the code again: {body}");
    })
    .await;
}

/// Ported from the Zitadel era (#124): an invitee whose identity user was deleted gets 410
/// `invite_revoked`.
#[tokio::test]
#[ignore = "needs the workbench stack and the identity service"]
async fn accept_invite_for_a_deleted_user_is_revoked() {
    with_invitee("deleted", |inv| async move {
        identity().delete_user(&Pool::Members, &inv.user).await.unwrap();
        let (status, body) = ui("accept-invite", accept(&inv, &inv.code, STRONG)).await;
        assert_eq!((status, code_of(&body)), (410, "invite_revoked"), "{body}");
    })
    .await;
}

/// The stored invite tells an expired link from a wrong code: a wrong code on an invite whose
/// stored expiry passed is 410 `invite_expired`; once it is open again, 400 `invalid_request`;
/// the right code still works (ported to the identity service's codes, #124).
#[tokio::test]
#[ignore = "needs the workbench stack and the identity service"]
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
        let (status, body) = ui("accept-invite", accept(&inv, "000000", STRONG)).await;
        assert_eq!((status, code_of(&body)), (410, "invite_expired"), "{body}");
        set_expiry("+ interval '1 day'").await;
        let (status, body) = ui("accept-invite", accept(&inv, "000000", STRONG)).await;
        assert_eq!((status, code_of(&body)), (400, "invalid_request"), "{body}");
        let (status, body) = ui("accept-invite", accept(&inv, &inv.code, STRONG)).await;
        assert_eq!(status, 200, "the right code: {body}");
    })
    .await;
}

/// Replaces `zitadel_policy_refusals_are_weak_passwords_in_accept_invite` (#124): a weak password
/// is refused (`weak_password`) before the code is used, with no ticket, so the same code then
/// sets a strong one; and a reset's confirm refuses it before anything changes.
#[tokio::test]
#[ignore = "needs the workbench stack and the identity service"]
async fn weak_passwords_keep_the_invite_code_live() {
    with_invitee("weak", |inv| async move {
        let (status, body) = ui("accept-invite", accept(&inv, &inv.code, "weakpass")).await;
        assert_eq!((status, code_of(&body)), (400, "weak_password"), "{body}");
        assert!(body["error"].get("accept_ticket").is_none(), "nothing was spent: {body}");
        let (status, body) = ui("accept-invite", accept(&inv, &inv.code, STRONG)).await;
        assert_eq!(status, 200, "the same code: {body}");
        let code = code_for(&inv.user, CodeKind::Reset).await;
        let (status, body) = ui(
            "password-reset/confirm",
            json!({"email": inv.email, "code": code, "password": "weakpass"}),
        )
        .await;
        assert_eq!((status, code_of(&body)), (400, "weak_password"), "{body}");
        identity()
            .sign_in(&Pool::Members, &inv.email, STRONG)
            .await
            .expect("the password is unchanged");
    })
    .await;
}

/// #78, ported to the identity service (#124): a removed member (banned with our reason) asks for
/// a reset code (anyone can) and uses it. The reset sets the password, but nothing lifts the ban:
/// the code accepts no invite, sign-in sets no session, and the ban keeps its reason.
#[tokio::test]
#[ignore = "needs the workbench stack and the identity service"]
async fn a_removed_members_own_reset_code_never_reactivates() {
    with_invitee("removed", |inv| async move {
        let (status, body) = ui("accept-invite", accept(&inv, &inv.code, STRONG)).await;
        assert_eq!(status, 200, "accept: {body}");
        let (status, body) =
            api(Method::DELETE, &format!("/v1/members/{}", inv.member), None).await;
        assert!(status == 200 || status == 204, "remove: {status} {body}");
        let banned = || async {
            let d = identity().user_detail(&Pool::Members, &inv.user).await.unwrap();
            (d.user.banned, d.ban_reason)
        };
        assert_eq!(banned().await, (true, Some(MEMBER_REMOVED_BAN_REASON.to_string())), "removal bans");
        let code = code_for(&inv.user, CodeKind::Reset).await;
        let (status, body, cookie) = accept_call(accept(&inv, &code, "Other-pass-34!")).await;
        assert_eq!((status, code_of(&body), cookie), (400, "invalid_request", None), "{body}");
        let (status, body) = ui(
            "password-reset/confirm",
            json!({"email": inv.email, "code": code, "password": "Other-pass-34!"}),
        )
        .await;
        assert_eq!(status, 200, "the reset itself: {body}");
        let (status, body, cookie) =
            session_call("sign-in", None, Some(json!({"email": inv.email, "password": "Other-pass-34!"}))).await;
        assert_eq!((status, code_of(&body), cookie), (401, "invalid_credentials", None), "{body}");
        assert_eq!(banned().await, (true, Some(MEMBER_REMOVED_BAN_REASON.to_string())), "still banned");
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
