//! Personal Postgres logins per (member, Chelabase database), suspension, revocation and the
//! access log's writes for them (members hub step 1, spec Part 6).
//!
//! A login is one `personal_logins` record (its name and plain SCRAM verifier, P5) and, per
//! branch where the worker made the role, one `neon_roles` row of origin `personal` that the
//! proxy reads. The rows belong to their record (FK, CHECKs): only the functions here write
//! them, and the generic `neon_roles` writers leave them alone (S1).
//!
//! Revocation (MH-9, P3): every write that can make `chelabase_personal_login_qualifies`
//! false revokes the logins it disqualifies in its own transaction ([`revoke_unqualified_logins`]),
//! and the explicit writes (suspend, revoke-now, offboarding, removal) revoke them all
//! ([`revoke_logins`]). A revoked record and its rows hold no verifier, and nothing writes one
//! back. Lock order is the members module's: the tenant row, then the member row.

use std::collections::BTreeSet;

use chelabase_types::TenantId;
use chelabase_types::access::PlatformRole;
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use sqlx::Row;
use sqlx::postgres::PgRow;
use thiserror::Error;
use uuid::Uuid;

use crate::members::{
    AccessAction, AccessWriteError, MemberStatus, NewEvent, Tx, lock_member,
    lock_member_by_identity_user, lock_tenant, member_tenant, other_active_admins, touched_projects,
};
use crate::{MetadataClient, MetadataError};

/// Why a login was revoked (`personal_logins.revoked_reason`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RevokeReason {
    AccessLost,
    Suspended,
    RevokedNow,
    IdentityOffboarded,
    MemberRemoved,
}

impl RevokeReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AccessLost => "access_lost",
            Self::Suspended => "suspended",
            Self::RevokedNow => "revoked_now",
            Self::IdentityOffboarded => "identity_offboarded",
            Self::MemberRemoved => "member_removed",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "access_lost" => Some(Self::AccessLost),
            "suspended" => Some(Self::Suspended),
            "revoked_now" => Some(Self::RevokedNow),
            "identity_offboarded" => Some(Self::IdentityOffboarded),
            "member_removed" => Some(Self::MemberRemoved),
            _ => None,
        }
    }
}

/// Where a member's access to a project comes from (the roster's `via`, P13).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum GrantVia {
    Own,
    Team,
    Both,
}

/// A member's best level on a project among its sources (P13).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberProjectRole {
    pub member: Uuid,
    pub project: Uuid,
    pub platform_role: PlatformRole,
    pub via: GrantVia,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginStatus {
    Active,
    Expired,
    Revoked,
}

/// A personal login as the API lists it (never its verifier).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersonalLogin {
    pub member_id: Uuid,
    pub database_id: Uuid,
    pub project_id: Uuid,
    pub database_name: String,
    pub role_name: String,
    pub valid_until: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
    pub revoked_at: Option<DateTime<Utc>>,
    pub revoked_reason: Option<RevokeReason>,
    pub status: LoginStatus,
}

/// The worker's read: `qualifies` from `chelabase_personal_login_qualifies` in the same query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersonalLoginRecord {
    pub member_id: Uuid,
    pub database_id: Uuid,
    pub role_name: String,
    pub scram_verifier: Option<String>,
    pub valid_until: DateTime<Utc>,
    pub revoked: bool,
    pub qualifies: bool,
}

/// Where a login's proxy rows are: one per branch endpoint holding a personal `neon_roles` row.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct LoginEndpoint {
    pub branch_id: Uuid,
    pub endpoint_id: String,
    pub role_name: String,
}

#[derive(Debug, Error)]
pub enum LoginWriteError {
    #[error("the member doesn't qualify for a personal login on this database")]
    NotQualified,
    #[error("a live personal login exists on this database")]
    Exists,
    #[error("the name is taken on this database")]
    NameTaken,
    #[error("not found")]
    NotFound,
    #[error(transparent)]
    Access(AccessWriteError),
    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),
}

/// One `access_events` row written by a caller outside this crate's own writes.
#[derive(Debug, Clone)]
pub struct AccessEventInput<'a> {
    pub tenant: TenantId,
    pub actor: Option<Uuid>,
    pub action: AccessAction,
    pub target: Option<Uuid>,
    pub project: Option<Uuid>,
    pub database: Option<Uuid>,
    pub branch: Option<Uuid>,
    pub before: Option<&'a Value>,
    pub after: Option<&'a Value>,
}

// --- Reads --------------------------------------------------------------------------------------

/// The select list of [`PersonalLogin`] over `personal_logins l JOIN databases d`. A macro, so
/// the queries stay `&'static str`. `expired` is judged by the database's clock, like the
/// proxy's read.
macro_rules! login_columns {
    () => {
        "l.member_id, l.database_id, d.project_id, d.name AS database_name, l.role_name,
         l.valid_until, l.created_at, l.revoked_at, l.revoked_reason,
         l.valid_until <= now() AS expired"
    };
}

fn login_from_row(r: &PgRow) -> Result<PersonalLogin, sqlx::Error> {
    let revoked_at: Option<DateTime<Utc>> = r.try_get("revoked_at")?;
    let reason: Option<String> = r.try_get("revoked_reason")?;
    let revoked_reason = reason
        .map(|s| {
            RevokeReason::parse(&s)
                .ok_or_else(|| sqlx::Error::Decode(format!("invalid revoke reason {s:?}").into()))
        })
        .transpose()?;
    let status = if revoked_at.is_some() {
        LoginStatus::Revoked
    } else if r.try_get::<bool, _>("expired")? {
        LoginStatus::Expired
    } else {
        LoginStatus::Active
    };
    Ok(PersonalLogin {
        member_id: r.try_get("member_id")?,
        database_id: r.try_get("database_id")?,
        project_id: r.try_get("project_id")?,
        database_name: r.try_get("database_name")?,
        role_name: r.try_get("role_name")?,
        valid_until: r.try_get("valid_until")?,
        created_at: r.try_get("created_at")?,
        revoked_at,
        revoked_reason,
        status,
    })
}

