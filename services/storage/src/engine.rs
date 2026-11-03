//! How a request reaches the branch's database.
//!
//! - **As the caller:** a role session on the `authenticator` pool
//!   ([`Engine::as_caller`]): the key's role and claims, a secret key's role refused when it is
//!   owner-level (as the data API refuses it), and the request's statement in one transaction
//!   after the role's preamble, so row level security decides.
//! - **As storage itself:** the managed pool ([`ServicePool`]), logged in as
//!   `chelabase_storage` with the branch's own password, running only [`NamedStatement`]s.
//!
//! Both share storage's per-branch budget (`storage_max_connections`: [`storage_budget`]). The
//! managed calls never nest inside a role session.
//!
//! Before any caller statement, [`Engine::check_schema`] (on the managed pool, cached
//! [`SCHEMA_CHECK_TTL`]) requires the database's `storage` schema to be `chelabase_admin`'s
//! with a stamp of at least [`MIN_SCHEMA_VERSION`]; otherwise every route answers 503
//! `storage_unavailable`, so a user-owned `storage` schema is never served.

use std::time::Duration;

use chelabase_auth::internal::DataIdentity;
use chelabase_pgsession::owner_level::{self, OwnerLevelCache, RoleRefused};
use chelabase_pgsession::service::ServiceError;
use chelabase_pgsession::{
    ConnInfo, ConnSource, NamedStatement, Pools, ServedDatabase, Service, ServicePool,
    named_statement, storage_budget,
};
use chelabase_query::QueryFailure;
use tokio_postgres::Client;
use uuid::Uuid;

use crate::error::Failure;

/// The oldest `storage` schema this service works with (`chelabase storage schema <n>`): the
/// one with `storage.buckets.write_once` (m7), which the bucket routes write. An older one
/// (reconcile brings it up within a pass) answers 503 rather than failing on the column.
pub const MIN_SCHEMA_VERSION: u32 = 7;

/// How long a database's schema check is trusted.
pub const SCHEMA_CHECK_TTL: Duration = Duration::from_secs(30);

/// The owner and the stamp of the database's `storage` schema (no row: there is none).
const STORAGE_SCHEMA: NamedStatement = named_statement!(
    "storage schema",
    "SELECT r.rolname::pg_catalog.text, pg_catalog.obj_description(n.oid, 'pg_namespace') FROM pg_catalog.pg_namespace n JOIN pg_catalog.pg_roles r ON r.oid OPERATOR(pg_catalog.=) n.nspowner WHERE n.nspname OPERATOR(pg_catalog.=) 'storage'",
    &[]
);

/// The OID of the database a caller statement runs in, read in that statement's own
/// transaction: it, never a cache, decides a blob's key.
pub const DATOID_SQL: &str = "SELECT oid::pg_catalog.int8 FROM pg_catalog.pg_database WHERE datname OPERATOR(pg_catalog.=) pg_catalog.current_database()";

/// A database OID as Postgres sends it (int8 here), when it is one.
pub fn datoid_of(oid: Option<i64>) -> Option<u32> {
    oid.and_then(|o| u32::try_from(o).ok())
}

/// A managed statement's failure as the caller sees it: never its message, detail or SQLSTATE
/// (logged with the request id instead). A constraint (a ticket already minted for the
/// version) is a fixed `conflict`; anything else, storage isn't available.
pub fn managed_failure(f: QueryFailure, request_id: &str) -> Failure {
    tracing::warn!(request_id, sqlstate = ?f.sqlstate, "storage's own statement failed: {}", f.message);
    if f.sqlstate.as_deref().is_some_and(|s| s.starts_with("23")) {
        Failure::object_conflict()
    } else {
        Failure::storage_unavailable()
    }
}

