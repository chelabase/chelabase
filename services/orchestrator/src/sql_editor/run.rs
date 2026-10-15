//! One script run on a branch's database.
//!
//! [`connect`] opens a fresh connection (as the owner, or as the caller's member role for an
//! act-as run); [`run`] sends the script in one
//! simple-protocol query (so multi-statement scripts and explicit transactions work) and turns
//! the server's messages into [`RunEvent`]s. The run task sends Postgres's cancel request
//! whenever it stops early: cancelled, deadline passed, response cap hit or client gone.

use std::fmt;
use std::net::SocketAddr;
use std::pin::{Pin, pin};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use chelabase_types::role_name::quote_ident;
use futures_util::{Stream, StreamExt, stream};
use tokio::sync::mpsc;
use tokio::time::{Instant, sleep_until};
use tokio_postgres::error::{DbError, ErrorPosition, SqlState};
use tokio_postgres::config::{ChannelBinding, SslMode};
use tokio_postgres::tls::TlsStream;
use tokio_postgres::{AsyncMessage, Client, Connection, NoTls, Socket, SimpleQueryMessage};
use chelabase_neon::compute_tls::ClientTls;
use tokio_util::sync::CancellationToken;

use super::events::{ColumnInfo, Outcome, RunEvent};
use super::registry::RunGuard;
use super::{
    APPLICATION_NAME, HEARTBEAT, MAX_RESPONSE_BYTES, MAX_ROWS_PER_STATEMENT, ROWS_PER_EVENT,
};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Events buffered between the run task and the response body.
const EVENT_BUFFER: usize = 64;
/// Notices queued between the connection task and the run task. When the queue is full the
/// connection task stops reading the socket until the run task catches up (backpressure), so
/// a notice flood against a slow reader can't grow the orchestrator's memory.
const NOTICE_BUFFER: usize = 256;
/// How early a server-side `query_canceled` may arrive and still count as the deadline.
const TIMEOUT_SLACK: Duration = Duration::from_millis(50);
/// SQLSTATE for errors that aren't the server's (the connection broke).
const CONNECTION_FAILURE: &str = "08006";

/// The built-in types whose names a `columns` event carries, by OID (with their arrays).
const TYPE_NAMES: &[(u32, &str)] = &[
    (16, "bool"),
    (17, "bytea"),
    (19, "name"),
    (20, "int8"),
    (21, "int2"),
    (23, "int4"),
    (25, "text"),
    (26, "oid"),
    (114, "json"),
    (700, "float4"),
    (701, "float8"),
    (1042, "bpchar"),
    (1043, "varchar"),
    (1082, "date"),
    (1083, "time"),
    (1114, "timestamp"),
    (1184, "timestamptz"),
    (1186, "interval"),
    (1266, "timetz"),
    (1700, "numeric"),
    (2950, "uuid"),
    (3802, "jsonb"),
    (199, "_json"),
    (1000, "_bool"),
    (1001, "_bytea"),
    (1003, "_name"),
    (1005, "_int2"),
    (1007, "_int4"),
    (1009, "_text"),
    (1014, "_bpchar"),
    (1015, "_varchar"),
    (1016, "_int8"),
    (1021, "_float4"),
    (1022, "_float8"),
    (1028, "_oid"),
    (1115, "_timestamp"),
    (1182, "_date"),
    (1183, "_time"),
    (1185, "_timestamptz"),
    (1187, "_interval"),
    (1231, "_numeric"),
    (1270, "_timetz"),
    (2951, "_uuid"),
    (3807, "_jsonb"),
];

/// A column type's name for the `columns` event: built-ins only, `""` for any other OID.
pub fn type_name(oid: u32) -> &'static str {
    TYPE_NAMES
        .iter()
        .find(|(o, _)| *o == oid)
        .map_or("", |(_, name)| name)
}

/// The row count at the end of a command tag (`SELECT 3` → 3, `INSERT 0 5` → 5), or 0.
pub fn tag_rows(tag: &str) -> u64 {
    tag.rsplit(' ')
        .next()
        .and_then(|n| n.parse().ok())
        .unwrap_or(0)
}

/// Where to run: the compute's address, the credentials (the owner's or the member role's) and
/// the Postgres database.
pub struct RunTarget {
    pub addr: SocketAddr,
    pub user: String,
    pub password: String,
    pub database: String,
    pub tls: RunTls,
}

