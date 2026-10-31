//! A stateful fake of the public endpoints the seed uses (auth, Zitadel's OIDC, the members
//! API), with crash and failure injection. Every secret it hands out is recorded, so a test can
//! scan output for them.

use super::session::{Body, Http, Request, Response, Site};
use anyhow::{Result, anyhow, bail};
use base64::Engine;
use reqwest::{Method, Url};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::sync::{Mutex, OnceLock};

pub const SETUP_CODE: &str = "SETU-PC0D-E234-5678-9ABC";
const STUDIO: &str = "http://studio.chelabase.localhost:8000";

/// The test environment: a local stack.
pub fn env(name: &str) -> Option<String> {
    static DIR: OnceLock<tempfile::TempDir> = OnceLock::new();
    let dir = DIR.get_or_init(|| {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("setup.code");
        std::fs::write(&f, format!("{SETUP_CODE}\n")).unwrap();
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o600)).unwrap();
        d
    });
    let v = match name {
        "CHELABASE_PUBLIC_SCHEME" => "http",
        "CHELABASE_STUDIO_HOST" => "studio.chelabase.localhost:8000",
        "CHELABASE_API_HOST" => "api.chelabase.localhost:8000",
        "CHELABASE_AUTH_HOST" => "auth.chelabase.localhost:8000",
        "CHELABASE_EXAMPLE_GATEWAY_URL" => "http://gateway:8000",
        "CHELABASE_HTTP_ADDR" | "CHELABASE_PROXY_ADDR" => "127.0.0.1",
        "CHELABASE_SETUP_CODE_FILE" => {
            return Some(dir.path().join("setup.code").display().to_string());
        }
        _ => return None,
    };
    Some(v.to_string())
}

pub fn config() -> super::Config {
    let mut cfg = super::Config::from_lookup(env).unwrap();
    cfg.timeout = std::time::Duration::from_secs(5);
    cfg
}

#[derive(Debug, Clone)]
pub struct Member {
    pub user: String,
    pub status: &'static str,
    pub admin: bool,
    pub invite: Option<String>,
}

#[derive(Default)]
pub struct State {
    /// The email the setup claim is bound to (the orchestrator's `setup_claims` row). A code
    /// sign-up only binds it; the claimant's first bearer request makes the admin member row.
    pub claimed_by: Option<String>,
    pub open_sign_up: bool,
    /// email -> (user id, password)
    pub users: BTreeMap<String, (String, Option<String>)>,
    pub members: BTreeMap<String, Member>,
    /// authRequest -> (state, challenge)
    pending: BTreeMap<String, (String, String)>,
    /// code -> (email, challenge)
    codes: BTreeMap<String, (String, String)>,
    /// user id -> open invite code
    invite_codes: BTreeMap<String, String>,
    tokens: BTreeMap<String, String>,
    pub log: Vec<String>,
    secrets: Vec<String>,
    next: u32,
    calls: usize,
    fail_at: Option<usize>,
    /// Apply the nth (1-based) call with this key, then drop the answer.
    crash: Option<(String, usize)>,
}

pub struct Fake(pub Mutex<State>);

const WRITES: [&str; 5] = [
    "POST /auth/v1/sign-up",
    "POST /v1/invites",
    "POST /v1/invites/resend",
    "POST /auth/v1/accept-invite",
    "POST /fake/claim",
];

impl Fake {
    pub fn new() -> Self {
        let s = State {
            secrets: vec![SETUP_CODE.to_string()],
            ..State::default()
        };
        Self(Mutex::new(s))
    }

    pub fn calls(&self) -> usize {
        self.0.lock().unwrap().calls
    }

    pub fn fail_at(&self, n: usize) {
        self.0.lock().unwrap().fail_at = Some(n);
    }

    pub fn crash_after(&self, key: &str, nth: usize) {
        self.0.lock().unwrap().crash = Some((key.to_string(), nth));
    }

    pub fn secrets(&self) -> Vec<String> {
        self.0.lock().unwrap().secrets.clone()
    }

