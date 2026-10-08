//! Schema merges between a branch and its parent: the merge row (state machine, analysis,
//! plan, rehearsal, result) and its event log.

use chelabase_types::{BranchId, DatabaseId};
use chrono::{DateTime, Utc};
use sqlx::postgres::PgRow;
use sqlx::{AssertSqlSafe, QueryBuilder, Row};
use uuid::Uuid;

use crate::{MetadataClient, MetadataError};

const OPEN_INDEX: &str = "merges_one_open";

macro_rules! columns {
    () => {
        "id, database_id, source_branch_id, target_branch_id, direction, database_name, status, \
         failed_phase, version, base_fingerprint, source_fingerprint, target_fingerprint, \
         analysis, resolutions, user_steps, acknowledgements, options, plan, rehearsal, result, \
         rebaselined, error, claimed_at, created_by, created_at, updated_at"
    };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MergeStatus {
    Computing,
    Draft,
    Rehearsing,
    Rehearsed,
    Applying,
    Online,
    Finishing,
    Done,
    AppliedWithDrift,
    Failed,
    Stale,
    Cancelled,
}

impl MergeStatus {
    pub const ALL: [MergeStatus; 12] = [
        Self::Computing,
        Self::Draft,
        Self::Rehearsing,
        Self::Rehearsed,
        Self::Applying,
        Self::Online,
        Self::Finishing,
        Self::Done,
        Self::AppliedWithDrift,
        Self::Failed,
        Self::Stale,
        Self::Cancelled,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Computing => "computing",
            Self::Draft => "draft",
            Self::Rehearsing => "rehearsing",
            Self::Rehearsed => "rehearsed",
            Self::Applying => "applying",
            Self::Online => "online",
            Self::Finishing => "finishing",
            Self::Done => "done",
            Self::AppliedWithDrift => "applied_with_drift",
            Self::Failed => "failed",
            Self::Stale => "stale",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn parse(value: &str) -> Result<Self, MetadataError> {
        Self::ALL
            .into_iter()
            .find(|s| s.as_str() == value)
            .ok_or_else(|| MetadataError::InvalidStatus(format!("merge status {value}")))
    }

    /// A worker holds the merge (its claim is refreshed while it runs).
    pub fn is_working(self) -> bool {
        matches!(
            self,
            Self::Computing | Self::Rehearsing | Self::Applying | Self::Online | Self::Finishing
        )
    }
}

/// The phase a merge fails in that leaves it half applied: phase 1 committed, phase 2 didn't
/// finish (E27).
pub const ONLINE_PHASE: &str = "online";

/// The phases a merge can fail in while applying (`failed_phase`); Retry takes them (E20).
pub const APPLY_PHASES: [&str; 4] = ["roles", "enum_add", "tx", ONLINE_PHASE];

/// What a user does to a merge (the API's actions; T14 F3's state machine).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeAction {
    /// A draft save that changes what the plan is made from (resolutions, steps, lock timeout):
    /// recomputed.
    Edit,
    /// A draft save of acknowledgements or the after-merge choice only (E34): the plan and a
    /// rehearsal stay.
    EditKeepsPlan,
    Rehearse,
    Apply,
    Recompute,
    Retry,
    Cancel,
    /// E27: give up a merge that failed in the online phase; its leftovers are listed.
    Abandon,
    /// E21: run the after-merge choice a merge applied with drift skipped.
    Finish,
}

/// A merge's place in the state machine: its status and, for `failed`, the phase it failed in
/// (one of [`APPLY_PHASES`], or `None`: failed before applying or for good).
///
/// **The invariant** (T14 F3, tested in full by `the_state_machine_table`): a state is *open*
/// (the pair can't start another merge), *applying* (the target can't take another merge) and
/// *holds its branches* (reset, restore and delete of the source and the target are refused) as
/// [`Self::is_open`], [`Self::is_applying`] and [`Self::holds_branches`] say; applying implies
/// holding, holding implies open; an action moves a merge only as [`Self::after`] says. The SQL
/// predicates [`OPEN_SQL`], [`APPLYING_SQL`] and [`HOLDS_SQL`] say the same.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MergeState {
    pub status: MergeStatus,
    pub failed_phase: Option<&'static str>,
}

/// `merges_one_open`'s predicate: the merge is open.
pub const OPEN_SQL: &str = "NOT (status IN ('done', 'applied_with_drift', 'stale', 'cancelled') \
     OR (status = 'failed' AND failed_phase IS DISTINCT FROM 'online'))";

/// `merges_one_applying`'s predicate: the merge changes (or half changed) its target.
pub const APPLYING_SQL: &str = "(status IN ('applying', 'online', 'finishing') \
     OR (status = 'failed' AND failed_phase = 'online'))";

/// The merge holds its source and target: no reset, restore or delete.
pub const HOLDS_SQL: &str = "(status IN ('rehearsing', 'applying', 'online', 'finishing') \
     OR (status = 'failed' AND failed_phase = 'online'))";

