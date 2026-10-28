//! The service: its dependencies, its two listeners and its handle.
//!
//! - [`build`]: production dependencies from the [`Config`] (the gateway's verify key file,
//!   OIDC, NATS, Valkey; T8's change source), then [`build_with`].
//! - [`Deps::connect`]: the same dependencies with a given identity verifier (the live tests
//!   sign their own identities: the runner has no gateway key).
//! - [`build_with`]: binds `config.addr` and `config.internal_addr` (port 0 for an ephemeral
//!   port), starts serving and returns at once.

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Context;
use axum::Router;
use axum::extract::State;
use axum::routing::{get, post};
use chelabase_auth::internal::InternalVerifier;
use chelabase_nats::rt::{ACCESS, AccessEvent, PRESENCE, PROJECT, ProjectEvent};
use futures_util::StreamExt;
use tokio::sync::{mpsc, watch};
use tokio::task::{AbortHandle, JoinHandle};
use uuid::Uuid;

use crate::auth::{OidcVerifier, TokenVerifier};
use crate::bus::{Bus, BusStream, NatsBus};
use crate::channel::Scope;
use crate::config::Config;
use crate::hub::{Hub, HubEvent};
use crate::limits::{SocketCounter, TicketRate};
use crate::orchestrator::OrchestratorClient;
use crate::presence::{PresenceError, PresenceMsg, heartbeat_subject};
use crate::project::ProjectDedup;
use crate::orchestrator::{AuthTarget, AuthorizeOutcome};
use crate::protocol::Member;
use crate::reauth::{Coalescer, CycleCache, Offer};
use crate::shutdown::{DRAIN, SHUTDOWN_BUDGET};
use crate::socket::{self, HEALTH_PATH, SOCKET_PATH, TICKETS_PATH};
use crate::check::PgChecker;
use crate::lease::ensure_buckets;
use crate::owner::OwnerClient;
use crate::pool::{CheckPools, ConnLookup};
use crate::replication::{ReplicationSource, SourceDeps, SourceSettings};
use crate::client_tls::ClientPair;
use crate::slots::PgReplicator;
use crate::source::ChangeSource;
use chelabase_query::server_tls::ServerTls;
use crate::store::{CounterStore, TicketStore, ValkeyStore};
use crate::ticket::Tickets;

pub struct Deps {
    /// Verifies the gateway's identity on ticket requests (audience `realtime`).
    pub identity: Arc<InternalVerifier>,
    pub verifier: Arc<dyn TokenVerifier>,
    pub tickets: Arc<dyn TicketStore>,
    pub counters: Arc<dyn CounterStore>,
    pub bus: Arc<dyn Bus>,
    pub source: Arc<dyn ChangeSource>,
}

impl Deps {
    /// NATS (`config.nats`), Valkey (`config.valkey`), the OIDC verifier (`config.oidc`) and the
    /// change source, with `identity` as the identity verifier.
    pub async fn connect(config: &Config, identity: Arc<InternalVerifier>) -> anyhow::Result<Self> {
        let oidc = config
            .oidc
            .clone()
            .context("the gateway's OIDC settings (CHELABASE_AUTH_HOST) are required")?;
        let nats = chelabase_nats::connect(&config.nats)
            .await
            .with_context(|| format!("NATS at {}", config.nats.url))?;
        let valkey = Arc::new(
            ValkeyStore::connect(&config.valkey)
                .await
                .with_context(|| format!("Valkey at {}", config.valkey.url))?,
        );
        let bus: Arc<dyn Bus> = Arc::new(NatsBus::new(nats.clone()));
        // T8: the replication-backed change source (leases and slot indexes in JetStream KV,
        // checks on the computes' authenticator pools and the orchestrator's /visible).
        let password = config.replication_password.clone().context(
            "CHELABASE_REALTIME_PG_PASSWORD_FILE is required (chelabase_realtime's password)",
        )?;
        let ca = config
            .compute_ca
            .as_ref()
            .context("CHELABASE_COMPUTE_CA_FILE is required (computes' TLS is verified, U15)")?;
        let tls = ServerTls::from_ca_file(ca)
            .map_err(|e| anyhow::anyhow!("CHELABASE_COMPUTE_CA_FILE: {e}"))?;
        let client_pair = Arc::new(ClientPair::new(config.realtime_tls_dir.clone()));
        client_pair.spawn_reload();
        let buckets = ensure_buckets(&async_nats::jetstream::new(nats))
            .await
            .map_err(|e| anyhow::anyhow!("the realtime KV buckets: {e}"))?;
        let lookup = Arc::new(ConnLookup::new(&config.internal_url, &config.internal_token));
        let checker = Arc::new(PgChecker::new(
            CheckPools::new(lookup.clone(), Some(tls)),
            OwnerClient::new(&config.orchestrator_url, &config.internal_token),
        ));
        let source = ReplicationSource::start(SourceDeps {
            instance_id: config.instance_id.clone(),
            bus: bus.clone(),
            leases: Arc::new(buckets.leases),
            slots: Arc::new(buckets.slots),
            checker,
            replicator: Arc::new(PgReplicator::new(lookup, client_pair, password)),
            settings: SourceSettings {
                max_slots_per_compute: config.max_slots_per_compute,
                ..SourceSettings::default()
            },
        })
        .await?;
        Ok(Self {
            identity,
            verifier: Arc::new(OidcVerifier::new(oidc)),
            tickets: valkey.clone(),
            counters: valkey,
            bus,
            source,
        })
    }
}

