//! The socket tests' harness: realtime in-process (`app::build_with`) on ephemeral loopback
//! ports with the in-memory store and bus, the fake change source, the fake OIDC verifier, a
//! gateway identity signer of the test's own and a fake orchestrator serving T6's
//! `/internal/realtime/{authorize,projects}` answers. Several instances share the store, the bus
//! and the orchestrator, as instances share Valkey, NATS and the orchestrator.

// Test code: plain clients against local test servers (clippy.toml allows tests only).
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]
#![allow(dead_code)]

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::Router;
use axum::body::Bytes;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::post;
use chelabase_auth::internal::{
    AuthMethod, DataIdentity, IDENTITY_HEADER, Identity, InternalSigner, InternalVerifier,
    generate_keypair_pem,
};
use chelabase_types::data_key::DataKeyKind;
use futures_util::{SinkExt, StreamExt};
use realtime::app::{Deps, Realtime, build_with};
use realtime::auth::MemberToken;
use realtime::auth::fake::FakeVerifier;
use realtime::bus::MemoryBus;
use realtime::config::Config;
use realtime::protocol::SUBPROTOCOL;
use realtime::source::fake::FakeChangeSource;
use realtime::store::MemoryStore;
use realtime::ticket::{TicketIdentity, TicketKey, Tickets};
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::{self, Message};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use uuid::Uuid;

pub const TENANT: Uuid = Uuid::from_u128(0x0000_0000_0000_4000_8000_0000_0000_0001);
pub const PROJECT: Uuid = Uuid::from_u128(0x0000_0000_0000_4000_8000_0000_0000_0002);
pub const DB: Uuid = Uuid::from_u128(0x1111_1111_1111_4111_8111_1111_1111_1111);
pub const BR: Uuid = Uuid::from_u128(0x2222_2222_2222_4222_8222_2222_2222_2222);
pub const BEARER: &str = "rt-internal-test-token";

