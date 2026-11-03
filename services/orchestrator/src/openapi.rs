//! The Management API's OpenAPI document, generated from the handlers' `#[utoipa::path]`
//! annotations. `openapi/management.json` is the committed copy; `just api-gen` rewrites it.

use utoipa::openapi::OpenApi;
use utoipa::openapi::path::{Operation, PathItem};
use utoipa::openapi::security::{HttpAuthScheme, HttpBuilder, SecurityRequirement, SecurityScheme};
use utoipa::openapi::{InfoBuilder, LicenseBuilder};

use super::*;

/// The document, serialized once.
static DOCUMENT: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
    serde_json::to_string(&openapi()).expect("the document serializes")
});

/// `GET /v1/openapi.json`: public, a plain route (so not in the document itself).
pub(crate) fn router<S>() -> axum::Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    axum::Router::new().route(
        chelabase_routes::orchestrator::OPENAPI_JSON.path,
        axum::routing::get(openapi_json),
    )
}

async fn openapi_json() -> axum::response::Response {
    use axum::response::IntoResponse;
    (
        [
            (axum::http::header::CONTENT_TYPE, "application/json"),
            (axum::http::header::CACHE_CONTROL, "no-cache"),
        ],
        DOCUMENT.as_str(),
    )
        .into_response()
}

/// The Management API's OpenAPI document.
pub fn openapi() -> OpenApi {
    let (_, mut doc) = api_router::<Doc, Doc>().split_for_parts();
    doc.info = InfoBuilder::new()
        .title("Chelabase Management API")
        .version("1")
        .license(Some(LicenseBuilder::new().name("Apache-2.0").build()))
        .build();
    doc.components
        .get_or_insert_with(Default::default)
        .add_security_scheme(
            "bearer",
            SecurityScheme::Http(
                HttpBuilder::new()
                    .scheme(HttpAuthScheme::Bearer)
                    .bearer_format("JWT")
                    .build(),
            ),
        );
    for item in doc.paths.paths.values_mut() {
        for op in operations_mut(item) {
            op.security = Some(vec![SecurityRequirement::new(
                "bearer",
                Vec::<String>::new(),
            )]);
        }
    }
    document_access(&mut doc);
    doc
}

/// Adds each operation's access answers (from its endpoint's `Access` in `chelabase_routes`,
/// the one table of who may call what) to its 403 and 404 descriptions.
fn document_access(doc: &mut OpenApi) {
    use chelabase_types::access::Access;
    const ADMISSION: &str = "`no_access_yet`: you have no grant on any project yet (ask a tenant admin); \
`pending_approval`: you signed up and an admin hasn't approved you yet; \
`not_available_in_platform_mode`: platform mode, and you aren't a member of the tenant";
    for (path, item) in doc.paths.paths.iter_mut() {
        let ops = [
            ("GET", item.get.as_mut()),
            ("POST", item.post.as_mut()),
            ("PUT", item.put.as_mut()),
            ("PATCH", item.patch.as_mut()),
            ("DELETE", item.delete.as_mut()),
        ];
        for (method, op) in ops {
            let Some(op) = op else { continue };
            let Some(endpoint) = chelabase_routes::ENDPOINTS.iter().find(|e| {
                e.service == Service::Orchestrator && e.method.as_str() == method && e.path == path
            }) else {
                continue;
            };
            let (forbidden, not_found) = match endpoint.policy.access {
                Access::Public | Access::DataKey | Access::SignedIn => continue,
                Access::TenantMember => (
                    ADMISSION.to_string(),
                    "you were removed from the tenant".to_string(),
                ),
                Access::TenantAdmin => (
                    format!("`forbidden`: only a tenant admin may (`required`: `tenant admin`); {ADMISSION}"),
                    "you were removed from the tenant".to_string(),
                ),
                Access::Instance => (
                    format!("`forbidden`: only an instance admin may (`required`: `instance admin`); {ADMISSION}"),
                    "you were removed from the tenant".to_string(),
                ),
                Access::Project(min) => (
                    format!(
                        "`forbidden`: your role on the project is below `{}` (`required` names it); {ADMISSION}",
                        min.as_str()
                    ),
                    "you have no access to it".to_string(),
                ),
                Access::Branch { unprotected, protected } => (
                    format!(
                        "`forbidden`: your role on the project is below `{}`, or `{}` on a protected branch (for a merge, the branch merged into; `required` names it); {ADMISSION}",
                        unprotected.as_str(),
                        protected.as_str()
                    ),
                    "you have no access to it".to_string(),
                ),
                Access::ActAsBranch { unprotected, protected } => (
                    format!(
                        "`forbidden`: your role on the project is below `{}` on an unprotected branch, below `{}` on a protected one, or the requested role isn't in your set (the owner, a LOGIN role or an owner-level role need `admin`); `required` names what's missing (`role:<name>` for a role); {ADMISSION}",
                        unprotected.as_str(),
                        protected.as_str()
                    ),
                    "you have no access to it".to_string(),
                ),
                Access::ActAs(min) => (
                    format!(
                        "`forbidden`: your role on the project is below `{}`, or you may not act as the requested role: it isn't in your roles or reachable through one of them, or it is the owner, a LOGIN role or an owner-level role (all need `admin`); `required` names what's missing (`role:<name>` for a role); {ADMISSION}",
                        min.as_str()
                    ),
                    "you have no access to it".to_string(),
                ),
            };
            let responses = &mut op.responses.responses;
            // Every operation documents its 401 with the error envelope: a missing 403 or 404
            // starts from it.
            let template = responses.get("401").cloned();
            for (status, text) in [("403", forbidden), ("404", format!("`not_found`: {not_found}"))] {
                match responses.get_mut(status) {
                    Some(utoipa::openapi::RefOr::T(response)) => {
                        response.description = format!("{}; {text}", response.description);
                    }
                    Some(utoipa::openapi::RefOr::Ref(_)) => {}
                    None => {
                        if let Some(utoipa::openapi::RefOr::T(mut response)) = template.clone() {
                            response.description = text;
                            responses.insert(status.to_string(), utoipa::openapi::RefOr::T(response));
                        }
                    }
                }
            }
        }
    }
}

fn operations_mut(item: &mut PathItem) -> impl Iterator<Item = &mut Operation> {
    [
        item.get.as_mut(),
        item.put.as_mut(),
        item.post.as_mut(),
        item.delete.as_mut(),
        item.options.as_mut(),
        item.head.as_mut(),
        item.patch.as_mut(),
        item.trace.as_mut(),
    ]
    .into_iter()
    .flatten()
}

/// A state type that can't exist: `openapi()` only needs the router's types, never a value.
enum Doc {}