/// How a connection to the compute is secured.
#[derive(Clone)]
pub enum RunTls {
    /// No TLS (the metadata database in tests).
    Plain,
    /// TLS to a compute on the control network: `host` is the compute's certificate name,
    /// `hostaddr` its control address; the server certificate is checked against our CA and
    /// `client`'s pair is presented (the compute requires it for member logins).
    Compute {
        server_name: String,
        client: Arc<ClientTls>,
    },
}

/// What to run.
pub struct RunSpec {
    pub sql: String,
    /// `SET ROLE` to this role first; `None` runs as whoever connected (the owner).
    pub role: Option<String>,
    pub timeout: Duration,
    /// Awaited once the run is over, however it ended (finished, failed, cancelled, timed out
    /// or its client gone), after `done` was sent and the connection closed. An act-as run's
    /// hand-over (R5: the worker's `member_hand_over_sql`, which ends this backend first) goes
    /// here, so nothing runs in the script's own session.
    pub after: Option<AfterRun>,
}

/// The run itself, without what comes after it.
struct Script {
    sql: String,
    role: Option<String>,
    timeout: Duration,
}

/// Work for after a run ([`RunSpec::after`]).
pub type AfterRun = Pin<Box<dyn Future<Output = ()> + Send>>;

/// An open editor connection: the client, the notices its connection task receives and its
/// backend's process id.
pub struct Connected {
    client: Client,
    tls: RunTls,
    notices: mpsc::Receiver<DbError>,
    pid: i32,
    started: String,
}

impl Connected {
    /// The connection's client (the schema read runs on it before a run would).
    pub fn client(&self) -> &Client {
        &self.client
    }

    /// The backend's process id (`pg_backend_pid()`), for ending it after an act-as run.
    pub fn backend_pid(&self) -> i32 {
        self.pid
    }

    /// The backend's `backend_start` (text): with the pid, it names this backend only.
    pub fn backend_start(&self) -> &str {
        &self.started
    }

    /// The client and the queue of notices its connection receives, in wire order. Once a
    /// query's future has resolved, every notice the server sent before its result is queued
    /// (the connection task queues a notice before it reads the next message). The queue is
    /// bounded: a caller awaiting a query must keep taking notices meanwhile, or a flood of
    /// them stalls the connection.
    pub fn client_and_notices(&mut self) -> (&Client, &mut mpsc::Receiver<DbError>) {
        (&self.client, &mut self.notices)
    }
}

/// Why [`connect`] failed.
#[derive(Debug)]
pub enum ConnectError {
    /// A failure before the server answered: the TLS handshake, the certificate check, or no
    /// client pair loaded.
    Tls(String),
    /// No answer from the compute (refused, timed out, broken).
    Unreachable(String),
    /// The server answered with an error (authentication, missing database, ...): its
    /// SQLSTATE and message.
    Rejected { code: String, message: String },
}

impl fmt::Display for ConnectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConnectError::Tls(m) => write!(f, "the TLS connection to the compute failed: {m}"),
            ConnectError::Unreachable(m) => write!(f, "the compute can't be reached: {m}"),
            ConnectError::Rejected { code, message } => {
                write!(f, "the compute refused the connection ({code}): {message}")
            }
        }
    }
}

impl std::error::Error for ConnectError {}

/// A fresh connection (R6), TLS per `target.tls`, `application_name` = [`APPLICATION_NAME`],
/// connect timeout 5 s.
pub async fn connect(target: &RunTarget) -> Result<Connected, ConnectError> {
    let mut config = tokio_postgres::Config::new();
    config
        .port(target.addr.port())
        .user(&target.user)
        .password(&target.password)
        .dbname(&target.database)
        .application_name(APPLICATION_NAME)
        .connect_timeout(CONNECT_TIMEOUT);
    match &target.tls {
        RunTls::Plain => {
            config.host(target.addr.ip().to_string());
            let (client, connection) = config
                .connect(NoTls)
                .await
                .map_err(|err| connect_error(&err, &target.tls))?;
            finish(client, connection, target.tls.clone()).await
        }
        RunTls::Compute {
            server_name,
            client: tls,
        } => {
            config
                .host(server_name)
                .hostaddr(target.addr.ip())
                .ssl_mode(SslMode::Require)
                .channel_binding(ChannelBinding::Require);
            let (client, connection) = config
                .connect(tls.connector())
                .await
                .map_err(|err| connect_error(&err, &target.tls))?;
            finish(client, connection, target.tls.clone()).await
        }
    }
}