pub fn loopback() -> SocketAddr {
    SocketAddr::from((Ipv4Addr::LOCALHOST, 0))
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// A member token for `sub` in [`TENANT`], expiring in `exp_in` seconds.
pub fn token(sub: &str, exp_in: i64) -> String {
    token_in(sub, TENANT, exp_in)
}

pub fn token_in(sub: &str, tenant: Uuid, exp_in: i64) -> String {
    FakeVerifier::token(&MemberToken {
        sub: sub.into(),
        tenant_id: Some(tenant),
        exp: (now() as i64 + exp_in) as u64,
        name: Some(format!("Name {sub}")),
        email: Some(format!("{sub}@example.com")),
    })
}

/// The sub a fake token names.
pub fn sub_of(token: &str) -> Option<String> {
    FakeVerifier::decode(token).map(|t| t.sub)
}

pub fn allowed(sub: &str) -> (u16, Value) {
    (
        200,
        json!({"allowed": true, "tenant_id": TENANT,
        "member": {"sub": sub, "name": format!("Name {sub}"), "email": format!("{sub}@example.com")}}),
    )
}

pub fn refused(reason: &str) -> (u16, Value) {
    (200, json!({"allowed": false, "reason": reason}))
}

pub fn projects_of(list: &[(Uuid, &[Uuid])]) -> (u16, Value) {
    let projects: Vec<Value> = list
        .iter()
        .map(|(p, dbs)| json!({"project_id": p, "database_ids": dbs}))
        .collect();
    (200, json!({"tenant_id": TENANT, "projects": projects}))
}

type Rule = Arc<dyn Fn(&Value) -> (u16, Value) + Send + Sync>;

struct OrchState {
    authorize: Mutex<Rule>,
    projects: Mutex<Rule>,
    /// Final round A (T8-1): `/internal/realtime/member`, asked before a Studio ticket.
    member: Mutex<Rule>,
    authorize_calls: Mutex<Vec<Value>>,
    projects_calls: Mutex<Vec<Value>>,
    member_calls: Mutex<Vec<Value>>,
    /// Each `/authorize` answers after this long (a slow orchestrator).
    authorize_delay: Mutex<Duration>,
}

/// T6's `/internal/realtime/{authorize,projects}` with programmable answers. By default every
/// fake token is allowed (as its own sub) and sees [`PROJECT`] with [`DB`].
#[derive(Clone)]
pub struct FakeOrchestrator {
    pub url: String,
    state: Arc<OrchState>,
}

impl FakeOrchestrator {
    pub async fn start() -> Self {
        let default_authorize: Rule =
            Arc::new(
                |body: &Value| match body["token"].as_str().and_then(sub_of) {
                    Some(sub) => allowed(&sub),
                    None => refused("forbidden"),
                },
            );
        let default_projects: Rule = Arc::new(|_: &Value| projects_of(&[(PROJECT, &[DB])]));
        let default_member: Rule = Arc::new(|_: &Value| (200, json!({"allowed": true})));
        let state = Arc::new(OrchState {
            authorize: Mutex::new(default_authorize),
            projects: Mutex::new(default_projects),
            member: Mutex::new(default_member),
            authorize_calls: Mutex::new(vec![]),
            projects_calls: Mutex::new(vec![]),
            member_calls: Mutex::new(vec![]),
            authorize_delay: Mutex::new(Duration::ZERO),
        });
        let route = |which: &'static str, state: Arc<OrchState>| {
            move |headers: HeaderMap, body: Bytes| {
                let state = state.clone();
                async move {
                    let bearer = headers.get("authorization").and_then(|v| v.to_str().ok());
                    if bearer != Some(&format!("Bearer {BEARER}")) {
                        return StatusCode::UNAUTHORIZED.into_response();
                    }
                    let Ok(body) = serde_json::from_slice::<Value>(&body) else {
                        return StatusCode::BAD_REQUEST.into_response();
                    };
                    let rule = if which == "authorize" {
                        state.authorize_calls.lock().unwrap().push(body.clone());
                        let delay = *state.authorize_delay.lock().unwrap();
                        if !delay.is_zero() {
                            tokio::time::sleep(delay).await;
                        }
                        state.authorize.lock().unwrap().clone()
                    } else if which == "member" {
                        state.member_calls.lock().unwrap().push(body.clone());
                        state.member.lock().unwrap().clone()
                    } else {
                        state.projects_calls.lock().unwrap().push(body.clone());
                        state.projects.lock().unwrap().clone()
                    };
                    let (status, answer) = rule(&body);
                    (StatusCode::from_u16(status).unwrap(), axum::Json(answer)).into_response()
                }
            }
        };
        let app = Router::new()
            .route(
                "/internal/realtime/authorize",
                post(route("authorize", state.clone())),
            )
            .route(
                "/internal/realtime/projects",
                post(route("projects", state.clone())),
            )
            .route(
                "/internal/realtime/member",
                post(route("member", state.clone())),
            );
        let listener = tokio::net::TcpListener::bind(loopback()).await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        FakeOrchestrator {
            url: format!("http://{addr}"),
            state,
        }
    }
    pub fn on_authorize(&self, f: impl Fn(&Value) -> (u16, Value) + Send + Sync + 'static) {
        *self.state.authorize.lock().unwrap() = Arc::new(f);
    }
    /// From now on every `/authorize` answers after `d`.
    pub fn delay_authorize(&self, d: Duration) {
        *self.state.authorize_delay.lock().unwrap() = d;
    }
    pub fn on_projects(&self, f: impl Fn(&Value) -> (u16, Value) + Send + Sync + 'static) {
        *self.state.projects.lock().unwrap() = Arc::new(f);
    }
    /// Final round A (T8-1): how `/internal/realtime/member` answers from now on.
    pub fn on_member(&self, f: impl Fn(&Value) -> (u16, Value) + Send + Sync + 'static) {
        *self.state.member.lock().unwrap() = Arc::new(f);
    }
    pub fn member_calls(&self) -> Vec<Value> {
        self.state.member_calls.lock().unwrap().clone()
    }
    pub fn authorize_calls(&self) -> Vec<Value> {
        self.state.authorize_calls.lock().unwrap().clone()
    }
    pub fn projects_calls(&self) -> Vec<Value> {
        self.state.projects_calls.lock().unwrap().clone()
    }
    /// `/authorize` calls whose token names `sub`.
    pub fn authorize_calls_for(&self, sub: &str) -> usize {
        self.authorize_calls()
            .iter()
            .filter(|b| b["token"].as_str().and_then(sub_of).as_deref() == Some(sub))
            .count()
    }
}

