//! Opt-in tests against a real metadata Postgres (`just deps-up` first):
//!
//! ```sh
//! cargo test -p cheladb-metadata --test live_db -- --ignored --test-threads=1
//! ```
//!
//! They use the seeded demo tenant/project and delete the rows they create.

use cheladb_metadata::{
    BranchDeleteOutcome, BranchKind, BranchRecord, BranchStatus, CreateBranchOutcome, DatabaseSettings, DatabaseStatus,
    MetadataClient, MetadataConfig, MetadataError, NewBranch, NewDataKey, NewEndpoint, SettingsPatch,
    DEMO_PROJECT_ID, DEMO_TENANT_ID,
};
use cheladb_types::data_key::{self, DataKeyKind};
use cheladb_types::{BranchId, DatabaseId, ProjectId, TenantId};

async fn client() -> MetadataClient {
    let database_url = std::env::var("CHELADB_METADATA_DATABASE_URL").unwrap_or_else(|_| {
        "postgres://cheladb:cheladb@127.0.0.1:5433/cheladb_metadata".to_string()
    });
    let client = MetadataClient::connect(&MetadataConfig { database_url })
        .await
        .expect("metadata Postgres reachable (just deps-up)");
    client.migrate().await.unwrap();
    sweep_leftovers(&client).await;
    client
}

/// Deletes the databases these tests made (with their branches and endpoints): a test that
/// panics skips its cleanup, and the fake sealed values it leaves make the key check refuse the
/// dev stack's key. Only the harness's own: in the demo project, named `live-test-<its own id>`
/// as [`insert_pending`] names them, so a database made by hand survives. Safe because the
/// tests run one at a time (`--test-threads=1`). One transaction, in the lock order: the
/// database rows, then their branch rows, then endpoints.
async fn sweep_leftovers(client: &MetadataClient) {
    let mut tx = client.pool().begin().await.unwrap();
    // FOR UPDATE, not the usual FOR NO KEY UPDATE: the rows are deleted below anyway.
    let ids: Vec<uuid::Uuid> = sqlx::query_scalar(
        "SELECT id FROM databases
         WHERE project_id = $1 AND name = 'live-test-' || id::text
         ORDER BY id FOR UPDATE",
    )
    .bind(DEMO_PROJECT_ID)
    .fetch_all(&mut *tx)
    .await
    .unwrap();
    if ids.is_empty() {
        return;
    }
    let endpoints: Vec<String> = sqlx::query_scalar(
        "SELECT endpoint_id FROM branches
         WHERE database_id = ANY($1) AND endpoint_id IS NOT NULL
         ORDER BY id FOR UPDATE",
    )
    .bind(&ids)
    .fetch_all(&mut *tx)
    .await
    .unwrap();
    sqlx::query("DELETE FROM neon_endpoints WHERE id = ANY($1)")
        .bind(&endpoints)
        .execute(&mut *tx)
        .await
        .unwrap();
    for sql in [
        "DELETE FROM branches WHERE database_id = ANY($1)",
        "DELETE FROM databases WHERE id = ANY($1)",
    ] {
        sqlx::query(sql).bind(&ids).execute(&mut *tx).await.unwrap();
    }
    tx.commit().await.unwrap();
    eprintln!("swept {} leftover live-test database(s)", ids.len());
}

async fn insert_pending(client: &MetadataClient) -> DatabaseId {
    let id = DatabaseId::new();
    client
        .insert_database_pending(
            id,
            TenantId(DEMO_TENANT_ID),
            ProjectId(DEMO_PROJECT_ID),
            &format!("live-test-{id}"),
        )
        .await
        .unwrap();
    id
}

/// Deletes the database row, its branches and every endpoint still linked to them.
async fn cleanup(client: &MetadataClient, id: DatabaseId) {
    sqlx::query(
        "DELETE FROM neon_endpoints WHERE id IN (SELECT endpoint_id FROM branches WHERE database_id = $1)",
    )
    .bind(id.as_uuid())
    .execute(client.pool())
    .await
    .unwrap();
    sqlx::query("DELETE FROM branches WHERE database_id = $1")
        .bind(id.as_uuid())
        .execute(client.pool())
        .await
        .unwrap();
    sqlx::query("DELETE FROM databases WHERE id = $1")
        .bind(id.as_uuid())
        .execute(client.pool())
        .await
        .unwrap();
}

fn hex32() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

fn ep_id() -> String {
    format!("ep-{}", &hex32()[..12])
}

fn main_of(id: DatabaseId) -> BranchId {
    BranchId::from(id)
}

fn new_endpoint<'a>(id: &'a str, tenant: &'a str, timeline: &'a str) -> NewEndpoint<'a> {
    NewEndpoint {
        id,
        tenant_id: tenant,
        timeline_id: timeline,
        compute_token_sha256: "cd",
        owner_role: "postgres",
        owner_verifier: "SCRAM-SHA-256$v1",
        database_name: "db",
    }
}

/// Creates an endpoint on a fresh timeline and links it to `branch`; returns its id.
async fn link_branch(client: &MetadataClient, branch: BranchId) -> String {
    let ep = ep_id();
    let (tenant, timeline) = (hex32(), hex32());
    assert!(client
        .create_endpoint_for_branch(branch, &timeline, &new_endpoint(&ep, &tenant, &timeline))
        .await
        .unwrap());
    ep
}

/// Creates and links main's endpoint; returns its id.
async fn link_endpoint(client: &MetadataClient, id: DatabaseId) -> String {
    link_branch(client, main_of(id)).await
}

async fn endpoint_exists(client: &MetadataClient, ep: &str) -> bool {
    sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM neon_endpoints WHERE id = $1)")
        .bind(ep)
        .fetch_one(client.pool())
        .await
        .unwrap()
}

/// What provisioning does once main's compute answers: claim, then ready with `sealed`.
async fn make_ready(client: &MetadataClient, id: DatabaseId, sealed: &str) -> bool {
    client.set_status(id, DatabaseStatus::Provisioning).await.unwrap();
    client.set_branch_ready(main_of(id), sealed, None, None).await.unwrap()
}

/// A ready database whose main has an endpoint; returns the database and endpoint ids.
async fn ready_database(client: &MetadataClient) -> (DatabaseId, String) {
    let id = insert_pending(client).await;
    let ep = link_endpoint(client, id).await;
    assert!(make_ready(client, id, "v1:owner").await);
    (id, ep)
}

async fn branch(client: &MetadataClient, id: BranchId) -> BranchRecord {
    client.get_branch(id).await.unwrap().expect("branch row")
}

async fn main_branch(client: &MetadataClient, id: DatabaseId) -> BranchRecord {
    branch(client, main_of(id)).await
}

// Review Focus 1: a delete that lands mid-provision must stick.
#[tokio::test]
#[ignore]
async fn provisioning_writes_never_resurrect_a_tombstone() {
    let client = client().await;
    let id = insert_pending(&client).await;
    assert!(client.begin_delete(id).await.unwrap());
    client.mark_branch_deleted(main_of(id)).await.unwrap();
    client.mark_deleted(id).await.unwrap();

    let status_applied = client.set_status(id, DatabaseStatus::Provisioning).await.unwrap();
    let ready_applied = client.set_branch_ready(main_of(id), "v1:owner", None, None).await.unwrap();
    client.set_failed(id, "boom").await.unwrap();

    let row = client.get_database(id).await.unwrap().unwrap();
    let main = main_branch(&client, id).await;
    cleanup(&client, id).await;
    assert!(!status_applied, "set_status overwrote a tombstone");
    assert!(!ready_applied, "set_branch_ready overwrote a tombstone");
    assert_eq!(row.status, DatabaseStatus::Deleted);
    assert_eq!(row.error, None);
    assert_eq!(main.status, BranchStatus::Deleted);
    assert_eq!(main.owner_password_enc, None);
    assert_eq!(main.error, None);
}

#[tokio::test]
#[ignore]
async fn provisioning_writes_still_apply_while_deleting_has_not_started() {
    let client = client().await;
    let id = insert_pending(&client).await;

    let ep = link_endpoint(&client, id).await;
    let status_applied = client.set_status(id, DatabaseStatus::Provisioning).await.unwrap();
    let claimed = main_branch(&client, id).await;
    let ready_applied = client.set_branch_ready(main_of(id), "v1:owner", None, None).await.unwrap();

    let row = client.get_database(id).await.unwrap().unwrap();
    let main = main_branch(&client, id).await;
    cleanup(&client, id).await;
    assert!(status_applied);
    assert_eq!(claimed.status, BranchStatus::Provisioning, "main is claimed with its database");
    assert!(ready_applied);
    assert_eq!(row.status, DatabaseStatus::Ready);
    assert_eq!(row.database_name.as_deref(), Some("db"));
    assert_eq!(row.owner_role.as_deref(), Some("postgres"));
    assert_eq!(main.status, BranchStatus::Ready);
    assert_eq!(main.endpoint_id.as_deref(), Some(ep.as_str()));
    assert_eq!(main.owner_password_enc.as_deref(), Some("v1:owner"));
}

async fn backdate(client: &MetadataClient, id: DatabaseId, secs: i64) {
    sqlx::query("UPDATE databases SET updated_at = now() - make_interval(secs => $2) WHERE id = $1")
        .bind(id.as_uuid())
        .bind(secs as f64)
        .execute(client.pool())
        .await
        .unwrap();
}

// Review fix: reconcile must re-check staleness at write time, not trust its snapshot.
#[tokio::test]
#[ignore]
async fn fail_if_stale_only_fails_rows_that_are_still_stale() {
    let client = client().await;

    let stale = insert_pending(&client).await;
    backdate(&client, stale, 700).await;
    let fresh = insert_pending(&client).await;
    let revived = insert_pending(&client).await;
    backdate(&client, revived, 700).await;
    // The provision consumer picked it up after reconcile's snapshot.
    client.set_status(revived, DatabaseStatus::Provisioning).await.unwrap();

    let stale_failed = client.fail_if_stale(stale, 600).await.unwrap();
    let fresh_failed = client.fail_if_stale(fresh, 600).await.unwrap();
    let revived_failed = client.fail_if_stale(revived, 600).await.unwrap();
    let statuses = [
        client.get_database(stale).await.unwrap().unwrap().status,
        client.get_database(fresh).await.unwrap().unwrap().status,
        client.get_database(revived).await.unwrap().unwrap().status,
    ];
    let mains = [
        main_branch(&client, stale).await,
        main_branch(&client, fresh).await,
        main_branch(&client, revived).await,
    ];
    for id in [stale, fresh, revived] {
        cleanup(&client, id).await;
    }
    assert!(stale_failed);
    assert!(!fresh_failed);
    assert!(!revived_failed);
    assert_eq!(
        statuses,
        [DatabaseStatus::Failed, DatabaseStatus::Pending, DatabaseStatus::Provisioning]
    );
    // Main follows its database while provisioning (ruling 4).
    assert_eq!(
        mains.each_ref().map(|b| b.status),
        [BranchStatus::Failed, BranchStatus::Pending, BranchStatus::Provisioning]
    );
    assert_eq!(mains[0].error.as_deref(), Some("provisioning timed out"));
}

/// The environment's real key-check value, restored after a key-check test so
/// running these tests never locks the local services out.
async fn saved_key_check(client: &MetadataClient) -> Option<String> {
    sqlx::query_scalar("SELECT value FROM control_settings WHERE key = 'secrets_key_check'")
        .fetch_optional(client.pool())
        .await
        .unwrap()
}

async fn restore_key_check(client: &MetadataClient, saved: Option<String>) {
    sqlx::query("DELETE FROM control_settings WHERE key = 'secrets_key_check'")
        .execute(client.pool())
        .await
        .unwrap();
    if let Some(value) = saved {
        sqlx::query("INSERT INTO control_settings (key, value) VALUES ('secrets_key_check', $1)")
            .bind(value)
            .execute(client.pool())
            .await
            .unwrap();
    }
}

// Review fix: a lost or replaced secrets key must be caught at startup.
#[tokio::test]
#[ignore]
async fn key_check_records_matches_and_refuses_a_different_key_while_credentials_exist() {
    use cheladb_metadata::KeyCheck;
    let client = client().await;
    let saved = saved_key_check(&client).await;
    sqlx::query("DELETE FROM control_settings WHERE key = 'secrets_key_check'")
        .execute(client.pool())
        .await
        .unwrap();

    let first = client.ensure_key_check("check-a", |v| v == "check-a").await.unwrap();
    let same = client.ensure_key_check("check-a2", |v| v == "check-a").await.unwrap();

    // Stored credentials exist (on a branch now): a different key must be refused.
    let id = insert_pending(&client).await;
    sqlx::query("UPDATE branches SET owner_password_enc = 'v1:x' WHERE id = $1")
        .bind(id.as_uuid())
        .execute(client.pool())
        .await
        .unwrap();
    let other = client.ensure_key_check("check-b", |v| v == "check-b").await.unwrap();
    cleanup(&client, id).await;
    restore_key_check(&client, saved).await;

    assert_eq!(first, KeyCheck::Recorded);
    assert_eq!(same, KeyCheck::Matches);
    assert!(matches!(other, KeyCheck::Mismatch { sealed_rows } if sealed_rows >= 1));
}

#[tokio::test]
#[ignore]
async fn key_check_adopts_a_new_key_when_nothing_is_encrypted_yet() {
    use cheladb_metadata::KeyCheck;
    let client = client().await;
    let sealed: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM branches WHERE owner_password_enc IS NOT NULL",
    )
    .fetch_one(client.pool())
    .await
    .unwrap();
    if sealed > 0 {
        eprintln!("skipping: {sealed} branches hold encrypted credentials in this environment");
        return;
    }
    let saved = saved_key_check(&client).await;
    client.ensure_key_check("check-a", |v| v == "check-a").await.unwrap();
    let replaced = client.ensure_key_check("check-b", |v| v == "check-b").await.unwrap();
    let again = client.ensure_key_check("check-b2", |v| v == "check-b").await.unwrap();
    restore_key_check(&client, saved).await;
    assert_eq!(replaced, KeyCheck::Replaced);
    assert_eq!(again, KeyCheck::Matches);
}

