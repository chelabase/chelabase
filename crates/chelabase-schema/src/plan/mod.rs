//! The planner (T4): the merged model as ordered, phased steps, and the phase 1 script.
//!
//! [`plan`] diffs the target against the merged model ([`diff`]: matching, order, phase
//! transforms), adds own SQL and user steps (classified with pg_query, [`crate::steps`]),
//! moves enum values the plan uses into phase 1a, and links each step to the analysis items it
//! implements. Execution (T8): phase 0 through the roles path ([`Step::role_op`]); phase 1a and
//! phase 2 one [`step_script`] each; phase 1 as one [`phase1_script`], after
//! [`marker_setup_sql`] on the admin session. Every script runs in the owner's own session
//! (E15: psql `-U` [`OWNER_ROLE`]). Scripts go to psql on stdin (never `-c`, which wraps
//! several statements in one transaction).

pub(crate) mod ddl;
mod digest;
mod diff;

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Deserializer, Serialize};
use uuid::Uuid;

use crate::analysis::{Analysis, ItemId, MergedModel, Outcome};
use crate::error::SchemaError;
use crate::model::{Oid, SchemaModel};
use crate::sql::literal;
use crate::steps::{StepError, UserStep, classify_steps};

pub use digest::{DIGEST_PRELUDE, TARGET_DIGEST_SQL, admin_digest_sql};
pub use ddl::OWNER_ROLE;
pub(crate) use ddl::{grantee_name, rename_identifiers};
pub(crate) use diff::default_grants;

/// A step's stable id.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct StepId(pub String);

/// Phases 0, 1a, 1 and 2.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// 0: roles, through the control plane.
    Roles,
    /// 1a: enum values used later in the plan, in their own committed transaction.
    EnumAdd,
    /// 1: one transaction.
    Tx,
    /// 2: online, one statement at a time, resumable.
    Online,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepKind {
    Generated,
    User,
    OwnSql,
}

/// A role to create through the control plane (E5: T9's `RoleSpec`). `valid_until` is RFC 3339
/// (`2030-01-01T00:00:00+00:00`), converted from the catalog's `timestamptz` text; `member_of`
/// leaves out the owner-level group (that's `owner_level`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleSpecModel {
    pub name: String,
    pub owner_level: bool,
    pub login: bool,
    pub inherit: bool,
    pub connection_limit: i32,
    pub valid_until: Option<String>,
    pub bypass_rls: bool,
    pub member_of: Vec<String>,
}

/// Attribute changes to an existing role (E5: T9's `RolePatch`); `None` = unchanged.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RolePatchModel {
    pub owner_level: Option<bool>,
    pub login: Option<bool>,
    pub inherit: Option<bool>,
    pub connection_limit: Option<i32>,
    /// `Some(None)` clears the expiry; absent = unchanged.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "some_option"
    )]
    pub valid_until: Option<Option<String>>,
    pub bypass_rls: Option<bool>,
    pub member_of: Option<Vec<String>>,
}

/// A present field (even `null`) is `Some`; an absent one stays `None` through `default`.
fn some_option<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Option<String>>, D::Error> {
    Option::<String>::deserialize(d).map(Some)
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoleOp {
    Create(RoleSpecModel),
    Update { name: String, patch: RolePatchModel },
    /// E29: the source renamed a role the target has (the same role by OID). Run through the
    /// control plane, which moves the role's stored row, proxy copy and secret keys with it.
    /// Every rename comes first in phase 0, in an order where each new name is free; the
    /// other role ops of the plan name the role by `to`.
    Rename { oid: Oid, from: String, to: String },
}

/// A step that breaks something outside the database, so it needs an acknowledgement like a
/// drop (E29a).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Breaking {
    /// A role rename: clients connecting as `role` fail to authenticate after the apply. The
    /// worker and Studio look up the role's live logins and secret keys by `role` on the
    /// target.
    RoleRename { role: String, new_name: String },
}

impl Breaking {
    /// The flag's text for the plan's reader.
    pub fn message(&self) -> String {
        match self {
            Breaking::RoleRename { role, new_name } => format!(
                "renames role {role} to {new_name}: clients connecting to the target as {role} \
                 will fail to authenticate after the apply"
            ),
        }
    }
}

