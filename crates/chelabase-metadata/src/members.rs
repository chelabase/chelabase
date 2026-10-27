//! Members of a tenant, their per-project grants and narrowing overrides, invites, the
//! reconciler's per-member Postgres roles, and the append-only access log (members-and-roles
//! design, parts 1 and 5).
//!
//! Lock order, in every transaction here: the tenant row, then member rows, then invite rows.
//! Tenant and member rows are locked `FOR NO KEY UPDATE`, never `FOR UPDATE`: the foreign-key
//! checks of rows that reference them (`access_events`, `project_grants`, ...) take `KEY SHARE`,
//! which `FOR UPDATE` would block. A grant or override write locks only its member (then takes
//! `KEY SHARE` on the tenant for its event), so it serializes with other writes for that member
//! and never waits in a cycle with a remover, which holds the tenant before the member.
//!
//! Invariant: a tenant with any active member has at least one active tenant admin. Every write
//! that could break it (demoting, removing) locks the tenant row and re-checks it; every write
//! that creates a member locks the tenant row too, so the first-admin claim is atomic.
//!
//! Emails (R8): the newest claim of an address wins. Every write that sets a member's email
//! holds the tenant row and first clears it (NULL) on any other live member holding it, so two
//! claims never both fail or both hold it. A claim refresh logs no event.

use chelabase_types::TenantId;
use chelabase_types::access::{Grant, GrantSources, Narrowing, PgRole, PlatformRole, RoleSet};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{Postgres, Row, Transaction};
use thiserror::Error;
use uuid::Uuid;

use crate::personal_logins::{
    AccessEventInput, RevokeReason, revoke_logins, revoke_unqualified_logins,
};
use crate::{MetadataClient, MetadataError};

/// The symbolic database owner. Never stored in a grant's or override's `pg_roles` (R7): the
/// Admin platform role implies the database owner, so writes naming it are `Conflict`.
pub const OWNER_ROLE: &str = "owner";

/// The `Conflict` message for a role set naming [`OWNER_ROLE`].
const OWNER_IS_IMPLIED: &str =
    "the owner role is never granted: the admin platform role implies the database owner";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemberStatus {
    Invited,
    /// Signed up by itself (open sign-up) and waiting for an admin to approve or decline it
    /// (R40). No access of any kind until approved.
    Pending,
    Active,
    Removed,
    /// Suspended by a tenant admin (members hub spec Part 6 "Suspend"): grants are kept, but
    /// the member reaches nothing (403 `member_suspended`) and holds no live personal login
    /// until reinstated. A sign-in never reactivates one; only the operator's bootstrap does.
    Suspended,
}

impl MemberStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Invited => "invited",
            Self::Pending => "pending",
            Self::Active => "active",
            Self::Removed => "removed",
            Self::Suspended => "suspended",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "invited" => Some(Self::Invited),
            "pending" => Some(Self::Pending),
            "active" => Some(Self::Active),
            "removed" => Some(Self::Removed),
            "suspended" => Some(Self::Suspended),
            _ => None,
        }
    }
}

/// What a person with no member row becomes when they first ask in ([`MetadataClient::ensure_member`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    /// An active member without grants.
    Active,
    /// A `pending` member: open sign-up is on and an admin must approve (R39, R40).
    Pending,
    /// No row at all: the tenant takes no sign-ups.
    Closed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MemberRecord {
    pub id: Uuid,
    pub tenant_id: TenantId,
    pub zitadel_user_id: String,
    /// Lowercased and trimmed. `None` when unknown or stale (R8: the newest claim of an email
    /// wins, so a member whose address another member claimed since has none until its own next
    /// request brings its current one). For T5: show such a member without an email. For T7:
    /// matching an invite by email never matches a `None`, and Zitadel stays the truth for who
    /// holds an address.
    pub email: Option<String>,
    pub display_name: String,
    pub tenant_admin: bool,
    pub status: MemberStatus,
    pub accepted_at: Option<DateTime<Utc>>,
    /// D7: the member's Zitadel user was deactivated, locked or removed (the Actions webhook's
    /// or the identity sweep's mark, `members.offboarded_at`); cleared when Zitadel shows the
    /// user active again. Independent of `status`.
    pub offboarded: bool,
    /// D10: when the member was suspended (`members.suspended_at`); `None` unless suspended.
    pub suspended_at: Option<DateTime<Utc>>,
    /// D10: the member who suspended it (`members.suspended_by`); `None` unless suspended, and
    /// for a member suspended before the column existed.
    pub suspended_by: Option<Uuid>,
}

impl MemberRecord {
    /// Active and not offboarded (D7): a member that may use what its grants give.
    pub fn in_good_standing(&self) -> bool {
        self.status == MemberStatus::Active && !self.offboarded
    }
}

/// A member as the member list shows it: the row plus what the list adds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberListing {
    pub member: MemberRecord,
    /// A tenant admin, or holding at least one project grant (an override alone doesn't count).
    pub has_access: bool,
    pub created_at: DateTime<Utc>,
}

impl std::ops::Deref for MemberListing {
    type Target = MemberRecord;

    fn deref(&self) -> &MemberRecord {
        &self.member
    }
}

/// What an open invite tells the person holding its link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InviteDescription {
    /// SHA-256 of the invite's token; `None` for an invite made before tokens existed.
    pub token_hash: Option<Vec<u8>>,
    /// The inviter's display name; empty when they have none.
    pub inviter_name: String,
    pub inviter_email: Option<String>,
    pub tenant_slug: String,
    /// The tenant's chosen name; callers fall back to the slug.
    pub tenant_display_name: Option<String>,
}

impl GrantRow {
    /// True when `pg_roles` names [`OWNER_ROLE`]: refused whatever the platform role (R7).
    pub fn names_owner(&self) -> bool {
        self.pg_roles.iter().any(|r| r == OWNER_ROLE)
    }

    /// A grant of `platform_role` with `pg_roles`, as the API writes it: R17, an Admin grant
    /// holds every role on the project, so it names none (whatever was asked); below Admin the
    /// roles sorted and deduplicated. [`GrantRow::to_grant`] reads an Admin row as every role
    /// whatever it names.
    pub fn new(platform_role: PlatformRole, pg_roles: &[String]) -> Self {
        let pg_roles = match platform_role {
            PlatformRole::Admin => Vec::new(),
            _ => normalize_roles(pg_roles),
        };
        Self { platform_role, pg_roles }
    }

    /// R17: whether the grant holds every role on the project (an Admin grant).
    pub fn every_role(&self) -> bool {
        self.platform_role == PlatformRole::Admin
    }

    /// The access grant it gives: every role for Admin (R17, whatever a row from before that
    /// names), else the named roles (`owner` is never stored, R7).
    pub fn to_grant(&self) -> Grant {
        let roles = if self.every_role() {
            RoleSet::all()
        } else {
            RoleSet::Only(self.pg_roles.iter().map(|r| PgRole::parse(r)).collect())
        };
        Grant { platform_role: self.platform_role, roles }
    }
}

impl OverrideRow {
    /// True when `pg_roles` names [`OWNER_ROLE`]: refused (R7).
    pub fn names_owner(&self) -> bool {
        self.pg_roles.iter().flatten().any(|r| r == OWNER_ROLE)
    }

    /// The narrowing it applies.
    pub fn to_narrowing(&self) -> Narrowing {
        Narrowing {
            platform_role: self.platform_role,
            roles: self.pg_roles.as_ref().map(|roles| roles.iter().map(|r| PgRole::parse(r)).collect()),
        }
    }
}

/// A claim's email, trimmed (SQL lowercases it with `lower(btrim(...))`, as the CHECK does).
/// An empty claim is unknown: `None`, stored NULL.
fn claim_email(email: &str) -> Option<&str> {
    Some(email.trim()).filter(|e| !e.is_empty())
}

/// A member's grant on one project. `pg_roles` never holds [`OWNER_ROLE`]: Admin implies the
/// database owner (R7).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrantRow {
    pub platform_role: PlatformRole,
    pub pg_roles: Vec<String>,
}

/// A narrowing override on a Chelabase database. A `None` field doesn't narrow. At least one
/// field is set.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OverrideRow {
    pub platform_role: Option<PlatformRole>,
    pub pg_roles: Option<Vec<String>>,
}

/// Reconciler state: a member's hidden login role on one branch (R17: one per member and branch).
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct MemberBranchRole {
    pub member_id: Uuid,
    pub branch_id: Uuid,
    pub role_name: String,
    /// Sealed with `Purpose::MemberRole` (AAD = branch + role name).
    pub password_enc: String,
    pub applied_roles: Vec<String>,
}

/// One row of `access_events`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AccessEvent {
    pub id: i64,
    pub tenant_id: TenantId,
    /// `None` only for system actions (the bootstrap, the first-admin claim).
    pub actor_member_id: Option<Uuid>,
    pub action: String,
    pub target_member_id: Option<Uuid>,
    pub project_id: Option<Uuid>,
    pub database_id: Option<Uuid>,
    pub before: Option<Value>,
    pub after: Option<Value>,
    pub at: DateTime<Utc>,
    /// The actor's email now (members are kept when removed); `None` for the system, or a
    /// member whose address went stale.
    pub actor_email: Option<String>,
    pub target_email: Option<String>,
    /// The project's name now, if its row still exists.
    pub project_name: Option<String>,
    /// MH-37: the branch of a per-branch event (role changes); `None` for the others.
    pub branch_id: Option<Uuid>,
}

/// An invite that is neither revoked nor accepted (it may be past `expires_at`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenInvite {
    pub id: Uuid,
    pub member_id: Uuid,
    pub expires_at: DateTime<Utc>,
}

/// A project's grants by member, and the overrides on its databases by (member, database).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProjectAccess {
    pub grants: Vec<(Uuid, GrantRow)>,
    pub overrides: Vec<(Uuid, Uuid, OverrideRow)>,
    /// Every member of every team with a grant on the project: (team, member, grant), by team
    /// then member (step 2).
    pub teams: Vec<(Uuid, Uuid, GrantRow)>,
}

/// The `access_events.action` values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessAction {
    MemberJoined,
    AdminClaimed,
    AdminSet,
    AdminUnset,
    MemberRemoved,
    GrantPut,
    GrantDeleted,
    OverridePut,
    OverrideDeleted,
    InviteCreated,
    InviteResent,
    InviteRevoked,
    InviteAccepted,
    OpenSignupSet,
    TenantRenamed,
    /// An admin approved a pending sign-up, with its first grants (R40).
    MemberApproved,
    /// An admin declined a pending sign-up (R40).
    MemberDeclined,
    /// A person already in the tenant's Zitadel org was invited: added active, no invite code.
    MemberAdded,
    /// A tenant admin suspended an active member (members hub D5).
    MemberSuspended,
    /// A tenant admin reinstated a suspended member.
    MemberReinstated,
    /// Personal logins were revoked: `after: {count, reason, logins}` (and `rotated_branches`
    /// for "Revoke Postgres access now"). Never written for a count of 0, except by revoke-now.
    PersonalLoginsRevoked,
    /// `after: {role_name, valid_until, level}`; never a secret.
    PersonalLoginCreated,
    /// `after: {role_name, valid_until}`; never a secret.
    PersonalLoginReset,
    /// `before: {role_name}`.
    PersonalLoginDeleted,
    /// Written by the role routes through [`MetadataClient::record_access_event`], with
    /// `branch_id`.
    RoleCreated,
    RoleUpdated,
    RoleDeleted,
    /// Written by the privilege editor: `after: {pg_database, acted_as, ops}`, never SQL.
    PrivilegesChanged,
    /// Team events (P9): `after` (and `before` for deletes and removals) is `{team: {id, name},
    /// grants, affected_member_ids}`; created and updated also carry `description`.
    TeamCreated,
    TeamUpdated,
    TeamDeleted,
    TeamGrantPut,
    TeamGrantDeleted,
    /// Targets the member added.
    TeamMemberAdded,
    /// Targets the member removed.
    TeamMemberRemoved,
}

impl AccessAction {
    pub const ALL: [AccessAction; 35] = [
        Self::MemberJoined,
        Self::AdminClaimed,
        Self::AdminSet,
        Self::AdminUnset,
        Self::MemberRemoved,
        Self::GrantPut,
        Self::GrantDeleted,
        Self::OverridePut,
        Self::OverrideDeleted,
        Self::InviteCreated,
        Self::InviteResent,
        Self::InviteRevoked,
        Self::InviteAccepted,
        Self::OpenSignupSet,
        Self::TenantRenamed,
        Self::MemberApproved,
        Self::MemberDeclined,
        Self::MemberAdded,
        Self::MemberSuspended,
        Self::MemberReinstated,
        Self::PersonalLoginsRevoked,
        Self::PersonalLoginCreated,
        Self::PersonalLoginReset,
        Self::PersonalLoginDeleted,
        Self::RoleCreated,
        Self::RoleUpdated,
        Self::RoleDeleted,
        Self::PrivilegesChanged,
        Self::TeamCreated,
        Self::TeamUpdated,
        Self::TeamDeleted,
        Self::TeamGrantPut,
        Self::TeamGrantDeleted,
        Self::TeamMemberAdded,
        Self::TeamMemberRemoved,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::MemberJoined => "member_joined",
            Self::AdminClaimed => "admin_claimed",
            Self::AdminSet => "admin_set",
            Self::AdminUnset => "admin_unset",
            Self::MemberRemoved => "member_removed",
            Self::GrantPut => "grant_put",
            Self::GrantDeleted => "grant_deleted",
            Self::OverridePut => "override_put",
            Self::OverrideDeleted => "override_deleted",
            Self::InviteCreated => "invite_created",
            Self::InviteResent => "invite_resent",
            Self::InviteRevoked => "invite_revoked",
            Self::InviteAccepted => "invite_accepted",
            Self::OpenSignupSet => "open_signup_set",
            Self::TenantRenamed => "tenant_renamed",
            Self::MemberApproved => "member_approved",
            Self::MemberDeclined => "member_declined",
            Self::MemberAdded => "member_added",
            Self::MemberSuspended => "member_suspended",
            Self::MemberReinstated => "member_reinstated",
            Self::PersonalLoginsRevoked => "personal_logins_revoked",
            Self::PersonalLoginCreated => "personal_login_created",
            Self::PersonalLoginReset => "personal_login_reset",
            Self::PersonalLoginDeleted => "personal_login_deleted",
            Self::RoleCreated => "role_created",
            Self::RoleUpdated => "role_updated",
            Self::RoleDeleted => "role_deleted",
            Self::PrivilegesChanged => "privileges_changed",
            Self::TeamCreated => "team_created",
            Self::TeamUpdated => "team_updated",
            Self::TeamDeleted => "team_deleted",
            Self::TeamGrantPut => "team_grant_put",
            Self::TeamGrantDeleted => "team_grant_deleted",
            Self::TeamMemberAdded => "team_member_added",
            Self::TeamMemberRemoved => "team_member_removed",
        }
    }
}

/// Why an access write was refused.
#[derive(Debug, Error)]
pub enum AccessWriteError {
    /// The write would leave the tenant without an active tenant admin.
    #[error("the tenant must keep at least one active tenant admin")]
    LastTenantAdmin,
    #[error("not found")]
    NotFound,
    /// The member (of an approve or decline) is missing, another tenant's or removed.
    #[error("member not found")]
    MemberNotFound,
    #[error("conflict: {0}")]
    Conflict(String),
    /// A checked write found, under the member's lock, a grant or override other than the one
    /// its check read (I4): nothing was written; check again.
    #[error("the member's access changed since it was checked")]
    Changed,
    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),
}

/// What a handler read to check a grant or override write against (I4): the member's grant on
/// one project and their overrides on its databases. A checked write re-reads both under the
/// member's lock and writes only when they are still these; the check's verdict is then about
/// the state the write replaces.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Observed {
    pub project: Uuid,
    pub grant: Option<GrantRow>,
    /// By database.
    pub overrides: std::collections::BTreeMap<Uuid, OverrideRow>,
    /// Team id → that team's grant on `project`, for the member's teams (step 2, P7).
    pub teams: std::collections::BTreeMap<Uuid, GrantRow>,
}

impl Observed {
    /// The sources on `database` (its override, if any), or at project level for `None`.
    pub fn sources(&self, database: Option<Uuid>) -> GrantSources {
        GrantSources::new(
            self.grant.as_ref().map(GrantRow::to_grant),
            self.teams.values().map(GrantRow::to_grant).collect(),
            database
                .and_then(|db| self.overrides.get(&db))
                .map(OverrideRow::to_narrowing),
        )
    }

    /// The state with the own grant replaced (`None`: deleted).
    pub fn with_own(&self, grant: Option<GrantRow>) -> Observed {
        Observed { grant, ..self.clone() }
    }

    /// The state with the override on `database` replaced (`None`: deleted).
    pub fn with_override(&self, database: Uuid, o: Option<OverrideRow>) -> Observed {
        let mut after = self.clone();
        match o {
            Some(o) => after.overrides.insert(database, o),
            None => after.overrides.remove(&database),
        };
        after
    }

    /// The state with `team`'s grant replaced (`None`: the team gives nothing here, or the
    /// member isn't in it).
    pub fn with_team(&self, team: Uuid, grant: Option<GrantRow>) -> Observed {
        let mut after = self.clone();
        match grant {
            Some(g) => after.teams.insert(team, g),
            None => after.teams.remove(&team),
        };
        after
    }
}

