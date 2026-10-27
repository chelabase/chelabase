//! Teams (members hub step 2, spec Part 5): a team holds per-project grants and its members get
//! them live.
//!
//! Every team write takes the tenant row, then the affected members' rows in id order (P6), so
//! team writes serialize on the tenant and a team's member set is stable under that lock: the
//! only writers of `team_members` (team writes, invites and approvals with teams,
//! `remove_member`, `revoke_invite`) all hold it first (P26). The writes don't validate role
//! chains: the orchestrator checks the union with the [`Observed`] it read
//! ([`MetadataClient::team_change_observed`]) and passes that read as `expect`; under the locks
//! the write reads it again and refuses any difference, the affected set included (I4).
//!
//! Reads and writes count a team grant as the shared shape does (P4, P27): only on a
//! non-deleted project of the team's tenant, and only for members of that tenant.

use std::collections::BTreeMap;

use chelabase_types::TenantId;
use chrono::{DateTime, Utc};
use serde_json::json;
use sqlx::Row;
use uuid::Uuid;

use crate::members::{
    AccessAction, GrantRow, MemberStatus, NewEvent, OWNER_ROLE, Observed, Tx, lock_member,
    lock_tenant, observe_many_tx, team_event_json, team_grants_tx,
};
use crate::personal_logins::revoke_unqualified_logins;
use crate::{MetadataClient, MetadataError};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeamRecord {
    pub id: Uuid,
    pub tenant_id: TenantId,
    pub name: String,
    pub description: String,
    pub created_by: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// A team with its grants on non-deleted projects (by project id) and its members (by id).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeamDetail {
    pub team: TeamRecord,
    pub grants: Vec<(Uuid, GrantRow)>,
    pub member_ids: Vec<Uuid>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectTeamGrant {
    pub team_id: Uuid,
    pub name: String,
    pub grant: GrantRow,
    pub member_ids: Vec<Uuid>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TeamChange {
    PutGrant {
        team: Uuid,
        project: Uuid,
        grant: GrantRow,
    },
    DeleteGrant {
        team: Uuid,
        project: Uuid,
    },
    AddMember {
        team: Uuid,
        member: Uuid,
    },
    RemoveMember {
        team: Uuid,
        member: Uuid,
    },
    Delete {
        team: Uuid,
    },
}

impl TeamChange {
    /// The team the change is about.
    pub fn team(&self) -> Uuid {
        match self {
            Self::PutGrant { team, .. }
            | Self::DeleteGrant { team, .. }
            | Self::AddMember { team, .. }
            | Self::RemoveMember { team, .. }
            | Self::Delete { team } => *team,
        }
    }
}

/// What a committed team write reaches: each affected member as (id, Zitadel user), by id, and
/// every project whose branches reconcile (the team's grant projects before and after), by id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeamWrite {
    pub changed: bool,
    pub affected: Vec<(Uuid, String)>,
    pub projects: Vec<Uuid>,
}

#[derive(Debug, thiserror::Error)]
pub enum TeamWriteError {
    /// The team (or another tenant's).
    #[error("team not found")]
    NotFound,
    /// Not an active project of the tenant.
    #[error("project not found")]
    ProjectNotFound,
    /// Missing, another tenant's or removed.
    #[error("member not found")]
    MemberNotFound,
    /// A sign-up waiting for approval.
    #[error("the member is waiting for approval")]
    MemberPending,
    #[error("a team of the tenant already has this name")]
    NameTaken,
    /// I4: an `expect` no longer holds, or the affected set changed.
    #[error("the team's access changed since it was checked")]
    Changed,
    /// R7 (`owner`), or a name or description out of bounds (P2).
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),
}

/// The check's sets of a change (pure): the members it reaches and the projects whose access
/// changes for them, both by id and without duplicates, given the team's members and the
/// projects of its grants as they stand before the change. A grant put or delete reaches every
/// member on that one project; a member added or removed reaches that member on every grant
/// project; a team delete reaches every member on every grant project.
pub(crate) fn affected_sets(
    change: &TeamChange,
    members: &[Uuid],
    grant_projects: &[Uuid],
) -> (Vec<Uuid>, Vec<Uuid>) {
    let sorted = |ids: &[Uuid]| {
        let mut ids = ids.to_vec();
        ids.sort();
        ids.dedup();
        ids
    };
    match change {
        TeamChange::PutGrant { project, .. } | TeamChange::DeleteGrant { project, .. } => {
            (sorted(members), vec![*project])
        }
        TeamChange::AddMember { member, .. } | TeamChange::RemoveMember { member, .. } => {
            (vec![*member], sorted(grant_projects))
        }
        TeamChange::Delete { .. } => (sorted(members), sorted(grant_projects)),
    }
}