pub struct Harness {
    pub orch: FakeOrchestrator,
    pub source: Arc<FakeChangeSource>,
    pub store: Arc<MemoryStore>,
    pub bus: Arc<MemoryBus>,
    pub verifier: Arc<FakeVerifier>,
    pub key: TicketKey,
    pub config: Config,
    signer: InternalSigner,
    verify_pem: String,
    rt: Option<Realtime>,
    /// #119: the instances' token verifier instead of [`Self::verifier`] (the real one).
    token_verifier: Option<Arc<dyn realtime::auth::TokenVerifier>>,
}

impl Harness {
    pub async fn start() -> Self {
        Self::start_with(|_| {}).await
    }

    /// One instance; `tune` adjusts its config (timings, limits) before it starts.
    pub async fn start_with(tune: impl FnOnce(&mut Config)) -> Self {
        Self::start_full(tune, None).await
    }

    /// #119: one instance whose `auth` frames are checked by `verifier` (the production one).
    pub async fn start_verifying(verifier: Arc<dyn realtime::auth::TokenVerifier>) -> Self {
        Self::start_full(|_| {}, Some(verifier)).await
    }

    async fn start_full(
        tune: impl FnOnce(&mut Config),
        token_verifier: Option<Arc<dyn realtime::auth::TokenVerifier>>,
    ) -> Self {
        let (signing_pem, verify_pem) = generate_keypair_pem();
        let key = TicketKey::generate();
        let orch = FakeOrchestrator::start().await;
        let mut config = Config::with_defaults(key.clone(), BEARER);
        config.addr = loopback();
        config.internal_addr = loopback();
        config.orchestrator_url = orch.url.clone();
        config.internal_url = orch.url.clone();
        tune(&mut config);
        let mut h = Harness {
            orch,
            source: FakeChangeSource::new(),
            store: MemoryStore::new(),
            bus: MemoryBus::new(),
            verifier: FakeVerifier::new(),
            key,
            config,
            signer: InternalSigner::from_pem(signing_pem.as_bytes()).unwrap(),
            verify_pem,
            rt: None,
            token_verifier,
        };
        let rt = h.instance(|_| {}).await;
        h.rt = Some(rt);
        h
    }

    /// Another instance sharing the store, the bus, the source, the verifier and the
    /// orchestrator, with its own instance id.
    pub async fn instance(&self, tune: impl FnOnce(&mut Config)) -> Realtime {
        let mut config = self.config.clone();
        config.instance_id = Uuid::new_v4().to_string();
        tune(&mut config);
        let deps = Deps {
            identity: Arc::new(InternalVerifier::from_pem(self.verify_pem.as_bytes()).unwrap()),
            verifier: match &self.token_verifier {
                Some(v) => v.clone(),
                None => self.verifier.clone(),
            },
            tickets: self.store.clone(),
            counters: self.store.clone(),
            bus: self.bus.clone(),
            source: self.source.clone(),
        };
        build_with(config, deps).await.expect("realtime starts")
    }

    pub fn rt(&self) -> &Realtime {
        self.rt.as_ref().unwrap()
    }

    pub fn take_rt(&mut self) -> Realtime {
        self.rt.take().unwrap()
    }

    /// #119: the gateway identity for a member token of `sub` naming `tenant`, minted for
    /// realtime.
    pub fn identity(&self, sub: &str, tenant: Uuid) -> String {
        let identity = Identity {
            sub: sub.into(),
            auth: AuthMethod::ManagementJwt,
            request_id: format!("rid-{}", Uuid::new_v4()),
            data: None,
            email: Some(format!("{sub}@example.com")),
            name: Some(format!("Name {sub}")),
        };
        self.signer
            .sign_for_tenant("realtime", &identity, tenant)
            .unwrap()
    }

    /// `POST /v1/realtime/tickets` on `rt` with these headers: (status, headers, body).
    pub async fn post_ticket(
        &self,
        rt: &Realtime,
        identity: Option<&str>,
    ) -> (u16, reqwest::header::HeaderMap, Value) {
        let mut req =
            reqwest::Client::new().post(format!("http://{}/v1/realtime/tickets", rt.addr()));
        if let Some(id) = identity {
            req = req.header(IDENTITY_HEADER, id);
        }
        let resp = req.send().await.expect("the ticket route answers");
        let status = resp.status().as_u16();
        let headers = resp.headers().clone();
        let body = resp.json::<Value>().await.unwrap_or(Value::Null);
        (status, headers, body)
    }

    /// A Studio ticket for `sub` in [`TENANT`] from the first instance.
    pub async fn ticket(&self, sub: &str) -> String {
        self.ticket_on(self.rt(), sub, TENANT).await
    }

