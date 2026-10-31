//! A person's session, headless: the Studio public client's OIDC code flow with PKCE, driven
//! through the public sign-in endpoints (sign-up, sign-in, accept-invite) instead of a browser,
//! and an authenticated management-API client for the token it ends with.
//!
//! Every request goes to the gateway service with `Host` set to the public host, so the token's
//! `iss` is the issuer the gateway verifies. No redirect is ever followed: the pending sign-in
//! (`authRequest`) and the code are read from `Location`. 429s are retried after `Retry-After`;
//! an unanswered or 502–504 request is retried only when it is safe to repeat.
//!
//! Nothing here prints or returns a secret: errors name the call, the status and the error code
//! only (never a body, a URL with its query or a `Location`), and tokens, codes and the PKCE
//! verifier are [`Secret`]s.

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use chelabase_types::entry::Entry;
use reqwest::{Method, Url};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::future::Future;
use std::time::Duration;

/// A secret value: its `Debug` hides it and it has no `Display`.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(..)")
    }
}

/// Which public host a request is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Site {
    Api,
    Auth,
}

/// A request body. It carries passwords, the setup code, invite codes and the OIDC code, so its
/// `Debug` shows only the field names.
#[derive(Clone)]
pub enum Body {
    Empty,
    Json(Value),
    Form(Vec<(&'static str, String)>),
}

impl std::fmt::Debug for Body {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => f.write_str("Empty"),
            Self::Json(Value::Object(m)) => f
                .debug_tuple("Json")
                .field(&m.keys().collect::<Vec<_>>())
                .finish(),
            Self::Json(_) => f.write_str("Json(..)"),
            Self::Form(pairs) => f
                .debug_tuple("Form")
                .field(&pairs.iter().map(|(k, _)| k).collect::<Vec<_>>())
                .finish(),
        }
    }
}

/// The path without its query (which can hold a code or `state`).
fn path_only(path: &str) -> String {
    match path.split_once('?') {
        Some((p, _)) => format!("{p}?.."),
        None => path.to_string(),
    }
}

/// `Debug` hides the query, the bearer and the body's values.
#[derive(Clone)]
pub struct Request {
    pub method: Method,
    pub site: Site,
    /// Path and query.
    pub path: String,
    pub bearer: Option<Secret>,
    pub body: Body,
    /// Safe to send again after no answer or a 502–504.
    pub idempotent: bool,
}

impl std::fmt::Debug for Request {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Request")
            .field("method", &self.method)
            .field("site", &self.site)
            .field("path", &path_only(&self.path))
            .field("bearer", &self.bearer)
            .field("body", &self.body)
            .field("idempotent", &self.idempotent)
            .finish()
    }
}

/// `Debug` shows the status and sizes only: `Location` and the body can hold codes and tokens.
#[derive(Clone, Default)]
pub struct Response {
    pub status: u16,
    pub location: Option<String>,
    pub retry_after: Option<u64>,
    pub body: Vec<u8>,
}

impl std::fmt::Debug for Response {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Response")
            .field("status", &self.status)
            .field("location", &self.location.as_ref().map(|_| ".."))
            .field("retry_after", &self.retry_after)
            .field("body_len", &self.body.len())
            .finish()
    }
}

/// One HTTP exchange; the error is a transport failure, already free of any URL.
pub trait Http: Send + Sync {
    fn send(&self, request: &Request) -> impl Future<Output = Result<Response>> + Send;
}

/// The gateway service, addressed by the public hosts.
pub struct Gateway {
    http: chelabase_http::Client,
    base: String,
    entry: Entry,
}

impl Gateway {
    pub fn new(base: &str, entry: Entry) -> Result<Self> {
        Ok(Self {
            http: chelabase_http::client_builder()
                .timeout(Duration::from_secs(60))
                .build()
                .context("building the HTTP client")?,
            base: base.trim_end_matches('/').to_string(),
            entry,
        })
    }
}