impl MergeState {
    /// The state of a merge in `status` that failed in `failed_phase` (kept only for `failed`,
    /// and only when it's one of [`APPLY_PHASES`]).
    pub fn of(status: MergeStatus, failed_phase: Option<&str>) -> Self {
        let failed_phase = match status {
            MergeStatus::Failed => {
                failed_phase.and_then(|p| APPLY_PHASES.iter().copied().find(|k| *k == p))
            }
            _ => None,
        };
        Self {
            status,
            failed_phase,
        }
    }

    /// Every state: each status, `failed` once per phase and once without one.
    pub fn all() -> Vec<Self> {
        let mut out = Vec::new();
        for status in MergeStatus::ALL {
            if status == MergeStatus::Failed {
                for phase in APPLY_PHASES.iter().copied().map(Some).chain([None]) {
                    out.push(Self {
                        status,
                        failed_phase: phase,
                    });
                }
            } else {
                out.push(Self::of(status, None));
            }
        }
        out
    }

    fn failed_online(self) -> bool {
        self.status == MergeStatus::Failed && self.failed_phase == Some(ONLINE_PHASE)
    }

    /// Not finished: the pair may not start another merge ([`OPEN_SQL`]).
    pub fn is_open(self) -> bool {
        match self.status {
            MergeStatus::Done
            | MergeStatus::AppliedWithDrift
            | MergeStatus::Stale
            | MergeStatus::Cancelled => false,
            MergeStatus::Failed => self.failed_online(),
            _ => true,
        }
    }

    /// Changing the target, or half changed it: no other merge into it ([`APPLYING_SQL`]).
    pub fn is_applying(self) -> bool {
        matches!(
            self.status,
            MergeStatus::Applying | MergeStatus::Online | MergeStatus::Finishing
        ) || self.failed_online()
    }

    /// Reset, restore and delete of the source and the target are refused ([`HOLDS_SQL`]).
    pub fn holds_branches(self) -> bool {
        self.status == MergeStatus::Rehearsing || self.is_applying()
    }

    /// Where `action` takes a merge in this state; `None` when it's refused.
    pub fn after(self, action: MergeAction) -> Option<MergeStatus> {
        use MergeStatus as S;
        match (action, self.status) {
            (MergeAction::Edit, S::Draft | S::Rehearsed) => Some(S::Computing),
            (MergeAction::EditKeepsPlan, s @ (S::Draft | S::Rehearsed)) => Some(s),
            (MergeAction::Rehearse, S::Draft) => Some(S::Rehearsing),
            (MergeAction::Apply, S::Rehearsed) => Some(S::Applying),
            (MergeAction::Recompute, S::Draft | S::Stale) => Some(S::Computing),
            (MergeAction::Recompute, S::Failed) if !self.failed_online() => Some(S::Computing),
            (MergeAction::Retry, S::Failed) => match self.failed_phase {
                Some(ONLINE_PHASE) => Some(S::Online),
                Some(_) => Some(S::Applying),
                None => None,
            },
            (MergeAction::Cancel, S::Computing | S::Draft | S::Rehearsing | S::Rehearsed) => {
                Some(S::Cancelled)
            }
            (MergeAction::Abandon, S::Failed) if self.failed_online() => Some(S::Cancelled),
            (MergeAction::Finish, S::AppliedWithDrift) => Some(S::AppliedWithDrift),
            _ => None,
        }
    }
}

impl MergeRecord {
    /// The merge's [`MergeState`].
    pub fn state(&self) -> MergeState {
        MergeState::of(self.status, self.failed_phase.as_deref())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeDirection {
    IntoParent,
    FromParent,
}

impl MergeDirection {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::IntoParent => "into_parent",
            Self::FromParent => "from_parent",
        }
    }

    pub fn parse(value: &str) -> Result<Self, MetadataError> {
        match value {
            "into_parent" => Ok(Self::IntoParent),
            "from_parent" => Ok(Self::FromParent),
            other => Err(MetadataError::InvalidStatus(format!("merge direction {other}"))),
        }
    }
}

