//! The compute job: `computing` → `draft`.
//!
//! Reads the target's digest (E4, before its model, so a change in between can only make the
//! merge stale), the target's and the source's models, and the base of the child branch (the
//! source into a parent, the target from one), or the target's model with an empty identity
//! map on a re-baseline (E12c, E17). Then the analysis, the resolutions that still apply
//! (E12e), and, when no conflict is open, the merged model and the plan, stored with the model
//! the target should read back afterwards (P7, from the target's default privileges with the
//! data API's rows) for the rehearsal's and the apply's drift check.

use chelabase_metadata::{
    BranchRecord, MergeDirection, MergeRecord, MergeStatus, MergeStatusPatch, ONLINE_PHASE,
};
use chelabase_neon::{ComputeBackend, SqlOpts};
use chelabase_schema::analysis::{
    Analysis, Choice, ConflictKind, MergeError, Resolution, analyze, merge,
};
use chelabase_schema::catalog::{default_acl_sql, parse_catalog};
use chelabase_schema::compare::{expected_model, own_sql_keys};
use chelabase_schema::fingerprint::precheck_fingerprint;
use chelabase_schema::plan::{admin_digest_sql, plan};
use chelabase_schema::steps::UserStep;
use chelabase_schema::{Direction, IdentityMap, SchemaError, fingerprint};
use chelabase_types::BranchId;
use serde_json::Value;
use tracing::info;
use uuid::Uuid;

use super::{StoredPlan, plan_options};
use futures_util::future::BoxFuture;

use crate::Worker;
use crate::branch_job::Attempt;
use crate::ports::{CacheNotifier, SpecPusher, Storage, WorkerMetadata};
use crate::schema_base::{BaseError, CATALOG_READ_TIMEOUT};

/// Why a compute gave no analysis.
enum ComputeError {
    /// No base can be had (E17): the merge offers re-baseline.
    BaseUnavailable(String),
    /// May work on a redelivery (metadata, a compute that doesn't answer).
    Transient(String),
    /// Won't work on a redelivery.
    Permanent(String),
}

/// What [`Worker::merge_compute`]'s `error` says when no base can be had; the API documents it.
pub(crate) const BASE_UNAVAILABLE: &str = "merge_base_unavailable";

/// A recompute of a merge whose plan's transaction is on the target (T14 F3, I2): it stays as
/// it was planned, failed in the online phase.
const PHASE1_ON_TARGET: &str = "merge_half_applied: an earlier apply's transaction is on the \
                                target, so the merge isn't computed again; retry it (its online \
                                steps run) or abandon it";

/// The choices a conflict of `kind` can take (E12b): column order takes only "target's columns
/// first" or own SQL; own SQL must say something.
fn valid_choice(kind: ConflictKind, choice: &Choice) -> bool {
    match choice {
        Choice::OwnSql(sql) => !sql.trim().is_empty(),
        Choice::TargetColumnsFirst => kind == ConflictKind::ColumnOrder,
        Choice::KeepTarget | Choice::TakeSource => kind != ConflictKind::ColumnOrder,
    }
}

/// The stored resolutions that still answer a conflict of `analysis`, in their order, the last
/// one per conflict (T3 ruling 9). Dropped (T10: never counted as resolving): entries that
/// don't parse, ids that match no conflict, choices the conflict can't take, and (E12e) any
/// whose conflict changed kind or snapshots since `previous`, the analysis the user chose
/// against; with no previous analysis nothing can be compared, so nothing is kept.
fn kept_resolutions(
    stored: &Value,
    previous: Option<&Analysis>,
    analysis: &Analysis,
) -> Vec<Resolution> {
    let mut kept: Vec<Resolution> = Vec::new();
    let parsed = stored
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| serde_json::from_value::<Resolution>(v.clone()).ok());
    for r in parsed {
        let Some(now) = analysis.conflicts.iter().find(|c| c.id == r.conflict) else {
            continue;
        };
        let Some(before) = previous.and_then(|p| p.conflicts.iter().find(|c| c.id == r.conflict))
        else {
            continue;
        };
        let unchanged = before.kind == now.kind
            && before.base == now.base
            && before.target == now.target
            && before.source == now.source;
        if !unchanged || !valid_choice(now.kind, &r.choice) {
            continue;
        }
        kept.retain(|k| k.conflict != r.conflict);
        kept.push(r);
    }
    kept
}