/// Whether a `storage` schema owned by `owner` and stamped `stamp` may be served: Chelabase's
/// own (`chelabase_admin`'s) at [`MIN_SCHEMA_VERSION`] or later. A user's own schema, no
/// stamp, or any other comment is refused.
pub fn schema_acceptable(owner: Option<&str>, stamp: Option<&str>) -> bool {
    let version = stamp
        .and_then(|s| s.strip_prefix("chelabase storage schema "))
        .filter(|d| !d.is_empty() && d.len() <= 9 && d.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|d| d.parse::<u32>().ok());
    owner == Some("chelabase_admin") && version.is_some_and(|v| v >= MIN_SCHEMA_VERSION)
}

/// The database a request runs in: `requested`, or the branch's main one; it must be served
/// and, when the key has a list, on it. Anything else is one 403 (no name discovery).
pub fn pick_database<'a>(
    info: &'a ConnInfo,
    requested: Option<&str>,
    allowed: Option<&[String]>,
) -> Result<&'a ServedDatabase, Failure> {
    let name = requested.unwrap_or(&info.database);
    let listed = allowed.is_none_or(|list| list.iter().any(|d| d == name));
    match info.served(name) {
        Some(served) if listed => Ok(served),
        _ => Err(Failure::database_not_allowed()),
    }
}

/// A request's database ([`Engine::locate`]). Its blob keys' prefix is
/// `{database_id}/{datoid}/` ([`crate::names::object_prefix`]), the OID read in the request's
/// own caller transaction.
#[derive(Debug, Clone)]
pub struct Located {
    pub database: String,
    pub database_id: Uuid,
}

pub struct Engine<S, M> {
    pools: Pools<S>,
    managed: ServicePool<M>,
    owner_level: OwnerLevelCache,
    /// `(branch, database)` → whether its schema may be served.
    schema_ok: moka::future::Cache<(Uuid, String), bool>,
}

impl<S: ConnSource, M: ConnSource> Engine<S, M> {
    /// `pools` connect as `authenticator`; `managed` is the storage login's source. Both
    /// become storage's, within its per-branch budget.
    pub fn new(pools: Pools<S>, managed: M) -> Self {
        let pools = pools.for_service(Service::Storage, storage_budget);
        let managed = ServicePool::beside(&pools, managed);
        Engine {
            pools,
            managed,
            owner_level: owner_level::owner_level_cache(),
            schema_ok: moka::future::Cache::builder()
                .max_capacity(10_000)
                .time_to_live(SCHEMA_CHECK_TTL)
                .build(),
        }
    }

    /// The served database `data` asks for ([`pick_database`]), looking again at most every
    /// few seconds when the cached info doesn't serve it yet.
    pub async fn served_database(
        &self,
        data: &DataIdentity,
        requested: Option<&str>,
    ) -> Result<ServedDatabase, Failure> {
        self.served_on(data.branch_id, requested, data.databases.as_deref())
            .await
    }

    /// [`Engine::served_database`] on `branch`, for a key's list `allowed` (`None`: any).
    pub async fn served_on(
        &self,
        branch: Uuid,
        requested: Option<&str>,
        allowed: Option<&[String]>,
    ) -> Result<ServedDatabase, Failure> {
        let mut info = self
            .pools
            .info(branch)
            .await
            .map_err(|e| Failure::from_conn(&e))?;
        if let Some(name) = requested
            && info.served(name).is_none()
            && let Some(fresh) = self
                .pools
                .refresh_stale(branch)
                .await
                .map_err(|e| Failure::from_conn(&e))?
        {
            info = fresh;
        }
        pick_database(&info, requested, allowed).cloned()
    }

    /// Where a request's objects live: the served database it asks for (schema checked) and
    /// the Chelabase database's id.
    pub async fn locate(
        &self,
        data: &DataIdentity,
        requested: Option<&str>,
        request_id: &str,
    ) -> Result<Located, Failure> {
        self.locate_on(
            data.branch_id,
            requested,
            data.databases.as_deref(),
            request_id,
        )
        .await
    }

