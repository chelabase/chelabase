//! Teams (members hub step 2, spec Part 5): a team holds per-project grants (a platform role and
//! Postgres roles) and its members get them live, as a second source of access beside their own
//! grant (D1).
//!
//! - Everyone in the tenant reads teams; a read names only the projects the caller can see (R1)
//!   and a team's Postgres roles on a project only to tenant admins and that project's Admins
//!   (MH-34). Tenant admins write them.
//! - Every team write is checked on the union of each affected member's sources, narrowed by
//!   their override, on every database of every affected project (MH-4), with the metadata's
//!   re-read under the tenant and member locks as its guard (I4); a refusal names the member,
//!   the team and the database.
//! - A write that changed something fires the grant writes' side effects for every affected
//!   member and project (MH-5, [`members::access_changed_for`]).

use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use chelabase_metadata::members::{GrantRow, MemberStatus, Observed};
use chelabase_metadata::teams::{TeamChange, TeamDetail, TeamRecord, TeamWrite, TeamWriteError};
use chelabase_routes::orchestrator as ep;
use chelabase_types::access::PlatformRole;
use chelabase_types::api_error::{ApiError, ErrorCode, ErrorEnvelope};
use chelabase_types::{ApiJson, ApiPath, ProjectId, TenantId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tracing::info;
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

use crate::access::{Target, authorize, authorize_project};
use crate::members::{self, CHECKED_WRITE_TRIES, RoleContext, actor, internal, tenant_member, unprocessable};
use crate::{AppState, AuthPrincipal, JobPort, MetadataPort};

pub(crate) fn routes<M, J>() -> OpenApiRouter<Arc<AppState<M, J>>>
where
    M: MetadataPort,
    J: JobPort,
{
    OpenApiRouter::new()
        .routes(routes!(list_teams, create_team))
        .routes(routes!(get_team, update_team, delete_team))
        .routes(routes!(put_team_grant, delete_team_grant))
        .routes(routes!(put_team_member, delete_team_member))
        .routes(routes!(list_member_teams))
        .routes(routes!(list_project_team_grants))
}

// --- Shapes ------------------------------------------------------------------------------------

/// The longest team name and description, in characters (P2).
const NAME_MAX: usize = 80;
const DESCRIPTION_MAX: usize = 500;

/// A team of the tenant.
#[derive(Debug, Serialize, ToSchema)]
pub(crate) struct Team {
    id: Uuid,
    /// Trimmed, 1 to 80 characters, unique in the tenant ignoring case.
    name: String,
    /// At most 500 characters; empty when it has none.
    description: String,
    member_count: usize,
    /// The members' ids, by id. In `getTeam`, the writes' answers and `listMemberTeams`; absent
    /// from `listTeams`.
    #[serde(skip_serializing_if = "Option::is_none")]
    member_ids: Option<Vec<Uuid>>,
    /// The team's grants on the projects the caller can see, by project id.
    projects: Vec<TeamProjectAccess>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

/// What a team gives its members on one project.
#[derive(Debug, Serialize, ToSchema)]
pub(crate) struct TeamProjectAccess {
    project_id: ProjectId,
    platform_role: PlatformRole,
    /// The Postgres roles, sorted (none for `admin`, which holds every role); `null` unless
    /// the caller is a tenant admin or an Admin of the project.
    #[schema(required = true)]
    pg_roles: Option<Vec<String>>,
}

/// One team's grant on a project, with the team's members (`listProjectTeamGrants`).
#[derive(Debug, Serialize, ToSchema)]
pub(crate) struct ProjectTeamGrantView {
    team_id: Uuid,
    name: String,
    platform_role: PlatformRole,
    /// Sorted; none for `admin`, which holds every role.
    pg_roles: Vec<String>,
    /// By id.
    member_ids: Vec<Uuid>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub(crate) struct CreateTeamRequest {
    /// Trimmed; 1 to 80 characters, unique in the tenant ignoring case.
    name: String,
    /// At most 500 characters. Default empty.
    #[serde(default)]
    description: String,
}

#[derive(Debug, Deserialize, ToSchema)]
pub(crate) struct UpdateTeamRequest {
    /// Trimmed; 1 to 80 characters, unique in the tenant ignoring case. Left out: unchanged.
    #[serde(default)]
    name: Option<String>,
    /// At most 500 characters. Left out: unchanged.
    #[serde(default)]
    description: Option<String>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub(crate) struct TeamGrantRequest {
    platform_role: PlatformRole,
    /// Postgres role names; for `admin`, which holds every role (R17), checked and dropped.
    /// Never `owner` or the database's owner, a reserved name or a personal login. Below admin,
    /// only NOLOGIN groups whose memberships reach no owner-level, LOGIN or reserved role, for
    /// every member of the team on every database (with their own grant and overrides).
    #[serde(default)]
    pg_roles: Vec<String>,
}

// --- Helpers -----------------------------------------------------------------------------------

/// What the caller may see of a team (R1, P11, MH-34): the projects it can see, and those it is
/// an Admin of (a tenant admin: every project of the tenant, for both).
struct Viewer {
    tenant_admin: bool,
    visible: HashSet<Uuid>,
    admin_of: HashSet<Uuid>,
}

impl Viewer {
    async fn of<M, J>(state: &AppState<M, J>, principal: &AuthPrincipal) -> Result<Self, ApiError>
    where
        M: MetadataPort,
    {
        let Some(caller) = principal.member.as_ref() else {
            return Err(internal("teams", "allowed a caller with no member row"));
        };
        if caller.tenant_admin {
            let projects = state
                .metadata
                .list_projects(principal.tenant_id)
                .await
                .map_err(|err| internal("teams: list projects", err))?;
            let all: HashSet<Uuid> = projects.into_iter().map(|p| p.id.as_uuid()).collect();
            return Ok(Self { tenant_admin: true, admin_of: all.clone(), visible: all });
        }
        // R1: every access source counts (T2-4); MH-34: an Admin from any source sees the roles.
        let sources = state
            .metadata
            .member_grant_sources(principal.tenant_id, caller.id)
            .await
            .map_err(|err| internal("teams: caller projects", err))?;
        let mut visible = HashSet::new();
        let mut admin_of = HashSet::new();
        for (project, sources) in sources {
            let Some(effective) = sources.effective() else {
                continue;
            };
            visible.insert(project);
            if effective.platform_role == PlatformRole::Admin {
                admin_of.insert(project);
            }
        }
        Ok(Self { tenant_admin: false, visible, admin_of })
    }

    /// The team as this caller sees it; `members` adds the member ids.
    fn team(&self, detail: TeamDetail, members: bool) -> Team {
        let projects = detail
            .grants
            .into_iter()
            .filter(|(p, _)| self.visible.contains(p))
            .map(|(p, g)| TeamProjectAccess {
                project_id: ProjectId::from(p),
                platform_role: g.platform_role,
                pg_roles: (self.tenant_admin || self.admin_of.contains(&p)).then(|| sorted(g.pg_roles)),
            })
            .collect();
        Team {
            id: detail.team.id,
            name: detail.team.name,
            description: detail.team.description,
            member_count: detail.member_ids.len(),
            member_ids: members.then_some(detail.member_ids),
            projects,
            created_at: detail.team.created_at,
            updated_at: detail.team.updated_at,
        }
    }
}

fn sorted(mut roles: Vec<String>) -> Vec<String> {
    roles.sort();
    roles.dedup();
    roles
}

/// P2: a name trimmed, 1 to 80 characters (422 otherwise, checked before the store).
fn team_name(raw: &str) -> Result<&str, ApiError> {
    let name = raw.trim();
    if name.is_empty() || name.chars().count() > NAME_MAX {
        return Err(unprocessable(format!("a team name has 1 to {NAME_MAX} characters")));
    }
    Ok(name)
}

/// P2: a description at most 500 characters.
fn team_description(raw: &str) -> Result<&str, ApiError> {
    if raw.chars().count() > DESCRIPTION_MAX {
        return Err(unprocessable(format!("a team description has at most {DESCRIPTION_MAX} characters")));
    }
    Ok(raw)
}

/// A refused team write's answer (P17).
fn team_failed(what: &str, err: TeamWriteError) -> ApiError {
    match err {
        TeamWriteError::NotFound => ApiError::not_found("team not found"),
        TeamWriteError::ProjectNotFound => ApiError::not_found("project not found"),
        TeamWriteError::MemberNotFound => ApiError::not_found("member not found"),
        TeamWriteError::MemberPending => pending(),
        TeamWriteError::NameTaken => {
            ApiError::new(409, ErrorCode::NameTaken, "a team of the tenant already has this name")
        }
        TeamWriteError::Changed => ApiError::new(
            409,
            ErrorCode::Conflict,
            "the team's members or their access changed while this was being checked; try again",
        ),
        // R7 and the bounds, which the handlers check first (T1 audit: 422).
        TeamWriteError::Conflict(message) => unprocessable(message),
        TeamWriteError::Db(err) => internal(what, err),
    }
}

fn pending() -> ApiError {
    ApiError::new(
        409,
        ErrorCode::Conflict,
        "this person's sign-up is waiting for approval: approve the sign-up first",
    )
}

/// The team of the caller's tenant, or 404.
async fn load_team<M, J>(state: &AppState<M, J>, tenant: TenantId, id: Uuid) -> Result<TeamDetail, ApiError>
where
    M: MetadataPort,
{
    match state.metadata.team(tenant, id).await {
        Ok(Some(team)) => Ok(team),
        Ok(None) => Err(ApiError::not_found("team not found")),
        Err(err) => Err(internal("load team", err)),
    }
}

/// A member a team write names: of the tenant and not removed (404), and for an addition not
/// waiting for approval (409, P17).
async fn team_member<M, J>(state: &AppState<M, J>, tenant: TenantId, id: Uuid, adding: bool) -> Result<(), ApiError>
where
    M: MetadataPort,
{
    let m = tenant_member(state, tenant, id).await?;
    match m.status {
        MemberStatus::Removed => Err(ApiError::not_found("member not found")),
        MemberStatus::Pending if adding => Err(pending()),
        _ => Ok(()),
    }
}

/// A team write through its check (MH-4, P7, I4): reads every affected member × project as it
/// stands, checks the state the change leaves on every database of each project (one role
/// context per project, `contexts` may hold some for the first try), and writes only while
/// that read still holds; a changed read is checked again, with the role graphs read again
/// too, then 409. A tenant admin holds every role (R17), so it is never checked; the
/// write refuses (`Changed`) when one it skipped is no longer a tenant admin. A grant put on
/// a team nobody is checked for (an empty team) is checked on its own, as an own grant is. A
/// change that wrote fires the grant writes' side effects for every member and project it
/// reached (MH-5).
async fn checked_team_write<M, J>(
    state: &AppState<M, J>,
    principal: &AuthPrincipal,
    change: &TeamChange,
    mut contexts: HashMap<Uuid, RoleContext>,
) -> Result<TeamWrite, ApiError>
where
    M: MetadataPort,
    J: JobPort,
{
    let actor = actor(principal.member.as_ref())?;
    let tenant = principal.tenant_id;
    let team = change.team();
    let mut tries = 0;
    let write = loop {
        tries += 1;
        if tries > 1 {
            // I4: the retry checks the role graphs as they are now, not as the first try read
            // them; a grant's names are checked again on the fresh context.
            contexts.clear();
            if let TeamChange::PutGrant { project, grant, .. } = change {
                let context = RoleContext::project(state, ProjectId::from(*project)).await?;
                context
                    .check_names(&grant.pg_roles)
                    .map_err(|err| err.with_extra("team_id", json!(team)))?;
                contexts.insert(*project, context);
            }
        }
        let observed = state
            .metadata
            .team_change_observed(tenant, change)
            .await
            .map_err(|err| team_failed("team write: read", err))?;
        // Defect 1: an empty team has no union to check, but its grant must still be one a
        // member could hold on its own.
        if let TeamChange::PutGrant { project, grant, .. } = change
            && observed.is_empty()
        {
            let alone = Observed {
                project: *project,
                teams: BTreeMap::from([(team, grant.clone())]),
                ..Observed::default()
            };
            let context = match contexts.entry(*project) {
                Entry::Occupied(e) => e.into_mut(),
                Entry::Vacant(e) => e.insert(RoleContext::project(state, ProjectId::from(*project)).await?),
            };
            context.check_effective(&alone, None).map_err(|err| {
                err.with_extra("team_id", json!(team)).with_extra("project_id", json!(project))
            })?;
        }
        let mut tenant_admins: HashMap<Uuid, bool> = HashMap::new();
        for (member, now) in &observed {
            // R17: a tenant admin is Admin on every project, whatever its sources say.
            let tenant_admin = match tenant_admins.entry(*member) {
                Entry::Occupied(e) => *e.get(),
                Entry::Vacant(e) => *e.insert(is_tenant_admin(state, tenant, *member).await?),
            };
            if tenant_admin {
                continue;
            }
            let project = now.project;
            // The state the change leaves. For `AddMember` the read already holds the team's
            // grant on the project (what the member gets; the write compares it, I4).
            let after = match change {
                TeamChange::PutGrant { grant, .. } => now.with_team(team, Some(grant.clone())),
                TeamChange::AddMember { .. } => now.clone(),
                TeamChange::DeleteGrant { .. } | TeamChange::RemoveMember { .. } | TeamChange::Delete { .. } => {
                    now.with_team(team, None)
                }
            };
            let context = match contexts.entry(project) {
                Entry::Occupied(e) => e.into_mut(),
                Entry::Vacant(e) => e.insert(RoleContext::project(state, ProjectId::from(project)).await?),
            };
            context.check_effective(&after, None).map_err(|err| {
                err.with_extra("member_id", json!(member))
                    .with_extra("team_id", json!(team))
                    .with_extra("project_id", json!(project))
            })?;
        }
        // R17 race: the flags were read before the lock; the write re-reads them under it, so
        // a demotion committed since is `Changed` and checked again.
        let skipped: Vec<Uuid> = tenant_admins.iter().filter(|(_, admin)| **admin).map(|(m, _)| *m).collect();
        match state.metadata.write_team_checked(actor, tenant, change, &observed, &skipped).await {
            Err(TeamWriteError::Changed) if tries < CHECKED_WRITE_TRIES => continue,
            other => break other.map_err(|err| team_failed("team write", err))?,
        }
    };
    if write.changed {
        info!(request_id = %principal.request_id, team_id = %team, members = write.affected.len(), "team: written");
        members::access_changed_for(state, tenant, &write.affected, write.projects.clone()).await;
    }
    Ok(write)
}

/// Whether `member` is a tenant admin of `tenant` now (R17: Admin on every project). A member
/// that is gone answers no: the write's re-read refuses it.
async fn is_tenant_admin<M, J>(state: &AppState<M, J>, tenant: TenantId, member: Uuid) -> Result<bool, ApiError>
where
    M: MetadataPort,
{
    match state.metadata.member(member).await {
        Ok(Some(m)) => Ok(m.tenant_id == tenant && m.tenant_admin && m.status != MemberStatus::Removed),
        Ok(None) => Ok(false),
        Err(err) => Err(internal("team write: load member", err)),
    }
}

/// The team as the caller sees it after a write (with its members).
async fn team_answer<M, J>(state: &AppState<M, J>, principal: &AuthPrincipal, id: Uuid) -> Result<Team, ApiError>
where
    M: MetadataPort,
{
    let detail = load_team(state, principal.tenant_id, id).await?;
    Ok(Viewer::of(state, principal).await?.team(detail, true))
}

/// A new or renamed team's answer: its record, with the members and grants read now.
async fn record_answer<M, J>(
    state: &AppState<M, J>,
    principal: &AuthPrincipal,
    record: TeamRecord,
) -> Result<Team, ApiError>
where
    M: MetadataPort,
{
    let mut team = team_answer(state, principal, record.id).await?;
    team.name = record.name;
    team.description = record.description;
    team.updated_at = record.updated_at;
    Ok(team)
}

// --- Handlers ----------------------------------------------------------------------------------

/// Unwraps a handler step, answering its error.
macro_rules! tri {
    ($e:expr) => {
        match $e {
            Ok(v) => v,
            Err(err) => return err.into_response(),
        }
    };
}

#[utoipa::path(
    get,
    path = "/v1/teams",
    operation_id = "listTeams",
    tag = "teams",
    summary = "List the tenant's teams",
    description = "Every team of the tenant, by name ignoring case, with its member count and its grants on the projects the caller can see (a tenant admin sees all; anyone else the projects where they have any access). A team's Postgres roles on a project are shown only to tenant admins and to Admins of that project; others get `pg_roles: null`. No member ids (`getTeam` has them).",
    responses(
        (status = 200, description = "The teams", body = Vec<Team>),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Chelabase tenant", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
    ),
)]
async fn list_teams<M, J>(State(state): State<Arc<AppState<M, J>>>, principal: AuthPrincipal) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    tri!(authorize(&state, &principal, ep::LIST_TEAMS.policy.access, Target::None).await);
    let teams = tri!(
        state
            .metadata
            .list_teams(principal.tenant_id)
            .await
            .map_err(|err| internal("list teams", err))
    );
    let viewer = tri!(Viewer::of(&state, &principal).await);
    let out: Vec<Team> = teams.into_iter().map(|t| viewer.team(t, false)).collect();
    Json(out).into_response()
}

#[utoipa::path(
    post,
    path = "/v1/teams",
    operation_id = "createTeam",
    tag = "teams",
    summary = "Create a team",
    description = "An empty team: no members, no grants. One `team_created` access event.",
    request_body = CreateTeamRequest,
    responses(
        (status = 201, description = "The team", body = Team),
        (status = 400, description = "`invalid_request`: the body isn't well-formed JSON", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Chelabase tenant", body = ErrorEnvelope),
        (status = 409, description = "`name_taken`: a team of the tenant has this name (ignoring case)", body = ErrorEnvelope),
        (status = 413, description = "`too_large`: the body is over the 2 MB limit", body = ErrorEnvelope),
        (status = 415, description = "`invalid_request`: the request isn't `Content-Type: application/json`", body = ErrorEnvelope),
        (status = 422, description = "`invalid_request`: the name is empty or over 80 characters once trimmed, or the description is over 500 characters", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
    ),
)]
async fn create_team<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiJson(body): ApiJson<CreateTeamRequest>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    tri!(authorize(&state, &principal, ep::CREATE_TEAM.policy.access, Target::None).await);
    let actor = tri!(actor(principal.member.as_ref()));
    let name = tri!(team_name(&body.name));
    let description = tri!(team_description(&body.description));
    let record = tri!(
        state
            .metadata
            .create_team(actor, principal.tenant_id, name, description)
            .await
            .map_err(|err| team_failed("create team", err))
    );
    info!(request_id = %principal.request_id, team_id = %record.id, "team: created");
    // An empty team: nothing to read back.
    let team = Team {
        id: record.id,
        name: record.name,
        description: record.description,
        member_count: 0,
        member_ids: Some(Vec::new()),
        projects: Vec::new(),
        created_at: record.created_at,
        updated_at: record.updated_at,
    };
    (StatusCode::CREATED, Json(team)).into_response()
}

