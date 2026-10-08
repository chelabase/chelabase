//! Finishing an applied merge (T8b; Part 4 "After apply"): `finishing` → `done` or
//! `applied_with_drift`. Idempotent, so a crash here is recovered by running it again.
//!
//! 1. The target is read back (`read_schema`, `cloud_admin`, E15) and compared with the plan's
//!    expected model: a difference other than what own SQL produced is drift.
//! 2. `result.created_roles`: the roles phase 0 created, each with the branch whose Connection
//!    panel reveals its password (U3).
//! 3. The after-merge choice (D11; into the parent only, an update from the parent always
//!    keeps): **keep** appends the source's schema as the child branch's base (`merge`, or
//!    `update` from the parent), located at the source's head and valid from the child's own
//!    position, with a full identity map (E6, E18); **reset** and **delete** run the usual
//!    flows with their usual refusals, and a refusal is recorded, never a failure. With drift
//!    the choice is skipped (recorded as such): the source and its base stay as they were.
//!
//! The `result` column (Studio's `readResult`): `{created_roles: [{name, branch_id}], drift:
//! [Drift], after: {choice, …}}`.

use chelabase_metadata::{
    BaseSource, BranchRecord, BranchStatus, ClaimOutcome, MergeDirection, MergeRecord,
    MergeStatus, MergeStatusPatch, merge_busy_refusal,
};
use chelabase_neon::{ComputeBackend, NeonId};
use chelabase_schema::compare::compare;
use chelabase_schema::plan::{Phase, RoleOp};
use chelabase_schema::{
    Direction, IdentityMap, IdentityPair, ObjAt, ObjClass, Oid, SchemaModel, Side, fingerprint,
};
use chelabase_types::BranchId;
use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use tracing::{info, warn};
use uuid::Uuid;

use super::phases::phase_name;
use super::{StoredPlan, stored_plan};
use crate::Worker;
use crate::branch_job::Attempt;
use crate::ports::{CacheNotifier, SpecPusher, Storage, WorkerMetadata};

/// `result.after` once a reset or delete has started (T14 M8): the choice, `started` and the
/// source's timeline then, keeping `requested` from `previous` (a `finishMerge` request), so
/// reconcile still finds a request whose job went away mid-run (T14 F3 fix round, M1).
fn started_after(previous: &Value, choice: &str, timeline: Option<String>) -> Value {
    let mut after = json!({ "choice": choice, "started": true, "timeline": timeline });
    if previous["requested"] == json!(true) {
        after["requested"] = json!(true);
    }
    after
}

/// The pair of base object `at` (the source's own, the new base being the source's schema),
/// matched on the target as `matched` (`None`: the target has no such object).
fn pair(direction: Direction, class: ObjClass, at: ObjAt, matched: Option<ObjAt>) -> IdentityPair {
    let (parent, branch) = match direction.source_side() {
        Side::Parent => (Some(at), matched),
        Side::Branch => (matched, Some(at)),
    };
    IdentityPair {
        class,
        base: at,
        parent,
        branch,
    }
}

/// The identity map of a merge's keep base (E6: full): every object of `source` (the new base)
/// paired with the target's object after the merge, or `None`. An object the old base knew is
/// followed through `old` (so a rename the target kept stays the same object), when the target
/// still has it; any other is matched by name (what the merge created has the target's own
/// OIDs, P2).
pub(crate) fn keep_identity_map(
    source: &SchemaModel,
    target: &SchemaModel,
    old: Option<&IdentityMap>,
    direction: Direction,
) -> IdentityMap {
    let (from, to) = (direction.source_side(), direction.target_side());
    let via_old = |class: ObjClass, oid: Oid, sub: Option<i16>| {
        let map = old?;
        let (base, base_sub) = map.to_base(from, class, oid, sub)?;
        map.from_base(to, class, base, base_sub)
    };
    let at = |oid: Oid| ObjAt::new(oid, None);
    let mut pairs: Vec<IdentityPair> = Vec::new();

    for s in &source.schemas {
        let matched = via_old(ObjClass::Schema, s.oid, None)
            .and_then(|(o, _)| target.schemas.iter().find(|x| x.oid == o))
            .or_else(|| target.schemas.iter().find(|x| x.name == s.name));
        pairs.push(pair(
            direction,
            ObjClass::Schema,
            at(s.oid),
            matched.map(|x| at(x.oid)),
        ));
    }
    for t in &source.tables {
        let tt = via_old(ObjClass::Table, t.oid, None)
            .and_then(|(o, _)| target.tables.iter().find(|x| x.oid == o))
            .or_else(|| {
                target
                    .tables
                    .iter()
                    .find(|x| x.schema == t.schema && x.name == t.name)
            });
        pairs.push(pair(
            direction,
            ObjClass::Table,
            at(t.oid),
            tt.map(|x| at(x.oid)),
        ));
        for c in &t.columns {
            let matched = tt.and_then(|x| {
                via_old(ObjClass::Column, t.oid, Some(c.attnum))
                    .filter(|(o, _)| *o == x.oid)
                    .and_then(|(_, n)| x.columns.iter().find(|xc| Some(xc.attnum) == n))
                    .or_else(|| x.columns.iter().find(|xc| xc.name == c.name))
                    .map(|xc| ObjAt::new(x.oid, Some(xc.attnum)))
            });
            pairs.push(pair(
                direction,
                ObjClass::Column,
                ObjAt::new(t.oid, Some(c.attnum)),
                matched,
            ));
        }
        for c in &t.constraints {
            let matched = tt.and_then(|x| {
                via_old(ObjClass::Constraint, c.oid, None)
                    .and_then(|(o, _)| x.constraints.iter().find(|xc| xc.oid == o))
                    .or_else(|| x.constraints.iter().find(|xc| xc.name == c.name))
            });
            pairs.push(pair(
                direction,
                ObjClass::Constraint,
                at(c.oid),
                matched.map(|x| at(x.oid)),
            ));
        }
        for i in &t.indexes {
            let matched = tt.and_then(|x| {
                via_old(ObjClass::Index, i.oid, None)
                    .and_then(|(o, _)| x.indexes.iter().find(|xi| xi.oid == o))
                    .or_else(|| x.indexes.iter().find(|xi| xi.name == i.name))
            });
            pairs.push(pair(
                direction,
                ObjClass::Index,
                at(i.oid),
                matched.map(|x| at(x.oid)),
            ));
        }
        for p in &t.policies {
            let matched = tt.and_then(|x| {
                via_old(ObjClass::Policy, p.oid, None)
                    .and_then(|(o, _)| x.policies.iter().find(|xp| xp.oid == o))
                    .or_else(|| x.policies.iter().find(|xp| xp.name == p.name))
            });
            pairs.push(pair(
                direction,
                ObjClass::Policy,
                at(p.oid),
                matched.map(|x| at(x.oid)),
            ));
        }
    }
    for e in &source.enums {
        let te = via_old(ObjClass::Enum, e.oid, None)
            .and_then(|(o, _)| target.enums.iter().find(|x| x.oid == o))
            .or_else(|| {
                target
                    .enums
                    .iter()
                    .find(|x| x.schema == e.schema && x.name == e.name)
            });
        pairs.push(pair(
            direction,
            ObjClass::Enum,
            at(e.oid),
            te.map(|x| at(x.oid)),
        ));
        for l in &e.labels {
            let matched = te.and_then(|x| {
                via_old(ObjClass::EnumLabel, l.oid, None)
                    .and_then(|(o, _)| x.labels.iter().find(|xl| xl.oid == o))
                    .or_else(|| x.labels.iter().find(|xl| xl.label == l.label))
            });
            pairs.push(pair(
                direction,
                ObjClass::EnumLabel,
                at(l.oid),
                matched.map(|x| at(x.oid)),
            ));
        }
    }
    for q in &source.sequences {
        let matched = via_old(ObjClass::Sequence, q.oid, None)
            .and_then(|(o, _)| target.sequences.iter().find(|x| x.oid == o))
            .or_else(|| {
                target
                    .sequences
                    .iter()
                    .find(|x| x.schema == q.schema && x.name == q.name)
            });
        pairs.push(pair(
            direction,
            ObjClass::Sequence,
            at(q.oid),
            matched.map(|x| at(x.oid)),
        ));
    }
    for r in &source.roles {
        let matched = via_old(ObjClass::Role, r.oid, None)
            .and_then(|(o, _)| target.roles.iter().find(|x| x.oid == o))
            .or_else(|| target.roles.iter().find(|x| x.name == r.name));
        pairs.push(pair(
            direction,
            ObjClass::Role,
            at(r.oid),
            matched.map(|x| at(x.oid)),
        ));
    }
    IdentityMap::from_pairs(pairs)
}

