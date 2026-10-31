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
    pub acme: Acme,
}

/// The Acme organisation as the management API holds it.
#[derive(Default)]
pub struct Acme {
    /// id, name, app users status
    pub projects: Vec<(String, String, Option<&'static str>)>,
    /// id, name, project id
    pub databases: Vec<(String, String, String)>,
    pub roles: Vec<String>,
    pub teams: Vec<FTeam>,
    /// (member id, database id)
    pub overrides: Vec<(String, String)>,
    pub data_api_on: bool,
    pub default_schema: String,
    pub users: Vec<(String, String)>,
    pub schema_done: bool,
    pub feature: bool,
    pub feature_col: bool,
    pub protected: bool,
    pub keys: Vec<String>,
    pub queries: Vec<String>,
    /// Make the next non-probe SQL run end in this error.
    pub sql_error: Option<&'static str>,
}

#[derive(Debug, Clone)]
pub struct FTeam {
    pub id: String,
    pub name: String,
    pub members: Vec<String>,
    pub grant: Option<(String, Vec<String>)>,
}

pub struct Fake(pub Mutex<State>);

const WRITES: [&str; 22] = [
    "POST /auth/v1/sign-up",
    "POST /v1/invites",
    "POST /v1/invites/resend",
    "POST /auth/v1/accept-invite",
    "POST /fake/claim",
    "POST /v1/projects",
    "POST /v1/databases",
    "POST /v1/branches",
    "POST /v1/roles",
    "POST /v1/data-api",
    "PATCH /v1/pg-database",
    "PUT /v1/app-users",
    "POST /v1/users",
    "POST /v1/teams",
    "PUT /v1/team-grant",
    "PUT /v1/team-member",
    "PUT /v1/override",
    "POST /v1/sql",
    "POST /v1/keys",
    "POST /v1/sql-queries",
    "PATCH /v1/branch",
    "PUT /v1/app-users-settings",
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
                        {
                            let projects: Vec<Value> = self
                                .acme
                                .projects
                                .iter()
                                .map(|(id, name, _)| {
                                    let dbs: Vec<Value> = self
                                        .acme
                                        .databases
                                        .iter()
                                        .filter(|d| d.2 == *id)
                                        .map(|d| json!({"database_id": d.0, "name": d.1}))
                                        .collect();
                                    json!({"id": id, "name": name, "databases": dbs})
                                })
                                .collect();
                            {
                                let narrowed = me.as_ref().is_some_and(|m| {
                                    self.acme.overrides.iter().any(|(who, _)| *who == m.user)
                                });
                                let level = if narrowed { "viewer" } else { "admin" };
                                let mine: Vec<Value> = self.acme.projects.iter().map(|(id, _, _)| json!({"project_id": id, "platform_role": "admin",
                                    "databases": self.acme.databases.iter().filter(|d| d.2 == *id).map(|d| json!({"database_id": d.0, "platform_role": level})).collect::<Vec<_>>()})).collect();
                                json!({"member": me.map(|m| json!({"status": m.status, "tenant_admin": m.admin})), "tenant": {"projects": projects}, "projects": mine})
                            }
                        },
                        None,
                    ),
                    _ if !me.as_ref().is_some_and(|m| m.status == "active" && m.admin) => {
                        return err(403, "forbidden");
                    }
                    "GET /v1/members" => {
                        let list: Vec<Value> = self.members.iter().map(|(e, m)| json!({"id": m.user, "email": e, "status": m.status, "tenant_admin": m.admin, "invite": m.invite.as_ref().map(|i| json!({"id": i}))})).collect();
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
                    _ => return self.acme_handle(key, r, &body),
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
        let key = if path == "/v1/invites/resend" {
            format!("{} {path}", r.method)
        } else {
            normalise(&r.method, path, r)
        };
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
            body: match &body {
                Value::Null => Vec::new(),
                Value::String(lines) => lines.clone().into_bytes(),
                _ => body.to_string().into_bytes(),
            },
        })
    }
}