/// A team as an invite's check read it (P25): its grants on non-deleted projects, by project id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeamSeen {
    pub team: Uuid,
    pub grants: Vec<(Uuid, GrantRow)>,
}

impl ProjectAccess {
    /// `member`'s part of this project's access, as [`Observed`].
    pub fn observed(&self, project: Uuid, member: Uuid) -> Observed {
        Observed {
            project,
            grant: self.grants.iter().find(|(m, _)| *m == member).map(|(_, g)| g.clone()),
            overrides: self
                .overrides
                .iter()
                .filter(|(m, _, _)| *m == member)
                .map(|(_, d, o)| (*d, o.clone()))
                .collect(),
            teams: self
                .teams
                .iter()
                .filter(|(_, m, _)| *m == member)
                .map(|(t, _, g)| (*t, g.clone()))
                .collect(),
        }
    }
}

/// [`Observed`] as it stands now, read inside the write's transaction. The team grants are
/// read as the shared shape reads them (P4, P27): teams of the member's tenant, on a
/// non-deleted project of that tenant.
pub(crate) async fn observe_tx(tx: &mut Tx, member: Uuid, project: Uuid) -> Result<Observed, sqlx::Error> {
    let mut all = observe_many_tx(tx, &[member], &[project]).await?;
    Ok(all.remove(&(member, project)).unwrap_or(Observed { project, ..Observed::default() }))
}

/// [`observe_tx`] for every pair of `members` × `projects` at once: three set-based reads,
/// whatever the sizes. Every pair has an entry (empty when the member has no access there).
pub(crate) async fn observe_many_tx(
    tx: &mut Tx,
    members: &[Uuid],
    projects: &[Uuid],
) -> Result<std::collections::BTreeMap<(Uuid, Uuid), Observed>, sqlx::Error> {
    let mut out: std::collections::BTreeMap<(Uuid, Uuid), Observed> = members
        .iter()
        .flat_map(|m| projects.iter().map(move |p| ((*m, *p), Observed { project: *p, ..Observed::default() })))
        .collect();
    if out.is_empty() {
        return Ok(out);
    }
    let rows = sqlx::query(
        "SELECT member_id, project_id, platform_role, pg_roles FROM project_grants
         WHERE member_id = ANY($1) AND project_id = ANY($2)",
    )
    .bind(members)
    .bind(projects)
    .fetch_all(&mut **tx)
    .await?;
    for r in &rows {
        let key: (Uuid, Uuid) = (r.try_get("member_id")?, r.try_get("project_id")?);
        if let Some(o) = out.get_mut(&key) {
            o.grant = Some(grant_from_parts(r.try_get("platform_role")?, r.try_get("pg_roles")?)?);
        }
    }
    let rows = sqlx::query(
        "SELECT o.member_id, d.project_id, o.database_id, o.platform_role, o.pg_roles
         FROM database_grant_overrides o JOIN databases d ON d.id = o.database_id
         WHERE o.member_id = ANY($1) AND d.project_id = ANY($2)",
    )
    .bind(members)
    .bind(projects)
    .fetch_all(&mut **tx)
    .await?;
    for r in &rows {
        let key: (Uuid, Uuid) = (r.try_get("member_id")?, r.try_get("project_id")?);
        if let Some(o) = out.get_mut(&key) {
            let role: Option<String> = r.try_get("platform_role")?;
            o.overrides.insert(
                r.try_get("database_id")?,
                override_from_parts(role.as_deref(), r.try_get("pg_roles")?)?,
            );
        }
    }
    let rows = sqlx::query(
        "SELECT tm.member_id, tg.project_id, tg.team_id, tg.platform_role, tg.pg_roles
         FROM team_members tm
         JOIN teams tt ON tt.id = tm.team_id
         JOIN members mm ON mm.id = tm.member_id AND mm.tenant_id = tt.tenant_id
         JOIN team_grants tg ON tg.team_id = tm.team_id AND tg.project_id = ANY($2)
         JOIN projects tp ON tp.id = tg.project_id AND tp.tenant_id = tt.tenant_id
                         AND tp.status <> 'deleted'
         WHERE tm.member_id = ANY($1)",
    )
    .bind(members)
    .bind(projects)
    .fetch_all(&mut **tx)
    .await?;
    for r in &rows {
        let key: (Uuid, Uuid) = (r.try_get("member_id")?, r.try_get("project_id")?);
        if let Some(o) = out.get_mut(&key) {
            o.teams.insert(
                r.try_get("team_id")?,
                grant_from_parts(r.try_get("platform_role")?, r.try_get("pg_roles")?)?,
            );
        }
    }
    Ok(out)
}

/// `Changed` unless the member's access is still what `expect` read.
async fn require_observed(
    tx: &mut Tx,
    member: Uuid,
    expect: &Observed,
) -> Result<(), AccessWriteError> {
    if observe_tx(tx, member, expect.project).await? == *expect {
        Ok(())
    } else {
        Err(AccessWriteError::Changed)
    }
}

/// Everything `member` holds, as [`Observed`] per non-deleted project where they have an own
/// grant, an override or a team's grant (demotion, R17): sorted by project, none empty.
pub(crate) async fn member_observed_tx(tx: &mut Tx, member: Uuid) -> Result<Vec<Observed>, sqlx::Error> {
    let projects: Vec<Uuid> = sqlx::query_scalar(
        "SELECT s.project_id FROM (
             SELECT project_id FROM project_grants WHERE member_id = $1
             UNION
             SELECT d.project_id FROM database_grant_overrides o
               JOIN databases d ON d.id = o.database_id WHERE o.member_id = $1
             UNION
             SELECT tg.project_id FROM team_members tm
               JOIN team_grants tg ON tg.team_id = tm.team_id WHERE tm.member_id = $1
         ) s JOIN projects p ON p.id = s.project_id AND p.status <> 'deleted'
         ORDER BY 1",
    )
    .bind(member)
    .fetch_all(&mut **tx)
    .await?;
    Ok(observe_many_tx(tx, &[member], &projects)
        .await?
        .into_values()
        .filter(|o| o.grant.is_some() || !o.overrides.is_empty() || !o.teams.is_empty())
        .collect())
}

// --- Row mapping -------------------------------------------------------------------------------

#[derive(Debug, Clone, sqlx::FromRow)]
struct MemberRaw {
    id: Uuid,
    tenant_id: Uuid,
    zitadel_user_id: String,
    email: Option<String>,
    display_name: String,
    tenant_admin: bool,
    status: String,
    accepted_at: Option<DateTime<Utc>>,
}

impl TryFrom<MemberRaw> for MemberRecord {
    type Error = sqlx::Error;

    fn try_from(raw: MemberRaw) -> Result<Self, Self::Error> {
        let status = MemberStatus::parse(&raw.status)
            .ok_or_else(|| decode_error(format!("invalid member status {:?}", raw.status)))?;
        Ok(MemberRecord {
            id: raw.id,
            tenant_id: TenantId(raw.tenant_id),
            zitadel_user_id: raw.zitadel_user_id,
            email: raw.email,
            display_name: raw.display_name,
            tenant_admin: raw.tenant_admin,
            status,
            accepted_at: raw.accepted_at,
            offboarded: false, suspended_at: None, suspended_by: None,
        })
    }
}

/// A `members` row as [`member_columns`] selects it: the base columns plus D7's mark
/// (`offboarded_at IS NOT NULL AS offboarded`).
#[derive(Debug, Clone, sqlx::FromRow)]
struct MemberRow {
    #[sqlx(flatten)]
    base: MemberRaw,
    offboarded: bool,
    suspended_at: Option<DateTime<Utc>>,
    suspended_by: Option<Uuid>,
}

impl TryFrom<MemberRow> for MemberRecord {
    type Error = sqlx::Error;

    fn try_from(row: MemberRow) -> Result<Self, Self::Error> {
        Ok(MemberRecord {
            offboarded: row.offboarded,
            suspended_at: row.suspended_at,
            suspended_by: row.suspended_by,
            ..MemberRecord::try_from(row.base)?
        })
    }
}

#[derive(Debug, Clone, sqlx::FromRow)]
struct EventRaw {
    id: i64,
    tenant_id: Uuid,
    actor_member_id: Option<Uuid>,
    action: String,
    target_member_id: Option<Uuid>,
    project_id: Option<Uuid>,
    database_id: Option<Uuid>,
    before: Option<Value>,
    after: Option<Value>,
    at: DateTime<Utc>,
    actor_email: Option<String>,
    target_email: Option<String>,
    project_name: Option<String>,
}

/// An `access_events` row as the log reads it: [`EventRaw`] plus the branch (MH-37).
#[derive(Debug, Clone, sqlx::FromRow)]
struct EventRow {
    #[sqlx(flatten)]
    raw: EventRaw,
    branch_id: Option<Uuid>,
}

impl From<EventRow> for AccessEvent {
    fn from(row: EventRow) -> Self {
        AccessEvent { branch_id: row.branch_id, ..AccessEvent::from(row.raw) }
    }
}

#[derive(Debug, Clone, sqlx::FromRow)]
struct ListingRaw {
    #[sqlx(flatten)]
    member: MemberRow,
    has_access: bool,
    created_at: DateTime<Utc>,
}

impl From<EventRaw> for AccessEvent {
    fn from(raw: EventRaw) -> Self {
        AccessEvent {
            id: raw.id,
            tenant_id: TenantId(raw.tenant_id),
            actor_member_id: raw.actor_member_id,
            action: raw.action,
            target_member_id: raw.target_member_id,
            project_id: raw.project_id,
            database_id: raw.database_id,
            before: raw.before,
            after: raw.after,
            at: raw.at,
            actor_email: raw.actor_email,
            target_email: raw.target_email,
            project_name: raw.project_name,
            branch_id: None,
        }
    }
}

impl TryFrom<ListingRaw> for MemberListing {
    type Error = sqlx::Error;

    fn try_from(raw: ListingRaw) -> Result<Self, Self::Error> {
        Ok(MemberListing {
            member: MemberRecord::try_from(raw.member)?,
            has_access: raw.has_access,
            created_at: raw.created_at,
        })
    }
}

fn decode_error(message: String) -> sqlx::Error {
    sqlx::Error::Decode(message.into())
}

fn parse_role(value: &str) -> Result<PlatformRole, sqlx::Error> {
    PlatformRole::parse(value)
        .ok_or_else(|| decode_error(format!("invalid platform role {value:?}")))
}

/// A `project_grants` row's role and roles.
fn grant_from_parts(platform_role: &str, pg_roles: Vec<String>) -> Result<GrantRow, sqlx::Error> {
    Ok(GrantRow {
        platform_role: parse_role(platform_role)?,
        pg_roles,
    })
}

/// A `database_grant_overrides` row's role and roles.
fn override_from_parts(
    platform_role: Option<&str>,
    pg_roles: Option<Vec<String>>,
) -> Result<OverrideRow, sqlx::Error> {
    Ok(OverrideRow {
        platform_role: platform_role.map(parse_role).transpose()?,
        pg_roles,
    })
}

/// The one select list every access read shares (MH-2): the own grant `g`, the override `d`
/// and the team grants `t` (from [`team_grants_lateral!`]). A macro, so the queries stay
/// `&'static str`.
macro_rules! grant_source_columns {
    () => {
        "g.platform_role AS g_role, g.pg_roles AS g_roles,
         d.member_id IS NOT NULL AS d_found, d.platform_role AS d_role, d.pg_roles AS d_roles,
         t.t_grants"
    };
}

/// P4, P27: the member's team grants on one project, as the column `t.t_grants` (a JSON array
/// of `{team_id, platform_role, pg_roles}` by team id, `'[]'` when none). Only teams of the
/// member's own tenant count, and only their grants on a non-deleted project of that tenant, so
/// a crafted cross-tenant `team_members` or `team_grants` row is ignored. Both arguments are
/// SQL expressions given as string literals (the member's id and the project's id).
macro_rules! team_grants_lateral {
    ($member:literal, $project:literal) => {
        concat!(
            " LEFT JOIN LATERAL (
                SELECT coalesce(jsonb_agg(jsonb_build_object('team_id', tg.team_id,
                         'platform_role', tg.platform_role, 'pg_roles', tg.pg_roles)
                         ORDER BY tg.team_id), '[]'::jsonb) AS t_grants
                FROM team_members tm
                JOIN teams tt ON tt.id = tm.team_id
                JOIN members mm ON mm.id = tm.member_id AND mm.tenant_id = tt.tenant_id
                JOIN team_grants tg ON tg.team_id = tm.team_id AND tg.project_id = ",
            $project,
            "
                JOIN projects tp ON tp.id = tg.project_id AND tp.tenant_id = tt.tenant_id
                                AND tp.status <> 'deleted'
                WHERE tm.member_id = ",
            $member,
            ") t ON true "
        )
    };
}

/// One element of [`team_grants_lateral!`]'s array (its `team_id` only orders it).
#[derive(Deserialize)]
struct TeamGrantJson {
    platform_role: String,
    pg_roles: Vec<String>,
}

/// A row of [`grant_source_columns!`] as [`GrantSources`].
fn grant_sources_of(r: &sqlx::postgres::PgRow) -> Result<GrantSources, sqlx::Error> {
    let own = match r.try_get::<Option<String>, _>("g_role")? {
        Some(role) => Some(grant_from_parts(&role, r.try_get("g_roles")?)?.to_grant()),
        None => None,
    };
    let database = if r.try_get::<bool, _>("d_found")? {
        let role: Option<String> = r.try_get("d_role")?;
        Some(override_from_parts(role.as_deref(), r.try_get("d_roles")?)?.to_narrowing())
    } else {
        None
    };
    let teams: Vec<TeamGrantJson> = serde_json::from_value(r.try_get::<Value, _>("t_grants")?)
        .map_err(|e| decode_error(format!("invalid team grants: {e}")))?;
    let teams = teams
        .into_iter()
        .map(|t| Ok(grant_from_parts(&t.platform_role, t.pg_roles)?.to_grant()))
        .collect::<Result<Vec<_>, sqlx::Error>>()?;
    Ok(GrantSources::new(own, teams, database))
}

/// Role sets are stored sorted, without duplicates.
fn normalize_roles(roles: &[String]) -> Vec<String> {
    let mut roles = roles.to_vec();
    roles.sort();
    roles.dedup();
    roles
}

/// `before`/`after` JSON of a grant.
fn grant_json(grant: &GrantRow) -> Value {
    json!({ "platform_role": grant.platform_role.as_str(), "pg_roles": grant.pg_roles })
}

/// `before`/`after` JSON of an override.
fn override_json(o: &OverrideRow) -> Value {
    json!({ "platform_role": o.platform_role.map(|r| r.as_str()), "pg_roles": o.pg_roles })
}

// --- Queries -----------------------------------------------------------------------------------

macro_rules! member_columns {
    () => {
        "id, tenant_id, zitadel_user_id, email, display_name, tenant_admin, status, accepted_at,
         offboarded_at IS NOT NULL AS offboarded, suspended_at, suspended_by"
    };
}

/// The columns [`MemberListing`] adds to [`member_columns`] (selected from `members` alone). A
/// grant on a soft-deleted project is no access, as in `member_projects`; a team grant counts
/// as the shared shape reads it (P4, P27).
macro_rules! listing_columns {
    () => {
        ", created_at, (tenant_admin OR EXISTS (
            SELECT 1 FROM project_grants g JOIN projects p ON p.id = g.project_id
            WHERE g.member_id = members.id AND p.status <> 'deleted') OR EXISTS (
            SELECT 1 FROM team_members tm
            JOIN teams tt ON tt.id = tm.team_id AND tt.tenant_id = members.tenant_id
            JOIN team_grants tg ON tg.team_id = tm.team_id
            JOIN projects tp ON tp.id = tg.project_id AND tp.tenant_id = tt.tenant_id
                            AND tp.status <> 'deleted'
            WHERE tm.member_id = members.id)) AS has_access"
    };
}

/// `RETURNING` columns of a write that builds the [`MemberListing`] itself: the caller sets
/// `has_access` from the grants it wrote.
macro_rules! returning_listing {
    () => {
        concat!(member_columns!(), ", created_at, false AS has_access")
    };
}

pub(crate) type Tx = Transaction<'static, Postgres>;

/// When an invite was revoked or accepted, if it was.
type Stamp = Option<DateTime<Utc>>;

/// An invite's member, expiry, and when it was revoked or accepted.
pub type InviteState = (Uuid, DateTime<Utc>, Option<DateTime<Utc>>, Option<DateTime<Utc>>);

/// An open invite's token hash, inviter name and email, tenant slug and display name.
type InviteDescriptionRow = (Option<Vec<u8>>, String, Option<String>, String, Option<String>);

/// The person [`MetadataClient::create_invited_member`] invites: their Zitadel user, email, the
/// invite's expiry and token hash, and the grants they get once active.
#[derive(Debug, Clone, Copy)]
pub struct NewInvite<'a> {
    pub zitadel_user_id: &'a str,
    pub email: &'a str,
    pub expires_at: DateTime<Utc>,
    pub token_hash: [u8; 32],
    pub grants: &'a [(Uuid, GrantRow)],
    /// The teams the invitee joins, as the invite's check read them (P25).
    pub teams: &'a [TeamSeen],
}