/// Every column of `merges`.
#[derive(Debug, Clone, PartialEq)]
pub struct MergeRecord {
    pub id: Uuid,
    pub database_id: Uuid,
    pub source_branch_id: Uuid,
    pub target_branch_id: Uuid,
    pub direction: MergeDirection,
    pub database_name: String,
    pub status: MergeStatus,
    pub failed_phase: Option<String>,
    /// Bumped by every draft save (and by a worker write of `analysis` or `plan`).
    pub version: i64,
    pub base_fingerprint: Option<String>,
    pub source_fingerprint: Option<String>,
    pub target_fingerprint: Option<String>,
    pub analysis: Option<serde_json::Value>,
    pub resolutions: serde_json::Value,
    pub user_steps: serde_json::Value,
    pub acknowledgements: serde_json::Value,
    pub options: serde_json::Value,
    pub plan: Option<serde_json::Value>,
    pub rehearsal: Option<serde_json::Value>,
    pub result: Option<serde_json::Value>,
    pub rebaselined: bool,
    pub error: Option<String>,
    pub claimed_at: Option<DateTime<Utc>>,
    pub created_by: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// A merge to create (status `computing`).
#[derive(Debug, Clone)]
pub struct NewMerge {
    pub id: Uuid,
    pub database_id: DatabaseId,
    pub source_branch_id: BranchId,
    pub target_branch_id: BranchId,
    pub direction: MergeDirection,
    pub database_name: String,
    /// `None` keeps the column default.
    pub options: Option<serde_json::Value>,
    pub created_by: String,
}

// Short-lived and matched immediately; boxing the record would only touch every caller.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq)]
pub enum MergeCreate {
    Created(MergeRecord),
    /// The pair already has an open merge for this database name.
    OpenExists(Uuid),
    /// The target is being changed by another merge.
    ApplyingExists(Uuid),
}

/// What a user may change while a merge is a draft; `None` leaves a field.
#[derive(Debug, Clone, Default)]
pub struct MergeDraftPatch {
    pub resolutions: Option<serde_json::Value>,
    pub user_steps: Option<serde_json::Value>,
    pub acknowledgements: Option<serde_json::Value>,
    pub options: Option<serde_json::Value>,
    /// Also move the merge to `computing` (claimed now) in the same statement, so the worker
    /// recomputes the analysis and plan from what was saved: a crash between a save and a
    /// separate status change can't leave a stale analysis, and two saves can't both win.
    pub start_compute: bool,
    /// E34: the save changes only what the plan isn't made from (acknowledgements, the
    /// after-merge choice): the status and a rehearsal stay. Ignored with `start_compute`.
    pub keep_plan: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum MergeWrite {
    Saved(MergeRecord),
    /// The version didn't match; the current row.
    Stale(MergeRecord),
    NotFound,
    /// Only `draft` and `rehearsed` merges can be edited.
    NotEditable(MergeStatus),
}

/// Fields a status change writes along with it. `None` leaves a field; for `error` and
/// `failed_phase`, `Some(None)` clears it; for `analysis` and `plan`, `Some(Value::Null)`
/// clears it (SQL `NULL`).
#[derive(Debug, Clone, Default)]
pub struct MergeStatusPatch {
    pub analysis: Option<serde_json::Value>,
    pub plan: Option<serde_json::Value>,
    /// The resolutions a recompute kept (T8a: the ones whose conflict is unchanged).
    pub resolutions: Option<serde_json::Value>,
    pub rehearsal: Option<serde_json::Value>,
    pub result: Option<serde_json::Value>,
    pub error: Option<Option<String>>,
    pub failed_phase: Option<Option<String>>,
    pub base_fingerprint: Option<String>,
    pub source_fingerprint: Option<String>,
    pub target_fingerprint: Option<String>,
    pub rebaselined: Option<bool>,
    /// Replaces the merge's options (the caller merges them with the current ones).
    pub options: Option<serde_json::Value>,
    /// Guard: the row's `failed_phase` must be this (`Some(None)`: NULL), or nothing is
    /// written (the state is status and phase, T14 F3).
    pub guard_failed_phase: Option<Option<String>>,
    /// Guard: the row's `version` must be this (T14 M11: a rehearsal of the draft the user saw).
    pub guard_version: Option<i64>,
    /// Guard: the row's `result` must be this (`Some(None)`: NULL), so of two writers that read
    /// one result only one wins (T14 M8: `finishMerge`'s request).
    pub guard_result: Option<Option<serde_json::Value>>,
    /// Guard: both branches are `ready`, read under a share lock on their rows, so a branch
    /// claim (reset, restore, delete), which checks for a merge holding the branch under its
    /// row lock, can't interleave (T14 M12).
    pub guard_branches_ready: bool,
    /// Also log the change (`status` event, `detail.to`) in the same transaction: the API's
    /// moves to `applying` start an apply attempt (T14 I2).
    pub log_status: bool,
}

/// Why a branch can't be reset, restored or deleted now (T14 F3): a merge holds it (see
/// [`MergeState::holds_branches`]), or a merge's rehearsal branch still hangs off it (a
/// cancelled rehearsal not torn down yet, I4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BranchBusy {
    Merge(Uuid),
    Rehearsal(BranchId),
}

/// The busy check's query: `$1` the branch, `$2` a merge that doesn't count (the one finishing,
/// whose own after-merge choice resets or deletes its source; NULL: none). A merge holding it
/// first, then a live rehearsal child; `(kind, id)`.
const BUSY_SQL: &str = "(SELECT 'merge' AS kind, id FROM merges
                         WHERE (source_branch_id = $1 OR target_branch_id = $1) AND HOLDS
                           AND ($2::uuid IS NULL OR id <> $2)
                         ORDER BY created_at LIMIT 1)
                        UNION ALL
                        (SELECT 'rehearsal', id FROM branches
                         WHERE parent_branch_id = $1 AND kind = 'rehearsal' AND status <> 'deleted'
                         ORDER BY created_at LIMIT 1)
                        LIMIT 1";