/// The after-merge choice (D11): `keep`, `reset` or `delete` into the parent; an update from
/// the parent always keeps.
fn after_choice(merge: &MergeRecord) -> &str {
    match merge.direction {
        MergeDirection::FromParent => "keep",
        MergeDirection::IntoParent => match merge.options.get("after").and_then(Value::as_str) {
            Some(choice @ ("reset" | "delete")) => choice,
            _ => "keep",
        },
    }
}

/// The base a keep records for: the child branch (the source into a parent, the target from
/// one), and the base's source (`merge` or `update`).
fn keep_child(merge: &MergeRecord) -> (Uuid, BaseSource, Direction) {
    match merge.direction {
        MergeDirection::IntoParent => (
            merge.source_branch_id,
            BaseSource::Merge,
            Direction::IntoParent,
        ),
        MergeDirection::FromParent => (
            merge.target_branch_id,
            BaseSource::Update,
            Direction::FromParent,
        ),
    }
}

impl<M: WorkerMetadata, S: Storage, C: ComputeBackend, N: CacheNotifier, P: SpecPusher>
    Worker<M, S, C, N, P>
{
    /// The finish (also a Resume in `finishing`). `Err` (redelivered) when the target can't be
    /// read back.
    // A plain `fn` returning a boxed future: callers hold a pointer instead of this future's
    // state (their frames overflowed a 2 MiB stack in debug builds on the reset-after-merge
    // path), and the state is built here and moved to the heap.
    pub(crate) fn merge_finish(&self, id: Uuid) -> BoxFuture<'_, Result<(), String>> {
        Box::pin(self.merge_finish_inner(id))
    }

    async fn merge_finish_inner(&self, id: Uuid) -> Result<(), String> {
        let Some(merge) = self.claim_merge(id, MergeStatus::Finishing).await? else {
            return Ok(());
        };
        let (to, patch) = self
            .keep_merge_claimed(id, MergeStatus::Finishing, self.finish_merge(&merge))
            .await?;
        let moved = self
            .metadata
            .set_merge_status(id, &[MergeStatus::Finishing], to, patch)
            .await
            .map_err(|e| format!("finishing merge {id}: {e}"))?;
        if moved {
            info!(merge_id = %id, status = to.as_str(), "merge finished");
            self.log_status(id, to).await;
        }
        Ok(())
    }

    fn finish_merge<'a>(
        &'a self,
        merge: &'a MergeRecord,
    ) -> BoxFuture<'a, Result<(MergeStatus, MergeStatusPatch), String>> {
        Box::pin(self.finish_merge_inner(merge))
    }

    async fn finish_merge_inner(
        &self,
        merge: &MergeRecord,
    ) -> Result<(MergeStatus, MergeStatusPatch), String> {
        let plan = stored_plan(merge)?;
        let target = self
            .metadata
            .get_branch(BranchId::from(merge.target_branch_id))
            .await
            .map_err(|e| format!("reading the target: {e}"))?
            .ok_or_else(|| "the target no longer exists".to_string())?;
        let endpoint = target
            .endpoint_id
            .clone()
            .ok_or_else(|| format!("the target {} has no compute", target.name))?;
        let actual = self.read_schema(&endpoint, &merge.database_name).await?;
        let expected = plan.expected.as_ref().expect("stored_plan checks it");
        // E31: until a rehearsal stored its version of the objects following a rename, their
        // differences are what Postgres produced, not drift.
        let mut produced = plan.own_sql.clone();
        if !plan.expected_rehearsed {
            produced.extend(plan.follows_rename.iter().map(|f| f.key.clone()));
        }
        let drift = compare(expected, &actual, &produced);
        let drifted = drift.iter().any(|d| !d.produced);
        let created_roles = self.created_roles(merge, &plan, target.id).await;
        // The choice is for a merge that landed as planned: with drift, the source stays as it
        // is and its base isn't advanced, so the next merge shows what's still different.
        let after = if drifted {
            json!({
                "choice": after_choice(merge),
                "skipped": "the target doesn't read back as merged",
            })
        } else {
            self.after_merge(merge, MergeStatus::Finishing, after_choice(merge), &actual)
                .await
        };
        let result = json!({
            "created_roles": created_roles,
            "drift": serde_json::to_value(&drift).map_err(|e| format!("serializing the drift: {e}"))?,
            "after": after,
        });
        let to = if drifted {
            MergeStatus::AppliedWithDrift
        } else {
            MergeStatus::Done
        };
        Ok((
            to,
            MergeStatusPatch {
                result: Some(result),
                error: Some(None),
                failed_phase: Some(None),
                ..Default::default()
            },
        ))
    }

    /// The roles phase 0 created (U3): the plan's creates the log records as `ok` (a role
    /// skipped as already in metadata wasn't created by this merge). With no log, every
    /// create.
    async fn created_roles(
        &self,
        merge: &MergeRecord,
        plan: &StoredPlan,
        target: BranchId,
    ) -> Vec<Value> {
        let done: Option<Vec<String>> = match self.metadata.merge_events(merge.id).await {
            Ok(events) => Some(
                events
                    .into_iter()
                    .filter(|e| {
                        e.kind == "step"
                            && e.phase.as_deref() == Some(phase_name(Phase::Roles))
                            && e.outcome.as_deref() == Some("ok")
                    })
                    .filter_map(|e| e.step_id)
                    .collect(),
            ),
            Err(e) => {
                warn!(merge_id = %merge.id, "reading the log for the created roles: {e}");
                None
            }
        };
        plan.steps
            .iter()
            .filter(|s| done.as_ref().is_none_or(|d| d.contains(&s.id.0)))
            .filter_map(|s| match &s.role_op {
                Some(RoleOp::Create(m)) => {
                    Some(json!({ "name": m.name, "branch_id": target.to_string() }))
                }
                _ => None,
            })
            .collect()
    }

    /// The `finish` job (E21): the after-merge choice a merge applied with drift skipped, run
    /// by hand. Only for a request `finishMerge` recorded (`result.after.requested`), once: the
    /// outcome replaces it. The merge stays `applied_with_drift`.
    pub(crate) async fn merge_finish_by_hand(
        &self,
        id: Uuid,
        after: chelabase_nats::MergeAfter,
    ) -> Result<(), String> {
        let Some(merge) = self
            .metadata
            .merge(id)
            .await
            .map_err(|e| format!("reading merge {id}: {e}"))?
        else {
            return Ok(());
        };
        let recorded = merge
            .result
            .as_ref()
            .map(|r| r["after"].clone())
            .unwrap_or_default();
        // Requested, or started by a run that crashed before its outcome (M8).
        let waiting = recorded["requested"] == json!(true) || recorded["started"] == json!(true);
        if merge.status != MergeStatus::AppliedWithDrift || !waiting {
            info!(merge_id = %id, "no after-merge choice waits; nothing to do");
            return Ok(());
        }
        let target = self
            .metadata
            .get_branch(BranchId::from(merge.target_branch_id))
            .await
            .map_err(|e| format!("reading the target: {e}"))?
            .ok_or_else(|| "the target no longer exists".to_string())?;
        let endpoint = target
            .endpoint_id
            .clone()
            .ok_or_else(|| format!("the target {} has no compute", target.name))?;
        let target_now = self.read_schema(&endpoint, &merge.database_name).await?;
        // T14 M8: the choice recorded with the request (`finishMerge` wrote it with a guard); the
        // job's own is only a fallback.
        let choice = match (merge.direction, recorded["choice"].as_str()) {
            (MergeDirection::FromParent, _) => "keep",
            (MergeDirection::IntoParent, Some(c @ ("keep" | "reset" | "delete"))) => c,
            (MergeDirection::IntoParent, _) => after.as_str(),
        };
        let outcome = self
            .after_merge(&merge, MergeStatus::AppliedWithDrift, choice, &target_now)
            .await;
        let mut result = merge.result.clone().unwrap_or_else(|| json!({}));
        result["after"] = outcome;
        let patch = MergeStatusPatch {
            result: Some(result),
            ..Default::default()
        };
        self.metadata
            .set_merge_status(
                id,
                &[MergeStatus::AppliedWithDrift],
                MergeStatus::AppliedWithDrift,
                patch,
            )
            .await
            .map_err(|e| format!("storing merge {id}'s after-merge choice: {e}"))?;
        info!(merge_id = %id, choice, "after-merge choice run by hand");
        Ok(())
    }

    /// The after-merge `choice` (into the parent only) and what it did. A reset or delete first
    /// records that it started (`result.after.started`, with the source's timeline for a
    /// reset), so a run again after a crash reports what the branch shows instead of acting
    /// twice (T14 M8). `status` is the merge's, for that write.
    fn after_merge<'a>(
        &'a self,
        merge: &'a MergeRecord,
        status: MergeStatus,
        choice: &'a str,
        target_now: &'a SchemaModel,
    ) -> BoxFuture<'a, Value> {
        Box::pin(self.after_merge_inner(merge, status, choice, target_now))
    }

    async fn after_merge_inner(
        &self,
        merge: &MergeRecord,
        status: MergeStatus,
        choice: &str,
        target_now: &SchemaModel,
    ) -> Value {
        let source = BranchId::from(merge.source_branch_id);
        let before = merge
            .result
            .as_ref()
            .map(|r| r["after"].clone())
            .filter(|a| a["started"] == json!(true) && a["choice"] == json!(choice));
        if matches!(choice, "reset" | "delete")
            && merge.direction == MergeDirection::IntoParent
            && before.is_none()
        {
            let timeline = match self.metadata.get_branch(source).await {
                Ok(b) => b.and_then(|b| b.timeline_id),
                Err(e) => return json!({ "choice": choice, "error": e.to_string() }),
            };
            let mut result = merge.result.clone().unwrap_or_else(|| json!({}));
            result["after"] = started_after(&result["after"], choice, timeline);
            let patch = MergeStatusPatch {
                result: Some(result),
                ..Default::default()
            };
            match self.metadata.set_merge_status(merge.id, &[status], status, patch).await {
                Ok(true) => {}
                Ok(false) => return json!({ "choice": choice, "error": "the merge moved on" }),
                Err(e) => return json!({ "choice": choice, "error": e.to_string() }),
            }
        }
        match (merge.direction, choice) {
            (MergeDirection::IntoParent, "reset") => {
                let started = before.as_ref().map(|b| b["timeline"].as_str().map(str::to_string));
                self.after_reset(merge.id, source, started).await
            }
            (MergeDirection::IntoParent, "delete") => self.after_delete(merge.id, source).await,
            _ => match self.after_keep(merge, target_now).await {
                Ok(base) => json!({ "choice": "keep", "base": base }),
                Err(e) => {
                    warn!(merge_id = %merge.id, "recording the merge's base: {e}");
                    json!({ "choice": "keep", "base": "not_recorded", "error": e })
                }
            },
        }
    }

    /// Keep: the source's schema becomes the child's base, if it's still what was merged
    /// (`recorded`); a source that changed since the merge was computed leaves the base as it
    /// was (`not_advanced`: its newer changes would otherwise count as merged).
    async fn after_keep(
        &self,
        merge: &MergeRecord,
        target_now: &SchemaModel,
    ) -> Result<&'static str, String> {
        let db = merge.database_name.as_str();
        let (child_id, base_source, direction) = keep_child(merge);
        let branch = |id: Uuid| async move {
            self.metadata
                .get_branch(BranchId::from(id))
                .await
                .map_err(|e| format!("reading branch {id}: {e}"))?
                .ok_or_else(|| format!("branch {id} no longer exists"))
        };
        let source = branch(merge.source_branch_id).await?;
        let child = branch(child_id).await?;
        let tenant = self
            .tenant_of(source.database_id)
            .await?
            .ok_or_else(|| "the database has no tenant".to_string())?;
        // The source's position first, then its schema: the schema read is at or after it.
        let (source_timeline, source_lsn) = self.head_of(&tenant, &source).await?;
        let source_ep = source
            .endpoint_id
            .clone()
            .ok_or_else(|| format!("the source {} has no compute", source.name))?;
        let source_now = self.read_schema(&source_ep, db).await?;
        if merge.source_fingerprint.as_deref() != Some(fingerprint(&source_now).as_str()) {
            info!(merge_id = %merge.id, "the source changed since the merge was computed; its base stays");
            return Ok("not_advanced");
        }
        // A finish run again after a crash: recorded already.
        if let Ok(Some(current)) = self.metadata.schema_base(child.id, db).await
            && matches!(current.source, BaseSource::Merge | BaseSource::Update)
            && current
                .model
                .and_then(|m| serde_json::from_value::<SchemaModel>(m).ok())
                .is_some_and(|m| fingerprint(&m) == fingerprint(&source_now))
        {
            return Ok("recorded");
        }
        let old = match self.load_base(&child, db).await {
            Ok((_, map)) => Some(map),
            Err(e) => {
                warn!(merge_id = %merge.id, "reading the old base's identity map; matching by name: {e}");
                None
            }
        };
        let map = keep_identity_map(&source_now, target_now, old.as_ref(), direction);
        let (child_timeline, child_lsn) = if child.id == source.id {
            (source_timeline.clone(), source_lsn.clone())
        } else {
            self.head_of(&tenant, &child).await?
        };
        self.record_merge_base(
            child.id,
            db,
            &source_now,
            &map,
            base_source,
            (&source_timeline, &source_lsn),
            &child_timeline,
            &child_lsn,
        )
        .await?;
        Ok("recorded")
    }

    /// A branch's current timeline and its head LSN.
    async fn head_of(&self, tenant: &NeonId, b: &BranchRecord) -> Result<(String, String), String> {
        let timeline = b
            .timeline_id
            .clone()
            .ok_or_else(|| format!("branch {} has no timeline", b.name))?;
        let id: NeonId = timeline
            .parse()
            .map_err(|e| format!("timeline {timeline:?}: {e}"))?;
        let lsn = self
            .storage
            .get_timeline(tenant, &id)
            .await
            .map_err(|e| format!("reading {}'s position: {e:#}", b.name))?;
        Ok((timeline, lsn.to_string()))
    }

    /// Reset of the source to its parent (now holding the merge), with the API's refusals: the
    /// claim (under the branch's row lock) refuses a protected branch and one another merge
    /// holds (E27; `merge`'s own hold doesn't count, T14 F3 I1), then children refuse it.
    /// `started`: an earlier run started it from that timeline (M8): a branch on another
    /// timeline was reset (`done`); one still switching is left to finish (`started`).
    fn after_reset(
        &self,
        merge: Uuid,
        id: BranchId,
        started: Option<Option<String>>,
    ) -> BoxFuture<'_, Value> {
        Box::pin(self.after_reset_inner(merge, id, started))
    }

    async fn after_reset_inner(
        &self,
        merge: Uuid,
        id: BranchId,
        started: Option<Option<String>>,
    ) -> Value {
        let refused = |why: String| json!({ "choice": "reset", "refused": why });
        let failed = |why: String| json!({ "choice": "reset", "error": why });
        let branch = match self.metadata.get_branch(id).await {
            Ok(Some(b)) => b,
            Ok(None) => return refused("the branch no longer exists".into()),
            Err(e) => return failed(e.to_string()),
        };
        if let Some(from) = started {
            if branch.timeline_id != from {
                return json!({ "choice": "reset", "outcome": "done" });
            }
            if branch.status == BranchStatus::Resetting {
                // Its switch carries on (reconcile finishes it).
                return json!({ "choice": "reset", "outcome": "started" });
            }
        }
        match self
            .metadata
            .claim_branch_for_switch(id, false, Some(merge))
            .await
        {
            Ok(ClaimOutcome::Claimed) => {}
            Ok(ClaimOutcome::Protected) => {
                return refused(format!("{} is protected; unprotect it first", branch.name));
            }
            Ok(ClaimOutcome::MergeBusy(busy)) => {
                return refused(merge_busy_refusal(&branch.name, busy));
            }
            Ok(ClaimOutcome::NotReady) => return refused(format!("{} isn't ready", branch.name)),
            Err(e) => return failed(e.to_string()),
        }
        // Held now (`resetting`): no child can be made under it meanwhile.
        let children = match self.metadata.live_children(id).await {
            Ok(children) => children,
            Err(e) => {
                self.release_claim(id).await;
                return failed(e.to_string());
            }
        };
        if !children.is_empty() {
            self.release_claim(id).await;
            let names: Vec<String> = children.into_iter().map(|c| c.name).collect();
            return refused(format!(
                "{} has child branches ({}); delete them first",
                branch.name,
                names.join(", ")
            ));
        }
        // Boxed: the switch's future is large, and unboxed it sits inline in the merge job's
        // future, which overflowed a 2 MiB stack in debug builds.
        if let Err(e) = self.branch_reset_attempt(id, Attempt::LAST).await {
            return failed(e);
        }
        match self.metadata.get_branch(id).await {
            Ok(Some(after)) if after.timeline_id != branch.timeline_id => {
                json!({ "choice": "reset", "outcome": "done" })
            }
            Ok(after) => failed(
                after
                    .and_then(|b| b.error)
                    .unwrap_or_else(|| "the reset didn't happen".to_string()),
            ),
            Err(e) => failed(e.to_string()),
        }
    }

    /// A claimed branch back to ready (best effort: reconcile finds a branch left resetting).
    async fn release_claim(&self, id: BranchId) {
        if let Err(e) = self
            .metadata
            .set_branch_status(id, &[BranchStatus::Resetting], BranchStatus::Ready)
            .await
        {
            warn!(branch_id = %id, "putting a claimed branch back to ready: {e}");
        }
    }

    /// Delete of the source, with the delete's refusals (children, protected, another merge
    /// holding it: E27; `merge`'s own hold doesn't count, T14 F3 I1) recorded.
    async fn after_delete(&self, merge: Uuid, id: BranchId) -> Value {
        let branch = match self.metadata.get_branch(id).await {
            Ok(Some(b)) => b,
            Ok(None) => return json!({ "choice": "delete", "outcome": "done" }),
            Err(e) => return json!({ "choice": "delete", "error": e.to_string() }),
        };
        match self.metadata.begin_branch_delete(id, Some(merge)).await {
            Ok(outcome) => {
                if let Some(why) = outcome.refusal(&branch.name) {
                    return json!({ "choice": "delete", "refused": why });
                }
            }
            Err(e) => return json!({ "choice": "delete", "error": e.to_string() }),
        }
        // T14 M12: its other merges nothing runs go with it (a draft from its parent is another
        // pair), as the API's delete does.
        match self.metadata.cancel_drafts_for_branch(id).await {
            Ok(0) => {}
            Ok(n) => info!(branch_id = %id, cancelled = n, "cancelled merges of a deleted branch"),
            Err(e) => warn!(branch_id = %id, "cancelling merges of a deleted branch: {e}"),
        }
        match self.handle_branch_delete(id).await {
            Ok(()) => json!({ "choice": "delete", "outcome": "done" }),
            // The branch stays `deleting`: reconcile retries the delete.
            Err(e) => json!({ "choice": "delete", "error": e }),
        }
    }
}