#[utoipa::path(
    get,
    path = "/v1/teams/{team_id}",
    operation_id = "getTeam",
    tag = "teams",
    summary = "Read a team",
    description = "The team with its member ids, filtered as `listTeams` is.",
    params(("team_id" = Uuid, Path, description = "Team id")),
    responses(
        (status = 200, description = "The team", body = Team),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Chelabase tenant", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: no such team (another tenant's answers the same)", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
    ),
)]
async fn get_team<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath(id): ApiPath<Uuid>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    tri!(authorize(&state, &principal, ep::GET_TEAM.policy.access, Target::None).await);
    Json(tri!(team_answer(&state, &principal, id).await)).into_response()
}

#[utoipa::path(
    patch,
    path = "/v1/teams/{team_id}",
    operation_id = "updateTeam",
    tag = "teams",
    summary = "Rename a team or change its description",
    description = "A field left out is unchanged; an unchanged team writes nothing. One `team_updated` access event.",
    params(("team_id" = Uuid, Path, description = "Team id")),
    request_body = UpdateTeamRequest,
    responses(
        (status = 200, description = "The team", body = Team),
        (status = 400, description = "`invalid_request`: the body isn't well-formed JSON, or a path id isn't a valid UUID", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Chelabase tenant", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: no such team (another tenant's answers the same)", body = ErrorEnvelope),
        (status = 409, description = "`name_taken`: another team of the tenant has this name (ignoring case)", body = ErrorEnvelope),
        (status = 413, description = "`too_large`: the body is over the 2 MB limit", body = ErrorEnvelope),
        (status = 415, description = "`invalid_request`: the request isn't `Content-Type: application/json`", body = ErrorEnvelope),
        (status = 422, description = "`invalid_request`: the name is empty or over 80 characters once trimmed, or the description is over 500 characters", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
    ),
)]
async fn update_team<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath(id): ApiPath<Uuid>,
    ApiJson(body): ApiJson<UpdateTeamRequest>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    tri!(authorize(&state, &principal, ep::UPDATE_TEAM.policy.access, Target::None).await);
    let actor = tri!(actor(principal.member.as_ref()));
    let name = tri!(body.name.as_deref().map(team_name).transpose());
    let description = tri!(body.description.as_deref().map(team_description).transpose());
    let record = tri!(
        state
            .metadata
            .update_team(actor, principal.tenant_id, id, name, description)
            .await
            .map_err(|err| team_failed("update team", err))
    );
    info!(request_id = %principal.request_id, team_id = %id, "team: updated");
    Json(tri!(record_answer(&state, &principal, record).await)).into_response()
}

