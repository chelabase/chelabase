//! Base snapshots for schema merges: what a branch's schema looked like when it last
//! agreed with its parent, per Chelabase database (`pg_database`).
//!
//! The bases are a history (E18): every write appends an entry, and the current base is the
//! newest one. Each entry records the branch's own position (`branch_timeline_id`,
//! `branch_lsn`) from which it is valid, so a restore to an earlier point takes the entry that
//! was current there (see [`MetadataClient::switch_to_restored_timeline`]).

use chelabase_types::BranchId;
use chrono::{DateTime, Utc};
use sqlx::postgres::PgRow;
use sqlx::{Postgres, Row, Transaction};

use crate::{MetadataClient, MetadataError};

/// Why a base was recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BaseSource {
    Create,
    Reset,
    Merge,
    Update,
    Rebaseline,
}

impl BaseSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Create => "create",
            Self::Reset => "reset",
            Self::Merge => "merge",
            Self::Update => "update",
            Self::Rebaseline => "rebaseline",
        }
    }

    pub fn parse(value: &str) -> Result<Self, MetadataError> {
        match value {
            "create" => Ok(Self::Create),
            "reset" => Ok(Self::Reset),
            "merge" => Ok(Self::Merge),
            "update" => Ok(Self::Update),
            "rebaseline" => Ok(Self::Rebaseline),
            other => Err(MetadataError::InvalidStatus(format!("base source {other}"))),
        }
    }
}

/// One entry of a branch's base history for one database.
#[derive(Debug, Clone, PartialEq)]
pub struct SchemaBase {
    /// The entry's place in the history (newest is highest). Ignored by
    /// [`MetadataClient::append_schema_base`], which assigns it.
    pub id: i64,
    pub branch_id: BranchId,
    pub database_name: String,
    /// `None` until read; the location says where to read it.
    pub model: Option<serde_json::Value>,
    pub model_version: Option<i32>,
    pub server_major: Option<i32>,
    pub source: BaseSource,
    /// The location the model was (or is to be) read at; `None` for a re-baseline.
    pub timeline_id: Option<String>,
    pub lsn: Option<String>,
    /// The branch's own position from which this base is valid: its timeline and an LSN
    /// (`X/Y`) on it. For create and reset, the new timeline and its start; for a merge or
    /// update with keep, the kept branch's timeline and its LSN when the merge finished.
    pub branch_timeline_id: Option<String>,
    pub branch_lsn: Option<String>,
    pub identity_map: serde_json::Value,
    pub taken_at: DateTime<Utc>,
    /// When the entry was written (a restore's copy: the restore). Set by the metadata;
    /// ignored by [`MetadataClient::append_schema_base`].
    pub recorded_at: DateTime<Utc>,
}

fn base_from_row(r: &PgRow) -> Result<SchemaBase, MetadataError> {
    Ok(SchemaBase {
        id: r.get("id"),
        branch_id: BranchId::from(r.get::<uuid::Uuid, _>("branch_id")),
        database_name: r.get("database_name"),
        model: r.get("model"),
        model_version: r.get("model_version"),
        server_major: r.get("server_major"),
        source: BaseSource::parse(r.get("source"))?,
        timeline_id: r.get("timeline_id"),
        lsn: r.get("lsn"),
        branch_timeline_id: r.get("branch_timeline_id"),
        branch_lsn: r.get("branch_lsn"),
        identity_map: r.get("identity_map"),
        taken_at: r.get("taken_at"),
        recorded_at: r.get("recorded_at"),
    })
}

/// How deep a restore walks back its chain of restored timelines; a guard only (each restore
/// adds one level, and the chain has no cycles).
const RESTORE_CHAIN_DEPTH: i32 = 10_000;