#[cfg(test)]
mod tests {
    use chelabase_metadata::{BaseSource, BranchStatus, MergeDirection, MergeStatus};
    use chelabase_nats::MergeJob;
    use chelabase_neon::{Lsn, NeonId};
    use chelabase_schema::catalog::parse_catalog;
    use chelabase_schema::{IdentityMap, ObjAt, ObjClass, Oid, fingerprint};
    use serde_json::json;
    use uuid::Uuid;

    use crate::branch_job::Attempt;
    use crate::fakes::{FakeWorker, ready_branch, worker};
    use crate::merge::testkit::{
        DIGEST, DIGEST_READ, Scenario, Setup, acknowledge_all, answer_catalog,
        answer_catalog_with_roles, answer_elsewhere, applying, before_apply, catalog_with_roles,
        items, move_to, orders, setup,
    };

    async fn apply(w: &FakeWorker, id: Uuid) {
        w.handle_merge_job(MergeJob::Apply { merge_id: id }, Attempt(1))
            .await
            .unwrap();
    }

    fn set_after(w: &FakeWorker, id: Uuid, after: &str) {
        w.metadata.state().merges.get_mut(&id).unwrap().options =
            json!({ "lock_timeout_ms": 5000, "after": after });
    }

    /// The newest base entry of `branch`.
    fn newest_base(
        w: &FakeWorker,
        branch: chelabase_types::BranchId,
    ) -> chelabase_metadata::SchemaBase {
        w.metadata
            .state()
            .schema_bases
            .iter()
            .rev()
            .find(|b| b.branch_id == branch && b.database_name == "app")
            .cloned()
            .expect("a base")
    }