#[utoipa::path(
    delete,
    path = "/v1/teams/{team_id}",
    operation_id = "deleteTeam",
    tag = "teams",
    summary = "Delete a team",
    description = "Its members lose what it gave them (their own grants stay), checked first on each member's remaining access as any team write is. Their personal logins that stop qualifying are revoked, and every branch of the team's projects reconciles its member roles. One `team_deleted` access event naming the members it reached.",
    params(("team_id" = Uuid, Path, description = "Team id")),
    responses(
        (status = 204, description = "Deleted"),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Chelabase tenant", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: no such team (another tenant's answers the same)", body = ErrorEnvelope),
        (status = 409, description = "`conflict`: the team's members or grants kept changing while this was being checked; try again", body = ErrorEnvelope),
        (status = 422, description = "`invalid_request`: a member would be left holding a role below admin that can be held only with the admin platform role (`role`, `member_id`, `team_id`, `database_id`; `chain`, `branch_id`, `branch` and `branch_main` too when its memberships on a branch are why)", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
    ),
)]
async fn delete_team<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath(id): ApiPath<Uuid>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    tri!(authorize(&state, &principal, ep::DELETE_TEAM.policy.access, Target::None).await);
    tri!(load_team(&state, principal.tenant_id, id).await);
    let change = TeamChange::Delete { team: id };
    tri!(checked_team_write(&state, &principal, &change, HashMap::new()).await);
    StatusCode::NO_CONTENT.into_response()
}