/// A planner error for the merge's `error`: for refused statements, the count, then one line per
/// statement (`statement <n>: step <id>: <reason>`, which Studio's `stepErrors` reads per step).
fn plan_error(e: &SchemaError) -> String {
    match e {
        SchemaError::Steps(errors) => {
            let reasons: Vec<String> = errors
                .iter()
                .map(|r| r.to_string().replace(['\n', '\r'], " "))
                .collect();
            format!("{e}:\n{}", reasons.join("\n"))
        }
        other => other.to_string(),
    }
}

/// An `md5` in hex, as the digest query prints it.
fn is_digest(s: &str) -> bool {
    s.len() == 32 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

impl<M: WorkerMetadata, S: Storage, C: ComputeBackend, N: CacheNotifier, P: SpecPusher>
    Worker<M, S, C, N, P>
{
    /// Delivery `attempt` of a compute job. The claim is taken first, then the merge's
    /// inputs are read; the result moves it to `draft` only while it's still `computing` (a
    /// cancel meanwhile wins). A transient failure with retries left is redelivered; otherwise
    /// the merge becomes a draft with the error and no plan (the analysis stays, unless no base
    /// could be had).
    pub(crate) fn merge_compute(&self, id: Uuid, attempt: Attempt) -> BoxFuture<'_, Result<(), String>> {
        Box::pin(self.merge_compute_inner(id, attempt))
    }

    async fn merge_compute_inner(&self, id: Uuid, attempt: Attempt) -> Result<(), String> {
        let Some(record) = self.claim_merge(id, MergeStatus::Computing).await? else {
            info!(merge_id = %id, "the merge isn't computing; nothing to compute");
            return Ok(());
        };
        // T14 F3 (I2, E27): a merge whose phase 1 is on the target (a `tx` failure the marker
        // couldn't settle, then a recompute) is half applied: never planned again, so no later
        // apply skips a new plan's phase 1. A marker that can't be read redelivers the job.
        if self.phase1_on_target(&record).await? {
            let patch = MergeStatusPatch {
                error: Some(Some(PHASE1_ON_TARGET.to_string())),
                failed_phase: Some(Some(ONLINE_PHASE.to_string())),
                ..Default::default()
            };
            let moved = self
                .metadata
                .set_merge_status(id, &[MergeStatus::Computing], MergeStatus::Failed, patch)
                .await
                .map_err(|e| format!("failing merge {id} in the online phase: {e}"))?;
            if moved {
                self.log_status(id, MergeStatus::Failed).await;
            }
            return Ok(());
        }
        let outcome = self
            .keep_merge_claimed(id, MergeStatus::Computing, self.compute_draft(&record))
            .await;
        // A failed compute drops the plan (it may not match what was saved) but keeps the
        // analysis: the resolutions were chosen against it, and the next recompute compares
        // with it to keep them (E12e). Without a base there is no analysis to keep.
        let without_plan = |error: String| MergeStatusPatch {
            plan: Some(Value::Null),
            error: Some(Some(error)),
            ..Default::default()
        };
        let patch = match outcome {
            Ok(patch) => patch,
            Err(ComputeError::Transient(e)) if attempt.retries_left() => return Err(e),
            Err(ComputeError::BaseUnavailable(why)) => MergeStatusPatch {
                analysis: Some(Value::Null),
                rebaselined: Some(false),
                ..without_plan(format!("{BASE_UNAVAILABLE}: {why}"))
            },
            Err(ComputeError::Transient(e) | ComputeError::Permanent(e)) => without_plan(e),
        };
        match self
            .metadata
            .set_merge_status(id, &[MergeStatus::Computing], MergeStatus::Draft, patch)
            .await
        {
            Ok(true) => Ok(()),
            Ok(false) => {
                info!(merge_id = %id, "the merge moved on during its compute; its result is dropped");
                Ok(())
            }
            Err(e) => Err(format!("storing merge {id}'s draft: {e}")),
        }
    }

    /// The draft's analysis, resolutions, plan and fingerprints for `record`.
    async fn compute_draft(&self, record: &MergeRecord) -> Result<MergeStatusPatch, ComputeError> {
        let (source, source_ep) = self.merge_branch(record.source_branch_id).await?;
        let (target, target_ep) = self.merge_branch(record.target_branch_id).await?;
        let db = record.database_name.as_str();
        let direction = match record.direction {
            MergeDirection::IntoParent => Direction::IntoParent,
            MergeDirection::FromParent => Direction::FromParent,
        };
        let child = match direction {
            Direction::IntoParent => &source,
            Direction::FromParent => &target,
        };
        let rebaseline = record.options.get("rebaseline").and_then(Value::as_bool) == Some(true);

        // E4: the digest first; E15: on cloud_admin only through `admin_digest_sql`.
        let digest_opts = SqlOpts {
            timeout: CATALOG_READ_TIMEOUT,
            ..SqlOpts::admin_default()
        };
        let digest = self
            .computes
            .admin_sql_with(&target_ep, db, &admin_digest_sql(), &digest_opts)
            .await
            .map_err(|e| ComputeError::Transient(format!("reading the target's digest: {e:#}")))?
            .trim()
            .to_string();
        if !is_digest(&digest) {
            return Err(ComputeError::Transient(format!(
                "the target's digest is unreadable: {digest:?}"
            )));
        }
        let target_model = self
            .read_schema(&target_ep, db)
            .await
            .map_err(ComputeError::Transient)?;
        // P7: the grants new objects get from the target's default privileges, the data API's
        // included, for the model the target should read back after the plan.
        let default_acl = self
            .computes
            .admin_sql_with(&target_ep, db, &default_acl_sql(), &digest_opts)
            .await
            .map_err(|e| format!("reading the target's default privileges: {e:#}"))
            .and_then(|out| {
                parse_catalog(out.trim())
                    .map_err(|e| format!("reading the target's default privileges: {e}"))
            })
            .map_err(ComputeError::Transient)?;
        let source_model = self
            .read_schema(&source_ep, db)
            .await
            .map_err(ComputeError::Transient)?;
        let (base, map) = if rebaseline {
            (target_model.clone(), IdentityMap::default())
        } else {
            self.load_base(child, db).await.map_err(|e| match e {
                BaseError::Unavailable(why) => ComputeError::BaseUnavailable(why),
                BaseError::Transient(why) => ComputeError::Transient(why),
            })?
        };

        let analysis = analyze(&base, &target_model, &source_model, &map, direction);
        let previous: Option<Analysis> = record
            .analysis
            .clone()
            .and_then(|v| serde_json::from_value(v).ok());
        let mut kept = kept_resolutions(&record.resolutions, previous.as_ref(), &analysis);
        let user_steps = serde_json::from_value::<Vec<UserStep>>(record.user_steps.clone());
        let opts = plan_options(record);
        // T14 M4: resolutions the merge refuses, with why (`<conflict>: <reason>`).
        let mut refused: Vec<String> = Vec::new();
        let (stored_plan, error) = loop {
            let open = analysis
                .conflicts
                .iter()
                .any(|c| !kept.iter().any(|r| r.conflict == c.id));
            if open {
                break (None, None);
            }
            let merged = match merge(
                &analysis,
                &kept,
                &base,
                &target_model,
                &source_model,
                &map,
                direction,
            ) {
                Ok(merged) => merged,
                // Not caught above (a choice the merge refuses): dropped, so it's open again.
                Err(MergeError::InvalidChoice { conflict, reason }) => {
                    kept.retain(|r| r.conflict != conflict);
                    refused.push(format!("{}: {reason}", conflict.0));
                    continue;
                }
                Err(e) => break (None, Some(e.to_string())),
            };
            let steps = match &user_steps {
                Ok(steps) => steps,
                Err(e) => break (None, Some(format!("the merge's steps are unreadable: {e}"))),
            };
            match plan(&target_model, &merged, &analysis, steps, &opts) {
                Ok(p) => {
                    let precheck = precheck_fingerprint(&target_model, &p.steps);
                    break (
                        Some(StoredPlan {
                            precheck_fingerprint: Some(precheck),
                            steps: p.steps,
                            target_digest: digest.clone(),
                            expected: Some(expected_model(&target_model, &merged, &default_acl)),
                            own_sql: own_sql_keys(&analysis, &merged),
                            follows_rename: merged.follows_rename.clone(),
                            expected_rehearsed: false,
                        }),
                        None,
                    );
                }
                Err(e) => break (None, Some(plan_error(&e))),
            }
        };

        // The refused resolutions are named (the conflict is open again), before any other error.
        let error = if refused.is_empty() {
            error
        } else {
            let named = format!("merge_resolution_refused: {}", refused.join("; "));
            Some(match error {
                Some(e) => format!("{named}\n{e}"),
                None => named,
            })
        };
        let json = |v: Result<Value, serde_json::Error>| {
            v.map_err(|e| ComputeError::Permanent(format!("serializing the merge: {e}")))
        };
        Ok(MergeStatusPatch {
            analysis: Some(json(serde_json::to_value(&analysis))?),
            plan: Some(match &stored_plan {
                Some(p) => json(serde_json::to_value(p))?,
                None => Value::Null,
            }),
            resolutions: Some(json(serde_json::to_value(&kept))?),
            error: Some(error),
            base_fingerprint: Some(fingerprint(&base)),
            source_fingerprint: Some(fingerprint(&source_model)),
            target_fingerprint: Some(fingerprint(&target_model)),
            rebaselined: Some(rebaseline),
            ..Default::default()
        })
    }

    /// A merge's branch and its endpoint.
    async fn merge_branch(&self, id: Uuid) -> Result<(BranchRecord, String), ComputeError> {
        let branch = self
            .metadata
            .get_branch(BranchId::from(id))
            .await
            .map_err(|e| ComputeError::Transient(format!("reading branch {id}: {e}")))?
            .ok_or_else(|| ComputeError::Permanent(format!("branch {id} no longer exists")))?;
        let ep = branch.endpoint_id.clone().ok_or_else(|| {
            ComputeError::Transient(format!("branch {} has no compute", branch.name))
        })?;
        Ok((branch, ep))
    }
}