/// `R7`'s message, as the grant writes word it.
const OWNER_IS_IMPLIED: &str =
    "the owner role is never granted: the admin platform role implies the database owner";

/// A refused unique index on the team's name is `NameTaken`; a refused CHECK `Conflict`.
fn team_write_error(e: sqlx::Error) -> TeamWriteError {
    if let sqlx::Error::Database(db) = &e {
        if db.is_unique_violation() {
            return TeamWriteError::NameTaken;
        }
        if db.is_check_violation() {
            return TeamWriteError::Conflict(format!(
                "refused by {}",
                db.constraint().unwrap_or("a constraint")
            ));
        }
    }
    TeamWriteError::Db(e)
}

/// P2: a team name is trimmed and 1–80 characters; a description at most 500.
fn check_name(name: &str) -> Result<&str, TeamWriteError> {
    let name = name.trim();
    match name.chars().count() {
        1..=80 => Ok(name),
        _ => Err(TeamWriteError::Conflict(
            "a team name has 1 to 80 characters".into(),
        )),
    }
}

fn check_description(description: &str) -> Result<&str, TeamWriteError> {
    if description.chars().count() > 500 {
        return Err(TeamWriteError::Conflict(
            "a team description has at most 500 characters".into(),
        ));
    }
    Ok(description)
}

macro_rules! team_columns {
    () => {
        "id, tenant_id, name, description, created_by, created_at, updated_at"
    };
}

type TeamRow = (
    Uuid,
    Uuid,
    String,
    String,
    Option<Uuid>,
    DateTime<Utc>,
    DateTime<Utc>,
);

fn record(
    (id, tenant, name, description, created_by, created_at, updated_at): TeamRow,
) -> TeamRecord {
    TeamRecord {
        id,
        tenant_id: TenantId(tenant),
        name,
        description,
        created_by,
        created_at,
        updated_at,
    }
}

/// The team when it is one of `tenant`'s.
async fn team_in(tx: &mut Tx, tenant: Uuid, team: Uuid) -> Result<Option<TeamRecord>, sqlx::Error> {
    Ok(sqlx::query_as::<_, TeamRow>(concat!(
        "SELECT ",
        team_columns!(),
        " FROM teams WHERE id = $1 AND tenant_id = $2"
    ))
    .bind(team)
    .bind(tenant)
    .fetch_optional(&mut **tx)
    .await?
    .map(record))
}

/// The team's members (of its own tenant), by id.
async fn team_member_ids(tx: &mut Tx, tenant: Uuid, team: Uuid) -> Result<Vec<Uuid>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT tm.member_id FROM team_members tm
         JOIN members m ON m.id = tm.member_id AND m.tenant_id = $1
         WHERE tm.team_id = $2 ORDER BY tm.member_id",
    )
    .bind(tenant)
    .bind(team)
    .fetch_all(&mut **tx)
    .await
}

/// What a change reaches, as it stands now (validated).
struct Plan {
    team: TeamRecord,
    grants: Vec<(Uuid, GrantRow)>,
    affected: Vec<Uuid>,
    projects: Vec<Uuid>,
}

