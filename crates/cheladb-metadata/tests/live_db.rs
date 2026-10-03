//! Opt-in tests against a real metadata Postgres (`just deps-up` first):
//!
//! ```sh
//! cargo test -p cheladb-metadata --test live_db -- --ignored --test-threads=1
//! ```
//!
//! They use the seeded demo tenant/project and delete the rows they create. Every run names its
//! databases `live-test-<run id>-<id>` and sweeps only those, so runs can overlap. Rows a crashed
//! run leaves survive for up to an hour (`STALE_AFTER`); `just sweep-dbs` is the manual remedy.

use cheladb_metadata::{
    BranchDeleteOutcome, BranchKind, BranchRecord, BranchStatus, CreateBranchOutcome, DatabaseSettings, DatabaseStatus,
    BeginDelete, ClaimOutcome, CopyUpsert, DatabaseOrigin, InsertRoleOutcome, MetadataClient, MetadataConfig,
    MetadataError, NewBranch, NewDataKey, NewEndpoint, NewRoleCopy, ObservedDatabase, PasswordState, ResyncApplied,
    ResyncWrite, RoleLevel, RoleOrigin, SealedChange, SettingsPatch,
    DataApiState, EnableStart, ServedDatabase,
    BeginEnable, BeginProjectDelete, NewAppSession, NewProjectAuth, OnDelete, ProjectStatus, RefreshNext,
    RefreshOutcome, RenameProject, AppUsersStatus,
    SAVED_QUERY_CAP, SavedQuery, SavedQueryPatch, SavedQueryWrite,
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

/// This process's run id: every database a test makes is named `live-test-<run>-<id>`, so
/// concurrent runs (two terminals, two worktrees) never see or delete each other's rows.
fn run_id() -> &'static str {
    static RUN: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    RUN.get_or_init(|| hex32()[..8].to_string())
}

/// Rows older than this are swept whoever made them (a run that died). It must exceed the longest
/// live run, or a long run's databases could be swept from under it.
const STALE_AFTER: std::time::Duration = std::time::Duration::from_secs(3600);

/// Deletes the databases this run made (with their branches and endpoints): a test that
/// panics skips its cleanup, and the fake sealed values it leaves make the key check refuse the
/// dev stack's key. Only this run's own (in the demo project, named `live-test-<run>-<its own
/// id>` as [`insert_pending`] names them) plus the harness's own rows older than [`STALE_AFTER`]
/// (`live-test-<8 hex>-<id>`, or the older `live-test-<id>`; left by runs that died); a database made by hand
/// survives, and so does every other live run's. One transaction, in the lock order: the
/// database rows, then their branch rows, then endpoints.
async fn sweep_leftovers(client: &MetadataClient) {
    let mut tx = client.pool().begin().await.unwrap();
    // Projects the harness made (this run's, or older than STALE_AFTER and a run's): their
    // sessions and app-user rows go, and the projects are tombstoned.
    let mut projects: Vec<uuid::Uuid> = sqlx::query_scalar(
        "SELECT id FROM projects
         WHERE tenant_id = $1
           AND (name LIKE 'live-test-' || $2 || '%'
                OR (created_at < now() - make_interval(secs => $3)
                    AND name ~ '^live-test-[0-9a-f]{8}'))
         ORDER BY id FOR UPDATE",
    )
    .bind(DEMO_TENANT_ID)
    .bind(run_id())
    .bind(STALE_AFTER.as_secs_f64())
    .fetch_all(&mut *tx)
    .await
    .unwrap();
    for sql in [
        "DELETE FROM app_sessions WHERE project_id = ANY($1)",
        "DELETE FROM project_auth WHERE project_id = ANY($1)",
        "UPDATE projects SET status = 'deleted', deleted_at = now(), updated_at = now()
         WHERE id = ANY($1) AND status <> 'deleted'",
    ] {
        sqlx::query(sql).bind(&projects).execute(&mut *tx).await.unwrap();
    }
    projects.push(DEMO_PROJECT_ID);
    // FOR UPDATE, not the usual FOR NO KEY UPDATE: the rows are deleted below anyway.
    let ids: Vec<uuid::Uuid> = sqlx::query_scalar(
        "SELECT id FROM databases
         WHERE project_id = ANY($4)
           AND (name = 'live-test-' || $2 || '-' || id::text
                OR (created_at < now() - make_interval(secs => $3)
                    AND (name = 'live-test-' || id::text
                         OR name ~ ('^live-test-[0-9a-f]{8}-' || id::text || '$'))))
         ORDER BY id FOR UPDATE",
    )
    .bind(DEMO_PROJECT_ID)
    .bind(run_id())
    .bind(STALE_AFTER.as_secs_f64())
    .bind(&projects)
    .fetch_all(&mut *tx)
    .await
    .unwrap();
    if ids.is_empty() {
        tx.commit().await.unwrap();
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
        "DELETE FROM saved_queries WHERE database_id = ANY($1)",
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
            &format!("live-test-{}-{id}", run_id()),
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
        role_copies: &[],
    }
}