#[cfg(test)]
mod tests {
    use chelabase_metadata::{MergeDirection, MergeStatus};
    use chelabase_nats::MergeJob;
    use chelabase_schema::catalog::parse_catalog;
    use chelabase_schema::fingerprint;
    use serde_json::{Value, json};
    use uuid::Uuid;

    use crate::branch_job::Attempt;
    use crate::fakes::{FakeWorker, worker};
    use crate::merge::testkit::{
        CATALOG_READ, DIGEST, DIGEST_READ, Setup, answer_catalog, catalog, orders, setup, table,
    };

    /// `dev` into main, `computing` as the API leaves it.
    fn computing(w: &FakeWorker, s: &Setup) -> Uuid {
        w.metadata.insert_merge(
            s.db,
            s.dev.id,
            s.main.id,
            MergeDirection::IntoParent,
            MergeStatus::Computing,
        )
    }

    async fn compute(w: &FakeWorker, id: Uuid, attempt: Attempt) -> Result<(), String> {
        w.handle_merge_job(MergeJob::Compute { merge_id: id }, attempt)
            .await
    }

    fn conflict_ids(analysis: &Value) -> Vec<String> {
        analysis["conflicts"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["id"].as_str().unwrap().to_string())
            .collect()
    }

    /// Drops dev's stored base and its branch point: no base can be had.
    fn lose_the_base(w: &FakeWorker, s: &Setup) {
        let mut state = w.metadata.state();
        state.schema_bases.retain(|b| b.branch_id != s.dev.id);
        state.branches.get_mut(&s.dev.id).unwrap().branched_at_lsn = None;
    }