/// [`BUSY_SQL`] run on `executor`, leaving out merge `except`. Every path that resets, restores
/// or deletes a branch runs it under the branch's row lock (T14 F3: the API's claim and delete,
/// the worker's delete job and its after-merge reset and delete).
pub(crate) async fn branch_busy<'e, E>(
    executor: E,
    branch: BranchId,
    except: Option<Uuid>,
) -> Result<Option<BranchBusy>, MetadataError>
where
    E: sqlx::PgExecutor<'e>,
{
    let row: Option<(String, Uuid)> =
        sqlx::query_as(AssertSqlSafe(BUSY_SQL.replace("HOLDS", HOLDS_SQL)))
            .bind(branch.as_uuid())
            .bind(except)
            .fetch_optional(executor)
            .await?;
    Ok(row.map(|(kind, id)| match kind.as_str() {
        "merge" => BranchBusy::Merge(id),
        _ => BranchBusy::Rehearsal(BranchId::from(id)),
    }))
}

#[derive(Debug, Clone, PartialEq)]
pub struct MergeEvent {
    /// Assigned on insert (ignored by `append_merge_event`).
    pub id: i64,
    pub merge_id: Uuid,
    /// Assigned on insert (ignored by `append_merge_event`).
    pub at: DateTime<Utc>,
    /// `status`, `step` or `note`.
    pub kind: String,
    pub step_id: Option<String>,
    pub phase: Option<String>,
    /// `ok`, `failed` or `skipped`.
    pub outcome: Option<String>,
    pub duration_ms: Option<i64>,
    pub detail: Option<serde_json::Value>,
}

impl MergeEvent {
    pub fn new(merge_id: Uuid, kind: &str) -> Self {
        Self {
            id: 0,
            merge_id,
            at: Utc::now(),
            kind: kind.to_string(),
            step_id: None,
            phase: None,
            outcome: None,
            duration_ms: None,
            detail: None,
        }
    }
}

fn merge_from_row(r: &PgRow) -> Result<MergeRecord, MetadataError> {
    Ok(MergeRecord {
        id: r.get("id"),
        database_id: r.get("database_id"),
        source_branch_id: r.get("source_branch_id"),
        target_branch_id: r.get("target_branch_id"),
        direction: MergeDirection::parse(r.get("direction"))?,
        database_name: r.get("database_name"),
        status: MergeStatus::parse(r.get("status"))?,
        failed_phase: r.get("failed_phase"),
        version: r.get("version"),
        base_fingerprint: r.get("base_fingerprint"),
        source_fingerprint: r.get("source_fingerprint"),
        target_fingerprint: r.get("target_fingerprint"),
        analysis: r.get("analysis"),
        resolutions: r.get("resolutions"),
        user_steps: r.get("user_steps"),
        acknowledgements: r.get("acknowledgements"),
        options: r.get("options"),
        plan: r.get("plan"),
        rehearsal: r.get("rehearsal"),
        result: r.get("result"),
        rebaselined: r.get("rebaselined"),
        error: r.get("error"),
        claimed_at: r.get("claimed_at"),
        created_by: r.get("created_by"),
        created_at: r.get("created_at"),
        updated_at: r.get("updated_at"),
    })
}

fn event_from_row(r: &PgRow) -> MergeEvent {
    MergeEvent {
        id: r.get("id"),
        merge_id: r.get("merge_id"),
        at: r.get("at"),
        kind: r.get("kind"),
        step_id: r.get("step_id"),
        phase: r.get("phase"),
        outcome: r.get("outcome"),
        duration_ms: r.get("duration_ms"),
        detail: r.get("detail"),
    }
}

