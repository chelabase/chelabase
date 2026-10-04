//! Neon endpoints, roles and databases: what the proxy, compute_ctl and the storage
//! controller ask the orchestrator's internal listener about.

use std::time::Duration;

use crate::{BranchStatus, MetadataClient, MetadataError, ServedDatabase};
use chrono::{DateTime, Utc};
use chelabase_types::BranchId;
use serde::Serialize;
use sqlx::{Postgres, Row, Transaction};

/// The first key of every endpoint lock (`pg_advisory_xact_lock(int, int)`), so they
/// can't collide with other advisory locks (sqlx's migration lock uses the one-key form).
const ENDPOINT_LOCK_CLASS: i32 = 0x6e65_6f6e; // "neon"
/// How long a locker queues for an endpoint's lock before giving up. Under the worker's 8 s
/// `RESET_DEADLINE`, so a reset that waits its full turn still ends with a clear error and
/// some time to spare; reconcile gives up after it too and retries on its next pass.
pub const ENDPOINT_LOCK_TIMEOUT: Duration = Duration::from_secs(5);
/// Postgres' `lock_not_available`: the `lock_timeout` ran out.
const LOCK_NOT_AVAILABLE: &str = "55P03";

/// An endpoint lock from [`MetadataClient::lock_endpoint`]. Dropping it releases the lock:
/// the transaction it lives in rolls back when its connection goes back to the pool.
pub struct EndpointLock {
    _tx: Transaction<'static, Postgres>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NeonEndpoint {
    pub id: String,
    pub tenant_id: String,
    pub timeline_id: String,
    pub compute_token_sha256: String,
}

/// Everything `create_endpoint_for_branch` writes.
#[derive(Debug, Clone, Copy)]
pub struct NewEndpoint<'a> {
    pub id: &'a str,
    pub tenant_id: &'a str,
    pub timeline_id: &'a str,
    pub compute_token_sha256: &'a str,
    pub owner_role: &'a str,
    pub owner_verifier: &'a str,
    pub database_name: &'a str,
    /// Role copies written with the endpoint (a branch copy's), in the same transaction. The
    /// main database's `branch_databases` row is written with them.
    pub role_copies: &'a [NewRoleCopy<'a>],
}

/// A non-owner `neon_roles` row to insert with a new endpoint.
#[derive(Debug, Clone, Copy)]
pub struct NewRoleCopy<'a> {
    pub name: &'a str,
    /// `Sql` or `Api`.
    pub origin: RoleOrigin,
    /// `None` stores NULL (an MD5 role).
    pub verifier: Option<&'a str>,
    pub password_enc: Option<&'a str>,
    pub login: bool,
    pub level: RoleLevel,
    pub api_owner_level: bool,
    pub pending: bool,
}

/// How [`MetadataClient::insert_api_role`] ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InsertRoleOutcome {
    Inserted,
    /// A role (owner, SQL-made or API-made) already has that name.
    Exists,
    /// The endpoint already has `cap` copies.
    CapReached,
}

/// Where a `neon_roles` row came from: the branch's owner, a role made in SQL, or one made
/// through the API.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub enum RoleOrigin {
    Owner,
    Sql,
    Api,
}

impl RoleOrigin {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Owner => "owner",
            Self::Sql => "sql",
            Self::Api => "api",
        }
    }

    pub(crate) fn parse(s: &str) -> Result<Self, MetadataError> {
        match s {
            "owner" => Ok(Self::Owner),
            "sql" => Ok(Self::Sql),
            "api" => Ok(Self::Api),
            other => Err(MetadataError::InvalidStatus(format!("role origin {other}"))),
        }
    }
}

/// What a role may do: `OwnerLevel` is the wide definition (superuser, CREATEROLE, CREATEDB,
/// BYPASSRLS, REPLICATION, or `neon_superuser` at any depth).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub enum RoleLevel {
    Plain,
    OwnerLevel,
}

impl RoleLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Plain => "plain",
            Self::OwnerLevel => "owner_level",
        }
    }

    pub(crate) fn parse(s: &str) -> Result<Self, MetadataError> {
        match s {
            "plain" => Ok(Self::Plain),
            "owner_level" => Ok(Self::OwnerLevel),
            other => Err(MetadataError::InvalidStatus(format!("role level {other}"))),
        }
    }
}

/// Where a role's password stands, as Studio shows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub enum PasswordState {
    /// Made by Chelabase; the sealed password can be revealed.
    Generated,
    /// Set in SQL; only the hash is known.
    SetInSql,
    /// An MD5 hash: the proxy can't use it.
    Md5Unusable,
    /// Made by Chelabase, but the sealed password was dropped (it didn't open under the key in
    /// use); a new password can be set.
    Unreadable,
}

/// One `neon_roles` row with its secrets. Debug redacts the verifier and the sealed password.
#[derive(Clone, PartialEq, Eq)]
pub struct RoleCopy {
    pub name: String,
    pub origin: RoleOrigin,
    /// `None` for an MD5 role.
    pub scram_verifier: Option<String>,
    pub password_enc: Option<String>,
    pub login: bool,
    pub level: RoleLevel,
    /// Only for `Api` rows: re-create with `neon_superuser`.
    pub api_owner_level: bool,
    pub pending: bool,
    pub created_at: DateTime<Utc>,
}

impl std::fmt::Debug for RoleCopy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let redact = |v: &Option<String>| v.as_ref().map(|_| "<redacted>");
        f.debug_struct("RoleCopy")
            .field("name", &self.name)
            .field("origin", &self.origin)
            .field("scram_verifier", &redact(&self.scram_verifier))
            .field("password_enc", &redact(&self.password_enc))
            .field("login", &self.login)
            .field("level", &self.level)
            .field("api_owner_level", &self.api_owner_level)
            .field("pending", &self.pending)
            .field("created_at", &self.created_at)
            .finish()
    }
}

/// A role as listed for an endpoint (no secrets).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleRow {
    pub name: String,
    pub origin: RoleOrigin,
    pub level: RoleLevel,
    pub login: bool,
    pub password: PasswordState,
    /// Written but not yet applied to the compute.
    pub pending: bool,
    pub created_at: DateTime<Utc>,
}

/// Where a `branch_databases` row came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub enum DatabaseOrigin {
    /// The branch's main database.
    Main,
    /// Made in SQL.
    Sql,
    /// The cluster's own `postgres` database.
    System,
}

