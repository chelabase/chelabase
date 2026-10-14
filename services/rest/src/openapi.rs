//! The data API's OpenAPI document, generated from the public handlers' `#[utoipa::path]`
//! annotations and the request grammar's schemas ([`chelabase_query::openapi`]).
//! `openapi/data.json` is the committed copy; `just api-gen` rewrites it. The internal
//! listener's routes aren't in it.

use chelabase_types::data_api_schema::DataApiSchema;
use serde::Serialize;
use utoipa::ToSchema;
use utoipa::openapi::security::{ApiKey, ApiKeyValue, HttpAuthScheme, HttpBuilder, SecurityScheme};
use utoipa::openapi::{InfoBuilder, LicenseBuilder, OpenApi};
use uuid::Uuid;

use crate::app::api_router;
use crate::conn::{ConnError, ConnInfo, ConnSource};

/// The data API's OpenAPI document.
pub fn openapi() -> OpenApi {
    let (_, mut doc) = api_router::<Doc>().split_for_parts();
    doc.info = InfoBuilder::new()
        .title("Chelabase Data API")
        .version("1")
        .license(Some(LicenseBuilder::new().name("Apache-2.0").build()))
        .build();
    let components = doc.components.get_or_insert_with(Default::default);
    components.add_security_scheme(
        "bearer",
        SecurityScheme::Http(
            HttpBuilder::new()
                .scheme(HttpAuthScheme::Bearer)
                .bearer_format("data API key")
                .description(Some(
                    "A data API key (`chk_pub_…` or `chk_sec_…`) as `Authorization: Bearer <key>`",
                ))
                .build(),
        ),
    );
    components.add_security_scheme(
        "user",
        SecurityScheme::ApiKey(ApiKey::Header(ApiKeyValue::with_description(
            "Chelabase-User",
            "An app user's access token, with a public key only: the query runs as `authenticated` with the user's claims",
        ))),
    );
    doc
}

/// A state type that can't exist: `openapi()` only needs the router's types, never a value.
enum Doc {}

impl ConnSource for Doc {
    async fn fetch(&self, _branch: Uuid) -> Result<ConnInfo, ConnError> {
        match *self {}
    }
}

/// `GET /data/v1/schema`'s body: the schema as the key's role sees it, and the databases
/// the key may reach.
#[derive(Serialize, ToSchema)]
pub struct DataSchemaResponse {
    #[serde(flatten)]
    pub schema: DataApiSchema,
    /// The served databases the key may reach (`database` on a query), main first.
    pub databases: Vec<String>,
}

/// Every data API error: `{"error": {...}}`.
#[derive(Serialize, ToSchema)]
pub struct DataErrorEnvelope {
    pub error: DataErrorBody,
}

#[derive(Serialize, ToSchema)]
pub struct DataErrorBody {
    pub code: DataErrorCode,
    /// What went wrong, for a person; not stable.
    pub message: String,
    /// Postgres's detail, on a mapped database error.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub detail: Option<String>,
    /// Postgres's hint, on a mapped database error.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub hint: Option<String>,
    /// The SQLSTATE, on a mapped database error.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub sqlstate: Option<String>,
}

/// The stable error code. Each response's description lists the codes its status carries.
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum DataErrorCode {
    InvalidQuery,
    UnknownColumn,
    InvalidValue,
    Raised,
    UserTokenWithSecretKey,
    Unauthorized,
    UserTokenInvalid,
    UserTokenExpired,
    UserSessionRevoked,
    PermissionDenied,
    RoleNotAllowed,
    UnknownTable,
    UnknownFunction,
    UnknownDatabase,
    Conflict,
    Retry,
    TooLarge,
    RateLimited,
    Internal,
    Unavailable,
    Timeout,
}

#[cfg(test)]
mod tests {
    use super::*;
    use chelabase_routes::{ENDPOINTS, Service};
    use serde_json::{Value, json};
    use std::collections::BTreeSet;

    const DOCUMENT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../openapi/data.json");
    const METHODS: [&str; 5] = ["get", "post", "put", "patch", "delete"];

    fn document() -> Value {
        serde_json::to_value(openapi()).unwrap()
    }