/// What a phase 2 resume checks before re-running a step: when the check holds, the step is
/// done and is skipped. A step without a check is idempotent (`IF EXISTS` / `IF NOT EXISTS`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnlineCheck {
    /// The index exists on `schema.table` (its `indrelid`) and is valid. An invalid one is
    /// dropped concurrently and rebuilt. An index of that name on another table means the step
    /// isn't done (and its `CREATE INDEX` fails on the name, which the rehearsal catches).
    IndexValid {
        schema: String,
        table: String,
        name: String,
    },
    /// The constraint exists and is validated.
    ConstraintValidated {
        schema: String,
        table: String,
        name: String,
    },
    /// The constraint exists (validated or not): a foreign key added `NOT VALID` in phase 2,
    /// because the unique index it needs is only built there.
    ConstraintExists {
        schema: String,
        table: String,
        name: String,
    },
    /// The column is `NOT NULL`.
    NotNullSet {
        schema: String,
        table: String,
        column: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Step {
    pub id: StepId,
    pub kind: StepKind,
    pub phase: Phase,
    pub sql: String,
    pub items: Vec<ItemId>,
    pub data_loss: bool,
    pub rewrite: bool,
    pub security: bool,
    pub locks: Vec<String>,
    pub role_op: Option<RoleOp>,
    pub online: Option<OnlineCheck>,
    /// E29a: what the step breaks outside the database (a role rename).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub breaking: Option<Breaking>,
}

impl Step {
    /// Whether applying the step needs the user's acknowledgement (by the step's items, or its
    /// id when it has none): data loss, a security change (BYPASSRLS, owner-level; P9, E28),
    /// or a breaking change (E29a).
    pub fn needs_acknowledgement(&self) -> bool {
        self.data_loss || self.security || self.breaking.is_some()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    pub steps: Vec<Step>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanOptions {
    pub lock_timeout_ms: u32,
    pub merge_id: Uuid,
}

/// R2: phase 1's `lock_timeout` when the merge's options don't say.
pub const DEFAULT_LOCK_TIMEOUT_MS: u32 = 5000;

/// Phase 1's `statement_timeout` (P3), in the script and the `.sql` export.
pub const PHASE1_STATEMENT_TIMEOUT: &str = "15min";

/// A merge's `options.lock_timeout_ms`, or [`DEFAULT_LOCK_TIMEOUT_MS`].
pub fn lock_timeout_ms(options: &serde_json::Value) -> u32 {
    options
        .get("lock_timeout_ms")
        .and_then(serde_json::Value::as_u64)
        .and_then(|ms| u32::try_from(ms).ok())
        .unwrap_or(DEFAULT_LOCK_TIMEOUT_MS)
}

/// The error phase 1's guard raises when the target changed since the rehearsal.
pub const STALE_ERROR: &str = "chelabase_merge_stale";

/// The ordered, phased plan from the target to the merged model.
///
/// Own SQL (`merged.own_sql`) goes at the place of its conflict's object kind; user steps go
/// right after their anchor (`None`: first), one step per statement (ids `<id>.<n>` when a
/// step has several), each in the phase its statement needs: a concurrent statement anchored
/// among phase 1 steps runs in phase 2 at the same relative position. A statement whose phase
/// comes before what it follows (a phase 1 statement anchored after a phase 2 step, or after a
/// concurrent statement of its own step) would run before it and is refused. Refused
/// statements, unknown anchors and step ids the plan reserves (T14 minor 3: `:`, `.`, control
/// characters, duplicates) fail the plan with [`SchemaError::Steps`].
///
/// **Dependency gaps the rehearsal catches (accepted, T4):** the planner doesn't follow every
/// dependency Postgres enforces; these fail the rehearsal (and so never reach the target) and
/// are resolved with own SQL:
/// - a unique constraint or index that changes while an unchanged foreign key elsewhere uses it
///   (dropping it fails on the dependent key);
/// - a target index left INVALID that the merged model keeps as it is (the plan doesn't rebuild
///   it; the drift check reads it back as the target had it);
/// - a column a dropped or retyped object's view, rule or other reported object depends on
///   (reported objects aren't modelled, so their dependencies aren't seen).
///
/// A generated-column conversion under policies, indexes, constraints and column grants, once
/// on this list, is planned (T14 minor 13).
pub fn plan(
    target: &SchemaModel,
    merged: &MergedModel,
    analysis: &Analysis,
    user_steps: &[UserStep],
    opts: &PlanOptions,
) -> Result<Plan, SchemaError> {
    let mut outs = diff::generate(target, &merged.model, analysis, opts.merge_id)?;
    let mut errors: Vec<StepError> = Vec::new();

    for (conflict, sql) in &merged.own_sql {
        let (bucket, items) = diff::bucket_for_conflict(analysis, conflict);
        match classify_steps(sql) {
            Ok(stmts) => {
                let many = stmts.len() > 1;
                for (k, s) in stmts.into_iter().enumerate() {
                    let id = if many {
                        format!("own:{}.{}", conflict.0, k + 1)
                    } else {
                        format!("own:{}", conflict.0)
                    };
                    let mut o = diff::Out::new(bucket, s.phase, StepKind::OwnSql, id, s.sql);
                    o.items = items.clone();
                    o.online = s.online;
                    outs.push(o);
                }
            }
            Err(mut e) => {
                e.reason = format!("own SQL for conflict {}: {}", conflict.0, e.reason);
                errors.push(e);
            }
        }
    }
    // Stable: within a bucket, the generation order (own SQL last).
    outs.sort_by_key(|o| o.bucket);
    // T14 minor 12: the enum values the plan's own statements use move to phase 1a before user
    // steps are placed, so a user statement is placed against the final phases.
    move_used_enum_adds(&mut outs)?;

    // T14 minor 3: a user step's id is never a generated or split id.
    let id_errors = check_user_ids(user_steps);
    if !id_errors.is_empty() {
        return Err(SchemaError::Steps(id_errors));
    }

    for us in user_steps {
        let stmts = match classify_steps(&us.sql) {
            Ok(s) => s,
            Err(mut e) => {
                e.reason = format!("step {}: {}", us.id.0, e.reason);
                errors.push(e);
                continue;
            }
        };
        let anchor = us.after.as_ref().map(|a| a.0.clone());
        let start = match &anchor {
            None => 0,
            Some(a) => match outs.iter().rposition(|o| {
                o.id == *a || o.user.as_ref().is_some_and(|u| u.step == *a)
            }) {
                Some(i) => i + 1,
                None => {
                    errors.push(StepError {
                        index: 0,
                        sql: us.sql.trim().to_string(),
                        reason: format!("step {}: anchor {a} isn't in the plan", us.id.0),
                    });
                    continue;
                }
            },
        };
        // After the steps already inserted at the same anchor, in their given order.
        let mut at = start;
        while outs
            .get(at)
            .and_then(|o| o.user.as_ref())
            .is_some_and(|u| u.anchor == anchor)
        {
            at += 1;
        }
        // Phases run in order (0, 1a, 1, 2), so a statement can't run in an earlier phase than
        // what it follows (its anchor, the user steps already placed at the same anchor, or
        // the step's previous statement): it would run before it. Refused, rather than moved
        // into the later phase (a phase 1 statement run as a phase 2 step would lose phase 1's
        // transaction and be re-run on a resume).
        // `start - 1` is the anchor; with no anchor (`start` 0) the slice holds only the user
        // steps already placed first.
        let mut floor = outs[start.saturating_sub(1)..at]
            .iter()
            .map(|o| o.phase)
            .max()
            .unwrap_or(Phase::Roles);
        let late: Vec<StepError> = stmts
            .iter()
            .enumerate()
            .filter_map(|(index, s)| {
                if s.phase < floor {
                    return Some(StepError {
                        index,
                        sql: s.sql.clone(),
                        reason: format!(
                            "step {}: this statement runs in {} but follows a {} statement, so it would run before it; place it before the {} statements",
                            us.id.0,
                            phase_label(s.phase),
                            phase_label(floor),
                            phase_label(floor)
                        ),
                    });
                }
                floor = floor.max(s.phase);
                None
            })
            .collect();
        if !late.is_empty() {
            errors.extend(late);
            continue;
        }
        let many = stmts.len() > 1;
        for (k, s) in stmts.into_iter().enumerate() {
            let id = if many {
                format!("{}.{}", us.id.0, k + 1)
            } else {
                us.id.0.clone()
            };
            let mut o = diff::Out::new(diff::Bucket::Grant, s.phase, StepKind::User, id, s.sql);
            o.online = s.online;
            o.user = Some(diff::UserOrigin {
                step: us.id.0.clone(),
                anchor: anchor.clone(),
            });
            outs.insert(at, o);
            at += 1;
        }
    }
    if !errors.is_empty() {
        return Err(SchemaError::Steps(errors));
    }

    // The values user statements use, too.
    move_used_enum_adds(&mut outs)?;

    // Items: by the planner keys their snapshots name (changes that apply or conflict).
    let mut by_key: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, item) in analysis.items.iter().enumerate() {
        if !matches!(item.outcome, Outcome::Apply | Outcome::Conflict(_)) {
            continue;
        }
        for change in [&item.source, &item.target].into_iter().flatten() {
            for snap in diff::change_snapshots(change) {
                for k in diff::snapshot_keys(snap) {
                    let v = by_key.entry(k).or_default();
                    if !v.contains(&i) {
                        v.push(i);
                    }
                }
            }
        }
    }
    let mut steps: Vec<Step> = outs
        .into_iter()
        .map(|o| {
            let mut linked: Vec<usize> = o
                .keys
                .iter()
                .filter_map(|k| by_key.get(k))
                .flatten()
                .copied()
                .collect();
            linked.sort_unstable();
            linked.dedup();
            let mut items: Vec<ItemId> = linked.iter().map(|&i| analysis.items[i].id.clone()).collect();
            for id in &o.items {
                if !items.contains(id) {
                    items.push(id.clone());
                }
            }
            let item_loss = linked.iter().any(|&i| analysis.items[i].data_loss);
            let item_security = linked.iter().any(|&i| analysis.items[i].security);
            Step {
                id: StepId(o.id),
                kind: o.kind,
                phase: o.phase,
                sql: o.sql,
                items,
                data_loss: o.drop || (o.type_change && (linked.is_empty() || item_loss)),
                rewrite: o.rewrite,
                security: o.security || (o.role_op.is_some() && item_security),
                locks: o.locks,
                role_op: o.role_op,
                online: o.online,
                breaking: o.breaking,
            }
        })
        .collect();
    steps.sort_by_key(|s| s.phase);
    Ok(Plan { steps })
}

/// Whether `sql` names the enum value `label` as a string literal, in either form: the planner's
/// ([`literal`]: `E'a\\b'` for a backslash) or Postgres's own (`pg_get_expr`: standard-conforming,
/// quotes doubled). Conservative: a longer literal ending the same way also counts (it only moves
/// a value to phase 1a needlessly).
fn uses_label(sql: &str, label: &str) -> bool {
    sql.contains(&literal(label)) || sql.contains(&format!("'{}'", label.replace('\'', "''")))
}

/// Moves to phase 1a every enum value a phase 1 statement uses (all of an enum's added values
/// together, so a value placed next to another finds it). Idempotent. A value that is also a
/// target value the plan renames away can't go there (T14 minor 11: phase 1a runs before the
/// rename, and `ADD VALUE IF NOT EXISTS` would find the old value): unplannable.
fn move_used_enum_adds(outs: &mut [diff::Out]) -> Result<(), SchemaError> {
    let used: HashSet<String> = outs
        .iter()
        .filter_map(|o| o.enum_add.as_ref())
        .filter(|e| {
            outs.iter().any(|o| {
                o.phase == Phase::Tx
                    && o.enum_add.is_none()
                    // A value rename names values; it doesn't use one.
                    && !(o.kind == StepKind::Generated && o.id.starts_with("rename-enum-value:"))
                    && uses_label(&o.sql, &e.label)
            })
        })
        .map(|e| e.group.clone())
        .collect();
    if let Some(e) = outs
        .iter()
        .filter_map(|o| o.enum_add.as_ref())
        .find(|e| used.contains(&e.group) && e.renamed_away)
    {
        return Err(SchemaError::Unplannable {
            object: e.group.replace('\u{1f}', "."),
            reason: format!(
                "the plan adds the value {} while it renames the target's value of that name, \
                 and phase 1 uses it; merge this enum with own SQL",
                literal(&e.label)
            ),
        });
    }
    for o in outs.iter_mut() {
        if let Some(e) = &o.enum_add
            && used.contains(&e.group)
        {
            o.phase = Phase::EnumAdd;
            o.sql = e.sql_1a.clone();
        }
    }
    Ok(())
}

/// T14 minor 3: user step ids the plan refuses: empty, holding `:` or `.` (generated ids hold
/// `:`, the statements of a split step are `<id>.<n>`, the script's own marks are
/// `chelabase:…`), holding a control character, or used twice.
fn check_user_ids(user_steps: &[UserStep]) -> Vec<StepError> {
    let mut errors = Vec::new();
    let mut seen: HashSet<&str> = HashSet::new();
    for us in user_steps {
        let id = us.id.0.as_str();
        let reason = if id.is_empty() || id.chars().any(|c| c == ':' || c == '.' || c.is_control())
        {
            Some(format!(
                "step id {id:?} can't be empty or hold `:`, `.` or control characters (the \
                 plan's own ids use them)"
            ))
        } else if id == PHASE1_STEP_ID {
            Some(format!(
                "step id {id:?} is reserved (the merge's log names the plan's transaction so)"
            ))
        } else if !seen.insert(id) {
            Some(format!("step id {id:?} is used by another step"))
        } else {
            None
        };
        if let Some(reason) = reason {
            errors.push(StepError {
                index: 0,
                sql: us.sql.trim().to_string(),
                reason,
            });
        }
    }
    errors
}

/// An index's `CREATE INDEX` on `table_q` (a qualified table name), as the planner writes it.
pub fn index_definition(ix: &crate::model::Index, table_q: &str) -> String {
    ddl::create_index(ix, &ix.name, table_q, false)
}

/// A phase's name in messages.
fn phase_label(p: Phase) -> &'static str {
    match p {
        Phase::Roles => "phase 0",
        Phase::EnumAdd => "phase 1a",
        Phase::Tx => "phase 1",
        Phase::Online => "phase 2",
    }
}

/// A statement for a script: `;`-terminated, on its own line when it ends in a line comment.
fn terminated(sql: &str) -> String {
    let sql = sql.trim_end();
    if sql.lines().last().is_some_and(|l| l.contains("--")) {
        format!("{sql}\n;")
    } else {
        format!("{sql};")
    }
}

/// Phase 1's script: `BEGIN; SET LOCAL lock_timeout …; SET LOCAL statement_timeout '15min';`, a
/// guard that raises [`STALE_ERROR`] when [`TARGET_DIGEST_SQL`] doesn't give
/// `expected_digest`, the phase 1 steps (each after a `-- step <id>` line), `-- step
/// chelabase:marker` and the marker insert ([`marker_sql`]), `-- step chelabase:commit` and
/// `COMMIT`. [`phase1`] gives the same text with the line each part starts at.
///
/// E15: it runs in the owner's own session (psql `-U` [`OWNER_ROLE`]), never as a superuser
/// with a role switch, so a user step can't reach more than the owner can. The marker table
/// must exist first ([`marker_setup_sql`], on the admin session). Run it with
/// `ON_ERROR_STOP`: any error rolls everything back.
pub fn phase1_script(plan: &Plan, opts: &PlanOptions, expected_digest: &str) -> String {
    phase1(plan, opts, expected_digest).text
}

/// The id of the `-- step` line before the marker insert (T14 minor 2).
pub const MARKER_STEP_ID: &str = "chelabase:marker";

/// The id of the `-- step` line before `COMMIT` (T14 minor 2).
pub const COMMIT_STEP_ID: &str = "chelabase:commit";

/// Phase 1's entry in a merge's log and step reports: the whole transaction is one step. A user
/// step can't take this id (T14 F3 fix round, M8).
pub const PHASE1_STEP_ID: &str = "phase1";

/// What a line of the phase 1 script belongs to.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScriptPart {
    /// `BEGIN` and the `SET LOCAL` timeouts.
    Prelude,
    /// The stale guard (a `DO` block raising [`STALE_ERROR`]).
    Guard,
    /// A phase 1 step: its `-- step <id>` line and its statement.
    Step(StepId),
    /// The recovery marker insert ([`MARKER_STEP_ID`]).
    Marker,
    /// `COMMIT` ([`COMMIT_STEP_ID`]): a deferred constraint fails here.
    Commit,
}