    /// Moves `branch`'s timeline head to `lsn`, as writes do.
    fn advance(w: &FakeWorker, s: &Setup, branch: chelabase_types::BranchId, lsn: Lsn) -> String {
        let timeline = w.metadata.branch(branch).timeline_id.unwrap();
        let tenant: NeonId = w
            .metadata
            .database(s.db)
            .neon_tenant_id
            .unwrap()
            .parse()
            .unwrap();
        w.storage
            .timelines
            .lock()
            .unwrap()
            .get_mut(&(tenant, timeline.parse().unwrap()))
            .unwrap()
            .head = lsn;
        timeline
    }

    fn pair(
        map: &IdentityMap,
        class: ObjClass,
        oid: u32,
        sub: Option<i16>,
    ) -> (Option<ObjAt>, Option<ObjAt>) {
        let p = map
            .pairs()
            .iter()
            .find(|p| p.class == class && p.base == ObjAt::new(Oid(oid), sub))
            .unwrap_or_else(|| panic!("no pair for {class:?} {oid}: {:?}", map.pairs()));
        (p.parent, p.branch)
    }

    fn at(oid: u32, sub: Option<i16>) -> Option<ObjAt> {
        Some(ObjAt::new(Oid(oid), sub))
    }

    // E6: an object the old base knew is followed through it, so a rename the target kept
    // (keep target) stays one object; what the merge created is matched by name; what the
    // target doesn't have is `None`.
    #[test]
    fn identity_map_follows_the_old_base_for_a_kept_rename() {
        use crate::merge::testkit::{catalog, table};
        // The branch renamed `orders` (16400) to `purchases`; main kept `orders`, which is its
        // 17000 (the old base pairs them).
        let source = parse_catalog(&catalog(&[
            table(16400, "purchases", &[(1, "id", "bigint", 20)]),
            items(16500),
            table(16510, "scratch", &[(1, "id", "bigint", 20)]),
        ]))
        .unwrap();
        let target = parse_catalog(&catalog(&[
            table(17000, "orders", &[(1, "id", "bigint", 20)]),
            items(17100),
        ]))
        .unwrap();
        let old = IdentityMap::from_pairs([chelabase_schema::IdentityPair {
            class: ObjClass::Table,
            base: ObjAt::new(Oid(16400), None),
            parent: at(17000, None),
            branch: at(16400, None),
        }]);

        let map = super::keep_identity_map(
            &source,
            &target,
            Some(&old),
            chelabase_schema::Direction::IntoParent,
        );

        assert_eq!(
            pair(&map, ObjClass::Table, 16400, None),
            (at(17000, None), at(16400, None))
        );
        assert_eq!(
            pair(&map, ObjClass::Table, 16500, None),
            (at(17100, None), at(16500, None))
        );
        assert_eq!(
            pair(&map, ObjClass::Table, 16510, None),
            (None, at(16510, None))
        );
        assert_eq!(
            pair(&map, ObjClass::Column, 16400, Some(1)),
            (at(17000, Some(1)), at(16400, Some(1)))
        );
    }

