//! Who may call what: every route's access check (members and roles, parts 2 and 4).
//!
//! [`authorize`] is the one way a handler reaches a project or database for a caller. It
//! resolves the target (project ← database ← branch), checks that it belongs to the caller's
//! tenant, reads the caller's grant rows (no cache: a revoke counts on the next request),
//! computes the effective grant and asks [`decide`]. Each handler passes its own endpoint's
//! `policy.access`, so `chelabase_routes` stays the single table of who may call what.

use chelabase_metadata::members::{GrantRow, MemberRecord, MemberStatus, OverrideRow};
use std::collections::HashSet;
use std::sync::Arc;

use chelabase_metadata::{DatabaseRecord, DatabaseStatus, ProjectRecord, RoleLevel, RoleRow};
use chelabase_types::access::{
    Access, CallerFacts, Deny, Grant, MemberFacts, Narrowing, PgRole, PlatformRole, Requested,
    RoleSet, act_as_reserved, decide, effective, tenant_admin_grant,
};
use chelabase_types::api_error::{ApiError, ErrorCode};
use chelabase_types::{BranchId, DatabaseId, ProjectId};
use tracing::error;
use uuid::Uuid;

use crate::{AppState, AuthPrincipal, JobPort, MetadataPort, roles};

/// What a route acts on, as far as its path (or body) names it.
#[derive(Clone, Copy, Debug)]
pub enum Target<'a> {
    /// Nothing below the tenant (project list and create, members, instance routes).
    None,
    Project(Uuid),
    /// A Chelabase database.
    Database(Uuid),
    /// A branch of a database. Its protection is read for `Branch` routes (for merges, pass
    /// the branch merged into).
    Branch {
        database: Uuid,
        branch: Uuid,
    },
    /// A Postgres database on a branch; `""` is the database's main one. Checked as the branch
    /// is: grants have no Postgres-database level (R17), so the name only documents the route.
    PgDatabase {
        database: Uuid,
        branch: Uuid,
        pg_database: &'a str,
    },
    /// Acting as a Postgres role in a Postgres database on a branch (`ActAs` routes); `""` is
    /// the main database. The role is checked against the Chelabase-database grant (R17), and
    /// an owner-level role needs Admin (R16).
    Role {
        database: Uuid,
        branch: Uuid,
        pg_database: &'a str,
        role: Requested<'a>,
    },
}

impl<'a> Target<'a> {
    fn database(&self) -> Option<Uuid> {
        match *self {
            Target::None | Target::Project(_) => None,
            Target::Database(database)
            | Target::Branch { database, .. }
            | Target::PgDatabase { database, .. }
            | Target::Role { database, .. } => Some(database),
        }
    }

    fn branch(&self) -> Option<Uuid> {
        match *self {
            Target::Branch { branch, .. }
            | Target::PgDatabase { branch, .. }
            | Target::Role { branch, .. } => Some(branch),
            _ => None,
        }
    }

    fn requested(&self) -> Option<Requested<'a>> {
        match *self {
            Target::Role { role, .. } => Some(role),
            _ => None,
        }
    }
}

/// What [`authorize`] resolved and allowed.
#[derive(Clone, Debug)]
pub struct Authorized {
    /// The database, for a database, branch, Postgres-database or role target.
    pub record: Option<DatabaseRecord>,
    /// The project row, for a project target.
    pub project: Option<ProjectRecord>,
    pub project_id: Option<Uuid>,
    /// The caller's effective grant on the target (a tenant admin's is
    /// [`tenant_admin_grant`]); `None` below a project or without one.
    pub grant: Option<Grant>,
    /// The target branch's protection, read for `Branch` routes; `None` when not read or the
    /// branch isn't the database's.
    pub branch_protected: Option<bool>,
    /// The branch's roles when the check already read them (M3): an act-as reads them once and
    /// the handler takes them from here. See [`Authorized::role_graph`].
    pub roles: Option<LoadedRoles>,
}