impl Http for Gateway {
    async fn send(&self, r: &Request) -> Result<Response> {
        let host = match r.site {
            Site::Api => self.entry.api_host(),
            Site::Auth => self.entry.auth_host(),
        };
        let mut b = self
            .http
            .request(r.method.clone(), format!("{}{}", self.base, r.path))
            .header(reqwest::header::HOST, host);
        if let Some(token) = &r.bearer {
            b = b.bearer_auth(token.expose());
        }
        b = match &r.body {
            Body::Empty => b,
            Body::Json(v) => b.json(v),
            Body::Form(pairs) => b.form(pairs),
        };
        let answer = b.send().await.map_err(|e| anyhow!("{}", e.without_url()))?;
        let header = |name| {
            answer
                .headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
        };
        let location = header(reqwest::header::LOCATION);
        let retry_after = header(reqwest::header::RETRY_AFTER).and_then(|v| v.trim().parse().ok());
        let status = answer.status().as_u16();
        let body = answer.bytes().await.map(|b| b.to_vec()).unwrap_or_default();
        Ok(Response {
            status,
            location,
            retry_after,
            body,
        })
    }
}

/// A refused call: the call's name, the status and the error code (never the body).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallError {
    pub what: &'static str,
    pub status: u16,
    pub code: Option<String>,
}

impl std::fmt::Display for CallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.what, self.status)?;
        match &self.code {
            Some(code) => write!(f, " {code}"),
            None => Ok(()),
        }
    }
}

impl std::error::Error for CallError {}

/// The [`CallError`] in `err`'s chain, if any.
pub fn call_error(err: &anyhow::Error) -> Option<&CallError> {
    err.chain().find_map(|e| e.downcast_ref::<CallError>())
}

/// An error code is a snake_case identifier (`[a-z0-9_]`, at most 64 bytes); anything else,
/// which could be an opaque token, is dropped.
fn error_code(body: &[u8]) -> Option<String> {
    let v: Value = serde_json::from_slice(body).ok()?;
    let code = v
        .pointer("/error/code")
        .or_else(|| v.get("error"))?
        .as_str()?;
    let ok = code.len() <= 64
        && !code.is_empty()
        && code
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
    ok.then(|| code.to_string())
}

/// A PKCE pair: a 43-character verifier and its S256 challenge.
pub struct Pkce {
    pub verifier: Secret,
    pub challenge: String,
}

impl Pkce {
    pub fn new() -> Self {
        Self::from_verifier(crate::random::text32())
    }

    pub fn from_verifier(verifier: String) -> Self {
        let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(Sha256::digest(verifier.as_bytes()));
        Self {
            verifier: Secret(verifier),
            challenge,
        }
    }
}

impl Default for Pkce {
    fn default() -> Self {
        Self::new()
    }
}

/// Studio's callback, byte for byte as the bootstrap registers it: scheme and Studio host,
/// port included.
pub fn redirect_uri(entry: &Entry) -> String {
    format!("{}/callback", entry.studio_origin())
}

/// What an invite link carries: the user, the invite code and the link's token.
#[derive(Debug)]
pub struct InviteLink {
    pub user: String,
    pub code: Secret,
    pub token: Secret,
    pub reset: bool,
}

impl InviteLink {
    /// Parses `…/accept-invite?user=…&code=…&t=…`; the error never shows the link.
    pub fn parse(url: &str) -> Result<Self> {
        let url = Url::parse(url).map_err(|_| anyhow!("the invite link is not a URL"))?;
        let get = |k: &str| {
            url.query_pairs()
                .find(|(n, _)| n == k)
                .map(|(_, v)| v.into_owned())
        };
        if url.path() != "/accept-invite" {
            bail!("the invite link is not an /accept-invite link");
        }
        match (get("user"), get("code"), get("t")) {
            (Some(user), Some(code), Some(t))
                if !user.is_empty() && !code.is_empty() && !t.is_empty() =>
            {
                Ok(Self {
                    user,
                    code: Secret(code),
                    token: Secret(t),
                    reset: get("reset").as_deref() == Some("1"),
                })
            }
            _ => bail!("the invite link lacks its user, code or token"),
        }
    }
}

