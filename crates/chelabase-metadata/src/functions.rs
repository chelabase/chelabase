//! Edge functions m2: a database's functions and services, their immutable deployments, and
//! per branch which deployment is active, whether it's on, its secrets, schedules and triggers.
//!
//! Bundle bytes never live here: a deployment records the bundle's SHA-256 and size, and the
//! bytes sit in object storage at `chelabase-functions/{database_id}/{sha256}`.
//!
//! A new branch inherits its parent's function state in its create transaction
//! ([`copy_branch_functions`]); a branch or database tombstone purges it in the tombstone's
//! transaction ([`purge_branch_functions`]). Branch and database rows are never deleted, so FK
//! cascades never do that work.
//!
//! # Lock order
//!
//! Every function write takes the rows it locks in this order (the crate's order, `branches.rs`,
//! extended with the function tables); the existing writers of the same rows take a subsequence:
//!
//! | # | Row | Function writes | Existing writers |
//! |---|---|---|---|
//! | 1 | `databases` | `FOR SHARE` (all) | `create_branch` `FOR NO KEY UPDATE`; database tombstone `UPDATE` |
//! | 2 | `branches`, by id | `FOR SHARE`: the target branch (per-branch writes), every live branch of the database (kind change, delete) | `protect_branch`, branch tombstone `UPDATE`; DDL receiver, key rotation `FOR NO KEY UPDATE` |
//! | 3 | `functions` | `FOR KEY SHARE` (per-branch writes), `FOR UPDATE` (kind change, delete) | none |
//! | 4 | `branch_functions` | `FOR NO KEY UPDATE` or `UPDATE` (per-branch writes) | branch create copies (insert), tombstone purge (delete) |
//! | 4a | `function_schedules` | `FOR UPDATE` (schedule update, delete); the worker's claim `FOR UPDATE SKIP LOCKED`, alone | branch create copies (insert), tombstone purge (delete) |
//! | 4b | `function_triggers` | `INSERT` (add); `UPDATE` (m13 enable/disable), `DELETE` (m13 trigger delete; function delete's cascade); the worker's settle `UPDATE` and the delivery record `UPDATE` (the dispatcher's, and m13b the worker's count of the events its cleanup dropped), each alone | branch create copies (insert), a reset's or restore's switch `UPDATE` (after its database and branch rows, before the endpoint), tombstone purge (delete) |
//! | 4c | `function_trigger_drops` (m13) | `INSERT` (trigger disable and delete, after 4b; function delete, before its cascade): no foreign key, so it locks no other row; the worker's clear `DELETE`, alone | tombstone purge (delete) |
//! | 5 | `neon_endpoints` | `FOR SHARE`, the branch's (run-as key) | DDL receiver, `delete_role_copy`, `rename_role_copy` `FOR UPDATE`; secret key create and rotate `FOR SHARE` |
//! | 6 | `data_api_keys` | `UPDATE`/`INSERT` (run-as key, function delete) | the endpoint writers above, revoke, resync |
//!
//! No writer takes a row of a lower number while holding a higher one: no writer of an
//! endpoint or key row (5, 6) touches 3 or 4 afterwards, and `set_run_as` takes the endpoint
//! (5) before it revokes or inserts a key (6); the worker's guarded run-as mint
//! ([`MetadataClient::settle_run_as_mint`]) takes the same rows in the same order. A function delete writes key rows without an
//! endpoint row: it revokes only its own managed keys (at most one live per branch) and waits
//! for nothing after them, so a role drop holding the endpoint and one of those rows can't
//! wait on it in a cycle. `set_function_last_error` updates its state row alone,
//! `set_service_signing_secret` (m9) its function row (3) alone, and `set_service_health` (m9)
//! takes a `service_health` row (beside 3, written by nobody else) and, inserting it, its
//! deployment row `FOR KEY SHARE` (the foreign key), and nothing after either; the
//! worker's schedule claim ([`MetadataClient::claim_due_schedules`]) locks schedule rows (4a)
//! alone, skipping any another transaction holds, and takes nothing after them.
//! The worker's trigger settle ([`MetadataClient::settle_trigger_installs`]) updates trigger rows (4b)
//! alone and takes nothing after them, as does the dispatcher's
//! [`MetadataClient::record_trigger_deliveries`] (m13); the worker's
//! [`MetadataClient::clear_trigger_drops`] deletes drop rows (4c) alone. A drop row is only
//! ever inserted, by a writer already holding what it needs, and has no foreign key, so it can't
//! close a wait cycle: a function delete inserts its drops before its cascade takes 4 and 4b.
//! `function_writes_keep_the_lock_order` (`tests/functions_live.rs`) runs each function write
//! against each existing writer, stalled on each row in turn, in both orders.
//!
//! So a branch create (database row `FOR NO KEY UPDATE`) copies a parent no write is
//! changing, a tombstone (an `UPDATE` of the branch or database row) either waits for a write
//! or makes it refused, and protecting a branch either waits for a kind change or delete or is
//! seen by it.

use chelabase_secrets::SecretKey;
use chelabase_types::{BranchId, DatabaseId};
use chrono::{DateTime, NaiveDate, Utc};
use sqlx::postgres::PgRow;
use sqlx::{PgConnection, Postgres, Row, Transaction};
use uuid::Uuid;

use crate::{MetadataClient, MetadataError};

/// A function's bundle may be at most 10 MiB.
pub const MAX_BUNDLE_SIZE: i64 = 10 * 1024 * 1024;
/// A secret value may be at most 64 KiB.
pub const MAX_SECRET_BYTES: usize = 64 * 1024;
/// A function's recorded last error is cut to this many characters.
pub const MAX_LAST_ERROR_CHARS: usize = 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FunctionKind {
    Function,
    Service,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FunctionAuth {
    Key,
    AppUser,
    Public,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriggerSource {
    Table,
    Storage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriggerState {
    PendingInstall,
    Installed,
    Failed,
}

macro_rules! text_enum {
    ($ty:ty, $what:literal, $($variant:path => $text:literal),+ $(,)?) => {
        impl $ty {
            pub fn as_str(self) -> &'static str {
                match self { $($variant => $text),+ }
            }

            pub fn parse(value: &str) -> Result<Self, MetadataError> {
                match value {
                    $($text => Ok($variant),)+
                    other => Err(MetadataError::InvalidStatus(format!(concat!($what, " {}"), other))),
                }
            }
        }
    };
}

text_enum!(FunctionKind, "function kind", FunctionKind::Function => "function", FunctionKind::Service => "service");
text_enum!(FunctionAuth, "function auth", FunctionAuth::Key => "key", FunctionAuth::AppUser => "app_user", FunctionAuth::Public => "public");
/// Why a branch's run-as key will never be minted by the worker (`run_as_unmintable`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunAsUnmintable {
    /// No role by that name on the branch.
    Missing,
    /// A role by that name with another OID than the approved one, or no approved OID.
    Changed,
    /// The role checks refuse the role.
    Refused,
}

text_enum!(RunAsUnmintable, "run-as outcome", RunAsUnmintable::Missing => "missing", RunAsUnmintable::Changed => "changed", RunAsUnmintable::Refused => "refused");

/// What the worker's child run-as mint settles ([`MetadataClient::settle_run_as_mint`]).
#[derive(Debug, Clone, Copy)]
pub enum RunAsMint<'a> {
    /// The key, bound to the approved role and OID.
    Key(&'a RunAs),
    /// Never: the outcome, and the `last_error` to show.
    Unmintable(RunAsUnmintable, &'a str),
}

text_enum!(TriggerSource, "trigger source", TriggerSource::Table => "table", TriggerSource::Storage => "storage");
text_enum!(TriggerState, "trigger state", TriggerState::PendingInstall => "pending_install", TriggerState::Installed => "installed", TriggerState::Failed => "failed");

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FunctionRecord {
    pub id: Uuid,
    pub database_id: DatabaseId,
    pub name: String,
    pub kind: FunctionKind,
    pub created_at: DateTime<Utc>,
}

/// A deployment to record. A function has a bundle (`bundle_sha256` and `bundle_size`); a
/// service has none, and `spec` holds its image or URL, port and health path.
#[derive(Debug, Clone)]
pub struct NewDeployment<'a> {
    pub id: Uuid,
    pub function_id: Uuid,
    pub bundle_sha256: Option<&'a str>,
    pub bundle_size: Option<i64>,
    pub spec: &'a serde_json::Value,
    pub compatibility_date: NaiveDate,
    pub created_by: &'a str,
    pub note: Option<&'a str>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DeploymentRecord {
    pub id: Uuid,
    pub function_id: Uuid,
    pub bundle_sha256: Option<String>,
    pub bundle_size: Option<i64>,
    pub spec: serde_json::Value,
    pub compatibility_date: NaiveDate,
    pub created_by: String,
    pub created_at: DateTime<Utc>,
    pub note: Option<String>,
}

/// A function's state on one branch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchFunction {
    pub function_id: Uuid,
    pub name: String,
    pub kind: FunctionKind,
    pub deployment_id: Uuid,
    pub enabled: bool,
    pub auth: FunctionAuth,
    pub run_as_role: Option<String>,
    pub run_as_key_id: Option<Uuid>,
    /// The role OID a member approved for `run_as_role` (`set_run_as`), copied to a new
    /// branch: a child's key is minted only for that OID.
    pub run_as_role_oid: Option<i64>,
    /// Why the worker will never mint this branch's run-as key, until a member sets run-as
    /// again ([`MetadataClient::settle_run_as_mint`]).
    pub run_as_unmintable: Option<RunAsUnmintable>,
    pub last_error: Option<String>,
    pub updated_at: DateTime<Utc>,
    /// m9: what the last health check of the active deployment saw (`healthy` or
    /// `unhealthy`); `None` before any, and for a function.
    pub health: Option<String>,
}

/// m9: an external service's deployment that some live branch serves enabled: what the health
/// checks poll.
#[derive(Debug, Clone, PartialEq)]
pub struct ServiceTarget {
    pub deployment_id: Uuid,
    pub spec: serde_json::Value,
}

/// What serving a function on a branch needs: its active deployment and how it's called.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedFunction {
    pub function_id: Uuid,
    pub kind: FunctionKind,
    pub deployment_id: Uuid,
    pub bundle_sha256: Option<String>,
    pub spec: serde_json::Value,
    pub compatibility_date: NaiveDate,
    pub auth: FunctionAuth,
    pub enabled: bool,
    pub run_as_role: Option<String>,
    pub run_as_key_id: Option<Uuid>,
}

#[derive(Debug, Clone)]
pub struct NewSchedule<'a> {
    pub branch_id: BranchId,
    pub function_id: Uuid,
    pub cron: &'a str,
    pub timezone: &'a str,
    pub next_run_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScheduleRecord {
    pub id: Uuid,
    pub branch_id: BranchId,
    pub function_id: Uuid,
    pub cron: String,
    pub timezone: String,
    pub next_run_at: Option<DateTime<Utc>>,
    pub enabled: bool,
}