/// Inside a restore's switch (`tx`): records that `to_timeline` was branched from
/// `from_timeline` at `at` (`branch_restores`), then for each database of `branch` appends a
/// copy of the base that was current at that point (its model, map, location, source and
/// `taken_at`), positioned at (`to_timeline`, `at`).
///
/// The base current on a timeline at a point is its newest entry at or before the point. When
/// the timeline has none, the point lies before the timeline's own history (it was restored
/// from another timeline at `from_at`, and its entries start there), or its first entry is
/// later: the walk goes on to the timeline it was restored from at the earlier of the point and
/// `from_at`, at any depth. A database with no entry anywhere on the walk gets none, so a merge
/// offers re-baseline (E17). A restore made before `branch_restores` existed has no link, so
/// the walk stops at its timeline (accepted: such a branch re-baselines instead).
pub(crate) async fn copy_bases_for_restore(
    tx: &mut Transaction<'_, Postgres>,
    branch: BranchId,
    from_timeline: &str,
    at: &str,
    to_timeline: &str,
) -> Result<(), MetadataError> {
    sqlx::query(
        "INSERT INTO branch_restores (branch_id, timeline_id, from_timeline_id, at_lsn)
         VALUES ($1, $2, $3, $4)
         ON CONFLICT (branch_id, timeline_id) DO NOTHING",
    )
    .bind(branch.as_uuid())
    .bind(to_timeline)
    .bind(from_timeline)
    .bind(at)
    .execute(&mut **tx)
    .await?;
    sqlx::query(
        "WITH RECURSIVE walk (timeline, point, depth) AS (
             SELECT $2::text, $4::pg_lsn, 0
             UNION ALL
             SELECT r.from_timeline_id, LEAST(w.point, r.at_lsn::pg_lsn), w.depth + 1
             FROM walk w
             JOIN branch_restores r ON r.branch_id = $1 AND r.timeline_id = w.timeline
             WHERE w.depth < $5
         )
         INSERT INTO branch_schema_bases
             (branch_id, database_name, model, model_version, server_major, source,
              timeline_id, lsn, identity_map, taken_at, branch_timeline_id, branch_lsn)
         SELECT branch_id, database_name, model, model_version, server_major, source,
                timeline_id, lsn, identity_map, taken_at, $3, $4
         FROM (
             SELECT DISTINCT ON (s.database_name) s.*
             FROM walk w
             JOIN branch_schema_bases s
               ON s.branch_id = $1 AND s.branch_timeline_id = w.timeline
              AND s.branch_lsn::pg_lsn <= w.point
             ORDER BY s.database_name, w.depth, s.id DESC
         ) picked
         ORDER BY database_name",
    )
    .bind(branch.as_uuid())
    .bind(from_timeline)
    .bind(to_timeline)
    .bind(at)
    .bind(RESTORE_CHAIN_DEPTH)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