/// The key of a request: ids in the path become `{id}`, so one handler serves every id.
fn normalise(method: &Method, path: &str, r: &Request) -> String {
    let segs: Vec<&str> = path.split('/').collect();
    let mut out = Vec::new();
    for (i, s) in segs.iter().enumerate() {
        let id = ["prj-", "db-", "br-", "team-", "q-", "k-"]
            .iter()
            .any(|p| s.starts_with(p))
            || (s.len() >= 6 && s.bytes().all(|b| b.is_ascii_digit()));
        out.push(if id {
            "{id}".to_string()
        } else if i > 0 && segs[i - 1] == "pg-databases" {
            "{name}".to_string()
        } else {
            (*s).to_string()
        });
    }
    let key = format!("{method} {}", out.join("/"));
    // Writes the tests count get one key each; reads and SQL probes stay out of WRITES.
    let sql_write = matches!(&r.body, Body::Json(v) if !v["sql"].as_str().unwrap_or("select").starts_with("select"));
    match key.as_str() {
        k if k.ends_with("/sql") && *method == Method::POST => {
            if sql_write {
                "POST /v1/sql".into()
            } else {
                "POST /v1/sql-probe".into()
            }
        }
        "POST /v1/databases/{id}/branches" => "POST /v1/branches".into(),
        "POST /v1/databases/{id}/branches/{id}/roles" => "POST /v1/roles".into(),
        k if k.ends_with("/data-api") && *method == Method::POST => "POST /v1/data-api".into(),
        "PATCH /v1/databases/{id}/branches/{id}/pg-databases/{name}" => {
            "PATCH /v1/pg-database".into()
        }
        "PUT /v1/projects/{id}/app-users" => "PUT /v1/app-users".into(),
        "POST /v1/projects/{id}/users" => "POST /v1/users".into(),
        "PUT /v1/teams/{id}/grants/{id}" => "PUT /v1/team-grant".into(),
        "PUT /v1/teams/{id}/members/{id}" => "PUT /v1/team-member".into(),
        k if k.ends_with("/overrides") => "PUT /v1/override".into(),
        "POST /v1/databases/{id}/branches/{id}/keys" => "POST /v1/keys".into(),
        "POST /v1/databases/{id}/sql-queries" => "POST /v1/sql-queries".into(),
        "PATCH /v1/databases/{id}/branches/{id}" => "PATCH /v1/branch".into(),
        _ => key,
    }
}