    /// What the user does between computes: saves `resolutions`, and the API moves the merge
    /// back to `computing`.
    fn save_and_recompute(w: &FakeWorker, id: Uuid, resolutions: Value) {
        let mut state = w.metadata.state();
        let m = state.merges.get_mut(&id).unwrap();
        m.resolutions = resolutions;
        m.status = MergeStatus::Computing;
    }

    fn items() -> Value {
        table(16500, "items", &[(1, "id", "bigint", 20)])
    }

    #[tokio::test]
    async fn compute_stores_analysis_and_fingerprints() {
        let (w, _journal) = worker();
        let s = setup(&w).await;
        answer_catalog(&w, s.main_ep(), &[orders()]);
        answer_catalog(&w, s.dev_ep(), &[orders(), items()]);
        let id = computing(&w, &s);

        compute(&w, id, Attempt(1)).await.unwrap();

        let m = w.metadata.merge_row(id);
        assert_eq!(m.status, MergeStatus::Draft, "{:?}", m.error);
        assert_eq!(m.error, None);
        assert_eq!(m.claimed_at, None);
        assert!(!m.rebaselined);
        let base = parse_catalog(&catalog(&[orders()])).unwrap();
        let source = parse_catalog(&catalog(&[orders(), items()])).unwrap();
        assert_eq!(m.base_fingerprint, Some(fingerprint(&base)));
        assert_eq!(m.target_fingerprint, Some(fingerprint(&base)));
        assert_eq!(m.source_fingerprint, Some(fingerprint(&source)));
        let analysis = m.analysis.expect("an analysis");
        assert!(conflict_ids(&analysis).is_empty());
        assert!(!analysis["items"].as_array().unwrap().is_empty());
        let plan = m.plan.expect("a plan: nothing is open");
        assert_eq!(plan["target_digest"], json!(DIGEST));
        let steps = plan["steps"].as_array().unwrap();
        assert!(
            steps
                .iter()
                .any(|s| s["sql"].as_str().unwrap().contains("CREATE TABLE")),
            "{plan}"
        );
        // E4: the digest is read before the target's model, so a change in between can only
        // make the merge stale, never pass the guard with an older model.
        let sql = w.computes.sql.lock().unwrap().clone();
        let at = |needle: &str| {
            sql.iter()
                .position(|(ep, _, q)| ep == s.main_ep() && q.contains(needle))
                .unwrap_or_else(|| panic!("{needle} not run on main"))
        };
        assert!(at(DIGEST_READ) < at(CATALOG_READ));
        // E15: catalog and digest reads stay on cloud_admin.
        assert!(
            w.computes
                .sql_opts
                .lock()
                .unwrap()
                .iter()
                .all(|o| o.user.is_none())
        );
    }