#[allow(unused_variables)]
impl MetadataPort for Doc {
    async fn get_tenant_by_zitadel_org_id(
        &self,
        zitadel_org_id: &str,
    ) -> Result<Option<TenantId>, MetadataError> {
        match *self {}
    }
    async fn insert_database_pending(
        &self,
        database_id: DatabaseId,
        tenant_id: TenantId,
        project_id: ProjectId,
        name: &str,
    ) -> Result<(), MetadataError> {
        match *self {}
    }
    async fn set_failed(&self, database_id: DatabaseId, error: &str) -> Result<(), MetadataError> {
        match *self {}
    }
    async fn get_database(
        &self,
        database_id: DatabaseId,
    ) -> Result<Option<DatabaseRecord>, MetadataError> {
        match *self {}
    }
    async fn tenant_overview(
        &self,
        tenant_id: TenantId,
    ) -> Result<Option<TenantOverview>, MetadataError> {
        match *self {}
    }
    async fn get_branch(&self, branch_id: BranchId) -> Result<Option<BranchRecord>, MetadataError> {
        match *self {}
    }
    async fn begin_delete(&self, database_id: DatabaseId) -> Result<BeginDelete, MetadataError> {
        match *self {}
    }
    async fn list_branches(
        &self,
        database_id: DatabaseId,
    ) -> Result<Vec<BranchRecord>, MetadataError> {
        match *self {}
    }
    async fn main_branches_with_counts(
        &self,
        tenant_id: TenantId,
    ) -> Result<Vec<(BranchRecord, i64)>, MetadataError> {
        match *self {}
    }
    async fn create_branch(
        &self,
        branch: &NewBranch<'_>,
    ) -> Result<CreateBranchOutcome, MetadataError> {
        match *self {}
    }
    async fn live_children(&self, branch_id: BranchId) -> Result<Vec<BranchRecord>, MetadataError> {
        match *self {}
    }
    async fn set_branch_status(
        &self,
        branch_id: BranchId,
        from: &[BranchStatus],
        to: BranchStatus,
    ) -> Result<bool, MetadataError> {
        match *self {}
    }
    async fn claim_branch_for_switch(
        &self,
        branch_id: BranchId,
        allow_protected: bool,
    ) -> Result<ClaimOutcome, MetadataError> {
        match *self {}
    }
    async fn set_branch_protected(
        &self,
        branch_id: BranchId,
        protected: bool,
    ) -> Result<Option<BranchRecord>, MetadataError> {
        match *self {}
    }
    async fn set_branch_guard_logins(
        &self,
        branch_id: BranchId,
        on: bool,
    ) -> Result<Option<BranchRecord>, MetadataError> {
        match *self {}
    }
    async fn list_guard_exemptions(&self, branch_id: BranchId) -> Result<Vec<String>, MetadataError> {
        match *self {}
    }
    async fn add_guard_exemption(&self, branch_id: BranchId, role: &str) -> Result<bool, MetadataError> {
        match *self {}
    }
    async fn remove_guard_exemption(&self, branch_id: BranchId, role: &str) -> Result<bool, MetadataError> {
        match *self {}
    }
    async fn protect_branch(
        &self,
        branch_id: BranchId,
        hash: &str,
        keys: &[String],
    ) -> Result<Option<BranchRecord>, MetadataError> {
        match *self {}
    }
    async fn acknowledge_review(
        &self,
        branch_id: BranchId,
        hash: &str,
        keys: &[String],
    ) -> Result<Option<BranchRecord>, MetadataError> {
        match *self {}
    }
    async fn set_branch_failed(
        &self,
        branch_id: BranchId,
        error: &str,
    ) -> Result<(), MetadataError> {
        match *self {}
    }
    async fn begin_branch_delete(
        &self,
        branch_id: BranchId,
    ) -> Result<BranchDeleteOutcome, MetadataError> {
        match *self {}
    }
    async fn update_database_settings(
        &self,
        database_id: DatabaseId,
        patch: &SettingsPatch,
    ) -> Result<bool, MetadataError> {
        match *self {}
    }
    async fn insert_data_key(&self, key: &NewDataKey) -> Result<(), MetadataError> {
        match *self {}
    }
    async fn create_secret_key(&self, key: &NewDataKey) -> Result<(), MetadataError> {
        match *self {}
    }
    async fn list_data_keys(&self, branch: BranchId) -> Result<Vec<DataKeyRecord>, MetadataError> {
        match *self {}
    }
    async fn revoke_data_key(&self, branch: BranchId, key_id: Uuid) -> Result<bool, MetadataError> {
        match *self {}
    }
    async fn rotate_data_key(
        &self,
        branch: BranchId,
        old: Uuid,
        new: &NewDataKey,
    ) -> Result<bool, MetadataError> {
        match *self {}
    }
    async fn rotate_secret_key(
        &self,
        branch: BranchId,
        old: Uuid,
        new: &NewDataKey,
    ) -> Result<bool, MetadataError> {
        match *self {}
    }
    async fn list_roles(&self, endpoint_id: &str) -> Result<Vec<RoleRow>, MetadataError> {
        match *self {}
    }
    async fn member_branch_roles(
        &self,
        branch: Uuid,
    ) -> Result<Vec<chelabase_metadata::members::MemberBranchRole>, MetadataError> {
        match *self {}
    }
    async fn record_member_acted_role(
        &self,
        member: Uuid,
        branch: Uuid,
        role: &str,
    ) -> Result<bool, MetadataError> {
        match *self {}
    }
    async fn role_password(
        &self,
        endpoint_id: &str,
        name: &str,
    ) -> Result<Option<String>, MetadataError> {
        match *self {}
    }
    async fn branch_databases(
        &self,
        branch: BranchId,
    ) -> Result<Vec<BranchDatabase>, MetadataError> {
        match *self {}
    }
    async fn branches_with_main_database(
        &self,
        ids: &[BranchId],
    ) -> Result<std::collections::HashSet<BranchId>, MetadataError> {
        match *self {}
    }
    async fn begin_enable_data_api(
        &self,
        branch: BranchId,
        name: &str,
        cap: u32,
    ) -> Result<EnableStart, MetadataError> {
        match *self {}
    }
    async fn set_database_default_schema(
        &self,
        branch: BranchId,
        name: &str,
        schema: &str,
    ) -> Result<bool, MetadataError> {
        match *self {}
    }
    async fn set_key_databases(
        &self,
        branch: BranchId,
        key_id: Uuid,
        databases: Option<Vec<String>>,
    ) -> Result<bool, MetadataError> {
        match *self {}
    }
    async fn list_projects(
        &self,
        tenant: TenantId,
    ) -> Result<Vec<ProjectRecord>, MetadataError> {
        match *self {}
    }
    async fn get_project(
        &self,
        tenant: TenantId,
        id: ProjectId,
    ) -> Result<Option<ProjectRecord>, MetadataError> {
        match *self {}
    }
    async fn project_by_id(
        &self,
        id: ProjectId,
    ) -> Result<Option<ProjectRecord>, MetadataError> {
        match *self {}
    }
    async fn create_project(
        &self,
        tenant: TenantId,
        name: &str,
    ) -> Result<ProjectRecord, MetadataError> {
        match *self {}
    }
    async fn rename_project(
        &self,
        tenant: TenantId,
        id: ProjectId,
        name: &str,
    ) -> Result<RenameProject, MetadataError> {
        match *self {}
    }
    async fn begin_project_delete(
        &self,
        tenant: TenantId,
        id: ProjectId,
    ) -> Result<BeginProjectDelete, MetadataError> {
        match *self {}
    }
    async fn project_databases(
        &self,
        id: ProjectId,
    ) -> Result<Vec<(DatabaseId, DatabaseStatus, Option<String>)>, MetadataError> {
        match *self {}
    }
    async fn project_auth(
        &self,
        project: ProjectId,
    ) -> Result<Option<ProjectAuth>, MetadataError> {
        match *self {}
    }
    async fn project_auth_statuses(
        &self,
        tenant: TenantId,
    ) -> Result<std::collections::HashMap<ProjectId, AppUsersStatus>, MetadataError> {
        match *self {}
    }
    async fn begin_enable_app_users(
        &self,
        new: &NewProjectAuth<'_>,
    ) -> Result<BeginEnable, MetadataError> {
        match *self {}
    }
    async fn update_app_users_settings(
        &self,
        project: ProjectId,
        email_verification: Option<bool>,
        on_delete: Option<OnDelete>,
    ) -> Result<Option<ProjectAuth>, MetadataError> {
        match *self {}
    }
    async fn begin_disable_app_users(
        &self,
        project: ProjectId,
    ) -> Result<bool, MetadataError> {
        match *self {}
    }
    async fn branch_auth_sync(
        &self,
        branch: BranchId,
    ) -> Result<Option<BranchAuthSync>, MetadataError> {
        match *self {}
    }
    async fn list_user_sessions(
        &self,
        project: ProjectId,
        user: Uuid,
    ) -> Result<Vec<AppSession>, MetadataError> {
        match *self {}
    }
    async fn revoke_user_sessions(
        &self,
        project: ProjectId,
        user: Uuid,
        keep: Option<Uuid>,
        created_before: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<Vec<AppSession>, MetadataError> {
        match *self {}
    }
    async fn app_auth_for_database(
        &self,
        db: DatabaseId,
    ) -> Result<Option<ProjectAuth>, MetadataError> {
        match *self {}
    }

    async fn list_saved_queries(
        &self,
        database: Uuid,
    ) -> Result<Vec<SavedQuerySummary>, MetadataError> {
        match *self {}
    }

    async fn get_saved_query(
        &self,
        database: Uuid,
        id: Uuid,
    ) -> Result<Option<SavedQuery>, MetadataError> {
        match *self {}
    }

    async fn create_saved_query(
        &self,
        database: Uuid,
        user: &str,
        author: Option<Uuid>,
        id: Uuid,
        name: &str,
        sql: &str,
        pg_database: Option<&str>,
        role: Option<&str>,
    ) -> Result<SavedQueryWrite, MetadataError> {
        match *self {}
    }

    async fn update_saved_query(
        &self,
        database: Uuid,
        by: SavedQueryEditor,
        id: Uuid,
        version: i32,
        patch: &SavedQueryPatch,
    ) -> Result<SavedQueryWrite, MetadataError> {
        match *self {}
    }

    async fn delete_saved_query(
        &self,
        database: Uuid,
        by: SavedQueryEditor,
        id: Uuid,
    ) -> Result<bool, MetadataError> {
        match *self {}
    }
    async fn create_merge(&self, merge: &NewMerge) -> Result<MergeCreate, MetadataError> {
        match *self {}
    }
    async fn get_merge(&self, id: Uuid) -> Result<Option<MergeRecord>, MetadataError> {
        match *self {}
    }
    async fn list_merges(
        &self,
        database_id: DatabaseId,
        branch: Option<BranchId>,
    ) -> Result<Vec<MergeRecord>, MetadataError> {
        match *self {}
    }
    async fn update_merge_draft(
        &self,
        id: Uuid,
        expected_version: i64,
        patch: &MergeDraftPatch,
    ) -> Result<MergeWrite, MetadataError> {
        match *self {}
    }
    async fn set_merge_status(
        &self,
        id: Uuid,
        from: &[MergeStatus],
        to: MergeStatus,
        patch: MergeStatusPatch,
    ) -> Result<bool, MetadataError> {
        match *self {}
    }
    async fn merge_events(&self, id: Uuid) -> Result<Vec<MergeEvent>, MetadataError> {
        match *self {}
    }
    async fn cancel_drafts_for_branch(&self, branch: BranchId) -> Result<u64, MetadataError> {
        match *self {}
    }
    async fn branch_has_busy_merge(&self, branch: BranchId) -> Result<Option<chelabase_metadata::BranchBusy>, MetadataError> {
        match *self {}
    }
    async fn member_by_identity_user(
        &self,
        tenant: TenantId,
        identity_user_id: &str,
    ) -> Result<Option<MemberRecord>, MetadataError> {
        match *self {}
    }
    async fn admit_member(
        &self,
        tenant: TenantId,
        identity_user_id: &str,
        email: &str,
        name: &str,
        admission: chelabase_metadata::members::Admission,
    ) -> Result<MemberRecord, MetadataError> {
        match *self {}
    }
    async fn grant_sources(
        &self,
        member: Uuid,
        project: Uuid,
        database: Option<Uuid>,
    ) -> Result<chelabase_types::access::GrantSources, MetadataError> {
        match *self {}
    }
    async fn member_projects(&self, member: Uuid) -> Result<Vec<(Uuid, GrantRow)>, MetadataError> {
        match *self {}
    }
    async fn member_overrides(&self, member: Uuid) -> Result<Vec<(Uuid, Uuid, chelabase_metadata::members::OverrideRow)>, MetadataError> {
        match *self {}
    }
    async fn list_teams(&self, tenant: TenantId) -> Result<Vec<chelabase_metadata::teams::TeamDetail>, MetadataError> {
        match *self {}
    }
    async fn team(&self, tenant: TenantId, team: Uuid) -> Result<Option<chelabase_metadata::teams::TeamDetail>, MetadataError> {
        match *self {}
    }
    async fn member_teams(&self, member: Uuid) -> Result<Vec<chelabase_metadata::teams::TeamDetail>, MetadataError> {
        match *self {}
    }
    async fn project_team_grants(&self, project: Uuid) -> Result<Vec<chelabase_metadata::teams::ProjectTeamGrant>, MetadataError> {
        match *self {}
    }
    async fn create_team(
        &self,
        actor: Uuid,
        tenant: TenantId,
        name: &str,
        description: &str,
    ) -> Result<chelabase_metadata::teams::TeamRecord, chelabase_metadata::teams::TeamWriteError> {
        match *self {}
    }
    async fn update_team(
        &self,
        actor: Uuid,
        tenant: TenantId,
        team: Uuid,
        name: Option<&str>,
        description: Option<&str>,
    ) -> Result<chelabase_metadata::teams::TeamRecord, chelabase_metadata::teams::TeamWriteError> {
        match *self {}
    }
    async fn team_change_observed(
        &self,
        tenant: TenantId,
        change: &chelabase_metadata::teams::TeamChange,
    ) -> Result<Vec<(Uuid, chelabase_metadata::members::Observed)>, chelabase_metadata::teams::TeamWriteError> {
        match *self {}
    }
    async fn write_team_checked(
        &self,
        actor: Uuid,
        tenant: TenantId,
        change: &chelabase_metadata::teams::TeamChange,
        expect: &[(Uuid, chelabase_metadata::members::Observed)],
        skipped_admins: &[Uuid],
    ) -> Result<chelabase_metadata::teams::TeamWrite, chelabase_metadata::teams::TeamWriteError> {
        match *self {}
    }
}

#[allow(unused_variables)]
impl JobPort for Doc {
    async fn publish_provision(
        &self,
        job: &JobEnvelope<ProvisionDatabaseJob>,
    ) -> Result<(), BoxError> {
        match *self {}
    }
    async fn publish_delete(&self, job: &JobEnvelope<DeleteDatabaseJob>) -> Result<(), BoxError> {
        match *self {}
    }
    async fn publish_branch(&self, job: &JobEnvelope<BranchJob>) -> Result<(), BoxError> {
        match *self {}
    }
    async fn publish_database_settings(
        &self,
        job: &JobEnvelope<DatabaseSettingsJob>,
    ) -> Result<(), BoxError> {
        match *self {}
    }
    async fn publish_merge(&self, job: &JobEnvelope<MergeJob>) -> Result<(), BoxError> {
        match *self {}
    }
    async fn check_data_role(
        &self,
        branch_id: BranchId,
        role: &str,
    ) -> Result<CheckDataRoleReply, ResetError> {
        match *self {}
    }
    async fn reconcile_member_roles(
        &self,
        request: chelabase_nats::MemberRolesReconcileRequest,
    ) -> Result<chelabase_nats::MemberRolesReply, ResetError> {
        match *self {}
    }
    async fn hand_over_member_role(
        &self,
        request: chelabase_nats::MemberRoleHandOverRequest,
    ) -> Result<chelabase_nats::MemberRolesReply, ResetError> {
        match *self {}
    }
    async fn review(
        &self,
        request: chelabase_nats::ReviewRequest,
    ) -> Result<chelabase_nats::ReviewReply, ResetError> {
        match *self {}
    }
    async fn enable_data_api(&self, branch_id: BranchId, name: &str) -> Result<(), BoxError> {
        match *self {}
    }
    async fn disable_data_api(
        &self,
        branch_id: BranchId,
        name: &str,
        remove: bool,
    ) -> Result<CheckDataRoleReply, ResetError> {
        match *self {}
    }
    async fn rebootstrap_data_api(
        &self,
        branch_id: BranchId,
        name: &str,
        grant_existing: bool,
    ) -> Result<CheckDataRoleReply, ResetError> {
        match *self {}
    }
    async fn reset_password(
        &self,
        database_id: DatabaseId,
        branch_id: BranchId,
    ) -> Result<ResetPasswordReply, ResetError> {
        match *self {}
    }
    async fn create_role(&self, request: RoleCreateRequest) -> Result<RoleReply, ResetError> {
        match *self {}
    }
    async fn reset_role(&self, request: RoleRequest) -> Result<RoleReply, ResetError> {
        match *self {}
    }
    async fn update_role(&self, request: RoleUpdateRequest) -> Result<RoleReply, ResetError> {
        match *self {}
    }
    async fn delete_role(&self, request: chelabase_nats::RoleDeleteRequest) -> Result<RoleReply, ResetError> {
        match *self {}
    }
    async fn preview_role_delete(&self, request: chelabase_nats::RoleDeleteRequest) -> Result<RoleReply, ResetError> {
        match *self {}
    }
    async fn role_sessions(&self, request: RoleRequest) -> Result<RoleSessionsReply, ResetError> {
        match *self {}
    }
    async fn check_all_rows(
        &self,
        request: chelabase_nats::AllRowsCheckRequest,
    ) -> Result<chelabase_nats::AllRowsCheckReply, ResetError> {
        match *self {}
    }
    async fn publish_project_delete(&self, job: &JobEnvelope<ProjectDeleteJob>) -> Result<(), BoxError> {
        match *self {}
    }
    async fn publish_app_users(&self, job: &JobEnvelope<AppUsersJob>) -> Result<(), BoxError> {
        match *self {}
    }
    async fn publish_app_users_sync(&self, job: &JobEnvelope<AppUsersSyncJob>) -> Result<(), BoxError> {
        match *self {}
    }
    async fn check_app_users_location(
        &self,
        request: &AppUsersCheckRequest,
    ) -> Result<AppUsersCheckReply, BoxError> {
        match *self {}
    }
}

#[allow(unused_variables)]
impl members::MemberRolesQueue for Doc {
    async fn enqueue_member_roles(&self, branch: BranchId) -> Result<(), BoxError> {
        match *self {}
    }
}

#[allow(unused_variables)]
impl members::MembersPort for Doc {
    async fn member(&self, member: Uuid) -> Result<Option<MemberRecord>, MetadataError> {
        match *self {}
    }
    async fn member_by_email(&self, tenant: TenantId, email: &str) -> Result<Option<MemberRecord>, MetadataError> {
        match *self {}
    }
    async fn list_members(
        &self,
        tenant: TenantId,
    ) -> Result<Vec<chelabase_metadata::members::MemberListing>, MetadataError> {
        match *self {}
    }
    async fn member_listing(
        &self,
        member: Uuid,
    ) -> Result<Option<chelabase_metadata::members::MemberListing>, MetadataError> {
        match *self {}
    }
    async fn open_invites(
        &self,
        tenant: TenantId,
    ) -> Result<Vec<chelabase_metadata::members::OpenInvite>, MetadataError> {
        match *self {}
    }
    async fn invite(
        &self,
        invite: Uuid,
    ) -> Result<Option<chelabase_metadata::members::InviteState>, MetadataError> {
        match *self {}
    }
    async fn project_access(
        &self,
        project: Uuid,
    ) -> Result<chelabase_metadata::members::ProjectAccess, MetadataError> {
        match *self {}
    }
    async fn project_branches(&self, project: Uuid) -> Result<Vec<Uuid>, MetadataError> {
        match *self {}
    }
    async fn open_signup(&self, tenant: TenantId) -> Result<bool, MetadataError> {
        match *self {}
    }
    async fn admin_claimed_since(&self, tenant: TenantId, since: chrono::DateTime<chrono::Utc>) -> Result<bool, MetadataError> {
        match *self {}
    }
    async fn access_events(
        &self,
        tenant: TenantId,
        project: Option<Uuid>,
        member: Option<Uuid>,
        before_id: Option<i64>,
        limit: u32,
    ) -> Result<Vec<chelabase_metadata::members::AccessEvent>, MetadataError> {
        match *self {}
    }
    async fn member_project_roles(&self, tenant: TenantId) -> Result<Vec<chelabase_metadata::personal_logins::MemberProjectRole>, MetadataError> {
        match *self {}
    }
    async fn record_access_event(&self, e: &chelabase_metadata::personal_logins::AccessEventInput<'_>) -> Result<(), MetadataError> {
        match *self {}
    }
    async fn is_personal_login_name(&self, database: Uuid, name: &str) -> Result<bool, MetadataError> {
        match *self {}
    }
    async fn personal_login_names(&self, project: Uuid) -> Result<std::collections::BTreeSet<String>, MetadataError> {
        match *self {}
    }
    async fn create_personal_login(
        &self,
        member: Uuid,
        database: Uuid,
        role_name: &str,
        scram_verifier: &str,
        valid_until: chrono::DateTime<chrono::Utc>,
    ) -> Result<chelabase_metadata::personal_logins::PersonalLogin, chelabase_metadata::personal_logins::LoginWriteError> {
        match *self {}
    }
    async fn reset_personal_login(
        &self,
        member: Uuid,
        database: Uuid,
        scram_verifier: &str,
        valid_until: chrono::DateTime<chrono::Utc>,
    ) -> Result<
        (chelabase_metadata::personal_logins::PersonalLogin, Vec<chelabase_metadata::personal_logins::LoginEndpoint>),
        chelabase_metadata::personal_logins::LoginWriteError,
    > {
        match *self {}
    }
    async fn delete_personal_login(
        &self,
        actor: Uuid,
        member: Uuid,
        database: Uuid,
    ) -> Result<Vec<chelabase_metadata::personal_logins::LoginEndpoint>, chelabase_metadata::personal_logins::LoginWriteError> {
        match *self {}
    }
    async fn member_personal_logins(&self, member: Uuid) -> Result<Vec<chelabase_metadata::personal_logins::PersonalLogin>, MetadataError> {
        match *self {}
    }
    async fn database_personal_logins(&self, database: Uuid) -> Result<Vec<chelabase_metadata::personal_logins::PersonalLogin>, MetadataError> {
        match *self {}
    }
    async fn member_login_endpoints(&self, member: Uuid) -> Result<Vec<chelabase_metadata::personal_logins::LoginEndpoint>, MetadataError> {
        match *self {}
    }
    async fn suspend_member(&self, actor: Uuid, member: Uuid) -> Result<Vec<Uuid>, DocWriteError> {
        match *self {}
    }
    async fn reinstate_member(&self, actor: Uuid, member: Uuid) -> Result<Vec<Uuid>, DocWriteError> {
        match *self {}
    }
    async fn revoke_postgres_access(&self, actor: Uuid, member: Uuid) -> Result<Vec<Uuid>, DocWriteError> {
        match *self {}
    }
    async fn member_observed(&self, member: Uuid) -> Result<Vec<chelabase_metadata::members::Observed>, MetadataError> {
        match *self {}
    }
    async fn set_tenant_admin(
        &self,
        actor: Option<Uuid>,
        member: Uuid,
        admin: bool,
        expect: Option<&[chelabase_metadata::members::Observed]>,
    ) -> Result<(), DocWriteError> {
        match *self {}
    }
    async fn remove_member(&self, actor: Uuid, member: Uuid) -> Result<Vec<Uuid>, DocWriteError> {
        match *self {}
    }
    async fn put_project_grant(&self, actor: Uuid, member: Uuid, project: Uuid, grant: &GrantRow, expect: &chelabase_metadata::members::Observed) -> Result<bool, DocWriteError> {
        match *self {}
    }
    async fn put_project_grants(
        &self,
        actor: Uuid,
        member: Uuid,
        grants: &[(Uuid, GrantRow)],
        teams: &[chelabase_metadata::members::TeamSeen],
        expect: &[chelabase_metadata::members::Observed],
    ) -> Result<Vec<Uuid>, DocWriteError> {
        match *self {}
    }
    async fn delete_project_grant(
        &self,
        actor: Uuid,
        member: Uuid,
        project: Uuid,
        expect: &chelabase_metadata::members::Observed,
    ) -> Result<(), DocWriteError> {
        match *self {}
    }
    async fn put_override(
        &self,
        actor: Uuid,
        member: Uuid,
        database: Uuid,
        o: &chelabase_metadata::members::OverrideRow,
        expect: &chelabase_metadata::members::Observed,
    ) -> Result<bool, DocWriteError> {
        match *self {}
    }
    async fn delete_override(&self, actor: Uuid, member: Uuid, database: Uuid, expect: &chelabase_metadata::members::Observed) -> Result<(), DocWriteError> {
        match *self {}
    }
    async fn create_invited_member(
        &self,
        actor: Uuid,
        tenant: TenantId,
        new: chelabase_metadata::members::NewInvite<'_>,
    ) -> Result<(chelabase_metadata::members::MemberListing, Uuid), DocWriteError> {
        match *self {}
    }
    async fn resend_invite(
        &self,
        actor: Uuid,
        invite: Uuid,
        expires_at: chrono::DateTime<chrono::Utc>,
        token_hash: [u8; 32],
    ) -> Result<(), DocWriteError> {
        match *self {}
    }
    async fn revoke_invite(&self, actor: Uuid, invite: Uuid) -> Result<String, DocWriteError> {
        match *self {}
    }
    async fn revoke_undelivered_invite(&self, actor: Uuid, invite: Uuid) -> Result<String, DocWriteError> {
        match *self {}
    }
    async fn set_open_signup(&self, actor: Uuid, tenant: TenantId, on: bool) -> Result<(), DocWriteError> {
        match *self {}
    }
    async fn approve_member(
        &self,
        actor: Uuid,
        member: Uuid,
        grants: &[(Uuid, GrantRow)],
        teams: &[chelabase_metadata::members::TeamSeen],
    ) -> Result<chelabase_metadata::members::MemberListing, DocWriteError> {
        match *self {}
    }
    async fn has_other_membership(&self, member: Uuid) -> Result<bool, MetadataError> {
        match *self {}
    }
    async fn decline_member(&self, actor: Uuid, member: Uuid) -> Result<String, DocWriteError> {
        match *self {}
    }
    async fn revoke_logins_for_identity_user(
        &self,
        tenant: TenantId,
        identity_user_id: &str,
    ) -> Result<Vec<chelabase_metadata::personal_logins::LoginEndpoint>, MetadataError> {
        match *self {}
    }
    async fn clear_offboarded(&self, tenant: TenantId, identity_user_id: &str) -> Result<bool, MetadataError> {
        match *self {}
    }
    async fn set_tenant_display_name(
        &self,
        actor: Uuid,
        tenant: TenantId,
        display_name: Option<&str>,
    ) -> Result<(), DocWriteError> {
        match *self {}
    }
}

type DocWriteError = chelabase_metadata::members::AccessWriteError;

#[cfg(test)]
mod tests {
    use super::*;
    use chelabase_routes::{ENDPOINTS, Service};
    use serde_json::Value;
    use std::collections::BTreeSet;