/// A schedule's part a caller got wrong ([`Schedule::parse`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScheduleError {
    Cron(String),
    Timezone(String),
}

impl ScheduleError {
    /// The request field it names: `cron` or `timezone`.
    pub fn field(&self) -> &'static str {
        match self {
            ScheduleError::Cron(_) => "cron",
            ScheduleError::Timezone(_) => "timezone",
        }
    }
}

impl std::fmt::Display for ScheduleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ScheduleError::Cron(why) | ScheduleError::Timezone(why) => {
                write!(f, "{}: {why}", self.field())
            }
        }
    }
}

/// A checked schedule: a 5-field cron (minute granularity, no seconds or years) in an IANA
/// time zone. Times in a DST gap run at the first instant after it; times a fall-back repeats
/// run once.
#[derive(Debug, Clone)]
pub struct Schedule {
    cron: croner::Cron,
    tz: chrono_tz::Tz,
}

impl Schedule {
    pub fn parse(cron: &str, timezone: &str) -> Result<Schedule, ScheduleError> {
        use croner::parser::{CronParser, Seconds, Year};
        if cron.is_empty() || cron.len() > 100 {
            return Err(ScheduleError::Cron("1 to 100 characters".into()));
        }
        if timezone.is_empty() || timezone.len() > 64 {
            return Err(ScheduleError::Timezone("1 to 64 characters".into()));
        }
        let parsed = CronParser::builder()
            .seconds(Seconds::Disallowed)
            .year(Year::Disallowed)
            .build()
            .parse(cron)
            .map_err(|e| ScheduleError::Cron(format!("not a 5-field cron ({e})")))?;
        let tz = timezone
            .parse::<chrono_tz::Tz>()
            .map_err(|_| ScheduleError::Timezone(format!("unknown IANA time zone {timezone:?}")))?;
        Ok(Schedule { cron: parsed, tz })
    }

    /// The first time strictly after `after`; `None` when there is none (a date that never
    /// comes).
    pub fn next_after(&self, after: DateTime<Utc>) -> Option<DateTime<Utc>> {
        self.cron
            .find_next_occurrence(&after.with_timezone(&self.tz), false)
            .ok()
            .map(|t| t.with_timezone(&Utc))
    }
}

/// What a claim does with one due schedule ([`advance`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Advance {
    /// The new `next_run_at`.
    pub next: Option<DateTime<Utc>>,
    /// Run the function for this tick (the `scheduledTime`): the due `next_run_at`, once, however
    /// many ticks were missed; `None` for a disabled or undeployed function, or a schedule that
    /// had no time yet.
    pub run: Option<DateTime<Utc>>,
    /// The schedule stays enabled; false when its cron or time zone no longer parses or it has
    /// no time left.
    pub enabled: bool,
}

/// A due schedule's step at `now`: missed ticks coalesce into one run, and `next_run_at` moves
/// to the first tick after `now`, whether the function runs or not.
pub fn advance(
    cron: &str,
    timezone: &str,
    due: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
    function_enabled: bool,
) -> Advance {
    let next = Schedule::parse(cron, timezone)
        .ok()
        .and_then(|s| s.next_after(now));
    Advance {
        next,
        run: due.filter(|_| function_enabled && next.is_some()),
        enabled: next.is_some(),
    }
}

/// A schedule a claim advanced ([`MetadataClient::claim_due_schedules`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimedSchedule {
    pub id: Uuid,
    pub branch_id: BranchId,
    pub function_name: String,
    pub cron: String,
    /// The tick to run, `None` when nothing runs ([`Advance::run`]).
    pub run: Option<DateTime<Utc>>,
    pub next_run_at: Option<DateTime<Utc>>,
}

/// A trigger to add. `table`: `schema_name`, `table_name` and events from insert, update and
/// delete. `storage`: optional `bucket` and `prefix`, and `object.*` events. The schema's
/// CHECK ties the columns to the source.
#[derive(Debug, Clone)]
pub struct NewTrigger<'a> {
    pub branch_id: BranchId,
    pub function_id: Uuid,
    pub source: TriggerSource,
    pub pg_database: &'a str,
    pub schema_name: Option<&'a str>,
    pub table_name: Option<&'a str>,
    pub bucket: Option<&'a str>,
    pub prefix: Option<&'a str>,
    pub events: &'a [&'a str],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TriggerRecord {
    pub id: Uuid,
    pub branch_id: BranchId,
    pub function_id: Uuid,
    pub source: TriggerSource,
    pub enabled: bool,
    pub state: TriggerState,
    pub enabled_since: DateTime<Utc>,
    pub pg_database: String,
    pub schema_name: Option<String>,
    pub table_name: Option<String>,
    pub bucket: Option<String>,
    pub prefix: Option<String>,
    pub events: Vec<String>,
    /// Why the worker couldn't install it (state `failed`), as the worker words it.
    pub last_error: Option<String>,
    /// m13: events that went dead (10 failed deliveries), since the trigger was made.
    pub dead_count: i64,
    /// m13: why the last failed delivery failed; cleared by a delivery.
    pub last_delivery_error: Option<String>,
    /// m13: when an event was last delivered.
    pub last_delivered_at: Option<DateTime<Utc>>,
    /// m13b: events the outbox bound removed undelivered (dead ones past the cap or the
    /// retention, a disabled trigger's queued ones).
    pub dropped_count: i64,
}

/// m13: what one dispatcher pass saw for a trigger ([`MetadataClient::record_trigger_deliveries`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TriggerDelivery {
    pub id: Uuid,
    /// At least one event was delivered (a 2xx).
    pub delivered: bool,
    /// Events that went dead.
    pub dead: i64,
    /// m13b: events the outbox bound removed undelivered.
    pub dropped: i64,
    /// The last failure's reason, when a delivery failed.
    pub error: Option<String>,
}

/// m14: an enabled storage trigger the worker delivers to
/// ([`MetadataClient::storage_trigger_targets`]): the trigger, its branch, its function's
/// name and database, its filters and the time it was (re)enabled (its consumer starts then).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageTriggerTarget {
    pub id: Uuid,
    pub branch_id: BranchId,
    pub database_id: DatabaseId,
    pub function_name: String,
    pub pg_database: String,
    pub bucket: Option<String>,
    pub prefix: Option<String>,
    pub events: Vec<String>,
    pub enabled_since: DateTime<Utc>,
}

/// One table trigger's install, as the worker reports it
/// ([`MetadataClient::settle_trigger_installs`]): the row and the `enabled_since` it read (the
/// generation it installed), and `None` when the trigger is in the database, or why not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TriggerInstall {
    pub id: Uuid,
    pub enabled_since: DateTime<Utc>,
    pub error: Option<String>,
}

/// A function's run-as role and the secret data key minted for it ([`MetadataClient::set_run_as`]).
/// The caller generates the key and checks the role on the compute (its OID, timeline and
/// BYPASSRLS, as for any secret key); the store seals `secret` for display to the functions
/// service only.
#[derive(Clone)]
pub struct RunAs {
    pub role: String,
    pub role_oid: i64,
    pub role_timeline: Option<String>,
    pub role_bypass_rls: bool,
    pub key_id: Uuid,
    pub prefix: String,
    pub sha256: [u8; 32],
    /// The key's plaintext.
    pub secret: String,
}

impl std::fmt::Debug for RunAs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunAs")
            .field("role", &self.role)
            .field("role_oid", &self.role_oid)
            .field("role_timeline", &self.role_timeline)
            .field("role_bypass_rls", &self.role_bypass_rls)
            .field("key_id", &self.key_id)
            .field("prefix", &self.prefix)
            .field("sha256", &"<redacted>")
            .field("secret", &"<redacted>")
            .finish()
    }
}

/// `^[a-z][a-z0-9-]{0,62}$`, as the schema checks.
pub fn valid_function_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    matches!(bytes.next(), Some(b'a'..=b'z'))
        && name.len() <= 63
        && bytes.all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'-'))
}

/// `^[A-Za-z_][A-Za-z0-9_]{0,127}$`, as the schema checks (an environment variable name).
pub fn valid_secret_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    matches!(bytes.next(), Some(b'A'..=b'Z' | b'a'..=b'z' | b'_'))
        && name.len() <= 128
        && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