// A reset finishing after a delete must not write a password back onto the tombstone, and a
// half-applied reset must never leave the sealed password and the verifier out of step.
#[tokio::test]
#[ignore]
async fn replace_owner_secret_is_atomic_and_needs_ready() {
    let client = client().await;
    let verifier = |ep: &str| {
        let ep = ep.to_string();
        let pool = client.pool().clone();
        async move {
            sqlx::query_as::<_, (String, bool)>(
                "SELECT r.scram_verifier, e.needs_reconfigure FROM neon_roles r
                 JOIN neon_endpoints e ON e.id = r.endpoint_id
                 WHERE r.endpoint_id = $1 AND r.name = 'postgres'",
            )
            .bind(ep)
            .fetch_one(&pool)
            .await
            .unwrap()
        }
    };

    // Linked but still provisioning: refused, nothing changes.
    let not_ready = insert_pending(&client).await;
    let ep_not_ready = link_endpoint(&client, not_ready).await;
    let refused = client
        .replace_owner_secret(main_of(not_ready), &ep_not_ready, "postgres", "SCRAM-SHA-256$v2", "v1:new")
        .await
        .unwrap();
    let untouched = verifier(&ep_not_ready).await;
    let untouched_row = main_branch(&client, not_ready).await;

    // Ready and linked: everything changes together.
    let (ok, ep_ok) = ready_database(&client).await;
    let stored = client
        .replace_owner_secret(main_of(ok), &ep_ok, "postgres", "SCRAM-SHA-256$v2", "v1:new")
        .await
        .unwrap();
    let changed = verifier(&ep_ok).await;
    let changed_row = main_branch(&client, ok).await;

    // Wrong endpoint, and an unknown role: refused, nothing changes.
    let wrong_ep = client
        .replace_owner_secret(main_of(ok), "ep-ffffffffffff", "postgres", "SCRAM-SHA-256$v3", "v1:x")
        .await
        .unwrap();
    let unknown_role = client
        .replace_owner_secret(main_of(ok), &ep_ok, "nobody", "SCRAM-SHA-256$v3", "v1:x")
        .await
        .unwrap();
    let after = main_branch(&client, ok).await;
    let after_verifier = verifier(&ep_ok).await;

    // Deleted: refused because the status is no longer ready.
    let (gone, ep_gone) = ready_database(&client).await;
    assert!(client.begin_delete(gone).await.unwrap());
    client.mark_branch_deleted(main_of(gone)).await.unwrap();
    client.mark_deleted(gone).await.unwrap();
    let on_tombstone = client
        .replace_owner_secret(main_of(gone), &ep_gone, "postgres", "SCRAM-SHA-256$v2", "v1:new")
        .await
        .unwrap();
    let tombstone = main_branch(&client, gone).await;

    for id in [not_ready, ok, gone] {
        cleanup(&client, id).await;
    }
    assert!(!refused);
    assert_eq!(untouched, ("SCRAM-SHA-256$v1".to_string(), false));
    assert_eq!(untouched_row.owner_password_enc, None);
    assert!(stored);
    assert_eq!(changed, ("SCRAM-SHA-256$v2".to_string(), true));
    assert_eq!(changed_row.owner_password_enc.as_deref(), Some("v1:new"));
    assert!(!wrong_ep);
    assert!(!unknown_role);
    assert_eq!(after.owner_password_enc.as_deref(), Some("v1:new"));
    assert_eq!(after_verifier, ("SCRAM-SHA-256$v2".to_string(), true));
    assert!(!on_tombstone);
    assert_eq!(tombstone.owner_password_enc, None);
}

// A backup has no compute: its password is never replaced (it has none).
#[tokio::test]
#[ignore]
async fn replace_owner_secret_refuses_a_backup() {
    let client = client().await;
    let (id, ep) = ready_database(&client).await;
    let backup = BranchId::new();
    // Force the shape: a ready backup pointing at main's endpoint would be a bug elsewhere;
    // the guard must still refuse it.
    sqlx::query(
        "INSERT INTO branches (id, database_id, name, parent_branch_id, kind, status)
         VALUES ($1, $2, 'b', $2, 'backup', 'ready')",
    )
    .bind(backup.as_uuid())
    .bind(id.as_uuid())
    .execute(client.pool())
    .await
    .unwrap();
    sqlx::query("UPDATE branches SET endpoint_id = NULL WHERE id = $1")
        .bind(id.as_uuid())
        .execute(client.pool())
        .await
        .unwrap();
    sqlx::query("UPDATE branches SET endpoint_id = $2 WHERE id = $1")
        .bind(backup.as_uuid())
        .bind(&ep)
        .execute(client.pool())
        .await
        .unwrap();
    let replaced = client
        .replace_owner_secret(backup, &ep, "postgres", "SCRAM-SHA-256$v2", "v1:new")
        .await
        .unwrap();
    let row = branch(&client, backup).await;
    cleanup(&client, id).await;
    assert!(!replaced);
    assert_eq!(row.owner_password_enc, None);
}

// Step 4's migration, re-run on step 4's own starting shapes in a scratch schema: the columns
// its UPDATE reads are gone from the live schema since step 5.
#[tokio::test]
#[ignore]
async fn migration_tombstones_rows_without_an_endpoint() {
    let pool = raw_pool().await;
    let schema = format!("step4_migration_{}", hex32());
    let mut conn = pool.acquire().await.unwrap();
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "CREATE SCHEMA {schema}; SET search_path TO {schema};"
    )))
    .execute(&mut *conn)
    .await
    .unwrap();
    sqlx::raw_sql(
        "CREATE TABLE neon_endpoints (
             id text PRIMARY KEY, tenant_id text NOT NULL, timeline_id text NOT NULL,
             compute_token_sha256 text NOT NULL, created_at timestamptz NOT NULL DEFAULT now()
         );
         CREATE TABLE databases (
             id uuid PRIMARY KEY, project_id uuid NOT NULL, name text NOT NULL,
             status text NOT NULL, container_id text, host text, port int, error text,
             created_at timestamptz NOT NULL DEFAULT now(),
             updated_at timestamptz NOT NULL DEFAULT now(),
             database_name text, owner_role text, owner_password_enc text,
             platform_password_enc text, deleted_at timestamptz
         );",
    )
    .execute(&mut *conn)
    .await
    .unwrap();
    let (legacy, linked) = (uuid::Uuid::new_v4(), uuid::Uuid::new_v4());
    let ep = ep_id();
    sqlx::query(
        "INSERT INTO databases (id, project_id, name, status, owner_password_enc)
         VALUES ($1, $3, 'legacy', 'ready', 'v1:old'), ($2, $3, 'linked', 'ready', 'v1:owner')",
    )
    .bind(legacy)
    .bind(linked)
    .bind(uuid::Uuid::new_v4())
    .execute(&mut *conn)
    .await
    .unwrap();
    sqlx::query("INSERT INTO neon_endpoints VALUES ($1, $2, $3, 'cd')")
        .bind(&ep)
        .bind(hex32())
        .bind(hex32())
        .execute(&mut *conn)
        .await
        .unwrap();
    // Step 4's own statements link the endpoint after adding the column; do the same by
    // running its first statement, linking, then the rest.
    let step4 = include_str!("../migrations/20260929000001_neon_provisioning.sql");
    let (add_column, rest) = step4.split_once(";\n").unwrap();
    let outcome = async {
        sqlx::raw_sql(sqlx::AssertSqlSafe(add_column.to_string())).execute(&mut *conn).await?;
        sqlx::query("UPDATE databases SET endpoint_id = $2 WHERE id = $1")
            .bind(linked)
            .bind(&ep)
            .execute(&mut *conn)
            .await?;
        sqlx::raw_sql(sqlx::AssertSqlSafe(rest.to_string())).execute(&mut *conn).await?;
        sqlx::query_as::<_, (uuid::Uuid, String, Option<String>, bool)>(
            "SELECT id, status, owner_password_enc, deleted_at IS NOT NULL FROM databases ORDER BY name",
        )
        .fetch_all(&mut *conn)
        .await
    }
    .await;
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "SET search_path TO public; DROP SCHEMA {schema} CASCADE;"
    )))
    .execute(&mut *conn)
    .await
    .unwrap();

    let rows = outcome.expect("step 4's migration applies");
    assert_eq!(
        rows,
        vec![
            (legacy, "deleted".to_string(), None, true),
            (linked, "ready".to_string(), Some("v1:owner".to_string()), false),
        ]
    );
}

#[tokio::test]
#[ignore]
async fn create_endpoint_links_and_refuses_a_deleting_database() {
    let client = client().await;
    let id = insert_pending(&client).await;
    let ep = ep_id();
    let (tenant, timeline) = (hex32(), hex32());
    let created = client
        .create_endpoint_for_branch(main_of(id), &timeline, &new_endpoint(&ep, &tenant, &timeline))
        .await
        .unwrap();
    let main = main_branch(&client, id).await;
    let roles = client.neon_roles(&ep).await.unwrap();
    let dbs = client.neon_databases(&ep).await.unwrap();
    let endpoint = client.neon_endpoint(&ep).await.unwrap().unwrap();

    // Already linked: refused, and the second endpoint isn't written.
    let ep2 = ep_id();
    let again = client
        .create_endpoint_for_branch(main_of(id), &timeline, &new_endpoint(&ep2, &tenant, &timeline))
        .await
        .unwrap();
    let ep2_written = endpoint_exists(&client, &ep2).await;

    // Deleting, deleted and missing branches are refused without writing anything.
    let deleting = insert_pending(&client).await;
    assert!(client.begin_delete(deleting).await.unwrap());
    let deleted = insert_pending(&client).await;
    assert!(client.begin_delete(deleted).await.unwrap());
    client.mark_branch_deleted(main_of(deleted)).await.unwrap();
    client.mark_deleted(deleted).await.unwrap();
    let mut refused = Vec::new();
    for target in [main_of(deleting), main_of(deleted), BranchId::new()] {
        let ep = ep_id();
        let ok = client
            .create_endpoint_for_branch(target, &timeline, &new_endpoint(&ep, &tenant, &timeline))
            .await
            .unwrap();
        refused.push((ok, endpoint_exists(&client, &ep).await));
    }
    for id in [id, deleting, deleted] {
        cleanup(&client, id).await;
    }

    assert!(created);
    assert_eq!(main.endpoint_id.as_deref(), Some(ep.as_str()));
    assert_eq!(main.timeline_id.as_deref(), Some(timeline.as_str()));
    assert_eq!(endpoint.tenant_id, tenant);
    assert_eq!(endpoint.timeline_id, timeline);
    assert_eq!(endpoint.compute_token_sha256, "cd");
    assert_eq!(roles.len(), 1);
    assert_eq!((roles[0].name.as_str(), roles[0].scram_verifier.as_str()), ("postgres", "SCRAM-SHA-256$v1"));
    assert_eq!((dbs[0].name.as_str(), dbs[0].owner.as_str()), ("db", "postgres"));
    assert!(!again);
    assert!(!ep2_written);
    assert_eq!(refused, vec![(false, false); 3]);
}

#[tokio::test]
#[ignore]
async fn delete_endpoint_cascades_and_unlinks() {
    let client = client().await;
    let (id, ep) = ready_database(&client).await;

    client.delete_endpoint(&ep).await.unwrap();
    client.delete_endpoint(&ep).await.unwrap(); // missing is fine

    let row = client.get_database(id).await.unwrap().unwrap();
    let main = main_branch(&client, id).await;
    let roles = client.neon_roles(&ep).await.unwrap();
    let dbs = client.neon_databases(&ep).await.unwrap();
    let exists = endpoint_exists(&client, &ep).await;
    cleanup(&client, id).await;
    assert!(!exists);
    assert!(roles.is_empty() && dbs.is_empty());
    assert_eq!(main.endpoint_id, None);
    assert_eq!(main.status, BranchStatus::Ready);
    assert_eq!(row.status, DatabaseStatus::Ready);
}

#[tokio::test]
#[ignore]
async fn list_endpoints_for_reconcile_joins_status() {
    let client = client().await;
    let (linked, ep_linked) = ready_database(&client).await;
    let orphan = ep_id();
    let (tenant, timeline) = (hex32(), hex32());
    sqlx::query("INSERT INTO neon_endpoints (id, tenant_id, timeline_id, compute_token_sha256) VALUES ($1, $2, $3, 'aa')")
        .bind(&orphan)
        .bind(&tenant)
        .bind(&timeline)
        .execute(client.pool())
        .await
        .unwrap();
    let deleting = insert_pending(&client).await;
    let ep_deleting = link_endpoint(&client, deleting).await;
    assert!(client.begin_delete(deleting).await.unwrap());
    client
        .replace_owner_secret(main_of(linked), &ep_linked, "postgres", "SCRAM-SHA-256$v2", "v1:n")
        .await
        .unwrap();

    let all = client.list_endpoints_for_reconcile().await.unwrap();
    let find = |id: &str| all.iter().find(|e| e.id == id).cloned().unwrap();
    let (a, b, c) = (find(&ep_linked), find(&orphan), find(&ep_deleting));
    sqlx::query("DELETE FROM neon_endpoints WHERE id = $1")
        .bind(&orphan)
        .execute(client.pool())
        .await
        .unwrap();
    cleanup(&client, linked).await;
    cleanup(&client, deleting).await;

    assert_eq!(a.branch_id, Some(main_of(linked)));
    assert_eq!(a.branch_status, Some(BranchStatus::Ready));
    assert!(a.needs_reconfigure);
    assert_eq!(b.branch_id, None);
    assert_eq!(b.branch_status, None);
    assert_eq!(b.tenant_id, tenant);
    assert!(!b.needs_reconfigure);
    assert_eq!(c.branch_status, Some(BranchStatus::Deleting));
}

// Studio's list comes from tenant_overview; tombstones must never show up there.
#[tokio::test]
#[ignore]
async fn tenant_overview_hides_deleted_databases() {
    let client = client().await;
    let live = insert_pending(&client).await;
    let deleted = insert_pending(&client).await;
    assert!(client.begin_delete(deleted).await.unwrap());
    client.mark_deleted(deleted).await.unwrap();

    let overview = client.tenant_overview(TenantId(DEMO_TENANT_ID)).await.unwrap().unwrap();
    let listed: Vec<DatabaseId> = overview
        .projects
        .iter()
        .flat_map(|p| p.databases.iter().map(|d| d.database_id))
        .collect();
    cleanup(&client, live).await;
    cleanup(&client, deleted).await;
    assert!(listed.contains(&live));
    assert!(!listed.contains(&deleted));
}

// --- Step 5: branches ----------------------------------------------------------------------

/// A pool that doesn't run the migrations (for scratch-schema tests).
async fn raw_pool() -> sqlx::PgPool {
    let database_url = std::env::var("CHELADB_METADATA_DATABASE_URL").unwrap_or_else(|_| {
        "postgres://cheladb:cheladb@127.0.0.1:5433/cheladb_metadata".to_string()
    });
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&database_url)
        .await
        .expect("metadata Postgres reachable (just deps-up)")
}

fn new_branch<'a>(db: DatabaseId, name: &'a str, parent: BranchId) -> NewBranch<'a> {
    NewBranch {
        id: BranchId::new(),
        database_id: db,
        name,
        parent_branch_id: parent,
        branched_at_time: None,
        branched_at_lsn: None,
    }
}

