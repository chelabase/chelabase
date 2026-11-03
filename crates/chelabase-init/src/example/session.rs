//! A person's session, headless: Studio's sign-in, as Studio's pages make it. Sign-in, sign-up
//! and accept-invite (`POST /auth/v1/…`, sent with Studio's `Origin` and `X-Chelabase-Studio`)
//! set auth's session cookie; `POST /auth/v1/token` with that cookie mints the member token the
//! management API takes.
//!
//! Every request goes to the gateway service with `Host` set to the API host. 429s are retried
//! after `Retry-After`; an unanswered or 502–504 request is retried only when it is safe to
//! repeat.
//!
//! Nothing here prints or returns a secret: errors name the call, the status and the error code
//! only (never a body or a URL with its query), and tokens, codes and the session cookie are
//! [`Secret`]s.

use anyhow::{Context, Result, anyhow, bail};
use chelabase_types::entry::Entry;
use reqwest::{Method, Url};
use serde_json::{Value, json};
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

/// auth's session cookie.
const SESSION_COOKIE: &str = "chelabase_session";

/// Which public host a request is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Site {
    Api,
}

/// A request body. It carries passwords, the setup code and invite codes, so its `Debug` shows
/// only the field names.
#[derive(Clone)]
pub enum Body {
    Empty,
    Json(Value),
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
        }
    }
}

/// The path without its query.
fn path_only(path: &str) -> String {
    match path.split_once('?') {
        Some((p, _)) => format!("{p}?.."),
        None => path.to_string(),
    }
}

/// `Debug` hides the query, the bearer, the session and the body's values.
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
    /// Sent as Studio sends it (`Origin` and `X-Chelabase-Studio: 1`).
    pub studio: bool,
    /// The session cookie's value.
    pub session: Option<Secret>,
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
            .field("studio", &self.studio)
            .field("session", &self.session)
            .finish()
    }
}

/// `Debug` shows the status and sizes only: the body can hold tokens.
#[derive(Clone, Default)]
pub struct Response {
    pub status: u16,
    pub retry_after: Option<u64>,
    pub body: Vec<u8>,
    /// The session cookie a sign-in set.
    pub session: Option<Secret>,
}

impl std::fmt::Debug for Response {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Response")
            .field("status", &self.status)
            .field("retry_after", &self.retry_after)
            .field("body_len", &self.body.len())
            .field("session", &self.session)
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
        };
        let mut b = self
            .http
            .request(r.method.clone(), format!("{}{}", self.base, r.path))
            .header(reqwest::header::HOST, host);
        if let Some(token) = &r.bearer {
            b = b.bearer_auth(token.expose());
        }
        if r.studio {
            b = b
                .header(reqwest::header::ORIGIN, self.entry.studio_origin())
                .header("x-chelabase-studio", "1");
        }
        if let Some(session) = &r.session {
            b = b.header(
                reqwest::header::COOKIE,
                format!("{SESSION_COOKIE}={}", session.expose()),
            );
        }
        b = match &r.body {
            Body::Empty => b,
            Body::Json(v) => b.json(v),
        };
        let answer = b.send().await.map_err(|e| anyhow!("{}", e.without_url()))?;
        let header = |name| {
            answer
                .headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
        };
        let session = session_cookie(
            answer
                .headers()
                .get_all(reqwest::header::SET_COOKIE)
                .iter()
                .filter_map(|v| v.to_str().ok()),
        );
        let retry_after = header(reqwest::header::RETRY_AFTER).and_then(|v| v.trim().parse().ok());
        let status = answer.status().as_u16();
        let body = answer.bytes().await.map(|b| b.to_vec()).unwrap_or_default();
        Ok(Response {
            status,
            retry_after,
            body,
            session,
        })
    }
}

/// The session cookie's value among `Set-Cookie` headers (a cleared one is none).
fn session_cookie<'a>(mut headers: impl Iterator<Item = &'a str>) -> Option<Secret> {
    headers.find_map(|h| {
        let pair = h.split(';').next().unwrap_or("");
        let (name, value) = pair.split_once('=')?;
        (name.trim() == SESSION_COOKIE && !value.trim().is_empty())
            .then(|| Secret(value.trim().to_string()))
    })
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

/// What an invite link carries: the user, the invite code and the link's token.
#[derive(Debug)]
pub struct InviteLink {
    pub user: String,
    pub code: Secret,
    pub token: Secret,
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
                })
            }
            _ => bail!("the invite link lacks its user, code or token"),
        }
    }
}

/// A bearer token for the management API.
#[derive(Clone, Debug)]
pub struct Token(pub Secret);

/// The headless Studio client. Built by [`Session::connect`].
pub struct Session<'a, H: Http> {
    http: &'a H,
    entry: Entry,
    /// The length of one `Retry-After` second (shorter in tests).
    second: Duration,
    /// How long a call keeps retrying.
    timeout: Duration,
}