fn valid_sha256(hash: &str) -> bool {
    hash.len() == 64 && hash.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// A secret's AAD: it opens only for that branch, function and name.
fn secret_aad(branch: Uuid, function: Uuid, name: &str) -> String {
    format!("fn-secret:{branch}:{function}:{name}")
}

fn signing_aad(function: Uuid) -> String {
    format!("{function}:service-signing")
}

fn invalid(reason: impl Into<String>) -> MetadataError {
    MetadataError::InvalidFunction(reason.into())
}

/// A CHECK violation (bad trigger columns, say) is the caller's input, not a fault.
fn map_check(err: sqlx::Error) -> MetadataError {
    match err {
        sqlx::Error::Database(e) if e.code().as_deref() == Some("23514") => invalid(format!(
            "refused by {}",
            e.constraint().unwrap_or("a check")
        )),
        err => err.into(),
    }
}

/// Takes the locks every per-branch write needs (see the module docs) and checks that the
/// branch and its database are live and that `function` belongs to that database. Anything
/// missing, tombstoned or foreign is [`MetadataError::FunctionNotFound`].
async fn lock_branch_function(
    tx: &mut Transaction<'static, Postgres>,
    branch: BranchId,
    function: Uuid,
) -> Result<(), MetadataError> {
    let database: Option<Uuid> =
        sqlx::query_scalar("SELECT database_id FROM branches WHERE id = $1")
            .bind(branch.as_uuid())
            .fetch_optional(&mut **tx)
            .await?;
    let Some(database) = database else {
        return Err(MetadataError::FunctionNotFound);
    };
    let live: Option<i32> = sqlx::query_scalar(
        "SELECT 1 FROM databases WHERE id = $1 AND status NOT IN ('deleting', 'deleted') FOR SHARE",
    )
    .bind(database)
    .fetch_optional(&mut **tx)
    .await?;
    let branch_live: Option<i32> = match live {
        Some(_) => {
            sqlx::query_scalar(
                "SELECT 1 FROM branches
                 WHERE id = $1 AND status NOT IN ('deleting', 'deleted') FOR SHARE",
            )
            .bind(branch.as_uuid())
            .fetch_optional(&mut **tx)
            .await?
        }
        None => None,
    };
    let ours: Option<i32> = match branch_live {
        Some(_) => {
            sqlx::query_scalar(
                "SELECT 1 FROM functions WHERE id = $1 AND database_id = $2 FOR KEY SHARE",
            )
            .bind(function)
            .bind(database)
            .fetch_optional(&mut **tx)
            .await?
        }
        None => None,
    };
    ours.map(|_| ()).ok_or(MetadataError::FunctionNotFound)
}

/// Takes the branch's endpoint row `FOR SHARE` (waiting at most 5 s), after the branch row:
/// the order `create_secret_key` and the DDL receiver use.
async fn lock_endpoint_share(
    tx: &mut Transaction<'static, Postgres>,
    branch: BranchId,
) -> Result<(), MetadataError> {
    sqlx::query("SELECT set_config('lock_timeout', '5s', true)")
        .execute(&mut **tx)
        .await?;
    let endpoint: Option<Option<String>> =
        sqlx::query_scalar("SELECT endpoint_id FROM branches WHERE id = $1")
            .bind(branch.as_uuid())
            .fetch_optional(&mut **tx)
            .await?;
    if let Some(Some(endpoint)) = endpoint {
        sqlx::query("SELECT id FROM neon_endpoints WHERE id = $1 FOR SHARE")
            .bind(&endpoint)
            .fetch_optional(&mut **tx)
            .await
            .map_err(|e| crate::ddl::lock_timeout(e, &endpoint))?;
    }
    sqlx::query("SELECT set_config('lock_timeout', '0', true)")
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// For a write to a function as a whole (its kind, its deletion), which changes every branch
/// that has it active: takes the database row `FOR SHARE`, then every live branch row of the
/// database `FOR SHARE` (by id), then the function row `FOR UPDATE`, and refuses with
/// [`MetadataError::FunctionActiveOnProtectedBranch`] when a live protected branch has the
/// function active. The function's kind, or `None` for an unknown function or another
/// database's.
///
/// Protecting a branch updates its row, so it waits for this transaction, or this waits for it
/// and then reads the branch protected (each statement reads what committed before it). A
/// branch create holds the database row `FOR NO KEY UPDATE`, so no branch appears meanwhile.
/// The order (database, branches, function) is the per-branch writes' ([`lock_branch_function`]).
async fn lock_function_unprotected(
    tx: &mut Transaction<'static, Postgres>,
    database: DatabaseId,
    function: Uuid,
) -> Result<Option<FunctionKind>, MetadataError> {
    sqlx::query("SELECT 1 FROM databases WHERE id = $1 FOR SHARE")
        .bind(database.as_uuid())
        .execute(&mut **tx)
        .await?;
    sqlx::query(
        "SELECT id FROM branches
         WHERE database_id = $1 AND status NOT IN ('deleting', 'deleted')
         ORDER BY id FOR SHARE",
    )
    .bind(database.as_uuid())
    .execute(&mut **tx)
    .await?;
    let kind: Option<String> = sqlx::query_scalar(
        "SELECT kind FROM functions WHERE id = $1 AND database_id = $2 FOR UPDATE",
    )
    .bind(function)
    .bind(database.as_uuid())
    .fetch_optional(&mut **tx)
    .await?;
    let Some(kind) = kind else {
        return Ok(None);
    };
    let protected: bool = sqlx::query_scalar(
        "SELECT EXISTS (
             SELECT 1 FROM branch_functions bf JOIN branches b ON b.id = bf.branch_id
             WHERE bf.function_id = $1 AND b.protected
               AND b.status NOT IN ('deleting', 'deleted'))",
    )
    .bind(function)
    .fetch_one(&mut **tx)
    .await?;
    if protected {
        return Err(MetadataError::FunctionActiveOnProtectedBranch);
    }
    Ok(Some(FunctionKind::parse(&kind)?))
}

/// m13: records that `trigger`'s database trigger on `branch` must be dropped (4c: an insert
/// with no foreign key, so it locks no other row).
async fn record_drop(
    tx: &mut Transaction<'static, Postgres>,
    branch: BranchId,
    trigger: Uuid,
) -> Result<(), MetadataError> {
    sqlx::query("INSERT INTO function_trigger_drops (branch_id, trigger_id) VALUES ($1, $2)")
        .bind(branch.as_uuid())
        .bind(trigger)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// Revokes the keys a function manages (dropping their sealed values, before the delete nulls
/// `managed_by_function_id`) and deletes it, with its deployments and every branch's state.
/// The caller holds the function row `FOR UPDATE`.
async fn delete_locked_function(
    tx: &mut Transaction<'static, Postgres>,
    function: Uuid,
) -> Result<(), MetadataError> {
    // m13: drop, then forget. The cascade below deletes the trigger rows; their database
    // triggers are dropped by the worker from these rows (4c, an insert that locks nothing).
    sqlx::query(
        "INSERT INTO function_trigger_drops (branch_id, trigger_id)
         SELECT branch_id, id FROM function_triggers WHERE function_id = $1 AND source = 'table'",
    )
    .bind(function)
    .execute(&mut **tx)
    .await?;
    sqlx::query(
        "UPDATE data_api_keys SET revoked_at = coalesce(revoked_at, now()), sealed = NULL
         WHERE managed_by_function_id = $1 AND (revoked_at IS NULL OR sealed IS NOT NULL)",
    )
    .bind(function)
    .execute(&mut **tx)
    .await?;
    sqlx::query("DELETE FROM functions WHERE id = $1")
        .bind(function)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

fn function_from_row(r: &PgRow) -> Result<FunctionRecord, MetadataError> {
    Ok(FunctionRecord {
        id: r.get("id"),
        database_id: DatabaseId(r.get("database_id")),
        name: r.get("name"),
        kind: FunctionKind::parse(r.get("kind"))?,
        created_at: r.get("created_at"),
    })
}

fn deployment_from_row(r: &PgRow) -> DeploymentRecord {
    DeploymentRecord {
        id: r.get("id"),
        function_id: r.get("function_id"),
        bundle_sha256: r.get("bundle_sha256"),
        bundle_size: r.get("bundle_size"),
        spec: r.get("spec"),
        compatibility_date: r.get("compatibility_date"),
        created_by: r.get("created_by"),
        created_at: r.get("created_at"),
        note: r.get("note"),
    }
}

/// The columns [`trigger_from_row`] reads.
const TRIGGER_COLUMNS: &str = "id, branch_id, function_id, source, enabled, state, enabled_since,
    pg_database, schema_name, table_name, bucket, prefix, events, last_error, dead_count, dropped_count,
    last_delivery_error, last_delivered_at";

fn trigger_from_row(r: &PgRow) -> Result<TriggerRecord, MetadataError> {
    Ok(TriggerRecord {
        id: r.get("id"),
        branch_id: BranchId(r.get("branch_id")),
        function_id: r.get("function_id"),
        source: TriggerSource::parse(r.get("source"))?,
        enabled: r.get("enabled"),
        state: TriggerState::parse(r.get("state"))?,
        enabled_since: r.get("enabled_since"),
        pg_database: r.get("pg_database"),
        schema_name: r.get("schema_name"),
        table_name: r.get("table_name"),
        bucket: r.get("bucket"),
        prefix: r.get("prefix"),
        events: r.get("events"),
        last_error: r.get("last_error"),
        dead_count: r.get("dead_count"),
        dropped_count: r.get("dropped_count"),
        last_delivery_error: r.get("last_delivery_error"),
        last_delivered_at: r.get("last_delivered_at"),
    })
}

/// In a reset's or restore's switch transaction (which holds the database and branch rows):
/// every table trigger of `branch` goes back to `pending_install` with a new `enabled_since`
/// and no error, so the worker truncates the outbox and reinstalls them on the new timeline
/// before any is `installed` again (edge functions m8). Storage triggers don't live in the
/// database and are left alone.
pub(crate) async fn pend_table_triggers(
    tx: &mut PgConnection,
    branch: Uuid,
) -> Result<(), MetadataError> {
    sqlx::query(
        "UPDATE function_triggers
         SET state = 'pending_install', enabled_since = clock_timestamp(), last_error = NULL
         WHERE branch_id = $1 AND source = 'table'",
    )
    .bind(branch)
    .execute(&mut *tx)
    .await?;
    Ok(())
}

/// Copies `parent`'s function state onto `child`, a branch row inserted in the same
/// transaction (`create_branch`, which holds the database row `FOR NO KEY UPDATE`, so no
/// function write on the parent runs meanwhile). The child gets the same active deployment,
/// enabled flag, auth and `run_as_role`, but never the parent's key (`run_as_key_id` is null:
/// the worker mints the child's own). Schedules and triggers get new ids; table triggers start
/// `pending_install` (the child's database needs its own), storage triggers `installed` from
/// now, so the child never replays the parent's events. Secrets are opened and sealed again for
/// the child's AAD, last: any failure rolls the whole create back.
///
/// Only `create_branch` copies. A backup branch (`switch_to_restored_timeline`) gets no function
/// state, so a branch later created from a backup inherits none either.
pub(crate) async fn copy_branch_functions(
    tx: &mut PgConnection,
    key: Option<&SecretKey>,
    parent: Uuid,
    child: Uuid,
) -> Result<(), MetadataError> {
    sqlx::query(
        "INSERT INTO branch_functions
             (branch_id, function_id, deployment_id, enabled, auth, run_as_role, run_as_key_id,
              run_as_role_oid)
         SELECT $2, function_id, deployment_id, enabled, auth, run_as_role, NULL, run_as_role_oid
         FROM branch_functions WHERE branch_id = $1",
    )
    .bind(parent)
    .bind(child)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO function_schedules (id, branch_id, function_id, cron, timezone, next_run_at, enabled)
         SELECT gen_random_uuid(), $2, function_id, cron, timezone, next_run_at, enabled
         FROM function_schedules WHERE branch_id = $1",
    )
    .bind(parent)
    .bind(child)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO function_triggers
             (id, branch_id, function_id, source, enabled, state, enabled_since, pg_database,
              schema_name, table_name, bucket, prefix, events)
         SELECT gen_random_uuid(), $2, function_id, source, enabled,
                CASE source WHEN 'table' THEN 'pending_install' ELSE 'installed' END,
                clock_timestamp(), pg_database, schema_name, table_name, bucket, prefix, events
         FROM function_triggers WHERE branch_id = $1",
    )
    .bind(parent)
    .bind(child)
    .execute(&mut *tx)
    .await?;
    let secrets: Vec<(Uuid, String, String)> = sqlx::query_as(
        "SELECT function_id, name, ciphertext FROM branch_function_secrets WHERE branch_id = $1",
    )
    .bind(parent)
    .fetch_all(&mut *tx)
    .await?;
    if secrets.is_empty() {
        return Ok(());
    }
    let key = key.ok_or_else(|| {
        MetadataError::Config("the secrets key is needed to copy function secrets".into())
    })?;
    for (function, name, sealed) in secrets {
        let plain = chelabase_secrets::open(key, &secret_aad(parent, function, &name), &sealed)
            .map_err(|e| MetadataError::Secret(format!("function secret {name}: {e}")))?;
        let resealed = chelabase_secrets::seal(key, &secret_aad(child, function, &name), &plain);
        sqlx::query(
            "INSERT INTO branch_function_secrets (branch_id, function_id, name, ciphertext)
             VALUES ($1, $2, $3, $4)",
        )
        .bind(child)
        .bind(function)
        .bind(&name)
        .bind(resealed)
        .execute(&mut *tx)
        .await?;
    }
    Ok(())
}

/// Deletes the branches' function state (secrets, schedules, triggers, active deployments),
/// in a tombstone's transaction.
pub(crate) async fn purge_branch_functions(
    tx: &mut PgConnection,
    branches: &[Uuid],
) -> Result<(), MetadataError> {
    for table in [
        "branch_function_secrets",
        "function_schedules",
        "function_triggers",
        "function_trigger_drops",
        "branch_functions",
    ] {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "DELETE FROM {table} WHERE branch_id = ANY($1)"
        )))
        .bind(branches)
        .execute(&mut *tx)
        .await?;
    }
    Ok(())
}