#[utoipa::path(
    put,
    path = "/v1/teams/{team_id}/grants/{project_id}",
    operation_id = "putTeamGrant",
    tag = "teams",
    summary = "Give a team access to a project",
    description = "Creates or replaces the team's grant on the project; its members get it live, beside their own grants (each Postgres role at the level of the grant naming it). `admin` holds every role (the names in `pg_roles` are checked, then dropped). The grant is checked on every member's union of sources narrowed by their overrides, on every database of the project, as `putProjectGrant` checks one grant; a refusal names the member, the team and the database. Personal logins that stop qualifying are revoked, and every branch of the project reconciles its member roles. An unchanged grant writes nothing.",
    params(
        ("team_id" = Uuid, Path, description = "Team id"),
        ("project_id" = Uuid, Path, description = "Project id"),
    ),
    request_body = TeamGrantRequest,
    responses(
        (status = 200, description = "The team", body = Team),
        (status = 400, description = "`invalid_request`: the body isn't well-formed JSON, or a path id isn't a valid UUID", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Chelabase tenant", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: no such team or project (another tenant's answers the same)", body = ErrorEnvelope),
        (status = 409, description = "`project_deleting`: the project is being deleted; `conflict`: the team's members kept changing while this was being checked; try again", body = ErrorEnvelope),
        (status = 413, description = "`too_large`: the body is over the 2 MB limit", body = ErrorEnvelope),
        (status = 415, description = "`invalid_request`: the request isn't `Content-Type: application/json`", body = ErrorEnvelope),
        (status = 422, description = "`invalid_request`: a role can't be granted (`role` names it; `member_id`, `team_id`, `database_id`, `chain`, `branch_id`, `branch` and `branch_main` too when a member's union on a branch is why)", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
    ),
)]
async fn put_team_grant<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath((id, project_id)): ApiPath<(Uuid, Uuid)>,
    ApiJson(body): ApiJson<TeamGrantRequest>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    tri!(authorize(&state, &principal, ep::PUT_TEAM_GRANT.policy.access, Target::None).await);
    tri!(load_team(&state, principal.tenant_id, id).await);
    match state.metadata.get_project(principal.tenant_id, ProjectId::from(project_id)).await {
        Ok(Some(p)) if p.status == chelabase_metadata::ProjectStatus::Active => {}
        Ok(Some(p)) if p.status != chelabase_metadata::ProjectStatus::Deleted => {
            return ApiError::new(409, ErrorCode::ProjectDeleting, "project is being deleted").into_response();
        }
        Ok(_) => return ApiError::not_found("project not found").into_response(),
        Err(err) => return internal("team grant: load project", err).into_response(),
    }
    let context = tri!(RoleContext::project(&state, ProjectId::from(project_id)).await);
    // R7, MH-21, S12: names no grant may hold, at any level; R17: then an Admin grant is every
    // role on the project.
    tri!(context.check_names(&body.pg_roles).map_err(|err| err.with_extra("team_id", json!(id))));
    let grant = GrantRow::new(body.platform_role, &body.pg_roles);
    let change = TeamChange::PutGrant { team: id, project: project_id, grant };
    let contexts = HashMap::from([(project_id, context)]);
    tri!(checked_team_write(&state, &principal, &change, contexts).await);
    Json(tri!(team_answer(&state, &principal, id).await)).into_response()
}