/// Where a part of the script starts.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScriptRegion {
    /// 1-based, as psql numbers its input lines (`psql:<stdin>:<line>:`).
    pub first_line: usize,
    pub part: ScriptPart,
}

/// Phase 1's script and where its generator put each part (T14 minor 2): a failure is mapped to
/// a part by psql's line number ([`Phase1Script::at_line`]), never by scanning the text, which
/// holds user SQL (a user's `-- step x` line can't steer the mapping).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Phase1Script {
    pub text: String,
    /// In order, starting at line 1; each part runs until the next one starts.
    pub parts: Vec<ScriptRegion>,
}

impl Phase1Script {
    /// The part holding 1-based `line`; `None` for 0 or a line past the script's end.
    pub fn at_line(&self, line: usize) -> Option<&ScriptPart> {
        if line == 0 || line > self.text.lines().count() {
            return None;
        }
        self.parts
            .iter()
            .rev()
            .find(|r| r.first_line <= line)
            .map(|r| &r.part)
    }
}

/// [`phase1_script`] with its parts' line numbers.
pub fn phase1(plan: &Plan, opts: &PlanOptions, expected_digest: &str) -> Phase1Script {
    let mut text = String::new();
    let mut parts = Vec::new();
    let mut add = |text: &mut String, part: ScriptPart, chunk: &str| {
        parts.push(ScriptRegion {
            first_line: text.matches('\n').count() + 1,
            part,
        });
        text.push_str(chunk);
    };
    add(
        &mut text,
        ScriptPart::Prelude,
        &format!(
            "BEGIN;\n\
             SET LOCAL lock_timeout = '{lock}ms';\n\
             SET LOCAL statement_timeout = '{PHASE1_STATEMENT_TIMEOUT}';\n",
            lock = opts.lock_timeout_ms,
        ),
    );
    add(
        &mut text,
        ScriptPart::Guard,
        &format!(
            "DO $chelabase_guard$\n\
             BEGIN\n\
             IF ({TARGET_DIGEST_SQL}) IS DISTINCT FROM {expected} THEN\n\
             RAISE EXCEPTION '{STALE_ERROR}';\n\
             END IF;\n\
             END\n\
             $chelabase_guard$;\n",
            expected = literal(expected_digest),
        ),
    );
    for step in plan.steps.iter().filter(|s| s.phase == Phase::Tx) {
        add(
            &mut text,
            ScriptPart::Step(step.id.clone()),
            &format!("-- step {}\n{}\n", one_line(&step.id.0), terminated(&step.sql)),
        );
    }
    add(
        &mut text,
        ScriptPart::Marker,
        &format!("-- step {MARKER_STEP_ID}\n{}\n", marker_sql(opts.merge_id)),
    );
    add(
        &mut text,
        ScriptPart::Commit,
        &format!("-- step {COMMIT_STEP_ID}\nCOMMIT;\n"),
    );
    Phase1Script { text, parts }
}