impl DatabaseOrigin {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Main => "main",
            Self::Sql => "sql",
            Self::System => "system",
        }
    }

    pub(crate) fn parse(s: &str) -> Result<Self, MetadataError> {
        match s {
            "main" => Ok(Self::Main),
            "sql" => Ok(Self::Sql),
            "system" => Ok(Self::System),
            other => Err(MetadataError::InvalidStatus(format!("database origin {other}"))),
        }
    }
}

/// Whether the data API serves a database of a branch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub enum DataApiState {
    Off,
    /// A worker job is setting the database up.
    Enabling,
    On,
    /// The last enable failed; `data_api_error` says why.
    Error,
}

impl DataApiState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Enabling => "enabling",
            Self::On => "on",
            Self::Error => "error",
        }
    }

    pub(crate) fn parse(s: &str) -> Result<Self, MetadataError> {
        match s {
            "off" => Ok(Self::Off),
            "enabling" => Ok(Self::Enabling),
            "on" => Ok(Self::On),
            "error" => Ok(Self::Error),
            other => Err(MetadataError::InvalidStatus(format!("data api state {other}"))),
        }
    }
}

/// What [`MetadataClient::begin_enable_data_api`] decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnableStart {
    /// The row is now `enabling` (also for a retry of one already `enabling` or in `error`).
    Started,
    AlreadyOn,
    /// The branch has no such database.
    Unknown,
    /// The cluster's own `postgres` database is never served.
    System,
    /// The branch already serves this many databases (the cap).
    OverCap(u32),
}

/// A `branch_databases` row as read: name, owner, origin, data API state and error, default
/// schema, bootstrapped.
type BranchDatabaseRow = (String, String, String, String, Option<String>, String, bool);

/// A Postgres database of a branch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchDatabase {
    pub name: String,
    pub owner: String,
    pub origin: DatabaseOrigin,
    pub data_api: DataApiState,
    pub data_api_error: Option<String>,
    pub default_schema: String,
    /// An enable began on this database at least once (main: always), so the bootstrap may
    /// have installed the machinery; never cleared.
    pub data_api_bootstrapped: bool,
}

/// What a resync observed on the compute, to write in one transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResyncWrite {
    pub upserts: Vec<CopyUpsert>,
    pub removals: Vec<String>,
    /// Every database observed: `branch_databases` is set to exactly these.
    pub databases: Vec<ObservedDatabase>,
}

/// A database the resync saw. `system` is true for the bootstrap-owned `postgres` database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedDatabase {
    pub name: String,
    pub owner: String,
    pub system: bool,
}

/// Insert-or-update one row. `origin` is used only on insert; an existing row keeps its origin
/// (and its `api_owner_level`). `verifier: None` stores NULL (MD5). Debug redacts the
/// verifier.
#[derive(Clone, PartialEq, Eq)]
pub struct CopyUpsert {
    pub name: String,
    pub origin: RoleOrigin,
    pub verifier: Option<String>,
    pub password_enc: SealedChange,
    pub login: bool,
    pub level: RoleLevel,
    pub pending: bool,
}

impl std::fmt::Debug for CopyUpsert {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CopyUpsert")
            .field("name", &self.name)
            .field("origin", &self.origin)
            .field("verifier", &self.verifier.as_ref().map(|_| "<redacted>"))
            .field("password_enc", &self.password_enc)
            .field("login", &self.login)
            .field("level", &self.level)
            .field("pending", &self.pending)
            .finish()
    }
}

/// What an upsert does to a row's sealed password.
#[derive(Clone, PartialEq, Eq)]
pub enum SealedChange {
    Keep,
    Clear,
    Set(String),
}

impl std::fmt::Debug for SealedChange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Keep => f.write_str("Keep"),
            Self::Clear => f.write_str("Clear"),
            Self::Set(_) => f.write_str("Set(<redacted>)"),
        }
    }
}

/// How [`MetadataClient::apply_resync`] ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResyncApplied {
    /// `changed` names the role rows inserted, updated or removed.
    Applied { changed: Vec<String> },
    /// `roles_version` moved since it was read: nothing was written.
    VersionMoved,
}

/// An endpoint and the branch linked to it, as seen by the reconcile loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndpointState {
    pub id: String,
    pub tenant_id: String,
    pub branch_id: Option<BranchId>,
    pub branch_status: Option<BranchStatus>,
    pub needs_reconfigure: bool,
    /// The compute settings version its compute was last created with (0 before step 6a).
    pub compute_settings_version: i32,
}

/// `roles_version` and a left-joined owner's name and verifier.
type VersionedRoleRow = (i64, Option<String>, Option<String>);

/// `list_roles`' row: name, origin, level, login, no verifier, sealed, pending, created, owner sealed.
type RoleRowTuple = (String, String, String, bool, bool, bool, bool, DateTime<Utc>, bool, bool);

/// `roles_version` and a left-joined row's columns, as [`copy_from_tuple`] reads them.
type VersionedCopyRow = (
    i64,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<bool>,
    Option<String>,
    Option<bool>,
    Option<bool>,
    Option<DateTime<Utc>>,
);

type RoleCopyTuple = (
    String,
    String,
    Option<String>,
    Option<String>,
    bool,
    String,
    bool,
    bool,
    DateTime<Utc>,
);

pub(crate) fn copy_from_tuple(t: RoleCopyTuple) -> Result<RoleCopy, MetadataError> {
    let (name, origin, scram_verifier, password_enc, login, level, api_owner_level, pending, created_at) = t;
    Ok(RoleCopy {
        name,
        origin: RoleOrigin::parse(&origin)?,
        scram_verifier,
        password_enc,
        login,
        level: RoleLevel::parse(&level)?,
        api_owner_level,
        pending,
        created_at,
    })
}

/// The owner's role as the compute spec lists it. Debug redacts the verifier.
#[derive(Clone, PartialEq, Eq)]
pub struct NeonRole {
    pub name: String,
    pub scram_verifier: String,
}