#[utoipa::path(
    delete,
    path = "/v1/teams/{team_id}/grants/{project_id}",
    operation_id = "deleteTeamGrant",
    tag = "teams",
    summary = "Take a team's access to a project away",
    description = "Its members lose what the team gave them there, checked first on each member's remaining access. Also 204 when the team had no grant there (nothing is written).",
    params(
        ("team_id" = Uuid, Path, description = "Team id"),
        ("project_id" = Uuid, Path, description = "Project id"),
    ),
    responses(
        (status = 204, description = "Deleted, or there was none"),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Chelabase tenant", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: no such team or project (another tenant's answers the same)", body = ErrorEnvelope),
        (status = 409, description = "`conflict`: the team's members kept changing while this was being checked; try again", body = ErrorEnvelope),
        (status = 422, description = "`invalid_request`: a member would be left holding a role that can be held only with the admin platform role (`role`, `member_id`, `team_id`, `database_id`, …)", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
    ),
)]
async fn delete_team_grant<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath((id, project_id)): ApiPath<(Uuid, Uuid)>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    tri!(authorize(&state, &principal, ep::DELETE_TEAM_GRANT.policy.access, Target::None).await);
    tri!(load_team(&state, principal.tenant_id, id).await);
    let change = TeamChange::DeleteGrant { team: id, project: project_id };
    tri!(checked_team_write(&state, &principal, &change, HashMap::new()).await);
    StatusCode::NO_CONTENT.into_response()
}