/// Validates `change` in `tenant` and computes its affected sets. Tenancy, R7 and pending
/// members are checked here; role chains are the orchestrator's.
async fn plan(tx: &mut Tx, tenant: Uuid, change: &TeamChange) -> Result<Plan, TeamWriteError> {
    if let TeamChange::PutGrant { grant, .. } = change
        && grant.pg_roles.iter().any(|r| r == OWNER_ROLE)
    {
        return Err(TeamWriteError::Conflict(OWNER_IS_IMPLIED.into()));
    }
    let team = team_in(tx, tenant, change.team())
        .await?
        .ok_or(TeamWriteError::NotFound)?;
    match change {
        TeamChange::PutGrant { project, .. } => {
            let active: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM projects
                                 WHERE id = $1 AND tenant_id = $2 AND status = 'active')",
            )
            .bind(project)
            .bind(tenant)
            .fetch_one(&mut **tx)
            .await?;
            if !active {
                return Err(TeamWriteError::ProjectNotFound);
            }
        }
        TeamChange::DeleteGrant { project, .. } => {
            let found: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM projects WHERE id = $1 AND tenant_id = $2)",
            )
            .bind(project)
            .bind(tenant)
            .fetch_one(&mut **tx)
            .await?;
            if !found {
                return Err(TeamWriteError::ProjectNotFound);
            }
        }
        TeamChange::AddMember { member, .. } | TeamChange::RemoveMember { member, .. } => {
            let row: Option<(Uuid, String)> =
                sqlx::query_as("SELECT tenant_id, status FROM members WHERE id = $1")
                    .bind(member)
                    .fetch_optional(&mut **tx)
                    .await?;
            match row {
                Some((t, status)) if t == tenant => match MemberStatus::parse(&status) {
                    Some(MemberStatus::Removed) | None => {
                        return Err(TeamWriteError::MemberNotFound);
                    }
                    Some(MemberStatus::Pending)
                        if matches!(change, TeamChange::AddMember { .. }) =>
                    {
                        return Err(TeamWriteError::MemberPending);
                    }
                    _ => {}
                },
                _ => return Err(TeamWriteError::MemberNotFound),
            }
        }
        TeamChange::Delete { .. } => {}
    }
    let members = team_member_ids(tx, tenant, team.id).await?;
    let grants = team_grants_tx(tx, tenant, team.id).await?;
    let projects: Vec<Uuid> = grants.iter().map(|(p, _)| *p).collect();
    let (affected, projects) = affected_sets(change, &members, &projects);
    Ok(Plan {
        team,
        grants,
        affected,
        projects,
    })
}

/// `(member, Observed)` for every affected member × affected project, by member then project.
///
/// For `AddMember`, each `Observed` also holds the team's grant on its project, as the write
/// would add it: the member isn't in the team yet, so without it a team grant changed between
/// the check and the write (same projects, other roles) would compare equal and the member
/// would join with grants nobody checked.
async fn observe(
    tx: &mut Tx,
    plan: &Plan,
    change: &TeamChange,
) -> Result<Vec<(Uuid, Observed)>, sqlx::Error> {
    // Three set-based reads for the whole member × project set (not three per pair).
    let mut all = observe_many_tx(tx, &plan.affected, &plan.projects).await?;
    let mut out = Vec::with_capacity(plan.affected.len() * plan.projects.len());
    for member in &plan.affected {
        for project in &plan.projects {
            let mut observed = all.remove(&(*member, *project)).unwrap_or(Observed {
                project: *project,
                ..Observed::default()
            });
            if let TeamChange::AddMember { team, .. } = change
                && let Some((_, grant)) = plan.grants.iter().find(|(p, _)| p == project)
            {
                observed.teams.insert(*team, grant.clone());
            }
            out.push((*member, observed));
        }
    }
    Ok(out)
}

