//! Schema changes in the decoded stream (R2, W10): a DDL in a data-API database bumps
//! `chelabase.schema_version` in the DDL's own transaction, so it arrives as a change of that
//! table. A window is a schema change when it holds an `all` transaction or any change of the
//! relation whose OID is the database's `chelabase.schema_version` (matched by OID, resolved per
//! database, never by the `s`/`t` names a message carries). A schema change pauses the
//! database's channels, reruns every group's probe and re-resolves the channels' OIDs **before**
//! any key of that window is released.

use crate::merge::Window;

/// The OID of the database's `chelabase.schema_version` (`None`: the database has none),
/// resolved through the catalog, never `search_path`.
pub const VERSION_OID_SQL: &str =
    "SELECT pg_catalog.to_regclass('chelabase.schema_version')::pg_catalog.oid";

/// Whether `window` changes the schema of the database whose version table has `version_oid`.
pub fn is_schema_change(window: &Window, version_oid: Option<u32>) -> bool {
    window.all || version_oid.is_some_and(|v| window.rels.iter().any(|r| r.rel == v))
}