impl std::fmt::Debug for NeonRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NeonRole")
            .field("name", &self.name)
            .field("scram_verifier", &"<redacted>")
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_hides_the_verifier() {
        let role = NeonRole {
            name: "postgres".into(),
            scram_verifier: "SCRAM-SHA-256$secret".into(),
        };
        let shown = format!("{role:?}");
        assert!(shown.contains("postgres") && shown.contains("<redacted>"));
        assert!(!shown.contains("SCRAM"));
    }

    #[test]
    fn debug_redacts_role_copy() {
        let copy = RoleCopy {
            name: "app".into(),
            origin: RoleOrigin::Api,
            scram_verifier: Some("SCRAM-SHA-256$secret".into()),
            password_enc: Some("v1:sealed-password".into()),
            login: true,
            level: RoleLevel::Plain,
            api_owner_level: false,
            pending: true,
            created_at: Utc::now(),
        };
        let shown = format!("{copy:?}");
        assert!(shown.contains("app") && shown.contains("<redacted>"), "{shown}");
        assert!(!shown.contains("SCRAM") && !shown.contains("sealed-password"), "{shown}");
        // A missing secret reads as missing, not as redacted.
        let md5 = RoleCopy { scram_verifier: None, password_enc: None, ..copy };
        let shown = format!("{md5:?}");
        assert!(!shown.contains("<redacted>"), "{shown}");
    }

    #[test]
    fn debug_redacts_copy_upsert_and_resync_write() {
        let up = CopyUpsert {
            name: "app".into(),
            origin: RoleOrigin::Sql,
            verifier: Some("SCRAM-SHA-256$secret".into()),
            password_enc: SealedChange::Set("v1:sealed-password".into()),
            login: true,
            level: RoleLevel::Plain,
            pending: false,
        };
        let write = ResyncWrite { upserts: vec![up.clone()], removals: vec![], databases: vec![] };
        for shown in [format!("{up:?}"), format!("{write:?}")] {
            assert!(shown.contains("app") && shown.contains("<redacted>"), "{shown}");
            assert!(!shown.contains("SCRAM") && !shown.contains("sealed-password"), "{shown}");
        }
        let md5 = CopyUpsert { verifier: None, password_enc: SealedChange::Keep, ..up };
        assert!(!format!("{md5:?}").contains("<redacted>"));
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NeonDatabase {
    pub name: String,
    pub owner: String,
}

impl MetadataClient {
    pub async fn neon_endpoint(&self, id: &str) -> Result<Option<NeonEndpoint>, MetadataError> {
        let row: Option<(String, String, String, String)> = sqlx::query_as(
            "SELECT id, tenant_id, timeline_id, compute_token_sha256 FROM neon_endpoints WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(
            |(id, tenant_id, timeline_id, compute_token_sha256)| NeonEndpoint {
                id,
                tenant_id,
                timeline_id,
                compute_token_sha256,
            },
        ))
    }

    /// `None` for an unknown endpoint; `Some(None)` for a known endpoint without that role, or
    /// whose row has no SCRAM verifier (an MD5 role).
    pub async fn neon_role_secret(
        &self,
        endpoint_id: &str,
        role: &str,
    ) -> Result<Option<Option<String>>, MetadataError> {
        let row: Option<(Option<String>,)> = sqlx::query_as(
            "SELECT r.scram_verifier FROM neon_endpoints e
             LEFT JOIN neon_roles r ON r.endpoint_id = e.id AND r.name = $2
             WHERE e.id = $1",
        )
        .bind(endpoint_id)
        .bind(role)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|(verifier,)| verifier))
    }

    /// Only the owner's row: what the compute spec lists.
    pub async fn neon_roles(&self, endpoint_id: &str) -> Result<Vec<NeonRole>, MetadataError> {
        Ok(self.neon_roles_versioned(endpoint_id).await?.0)
    }

    /// `(owner roles, roles_version)` read in one statement. An unknown endpoint gives
    /// `([], 0)`.
    pub async fn neon_roles_versioned(
        &self,
        endpoint_id: &str,
    ) -> Result<(Vec<NeonRole>, i64), MetadataError> {
        let rows: Vec<VersionedRoleRow> = sqlx::query_as(
            "SELECT e.roles_version, r.name, r.scram_verifier
             FROM neon_endpoints e
             LEFT JOIN neon_roles r ON r.endpoint_id = e.id AND r.origin = 'owner'
             WHERE e.id = $1",
        )
        .bind(endpoint_id)
        .fetch_all(&self.pool)
        .await?;
        let version = rows.first().map_or(0, |r| r.0);
        let roles = rows
            .into_iter()
            .filter_map(|(_, name, scram_verifier)| {
                Some(NeonRole {
                    name: name?,
                    scram_verifier: scram_verifier?,
                })
            })
            .collect();
        Ok((roles, version))
    }

    /// Owner first, then by name (`COLLATE "C"`).
    pub async fn list_roles(&self, endpoint_id: &str) -> Result<Vec<RoleRow>, MetadataError> {
        let rows: Vec<RoleRowTuple> = sqlx::query_as(
            "SELECT r.name, r.origin, r.level, r.login, r.scram_verifier IS NULL,
                    r.password_enc IS NOT NULL, r.pending, r.created_at,
                    r.password_unreadable,
                    coalesce((SELECT b.owner_password_enc IS NOT NULL FROM branches b
                              WHERE b.endpoint_id = r.endpoint_id LIMIT 1), false)
             FROM neon_roles r
             WHERE r.endpoint_id = $1
             ORDER BY (r.origin <> 'owner'), r.name COLLATE \"C\"",
        )
        .bind(endpoint_id)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(
                |(name, origin, level, login, md5, sealed, pending, created_at, unreadable, owner_sealed)| {
                    let origin = RoleOrigin::parse(&origin)?;
                    let password = match origin {
                        RoleOrigin::Owner if owner_sealed => PasswordState::Generated,
                        RoleOrigin::Owner => PasswordState::SetInSql,
                        _ if md5 => PasswordState::Md5Unusable,
                        _ if unreadable => PasswordState::Unreadable,
                        _ if sealed => PasswordState::Generated,
                        _ => PasswordState::SetInSql,
                    };
                    Ok(RoleRow {
                        name,
                        origin,
                        level: RoleLevel::parse(&level)?,
                        login,
                        password,
                        pending,
                        created_at,
                    })
                },
            )
            .collect()
    }

    /// Every row of the endpoint with its secrets, and `roles_version`, read in one statement.
    /// An unknown endpoint gives `([], 0)`.
    pub async fn role_copies(
        &self,
        endpoint_id: &str,
    ) -> Result<(Vec<RoleCopy>, i64), MetadataError> {
        let rows: Vec<VersionedCopyRow> = sqlx::query_as(
            "SELECT e.roles_version, r.name, r.origin, r.scram_verifier, r.password_enc, r.login,
                    r.level, r.api_owner_level, r.pending, r.created_at
             FROM neon_endpoints e
             LEFT JOIN neon_roles r ON r.endpoint_id = e.id
             WHERE e.id = $1
             ORDER BY (r.origin <> 'owner'), r.name COLLATE \"C\"",
        )
        .bind(endpoint_id)
        .fetch_all(&self.pool)
        .await?;
        let version = rows.first().map_or(0, |r| r.0);
        let copies = rows
            .into_iter()
            .filter_map(|(_, name, origin, verifier, sealed, login, level, api, pending, created)| {
                Some(copy_from_tuple((
                    name?, origin?, verifier, sealed, login?, level?, api?, pending?, created?,
                )))
            })
            .collect::<Result<_, _>>()?;
        Ok((copies, version))
    }

    /// The sealed password of a non-owner row; `None` when there's no row or nothing sealed.
    pub async fn role_password(
        &self,
        endpoint_id: &str,
        name: &str,
    ) -> Result<Option<String>, MetadataError> {
        Ok(sqlx::query_scalar(
            "SELECT password_enc FROM neon_roles
             WHERE endpoint_id = $1 AND name = $2 AND origin <> 'owner'",
        )
        .bind(endpoint_id)
        .bind(name)
        .fetch_optional(&self.pool)
        .await?
        .flatten())
    }

    /// The branch's Postgres databases: the main one first, then by name (`COLLATE "C"`).
    pub async fn branch_databases(
        &self,
        branch: BranchId,
    ) -> Result<Vec<BranchDatabase>, MetadataError> {
        let rows: Vec<BranchDatabaseRow> =
            sqlx::query_as(
                "SELECT name, owner, origin, data_api, data_api_error, default_schema,
                        data_api_bootstrapped
                 FROM branch_databases WHERE branch_id = $1
                 ORDER BY (origin <> 'main'), name COLLATE \"C\"",
            )
            .bind(branch.as_uuid())
            .fetch_all(&self.pool)
            .await?;
        rows.into_iter()
            .map(|(name, owner, origin, state, data_api_error, default_schema, bootstrapped)| {
                Ok(BranchDatabase {
                    name,
                    owner,
                    origin: DatabaseOrigin::parse(&origin)?,
                    data_api: DataApiState::parse(&state)?,
                    data_api_error,
                    default_schema,
                    data_api_bootstrapped: bootstrapped,
                })
            })
            .collect()
    }

    /// Claims the right to set the data API up on a database: the row becomes `enabling` (its
    /// error cleared). Locks the branch row, so two enables can't both pass the cap; the cap
    /// counts the other databases that are `on` or `enabling` (main included). A row already
    /// `enabling` or in `error` starts again.
    pub async fn begin_enable_data_api(
        &self,
        branch: BranchId,
        name: &str,
        cap: u32,
    ) -> Result<EnableStart, MetadataError> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("SELECT 1 FROM branches WHERE id = $1 FOR NO KEY UPDATE")
            .bind(branch.as_uuid())
            .execute(&mut *tx)
            .await?;
        let row: Option<(String, String)> = sqlx::query_as(
            "SELECT origin, data_api FROM branch_databases WHERE branch_id = $1 AND name = $2",
        )
        .bind(branch.as_uuid())
        .bind(name)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((origin, state)) = row else {
            return Ok(EnableStart::Unknown);
        };
        if origin == "system" {
            return Ok(EnableStart::System);
        }
        if state == "on" {
            return Ok(EnableStart::AlreadyOn);
        }
        let others: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM branch_databases
             WHERE branch_id = $1 AND name <> $2 AND data_api IN ('on', 'enabling')",
        )
        .bind(branch.as_uuid())
        .bind(name)
        .fetch_one(&mut *tx)
        .await?;
        if others >= i64::from(cap) {
            return Ok(EnableStart::OverCap(cap));
        }
        sqlx::query(
            "UPDATE branch_databases
             SET data_api = 'enabling', data_api_error = NULL, data_api_bootstrapped = true
             WHERE branch_id = $1 AND name = $2",
        )
        .bind(branch.as_uuid())
        .bind(name)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(EnableStart::Started)
    }

    /// Ends an enable: `Ok` makes the row `on`, `Err(text)` makes it `error` with the text.
    /// Only a row still `enabling` moves (one switched off meanwhile stays off); false otherwise.
    pub async fn finish_enable_data_api(
        &self,
        branch: BranchId,
        name: &str,
        outcome: Result<(), String>,
    ) -> Result<bool, MetadataError> {
        let (state, error) = match &outcome {
            Ok(()) => ("on", None),
            Err(text) => ("error", Some(text.as_str())),
        };
        let updated = sqlx::query(
            "UPDATE branch_databases SET data_api = $3, data_api_error = $4
             WHERE branch_id = $1 AND name = $2 AND data_api = 'enabling'",
        )
        .bind(branch.as_uuid())
        .bind(name)
        .bind(state)
        .bind(error)
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(updated == 1)
    }

    /// Switches a database's data API off, clearing any error. False for the main database
    /// (always served) and for an unknown name.
    pub async fn set_data_api_off(
        &self,
        branch: BranchId,
        name: &str,
    ) -> Result<bool, MetadataError> {
        let updated = sqlx::query(
            "UPDATE branch_databases SET data_api = 'off', data_api_error = NULL
             WHERE branch_id = $1 AND name = $2 AND origin <> 'main'",
        )
        .bind(branch.as_uuid())
        .bind(name)
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(updated == 1)
    }

    /// Sets the schema requests to a database use when they name none. False for an unknown
    /// name. The column's CHECK bounds the value (`^[a-z_][a-z0-9_]{0,62}$`); callers
    /// validate first for a friendly message.
    pub async fn set_database_default_schema(
        &self,
        branch: BranchId,
        name: &str,
        schema: &str,
    ) -> Result<bool, MetadataError> {
        let updated = sqlx::query(
            "UPDATE branch_databases SET default_schema = $3 WHERE branch_id = $1 AND name = $2",
        )
        .bind(branch.as_uuid())
        .bind(name)
        .bind(schema)
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(updated == 1)
    }

    /// The databases the data API serves (state `on`): the main one first, then by name
    /// (`COLLATE "C"`).
    pub async fn served_databases(
        &self,
        branch: BranchId,
    ) -> Result<Vec<ServedDatabase>, MetadataError> {
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT name, default_schema FROM branch_databases
             WHERE branch_id = $1 AND data_api = 'on'
             ORDER BY (origin <> 'main'), name COLLATE \"C\"",
        )
        .bind(branch.as_uuid())
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(name, default_schema)| ServedDatabase { name, default_schema })
            .collect())
    }

    /// Every `(branch, database)` row in state `enabling`, for reconcile to retry the ones whose
    /// enable job was lost.
    pub async fn enabling_databases(&self) -> Result<Vec<(BranchId, String)>, MetadataError> {
        let rows: Vec<(uuid::Uuid, String)> = sqlx::query_as(
            "SELECT branch_id, name FROM branch_databases WHERE data_api = 'enabling'
             ORDER BY branch_id, name COLLATE \"C\"",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|(b, n)| (BranchId(b), n)).collect())
    }

    /// Gives a child branch its parent's data API flags, per database name: `on` stays `on`,
    /// `enabling` and `error` become `off`, and the schema is copied. Call it after the child's
    /// first resync wrote its rows. A branch without a parent, and names the parent lacks, are
    /// left alone.
    pub async fn copy_parent_database_flags(&self, child: BranchId) -> Result<(), MetadataError> {
        sqlx::query(
            "UPDATE branch_databases c
             SET data_api = CASE p.data_api WHEN 'on' THEN 'on' ELSE 'off' END,
                 data_api_error = NULL,
                 default_schema = p.default_schema,
                 data_api_bootstrapped = c.data_api_bootstrapped OR p.data_api_bootstrapped
             FROM branches b
             JOIN branch_databases p ON p.branch_id = b.parent_branch_id
             WHERE b.id = $1 AND c.branch_id = $1 AND p.name = c.name AND c.origin <> 'system'",
        )
        .bind(child.as_uuid())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Records the compute settings version the endpoint's compute was last created with.
    pub async fn set_compute_settings_version(
        &self,
        endpoint_id: &str,
        version: i32,
    ) -> Result<(), MetadataError> {
        sqlx::query("UPDATE neon_endpoints SET compute_settings_version = $2 WHERE id = $1")
            .bind(endpoint_id)
            .bind(version)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn neon_databases(
        &self,
        endpoint_id: &str,
    ) -> Result<Vec<NeonDatabase>, MetadataError> {
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT name, owner FROM neon_databases WHERE endpoint_id = $1 ORDER BY name",
        )
        .bind(endpoint_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(name, owner)| NeonDatabase { name, owner })
            .collect())
    }

    pub async fn neon_endpoints_for_tenant(
        &self,
        tenant_id: &str,
    ) -> Result<Vec<String>, MetadataError> {
        Ok(
            sqlx::query_scalar("SELECT id FROM neon_endpoints WHERE tenant_id = $1 ORDER BY id")
                .bind(tenant_id)
                .fetch_all(&self.pool)
                .await?,
        )
    }

    pub async fn neon_endpoints_for_timeline(
        &self,
        timeline_id: &str,
    ) -> Result<Vec<String>, MetadataError> {
        Ok(
            sqlx::query_scalar("SELECT id FROM neon_endpoints WHERE timeline_id = $1 ORDER BY id")
                .bind(timeline_id)
                .fetch_all(&self.pool)
                .await?,
        )
    }
}

