//! Names derived from a database: its Postgres database name and data volume.

use cheladb_types::DatabaseId;

const VOLUME_PREFIX: &str = "cheladb-db-";

/// A Postgres database name from the user's name (see
/// [`cheladb_metadata::postgres_database_name`], which also rejects reserved
/// and over-long names; the orchestrator refuses those before queueing).
pub fn sanitize_db_name(name: &str) -> Result<String, String> {
    cheladb_metadata::postgres_database_name(name).map_err(|e| e.to_string())
}

/// The named Docker volume holding a database's data.
pub fn volume_name(id: DatabaseId) -> String {
    format!("{VOLUME_PREFIX}{id}")
}

pub fn database_id_from_volume(volume: &str) -> Option<DatabaseId> {
    let raw = volume.strip_prefix(VOLUME_PREFIX)?;
    uuid::Uuid::parse_str(raw).ok().map(DatabaseId)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes_like_before() {
        assert_eq!(sanitize_db_name("web-1 prod").unwrap(), "web_1_prod");
        assert_eq!(sanitize_db_name("---").unwrap(), "___");
        assert_eq!(sanitize_db_name("").unwrap(), "cheladb");
        assert!(sanitize_db_name("template1").is_err());
    }

    #[test]
    fn volume_names_round_trip() {
        let id = DatabaseId::new();
        assert_eq!(volume_name(id), format!("cheladb-db-{id}"));
        assert_eq!(database_id_from_volume(&volume_name(id)), Some(id));
        assert_eq!(database_id_from_volume("cheladb-db-not-a-uuid"), None);
        assert_eq!(database_id_from_volume("other-volume"), None);
    }
}