    /// [`Engine::locate`] on `branch` without a caller (signed and public URLs).
    pub async fn locate_on(
        &self,
        branch: Uuid,
        requested: Option<&str>,
        allowed: Option<&[String]>,
        request_id: &str,
    ) -> Result<Located, Failure> {
        let served = self.served_on(branch, requested, allowed).await?;
        self.check_schema(branch, &served.name, request_id).await?;
        let info = self
            .pools
            .info(branch)
            .await
            .map_err(|e| Failure::from_conn(&e))?;
        let database_id = info.database_id.ok_or_else(Failure::storage_unavailable)?;
        Ok(Located {
            database: served.name,
            database_id,
        })
    }

    /// 503 `storage_unavailable` unless the database's `storage` schema may be served
    /// ([`schema_acceptable`]), asked as `chelabase_storage` and cached.
    pub async fn check_schema(
        &self,
        branch: Uuid,
        database: &str,
        request_id: &str,
    ) -> Result<(), Failure> {
        let key = (branch, database.to_owned());
        let ok = match self.schema_ok.get(&key).await {
            Some(ok) => ok,
            None => {
                let rows = self
                    .managed(branch, database, &STORAGE_SCHEMA, &[], request_id)
                    .await?;
                let ok = rows
                    .first()
                    .is_some_and(|row| schema_acceptable(row.get(0), row.get(1)));
                self.schema_ok.insert(key, ok).await;
                ok
            }
        };
        if ok {
            Ok(())
        } else {
            tracing::warn!(request_id, branch_id = %branch, database, "the storage schema isn't Chelabase's current one");
            Err(Failure::storage_unavailable())
        }
    }

    /// `f` in one transaction as `data`'s role and claims, in the database it asks for, after
    /// the schema check. Returns the database too.
    pub async fn as_caller<T>(
        &self,
        data: &DataIdentity,
        requested: Option<&str>,
        request_id: &str,
        f: impl AsyncFnOnce(&Client, &str) -> Result<T, QueryFailure>,
    ) -> Result<(T, String), Failure> {
        let served = self.served_database(data, requested).await?;
        let branch = data.branch_id;
        self.check_schema(branch, &served.name, request_id).await?;
        let conn = self
            .pools
            .acquire(branch, &served.name)
            .await
            .map_err(|e| Failure::from_acquire(&e, request_id))?;
        let out = conn
            .with_role_session(&data.role, &data.claims, data.role_oid, async |session| {
                owner_level::ensure_not_owner_level(&self.owner_level, data, || async {
                    session
                        .owner_level(&data.role, request_id)
                        .await
                        .map_err(|e| QueryFailure::from_pg(&e, request_id))
                })
                .await
                .map_err(|e| match e {
                    RoleRefused::OwnerLevel(role) | RoleRefused::UnknownRole(role) => {
                        Failure::role_not_allowed(&role)
                    }
                    RoleRefused::Check(f) => Failure(f),
                })?;
                let settings = session.settings(&served);
                session
                    .transaction(&served.name, &settings, request_id, f)
                    .await
                    .map_err(Failure)
            })
            .await
            .map_err(|_| Failure::unauthorized("the key names no role"))??;
        Ok((out, served.name))
    }