/// What the instance tells a socket's session.
pub(crate) enum SessionMsg {
    /// An access burst for the session's member (coalesced per member on the instance, R15):
    /// the member's sockets share one `/authorize` per target for it.
    Access(u64),
}

pub(crate) struct SessionEntry {
    pub tx: mpsc::UnboundedSender<SessionMsg>,
    pub sub: String,
    /// The tenants the session's channels were allowed in.
    pub tenants: Arc<Mutex<HashSet<Uuid>>>,
    pub abort: Option<AbortHandle>,
}

/// R15's per-member coalescing of access events: the window per member (sub), and the
/// tenants each member's trailing burst is for.
pub(crate) type AccessBursts = (Coalescer<String>, HashMap<String, HashSet<Uuid>>);

/// One instance's state, shared by its routes, its sessions and its background tasks.
pub(crate) struct Shared {
    pub config: Config,
    pub identity: Arc<InternalVerifier>,
    pub verifier: Arc<dyn TokenVerifier>,
    pub tickets: Tickets,
    pub sockets: SocketCounter,
    pub ticket_rate: TicketRate,
    pub bus: Arc<dyn Bus>,
    pub source: Arc<dyn ChangeSource>,
    pub orch: OrchestratorClient,
    pub sessions: Mutex<HashMap<u64, SessionEntry>>,
    pub hubs: Mutex<HashMap<String, Arc<Hub>>>,
    pub projects: Mutex<HashMap<Uuid, HashMap<u64, mpsc::UnboundedSender<ProjectEvent>>>>,
    pub dedup: Mutex<ProjectDedup>,
    pub cycle_cache: Mutex<CycleCache>,
    /// R15: access events coalesced per member (sub) on the instance, and the tenants each
    /// member's trailing run is for.
    pub access: Mutex<AccessBursts>,
    pub shutdown: watch::Sender<bool>,
    next_id: AtomicU64,
}

impl Shared {
    pub fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    /// The periodic re-authorization cycle now (W28).
    pub fn cycle(&self) -> u64 {
        let interval = self.config.timings.reauth_interval.as_millis().max(1) as u64;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        now / interval
    }

    /// A fresh `/authorize` answer (a join's, a renewal's, an access burst's) replaces the
    /// cycle's cached one, so the member's other sockets don't act on a stale answer this
    /// cycle. Only answers about the member's access are kept (not an expired token's, not a
    /// transient `unavailable`).
    pub fn fresh_answer(&self, sub: &str, target: &AuthTarget, outcome: &AuthorizeOutcome) {
        if matches!(
            outcome,
            AuthorizeOutcome::Allowed { .. }
                | AuthorizeOutcome::Forbidden
                | AuthorizeOutcome::NotFound
        ) {
            let cycle = self.cycle();
            self.cycle_cache
                .lock()
                .unwrap()
                .put(sub, target, cycle, outcome.clone());
        }
    }

    /// Starts an access burst for `sub`: every session of the member in one of `tenants`
    /// re-authorizes, sharing the answers.
    fn access_burst(&self, sub: &str, tenants: &HashSet<Uuid>) {
        let burst = self.next_id();
        self.cycle_cache.lock().unwrap().prune_bursts(sub, burst);
        let sessions = self.sessions.lock().unwrap();
        for s in sessions.values().filter(|s| s.sub == sub) {
            if s.tenants.lock().unwrap().iter().any(|t| tenants.contains(t)) {
                let _ = s.tx.send(SessionMsg::Access(burst));
            }
        }
    }