impl MetadataClient {
    fn secrets_key(&self) -> Result<&SecretKey, MetadataError> {
        self.secrets_key
            .as_deref()
            .ok_or_else(|| MetadataError::Config("this metadata client has no secrets key".into()))
    }

    /// Adds a function or service to a live database. A taken name is
    /// [`MetadataError::Conflict`]; a bad one [`MetadataError::InvalidFunction`].
    pub async fn create_function(
        &self,
        database_id: DatabaseId,
        name: &str,
        kind: FunctionKind,
    ) -> Result<FunctionRecord, MetadataError> {
        if !valid_function_name(name) {
            return Err(invalid(
                "a name is a lowercase letter, then up to 62 of a-z, 0-9 and -",
            ));
        }
        let mut tx = self.pool.begin().await?;
        let live: Option<i32> = sqlx::query_scalar(
            "SELECT 1 FROM databases WHERE id = $1 AND status NOT IN ('deleting', 'deleted') FOR SHARE",
        )
        .bind(database_id.as_uuid())
        .fetch_optional(&mut *tx)
        .await?;
        if live.is_none() {
            return Err(MetadataError::FunctionNotFound);
        }
        let row = sqlx::query(
            "INSERT INTO functions (id, database_id, name, kind) VALUES ($1, $2, $3, $4)
             RETURNING id, database_id, name, kind, created_at",
        )
        .bind(Uuid::new_v4())
        .bind(database_id.as_uuid())
        .bind(name)
        .bind(kind.as_str())
        .fetch_one(&mut *tx)
        .await;
        let row = match row {
            Ok(row) => row,
            Err(sqlx::Error::Database(e)) if e.is_unique_violation() => {
                return Err(MetadataError::Conflict);
            }
            Err(e) => return Err(map_check(e)),
        };
        tx.commit().await?;
        function_from_row(&row)
    }

    /// The database's functions, by name.
    pub async fn list_functions(
        &self,
        database_id: DatabaseId,
    ) -> Result<Vec<FunctionRecord>, MetadataError> {
        sqlx::query(
            "SELECT id, database_id, name, kind, created_at FROM functions
             WHERE database_id = $1 ORDER BY name",
        )
        .bind(database_id.as_uuid())
        .fetch_all(&self.pool)
        .await?
        .iter()
        .map(function_from_row)
        .collect()
    }

    /// Deletes a function with its deployments and every branch's state for it, and revokes
    /// the data keys it manages. False for an unknown function or one of another database.
    pub async fn delete_function(
        &self,
        database_id: DatabaseId,
        function_id: Uuid,
    ) -> Result<bool, MetadataError> {
        let mut tx = self.pool.begin().await?;
        // The database row first, as every function write takes it: a branch create (which
        // holds it `FOR NO KEY UPDATE` while it copies) waits for this delete, or this waits
        // for the create, so the copy never names a function deleted under it.
        sqlx::query("SELECT 1 FROM databases WHERE id = $1 FOR SHARE")
            .bind(database_id.as_uuid())
            .execute(&mut *tx)
            .await?;
        let found: Option<i32> = sqlx::query_scalar(
            "SELECT 1 FROM functions WHERE id = $1 AND database_id = $2 FOR UPDATE",
        )
        .bind(function_id)
        .bind(database_id.as_uuid())
        .fetch_optional(&mut *tx)
        .await?;
        if found.is_none() {
            return Ok(false);
        }
        delete_locked_function(&mut tx, function_id).await?;
        tx.commit().await?;
        Ok(true)
    }

    /// Sets how the function is called on the branch. False when it has no deployment there.
    pub async fn set_function_auth(
        &self,
        branch: BranchId,
        function_id: Uuid,
        auth: FunctionAuth,
    ) -> Result<bool, MetadataError> {
        let mut tx = self.pool.begin().await?;
        lock_branch_function(&mut tx, branch, function_id).await?;
        let updated = sqlx::query(
            "UPDATE branch_functions SET auth = $3, updated_at = now()
             WHERE branch_id = $1 AND function_id = $2",
        )
        .bind(branch.as_uuid())
        .bind(function_id)
        .bind(auth.as_str())
        .execute(&mut *tx)
        .await?
        .rows_affected();
        tx.commit().await?;
        Ok(updated == 1)
    }

    /// Sets (or with `None` clears) the role the function runs as on the branch, in one
    /// transaction: every live key the function manages on the branch is revoked (its sealed
    /// value dropped), then for `Some` a secret key bound to the role's identity is stored with
    /// `managed_by_function_id`, its plaintext sealed with `Purpose::DataKey` and the key's id
    /// as AAD (the public key's seal), and `run_as_role` and `run_as_key_id` name it. False,
    /// nothing written, when the function has no deployment on the branch.
    ///
    /// Lock order (module docs): the branch-function locks ([`lock_branch_function`]), the
    /// state row, then the branch's endpoint row `FOR SHARE` before any key row (the DDL
    /// receiver, a role drop and a rename hold it `FOR UPDATE` and then write key rows, so a
    /// DROP after this commit sees the key). A wait over 5 s for it is
    /// [`MetadataError::EndpointLockTimeout`].
    pub async fn set_run_as(
        &self,
        branch: BranchId,
        function_id: Uuid,
        run_as: Option<&RunAs>,
    ) -> Result<bool, MetadataError> {
        let sealed = match run_as {
            Some(r) => Some(chelabase_secrets::seal_credential(
                self.secrets_key()?,
                &r.key_id.to_string(),
                chelabase_secrets::Purpose::DataKey,
                &r.secret,
            )),
            None => None,
        };
        let mut tx = self.pool.begin().await?;
        lock_branch_function(&mut tx, branch, function_id).await?;
        let name: Option<String> = sqlx::query_scalar(
            "SELECT f.name FROM branch_functions bf JOIN functions f ON f.id = bf.function_id
             WHERE bf.branch_id = $1 AND bf.function_id = $2 FOR NO KEY UPDATE OF bf",
        )
        .bind(branch.as_uuid())
        .bind(function_id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(name) = name else {
            return Ok(false);
        };
        // Before any key row (audit B1): a role drop or rename holds the endpoint `FOR UPDATE`
        // and then writes key rows.
        lock_endpoint_share(&mut tx, branch).await?;
        sqlx::query(
            "UPDATE data_api_keys SET revoked_at = now(), sealed = NULL
             WHERE branch_id = $1 AND managed_by_function_id = $2 AND revoked_at IS NULL",
        )
        .bind(branch.as_uuid())
        .bind(function_id)
        .execute(&mut *tx)
        .await?;
        if let Some(r) = run_as {
            sqlx::query(
                "INSERT INTO data_api_keys
                     (id, branch_id, kind, role, name, prefix, sha256, sealed, role_oid,
                      role_timeline, role_bypass_rls, managed_by_function_id)
                 VALUES ($1, $2, 'secret', $3, $4, $5, $6, $7, $8, $9, $10, $11)",
            )
            .bind(r.key_id)
            .bind(branch.as_uuid())
            .bind(&r.role)
            .bind(&name)
            .bind(&r.prefix)
            .bind(r.sha256.as_slice())
            .bind(&sealed)
            .bind(r.role_oid)
            .bind(&r.role_timeline)
            .bind(r.role_bypass_rls)
            .bind(function_id)
            .execute(&mut *tx)
            .await
            .map_err(map_check)?;
        }
        sqlx::query(
            "UPDATE branch_functions
             SET run_as_role = $3, run_as_key_id = $4, run_as_role_oid = $5,
                 run_as_unmintable = NULL, updated_at = now()
             WHERE branch_id = $1 AND function_id = $2",
        )
        .bind(branch.as_uuid())
        .bind(function_id)
        .bind(run_as.map(|r| r.role.as_str()))
        .bind(run_as.map(|r| r.key_id))
        .bind(run_as.map(|r| r.role_oid))
        .execute(&mut *tx)
        .await
        .map_err(map_check)?;
        tx.commit().await?;
        Ok(true)
    }

    /// The worker's child run-as mint (edge functions m8, audit round 3), guarded: writes only
    /// while the branch's state row still has `run_as_role = expected_role`, `run_as_role_oid`
    /// equal to `expected_oid` (the approved OID), no key and no terminal outcome. A `Key` must
    /// be bound to that role and OID ([`MetadataError::InvalidFunction`] otherwise); it is
    /// stored as `set_run_as` stores one. An `Unmintable` records the outcome and its
    /// `last_error`. False, with nothing written (the caller drops the key), when the guard
    /// doesn't match: a member changed or cleared the run-as role meanwhile, or another worker
    /// settled it first. So the worker never overrides a member's run-as choice.
    ///
    /// Lock order (module docs): `set_run_as`'s. The branch-function locks, the state row
    /// `FOR NO KEY UPDATE`, then (a key only) the branch's endpoint row `FOR SHARE` before any
    /// key row.
    pub async fn settle_run_as_mint(
        &self,
        branch: BranchId,
        function_id: Uuid,
        expected_role: &str,
        expected_oid: Option<i64>,
        outcome: RunAsMint<'_>,
    ) -> Result<bool, MetadataError> {
        let sealed = match outcome {
            RunAsMint::Key(r) => {
                if r.role != expected_role || Some(r.role_oid) != expected_oid {
                    return Err(invalid("a run-as key is bound to the approved role and OID"));
                }
                Some(chelabase_secrets::seal_credential(
                    self.secrets_key()?,
                    &r.key_id.to_string(),
                    chelabase_secrets::Purpose::DataKey,
                    &r.secret,
                ))
            }
            RunAsMint::Unmintable(..) => None,
        };
        let mut tx = self.pool.begin().await?;
        lock_branch_function(&mut tx, branch, function_id).await?;
        let name: Option<String> = sqlx::query_scalar(
            "SELECT f.name FROM branch_functions bf JOIN functions f ON f.id = bf.function_id
             WHERE bf.branch_id = $1 AND bf.function_id = $2 AND bf.run_as_role = $3
               AND bf.run_as_role_oid IS NOT DISTINCT FROM $4 AND bf.run_as_key_id IS NULL
               AND bf.run_as_unmintable IS NULL
             FOR NO KEY UPDATE OF bf",
        )
        .bind(branch.as_uuid())
        .bind(function_id)
        .bind(expected_role)
        .bind(expected_oid)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(name) = name else {
            return Ok(false);
        };
        match outcome {
            RunAsMint::Key(r) => {
                lock_endpoint_share(&mut tx, branch).await?;
                sqlx::query(
                    "UPDATE data_api_keys SET revoked_at = now(), sealed = NULL
                     WHERE branch_id = $1 AND managed_by_function_id = $2 AND revoked_at IS NULL",
                )
                .bind(branch.as_uuid())
                .bind(function_id)
                .execute(&mut *tx)
                .await?;
                sqlx::query(
                    "INSERT INTO data_api_keys
                         (id, branch_id, kind, role, name, prefix, sha256, sealed, role_oid,
                          role_timeline, role_bypass_rls, managed_by_function_id)
                     VALUES ($1, $2, 'secret', $3, $4, $5, $6, $7, $8, $9, $10, $11)",
                )
                .bind(r.key_id)
                .bind(branch.as_uuid())
                .bind(&r.role)
                .bind(&name)
                .bind(&r.prefix)
                .bind(r.sha256.as_slice())
                .bind(&sealed)
                .bind(r.role_oid)
                .bind(&r.role_timeline)
                .bind(r.role_bypass_rls)
                .bind(function_id)
                .execute(&mut *tx)
                .await
                .map_err(map_check)?;
                sqlx::query(
                    "UPDATE branch_functions SET run_as_key_id = $3, updated_at = now()
                     WHERE branch_id = $1 AND function_id = $2",
                )
                .bind(branch.as_uuid())
                .bind(function_id)
                .bind(r.key_id)
                .execute(&mut *tx)
                .await?;
            }
            RunAsMint::Unmintable(why, error) => {
                let error: String = error.chars().take(MAX_LAST_ERROR_CHARS).collect();
                sqlx::query(
                    "UPDATE branch_functions
                     SET run_as_unmintable = $3, last_error = $4, updated_at = now()
                     WHERE branch_id = $1 AND function_id = $2",
                )
                .bind(branch.as_uuid())
                .bind(function_id)
                .bind(why.as_str())
                .bind(error)
                .execute(&mut *tx)
                .await?;
            }
        }
        tx.commit().await?;
        Ok(true)
    }

    /// The function's run-as key on the branch, opened: `(key id, plaintext)`. Only a live
    /// secret key the function itself manages, named by its state row, on a live `normal`
    /// branch of a live database; `None` otherwise. A sealed value that doesn't open for that
    /// key id is [`MetadataError::Secret`].
    pub async fn managed_run_as_key(
        &self,
        branch: BranchId,
        function_id: Uuid,
    ) -> Result<Option<(Uuid, String)>, MetadataError> {
        let row: Option<(Uuid, String)> = sqlx::query_as(
            "SELECT k.id, k.sealed
             FROM branch_functions bf
             JOIN data_api_keys k ON k.id = bf.run_as_key_id AND k.branch_id = bf.branch_id
             JOIN branches b ON b.id = bf.branch_id
             JOIN databases d ON d.id = b.database_id
             WHERE bf.branch_id = $1 AND bf.function_id = $2
               AND k.managed_by_function_id = bf.function_id AND k.kind = 'secret'
               AND k.revoked_at IS NULL AND k.sealed IS NOT NULL
               AND b.kind = 'normal' AND b.status NOT IN ('deleting', 'deleted')
               AND d.status NOT IN ('deleting', 'deleted')",
        )
        .bind(branch.as_uuid())
        .bind(function_id)
        .fetch_optional(&self.pool)
        .await?;
        let Some((id, sealed)) = row else {
            return Ok(None);
        };
        let secret = chelabase_secrets::open_credential(
            self.secrets_key()?,
            &id.to_string(),
            chelabase_secrets::Purpose::DataKey,
            &sealed,
        )
        .map_err(|e| MetadataError::Secret(format!("run-as key {id}: {e}")))?;
        Ok(Some((id, secret)))
    }

    /// Records the function's last invocation error on the branch (`None` clears it), cut to
    /// [`MAX_LAST_ERROR_CHARS`] characters. A branch or database that is deleting or deleted, or
    /// a function without state there, is left as it is.
    pub async fn set_function_last_error(
        &self,
        branch: BranchId,
        function_id: Uuid,
        error: Option<&str>,
    ) -> Result<(), MetadataError> {
        let error: Option<String> = error.map(|e| e.chars().take(MAX_LAST_ERROR_CHARS).collect());
        sqlx::query(
            "UPDATE branch_functions bf SET last_error = $3, updated_at = now()
             FROM branches b JOIN databases d ON d.id = b.database_id
             WHERE bf.branch_id = $1 AND bf.function_id = $2 AND b.id = bf.branch_id
               AND b.status NOT IN ('deleting', 'deleted')
               AND d.status NOT IN ('deleting', 'deleted')",
        )
        .bind(branch.as_uuid())
        .bind(function_id)
        .bind(error)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Changes a function's kind. False for an unknown function or one of another database;
    /// the same kind is a no-op. Refused with
    /// [`MetadataError::FunctionActiveOnProtectedBranch`] while a live protected branch has it
    /// active ([`lock_function_unprotected`]), and with [`MetadataError::InvalidFunction`] once
    /// it has a deployment (a deployment is a function's bundle or a service's spec, never
    /// both).
    pub async fn set_function_kind(
        &self,
        database_id: DatabaseId,
        function_id: Uuid,
        kind: FunctionKind,
    ) -> Result<bool, MetadataError> {
        let mut tx = self.pool.begin().await?;
        let Some(current) = lock_function_unprotected(&mut tx, database_id, function_id).await?
        else {
            return Ok(false);
        };
        if current == kind {
            return Ok(true);
        }
        let deployed: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM function_deployments WHERE function_id = $1)",
        )
        .bind(function_id)
        .fetch_one(&mut *tx)
        .await?;
        if deployed {
            return Err(invalid(
                "a function with deployments keeps its kind (delete it and create it again)",
            ));
        }
        sqlx::query("UPDATE functions SET kind = $2 WHERE id = $1")
            .bind(function_id)
            .bind(kind.as_str())
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(true)
    }