    #[tokio::test]
    async fn compute_without_base_offers_rebaseline() {
        let (w, _journal) = worker();
        let s = setup(&w).await;
        answer_catalog(&w, s.main_ep(), &[orders()]);
        answer_catalog(&w, s.dev_ep(), &[orders(), items()]);
        lose_the_base(&w, &s);
        let id = computing(&w, &s);

        compute(&w, id, Attempt(1)).await.unwrap();

        let m = w.metadata.merge_row(id);
        assert_eq!(m.status, MergeStatus::Draft);
        let error = m.error.unwrap_or_default();
        assert!(error.starts_with("merge_base_unavailable: "), "{error}");
        assert_eq!((m.analysis, m.plan), (None, None));
        assert!(!m.rebaselined);
    }

    #[tokio::test]
    async fn rebaseline_uses_target_as_base() {
        let (w, _journal) = worker();
        let s = setup(&w).await;
        let target = [orders(), table(16600, "audit", &[(1, "id", "bigint", 20)])];
        answer_catalog(&w, s.main_ep(), &target);
        answer_catalog(&w, s.dev_ep(), &[orders(), items()]);
        lose_the_base(&w, &s);
        let id = computing(&w, &s);
        w.metadata.state().merges.get_mut(&id).unwrap().options =
            json!({ "lock_timeout_ms": 5000, "after": "keep", "rebaseline": true });

        compute(&w, id, Attempt(1)).await.unwrap();

        let m = w.metadata.merge_row(id);
        assert_eq!(m.status, MergeStatus::Draft, "{:?}", m.error);
        assert_eq!(m.error, None);
        assert!(m.rebaselined);
        assert_eq!(m.base_fingerprint, m.target_fingerprint);
        // A two-way diff: the target's own table reads as one the source dropped.
        let plan = m.plan.expect("a plan");
        assert!(plan.to_string().contains("DROP TABLE"), "{plan}");
    }

    /// Main and dev both created `t`, differently: one create collision.
    fn collide(w: &FakeWorker, s: &Setup, target_type: (&str, u32)) {
        let (ty, oid) = target_type;
        answer_catalog(
            w,
            s.main_ep(),
            &[orders(), table(16500, "t", &[(1, "id", ty, oid)])],
        );
        answer_catalog(
            w,
            s.dev_ep(),
            &[orders(), table(16500, "t", &[(1, "id", "text", 25)])],
        );
    }