/// Creates `name` under `parent`, links an endpoint and makes it ready; returns its id.
async fn ready_branch(client: &MetadataClient, db: DatabaseId, name: &str, parent: BranchId) -> BranchId {
    let b = new_branch(db, name, parent);
    assert_eq!(client.create_branch(&b).await.unwrap(), CreateBranchOutcome::Created);
    link_branch(client, b.id).await;
    assert!(client
        .set_branch_status(b.id, &[BranchStatus::Pending], BranchStatus::Provisioning)
        .await
        .unwrap());
    assert!(client.set_branch_ready(b.id, "v1:branch", Some("0/1A2B3C"), None).await.unwrap());
    b.id
}

/// Moves `id` to resetting with a pending timeline; returns that timeline.
async fn start_reset(client: &MetadataClient, id: BranchId) -> String {
    assert!(client
        .set_branch_status(id, &[BranchStatus::Ready], BranchStatus::Resetting)
        .await
        .unwrap());
    let pending = hex32();
    assert!(client.set_pending_timeline(id, Some(&pending)).await.unwrap());
    pending
}

fn names(branches: &[BranchRecord]) -> Vec<&str> {
    branches.iter().map(|b| b.name.as_str()).collect()
}

// Review Focus 3: the cap is checked under the database row's lock.
#[tokio::test]
#[ignore]
async fn create_branch_enforces_the_cap_under_a_race() {
    let client = client().await;
    for round in 0..5 {
        let (id, _ep) = ready_database(&client).await;
        assert!(client.update_database_settings(id, &SettingsPatch { max_branches: Some(1), ..Default::default() }).await.unwrap());
        let (a, b) = (new_branch(id, "a", main_of(id)), new_branch(id, "b", main_of(id)));
        let (ra, rb) = tokio::join!(client.create_branch(&a), client.create_branch(&b));
        let listed = client.list_branches(id).await.unwrap();
        cleanup(&client, id).await;
        let mut outcomes = [ra.unwrap(), rb.unwrap()];
        outcomes.sort_by_key(|o| format!("{o:?}"));
        assert_eq!(
            outcomes,
            [CreateBranchOutcome::CapReached { max: 1 }, CreateBranchOutcome::Created],
            "round {round}"
        );
        assert_eq!(listed.len(), 2, "main and one branch, round {round}");
    }
}

#[tokio::test]
#[ignore]
async fn create_branch_refuses_taken_names_and_unready_parents() {
    let client = client().await;
    let (id, _ep) = ready_database(&client).await;
    let (other, _ep) = ready_database(&client).await;
    let at = chrono::DateTime::parse_from_rfc3339("2026-09-30T12:00:00Z").unwrap().to_utc();

    let mut dev = new_branch(id, "dev", main_of(id));
    dev.branched_at_time = Some(at);
    dev.branched_at_lsn = Some("0/16B3748");
    let created = client.create_branch(&dev).await.unwrap();
    let dev_row = branch(&client, dev.id).await;
    let taken = client.create_branch(&new_branch(id, "dev", main_of(id))).await.unwrap();
    let main_taken = client.create_branch(&new_branch(id, "main", main_of(id))).await.unwrap();
    // dev is still pending, main of another database isn't ours, and an unknown parent.
    let pending_parent = client.create_branch(&new_branch(id, "c1", dev.id)).await.unwrap();
    let foreign_parent = client.create_branch(&new_branch(id, "c2", main_of(other))).await.unwrap();
    let unknown_parent = client.create_branch(&new_branch(id, "c3", BranchId::new())).await.unwrap();
    // A deleted branch frees its name.
    assert_eq!(client.begin_branch_delete(dev.id).await.unwrap(), BranchDeleteOutcome::Started);
    let deleting_name = client.create_branch(&new_branch(id, "dev", main_of(id))).await.unwrap();
    client.mark_branch_deleted(dev.id).await.unwrap();
    let reused = client.create_branch(&new_branch(id, "dev", main_of(id))).await.unwrap();
    let listed = client.list_branches(id).await.unwrap();
    // A database being deleted, and one that doesn't exist.
    assert!(client.begin_delete(other).await.unwrap());
    let not_live = client
        .create_branch(&new_branch(other, "x", main_of(other)))
        .await
        .unwrap();
    let missing_db = DatabaseId::new();
    let missing = client
        .create_branch(&new_branch(missing_db, "x", main_of(missing_db)))
        .await
        .unwrap();
    cleanup(&client, id).await;
    cleanup(&client, other).await;

    assert_eq!(created, CreateBranchOutcome::Created);
    assert_eq!(dev_row.database_id, id);
    assert_eq!(dev_row.name, "dev");
    assert_eq!(dev_row.parent_branch_id, Some(main_of(id)));
    assert_eq!(dev_row.kind, BranchKind::Normal);
    assert_eq!(dev_row.status, BranchStatus::Pending);
    assert_eq!(dev_row.branched_at_time, Some(at));
    assert_eq!(dev_row.branched_at_lsn.as_deref(), Some("0/16B3748"));
    assert_eq!((dev_row.endpoint_id, dev_row.timeline_id, dev_row.owner_password_enc), (None, None, None));
    assert_eq!(taken, CreateBranchOutcome::NameTaken);
    assert_eq!(main_taken, CreateBranchOutcome::NameTaken);
    assert_eq!(pending_parent, CreateBranchOutcome::ParentNotReady);
    assert_eq!(foreign_parent, CreateBranchOutcome::ParentNotReady);
    assert_eq!(unknown_parent, CreateBranchOutcome::ParentNotReady);
    assert_eq!(deleting_name, CreateBranchOutcome::NameTaken);
    assert_eq!(reused, CreateBranchOutcome::Created);
    assert_eq!(names(&listed), ["main", "dev"], "deleted branches aren't listed");
    assert_eq!(not_live, CreateBranchOutcome::DatabaseNotLive);
    assert_eq!(missing, CreateBranchOutcome::DatabaseNotLive);
}

#[tokio::test]
#[ignore]
async fn backups_do_not_count_as_children_or_toward_the_cap() {
    let client = client().await;
    let (id, _ep) = ready_database(&client).await;
    assert!(client.update_database_settings(id, &SettingsPatch { max_branches: Some(2), ..Default::default() }).await.unwrap());
    let dev = ready_branch(&client, id, "dev", main_of(id)).await;
    let pending = start_reset(&client, dev).await;
    let backup = BranchId::new();
    assert!(client
        .switch_to_restored_timeline(dev, &pending, backup, "dev-before-restore-20260930T120000Z")
        .await
        .unwrap());

    let children_of_dev = client.live_children(dev).await.unwrap();
    let backups_of_dev = client.live_backups_of(dev).await.unwrap();
    let children_of_main = client.live_children(main_of(id)).await.unwrap();
    // A backup can be a parent; its child counts, the backup doesn't: dev + from-backup = 2.
    let from_backup = client.create_branch(&new_branch(id, "from-backup", backup)).await.unwrap();
    let third = client.create_branch(&new_branch(id, "third", main_of(id))).await.unwrap();
    let children_of_backup = client.live_children(backup).await.unwrap();
    cleanup(&client, id).await;

    assert!(children_of_dev.is_empty(), "backups aren't children");
    assert_eq!(backups_of_dev.iter().map(|b| b.id).collect::<Vec<_>>(), [backup]);
    assert_eq!(names(&children_of_main), ["dev"]);
    assert_eq!(from_backup, CreateBranchOutcome::Created);
    assert_eq!(third, CreateBranchOutcome::CapReached { max: 2 });
    assert_eq!(names(&children_of_backup), ["from-backup"]);
}

#[tokio::test]
#[ignore]
async fn switch_to_pending_timeline_moves_endpoint_and_records_superseded() {
    let client = client().await;
    let (id, ep) = ready_database(&client).await;
    let main = main_of(id);
    let old = main_branch(&client, id).await.timeline_id.unwrap();

    // Ready, not resetting: refused, nothing moves.
    let pending = hex32();
    assert!(client.set_pending_timeline(main, Some(&pending)).await.unwrap());
    let while_ready = client.switch_to_pending_timeline(main, &pending, None).await.unwrap();
    let untouched = main_branch(&client, id).await;

    assert!(client
        .set_branch_status(main, &[BranchStatus::Ready], BranchStatus::Resetting)
        .await
        .unwrap());
    let switched = client.switch_to_pending_timeline(main, &pending, None).await.unwrap();
    let after = main_branch(&client, id).await;
    let endpoint = client.neon_endpoint(&ep).await.unwrap().unwrap();
    // Nothing pending any more: a second switch is refused.
    let again = client.switch_to_pending_timeline(main, &pending, None).await.unwrap();
    client.clear_superseded_timeline(main).await.unwrap();
    let cleared = main_branch(&client, id).await;

    // Deleting: refused even with a pending timeline.
    let (gone, _ep) = ready_database(&client).await;
    let gone_pending = start_reset(&client, main_of(gone)).await;
    assert!(client.begin_delete(gone).await.unwrap());
    let while_deleting = client
        .switch_to_pending_timeline(main_of(gone), &gone_pending, None)
        .await
        .unwrap();
    cleanup(&client, id).await;
    cleanup(&client, gone).await;

    assert!(!while_ready);
    assert_eq!(untouched.timeline_id.as_deref(), Some(old.as_str()));
    assert_eq!(untouched.pending_timeline_id.as_deref(), Some(pending.as_str()));
    assert!(switched);
    assert_eq!(after.timeline_id.as_deref(), Some(pending.as_str()));
    assert_eq!(after.superseded_timeline_id.as_deref(), Some(old.as_str()));
    assert_eq!(after.pending_timeline_id, None);
    assert_eq!(after.status, BranchStatus::Resetting, "the flow moves it back to ready");
    assert_eq!(endpoint.timeline_id, pending);
    assert!(!again);
    assert_eq!(cleared.superseded_timeline_id, None);
    assert_eq!(cleared.timeline_id.as_deref(), Some(pending.as_str()));
    assert!(!while_deleting);
}

#[tokio::test]
#[ignore]
async fn switch_to_restored_timeline_inserts_the_backup_atomically() {
    let client = client().await;
    let (id, ep) = ready_database(&client).await;
    let main = main_of(id);
    let old = main_branch(&client, id).await.timeline_id.unwrap();

    // Not resetting: refused, no backup.
    let refused_backup = BranchId::new();
    let refused = client
        .switch_to_restored_timeline(main, &hex32(), refused_backup, "main-before-restore-20260930T110000Z")
        .await
        .unwrap();
    let no_backup = client.get_branch(refused_backup).await.unwrap();

    let pending = start_reset(&client, main).await;
    let backup = BranchId::new();
    let name = "main-before-restore-20260930T120000Z";
    let switched = client.switch_to_restored_timeline(main, &pending, backup, name).await.unwrap();
    let after = main_branch(&client, id).await;
    let backup_row = branch(&client, backup).await;
    let endpoint = client.neon_endpoint(&ep).await.unwrap().unwrap();

    // The backup can't be written (its name is taken): nothing else is either.
    let second = hex32();
    assert!(client.set_pending_timeline(main, Some(&second)).await.unwrap());
    let before_failed = main_branch(&client, id).await;
    let failed = client.switch_to_restored_timeline(main, &second, BranchId::new(), name).await;
    let after_failed = main_branch(&client, id).await;
    let endpoint_after_failed = client.neon_endpoint(&ep).await.unwrap().unwrap();
    let backups = client.live_backups_of(main).await.unwrap();
    cleanup(&client, id).await;

    assert!(!refused);
    assert_eq!(no_backup, None);
    assert!(switched);
    assert_eq!(after.timeline_id.as_deref(), Some(pending.as_str()));
    assert_eq!(after.pending_timeline_id, None);
    assert_eq!(after.superseded_timeline_id, None, "the old timeline lives on in the backup");
    assert_eq!(endpoint.timeline_id, pending);
    assert_eq!(backup_row.database_id, id);
    assert_eq!(backup_row.name, name);
    assert_eq!(backup_row.parent_branch_id, Some(main));
    assert_eq!(backup_row.kind, BranchKind::Backup);
    assert_eq!(backup_row.status, BranchStatus::Ready);
    assert_eq!(backup_row.timeline_id.as_deref(), Some(old.as_str()));
    assert_eq!(backup_row.endpoint_id, None);
    assert_eq!(backup_row.owner_password_enc, None);
    assert!(
        matches!(&failed, Err(cheladb_metadata::MetadataError::BranchNameTaken(n)) if n == name),
        "{failed:?}"
    );
    assert_eq!(after_failed, before_failed);
    assert_eq!(endpoint_after_failed.timeline_id, pending);
    assert_eq!(backups.len(), 1);
}

// Another run replaced the pending timeline: a switch naming the old one changes nothing.
#[tokio::test]
#[ignore]
async fn a_switch_expecting_another_pending_timeline_changes_nothing() {
    let client = client().await;
    let (id, ep) = ready_database(&client).await;
    let main = main_of(id);
    let theirs = start_reset(&client, main).await;
    let before = main_branch(&client, id).await;
    let endpoint_before = client.neon_endpoint(&ep).await.unwrap().unwrap();

    let mine = hex32();
    let reset = client.switch_to_pending_timeline(main, &mine, None).await.unwrap();
    let backup = BranchId::new();
    let restored = client
        .switch_to_restored_timeline(main, &mine, backup, "main-before-restore-20260930T130000Z")
        .await
        .unwrap();
    let after = main_branch(&client, id).await;
    let endpoint_after = client.neon_endpoint(&ep).await.unwrap().unwrap();
    let no_backup = client.get_branch(backup).await.unwrap();
    // The run that owns it still can.
    let owner = client.switch_to_pending_timeline(main, &theirs, None).await.unwrap();
    cleanup(&client, id).await;

    assert!(!reset);
    assert!(!restored);
    assert_eq!(after, before);
    assert_eq!(after.pending_timeline_id.as_deref(), Some(theirs.as_str()));
    assert_eq!(endpoint_after.timeline_id, endpoint_before.timeline_id);
    assert_eq!(no_backup, None);
    assert!(owner);
}

