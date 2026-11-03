//! The public listener's two routes (spec §1, §2; S18) and one socket's session.
//!
//! - `POST /v1/realtime/tickets`: the gateway's identity (`x-chelabase-identity`, audience
//!   `realtime`, a management JWT's identity) → `201 {ticket, expires_in: 30}` with
//!   `Cache-Control: no-store`; no or a bad identity 401; over 10 tickets per member per second
//!   429; Valkey down 503.
//! - `GET /v1/realtime/socket?ticket=…`: the subprotocol [`crate::protocol::SUBPROTOCOL`] is
//!   required (400 without it or with only another one); the ticket is redeemed (`GETDEL`)
//!   before the 101 (no, an unknown or a used ticket 401; Valkey down 503); the 101 echoes the
//!   subprotocol. Over the member's socket limit the socket opens and closes 4029.
//! - The session: a Studio socket's first frame must be `auth` within 5 s (else 4001); renewals
//!   per [`crate::auth`]; joins per [`crate::channel`], authorized through the orchestrator then
//!   watched through the change source; `rows` numbered per channel from 1; presence; project
//!   events; re-authorization; pings every 25 s, 4008 without a pong in 10 s; frames over 64 KB
//!   close 1009; 1012 on shutdown. A client that stops reading is closed 1008 when its queue
//!   fills ([`crate::outq`]: 1024 frames or 4 MiB), or dropped when a frame waits on its socket
//!   longer than the write timeout; a degraded channel gets keyless `changed` only (spec §4).
//! - The socket loop never waits on the orchestrator or the change source: a join's
//!   `/authorize` and watch, and every re-authorization, run in tasks of the session that hand
//!   their answers back, so renewals, pongs and leaves are read meanwhile. A channel joins once
//!   its answer is back (until then a second join of it is `already joined`, a leave cancels
//!   it); joins are still answered in the order they came; re-authorizations run one at a
//!   time, the next one folded. `view` and `presence`
//!   frames are latest-wins per channel.
//! - Logs never carry the request's query string or an `auth` frame's contents (R22).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{RawQuery, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use chelabase_auth::internal::{AuthMethod, IDENTITY_HEADER};
use chelabase_nats::rt::ProjectEvent;
use futures_util::future::join_all;
use serde_json::json;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use uuid::Uuid;

use crate::app::{SessionEntry, SessionMsg, Shared};
use crate::auth::{AuthCheck, TokenError, check_auth_frame};
use crate::channel::{JoinTarget, Scope, TableChannel, channel_hash, parse_join};
use crate::hub::{Hub, HubEvent};
use crate::limits::RateLimiter;
use crate::orchestrator::{AuthTarget, AuthorizeOutcome, MemberInfo, ProjectsError};
use crate::outq::{OUT_BYTES, OUT_FRAMES, OutRx, OutTx, out_queue};
use crate::presence::{PresenceError, PresenceThrottle, blank, parse_table_state};
use crate::project::{Delivery, ProjectView, route};
use crate::protocol::{
    Change, ChangeOp, ClientFrame, ErrorCode, FrameError, JoinedInfo, MAX_FRAME_BYTES,
    MAX_VIEW_KEYS, Member, SUBPROTOCOL, ServerFrame, StatusReason, TableState, close, color_seed,
    parse_client_frame,
};
use crate::reauth::{ReauthAction, ReauthState};
use crate::shutdown::DRAIN;
use crate::source::{ChannelEvent, CheckIdentity, JoinInfo, JoinRefusal, WatchHandle};
use crate::ticket::{TICKET_TTL, TicketError, TicketIdentity, TicketKind};

/// The ticket route's path.
pub const TICKETS_PATH: &str = "/v1/realtime/tickets";
/// The socket route's path.
pub const SOCKET_PATH: &str = "/v1/realtime/socket";
/// The internal listener's one route.
pub const HEALTH_PATH: &str = "/health";

/// Inbound messages over this size never reach realtime's code (the socket fails instead);
/// between it and [`MAX_FRAME_BYTES`] a frame is refused with 1009.
const MAX_WS_MESSAGE: usize = 1024 * 1024;
/// A change source's events queued for one channel.
const SINK_QUEUE: usize = 256;
/// A project channel refreshes its database list for an unknown database at most this often.
const PROJECT_REFRESH_GAP: Duration = Duration::from_millis(250);
/// RFC 6455 "policy violation": the client stopped reading.
const CLOSE_TOO_SLOW: u16 = 1008;
/// RFC 6455 "try again later": the token can't be checked now (no member-token key) or Valkey is down.
const CLOSE_TRY_LATER: u16 = 1013;

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn unix_secs() -> u64 {
    unix_ms() / 1000
}

/// The instant a unix `exp` (seconds) falls on.
fn instant_of_exp(exp: u64) -> Instant {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    Instant::now() + Duration::from_secs_f64((exp as f64 - now).max(0.0))
}

async fn sleep_opt(at: Option<Instant>) {
    match at {
        Some(t) => tokio::time::sleep_until(t).await,
        None => std::future::pending().await,
    }
}

fn error_frame(
    r#ref: Option<&str>,
    channel: Option<&str>,
    code: ErrorCode,
    message: &str,
) -> String {
    ServerFrame::Error {
        r#ref: r#ref.map(str::to_string),
        channel: channel.map(str::to_string),
        code,
        message: message.to_string(),
    }
    .to_text()
}

// --- The ticket route ------------------------------------------------------------------------