    /// [`Self::delete_function`] for the management API: refused with
    /// [`MetadataError::FunctionActiveOnProtectedBranch`], nothing written, while a live
    /// protected branch has the function active ([`lock_function_unprotected`]).
    pub async fn delete_function_unprotected(
        &self,
        database_id: DatabaseId,
        function_id: Uuid,
    ) -> Result<bool, MetadataError> {
        let mut tx = self.pool.begin().await?;
        if lock_function_unprotected(&mut tx, database_id, function_id)
            .await?
            .is_none()
        {
            return Ok(false);
        }
        delete_locked_function(&mut tx, function_id).await?;
        tx.commit().await?;
        Ok(true)
    }

    /// Records an immutable deployment. The bundle's bytes are already in object storage; this
    /// stores their hash and size (at most [`MAX_BUNDLE_SIZE`]). A function needs a bundle and
    /// a service has none.
    pub async fn create_deployment(
        &self,
        d: &NewDeployment<'_>,
    ) -> Result<DeploymentRecord, MetadataError> {
        match (d.bundle_sha256, d.bundle_size) {
            (Some(hash), Some(size)) => {
                if !valid_sha256(hash) {
                    return Err(invalid("a bundle hash is 64 lowercase hex characters"));
                }
                if !(0..=MAX_BUNDLE_SIZE).contains(&size) {
                    return Err(invalid(format!(
                        "a bundle is at most {MAX_BUNDLE_SIZE} bytes"
                    )));
                }
            }
            (None, None) => {}
            _ => return Err(invalid("a bundle has both a hash and a size")),
        }
        let mut tx = self.pool.begin().await?;
        let kind: Option<String> = sqlx::query_scalar(
            "SELECT f.kind FROM functions f JOIN databases d ON d.id = f.database_id
             WHERE f.id = $1 AND d.status NOT IN ('deleting', 'deleted')
             FOR KEY SHARE OF f",
        )
        .bind(d.function_id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(kind) = kind else {
            return Err(MetadataError::FunctionNotFound);
        };
        let has_bundle = d.bundle_sha256.is_some();
        match (FunctionKind::parse(&kind)?, has_bundle) {
            (FunctionKind::Function, false) => {
                return Err(invalid("a function deployment has a bundle"));
            }
            (FunctionKind::Service, true) => {
                return Err(invalid("a service deployment has no bundle"));
            }
            _ => {}
        }
        let row = sqlx::query(
            "INSERT INTO function_deployments
                 (id, function_id, bundle_sha256, bundle_size, spec, compatibility_date,
                  created_by, note)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
             RETURNING *",
        )
        .bind(d.id)
        .bind(d.function_id)
        .bind(d.bundle_sha256)
        .bind(d.bundle_size)
        .bind(d.spec)
        .bind(d.compatibility_date)
        .bind(d.created_by)
        .bind(d.note)
        .fetch_one(&mut *tx)
        .await
        .map_err(map_check)?;
        tx.commit().await?;
        Ok(deployment_from_row(&row))
    }

    /// The function's deployments, newest first (none for an unknown function). Callers scope
    /// the function to its database first ([`Self::list_functions`]).
    pub async fn list_deployments(
        &self,
        function_id: Uuid,
    ) -> Result<Vec<DeploymentRecord>, MetadataError> {
        Ok(sqlx::query(
            "SELECT * FROM function_deployments WHERE function_id = $1
             ORDER BY created_at DESC, id DESC",
        )
        .bind(function_id)
        .fetch_all(&self.pool)
        .await?
        .iter()
        .map(deployment_from_row)
        .collect())
    }

    /// Makes `deployment_id` the function's active deployment on the branch (a new branch
    /// state starts disabled, auth `key`). A deployment of another function is
    /// [`MetadataError::InvalidFunction`].
    pub async fn set_active_deployment(
        &self,
        branch: BranchId,
        function_id: Uuid,
        deployment_id: Uuid,
    ) -> Result<(), MetadataError> {
        let mut tx = self.pool.begin().await?;
        lock_branch_function(&mut tx, branch, function_id).await?;
        let owner: Option<Uuid> =
            sqlx::query_scalar("SELECT function_id FROM function_deployments WHERE id = $1")
                .bind(deployment_id)
                .fetch_optional(&mut *tx)
                .await?;
        if owner != Some(function_id) {
            return Err(invalid("the deployment belongs to another function"));
        }
        sqlx::query(
            "INSERT INTO branch_functions (branch_id, function_id, deployment_id)
             VALUES ($1, $2, $3)
             ON CONFLICT (branch_id, function_id)
             DO UPDATE SET deployment_id = EXCLUDED.deployment_id, last_error = NULL,
                           updated_at = now()",
        )
        .bind(branch.as_uuid())
        .bind(function_id)
        .bind(deployment_id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Turns the function on or off on the branch. False when it has no deployment there.
    pub async fn set_enabled(
        &self,
        branch: BranchId,
        function_id: Uuid,
        enabled: bool,
    ) -> Result<bool, MetadataError> {
        let mut tx = self.pool.begin().await?;
        lock_branch_function(&mut tx, branch, function_id).await?;
        let updated = sqlx::query(
            "UPDATE branch_functions SET enabled = $3, updated_at = now()
             WHERE branch_id = $1 AND function_id = $2",
        )
        .bind(branch.as_uuid())
        .bind(function_id)
        .bind(enabled)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        tx.commit().await?;
        Ok(updated == 1)
    }

    /// The branch's deployed functions, by name.
    pub async fn list_for_branch(
        &self,
        branch: BranchId,
    ) -> Result<Vec<BranchFunction>, MetadataError> {
        sqlx::query(
            "SELECT bf.function_id, f.name, f.kind, bf.deployment_id, bf.enabled, bf.auth,
                    bf.run_as_role, bf.run_as_key_id, bf.run_as_role_oid, bf.run_as_unmintable,
                    bf.last_error, bf.updated_at, h.status AS health
             FROM branch_functions bf JOIN functions f ON f.id = bf.function_id
             LEFT JOIN service_health h ON h.deployment_id = bf.deployment_id
             WHERE bf.branch_id = $1 ORDER BY f.name",
        )
        .bind(branch.as_uuid())
        .fetch_all(&self.pool)
        .await?
        .iter()
        .map(|r| {
            Ok(BranchFunction {
                function_id: r.get("function_id"),
                name: r.get("name"),
                kind: FunctionKind::parse(r.get("kind"))?,
                deployment_id: r.get("deployment_id"),
                enabled: r.get("enabled"),
                auth: FunctionAuth::parse(r.get("auth"))?,
                run_as_role: r.get("run_as_role"),
                run_as_key_id: r.get("run_as_key_id"),
                run_as_role_oid: r.get("run_as_role_oid"),
                run_as_unmintable: r
                    .get::<Option<&str>, _>("run_as_unmintable")
                    .map(RunAsUnmintable::parse)
                    .transpose()?,
                last_error: r.get("last_error"),
                updated_at: r.get("updated_at"),
                health: r.get("health"),
            })
        })
        .collect()
    }

    /// The function `name` as the branch serves it, enabled or not. `None` when it has no
    /// deployment there, or the branch or its database is deleting or deleted.
    pub async fn resolve(
        &self,
        branch: BranchId,
        name: &str,
    ) -> Result<Option<ResolvedFunction>, MetadataError> {
        let row = sqlx::query(
            "SELECT f.id, f.kind, d.id AS deployment_id, d.bundle_sha256, d.spec,
                    d.compatibility_date, bf.auth, bf.enabled, bf.run_as_role, bf.run_as_key_id
             FROM branch_functions bf
             JOIN functions f ON f.id = bf.function_id
             JOIN function_deployments d ON d.id = bf.deployment_id
             JOIN branches b ON b.id = bf.branch_id
             JOIN databases db ON db.id = b.database_id AND db.id = f.database_id
             WHERE bf.branch_id = $1 AND f.name = $2
               AND b.status NOT IN ('deleting', 'deleted')
               AND db.status NOT IN ('deleting', 'deleted')",
        )
        .bind(branch.as_uuid())
        .bind(name)
        .fetch_optional(&self.pool)
        .await?;
        row.map(|r| {
            Ok(ResolvedFunction {
                function_id: r.get("id"),
                kind: FunctionKind::parse(r.get("kind"))?,
                deployment_id: r.get("deployment_id"),
                bundle_sha256: r.get("bundle_sha256"),
                spec: r.get("spec"),
                compatibility_date: r.get("compatibility_date"),
                auth: FunctionAuth::parse(r.get("auth"))?,
                enabled: r.get("enabled"),
                run_as_role: r.get("run_as_role"),
                run_as_key_id: r.get("run_as_key_id"),
            })
        })
        .transpose()
    }

    /// m9: sets the service's signing secret, sealed for the function. False when the database
    /// has no such function (or is going away). Updates the function row alone.
    pub async fn set_service_signing_secret(
        &self,
        database_id: DatabaseId,
        function_id: Uuid,
        secret: &str,
    ) -> Result<bool, MetadataError> {
        let sealed = chelabase_secrets::seal(self.secrets_key()?, &signing_aad(function_id), secret);
        let updated = sqlx::query(
            "UPDATE functions f SET signing_secret = $3
             WHERE f.id = $1 AND f.database_id = $2
               AND EXISTS (SELECT 1 FROM databases d WHERE d.id = f.database_id
                           AND d.status NOT IN ('deleting', 'deleted'))",
        )
        .bind(function_id)
        .bind(database_id.as_uuid())
        .bind(sealed)
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(updated == 1)
    }

    /// m9: the service's signing secret, opened; `None` when it has none.
    pub async fn service_signing_secret(
        &self,
        function_id: Uuid,
    ) -> Result<Option<String>, MetadataError> {
        let sealed: Option<Option<String>> =
            sqlx::query_scalar("SELECT signing_secret FROM functions WHERE id = $1")
                .bind(function_id)
                .fetch_optional(&self.pool)
                .await?;
        sealed
            .flatten()
            .map(|s| {
                chelabase_secrets::open(self.secrets_key()?, &signing_aad(function_id), &s)
                    .map_err(|e| MetadataError::Secret(format!("service signing secret: {e}")))
            })
            .transpose()
    }

    /// m9: the external services' deployments active and enabled on a live branch, each once.
    pub async fn list_service_targets(&self) -> Result<Vec<ServiceTarget>, MetadataError> {
        Ok(sqlx::query_as::<_, (Uuid, serde_json::Value)>(
            "SELECT DISTINCT d.id, d.spec
             FROM branch_functions bf
             JOIN functions f ON f.id = bf.function_id
             JOIN function_deployments d ON d.id = bf.deployment_id
             JOIN branches b ON b.id = bf.branch_id
             JOIN databases db ON db.id = b.database_id
             WHERE bf.enabled AND f.kind = 'service' AND d.spec->>'backend' = 'external'
               AND b.status NOT IN ('deleting', 'deleted')
               AND db.status NOT IN ('deleting', 'deleted')",
        )
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(|(deployment_id, spec)| ServiceTarget { deployment_id, spec })
        .collect())
    }

    /// m9: records what a health check of the deployment saw. A deployment that is gone is
    /// skipped.
    pub async fn set_service_health(
        &self,
        deployment_id: Uuid,
        healthy: bool,
    ) -> Result<(), MetadataError> {
        sqlx::query(
            "INSERT INTO service_health (deployment_id, status)
             SELECT id, $2 FROM function_deployments WHERE id = $1
             ON CONFLICT (deployment_id)
             DO UPDATE SET status = EXCLUDED.status, checked_at = now()",
        )
        .bind(deployment_id)
        .bind(if healthy { "healthy" } else { "unhealthy" })
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Sets a secret of the function on the branch, sealed for that branch, function and name.
    pub async fn put_secret(
        &self,
        branch: BranchId,
        function_id: Uuid,
        name: &str,
        value: &str,
    ) -> Result<(), MetadataError> {
        if !valid_secret_name(name) {
            return Err(invalid(
                "a secret name is a letter or _, then up to 127 of A-Z, a-z, 0-9 and _",
            ));
        }
        if value.len() > MAX_SECRET_BYTES {
            return Err(invalid(format!(
                "a secret is at most {MAX_SECRET_BYTES} bytes"
            )));
        }
        let sealed = chelabase_secrets::seal(
            self.secrets_key()?,
            &secret_aad(branch.as_uuid(), function_id, name),
            value,
        );
        let mut tx = self.pool.begin().await?;
        lock_branch_function(&mut tx, branch, function_id).await?;
        sqlx::query(
            "INSERT INTO branch_function_secrets (branch_id, function_id, name, ciphertext)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (branch_id, function_id, name)
             DO UPDATE SET ciphertext = EXCLUDED.ciphertext, updated_at = now()",
        )
        .bind(branch.as_uuid())
        .bind(function_id)
        .bind(name)
        .bind(sealed)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Deletes a secret. False when there was none.
    pub async fn delete_secret(
        &self,
        branch: BranchId,
        function_id: Uuid,
        name: &str,
    ) -> Result<bool, MetadataError> {
        let mut tx = self.pool.begin().await?;
        lock_branch_function(&mut tx, branch, function_id).await?;
        let deleted = sqlx::query(
            "DELETE FROM branch_function_secrets
             WHERE branch_id = $1 AND function_id = $2 AND name = $3",
        )
        .bind(branch.as_uuid())
        .bind(function_id)
        .bind(name)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        tx.commit().await?;
        Ok(deleted == 1)
    }

    /// The names of the function's secrets on the branch, sorted.
    pub async fn secret_names(
        &self,
        branch: BranchId,
        function_id: Uuid,
    ) -> Result<Vec<String>, MetadataError> {
        Ok(sqlx::query_scalar(
            "SELECT name FROM branch_function_secrets
             WHERE branch_id = $1 AND function_id = $2 ORDER BY name",
        )
        .bind(branch.as_uuid())
        .bind(function_id)
        .fetch_all(&self.pool)
        .await?)
    }

    /// The function's secrets on the branch, opened, sorted by name. A value that doesn't
    /// open for this branch, function and name is [`MetadataError::Secret`].
    pub async fn decrypt_secrets(
        &self,
        branch: BranchId,
        function_id: Uuid,
    ) -> Result<Vec<(String, String)>, MetadataError> {
        let key = self.secrets_key()?;
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT name, ciphertext FROM branch_function_secrets
             WHERE branch_id = $1 AND function_id = $2 ORDER BY name",
        )
        .bind(branch.as_uuid())
        .bind(function_id)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|(name, sealed)| {
                let aad = secret_aad(branch.as_uuid(), function_id, &name);
                let value = chelabase_secrets::open(key, &aad, &sealed)
                    .map_err(|e| MetadataError::Secret(format!("function secret {name}: {e}")))?;
                Ok((name, value))
            })
            .collect()
    }

    /// Adds a schedule for the function on the branch.
    pub async fn add_schedule(&self, s: &NewSchedule<'_>) -> Result<Uuid, MetadataError> {
        if s.cron.is_empty() || s.cron.len() > 100 || s.timezone.is_empty() || s.timezone.len() > 64
        {
            return Err(invalid(
                "a cron is 1 to 100 characters and a time zone 1 to 64",
            ));
        }
        let mut tx = self.pool.begin().await?;
        lock_branch_function(&mut tx, s.branch_id, s.function_id).await?;
        let id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO function_schedules (id, branch_id, function_id, cron, timezone, next_run_at)
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(id)
        .bind(s.branch_id.as_uuid())
        .bind(s.function_id)
        .bind(s.cron)
        .bind(s.timezone)
        .bind(s.next_run_at)
        .execute(&mut *tx)
        .await
        .map_err(map_check)?;
        tx.commit().await?;
        Ok(id)
    }

    /// The branch's schedules, oldest first.
    pub async fn list_schedules(
        &self,
        branch: BranchId,
    ) -> Result<Vec<ScheduleRecord>, MetadataError> {
        Ok(sqlx::query(
            "SELECT id, branch_id, function_id, cron, timezone, next_run_at, enabled
             FROM function_schedules WHERE branch_id = $1 ORDER BY created_at, id",
        )
        .bind(branch.as_uuid())
        .fetch_all(&self.pool)
        .await?
        .iter()
        .map(|r| ScheduleRecord {
            id: r.get("id"),
            branch_id: BranchId(r.get("branch_id")),
            function_id: r.get("function_id"),
            cron: r.get("cron"),
            timezone: r.get("timezone"),
            next_run_at: r.get("next_run_at"),
            enabled: r.get("enabled"),
        })
        .collect())
    }

    /// Changes a schedule of the function on the branch: what is given of `cron`, `timezone`
    /// and `enabled`. A changed cron or time zone, or turning it on, moves `next_run_at` to the
    /// first tick after now. `None` when the branch has no such schedule for the function.
    pub async fn update_schedule(
        &self,
        branch: BranchId,
        function_id: Uuid,
        id: Uuid,
        cron: Option<&str>,
        timezone: Option<&str>,
        enabled: Option<bool>,
    ) -> Result<Option<ScheduleRecord>, MetadataError> {
        let mut tx = self.pool.begin().await?;
        lock_branch_function(&mut tx, branch, function_id).await?;
        let row = sqlx::query(
            "SELECT cron, timezone, next_run_at, enabled, now() AS now FROM function_schedules
             WHERE id = $1 AND branch_id = $2 AND function_id = $3 FOR UPDATE",
        )
        .bind(id)
        .bind(branch.as_uuid())
        .bind(function_id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let was: (String, String, bool) = (row.get("cron"), row.get("timezone"), row.get("enabled"));
        let new_cron = cron.unwrap_or(&was.0);
        let new_tz = timezone.unwrap_or(&was.1);
        let new_enabled = enabled.unwrap_or(was.2);
        let schedule = Schedule::parse(new_cron, new_tz).map_err(|e| invalid(e.to_string()))?;
        let mut next: Option<DateTime<Utc>> = row.get("next_run_at");
        if new_cron != was.0 || new_tz != was.1 || (new_enabled && !was.2) {
            next = schedule.next_after(row.get("now"));
        }
        sqlx::query(
            "UPDATE function_schedules SET cron = $2, timezone = $3, enabled = $4, next_run_at = $5
             WHERE id = $1",
        )
        .bind(id)
        .bind(new_cron)
        .bind(new_tz)
        .bind(new_enabled)
        .bind(next)
        .execute(&mut *tx)
        .await
        .map_err(map_check)?;
        tx.commit().await?;
        Ok(Some(ScheduleRecord {
            id,
            branch_id: branch,
            function_id,
            cron: new_cron.to_owned(),
            timezone: new_tz.to_owned(),
            next_run_at: next,
            enabled: new_enabled,
        }))
    }

    /// Deletes a schedule of the function on the branch. False when there was none.
    pub async fn delete_schedule(
        &self,
        branch: BranchId,
        function_id: Uuid,
        id: Uuid,
    ) -> Result<bool, MetadataError> {
        let mut tx = self.pool.begin().await?;
        lock_branch_function(&mut tx, branch, function_id).await?;
        let deleted = sqlx::query(
            "DELETE FROM function_schedules WHERE id = $1 AND branch_id = $2 AND function_id = $3",
        )
        .bind(id)
        .bind(branch.as_uuid())
        .bind(function_id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        tx.commit().await?;
        Ok(deleted == 1)
    }

    /// The worker's claim: up to `limit` enabled schedules due by the database's `now()` (or
    /// with no time yet) on a live branch of a live database, locked `FOR UPDATE SKIP LOCKED`
    /// so concurrent claimers never take the same row, each advanced by [`advance`] in the same
    /// transaction. The caller runs the ones with [`ClaimedSchedule::run`] after this returns
    /// (at most once: a failed run isn't retried).
    pub async fn claim_due_schedules(
        &self,
        limit: i64,
    ) -> Result<Vec<ClaimedSchedule>, MetadataError> {
        let mut tx = self.pool.begin().await?;
        let rows = sqlx::query(
            "SELECT s.id, s.branch_id, f.name, s.cron, s.timezone, s.next_run_at,
                    coalesce(bf.enabled, false) AS function_enabled, now() AS now
             FROM function_schedules s
             JOIN branches b ON b.id = s.branch_id AND b.status NOT IN ('deleting', 'deleted')
             JOIN databases db ON db.id = b.database_id AND db.status NOT IN ('deleting', 'deleted')
             JOIN functions f ON f.id = s.function_id AND f.database_id = db.id
             LEFT JOIN branch_functions bf
               ON bf.branch_id = s.branch_id AND bf.function_id = s.function_id
             WHERE s.enabled AND (s.next_run_at IS NULL OR s.next_run_at <= now())
             ORDER BY s.next_run_at NULLS FIRST, s.id
             LIMIT $1
             FOR UPDATE OF s SKIP LOCKED",
        )
        .bind(limit)
        .fetch_all(&mut *tx)
        .await?;
        let mut claimed = Vec::with_capacity(rows.len());
        let (mut ids, mut nexts, mut enabled) = (Vec::new(), Vec::new(), Vec::new());
        for r in &rows {
            let cron: String = r.get("cron");
            let step = advance(
                &cron,
                r.get("timezone"),
                r.get("next_run_at"),
                r.get("now"),
                r.get("function_enabled"),
            );
            let id: Uuid = r.get("id");
            ids.push(id);
            nexts.push(step.next);
            enabled.push(step.enabled);
            claimed.push(ClaimedSchedule {
                id,
                branch_id: BranchId(r.get("branch_id")),
                function_name: r.get("name"),
                cron,
                run: step.run,
                next_run_at: step.next,
            });
        }
        if !ids.is_empty() {
            sqlx::query(
                "UPDATE function_schedules AS s SET next_run_at = v.next, enabled = v.enabled
                 FROM unnest($1::uuid[], $2::timestamptz[], $3::bool[]) AS v(id, next, enabled)
                 WHERE s.id = v.id",
            )
            .bind(ids)
            .bind(nexts)
            .bind(enabled)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(claimed)
    }

    /// Adds a trigger for the function on the branch: a table trigger `pending_install` (the
    /// worker installs it), a storage trigger `installed` from now. Columns that don't fit the
    /// source, and a repeated event, are [`MetadataError::InvalidFunction`].
    pub async fn add_trigger(&self, t: &NewTrigger<'_>) -> Result<Uuid, MetadataError> {
        // A repeat would install `AFTER INSERT OR INSERT` or deliver one event twice.
        if t.events
            .iter()
            .enumerate()
            .any(|(i, e)| t.events[..i].contains(e))
        {
            return Err(invalid("a trigger names each event once"));
        }
        let mut tx = self.pool.begin().await?;
        lock_branch_function(&mut tx, t.branch_id, t.function_id).await?;
        let id = Uuid::new_v4();
        let state = match t.source {
            TriggerSource::Table => TriggerState::PendingInstall,
            TriggerSource::Storage => TriggerState::Installed,
        };
        let events: Vec<&str> = t.events.to_vec();
        sqlx::query(
            "INSERT INTO function_triggers
                 (id, branch_id, function_id, source, state, pg_database, schema_name,
                  table_name, bucket, prefix, events)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
        )
        .bind(id)
        .bind(t.branch_id.as_uuid())
        .bind(t.function_id)
        .bind(t.source.as_str())
        .bind(state.as_str())
        .bind(t.pg_database)
        .bind(t.schema_name)
        .bind(t.table_name)
        .bind(t.bucket)
        .bind(t.prefix)
        .bind(events)
        .execute(&mut *tx)
        .await
        .map_err(map_check)?;
        tx.commit().await?;
        Ok(id)
    }

    /// The branch's triggers, oldest first.
    pub async fn list_triggers(
        &self,
        branch: BranchId,
    ) -> Result<Vec<TriggerRecord>, MetadataError> {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT {TRIGGER_COLUMNS} FROM function_triggers WHERE branch_id = $1
             ORDER BY created_at, id"
        )))
        .bind(branch.as_uuid())
        .fetch_all(&self.pool)
        .await?
        .iter()
        .map(trigger_from_row)
        .collect()
    }

    /// m13: turns the function's trigger on the branch on or off; `None` for an unknown
    /// trigger, or one of another function or branch. Turning a table trigger on pends it
    /// again (a new `enabled_since`, no error: the worker installs it); turning it off records
    /// a drop (`function_trigger_drops`): the worker drops its database trigger and its queued
    /// events, so a disabled trigger queues nothing. A storage trigger turned on delivers from
    /// then (`enabled_since`).
    ///
    /// Lock order (module docs): the per-branch locks (1, 2, 3), then the trigger row (4b),
    /// then the drop insert (4c), which locks nothing.
    pub async fn update_trigger(
        &self,
        branch: BranchId,
        function_id: Uuid,
        id: Uuid,
        enabled: bool,
    ) -> Result<Option<TriggerRecord>, MetadataError> {
        let mut tx = self.pool.begin().await?;
        lock_branch_function(&mut tx, branch, function_id).await?;
        let row = sqlx::query(sqlx::AssertSqlSafe(format!(
            "UPDATE function_triggers t
             SET enabled = $4,
                 state = CASE WHEN $4 AND NOT t.enabled AND t.source = 'table'
                              THEN 'pending_install' ELSE t.state END,
                 enabled_since = CASE WHEN $4 AND NOT t.enabled THEN clock_timestamp()
                                      ELSE t.enabled_since END,
                 last_error = CASE WHEN $4 AND NOT t.enabled AND t.source = 'table'
                                   THEN NULL ELSE t.last_error END
             WHERE t.id = $3 AND t.branch_id = $1 AND t.function_id = $2
             RETURNING {TRIGGER_COLUMNS}"
        )))
        .bind(branch.as_uuid())
        .bind(function_id)
        .bind(id)
        .bind(enabled)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let record = trigger_from_row(&row)?;
        if !enabled && record.source == TriggerSource::Table {
            record_drop(&mut tx, branch, id).await?;
        }
        tx.commit().await?;
        Ok(Some(record))
    }

    /// m13: deletes the function's trigger on the branch ("drop, then forget": a table
    /// trigger's drop is recorded in the same transaction, and the worker drops its database
    /// trigger and queued events). False for an unknown trigger, or one of another function
    /// or branch. Lock order: as [`Self::update_trigger`].
    pub async fn delete_trigger(
        &self,
        branch: BranchId,
        function_id: Uuid,
        id: Uuid,
    ) -> Result<bool, MetadataError> {
        let mut tx = self.pool.begin().await?;
        lock_branch_function(&mut tx, branch, function_id).await?;
        let source: Option<String> = sqlx::query_scalar(
            "DELETE FROM function_triggers WHERE id = $3 AND branch_id = $1 AND function_id = $2
             RETURNING source",
        )
        .bind(branch.as_uuid())
        .bind(function_id)
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(source) = source else {
            return Ok(false);
        };
        if TriggerSource::parse(&source)? == TriggerSource::Table {
            record_drop(&mut tx, branch, id).await?;
        }
        tx.commit().await?;
        Ok(true)
    }

    /// m13: the branch's recorded drops (`function_trigger_drops` ids), for the worker to
    /// clear ([`Self::clear_trigger_drops`]) once it has synced every database of the branch
    /// after reading them. A read.
    pub async fn trigger_drops(&self, branch: BranchId) -> Result<Vec<i64>, MetadataError> {
        Ok(sqlx::query_scalar(
            "SELECT id FROM function_trigger_drops WHERE branch_id = $1 ORDER BY id",
        )
        .bind(branch.as_uuid())
        .fetch_all(&self.pool)
        .await?)
    }

    /// m13: forgets the drops `ids` of the branch (read before a sync that then completed in
    /// every database). One statement on drop rows (4c) alone.
    pub async fn clear_trigger_drops(
        &self,
        branch: BranchId,
        ids: &[i64],
    ) -> Result<u64, MetadataError> {
        if ids.is_empty() {
            return Ok(0);
        }
        Ok(sqlx::query("DELETE FROM function_trigger_drops WHERE branch_id = $1 AND id = ANY($2)")
            .bind(branch.as_uuid())
            .bind(ids)
            .execute(&self.pool)
            .await?
            .rows_affected())
    }

    /// m13: what the dispatcher saw on the branch: each trigger's dead events are added to
    /// `dead_count` (m13b: and what the outbox bound or the worker's cleanup removed, to
    /// `dropped_count`); a failure sets `last_delivery_error`, a delivery without failure clears
    /// it; a delivery sets `last_delivered_at`. Rows of another branch, or gone, are skipped.
    /// One statement on trigger rows (4b) alone, taking nothing after.
    pub async fn record_trigger_deliveries(
        &self,
        branch: BranchId,
        deliveries: &[TriggerDelivery],
    ) -> Result<u64, MetadataError> {
        if deliveries.is_empty() {
            return Ok(0);
        }
        let ids: Vec<Uuid> = deliveries.iter().map(|d| d.id).collect();
        let delivered: Vec<bool> = deliveries.iter().map(|d| d.delivered).collect();
        let dead: Vec<i64> = deliveries.iter().map(|d| d.dead.max(0)).collect();
        let dropped: Vec<i64> = deliveries.iter().map(|d| d.dropped.max(0)).collect();
        let errors: Vec<Option<String>> = deliveries
            .iter()
            .map(|d| d.error.as_ref().map(|e| e.chars().take(MAX_LAST_ERROR_CHARS).collect()))
            .collect();
        Ok(sqlx::query(
            "UPDATE function_triggers t
             SET dead_count = t.dead_count + d.dead,
                 dropped_count = t.dropped_count + d.dropped,
                 last_delivery_error = CASE WHEN d.error IS NOT NULL THEN d.error
                                            WHEN d.delivered THEN NULL
                                            ELSE t.last_delivery_error END,
                 last_delivered_at = CASE WHEN d.delivered THEN now() ELSE t.last_delivered_at END
             FROM unnest($2::uuid[], $3::bool[], $4::bigint[], $5::text[], $6::bigint[])
                  AS d(id, delivered, dead, error, dropped)
             WHERE t.id = d.id AND t.branch_id = $1",
        )
        .bind(branch.as_uuid())
        .bind(ids)
        .bind(delivered)
        .bind(dead)
        .bind(errors)
        .bind(dropped)
        .execute(&self.pool)
        .await?
        .rows_affected())
    }

    /// m13: up to `limit` (ready branch of a live database, Postgres database) pairs with at
    /// least one `installed`, enabled table trigger: where the dispatcher runs. A read. In a
    /// random order (m13b, N2): past `limit`, no branch always loses.
    pub async fn dispatch_targets(
        &self,
        limit: i64,
    ) -> Result<Vec<(BranchId, String)>, MetadataError> {
        let rows: Vec<(Uuid, String)> = sqlx::query_as(
            "SELECT branch_id, pg_database FROM (
               SELECT DISTINCT t.branch_id, t.pg_database
               FROM function_triggers t JOIN branches b ON b.id = t.branch_id
               JOIN databases d ON d.id = b.database_id
               WHERE t.source = 'table' AND t.state = 'installed' AND t.enabled
                 AND b.status = 'ready' AND d.status NOT IN ('deleting', 'deleted')) targets
             ORDER BY random() LIMIT $1",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|(b, db)| (BranchId(b), db)).collect())
    }

    /// m14: every enabled storage trigger on a live branch of a live database, for the
    /// worker's storage consumers. Whether the function itself is on doesn't matter here: a
    /// disabled function's events wait (the functions service refuses them, so they are
    /// retried). A read.
    pub async fn storage_trigger_targets(&self) -> Result<Vec<StorageTriggerTarget>, MetadataError> {
        let rows = sqlx::query(
            "SELECT t.id, t.branch_id, f.database_id, f.name, t.pg_database, t.bucket, t.prefix,
                    t.events, t.enabled_since
             FROM function_triggers t JOIN functions f ON f.id = t.function_id
             JOIN branches b ON b.id = t.branch_id JOIN databases d ON d.id = b.database_id
             WHERE t.source = 'storage' AND t.enabled
               AND b.status NOT IN ('deleting', 'deleted') AND d.status NOT IN ('deleting', 'deleted')
             ORDER BY t.id",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .map(|r| StorageTriggerTarget {
                id: r.get("id"),
                branch_id: BranchId(r.get("branch_id")),
                database_id: DatabaseId(r.get("database_id")),
                function_name: r.get("name"),
                pg_database: r.get("pg_database"),
                bucket: r.get("bucket"),
                prefix: r.get("prefix"),
                events: r.get("events"),
                enabled_since: r.get("enabled_since"),
            })
            .collect())
    }

    /// Settles the worker's table-trigger installs on `branch` (edge functions m8): each row
    /// still `pending_install` at the `enabled_since` the worker read becomes `installed` (no
    /// error) or `failed` (its error, cut to [`MAX_LAST_ERROR_CHARS`]); an `installed` row
    /// whose database trigger the worker couldn't put back (drift: its table is gone, or the
    /// database was refused) becomes `failed`. A row pended again meanwhile (a new generation),
    /// already failed, or of another branch is left alone. The number of rows settled.
    ///
    /// Lock order (module docs): one statement on trigger rows (4b) alone, taking nothing after.
    pub async fn settle_trigger_installs(
        &self,
        branch: BranchId,
        installs: &[TriggerInstall],
    ) -> Result<u64, MetadataError> {
        if installs.is_empty() {
            return Ok(0);
        }
        let ids: Vec<Uuid> = installs.iter().map(|i| i.id).collect();
        let since: Vec<DateTime<Utc>> = installs.iter().map(|i| i.enabled_since).collect();
        let errors: Vec<Option<String>> = installs
            .iter()
            .map(|i| i.error.as_ref().map(|e| e.chars().take(MAX_LAST_ERROR_CHARS).collect()))
            .collect();
        Ok(sqlx::query(
            "UPDATE function_triggers t
             SET state = CASE WHEN i.error IS NULL THEN 'installed' ELSE 'failed' END,
                 last_error = i.error
             FROM unnest($2::uuid[], $3::timestamptz[], $4::text[]) AS i(id, since, error)
             WHERE t.id = i.id AND t.branch_id = $1 AND t.source = 'table'
               AND (t.state = 'pending_install' OR (t.state = 'installed' AND i.error IS NOT NULL))
               AND t.enabled_since = i.since",
        )
        .bind(branch.as_uuid())
        .bind(ids)
        .bind(since)
        .bind(errors)
        .execute(&self.pool)
        .await?
        .rows_affected())
    }

    /// Up to `limit` ready branches (of live databases) with a table trigger the worker's
    /// sweep looks at (edge functions m8): `pending_install` (to install) or `installed` (to
    /// check for drift), each with whether it has a pending one. A `failed` row never comes
    /// back here until it is pended again (a reset, a restore, or a change to the trigger), so
    /// a refusal is terminal. The worker orders them by its own last attempt. A read.
    pub async fn trigger_sweep_branches(
        &self,
        limit: i64,
    ) -> Result<Vec<(BranchId, bool)>, MetadataError> {
        // m13: a disabled pending row is never installed, so it doesn't count as pending; a
        // branch with a trigger to drop (`function_trigger_drops`) does.
        let rows: Vec<(Uuid, bool)> = sqlx::query_as(
            "SELECT b.id, bool_or(c.pending)
             FROM branches b JOIN databases d ON d.id = b.database_id
             JOIN (SELECT t.branch_id, t.state = 'pending_install' AND t.enabled AS pending
                   FROM function_triggers t
                   WHERE t.source = 'table' AND t.state IN ('pending_install', 'installed')
                   UNION ALL
                   SELECT branch_id, true FROM function_trigger_drops) c ON c.branch_id = b.id
             WHERE b.status = 'ready' AND d.status NOT IN ('deleting', 'deleted')
             GROUP BY b.id ORDER BY b.id LIMIT $1",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|(id, pending)| (BranchId(id), pending)).collect())
    }

    /// Up to `limit` ready `normal` branches (of live databases) with a function that has a
    /// `run_as_role`, no key and no terminal outcome (`run_as_unmintable`): the worker's sweep
    /// mints their run-as keys (edge functions m8). Chosen by state alone, whatever `last_error` says (the functions service overwrites
    /// it on any invocation error); the worker orders them by its own last attempt. A read.
    pub async fn pending_run_as_branches(
        &self,
        limit: i64,
    ) -> Result<Vec<BranchId>, MetadataError> {
        let ids: Vec<Uuid> = sqlx::query_scalar(
            "SELECT b.id FROM branches b JOIN databases d ON d.id = b.database_id
             WHERE b.status = 'ready' AND b.kind = 'normal'
               AND d.status NOT IN ('deleting', 'deleted')
               AND EXISTS (SELECT 1 FROM branch_functions bf WHERE bf.branch_id = b.id
                           AND bf.run_as_role IS NOT NULL AND bf.run_as_key_id IS NULL
                           AND bf.run_as_unmintable IS NULL)
             ORDER BY b.id LIMIT $1",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(ids.into_iter().map(BranchId).collect())
    }

    /// Bundle hashes any deployment of the database still names (for the bundle sweep, with
    /// [`Self::live_database`]: a missing or tombstoned database references nothing).
    pub async fn referenced_bundles(
        &self,
        database_id: DatabaseId,
    ) -> Result<Vec<String>, MetadataError> {
        Ok(sqlx::query_scalar(
            "SELECT DISTINCT d.bundle_sha256 FROM function_deployments d
             JOIN functions f ON f.id = d.function_id
             WHERE f.database_id = $1 AND d.bundle_sha256 IS NOT NULL
             ORDER BY 1",
        )
        .bind(database_id.as_uuid())
        .fetch_all(&self.pool)
        .await?)
    }

    /// The database exists and is neither deleting nor deleted.
    pub async fn live_database(&self, database_id: DatabaseId) -> Result<bool, MetadataError> {
        Ok(sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM databases
                            WHERE id = $1 AND status NOT IN ('deleting', 'deleted'))",
        )
        .bind(database_id.as_uuid())
        .fetch_one(&self.pool)
        .await?)
    }
}