    /// A [`NamedStatement`] as `chelabase_storage` in `database` of `branch`.
    pub async fn managed(
        &self,
        branch: Uuid,
        database: &str,
        statement: &NamedStatement,
        params: &[&(dyn tokio_postgres::types::ToSql + Sync)],
        request_id: &str,
    ) -> Result<Vec<tokio_postgres::Row>, Failure> {
        self.managed
            .query(branch, database, statement, params, request_id)
            .await
            .map_err(|e| match e {
                ServiceError::Acquire(e) => {
                    // Storage's own login can't connect (not provisioned yet, or refused by its
                    // login check): storage isn't available here.
                    tracing::warn!(request_id, branch_id = %branch, statement = statement.name(), "the storage login: {e}");
                    Failure::storage_unavailable()
                }
                ServiceError::Query(f) => managed_failure(f, request_id),
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// AC 6: only Chelabase's own schema, at the oldest version this service knows or later,
    /// is served; a user-owned schema, a missing or foreign stamp is never.
    #[test]
    fn only_chelabase_s_current_schema_is_served() {
        let admin = Some("chelabase_admin");
        assert!(schema_acceptable(admin, Some("chelabase storage schema 7")));
        assert!(schema_acceptable(
            admin,
            Some("chelabase storage schema 12")
        ));
        for (owner, stamp) in [
            (Some("postgres"), Some("chelabase storage schema 7")),
            (Some("app_owner"), Some("chelabase storage schema 99")),
            // Before `write_once` (m7): the bucket routes would fail on the column.
            (admin, Some("chelabase storage schema 6")),
            (admin, Some("chelabase storage schema 3")),
            (admin, Some("chelabase storage schema ")),
            (admin, Some("chelabase storage schema 7 ")),
            (admin, Some("chelabase storage schema 1234567890")),
            (admin, Some("my storage")),
            (admin, None),
            (None, None),
        ] {
            assert!(!schema_acceptable(owner, stamp), "{owner:?} {stamp:?}");
        }
    }

    fn info() -> ConnInfo {
        chelabase_pgsession::test_support::two_databases()
    }

    /// A key reaches only the served databases on its list; anything else is one 403.
    #[test]
    fn a_key_reaches_only_its_listed_served_databases() {
        let info = info();
        let main = info.database.clone();
        let other = info.databases[1].name.clone();
        assert_eq!(pick_database(&info, None, None).unwrap().name, main);
        assert_eq!(
            pick_database(&info, Some(&other), None).unwrap().name,
            other
        );
        let list = vec![main.clone()];
        assert_eq!(pick_database(&info, None, Some(&list)).unwrap().name, main);
        for (requested, allowed) in [(Some(other.as_str()), Some(&list)), (Some("ghost"), None)] {
            let err = pick_database(&info, requested, allowed.map(|l| l.as_slice())).unwrap_err();
            assert_eq!(
                (err.0.status.as_u16(), err.0.code),
                (403, "database_not_allowed")
            );
        }
        let none: Vec<String> = Vec::new();
        assert!(pick_database(&info, None, Some(&none)).is_err());
    }

    /// SF1: a managed statement's own failure never reaches the caller: a constraint (a ticket
    /// already minted) is a fixed `conflict`, anything else a fixed 503; neither carries the
    /// statement's message, detail or SQLSTATE.
    #[test]
    fn a_managed_failure_is_answered_with_a_fixed_code() {
        let mut dup = QueryFailure::new(
            axum::http::StatusCode::CONFLICT,
            "conflict",
            "duplicate key value violates unique constraint \"upload_tickets_pkey\"",
        );
        dup.detail = Some("Key (version_id)=(2212c5ca) already exists.".into());
        dup.sqlstate = Some("23505".into());
        let other = QueryFailure::new(
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            "relation storage.x does not exist",
        );
        for (f, status, code) in [(dup, 409, "conflict"), (other, 503, "storage_unavailable")] {
            let out = managed_failure(f, "rid");
            assert_eq!((out.0.status.as_u16(), out.0.code), (status, code));
            assert!(out.0.detail.is_none() && out.0.sqlstate.is_none() && out.0.hint.is_none());
            assert!(
                !out.0.message.contains("upload_tickets") && !out.0.message.contains("storage.x"),
                "{}",
                out.0.message
            );
        }
    }

    /// A database OID as the caller transaction reads it (int8).
    #[test]
    fn a_database_oid_parses() {
        assert_eq!(datoid_of(Some(16384)), Some(16384));
        assert_eq!(datoid_of(Some(-1)), None);
        assert_eq!(datoid_of(None), None);
    }
}