/// `member`'s login on `database`, read in the caller's transaction.
async fn login_tx(tx: &mut Tx, member: Uuid, database: Uuid) -> Result<PersonalLogin, sqlx::Error> {
    let row = sqlx::query(concat!(
        "SELECT ",
        login_columns!(),
        " FROM personal_logins l JOIN databases d ON d.id = l.database_id
          WHERE l.member_id = $1 AND l.database_id = $2"
    ))
    .bind(member)
    .bind(database)
    .fetch_one(&mut **tx)
    .await?;
    login_from_row(&row)
}

/// The personal rows of `member` (on `database` only, when given), wherever they are: one per
/// endpoint linked to a branch.
async fn endpoints_tx(
    tx: &mut Tx,
    member: Uuid,
    database: Option<Uuid>,
) -> Result<Vec<LoginEndpoint>, sqlx::Error> {
    let rows: Vec<(Uuid, String, String)> = sqlx::query_as(
        "SELECT b.id, r.endpoint_id, r.name
         FROM neon_roles r JOIN branches b ON b.endpoint_id = r.endpoint_id
         WHERE r.member_id = $1 AND r.origin = 'personal'
           AND ($2::uuid IS NULL OR r.personal_database_id = $2)
         ORDER BY b.id, r.endpoint_id, r.name",
    )
    .bind(member)
    .bind(database)
    .fetch_all(&mut **tx)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(branch_id, endpoint_id, role_name)| LoginEndpoint {
            branch_id,
            endpoint_id,
            role_name,
        })
        .collect())
}

// --- Revocation --------------------------------------------------------------------------------

/// Revokes `member`'s live logins (with `only_unqualified`, only those whose
/// `chelabase_personal_login_qualifies` is false as this transaction sees it): `revoked_at`,
/// `reason`, no verifier, and no verifier on any of their proxy rows, in one statement.
/// Returns the revoked logins as `(project, database, role name)`.
async fn revoke_where(
    tx: &mut Tx,
    member: Uuid,
    reason: RevokeReason,
    only_unqualified: bool,
) -> Result<Vec<(Uuid, Uuid, String)>, sqlx::Error> {
    // I1: lock the live records first, in a statement of their own. A worker write holding one
    // `FOR SHARE` (`upsert_personal_role_row`) finishes before this returns, so the next
    // statement's snapshot sees the row it landed and clears it. Lock order, as in
    // `create_personal_login` and `reset_personal_login`: the member lock (the caller's), then
    // the records (`FOR UPDATE`, by database), then their `neon_roles` rows.
    sqlx::query(
        "SELECT 1 FROM personal_logins WHERE member_id = $1 AND revoked_at IS NULL
         ORDER BY database_id FOR UPDATE",
    )
    .bind(member)
    .execute(&mut **tx)
    .await?;
    // A data-modifying CTE runs to completion whether or not the main query reads it.
    sqlx::query_as(
        "WITH revoked AS (
             UPDATE personal_logins
             SET revoked_at = now(), revoked_reason = $2, scram_verifier = NULL, updated_at = now()
             WHERE member_id = $1 AND revoked_at IS NULL
               AND (NOT $3 OR NOT chelabase_personal_login_qualifies(member_id, database_id))
             RETURNING member_id, database_id, role_name),
         cleared AS (
             UPDATE neon_roles r SET scram_verifier = NULL
             FROM revoked v
             WHERE r.member_id = v.member_id AND r.personal_database_id = v.database_id
               AND r.name = v.role_name AND r.origin = 'personal'
             RETURNING 1)
         SELECT d.project_id, v.database_id, v.role_name
         FROM revoked v JOIN databases d ON d.id = v.database_id
         ORDER BY d.project_id, v.database_id",
    )
    .bind(member)
    .bind(reason.as_str())
    .bind(only_unqualified)
    .fetch_all(&mut **tx)
    .await
}

/// The `personal_logins_revoked` event of `revoked` (never a secret): `after: {count, reason,
/// logins}` plus `extra`'s fields, with the project and database when they are the only ones.
async fn write_revoked_event(
    tx: &mut Tx,
    actor: Option<Uuid>,
    member: Uuid,
    reason: RevokeReason,
    revoked: &[(Uuid, Uuid, String)],
    extra: Option<(&str, Value)>,
) -> Result<(), sqlx::Error> {
    let tenant: Uuid = sqlx::query_scalar("SELECT tenant_id FROM members WHERE id = $1")
        .bind(member)
        .fetch_one(&mut **tx)
        .await?;
    let logins: Vec<Value> = revoked
        .iter()
        .map(|(_, database, role)| json!({ "database_id": database, "role_name": role }))
        .collect();
    let mut after = json!({ "count": revoked.len(), "reason": reason.as_str(), "logins": logins });
    if let Some((key, value)) = extra {
        after[key] = value;
    }
    let mut event = NewEvent::new(
        tenant,
        actor,
        AccessAction::PersonalLoginsRevoked,
        Some(member),
    );
    if let Some((project, _, _)) = revoked.first()
        && revoked.iter().all(|(p, _, _)| p == project)
    {
        event = event.project(*project);
    }
    if let [(_, database, _)] = revoked {
        event = event.database(*database);
    }
    event.after(after).write(tx).await
}

