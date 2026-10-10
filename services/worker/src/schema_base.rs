//! Base snapshots for schema merges (step 6g, Part 2): the schema a branch shared with its
//! parent at its branch point (or last merge), per Postgres database, with its identity map.
//!
//! Recorded at branch create and reset to parent (best effort, R6); read back by a merge. A base
//! that's missing its model or was written by another model version is read live through a
//! temporary static compute pinned at the base's recorded location (timeline + LSN).

use std::time::Duration;

use chelabase_metadata::{
    BaseSource, BranchKind, BranchRecord, DatabaseOrigin, NewStaticEndpoint, SchemaBase,
};
use chelabase_neon::{ComputeBackend, Lsn, NeonId, SqlOpts, StorconError};
use chelabase_schema::catalog::{CATALOG_SQL, parse_catalog};
use chelabase_schema::{IdentityMap, MODEL_VERSION, SUPPORTED_SERVER_MAJOR, SchemaModel};
use chelabase_types::BranchId;
use chrono::Utc;
use reqwest::StatusCode;
use tracing::{info, warn};

use crate::Worker;
use crate::ports::{CacheNotifier, SpecPusher, Storage, WorkerMetadata};
use crate::provision::{new_compute_token, new_endpoint_id, sha256_hex};

/// How long one catalog read may take (E15: on `cloud_admin`, otherwise `admin_sql`'s options).
pub const CATALOG_READ_TIMEOUT: Duration = Duration::from_secs(60);

/// Reconcile removes a static compute's endpoint older than this (R3). A base read through
/// one takes at most [`crate::provision::READY_TIMEOUT`] to come up plus
/// [`CATALOG_READ_TIMEOUT`] to read (2 minutes), so reconcile never removes one a read still
/// uses: the bound below keeps a margin of more than twice that (T14 M4).
pub const STALE_STATIC_COMPUTE_SECS: i64 = 600;

const _: () = assert!(
    STALE_STATIC_COMPUTE_SECS as u64
        > 2 * (crate::provision::READY_TIMEOUT.as_secs() + CATALOG_READ_TIMEOUT.as_secs())
);

/// Why no trusted base could be had.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BaseError {
    /// The base can't be read any more (no location, history gone or behind the GC horizon):
    /// the merge offers re-baseline.
    Unavailable(String),
    /// Something that may work on a retry (metadata, storage or the compute).
    Transient(String),
}

impl std::fmt::Display for BaseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BaseError::Unavailable(why) => write!(f, "the base is unavailable: {why}"),
            BaseError::Transient(why) => write!(f, "reading the base: {why}"),
        }
    }
}

/// What the pageserver says when a read is below the timeline's GC cutoff
/// (`page_service.rs`: "tried to request a page version that was garbage collected";
/// `Timeline::check_lsn_is_in_scope`: "is earlier than latest GC cutoff").
fn behind_gc_horizon(err: &str) -> bool {
    err.contains("garbage collected") || err.contains("GC cutoff")
}

/// What psql says when the database isn't there (`FATAL:  database "x" does not exist`): a
/// stale `branch_databases` row after a reset, for a database only the old timeline had.
fn database_missing(err: &str, db: &str) -> bool {
    err.contains(&format!("database \"{db}\" does not exist"))
}

/// A base entry for `model` read at `at` (timeline, LSN), or for its location only when the
/// read failed (`None`: the model is read from there when a merge needs it, R6). It's valid on
/// the branch from `valid_from` (its timeline, LSN; E18).
fn base_row(
    branch: BranchId,
    db: &str,
    source: BaseSource,
    (timeline, lsn): (&str, &str),
    (branch_timeline, branch_lsn): (&str, &str),
    model: Option<&SchemaModel>,
    map: &IdentityMap,
) -> Result<SchemaBase, String> {
    let json = model
        .map(serde_json::to_value)
        .transpose()
        .map_err(|e| format!("serializing the model: {e}"))?;
    Ok(SchemaBase {
        id: 0,
        branch_id: branch,
        database_name: db.to_string(),
        model: json,
        model_version: model.map(|m| m.model_version as i32),
        server_major: model.map(|m| m.server_major as i32),
        source,
        timeline_id: Some(timeline.to_string()),
        lsn: Some(lsn.to_string()),
        branch_timeline_id: Some(branch_timeline.to_string()),
        branch_lsn: Some(branch_lsn.to_string()),
        identity_map: serde_json::to_value(map)
            .map_err(|e| format!("serializing the identity map: {e}"))?,
        // The metadata sets both.
        taken_at: Utc::now(),
        recorded_at: Utc::now(),
    })
}

/// The stored model, when this worker can trust it: written by this model version, for the
/// supported server, and readable.
fn trusted_model(base: &SchemaBase) -> Option<SchemaModel> {
    if base.model_version != Some(MODEL_VERSION as i32)
        || base.server_major != Some(SUPPORTED_SERVER_MAJOR as i32)
    {
        return None;
    }
    let model: SchemaModel = serde_json::from_value(base.model.clone()?).ok()?;
    (model.model_version == MODEL_VERSION && model.server_major == SUPPORTED_SERVER_MAJOR)
        .then_some(model)
}

/// A branch point's base (create or reset) pairs every object with itself (E6).
fn is_branch_point(source: BaseSource) -> bool {
    matches!(source, BaseSource::Create | BaseSource::Reset)
}

/// Where a base is read when its model can't be used, and the identity map that stays with it.
struct Location {
    source: BaseSource,
    timeline: String,
    lsn: String,
    /// The stored map; `None` when there's no row or its map is empty or unreadable.
    map: Option<IdentityMap>,
}