/// A branch's role graph as [`authorize`] loaded it (`graph` is `None` for a branch with no
/// endpoint to read).
#[derive(Clone, Debug)]
pub struct LoadedRoles {
    pub branch: Uuid,
    pub graph: Option<Arc<RoleGraph>>,
}

impl Authorized {
    /// The roles of `branch`: the ones the access check read when it was for this branch, else
    /// read now.
    pub async fn role_graph<M: MetadataPort, J: JobPort>(
        &self,
        state: &AppState<M, J>,
        record: &DatabaseRecord,
        branch: Uuid,
    ) -> Result<Option<Arc<RoleGraph>>, ApiError> {
        if let Some(loaded) = self.roles.as_ref().filter(|l| l.branch == branch) {
            return Ok(loaded.graph.clone());
        }
        Ok(RoleGraph::load(state, record, Some(branch)).await?.map(Arc::new))
    }

    /// The database of a database-level target.
    pub fn into_record(self) -> Result<DatabaseRecord, ApiError> {
        self.record.ok_or_else(|| {
            error!("authorize: a database route resolved no database");
            ApiError::internal()
        })
    }

    /// The project of a project target.
    pub fn into_project(self) -> Result<ProjectRecord, ApiError> {
        self.project.ok_or_else(|| {
            error!("authorize: a project route resolved no project");
            ApiError::internal()
        })
    }
}

/// The API answer for a refusal. `NotFound` gets a generic message here; [`authorize`] answers
/// it with the target's own "not found" message so it can't be told from a missing one.
pub fn deny_to_api(d: Deny) -> ApiError {
    match d {
        Deny::NotFound => ApiError::not_found("not found"),
        Deny::Forbidden { required } => {
            ApiError::new(403, ErrorCode::Forbidden, format!("this needs {required}"))
                .with_extra("required", serde_json::Value::String(required))
        }
        Deny::NoAccessYet => ApiError::new(
            403,
            ErrorCode::NoAccessYet,
            "you have no access yet; a tenant admin must grant you access to a project",
        ),
        Deny::PendingApproval => ApiError::new(
            403,
            ErrorCode::PendingApproval,
            "your sign-up is waiting for an admin to approve it",
        ),
        Deny::NotAvailableInPlatformMode => ApiError::new(
            403,
            ErrorCode::NotAvailableInPlatformMode,
            "not available in platform mode",
        ),
    }
}

fn internal(what: &str, err: impl std::fmt::Display) -> ApiError {
    error!("authorize: {what}: {err}");
    ApiError::internal()
}

/// A stored project grant as an access grant (`owner` never stored, R7).
pub(crate) fn grant_of(row: &GrantRow) -> Grant {
    Grant {
        platform_role: row.platform_role,
        roles: RoleSet::Only(row.pg_roles.iter().map(|r| PgRole::parse(r)).collect()),
    }
}

pub(crate) fn narrowing_of(row: &OverrideRow) -> Narrowing {
    Narrowing {
        platform_role: row.platform_role,
        roles: row
            .pg_roles
            .as_ref()
            .map(|roles| roles.iter().map(|r| PgRole::parse(r)).collect()),
    }
}

/// The caller's membership, as [`decide`] reads it (`has_any_grant` is filled in later).
pub(crate) fn member_facts(member: Option<&MemberRecord>) -> MemberFacts {
    match member {
        None => MemberFacts::NotMember,
        Some(m) => match m.status {
            MemberStatus::Invited => MemberFacts::Invited,
            MemberStatus::Pending => MemberFacts::Pending,
            MemberStatus::Removed => MemberFacts::Removed,
            MemberStatus::Active => MemberFacts::Active {
                tenant_admin: m.tenant_admin,
                has_any_grant: true,
            },
        },
    }
}

