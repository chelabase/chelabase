//! Edge functions m13b (#186, moved from the worker's m13 dispatcher): database events
//! delivered to functions.
//!
//! m8 (the worker's `fn_triggers`) records each committed row change of a triggered table in
//! the branch database's `chelabase_functions.outbox` and notifies `chelabase_functions`. The
//! dispatcher ([`dispatch`]) holds one connection per (ready branch, Postgres database) with an
//! `installed`, enabled table trigger, as the dedicated login `chelabase_functions_dispatch`,
//! and wakes on `LISTEN` (plus a sweep every [`SWEEP_EVERY`]). Each pass:
//!
//! 1. reads the branch, its triggers and its functions from the metadata and plans ([`plan`]):
//!    a trigger is *delivered* when it is `installed`, enabled, of this database and its
//!    function is enabled on the branch; *held* (its events stay queued) when it is enabled
//!    but still `pending_install` or its function is disabled or not deployed. The task ends
//!    when the branch isn't ready, its timeline changed, or the database has no installed,
//!    enabled trigger left;
//! 2. on a sweep, deletes rows of another timeline and rows of any trigger neither delivered
//!    nor held (a disabled trigger's are counted in its `dropped_count`), and bounds the dead
//!    rows (B1): past [`OUTBOX_CAP`] dead rows, or dead for longer than [`RETENTION`], the
//!    oldest are deleted and counted. A pending row is never deleted by the bound: delivery is
//!    at least once, and a function that stays off keeps its events queued (unbounded; the
//!    limit is the database's own size);
//! 3. claims up to [`CLAIM_BATCH`] due rows with `FOR UPDATE SKIP LOCKED`, counting the attempt
//!    and leasing them for [`LEASE`];
//! 4. vets each row ([`vet`]): outbox rows are untrusted (an owner-level role, a member of
//!    `pg_write_all_data`, can write them), so the row's trigger must be one delivered here,
//!    its timeline the branch's, its schema, table and operation the trigger's, and its records
//!    shaped for the operation. A row that fails is deleted, never delivered;
//! 5. re-reads the branch, the trigger and the function before each invocation (S3): a row of
//!    a trigger deleted or disabled since the claim is deleted, a row of a function turned off
//!    is released; neither is delivered;
//! 6. invokes the function in this process (its `fetch` handler, marked `chelabase-event: db`),
//!    body `{id, type, schema, table, record, old_record, txid}` ([`event_body`]; over
//!    [`MAX_EVENT_BYTES`] the records become null with `truncated: true` and `primary_key`);
//! 7. settles ([`settle`]): a 2xx deletes the row; anything else retries after [`backoff`],
//!    and the [`MAX_ATTEMPTS`]th failure leaves it `dead` (kept, counted in the trigger's
//!    `dead_count`, until the bound removes it), and records what it saw in the metadata.
//!
//! Delivery is at least once: a dispatcher that stops between a delivery and its settle
//! delivers the event again once the lease runs out. `id` (`<timeline>:<database>:<row id>`)
//! identifies an event for dedupe.

pub mod dispatch;

use std::time::Duration;

use chelabase_metadata::{
    BranchFunction, FunctionKind, TriggerRecord, TriggerSource, TriggerState,
};
use serde_json::{Map, Value, json};
use uuid::Uuid;

/// A failed event is retried until this many attempts, then it is `dead`.
pub const MAX_ATTEMPTS: i32 = 10;
/// Rows claimed per pass.
pub const CLAIM_BATCH: i64 = 10;
/// A claimed row is due again after this long when its settle never comes (the dispatcher
/// stopped): longer than a batch of invocations can take.
pub const LEASE: Duration = Duration::from_secs(900);
/// The most dead rows an outbox keeps; the oldest beyond are deleted and counted (B1: never a
/// pending row).
pub const OUTBOX_CAP: i64 = 10_000;
/// Dead rows older than this are deleted and counted.
pub const RETENTION: Duration = Duration::from_secs(7 * 24 * 3600);
/// An event body over this is sent truncated.
pub const MAX_EVENT_BYTES: usize = 256 * 1024;
/// The safety sweep: a pass runs at least this often without a notification (and a deleted
/// branch's task ends within it).
pub const SWEEP_EVERY: Duration = Duration::from_secs(30);
/// How often the supervisor looks for databases to dispatch.
pub const SUPERVISE_EVERY: Duration = Duration::from_secs(5);