    /// Every operation as (method, path, operation object).
    fn operations(doc: &Value) -> Vec<(String, String, &Value)> {
        let mut out = Vec::new();
        for (path, item) in doc["paths"].as_object().into_iter().flatten() {
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
            .expect("openapi/data.json is missing; run `just api-gen`");
        assert!(
            committed == json,
            "openapi/data.json is stale; run `just api-gen` and commit it"
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
                e.service == Service::Rest && matches!(e.name, "rest.query" | "rest.schema")
            })
            .map(|e| (e.method.as_str().to_string(), e.path.to_string()))
            .collect();
        assert_eq!(
            registry.len(),
            2,
            "the registry's public data API endpoints"
        );
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
            ("GET /data/v1/schema", "getDataSchema"),
            ("POST /data/v1/query", "runQuery"),
        ]
        .map(|(at, id)| (at.to_string(), id.to_string()));
        assert_eq!(ids, expected);
    }

    #[test]
    fn every_response_has_a_description_and_errors_use_the_envelope() {
        let doc = document();
        assert_eq!(doc["info"]["title"], "Chelabase Data API");
        assert_eq!(doc["info"]["version"], "1");
        assert_eq!(doc["info"]["license"]["name"], "Apache-2.0");
        let schemes = &doc["components"]["securitySchemes"];
        assert_eq!(schemes["bearer"]["type"], "http");
        assert_eq!(schemes["bearer"]["scheme"], "bearer");
        assert_eq!(schemes["bearer"]["bearerFormat"], "data API key");
        assert_eq!(schemes["user"]["type"], "apiKey");
        assert_eq!(schemes["user"]["in"], "header");
        assert_eq!(schemes["user"]["name"], "Chelabase-User");
        let ops = operations(&doc);
        assert_eq!(ops.len(), 2, "the document has both operations");
        for (method, path, op) in ops {
            let at = format!("{method} {path}");
            let security = if method == "POST" {
                json!([{ "bearer": [] }, { "bearer": [], "user": [] }])
            } else {
                json!([{ "bearer": [] }])
            };
            assert_eq!(op["security"], security, "{at} security");
            assert_eq!(op["tags"], json!(["data"]), "{at} tag");
            let responses = op["responses"].as_object().unwrap();
            for status in ["200", "401", "500", "503"] {
                assert!(responses.contains_key(status), "{at} documents {status}");
            }
            for (status, response) in responses {
                let description = response["description"].as_str().unwrap_or("");
                assert!(!description.is_empty(), "{at} {status} has a description");
                if status.parse::<u16>().unwrap() >= 400 {
                    assert!(
                        description.starts_with('`'),
                        "{at} {status} starts with its codes"
                    );
                    assert_eq!(
                        response["content"]["application/json"]["schema"]["$ref"],
                        "#/components/schemas/DataErrorEnvelope",
                        "{at} {status} uses the error envelope"
                    );
                }
            }
            assert!(
                responses["503"]["headers"].get("Retry-After").is_some(),
                "{at} 503 has Retry-After"
            );
        }
    }

    /// Schema names become type names in generated clients: clear and unique.
    #[test]
    fn schema_names_are_api_facing() {
        let doc = document();
        let names: Vec<&str> = doc["components"]["schemas"]
            .as_object()
            .map(|m| m.keys().map(String::as_str).collect())
            .unwrap_or_default();
        let expected = [
            "AppUsersTable",
            "ArgDesc",
            "BatchCall",
            "BatchCount",
            "BatchDelete",
            "BatchInsert",
            "BatchRequest",
            "BatchSelect",
            "BatchUpdate",
            "CallRequest",
            "ColumnDesc",
            "CountRequest",
            "DataApiSchema",
            "DataErrorBody",
            "DataErrorCode",
            "DataErrorEnvelope",
            "DataSchemaResponse",
            "DeleteRequest",
            "Filter",
            "FunctionDesc",
            "InsertRequest",
            "OrderBy",
            "Privilege",
            "QueryRequest",
            "QueryResult",
            "ReturnDesc",
            "Returning",
            "SchemaEntry",
            "SelectItem",
            "SelectRequest",
            "TableDesc",
            "TableKind",
            "TableName",
            "UpdateRequest",
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