/// A step id on one line (ids of user steps are the user's).
fn one_line(id: &str) -> String {
    id.replace(['\n', '\r'], " ")
}

/// [`marker_setup_sql`]'s trust check: raises unless any existing `chelabase_merge` schema is
/// owned by `session_user` and any existing `applied` is a plain table owned by it, with no
/// triggers or rules.
const MARKER_CHECK: &str = "DO $chelabase_marker$\n\
DECLARE\n\
nsp pg_catalog.oid;\n\
rel pg_catalog.oid;\n\
BEGIN\n\
SELECT n.oid INTO nsp FROM pg_catalog.pg_namespace n WHERE n.nspname = 'chelabase_merge';\n\
IF nsp IS NULL THEN\n\
RETURN;\n\
END IF;\n\
IF (SELECT pg_catalog.pg_get_userbyid(n.nspowner) FROM pg_catalog.pg_namespace n WHERE n.oid = nsp) IS DISTINCT FROM session_user THEN\n\
RAISE EXCEPTION 'chelabase_merge_marker_untrusted: schema chelabase_merge is not owned by %', session_user;\n\
END IF;\n\
SELECT c.oid INTO rel FROM pg_catalog.pg_class c WHERE c.relnamespace = nsp AND c.relname = 'applied';\n\
IF rel IS NULL THEN\n\
RETURN;\n\
END IF;\n\
IF EXISTS (SELECT 1 FROM pg_catalog.pg_class c WHERE c.oid = rel AND (c.relkind <> 'r' OR pg_catalog.pg_get_userbyid(c.relowner) IS DISTINCT FROM session_user)) THEN\n\
RAISE EXCEPTION 'chelabase_merge_marker_untrusted: chelabase_merge.applied is not a plain table owned by %', session_user;\n\
END IF;\n\
IF EXISTS (SELECT 1 FROM pg_catalog.pg_trigger t WHERE t.tgrelid = rel) OR EXISTS (SELECT 1 FROM pg_catalog.pg_rewrite w WHERE w.ev_class = rel) THEN\n\
RAISE EXCEPTION 'chelabase_merge_marker_untrusted: chelabase_merge.applied has triggers or rules';\n\
END IF;\n\
END\n\
$chelabase_marker$;\n";