    const DOCUMENT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../openapi/management.json");
    const METHODS: [&str; 5] = ["get", "post", "put", "patch", "delete"];

    fn document() -> Value {
        serde_json::to_value(openapi()).unwrap()
    }

    /// Every operation as (method, path, operation object).
    fn operations(doc: &Value) -> Vec<(String, String, &Value)> {
        let mut out = Vec::new();
        for (path, item) in doc["paths"].as_object().unwrap() {
            for method in METHODS {
                if let Some(op) = item.get(method) {
                    out.push((method.to_uppercase(), path.clone(), op));
                }
            }
        }
        out
    }

    #[test]
    fn document_is_current() {
        let mut json = serde_json::to_string_pretty(&openapi()).unwrap();
        json.push('\n');
        if std::env::var_os("CHELABASE_BLESS").is_some() {
            std::fs::write(DOCUMENT, &json).unwrap();
        }
        let committed = std::fs::read_to_string(DOCUMENT)
            .expect("openapi/management.json is missing; run `just api-gen`");
        assert!(
            committed == json,
            "openapi/management.json is stale; run `just api-gen` and commit it"
        );
    }

    #[test]
    fn document_matches_the_registry() {
        let doc = document();
        let documented: BTreeSet<(String, String)> = operations(&doc)
            .into_iter()
            .map(|(method, path, _)| (method, path))
            .collect();
        let registry: BTreeSet<(String, String)> = ENDPOINTS
            .iter()
            .filter(|e| {
                e.service == Service::Orchestrator
                    && !matches!(
                        e.name,
                        "orchestrator.health"
                            | "orchestrator.openapi_json"
                    )
            })
            .map(|e| (e.method.as_str().to_string(), e.path.to_string()))
            .collect();
        assert_eq!(documented, registry);
    }

