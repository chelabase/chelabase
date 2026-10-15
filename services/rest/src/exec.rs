//! One request, one transaction: pick the database (main when the tree names none; it must
//! be served and on the key's list, else 404 `unknown_database`); for a secret key, refuse
//! an owner-level role ([`OWNER_LEVEL_SQL`], asked before the transaction and cached); run
//! the request through [`chelabase_query::run`] (`[ROLLBACK;] BEGIN; RESET_SQL`, preamble,
//! schema cache, shape cache, plans, response cap, `COMMIT`, `RESET_SQL`) with that
//! database's `default_schema`, on a leased connection ([`chelabase_query::Lease`]: a
//! cancelled request's connection is closed, not pooled).
//!
//! The caches are keyed by `(branch, database)`; [`Engine::invalidate`] drops a branch's
//! connection info, pools, schemas and plans at once.

use std::sync::Arc;

use chelabase_auth::internal::DataIdentity;
use chelabase_query::engine::{self, QueryCache, RunAs, RunSettings, with_transaction};
use chelabase_query::schema::{RoleView, Schema, Visibility};
use chelabase_query::{Lease, QueryClient};
use chelabase_types::data_key::DataKeyKind;
use serde_json::Value;
use tokio_postgres::TransactionStatus;
use tokio_postgres::types::Type;
use uuid::Uuid;

use crate::conn::{
    AcquireError, ConnError, ConnInfo, ConnSource, Pools, RESET_SQL, ServedDatabase,
};
use crate::error::ApiError;

#[cfg(test)]
use chelabase_query::engine::SchemaKey;
pub use chelabase_query::engine::{DEFAULT_MAX_RESPONSE_BYTES, SCHEMA_CACHE};

/// Whether role `$1` may not back a secret key (E33), exactly what the worker's create-time check
/// refuses: superuser, an attribute that administers (CREATEROLE, CREATEDB, REPLICATION), or
/// membership at any depth in a superuser role or one of
/// [`chelabase_types::data_key::KEY_ROLE_REFUSED_GROUPS`] (`neon_superuser` and the system
/// roles). BYPASSRLS alone isn't refused. No row: the role doesn't exist.
pub const OWNER_LEVEL_SQL: &str = "SELECT r.rolsuper OR r.rolcreaterole OR r.rolcreatedb OR r.rolreplication \
    OR EXISTS (SELECT FROM pg_catalog.pg_roles s WHERE (s.rolsuper OR s.rolname IN ('neon_superuser', \
    'chelabase_admin', 'pg_execute_server_program', 'pg_read_server_files', 'pg_write_server_files')) \
    AND pg_catalog.pg_has_role(r.oid, s.oid, 'MEMBER')) \
    FROM pg_catalog.pg_roles r WHERE r.rolname = $1";

/// Sent just before [`OWNER_LEVEL_SQL`], in the same round trip: the check runs before the
/// request's transaction, so it ends whatever an earlier request left open (a `ROLLBACK`,
/// unless the connection is idle: outside a transaction Postgres logs a warning for it) and
/// resets the session it may have changed (a `statement_timeout`, a `search_path`, a role).
fn owner_check_batch(status: TransactionStatus) -> String {
    if status == TransactionStatus::Idle {
        RESET_SQL.to_owned()
    } else {
        format!("ROLLBACK; {RESET_SQL}")
    }
}

/// Asks Postgres whether `role` is owner-level (`None`: it doesn't exist), before the
/// request's transaction. A request dropped half-way may have left the connection inside
/// a transaction, maybe a failed one, so a `ROLLBACK` goes first (unless the connection is
/// idle), in the same round trip. When that clean-up batch fails the session may not be
/// clean: the connection is marked broken (discarded); the answer stays the check's own.
async fn fetch_owner_level<C: QueryClient + ?Sized>(
    client: &C,
    role: &str,
    request_id: &str,
) -> Result<Option<bool>, ApiError> {
    let params: [(&(dyn tokio_postgres::types::ToSql + Sync), Type); 1] = [(&role, Type::TEXT)];
    // Read now, while nothing is in flight.
    let clean = owner_check_batch(client.transaction_status());
    let c = client.client();
    // `biased`: the clean-up is sent first.
    let (cleaned, row) = tokio::join!(
        biased;
        c.batch_execute(&clean),
        c.query_typed_opt(OWNER_LEVEL_SQL, &params),
    );
    if let Err(e) = cleaned {
        tracing::warn!(request_id, "the owner check's clean-up failed; the connection is discarded: {e}");
        client.mark_broken();
    }
    row.and_then(|row| row.map(|r| r.try_get::<_, Option<bool>>(0)).transpose())
        .map(|level| level.map(|l| l.unwrap_or(false)))
        .map_err(|e| ApiError::from_pg(&e, request_id))
}

/// How long an owner-level answer is kept.
const OWNER_LEVEL_TTL: std::time::Duration = std::time::Duration::from_secs(30);

