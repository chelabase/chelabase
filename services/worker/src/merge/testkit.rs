//! Test helpers for the merge jobs: catalog documents as `CATALOG_SQL` prints them, and a
//! database with a `dev` branch whose base was recorded.

use chelabase_metadata::BranchRecord;
use chelabase_types::DatabaseId;
use serde_json::{Value, json};

use crate::fakes::{FakeWorker, ready_branch, ready_database};

/// Only the catalog read's first statement has this.
pub const CATALOG_READ: &str = "REPEATABLE READ READ ONLY";
/// Only the admin digest read has this.
pub const DIGEST_READ: &str = "SELECT pg_catalog.md5(";
/// Only the model's catalog read has this (the default-ACL read drops it, P7).
pub const DEFAULT_ACL_FILTER: &str = "IN ('anon', 'authenticated')";
/// The target's digest the fakes answer.
pub const DIGEST: &str = "0123456789abcdef0123456789abcdef";

/// A column: `(attnum, name, type, type oid)`.
pub type Col<'a> = (i16, &'a str, &'a str, u32);

/// A table in `public` owned by the owner.
pub fn table(oid: u32, name: &str, columns: &[Col<'_>]) -> Value {
    let columns: Vec<Value> = columns
        .iter()
        .map(|(attnum, name, ty, type_oid)| {
            json!({
                "attnum": attnum, "name": name, "data_type": ty, "type_oid": type_oid,
                "collation": null, "default_expr": null, "generated": "", "identity": "",
                "identity_sequence": null, "not_null": false, "comment": null, "acl": [],
            })
        })
        .collect();
    json!({
        "oid": oid, "schema": "public", "name": name, "owner": { "oid": 10, "name": "postgres" },
        "comment": null, "rls_enabled": false, "rls_forced": false, "persistence": "p",
        "options": [], "acl": [], "columns": columns,
        "constraints": [], "indexes": [], "policies": [],
    })
}

/// `orders (id bigint)`, the table every test database starts with.
pub fn orders() -> Value {
    table(16400, "orders", &[(1, "id", "bigint", 20)])
}

/// `table` with a btree index `name` on its column `column`.
pub fn with_index(mut table: Value, oid: u32, name: &str, column: &str) -> Value {
    let qualified = format!(
        "{}.{}",
        table["schema"].as_str().unwrap(),
        table["name"].as_str().unwrap()
    );
    table["indexes"].as_array_mut().unwrap().push(json!({
        "oid": oid, "name": name, "method": "btree", "unique": false,
        "nulls_not_distinct": false,
        "definition": format!("CREATE INDEX {name} ON {qualified} USING btree ({column})"),
        "key_count": 1, "include": [], "predicate": null, "options": [], "valid": true,
        "comment": null,
    }));
    table
}

/// A role as `CATALOG_SQL` prints it, with no membership.
pub fn role(oid: u32, name: &str, login: bool, bypass_rls: bool) -> Value {
    json!({ "oid": oid, "name": name, "login": login, "inherit": true,
            "connection_limit": -1, "valid_until": null, "bypass_rls": bypass_rls,
            "member_of": [], "other_attributes": [], "comment": null })
}

/// A catalog document holding `public`, `tables` and the owner.
pub fn catalog(tables: &[Value]) -> String {
    catalog_with_roles(tables, &[])
}

/// [`catalog`] with `roles` besides the owner.
pub fn catalog_with_roles(tables: &[Value], roles: &[Value]) -> String {
    catalog_all(tables, roles, &[])
}

/// An enum `public.<name>` with its values `(oid, label)`, in order.
pub fn enum_type(oid: u32, name: &str, labels: &[(u32, &str)]) -> Value {
    let labels: Vec<Value> = labels
        .iter()
        .map(|(oid, label)| json!({ "oid": oid, "label": label }))
        .collect();
    json!({ "oid": oid, "schema": "public", "name": name,
            "owner": { "oid": 10, "name": "postgres" }, "comment": null, "labels": labels })
}

/// [`catalog_with_roles`] with `enums`. Every role was made in the owner's session, so the owner
/// has Postgres's automatic memberships in each (E22: `chelabase_schema::owner_grants`).
pub fn catalog_all(tables: &[Value], roles: &[Value], enums: &[Value]) -> String {
    let postgres = json!({ "oid": 10, "name": "postgres" });
    let mut owner = role(10, "postgres", true, true);
    let owner_name = chelabase_schema::NamedRole {
        oid: chelabase_schema::Oid(10),
        name: "postgres".into(),
    };
    let grants: Vec<Value> = roles
        .iter()
        .flat_map(|r| {
            let made = chelabase_schema::NamedRole {
                oid: chelabase_schema::Oid(r["oid"].as_u64().unwrap() as u32),
                name: r["name"].as_str().unwrap().to_string(),
            };
            chelabase_schema::owner_grants(&made, &owner_name)
        })
        .map(|m| {
            json!({ "role": { "oid": m.role.oid.0, "name": m.role.name },
                    "grantor": { "oid": m.grantor.oid.0, "name": m.grantor.name },
                    "admin": m.admin, "inherit": m.inherit, "set": m.set })
        })
        .collect();
    owner["member_of"] = Value::Array(grants);
    let mut all = vec![owner];
    all.extend(roles.iter().cloned());
    json!({
        "server_version_num": 170005,
        "schemas": [{ "oid": 2200, "name": "public", "owner": postgres, "comment": null,
                      "acl": [] }],
        "tables": tables,
        "enums": enums,
        "sequences": [],
        "roles": all,
        "default_privileges": [],
        "reported": [],
    })
    .to_string()
}

/// A ready database whose `app` base on `dev` holds `orders`; main and dev.
pub struct Setup {
    pub db: DatabaseId,
    pub main: BranchRecord,
    pub dev: BranchRecord,
}

impl Setup {
    pub fn main_ep(&self) -> &str {
        self.main.endpoint_id.as_deref().unwrap()
    }

    pub fn dev_ep(&self) -> &str {
        self.dev.endpoint_id.as_deref().unwrap()
    }
}

pub async fn setup(w: &FakeWorker) -> Setup {
    let db = ready_database(w).await;
    w.computes
        .answer_in("app", CATALOG_READ, &catalog(&[orders()]));
    let main = w.metadata.main(db);
    let dev = ready_branch(w, db, "dev", main.id).await;
    w.computes.answers_in.lock().unwrap().clear();
    let dev = w.metadata.branch(dev);
    let main = w.metadata.main(db);
    w.computes.answer_at(
        main.endpoint_id.as_deref().unwrap(),
        DIGEST_READ,
        &format!("{DIGEST}\n"),
    );
    // A recovery finds none of the merge's statements running.
    w.computes.answer("SELECT count(*) FROM pg_stat_activity", "0\n");
    // And no marker of a merge's transaction (`chelabase_merge.applied` has no row of it).
    w.computes
        .answer("SELECT count(*) FROM chelabase_merge.applied", "0\n");
    Setup { db, main, dev }
}

/// From now on `ep`'s catalog read answers `tables` (replacing an earlier answer for it).
pub fn answer_catalog(w: &FakeWorker, ep: &str, tables: &[Value]) {
    answer_catalog_with_roles(w, ep, tables, &[]);
}

/// [`answer_catalog`] with `roles` besides the owner.
pub fn answer_catalog_with_roles(w: &FakeWorker, ep: &str, tables: &[Value], roles: &[Value]) {
    w.computes
        .answers_at
        .lock()
        .unwrap()
        .retain(|(e, needle, _)| !(e == ep && needle == CATALOG_READ));
    w.computes
        .answer_at(ep, CATALOG_READ, &catalog_with_roles(tables, roles));
}

/// From now on a catalog read on any endpoint without its own answer (a rehearsal's) answers
/// `tables` and `roles` (replacing an earlier such answer).
pub fn answer_elsewhere(w: &FakeWorker, tables: &[Value], roles: &[Value]) {
    w.computes
        .answers_in
        .lock()
        .unwrap()
        .retain(|(db, needle, _)| !(db == "app" && needle == CATALOG_READ));
    w.computes
        .answer_in("app", CATALOG_READ, &catalog_with_roles(tables, roles));
}

/// `items (id bigint)`: the table dev adds in the merge scenarios.
pub fn items(oid: u32) -> Value {
    table(oid, "items", &[(1, "id", "bigint", 20)])
}

/// The merge scenario: dev adds `items`, an index on `orders` and the role `reporter`. Main as
/// it is before and after the merge (with its own OIDs for what the merge creates).
pub struct Scenario;

impl Scenario {
    pub fn dev_tables() -> Vec<Value> {
        vec![
            with_index(orders(), 16450, "orders_id_idx", "id"),
            items(16500),
        ]
    }

    pub fn dev_roles() -> Vec<Value> {
        vec![role(16460, "reporter", true, false)]
    }

    /// Main after the merge: the objects the merge created have main's own OIDs.
    pub fn merged_tables() -> Vec<Value> {
        vec![
            with_index(orders(), 16700, "orders_id_idx", "id"),
            items(16600),
        ]
    }

    pub fn merged_roles() -> Vec<Value> {
        vec![role(16800, "reporter", true, false)]
    }

    /// A rehearsal after the merge: as main, the role NOLOGIN (P5).
    pub fn rehearsed_roles() -> Vec<Value> {
        vec![role(16800, "reporter", false, false)]
    }
}

/// A `dev` → main merge of the [`Scenario`], computed (a draft with a plan).
pub async fn computed(w: &FakeWorker, s: &Setup) -> uuid::Uuid {
    use chelabase_metadata::{MergeDirection, MergeStatus};
    answer_catalog(w, s.main_ep(), &[orders()]);
    answer_catalog_with_roles(w, s.dev_ep(), &Scenario::dev_tables(), &Scenario::dev_roles());
    let id = w.metadata.insert_merge(
        s.db,
        s.dev.id,
        s.main.id,
        MergeDirection::IntoParent,
        MergeStatus::Computing,
    );
    w.handle_merge_job(
        chelabase_nats::MergeJob::Compute { merge_id: id },
        crate::branch_job::Attempt(1),
    )
    .await
    .unwrap();
    let m = w.metadata.merge_row(id);
    assert_eq!(m.status, MergeStatus::Draft, "{:?}", m.error);
    assert!(m.plan.is_some(), "a plan: {:?}", m.analysis);
    id
}

/// The [`Scenario`] computed, rehearsed and acknowledged; main answers its merged schema from
/// now on (what the apply leaves).
pub async fn rehearsed(w: &FakeWorker) -> (Setup, uuid::Uuid) {
    use chelabase_metadata::MergeStatus;
    let s = setup(w).await;
    let id = computed(w, &s).await;
    answer_elsewhere(w, &Scenario::merged_tables(), &Scenario::rehearsed_roles());
    move_to(w, id, MergeStatus::Rehearsing);
    w.handle_merge_job(
        chelabase_nats::MergeJob::Rehearse { merge_id: id },
        crate::branch_job::Attempt(1),
    )
    .await
    .unwrap();
    let m = w.metadata.merge_row(id);
    assert_eq!(m.status, MergeStatus::Rehearsed, "{:?}", m.error);
    acknowledge_all(w, id);
    answer_catalog_with_roles(
        w,
        s.main_ep(),
        &Scenario::merged_tables(),
        &Scenario::merged_roles(),
    );
    (s, id)
}

/// [`rehearsed`], then moved to `applying` as `applyMerge` does; the apply's pre-check reads
/// main as it was when computed ([`before_apply`]).
pub async fn applying(w: &FakeWorker) -> (Setup, uuid::Uuid) {
    let (s, id) = rehearsed(w).await;
    move_to(w, id, chelabase_metadata::MergeStatus::Applying);
    before_apply(w, s.main_ep(), &[orders()], &[]);
    (s, id)
}

/// A rehearsed merge `id` acknowledged and moved to `applying`: main reads `before` at the
/// pre-check, then `after`.
pub fn applying_from_rehearsed(
    w: &FakeWorker,
    s: &Setup,
    id: uuid::Uuid,
    before: &[Value],
    after: &[Value],
) {
    acknowledge_all(w, id);
    answer_catalog(w, s.main_ep(), after);
    move_to(w, id, chelabase_metadata::MergeStatus::Applying);
    before_apply(w, s.main_ep(), before, &[]);
}

/// The next catalog read on `ep` (the apply's pre-check, I1) answers `tables` and `roles`:
/// the target as the merge was computed against; later reads answer as before (the merged
/// target).
pub fn before_apply(w: &FakeWorker, ep: &str, tables: &[Value], roles: &[Value]) {
    w.computes
        .answer_once_at(ep, CATALOG_READ, &catalog_with_roles(tables, roles));
}

/// Moves merge `id` to `status`, as the API does before publishing a job (claimed now).
pub fn move_to(w: &FakeWorker, id: uuid::Uuid, status: chelabase_metadata::MergeStatus) {
    let mut state = w.metadata.state();
    let m = state.merges.get_mut(&id).unwrap();
    m.status = status;
    m.claimed_at = status.is_working().then(chrono::Utc::now);
}

/// Acknowledges every flagged step of merge `id`'s plan, as the user does before apply.
pub fn acknowledge_all(w: &FakeWorker, id: uuid::Uuid) {
    let mut state = w.metadata.state();
    let m = state.merges.get_mut(&id).unwrap();
    let mut acks: Vec<Value> = Vec::new();
    for step in m.plan.as_ref().unwrap()["steps"].as_array().unwrap() {
        if step["data_loss"] == json!(true) || step["security"] == json!(true) {
            let items = step["items"].as_array().unwrap();
            if items.is_empty() {
                acks.push(step["id"].clone());
            }
            acks.extend(items.iter().cloned());
        }
    }
    m.acknowledgements = Value::Array(acks);
}

/// The ids of merge `id`'s plan steps in `phase` (`roles`, `enum_add`, `tx`, `online`).
pub fn step_ids(w: &FakeWorker, id: uuid::Uuid, phase: &str) -> Vec<String> {
    w.metadata.merge_row(id).plan.unwrap()["steps"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|s| s["phase"] == json!(phase))
        .map(|s| s["id"].as_str().unwrap().to_string())
        .collect()
}

/// Every endpoint other than main's and dev's that ran SQL (rehearsals'), in order: a torn-down
/// branch no longer names its endpoint.
pub fn other_endpoints(w: &FakeWorker, s: &Setup) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    for (ep, _, _) in w.computes.sql.lock().unwrap().iter() {
        if ep != s.main_ep() && ep != s.dev_ep() && !found.contains(ep) {
            found.push(ep.clone());
        }
    }
    found
}

/// The merge's rehearsal branches (any status), oldest first.
pub fn rehearsals(w: &FakeWorker, db: DatabaseId) -> Vec<BranchRecord> {
    let mut found: Vec<BranchRecord> = w
        .metadata
        .state()
        .branches
        .values()
        .filter(|b| b.database_id == db && b.kind == chelabase_metadata::BranchKind::Rehearsal)
        .cloned()
        .collect();
    found.sort_by_key(|b| b.created_at);
    found
}