#[utoipa::path(
    put,
    path = "/v1/teams/{team_id}/members/{member_id}",
    operation_id = "putTeamMember",
    tag = "teams",
    summary = "Add a member to a team",
    description = "The member gets the team's grants live, checked first on their union on every database of the team's projects. Also 200 when they are already in (nothing is written).",
    params(
        ("team_id" = Uuid, Path, description = "Team id"),
        ("member_id" = Uuid, Path, description = "Member id"),
    ),
    responses(
        (status = 200, description = "The team", body = Team),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Chelabase tenant", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: no such team or member (another tenant's, or a removed member, answers the same)", body = ErrorEnvelope),
        (status = 409, description = "`conflict`: the member's sign-up waits for approval (approve it first), or their access kept changing while this was being checked", body = ErrorEnvelope),
        (status = 422, description = "`invalid_request`: the member would hold a role that can be held only with the admin platform role (`role`, `member_id`, `team_id`, `database_id`, …)", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
    ),
)]
async fn put_team_member<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath((id, member_id)): ApiPath<(Uuid, Uuid)>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    tri!(authorize(&state, &principal, ep::PUT_TEAM_MEMBER.policy.access, Target::None).await);
    tri!(load_team(&state, principal.tenant_id, id).await);
    tri!(team_member(&state, principal.tenant_id, member_id, true).await);
    let change = TeamChange::AddMember { team: id, member: member_id };
    tri!(checked_team_write(&state, &principal, &change, HashMap::new()).await);
    Json(tri!(team_answer(&state, &principal, id).await)).into_response()
}

