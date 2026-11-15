//! Object storage's URL routes (m6), behind storage's bearer: the key its signed URLs are
//! signed with, whether the key that minted one is still live (a data key, or the member who
//! minted it from the console, m9), and the one-time upload tokens. And s3-endpoint m3: an S3
//! access key's secret and scope, for storage's SigV4 check.

use std::future::Future;
use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::http::header::CACHE_CONTROL;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use chelabase_metadata::{
    BranchKind, BranchStatus, DataKeyLookup, DatabaseStatus, MetadataError, S3KeyLookup,
};
use chelabase_types::access::{PlatformRole, Requested};
use chelabase_types::s3_key::is_access_key_id;
use chelabase_types::{BranchId, ProjectId};
use chelabase_neon::ComputeLocator;
use chelabase_secrets::{Purpose, derive_key};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::error;
use uuid::Uuid;

use super::{ConsoleUrlFacts, InternalState, NeonMetadataPort};
use crate::access::{RoleGraph, act_as_allowed, act_as_reads_graph, member_facts};
use crate::roles;

/// The URL-signing key's current generation. A new one (a rotation) makes every URL signed
/// with an older one fail.
pub const URL_KEY_ID: u32 = 1;

type St<M, L> = State<Arc<InternalState<M, L>>>;

pub(super) fn routes<M, L>() -> Router<Arc<InternalState<M, L>>>
where
    M: NeonMetadataPort,
    L: ComputeLocator,
{
    Router::new()
        .route("/internal/storage/url-key", get(url_key::<M, L>))
        .route("/internal/storage/keys/{key_id}", get(live_key::<M, L>))
        .route(
            "/internal/storage/s3-keys/{access_key_id}",
            get(s3_key::<M, L>),
        )
        .route(
            "/internal/storage/upload-tokens",
            post(insert_token::<M, L>),
        )
        .route(
            "/internal/storage/upload-tokens/{token_id}/consume",
            post(consume_token::<M, L>),
        )
}

fn key_json(key: &DataKeyLookup) -> Value {
    json!({
        "key_id": key.key_id,
        "branch_id": key.branch_id,
        "project_id": key.project_id,
        "kind": key.kind,
        "role": key.role,
        "databases": key.databases,
        "role_oid": key.role_oid,
    })
}

fn failed(context: &str, err: impl std::fmt::Display) -> Response {
    error!("{context}: {err}");
    StatusCode::INTERNAL_SERVER_ERROR.into_response()
}

/// `{"kid", "key"}`: the URL-signing key (32 bytes, base64), derived from the secrets key for
/// [`Purpose::StorageUrl`] and [`URL_KEY_ID`].
async fn url_key<M, L>(State(state): St<M, L>) -> Response
where
    M: NeonMetadataPort,
    L: ComputeLocator,
{
    let key = derive_key(&state.secrets, Purpose::StorageUrl, URL_KEY_ID);
    Json(json!({ "kid": URL_KEY_ID, "key": STANDARD.encode(key) })).into_response()
}

/// `?branch=`: the branch the URL was signed on; for a console URL, how it was minted.
#[derive(Deserialize)]
struct LiveQuery {
    branch: Option<Uuid>,
    /// The URL was minted in owner mode (m9): its member must still be an Admin.
    #[serde(default)]
    owner: bool,
    /// The role the URL was minted as (#100): its member must still hold it at Developer.
    #[serde(default)]
    role: Option<String>,
}

/// How a console URL was minted, as its signed claims say.
#[derive(Clone, Copy, Debug)]
pub enum ConsoleMinter<'a> {
    Owner,
    Role(&'a str),
}

