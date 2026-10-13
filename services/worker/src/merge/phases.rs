//! The phase executor (E3): runs a merge's plan on a target, as a rehearsal or for real.
//!
//! - **Phase 0** (roles): for real through the control plane (`create_role` / `update_role`,
//!   P5), a role already in metadata skipped; in a rehearsal by SQL only, NOLOGIN, with no
//!   password and no metadata (T9).
//! - **Phase 1a**: each enum value in its own committed statement.
//! - **Phase 1**: [`marker_setup_sql`] on `cloud_admin` (idempotent), then [`phase1_script`]
//!   in one transaction: its guard raises [`STALE_ERROR`] when the target's digest moved.
//! - **Phase 2**: one statement per step; each step's [`OnlineCheck`] runs first (done:
//!   skipped; an INVALID index: dropped concurrently, then rebuilt). For real, a step the log
//!   already records as done is skipped without a check, so a retry continues after it.
//!
//! Every script of phases 1a, 1 and 2 (user steps and own SQL included) runs in the owner's own
//! session (E15: psql `-U postgres`), named `chelabase-merge-<id>` (P3); catalog checks and the
//! marker setup stay on `cloud_admin`. A call that outlasts its admin timeout has its backend
//! cancelled by that name. For real, every step's outcome goes to the merge's log.

use std::collections::HashSet;
use std::future::Future;
use std::time::Instant;

use chelabase_metadata::{BranchKind, MergeEvent, RoleCopy, RoleOrigin};
use chelabase_neon::{ComputeBackend, SqlOpts};
use chelabase_schema::plan::{
    COMMIT_STEP_ID, MARKER_STEP_ID, OWNER_ROLE as SCRIPT_OWNER, OnlineCheck, Phase, Plan,
    PlanOptions, RoleOp, RolePatchModel, RoleSpecModel, STALE_ERROR, ScriptPart, Step, StepId,
    marker_setup_sql, phase1, step_script,
};
use chelabase_schema::sql::{literal, qualified};
use chelabase_types::BranchId;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tracing::warn;
use uuid::Uuid;

use crate::ports::{CacheNotifier, SpecPusher, Storage, WorkerMetadata};
use crate::resync::{WantedRole, create_role_sql, parse_observed, role_read_sql};
use crate::roles::{
    RoleOutcome, RolePatch, RoleSpec, check_patch, check_rename, check_spec, current_role_state,
    exists_sql, name_taken, parse_role_names, rename_is_ours, role_names_sql, role_reply,
    update_statements,
};
use crate::{OWNER_ROLE, Worker};

/// Phase 1's entry in a report and the log: the whole transaction is one step (a user step
/// can't take the id, T14 F3 M8).
pub(crate) const PHASE1_STEP: &str = chelabase_schema::plan::PHASE1_STEP_ID;

/// How [`Worker::run_phases`] runs a plan.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RunMode {
    /// On a throwaway branch of the target: roles by SQL only, nothing logged.
    Rehearsal,
    /// On the target itself: roles through the control plane, every step logged.
    Real,
}

/// Where a plan runs: the branch (for the roles path), its endpoint and the Postgres database.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Target {
    pub branch: BranchId,
    pub endpoint: String,
    pub database: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum StepOutcome {
    Ok,
    /// Already done (a role in metadata, a valid index, a step the log records).
    Skipped,
}

/// One step that ran (phase 1: the whole transaction, [`PHASE1_STEP`]).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct StepRun {
    pub id: String,
    pub phase: Phase,
    pub outcome: StepOutcome,
    pub duration_ms: u64,
}

/// The steps that ran, in order: the rehearsal's timings (P15: no lock waits).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PhaseReport {
    pub steps: Vec<StepRun>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FailureKind {
    /// Phase 1's guard: the target changed since the plan was made; rolled back.
    Stale,
    /// A step failed (Postgres's error, or a refusal of the roles path).
    Failed,
    /// A call outlasted its admin timeout; its backend was cancelled.
    TimedOut,
}

/// Where a run stopped. Phase 1 failures changed nothing (one transaction); in phase 2 the
/// steps of `report` are done and a retry continues after them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PhaseFailure {
    pub kind: FailureKind,
    pub phase: Phase,
    /// The failing step, when known (phase 1: from psql's line number; `None` for the guard
    /// and the marker setup).
    pub step: Option<StepId>,
    pub error: String,
    /// The steps that ran before it.
    pub report: PhaseReport,
}

/// A step's failure before it's placed in its phase.
struct Fail {
    kind: FailureKind,
    step: Option<StepId>,
    error: String,
}

impl Fail {
    fn failed(error: String) -> Self {
        Self {
            kind: FailureKind::Failed,
            step: None,
            error,
        }
    }

    fn into_failure(
        self,
        phase: Phase,
        step: Option<&StepId>,
        report: PhaseReport,
    ) -> PhaseFailure {
        PhaseFailure {
            kind: self.kind,
            phase,
            step: self.step.or_else(|| step.cloned()),
            error: self.error,
            report,
        }
    }
}

/// A phase's name in the log and in `failed_phase` (the plan's serde names).
pub(crate) fn phase_name(phase: Phase) -> &'static str {
    match phase {
        Phase::Roles => "roles",
        Phase::EnumAdd => "enum_add",
        Phase::Tx => "tx",
        Phase::Online => "online",
    }
}

/// The merge's psql sessions' `application_name` (P3): 52 characters, under Postgres's 63.
pub(crate) fn app_name(merge_id: Uuid) -> String {
    format!("chelabase-merge-{merge_id}")
}

/// Cancels the merge's running statements (P3), as `cloud_admin`.
pub(crate) fn cancel_sql(merge_id: Uuid) -> String {
    format!(
        "SET search_path = pg_catalog, pg_temp;\nSELECT pg_cancel_backend(pid) FROM pg_stat_activity WHERE application_name = {};\n",
        literal(&app_name(merge_id))
    )
}

/// Where the current apply attempt starts in a merge's log: just after its last `status` →
/// `applying` line (the API logs one with each apply and retry, T14 I2); 0 when there's none.
pub(crate) fn attempt_start(events: &[MergeEvent]) -> usize {
    events
        .iter()
        .rposition(|e| {
            e.kind == "status" && e.detail.as_ref().is_some_and(|d| d["to"] == "applying")
        })
        .map_or(0, |i| i + 1)
}

/// What a phase 2 step's check found.
#[derive(Debug, PartialEq, Eq)]
enum CheckState {
    Done,
    /// The index exists on its table but is INVALID (an interrupted concurrent build).
    Invalid,
    NotDone,
}

/// The check's catalog query, as `cloud_admin`: prints `done`, `invalid` (an index only) or
/// nothing.
fn check_sql(check: &OnlineCheck) -> String {
    let on_table = |schema: &str, table: &str| {
        format!(
            "JOIN pg_class t ON t.oid = {{rel}} JOIN pg_namespace n ON n.oid = t.relnamespace \
             WHERE n.nspname = {} AND t.relname = {}",
            literal(schema),
            literal(table)
        )
    };
    let body = match check {
        OnlineCheck::IndexValid {
            schema,
            table,
            name,
        } => format!(
            "SELECT CASE WHEN i.indisvalid THEN 'done' ELSE 'invalid' END FROM pg_index i \
             JOIN pg_class c ON c.oid = i.indexrelid {} AND c.relname = {} AND c.relnamespace = n.oid;",
            on_table(schema, table).replace("{rel}", "i.indrelid"),
            literal(name)
        ),
        OnlineCheck::ConstraintValidated {
            schema,
            table,
            name,
        } => format!(
            "SELECT 'done' FROM pg_constraint co {} AND co.conname = {} AND co.convalidated;",
            on_table(schema, table).replace("{rel}", "co.conrelid"),
            literal(name)
        ),
        OnlineCheck::ConstraintExists {
            schema,
            table,
            name,
        } => format!(
            "SELECT 'done' FROM pg_constraint co {} AND co.conname = {};",
            on_table(schema, table).replace("{rel}", "co.conrelid"),
            literal(name)
        ),
        OnlineCheck::NotNullSet {
            schema,
            table,
            column,
        } => format!(
            "SELECT 'done' FROM pg_attribute a {} AND a.attname = {} AND a.attnum > 0 \
             AND NOT a.attisdropped AND a.attnotnull;",
            on_table(schema, table).replace("{rel}", "a.attrelid"),
            literal(column)
        ),
    };
    format!("SET search_path = pg_catalog, pg_temp;\n{body}\n")
}