/// One `access_events` row, written in the transaction of the change it records.
pub(crate) struct NewEvent {
    tenant: Uuid,
    actor: Option<Uuid>,
    action: AccessAction,
    target: Option<Uuid>,
    project: Option<Uuid>,
    database: Option<Uuid>,
    branch: Option<Uuid>,
    before: Option<Value>,
    after: Option<Value>,
}

impl NewEvent {
    pub(crate) fn new(
        tenant: Uuid,
        actor: Option<Uuid>,
        action: AccessAction,
        target: Option<Uuid>,
    ) -> Self {
        NewEvent {
            tenant,
            actor,
            action,
            target,
            project: None,
            database: None,
            branch: None,
            before: None,
            after: None,
        }
    }

    /// Any event, as [`MetadataClient::record_access_event`] takes it.
    pub(crate) fn from_input(e: &AccessEventInput<'_>) -> Self {
        NewEvent {
            tenant: e.tenant.as_uuid(),
            actor: e.actor,
            action: e.action,
            target: e.target,
            project: e.project,
            database: e.database,
            branch: e.branch,
            before: e.before.cloned(),
            after: e.after.cloned(),
        }
    }

    pub(crate) fn project(mut self, project: Uuid) -> Self {
        self.project = Some(project);
        self
    }

    /// The project of the only grant, when there is exactly one (a project invite).
    fn single_project(mut self, grants: &[(Uuid, GrantRow)]) -> Self {
        if let [(project, _)] = grants {
            self.project = Some(*project);
        }
        self
    }

    pub(crate) fn database(mut self, database: Uuid) -> Self {
        self.database = Some(database);
        self
    }

    pub(crate) fn before(mut self, before: Option<Value>) -> Self {
        self.before = before;
        self
    }

    pub(crate) fn after(mut self, after: Value) -> Self {
        self.after = Some(after);
        self
    }

    pub(crate) async fn write(self, tx: &mut Tx) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO access_events
                 (tenant_id, actor_member_id, action, target_member_id, project_id, database_id,
                  branch_id, before, after)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
        )
        .bind(self.tenant)
        .bind(self.actor)
        .bind(self.action.as_str())
        .bind(self.target)
        .bind(self.project)
        .bind(self.database)
        .bind(self.branch)
        .bind(self.before)
        .bind(self.after)
        .execute(&mut **tx)
        .await?;
        Ok(())
    }
}

/// A refused constraint becomes `Conflict`; anything else stays a database error.
fn write_error(e: sqlx::Error) -> AccessWriteError {
    if let sqlx::Error::Database(db) = &e
        && (db.is_check_violation() || db.is_unique_violation())
    {
        return AccessWriteError::Conflict(format!(
            "refused by {}",
            db.constraint().unwrap_or("a constraint")
        ));
    }
    AccessWriteError::Db(e)
}

/// Locks the tenant row (see the module docs); false when there is no such tenant.
pub(crate) async fn lock_tenant(tx: &mut Tx, tenant: Uuid) -> Result<bool, sqlx::Error> {
    Ok(
        sqlx::query_scalar::<_, i32>("SELECT 1 FROM tenants WHERE id = $1 FOR NO KEY UPDATE")
            .bind(tenant)
            .fetch_optional(&mut **tx)
            .await?
            .is_some(),
    )
}

pub(crate) async fn lock_member_by_zitadel(
    tx: &mut Tx,
    tenant: Uuid,
    zitadel_user_id: &str,
) -> Result<Option<MemberRecord>, sqlx::Error> {
    sqlx::query_as::<_, MemberRow>(concat!(
        "SELECT ",
        member_columns!(),
        " FROM members WHERE tenant_id = $1 AND zitadel_user_id = $2 FOR NO KEY UPDATE"
    ))
    .bind(tenant)
    .bind(zitadel_user_id)
    .fetch_optional(&mut **tx)
    .await?
    .map(MemberRecord::try_from)
    .transpose()
}

/// Locks a member row for a change to it (`FOR NO KEY UPDATE`).
pub(crate) async fn lock_member(tx: &mut Tx, member: Uuid) -> Result<Option<MemberRecord>, sqlx::Error> {
    sqlx::query_as::<_, MemberRow>(concat!(
        "SELECT ",
        member_columns!(),
        " FROM members WHERE id = $1 FOR NO KEY UPDATE"
    ))
    .bind(member)
    .fetch_optional(&mut **tx)
    .await?
    .map(MemberRecord::try_from)
    .transpose()
}

/// Locks a member row for a write of its grants or overrides (`FOR NO KEY UPDATE`): writes for
/// one member serialize, so each reads the previous one's result as its `before`, and a removal
/// can't slip in between. A missing or removed member is `NotFound`; a `pending` one is
/// `Conflict`: it has no grants or overrides until an admin approves it.
async fn lock_writable_member(tx: &mut Tx, member: Uuid) -> Result<MemberRecord, AccessWriteError> {
    let m = lock_member(tx, member)
        .await?
        .filter(|m| m.status != MemberStatus::Removed)
        .ok_or(AccessWriteError::NotFound)?;
    if m.status == MemberStatus::Pending {
        return Err(AccessWriteError::Conflict(
            "the member's sign-up is waiting for approval: approve it first".into(),
        ));
    }
    Ok(m)
}

pub(crate) async fn member_tenant(pool: &sqlx::PgPool, member: Uuid) -> Result<Option<Uuid>, sqlx::Error> {
    sqlx::query_scalar("SELECT tenant_id FROM members WHERE id = $1")
        .bind(member)
        .fetch_optional(pool)
        .await
}

/// Active tenant admins of `tenant` other than `member`.
pub(crate) async fn other_active_admins(tx: &mut Tx, tenant: Uuid, member: Uuid) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT count(*) FROM members
         WHERE tenant_id = $1 AND id <> $2 AND status = 'active' AND tenant_admin",
    )
    .bind(tenant)
    .bind(member)
    .fetch_one(&mut **tx)
    .await
}

/// True when `project` is an active project of `tenant`.
async fn active_project_in(tx: &mut Tx, tenant: Uuid, project: Uuid) -> Result<bool, sqlx::Error> {
    Ok(sqlx::query_scalar::<_, i32>(
        "SELECT 1 FROM projects WHERE id = $1 AND tenant_id = $2 AND status = 'active'",
    )
    .bind(project)
    .bind(tenant)
    .fetch_optional(&mut **tx)
    .await?
    .is_some())
}

/// The project of `database` when it belongs to `tenant` (any status).
async fn database_project_in(
    tx: &mut Tx,
    tenant: Uuid,
    database: Uuid,
    live_only: bool,
) -> Result<Option<Uuid>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT d.project_id FROM databases d JOIN projects p ON p.id = d.project_id
         WHERE d.id = $1 AND p.tenant_id = $2
           AND (NOT $3 OR d.status NOT IN ('deleting', 'deleted'))",
    )
    .bind(database)
    .bind(tenant)
    .bind(live_only)
    .fetch_optional(&mut **tx)
    .await
}

/// R8: clears `email` (NULL) on every live member of `tenant` other than `keep` that holds it.
/// The caller holds the tenant row. No write when nobody else holds it, or for a `None` claim.
async fn release_email(
    tx: &mut Tx,
    tenant: Uuid,
    keep: Uuid,
    email: Option<&str>,
) -> Result<(), sqlx::Error> {
    if email.is_none() {
        return Ok(());
    }
    sqlx::query(
        "UPDATE members SET email = NULL, updated_at = now()
         WHERE tenant_id = $1 AND id <> $2 AND status <> 'removed' AND email = lower(btrim($3))",
    )
    .bind(tenant)
    .bind(keep)
    .bind(email)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Sets an active member's email and name from its claims, after [`release_email`]. `None`
/// when nothing changed (no write) or the member isn't active. The caller holds the tenant row.
async fn take_claims(
    tx: &mut Tx,
    tenant: Uuid,
    member: Uuid,
    email: Option<&str>,
    name: &str,
) -> Result<Option<MemberRecord>, sqlx::Error> {
    release_email(tx, tenant, member, email).await?;
    sqlx::query_as::<_, MemberRow>(concat!(
        "UPDATE members SET email = lower(btrim($2)), display_name = $3, updated_at = now()
         WHERE id = $1 AND status = 'active'
           AND (email IS DISTINCT FROM lower(btrim($2)) OR display_name <> $3)
         RETURNING ",
        member_columns!()
    ))
    .bind(member)
    .bind(email)
    .bind(name)
    .fetch_optional(&mut **tx)
    .await?
    .map(MemberRecord::try_from)
    .transpose()
}

/// The projects whose branches a member's access change must reconcile, by id: those of its
/// grants, its overrides, its member roles, its personal logins (any status: a revoked
/// login's role may still be on a branch) and its teams' grants (MH-5).
pub(crate) async fn touched_projects(tx: &mut Tx, member: Uuid) -> Result<Vec<Uuid>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT project_id FROM project_grants WHERE member_id = $1
         UNION
         SELECT d.project_id FROM database_grant_overrides o
           JOIN databases d ON d.id = o.database_id WHERE o.member_id = $1
         UNION
         SELECT d.project_id FROM member_branch_roles r
           JOIN branches b ON b.id = r.branch_id
           JOIN databases d ON d.id = b.database_id WHERE r.member_id = $1
         UNION
         SELECT d.project_id FROM personal_logins l
           JOIN databases d ON d.id = l.database_id WHERE l.member_id = $1
         UNION
         SELECT tg.project_id FROM team_members tm
           JOIN team_grants tg ON tg.team_id = tm.team_id WHERE tm.member_id = $1
         ORDER BY 1",
    )
    .bind(member)
    .fetch_all(&mut **tx)
    .await
}

/// Deletes a member's grants and overrides; returns both as event JSON (grants by project,
/// overrides by database and Postgres database).
async fn delete_member_grants(
    tx: &mut Tx,
    member: Uuid,
) -> Result<(Vec<Value>, Vec<Value>), sqlx::Error> {
    let rows = sqlx::query(
        "DELETE FROM project_grants WHERE member_id = $1
         RETURNING project_id, platform_role, pg_roles",
    )
    .bind(member)
    .fetch_all(&mut **tx)
    .await?;
    let mut grants = rows
        .iter()
        .map(|r| {
            let project: Uuid = r.try_get("project_id")?;
            let grant = grant_from_parts(r.try_get("platform_role")?, r.try_get("pg_roles")?)?;
            let mut value = grant_json(&grant);
            value["project_id"] = json!(project);
            Ok((project, value))
        })
        .collect::<Result<Vec<_>, sqlx::Error>>()?;
    grants.sort_by_key(|(project, _)| *project);
    let rows = sqlx::query(
        "DELETE FROM database_grant_overrides WHERE member_id = $1
         RETURNING database_id, platform_role, pg_roles",
    )
    .bind(member)
    .fetch_all(&mut **tx)
    .await?;
    let mut overrides = rows
        .iter()
        .map(|r| {
            let database: Uuid = r.try_get("database_id")?;
            let role: Option<String> = r.try_get("platform_role")?;
            let o = override_from_parts(role.as_deref(), r.try_get("pg_roles")?)?;
            let mut value = override_json(&o);
            value["database_id"] = json!(database);
            Ok((database, value))
        })
        .collect::<Result<Vec<_>, sqlx::Error>>()?;
    overrides.sort_by_key(|(database, _)| *database);
    Ok((
        grants.into_iter().map(|(_, v)| v).collect(),
        overrides.into_iter().map(|(_, v)| v).collect(),
    ))
}

/// Creates or replaces one grant of a member the caller locked; writes nothing (false) when
/// unchanged. A project that isn't active in `tenant` is `NotFound`.
async fn put_grant_tx(
    tx: &mut Tx,
    tenant: Uuid,
    actor: Uuid,
    member: Uuid,
    project: Uuid,
    grant: &GrantRow,
) -> Result<bool, AccessWriteError> {
    let after = GrantRow {
        platform_role: grant.platform_role,
        pg_roles: normalize_roles(&grant.pg_roles),
    };
    if !active_project_in(tx, tenant, project).await? {
        return Err(AccessWriteError::NotFound);
    }
    let before = sqlx::query(
        "SELECT platform_role, pg_roles FROM project_grants
         WHERE member_id = $1 AND project_id = $2 FOR UPDATE",
    )
    .bind(member)
    .bind(project)
    .fetch_optional(&mut **tx)
    .await?
    .map(|r| grant_from_parts(r.try_get("platform_role")?, r.try_get("pg_roles")?))
    .transpose()?;
    if before.as_ref() == Some(&after) {
        return Ok(false);
    }
    sqlx::query(
        "INSERT INTO project_grants (member_id, project_id, platform_role, pg_roles, granted_by)
         VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (member_id, project_id) DO UPDATE
         SET platform_role = EXCLUDED.platform_role, pg_roles = EXCLUDED.pg_roles,
             granted_by = EXCLUDED.granted_by, updated_at = now()",
    )
    .bind(member)
    .bind(project)
    .bind(after.platform_role.as_str())
    .bind(&after.pg_roles)
    .bind(actor)
    .execute(&mut **tx)
    .await
    .map_err(write_error)?;
    NewEvent::new(tenant, Some(actor), AccessAction::GrantPut, Some(member))
        .project(project)
        .before(before.as_ref().map(grant_json))
        .after(grant_json(&after))
        .write(tx)
        .await?;
    Ok(true)
}

/// Inserts a new member's grants (roles sorted and deduplicated); returns them as event JSON.
/// A project that isn't active in `tenant` is `NotFound`; one named twice is `Conflict`.
async fn insert_grants(
    tx: &mut Tx,
    tenant: Uuid,
    member: Uuid,
    actor: Uuid,
    grants: &[(Uuid, GrantRow)],
) -> Result<Vec<Value>, AccessWriteError> {
    let mut grants_json = Vec::with_capacity(grants.len());
    for (project, grant) in grants {
        if !active_project_in(tx, tenant, *project).await? {
            return Err(AccessWriteError::NotFound);
        }
        let roles = normalize_roles(&grant.pg_roles);
        sqlx::query(
            "INSERT INTO project_grants (member_id, project_id, platform_role, pg_roles, granted_by)
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(member)
        .bind(project)
        .bind(grant.platform_role.as_str())
        .bind(&roles)
        .bind(actor)
        .execute(&mut **tx)
        .await
        .map_err(write_error)?;
        let mut value = grant_json(&GrantRow {
            platform_role: grant.platform_role,
            pg_roles: roles,
        });
        value["project_id"] = json!(project);
        grants_json.push(value);
    }
    Ok(grants_json)
}

/// A team's grants on non-deleted projects of `tenant`, by project id (as every read counts
/// them, P27).
pub(crate) async fn team_grants_tx(
    tx: &mut Tx,
    tenant: Uuid,
    team: Uuid,
) -> Result<Vec<(Uuid, GrantRow)>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT tg.project_id, tg.platform_role, tg.pg_roles
         FROM team_grants tg
         JOIN projects p ON p.id = tg.project_id AND p.tenant_id = $1 AND p.status <> 'deleted'
         WHERE tg.team_id = $2
         ORDER BY tg.project_id",
    )
    .bind(tenant)
    .bind(team)
    .fetch_all(&mut **tx)
    .await?;
    rows.iter()
        .map(|r| {
            Ok((
                r.try_get("project_id")?,
                grant_from_parts(r.try_get("platform_role")?, r.try_get("pg_roles")?)?,
            ))
        })
        .collect()
}

/// A team's name when it is a team of `tenant`.
pub(crate) async fn team_name_in(
    tx: &mut Tx,
    tenant: Uuid,
    team: Uuid,
) -> Result<Option<String>, sqlx::Error> {
    sqlx::query_scalar("SELECT name FROM teams WHERE id = $1 AND tenant_id = $2")
        .bind(team)
        .bind(tenant)
        .fetch_optional(&mut **tx)
        .await
}

/// The `grants` of a team event (P9): `[{project_id, platform_role, pg_roles}]`.
pub(crate) fn team_grants_json(grants: &[(Uuid, GrantRow)]) -> Value {
    Value::Array(
        grants
            .iter()
            .map(|(project, g)| {
                let mut value = grant_json(g);
                value["project_id"] = json!(project);
                value
            })
            .collect(),
    )
}

/// A team event's `before`/`after` (P9, P38): the team, the grants the event concerns and the
/// members it reaches (lowercase hyphenated ids, sorted).
pub(crate) fn team_event_json(team: Uuid, name: &str, grants: &[(Uuid, GrantRow)], affected: &[Uuid]) -> Value {
    let mut ids: Vec<String> = affected.iter().map(Uuid::to_string).collect();
    ids.sort();
    ids.dedup();
    json!({
        "team": { "id": team, "name": name },
        "grants": team_grants_json(grants),
        "affected_member_ids": ids,
    })
}

/// The distinct teams of an invite or approval, in the order given.
fn distinct_teams(teams: &[TeamSeen]) -> Vec<&TeamSeen> {
    let mut seen = std::collections::HashSet::new();
    teams.iter().filter(|t| seen.insert(t.team)).collect()
}

/// P25: under the tenant lock, `Changed` unless every team is still a team of `tenant` whose
/// grants on non-deleted projects are the ones the check read.
async fn require_teams_seen(tx: &mut Tx, tenant: Uuid, teams: &[TeamSeen]) -> Result<(), AccessWriteError> {
    for seen in distinct_teams(teams) {
        if team_name_in(tx, tenant, seen.team).await?.is_none() {
            return Err(AccessWriteError::Changed);
        }
        let mut want = seen.grants.clone();
        want.sort_by_key(|(project, _)| *project);
        if team_grants_tx(tx, tenant, seen.team).await? != want {
            return Err(AccessWriteError::Changed);
        }
    }
    Ok(())
}