impl MetadataClient {
    /// Appends an entry to the branch's base history for one database (it becomes the
    /// current base); `taken_at` and `recorded_at` are now. Returns its id.
    ///
    /// In the same transaction (T14 M7), the models of this database's older entries that no
    /// restore can pick any more are dropped: an entry superseded on its own timeline by a newer
    /// one recorded more than the database's history window ago. A restore reaches back only
    /// within the window, so it finds that newer entry first. The entry itself stays (its
    /// location and identity map): should a restore still pick it, the model is read live from
    /// its location, or re-baseline is offered, never a wrong base.
    pub async fn append_schema_base(&self, b: &SchemaBase) -> Result<i64, MetadataError> {
        let mut tx = self.pool.begin().await?;
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO branch_schema_bases
                 (branch_id, database_name, model, model_version, server_major, source,
                  timeline_id, lsn, identity_map, branch_timeline_id, branch_lsn)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
             RETURNING id",
        )
        .bind(b.branch_id.as_uuid())
        .bind(&b.database_name)
        .bind(&b.model)
        .bind(b.model_version)
        .bind(b.server_major)
        .bind(b.source.as_str())
        .bind(&b.timeline_id)
        .bind(&b.lsn)
        .bind(&b.identity_map)
        .bind(&b.branch_timeline_id)
        .bind(&b.branch_lsn)
        .fetch_one(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE branch_schema_bases e
             SET model = NULL, model_version = NULL, server_major = NULL
             FROM branches br JOIN databases d ON d.id = br.database_id
             WHERE br.id = e.branch_id AND e.branch_id = $1 AND e.database_name = $2
               AND e.model IS NOT NULL
               AND EXISTS (SELECT 1 FROM branch_schema_bases f
                           WHERE f.branch_id = e.branch_id AND f.database_name = e.database_name
                             AND f.branch_timeline_id IS NOT DISTINCT FROM e.branch_timeline_id
                             AND f.id > e.id
                             AND f.recorded_at < now() - d.history_window)",
        )
        .bind(b.branch_id.as_uuid())
        .bind(&b.database_name)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(id)
    }

    /// The current base of (branch, database): the newest entry.
    pub async fn schema_base(
        &self,
        branch: BranchId,
        database_name: &str,
    ) -> Result<Option<SchemaBase>, MetadataError> {
        sqlx::query(
            "SELECT * FROM branch_schema_bases WHERE branch_id = $1 AND database_name = $2
             ORDER BY id DESC LIMIT 1",
        )
        .bind(branch.as_uuid())
        .bind(database_name)
        .fetch_optional(&self.pool)
        .await?
        .as_ref()
        .map(base_from_row)
        .transpose()
    }

    /// The base that was current on the branch's `timeline` at `lsn`: the newest entry
    /// positioned on `timeline` at or before `lsn` (compared as LSNs).
    pub async fn schema_base_at(
        &self,
        branch: BranchId,
        database_name: &str,
        timeline: &str,
        lsn: &str,
    ) -> Result<Option<SchemaBase>, MetadataError> {
        sqlx::query(
            "SELECT * FROM branch_schema_bases
             WHERE branch_id = $1 AND database_name = $2 AND branch_timeline_id = $3
               AND branch_lsn::pg_lsn <= $4::pg_lsn
             ORDER BY id DESC LIMIT 1",
        )
        .bind(branch.as_uuid())
        .bind(database_name)
        .bind(timeline)
        .bind(lsn)
        .fetch_optional(&self.pool)
        .await?
        .as_ref()
        .map(base_from_row)
        .transpose()
    }

    /// Fills entry `b.id` in place with `b`'s model (and its version and server) and identity
    /// map: a base read live from its location (R6). Nothing else of the entry changes.
    /// `false` when the entry is gone.
    pub async fn update_schema_base(&self, b: &SchemaBase) -> Result<bool, MetadataError> {
        Ok(sqlx::query(
            "UPDATE branch_schema_bases
             SET model = $2, model_version = $3, server_major = $4, identity_map = $5
             WHERE id = $1",
        )
        .bind(b.id)
        .bind(&b.model)
        .bind(b.model_version)
        .bind(b.server_major)
        .bind(&b.identity_map)
        .execute(&self.pool)
        .await?
        .rows_affected()
            == 1)
    }

    /// Deletes the branch's entries for every database not in `keep` (databases the branch no
    /// longer has). Returns how many went.
    pub async fn prune_schema_bases(
        &self,
        branch: BranchId,
        keep: &[String],
    ) -> Result<u64, MetadataError> {
        Ok(sqlx::query(
            "DELETE FROM branch_schema_bases
             WHERE branch_id = $1 AND NOT (database_name = ANY($2))",
        )
        .bind(branch.as_uuid())
        .bind(keep)
        .execute(&self.pool)
        .await?
        .rows_affected())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// T14 M12 (T5 minor): every source round-trips, the column's CHECK allows each, and an
    /// unknown one is refused.
    #[test]
    fn base_source_round_trips() {
        let all = [
            BaseSource::Create,
            BaseSource::Reset,
            BaseSource::Merge,
            BaseSource::Update,
            BaseSource::Rebaseline,
        ];
        let migration = include_str!("../migrations/20261010000001_schema_merge.sql");
        for source in all {
            assert_eq!(BaseSource::parse(source.as_str()).unwrap(), source);
            assert!(migration.contains(&format!("'{}'", source.as_str())), "{source:?}");
        }
        assert!(BaseSource::parse("restore").is_err());
    }
}
