//! The Management API's OpenAPI document, generated from the handlers' `#[utoipa::path]`
//! annotations. `openapi/management.json` is the committed copy; `just api-gen` rewrites it.

use utoipa::openapi::OpenApi;
use utoipa::openapi::path::{Operation, PathItem};
use utoipa::openapi::security::{HttpAuthScheme, HttpBuilder, SecurityRequirement, SecurityScheme};
use utoipa::openapi::{InfoBuilder, LicenseBuilder};

use super::*;

/// The Management API's OpenAPI document.
pub fn openapi() -> OpenApi {
    let (_, mut doc) = api_router::<Doc, Doc>().split_for_parts();
    doc.info = InfoBuilder::new()
        .title("Cheladb Management API")
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
    async fn check_data_role(
        &self,
        branch_id: BranchId,
        role: &str,
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
    async fn delete_role(&self, request: RoleRequest) -> Result<RoleReply, ResetError> {
        match *self {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cheladb_routes::{ENDPOINTS, Service};
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
        if std::env::var_os("CHELADB_BLESS").is_some() {
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
                            | "orchestrator.api_docs"
                            | "orchestrator.api_docs_script"
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
            ("GET /v1/databases/{id}/connection", "getDatabaseConnection"),
            ("GET /v1/me", "getMe"),
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
            (
                "POST /v1/databases/{id}/reset-password",
                "resetDatabasePassword",
            ),
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
        assert_eq!(doc["info"]["title"], "Cheladb Management API");
        assert_eq!(doc["info"]["version"], "1");
        assert_eq!(doc["info"]["license"]["name"], "Apache-2.0");
        assert_eq!(
            doc["components"]["securitySchemes"]["bearer"]["scheme"],
            "bearer"
        );
        let tags = ["databases", "branches", "keys", "data-api", "roles", "me"];
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
            "ArgDesc",
            "Branch",
            "BranchDatabaseView",
            "BranchId",
            "BranchKind",
            "BranchRole",
            "BranchStatus",
            "ColumnDesc",
            "ConnectionDetails",
            "ConsoleRequest",
            "CreateBranchRequest",
            "CreateDatabaseAccepted",
            "CreateDatabaseRequest",
            "CreateKeyRequest",
            "CreateRoleRequest",
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
            "ErrorBody",
            "ErrorCode",
            "ErrorEnvelope",
            "FunctionDesc",
            "Me",
            "MeUser",
            "OperationAccepted",
            "PasswordState",
            "PointInTime",
            "Privilege",
            "Project",
            "ProjectId",
            "RestoreAccepted",
            "RestoreRequest",
            "ReturnDesc",
            "RoleInUseKey",
            "RoleLevel",
            "RoleOrigin",
            "SchemaEntry",
            "TableDesc",
            "TableKind",
            "Tenant",
            "TenantId",
            "UpdateBranchRequest",
            "UpdateDatabaseRequest",
            "UpdateKeyRequest",
            "UpdatePgDatabaseRequest",
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