/// A bearer token for the management API.
#[derive(Clone, Debug)]
pub struct Token(pub Secret);

/// Studio's public sign-in values (`GET /auth/v1/studio-config`).
#[derive(Debug, Clone)]
struct StudioConfig {
    client_id: String,
    project_id: String,
    org_id: String,
}

/// A pending sign-in: Zitadel's `authRequest` id, our `state` and PKCE verifier.
struct Pending {
    id: Secret,
    state: String,
    pkce: Pkce,
}

/// The headless Studio client. Built by [`Session::connect`].
pub struct Session<'a, H: Http> {
    http: &'a H,
    entry: Entry,
    studio: StudioConfig,
    /// The length of one `Retry-After` second (shorter in tests).
    second: Duration,
    /// How long a call keeps retrying.
    timeout: Duration,
}

/// The answer to sign-in: a token, or refused credentials.
pub enum SignIn {
    Token(Token),
    Refused,
}

impl<'a, H: Http> Session<'a, H> {
    /// Reads Studio's public config, retrying while the stack comes up.
    pub async fn connect(
        http: &'a H,
        entry: Entry,
        timeout: Duration,
        second: Duration,
    ) -> Result<Self> {
        let mut s = Self {
            http,
            entry,
            studio: StudioConfig {
                client_id: String::new(),
                project_id: String::new(),
                org_id: String::new(),
            },
            second,
            timeout,
        };
        let v = s
            .json(
                Method::GET,
                Site::Api,
                "/auth/v1/studio-config",
                None,
                Body::Empty,
                "GET /auth/v1/studio-config",
            )
            .await?;
        let field = |k: &str| {
            v.get(k)
                .and_then(Value::as_str)
                .filter(|s| {
                    !s.is_empty()
                        && s.bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
                })
                .map(str::to_string)
                .ok_or_else(|| anyhow!("studio-config: no usable {k}"))
        };
        if v.get("issuer").and_then(Value::as_str) != Some(s.entry.issuer().as_str()) {
            bail!("studio-config: the issuer is not {}", s.entry.issuer());
        }
        s.studio = StudioConfig {
            client_id: field("clientId")?,
            project_id: field("projectId")?,
            org_id: field("orgId")?,
        };
        Ok(s)
    }

    /// Sends `r`, retrying 429s (after `Retry-After`) and, when `r` is idempotent, no answer or
    /// a 502–504, until the timeout.
    pub async fn send(&self, r: &Request, what: &'static str) -> Result<Response> {
        let deadline = tokio::time::Instant::now() + self.timeout;
        let mut backoff = 1u64;
        loop {
            let (wait, why) = match self.http.send(r).await {
                Ok(a) if a.status == 429 => (a.retry_after.unwrap_or(backoff), "429".to_string()),
                Ok(a) if r.idempotent && matches!(a.status, 502..=504) => {
                    (backoff, a.status.to_string())
                }
                Ok(a) => return Ok(a),
                Err(e) if r.idempotent => (backoff, format!("no answer ({e})")),
                Err(e) => return Err(e.context(what)),
            };
            let pause = self.second * u32::try_from(wait).unwrap_or(u32::MAX);
            if tokio::time::Instant::now() + pause > deadline {
                bail!("{what}: still {why} after {:?}", self.timeout);
            }
            tokio::time::sleep(pause).await;
            backoff = (backoff * 2).min(8);
        }
    }

    async fn json(
        &self,
        method: Method,
        site: Site,
        path: &str,
        bearer: Option<&Token>,
        body: Body,
        what: &'static str,
    ) -> Result<Value> {
        let idempotent = method == Method::GET;
        let r = Request {
            method,
            site,
            path: path.to_string(),
            bearer: bearer.map(|t| t.0.clone()),
            body,
            idempotent,
        };
        let a = self.send(&r, what).await?;
        if !(200..300).contains(&a.status) {
            return Err(CallError {
                what,
                status: a.status,
                code: error_code(&a.body),
            }
            .into());
        }
        if a.body.is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_slice(&a.body).map_err(|_| anyhow!("{what}: the answer is not JSON"))
    }