    // Part 4: a target that doesn't read back as expected ends `applied_with_drift`, with the
    // difference in the result.
    #[tokio::test]
    async fn drift_marks_applied_with_drift() {
        let (w, _journal) = worker();
        let (s, id) = applying(&w).await;
        answer_catalog_with_roles(
            &w,
            s.main_ep(),
            &[Scenario::merged_tables()[0].clone()],
            &Scenario::merged_roles(),
        );

        apply(&w, id).await;

        let m = w.metadata.merge_row(id);
        assert_eq!(m.status, MergeStatus::AppliedWithDrift, "{:?}", m.error);
        let result = m.result.unwrap();
        let drift = result["drift"].as_array().unwrap();
        assert_eq!(drift.len(), 1, "{drift:?}");
        assert_eq!(drift[0]["key"]["name"], json!("items"));
        assert_eq!(drift[0]["kind"], json!("missing"));
        // The after-merge choice waits for a target that reads back as merged (docs): the base
        // isn't advanced, nothing is reset or deleted.
        assert_eq!(result["after"]["choice"], json!("keep"));
        assert!(result["after"]["skipped"].is_string(), "{result}");
        assert_ne!(newest_base(&w, s.dev.id).source, BaseSource::Merge);
    }

    // D11, E6, E18: keep appends the source's schema as dev's base, at the source's position,
    // with a full identity map: each source object paired with the target's (main's OIDs for
    // what the merge created).
    #[tokio::test]
    async fn keep_writes_new_base_and_identity_map() {
        let (w, _journal) = worker();
        let (s, id) = applying(&w).await;
        let timeline = advance(&w, &s, s.dev.id, Lsn(0x0300_0000));

        apply(&w, id).await;

        let m = w.metadata.merge_row(id);
        assert_eq!(m.status, MergeStatus::Done, "{:?}", m.error);
        assert_eq!(
            m.result.unwrap()["after"],
            json!({ "choice": "keep", "base": "recorded" })
        );
        let base = newest_base(&w, s.dev.id);
        assert_eq!(base.source, BaseSource::Merge);
        assert_eq!(base.timeline_id.as_deref(), Some(timeline.as_str()));
        assert_eq!(base.lsn.as_deref(), Some("0/3000000"));
        assert_eq!(base.branch_timeline_id.as_deref(), Some(timeline.as_str()));
        assert_eq!(base.branch_lsn.as_deref(), Some("0/3000000"));
        let dev_model = parse_catalog(&catalog_with_roles(
            &Scenario::dev_tables(),
            &Scenario::dev_roles(),
        ))
        .unwrap();
        let stored: chelabase_schema::SchemaModel =
            serde_json::from_value(base.model.clone().unwrap()).unwrap();
        assert_eq!(fingerprint(&stored), fingerprint(&dev_model));

        // Into the parent: the branch side is dev's own OIDs, the parent side main's.
        let map: IdentityMap = serde_json::from_value(base.identity_map).unwrap();
        assert_eq!(
            pair(&map, ObjClass::Table, 16400, None),
            (at(16400, None), at(16400, None))
        );
        assert_eq!(
            pair(&map, ObjClass::Table, 16500, None),
            (at(16600, None), at(16500, None))
        );
        assert_eq!(
            pair(&map, ObjClass::Column, 16500, Some(1)),
            (at(16600, Some(1)), at(16500, Some(1)))
        );
        assert_eq!(
            pair(&map, ObjClass::Index, 16450, None),
            (at(16700, None), at(16450, None))
        );
        assert_eq!(
            pair(&map, ObjClass::Role, 16460, None),
            (at(16800, None), at(16460, None))
        );
        assert_eq!(
            pair(&map, ObjClass::Schema, 2200, None),
            (at(2200, None), at(2200, None))
        );
    }