    /// RT1 T3: realtime has no public OpenAPI document and its routes are not in this one
    /// (Studio calls the ticket route with a hand-written `fetch`).
    #[test]
    fn document_leaves_out_the_realtime_service() {
        assert!(
            ENDPOINTS.iter().any(|e| e.service == Service::Realtime),
            "the realtime routes must be declared for this guard to mean anything"
        );
        let doc = document();
        for (method, path, _) in operations(&doc) {
            assert!(!path.contains("realtime"), "{method} {path}");
        }
    }

    #[test]
    fn operation_ids_are_the_contract() {
        let doc = document();
        let mut ids: Vec<(String, String)> = operations(&doc)
            .into_iter()
            .map(|(method, path, op)| {
                (
                    format!("{method} {path}"),
                    op["operationId"].as_str().unwrap_or("").to_string(),
                )
            })
            .collect();
        ids.sort();
        let expected = [
            ("DELETE /v1/databases/{id}", "deleteDatabase"),
            (
                "DELETE /v1/databases/{id}/branches/{branch_id}",
                "deleteBranch",
            ),
            (
                "DELETE /v1/databases/{id}/branches/{branch_id}/guard-exemptions/{role}",
                "deleteGuardExemption",
            ),
            (
                "DELETE /v1/databases/{id}/branches/{branch_id}/keys/{key_id}",
                "revokeKey",
            ),
            (
                "DELETE /v1/databases/{id}/branches/{branch_id}/pg-databases/{name}/data-api",
                "disableDataApi",
            ),
            (
                "DELETE /v1/databases/{id}/branches/{branch_id}/roles/{role}",
                "deleteRole",
            ),
            // Members hub T8 (S14).
            ("DELETE /v1/databases/{id}/personal-logins/{member_id}", "deletePersonalLogin"),
            ("DELETE /v1/databases/{id}/sql-queries/{query_id}", "deleteSavedQuery"),
            ("DELETE /v1/invites/{invite_id}", "revokeInvite"),
            ("DELETE /v1/members/{member_id}", "removeMember"),
            ("DELETE /v1/projects/{id}", "deleteProject"),
            ("DELETE /v1/projects/{id}/app-users", "disableAppUsers"),
            ("DELETE /v1/projects/{id}/grants/{member_id}", "deleteProjectGrant"),
            (
                "DELETE /v1/projects/{id}/grants/{member_id}/overrides",
                "deleteGrantOverride",
            ),
            ("DELETE /v1/projects/{id}/users/{user_id}", "deleteAppUser"),
            ("DELETE /v1/projects/{id}/users/{user_id}/sessions", "revokeAppUserSessions"),
            ("DELETE /v1/teams/{team_id}", "deleteTeam"),
            ("DELETE /v1/teams/{team_id}/grants/{project_id}", "deleteTeamGrant"),
            ("DELETE /v1/teams/{team_id}/members/{member_id}", "deleteTeamMember"),
            ("GET /v1/access-events", "listAccessEvents"),
            ("GET /v1/databases/{id}", "getDatabase"),
            ("GET /v1/databases/{id}/branches", "listBranches"),
            (
                "GET /v1/databases/{id}/branches/{branch_id}/connection",
                "getBranchConnection",
            ),
            (
                "GET /v1/databases/{id}/branches/{branch_id}/console/schema",
                "getConsoleSchema",
            ),
            (
                "GET /v1/databases/{id}/branches/{branch_id}/data-api/schema",
                "getDataApiSchema",
            ),
            (
                "GET /v1/databases/{id}/branches/{branch_id}/data-api/types",
                "getDataApiTypes",
            ),
            (
                "GET /v1/databases/{id}/branches/{branch_id}/databases",
                "listBranchDatabases",
            ),
            (
                "GET /v1/databases/{id}/branches/{branch_id}/guard-exemptions",
                "listGuardExemptions",
            ),
            (
                "GET /v1/databases/{id}/branches/{branch_id}/keys",
                "listKeys",
            ),
            ("GET /v1/databases/{id}/branches/{branch_id}/objects", "listObjects"),
            (
                "GET /v1/databases/{id}/branches/{branch_id}/pg-databases/{name}/privileges",
                "getPrivileges",
            ),
            ("GET /v1/databases/{id}/branches/{branch_id}/review", "getBranchReview"),
            (
                "GET /v1/databases/{id}/branches/{branch_id}/roles",
                "listRoles",
            ),
            (
                "GET /v1/databases/{id}/branches/{branch_id}/roles/{role}/delete-preview",
                "previewRoleDelete",
            ),
            (
                "GET /v1/databases/{id}/branches/{branch_id}/roles/{role}/sessions",
                "listRoleSessions",
            ),
            (
                "GET /v1/databases/{id}/branches/{branch_id}/sql/schema",
                "getSqlSchema",
            ),
            ("GET /v1/databases/{id}/branches/{branch_id}/tables/{schema}/{table}", "getTable"),
            ("GET /v1/databases/{id}/branches/{branch_id}/types", "listTypes"),
            ("GET /v1/databases/{id}/connection", "getDatabaseConnection"),
            ("GET /v1/databases/{id}/merges", "listMerges"),
            ("GET /v1/databases/{id}/merges/{merge_id}", "getMerge"),
            ("GET /v1/databases/{id}/merges/{merge_id}/plan.sql", "exportMergePlan"),
            ("GET /v1/databases/{id}/personal-logins", "listDatabasePersonalLogins"),
            ("GET /v1/databases/{id}/sql-queries", "listSavedQueries"),
            ("GET /v1/databases/{id}/sql-queries/{query_id}", "getSavedQuery"),
            ("GET /v1/instance/settings", "getInstanceSettings"),
            ("GET /v1/invites/delivery", "getInviteDelivery"),
            ("GET /v1/me", "getMe"),
            ("GET /v1/members", "listMembers"),
            ("GET /v1/members/{member_id}/personal-logins", "listMemberPersonalLogins"),
            ("GET /v1/members/{member_id}/teams", "listMemberTeams"),
            ("GET /v1/projects", "listProjects"),
            ("GET /v1/projects/{id}", "getProject"),
            ("GET /v1/projects/{id}/access-events", "listProjectAccessEvents"),
            ("GET /v1/projects/{id}/app-users", "getAppUsers"),
            ("GET /v1/projects/{id}/grants", "listProjectGrants"),
            ("GET /v1/projects/{id}/team-grants", "listProjectTeamGrants"),
            ("GET /v1/projects/{id}/users", "listAppUsers"),
            ("GET /v1/projects/{id}/users/{user_id}", "getAppUser"),
            ("GET /v1/projects/{id}/users/{user_id}/sessions", "listAppUserSessions"),
            ("GET /v1/teams", "listTeams"),
            ("GET /v1/teams/{team_id}", "getTeam"),
            ("PATCH /v1/databases/{id}", "updateDatabase"),
            (
                "PATCH /v1/databases/{id}/branches/{branch_id}",
                "updateBranch",
            ),
            (
                "PATCH /v1/databases/{id}/branches/{branch_id}/keys/{key_id}",
                "updateKey",
            ),
            (
                "PATCH /v1/databases/{id}/branches/{branch_id}/pg-databases/{name}",
                "updatePgDatabase",
            ),
            (
                "PATCH /v1/databases/{id}/branches/{branch_id}/roles/{role}",
                "updateRole",
            ),
            ("PATCH /v1/databases/{id}/merges/{merge_id}", "updateMerge"),
            ("PATCH /v1/databases/{id}/sql-queries/{query_id}", "updateSavedQuery"),
            ("PATCH /v1/instance/settings", "updateInstanceSettings"),
            ("PATCH /v1/members/{member_id}", "updateMember"),
            ("PATCH /v1/projects/{id}", "updateProject"),
            ("PATCH /v1/projects/{id}/users/{user_id}", "updateAppUser"),
            ("PATCH /v1/teams/{team_id}", "updateTeam"),
            ("PATCH /v1/tenant", "updateTenant"),
            ("POST /v1/databases", "createDatabase"),
            ("POST /v1/databases/{id}/branches", "createBranch"),
            (
                "POST /v1/databases/{id}/branches/{branch_id}/console/query",
                "consoleQuery",
            ),
            (
                "POST /v1/databases/{id}/branches/{branch_id}/keys",
                "createKey",
            ),
            (
                "POST /v1/databases/{id}/branches/{branch_id}/keys/{key_id}/rotate",
                "rotateKey",
            ),
            (
                "POST /v1/databases/{id}/branches/{branch_id}/merges",
                "createMerge",
            ),
            (
                "POST /v1/databases/{id}/branches/{branch_id}/pg-databases/{name}/data-api",
                "enableDataApi",
            ),
            (
                "POST /v1/databases/{id}/branches/{branch_id}/reset-password",
                "resetBranchPassword",
            ),
            (
                "POST /v1/databases/{id}/branches/{branch_id}/reset-to-parent",
                "resetBranchToParent",
            ),
            (
                "POST /v1/databases/{id}/branches/{branch_id}/restore",
                "restoreBranch",
            ),
            (
                "POST /v1/databases/{id}/branches/{branch_id}/review/acknowledge",
                "acknowledgeBranchReview",
            ),
            (
                "POST /v1/databases/{id}/branches/{branch_id}/review/fix",
                "fixBranchReviewItem",
            ),
            (
                "POST /v1/databases/{id}/branches/{branch_id}/roles",
                "createRole",
            ),
            (
                "POST /v1/databases/{id}/branches/{branch_id}/roles/{role}/reset-password",
                "resetRolePassword",
            ),
            ("POST /v1/databases/{id}/branches/{branch_id}/schema-changes", "applySchemaChanges"),
            ("POST /v1/databases/{id}/branches/{branch_id}/sql", "runSql"),
            (
                "POST /v1/databases/{id}/branches/{branch_id}/sql/runs/{run_id}/cancel",
                "cancelSqlRun",
            ),
            ("POST /v1/databases/{id}/branches/{branch_id}/tables/{schema}/{table}/cell", "getCell"),
            ("POST /v1/databases/{id}/branches/{branch_id}/tables/{schema}/{table}/rows/default", "insertDefaultRow"),
            ("POST /v1/databases/{id}/merges/{merge_id}/abandon", "abandonMerge"),
            ("POST /v1/databases/{id}/merges/{merge_id}/apply", "applyMerge"),
            ("POST /v1/databases/{id}/merges/{merge_id}/cancel", "cancelMerge"),
            ("POST /v1/databases/{id}/merges/{merge_id}/finish", "finishMerge"),
            ("POST /v1/databases/{id}/merges/{merge_id}/recompute", "recomputeMerge"),
            ("POST /v1/databases/{id}/merges/{merge_id}/rehearse", "rehearseMerge"),
            ("POST /v1/databases/{id}/merges/{merge_id}/retry", "retryMerge"),
            (
                "POST /v1/databases/{id}/personal-login/reset-password",
                "resetPersonalLoginPassword",
            ),
            (
                "POST /v1/databases/{id}/reset-password",
                "resetDatabasePassword",
            ),
            ("POST /v1/databases/{id}/sql-queries", "createSavedQuery"),
            ("POST /v1/invites", "createInvite"),
            ("POST /v1/invites/{invite_id}/resend", "resendInvite"),
            ("POST /v1/members/{member_id}/approve", "approveMember"),
            ("POST /v1/members/{member_id}/decline", "declineMember"),
            ("POST /v1/members/{member_id}/reinstate", "reinstateMember"),
            ("POST /v1/members/{member_id}/revoke-postgres-access", "revokeMemberPostgresAccess"),
            ("POST /v1/members/{member_id}/suspend", "suspendMember"),
            ("POST /v1/projects", "createProject"),
            ("POST /v1/projects/{id}/invites", "createProjectInvite"),
            ("POST /v1/projects/{id}/users", "createAppUser"),
            ("POST /v1/projects/{id}/users/{user_id}/deactivate", "deactivateAppUser"),
            ("POST /v1/projects/{id}/users/{user_id}/reactivate", "reactivateAppUser"),
            ("POST /v1/teams", "createTeam"),
            (
                "PUT /v1/databases/{id}/branches/{branch_id}/guard-exemptions/{role}",
                "putGuardExemption",
            ),
            ("PUT /v1/databases/{id}/personal-login", "createPersonalLogin"),
            ("PUT /v1/projects/{id}/app-users", "putAppUsers"),
            ("PUT /v1/projects/{id}/grants/{member_id}", "putProjectGrant"),
            (
                "PUT /v1/projects/{id}/grants/{member_id}/overrides",
                "putGrantOverride",
            ),
            ("PUT /v1/teams/{team_id}/grants/{project_id}", "putTeamGrant"),
            ("PUT /v1/teams/{team_id}/members/{member_id}", "putTeamMember"),
        ];
        let expected: Vec<(String, String)> = expected
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        assert_eq!(ids, expected);
        let unique: BTreeSet<_> = ids.iter().map(|(_, id)| id).collect();
        assert_eq!(unique.len(), ids.len(), "operation ids are unique");
        for (_, id) in &ids {
            assert!(
                id.chars().next().is_some_and(|c| c.is_ascii_lowercase())
                    && id.chars().all(|c| c.is_ascii_alphanumeric()),
                "{id} is camelCase"
            );
        }
    }