/// The error text with its sources (rustls' reason sits in the source chain).
fn error_chain(err: &(dyn std::error::Error + 'static)) -> String {
    let mut text = err.to_string();
    let mut source = err.source();
    while let Some(next) = source {
        text.push_str(": ");
        text.push_str(&next.to_string());
        source = next.source();
    }
    text
}

/// The `rustls::Error` under `err`, if any: anywhere in its source chain, including inside an
/// `io::Error` (whose `source` skips the error it wraps, so `get_ref` is read too).
fn rustls_error<'a>(err: &'a (dyn std::error::Error + 'static)) -> Option<&'a rustls::Error> {
    if let Some(found) = err.downcast_ref::<rustls::Error>() {
        return Some(found);
    }
    if let Some(inner) = err
        .downcast_ref::<std::io::Error>()
        .and_then(std::io::Error::get_ref)
        && let Some(found) = rustls_error(inner)
    {
        return Some(found);
    }
    err.source().and_then(rustls_error)
}

/// Whether `err` is a plain I/O failure (an `io::Error` with no TLS error in it) under a
/// `Kind::Tls` error: the connection was reset or closed during the handshake.
fn is_plain_io(err: &tokio_postgres::Error) -> bool {
    use std::error::Error as _;
    err.source()
        .is_some_and(|cause| cause.downcast_ref::<std::io::Error>().is_some())
        && rustls_error(err).is_none()
}

/// Classifies a failed connect by type, never by message text (fork round 3, T10 I2/m4, U9).
///
/// Invariant: on a compute connection ([`RunTls::Compute`]) it is [`ConnectError::Tls`] iff the
/// TLS layer or the compute's post-password certificate check refused it:
/// - TLS setup failed (`Error::is_tls`), except a plain reset or close during the handshake;
/// - a `rustls::Error` anywhere under it (a TLS 1.3 alert such as `unknown_ca` arrives on the
///   first read after the handshake, as an I/O error);
/// - channel binding was refused (`Error::is_channel_binding_refused`);
/// - 28P01 after the server's SCRAM final message verified (`Error::sasl_verified`): the
///   password was right, so the refusal is `clientcert=verify-full`'s (a CN mismatch).
///
/// Any other server error is [`ConnectError::Rejected`] (a wrong password, a stale role),
/// everything else [`ConnectError::Unreachable`]. A plain connection's failures are never `Tls`.
pub(crate) fn connect_error(err: &tokio_postgres::Error, tls: &RunTls) -> ConnectError {
    let compute = matches!(tls, RunTls::Compute { .. });
    if let Some(db) = err.as_db_error() {
        if compute && err.sasl_verified() && *db.code() == SqlState::INVALID_PASSWORD {
            return ConnectError::Tls(format!(
                "the compute refused the connection after the password verified, at its \
                 client certificate check ({}): {}",
                db.code().code(),
                db.message()
            ));
        }
        return ConnectError::Rejected {
            code: db.code().code().to_string(),
            message: db.message().to_string(),
        };
    }
    let text = error_chain(err);
    let tls_failure = compute
        && ((err.is_tls() && !is_plain_io(err))
            || rustls_error(err).is_some()
            || err.is_channel_binding_refused());
    if tls_failure {
        // rustls' reason goes to the log, whatever its wrapper's words.
        match rustls_error(err) {
            Some(reason) if !text.contains(&reason.to_string()) => {
                ConnectError::Tls(format!("{text}: {reason}"))
            }
            _ => ConnectError::Tls(text),
        }
    } else {
        ConnectError::Unreachable(text)
    }
}

async fn finish<S>(
    client: Client,
    mut connection: Connection<Socket, S>,
    tls: RunTls,
) -> Result<Connected, ConnectError>
where
    S: TlsStream + Unpin + Send + 'static,
{
    let (tx, notices) = mpsc::channel(NOTICE_BUFFER);
    tokio::spawn(async move {
        let mut messages = stream::poll_fn(move |cx| connection.poll_message(cx));
        // The first error is terminal (`poll_message`'s contract): the connection is done.
        while let Some(Ok(message)) = messages.next().await {
            if let AsyncMessage::Notice(notice) = message {
                // Waits while the queue is full; fails (and drops the notice) once the run
                // task let go of the receiver.
                let _ = tx.send(notice).await;
            }
        }
    });
    let backend = backend_of(&client)
        .await
        .map_err(|err| connect_error(&err, &tls))?;
    let (pid, started) = parse_backend(backend).ok_or_else(|| {
        ConnectError::Unreachable("the server didn't say which backend this is".into())
    })?;
    Ok(Connected { client, tls, notices, pid, started })
}

/// This connection's backend: its pid and `backend_start`, as the server sent them.
async fn backend_of(client: &Client) -> Result<Option<(String, String)>, tokio_postgres::Error> {
    let sql = "SELECT pg_catalog.pg_backend_pid()::text, s.backend_start::text \
               FROM pg_catalog.pg_stat_activity s WHERE s.pid = pg_catalog.pg_backend_pid()";
    for message in client.simple_query(sql).await? {
        if let SimpleQueryMessage::Row(row) = message
            && let (Some(pid), Some(started)) = (row.get(0), row.get(1))
        {
            return Ok(Some((pid.to_owned(), started.to_owned())));
        }
    }
    Ok(None)
}

/// M-a: a pid above 0 and a start time, or `None` (no usable backend: an error, never pid 0).
fn parse_backend(backend: Option<(String, String)>) -> Option<(i32, String)> {
    let (pid, started) = backend?;
    let pid: i32 = pid.parse().ok().filter(|p| *p > 0)?;
    (!started.is_empty()).then_some((pid, started))
}

/// How a run ended (`done`'s outcome and duration), set before `done` is sent: the audit's
/// record even when a slow reader never takes `done`. Unset when the client went first.
pub type Finished = Arc<OnceLock<(Outcome, u64)>>;

/// Runs `spec` and yields its events; always ends with `Done`. Dropping the stream cancels the run.
pub fn run(
    connected: Connected,
    spec: RunSpec,
    guard: RunGuard,
) -> impl Stream<Item = RunEvent> + Send + 'static {
    run_reporting(connected, spec, guard, Finished::default())
}

/// [`run`], recording how it ended in `finished`.
pub fn run_reporting(
    connected: Connected,
    spec: RunSpec,
    guard: RunGuard,
    finished: Finished,
) -> impl Stream<Item = RunEvent> + Send + 'static {
    let (tx, rx) = mpsc::channel(EVENT_BUFFER);
    tokio::spawn(drive(connected, spec, guard, tx, finished));
    stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|e| (e, rx)) })
}

