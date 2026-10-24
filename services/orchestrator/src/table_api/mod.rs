//! The table API: structured reads of a branch's tables, run as the database owner, and its
//! DDL and enum changes (`POST …/schema-changes`), run as the owner or the role the caller acts as.

use std::sync::Arc;

use utoipa_axum::router::OpenApiRouter;

use crate::{AppState, JobPort, MetadataPort};

mod changes;
mod privilege_ops;
mod read;
mod rows;

pub use changes::{DEFAULT_DRY_RUN_MAX_BYTES, dry_run_max_bytes};
pub(crate) use read::{READ_TIMEOUT_SQL, read_failure};

pub(crate) fn routes<M: MetadataPort, J: JobPort>() -> OpenApiRouter<Arc<AppState<M, J>>> {
    read::routes::<M, J>()
        .merge(changes::routes::<M, J>())
        .merge(rows::routes::<M, J>())
}