    // The source changed after the merge was computed: its schema now isn't what was merged,
    // so the base isn't advanced (the next merge sees the merged changes on both sides).
    #[tokio::test]
    async fn keep_without_the_merged_source_leaves_the_base() {
        let (w, _journal) = worker();
        let (s, id) = applying(&w).await;
        let before = newest_base(&w, s.dev.id);
        let mut changed = Scenario::dev_tables();
        changed.push(crate::merge::testkit::table(
            16900,
            "later",
            &[(1, "id", "bigint", 20)],
        ));
        answer_catalog_with_roles(&w, s.dev_ep(), &changed, &Scenario::dev_roles());

        apply(&w, id).await;

        let m = w.metadata.merge_row(id);
        assert_eq!(m.status, MergeStatus::Done);
        let after = &m.result.unwrap()["after"];
        assert_eq!(after["choice"], json!("keep"));
        assert_eq!(after["base"], json!("not_advanced"));
        assert_eq!(newest_base(&w, s.dev.id).id, before.id);
    }

    #[tokio::test]
    async fn reset_after_merge_runs_reset() {
        let (w, _journal) = worker();
        let (s, id) = applying(&w).await;
        set_after(&w, id, "reset");
        let old_timeline = w.metadata.branch(s.dev.id).timeline_id;

        apply(&w, id).await;

        let m = w.metadata.merge_row(id);
        assert_eq!(m.status, MergeStatus::Done, "{:?}", m.error);
        assert_eq!(
            m.result.unwrap()["after"],
            json!({ "choice": "reset", "outcome": "done" })
        );
        let dev = w.metadata.branch(s.dev.id);
        assert_eq!(dev.status, BranchStatus::Ready, "{:?}", dev.error);
        assert_ne!(dev.timeline_id, old_timeline);
        assert_eq!(newest_base(&w, s.dev.id).source, BaseSource::Reset);
    }

    // T15: the reset after a merge runs on a small stack. The reset is nested inside the merge's
    // job future, and an unboxed chain of large async futures overflowed the 2 MiB test thread
    // of the live suite in debug builds (the callers' frames held every callee's future). This
    // drives the same path (fakes) on a thread with a fraction of that, so a future that grows back inline fails here first.
    #[test]
    fn reset_after_merge_fits_a_small_stack() {
        // The unboxed chain needed over 600 KiB here (fakes; the real worker's is bigger).
        const STACK: usize = 448 * 1024;
        std::thread::Builder::new()
            .name("small-stack".into())
            .stack_size(STACK)
            .spawn(|| {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap()
                    .block_on(async {
                        let (w, _journal) = worker();
                        let (s, id) = applying(&w).await;
                        set_after(&w, id, "reset");
                        apply(&w, id).await;
                        let m = w.metadata.merge_row(id);
                        assert_eq!(m.status, MergeStatus::Done, "{:?}", m.error);
                        assert_eq!(
                            m.result.unwrap()["after"],
                            json!({ "choice": "reset", "outcome": "done" })
                        );
                        assert_eq!(w.metadata.branch(s.dev.id).status, BranchStatus::Ready);
                    });
            })
            .unwrap()
            .join()
            .unwrap();
    }

    // D11: the delete's usual refusals apply; one is recorded and the merge is still done.
    #[tokio::test]
    async fn delete_after_merge_refused_by_children_is_recorded() {
        let (w, _journal) = worker();
        let (s, id) = applying(&w).await;
        set_after(&w, id, "delete");
        ready_branch(&w, s.db, "child", s.dev.id).await;

        apply(&w, id).await;

        let m = w.metadata.merge_row(id);
        assert_eq!(m.status, MergeStatus::Done, "{:?}", m.error);
        let after = m.result.unwrap()["after"].clone();
        assert_eq!(after["choice"], json!("delete"));
        let refused = after["refused"].as_str().unwrap();
        assert!(refused.contains("child branches"), "{refused}");
        assert_eq!(w.metadata.branch(s.dev.id).status, BranchStatus::Ready);
    }

    #[tokio::test]
    async fn delete_after_merge_deletes_the_branch() {
        let (w, _journal) = worker();
        let (s, id) = applying(&w).await;
        set_after(&w, id, "delete");

        apply(&w, id).await;

        let m = w.metadata.merge_row(id);
        assert_eq!(m.status, MergeStatus::Done, "{:?}", m.error);
        assert_eq!(
            m.result.unwrap()["after"],
            json!({ "choice": "delete", "outcome": "done" })
        );
        assert_eq!(w.metadata.branch(s.dev.id).status, BranchStatus::Deleted);
    }

    // Recovery in `finishing`: finish runs again, and records the base once.
    #[tokio::test]
    async fn finishing_is_redone_after_a_crash() {
        let (w, _journal) = worker();
        let (s, id) = applying(&w).await;
        apply(&w, id).await;
        let bases = w.metadata.state().schema_bases.len();
        move_to(&w, id, MergeStatus::Finishing);

        w.handle_merge_job(MergeJob::Resume { merge_id: id }, Attempt(1))
            .await
            .unwrap();

        assert_eq!(w.metadata.merge_row(id).status, MergeStatus::Done);
        assert_eq!(
            w.metadata.state().schema_bases.len(),
            bases,
            "no second entry"
        );
        assert_eq!(newest_base(&w, s.dev.id).source, BaseSource::Merge);
    }