/// Why a run stopped before the server finished it.
enum Stop {
    Gone,
    Cancelled,
    Timeout,
    Cap,
}

/// Sends events within the response cap, watching the token and the deadline.
struct Emitter {
    tx: mpsc::Sender<RunEvent>,
    token: CancellationToken,
    deadline: Instant,
    bytes: usize,
    last: Instant,
}

impl Emitter {
    async fn send(&mut self, event: RunEvent) -> Result<(), Stop> {
        let len = event.to_line().len();
        if self.bytes + len > MAX_RESPONSE_BYTES {
            return Err(Stop::Cap);
        }
        tokio::select! {
            biased;
            _ = self.token.cancelled() => Err(Stop::Cancelled),
            _ = sleep_until(self.deadline) => Err(Stop::Timeout),
            sent = self.tx.send(event) => {
                sent.map_err(|_| Stop::Gone)?;
                self.bytes += len;
                self.last = Instant::now();
                Ok(())
            }
        }
    }

    /// Waits for `future` unless the run is cancelled, times out or loses its client first.
    async fn guarded<T>(&self, future: impl Future<Output = T>) -> Result<T, Stop> {
        tokio::select! {
            biased;
            _ = self.token.cancelled() => Err(Stop::Cancelled),
            _ = sleep_until(self.deadline) => Err(Stop::Timeout),
            _ = self.tx.closed() => Err(Stop::Gone),
            value = future => Ok(value),
        }
    }
}