pub(crate) async fn tickets(State(shared): State<Arc<Shared>>, headers: HeaderMap) -> Response {
    let Some(identity) = headers.get(IDENTITY_HEADER).and_then(|v| v.to_str().ok()) else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let claims = match shared.identity.verify(identity, "realtime") {
        Ok(c) => c,
        Err(_) => {
            tracing::info!("a ticket request without a valid gateway identity");
            return StatusCode::UNAUTHORIZED.into_response();
        }
    };
    // RT1 issues Studio tickets only; an app's ticket is RT2's route.
    if claims.auth != AuthMethod::ManagementJwt {
        return StatusCode::FORBIDDEN.into_response();
    }
    match shared.ticket_rate.allow(&claims.sub, unix_ms()).await {
        Ok(true) => {}
        Ok(false) => return StatusCode::TOO_MANY_REQUESTS.into_response(),
        Err(e) => {
            tracing::warn!(error = %e, "ticket rate check failed: Valkey");
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
    }
    // T8-1, D7: a suspended or offboarded member (or one no longer a member) gets no ticket;
    // the orchestrator decides, and one that can't answer is a 503.
    match shared
        .orch
        .member_allowed(&claims.sub, claims.tenant_id, claims.sid.as_deref())
        .await
    {
        Some(true) => {}
        Some(false) => return StatusCode::FORBIDDEN.into_response(),
        None => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
    match shared
        .tickets
        .issue(&TicketIdentity::from_claims(&claims))
        .await
    {
        Ok(ticket) => (
            StatusCode::CREATED,
            [(header::CACHE_CONTROL, "no-store")],
            axum::Json(json!({"ticket": ticket, "expires_in": TICKET_TTL.as_secs()})),
        )
            .into_response(),
        Err(e) => {
            tracing::warn!(error = %e, "a ticket couldn't be stored");
            StatusCode::SERVICE_UNAVAILABLE.into_response()
        }
    }
}

// --- The upgrade -----------------------------------------------------------------------------

/// Whether the client offered [`SUBPROTOCOL`] (among others, comma-separated).
fn offers_subprotocol(headers: &HeaderMap) -> bool {
    headers
        .get_all(header::SEC_WEBSOCKET_PROTOCOL)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .any(|p| p.trim() == SUBPROTOCOL)
}

/// The `ticket` query parameter, decoded.
fn ticket_param(query: Option<&str>) -> Option<String> {
    query?
        .split('&')
        .find_map(|pair| pair.strip_prefix("ticket="))
        .and_then(|v| urlencoding::decode(v).ok())
        .map(|v| v.into_owned())
        .filter(|v| !v.is_empty())
}

pub(crate) async fn upgrade(
    State(shared): State<Arc<Shared>>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
    ws: WebSocketUpgrade,
) -> Response {
    // The version first: a refused upgrade never spends the ticket.
    if !offers_subprotocol(&headers) {
        return (
            StatusCode::BAD_REQUEST,
            "the chelabase.realtime.v1 subprotocol is required",
        )
            .into_response();
    }
    let Some(ticket) = ticket_param(query.as_deref()) else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let identity = match shared.tickets.redeem(&ticket).await {
        Ok(identity) => identity,
        Err(TicketError::Store(e)) => {
            tracing::warn!(error = %e, "a ticket couldn't be redeemed: Valkey");
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
        Err(TicketError::Forged) => {
            tracing::warn!("a ticket's stored value didn't open (tampered or copied)");
            return StatusCode::UNAUTHORIZED.into_response();
        }
        Err(_) => return StatusCode::UNAUTHORIZED.into_response(),
    };
    ws.protocols([SUBPROTOCOL])
        .max_message_size(MAX_WS_MESSAGE)
        .max_frame_size(MAX_WS_MESSAGE)
        .on_failed_upgrade(|e| tracing::debug!(error = %e, "a socket upgrade failed"))
        .on_upgrade(move |socket| start_session(shared, socket, identity))
}

async fn start_session(shared: Arc<Shared>, ws: WebSocket, ticket: TicketIdentity) {
    let id = shared.next_id();
    let (tx, ctl) = mpsc::unbounded_channel();
    let tenants = Arc::new(Mutex::new(HashSet::new()));
    shared.sessions.lock().unwrap().insert(
        id,
        SessionEntry {
            tx,
            sub: ticket.sub.clone(),
            tenants: tenants.clone(),
            abort: None,
        },
    );
    let session = Session::new(shared.clone(), id, ws, ticket, ctl, tenants);
    let task = tokio::spawn(session.run());
    if let Some(entry) = shared.sessions.lock().unwrap().get_mut(&id) {
        entry.abort = Some(task.abort_handle());
    }
    let _ = task.await;
}

// --- One socket ------------------------------------------------------------------------------

/// A channel's task: aborted when its channel goes, and when its session is aborted (a
/// killed instance leaves nothing running).
struct TaskGuard(Option<JoinHandle<()>>);

impl TaskGuard {
    fn spawn(f: impl std::future::Future<Output = ()> + Send + 'static) -> Self {
        Self(Some(tokio::spawn(f)))
    }
    /// Aborts the task and waits until it is gone (its watch handle and hub dropped).
    async fn stop(mut self) {
        if let Some(t) = self.0.take() {
            t.abort();
            let _ = t.await;
        }
    }
}

impl Drop for TaskGuard {
    fn drop(&mut self) {
        if let Some(t) = &self.0 {
            t.abort();
        }
    }
}

enum Auth {
    /// A Studio socket before its first `auth` frame.
    Waiting {
        deadline: Instant,
    },
    Member {
        token: Arc<str>,
        exp_at: Instant,
        /// The token's identity session: re-authorization answers are shared only within it.
        sid: Arc<str>,
    },
    /// An app socket (RT2): never sends `auth`.
    App,
}

/// The last status a table channel's source reported (sent again when it stops being
/// degraded).
type StatusCell = Arc<Mutex<(bool, Option<StatusReason>)>>;

enum ChannelCmd {
    Degraded(bool),
}

struct TableChan {
    target: AuthTarget,
    handle: Arc<WatchHandle>,
    hub: Arc<Hub>,
    hub_key: String,
    viewer: u64,
    member_id: String,
    cmds: mpsc::UnboundedSender<ChannelCmd>,
    /// Latest-wins: a `presence` or `view` frame replaces one the task hasn't taken yet.
    presence: watch::Sender<Option<TableState>>,
    view: watch::Sender<Option<Vec<String>>>,
    status: StatusCell,
    reauth: ReauthState,
    task: TaskGuard,
}

struct ProjectChan {
    project_id: Uuid,
    registration: u64,
    view: Arc<Mutex<ProjectView>>,
    reauth: ReauthState,
    task: TaskGuard,
}

enum Chan {
    Table(TableChan),
    Project(ProjectChan),
}

enum Flow {
    Go,
    Stop,
}

/// How a re-authorization asks the orchestrator.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Fresh {
    /// Every answer from the orchestrator, with this socket's token (a renewal).
    Always,
    /// An access burst of the member (instance-wide coalescing, R15): one `/authorize` per
    /// target for all the member's sockets.
    Burst(u64),
    /// The periodic cycle: the instance's answer for this (member, target, cycle) if it has one.
    Cycle(u64),
}

impl Fresh {
    /// Of two runs folded into one, the one that asks the most freshly (the later of two
    /// bursts or cycles).
    fn stronger(self, other: Fresh) -> Fresh {
        let rank = |f: Fresh| match f {
            Fresh::Cycle(n) => (0, n),
            Fresh::Burst(n) => (1, n),
            Fresh::Always => (2, 0),
        };
        if rank(other) > rank(self) { other } else { self }
    }
}

/// A table join's answers, handed back to the session.
struct TableJoined {
    tenant_id: Uuid,
    member: MemberInfo,
    handle: WatchHandle,
    info: JoinInfo,
    events: mpsc::Receiver<ChannelEvent>,
}

/// One channel's re-authorization answer.
struct ReauthAnswer {
    id: String,
    /// The channel's identity when asked (its viewer or registration id): a channel left and
    /// joined again meanwhile isn't the one asked about.
    generation: u64,
    outcome: AuthorizeOutcome,
    view: Option<ProjectView>,
    /// S16: an allowed table's reprobe.
    probed: Option<Result<JoinInfo, JoinRefusal>>,
}

/// A join's place in the order joins are answered in.
enum JoinSlot {
    /// Its task runs (the channel id).
    Waiting(String),
    /// Its answers are in, waiting for an earlier join's.
    Ready(Box<Done>),
    /// Refused at once, waiting for an earlier join's answer.
    Refused(String),
}

/// What the session's background work hands back.
enum Done {
    Table {
        id: String,
        r#ref: String,
        channel: TableChannel,
        role: Option<String>,
        token: Arc<str>,
        target: AuthTarget,
        result: Result<Box<TableJoined>, (ErrorCode, String)>,
    },
    Project {
        id: String,
        r#ref: String,
        project_id: Uuid,
        result: Result<ProjectView, (ErrorCode, &'static str)>,
    },
    Reauth(Vec<ReauthAnswer>),
}

impl Done {
    /// The channel a join's answer is for.
    fn join_id(&self) -> Option<&str> {
        match self {
            Done::Table { id, .. } | Done::Project { id, .. } => Some(id),
            Done::Reauth(_) => None,
        }
    }
}

struct Session {
    shared: Arc<Shared>,
    id: u64,
    socket_id: String,
    ws: WebSocket,
    ticket: TicketIdentity,
    auth: Auth,
    token_tx: watch::Sender<Arc<str>>,
    out_tx: OutTx,
    out_rx: OutRx,
    overflow: bool,
    ctl: mpsc::UnboundedReceiver<SessionMsg>,
    tenants: Arc<Mutex<HashSet<Uuid>>>,
    channels: HashMap<String, Chan>,
    /// Joins waiting for their answers, by channel id.
    pending: HashMap<String, TaskGuard>,
    /// Joins in the order they came, answered in that order.
    join_queue: std::collections::VecDeque<JoinSlot>,
    done_tx: mpsc::UnboundedSender<Done>,
    done_rx: mpsc::UnboundedReceiver<Done>,
    /// The re-authorization running, and the one folded behind it.
    reauth_task: Option<TaskGuard>,
    reauth_next: Option<Fresh>,
    joins: RateLimiter,
    pong_deadline: Option<Instant>,
    write_timeout: Duration,
    /// Which of the socket's two directions [`next_io`] tries first this time.
    prefer_out: bool,
}

/// The socket's next inbound message or queued outbound frame.
enum Io {
    In(Option<Result<Message, axum::Error>>),
    Out(String),
}

/// Reading and writing take turns at going first, so neither direction starves the other: a
/// client that never stops sending still gets its frames (or fills its queue), and a busy
/// channel never keeps the client's frames (its renewals) unread.
async fn next_io(ws: &mut WebSocket, out: &mut OutRx, prefer_out: &mut bool) -> Io {
    *prefer_out = !*prefer_out;
    if *prefer_out {
        tokio::select! {
            biased;
            Some(text) = out.recv() => Io::Out(text),
            m = ws.recv() => Io::In(m),
        }
    } else {
        tokio::select! {
            biased;
            m = ws.recv() => Io::In(m),
            Some(text) = out.recv() => Io::Out(text),
        }
    }
}

impl Session {
    fn new(
        shared: Arc<Shared>,
        id: u64,
        ws: WebSocket,
        ticket: TicketIdentity,
        ctl: mpsc::UnboundedReceiver<SessionMsg>,
        tenants: Arc<Mutex<HashSet<Uuid>>>,
    ) -> Self {
        let timings = &shared.config.timings;
        let auth = match ticket.kind() {
            TicketKind::Studio => Auth::Waiting {
                deadline: Instant::now() + timings.auth_timeout,
            },
            TicketKind::App => Auth::App,
        };
        let (out_tx, out_rx) = out_queue(OUT_FRAMES, OUT_BYTES);
        let (done_tx, done_rx) = mpsc::unbounded_channel();
        let joins = RateLimiter::new(shared.config.limits.joins_per_sec);
        Self {
            id,
            socket_id: Uuid::new_v4().to_string(),
            ws,
            ticket,
            auth,
            token_tx: watch::channel(Arc::<str>::from("")).0,
            out_tx,
            out_rx,
            overflow: false,
            ctl,
            tenants,
            channels: HashMap::new(),
            pending: HashMap::new(),
            join_queue: std::collections::VecDeque::new(),
            done_tx,
            done_rx,
            reauth_task: None,
            reauth_next: None,
            joins,
            pong_deadline: None,
            write_timeout: timings.write_timeout,
            prefer_out: false,
            shared,
        }
    }

    /// Writes one message; `false` when the socket failed or didn't take it within the write
    /// timeout (a client that stopped reading: the socket is given up).
    async fn write(&mut self, m: Message) -> bool {
        matches!(
            tokio::time::timeout(self.write_timeout, self.ws.send(m)).await,
            Ok(Ok(()))
        )
    }

    /// Queues a frame; a client that doesn't read fills its queue and is closed.
    fn send(&mut self, text: String) {
        if self.out_tx.try_send(text).is_err() {
            self.overflow = true;
        }
    }

    fn error(
        &mut self,
        r#ref: Option<&str>,
        channel: Option<&str>,
        code: ErrorCode,
        message: &str,
    ) {
        self.send(error_frame(r#ref, channel, code, message));
    }

    fn token(&self) -> Option<Arc<str>> {
        match &self.auth {
            Auth::Member { token, .. } => Some(token.clone()),
            _ => None,
        }
    }

    /// The current token's identity session (empty before the first `auth` frame).
    fn sid(&self) -> Arc<str> {
        match &self.auth {
            Auth::Member { sid, .. } => sid.clone(),
            _ => Arc::from(""),
        }
    }

    /// Sends the close frame, then reads until the client answers it (or [`DRAIN`] passes), so
    /// the close isn't lost to a reset.
    async fn close(&mut self, code: u16, reason: &str) {
        let frame = CloseFrame {
            code,
            reason: reason.into(),
        };
        if !self.write(Message::Close(Some(frame))).await {
            return;
        }
        let _ = tokio::time::timeout(DRAIN, async {
            while let Some(Ok(m)) = self.ws.recv().await {
                if matches!(m, Message::Close(_)) {
                    break;
                }
            }
        })
        .await;
    }

    async fn run(mut self) {
        let timings = self.shared.config.timings.clone();
        let mut shutdown = self.shared.shutdown.subscribe();
        if *shutdown.borrow() {
            self.close(close::RESTARTING, "restarting").await;
            self.cleanup().await;
            return;
        }
        match self
            .shared
            .sockets
            .admit(&self.ticket.sub, &self.socket_id, unix_ms())
            .await
        {
            Ok(true) => {}
            Ok(false) => {
                self.close(close::LIMIT, "too many sockets").await;
                self.cleanup().await;
                return;
            }
            Err(e) => {
                tracing::warn!(error = %e, "the socket count is unavailable: Valkey");
                self.close(CLOSE_TRY_LATER, "try again later").await;
                self.cleanup().await;
                return;
            }
        }
        let start = Instant::now();
        let mut ping =
            tokio::time::interval_at(start + timings.ping_interval, timings.ping_interval);
        ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut reauth =
            tokio::time::interval_at(start + timings.reauth_interval, timings.reauth_interval);
        reauth.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut refresh =
            tokio::time::interval_at(start + timings.socket_refresh, timings.socket_refresh);
        refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            if self.overflow {
                self.close(CLOSE_TOO_SLOW, "the client isn't reading").await;
                break;
            }
            let (auth_deadline, exp_at) = match &self.auth {
                Auth::Waiting { deadline } => (Some(*deadline), None),
                Auth::Member { exp_at, .. } => (None, Some(*exp_at)),
                Auth::App => (None, None),
            };
            // Deadlines and the instance's own events come before the socket's traffic (they're
            // ready only now and then, so they starve nothing): a client that never stops
            // sending is still closed at its deadlines and still re-authorized. The pong
            // deadline comes last, so a pong that arrived while the session was busy is read
            // before it counts as missing.
            let flow = tokio::select! {
                biased;
                _ = async { let _ = shutdown.wait_for(|s| *s).await; } => {
                    self.close(close::RESTARTING, "restarting").await;
                    Flow::Stop
                }
                _ = sleep_opt(auth_deadline) => {
                    self.close(close::TOKEN_EXPIRED, "no auth frame").await;
                    Flow::Stop
                }
                _ = sleep_opt(exp_at) => {
                    self.close(close::TOKEN_EXPIRED, "the token expired").await;
                    Flow::Stop
                }
                Some(SessionMsg::Access(burst)) = self.ctl.recv() => {
                    self.reauthorize(Fresh::Burst(burst));
                    Flow::Go
                }
                Some(done) = self.done_rx.recv() => self.on_done(done).await,
                _ = reauth.tick() => {
                    self.reauthorize(Fresh::Cycle(self.shared.cycle()));
                    Flow::Go
                }
                _ = refresh.tick() => {
                    if let Err(e) = self.shared.sockets.refresh(&self.ticket.sub, &self.socket_id, unix_ms()).await {
                        tracing::warn!(error = %e, "the socket count refresh failed: Valkey");
                    }
                    Flow::Go
                }
                _ = ping.tick() => {
                    if self.write(Message::Ping(Bytes::new())).await {
                        self.pong_deadline.get_or_insert(Instant::now() + timings.pong_timeout);
                        Flow::Go
                    } else {
                        Flow::Stop
                    }
                }
                io = next_io(&mut self.ws, &mut self.out_rx, &mut self.prefer_out) => match io {
                    Io::In(Some(Ok(m))) => self.on_message(m).await,
                    Io::In(_) => Flow::Stop,
                    Io::Out(text) => {
                        if self.write(Message::Text(text.into())).await {
                            Flow::Go
                        } else {
                            Flow::Stop
                        }
                    }
                },
                _ = sleep_opt(self.pong_deadline) => {
                    self.close(close::PONG_TIMEOUT, "no pong").await;
                    Flow::Stop
                }
            };
            if let Flow::Stop = flow {
                break;
            }
        }
        self.cleanup().await;
    }

    /// Leaves every channel (presence leaves, unwatches) and gives the socket's count back.
    async fn cleanup(&mut self) {
        // Background work first: a join that comes back now is dropped (its watch with it).
        self.pending.clear();
        self.join_queue.clear();
        self.reauth_task = None;
        while self.done_rx.try_recv().is_ok() {}
        let ids: Vec<String> = self.channels.keys().cloned().collect();
        for id in ids {
            if let Some(ch) = self.channels.remove(&id) {
                self.drop_channel(ch).await;
            }
        }
        if let Err(e) = self
            .shared
            .sockets
            .release(&self.ticket.sub, &self.socket_id)
            .await
        {
            tracing::warn!(error = %e, "the socket count release failed: Valkey (heals in 90 s)");
        }
        self.shared.sessions.lock().unwrap().remove(&self.id);
    }

    async fn on_message(&mut self, msg: Message) -> Flow {
        match msg {
            Message::Text(text) => {
                if text.len() > MAX_FRAME_BYTES {
                    self.close(close::TOO_BIG, "frames are at most 64 KB").await;
                    return Flow::Stop;
                }
                let parsed = parse_client_frame(text.as_str());
                self.on_frame(parsed).await
            }
            Message::Binary(bytes) => {
                if bytes.len() > MAX_FRAME_BYTES {
                    self.close(close::TOO_BIG, "frames are at most 64 KB").await;
                    return Flow::Stop;
                }
                if matches!(self.auth, Auth::Waiting { .. }) {
                    self.close(close::TOKEN_EXPIRED, "the first frame must be auth")
                        .await;
                    return Flow::Stop;
                }
                self.error(
                    None,
                    None,
                    ErrorCode::Invalid,
                    "binary frames aren't the protocol",
                );
                Flow::Go
            }
            Message::Pong(_) => {
                self.pong_deadline = None;
                Flow::Go
            }
            Message::Ping(_) => Flow::Go,
            Message::Close(_) => Flow::Stop,
        }
    }

    async fn on_frame(&mut self, parsed: Result<ClientFrame, FrameError>) -> Flow {
        match (&self.auth, parsed) {
            (_, Err(FrameError::TooLarge)) => {
                self.close(close::TOO_BIG, "frames are at most 64 KB").await;
                Flow::Stop
            }
            (Auth::Waiting { .. }, Ok(ClientFrame::Auth { token })) => self.first_auth(token).await,
            (Auth::Waiting { .. }, _) => {
                self.close(close::TOKEN_EXPIRED, "the first frame must be auth")
                    .await;
                Flow::Stop
            }
            (Auth::App, Ok(ClientFrame::Auth { .. })) => {
                self.close(close::REVOKED, "an app socket sends no auth frame")
                    .await;
                Flow::Stop
            }
            (Auth::Member { .. }, Ok(ClientFrame::Auth { token })) => self.renewal(token).await,
            (_, Err(FrameError::UnknownType(_))) => {
                self.error(None, None, ErrorCode::Invalid, "unknown frame type");
                Flow::Go
            }
            (_, Err(FrameError::Invalid(message))) => {
                self.error(None, None, ErrorCode::Invalid, &message);
                Flow::Go
            }
            (
                _,
                Ok(ClientFrame::Join {
                    r#ref,
                    channel,
                    params,
                }),
            ) => {
                self.join(&r#ref, &channel, params);
                Flow::Go
            }
            (_, Ok(ClientFrame::Leave { r#ref, channel })) => {
                if self.pending.remove(&channel).is_some() {
                    // A join waiting for its answer: cancelled (its answer is dropped), and
                    // the joins after it no longer wait for it.
                    self.join_queue
                        .retain(|s| !matches!(s, JoinSlot::Waiting(id) if *id == channel));
                    self.drain_joins().await;
                    return Flow::Go;
                }
                match self.channels.remove(&channel) {
                    Some(ch) => self.drop_channel(ch).await,
                    None => self.error(
                        Some(&r#ref),
                        Some(&channel),
                        ErrorCode::Invalid,
                        "not joined",
                    ),
                }
                Flow::Go
            }
            (_, Ok(ClientFrame::Presence { channel, state })) => {
                self.presence(&channel, &state);
                Flow::Go
            }
            (_, Ok(ClientFrame::View { channel, keys })) => {
                self.view(&channel, keys);
                Flow::Go
            }
        }
    }

    /// R9/S14: the first `auth` frame.
    async fn first_auth(&mut self, token: String) -> Flow {
        let verified = self.shared.verifier.verify(&token).await;
        let member = match verified {
            Ok(m) => m,
            Err(TokenError::Unavailable) => {
                tracing::warn!("a socket's token can't be checked now (the member-token verifier)");
                self.close(CLOSE_TRY_LATER, "the token can't be checked now")
                    .await;
                return Flow::Stop;
            }
            Err(_) => {
                self.close(close::TOKEN_EXPIRED, "the token doesn't verify")
                    .await;
                return Flow::Stop;
            }
        };
        match check_auth_frame(&self.ticket, &member, unix_secs()) {
            AuthCheck::Mismatch => {
                tracing::warn!(sub = %self.ticket.sub, "an auth frame for another person or org");
                self.close(close::REVOKED, "the token is another person's")
                    .await;
                Flow::Stop
            }
            AuthCheck::Expired => {
                self.close(close::TOKEN_EXPIRED, "the token expired").await;
                Flow::Stop
            }
            AuthCheck::Ok => {
                let token: Arc<str> = Arc::from(token);
                self.token_tx.send_replace(token.clone());
                self.auth = Auth::Member {
                    token,
                    exp_at: instant_of_exp(member.exp),
                    sid: Arc::from(member.sid),
                };
                Flow::Go
            }
        }
    }

    /// W32: a renewal that fails closes at once; one that verifies re-authorizes every channel.
    async fn renewal(&mut self, token: String) -> Flow {
        let member = match self.shared.verifier.verify(&token).await {
            Ok(m) => m,
            Err(TokenError::Unavailable) => {
                // Not a failed renewal: the token in hand stands until its own exp.
                tracing::warn!("a renewal can't be checked now (the member-token verifier): the current token stands");
                return Flow::Go;
            }
            Err(_) => {
                self.close(close::TOKEN_EXPIRED, "the renewal doesn't verify")
                    .await;
                return Flow::Stop;
            }
        };
        match check_auth_frame(&self.ticket, &member, unix_secs()) {
            AuthCheck::Mismatch => {
                tracing::warn!(sub = %self.ticket.sub, "a renewal for another person or org");
                self.close(close::REVOKED, "the token is another person's")
                    .await;
                Flow::Stop
            }
            AuthCheck::Expired => {
                self.close(close::TOKEN_EXPIRED, "the token expired").await;
                Flow::Stop
            }
            AuthCheck::Ok => {
                let token: Arc<str> = Arc::from(token);
                self.token_tx.send_replace(token.clone());
                // T8: the owner path's checks carry the verified renewal from now on.
                let handles: Vec<Arc<WatchHandle>> = self
                    .channels
                    .values()
                    .filter_map(|c| match c {
                        Chan::Table(t) => Some(t.handle.clone()),
                        Chan::Project(_) => None,
                    })
                    .collect();
                for h in handles {
                    self.shared.source.renew(&h, token.clone()).await;
                }
                self.auth = Auth::Member {
                    token,
                    exp_at: instant_of_exp(member.exp),
                    sid: Arc::from(member.sid),
                };
                self.reauthorize(Fresh::Always);
                Flow::Go
            }
        }
    }

    fn join(&mut self, r#ref: &str, channel: &str, params: Option<serde_json::Value>) {
        let target = match parse_join(channel, params.as_ref()) {
            Ok(t) => t,
            Err(message) => return self.join_error(Some(r#ref), None, ErrorCode::Invalid, &message),
        };
        let id = target.id();
        // R9: a Studio ticket joins tables and projects, an app ticket app channels only.
        let kind = self.ticket.kind();
        match (&target, kind) {
            (JoinTarget::App(_), TicketKind::Studio)
            | (JoinTarget::Table { .. } | JoinTarget::Project(_), TicketKind::App) => {
                return self.join_error(
                    Some(r#ref),
                    Some(&id),
                    ErrorCode::Forbidden,
                    "this ticket can't join that channel",
                );
            }
            (JoinTarget::App(_), TicketKind::App) => {
                return self.join_error(
                    Some(r#ref),
                    Some(&id),
                    ErrorCode::Unavailable,
                    "app channels aren't served yet",
                );
            }
            _ => {}
        }
        if !self.joins.allow(std::time::Instant::now()) {
            let message = format!(
                "at most {} joins a second",
                self.shared.config.limits.joins_per_sec
            );
            return self.join_error(Some(r#ref), Some(&id), ErrorCode::Limit, &message);
        }
        if self.channels.contains_key(&id) || self.pending.contains_key(&id) {
            return self.join_error(Some(r#ref), Some(&id), ErrorCode::Invalid, "already joined");
        }
        if self.channels.len() + self.pending.len() >= self.shared.config.limits.channels_per_socket
        {
            let message = format!(
                "at most {} channels per socket",
                self.shared.config.limits.channels_per_socket
            );
            return self.join_error(Some(r#ref), Some(&id), ErrorCode::Limit, &message);
        }
        let Some(token) = self.token() else {
            return;
        };
        let (shared, done) = (self.shared.clone(), self.done_tx.clone());
        let r#ref = r#ref.to_string();
        let task = match target {
            JoinTarget::Table { channel, role } => {
                let target = AuthTarget {
                    database_id: channel.database_id,
                    branch_id: channel.branch_id,
                    pg_database: channel.pg_database.clone(),
                    role: role.clone(),
                };
                let (sub, sid) = (self.ticket.sub.clone(), self.sid());
                let id = id.clone();
                TaskGuard::spawn(async move {
                    let result =
                        table_join_work(&shared, (&sub, &sid), &channel, &role, &token, &target)
                            .await;
                    let _ = done.send(Done::Table {
                        id,
                        r#ref,
                        channel,
                        role,
                        token,
                        target,
                        result,
                    });
                })
            }
            JoinTarget::Project(project_id) => {
                let id = id.clone();
                TaskGuard::spawn(async move {
                    let result = project_join_work(&shared, project_id, &token).await;
                    let _ = done.send(Done::Project {
                        id,
                        r#ref,
                        project_id,
                        result,
                    });
                })
            }
            JoinTarget::App(_) => return,
        };
        self.join_queue.push_back(JoinSlot::Waiting(id.clone()));
        self.pending.insert(id, task);
    }

    /// A join refused at once: answered now, or after the joins before it.
    fn join_error(&mut self, r#ref: Option<&str>, channel: Option<&str>, code: ErrorCode, message: &str) {
        let frame = error_frame(r#ref, channel, code, message);
        if self.join_queue.is_empty() {
            self.send(frame);
        } else {
            self.join_queue.push_back(JoinSlot::Refused(frame));
        }
    }

    /// Answers the joins at the front of the order whose answers are in.
    async fn drain_joins(&mut self) {
        while let Some(front) = self.join_queue.front() {
            if matches!(front, JoinSlot::Waiting(_)) {
                return;
            }
            match self.join_queue.pop_front() {
                Some(JoinSlot::Refused(frame)) => self.send(frame),
                Some(JoinSlot::Ready(done)) => self.finish_join(*done).await,
                _ => {}
            }
        }
    }

    /// A join's or a re-authorization's answers, back from its task.
    async fn on_done(&mut self, done: Done) -> Flow {
        if let Some(id) = done.join_id() {
            // A join left meanwhile has no place any more: dropped (its watch with it).
            if let Some(slot) = self
                .join_queue
                .iter_mut()
                .find(|s| matches!(s, JoinSlot::Waiting(w) if w == id))
            {
                *slot = JoinSlot::Ready(Box::new(done));
                self.drain_joins().await;
            }
            return Flow::Go;
        }
        if let Done::Reauth(answers) = done {
            self.reauth_task = None;
            for answer in answers {
                self.apply_reauth(answer).await;
            }
            if let Some(next) = self.reauth_next.take() {
                self.reauthorize(next);
            }
        }
        Flow::Go
    }

    /// A join's answers, in its turn.
    async fn finish_join(&mut self, done: Done) {
        match done {
            Done::Table {
                id,
                r#ref,
                channel,
                role,
                token,
                target,
                result,
            } => {
                if self.pending.remove(&id).is_none() {
                    return;
                }
                match result {
                    Ok(joined) => {
                        self.join_table(&r#ref, id, channel, role, token, target, joined)
                            .await
                    }
                    Err((code, message)) => self.error(Some(&r#ref), Some(&id), code, &message),
                }
            }
            Done::Project {
                id,
                r#ref,
                project_id,
                result,
            } => {
                if self.pending.remove(&id).is_none() {
                    return;
                }
                match result {
                    Ok(view) => self.join_project(&r#ref, id, project_id, view),
                    Err((code, message)) => self.error(Some(&r#ref), Some(&id), code, message),
                }
            }
            Done::Reauth(_) => {}
        }
    }

    /// A table channel whose authorization and watch came back.
    #[allow(clippy::too_many_arguments)]
    async fn join_table(
        &mut self,
        r#ref: &str,
        id: String,
        channel: TableChannel,
        role: Option<String>,
        token: Arc<str>,
        target: AuthTarget,
        joined: Box<TableJoined>,
    ) {
        let TableJoined {
            tenant_id,
            member,
            handle,
            info,
            events,
        } = *joined;
        self.tenants.lock().unwrap().insert(tenant_id);
        let handle = Arc::new(handle);
        // A renewal verified while the join waited: the watch carries it from now on.
        if let Some(current) = self.token()
            && current != token
        {
            self.shared.source.renew(&handle, current).await;
        }
        let scope = Scope::Table {
            branch_id: channel.branch_id,
            db_oid: info.db_oid,
        };
        let hub_key = channel_hash(&scope, &id);
        let own = Member {
            id: Uuid::new_v4().to_string(),
            sub: self.ticket.sub.clone(),
            name: member.name,
            email: member.email,
            role,
            color_seed: color_seed(&self.ticket.sub),
            state: TableState::default(),
        };
        let viewer = self.shared.next_id();
        let (hub, hub_rx, join_msg) = match self.shared.join_hub(scope, &id, &hub_key, viewer, &own)
        {
            Ok(j) => j,
            Err(PresenceError::Full) => {
                let message = format!(
                    "at most {} members per channel",
                    self.shared.config.limits.members_per_channel
                );
                return self.error(Some(r#ref), Some(&id), ErrorCode::Limit, &message);
            }
            Err(e) => {
                return self.error(Some(r#ref), Some(&id), ErrorCode::Invalid, &e.to_string());
            }
        };
        self.send(
            ServerFrame::Joined {
                r#ref: r#ref.to_string(),
                channel: id.clone(),
                info: JoinedInfo::Table {
                    pk: info.pk.clone(),
                    live: info.live,
                    reason: info.reason,
                },
            }
            .to_text(),
        );
        let status: StatusCell = Arc::new(Mutex::new((info.live, info.reason)));
        let (cmds, cmd_rx) = mpsc::unbounded_channel();
        let (presence, presence_rx) = watch::channel(None);
        let (view, view_rx) = watch::channel(None);
        let task = TaskGuard::spawn(
            TableTask {
                channel: id.clone(),
                handle: handle.clone(),
                shared: self.shared.clone(),
                out: self.out_tx.clone(),
                hub: hub.clone(),
                own: own.clone(),
                throttle: PresenceThrottle::new(self.shared.config.limits.presence_per_sec),
                raw: BTreeMap::new(),
                shown: BTreeMap::new(),
                degraded: false,
                seq: 0,
                status: status.clone(),
            }
            .run(events, hub_rx, cmd_rx, presence_rx, view_rx),
        );
        hub.publish(&join_msg).await;
        self.channels.insert(
            id,
            Chan::Table(TableChan {
                target,
                handle,
                hub,
                hub_key,
                viewer,
                member_id: own.id,
                cmds,
                presence,
                view,
                status,
                reauth: ReauthState::new(),
                task,
            }),
        );
    }

    /// A project channel whose project list came back.
    fn join_project(&mut self, r#ref: &str, id: String, project_id: Uuid, view: ProjectView) {
        self.tenants.lock().unwrap().insert(view.tenant_id);
        let registration = self.shared.next_id();
        let events = self.shared.register_project(project_id, registration);
        self.send(
            ServerFrame::Joined {
                r#ref: r#ref.to_string(),
                channel: id.clone(),
                info: JoinedInfo::Project {},
            }
            .to_text(),
        );
        let view = Arc::new(Mutex::new(view));
        let task = TaskGuard::spawn(project_task(
            self.shared.clone(),
            id.clone(),
            project_id,
            view.clone(),
            events,
            self.token_tx.subscribe(),
            self.out_tx.clone(),
        ));
        self.channels.insert(
            id,
            Chan::Project(ProjectChan {
                project_id,
                registration,
                view,
                reauth: ReauthState::new(),
                task,
            }),
        );
    }

    /// Leaves a channel: its presence member goes, its watch is dropped.
    async fn drop_channel(&mut self, ch: Chan) {
        match ch {
            Chan::Table(t) => {
                let leave = self
                    .shared
                    .leave_hub(&t.hub_key, &t.hub, t.viewer, &t.member_id);
                t.task.stop().await;
                drop(t.handle);
                t.hub.publish(&leave).await;
            }
            Chan::Project(p) => {
                self.shared.unregister_project(p.project_id, p.registration);
                p.task.stop().await;
            }
        }
    }

    fn presence(&mut self, channel: &str, state: &serde_json::Value) {
        let Some(Chan::Table(t)) = self.channels.get(channel) else {
            return self.error(
                None,
                Some(channel),
                ErrorCode::Invalid,
                "not a joined table",
            );
        };
        match parse_table_state(state) {
            Ok(s) => {
                t.presence.send_replace(Some(s));
            }
            Err(PresenceError::TooLarge) => self.error(
                None,
                Some(channel),
                ErrorCode::Limit,
                "presence state is at most 1 KB",
            ),
            Err(e) => self.error(None, Some(channel), ErrorCode::Invalid, &e.to_string()),
        }
    }

    fn view(&mut self, channel: &str, keys: Vec<String>) {
        let Some(Chan::Table(t)) = self.channels.get(channel) else {
            return self.error(
                None,
                Some(channel),
                ErrorCode::Invalid,
                "not a joined table",
            );
        };
        if keys.len() > MAX_VIEW_KEYS {
            let message = format!("at most {MAX_VIEW_KEYS} keys in a view");
            return self.error(None, Some(channel), ErrorCode::Limit, &message);
        }
        t.view.send_replace(Some(keys));
    }

    /// Every joined channel asks again (spec §4, W19, W28), in a task of the session: its
    /// answers come back as [`Done::Reauth`]. One at a time: a run asked for meanwhile is folded
    /// into one that starts when this one's answers are in.
    fn reauthorize(&mut self, fresh: Fresh) {
        let Some(token) = self.token() else {
            return;
        };
        if self.channels.is_empty() {
            return;
        }
        if self.reauth_task.is_some() {
            self.reauth_next = Some(match self.reauth_next {
                Some(prev) => prev.stronger(fresh),
                None => fresh,
            });
            return;
        }
        let asks: Vec<(String, u64, Ask)> = self
            .channels
            .iter()
            .map(|(id, ch)| match ch {
                Chan::Table(t) => (
                    id.clone(),
                    t.viewer,
                    Ask::Table(t.target.clone(), t.handle.clone()),
                ),
                Chan::Project(p) => (id.clone(), p.registration, Ask::Project(p.project_id)),
            })
            .collect();
        let (shared, done) = (self.shared.clone(), self.done_tx.clone());
        let (sub, sid) = (self.ticket.sub.clone(), self.sid());
        self.reauth_task = Some(TaskGuard::spawn(async move {
            let answers = reauth_work(&shared, (&sub, &sid), &token, fresh, asks).await;
            let _ = done.send(Done::Reauth(answers));
        }));
    }

    async fn apply_reauth(&mut self, answer: ReauthAnswer) {
        let ReauthAnswer {
            id,
            generation,
            outcome,
            view,
            probed: reprobed,
        } = answer;
        let id = id.as_str();
        let Some(ch) = self.channels.get_mut(id) else {
            return;
        };
        let (reauth, was_degraded) = match ch {
            Chan::Table(t) if t.viewer == generation => {
                let was = t.reauth.degraded();
                (&mut t.reauth, was)
            }
            Chan::Project(p) if p.registration == generation => {
                let was = p.reauth.degraded();
                (&mut p.reauth, was)
            }
            // Left and joined again since it was asked: the new channel's own join answered.
            _ => return,
        };
        let mut action = reauth.on_outcome(&outcome);
        if let AuthorizeOutcome::Allowed { tenant_id, .. } = &outcome {
            self.tenants.lock().unwrap().insert(*tenant_id);
        }
        // S16: an allowed table is probed again (privileges or the table itself may have
        // changed underneath the membership); the task did it.
        let mut probed = None;
        if action == ReauthAction::Keep
            && let Some(Chan::Table(_)) = self.channels.get(id)
            && let Some(result) = reprobed
        {
            match result {
                Ok(info) => probed = Some(info),
                Err(JoinRefusal::Forbidden) => action = ReauthAction::Leave(ErrorCode::Forbidden),
                Err(JoinRefusal::NotFound) => action = ReauthAction::Leave(ErrorCode::NotFound),
                Err(JoinRefusal::Invalid(_)) => action = ReauthAction::Leave(ErrorCode::Invalid),
                Err(JoinRefusal::Unavailable) => {
                    if let Some(Chan::Table(t)) = self.channels.get_mut(id) {
                        action = t.reauth.on_outcome(&AuthorizeOutcome::Unavailable);
                    }
                }
            }
        }
        match action {
            ReauthAction::Keep => match self.channels.get(id) {
                Some(Chan::Table(t)) => {
                    let (changed, (live, reason)) = {
                        let mut cell = t.status.lock().unwrap();
                        let before = *cell;
                        if let Some(info) = &probed {
                            *cell = (info.live, info.reason);
                        }
                        (*cell != before || was_degraded, *cell)
                    };
                    let cmds = t.cmds.clone();
                    // The status first: the channel's task sends the presence it held back
                    // after it, once it hears the degradation is over.
                    if changed {
                        self.send(
                            ServerFrame::Status {
                                channel: id.to_string(),
                                live,
                                reason,
                            }
                            .to_text(),
                        );
                    }
                    if was_degraded {
                        let _ = cmds.send(ChannelCmd::Degraded(false));
                    }
                }
                Some(Chan::Project(p)) => {
                    if let Some(v) = view {
                        *p.view.lock().unwrap() = v;
                    }
                }
                None => {}
            },
            ReauthAction::AwaitRenewal => {}
            ReauthAction::Degrade => {
                if !was_degraded && let Some(Chan::Table(t)) = self.channels.get(id) {
                    let _ = t.cmds.send(ChannelCmd::Degraded(true));
                    self.send(
                        ServerFrame::Status {
                            channel: id.to_string(),
                            live: false,
                            reason: Some(StatusReason::Unavailable),
                        }
                        .to_text(),
                    );
                }
            }
            ReauthAction::Leave(code) => {
                let message = match code {
                    ErrorCode::Forbidden => "access revoked",
                    ErrorCode::NotFound => "the channel's target is gone",
                    ErrorCode::Unavailable => "access can't be checked",
                    _ => "the channel can't be watched any more",
                };
                self.error(None, Some(id), code, message);
                if let Some(ch) = self.channels.remove(id) {
                    self.drop_channel(ch).await;
                }
            }
        }
    }
}

/// What one channel's re-authorization asks.
enum Ask {
    Table(AuthTarget, Arc<WatchHandle>),
    Project(Uuid),
}

/// A table join's `/authorize` (a fresh answer for the cycle cache) and watch.
async fn table_join_work(
    shared: &Arc<Shared>,
    (sub, sid): (&str, &str),
    channel: &TableChannel,
    role: &Option<String>,
    token: &Arc<str>,
    target: &AuthTarget,
) -> Result<Box<TableJoined>, (ErrorCode, String)> {
    let outcome = shared.orch.authorize(token, target).await;
    shared.fresh_answer(sub, sid, target, &outcome);
    let (tenant_id, member) = match outcome {
        AuthorizeOutcome::Allowed { tenant_id, member } => (tenant_id, member),
        refusal => {
            let (code, message) = refusal_of(&refusal);
            return Err((code, message.to_string()));
        }
    };
    let (sink, events) = mpsc::channel(SINK_QUEUE);
    let identity = CheckIdentity {
        role: role.clone(),
        token: token.clone(),
    };
    let (handle, info) = shared
        .source
        .watch(channel, &identity, sink)
        .await
        .map_err(|refusal| match refusal {
            JoinRefusal::Forbidden => (ErrorCode::Forbidden, "not allowed".to_string()),
            JoinRefusal::NotFound => (ErrorCode::NotFound, "no such table".to_string()),
            JoinRefusal::Unavailable => (
                ErrorCode::Unavailable,
                "the table can't be watched now".to_string(),
            ),
            JoinRefusal::Invalid(m) => (ErrorCode::Invalid, m),
        })?;
    Ok(Box::new(TableJoined {
        tenant_id,
        member,
        handle,
        info,
        events,
    }))
}

/// A project join's `/projects`.
async fn project_join_work(
    shared: &Arc<Shared>,
    project_id: Uuid,
    token: &Arc<str>,
) -> Result<ProjectView, (ErrorCode, &'static str)> {
    match shared.orch.projects(token).await {
        Ok(v) if v.has_project(project_id) => Ok(v),
        Ok(_) | Err(ProjectsError::Forbidden) => Err((ErrorCode::Forbidden, "not allowed")),
        Err(ProjectsError::TokenExpired) => {
            Err((ErrorCode::Unavailable, "the token expired; renew it"))
        }
        Err(ProjectsError::Unavailable) => {
            Err((ErrorCode::Unavailable, "the projects can't be checked now"))
        }
    }
}

/// Every channel's re-authorization answer (all concurrently), and an allowed table's reprobe.
async fn reauth_work(
    shared: &Arc<Shared>,
    (sub, sid): (&str, &str),
    token: &Arc<str>,
    fresh: Fresh,
    asks: Vec<(String, u64, Ask)>,
) -> Vec<ReauthAnswer> {
    join_all(asks.into_iter().map(|(id, generation, ask)| async move {
        match ask {
            Ask::Table(target, handle) => {
                let outcome = match fresh {
                    Fresh::Cycle(cycle) => {
                        let cached = shared.cycle_cache.lock().unwrap().get(sub, sid, &target, cycle);
                        match cached {
                            Some(o) => o,
                            None => {
                                let o = shared.orch.authorize(token, &target).await;
                                shared
                                    .cycle_cache
                                    .lock()
                                    .unwrap()
                                    .put(sub, sid, &target, cycle, o.clone());
                                o
                            }
                        }
                    }
                    Fresh::Always => {
                        let o = shared.orch.authorize(token, &target).await;
                        shared.fresh_answer(sub, sid, &target, &o);
                        o
                    }
                    Fresh::Burst(burst) => {
                        let answer = shared.cycle_cache.lock().unwrap().burst(
                            sub,
                            sid,
                            &target,
                            burst,
                            || {
                                let (shared, token, target) =
                                    (shared.clone(), token.clone(), target.clone());
                                Box::pin(async move { shared.orch.authorize(&token, &target).await })
                            },
                        );
                        let o = answer.await;
                        shared.fresh_answer(sub, sid, &target, &o);
                        o
                    }
                };
                let probed = match outcome {
                    AuthorizeOutcome::Allowed { .. } => Some(shared.source.reprobe(&handle).await),
                    _ => None,
                };
                ReauthAnswer {
                    id,
                    generation,
                    outcome,
                    view: None,
                    probed,
                }
            }
            Ask::Project(project_id) => {
                let (outcome, view) = match shared.orch.projects(token).await {
                    Ok(view) if view.has_project(project_id) => (
                        AuthorizeOutcome::Allowed {
                            tenant_id: view.tenant_id,
                            member: MemberInfo {
                                sub: sub.to_string(),
                                name: None,
                                email: None,
                            },
                        },
                        Some(view),
                    ),
                    Ok(_) | Err(ProjectsError::Forbidden) => (AuthorizeOutcome::Forbidden, None),
                    Err(ProjectsError::TokenExpired) => (AuthorizeOutcome::TokenExpired, None),
                    Err(ProjectsError::Unavailable) => (AuthorizeOutcome::Unavailable, None),
                };
                ReauthAnswer {
                    id,
                    generation,
                    outcome,
                    view,
                    probed: None,
                }
            }
        }
    }))
    .await
}

/// An `/authorize` refusal as a join's `error`.
fn refusal_of(outcome: &AuthorizeOutcome) -> (ErrorCode, &'static str) {
    match outcome {
        AuthorizeOutcome::Forbidden => (ErrorCode::Forbidden, "not allowed"),
        AuthorizeOutcome::NotFound => (ErrorCode::NotFound, "no such database, branch or role"),
        AuthorizeOutcome::TokenExpired => (ErrorCode::Unavailable, "the token expired; renew it"),
        AuthorizeOutcome::Unavailable | AuthorizeOutcome::Allowed { .. } => {
            (ErrorCode::Unavailable, "access can't be checked now")
        }
    }
}

// --- One table channel of one socket ----------------------------------------------------------

/// Rows, status and presence for one (socket, table): `seq`, the viewer's own visibility of
/// every member's selected row (R15), the setter's check and the presence rate (20/s).
struct TableTask {
    channel: String,
    handle: Arc<WatchHandle>,
    shared: Arc<Shared>,
    out: OutTx,
    hub: Arc<Hub>,
    own: Member,
    throttle: PresenceThrottle,
    /// Every member as the hub has it.
    raw: BTreeMap<String, Member>,
    /// Every member as this viewer was last sent it (blanked where it can't see the row).
    shown: BTreeMap<String, Member>,
    degraded: bool,
    seq: u64,
    status: StatusCell,
}

impl TableTask {
    async fn run(
        mut self,
        mut events: mpsc::Receiver<ChannelEvent>,
        mut hub_rx: mpsc::UnboundedReceiver<HubEvent>,
        mut cmds: mpsc::UnboundedReceiver<ChannelCmd>,
        mut presence: watch::Receiver<Option<TableState>>,
        mut view: watch::Receiver<Option<Vec<String>>>,
    ) {
        let mut events_open = true;
        loop {
            let held = self.throttle.next_slot().map(Instant::from_std);
            let ok = tokio::select! {
                ev = events.recv(), if events_open => match ev {
                    Some(ev) => self.on_event(ev).await,
                    None => { events_open = false; true }
                },
                ev = hub_rx.recv() => match ev {
                    Some(HubEvent::State(members)) => self.on_state(members).await,
                    Some(HubEvent::Diff(diff)) => self.on_diff(diff.leaves, diff.joins).await,
                    None => break,
                },
                cmd = cmds.recv() => match cmd {
                    Some(cmd) => self.on_cmd(cmd).await,
                    None => break,
                },
                Ok(()) = presence.changed() => {
                    // A degradation sent before this frame applies to it first (R20).
                    let mut ok = true;
                    while let Ok(cmd) = cmds.try_recv() {
                        ok = ok && self.on_cmd(cmd).await;
                    }
                    let state = presence.borrow_and_update().clone();
                    match state {
                        Some(state) if ok => self.on_presence(state).await,
                        _ => ok,
                    }
                }
                Ok(()) = view.changed() => {
                    let keys = view.borrow_and_update().clone();
                    if let Some(keys) = keys {
                        self.shared.source.view(&self.handle, keys).await;
                    }
                    true
                }
                _ = sleep_opt(held) => {
                    match self.throttle.take_pending(std::time::Instant::now()) {
                        Some(state) => self.set_state(state).await,
                        None => true,
                    }
                }
            };
            if !ok {
                break;
            }
        }
    }

    async fn send(&self, frame: ServerFrame) -> bool {
        self.out.send(frame.to_text()).await.is_ok()
    }

    /// A `presence` frame's state (latest-wins), through the rate (20/s).
    async fn on_presence(&mut self, state: TableState) -> bool {
        // R20: a degraded channel has no presence, its own included.
        if self.degraded {
            return true;
        }
        match self.throttle.offer(state, std::time::Instant::now()) {
            Some(state) => self.set_state(state).await,
            None => true,
        }
    }

    async fn on_event(&mut self, ev: ChannelEvent) -> bool {
        match ev {
            ChannelEvent::Rows(changes) => {
                self.seq += 1;
                // Spec §4: while the viewer's access can't be checked, no key reaches it; the
                // client refetches as itself instead. (Nobody's presence goes out either, R20.)
                let changes = if self.degraded {
                    vec![Change {
                        op: ChangeOp::Changed,
                        keys: None,
                    }]
                } else {
                    changes
                };
                let touched = touched(&changes);
                let ok = self
                    .send(ServerFrame::Rows {
                        channel: self.channel.clone(),
                        seq: self.seq,
                        changes,
                    })
                    .await;
                ok && self.recheck(&touched).await
            }
            ChannelEvent::Status { live, reason } => {
                *self.status.lock().unwrap() = (live, reason);
                // A degraded channel already says `unavailable`; the source's state follows
                // once it isn't.
                if self.degraded {
                    return true;
                }
                self.send(ServerFrame::Status {
                    channel: self.channel.clone(),
                    live,
                    reason,
                })
                .await
            }
            ChannelEvent::Resync { reason } => {
                self.send(ServerFrame::Resync {
                    channel: self.channel.clone(),
                    reason,
                })
                .await
            }
        }
    }

    /// Each member as this viewer may see it: the state blanked where the viewer's check
    /// identity can't see the row (an error counts as not visible).
    async fn visible_forms(&self, members: &[Member]) -> Vec<Member> {
        let rows: Vec<String> = members
            .iter()
            .filter(|m| m.id != self.own.id)
            .filter_map(|m| m.state.row.clone())
            .collect();
        let seen: HashMap<String, bool> = if rows.is_empty() {
            HashMap::new()
        } else {
            match self.shared.source.visible(&self.handle, &rows).await {
                Ok(v) if v.len() == rows.len() => rows.into_iter().zip(v).collect(),
                _ => HashMap::new(),
            }
        };
        members
            .iter()
            .map(|m| match &m.state.row {
                Some(row) if m.id != self.own.id && !seen.get(row).copied().unwrap_or(false) => {
                    blank(m)
                }
                _ => m.clone(),
            })
            .collect()
    }

    async fn on_state(&mut self, members: Vec<Member>) -> bool {
        self.raw = members.iter().map(|m| (m.id.clone(), m.clone())).collect();
        let forms = self.visible_forms(&members).await;
        self.shown = forms.into_iter().map(|m| (m.id.clone(), m)).collect();
        if self.degraded {
            return true;
        }
        self.send_state().await
    }

    async fn send_state(&self) -> bool {
        self.send(ServerFrame::PresenceState {
            channel: self.channel.clone(),
            members: self.shown.values().cloned().collect(),
        })
        .await
    }

    /// A diff from the hub, filtered for this viewer (an update is a leave plus a join).
    async fn on_diff(&mut self, leaves: Vec<Member>, joins: Vec<Member>) -> bool {
        let rejoined: HashSet<&str> = joins.iter().map(|m| m.id.as_str()).collect();
        let mut out_leaves = vec![];
        for l in &leaves {
            if rejoined.contains(l.id.as_str()) {
                continue;
            }
            self.raw.remove(&l.id);
            if let Some(shown) = self.shown.remove(&l.id) {
                out_leaves.push(shown);
            }
        }
        for j in &joins {
            self.raw.insert(j.id.clone(), j.clone());
        }
        let forms = self.visible_forms(&joins).await;
        self.emit(forms, out_leaves).await
    }

    /// Sends what changed between `forms` and what the viewer has.
    async fn emit(&mut self, forms: Vec<Member>, mut out_leaves: Vec<Member>) -> bool {
        let mut out_joins = vec![];
        for f in forms {
            match self.shown.insert(f.id.clone(), f.clone()) {
                Some(old) if old == f => {}
                Some(old) => {
                    out_leaves.push(old);
                    out_joins.push(f);
                }
                None => out_joins.push(f),
            }
        }
        if self.degraded || (out_joins.is_empty() && out_leaves.is_empty()) {
            return true;
        }
        self.send(ServerFrame::PresenceDiff {
            channel: self.channel.clone(),
            joins: out_joins,
            leaves: out_leaves,
        })
        .await
    }

    /// R15: a rows batch touching a selected key checks those members again (`None` = every
    /// selected member: a `changed`).
    async fn recheck(&mut self, touched: &Option<HashSet<String>>) -> bool {
        let members: Vec<Member> = self
            .raw
            .values()
            .filter(|m| m.id != self.own.id)
            .filter(|m| {
                m.state
                    .row
                    .as_ref()
                    .is_some_and(|row| touched.as_ref().is_none_or(|keys| keys.contains(row)))
            })
            .cloned()
            .collect();
        if members.is_empty() {
            return true;
        }
        let forms = self.visible_forms(&members).await;
        self.emit(forms, vec![]).await
    }

    async fn on_cmd(&mut self, cmd: ChannelCmd) -> bool {
        match cmd {
            ChannelCmd::Degraded(on) => {
                if on == self.degraded {
                    return true;
                }
                self.degraded = on;
                if on { true } else { self.send_state().await }
            }
        }
    }

    /// The setter's own check (R15), then the new state to every viewer.
    async fn set_state(&mut self, state: TableState) -> bool {
        if let Some(row) = &state.row {
            let visible = self
                .shared
                .source
                .visible(&self.handle, std::slice::from_ref(row))
                .await;
            if !matches!(visible.as_deref(), Ok([true])) {
                return self
                    .send(ServerFrame::Error {
                        r#ref: None,
                        channel: Some(self.channel.clone()),
                        code: ErrorCode::Invalid,
                        message: "the row isn't visible to you".into(),
                    })
                    .await;
            }
        }
        self.own.state = state;
        if let Some(msg) = self.hub.update_local(&self.own) {
            self.hub.publish(&msg).await;
        }
        true
    }
}

/// The keys a batch touches; `None` when one change has no keys (`changed`, a keyless delete).
fn touched(changes: &[Change]) -> Option<HashSet<String>> {
    let mut keys = HashSet::new();
    for c in changes {
        keys.extend(c.keys.as_ref()?.iter().cloned());
    }
    Some(keys)
}

// --- One project channel of one socket --------------------------------------------------------

async fn project_task(
    shared: Arc<Shared>,
    channel: String,
    project_id: Uuid,
    view: Arc<Mutex<ProjectView>>,
    mut events: mpsc::UnboundedReceiver<ProjectEvent>,
    token: watch::Receiver<Arc<str>>,
    out: OutTx,
) {
    let mut last_refresh: Option<Instant> = None;
    while let Some(ev) = events.recv().await {
        let current = view.lock().unwrap().clone();
        let deliver = match route(&current, project_id, &ev, false) {
            Delivery::Deliver => true,
            Delivery::Drop => false,
            Delivery::Refresh => {
                // R21: refreshed first, delivered only if the fresh list has the database. At
                // most one refresh per gap: a second unknown database waits for the gap's end
                // (the events behind it queue), it isn't dropped.
                if let Some(t) = last_refresh {
                    tokio::time::sleep_until(t + PROJECT_REFRESH_GAP).await;
                }
                // The list may have been refreshed meanwhile (a re-authorization).
                let current = view.lock().unwrap().clone();
                match route(&current, project_id, &ev, false) {
                    Delivery::Deliver => true,
                    Delivery::Drop => false,
                    Delivery::Refresh => {
                        last_refresh = Some(Instant::now());
                        let t = token.borrow().clone();
                        match shared.orch.projects(&t).await {
                            Ok(fresh) => {
                                let d = route(&fresh, project_id, &ev, true) == Delivery::Deliver;
                                *view.lock().unwrap() = fresh;
                                d
                            }
                            Err(_) => false,
                        }
                    }
                }
            }
        };
        if deliver {
            let frame = ServerFrame::Project {
                channel: channel.clone(),
                event: ev,
            };
            if out.send(frame.to_text()).await.is_err() {
                break;
            }
        }
    }
}