/// The live key with this id (as the gateway's lookup answers it); else, for a URL minted from
/// the console (m9, the member as its key), the member while [`console_url_live`] says so,
/// on `?branch=`; 404 for anything else.
async fn live_key<M, L>(
    State(state): St<M, L>,
    Path(key_id): Path<Uuid>,
    Query(q): Query<LiveQuery>,
) -> Response
where
    M: NeonMetadataPort,
    L: ComputeLocator,
{
    match state.metadata.live_data_key(key_id).await {
        Ok(Some(key)) => return Json(key_json(&key)).into_response(),
        Ok(None) => {}
        Err(err) => return failed("storage key liveness", err),
    }
    let Some(branch) = q.branch else {
        return StatusCode::NOT_FOUND.into_response();
    };
    // A console URL names exactly one of owner mode and its role; one naming neither (minted
    // before #100) or both is never live.
    let minted = match (q.owner, q.role.as_deref()) {
        (true, None) => ConsoleMinter::Owner,
        (false, Some(role)) if !role.is_empty() => ConsoleMinter::Role(role),
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    match console_url_live(&state.metadata, key_id, BranchId(branch), minted).await {
        Ok(Some(project)) => Json(json!({
            "key_id": key_id,
            "branch_id": branch,
            "project_id": project,
            "kind": "secret",
            "role": "",
            "databases": null,
            "role_oid": null,
        }))
        .into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(err) => failed("console URL liveness", err),
    }
}

/// What the S3 key route reads: the [`NeonMetadataPort`] methods of the same names. A trait of
/// its own so the route's rules are tested on a small fake.
pub(super) trait S3Keys: Send + Sync + 'static {
    fn lookup_s3_key(
        &self,
        access_key_id: &str,
    ) -> impl Future<Output = Result<Option<S3KeyLookup>, MetadataError>> + Send;
    fn touch_s3_key_used(&self, key_id: Uuid) -> impl Future<Output = ()> + Send;
}

impl<M: NeonMetadataPort, L: ComputeLocator> S3Keys for InternalState<M, L> {
    async fn lookup_s3_key(&self, access_key_id: &str) -> Result<Option<S3KeyLookup>, MetadataError> {
        self.metadata.lookup_s3_key(access_key_id).await
    }
    async fn touch_s3_key_used(&self, key_id: Uuid) {
        self.metadata.touch_s3_key_used(key_id).await
    }
}

async fn s3_key<M, L>(State(state): St<M, L>, Path(access_key_id): Path<String>) -> Response
where
    M: NeonMetadataPort,
    L: ComputeLocator,
{
    s3_key_answer(state, &access_key_id).await
}