async fn drive(
    connected: Connected,
    spec: RunSpec,
    guard: RunGuard,
    tx: mpsc::Sender<RunEvent>,
    finished: Finished,
) {
    let started = Instant::now();
    let Connected {
        client,
        tls,
        mut notices,
        ..
    } = connected;
    let RunSpec {
        sql,
        role,
        timeout,
        after,
    } = spec;
    let spec = Script { sql, role, timeout };
    let mut out = Emitter {
        tx,
        token: guard.token(),
        deadline: started + spec.timeout,
        bytes: 0,
        last: started,
    };
    let outcome = match execute(&client, &mut notices, &spec, &mut out).await {
        Ok(outcome) => Some(outcome),
        Err(stop) => {
            // The backend may still be busy: stop it before the connection goes.
            let cancel = client.cancel_token();
            let _ = match &tls {
                RunTls::Plain => {
                    tokio::time::timeout(CONNECT_TIMEOUT, cancel.cancel_query(NoTls)).await
                }
                RunTls::Compute { client: tls, .. } => {
                    tokio::time::timeout(CONNECT_TIMEOUT, cancel.cancel_query(tls.connector()))
                        .await
                }
            };
            match stop {
                Stop::Gone => None,
                Stop::Cancelled => Some(Outcome::Cancelled),
                Stop::Timeout => Some(Outcome::Timeout),
                Stop::Cap => Some(Outcome::TruncatedResponse),
            }
        }
    };
    let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    // Free the connection and the registry slot first: a client that stopped reading must
    // not pin them.
    drop(client);
    drop(notices);
    drop(guard);
    if let Some(outcome) = outcome {
        let _ = finished.set((outcome, duration_ms));
        let done = RunEvent::Done {
            outcome,
            duration_ms,
        };
        // `done` goes out past the cap, but only waits so long for a slow reader.
        let _ = tokio::time::timeout(HEARTBEAT, out.tx.send(done)).await;
    }
    drop(out);
    // R5: however the run ended (its client gone too), once `done` is out.
    if let Some(after) = after {
        after.await;
    }
}

enum Next {
    Message(Option<Result<SimpleQueryMessage, tokio_postgres::Error>>),
    Notice(Box<DbError>),
    Heartbeat,
}

async fn execute(
    client: &Client,
    notices: &mut mpsc::Receiver<DbError>,
    spec: &Script,
    out: &mut Emitter,
) -> Result<Outcome, Stop> {
    let mut setup = vec![format!(
        "SET statement_timeout = {}",
        spec.timeout.as_millis()
    )];
    if let Some(role) = &spec.role {
        setup.push(format!("SET ROLE {}", quote_ident(role)));
    }
    for sql in setup {
        let result = out.guarded(client.simple_query(&sql)).await?;
        drain_notices(notices, out).await?;
        if let Err(err) = result {
            out.send(error_event(0, &err)).await?;
            return Ok(Outcome::Error);
        }
    }

    let stream = match out.guarded(client.simple_query_raw(&spec.sql)).await? {
        Ok(stream) => stream.with_command_tags(),
        Err(err) => {
            out.send(error_event(0, &err)).await?;
            return Ok(Outcome::Error);
        }
    };
    let mut stream = pin!(stream);
    let mut statement = 0usize;
    let mut seen = 0usize;
    let mut buffer = RowBuffer::default();
    loop {
        let beat = out.last + HEARTBEAT;
        let next = tokio::select! {
            biased;
            _ = out.token.cancelled() => return Err(Stop::Cancelled),
            _ = sleep_until(out.deadline) => return Err(Stop::Timeout),
            _ = out.tx.closed() => return Err(Stop::Gone),
            Some(notice) = notices.recv() => Next::Notice(Box::new(notice)),
            message = stream.next() => Next::Message(message),
            _ = sleep_until(beat) => Next::Heartbeat,
        };
        let message = match next {
            Next::Notice(notice) => {
                out.send(notice_event(&notice)).await?;
                continue;
            }
            Next::Heartbeat => {
                out.send(RunEvent::Heartbeat).await?;
                continue;
            }
            Next::Message(message) => message,
        };
        // Notices the server sent before this message are already queued: keep wire order.
        drain_notices(notices, out).await?;
        match message {
            None => return Ok(Outcome::Ok),
            Some(Err(err)) => {
                if is_statement_timeout(&err, out.deadline) {
                    return Err(Stop::Timeout);
                }
                flush(&mut buffer, statement, out).await?;
                out.send(error_event(statement, &err)).await?;
                return Ok(Outcome::Error);
            }
            Some(Ok(SimpleQueryMessage::RowDescription(columns))) => {
                seen = 0;
                let columns = columns
                    .iter()
                    .map(|c| ColumnInfo {
                        name: c.name().to_string(),
                        type_name: type_name(c.type_oid()).to_string(),
                    })
                    .collect();
                out.send(RunEvent::Columns { statement, columns }).await?;
            }
            Some(Ok(SimpleQueryMessage::Row(row))) => {
                if seen < MAX_ROWS_PER_STATEMENT {
                    let row: Vec<Option<String>> = (0..row.len())
                        .map(|i| row.get(i).map(str::to_owned))
                        .collect();
                    // Send what fits before this row would carry the buffer past the cap.
                    if buffer.would_overflow(out.bytes, row_cost(&row)) {
                        flush(&mut buffer, statement, out).await?;
                    }
                    buffer.push(row);
                    if buffer.full(out.bytes) {
                        flush(&mut buffer, statement, out).await?;
                    }
                }
                seen += 1;
            }
            Some(Ok(SimpleQueryMessage::CommandTag(tag))) => {
                flush(&mut buffer, statement, out).await?;
                out.send(RunEvent::Complete {
                    statement,
                    tag: tag.tag().to_string(),
                    rows: tag_rows(tag.tag()),
                    truncated: seen > MAX_ROWS_PER_STATEMENT,
                })
                .await?;
                statement += 1;
                seen = 0;
            }
            Some(Ok(SimpleQueryMessage::EmptyQuery)) => statement += 1,
            // `CommandComplete` doesn't occur with command tags on.
            Some(Ok(_)) => {}
        }
    }
}