    pub async fn ticket_on(&self, rt: &Realtime, sub: &str, tenant: Uuid) -> String {
        let (status, _, body) = self.post_ticket(rt, Some(&self.identity(sub, tenant))).await;
        assert_eq!(status, 201, "ticket for {sub}: {body}");
        body["ticket"].as_str().expect("a ticket").to_string()
    }

    /// An app (data key) ticket, as RT2's route would issue it.
    pub async fn app_ticket(&self, key_id: Uuid) -> String {
        let identity = TicketIdentity {
            sub: key_id.to_string(),
            tenant_id: None,
            auth: AuthMethod::DataKey,
            email: None,
            name: None,
            data: Some(DataIdentity {
                branch_id: BR,
                key_id,
                kind: DataKeyKind::Public,
                role: "anon".into(),
                claims: json!({}),
                databases: None,
                project_id: None,
                role_oid: None,
                server_vouched: false,
            }),
        };
        Tickets::new(self.key.clone(), self.store.clone())
            .issue(&identity)
            .await
            .unwrap()
    }

    /// The socket URL of `rt` for `ticket`.
    pub fn socket_url(rt: &Realtime, ticket: &str) -> String {
        format!("ws://{}/v1/realtime/socket?ticket={ticket}", rt.addr())
    }

    /// Opens a socket with the subprotocol; `Err(status)` when the upgrade is refused.
    pub async fn try_open(rt: &Realtime, ticket: &str, protocol: Option<&str>) -> Result<Ws, u16> {
        let mut req = Self::socket_url(rt, ticket).into_client_request().unwrap();
        if let Some(p) = protocol {
            req.headers_mut()
                .insert("Sec-WebSocket-Protocol", HeaderValue::from_str(p).unwrap());
        }
        match tokio_tungstenite::connect_async(req).await {
            Ok((stream, resp)) => {
                let echoed = resp
                    .headers()
                    .get("sec-websocket-protocol")
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string);
                Ok(Ws {
                    inner: stream,
                    protocol: echoed,
                })
            }
            Err(tungstenite::Error::Http(resp)) => Err(resp.status().as_u16()),
            Err(e) => panic!("the upgrade failed without an HTTP answer: {e}"),
        }
    }

    pub async fn open(rt: &Realtime, ticket: &str) -> Ws {
        Self::try_open(rt, ticket, Some(SUBPROTOCOL))
            .await
            .expect("the socket opens")
    }

    /// A Studio socket for `sub` on the first instance, authenticated for an hour.
    pub async fn authed(&self, sub: &str) -> Ws {
        self.authed_on(self.rt(), sub).await
    }

    pub async fn authed_on(&self, rt: &Realtime, sub: &str) -> Ws {
        let ticket = self.ticket_on(rt, sub, TENANT).await;
        let mut ws = Self::open(rt, &ticket).await;
        ws.send(json!({"type": "auth", "token": token(sub, 3600)}))
            .await;
        ws
    }

    /// Publishes JSON on the shared bus (what the orchestrator and the worker do on NATS).
    pub async fn publish(&self, subject: &str, body: &Value) {
        use realtime::bus::Bus;
        self.bus
            .publish(subject, None, body.to_string().into())
            .await
            .unwrap();
    }
}

/// The join params of `table` in [`DB`]/[`BR`], database `app`, schema `public`.
pub fn table_params(table: &str, role: Option<&str>) -> Value {
    json!({"database_id": DB, "branch_id": BR, "pg_database": "app", "schema": "public", "table": table, "role": role})
}