impl MetadataClient {
    /// Waits until this is the only holder of `endpoint_id`'s lock, across every process on
    /// this metadata database. A transaction-level advisory lock in its own transaction, so
    /// nothing can return a still-locked connection to the pool. Postgres queues the waiters
    /// first come, first served; each holds a lock-pool connection while it waits. After
    /// [`ENDPOINT_LOCK_TIMEOUT`] it gives up with [`MetadataError::EndpointLockTimeout`].
    pub async fn lock_endpoint(&self, endpoint_id: &str) -> Result<EndpointLock, MetadataError> {
        let mut tx = self.locks.begin().await?;
        // `SET LOCAL lock_timeout`, as a function so the value can be bound.
        sqlx::query("SELECT set_config('lock_timeout', $1, true)")
            .bind(format!("{}ms", ENDPOINT_LOCK_TIMEOUT.as_millis()))
            .execute(&mut *tx)
            .await?;
        let locked = sqlx::query("SELECT pg_advisory_xact_lock($1, hashtext($2))")
            .bind(ENDPOINT_LOCK_CLASS)
            .bind(endpoint_id)
            .execute(&mut *tx)
            .await;
        match locked {
            Ok(_) => Ok(EndpointLock { _tx: tx }),
            Err(sqlx::Error::Database(err))
                if err.code().as_deref() == Some(LOCK_NOT_AVAILABLE) =>
            {
                Err(MetadataError::EndpointLockTimeout(endpoint_id.to_string()))
            }
            Err(err) => Err(err.into()),
        }
    }