/// s3-endpoint m3: the live S3 key with this access key id, for storage's SigV4 check. 200
/// `{"key_id", "access_key_id", "secret", "branch_id", "project_id", "database", "role",
/// "role_oid", "role_timeline"}` with `Cache-Control: no-store` while the key is live, its
/// branch a `normal` one and neither the branch nor its database deleting or deleted (the
/// lookup's SQL filters these, and they are checked again here); its use is stamped in the
/// background. 400 for an id without the access key id's shape (nothing is looked up), 404
/// for any other id, 500 when the secret won't open or the row has no role identity.
///
/// The secret is in the body only: nothing here logs the body, the secret or the lookup
/// ([`S3KeyLookup`]'s `Debug` redacts it); errors name the key, never a value.
async fn s3_key_answer<K: S3Keys>(keys: Arc<K>, access_key_id: &str) -> Response {
    if !is_access_key_id(access_key_id) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let key = match keys.lookup_s3_key(access_key_id).await {
        Ok(Some(key)) => key,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        // The metadata's error names the key's id (a secret that won't open is
        // `MetadataError::Secret("S3 key {id}: …")`), never a value.
        Err(err) => {
            error!(access_key_id, "looking up an S3 key: {err}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    if matches!(key.branch_status, BranchStatus::Deleting | BranchStatus::Deleted)
        || matches!(key.database_status, DatabaseStatus::Deleting | DatabaseStatus::Deleted)
    {
        return StatusCode::NOT_FOUND.into_response();
    }
    // The table's CHECK keeps a live key's role identity set; a key without one is never
    // served (it would run as a role by name only).
    let identity = match (key.role_oid, key.role_timeline.as_deref()) {
        (Some(oid), Some(timeline)) if oid >= 1 && !timeline.is_empty() => Some((oid, timeline)),
        _ => None,
    };
    let Some((role_oid, role_timeline)) = identity else {
        error!(key_id = %key.key_id, "a live S3 key has no role identity: not served");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    let body = Json(json!({
        "key_id": key.key_id,
        "access_key_id": access_key_id,
        "secret": key.secret,
        "branch_id": key.branch_id,
        "project_id": key.project_id,
        "database": key.database,
        "role": key.role,
        "role_oid": role_oid,
        "role_timeline": role_timeline,
    }));
    let key_id = key.key_id;
    tokio::spawn(async move { keys.touch_s3_key_used(key_id).await });
    ([(CACHE_CONTROL, "no-store")], body).into_response()
}

/// Whether `member` may still use a URL they minted from the console on `branch` (#100): the
/// branch is a normal one not being deleted, and console `sign` by the member, as the role the
/// URL was minted as (or in owner mode), on that branch, would be authorized now. That is
/// [`act_as_allowed`] at Developer (`sign`'s level) on the same facts [`authorize`] reads: the
/// database (not deleted, in the member's tenant), the member (active, not offboarded; a
/// tenant admin as [`tenant_admin_effective`]), their own and team grants with the database's
/// override, the branch's role graph (chains, R21; owner-level, LOGIN and ADMIN roles, R16,
/// R22, R23) and the personal logins (MH-21). The project when so.
///
/// Nothing here is cached: storage keeps each answer for `LIVENESS_TTL` (5 s) and drops them
/// on an access event, so a grant change counts at once and a role graph change (DDL, which
/// sends no access event) within 5 s.
///
/// [`authorize`]: crate::access::authorize
/// [`tenant_admin_effective`]: chelabase_types::access::tenant_admin_effective
pub async fn console_url_live<F: ConsoleUrlFacts>(
    facts: &F,
    member: Uuid,
    branch: BranchId,
    minted: ConsoleMinter<'_>,
) -> Result<Option<ProjectId>, chelabase_metadata::MetadataError> {
    let Some(b) = facts.get_branch(branch).await? else {
        return Ok(None);
    };
    if b.kind != BranchKind::Normal || matches!(b.status, BranchStatus::Deleting | BranchStatus::Deleted) {
        return Ok(None);
    }
    let Some(db) = facts.get_database(b.database_id).await? else {
        return Ok(None);
    };
    if db.status == DatabaseStatus::Deleted {
        return Ok(None);
    }
    let Some((m, sources)) = facts.member_access(member, db.project_id, db.database_id).await? else {
        return Ok(None);
    };
    if m.tenant_id != db.tenant_id {
        return Ok(None);
    }
    let caller = member_facts(Some(&m));
    let grant = sources.effective();
    let requested = match minted {
        ConsoleMinter::Owner => Requested::Owner,
        ConsoleMinter::Role(role) => Requested::Role(role),
    };
    let owner = roles::owner_name(&db);
    let min = PlatformRole::Developer;
    let graph = match (&b.endpoint_id, act_as_reads_graph(caller, grant.as_ref(), min, requested, owner)) {
        (Some(endpoint), true) => Some(RoleGraph::new(owner, facts.list_roles(endpoint).await?)),
        _ => None,
    };
    let personal_login = match minted {
        ConsoleMinter::Owner => false,
        ConsoleMinter::Role(role) => match Requested::from_name(role, owner) {
            Requested::Role(name) => facts.is_personal_login_name(db.database_id, name).await?,
            Requested::Owner => false,
        },
    };
    let allowed = act_as_allowed(caller, grant.as_ref(), min, requested, owner, graph.as_ref(), personal_login);
    Ok(allowed.is_ok().then_some(db.project_id))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewToken {
    token_id: Uuid,
    key_id: Uuid,
    claims: Value,
    expires_at: DateTime<Utc>,
}

/// Why an upload token's app session doesn't let it through.
enum NotLive {
    /// Not live, not the key's project's, or not a session id.
    Refused,
    /// The metadata store failed (logged).
    Failed,
}

/// The app session an upload token's claims name (`sid`, an app user's token), if any: its
/// expiry while it is live and in the key's project.
async fn session_of<M, L>(
    state: &InternalState<M, L>,
    claims: &Value,
    key_id: Uuid,
) -> Result<Option<DateTime<Utc>>, NotLive>
where
    M: NeonMetadataPort,
    L: ComputeLocator,
{
    let Some(sid) = claims.get("sid") else {
        return Ok(None);
    };
    let sid: Uuid = sid
        .as_str()
        .and_then(|s| s.parse().ok())
        .ok_or(NotLive::Refused)?;
    let key = match state.metadata.live_data_key(key_id).await {
        Ok(Some(key)) => key,
        Ok(None) => return Err(NotLive::Refused),
        Err(err) => {
            error!("storage key liveness: {err}");
            return Err(NotLive::Failed);
        }
    };
    match state.metadata.app_session(sid).await {
        Ok(Some(s))
            if s.revoked_at.is_none()
                && s.expires_at > Utc::now()
                && s.project_id == key.project_id =>
        {
            Ok(Some(s.expires_at))
        }
        Ok(_) => Err(NotLive::Refused),
        Err(err) => {
            error!("an upload token's app session: {err}");
            Err(NotLive::Failed)
        }
    }
}

/// Records a minted upload token: `{"expires_at"}` (seconds since the epoch), or 409 when its
/// id is taken or its key isn't live. An app user's token (its claims carry the session's
/// `sid`) needs that session live and in the key's project, and never outlives it: its expiry
/// is cut to the session's.
async fn insert_token<M, L>(State(state): St<M, L>, Json(t): Json<NewToken>) -> Response
where
    M: NeonMetadataPort,
    L: ComputeLocator,
{
    let expires_at = match session_of(&state, &t.claims, t.key_id).await {
        Ok(session) => session.map_or(t.expires_at, |s| s.min(t.expires_at)),
        Err(NotLive::Refused) => return StatusCode::CONFLICT.into_response(),
        Err(NotLive::Failed) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    match state
        .metadata
        .insert_upload_token(t.token_id, t.key_id, &t.claims, expires_at)
        .await
    {
        Ok(true) => Json(json!({ "expires_at": expires_at.timestamp() })).into_response(),
        Ok(false) => StatusCode::CONFLICT.into_response(),
        Err(err) => failed("recording an upload token", err),
    }
}

/// Spends an upload token: `{"key", "claims"}` (the key as it is now) once; 404 after, or for
/// an unknown or expired token, a dead key, or an app user's session no longer live.
async fn consume_token<M, L>(State(state): St<M, L>, Path(token_id): Path<Uuid>) -> Response
where
    M: NeonMetadataPort,
    L: ComputeLocator,
{
    match state.metadata.consume_upload_token(token_id).await {
        Ok(Some((key, claims))) => match session_of(&state, &claims, key.key_id).await {
            Ok(_) => Json(json!({ "key": key_json(&key), "claims": claims })).into_response(),
            Err(NotLive::Refused) => StatusCode::NOT_FOUND.into_response(),
            Err(NotLive::Failed) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        },
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(err) => failed("spending an upload token", err),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    use chelabase_types::DatabaseId;
    use tokio::sync::mpsc;
    use tracing::instrument::WithSubscriber;

    use super::*;

    const TIMELINE: &str = "0123456789abcdef0123456789abcdef";

    /// What the fake's lookup answers for an access key id.
    enum Row {
        /// The row as the metadata reads it (the SQL's own filters are tested live).
        Found(S3KeyLookup),
        /// A sealed value that doesn't open: the metadata's own error for key `0`.
        WontOpen(Uuid),
    }

    /// A model of the S3 key metadata: lookups counted, touches stamped at most once a minute
    /// (as `touch_s3_key_used`'s SQL) and reported on `touched`.
    struct Fake {
        rows: Mutex<HashMap<String, Row>>,
        lookups: AtomicUsize,
        last_used: Mutex<HashMap<Uuid, Instant>>,
        writes: AtomicUsize,
        touched: mpsc::UnboundedSender<Uuid>,
    }

    impl Fake {
        fn new() -> (Arc<Self>, mpsc::UnboundedReceiver<Uuid>) {
            let (touched, rx) = mpsc::unbounded_channel();
            let fake = Self {
                rows: Mutex::default(),
                lookups: AtomicUsize::new(0),
                last_used: Mutex::default(),
                writes: AtomicUsize::new(0),
                touched,
            };
            (Arc::new(fake), rx)
        }

        fn insert(&self, access_key_id: &str, row: Row) {
            self.rows.lock().unwrap().insert(access_key_id.into(), row);
        }
    }

    impl S3Keys for Fake {
        async fn lookup_s3_key(&self, access_key_id: &str) -> Result<Option<S3KeyLookup>, MetadataError> {
            self.lookups.fetch_add(1, Ordering::SeqCst);
            match self.rows.lock().unwrap().get(access_key_id) {
                None => Ok(None),
                Some(Row::Found(key)) => Ok(Some(key.clone())),
                Some(Row::WontOpen(id)) => Err(MetadataError::Secret(format!(
                    "S3 key {id}: {}",
                    chelabase_secrets::SecretsError::Decrypt
                ))),
            }
        }

        async fn touch_s3_key_used(&self, key_id: Uuid) {
            let now = Instant::now();
            let mut last = self.last_used.lock().unwrap();
            if last.get(&key_id).is_none_or(|t| now.duration_since(*t) >= Duration::from_secs(60)) {
                last.insert(key_id, now);
                self.writes.fetch_add(1, Ordering::SeqCst);
            }
            drop(last);
            let _ = self.touched.send(key_id);
        }
    }

    fn live(secret: &str) -> S3KeyLookup {
        S3KeyLookup {
            key_id: Uuid::new_v4(),
            branch_id: BranchId::new(),
            branch_status: BranchStatus::Ready,
            database_id: DatabaseId::new(),
            database_status: DatabaseStatus::Ready,
            project_id: ProjectId::new(),
            database: "app".into(),
            role: "app_writer".into(),
            role_oid: Some(16_384),
            role_timeline: Some(TIMELINE.into()),
            secret: secret.into(),
        }
    }

    async fn ask(fake: &Arc<Fake>, access_key_id: &str) -> (StatusCode, Option<String>, Vec<u8>) {
        let resp = s3_key_answer(fake.clone(), access_key_id).await;
        let status = resp.status();
        let cache = resp
            .headers()
            .get(CACHE_CONTROL)
            .map(|v| v.to_str().unwrap().to_owned());
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        (status, cache, body.to_vec())
    }

    /// A `tracing` writer into a shared buffer.
    #[derive(Clone, Default)]
    struct Logs(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for Logs {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Logs {
        fn subscriber(&self) -> impl tracing::Subscriber + Send + Sync {
            let writer = self.clone();
            tracing_subscriber::fmt()
                .with_ansi(false)
                .with_max_level(tracing::Level::TRACE)
                .with_writer(move || writer.clone())
                .finish()
        }
        fn text(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
    }

    #[tokio::test]
    async fn s3_key_live_answers_scope_and_secret() {
        let (fake, mut touched) = Fake::new();
        let pair = chelabase_types::s3_key::generate();
        let key = live(&pair.secret);
        fake.insert(&pair.access_key_id, Row::Found(key.clone()));

        let (status, cache, body) = ask(&fake, &pair.access_key_id).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(cache.as_deref(), Some("no-store"));
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            body,
            json!({
                "key_id": key.key_id,
                "access_key_id": pair.access_key_id,
                "secret": pair.secret,
                "branch_id": key.branch_id,
                "project_id": key.project_id,
                "database": "app",
                "role": "app_writer",
                "role_oid": 16_384,
                "role_timeline": TIMELINE,
            })
        );
        // Its use is stamped, in the background.
        assert_eq!(touched.recv().await, Some(key.key_id));
    }

    #[tokio::test]
    async fn s3_key_dead_is_404() {
        let (fake, mut touched) = Fake::new();
        let unknown = chelabase_types::s3_key::generate();
        let mut cases = Vec::new();
        for (branch, database) in [
            (BranchStatus::Deleting, DatabaseStatus::Ready),
            (BranchStatus::Deleted, DatabaseStatus::Ready),
            (BranchStatus::Ready, DatabaseStatus::Deleting),
            (BranchStatus::Ready, DatabaseStatus::Deleted),
        ] {
            let pair = chelabase_types::s3_key::generate();
            let mut key = live(&pair.secret);
            key.branch_status = branch;
            key.database_status = database;
            fake.insert(&pair.access_key_id, Row::Found(key));
            cases.push(pair.access_key_id);
        }
        // Unknown, or revoked (the lookup finds no live row either way).
        cases.push(unknown.access_key_id);
        for id in cases {
            let (status, cache, body) = ask(&fake, &id).await;
            assert_eq!((status, body.as_slice()), (StatusCode::NOT_FOUND, &b""[..]), "{id}");
            assert_eq!(cache, None, "{id}");
        }
        assert!(touched.try_recv().is_err(), "a dead key's use is never stamped");
    }

    #[tokio::test]
    async fn s3_key_bad_shape_is_400_without_lookup() {
        let (fake, _touched) = Fake::new();
        let good = chelabase_types::s3_key::generate().access_key_id;
        for bad in [
            "../x".to_owned(),
            good.to_lowercase(),
            good[..19].to_owned(),
            // 21 characters.
            format!("{good}A"),
            String::new(),
            format!("XBAK{}", &good[4..]),
            format!("{}1", &good[..19]),
            format!("{}=", &good[..19]),
            format!("{}\u{e9}", &good[..18]),
        ] {
            fake.insert(&bad, Row::Found(live("x")));
            let (status, _, body) = ask(&fake, &bad).await;
            assert_eq!((status, body.as_slice()), (StatusCode::BAD_REQUEST, &b""[..]), "{bad:?}");
        }
        assert_eq!(fake.lookups.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn s3_key_touch_is_throttled() {
        let (fake, mut touched) = Fake::new();
        let pair = chelabase_types::s3_key::generate();
        let key = live(&pair.secret);
        fake.insert(&pair.access_key_id, Row::Found(key.clone()));
        for _ in 0..2 {
            assert_eq!(ask(&fake, &pair.access_key_id).await.0, StatusCode::OK);
            assert_eq!(touched.recv().await, Some(key.key_id));
        }
        assert_eq!(fake.writes.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn s3_key_without_role_identity_is_500() {
        let (fake, mut touched) = Fake::new();
        let logs = Logs::default();
        let mut keys = Vec::new();
        for (oid, timeline) in [
            (None, Some(TIMELINE)),
            (Some(16_384), None),
            (None, None),
            (Some(0), Some(TIMELINE)),
            (Some(16_384), Some("")),
        ] {
            let pair = chelabase_types::s3_key::generate();
            let mut key = live(&pair.secret);
            key.role_oid = oid;
            key.role_timeline = timeline.map(str::to_owned);
            fake.insert(&pair.access_key_id, Row::Found(key.clone()));
            keys.push((pair, key.key_id));
        }
        for (pair, key_id) in &keys {
            let (status, cache, body) = ask(&fake, &pair.access_key_id)
                .with_subscriber(logs.subscriber())
                .await;
            assert_eq!((status, body.as_slice()), (StatusCode::INTERNAL_SERVER_ERROR, &b""[..]));
            assert_eq!(cache, None);
            let text = logs.text();
            assert!(text.contains(&key_id.to_string()), "{text}");
            assert!(!text.contains(&pair.secret), "{text}");
        }
        assert!(touched.try_recv().is_err());
    }

    #[tokio::test]
    async fn s3_key_open_failure_is_500_and_quiet() {
        let (fake, mut touched) = Fake::new();
        let logs = Logs::default();
        let pair = chelabase_types::s3_key::generate();
        let key_id = Uuid::new_v4();
        fake.insert(&pair.access_key_id, Row::WontOpen(key_id));
        let (status, cache, body) = ask(&fake, &pair.access_key_id)
            .with_subscriber(logs.subscriber())
            .await;
        assert_eq!((status, body.as_slice()), (StatusCode::INTERNAL_SERVER_ERROR, &b""[..]));
        assert_eq!(cache, None);
        let text = logs.text();
        assert!(text.contains(&key_id.to_string()), "{text}");
        assert!(!text.contains(&pair.secret), "{text}");
        assert!(touched.try_recv().is_err());
    }
}