#[utoipa::path(
    delete,
    path = "/v1/teams/{team_id}/members/{member_id}",
    operation_id = "deleteTeamMember",
    tag = "teams",
    summary = "Remove a member from a team",
    description = "The member loses what the team gave them, checked first on their remaining access. Also 204 when they weren't in (nothing is written).",
    params(
        ("team_id" = Uuid, Path, description = "Team id"),
        ("member_id" = Uuid, Path, description = "Member id"),
    ),
    responses(
        (status = 204, description = "Removed, or wasn't in"),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Chelabase tenant", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: no such team or member (another tenant's, or a removed member, answers the same)", body = ErrorEnvelope),
        (status = 409, description = "`conflict`: their access kept changing while this was being checked; try again", body = ErrorEnvelope),
        (status = 422, description = "`invalid_request`: the member would be left holding a role that can be held only with the admin platform role (`role`, `member_id`, `team_id`, `database_id`, …)", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
    ),
)]
async fn delete_team_member<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath((id, member_id)): ApiPath<(Uuid, Uuid)>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    tri!(authorize(&state, &principal, ep::DELETE_TEAM_MEMBER.policy.access, Target::None).await);
    tri!(load_team(&state, principal.tenant_id, id).await);
    tri!(team_member(&state, principal.tenant_id, member_id, false).await);
    let change = TeamChange::RemoveMember { team: id, member: member_id };
    tri!(checked_team_write(&state, &principal, &change, HashMap::new()).await);
    StatusCode::NO_CONTENT.into_response()
}