/// Reads the teams chosen by `filter` (a `WHERE` over `teams t` binding `$1`), by lower(name),
/// with their grants (non-deleted projects of the team's tenant) and members.
async fn details(
    pool: &sqlx::PgPool,
    filter: &'static str,
    bind: Uuid,
) -> Result<Vec<TeamDetail>, sqlx::Error> {
    let sql = format!(
        "SELECT t.id, t.tenant_id, t.name, t.description, t.created_by, t.created_at, t.updated_at
         FROM teams t WHERE {filter} ORDER BY lower(t.name), t.id"
    );
    let teams: Vec<TeamRecord> = sqlx::query_as::<_, TeamRow>(sqlx::AssertSqlSafe(sql))
        .bind(bind)
        .fetch_all(pool)
        .await?
        .into_iter()
        .map(record)
        .collect();
    let ids: Vec<Uuid> = teams.iter().map(|t| t.id).collect();
    let rows = sqlx::query(
        "SELECT tg.team_id, tg.project_id, tg.platform_role, tg.pg_roles
         FROM team_grants tg
         JOIN teams t ON t.id = tg.team_id
         JOIN projects p ON p.id = tg.project_id AND p.tenant_id = t.tenant_id
                        AND p.status <> 'deleted'
         WHERE tg.team_id = ANY($1)
         ORDER BY tg.team_id, tg.project_id",
    )
    .bind(&ids)
    .fetch_all(pool)
    .await?;
    let mut grants: BTreeMap<Uuid, Vec<(Uuid, GrantRow)>> = BTreeMap::new();
    for r in &rows {
        let role: String = r.try_get("platform_role")?;
        let role = chelabase_types::access::PlatformRole::parse(&role)
            .ok_or_else(|| sqlx::Error::Decode(format!("invalid platform role {role:?}").into()))?;
        grants.entry(r.try_get("team_id")?).or_default().push((
            r.try_get("project_id")?,
            GrantRow {
                platform_role: role,
                pg_roles: r.try_get("pg_roles")?,
            },
        ));
    }
    let rows: Vec<(Uuid, Uuid)> = sqlx::query_as(
        "SELECT tm.team_id, tm.member_id
         FROM team_members tm
         JOIN teams t ON t.id = tm.team_id
         JOIN members m ON m.id = tm.member_id AND m.tenant_id = t.tenant_id
         WHERE tm.team_id = ANY($1)
         ORDER BY tm.team_id, tm.member_id",
    )
    .bind(&ids)
    .fetch_all(pool)
    .await?;
    let mut members: BTreeMap<Uuid, Vec<Uuid>> = BTreeMap::new();
    for (team, member) in rows {
        members.entry(team).or_default().push(member);
    }
    Ok(teams
        .into_iter()
        .map(|team| TeamDetail {
            grants: grants.remove(&team.id).unwrap_or_default(),
            member_ids: members.remove(&team.id).unwrap_or_default(),
            team,
        })
        .collect())
}

impl MetadataClient {
    /// The tenant's teams, by lower(name).
    pub async fn list_teams(&self, tenant: TenantId) -> Result<Vec<TeamDetail>, MetadataError> {
        Ok(details(&self.pool, "t.tenant_id = $1", tenant.as_uuid()).await?)
    }

    /// One team of `tenant` (another tenant's is `None`).
    pub async fn team(
        &self,
        tenant: TenantId,
        team: Uuid,
    ) -> Result<Option<TeamDetail>, MetadataError> {
        let found = details(&self.pool, "t.id = $1", team).await?;
        Ok(found.into_iter().find(|d| d.team.tenant_id == tenant))
    }