/// Creates an endpoint on a fresh timeline and links it to `branch`; returns its id.
async fn link_branch(client: &MetadataClient, branch: BranchId) -> String {
    let ep = ep_id();
    let (tenant, timeline) = (hex32(), hex32());
    assert!(client
        .create_endpoint_for_branch(branch, &timeline, &new_endpoint(&ep, &tenant, &timeline), false)
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

/// True when the delete began.
async fn begin_delete(client: &MetadataClient, id: DatabaseId) -> bool {
    client.begin_delete(id).await.unwrap() == BeginDelete::Started
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
    let _sealed = sealed_lock().await;
    let client = client().await;
    let id = insert_pending(&client).await;
    assert!(begin_delete(&client, id).await);
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
    let _sealed = sealed_lock().await;
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

/// Advisory lock id shared by the key-check tests (exclusive) and every test that stores a
/// sealed owner password (shared): the key check counts all sealed branches, so the two kinds
/// must not overlap, in this run or another.
const KEY_CHECK_LOCK: i64 = 7_310_001;

/// Opens a connection that holds the advisory lock for as long as it lives (closed on drop).
async fn advisory_lock(statement: &'static str) -> sqlx::PgConnection {
    use sqlx::Connection;
    let database_url = std::env::var("CHELADB_METADATA_DATABASE_URL").unwrap_or_else(|_| {
        "postgres://cheladb:cheladb@127.0.0.1:5433/cheladb_metadata".to_string()
    });
    let mut conn = sqlx::PgConnection::connect(&database_url).await.unwrap();
    sqlx::query(statement).bind(KEY_CHECK_LOCK).execute(&mut conn).await.unwrap();
    conn
}

/// Exclusive: the key-check tests rewrite one global row and count every sealed branch.
async fn key_check_lock() -> sqlx::PgConnection {
    advisory_lock("SELECT pg_advisory_lock($1)").await
}

/// Shared: tests that store sealed credentials may overlap each other, not a key-check test.
async fn sealed_lock() -> sqlx::PgConnection {
    advisory_lock("SELECT pg_advisory_lock_shared($1)").await
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
    let _lock = key_check_lock().await;
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
    let _lock = key_check_lock().await;
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
    let _sealed = sealed_lock().await;
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
    assert!(begin_delete(&client, gone).await);
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
    let _sealed = sealed_lock().await;
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
        .create_endpoint_for_branch(main_of(id), &timeline, &new_endpoint(&ep, &tenant, &timeline), false)
        .await
        .unwrap();
    let main = main_branch(&client, id).await;
    let roles = client.neon_roles(&ep).await.unwrap();
    let dbs = client.neon_databases(&ep).await.unwrap();
    let endpoint = client.neon_endpoint(&ep).await.unwrap().unwrap();

    // Already linked: refused, and the second endpoint isn't written.
    let ep2 = ep_id();
    let again = client
        .create_endpoint_for_branch(main_of(id), &timeline, &new_endpoint(&ep2, &tenant, &timeline), false)
        .await
        .unwrap();
    let ep2_written = endpoint_exists(&client, &ep2).await;

    // Deleting, deleted and missing branches are refused without writing anything.
    let deleting = insert_pending(&client).await;
    assert!(begin_delete(&client, deleting).await);
    let deleted = insert_pending(&client).await;
    assert!(begin_delete(&client, deleted).await);
    client.mark_branch_deleted(main_of(deleted)).await.unwrap();
    client.mark_deleted(deleted).await.unwrap();
    let mut refused = Vec::new();
    for target in [main_of(deleting), main_of(deleted), BranchId::new()] {
        let ep = ep_id();
        let ok = client
            .create_endpoint_for_branch(target, &timeline, &new_endpoint(&ep, &tenant, &timeline), false)
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
    let _sealed = sealed_lock().await;
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
    let _sealed = sealed_lock().await;
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
    assert!(begin_delete(&client, deleting).await);
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
    assert!(begin_delete(&client, deleted).await);
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
    let _sealed = sealed_lock().await;
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
    let _sealed = sealed_lock().await;
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
    assert!(begin_delete(&client, other).await);
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
    let _sealed = sealed_lock().await;
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
    let _sealed = sealed_lock().await;
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
    assert!(begin_delete(&client, gone).await);
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
    let _sealed = sealed_lock().await;
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
    let _sealed = sealed_lock().await;
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

// T6: the delete's own UPDATE refuses a protected branch (a flag set after the API's check).
#[tokio::test]
#[ignore]
async fn begin_branch_delete_refuses_a_protected_branch() {
    let _sealed = sealed_lock().await;
    let client = client().await;
    let (id, _ep) = ready_database(&client).await;
    let dev = ready_branch(&client, id, "dev", main_of(id)).await;
    assert!(client.set_branch_protected(dev, true).await.unwrap().is_some());
    let refused = client.begin_branch_delete(dev).await.unwrap();
    let row = branch(&client, dev).await;
    assert!(client.set_branch_protected(dev, false).await.unwrap().is_some());
    let moved = client.begin_branch_delete(dev).await.unwrap();
    cleanup(&client, id).await;
    assert_eq!(refused, BranchDeleteOutcome::Protected);
    assert_eq!(row.status, BranchStatus::Ready, "nothing written");
    assert_eq!(moved, BranchDeleteOutcome::Started);
}

#[tokio::test]
#[ignore]
async fn begin_branch_delete_takes_backups_along() {
    let _sealed = sealed_lock().await;
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
    let _sealed = sealed_lock().await;
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
    let _sealed = sealed_lock().await;
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
    let _sealed = sealed_lock().await;
    let client = client().await;
    let (id, _ep) = ready_database(&client).await;
    let dev = ready_branch(&client, id, "dev", main_of(id)).await;
    let gone = ready_branch(&client, id, "gone", main_of(id)).await;
    assert_eq!(client.begin_branch_delete(gone).await.unwrap(), BranchDeleteOutcome::Started);
    client.mark_branch_deleted(gone).await.unwrap();

    assert!(begin_delete(&client, id).await);
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
    assert!(begin_delete(&client, id).await);
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
    let _sealed = sealed_lock().await;
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
    let _sealed = sealed_lock().await;
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
        let delete = tokio::spawn(async move { a.begin_delete(id).await.map(|o| o == BeginDelete::Started) });
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
        c.create_endpoint_for_branch(main_of(id), &timeline, &new_endpoint(&ep_racer, &tenant, &timeline), false).await
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
    let _sealed = sealed_lock().await;
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
    let _sealed = sealed_lock().await;
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
    let _sealed = sealed_lock().await;
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
    let _sealed = sealed_lock().await;
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
        let delete = tokio::spawn(async move { a.begin_delete(id).await.map(|o| o == BeginDelete::Started) });
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
    let _sealed = sealed_lock().await;
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
    let _sealed = sealed_lock().await;
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
    let _sealed = sealed_lock().await;
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
    let _sealed = sealed_lock().await;
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
    let _sealed = sealed_lock().await;
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
    let _sealed = sealed_lock().await;
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
    let _sealed = sealed_lock().await;
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
    let _sealed = sealed_lock().await;
    let client = client().await;
    let (id, _ep) = ready_database(&client).await;
    let dev = ready_branch(&client, id, "dev", main_of(id)).await;
    client.set_branch_error(dev, Some("an old failure")).await.unwrap();
    client.set_branch_error(main_of(id), Some("an old failure")).await.unwrap();

    let claimed = client.claim_branch_for_switch(dev, false).await.unwrap() == ClaimOutcome::Claimed;
    let dev_row = branch(&client, dev).await;
    let again = client.claim_branch_for_switch(dev, false).await.unwrap() == ClaimOutcome::Claimed;
    let main_claimed = client.claim_branch_for_switch(main_of(id), false).await.unwrap() == ClaimOutcome::Claimed;
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
    let _sealed = sealed_lock().await;
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
    let _sealed = sealed_lock().await;
    // What a test that panicked before its cleanup leaves: a database, main with its endpoint
    // and a branch with its own.
    let first = client().await;
    let (id, main_ep) = ready_database(&first).await;
    let dev = ready_branch(&first, id, "dev", main_of(id)).await;
    let dev_ep = branch(&first, dev).await.endpoint_id.unwrap();
    // Databases a developer made by hand, with names like the tests': never swept.
    let by_hand = DatabaseId::new();
    first
        .insert_database_pending(by_hand, TenantId(DEMO_TENANT_ID), ProjectId(DEMO_PROJECT_ID), &format!("live-test-by-hand-{}", run_id()))
        .await
        .unwrap();
    let lookalike = DatabaseId::new();
    first
        .insert_database_pending(
            lookalike,
            TenantId(DEMO_TENANT_ID),
            ProjectId(DEMO_PROJECT_ID),
            &format!("live-test-{}-{}", run_id(), DatabaseId::new()),
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
            databases: None,
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
    let _sealed = sealed_lock().await;
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
    let _sealed = sealed_lock().await;
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
    let _sealed = sealed_lock().await;
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
    let _sealed = sealed_lock().await;
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
    assert!(begin_delete(&client, id).await);
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
    let _sealed = sealed_lock().await;
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
    assert!(begin_delete(&client, id).await);
    let on_deleting = client
        .update_database_settings(id, &SettingsPatch { max_rows: Some(5), ..Default::default() })
        .await
        .unwrap();
    cleanup(&client, id).await;
    assert_eq!(
        (defaults.max_rows, defaults.statement_timeout_ms, defaults.max_connections),
        (1000, 8000, 5)
    );
    assert_eq!(rejected, bad.len());
    assert_eq!(unchanged, defaults);
    assert!(applied);
    assert_eq!(
        (after.max_rows, after.statement_timeout_ms, after.max_connections, after.max_branches),
        (100_000, 100, 50, 7)
    );
    assert_eq!((partial.max_rows, partial.statement_timeout_ms, partial.max_connections), (1, 100, 50));
    assert!(!on_deleting);
}

#[tokio::test]
#[ignore]
async fn branch_connection_returns_settings() {
    let _sealed = sealed_lock().await;
    let client = client().await;
    let id = insert_pending(&client).await;
    let pending = client.branch_connection(main_of(id)).await.unwrap();
    let ep = link_endpoint(&client, id).await;
    assert!(make_ready(&client, id, "v1:owner").await);
    let ready = client.branch_connection(main_of(id)).await.unwrap().expect("row");
    client
        .update_database_settings(
            id,
            &SettingsPatch { max_rows: Some(50), ..Default::default() },
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
    assert_eq!(ready.settings.max_connections, 5);
    assert_eq!(with_password.authenticator_password_enc.as_deref(), Some("v1:auth"));
    assert_eq!(with_password.settings.max_rows, 50);
    assert!(missing.is_none());
    assert_eq!(record.authenticator_password_enc.as_deref(), Some("v1:auth"));
}

#[tokio::test]
#[ignore]
async fn branches_missing_data_api_lists_ready_branches_without_key_or_password() {
    let _sealed = sealed_lock().await;
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

// ---- Step 6a revision 2: login roles and databases managed from SQL ----

const SQL_VERIFIER: &str = "SCRAM-SHA-256$sql";

async fn flag_and_version(client: &MetadataClient, ep: &str) -> (bool, i64) {
    sqlx::query_as("SELECT needs_reconfigure, roles_version FROM neon_endpoints WHERE id = $1")
        .bind(ep)
        .fetch_one(client.pool())
        .await
        .unwrap()
}

async fn version(client: &MetadataClient, ep: &str) -> i64 {
    flag_and_version(client, ep).await.1
}

/// The constraint a failed statement violated.
async fn violated(client: &MetadataClient, sql: &'static str, ep: &str, name: &str) -> String {
    let err = sqlx::query(sql).bind(ep).bind(name).execute(client.pool()).await.unwrap_err();
    match err {
        sqlx::Error::Database(e) => e.constraint().unwrap_or("<none>").to_string(),
        other => panic!("not a database error: {other}"),
    }
}

/// A row as the receiver or a resync would have left it.
async fn seed_copy(client: &MetadataClient, ep: &str, name: &str, origin: &str, verifier: Option<&str>, sealed: Option<&str>) {
    sqlx::query(
        "INSERT INTO neon_roles (endpoint_id, name, scram_verifier, origin, password_enc)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(ep)
    .bind(name)
    .bind(verifier)
    .bind(origin)
    .bind(sealed)
    .execute(client.pool())
    .await
    .unwrap();
}

async fn set_forwarded(client: &MetadataClient, ep: &str, name: &str, secs_ago: i32) {
    sqlx::query(
        "UPDATE neon_roles SET forwarded_at = now() - make_interval(secs => $3)
         WHERE endpoint_id = $1 AND name = $2",
    )
    .bind(ep)
    .bind(name)
    .bind(f64::from(secs_ago))
    .execute(client.pool())
    .await
    .unwrap();
}

async fn names_of(client: &MetadataClient, ep: &str) -> Vec<String> {
    client.list_roles(ep).await.unwrap().into_iter().map(|r| r.name).collect()
}

fn upsert(name: &str, origin: RoleOrigin, verifier: &str) -> CopyUpsert {
    CopyUpsert {
        name: name.into(),
        origin,
        verifier: Some(verifier.into()),
        password_enc: SealedChange::Keep,
        login: true,
        level: RoleLevel::Plain,
        pending: false,
    }
}

fn observed(name: &str, owner: &str, system: bool) -> ObservedDatabase {
    ObservedDatabase { name: name.into(), owner: owner.into(), system }
}

async fn databases_of(client: &MetadataClient, branch: BranchId) -> Vec<(String, String, DatabaseOrigin)> {
    client
        .branch_databases(branch)
        .await
        .unwrap()
        .into_iter()
        .map(|d| (d.name, d.owner, d.origin))
        .collect()
}

#[tokio::test]
#[ignore]
async fn migration_maps_revision_one_rows() {
    let pool = raw_pool().await;
    let schema = format!("step6a_migration_{}", hex32());
    let mut conn = pool.acquire().await.unwrap();
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}; SET search_path TO {schema};")))
        .execute(&mut *conn)
        .await
        .unwrap();
    // The tables as revision 1 left them (the real migrations), and a stub of `branches`.
    sqlx::raw_sql(include_str!("../migrations/20260928000003_neon_endpoints.sql")).execute(&mut *conn).await.unwrap();
    sqlx::raw_sql(include_str!("../migrations/20261003000001_login_roles.sql")).execute(&mut *conn).await.unwrap();
    sqlx::raw_sql("CREATE TABLE branches (id uuid PRIMARY KEY, endpoint_id text, status text NOT NULL)")
        .execute(&mut *conn)
        .await
        .unwrap();
    let ep = ep_id();
    let (live, gone) = (uuid::Uuid::new_v4(), uuid::Uuid::new_v4());
    for sql in [
        "INSERT INTO neon_endpoints (id, tenant_id, timeline_id, compute_token_sha256) VALUES ($1, 't', 'l', 'cd')",
        "INSERT INTO neon_roles (endpoint_id, name, scram_verifier) VALUES ($1, 'postgres', 'SCRAM-SHA-256$o')",
        "INSERT INTO neon_roles (endpoint_id, name, scram_verifier, kind, privileged, password_enc)
         VALUES ($1, 'reader', 'SCRAM-SHA-256$r', 'login', false, 'v1:r')",
        "INSERT INTO neon_roles (endpoint_id, name, scram_verifier, kind, privileged, password_enc)
         VALUES ($1, 'writer', 'SCRAM-SHA-256$w', 'login', true, 'v1:w')",
        "INSERT INTO neon_databases (endpoint_id, name, owner) VALUES ($1, 'db', 'postgres')",
    ] {
        sqlx::query(sql).bind(&ep).execute(&mut *conn).await.unwrap();
    }
    sqlx::query("INSERT INTO branches (id, endpoint_id, status) VALUES ($1, $2, 'ready'), ($3, NULL, 'deleted')")
        .bind(live)
        .bind(&ep)
        .bind(gone)
        .execute(&mut *conn)
        .await
        .unwrap();

    sqlx::raw_sql(include_str!("../migrations/20261004000001_sql_roles.sql")).execute(&mut *conn).await.unwrap();

    let rows: Vec<(String, String, String, bool, bool, Option<String>)> = sqlx::query_as(
        "SELECT name, origin, level, api_owner_level, login, password_enc FROM neon_roles ORDER BY name",
    )
    .fetch_all(&mut *conn)
    .await
    .unwrap();
    let dbs: Vec<(uuid::Uuid, String, String, String)> =
        sqlx::query_as("SELECT branch_id, name, owner, origin FROM branch_databases")
            .fetch_all(&mut *conn)
            .await
            .unwrap();
    let settings: i32 = sqlx::query_scalar("SELECT compute_settings_version FROM neon_endpoints")
        .fetch_one(&mut *conn)
        .await
        .unwrap();
    let protected: bool = sqlx::query_scalar("SELECT protected FROM branches WHERE id = $1")
        .bind(live)
        .fetch_one(&mut *conn)
        .await
        .unwrap();
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!("SET search_path TO public; DROP SCHEMA {schema} CASCADE;")))
        .execute(&mut *conn)
        .await
        .unwrap();

    let s = |v: &str| v.to_string();
    assert_eq!(
        rows,
        vec![
            (s("postgres"), s("owner"), s("owner_level"), false, true, None),
            (s("reader"), s("api"), s("plain"), false, true, Some(s("v1:r"))),
            (s("writer"), s("api"), s("owner_level"), true, true, Some(s("v1:w"))),
        ]
    );
    assert_eq!(dbs, vec![(live, s("db"), s("postgres"), s("main"))]);
    assert_eq!(settings, 0);
    assert!(!protected);
}

#[tokio::test]
#[ignore]
async fn owner_constraints() {
    let _sealed = sealed_lock().await;
    let client = client().await;
    let (id, ep) = ready_database(&client).await;
    let null_verifier = violated(
        &client,
        "INSERT INTO neon_roles (endpoint_id, name, scram_verifier, origin) VALUES ($1, $2, NULL, 'owner')",
        &ep,
        "o1",
    )
    .await;
    let sealed = violated(
        &client,
        "INSERT INTO neon_roles (endpoint_id, name, scram_verifier, origin, password_enc)
         VALUES ($1, $2, 'x', 'owner', 'v1:x')",
        &ep,
        "o2",
    )
    .await;
    let second_owner = violated(
        &client,
        "INSERT INTO neon_roles (endpoint_id, name, scram_verifier, origin) VALUES ($1, $2, 'x', 'owner')",
        &ep,
        "o3",
    )
    .await;
    let sql_owner_level = violated(
        &client,
        "INSERT INTO neon_roles (endpoint_id, name, scram_verifier, origin, api_owner_level)
         VALUES ($1, $2, 'x', 'sql', true)",
        &ep,
        "o4",
    )
    .await;
    cleanup(&client, id).await;
    assert_eq!(null_verifier, "neon_roles_owner_verifier");
    assert_eq!(sealed, "neon_roles_owner_unsealed");
    assert_eq!(second_owner, "neon_roles_one_owner");
    assert_eq!(sql_owner_level, "neon_roles_api_owner_level");
}

#[tokio::test]
#[ignore]
async fn sql_names_may_be_anything_postgres_allows() {
    let _sealed = sealed_lock().await;
    let client = client().await;
    let (id, ep) = ready_database(&client).await;
    let insert = "INSERT INTO neon_roles (endpoint_id, name, scram_verifier, origin) VALUES ($1, $2, 'x', 'sql')";
    let ok_names = ["My Role", "a\"b", "É", &"x".repeat(63)];
    for name in ok_names {
        sqlx::query(insert).bind(&ep).bind(name).execute(client.pool()).await.unwrap();
    }
    let too_long = violated(&client, insert, &ep, &"x".repeat(64)).await;
    let too_long_multibyte = violated(&client, insert, &ep, &"é".repeat(32)).await;
    let empty = violated(&client, insert, &ep, "").await;
    let stored = names_of(&client, &ep).await;
    cleanup(&client, id).await;
    for name in ok_names {
        assert!(stored.iter().any(|n| n == name), "{name} stored");
    }
    assert_eq!(too_long, "neon_roles_name_length");
    assert_eq!(too_long_multibyte, "neon_roles_name_length");
    assert_eq!(empty, "neon_roles_name_length");
}

#[tokio::test]
#[ignore]
async fn neon_roles_lists_only_the_owner() {
    let _sealed = sealed_lock().await;
    let client = client().await;
    let (id, ep) = ready_database(&client).await;
    seed_copy(&client, &ep, "from_sql", "sql", Some(SQL_VERIFIER), None).await;
    seed_copy(&client, &ep, "from_api", "api", Some(SQL_VERIFIER), Some("v1:x")).await;
    seed_copy(&client, &ep, "from_md5", "sql", None, None).await;
    let roles = client.neon_roles(&ep).await.unwrap();
    let (versioned, v) = client.neon_roles_versioned(&ep).await.unwrap();
    let unknown = client.neon_roles_versioned("ep-nothing").await.unwrap();
    cleanup(&client, id).await;
    let owner = cheladb_metadata::NeonRole { name: "postgres".into(), scram_verifier: "SCRAM-SHA-256$v1".into() };
    assert_eq!(roles, vec![owner.clone()]);
    assert_eq!(versioned, vec![owner]);
    assert_eq!(v, 0);
    assert_eq!(unknown, (vec![], 0));
}

#[tokio::test]
#[ignore]
async fn role_secret_of_an_md5_row_is_none() {
    let _sealed = sealed_lock().await;
    let client = client().await;
    let (id, ep) = ready_database(&client).await;
    seed_copy(&client, &ep, "legacy", "sql", None, None).await;
    seed_copy(&client, &ep, "modern", "sql", Some(SQL_VERIFIER), None).await;
    let md5 = client.neon_role_secret(&ep, "legacy").await.unwrap();
    let scram = client.neon_role_secret(&ep, "modern").await.unwrap();
    let missing = client.neon_role_secret(&ep, "nobody").await.unwrap();
    let unknown = client.neon_role_secret("ep-nothing", "legacy").await.unwrap();
    cleanup(&client, id).await;
    assert_eq!(md5, Some(None));
    assert_eq!(scram, Some(Some(SQL_VERIFIER.to_string())));
    assert_eq!(missing, Some(None));
    assert_eq!(unknown, None);
}

#[tokio::test]
#[ignore]
async fn list_roles_password_states() {
    let _sealed = sealed_lock().await;
    let client = client().await;
    let (id, ep) = ready_database(&client).await;
    assert_eq!(client.insert_api_role(&ep, "a_gen", false, "SCRAM-SHA-256$g", "v1:g", 10).await.unwrap(), InsertRoleOutcome::Inserted);
    seed_copy(&client, &ep, "b_sql", "sql", Some(SQL_VERIFIER), None).await;
    seed_copy(&client, &ep, "c_md5", "sql", None, None).await;
    let generated_owner = client.list_roles(&ep).await.unwrap();
    sqlx::query("UPDATE branches SET owner_password_enc = NULL WHERE id = $1")
        .bind(main_of(id).as_uuid())
        .execute(client.pool())
        .await
        .unwrap();
    let set_owner = client.list_roles(&ep).await.unwrap();
    let states = |rows: &[cheladb_metadata::RoleRow]| {
        rows.iter().map(|r| (r.name.clone(), r.origin, r.password)).collect::<Vec<_>>()
    };
    cleanup(&client, id).await;
    let tail = vec![
        ("a_gen".to_string(), RoleOrigin::Api, PasswordState::Generated),
        ("b_sql".to_string(), RoleOrigin::Sql, PasswordState::SetInSql),
        ("c_md5".to_string(), RoleOrigin::Sql, PasswordState::Md5Unusable),
    ];
    let with_owner = |state| [vec![("postgres".to_string(), RoleOrigin::Owner, state)], tail.clone()].concat();
    assert_eq!(states(&generated_owner), with_owner(PasswordState::Generated));
    assert_eq!(states(&set_owner), with_owner(PasswordState::SetInSql));
}

#[tokio::test]
#[ignore]
async fn list_roles_owner_first_then_c_collation() {
    let _sealed = sealed_lock().await;
    let client = client().await;
    let (id, ep) = ready_database(&client).await;
    for name in ["zeta", "alpha", "_first", "Beta"] {
        seed_copy(&client, &ep, name, "sql", Some(SQL_VERIFIER), None).await;
    }
    let names = names_of(&client, &ep).await;
    cleanup(&client, id).await;
    assert_eq!(names, ["postgres", "Beta", "_first", "alpha", "zeta"]);
}

#[tokio::test]
#[ignore]
async fn api_insert_respects_the_cap_under_concurrency() {
    let _sealed = sealed_lock().await;
    let client = client().await;
    // MD5 rows don't count.
    let (id, ep) = ready_database(&client).await;
    seed_copy(&client, &ep, "legacy", "sql", None, None).await;
    let mut tasks = Vec::new();
    for name in ["r1", "r2", "r3", "r4", "r5"] {
        let (c, ep) = (client.clone(), ep.clone());
        tasks.push(tokio::spawn(async move {
            c.insert_api_role(&ep, name, false, "SCRAM-SHA-256$r", "v1:r", 2).await.unwrap()
        }));
    }
    let mut outcomes = Vec::new();
    for t in tasks {
        outcomes.push(t.await.unwrap());
    }
    let count = |rows: &[cheladb_metadata::RoleRow], o| rows.iter().filter(|r| r.origin == o).count();
    let rows = client.list_roles(&ep).await.unwrap();
    cleanup(&client, id).await;
    assert_eq!(outcomes.iter().filter(|o| **o == InsertRoleOutcome::Inserted).count(), 2);
    assert_eq!(outcomes.iter().filter(|o| **o == InsertRoleOutcome::CapReached).count(), 3);
    assert_eq!(count(&rows, RoleOrigin::Api), 2);

    // SQL rows with a verifier do.
    let (id, ep) = ready_database(&client).await;
    seed_copy(&client, &ep, "from_sql", "sql", Some(SQL_VERIFIER), None).await;
    let first = client.insert_api_role(&ep, "a1", false, "SCRAM-SHA-256$a", "v1:a", 2).await.unwrap();
    let second = client.insert_api_role(&ep, "a2", false, "SCRAM-SHA-256$a", "v1:a", 2).await.unwrap();
    cleanup(&client, id).await;
    assert_eq!((first, second), (InsertRoleOutcome::Inserted, InsertRoleOutcome::CapReached));
}

#[tokio::test]
#[ignore]
async fn api_insert_sets_level_pending_and_versions() {
    let _sealed = sealed_lock().await;
    let client = client().await;
    let (id, ep) = ready_database(&client).await;
    let plain = client.insert_api_role(&ep, "app", false, "SCRAM-SHA-256$p", "v1:p", 10).await.unwrap();
    let admin = client.insert_api_role(&ep, "admin", true, "SCRAM-SHA-256$a", "v1:a", 10).await.unwrap();
    let again = client.insert_api_role(&ep, "app", true, "SCRAM-SHA-256$x", "v1:x", 10).await.unwrap();
    let owner = client.insert_api_role(&ep, "postgres", false, "SCRAM-SHA-256$x", "v1:x", 10).await.unwrap();
    let flag = flag_and_version(&client, &ep).await;
    let (copies, v) = client.role_copies(&ep).await.unwrap();
    let password = client.role_password(&ep, "app").await.unwrap();
    let owner_password = client.role_password(&ep, "postgres").await.unwrap();
    let nobody = client.role_password(&ep, "nobody").await.unwrap();
    cleanup(&client, id).await;
    assert_eq!((plain, admin), (InsertRoleOutcome::Inserted, InsertRoleOutcome::Inserted));
    assert_eq!((again, owner), (InsertRoleOutcome::Exists, InsertRoleOutcome::Exists));
    // The spec lists only the owner: an API role needs no compute reconfigure (M5).
    assert_eq!(flag, (false, 2));
    assert_eq!(v, 2);
    let by_name = |n: &str| copies.iter().find(|c| c.name == n).unwrap();
    assert_eq!(by_name("app").origin, RoleOrigin::Api);
    assert_eq!((by_name("app").level, by_name("app").api_owner_level), (RoleLevel::Plain, false));
    assert_eq!((by_name("admin").level, by_name("admin").api_owner_level), (RoleLevel::OwnerLevel, true));
    assert!(by_name("app").pending && by_name("app").login);
    assert_eq!(by_name("app").scram_verifier.as_deref(), Some("SCRAM-SHA-256$p"));
    assert_eq!(by_name("postgres").origin, RoleOrigin::Owner);
    assert_eq!(password.as_deref(), Some("v1:p"));
    assert_eq!(owner_password, None);
    assert_eq!(nobody, None);
}

#[tokio::test]
#[ignore]
async fn confirm_and_replace_secret_and_delete_copy() {
    let _sealed = sealed_lock().await;
    let client = client().await;
    let (id, ep) = ready_database(&client).await;
    client.insert_api_role(&ep, "app", false, "SCRAM-SHA-256$old", "v1:old", 10).await.unwrap();
    seed_copy(&client, &ep, "legacy", "sql", None, None).await;
    let (_, v) = client.role_copies(&ep).await.unwrap();

    // A stale version confirms nothing; the current one clears just that row.
    let stale = client.confirm_role(&ep, "app", v - 1).await.unwrap();
    let pending_after_stale = client.list_roles(&ep).await.unwrap().iter().filter(|r| r.pending).count();
    let confirmed = client.confirm_role(&ep, "app", v).await.unwrap();
    let pending_after = client.list_roles(&ep).await.unwrap().iter().filter(|r| r.pending).count();

    // Reset of an MD5 row gives it a verifier and a sealed password; the owner's row is left alone.
    let replaced = client.replace_role_secret(&ep, "legacy", "SCRAM-SHA-256$new", "v1:new").await.unwrap();
    let owner_replaced = client.replace_role_secret(&ep, "postgres", "SCRAM-SHA-256$x", "v1:x").await.unwrap();
    let missing = client.replace_role_secret(&ep, "nobody", "SCRAM-SHA-256$x", "v1:x").await.unwrap();
    let legacy = client.role_copies(&ep).await.unwrap().0.into_iter().find(|c| c.name == "legacy").unwrap();
    let after_replace = flag_and_version(&client, &ep).await;

    let owner_deleted = client.delete_role_copy(&ep, "postgres").await.unwrap();
    let deleted = client.delete_role_copy(&ep, "app").await.unwrap();
    let again = client.delete_role_copy(&ep, "app").await.unwrap();
    let after_delete = flag_and_version(&client, &ep).await;
    let names = names_of(&client, &ep).await;
    cleanup(&client, id).await;

    assert!(!stale);
    assert_eq!(pending_after_stale, 1);
    assert!(confirmed);
    assert_eq!(pending_after, 0);
    assert!(replaced && !owner_replaced && !missing);
    assert_eq!(legacy.scram_verifier.as_deref(), Some("SCRAM-SHA-256$new"));
    assert_eq!(legacy.password_enc.as_deref(), Some("v1:new"));
    assert!(legacy.pending);
    // Versions only: the spec lists only the owner (M5).
    assert_eq!(after_replace, (false, v + 1));
    assert!(!owner_deleted && deleted && !again);
    assert_eq!(after_delete, (false, v + 2));
    assert_eq!(names, ["postgres", "legacy"]);
}

#[tokio::test]
#[ignore]
async fn clear_needs_reconfigure_clears_only_the_owner_pending() {
    let _sealed = sealed_lock().await;
    let client = client().await;
    let (id, ep) = ready_database(&client).await;
    client.insert_api_role(&ep, "app", false, "SCRAM-SHA-256$a", "v1:a", 10).await.unwrap();
    assert!(client.replace_owner_secret(main_of(id), &ep, "postgres", "SCRAM-SHA-256$v2", "v1:new").await.unwrap());
    let v = version(&client, &ep).await;
    let cleared = client.clear_needs_reconfigure(&ep, v).await.unwrap();
    let rows = client.list_roles(&ep).await.unwrap();
    cleanup(&client, id).await;
    assert!(cleared);
    let pending: Vec<(&str, bool)> = rows.iter().map(|r| (r.name.as_str(), r.pending)).collect();
    assert_eq!(pending, [("postgres", false), ("app", true)]);
}

#[tokio::test]
#[ignore]
async fn resync_skips_recently_forwarded_rows() {
    let _sealed = sealed_lock().await;
    let client = client().await;
    let (id, ep) = ready_database(&client).await;
    let branch = main_of(id);
    for name in ["gone_recent", "gone_old", "up_recent", "up_old"] {
        seed_copy(&client, &ep, name, "sql", Some("SCRAM-SHA-256$before"), None).await;
    }
    set_forwarded(&client, &ep, "gone_recent", 1).await;
    set_forwarded(&client, &ep, "gone_old", 11).await;
    set_forwarded(&client, &ep, "up_recent", 1).await;
    set_forwarded(&client, &ep, "up_old", 11).await;
    let (_, v) = client.role_copies(&ep).await.unwrap();
    let write = ResyncWrite {
        upserts: vec![
            upsert("up_recent", RoleOrigin::Sql, "SCRAM-SHA-256$after"),
            upsert("up_old", RoleOrigin::Sql, "SCRAM-SHA-256$after"),
            upsert("fresh", RoleOrigin::Sql, "SCRAM-SHA-256$after"),
        ],
        removals: vec!["gone_recent".into(), "gone_old".into()],
        databases: vec![observed("db", "postgres", false)],
    };
    let applied = client.apply_resync(&ep, branch, v, &write).await.unwrap();
    let after = version(&client, &ep).await;
    let (copies, _) = client.role_copies(&ep).await.unwrap();
    // The same write again changes nothing, so the version holds.
    let (_, v2) = client.role_copies(&ep).await.unwrap();
    let again = client.apply_resync(&ep, branch, v2, &ResyncWrite {
        upserts: vec![upsert("fresh", RoleOrigin::Sql, "SCRAM-SHA-256$after")],
        removals: vec!["gone_old".into()],
        databases: vec![observed("db", "postgres", false)],
    }).await.unwrap();
    let after_again = version(&client, &ep).await;
    cleanup(&client, id).await;

    let ResyncApplied::Applied { mut changed } = applied else { panic!("{applied:?}") };
    changed.sort();
    assert_eq!(changed, ["fresh", "gone_old", "up_old"]);
    assert_eq!(after, v + 1);
    let verifier = |n: &str| copies.iter().find(|c| c.name == n).map(|c| c.scram_verifier.clone());
    assert_eq!(verifier("up_recent"), Some(Some("SCRAM-SHA-256$before".to_string())));
    assert_eq!(verifier("up_old"), Some(Some("SCRAM-SHA-256$after".to_string())));
    assert!(verifier("gone_recent").is_some());
    assert_eq!(verifier("gone_old"), None);
    assert_eq!(again, ResyncApplied::Applied { changed: vec![] });
    assert_eq!(after_again, after);
}

#[tokio::test]
#[ignore]
async fn resync_upsert_keeps_the_origin_and_applies_the_sealed_change() {
    let _sealed = sealed_lock().await;
    let client = client().await;
    let (id, ep) = ready_database(&client).await;
    let branch = main_of(id);
    client.insert_api_role(&ep, "app", true, "SCRAM-SHA-256$old", "v1:old", 10).await.unwrap();
    let (_, v) = client.role_copies(&ep).await.unwrap();
    let mut changed_row = upsert("app", RoleOrigin::Sql, "SCRAM-SHA-256$new");
    changed_row.password_enc = SealedChange::Clear;
    changed_row.login = false;
    changed_row.pending = false;
    let mut md5 = upsert("legacy", RoleOrigin::Sql, "x");
    md5.verifier = None;
    md5.password_enc = SealedChange::Set("v1:set".into());
    let write = ResyncWrite {
        upserts: vec![changed_row, md5, upsert("postgres", RoleOrigin::Sql, "SCRAM-SHA-256$hijack")],
        removals: vec![],
        databases: vec![observed("db", "postgres", false)],
    };
    client.apply_resync(&ep, branch, v, &write).await.unwrap();
    let (copies, _) = client.role_copies(&ep).await.unwrap();
    cleanup(&client, id).await;
    let by_name = |n: &str| copies.iter().find(|c| c.name == n).unwrap();
    let app = by_name("app");
    assert_eq!(app.origin, RoleOrigin::Api, "an existing row keeps its origin");
    assert_eq!(app.scram_verifier.as_deref(), Some("SCRAM-SHA-256$new"));
    assert_eq!(app.password_enc, None);
    assert!(!app.login && !app.pending);
    assert_eq!(app.level, RoleLevel::Plain);
    assert!(app.api_owner_level, "api_owner_level is not the resync's to change");
    let legacy = by_name("legacy");
    assert_eq!((legacy.origin, legacy.scram_verifier.clone()), (RoleOrigin::Sql, None));
    assert_eq!(legacy.password_enc.as_deref(), Some("v1:set"));
    let owner = by_name("postgres");
    assert_eq!((owner.origin, owner.scram_verifier.as_deref()), (RoleOrigin::Owner, Some("SCRAM-SHA-256$v1")));
}

#[tokio::test]
#[ignore]
async fn resync_version_guard() {
    let _sealed = sealed_lock().await;
    let client = client().await;
    let (id, ep) = ready_database(&client).await;
    let branch = main_of(id);
    let (_, read) = client.role_copies(&ep).await.unwrap();
    // The receiver commits between the read and the write.
    let mut tx = client.begin_ddl(&ep).await.unwrap().expect("endpoint");
    tx.upsert_sql_copy("from_ddl", SQL_VERIFIER).await.unwrap();
    tx.commit().await.unwrap();
    let write = ResyncWrite {
        upserts: vec![upsert("from_resync", RoleOrigin::Sql, "SCRAM-SHA-256$r")],
        removals: vec!["from_ddl".into()],
        databases: vec![observed("elsewhere", "postgres", false)],
    };
    let outcome = client.apply_resync(&ep, branch, read, &write).await.unwrap();
    let names = names_of(&client, &ep).await;
    let dbs = databases_of(&client, branch).await;
    cleanup(&client, id).await;
    assert_eq!(outcome, ResyncApplied::VersionMoved);
    assert_eq!(names, ["postgres", "from_ddl"]);
    assert_eq!(dbs, vec![("db".to_string(), "postgres".to_string(), DatabaseOrigin::Main)]);
}

#[tokio::test]
#[ignore]
async fn resync_sets_databases_with_origins() {
    let _sealed = sealed_lock().await;
    let client = client().await;
    let (id, ep) = ready_database(&client).await;
    let branch = main_of(id);
    let initial = databases_of(&client, branch).await;
    let sync = |dbs: Vec<ObservedDatabase>| {
        let client = client.clone();
        let ep = ep.clone();
        async move {
            let (_, v) = client.role_copies(&ep).await.unwrap();
            client
                .apply_resync(&ep, branch, v, &ResyncWrite { upserts: vec![], removals: vec![], databases: dbs })
                .await
                .unwrap();
            databases_of(&client, branch).await
        }
    };
    let s = |n: &str, o: &str, origin| (n.to_string(), o.to_string(), origin);
    let full = sync(vec![
        observed("postgres", "cloud_admin", true),
        observed("db", "postgres", false),
        observed("extra", "app", false),
    ])
    .await;
    // Q1: the main database was dropped; its row goes.
    let dropped = sync(vec![observed("postgres", "cloud_admin", true), observed("extra", "app", false)]).await;
    let (_, ver_dropped) = client.role_copies(&ep).await.unwrap();
    // It comes back as `main`.
    let back = sync(vec![observed("postgres", "cloud_admin", true), observed("db", "postgres", false)]).await;
    let (_, ver_back) = client.role_copies(&ep).await.unwrap();
    cleanup(&client, id).await;

    assert_eq!(initial, vec![s("db", "postgres", DatabaseOrigin::Main)]);
    assert_eq!(
        full,
        vec![
            s("db", "postgres", DatabaseOrigin::Main),
            s("extra", "app", DatabaseOrigin::Sql),
            s("postgres", "cloud_admin", DatabaseOrigin::System),
        ]
    );
    assert_eq!(
        dropped,
        vec![s("extra", "app", DatabaseOrigin::Sql), s("postgres", "cloud_admin", DatabaseOrigin::System)]
    );
    assert_eq!(
        back,
        vec![s("db", "postgres", DatabaseOrigin::Main), s("postgres", "cloud_admin", DatabaseOrigin::System)]
    );
    assert!(ver_back > ver_dropped);
}

#[tokio::test]
#[ignore]
async fn database_count_ignores_system() {
    let _sealed = sealed_lock().await;
    let client = client().await;
    let (id, ep) = ready_database(&client).await;
    let branch = main_of(id);
    let (_, v) = client.role_copies(&ep).await.unwrap();
    let write = ResyncWrite {
        upserts: vec![],
        removals: vec![],
        databases: vec![
            observed("postgres", "cloud_admin", true),
            observed("db", "postgres", false),
            observed("extra", "app", false),
        ],
    };
    client.apply_resync(&ep, branch, v, &write).await.unwrap();
    let mut tx = client.begin_ddl(&ep).await.unwrap().expect("endpoint");
    let count = tx.database_count().await.unwrap();
    drop(tx);
    cleanup(&client, id).await;
    assert_eq!(count, 2);
}

#[tokio::test]
#[ignore]
async fn ddl_begin_reads_the_endpoint_branch_and_owner() {
    let _sealed = sealed_lock().await;
    let client = client().await;
    let (id, ep) = ready_database(&client).await;
    let tx = client.begin_ddl(&ep).await.unwrap().expect("endpoint");
    let seen = (tx.branch_id(), tx.owner().to_string(), tx.main_database().map(str::to_string));
    drop(tx);
    let unknown = client.begin_ddl("ep-nothing").await.unwrap().is_none();
    cleanup(&client, id).await;
    assert_eq!(seen, (Some(main_of(id)), "postgres".to_string(), Some("db".to_string())));
    assert!(unknown);
}

#[tokio::test]
#[ignore]
async fn ddl_copy_count_and_upsert_follow_r5() {
    let _sealed = sealed_lock().await;
    let client = client().await;
    let (id, ep) = ready_database(&client).await;
    seed_copy(&client, &ep, "legacy", "sql", None, None).await;
    client.insert_api_role(&ep, "api1", false, "SCRAM-SHA-256$a", "v1:a", 10).await.unwrap();
    seed_copy(&client, &ep, "from_sql", "sql", Some("SCRAM-SHA-256$old"), None).await;
    let (_, v) = client.role_copies(&ep).await.unwrap();

    let mut tx = client.begin_ddl(&ep).await.unwrap().expect("endpoint");
    let count = tx.copy_count().await.unwrap();
    tx.upsert_sql_copy("fresh", "SCRAM-SHA-256$f").await.unwrap();
    tx.upsert_sql_copy("api1", "SCRAM-SHA-256$typed").await.unwrap();
    tx.upsert_sql_copy("from_sql", "SCRAM-SHA-256$new").await.unwrap();
    let count_after = tx.copy_count().await.unwrap();
    let seen = tx.role("api1").await.unwrap().expect("row");
    let owner_row = tx.role("postgres").await.unwrap().expect("owner");
    let none = tx.role("nobody").await.unwrap();
    tx.commit().await.unwrap();

    let (copies, v_after) = client.role_copies(&ep).await.unwrap();
    // A transaction that wrote nothing leaves the version alone.
    let quiet = client.begin_ddl(&ep).await.unwrap().expect("endpoint");
    quiet.commit().await.unwrap();
    let v_quiet = version(&client, &ep).await;
    let forwarded: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("SELECT forwarded_at FROM neon_roles WHERE endpoint_id = $1 AND name = 'fresh'")
            .bind(&ep)
            .fetch_one(client.pool())
            .await
            .unwrap();
    cleanup(&client, id).await;

    assert_eq!(count, 2, "api1 and from_sql count; the MD5 row doesn't");
    assert_eq!(count_after, 3);
    assert_eq!(seen.origin, RoleOrigin::Api);
    assert_eq!(owner_row.origin, RoleOrigin::Owner);
    assert!(none.is_none());
    let by_name = |n: &str| copies.iter().find(|c| c.name == n).unwrap();
    let fresh = by_name("fresh");
    assert_eq!((fresh.origin, fresh.login, fresh.level, fresh.password_enc.clone()), (RoleOrigin::Sql, true, RoleLevel::Plain, None));
    assert_eq!(by_name("api1").password_enc, None, "a password set in SQL unseals the copy");
    assert_eq!(by_name("api1").origin, RoleOrigin::Api);
    assert_eq!(by_name("from_sql").scram_verifier.as_deref(), Some("SCRAM-SHA-256$new"));
    assert_eq!(v_after, v + 1);
    assert_eq!(v_quiet, v_after);
    assert!(forwarded.is_some());
}

#[tokio::test]
#[ignore]
async fn ddl_main_database_drop_and_recreate() {
    let _sealed = sealed_lock().await;
    let client = client().await;
    let (id, ep) = ready_database(&client).await;
    let branch = main_of(id);
    let mut tx = client.begin_ddl(&ep).await.unwrap().expect("endpoint");
    let removed = tx.remove_database("db").await.unwrap();
    let removed_again = tx.remove_database("db").await.unwrap();
    tx.commit().await.unwrap();
    let after_drop = databases_of(&client, branch).await;

    let mut tx = client.begin_ddl(&ep).await.unwrap().expect("endpoint");
    tx.upsert_database("db", "postgres").await.unwrap();
    tx.upsert_database("other", "app").await.unwrap();
    tx.commit().await.unwrap();
    let after_create = databases_of(&client, branch).await;
    cleanup(&client, id).await;
    assert!(removed && !removed_again);
    assert!(after_drop.is_empty());
    assert_eq!(
        after_create,
        vec![
            ("db".to_string(), "postgres".to_string(), DatabaseOrigin::Main),
            ("other".to_string(), "app".to_string(), DatabaseOrigin::Sql),
        ]
    );
}

#[tokio::test]
#[ignore]
async fn ddl_rename_database_moves_the_row() {
    let _sealed = sealed_lock().await;
    let client = client().await;
    let (id, ep) = ready_database(&client).await;
    let branch = main_of(id);
    let mut tx = client.begin_ddl(&ep).await.unwrap().expect("endpoint");
    tx.upsert_database("one", "app").await.unwrap();
    let renamed = tx.rename_database("one", "two").await.unwrap();
    let missing = tx.rename_database("nobody", "three").await.unwrap();
    tx.commit().await.unwrap();
    let dbs = databases_of(&client, branch).await;
    cleanup(&client, id).await;
    assert!(renamed && !missing);
    assert_eq!(dbs[1], ("two".to_string(), "app".to_string(), DatabaseOrigin::Sql));
    assert_eq!(dbs.len(), 2);
}

#[tokio::test]
#[ignore]
async fn ddl_has_database_sees_the_open_transaction() {
    let _sealed = sealed_lock().await;
    let client = client().await;
    let (id, ep) = ready_database(&client).await;
    let mut tx = client.begin_ddl(&ep).await.unwrap().expect("endpoint");
    let main = tx.main_database().map(str::to_string).expect("main database");
    let has_main = tx.has_database(&main).await.unwrap();
    let before = tx.has_database("extra").await.unwrap();
    tx.upsert_database("extra", "app").await.unwrap();
    let after = tx.has_database("extra").await.unwrap();
    drop(tx);
    cleanup(&client, id).await;
    assert!(has_main && !before && after);
}

#[tokio::test]
#[ignore]
async fn ddl_rename_moves_copy_and_keys() {
    let _sealed = sealed_lock().await;
    let client = client().await;
    let (id, ep) = ready_database(&client).await;
    let branch = main_of(id);
    client.insert_api_role(&ep, "old", true, "SCRAM-SHA-256$o", "v1:old", 10).await.unwrap();
    let (live, _) = new_key(branch, DataKeyKind::Secret, "old", "live");
    let (revoked, _) = new_key(branch, DataKeyKind::Secret, "old", "revoked");
    let (other, _) = new_key(branch, DataKeyKind::Secret, "service_role", "other");
    for key in [&live, &revoked, &other] {
        client.insert_data_key(key).await.unwrap();
    }
    sqlx::query("UPDATE data_api_keys SET revoked_at = now() WHERE id = $1")
        .bind(revoked.id)
        .execute(client.pool())
        .await
        .unwrap();
    let v = version(&client, &ep).await;

    let mut tx = client.begin_ddl(&ep).await.unwrap().expect("endpoint");
    let live_before = tx.live_secret_keys("old").await.unwrap();
    let renamed = tx.rename_copy("old", "new", Some("v1:resealed")).await.unwrap();
    let missing = tx.rename_copy("nobody", "x", None).await.unwrap();
    let live_after = tx.live_secret_keys("new").await.unwrap();
    let live_old = tx.live_secret_keys("old").await.unwrap();
    tx.commit().await.unwrap();

    let (copies, v_after) = client.role_copies(&ep).await.unwrap();
    let roles: Vec<(String, String)> =
        sqlx::query_as("SELECT name, role FROM data_api_keys WHERE branch_id = $1 ORDER BY name")
            .bind(branch.as_uuid())
            .fetch_all(client.pool())
            .await
            .unwrap();
    // A rename with nothing to reseal leaves the copy unsealed.
    let mut tx = client.begin_ddl(&ep).await.unwrap().expect("endpoint");
    tx.rename_copy("new", "newer", None).await.unwrap();
    tx.commit().await.unwrap();
    let (copies_unsealed, _) = client.role_copies(&ep).await.unwrap();
    cleanup(&client, id).await;

    assert!(renamed && !missing);
    assert_eq!(live_before, vec![(live.id, "live".to_string())]);
    assert_eq!(live_after, live_before);
    assert!(live_old.is_empty());
    assert_eq!(v_after, v + 1);
    assert!(copies.iter().all(|c| c.name != "old"));
    let new = copies.iter().find(|c| c.name == "new").expect("renamed");
    assert_eq!((new.origin, new.level, new.api_owner_level), (RoleOrigin::Api, RoleLevel::OwnerLevel, true));
    assert_eq!(new.password_enc.as_deref(), Some("v1:resealed"));
    assert_eq!(
        roles,
        vec![
            ("live".to_string(), "new".to_string()),
            ("other".to_string(), "service_role".to_string()),
            ("revoked".to_string(), "new".to_string()),
        ]
    );
    assert_eq!(copies_unsealed.iter().find(|c| c.name == "newer").unwrap().password_enc, None);
}

#[tokio::test]
#[ignore]
async fn ddl_remove_copy_and_commit_versions() {
    let _sealed = sealed_lock().await;
    let client = client().await;
    let (id, ep) = ready_database(&client).await;
    seed_copy(&client, &ep, "tmp", "sql", Some(SQL_VERIFIER), None).await;
    let v = version(&client, &ep).await;
    let mut tx = client.begin_ddl(&ep).await.unwrap().expect("endpoint");
    let owner = tx.remove_copy("postgres").await.unwrap();
    let removed = tx.remove_copy("tmp").await.unwrap();
    let again = tx.remove_copy("tmp").await.unwrap();
    tx.commit().await.unwrap();
    let (flag, after) = flag_and_version(&client, &ep).await;
    let names = names_of(&client, &ep).await;
    cleanup(&client, id).await;
    assert!(!owner && removed && !again);
    assert_eq!(after, v + 1);
    assert!(!flag, "the compute already has what the DDL did");
    assert_eq!(names, ["postgres"]);
}

#[tokio::test]
#[ignore]
async fn ddl_dropped_transaction_writes_nothing() {
    let _sealed = sealed_lock().await;
    let client = client().await;
    let (id, ep) = ready_database(&client).await;
    let v = version(&client, &ep).await;
    let mut tx = client.begin_ddl(&ep).await.unwrap().expect("endpoint");
    tx.upsert_sql_copy("ghost", SQL_VERIFIER).await.unwrap();
    drop(tx);
    let names = names_of(&client, &ep).await;
    let after = version(&client, &ep).await;
    cleanup(&client, id).await;
    assert_eq!(names, ["postgres"]);
    assert_eq!(after, v);
}

#[tokio::test]
#[ignore]
async fn ddl_owner_verifier_clears_the_sealed_owner_password() {
    let _sealed = sealed_lock().await;
    let client = client().await;
    let (id, ep) = ready_database(&client).await;
    let sealed_before = main_branch(&client, id).await.owner_password_enc;
    let v = version(&client, &ep).await;
    let mut tx = client.begin_ddl(&ep).await.unwrap().expect("endpoint");
    tx.set_owner_verifier("SCRAM-SHA-256$typed").await.unwrap();
    tx.commit().await.unwrap();
    let sealed_after = main_branch(&client, id).await.owner_password_enc;
    let owner = client.neon_roles(&ep).await.unwrap();
    let forwarded: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("SELECT forwarded_at FROM neon_roles WHERE endpoint_id = $1 AND origin = 'owner'")
            .bind(&ep)
            .fetch_one(client.pool())
            .await
            .unwrap();
    let state = client.list_roles(&ep).await.unwrap()[0].password;
    let after = version(&client, &ep).await;
    cleanup(&client, id).await;
    assert_eq!(sealed_before.as_deref(), Some("v1:owner"));
    assert_eq!(sealed_after, None);
    assert_eq!(owner[0].scram_verifier, "SCRAM-SHA-256$typed");
    assert!(forwarded.is_some());
    assert_eq!(state, PasswordState::SetInSql);
    assert_eq!(after, v + 1);
}

#[tokio::test]
#[ignore]
async fn ddl_lock_times_out() {
    let _sealed = sealed_lock().await;
    let client = client().await;
    let (id, ep) = ready_database(&client).await;
    let mut holder = client.pool().begin().await.unwrap();
    sqlx::query("SELECT id FROM neon_endpoints WHERE id = $1 FOR UPDATE")
        .bind(&ep)
        .execute(&mut *holder)
        .await
        .unwrap();
    let release = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        holder.commit().await.unwrap();
    });
    let started = std::time::Instant::now();
    let outcome = client.begin_ddl(&ep).await;
    let waited = started.elapsed();
    let timed_out = matches!(&outcome, Err(MetadataError::EndpointLockTimeout(e)) if *e == ep);
    drop(outcome);
    release.await.unwrap();
    // Once released, it works.
    let free = client.begin_ddl(&ep).await.unwrap().is_some();
    cleanup(&client, id).await;
    assert!(timed_out);
    assert!(waited >= std::time::Duration::from_millis(1800) && waited < std::time::Duration::from_millis(2800), "{waited:?}");
    assert!(free);
}

#[tokio::test]
#[ignore]
async fn branch_copy_writes_role_copies_and_main_database() {
    let client = client().await;
    let id = insert_pending(&client).await;
    let ep = ep_id();
    let (tenant, timeline) = (hex32(), hex32());
    let copies = [
        NewRoleCopy { name: "reader", origin: RoleOrigin::Api, verifier: Some("SCRAM-SHA-256$r"), password_enc: Some("v1:reader"), login: true, level: RoleLevel::Plain, api_owner_level: false, pending: true },
        NewRoleCopy { name: "writer", origin: RoleOrigin::Api, verifier: Some("SCRAM-SHA-256$w"), password_enc: Some("v1:writer"), login: true, level: RoleLevel::OwnerLevel, api_owner_level: true, pending: true },
        NewRoleCopy { name: "My Role", origin: RoleOrigin::Sql, verifier: Some("SCRAM-SHA-256$s"), password_enc: None, login: false, level: RoleLevel::OwnerLevel, api_owner_level: false, pending: false },
        NewRoleCopy { name: "legacy", origin: RoleOrigin::Sql, verifier: None, password_enc: None, login: true, level: RoleLevel::Plain, api_owner_level: false, pending: false },
    ];
    let mut new = new_endpoint(&ep, &tenant, &timeline);
    new.role_copies = &copies;
    assert!(client.create_endpoint_for_branch(main_of(id), &timeline, &new, false).await.unwrap());
    let rows = client.list_roles(&ep).await.unwrap();
    let (all, _) = client.role_copies(&ep).await.unwrap();
    let dbs = databases_of(&client, main_of(id)).await;
    let reader = client.role_password(&ep, "reader").await.unwrap();
    let owner_pw = client.role_password(&ep, "postgres").await.unwrap();
    let spec_roles = client.neon_roles(&ep).await.unwrap();
    cleanup(&client, id).await;
    assert_eq!(
        rows.iter().map(|r| (r.name.as_str(), r.origin, r.level, r.login, r.pending)).collect::<Vec<_>>(),
        vec![
            ("postgres", RoleOrigin::Owner, RoleLevel::OwnerLevel, true, false),
            ("My Role", RoleOrigin::Sql, RoleLevel::OwnerLevel, false, false),
            ("legacy", RoleOrigin::Sql, RoleLevel::Plain, true, false),
            ("reader", RoleOrigin::Api, RoleLevel::Plain, true, true),
            ("writer", RoleOrigin::Api, RoleLevel::OwnerLevel, true, true),
        ]
    );
    assert!(all.iter().find(|c| c.name == "writer").unwrap().api_owner_level);
    assert_eq!(all.iter().find(|c| c.name == "legacy").unwrap().scram_verifier, None);
    assert_eq!(dbs, vec![("db".to_string(), "postgres".to_string(), DatabaseOrigin::Main)]);
    assert_eq!(reader.as_deref(), Some("v1:reader"));
    assert_eq!(owner_pw, None);
    assert_eq!(spec_roles.len(), 1);
}

#[tokio::test]
#[ignore]
async fn claim_refuses_a_protected_branch_unless_allowed() {
    let _sealed = sealed_lock().await;
    let client = client().await;
    let (id, _ep) = ready_database(&client).await;
    let dev = ready_branch(&client, id, "dev", main_of(id)).await;
    let marked = client.set_branch_protected(dev, true).await.unwrap().expect("a normal branch");
    let refused = client.claim_branch_for_switch(dev, false).await.unwrap();
    let still_ready = branch(&client, dev).await.status;
    let allowed = client.claim_branch_for_switch(dev, true).await.unwrap();
    let again = client.claim_branch_for_switch(dev, true).await.unwrap();
    let unmarked = client.set_branch_protected(dev, false).await.unwrap().expect("a normal branch");
    let unknown = client.set_branch_protected(BranchId::new(), true).await.unwrap();
    // Not once a delete has begun.
    let mut other = ready_branch(&client, id, "gone", main_of(id)).await;
    client.begin_branch_delete(other).await.unwrap();
    let deleting = client.set_branch_protected(other, true).await.unwrap();
    other = main_of(id);
    let main_marked = client.set_branch_protected(other, true).await.unwrap().map(|b| b.protected);
    cleanup(&client, id).await;
    assert!(marked.protected);
    assert_eq!(refused, ClaimOutcome::Protected);
    assert_eq!(still_ready, BranchStatus::Ready);
    assert_eq!(allowed, ClaimOutcome::Claimed);
    assert_eq!(again, ClaimOutcome::NotReady, "protected or not, only a ready branch is claimed");
    assert!(!unmarked.protected);
    assert!(unknown.is_none() && deleting.is_none());
    assert_eq!(main_marked, Some(true));
}

#[tokio::test]
#[ignore]
async fn begin_delete_refuses_with_a_protected_branch() {
    let _sealed = sealed_lock().await;
    let client = client().await;
    let (id, _ep) = ready_database(&client).await;
    let dev = ready_branch(&client, id, "dev", main_of(id)).await;
    client.set_branch_protected(dev, true).await.unwrap().unwrap();
    let refused = client.begin_delete(id).await.unwrap();
    let db_status = client.get_database(id).await.unwrap().unwrap().status;
    let branch_status = branch(&client, dev).await.status;
    client.set_branch_protected(dev, false).await.unwrap().unwrap();
    let started = client.begin_delete(id).await.unwrap();
    let again = client.begin_delete(id).await.unwrap();
    cleanup(&client, id).await;
    assert_eq!(refused, BeginDelete::Protected);
    assert_eq!(db_status, DatabaseStatus::Ready);
    assert_eq!(branch_status, BranchStatus::Ready);
    assert_eq!(started, BeginDelete::Started);
    assert_eq!(again, BeginDelete::NotReady);
}

// ---- login-roles follow-ups (Task 1) ----

async fn dbs_stamp_age_secs(client: &MetadataClient, ep: &str) -> Option<f64> {
    sqlx::query_scalar(
        "SELECT extract(epoch FROM now() - dbs_forwarded_at)::float8 FROM neon_endpoints WHERE id = $1",
    )
    .bind(ep)
    .fetch_one(client.pool())
    .await
    .unwrap()
}

async fn set_dbs_stamp(client: &MetadataClient, ep: &str, secs_ago: Option<i32>) {
    sqlx::query(
        "UPDATE neon_endpoints SET dbs_forwarded_at = CASE WHEN $2::float8 IS NULL THEN NULL
                                    ELSE now() - make_interval(secs => $2::float8) END WHERE id = $1",
    )
    .bind(ep)
    .bind(secs_ago.map(f64::from))
    .execute(client.pool())
    .await
    .unwrap();
}

#[tokio::test]
#[ignore]
async fn database_ddl_stamps_the_endpoint() {
    let _sealed = sealed_lock().await;
    let client = client().await;
    let (id, ep) = ready_database(&client).await;
    let fresh = dbs_stamp_age_secs(&client, &ep).await;
    let mut seen = Vec::new();
    for step in 0..3 {
        set_dbs_stamp(&client, &ep, None).await;
        let mut tx = client.begin_ddl(&ep).await.unwrap().expect("endpoint");
        match step {
            0 => tx.upsert_database("extra", "postgres").await.unwrap(),
            1 => assert!(tx.rename_database("extra", "extra2").await.unwrap()),
            _ => assert!(tx.remove_database("extra2").await.unwrap()),
        }
        tx.commit().await.unwrap();
        seen.push(dbs_stamp_age_secs(&client, &ep).await);
    }
    cleanup(&client, id).await;
    assert!(fresh.is_none());
    for age in seen {
        assert!(age.is_some_and(|a| (0.0..5.0).contains(&a)), "{age:?}");
    }
}

#[tokio::test]
#[ignore]
async fn resync_skips_databases_after_a_recent_ddl() {
    let _sealed = sealed_lock().await;
    let client = client().await;
    let (id, ep) = ready_database(&client).await;
    let branch = main_of(id);
    let write = ResyncWrite {
        upserts: vec![],
        removals: vec![],
        databases: vec![observed("db", "postgres", false), observed("kept", "postgres", false)],
    };
    set_dbs_stamp(&client, &ep, Some(1)).await;
    let (_, v) = client.role_copies(&ep).await.unwrap();
    client.apply_resync(&ep, branch, v, &write).await.unwrap();
    let skipped = databases_of(&client, branch).await.len();
    set_dbs_stamp(&client, &ep, Some(11)).await;
    let (_, v) = client.role_copies(&ep).await.unwrap();
    client.apply_resync(&ep, branch, v, &write).await.unwrap();
    let applied = databases_of(&client, branch).await.len();
    cleanup(&client, id).await;
    assert_eq!(skipped, 1, "a stamp under 10 s old leaves branch_databases alone");
    assert_eq!(applied, 2);
}

#[tokio::test]
#[ignore]
async fn clear_forwarded_lifts_both_guards() {
    let _sealed = sealed_lock().await;
    let client = client().await;
    let (id, ep) = ready_database(&client).await;
    seed_copy(&client, &ep, "app", "sql", Some("SCRAM-SHA-256$a"), None).await;
    set_forwarded(&client, &ep, "app", 1).await;
    set_dbs_stamp(&client, &ep, Some(1)).await;
    client.clear_forwarded(&ep).await.unwrap();
    let roles_stamp: Option<f64> = sqlx::query_scalar(
        "SELECT extract(epoch FROM forwarded_at)::float8 FROM neon_roles WHERE endpoint_id = $1 AND name = 'app'",
    )
    .bind(&ep)
    .fetch_one(client.pool())
    .await
    .unwrap();
    let dbs_stamp = dbs_stamp_age_secs(&client, &ep).await;
    cleanup(&client, id).await;
    assert!(roles_stamp.is_none() && dbs_stamp.is_none());
}

#[tokio::test]
#[ignore]
async fn unreadable_password_state() {
    let _sealed = sealed_lock().await;
    let client = client().await;
    let (id, ep) = ready_database(&client).await;
    seed_copy(&client, &ep, "app", "sql", Some("SCRAM-SHA-256$a"), Some("v1:a")).await;
    let before = password_of(&client, &ep, "app").await;
    client.mark_password_unreadable(&ep, "app").await.unwrap();
    let marked = password_of(&client, &ep, "app").await;
    assert!(client.replace_role_secret(&ep, "app", "SCRAM-SHA-256$b", "v1:b").await.unwrap());
    let resealed = password_of(&client, &ep, "app").await;
    // The receiver's path: mark inside the DDL transaction, then a new verifier clears it.
    let mut tx = client.begin_ddl(&ep).await.unwrap().expect("endpoint");
    tx.mark_password_unreadable("app").await.unwrap();
    tx.commit().await.unwrap();
    let marked_in_tx = password_of(&client, &ep, "app").await;
    let mut tx = client.begin_ddl(&ep).await.unwrap().expect("endpoint");
    tx.upsert_sql_copy("app", "SCRAM-SHA-256$c").await.unwrap();
    tx.commit().await.unwrap();
    let after_sql = password_of(&client, &ep, "app").await;
    cleanup(&client, id).await;
    assert_eq!(before, PasswordState::Generated);
    assert_eq!(marked, PasswordState::Unreadable);
    assert_eq!(resealed, PasswordState::Generated);
    assert_eq!(marked_in_tx, PasswordState::Unreadable);
    assert_eq!(after_sql, PasswordState::SetInSql);
}

async fn password_of(client: &MetadataClient, ep: &str, name: &str) -> PasswordState {
    client.list_roles(ep).await.unwrap().into_iter().find(|r| r.name == name).unwrap().password
}

#[tokio::test]
#[ignore]
async fn endpoint_insert_refuses_a_changed_parent_protection() {
    let _sealed = sealed_lock().await;
    let client = client().await;
    let (id, _ep) = ready_database(&client).await;
    let parent = main_of(id);
    let b = new_branch(id, "child", parent);
    assert_eq!(client.create_branch(&b).await.unwrap(), CreateBranchOutcome::Created);
    assert!(client.set_branch_protected(parent, true).await.unwrap().is_some());
    let (ep, tenant, timeline) = (ep_id(), hex32(), hex32());
    let stale = client
        .create_endpoint_for_branch(b.id, &timeline, &new_endpoint(&ep, &tenant, &timeline), false)
        .await;
    let linked_after_stale = branch(&client, b.id).await.endpoint_id.is_some();
    let current = client
        .create_endpoint_for_branch(b.id, &timeline, &new_endpoint(&ep, &tenant, &timeline), true)
        .await;
    cleanup(&client, id).await;
    assert!(matches!(stale, Err(MetadataError::ParentProtectionChanged)), "{stale:?}");
    assert!(!linked_after_stale);
    assert!(matches!(current, Ok(true)), "{current:?}");
}

#[tokio::test]
#[ignore]
async fn branches_with_main_database_in_one_query() {
    let _sealed = sealed_lock().await;
    let client = client().await;
    let (id, _ep) = ready_database(&client).await;
    let with = ready_branch(&client, id, "with", main_of(id)).await;
    let without = ready_branch(&client, id, "without", main_of(id)).await;
    sqlx::query("DELETE FROM branch_databases WHERE branch_id = $1")
        .bind(without.as_uuid())
        .execute(client.pool())
        .await
        .unwrap();
    let found = client
        .branches_with_main_database(&[with, without, BranchId::new()])
        .await
        .unwrap();
    let none = client.branches_with_main_database(&[]).await.unwrap();
    cleanup(&client, id).await;
    assert!(found.contains(&with) && !found.contains(&without) && found.len() == 1, "{found:?}");
    assert!(none.is_empty());
}

#[tokio::test]
#[ignore]
async fn resync_password_change_clears_the_unreadable_mark() {
    let _sealed = sealed_lock().await;
    let client = client().await;
    let (id, ep) = ready_database(&client).await;
    seed_copy(&client, &ep, "app", "sql", Some("SCRAM-SHA-256$a"), Some("v1:a")).await;
    client.mark_password_unreadable(&ep, "app").await.unwrap();
    let (_, v) = client.role_copies(&ep).await.unwrap();
    // The same verifier with the sealed value kept: still unreadable.
    let same = ResyncWrite {
        upserts: vec![upsert("app", RoleOrigin::Sql, "SCRAM-SHA-256$a")],
        removals: vec![],
        databases: vec![observed("db", "postgres", false)],
    };
    client.apply_resync(&ep, main_of(id), v, &same).await.unwrap();
    let kept = password_of(&client, &ep, "app").await;
    let (_, v) = client.role_copies(&ep).await.unwrap();
    let changed = ResyncWrite {
        upserts: vec![upsert("app", RoleOrigin::Sql, "SCRAM-SHA-256$b")],
        removals: vec![],
        databases: vec![observed("db", "postgres", false)],
    };
    client.apply_resync(&ep, main_of(id), v, &changed).await.unwrap();
    let after = password_of(&client, &ep, "app").await;
    cleanup(&client, id).await;
    assert_eq!(kept, PasswordState::Unreadable);
    assert_eq!(after, PasswordState::SetInSql);
}

#[tokio::test]
#[ignore]
async fn rename_copy_keeps_the_unreadable_mark_when_nothing_was_resealed() {
    let _sealed = sealed_lock().await;
    let client = client().await;
    let (id, ep) = ready_database(&client).await;
    seed_copy(&client, &ep, "app", "sql", Some("SCRAM-SHA-256$a"), Some("v1:a")).await;
    client.mark_password_unreadable(&ep, "app").await.unwrap();
    let mut tx = client.begin_ddl(&ep).await.unwrap().expect("endpoint");
    tx.rename_copy("app", "app2", None).await.unwrap();
    tx.commit().await.unwrap();
    let kept = password_of(&client, &ep, "app2").await;
    let mut tx = client.begin_ddl(&ep).await.unwrap().expect("endpoint");
    tx.rename_copy("app2", "app3", Some("v1:fresh")).await.unwrap();
    tx.commit().await.unwrap();
    let resealed = password_of(&client, &ep, "app3").await;
    cleanup(&client, id).await;
    assert_eq!(kept, PasswordState::Unreadable);
    assert_eq!(resealed, PasswordState::Generated);
}

/// The endpoint insert for a child branch takes the database row before any branch row, the
/// order `begin_delete` uses. While another transaction holds the database row, the insert
/// waits there and has not touched the child (or parent) branch row, so a third transaction
/// can still lock those rows with NOWAIT. With the old order (child row first) that NOWAIT
/// lock fails.
#[tokio::test]
#[ignore]
async fn child_endpoint_insert_waits_on_the_database_row_before_branch_rows() {
    let _sealed = sealed_lock().await;
    let client = client().await;
    let (id, _ep) = ready_database(&client).await;
    let parent = main_of(id);
    let b = new_branch(id, "child", parent);
    assert_eq!(client.create_branch(&b).await.unwrap(), CreateBranchOutcome::Created);

    let mut holder = client.pool().begin().await.unwrap();
    sqlx::query("SELECT 1 FROM databases WHERE id = $1 FOR UPDATE")
        .bind(id.as_uuid())
        .execute(&mut *holder)
        .await
        .unwrap();

    let (ep, tenant, timeline) = (ep_id(), hex32(), hex32());
    let task = {
        let client = client.clone();
        let (timeline, ep, tenant) = (timeline.clone(), ep.clone(), tenant.clone());
        tokio::spawn(async move {
            client
                .create_endpoint_for_branch(b.id, &timeline, &new_endpoint(&ep, &tenant, &timeline), false)
                .await
        })
    };
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let still_waiting = !task.is_finished();
    let mut probe = client.pool().begin().await.unwrap();
    let rows_free = sqlx::query(
        "SELECT 1 FROM branches WHERE id = ANY($1) FOR NO KEY UPDATE NOWAIT",
    )
    .bind(vec![b.id.as_uuid(), parent.as_uuid()])
    .fetch_all(&mut *probe)
    .await;
    probe.rollback().await.unwrap();
    holder.commit().await.unwrap();
    let linked = task.await.unwrap();
    cleanup(&client, id).await;
    assert!(still_waiting, "the insert should wait on the database row");
    assert!(rows_free.is_ok(), "branch rows were locked before the database row: {rows_free:?}");
    assert!(matches!(linked, Ok(true)), "{linked:?}");
}

// ---- step 6c: data API across databases ----

async fn state_of(client: &MetadataClient, branch: BranchId, name: &str) -> (DataApiState, Option<String>, String) {
    client
        .branch_databases(branch)
        .await
        .unwrap()
        .into_iter()
        .find(|d| d.name == name)
        .map(|d| (d.data_api, d.data_api_error, d.default_schema))
        .expect("database row")
}

/// Adds `names` (owner `app`, origin sql) to the branch's endpoint through a DDL transaction.
async fn add_databases(client: &MetadataClient, ep: &str, names: &[&str]) {
    let mut tx = client.begin_ddl(ep).await.unwrap().expect("endpoint");
    for name in names {
        tx.upsert_database(name, "app").await.unwrap();
    }
    tx.commit().await.unwrap();
}

fn key_with(branch: BranchId, role: &str, name: &str, databases: Option<&[&str]>) -> NewDataKey {
    let (mut key, _) = new_key(branch, DataKeyKind::Secret, role, name);
    key.databases = databases.map(|d| d.iter().map(|s| s.to_string()).collect());
    key
}

async fn key_databases(client: &MetadataClient, branch: BranchId, id: uuid::Uuid) -> Option<Vec<String>> {
    client.list_data_keys(branch).await.unwrap().into_iter().find(|k| k.id == id).map(|k| k.databases.unwrap_or_default())
}

#[tokio::test]
#[ignore]
async fn migration_backfills_main_as_on() {
    let _sealed = sealed_lock().await;
    let client = client().await;
    let (id, _) = ready_database(&client).await;
    let main = state_of(&client, main_of(id), "db").await;
    let cheladb_main_rows_off: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM branch_databases WHERE origin = 'main' AND data_api <> 'on'
           AND branch_id IN (SELECT id FROM branches WHERE database_id = $1)",
    )
    .bind(id.as_uuid())
    .fetch_one(client.pool())
    .await
    .unwrap();
    let column_gone: bool = sqlx::query_scalar(
        "SELECT NOT EXISTS (SELECT 1 FROM information_schema.columns
                            WHERE table_schema = current_schema() AND table_name = 'databases' AND column_name = 'default_schema')",
    )
    .fetch_one(client.pool())
    .await
    .unwrap();
    cleanup(&client, id).await;
    assert_eq!(main, (DataApiState::On, None, "public".to_string()));
    assert_eq!(cheladb_main_rows_off, 0);
    assert!(column_gone, "databases.default_schema moved to branch_databases");
}

#[tokio::test]
#[ignore]
async fn enable_respects_the_cap_and_refuses_system() {
    let _sealed = sealed_lock().await;
    let client = client().await;
    let (id, ep) = ready_database(&client).await;
    let branch = main_of(id);
    add_databases(&client, &ep, &["one", "two"]).await;
    // A system row, as the resync writes it.
    sqlx::query("INSERT INTO branch_databases (branch_id, name, owner, origin) VALUES ($1, 'postgres', 'cloud_admin', 'system')")
        .bind(branch.as_uuid())
        .execute(client.pool())
        .await
        .unwrap();
    // main is on, so a cap of 2 leaves room for one more.
    let first = client.begin_enable_data_api(branch, "one", 2).await.unwrap();
    let over = client.begin_enable_data_api(branch, "two", 2).await.unwrap();
    let again = client.begin_enable_data_api(branch, "one", 2).await.unwrap();
    let unknown = client.begin_enable_data_api(branch, "nope", 2).await.unwrap();
    let system = client.begin_enable_data_api(branch, "postgres", 2).await.unwrap();
    let main_again = client.begin_enable_data_api(branch, "db", 1).await.unwrap();
    let one_state = state_of(&client, branch, "one").await.0;
    let two_state = state_of(&client, branch, "two").await.0;
    let check_blocks_system = sqlx::query("UPDATE branch_databases SET data_api = 'on' WHERE branch_id = $1 AND name = 'postgres'")
        .bind(branch.as_uuid())
        .execute(client.pool())
        .await
        .is_err();
    cleanup(&client, id).await;
    assert_eq!(first, EnableStart::Started);
    assert_eq!(over, EnableStart::OverCap(2));
    assert_eq!(again, EnableStart::Started, "a retry while enabling starts again");
    assert_eq!(unknown, EnableStart::Unknown);
    assert_eq!(system, EnableStart::System);
    assert_eq!(main_again, EnableStart::AlreadyOn);
    assert_eq!((one_state, two_state), (DataApiState::Enabling, DataApiState::Off));
    assert!(check_blocks_system);
}

#[tokio::test]
#[ignore]
async fn enable_finish_records_errors() {
    let _sealed = sealed_lock().await;
    let client = client().await;
    let (id, ep) = ready_database(&client).await;
    let branch = main_of(id);
    add_databases(&client, &ep, &["one"]).await;
    assert_eq!(client.begin_enable_data_api(branch, "one", 10).await.unwrap(), EnableStart::Started);
    let failed = client.finish_enable_data_api(branch, "one", Err("boom".into())).await.unwrap();
    let after_fail = state_of(&client, branch, "one").await;
    assert_eq!(client.begin_enable_data_api(branch, "one", 10).await.unwrap(), EnableStart::Started);
    let cleared = state_of(&client, branch, "one").await;
    let ok = client.finish_enable_data_api(branch, "one", Ok(())).await.unwrap();
    let after_ok = state_of(&client, branch, "one").await;
    // Off again, and a finish that lands afterwards doesn't bring it back.
    let off = client.set_data_api_off(branch, "one").await.unwrap();
    let late = client.finish_enable_data_api(branch, "one", Ok(())).await.unwrap();
    let after_off = state_of(&client, branch, "one").await;
    let main_off = client.set_data_api_off(branch, "db").await.unwrap();
    let unknown_off = client.set_data_api_off(branch, "nope").await.unwrap();
    cleanup(&client, id).await;
    assert!(failed);
    assert_eq!(after_fail.0, DataApiState::Error);
    assert_eq!(after_fail.1.as_deref(), Some("boom"));
    assert_eq!((cleared.0, cleared.1), (DataApiState::Enabling, None));
    assert!(ok);
    assert_eq!((after_ok.0, after_ok.1), (DataApiState::On, None));
    assert!(off && !late);
    assert_eq!(after_off.0, DataApiState::Off);
    assert!(!main_off && !unknown_off, "main stays on; unknown is false");
}

/// A row remembers that an enable ever began on it (the bootstrap may have run), so a
/// `remove` can be refused on a database the data API never touched. Main is bootstrapped
/// with the branch; switching off keeps the mark.
#[tokio::test]
#[ignore]
async fn rows_remember_an_enable_began() {
    let _sealed = sealed_lock().await;
    let client = client().await;
    let (id, ep) = ready_database(&client).await;
    let branch = main_of(id);
    add_databases(&client, &ep, &["one", "two"]).await;
    let marks = |client: &MetadataClient| {
        let client = client.clone();
        async move {
            client
                .branch_databases(branch)
                .await
                .unwrap()
                .into_iter()
                .map(|d| (d.name, d.data_api_bootstrapped))
                .collect::<Vec<_>>()
        }
    };
    let before = marks(&client).await;
    client.begin_enable_data_api(branch, "one", 10).await.unwrap();
    client.finish_enable_data_api(branch, "one", Err("boom".into())).await.unwrap();
    client.set_data_api_off(branch, "one").await.unwrap();
    let after = marks(&client).await;
    cleanup(&client, id).await;
    let m = |n: &str, b: bool| (n.to_string(), b);
    assert_eq!(before, vec![m("db", true), m("one", false), m("two", false)]);
    assert_eq!(after, vec![m("db", true), m("one", true), m("two", false)]);
}

#[tokio::test]
#[ignore]
async fn served_databases_lists_on_only_main_first() {
    let _sealed = sealed_lock().await;
    let client = client().await;
    let (id, ep) = ready_database(&client).await;
    let branch = main_of(id);
    add_databases(&client, &ep, &["b_on", "a_on", "c_off", "d_enabling"]).await;
    for name in ["b_on", "a_on"] {
        client.begin_enable_data_api(branch, name, 10).await.unwrap();
        client.finish_enable_data_api(branch, name, Ok(())).await.unwrap();
    }
    client.begin_enable_data_api(branch, "d_enabling", 10).await.unwrap();
    assert!(client.set_database_default_schema(branch, "a_on", "app").await.unwrap());
    let served = client.served_databases(branch).await.unwrap();
    let none = client.served_databases(BranchId::new()).await.unwrap();
    cleanup(&client, id).await;
    let s = |n: &str, d: &str| ServedDatabase { name: n.into(), default_schema: d.into() };
    assert_eq!(served, vec![s("db", "public"), s("a_on", "app"), s("b_on", "public")]);
    assert!(none.is_empty());
}

#[tokio::test]
#[ignore]
async fn default_schema_check() {
    let _sealed = sealed_lock().await;
    let client = client().await;
    let (id, _) = ready_database(&client).await;
    let branch = main_of(id);
    let mut rejected = 0;
    for bad in ["Public", "1abc", "", &"a".repeat(64), "a-b"] {
        if matches!(client.set_database_default_schema(branch, "db", bad).await, Err(MetadataError::Database(_))) {
            rejected += 1;
        }
    }
    let edge = client.set_database_default_schema(branch, "db", &format!("_{}", "a".repeat(62))).await.unwrap();
    let ok = client.set_database_default_schema(branch, "db", "_app_1").await.unwrap();
    let unknown = client.set_database_default_schema(branch, "nope", "app").await.unwrap();
    let after = state_of(&client, branch, "db").await.2;
    cleanup(&client, id).await;
    assert_eq!(rejected, 5);
    assert!(edge && ok && !unknown);
    assert_eq!(after, "_app_1");
}

#[tokio::test]
#[ignore]
async fn child_copies_parent_flags() {
    let _sealed = sealed_lock().await;
    let client = client().await;
    let (id, ep) = ready_database(&client).await;
    let parent = main_of(id);
    add_databases(&client, &ep, &["served", "pending", "plain"]).await;
    client.begin_enable_data_api(parent, "served", 10).await.unwrap();
    client.finish_enable_data_api(parent, "served", Ok(())).await.unwrap();
    client.set_database_default_schema(parent, "served", "app").await.unwrap();
    client.set_database_default_schema(parent, "db", "main_schema").await.unwrap();
    client.begin_enable_data_api(parent, "pending", 10).await.unwrap();

    let b = new_branch(id, "child", parent);
    assert_eq!(client.create_branch(&b).await.unwrap(), CreateBranchOutcome::Created);
    let child_ep = link_branch(&client, b.id).await;
    // The child's main row is served at once, with the parent's schema.
    let main_at_creation = state_of(&client, b.id, "db").await;
    // The first resync brings the other rows in, all off.
    add_databases(&client, &child_ep, &["served", "pending", "plain", "extra"]).await;
    let before = (state_of(&client, b.id, "served").await.0, state_of(&client, b.id, "pending").await.0);
    client.copy_parent_database_flags(b.id).await.unwrap();
    let served = state_of(&client, b.id, "served").await;
    let pending = state_of(&client, b.id, "pending").await;
    let plain = state_of(&client, b.id, "plain").await;
    let extra = state_of(&client, b.id, "extra").await;
    let main = state_of(&client, b.id, "db").await;
    let parent_pending = state_of(&client, parent, "pending").await.0;
    // A branch with no parent is a no-op.
    client.copy_parent_database_flags(parent).await.unwrap();
    cleanup(&client, id).await;
    assert_eq!(main_at_creation, (DataApiState::On, None, "main_schema".to_string()));
    assert_eq!(before, (DataApiState::Off, DataApiState::Off));
    assert_eq!(served, (DataApiState::On, None, "app".to_string()));
    assert_eq!(pending.0, DataApiState::Off);
    assert_eq!(plain.0, DataApiState::Off);
    assert_eq!(extra, (DataApiState::Off, None, "public".to_string()));
    assert_eq!(main, (DataApiState::On, None, "main_schema".to_string()));
    assert_eq!(parent_pending, DataApiState::Enabling, "the parent keeps its own state");
}

#[tokio::test]
#[ignore]
async fn key_lists_follow_renames() {
    let _sealed = sealed_lock().await;
    let client = client().await;
    let (id, ep) = ready_database(&client).await;
    let branch = main_of(id);
    add_databases(&client, &ep, &["a", "b"]).await;
    let other = ready_branch(&client, id, "other", branch).await;
    let listed = key_with(branch, "service_role", "listed", Some(&["a", "b"]));
    let every = key_with(branch, "service_role", "every", None);
    let elsewhere = key_with(other, "service_role", "elsewhere", Some(&["a"]));
    for k in [&listed, &every, &elsewhere] {
        client.insert_data_key(k).await.unwrap();
    }
    let mut tx = client.begin_ddl(&ep).await.unwrap().expect("endpoint");
    assert!(tx.rename_database("a", "c").await.unwrap());
    tx.commit().await.unwrap();
    let after = key_databases(&client, branch, listed.id).await;
    let every_after = client.list_data_keys(branch).await.unwrap().into_iter().find(|k| k.id == every.id).unwrap();
    let other_after = key_databases(&client, other, elsewhere.id).await;
    cleanup(&client, id).await;
    assert_eq!(after, Some(vec!["c".to_string(), "b".to_string()]));
    assert_eq!(every_after.databases, None, "an unlisted key stays unlisted");
    assert_eq!(other_after, Some(vec!["a".to_string()]), "another branch's keys are untouched");
}

#[tokio::test]
#[ignore]
async fn dropping_the_last_allowed_database_revokes_the_key() {
    let _sealed = sealed_lock().await;
    let client = client().await;
    let (id, ep) = ready_database(&client).await;
    let branch = main_of(id);
    add_databases(&client, &ep, &["a", "b"]).await;
    let only_a = key_with(branch, "service_role", "only-a", Some(&["a"]));
    let both = key_with(branch, "service_role", "both", Some(&["a", "b"]));
    let every = key_with(branch, "service_role", "every", None);
    for k in [&only_a, &both, &every] {
        client.insert_data_key(k).await.unwrap();
    }
    let mut tx = client.begin_ddl(&ep).await.unwrap().expect("endpoint");
    assert!(tx.remove_database("a").await.unwrap());
    tx.commit().await.unwrap();
    let live = client.list_data_keys(branch).await.unwrap();
    let revoked_lookup = client.lookup_data_key(&only_a.sha256).await.unwrap();
    let both_after = live.iter().find(|k| k.id == both.id).and_then(|k| k.databases.clone());
    let every_live = live.iter().any(|k| k.id == every.id);
    let only_a_live = live.iter().any(|k| k.id == only_a.id);
    cleanup(&client, id).await;
    assert!(!only_a_live && revoked_lookup.is_none());
    assert_eq!(both_after, Some(vec!["b".to_string()]));
    assert!(every_live);
}

#[tokio::test]
#[ignore]
async fn rotation_keeps_the_list() {
    let _sealed = sealed_lock().await;
    let client = client().await;
    let (id, _) = ready_database(&client).await;
    let branch = main_of(id);
    let old = key_with(branch, "service_role", "old", Some(&["db", "x"]));
    client.insert_data_key(&old).await.unwrap();
    let (mut new, _) = new_key(branch, DataKeyKind::Secret, "service_role", "old");
    new.databases = Some(vec!["ignored".into()]);
    assert!(client.rotate_data_key(branch, old.id, &new).await.unwrap());
    let after = key_databases(&client, branch, new.id).await;
    cleanup(&client, id).await;
    assert_eq!(after, Some(vec!["db".to_string(), "x".to_string()]));
}

#[tokio::test]
#[ignore]
async fn lookup_returns_the_list() {
    let _sealed = sealed_lock().await;
    let client = client().await;
    let (id, _) = ready_database(&client).await;
    let branch = main_of(id);
    let listed = key_with(branch, "service_role", "listed", Some(&["db"]));
    let every = key_with(branch, "service_role", "every", None);
    client.insert_data_key(&listed).await.unwrap();
    client.insert_data_key(&every).await.unwrap();
    let l = client.lookup_data_key(&listed.sha256).await.unwrap().expect("listed");
    let e = client.lookup_data_key(&every.sha256).await.unwrap().expect("every");
    // set_key_databases: replace, clear, refuse a revoked or unknown key, and the CHECK bounds.
    let set = client.set_key_databases(branch, listed.id, Some(vec!["x".into(), "y".into()])).await.unwrap();
    let after_set = client.lookup_data_key(&listed.sha256).await.unwrap().unwrap().databases;
    let cleared = client.set_key_databases(branch, listed.id, None).await.unwrap();
    let after_clear = client.lookup_data_key(&listed.sha256).await.unwrap().unwrap().databases;
    let unknown = client.set_key_databases(branch, uuid::Uuid::new_v4(), None).await.unwrap();
    let other_branch = client.set_key_databases(BranchId::new(), listed.id, None).await.unwrap();
    let empty = client.set_key_databases(branch, listed.id, Some(vec![])).await;
    let too_many = client
        .set_key_databases(branch, listed.id, Some((0..21).map(|i| format!("d{i}")).collect()))
        .await;
    cleanup(&client, id).await;
    assert_eq!(l.databases, Some(vec!["db".to_string()]));
    assert_eq!(e.databases, None);
    assert!(set && cleared && !unknown && !other_branch);
    assert_eq!(after_set, Some(vec!["x".to_string(), "y".to_string()]));
    assert_eq!(after_clear, None);
    assert!(matches!(empty, Err(MetadataError::Database(_))));
    assert!(matches!(too_many, Err(MetadataError::Database(_))));
}

#[tokio::test]
#[ignore]
async fn branch_connection_lists_served_databases() {
    let _sealed = sealed_lock().await;
    let client = client().await;
    let (id, ep) = ready_database(&client).await;
    let branch = main_of(id);
    add_databases(&client, &ep, &["two", "three"]).await;
    client.begin_enable_data_api(branch, "two", 10).await.unwrap();
    client.finish_enable_data_api(branch, "two", Ok(())).await.unwrap();
    client.set_database_default_schema(branch, "two", "app").await.unwrap();
    let row = client.branch_connection(branch).await.unwrap().expect("row");
    cleanup(&client, id).await;
    let s = |n: &str, d: &str| ServedDatabase { name: n.into(), default_schema: d.into() };
    assert_eq!(row.databases, vec![s("db", "public"), s("two", "app")]);
}

#[tokio::test]
#[ignore]
async fn resync_drop_removes_the_name_from_key_lists() {
    let _sealed = sealed_lock().await;
    let client = client().await;
    let (id, ep) = ready_database(&client).await;
    let branch = main_of(id);
    let resync = |dbs: Vec<ObservedDatabase>| {
        let client = client.clone();
        let ep = ep.clone();
        async move {
            let (_, v) = client.role_copies(&ep).await.unwrap();
            client
                .apply_resync(&ep, branch, v, &ResyncWrite { upserts: vec![], removals: vec![], databases: dbs })
                .await
                .unwrap();
        }
    };
    resync(vec![
        observed("postgres", "cloud_admin", true),
        observed("db", "postgres", false),
        observed("a", "app", false),
        observed("b", "app", false),
    ])
    .await;
    let only_a = key_with(branch, "service_role", "only-a", Some(&["a"]));
    let both = key_with(branch, "service_role", "both", Some(&["a", "b"]));
    let every = key_with(branch, "service_role", "every", None);
    for k in [&only_a, &both, &every] {
        client.insert_data_key(k).await.unwrap();
    }
    resync(vec![
        observed("postgres", "cloud_admin", true),
        observed("db", "postgres", false),
        observed("b", "app", false),
    ])
    .await;
    let live = client.list_data_keys(branch).await.unwrap();
    let revoked_lookup = client.lookup_data_key(&only_a.sha256).await.unwrap();
    let both_after = live.iter().find(|k| k.id == both.id).and_then(|k| k.databases.clone());
    let every_live = live.iter().any(|k| k.id == every.id);
    let only_a_live = live.iter().any(|k| k.id == only_a.id);
    cleanup(&client, id).await;
    assert!(!only_a_live && revoked_lookup.is_none(), "a list emptied by the drop revokes the key");
    assert_eq!(both_after, Some(vec!["b".to_string()]));
    assert!(every_live);
}

#[tokio::test]
#[ignore]
async fn resync_main_origin_is_served() {
    let _sealed = sealed_lock().await;
    let client = client().await;
    let (id, ep) = ready_database(&client).await;
    let branch = main_of(id);
    // A row that was `sql` and `off` becomes the main database (the endpoint's first one).
    sqlx::query("UPDATE branch_databases SET origin = 'sql', data_api = 'off' WHERE branch_id = $1 AND name = 'db'")
        .bind(branch.as_uuid())
        .execute(client.pool())
        .await
        .unwrap();
    let (_, v) = client.role_copies(&ep).await.unwrap();
    let write = ResyncWrite {
        upserts: vec![],
        removals: vec![],
        databases: vec![observed("postgres", "cloud_admin", true), observed("db", "postgres", false)],
    };
    client.apply_resync(&ep, branch, v, &write).await.unwrap();
    let served = client.served_databases(branch).await.unwrap();
    cleanup(&client, id).await;
    assert_eq!(served.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["db"]);
}

#[tokio::test]
#[ignore]
async fn enabling_databases_lists_only_enabling_rows() {
    let _sealed = sealed_lock().await;
    let client = client().await;
    let (id, ep) = ready_database(&client).await;
    let branch = main_of(id);
    add_databases(&client, &ep, &["pending", "done", "idle"]).await;
    client.begin_enable_data_api(branch, "pending", 10).await.unwrap();
    client.begin_enable_data_api(branch, "done", 10).await.unwrap();
    client.finish_enable_data_api(branch, "done", Ok(())).await.unwrap();
    let listed = client.enabling_databases().await.unwrap();
    cleanup(&client, id).await;
    let mine: Vec<_> = listed.into_iter().filter(|(b, _)| *b == branch).collect();
    assert_eq!(mine, vec![(branch, "pending".to_string())]);
}

// ---- Step 6b: projects and app users ----

/// A project of this run in the demo tenant, named `live-test-<run> <tag>` (slug
/// `live-test-<run>-<tag>`).
async fn new_project(client: &MetadataClient, tag: &str) -> ProjectId {
    client
        .create_project(TenantId(DEMO_TENANT_ID), &format!("live-test-{} {tag}", run_id()))
        .await
        .unwrap()
        .id
}

async fn insert_pending_in(client: &MetadataClient, project: ProjectId) -> DatabaseId {
    let id = DatabaseId::new();
    client
        .insert_database_pending(id, TenantId(DEMO_TENANT_ID), project, &format!("live-test-{}-{id}", run_id()))
        .await
        .unwrap();
    id
}

/// A ready database (main with an endpoint) in `project`. Hold `sealed_lock` while it lives.
async fn ready_database_in(client: &MetadataClient, project: ProjectId) -> DatabaseId {
    let id = insert_pending_in(client, project).await;
    link_endpoint(client, id).await;
    assert!(make_ready(client, id, "v1:owner").await);
    id
}

/// Removes everything a test made in `project`: sessions, app-user rows, databases, the project.
async fn cleanup_project(client: &MetadataClient, project: ProjectId, databases: &[DatabaseId]) {
    sqlx::query("DELETE FROM app_sessions WHERE project_id = $1")
        .bind(project.as_uuid())
        .execute(client.pool())
        .await
        .unwrap();
    sqlx::query("DELETE FROM project_auth WHERE project_id = $1")
        .bind(project.as_uuid())
        .execute(client.pool())
        .await
        .unwrap();
    for db in databases {
        cleanup(client, *db).await;
    }
    sqlx::query("DELETE FROM projects WHERE id = $1")
        .bind(project.as_uuid())
        .execute(client.pool())
        .await
        .unwrap();
}

fn demo() -> TenantId {
    TenantId(DEMO_TENANT_ID)
}

fn new_auth<'a>(project: ProjectId, db: DatabaseId) -> NewProjectAuth<'a> {
    NewProjectAuth {
        project_id: project,
        database_id: db,
        pg_database: "db",
        schema_name: "public",
        table_name: "users",
        email_verification: false,
        on_delete: OnDelete::Delete,
    }
}

async fn auth_status(client: &MetadataClient, project: ProjectId) -> Option<AppUsersStatus> {
    client.project_auth(project).await.unwrap().map(|a| a.status)
}

fn hash(n: u8) -> [u8; 32] {
    [n; 32]
}

fn new_session<'a>(project: ProjectId, user: uuid::Uuid, refresh: u8) -> NewAppSession<'a> {
    NewAppSession {
        id: uuid::Uuid::new_v4(),
        project_id: project,
        user_id: user,
        zitadel_session_id: "zs-1",
        refresh_hash: hash(refresh),
        expires_at: chrono::Utc::now() + chrono::Duration::days(1),
    }
}

fn next(new: u8, grace_secs: u64) -> RefreshNext {
    RefreshNext {
        new_hash: hash(new),
        new_sealed: format!("sealed-{new}"),
        sliding: std::time::Duration::from_secs(3600),
        cap: std::time::Duration::from_secs(30 * 86400),
        grace: std::time::Duration::from_secs(grace_secs),
    }
}

async fn backdate_session(client: &MetadataClient, id: uuid::Uuid, secs: i64) {
    sqlx::query("UPDATE app_sessions SET created_at = now() - make_interval(secs => $2) WHERE id = $1")
        .bind(id)
        .bind(secs as f64)
        .execute(client.pool())
        .await
        .unwrap();
}

#[tokio::test]
#[ignore]
async fn project_slug_collisions_get_suffixes() {
    let client = client().await;
    let name = format!("live-test-{} My App", run_id());
    let first = client.create_project(demo(), &name).await.unwrap();
    let second = client.create_project(demo(), &name).await.unwrap();
    let base = format!("live-test-{}-my-app", run_id());
    cleanup_project(&client, first.id, &[]).await;
    cleanup_project(&client, second.id, &[]).await;
    assert_eq!(first.slug, base);
    assert_eq!(second.slug, format!("{base}-2"));
    assert_eq!(first.status, ProjectStatus::Active);
    assert_eq!(first.name, name);
}

#[tokio::test]
#[ignore]
async fn rename_changes_name_only() {
    let client = client().await;
    let project = new_project(&client, "rename").await;
    let before = client.get_project(demo(), project).await.unwrap().unwrap();
    let renamed = client.rename_project(demo(), project, "Renamed").await.unwrap();
    let other_tenant = client.rename_project(TenantId::new(), project, "Nope").await.unwrap();
    let after = client.get_project(demo(), project).await.unwrap().unwrap();
    cleanup_project(&client, project, &[]).await;
    let RenameProject::Renamed(record) = renamed else { panic!("{renamed:?}") };
    assert_eq!(record.name, "Renamed");
    assert_eq!(record.slug, before.slug);
    assert_eq!(after.name, "Renamed");
    assert_eq!(after.slug, before.slug);
    assert_eq!(other_tenant, RenameProject::NotFound);
}

#[tokio::test]
#[ignore]
async fn begin_project_delete_marks_databases() {
    let _sealed = sealed_lock().await;
    let client = client().await;
    let project = new_project(&client, "del").await;
    let (a, b) = (ready_database_in(&client, project).await, ready_database_in(&client, project).await);
    let started = client.begin_project_delete(demo(), project).await.unwrap();
    let statuses = [
        client.get_database(a).await.unwrap().unwrap().status,
        client.get_database(b).await.unwrap().unwrap().status,
    ];
    let mains = [main_branch(&client, a).await.status, main_branch(&client, b).await.status];
    let record = client.get_project(demo(), project).await.unwrap().unwrap();
    let listed = client.project_databases(project).await.unwrap();
    cleanup_project(&client, project, &[a, b]).await;
    let BeginProjectDelete::Started(mut ids) = started else { panic!("{started:?}") };
    ids.sort_by_key(|d| d.as_uuid());
    let mut want = vec![a, b];
    want.sort_by_key(|d| d.as_uuid());
    assert_eq!(ids, want);
    assert_eq!(statuses, [DatabaseStatus::Deleting; 2]);
    assert_eq!(mains, [BranchStatus::Deleting; 2]);
    assert_eq!(record.status, ProjectStatus::Deleting);
    assert_eq!(listed.len(), 2);
}

#[tokio::test]
#[ignore]
async fn begin_project_delete_refusals() {
    let _sealed = sealed_lock().await;
    let client = client().await;

    // Another tenant's project is not found.
    let project = new_project(&client, "ref-tenant").await;
    let other = client.begin_project_delete(TenantId::new(), project).await.unwrap();
    cleanup_project(&client, project, &[]).await;
    assert_eq!(other, BeginProjectDelete::NotFound);
    assert_eq!(
        client.begin_project_delete(demo(), ProjectId::new()).await.unwrap(),
        BeginProjectDelete::NotFound
    );

    // A database still provisioning.
    let project = new_project(&client, "ref-pending").await;
    let pending = insert_pending_in(&client, project).await;
    let refused = client.begin_project_delete(demo(), project).await.unwrap();
    let status = client.get_project(demo(), project).await.unwrap().unwrap().status;
    cleanup_project(&client, project, &[pending]).await;
    assert_eq!(refused, BeginProjectDelete::NotReady(pending));
    assert_eq!(status, ProjectStatus::Active, "nothing written");

    // A protected branch.
    let project = new_project(&client, "ref-protected").await;
    let db = ready_database_in(&client, project).await;
    let dev = ready_branch(&client, db, "dev", main_of(db)).await;
    assert!(client.set_branch_protected(dev, true).await.unwrap().is_some());
    let refused = client.begin_project_delete(demo(), project).await.unwrap();
    let db_status = client.get_database(db).await.unwrap().unwrap().status;
    cleanup_project(&client, project, &[db]).await;
    assert_eq!(refused, BeginProjectDelete::Protected(db));
    assert_eq!(db_status, DatabaseStatus::Ready, "nothing written");

    // App users being enabled.
    let project = new_project(&client, "ref-busy").await;
    let db = ready_database_in(&client, project).await;
    assert_eq!(client.begin_enable_app_users(&new_auth(project, db)).await.unwrap(), BeginEnable::Started);
    let busy = client.begin_project_delete(demo(), project).await.unwrap();
    assert!(client.finish_enable_app_users(project, Ok(())).await.unwrap());
    assert!(client.begin_disable_app_users(project).await.unwrap());
    let busy_disabling = client.begin_project_delete(demo(), project).await.unwrap();
    cleanup_project(&client, project, &[db]).await;
    assert_eq!(busy, BeginProjectDelete::AppUsersBusy);
    assert_eq!(busy_disabling, BeginProjectDelete::AppUsersBusy);

    // Already deleting.
    let project = new_project(&client, "ref-twice").await;
    let first = client.begin_project_delete(demo(), project).await.unwrap();
    let second = client.begin_project_delete(demo(), project).await.unwrap();
    cleanup_project(&client, project, &[]).await;
    assert_eq!(first, BeginProjectDelete::Started(vec![]));
    assert_eq!(second, BeginProjectDelete::AlreadyDeleting);
}

#[tokio::test]
#[ignore]
async fn insert_database_refuses_a_deleting_project() {
    let client = client().await;
    let project = new_project(&client, "ins").await;
    assert_eq!(client.begin_project_delete(demo(), project).await.unwrap(), BeginProjectDelete::Started(vec![]));
    let err = client
        .insert_database_pending(DatabaseId::new(), demo(), project, "never")
        .await
        .unwrap_err();
    cleanup_project(&client, project, &[]).await;
    assert!(matches!(err, MetadataError::ProjectDeleting(p) if p == project), "{err:?}");
}

#[tokio::test]
#[ignore]
async fn finish_project_delete_cleans_up() {
    let _sealed = sealed_lock().await;
    let client = client().await;
    let project = new_project(&client, "fin").await;
    let slug = client.get_project(demo(), project).await.unwrap().unwrap().slug;
    let db = ready_database_in(&client, project).await;
    assert_eq!(client.begin_enable_app_users(&new_auth(project, db)).await.unwrap(), BeginEnable::Started);
    assert!(client.finish_enable_app_users(project, Ok(())).await.unwrap());
    client.set_branch_auth_sync(main_of(db), "db", "v1:sync").await.unwrap();
    let session = new_session(project, uuid::Uuid::new_v4(), 1);
    client.insert_app_session(&session).await.unwrap();
    client.set_project_error(project, Some("waiting")).await.unwrap();
    assert!(matches!(
        client.begin_project_delete(demo(), project).await.unwrap(),
        BeginProjectDelete::Started(_)
    ));

    client.finish_project_delete(project).await.unwrap();
    let revoked = client.app_session(session.id).await.unwrap().unwrap().revoked_at;
    let auth = client.project_auth(project).await.unwrap();
    let sync = client.branch_auth_sync(main_of(db)).await.unwrap();
    let live = client.get_project(demo(), project).await.unwrap();
    let by_id = client.project_by_id(project).await.unwrap();
    let (status, error, deleted_at): (String, Option<String>, Option<chrono::DateTime<chrono::Utc>>) =
        sqlx::query_as("SELECT status, error, deleted_at FROM projects WHERE id = $1")
            .bind(project.as_uuid())
            .fetch_one(client.pool())
            .await
            .unwrap();
    let again = client.create_project(demo(), &format!("live-test-{} fin", run_id())).await.unwrap();
    cleanup_project(&client, project, &[db]).await;
    cleanup_project(&client, again.id, &[]).await;
    assert!(revoked.is_some());
    assert!(auth.is_none() && sync.is_none() && live.is_none() && by_id.is_none());
    assert_eq!((status.as_str(), error), ("deleted", None));
    assert!(deleted_at.is_some());
    assert_eq!(again.slug, slug, "the slug is free again");
}

#[tokio::test]
#[ignore]
async fn enable_app_users_start_retry_exists() {
    let _sealed = sealed_lock().await;
    let client = client().await;
    let project = new_project(&client, "enable").await;
    let db = ready_database_in(&client, project).await;
    let auth = new_auth(project, db);

    let started = client.begin_enable_app_users(&auth).await.unwrap();
    let again = client.begin_enable_app_users(&auth).await.unwrap();
    assert!(client.finish_enable_app_users(project, Err("boom".into())).await.unwrap());
    let failed = client.project_auth(project).await.unwrap().unwrap();
    // The same place retries, with the new settings.
    let changed = NewProjectAuth { email_verification: true, on_delete: OnDelete::Mark, ..auth };
    let retry = client.begin_enable_app_users(&changed).await.unwrap();
    let retried = client.project_auth(project).await.unwrap().unwrap();
    // Another place doesn't, even from `failed`.
    assert!(client.finish_enable_app_users(project, Err("boom".into())).await.unwrap());
    let elsewhere = client.begin_enable_app_users(&NewProjectAuth { table_name: "members", ..auth }).await.unwrap();
    // Nor does an enabled row.
    assert!(client.begin_enable_app_users(&changed).await.unwrap() == BeginEnable::Retry);
    assert!(client.finish_enable_app_users(project, Ok(())).await.unwrap());
    let enabled = client.begin_enable_app_users(&changed).await.unwrap();
    let by_db = client.app_auth_for_database(db).await.unwrap();
    let listed = client.enabled_app_projects().await.unwrap();
    client.set_app_users_org(project, "org-live-test").await.unwrap();
    let by_org = client.project_auth_by_org("org-live-test").await.unwrap();
    let settings = client
        .update_app_users_settings(project, Some(false), None)
        .await
        .unwrap()
        .unwrap();
    cleanup_project(&client, project, &[db]).await;
    assert_eq!(started, BeginEnable::Started);
    assert!(matches!(again, BeginEnable::Exists(ref a) if a.status == AppUsersStatus::Enabling));
    assert_eq!((failed.status, failed.error.as_deref()), (AppUsersStatus::Failed, Some("boom")));
    assert_eq!(retry, BeginEnable::Retry);
    assert_eq!((retried.status, retried.error), (AppUsersStatus::Enabling, None));
    assert!(retried.email_verification && retried.on_delete == OnDelete::Mark);
    assert!(matches!(elsewhere, BeginEnable::Exists(ref a) if a.table_name == "users"));
    assert!(matches!(enabled, BeginEnable::Exists(ref a) if a.status == AppUsersStatus::Enabled));
    assert_eq!(by_db.map(|a| a.project_id), Some(project));
    assert!(listed.iter().any(|a| a.project_id == project));
    assert_eq!(by_org.map(|a| a.project_id), Some(project));
    assert!(!settings.email_verification && settings.on_delete == OnDelete::Mark);
}

#[tokio::test]
#[ignore]
async fn finish_enable_moves_only_enabling() {
    let _sealed = sealed_lock().await;
    let client = client().await;
    let project = new_project(&client, "finish-enable").await;
    let db = ready_database_in(&client, project).await;
    assert!(!client.finish_enable_app_users(project, Ok(())).await.unwrap(), "no row");
    client.begin_enable_app_users(&new_auth(project, db)).await.unwrap();
    let first = client.finish_enable_app_users(project, Ok(())).await.unwrap();
    let second = client.finish_enable_app_users(project, Err("late".into())).await.unwrap();
    let status = auth_status(&client, project).await;
    let disabled = client.begin_disable_app_users(project).await.unwrap();
    let disabled_again = client.begin_disable_app_users(project).await.unwrap();
    let finish_disabling = client.finish_enable_app_users(project, Ok(())).await.unwrap();
    let still = auth_status(&client, project).await;
    cleanup_project(&client, project, &[db]).await;
    assert!(first && !second);
    assert_eq!(status, Some(AppUsersStatus::Enabled));
    assert!(disabled && !disabled_again && !finish_disabling);
    assert_eq!(still, Some(AppUsersStatus::Disabling));
}

#[tokio::test]
#[ignore]
async fn enable_refuses_a_deleting_project() {
    let client = client().await;
    let project = new_project(&client, "enable-deleting").await;
    client.begin_project_delete(demo(), project).await.unwrap();
    let err = client
        .begin_enable_app_users(&new_auth(project, DatabaseId::new()))
        .await
        .unwrap_err();
    cleanup_project(&client, project, &[]).await;
    assert!(matches!(err, MetadataError::ProjectDeleting(p) if p == project), "{err:?}");
}

#[tokio::test]
#[ignore]
async fn branch_auth_sync_upserts() {
    let _sealed = sealed_lock().await;
    let client = client().await;
    let (id, _) = ready_database(&client).await;
    let branch = main_of(id);
    let none = client.branch_auth_sync(branch).await.unwrap();
    client.set_branch_auth_sync(branch, "db", "v1:one").await.unwrap();
    let one = client.branch_auth_sync(branch).await.unwrap().unwrap();
    client.set_branch_auth_sync(branch, "db2", "v1:two").await.unwrap();
    let two = client.branch_auth_sync(branch).await.unwrap().unwrap();
    cleanup(&client, id).await;
    assert!(none.is_none());
    assert_eq!((one.pg_database.as_str(), one.password_sealed.as_str()), ("db", "v1:one"));
    assert_eq!((two.pg_database.as_str(), two.password_sealed.as_str()), ("db2", "v1:two"));
    assert_eq!(two.branch_id, branch);
}

// End-of-step fix (T8 race): two bootstraps of one branch end up with one stored password.
#[tokio::test]
#[ignore]
async fn branch_auth_sync_claim_keeps_the_first_for_the_same_database() {
    let _sealed = sealed_lock().await;
    let client = client().await;
    let (id, _) = ready_database(&client).await;
    let branch = main_of(id);
    let first = client.claim_branch_auth_sync(branch, "db", "v1:a").await.unwrap();
    let second = client.claim_branch_auth_sync(branch, "db", "v1:b").await.unwrap();
    let stored = client.branch_auth_sync(branch).await.unwrap().unwrap();
    // A row made for another Postgres database is replaced.
    let moved = client.claim_branch_auth_sync(branch, "db2", "v1:c").await.unwrap();
    let after = client.branch_auth_sync(branch).await.unwrap().unwrap();
    cleanup(&client, id).await;
    assert_eq!((first.as_str(), second.as_str()), ("v1:a", "v1:a"));
    assert_eq!((stored.pg_database.as_str(), stored.password_sealed.as_str()), ("db", "v1:a"));
    assert_eq!(moved, "v1:c");
    assert_eq!((after.pg_database.as_str(), after.password_sealed.as_str()), ("db2", "v1:c"));
}

// Fix follow-up (M3): replacing an unreadable password is a compare-and-swap, so a concurrent
// replacement wins once and the loser gets the winner's value back.
#[tokio::test]
#[ignore]
async fn branch_auth_sync_swap_replaces_only_the_expected_value() {
    let _sealed = sealed_lock().await;
    let client = client().await;
    let (id, _) = ready_database(&client).await;
    let branch = main_of(id);
    client.claim_branch_auth_sync(branch, "db", "v1:old").await.unwrap();
    let first = client.swap_branch_auth_sync(branch, "db", "v1:old", "v1:a").await.unwrap();
    let second = client.swap_branch_auth_sync(branch, "db", "v1:old", "v1:b").await.unwrap();
    let stored = client.branch_auth_sync(branch).await.unwrap().unwrap();
    // No row (or one for another database): the swap stores the new value.
    client.claim_branch_auth_sync(branch, "db2", "v1:other").await.unwrap();
    let moved = client.swap_branch_auth_sync(branch, "db", "v1:a", "v1:c").await.unwrap();
    cleanup(&client, id).await;
    assert_eq!((first.as_str(), second.as_str()), ("v1:a", "v1:a"));
    assert_eq!(stored.password_sealed, "v1:a");
    assert_eq!(moved, "v1:c");
}

// End-of-step fix (T1): Studio's session list leaves out expired sessions.
#[tokio::test]
#[ignore]
async fn list_user_sessions_leaves_out_expired_ones() {
    let client = client().await;
    let project = new_project(&client, "list-expired").await;
    let user = uuid::Uuid::new_v4();
    let (live, expired) = (new_session(project, user, 1), new_session(project, user, 2));
    for s in [&live, &expired] {
        client.insert_app_session(s).await.unwrap();
    }
    sqlx::query("UPDATE app_sessions SET expires_at = now() - interval '1 second' WHERE id = $1")
        .bind(expired.id)
        .execute(client.pool())
        .await
        .unwrap();
    let listed = client.list_user_sessions(project, user).await.unwrap();
    cleanup_project(&client, project, &[]).await;
    assert_eq!(listed.iter().map(|s| s.id).collect::<Vec<_>>(), vec![live.id]);
}

// End-of-step fix (N5): the sealed grace token doesn't outlive the grace window.
#[tokio::test]
#[ignore]
async fn purge_app_sessions_clears_grace_tokens_past_the_window() {
    let client = client().await;
    let project = new_project(&client, "purge-grace").await;
    let user = uuid::Uuid::new_v4();
    let (past, within) = (new_session(project, user, 1), new_session(project, user, 3));
    client.insert_app_session(&past).await.unwrap();
    client.insert_app_session(&within).await.unwrap();
    client.refresh_app_session(past.id, project, hash(1), &next(2, 60)).await.unwrap();
    client.refresh_app_session(within.id, project, hash(3), &next(4, 60)).await.unwrap();
    sqlx::query("UPDATE app_sessions SET grace_until = now() - interval '1 second' WHERE id = $1")
        .bind(past.id)
        .execute(client.pool())
        .await
        .unwrap();
    client.purge_app_sessions(std::time::Duration::from_secs(86400)).await.unwrap();
    let sealed = |id: uuid::Uuid| {
        let client = &client;
        async move {
            sqlx::query_scalar::<_, Option<String>>("SELECT grace_refresh_sealed FROM app_sessions WHERE id = $1")
                .bind(id)
                .fetch_one(client.pool())
                .await
                .unwrap()
        }
    };
    let (past_sealed, within_sealed) = (sealed(past.id).await, sealed(within.id).await);
    // The previous token past the window is reuse, not grace.
    let reused = client.refresh_app_session(past.id, project, hash(1), &next(5, 60)).await.unwrap();
    cleanup_project(&client, project, &[]).await;
    assert_eq!(past_sealed, None);
    assert_eq!(within_sealed.as_deref(), Some("sealed-4"));
    assert!(matches!(reused, RefreshOutcome::Reused(_)), "{reused:?}");
}

// Review Focus 1 (SQL half): rotation, the grace window and reuse detection.
#[tokio::test]
#[ignore]
async fn refresh_rotates_grace_and_reuse() {
    let client = client().await;
    let project = new_project(&client, "refresh").await;
    let session = new_session(project, uuid::Uuid::new_v4(), 1);
    client.insert_app_session(&session).await.unwrap();

    let rotated = client.refresh_app_session(session.id, project, hash(1), &next(2, 2)).await.unwrap();
    let within = client.refresh_app_session(session.id, project, hash(1), &next(3, 2)).await.unwrap();
    let after_grace_check = client.app_session(session.id).await.unwrap().unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(2100)).await;
    let reused = client.refresh_app_session(session.id, project, hash(1), &next(4, 2)).await.unwrap();
    let current = client.refresh_app_session(session.id, project, hash(2), &next(5, 2)).await.unwrap();
    cleanup_project(&client, project, &[]).await;

    let RefreshOutcome::Rotated(r) = rotated else { panic!("{rotated:?}") };
    assert_eq!(r.id, session.id);
    assert!(r.expires_at < chrono::Utc::now() + chrono::Duration::hours(2), "sliding applied");
    let RefreshOutcome::Grace { sealed, session: g } = within else { panic!("{within:?}") };
    assert_eq!(sealed, "sealed-2", "the grace hands out the token the rotation made");
    assert_eq!(g.revoked_at, None);
    assert_eq!(after_grace_check.revoked_at, None);
    let RefreshOutcome::Reused(u) = reused else { panic!("{reused:?}") };
    assert!(u.revoked_at.is_some());
    assert_eq!(current, RefreshOutcome::Invalid, "a revoked session refuses even its current token");
}

#[tokio::test]
#[ignore]
async fn refresh_expires_on_cap() {
    let client = client().await;
    let project = new_project(&client, "cap").await;
    let capped = new_session(project, uuid::Uuid::new_v4(), 1);
    let expired = new_session(project, uuid::Uuid::new_v4(), 2);
    client.insert_app_session(&capped).await.unwrap();
    client.insert_app_session(&expired).await.unwrap();
    backdate_session(&client, capped.id, 2 * 86400).await;
    sqlx::query("UPDATE app_sessions SET expires_at = now() - interval '1 second' WHERE id = $1")
        .bind(expired.id)
        .execute(client.pool())
        .await
        .unwrap();
    let mut one_day = next(9, 2);
    one_day.cap = std::time::Duration::from_secs(86400);

    let a = client.refresh_app_session(capped.id, project, hash(1), &one_day).await.unwrap();
    let b = client.refresh_app_session(expired.id, project, hash(2), &one_day).await.unwrap();
    let a_row = client.app_session(capped.id).await.unwrap().unwrap();
    let b_row = client.app_session(expired.id).await.unwrap().unwrap();
    cleanup_project(&client, project, &[]).await;
    assert_eq!((a, b), (RefreshOutcome::Invalid, RefreshOutcome::Invalid));
    assert!(a_row.revoked_at.is_some() && b_row.revoked_at.is_some());
}

#[tokio::test]
#[ignore]
async fn unknown_secret_is_invalid_without_revoking() {
    let client = client().await;
    let project = new_project(&client, "unknown").await;
    let session = new_session(project, uuid::Uuid::new_v4(), 1);
    client.insert_app_session(&session).await.unwrap();
    let wrong = client.refresh_app_session(session.id, project, hash(77), &next(2, 2)).await.unwrap();
    let missing = client
        .refresh_app_session(uuid::Uuid::new_v4(), project, hash(1), &next(2, 2))
        .await
        .unwrap();
    let row = client.app_session(session.id).await.unwrap().unwrap();
    // Nothing changed: the real token still rotates.
    let rotated = client.refresh_app_session(session.id, project, hash(1), &next(2, 2)).await.unwrap();
    cleanup_project(&client, project, &[]).await;
    assert_eq!((wrong, missing), (RefreshOutcome::Invalid, RefreshOutcome::Invalid));
    assert_eq!(row.revoked_at, None);
    assert!(matches!(rotated, RefreshOutcome::Rotated(_)));
}

#[tokio::test]
#[ignore]
async fn revoke_by_hash_needs_a_matching_token() {
    let client = client().await;
    let project = new_project(&client, "by-hash").await;
    let (a, b) = (new_session(project, uuid::Uuid::new_v4(), 1), new_session(project, uuid::Uuid::new_v4(), 2));
    client.insert_app_session(&a).await.unwrap();
    client.insert_app_session(&b).await.unwrap();
    // `b` rotated once: both its tokens match.
    client.refresh_app_session(b.id, project, hash(2), &next(3, 2)).await.unwrap();

    let wrong = client.revoke_app_session_by_hash(a.id, project, hash(9)).await.unwrap();
    let live = client.app_session(a.id).await.unwrap().unwrap().revoked_at;
    let right = client.revoke_app_session_by_hash(a.id, project, hash(1)).await.unwrap();
    let twice = client.revoke_app_session_by_hash(a.id, project, hash(1)).await.unwrap();
    let previous = client.revoke_app_session_by_hash(b.id, project, hash(2)).await.unwrap();
    cleanup_project(&client, project, &[]).await;
    assert!(wrong.is_none());
    assert_eq!(live, None);
    assert!(right.is_some_and(|s| s.revoked_at.is_some()));
    assert!(twice.is_none(), "already revoked");
    assert!(previous.is_some(), "the previous token matches too");
}

#[tokio::test]
#[ignore]
async fn session_functions_refuse_another_project() {
    let client = client().await;
    let (mine, other) = (new_project(&client, "mine").await, new_project(&client, "other").await);
    let session = new_session(mine, uuid::Uuid::new_v4(), 1);
    client.insert_app_session(&session).await.unwrap();

    let refreshed = client.refresh_app_session(session.id, other, hash(1), &next(2, 2)).await.unwrap();
    let revoked = client.revoke_app_session(session.id, other).await.unwrap();
    let by_hash = client.revoke_app_session_by_hash(session.id, other, hash(1)).await.unwrap();
    let user_wide = client.revoke_user_sessions(other, session.user_id, None, None).await.unwrap();
    let project_wide = client.revoke_project_sessions(other).await.unwrap();
    let row = client.app_session(session.id).await.unwrap().unwrap();
    // The current token is unchanged: it still rotates in the right project.
    let rotated = client.refresh_app_session(session.id, mine, hash(1), &next(2, 2)).await.unwrap();
    cleanup_project(&client, mine, &[]).await;
    cleanup_project(&client, other, &[]).await;
    assert_eq!(refreshed, RefreshOutcome::Invalid);
    assert!(revoked.is_none() && by_hash.is_none() && user_wide.is_empty() && project_wide.is_empty());
    assert_eq!(row.revoked_at, None);
    assert!(matches!(rotated, RefreshOutcome::Rotated(_)));
}

#[tokio::test]
#[ignore]
async fn revoke_user_sessions_keeps_and_filters_by_time() {
    let client = client().await;
    let project = new_project(&client, "user-sessions").await;
    let (user, other_user) = (uuid::Uuid::new_v4(), uuid::Uuid::new_v4());
    let (s1, s2, s3) = (new_session(project, user, 1), new_session(project, user, 2), new_session(project, user, 3));
    let theirs = new_session(project, other_user, 4);
    for s in [&s1, &s2, &s3, &theirs] {
        client.insert_app_session(s).await.unwrap();
    }
    backdate_session(&client, s1.id, 7200).await;

    let before = chrono::Utc::now() - chrono::Duration::hours(1);
    let old_only = client.revoke_user_sessions(project, user, None, Some(before)).await.unwrap();
    let listed = client.list_user_sessions(project, user).await.unwrap();
    let rest = client.revoke_user_sessions(project, user, Some(s3.id), None).await.unwrap();
    let kept = client.list_user_sessions(project, user).await.unwrap();
    let others = client.list_user_sessions(project, other_user).await.unwrap();
    let all = client.revoke_project_sessions(project).await.unwrap();
    cleanup_project(&client, project, &[]).await;
    assert_eq!(old_only.iter().map(|s| s.id).collect::<Vec<_>>(), vec![s1.id]);
    assert_eq!(listed.len(), 2);
    assert_eq!(rest.iter().map(|s| s.id).collect::<Vec<_>>(), vec![s2.id]);
    assert_eq!(kept.iter().map(|s| s.id).collect::<Vec<_>>(), vec![s3.id]);
    assert_eq!(others.len(), 1);
    assert_eq!(all.len(), 2, "s3 and the other user's session were still live");
}

#[tokio::test]
#[ignore]
async fn purge_app_sessions_drops_old_revoked_and_expired() {
    let client = client().await;
    let project = new_project(&client, "purge").await;
    let user = uuid::Uuid::new_v4();
    let (old_revoked, fresh_revoked, old_expired, live) =
        (new_session(project, user, 1), new_session(project, user, 2), new_session(project, user, 3), new_session(project, user, 4));
    for s in [&old_revoked, &fresh_revoked, &old_expired, &live] {
        client.insert_app_session(s).await.unwrap();
    }
    client.revoke_app_session(old_revoked.id, project).await.unwrap();
    client.revoke_app_session(fresh_revoked.id, project).await.unwrap();
    sqlx::query("UPDATE app_sessions SET revoked_at = now() - interval '2 days' WHERE id = $1")
        .bind(old_revoked.id)
        .execute(client.pool())
        .await
        .unwrap();
    sqlx::query("UPDATE app_sessions SET expires_at = now() - interval '2 days' WHERE id = $1")
        .bind(old_expired.id)
        .execute(client.pool())
        .await
        .unwrap();
    client.purge_app_sessions(std::time::Duration::from_secs(86400)).await.unwrap();
    let left = [
        client.app_session(old_revoked.id).await.unwrap().is_some(),
        client.app_session(fresh_revoked.id).await.unwrap().is_some(),
        client.app_session(old_expired.id).await.unwrap().is_some(),
        client.app_session(live.id).await.unwrap().is_some(),
    ];
    cleanup_project(&client, project, &[]).await;
    assert_eq!(left, [false, true, false, true]);
}

#[tokio::test]
#[ignore]
async fn listers_find_stale_transitions_and_deleting_projects() {
    let _sealed = sealed_lock().await;
    let client = client().await;
    let (enabling, disabling, deleting) = (
        new_project(&client, "stale-enabling").await,
        new_project(&client, "stale-disabling").await,
        new_project(&client, "stale-deleting").await,
    );
    let (db1, db2) = (ready_database_in(&client, enabling).await, ready_database_in(&client, disabling).await);
    client.begin_enable_app_users(&new_auth(enabling, db1)).await.unwrap();
    client.begin_enable_app_users(&new_auth(disabling, db2)).await.unwrap();
    client.finish_enable_app_users(disabling, Ok(())).await.unwrap();
    client.begin_disable_app_users(disabling).await.unwrap();
    client.begin_project_delete(demo(), deleting).await.unwrap();
    let hour = std::time::Duration::from_secs(3600);
    let fresh_transitions = client.app_users_in_transition(hour).await.unwrap();
    let fresh_deleting = client.deleting_projects(hour).await.unwrap();
    sqlx::query("UPDATE project_auth SET updated_at = now() - interval '2 hours' WHERE project_id = ANY($1)")
        .bind([enabling.as_uuid(), disabling.as_uuid()].as_slice())
        .execute(client.pool())
        .await
        .unwrap();
    sqlx::query("UPDATE projects SET updated_at = now() - interval '2 hours' WHERE id = $1")
        .bind(deleting.as_uuid())
        .execute(client.pool())
        .await
        .unwrap();
    let stale: Vec<ProjectId> = client.app_users_in_transition(hour).await.unwrap().iter().map(|a| a.project_id).collect();
    let stale_deleting = client.deleting_projects(hour).await.unwrap();
    cleanup_project(&client, enabling, &[db1]).await;
    cleanup_project(&client, disabling, &[db2]).await;
    cleanup_project(&client, deleting, &[]).await;
    assert!(fresh_transitions.iter().all(|a| a.project_id != enabling && a.project_id != disabling));
    assert!(!fresh_deleting.contains(&deleting));
    assert!(stale.contains(&enabling) && stale.contains(&disabling));
    assert!(stale_deleting.contains(&deleting));
}

#[tokio::test]
#[ignore]
async fn key_lookup_returns_the_project() {
    let _sealed = sealed_lock().await;
    let client = client().await;
    let project = new_project(&client, "key").await;
    let db = ready_database_in(&client, project).await;
    let (key, _) = new_key(main_of(db), DataKeyKind::Secret, "service_role", "backend");
    client.insert_data_key(&key).await.unwrap();
    let found = client.lookup_data_key(&key.sha256).await.unwrap();
    cleanup_project(&client, project, &[db]).await;
    let found = found.expect("the key is live");
    assert_eq!(found.project_id, project);
    assert_eq!(found.branch_id, main_of(db));
}

#[tokio::test]
#[ignore]
async fn tenant_overview_hides_deleted_projects() {
    let client = client().await;
    let live = new_project(&client, "ov-live").await;
    let deleting = new_project(&client, "ov-deleting").await;
    let gone = new_project(&client, "ov-gone").await;
    client.begin_project_delete(demo(), deleting).await.unwrap();
    client.begin_project_delete(demo(), gone).await.unwrap();
    client.finish_project_delete(gone).await.unwrap();
    let overview = client.tenant_overview(demo()).await.unwrap().unwrap();
    let find = |id: ProjectId| overview.projects.iter().find(|p| p.id == id).map(|p| (p.name.clone(), p.status));
    let seed = overview.projects.iter().find(|p| p.id == ProjectId(DEMO_PROJECT_ID)).map(|p| p.status);
    let (l, d, g) = (find(live), find(deleting), find(gone));
    for p in [live, deleting, gone] {
        cleanup_project(&client, p, &[]).await;
    }
    assert_eq!(l.map(|(_, s)| s), Some(ProjectStatus::Active));
    assert_eq!(d.map(|(_, s)| s), Some(ProjectStatus::Deleting));
    assert!(g.is_none());
    assert_eq!(seed, Some(ProjectStatus::Active), "the seed project stays active");
}

// ---- saved queries

fn saved(write: SavedQueryWrite) -> SavedQuery {
    match write {
        SavedQueryWrite::Saved(q) => q,
        other => panic!("expected Saved, got {other:?}"),
    }
}

async fn create_saved(client: &MetadataClient, db: DatabaseId, user: &str, name: &str) -> SavedQueryWrite {
    client
        .create_saved_query(db.as_uuid(), user, uuid::Uuid::new_v4(), name, "select 1", None, None)
        .await
        .unwrap()
}

#[tokio::test]
#[ignore]
async fn saved_query_round_trip() {
    let client = client().await;
    let db = insert_pending(&client).await;
    let d = db.as_uuid();
    let id = uuid::Uuid::new_v4();
    let created = saved(
        client.create_saved_query(d, "u1", id, "Beta", "select 2", Some("app"), Some("anon")).await.unwrap(),
    );
    saved(create_saved(&client, db, "u1", "alpha").await);
    assert_eq!((created.version, created.pg_database.as_deref(), created.role.as_deref()), (1, Some("app"), Some("anon")));

    let list = client.list_saved_queries(d, "u1").await.unwrap();
    assert_eq!(list.iter().map(|q| q.name.as_str()).collect::<Vec<_>>(), ["alpha", "Beta"]);
    assert_eq!(client.get_saved_query(d, "u1", id).await.unwrap(), Some(created.clone()));
    assert_eq!(client.get_saved_query(d, "u2", id).await.unwrap(), None);
    assert!(client.list_saved_queries(d, "u2").await.unwrap().is_empty());

    // null clears a hint, an absent field stays.
    let patch = SavedQueryPatch { sql: Some("select 3".into()), role: Some(None), ..Default::default() };
    let updated = saved(client.update_saved_query(d, "u1", id, 1, &patch).await.unwrap());
    assert_eq!((updated.version, updated.sql.as_str(), updated.role.clone()), (2, "select 3", None));
    assert_eq!((updated.pg_database.as_deref(), updated.name.as_str()), (Some("app"), "Beta"));
    assert!(updated.updated_at >= created.updated_at);

    assert!(!client.delete_saved_query(d, "u2", id).await.unwrap());
    assert!(client.delete_saved_query(d, "u1", id).await.unwrap());
    assert!(!client.delete_saved_query(d, "u1", id).await.unwrap());
    cleanup(&client, db).await;
}

#[tokio::test]
#[ignore]
async fn saved_query_names_are_unique_per_user_case_insensitive() {
    let client = client().await;
    let db = insert_pending(&client).await;
    saved(create_saved(&client, db, "u1", "Report").await);
    assert_eq!(create_saved(&client, db, "u1", "REPORT").await, SavedQueryWrite::NameTaken);
    saved(create_saved(&client, db, "u2", "report").await);
    let other = saved(create_saved(&client, db, "u1", "other").await);
    let patch = SavedQueryPatch { name: Some("rePort".into()), ..Default::default() };
    let renamed = client.update_saved_query(db.as_uuid(), "u1", other.id, 1, &patch).await.unwrap();
    assert_eq!(renamed, SavedQueryWrite::NameTaken);
    cleanup(&client, db).await;
}

#[tokio::test]
#[ignore]
async fn saved_query_update_detects_stale_versions() {
    let client = client().await;
    let db = insert_pending(&client).await;
    let d = db.as_uuid();
    let q = saved(create_saved(&client, db, "u1", "q").await);
    let patch = SavedQueryPatch { sql: Some("select 2".into()), ..Default::default() };
    let first = saved(client.update_saved_query(d, "u1", q.id, 1, &patch).await.unwrap());
    let again = client.update_saved_query(d, "u1", q.id, 1, &patch).await.unwrap();
    assert_eq!(again, SavedQueryWrite::Stale(first));
    let missing = client.update_saved_query(d, "u1", uuid::Uuid::new_v4(), 1, &patch).await.unwrap();
    assert_eq!(missing, SavedQueryWrite::NotFound);
    let foreign = client.update_saved_query(d, "u2", q.id, 2, &patch).await.unwrap();
    assert_eq!(foreign, SavedQueryWrite::NotFound);
    cleanup(&client, db).await;
}

#[tokio::test]
#[ignore]
async fn saved_query_cap_is_enforced() {
    let client = client().await;
    let db = insert_pending(&client).await;
    sqlx::query(
        "INSERT INTO saved_queries (id, database_id, user_id, name, sql)
         SELECT gen_random_uuid(), $1, 'u1', 'q' || n, 'select 1' FROM generate_series(1, $2) n",
    )
    .bind(db.as_uuid())
    .bind(SAVED_QUERY_CAP as i32)
    .execute(client.pool())
    .await
    .unwrap();
    assert_eq!(create_saved(&client, db, "u1", "one more").await, SavedQueryWrite::CapReached);
    // The cap is per user.
    saved(create_saved(&client, db, "u2", "mine").await);
    cleanup(&client, db).await;
}

#[tokio::test]
#[ignore]
async fn saved_queries_cascade_with_their_database() {
    let client = client().await;
    let db = insert_pending(&client).await;
    saved(create_saved(&client, db, "u1", "q").await);
    cleanup(&client, db).await;
    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM saved_queries WHERE database_id = $1")
        .bind(db.as_uuid())
        .fetch_one(client.pool())
        .await
        .unwrap();
    assert_eq!(left, 0);
}