/// The recovery marker's setup (P1, E15), run by the worker on the **admin** session
/// (`cloud_admin`, a superuser) before phase 1: the schema `chelabase_merge` and its table
/// `applied`, created when missing and owned by the admin role, closed to PUBLIC; the owner
/// ([`OWNER_ROLE`]) gets `USAGE` on the schema and `SELECT, INSERT` on the table (no update or
/// delete), so phase 1 can insert its marker inside its own transaction.
///
/// It runs as superuser on names the tenant could create first, so it pins `search_path` to
/// `pg_catalog, pg_temp`, qualifies every type and function, and refuses (raises) when an
/// existing `chelabase_merge` schema or `applied` relation isn't owned by `session_user`, or
/// `applied` isn't a plain table or has triggers or rules. The check runs before the
/// `CREATE … IF NOT EXISTS` statements and again after them (a tenant could create the schema
/// in between, and `IF NOT EXISTS` would then accept it), before any grant. One transaction;
/// idempotent.
pub fn marker_setup_sql() -> String {
    let owner = crate::sql::ident(OWNER_ROLE);
    let check = MARKER_CHECK;
    format!(
        "BEGIN;\n\
SET LOCAL search_path = pg_catalog, pg_temp;\n\
{check}\
CREATE SCHEMA IF NOT EXISTS chelabase_merge;\n\
CREATE TABLE IF NOT EXISTS chelabase_merge.applied (merge_id pg_catalog.uuid PRIMARY KEY, applied_at pg_catalog.timestamptz NOT NULL DEFAULT pg_catalog.now());\n\
{check}\
REVOKE ALL ON SCHEMA chelabase_merge FROM PUBLIC;\n\
REVOKE ALL ON TABLE chelabase_merge.applied FROM PUBLIC;\n\
GRANT USAGE ON SCHEMA chelabase_merge TO {owner};\n\
GRANT SELECT, INSERT ON TABLE chelabase_merge.applied TO {owner};\n\
COMMIT;"
    )
}

