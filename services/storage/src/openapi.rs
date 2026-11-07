//! The storage API's OpenAPI document, generated from the public handlers' `#[utoipa::path]`
//! annotations. `openapi/storage.json` is the committed copy; `just api-gen` rewrites it.

use serde::Serialize;
use utoipa::ToSchema;
use utoipa::openapi::security::{HttpAuthScheme, HttpBuilder, SecurityScheme};
use utoipa::openapi::{InfoBuilder, LicenseBuilder, OpenApi};
use uuid::Uuid;

use chelabase_pgsession::{ConnError, ConnInfo, ConnSource};

use crate::app::api_router;

/// The storage API's OpenAPI document.
pub fn openapi() -> OpenApi {
    let (_, mut doc) = api_router::<Doc, Doc>().split_for_parts();
    doc.info = InfoBuilder::new()
        .title("Chelabase Storage API")
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
                .bearer_format("data API key")
                .description(Some(
                    "A data API key (`chk_pub_…` or `chk_sec_…`) as `Authorization: Bearer <key>`",
                ))
                .build(),
        ),
    );
    doc
}

/// A state type that can't exist: `openapi()` only needs the router's types.
enum Doc {}

impl ConnSource for Doc {
    async fn fetch(&self, _branch: Uuid) -> Result<ConnInfo, ConnError> {
        match *self {}
    }
}

/// Every storage error: `{"error": {...}}`, the data API's envelope.
#[derive(Serialize, ToSchema)]
pub struct StorageErrorEnvelope {
    pub error: StorageErrorBody,
}

#[derive(Serialize, ToSchema)]
pub struct StorageErrorBody {
    /// Stable: `unauthorized`, `invalid_request`, `invalid_value`, `permission_denied`,
    /// `database_not_allowed`, `role_not_allowed`, `bucket_not_found`, `bucket_not_empty`,
    /// `conflict`, `storage_unavailable`, `unavailable`, `internal`.
    pub code: String,
    /// What went wrong, for a person; not stable.
    pub message: String,
    /// The SQLSTATE of a mapped database error.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sqlstate: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use chelabase_routes::{ENDPOINTS, Service};
    use std::collections::BTreeSet;

    const DOCUMENT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../openapi/storage.json");

    #[test]
    fn document_is_current() {
        let mut json = serde_json::to_string_pretty(&openapi()).unwrap();
        json.push('\n');
        if std::env::var_os("CHELABASE_BLESS").is_some() {
            std::fs::write(DOCUMENT, &json).unwrap();
        }
        let committed = std::fs::read_to_string(DOCUMENT)
            .expect("openapi/storage.json is missing; run `just api-gen`");
        assert!(
            committed == json,
            "openapi/storage.json is stale; run `just api-gen` and commit it"
        );
    }

    /// Every storage route the gateway routes is documented, and nothing else.
    #[test]
    fn document_matches_the_registry() {
        let doc = serde_json::to_value(openapi()).unwrap();
        let documented: BTreeSet<(String, String)> = doc["paths"]
            .as_object()
            .unwrap()
            .iter()
            .flat_map(|(path, ops)| {
                ops.as_object()
                    .unwrap()
                    .keys()
                    .map(move |m| (m.to_uppercase(), path.clone()))
            })
            .collect();
        let registry: BTreeSet<(String, String)> = ENDPOINTS
            .iter()
            .filter(|e| e.service == Service::Storage)
            .map(|e| (e.method.as_str().to_string(), e.path.to_string()))
            .collect();
        assert_eq!(documented, registry);
    }
}