    /// Registers a viewer on the channel's hub (made on first use): the hub, the viewer's
    /// events and the join message to publish.
    pub fn join_hub(
        &self,
        scope: Scope,
        channel_id: &str,
        key: &str,
        viewer: u64,
        member: &Member,
    ) -> Result<(Arc<Hub>, mpsc::UnboundedReceiver<HubEvent>, PresenceMsg), PresenceError> {
        let mut hubs = self.hubs.lock().unwrap();
        let hub = hubs
            .entry(key.to_string())
            .or_insert_with(|| {
                Arc::new(Hub::start(
                    scope,
                    channel_id,
                    key,
                    &self.config.instance_id,
                    self.bus.clone(),
                    self.config.limits.members_per_channel,
                    self.config.timings.presence_sync_wait,
                ))
            })
            .clone();
        match hub.add_viewer(viewer, member) {
            Ok((rx, msg)) => Ok((hub, rx, msg)),
            Err(e) => {
                if hub.viewers() == 0 {
                    hubs.remove(key);
                }
                Err(e)
            }
        }
    }

    /// Takes a viewer off its hub (dropped with its last viewer): the leave message to publish.
    pub fn leave_hub(
        &self,
        key: &str,
        hub: &Arc<Hub>,
        viewer: u64,
        member_id: &str,
    ) -> PresenceMsg {
        let mut hubs = self.hubs.lock().unwrap();
        let (msg, left) = hub.remove_viewer(viewer, member_id);
        if left == 0 && hubs.get(key).is_some_and(|h| Arc::ptr_eq(h, hub)) {
            hubs.remove(key);
        }
        msg
    }

    fn all_hubs(&self) -> Vec<Arc<Hub>> {
        self.hubs.lock().unwrap().values().cloned().collect()
    }

    /// A project channel's event feed.
    pub fn register_project(
        &self,
        project_id: Uuid,
        registration: u64,
    ) -> mpsc::UnboundedReceiver<ProjectEvent> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.projects
            .lock()
            .unwrap()
            .entry(project_id)
            .or_default()
            .insert(registration, tx);
        rx
    }

    pub fn unregister_project(&self, project_id: Uuid, registration: u64) {
        let mut projects = self.projects.lock().unwrap();
        if let Some(regs) = projects.get_mut(&project_id) {
            regs.remove(&registration);
            if regs.is_empty() {
                projects.remove(&project_id);
            }
        }
    }
}

/// A running instance.
pub struct Realtime {
    addr: SocketAddr,
    internal_addr: SocketAddr,
    shared: Arc<Shared>,
    listeners: Vec<JoinHandle<()>>,
    background: Vec<JoinHandle<()>>,
}