/// Phase 1's marker: the insert of `merge_id` into `chelabase_merge.applied`, run by the owner
/// inside phase 1's transaction (the table comes from [`marker_setup_sql`]).
pub fn marker_sql(merge_id: Uuid) -> String {
    format!(
        "INSERT INTO chelabase_merge.applied (merge_id) VALUES ({});",
        literal(&merge_id.to_string())
    )
}

/// Locks a phase 2 step waits for at most the merge's `lock_timeout` (E19): they block reads or
/// writes while queued.
const BLOCKING_LOCKS: [&str; 2] = ["ACCESS EXCLUSIVE ", "SHARE ROW EXCLUSIVE "];

/// Whether the step waits for its lock at most `lock_timeout` (E19): every phase 1a value, and
/// a phase 2 step taking a [`BLOCKING_LOCKS`] lock (`SET NOT NULL`, the check's drop, the online
/// foreign key). Concurrent builds and validations take weaker locks and wait as long as needed.
fn bounded_lock_wait(step: &Step) -> bool {
    match step.phase {
        Phase::EnumAdd => true,
        Phase::Online => step
            .locks
            .iter()
            .any(|l| BLOCKING_LOCKS.iter().any(|m| l.starts_with(m))),
        Phase::Roles | Phase::Tx => false,
    }
}

