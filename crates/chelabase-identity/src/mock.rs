//! An in-memory stand-in for the identity service, for tests. It is an HTTP server on a local
//! port, so the real [`Identity`] client talks to it and a test needs no other wiring.
//!
//! It behaves like the service on what callers rely on: pools are separate (the same email can
//! live in two), a ban refuses sign-in and kills sessions, `revoke_all` leaves other users
//! alone, and codes (see [`codes`]) are single use, locked after 5 wrong attempts, bound to
//! pool, user and kind, and expire per kind. It does not model caller tokens beyond "a bearer
//! is present", nor Better Auth's hashing.

mod codes;

use crate::Identity;
use crate::codes::CodeKind;
use crate::types::Pool;
use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// The bearer the fake's [`FakeIdentity::client`] sends. Any non-empty bearer is accepted.
pub const TOKEN: &str = "test-identity-token";

const SESSION_TTL_DAYS: i64 = 7;
const MIN_PASSWORD: usize = 8;
const MAX_PASSWORD: usize = 128;

#[derive(Clone, Debug)]
pub(crate) struct FUser {
    pub id: String,
    pub email: String,
    pub password: Option<String>,
    pub name: String,
    pub verified: bool,
    pub banned: bool,
    pub created: DateTime<Utc>,
    pub updated: DateTime<Utc>,
}

#[derive(Clone, Debug)]
pub(crate) struct FSession {
    pub id: String,
    pub token: String,
    pub user_id: String,
    pub created: DateTime<Utc>,
    pub expires: DateTime<Utc>,
}

#[derive(Default)]
pub(crate) struct PoolState {
    pub users: Vec<FUser>,
    pub sessions: Vec<FSession>,
    pub codes: HashMap<(String, CodeKind), codes::CodeRow>,
}

/// A code the fake "mailed" (`delivery: "email"`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SentCode {
    pub pool: String,
    pub user_id: String,
    pub email: String,
    pub kind: CodeKind,
    pub code: String,
}

struct Fault {
    pattern: String,
    status: u16,
    error: String,
    remaining: u32,
}

pub(crate) struct Inner {
    pub pools: HashMap<String, PoolState>,
    pub calls: Vec<String>,
    pub next: u64,
    pub skew: ChronoDuration,
    pub down: bool,
    pub email_configured: bool,
    pub sent: Vec<SentCode>,
    faults: Vec<Fault>,
}

impl Inner {
    pub fn now(&self) -> DateTime<Utc> {
        Utc::now() + self.skew
    }
    pub fn id(&mut self, prefix: &str) -> String {
        self.next += 1;
        format!("{prefix}{}", self.next)
    }
    /// A user or session id: a UUID, as the service's are (deterministic here).
    pub fn uuid(&mut self) -> String {
        self.next += 1;
        uuid_of(self.next)
    }
}

fn uuid_of(n: u64) -> String {
    uuid::Uuid::from_u128(u128::from(n))
        .hyphenated()
        .to_string()
}

#[derive(Clone)]
pub struct FakeIdentity {
    pub base: String,
    inner: Arc<Mutex<Inner>>,
}