/// What a write that joins teams knows of the member's access before it (I4).
#[derive(Clone, Copy)]
enum Before<'a> {
    /// A member with no access until this transaction (new, re-invited or pending), so nothing
    /// to compare.
    Fresh,
    /// What the check read, one per project; every project a joined team reaches must be among
    /// them (T1 audit (b)).
    Checked(&'a [Observed]),
}

/// T1 audit (b), I4: `Conflict` unless `expect` names every project the `teams` reach, with
/// nothing written: a project missing there was never checked.
fn require_expect_covers(teams: &[TeamSeen], expect: &[Observed]) -> Result<(), AccessWriteError> {
    let missing = teams
        .iter()
        .flat_map(|t| t.grants.iter().map(|(p, _)| *p))
        .find(|p| !expect.iter().any(|o| o.project == *p));
    match missing {
        Some(project) => Err(AccessWriteError::Conflict(format!(
            "the member's access on project {project}, which a team reaches, wasn't checked"
        ))),
        None => Ok(()),
    }
}

/// P12: adds `member` to each team (checked by [`require_teams_seen`] in this transaction,
/// under the tenant lock); one `team_member_added` (targeting the member) per membership
/// added. Returns the grant projects of the teams joined (the projects whose access changed for
/// `member`), by id, without duplicates. With `Before::Checked`, refuses first unless the check
/// read every project the teams reach ([`require_expect_covers`]).
async fn add_team_memberships(
    tx: &mut Tx,
    tenant: Uuid,
    actor: Uuid,
    member: Uuid,
    teams: &[TeamSeen],
    before: Before<'_>,
) -> Result<Vec<Uuid>, AccessWriteError> {
    if let Before::Checked(expect) = before {
        require_expect_covers(teams, expect)?;
    }
    let mut projects = Vec::new();
    for seen in distinct_teams(teams) {
        let name = team_name_in(tx, tenant, seen.team).await?.ok_or(AccessWriteError::Changed)?;
        let inserted = sqlx::query(
            "INSERT INTO team_members (team_id, member_id, added_by) VALUES ($1, $2, $3)
             ON CONFLICT (team_id, member_id) DO NOTHING",
        )
        .bind(seen.team)
        .bind(member)
        .bind(actor)
        .execute(&mut **tx)
        .await?
        .rows_affected();
        if inserted == 0 {
            continue;
        }
        let mut grants = seen.grants.clone();
        grants.sort_by_key(|(project, _)| *project);
        projects.extend(grants.iter().map(|(project, _)| *project));
        NewEvent::new(tenant, Some(actor), AccessAction::TeamMemberAdded, Some(member))
            .after(team_event_json(seen.team, &name, &grants, &[member]))
            .write(tx)
            .await?;
    }
    projects.sort();
    projects.dedup();
    Ok(projects)
}

/// Whether an invite's grants or teams give any access (the listing's `has_access`).
fn gives_access(grants: &[(Uuid, GrantRow)], teams: &[TeamSeen]) -> bool {
    !grants.is_empty() || teams.iter().any(|t| !t.grants.is_empty())
}

/// P10: deletes every team membership of `member`; returns the teams as `[{id, name}]` by
/// name. The caller holds the tenant row (P26).
async fn delete_team_memberships(tx: &mut Tx, member: Uuid) -> Result<Value, sqlx::Error> {
    let rows: Vec<(Uuid, String)> = sqlx::query_as(
        "SELECT t.id, t.name FROM team_members tm JOIN teams t ON t.id = tm.team_id
         WHERE tm.member_id = $1 ORDER BY lower(t.name), t.id",
    )
    .bind(member)
    .fetch_all(&mut **tx)
    .await?;
    sqlx::query("DELETE FROM team_members WHERE member_id = $1")
        .bind(member)
        .execute(&mut **tx)
        .await?;
    Ok(Value::Array(
        rows.into_iter().map(|(id, name)| json!({ "id": id, "name": name })).collect(),
    ))
}

fn members_from(rows: Vec<MemberRow>) -> Result<Vec<MemberRecord>, sqlx::Error> {
    rows.into_iter().map(MemberRecord::try_from).collect()
}

impl MetadataClient {
    pub async fn member_by_zitadel(
        &self,
        tenant: TenantId,
        zitadel_user_id: &str,
    ) -> Result<Option<MemberRecord>, MetadataError> {
        Ok(sqlx::query_as::<_, MemberRow>(concat!(
            "SELECT ",
            member_columns!(),
            " FROM members WHERE tenant_id = $1 AND zitadel_user_id = $2"
        ))
        .bind(tenant.as_uuid())
        .bind(zitadel_user_id)
        .fetch_optional(&self.pool)
        .await?
        .map(MemberRecord::try_from)
        .transpose()?)
    }