#[tokio::test]
#[ignore]
async fn begin_branch_delete_takes_backups_along() {
    let client = client().await;
    let (id, _ep) = ready_database(&client).await;
    let dev = ready_branch(&client, id, "dev", main_of(id)).await;
    let pending = start_reset(&client, dev).await;
    let backup = BranchId::new();
    assert!(client
        .switch_to_restored_timeline(dev, &pending, backup, "dev-before-restore-20260930T120000Z")
        .await
        .unwrap());
    assert!(client
        .set_branch_status(dev, &[BranchStatus::Resetting], BranchStatus::Ready)
        .await
        .unwrap());

    let moved = client.begin_branch_delete(dev).await.unwrap();
    let again = client.begin_branch_delete(dev).await.unwrap();
    let (dev_row, backup_row, main) = (
        branch(&client, dev).await,
        branch(&client, backup).await,
        main_branch(&client, id).await,
    );
    client.mark_branch_deleted(dev).await.unwrap();
    let tombstone = branch(&client, dev).await;
    let deleted_at: bool = sqlx::query_scalar("SELECT deleted_at IS NOT NULL FROM branches WHERE id = $1")
        .bind(dev.as_uuid())
        .fetch_one(client.pool())
        .await
        .unwrap();
    let on_deleted = client.begin_branch_delete(dev).await.unwrap();
    let still_deleted = branch(&client, dev).await;
    cleanup(&client, id).await;

    assert_eq!(moved, BranchDeleteOutcome::Started);
    assert_eq!(again, BranchDeleteOutcome::NotMoved, "already deleting");
    assert_eq!(dev_row.status, BranchStatus::Deleting);
    assert_eq!(backup_row.status, BranchStatus::Deleting);
    assert_eq!(main.status, BranchStatus::Ready, "the parent is untouched");
    assert_eq!(tombstone.status, BranchStatus::Deleted);
    assert_eq!(tombstone.owner_password_enc, None);
    assert!(deleted_at);
    assert_eq!(on_deleted, BranchDeleteOutcome::NotMoved);
    assert_eq!(still_deleted.status, BranchStatus::Deleted);
}

#[tokio::test]
#[ignore]
async fn set_branch_status_never_leaves_deleting() {
    let client = client().await;
    let (id, _ep) = ready_database(&client).await;
    let dev = new_branch(id, "dev", main_of(id));
    client.create_branch(&dev).await.unwrap();

    let wrong_from = client
        .set_branch_status(dev.id, &[BranchStatus::Ready], BranchStatus::Resetting)
        .await
        .unwrap();
    let claimed = client
        .set_branch_status(dev.id, &[BranchStatus::Pending, BranchStatus::Failed], BranchStatus::Provisioning)
        .await
        .unwrap();
    client.set_branch_error(dev.id, Some("creating timeline: boom")).await.unwrap();
    let with_error = branch(&client, dev.id).await;
    client.set_branch_error(dev.id, None).await.unwrap();
    let error_cleared = branch(&client, dev.id).await;

    assert_eq!(client.begin_branch_delete(dev.id).await.unwrap(), BranchDeleteOutcome::Started);
    let out_of_deleting = client
        .set_branch_status(dev.id, &[BranchStatus::Deleting], BranchStatus::Ready)
        .await
        .unwrap();
    client.set_branch_failed(dev.id, "late failure").await.unwrap();
    let ready_after_delete = client.set_branch_ready(dev.id, "v1:x", None, None).await.unwrap();
    let deleting = branch(&client, dev.id).await;
    client.mark_branch_deleted(dev.id).await.unwrap();
    let out_of_deleted = client
        .set_branch_status(dev.id, &[BranchStatus::Deleted], BranchStatus::Ready)
        .await
        .unwrap();
    let deleted = branch(&client, dev.id).await;

    // set_branch_failed applies while live, and set_branch_ready only from provisioning.
    let other = new_branch(id, "other", main_of(id));
    client.create_branch(&other).await.unwrap();
    let ready_from_pending = client.set_branch_ready(other.id, "v1:x", None, None).await.unwrap();
    client.set_branch_failed(other.id, "creating timeline: boom").await.unwrap();
    let failed = branch(&client, other.id).await;
    cleanup(&client, id).await;

    assert!(!wrong_from);
    assert!(claimed);
    assert_eq!(with_error.status, BranchStatus::Provisioning, "set_branch_error keeps the status");
    assert_eq!(with_error.error.as_deref(), Some("creating timeline: boom"));
    assert_eq!(error_cleared.error, None);
    assert!(!out_of_deleting);
    assert!(!ready_after_delete);
    assert_eq!(deleting.status, BranchStatus::Deleting);
    assert_eq!(deleting.error, None);
    assert_eq!(deleting.owner_password_enc, None);
    assert!(!out_of_deleted);
    assert_eq!(deleted.status, BranchStatus::Deleted);
    assert!(!ready_from_pending);
    assert_eq!(failed.status, BranchStatus::Failed);
    assert_eq!(failed.error.as_deref(), Some("creating timeline: boom"));
}

// Ruling 4: provisioning moves main and its database together.
#[tokio::test]
#[ignore]
async fn main_set_ready_sets_the_database_ready_too() {
    let client = client().await;
    let id = insert_pending(&client).await;
    let pending_main = main_branch(&client, id).await;
    link_endpoint(&client, id).await;
    // Still pending: set_branch_ready only moves a provisioning branch.
    let too_early = client.set_branch_ready(main_of(id), "v1:owner", None, None).await.unwrap();
    let pending_row = client.get_database(id).await.unwrap().unwrap();
    client.set_status(id, DatabaseStatus::Provisioning).await.unwrap();
    let ready = client.set_branch_ready(main_of(id), "v1:owner", None, None).await.unwrap();
    let row = client.get_database(id).await.unwrap().unwrap();
    let main = main_branch(&client, id).await;

    // A non-main branch never touches its database.
    let other = insert_pending(&client).await;
    link_endpoint(&client, other).await;
    assert!(make_ready(&client, other, "v1:owner").await);
    let dev = new_branch(other, "dev", main_of(other));
    client.create_branch(&dev).await.unwrap();
    client
        .set_branch_status(dev.id, &[BranchStatus::Pending], BranchStatus::Provisioning)
        .await
        .unwrap();
    client.set_branch_failed(dev.id, "boom").await.unwrap();
    let other_row = client.get_database(other).await.unwrap().unwrap();
    cleanup(&client, id).await;
    cleanup(&client, other).await;

    assert_eq!(pending_main.id, main_of(id));
    assert_eq!(pending_main.name, "main");
    assert_eq!(pending_main.parent_branch_id, None);
    assert_eq!(pending_main.kind, BranchKind::Normal);
    assert_eq!(pending_main.status, BranchStatus::Pending);
    assert!(!too_early);
    assert_eq!(pending_row.status, DatabaseStatus::Pending);
    assert!(ready);
    assert_eq!(row.status, DatabaseStatus::Ready);
    assert_eq!(row.error, None);
    assert_eq!(main.status, BranchStatus::Ready);
    assert_eq!(main.owner_password_enc.as_deref(), Some("v1:owner"));
    assert_eq!(main.branched_at_lsn, None);
    assert_eq!(other_row.status, DatabaseStatus::Ready);
}

// A delete moves the database and every live branch to deleting in one step.
#[tokio::test]
#[ignore]
async fn begin_delete_takes_every_branch_along() {
    let client = client().await;
    let (id, _ep) = ready_database(&client).await;
    let dev = ready_branch(&client, id, "dev", main_of(id)).await;
    let gone = ready_branch(&client, id, "gone", main_of(id)).await;
    assert_eq!(client.begin_branch_delete(gone).await.unwrap(), BranchDeleteOutcome::Started);
    client.mark_branch_deleted(gone).await.unwrap();

    assert!(client.begin_delete(id).await.unwrap());
    let statuses = [
        main_branch(&client, id).await.status,
        branch(&client, dev).await.status,
        branch(&client, gone).await.status,
    ];
    let refused = client.create_branch(&new_branch(id, "late", main_of(id))).await.unwrap();
    cleanup(&client, id).await;
    assert_eq!(statuses, [BranchStatus::Deleting, BranchStatus::Deleting, BranchStatus::Deleted]);
    assert_eq!(refused, CreateBranchOutcome::DatabaseNotLive);
}

#[tokio::test]
#[ignore]
async fn database_settings_and_neon_tenant_round_trip() {
    let client = client().await;
    let id = insert_pending(&client).await;
    let defaults = client.database_settings(id).await.unwrap();
    let hour = std::time::Duration::from_secs(3600);
    assert!(client.update_database_settings(id, &SettingsPatch { history_window: Some(hour), ..Default::default() }).await.unwrap());
    let window_only = client.database_settings(id).await.unwrap();
    assert!(client.update_database_settings(id, &SettingsPatch { max_branches: Some(0), ..Default::default() }).await.unwrap());
    let cap_only = client.database_settings(id).await.unwrap();
    let tenant = hex32();
    let tenant_set = client.set_neon_tenant(id, &tenant).await.unwrap();
    let row = client.get_database(id).await.unwrap().unwrap();
    assert!(client.begin_delete(id).await.unwrap());
    let on_deleting = client.update_database_settings(id, &SettingsPatch { max_branches: Some(3), ..Default::default() }).await.unwrap();
    let tenant_on_deleting = client.set_neon_tenant(id, &hex32()).await.unwrap();
    let after_delete = client.database_settings(id).await.unwrap();
    let missing = client.database_settings(DatabaseId::new()).await.unwrap();
    cleanup(&client, id).await;

    let week = std::time::Duration::from_secs(7 * 24 * 3600);
    assert_eq!(defaults, Some(DatabaseSettings { history_window: week, max_branches: 5 }));
    assert_eq!(window_only, Some(DatabaseSettings { history_window: hour, max_branches: 5 }));
    assert_eq!(cap_only, Some(DatabaseSettings { history_window: hour, max_branches: 0 }));
    assert!(tenant_set);
    assert_eq!(row.neon_tenant_id.as_deref(), Some(tenant.as_str()));
    assert_eq!((row.history_window, row.max_branches), (hour, 0));
    assert!(!on_deleting);
    assert!(!tenant_on_deleting);
    assert_eq!(after_delete, cap_only);
    assert_eq!(missing, None);
}

#[tokio::test]
#[ignore]
async fn list_live_branches_for_reconcile_skips_deleted_branches() {
    let client = client().await;
    let (id, ep) = ready_database(&client).await;
    let dev = ready_branch(&client, id, "dev", main_of(id)).await;
    let gone = ready_branch(&client, id, "gone", main_of(id)).await;
    assert_eq!(client.begin_branch_delete(gone).await.unwrap(), BranchDeleteOutcome::Started);
    client.mark_branch_deleted(gone).await.unwrap();
    sqlx::query("UPDATE branches SET updated_at = now() - interval '700 seconds' WHERE id = $1")
        .bind(dev.as_uuid())
        .execute(client.pool())
        .await
        .unwrap();

    let all = client.list_live_branches_for_reconcile().await.unwrap();
    let main = all.iter().find(|b| b.id == main_of(id)).cloned();
    let dev_row = all.iter().find(|b| b.id == dev).cloned();
    let listed_gone = all.iter().any(|b| b.id == gone);
    let main_timeline = main_branch(&client, id).await.timeline_id;
    cleanup(&client, id).await;

    let main = main.expect("main is listed");
    assert_eq!(main.database_id, id);
    assert_eq!(main.kind, BranchKind::Normal);
    assert_eq!(main.status, BranchStatus::Ready);
    assert_eq!(main.endpoint_id.as_deref(), Some(ep.as_str()));
    assert_eq!(main.timeline_id, main_timeline);
    assert_eq!((main.pending_timeline_id, main.superseded_timeline_id), (None, None));
    assert!(main.age_secs < 60);
    let dev_row = dev_row.expect("dev is listed");
    assert!(dev_row.age_secs >= 700, "{}", dev_row.age_secs);
    assert!(!listed_gone);
}

#[tokio::test]
#[ignore]
async fn get_branch_of_an_unknown_id_is_none() {
    let client = client().await;
    assert_eq!(client.get_branch(BranchId::new()).await.unwrap(), None);
    assert!(client.list_branches(DatabaseId::new()).await.unwrap().is_empty());
}

// --- Fix round 1: lock order and the mirror guard -----------------------------------------

/// Postgres' `deadlock_detected`.
const DEADLOCK: &str = "40P01";

fn is_deadlock(err: &cheladb_metadata::MetadataError) -> bool {
    matches!(err, cheladb_metadata::MetadataError::Database(sqlx::Error::Database(e))
        if e.code().as_deref() == Some(DEADLOCK))
}

/// Runs `begin_delete`'s statements by hand, pausing between them while `racer` (a write on
/// main) starts: the database row is held first, as every transaction must take it.
async fn delete_holding_the_database_row<F, Fut>(client: &MetadataClient, id: DatabaseId, racer: F) -> (Result<(), sqlx::Error>, Result<bool, cheladb_metadata::MetadataError>)
where
    F: FnOnce(MetadataClient) -> Fut,
    Fut: std::future::Future<Output = Result<bool, cheladb_metadata::MetadataError>> + Send + 'static,
{
    let mut tx = client.pool().begin().await.unwrap();
    sqlx::query("UPDATE databases SET status = 'deleting', updated_at = now() WHERE id = $1 AND status NOT IN ('deleting', 'deleted')")
        .bind(id.as_uuid())
        .execute(&mut *tx)
        .await
        .unwrap();
    let racing = tokio::spawn(racer(client.clone()));
    // Long enough for the racer to take whatever it locks first and queue on the rest.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let delete = async {
        sqlx::query("UPDATE branches SET status = 'deleting', updated_at = now() WHERE database_id = $1 AND status NOT IN ('deleting', 'deleted')")
            .bind(id.as_uuid())
            .execute(&mut *tx)
            .await?;
        tx.commit().await
    }
    .await;
    (delete, racing.await.unwrap())
}

// Fix round 1, I2: a delete racing provision's last write on main. Both lock the database row
// before main's branch row, so neither waits on the other in a cycle.
#[tokio::test]
#[ignore]
async fn begin_delete_racing_main_set_ready_never_deadlocks() {
    let client = client().await;
    // Deterministic: the delete holds the database row while set_branch_ready starts.
    let id = insert_pending(&client).await;
    link_endpoint(&client, id).await;
    client.set_status(id, DatabaseStatus::Provisioning).await.unwrap();
    let (delete, ready) = delete_holding_the_database_row(&client, id, move |c| async move {
        c.set_branch_ready(main_of(id), "v1:owner", None, None).await
    })
    .await;
    let (row, main) = (client.get_database(id).await.unwrap().unwrap(), main_branch(&client, id).await);
    cleanup(&client, id).await;
    assert!(delete.is_ok(), "the delete failed: {delete:?}");
    assert!(matches!(ready, Ok(false)), "set_branch_ready: {ready:?}");
    assert_eq!((row.status, main.status), (DatabaseStatus::Deleting, BranchStatus::Deleting));

    // And really concurrent, repeatedly.
    for round in 0..20 {
        let id = insert_pending(&client).await;
        link_endpoint(&client, id).await;
        client.set_status(id, DatabaseStatus::Provisioning).await.unwrap();
        let (a, b) = (client.clone(), client.clone());
        let delete = tokio::spawn(async move { a.begin_delete(id).await });
        let ready = tokio::spawn(async move { b.set_branch_ready(main_of(id), "v1:owner", None, None).await });
        let (delete, ready) = (delete.await.unwrap(), ready.await.unwrap());
        let (row, main) = (client.get_database(id).await.unwrap().unwrap(), main_branch(&client, id).await);
        cleanup(&client, id).await;
        if let Err(e) = &delete {
            assert!(!is_deadlock(e), "round {round}: begin_delete deadlocked");
        }
        if let Err(e) = &ready {
            assert!(!is_deadlock(e), "round {round}: set_branch_ready deadlocked");
        }
        assert!(delete.unwrap(), "round {round}");
        ready.unwrap();
        // Either ready then deleting, or deleting first (set_branch_ready refused): both end here.
        assert_eq!((row.status, main.status), (DatabaseStatus::Deleting, BranchStatus::Deleting), "round {round}");
    }
}