/// A refusal: status, the service's error code and a message.
pub(crate) type Refusal = (u16, &'static str, String);
pub(crate) type Reply = Result<(u16, Option<Value>), Refusal>;

pub(crate) fn refuse(status: u16, code: &'static str, message: &str) -> Refusal {
    (status, code, message.to_string())
}

fn ts(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

pub(crate) fn user_out(u: &FUser) -> Value {
    json!({
        "id": u.id, "email": u.email, "name": u.name, "email_verified": u.verified,
        "banned": u.banned, "created_at": ts(u.created), "updated_at": ts(u.updated),
    })
}

fn session_out(s: &FSession) -> Value {
    json!({ "id": s.id, "user_id": s.user_id, "expires_at": ts(s.expires), "created_at": ts(s.created) })
}

fn parse_pool(segment: &str) -> Option<Pool> {
    if segment == "members" {
        return Some(Pool::Members);
    }
    let id = uuid::Uuid::parse_str(segment.strip_prefix("app-")?).ok()?;
    (Pool::App(id).to_string() == segment).then_some(Pool::App(id))
}

pub(crate) fn check_password(p: &str) -> Result<(), Refusal> {
    let n = p.chars().count();
    if !(MIN_PASSWORD..=MAX_PASSWORD).contains(&n) {
        return Err(refuse(
            400,
            "weak_password",
            "password must be 8 to 128 characters",
        ));
    }
    Ok(())
}

fn check_email(email: &str) -> Result<String, Refusal> {
    let email = email.to_lowercase();
    let ok = email.len() <= 320
        && email
            .split_once('@')
            .is_some_and(|(l, d)| !l.is_empty() && d.contains('.'));
    if ok {
        Ok(email)
    } else {
        Err(refuse(400, "invalid_email", "invalid email"))
    }
}

pub(crate) fn str_field<'a>(body: &'a Value, key: &str) -> Result<&'a str, Refusal> {
    match body.get(key).and_then(Value::as_str) {
        Some(v) if !v.is_empty() => Ok(v),
        _ => Err(refuse(
            400,
            "invalid_request",
            &format!("{key} must be a non-empty string"),
        )),
    }
}

fn bool_field(body: &Value, key: &str) -> Result<bool, Refusal> {
    match body.get(key) {
        None => Ok(false),
        Some(Value::Bool(b)) => Ok(*b),
        Some(_) => Err(refuse(
            400,
            "invalid_request",
            &format!("{key} must be a boolean"),
        )),
    }
}

pub(crate) fn user_idx(p: &PoolState, id: &str) -> Result<usize, Refusal> {
    p.users
        .iter()
        .position(|u| u.id == id)
        .ok_or_else(|| refuse(404, "user_not_found", "no such user"))
}

/// A live session by predicate, with its user; a banned user's session is dropped, as the
/// service does when it finds one.
fn live_session(
    p: &mut PoolState,
    now: DateTime<Utc>,
    find: impl Fn(&FSession) -> bool,
) -> Option<(FSession, FUser)> {
    let s = p
        .sessions
        .iter()
        .find(|s| find(s) && s.expires > now)?
        .clone();
    let u = p.users.iter().find(|u| u.id == s.user_id)?.clone();
    if u.banned {
        p.sessions.retain(|x| x.id != s.id);
        return None;
    }
    Some((s, u))
}

fn pool_mut<'a>(i: &'a mut Inner, name: &str) -> Result<&'a mut PoolState, Refusal> {
    i.pools
        .get_mut(name)
        .ok_or_else(|| refuse(404, "pool_not_found", "pool does not exist"))
}