    #[test]
    fn every_response_has_a_description_and_errors_use_the_envelope() {
        let doc = document();
        assert!(!operations(&doc).is_empty(), "the document has operations");
        assert_eq!(doc["info"]["title"], "Chelabase Management API");
        assert_eq!(doc["info"]["version"], "1");
        assert_eq!(doc["info"]["license"]["name"], "Apache-2.0");
        assert_eq!(
            doc["components"]["securitySchemes"]["bearer"]["scheme"],
            "bearer"
        );
        let tags = ["databases", "branches", "keys", "data-api", "roles", "me", "projects", "app-users", "sql-editor", "merges", "members", "tables", "teams"];
        for (method, path, op) in operations(&doc) {
            let at = format!("{method} {path}");
            assert_eq!(
                op["security"],
                serde_json::json!([{ "bearer": [] }]),
                "{at} security"
            );
            let op_tags = op["tags"]
                .as_array()
                .unwrap_or_else(|| panic!("{at} has no tags"));
            assert!(
                op_tags.len() == 1 && tags.contains(&op_tags[0].as_str().unwrap()),
                "{at} tag"
            );
            let responses = op["responses"].as_object().unwrap();
            assert!(responses.contains_key("401"), "{at} documents 401");
            assert!(responses.contains_key("500"), "{at} documents 500");
            for (status, response) in responses {
                let description = response["description"].as_str().unwrap_or("");
                assert!(!description.is_empty(), "{at} {status} has a description");
                if status.parse::<u16>().unwrap() >= 400 {
                    assert_eq!(
                        response["content"]["application/json"]["schema"]["$ref"],
                        "#/components/schemas/ErrorEnvelope",
                        "{at} {status} uses the error envelope"
                    );
                }
            }
        }
    }