async fn drain_notices(
    notices: &mut mpsc::Receiver<DbError>,
    out: &mut Emitter,
) -> Result<(), Stop> {
    while let Ok(notice) = notices.try_recv() {
        out.send(notice_event(&notice)).await?;
    }
    Ok(())
}

/// A lower bound of `row`'s size in a `rows` event's JSON: `[`, `]`, the commas, and each
/// value as `null` or its text in quotes (escaping only adds to it).
fn row_cost(row: &[Option<String>]) -> usize {
    let values: usize = row
        .iter()
        .map(|v| v.as_ref().map_or(4, |text| text.len() + 2))
        .sum();
    2 + values + row.len().saturating_sub(1)
}

/// The rows waiting for their `rows` event, with a lower bound of their JSON size, so the
/// response cap bounds the orchestrator's memory too: a flush that would pass the cap stops
/// the run, at most one row past it.
#[derive(Default)]
struct RowBuffer {
    rows: Vec<Vec<Option<String>>>,
    bytes: usize,
}

impl RowBuffer {
    /// Whether a row of `cost` would carry the rows buffered past the cap, given `sent`
    /// bytes already sent: flush first. An empty buffer always takes the row.
    fn would_overflow(&self, sent: usize, cost: usize) -> bool {
        !self.rows.is_empty() && sent + self.bytes + cost > MAX_RESPONSE_BYTES
    }

    fn push(&mut self, row: Vec<Option<String>>) {
        self.bytes += row_cost(&row);
        self.rows.push(row);
    }

    /// Whether to flush now: an event's worth of rows, or past the cap (the send then stops
    /// the run with `truncated_response`).
    fn full(&self, sent: usize) -> bool {
        self.rows.len() >= ROWS_PER_EVENT || sent + self.bytes > MAX_RESPONSE_BYTES
    }

    fn take(&mut self) -> Vec<Vec<Option<String>>> {
        self.bytes = 0;
        std::mem::take(&mut self.rows)
    }
}

async fn flush(buffer: &mut RowBuffer, statement: usize, out: &mut Emitter) -> Result<(), Stop> {
    if buffer.rows.is_empty() {
        return Ok(());
    }
    let rows = buffer.take();
    out.send(RunEvent::Rows { statement, rows }).await
}

/// The server's own statement timeout (set to the run's timeout) counts as the deadline: a
/// `query_canceled` at or near the deadline is a timeout, one before it (another session's
/// `pg_cancel_backend`) stays an error. Locale-independent: the message text isn't read.
fn is_statement_timeout(err: &tokio_postgres::Error, deadline: Instant) -> bool {
    err.code() == Some(&SqlState::QUERY_CANCELED) && Instant::now() + TIMEOUT_SLACK >= deadline
}

