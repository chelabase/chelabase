//! Studio's SQL editor: runs scripts on a branch's database, streams results, serves the
//! schema for completion and keeps each user's saved queries.

use std::sync::Arc;

use utoipa_axum::router::OpenApiRouter;

use crate::{AppState, JobPort, MetadataPort};

pub mod events;
pub mod guard;
pub mod registry;
pub mod run;
pub mod saved;
pub mod schema;
mod routes;

pub const MAX_SQL_BYTES: usize = 1_048_576;
pub const MIN_TIMEOUT_MS: u64 = 100;
pub const MAX_TIMEOUT_MS: u64 = 900_000;
pub const DEFAULT_TIMEOUT_MS: u64 = 60_000;
pub const MAX_ROWS_PER_STATEMENT: usize = 1_000;
pub const ROWS_PER_EVENT: usize = 200;
pub const MAX_RESPONSE_BYTES: usize = 10 * 1_048_576;
pub const HEARTBEAT: std::time::Duration = std::time::Duration::from_secs(15);
pub const RUNS_PER_USER: usize = 4;
pub const RUNS_PER_TARGET: usize = 4;
pub const APPLICATION_NAME: &str = "chelabase-sql-editor";
pub const PGLS_VERSION: &str = "0.26.0";
pub const MAX_SCHEMA_BYTES: usize = 8 * 1_048_576;
pub use chelabase_metadata::SAVED_QUERY_CAP;
pub const DATA_ROLE_GRANT_TTL: std::time::Duration = std::time::Duration::from_secs(300);

/// Why a script can't be run or saved, or `None`: 1 byte to 1 MiB, and no NUL byte (the
/// wire protocol can't carry one).
pub(crate) fn invalid_sql(sql: &str) -> Option<&'static str> {
    if sql.is_empty() || sql.len() > MAX_SQL_BYTES {
        Some("`sql` must be 1 byte to 1 MiB")
    } else if sql.contains('\0') {
        Some("`sql` must not contain a NUL byte")
    } else {
        None
    }
}

/// The editor's state on `AppState`.
pub struct SqlEditor {
    pub(crate) runs: registry::RunRegistry,
    pub(crate) granted: moka::sync::Cache<(chelabase_types::BranchId, String), ()>,
}

impl SqlEditor {
    /// The runs in flight. Public for the integration tests, which hold slots directly.
    pub fn runs(&self) -> &registry::RunRegistry {
        &self.runs
    }
}

impl Default for SqlEditor {
    fn default() -> Self {
        Self {
            runs: registry::RunRegistry::default(),
            granted: moka::sync::Cache::builder()
                .max_capacity(10_000)
                .time_to_live(DATA_ROLE_GRANT_TTL)
                .build(),
        }
    }
}

pub(crate) fn routes<M: MetadataPort, J: JobPort>() -> OpenApiRouter<Arc<AppState<M, J>>> {
    routes::routes::<M, J>().merge(saved::routes::<M, J>())
}

#[cfg(test)]
mod tests {
    use super::PGLS_VERSION;

    #[test]
    fn pgls_version_matches_studio() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../apps/studio/package.json");
        let text = std::fs::read_to_string(path).expect("Studio's package.json");
        let package: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(
            package["dependencies"]["@postgres-language-server/wasm"],
            PGLS_VERSION,
            "Studio's pgls pin and PGLS_VERSION differ"
        );
    }
}