    #[tokio::test]
    async fn recompute_keeps_resolutions_whose_ids_remain() {
        let (w, _journal) = worker();
        let s = setup(&w).await;
        collide(&w, &s, ("bigint", 20));
        let id = computing(&w, &s);
        compute(&w, id, Attempt(1)).await.unwrap();
        let m = w.metadata.merge_row(id);
        let ids = conflict_ids(m.analysis.as_ref().unwrap());
        assert_eq!(ids.len(), 1, "{ids:?}");
        assert_eq!(m.plan, None, "a conflict is open");

        let keep = json!({ "conflict": ids[0], "choice": "keep_target" });
        save_and_recompute(
            &w,
            id,
            json!([keep, { "conflict": "gone", "choice": "take_source" }]),
        );
        compute(&w, id, Attempt(1)).await.unwrap();

        let m = w.metadata.merge_row(id);
        assert_eq!(m.status, MergeStatus::Draft, "{:?}", m.error);
        assert_eq!(m.resolutions, json!([keep]));
        assert!(m.plan.is_some(), "every conflict is resolved");
    }

    // E12e: a stored choice can't change meaning when its conflict changed.
    #[tokio::test]
    async fn recompute_drops_a_resolution_whose_conflict_changed() {
        let (w, _journal) = worker();
        let s = setup(&w).await;
        collide(&w, &s, ("bigint", 20));
        let id = computing(&w, &s);
        compute(&w, id, Attempt(1)).await.unwrap();
        let cid = conflict_ids(w.metadata.merge_row(id).analysis.as_ref().unwrap())[0].clone();
        save_and_recompute(
            &w,
            id,
            json!([{ "conflict": cid, "choice": "keep_target" }]),
        );
        // Main changed its `t` meanwhile: same conflict id, other snapshots.
        collide(&w, &s, ("integer", 23));

        compute(&w, id, Attempt(1)).await.unwrap();

        let m = w.metadata.merge_row(id);
        assert_eq!(conflict_ids(m.analysis.as_ref().unwrap()), [cid]);
        assert_eq!(m.resolutions, json!([]));
        assert_eq!(m.plan, None);
    }

    // T10: the API checks resolutions by shape only, so the worker never counts an invalid
    // one as resolving: duplicates keep the last, a choice the conflict can't take is dropped.
    #[tokio::test]
    async fn invalid_resolutions_are_dropped_not_counted() {
        let (w, _journal) = worker();
        let s = setup(&w).await;
        collide(&w, &s, ("bigint", 20));
        let id = computing(&w, &s);
        compute(&w, id, Attempt(1)).await.unwrap();
        let cid = conflict_ids(w.metadata.merge_row(id).analysis.as_ref().unwrap())[0].clone();

        // Only a column order conflict takes `target_columns_first`; empty own SQL is invalid.
        for choice in [json!("target_columns_first"), json!({ "own_sql": "  " })] {
            save_and_recompute(&w, id, json!([{ "conflict": cid, "choice": choice }]));
            compute(&w, id, Attempt(1)).await.unwrap();
            let m = w.metadata.merge_row(id);
            assert_eq!(m.resolutions, json!([]), "{choice}");
            assert_eq!(m.plan, None, "{choice}");
        }

        let last = json!({ "conflict": cid, "choice": "keep_target" });
        save_and_recompute(
            &w,
            id,
            json!([{ "conflict": cid, "choice": "take_source" }, last, "not a resolution"]),
        );
        compute(&w, id, Attempt(1)).await.unwrap();
        let m = w.metadata.merge_row(id);
        assert_eq!(m.resolutions, json!([last]));
        assert!(m.plan.is_some());
    }

    // T10: a cancel landing during the compute wins; the compute's draft write is guarded.
    #[tokio::test]
    async fn a_cancel_during_compute_is_not_overwritten() {
        let (w, _journal) = worker();
        let s = setup(&w).await;
        answer_catalog(&w, s.main_ep(), &[orders()]);
        answer_catalog(&w, s.dev_ep(), &[orders(), items()]);
        let id = computing(&w, &s);
        w.metadata.state().before_merge_write = Some(Box::new(move |state| {
            state.merges.get_mut(&id).unwrap().status = MergeStatus::Cancelled;
        }));

        compute(&w, id, Attempt(1)).await.unwrap();

        let m = w.metadata.merge_row(id);
        assert_eq!(m.status, MergeStatus::Cancelled);
        assert_eq!((m.analysis, m.plan), (None, None));
    }