/// The answer to a sign-in or a sign-up: a token, refused credentials, or an account that
/// waits for an admin's approval (no session).
pub enum SignIn {
    Token(Token),
    Refused,
    Pending,
}

impl<'a, H: Http> Session<'a, H> {
    /// Reads Studio's public config, retrying while the stack comes up, and checks that it is
    /// this stack's.
    pub async fn connect(
        http: &'a H,
        entry: Entry,
        timeout: Duration,
        second: Duration,
    ) -> Result<Self> {
        let s = Self {
            http,
            entry,
            second,
            timeout,
        };
        let v = s
            .json(
                Method::GET,
                "/auth/v1/studio-config",
                None,
                Body::Empty,
                "GET /auth/v1/studio-config",
            )
            .await?;
        if v.get("studioHost").and_then(Value::as_str) != Some(s.entry.studio_host()) {
            bail!("studio-config: Studio's host is not {}", s.entry.studio_host());
        }
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
        path: &str,
        bearer: Option<&Token>,
        body: Body,
        what: &'static str,
    ) -> Result<Value> {
        let idempotent = method == Method::GET;
        let r = Request {
            method,
            site: Site::Api,
            path: path.to_string(),
            bearer: bearer.map(|t| t.0.clone()),
            body,
            idempotent,
            studio: false,
            session: None,
        };
        let a = self.send(&r, what).await?;
        Ok(parse(a, what)?.0)
    }

    /// A POST to auth's sign-in pages as Studio sends it: the JSON answer and the session
    /// cookie it set, or a [`CallError`]. Never retried after no answer: it may have signed in.
    async fn studio_post(
        &self,
        path: &str,
        body: Body,
        session: Option<&Secret>,
        what: &'static str,
    ) -> Result<(Value, Option<Secret>)> {
        let r = Request {
            method: Method::POST,
            site: Site::Api,
            path: path.to_string(),
            bearer: None,
            body,
            idempotent: false,
            studio: true,
            session: session.cloned(),
        };
        parse(self.send(&r, what).await?, what)
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
        self.json(method, path, Some(token), body, what).await
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
        self.json(method, path, None, body, what).await
    }

    /// How long a call keeps retrying, and the length of one `Retry-After` second: what a
    /// step that polls a background job uses too.
    pub fn patience(&self) -> (Duration, Duration) {
        (self.timeout, self.second)
    }

    /// Runs SQL on a branch through the management API (`POST …/branches/{b}/sql`), as the
    /// database's owner or as `role`. The answer is an NDJSON stream; the rows of every
    /// statement come back as text. A statement that fails is an error naming its SQLSTATE and
    /// message (our own scripts' errors, never a secret). Never retried: a script may have run.
    pub async fn sql(
        &self,
        token: &Token,
        (database, branch): (&str, &str),
        sql: &str,
        role: Option<&str>,
        confirm: bool,
        what: &'static str,
    ) -> Result<Vec<Vec<Option<String>>>> {
        let mut body = json!({
            "run_id": uuid::Uuid::new_v4().to_string(),
            "sql": sql,
            "confirm": confirm,
        });
        if let Some(role) = role {
            body["role"] = json!(role);
        }
        let r = Request {
            method: Method::POST,
            site: Site::Api,
            path: format!("/v1/databases/{database}/branches/{branch}/sql"),
            bearer: Some(token.0.clone()),
            body: Body::Json(body),
            idempotent: false,
            studio: false,
            session: None,
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
        let mut rows = Vec::new();
        let mut outcome = None;
        for line in a.body.split(|b| *b == b'\n').filter(|l| !l.is_empty()) {
            let event: Value = serde_json::from_slice(line)
                .map_err(|_| anyhow!("{what}: an event of the answer is not JSON"))?;
            let field = |k: &str| event.get(k).and_then(Value::as_str).unwrap_or("");
            match event.get("t").and_then(Value::as_str) {
                Some("rows") => {
                    for row in event
                        .get("rows")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                    {
                        rows.push(
                            row.as_array()
                                .into_iter()
                                .flatten()
                                .map(|v| v.as_str().map(str::to_string))
                                .collect(),
                        );
                    }
                }
                Some("error") => {
                    let message: String = field("message").chars().take(300).collect();
                    bail!("{what}: SQL error {}: {message}", field("code"));
                }
                Some("done") => outcome = Some(field("outcome").to_string()),
                _ => {}
            }
        }
        match outcome.as_deref() {
            Some("ok") => Ok(rows),
            other => bail!(
                "{what}: the run ended {}",
                other.unwrap_or("without a done event")
            ),
        }
    }

    /// The member token for a session (`POST /auth/v1/token`).
    async fn token(&self, session: &Secret) -> Result<Token> {
        let what = "POST /auth/v1/token";
        let (v, _) = self
            .studio_post("/auth/v1/token", Body::Empty, Some(session), what)
            .await?;
        v.get("access_token")
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty())
            .map(|t| Token(Secret(t.to_string())))
            .ok_or_else(|| anyhow!("{what}: no access_token"))
    }