// Fix round 1, I2: the same for recording main's endpoint.
#[tokio::test]
#[ignore]
async fn begin_delete_racing_main_endpoint_never_deadlocks() {
    let client = client().await;
    let id = insert_pending(&client).await;
    let ep = ep_id();
    let ep_racer = ep.clone();
    let (delete, linked) = delete_holding_the_database_row(&client, id, move |c| async move {
        let (tenant, timeline) = (hex32(), hex32());
        c.create_endpoint_for_branch(main_of(id), &timeline, &new_endpoint(&ep_racer, &tenant, &timeline)).await
    })
    .await;
    let written = endpoint_exists(&client, &ep).await;
    let main = main_branch(&client, id).await;
    cleanup(&client, id).await;
    sqlx::query("DELETE FROM neon_endpoints WHERE id = $1").bind(&ep).execute(client.pool()).await.unwrap();
    assert!(delete.is_ok(), "the delete failed: {delete:?}");
    assert!(matches!(linked, Ok(false)), "create_endpoint_for_branch: {linked:?}");
    assert!(!written);
    assert_eq!(main.status, BranchStatus::Deleting);
}

// Fix round 1, I2 (found while auditing the lock order): a restore holds its branch row and
// then inserts the backup, whose foreign key takes a KEY SHARE lock on the database row. A
// create under that branch holds the database row and waits for the branch. The create's
// database lock must not conflict with KEY SHARE.
#[tokio::test]
#[ignore]
async fn create_branch_racing_a_restore_of_its_parent_never_deadlocks() {
    let client = client().await;
    let (id, _ep) = ready_database(&client).await;
    let backup = BranchId::new();
    // The restore's transaction, by hand: its branch row first...
    let mut tx = client.pool().begin().await.unwrap();
    sqlx::query("SELECT 1 FROM branches WHERE id = $1 FOR UPDATE")
        .bind(id.as_uuid())
        .execute(&mut *tx)
        .await
        .unwrap();
    let c = client.clone();
    let create = tokio::spawn(async move { c.create_branch(&new_branch(id, "dev", main_of(id))).await });
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    // ...then the backup row, referencing the database.
    let restore = async {
        sqlx::query(
            "INSERT INTO branches (id, database_id, name, parent_branch_id, kind, status)
             VALUES ($1, $2, 'main-before-restore-20260930T120000Z', $2, 'backup', 'ready')",
        )
        .bind(backup.as_uuid())
        .bind(id.as_uuid())
        .execute(&mut *tx)
        .await?;
        tx.commit().await
    }
    .await;
    let create = create.await.unwrap();
    cleanup(&client, id).await;
    assert!(restore.is_ok(), "the restore failed: {restore:?}");
    assert!(matches!(create, Ok(CreateBranchOutcome::Created)), "create_branch: {create:?}");
}

// Fix round 1, minor: the database's provisioning writes never overwrite a main that has moved
// on (a reset in flight, say); they only mirror onto a pending, provisioning or failed main.
#[tokio::test]
#[ignore]
async fn database_writes_mirror_only_onto_a_provisioning_main() {
    let client = client().await;
    let (id, _ep) = ready_database(&client).await;
    assert!(client
        .set_branch_status(main_of(id), &[BranchStatus::Ready], BranchStatus::Resetting)
        .await
        .unwrap());
    client.set_status(id, DatabaseStatus::Provisioning).await.unwrap();
    let after_status = main_branch(&client, id).await;
    client.set_failed(id, "boom").await.unwrap();
    let after_failed = main_branch(&client, id).await;
    backdate(&client, id, 700).await;
    client.set_status(id, DatabaseStatus::Provisioning).await.unwrap();
    backdate(&client, id, 700).await;
    let stale = client.fail_if_stale(id, 600).await.unwrap();
    let after_stale = main_branch(&client, id).await;

    // A failed main still follows (a retried provision claims it again).
    let other = insert_pending(&client).await;
    client.set_failed(other, "first try").await.unwrap();
    client.set_status(other, DatabaseStatus::Provisioning).await.unwrap();
    let reclaimed = main_branch(&client, other).await;
    cleanup(&client, id).await;
    cleanup(&client, other).await;

    assert!(stale);
    for (step, main) in [("set_status", after_status), ("set_failed", after_failed), ("fail_if_stale", after_stale)] {
        assert_eq!(main.status, BranchStatus::Resetting, "{step}");
        assert_eq!(main.error, None, "{step}");
    }
    assert_eq!(reclaimed.status, BranchStatus::Provisioning);
    assert_eq!(reclaimed.error, None);
}

// --- Step 5: the branches migration ------------------------------------------------------

/// The step 5 migration, exactly as sqlx applies it.
const BRANCHES_MIGRATION: &str = include_str!("../migrations/20260930000001_branches.sql");

/// The tables the step 5 migration touches, shaped as step 4 left them (without the FKs to
/// projects, which the migration never reads).
const PRE_STEP_5_SCHEMA: &str = r#"
CREATE TABLE neon_endpoints (
    id                   text PRIMARY KEY,
    tenant_id            text NOT NULL,
    timeline_id          text NOT NULL,
    compute_token_sha256 text NOT NULL,
    created_at           timestamptz NOT NULL DEFAULT now(),
    needs_reconfigure    boolean NOT NULL DEFAULT false
);
CREATE TABLE databases (
    id                 uuid PRIMARY KEY,
    project_id         uuid NOT NULL,
    name               text NOT NULL,
    status             text NOT NULL,
    error              text,
    created_at         timestamptz NOT NULL DEFAULT now(),
    updated_at         timestamptz NOT NULL DEFAULT now(),
    database_name      text,
    owner_role         text,
    owner_password_enc text,
    deleted_at         timestamptz,
    endpoint_id        text REFERENCES neon_endpoints (id) ON DELETE SET NULL
);
CREATE UNIQUE INDEX databases_live_name_key ON databases (project_id, name) WHERE status <> 'deleted';
CREATE UNIQUE INDEX databases_endpoint_id_key ON databases (endpoint_id) WHERE endpoint_id IS NOT NULL;
"#;

/// `(id, status, error, created_at, updated_at, neon_tenant_id, history_window, max_branches)`
/// of a migrated `databases` row, times and interval as text.
type MigratedDatabase = (uuid::Uuid, String, Option<String>, String, String, Option<String>, String, i32);

/// A pre-migration `databases` row.
struct OldDatabase {
    id: uuid::Uuid,
    status: &'static str,
    endpoint: Option<(String, String, String)>, // (id, tenant, timeline)
    sealed: Option<String>,
    error: Option<&'static str>,
}

// Review Focus 4: the migration runs on another machine's metadata. It's applied, byte for
// byte, to a scratch schema holding step 4's shapes: every live database gets exactly one
// main whose sealed password still opens, and nothing else changes.
#[tokio::test]
#[ignore]
async fn migration_backfills_main_for_live_databases_only() {
    use cheladb_secrets::{open_credential, seal_credential, Purpose, SecretKey};
    use sqlx::Row;

    let database_url = std::env::var("CHELADB_METADATA_DATABASE_URL").unwrap_or_else(|_| {
        "postgres://cheladb:cheladb@127.0.0.1:5433/cheladb_metadata".to_string()
    });
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&database_url)
        .await
        .expect("metadata Postgres reachable (just deps-up)");
    let schema = format!("step5_migration_{}", hex32());
    let mut conn = pool.acquire().await.unwrap();
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}; SET search_path TO {schema};")))
        .execute(&mut *conn)
        .await
        .unwrap();
    sqlx::raw_sql(PRE_STEP_5_SCHEMA).execute(&mut *conn).await.unwrap();

    let key = SecretKey::generate();
    let endpoint = || (ep_id(), hex32(), hex32());
    let sealed = |id: uuid::Uuid| Some(seal_credential(&key, &id.to_string(), Purpose::Owner, "s3cret"));
    let mut rows = Vec::new();
    // Pending before its endpoint exists.
    rows.push(OldDatabase { id: uuid::Uuid::new_v4(), status: "pending", endpoint: None, sealed: None, error: None });
    // Provisioning with its endpoint recorded.
    rows.push(OldDatabase { id: uuid::Uuid::new_v4(), status: "provisioning", endpoint: Some(endpoint()), sealed: None, error: None });
    // Ready: an endpoint and a sealed password.
    let ready_id = uuid::Uuid::new_v4();
    rows.push(OldDatabase { id: ready_id, status: "ready", endpoint: Some(endpoint()), sealed: sealed(ready_id), error: None });
    // Failed, its endpoint left for reconcile.
    rows.push(OldDatabase { id: uuid::Uuid::new_v4(), status: "failed", endpoint: Some(endpoint()), sealed: None, error: Some("creating timeline: boom") });
    // Deleting, mid-teardown.
    let deleting_id = uuid::Uuid::new_v4();
    rows.push(OldDatabase { id: deleting_id, status: "deleting", endpoint: Some(endpoint()), sealed: sealed(deleting_id), error: None });
    // Deleted after step 4, and a pre-step-4 tombstone (both without an endpoint or password).
    rows.push(OldDatabase { id: uuid::Uuid::new_v4(), status: "deleted", endpoint: None, sealed: None, error: None });
    rows.push(OldDatabase { id: uuid::Uuid::new_v4(), status: "deleted", endpoint: None, sealed: None, error: Some("postgres:17 era") });
    let project = uuid::Uuid::new_v4();
    for (i, row) in rows.iter().enumerate() {
        if let Some((ep, tenant, timeline)) = &row.endpoint {
            sqlx::query("INSERT INTO neon_endpoints (id, tenant_id, timeline_id, compute_token_sha256) VALUES ($1, $2, $3, 'cd')")
                .bind(ep).bind(tenant).bind(timeline)
                .execute(&mut *conn).await.unwrap();
        }
        sqlx::query(
            "INSERT INTO databases (id, project_id, name, status, error, created_at, updated_at,
                                    database_name, owner_role, owner_password_enc, deleted_at, endpoint_id)
             VALUES ($1, $2, $3, $4, $5, now() - make_interval(hours => $6), now() - make_interval(mins => $6),
                     'db', 'postgres', $7, CASE WHEN $4 = 'deleted' THEN now() END, $8)",
        )
        .bind(row.id)
        .bind(project)
        .bind(format!("db{i}"))
        .bind(row.status)
        .bind(row.error)
        .bind(i as i32 + 1)
        .bind(&row.sealed)
        .bind(row.endpoint.as_ref().map(|e| &e.0))
        .execute(&mut *conn)
        .await
        .unwrap();
    }
    // A step 3 demo endpoint that no database links to.
    let (orphan, orphan_tenant, orphan_timeline) = endpoint();
    sqlx::query("INSERT INTO neon_endpoints (id, tenant_id, timeline_id, compute_token_sha256) VALUES ($1, $2, $3, 'cd')")
        .bind(&orphan).bind(&orphan_tenant).bind(&orphan_timeline)
        .execute(&mut *conn).await.unwrap();
    let databases_before: Vec<(uuid::Uuid, String, Option<String>, String, String)> = sqlx::query_as(
        "SELECT id, status, error, created_at::text, updated_at::text FROM databases ORDER BY id",
    )
    .fetch_all(&mut *conn)
    .await
    .unwrap();
    let endpoints_before: Vec<(String, String, String)> =
        sqlx::query_as("SELECT id, tenant_id, timeline_id FROM neon_endpoints ORDER BY id")
            .fetch_all(&mut *conn)
            .await
            .unwrap();

    let migrated = sqlx::raw_sql(BRANCHES_MIGRATION).execute(&mut *conn).await;

    let branches = if migrated.is_ok() {
        sqlx::query(
            "SELECT b.*, d.created_at AS d_created, d.updated_at AS d_updated
               FROM branches b JOIN databases d ON d.id = b.database_id ORDER BY b.id",
        )
        .fetch_all(&mut *conn)
        .await
    } else {
        Ok(Vec::new())
    };
    let databases_after: Result<Vec<MigratedDatabase>, _> =
        sqlx::query_as(
            "SELECT id, status, error, created_at::text, updated_at::text, neon_tenant_id,
                    history_window::text, max_branches
               FROM databases ORDER BY id",
        )
        .fetch_all(&mut *conn)
        .await;
    let dropped: Result<Vec<String>, _> = sqlx::query_scalar(
        "SELECT column_name::text FROM information_schema.columns
          WHERE table_schema = $1 AND table_name = 'databases'
            AND column_name IN ('endpoint_id', 'owner_password_enc')",
    )
    .bind(&schema)
    .fetch_all(&mut *conn)
    .await;
    let endpoints_after: Result<Vec<(String, String, String)>, _> =
        sqlx::query_as("SELECT id, tenant_id, timeline_id FROM neon_endpoints ORDER BY id")
            .fetch_all(&mut *conn)
            .await;
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!("SET search_path TO public; DROP SCHEMA {schema} CASCADE;")))
        .execute(&mut *conn)
        .await
        .unwrap();

    migrated.expect("the migration applies to step 4's shapes");
    let branches = branches.unwrap();
    let live: Vec<&OldDatabase> = rows.iter().filter(|r| r.status != "deleted").collect();
    assert_eq!(branches.len(), live.len(), "one main per live database, none for deleted ones");
    for row in &live {
        let b = branches
            .iter()
            .find(|b| b.get::<uuid::Uuid, _>("id") == row.id)
            .unwrap_or_else(|| panic!("no main for the {} database", row.status));
        assert_eq!(b.get::<uuid::Uuid, _>("database_id"), row.id);
        assert_eq!(b.get::<String, _>("name"), "main");
        assert_eq!(b.get::<Option<uuid::Uuid>, _>("parent_branch_id"), None);
        assert_eq!(b.get::<String, _>("kind"), "normal");
        assert_eq!(b.get::<String, _>("status"), row.status);
        assert_eq!(b.get::<Option<String>, _>("endpoint_id"), row.endpoint.as_ref().map(|e| e.0.clone()));
        assert_eq!(b.get::<Option<String>, _>("timeline_id"), row.endpoint.as_ref().map(|e| e.2.clone()));
        assert_eq!(b.get::<Option<String>, _>("pending_timeline_id"), None);
        assert_eq!(b.get::<Option<String>, _>("superseded_timeline_id"), None);
        assert_eq!(b.get::<Option<String>, _>("error"), row.error.map(str::to_string));
        assert_eq!(b.get::<Option<String>, _>("owner_password_enc"), row.sealed);
        assert_eq!(b.get::<Option<chrono::DateTime<chrono::Utc>>, _>("deleted_at"), None);
        assert_eq!(
            b.get::<chrono::DateTime<chrono::Utc>, _>("created_at"),
            b.get::<chrono::DateTime<chrono::Utc>, _>("d_created")
        );
        assert_eq!(
            b.get::<chrono::DateTime<chrono::Utc>, _>("updated_at"),
            b.get::<chrono::DateTime<chrono::Utc>, _>("d_updated")
        );
        // The AAD is the branch id, which is the database id: the password still opens.
        if let Some(sealed) = b.get::<Option<String>, _>("owner_password_enc") {
            let branch_id = b.get::<uuid::Uuid, _>("id").to_string();
            assert_eq!(open_credential(&key, &branch_id, Purpose::Owner, &sealed).unwrap(), "s3cret");
        }
    }

    // Nothing else changes: the same databases with the same statuses, errors and times, the
    // Neon tenant copied from the endpoint, the new settings at their defaults.
    let databases_after = databases_after.unwrap();
    assert_eq!(databases_after.len(), databases_before.len());
    for (before, after) in databases_before.iter().zip(&databases_after) {
        assert_eq!((&before.0, &before.1, &before.2, &before.3, &before.4), (&after.0, &after.1, &after.2, &after.3, &after.4));
        let row = rows.iter().find(|r| r.id == after.0).unwrap();
        assert_eq!(after.5, row.endpoint.as_ref().map(|e| e.1.clone()), "neon_tenant_id");
        assert_eq!(after.6, "7 days");
        assert_eq!(after.7, 5);
    }
    assert!(dropped.unwrap().is_empty(), "databases.endpoint_id and owner_password_enc are dropped");
    // Endpoints, the orphan included, are untouched.
    assert_eq!(endpoints_after.unwrap(), endpoints_before);
    assert!(!branches.iter().any(|b| b.get::<Option<String>, _>("endpoint_id").as_deref() == Some(orphan.as_str())));
}