/// In the caller's transaction (P3, MH-9): every live login of `member` whose
/// `chelabase_personal_login_qualifies` is false after the caller's write is revoked
/// (`access_lost`): `revoked_at`, the reason, no verifier, and no verifier on its proxy rows.
/// One `personal_logins_revoked` event (by `actor`), written only when the count is above 0
/// (S18). Returns how many. Every writer of `project_grants`, `database_grant_overrides`,
/// `members.status` or `members.tenant_admin` calls it (or [`revoke_logins`]) before its
/// commit, after its own event.
pub(crate) async fn revoke_unqualified_logins(
    tx: &mut Tx,
    actor: Option<Uuid>,
    member: Uuid,
) -> Result<usize, sqlx::Error> {
    let revoked = revoke_where(tx, member, RevokeReason::AccessLost, true).await?;
    if !revoked.is_empty() {
        write_revoked_event(tx, actor, member, RevokeReason::AccessLost, &revoked, None).await?;
    }
    Ok(revoked.len())
}

/// EOS-6, T3-M3 (MH-9): in the caller's transaction, after its write ended the life of
/// `databases` (or of their project), every live login on them is revoked (`access_lost`, one
/// event per member, by no actor): `chelabase_personal_login_qualifies` is false for a
/// database or project that is deleting or deleted. Each member's other logins are judged by
/// the predicate too, so only what no longer qualifies goes. Takes no member lock (the caller
/// holds project and database rows a member writer may wait for); the records are locked
/// `FOR UPDATE` as every revocation does. A create racing this one locks the database row
/// `FOR SHARE` first, so it waits for the caller's commit and sees the new status. Returns how
/// many were revoked.
pub(crate) async fn revoke_database_logins(
    tx: &mut Tx,
    databases: &[Uuid],
) -> Result<usize, sqlx::Error> {
    if databases.is_empty() {
        return Ok(0);
    }
    let members: Vec<Uuid> = sqlx::query_scalar(
        "SELECT DISTINCT member_id FROM personal_logins
         WHERE database_id = ANY($1) AND revoked_at IS NULL ORDER BY member_id",
    )
    .bind(databases)
    .fetch_all(&mut **tx)
    .await?;
    let mut revoked = 0;
    for member in members {
        revoked += revoke_unqualified_logins(tx, None, member).await?;
    }
    Ok(revoked)
}

/// [`revoke_unqualified_logins`] for every live login of `member`, whatever the predicate says
/// (removal, suspension, offboarding), with `reason`. No event for a count of 0.
pub(crate) async fn revoke_logins(
    tx: &mut Tx,
    actor: Option<Uuid>,
    member: Uuid,
    reason: RevokeReason,
) -> Result<usize, sqlx::Error> {
    let revoked = revoke_where(tx, member, reason, false).await?;
    if !revoked.is_empty() {
        write_revoked_event(tx, actor, member, reason, &revoked, None).await?;
    }
    Ok(revoked.len())
}

/// S2 / P16 (b): marks `member`'s `cb_m_*` rows for rotation: `password_enc = ''` never opens,
/// so the next reconcile of each branch gives the role a fresh password and ends its sessions.
/// The rows stay (R31's `last_acted_role` with them). Returns the branches, by id.
async fn mark_member_roles_for_rotation(
    tx: &mut Tx,
    member: Uuid,
) -> Result<Vec<Uuid>, sqlx::Error> {
    sqlx::query_scalar(
        "WITH marked AS (
             UPDATE member_branch_roles SET password_enc = '' WHERE member_id = $1
             RETURNING branch_id)
         SELECT branch_id FROM marked ORDER BY branch_id",
    )
    .bind(member)
    .fetch_all(&mut **tx)
    .await
}