impl MetadataClient {
    /// `Created` (status `computing`, claimed now), `ApplyingExists` when another merge is
    /// changing the target, or `OpenExists` when the pair has an open merge for the database.
    pub async fn create_merge(&self, m: &NewMerge) -> Result<MergeCreate, MetadataError> {
        let applying: Option<Uuid> = sqlx::query_scalar(AssertSqlSafe(format!(
            "SELECT id FROM merges WHERE target_branch_id = $1 AND {APPLYING_SQL}"
        )))
        .bind(m.target_branch_id.as_uuid())
        .fetch_optional(&self.pool)
        .await?;
        if let Some(id) = applying {
            return Ok(MergeCreate::ApplyingExists(id));
        }
        let inserted = sqlx::query(concat!(
            "INSERT INTO merges (id, database_id, source_branch_id, target_branch_id, direction,
                                 database_name, status, options, claimed_at, created_by)
             VALUES ($1, $2, $3, $4, $5, $6, 'computing',
                     COALESCE($7, '{\"lock_timeout_ms\":5000,\"after\":\"keep\"}'::jsonb), now(), $8)
             RETURNING ",
            columns!()
        ))
        .bind(m.id)
        .bind(m.database_id.as_uuid())
        .bind(m.source_branch_id.as_uuid())
        .bind(m.target_branch_id.as_uuid())
        .bind(m.direction.as_str())
        .bind(&m.database_name)
        .bind(&m.options)
        .bind(&m.created_by)
        .fetch_one(&self.pool)
        .await;
        match inserted {
            Ok(row) => Ok(MergeCreate::Created(merge_from_row(&row)?)),
            Err(sqlx::Error::Database(e)) if e.constraint() == Some(OPEN_INDEX) => {
                let open: Option<Uuid> = sqlx::query_scalar(AssertSqlSafe(format!(
                    "SELECT id FROM merges
                     WHERE source_branch_id = $1 AND target_branch_id = $2 AND database_name = $3
                       AND {OPEN_SQL}"
                )))
                .bind(m.source_branch_id.as_uuid())
                .bind(m.target_branch_id.as_uuid())
                .bind(&m.database_name)
                .fetch_optional(&self.pool)
                .await?;
                match open {
                    Some(id) => Ok(MergeCreate::OpenExists(id)),
                    // It finished between the insert and the lookup; the caller may retry.
                    None => Err(MetadataError::Conflict),
                }
            }
            Err(err) => Err(err.into()),
        }
    }

    pub async fn merge(&self, id: Uuid) -> Result<Option<MergeRecord>, MetadataError> {
        sqlx::query(concat!("SELECT ", columns!(), " FROM merges WHERE id = $1"))
            .bind(id)
            .fetch_optional(&self.pool)
            .await?
            .as_ref()
            .map(merge_from_row)
            .transpose()
    }

    /// The database's merges, newest first; with `branch`, those where it is source or target.
    pub async fn list_merges(
        &self,
        database_id: DatabaseId,
        branch: Option<BranchId>,
    ) -> Result<Vec<MergeRecord>, MetadataError> {
        sqlx::query(concat!(
            "SELECT ",
            columns!(),
            " FROM merges
              WHERE database_id = $1
                AND ($2::uuid IS NULL OR source_branch_id = $2 OR target_branch_id = $2)
              ORDER BY created_at DESC, id"
        ))
        .bind(database_id.as_uuid())
        .bind(branch.map(|b| b.as_uuid()))
        .fetch_all(&self.pool)
        .await?
        .iter()
        .map(merge_from_row)
        .collect()
    }

    /// Applies `patch` when `expected_version` is current and the merge is a draft or
    /// rehearsed. Saving a rehearsed merge returns it to `draft` and clears its rehearsal
    /// (the rehearsal no longer matches); with `start_compute` it goes to `computing` instead,
    /// in the same statement; with `keep_plan` (E34) the status and rehearsal stay. `Stale`
    /// carries the current row.
    pub async fn update_merge_draft(
        &self,
        id: Uuid,
        expected_version: i64,
        patch: &MergeDraftPatch,
    ) -> Result<MergeWrite, MetadataError> {
        let mut q = QueryBuilder::new("UPDATE merges SET version = version + 1, updated_at = now()");
        if patch.start_compute {
            q.push(
                ", rehearsal = CASE WHEN status = 'rehearsed' THEN NULL ELSE rehearsal END, \
                 status = 'computing', claimed_at = now()",
            );
        } else if !patch.keep_plan {
            q.push(
                ", rehearsal = CASE WHEN status = 'rehearsed' THEN NULL ELSE rehearsal END, \
                 status = CASE WHEN status = 'rehearsed' THEN 'draft' ELSE status END",
            );
        }
        if let Some(v) = &patch.resolutions {
            q.push(", resolutions = ").push_bind(v);
        }
        if let Some(v) = &patch.user_steps {
            q.push(", user_steps = ").push_bind(v);
        }
        if let Some(v) = &patch.acknowledgements {
            q.push(", acknowledgements = ").push_bind(v);
        }
        if let Some(v) = &patch.options {
            q.push(", options = ").push_bind(v);
        }
        q.push(" WHERE id = ").push_bind(id);
        q.push(" AND version = ").push_bind(expected_version);
        q.push(concat!(
            " AND status IN ('draft', 'rehearsed') RETURNING ",
            columns!()
        ));
        if let Some(row) = q.build().fetch_optional(&self.pool).await? {
            return Ok(MergeWrite::Saved(merge_from_row(&row)?));
        }
        Ok(match self.merge(id).await? {
            None => MergeWrite::NotFound,
            Some(current) if !matches!(current.status, MergeStatus::Draft | MergeStatus::Rehearsed) => {
                MergeWrite::NotEditable(current.status)
            }
            Some(current) => MergeWrite::Stale(current),
        })
    }

    /// Moves the merge from one of `from` to `to`, writing `patch` with it; `false` when it
    /// isn't in one of `from` (or doesn't exist), with nothing written. The claim is set
    /// when `to` is a working status and cleared otherwise; `version` is bumped when
    /// `analysis` or `plan` is written. Moving a second merge of one target to `applying`
    /// breaks `merges_one_applying` and is an `Err`: callers check first.
    pub async fn set_merge_status(
        &self,
        id: Uuid,
        from: &[MergeStatus],
        to: MergeStatus,
        patch: MergeStatusPatch,
    ) -> Result<bool, MetadataError> {
        let from: Vec<&str> = from.iter().map(|s| s.as_str()).collect();
        let mut q = QueryBuilder::new("UPDATE merges SET updated_at = now(), status = ");
        q.push_bind(to.as_str());
        q.push(if to.is_working() { ", claimed_at = now()" } else { ", claimed_at = NULL" });
        if patch.analysis.is_some() || patch.plan.is_some() {
            q.push(", version = version + 1");
        }
        match &patch.analysis {
            Some(serde_json::Value::Null) => {
                q.push(", analysis = NULL");
            }
            Some(v) => {
                q.push(", analysis = ").push_bind(v);
            }
            None => {}
        }
        match &patch.plan {
            Some(serde_json::Value::Null) => {
                q.push(", plan = NULL");
            }
            Some(v) => {
                q.push(", plan = ").push_bind(v);
            }
            None => {}
        }
        if let Some(v) = &patch.resolutions {
            q.push(", resolutions = ").push_bind(v);
        }
        if let Some(v) = &patch.rehearsal {
            q.push(", rehearsal = ").push_bind(v);
        }
        if let Some(v) = &patch.result {
            q.push(", result = ").push_bind(v);
        }
        if let Some(v) = &patch.error {
            q.push(", error = ").push_bind(v);
        }
        if let Some(v) = &patch.failed_phase {
            q.push(", failed_phase = ").push_bind(v);
        }
        if let Some(v) = &patch.base_fingerprint {
            q.push(", base_fingerprint = ").push_bind(v);
        }
        if let Some(v) = &patch.source_fingerprint {
            q.push(", source_fingerprint = ").push_bind(v);
        }
        if let Some(v) = &patch.target_fingerprint {
            q.push(", target_fingerprint = ").push_bind(v);
        }
        if let Some(v) = patch.rebaselined {
            q.push(", rebaselined = ").push_bind(v);
        }
        if let Some(v) = &patch.options {
            q.push(", options = ").push_bind(v);
        }
        q.push(" WHERE id = ").push_bind(id);
        q.push(" AND status = ANY(").push_bind(from).push(")");
        if let Some(phase) = &patch.guard_failed_phase {
            q.push(" AND failed_phase IS NOT DISTINCT FROM ").push_bind(phase.clone());
        }
        if let Some(version) = patch.guard_version {
            q.push(" AND version = ").push_bind(version);
        }
        if let Some(result) = &patch.guard_result {
            q.push(" AND result IS NOT DISTINCT FROM ").push_bind(result.clone());
        }
        if !patch.guard_branches_ready && !patch.log_status {
            return Ok(q.build().execute(&self.pool).await?.rows_affected() == 1);
        }
        let mut tx = self.pool.begin().await?;
        if patch.guard_branches_ready {
            // Share locks on both branches (in id order): a claim's row lock waits for this
            // transaction, then sees the merge holding the branch; or this waits for the claim
            // and sees the branch no longer ready.
            let ready: Vec<bool> = sqlx::query_scalar(
                "SELECT b.status = 'ready' FROM branches b
                 WHERE b.id IN (SELECT source_branch_id FROM merges WHERE id = $1
                                UNION SELECT target_branch_id FROM merges WHERE id = $1)
                 ORDER BY b.id FOR SHARE OF b",
            )
            .bind(id)
            .fetch_all(&mut *tx)
            .await?;
            if ready.is_empty() || ready.iter().any(|r| !r) {
                return Ok(false);
            }
        }
        if q.build().execute(&mut *tx).await?.rows_affected() != 1 {
            return Ok(false);
        }
        if patch.log_status {
            sqlx::query(
                "INSERT INTO merge_events (merge_id, kind, detail)
                 VALUES ($1, 'status', jsonb_build_object('to', $2::text))",
            )
            .bind(id)
            .bind(to.as_str())
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(true)
    }

    /// Refreshes the claim heartbeat of a merge still in `status`; `false` when it moved on.
    pub async fn touch_merge_claim(&self, id: Uuid, status: MergeStatus) -> Result<bool, MetadataError> {
        Ok(sqlx::query("UPDATE merges SET claimed_at = now() WHERE id = $1 AND status = $2")
            .bind(id)
            .bind(status.as_str())
            .execute(&self.pool)
            .await?
            .rows_affected()
            == 1)
    }

    /// Merges in a working status whose claim is older than `older_than` (R3), leaving out one
    /// handed back since its last claim less than `rehand_after` ago: its `Resume` may still be
    /// queued behind a long job (T14 M10).
    pub async fn stale_merge_claims(
        &self,
        older_than: std::time::Duration,
        rehand_after: std::time::Duration,
    ) -> Result<Vec<MergeRecord>, MetadataError> {
        sqlx::query(concat!(
            "SELECT ",
            columns!(),
            " FROM merges
              WHERE status IN ('computing', 'rehearsing', 'applying', 'online', 'finishing')
                AND COALESCE(claimed_at, updated_at) < now() - make_interval(secs => $1)
                AND (handed_back_at IS NULL
                     OR handed_back_at < COALESCE(claimed_at, updated_at)
                     OR handed_back_at < now() - make_interval(secs => $2))
              ORDER BY created_at, id"
        ))
        .bind(older_than.as_secs_f64())
        .bind(rehand_after.as_secs_f64())
        .fetch_all(&self.pool)
        .await?
        .iter()
        .map(merge_from_row)
        .collect()
    }

    /// Merges applied with drift whose after-merge choice waits (`result.after.requested`, or
    /// `started` by a run that went away before its outcome, T14 F3 M1) with no write for more
    /// than `older_than`: the `Finish` job was lost (T14 M8). One handed back since its last
    /// write is left out for `rehand_after` (M2: its job may wait behind a long one), as
    /// [`Self::stale_merge_claims`] does.
    pub async fn stale_finish_requests(
        &self,
        older_than: std::time::Duration,
        rehand_after: std::time::Duration,
    ) -> Result<Vec<MergeRecord>, MetadataError> {
        sqlx::query(concat!(
            "SELECT ",
            columns!(),
            " FROM merges
              WHERE status = 'applied_with_drift'
                AND (result -> 'after' -> 'requested' = 'true'::jsonb
                     OR result -> 'after' -> 'started' = 'true'::jsonb)
                AND updated_at < now() - make_interval(secs => $1)
                AND (handed_back_at IS NULL
                     OR handed_back_at < updated_at
                     OR handed_back_at < now() - make_interval(secs => $2))
              ORDER BY created_at, id"
        ))
        .bind(older_than.as_secs_f64())
        .bind(rehand_after.as_secs_f64())
        .fetch_all(&self.pool)
        .await?
        .iter()
        .map(merge_from_row)
        .collect()
    }

    /// Records that reconcile handed the merge back (a job re-published) while it's still in
    /// `status`; `false` when it moved on.
    pub async fn mark_merge_handed_back(&self, id: Uuid, status: MergeStatus) -> Result<bool, MetadataError> {
        Ok(sqlx::query("UPDATE merges SET handed_back_at = now() WHERE id = $1 AND status = $2")
            .bind(id)
            .bind(status.as_str())
            .execute(&self.pool)
            .await?
            .rows_affected()
            == 1)
    }

    /// Appends to the merge's log (`id` and `at` of `e` are assigned by the database).
    pub async fn append_merge_event(&self, e: &MergeEvent) -> Result<(), MetadataError> {
        sqlx::query(
            "INSERT INTO merge_events (merge_id, kind, step_id, phase, outcome, duration_ms, detail)
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(e.merge_id)
        .bind(&e.kind)
        .bind(&e.step_id)
        .bind(&e.phase)
        .bind(&e.outcome)
        .bind(e.duration_ms)
        .bind(&e.detail)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn merge_events(&self, id: Uuid) -> Result<Vec<MergeEvent>, MetadataError> {
        Ok(sqlx::query("SELECT * FROM merge_events WHERE merge_id = $1 ORDER BY id")
            .bind(id)
            .fetch_all(&self.pool)
            .await?
            .iter()
            .map(event_from_row)
            .collect())
    }

    /// Cancels the branch's merges that nothing is running (`computing`, `draft`,
    /// `rehearsed`), as source or target; returns how many. Rehearsing and applying merges
    /// are left alone.
    pub async fn cancel_drafts_for_branch(&self, branch: BranchId) -> Result<u64, MetadataError> {
        Ok(sqlx::query(
            "UPDATE merges SET status = 'cancelled', claimed_at = NULL, updated_at = now()
             WHERE (source_branch_id = $1 OR target_branch_id = $1)
               AND status IN ('computing', 'draft', 'rehearsed')",
        )
        .bind(branch.as_uuid())
        .execute(&self.pool)
        .await?
        .rows_affected())
    }

    /// What keeps the branch from being reset, restored or deleted: a merge holding it as
    /// source or target ([`MergeState::holds_branches`]: rehearsing, applying, or failed in the
    /// online phase), or a live rehearsal branch of it (I4). The API's claim checks the same under
    /// the branch's row lock ([`MetadataClient::claim_branch_for_switch`]).
    pub async fn branch_has_busy_merge(&self, branch: BranchId) -> Result<Option<BranchBusy>, MetadataError> {
        branch_busy(&self.pool, branch, None).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_status_round_trips() {
        for status in MergeStatus::ALL {
            assert_eq!(MergeStatus::parse(status.as_str()).unwrap(), status);
        }
        assert_eq!(MergeStatus::ALL.len(), 12);
        assert!(MergeStatus::parse("nope").is_err());
        assert_eq!(MergeDirection::parse("into_parent").unwrap(), MergeDirection::IntoParent);
        assert_eq!(MergeDirection::FromParent.as_str(), "from_parent");
    }

    /// The state machine's invariant (T14 F3, E27), written out in full: for every state, what it
    /// holds (`open`: the pair can't start another merge; `applying`: the target can't take another
    /// merge; `holds`: reset, restore and delete of either branch are refused) and where each
    /// action takes it (`-`: refused).
    #[test]
    fn the_state_machine_table() {
        use MergeAction::*;
        use MergeStatus::*;
        const ACTIONS: [MergeAction; 9] =
            [Edit, EditKeepsPlan, Rehearse, Apply, Recompute, Retry, Cancel, Abandon, Finish];
        let failed = |phase: Option<&'static str>| MergeState { status: Failed, failed_phase: phase };
        let at = |status| MergeState { status, failed_phase: None };
        #[rustfmt::skip]
        type Row = (MergeState, (bool, bool, bool), [Option<MergeStatus>; 9]);
        let table: Vec<Row> = vec![
            //                         open   applying holds    Edit             EditKeepsPlan    Rehearse          Apply           Recompute        Retry           Cancel           Abandon          Finish
            (at(Computing),        (true,  false, false), [None,            None,            None,             None,           None,            None,           Some(Cancelled), None,            None]),
            (at(Draft),            (true,  false, false), [Some(Computing), Some(Draft),     Some(Rehearsing), None,           Some(Computing), None,           Some(Cancelled), None,            None]),
            (at(Rehearsing),       (true,  false, true),  [None,            None,            None,             None,           None,            None,           Some(Cancelled), None,            None]),
            (at(Rehearsed),        (true,  false, false), [Some(Computing), Some(Rehearsed), None,             Some(Applying), None,            None,           Some(Cancelled), None,            None]),
            (at(Applying),         (true,  true,  true),  [None,            None,            None,             None,           None,            None,           None,            None,            None]),
            (at(Online),           (true,  true,  true),  [None,            None,            None,             None,           None,            None,           None,            None,            None]),
            (at(Finishing),        (true,  true,  true),  [None,            None,            None,             None,           None,            None,           None,            None,            None]),
            (at(Done),             (false, false, false), [None,            None,            None,             None,           None,            None,           None,            None,            None]),
            (at(AppliedWithDrift), (false, false, false), [None,            None,            None,             None,           None,            None,           None,            None,            Some(AppliedWithDrift)]),
            (failed(Some("roles")),    (false, false, false), [None,        None,            None,             None,           Some(Computing), Some(Applying), None,            None,            None]),
            (failed(Some("enum_add")), (false, false, false), [None,        None,            None,             None,           Some(Computing), Some(Applying), None,            None,            None]),
            (failed(Some("tx")),       (false, false, false), [None,        None,            None,             None,           Some(Computing), Some(Applying), None,            None,            None]),
            (failed(Some("online")),   (true,  true,  true),  [None,        None,            None,             None,           None,            Some(Online),   None,            Some(Cancelled), None]),
            (failed(None),             (false, false, false), [None,        None,            None,             None,           Some(Computing), None,           None,            None,            None]),
            (at(Stale),            (false, false, false), [None,            None,            None,             None,           Some(Computing), None,           None,            None,            None]),
            (at(Cancelled),        (false, false, false), [None,            None,            None,             None,           None,            None,           None,            None,            None]),
        ];
        assert_eq!(
            table.iter().map(|(s, _, _)| *s).collect::<Vec<_>>(),
            MergeState::all(),
            "the table covers every state once"
        );
        for (state, (open, applying, holds), after) in &table {
            assert_eq!(state.is_open(), *open, "{state:?} open");
            assert_eq!(state.is_applying(), *applying, "{state:?} applying");
            assert_eq!(state.holds_branches(), *holds, "{state:?} holds");
            for (action, to) in ACTIONS.iter().zip(after) {
                assert_eq!(state.after(*action), *to, "{state:?} {action:?}");
            }
            // What applies to a target holds it; what holds is open.
            assert!(!state.is_applying() || state.holds_branches(), "{state:?}");
            assert!(!state.holds_branches() || state.is_open(), "{state:?}");
        }
    }

    /// A failed phase other than the four known ones reads as none (not retryable, not holding).
    #[test]
    fn merge_state_of_a_record() {
        assert_eq!(MergeState::of(MergeStatus::Failed, Some("online")).failed_phase, Some("online"));
        assert_eq!(MergeState::of(MergeStatus::Failed, Some("phase1")).failed_phase, None);
        assert_eq!(MergeState::of(MergeStatus::Done, Some("online")).failed_phase, None);
    }

    /// The SQL the unique indexes and the busy check use says what the table says (the live
    /// test `merge_state_sql_matches_the_table` runs it against every state).
    #[test]
    fn the_migration_uses_the_state_predicates() {
        let normalize = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
        let migration = normalize(include_str!("../migrations/20261013000001_merge_failed_online.sql"));
        assert!(migration.contains(&normalize(&format!("WHERE {OPEN_SQL}"))), "{migration}");
        assert!(migration.contains(&normalize(&format!("WHERE {APPLYING_SQL}"))), "{migration}");
    }
}