/// Resolves `target`, checks it belongs to the caller's tenant, then decides `access` for the
/// caller. A missing target, another tenant's, and one the caller has no grant on all answer
/// the same 404.
///
/// The order matters for what an answer reveals:
/// 0. admission: answers that don't depend on the target (not a member, removed, no grant on
///    any project) come first, so they don't tell an existing target from a missing one;
/// 1. the target, in the caller's tenant;
/// 2. the target branch's protection (`Branch` routes);
/// 3. the effective grant (no cache), and `decide`;
/// 4. R16: acting as an owner-level role needs Admin.
pub async fn authorize<M: MetadataPort, J: JobPort>(
    state: &AppState<M, J>,
    principal: &AuthPrincipal,
    access: Access,
    target: Target<'_>,
) -> Result<Authorized, ApiError> {
    let not_found_message = match target {
        Target::None => "not found",
        Target::Project(_) => "project not found",
        _ => "database not found",
    };
    let answer = |deny: Deny| match deny {
        Deny::NotFound => ApiError::not_found(not_found_message),
        other => deny_to_api(other),
    };

    // 0. Admission.
    let needs_project = matches!(
        access,
        Access::Project(_) | Access::Branch { .. } | Access::ActAs(_)
    );
    let mut member = member_facts(principal.member.as_ref());
    match member {
        MemberFacts::Active {
            tenant_admin: false,
            ..
        } if needs_project => {
            let member_id = principal.member.as_ref().expect("an active member").id;
            let any = state
                .metadata
                .member_projects(member_id)
                .await
                .map_err(|err| internal("load member projects", err))?;
            if any.is_empty() {
                return Err(answer(Deny::NoAccessYet));
            }
            member = MemberFacts::Active {
                tenant_admin: false,
                has_any_grant: true,
            };
        }
        MemberFacts::Active { .. } => {}
        // Not an active member: the answer is the same whatever the target.
        _ => {
            let caller = CallerFacts {
                member,
                in_operator_org: principal.in_operator_org,
            };
            decide(&state.policy, &caller, None, None, None, None, access).map_err(answer)?;
        }
    }

    // 1. The target, in the caller's tenant.
    let mut record = None;
    let mut project = None;
    let project_id = match target {
        Target::None => None,
        Target::Project(id) => {
            match state
                .metadata
                .get_project(principal.tenant_id, ProjectId(id))
                .await
            {
                Ok(Some(p)) => {
                    let id = p.id.as_uuid();
                    project = Some(p);
                    Some(id)
                }
                Ok(None) => return Err(ApiError::not_found(not_found_message)),
                Err(err) => return Err(internal("load project", err)),
            }
        }
        _ => {
            let id = target.database().expect("a database target");
            match state.metadata.get_database(DatabaseId(id)).await {
                Ok(Some(r))
                    if r.status != DatabaseStatus::Deleted
                        && r.tenant_id == principal.tenant_id =>
                {
                    let project = r.project_id.as_uuid();
                    record = Some(r);
                    Some(project)
                }
                Ok(_) => return Err(ApiError::not_found(not_found_message)),
                Err(err) => return Err(internal("load database", err)),
            }
        }
    };

    // 2. The branch's protection, for routes whose minimum depends on it.
    let mut branch_protected = None;
    if let (Access::Branch { .. }, Some(branch), Some(r)) =
        (access, target.branch(), record.as_ref())
    {
        branch_protected = match state.metadata.get_branch(BranchId(branch)).await {
            Ok(Some(b)) if b.database_id == r.database_id => Some(b.protected),
            // Unknown, or another database's: counts as protected; the handler answers 404.
            Ok(_) => None,
            Err(err) => return Err(internal("load branch", err)),
        };
    }

    // 3. The effective grant on the target (read every time, no cache), and the decision.
    let mut grant = None;
    if let (MemberFacts::Active { tenant_admin, .. }, Some(project_id)) = (member, project_id) {
        if tenant_admin {
            grant = Some(tenant_admin_grant());
        } else {
            let member_id = principal.member.as_ref().expect("an active member").id;
            let rows = state
                .metadata
                .grant_rows(member_id, project_id, target.database())
                .await
                .map_err(|err| internal("load grants", err))?;
            grant = rows.project.as_ref().map(|project| {
                effective(
                    &grant_of(project),
                    rows.database.as_ref().map(narrowing_of).as_ref(),
                )
            });
        }
    }
    let caller = CallerFacts {
        member,
        in_operator_org: principal.in_operator_org,
    };
    let owner_role = match (access, record.as_ref()) {
        (Access::ActAs(_), Some(r)) => Some(roles::owner_name(r)),
        _ => None,
    };
    let decided = decide(
        &state.policy,
        &caller,
        grant.as_ref(),
        branch_protected,
        target.requested(),
        owner_role,
        access,
    );
    let act_as_role = match (access, target.requested()) {
        (Access::ActAs(_), Some(Requested::Role(name))) => Some(name),
        _ => None,
    };
    // The roles of the branch, read once for the checks that need them.
    let mut graph: Option<RoleGraph> = None;
    let mut graph_loaded = false;
    if let Err(deny) = decided {
        // R21: a role reachable through a granted role's chain counts as granted.
        let allowed_by_chain = match (&deny, act_as_role, grant.as_ref(), record.as_ref()) {
            (Deny::Forbidden { required }, Some(name), Some(g), Some(r))
                if required.starts_with("role:") && !act_as_reserved(name) =>
            {
                graph = RoleGraph::load(state, r, target.branch()).await?;
                graph_loaded = true;
                graph.as_ref().is_some_and(|gr| gr.chain_grants(&g.roles, name))
            }
            _ => false,
        };
        if !allowed_by_chain {
            return Err(answer(deny));
        }
    }

    // 4. R16, R22, R23 (the single rule, `RoleGraph::refusal`): a role that is, or reaches,
    // an owner-level, LOGIN or reserved role is Admin-only, even when the grant lists it.
    if let (Some(name), Some(r), Some(g)) = (act_as_role, record.as_ref(), grant.as_ref())
        && g.platform_role < PlatformRole::Admin
    {
        if !graph_loaded {
            graph = RoleGraph::load(state, r, target.branch()).await?;
            graph_loaded = true;
        }
        if graph.as_ref().is_some_and(|gr| gr.refusal(name).is_some()) {
            return Err(answer(Deny::Forbidden {
                required: PlatformRole::Admin.as_str().to_string(),
            }));
        }
    }
    let roles = match (graph_loaded, target.branch()) {
        (true, Some(branch)) => Some(LoadedRoles { branch, graph: graph.map(Arc::new) }),
        _ => None,
    };

    Ok(Authorized {
        record,
        project,
        project_id,
        grant,
        branch_protected,
        roles,
    })
}