fn original_position(db: &DbError) -> Option<u32> {
    match db.position() {
        Some(ErrorPosition::Original(n)) => Some(*n),
        _ => None,
    }
}

fn notice_event(notice: &DbError) -> RunEvent {
    RunEvent::Notice {
        severity: notice.severity().to_string(),
        code: notice.code().code().to_string(),
        message: notice.message().to_string(),
        detail: notice.detail().map(str::to_owned),
        hint: notice.hint().map(str::to_owned),
        position: original_position(notice),
    }
}

fn error_event(statement: usize, err: &tokio_postgres::Error) -> RunEvent {
    match err.as_db_error() {
        Some(db) => RunEvent::Error {
            statement,
            code: db.code().code().to_string(),
            message: db.message().to_string(),
            detail: db.detail().map(str::to_owned),
            hint: db.hint().map(str::to_owned),
            position: original_position(db),
        },
        None => RunEvent::Error {
            statement,
            code: CONNECTION_FAILURE.to_string(),
            message: err.to_string(),
            detail: None,
            hint: None,
            position: None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql_editor::fake_pg::{self, Want};

    fn is(want: Want, got: &ConnectError) -> bool {
        match (want, got) {
            (Want::Tls, ConnectError::Tls(_)) => true,
            (Want::Unreachable, ConnectError::Unreachable(_)) => true,
            (Want::Rejected(want), ConnectError::Rejected { code, .. }) => want == code,
            _ => false,
        }
    }

    // T10 I2/m4, U9 (fork round 3, final fix round). Invariant: a compute connection failure
    // is `ConnectError::Tls` iff the TLS layer or the post-password certificate check refused
    // it, judged by type (`Error::is_tls`, a `rustls::Error` anywhere under it, including
    // inside an `io::Error`, `Error::is_channel_binding_refused`, a 28P01 after
    // `Error::sasl_verified`), never by message text; a plain connection's failures are never
    // `Tls`. Exhaustive over every error kind `connect` returns (`fake_pg::TABLE`).
    #[tokio::test]
    async fn connect_errors_are_classified_by_type() {
        let compute = fake_pg::compute_tls();
        for &(case, on_compute, on_plain) in fake_pg::TABLE {
            for (tls, want, label) in [
                (&compute, on_compute, "compute"),
                (&RunTls::Plain, on_plain, "plain"),
            ] {
                let err = case.error().await;
                let got = connect_error(&err, tls);
                assert!(is(want, &got), "{case:?} on a {label} connection: want {want:?}, got {got:?}");
                // The reason goes to the log: a typed TLS failure keeps rustls' words.
                if let (ConnectError::Tls(message), Some(reason)) = (&got, fake_pg::tls_reason(case))
                {
                    assert!(message.contains(reason), "{case:?}: {message}");
                }
            }
        }
    }

    // m4: a reset is a reset, during the handshake or after it, whatever the words say.
    #[tokio::test]
    async fn a_plain_reset_is_never_a_tls_failure() {
        let compute = fake_pg::compute_tls();
        for case in [
            fake_pg::Case::HandshakeReset,
            fake_pg::Case::ResetAfterHandshake,
        ] {
            let got = connect_error(&case.error().await, &compute);
            assert!(matches!(got, ConnectError::Unreachable(_)), "{case:?}: {got:?}");
        }
    }

    /// A wrapper error with a source, as a connector adds context.
    #[derive(Debug)]
    struct Wrapped(Box<dyn std::error::Error + Send + Sync>);

    impl fmt::Display for Wrapped {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("wrapped")
        }
    }

    impl std::error::Error for Wrapped {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            Some(&*self.0)
        }
    }

    // An `io::Error`'s `source` skips the error it wraps, so a rustls error behind two of
    // them (with a wrapper between) is found only through `get_ref`; a plain I/O error at the
    // same depth isn't one.
    #[test]
    fn rustls_errors_are_found_behind_nested_io_errors() {
        let alert = rustls::Error::AlertReceived(rustls::AlertDescription::UnknownCA);
        let deep = std::io::Error::other(Wrapped(Box::new(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            alert.clone(),
        ))));
        assert_eq!(rustls_error(&deep), Some(&alert));
        let plain = std::io::Error::other(Wrapped(Box::new(std::io::Error::from(
            std::io::ErrorKind::ConnectionReset,
        ))));
        assert_eq!(rustls_error(&plain), None);
    }

    #[test]
    fn row_cost_is_a_lower_bound_of_the_json() {
        let rows: Vec<Vec<Option<String>>> = vec![
            vec![],
            vec![None],
            vec![Some(String::new())],
            vec![Some("abc".into()), None, Some("12".into())],
            vec![Some("quote \" back \\ nl \n nul \u{0}".into())],
            vec![Some("é ✓ 😀".into()), None],
        ];
        for row in &rows {
            let json = serde_json::to_string(row).unwrap().len();
            assert!(row_cost(row) <= json, "{row:?}: {} > {json}", row_cost(row));
        }
        // Exact for plain text.
        let plain = vec![Some("abc".into()), None, Some("12".into())];
        assert_eq!(row_cost(&plain), serde_json::to_string(&plain).unwrap().len());
    }

    #[test]
    fn row_buffer_flushes_before_the_cap() {
        let third = vec![Some("x".repeat(MAX_RESPONSE_BYTES / 3 - 4))];
        let cost = row_cost(&third);
        let mut buffer = RowBuffer::default();
        for _ in 0..3 {
            assert!(!buffer.would_overflow(0, cost));
            buffer.push(third.clone());
            assert!(!buffer.full(0));
        }
        // A fourth would carry the buffer past the cap: flush the three first.
        assert!(buffer.would_overflow(0, cost));
        // What was sent already counts.
        assert!(buffer.full(MAX_RESPONSE_BYTES - cost));
        assert_eq!(buffer.take().len(), 3);
        assert_eq!(buffer.bytes, 0);
        // An empty buffer takes any row; one row past the cap is full at once.
        let huge = vec![Some("x".repeat(MAX_RESPONSE_BYTES + 1))];
        assert!(!buffer.would_overflow(0, row_cost(&huge)));
        buffer.push(huge);
        assert!(buffer.full(0));
        buffer.take();
        // 200 small rows fill an event.
        for _ in 0..ROWS_PER_EVENT {
            assert!(!buffer.full(0));
            buffer.push(vec![Some("1".into())]);
        }
        assert!(buffer.full(0));
    }

    // M-a: the hand-over ends this backend by pid and start; pid 0, a bad pid or no start is
    // an error, never a backend to end.
    #[test]
    fn a_backend_needs_a_pid_and_a_start() {
        let b = |pid: &str, started: &str| Some((pid.to_string(), started.to_string()));
        assert_eq!(parse_backend(b("42", "2026-10-09 10:00:00+00")), Some((42, "2026-10-09 10:00:00+00".into())));
        for backend in [None, b("0", "t"), b("-1", "t"), b("x", "t"), b("42", "")] {
            assert_eq!(parse_backend(backend.clone()), None, "{backend:?}");
        }
    }

    #[test]
    fn tag_row_counts() {
        assert_eq!(tag_rows("SELECT 3"), 3);
        assert_eq!(tag_rows("INSERT 0 5"), 5);
        assert_eq!(tag_rows("CREATE TABLE"), 0);
        assert_eq!(tag_rows(""), 0);
    }

    #[test]
    fn type_names_cover_the_builtins_only() {
        use tokio_postgres::types::Type;
        let listed = [
            "int2",
            "int4",
            "int8",
            "numeric",
            "float4",
            "float8",
            "bool",
            "text",
            "varchar",
            "bpchar",
            "name",
            "uuid",
            "date",
            "time",
            "timetz",
            "timestamp",
            "timestamptz",
            "interval",
            "json",
            "jsonb",
            "bytea",
            "oid",
        ];
        for oid in 0..10_000u32 {
            let expected = Type::from_oid(oid)
                .map(|t| t.name().to_string())
                .filter(|n| listed.contains(&n.trim_start_matches('_')))
                .unwrap_or_default();
            assert_eq!(type_name(oid), expected, "oid {oid}");
        }
        assert_eq!(type_name(23), "int4");
        assert_eq!(type_name(1007), "_int4");
        assert_eq!(type_name(600), ""); // point
    }
}