impl Realtime {
    /// The public listener's bound address.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }
    /// The internal listener's bound address.
    pub fn internal_addr(&self) -> SocketAddr {
        self.internal_addr
    }
    pub fn instance_id(&self) -> &str {
        &self.shared.config.instance_id
    }
    /// Closes every socket with 1012, then stops the listeners; the change source closes its
    /// replication connections and releases its leases and slot indexes.
    pub fn shutdown(&self) {
        self.shared.shutdown.send_replace(true);
        let source = self.shared.source.clone();
        tokio::spawn(async move { source.shutdown().await });
    }
    /// Stops at once, as a crash would: no close frames, no presence leaves, no releases.
    pub fn kill(self) {
        for t in self.listeners.iter().chain(self.background.iter()) {
            t.abort();
        }
        self.shared.source.abort();
        let sessions: Vec<SessionEntry> = self
            .shared
            .sessions
            .lock()
            .unwrap()
            .drain()
            .map(|(_, e)| e)
            .collect();
        for s in sessions {
            if let Some(a) = s.abort {
                a.abort();
            }
        }
        self.shared.hubs.lock().unwrap().clear();
        self.shared.projects.lock().unwrap().clear();
    }
    /// Until both listeners have stopped, the sessions have closed and the change source has
    /// let go, all within one deadline ([`SHUTDOWN_BUDGET`], inside compose's 10 s stop grace):
    /// whatever still hangs at its end is abandoned (a lease expires on its own).
    pub async fn wait(self) -> anyhow::Result<()> {
        let started = Instant::now();
        let deadline = started + SHUTDOWN_BUDGET;
        // The listeners and the sessions together, leaving the change source its share: the
        // listeners stop accepting at once (a connection still sending its request is cut at
        // the end), and every session closes its socket with 1012 and cleans up.
        let first = (started + DRAIN + Duration::from_secs(1)).min(deadline);
        loop {
            let listening = self.listeners.iter().any(|t| !t.is_finished());
            let sessions = !self.shared.sessions.lock().unwrap().is_empty();
            if (!listening && !sessions) || Instant::now() >= first {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        for t in &self.listeners {
            t.abort();
        }
        // The leases and slot indexes at their own revisions (the spawned one may still run).
        let left = deadline.saturating_duration_since(Instant::now());
        let _ = tokio::time::timeout(left, self.shared.source.shutdown()).await;
        for t in &self.background {
            t.abort();
        }
        self.shared.hubs.lock().unwrap().clear();
        Ok(())
    }
}

pub async fn build(config: Config) -> anyhow::Result<Realtime> {
    let identity = InternalVerifier::from_pem_file(&config.verify_key)
        .map_err(|e| anyhow::anyhow!("CHELABASE_INTERNAL_VERIFY_KEY: {e}"))?;
    let deps = Deps::connect(&config, Arc::new(identity)).await?;
    build_with(config, deps).await
}

pub async fn build_with(config: Config, deps: Deps) -> anyhow::Result<Realtime> {
    let public = tokio::net::TcpListener::bind(config.addr)
        .await
        .with_context(|| format!("CHELABASE_REALTIME_ADDR={}", config.addr))?;
    let internal = tokio::net::TcpListener::bind(config.internal_addr)
        .await
        .with_context(|| format!("CHELABASE_REALTIME_INTERNAL_ADDR={}", config.internal_addr))?;
    let addr = public.local_addr()?;
    let internal_addr = internal.local_addr()?;

    let orch = OrchestratorClient::new(
        &config.orchestrator_url,
        &config.internal_url,
        &config.internal_token,
    );
    let (shutdown, _) = watch::channel(false);
    let shared = Arc::new(Shared {
        identity: deps.identity,
        verifier: deps.verifier,
        tickets: Tickets::new(config.ticket_key.clone(), deps.tickets),
        sockets: SocketCounter::new(deps.counters.clone(), config.limits.sockets_per_member),
        ticket_rate: TicketRate::new(deps.counters, config.limits.tickets_per_sec),
        bus: deps.bus,
        source: deps.source,
        orch,
        sessions: Mutex::new(HashMap::new()),
        hubs: Mutex::new(HashMap::new()),
        projects: Mutex::new(HashMap::new()),
        dedup: Mutex::new(ProjectDedup::with_window(config.timings.project_dedup)),
        cycle_cache: Mutex::new(CycleCache::new()),
        access: Mutex::new((
            Coalescer::new(config.timings.access_coalesce),
            HashMap::new(),
        )),
        shutdown,
        next_id: AtomicU64::new(1),
        config,
    });

    // The platform's events and the other instances' heartbeats, subscribed before serving so
    // nothing published after the start is missed.
    let access = shared.bus.subscribe(&format!("{ACCESS}.*")).await?;
    let projects = shared.bus.subscribe(&format!("{PROJECT}.*")).await?;
    let heartbeats = shared.bus.subscribe(&format!("{PRESENCE}.hb.*")).await?;
    let background = vec![
        tokio::spawn(access_events(shared.clone(), access)),
        tokio::spawn(project_events(shared.clone(), projects)),
        tokio::spawn(heartbeats_in(shared.clone(), heartbeats)),
        tokio::spawn(heartbeats_out(shared.clone())),
    ];

    let public_app = Router::new()
        .route(TICKETS_PATH, post(socket::tickets))
        .route(SOCKET_PATH, get(socket::upgrade))
        .with_state(shared.clone());
    // Readiness too (#22): 503 while member tokens can't be checked yet.
    let internal_app = Router::new()
        .route(
            HEALTH_PATH,
            get(|State(shared): State<Arc<Shared>>| async move {
                if shared.verifier.is_ready() {
                    (axum::http::StatusCode::OK, "ok")
                } else {
                    (axum::http::StatusCode::SERVICE_UNAVAILABLE, "waiting for the token audiences")
                }
            }),
        )
        .with_state(shared.clone());
    let listeners = vec![
        tokio::spawn(serve(public, public_app, shared.shutdown.subscribe())),
        tokio::spawn(serve(internal, internal_app, shared.shutdown.subscribe())),
    ];
    tracing::info!(%addr, %internal_addr, instance = %shared.config.instance_id, "realtime listening");
    Ok(Realtime {
        addr,
        internal_addr,
        shared,
        listeners,
        background,
    })
}

async fn serve(listener: tokio::net::TcpListener, app: Router, mut stop: watch::Receiver<bool>) {
    let result = axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            let _ = stop.wait_for(|s| *s).await;
        })
        .await;
    if let Err(e) = result {
        tracing::error!(error = %e, "a listener failed");
    }
}

/// The last token of a subject (`chelabase.rt.access.<id>` → `<id>`).
fn last_token(subject: &str) -> &str {
    subject.rsplit('.').next().unwrap_or("")
}