/// Roles that make whoever reaches them owner-level, besides the database's owner.
const OWNER_LEVEL_ROLES: [&str; 2] = ["neon_superuser", "cloud_admin"];

/// A branch's roles as 6a records them: who is owner-level, who can log in, who is a member of
/// whom.
#[derive(Clone, Debug)]
pub struct RoleGraph {
    owner: String,
    rows: Vec<RoleRow>,
}

/// Why a role's chain makes it Admin-only to grant (R16, R21–R23).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChainRefusal {
    /// The owner, `neon_superuser`, `cloud_admin`, or SUPERUSER, CREATEROLE, CREATEDB,
    /// REPLICATION or BYPASSRLS.
    OwnerLevel,
    /// A LOGIN role (an identity, not a group).
    Login,
    /// A role that holds ADMIN on some role (R30, I2): it could grant the group on.
    Admin,
    /// A reserved name (`pg_*`, `cb_m_*`, `chelabase_*`, the platform's own roles).
    Reserved,
}

impl ChainRefusal {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OwnerLevel => "owner-level",
            Self::Login => "a LOGIN role",
            Self::Admin => "a role that holds ADMIN on another role",
            Self::Reserved => "reserved",
        }
    }

    /// The refusal's `extra.why`: `owner_level`, `login`, `admin` or `reserved`.
    pub fn code(self) -> &'static str {
        match self {
            Self::OwnerLevel => "owner_level",
            Self::Login => "login",
            Self::Admin => "admin",
            Self::Reserved => "reserved",
        }
    }
}