    /// A management-API call as `token`: the JSON answer, or a [`CallError`].
    pub async fn api(
        &self,
        method: Method,
        path: &str,
        token: &Token,
        body: Option<Value>,
        what: &'static str,
    ) -> Result<Value> {
        let body = body.map_or(Body::Empty, Body::Json);
        self.json(method, Site::Api, path, Some(token), body, what)
            .await
    }

    /// An unauthenticated call to the API host (`/auth/v1/…`).
    pub async fn public(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
        what: &'static str,
    ) -> Result<Value> {
        let body = body.map_or(Body::Empty, Body::Json);
        self.json(method, Site::Api, path, None, body, what).await
    }

    /// Starts a sign-in: `GET /oauth/v2/authorize`, never followed; the pending sign-in's id
    /// from `Location`.
    async fn begin(&self) -> Result<Pending> {
        let pkce = Pkce::new();
        let state = crate::random::text32();
        let scope = [
            "openid".to_string(),
            "profile".into(),
            "email".into(),
            "urn:zitadel:iam:user:resourceowner".into(),
            format!(
                "urn:zitadel:iam:org:project:id:{}:aud",
                self.studio.project_id
            ),
            format!("urn:zitadel:iam:org:id:{}", self.studio.org_id),
        ]
        .join(" ");
        let mut url = Url::parse("http://x/oauth/v2/authorize").expect("a valid URL");
        url.query_pairs_mut()
            .append_pair("client_id", &self.studio.client_id)
            .append_pair("redirect_uri", &redirect_uri(&self.entry))
            .append_pair("response_type", "code")
            .append_pair("scope", &scope)
            .append_pair("code_challenge", &pkce.challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("state", &state);
        let what = "GET /oauth/v2/authorize";
        let r = Request {
            method: Method::GET,
            site: Site::Auth,
            path: format!("/oauth/v2/authorize?{}", url.query().unwrap_or("")),
            bearer: None,
            body: Body::Empty,
            idempotent: true,
        };
        let a = self.send(&r, what).await?;
        let id = a
            .location
            .as_deref()
            .filter(|_| (300..400).contains(&a.status))
            .and_then(|l| query_value(l, "authRequest"))
            .filter(|id| id.starts_with("V2_"))
            .ok_or_else(|| anyhow!("{what}: {} without a pending sign-in in Location", a.status))?;
        Ok(Pending {
            id: Secret(id),
            state,
            pkce,
        })
    }

    /// Finishes a sign-in from the `done` answer's `redirect`: the code from Studio's callback
    /// URL (following Zitadel's own callback once, by hand, when that is what it is), `state`
    /// checked, then the token.
    async fn finish(&self, pending: &Pending, answer: &Value) -> Result<Token> {
        let redirect = answer
            .get("redirect")
            .and_then(Value::as_str)
            .filter(|_| answer.get("next").and_then(Value::as_str) == Some("done"))
            .ok_or_else(|| anyhow!("the sign-in did not finish (no `done` with a redirect)"))?;
        let callback = redirect_uri(&self.entry);
        let location = if is_callback(redirect, &callback) {
            redirect.to_string()
        } else {
            let issuer = self.entry.issuer();
            let path = redirect
                .strip_prefix(issuer.as_str())
                .filter(|p| p.starts_with('/'))
                .ok_or_else(|| {
                    anyhow!("the sign-in redirects to neither Studio's callback nor the issuer")
                })?;
            let what = "GET Zitadel's callback";
            let r = Request {
                method: Method::GET,
                site: Site::Auth,
                path: path.to_string(),
                bearer: None,
                body: Body::Empty,
                idempotent: false,
            };
            let a = self.send(&r, what).await?;
            a.location
                .filter(|l| (300..400).contains(&a.status) && is_callback(l, &callback))
                .ok_or_else(|| {
                    anyhow!("{what}: {} without Studio's callback in Location", a.status)
                })?
        };
        let code = callback_code(&location, &pending.state)?;
        let what = "POST /oauth/v2/token";
        let r = Request {
            method: Method::POST,
            site: Site::Auth,
            path: "/oauth/v2/token".to_string(),
            bearer: None,
            body: Body::Form(vec![
                ("grant_type", "authorization_code".to_string()),
                ("code", code.expose().to_string()),
                ("redirect_uri", callback),
                ("client_id", self.studio.client_id.clone()),
                ("code_verifier", pending.pkce.verifier.expose().to_string()),
            ]),
            idempotent: false,
        };
        let a = self.send(&r, what).await?;
        if a.status != 200 {
            return Err(CallError {
                what,
                status: a.status,
                code: error_code(&a.body),
            }
            .into());
        }
        let v: Value = serde_json::from_slice(&a.body)
            .map_err(|_| anyhow!("{what}: the answer is not JSON"))?;
        v.get("access_token")
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty())
            .map(|t| Token(Secret(t.to_string())))
            .ok_or_else(|| anyhow!("{what}: no access_token"))
    }