    /// After a sign-in, sign-up or accept: `studio` with the session cookie mints a token,
    /// `pending` has none.
    async fn signed_in(
        &self,
        (answer, session): (Value, Option<Secret>),
        what: &'static str,
    ) -> Result<SignIn> {
        match (answer.get("next").and_then(Value::as_str), session) {
            (Some("studio"), Some(session)) => Ok(SignIn::Token(self.token(&session).await?)),
            (Some("pending"), _) => Ok(SignIn::Pending),
            (Some("verify"), _) => {
                bail!("{what}: auth wants the email verified first (email verification is on)")
            }
            (Some("studio"), None) => bail!("{what}: signed in without a session cookie"),
            _ => bail!("{what}: an answer without a known next step"),
        }
    }

    /// Signs in with email and password.
    pub async fn sign_in(&self, email: &str, password: &str) -> Result<SignIn> {
        let what = "POST /auth/v1/sign-in";
        let body = Body::Json(json!({"email": email, "password": password}));
        match self.studio_post("/auth/v1/sign-in", body, None, what).await {
            Ok(answer) => self.signed_in(answer, what).await,
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
    ) -> Result<SignIn> {
        let what = "POST /auth/v1/sign-up";
        let mut body = json!({
            "email": email,
            "password": password,
            "givenName": name.0,
            "familyName": name.1,
        });
        if let Some(code) = setup_code {
            body["setupCode"] = json!(code.expose());
        }
        let answer = self
            .studio_post("/auth/v1/sign-up", Body::Json(body), None, what)
            .await?;
        self.signed_in(answer, what).await
    }

    /// Accepts an invite with `password` and signs in.
    pub async fn accept_invite(&self, link: &InviteLink, password: &str) -> Result<Token> {
        let what = "POST /auth/v1/accept-invite";
        let body = Body::Json(json!({
            "t": link.token.expose(),
            "user": link.user,
            "code": link.code.expose(),
            "password": password,
        }));
        let answer = self
            .studio_post("/auth/v1/accept-invite", body, None, what)
            .await?;
        match self.signed_in(answer, what).await? {
            SignIn::Token(token) => Ok(token),
            _ => bail!("{what}: the invitee was not signed in"),
        }
    }
}

/// A 2xx answer's JSON (`Null` when empty) and session cookie, or a [`CallError`].
fn parse(a: Response, what: &'static str) -> Result<(Value, Option<Secret>)> {
    if !(200..300).contains(&a.status) {
        return Err(CallError {
            what,
            status: a.status,
            code: error_code(&a.body),
        }
        .into());
    }
    if a.body.is_empty() {
        return Ok((Value::Null, a.session));
    }
    let v = serde_json::from_slice(&a.body).map_err(|_| anyhow!("{what}: the answer is not JSON"))?;
    Ok((v, a.session))
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
        let session = "SESSIONcookie42";
        let json = Body::Json(json!({"setupCode": code, "password": password}));
        let request = Request {
            method: Method::POST,
            site: Site::Api,
            path: format!("/x?code={code}"),
            bearer: Some(Secret::new(token)),
            body: json.clone(),
            idempotent: false,
            studio: true,
            session: Some(Secret::new(session)),
        };
        let response = Response {
            status: 201,
            retry_after: None,
            body: json!({"access_token": token}).to_string().into_bytes(),
            session: Some(Secret::new(session)),
        };
        let shown = format!("{json:?} {request:?} {request:#?} {response:?}");
        for secret in [code, password, token, session] {
            assert!(!shown.contains(secret), "{secret} in {shown}");
        }
        assert!(shown.contains("POST") && shown.contains("201"), "{shown}");
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
    fn the_session_cookie_is_read_from_set_cookie() {
        let headers = [
            "chelabase_login=; Max-Age=0; Path=/auth/v1",
            "chelabase_session=abc.def; HttpOnly; Secure; Path=/auth/v1",
        ];
        assert_eq!(
            session_cookie(headers.into_iter()).map(|s| s.expose().to_string()),
            Some("abc.def".into())
        );
        assert!(session_cookie(headers[..1].iter().copied()).is_none());
        assert!(session_cookie(["chelabase_session=; Max-Age=0"].into_iter()).is_none());
    }

    #[test]
    fn invite_links_parse_and_errors_hide_them() {
        let l = InviteLink::parse("http://studio.localhost/accept-invite?user=123&code=ABC&t=tok")
            .unwrap();
        assert_eq!(
            (l.user.as_str(), l.code.expose(), l.token.expose()),
            ("123", "ABC", "tok")
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
            studio: false,
            session: None,
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
}