/// The line of phase 1's script psql's error names (`psql:<stdin>:<line>:`), if any.
fn error_line(error: &str) -> Option<usize> {
    let rest = &error[error.find("<stdin>:")? + "<stdin>:".len()..];
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// E5: the schema crate's role spec as the roles path's.
fn role_spec(m: &RoleSpecModel) -> Result<RoleSpec, Fail> {
    Ok(RoleSpec {
        name: m.name.clone(),
        owner_level: m.owner_level,
        login: m.login,
        inherit: m.inherit,
        connection_limit: m.connection_limit,
        valid_until: m.valid_until.as_deref().map(timestamp).transpose()?,
        bypass_rls: m.bypass_rls,
        member_of: m.member_of.clone(),
    })
}

/// E5: the schema crate's role patch as the roles path's.
fn role_patch(m: &RolePatchModel) -> Result<RolePatch, Fail> {
    Ok(RolePatch {
        owner_level: m.owner_level,
        login: m.login,
        inherit: m.inherit,
        connection_limit: m.connection_limit,
        valid_until: match &m.valid_until {
            Some(at) => Some(at.as_deref().map(timestamp).transpose()?),
            None => None,
        },
        bypass_rls: m.bypass_rls,
        member_of: m.member_of.clone(),
    })
}

fn timestamp(at: &str) -> Result<DateTime<Utc>, Fail> {
    DateTime::parse_from_rfc3339(at)
        .map(|t| t.with_timezone(&Utc))
        .map_err(|e| Fail::failed(format!("VALID UNTIL {at:?}: {e}")))
}

/// A roles-path outcome as a step's.
fn role_outcome(outcome: RoleOutcome) -> Result<StepOutcome, Fail> {
    if outcome == RoleOutcome::Ok {
        return Ok(StepOutcome::Ok);
    }
    Err(refused(outcome))
}

/// A roles-path refusal or failure as a step's failure: `code: message`.
fn refused(outcome: RoleOutcome) -> Fail {
    let reply = role_reply(outcome);
    Fail::failed(format!(
        "{}: {}",
        reply.code.unwrap_or_default(),
        reply.error.unwrap_or_default()
    ))
}

impl<M: WorkerMetadata, S: Storage, C: ComputeBackend, N: CacheNotifier, P: SpecPusher>
    Worker<M, S, C, N, P>
{
    /// Runs `plan` on `target`: phases 0, 1a, 1 (its guard expects `digest`, E4) and 2, in
    /// order, stopping at the first failure.
    pub(crate) async fn run_phases(
        &self,
        target: &Target,
        plan: &Plan,
        opts: &PlanOptions,
        digest: &str,
        mode: RunMode,
    ) -> Result<PhaseReport, PhaseFailure> {
        let mut report = PhaseReport::default();
        for phase in [Phase::Roles, Phase::EnumAdd] {
            for step in plan.steps.iter().filter(|s| s.phase == phase) {
                let run = self.step_run(target, step, opts, mode);
                match self.timed(mode, opts, phase, &step.id.0, run).await {
                    Ok(run) => report.steps.push(run),
                    Err(fail) => return Err(fail.into_failure(phase, Some(&step.id), report)),
                }
            }
        }

        // The recovery marker's table, on the admin session (P1, E15).
        if let Err(e) = self
            .computes
            .admin_sql_with(
                &target.endpoint,
                &target.database,
                &marker_setup_sql(),
                &SqlOpts::admin_default(),
            )
            .await
        {
            let fail = Fail::failed(format!("setting up the merge marker: {e:#}"));
            return Err(fail.into_failure(Phase::Tx, None, report));
        }
        let tx = self.phase1(target, plan, opts, digest);
        match self.timed(mode, opts, Phase::Tx, PHASE1_STEP, tx).await {
            Ok(run) => report.steps.push(run),
            Err(fail) => return Err(fail.into_failure(Phase::Tx, None, report)),
        }

        self.run_online(target, plan, opts, mode, report).await
    }

    /// Phase 2 only, after `report`: an apply's continuation and a retry (T8b). For real, the
    /// steps the log records as done are skipped without a check.
    pub(crate) async fn run_online(
        &self,
        target: &Target,
        plan: &Plan,
        opts: &PlanOptions,
        mode: RunMode,
        mut report: PhaseReport,
    ) -> Result<PhaseReport, PhaseFailure> {
        let done = match mode {
            RunMode::Real => self.online_done(opts.merge_id).await,
            RunMode::Rehearsal => HashSet::new(),
        };
        for step in plan.steps.iter().filter(|s| s.phase == Phase::Online) {
            if done.contains(&step.id.0) {
                report.steps.push(StepRun {
                    id: step.id.0.clone(),
                    phase: Phase::Online,
                    outcome: StepOutcome::Skipped,
                    duration_ms: 0,
                });
                continue;
            }
            let run = self.online_step(target, step, opts);
            match self.timed(mode, opts, Phase::Online, &step.id.0, run).await {
                Ok(run) => report.steps.push(run),
                Err(fail) => {
                    return Err(fail.into_failure(Phase::Online, Some(&step.id), report));
                }
            }
        }
        Ok(report)
    }

    /// Runs `work` for one step, timing it and, for real, logging its outcome.
    async fn timed(
        &self,
        mode: RunMode,
        opts: &PlanOptions,
        phase: Phase,
        id: &str,
        work: impl Future<Output = Result<StepOutcome, Fail>>,
    ) -> Result<StepRun, Fail> {
        let start = Instant::now();
        let result = work.await;
        let duration_ms = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
        if mode == RunMode::Real {
            let mut event = MergeEvent::new(opts.merge_id, "step");
            event.step_id = Some(id.to_string());
            event.phase = Some(phase_name(phase).to_string());
            event.duration_ms = i64::try_from(duration_ms).ok();
            match &result {
                Ok(StepOutcome::Ok) => event.outcome = Some("ok".into()),
                Ok(StepOutcome::Skipped) => event.outcome = Some("skipped".into()),
                Err(fail) => {
                    event.outcome = Some("failed".into());
                    event.detail = Some(serde_json::json!({ "error": fail.error }));
                }
            }
            // Best effort: a lost line only means a retry checks the step's state again.
            if let Err(e) = self.metadata.append_merge_event(&event).await {
                warn!(merge_id = %opts.merge_id, step = id, "logging the step: {e}");
            }
        }
        result.map(|outcome| StepRun {
            id: id.to_string(),
            phase,
            outcome,
            duration_ms,
        })
    }

    /// The online steps the log records as done (`ok` or `skipped`) in the current apply
    /// attempt (I2: an earlier attempt's, before a recompute, never count).
    async fn online_done(&self, merge_id: Uuid) -> HashSet<String> {
        match self.metadata.merge_events(merge_id).await {
            Ok(events) => {
                let start = attempt_start(&events);
                events.into_iter().skip(start)
                .filter(|e| {
                    e.kind == "step"
                        && e.phase.as_deref() == Some(phase_name(Phase::Online))
                        && matches!(e.outcome.as_deref(), Some("ok" | "skipped"))
                })
                .filter_map(|e| e.step_id)
                .collect()
            }
            Err(e) => {
                warn!(%merge_id, "reading the merge's log; every online step is checked: {e}");
                HashSet::new()
            }
        }
    }

    /// The owner session's options for this merge (E15, P3).
    fn owner_opts(&self, opts: &PlanOptions, timeout: std::time::Duration) -> SqlOpts {
        SqlOpts {
            timeout,
            app_name: Some(app_name(opts.merge_id)),
            // The owner's scripts carry no secret, so errors keep LINE, DETAIL and HINT.
            verbose_errors: true,
            user: Some(SCRIPT_OWNER.to_string()),
        }
    }

    /// Runs `sql` with `sql_opts`; a timeout cancels the merge's backends first.
    async fn merge_sql(
        &self,
        target: &Target,
        sql: &str,
        sql_opts: &SqlOpts,
        merge_id: Uuid,
    ) -> Result<String, Fail> {
        match self
            .computes
            .admin_sql_with(&target.endpoint, &target.database, sql, sql_opts)
            .await
        {
            Ok(out) => Ok(out),
            Err(e) => {
                let error = format!("{e:#}");
                // Told by its type (T14 apply minor 1): a user's error can say anything.
                if !chelabase_neon::is_timeout(&e) {
                    return Err(Fail::failed(error));
                }
                // The psql inside the compute still runs (T6): stop its statement.
                if let Err(e) = self
                    .computes
                    .admin_sql(&target.endpoint, &target.database, &cancel_sql(merge_id))
                    .await
                {
                    warn!(%merge_id, "cancelling the merge's backend: {e:#}");
                }
                Err(Fail {
                    kind: FailureKind::TimedOut,
                    step: None,
                    error,
                })
            }
        }
    }

    /// Phase 1: the guarded transaction.
    async fn phase1(
        &self,
        target: &Target,
        plan: &Plan,
        opts: &PlanOptions,
        digest: &str,
    ) -> Result<StepOutcome, Fail> {
        let script = phase1(plan, opts, digest);
        let sql_opts = self.owner_opts(opts, self.merge_tx_timeout);
        match self
            .merge_sql(target, &script.text, &sql_opts, opts.merge_id)
            .await
        {
            Ok(_) => Ok(StepOutcome::Ok),
            Err(mut fail) => {
                // T14 M2: placed by the line the generator gave each part, never by the text
                // (it holds user SQL): only the guard's own error is stale.
                if fail.kind == FailureKind::Failed {
                    match error_line(&fail.error).and_then(|l| script.at_line(l)) {
                        Some(ScriptPart::Guard) if fail.error.contains(STALE_ERROR) => {
                            fail.kind = FailureKind::Stale;
                        }
                        Some(ScriptPart::Step(id)) => fail.step = Some(id.clone()),
                        Some(ScriptPart::Marker) => fail.step = Some(StepId(MARKER_STEP_ID.into())),
                        Some(ScriptPart::Commit) => fail.step = Some(StepId(COMMIT_STEP_ID.into())),
                        Some(ScriptPart::Prelude | ScriptPart::Guard) | None => {}
                    }
                }
                Err(fail)
            }
        }
    }

    /// A phase 0 or phase 1a step.
    async fn step_run(
        &self,
        target: &Target,
        step: &Step,
        opts: &PlanOptions,
        mode: RunMode,
    ) -> Result<StepOutcome, Fail> {
        if step.phase == Phase::EnumAdd {
            let sql_opts = self.owner_opts(opts, self.merge_tx_timeout);
            self.merge_sql(target, &step_script(step, opts), &sql_opts, opts.merge_id)
                .await?;
            return Ok(StepOutcome::Ok);
        }
        let Some(op) = &step.role_op else {
            return Err(Fail::failed(format!(
                "phase 0 step {} has no role operation",
                step.id.0
            )));
        };
        match (op, mode) {
            (RoleOp::Create(m), RunMode::Real) => {
                let spec = role_spec(m)?;
                // Part 5 (T14 I3): phase 0 is idempotent. A role whose API row is settled and
                // says what the plan creates was made by an earlier attempt: skipped. A row
                // stored but not applied (`pending`) is driven again first.
                let Some(row) = self.api_row(&target.endpoint, &spec.name).await? else {
                    return role_outcome(self.create_role(target.branch, &spec).await);
                };
                let driven = row.pending || row.attributes_pending;
                let row = if driven {
                    self.drive_again(target, &spec.name).await?
                } else {
                    row
                };
                if row.login == spec.login
                    && row.api_owner_level == spec.owner_level
                    && row.attributes == spec.attributes()
                {
                    // Made now (by the resync), or by an earlier attempt.
                    return Ok(if driven { StepOutcome::Ok } else { StepOutcome::Skipped });
                }
                Err(Fail::failed(format!(
                    "role_exists: a role named {} was made through the API since the merge was \
                     computed, with other attributes; recompute the merge",
                    spec.name
                )))
            }
            (RoleOp::Update { name, patch }, RunMode::Real) => {
                let patch = role_patch(patch)?;
                role_outcome(self.update_role(target.branch, name, &patch).await)
            }
            (RoleOp::Create(m), RunMode::Rehearsal) => self.rehearse_create(target, m).await,
            (RoleOp::Update { name, patch }, RunMode::Rehearsal) => {
                self.rehearse_update(target, name, patch).await
            }
            // E29: renamed through the control plane (its row, proxy copies and keys move with
            // it); in a rehearsal by SQL only.
            (RoleOp::Rename { from, to, .. }, mode) => {
                let names = self.role_names(target, &[from, to]).await?;
                if names.contains(to) && !names.contains(from) {
                    // An earlier attempt renamed it.
                    return Ok(StepOutcome::Skipped);
                }
                match mode {
                    RunMode::Real => role_outcome(self.rename_role(target.branch, from, to).await),
                    RunMode::Rehearsal => self.rehearse_rename(target, from, to).await,
                }
            }
        }
    }

    /// The API row of role `name` on `endpoint`, if any.
    async fn api_row(&self, endpoint: &str, name: &str) -> Result<Option<RoleCopy>, Fail> {
        let (rows, _) = self
            .metadata
            .role_copies(endpoint)
            .await
            .map_err(|e| Fail::failed(format!("reading the roles: {e}")))?;
        Ok(rows
            .into_iter()
            .find(|r| r.origin == RoleOrigin::Api && r.name == name))
    }

    /// A pending API row driven again (T14 I3): under the endpoint's lock the resync makes the
    /// role as stored and confirms the row. The row after it; still pending is a failure.
    async fn drive_again(&self, target: &Target, name: &str) -> Result<RoleCopy, Fail> {
        {
            let _lock = self
                .metadata
                .lock_endpoint(&target.endpoint)
                .await
                .map_err(|e| Fail::failed(format!("locking the endpoint: {e}")))?;
            self.resync_locked(target.branch, &target.endpoint, &target.database)
                .await
                .map_err(Fail::failed)?;
        }
        match self.api_row(&target.endpoint, name).await? {
            Some(row) if !row.pending && !row.attributes_pending => Ok(row),
            _ => Err(Fail::failed(format!(
                "not_applied: role {name} is stored but couldn't be made yet; retry the merge"
            ))),
        }
    }

    /// Which of `names` exist on `target`'s compute.
    async fn role_names(
        &self,
        target: &Target,
        names: &[&String],
    ) -> Result<HashSet<String>, Fail> {
        let names: Vec<&str> = names.iter().map(|n| n.as_str()).collect();
        self.computes
            .admin_sql(&target.endpoint, &target.database, &role_names_sql(&names))
            .await
            .map_err(|e| format!("{e:#}"))
            .and_then(|out| parse_role_names(&out))
            .map_err(|e| Fail::failed(format!("reading the roles: {e}")))
    }

    /// The endpoint whose role rows are `target`'s: the branch's own, or for a merge's rehearsal
    /// the target it was made from (a rehearsal has none of its own).
    async fn rows_endpoint(&self, target: &Target) -> Result<String, Fail> {
        let branch = self
            .metadata
            .get_branch(target.branch)
            .await
            .map_err(|e| Fail::failed(format!("reading the branch: {e}")))?;
        let parent = match branch {
            Some(b) if b.kind == BranchKind::Rehearsal => b.parent_branch_id,
            _ => None,
        };
        let Some(parent) = parent else {
            return Ok(target.endpoint.clone());
        };
        self.metadata
            .get_branch(parent)
            .await
            .map_err(|e| Fail::failed(format!("reading the target: {e}")))?
            .and_then(|b| b.endpoint_id)
            .ok_or_else(|| Fail::failed("the rehearsal's target has no compute".to_string()))
    }

    /// A rehearsal's role create: the roles path's rules, then by SQL, NOLOGIN, no password.
    async fn rehearse_create(
        &self,
        target: &Target,
        m: &RoleSpecModel,
    ) -> Result<StepOutcome, Fail> {
        let spec = role_spec(m)?;
        check_spec(&spec).map_err(refused)?;
        // `exists_sql` pins the search path (the superuser session, fix round 1).
        let exists = self
            .computes
            .admin_sql(&target.endpoint, &target.database, &exists_sql(&spec.name))
            .await
            .map_err(|e| Fail::failed(format!("checking role {}: {e:#}", spec.name)))?;
        if !exists.trim().is_empty() {
            // As the apply decides (T14 I3): the target's settled API row saying what the plan
            // creates is skipped; any other role of that name is refused.
            let rows_ep = self.rows_endpoint(target).await?;
            return match self.api_row(&rows_ep, &spec.name).await? {
                Some(r)
                    if r.pending
                        || r.attributes_pending
                        || (r.login == spec.login
                            && r.api_owner_level == spec.owner_level
                            && r.attributes == spec.attributes()) =>
                {
                    Ok(StepOutcome::Skipped)
                }
                _ => Err(refused(RoleOutcome::Refused(
                    "role_exists",
                    format!("a role named {} already exists", spec.name),
                ))),
            };
        }
        // T14 I4: the membership checks the apply's roles path runs, on the rehearsal.
        self.check_member_targets(&target.endpoint, &target.database, &spec.member_of)
            .await
            .map_err(refused)?;
        let attributes = spec.attributes();
        let wanted = WantedRole {
            name: &spec.name,
            owner_level: spec.owner_level,
            login: false,
            attributes: &attributes,
        };
        let sql = format!(
            "SET search_path = pg_catalog, pg_temp;\n{}",
            create_role_sql(OWNER_ROLE, &wanted, None)
        );
        self.computes
            .admin_sql(&target.endpoint, &target.database, &sql)
            .await
            .map_err(|e| Fail::failed(format!("creating role {}: {e:#}", spec.name)))?;
        Ok(StepOutcome::Ok)
    }

    /// A rehearsal's role rename (E29): what the apply's `rename_role` refuses is refused here
    /// too (T14 F3 M5: the names, a role that isn't the user's, a new name taken in SQL or by a
    /// row of the target), then by SQL only (no row, no keys: a rehearsal has none of its own).
    async fn rehearse_rename(
        &self,
        target: &Target,
        from: &str,
        to: &str,
    ) -> Result<StepOutcome, Fail> {
        check_rename(from, to).map_err(refused)?;
        let observed = self
            .computes
            .admin_sql(
                &target.endpoint,
                &target.database,
                &role_read_sql(OWNER_ROLE, from),
            )
            .await
            .map_err(|e| format!("{e:#}"))
            .and_then(|out| parse_observed(&out).map_err(|e| e.to_string()))
            .map_err(|e| Fail::failed(format!("reading role {from}: {e}")))?;
        let observed = observed.roles.into_iter().find(|o| o.name == from);
        let rows_ep = self.rows_endpoint(target).await?;
        let (rows, _) = self
            .metadata
            .role_copies(&rows_ep)
            .await
            .map_err(|e| Fail::failed(format!("reading the roles: {e}")))?;
        let row = rows
            .iter()
            .find(|r| r.origin != RoleOrigin::Owner && r.name == from);
        if !rename_is_ours(observed.as_ref(), row) {
            return Err(refused(RoleOutcome::NotFound));
        }
        let in_sql = self
            .computes
            .admin_sql(&target.endpoint, &target.database, &exists_sql(to))
            .await
            .map_err(|e| Fail::failed(format!("checking role {to}: {e:#}")))?;
        if !in_sql.trim().is_empty() || rows.iter().any(|r| r.name == to) {
            return Err(refused(name_taken(to)));
        }
        let sql = format!(
            "SET search_path = pg_catalog, pg_temp;\nALTER ROLE {} RENAME TO {};",
            chelabase_types::role_name::quote_ident(from),
            chelabase_types::role_name::quote_ident(to)
        );
        self.computes
            .admin_sql(&target.endpoint, &target.database, &sql)
            .await
            .map_err(|e| Fail::failed(format!("renaming role {from}: {e:#}")))?;
        Ok(StepOutcome::Ok)
    }

    /// A rehearsal's role update: the roles path's rules, then by SQL against the role's
    /// catalog state; LOGIN is never turned on (no password).
    async fn rehearse_update(
        &self,
        target: &Target,
        name: &str,
        m: &RolePatchModel,
    ) -> Result<StepOutcome, Fail> {
        let mut patch = role_patch(m)?;
        check_patch(name, &patch).map_err(refused)?;
        patch.login = None;
        let observed = self
            .computes
            .admin_sql(
                &target.endpoint,
                &target.database,
                &role_read_sql(OWNER_ROLE, name),
            )
            .await
            .map_err(|e| format!("{e:#}"))
            .and_then(|out| parse_observed(&out).map_err(|e| e.to_string()))
            .map_err(|e| Fail::failed(format!("reading role {name}: {e}")))?;
        let observed = observed.roles.into_iter().find(|o| o.name == name);
        // T14 I4: as the apply's roles path decides: a role with no API row on the target is
        // the user's only when the owner administers it.
        let rows_ep = self.rows_endpoint(target).await?;
        let (rows, _) = self
            .metadata
            .role_copies(&rows_ep)
            .await
            .map_err(|e| Fail::failed(format!("reading the roles: {e}")))?;
        let row = rows
            .iter()
            .find(|r| r.origin != RoleOrigin::Owner && r.name == name);
        current_role_state(row, observed.as_ref()).map_err(refused)?;
        let Some(o) = observed else {
            return Err(Fail::failed(format!("the role {name} doesn't exist")));
        };
        if let Some(members) = &patch.member_of {
            self.check_member_targets(&target.endpoint, &target.database, members)
                .await
                .map_err(refused)?;
        }
        let attributes = patch.apply(&o.attributes());
        let wanted = WantedRole {
            name,
            owner_level: patch.owner_level.unwrap_or(o.owner_attributes),
            login: o.login,
            attributes: &attributes,
        };
        let statements = update_statements(&wanted, &o, None, &patch);
        if statements.is_empty() {
            return Ok(StepOutcome::Skipped);
        }
        let sql = format!(
            "SET search_path = pg_catalog, pg_temp;\n{}",
            statements.join("\n")
        );
        self.computes
            .admin_sql(&target.endpoint, &target.database, &sql)
            .await
            .map_err(|e| Fail::failed(format!("changing role {name}: {e:#}")))?;
        Ok(StepOutcome::Ok)
    }

    /// A phase 2 step: its check, an invalid index dropped, then the statement.
    async fn online_step(
        &self,
        target: &Target,
        step: &Step,
        opts: &PlanOptions,
    ) -> Result<StepOutcome, Fail> {
        let sql_opts = self.owner_opts(opts, self.merge_online_step_timeout);
        if let Some(check) = &step.online {
            match self.check_online(target, check).await? {
                CheckState::Done => return Ok(StepOutcome::Skipped),
                CheckState::Invalid => {
                    if let OnlineCheck::IndexValid { schema, name, .. } = check {
                        let drop = format!(
                            "SET statement_timeout = 0;\nDROP INDEX CONCURRENTLY IF EXISTS {};\n",
                            qualified(schema, name)
                        );
                        self.merge_sql(target, &drop, &sql_opts, opts.merge_id)
                            .await?;
                    }
                }
                CheckState::NotDone => {}
            }
        }
        self.merge_sql(target, &step_script(step, opts), &sql_opts, opts.merge_id)
            .await?;
        Ok(StepOutcome::Ok)
    }

    async fn check_online(&self, target: &Target, check: &OnlineCheck) -> Result<CheckState, Fail> {
        let out = self
            .computes
            .admin_sql(&target.endpoint, &target.database, &check_sql(check))
            .await
            .map_err(|e| Fail::failed(format!("checking the step's state: {e:#}")))?;
        Ok(match out.trim() {
            "done" => CheckState::Done,
            "invalid" => CheckState::Invalid,
            _ => CheckState::NotDone,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use chelabase_metadata::{MergeDirection, MergeStatus};
    use chelabase_neon::SqlOpts;
    use chelabase_schema::plan::{
        OnlineCheck, Phase, Plan, PlanOptions, RoleOp, RoleSpecModel, Step, StepId, StepKind,
        phase1_script,
    };
    use uuid::Uuid;

    use super::*;
    use crate::fakes::{FakeWorker, Journal, entries, ready_database, worker};
    use crate::merge::testkit::DIGEST;

    fn step(id: &str, phase: Phase, sql: &str) -> Step {
        Step {
            id: StepId(id.to_string()),
            kind: StepKind::Generated,
            phase,
            sql: sql.to_string(),
            items: Vec::new(),
            data_loss: false,
            rewrite: false,
            security: false,
            locks: Vec::new(),
            role_op: None,
            online: None,
            breaking: None,
        }
    }

    fn index_step(id: &str, name: &str) -> Step {
        Step {
            online: Some(OnlineCheck::IndexValid {
                schema: "public".into(),
                table: "orders".into(),
                name: name.into(),
            }),
            ..step(
                id,
                Phase::Online,
                &format!("CREATE INDEX CONCURRENTLY \"{name}\" ON public.orders (id)"),
            )
        }
    }

    fn role_step(id: &str, name: &str, login: bool) -> Step {
        let spec = RoleSpecModel {
            name: name.into(),
            owner_level: false,
            login,
            inherit: true,
            connection_limit: -1,
            valid_until: None,
            bypass_rls: true,
            member_of: Vec::new(),
        };
        Step {
            role_op: Some(RoleOp::Create(spec)),
            security: true,
            ..step(id, Phase::Roles, &format!("-- create role {name}"))
        }
    }

    /// Main of a ready database, with a merge row for the log.
    async fn target(w: &FakeWorker) -> (Target, PlanOptions) {
        let db = ready_database(w).await;
        let main = w.metadata.main(db);
        let merge_id = w.metadata.insert_merge(
            db,
            main.id,
            main.id,
            MergeDirection::IntoParent,
            MergeStatus::Applying,
        );
        let target = Target {
            branch: main.id,
            endpoint: main.endpoint_id.clone().unwrap(),
            database: "app".to_string(),
        };
        (
            target,
            PlanOptions {
                lock_timeout_ms: 5000,
                merge_id,
            },
        )
    }

    /// Every admin SQL that reached the compute, with its options.
    fn runs(w: &FakeWorker) -> Vec<(String, SqlOpts)> {
        let sql = w.computes.sql.lock().unwrap().clone();
        let opts = w.computes.sql_opts.lock().unwrap().clone();
        sql.into_iter().map(|(_, _, q)| q).zip(opts).collect()
    }

    fn ran(w: &FakeWorker, needle: &str) -> usize {
        runs(w).iter().filter(|(q, _)| q.contains(needle)).count()
    }

    fn first(w: &FakeWorker, needle: &str) -> (usize, SqlOpts) {
        runs(w)
            .into_iter()
            .enumerate()
            .find(|(_, (q, _))| q.contains(needle))
            .map(|(i, (_, o))| (i, o))
            .unwrap_or_else(|| panic!("{needle:?} never ran"))
    }

    fn app_name(opts: &PlanOptions) -> String {
        format!("chelabase-merge-{}", opts.merge_id)
    }

    fn logged(journal: &Journal) -> Vec<String> {
        entries(journal)
            .into_iter()
            .filter_map(|e| {
                e.strip_prefix("metadata.append_merge_event ")
                    .map(str::to_string)
            })
            .collect()
    }

    fn plan(steps: Vec<Step>) -> Plan {
        Plan { steps }
    }

    #[tokio::test]
    async fn phases_run_in_order_in_the_owner_session() {
        let (w, journal) = worker();
        let (t, opts) = target(&w).await;
        let plan = plan(vec![
            step(
                "e1",
                Phase::EnumAdd,
                "ALTER TYPE public.s ADD VALUE IF NOT EXISTS 'x'",
            ),
            step("t1", Phase::Tx, "CREATE TABLE public.a (id bigint)"),
            step("t2", Phase::Tx, "CREATE TABLE public.b (id bigint)"),
            index_step("o1", "ix_a"),
        ]);
        journal.lock().unwrap().clear();
        w.computes.sql.lock().unwrap().clear();
        w.computes.sql_opts.lock().unwrap().clear();

        let report = w
            .run_phases(&t, &plan, &opts, DIGEST, RunMode::Real)
            .await
            .unwrap();

        let (enum_at, enum_opts) = first(&w, "ADD VALUE");
        // E19: 1a waits for its lock at most the merge's lock_timeout; a concurrent build doesn't.
        assert!(
            runs(&w)[enum_at]
                .0
                .starts_with("SET lock_timeout = '5000ms';\n")
        );
        let (build_at, _) = first(&w, "CREATE INDEX CONCURRENTLY");
        assert!(!runs(&w)[build_at].0.contains("lock_timeout"));
        let (setup_at, setup_opts) = first(&w, "CREATE SCHEMA IF NOT EXISTS chelabase_merge");
        let (tx_at, tx_opts) = first(&w, "$chelabase_guard$");
        // The guard's digest names `indisvalid` too: the check is the one naming the index.
        let (check_at, check_opts) = first(&w, "'ix_a'");
        let (online_at, online_opts) = first(&w, "CREATE INDEX CONCURRENTLY");
        assert!(enum_at < setup_at && setup_at < tx_at && tx_at < check_at && check_at < online_at);
        let owner = Some("postgres".to_string());
        // E15: the scripts run as the owner; the marker setup and catalog checks as cloud_admin.
        assert_eq!(enum_opts.user, owner);
        assert_eq!(tx_opts.user, owner);
        assert_eq!(online_opts.user, owner);
        assert_eq!((setup_opts.user, check_opts.user), (None, None));
        // P3: named sessions and timeouts.
        assert_eq!(tx_opts.app_name, Some(app_name(&opts)));
        assert_eq!(online_opts.app_name, Some(app_name(&opts)));
        assert_eq!(tx_opts.timeout, w.merge_tx_timeout);
        assert_eq!(enum_opts.timeout, w.merge_tx_timeout);
        assert_eq!(online_opts.timeout, w.merge_online_step_timeout);
        assert!(tx_opts.verbose_errors);
        let script = &runs(&w)[tx_at].0;
        assert_eq!(script, &phase1_script(&plan, &opts, DIGEST));
        // The report and the log, step by step.
        let ids: Vec<(&str, StepOutcome)> = report
            .steps
            .iter()
            .map(|s| (s.id.as_str(), s.outcome))
            .collect();
        assert_eq!(
            ids,
            [
                ("e1", StepOutcome::Ok),
                (PHASE1_STEP, StepOutcome::Ok),
                ("o1", StepOutcome::Ok)
            ]
        );
        assert_eq!(
            logged(&journal),
            [
                "step enum_add e1 ok",
                "step tx phase1 ok",
                "step online o1 ok"
            ]
        );
    }

    #[tokio::test]
    async fn phase0_is_idempotent() {
        let (w, journal) = worker();
        let (t, opts) = target(&w).await;
        let plan = plan(vec![role_step("r1", "reporter", true)]);

        let first = w
            .run_phases(&t, &plan, &opts, DIGEST, RunMode::Real)
            .await
            .unwrap();
        assert_eq!(first.steps[0].outcome, StepOutcome::Ok);
        assert_eq!(ran(&w, "CREATE ROLE \"reporter\""), 1);
        // Through the control plane (P5): the role has its row, and a password.
        assert!(
            entries(&journal)
                .iter()
                .any(|e| e.starts_with("metadata.insert_api_role"))
        );

        // A second run (a redelivery) finds the role in metadata and skips it.
        let again = w
            .run_phases(&t, &plan, &opts, DIGEST, RunMode::Real)
            .await
            .unwrap();
        assert_eq!(again.steps[0].outcome, StepOutcome::Skipped);
        assert_eq!(ran(&w, "CREATE ROLE \"reporter\""), 1);
        assert!(logged(&journal).contains(&"step roles r1 skipped".to_string()));
    }

    // P5 and T9: a rehearsal makes roles by SQL only, NOLOGIN, with no password and no row.
    #[tokio::test]
    async fn rehearsal_phase0_creates_nologin_roles_by_sql() {
        let (w, journal) = worker();
        let (t, opts) = target(&w).await;
        let plan = plan(vec![role_step("r1", "reporter", true)]);
        journal.lock().unwrap().clear();

        let report = w
            .run_phases(&t, &plan, &opts, DIGEST, RunMode::Rehearsal)
            .await
            .unwrap();

        assert_eq!(report.steps[0].outcome, StepOutcome::Ok);
        let (at, role_opts) = first(&w, "CREATE ROLE \"reporter\"");
        let sql = &runs(&w)[at].0;
        assert!(
            sql.contains("NOLOGIN") && sql.contains("BYPASSRLS"),
            "{sql}"
        );
        assert!(!sql.contains("PASSWORD"), "{sql}");
        assert_eq!(role_opts.user, None);
        let all = entries(&journal);
        assert!(
            !all.iter().any(|e| e.starts_with("metadata.insert_api_role")
                || e.starts_with("metadata.append_merge_event")),
            "no metadata in a rehearsal: {all:?}"
        );

        // A role the rehearsal branch already has is decided as the apply would (T14 I3): not the
        // target's API role of the plan's spec, refused; that one, skipped.
        w.computes
            .answer_at(&t.endpoint, "WHERE rolname = 'reporter'", "1\n");
        let taken = w
            .run_phases(&t, &plan, &opts, DIGEST, RunMode::Rehearsal)
            .await
            .unwrap_err();
        assert!(taken.error.starts_with("role_exists"), "{}", taken.error);
        let Some(RoleOp::Create(spec)) = &plan.steps[0].role_op else { unreachable!() };
        let spec = role_spec(spec).ok().unwrap();
        w.computes.answers_at.lock().unwrap().clear();
        assert_eq!(w.create_role(t.branch, &spec).await, RoleOutcome::Ok);
        w.computes
            .answer_at(&t.endpoint, "WHERE rolname = 'reporter'", "1\n");
        let again = w
            .run_phases(&t, &plan, &opts, DIGEST, RunMode::Rehearsal)
            .await
            .unwrap();
        assert_eq!(again.steps[0].outcome, StepOutcome::Skipped);
        assert_eq!(ran(&w, "CREATE ROLE \"reporter\""), 2, "the rehearsal's, then the API's");
    }

    // Review Focus 2: the target changed since the rehearsal; phase 1 rolls back, nothing runs
    // after it.
    #[tokio::test]
    async fn phase1_recheck_mismatch_is_stale_and_runs_nothing_after() {
        let (w, journal) = worker();
        let (t, opts) = target(&w).await;
        let plan = plan(vec![
            step("t1", Phase::Tx, "CREATE TABLE public.a (id bigint)"),
            index_step("o1", "ix_a"),
        ]);
        *w.computes.fail_sql_containing.lock().unwrap() = Some((
            "$chelabase_guard$".into(),
            "psql in compute ep exited with 3: psql:<stdin>:9: ERROR:  chelabase_merge_stale"
                .into(),
        ));

        let failure = w
            .run_phases(&t, &plan, &opts, DIGEST, RunMode::Real)
            .await
            .unwrap_err();

        assert_eq!(failure.kind, FailureKind::Stale);
        assert_eq!(failure.phase, Phase::Tx);
        assert_eq!(ran(&w, "CREATE INDEX CONCURRENTLY"), 0);
        assert_eq!(ran(&w, "'ix_a'"), 0, "no check either");
        assert_eq!(logged(&journal), ["step tx phase1 failed"]);
    }

    #[tokio::test]
    async fn phase1_failure_names_its_step() {
        let (w, _journal) = worker();
        let (t, opts) = target(&w).await;
        let plan = plan(vec![
            step("t1", Phase::Tx, "CREATE TABLE public.a (id bigint)"),
            step("t2", Phase::Tx, "ALTER TABLE public.a ADD COLUMN x nope"),
            index_step("o1", "ix_a"),
        ]);
        let script = phase1_script(&plan, &opts, DIGEST);
        let line = script
            .lines()
            .position(|l| l.contains("ADD COLUMN x nope"))
            .unwrap()
            + 1;
        *w.computes.fail_sql_containing.lock().unwrap() = Some((
            "$chelabase_guard$".into(),
            format!(
                "psql in compute ep exited with 3: psql:<stdin>:{line}: ERROR:  type \"nope\" does not exist"
            ),
        ));

        let failure = w
            .run_phases(&t, &plan, &opts, DIGEST, RunMode::Rehearsal)
            .await
            .unwrap_err();

        assert_eq!(failure.kind, FailureKind::Failed);
        assert_eq!(failure.phase, Phase::Tx);
        assert_eq!(failure.step, Some(StepId("t2".into())));
        assert!(
            failure.error.contains("does not exist"),
            "{}",
            failure.error
        );
        assert_eq!(ran(&w, "CREATE INDEX CONCURRENTLY"), 0);
    }

    // Review Focus 3: an INVALID index left by an interrupted build is dropped concurrently and
    // rebuilt; a valid one is skipped.
    #[tokio::test]
    async fn resume_drops_an_invalid_index_and_rebuilds() {
        let (w, _journal) = worker();
        let (t, opts) = target(&w).await;
        let plan = plan(vec![
            index_step("o1", "ix_bad"),
            index_step("o2", "ix_good"),
        ]);
        w.computes.answer_at(&t.endpoint, "'ix_bad'", "invalid\n");
        w.computes.answer_at(&t.endpoint, "'ix_good'", "done\n");

        let report = w
            .run_online(&t, &plan, &opts, RunMode::Real, PhaseReport::default())
            .await
            .unwrap();

        let (drop_at, drop_opts) = first(&w, "DROP INDEX CONCURRENTLY IF EXISTS public.ix_bad");
        let (build_at, _) = first(&w, "CREATE INDEX CONCURRENTLY \"ix_bad\"");
        assert!(drop_at < build_at);
        assert_eq!(drop_opts.user, Some("postgres".into()));
        assert_eq!(ran(&w, "CREATE INDEX CONCURRENTLY \"ix_good\""), 0);
        let outcomes: Vec<StepOutcome> = report.steps.iter().map(|s| s.outcome).collect();
        assert_eq!(outcomes, [StepOutcome::Ok, StepOutcome::Skipped]);
    }

    // P3: a step past its timeout has its backend cancelled by application name.
    #[tokio::test]
    async fn online_step_timeout_cancels_the_backend() {
        let (mut w, _journal) = worker();
        w.merge_online_step_timeout = Duration::from_secs(1);
        let (t, opts) = target(&w).await;
        let plan = plan(vec![index_step("o1", "ix_a")]);
        *w.computes.sql_delay.lock().unwrap() = Some(Duration::from_secs(5));

        let failure = w
            .run_phases(&t, &plan, &opts, DIGEST, RunMode::Real)
            .await
            .unwrap_err();

        assert_eq!(failure.kind, FailureKind::TimedOut);
        assert_eq!(failure.phase, Phase::Online);
        assert_eq!(failure.step, Some(StepId("o1".into())));
        let (at, cancel_opts) = first(&w, "pg_cancel_backend");
        let sql = &runs(&w)[at].0;
        assert!(
            sql.contains(&format!("application_name = '{}'", app_name(&opts))),
            "{sql}"
        );
        assert_eq!(cancel_opts.user, None);
    }

    #[tokio::test]
    async fn online_failure_then_retry_continues_after_last_ok_step() {
        let (w, journal) = worker();
        let (t, opts) = target(&w).await;
        let plan = plan(vec![
            index_step("o1", "ix_a"),
            index_step("o2", "ix_b"),
            index_step("o3", "ix_c"),
        ]);
        *w.computes.fail_sql_containing.lock().unwrap() = Some((
            "CREATE INDEX CONCURRENTLY \"ix_b\"".into(),
            "lock timeout".into(),
        ));

        let failure = w
            .run_phases(&t, &plan, &opts, DIGEST, RunMode::Real)
            .await
            .unwrap_err();
        assert_eq!(failure.kind, FailureKind::Failed);
        assert_eq!(
            (failure.phase, failure.step.clone()),
            (Phase::Online, Some(StepId("o2".into())))
        );
        assert_eq!(ran(&w, "\"ix_c\""), 0);

        // The retry: o1 is done by the log, so it isn't even checked.
        *w.computes.fail_sql_containing.lock().unwrap() = None;
        journal.lock().unwrap().clear();
        let report = w
            .run_online(&t, &plan, &opts, RunMode::Real, PhaseReport::default())
            .await
            .unwrap();
        assert_eq!(ran(&w, "CREATE INDEX CONCURRENTLY \"ix_a\""), 1);
        assert_eq!(ran(&w, "'ix_a'"), 1, "checked once, on the first run");
        assert_eq!(ran(&w, "CREATE INDEX CONCURRENTLY \"ix_b\""), 1);
        assert_eq!(ran(&w, "CREATE INDEX CONCURRENTLY \"ix_c\""), 1);
        let outcomes: Vec<(&str, StepOutcome)> = report
            .steps
            .iter()
            .map(|s| (s.id.as_str(), s.outcome))
            .collect();
        assert_eq!(
            outcomes,
            [
                ("o1", StepOutcome::Skipped),
                ("o2", StepOutcome::Ok),
                ("o3", StepOutcome::Ok)
            ]
        );
        assert_eq!(logged(&journal), ["step online o2 ok", "step online o3 ok"]);
    }

    // E19: a locking phase 2 step runs with the merge's lock_timeout; a lock timeout fails it
    // (online), and the retry runs it again.
    #[tokio::test]
    async fn online_lock_timeout_fails_the_step_and_retry_runs_it_again() {
        let (w, _journal) = worker();
        let (t, opts) = target(&w).await;
        let mut set_not_null = step(
            "o1",
            Phase::Online,
            "ALTER TABLE public.orders ALTER COLUMN id SET NOT NULL",
        );
        set_not_null.locks = vec!["ACCESS EXCLUSIVE public.orders".into()];
        let plan = plan(vec![set_not_null]);
        *w.computes.fail_sql_containing.lock().unwrap() = Some((
            "SET NOT NULL".into(),
            "psql:<stdin>:3: ERROR:  canceling statement due to lock timeout".into(),
        ));

        let failure = w
            .run_phases(&t, &plan, &opts, DIGEST, RunMode::Real)
            .await
            .unwrap_err();
        assert_eq!(failure.kind, FailureKind::Failed);
        assert_eq!(
            (failure.phase, failure.step.clone()),
            (Phase::Online, Some(StepId("o1".into())))
        );

        *w.computes.fail_sql_containing.lock().unwrap() = None;
        w.run_online(&t, &plan, &opts, RunMode::Real, PhaseReport::default())
            .await
            .unwrap();
        let (at, _) = first(&w, "SET NOT NULL");
        assert!(
            runs(&w)[at]
                .0
                .starts_with("SET statement_timeout = 0;\nSET lock_timeout = '5000ms';\n")
        );
    }

    /// psql's failure at `line` of a script.
    fn psql_error(line: usize, text: &str) -> String {
        format!("psql in compute ep exited with 3: psql:<stdin>:{line}: ERROR:  {text}")
    }

    /// The 1-based line of `script` that holds `needle`.
    fn line_of(script: &str, needle: &str) -> usize {
        script
            .lines()
            .position(|l| l.contains(needle))
            .unwrap_or_else(|| panic!("{needle:?} not in {script}"))
            + 1
    }

    // T14 M2 (apply minors 1, 2): a phase 1 failure is placed by the line the script's
    // generator gave each part: a user's `-- step` comment can't steer it, a failure at the
    // marker or at COMMIT is named so, and only the guard's own failure is stale (a user's RAISE
    // of the same text isn't).
    #[tokio::test]
    async fn phase1_failures_map_by_the_script_parts() {
        let plan = plan(vec![
            step("t1", Phase::Tx, "SELECT 42"),
            step("t2", Phase::Tx, "-- step t1\nALTER TABLE public.a ADD COLUMN x nope"),
        ]);
        let cases: Vec<(&str, &str, FailureKind, Option<&str>)> = vec![
            ("ADD COLUMN x nope", "type \"nope\" does not exist", FailureKind::Failed, Some("t2")),
            ("INSERT INTO chelabase_merge.applied", "permission denied", FailureKind::Failed, Some("chelabase:marker")),
            ("COMMIT;", "insert or update violates foreign key", FailureKind::Failed, Some("chelabase:commit")),
            ("RAISE EXCEPTION", STALE_ERROR, FailureKind::Stale, None),
            ("SELECT 42", STALE_ERROR, FailureKind::Failed, Some("t1")),
        ];
        for (at, text, kind, blamed) in cases {
            let (w, _journal) = worker();
            let (t, opts) = target(&w).await;
            let script = phase1_script(&plan, &opts, DIGEST);
            *w.computes.fail_sql_containing.lock().unwrap() =
                Some(("$chelabase_guard$".into(), psql_error(line_of(&script, at), text)));
            let failure = w
                .run_phases(&t, &plan, &opts, DIGEST, RunMode::Rehearsal)
                .await
                .unwrap_err();
            assert_eq!(failure.kind, kind, "{at}");
            assert_eq!(failure.step.map(|s| s.0), blamed.map(str::to_string), "{at}");
        }
        // An error naming the stale text without a line (not from the guard) isn't stale either.
        let (w, _journal) = worker();
        let (t, opts) = target(&w).await;
        *w.computes.fail_sql_containing.lock().unwrap() =
            Some(("$chelabase_guard$".into(), format!("connection reset: {STALE_ERROR}")));
        let failure = w
            .run_phases(&t, &plan, &opts, DIGEST, RunMode::Rehearsal)
            .await
            .unwrap_err();
        assert_eq!((failure.kind, failure.step), (FailureKind::Failed, None));
    }

    // T14 apply minor 1: a timeout is told by its type, not by an error's text.
    #[tokio::test]
    async fn a_timeout_is_told_by_its_type() {
        let (w, _journal) = worker();
        let (t, opts) = target(&w).await;
        let plan = plan(vec![index_step("o1", "ix_a")]);
        *w.computes.fail_sql_containing.lock().unwrap() = Some((
            "CREATE INDEX CONCURRENTLY".into(),
            "psql:<stdin>:2: ERROR:  timed out after 5s".into(),
        ));
        let failure = w
            .run_phases(&t, &plan, &opts, DIGEST, RunMode::Real)
            .await
            .unwrap_err();
        assert_eq!(failure.kind, FailureKind::Failed);
        assert_eq!(ran(&w, "pg_cancel_backend"), 0, "nothing to cancel");

        let (w, _journal) = worker();
        let (t, opts) = target(&w).await;
        w.computes.time_out_when("CREATE INDEX CONCURRENTLY", false);
        let failure = w
            .run_phases(&t, &plan, &opts, DIGEST, RunMode::Real)
            .await
            .unwrap_err();
        assert_eq!(failure.kind, FailureKind::TimedOut);
        assert_eq!(ran(&w, "pg_cancel_backend"), 1);
    }

    // T14 M12 (test gap): a marker setup that fails fails phase 1 before its transaction.
    #[tokio::test]
    async fn a_marker_setup_failure_fails_in_tx_before_the_transaction() {
        let (w, _journal) = worker();
        let (t, opts) = target(&w).await;
        let plan = plan(vec![step("t1", Phase::Tx, "CREATE TABLE public.a (id bigint)")]);
        *w.computes.fail_sql_containing.lock().unwrap() = Some((
            "CREATE SCHEMA IF NOT EXISTS chelabase_merge".into(),
            "psql:<stdin>:3: ERROR:  chelabase_merge_marker_untrusted".into(),
        ));
        let failure = w
            .run_phases(&t, &plan, &opts, DIGEST, RunMode::Real)
            .await
            .unwrap_err();
        assert_eq!((failure.kind, failure.phase, failure.step), (FailureKind::Failed, Phase::Tx, None));
        assert!(failure.error.contains("setting up the merge marker"), "{}", failure.error);
        assert_eq!(ran(&w, "$chelabase_guard$"), 0);
    }

    // T14 M12 (test gap): a validated constraint and a NOT NULL already set are done.
    #[tokio::test]
    async fn checks_skip_a_validated_constraint_and_a_set_not_null() {
        let (w, _journal) = worker();
        let (t, opts) = target(&w).await;
        let validate = Step {
            online: Some(OnlineCheck::ConstraintValidated {
                schema: "public".into(),
                table: "orders".into(),
                name: "orders_fk".into(),
            }),
            ..step("o1", Phase::Online, "ALTER TABLE public.orders VALIDATE CONSTRAINT orders_fk")
        };
        let not_null = Step {
            online: Some(OnlineCheck::NotNullSet {
                schema: "public".into(),
                table: "orders".into(),
                column: "id".into(),
            }),
            ..step("o2", Phase::Online, "ALTER TABLE public.orders ALTER COLUMN id SET NOT NULL")
        };
        w.computes.answer_at(&t.endpoint, "'orders_fk'", "done\n");
        w.computes.answer_at(&t.endpoint, "a.attnotnull", "done\n");
        let report = w
            .run_online(&t, &plan(vec![validate, not_null]), &opts, RunMode::Real, PhaseReport::default())
            .await
            .unwrap();
        let outcomes: Vec<StepOutcome> = report.steps.iter().map(|s| s.outcome).collect();
        assert_eq!(outcomes, [StepOutcome::Skipped, StepOutcome::Skipped]);
        assert_eq!(ran(&w, "VALIDATE CONSTRAINT"), 0);
        assert_eq!(ran(&w, "SET NOT NULL"), 0);
    }

    // T14 I3 / M1 (T8a M1): phase 0 skips a role only when its API row is settled and says
    // what the plan creates; a settled row saying something else fails phase 0.
    #[tokio::test]
    async fn phase0_fails_on_a_settled_row_with_other_attributes() {
        let (w, _journal) = worker();
        let (t, opts) = target(&w).await;
        let plain = crate::roles::RoleSpec::login("reporter", false);
        assert_eq!(w.create_role(t.branch, &plain).await, RoleOutcome::Ok);
        let plan = plan(vec![role_step("r1", "reporter", true)]);
        let failure = w
            .run_phases(&t, &plan, &opts, DIGEST, RunMode::Real)
            .await
            .unwrap_err();
        assert_eq!((failure.kind, failure.phase), (FailureKind::Failed, Phase::Roles));
        assert!(failure.error.starts_with("role_exists: "), "{}", failure.error);
    }

    // T14 I3 / M1: a row stored but not applied (`pending`) is driven again (the resync makes
    // the role as stored), never skipped.
    #[tokio::test]
    async fn phase0_drives_a_pending_row_again() {
        let (w, _journal) = worker();
        let (t, opts) = target(&w).await;
        let plan = plan(vec![role_step("r1", "reporter", true)]);
        w.run_phases(&t, &plan, &opts, DIGEST, RunMode::Real).await.unwrap();
        assert_eq!(ran(&w, "CREATE ROLE \"reporter\""), 1);
        w.metadata
            .state()
            .endpoints
            .get_mut(&t.endpoint)
            .unwrap()
            .logins
            .get_mut("reporter")
            .unwrap()
            .attributes_pending = true;

        let again = w.run_phases(&t, &plan, &opts, DIGEST, RunMode::Real).await.unwrap();

        assert_eq!(again.steps[0].outcome, StepOutcome::Ok, "driven, not skipped");
        assert_eq!(ran(&w, "CREATE ROLE \"reporter\""), 2, "the resync made it again");
        assert!(!w.metadata.state().endpoints[&t.endpoint].logins["reporter"].attributes_pending);
    }

    fn rename_step(from: &str, to: &str) -> Step {
        Step {
            role_op: Some(RoleOp::Rename {
                oid: chelabase_schema::Oid(16500),
                from: from.into(),
                to: to.into(),
            }),
            ..step(&format!("role:rename:{from}"), Phase::Roles, "-- rename")
        }
    }

    /// The catalog's answer to the role read for `role`.
    fn in_catalog(w: &FakeWorker, role: &crate::resync::ObservedRole) {
        w.computes.answers.lock().unwrap().retain(|(n, _)| n != "FROM pg_authid");
        w.computes.answer("FROM pg_authid", &crate::resync::role_line(role));
    }

    // E29: phase 0 renames a role through the control plane (its row moves too); a rename an
    // earlier attempt made (the new name there, the old gone) is skipped.
    #[tokio::test]
    async fn phase0_renames_through_the_control_plane() {
        let (w, _journal) = worker();
        let (t, opts) = target(&w).await;
        let app = crate::roles::RoleSpec::login("app", false);
        assert_eq!(w.create_role(t.branch, &app).await, RoleOutcome::Ok);
        in_catalog(&w, &crate::resync::ObservedRole::plain("app"));
        let plan = plan(vec![rename_step("app", "web")]);

        let report = w.run_phases(&t, &plan, &opts, DIGEST, RunMode::Real).await.unwrap();

        assert_eq!(report.steps[0].outcome, StepOutcome::Ok);
        assert_eq!(ran(&w, "ALTER ROLE \"app\" RENAME TO \"web\""), 1);
        let logins = w.metadata.state().endpoints[&t.endpoint].logins.clone();
        assert!(logins.contains_key("web") && !logins.contains_key("app"));

        // Again (a retry): the catalog has `web` and no `app`.
        w.computes
            .answer_at(&t.endpoint, "chelabase: role names", &format!("{}\n", hex::encode("web")));
        let again = w.run_phases(&t, &plan, &opts, DIGEST, RunMode::Real).await.unwrap();
        assert_eq!(again.steps[0].outcome, StepOutcome::Skipped);
        assert_eq!(ran(&w, "ALTER ROLE \"app\" RENAME TO"), 1);
    }

    // E29: a rehearsal renames by SQL only (no row, no keys: the rehearsal has none of its own).
    #[tokio::test]
    async fn the_rehearsal_renames_by_sql() {
        let (w, journal) = worker();
        let (t, opts) = target(&w).await;
        w.computes
            .answer_at(&t.endpoint, "chelabase: role names", &format!("{}\n", hex::encode("app")));
        // A role the owner administers (T14 F3 M5: the rehearsal checks it as the apply does).
        in_catalog(&w, &crate::resync::ObservedRole::plain("app"));
        journal.lock().unwrap().clear();
        let report = w
            .run_phases(&t, &plan(vec![rename_step("app", "web")]), &opts, DIGEST, RunMode::Rehearsal)
            .await
            .unwrap();
        assert_eq!(report.steps[0].outcome, StepOutcome::Ok);
        assert_eq!(ran(&w, "ALTER ROLE \"app\" RENAME TO \"web\""), 1);
        assert!(!entries(&journal).iter().any(|e| e.starts_with("metadata.rename_role_copy")));
    }

    // T14 F3 fix round (M5): the rehearsal's rename refuses what the apply's would (E29,
    // `rename_role`): a reserved name, a role that isn't the user's, a new name that's taken.
    #[tokio::test]
    async fn the_rehearsal_rename_refuses_what_the_apply_would() {
        let (w, _journal) = worker();
        let (t, opts) = target(&w).await;
        w.computes
            .answer_at(&t.endpoint, "chelabase: role names", &format!("{}\n", hex::encode("app")));
        let rehearse = |to: &'static str| {
            let w = &w;
            let (t, opts) = (t.clone(), opts.clone());
            async move {
                w.run_phases(&t, &plan(vec![rename_step("app", to)]), &opts, DIGEST, RunMode::Rehearsal)
                    .await
                    .unwrap_err()
                    .error
            }
        };
        in_catalog(&w, &crate::resync::ObservedRole::plain("app"));
        for to in ["postgres", "chelabase_x", "pg_x"] {
            let error = rehearse(to).await;
            assert!(error.starts_with("reserved_role_name"), "{to}: {error}");
        }
        // The new name is taken in SQL, or by an API row of the target.
        w.computes.answer_once_at(&t.endpoint, "rolname = 'taken'", "1\n");
        let error = rehearse("taken").await;
        assert!(error.starts_with("role_exists"), "{error}");
        let held = crate::roles::RoleSpec::login("held", false);
        assert_eq!(w.create_role(t.branch, &held).await, RoleOutcome::Ok);
        let error = rehearse("held").await;
        assert!(error.starts_with("role_exists"), "{error}");
        // A role made in SQL by another role, with no API row: not the user's.
        let mut theirs = crate::resync::ObservedRole::plain("app");
        theirs.owner_admin = false;
        in_catalog(&w, &theirs);
        let error = rehearse("web").await;
        assert!(error.starts_with("not_found"), "{error}");
        assert_eq!(ran(&w, "ALTER ROLE \"app\" RENAME"), 0);
    }

    // T14 I4 / M3 (T8a M6): the rehearsal refuses what the apply would: a membership in a group
    // that reaches a reserved role, and a change to a role the owner doesn't administer.
    #[tokio::test]
    async fn the_rehearsal_refuses_what_the_apply_would() {
        let (w, _journal) = worker();
        let (t, opts) = target(&w).await;
        let mut create = role_step("r1", "reporter", false);
        if let Some(RoleOp::Create(spec)) = &mut create.role_op {
            spec.member_of = vec!["writers".into()];
        }
        w.computes.answer(
            "MEMBER WITH ADMIN OPTION",
            &format!("{}|t|t\n", hex::encode("writers")),
        );
        let failure = w
            .run_phases(&t, &plan(vec![create]), &opts, DIGEST, RunMode::Rehearsal)
            .await
            .unwrap_err();
        assert!(failure.error.starts_with("role_membership_not_allowed"), "{}", failure.error);
        assert_eq!(ran(&w, "CREATE ROLE \"reporter\""), 0);

        let mut theirs = crate::resync::ObservedRole::plain("theirs");
        theirs.owner_admin = false;
        in_catalog(&w, &theirs);
        let update = Step {
            role_op: Some(RoleOp::Update {
                name: "theirs".into(),
                patch: chelabase_schema::plan::RolePatchModel {
                    connection_limit: Some(3),
                    ..Default::default()
                },
            }),
            ..step("r2", Phase::Roles, "-- update")
        };
        let failure = w
            .run_phases(&t, &plan(vec![update]), &opts, DIGEST, RunMode::Rehearsal)
            .await
            .unwrap_err();
        assert!(failure.error.starts_with("not_found"), "{}", failure.error);
        assert_eq!(ran(&w, "CONNECTION LIMIT 3"), 0);
    }

    // T14 I2: the online steps a retry skips are the current attempt's (after the last move to
    // `applying`), never an earlier attempt's.
    #[tokio::test]
    async fn online_steps_of_an_earlier_attempt_dont_count() {
        let (w, _journal) = worker();
        let (t, opts) = target(&w).await;
        let plan = plan(vec![index_step("o1", "ix_a"), index_step("o2", "ix_b")]);
        let push = |e: MergeEvent| w.metadata.state().merge_events.push(e);
        let mut done = MergeEvent::new(opts.merge_id, "step");
        done.phase = Some("online".into());
        done.step_id = Some("o1".into());
        done.outcome = Some("ok".into());
        push(done.clone());
        let mut start = MergeEvent::new(opts.merge_id, "status");
        start.detail = Some(serde_json::json!({ "to": "applying" }));
        push(start);
        done.step_id = Some("o2".into());
        push(done);

        let report = w
            .run_online(&t, &plan, &opts, RunMode::Real, PhaseReport::default())
            .await
            .unwrap();

        let outcomes: Vec<(&str, StepOutcome)> =
            report.steps.iter().map(|s| (s.id.as_str(), s.outcome)).collect();
        assert_eq!(outcomes, [("o1", StepOutcome::Ok), ("o2", StepOutcome::Skipped)]);
    }

    #[test]
    fn report_serializes_for_the_rehearsal_record() {
        let report = PhaseReport {
            steps: vec![StepRun {
                id: "o1".into(),
                phase: Phase::Online,
                outcome: StepOutcome::Skipped,
                duration_ms: 3,
            }],
        };
        assert_eq!(
            serde_json::to_value(&report).unwrap(),
            serde_json::json!({ "steps": [
                { "id": "o1", "phase": "online", "outcome": "skipped", "duration_ms": 3 }
            ] })
        );
        let _ = Uuid::nil();
    }
}