    pub fn count(&self, key: &str) -> usize {
        self.0
            .lock()
            .unwrap()
            .log
            .iter()
            .filter(|k| *k == key)
            .count()
    }

    pub fn writes(&self) -> usize {
        self.0
            .lock()
            .unwrap()
            .log
            .iter()
            .filter(|k| WRITES.contains(&k.as_str()))
            .count()
    }

    /// Someone else claims the instance first.
    pub fn claim_as(&self, email: &str) {
        let mut s = self.0.lock().unwrap();
        let id = s.id();
        s.users
            .insert(email.into(), (id.clone(), Some("theirs".into())));
        s.members.insert(
            email.into(),
            Member {
                user: id,
                status: "active",
                admin: true,
                invite: None,
            },
        );
        s.claimed_by = Some(email.into());
    }

    pub fn status(&self, email: &str) -> Option<&'static str> {
        self.0.lock().unwrap().members.get(email).map(|m| m.status)
    }
}

impl State {
    fn id(&mut self) -> String {
        self.next += 1;
        format!("{}", 300_000 + self.next)
    }

    fn secret(&mut self, prefix: &str) -> String {
        self.next += 1;
        let s = format!("{prefix}{:06}X", self.next);
        self.secrets.push(s.clone());
        s
    }

    fn done(&mut self, auth_request: &str, email: &str) -> Result<Value> {
        let (state, challenge) = self
            .pending
            .remove(auth_request)
            .ok_or_else(|| anyhow!("unknown authRequest"))?;
        let code = self.secret("OIDCCODE");
        self.codes
            .insert(code.clone(), (email.to_string(), challenge));
        Ok(
            json!({"next": "done", "redirect": format!("{STUDIO}/callback?code={code}&state={state}")}),
        )
    }

    /// `setupRequired` as the orchestrator answers it: no member that isn't removed.
    fn setup_required(&self) -> bool {
        self.members.values().all(|m| m.status == "removed")
    }

    fn create_user(&mut self, email: &str, password: Option<String>) -> String {
        let id = self.id();
        self.users.insert(email.into(), (id.clone(), password));
        id
    }

    fn create_member(&mut self, email: &str, status: &'static str, admin: bool) -> String {
        let id = self.create_user(email, None);
        self.members.insert(
            email.into(),
            Member {
                user: id.clone(),
                status,
                admin,
                invite: None,
            },
        );
        id
    }

    fn invite_link(&mut self, email: &str) -> String {
        let user = self.members[email].user.clone();
        let code = self.secret("INVCODE");
        let t = self.secret("LINKTOKEN");
        self.invite_codes.insert(user.clone(), code.clone());
        format!("{STUDIO}/accept-invite?user={user}&code={code}&t={t}")
    }

    /// The caller of a bearer request. Like the orchestrator's `admit_member`, their first
    /// request makes their member row: the tenant admin when the setup claim is bound to them
    /// and the tenant has no active admin, else a pending newcomer. It also activates an
    /// accepted invite.
    fn caller(&mut self, r: &Request) -> Option<String> {
        let email = self.tokens.get(r.bearer.as_ref()?.expose())?.clone();
        if !self.members.contains_key(&email) {
            let user = self.users[&email].0.clone();
            let has_admin = self
                .members
                .values()
                .any(|m| m.admin && m.status == "active");
            let admin = !has_admin && self.claimed_by.as_deref() == Some(email.as_str());
            let member = Member {
                user,
                status: if admin { "active" } else { "pending" },
                admin,
                invite: None,
            };
            self.members.insert(email.clone(), member);
        }
        let has_password = self.users.get(&email).is_some_and(|u| u.1.is_some());
        if let Some(m) = self.members.get_mut(&email)
            && m.status == "invited"
            && has_password
        {
            m.status = "active";
            m.invite = None;
        }
        Some(email)
    }