impl<M: WorkerMetadata, S: Storage, C: ComputeBackend, N: CacheNotifier, P: SpecPusher>
    Worker<M, S, C, N, P>
{
    /// Reads `db` on `ep` ([`chelabase_schema::catalog::CATALOG_SQL`] through `admin_sql_with`,
    /// [`CATALOG_READ_TIMEOUT`]) and parses it.
    pub(crate) async fn read_schema(&self, ep: &str, db: &str) -> Result<SchemaModel, String> {
        let opts = SqlOpts {
            timeout: CATALOG_READ_TIMEOUT,
            ..SqlOpts::admin_default()
        };
        let out = self
            .computes
            .admin_sql_with(ep, db, CATALOG_SQL, &opts)
            .await
            .map_err(|e| format!("reading the schema of {db}: {e:#}"))?;
        parse_catalog(out.trim()).map_err(|e| format!("reading the schema of {db}: {e}"))
    }

    /// Best effort (R6): reads every non-system database of a ready branch on `ep` and appends
    /// its base, located at and valid from (`timeline`, `lsn`). A branch point's base only
    /// (`Create`, `Reset`): the identity map pairs every object with itself. A failed read
    /// stores the location without the model. A database the compute says doesn't exist is
    /// skipped, and the entries of every database not recorded here are deleted. Nothing here
    /// fails the caller.
    pub(crate) async fn record_bases(
        &self,
        branch: BranchId,
        ep: &str,
        source: BaseSource,
        timeline: &str,
        lsn: &str,
    ) {
        debug_assert!(is_branch_point(source), "{source:?}");
        let databases = match self.metadata.branch_databases(branch).await {
            Ok(databases) => databases,
            Err(e) => {
                warn!(branch_id = %branch, "recording the schema bases: listing the databases: {e}");
                return;
            }
        };
        let mut kept = Vec::new();
        for db in databases
            .iter()
            .filter(|d| d.origin != DatabaseOrigin::System)
        {
            let model = match self.read_schema(ep, &db.name).await {
                Ok(model) => Some(model),
                Err(err) if database_missing(&err, &db.name) => {
                    info!(branch_id = %branch, database = %db.name, "no schema base: the database isn't on this timeline");
                    continue;
                }
                Err(err) => {
                    warn!(branch_id = %branch, database = %db.name, "recording the schema base without its model; a merge reads it from its location: {err}");
                    None
                }
            };
            kept.push(db.name.clone());
            let map = model
                .as_ref()
                .map(IdentityMap::self_mapped)
                .unwrap_or_default();
            let stored = match base_row(
                branch,
                &db.name,
                source,
                (timeline, lsn),
                (timeline, lsn),
                model.as_ref(),
                &map,
            ) {
                Ok(row) => self
                    .metadata
                    .append_schema_base(&row)
                    .await
                    .map(drop)
                    .map_err(|e| e.to_string()),
                Err(err) => Err(err),
            };
            if let Err(err) = stored {
                warn!(branch_id = %branch, database = %db.name, "recording the schema base: {err}");
            }
        }
        self.prune_bases(branch, &kept).await;
    }

    /// Deletes the base entries of every database of `branch` not in `keep` (E18). Best
    /// effort: stale entries are only ever looked up by a database's name.
    async fn prune_bases(&self, branch: BranchId, keep: &[String]) {
        if let Err(e) = self.metadata.prune_schema_bases(branch, keep).await {
            warn!(branch_id = %branch, "deleting the schema bases of databases the branch no longer has: {e}");
        }
    }

    /// After a merge or an update from the parent with keep (T8b): appends the base `model`
    /// (read at `location`: timeline, LSN) with its identity map `map`, valid on `branch` from
    /// (`branch_timeline`, `branch_lsn`), and deletes the entries of databases the branch no
    /// longer has. Only [`BaseSource::Merge`] and [`BaseSource::Update`]; `Err` when the entry
    /// isn't written.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn record_merge_base(
        &self,
        branch: BranchId,
        db: &str,
        model: &SchemaModel,
        map: &IdentityMap,
        source: BaseSource,
        location: (&str, &str),
        branch_timeline: &str,
        branch_lsn: &str,
    ) -> Result<(), String> {
        if !matches!(source, BaseSource::Merge | BaseSource::Update) {
            return Err(format!(
                "a merge's base is recorded as merge or update, not {}",
                source.as_str()
            ));
        }
        let row = base_row(
            branch,
            db,
            source,
            location,
            (branch_timeline, branch_lsn),
            Some(model),
            map,
        )?;
        self.metadata
            .append_schema_base(&row)
            .await
            .map_err(|e| format!("recording the merge's schema base: {e}"))?;
        match self.metadata.branch_databases(branch).await {
            Ok(databases) => {
                let mut keep: Vec<String> = databases
                    .into_iter()
                    .filter(|d| d.origin != DatabaseOrigin::System)
                    .map(|d| d.name)
                    .collect();
                if !keep.iter().any(|name| name == db) {
                    keep.push(db.to_string());
                }
                self.prune_bases(branch, &keep).await;
            }
            Err(e) => {
                warn!(branch_id = %branch, "pruning the schema bases: listing the databases: {e}");
            }
        }
        Ok(())
    }

    /// The trusted base for (branch, db): the current entry (the newest) if version-compatible,
    /// else read live through a static compute at its recorded location (Part 2) and filled in
    /// place, else [`BaseError::Unavailable`]. A branch without a base row (made before bases
    /// were recorded) is read at its branch point: its timeline at `branched_at_lsn`. A current
    /// entry positioned on another timeline than the branch's (a restore found no entry at or
    /// before its point, E18) is [`BaseError::Unavailable`]: the merge offers re-baseline.
    pub(crate) async fn load_base(
        &self,
        branch: &BranchRecord,
        db: &str,
    ) -> Result<(SchemaModel, IdentityMap), BaseError> {
        let stored = self
            .metadata
            .schema_base(branch.id, db)
            .await
            .map_err(|e| BaseError::Transient(format!("reading the stored base: {e}")))?;
        if let Some(base) = &stored
            && let (Some(valid_on), Some(current)) = (
                base.branch_timeline_id.as_deref(),
                branch.timeline_id.as_deref(),
            )
            && valid_on != current
        {
            return Err(BaseError::Unavailable(
                "no base was recorded for the branch's current timeline (a restore found none at \
                 or before its point, or a reset couldn't record one)"
                    .to_string(),
            ));
        }
        let stored_map = stored.as_ref().and_then(|b| {
            serde_json::from_value::<IdentityMap>(b.identity_map.clone())
                .inspect_err(|e| warn!(branch_id = %branch.id, database = db, "the stored identity map is unreadable: {e}"))
                .ok()
        });
        if let Some(model) = stored.as_ref().and_then(trusted_model) {
            return match (stored_map, stored.as_ref().map(|b| b.source)) {
                (Some(map), _) => Ok((model, map)),
                (None, Some(source)) if is_branch_point(source) => {
                    let map = IdentityMap::self_mapped(&model);
                    Ok((model, map))
                }
                _ => Err(BaseError::Unavailable(
                    "the base's identity map is unreadable".to_string(),
                )),
            };
        }
        let location = match &stored {
            Some(base) => match (&base.timeline_id, &base.lsn) {
                (Some(timeline), Some(lsn)) => Location {
                    source: base.source,
                    timeline: timeline.clone(),
                    lsn: lsn.clone(),
                    map: stored_map.filter(|m| !m.pairs().is_empty()),
                },
                _ => {
                    return Err(BaseError::Unavailable(
                        "the base was written by another version and has no location to read \
                         it from"
                            .to_string(),
                    ));
                }
            },
            None => match (
                branch.kind,
                branch.parent_branch_id,
                &branch.timeline_id,
                &branch.branched_at_lsn,
            ) {
                (BranchKind::Normal, Some(_), Some(timeline), Some(lsn)) => Location {
                    source: BaseSource::Create,
                    timeline: timeline.clone(),
                    lsn: lsn.clone(),
                    map: None,
                },
                _ => {
                    return Err(BaseError::Unavailable(
                        "no base was recorded for this branch".to_string(),
                    ));
                }
            },
        };
        let model = self
            .read_base_live(branch, db, &location.timeline, &location.lsn)
            .await?;
        let map = match location.map {
            Some(map) => map,
            None if is_branch_point(location.source) => IdentityMap::self_mapped(&model),
            // A merge's base keeps the pairs the merge recorded; with none, every object is
            // matched by name.
            None => IdentityMap::default(),
        };
        // Stored for next time; a failure only means the next merge reads it again. A stored
        // entry is filled in place (its position in the history stays); a branch without one
        // gets its branch point's, valid from there.
        let at = (location.timeline.as_str(), location.lsn.as_str());
        let row = base_row(branch.id, db, location.source, at, at, Some(&model), &map);
        let stored = match (row, &stored) {
            (Ok(row), Some(entry)) => {
                let filled = SchemaBase {
                    id: entry.id,
                    ..row
                };
                match self.metadata.update_schema_base(&filled).await {
                    Ok(true) => Ok(()),
                    Ok(false) => Err("the entry is gone".to_string()),
                    Err(e) => Err(e.to_string()),
                }
            }
            (Ok(row), None) => self
                .metadata
                .append_schema_base(&row)
                .await
                .map(drop)
                .map_err(|e| e.to_string()),
            (Err(err), _) => Err(err),
        };
        if let Err(err) = stored {
            warn!(branch_id = %branch.id, database = db, "storing the base read live: {err}");
        }
        Ok((model, map))
    }

    /// Reads `db` at (`timeline`, `lsn`) through a temporary static compute: an unroutable
    /// endpoint row pinned at `lsn`, the compute, a readiness check, the catalog read; then the
    /// compute and the row go, whatever happened. An LSN behind the timeline's GC horizon is
    /// read on the timeline's ancestor when it is the timeline's branch point (retained there
    /// while the timeline exists); any other such LSN, or a timeline that's gone, is
    /// [`BaseError::Unavailable`] and starts nothing.
    async fn read_base_live(
        &self,
        branch: &BranchRecord,
        db: &str,
        timeline: &str,
        lsn: &str,
    ) -> Result<SchemaModel, BaseError> {
        let tenant = self
            .tenant_of(branch.database_id)
            .await
            .map_err(BaseError::Transient)?
            .ok_or_else(|| BaseError::Unavailable("the database has no tenant".to_string()))?;
        let timeline_id: NeonId = timeline
            .parse()
            .map_err(|e| BaseError::Unavailable(format!("timeline {timeline:?}: {e}")))?;
        let at: Lsn = lsn
            .parse()
            .map_err(|e| BaseError::Unavailable(format!("LSN {lsn:?}: {e}")))?;
        let gone = |e: StorconError| match e {
            StorconError::Rejected { status, .. } if status == StatusCode::NOT_FOUND => {
                BaseError::Unavailable(format!("the base's timeline {timeline} no longer exists"))
            }
            e => BaseError::Transient(format!("checking the base's history: {e:#}")),
        };
        // Behind the timeline's GC horizon, the point is still readable when it's the
        // timeline's branch point: its ancestor retains it while the timeline exists, and the
        // pageserver serves retained points below its cutoff. Anything else is gone (E17).
        let read_on = match self.storage.gc_cutoff(&tenant, &timeline_id).await {
            Ok(Some(cutoff)) if at < cutoff => {
                match self
                    .storage
                    .ancestor(&tenant, &timeline_id)
                    .await
                    .map_err(gone)?
                {
                    Some((parent, point)) if point == at => parent.as_str().to_string(),
                    _ => {
                        return Err(BaseError::Unavailable(format!(
                            "the base at {at} is behind the history's GC horizon ({cutoff}) on \
                             timeline {timeline}, and no timeline retains it"
                        )));
                    }
                }
            }
            Ok(_) => timeline.to_string(),
            Err(e) => return Err(gone(e)),
        };
        let timeline = read_on.as_str();
        let ep = new_endpoint_id();
        let token = new_compute_token();
        let token_hash = sha256_hex(&token);
        self.metadata
            .create_static_endpoint(&NewStaticEndpoint {
                id: &ep,
                tenant_id: tenant.as_str(),
                timeline_id: timeline,
                compute_token_sha256: &token_hash,
                static_lsn: lsn,
            })
            .await
            .map_err(|e| BaseError::Transient(format!("recording the static compute: {e}")))?;
        info!(branch_id = %branch.id, database = db, endpoint_id = %ep, %timeline, %lsn, "reading a base through a static compute");
        let read = async {
            self.computes
                .create(&ep, &token)
                .await
                .map_err(|e| format!("starting the static compute: {e:#}"))?;
            self.wait_until_ready(&ep, db).await?;
            self.read_schema(&ep, db).await
        }
        .await;
        if let Err(err) = self.remove_static_compute(&ep).await {
            warn!(endpoint_id = %ep, "removing the static compute; reconcile retries: {err}");
        }
        read.map_err(|err| {
            if behind_gc_horizon(&err) {
                BaseError::Unavailable(format!(
                    "the base is behind the history's GC horizon: {err}"
                ))
            } else {
                BaseError::Transient(err)
            }
        })
    }

    /// Removes a static compute and then its endpoint row. A compute that can't be removed
    /// keeps its row, so reconcile retries.
    pub(crate) async fn remove_static_compute(&self, ep: &str) -> Result<(), String> {
        self.computes
            .delete(ep)
            .await
            .map_err(|e| format!("removing the static compute: {e:#}"))?;
        self.metadata
            .delete_endpoint(ep)
            .await
            .map_err(|e| format!("removing the static compute's endpoint: {e}"))
    }

    /// After a reset's switch: the branch's new branch point becomes its base (its timeline at
    /// `branched_at_lsn`, both written with the switch). Best effort, like [`Self::record_bases`].
    pub(crate) async fn record_reset_bases(&self, branch: BranchId, ep: &str) {
        let current = match self.metadata.get_branch(branch).await {
            Ok(Some(current)) => current,
            Ok(None) => return,
            Err(e) => {
                warn!(branch_id = %branch, "recording the reset's schema bases: reading the branch: {e}");
                return;
            }
        };
        match (
            current.timeline_id.as_deref(),
            current.branched_at_lsn.as_deref(),
        ) {
            (Some(timeline), Some(lsn)) => {
                self.record_bases(branch, ep, BaseSource::Reset, timeline, lsn)
                    .await;
            }
            _ => {
                warn!(branch_id = %branch, "recording the reset's schema bases: the branch has no timeline or branch point");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use chelabase_metadata::{BaseSource, BranchKind, BranchStatus, SchemaBase};
    use chelabase_nats::PointInTime;
    use chelabase_neon::{Lsn, NeonId};
    use chelabase_schema::catalog::parse_catalog;
    use chelabase_schema::{IdentityMap, MODEL_VERSION, SchemaModel};
    use chelabase_types::{BranchId, DatabaseId};
    use serde_json::json;

    use super::*;
    use crate::fakes::{
        FakeWorker, Journal, entries, pending_branch, position, ready_branch, ready_database,
        worker,
    };
    use crate::reconcile::STALE_PROVISION_SECS;

    /// Only the catalog read's first statement has this.
    const CATALOG_READ: &str = "REPEATABLE READ READ ONLY";

    /// A catalog document as `CATALOG_SQL` prints it: `public`, one table and the owner.
    fn catalog(table: &str) -> String {
        let postgres = json!({ "oid": 10, "name": "postgres" });
        json!({
            "server_version_num": 170005,
            "schemas": [{ "oid": 2200, "name": "public", "owner": postgres, "comment": null,
                          "acl": [] }],
            "tables": [{
                "oid": 16400, "schema": "public", "name": table, "owner": postgres,
                "comment": null, "rls_enabled": false, "rls_forced": false, "persistence": "p",
                "options": [], "acl": [],
                "columns": [{
                    "attnum": 1, "name": "id", "data_type": "bigint", "type_oid": 20,
                    "collation": null, "default_expr": null, "generated": "", "identity": "",
                    "identity_sequence": null, "not_null": true, "comment": null, "acl": [],
                }],
                "constraints": [], "indexes": [], "policies": [],
            }],
            "enums": [],
            "sequences": [],
            "roles": [{ "oid": 10, "name": "postgres", "login": true, "inherit": true,
                        "connection_limit": -1, "valid_until": null, "bypass_rls": true,
                        "member_of": [], "other_attributes": [], "comment": null }],
            "default_privileges": [],
            "reported": [],
        })
        .to_string()
    }

    fn model(table: &str) -> SchemaModel {
        parse_catalog(&catalog(table)).unwrap()
    }

    /// The resync's database rows (`d|name||owner|system||`, hex-encoded): `names` and the
    /// system database.
    fn databases(names: &[&str]) -> String {
        let mut out = format!(
            "d|{}||{}|t|||||||\n",
            hex::encode("postgres"),
            hex::encode("cloud_admin")
        );
        for name in names {
            out.push_str(&format!(
                "d|{}||{}|f|||||||\n",
                hex::encode(name),
                hex::encode("postgres")
            ));
        }
        out
    }

    fn pos(journal: &Journal, prefix: &str) -> usize {
        position(journal, prefix)
            .unwrap_or_else(|| panic!("{prefix:?} not in journal {:?}", entries(journal)))
    }

    fn last(journal: &Journal, prefix: &str) -> usize {
        entries(journal)
            .iter()
            .rposition(|e| e.starts_with(prefix))
            .unwrap_or_else(|| panic!("{prefix:?} not in journal {:?}", entries(journal)))
    }

    fn has(journal: &Journal, prefix: &str) -> bool {
        position(journal, prefix).is_some()
    }

    /// The current base of (branch, db): its newest entry.
    fn stored(w: &FakeWorker, branch: BranchId, db: &str) -> Option<SchemaBase> {
        history(w, branch, db).pop()
    }

    /// Every entry of (branch, db), oldest first.
    fn history(w: &FakeWorker, branch: BranchId, db: &str) -> Vec<SchemaBase> {
        w.metadata
            .state()
            .schema_bases
            .iter()
            .filter(|b| b.branch_id == branch && b.database_name == db)
            .cloned()
            .collect()
    }

    /// Changes the current base of (branch, db) in place.
    fn edit_current(
        w: &FakeWorker,
        branch: BranchId,
        db: &str,
        edit: impl FnOnce(&mut SchemaBase),
    ) {
        let mut state = w.metadata.state();
        let base = state
            .schema_bases
            .iter_mut()
            .rev()
            .find(|b| b.branch_id == branch && b.database_name == db)
            .expect("a base");
        edit(base);
    }

    fn stored_model(base: &SchemaBase) -> SchemaModel {
        serde_json::from_value(base.model.clone().expect("a model")).unwrap()
    }

    fn stored_map(base: &SchemaBase) -> IdentityMap {
        serde_json::from_value(base.identity_map.clone()).unwrap()
    }

    /// Answers the catalog read in `db` with `table`'s document from now on (replacing earlier
    /// answers).
    fn answer_catalog(w: &FakeWorker, db: &str, table: &str) {
        w.computes.answers_in.lock().unwrap().clear();
        w.computes.answer_in(db, CATALOG_READ, &catalog(table));
    }

    /// A ready database and a ready `dev` of main whose `app` base was recorded (`orders`).
    async fn ready_dev(w: &FakeWorker, journal: &Journal) -> (DatabaseId, BranchId) {
        let db = ready_database(w).await;
        answer_catalog(w, "app", "orders");
        let main = w.metadata.main(db);
        let dev = ready_branch(w, db, "dev", main.id).await;
        journal.lock().unwrap().clear();
        (db, dev)
    }

    /// Marks the stored `app` base of `dev` as written by another model version.
    fn age_the_base(w: &FakeWorker, dev: BranchId) {
        edit_current(w, dev, "app", |base| {
            base.model_version = Some(0);
            base.model = Some(json!({ "an": "older shape" }));
        });
    }

    /// The static compute's endpoint id, from its journal entry.
    fn static_endpoint(journal: &Journal) -> String {
        let entry = &entries(journal)[pos(journal, "metadata.create_static_endpoint ")];
        entry
            .strip_prefix("metadata.create_static_endpoint ")
            .and_then(|rest| rest.split(' ').next())
            .unwrap()
            .to_string()
    }

    fn tenant_and_timeline(w: &FakeWorker, db: DatabaseId, dev: BranchId) -> (NeonId, NeonId) {
        let tenant = w.metadata.database(db).neon_tenant_id.unwrap();
        let timeline = w.metadata.branch(dev).timeline_id.unwrap();
        (tenant.parse().unwrap(), timeline.parse().unwrap())
    }

    fn set_main_head(w: &FakeWorker, db: DatabaseId, head: Lsn) {
        let tenant: NeonId = w
            .metadata
            .database(db)
            .neon_tenant_id
            .unwrap()
            .parse()
            .unwrap();
        let main: NeonId = w.metadata.main(db).timeline_id.unwrap().parse().unwrap();
        w.storage
            .timelines
            .lock()
            .unwrap()
            .get_mut(&(tenant, main))
            .unwrap()
            .head = head;
    }

    // --- Recording ----------------------------------------------------------------------

    #[tokio::test]
    async fn create_records_a_base_per_database() {
        let (w, journal) = worker();
        let db = ready_database(&w).await;
        let main = w.metadata.main(db);
        let dev = pending_branch(&w, db, "dev", main.id);
        w.computes
            .answer("SELECT 'd',", &databases(&["app", "shop"]));
        w.computes
            .answer_in("app", CATALOG_READ, &catalog("orders"));
        w.computes
            .answer_in("shop", CATALOG_READ, &catalog("items"));
        journal.lock().unwrap().clear();

        w.handle_branch_create(dev, None).await.unwrap();

        let b = w.metadata.branch(dev);
        assert_eq!(b.status, BranchStatus::Ready, "{:?}", b.error);
        let (ep, timeline) = (b.endpoint_id.unwrap(), b.timeline_id.unwrap());
        let ready = pos(&journal, "metadata.set_branch_ready");
        for (name, table) in [("app", "orders"), ("shop", "items")] {
            let base = stored(&w, dev, name).unwrap_or_else(|| panic!("no base for {name}"));
            assert_eq!(base.source, BaseSource::Create);
            assert_eq!(base.timeline_id.as_deref(), Some(timeline.as_str()));
            assert_eq!(base.lsn.as_deref(), Some("0/2000000"));
            // E18: valid from the new timeline's start.
            assert_eq!(base.branch_timeline_id.as_deref(), Some(timeline.as_str()));
            assert_eq!(base.branch_lsn.as_deref(), Some("0/2000000"));
            assert_eq!(base.model_version, Some(MODEL_VERSION as i32));
            assert_eq!(base.server_major, Some(17));
            assert_eq!(stored_model(&base), model(table));
            // E6: a branch point's map pairs every base object with itself.
            let map = stored_map(&base);
            assert_eq!(map, IdentityMap::self_mapped(&model(table)));
            assert!(!map.pairs().is_empty());
            // Read on the new branch's own compute, and stored before it's ready.
            let read = pos(
                &journal,
                &format!("compute.sql {ep} {name} BEGIN ISOLATION"),
            );
            let upsert = pos(
                &journal,
                &format!("metadata.append_schema_base {dev} {name} create"),
            );
            assert!(read < upsert && upsert < ready, "{:?}", entries(&journal));
        }
        // Neither the system database nor main gets one.
        assert_eq!(w.metadata.state().schema_bases.len(), 2);
    }

    #[tokio::test]
    async fn a_failed_base_read_does_not_fail_the_create() {
        let (w, journal) = worker();
        let db = ready_database(&w).await;
        let main = w.metadata.main(db);
        let dev = pending_branch(&w, db, "dev", main.id);
        *w.computes.fail_sql_containing.lock().unwrap() =
            Some((CATALOG_READ.into(), "server closed the connection".into()));
        journal.lock().unwrap().clear();

        w.handle_branch_create(dev, None).await.unwrap();

        let b = w.metadata.branch(dev);
        assert_eq!(b.status, BranchStatus::Ready, "{:?}", b.error);
        assert_eq!(b.error, None);
        // R6: the location is kept, so a merge reads the model from it later.
        let base = stored(&w, dev, "app").expect("a base without its model");
        assert_eq!(base.model, None);
        assert_eq!(base.model_version, None);
        assert_eq!(base.source, BaseSource::Create);
        assert_eq!(base.timeline_id, b.timeline_id);
        assert_eq!(base.lsn.as_deref(), Some("0/2000000"));
        assert_eq!(base.identity_map, json!({ "pairs": [] }));

        // Nor does a metadata failure while storing it.
        *w.computes.fail_sql_containing.lock().unwrap() = None;
        w.computes
            .answer_in("app", CATALOG_READ, &catalog("orders"));
        w.metadata.state().fail = Some("append_schema_base");
        let dev2 = pending_branch(&w, db, "dev2", main.id);
        w.handle_branch_create(dev2, None).await.unwrap();
        let b2 = w.metadata.branch(dev2);
        assert_eq!(b2.status, BranchStatus::Ready, "{:?}", b2.error);
        assert!(stored(&w, dev2, "app").is_none());
    }

    #[tokio::test]
    async fn rehearsal_branches_record_no_base() {
        let (w, journal) = worker();
        let db = ready_database(&w).await;
        let main = w.metadata.main(db);
        let rehearsal = pending_branch(&w, db, "_rehearsal-0123abcd", main.id);
        w.metadata
            .state()
            .branches
            .get_mut(&rehearsal)
            .unwrap()
            .kind = BranchKind::Rehearsal;
        w.computes
            .answer_in("app", CATALOG_READ, &catalog("orders"));
        journal.lock().unwrap().clear();

        w.handle_branch_create(rehearsal, None).await.unwrap();

        assert!(w.metadata.state().schema_bases.is_empty());
        assert!(
            !entries(&journal)
                .iter()
                .any(|e| e.contains(CATALOG_READ) || e.contains("BEGIN ISOLATION")),
            "{:?}",
            entries(&journal)
        );
    }

    #[tokio::test]
    async fn reset_updates_branched_at_lsn_and_rebases() {
        let (w, journal) = worker();
        let (db, dev) = ready_dev(&w, &journal).await;
        let ep = w.metadata.branch(dev).endpoint_id.unwrap();
        // Main moved on and changed; the reset takes its head.
        set_main_head(&w, db, Lsn(0x0300_0000));
        answer_catalog(&w, "app", "orders_v2");
        w.metadata.state().branches.get_mut(&dev).unwrap().status = BranchStatus::Resetting;

        w.handle_branch_reset_to_parent(dev).await.unwrap();

        let b = w.metadata.branch(dev);
        assert_eq!(b.status, BranchStatus::Ready, "{:?}", b.error);
        assert_eq!(b.branched_at_lsn.as_deref(), Some("0/3000000"));
        let base = stored(&w, dev, "app").unwrap();
        assert_eq!(base.source, BaseSource::Reset);
        assert_eq!(base.lsn.as_deref(), Some("0/3000000"));
        assert_eq!(base.timeline_id, b.timeline_id, "the new timeline");
        assert_eq!(base.branch_timeline_id, b.timeline_id);
        assert_eq!(base.branch_lsn.as_deref(), Some("0/3000000"));
        assert_eq!(stored_model(&base), model("orders_v2"));
        // E18: appended; the create's entry stays in the history.
        let sources: Vec<_> = history(&w, dev, "app").iter().map(|b| b.source).collect();
        assert_eq!(sources, [BaseSource::Create, BaseSource::Reset]);
        assert_eq!(
            stored_map(&base),
            IdentityMap::self_mapped(&model("orders_v2"))
        );
        // Read on the re-created compute, before the branch is ready again.
        let order = [
            pos(&journal, "metadata.switch_to_pending_timeline"),
            pos(&journal, &format!("compute.create {ep}")),
            pos(&journal, &format!("compute.sql {ep} app BEGIN ISOLATION")),
            pos(
                &journal,
                &format!("metadata.append_schema_base {dev} app reset"),
            ),
            last(&journal, "metadata.set_branch_status"),
        ];
        assert!(order.is_sorted(), "{:?}", entries(&journal));
    }

    #[tokio::test]
    async fn a_reset_finished_by_reconcile_records_its_base() {
        let (w, journal) = worker();
        let (db, dev) = ready_dev(&w, &journal).await;
        set_main_head(&w, db, Lsn(0x0300_0000));
        answer_catalog(&w, "app", "orders_v2");
        w.metadata.state().branches.get_mut(&dev).unwrap().status = BranchStatus::Resetting;
        // The compute doesn't come back after the switch: reconcile finishes the reset.
        *w.computes.fail_create.lock().unwrap() = Some("no capacity".into());
        w.handle_branch_reset_to_parent(dev).await.unwrap();
        assert_eq!(w.metadata.branch(dev).status, BranchStatus::Resetting);
        assert_eq!(stored(&w, dev, "app").unwrap().source, BaseSource::Create);

        *w.computes.fail_create.lock().unwrap() = None;
        w.metadata
            .state()
            .branch_ages
            .insert(dev, STALE_PROVISION_SECS + 1);
        w.finish_switch(dev).await.unwrap();

        let b = w.metadata.branch(dev);
        assert_eq!(b.status, BranchStatus::Ready, "{:?}", b.error);
        let base = stored(&w, dev, "app").unwrap();
        assert_eq!(base.source, BaseSource::Reset);
        assert_eq!(base.lsn.as_deref(), Some("0/3000000"));
        assert_eq!(base.timeline_id, b.timeline_id);
        assert_eq!(stored_model(&base), model("orders_v2"));
    }

    // --- Loading ------------------------------------------------------------------------

    #[tokio::test]
    async fn stored_base_of_current_version_is_used() {
        let (w, journal) = worker();
        let (_db, dev) = ready_dev(&w, &journal).await;
        let branch = w.metadata.branch(dev);

        let (base, map) = w.load_base(&branch, "app").await.unwrap();

        assert_eq!(base, model("orders"));
        assert_eq!(map, IdentityMap::self_mapped(&model("orders")));
        for prefix in [
            "metadata.create_static_endpoint",
            "compute.",
            "metadata.append_schema_base",
            "metadata.update_schema_base",
        ] {
            assert!(!has(&journal, prefix), "{prefix}: {:?}", entries(&journal));
        }
    }

    #[tokio::test]
    async fn old_version_base_is_read_live_through_a_static_compute() {
        let (w, journal) = worker();
        let (_db, dev) = ready_dev(&w, &journal).await;
        let branch = w.metadata.branch(dev);
        let timeline = branch.timeline_id.clone().unwrap();
        let before = stored(&w, dev, "app").unwrap();
        let map_before = stored_map(&before);
        age_the_base(&w, dev);

        let (base, map) = w.load_base(&branch, "app").await.unwrap();

        assert_eq!(base, model("orders"));
        assert_eq!(map, map_before, "the identity map survives a model upgrade");
        let ep = static_endpoint(&journal);
        let order = [
            pos(
                &journal,
                &format!("metadata.create_static_endpoint {ep} {timeline} at 0/2000000"),
            ),
            pos(&journal, &format!("compute.create {ep}")),
            pos(&journal, &format!("compute.sql {ep} app SELECT 1")),
            pos(&journal, &format!("compute.sql {ep} app BEGIN ISOLATION")),
            pos(&journal, &format!("compute.delete {ep}")),
            pos(&journal, "metadata.delete_endpoint"),
            pos(
                &journal,
                &format!("metadata.update_schema_base {}", before.id),
            ),
        ];
        // E18: the same entry is filled in place; nothing is appended.
        assert!(!has(&journal, "metadata.append_schema_base"));
        assert_eq!(history(&w, dev, "app").len(), 1);
        assert!(order.is_sorted(), "{:?}", entries(&journal));
        // Nothing of the static compute is left.
        assert!(!w.metadata.state().endpoints.contains_key(&ep));
        assert!(!w.computes.list().await.unwrap().contains(&ep));
        // The base is stored again, in this version, at the same location.
        let again = stored(&w, dev, "app").unwrap();
        assert_eq!(again.id, before.id);
        assert_eq!(again.model_version, Some(MODEL_VERSION as i32));
        assert_eq!(stored_model(&again), model("orders"));
        assert_eq!(again.source, BaseSource::Create);
        assert_eq!(again.timeline_id.as_deref(), Some(timeline.as_str()));
        assert_eq!(again.lsn.as_deref(), Some("0/2000000"));
        assert_eq!(stored_map(&again), map_before);
    }

    #[tokio::test]
    async fn static_compute_is_removed_on_read_error() {
        let (w, journal) = worker();
        let (_db, dev) = ready_dev(&w, &journal).await;
        let branch = w.metadata.branch(dev);
        let timeline = branch.timeline_id.clone().unwrap();
        age_the_base(&w, dev);
        let endpoints_before = w.metadata.endpoint_ids();

        // The catalog read fails.
        *w.computes.fail_sql_containing.lock().unwrap() =
            Some((CATALOG_READ.into(), "server closed the connection".into()));
        let err = w.load_base(&branch, "app").await.unwrap_err();
        assert!(
            matches!(&err, BaseError::Transient(m) if m.contains("server closed the connection")),
            "{err:?}"
        );
        let ep = static_endpoint(&journal);
        assert!(
            pos(&journal, &format!("compute.delete {ep}"))
                < pos(&journal, "metadata.delete_endpoint")
        );
        assert_eq!(w.metadata.endpoint_ids(), endpoints_before);
        assert!(!w.computes.list().await.unwrap().contains(&ep));
        // The stored base is left as it was.
        assert_eq!(stored(&w, dev, "app").unwrap().model_version, Some(0));

        // The compute doesn't start.
        *w.computes.fail_sql_containing.lock().unwrap() = None;
        *w.computes.fail_create.lock().unwrap() = Some("no capacity".into());
        journal.lock().unwrap().clear();
        let err = w.load_base(&branch, "app").await.unwrap_err();
        assert!(
            matches!(&err, BaseError::Transient(m) if m.contains("no capacity")),
            "{err:?}"
        );
        assert!(has(&journal, "metadata.delete_endpoint"));
        assert_eq!(w.metadata.endpoint_ids(), endpoints_before);

        // The read works but the compute can't be removed: the row stays for reconcile.
        *w.computes.fail_create.lock().unwrap() = None;
        *w.computes.fail_delete.lock().unwrap() = Some("docker is down".into());
        journal.lock().unwrap().clear();
        let (base, _) = w.load_base(&branch, "app").await.unwrap();
        assert_eq!(base, model("orders"));
        let ep = static_endpoint(&journal);
        assert!(!has(&journal, "metadata.delete_endpoint"));
        let row = w.metadata.state().endpoints[&ep].endpoint.clone();
        assert!(!row.routable, "the proxy never routes to it");
        assert_eq!(row.static_lsn.as_deref(), Some("0/2000000"));
        assert_eq!(row.timeline_id, timeline);
        assert!(
            !w.metadata
                .state()
                .branches
                .values()
                .any(|b| b.endpoint_id.as_deref() == Some(ep.as_str())),
            "linked to no branch"
        );
    }

    #[tokio::test]
    async fn gc_horizon_error_is_unavailable() {
        let (w, journal) = worker();
        let (db, dev) = ready_dev(&w, &journal).await;
        let branch = w.metadata.branch(dev);
        let (tenant, timeline) = tenant_and_timeline(&w, db, dev);
        age_the_base(&w, dev);
        let endpoints_before = w.metadata.endpoint_ids();

        // GC passed the base's LSN, and it isn't the timeline's branch point (so no ancestor
        // retains it): no compute is started. Detached, or branched at another point.
        let key = (tenant.clone(), timeline.clone());
        let ancestor = w
            .storage
            .timeline(tenant.as_str(), timeline.as_str())
            .unwrap()
            .ancestor;
        let (parent, _) = ancestor.clone().expect("dev is main's child");
        w.storage
            .gc_cutoffs
            .lock()
            .unwrap()
            .insert(timeline.clone(), Lsn(0x0300_0000));
        for other in [None, Some((parent, Lsn(0x0100_0000)))] {
            w.storage
                .timelines
                .lock()
                .unwrap()
                .get_mut(&key)
                .unwrap()
                .ancestor = other.clone();
            let err = w.load_base(&branch, "app").await.unwrap_err();
            assert!(
                matches!(&err, BaseError::Unavailable(m) if m.contains("GC")),
                "{other:?}: {err:?}"
            );
            assert!(!has(&journal, "metadata.create_static_endpoint"));
        }
        w.storage
            .timelines
            .lock()
            .unwrap()
            .get_mut(&key)
            .unwrap()
            .ancestor = ancestor;

        // The pageserver says so while the compute reads.
        w.storage.gc_cutoffs.lock().unwrap().clear();
        *w.computes.fail_sql_containing.lock().unwrap() = Some((
            CATALOG_READ.into(),
            "could not read block: tried to request a page version that was garbage collected. \
             requested at 0/2000000 gc cutoff 0/3000000"
                .into(),
        ));
        let err = w.load_base(&branch, "app").await.unwrap_err();
        assert!(matches!(&err, BaseError::Unavailable(_)), "{err:?}");
        assert!(has(&journal, "metadata.delete_endpoint"));
        assert_eq!(w.metadata.endpoint_ids(), endpoints_before);
        *w.computes.fail_sql_containing.lock().unwrap() = None;

        // The storage controller can't be asked: try again later.
        *w.storage.fail.lock().unwrap() = Some("gc_cutoff");
        let err = w.load_base(&branch, "app").await.unwrap_err();
        assert!(matches!(&err, BaseError::Transient(_)), "{err:?}");
        *w.storage.fail.lock().unwrap() = None;

        // The timeline is gone.
        w.storage
            .timelines
            .lock()
            .unwrap()
            .remove(&(tenant, timeline));
        let err = w.load_base(&branch, "app").await.unwrap_err();
        assert!(
            matches!(&err, BaseError::Unavailable(m) if m.contains("no longer exists")),
            "{err:?}"
        );
    }

    /// E17 after patch 4: a base at the timeline's branch point, behind its GC horizon, is read
    /// on the ancestor timeline at that point, which the pageserver retains (and serves below
    /// its own cutoff) while the branch exists. The entry keeps its own location.
    #[tokio::test]
    async fn a_branch_point_behind_gc_is_read_on_the_ancestor_that_retains_it() {
        let (w, journal) = worker();
        let (db, dev) = ready_dev(&w, &journal).await;
        let branch = w.metadata.branch(dev);
        let (tenant, timeline) = tenant_and_timeline(&w, db, dev);
        let (parent, point) = w
            .storage
            .timeline(tenant.as_str(), timeline.as_str())
            .unwrap()
            .ancestor
            .expect("dev is main's child");
        assert_eq!(
            point.to_string(),
            "0/2000000",
            "the base sits at dev's branch point"
        );
        age_the_base(&w, dev);
        // GC passed the point on both timelines.
        for t in [&timeline, &parent] {
            w.storage
                .gc_cutoffs
                .lock()
                .unwrap()
                .insert(t.clone(), Lsn(0x0300_0000));
        }

        let (base, map) = w.load_base(&branch, "app").await.unwrap();

        assert_eq!(base, model("orders"));
        assert_eq!(map, IdentityMap::self_mapped(&model("orders")));
        assert!(has(
            &journal,
            &format!(
                "metadata.create_static_endpoint {} {parent} at 0/2000000",
                static_endpoint(&journal)
            )
        ));
        let again = stored(&w, dev, "app").unwrap();
        assert_eq!(stored_model(&again), model("orders"));
        assert_eq!(again.timeline_id.as_deref(), Some(timeline.as_str()));
        assert_eq!(again.lsn.as_deref(), Some("0/2000000"));

        // The ancestor can't be read: try again later.
        age_the_base(&w, dev);
        *w.storage.fail.lock().unwrap() = Some("ancestor");
        let err = w.load_base(&branch, "app").await.unwrap_err();
        assert!(matches!(&err, BaseError::Transient(_)), "{err:?}");
    }

    #[tokio::test]
    async fn a_base_without_its_model_is_read_and_self_mapped() {
        let (w, journal) = worker();
        let (_db, dev) = ready_dev(&w, &journal).await;
        let branch = w.metadata.branch(dev);
        edit_current(&w, dev, "app", |base| {
            base.model = None;
            base.model_version = None;
            base.server_major = None;
            base.identity_map = json!({ "pairs": [] });
        });

        let (base, map) = w.load_base(&branch, "app").await.unwrap();

        assert_eq!(base, model("orders"));
        assert_eq!(map, IdentityMap::self_mapped(&model("orders")));
        let again = stored(&w, dev, "app").unwrap();
        assert_eq!(stored_model(&again), model("orders"));
        assert_eq!(stored_map(&again), map);
    }

    #[tokio::test]
    async fn a_branch_without_a_base_is_read_at_its_branch_point() {
        let (w, journal) = worker();
        let (_db, dev) = ready_dev(&w, &journal).await;
        // A branch made before bases were recorded.
        w.metadata.state().schema_bases.clear();
        let branch = w.metadata.branch(dev);
        let timeline = branch.timeline_id.clone().unwrap();

        let (base, map) = w.load_base(&branch, "app").await.unwrap();

        assert_eq!(base, model("orders"));
        assert_eq!(map, IdentityMap::self_mapped(&model("orders")));
        assert!(has(
            &journal,
            &format!(
                "metadata.create_static_endpoint {} {timeline} at 0/2000000",
                static_endpoint(&journal)
            )
        ));
        let again = stored(&w, dev, "app").unwrap();
        assert_eq!(again.source, BaseSource::Create);
        assert_eq!(again.lsn.as_deref(), Some("0/2000000"));
        // Positioned at the branch point it was read at.
        assert_eq!(again.branch_timeline_id.as_deref(), Some(timeline.as_str()));
        assert_eq!(again.branch_lsn.as_deref(), Some("0/2000000"));
    }

    #[tokio::test]
    async fn no_base_without_a_location() {
        let (w, journal) = worker();
        let (db, dev) = ready_dev(&w, &journal).await;
        // Main has no branch point.
        let main = w.metadata.main(db);
        let err = w.load_base(&main, "app").await.unwrap_err();
        assert!(matches!(err, BaseError::Unavailable(_)), "{err:?}");
        // A re-baselined base of another version has no location to read it from.
        age_the_base(&w, dev);
        edit_current(&w, dev, "app", |base| {
            base.source = BaseSource::Rebaseline;
            base.timeline_id = None;
            base.lsn = None;
        });
        let err = w
            .load_base(&w.metadata.branch(dev), "app")
            .await
            .unwrap_err();
        assert!(matches!(err, BaseError::Unavailable(_)), "{err:?}");
        assert!(!has(&journal, "metadata.create_static_endpoint"));
    }

    // --- History and restore (E18) -------------------------------------------------------

    fn set_branch_head(w: &FakeWorker, db: DatabaseId, branch: BranchId, head: Lsn) {
        let key = tenant_and_timeline(w, db, branch);
        w.storage
            .timelines
            .lock()
            .unwrap()
            .get_mut(&key)
            .unwrap()
            .head = head;
    }

    /// `dev` merged into its parent with keep at 0/5000000: its base is now `orders_merged`,
    /// read on its own timeline there. Returns the map recorded with it.
    async fn keep_merge(w: &FakeWorker, dev: BranchId) -> IdentityMap {
        let timeline = w.metadata.branch(dev).timeline_id.unwrap();
        let merged = model("orders_merged");
        let map = IdentityMap::default();
        w.record_merge_base(
            dev,
            "app",
            &merged,
            &map,
            BaseSource::Merge,
            (&timeline, "0/5000000"),
            &timeline,
            "0/5000000",
        )
        .await
        .unwrap();
        map
    }

    async fn start_restore(w: &FakeWorker, dev: BranchId, lsn: &str) -> Result<(), String> {
        w.metadata.state().branches.get_mut(&dev).unwrap().status = BranchStatus::Resetting;
        // A name of its own per restore: each one keeps a backup.
        let backup = BranchId::new();
        w.handle_branch_restore(
            dev,
            backup,
            &format!("dev-before-restore-{backup}"),
            PointInTime::Lsn(lsn.to_string()),
        )
        .await
    }

    /// Restores `dev` to `lsn`; returns the restored timeline.
    async fn restore_to(w: &FakeWorker, dev: BranchId, lsn: &str) -> String {
        start_restore(w, dev, lsn).await.unwrap();
        let b = w.metadata.branch(dev);
        assert_eq!((b.status, b.error.as_deref()), (BranchStatus::Ready, None));
        b.timeline_id.unwrap()
    }

    #[tokio::test]
    async fn record_merge_base_appends_a_positioned_entry_and_prunes() {
        let (w, journal) = worker();
        let (_db, dev) = ready_dev(&w, &journal).await;
        let timeline = w.metadata.branch(dev).timeline_id.unwrap();
        let parent_timeline = NeonId::new().to_string();
        // A database the branch no longer has.
        let mut gone = stored(&w, dev, "app").unwrap();
        gone.database_name = "gone".into();
        w.metadata.state().schema_bases.push(gone);
        let merged = model("orders_merged");
        let map = IdentityMap::self_mapped(&merged);

        // An update from the parent: read on the parent, valid on the branch from 0/5000000.
        w.record_merge_base(
            dev,
            "app",
            &merged,
            &map,
            BaseSource::Update,
            (&parent_timeline, "0/4000000"),
            &timeline,
            "0/5000000",
        )
        .await
        .unwrap();

        let base = stored(&w, dev, "app").unwrap();
        assert_eq!(base.source, BaseSource::Update);
        assert_eq!(
            (base.timeline_id.as_deref(), base.lsn.as_deref()),
            (Some(parent_timeline.as_str()), Some("0/4000000"))
        );
        assert_eq!(
            (
                base.branch_timeline_id.as_deref(),
                base.branch_lsn.as_deref()
            ),
            (Some(timeline.as_str()), Some("0/5000000"))
        );
        assert_eq!(stored_model(&base), merged);
        assert_eq!(base.model_version, Some(MODEL_VERSION as i32));
        assert_eq!(stored_map(&base), map);
        assert_eq!(history(&w, dev, "app").len(), 2);
        assert!(history(&w, dev, "gone").is_empty());
        // The current base is the merge's.
        let (loaded, loaded_map) = w.load_base(&w.metadata.branch(dev), "app").await.unwrap();
        assert_eq!((loaded, loaded_map), (merged.clone(), map.clone()));
        // Only a merge or an update writes this way.
        let err = w
            .record_merge_base(
                dev,
                "app",
                &merged,
                &map,
                BaseSource::Create,
                (&timeline, "0/5000000"),
                &timeline,
                "0/5000000",
            )
            .await
            .unwrap_err();
        assert!(err.contains("create"), "{err}");
        assert_eq!(history(&w, dev, "app").len(), 2);
    }

    #[tokio::test]
    async fn restore_before_a_keep_merge_restores_the_older_base() {
        let (w, journal) = worker();
        let (db, dev) = ready_dev(&w, &journal).await;
        let old = w.metadata.branch(dev).timeline_id.unwrap();
        set_branch_head(&w, db, dev, Lsn(0x0700_0000));
        keep_merge(&w, dev).await;
        assert_eq!(stored(&w, dev, "app").unwrap().source, BaseSource::Merge);

        let new = restore_to(&w, dev, "0/3000000").await;

        let base = stored(&w, dev, "app").unwrap();
        assert_eq!(
            base.source,
            BaseSource::Create,
            "the copy keeps the original's source"
        );
        assert_eq!(
            (
                base.branch_timeline_id.as_deref(),
                base.branch_lsn.as_deref()
            ),
            (Some(new.as_str()), Some("0/3000000")),
            "positioned at the restored timeline's start"
        );
        assert_eq!(
            (base.timeline_id.as_deref(), base.lsn.as_deref()),
            (Some(old.as_str()), Some("0/2000000")),
            "read where the original was"
        );
        assert_eq!(stored_model(&base), model("orders"));
        assert_eq!(
            stored_map(&base),
            IdentityMap::self_mapped(&model("orders"))
        );
        assert_eq!(history(&w, dev, "app").len(), 3);
        // The next merge compares against it, as stored.
        journal.lock().unwrap().clear();
        let (loaded, map) = w.load_base(&w.metadata.branch(dev), "app").await.unwrap();
        assert_eq!(loaded, model("orders"));
        assert_eq!(map, IdentityMap::self_mapped(&model("orders")));
        assert!(!has(&journal, "metadata.create_static_endpoint"));
    }

    #[tokio::test]
    async fn restore_after_a_keep_merge_keeps_it() {
        let (w, journal) = worker();
        let (db, dev) = ready_dev(&w, &journal).await;
        let old = w.metadata.branch(dev).timeline_id.unwrap();
        set_branch_head(&w, db, dev, Lsn(0x0700_0000));
        let map = keep_merge(&w, dev).await;

        let new = restore_to(&w, dev, "0/6000000").await;

        let base = stored(&w, dev, "app").unwrap();
        assert_eq!(base.source, BaseSource::Merge);
        assert_eq!(
            (
                base.branch_timeline_id.as_deref(),
                base.branch_lsn.as_deref()
            ),
            (Some(new.as_str()), Some("0/6000000"))
        );
        assert_eq!(
            (base.timeline_id.as_deref(), base.lsn.as_deref()),
            (Some(old.as_str()), Some("0/5000000"))
        );
        assert_eq!(stored_model(&base), model("orders_merged"));
        assert_eq!(stored_map(&base), map);
        let (loaded, _) = w.load_base(&w.metadata.branch(dev), "app").await.unwrap();
        assert_eq!(loaded, model("orders_merged"));
    }

    /// A restore resumed after its detach timed out takes the point its timeline was made at,
    /// not one resolved again.
    #[tokio::test]
    async fn a_resumed_restore_copies_the_base_at_its_timelines_point() {
        let (w, journal) = worker();
        let (db, dev) = ready_dev(&w, &journal).await;
        set_branch_head(&w, db, dev, Lsn(0x0700_0000));
        keep_merge(&w, dev).await;
        *w.storage.detach.lock().unwrap() = crate::fakes::FakeDetach::TimesOut;
        start_restore(&w, dev, "0/3000000").await.unwrap_err();
        *w.storage.detach.lock().unwrap() = crate::fakes::FakeDetach::Works;

        let new = restore_to(&w, dev, "0/6000000").await;

        let base = stored(&w, dev, "app").unwrap();
        assert_eq!(base.source, BaseSource::Create);
        assert_eq!(
            (
                base.branch_timeline_id.as_deref(),
                base.branch_lsn.as_deref()
            ),
            (Some(new.as_str()), Some("0/3000000"))
        );
    }

    #[tokio::test]
    async fn restore_with_no_entry_leaves_no_base() {
        let (w, journal) = worker();
        let (db, dev) = ready_dev(&w, &journal).await;
        set_branch_head(&w, db, dev, Lsn(0x0700_0000));
        // The create's base was never written; only a later merge's is.
        w.metadata.state().schema_bases.clear();
        keep_merge(&w, dev).await;

        let new = restore_to(&w, dev, "0/3000000").await;

        let entries = history(&w, dev, "app");
        assert_eq!(entries.len(), 1, "nothing appended");
        assert_ne!(entries[0].branch_timeline_id.as_deref(), Some(new.as_str()));
        // The merge offers re-baseline (E17): the newer merge's base isn't used.
        journal.lock().unwrap().clear();
        let err = w
            .load_base(&w.metadata.branch(dev), "app")
            .await
            .unwrap_err();
        assert!(matches!(err, BaseError::Unavailable(_)), "{err:?}");
        assert!(!has(&journal, "metadata.create_static_endpoint"));
    }

    /// T14 I1: a restore to a point before an earlier restore's walks the chain of restored
    /// timelines back (at any depth) to the base current there, through a timeline whose own
    /// first entry is after the point.
    #[tokio::test]
    async fn a_second_restore_before_the_first_walks_the_restore_chain() {
        let (w, journal) = worker();
        let (db, dev) = ready_dev(&w, &journal).await;
        let t1 = w.metadata.branch(dev).timeline_id.unwrap();
        set_branch_head(&w, db, dev, Lsn(0x0700_0000));
        let map = keep_merge(&w, dev).await;

        // t1 -> t2 at 0/6000000: the merge's base (0/5000000) is copied.
        let t2 = restore_to(&w, dev, "0/6000000").await;
        let on_t2 = stored(&w, dev, "app").unwrap();
        assert_eq!(on_t2.source, BaseSource::Merge);
        assert_eq!(stored_map(&on_t2), map);
        // t2 -> t3 at 0/3000000: t2's only entry is at 0/6000000, so the walk goes back to t1
        // at 0/3000000, where the create's base (0/2000000) was current.
        let t3 = restore_to(&w, dev, "0/3000000").await;
        let on_t3 = stored(&w, dev, "app").unwrap();
        // t3 -> t4 at 0/2800000: t3's copy is at 0/3000000; t2 has nothing that early; t1 does.
        let t4 = restore_to(&w, dev, "0/2800000").await;
        let on_t4 = stored(&w, dev, "app").unwrap();

        for (base, timeline, at) in [(&on_t3, &t3, "0/3000000"), (&on_t4, &t4, "0/2800000")] {
            assert_eq!(base.source, BaseSource::Create, "{at}");
            assert_eq!(
                (base.branch_timeline_id.as_deref(), base.branch_lsn.as_deref()),
                (Some(timeline.as_str()), Some(at))
            );
            assert_eq!(
                (base.timeline_id.as_deref(), base.lsn.as_deref()),
                (Some(t1.as_str()), Some("0/2000000")),
                "read where the create's base was"
            );
            assert_eq!(stored_model(base), model("orders"));
        }
        assert_ne!(t2, t3);
        // The next merge compares against it, without a live read.
        journal.lock().unwrap().clear();
        let (loaded, loaded_map) = w.load_base(&w.metadata.branch(dev), "app").await.unwrap();
        assert_eq!(loaded, model("orders"));
        assert_eq!(loaded_map, IdentityMap::self_mapped(&model("orders")));
        assert!(!has(&journal, "metadata.create_static_endpoint"));
    }

    /// T14 M7: an entry superseded on its timeline by one recorded more than the history
    /// window ago drops its model; a restore that still picks it reads it live from its
    /// location.
    #[tokio::test]
    async fn a_superseded_base_drops_its_model_and_is_read_live_when_picked() {
        let (w, journal) = worker();
        let (db, dev) = ready_dev(&w, &journal).await;
        set_branch_head(&w, db, dev, Lsn(0x0700_0000));
        let create_id = stored(&w, dev, "app").unwrap().id;
        keep_merge(&w, dev).await;
        // The merge's entry was recorded eight days ago (the window is seven); the next append
        // finds the create's entry superseded long enough.
        let merge_id = stored(&w, dev, "app").unwrap().id;
        edit_current(&w, dev, "app", |b| {
            b.recorded_at = Utc::now() - chrono::Duration::days(8);
        });
        let timeline = w.metadata.branch(dev).timeline_id.unwrap();
        w.record_merge_base(
            dev,
            "app",
            &model("orders_v3"),
            &IdentityMap::default(),
            BaseSource::Merge,
            (&timeline, "0/6000000"),
            &timeline,
            "0/6000000",
        )
        .await
        .unwrap();
        let entries = history(&w, dev, "app");
        let by_id = |id: i64| entries.iter().find(|b| b.id == id).unwrap().clone();
        assert_eq!(by_id(create_id).model, None, "superseded long enough");
        assert_eq!(by_id(create_id).model_version, None);
        assert_eq!(by_id(create_id).lsn.as_deref(), Some("0/2000000"), "its location stays");
        assert!(by_id(merge_id).model.is_some(), "superseded only recently");

        // A restore before the merge picks the create's entry: read live at its location.
        restore_to(&w, dev, "0/3000000").await;
        journal.lock().unwrap().clear();
        let (loaded, _) = w.load_base(&w.metadata.branch(dev), "app").await.unwrap();
        assert_eq!(loaded, model("orders"));
        assert!(has(&journal, "metadata.create_static_endpoint"));
    }

    /// T14 M4: a static compute isn't cleaned up while a base read through it may still run.
    #[test]
    fn static_compute_cleanup_outlasts_a_base_read() {
        let read = crate::provision::READY_TIMEOUT + CATALOG_READ_TIMEOUT;
        assert!(Duration::from_secs(STALE_STATIC_COMPUTE_SECS as u64) > read * 2);
    }

    /// T7 review minor 3: a database only the old timeline had is skipped, and its entries go.
    #[tokio::test]
    async fn reset_skips_a_database_missing_on_the_new_timeline() {
        let (w, journal) = worker();
        let db = ready_database(&w).await;
        let main = w.metadata.main(db);
        let dev = pending_branch(&w, db, "dev", main.id);
        w.computes
            .answer("SELECT 'd',", &databases(&["app", "old"]));
        w.computes
            .answer_in("app", CATALOG_READ, &catalog("orders"));
        w.computes.answer_in("old", CATALOG_READ, &catalog("items"));
        w.handle_branch_create(dev, None).await.unwrap();
        assert_eq!(history(&w, dev, "old").len(), 1);
        set_main_head(&w, db, Lsn(0x0300_0000));
        w.metadata.state().branches.get_mut(&dev).unwrap().status = BranchStatus::Resetting;
        // `old` is still listed, but the new timeline doesn't have it. (The fake fails every
        // catalog read with this message; only `old`'s names it.)
        *w.computes.fail_sql_containing.lock().unwrap() = Some((
            CATALOG_READ.into(),
            "psql: error: connection to server on socket \"/tmp/.s.PGSQL.55433\" failed: \
             FATAL:  database \"old\" does not exist"
                .into(),
        ));
        journal.lock().unwrap().clear();

        w.handle_branch_reset_to_parent(dev).await.unwrap();

        let b = w.metadata.branch(dev);
        assert_eq!(b.status, BranchStatus::Ready, "{:?}", b.error);
        assert!(
            history(&w, dev, "old").is_empty(),
            "{:?}",
            history(&w, dev, "old")
        );
        assert!(!has(
            &journal,
            &format!("metadata.append_schema_base {dev} old")
        ));
        // `app`'s read failed with a message about another database: its location is kept.
        let app = stored(&w, dev, "app").unwrap();
        assert_eq!((app.source, app.model.clone()), (BaseSource::Reset, None));
        assert_eq!(app.timeline_id, b.timeline_id);
    }

    // --- P6 -----------------------------------------------------------------------------

    /// The exclusion lists `chelabase-schema` copied from the worker still match it.
    #[test]
    fn exclusion_lists_match_worker_constants() {
        use crate::app_users_sql::{MANAGED_COLUMNS, USERS_SYNC_ROLE, users_bootstrap_sql};
        use chelabase_schema::exclusions as ex;

        assert_eq!(ex::USERS_MANAGED_COLUMNS, MANAGED_COLUMNS);
        assert_eq!(ex::USERS_SYNC_ROLE, USERS_SYNC_ROLE);
        assert!(ex::RESERVED_ROLES.contains(&USERS_SYNC_ROLE));
        assert!(ex::USERS_GRANTEES.contains(&USERS_SYNC_ROLE));
        assert_eq!(
            ex::MEMBER_ROLE_PREFIX,
            chelabase_types::role_name::MEMBER_ROLE_PREFIX
        );
        assert!(crate::resync::resync_sql("postgres").contains(ex::MEMBER_ROLE_PREFIX));
        // The users machinery's names, as the worker's bootstrap writes them.
        let users = users_bootstrap_sql("public", "users", "SCRAM-SHA-256$x", crate::OWNER_ROLE);
        for name in ex::USERS_GUARD_TRIGGERS
            .iter()
            .chain(&ex::USERS_POLICIES)
            .chain([&ex::USERS_EMAIL_INDEX])
        {
            assert!(users.contains(name), "users_bootstrap_sql lacks {name}");
        }
        // The data API's roles, as provisioning creates them.
        let roles = crate::provision::bootstrap_sql(false);
        for name in ["anon", "authenticated", "authenticator"] {
            assert!(ex::RESERVED_ROLES.contains(&name));
            assert!(
                roles.contains(&format!("rolname = '{name}'")),
                "bootstrap_sql lacks {name}"
            );
        }
    }
}