type OwnerLevelCache = moka::future::Cache<(Uuid, String), bool>;

fn owner_level_cache() -> OwnerLevelCache {
    moka::future::Cache::builder()
        .max_capacity(SCHEMA_CACHE * 10)
        .time_to_live(OWNER_LEVEL_TTL)
        .support_invalidation_closures()
        .build()
}

/// Whether `role` is owner-level on `branch` (cached), `None` when it doesn't exist (not
/// cached: it may be created a moment later). `fetch` asks Postgres on a miss.
async fn owner_level<F, Fut>(
    cache: &OwnerLevelCache,
    branch: Uuid,
    role: &str,
    fetch: F,
) -> Result<Option<bool>, ApiError>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<Option<bool>, ApiError>>,
{
    let key = (branch, role.to_owned());
    if let Some(v) = cache.get(&key).await {
        return Ok(Some(v));
    }
    let v = fetch().await?;
    if let Some(v) = v {
        cache.insert(key, v).await;
    }
    Ok(v)
}

/// For a secret key only: refuses a role that is owner-level. A missing role counts as
/// plain (the preamble reports it).
async fn ensure_not_owner_level<F, Fut>(
    cache: &OwnerLevelCache,
    data: &DataIdentity,
    fetch: F,
) -> Result<(), ApiError>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<Option<bool>, ApiError>>,
{
    if data.kind != DataKeyKind::Secret {
        return Ok(());
    }
    match owner_level(cache, data.branch_id, &data.role, fetch).await? {
        Some(true) => Err(ApiError::role_not_allowed(&data.role)),
        _ => Ok(()),
    }
}

/// The console's role must exist and not be owner-level (the secret key's 403).
async fn ensure_console_role<F, Fut>(
    cache: &OwnerLevelCache,
    branch: Uuid,
    role: &str,
    fetch: F,
) -> Result<(), ApiError>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<Option<bool>, ApiError>>,
{
    match owner_level(cache, branch, role, fetch).await? {
        None => Err(ApiError::unknown_role(role)),
        Some(true) => Err(ApiError::role_not_allowed(role)),
        Some(false) => Ok(()),
    }
}

/// Who a console request runs as: `role`, through the secret-key path, with no claims and
/// no allow-list (the orchestrator checked the caller owns the database).
fn console_identity(branch: Uuid, role: &str) -> DataIdentity {
    DataIdentity {
        branch_id: branch,
        key_id: Uuid::nil(),
        kind: DataKeyKind::Secret,
        role: role.to_owned(),
        claims: Value::Object(Default::default()),
        databases: None,
        project_id: None,
        role_oid: None,
    }
}

/// The database a request runs in: `requested`, or main; it must be served and, when the
/// key has an allow-list, on it.
fn pick_database<'a>(
    info: &'a ConnInfo,
    requested: Option<&str>,
    allowed: Option<&[String]>,
) -> Result<&'a ServedDatabase, ApiError> {
    let name = requested.unwrap_or(&info.database);
    if allowed.is_some_and(|list| !list.iter().any(|d| d == name)) {
        return Err(ApiError::unknown_database());
    }
    info.served(name).ok_or_else(ApiError::unknown_database)
}

/// The roles the internal schema route describes: the console's `role`, or `anon` and
/// `authenticated`.
fn internal_schema_roles(role: Option<&str>) -> Vec<&str> {
    match role {
        Some(role) => vec![role],
        None => vec!["anon", "authenticated"],
    }
}

/// The check a request passes before its transaction.
#[derive(Clone, Copy)]
enum Guard<'a> {
    None,
    /// A secret key: its role mustn't be owner-level.
    Key(&'a DataIdentity),
    /// The console's role: it must exist and not be owner-level.
    Console(&'a str),
}

impl Guard<'_> {
    /// The role OID the preamble holds the role to: a key's, when it has one.
    fn role_oid(&self) -> Option<i64> {
        match self {
            Guard::Key(data) => data.role_oid,
            Guard::None | Guard::Console(_) => None,
        }
    }
}

/// The served databases a key may reach, main first.
fn reachable(info: &ConnInfo, allowed: Option<&[String]>) -> Vec<String> {
    info.databases
        .iter()
        .filter(|d| allowed.is_none_or(|list| list.contains(&d.name)))
        .map(|d| d.name.clone())
        .collect()
}

/// The schema document with the reachable databases' names as `databases`.
fn with_databases(mut doc: Value, databases: Vec<String>) -> Value {
    if let Value::Object(map) = &mut doc {
        map.insert("databases".into(), Value::from(databases));
    }
    doc
}

/// Why a request failed before or while it ran.
#[derive(Debug)]
pub enum Failure {
    /// No connection info for the branch: the caller decides what that means.
    Conn(ConnError),
    Api(ApiError),
}

impl From<ApiError> for Failure {
    fn from(e: ApiError) -> Self {
        Failure::Api(e)
    }
}