fn route(i: &mut Inner, method: &str, path: &str, query: &str, body: &Value) -> Reply {
    let rest = path
        .strip_prefix("/internal/v1/")
        .ok_or_else(|| refuse(404, "not_found", "no such route"))?;
    let seg: Vec<&str> = rest.split('/').collect();
    if (method, seg.as_slice()) == ("GET", &["email", "status"][..]) {
        return Ok((
            200,
            Some(json!({ "configured": i.email_configured, "reachable": i.email_configured })),
        ));
    }
    let ["pools", pool_seg, tail @ ..] = seg.as_slice() else {
        return Err(refuse(404, "not_found", "no such route"));
    };
    let pool = parse_pool(pool_seg).ok_or_else(|| {
        refuse(
            400,
            "invalid_pool",
            "pool must be `members` or `app-<uuid>`",
        )
    })?;
    let name = pool.to_string();
    match (method, tail) {
        ("POST", []) => {
            i.pools.entry(name.clone()).or_default();
            return Ok((201, Some(json!({ "pool": name }))));
        }
        ("DELETE", []) => {
            i.pools.remove(&name);
            return Ok((204, None));
        }
        _ => {}
    }
    let now = i.now();
    if let Some(reply) = codes::route(i, &name, method, tail, body) {
        return reply;
    }
    let mut next = i.next;
    let p = pool_mut(i, &name)?;
    let mut tick = || {
        next += 1;
        next
    };
    let reply = match (method, tail) {
        ("POST", ["users"]) => {
            let email = check_email(str_field(body, "email")?)?;
            let password = body.get("password").and_then(Value::as_str);
            if let Some(pw) = password {
                check_password(pw)?;
            }
            let verified = bool_field(body, "email_verified")?;
            if p.users.iter().any(|u| u.email == email) {
                return Err(refuse(409, "conflict", "a user with this email exists"));
            }
            let u = FUser {
                id: uuid_of(tick()),
                email,
                password: password.map(str::to_string),
                name: body
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                verified,
                banned: false,
                created: now,
                updated: now,
            };
            let out = user_out(&u);
            p.users.push(u);
            (201, Some(json!({ "user": out })))
        }
        ("GET", ["users"]) => {
            let q: HashMap<_, _> = query
                .split('&')
                .filter_map(|kv| kv.split_once('='))
                .collect();
            let num = |key: &str, default: usize, min: usize, max: usize| match q.get(key) {
                None => Ok(default),
                Some(v) => v
                    .parse::<usize>()
                    .ok()
                    .filter(|n| (min..=max).contains(n))
                    .ok_or_else(|| {
                        refuse(
                            400,
                            "invalid_request",
                            &format!("{key} must be an integer from {min} to {max}"),
                        )
                    }),
            };
            let limit = num("limit", 50, 1, 200)?;
            let offset = num("offset", 0, 0, 1_000_000_000)?;
            // Part of the email, ignoring case (emails are stored lowercase).
            let search = q
                .get("search")
                .map(|v| urlencoding::decode(v).map_or_else(|_| v.to_string(), |d| d.into_owned()))
                .unwrap_or_default()
                .to_lowercase();
            if search.chars().count() > 320 {
                return Err(refuse(
                    400,
                    "invalid_request",
                    "search must be at most 320 characters",
                ));
            }
            let matching: Vec<&FUser> = p
                .users
                .iter()
                .filter(|u| u.email.contains(&search))
                .collect();
            let users: Vec<Value> = matching
                .iter()
                .skip(offset)
                .take(limit)
                .map(|u| user_out(u))
                .collect();
            (
                200,
                Some(
                    json!({ "users": users, "total": matching.len(), "limit": limit, "offset": offset }),
                ),
            )
        }
        ("GET", ["users", id]) => (
            200,
            Some(json!({ "user": user_out(&p.users[user_idx(p, id)?]) })),
        ),
        ("DELETE", ["users", id]) => {
            let at = user_idx(p, id)?;
            p.users.remove(at);
            p.sessions.retain(|s| s.user_id != *id);
            p.codes.retain(|(uid, _), _| uid != id);
            (204, None)
        }
        ("PUT", ["users", id, "email"]) => {
            let email = check_email(str_field(body, "email")?)?;
            let verified = bool_field(body, "email_verified")?;
            if p.users.iter().any(|u| u.email == email && u.id != *id) {
                return Err(refuse(409, "conflict", "another user has this email"));
            }
            let at = user_idx(p, id)?;
            let u = &mut p.users[at];
            (u.email, u.verified, u.updated) = (email, verified, now);
            (200, Some(json!({ "user": user_out(u) })))
        }
        ("PUT", ["users", id, "password"]) => {
            let password = str_field(body, "password")?;
            check_password(password)?;
            let at = user_idx(p, id)?;
            p.users[at].password = Some(password.to_string());
            (200, Some(json!({ "status": true })))
        }
        ("POST", ["users", id, "verify-email"]) => {
            let at = user_idx(p, id)?;
            let u = &mut p.users[at];
            (u.verified, u.updated) = (true, now);
            (200, Some(json!({ "user": user_out(u) })))
        }
        ("POST", ["users", id, verb @ ("ban" | "unban")]) => {
            let at = user_idx(p, id)?;
            let u = &mut p.users[at];
            (u.banned, u.updated) = (*verb == "ban", now);
            let out = user_out(u);
            if *verb == "ban" {
                p.sessions.retain(|s| s.user_id != *id);
            }
            (200, Some(json!({ "user": out })))
        }
        ("DELETE", ["users", id, "sessions"]) => {
            user_idx(p, id)?;
            p.sessions.retain(|s| s.user_id != *id);
            (204, None)
        }
        ("POST", ["sessions"]) => {
            let email = str_field(body, "email")?.to_lowercase();
            let password = str_field(body, "password")?;
            let bad = || refuse(401, "bad_credentials", "invalid email or password");
            let u = p
                .users
                .iter()
                .find(|u| u.email == email)
                .ok_or_else(bad)?
                .clone();
            if u.password.as_deref() != Some(password) {
                return Err(bad());
            }
            if u.banned {
                return Err(refuse(403, "banned", "user is banned"));
            }
            let s = FSession {
                id: uuid_of(tick()),
                token: format!("tok-{}", tick()),
                user_id: u.id.clone(),
                created: now,
                expires: now + ChronoDuration::days(SESSION_TTL_DAYS),
            };
            let out = json!({ "session_id": s.id, "session_token": s.token, "user": user_out(&u) });
            p.sessions.push(s);
            (201, Some(out))
        }
        ("POST", ["sessions", "lookup"]) => {
            let token = str_field(body, "token")?;
            let (s, u) = live_session(p, now, |s| s.token == token).ok_or_else(no_session)?;
            (
                200,
                Some(json!({ "session": session_out(&s), "user": user_out(&u) })),
            )
        }
        ("GET", ["sessions", id]) => {
            let (s, u) = live_session(p, now, |s| s.id == *id).ok_or_else(no_session)?;
            (
                200,
                Some(json!({ "session": session_out(&s), "user": user_out(&u) })),
            )
        }
        ("DELETE", ["sessions", id]) => {
            let (s, _) = live_session(p, now, |s| s.id == *id).ok_or_else(no_session)?;
            p.sessions.retain(|x| x.id != s.id);
            (204, None)
        }
        _ => return Err(refuse(404, "not_found", "no such route")),
    };
    i.next = next;
    Ok(reply)
}