    /// Roles and databases cascade; branches.endpoint_id becomes NULL. Missing is fine.
    pub async fn delete_endpoint(&self, endpoint_id: &str) -> Result<(), MetadataError> {
        sqlx::query("DELETE FROM neon_endpoints WHERE id = $1")
            .bind(endpoint_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// One transaction: sealed password (only while the branch is ready, `normal` and linked to
    /// endpoint_id), the role's verifier and `pending`, `roles_version += 1`,
    /// needs_reconfigure = true. False (nothing written) otherwise.
    pub async fn replace_owner_secret(
        &self,
        id: BranchId,
        endpoint_id: &str,
        role: &str,
        verifier: &str,
        sealed_password: &str,
    ) -> Result<bool, MetadataError> {
        let mut tx = self.pool.begin().await?;
        // Lock order: branch row, then endpoint row, then role rows (as the timeline switches).
        let sealed = sqlx::query(
            "UPDATE branches SET owner_password_enc = $3, updated_at = now()
             WHERE id = $1 AND endpoint_id = $2 AND status = 'ready' AND kind = 'normal'",
        )
        .bind(id.as_uuid())
        .bind(endpoint_id)
        .bind(sealed_password)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if sealed != 1 || !lock_endpoint_row(&mut tx, endpoint_id).await? {
            return Ok(false);
        }
        let verified = sqlx::query(
            "UPDATE neon_roles SET scram_verifier = $3, pending = true, password_unreadable = false
             WHERE endpoint_id = $1 AND name = $2 AND origin = 'owner'",
        )
        .bind(endpoint_id)
        .bind(role)
        .bind(verifier)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if verified != 1 {
            return Ok(false);
        }
        bump_roles_version(&mut tx, endpoint_id).await?;
        tx.commit().await?;
        Ok(true)
    }

    /// API create: one transaction that locks the endpoint row (`FOR UPDATE`, which serializes
    /// the writers of its roles), counts its copies (rows of origin `sql` or `api` with a
    /// verifier; at `cap` or more is `CapReached`), and inserts an `api` row with
    /// `pending = true` and the level from `owner_level` (a name already there is `Exists`).
    /// Then `roles_version += 1` (no reconfigure: the spec lists only the owner). An unknown endpoint is a
    /// `RowNotFound` database error.
    pub async fn insert_api_role(
        &self,
        endpoint_id: &str,
        name: &str,
        owner_level: bool,
        verifier: &str,
        password_enc: &str,
        cap: u32,
    ) -> Result<InsertRoleOutcome, MetadataError> {
        let mut tx = self.pool.begin().await?;
        sqlx::query_scalar::<_, String>("SELECT id FROM neon_endpoints WHERE id = $1 FOR UPDATE")
            .bind(endpoint_id)
            .fetch_one(&mut *tx)
            .await?;
        if copy_count(&mut tx, endpoint_id).await? >= i64::from(cap) {
            return Ok(InsertRoleOutcome::CapReached);
        }
        let level = if owner_level { RoleLevel::OwnerLevel } else { RoleLevel::Plain };
        let inserted = sqlx::query(
            "INSERT INTO neon_roles
                 (endpoint_id, name, scram_verifier, origin, level, api_owner_level, password_enc,
                  pending, password_unreadable)
             VALUES ($1, $2, $3, 'api', $4, $5, $6, true, false)
             ON CONFLICT DO NOTHING",
        )
        .bind(endpoint_id)
        .bind(name)
        .bind(verifier)
        .bind(level.as_str())
        .bind(owner_level)
        .bind(password_enc)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if inserted == 0 {
            return Ok(InsertRoleOutcome::Exists);
        }
        bump_roles_version_only(&mut tx, endpoint_id).await?;
        tx.commit().await?;
        Ok(InsertRoleOutcome::Inserted)
    }

    /// One transaction: the verifier and sealed password of any non-owner row, `pending = true`,
    /// `roles_version += 1` (no reconfigure: the spec lists only the owner). False when there's
    /// no such row.
    pub async fn replace_role_secret(
        &self,
        endpoint_id: &str,
        name: &str,
        verifier: &str,
        password_enc: &str,
    ) -> Result<bool, MetadataError> {
        let mut tx = self.pool.begin().await?;
        if !lock_endpoint_row(&mut tx, endpoint_id).await? {
            return Ok(false);
        }
        let updated = sqlx::query(
            "UPDATE neon_roles SET scram_verifier = $3, password_enc = $4, pending = true,
                 password_unreadable = false
             WHERE endpoint_id = $1 AND name = $2 AND origin <> 'owner'",
        )
        .bind(endpoint_id)
        .bind(name)
        .bind(verifier)
        .bind(password_enc)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if updated != 1 {
            return Ok(false);
        }
        bump_roles_version_only(&mut tx, endpoint_id).await?;
        tx.commit().await?;
        Ok(true)
    }

    /// Clears `pending` of one row, only while `roles_version` still equals `version` (what the
    /// caller read before applying). True when cleared. The endpoint row is locked first, like
    /// every writer, so a write in flight is waited for and then seen.
    pub async fn confirm_role(
        &self,
        endpoint_id: &str,
        name: &str,
        version: i64,
    ) -> Result<bool, MetadataError> {
        let mut tx = self.pool.begin().await?;
        let current: Option<i64> = sqlx::query_scalar(
            "SELECT roles_version FROM neon_endpoints WHERE id = $1 FOR UPDATE",
        )
        .bind(endpoint_id)
        .fetch_optional(&mut *tx)
        .await?;
        if current != Some(version) {
            return Ok(false);
        }
        let cleared = sqlx::query(
            "UPDATE neon_roles SET pending = false
             WHERE endpoint_id = $1 AND name = $2 AND pending",
        )
        .bind(endpoint_id)
        .bind(name)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        tx.commit().await?;
        Ok(cleared == 1)
    }

    /// One transaction: deletes a non-owner row, `roles_version += 1` (no reconfigure: the spec
    /// lists only the owner). False when there was none (the owner's row is never deleted here).
    pub async fn delete_role_copy(
        &self,
        endpoint_id: &str,
        name: &str,
    ) -> Result<bool, MetadataError> {
        let mut tx = self.pool.begin().await?;
        if !lock_endpoint_row(&mut tx, endpoint_id).await? {
            return Ok(false);
        }
        let deleted = sqlx::query(
            "DELETE FROM neon_roles WHERE endpoint_id = $1 AND name = $2 AND origin <> 'owner'",
        )
        .bind(endpoint_id)
        .bind(name)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if deleted != 1 {
            return Ok(false);
        }
        bump_roles_version_only(&mut tx, endpoint_id).await?;
        tx.commit().await?;
        Ok(true)
    }

    /// Lifts both resync guards of the endpoint: every role's `forwarded_at` and the endpoint's
    /// `dbs_forwarded_at` become NULL (the catalog was replaced, so nothing in flight is
    /// pending). Locks the endpoint row first, as the other writers do.
    pub async fn clear_forwarded(&self, endpoint_id: &str) -> Result<(), MetadataError> {
        let mut tx = self.pool.begin().await?;
        lock_endpoint_row(&mut tx, endpoint_id).await?;
        sqlx::query("UPDATE neon_roles SET forwarded_at = NULL WHERE endpoint_id = $1")
            .bind(endpoint_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE neon_endpoints SET dbs_forwarded_at = NULL WHERE id = $1")
            .bind(endpoint_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Marks a row whose sealed password was dropped (it didn't open): `password_enc` becomes
    /// NULL and `password_unreadable` true, so [`PasswordState::Unreadable`] shows. Any later
    /// write of a password or verifier to the row clears the mark. No such row is a no-op.
    pub async fn mark_password_unreadable(
        &self,
        endpoint_id: &str,
        name: &str,
    ) -> Result<(), MetadataError> {
        let mut tx = self.pool.begin().await?;
        if !lock_endpoint_row(&mut tx, endpoint_id).await? {
            return Ok(());
        }
        sqlx::query(
            "UPDATE neon_roles SET password_enc = NULL, password_unreadable = true
             WHERE endpoint_id = $1 AND name = $2",
        )
        .bind(endpoint_id)
        .bind(name)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// The resync's one transaction. Locks the endpoint row; if `roles_version` isn't
    /// `read_version` nothing is written (`VersionMoved`). Otherwise applies `write`, skipping
    /// in SQL every row whose `forwarded_at` is under 10 s old (a DDL the receiver answered
    /// whose commit the catalog read may not have seen) and never touching the owner's row, and
    /// sets the branch's `branch_databases` to exactly `write.databases`. A database's origin is
    /// `system` when observed as system, `main` for the recorded main database's name
    /// (`neon_databases`), `sql` otherwise. `roles_version += 1` when anything changed (the
    /// compute needs no reconfigure for this). An unknown endpoint is a `RowNotFound` error.
    pub async fn apply_resync(
        &self,
        endpoint_id: &str,
        branch: BranchId,
        read_version: i64,
        write: &ResyncWrite,
    ) -> Result<ResyncApplied, MetadataError> {
        let mut tx = self.pool.begin().await?;
        let current: i64 = sqlx::query_scalar(
            "SELECT roles_version FROM neon_endpoints WHERE id = $1 FOR UPDATE",
        )
        .bind(endpoint_id)
        .fetch_one(&mut *tx)
        .await?;
        if current != read_version {
            return Ok(ResyncApplied::VersionMoved);
        }
        let mut changed = Vec::new();
        for up in &write.upserts {
            let (mode, sealed) = match &up.password_enc {
                SealedChange::Keep => (0_i32, None),
                SealedChange::Clear => (1, None),
                SealedChange::Set(sealed) => (2, Some(sealed.as_str())),
            };
            let name: Option<String> = sqlx::query_scalar(
                "INSERT INTO neon_roles AS r
                     (endpoint_id, name, scram_verifier, origin, password_enc, login, level, pending)
                 VALUES ($1, $2, $3, $4, CASE WHEN $7::int = 2 THEN $8 ELSE NULL END, $5, $6, $9)
                 ON CONFLICT (endpoint_id, name) DO UPDATE SET
                     scram_verifier = EXCLUDED.scram_verifier,
                     password_enc = CASE $7::int WHEN 0 THEN r.password_enc WHEN 1 THEN NULL
                                    ELSE $8 END,
                     login = EXCLUDED.login,
                     level = EXCLUDED.level,
                     pending = EXCLUDED.pending,
                     password_unreadable = CASE WHEN r.scram_verifier IS DISTINCT FROM EXCLUDED.scram_verifier
                                                  OR $7::int <> 0
                                                THEN false ELSE r.password_unreadable END
                 WHERE r.origin <> 'owner'
                   AND (r.forwarded_at IS NULL OR r.forwarded_at <= now() - interval '10 seconds')
                   AND (r.scram_verifier IS DISTINCT FROM EXCLUDED.scram_verifier
                        OR r.login IS DISTINCT FROM EXCLUDED.login
                        OR r.level IS DISTINCT FROM EXCLUDED.level
                        OR r.pending IS DISTINCT FROM EXCLUDED.pending
                        OR r.password_enc IS DISTINCT FROM
                           CASE $7::int WHEN 0 THEN r.password_enc WHEN 1 THEN NULL ELSE $8 END)
                 RETURNING r.name",
            )
            .bind(endpoint_id)
            .bind(&up.name)
            .bind(up.verifier.as_deref())
            .bind(up.origin.as_str())
            .bind(up.login)
            .bind(up.level.as_str())
            .bind(mode)
            .bind(sealed)
            .bind(up.pending)
            .fetch_optional(&mut *tx)
            .await?;
            changed.extend(name);
        }
        if !write.removals.is_empty() {
            let removed: Vec<String> = sqlx::query_scalar(
                "DELETE FROM neon_roles
                 WHERE endpoint_id = $1 AND name = ANY($2) AND origin <> 'owner'
                   AND (forwarded_at IS NULL OR forwarded_at <= now() - interval '10 seconds')
                 RETURNING name",
            )
            .bind(endpoint_id)
            .bind(&write.removals)
            .fetch_all(&mut *tx)
            .await?;
            changed.extend(removed);
        }
        let databases_changed = set_databases(&mut tx, endpoint_id, branch, &write.databases).await?;
        if !changed.is_empty() || databases_changed {
            sqlx::query("UPDATE neon_endpoints SET roles_version = roles_version + 1 WHERE id = $1")
                .bind(endpoint_id)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(ResyncApplied::Applied { changed })
    }

    /// Clears the flag and the owner row's `pending` (the other rows' go through
    /// [`Self::confirm_role`]), only while `roles_version` still equals
    /// `applied_version` (what the caller read before applying). True when cleared.
    ///
    /// Every writer locks the endpoint row first, so this `UPDATE` waits for one in flight and
    /// then re-checks the version against its committed value (READ COMMITTED).
    pub async fn clear_needs_reconfigure(
        &self,
        endpoint_id: &str,
        applied_version: i64,
    ) -> Result<bool, MetadataError> {
        let mut tx = self.pool.begin().await?;
        let cleared = sqlx::query(
            "UPDATE neon_endpoints SET needs_reconfigure = false
             WHERE id = $1 AND needs_reconfigure AND roles_version = $2",
        )
        .bind(endpoint_id)
        .bind(applied_version)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if cleared != 1 {
            return Ok(false);
        }
        sqlx::query(
            "UPDATE neon_roles SET pending = false
             WHERE endpoint_id = $1 AND pending AND origin = 'owner'",
        )
            .bind(endpoint_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(true)
    }

    pub async fn replace_compute_token(
        &self,
        endpoint_id: &str,
        sha256_hex: &str,
    ) -> Result<(), MetadataError> {
        sqlx::query("UPDATE neon_endpoints SET compute_token_sha256 = $2 WHERE id = $1")
            .bind(endpoint_id)
            .bind(sha256_hex)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Every endpoint, left-joined to the branch linked to it.
    pub async fn list_endpoints_for_reconcile(&self) -> Result<Vec<EndpointState>, MetadataError> {
        let rows = sqlx::query(
            "SELECT e.id, e.tenant_id, e.needs_reconfigure, e.compute_settings_version,
                    b.id AS branch_id, b.status
             FROM neon_endpoints e
             LEFT JOIN branches b ON b.endpoint_id = e.id
             ORDER BY e.id",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|r| {
                let status: Option<String> = r.get("status");
                Ok(EndpointState {
                    id: r.get("id"),
                    tenant_id: r.get("tenant_id"),
                    branch_id: r.get::<Option<uuid::Uuid>, _>("branch_id").map(BranchId),
                    branch_status: status.as_deref().map(BranchStatus::parse).transpose()?,
                    needs_reconfigure: r.get("needs_reconfigure"),
                    compute_settings_version: r.get("compute_settings_version"),
                })
            })
            .collect()
    }
}

/// Locks the endpoint's row (`FOR UPDATE`): every writer of its roles does this first, so they
/// queue, and [`MetadataClient::clear_needs_reconfigure`] waits for the one in flight. False
/// for an unknown endpoint.
async fn lock_endpoint_row(
    tx: &mut Transaction<'_, Postgres>,
    endpoint_id: &str,
) -> Result<bool, MetadataError> {
    let id: Option<String> =
        sqlx::query_scalar("SELECT id FROM neon_endpoints WHERE id = $1 FOR UPDATE")
            .bind(endpoint_id)
            .fetch_optional(&mut **tx)
            .await?;
    Ok(id.is_some())
}

async fn bump_roles_version(
    tx: &mut Transaction<'_, Postgres>,
    endpoint_id: &str,
) -> Result<(), MetadataError> {
    sqlx::query(
        "UPDATE neon_endpoints SET roles_version = roles_version + 1, needs_reconfigure = true
         WHERE id = $1",
    )
    .bind(endpoint_id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// `roles_version += 1` alone: a write to a role the compute spec doesn't list (every one but
/// the owner's), so the compute needs no reconfigure.
async fn bump_roles_version_only(
    tx: &mut Transaction<'_, Postgres>,
    endpoint_id: &str,
) -> Result<(), MetadataError> {
    sqlx::query("UPDATE neon_endpoints SET roles_version = roles_version + 1 WHERE id = $1")
        .bind(endpoint_id)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// Rows of origin `sql` or `api` with a verifier: what the cap counts (an MD5 row can't log in
/// through the proxy, and the owner has its own).
pub(crate) async fn copy_count(
    tx: &mut Transaction<'_, Postgres>,
    endpoint_id: &str,
) -> Result<i64, MetadataError> {
    Ok(sqlx::query_scalar(
        "SELECT count(*) FROM neon_roles
         WHERE endpoint_id = $1 AND origin IN ('sql', 'api') AND scram_verifier IS NOT NULL",
    )
    .bind(endpoint_id)
    .fetch_one(&mut **tx)
    .await?)
}

/// Sets the branch's `branch_databases` to exactly `observed`. True when anything changed.
/// Does nothing while the endpoint's `dbs_forwarded_at` is under 10 s old.
async fn set_databases(
    tx: &mut Transaction<'_, Postgres>,
    endpoint_id: &str,
    branch: BranchId,
    observed: &[ObservedDatabase],
) -> Result<bool, MetadataError> {
    // A database DDL the receiver answered stamps the endpoint; the catalog read may not have
    // seen its commit (and a drop leaves no row to stamp), so leave the rows alone for 10 s.
    let recent: bool = sqlx::query_scalar(
        "SELECT coalesce(dbs_forwarded_at > now() - interval '10 seconds', false)
         FROM neon_endpoints WHERE id = $1",
    )
    .bind(endpoint_id)
    .fetch_one(&mut **tx)
    .await?;
    if recent {
        return Ok(false);
    }
    let main: Option<String> = sqlx::query_scalar(
        "SELECT name FROM neon_databases WHERE endpoint_id = $1 ORDER BY name LIMIT 1",
    )
    .bind(endpoint_id)
    .fetch_optional(&mut **tx)
    .await?;
    let existing: Vec<(String, String, String)> =
        sqlx::query_as("SELECT name, owner, origin FROM branch_databases WHERE branch_id = $1")
            .bind(branch.as_uuid())
            .fetch_all(&mut **tx)
            .await?;
    let mut desired: Vec<(&str, &str, DatabaseOrigin)> = observed
        .iter()
        .map(|d| {
            let origin = if d.system {
                DatabaseOrigin::System
            } else if main.as_deref() == Some(d.name.as_str()) {
                DatabaseOrigin::Main
            } else {
                DatabaseOrigin::Sql
            };
            (d.name.as_str(), d.owner.as_str(), origin)
        })
        .collect();
    // The unique `main` index: a row giving up `main` is written before one taking it.
    desired.sort_by_key(|(_, _, origin)| *origin == DatabaseOrigin::Main);
    let mut changed = false;
    let keep: Vec<&str> = desired.iter().map(|(name, _, _)| *name).collect();
    let removed: Vec<String> = sqlx::query_scalar(
        "DELETE FROM branch_databases WHERE branch_id = $1 AND NOT (name = ANY($2)) RETURNING name",
    )
    .bind(branch.as_uuid())
    .bind(&keep)
    .fetch_all(&mut **tx)
    .await?;
    changed |= !removed.is_empty();
    for name in &removed {
        crate::ddl::drop_name_from_key_lists(tx, branch, name).await?;
    }
    for (name, owner, origin) in desired {
        let same = existing
            .iter()
            .any(|(n, o, org)| n == name && o == owner && org == origin.as_str());
        if same {
            continue;
        }
        sqlx::query(
            "INSERT INTO branch_databases (branch_id, name, owner, origin, data_api, data_api_bootstrapped)
             VALUES ($1, $2, $3, $4, CASE WHEN $4::text = 'main' THEN 'on' ELSE 'off' END,
                     $4::text = 'main')
             ON CONFLICT (branch_id, name) DO UPDATE
             SET owner = EXCLUDED.owner,
                 origin = EXCLUDED.origin,
                 data_api_bootstrapped = branch_databases.data_api_bootstrapped
                                         OR EXCLUDED.origin = 'main',
                 data_api = CASE EXCLUDED.origin WHEN 'system' THEN 'off'
                                                 WHEN 'main' THEN 'on'
                                                 ELSE branch_databases.data_api END",
        )
        .bind(branch.as_uuid())
        .bind(name)
        .bind(owner)
        .bind(origin.as_str())
        .execute(&mut **tx)
        .await?;
        changed = true;
    }
    Ok(changed)
}