impl From<chelabase_query::QueryFailure> for Failure {
    fn from(e: chelabase_query::QueryFailure) -> Self {
        Failure::Api(e.into())
    }
}

impl From<ConnError> for Failure {
    fn from(e: ConnError) -> Self {
        Failure::Conn(e)
    }
}

pub struct Engine<S> {
    pools: Pools<S>,
    max_response_bytes: usize,
    cache: QueryCache,
    owner_level: OwnerLevelCache,
}

/// The request limits from the branch's connection info, for `database`.
fn run_settings(info: &ConnInfo, database: &ServedDatabase) -> RunSettings {
    RunSettings {
        max_rows: info.settings.max_rows,
        statement_timeout_ms: info.settings.statement_timeout_ms,
        default_schema: database.default_schema.clone(),
        reset_sql: RESET_SQL,
    }
}

/// What a schema read returns: the schema, each role's view, the database's
/// `default_schema` and the connection info it was read with.
type Schemas = (Arc<Schema>, Vec<Arc<RoleView>>, String, Arc<ConnInfo>);

impl<S: ConnSource> Engine<S> {
    /// `max_connections` caps connections across every branch. Responses are capped at
    /// [`DEFAULT_MAX_RESPONSE_BYTES`] ([`Self::with_max_response_bytes`] changes it).
    /// No CA: computes are reached plain (the fakes only; the service uses
    /// [`Self::with_server_tls`]).
    pub fn new(source: S, max_connections: usize) -> Self {
        Self::with_pools(Pools::new(source, max_connections))
    }

    /// Every compute's TLS verified against `server_tls` (U15): the service's engine.
    pub fn with_server_tls(
        source: S,
        max_connections: usize,
        server_tls: chelabase_query::server_tls::ServerTls,
    ) -> Self {
        Self::with_pools(Pools::with_server_tls(source, max_connections, server_tls))
    }

    fn with_pools(pools: Pools<S>) -> Self {
        Engine {
            pools,
            max_response_bytes: DEFAULT_MAX_RESPONSE_BYTES,
            cache: QueryCache::default(),
            owner_level: owner_level_cache(),
        }
    }