/// The delay before attempt `attempts + 1`, after `attempts` failed ones: 1 s, doubling, at
/// most 5 minutes.
pub fn backoff(attempts: i32) -> Duration {
    let exp = attempts.clamp(1, 20) as u32 - 1;
    Duration::from_secs((1u64 << exp).min(300))
}

/// What became of one delivery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The function answered 2xx.
    Delivered,
    /// Anything else: another status, a timeout, no answer.
    Failed(String),
}

/// What happens to a claimed row after its delivery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Settle {
    Delete,
    Retry(Duration),
    Dead,
    /// m13b (S3): not delivered after all (its function was turned off after the claim): due
    /// again at once, the claim's attempt taken back.
    Release,
}

/// The row's fate after its `attempts`th attempt (the claim counted it) ended in `outcome`.
pub fn settle(attempts: i32, outcome: &Outcome) -> Settle {
    match outcome {
        Outcome::Delivered => Settle::Delete,
        Outcome::Failed(_) if attempts >= MAX_ATTEMPTS => Settle::Dead,
        Outcome::Failed(_) => Settle::Retry(backoff(attempts)),
    }
}

/// A trigger whose events this database delivers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Deliverable {
    pub trigger_id: Uuid,
    pub function: String,
    pub schema: String,
    pub table: String,
    pub events: Vec<String>,
}

/// One pass's plan for a database ([`plan`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    /// Whether the task keeps running: some trigger here is `installed` and enabled.
    pub run: bool,
    pub deliver: Vec<Deliverable>,
    /// Triggers whose events stay queued, undelivered.
    pub hold: Vec<Uuid>,
}

/// Plans a pass over `db` (see the module docs) from the branch's triggers and functions.
pub fn plan(db: &str, triggers: &[TriggerRecord], functions: &[BranchFunction]) -> Plan {
    let mut out = Plan::default();
    for t in triggers
        .iter()
        .filter(|t| t.source == TriggerSource::Table && t.enabled && t.pg_database == db)
    {
        let function = functions
            .iter()
            .find(|f| f.function_id == t.function_id && f.kind == FunctionKind::Function);
        match (t.state, function, &t.schema_name, &t.table_name) {
            (TriggerState::Installed, Some(f), Some(schema), Some(table)) if f.enabled => {
                out.run = true;
                out.deliver.push(Deliverable {
                    trigger_id: t.id,
                    function: f.name.clone(),
                    schema: schema.clone(),
                    table: table.clone(),
                    events: t.events.clone(),
                });
            }
            (TriggerState::Installed, ..) => {
                out.run = true;
                out.hold.push(t.id);
            }
            (TriggerState::PendingInstall, ..) => out.hold.push(t.id),
            (TriggerState::Failed, ..) => {}
        }
    }
    out
}

/// A claimed outbox row as the claim prints it (untrusted: see [`vet`]).
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
pub struct Claimed {
    pub id: i64,
    pub trigger_id: Uuid,
    pub attempts: i32,
    pub timeline_id: Option<String>,
    pub payload: Value,
    /// The payload was over [`MAX_EVENT_BYTES`]: the claim left its records out.
    #[serde(default)]
    pub big: bool,
    /// The primary key's values (from the record, else the old record), by column.
    #[serde(default)]
    pub pk: Value,
}

/// A vetted event.
#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    pub id: String,
    pub kind: &'static str,
    pub schema: String,
    pub table: String,
    pub record: Value,
    pub old_record: Value,
    pub txid: Value,
    pub big: bool,
    pub pk: Value,
}