    // From the parent: the branch is the target and keeps the parent's schema as its base
    // (`update`), located on the parent, valid from the branch's own position.
    #[tokio::test]
    async fn update_from_parent_keeps_an_update_base() {
        let (w, _journal) = worker();
        let s = setup(&w).await;
        answer_catalog(&w, s.main_ep(), &[orders(), items(16500)]);
        answer_catalog(&w, s.dev_ep(), &[orders()]);
        w.computes
            .answer_at(s.dev_ep(), DIGEST_READ, &format!("{DIGEST}\n"));
        let id = w.metadata.insert_merge(
            s.db,
            s.main.id,
            s.dev.id,
            MergeDirection::FromParent,
            MergeStatus::Computing,
        );
        w.handle_merge_job(MergeJob::Compute { merge_id: id }, Attempt(1))
            .await
            .unwrap();
        assert!(w.metadata.merge_row(id).plan.is_some());
        answer_elsewhere(&w, &[orders(), items(16600)], &[]);
        move_to(&w, id, MergeStatus::Rehearsing);
        w.handle_merge_job(MergeJob::Rehearse { merge_id: id }, Attempt(1))
            .await
            .unwrap();
        assert_eq!(w.metadata.merge_row(id).status, MergeStatus::Rehearsed);
        acknowledge_all(&w, id);
        answer_catalog(&w, s.dev_ep(), &[orders(), items(16700)]);
        let main_timeline = advance(&w, &s, s.main.id, Lsn(0x0400_0000));
        let dev_timeline = advance(&w, &s, s.dev.id, Lsn(0x0300_0000));
        move_to(&w, id, MergeStatus::Applying);
        before_apply(&w, s.dev_ep(), &[orders()], &[]);

        apply(&w, id).await;

        let m = w.metadata.merge_row(id);
        assert_eq!(m.status, MergeStatus::Done, "{:?}", m.error);
        let base = newest_base(&w, s.dev.id);
        assert_eq!(base.source, BaseSource::Update);
        assert_eq!(
            (base.timeline_id.as_deref(), base.lsn.as_deref()),
            (Some(main_timeline.as_str()), Some("0/4000000"))
        );
        assert_eq!(
            (
                base.branch_timeline_id.as_deref(),
                base.branch_lsn.as_deref()
            ),
            (Some(dev_timeline.as_str()), Some("0/3000000"))
        );
        let map: IdentityMap = serde_json::from_value(base.identity_map).unwrap();
        // The parent is the source: main's OIDs on the parent side, dev's on the branch side.
        assert_eq!(
            pair(&map, ObjClass::Table, 16500, None),
            (at(16500, None), at(16700, None))
        );
    }

    /// The scenario applied with drift (main lacks `items`), then `finishMerge` with `after`:
    /// the API records the request in `result.after` and publishes the job.
    async fn drifted_then_requested(
        w: &FakeWorker,
        after: chelabase_nats::MergeAfter,
    ) -> (Setup, Uuid) {
        let (s, id) = applying(w).await;
        answer_catalog_with_roles(
            w,
            s.main_ep(),
            &[Scenario::merged_tables()[0].clone()],
            &Scenario::merged_roles(),
        );
        apply(w, id).await;
        assert_eq!(w.metadata.merge_row(id).status, MergeStatus::AppliedWithDrift);
        {
            let mut state = w.metadata.state();
            let m = state.merges.get_mut(&id).unwrap();
            m.result.as_mut().unwrap()["after"] =
                json!({ "choice": after.as_str(), "requested": true });
        }
        w.handle_merge_job(MergeJob::Finish { merge_id: id, after }, Attempt(1))
            .await
            .unwrap();
        (s, id)
    }

    // E21: with drift the choice waits; run by hand, it does what it would have done, and the
    // merge stays `applied_with_drift` with its drift.
    #[tokio::test]
    async fn finish_by_hand_runs_the_choice_after_drift() {
        let (w, _journal) = worker();
        let (s, id) = drifted_then_requested(&w, chelabase_nats::MergeAfter::Reset).await;

        let m = w.metadata.merge_row(id);
        assert_eq!(m.status, MergeStatus::AppliedWithDrift);
        let result = m.result.unwrap();
        assert_eq!(result["after"], json!({ "choice": "reset", "outcome": "done" }));
        assert_eq!(result["drift"].as_array().unwrap().len(), 1, "the drift stays");
        assert!(!result["created_roles"].as_array().unwrap().is_empty());
        assert_eq!(newest_base(&w, s.dev.id).source, BaseSource::Reset);
    }

    // E21: keep by hand advances the base under the usual rule (the source is what was merged).
    #[tokio::test]
    async fn finish_by_hand_keep_advances_the_base() {
        let (w, _journal) = worker();
        let (s, id) = drifted_then_requested(&w, chelabase_nats::MergeAfter::Keep).await;

        let m = w.metadata.merge_row(id);
        assert_eq!(
            m.result.unwrap()["after"],
            json!({ "choice": "keep", "base": "recorded" })
        );
        assert_eq!(newest_base(&w, s.dev.id).source, BaseSource::Merge);
    }

    // T14 M8: the choice run by hand is the one recorded with the request, not the job's.
    #[tokio::test]
    async fn finish_by_hand_runs_the_recorded_choice() {
        let (w, _journal) = worker();
        let (s, id) = applying(&w).await;
        answer_catalog_with_roles(&w, s.main_ep(), &[Scenario::merged_tables()[0].clone()], &Scenario::merged_roles());
        apply(&w, id).await;
        w.metadata.state().merges.get_mut(&id).unwrap().result.as_mut().unwrap()["after"] =
            json!({ "choice": "delete", "requested": true });
        w.handle_merge_job(MergeJob::Finish { merge_id: id, after: chelabase_nats::MergeAfter::Reset }, Attempt(1))
            .await
            .unwrap();
        assert_eq!(w.metadata.merge_row(id).result.unwrap()["after"], json!({ "choice": "delete", "outcome": "done" }));
        assert_eq!(w.metadata.branch(s.dev.id).status, BranchStatus::Deleted);
    }

    // T14 M8: a finish run again after its reset happened (a crash before the outcome was
    // stored) reports it done; it doesn't reset the branch again.
    #[tokio::test]
    async fn a_finish_run_again_after_its_reset_reports_it_done() {
        let (w, journal) = worker();
        let (s, id) = applying(&w).await;
        set_after(&w, id, "reset");
        let before = w.metadata.branch(s.dev.id).timeline_id.unwrap();
        apply(&w, id).await;
        let after = w.metadata.branch(s.dev.id).timeline_id.unwrap();
        assert_ne!(before, after);
        {
            let mut state = w.metadata.state();
            let m = state.merges.get_mut(&id).unwrap();
            m.result.as_mut().unwrap()["after"] = json!({ "choice": "reset", "started": true, "timeline": before });
        }
        move_to(&w, id, MergeStatus::Finishing);
        journal.lock().unwrap().clear();

        w.handle_merge_job(MergeJob::Resume { merge_id: id }, Attempt(1)).await.unwrap();

        let m = w.metadata.merge_row(id);
        assert_eq!(m.result.unwrap()["after"], json!({ "choice": "reset", "outcome": "done" }));
        assert_eq!(w.metadata.branch(s.dev.id).timeline_id.unwrap(), after, "not reset again");
        assert!(!crate::fakes::entries(&journal).iter().any(|e| e.contains("Resetting") || e.starts_with("storage.create_branch_timeline")));
    }