impl RoleGraph {
    /// The roles of an endpoint (`rows`, from `list_roles`) of a database owned by `owner`.
    pub fn new(owner: &str, rows: Vec<RoleRow>) -> Self {
        Self { owner: owner.to_string(), rows }
    }

    /// Why `role` itself may not be in a non-Admin member's chain, if it may not.
    fn refused(&self, role: &str) -> Option<ChainRefusal> {
        if self.owner_level_alone(role) {
            Some(ChainRefusal::OwnerLevel)
        } else if self.row(role).is_some_and(|r| r.login) {
            Some(ChainRefusal::Login)
        } else if self.row(role).is_some_and(|r| !r.attributes.member_admin.is_empty()) {
            Some(ChainRefusal::Admin)
        } else if act_as_reserved(role) {
            Some(ChainRefusal::Reserved)
        } else {
            None
        }
    }

    /// R16, R21–R23, R30: the shortest chain from `name` (itself included) to a role that is
    /// owner-level, LOGIN, an ADMIN holder or reserved, and why that role is refused; `None`
    /// when `name` and everything it is a member of are plain NOLOGIN groups. The walk follows
    /// every membership, whatever its options (I2): an `INHERIT`-only edge carries privileges
    /// and an `ADMIN` edge the power to regrant. Memberships are followed in 6a's (sorted)
    /// order, so the answer is stable.
    ///
    /// This is the single predicate for "a non-Admin member may hold or become this role":
    /// grant and override validation (the member API) and the chain rule of act-as
    /// ([`Self::chain_grants`]) use it, and nothing else should decide it. The worker's
    /// `unsafe_node` and Studio's `refusal` must agree with it on every graph
    /// (`tests/role_rules_agree.rs`).
    pub fn refusal(&self, name: &str) -> Option<(Vec<String>, ChainRefusal)> {
        let mut parent: std::collections::HashMap<String, Option<String>> =
            std::collections::HashMap::new();
        let mut queue = std::collections::VecDeque::from([name.to_string()]);
        parent.insert(name.to_string(), None);
        while let Some(role) = queue.pop_front() {
            if let Some(why) = self.refused(&role) {
                let mut chain = vec![role.clone()];
                let mut at = role;
                while let Some(Some(up)) = parent.get(&at) {
                    chain.push(up.clone());
                    at = up.clone();
                }
                chain.reverse();
                return Some((chain, why));
            }
            for next in self.row(&role).map(|r| r.attributes.member_of.as_slice()).unwrap_or(&[]) {
                if !parent.contains_key(next) {
                    parent.insert(next.clone(), Some(role.clone()));
                    queue.push_back(next.clone());
                }
            }
        }
        None
    }

    /// The roles of `branch`. A branch that isn't the database's, or has no endpoint, has
    /// nothing to act on (the handler refuses it): `None`.
    pub(crate) async fn load<M: MetadataPort, J: JobPort>(
        state: &AppState<M, J>,
        record: &DatabaseRecord,
        branch: Option<Uuid>,
    ) -> Result<Option<RoleGraph>, ApiError> {
        let Some(branch) = branch else {
            return Ok(None);
        };
        let endpoint = match state.metadata.get_branch(BranchId(branch)).await {
            Ok(Some(b)) if b.database_id == record.database_id => b.endpoint_id,
            Ok(_) => None,
            Err(err) => return Err(internal("load branch", err)),
        };
        let Some(endpoint) = endpoint else {
            return Ok(None);
        };
        let rows = state
            .metadata
            .list_roles(&endpoint)
            .await
            .map_err(|err| internal("load roles", err))?;
        Ok(Some(RoleGraph { owner: roles::owner_name(record).to_string(), rows }))
    }

    fn row(&self, name: &str) -> Option<&RoleRow> {
        self.rows.iter().find(|r| r.name == name)
    }

    /// `name` and every role it reaches through memberships with the SET option (the roles it
    /// can `SET ROLE` to, I2).
    pub fn closure(&self, name: &str) -> HashSet<String> {
        let mut seen = HashSet::new();
        let mut queue = vec![name.to_string()];
        while let Some(role) = queue.pop() {
            if !seen.insert(role.clone()) {
                continue;
            }
            if let Some(row) = self.row(&role) {
                let a = &row.attributes;
                queue.extend(a.member_of.iter().filter(|g| !a.member_no_set.contains(g)).cloned());
            }
        }
        seen
    }