/// The script for one phase 1a or phase 2 step, in the owner's own session (E15), with no
/// transaction block: for phase 2 `SET statement_timeout = 0` (P3); for a step that waits for a
/// blocking lock `SET lock_timeout` from `opts` (E19: a timeout fails the step, and a retry
/// runs it again); then the statement.
pub fn step_script(step: &Step, opts: &PlanOptions) -> String {
    let mut s = String::new();
    if step.phase == Phase::Online {
        s.push_str("SET statement_timeout = 0;\n");
    }
    if bounded_lock_wait(step) {
        s.push_str(&format!(
            "SET lock_timeout = '{}ms';\n",
            opts.lock_timeout_ms
        ));
    }
    s.push_str(&terminated(&step.sql));
    s.push('\n');
    s
}

/// What a merge failed in its online phase leaves on the target when it's abandoned (E27).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum Leftover {
    /// A constraint phase 1 added `NOT VALID` whose validation didn't run: it checks new rows,
    /// not the existing ones.
    NotValidConstraint { schema: String, table: String, name: String },
    /// A temporary `chelabase_merge_nn_*` check (a `NOT NULL` on its way) that wasn't dropped.
    NotNullCheck { schema: String, table: String, name: String },
    /// An index whose concurrent build (or concurrent drop) stopped part-way: Postgres may have
    /// left it INVALID (unused by queries, still updated by writes). Drop it with
    /// `DROP INDEX CONCURRENTLY IF EXISTS` if it's there.
    InvalidIndex { schema: String, table: String, name: String },
    /// A valid index the merge built that its next step didn't put into place: a temporary
    /// `chelabase_merge_ix_*` index (E19) not renamed over the index it replaces, or a unique
    /// constraint's index not attached to its constraint. Nothing uses it; it costs on every
    /// write.
    UnattachedIndex { schema: String, table: String, name: String },
}