/// Checks a claimed row against the trigger it names (`deliver`, found by its id among those
/// delivered here) and the branch's timeline; the event, or why the row is dropped.
pub fn vet(
    row: &Claimed,
    deliver: Option<&Deliverable>,
    timeline: &str,
    db: &str,
) -> Result<Event, &'static str> {
    let d = deliver.ok_or("not a trigger delivered here")?;
    if d.trigger_id != row.trigger_id {
        return Err("not a trigger delivered here");
    }
    if row.timeline_id.as_deref() != Some(timeline) {
        return Err("another timeline");
    }
    let p = row.payload.as_object().ok_or("not an object")?;
    let text = |k: &str| p.get(k).and_then(Value::as_str);
    if text("schema") != Some(d.schema.as_str()) || text("table") != Some(d.table.as_str()) {
        return Err("another table");
    }
    let kind = match text("type") {
        Some("INSERT") => "INSERT",
        Some("UPDATE") => "UPDATE",
        Some("DELETE") => "DELETE",
        _ => return Err("not an insert, update or delete"),
    };
    if !d.events.iter().any(|e| e.eq_ignore_ascii_case(kind)) {
        return Err("an operation the trigger doesn't name");
    }
    let field = |k: &str, wanted: bool| -> Result<Value, &'static str> {
        match p.get(k).unwrap_or(&Value::Null) {
            Value::Null if !wanted || row.big => Ok(Value::Null),
            v @ Value::Object(_) if wanted && !row.big => Ok(v.clone()),
            _ => Err("records not shaped for the operation"),
        }
    };
    let record = field("record", kind != "DELETE")?;
    let old_record = field("old_record", kind != "INSERT")?;
    let txid = match p.get("txid") {
        None | Some(Value::Null) => Value::Null,
        Some(v) if v.as_u64().is_some() => v.clone(),
        Some(_) => return Err("a bad txid"),
    };
    let pk = match &row.pk {
        v @ Value::Object(_) => v.clone(),
        _ => Value::Null,
    };
    Ok(Event {
        id: format!("{timeline}:{db}:{}", row.id),
        kind,
        schema: d.schema.clone(),
        table: d.table.clone(),
        record,
        old_record,
        txid,
        big: row.big,
        pk,
    })
}