impl State {
    /// The Acme endpoints. The path is read from the request; `key` is its normalised form.
    fn acme_handle(
        &mut self,
        key: &str,
        r: &Request,
        body: &Value,
    ) -> Result<(u16, Value, Option<String>)> {
        let path = r.path.split('?').next().unwrap_or("");
        let segs: Vec<&str> = path.split('/').collect();
        let s = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
        let json_ok = |status, v: Value| Ok((status, v, None));
        let branches = |a: &Acme| {
            let mut v = vec![
                json!({"id": "br-main", "name": "main", "kind": "normal", "parent_branch_id": null, "status": "ready", "protected": a.protected}),
            ];
            if a.feature {
                v.push(json!({"id": "br-feature", "name": "feature-discounts", "kind": "normal", "parent_branch_id": "br-main", "status": "ready", "protected": false}));
            }
            v
        };
        let team_json = |t: &FTeam, project: &str| {
            json!({"id": t.id, "name": t.name, "member_ids": t.members, "member_count": t.members.len(),
                "projects": t.grant.iter().map(|(role, pg)| json!({"project_id": project, "platform_role": role, "pg_roles": pg})).collect::<Vec<_>>()})
        };
        let project = self
            .acme
            .projects
            .first()
            .map(|p| p.0.clone())
            .unwrap_or_default();
        match key {
            "POST /v1/projects" => {
                let id = format!("prj-{}", self.id());
                self.acme.projects.push((id.clone(), s(body, "name"), None));
                json_ok(201, json!({"id": id}))
            }
            "POST /v1/databases" => {
                let id = format!("db-{}", self.id());
                self.acme.databases.push((id.clone(), s(body, "name"), s(body, "project_id")));
                json_ok(202, json!({"database_id": id, "job_id": "j", "status": "pending"}))
            }
            "GET /v1/databases/{id}" => json_ok(200, json!({"status": "ready"})),
            "GET /v1/databases/{id}/branches" => json_ok(200, Value::Array(branches(&self.acme))),
            "POST /v1/branches" => {
                assert_eq!(s(body, "parent_branch_id"), "br-main");
                self.acme.feature = true;
                json_ok(202, json!({"id": "br-feature"}))
            }
            "GET /v1/databases/{id}/branches/{id}/roles" => json_ok(
                200,
                Value::Array(self.acme.roles.iter().map(|r| json!({"name": r})).collect()),
            ),
            "POST /v1/roles" => {
                assert_eq!(body["login"], false);
                self.acme.roles.push(s(body, "name"));
                json_ok(201, json!({"name": s(body, "name")}))
            }
            "GET /v1/databases/{id}/branches/{id}/databases" => json_ok(
                200,
                json!([{"name": "acme", "main": true, "data_api": if self.acme.data_api_on { "on" } else { "off" },
                    "data_api_error": null, "default_schema": if self.acme.default_schema.is_empty() { "public" } else { &self.acme.default_schema }}]),
            ),
            "POST /v1/data-api" => {
                self.acme.data_api_on = true;
                json_ok(202, json!({"name": "acme"}))
            }
            "PATCH /v1/pg-database" => {
                self.acme.default_schema = s(body, "default_schema");
                json_ok(200, json!({"name": "acme"}))
            }
            "GET /v1/projects/{id}" => {
                let app = self.acme.projects.first().and_then(|p| p.2);
                json_ok(200, json!({"id": project, "app_users": app}))
            }
            "PUT /v1/app-users" => {
                assert!(self.acme.data_api_on, "app users need the data API");
                self.acme.projects[0].2 = Some("enabled");
                json_ok(202, json!({"status": "enabling"}))
            }
            "GET /v1/projects/{id}/users" => json_ok(
                200,
                json!({"users": self.acme.users.iter().map(|(e, i)| json!({"id": i, "email": e})).collect::<Vec<_>>(), "next_page_token": null}),
            ),
            "POST /v1/users" => {
                assert_eq!(body["password"], super::DEMO_PASSWORD);
                let id = self.id();
                self.acme.users.push((s(body, "email"), id.clone()));
                json_ok(201, json!({"id": id, "email": s(body, "email")}))
            }
            "GET /v1/teams" => json_ok(
                200,
                Value::Array(self.acme.teams.iter().map(|t| team_json(t, &project)).collect()),
            ),
            "POST /v1/teams" => {
                let id = format!("team-{}", self.id());
                let name = s(body, "name");
                if self.acme.teams.iter().any(|t| t.name.eq_ignore_ascii_case(&name)) {
                    return Ok((409, json!({"error": {"code": "conflict", "message": "no"}}), None));
                }
                self.acme.teams.push(FTeam { id: id.clone(), name, members: vec![], grant: None });
                json_ok(201, team_json(self.acme.teams.last().unwrap(), &project))
            }
            "GET /v1/teams/{id}" => match self.acme.teams.iter().find(|t| t.id == segs[3]) {
                Some(t) => json_ok(200, team_json(t, &project)),
                None => json_ok(404, json!({"error": {"code": "not_found", "message": "no"}})),
            },
            "PUT /v1/team-grant" => {
                let pg: Vec<String> = body["pg_roles"]
                    .as_array()
                    .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
                    .unwrap_or_default();
                for role in &pg {
                    assert!(self.acme.roles.contains(role), "grant names a role that doesn't exist: {role}");
                }
                let t = self.acme.teams.iter_mut().find(|t| t.id == segs[3]).unwrap();
                // Admin holds every role: the names are dropped.
                let pg = if body["platform_role"] == "admin" { vec![] } else { pg };
                t.grant = Some((s(body, "platform_role"), pg));
                json_ok(200, json!({}))
            }
            "PUT /v1/team-member" => {
                let t = self.acme.teams.iter_mut().find(|t| t.id == segs[3]).unwrap();
                if !t.members.iter().any(|m| m == segs[5]) {
                    t.members.push(segs[5].to_string());
                }
                json_ok(200, json!({}))
            }
            "GET /v1/projects/{id}/grants" => json_ok(
                200,
                Value::Array(
                    self.acme.overrides.iter().map(|(m, d)| json!({"member_id": m, "platform_role": "admin", "pg_roles": "all",
                        "overrides": [{"database_id": d, "platform_role": "viewer", "pg_roles": null}]})).collect(),
                ),
            ),
            "PUT /v1/override" => {
                self.acme.overrides.push((segs[5].to_string(), s(body, "database_id")));
                json_ok(200, json!({}))
            }
            "POST /v1/sql-probe" => {
                let sql = s(body, "sql");
                let done = if sql.contains("discount_pct") {
                    r.path.contains("br-feature") && self.acme.feature_col
                } else {
                    self.acme.schema_done
                };
                let on_feature = r.path.contains("br-feature");
                let done = if sql.contains("discount_pct") { on_feature && self.acme.feature_col } else { done };
                let rows = json!({"t": "rows", "statement": 0, "rows": [[if done { "true" } else { "false" }]]});
                json_ok(200, Value::String(format!("{rows}\n{}\n", json!({"t": "done", "outcome": "ok", "duration_ms": 1}))))
            }
            "POST /v1/sql" => {
                let sql = s(body, "sql");
                assert_eq!(body["confirm"], true);
                if self.acme.protected {
                    return Ok((409, json!({"error": {"code": "sql_confirm_required", "message": "no"}}), None));
                }
                if let Some(code) = self.acme.sql_error.take() {
                    let e = json!({"t": "error", "statement": 0, "code": code, "message": "permission denied for schema sales"});
                    return json_ok(200, Value::String(format!("{e}\n{}\n", json!({"t": "done", "outcome": "error", "duration_ms": 1}))));
                }
                if sql.contains("discount_pct") {
                    assert!(r.path.contains("br-feature"));
                    self.acme.feature_col = true;
                } else {
                    assert!(self.acme.users.len() == 3 && self.acme.roles.len() == 4, "the script needs its roles and app users first");
                    self.acme.schema_done = true;
                }
                json_ok(200, Value::String(format!("{}\n", json!({"t": "done", "outcome": "ok", "duration_ms": 1}))))
            }
            "GET /v1/databases/{id}/branches/{id}/keys" => json_ok(
                200,
                Value::Array(self.acme.keys.iter().map(|k| json!({"name": k, "kind": "secret"})).collect()),
            ),
            "POST /v1/keys" => {
                let secret = self.secret("chk_sec_");
                self.acme.keys.push(s(body, "name"));
                json_ok(201, json!({"name": s(body, "name"), "key": secret, "prefix": "chk_sec_"}))
            }
            "GET /v1/databases/{id}/sql-queries" => json_ok(
                200,
                Value::Array(self.acme.queries.iter().map(|q| json!({"name": q})).collect()),
            ),
            "POST /v1/sql-queries" => {
                self.acme.queries.push(s(body, "name"));
                json_ok(201, json!({"name": s(body, "name")}))
            }
            "GET /v1/databases/{id}/branches/{id}/review" => json_ok(200, json!({"items": [], "hash": "h"})),
            "PATCH /v1/branch" => {
                assert_eq!(body["protected"], true);
                self.acme.protected = true;
                json_ok(200, json!({}))
            }
            _ => bail!("the fake has no {key}"),
        }
    }
}