    /// Signs in with email and password.
    pub async fn sign_in(&self, email: &str, password: &str) -> Result<SignIn> {
        let pending = self.begin().await?;
        let body =
            json!({"authRequest": pending.id.expose(), "email": email, "password": password});
        match self
            .public(
                Method::POST,
                "/auth/v1/sign-in",
                Some(body),
                "POST /auth/v1/sign-in",
            )
            .await
        {
            Ok(answer) => Ok(SignIn::Token(self.finish(&pending, &answer).await?)),
            Err(e) if call_error(&e).is_some_and(|c| c.status == 401) => Ok(SignIn::Refused),
            Err(e) => Err(e),
        }
    }

    /// Creates an account (with the setup code on an unclaimed instance) and signs in.
    pub async fn sign_up(
        &self,
        email: &str,
        password: &str,
        name: (&str, &str),
        setup_code: Option<&Secret>,
    ) -> Result<Token> {
        let pending = self.begin().await?;
        let mut body = json!({
            "authRequest": pending.id.expose(),
            "email": email,
            "password": password,
            "givenName": name.0,
            "familyName": name.1,
        });
        if let Some(code) = setup_code {
            body["setupCode"] = json!(code.expose());
        }
        let answer = self
            .public(
                Method::POST,
                "/auth/v1/sign-up",
                Some(body),
                "POST /auth/v1/sign-up",
            )
            .await?;
        self.finish(&pending, &answer).await
    }

    /// Accepts an invite with `password` and signs in.
    pub async fn accept_invite(&self, link: &InviteLink, password: &str) -> Result<Token> {
        let pending = self.begin().await?;
        let body = json!({
            "user": link.user,
            "code": link.code.expose(),
            "reset": link.reset,
            "password": password,
            "authRequest": pending.id.expose(),
        });
        let answer = self
            .public(
                Method::POST,
                "/auth/v1/accept-invite",
                Some(body),
                "POST /auth/v1/accept-invite",
            )
            .await?;
        self.finish(&pending, &answer).await
    }
}

/// `url` is `callback` with a query.
fn is_callback(url: &str, callback: &str) -> bool {
    url.strip_prefix(callback)
        .is_some_and(|rest| rest.starts_with('?'))
}

/// One query parameter of an absolute URL.
fn query_value(url: &str, key: &str) -> Option<String> {
    let parsed = Url::parse(url).ok()?;
    parsed
        .query_pairs()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.into_owned())
}