    // T14 M8: the same for a delete: run again, it reports what the branch shows.
    #[tokio::test]
    async fn a_finish_run_again_after_its_delete_reports_it_done() {
        let (w, _journal) = worker();
        let (s, id) = applying(&w).await;
        set_after(&w, id, "delete");
        apply(&w, id).await;
        {
            let mut state = w.metadata.state();
            let m = state.merges.get_mut(&id).unwrap();
            m.result.as_mut().unwrap()["after"] = json!({ "choice": "delete", "started": true });
        }
        move_to(&w, id, MergeStatus::Finishing);
        w.handle_merge_job(MergeJob::Resume { merge_id: id }, Attempt(1)).await.unwrap();
        let m = w.metadata.merge_row(id);
        assert_eq!(m.result.unwrap()["after"], json!({ "choice": "delete", "outcome": "done" }));
        let _ = s;
    }

    // T14 M12: deleting the source after the merge cancels its other merges nothing runs (a
    // draft from its parent is another pair).
    #[tokio::test]
    async fn the_after_merge_delete_cancels_the_source_s_drafts() {
        let (w, _journal) = worker();
        let (s, id) = applying(&w).await;
        set_after(&w, id, "delete");
        let draft = w.metadata.insert_merge(s.db, s.main.id, s.dev.id, MergeDirection::FromParent, MergeStatus::Draft);
        apply(&w, id).await;
        assert_eq!(w.metadata.branch(s.dev.id).status, BranchStatus::Deleted);
        assert_eq!(w.metadata.merge_row(draft).status, MergeStatus::Cancelled);
        assert_eq!(w.metadata.merge_row(id).status, MergeStatus::Done);
    }

    // T14 F3 fix round (M1): the `started` record of a reset or delete keeps a `finishMerge`
    // request's flag (reconcile finds a lost job by it); the finish's own run has none.
    #[test]
    fn a_started_choice_keeps_its_request() {
        let requested = json!({ "choice": "reset", "requested": true });
        assert_eq!(
            super::started_after(&requested, "reset", Some("t1".into())),
            json!({ "choice": "reset", "started": true, "timeline": "t1", "requested": true })
        );
        assert_eq!(
            super::started_after(&serde_json::Value::Null, "delete", None),
            json!({ "choice": "delete", "started": true, "timeline": null })
        );
    }

    // T14 F3 fix round (I1, the hold invariant on every path): the after-merge reset and delete
    // of the source are refused while another merge holds it, as source or as target, in exactly
    // the states that hold (E27); the source keeps its timeline. The finishing merge's own hold
    // (it is `finishing`) never counts.
    #[tokio::test]
    async fn the_after_merge_reset_and_delete_follow_the_hold_table() {
        use chelabase_metadata::MergeState;
        for state in MergeState::all() {
            for choice in ["reset", "delete"] {
                for dev_is_source in [true, false] {
                    // Another merge into main can't be applying while this one finishes into
                    // it (`merges_one_applying`); with dev as the target every state can.
                    if dev_is_source && state.is_applying() {
                        continue;
                    }
                    let (w, _journal) = worker();
                    let (s, id) = applying(&w).await;
                    // Applied (keeping dev), then finishing again with `choice`: the other merge
                    // below joins only now (one applying into main would refuse this one's
                    // apply, `merges_one_applying`).
                    apply(&w, id).await;
                    set_after(&w, id, choice);
                    move_to(&w, id, MergeStatus::Finishing);
                    let (source, target, direction) = if dev_is_source {
                        (s.dev.id, s.main.id, MergeDirection::IntoParent)
                    } else {
                        (s.main.id, s.dev.id, MergeDirection::FromParent)
                    };
                    // Another database's merge of the pair, or the parent's update of dev.
                    let other = w.metadata.insert_merge(s.db, source, target, direction, state.status);
                    {
                        let mut st = w.metadata.state();
                        let row = st.merges.get_mut(&other).unwrap();
                        row.database_name = "other".into();
                        row.failed_phase = state.failed_phase.map(str::to_string);
                    }
                    let before = w.metadata.branch(s.dev.id).timeline_id;

                    w.handle_merge_job(MergeJob::Resume { merge_id: id }, Attempt(1))
                        .await
                        .unwrap();

                    let m = w.metadata.merge_row(id);
                    let after = m.result.clone().unwrap()["after"].clone();
                    let dev = w.metadata.branch(s.dev.id);
                    let ctx = format!("{state:?} {choice} dev_is_source={dev_is_source}: {after}");
                    assert_eq!(m.status, MergeStatus::Done, "{ctx}");
                    if state.holds_branches() {
                        let refused = after["refused"].as_str().unwrap_or_default();
                        assert!(refused.contains(&other.to_string()), "{ctx}");
                        assert_eq!((dev.status, dev.timeline_id), (BranchStatus::Ready, before), "{ctx}");
                    } else {
                        assert_eq!(after["outcome"], json!("done"), "{ctx}");
                    }
                }
            }
        }
    }

    // E31: until a rehearsal stored its version of an object following a rename, the finish
    // counts its differences as produced, not drift.
    #[tokio::test]
    async fn objects_following_a_rename_are_produced_until_rehearsed() {
        let (w, _journal) = worker();
        let (s, id) = applying(&w).await;
        answer_catalog_with_roles(&w, s.main_ep(), &[Scenario::merged_tables()[0].clone()], &Scenario::merged_roles());
        w.metadata.state().merges.get_mut(&id).unwrap().plan.as_mut().unwrap()["follows_rename"] = json!([{
            "key": { "kind": "table", "schema": "public", "table": null, "name": "items" },
            "renames": []
        }]);
        apply(&w, id).await;
        let m = w.metadata.merge_row(id);
        assert_eq!(m.status, MergeStatus::Done, "{:?}", m.result);
        assert_eq!(m.result.unwrap()["drift"][0]["produced"], json!(true));
    }

    // E21: only a pending request runs, once; a second job (or one for a merge without drift)
    // does nothing.
    #[tokio::test]
    async fn a_finish_job_runs_only_a_pending_request() {
        let (w, journal) = worker();
        let (s, id) = drifted_then_requested(&w, chelabase_nats::MergeAfter::Delete).await;
        assert_eq!(w.metadata.branch(s.dev.id).status, BranchStatus::Deleted);
        let after = w.metadata.merge_row(id).result.unwrap()["after"].clone();
        assert_eq!(after, json!({ "choice": "delete", "outcome": "done" }));
        journal.lock().unwrap().clear();

        w.handle_merge_job(
            MergeJob::Finish {
                merge_id: id,
                after: chelabase_nats::MergeAfter::Delete,
            },
            Attempt(2),
        )
        .await
        .unwrap();

        assert_eq!(w.metadata.merge_row(id).result.unwrap()["after"], after);
        assert!(
            !crate::fakes::entries(&journal)
                .iter()
                .any(|e| e.starts_with("metadata.set_merge_status")),
            "nothing written"
        );
    }
}