    fn operation<'a>(doc: &'a Value, id: &str) -> &'a Value {
        operations(doc)
            .into_iter()
            .find(|(_, _, op)| op["operationId"] == id)
            .unwrap_or_else(|| panic!("no operation {id}"))
            .2
    }

    // T14 F3 fix round (M7): the branch rewrites say a merge holding the branch refuses them,
    // and what the 409 carries.
    #[test]
    fn branch_rewrites_document_the_merge_hold() {
        let doc = document();
        for id in ["resetBranchToParent", "restoreBranch", "deleteBranch"] {
            let conflict = operation(&doc, id)["responses"]["409"]["description"]
                .as_str()
                .unwrap();
            for needle in ["`branch_merge_busy`", "`error.merge_id`", "`error.rehearsal_branch_id`", "abandon"] {
                assert!(conflict.contains(needle), "{id} lacks {needle}: {conflict}");
            }
        }
    }

    // T14 F3 fix round (M11): a membership list never revokes `pg_*` or Chelabase's own groups.
    #[test]
    fn update_role_documents_the_memberships_that_stay() {
        let doc = document();
        let description = operation(&doc, "updateRole")["description"].as_str().unwrap();
        assert!(!description.contains("replaces the whole list"), "{description}");
        assert!(description.contains("`pg_*` and Chelabase's own groups stay"), "{description}");
        let field = doc["components"]["schemas"]["UpdateRoleRequest"]["properties"]["member_of"]
            ["description"]
            .as_str()
            .unwrap();
        assert!(field.contains("`pg_*` and Chelabase's own groups stay"), "{field}");
    }

    // T10 fix (preview, then confirm): the delete takes `confirm` and documents the 409; the
    // preview takes `successor`, answers `RoleDeletePreview`, and refuses as the delete does.
    #[test]
    fn role_delete_documents_preview_and_confirm() {
        let doc = document();
        let params = |id: &str| -> Vec<String> {
            operation(&doc, id)["parameters"]
                .as_array()
                .unwrap_or_else(|| panic!("{id} has no parameters"))
                .iter()
                .map(|p| p["name"].as_str().unwrap().to_string())
                .collect()
        };
        let delete = params("deleteRole");
        assert!(delete.contains(&"successor".to_string()) && delete.contains(&"confirm".to_string()), "{delete:?}");
        let conflict = operation(&doc, "deleteRole")["responses"]["409"]["description"].as_str().unwrap();
        for needle in ["`exposure_unconfirmed`", "`exposures`", "`confirm`"] {
            assert!(conflict.contains(needle), "deleteRole 409 lacks {needle}: {conflict}");
        }

        let preview = params("previewRoleDelete");
        assert!(preview.contains(&"successor".to_string()), "{preview:?}");
        assert!(!preview.contains(&"confirm".to_string()), "{preview:?}");
        let op = operation(&doc, "previewRoleDelete");
        assert_eq!(op["tags"], serde_json::json!(["roles"]));
        assert_eq!(
            op["responses"]["200"]["content"]["application/json"]["schema"]["$ref"],
            "#/components/schemas/RoleDeletePreview"
        );
        for status in ["403", "404", "409", "422", "503"] {
            assert!(op["responses"].get(status).is_some(), "previewRoleDelete documents {status}");
        }
        let unprocessable = op["responses"]["422"]["description"].as_str().unwrap();
        assert!(unprocessable.contains("`invalid_successor`"), "{unprocessable}");
        let schema = &doc["components"]["schemas"]["RoleDeletePreview"];
        let mut fields: Vec<&str> = schema["properties"].as_object().unwrap().keys().map(String::as_str).collect();
        fields.sort();
        assert_eq!(fields, ["confirm", "exposures", "successor"], "{schema}");
        let exposure = &doc["components"]["schemas"]["RoleExposure"];
        let mut fields: Vec<&str> = exposure["properties"].as_object().unwrap().keys().map(String::as_str).collect();
        fields.sort();
        assert_eq!(fields, ["database", "grantees", "identity", "kind"], "{exposure}");
        assert_eq!(
            doc["components"]["schemas"]["RoleExposureKind"]["enum"],
            serde_json::json!(["function", "procedure", "view", "materialized_view"])
        );
    }

    /// Schema names become the TypeScript type names of the public client: clear and unique.
    #[test]
    fn schema_names_are_api_facing() {
        let doc = document();
        let names: Vec<&str> = doc["components"]["schemas"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        let expected = [
            "AccessEvent",
            "AccessEventPage",
            "AccessLevel",
            "AccessVia",
            "AcknowledgeReviewRequest",
            "AfterMerge",
            "AllRoles",
            "AllRows",
            "AppUser",
            "AppUserPage",
            "AppUserSession",
            "AppUserState",
            "AppUsersSettings",
            "AppUsersStatusView",
            "AppUsersTable",
            "ApplyRequest",
            "ApproveMemberRequest",
            "ArgDesc",
            "Branch",
            "BranchDatabaseView",
            "BranchId",
            "BranchKind",
            "BranchRole",
            "BranchStatus",
            "CascadeConfirm",
            "CellRequest",
            "CellValue",
            "ChoiceBody",
            "CollationRef",
            "Column",
            "ColumnChange",
            "ColumnDesc",
            "ColumnPrivileges",
            "ColumnSpec",
            "CommentChange",
            "ConnectionDetails",
            "ConsoleRequest",
            "Constraint",
            "ConstraintKind",
            "ConstraintKindSpec",
            "ConstraintSpec",
            "CreateAppUserRequest",
            "CreateBranchRequest",
            "CreateDatabaseAccepted",
            "CreateDatabaseRequest",
            "CreateInviteRequest",
            "CreateKeyRequest",
            "CreateMergeRequest",
            "CreateProjectInviteRequest",
            "CreateProjectRequest",
            "CreateRoleRequest",
            "CreateSavedQueryRequest",
            "CreateTable",
            "CreateTeamRequest",
            "CreatedDataKey",
            "DataApiExposure",
            "DataApiSchema",
            "DataApiState",
            "DataKey",
            "DataKeyKind",
            "Database",
            "DatabaseId",
            "DatabaseOrigin",
            "DatabaseSettings",
            "DatabaseStatus",
            "DatabaseSummary",
            "DefaultChange",
            "DefaultObject",
            "DefaultPrivileges",
            "DefaultRow",
            "DefaultRowRequest",
            "DeleteProjectRequest",
            "DeletedRole",
            "Delivery",
            "DeliveryReason",
            "Dependent",
            "Dependents",
            "Direction",
            "EnumPosition",
            "ErrorBody",
            "ErrorCode",
            "ErrorEnvelope",
            "ExclusionElement",
            "ExpectedAcl",
            "FinishRequest",
            "FixAction",
            "FkAction",
            "ForeignKeyTarget",
            "FunctionDesc",
            "FunctionRef",
            "Grant",
            "GrantOverride",
            "Grantee",
            "GuardExemptions",
            "Identity",
            "IdentityChange",
            "IdentityGeneration",
            "IdentityKind",
            "IndexKey",
            "IndexSpec",
            "InstanceSettings",
            "Invite",
            "InviteDelivery",
            "InviteGrant",
            "InviteOutcome",
            "InviteResult",
            "LockLevel",
            "MeAdmin",
            "MeDatabase",
            "MeMember",
            "MeProject",
            "MeResponse",
            "MeRoles",
            "MeUser",
            "Member",
            "MemberAccess",
            "MemberProjectAccess",
            "MemberStatusView",
            "Merge",
            "MergeDetail",
            "MergeLogEntry",
            "MergeOptionsBody",
            "MergeState",
            "MergeSummary",
            "NamedRole",
            "NullsOrder",
            "ObjectKind",
            "ObjectList",
            "ObjectPrivileges",
            "ObjectSummary",
            "Offered",
            "OnDelete",
            "Operation",
            "OperationAccepted",
            "OperatorRef",
            "PasswordState",
            "PersonalLoginList",
            "PersonalLoginRevokeReason",
            "PersonalLoginSecret",
            "PersonalLoginStatus",
            "PersonalLoginView",
            "PlatformRole",
            "PointInTime",
            "Policy",
            "PolicyCommand",
            "PolicyCommandSpec",
            "PolicySpec",
            "Privilege",
            "PrivilegeName",
            "PrivilegeObjectKind",
            "PrivilegeTarget",
            "PrivilegesPage",
            "Project",
            "ProjectGrant",
            "ProjectId",
            "ProjectInfo",
            "ProjectStatusView",
            "ProjectTeamGrantView",
            "PutAppUsersRequest",
            "PutGrantOverrideRequest",
            "PutGrantRequest",
            "PutPersonalLoginRequest",
            "ReassignedObjects",
            "RecomputeRequest",
            "RehearseRequest",
            "Relation",
            "ResendInviteRequest",
            "ResentInvite",
            "ResolutionBody",
            "RestoreAccepted",
            "RestoreRequest",
            "ReturnDesc",
            "Review",
            "ReviewFixRequest",
            "ReviewFixResponse",
            "ReviewItem",
            "ReviewItemKind",
            "ReviewReason",
            "ReviewReasonKind",
            "RevokeResult",
            "RlsSpec",
            // T10 fix: a role delete's preview.
            "RoleDeletePreview",
            "RoleExposure",
            "RoleExposureKind",
            "RoleInUseKey",
            "RoleLevel",
            "RoleOrigin",
            "RoleRef",
            "RoleSessionView",
            "RoleSessions",
            "RunSqlRequest",
            "SavedQuery",
            "SavedQuerySummary",
            "SchemaChangeRequest",
            "SchemaChangeResponse",
            "SchemaEntry",
            "SchemaPrivilege",
            "SchemaPrivileges",
            "SequenceParams",
            "SequencePrivilege",
            "SkipReason",
            "StatementResult",
            "Table",
            "TableDesc",
            "TableGrant",
            "TableIndex",
            "TableKind",
            "TablePrivilege",
            "TableRef",
            "Team",
            "TeamGrantRequest",
            "TeamProjectAccess",
            "Tenant",
            "TenantId",
            "TenantName",
            "TypeCatalog",
            "TypeEntry",
            "TypeKind",
            "TypeSpec",
            "UpdateAppUserRequest",
            "UpdateBranchRequest",
            "UpdateDatabaseRequest",
            "UpdateInstanceSettingsRequest",
            "UpdateKeyRequest",
            "UpdateMemberRequest",
            "UpdateMergeRequest",
            "UpdatePgDatabaseRequest",
            "UpdateProjectRequest",
            "UpdateRoleRequest",
            "UpdateSavedQueryRequest",
            "UpdateTeamRequest",
            "UpdateTenantRequest",
            "UserStepBody",
        ];
        // The names are map keys, so a clash between two types would drop one from this list.
        assert_eq!(names, expected);
        for placeholder in [
            "Empty", "Body", "Response", "Request", "Data", "Error", "Item",
        ] {
            assert!(
                !names.contains(&placeholder),
                "{placeholder} is a placeholder name"
            );
        }
    }
}