// One query gives /v1/me each database's main and its live normal-branch count (main included,
// backups and deleted branches excluded).
#[tokio::test]
#[ignore]
async fn main_branches_with_counts_covers_a_tenants_databases() {
    let client = client().await;
    let (id, _ep) = ready_database(&client).await;
    let dev = ready_branch(&client, id, "dev", main_of(id)).await;
    let gone = ready_branch(&client, id, "gone", main_of(id)).await;
    client.begin_branch_delete(gone).await.unwrap();
    client.mark_branch_deleted(gone).await.unwrap();
    // A backup of dev doesn't count.
    let backup = new_branch(id, "dev-backup", dev);
    sqlx::query(
        "INSERT INTO branches (id, database_id, name, parent_branch_id, kind, status)
         VALUES ($1, $2, $3, $4, 'backup', 'ready')",
    )
    .bind(backup.id.as_uuid())
    .bind(id.as_uuid())
    .bind(backup.name)
    .bind(dev.as_uuid())
    .execute(client.pool())
    .await
    .unwrap();
    let (other, _ep) = ready_database(&client).await;

    let rows = client.main_branches_with_counts(TenantId(DEMO_TENANT_ID)).await.unwrap();
    let find = |db: DatabaseId| rows.iter().find(|(main, _)| main.database_id == db).cloned();
    let mine = find(id);
    let theirs = find(other);
    cleanup(&client, id).await;
    cleanup(&client, other).await;

    let (main, count) = mine.expect("the database is listed");
    assert_eq!(main.id, main_of(id));
    assert_eq!(main.name, "main");
    assert_eq!(count, 2, "main and dev");
    assert_eq!(theirs.expect("the other database is listed").1, 1);
}

// --- Step 5, Task 4: begin_branch_delete against begin_delete ---------------------------

/// A ready branch `dev` of a ready database, with one backup (from a restore).
async fn branch_with_backup(client: &MetadataClient) -> (DatabaseId, BranchId, BranchId) {
    let (id, _ep) = ready_database(client).await;
    let dev = ready_branch(client, id, "dev", main_of(id)).await;
    let pending = start_reset(client, dev).await;
    let backup = BranchId::new();
    assert!(client
        .switch_to_restored_timeline(dev, &pending, backup, "dev-before-restore-20260930T120000Z")
        .await
        .unwrap());
    assert!(client
        .set_branch_status(dev, &[BranchStatus::Resetting], BranchStatus::Ready)
        .await
        .unwrap());
    (id, dev, backup)
}

// Carried from Task 2: begin_branch_delete took the branch row, then its backups', while
// begin_delete takes the database row, then every branch row in its scan's order. When the
// scan reaches the backup before the branch, each held what the other wanted next. Now
// begin_branch_delete takes the database row first, as every transaction must.
#[tokio::test]
#[ignore]
async fn begin_branch_delete_racing_begin_delete_never_deadlocks() {
    let client = client().await;
    // Deterministic: begin_delete's statements by hand. Its UPDATE of the branches has locked
    // the backup (reached first in its scan) when begin_branch_delete starts, then goes on to
    // the branch.
    let (id, dev, backup) = branch_with_backup(&client).await;
    let mut tx = client.pool().begin().await.unwrap();
    sqlx::query("UPDATE databases SET status = 'deleting', updated_at = now() WHERE id = $1 AND status NOT IN ('deleting', 'deleted')")
        .bind(id.as_uuid())
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("UPDATE branches SET status = 'deleting', updated_at = now() WHERE id = $1")
        .bind(backup.as_uuid())
        .execute(&mut *tx)
        .await
        .unwrap();
    let c = client.clone();
    let racing = tokio::spawn(async move { c.begin_branch_delete(dev).await });
    // Long enough for the racer to take whatever it locks first and queue on the rest.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let delete = async {
        sqlx::query("UPDATE branches SET status = 'deleting', updated_at = now() WHERE database_id = $1 AND status NOT IN ('deleting', 'deleted')")
            .bind(id.as_uuid())
            .execute(&mut *tx)
            .await?;
        tx.commit().await
    }
    .await;
    let branch_delete = racing.await.unwrap();
    let (dev_row, backup_row) = (branch(&client, dev).await, branch(&client, backup).await);
    cleanup(&client, id).await;
    if let Err(sqlx::Error::Database(e)) = &delete {
        assert_ne!(e.code().as_deref(), Some(DEADLOCK), "begin_delete deadlocked");
    }
    if let Err(e) = &branch_delete {
        assert!(!is_deadlock(e), "begin_branch_delete deadlocked");
    }
    assert!(delete.is_ok(), "the delete failed: {delete:?}");
    assert!(matches!(branch_delete, Ok(BranchDeleteOutcome::NotMoved)), "begin_branch_delete: {branch_delete:?}");
    assert_eq!((dev_row.status, backup_row.status), (BranchStatus::Deleting, BranchStatus::Deleting));

    // And really concurrent, repeatedly.
    for round in 0..20 {
        let (id, dev, backup) = branch_with_backup(&client).await;
        let (a, b) = (client.clone(), client.clone());
        let delete = tokio::spawn(async move { a.begin_delete(id).await });
        let branch_delete = tokio::spawn(async move { b.begin_branch_delete(dev).await });
        let (delete, branch_delete) = (delete.await.unwrap(), branch_delete.await.unwrap());
        let (dev_row, backup_row) = (branch(&client, dev).await, branch(&client, backup).await);
        cleanup(&client, id).await;
        if let Err(e) = &delete {
            assert!(!is_deadlock(e), "round {round}: begin_delete deadlocked");
        }
        if let Err(e) = &branch_delete {
            assert!(!is_deadlock(e), "round {round}: begin_branch_delete deadlocked");
        }
        assert!(delete.unwrap(), "round {round}");
        branch_delete.unwrap();
        assert_eq!((dev_row.status, backup_row.status), (BranchStatus::Deleting, BranchStatus::Deleting), "round {round}");
    }
}

// Fix round 1, I1: a restore's backup can be a parent. Deleting its branch would take the
// backup along, and Neon can't delete a timeline others branch from, so the delete would
// retry forever. begin_branch_delete refuses, under the database row's lock (which every
// create takes first), while the branch or any live backup of it has live normal children.
#[tokio::test]
#[ignore]
async fn begin_branch_delete_refuses_while_the_branch_or_a_backup_has_children() {
    let client = client().await;
    // A child of dev's backup.
    let (id, dev, backup) = branch_with_backup(&client).await;
    let backup_name = branch(&client, backup).await.name;
    let x = new_branch(id, "x", backup);
    assert_eq!(client.create_branch(&x).await.unwrap(), CreateBranchOutcome::Created);
    let refused_for_backup = client.begin_branch_delete(dev).await.unwrap();
    let (dev_after, backup_after) = (branch(&client, dev).await, branch(&client, backup).await);
    // Once the child is gone, the delete goes ahead.
    assert_eq!(client.begin_branch_delete(x.id).await.unwrap(), BranchDeleteOutcome::Started);
    client.mark_branch_deleted(x.id).await.unwrap();
    let started = client.begin_branch_delete(dev).await.unwrap();
    let (dev_final, backup_final) = (branch(&client, dev).await, branch(&client, backup).await);
    cleanup(&client, id).await;

    // A direct child of the branch.
    let (other, other_dev, _) = branch_with_backup(&client).await;
    let kid = new_branch(other, "kid", other_dev);
    assert_eq!(client.create_branch(&kid).await.unwrap(), CreateBranchOutcome::Created);
    let refused_for_dev = client.begin_branch_delete(other_dev).await.unwrap();
    let other_dev_after = branch(&client, other_dev).await;
    cleanup(&client, other).await;

    assert_eq!(
        refused_for_backup,
        BranchDeleteOutcome::HasChildren { backup: Some(backup_name.clone()), children: vec!["x".to_string()] }
    );
    assert_eq!(
        refused_for_backup.refusal("dev").as_deref(),
        Some(format!("cannot delete dev: its backup {backup_name} has child branches (x); delete them first").as_str())
    );
    assert_eq!((dev_after.status, backup_after.status), (BranchStatus::Ready, BranchStatus::Ready), "nothing written");
    assert_eq!(started, BranchDeleteOutcome::Started);
    assert_eq!((dev_final.status, backup_final.status), (BranchStatus::Deleting, BranchStatus::Deleting));
    assert_eq!(
        refused_for_dev,
        BranchDeleteOutcome::HasChildren { backup: None, children: vec!["kid".to_string()] }
    );
    assert_eq!(other_dev_after.status, BranchStatus::Ready);
}

// Step 5 Task 6, ruling 1: a pending timeline is cleared by compare-and-swap before it's
// deleted, so a run whose swap fails leaves the timeline to whoever owns it now.
#[tokio::test]
#[ignore]
async fn swap_pending_timeline_is_a_compare_and_swap() {
    let client = client().await;
    let (id, _ep) = ready_database(&client).await;
    let dev = ready_branch(&client, id, "dev", main_of(id)).await;
    let pending = start_reset(&client, dev).await;
    let other = hex32();

    let wrong_expected = client.swap_pending_timeline(dev, Some(&other), None).await.unwrap();
    let after_wrong = branch(&client, dev).await.pending_timeline_id;
    let cleared = client.swap_pending_timeline(dev, Some(&pending), None).await.unwrap();
    let after_clear = branch(&client, dev).await.pending_timeline_id;
    let cleared_twice = client.swap_pending_timeline(dev, Some(&pending), None).await.unwrap();
    let rerecorded = client.swap_pending_timeline(dev, None, Some(&pending)).await.unwrap();
    let after_rerecord = branch(&client, dev).await.pending_timeline_id;
    let over_another = client.swap_pending_timeline(dev, None, Some(&other)).await.unwrap();
    let outcome = client.begin_branch_delete(dev).await.unwrap();
    let while_deleting = client.swap_pending_timeline(dev, Some(&pending), None).await.unwrap();
    let after_delete = branch(&client, dev).await.pending_timeline_id;
    cleanup(&client, id).await;

    assert!(!wrong_expected);
    assert_eq!(after_wrong.as_deref(), Some(pending.as_str()));
    assert!(cleared);
    assert_eq!(after_clear, None);
    assert!(!cleared_twice);
    assert!(rerecorded);
    assert_eq!(after_rerecord.as_deref(), Some(pending.as_str()));
    assert!(!over_another, "only from the expected value");
    assert_eq!(outcome, BranchDeleteOutcome::Started);
    assert!(!while_deleting, "the delete job owns it now");
    assert_eq!(after_delete.as_deref(), Some(pending.as_str()));
}

// Step 5 Task 6, fix round 1: reconcile's `finish_switch` claims a resetting branch only once
// it's stale, and the claim refreshes it, so only the first of two overlapping runs wins.
#[tokio::test]
#[ignore]
async fn claim_stale_reset_claims_only_a_stale_resetting_branch_once() {
    let client = client().await;
    let (id, _ep) = ready_database(&client).await;
    let stale = ready_branch(&client, id, "stale", main_of(id)).await;
    let fresh = ready_branch(&client, id, "fresh", main_of(id)).await;
    let ready = ready_branch(&client, id, "ready", main_of(id)).await;
    let deleting = ready_branch(&client, id, "deleting", main_of(id)).await;
    for b in [stale, fresh, main_of(id), deleting] {
        start_reset(&client, b).await;
    }
    assert_eq!(client.begin_branch_delete(deleting).await.unwrap(), BranchDeleteOutcome::Started);
    for b in [stale, ready, main_of(id), deleting] {
        sqlx::query("UPDATE branches SET updated_at = now() - interval '700 seconds' WHERE id = $1")
            .bind(b.as_uuid())
            .execute(client.pool())
            .await
            .unwrap();
    }

    let claimed = [
        client.claim_stale_reset(stale, 600).await.unwrap(),
        client.claim_stale_reset(fresh, 600).await.unwrap(),
        client.claim_stale_reset(ready, 600).await.unwrap(),
        client.claim_stale_reset(main_of(id), 600).await.unwrap(),
        client.claim_stale_reset(deleting, 600).await.unwrap(),
    ];
    let claimed_again = client.claim_stale_reset(stale, 600).await.unwrap();
    let rows = [
        branch(&client, stale).await,
        branch(&client, main_of(id)).await,
        branch(&client, deleting).await,
    ];
    cleanup(&client, id).await;

    assert_eq!(claimed, [true, false, false, true, false], "stale, fresh, ready, main, deleting");
    assert!(!claimed_again, "the first claim refreshed it");
    assert_eq!(
        rows.each_ref().map(|b| b.status),
        [BranchStatus::Resetting, BranchStatus::Resetting, BranchStatus::Deleting]
    );
    assert!(rows[0].pending_timeline_id.is_some(), "the claim writes nothing else");
}