    #[tokio::test]
    async fn a_merge_no_longer_computing_is_left_alone() {
        let (w, journal) = worker();
        let s = setup(&w).await;
        let id = computing(&w, &s);
        w.metadata.state().merges.get_mut(&id).unwrap().status = MergeStatus::Cancelled;
        let before = w.metadata.merge_row(id);
        journal.lock().unwrap().clear();

        compute(&w, id, Attempt(1)).await.unwrap();

        assert_eq!(w.metadata.merge_row(id), before);
        assert!(
            !journal
                .lock()
                .unwrap()
                .iter()
                .any(|e| e.starts_with("compute.sql")),
            "nothing read"
        );
        // An unknown merge is acked too.
        compute(&w, Uuid::new_v4(), Attempt(1)).await.unwrap();
    }

    #[tokio::test]
    async fn a_failed_read_is_retried_then_recorded() {
        let (w, _journal) = worker();
        let s = setup(&w).await;
        answer_catalog(&w, s.main_ep(), &[orders()]);
        answer_catalog(&w, s.dev_ep(), &[orders(), items()]);
        *w.computes.fail_sql_containing.lock().unwrap() =
            Some((CATALOG_READ.to_string(), "connection refused".to_string()));
        let id = computing(&w, &s);

        let err = compute(&w, id, Attempt(1)).await.unwrap_err();
        assert!(err.contains("connection refused"), "{err}");
        assert_eq!(w.metadata.merge_row(id).status, MergeStatus::Computing);

        compute(&w, id, Attempt::LAST).await.unwrap();
        let m = w.metadata.merge_row(id);
        assert_eq!(m.status, MergeStatus::Draft);
        let error = m.error.unwrap_or_default();
        assert!(error.contains("connection refused"), "{error}");
        assert_eq!((m.analysis, m.plan), (None, None));
    }

    // A failed compute keeps the analysis the resolutions were chosen against, so the next
    // recompute can still keep them (E12e compares with it); only the plan goes.
    #[tokio::test]
    async fn resolutions_survive_a_failed_compute() {
        let (w, _journal) = worker();
        let s = setup(&w).await;
        collide(&w, &s, ("bigint", 20));
        let id = computing(&w, &s);
        compute(&w, id, Attempt(1)).await.unwrap();
        let cid = conflict_ids(w.metadata.merge_row(id).analysis.as_ref().unwrap())[0].clone();
        let keep = json!([{ "conflict": cid, "choice": "keep_target" }]);
        save_and_recompute(&w, id, keep.clone());
        compute(&w, id, Attempt(1)).await.unwrap();
        assert!(w.metadata.merge_row(id).plan.is_some());

        // The next compute fails for good.
        save_and_recompute(&w, id, keep.clone());
        *w.computes.fail_sql_containing.lock().unwrap() =
            Some((CATALOG_READ.to_string(), "connection refused".to_string()));
        compute(&w, id, Attempt::LAST).await.unwrap();
        let m = w.metadata.merge_row(id);
        assert_eq!(m.status, MergeStatus::Draft);
        assert!(m.error.is_some());
        assert!(m.analysis.is_some(), "the analysis stays");
        assert_eq!(m.plan, None, "the plan goes");
        assert_eq!(m.resolutions, keep);

        // The recompute after it keeps the resolution.
        *w.computes.fail_sql_containing.lock().unwrap() = None;
        save_and_recompute(&w, id, keep.clone());
        compute(&w, id, Attempt(1)).await.unwrap();
        let m = w.metadata.merge_row(id);
        assert_eq!(m.error, None);
        assert_eq!(m.resolutions, keep);
        assert!(m.plan.is_some());
    }