/// The code from Studio's callback URL, once `state` matches. An OAuth `error` is named (it is
/// a fixed word), never the URL.
pub fn callback_code(location: &str, state: &str) -> Result<Secret> {
    if query_value(location, "state").as_deref() != Some(state) {
        bail!("the callback's state does not match");
    }
    match query_value(location, "code").filter(|c| !c.is_empty()) {
        Some(code) => Ok(Secret(code)),
        None => {
            let error = query_value(location, "error")
                .filter(|e| {
                    e.len() <= 64 && e.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
                })
                .unwrap_or_else(|| "none".to_string());
            bail!("the callback has no code (error: {error})")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn entry(studio: &str) -> Entry {
        Entry::from_lookup(|k| {
            match k {
                "CHELABASE_PUBLIC_SCHEME" => Some("http"),
                "CHELABASE_STUDIO_HOST" => Some(studio),
                "CHELABASE_API_HOST" => Some("api.chelabase.localhost:8000"),
                "CHELABASE_AUTH_HOST" => Some("auth.chelabase.localhost:8000"),
                _ => None,
            }
            .map(str::to_string)
        })
        .unwrap()
    }

    #[test]
    fn debug_of_a_request_or_answer_shows_no_secret() {
        let code = "SETU-PC0D-E234-5678-9ABC";
        let password = "demo-Password-1234";
        let token = "eyJsecretTOKEN123";
        let json = Body::Json(json!({"setupCode": code, "password": password}));
        let form = Body::Form(vec![("code", token.to_string())]);
        let request = Request {
            method: Method::POST,
            site: Site::Auth,
            path: format!("/x?code={code}"),
            bearer: Some(Secret::new(token)),
            body: json.clone(),
            idempotent: false,
        };
        let response = Response {
            status: 302,
            location: Some(format!("http://studio/callback?code={code}")),
            retry_after: None,
            body: json!({"access_token": token}).to_string().into_bytes(),
        };
        let shown = format!("{json:?} {form:?} {request:?} {request:#?} {response:?}");
        for secret in [code, password, token] {
            assert!(!shown.contains(secret), "{secret} in {shown}");
        }
        assert!(shown.contains("POST") && shown.contains("302"), "{shown}");
    }

    #[test]
    fn only_snake_case_error_codes_pass_through() {
        let code = |v: Value| error_code(v.to_string().as_bytes());
        assert_eq!(
            code(json!({"error": {"code": "email_taken"}})).as_deref(),
            Some("email_taken")
        );
        assert_eq!(
            code(json!({"error": "invalid_grant"})).as_deref(),
            Some("invalid_grant")
        );
        assert_eq!(code(json!({"error": "a1_b2"})).as_deref(), Some("a1_b2"));
        for opaque in [
            "SETUPC0DE1234",
            "Invalid",
            "abcDEF",
            "a-b",
            "",
            &"a".repeat(65),
        ] {
            assert_eq!(code(json!({"error": opaque})), None, "{opaque}");
        }
        assert_eq!(code(json!({"error": "a".repeat(64)})), Some("a".repeat(64)));
    }

    #[test]
    fn pkce_challenge_is_s256_of_the_verifier() {
        // Computed independently: base64url(sha256(verifier)), unpadded.
        let p = Pkce::from_verifier("dBjftJeZ4CVP-mJ92IqXLKbS0Sb2fVAuSxfNoq7f0sQ".into());
        assert_eq!(p.challenge, "06Uhd8l1DHcTHOVafQ7MZC33DdSv1VsnMgmH3YpqHs4");
        let fresh = Pkce::new();
        assert_eq!(fresh.verifier.expose().len(), 43);
        assert_ne!(fresh.verifier.expose(), Pkce::new().verifier.expose());
        assert_eq!(format!("{:?}", fresh.verifier), "Secret(..)");
    }

    #[test]
    fn redirect_uri_keeps_a_non_default_port() {
        assert_eq!(
            redirect_uri(&entry("studio.chelabase.localhost:9443")),
            "http://studio.chelabase.localhost:9443/callback"
        );
        assert_eq!(
            redirect_uri(&entry("studio.chelabase.localhost")),
            "http://studio.chelabase.localhost/callback"
        );
    }

    #[test]
    fn callback_needs_matching_state_and_a_code() {
        let ok = callback_code("http://s/callback?code=c0de&state=s1", "s1").unwrap();
        assert_eq!(ok.expose(), "c0de");
        let mismatch = callback_code("http://s/callback?code=c0de&state=evil", "s1").unwrap_err();
        assert!(mismatch.to_string().contains("state"), "{mismatch}");
        assert!(!mismatch.to_string().contains("c0de"));
        let none =
            callback_code("http://s/callback?error=access_denied&state=s1", "s1").unwrap_err();
        assert_eq!(
            none.to_string(),
            "the callback has no code (error: access_denied)"
        );
        assert!(callback_code("http://s/callback?state=s1", "s1").is_err());
    }

    #[test]
    fn invite_links_parse_and_errors_hide_them() {
        let l = InviteLink::parse("http://studio.localhost/accept-invite?user=123&code=ABC&t=tok")
            .unwrap();
        assert_eq!(
            (l.user.as_str(), l.code.expose(), l.token.expose(), l.reset),
            ("123", "ABC", "tok", false)
        );
        let e = InviteLink::parse("http://studio.localhost/accept-invite?user=123&code=SECRETCODE")
            .unwrap_err();
        assert!(!e.to_string().contains("SECRETCODE"));
    }

    /// Answers each request with the next canned response.
    struct Script(Mutex<Vec<Result<Response>>>, Mutex<usize>);

    impl Http for Script {
        async fn send(&self, _: &Request) -> Result<Response> {
            *self.1.lock().unwrap() += 1;
            self.0.lock().unwrap().remove(0)
        }
    }

    fn answer(status: u16, retry_after: Option<u64>) -> Result<Response> {
        Ok(Response {
            status,
            retry_after,
            ..Response::default()
        })
    }

    fn session(http: &Script) -> Session<'_, Script> {
        Session {
            http,
            entry: entry("studio.chelabase.localhost:8000"),
            studio: StudioConfig {
                client_id: "c".into(),
                project_id: "p".into(),
                org_id: "o".into(),
            },
            second: Duration::from_millis(1),
            timeout: Duration::from_secs(5),
        }
    }

    fn post() -> Request {
        Request {
            method: Method::POST,
            site: Site::Api,
            path: "/x".into(),
            bearer: None,
            body: Body::Empty,
            idempotent: false,
        }
    }

    #[tokio::test]
    async fn a_429_is_retried_after_retry_after() {
        let http = Script(
            Mutex::new(vec![
                answer(429, Some(2)),
                answer(429, None),
                answer(201, None),
            ]),
            Mutex::new(0),
        );
        let a = session(&http).send(&post(), "POST /x").await.unwrap();
        assert_eq!((a.status, *http.1.lock().unwrap()), (201, 3));
    }

    #[tokio::test]
    async fn a_503_or_no_answer_is_retried_only_when_idempotent() {
        let http = Script(Mutex::new(vec![answer(503, None)]), Mutex::new(0));
        assert_eq!(
            session(&http)
                .send(&post(), "POST /x")
                .await
                .unwrap()
                .status,
            503
        );
        let http = Script(Mutex::new(vec![Err(anyhow!("refused"))]), Mutex::new(0));
        assert!(session(&http).send(&post(), "POST /x").await.is_err());
        let http = Script(
            Mutex::new(vec![
                Err(anyhow!("refused")),
                answer(503, None),
                answer(200, None),
            ]),
            Mutex::new(0),
        );
        let get = Request {
            method: Method::GET,
            idempotent: true,
            ..post()
        };
        assert_eq!(
            session(&http).send(&get, "GET /x").await.unwrap().status,
            200
        );
    }

    #[tokio::test]
    async fn a_location_without_a_pending_sign_in_fails() {
        let to = |l: &str| {
            Ok(Response {
                status: 302,
                location: Some(l.into()),
                ..Response::default()
            })
        };
        let http = Script(
            Mutex::new(vec![to(
                "http://studio.chelabase.localhost:8000/callback?error=invalid_scope&state=x",
            )]),
            Mutex::new(0),
        );
        let e = session(&http).begin().await.err().unwrap();
        assert!(e.to_string().contains("without a pending sign-in"), "{e}");
        let http = Script(
            Mutex::new(vec![to(
                "http://studio.chelabase.localhost:8000/login?authRequest=V2_42",
            )]),
            Mutex::new(0),
        );
        assert_eq!(session(&http).begin().await.unwrap().id.expose(), "V2_42");
    }
}