/// `quote_ident`'s output back to the name.
fn unquote(quoted: &str) -> String {
    match quoted
        .strip_prefix('"')
        .and_then(|q| q.strip_suffix('"'))
    {
        Some(inner) => inner.replace("\"\"", "\""),
        None => quoted.to_string(),
    }
}

/// E27: what `plan` left on the target when the online steps `done` (their ids) are all that
/// ran to the end. Phase 2 runs in order and stops at its first failure, so the first online
/// step not done is the one that stopped (it may have run part-way) and the ones after it never
/// ran. Listed, in plan order:
/// - each constraint that exists `NOT VALID` (added in phase 1, or by an online step that ran)
///   whose validation didn't run;
/// - each temporary `NOT NULL` check whose drop didn't run;
/// - the index the stopped step was building, or dropping concurrently (maybe INVALID);
/// - each index built whose step putting it into place (the rename of a `chelabase_merge_ix_*`
///   index, the `ADD CONSTRAINT … USING INDEX`) didn't run.
pub fn leftovers(plan: &Plan, done: &std::collections::HashSet<String>) -> Vec<Leftover> {
    use crate::exclusions::{MERGE_INDEX_PREFIX, MERGE_NOT_NULL_CHECK_PREFIX};
    use crate::sql::{ident, qualified};
    let online = || plan.steps.iter().filter(|s| s.phase == Phase::Online);
    let is_done = |s: &Step| done.contains(&s.id.0);
    let stopped = online().find(|s| !is_done(s)).map(|s| &s.id);
    let mut out = Vec::new();
    for step in online() {
        match &step.online {
            Some(OnlineCheck::ConstraintValidated {
                schema,
                table,
                name,
            }) => {
                let (schema, table, name) = (schema.clone(), table.clone(), name.clone());
                if name.starts_with(MERGE_NOT_NULL_CHECK_PREFIX) {
                    let drop = format!("DROP CONSTRAINT IF EXISTS {}", ident(&name));
                    if online().any(|s| s.sql.contains(&drop) && !is_done(s)) {
                        out.push(Leftover::NotNullCheck {
                            schema,
                            table,
                            name,
                        });
                    }
                    continue;
                }
                // `ADD CONSTRAINT … USING INDEX` (its index is listed below) isn't a
                // validation: only a `VALIDATE CONSTRAINT` that didn't run leaves one NOT VALID,
                // and only once the constraint exists.
                let validates =
                    step.sql.contains(&format!("VALIDATE CONSTRAINT {}", ident(&name)));
                let added_online = online().find(|s| {
                    matches!(&s.online, Some(OnlineCheck::ConstraintExists { schema: cs, table: ct, name: cn })
                        if *cs == schema && *ct == table && *cn == name)
                });
                if validates && !is_done(step) && added_online.is_none_or(is_done) {
                    out.push(Leftover::NotValidConstraint {
                        schema,
                        table,
                        name,
                    });
                }
            }
            Some(OnlineCheck::IndexValid {
                schema,
                table,
                name,
            }) => {
                let (schema, table, name) = (schema.clone(), table.clone(), name.clone());
                if Some(&step.id) == stopped {
                    out.push(Leftover::InvalidIndex {
                        schema,
                        table,
                        name,
                    });
                    continue;
                }
                if !is_done(step) {
                    continue;
                }
                // Built: does the step that puts it into place remain?
                let rename = format!("ALTER INDEX IF EXISTS {} RENAME TO ", qualified(&schema, &name));
                let attach = format!("USING INDEX {}", ident(&name));
                let placing = online().find(|s| {
                    (name.starts_with(MERGE_INDEX_PREFIX) && s.sql.starts_with(&rename))
                        || s.sql.contains(&attach)
                });
                if let Some(placing) = placing.filter(|s| !is_done(s)) {
                    // A replaced index's concurrent drop that stopped part-way: the old index
                    // may be INVALID (E19: create the new one, drop the old, rename).
                    if let Some(old) = placing.sql.strip_prefix(&rename) {
                        let drop = format!("DROP INDEX CONCURRENTLY IF EXISTS {}.{old}", ident(&schema));
                        if online().any(|s| Some(&s.id) == stopped && s.sql == drop) {
                            out.push(Leftover::UnattachedIndex {
                                schema: schema.clone(),
                                table: table.clone(),
                                name,
                            });
                            out.push(Leftover::InvalidIndex {
                                schema,
                                table,
                                name: unquote(old),
                            });
                            continue;
                        }
                    }
                    out.push(Leftover::UnattachedIndex {
                        schema,
                        table,
                        name,
                    });
                }
            }
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests;