// Step 5 re-review I-2: where a branch's history begins, set with its timeline and moved with it.
#[tokio::test]
#[ignore]
async fn history_start_is_set_when_ready_and_follows_the_timeline() {
    let client = client().await;
    let (id, _ep) = ready_database(&client).await;
    let main = main_branch(&client, id).await;
    let past: chrono::DateTime<chrono::Utc> = "2026-09-20T09:00:00Z".parse().unwrap();
    let b = new_branch(id, "dev", main_of(id));
    assert_eq!(client.create_branch(&b).await.unwrap(), CreateBranchOutcome::Created);
    link_branch(&client, b.id).await;
    assert!(client
        .set_branch_status(b.id, &[BranchStatus::Pending], BranchStatus::Provisioning)
        .await
        .unwrap());
    assert!(client.set_branch_ready(b.id, "v1:branch", Some("0/1A2B3C"), Some(past)).await.unwrap());
    let dev_ready = branch(&client, b.id).await;
    // A restore: the backup and the branch both keep the old history (Neon copies the layers).
    let pending = start_reset(&client, b.id).await;
    let backup = BranchId::new();
    assert!(client
        .switch_to_restored_timeline(b.id, &pending, backup, "dev-before-restore-20260930T120000Z")
        .await
        .unwrap());
    let (dev_restored, backup_row) = (branch(&client, b.id).await, branch(&client, backup).await);
    // A reset with no commit time: the new timeline's history starts now.
    assert!(client
        .set_branch_status(b.id, &[BranchStatus::Resetting], BranchStatus::Ready)
        .await
        .unwrap());
    let pending = start_reset(&client, b.id).await;
    let before: chrono::DateTime<chrono::Utc> =
        sqlx::query_scalar("SELECT now()").fetch_one(client.pool()).await.unwrap();
    assert!(client.switch_to_pending_timeline(b.id, &pending, None).await.unwrap());
    let dev_reset = branch(&client, b.id).await;
    cleanup(&client, id).await;

    assert_eq!(main.history_start, Some(main.created_at), "main falls back to created_at");
    assert_eq!(dev_ready.history_start, Some(past));
    assert_eq!(backup_row.history_start, Some(past), "the backup keeps the old timeline's");
    assert_eq!(dev_restored.history_start, Some(past), "a restore keeps the branch's history_start");
    assert!(dev_reset.history_start.is_some_and(|t| t >= before), "{:?}", dev_reset.history_start);
}

// Step 5 re-review I-1: a restore whose detach stays unresolved fails once its pending
// timeline was recorded long enough ago, keeping that timeline recorded.
#[tokio::test]
#[ignore]
async fn fail_stuck_restore_fails_only_a_resetting_branch_whose_pending_timeline_is_old() {
    let client = client().await;
    let (id, _ep) = ready_database(&client).await;
    let stuck = ready_branch(&client, id, "stuck", main_of(id)).await;
    let fresh = ready_branch(&client, id, "fresh", main_of(id)).await;
    let cleared = ready_branch(&client, id, "cleared", main_of(id)).await;
    let stuck_pending = start_reset(&client, stuck).await;
    start_reset(&client, fresh).await;
    let cleared_pending = start_reset(&client, cleared).await;
    let recorded_at = branch(&client, stuck).await.pending_timeline_at;
    assert!(client.swap_pending_timeline(cleared, Some(&cleared_pending), None).await.unwrap());
    let after_clear = branch(&client, cleared).await.pending_timeline_at;
    // Refreshing the claim (as every redelivery does) doesn't move the recorded time.
    sqlx::query("UPDATE branches SET pending_timeline_at = now() - interval '1300 seconds' WHERE id = ANY($1)")
        .bind(vec![stuck.as_uuid(), cleared.as_uuid()])
        .execute(client.pool())
        .await
        .unwrap();
    assert!(client
        .set_branch_status(stuck, &[BranchStatus::Resetting], BranchStatus::Resetting)
        .await
        .unwrap());
    client.set_branch_error(stuck, Some("detaching: timed out")).await.unwrap();

    let failed = [
        client.fail_stuck_restore(stuck, 1200, "gave up").await.unwrap(),
        client.fail_stuck_restore(fresh, 1200, "gave up").await.unwrap(),
        client.fail_stuck_restore(cleared, 1200, "gave up").await.unwrap(),
    ];
    let failed_again = client.fail_stuck_restore(stuck, 1200, "again").await.unwrap();
    let rows = [branch(&client, stuck).await, branch(&client, fresh).await];
    cleanup(&client, id).await;

    assert!(recorded_at.is_some(), "recording a pending timeline records when");
    assert_eq!(after_clear, None, "clearing it clears when");
    assert_eq!(failed, [true, false, false], "stuck, fresh, cleared");
    assert!(!failed_again, "only a resetting branch");
    assert_eq!(rows.each_ref().map(|b| b.status), [BranchStatus::Failed, BranchStatus::Resetting]);
    assert_eq!(rows[0].error.as_deref(), Some("gave up"));
    assert_eq!(rows[0].pending_timeline_id.as_deref(), Some(stuck_pending.as_str()), "kept");
}

#[tokio::test]
#[ignore]
async fn fail_branch_if_stale_only_fails_stale_branches_and_never_main() {
    let client = client().await;
    let (id, _ep) = ready_database(&client).await;
    let stale = new_branch(id, "stale", main_of(id));
    let fresh = new_branch(id, "fresh", main_of(id));
    let ready = new_branch(id, "ready", main_of(id));
    for b in [&stale, &fresh, &ready] {
        assert_eq!(client.create_branch(b).await.unwrap(), CreateBranchOutcome::Created);
    }
    assert!(client
        .set_branch_status(stale.id, &[BranchStatus::Pending], BranchStatus::Provisioning)
        .await
        .unwrap());
    assert!(client
        .set_branch_status(ready.id, &[BranchStatus::Pending], BranchStatus::Ready)
        .await
        .unwrap());
    let pending_main = insert_pending(&client).await;
    for b in [stale.id, ready.id, main_of(pending_main)] {
        sqlx::query("UPDATE branches SET updated_at = now() - interval '700 seconds' WHERE id = $1")
            .bind(b.as_uuid())
            .execute(client.pool())
            .await
            .unwrap();
    }

    let failed = [
        client.fail_branch_if_stale(stale.id, 600).await.unwrap(),
        client.fail_branch_if_stale(fresh.id, 600).await.unwrap(),
        client.fail_branch_if_stale(ready.id, 600).await.unwrap(),
        client.fail_branch_if_stale(main_of(pending_main), 600).await.unwrap(),
    ];
    let rows = [
        branch(&client, stale.id).await,
        branch(&client, fresh.id).await,
        branch(&client, ready.id).await,
        main_branch(&client, pending_main).await,
    ];
    cleanup(&client, id).await;
    cleanup(&client, pending_main).await;

    assert_eq!(failed, [true, false, false, false]);
    assert_eq!(
        rows.each_ref().map(|b| b.status),
        [BranchStatus::Failed, BranchStatus::Pending, BranchStatus::Ready, BranchStatus::Pending]
    );
    assert_eq!(rows[0].error.as_deref(), Some("provisioning timed out"));
}

#[tokio::test]
#[ignore]
async fn list_live_for_reconcile_carries_the_tenant_and_history_window() {
    let client = client().await;
    let (id, ep) = ready_database(&client).await;
    let tenant = hex32();
    assert!(client.set_neon_tenant(id, &tenant).await.unwrap());
    let hour = std::time::Duration::from_secs(3600);
    assert!(client.update_database_settings(id, &SettingsPatch { history_window: Some(hour), ..Default::default() }).await.unwrap());
    let fresh = insert_pending(&client).await;

    let rows = client.list_live_for_reconcile().await.unwrap();
    let row = rows.iter().find(|d| d.id == id).cloned();
    let fresh_row = rows.iter().find(|d| d.id == fresh).cloned();
    cleanup(&client, id).await;
    cleanup(&client, fresh).await;

    let row = row.expect("listed");
    assert_eq!(row.endpoint_id.as_deref(), Some(ep.as_str()));
    assert_eq!(row.neon_tenant_id.as_deref(), Some(tenant.as_str()));
    assert_eq!(row.history_window, hour);
    let fresh_row = fresh_row.expect("listed");
    assert_eq!(fresh_row.neon_tenant_id, None);
    assert_eq!(fresh_row.history_window, std::time::Duration::from_secs(7 * 24 * 3600));
}

#[tokio::test]
#[ignore]
async fn claim_branch_for_switch_starts_each_attempt_without_the_old_error() {
    let client = client().await;
    let (id, _ep) = ready_database(&client).await;
    let dev = ready_branch(&client, id, "dev", main_of(id)).await;
    client.set_branch_error(dev, Some("an old failure")).await.unwrap();
    client.set_branch_error(main_of(id), Some("an old failure")).await.unwrap();

    let claimed = client.claim_branch_for_switch(dev).await.unwrap();
    let dev_row = branch(&client, dev).await;
    let again = client.claim_branch_for_switch(dev).await.unwrap();
    let main_claimed = client.claim_branch_for_switch(main_of(id)).await.unwrap();
    let main_row = main_branch(&client, id).await;
    cleanup(&client, id).await;

    assert!(claimed);
    assert_eq!(dev_row.status, BranchStatus::Resetting);
    assert_eq!(dev_row.error, None, "each attempt starts clean");
    assert!(!again, "only a ready branch is claimed");
    assert!(main_claimed);
    assert_eq!(main_row.status, BranchStatus::Resetting);
    assert_eq!(main_row.error, None);
}

#[tokio::test]
#[ignore]
async fn begin_branch_delete_clears_the_old_error() {
    let client = client().await;
    let (id, _ep) = ready_database(&client).await;
    let dev = ready_branch(&client, id, "dev", main_of(id)).await;
    client.set_branch_error(dev, Some("an old failure")).await.unwrap();

    let moved = client.begin_branch_delete(dev).await.unwrap();
    let dev_row = branch(&client, dev).await;
    cleanup(&client, id).await;

    assert_eq!(moved, BranchDeleteOutcome::Started);
    assert_eq!(dev_row.status, BranchStatus::Deleting);
    assert_eq!(dev_row.error, None);
}

#[tokio::test]
#[ignore]
async fn client_sweeps_databases_an_earlier_run_left_behind() {
    // What a test that panicked before its cleanup leaves: a database, main with its endpoint
    // and a branch with its own.
    let first = client().await;
    let (id, main_ep) = ready_database(&first).await;
    let dev = ready_branch(&first, id, "dev", main_of(id)).await;
    let dev_ep = branch(&first, dev).await.endpoint_id.unwrap();
    // Databases a developer made by hand, with names like the tests': never swept.
    let by_hand = DatabaseId::new();
    first
        .insert_database_pending(by_hand, TenantId(DEMO_TENANT_ID), ProjectId(DEMO_PROJECT_ID), "live-test-by-hand")
        .await
        .unwrap();
    let lookalike = DatabaseId::new();
    first
        .insert_database_pending(
            lookalike,
            TenantId(DEMO_TENANT_ID),
            ProjectId(DEMO_PROJECT_ID),
            &format!("live-test-{}", DatabaseId::new()),
        )
        .await
        .unwrap();

    let second = client().await;
    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM databases WHERE id = $1")
        .bind(id.as_uuid())
        .fetch_one(second.pool())
        .await
        .unwrap();
    let kept: i64 = sqlx::query_scalar("SELECT count(*) FROM databases WHERE id = ANY($1)")
        .bind(vec![by_hand.as_uuid(), lookalike.as_uuid()])
        .fetch_one(second.pool())
        .await
        .unwrap();
    cleanup(&second, by_hand).await;
    cleanup(&second, lookalike).await;
    let branches: i64 = sqlx::query_scalar("SELECT count(*) FROM branches WHERE database_id = $1")
        .bind(id.as_uuid())
        .fetch_one(second.pool())
        .await
        .unwrap();
    let (main_left, dev_left) = (endpoint_exists(&second, &main_ep).await, endpoint_exists(&second, &dev_ep).await);
    cleanup(&second, id).await;

    assert_eq!(left, 0, "the test's database doesn't survive a new client");
    assert_eq!(kept, 2, "databases the tests didn't make are kept");
    assert_eq!(branches, 0);
    assert!(!main_left && !dev_left, "their endpoints go too");
}

// ---- data API: keys, authenticator password, settings ----

fn new_key(branch: BranchId, kind: DataKeyKind, role: &str, name: &str) -> (NewDataKey, String) {
    let generated = data_key::generate(kind);
    let sealed = (kind == DataKeyKind::Public).then(|| "v1:sealed-public-key".to_string());
    (
        NewDataKey {
            id: uuid::Uuid::new_v4(),
            branch_id: branch,
            kind,
            role: role.into(),
            name: name.into(),
            prefix: generated.prefix,
            sha256: generated.sha256,
            sealed,
        },
        generated.key,
    )
}

async fn last_used(client: &MetadataClient, key: uuid::Uuid) -> Option<chrono::DateTime<chrono::Utc>> {
    sqlx::query_scalar("SELECT last_used_at FROM data_api_keys WHERE id = $1")
        .bind(key)
        .fetch_one(client.pool())
        .await
        .unwrap()
}

#[tokio::test]
#[ignore]
async fn data_keys_insert_lookup_touch_revoke() {
    let client = client().await;
    let (id, _) = ready_database(&client).await;
    let branch = main_of(id);
    let (public, _) = new_key(branch, DataKeyKind::Public, "anon", "default");
    let (secret, _) = new_key(branch, DataKeyKind::Secret, "service_role", "backend");
    client.insert_data_key(&public).await.unwrap();
    client.insert_data_key(&secret).await.unwrap();

    let listed = client.list_data_keys(branch).await.unwrap();
    let found = client.lookup_data_key(&secret.sha256).await.unwrap();
    let unknown = client.lookup_data_key(&[7u8; 32]).await.unwrap();
    let has_public = client.has_live_public_key(branch).await.unwrap();

    let before = last_used(&client, secret.id).await;
    client.touch_data_key(secret.id).await.unwrap();
    let first = last_used(&client, secret.id).await;
    // A second touch within a minute leaves the stamp alone; an old one is refreshed.
    client.touch_data_key(secret.id).await.unwrap();
    let second = last_used(&client, secret.id).await;
    sqlx::query("UPDATE data_api_keys SET last_used_at = now() - interval '2 minutes' WHERE id = $1")
        .bind(secret.id)
        .execute(client.pool())
        .await
        .unwrap();
    let stale = last_used(&client, secret.id).await;
    client.touch_data_key(secret.id).await.unwrap();
    let refreshed = last_used(&client, secret.id).await;

    let public_revoked = client.revoke_data_key(branch, public.id).await.unwrap();
    let secret_revoked = client.revoke_data_key(branch, secret.id).await.unwrap();
    let again = client.revoke_data_key(branch, secret.id).await.unwrap();
    let unknown_revoked = client.revoke_data_key(branch, uuid::Uuid::new_v4()).await.unwrap();
    let after = client.list_data_keys(branch).await.unwrap();
    let gone = client.lookup_data_key(&secret.sha256).await.unwrap();
    cleanup(&client, id).await;

    assert_eq!(listed.iter().map(|k| k.id).collect::<Vec<_>>(), [secret.id, public.id]);
    assert_eq!(listed[1].sealed.as_deref(), Some("v1:sealed-public-key"));
    assert_eq!(listed[0].sealed, None);
    assert_eq!((listed[0].kind, listed[0].role.as_str(), listed[0].name.as_str()), (DataKeyKind::Secret, "service_role", "backend"));
    assert_eq!(listed[0].prefix, secret.prefix);
    assert!(listed.iter().all(|k| k.revoked_at.is_none() && k.branch_id == branch));
    let found = found.expect("secret key found");
    assert_eq!((found.key_id, found.branch_id, found.kind, found.role.as_str()), (secret.id, branch, DataKeyKind::Secret, "service_role"));
    assert!(unknown.is_none());
    assert!(has_public);
    assert!(before.is_none() && first.is_some());
    assert_eq!(first, second, "touched twice within a minute");
    assert!(refreshed > stale, "an old stamp is refreshed");
    assert!(!public_revoked, "the live public key can only be rotated");
    assert!(secret_revoked && !again && !unknown_revoked);
    assert_eq!(after.iter().map(|k| k.id).collect::<Vec<_>>(), [public.id]);
    assert!(gone.is_none());
}