/// Access events (R15, W19): every session of the member, or of the whole tenant, coalesced per
/// member on the instance (one burst at once, then at most one trailing burst per window).
async fn access_events(shared: Arc<Shared>, mut events: BusStream) {
    loop {
        let trailing = shared.access.lock().unwrap().0.next_due();
        let due = async {
            match trailing {
                Some(at) => tokio::time::sleep_until(tokio::time::Instant::from_std(at)).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            m = events.next() => {
                let Some(m) = m else { return };
                access_event(&shared, &m);
            }
            _ = due => {
                let now = Instant::now();
                let due: Vec<(String, HashSet<Uuid>)> = {
                    let mut access = shared.access.lock().unwrap();
                    let (coalescer, tenants) = &mut *access;
                    coalescer
                        .due(now)
                        .into_iter()
                        .map(|sub| {
                            let t = tenants.remove(&sub).unwrap_or_default();
                            (sub, t)
                        })
                        .collect()
                };
                for (sub, tenants) in due {
                    shared.access_burst(&sub, &tenants);
                }
            }
        }
    }
}

/// One access event: a burst for each member it names (or every member of the tenant with a
/// session here) now, or folded into the member's trailing burst.
fn access_event(shared: &Shared, m: &crate::bus::BusMessage) {
    let Ok(ev) = serde_json::from_slice::<AccessEvent>(&m.payload) else {
        tracing::debug!("an access event that isn't one: ignored");
        return;
    };
    // The payload's tenant must be the subject's (a publisher can't widen its event).
    if last_token(&m.subject) != ev.tenant_id.hyphenated().to_string() {
        tracing::warn!("an access event whose tenant isn't its subject's: ignored");
        return;
    }
    let subs: HashSet<String> = shared
        .sessions
        .lock()
        .unwrap()
        .values()
        .filter(|s| ev.member_sub.as_deref().is_none_or(|sub| sub == s.sub))
        .filter(|s| s.tenants.lock().unwrap().contains(&ev.tenant_id))
        .map(|s| s.sub.clone())
        .collect();
    let now = Instant::now();
    for sub in subs {
        let offer = {
            let mut access = shared.access.lock().unwrap();
            let (coalescer, tenants) = &mut *access;
            let offer = coalescer.offer(sub.clone(), now);
            if offer == Offer::Coalesced {
                tenants.entry(sub.clone()).or_default().insert(ev.tenant_id);
            }
            offer
        };
        if offer == Offer::RunNow {
            shared.access_burst(&sub, &HashSet::from([ev.tenant_id]));
        }
    }
}

/// Project events (R21, W34): deduplicated per project, then to every joined channel.
async fn project_events(shared: Arc<Shared>, mut events: BusStream) {
    while let Some(m) = events.next().await {
        let Ok(project_id) = Uuid::parse_str(last_token(&m.subject)) else {
            continue;
        };
        let Ok(ev) = serde_json::from_slice::<ProjectEvent>(&m.payload) else {
            tracing::debug!(%project_id, "a project event that isn't one: ignored");
            continue;
        };
        if !shared
            .dedup
            .lock()
            .unwrap()
            .admit(project_id, &ev, Instant::now())
        {
            continue;
        }
        let mut projects = shared.projects.lock().unwrap();
        if let Some(regs) = projects.get_mut(&project_id) {
            regs.retain(|_, tx| tx.send(ev.clone()).is_ok());
        }
    }
}

/// The other instances' heartbeats, for every channel's member expiry.
async fn heartbeats_in(shared: Arc<Shared>, mut beats: BusStream) {
    let prefix = format!("{PRESENCE}.hb.");
    while let Some(m) = beats.next().await {
        let Some(instance) = m.subject.strip_prefix(&prefix) else {
            continue;
        };
        if instance == shared.config.instance_id {
            continue;
        }
        let now = Instant::now();
        for hub in shared.all_hubs() {
            hub.heartbeat(instance, now);
        }
    }
}

/// This instance's heartbeat, and the expiry of silent instances' members.
async fn heartbeats_out(shared: Arc<Shared>) {
    let every = shared.config.timings.presence_heartbeat;
    let after = every * shared.config.timings.presence_missed;
    let subject = heartbeat_subject(&shared.config.instance_id);
    let mut tick = tokio::time::interval(every);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        if let Err(e) = shared.bus.publish(&subject, None, "{}".into()).await {
            tracing::warn!(error = %e, "presence heartbeat failed");
        }
        let now = Instant::now();
        for hub in shared.all_hubs() {
            hub.tick(now, after);
        }
    }
}