    // A user step the planner refuses: a draft with the analysis, the error, and no plan.
    #[tokio::test]
    async fn a_refused_user_step_leaves_a_draft_without_a_plan() {
        let (w, _journal) = worker();
        let s = setup(&w).await;
        answer_catalog(&w, s.main_ep(), &[orders()]);
        answer_catalog(&w, s.dev_ep(), &[orders(), items()]);
        let id = computing(&w, &s);
        w.metadata.state().merges.get_mut(&id).unwrap().user_steps =
            json!([{ "id": "u1", "after": null, "sql": "COMMIT" }]);

        compute(&w, id, Attempt(1)).await.unwrap();

        let m = w.metadata.merge_row(id);
        assert_eq!(m.status, MergeStatus::Draft);
        assert!(m.analysis.is_some());
        assert_eq!(m.plan, None);
        let error = m.error.unwrap_or_default();
        // Studio's `stepErrors` reads one refused statement per line, `… step <id>: <reason>`.
        let lines: Vec<&str> = error.lines().collect();
        assert_eq!(lines.len(), 2, "{error}");
        assert!(lines[1].contains("step u1: "), "{error}");
    }

    // T14 M4 (T8a M7): a resolution the merge refuses (taking a source that drops an enum
    // value) is dropped, and the draft says which and why.
    #[tokio::test]
    async fn a_refused_resolution_is_named_in_the_error() {
        use crate::merge::testkit::{catalog_all, enum_type};
        let (w, _journal) = worker();
        let s = setup(&w).await;
        let status = |labels: &[(u32, &str)]| enum_type(16300, "status", labels);
        let base = parse_catalog(&catalog_all(&[orders()], &[], &[status(&[(1, "a"), (2, "b")])])).unwrap();
        {
            let mut state = w.metadata.state();
            let entry = state.schema_bases.iter_mut().rev().find(|b| b.branch_id == s.dev.id).unwrap();
            entry.model = Some(serde_json::to_value(&base).unwrap());
            entry.identity_map = serde_json::to_value(chelabase_schema::IdentityMap::self_mapped(&base)).unwrap();
        }
        w.computes.answers_at.lock().unwrap().clear();
        w.computes.answer_at(s.main_ep(), CATALOG_READ, &catalog_all(&[orders()], &[], &[status(&[(1, "a"), (2, "b"), (3, "c")])]));
        w.computes.answer_at(s.dev_ep(), CATALOG_READ, &catalog_all(&[orders()], &[], &[status(&[(1, "a")])]));
        w.computes.answer_at(s.main_ep(), DIGEST_READ, &format!("{DIGEST}\n"));
        let id = computing(&w, &s);
        compute(&w, id, Attempt(1)).await.unwrap();
        let ids = conflict_ids(w.metadata.merge_row(id).analysis.as_ref().unwrap());
        assert_eq!(ids.len(), 1, "{ids:?}");

        save_and_recompute(&w, id, json!([{ "conflict": ids[0], "choice": "take_source" }]));
        compute(&w, id, Attempt(1)).await.unwrap();

        let m = w.metadata.merge_row(id);
        assert_eq!((m.resolutions.clone(), m.plan.clone()), (json!([]), None));
        let error = m.error.unwrap_or_default();
        assert!(error.starts_with("merge_resolution_refused: "), "{error}");
        assert!(error.contains(&ids[0]) && error.contains("enum values"), "{error}");
    }

    // The plan stores what the drift check compares against: the expected model (P7, the
    // target's default privileges with the data API's rows) and the own-SQL keys.
    #[tokio::test]
    async fn the_plan_carries_the_expected_model() {
        use crate::merge::testkit::DEFAULT_ACL_FILTER;
        let (w, _journal) = worker();
        let s = setup(&w).await;
        answer_catalog(&w, s.main_ep(), &[orders()]);
        answer_catalog(&w, s.dev_ep(), &[orders(), items()]);
        let id = computing(&w, &s);

        compute(&w, id, Attempt(1)).await.unwrap();

        let plan = w.metadata.merge_row(id).plan.expect("a plan");
        let expected: chelabase_schema::SchemaModel =
            serde_json::from_value(plan["expected"].clone()).expect("the expected model");
        assert!(expected.tables.iter().any(|t| t.name == "items"));
        assert_eq!(plan["own_sql"], json!([]));
        // On main: the model read (with the data API filter) and the default-ACL read (without).
        let sql = w.computes.sql.lock().unwrap().clone();
        let reads: Vec<bool> = sql
            .iter()
            .filter(|(ep, _, q)| ep == s.main_ep() && q.contains(CATALOG_READ))
            .map(|(_, _, q)| q.contains(DEFAULT_ACL_FILTER))
            .collect();
        assert_eq!(reads, [true, false], "the model, then the default ACL");
    }
}