impl MetadataClient {
    /// Creates `member`'s own login on `database` (MH-15, D3, P6). Under the member's lock: a
    /// member that doesn't qualify (`chelabase_personal_login_qualifies`: active, Developer or
    /// above on that database of its tenant, which isn't deleted) is `NotQualified`; a live
    /// record (not revoked, not expired) is `Exists`, whatever the name; a name held by another
    /// login of the database (any status) or by any `neon_roles` row of a branch endpoint of
    /// the database is `NameTaken`, except the rows of the member's own record there, which is
    /// replaced (S10). A revoked or expired record is replaced by DELETE (its proxy rows
    /// cascade) then INSERT, never an UPDATE of its name. Event `personal_login_created`
    /// (actor = target = the member; `after: {role_name, valid_until, level}`). A refusal
    /// writes nothing.
    pub async fn create_personal_login(
        &self,
        member: Uuid,
        database: Uuid,
        role_name: &str,
        scram_verifier: &str,
        valid_until: DateTime<Utc>,
    ) -> Result<PersonalLogin, LoginWriteError> {
        let mut tx = self.pool.begin().await?;
        let Some(m) = lock_member(&mut tx, member).await? else {
            return Err(LoginWriteError::NotQualified);
        };
        // EOS-6: a database (or project) delete in flight is waited for, and its status read
        // after it (`revoke_database_logins` can't see a record inserted under it).
        sqlx::query("SELECT 1 FROM databases WHERE id = $1 FOR SHARE")
            .bind(database)
            .execute(&mut *tx)
            .await?;
        let (qualifies, level): (bool, Option<String>) = sqlx::query_as(
            "SELECT chelabase_personal_login_qualifies($1, $2), chelabase_member_level($1, $2)",
        )
        .bind(member)
        .bind(database)
        .fetch_one(&mut *tx)
        .await?;
        if !qualifies {
            return Err(LoginWriteError::NotQualified);
        }
        let live: Option<bool> = sqlx::query_scalar(
            "SELECT revoked_at IS NULL AND valid_until > now() FROM personal_logins
             WHERE member_id = $1 AND database_id = $2 FOR UPDATE",
        )
        .bind(member)
        .bind(database)
        .fetch_optional(&mut *tx)
        .await?;
        if live == Some(true) {
            return Err(LoginWriteError::Exists);
        }
        // D3: exact lookups, `personal_logins (database_id, role_name)` and each branch
        // endpoint's `neon_roles (endpoint_id, name)`.
        let taken: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM personal_logins
                            WHERE database_id = $1 AND role_name = $2 AND member_id <> $3)
                 OR EXISTS (SELECT 1 FROM branches b
                            JOIN neon_roles r ON r.endpoint_id = b.endpoint_id AND r.name = $2
                            WHERE b.database_id = $1 AND b.status <> 'deleted'
                              AND NOT (r.origin = 'personal' AND r.member_id = $3
                                       AND r.personal_database_id = $1))",
        )
        .bind(database)
        .bind(role_name)
        .bind(member)
        .fetch_one(&mut *tx)
        .await?;
        if taken {
            return Err(LoginWriteError::NameTaken);
        }
        if live.is_some() {
            sqlx::query("DELETE FROM personal_logins WHERE member_id = $1 AND database_id = $2")
                .bind(member)
                .bind(database)
                .execute(&mut *tx)
                .await?;
        }
        sqlx::query(
            "INSERT INTO personal_logins (member_id, database_id, role_name, scram_verifier, valid_until)
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(member)
        .bind(database)
        .bind(role_name)
        .bind(scram_verifier)
        .bind(valid_until)
        .execute(&mut *tx)
        .await
        .map_err(|e| match &e {
            // Another member's create of the same name won the race.
            sqlx::Error::Database(db) if db.is_unique_violation() => LoginWriteError::NameTaken,
            _ => LoginWriteError::Db(e),
        })?;
        let login = login_tx(&mut tx, member, database).await?;
        NewEvent::new(
            m.tenant_id.as_uuid(),
            Some(member),
            AccessAction::PersonalLoginCreated,
            Some(member),
        )
        .project(login.project_id)
        .database(database)
        .after(json!({
            "role_name": login.role_name,
            "valid_until": login.valid_until,
            "level": level,
        }))
        .write(&mut tx)
        .await?;
        tx.commit().await?;
        Ok(login)
    }

    /// Resets `member`'s own login on `database` (P8): the record's verifier and `valid_until`
    /// are replaced, and every personal `neon_roles` row of it gets the new verifier, in one
    /// transaction, so the old password is refused at once. An expired (not revoked) login is
    /// renewed this way. `NotFound` without a record that isn't revoked; `NotQualified` for a
    /// member that no longer qualifies. Returns the login and where its rows are. Event
    /// `personal_login_reset` (`after: {role_name, valid_until}`).
    pub async fn reset_personal_login(
        &self,
        member: Uuid,
        database: Uuid,
        scram_verifier: &str,
        valid_until: DateTime<Utc>,
    ) -> Result<(PersonalLogin, Vec<LoginEndpoint>), LoginWriteError> {
        let mut tx = self.pool.begin().await?;
        let Some(m) = lock_member(&mut tx, member).await? else {
            return Err(LoginWriteError::NotFound);
        };
        let name: Option<String> = sqlx::query_scalar(
            "SELECT role_name FROM personal_logins
             WHERE member_id = $1 AND database_id = $2 AND revoked_at IS NULL FOR UPDATE",
        )
        .bind(member)
        .bind(database)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(name) = name else {
            return Err(LoginWriteError::NotFound);
        };
        let qualifies: bool =
            sqlx::query_scalar("SELECT chelabase_personal_login_qualifies($1, $2)")
                .bind(member)
                .bind(database)
                .fetch_one(&mut *tx)
                .await?;
        if !qualifies {
            return Err(LoginWriteError::NotQualified);
        }
        sqlx::query(
            "UPDATE personal_logins SET scram_verifier = $3, valid_until = $4, updated_at = now()
             WHERE member_id = $1 AND database_id = $2",
        )
        .bind(member)
        .bind(database)
        .bind(scram_verifier)
        .bind(valid_until)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE neon_roles SET scram_verifier = $4
             WHERE member_id = $1 AND personal_database_id = $2 AND name = $3 AND origin = 'personal'",
        )
        .bind(member)
        .bind(database)
        .bind(&name)
        .bind(scram_verifier)
        .execute(&mut *tx)
        .await?;
        let login = login_tx(&mut tx, member, database).await?;
        let endpoints = endpoints_tx(&mut tx, member, Some(database)).await?;
        NewEvent::new(
            m.tenant_id.as_uuid(),
            Some(member),
            AccessAction::PersonalLoginReset,
            Some(member),
        )
        .project(login.project_id)
        .database(database)
        .after(json!({ "role_name": login.role_name, "valid_until": login.valid_until }))
        .write(&mut tx)
        .await?;
        tx.commit().await?;
        Ok((login, endpoints))
    }

    /// Deletes `member`'s record on `database`, any status (its proxy rows cascade), and
    /// returns where those rows were (the proxy cache must be told). `NotFound` without one.
    /// The caller checks that `actor` may. Event `personal_login_deleted` (`before:
    /// {role_name}`).
    pub async fn delete_personal_login(
        &self,
        actor: Uuid,
        member: Uuid,
        database: Uuid,
    ) -> Result<Vec<LoginEndpoint>, LoginWriteError> {
        let mut tx = self.pool.begin().await?;
        let Some(m) = lock_member(&mut tx, member).await? else {
            return Err(LoginWriteError::NotFound);
        };
        // T8-8: the record first (`FOR UPDATE`, the lock order of every record writer). A
        // worker write holding it `FOR SHARE` (`upsert_personal_role_row`) finishes before this
        // returns, so the read below (a new snapshot) lists the row it landed, and one that
        // starts later finds no record and writes nothing.
        sqlx::query("SELECT 1 FROM personal_logins WHERE member_id = $1 AND database_id = $2 FOR UPDATE")
            .bind(member)
            .bind(database)
            .execute(&mut *tx)
            .await?;
        let endpoints = endpoints_tx(&mut tx, member, Some(database)).await?;
        let deleted: Option<(String, Uuid)> = sqlx::query_as(
            "DELETE FROM personal_logins l USING databases d
             WHERE l.member_id = $1 AND l.database_id = $2 AND d.id = l.database_id
             RETURNING l.role_name, d.project_id",
        )
        .bind(member)
        .bind(database)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((role_name, project)) = deleted else {
            return Err(LoginWriteError::NotFound);
        };
        NewEvent::new(
            m.tenant_id.as_uuid(),
            Some(actor),
            AccessAction::PersonalLoginDeleted,
            Some(member),
        )
        .project(project)
        .database(database)
        .before(Some(json!({ "role_name": role_name })))
        .write(&mut tx)
        .await?;
        tx.commit().await?;
        Ok(endpoints)
    }

    /// The member's logins, any status, by database name.
    pub async fn member_personal_logins(
        &self,
        member: Uuid,
    ) -> Result<Vec<PersonalLogin>, MetadataError> {
        let rows = sqlx::query(concat!(
            "SELECT ",
            login_columns!(),
            " FROM personal_logins l JOIN databases d ON d.id = l.database_id
              WHERE l.member_id = $1 ORDER BY d.name, l.database_id"
        ))
        .bind(member)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(login_from_row).collect::<Result<_, _>>()?)
    }

    /// The database's logins, any status, by role name (P24).
    pub async fn database_personal_logins(
        &self,
        database: Uuid,
    ) -> Result<Vec<PersonalLogin>, MetadataError> {
        let rows = sqlx::query(concat!(
            "SELECT ",
            login_columns!(),
            " FROM personal_logins l JOIN databases d ON d.id = l.database_id
              WHERE l.database_id = $1 ORDER BY l.role_name"
        ))
        .bind(database)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(login_from_row).collect::<Result<_, _>>()?)
    }

    /// The worker's read of the database's records, by role name, each with
    /// `chelabase_personal_login_qualifies` from the same statement. A revoked record has no
    /// verifier.
    pub async fn personal_login_records(
        &self,
        database: Uuid,
    ) -> Result<Vec<PersonalLoginRecord>, MetadataError> {
        let rows = sqlx::query(
            "SELECT member_id, database_id, role_name, scram_verifier, valid_until,
                    revoked_at IS NOT NULL AS revoked,
                    chelabase_personal_login_qualifies(member_id, database_id) AS qualifies
             FROM personal_logins WHERE database_id = $1 ORDER BY role_name",
        )
        .bind(database)
        .fetch_all(&self.pool)
        .await?;
        let record = |r: &PgRow| -> Result<PersonalLoginRecord, sqlx::Error> {
            Ok(PersonalLoginRecord {
                member_id: r.try_get("member_id")?,
                database_id: r.try_get("database_id")?,
                role_name: r.try_get("role_name")?,
                scram_verifier: r.try_get("scram_verifier")?,
                valid_until: r.try_get("valid_until")?,
                revoked: r.try_get("revoked")?,
                qualifies: r.try_get("qualifies")?,
            })
        };
        Ok(rows.iter().map(record).collect::<Result<_, _>>()?)
    }

    /// Whether `name` is a personal login of `database` (any status).
    pub async fn is_personal_login_name(
        &self,
        database: Uuid,
        name: &str,
    ) -> Result<bool, MetadataError> {
        Ok(sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM personal_logins WHERE database_id = $1 AND role_name = $2)",
        )
        .bind(database)
        .bind(name)
        .fetch_one(&self.pool)
        .await?)
    }

    /// S12: every personal-login name (any status) of every database of `project`.
    pub async fn personal_login_names(
        &self,
        project: Uuid,
    ) -> Result<BTreeSet<String>, MetadataError> {
        let names: Vec<String> = sqlx::query_scalar(
            "SELECT l.role_name FROM personal_logins l JOIN databases d ON d.id = l.database_id
             WHERE d.project_id = $1",
        )
        .bind(project)
        .fetch_all(&self.pool)
        .await?;
        Ok(names.into_iter().collect())
    }

    /// Every personal `neon_roles` row of the member, whatever its record's status (a revoked
    /// record's rows stay until the worker drops the role, and the cache must still be told).
    pub async fn member_login_endpoints(
        &self,
        member: Uuid,
    ) -> Result<Vec<LoginEndpoint>, MetadataError> {
        let mut tx = self.pool.begin().await?;
        let endpoints = endpoints_tx(&mut tx, member, None).await?;
        tx.commit().await?;
        Ok(endpoints)
    }

    /// The worker's write: the personal row of the record `(member, database, role_name)` on
    /// `endpoint_id`, inserted or given the verifier. The verifier stored is the record's own,
    /// read `FOR SHARE` in the same statement before the insert (I1): a reset or revocation in
    /// flight is waited for and its outcome written (none for a revoked record, MH-9: never
    /// written back), and one that starts later waits for this write, then sees its row. The
    /// caller's `_scram_verifier` (the one it read, which can only be older) is not used. No
    /// record of that name: nothing is written. A row of that name with another origin or
    /// record is left alone, and so is an endpoint that isn't a branch of the record's own
    /// database (T3-M2).
    ///
    /// Lock order: the record (`FOR SHARE`), then the `neon_roles` row. Every writer of a
    /// record takes the member lock, then the record (`FOR UPDATE`), then its rows; this
    /// takes no member lock and waits for nothing it could hold, so it can't deadlock them.
    pub async fn upsert_personal_role_row(
        &self,
        endpoint_id: &str,
        member: Uuid,
        database: Uuid,
        role_name: &str,
        _scram_verifier: &str,
    ) -> Result<(), MetadataError> {
        sqlx::query(
            "WITH l AS (
                 SELECT member_id, database_id, role_name, scram_verifier FROM personal_logins
                 WHERE member_id = $2 AND database_id = $3 AND role_name = $4
                 FOR SHARE)
             INSERT INTO neon_roles AS r
                 (endpoint_id, name, scram_verifier, origin, login, level, member_id,
                  personal_database_id)
             SELECT $1, l.role_name, l.scram_verifier, 'personal', true, 'plain', l.member_id,
                    l.database_id
             FROM l
             WHERE EXISTS (SELECT 1 FROM branches b
                           WHERE b.endpoint_id = $1 AND b.database_id = l.database_id)
             ON CONFLICT (endpoint_id, name) DO UPDATE SET scram_verifier = EXCLUDED.scram_verifier
             WHERE r.origin = 'personal' AND r.member_id = EXCLUDED.member_id
               AND r.personal_database_id = EXCLUDED.personal_database_id",
        )
        .bind(endpoint_id)
        .bind(member)
        .bind(database)
        .bind(role_name)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Deletes the row `role_name` of `endpoint_id` only when its origin is `personal`.
    pub async fn delete_personal_role_row(
        &self,
        endpoint_id: &str,
        role_name: &str,
    ) -> Result<(), MetadataError> {
        sqlx::query(
            "DELETE FROM neon_roles WHERE endpoint_id = $1 AND name = $2 AND origin = 'personal'",
        )
        .bind(endpoint_id)
        .bind(role_name)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Suspends an active member (D5): `suspended`, every live login revoked (`suspended`) and
    /// its member roles marked for rotation (S2), in one transaction; event `member_suspended`
    /// (`after.rotated_branches`), then `personal_logins_revoked` when any was live. Grants are
    /// kept. Returns the touched projects (the `remove_member` union plus the projects of the
    /// member's personal logins). Already suspended: `Ok` with the touched projects and no
    /// write (S7: a retry after a failed identity ban). The caller themself, or an invited,
    /// pending or removed member, is `Conflict`; the last active tenant admin is
    /// `LastTenantAdmin` (P15); a missing member `NotFound`.
    pub async fn suspend_member(
        &self,
        actor: Uuid,
        member: Uuid,
    ) -> Result<Vec<Uuid>, AccessWriteError> {
        let tenant = member_tenant(&self.pool, member)
            .await?
            .ok_or(AccessWriteError::NotFound)?;
        let mut tx = self.pool.begin().await?;
        lock_tenant(&mut tx, tenant).await?;
        let m = lock_member(&mut tx, member)
            .await?
            .ok_or(AccessWriteError::NotFound)?;
        match m.status {
            MemberStatus::Active => {}
            MemberStatus::Suspended => {
                let touched = touched_projects(&mut tx, member).await?;
                tx.commit().await?;
                return Ok(touched);
            }
            MemberStatus::Invited | MemberStatus::Pending | MemberStatus::Removed => {
                return Err(AccessWriteError::Conflict(
                    "only an active member can be suspended".into(),
                ));
            }
        }
        if actor == member {
            return Err(AccessWriteError::Conflict(
                "you can't suspend yourself".into(),
            ));
        }
        if m.tenant_admin && other_active_admins(&mut tx, tenant, member).await? == 0 {
            return Err(AccessWriteError::LastTenantAdmin);
        }
        sqlx::query("UPDATE members SET status = 'suspended', suspended_at = now(), suspended_by = $2,
                    updated_at = now() WHERE id = $1")
            .bind(member)
            .bind(actor)
            .execute(&mut *tx)
            .await?;
        let rotated = mark_member_roles_for_rotation(&mut tx, member).await?;
        NewEvent::new(
            tenant,
            Some(actor),
            AccessAction::MemberSuspended,
            Some(member),
        )
        .before(Some(json!({ "status": "active" })))
        .after(json!({ "status": "suspended", "rotated_branches": rotated }))
        .write(&mut tx)
        .await?;
        revoke_logins(&mut tx, Some(actor), member, RevokeReason::Suspended).await?;
        let touched = touched_projects(&mut tx, member).await?;
        tx.commit().await?;
        Ok(touched)
    }

    /// Reinstates a suspended member: `active` again, with its grants and teams; its logins stay
    /// revoked (the member makes new ones). Event `member_reinstated`. Returns the touched
    /// projects (P16: those of its grants, overrides, member roles, personal logins and teams'
    /// grants), also on the already-active no-op (S7: no write); invited, pending or removed:
    /// `Conflict`; missing: `NotFound`.
    pub async fn reinstate_member(
        &self,
        actor: Uuid,
        member: Uuid,
    ) -> Result<Vec<Uuid>, AccessWriteError> {
        let tenant = member_tenant(&self.pool, member)
            .await?
            .ok_or(AccessWriteError::NotFound)?;
        let mut tx = self.pool.begin().await?;
        lock_tenant(&mut tx, tenant).await?;
        let m = lock_member(&mut tx, member)
            .await?
            .ok_or(AccessWriteError::NotFound)?;
        match m.status {
            MemberStatus::Suspended => {}
            MemberStatus::Active => {
                let touched = touched_projects(&mut tx, member).await?;
                tx.commit().await?;
                return Ok(touched);
            }
            MemberStatus::Invited | MemberStatus::Pending | MemberStatus::Removed => {
                return Err(AccessWriteError::Conflict(
                    "only a suspended member can be reinstated".into(),
                ));
            }
        }
        sqlx::query("UPDATE members SET status = 'active', suspended_at = NULL, suspended_by = NULL,
                    updated_at = now() WHERE id = $1")
            .bind(member)
            .execute(&mut *tx)
            .await?;
        NewEvent::new(
            tenant,
            Some(actor),
            AccessAction::MemberReinstated,
            Some(member),
        )
        .before(Some(json!({ "status": "suspended" })))
        .after(json!({ "status": "active" }))
        .write(&mut tx)
        .await?;
        revoke_unqualified_logins(&mut tx, Some(actor), member).await?;
        let touched = touched_projects(&mut tx, member).await?;
        tx.commit().await?;
        Ok(touched)
    }

    /// "Revoke Postgres access now" (P16 b, S2): every live login of the member revoked
    /// (`revoked_now`) and its member roles marked for rotation, in one transaction; one event
    /// `personal_logins_revoked` (`after: {count, reason, logins, rotated_branches}`), written
    /// even for a count of 0 (an admin's action). Studio access is untouched. Returns the
    /// touched projects; a missing member is `NotFound`.
    pub async fn revoke_postgres_access(
        &self,
        actor: Uuid,
        member: Uuid,
    ) -> Result<Vec<Uuid>, AccessWriteError> {
        let mut tx = self.pool.begin().await?;
        lock_member(&mut tx, member)
            .await?
            .ok_or(AccessWriteError::NotFound)?;
        let revoked = revoke_where(&mut tx, member, RevokeReason::RevokedNow, false).await?;
        let rotated = mark_member_roles_for_rotation(&mut tx, member).await?;
        write_revoked_event(
            &mut tx,
            Some(actor),
            member,
            RevokeReason::RevokedNow,
            &revoked,
            Some(("rotated_branches", json!(rotated))),
        )
        .await?;
        let touched = touched_projects(&mut tx, member).await?;
        tx.commit().await?;
        Ok(touched)
    }

    /// The offboarding write (D4): the member of `tenant` with this identity user
    /// has every live login revoked (`identity_offboarded`; one event when any was live, by no
    /// actor). Returns every personal row of the member, revoked earlier ones included, so a
    /// retried delivery still clears the proxy cache. An unknown user: empty, no write.
    pub async fn revoke_logins_for_identity_user(
        &self,
        tenant: TenantId,
        identity_user_id: &str,
    ) -> Result<Vec<LoginEndpoint>, MetadataError> {
        let mut tx = self.pool.begin().await?;
        let Some(m) = lock_member_by_identity_user(&mut tx, tenant.as_uuid(), identity_user_id).await?
        else {
            return Ok(Vec::new());
        };
        // D7: the mark comes first, so the revocation below (and every later one) sees a
        // member that qualifies for nothing. A retried delivery keeps the first time.
        sqlx::query(
            "UPDATE members SET offboarded_at = coalesce(offboarded_at, now()), updated_at = now()
             WHERE id = $1",
        )
        .bind(m.id)
        .execute(&mut *tx)
        .await?;
        revoke_logins(&mut tx, None, m.id, RevokeReason::IdentityOffboarded).await?;
        let endpoints = endpoints_tx(&mut tx, m.id, None).await?;
        tx.commit().await?;
        Ok(endpoints)
    }

    /// D7: the member of `tenant` with this identity user is no longer marked offboarded (an
    /// accepted invite shows the user back).
    /// `true` when a mark was cleared; an unknown or unmarked member is `false` and nothing is
    /// written. Its logins stay revoked. Under the member's lock, so it orders with the
    /// offboarding write.
    pub async fn clear_offboarded(
        &self,
        tenant: TenantId,
        identity_user_id: &str,
    ) -> Result<bool, MetadataError> {
        let mut tx = self.pool.begin().await?;
        let Some(m) = lock_member_by_identity_user(&mut tx, tenant.as_uuid(), identity_user_id).await?
        else {
            return Ok(false);
        };
        if !m.offboarded {
            return Ok(false);
        }
        sqlx::query("UPDATE members SET offboarded_at = NULL, updated_at = now() WHERE id = $1")
            .bind(m.id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(true)
    }

    /// The roster's levels (MH-35, P13): for each (member, project) of the tenant's members on
    /// projects that aren't deleted, the best level among the member's own grant and its teams'
    /// grants (as the shared shape counts them, P4, P27), and where it comes from; by member
    /// then project.
    pub async fn member_project_roles(
        &self,
        tenant: TenantId,
    ) -> Result<Vec<MemberProjectRole>, MetadataError> {
        let rows: Vec<(Uuid, Uuid, String, bool, bool)> = sqlx::query_as(
            "WITH s AS (
               SELECT g.member_id, g.project_id, g.platform_role, true AS own
               FROM project_grants g
               JOIN members m ON m.id = g.member_id
               JOIN projects p ON p.id = g.project_id
               WHERE m.tenant_id = $1 AND p.status <> 'deleted' AND m.status <> 'removed'
               UNION ALL
               SELECT tm.member_id, tg.project_id, tg.platform_role, false AS own
               FROM team_members tm
               JOIN teams tt ON tt.id = tm.team_id AND tt.tenant_id = $1
               JOIN members m ON m.id = tm.member_id AND m.tenant_id = tt.tenant_id
               JOIN team_grants tg ON tg.team_id = tm.team_id
               JOIN projects p ON p.id = tg.project_id AND p.tenant_id = tt.tenant_id
               WHERE p.status <> 'deleted' AND m.status <> 'removed')
             SELECT member_id, project_id,
                    (ARRAY['viewer','developer','admin'])[max(array_position(
                        ARRAY['viewer','developer','admin'], platform_role))],
                    bool_or(own), bool_or(NOT own)
             FROM s GROUP BY member_id, project_id
             ORDER BY member_id, project_id",
        )
        .bind(tenant.as_uuid())
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|(member, project, role, own, team)| {
                let role = PlatformRole::parse(&role).ok_or_else(|| {
                    sqlx::Error::Decode(format!("invalid platform role {role:?}").into())
                })?;
                let via = match (own, team) {
                    (true, true) => GrantVia::Both,
                    (false, true) => GrantVia::Team,
                    _ => GrantVia::Own,
                };
                Ok(MemberProjectRole { member, project, platform_role: role, via })
            })
            .collect::<Result<_, sqlx::Error>>()
            .map_err(MetadataError::from)
    }

    /// Writes one event as given (role events, `privileges_changed`), in its own transaction.
    /// The caller never puts a secret or SQL in `before`/`after`.
    pub async fn record_access_event(&self, e: &AccessEventInput<'_>) -> Result<(), MetadataError> {
        let mut tx = self.pool.begin().await?;
        NewEvent::from_input(e).write(&mut tx).await?;
        tx.commit().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The step's migration (P19), read at run time so a missing file fails the test, not the
    /// build.
    fn migration() -> String {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/migrations/20261023000001_members_hub.sql"
        );
        std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{path}: {e}"))
    }

    // The stored `revoked_reason` values (the migration's CHECK lists exactly these).
    #[test]
    fn revoke_reasons_are_the_check_values() {
        let all = [
            RevokeReason::AccessLost,
            RevokeReason::Suspended,
            RevokeReason::RevokedNow,
            RevokeReason::IdentityOffboarded,
            RevokeReason::MemberRemoved,
        ];
        let names: Vec<&str> = all.iter().map(|r| r.as_str()).collect();
        assert_eq!(
            names,
            [
                "access_lost",
                "suspended",
                "revoked_now",
                "identity_offboarded",
                "member_removed"
            ]
        );
        let sql = migration();
        for name in names {
            assert!(
                sql.contains(&format!("'{name}'")),
                "{name} missing from the CHECK"
            );
        }
    }

    // P18: the migration deletes the leftover personal rows before the new constraints.
    #[test]
    fn the_migration_drops_leftover_personal_rows_before_its_constraints() {
        let sql = migration().to_ascii_lowercase();
        let delete = sql
            .find("delete from neon_roles where origin = 'personal'")
            .expect("P18's DELETE");
        let check = sql.find("neon_roles_personal_member").expect("the CHECK");
        assert!(delete < check, "the DELETE runs before the CHECK is added");
        assert!(sql.contains("neon_roles_personal_unsealed"));
        assert!(sql.contains("neon_roles_personal_login"));
        assert!(
            sql.contains("'suspended'"),
            "members.status allows suspended"
        );
        assert!(sql.contains("add column branch_id uuid"));
    }

    // D7, P19a (final round A): the offboarded mark comes in its own migration; T3's is left as
    // it was applied.
    #[test]
    fn the_offboarded_mark_has_its_own_migration() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/migrations/20261023000002_members_offboarded.sql"
        );
        let sql = std::fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("{path}: {e}"))
            .to_ascii_lowercase();
        assert!(sql.contains("alter table members"), "{sql}");
        assert!(sql.contains("offboarded_at"), "{sql}");
        assert!(
            !migration().to_ascii_lowercase().contains("offboarded_at"),
            "T3's migration is unchanged"
        );
    }

    // D10 (fix K): who suspended a member and when come in their own migration, ordered after
    // the offboarded mark's and before nothing else.
    #[test]
    fn the_suspended_by_columns_have_their_own_migration_in_order() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/migrations");
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".sql"))
            .collect();
        names.sort();
        let file = "20261023000003_members_suspended_by.sql";
        let at = names
            .iter()
            .position(|n| n == file)
            .unwrap_or_else(|| panic!("{file} is missing from {names:?}"));
        assert_eq!(names[at - 1], "20261023000002_members_offboarded.sql", "{names:?}");
        // Step 2's teams migration (P1) comes after it; it was the newest only in step 1.
        assert!(
            names[at + 1..].iter().any(|n| n == "20261027000001_teams.sql"),
            "the teams migration runs after it: {names:?}"
        );
        let sql = std::fs::read_to_string(format!("{dir}/{file}"))
            .unwrap()
            .to_ascii_lowercase();
        assert!(sql.contains("alter table members"), "{sql}");
        assert!(sql.contains("suspended_at"), "{sql}");
        assert!(sql.contains("suspended_by"), "{sql}");
        assert!(sql.contains("references members(id)") || sql.contains("references members (id)"), "{sql}");
        assert!(sql.contains("check"), "{sql}");
    }
}