    /// How many requests have gone out as one pipeline ([`QueryCache::pipelined_runs`]).
    pub fn pipelined_runs(&self) -> u64 {
        self.cache
            .pipelined_runs
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Caps a query's response at `bytes`.
    pub fn with_max_response_bytes(mut self, bytes: usize) -> Self {
        self.max_response_bytes = bytes;
        self
    }

    /// Drops everything held for `branch`: connection info, pools, schemas, plans and
    /// owner-level answers (its slots stay, resized by the next info). The next request asks
    /// the orchestrator again.
    pub async fn invalidate(&self, branch: Uuid) {
        self.pools.invalidate(branch).await;
        self.cache.invalidate_branch(branch);
        let _ = self
            .owner_level
            .invalidate_entries_if(move |k, _| k.0 == branch);
    }

    /// The served database a request runs in (see [`pick_database`]), with the branch's
    /// connection info.
    async fn database(
        &self,
        branch: Uuid,
        requested: Option<&str>,
        allowed: Option<&[String]>,
    ) -> Result<(Arc<ConnInfo>, ServedDatabase), Failure> {
        let mut info = self.pools.info(branch).await?;
        let listed = |name: &str| allowed.is_none_or(|list| list.iter().any(|d| d == name));
        // A database enabled since the info was cached: look again, at most every
        // INFO_REFRESH_AFTER per branch (the orchestrator's invalidate may not have reached us).
        if let Some(name) = requested
            && listed(name)
            && info.served(name).is_none()
            && let Some(fresh) = self.pools.refresh_stale(branch).await?
        {
            info = fresh;
        }
        let database = pick_database(&info, requested, allowed)?.clone();
        Ok((info, database))
    }

    async fn acquire(
        &self,
        branch: Uuid,
        database: &str,
        request_id: &str,
    ) -> Result<crate::conn::Conn, Failure> {
        self.pools
            .acquire(branch, database)
            .await
            .map_err(|e| acquire_failure(e, branch, request_id))
    }

    /// The owner-level check (cached), before the request's transaction.
    async fn check_owner_level(
        &self,
        client: &(impl QueryClient + ?Sized),
        data: &DataIdentity,
        request_id: &str,
    ) -> Result<(), ApiError> {
        ensure_not_owner_level(&self.owner_level, data, || {
            fetch_owner_level(client, &data.role, request_id)
        })
        .await
    }

    /// The check a request on `branch` passes before its transaction.
    async fn guard(
        &self,
        client: &(impl QueryClient + ?Sized),
        branch: Uuid,
        guard: Guard<'_>,
        request_id: &str,
    ) -> Result<(), ApiError> {
        match guard {
            Guard::None => Ok(()),
            Guard::Key(data) => self.check_owner_level(client, data, request_id).await,
            Guard::Console(role) => {
                ensure_console_role(&self.owner_level, branch, role, || {
                    fetch_owner_level(client, role, request_id)
                })
                .await
            }
        }
    }

    /// `POST /data/v1/query`: the JSON result, unparsed.
    pub async fn query(
        &self,
        data: &DataIdentity,
        request_id: &str,
        body: &[u8],
    ) -> Result<String, Failure> {
        let parsed = chelabase_query::parse(body).map_err(ApiError::from)?;
        let (_, database) = self
            .database(
                data.branch_id,
                parsed.database.as_deref(),
                data.databases.as_deref(),
            )
            .await?;
        self.run(data, &database, &parsed.op, false, request_id).await
    }

    /// The internal console route: `tree` as `role` in `database` (main when neither it nor
    /// the tree names one), with no allow-list. The role must exist and not be owner-level.
    pub async fn console(
        &self,
        branch: Uuid,
        role: &str,
        database: Option<&str>,
        tree: &Value,
        request_id: &str,
    ) -> Result<String, Failure> {
        let body = serde_json::to_vec(tree)
            .map_err(|e| ApiError::invalid_query(format!("the tree: {e}")))?;
        let parsed = chelabase_query::parse(&body).map_err(ApiError::from)?;
        let requested = match (database, parsed.database.as_deref()) {
            (Some(a), Some(b)) if a != b => {
                return Err(ApiError::invalid_query(
                    "the request and its tree name different databases",
                )
                .into());
            }
            (a, b) => a.or(b),
        };
        let (_, served) = self.database(branch, requested, None).await?;
        let data = console_identity(branch, role);
        self.run(&data, &served, &parsed.op, true, request_id).await
    }

    /// Runs `op` in `database` as `data`'s role, in one transaction. `console`: the role
    /// must also exist.
    ///
    /// The connection is leased ([`Lease`]): it goes back to the pool only when the owner
    /// check and the request both ran to the end (an answer, an error included) and the
    /// session was reset; a cancelled request's connection is closed.
    async fn run(
        &self,
        data: &DataIdentity,
        database: &ServedDatabase,
        op: &chelabase_query::Op,
        console: bool,
        request_id: &str,
    ) -> Result<String, Failure> {
        let branch = data.branch_id;
        let conn = self.acquire(branch, &database.name, request_id).await?;
        // Declared after `conn`, dropped before its permits are released.
        let mut lease = Lease::new(conn.client);
        let guard = if console {
            Guard::Console(&data.role)
        } else {
            Guard::Key(data)
        };
        let out = async {
            self.guard(&lease, branch, guard, request_id)
                .await?;
            // `run` sends `[ROLLBACK;] BEGIN; RESET_SQL`, `COMMIT` and `RESET_SQL` itself.
            Ok::<_, Failure>(
                chelabase_query::run(
                    &lease,
                    &self.cache,
                    (branch, &database.name),
                    RunAs::Role {
                        role: &data.role,
                        claims: &data.claims,
                        oid: guard.role_oid(),
                    },
                    &run_settings(&conn.info, database),
                    self.max_response_bytes,
                    op,
                    request_id,
                )
                .await?,
            )
        }
        .await;
        lease.done();
        out
    }

    /// `GET /data/v1/schema?database=`: what the key's role can reach in the database,
    /// without privileges, and the databases the key may reach (`databases`).
    pub async fn role_schema(
        &self,
        data: &DataIdentity,
        database: Option<&str>,
        request_id: &str,
    ) -> Result<Value, Failure> {
        let allowed = data.databases.as_deref();
        let (_, served) = self.database(data.branch_id, database, allowed).await?;
        let (schema, views, default_schema, info) = self
            .schemas_for(
                data.branch_id,
                &served,
                &data.role,
                &[&data.role],
                Guard::Key(data),
                request_id,
            )
            .await?;
        let roles: Vec<(&str, &Visibility)> = vec![(data.role.as_str(), &views[0].visibility)];
        Ok(with_databases(
            schema.to_json(&default_schema, &roles, false),
            reachable(&info, allowed),
        ))
    }

    /// The internal `GET /internal/schema/{branch}?database=&role=`: everything `anon` or
    /// `authenticated` can reach in the database (main by default), with what each holds; with
    /// `role` (the console's), what that role reaches. The role must exist and not be
    /// owner-level, as on the console route.
    pub async fn branch_schema(
        &self,
        branch: Uuid,
        database: Option<&str>,
        role: Option<&str>,
        request_id: &str,
    ) -> Result<Value, Failure> {
        let (_, served) = self.database(branch, database, None).await?;
        let names = internal_schema_roles(role);
        let guard = role.map_or(Guard::None, Guard::Console);
        let (schema, views, default_schema, _) = self
            .schemas_for(branch, &served, "none", &names, guard, request_id)
            .await?;
        let roles: Vec<(&str, &Visibility)> = names
            .iter()
            .zip(&views)
            .map(|(name, view)| (*name, &view.visibility))
            .collect();
        Ok(schema.to_json(&default_schema, &roles, true))
    }

    /// The schema and the roles' views, on a leased connection (see [`Self::run`]).
    async fn schemas_for(
        &self,
        branch: Uuid,
        database: &ServedDatabase,
        role: &str,
        roles: &[&str],
        guard: Guard<'_>,
        request_id: &str,
    ) -> Result<Schemas, Failure> {
        let conn = self.acquire(branch, &database.name, request_id).await?;
        let info = conn.info.clone();
        // Declared after `conn`, dropped before its permits are released.
        let mut lease = Lease::new(conn.client);
        let claims = Value::Object(Default::default());
        let out = async {
            self.guard(&lease, branch, guard, request_id)
                .await?;
            Ok::<_, Failure>(
                with_transaction(&lease, RESET_SQL, request_id, async |client| {
                    let (schema, _, _) = chelabase_query::load_schema(
                        client,
                        &self.cache,
                        (branch, &database.name),
                        RunAs::Role {
                            role,
                            claims: &claims,
                            oid: guard.role_oid(),
                        },
                        info.settings.statement_timeout_ms,
                        request_id,
                    )
                    .await?;
                    let mut views = Vec::new();
                    for r in roles {
                        views.push(engine::view(client, &schema, r, request_id).await?);
                    }
                    Ok((schema, views))
                })
                .await?,
            )
        }
        .await;
        lease.done();
        let (schema, views) = out?;
        Ok((schema, views, database.default_schema.clone(), info))
    }
}

/// Why no connection could be had, as the client sees it (the cause is logged).
fn acquire_failure(e: AcquireError, branch: Uuid, request_id: &str) -> Failure {
    match e {
        AcquireError::Conn(e) => Failure::Conn(e),
        AcquireError::TimedOut => {
            ApiError::unavailable("no connection to the branch's database in time; try again")
                .into()
        }
        AcquireError::NotServed => ApiError::unknown_database().into(),
        AcquireError::Busy => {
            ApiError::unavailable("too many connections in use; try again").into()
        }
        AcquireError::Db(e) => {
            tracing::warn!(request_id, branch_id = %branch, "connecting: {e}");
            connect_failure(&e.0, request_id).into()
        }
    }
}

/// A connection that couldn't be had is always a 503 [`crate::error::UNREACHABLE`]:
/// Postgres's refusals in the 503 classes go through [`ApiError::from_pool`]; any other
/// refusal (a database dropped under the info) or driver failure is still the branch's
/// database out of reach, logged as such (never as an internal error).
fn connect_failure(e: &deadpool_postgres::PoolError, request_id: &str) -> ApiError {
    use deadpool_postgres::PoolError;
    let unavailable_class = |pg: &tokio_postgres::Error| {
        pg.as_db_error()
            .and_then(|db| crate::error::classify(db.code().code()))
            .is_some_and(|(status, _)| status == 503)
    };
    match e {
        PoolError::Backend(pg) if !unavailable_class(pg) => ApiError::unreachable(request_id, pg),
        other => ApiError::from_pool(other, request_id),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_postgres::Client;

    /// Failing to connect answers a fixed message (the preamble's half of this test is in
    /// `chelabase_query::engine`).
    #[tokio::test]
    async fn connection_and_role_failures_are_fixed_messages() {
        use crate::conn::DbAcquireError;
        use crate::conn::test_support::{Answer, connect_error};
        use deadpool_postgres::PoolError;
        const SECRET: &str = "10.1.2.3:5432: permission denied to set role \"x\"";
        // Refused credentials, and a refusal no SQLSTATE class makes a 503.
        for code in ["28P01", "3D000"] {
            let refused = PoolError::Backend(connect_error(Answer::Error(code, SECRET)).await);
            let Failure::Api(e) =
                acquire_failure(AcquireError::Db(DbAcquireError(refused)), Uuid::nil(), "rid-1")
            else {
                panic!("an API error");
            };
            assert_eq!(e.status, axum::http::StatusCode::SERVICE_UNAVAILABLE, "{code}");
            assert!(!e.message.contains("10.1.2.3"), "{}", e.message);
            assert_eq!((e.detail, e.hint, e.sqlstate), (None, None, None), "{code}");
        }
    }

    #[test]
    fn owner_level_sql_is_pinned() {
        for needle in [
            "r.rolsuper",
            "r.rolcreaterole",
            "r.rolcreatedb",
            "r.rolreplication",
            "pg_catalog.pg_has_role(r.oid, s.oid, 'MEMBER')",
            "FROM pg_catalog.pg_roles r WHERE r.rolname = $1",
        ] {
            assert!(OWNER_LEVEL_SQL.contains(needle), "{needle}: {OWNER_LEVEL_SQL}");
        }
        // E33: a key's role may have BYPASSRLS.
        assert!(!OWNER_LEVEL_SQL.contains("rolbypassrls"), "{OWNER_LEVEL_SQL}");
        // F2 fix round M7: every request refuses what the create-time check refuses: a
        // member, at any depth, of a superuser role or of each refused group.
        assert!(OWNER_LEVEL_SQL.contains("s.rolsuper"), "{OWNER_LEVEL_SQL}");
        for group in chelabase_types::data_key::KEY_ROLE_REFUSED_GROUPS {
            assert!(OWNER_LEVEL_SQL.contains(&format!("'{group}'")), "{group}: {OWNER_LEVEL_SQL}");
        }
        // A function call a `search_path` could shadow: only the qualified one.
        assert_eq!(
            OWNER_LEVEL_SQL.matches("pg_has_role(").count(),
            OWNER_LEVEL_SQL.matches("pg_catalog.pg_has_role(").count(),
            "{OWNER_LEVEL_SQL}"
        );
    }

    /// The owner-level check runs before the request's transaction, on a connection an
    /// earlier request may have left in a transaction or with session settings changed
    /// (`statement_timeout`, `search_path`, a role): it ends the one and resets the others
    /// first, in the same round trip.
    #[test]
    fn the_owner_check_starts_from_a_clean_session() {
        use tokio_postgres::TransactionStatus;
        // Idle: nothing to end (a `ROLLBACK` there only logs a warning).
        assert_eq!(
            owner_check_batch(TransactionStatus::Idle),
            "SET ROLE NONE; RESET ALL"
        );
        for status in [
            TransactionStatus::InTransaction,
            TransactionStatus::Failed,
            TransactionStatus::Unknown,
        ] {
            assert_eq!(
                owner_check_batch(status),
                format!("ROLLBACK; {RESET_SQL}"),
                "{status:?}"
            );
        }
    }

    /// A [`QueryClient`] over a connection whose server is gone, recording `mark_broken`.
    struct Gone {
        client: Client,
        broken: std::sync::atomic::AtomicBool,
    }

    impl QueryClient for Gone {
        fn client(&self) -> &Client {
            &self.client
        }
        async fn prepare_cached(
            &self,
            sql: &str,
            types: &[Type],
        ) -> Result<tokio_postgres::Statement, tokio_postgres::Error> {
            self.client.prepare_typed(sql, types).await
        }
        fn mark_broken(&self) {
            self.broken.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// A client whose server went away: it answers the startup message with
    /// `AuthenticationOk` and `ReadyForQuery`, then drops the socket.
    async fn closed_client() -> Client {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let len = socket.read_i32().await.unwrap();
            let mut startup = vec![0; usize::try_from(len).unwrap() - 4];
            socket.read_exact(&mut startup).await.unwrap();
            socket
                .write_all(&[b'R', 0, 0, 0, 8, 0, 0, 0, 0, b'Z', 0, 0, 0, 5, b'I'])
                .await
                .unwrap();
            socket.flush().await.unwrap();
        });
        let mut config = tokio_postgres::Config::new();
        config.host("127.0.0.1").port(port).user("x").dbname("x");
        let (client, connection) = config
            .connect(tokio_postgres::NoTls)
            .await
            .expect("the fake server's handshake");
        server.await.unwrap();
        let _ = connection.await;
        client
    }

    /// The owner check's clean-up batch (`ROLLBACK`/reset) failing means the session may not
    /// be clean: the connection is discarded, whatever the check itself answered.
    #[tokio::test]
    async fn a_failed_owner_check_cleanup_marks_the_lease_broken() {
        let gone = Gone {
            client: closed_client().await,
            broken: Default::default(),
        };
        let out = fetch_owner_level(&gone, "r", "rid-1").await;
        assert!(out.is_err());
        assert!(gone.broken.load(std::sync::atomic::Ordering::Relaxed));
    }

    fn identity(kind: DataKeyKind, branch: Uuid, role: &str) -> DataIdentity {
        DataIdentity {
            branch_id: branch,
            key_id: Uuid::nil(),
            kind,
            role: role.into(),
            claims: serde_json::json!({}),
            databases: None,
            project_id: None,
            role_oid: None,
        }
    }

    /// A secret key's request runs as its role held to the key's OID; the console's role and
    /// the internal schema route check the name only (they aren't a key's identity).
    #[test]
    fn only_a_keys_guard_holds_the_role_to_an_oid() {
        let mut data = identity(DataKeyKind::Secret, Uuid::new_v4(), "reader");
        assert_eq!(Guard::Key(&data).role_oid(), None, "a key not backfilled yet");
        data.role_oid = Some(16390);
        assert_eq!(Guard::Key(&data).role_oid(), Some(16390));
        assert_eq!(Guard::Console("reader").role_oid(), None);
        assert_eq!(Guard::None.role_oid(), None);
    }

    fn owner_cache() -> moka::future::Cache<(Uuid, String), bool> {
        owner_level_cache()
    }

    #[tokio::test]
    async fn owner_level_check_is_cached_per_branch_and_role() {
        let cache = owner_cache();
        let branch = Uuid::new_v4();
        let queries = std::cell::Cell::new(0);
        let check = |branch: Uuid, role: &'static str, answer: bool| {
            let data = identity(DataKeyKind::Secret, branch, role);
            let cache = &cache;
            let queries = &queries;
            async move {
                ensure_not_owner_level(cache, &data, || async {
                    queries.set(queries.get() + 1);
                    Ok(Some(answer))
                })
                .await
            }
        };
        assert!(check(branch, "svc", false).await.is_ok());
        assert!(check(branch, "svc", false).await.is_ok());
        assert_eq!(queries.get(), 1);
        // Another role asks again; an owner-level answer is cached and refused.
        let e = check(branch, "boss", true).await.unwrap_err();
        assert_eq!((e.status, e.code), (axum::http::StatusCode::FORBIDDEN, "role_not_allowed"));
        assert_eq!(queries.get(), 2);
        assert!(check(branch, "boss", true).await.is_err());
        assert_eq!(queries.get(), 2);
        // Another branch asks again.
        assert!(check(Uuid::new_v4(), "svc", false).await.is_ok());
        assert_eq!(queries.get(), 3);
    }

    #[tokio::test]
    async fn publishable_keys_skip_the_check() {
        let cache = owner_cache();
        let data = identity(DataKeyKind::Public, Uuid::new_v4(), "anon");
        let r = ensure_not_owner_level(&cache, &data, || async {
            panic!("a publishable key must not be checked");
            #[allow(unreachable_code)]
            Ok(Some(true))
        })
        .await;
        assert!(r.is_ok());
    }

    /// The console runs as the role it names, through the secret-key path: an owner-level
    /// role gets the same 403 as a secret key's, and a missing role is refused.
    #[tokio::test]
    async fn console_route_runs_as_the_role_and_refuses_owner_level() {
        let branch = Uuid::new_v4();
        let id = console_identity(branch, "editor");
        assert_eq!(id.branch_id, branch);
        assert_eq!(id.kind, DataKeyKind::Secret);
        assert_eq!(id.role, "editor");
        assert_eq!(id.databases, None);
        assert_eq!(id.claims, serde_json::json!({}));

        let cache = owner_cache();
        let plain = ensure_console_role(&cache, branch, "editor", || async { Ok(Some(false)) });
        assert!(plain.await.is_ok());
        let e = ensure_console_role(&cache, branch, "boss", || async { Ok(Some(true)) })
            .await
            .unwrap_err();
        assert_eq!(e, ApiError::role_not_allowed("boss"));
        let e = ensure_console_role(&cache, branch, "ghost", || async { Ok(None) })
            .await
            .unwrap_err();
        assert_eq!((e.status.as_u16(), e.code), (404, "unknown_role"));
        // A missing role isn't cached: created a moment later, it's found.
        let found = ensure_console_role(&cache, branch, "ghost", || async { Ok(Some(false)) });
        assert!(found.await.is_ok());
    }

    /// The internal schema route describes `anon` and `authenticated` by default, or the one
    /// role the console names (checked like the console's queries).
    #[test]
    fn the_internal_schema_describes_the_named_role_or_the_two_api_roles() {
        assert_eq!(internal_schema_roles(None), vec!["anon", "authenticated"]);
        assert_eq!(internal_schema_roles(Some("editor")), vec!["editor"]);
    }

    #[test]
    fn schema_lists_reachable_databases() {
        let info = crate::conn::test_support::two_databases();
        assert_eq!(reachable(&info, None), vec!["main", "analytics"]);
        let list = vec!["analytics".to_string(), "archive".to_string()];
        assert_eq!(reachable(&info, Some(&list)), vec!["analytics"]);
        assert!(reachable(&info, Some(&[])).is_empty());

        let doc = with_databases(serde_json::json!({ "version": "1" }), reachable(&info, None));
        assert_eq!(doc["databases"], serde_json::json!(["main", "analytics"]));
        assert_eq!(doc["version"], "1");
    }

    #[test]
    fn the_database_is_main_by_default_and_must_be_served_and_listed() {
        let info = crate::conn::test_support::two_databases();
        let pick = |requested: Option<&str>, list: Option<&[String]>| {
            pick_database(&info, requested, list).map(|d| d.name.clone())
        };
        assert_eq!(pick(None, None).unwrap(), "main");
        assert_eq!(pick(Some("analytics"), None).unwrap(), "analytics");
        let list = vec!["analytics".to_string(), "archive".to_string()];
        assert_eq!(pick(Some("analytics"), Some(&list)).unwrap(), "analytics");
        for (requested, list) in [
            (Some("ghost"), None),
            (None, Some(&list[..])),
            (Some("archive"), Some(&list[..])),
            (None, Some(&[][..])),
        ] {
            let e = pick(requested, list).unwrap_err();
            assert_eq!(e, ApiError::unknown_database(), "{requested:?} {list:?}");
        }
    }

    /// Answers `main` alone until `enable` is set, then `main` and `analytics`; counts fetches.
    struct EnablingSource {
        enabled: std::sync::atomic::AtomicBool,
        fetches: std::sync::atomic::AtomicUsize,
    }

    impl ConnSource for Arc<EnablingSource> {
        async fn fetch(&self, _branch: Uuid) -> Result<ConnInfo, ConnError> {
            use std::sync::atomic::Ordering;
            self.fetches.fetch_add(1, Ordering::SeqCst);
            let mut info = crate::conn::test_support::two_databases();
            if !self.enabled.load(Ordering::SeqCst) {
                info.databases.truncate(1);
            }
            Ok(info)
        }
    }

    /// A database enabled after the branch's info was cached is found without an invalidate:
    /// an unknown name re-fetches the info once it is older than [`INFO_REFRESH_AFTER`] (so
    /// a stream of unknown names asks the orchestrator at most that often).
    #[tokio::test(start_paused = true)]
    async fn a_newly_enabled_database_is_found_without_an_invalidate() {
        use crate::conn::INFO_REFRESH_AFTER;
        use std::sync::atomic::Ordering;
        let source = Arc::new(EnablingSource {
            enabled: false.into(),
            fetches: 0.into(),
        });
        let engine = Engine::new(source.clone(), 10);
        let branch = Uuid::new_v4();
        let name = |r: Result<(Arc<ConnInfo>, ServedDatabase), Failure>| match r {
            Ok((_, d)) => Ok(d.name),
            Err(Failure::Api(e)) => Err(e.code),
            Err(Failure::Conn(e)) => panic!("{e}"),
        };
        assert_eq!(name(engine.database(branch, None, None).await).unwrap(), "main");
        source.enabled.store(true, Ordering::SeqCst);
        // Fresh info: an unknown name is answered from it.
        assert_eq!(
            name(engine.database(branch, Some("analytics"), None).await),
            Err("unknown_database")
        );
        assert_eq!(source.fetches.load(Ordering::SeqCst), 1);
        tokio::time::advance(INFO_REFRESH_AFTER + std::time::Duration::from_millis(1)).await;
        assert_eq!(
            name(engine.database(branch, Some("analytics"), None).await).unwrap(),
            "analytics"
        );
        assert_eq!(source.fetches.load(Ordering::SeqCst), 2);
        // A name that still isn't served, right after: no new fetch.
        assert_eq!(
            name(engine.database(branch, Some("ghost"), None).await),
            Err("unknown_database")
        );
        assert_eq!(source.fetches.load(Ordering::SeqCst), 2);
        // A name off the key's list never re-fetches.
        tokio::time::advance(INFO_REFRESH_AFTER * 2).await;
        let list = vec!["main".to_string()];
        assert_eq!(
            name(engine.database(branch, Some("ghost"), Some(&list)).await),
            Err("unknown_database")
        );
        assert_eq!(source.fetches.load(Ordering::SeqCst), 2);
    }

    /// One call drops the branch's info, pools, schemas and plans; another branch
    /// keeps its own.
    #[tokio::test]
    async fn invalidate_drops_pools_and_caches_at_once() {
        use chelabase_query::schema::Raw;
        let engine = Engine::new(crate::conn::test_support::RefusedSource, 10);
        let (branch, other) = (Uuid::new_v4(), Uuid::new_v4());
        let schema_key = |b: Uuid| SchemaKey {
            branch: b,
            database: "analytics".into(),
            version: "1".into(),
        };
        let shape_key = |b: Uuid| (schema_key(b), "anon".to_string(), "public".to_string(), 7u64);
        for b in [branch, other] {
            engine.pools.warm(b).await;
            engine
                .cache
                .cached_schema(schema_key(b), "rid", async {
                    Ok(Schema::build("1".into(), Raw::default()))
                })
                .await
                .unwrap();
            engine.cache.shapes.get_or_plan(shape_key(b), || Ok(vec![])).unwrap();
            assert_eq!(engine.pools.cached(b).await, (true, 2, true));
        }

        engine.invalidate(branch).await;

        // The slots stay (connections still held count against the branch).
        assert_eq!(engine.pools.cached(branch).await, (false, 0, true));
        assert_eq!(engine.pools.cached(other).await, (true, 2, true));
        for (b, dropped) in [(branch, true), (other, false)] {
            let introspected = std::cell::Cell::new(false);
            engine
                .cache
                .cached_schema(schema_key(b), "rid", async {
                    introspected.set(true);
                    Ok(Schema::build("1".into(), Raw::default()))
                })
                .await
                .unwrap();
            assert_eq!(introspected.get(), dropped, "schema of {b}");
            let planned = std::cell::Cell::new(false);
            engine
                .cache
                .shapes
                .get_or_plan(shape_key(b), || {
                    planned.set(true);
                    Ok(vec![])
                })
                .unwrap();
            assert_eq!(planned.get(), dropped, "plans of {b}");
        }
    }
}