#[tokio::test]
#[ignore]
async fn second_live_public_key_conflicts() {
    let client = client().await;
    let (id, _) = ready_database(&client).await;
    let (other, _) = ready_database(&client).await;
    let (first, _) = new_key(main_of(id), DataKeyKind::Public, "anon", "one");
    let (second, _) = new_key(main_of(id), DataKeyKind::Public, "anon", "two");
    let (elsewhere, _) = new_key(main_of(other), DataKeyKind::Public, "anon", "other branch");
    client.insert_data_key(&first).await.unwrap();
    let conflict = client.insert_data_key(&second).await;
    // Another branch is unaffected.
    client.insert_data_key(&elsewhere).await.unwrap();
    // A secret key with a non-anon role is fine; a public one with another role is not.
    let (bad_role, _) = new_key(main_of(id), DataKeyKind::Secret, "anon", "ok");
    let secret_anon = client.insert_data_key(&bad_role).await;
    let (public_other_role, _) = new_key(main_of(other), DataKeyKind::Public, "service_role", "bad");
    let check = client.insert_data_key(&public_other_role).await;
    let count = client.list_data_keys(main_of(id)).await.unwrap().len();
    cleanup(&client, id).await;
    cleanup(&client, other).await;
    assert!(matches!(conflict, Err(MetadataError::Conflict)), "{conflict:?}");
    assert!(secret_anon.is_ok());
    assert!(matches!(check, Err(MetadataError::Database(_))), "{check:?}");
    assert_eq!(count, 2);
}

#[tokio::test]
#[ignore]
async fn rotate_is_atomic() {
    let client = client().await;
    let (id, _) = ready_database(&client).await;
    let branch = main_of(id);
    let (public, _) = new_key(branch, DataKeyKind::Public, "anon", "default");
    let (secret, _) = new_key(branch, DataKeyKind::Secret, "service_role", "backend");
    client.insert_data_key(&public).await.unwrap();
    client.insert_data_key(&secret).await.unwrap();

    // The public key rotates: old revoked, new live, never two live.
    let (public2, _) = new_key(branch, DataKeyKind::Public, "anon", "default");
    let rotated_public = client.rotate_data_key(branch, public.id, &public2).await.unwrap();
    let lookup_old = client.lookup_data_key(&public.sha256).await.unwrap();
    let lookup_new = client.lookup_data_key(&public2.sha256).await.unwrap();

    // A dead old key: false, and the new key is not inserted.
    let (public3, _) = new_key(branch, DataKeyKind::Public, "anon", "default");
    let stale = client.rotate_data_key(branch, public.id, &public3).await.unwrap();
    let missing = client.rotate_data_key(branch, uuid::Uuid::new_v4(), &public3).await.unwrap();
    let inserted = client.lookup_data_key(&public3.sha256).await.unwrap();

    // Another branch's key can't be rotated through this one.
    let (other_db, _) = ready_database(&client).await;
    let (foreign, _) = new_key(main_of(other_db), DataKeyKind::Secret, "service_role", "foreign");
    client.insert_data_key(&foreign).await.unwrap();
    let (replacement, _) = new_key(branch, DataKeyKind::Secret, "service_role", "stolen");
    let cross = client.rotate_data_key(branch, foreign.id, &replacement).await.unwrap();

    // A failing insert rolls the revoke back: a secret rotated into a second live public key.
    let (clash, _) = new_key(branch, DataKeyKind::Public, "anon", "clash");
    let failed = client.rotate_data_key(branch, secret.id, &clash).await;
    let secret_still_live = client.lookup_data_key(&secret.sha256).await.unwrap();

    // A duplicate hash fails the same way.
    let (dup, _) = new_key(branch, DataKeyKind::Secret, "service_role", "dup");
    let dup = NewDataKey { sha256: secret.sha256, ..dup };
    let failed_dup = client.rotate_data_key(branch, secret.id, &dup).await;
    let still_live = client.lookup_data_key(&secret.sha256).await.unwrap();

    let live = client.list_data_keys(branch).await.unwrap();
    cleanup(&client, id).await;
    cleanup(&client, other_db).await;
    assert!(rotated_public);
    assert!(lookup_old.is_none());
    assert_eq!(lookup_new.unwrap().key_id, public2.id);
    assert!(!stale && !missing);
    assert!(inserted.is_none());
    assert!(!cross);
    assert!(matches!(failed, Err(MetadataError::Conflict)), "{failed:?}");
    assert!(secret_still_live.is_some());
    assert!(failed_dup.is_err());
    assert!(still_live.is_some());
    let mut ids: Vec<_> = live.iter().map(|k| k.id).collect();
    ids.sort();
    let mut want = vec![public2.id, secret.id];
    want.sort();
    assert_eq!(ids, want);
}

#[tokio::test]
#[ignore]
async fn lookup_ignores_revoked_keys_and_deleted_branches() {
    let client = client().await;
    let (id, _) = ready_database(&client).await;
    let branch = main_of(id);
    let (secret, _) = new_key(branch, DataKeyKind::Secret, "service_role", "backend");
    let (revoked, _) = new_key(branch, DataKeyKind::Secret, "service_role", "old");
    client.insert_data_key(&secret).await.unwrap();
    client.insert_data_key(&revoked).await.unwrap();
    assert!(client.revoke_data_key(branch, revoked.id).await.unwrap());
    let live = client.lookup_data_key(&secret.sha256).await.unwrap();
    let revoked_lookup = client.lookup_data_key(&revoked.sha256).await.unwrap();
    assert!(client.begin_delete(id).await.unwrap());
    let deleting = client.lookup_data_key(&secret.sha256).await.unwrap();
    client.mark_branch_deleted(branch).await.unwrap();
    client.mark_deleted(id).await.unwrap();
    let deleted = client.lookup_data_key(&secret.sha256).await.unwrap();
    cleanup(&client, id).await;
    assert!(live.is_some());
    assert!(revoked_lookup.is_none());
    assert!(deleting.is_none(), "a deleting branch's keys don't resolve");
    assert!(deleted.is_none());
}

#[tokio::test]
#[ignore]
async fn deleting_a_branch_row_cascades_keys() {
    let client = client().await;
    let (id, _) = ready_database(&client).await;
    let (secret, _) = new_key(main_of(id), DataKeyKind::Secret, "service_role", "backend");
    client.insert_data_key(&secret).await.unwrap();
    cleanup(&client, id).await;
    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM data_api_keys WHERE id = $1")
        .bind(secret.id)
        .fetch_one(client.pool())
        .await
        .unwrap();
    assert_eq!(left, 0);
}

#[tokio::test]
#[ignore]
async fn settings_patch_bounds_are_enforced_by_checks() {
    let client = client().await;
    let id = insert_pending(&client).await;
    let defaults = client.get_database(id).await.unwrap().unwrap();
    let bad = [
        SettingsPatch { max_rows: Some(0), ..Default::default() },
        SettingsPatch { max_rows: Some(100_001), ..Default::default() },
        SettingsPatch { statement_timeout_ms: Some(99), ..Default::default() },
        SettingsPatch { statement_timeout_ms: Some(120_001), ..Default::default() },
        SettingsPatch { max_connections: Some(0), ..Default::default() },
        SettingsPatch { max_connections: Some(51), ..Default::default() },
        SettingsPatch { default_schema: Some("Public".into()), ..Default::default() },
        SettingsPatch { default_schema: Some("1abc".into()), ..Default::default() },
        SettingsPatch { default_schema: Some("a".repeat(64)), ..Default::default() },
        SettingsPatch { default_schema: Some(String::new()), ..Default::default() },
        // One bad field spoils the whole patch.
        SettingsPatch { max_branches: Some(9), max_rows: Some(0), ..Default::default() },
    ];
    let mut rejected = 0;
    for patch in &bad {
        if matches!(client.update_database_settings(id, patch).await, Err(MetadataError::Database(_))) {
            rejected += 1;
        }
    }
    let unchanged = client.get_database(id).await.unwrap().unwrap();
    let edges = SettingsPatch {
        max_rows: Some(100_000),
        statement_timeout_ms: Some(100),
        max_connections: Some(50),
        default_schema: Some("_app_1".into()),
        max_branches: Some(7),
        ..Default::default()
    };
    let applied = client.update_database_settings(id, &edges).await.unwrap();
    let after = client.get_database(id).await.unwrap().unwrap();
    // Unset fields keep their value.
    client
        .update_database_settings(id, &SettingsPatch { max_rows: Some(1), ..Default::default() })
        .await
        .unwrap();
    let partial = client.get_database(id).await.unwrap().unwrap();
    assert!(client.begin_delete(id).await.unwrap());
    let on_deleting = client
        .update_database_settings(id, &SettingsPatch { max_rows: Some(5), ..Default::default() })
        .await
        .unwrap();
    cleanup(&client, id).await;
    assert_eq!(
        (defaults.max_rows, defaults.statement_timeout_ms, defaults.max_connections, defaults.default_schema.as_str()),
        (1000, 8000, 5, "public")
    );
    assert_eq!(rejected, bad.len());
    assert_eq!(unchanged, defaults);
    assert!(applied);
    assert_eq!(
        (after.max_rows, after.statement_timeout_ms, after.max_connections, after.default_schema.as_str(), after.max_branches),
        (100_000, 100, 50, "_app_1", 7)
    );
    assert_eq!((partial.max_rows, partial.statement_timeout_ms, partial.max_connections), (1, 100, 50));
    assert!(!on_deleting);
}

#[tokio::test]
#[ignore]
async fn branch_connection_returns_settings() {
    let client = client().await;
    let id = insert_pending(&client).await;
    let pending = client.branch_connection(main_of(id)).await.unwrap();
    let ep = link_endpoint(&client, id).await;
    assert!(make_ready(&client, id, "v1:owner").await);
    let ready = client.branch_connection(main_of(id)).await.unwrap().expect("row");
    client
        .update_database_settings(
            id,
            &SettingsPatch { max_rows: Some(50), default_schema: Some("app".into()), ..Default::default() },
        )
        .await
        .unwrap();
    client.set_authenticator_password(main_of(id), "v1:auth").await.unwrap();
    // A second write never replaces it: a racing run reads the first one back.
    client.set_authenticator_password(main_of(id), "v1:later").await.unwrap();
    let with_password = client.branch_connection(main_of(id)).await.unwrap().expect("row");
    let missing = client.branch_connection(BranchId::new()).await.unwrap();
    let record = main_branch(&client, id).await;
    cleanup(&client, id).await;
    assert_eq!(pending.expect("pending branches have a row").status, BranchStatus::Pending);
    assert_eq!(ready.branch_id, main_of(id));
    assert_eq!(ready.status, BranchStatus::Ready);
    assert_eq!(ready.endpoint_id.as_deref(), Some(ep.as_str()));
    assert_eq!(ready.database_name.as_deref(), Some("db"));
    assert_eq!(ready.authenticator_password_enc, None);
    assert_eq!((ready.settings.max_rows, ready.settings.statement_timeout_ms), (1000, 8000));
    assert_eq!((ready.settings.max_connections, ready.settings.default_schema.as_str()), (5, "public"));
    assert_eq!(with_password.authenticator_password_enc.as_deref(), Some("v1:auth"));
    assert_eq!(with_password.settings.max_rows, 50);
    assert_eq!(with_password.settings.default_schema, "app");
    assert!(missing.is_none());
    assert_eq!(record.authenticator_password_enc.as_deref(), Some("v1:auth"));
}

#[tokio::test]
#[ignore]
async fn branches_missing_data_api_lists_ready_branches_without_key_or_password() {
    let client = client().await;
    let (bare, _) = ready_database(&client).await;
    let (keyed, _) = ready_database(&client).await;
    let (passworded, _) = ready_database(&client).await;
    let (complete, _) = ready_database(&client).await;
    let not_ready = insert_pending(&client).await;
    for (db, key, password) in [(keyed, true, false), (passworded, false, true), (complete, true, true)] {
        if key {
            let (k, _) = new_key(main_of(db), DataKeyKind::Public, "anon", "default");
            client.insert_data_key(&k).await.unwrap();
        }
        if password {
            client.set_authenticator_password(main_of(db), "v1:auth").await.unwrap();
        }
    }
    let missing = client.branches_missing_data_api().await.unwrap();
    // A revoked-and-rotated public key still counts; a secret key alone doesn't.
    let (secret, _) = new_key(main_of(complete), DataKeyKind::Secret, "service_role", "s");
    client.insert_data_key(&secret).await.unwrap();
    let live_public = client.list_data_keys(main_of(complete)).await.unwrap();
    let public = live_public.iter().find(|k| k.kind == DataKeyKind::Public).unwrap().id;
    let (next, _) = new_key(main_of(complete), DataKeyKind::Public, "anon", "default");
    client.rotate_data_key(main_of(complete), public, &next).await.unwrap();
    let after_rotate = client.branches_missing_data_api().await.unwrap();
    cleanup(&client, bare).await;
    cleanup(&client, keyed).await;
    cleanup(&client, passworded).await;
    cleanup(&client, complete).await;
    cleanup(&client, not_ready).await;
    for db in [bare, keyed, passworded] {
        assert!(missing.contains(&main_of(db)), "{db} should be listed");
    }
    assert!(!missing.contains(&main_of(complete)));
    assert!(!missing.contains(&main_of(not_ready)), "a pending branch isn't listed");
    assert!(!after_rotate.contains(&main_of(complete)));
}
