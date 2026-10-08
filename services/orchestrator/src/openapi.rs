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
    doc
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
        user: &str,
    ) -> Result<Vec<SavedQuerySummary>, MetadataError> {
        match *self {}
    }

    async fn get_saved_query(
        &self,
        database: Uuid,
        user: &str,
        id: Uuid,
    ) -> Result<Option<SavedQuery>, MetadataError> {
        match *self {}
    }

    async fn create_saved_query(
        &self,
        database: Uuid,
        user: &str,
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
        user: &str,
        id: Uuid,
        version: i32,
        patch: &SavedQueryPatch,
    ) -> Result<SavedQueryWrite, MetadataError> {
        match *self {}
    }

    async fn delete_saved_query(
        &self,
        database: Uuid,
        user: &str,
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
    async fn grant_data_role_to_owner(
        &self,
        branch: BranchId,
        role: &str,
        owner: &str,
    ) -> Result<CheckDataRoleReply, ResetError> {
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
    async fn delete_role(&self, request: RoleRequest) -> Result<RoleReply, ResetError> {
        match *self {}
    }
    async fn role_sessions(&self, request: RoleRequest) -> Result<RoleSessionsReply, ResetError> {
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
            ("DELETE /v1/databases/{id}/sql-queries/{query_id}", "deleteSavedQuery"),
            ("DELETE /v1/projects/{id}", "deleteProject"),
            ("DELETE /v1/projects/{id}/app-users", "disableAppUsers"),
            ("DELETE /v1/projects/{id}/users/{user_id}", "deleteAppUser"),
            ("DELETE /v1/projects/{id}/users/{user_id}/sessions", "revokeAppUserSessions"),
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
                "GET /v1/databases/{id}/branches/{branch_id}/databases",
                "listBranchDatabases",
            ),
            (
                "GET /v1/databases/{id}/branches/{branch_id}/keys",
                "listKeys",
            ),
            (
                "GET /v1/databases/{id}/branches/{branch_id}/roles",
                "listRoles",
            ),
            (
                "GET /v1/databases/{id}/branches/{branch_id}/roles/{role}/sessions",
                "listRoleSessions",
            ),
            (
                "GET /v1/databases/{id}/branches/{branch_id}/sql/schema",
                "getSqlSchema",
            ),
            ("GET /v1/databases/{id}/connection", "getDatabaseConnection"),
            ("GET /v1/databases/{id}/merges", "listMerges"),
            ("GET /v1/databases/{id}/merges/{merge_id}", "getMerge"),
            ("GET /v1/databases/{id}/merges/{merge_id}/plan.sql", "exportMergePlan"),
            ("GET /v1/databases/{id}/sql-queries", "listSavedQueries"),
            ("GET /v1/databases/{id}/sql-queries/{query_id}", "getSavedQuery"),
            ("GET /v1/me", "getMe"),
            ("GET /v1/projects", "listProjects"),
            ("GET /v1/projects/{id}", "getProject"),
            ("GET /v1/projects/{id}/app-users", "getAppUsers"),
            ("GET /v1/projects/{id}/users", "listAppUsers"),
            ("GET /v1/projects/{id}/users/{user_id}", "getAppUser"),
            ("GET /v1/projects/{id}/users/{user_id}/sessions", "listAppUserSessions"),
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
            ("PATCH /v1/projects/{id}", "updateProject"),
            ("PATCH /v1/projects/{id}/users/{user_id}", "updateAppUser"),
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
                "POST /v1/databases/{id}/branches/{branch_id}/roles",
                "createRole",
            ),
            (
                "POST /v1/databases/{id}/branches/{branch_id}/roles/{role}/reset-password",
                "resetRolePassword",
            ),
            ("POST /v1/databases/{id}/branches/{branch_id}/sql", "runSql"),
            (
                "POST /v1/databases/{id}/branches/{branch_id}/sql/runs/{run_id}/cancel",
                "cancelSqlRun",
            ),
            ("POST /v1/databases/{id}/merges/{merge_id}/abandon", "abandonMerge"),
            ("POST /v1/databases/{id}/merges/{merge_id}/apply", "applyMerge"),
            ("POST /v1/databases/{id}/merges/{merge_id}/cancel", "cancelMerge"),
            ("POST /v1/databases/{id}/merges/{merge_id}/finish", "finishMerge"),
            ("POST /v1/databases/{id}/merges/{merge_id}/recompute", "recomputeMerge"),
            ("POST /v1/databases/{id}/merges/{merge_id}/rehearse", "rehearseMerge"),
            ("POST /v1/databases/{id}/merges/{merge_id}/retry", "retryMerge"),
            (
                "POST /v1/databases/{id}/reset-password",
                "resetDatabasePassword",
            ),
            ("POST /v1/databases/{id}/sql-queries", "createSavedQuery"),
            ("POST /v1/projects", "createProject"),
            ("POST /v1/projects/{id}/users", "createAppUser"),
            ("POST /v1/projects/{id}/users/{user_id}/deactivate", "deactivateAppUser"),
            ("POST /v1/projects/{id}/users/{user_id}/reactivate", "reactivateAppUser"),
            ("PUT /v1/projects/{id}/app-users", "putAppUsers"),
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
        let tags = ["databases", "branches", "keys", "data-api", "roles", "me", "projects", "app-users", "sql-editor", "merges"];
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
            "AfterMerge",
            "AppUser",
            "AppUserPage",
            "AppUserSession",
            "AppUserState",
            "AppUsersSettings",
            "AppUsersStatusView",
            "AppUsersTable",
            "ApplyRequest",
            "ArgDesc",
            "Branch",
            "BranchDatabaseView",
            "BranchId",
            "BranchKind",
            "BranchRole",
            "BranchStatus",
            "ChoiceBody",
            "ColumnDesc",
            "ConnectionDetails",
            "ConsoleRequest",
            "CreateAppUserRequest",
            "CreateBranchRequest",
            "CreateDatabaseAccepted",
            "CreateDatabaseRequest",
            "CreateKeyRequest",
            "CreateMergeRequest",
            "CreateProjectRequest",
            "CreateRoleRequest",
            "CreateSavedQueryRequest",
            "CreatedDataKey",
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
            "DeleteProjectRequest",
            "Direction",
            "ErrorBody",
            "ErrorCode",
            "ErrorEnvelope",
            "FinishRequest",
            "FunctionDesc",
            "Me",
            "MeUser",
            "Merge",
            "MergeDetail",
            "MergeLogEntry",
            "MergeOptionsBody",
            "MergeState",
            "MergeSummary",
            "OnDelete",
            "OperationAccepted",
            "PasswordState",
            "PointInTime",
            "Privilege",
            "Project",
            "ProjectId",
            "ProjectInfo",
            "ProjectStatusView",
            "PutAppUsersRequest",
            "RecomputeRequest",
            "RehearseRequest",
            "ResolutionBody",
            "RestoreAccepted",
            "RestoreRequest",
            "ReturnDesc",
            "RoleInUseKey",
            "RoleLevel",
            "RoleOrigin",
            "RoleSessionView",
            "RoleSessions",
            "RunSqlRequest",
            "SavedQuery",
            "SavedQuerySummary",
            "SchemaEntry",
            "TableDesc",
            "TableKind",
            "Tenant",
            "TenantId",
            "UpdateAppUserRequest",
            "UpdateBranchRequest",
            "UpdateDatabaseRequest",
            "UpdateKeyRequest",
            "UpdateMergeRequest",
            "UpdatePgDatabaseRequest",
            "UpdateProjectRequest",
            "UpdateRoleRequest",
            "UpdateSavedQueryRequest",
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