/// The POST body the function's `fetch` handler gets: `{id, type, schema, table, record,
/// old_record, txid}`, or, over [`MAX_EVENT_BYTES`], the records null with `truncated: true`
/// and `primary_key` (the key's values, null for a table without one).
pub fn event_body(e: &Event) -> Vec<u8> {
    let mut body = Map::new();
    body.insert("id".into(), json!(e.id));
    body.insert("type".into(), json!(e.kind));
    body.insert("schema".into(), json!(e.schema));
    body.insert("table".into(), json!(e.table));
    body.insert("record".into(), e.record.clone());
    body.insert("old_record".into(), e.old_record.clone());
    body.insert("txid".into(), e.txid.clone());
    if !e.big {
        let full = serde_json::to_vec(&body).unwrap_or_default();
        if full.len() <= MAX_EVENT_BYTES {
            return full;
        }
    }
    body.insert("record".into(), Value::Null);
    body.insert("old_record".into(), Value::Null);
    body.insert("truncated".into(), json!(true));
    body.insert("primary_key".into(), e.pk.clone());
    serde_json::to_vec(&body).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chelabase_types::BranchId;
    use proptest::prelude::*;

    const TL: &str = "0123456789abcdef0123456789abcdef";

    fn deliverable() -> Deliverable {
        Deliverable {
            trigger_id: Uuid::parse_str("11111111-2222-3333-4444-555555555555").unwrap(),
            function: "hello".into(),
            schema: "public".into(),
            table: "orders".into(),
            events: vec!["insert".into(), "update".into()],
        }
    }

    fn row(payload: Value) -> Claimed {
        Claimed {
            id: 7,
            trigger_id: deliverable().trigger_id,
            attempts: 1,
            timeline_id: Some(TL.into()),
            payload,
            big: false,
            pk: json!({"id": 1}),
        }
    }

    fn insert() -> Value {
        json!({"type": "INSERT", "schema": "public", "table": "orders", "record": {"id": 1}, "old_record": null, "txid": 42})
    }

    fn trigger(state: TriggerState, enabled: bool, function_id: Uuid) -> TriggerRecord {
        TriggerRecord {
            id: Uuid::new_v4(),
            branch_id: BranchId::new(),
            function_id,
            source: TriggerSource::Table,
            enabled,
            state,
            enabled_since: chrono::Utc::now(),
            pg_database: "app".into(),
            schema_name: Some("public".into()),
            table_name: Some("orders".into()),
            bucket: None,
            prefix: None,
            events: vec!["insert".into()],
            last_error: None,
            dead_count: 0,
            dropped_count: 0,
            last_delivery_error: None,
            last_delivered_at: None,
        }
    }

    fn function(id: Uuid, enabled: bool) -> BranchFunction {
        BranchFunction {
            function_id: id,
            name: "hello".into(),
            kind: FunctionKind::Function,
            deployment_id: Uuid::new_v4(),
            enabled,
            auth: chelabase_metadata::FunctionAuth::Key,
            run_as_role: None,
            run_as_key_id: None,
            run_as_role_oid: None,
            run_as_unmintable: None,
            last_error: None,
            updated_at: chrono::Utc::now(),
            health: None,
        }
    }

    /// The untrusted row's checks: its trigger delivered here, its timeline, its table, its
    /// operation among the trigger's, and records shaped for it. The event is built from the
    /// trigger's names, never the row's extra keys.
    #[test]
    fn events_vet_drops_forged_rows() {
        let d = deliverable();
        let ok = vet(&row(insert()), Some(&d), TL, "app").unwrap();
        assert_eq!(
            (ok.id.as_str(), ok.kind, ok.txid.clone()),
            (format!("{TL}:app:7").as_str(), "INSERT", json!(42))
        );
        assert_eq!(
            vet(&row(insert()), None, TL, "app"),
            Err("not a trigger delivered here")
        );
        let mut other = row(insert());
        other.trigger_id = Uuid::new_v4();
        assert_eq!(
            vet(&other, Some(&d), TL, "app"),
            Err("not a trigger delivered here")
        );
        let mut old_tl = row(insert());
        old_tl.timeline_id = Some("ffff".into());
        assert_eq!(vet(&old_tl, Some(&d), TL, "app"), Err("another timeline"));
        old_tl.timeline_id = None;
        assert_eq!(vet(&old_tl, Some(&d), TL, "app"), Err("another timeline"));
        let mut p = insert();
        p["table"] = json!("secrets");
        assert_eq!(vet(&row(p), Some(&d), TL, "app"), Err("another table"));
        let mut p = insert();
        p["schema"] = json!("private");
        assert_eq!(vet(&row(p), Some(&d), TL, "app"), Err("another table"));
        let mut p = insert();
        p["type"] = json!("DELETE");
        p["old_record"] = json!({"id": 1});
        p["record"] = Value::Null;
        assert_eq!(
            vet(&row(p), Some(&d), TL, "app"),
            Err("an operation the trigger doesn't name")
        );
        let mut p = insert();
        p["type"] = json!("TRUNCATE");
        assert_eq!(
            vet(&row(p), Some(&d), TL, "app"),
            Err("not an insert, update or delete")
        );
        let mut p = insert();
        p["old_record"] = json!({"id": 0});
        assert_eq!(
            vet(&row(p), Some(&d), TL, "app"),
            Err("records not shaped for the operation")
        );
        let mut p = insert();
        p["record"] = json!("text");
        assert_eq!(
            vet(&row(p), Some(&d), TL, "app"),
            Err("records not shaped for the operation")
        );
        let mut p = insert();
        p["txid"] = json!("1; drop");
        assert_eq!(vet(&row(p), Some(&d), TL, "app"), Err("a bad txid"));
        assert_eq!(
            vet(&row(json!([1])), Some(&d), TL, "app"),
            Err("not an object")
        );
        let mut p = insert();
        p["extra"] = json!("leak");
        let body: Value =
            serde_json::from_slice(&event_body(&vet(&row(p), Some(&d), TL, "app").unwrap()))
                .unwrap();
        assert_eq!(
            body,
            json!({"id": format!("{TL}:app:7"), "type": "INSERT", "schema": "public", "table": "orders", "record": {"id": 1}, "old_record": null, "txid": 42})
        );
    }

    /// Over 256 KiB: records null, `truncated`, the primary key; the claim's `big` rows too.
    #[test]
    fn events_truncates_over_256_kib() {
        let d = deliverable();
        let mut p = insert();
        p["record"] = json!({"id": 1, "blob": "x".repeat(MAX_EVENT_BYTES)});
        let body: Value =
            serde_json::from_slice(&event_body(&vet(&row(p), Some(&d), TL, "app").unwrap()))
                .unwrap();
        assert_eq!(
            (body["record"].clone(), body["old_record"].clone()),
            (Value::Null, Value::Null)
        );
        assert_eq!(
            (body["truncated"].clone(), body["primary_key"].clone()),
            (json!(true), json!({"id": 1}))
        );
        let mut small = insert();
        small["record"] = json!({"id": 1, "blob": "x".repeat(MAX_EVENT_BYTES - 200)});
        let body: Value =
            serde_json::from_slice(&event_body(&vet(&row(small), Some(&d), TL, "app").unwrap()))
                .unwrap();
        assert!(body.get("truncated").is_none(), "just under: whole");
        let mut big =
            row(json!({"type": "INSERT", "schema": "public", "table": "orders", "txid": 1}));
        big.big = true;
        let body: Value =
            serde_json::from_slice(&event_body(&vet(&big, Some(&d), TL, "app").unwrap())).unwrap();
        assert_eq!(
            (body["truncated"].clone(), body["primary_key"].clone()),
            (json!(true), json!({"id": 1}))
        );
    }

    /// Pending rows are held (nothing delivered for them), a disabled function's are held, a
    /// disabled or failed trigger's are neither (the claim deletes them), and another
    /// database's are ignored. The task runs only while an installed, enabled trigger is here.
    #[test]
    fn events_plan_holds_pending_and_disabled() {
        let f = Uuid::new_v4();
        let installed = trigger(TriggerState::Installed, true, f);
        let pending = trigger(TriggerState::PendingInstall, true, f);
        let failed = trigger(TriggerState::Failed, true, f);
        let off = trigger(TriggerState::Installed, false, f);
        let mut elsewhere = trigger(TriggerState::Installed, true, f);
        elsewhere.pg_database = "other".into();
        let all = [installed.clone(), pending.clone(), failed, off, elsewhere];
        let p = plan("app", &all, &[function(f, true)]);
        assert!(p.run);
        assert_eq!(
            p.deliver.iter().map(|d| d.trigger_id).collect::<Vec<_>>(),
            vec![installed.id]
        );
        assert_eq!(p.hold, vec![pending.id]);
        let p = plan("app", &all, &[function(f, false)]);
        assert!(p.run && p.deliver.is_empty());
        assert_eq!(
            p.hold,
            vec![installed.id, pending.id],
            "a disabled function's events queue"
        );
        let p = plan("app", &all, &[]);
        assert!(p.deliver.is_empty(), "no deployment on the branch: held");
        let p = plan("app", std::slice::from_ref(&pending), &[function(f, true)]);
        assert!(!p.run, "nothing installed: the task stops");
        assert!(
            !plan("app", &[], &[function(f, true)]).run,
            "every trigger deleted: the task stops"
        );
    }

    #[test]
    fn events_backoff_doubles_to_five_minutes() {
        let secs: Vec<u64> = (1..=MAX_ATTEMPTS).map(|n| backoff(n).as_secs()).collect();
        assert_eq!(secs, vec![1, 2, 4, 8, 16, 32, 64, 128, 256, 300]);
        assert_eq!(
            settle(MAX_ATTEMPTS - 1, &Outcome::Failed("500".into())),
            Settle::Retry(Duration::from_secs(256))
        );
        assert_eq!(
            settle(MAX_ATTEMPTS, &Outcome::Failed("500".into())),
            Settle::Dead
        );
        assert_eq!(settle(MAX_ATTEMPTS, &Outcome::Delivered), Settle::Delete);
    }

    /// One delivery attempt in the model: a 2xx, a 5xx, a timeout, or the worker stopping
    /// after the function ran and before the settle (its lease runs out).
    #[derive(Debug, Clone, Copy)]
    enum Step {
        Ok,
        Status5xx,
        Timeout,
        Crash,
    }

    fn step() -> impl Strategy<Value = Step> {
        prop_oneof![
            Just(Step::Ok),
            Just(Step::Status5xx),
            Just(Step::Timeout),
            Just(Step::Crash)
        ]
    }

    #[derive(Debug, Clone)]
    struct Row {
        attempts: i32,
        due: u64,
        dead: bool,
    }

    proptest! {
        /// The dispatcher model (#175): random 2xx/5xx/timeout/crash sequences per event, run
        /// through the claim's semantics (due, pending, attempt counted, lease) and
        /// [`settle`]: every event is delivered at least once or goes dead at its
        /// [`MAX_ATTEMPTS`]th attempt (later only when the worker stopped during that one),
        /// none is retried after it or delivered after a settled 2xx, and a dead one is never
        /// claimed again.
        #[test]
        fn events_model_at_least_once(scripts in prop::collection::vec(prop::collection::vec(step(), 0..24), 1..8)) {
            let mut rows: Vec<Option<Row>> = scripts.iter().map(|_| Some(Row { attempts: 0, due: 0, dead: false })).collect();
            let mut cursor = vec![0usize; scripts.len()];
            let mut delivered = vec![0u32; scripts.len()];
            let mut settled_ok = vec![false; scripts.len()];
            let mut now = 0u64;
            for _ in 0..10_000 {
                let due: Vec<usize> = rows.iter().enumerate()
                    .filter(|(_, r)| r.as_ref().is_some_and(|r| !r.dead && r.due <= now))
                    .map(|(i, _)| i).take(CLAIM_BATCH as usize).collect();
                if due.is_empty() {
                    let next = rows.iter().flatten().filter(|r| !r.dead).map(|r| r.due).min();
                    match next { Some(t) => { now = t; continue } None => break }
                }
                for i in due {
                    prop_assert!(!settled_ok[i], "claimed after a settled 2xx");
                    let r = rows[i].as_mut().unwrap();
                    r.attempts += 1;
                    r.due = now + LEASE.as_secs();
                    // Past the script's end, the function answers 2xx.
                    let s = scripts[i].get(cursor[i]).copied().unwrap_or(Step::Ok);
                    cursor[i] += 1;
                    let outcome = match s {
                        Step::Ok => { delivered[i] += 1; Outcome::Delivered }
                        Step::Status5xx => Outcome::Failed("the function answered 500".into()),
                        Step::Timeout => Outcome::Failed("timed out".into()),
                        Step::Crash => { delivered[i] += 1; continue }
                    };
                    match settle(r.attempts, &outcome) {
                        Settle::Delete => { rows[i] = None; settled_ok[i] = true; }
                        Settle::Retry(after) => {
                            prop_assert!(r.attempts < MAX_ATTEMPTS, "no retry after the last attempt");
                            r.due = now + after.as_secs();
                        }
                        Settle::Dead => {
                            prop_assert!(r.attempts >= MAX_ATTEMPTS, "dead before the last attempt");
                            r.dead = true;
                        }
                        Settle::Release => prop_assert!(false, "settle never releases"),
                    }
                }
                now += 1;
            }
            for (i, r) in rows.iter().enumerate() {
                match r {
                    None => prop_assert!(settled_ok[i] && delivered[i] >= 1),
                    Some(r) => {
                        prop_assert!(r.dead, "every event ends delivered or dead");
                        prop_assert!(r.attempts >= MAX_ATTEMPTS, "dead only after {} attempts", MAX_ATTEMPTS);
                    }
                }
            }
        }
    }
}
