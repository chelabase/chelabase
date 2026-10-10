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
#[derive(Default)]
pub struct SqlEditor {
    pub(crate) runs: registry::RunRegistry,
}

impl SqlEditor {
    /// The runs in flight. Public for the integration tests, which hold slots directly.
    pub fn runs(&self) -> &registry::RunRegistry {
        &self.runs
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