    /// The member's teams (of the member's own tenant), by lower(name).
    pub async fn member_teams(&self, member: Uuid) -> Result<Vec<TeamDetail>, MetadataError> {
        Ok(details(
            &self.pool,
            "t.id IN (SELECT tm.team_id FROM team_members tm
                       JOIN members m ON m.id = tm.member_id
                       WHERE tm.member_id = $1 AND m.tenant_id = t.tenant_id)",
            member,
        )
        .await?)
    }

    /// The team grants on `project` (teams of the project's tenant; none for a deleted
    /// project), by lower(name), each with the team's members.
    pub async fn project_team_grants(
        &self,
        project: Uuid,
    ) -> Result<Vec<ProjectTeamGrant>, MetadataError> {
        let teams = details(
            &self.pool,
            "t.id IN (SELECT tg.team_id FROM team_grants tg
                       JOIN projects p ON p.id = tg.project_id
                       WHERE tg.project_id = $1 AND p.tenant_id = t.tenant_id
                         AND p.status <> 'deleted')",
            project,
        )
        .await?;
        Ok(teams
            .into_iter()
            .filter_map(|d| {
                let grant = d.grants.iter().find(|(p, _)| *p == project)?.1.clone();
                Some(ProjectTeamGrant {
                    team_id: d.team.id,
                    name: d.team.name,
                    grant,
                    member_ids: d.member_ids,
                })
            })
            .collect())
    }

    /// Creates a team (P2: the name trimmed, 1–80 characters, unique per tenant ignoring case,
    /// else `NameTaken`; the description at most 500). Event `team_created`. A missing tenant is
    /// `NotFound`.
    pub async fn create_team(
        &self,
        actor: Uuid,
        tenant: TenantId,
        name: &str,
        description: &str,
    ) -> Result<TeamRecord, TeamWriteError> {
        let name = check_name(name)?;
        let description = check_description(description)?;
        let tenant_id = tenant.as_uuid();
        let mut tx = self.pool.begin().await?;
        if !lock_tenant(&mut tx, tenant_id).await? {
            return Err(TeamWriteError::NotFound);
        }
        let created = sqlx::query_as::<_, TeamRow>(concat!(
            "INSERT INTO teams (id, tenant_id, name, description, created_by)
             VALUES ($1, $2, $3, $4, $5) RETURNING ",
            team_columns!()
        ))
        .bind(Uuid::new_v4())
        .bind(tenant_id)
        .bind(name)
        .bind(description)
        .bind(actor)
        .fetch_one(&mut *tx)
        .await
        .map_err(team_write_error)?;
        let team = record(created);
        let mut after = team_event_json(team.id, &team.name, &[], &[]);
        after["description"] = json!(team.description);
        NewEvent::new(tenant_id, Some(actor), AccessAction::TeamCreated, None)
            .after(after)
            .write(&mut tx)
            .await?;
        tx.commit().await?;
        Ok(team)
    }

    /// Renames a team or changes its description (`None` keeps a field), with P2's rules. An
    /// unchanged team writes nothing. Event `team_updated`. Another tenant's team is `NotFound`.
    pub async fn update_team(
        &self,
        actor: Uuid,
        tenant: TenantId,
        team: Uuid,
        name: Option<&str>,
        description: Option<&str>,
    ) -> Result<TeamRecord, TeamWriteError> {
        let name = name.map(check_name).transpose()?;
        let description = description.map(check_description).transpose()?;
        let tenant_id = tenant.as_uuid();
        let mut tx = self.pool.begin().await?;
        if !lock_tenant(&mut tx, tenant_id).await? {
            return Err(TeamWriteError::NotFound);
        }
        let before = team_in(&mut tx, tenant_id, team)
            .await?
            .ok_or(TeamWriteError::NotFound)?;
        let new_name = name.unwrap_or(&before.name);
        let new_description = description.unwrap_or(&before.description);
        if new_name == before.name && new_description == before.description {
            return Ok(before);
        }
        let updated = sqlx::query_as::<_, TeamRow>(concat!(
            "UPDATE teams SET name = $2, description = $3, updated_at = now()
             WHERE id = $1 RETURNING ",
            team_columns!()
        ))
        .bind(team)
        .bind(new_name)
        .bind(new_description)
        .fetch_one(&mut *tx)
        .await
        .map_err(team_write_error)?;
        let after = record(updated);
        let mut before_json = team_event_json(before.id, &before.name, &[], &[]);
        before_json["description"] = json!(before.description);
        let mut after_json = team_event_json(after.id, &after.name, &[], &[]);
        after_json["description"] = json!(after.description);
        NewEvent::new(tenant_id, Some(actor), AccessAction::TeamUpdated, None)
            .before(Some(before_json))
            .after(after_json)
            .write(&mut tx)
            .await?;
        tx.commit().await?;
        Ok(after)
    }

    /// The check's read (I4): `(member, Observed)` for every affected member × affected project,
    /// by member then project, as they stand now (before the change), except that for
    /// `AddMember` each one already holds the team's grant on its project (what the member
    /// would get), so the write's re-read also compares the team's grants.
    pub async fn team_change_observed(
        &self,
        tenant: TenantId,
        change: &TeamChange,
    ) -> Result<Vec<(Uuid, Observed)>, TeamWriteError> {
        let mut tx = self.pool.begin().await?;
        let plan = plan(&mut tx, tenant.as_uuid(), change).await?;
        let observed = observe(&mut tx, &plan, change).await?;
        tx.rollback().await?;
        Ok(observed)
    }

    /// Under the tenant lock, then the affected members' locks in id order (P6): re-reads
    /// `team_change_observed`; any difference from `expect` (the affected set included) is
    /// `Changed` with nothing written. Otherwise writes the change, its event (P9), then
    /// `revoke_unqualified_logins` for each affected member in id order, and commits.
    pub async fn write_team_checked(
        &self,
        actor: Uuid,
        tenant: TenantId,
        change: &TeamChange,
        expect: &[(Uuid, Observed)],
    ) -> Result<TeamWrite, TeamWriteError> {
        self.write_team_checked_skipping(actor, tenant, change, expect, &[])
            .await
    }

    /// [`Self::write_team_checked`] for a check that skipped `skipped_admins` because they were
    /// tenant admins (R17): under the locks, each of them that is affected must still be a
    /// tenant admin of `tenant` (not removed), else `Changed` (a demotion committed since
    /// the check read the flag; the write is checked again).
    pub async fn write_team_checked_skipping(
        &self,
        actor: Uuid,
        tenant: TenantId,
        change: &TeamChange,
        expect: &[(Uuid, Observed)],
        skipped_admins: &[Uuid],
    ) -> Result<TeamWrite, TeamWriteError> {
        let tenant_id = tenant.as_uuid();
        let mut tx = self.pool.begin().await?;
        if !lock_tenant(&mut tx, tenant_id).await? {
            return Err(TeamWriteError::NotFound);
        }
        let plan = plan(&mut tx, tenant_id, change).await?;
        let mut affected = Vec::with_capacity(plan.affected.len());
        for member in &plan.affected {
            let m = lock_member(&mut tx, *member)
                .await?
                .ok_or(TeamWriteError::MemberNotFound)?;
            let still_admin = m.tenant_id == tenant
                && m.tenant_admin
                && m.status != MemberStatus::Removed;
            if skipped_admins.contains(&m.id) && !still_admin {
                return Err(TeamWriteError::Changed);
            }
            affected.push((m.id, m.zitadel_user_id));
        }
        if observe(&mut tx, &plan, change).await? != expect {
            return Err(TeamWriteError::Changed);
        }
        let team = &plan.team;
        let unchanged = TeamWrite {
            changed: false,
            affected: affected.clone(),
            projects: plan.projects.clone(),
        };
        let event = match change {
            TeamChange::PutGrant { project, grant, .. } => {
                let after = GrantRow::new(grant.platform_role, &grant.pg_roles);
                let before = plan
                    .grants
                    .iter()
                    .find(|(p, _)| p == project)
                    .map(|(_, g)| g.clone());
                if before.as_ref() == Some(&after) {
                    return Ok(unchanged);
                }
                sqlx::query(
                    "INSERT INTO team_grants (team_id, project_id, platform_role, pg_roles, granted_by)
                     VALUES ($1, $2, $3, $4, $5)
                     ON CONFLICT (team_id, project_id) DO UPDATE
                     SET platform_role = EXCLUDED.platform_role, pg_roles = EXCLUDED.pg_roles,
                         granted_by = EXCLUDED.granted_by, updated_at = now()",
                )
                .bind(team.id)
                .bind(project)
                .bind(after.platform_role.as_str())
                .bind(&after.pg_roles)
                .bind(actor)
                .execute(&mut *tx)
                .await
                .map_err(team_write_error)?;
                NewEvent::new(tenant_id, Some(actor), AccessAction::TeamGrantPut, None)
                    .project(*project)
                    .before(before.map(|b| {
                        team_event_json(team.id, &team.name, &[(*project, b)], &plan.affected)
                    }))
                    .after(team_event_json(
                        team.id,
                        &team.name,
                        &[(*project, after)],
                        &plan.affected,
                    ))
            }
            TeamChange::DeleteGrant { project, .. } => {
                let row: Option<(String, Vec<String>)> = sqlx::query_as(
                    "DELETE FROM team_grants WHERE team_id = $1 AND project_id = $2
                     RETURNING platform_role, pg_roles",
                )
                .bind(team.id)
                .bind(project)
                .fetch_optional(&mut *tx)
                .await?;
                let Some((role, pg_roles)) = row else {
                    return Ok(unchanged);
                };
                let platform_role = chelabase_types::access::PlatformRole::parse(&role)
                    .ok_or_else(|| {
                        sqlx::Error::Decode(format!("invalid platform role {role:?}").into())
                    })?;
                let before = GrantRow {
                    platform_role,
                    pg_roles,
                };
                NewEvent::new(tenant_id, Some(actor), AccessAction::TeamGrantDeleted, None)
                    .project(*project)
                    .before(Some(team_event_json(
                        team.id,
                        &team.name,
                        &[(*project, before)],
                        &plan.affected,
                    )))
            }
            TeamChange::AddMember { member, .. } => {
                let added = sqlx::query(
                    "INSERT INTO team_members (team_id, member_id, added_by) VALUES ($1, $2, $3)
                     ON CONFLICT (team_id, member_id) DO NOTHING",
                )
                .bind(team.id)
                .bind(member)
                .bind(actor)
                .execute(&mut *tx)
                .await?
                .rows_affected();
                if added == 0 {
                    return Ok(unchanged);
                }
                NewEvent::new(
                    tenant_id,
                    Some(actor),
                    AccessAction::TeamMemberAdded,
                    Some(*member),
                )
                .after(team_event_json(
                    team.id,
                    &team.name,
                    &plan.grants,
                    &[*member],
                ))
            }
            TeamChange::RemoveMember { member, .. } => {
                let removed =
                    sqlx::query("DELETE FROM team_members WHERE team_id = $1 AND member_id = $2")
                        .bind(team.id)
                        .bind(member)
                        .execute(&mut *tx)
                        .await?
                        .rows_affected();
                if removed == 0 {
                    return Ok(unchanged);
                }
                NewEvent::new(
                    tenant_id,
                    Some(actor),
                    AccessAction::TeamMemberRemoved,
                    Some(*member),
                )
                .before(Some(team_event_json(
                    team.id,
                    &team.name,
                    &plan.grants,
                    &[*member],
                )))
            }
            TeamChange::Delete { .. } => {
                sqlx::query("DELETE FROM teams WHERE id = $1")
                    .bind(team.id)
                    .execute(&mut *tx)
                    .await?;
                NewEvent::new(tenant_id, Some(actor), AccessAction::TeamDeleted, None).before(Some(
                    team_event_json(team.id, &team.name, &plan.grants, &plan.affected),
                ))
            }
        };
        event.write(&mut tx).await?;
        if !matches!(change, TeamChange::Delete { .. }) {
            sqlx::query("UPDATE teams SET updated_at = now() WHERE id = $1")
                .bind(team.id)
                .execute(&mut *tx)
                .await?;
        }
        // MH-9: the change is written, so the predicate sees it.
        for member in &plan.affected {
            revoke_unqualified_logins(&mut tx, Some(actor), *member).await?;
        }
        tx.commit().await?;
        Ok(TeamWrite {
            changed: true,
            affected,
            projects: plan.projects,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chelabase_types::access::PlatformRole;

    fn id(n: u128) -> Uuid {
        Uuid::from_u128(n)
    }

    // Which members and projects each change reaches (the check's sets, I4/MH-4), over a team
    // with members 3 and 1 and grants on projects 20 and 10 (given unsorted, with a duplicate).
    #[test]
    fn team_change_affected_sets() {
        let team = id(100);
        let members = [id(3), id(1), id(3)];
        let projects = [id(20), id(10)];
        let grant = GrantRow {
            platform_role: PlatformRole::Developer,
            pg_roles: vec![],
        };
        let cases = [
            (
                "put grant on a new project",
                TeamChange::PutGrant {
                    team,
                    project: id(30),
                    grant: grant.clone(),
                },
                (vec![id(1), id(3)], vec![id(30)]),
            ),
            (
                "put grant on a granted project",
                TeamChange::PutGrant {
                    team,
                    project: id(10),
                    grant,
                },
                (vec![id(1), id(3)], vec![id(10)]),
            ),
            (
                "delete grant",
                TeamChange::DeleteGrant {
                    team,
                    project: id(20),
                },
                (vec![id(1), id(3)], vec![id(20)]),
            ),
            (
                "add member",
                TeamChange::AddMember {
                    team,
                    member: id(2),
                },
                (vec![id(2)], vec![id(10), id(20)]),
            ),
            (
                "remove member",
                TeamChange::RemoveMember {
                    team,
                    member: id(1),
                },
                (vec![id(1)], vec![id(10), id(20)]),
            ),
            (
                "delete team",
                TeamChange::Delete { team },
                (vec![id(1), id(3)], vec![id(10), id(20)]),
            ),
        ];
        for (label, change, want) in cases {
            assert_eq!(affected_sets(&change, &members, &projects), want, "{label}");
        }
        // A team without members or grants reaches nothing but what the change names.
        assert_eq!(
            affected_sets(&TeamChange::Delete { team }, &[], &[]),
            (vec![], vec![]),
            "an empty team"
        );
        assert_eq!(
            affected_sets(
                &TeamChange::AddMember {
                    team,
                    member: id(2)
                },
                &[],
                &[]
            ),
            (vec![id(2)], vec![]),
            "a member added to a team without grants"
        );
    }
}