    fn handle(&mut self, r: &Request, key: &str) -> Result<(u16, Value, Option<String>)> {
        let body = match &r.body {
            Body::Json(v) => v.clone(),
            _ => Value::Null,
        };
        let s = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
        let err = |status, code: &str| {
            Ok((
                status,
                json!({"error": {"code": code, "message": "no"}}),
                None,
            ))
        };
        Ok(match key {
            "GET /auth/v1/studio-config" => (
                200,
                json!({"issuer": "http://auth.chelabase.localhost:8000", "clientId": "studio-1", "projectId": "p1", "orgId": "o1", "apiUrl": "x", "studioHost": "y"}),
                None,
            ),
            "GET /auth/v1/instance" => (200, json!({"setupRequired": self.setup_required()}), None),
            "GET /oauth/v2/authorize" => {
                let url = Url::parse(&format!("http://x{}", r.path))?;
                let q: BTreeMap<String, String> = url.query_pairs().into_owned().collect();
                if q["client_id"] != "studio-1"
                    || q["redirect_uri"] != format!("{STUDIO}/callback")
                    || q["code_challenge_method"] != "S256"
                {
                    bail!("bad authorize: {q:?}");
                }
                let id = format!("V2_{}", self.id());
                self.secrets.push(id.clone());
                self.pending.insert(
                    id.clone(),
                    (q["state"].clone(), q["code_challenge"].clone()),
                );
                (
                    302,
                    Value::Null,
                    Some(format!("{STUDIO}/login?authRequest={id}")),
                )
            }
            "POST /auth/v1/sign-up" => {
                let email = s(&body, "email");
                if self.users.contains_key(&email) {
                    return err(409, "email_taken");
                }
                // The account only: the member row comes with the first bearer request.
                if self.setup_required() {
                    if body.get("setupCode").and_then(Value::as_str) != Some(SETUP_CODE) {
                        return err(403, "setup_code_invalid");
                    }
                    // `bind_claim`: a claim already bound leaves this account a newcomer.
                    if self.claimed_by.is_none() {
                        self.claimed_by = Some(email.clone());
                    }
                } else if !self.open_sign_up {
                    return err(403, "sign_up_closed");
                }
                self.create_user(&email, Some(s(&body, "password")));
                (200, self.done(&s(&body, "authRequest"), &email)?, None)
            }
            "POST /auth/v1/sign-in" => {
                let email = s(&body, "email");
                match self.users.get(&email) {
                    Some((_, Some(p))) if *p == s(&body, "password") => {
                        (200, self.done(&s(&body, "authRequest"), &email)?, None)
                    }
                    _ => return err(401, "invalid_credentials"),
                }
            }
            "POST /auth/v1/accept-invite" => {
                let user = s(&body, "user");
                if self.invite_codes.get(&user).map(String::as_str)
                    != Some(s(&body, "code").as_str())
                {
                    return err(400, "invalid_request");
                }
                self.invite_codes.remove(&user);
                let email = self
                    .users
                    .iter()
                    .find(|(_, u)| u.0 == user)
                    .map(|(e, _)| e.clone())
                    .unwrap();
                self.users.get_mut(&email).unwrap().1 = Some(s(&body, "password"));
                (200, self.done(&s(&body, "authRequest"), &email)?, None)
            }
            "POST /oauth/v2/token" => {
                let Body::Form(f) = &r.body else {
                    bail!("token wants a form")
                };
                let f: BTreeMap<&str, &str> = f.iter().map(|(k, v)| (*k, v.as_str())).collect();
                let Some((email, challenge)) = self.codes.remove(f["code"]) else {
                    return err(400, "invalid_grant");
                };
                let want = base64::engine::general_purpose::URL_SAFE_NO_PAD
                    .encode(Sha256::digest(f["code_verifier"].as_bytes()));
                self.secrets.push(f["code_verifier"].to_string());
                if want != challenge
                    || f["redirect_uri"] != format!("{STUDIO}/callback")
                    || f["client_id"] != "studio-1"
                {
                    return err(400, "invalid_grant");
                }
                let token = format!("eyJ{}", self.secret("TOKEN"));
                self.secrets.push(token.clone());
                self.tokens.insert(token.clone(), email);
                (
                    200,
                    json!({"access_token": token, "token_type": "Bearer"}),
                    None,
                )
            }
            _ => {
                let Some(caller) = self.caller(r) else {
                    return err(401, "unauthorized");
                };
                let me = self.members.get(&caller).cloned();
                match key {
                    "GET /v1/me" => (
                        200,
                        json!({"member": me.map(|m| json!({"status": m.status, "tenant_admin": m.admin}))}),
                        None,
                    ),
                    _ if !me.as_ref().is_some_and(|m| m.status == "active" && m.admin) => {
                        return err(403, "forbidden");
                    }
                    "GET /v1/members" => {
                        let list: Vec<Value> = self.members.iter().map(|(e, m)| json!({"email": e, "status": m.status, "tenant_admin": m.admin, "invite": m.invite.as_ref().map(|i| json!({"id": i}))})).collect();
                        (200, Value::Array(list), None)
                    }
                    "POST /v1/invites" => {
                        let email = s(&body, "email");
                        assert_eq!(body["delivery"], "link");
                        if self.members.contains_key(&email) {
                            return Ok((200, json!({"outcome": "existing_member"}), None));
                        }
                        self.create_member(&email, "invited", false);
                        let invite = format!("inv-{}", self.id());
                        self.members.get_mut(&email).unwrap().invite = Some(invite);
                        let url = self.invite_link(&email);
                        (201, json!({"outcome": "invited", "invite_url": url}), None)
                    }
                    "POST /v1/invites/resend" => {
                        assert_eq!(body["delivery"], "link");
                        let id = r
                            .path
                            .trim_start_matches("/v1/invites/")
                            .trim_end_matches("/resend");
                        let Some(email) = self
                            .members
                            .iter()
                            .find(|(_, m)| m.invite.as_deref() == Some(id))
                            .map(|(e, _)| e.clone())
                        else {
                            return err(409, "invite_closed");
                        };
                        if self.users[&email].1.is_some() {
                            return err(409, "link_not_allowed");
                        }
                        let url = self.invite_link(&email);
                        (200, json!({"id": id, "invite_url": url}), None)
                    }
                    _ => bail!("the fake has no {key}"),
                }
            }
        })
    }
}