fn no_session() -> Refusal {
    refuse(404, "session_not_found", "no live session")
}

async fn handle(State(inner): State<Arc<Mutex<Inner>>>, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    let bytes = to_bytes(Body::new(body), 1 << 20).await.unwrap_or_default();
    let path = parts.uri.path().to_string();
    let call = format!("{} {path}", parts.method);
    let bearer = parts
        .headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .is_some_and(|t| !t.is_empty());
    let json_body: Value = if bytes.is_empty() {
        json!({})
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    let outcome = {
        let mut i = inner.lock().unwrap();
        i.calls.push(call.clone());
        if i.down {
            Err(refuse(503, "internal", "down"))
        } else if let Some(f) = i
            .faults
            .iter_mut()
            .find(|f| f.remaining > 0 && call.contains(&f.pattern))
        {
            f.remaining -= 1;
            Err((f.status, "forced", f.error.clone()))
        } else if !bearer {
            Err(refuse(
                401,
                "unauthorized",
                "missing or unknown caller token",
            ))
        } else if !json_body.is_object() {
            Err(refuse(400, "invalid_request", "body must be a JSON object"))
        } else {
            route(
                &mut i,
                parts.method.as_str(),
                &path,
                parts.uri.query().unwrap_or(""),
                &json_body,
            )
        }
    };
    match outcome {
        Ok((status, None)) => StatusCode::from_u16(status).unwrap().into_response(),
        Ok((status, Some(v))) => {
            (StatusCode::from_u16(status).unwrap(), axum::Json(v)).into_response()
        }
        // A forced fault carries the error code as its message.
        Err((status, "forced", code)) => (
            StatusCode::from_u16(status).unwrap(),
            axum::Json(json!({ "error": code, "message": "forced" })),
        )
            .into_response(),
        Err((status, code, message)) => (
            StatusCode::from_u16(status).unwrap(),
            axum::Json(json!({ "error": code, "message": message })),
        )
            .into_response(),
    }
}

impl FakeIdentity {
    pub async fn start() -> Self {
        let inner = Arc::new(Mutex::new(Inner {
            pools: HashMap::new(),
            calls: Vec::new(),
            next: 0,
            skew: ChronoDuration::zero(),
            down: false,
            email_configured: true,
            sent: Vec::new(),
            faults: Vec::new(),
        }));
        let app = Router::new().fallback(handle).with_state(inner.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self {
            base: format!("http://{addr}"),
            inner,
        }
    }

    /// A client for this fake, with [`TOKEN`].
    pub fn client(&self) -> Identity {
        Identity::new(&self.base, TOKEN)
    }

    fn with<R>(&self, f: impl FnOnce(&mut Inner) -> R) -> R {
        f(&mut self.inner.lock().unwrap())
    }

    /// Makes the pool (idempotent).
    pub fn with_pool(&self, pool: &Pool) {
        self.with(|i| {
            i.pools.entry(pool.to_string()).or_default();
        });
    }

    /// A verified user with a password, in a pool made if need be. Returns the user id.
    pub fn with_user(&self, pool: &Pool, email: &str, password: &str) -> String {
        self.add_user(pool, email, Some(password), true)
    }

    /// A user with no password yet and an unverified email (an invitee).
    pub fn with_invited_user(&self, pool: &Pool, email: &str) -> String {
        self.add_user(pool, email, None, false)
    }

    fn add_user(&self, pool: &Pool, email: &str, password: Option<&str>, verified: bool) -> String {
        self.with(|i| {
            let now = i.now();
            let id = i.uuid();
            let p = i.pools.entry(pool.to_string()).or_default();
            p.users.push(FUser {
                id: id.clone(),
                email: crate::rules::normalize_email(email),
                password: password.map(str::to_string),
                name: String::new(),
                verified,
                banned: false,
                created: now,
                updated: now,
            });
            id
        })
    }

    /// A live session for the user, without a password check. Returns `(session id, token)`.
    pub fn with_session(&self, pool: &Pool, user_id: &str) -> (String, String) {
        self.with(|i| {
            let now = i.now();
            let (id, token) = (i.uuid(), i.id("tok-"));
            let p = i.pools.get_mut(&pool.to_string()).expect("the pool exists");
            p.sessions.push(FSession {
                id: id.clone(),
                token: token.clone(),
                user_id: user_id.to_string(),
                created: now,
                expires: now + ChronoDuration::days(SESSION_TTL_DAYS),
            });
            (id, token)
        })
    }

    /// Bans the user and revokes their sessions.
    pub fn ban(&self, pool: &Pool, user_id: &str) {
        self.with(|i| {
            let p = i.pools.get_mut(&pool.to_string()).expect("the pool exists");
            p.users
                .iter_mut()
                .filter(|u| u.id == user_id)
                .for_each(|u| u.banned = true);
            p.sessions.retain(|s| s.user_id != user_id);
        });
    }

    /// Moves the fake's clock forward (session and code expiry).
    pub fn advance(&self, by: Duration) {
        self.with(|i| i.skew += ChronoDuration::from_std(by).unwrap());
    }

    /// While down, every call answers 503.
    pub fn set_down(&self, down: bool) {
        self.with(|i| i.down = down);
    }

    /// `false` makes `delivery: "email"` answer 409 `email_not_configured`.
    pub fn set_email_configured(&self, on: bool) {
        self.with(|i| i.email_configured = on);
    }

    /// The next `times` calls whose `"METHOD path"` contains `pattern` answer `status` with this
    /// error code.
    pub fn fail(&self, pattern: &str, status: u16, error: &str, times: u32) {
        self.with(|i| {
            i.faults.push(Fault {
                pattern: pattern.into(),
                status,
                error: error.into(),
                remaining: times,
            })
        });
    }

    /// `"METHOD path"` of every call, in order.
    pub fn calls(&self) -> Vec<String> {
        self.with(|i| i.calls.clone())
    }

    /// Every code the fake mailed, oldest first.
    pub fn codes_sent(&self) -> Vec<SentCode> {
        self.with(|i| i.sent.clone())
    }

    pub fn session_count(&self, pool: &Pool) -> usize {
        self.with(|i| {
            i.pools
                .get(&pool.to_string())
                .map_or(0, |p| p.sessions.len())
        })
    }
}