#[utoipa::path(
    get,
    path = "/v1/members/{member_id}/teams",
    operation_id = "listMemberTeams",
    tag = "teams",
    summary = "List a member's teams",
    description = "The member's teams, by name ignoring case, with their member ids, filtered as `listTeams` is: grants only on the projects the caller can see, Postgres roles only for tenant admins and the project's Admins.",
    params(("member_id" = Uuid, Path, description = "Member id")),
    responses(
        (status = 200, description = "The member's teams", body = Vec<Team>),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Chelabase tenant", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: no such member (another tenant's answers the same)", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
    ),
)]
async fn list_member_teams<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath(member_id): ApiPath<Uuid>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    tri!(authorize(&state, &principal, ep::LIST_MEMBER_TEAMS.policy.access, Target::None).await);
    tri!(tenant_member(&state, principal.tenant_id, member_id).await);
    let teams = tri!(
        state
            .metadata
            .member_teams(member_id)
            .await
            .map_err(|err| internal("member teams", err))
    );
    let viewer = tri!(Viewer::of(&state, &principal).await);
    let out: Vec<Team> = teams.into_iter().map(|t| viewer.team(t, true)).collect();
    Json(out).into_response()
}

#[utoipa::path(
    get,
    path = "/v1/projects/{id}/team-grants",
    operation_id = "listProjectTeamGrants",
    tag = "teams",
    summary = "List a project's team grants",
    description = "Every team's grant on the project, by team name ignoring case, with the team's members (`listProjectGrants` lists the members' own grants).",
    params(("id" = Uuid, Path, description = "Project id")),
    responses(
        (status = 200, description = "The team grants", body = Vec<ProjectTeamGrantView>),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Chelabase tenant", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: no such project (another tenant's answers the same)", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
    ),
)]
async fn list_project_team_grants<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath(id): ApiPath<Uuid>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    tri!(authorize_project(&state, &principal, ep::LIST_PROJECT_TEAM_GRANTS.policy.access, id).await);
    let grants = tri!(
        state
            .metadata
            .project_team_grants(id)
            .await
            .map_err(|err| internal("project team grants", err))
    );
    let out: Vec<ProjectTeamGrantView> = grants
        .into_iter()
        .map(|g| ProjectTeamGrantView {
            team_id: g.team_id,
            name: g.name,
            platform_role: g.grant.platform_role,
            pg_roles: sorted(g.grant.pg_roles),
            member_ids: g.member_ids,
        })
        .collect();
    Json(out).into_response()
}