    /// The caller's member row, made on its first request. Runs on every request, so an active
    /// member whose claims didn't change costs one read: no lock, no write. A changed name
    /// alone is refreshed by a plain UPDATE; a changed email takes the tenant row and the
    /// address (R8: the newest claim wins, any other live holder's email becomes NULL). Neither
    /// logs an event. The tenant row is also locked to insert a member (the first active admin
    /// of the tenant claims `tenant_admin`, event `admin_claimed`; anyone else `member_joined`)
    /// or to activate an invited one (`invite_accepted`). A removed or suspended member is
    /// returned as is (S6: no sign-in reactivates a suspended one, with an open invite either).
    /// An empty email claim is unknown: stored NULL.
    ///
    /// `admission` says what a person with no row becomes (R39). Whoever finds the tenant
    /// without an active admin claims it, whatever `admission` is (a pending or absent row
    /// never becomes the first admin); otherwise `Active` joins as a member, `Pending` waits for
    /// approval (event `member_joined`, status `pending`), and `Closed` writes nothing and is
    /// `NotAdmitted`. An existing row is never changed by `admission`, and a pending one stays
    /// pending.
    pub async fn ensure_member(
        &self,
        tenant: TenantId,
        zitadel_user_id: &str,
        email: &str,
        name: &str,
        admission: Admission,
    ) -> Result<MemberRecord, MetadataError> {
        #[derive(sqlx::FromRow)]
        struct Seen {
            #[sqlx(flatten)]
            member: MemberRow,
            email_same: bool,
            name_same: bool,
        }
        let email = claim_email(email);
        let seen = sqlx::query_as::<_, Seen>(concat!(
            "SELECT ",
            member_columns!(),
            ", email IS NOT DISTINCT FROM lower(btrim($3)) AS email_same,
               display_name = $4 AS name_same
             FROM members WHERE tenant_id = $1 AND zitadel_user_id = $2"
        ))
        .bind(tenant.as_uuid())
        .bind(zitadel_user_id)
        .bind(email)
        .bind(name)
        .fetch_optional(&self.pool)
        .await?;
        if let Some(seen) = seen {
            let member = MemberRecord::try_from(seen.member)?;
            match member.status {
                // S6: a suspended member stays suspended, whatever its claims or invites.
                MemberStatus::Removed | MemberStatus::Pending | MemberStatus::Suspended => {
                    return Ok(member);
                }
                MemberStatus::Active if seen.email_same && seen.name_same => return Ok(member),
                MemberStatus::Active if seen.email_same => {
                    let renamed = sqlx::query_as::<_, MemberRow>(concat!(
                        "UPDATE members SET display_name = $2, updated_at = now()
                         WHERE id = $1 AND status = 'active' AND display_name <> $2
                         RETURNING ",
                        member_columns!()
                    ))
                    .bind(member.id)
                    .bind(name)
                    .fetch_optional(&self.pool)
                    .await?
                    .map(MemberRecord::try_from)
                    .transpose()?;
                    return Ok(renamed.unwrap_or(member));
                }
                MemberStatus::Active | MemberStatus::Invited => {}
            }
        }

        let tenant_id = tenant.as_uuid();
        let mut tx = self.pool.begin().await?;
        if !lock_tenant(&mut tx, tenant_id).await? {
            return Err(MetadataError::TenantNotFound(tenant));
        }
        let member = match lock_member_by_zitadel(&mut tx, tenant_id, zitadel_user_id).await? {
            // S6: the row may have been suspended since the read above; the catch-all arm below
            // would otherwise activate it when it has an open invite.
            Some(m)
                if matches!(
                    m.status,
                    MemberStatus::Removed | MemberStatus::Pending | MemberStatus::Suspended
                ) =>
            {
                m
            }
            Some(m) if m.status == MemberStatus::Active => {
                take_claims(&mut tx, tenant_id, m.id, email, name)
                    .await?
                    .unwrap_or(m)
            }
            // I1: an invited member becomes active only while an open invite exists (not
            // revoked, not accepted, not expired). Otherwise a sign-in with a password the
            // person already had (a removed member invited again) would take the invite's
            // place: the member stays invited and gets `no_access_yet`.
            Some(m) => {
                let open: bool = sqlx::query_scalar(
                    "SELECT EXISTS (SELECT 1 FROM invites
                                     WHERE member_id = $1 AND revoked_at IS NULL
                                       AND accepted_at IS NULL AND expires_at > now())",
                )
                .bind(m.id)
                .fetch_one(&mut *tx)
                .await?;
                if !open {
                    tx.commit().await?;
                    return Ok(m);
                }
                release_email(&mut tx, tenant_id, m.id, email).await?;
                let active = sqlx::query_as::<_, MemberRow>(concat!(
                    "UPDATE members SET status = 'active', accepted_at = now(),
                         email = lower(btrim($2)), display_name = $3, updated_at = now()
                     WHERE id = $1 RETURNING ",
                    member_columns!()
                ))
                .bind(m.id)
                .bind(email)
                .bind(name)
                .fetch_one(&mut *tx)
                .await?;
                let active = MemberRecord::try_from(active)?;
                sqlx::query(
                    "UPDATE invites SET accepted_at = now()
                     WHERE member_id = $1 AND accepted_at IS NULL AND revoked_at IS NULL",
                )
                .bind(m.id)
                .execute(&mut *tx)
                .await?;
                NewEvent::new(
                    tenant.as_uuid(),
                    Some(m.id),
                    AccessAction::InviteAccepted,
                    Some(m.id),
                )
                .before(Some(json!({ "status": "invited" })))
                .after(json!({ "status": "active" }))
                .write(&mut tx)
                .await?;
                revoke_unqualified_logins(&mut tx, Some(m.id), m.id).await?;
                active
            }
            None => {
                let claims: bool = sqlx::query_scalar(
                    "SELECT NOT EXISTS (SELECT 1 FROM members
                                         WHERE tenant_id = $1 AND status = 'active' AND tenant_admin)",
                )
                .bind(tenant_id)
                .fetch_one(&mut *tx)
                .await?;
                let status = match (claims, admission) {
                    (true, _) | (false, Admission::Active) => MemberStatus::Active,
                    (false, Admission::Pending) => MemberStatus::Pending,
                    (false, Admission::Closed) => return Err(MetadataError::NotAdmitted(tenant)),
                };
                let id = Uuid::new_v4();
                release_email(&mut tx, tenant_id, id, email).await?;
                let inserted = sqlx::query_as::<_, MemberRow>(concat!(
                    "INSERT INTO members (id, tenant_id, zitadel_user_id, email, display_name,
                                          tenant_admin, status)
                     VALUES ($1, $2, $3, lower(btrim($4)), $5, $6, $7)
                     RETURNING ",
                    member_columns!()
                ))
                .bind(id)
                .bind(tenant.as_uuid())
                .bind(zitadel_user_id)
                .bind(email)
                .bind(name)
                .bind(claims)
                .bind(status.as_str())
                .fetch_one(&mut *tx)
                .await?;
                let inserted = MemberRecord::try_from(inserted)?;
                let (actor, action) = if inserted.tenant_admin {
                    (None, AccessAction::AdminClaimed)
                } else {
                    (Some(inserted.id), AccessAction::MemberJoined)
                };
                NewEvent::new(tenant.as_uuid(), actor, action, Some(inserted.id))
                    .after(json!({ "status": status.as_str(), "tenant_admin": inserted.tenant_admin }))
                    .write(&mut tx)
                    .await?;
                inserted
            }
        };
        tx.commit().await?;
        Ok(member)
    }

    /// Every source of a member's access on `project` and their override on `database`, in one
    /// query (MH-2): the own grant, the member's team grants (P4) and the override. A `None`
    /// database reads no override. Doesn't look at the member's status: callers check that.
    pub async fn grant_sources(
        &self,
        member: Uuid,
        project: Uuid,
        database: Option<Uuid>,
    ) -> Result<GrantSources, MetadataError> {
        let r = sqlx::query(concat!(
            "SELECT ",
            grant_source_columns!(),
            " FROM (VALUES ($1::uuid)) m (id)
             LEFT JOIN project_grants g ON g.member_id = m.id AND g.project_id = $2
             LEFT JOIN database_grant_overrides d ON d.member_id = m.id AND d.database_id = $3",
            team_grants_lateral!("m.id", "$2::uuid")
        ))
        .bind(member)
        .bind(project)
        .bind(database)
        .fetch_one(&self.pool)
        .await?;
        Ok(grant_sources_of(&r)?)
    }

    /// [`Self::grant_sources`] for every member with a grant on `project`, a team grant on it
    /// or an override on `database`, in one query, by member id (the member-role reconciler's
    /// read per branch). Doesn't look at the members' status: callers check that.
    pub async fn database_grant_sources(
        &self,
        project: Uuid,
        database: Uuid,
    ) -> Result<Vec<(Uuid, GrantSources)>, MetadataError> {
        let rows = sqlx::query(concat!(
            "SELECT m.id AS member_id, ",
            grant_source_columns!(),
            " FROM members m
             LEFT JOIN project_grants g ON g.member_id = m.id AND g.project_id = $1
             LEFT JOIN database_grant_overrides d ON d.member_id = m.id AND d.database_id = $2",
            team_grants_lateral!("m.id", "$1::uuid"),
            " WHERE g.member_id IS NOT NULL OR d.member_id IS NOT NULL OR t.t_grants <> '[]'::jsonb
             ORDER BY m.id"
        ))
        .bind(project)
        .bind(database)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .map(|r| Ok((r.try_get("member_id")?, grant_sources_of(r)?)))
            .collect::<Result<Vec<_>, sqlx::Error>>()?)
    }

    /// The branches the member-role sweep visits: every ready `normal` branch of a live
    /// database, members or not (PB R12: the sweep is also the protected-branch guard's
    /// backstop, so a member-less tenant's guard settings converge too).
    pub async fn member_role_branches(&self) -> Result<Vec<Uuid>, MetadataError> {
        Ok(sqlx::query_scalar(
            "SELECT b.id FROM branches b JOIN databases d ON d.id = b.database_id
             WHERE b.status = 'ready' AND b.kind = 'normal'
               AND d.status NOT IN ('deleting', 'deleted')
             ORDER BY b.id",
        )
        .fetch_all(&self.pool)
        .await?)
    }

    /// Forgets every member role recorded on a branch (its catalog was replaced by a reset or
    /// restore): the next reconcile gives each role a fresh password. Returns how many went.
    pub async fn delete_branch_member_roles(&self, branch: Uuid) -> Result<u64, MetadataError> {
        Ok(sqlx::query("DELETE FROM member_branch_roles WHERE branch_id = $1")
            .bind(branch)
            .execute(&self.pool)
            .await?
            .rows_affected())
    }

    /// The member's project grants, on projects that aren't deleted, by project id.
    pub async fn member_projects(
        &self,
        member: Uuid,
    ) -> Result<Vec<(Uuid, GrantRow)>, MetadataError> {
        let rows = sqlx::query(
            "SELECT g.project_id, g.platform_role, g.pg_roles
             FROM project_grants g JOIN projects p ON p.id = g.project_id
             WHERE g.member_id = $1 AND p.status <> 'deleted'
             ORDER BY g.project_id",
        )
        .bind(member)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .map(|r| {
                Ok((
                    r.try_get("project_id")?,
                    grant_from_parts(r.try_get("platform_role")?, r.try_get("pg_roles")?)?,
                ))
            })
            .collect::<Result<Vec<_>, sqlx::Error>>()?)
    }

    /// T2-4 (P1, MH-2): every project of the tenant that isn't deleted where the member has an
    /// access source, with its sources at the project level (no database narrowing), by project
    /// id. The one read `/v1/me`, the project list and realtime's project list take a member's
    /// projects from: an own grant or a team grant (P4) makes a row; an override alone doesn't.
    /// Doesn't look at the member's status.
    pub async fn member_grant_sources(
        &self,
        tenant: TenantId,
        member: Uuid,
    ) -> Result<Vec<(Uuid, GrantSources)>, MetadataError> {
        let rows = sqlx::query(concat!(
            "SELECT p.id AS project_id, ",
            grant_source_columns!(),
            " FROM projects p
             LEFT JOIN project_grants g ON g.project_id = p.id AND g.member_id = $2
             LEFT JOIN database_grant_overrides d ON false",
            team_grants_lateral!("$2::uuid", "p.id"),
            " WHERE p.tenant_id = $1 AND p.status <> 'deleted'
               AND (g.member_id IS NOT NULL OR t.t_grants <> '[]'::jsonb)
             ORDER BY p.id"
        ))
        .bind(tenant.as_uuid())
        .bind(member)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .map(|r| Ok((r.try_get("project_id")?, grant_sources_of(r)?)))
            .collect::<Result<Vec<_>, sqlx::Error>>()?)
    }

    /// The member's overrides on live databases of live projects, as (project, database,
    /// override) by project then database (`/v1/me` lists them).
    pub async fn member_overrides(
        &self,
        member: Uuid,
    ) -> Result<Vec<(Uuid, Uuid, OverrideRow)>, MetadataError> {
        let rows = sqlx::query(
            "SELECT d.project_id, o.database_id, o.platform_role, o.pg_roles
             FROM database_grant_overrides o
             JOIN databases d ON d.id = o.database_id
             JOIN projects p ON p.id = d.project_id
             WHERE o.member_id = $1 AND p.status <> 'deleted'
               AND d.status NOT IN ('deleting', 'deleted')
             ORDER BY d.project_id, o.database_id",
        )
        .bind(member)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .map(|r| {
                let role: Option<String> = r.try_get("platform_role")?;
                Ok((
                    r.try_get("project_id")?,
                    r.try_get("database_id")?,
                    override_from_parts(role.as_deref(), r.try_get("pg_roles")?)?,
                ))
            })
            .collect::<Result<Vec<_>, sqlx::Error>>()?)
    }

    /// The tenant's invited and active members (removed ones are history), by email.
    pub async fn list_members(&self, tenant: TenantId) -> Result<Vec<MemberRecord>, MetadataError> {
        let rows = sqlx::query_as::<_, MemberRow>(concat!(
            "SELECT ",
            member_columns!(),
            " FROM members WHERE tenant_id = $1 AND status <> 'removed' ORDER BY email, id"
        ))
        .bind(tenant.as_uuid())
        .fetch_all(&self.pool)
        .await?;
        Ok(members_from(rows)?)
    }

    /// [`Self::list_members`] with what the member list adds, in one query.
    pub async fn list_member_listings(
        &self,
        tenant: TenantId,
    ) -> Result<Vec<MemberListing>, MetadataError> {
        let rows = sqlx::query_as::<_, ListingRaw>(concat!(
            "SELECT ",
            member_columns!(),
            listing_columns!(),
            " FROM members WHERE tenant_id = $1 AND status <> 'removed' ORDER BY email, id"
        ))
        .bind(tenant.as_uuid())
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(MemberListing::try_from)
            .collect::<Result<Vec<_>, sqlx::Error>>()?)
    }

    /// One member as the list shows it, whatever its status.
    pub async fn member_listing(
        &self,
        member: Uuid,
    ) -> Result<Option<MemberListing>, MetadataError> {
        Ok(sqlx::query_as::<_, ListingRaw>(concat!(
            "SELECT ",
            member_columns!(),
            listing_columns!(),
            " FROM members WHERE id = $1"
        ))
        .bind(member)
        .fetch_optional(&self.pool)
        .await?
        .map(MemberListing::try_from)
        .transpose()?)
    }

    /// What the link of the Zitadel user's open invite shows: `None` when the user has no open
    /// (neither revoked, accepted nor expired) invite of an `invited` member. The caller checks
    /// the token against `token_hash` in constant time.
    pub async fn open_invite_for_user(
        &self,
        zitadel_user_id: &str,
    ) -> Result<Option<InviteDescription>, MetadataError> {
        let row: Option<InviteDescriptionRow> = sqlx::query_as(
            "SELECT i.token_hash, inviter.display_name, inviter.email, t.slug, t.display_name
             FROM invites i
             JOIN members m ON m.id = i.member_id
             JOIN tenants t ON t.id = m.tenant_id
             JOIN members inviter ON inviter.id = i.invited_by
             WHERE i.zitadel_user_id = $1 AND i.revoked_at IS NULL AND i.accepted_at IS NULL
               AND i.expires_at > now() AND m.status = 'invited'
             ORDER BY i.created_at DESC, i.id LIMIT 1",
        )
        .bind(zitadel_user_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(
            |(token_hash, inviter_name, inviter_email, tenant_slug, tenant_display_name)| {
                InviteDescription {
                    token_hash,
                    inviter_name,
                    inviter_email,
                    tenant_slug,
                    tenant_display_name,
                }
            },
        ))
    }

    /// A member by id, whatever its status (callers check its tenant).
    pub async fn member(&self, member: Uuid) -> Result<Option<MemberRecord>, MetadataError> {
        Ok(sqlx::query_as::<_, MemberRow>(concat!(
            "SELECT ",
            member_columns!(),
            " FROM members WHERE id = $1"
        ))
        .bind(member)
        .fetch_optional(&self.pool)
        .await?
        .map(MemberRecord::try_from)
        .transpose()?)
    }

    /// The tenant's invited or active member holding `email` (trimmed and lowercased). A NULL
    /// (stale or unknown, R8) email never matches, nor does an empty one.
    pub async fn member_by_email(
        &self,
        tenant: TenantId,
        email: &str,
    ) -> Result<Option<MemberRecord>, MetadataError> {
        let Some(email) = claim_email(email) else {
            return Ok(None);
        };
        Ok(sqlx::query_as::<_, MemberRow>(concat!(
            "SELECT ",
            member_columns!(),
            " FROM members
              WHERE tenant_id = $1 AND status <> 'removed' AND email = lower(btrim($2))"
        ))
        .bind(tenant.as_uuid())
        .bind(email)
        .fetch_optional(&self.pool)
        .await?
        .map(MemberRecord::try_from)
        .transpose()?)
    }

    /// The tenant's open invites (neither revoked nor accepted, expired ones included), by
    /// member then newest first.
    pub async fn open_invites(&self, tenant: TenantId) -> Result<Vec<OpenInvite>, MetadataError> {
        let rows: Vec<(Uuid, Uuid, DateTime<Utc>)> = sqlx::query_as(
            "SELECT i.id, i.member_id, i.expires_at
             FROM invites i JOIN members m ON m.id = i.member_id
             WHERE m.tenant_id = $1 AND i.revoked_at IS NULL AND i.accepted_at IS NULL
             ORDER BY i.member_id, i.created_at DESC, i.id",
        )
        .bind(tenant.as_uuid())
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(id, member_id, expires_at)| OpenInvite { id, member_id, expires_at })
            .collect())
    }

    /// The project's grants (any member status) by member id, the overrides on its databases
    /// by (member, database), and its team grants by (team, member) for every team member.
    pub async fn project_access(&self, project: Uuid) -> Result<ProjectAccess, MetadataError> {
        let rows = sqlx::query(
            "SELECT member_id, platform_role, pg_roles FROM project_grants
             WHERE project_id = $1 ORDER BY member_id",
        )
        .bind(project)
        .fetch_all(&self.pool)
        .await?;
        let grants = rows
            .iter()
            .map(|r| {
                Ok((
                    r.try_get("member_id")?,
                    grant_from_parts(r.try_get("platform_role")?, r.try_get("pg_roles")?)?,
                ))
            })
            .collect::<Result<Vec<_>, sqlx::Error>>()?;
        let rows = sqlx::query(
            "SELECT o.member_id, o.database_id, o.platform_role, o.pg_roles
             FROM database_grant_overrides o JOIN databases d ON d.id = o.database_id
             WHERE d.project_id = $1 ORDER BY o.member_id, o.database_id",
        )
        .bind(project)
        .fetch_all(&self.pool)
        .await?;
        let overrides = rows
            .iter()
            .map(|r| {
                let role: Option<String> = r.try_get("platform_role")?;
                Ok((
                    r.try_get("member_id")?,
                    r.try_get("database_id")?,
                    override_from_parts(role.as_deref(), r.try_get("pg_roles")?)?,
                ))
            })
            .collect::<Result<Vec<_>, sqlx::Error>>()?;
        // P4, P27: as the shared shape reads team grants (the member's tenant's teams, a
        // non-deleted project of that tenant).
        let rows = sqlx::query(
            "SELECT tg.team_id, tm.member_id, tg.platform_role, tg.pg_roles
             FROM team_grants tg
             JOIN teams tt ON tt.id = tg.team_id
             JOIN projects tp ON tp.id = tg.project_id AND tp.tenant_id = tt.tenant_id
                             AND tp.status <> 'deleted'
             JOIN team_members tm ON tm.team_id = tg.team_id
             JOIN members mm ON mm.id = tm.member_id AND mm.tenant_id = tt.tenant_id
             WHERE tg.project_id = $1
             ORDER BY tg.team_id, tm.member_id",
        )
        .bind(project)
        .fetch_all(&self.pool)
        .await?;
        let teams = rows
            .iter()
            .map(|r| {
                Ok((
                    r.try_get("team_id")?,
                    r.try_get("member_id")?,
                    grant_from_parts(r.try_get("platform_role")?, r.try_get("pg_roles")?)?,
                ))
            })
            .collect::<Result<Vec<_>, sqlx::Error>>()?;
        Ok(ProjectAccess { grants, overrides, teams })
    }

    /// Adds a person who is already in the tenant's Zitadel org, active and with a password
    /// (an invite that needs no code): an `active` member with these grants, one
    /// `member_added` event (with the project when there is exactly one grant). The email takes
    /// the address from any other live holder (R8: Zitadel is the truth for who holds it). Any
    /// member row with that Zitadel user, a removed one included (R29: a removed member comes
    /// back only through an invite code), or a grant naming `owner` (R7), is `Conflict`; a
    /// project that isn't active in the tenant, or a missing tenant, `NotFound`.
    #[allow(clippy::too_many_arguments)] // the pinned signature (P25)
    pub async fn add_existing_user(
        &self,
        actor: Uuid,
        tenant: TenantId,
        zitadel_user_id: &str,
        email: &str,
        name: &str,
        grants: &[(Uuid, GrantRow)],
        teams: &[TeamSeen],
    ) -> Result<MemberListing, AccessWriteError> {
        if grants.iter().any(|(_, g)| g.names_owner()) {
            return Err(AccessWriteError::Conflict(OWNER_IS_IMPLIED.into()));
        }
        let email = claim_email(email);
        let tenant_id = tenant.as_uuid();
        let mut tx = self.pool.begin().await?;
        if !lock_tenant(&mut tx, tenant_id).await? {
            return Err(AccessWriteError::NotFound);
        }
        if lock_member_by_zitadel(&mut tx, tenant_id, zitadel_user_id)
            .await?
            .is_some()
        {
            return Err(AccessWriteError::Conflict(
                "already a member of the tenant, or removed (invite them again)".into(),
            ));
        }
        require_teams_seen(&mut tx, tenant_id, teams).await?;
        let id = Uuid::new_v4();
        release_email(&mut tx, tenant_id, id, email).await?;
        let raw = sqlx::query_as::<_, ListingRaw>(concat!(
            "INSERT INTO members (id, tenant_id, zitadel_user_id, email, display_name,
                                  status, invited_by, accepted_at)
             VALUES ($1, $2, $3, lower(btrim($4)), $5, 'active', $6, now()) RETURNING ",
            returning_listing!()
        ))
        .bind(id)
        .bind(tenant_id)
        .bind(zitadel_user_id)
        .bind(email)
        .bind(name)
        .bind(actor)
        .fetch_one(&mut *tx)
        .await
        .map_err(write_error)?;
        let mut listing = MemberListing::try_from(raw)?;
        listing.has_access = gives_access(grants, teams);
        let member = listing.member.clone();
        let grants_json = insert_grants(&mut tx, tenant_id, member.id, actor, grants).await?;
        NewEvent::new(
            tenant_id,
            Some(actor),
            AccessAction::MemberAdded,
            Some(member.id),
        )
        .single_project(grants)
        .after(json!({ "status": "active", "email": member.email, "grants": grants_json }))
        .write(&mut tx)
        .await?;
        add_team_memberships(&mut tx, tenant_id, actor, member.id, teams, Before::Fresh).await?;
        tx.commit().await?;
        Ok(listing)
    }

    /// Approves a pending member (R40): `active`, with these first grants, in one transaction;
    /// one `member_approved` event (with the project when there is exactly one grant). A member
    /// that isn't pending is `Conflict` (a removed or missing one `MemberNotFound`); a grant naming
    /// `owner` (R7) is `Conflict`; a project that isn't active in the tenant, `NotFound`.
    pub async fn approve_member(
        &self,
        actor: Uuid,
        member: Uuid,
        grants: &[(Uuid, GrantRow)],
        teams: &[TeamSeen],
    ) -> Result<MemberListing, AccessWriteError> {
        if grants.iter().any(|(_, g)| g.names_owner()) {
            return Err(AccessWriteError::Conflict(OWNER_IS_IMPLIED.into()));
        }
        let tenant = member_tenant(&self.pool, member)
            .await?
            .ok_or(AccessWriteError::MemberNotFound)?;
        let mut tx = self.pool.begin().await?;
        lock_tenant(&mut tx, tenant).await?;
        let m = lock_member(&mut tx, member)
            .await?
            .ok_or(AccessWriteError::MemberNotFound)?;
        match m.status {
            MemberStatus::Pending => {}
            MemberStatus::Removed => return Err(AccessWriteError::MemberNotFound),
            _ => return Err(AccessWriteError::Conflict("the member isn't waiting for approval".into())),
        }
        require_teams_seen(&mut tx, tenant, teams).await?;
        let raw = sqlx::query_as::<_, ListingRaw>(concat!(
            "UPDATE members SET status = 'active', accepted_at = now(), updated_at = now()
             WHERE id = $1 RETURNING ",
            returning_listing!()
        ))
        .bind(member)
        .fetch_one(&mut *tx)
        .await?;
        let mut listing = MemberListing::try_from(raw)?;
        listing.has_access = gives_access(grants, teams);
        let grants_json = insert_grants(&mut tx, tenant, member, actor, grants).await?;
        NewEvent::new(tenant, Some(actor), AccessAction::MemberApproved, Some(member))
            .single_project(grants)
            .before(Some(json!({ "status": "pending" })))
            .after(json!({ "status": "active", "grants": grants_json }))
            .write(&mut tx)
            .await?;
        add_team_memberships(&mut tx, tenant, actor, member, teams, Before::Fresh).await?;
        revoke_unqualified_logins(&mut tx, Some(actor), member).await?;
        tx.commit().await?;
        Ok(listing)
    }

    /// Declines a pending member (R40): `removed`; one `member_declined` event. A member that
    /// isn't pending is `Conflict` (a removed or missing one `NotFound`). Returns the Zitadel user
    /// to deactivate.
    pub async fn decline_member(&self, actor: Uuid, member: Uuid) -> Result<String, AccessWriteError> {
        let tenant = member_tenant(&self.pool, member)
            .await?
            .ok_or(AccessWriteError::MemberNotFound)?;
        let mut tx = self.pool.begin().await?;
        lock_tenant(&mut tx, tenant).await?;
        let m = lock_member(&mut tx, member)
            .await?
            .ok_or(AccessWriteError::MemberNotFound)?;
        match m.status {
            MemberStatus::Pending => {}
            MemberStatus::Removed => return Err(AccessWriteError::MemberNotFound),
            _ => return Err(AccessWriteError::Conflict("the member isn't waiting for approval".into())),
        }
        sqlx::query(
            "UPDATE members SET status = 'removed', tenant_admin = false, updated_at = now()
             WHERE id = $1",
        )
        .bind(member)
        .execute(&mut *tx)
        .await?;
        NewEvent::new(tenant, Some(actor), AccessAction::MemberDeclined, Some(member))
            .before(Some(json!({ "status": "pending" })))
            .after(json!({ "status": "removed" }))
            .write(&mut tx)
            .await?;
        revoke_logins(&mut tx, Some(actor), member, RevokeReason::MemberRemoved).await?;
        tx.commit().await?;
        Ok(m.zitadel_user_id)
    }

    pub async fn active_tenant_admins(
        &self,
        tenant: TenantId,
    ) -> Result<Vec<MemberRecord>, MetadataError> {
        let rows = sqlx::query_as::<_, MemberRow>(concat!(
            "SELECT ",
            member_columns!(),
            " FROM members WHERE tenant_id = $1 AND status = 'active' AND tenant_admin
              ORDER BY email, id"
        ))
        .bind(tenant.as_uuid())
        .fetch_all(&self.pool)
        .await?;
        Ok(members_from(rows)?)
    }

    /// Whether the tenant has an invited, pending or active member (the sign-up gate).
    pub async fn tenant_has_members(&self, tenant: TenantId) -> Result<bool, MetadataError> {
        Ok(sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM members WHERE tenant_id = $1 AND status <> 'removed')",
        )
        .bind(tenant.as_uuid())
        .fetch_one(&self.pool)
        .await?)
    }

    /// Sets or clears a member's tenant-admin flag. `actor` is `None` for system changes.
    /// Clearing the last active admin's is `LastTenantAdmin`; promoting a removed member is
    /// `NotFound`, an invited one `Conflict` (R9, checked under the member's lock); an
    /// unchanged flag writes nothing.
    pub async fn set_tenant_admin(
        &self,
        actor: Option<Uuid>,
        member: Uuid,
        admin: bool,
    ) -> Result<(), AccessWriteError> {
        self.set_tenant_admin_checked(actor, member, admin, None).await
    }

    /// Everything the member holds (own grants, overrides, team grants), one [`Observed`] per
    /// non-deleted project they reach, sorted by project: what a demotion is checked against.
    pub async fn member_observed(&self, member: Uuid) -> Result<Vec<Observed>, MetadataError> {
        let mut tx = self.pool.begin().await?;
        let out = member_observed_tx(&mut tx, member).await?;
        tx.commit().await?;
        Ok(out)
    }

    /// [`Self::set_tenant_admin`] after a check (R17, P6, P25): a demotion that would write
    /// compares, under the tenant lock then the member's, everything the member keeps
    /// ([`Self::member_observed`]) with `expect`, what the check read; any difference is
    /// `Changed` with nothing written. `None`, a promotion, or an unchanged flag compare nothing.
    pub async fn set_tenant_admin_checked(
        &self,
        actor: Option<Uuid>,
        member: Uuid,
        admin: bool,
        expect: Option<&[Observed]>,
    ) -> Result<(), AccessWriteError> {
        let tenant = member_tenant(&self.pool, member)
            .await?
            .ok_or(AccessWriteError::NotFound)?;
        let mut tx = self.pool.begin().await?;
        lock_tenant(&mut tx, tenant).await?;
        let m = lock_member(&mut tx, member)
            .await?
            .ok_or(AccessWriteError::NotFound)?;
        if admin && m.status == MemberStatus::Removed {
            return Err(AccessWriteError::NotFound);
        }
        if admin && m.status != MemberStatus::Active {
            return Err(AccessWriteError::Conflict(
                "only an active member may become a tenant admin".into(),
            ));
        }
        if m.tenant_admin == admin {
            return Ok(());
        }
        if !admin
            && m.status == MemberStatus::Active
            && other_active_admins(&mut tx, tenant, member).await? == 0
        {
            return Err(AccessWriteError::LastTenantAdmin);
        }
        if !admin
            && let Some(expect) = expect
            && member_observed_tx(&mut tx, member).await? != expect
        {
            return Err(AccessWriteError::Changed);
        }
        sqlx::query("UPDATE members SET tenant_admin = $2, updated_at = now() WHERE id = $1")
            .bind(member)
            .bind(admin)
            .execute(&mut *tx)
            .await?;
        let action = if admin {
            AccessAction::AdminSet
        } else {
            AccessAction::AdminUnset
        };
        NewEvent::new(tenant, actor, action, Some(member))
            .before(Some(json!({ "tenant_admin": !admin })))
            .after(json!({ "tenant_admin": admin }))
            .write(&mut tx)
            .await?;
        revoke_unqualified_logins(&mut tx, actor, member).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Marks a member removed: its grants, overrides and team memberships are deleted (P10,
    /// under the tenant lock, P26), its open invites revoked and its live personal logins
    /// revoked (`member_removed`, whose `before` lists the teams). Returns the projects whose
    /// branches need a member-role reconcile (its grants', its overrides', its teams' grants',
    /// those it has member roles in and those of its personal logins), by id. Removing the last
    /// active admin is `LastTenantAdmin`; removing a removed member writes nothing.
    pub async fn remove_member(
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
        if m.status == MemberStatus::Removed {
            return Ok(Vec::new());
        }
        if m.status == MemberStatus::Active
            && m.tenant_admin
            && other_active_admins(&mut tx, tenant, member).await? == 0
        {
            return Err(AccessWriteError::LastTenantAdmin);
        }
        let touched = touched_projects(&mut tx, member).await?;
        let (grants, overrides) = delete_member_grants(&mut tx, member).await?;
        let teams = delete_team_memberships(&mut tx, member).await?;
        sqlx::query(
            "UPDATE invites SET revoked_at = now()
             WHERE member_id = $1 AND accepted_at IS NULL AND revoked_at IS NULL",
        )
        .bind(member)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE members SET status = 'removed', tenant_admin = false, updated_at = now()
             WHERE id = $1",
        )
        .bind(member)
        .execute(&mut *tx)
        .await?;
        NewEvent::new(
            tenant,
            Some(actor),
            AccessAction::MemberRemoved,
            Some(member),
        )
        .before(Some(json!({
            "status": m.status.as_str(),
            "tenant_admin": m.tenant_admin,
            "grants": grants,
            "overrides": overrides,
            "teams": teams,
        })))
        .after(json!({ "status": "removed", "tenant_admin": false }))
        .write(&mut tx)
        .await?;
        revoke_logins(&mut tx, Some(actor), member, RevokeReason::MemberRemoved).await?;
        tx.commit().await?;
        Ok(touched)
    }

    /// Creates or replaces a member's grant on a project of its tenant. Naming `owner` is
    /// `Conflict` whatever the platform role (R7); a missing or removed member, or a project that isn't active in the
    /// member's tenant, is `NotFound`. Roles are stored sorted and deduplicated; an unchanged
    /// grant writes nothing. Returns whether it wrote.
    pub async fn put_project_grant(
        &self,
        actor: Uuid,
        member: Uuid,
        project: Uuid,
        grant: &GrantRow,
    ) -> Result<bool, AccessWriteError> {
        Ok(!self
            .put_project_grants(actor, member, &[(project, grant.clone())])
            .await?
            .is_empty())
    }

    /// [`Self::put_project_grant`] for several projects in one transaction: all are written or
    /// none (one `grant_put` event per changed grant). A project named twice is `Conflict`.
    /// Returns the projects whose grant changed.
    pub async fn put_project_grants(
        &self,
        actor: Uuid,
        member: Uuid,
        grants: &[(Uuid, GrantRow)],
    ) -> Result<Vec<Uuid>, AccessWriteError> {
        self.put_project_grants_checked(actor, member, grants, &[], &[]).await
    }

    /// [`Self::put_project_grants`] after a check of `expect` (I4): under the member's lock each
    /// project's [`Observed`] is read again, and any that differs from the one given for it is
    /// `Changed` before anything is written. A project without an entry in `expect` is not
    /// compared.
    pub async fn put_project_grant_checked(
        &self,
        actor: Uuid,
        member: Uuid,
        project: Uuid,
        grant: &GrantRow,
        expect: &Observed,
    ) -> Result<bool, AccessWriteError> {
        Ok(!self
            .put_project_grants_checked(actor, member, &[(project, grant.clone())], &[], std::slice::from_ref(expect))
            .await?
            .is_empty())
    }

    /// [`Self::put_project_grants`] after a check of `expect`, adding the member to `teams`
    /// (P12, P25): with teams it takes the tenant row before the member (P6) and refuses with
    /// `Changed` any team whose grants differ from what the check read. Returns the projects
    /// whose access changed: those of the grants written (in the order given), then those of
    /// the teams joined that aren't among them (by id).
    pub async fn put_project_grants_checked(
        &self,
        actor: Uuid,
        member: Uuid,
        grants: &[(Uuid, GrantRow)],
        teams: &[TeamSeen],
        expect: &[Observed],
    ) -> Result<Vec<Uuid>, AccessWriteError> {
        if grants.iter().any(|(_, g)| g.names_owner()) {
            return Err(AccessWriteError::Conflict(OWNER_IS_IMPLIED.into()));
        }
        let mut seen = std::collections::HashSet::new();
        if !grants.iter().all(|(p, _)| seen.insert(*p)) {
            return Err(AccessWriteError::Conflict(
                "a project is named in more than one grant".into(),
            ));
        }
        // T1 audit (b): every project the teams reach was checked (refused before any lock).
        require_expect_covers(teams, expect)?;
        let mut tx = self.pool.begin().await?;
        // P6, P26: a write that adds team memberships takes the tenant row before the member.
        let locked_tenant = if teams.is_empty() {
            None
        } else {
            let tenant = member_tenant(&self.pool, member)
                .await?
                .ok_or(AccessWriteError::NotFound)?;
            lock_tenant(&mut tx, tenant).await?;
            Some(tenant)
        };
        let m = lock_writable_member(&mut tx, member).await?;
        let tenant = m.tenant_id.as_uuid();
        if locked_tenant.is_some_and(|t| t != tenant) {
            return Err(AccessWriteError::Changed);
        }
        require_teams_seen(&mut tx, tenant, teams).await?;
        for observed in expect {
            require_observed(&mut tx, member, observed).await?;
        }
        let mut changed = Vec::new();
        for (project, grant) in grants {
            if put_grant_tx(&mut tx, tenant, actor, member, *project, grant).await? {
                changed.push(*project);
            }
        }
        // A team joined without grants changes no access, so it reaches no project.
        let joined = add_team_memberships(&mut tx, tenant, actor, member, teams, Before::Checked(expect)).await?;
        if !changed.is_empty() || !joined.is_empty() {
            revoke_unqualified_logins(&mut tx, Some(actor), member).await?;
        }
        for project in joined {
            if !changed.contains(&project) {
                changed.push(project);
            }
        }
        tx.commit().await?;
        Ok(changed)
    }

    pub async fn delete_project_grant(
        &self,
        actor: Uuid,
        member: Uuid,
        project: Uuid,
    ) -> Result<(), AccessWriteError> {
        self.delete_project_grant_checked(actor, member, project, None).await
    }

    /// [`Self::delete_project_grant`] after a check of `expect` (I4, MH-4): `Changed` when,
    /// under the member's lock, their access on the project (own grant, overrides, team
    /// grants) is no longer what the check read. The member's lock is enough (P6): a team write
    /// locks every member it reaches before its own re-read, so it either commits before this
    /// re-read (and this one sees it) or re-reads after this commit (and sees the grant gone).
    pub async fn delete_project_grant_checked(
        &self,
        actor: Uuid,
        member: Uuid,
        project: Uuid,
        expect: Option<&Observed>,
    ) -> Result<(), AccessWriteError> {
        let mut tx = self.pool.begin().await?;
        let m = lock_writable_member(&mut tx, member).await?;
        if let Some(expect) = expect {
            if expect.project != project {
                return Err(AccessWriteError::Changed);
            }
            require_observed(&mut tx, member, expect).await?;
        }
        let before = sqlx::query(
            "DELETE FROM project_grants WHERE member_id = $1 AND project_id = $2
             RETURNING platform_role, pg_roles",
        )
        .bind(member)
        .bind(project)
        .fetch_optional(&mut *tx)
        .await?
        .map(|r| grant_from_parts(r.try_get("platform_role")?, r.try_get("pg_roles")?))
        .transpose()?
        .ok_or(AccessWriteError::NotFound)?;
        NewEvent::new(
            m.tenant_id.as_uuid(),
            Some(actor),
            AccessAction::GrantDeleted,
            Some(member),
        )
        .project(project)
        .before(Some(grant_json(&before)))
        .write(&mut tx)
        .await?;
        revoke_unqualified_logins(&mut tx, Some(actor), member).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Creates or replaces a narrowing override on a Chelabase database of the member's tenant
    /// (R17: no Postgres-database level). An override with neither field, or with roles naming `owner` (R7), is `Conflict`. Roles are
    /// stored sorted and deduplicated; an unchanged override writes nothing. Returns whether it wrote.
    pub async fn put_override(
        &self,
        actor: Uuid,
        member: Uuid,
        database: Uuid,
        o: &OverrideRow,
    ) -> Result<bool, AccessWriteError> {
        self.put_override_checked(actor, member, database, o, None).await
    }

    /// [`Self::put_override`] after a check of `expect` (I4): `Changed` when, under the member's
    /// lock, their access on the database's project is no longer what the check read.
    pub async fn put_override_checked(
        &self,
        actor: Uuid,
        member: Uuid,
        database: Uuid,
        o: &OverrideRow,
        expect: Option<&Observed>,
    ) -> Result<bool, AccessWriteError> {
        if o.platform_role.is_none() && o.pg_roles.is_none() {
            return Err(AccessWriteError::Conflict(
                "an override narrows the platform role, the roles or both".into(),
            ));
        }
        if o.names_owner() {
            return Err(AccessWriteError::Conflict(OWNER_IS_IMPLIED.into()));
        }
        let after = OverrideRow {
            platform_role: o.platform_role,
            pg_roles: o.pg_roles.as_deref().map(normalize_roles),
        };
        let mut tx = self.pool.begin().await?;
        let m = lock_writable_member(&mut tx, member).await?;
        let tenant = m.tenant_id.as_uuid();
        let project = database_project_in(&mut tx, tenant, database, true)
            .await?
            .ok_or(AccessWriteError::NotFound)?;
        if let Some(expect) = expect {
            if expect.project != project {
                return Err(AccessWriteError::Changed);
            }
            require_observed(&mut tx, member, expect).await?;
        }
        let before = sqlx::query(
            "SELECT platform_role, pg_roles FROM database_grant_overrides
             WHERE member_id = $1 AND database_id = $2
             FOR UPDATE",
        )
        .bind(member)
        .bind(database)
        .fetch_optional(&mut *tx)
        .await?
        .map(|r| {
            let role: Option<String> = r.try_get("platform_role")?;
            override_from_parts(role.as_deref(), r.try_get("pg_roles")?)
        })
        .transpose()?;
        if before.as_ref() == Some(&after) {
            return Ok(false);
        }
        let role = after.platform_role.map(|r| r.as_str());
        if before.is_some() {
            sqlx::query(
                "UPDATE database_grant_overrides
                 SET platform_role = $3, pg_roles = $4, set_by = $5, updated_at = now()
                 WHERE member_id = $1 AND database_id = $2",
            )
            .bind(member)
            .bind(database)
            .bind(role)
            .bind(&after.pg_roles)
            .bind(actor)
            .execute(&mut *tx)
            .await
            .map_err(write_error)?;
        } else {
            sqlx::query(
                "INSERT INTO database_grant_overrides
                     (member_id, database_id, platform_role, pg_roles, set_by)
                 VALUES ($1, $2, $3, $4, $5)",
            )
            .bind(member)
            .bind(database)
            .bind(role)
            .bind(&after.pg_roles)
            .bind(actor)
            .execute(&mut *tx)
            .await
            .map_err(write_error)?;
        }
        NewEvent::new(tenant, Some(actor), AccessAction::OverridePut, Some(member))
            .project(project)
            .database(database)
            .before(before.as_ref().map(override_json))
            .after(override_json(&after))
            .write(&mut tx)
            .await?;
        revoke_unqualified_logins(&mut tx, Some(actor), member).await?;
        tx.commit().await?;
        Ok(true)
    }

    pub async fn delete_override(
        &self,
        actor: Uuid,
        member: Uuid,
        database: Uuid,
    ) -> Result<(), AccessWriteError> {
        self.delete_override_checked(actor, member, database, None).await
    }

    /// [`Self::delete_override`] after a check of `expect`, as [`Self::put_override_checked`].
    pub async fn delete_override_checked(
        &self,
        actor: Uuid,
        member: Uuid,
        database: Uuid,
        expect: Option<&Observed>,
    ) -> Result<(), AccessWriteError> {
        let mut tx = self.pool.begin().await?;
        let m = lock_writable_member(&mut tx, member).await?;
        let tenant = m.tenant_id.as_uuid();
        let project = database_project_in(&mut tx, tenant, database, false)
            .await?
            .ok_or(AccessWriteError::NotFound)?;
        if let Some(expect) = expect {
            if expect.project != project {
                return Err(AccessWriteError::Changed);
            }
            require_observed(&mut tx, member, expect).await?;
        }
        let before = sqlx::query(
            "DELETE FROM database_grant_overrides
             WHERE member_id = $1 AND database_id = $2 
             RETURNING platform_role, pg_roles",
        )
        .bind(member)
        .bind(database)
        .fetch_optional(&mut *tx)
        .await?
        .map(|r| {
            let role: Option<String> = r.try_get("platform_role")?;
            override_from_parts(role.as_deref(), r.try_get("pg_roles")?)
        })
        .transpose()?
        .ok_or(AccessWriteError::NotFound)?;
        NewEvent::new(
            tenant,
            Some(actor),
            AccessAction::OverrideDeleted,
            Some(member),
        )
        .project(project)
        .database(database)
        .before(Some(override_json(&before)))
        .write(&mut tx)
        .await?;
        revoke_unqualified_logins(&mut tx, Some(actor), member).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Stores an `invited` member with its grants (inert until it is active) and an invite.
    /// A removed member with the same Zitadel user is invited again (same row, `tenant_admin`
    /// false, old grants gone). A live member with that Zitadel user or email, an empty email,
    /// a grant naming `owner` (R7), or a project twice is `Conflict`; a project that isn't active
    /// in the tenant, or a missing tenant, `NotFound`.
    pub async fn create_invited_member(
        &self,
        actor: Uuid,
        tenant: TenantId,
        new: NewInvite<'_>,
    ) -> Result<(MemberListing, Uuid), AccessWriteError> {
        let NewInvite { zitadel_user_id, email, expires_at, token_hash, grants, teams } = new;
        if grants.iter().any(|(_, g)| g.names_owner()) {
            return Err(AccessWriteError::Conflict(OWNER_IS_IMPLIED.into()));
        }
        let Some(email) = claim_email(email) else {
            return Err(AccessWriteError::Conflict(
                "an invite needs an email".into(),
            ));
        };
        let tenant_id = tenant.as_uuid();
        let mut tx = self.pool.begin().await?;
        if !lock_tenant(&mut tx, tenant_id).await? {
            return Err(AccessWriteError::NotFound);
        }
        let existing = lock_member_by_zitadel(&mut tx, tenant_id, zitadel_user_id).await?;
        if existing
            .as_ref()
            .is_some_and(|m| m.status != MemberStatus::Removed)
        {
            return Err(AccessWriteError::Conflict(
                "already a member of the tenant".into(),
            ));
        }
        let email_taken: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM members
                            WHERE tenant_id = $1 AND email = lower(btrim($2)) AND status <> 'removed')",
        )
        .bind(tenant_id)
        .bind(email)
        .fetch_one(&mut *tx)
        .await?;
        if email_taken {
            return Err(AccessWriteError::Conflict(
                "another member has this email".into(),
            ));
        }
        require_teams_seen(&mut tx, tenant_id, teams).await?;
        let raw = match &existing {
            Some(removed) => {
                sqlx::query_as::<_, ListingRaw>(concat!(
                    "UPDATE members SET status = 'invited', email = lower(btrim($2)), tenant_admin = false,
                         invited_by = $3, accepted_at = NULL, updated_at = now()
                     WHERE id = $1 RETURNING ",
                    returning_listing!()
                ))
                .bind(removed.id)
                .bind(email)
                .bind(actor)
                .fetch_one(&mut *tx)
                .await
            }
            None => {
                sqlx::query_as::<_, ListingRaw>(concat!(
                    "INSERT INTO members (id, tenant_id, zitadel_user_id, email, status, invited_by)
                     VALUES ($1, $2, $3, lower(btrim($4)), 'invited', $5) RETURNING ",
                    returning_listing!()
                ))
                .bind(Uuid::new_v4())
                .bind(tenant_id)
                .bind(zitadel_user_id)
                .bind(email)
                .bind(actor)
                .fetch_one(&mut *tx)
                .await
            }
        }
        .map_err(write_error)?;
        let mut listing = MemberListing::try_from(raw)?;
        listing.has_access = gives_access(grants, teams);
        let member = listing.member.clone();
        if existing.is_some() {
            delete_member_grants(&mut tx, member.id).await?;
            delete_team_memberships(&mut tx, member.id).await?;
        }
        let grants_json = insert_grants(&mut tx, tenant_id, member.id, actor, grants).await?;
        let invite = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO invites (id, member_id, zitadel_user_id, invited_by, expires_at, token_hash)
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(invite)
        .bind(member.id)
        .bind(zitadel_user_id)
        .bind(actor)
        .bind(expires_at)
        .bind(&token_hash[..])
        .execute(&mut *tx)
        .await?;
        NewEvent::new(
            tenant_id,
            Some(actor),
            AccessAction::InviteCreated,
            Some(member.id),
        )
        .single_project(grants)
        .after(json!({ "email": member.email, "expires_at": expires_at, "grants": grants_json }))
        .write(&mut tx)
        .await?;
        add_team_memberships(&mut tx, tenant_id, actor, member.id, teams, Before::Fresh).await?;
        revoke_unqualified_logins(&mut tx, Some(actor), member.id).await?;
        tx.commit().await?;
        Ok((listing, invite))
    }

    /// An invite's member, expiry, and when it was revoked or accepted.
    pub async fn invite(
        &self,
        invite: Uuid,
    ) -> Result<Option<InviteState>, MetadataError> {
        Ok(sqlx::query_as(
            "SELECT member_id, expires_at, revoked_at, accepted_at FROM invites WHERE id = $1",
        )
        .bind(invite)
        .fetch_optional(&self.pool)
        .await?)
    }

    /// Moves an open invite's expiry and replaces its token (a new code and link were sent). A
    /// revoked or accepted invite is `Conflict`.
    pub async fn resend_invite(
        &self,
        actor: Uuid,
        invite: Uuid,
        expires_at: DateTime<Utc>,
        token_hash: [u8; 32],
    ) -> Result<(), AccessWriteError> {
        let mut tx = self.pool.begin().await?;
        let row: Option<(Uuid, Uuid, DateTime<Utc>, Stamp, Stamp)> = sqlx::query_as(
            "SELECT i.member_id, m.tenant_id, i.expires_at, i.revoked_at, i.accepted_at
                 FROM invites i JOIN members m ON m.id = i.member_id
                 WHERE i.id = $1 FOR UPDATE OF i",
        )
        .bind(invite)
        .fetch_optional(&mut *tx)
        .await?;
        let (member, tenant, old, revoked, accepted) = row.ok_or(AccessWriteError::NotFound)?;
        if revoked.is_some() || accepted.is_some() {
            return Err(AccessWriteError::Conflict(
                "the invite is no longer open".into(),
            ));
        }
        sqlx::query("UPDATE invites SET expires_at = $2, token_hash = $3 WHERE id = $1")
            .bind(invite)
            .bind(expires_at)
            .bind(&token_hash[..])
            .execute(&mut *tx)
            .await?;
        NewEvent::new(
            tenant,
            Some(actor),
            AccessAction::InviteResent,
            Some(member),
        )
        .before(Some(json!({ "expires_at": old })))
        .after(json!({ "expires_at": expires_at }))
        .write(&mut tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Revokes an open invite: the member becomes `removed` and its grants and overrides are
    /// deleted. Returns the never-activated Zitadel user to delete. A revoked or accepted
    /// invite, or a member no longer invited, is `Conflict`.
    pub async fn revoke_invite(
        &self,
        actor: Uuid,
        invite: Uuid,
    ) -> Result<String, AccessWriteError> {
        self.revoke_invite_because(actor, invite, None).await
    }

    /// [`Self::revoke_invite`] naming why in the event (`after.reason`), as the compensating
    /// revoke of an invite whose delivery failed does (`"delivery_failed"`).
    pub async fn revoke_invite_because(
        &self,
        actor: Uuid,
        invite: Uuid,
        reason: Option<&str>,
    ) -> Result<String, AccessWriteError> {
        let member: Uuid = sqlx::query_scalar("SELECT member_id FROM invites WHERE id = $1")
            .bind(invite)
            .fetch_optional(&self.pool)
            .await?
            .ok_or(AccessWriteError::NotFound)?;
        let tenant: Uuid = member_tenant(&self.pool, member)
            .await?
            .ok_or(AccessWriteError::NotFound)?;
        let mut tx = self.pool.begin().await?;
        // Lock order: the tenant (its team memberships go, P26), the member, then its invite.
        lock_tenant(&mut tx, tenant).await?;
        let m = lock_member(&mut tx, member)
            .await?
            .ok_or(AccessWriteError::NotFound)?;
        let (zitadel_user_id, revoked, accepted): (
            String,
            Option<DateTime<Utc>>,
            Option<DateTime<Utc>>,
        ) = sqlx::query_as(
            "SELECT zitadel_user_id, revoked_at, accepted_at FROM invites WHERE id = $1 FOR UPDATE",
        )
        .bind(invite)
        .fetch_one(&mut *tx)
        .await?;
        if revoked.is_some() || accepted.is_some() {
            return Err(AccessWriteError::Conflict(
                "the invite is no longer open".into(),
            ));
        }
        if m.status != MemberStatus::Invited {
            return Err(AccessWriteError::Conflict(
                "the member is no longer invited".into(),
            ));
        }
        sqlx::query(
            "UPDATE invites SET revoked_at = now()
             WHERE member_id = $1 AND accepted_at IS NULL AND revoked_at IS NULL",
        )
        .bind(member)
        .execute(&mut *tx)
        .await?;
        let (grants, overrides) = delete_member_grants(&mut tx, member).await?;
        let teams = delete_team_memberships(&mut tx, member).await?;
        sqlx::query(
            "UPDATE members SET status = 'removed', tenant_admin = false, updated_at = now()
             WHERE id = $1",
        )
        .bind(member)
        .execute(&mut *tx)
        .await?;
        NewEvent::new(
            m.tenant_id.as_uuid(),
            Some(actor),
            AccessAction::InviteRevoked,
            Some(member),
        )
        .before(Some(
            json!({ "status": "invited", "grants": grants, "overrides": overrides, "teams": teams }),
        ))
        .after(match reason {
            Some(reason) => json!({ "status": "removed", "reason": reason }),
            None => json!({ "status": "removed" }),
        })
        .write(&mut tx)
        .await?;
        revoke_logins(&mut tx, Some(actor), member, RevokeReason::MemberRemoved).await?;
        tx.commit().await?;
        Ok(zitadel_user_id)
    }

    /// Whether the tenant's admin was claimed (first sign-in, `admin_claimed`) after `since`:
    /// the window in which a second sign-up of a fresh install may still be waiting to arrive
    /// (R41, M7).
    pub async fn admin_claimed_since(
        &self,
        tenant: TenantId,
        since: DateTime<Utc>,
    ) -> Result<bool, MetadataError> {
        Ok(sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM access_events
                             WHERE tenant_id = $1 AND action = 'admin_claimed' AND at > $2)",
        )
        .bind(tenant.as_uuid())
        .bind(since)
        .fetch_one(&self.pool)
        .await?)
    }

    pub async fn open_signup(&self, tenant: TenantId) -> Result<bool, MetadataError> {
        sqlx::query_scalar("SELECT open_signup FROM tenants WHERE id = $1")
            .bind(tenant.as_uuid())
            .fetch_optional(&self.pool)
            .await?
            .ok_or(MetadataError::TenantNotFound(tenant))
    }

    /// Turns open sign-up on or off; an unchanged switch writes nothing.
    pub async fn set_open_signup(
        &self,
        actor: Uuid,
        tenant: TenantId,
        on: bool,
    ) -> Result<(), AccessWriteError> {
        let mut tx = self.pool.begin().await?;
        let old: bool =
            sqlx::query_scalar("SELECT open_signup FROM tenants WHERE id = $1 FOR NO KEY UPDATE")
                .bind(tenant.as_uuid())
                .fetch_optional(&mut *tx)
                .await?
                .ok_or(AccessWriteError::NotFound)?;
        if old == on {
            return Ok(());
        }
        sqlx::query("UPDATE tenants SET open_signup = $2 WHERE id = $1")
            .bind(tenant.as_uuid())
            .bind(on)
            .execute(&mut *tx)
            .await?;
        NewEvent::new(
            tenant.as_uuid(),
            Some(actor),
            AccessAction::OpenSignupSet,
            None,
        )
        .before(Some(json!({ "open_signup": old })))
        .after(json!({ "open_signup": on }))
        .write(&mut tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Sets the tenant's display name (`None` clears it; the caller trims it, turns an empty
    /// one into `None` and keeps it to 80 characters). An unchanged name writes nothing.
    pub async fn set_tenant_display_name(
        &self,
        actor: Uuid,
        tenant: TenantId,
        display_name: Option<&str>,
    ) -> Result<(), AccessWriteError> {
        let mut tx = self.pool.begin().await?;
        let old: Option<String> = sqlx::query_scalar(
            "SELECT display_name FROM tenants WHERE id = $1 FOR NO KEY UPDATE",
        )
        .bind(tenant.as_uuid())
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(AccessWriteError::NotFound)?;
        if old.as_deref() == display_name {
            return Ok(());
        }
        sqlx::query("UPDATE tenants SET display_name = $2 WHERE id = $1")
            .bind(tenant.as_uuid())
            .bind(display_name)
            .execute(&mut *tx)
            .await?;
        NewEvent::new(
            tenant.as_uuid(),
            Some(actor),
            AccessAction::TenantRenamed,
            None,
        )
        .before(Some(json!({ "display_name": old })))
        .after(json!({ "display_name": display_name }))
        .write(&mut tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// The tenant's access log, newest first: `before_id` pages back, `project` keeps only
    /// that project's events: those carrying it as `project_id`, and those whose `before` or
    /// `after` lists a grant on it (invites, additions and removals naming several projects);
    /// `member` keeps only the events whose target or actor is that member, or whose `before`
    /// or `after` lists it in `affected_member_ids` (team events, MH-7). Both filters apply
    /// together.
    pub async fn access_events(
        &self,
        tenant: TenantId,
        project: Option<Uuid>,
        member: Option<Uuid>,
        before_id: Option<i64>,
        limit: u32,
    ) -> Result<Vec<AccessEvent>, MetadataError> {
        let rows = sqlx::query_as::<_, EventRow>(
            "SELECT e.id, e.tenant_id, e.actor_member_id, e.action, e.target_member_id, e.project_id,
                    e.database_id, e.before, e.after, e.at,
                    am.email AS actor_email, tm.email AS target_email,
                    pr.name AS project_name, e.branch_id
             FROM access_events e
             LEFT JOIN members am ON am.id = e.actor_member_id
             LEFT JOIN members tm ON tm.id = e.target_member_id
             LEFT JOIN projects pr ON pr.id = e.project_id
             WHERE e.tenant_id = $1
               AND ($2::uuid IS NULL OR e.project_id = $2
                    OR e.before -> 'grants' @> jsonb_build_array(jsonb_build_object('project_id', $2::text))
                    OR e.after -> 'grants' @> jsonb_build_array(jsonb_build_object('project_id', $2::text)))
               AND ($5::uuid IS NULL OR e.target_member_id = $5 OR e.actor_member_id = $5
                    OR e.after -> 'affected_member_ids' @> jsonb_build_array($5::text)
                    OR e.before -> 'affected_member_ids' @> jsonb_build_array($5::text))
               AND ($3::bigint IS NULL OR e.id < $3)
             ORDER BY e.id DESC
             LIMIT $4",
        )
        .bind(tenant.as_uuid())
        .bind(project)
        .bind(before_id)
        .bind(i64::from(limit))
        .bind(member)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(AccessEvent::from).collect())
    }

    /// Makes `zitadel_user_id` an active tenant admin (the bootstrap command): inserted, or
    /// activated and promoted. It reactivates any status, a suspended member included: it is
    /// the operator's recovery path (S6), unlike a sign-in; revoked personal logins stay
    /// revoked. Event `admin_set` with no actor, only when the status or the flag changed. A
    /// repeated bootstrap with the same email writes nothing at all. The email takes the
    /// address from any other live holder (R8).
    pub async fn bootstrap_admin(
        &self,
        tenant: TenantId,
        zitadel_user_id: &str,
        email: &str,
    ) -> Result<MemberRecord, MetadataError> {
        let email = claim_email(email);
        let tenant_id = tenant.as_uuid();
        let mut tx = self.pool.begin().await?;
        if !lock_tenant(&mut tx, tenant_id).await? {
            return Err(MetadataError::TenantNotFound(tenant));
        }
        let existing = lock_member_by_zitadel(&mut tx, tenant_id, zitadel_user_id).await?;
        let member = match &existing {
            Some(m) => {
                release_email(&mut tx, tenant_id, m.id, email).await?;
                sqlx::query_as::<_, MemberRow>(concat!(
                    "UPDATE members SET status = 'active', tenant_admin = true, email = lower(btrim($2)),
                         accepted_at = CASE WHEN status = 'invited' THEN now() ELSE accepted_at END,
                         suspended_at = NULL, suspended_by = NULL,
                         updated_at = now()
                     WHERE id = $1
                       AND (status <> 'active' OR NOT tenant_admin
                            OR email IS DISTINCT FROM lower(btrim($2)))
                     RETURNING ",
                    member_columns!()
                ))
                .bind(m.id)
                .bind(email)
                .fetch_optional(&mut *tx)
                .await?
                .map(MemberRecord::try_from)
                .transpose()?
                .unwrap_or_else(|| m.clone())
            }
            None => {
                let id = Uuid::new_v4();
                release_email(&mut tx, tenant_id, id, email).await?;
                let raw = sqlx::query_as::<_, MemberRow>(concat!(
                    "INSERT INTO members (id, tenant_id, zitadel_user_id, email, tenant_admin, status)
                     VALUES ($1, $2, $3, lower(btrim($4)), true, 'active') RETURNING ",
                    member_columns!()
                ))
                .bind(id)
                .bind(tenant_id)
                .bind(zitadel_user_id)
                .bind(email)
                .fetch_one(&mut *tx)
                .await?;
                MemberRecord::try_from(raw)?
            }
        };
        let before = existing.as_ref().map(|m| (m.status, m.tenant_admin));
        if matches!(before, Some((MemberStatus::Invited, _))) {
            sqlx::query(
                "UPDATE invites SET accepted_at = now()
                 WHERE member_id = $1 AND accepted_at IS NULL AND revoked_at IS NULL",
            )
            .bind(member.id)
            .execute(&mut *tx)
            .await?;
        }
        if before != Some((MemberStatus::Active, true)) {
            NewEvent::new(tenant_id, None, AccessAction::AdminSet, Some(member.id))
                .before(before.map(
                    |(status, admin)| json!({ "status": status.as_str(), "tenant_admin": admin }),
                ))
                .after(json!({ "status": "active", "tenant_admin": true }))
                .write(&mut tx)
                .await?;
            revoke_unqualified_logins(&mut tx, None, member.id).await?;
        }
        tx.commit().await?;
        Ok(member)
    }

    /// The member roles recorded on a branch, by member.
    pub async fn member_branch_roles(
        &self,
        branch: Uuid,
    ) -> Result<Vec<MemberBranchRole>, MetadataError> {
        Ok(sqlx::query_as::<_, MemberBranchRole>(
            "SELECT member_id, branch_id, role_name, password_enc, applied_roles
             FROM member_branch_roles WHERE branch_id = $1
             ORDER BY member_id",
        )
        .bind(branch)
        .fetch_all(&self.pool)
        .await?)
    }

    /// R31: records `role` as the last role `member` acted as on `branch` (an act-as run is
    /// starting). False when the member has no member role row there.
    pub async fn record_member_acted_role(
        &self,
        member: Uuid,
        branch: Uuid,
        role: &str,
    ) -> Result<bool, MetadataError> {
        let done = sqlx::query(
            "UPDATE member_branch_roles SET last_acted_role = $3
             WHERE member_id = $1 AND branch_id = $2",
        )
        .bind(member)
        .bind(branch)
        .bind(role)
        .execute(&self.pool)
        .await?;
        Ok(done.rows_affected() == 1)
    }

    /// R31: each member's last acted role on `branch`, where one is recorded.
    pub async fn member_acted_roles(
        &self,
        branch: Uuid,
    ) -> Result<Vec<(Uuid, String)>, MetadataError> {
        Ok(sqlx::query_as::<_, (Uuid, String)>(
            "SELECT member_id, last_acted_role FROM member_branch_roles
             WHERE branch_id = $1 AND last_acted_role IS NOT NULL
             ORDER BY member_id",
        )
        .bind(branch)
        .fetch_all(&self.pool)
        .await?)
    }

    pub async fn upsert_member_branch_role(
        &self,
        r: &MemberBranchRole,
    ) -> Result<(), MetadataError> {
        sqlx::query(
            "INSERT INTO member_branch_roles
                 (member_id, branch_id, role_name, password_enc, applied_roles)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (member_id, branch_id) DO UPDATE
             SET role_name = EXCLUDED.role_name, password_enc = EXCLUDED.password_enc,
                 applied_roles = EXCLUDED.applied_roles, applied_at = now()",
        )
        .bind(r.member_id)
        .bind(r.branch_id)
        .bind(&r.role_name)
        .bind(&r.password_enc)
        .bind(&r.applied_roles)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// EOS-1: [`MetadataClient::upsert_member_branch_role`] as a compare-and-set: the row is
    /// written only while its stored `password_enc` is still `read`, the value the writer's run
    /// read (`None`: there was no row). `false`, nothing written, when it changed meanwhile (a
    /// suspension's or revoke-now's rotation mark, `''`, must survive a reconcile that read the
    /// row before it).
    pub async fn upsert_member_branch_role_if(
        &self,
        r: &MemberBranchRole,
        read: Option<&str>,
    ) -> Result<bool, MetadataError> {
        let written = match read {
            // No row was read: only an insert. A row made meanwhile (another run's) is kept.
            None => sqlx::query(
                "INSERT INTO member_branch_roles
                     (member_id, branch_id, role_name, password_enc, applied_roles)
                 VALUES ($1, $2, $3, $4, $5)
                 ON CONFLICT (member_id, branch_id) DO NOTHING",
            )
            .bind(r.member_id)
            .bind(r.branch_id)
            .bind(&r.role_name)
            .bind(&r.password_enc)
            .bind(&r.applied_roles)
            .execute(&self.pool)
            .await?
            .rows_affected(),
            // The row as read: one statement, so the comparison and the write see the same row
            // (a concurrent writer's UPDATE is waited for, then the condition rechecked).
            Some(read) => sqlx::query(
                "UPDATE member_branch_roles
                 SET role_name = $3, password_enc = $4, applied_roles = $5, applied_at = now()
                 WHERE member_id = $1 AND branch_id = $2 AND password_enc = $6",
            )
            .bind(r.member_id)
            .bind(r.branch_id)
            .bind(&r.role_name)
            .bind(&r.password_enc)
            .bind(&r.applied_roles)
            .bind(read)
            .execute(&self.pool)
            .await?
            .rows_affected(),
        };
        Ok(written == 1)
    }

    pub async fn delete_member_branch_role(
        &self,
        member: Uuid,
        branch: Uuid,
    ) -> Result<(), MetadataError> {
        sqlx::query(
            "DELETE FROM member_branch_roles
             WHERE member_id = $1 AND branch_id = $2",
        )
        .bind(member)
        .bind(branch)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Every live branch (not deleting or deleted) of every live database of the project.
    pub async fn project_branches(&self, project: Uuid) -> Result<Vec<Uuid>, MetadataError> {
        Ok(sqlx::query_scalar(
            "SELECT b.id FROM branches b JOIN databases d ON d.id = b.database_id
             WHERE d.project_id = $1
               AND d.status NOT IN ('deleting', 'deleted')
               AND b.status NOT IN ('deleting', 'deleted')
             ORDER BY b.id",
        )
        .bind(project)
        .fetch_all(&self.pool)
        .await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // T1 audit (b), coder's edges: an `expect` must name every project a joined team reaches;
    // extra projects (the own grants') are fine, and teams without grants need nothing.
    #[test]
    fn an_expect_must_cover_every_team_project() {
        let (p, q, r) = (Uuid::from_u128(10), Uuid::from_u128(20), Uuid::from_u128(30));
        let g = GrantRow { platform_role: PlatformRole::Viewer, pg_roles: vec![] };
        let seen = |team: u128, projects: &[Uuid]| TeamSeen {
            team: Uuid::from_u128(team),
            grants: projects.iter().map(|p| (*p, g.clone())).collect(),
        };
        let at = |project: Uuid| Observed { project, ..Observed::default() };
        let teams = [seen(1, &[p]), seen(2, &[q, p])];
        assert!(require_expect_covers(&teams, &[at(p), at(q)]).is_ok());
        assert!(require_expect_covers(&teams, &[at(r), at(q), at(p)]).is_ok(), "extra projects");
        assert!(matches!(require_expect_covers(&teams, &[at(p)]), Err(AccessWriteError::Conflict(m)) if m.contains(&q.to_string())));
        assert!(matches!(require_expect_covers(&teams, &[]), Err(AccessWriteError::Conflict(_))));
        assert!(require_expect_covers(&[seen(3, &[])], &[]).is_ok(), "a team without grants");
        assert!(require_expect_covers(&[], &[]).is_ok(), "no teams");
    }

    fn raw(status: &str) -> MemberRaw {
        MemberRaw {
            id: Uuid::from_u128(1),
            tenant_id: Uuid::from_u128(2),
            zitadel_user_id: "z1".into(),
            email: Some("a@x.com".into()),
            display_name: "A".into(),
            tenant_admin: true,
            status: status.into(),
            accepted_at: None,
        }
    }

    #[test]
    fn member_status_round_trips() {
        for s in [
            MemberStatus::Invited,
            MemberStatus::Pending,
            MemberStatus::Active,
            MemberStatus::Removed,
            MemberStatus::Suspended,
        ] {
            assert_eq!(MemberStatus::parse(s.as_str()), Some(s));
            assert_eq!(serde_json::to_value(s).unwrap(), json!(s.as_str()));
        }
        // T3: the suspended status (members hub spec Part 6).
        assert_eq!(MemberStatus::Suspended.as_str(), "suspended");
        assert!(matches!(
            MemberRecord::try_from(raw("suspended")).map(|m| m.status),
            Ok(MemberStatus::Suspended)
        ));
        assert_eq!(MemberStatus::Invited.as_str(), "invited");
        assert_eq!(MemberStatus::Pending.as_str(), "pending");
        assert_eq!(MemberStatus::Active.as_str(), "active");
        assert_eq!(MemberStatus::Removed.as_str(), "removed");
        assert_eq!(MemberStatus::parse("Active"), None);
    }

    #[test]
    fn member_row_maps_to_record() {
        let record = MemberRecord::try_from(raw("active")).unwrap();
        assert_eq!(
            record,
            MemberRecord {
                id: Uuid::from_u128(1),
                tenant_id: TenantId(Uuid::from_u128(2)),
                zitadel_user_id: "z1".into(),
                email: Some("a@x.com".into()),
                display_name: "A".into(),
                tenant_admin: true,
                status: MemberStatus::Active,
                accepted_at: None,
                offboarded: false, suspended_at: None, suspended_by: None,
            }
        );
        // A stale email (taken by a newer claim) maps to None.
        let stale = MemberRecord::try_from(MemberRaw {
            email: None,
            ..raw("active")
        })
        .unwrap();
        assert_eq!(stale.email, None);
        assert!(matches!(
            MemberRecord::try_from(raw("gone")),
            Err(sqlx::Error::Decode(_))
        ));
    }

    #[test]
    fn grant_parts_map_to_rows() {
        let g = grant_from_parts("developer", vec!["app".into()]).unwrap();
        assert_eq!(
            g,
            GrantRow {
                platform_role: PlatformRole::Developer,
                pg_roles: vec!["app".into()]
            }
        );
        assert!(grant_from_parts("root", vec![]).is_err());

        assert_eq!(
            override_from_parts(None, None).unwrap(),
            OverrideRow::default()
        );
        assert_eq!(
            override_from_parts(Some("viewer"), Some(vec!["r".into()])).unwrap(),
            OverrideRow {
                platform_role: Some(PlatformRole::Viewer),
                pg_roles: Some(vec!["r".into()])
            }
        );
        assert!(override_from_parts(Some("boss"), None).is_err());
    }

    // R17: an Admin grant holds every role: the API writes it with no roles, and a row from
    // before (naming some) still reads as every role.
    #[test]
    fn an_admin_grant_is_every_role() {
        let names = |n: &[&str]| n.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let admin = GrantRow::new(PlatformRole::Admin, &names(&["b", "a"]));
        assert_eq!(admin.pg_roles, Vec::<String>::new());
        assert!(admin.every_role());
        assert_eq!(admin.to_grant(), Grant { platform_role: PlatformRole::Admin, roles: RoleSet::all() });
        let old = GrantRow { platform_role: PlatformRole::Admin, pg_roles: names(&["a"]) };
        assert_eq!(old.to_grant().roles, RoleSet::all());
        let dev = GrantRow::new(PlatformRole::Developer, &names(&["b", "a", "b"]));
        assert_eq!(dev.pg_roles, names(&["a", "b"]));
        assert!(!dev.every_role());
        assert_eq!(
            dev.to_grant().roles,
            RoleSet::Only([PgRole::Named("a".into()), PgRole::Named("b".into())].into_iter().collect())
        );
    }

    // R7: Admin implies the database owner, so `owner` is never stored, whatever the role.
    #[test]
    fn owner_is_never_stored() {
        let g = |role, roles: &[&str]| GrantRow {
            platform_role: role,
            pg_roles: roles.iter().map(|s| s.to_string()).collect(),
        };
        for role in [
            PlatformRole::Viewer,
            PlatformRole::Developer,
            PlatformRole::Admin,
        ] {
            assert!(g(role, &["app", "owner"]).names_owner(), "{role:?}");
            assert!(g(role, &["owner"]).names_owner(), "{role:?}");
            assert!(!g(role, &["app"]).names_owner(), "{role:?}");
            assert!(!g(role, &[]).names_owner(), "{role:?}");
            // Only the exact symbolic name: a Postgres role spelled "Owner" is another role.
            assert!(!g(role, &["Owner"]).names_owner(), "{role:?}");
        }
        let o = |roles: Option<&[&str]>| OverrideRow {
            platform_role: Some(PlatformRole::Admin),
            pg_roles: roles.map(|r| r.iter().map(|s| s.to_string()).collect()),
        };
        assert!(o(Some(&["owner"])).names_owner());
        assert!(o(Some(&["a", "owner"])).names_owner());
        assert!(!o(Some(&["a"])).names_owner());
        assert!(!o(Some(&["Owner"])).names_owner());
        assert!(!o(None).names_owner());
    }

    // R8: a claim's email is trimmed; an empty one is unknown (stored NULL).
    #[test]
    fn claim_email_is_trimmed_or_unknown() {
        assert_eq!(claim_email("  Bob@X.com\t"), Some("Bob@X.com"));
        assert_eq!(claim_email("a@x.com"), Some("a@x.com"));
        assert_eq!(claim_email(""), None);
        assert_eq!(claim_email(" \t "), None);
    }

    #[test]
    fn roles_are_sorted_and_deduplicated() {
        let roles: Vec<String> = ["b", "a", "b", "c", "a"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(normalize_roles(&roles), vec!["a", "b", "c"]);
        assert!(normalize_roles(&[]).is_empty());
    }

    #[test]
    fn event_json_is_the_grant_or_override() {
        let g = GrantRow {
            platform_role: PlatformRole::Admin,
            pg_roles: vec!["app".into()],
        };
        assert_eq!(
            grant_json(&g),
            json!({"platform_role": "admin", "pg_roles": ["app"]})
        );
        let o = OverrideRow {
            platform_role: None,
            pg_roles: Some(vec!["r".into()]),
        };
        assert_eq!(
            override_json(&o),
            json!({"platform_role": null, "pg_roles": ["r"]})
        );
    }

    #[test]
    fn actions_are_the_table_values() {
        let names: Vec<&str> = AccessAction::ALL.iter().map(|a| a.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "member_joined",
                "admin_claimed",
                "admin_set",
                "admin_unset",
                "member_removed",
                "grant_put",
                "grant_deleted",
                "override_put",
                "override_deleted",
                "invite_created",
                "invite_resent",
                "invite_revoked",
                "invite_accepted",
                "open_signup_set",
                "tenant_renamed",
                "member_approved",
                "member_declined",
                "member_added",
                // T3 (members hub step 1): every new action, in the plan's table order.
                "member_suspended",
                "member_reinstated",
                "personal_logins_revoked",
                "personal_login_created",
                "personal_login_reset",
                "personal_login_deleted",
                "role_created",
                "role_updated",
                "role_deleted",
                "privileges_changed",
                // Members hub step 2 (P9): the team events.
                "team_created",
                "team_updated",
                "team_deleted",
                "team_grant_put",
                "team_grant_deleted",
                "team_member_added",
                "team_member_removed",
            ]
        );
    }

    fn g(role: PlatformRole, roles: &[&str]) -> GrantRow {
        GrantRow { platform_role: role, pg_roles: roles.iter().map(|r| r.to_string()).collect() }
    }

    // P7: an `Observed` reads as the sources it holds: the own grant, every team grant (by team
    // id) and, for a database, that database's override; `with_*` builds the state a write would
    // leave without touching the original.
    #[test]
    fn observed_sources_and_the_after_state() {
        let (project, db1, db2) = (Uuid::from_u128(10), Uuid::from_u128(20), Uuid::from_u128(21));
        let (x, y) = (Uuid::from_u128(30), Uuid::from_u128(31));
        let own = g(PlatformRole::Viewer, &["app"]);
        let tx = g(PlatformRole::Developer, &["etl"]);
        let narrow = OverrideRow { platform_role: Some(PlatformRole::Viewer), pg_roles: None };
        let observed = Observed {
            project,
            grant: Some(own.clone()),
            overrides: [(db1, narrow.clone())].into_iter().collect(),
            teams: [(x, tx.clone())].into_iter().collect(),
        };
        assert_eq!(
            observed.sources(Some(db1)),
            GrantSources::new(Some(own.to_grant()), vec![tx.to_grant()], Some(narrow.to_narrowing())),
            "on a database with an override"
        );
        assert_eq!(
            observed.sources(Some(db2)),
            GrantSources::new(Some(own.to_grant()), vec![tx.to_grant()], None),
            "on a database without one"
        );
        assert_eq!(
            observed.sources(None),
            GrantSources::new(Some(own.to_grant()), vec![tx.to_grant()], None),
            "at project level"
        );

        // A second team, with a lower id, comes first.
        let w = Uuid::from_u128(29);
        let tw = g(PlatformRole::Admin, &[]);
        let more = observed.with_team(w, Some(tw.clone()));
        assert_eq!(
            more.sources(None),
            GrantSources::new(Some(own.to_grant()), vec![tw.to_grant(), tx.to_grant()], None)
        );
        // Replacing and removing a team's grant.
        let ty = g(PlatformRole::Viewer, &[]);
        assert_eq!(observed.with_team(y, Some(ty.clone())).teams.get(&y), Some(&ty));
        assert_eq!(observed.with_team(x, Some(ty.clone())).teams.get(&x), Some(&ty));
        assert!(observed.with_team(x, None).teams.is_empty(), "with_team(x, None) drops x");
        // Own grant and override.
        assert_eq!(
            observed.with_own(None).sources(None),
            GrantSources::new(None, vec![tx.to_grant()], None)
        );
        let admin = g(PlatformRole::Admin, &[]);
        assert_eq!(observed.with_own(Some(admin.clone())).grant, Some(admin));
        assert!(observed.with_override(db1, None).overrides.is_empty());
        let other = OverrideRow { platform_role: None, pg_roles: Some(vec!["app".into()]) };
        assert_eq!(observed.with_override(db2, Some(other.clone())).overrides.get(&db2), Some(&other));
        // The original is untouched.
        assert_eq!(observed.teams.len(), 1);
        assert_eq!(observed.grant, Some(own));
        assert_eq!(observed.overrides.len(), 1);
    }

    #[test]
    fn event_row_maps_to_event() {
        let at = Utc::now();
        let event = AccessEvent::from(EventRaw {
            id: 7,
            tenant_id: Uuid::from_u128(2),
            actor_member_id: None,
            action: "admin_set".into(),
            target_member_id: Some(Uuid::from_u128(1)),
            project_id: None,
            database_id: None,
            before: None,
            after: Some(json!({"tenant_admin": true})),
            at,
            actor_email: None,
            target_email: Some("t@x.com".into()),
            project_name: None,
        });
        assert_eq!(event.target_email.as_deref(), Some("t@x.com"));
        assert_eq!(event.id, 7);
        assert_eq!(event.tenant_id, TenantId(Uuid::from_u128(2)));
        assert_eq!(event.action, "admin_set");
        assert_eq!(event.target_member_id, Some(Uuid::from_u128(1)));
        assert_eq!(event.after, Some(json!({"tenant_admin": true})));
        assert_eq!(event.at, at);
    }
}