    /// Owner-level by itself: the owner, `neon_superuser`, `cloud_admin`, or a role 6a records as
    /// owner-level (SUPERUSER, CREATEROLE, CREATEDB, REPLICATION) or with BYPASSRLS.
    fn owner_level_alone(&self, name: &str) -> bool {
        name == self.owner
            || OWNER_LEVEL_ROLES.contains(&name)
            || self
                .row(name)
                .is_some_and(|r| r.level == RoleLevel::OwnerLevel || r.attributes.bypass_rls)
    }

    /// R21: whether a granted role's chain reaches `name`. A granted role whose chain holds an
    /// owner-level, LOGIN or reserved role contributes nothing.
    fn chain_grants(&self, granted: &RoleSet, name: &str) -> bool {
        let RoleSet::Only(set) = granted else {
            return true;
        };
        set.iter().any(|role| {
            let PgRole::Named(g) = role else {
                return false;
            };
            g != name && self.refusal(g).is_none() && self.closure(g).contains(name)
        })
    }

    /// Whether the branch has a role `name` as 6a records it.
    pub(crate) fn has(&self, name: &str) -> bool {
        self.row(name).is_some()
    }
}

/// A data key route's second check (T6): the key's bound role must be one the caller may use
/// (in their effective roles or reachable through a granted role's chain, R21; LOGIN and
/// owner-level roles need Admin, R22 and R16), unless the caller is Admin. The first check,
/// `Project(Developer)`, has passed and gave `authorized`, whose grant is used as it is.
pub(crate) async fn authorize_key_role<M: MetadataPort, J: JobPort>(
    state: &AppState<M, J>,
    authorized: &Authorized,
    branch: Uuid,
    role: &str,
) -> Result<(), ApiError> {
    let Some(record) = authorized.record.as_ref() else {
        return Err(internal("key role", "a key route resolved no database"));
    };
    let Some(grant) = authorized.grant.as_ref() else {
        return Err(deny_to_api(Deny::NotFound));
    };
    let admin = || {
        deny_to_api(Deny::Forbidden { required: PlatformRole::Admin.as_str().to_string() })
    };
    if grant.platform_role == PlatformRole::Admin {
        return Ok(());
    }
    let name = match Requested::from_name(role, roles::owner_name(record)) {
        Requested::Owner => return Err(admin()),
        Requested::Role(name) => name,
    };
    let graph = RoleGraph::load(state, record, Some(branch)).await?;
    let direct = !act_as_reserved(name) && grant.roles.contains(&PgRole::Named(name.to_string()));
    let by_chain = !act_as_reserved(name)
        && graph.as_ref().is_some_and(|g| g.chain_grants(&grant.roles, name));
    if !direct && !by_chain {
        return Err(deny_to_api(Deny::Forbidden { required: format!("role:{name}") }));
    }
    // R33: the single rule, as for act-as.
    if graph.as_ref().is_some_and(|g| g.refusal(name).is_some()) {
        return Err(admin());
    }
    Ok(())
}

/// [`authorize`] for a route on a database (or below), returning the database.
pub(crate) async fn authorize_database<M: MetadataPort, J: JobPort>(
    state: &AppState<M, J>,
    principal: &AuthPrincipal,
    access: Access,
    target: Target<'_>,
) -> Result<DatabaseRecord, ApiError> {
    authorize(state, principal, access, target)
        .await?
        .into_record()
}

/// [`authorize`] for a route on a project, returning the project.
pub(crate) async fn authorize_project<M: MetadataPort, J: JobPort>(
    state: &AppState<M, J>,
    principal: &AuthPrincipal,
    access: Access,
    id: Uuid,
) -> Result<ProjectRecord, ApiError> {
    authorize(state, principal, access, Target::Project(id))
        .await?
        .into_project()
}