/// The canonical channel id of [`table_params`].
pub fn table_id(table: &str) -> String {
    format!(r#"table:["{DB}","{BR}","app","public","{table}"]"#)
}

pub fn project_id(p: Uuid) -> String {
    format!("project:{p}")
}

/// What a socket read gave.
#[derive(Debug, Clone, PartialEq)]
pub enum Read {
    Frame(Value),
    /// The close code (1006 when the connection dropped without a close frame).
    Closed(u16),
    Timeout,
}

pub struct Ws {
    inner: WebSocketStream<MaybeTlsStream<TcpStream>>,
    /// The `Sec-WebSocket-Protocol` the 101 carried.
    pub protocol: Option<String>,
}

pub const WAIT: Duration = Duration::from_secs(5);

impl Ws {
    pub async fn send(&mut self, v: Value) {
        self.send_text(v.to_string()).await;
    }
    pub async fn send_text(&mut self, text: String) {
        self.inner
            .send(Message::Text(text.into()))
            .await
            .expect("send");
    }
    pub async fn send_binary(&mut self, bytes: Vec<u8>) {
        self.inner
            .send(Message::Binary(bytes.into()))
            .await
            .expect("send");
    }
    /// The next frame or close within `within` (pings and pongs are skipped).
    pub async fn read(&mut self, within: Duration) -> Read {
        let deadline = Instant::now() + within;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match tokio::time::timeout(left, self.inner.next()).await {
                Err(_) => return Read::Timeout,
                Ok(None) => return Read::Closed(1006),
                Ok(Some(Err(_))) => return Read::Closed(1006),
                Ok(Some(Ok(Message::Text(t)))) => {
                    return Read::Frame(
                        serde_json::from_str(t.as_str()).expect("server frames are JSON"),
                    );
                }
                Ok(Some(Ok(Message::Close(frame)))) => {
                    return Read::Closed(frame.map_or(1005, |f| u16::from(f.code)));
                }
                Ok(Some(Ok(Message::Binary(_)))) => panic!("the server sent a binary frame"),
                Ok(Some(Ok(_))) => continue,
            }
        }
    }
    /// The next frame; panics on a close or after [`WAIT`].
    pub async fn frame(&mut self) -> Value {
        match self.read(WAIT).await {
            Read::Frame(v) => v,
            other => panic!("wanted a frame, got {other:?}"),
        }
    }
    /// The next frame of `type`, skipping others; panics on a close or after [`WAIT`].
    pub async fn frame_of(&mut self, ty: &str) -> Value {
        let deadline = Instant::now() + WAIT;
        loop {
            match self
                .read(deadline.saturating_duration_since(Instant::now()))
                .await
            {
                Read::Frame(v) if v["type"] == ty => return v,
                Read::Frame(_) => continue,
                other => panic!("wanted a {ty} frame, got {other:?}"),
            }
        }
    }
    /// The next frame answering `ref` (`joined` or `error`), skipping others.
    pub async fn reply(&mut self, r: &str) -> Value {
        let deadline = Instant::now() + WAIT;
        loop {
            match self
                .read(deadline.saturating_duration_since(Instant::now()))
                .await
            {
                Read::Frame(v) if v["ref"] == r => return v,
                Read::Frame(_) => continue,
                other => panic!("wanted the answer to ref {r}, got {other:?}"),
            }
        }
    }
    /// The close code, skipping frames; panics after `within`.
    pub async fn closed_within(&mut self, within: Duration) -> u16 {
        let deadline = Instant::now() + within;
        loop {
            match self
                .read(deadline.saturating_duration_since(Instant::now()))
                .await
            {
                Read::Closed(code) => return code,
                Read::Frame(_) => continue,
                Read::Timeout => panic!("the socket is still open after {within:?}"),
            }
        }
    }
    pub async fn closed(&mut self) -> u16 {
        self.closed_within(WAIT).await
    }
    /// Every frame within `d`; panics if the socket closes.
    pub async fn frames_within(&mut self, d: Duration) -> Vec<Value> {
        let deadline = Instant::now() + d;
        let mut out = vec![];
        loop {
            match self
                .read(deadline.saturating_duration_since(Instant::now()))
                .await
            {
                Read::Frame(v) => out.push(v),
                Read::Timeout => return out,
                Read::Closed(code) => panic!("closed {code} while collecting frames: {out:?}"),
            }
        }
    }
    /// Joins `table` as `role` with ref `r`: the `joined` or `error` answer.
    pub async fn join_table(&mut self, r: &str, table: &str, role: Option<&str>) -> Value {
        self.send(json!({"type": "join", "ref": r, "channel": "table", "params": table_params(table, role)})).await;
        self.reply(r).await
    }
    pub async fn join_project(&mut self, r: &str, project: Uuid) -> Value {
        self.send(json!({"type": "join", "ref": r, "channel": project_id(project)}))
            .await;
        self.reply(r).await
    }
    pub async fn close(&mut self) {
        let _ = self.inner.close(None).await;
    }
}

/// Polls `f` every 20 ms until it holds or `within` passes.
pub async fn eventually(within: Duration, f: impl Fn() -> bool) -> bool {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if f() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    f()
}