impl Http for Fake {
    async fn send(&self, r: &Request) -> Result<Response> {
        let mut s = self.0.lock().unwrap();
        let path = r.path.split('?').next().unwrap_or("");
        let path = if path.starts_with("/v1/invites/") && path.ends_with("/resend") {
            "/v1/invites/resend"
        } else {
            path
        };
        let key = format!("{} {path}", r.method);
        let site_ok = match path {
            "/oauth/v2/authorize" | "/oauth/v2/token" => r.site == Site::Auth,
            _ => r.site == Site::Api,
        };
        assert!(site_ok, "{key} to the wrong host");
        assert!(r.method != Method::GET || r.idempotent, "{key}");
        let n = s.calls;
        s.calls += 1;
        if s.fail_at == Some(n) {
            // Everything a careless client might print.
            let all = s.secrets.join(" code=");
            let body = json!({"error": {"code": "internal", "message": format!("code={all} t={all} Bearer eyJ{all}")}});
            return Ok(Response {
                status: 500,
                location: Some(format!("{STUDIO}/callback?code={all}")),
                retry_after: None,
                body: body.to_string().into_bytes(),
            });
        }
        s.log.push(key.clone());
        let answer = s.handle(r, &key)?;
        if let Some((k, nth)) = &s.crash
            && *k == key
            && s.log.iter().filter(|l| **l == key).count() == *nth
        {
            s.crash = None;
            return Err(anyhow!("connection reset (crash)"));
        }
        let (status, body, location) = answer;
        Ok(Response {
            status,
            location,
            retry_after: None,
            body: if body.is_null() {
                Vec::new()
            } else {
                body.to_string().into_bytes()
            },
        })
    }
}
