//! Opt-in tests against a real metadata Postgres (the workbench stack first):
//!
//! ```sh
//! cargo test -p chelabase-metadata --test live_db -- --ignored --test-threads=1
//! ```
//!
//! They use the seeded demo tenant/project and delete the rows they create. Every run names its
//! databases `live-test-<run id>-<id>` and sweeps only those, so runs can overlap. Rows a crashed
//! run leaves survive for up to an hour (`STALE_AFTER`); `just sweep-dbs` is the manual remedy.

use chelabase_metadata::{
    BranchDeleteOutcome, BranchKind, BranchRecord, BranchStatus, CreateBranchOutcome, DatabaseSettings, DatabaseStatus,
    BeginDelete, ClaimOutcome, CopyUpsert, DatabaseOrigin, InsertRoleOutcome, MetadataClient, MetadataConfig,
    KeyRoleFix, MetadataError, NewBranch, NewDataKey, NewEndpoint, NewRoleCopy, ObservedDatabase, PasswordState, ResyncApplied,
    ResyncWrite, RoleLevel, RoleOrigin, SealedChange, SettingsPatch,
    DataApiState, EnableStart, ServedDatabase,
    BeginEnable, BeginProjectDelete, NewAppSession, NewProjectAuth, OnDelete, ProjectStatus, RefreshNext,
    PeekOutcome, RefreshOutcome, RenameProject, AppUsersStatus,
    SAVED_QUERY_CAP, SavedQuery, SavedQueryEditor, SavedQueryPatch, SavedQueryWrite,
    BaseSource, BranchBusy, MergeCreate, MergeDirection, MergeDraftPatch, MergeEvent, MergeState, MergeStatus, MergeStatusPatch,
    MergeWrite, NewMerge, NewStaticEndpoint, SchemaBase,
    DEMO_PROJECT_ID, DEMO_TENANT_ID,
};
use chelabase_metadata::{
    ApiRoleChange, NewApiRole, RenameCopyOutcome, RoleAttributes, RoleWriteOutcome, SecretChange,
};
use chelabase_metadata::members::Admission;
use chelabase_types::data_key::{self, DataKeyKind};
use chelabase_types::{BranchId, DatabaseId, ProjectId, TenantId};

async fn client() -> MetadataClient {
    let database_url = std::env::var("CHELABASE_METADATA_DATABASE_URL").unwrap_or_else(|_| {
        "postgres://chelabase:chelabase@metadata-db:5432/chelabase_metadata".to_string()
    });
    let client = MetadataClient::connect(&MetadataConfig { database_url })
        .await
        .expect("metadata Postgres reachable (is the workbench stack up?)");
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
        routable: true,
        static_lsn: None,
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
    let database_url = std::env::var("CHELABASE_METADATA_DATABASE_URL").unwrap_or_else(|_| {
        "postgres://chelabase:chelabase@metadata-db:5432/chelabase_metadata".to_string()
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

/// The stored key-check value. Every key-check test runs on a [`KeyCheckScratch`] database,
/// never the stack's own: a test that panicked half-way through rewriting the shared row would
/// leave the stack's services refusing to start.
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
    use chelabase_metadata::KeyCheck;
    let _lock = key_check_lock().await;
    let scratch = KeyCheckScratch::new().await;
    let client = &scratch.client;

    let first = client.ensure_key_check("check-a", |v| v == "check-a").await.unwrap();
    let same = client.ensure_key_check("check-a2", |v| v == "check-a").await.unwrap();

    // Stored credentials exist (on a branch now): a different key must be refused.
    let id = insert_pending(client).await;
    sqlx::query("UPDATE branches SET owner_password_enc = 'v1:x' WHERE id = $1")
        .bind(id.as_uuid())
        .execute(client.pool())
        .await
        .unwrap();
    let other = client.ensure_key_check("check-b", |v| v == "check-b").await.unwrap();
    scratch.finish().await;

    assert_eq!(first, KeyCheck::Recorded);
    assert_eq!(same, KeyCheck::Matches);
    assert!(matches!(other, KeyCheck::Mismatch { sealed_rows } if sealed_rows >= 1));
}

#[tokio::test]
#[ignore]
async fn key_check_adopts_a_new_key_when_nothing_is_encrypted_yet() {
    use chelabase_metadata::KeyCheck;
    let _lock = key_check_lock().await;
    let scratch = KeyCheckScratch::new().await;
    let client = &scratch.client;
    let empty = sealed_values(client).await;
    client.ensure_key_check("check-a", |v| v == "check-a").await.unwrap();
    let replaced = client.ensure_key_check("check-b", |v| v == "check-b").await.unwrap();
    let again = client.ensure_key_check("check-b2", |v| v == "check-b").await.unwrap();
    scratch.finish().await;
    assert_eq!(empty, 0, "a fresh metadata database holds no sealed values");
    assert_eq!(replaced, KeyCheck::Replaced);
    assert_eq!(again, KeyCheck::Matches);
}

/// Sealed values in every column the key check covers, counted with the check's own statement.
async fn sealed_values(client: &MetadataClient) -> i64 {
    // Constant identifiers and conditions from the list.
    sqlx::query_scalar(sqlx::AssertSqlSafe(chelabase_metadata::sealed_count_sql()))
        .fetch_one(client.pool())
        .await
        .unwrap()
}

/// A scratch metadata database (`cm_<12 hex>`, the pattern `members_live` sweeps when stale)
/// with every migration applied, demo tenant and project included. The tests that need no
/// sealed value anywhere run here: on the shared stack other live binaries write sealed columns
/// without [`sealed_lock`], so the stack's count is never reliably zero.
struct KeyCheckScratch {
    meta: sqlx::PgPool,
    name: String,
    client: MetadataClient,
}

impl KeyCheckScratch {
    async fn new() -> Self {
        let base = std::env::var("CHELABASE_METADATA_DATABASE_URL").unwrap_or_else(|_| {
            "postgres://chelabase:chelabase@metadata-db:5432/chelabase_metadata".to_string()
        });
        let meta = raw_pool().await;
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let name = format!("cm_{:08x}{}", secs as u32, &hex32()[..4]);
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!("CREATE DATABASE {name}")))
            .execute(&meta)
            .await
            .unwrap();
        let (head, tail) = base.rsplit_once('/').expect("a database in the URL");
        let query = tail.split_once('?').map(|(_, q)| format!("?{q}")).unwrap_or_default();
        let client = MetadataClient::connect(&MetadataConfig { database_url: format!("{head}/{name}{query}") })
            .await
            .unwrap();
        client.migrate().await.unwrap();
        KeyCheckScratch { meta, name, client }
    }

    async fn finish(self) {
        self.client.pool().close().await;
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!("DROP DATABASE {} WITH (FORCE)", self.name)))
            .execute(&self.meta)
            .await
            .unwrap();
    }
}

/// An app session in the demo project holding a sealed grace token whose window ends
/// `grace_secs` from now (negative: already over); returns its id.
async fn insert_grace_session(client: &MetadataClient, grace_secs: f64) -> uuid::Uuid {
    let session = uuid::Uuid::new_v4();
    sqlx::query(
        "INSERT INTO app_sessions (id, project_id, user_id, zitadel_session_id, refresh_hash,
                                   grace_refresh_sealed, grace_until, expires_at)
         VALUES ($1, $2, gen_random_uuid(), 'key-check', decode('00', 'hex'), 'v1:x',
                 now() + make_interval(secs => $3), now() + interval '1 hour')",
    )
    .bind(session)
    .bind(DEMO_PROJECT_ID)
    .bind(grace_secs)
    .execute(client.pool())
    .await
    .unwrap();
    session
}

/// The parent rows one sealed fixture needs, deleted by [`SealedFixture::remove`].
struct SealedFixture {
    database: DatabaseId,
    member: Option<uuid::Uuid>,
    session: Option<uuid::Uuid>,
}

impl SealedFixture {
    /// A fresh database (main branch and endpoint) with one sealed value in `table.column` and
    /// none in any other listed column.
    async fn new(client: &MetadataClient, table: &str, column: &str) -> Self {
        let database = insert_pending(client).await;
        let endpoint = link_endpoint(client, database).await;
        let branch = main_of(database).as_uuid();
        let mut fixture = SealedFixture { database, member: None, session: None };
        let pool = client.pool();
        match (table, column) {
            ("branches", "owner_password_enc" | "authenticator_password_enc") => {
                sqlx::query(sqlx::AssertSqlSafe(format!(
                    "UPDATE branches SET {column} = 'v1:x' WHERE id = $1"
                )))
                    .bind(branch)
                    .execute(pool)
                    .await
                    .unwrap();
            }
            // The storage login's password always has its salt beside it.
            ("branches", "storage_password_enc") => {
                sqlx::query(
                    "UPDATE branches SET storage_password_enc = 'v1:x', storage_password_salt = '\\x00000000000000000000000000000000' WHERE id = $1",
                )
                .bind(branch)
                .execute(pool)
                .await
                .unwrap();
            }
            ("data_api_keys", "sealed") => {
                sqlx::query(
                    "INSERT INTO data_api_keys (id, branch_id, kind, role, name, prefix, sha256, sealed)
                     VALUES (gen_random_uuid(), $1, 'public', 'anon', 'key-check', 'p',
                             sha256(gen_random_uuid()::text::bytea), 'v1:x')",
                )
                .bind(branch)
                .execute(pool)
                .await
                .unwrap();
            }
            ("neon_roles", "password_enc") => {
                sqlx::query(
                    "INSERT INTO neon_roles (endpoint_id, name, origin, password_enc)
                     VALUES ($1, 'key_check_role', 'sql', 'v1:x')",
                )
                .bind(&endpoint)
                .execute(pool)
                .await
                .unwrap();
            }
            ("member_branch_roles", "password_enc") => {
                let member = uuid::Uuid::new_v4();
                sqlx::query(
                    "INSERT INTO members (id, tenant_id, zitadel_user_id, status)
                     VALUES ($1, $2, $3, 'active')",
                )
                .bind(member)
                .bind(DEMO_TENANT_ID)
                .bind(format!("live-test-key-check-{member}"))
                .execute(pool)
                .await
                .unwrap();
                fixture.member = Some(member);
                sqlx::query(
                    "INSERT INTO member_branch_roles (member_id, branch_id, role_name, password_enc)
                     VALUES ($1, $2, 'key_check_member', 'v1:x')",
                )
                .bind(member)
                .bind(branch)
                .execute(pool)
                .await
                .unwrap();
            }
            ("branch_auth_sync", "password_sealed") => {
                sqlx::query(
                    "INSERT INTO branch_auth_sync (branch_id, pg_database, password_sealed)
                     VALUES ($1, 'db', 'v1:x')",
                )
                .bind(branch)
                .execute(pool)
                .await
                .unwrap();
            }
            ("app_sessions", "grace_refresh_sealed") => {
                // Within its grace window: it can still be opened.
                fixture.session = Some(insert_grace_session(client, 3600.0).await);
            }
            other => panic!("no fixture for sealed column {other:?}: add one here"),
        }
        fixture
    }

    async fn remove(self, client: &MetadataClient) {
        if let Some(session) = self.session {
            sqlx::query("DELETE FROM app_sessions WHERE id = $1")
                .bind(session)
                .execute(client.pool())
                .await
                .unwrap();
        }
        // Branch children (keys, roles, member roles, the sync row) cascade with the branch.
        cleanup(client, self.database).await;
        if let Some(member) = self.member {
            sqlx::query("DELETE FROM members WHERE id = $1")
                .bind(member)
                .execute(client.pool())
                .await
                .unwrap();
        }
    }
}

// #38: a wrong key is refused whichever listed column holds the only sealed value.
#[tokio::test]
#[ignore]
async fn key_check_refuses_when_only_one_column_is_sealed() {
    use chelabase_metadata::KeyCheck;
    let _lock = key_check_lock().await;
    let scratch = KeyCheckScratch::new().await;
    let client = &scratch.client;
    let mut wrong = Vec::new();
    for &(table, column, _) in chelabase_metadata::SECRETS_KEY_SEALED {
        // Other live binaries seal values without `sealed_lock`, so the count can move under
        // a check. A wrong count is retried while the environment moved (or the check lost a
        // deadlock to a writer), and fails once it is wrong with the environment stable.
        for attempt in 1..=KEY_CHECK_ATTEMPTS {
            let before = sealed_values(client).await;
            let fixture = SealedFixture::new(client, table, column).await;
            restore_key_check(client, Some("check-a".into())).await;
            let outcome = client.ensure_key_check("check-b", |v| v == "check-b").await;
            let after = sealed_values(client).await;
            let stored = saved_key_check(client).await;
            fixture.remove(client).await;
            let report = format!(
                "{table}.{column}: {outcome:?} ({before} before, {after} after), stored check {stored:?}"
            );
            // With a sealed value present, nothing the environment does may adopt the key.
            if stored.as_deref() != Some("check-a") {
                wrong.push(report);
                break;
            }
            // Exact: a value already in the environment must not hide a missed column.
            if matches!(outcome, Ok(KeyCheck::Mismatch { sealed_rows }) if sealed_rows == before + 1) {
                break;
            }
            let moved = after != before + 1 || outcome.is_err();
            if !moved || attempt == KEY_CHECK_ATTEMPTS {
                wrong.push(report);
                break;
            }
            eprintln!("retrying {table}.{column}: the environment moved ({report})");
        }
    }
    scratch.finish().await;
    assert!(wrong.is_empty(), "a wrong key was not refused: {wrong:#?}");
}

/// Tries per column before a count the environment kept moving counts as wrong.
const KEY_CHECK_ATTEMPTS: u32 = 5;

// #38: the key is adopted only once no listed column holds a sealed value; the refusal counts
// them all.
#[tokio::test]
#[ignore]
async fn key_check_adopts_only_with_no_sealed_values() {
    use chelabase_metadata::KeyCheck;
    let _lock = key_check_lock().await;
    let scratch = KeyCheckScratch::new().await;
    let client = &scratch.client;
    let empty = sealed_values(client).await;
    let mut fixtures = Vec::new();
    for &(table, column, _) in chelabase_metadata::SECRETS_KEY_SEALED {
        fixtures.push(SealedFixture::new(client, table, column).await);
    }
    restore_key_check(client, Some("check-a".into())).await;
    let mut outcomes = Vec::new();
    while let Some(fixture) = fixtures.pop() {
        let outcome = client.ensure_key_check("check-b", |v| v == "check-b").await.unwrap();
        outcomes.push((fixtures.len() as i64 + 1, outcome, saved_key_check(client).await));
        fixture.remove(client).await;
    }
    let adopted = client.ensure_key_check("check-b", |v| v == "check-b").await.unwrap();
    let after = saved_key_check(client).await;
    scratch.finish().await;
    assert_eq!(empty, 0, "a fresh metadata database holds no sealed values");
    for (remaining, outcome, stored) in outcomes {
        assert_eq!(outcome, KeyCheck::Mismatch { sealed_rows: remaining }, "{remaining} sealed values left");
        assert_eq!(stored.as_deref(), Some("check-a"), "replaced with {remaining} sealed values left");
    }
    assert_eq!(adopted, KeyCheck::Replaced);
    assert_eq!(after.as_deref(), Some("check-b"));
}

// #38 audit: a grace token past its window can never be opened again, so it doesn't hold a
// new key off.
#[tokio::test]
#[ignore]
async fn key_check_ignores_an_expired_grace_token() {
    use chelabase_metadata::KeyCheck;
    let _lock = key_check_lock().await;
    let scratch = KeyCheckScratch::new().await;
    let client = &scratch.client;
    let empty = sealed_values(client).await;
    insert_grace_session(client, -60.0).await;
    restore_key_check(client, Some("check-a".into())).await;
    let outcome = client.ensure_key_check("check-b", |v| v == "check-b").await;
    let stored = saved_key_check(client).await;
    scratch.finish().await;
    assert_eq!(empty, 0, "a fresh metadata database holds no sealed values");
    assert_eq!(outcome.unwrap(), KeyCheck::Replaced);
    assert_eq!(stored.as_deref(), Some("check-b"));
}

// #38 Review Focus: a sealed write still in flight when the key check counts must not be
// missed (the check waits for it, then refuses).
#[tokio::test]
#[ignore]
async fn key_check_waits_for_an_inflight_sealed_write() {
    use chelabase_metadata::KeyCheck;
    let _lock = key_check_lock().await;
    let scratch = KeyCheckScratch::new().await;
    let client = &scratch.client;
    let database = insert_pending(client).await;
    restore_key_check(client, Some("check-a".into())).await;
    let (finished_early, outcome, stored) = {
        let writer = client.pool().clone();
        let mut tx = writer.begin().await.unwrap();
        sqlx::query("UPDATE branches SET authenticator_password_enc = 'v1:x' WHERE id = $1")
            .bind(main_of(database).as_uuid())
            .execute(&mut *tx)
            .await
            .unwrap();
        let check = client.ensure_key_check("check-b", |v| v == "check-b");
        tokio::pin!(check);
        let early = tokio::time::timeout(std::time::Duration::from_millis(500), &mut check).await;
        let (finished_early, outcome) = match early {
            Ok(outcome) => {
                tx.commit().await.unwrap();
                (true, outcome)
            }
            Err(_) => {
                tx.commit().await.unwrap();
                (false, check.await)
            }
        };
        let stored = saved_key_check(client).await;
        (finished_early, outcome, stored)
    };
    cleanup(client, database).await;
    scratch.finish().await;
    assert!(!finished_early, "the check finished while a sealed write was uncommitted: {outcome:?}");
    assert!(matches!(outcome, Ok(KeyCheck::Mismatch { sealed_rows }) if sealed_rows >= 1), "{outcome:?}");
    assert_eq!(stored.as_deref(), Some("check-a"));
}

/// Metadata columns named like sealed ones that don't hold a secrets-key value, each with why.
const NOT_SECRETS_KEY_SEALED: &[(&str, &str, &str)] = &[];

// #38: a new sealed column must join the key check (or be excluded here, with a reason).
#[tokio::test]
#[ignore]
async fn every_sealed_column_is_checked() {
    let client = client().await;
    let columns: Vec<(String, String)> = sqlx::query_as(
        r"SELECT table_name::text, column_name::text FROM information_schema.columns
          WHERE table_schema = current_schema()
            AND (column_name LIKE '%\_enc' OR column_name LIKE '%sealed%')
          ORDER BY 1, 2",
    )
    .fetch_all(client.pool())
    .await
    .unwrap();
    assert!(!columns.is_empty(), "the pattern matched no column: is this the metadata schema?");
    let missing: Vec<String> = columns
        .iter()
        .filter(|(t, c)| {
            !chelabase_metadata::SECRETS_KEY_SEALED.iter().any(|(lt, lc, _)| (*lt, *lc) == (t.as_str(), c.as_str()))
                && !NOT_SECRETS_KEY_SEALED.iter().any(|(et, ec, _)| (*et, *ec) == (t.as_str(), c.as_str()))
        })
        .map(|(t, c)| format!("{t}.{c}"))
        .collect();
    assert!(
        missing.is_empty(),
        "sealed-looking columns neither in SECRETS_KEY_SEALED nor excluded: {missing:?}"
    );
    // The list names only columns that exist.
    for (table, column, _) in chelabase_metadata::SECRETS_KEY_SEALED {
        assert!(
            columns.iter().any(|(t, c)| (t.as_str(), c.as_str()) == (*table, *column)),
            "SECRETS_KEY_SEALED names {table}.{column}, which doesn't exist"
        );
    }
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
    let database_url = std::env::var("CHELABASE_METADATA_DATABASE_URL").unwrap_or_else(|_| {
        "postgres://chelabase:chelabase@metadata-db:5432/chelabase_metadata".to_string()
    });
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&database_url)
        .await
        .expect("metadata Postgres reachable (is the workbench stack up?)")
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
        .switch_to_restored_timeline(dev, &pending, backup, "dev-before-restore-20260930T120000Z", "0/1")
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
    let while_ready = client.switch_to_pending_timeline(main, &pending, None, "0/1").await.unwrap();
    let untouched = main_branch(&client, id).await;

    assert!(client
        .set_branch_status(main, &[BranchStatus::Ready], BranchStatus::Resetting)
        .await
        .unwrap());
    let switched = client.switch_to_pending_timeline(main, &pending, None, "0/1").await.unwrap();
    let after = main_branch(&client, id).await;
    let endpoint = client.neon_endpoint(&ep).await.unwrap().unwrap();
    // Nothing pending any more: a second switch is refused.
    let again = client.switch_to_pending_timeline(main, &pending, None, "0/1").await.unwrap();
    client.clear_superseded_timeline(main).await.unwrap();
    let cleared = main_branch(&client, id).await;

    // Deleting: refused even with a pending timeline.
    let (gone, _ep) = ready_database(&client).await;
    let gone_pending = start_reset(&client, main_of(gone)).await;
    assert!(begin_delete(&client, gone).await);
    let while_deleting = client
        .switch_to_pending_timeline(main_of(gone), &gone_pending, None, "0/1")
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
        .switch_to_restored_timeline(main, &hex32(), refused_backup, "main-before-restore-20260930T110000Z", "0/1")
        .await
        .unwrap();
    let no_backup = client.get_branch(refused_backup).await.unwrap();

    let pending = start_reset(&client, main).await;
    let backup = BranchId::new();
    let name = "main-before-restore-20260930T120000Z";
    let switched = client.switch_to_restored_timeline(main, &pending, backup, name, "0/1").await.unwrap();
    let after = main_branch(&client, id).await;
    let backup_row = branch(&client, backup).await;
    let endpoint = client.neon_endpoint(&ep).await.unwrap().unwrap();

    // The backup can't be written (its name is taken): nothing else is either.
    let second = hex32();
    assert!(client.set_pending_timeline(main, Some(&second)).await.unwrap());
    let before_failed = main_branch(&client, id).await;
    let failed = client.switch_to_restored_timeline(main, &second, BranchId::new(), name, "0/1").await;
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
        matches!(&failed, Err(chelabase_metadata::MetadataError::BranchNameTaken(n)) if n == name),
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
    let reset = client.switch_to_pending_timeline(main, &mine, None, "0/1").await.unwrap();
    let backup = BranchId::new();
    let restored = client
        .switch_to_restored_timeline(main, &mine, backup, "main-before-restore-20260930T130000Z", "0/1")
        .await
        .unwrap();
    let after = main_branch(&client, id).await;
    let endpoint_after = client.neon_endpoint(&ep).await.unwrap().unwrap();
    let no_backup = client.get_branch(backup).await.unwrap();
    // The run that owns it still can.
    let owner = client.switch_to_pending_timeline(main, &theirs, None, "0/1").await.unwrap();
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
        .switch_to_restored_timeline(dev, &pending, backup, "dev-before-restore-20260930T120000Z", "0/1")
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

fn is_deadlock(err: &chelabase_metadata::MetadataError) -> bool {
    matches!(err, chelabase_metadata::MetadataError::Database(sqlx::Error::Database(e))
        if e.code().as_deref() == Some(DEADLOCK))
}

/// Runs `begin_delete`'s statements by hand, pausing between them while `racer` (a write on
/// main) starts: the database row is held first, as every transaction must take it.
async fn delete_holding_the_database_row<F, Fut>(client: &MetadataClient, id: DatabaseId, racer: F) -> (Result<(), sqlx::Error>, Result<bool, chelabase_metadata::MetadataError>)
where
    F: FnOnce(MetadataClient) -> Fut,
    Fut: std::future::Future<Output = Result<bool, chelabase_metadata::MetadataError>> + Send + 'static,
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
    use chelabase_secrets::{open_credential, seal_credential, Purpose, SecretKey};
    use sqlx::Row;

    let database_url = std::env::var("CHELABASE_METADATA_DATABASE_URL").unwrap_or_else(|_| {
        "postgres://chelabase:chelabase@metadata-db:5432/chelabase_metadata".to_string()
    });
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&database_url)
        .await
        .expect("metadata Postgres reachable (is the workbench stack up?)");
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
        .switch_to_restored_timeline(dev, &pending, backup, "dev-before-restore-20260930T120000Z", "0/1")
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
        .switch_to_restored_timeline(b.id, &pending, backup, "dev-before-restore-20260930T120000Z", "0/1")
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
    assert!(client.switch_to_pending_timeline(b.id, &pending, None, "0/1").await.unwrap());
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
            role_oid: None,
            role_timeline: None,
            role_bypass_rls: false,
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

/// M11 (step 6g T13): a merge's rehearsal is built like a branch, so its data API bootstrap
/// gives it a public key; the gateway's lookup must never resolve it (found live: the gateway
/// served a `_rehearsal-*` branch with it).
#[tokio::test]
#[ignore]
async fn a_rehearsal_branchs_key_is_not_looked_up() {
    let client = client().await;
    let (id, _) = ready_database(&client).await;
    let rehearsal = client
        .create_rehearsal_branch(id, main_of(id), "_rehearsal-0badc0de", "live_db")
        .await
        .unwrap();
    sqlx::query("UPDATE branches SET status = 'ready' WHERE id = $1")
        .bind(rehearsal.id.as_uuid())
        .execute(client.pool())
        .await
        .unwrap();
    let (key, _) = new_key(rehearsal.id, DataKeyKind::Public, "anon", "public");
    client.insert_data_key(&key).await.unwrap();
    let (main_key, _) = new_key(main_of(id), DataKeyKind::Secret, "authenticated", "backend");
    client.insert_data_key(&main_key).await.unwrap();
    let found = client.lookup_data_key(&key.sha256).await.unwrap();
    let main_found = client.lookup_data_key(&main_key.sha256).await.unwrap();
    cleanup(&client, id).await;
    assert!(found.is_none(), "the rehearsal's key was looked up: {found:?}");
    assert!(main_found.is_some(), "a normal branch's key still is");
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
    let (id, ep) = ready_database(&client).await;
    client.insert_api_role(&ep, "service_role", true, "SCRAM-SHA-256$o", "v1:sr", 10).await.unwrap();
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

    // `rotate_data_key` is for public keys: a secret `old` is refused, nothing written.
    let (clash, _) = new_key(branch, DataKeyKind::Public, "anon", "clash");
    let failed = client.rotate_data_key(branch, secret.id, &clash).await;
    let clash_inserted = client.lookup_data_key(&clash.sha256).await.unwrap();
    let secret_still_live = client.lookup_data_key(&secret.sha256).await.unwrap();

    // A failing insert rolls the revoke back: a duplicate hash.
    let (dup, _) = new_key(branch, DataKeyKind::Secret, "service_role", "dup");
    let dup = NewDataKey { sha256: secret.sha256, ..dup };
    let failed_dup = client.rotate_secret_key(branch, secret.id, &dup).await;
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
    assert!(matches!(failed, Ok(false)), "{failed:?}");
    assert!(clash_inserted.is_none());
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
        SettingsPatch { storage_max_connections: Some(0), ..Default::default() },
        SettingsPatch { storage_max_connections: Some(21), ..Default::default() },
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
        storage_max_connections: Some(20),
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
    assert_eq!(defaults.storage_max_connections, 2);
    assert_eq!((after.storage_max_connections, partial.storage_max_connections), (20, 20));
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
    client
        .update_database_settings(id, &SettingsPatch { storage_max_connections: Some(7), ..Default::default() })
        .await
        .unwrap();
    // The storage login's password and salt are stored together, once.
    client.set_storage_password(main_of(id), "v1:storage", &[7; 16]).await.unwrap();
    client.set_storage_password(main_of(id), "v1:later", &[8; 16]).await.unwrap();
    let with_storage = client.branch_connection(main_of(id)).await.unwrap().expect("row");
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
    assert_eq!(ready.settings.storage_max_connections, 2);
    assert_eq!((ready.storage_password_enc.as_deref(), ready.storage_password_salt.as_deref()), (None, None));
    assert_eq!(with_storage.settings.storage_max_connections, 7);
    assert_eq!(with_storage.storage_password_enc.as_deref(), Some("v1:storage"));
    assert_eq!(with_storage.storage_password_salt.as_deref(), Some(&[7u8; 16][..]));
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
        attributes: RoleAttributes::default(),
        api_owner_level: None,
        no_password: false,
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
    let owner = chelabase_metadata::NeonRole { name: "postgres".into(), scram_verifier: "SCRAM-SHA-256$v1".into() };
    assert_eq!(roles, vec![owner.clone()]);
    assert_eq!(versioned, vec![owner]);
    assert_eq!(v, 0);
    assert_eq!(unknown, (vec![], 0));
}

// ---- Members hub step 1 (T3): personal rows belong to their records ------------------------
//
// A personal `neon_roles` row exists only with its `personal_logins` record (FK, CHECKs), and
// only the personal-login functions write one (S1). These tests need members, so each runs on
// its own scratch database (the shared demo tenant never keeps them).

/// A personal login's stored verifier in these tests.
const PERSONAL_VERIFIER: &str = "SCRAM-SHA-256$4096:c2FsdA==$personal";

/// Scratch databases (`cl_<8 hex unix seconds><4 hex>`) older than this are dropped by any run.
const SCRATCH_STALE_SECS: u64 = 3600;

/// A scratch database with every migration applied.
struct ScratchDb {
    meta: sqlx::PgPool,
    name: String,
    client: MetadataClient,
}

impl ScratchDb {
    async fn new() -> ScratchDb {
        let base = std::env::var("CHELABASE_METADATA_DATABASE_URL").unwrap_or_else(|_| {
            "postgres://chelabase:chelabase@metadata-db:5432/chelabase_metadata".to_string()
        });
        let meta = raw_pool().await;
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        let stale: Vec<String> =
            sqlx::query_scalar("SELECT datname FROM pg_catalog.pg_database WHERE datname ~ '^cl_[0-9a-f]{12}$'")
                .fetch_all(&meta)
                .await
                .unwrap();
        for old in stale {
            if u64::from_str_radix(&old[3..11], 16).unwrap_or(u64::MAX) < now.saturating_sub(SCRATCH_STALE_SECS) {
                let _ = sqlx::raw_sql(sqlx::AssertSqlSafe(format!("DROP DATABASE IF EXISTS {old} WITH (FORCE)")))
                    .execute(&meta)
                    .await;
            }
        }
        let name = format!("cl_{:08x}{}", now as u32, &hex32()[..4]);
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!("CREATE DATABASE {name}"))).execute(&meta).await.unwrap();
        let (head, tail) = base.rsplit_once('/').expect("a database in the URL");
        let query = tail.split_once('?').map(|(_, q)| format!("?{q}")).unwrap_or_default();
        let client = MetadataClient::connect(&MetadataConfig { database_url: format!("{head}/{name}{query}") })
            .await
            .unwrap();
        client.migrate().await.unwrap();
        ScratchDb { meta, name, client }
    }

    async fn finish(self) {
        self.client.pool().close().await;
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!("DROP DATABASE {} WITH (FORCE)", self.name)))
            .execute(&self.meta)
            .await
            .unwrap();
    }

    /// A tenant whose first member (`alice`, the claiming tenant admin) is returned, with one
    /// project and a ready database whose main has an endpoint: `(tenant, alice, database, endpoint)`.
    async fn ready(&self) -> (TenantId, uuid::Uuid, DatabaseId, String) {
        let c = &self.client;
        let t = TenantId(uuid::Uuid::new_v4());
        sqlx::query("INSERT INTO tenants (id, slug) VALUES ($1, $2)")
            .bind(t.as_uuid())
            .bind(format!("t-{t}"))
            .execute(c.pool())
            .await
            .unwrap();
        let alice = c
            .ensure_member_first_comer_for_tests(t, "alice", "alice@x.com", "Alice", Admission::Active)
            .await
            .unwrap();
        assert!(alice.tenant_admin);
        let p = uuid::Uuid::new_v4();
        sqlx::query("INSERT INTO projects (id, tenant_id, slug, name) VALUES ($1, $2, $3, $3)")
            .bind(p)
            .bind(t.as_uuid())
            .bind(format!("p-{p}"))
            .execute(c.pool())
            .await
            .unwrap();
        let db = DatabaseId::new();
        c.insert_database_pending(db, t, ProjectId(p), &format!("d-{db}")).await.unwrap();
        let ep = link_endpoint(c, db).await;
        assert!(make_ready(c, db, "v1:owner").await);
        (t, alice.id, db, ep)
    }

    /// [`Self::ready`] plus alice's personal login `alice` on the database, with its proxy row
    /// on main's endpoint.
    async fn with_login(&self) -> (TenantId, uuid::Uuid, DatabaseId, String) {
        let (t, alice, db, ep) = self.ready().await;
        let until = chrono::Utc::now() + chrono::Duration::days(90);
        self.client
            .create_personal_login(alice, db.as_uuid(), "alice", PERSONAL_VERIFIER, until)
            .await
            .unwrap();
        self.client
            .upsert_personal_role_row(&ep, alice, db.as_uuid(), "alice", PERSONAL_VERIFIER)
            .await
            .unwrap();
        (t, alice, db, ep)
    }
}

/// Everything a writer could change on a row: origin, verifier, login, sealed password, the
/// unreadable and no-password marks, and the record it belongs to.
type RowState = (String, Option<String>, bool, Option<String>, bool, bool, Option<uuid::Uuid>, Option<uuid::Uuid>);

async fn row_state(client: &MetadataClient, ep: &str, name: &str) -> Option<RowState> {
    sqlx::query_as(
        "SELECT origin, scram_verifier, login, password_enc, password_unreadable, no_password,
                member_id, personal_database_id
         FROM neon_roles WHERE endpoint_id = $1 AND name = $2",
    )
    .bind(ep)
    .bind(name)
    .fetch_optional(client.pool())
    .await
    .unwrap()
}

/// The constraint a statement violated, or `None` when it succeeded.
fn refused_by(result: Result<sqlx::postgres::PgQueryResult, sqlx::Error>) -> Option<String> {
    match result {
        Ok(_) => None,
        Err(sqlx::Error::Database(e)) => Some(e.constraint().unwrap_or("<none>").to_string()),
        Err(other) => panic!("not a database error: {other}"),
    }
}

// MH-9 / spec Part 6: the proxy's read answers no verifier for a personal row whose record is
// revoked or past `valid_until` (a join in the same statement), and the verifier again once
// the record is valid; other rows are unaffected.
#[tokio::test]
#[ignore]
async fn the_proxy_gets_no_secret_for_a_revoked_or_expired_login() {
    let x = ScratchDb::new().await;
    let c = &x.client;
    let (_t, alice, _db, ep) = x.with_login().await;
    seed_copy(c, &ep, "plain", "sql", Some(SQL_VERIFIER), None).await;
    let live = Some(Some(PERSONAL_VERIFIER.to_string()));
    assert_eq!(c.routable_role_secret(&ep, "alice").await.unwrap(), live);
    let set_until = |interval: &'static str| {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "UPDATE personal_logins SET valid_until = now() + interval '{interval}' WHERE member_id = $1"
        )))
        .bind(alice)
        .execute(c.pool())
    };
    set_until("-1 second").await.unwrap();
    assert_eq!(c.routable_role_secret(&ep, "alice").await.unwrap(), Some(None), "expired");
    assert_eq!(c.routable_role_secret(&ep, "plain").await.unwrap(), Some(Some(SQL_VERIFIER.to_string())));
    set_until("1 day").await.unwrap();
    assert_eq!(c.routable_role_secret(&ep, "alice").await.unwrap(), live, "valid again");
    c.revoke_postgres_access(alice, alice).await.unwrap();
    assert_eq!(c.routable_role_secret(&ep, "alice").await.unwrap(), Some(None), "revoked");
    x.finish().await;
}

// MH-15 / MH-11: a personal row needs its member and record (CHECK and FK), never holds a
// sealed password, and only a personal row may name a member.
#[tokio::test]
#[ignore]
async fn a_personal_row_needs_its_record() {
    let x = ScratchDb::new().await;
    let c = &x.client;
    let (_t, alice, db, ep) = x.ready().await;
    let insert = |name: &'static str, origin: &'static str, member: Option<uuid::Uuid>, sealed: Option<&'static str>| {
        sqlx::query(
            "INSERT INTO neon_roles (endpoint_id, name, scram_verifier, origin, password_enc, member_id, personal_database_id)
             VALUES ($1, $2, 'x', $3, $4, $5, $6)",
        )
        .bind(ep.clone())
        .bind(name)
        .bind(origin)
        .bind(sealed)
        .bind(member)
        .bind(member.map(|_| db.as_uuid()))
        .execute(c.pool())
    };
    let orphan = refused_by(
        sqlx::query("INSERT INTO neon_roles (endpoint_id, name, scram_verifier, origin) VALUES ($1, 'orphan', 'x', 'personal')")
            .bind(&ep)
            .execute(c.pool())
            .await,
    );
    assert_eq!(orphan.as_deref(), Some("neon_roles_personal_member"), "a personal row without its member");
    assert_eq!(refused_by(insert("named", "sql", Some(alice), None).await).as_deref(), Some("neon_roles_personal_member"));
    assert_eq!(
        refused_by(insert("alice", "personal", Some(alice), None).await).as_deref(),
        Some("neon_roles_personal_login"),
        "no record yet"
    );
    let until = chrono::Utc::now() + chrono::Duration::days(90);
    c.create_personal_login(alice, db.as_uuid(), "alice", PERSONAL_VERIFIER, until).await.unwrap();
    assert_eq!(
        refused_by(insert("alice", "personal", Some(alice), Some("v1:sealed")).await).as_deref(),
        Some("neon_roles_personal_unsealed")
    );
    assert_eq!(refused_by(insert("alice", "personal", Some(alice), None).await), None, "with its record");
    // Only the record's own name.
    assert_eq!(
        refused_by(insert("other", "personal", Some(alice), None).await).as_deref(),
        Some("neon_roles_personal_login")
    );
    c.delete_personal_login(alice, alice, db.as_uuid()).await.unwrap();
    assert_eq!(row_state(c, &ep, "alice").await, None, "the row goes with its record");
    x.finish().await;
}

// S1: no generic writer touches a personal row: the resync's upserts (no flip to `sql`, no
// verifier written back, MH-9/MH-10) and removals, an upsert of a new `personal` row (skipped),
// the API's by-name writers and the DDL receiver's. The resync's other rows are still written.
#[tokio::test]
#[ignore]
async fn no_generic_writer_touches_a_personal_row() {
    let x = ScratchDb::new().await;
    let c = &x.client;
    let (_t, _alice, db, ep) = x.with_login().await;
    let branch = main_of(db);
    let before = row_state(c, &ep, "alice").await.expect("alice's row");
    assert_eq!((before.0.as_str(), before.1.as_deref()), ("personal", Some(PERSONAL_VERIFIER)));

    // The resync: an upsert of the name as `sql` with the catalog's verifier, a new `sql` row,
    // and a new `personal` row.
    let (_, read) = c.role_copies(&ep).await.unwrap();
    let write = ResyncWrite {
        upserts: vec![
            upsert("alice", RoleOrigin::Sql, "SCRAM-SHA-256$catalog"),
            upsert("carol", RoleOrigin::Sql, SQL_VERIFIER),
            upsert("dave", RoleOrigin::Personal, SQL_VERIFIER),
        ],
        removals: vec![],
        databases: vec![observed("db", "postgres", false)],
    };
    let applied = c.apply_resync(&ep, branch, read, &write).await.unwrap();
    assert!(matches!(applied, ResyncApplied::Applied { .. }), "{applied:?}");
    assert_eq!(row_state(c, &ep, "alice").await.as_ref(), Some(&before), "the resync's upsert");
    assert_eq!(row_state(c, &ep, "carol").await.map(|r| r.0), Some("sql".to_string()), "other rows are written");
    assert_eq!(row_state(c, &ep, "dave").await, None, "a personal upsert is skipped");
    let (_, read) = c.role_copies(&ep).await.unwrap();
    let write = ResyncWrite {
        upserts: vec![],
        removals: vec!["alice".into(), "carol".into()],
        databases: vec![observed("db", "postgres", false)],
    };
    c.apply_resync(&ep, branch, read, &write).await.unwrap();
    assert_eq!(row_state(c, &ep, "alice").await.as_ref(), Some(&before), "the resync's removal");
    assert_eq!(row_state(c, &ep, "carol").await, None);

    // The API's by-name writers.
    c.store_role_secret(&ep, "alice", "SCRAM-SHA-256$api", "v1:api", 100).await.unwrap();
    c.replace_role_secret(&ep, "alice", "SCRAM-SHA-256$api", "v1:api").await.unwrap();
    c.delete_role_copy(&ep, "alice").await.unwrap();
    c.rename_role_copy(&ep, branch, "alice", "alice2", None, None).await.unwrap();
    c.mark_password_unreadable(&ep, "alice").await.unwrap();
    assert_eq!(row_state(c, &ep, "alice").await.as_ref(), Some(&before), "the API's writers");
    assert_eq!(row_state(c, &ep, "alice2").await, None);

    // The DDL receiver's.
    let mut tx = c.begin_ddl(&ep).await.unwrap().expect("the endpoint");
    tx.upsert_sql_copy("alice", "SCRAM-SHA-256$ddl").await.unwrap();
    tx.set_login("alice", false).await.unwrap();
    tx.mark_password_unreadable("alice").await.unwrap();
    tx.upsert_no_password("alice").await.unwrap();
    tx.remove_copy("alice").await.unwrap();
    tx.rename_copy("alice", "zed", None).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(row_state(c, &ep, "alice").await.as_ref(), Some(&before), "the DDL receiver's writers");
    assert_eq!(row_state(c, &ep, "zed").await, None);
    x.finish().await;
}

// S1: a branch copy leaves personal rows out (the child's come from its reconcile); its other
// copies are written.
#[tokio::test]
#[ignore]
async fn a_branch_copy_skips_personal_rows() {
    let x = ScratchDb::new().await;
    let c = &x.client;
    let (_t, _alice, db, _ep) = x.ready().await;
    let child = new_branch(db, "dev", main_of(db));
    assert_eq!(c.create_branch(&child).await.unwrap(), CreateBranchOutcome::Created);
    let plain = RoleAttributes::default();
    let copies = [
        NewRoleCopy { name: "alice", origin: RoleOrigin::Personal, verifier: Some(PERSONAL_VERIFIER), password_enc: None, login: true, level: RoleLevel::Plain, api_owner_level: false, pending: false, attributes: &plain },
        NewRoleCopy { name: "app", origin: RoleOrigin::Sql, verifier: Some(SQL_VERIFIER), password_enc: None, login: true, level: RoleLevel::Plain, api_owner_level: false, pending: false, attributes: &plain },
    ];
    let ep = ep_id();
    let (tenant, timeline) = (hex32(), hex32());
    let mut new = new_endpoint(&ep, &tenant, &timeline);
    new.role_copies = &copies;
    let created = c.create_endpoint_for_branch(child.id, &timeline, &new, false).await;
    let names: Vec<String> = c.role_copies(&ep).await.unwrap().0.into_iter().map(|r| r.name).collect();
    assert!(matches!(created, Ok(true)), "{created:?}");
    assert_eq!(names, vec!["postgres".to_string(), "app".to_string()], "no personal copy");
    x.finish().await;
}

// F2 round 2, changed deliberately by T3: a resync's removals never delete a personal row (an
// `sql` row listed with it is deleted); personal rows are the reconciler's and go with their
// record.
#[tokio::test]
#[ignore]
async fn a_resync_never_removes_a_personal_row() {
    let x = ScratchDb::new().await;
    let c = &x.client;
    let (_t, alice, db, ep) = x.with_login().await;
    seed_copy(c, &ep, "bob", "sql", Some(SQL_VERIFIER), None).await;
    let (_, read) = c.role_copies(&ep).await.unwrap();
    let write = ResyncWrite {
        upserts: vec![],
        removals: vec!["alice".into(), "bob".into()],
        databases: vec![observed("app", "postgres", false)],
    };
    c.apply_resync(&ep, main_of(db), read, &write).await.unwrap();
    let (after, _) = c.role_copies(&ep).await.unwrap();
    let origin_of = |n: &str| after.iter().find(|r| r.name == n).map(|r| r.origin);
    assert_eq!(origin_of("alice"), Some(RoleOrigin::Personal));
    assert_eq!(origin_of("bob"), None);
    c.delete_personal_login(alice, alice, db.as_uuid()).await.unwrap();
    assert_eq!(row_state(c, &ep, "alice").await, None, "gone with its record");
    x.finish().await;
}

// c3 (P10, SP13), changed deliberately by T3 (S1): the origin round-trips through `list_roles`
// and `role_copies`; the spec's roles (`neon_roles`, `neon_roles_versioned`) never carry a
// personal row (RT4-4); the resync no longer turns a personal row into `sql` (MH-10) nor stores
// a new personal row (only the reconciler's write does).
#[tokio::test]
#[ignore]
async fn a_personal_origin_round_trips_and_never_reaches_the_spec() {
    let x = ScratchDb::new().await;
    let c = &x.client;
    let (_t, _alice, db, ep) = x.with_login().await;
    seed_copy(c, &ep, "from_sql", "sql", Some(SQL_VERIFIER), None).await;
    let listed = c.list_roles(&ep).await.unwrap();
    let (copies, _) = c.role_copies(&ep).await.unwrap();
    let spec_roles = c.neon_roles(&ep).await.unwrap();
    let (versioned, _) = c.neon_roles_versioned(&ep).await.unwrap();
    let secret = c.neon_role_secret(&ep, "alice").await.unwrap();
    let branch = main_of(db);
    let (_, read) = c.role_copies(&ep).await.unwrap();
    let write = ResyncWrite {
        upserts: vec![upsert("alice", RoleOrigin::Sql, SQL_VERIFIER)],
        removals: vec![],
        databases: vec![observed("app", "postgres", false)],
    };
    c.apply_resync(&ep, branch, read, &write).await.unwrap();
    let (after, _) = c.role_copies(&ep).await.unwrap();
    let (_, read) = c.role_copies(&ep).await.unwrap();
    let write = ResyncWrite {
        upserts: vec![upsert("bob", RoleOrigin::Personal, SQL_VERIFIER)],
        removals: vec![],
        databases: vec![observed("app", "postgres", false)],
    };
    c.apply_resync(&ep, branch, read, &write).await.unwrap();
    let (with_bob, _) = c.role_copies(&ep).await.unwrap();
    let bogus = sqlx::query("INSERT INTO neon_roles (endpoint_id, name, scram_verifier, origin) VALUES ($1, 'x', 'x', 'member')")
        .bind(&ep)
        .execute(c.pool())
        .await;
    let origin_of = |rows: &[chelabase_metadata::RoleRow], n: &str| rows.iter().find(|r| r.name == n).map(|r| r.origin);
    assert_eq!(origin_of(&listed, "alice"), Some(RoleOrigin::Personal));
    assert_eq!(origin_of(&listed, "from_sql"), Some(RoleOrigin::Sql));
    assert_eq!(copies.iter().find(|r| r.name == "alice").unwrap().origin, RoleOrigin::Personal);
    let owner = chelabase_metadata::NeonRole { name: "postgres".into(), scram_verifier: "SCRAM-SHA-256$v1".into() };
    assert_eq!(spec_roles, vec![owner.clone()], "a personal row is never a spec role");
    assert_eq!(versioned, vec![owner]);
    assert_eq!(secret, Some(Some(PERSONAL_VERIFIER.to_string())), "the proxy authenticates a live login");
    let alice_after = after.iter().find(|r| r.name == "alice").unwrap();
    assert_eq!(alice_after.origin, RoleOrigin::Personal, "never flipped to sql");
    assert_eq!(alice_after.scram_verifier.as_deref(), Some(PERSONAL_VERIFIER), "no catalog verifier written back");
    assert!(!with_bob.iter().any(|r| r.name == "bob"), "the resync never stores a personal row");
    assert_eq!(refused_by(bogus).as_deref(), Some("neon_roles_origin"));
    x.finish().await;
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
    let states = |rows: &[chelabase_metadata::RoleRow]| {
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

// U1: a NOLOGIN api row has no secrets and shows `none`; an update stores the wanted state
// (pending, version bumped), and turning LOGIN on stores the new secrets.
#[tokio::test]
#[ignore]
async fn api_role_attributes_insert_and_update() {
    let client = client().await;
    let (id, ep) = ready_database(&client).await;
    let group = RoleAttributes {
        inherit: false,
        connection_limit: 3,
        valid_until: Some(chrono::DateTime::<chrono::Utc>::from_timestamp(1_900_000_000, 0).unwrap()),
        bypass_rls: true,
        member_of: vec![],
        member_no_set: vec![],
        member_admin: vec![],
    };
    let role = NewApiRole {
        name: "readers",
        owner_level: false,
        login: false,
        attributes: &group,
        verifier: None,
        password_enc: None,
        change_id: uuid::Uuid::new_v4(),
    };
    let inserted = client.insert_api_role_with(&ep, &role, 10).await.unwrap();
    let rows = client.list_roles(&ep).await.unwrap();
    let (_, before) = client.role_copies(&ep).await.unwrap();
    // I2: the options of a membership are stored with it.
    let member = RoleAttributes {
        member_of: vec!["readers".into()],
        member_no_set: vec!["readers".into()],
        member_admin: vec!["readers".into()],
        ..RoleAttributes::default()
    };
    let change = ApiRoleChange {
        owner_level: false,
        login: true,
        attributes: member.clone(),
        secret: SecretChange::Set("SCRAM-SHA-256$n".into(), "v1:n".into()),
        change_id: uuid::Uuid::new_v4(),
    };
    let updated = client.update_api_role(&ep, "readers", &change, 10).await.unwrap();
    let missing = client.update_api_role(&ep, "ghost", &change, 10).await.unwrap();
    let (copies, after) = client.role_copies(&ep).await.unwrap();
    let sealed = client.role_password(&ep, "readers").await.unwrap();
    let cleared = ApiRoleChange { login: false, secret: SecretChange::Clear, ..change.clone() };
    client.update_api_role(&ep, "readers", &cleared, 10).await.unwrap();
    let (cleared_copies, _) = client.role_copies(&ep).await.unwrap();
    cleanup(&client, id).await;
    assert_eq!(inserted, InsertRoleOutcome::Inserted);
    let row = rows.iter().find(|r| r.name == "readers").unwrap();
    assert_eq!((row.login, row.password, row.attributes.clone()), (false, PasswordState::None, group));
    assert_eq!((updated, missing), (RoleWriteOutcome::Done, RoleWriteOutcome::NotFound));
    assert_eq!(after, before + 1);
    let copy = copies.iter().find(|c| c.name == "readers").unwrap();
    assert_eq!(copy.attributes, member);
    assert!(copy.login && copy.pending);
    assert_eq!(copy.scram_verifier.as_deref(), Some("SCRAM-SHA-256$n"));
    assert_eq!(sealed.as_deref(), Some("v1:n"));
    let copy = cleared_copies.iter().find(|c| c.name == "readers").unwrap();
    assert_eq!((copy.login, copy.scram_verifier.as_deref(), copy.password_enc.as_deref()), (false, None, None));
}

// An SQL role turned LOGIN through the API: its row is made (or replaced) as `sql`, with
// the sealed password; the owner's row is never touched.
#[tokio::test]
#[ignore]
async fn store_role_secret_inserts_or_replaces_sql_rows() {
    let client = client().await;
    let (id, ep) = ready_database(&client).await;
    let fresh = client.store_role_secret(&ep, "made_in_sql", "SCRAM-SHA-256$a", "v1:a", 10).await.unwrap();
    let again = client.store_role_secret(&ep, "made_in_sql", "SCRAM-SHA-256$b", "v1:b", 10).await.unwrap();
    let owner = client.store_role_secret(&ep, "postgres", "SCRAM-SHA-256$x", "v1:x", 10).await.unwrap();
    let (copies, _) = client.role_copies(&ep).await.unwrap();
    cleanup(&client, id).await;
    assert_eq!((fresh, again, owner), (RoleWriteOutcome::Done, RoleWriteOutcome::Done, RoleWriteOutcome::NotFound));
    let copy = copies.iter().find(|c| c.name == "made_in_sql").unwrap();
    assert_eq!((copy.origin, copy.login, copy.pending), (RoleOrigin::Sql, true, true));
    assert_eq!(copy.scram_verifier.as_deref(), Some("SCRAM-SHA-256$b"));
    assert_eq!(copy.password_enc.as_deref(), Some("v1:b"));
    let owner_row = copies.iter().find(|c| c.name == "postgres").unwrap();
    assert_ne!(owner_row.scram_verifier.as_deref(), Some("SCRAM-SHA-256$x"));
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
    let count = |rows: &[chelabase_metadata::RoleRow], o| rows.iter().filter(|r| r.origin == o).count();
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
    let stale = client.confirm_role(&ep, "app", v - 1, None).await.unwrap();
    let pending_after_stale = client.list_roles(&ep).await.unwrap().iter().filter(|r| r.pending).count();
    let confirmed = client.confirm_role(&ep, "app", v, None).await.unwrap();
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

// The 10 s guard covers what the receiver owns (verifier, sealed password, login); the
// catalog-owned `level`, `pending` and attributes of a row stamped a moment ago still update.
#[tokio::test]
#[ignore]
async fn resync_updates_catalog_columns_of_a_recently_forwarded_row() {
    let _sealed = sealed_lock().await;
    let client = client().await;
    let (id, ep) = ready_database(&client).await;
    let branch = main_of(id);
    seed_copy(&client, &ep, "app", "sql", Some("SCRAM-SHA-256$forwarded"), None).await;
    sqlx::query("UPDATE neon_roles SET pending = true, login = false WHERE endpoint_id = $1 AND name = 'app'")
        .bind(&ep)
        .execute(client.pool())
        .await
        .unwrap();
    set_forwarded(&client, &ep, "app", 1).await;
    let (_, v) = client.role_copies(&ep).await.unwrap();
    let mut row = upsert("app", RoleOrigin::Sql, "SCRAM-SHA-256$catalog");
    row.level = RoleLevel::OwnerLevel;
    row.pending = false;
    row.login = true;
    row.password_enc = SealedChange::Set("v1:catalog".into());
    let wanted = RoleAttributes {
        inherit: false,
        connection_limit: 5,
        valid_until: Some(chrono::DateTime::<chrono::Utc>::from_timestamp(1_900_000_000, 0).unwrap()),
        bypass_rls: true,
        member_of: vec!["grp".into()],
        member_no_set: vec!["grp".into()],
        member_admin: vec![],
    };
    row.attributes = wanted.clone();
    let write = ResyncWrite {
        upserts: vec![row],
        removals: vec![],
        databases: vec![observed("db", "postgres", false)],
    };
    let applied = client.apply_resync(&ep, branch, v, &write).await.unwrap();
    let (copies, _) = client.role_copies(&ep).await.unwrap();
    cleanup(&client, id).await;
    let app = copies.iter().find(|c| c.name == "app").unwrap();
    assert_eq!(applied, ResyncApplied::Applied { changed: vec!["app".into()] });
    assert_eq!(app.level, RoleLevel::OwnerLevel, "level follows the catalog");
    assert!(!app.pending, "pending follows the catalog");
    assert_eq!(app.scram_verifier.as_deref(), Some("SCRAM-SHA-256$forwarded"));
    assert!(!app.login, "the forwarded login is kept");
    assert_eq!(app.password_enc, None, "the forwarded sealed password is kept");
    assert_eq!(app.attributes, wanted, "inherit, connection_limit, valid_until, bypass_rls and member_of follow the catalog");
}

/// A group role with no password is listed with password `none`, an MD5 one as unusable, and
/// a forwarded verifier later turns the no-password row back into a plain one.
#[tokio::test]
#[ignore]
async fn resync_lists_roles_without_a_password() {
    let _sealed = sealed_lock().await;
    let client = client().await;
    let (id, ep) = ready_database(&client).await;
    let branch = main_of(id);
    let (_, v) = client.role_copies(&ep).await.unwrap();
    let mut group = upsert("billing", RoleOrigin::Sql, "x");
    group.verifier = None;
    group.password_enc = SealedChange::Clear;
    group.login = false;
    group.no_password = true;
    let mut md5 = upsert("legacy", RoleOrigin::Sql, "x");
    md5.verifier = None;
    let write = ResyncWrite { upserts: vec![group, md5], removals: vec![], databases: vec![observed("db", "postgres", false)] };
    client.apply_resync(&ep, branch, v, &write).await.unwrap();
    let listed = client.list_roles(&ep).await.unwrap();
    let state = |n: &str| listed.iter().find(|r| r.name == n).map(|r| (r.password, r.login));
    let billing = state("billing");
    let legacy = state("legacy");
    // A password set later in SQL: the row is a plain one again.
    let (_, v) = client.role_copies(&ep).await.unwrap();
    let mut with_password = upsert("billing", RoleOrigin::Sql, "SCRAM-SHA-256$set");
    with_password.login = false;
    client
        .apply_resync(&ep, branch, v, &ResyncWrite { upserts: vec![with_password], removals: vec![], databases: vec![observed("db", "postgres", false)] })
        .await
        .unwrap();
    let after = client.list_roles(&ep).await.unwrap();
    cleanup(&client, id).await;
    assert_eq!(billing, Some((PasswordState::None, false)));
    assert_eq!(legacy, Some((PasswordState::Md5Unusable, true)));
    assert_eq!(after.iter().find(|r| r.name == "billing").map(|r| r.password), Some(PasswordState::SetInSql));
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
    let plain = RoleAttributes::default();
    let group = RoleAttributes {
        inherit: false,
        connection_limit: 3,
        valid_until: Some(chrono::DateTime::<chrono::Utc>::from_timestamp(1_900_000_000, 0).unwrap()),
        bypass_rls: true,
        member_of: vec!["readers".into()],
        member_no_set: vec![],
        member_admin: vec!["readers".into()],
    };
    let copies = [
        NewRoleCopy { name: "reader", origin: RoleOrigin::Api, verifier: Some("SCRAM-SHA-256$r"), password_enc: Some("v1:reader"), login: true, level: RoleLevel::Plain, api_owner_level: false, pending: true, attributes: &plain },
        NewRoleCopy { name: "writer", origin: RoleOrigin::Api, verifier: Some("SCRAM-SHA-256$w"), password_enc: Some("v1:writer"), login: true, level: RoleLevel::OwnerLevel, api_owner_level: true, pending: true, attributes: &group },
        NewRoleCopy { name: "My Role", origin: RoleOrigin::Sql, verifier: Some("SCRAM-SHA-256$s"), password_enc: None, login: false, level: RoleLevel::OwnerLevel, api_owner_level: false, pending: false, attributes: &plain },
        NewRoleCopy { name: "legacy", origin: RoleOrigin::Sql, verifier: None, password_enc: None, login: true, level: RoleLevel::Plain, api_owner_level: false, pending: false, attributes: &plain },
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
    // A child's copies keep the parent's attributes, so its resync restores nothing.
    assert_eq!(all.iter().find(|c| c.name == "writer").unwrap().attributes, group);
    assert_eq!(rows.iter().find(|r| r.name == "writer").unwrap().attributes, group);
    assert_eq!(all.iter().find(|c| c.name == "reader").unwrap().attributes, plain);
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

#[tokio::test]
#[ignore]
async fn ddl_set_login_flags_a_copy_and_stamps_it() {
    let _sealed = sealed_lock().await;
    let client = client().await;
    let (id, ep) = ready_database(&client).await;
    seed_copy(&client, &ep, "app", "sql", Some("SCRAM-SHA-256$a"), None).await;
    let mut tx = client.begin_ddl(&ep).await.unwrap().expect("endpoint");
    let flagged = tx.set_login("app", false).await.unwrap();
    let missing = tx.set_login("ghost", false).await.unwrap();
    tx.commit().await.unwrap();
    let (login, stamped): (bool, bool) = sqlx::query_as(
        "SELECT login, forwarded_at > now() - interval '10 seconds' FROM neon_roles
         WHERE endpoint_id = $1 AND name = 'app'",
    )
    .bind(&ep)
    .fetch_one(client.pool())
    .await
    .unwrap();
    cleanup(&client, id).await;
    assert!(flagged && !missing);
    assert!(!login && stamped);
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
    let chelabase_main_rows_off: i64 = sqlx::query_scalar(
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
    assert_eq!(chelabase_main_rows_off, 0);
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
    let (id, ep) = ready_database(&client).await;
    client.insert_api_role(&ep, "service_role", true, "SCRAM-SHA-256$o", "v1:sr", 10).await.unwrap();
    let branch = main_of(id);
    let old = key_with(branch, "service_role", "old", Some(&["db", "x"]));
    client.insert_data_key(&old).await.unwrap();
    let (mut new, _) = new_key(branch, DataKeyKind::Secret, "service_role", "old");
    new.databases = Some(vec!["ignored".into()]);
    assert!(client.rotate_secret_key(branch, old.id, &new).await.unwrap());
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

/// Every column a refresh could write, as text: peek must leave all of them as they were.
async fn session_row(client: &MetadataClient, id: uuid::Uuid) -> String {
    sqlx::query_scalar::<_, String>(
        "SELECT concat_ws('|', encode(refresh_hash, 'hex'), encode(prev_refresh_hash, 'hex'),
                grace_refresh_sealed, grace_until::text, last_used_at::text, expires_at::text,
                revoked_at::text)
         FROM app_sessions WHERE id = $1",
    )
    .bind(id)
    .fetch_one(client.pool())
    .await
    .unwrap()
}

// #59: peek answers what refresh would, by the same rules, and writes nothing: no rotation,
// no revocation on reuse, expiry or cap.
#[tokio::test]
#[ignore]
async fn peek_answers_like_refresh_and_writes_nothing() {
    let client = client().await;
    let (project, other) = (new_project(&client, "peek").await, new_project(&client, "peek-other").await);
    let user = uuid::Uuid::new_v4();
    let cap = std::time::Duration::from_secs(30 * 86400);
    // `s` rotated twice: 3 is current, 2 previous (within its grace), 1 two rotations old.
    let s = new_session(project, user, 1);
    // `late` rotated once and its grace is over: 4 is previous, 5 current.
    let late = new_session(project, user, 4);
    let (revoked, expired, capped) =
        (new_session(project, user, 6), new_session(project, user, 7), new_session(project, user, 8));
    for x in [&s, &late, &revoked, &expired, &capped] {
        client.insert_app_session(x).await.unwrap();
    }
    client.refresh_app_session(s.id, project, hash(1), &next(2, 60)).await.unwrap();
    client.refresh_app_session(s.id, project, hash(2), &next(3, 60)).await.unwrap();
    client.refresh_app_session(late.id, project, hash(4), &next(5, 60)).await.unwrap();
    sqlx::query("UPDATE app_sessions SET grace_until = now() - interval '1 second' WHERE id = $1")
        .bind(late.id)
        .execute(client.pool())
        .await
        .unwrap();
    client.revoke_app_session(revoked.id, project).await.unwrap();
    sqlx::query("UPDATE app_sessions SET expires_at = now() - interval '1 second' WHERE id = $1")
        .bind(expired.id)
        .execute(client.pool())
        .await
        .unwrap();
    backdate_session(&client, capped.id, 31 * 86400).await;
    let ids = [s.id, late.id, revoked.id, expired.id, capped.id];
    let mut before = Vec::new();
    for id in ids {
        before.push(session_row(&client, id).await);
    }

    let peek = |id, project, n| client.peek_app_session(id, project, hash(n), cap);
    let current = peek(s.id, project, 3).await.unwrap();
    let grace = peek(s.id, project, 2).await.unwrap();
    let old = peek(s.id, project, 1).await.unwrap();
    let unknown_hash = peek(s.id, project, 99).await.unwrap();
    let unknown_id = peek(uuid::Uuid::new_v4(), project, 3).await.unwrap();
    let elsewhere = peek(s.id, other, 3).await.unwrap();
    let reused = peek(late.id, project, 4).await.unwrap();
    let late_current = peek(late.id, project, 5).await.unwrap();
    let dead = peek(revoked.id, project, 6).await.unwrap();
    let past = peek(expired.id, project, 7).await.unwrap();
    let over_cap = peek(capped.id, project, 8).await.unwrap();
    let mut after = Vec::new();
    for id in ids {
        after.push(session_row(&client, id).await);
    }
    // The current token still rotates afterwards; the reused one still revokes.
    let rotated = client.refresh_app_session(s.id, project, hash(3), &next(10, 60)).await.unwrap();
    let revokes = client.refresh_app_session(late.id, project, hash(4), &next(11, 60)).await.unwrap();
    cleanup_project(&client, project, &[]).await;
    cleanup_project(&client, other, &[]).await;

    let PeekOutcome::Live { session: c, grace: false } = current else { panic!("{current:?}") };
    assert_eq!((c.id, c.user_id, c.project_id), (s.id, user, project));
    // #59 audit B1: the previous token within its grace says so.
    let PeekOutcome::Live { session: g, grace: true } = grace else { panic!("{grace:?}") };
    assert_eq!((g.id, g.user_id), (s.id, user));
    assert!(matches!(late_current, PeekOutcome::Live { grace: false, .. }), "{late_current:?}");
    assert_eq!(
        [old, unknown_hash, unknown_id, elsewhere, dead, past, over_cap],
        [const { PeekOutcome::Invalid }; 7]
    );
    assert_eq!(reused, PeekOutcome::Reused);
    assert_eq!(before, after, "peek wrote nothing");
    assert!(matches!(rotated, RefreshOutcome::Rotated(_)), "{rotated:?}");
    assert!(matches!(revokes, RefreshOutcome::Reused(_)), "{revokes:?}");
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
        .create_saved_query(db.as_uuid(), user, None, uuid::Uuid::new_v4(), name, "select 1", None, None)
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
    let tag = uuid::Uuid::new_v4().simple().to_string();
    let author = client
        .ensure_member_first_comer_for_tests(TenantId(DEMO_TENANT_ID), &format!("saved-{tag}"), &format!("saved-{tag}@example.test"), "Author", chelabase_metadata::members::Admission::Active)
        .await
        .unwrap()
        .id;
    let created = saved(
        client.create_saved_query(d, "u1", Some(author), id, "Beta", "select 2", Some("app"), Some("anon")).await.unwrap(),
    );
    saved(create_saved(&client, db, "u1", "alpha").await);
    assert_eq!((created.version, created.pg_database.as_deref(), created.role.as_deref()), (1, Some("app"), Some("anon")));

    let list = client.list_saved_queries(d).await.unwrap();
    assert_eq!(list.iter().map(|q| q.name.as_str()).collect::<Vec<_>>(), ["alpha", "Beta"]);
    assert_eq!(created.author_member_id, Some(author));
    // Shared in the project: not tied to the user who saved it.
    assert_eq!(client.get_saved_query(d, id).await.unwrap(), Some(created.clone()));
    assert_eq!(client.get_saved_query(d, uuid::Uuid::new_v4()).await.unwrap(), None);

    // null clears a hint, an absent field stays.
    let patch = SavedQueryPatch { sql: Some("select 3".into()), role: Some(None), ..Default::default() };
    // Not the author's: another member can't change it; an Admin can; a row without an author
    // is Admin-only.
    let stranger = SavedQueryEditor::Author(uuid::Uuid::new_v4());
    assert_eq!(client.update_saved_query(d, stranger, id, 1, &patch).await.unwrap(), SavedQueryWrite::NotFound);
    let no_author = saved(create_saved(&client, db, "u1", "orphan").await);
    assert_eq!(
        client.update_saved_query(d, SavedQueryEditor::Author(author), no_author.id, 1, &patch).await.unwrap(),
        SavedQueryWrite::NotFound
    );
    assert!(!client.delete_saved_query(d, SavedQueryEditor::Author(author), no_author.id).await.unwrap());
    assert!(client.delete_saved_query(d, SavedQueryEditor::Admin, no_author.id).await.unwrap());
    let updated = saved(client.update_saved_query(d, SavedQueryEditor::Author(author), id, 1, &patch).await.unwrap());
    assert_eq!((updated.version, updated.sql.as_str(), updated.role.clone()), (2, "select 3", None));
    assert_eq!((updated.pg_database.as_deref(), updated.name.as_str()), (Some("app"), "Beta"));
    assert!(updated.updated_at >= created.updated_at);

    assert!(!client.delete_saved_query(d, stranger, id).await.unwrap());
    assert!(client.delete_saved_query(d, SavedQueryEditor::Author(author), id).await.unwrap());
    assert!(!client.delete_saved_query(d, SavedQueryEditor::Admin, id).await.unwrap());
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
    let renamed = client.update_saved_query(db.as_uuid(), SavedQueryEditor::Admin, other.id, 1, &patch).await.unwrap();
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
    let first = saved(client.update_saved_query(d, SavedQueryEditor::Admin, q.id, 1, &patch).await.unwrap());
    let again = client.update_saved_query(d, SavedQueryEditor::Admin, q.id, 1, &patch).await.unwrap();
    assert_eq!(again, SavedQueryWrite::Stale(first));
    let missing = client.update_saved_query(d, SavedQueryEditor::Admin, uuid::Uuid::new_v4(), 1, &patch).await.unwrap();
    assert_eq!(missing, SavedQueryWrite::NotFound);
    let foreign = client.update_saved_query(d, SavedQueryEditor::Author(uuid::Uuid::new_v4()), q.id, 2, &patch).await.unwrap();
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

// --- Step 6g: schema merges -------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn rehearsal_kind_round_trips_and_is_not_counted() {
    let _sealed = sealed_lock().await;
    let client = client().await;
    let (db, _ep) = ready_database(&client).await;
    let main = main_of(db);
    let name = "_rehearsal-0a1b2c3d";

    let rehearsal = client.create_rehearsal_branch(db, main, name, "tester").await.unwrap();
    let read_back = branch(&client, rehearsal.id).await;
    let name_taken = client.create_rehearsal_branch(db, main, name, "tester").await;
    // Only main counts; a normal branch is still allowed up to the cap.
    let counts = client
        .main_branches_with_counts(TenantId(DEMO_TENANT_ID))
        .await
        .unwrap()
        .into_iter()
        .find(|(b, _)| b.id == main)
        .map(|(_, n)| n);
    let children: Vec<BranchId> = client.live_children(main).await.unwrap().iter().map(|b| b.id).collect();
    let dev = new_branch(db, "dev", main);
    let dev_created = client.create_branch(&dev).await.unwrap();
    cleanup(&client, db).await;

    assert_eq!((rehearsal.kind, rehearsal.status), (BranchKind::Rehearsal, BranchStatus::Pending));
    assert_eq!(rehearsal.parent_branch_id, Some(main));
    assert_eq!(read_back, rehearsal);
    assert!(matches!(name_taken, Err(MetadataError::BranchNameTaken(n)) if n == name));
    assert_eq!(counts, Some(1));
    assert!(children.is_empty());
    assert_eq!(dev_created, CreateBranchOutcome::Created);
    assert_eq!(BranchKind::parse("rehearsal").unwrap().as_str(), "rehearsal");
}

#[tokio::test]
#[ignore]
async fn branched_at_lsn_updates_with_the_switch() {
    let _sealed = sealed_lock().await;
    let client = client().await;
    let (id, _ep) = ready_database(&client).await;
    let main = main_of(id);
    let pending = start_reset(&client, main).await;
    let before = main_branch(&client, id).await;

    let switched = client.switch_to_pending_timeline(main, &pending, None, "0/2A2B3C").await.unwrap();
    let after = main_branch(&client, id).await;
    cleanup(&client, id).await;

    assert!(switched);
    assert_ne!(before.branched_at_lsn.as_deref(), Some("0/2A2B3C"));
    assert_eq!(after.branched_at_lsn.as_deref(), Some("0/2A2B3C"));
}

fn base(branch: BranchId, source: BaseSource, model: Option<serde_json::Value>) -> SchemaBase {
    let timeline = hex32();
    let mut b = base_at(branch, "app", source, &timeline, "0/1");
    b.model_version = model.as_ref().map(|_| 1);
    b.server_major = model.as_ref().map(|_| 17);
    b.model = model;
    b
}

/// A location-only base of `db`, read at and valid from (`timeline`, `lsn`).
fn base_at(branch: BranchId, db: &str, source: BaseSource, timeline: &str, lsn: &str) -> SchemaBase {
    SchemaBase {
        id: 0,
        branch_id: branch,
        database_name: db.into(),
        model: None,
        model_version: None,
        server_major: None,
        source,
        timeline_id: Some(timeline.into()),
        lsn: Some(lsn.into()),
        branch_timeline_id: Some(timeline.into()),
        branch_lsn: Some(lsn.into()),
        identity_map: serde_json::json!({"pairs": []}),
        taken_at: chrono::Utc::now(),
        recorded_at: chrono::Utc::now(),
    }
}

async fn base_entries(client: &MetadataClient, branch: BranchId) -> Vec<(i64, String, String)> {
    sqlx::query_as("SELECT id, database_name, source FROM branch_schema_bases WHERE branch_id = $1 ORDER BY id")
        .bind(branch.as_uuid())
        .fetch_all(client.pool())
        .await
        .unwrap()
}

// E18: bases are a history; the current one is the newest entry.
#[tokio::test]
#[ignore]
async fn schema_base_history_appends_and_current_is_newest() {
    let _sealed = sealed_lock().await;
    let client = client().await;
    let (db, _ep) = ready_database(&client).await;
    let main = main_of(db);

    let none_yet = client.schema_base(main, "app").await.unwrap();
    let first = base(main, BaseSource::Create, None);
    let first_id = client.append_schema_base(&first).await.unwrap();
    let unfilled = client.schema_base(main, "app").await.unwrap().unwrap();
    let second = base(main, BaseSource::Merge, Some(serde_json::json!({"tables": ["t"]})));
    let second_id = client.append_schema_base(&second).await.unwrap();
    let current = client.schema_base(main, "app").await.unwrap().unwrap();
    // The live fallback fills an entry in place: the first one stays first, the current one
    // stays current.
    let model = serde_json::json!({"tables": []});
    let fill = SchemaBase {
        model: Some(model.clone()),
        model_version: Some(1),
        server_major: Some(17),
        identity_map: serde_json::json!({"pairs": [1]}),
        ..unfilled.clone()
    };
    let filled = client.update_schema_base(&fill).await.unwrap();
    let missing = client.update_schema_base(&SchemaBase { id: -1, ..fill.clone() }).await.unwrap();
    let still_current = client.schema_base(main, "app").await.unwrap().unwrap();
    let first_after = client
        .schema_base_at(main, "app", first.branch_timeline_id.as_deref().unwrap(), "0/1")
        .await
        .unwrap()
        .unwrap();
    // Entries of a database the branch no longer has go when the base is next recorded.
    client.append_schema_base(&base_at(main, "gone", BaseSource::Create, &hex32(), "0/1")).await.unwrap();
    let before_prune = base_entries(&client, main).await;
    let pruned = client.prune_schema_bases(main, &["app".to_string()]).await.unwrap();
    let after_prune = base_entries(&client, main).await;
    cleanup(&client, db).await;

    assert!(none_yet.is_none());
    assert!(first_id < second_id, "{first_id} {second_id}");
    assert_eq!((unfilled.id, unfilled.source, unfilled.model.clone()), (first_id, BaseSource::Create, None));
    assert_eq!(unfilled.timeline_id, first.timeline_id);
    assert_eq!(unfilled.branch_timeline_id, first.branch_timeline_id);
    assert_eq!(unfilled.branch_lsn.as_deref(), Some("0/1"));
    assert_eq!(unfilled.identity_map, serde_json::json!({"pairs": []}));
    assert_eq!((current.id, current.source), (second_id, BaseSource::Merge));
    assert_eq!(current.model, Some(serde_json::json!({"tables": ["t"]})));
    assert!(filled);
    assert!(!missing);
    assert_eq!(still_current.id, second_id);
    assert_eq!(first_after.id, first_id);
    assert_eq!((first_after.model, first_after.model_version, first_after.server_major), (Some(model), Some(1), Some(17)));
    assert_eq!(first_after.identity_map, serde_json::json!({"pairs": [1]}));
    assert_eq!(first_after.source, BaseSource::Create, "a fill changes the model and map only");
    assert_eq!(before_prune.len(), 3);
    assert_eq!(pruned, 1);
    assert_eq!(
        after_prune,
        vec![(first_id, "app".to_string(), "create".to_string()), (second_id, "app".to_string(), "merge".to_string())]
    );
}

// E18: the history migration, applied byte for byte to T5's table shape in a scratch schema.
#[tokio::test]
#[ignore]
async fn migration_keeps_existing_bases_as_first_entries() {
    let pool = raw_pool().await;
    let schema = format!("t7b_migration_{}", hex32());
    let mut conn = pool.acquire().await.unwrap();
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}; SET search_path TO {schema};")))
        .execute(&mut *conn)
        .await
        .unwrap();
    // `branch_schema_bases` as 20261010000001 created it, and a stub of `branches`.
    sqlx::raw_sql(
        "CREATE TABLE branches (id uuid PRIMARY KEY, timeline_id text NULL, branched_at_lsn text NULL);
         CREATE TABLE branch_schema_bases (
             branch_id      uuid NOT NULL REFERENCES branches (id) ON DELETE CASCADE,
             database_name  text NOT NULL,
             model          jsonb NULL,
             model_version  integer NULL,
             server_major   integer NULL,
             source         text NOT NULL CHECK (source IN ('create', 'reset', 'merge', 'update', 'rebaseline')),
             timeline_id    text NULL,
             lsn            text NULL,
             identity_map   jsonb NOT NULL DEFAULT '{\"pairs\":[]}',
             taken_at       timestamptz NOT NULL DEFAULT now(),
             PRIMARY KEY (branch_id, database_name)
         );",
    )
    .execute(&mut *conn)
    .await
    .unwrap();
    let (dev, main) = (uuid::Uuid::new_v4(), uuid::Uuid::new_v4());
    sqlx::query("INSERT INTO branches VALUES ($1, 'tdev', '0/2000000'), ($2, 'tmain', NULL)")
        .bind(dev)
        .bind(main)
        .execute(&mut *conn)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO branch_schema_bases (branch_id, database_name, model, source, timeline_id, lsn)
         VALUES ($1, 'app', '{\"t\":1}', 'create', 'tread', '0/1000000'),
                ($1, 'shop', NULL, 'reset', 'tread', '0/1000000'),
                ($2, 'app', NULL, 'rebaseline', NULL, NULL)",
    )
    .bind(dev)
    .bind(main)
    .execute(&mut *conn)
    .await
    .unwrap();

    sqlx::raw_sql(include_str!("../migrations/20261010000004_schema_base_history.sql"))
        .execute(&mut *conn)
        .await
        .unwrap();

    type Row = (i64, uuid::Uuid, String, String, Option<String>, Option<String>, Option<String>, Option<String>);
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT id, branch_id, database_name, source, timeline_id, lsn, branch_timeline_id, branch_lsn
         FROM branch_schema_bases ORDER BY id",
    )
    .fetch_all(&mut *conn)
    .await
    .unwrap();
    // A history: a second entry for the same (branch, database) is allowed now.
    let appended: i64 = sqlx::query_scalar(
        "INSERT INTO branch_schema_bases (branch_id, database_name, source, timeline_id, lsn, branch_timeline_id, branch_lsn)
         VALUES ($1, 'app', 'merge', 'tdev', '0/3000000', 'tdev', '0/3000000') RETURNING id",
    )
    .bind(dev)
    .fetch_one(&mut *conn)
    .await
    .unwrap();
    let bad_lsn = sqlx::query(
        "INSERT INTO branch_schema_bases (branch_id, database_name, source, branch_timeline_id, branch_lsn)
         VALUES ($1, 'app', 'merge', 'tdev', 'not an lsn')",
    )
    .bind(dev)
    .execute(&mut *conn)
    .await;
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!("SET search_path TO public; DROP SCHEMA {schema} CASCADE;")))
        .execute(&mut *conn)
        .await
        .unwrap();

    let s = |v: &str| Some(v.to_string());
    let shape: Vec<_> = rows.iter().map(|r| (r.1, r.2.clone(), r.3.clone(), r.4.clone(), r.5.clone(), r.6.clone(), r.7.clone())).collect();
    assert_eq!(
        shape,
        vec![
            (dev, "app".to_string(), "create".to_string(), s("tread"), s("0/1000000"), s("tdev"), s("0/2000000")),
            (dev, "shop".to_string(), "reset".to_string(), s("tread"), s("0/1000000"), s("tdev"), s("0/2000000")),
            (main, "app".to_string(), "rebaseline".to_string(), None, None, s("tmain"), None),
        ]
    );
    assert!(rows.windows(2).all(|w| w[0].0 < w[1].0), "ids in the old rows' order");
    assert!(appended > rows[2].0);
    assert!(bad_lsn.is_err(), "branch_lsn must be an LSN");
}

// E18: a restore picks the newest entry on its timeline at or before its point, comparing LSNs
// as LSNs (0/A000 < 0/10000, though not as text).
#[tokio::test]
#[ignore]
async fn schema_base_at_or_before_picks_the_right_entry() {
    let _sealed = sealed_lock().await;
    let client = client().await;
    let (db, _ep) = ready_database(&client).await;
    let main = main_of(db);
    let (t, u) = (hex32(), hex32());
    let create = client.append_schema_base(&base_at(main, "app", BaseSource::Create, &t, "0/9000")).await.unwrap();
    let merge = client.append_schema_base(&base_at(main, "app", BaseSource::Merge, &t, "0/10000")).await.unwrap();
    // Newer by insertion, on another timeline and another database: never picked for (t, app).
    client.append_schema_base(&base_at(main, "app", BaseSource::Reset, &u, "0/1")).await.unwrap();
    client.append_schema_base(&base_at(main, "shop", BaseSource::Create, &t, "0/1")).await.unwrap();

    let mut picked = Vec::new();
    for lsn in ["0/8FFF", "0/9000", "0/A000", "0/10000", "1/0"] {
        picked.push(client.schema_base_at(main, "app", &t, lsn).await.unwrap().map(|b| b.id));
    }
    let [before_any, at_create, between, at_merge, later] = picked[..] else { unreachable!() };
    let other_timeline = client.schema_base_at(main, "app", &hex32(), "1/0").await.unwrap();
    cleanup(&client, db).await;

    assert_eq!(before_any, None);
    assert_eq!(at_create, Some(create));
    assert_eq!(between, Some(create), "0/A000 is before 0/10000");
    assert_eq!(at_merge, Some(merge));
    assert_eq!(later, Some(merge));
    assert!(other_timeline.is_none());
}

// E18: the restore's switch appends, per database, a copy of the newest entry on the old
// timeline at or before the restore point, positioned at the restored timeline's start.
#[tokio::test]
#[ignore]
async fn restore_switch_copies_the_base_at_or_before_the_point() {
    let _sealed = sealed_lock().await;
    let client = client().await;
    let (db, _ep) = ready_database(&client).await;
    let dev = ready_branch(&client, db, "dev", main_of(db)).await;
    let old = branch(&client, dev).await.timeline_id.unwrap();
    let mut create = base_at(dev, "app", BaseSource::Create, &old, "0/9000");
    create.model = Some(serde_json::json!({"tables": ["before"]}));
    create.identity_map = serde_json::json!({"pairs": [7]});
    let create_id = client.append_schema_base(&create).await.unwrap();
    client.append_schema_base(&base_at(dev, "app", BaseSource::Merge, &old, "0/10000")).await.unwrap();
    client.append_schema_base(&base_at(dev, "shop", BaseSource::Create, &old, "0/9000")).await.unwrap();
    // Only after the point: nothing to copy for it.
    client.append_schema_base(&base_at(dev, "late", BaseSource::Create, &old, "0/20000")).await.unwrap();
    let original = client.schema_base_at(dev, "app", &old, "0/9000").await.unwrap().unwrap();

    let pending = start_reset(&client, dev).await;
    let backup = BranchId::new();
    let switched = client
        .switch_to_restored_timeline(dev, &pending, backup, "dev-before-restore-20260930T120000Z", "0/A000")
        .await
        .unwrap();
    let app = client.schema_base(dev, "app").await.unwrap().unwrap();
    let shop = client.schema_base(dev, "shop").await.unwrap().unwrap();
    let late = client.schema_base(dev, "late").await.unwrap().unwrap();
    let entries = base_entries(&client, dev).await;
    cleanup(&client, db).await;

    assert!(switched);
    assert!(app.id > create_id);
    assert_eq!(app.source, BaseSource::Create, "the copy keeps the original's source");
    assert_eq!((app.branch_timeline_id.as_deref(), app.branch_lsn.as_deref()), (Some(pending.as_str()), Some("0/A000")));
    assert_eq!((app.timeline_id.as_deref(), app.lsn.as_deref()), (Some(old.as_str()), Some("0/9000")), "its location");
    assert_eq!(app.model, original.model);
    assert_eq!(app.identity_map, original.identity_map);
    assert_eq!(app.taken_at, original.taken_at);
    assert_eq!(shop.branch_timeline_id.as_deref(), Some(pending.as_str()));
    assert_eq!(late.branch_timeline_id.as_deref(), Some(old.as_str()), "nothing appended");
    assert_eq!(entries.len(), 6);
}

fn new_merge(db: DatabaseId, source: BranchId, target: BranchId, name: &str) -> NewMerge {
    NewMerge {
        id: uuid::Uuid::new_v4(),
        database_id: db,
        source_branch_id: source,
        target_branch_id: target,
        direction: MergeDirection::IntoParent,
        database_name: name.into(),
        options: None,
        created_by: "tester".into(),
    }
}

async fn created_merge(client: &MetadataClient, m: &NewMerge) -> chelabase_metadata::MergeRecord {
    match client.create_merge(m).await.unwrap() {
        MergeCreate::Created(r) => r,
        other => panic!("expected Created, got {other:?}"),
    }
}

/// Moves a merge through `statuses`, each from the one before.
async fn walk_merge(client: &MetadataClient, id: uuid::Uuid, from: MergeStatus, statuses: &[MergeStatus]) {
    let mut prev = from;
    for to in statuses {
        assert!(client.set_merge_status(id, &[prev], *to, MergeStatusPatch::default()).await.unwrap(), "{prev:?} -> {to:?}");
        prev = *to;
    }
}

#[tokio::test]
#[ignore]
async fn one_open_merge_per_pair() {
    let _sealed = sealed_lock().await;
    let client = client().await;
    let (db, _ep) = ready_database(&client).await;
    let main = main_of(db);
    let dev = ready_branch(&client, db, "dev", main).await;

    let first = new_merge(db, dev, main, "app");
    let created = created_merge(&client, &first).await;
    let second = client.create_merge(&new_merge(db, dev, main, "app")).await.unwrap();
    let other_db = client.create_merge(&new_merge(db, dev, main, "other")).await.unwrap();
    assert!(client.set_merge_status(first.id, &[MergeStatus::Computing], MergeStatus::Cancelled, MergeStatusPatch::default()).await.unwrap());
    let after_cancel = client.create_merge(&new_merge(db, dev, main, "app")).await.unwrap();
    let listed = client.list_merges(db, Some(dev)).await.unwrap();
    let listed_other = client.list_merges(db, Some(BranchId::new())).await.unwrap();
    cleanup(&client, db).await;

    assert_eq!((created.status, created.version), (MergeStatus::Computing, 1));
    assert_eq!(created.options, serde_json::json!({"lock_timeout_ms": 5000, "after": "keep"}));
    assert_eq!(second, MergeCreate::OpenExists(first.id));
    assert!(matches!(other_db, MergeCreate::Created(_)));
    assert!(matches!(after_cancel, MergeCreate::Created(_)));
    assert_eq!(listed.len(), 3);
    assert!(listed_other.is_empty());
}

#[tokio::test]
#[ignore]
async fn one_applying_merge_per_target() {
    let _sealed = sealed_lock().await;
    let client = client().await;
    let (db, _ep) = ready_database(&client).await;
    let main = main_of(db);
    let dev = ready_branch(&client, db, "dev", main).await;
    let dev2 = ready_branch(&client, db, "dev2", main).await;

    let a = created_merge(&client, &new_merge(db, dev, main, "app")).await;
    let b = created_merge(&client, &new_merge(db, dev2, main, "app")).await;
    walk_merge(
        &client,
        a.id,
        MergeStatus::Computing,
        &[MergeStatus::Draft, MergeStatus::Rehearsing, MergeStatus::Rehearsed, MergeStatus::Applying],
    )
    .await;
    walk_merge(&client, b.id, MergeStatus::Computing, &[MergeStatus::Draft, MergeStatus::Rehearsing, MergeStatus::Rehearsed]).await;
    let second_applying = client
        .set_merge_status(b.id, &[MergeStatus::Rehearsed], MergeStatus::Applying, MergeStatusPatch::default())
        .await;
    let blocked_create = client.create_merge(&new_merge(db, dev2, main, "again")).await.unwrap();
    // The first one finishes: the target is free again.
    walk_merge(&client, a.id, MergeStatus::Applying, &[MergeStatus::Online, MergeStatus::Finishing, MergeStatus::Done]).await;
    let b_applies = client
        .set_merge_status(b.id, &[MergeStatus::Rehearsed], MergeStatus::Applying, MergeStatusPatch::default())
        .await
        .unwrap();
    cleanup(&client, db).await;

    assert!(second_applying.is_err(), "merges_one_applying refuses a second applying merge");
    assert_eq!(blocked_create, MergeCreate::ApplyingExists(a.id));
    assert!(b_applies);
}

#[tokio::test]
#[ignore]
async fn draft_update_checks_version() {
    let _sealed = sealed_lock().await;
    let client = client().await;
    let (db, _ep) = ready_database(&client).await;
    let main = main_of(db);
    let dev = ready_branch(&client, db, "dev", main).await;
    let m = created_merge(&client, &new_merge(db, dev, main, "app")).await;

    let computing = client
        .update_merge_draft(m.id, 1, &MergeDraftPatch::default())
        .await
        .unwrap();
    walk_merge(&client, m.id, MergeStatus::Computing, &[MergeStatus::Draft]).await;
    let patch = MergeDraftPatch {
        resolutions: Some(serde_json::json!([{"item": "t", "take": "source"}])),
        ..Default::default()
    };
    let saved = client.update_merge_draft(m.id, 1, &patch).await.unwrap();
    let stale = client.update_merge_draft(m.id, 1, &MergeDraftPatch::default()).await.unwrap();
    let missing = client.update_merge_draft(uuid::Uuid::new_v4(), 1, &patch).await.unwrap();
    cleanup(&client, db).await;

    assert_eq!(computing, MergeWrite::NotEditable(MergeStatus::Computing));
    let MergeWrite::Saved(saved) = saved else { panic!("{saved:?}") };
    assert_eq!(saved.version, 2);
    assert_eq!(saved.resolutions, serde_json::json!([{"item": "t", "take": "source"}]));
    assert_eq!(saved.user_steps, serde_json::json!([]));
    assert_eq!(stale, MergeWrite::Stale(saved));
    assert_eq!(missing, MergeWrite::NotFound);
}

#[tokio::test]
#[ignore]
async fn saving_a_rehearsed_merge_returns_it_to_draft() {
    let _sealed = sealed_lock().await;
    let client = client().await;
    let (db, _ep) = ready_database(&client).await;
    let main = main_of(db);
    let dev = ready_branch(&client, db, "dev", main).await;
    let m = created_merge(&client, &new_merge(db, dev, main, "app")).await;
    walk_merge(&client, m.id, MergeStatus::Computing, &[MergeStatus::Draft, MergeStatus::Rehearsing]).await;
    let patch = MergeStatusPatch { rehearsal: Some(serde_json::json!({"ok": true})), ..Default::default() };
    assert!(client.set_merge_status(m.id, &[MergeStatus::Rehearsing], MergeStatus::Rehearsed, patch).await.unwrap());
    let rehearsed = client.merge(m.id).await.unwrap().unwrap();

    let saved = client
        .update_merge_draft(
            m.id,
            rehearsed.version,
            &MergeDraftPatch { user_steps: Some(serde_json::json!(["SELECT 1"])), ..Default::default() },
        )
        .await
        .unwrap();
    cleanup(&client, db).await;

    assert_eq!((rehearsed.status, rehearsed.rehearsal.is_some()), (MergeStatus::Rehearsed, true));
    let MergeWrite::Saved(saved) = saved else { panic!("{saved:?}") };
    assert_eq!(saved.status, MergeStatus::Draft);
    assert_eq!(saved.rehearsal, None);
    assert_eq!(saved.user_steps, serde_json::json!(["SELECT 1"]));
}

#[tokio::test]
#[ignore]
async fn saving_with_start_compute_moves_to_computing_in_one_statement() {
    let _sealed = sealed_lock().await;
    let client = client().await;
    let (db, _ep) = ready_database(&client).await;
    let main = main_of(db);
    let dev = ready_branch(&client, db, "dev", main).await;
    let m = created_merge(&client, &new_merge(db, dev, main, "app")).await;
    walk_merge(&client, m.id, MergeStatus::Computing, &[MergeStatus::Draft, MergeStatus::Rehearsing]).await;
    let patch = MergeStatusPatch { rehearsal: Some(serde_json::json!({"ok": true})), ..Default::default() };
    assert!(client.set_merge_status(m.id, &[MergeStatus::Rehearsing], MergeStatus::Rehearsed, patch).await.unwrap());
    let rehearsed = client.merge(m.id).await.unwrap().unwrap();

    let save = MergeDraftPatch {
        user_steps: Some(serde_json::json!([])),
        start_compute: true,
        ..Default::default()
    };
    let first = client.update_merge_draft(m.id, rehearsed.version, &save).await.unwrap();
    // A second save of the same version loses. The status check comes first, so it reports
    // `NotEditable(Computing)` (the row is no longer a draft), not `Stale`.
    let second = client.update_merge_draft(m.id, rehearsed.version, &save).await.unwrap();
    // The status is now computing: even the current version can't be saved over a running compute.
    let third = client.update_merge_draft(m.id, rehearsed.version + 1, &save).await.unwrap();
    cleanup(&client, db).await;

    let MergeWrite::Saved(first) = first else { panic!("{first:?}") };
    assert_eq!(first.status, MergeStatus::Computing);
    assert_eq!(first.rehearsal, None);
    assert!(first.claimed_at.is_some(), "the compute is claimed from the save");
    assert_eq!(first.version, rehearsed.version + 1);
    assert_eq!(second, MergeWrite::NotEditable(MergeStatus::Computing));
    assert_eq!(third, MergeWrite::NotEditable(MergeStatus::Computing));
}

#[tokio::test]
#[ignore]
async fn status_transition_is_guarded() {
    let _sealed = sealed_lock().await;
    let client = client().await;
    let (db, _ep) = ready_database(&client).await;
    let main = main_of(db);
    let dev = ready_branch(&client, db, "dev", main).await;
    let m = created_merge(&client, &new_merge(db, dev, main, "app")).await;

    let wrong_from = client.set_merge_status(m.id, &[MergeStatus::Draft], MergeStatus::Rehearsing, MergeStatusPatch::default()).await.unwrap();
    let unchanged = client.merge(m.id).await.unwrap().unwrap();
    let touched = client.touch_merge_claim(m.id, MergeStatus::Computing).await.unwrap();
    let touched_wrong = client.touch_merge_claim(m.id, MergeStatus::Draft).await.unwrap();
    let stale = client.stale_merge_claims(std::time::Duration::from_secs(3600), std::time::Duration::ZERO).await.unwrap();
    let stale_now = client.stale_merge_claims(std::time::Duration::ZERO, std::time::Duration::ZERO).await.unwrap();
    let patch = MergeStatusPatch {
        analysis: Some(serde_json::json!({"conflicts": []})),
        base_fingerprint: Some("b".into()),
        source_fingerprint: Some("s".into()),
        target_fingerprint: Some("t".into()),
        ..Default::default()
    };
    let moved = client.set_merge_status(m.id, &[MergeStatus::Computing, MergeStatus::Draft], MergeStatus::Draft, patch).await.unwrap();
    let draft = client.merge(m.id).await.unwrap().unwrap();
    let again = client.set_merge_status(m.id, &[MergeStatus::Computing], MergeStatus::Draft, MergeStatusPatch::default()).await.unwrap();
    let failed = MergeStatusPatch {
        error: Some(Some("boom".into())),
        failed_phase: Some(Some("phase1".into())),
        ..Default::default()
    };
    assert!(client.set_merge_status(m.id, &[MergeStatus::Draft], MergeStatus::Failed, failed).await.unwrap());
    let failed_row = client.merge(m.id).await.unwrap().unwrap();
    let unknown = client.set_merge_status(uuid::Uuid::new_v4(), &[MergeStatus::Computing], MergeStatus::Draft, MergeStatusPatch::default()).await.unwrap();
    cleanup(&client, db).await;

    assert!(!wrong_from);
    assert_eq!(unchanged.status, MergeStatus::Computing);
    assert!(unchanged.claimed_at.is_some());
    assert!(touched && !touched_wrong);
    assert!(stale.is_empty());
    assert!(stale_now.iter().any(|r| r.id == m.id));
    assert!(moved);
    assert_eq!(draft.status, MergeStatus::Draft);
    assert_eq!((draft.version, draft.claimed_at), (2, None), "analysis bumps the version; a draft isn't claimed");
    assert_eq!(draft.analysis, Some(serde_json::json!({"conflicts": []})));
    assert_eq!(
        (draft.base_fingerprint.as_deref(), draft.source_fingerprint.as_deref(), draft.target_fingerprint.as_deref()),
        (Some("b"), Some("s"), Some("t"))
    );
    assert!(!again);
    assert_eq!((failed_row.status, failed_row.error.as_deref(), failed_row.failed_phase.as_deref()), (MergeStatus::Failed, Some("boom"), Some("phase1")));
    assert!(!unknown);
}

// T8a: a recompute writes the resolutions it kept, and a JSON null clears the analysis and the
// plan (SQL NULL, not a jsonb `null`).
#[tokio::test]
#[ignore]
async fn status_patch_writes_resolutions_and_null_clears() {
    let _sealed = sealed_lock().await;
    let client = client().await;
    let (db, _ep) = ready_database(&client).await;
    let main = main_of(db);
    let dev = ready_branch(&client, db, "dev", main).await;
    let m = created_merge(&client, &new_merge(db, dev, main, "app")).await;

    let kept = serde_json::json!([{"conflict": "c1", "choice": "keep_target"}]);
    let first = MergeStatusPatch {
        analysis: Some(serde_json::json!({"conflicts": []})),
        plan: Some(serde_json::json!({"steps": [], "target_digest": "d"})),
        resolutions: Some(kept.clone()),
        ..Default::default()
    };
    assert!(client.set_merge_status(m.id, &[MergeStatus::Computing], MergeStatus::Computing, first).await.unwrap());
    let written = client.merge(m.id).await.unwrap().unwrap();
    let cleared = MergeStatusPatch {
        analysis: Some(serde_json::Value::Null),
        plan: Some(serde_json::Value::Null),
        ..Default::default()
    };
    assert!(client.set_merge_status(m.id, &[MergeStatus::Computing], MergeStatus::Draft, cleared).await.unwrap());
    let after = client.merge(m.id).await.unwrap().unwrap();
    cleanup(&client, db).await;

    assert_eq!(written.resolutions, kept);
    assert!(written.plan.is_some() && written.analysis.is_some());
    assert_eq!((after.analysis, after.plan), (None, None));
    assert_eq!(after.resolutions, kept, "untouched without a value");
}

#[tokio::test]
#[ignore]
async fn cancel_drafts_for_branch_leaves_applying_alone() {
    let _sealed = sealed_lock().await;
    let client = client().await;
    let (db, _ep) = ready_database(&client).await;
    let main = main_of(db);
    let dev = ready_branch(&client, db, "dev", main).await;
    let dev2 = ready_branch(&client, db, "dev2", main).await;
    let draft = created_merge(&client, &new_merge(db, dev, main, "app")).await;
    walk_merge(&client, draft.id, MergeStatus::Computing, &[MergeStatus::Draft]).await;
    let applying = created_merge(&client, &new_merge(db, dev2, main, "app")).await;
    walk_merge(
        &client,
        applying.id,
        MergeStatus::Computing,
        &[MergeStatus::Draft, MergeStatus::Rehearsing, MergeStatus::Rehearsed, MergeStatus::Applying],
    )
    .await;

    let cancelled = client.cancel_drafts_for_branch(main).await.unwrap();
    let draft_after = client.merge(draft.id).await.unwrap().unwrap();
    let applying_after = client.merge(applying.id).await.unwrap().unwrap();
    let nothing_left = client.cancel_drafts_for_branch(main).await.unwrap();
    cleanup(&client, db).await;

    assert_eq!(cancelled, 1);
    assert_eq!(draft_after.status, MergeStatus::Cancelled);
    assert_eq!(applying_after.status, MergeStatus::Applying);
    assert_eq!(nothing_left, 0);
}

#[tokio::test]
#[ignore]
async fn busy_merge_is_found_for_source_and_target() {
    let _sealed = sealed_lock().await;
    let client = client().await;
    let (db, _ep) = ready_database(&client).await;
    let main = main_of(db);
    let dev = ready_branch(&client, db, "dev", main).await;
    let other = ready_branch(&client, db, "other", main).await;
    let m = created_merge(&client, &new_merge(db, dev, main, "app")).await;
    walk_merge(&client, m.id, MergeStatus::Computing, &[MergeStatus::Draft]).await;

    let idle = (
        client.branch_has_busy_merge(dev).await.unwrap(),
        client.branch_has_busy_merge(main).await.unwrap(),
    );
    walk_merge(&client, m.id, MergeStatus::Draft, &[MergeStatus::Rehearsing]).await;
    let rehearsing = (
        client.branch_has_busy_merge(dev).await.unwrap(),
        client.branch_has_busy_merge(main).await.unwrap(),
        client.branch_has_busy_merge(other).await.unwrap(),
    );
    walk_merge(&client, m.id, MergeStatus::Rehearsing, &[MergeStatus::Rehearsed, MergeStatus::Applying]).await;
    let applying = client.branch_has_busy_merge(main).await.unwrap();
    walk_merge(&client, m.id, MergeStatus::Applying, &[MergeStatus::Online, MergeStatus::Finishing, MergeStatus::Done]).await;
    let done = client.branch_has_busy_merge(main).await.unwrap();
    cleanup(&client, db).await;

    assert_eq!(idle, (None, None));
    assert_eq!(rehearsing, (Some(BranchBusy::Merge(m.id)), Some(BranchBusy::Merge(m.id)), None));
    assert_eq!(applying, Some(BranchBusy::Merge(m.id)));
    assert_eq!(done, None);
}

/// Puts merge `id` in `state` (from any status), as the worker and the API leave it.
async fn put_merge_in(client: &MetadataClient, id: uuid::Uuid, state: MergeState) {
    let patch = MergeStatusPatch {
        failed_phase: Some(state.failed_phase.map(str::to_string)),
        ..Default::default()
    };
    assert!(client.set_merge_status(id, &MergeStatus::ALL, state.status, patch).await.unwrap(), "{state:?}");
}

// T14 F3 (E27): the SQL of the unique indexes, the create's checks and the busy check say what
// `MergeState` says, in every state.
#[tokio::test]
#[ignore]
async fn merge_state_sql_matches_the_table() {
    let _sealed = sealed_lock().await;
    let client = client().await;
    let (db, _ep) = ready_database(&client).await;
    let main = main_of(db);
    let dev = ready_branch(&client, db, "dev", main).await;
    let dev2 = ready_branch(&client, db, "dev2", main).await;
    let mut seen = Vec::new();
    for state in MergeState::all() {
        let m = created_merge(&client, &new_merge(db, dev, main, "app")).await;
        put_merge_in(&client, m.id, state).await;
        let busy = (
            client.branch_has_busy_merge(dev).await.unwrap(),
            client.branch_has_busy_merge(main).await.unwrap(),
        );
        let same_pair = client.create_merge(&new_merge(db, dev, main, "app")).await.unwrap();
        let other_source = client.create_merge(&new_merge(db, dev2, main, "app")).await.unwrap();
        seen.push((state, busy, same_pair.clone(), other_source.clone()));
        // Out of the way for the next state.
        for created in [same_pair, other_source] {
            if let MergeCreate::Created(r) = created {
                put_merge_in(&client, r.id, MergeState::of(MergeStatus::Cancelled, None)).await;
            }
        }
        put_merge_in(&client, m.id, MergeState::of(MergeStatus::Cancelled, None)).await;
    }
    cleanup(&client, db).await;

    for (state, (dev_busy, main_busy), same_pair, other_source) in seen {
        let held = state.holds_branches().then_some(());
        assert_eq!(dev_busy.map(|_| ()), held, "{state:?} source");
        assert_eq!(main_busy.map(|_| ()), held, "{state:?} target");
        let expect_pair = if state.is_applying() {
            "applying"
        } else if state.is_open() {
            "open"
        } else {
            "created"
        };
        let got = |c: &MergeCreate| match c {
            MergeCreate::ApplyingExists(_) => "applying",
            MergeCreate::OpenExists(_) => "open",
            MergeCreate::Created(_) => "created",
        };
        assert_eq!(got(&same_pair), expect_pair, "{state:?} same pair");
        let expect_other = if state.is_applying() { "applying" } else { "created" };
        assert_eq!(got(&other_source), expect_other, "{state:?} another merge into the target");
    }
}

// T14 F3 (I4, M12): the claim refuses a branch a merge holds, or one with a live rehearsal
// branch, under the branch's row lock; nothing is written.
#[tokio::test]
#[ignore]
async fn claim_refuses_a_branch_a_merge_holds() {
    let _sealed = sealed_lock().await;
    let client = client().await;
    let (db, _ep) = ready_database(&client).await;
    let main = main_of(db);
    let dev = ready_branch(&client, db, "dev", main).await;
    let m = created_merge(&client, &new_merge(db, dev, main, "app")).await;
    put_merge_in(&client, m.id, MergeState::of(MergeStatus::Failed, Some("online"))).await;
    let held = client.claim_branch_for_switch(dev, false).await.unwrap();
    let dev_after = client.get_branch(dev).await.unwrap().unwrap().status;
    put_merge_in(&client, m.id, MergeState::of(MergeStatus::Cancelled, None)).await;
    let rehearsal = client.create_rehearsal_branch(db, dev, "_rehearsal-0badc0de", "tester").await.unwrap();
    let with_rehearsal = client.claim_branch_for_switch(dev, false).await.unwrap();
    client.mark_branch_deleted(rehearsal.id).await.unwrap();
    let free = client.claim_branch_for_switch(dev, false).await.unwrap();
    cleanup(&client, db).await;

    assert_eq!(held, ClaimOutcome::MergeBusy(BranchBusy::Merge(m.id)));
    assert_eq!(dev_after, BranchStatus::Ready, "nothing written");
    assert_eq!(with_rehearsal, ClaimOutcome::MergeBusy(BranchBusy::Rehearsal(rehearsal.id)));
    assert_eq!(free, ClaimOutcome::Claimed);
}

// T14 F3: the status write's guards (failed phase, version, both branches ready) and its log line
// in the same transaction (I2).
#[tokio::test]
#[ignore]
async fn status_write_guards_and_logs() {
    let _sealed = sealed_lock().await;
    let client = client().await;
    let (db, _ep) = ready_database(&client).await;
    let main = main_of(db);
    let dev = ready_branch(&client, db, "dev", main).await;
    let m = created_merge(&client, &new_merge(db, dev, main, "app")).await;
    put_merge_in(&client, m.id, MergeState::of(MergeStatus::Failed, Some("tx"))).await;
    let to_applying = |phase: &str| MergeStatusPatch {
        guard_failed_phase: Some(Some(phase.to_string())),
        guard_branches_ready: true,
        log_status: true,
        ..Default::default()
    };
    let wrong_phase = client.set_merge_status(m.id, &[MergeStatus::Failed], MergeStatus::Applying, to_applying("online")).await.unwrap();
    client.set_branch_status(dev, &[BranchStatus::Ready], BranchStatus::Resetting).await.unwrap();
    let not_ready = client.set_merge_status(m.id, &[MergeStatus::Failed], MergeStatus::Applying, to_applying("tx")).await.unwrap();
    client.set_branch_status(dev, &[BranchStatus::Resetting], BranchStatus::Ready).await.unwrap();
    let events_before = client.merge_events(m.id).await.unwrap().len();
    let moved = client.set_merge_status(m.id, &[MergeStatus::Failed], MergeStatus::Applying, to_applying("tx")).await.unwrap();
    let events = client.merge_events(m.id).await.unwrap();
    let version = client.merge(m.id).await.unwrap().unwrap().version;
    put_merge_in(&client, m.id, MergeState::of(MergeStatus::Draft, None)).await;
    let stale_version = MergeStatusPatch { guard_version: Some(version + 1), ..Default::default() };
    let wrong_version = client.set_merge_status(m.id, &[MergeStatus::Draft], MergeStatus::Rehearsing, stale_version).await.unwrap();
    let right_version = MergeStatusPatch { guard_version: Some(version), ..Default::default() };
    let right = client.set_merge_status(m.id, &[MergeStatus::Draft], MergeStatus::Rehearsing, right_version).await.unwrap();
    cleanup(&client, db).await;

    assert!(!wrong_phase && !not_ready && moved);
    assert_eq!(events_before, 0, "a refused write logs nothing");
    assert_eq!(events.len(), 1);
    assert_eq!((events[0].kind.as_str(), events[0].detail.clone()), ("status", Some(serde_json::json!({"to": "applying"}))));
    assert!(!wrong_version && right);
}

// T14 M10, M8: a merge handed back isn't handed back again while its job may be queued; a lost
// Finish request is found.
#[tokio::test]
#[ignore]
async fn hand_backs_wait_for_their_job() {
    let _sealed = sealed_lock().await;
    let client = client().await;
    let (db, _ep) = ready_database(&client).await;
    let main = main_of(db);
    let dev = ready_branch(&client, db, "dev", main).await;
    let m = created_merge(&client, &new_merge(db, dev, main, "app")).await;
    let hour = std::time::Duration::from_secs(3600);
    let zero = std::time::Duration::ZERO;
    let stale = client.stale_merge_claims(zero, hour).await.unwrap();
    let marked = client.mark_merge_handed_back(m.id, MergeStatus::Computing).await.unwrap();
    let after_hand_back = client.stale_merge_claims(zero, hour).await.unwrap();
    let after_window = client.stale_merge_claims(zero, zero).await.unwrap();
    // The worker claims it again: it can be handed back once more when it goes silent.
    assert!(client.touch_merge_claim(m.id, MergeStatus::Computing).await.unwrap());
    let after_claim = client.stale_merge_claims(zero, hour).await.unwrap();
    let requested = MergeStatusPatch {
        result: Some(serde_json::json!({"after": {"choice": "keep", "requested": true}})),
        ..Default::default()
    };
    assert!(client.set_merge_status(m.id, &MergeStatus::ALL, MergeStatus::AppliedWithDrift, requested).await.unwrap());
    let lost = client.stale_finish_requests(zero, hour).await.unwrap();
    let recent = client.stale_finish_requests(hour, hour).await.unwrap();
    // T14 F3 M2: handed back, it waits out the window (unless written since).
    assert!(client.mark_merge_handed_back(m.id, MergeStatus::AppliedWithDrift).await.unwrap());
    let finish_handed_back = client.stale_finish_requests(zero, hour).await.unwrap();
    let finish_after_window = client.stale_finish_requests(zero, zero).await.unwrap();
    // T14 F3 M1: a run that recorded `started` and went away is found too.
    let started = MergeStatusPatch {
        result: Some(serde_json::json!({"after": {"choice": "reset", "started": true, "timeline": null}})),
        ..Default::default()
    };
    assert!(client.set_merge_status(m.id, &[MergeStatus::AppliedWithDrift], MergeStatus::AppliedWithDrift, started).await.unwrap());
    let lost_started = client.stale_finish_requests(zero, hour).await.unwrap();
    cleanup(&client, db).await;

    let has = |rows: &[chelabase_metadata::MergeRecord]| rows.iter().any(|r| r.id == m.id);
    assert!(has(&stale) && marked);
    assert!(!has(&after_hand_back), "handed back within the window");
    assert!(has(&after_window));
    assert!(has(&after_claim));
    assert!(has(&lost) && !has(&recent));
    assert!(!has(&finish_handed_back) && has(&finish_after_window));
    assert!(has(&lost_started), "a write since the hand-back re-arms it");
}

// T14 F3 fix round (I1, the hold invariant on every path): the claim (reset, restore, the
// worker's after-merge reset) and the delete's begin (the API's, the worker's delete job and its
// after-merge delete) refuse a branch a merge holds as its source or its target, in exactly the
// states that hold, under the branch's row lock with nothing written; a merge named as the one
// that doesn't count (the finishing merge's own hold) never refuses.
#[tokio::test]
#[ignore]
async fn every_branch_path_follows_the_hold_table() {
    let _sealed = sealed_lock().await;
    let client = client().await;
    let (db, _ep) = ready_database(&client).await;
    let main = main_of(db);
    let mut seen = Vec::new();
    for (i, state) in MergeState::all().into_iter().enumerate() {
        for dev_is_source in [true, false] {
            let dev = ready_branch(&client, db, &format!("d{i}{dev_is_source}"), main).await;
            let m = if dev_is_source {
                new_merge(db, dev, main, "app")
            } else {
                NewMerge { direction: MergeDirection::FromParent, ..new_merge(db, main, dev, "app") }
            };
            let m = created_merge(&client, &m).await;
            put_merge_in(&client, m.id, state).await;
            let claim = client.claim_branch_for_switch(dev, false).await.unwrap();
            client.set_branch_status(dev, &[BranchStatus::Resetting], BranchStatus::Ready).await.unwrap();
            let claim_except = client.claim_branch_for_switch_except(dev, false, Some(m.id)).await.unwrap();
            client.set_branch_status(dev, &[BranchStatus::Resetting], BranchStatus::Ready).await.unwrap();
            let delete = client.begin_branch_delete(dev).await.unwrap();
            let status_after = client.get_branch(dev).await.unwrap().unwrap().status;
            let delete_except = if delete == BranchDeleteOutcome::Started {
                // Already moved: the next call has nothing left to move.
                BranchDeleteOutcome::Started
            } else {
                client.begin_branch_delete_except(dev, Some(m.id)).await.unwrap()
            };
            seen.push((state, dev_is_source, m.id, claim, claim_except, delete, status_after, delete_except));
            put_merge_in(&client, m.id, MergeState::of(MergeStatus::Cancelled, None)).await;
            client.mark_branch_deleted(dev).await.unwrap();
        }
    }
    cleanup(&client, db).await;

    for (state, dev_is_source, id, claim, claim_except, delete, status_after, delete_except) in seen {
        let at = format!("{state:?} dev_is_source={dev_is_source}");
        if state.holds_branches() {
            assert_eq!(claim, ClaimOutcome::MergeBusy(BranchBusy::Merge(id)), "{at}");
            assert_eq!(delete, BranchDeleteOutcome::MergeBusy(BranchBusy::Merge(id)), "{at}");
            assert_eq!(status_after, BranchStatus::Ready, "{at}: nothing written");
        } else {
            assert_eq!(claim, ClaimOutcome::Claimed, "{at}");
            assert_eq!(delete, BranchDeleteOutcome::Started, "{at}");
        }
        assert_eq!(claim_except, ClaimOutcome::Claimed, "{at}: its own hold");
        assert_eq!(delete_except, BranchDeleteOutcome::Started, "{at}: its own hold");
    }
}

// E34: a save of acknowledgements or the after-merge choice keeps a rehearsed merge rehearsed.
#[tokio::test]
#[ignore]
async fn a_keep_plan_save_keeps_the_rehearsal() {
    let _sealed = sealed_lock().await;
    let client = client().await;
    let (db, _ep) = ready_database(&client).await;
    let main = main_of(db);
    let dev = ready_branch(&client, db, "dev", main).await;
    let m = created_merge(&client, &new_merge(db, dev, main, "app")).await;
    let patch = MergeStatusPatch { rehearsal: Some(serde_json::json!({"ok": true})), ..Default::default() };
    assert!(client.set_merge_status(m.id, &MergeStatus::ALL, MergeStatus::Rehearsed, patch).await.unwrap());
    let rehearsed = client.merge(m.id).await.unwrap().unwrap();
    let save = MergeDraftPatch {
        acknowledgements: Some(serde_json::json!(["x"])),
        keep_plan: true,
        ..Default::default()
    };
    let saved = client.update_merge_draft(m.id, rehearsed.version, &save).await.unwrap();
    cleanup(&client, db).await;

    let MergeWrite::Saved(saved) = saved else { panic!("{saved:?}") };
    assert_eq!(saved.status, MergeStatus::Rehearsed);
    assert_eq!(saved.rehearsal, Some(serde_json::json!({"ok": true})));
    assert_eq!(saved.acknowledgements, serde_json::json!(["x"]));
    assert_eq!(saved.version, rehearsed.version + 1);
}

#[tokio::test]
#[ignore]
async fn merge_events_in_order() {
    let _sealed = sealed_lock().await;
    let client = client().await;
    let (db, _ep) = ready_database(&client).await;
    let main = main_of(db);
    let dev = ready_branch(&client, db, "dev", main).await;
    let m = created_merge(&client, &new_merge(db, dev, main, "app")).await;

    let mut status = MergeEvent::new(m.id, "status");
    status.detail = Some(serde_json::json!({"to": "draft"}));
    let mut step = MergeEvent::new(m.id, "step");
    step.step_id = Some("s1".into());
    step.phase = Some("phase1".into());
    step.outcome = Some("ok".into());
    step.duration_ms = Some(42);
    let note = MergeEvent::new(m.id, "note");
    for e in [&status, &step, &note] {
        client.append_merge_event(e).await.unwrap();
    }
    let events = client.merge_events(m.id).await.unwrap();
    let none = client.merge_events(uuid::Uuid::new_v4()).await.unwrap();
    cleanup(&client, db).await;

    assert_eq!(events.iter().map(|e| e.kind.as_str()).collect::<Vec<_>>(), ["status", "step", "note"]);
    assert!(events.windows(2).all(|w| w[0].id < w[1].id));
    assert_eq!(events[0].detail, status.detail);
    assert_eq!(
        (events[1].step_id.as_deref(), events[1].phase.as_deref(), events[1].outcome.as_deref(), events[1].duration_ms),
        (Some("s1"), Some("phase1"), Some("ok"), Some(42))
    );
    assert!(none.is_empty());
}

#[tokio::test]
#[ignore]
async fn deleting_a_branch_cascades_its_bases() {
    let _sealed = sealed_lock().await;
    let client = client().await;
    let (db, _ep) = ready_database(&client).await;
    let main = main_of(db);
    let dev = ready_branch(&client, db, "dev", main).await;
    client.append_schema_base(&base(dev, BaseSource::Create, None)).await.unwrap();
    client.append_schema_base(&base(main, BaseSource::Create, None)).await.unwrap();
    let m = created_merge(&client, &new_merge(db, dev, main, "app")).await;
    client.append_merge_event(&MergeEvent::new(m.id, "note")).await.unwrap();

    sqlx::query("DELETE FROM branches WHERE id = $1").bind(dev.as_uuid()).execute(client.pool()).await.unwrap();
    let dev_base = client.schema_base(dev, "app").await.unwrap();
    let main_base = client.schema_base(main, "app").await.unwrap();
    let merge = client.merge(m.id).await.unwrap();
    let events = client.merge_events(m.id).await.unwrap();
    cleanup(&client, db).await;

    assert!(dev_base.is_none());
    assert!(main_base.is_some(), "the parent's base stays");
    assert!(merge.is_none(), "a merge of a deleted branch goes with it");
    assert!(events.is_empty());
}

// E1/P4: rehearsal and static endpoints are unroutable; the proxy lookups skip them.
#[tokio::test]
#[ignore]
async fn routable_lookup_skips_unroutable_endpoints() {
    let client = client().await;
    let id = insert_pending(&client).await;
    let ep = link_endpoint(&client, id).await;

    let found = client.routable_endpoint(&ep).await.unwrap();
    let secret = client.routable_role_secret(&ep, "postgres").await.unwrap();
    assert_eq!(found.as_ref().map(|e| (e.routable, e.static_lsn.clone())), Some((true, None)));
    assert_eq!(secret, Some(Some("SCRAM-SHA-256$v1".to_string())));

    sqlx::query("UPDATE neon_endpoints SET routable = false WHERE id = $1")
        .bind(&ep)
        .execute(client.pool())
        .await
        .unwrap();
    let found_after = client.routable_endpoint(&ep).await.unwrap();
    let secret_after = client.routable_role_secret(&ep, "postgres").await.unwrap();
    let plain = client.neon_endpoint(&ep).await.unwrap().unwrap();

    // An endpoint created unroutable and pinned records both columns.
    let id2 = insert_pending(&client).await;
    let ep2 = ep_id();
    let (tenant, timeline) = (hex32(), hex32());
    let mut new = new_endpoint(&ep2, &tenant, &timeline);
    new.routable = false;
    new.static_lsn = Some("0/16B3748");
    assert!(client.create_endpoint_for_branch(main_of(id2), &timeline, &new, false).await.unwrap());
    let pinned = client.neon_endpoint(&ep2).await.unwrap().unwrap();
    let pinned_routable = client.routable_endpoint(&ep2).await.unwrap();

    cleanup(&client, id).await;
    cleanup(&client, id2).await;
    assert_eq!(found_after, None);
    assert_eq!(secret_after, None);
    assert!(!plain.routable, "the plain lookup still sees it");
    assert!(!pinned.routable);
    assert_eq!(pinned.static_lsn.as_deref(), Some("0/16B3748"));
    assert_eq!(pinned_routable, None);
}

// T7: a base's static compute gets an unroutable, pinned endpoint row linked to no branch, and
// reconcile sees how old it is (R3).
#[tokio::test]
#[ignore]
async fn static_endpoint_is_unroutable_and_reconcile_sees_its_age() {
    let client = client().await;
    let (id, linked) = ready_database(&client).await;
    let ep = ep_id();
    let (tenant, timeline) = (hex32(), hex32());
    client
        .create_static_endpoint(&NewStaticEndpoint {
            id: &ep,
            tenant_id: &tenant,
            timeline_id: &timeline,
            compute_token_sha256: "aa",
            static_lsn: "0/2000000",
        })
        .await
        .unwrap();
    sqlx::query("UPDATE neon_endpoints SET created_at = now() - interval '11 minutes' WHERE id = $1")
        .bind(&ep)
        .execute(client.pool())
        .await
        .unwrap();

    let found = client.neon_endpoint(&ep).await.unwrap();
    let routed = client.routable_endpoint(&ep).await.unwrap();
    let roles = client.neon_roles(&ep).await.unwrap();
    let all = client.list_endpoints_for_reconcile().await.unwrap();
    let find = |id: &str| all.iter().find(|e| e.id == id).cloned().unwrap();
    let (fixed, normal) = (find(&ep), find(&linked));
    client.delete_endpoint(&ep).await.unwrap();
    let gone = client.neon_endpoint(&ep).await.unwrap();
    cleanup(&client, id).await;

    let found = found.unwrap();
    assert!(!found.routable);
    assert_eq!(found.static_lsn.as_deref(), Some("0/2000000"));
    assert_eq!(found.timeline_id, timeline);
    assert_eq!(routed, None);
    assert!(roles.is_empty());
    assert_eq!(fixed.branch_id, None);
    assert!(!fixed.routable);
    assert_eq!(fixed.static_lsn.as_deref(), Some("0/2000000"));
    assert!(fixed.age_secs >= 600, "{}", fixed.age_secs);
    assert!(normal.routable);
    assert_eq!(normal.static_lsn, None);
    assert!(normal.age_secs < 600, "{}", normal.age_secs);
    assert_eq!(gone, None);
}

// ---- secret keys vs role DROP / recreate (T13h) ----

/// How long a call that must be waiting on the endpoint lock is given to finish (it must not).
const STILL_BLOCKED: std::time::Duration = std::time::Duration::from_millis(400);

async fn key_rows(client: &MetadataClient, branch: BranchId, role: &str) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM data_api_keys WHERE branch_id = $1 AND role = $2")
        .bind(branch.as_uuid())
        .bind(role)
        .fetch_one(client.pool())
        .await
        .unwrap()
}

/// A secret key bound to role OID `oid`.
fn bound_key(branch: BranchId, role: &str, name: &str, oid: Option<i64>) -> NewDataKey {
    let (key, _) = new_key(branch, DataKeyKind::Secret, role, name);
    NewDataKey { role_oid: oid, ..key }
}

/// A secret key bound to role OID `oid` read on `timeline` (E26).
fn bound_on(branch: BranchId, role: &str, name: &str, oid: i64, timeline: &str) -> NewDataKey {
    NewDataKey { role_timeline: Some(timeline.into()), ..bound_key(branch, role, name, Some(oid)) }
}

const TL1: &str = "11111111111111111111111111111111";
const TL2: &str = "22222222222222222222222222222222";

// E26: a secret key stores the timeline its OID was read on; it round-trips through the list,
// carries over a rotation on the same timeline, and is re-stamped by a rotation checked on
// another timeline under the same name (refused under another name). The column's CHECK
// refuses a timeline without an OID.
#[tokio::test]
#[ignore]
async fn secret_key_role_timeline_round_trips() {
    let _sealed = sealed_lock().await;
    let client = client().await;
    let (id, _) = ready_database(&client).await;
    let branch = main_of(id);
    let key = bound_on(branch, "app", "tl", 16390, TL1);
    let created = client.create_secret_key(&key).await;
    let listed = client.list_data_keys(branch).await.unwrap();
    let same = bound_on(branch, "app", "tl", 16390, TL1);
    let rotated = client.rotate_secret_key(branch, key.id, &same).await;
    let switched = bound_on(branch, "app", "tl", 16390, TL2);
    let restamped = client.rotate_secret_key(branch, same.id, &switched).await;
    let other_name = bound_on(branch, "x", "tl", 16390, TL1);
    let refused = client.rotate_secret_key(branch, switched.id, &other_name).await;
    let after = client.list_data_keys(branch).await.unwrap();
    let orphan = NewDataKey { role_timeline: Some(TL1.into()), ..bound_key(branch, "app", "orphan", None) };
    let check = client.create_secret_key(&orphan).await;
    cleanup(&client, id).await;
    assert!(created.is_ok(), "{created:?}");
    let identity = |keys: &[chelabase_metadata::DataKeyRecord], id| {
        keys.iter().find(|k| k.id == id).map(|k| (k.role_oid, k.role_timeline.clone()))
    };
    assert_eq!(identity(&listed, key.id), Some((Some(16390), Some(TL1.to_string()))));
    assert!(matches!(rotated, Ok(true)), "{rotated:?}");
    assert!(matches!(restamped, Ok(true)), "{restamped:?}");
    assert!(matches!(&refused, Err(MetadataError::RoleRecreated(r)) if r == "app"), "{refused:?}");
    assert_eq!(identity(&after, switched.id), Some((Some(16390), Some(TL2.to_string()))));
    assert!(check.is_err(), "a timeline without an OID: {check:?}");
}

// C1: the worker checks the role on the compute; the store needs no `neon_roles` row (a
// NOLOGIN or password-less role has none) and keeps the role's OID, which the lookup returns.
#[tokio::test]
#[ignore]
async fn create_secret_key_stores_a_rowless_role_with_its_oid() {
    let _sealed = sealed_lock().await;
    let client = client().await;
    let (id, _) = ready_database(&client).await;
    let branch = main_of(id);
    let key = bound_key(branch, "reader", "rowless", Some(16390));
    let created = client.create_secret_key(&key).await;
    let listed = client.list_data_keys(branch).await.unwrap();
    let looked_up = client.lookup_data_key(&key.sha256).await.unwrap();
    cleanup(&client, id).await;
    assert!(created.is_ok(), "{created:?}");
    assert_eq!(listed.iter().find(|k| k.id == key.id).unwrap().role_oid, Some(16390));
    assert_eq!(looked_up.unwrap().role_oid, Some(16390));
}

// The receiver's transaction holds the endpoint row: a key creation waits for it, so a DROP
// forwarded first runs first (the key is then bound to an OID that is gone: dead), and one
// forwarded after sees the key.
#[tokio::test]
#[ignore]
async fn create_secret_key_waits_for_the_ddl_transaction() {
    let _sealed = sealed_lock().await;
    let client = client().await;
    let (id, ep) = ready_database(&client).await;
    let branch = main_of(id);
    let mut ddl = client.begin_ddl(&ep).await.unwrap().expect("endpoint");
    assert!(ddl.live_secret_keys("app").await.unwrap().is_empty());

    let key = bound_key(branch, "app", "late", Some(16390));
    let c = client.clone();
    let k = key.clone();
    let mut creating = tokio::spawn(async move { c.create_secret_key(&k).await });
    let waited = tokio::time::timeout(STILL_BLOCKED, &mut creating).await;
    assert!(waited.is_err(), "key creation must wait for the DDL transaction");

    ddl.commit().await.unwrap();
    let result = creating.await.unwrap();
    let rows = key_rows(&client, branch, "app").await;
    cleanup(&client, id).await;
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(rows, 1);
}

// Rotation keeps the identity: the old key's OID carries over; a different checked OID (the
// role dropped and created again) is refused with the revoke rolled back; a key without an
// OID takes the checked one.
#[tokio::test]
#[ignore]
async fn rotate_secret_key_holds_the_role_oid() {
    let _sealed = sealed_lock().await;
    let client = client().await;
    let (id, _) = ready_database(&client).await;
    let branch = main_of(id);
    let old = bound_key(branch, "app", "old", Some(16390));
    client.create_secret_key(&old).await.unwrap();
    let recreated = bound_key(branch, "app", "old", Some(16391));
    let refused = client.rotate_secret_key(branch, old.id, &recreated).await;
    let after_refusal: Vec<uuid::Uuid> = client.list_data_keys(branch).await.unwrap().iter().map(|k| k.id).collect();
    let same = bound_key(branch, "app", "old", Some(16390));
    let rotated = client.rotate_secret_key(branch, old.id, &same).await;
    let legacy = bound_key(branch, "legacy", "legacy", None);
    client.create_secret_key(&legacy).await.unwrap();
    let adopted = bound_key(branch, "legacy", "legacy", Some(16400));
    let adopted_ok = client.rotate_secret_key(branch, legacy.id, &adopted).await;
    let keys = client.list_data_keys(branch).await.unwrap();
    cleanup(&client, id).await;
    assert!(matches!(&refused, Err(MetadataError::RoleRecreated(r)) if r == "app"), "{refused:?}");
    assert_eq!(after_refusal, [old.id], "the revoke rolled back, nothing inserted");
    assert!(matches!(rotated, Ok(true)), "{rotated:?}");
    assert!(matches!(adopted_ok, Ok(true)), "{adopted_ok:?}");
    let oid_of = |id| keys.iter().find(|k| k.id == id).map(|k| k.role_oid);
    assert_eq!(oid_of(same.id), Some(Some(16390)));
    assert_eq!(oid_of(adopted.id), Some(Some(16400)));
}

// The resync's repairs apply only while the row still looks as the resync saw it.
#[tokio::test]
#[ignore]
async fn fix_key_roles_applies_each_repair_conditionally() {
    let _sealed = sealed_lock().await;
    let client = client().await;
    let (id, _) = ready_database(&client).await;
    let branch = main_of(id);
    let dead = bound_key(branch, "gone", "dead", Some(16385));
    let moved = bound_on(branch, "old_name", "moved", 16390, TL1);
    let legacy = bound_key(branch, "app", "legacy", None);
    let stale = bound_key(branch, "app", "stale", Some(16384));
    let elsewhere = bound_on(branch, "other", "elsewhere", 16391, TL2);
    let unstamped = bound_key(branch, "kept", "unstamped", Some(16392));
    let restamp_stale = bound_on(branch, "kept2", "restamp_stale", 16393, TL1);
    for key in [&dead, &moved, &legacy, &stale, &elsewhere, &unstamped, &restamp_stale] {
        client.create_secret_key(key).await.unwrap();
    }
    let changed = client
        .fix_key_roles(
            branch,
            &[
                KeyRoleFix::Revoke { id: dead.id, role: "gone".into(), oid: Some(16385) },
                KeyRoleFix::Follow { id: moved.id, from: "old_name".into(), to: "renamed".into(), oid: 16390, timeline: TL1.into() },
                KeyRoleFix::Backfill { id: legacy.id, role: "app".into(), oid: 16384, timeline: TL1.into() },
                // Seen with another OID than the row has now: left alone.
                KeyRoleFix::Revoke { id: stale.id, role: "app".into(), oid: Some(1) },
                // E26: a Follow never applies to a row bound on another timeline.
                KeyRoleFix::Follow { id: elsewhere.id, from: "other".into(), to: "x".into(), oid: 16391, timeline: TL1.into() },
                KeyRoleFix::Restamp { id: unstamped.id, role: "kept".into(), oid: 16392, from: None, timeline: TL1.into() },
                // Seen with another timeline than the row has now: left alone.
                KeyRoleFix::Restamp { id: restamp_stale.id, role: "kept2".into(), oid: 16393, from: Some(TL2.into()), timeline: TL1.into() },
            ],
        )
        .await
        .unwrap();
    let keys = client.list_data_keys(branch).await.unwrap();
    cleanup(&client, id).await;
    assert_eq!(changed, 4);
    let find = |id| keys.iter().find(|k| k.id == id);
    assert!(find(dead.id).is_none(), "revoked");
    assert_eq!(find(moved.id).unwrap().role, "renamed");
    assert_eq!(find(legacy.id).unwrap().role_oid, Some(16384));
    assert_eq!(find(legacy.id).unwrap().role_timeline.as_deref(), Some(TL1), "a backfill stamps the timeline");
    assert!(find(stale.id).is_some(), "a repair that no longer matches is skipped");
    assert_eq!(find(elsewhere.id).unwrap().role, "other", "not followed across timelines");
    assert_eq!(find(unstamped.id).unwrap().role_timeline.as_deref(), Some(TL1), "re-stamped");
    assert_eq!(find(restamp_stale.id).unwrap().role_timeline.as_deref(), Some(TL1), "left as it was");
}

#[tokio::test]
#[ignore]
async fn create_secret_key_before_a_ddl_drop_is_seen_by_it() {
    let _sealed = sealed_lock().await;
    let client = client().await;
    let (id, ep) = ready_database(&client).await;
    let branch = main_of(id);
    client.insert_api_role(&ep, "app", true, "SCRAM-SHA-256$o", "v1:app", 10).await.unwrap();
    let (key, _) = new_key(branch, DataKeyKind::Secret, "app", "first");
    client.create_secret_key(&key).await.unwrap();
    let mut ddl = client.begin_ddl(&ep).await.unwrap().expect("endpoint");
    let live = ddl.live_secret_keys("app").await.unwrap();
    drop(ddl);
    cleanup(&client, id).await;
    assert_eq!(live.iter().map(|(id, _)| *id).collect::<Vec<_>>(), [key.id]);
}

#[tokio::test]
#[ignore]
async fn rotate_secret_key_waits_for_a_password_removal_and_keeps_the_rowless_role() {
    let _sealed = sealed_lock().await;
    let client = client().await;
    let (id, ep) = ready_database(&client).await;
    let branch = main_of(id);
    client.insert_api_role(&ep, "app", true, "SCRAM-SHA-256$o", "v1:app", 10).await.unwrap();
    let (old, _) = new_key(branch, DataKeyKind::Secret, "app", "old");
    client.create_secret_key(&old).await.unwrap();
    // `ALTER ROLE app PASSWORD NULL` through the receiver, uncommitted: the row goes, the role
    // stays (C1: a role without a row still backs its keys).
    let mut ddl = client.begin_ddl(&ep).await.unwrap().expect("endpoint");
    assert!(ddl.remove_copy("app").await.unwrap());

    let (new, _) = new_key(branch, DataKeyKind::Secret, "app", "old");
    let c = client.clone();
    let n = new.clone();
    let old_id = old.id;
    let mut rotating = tokio::spawn(async move { c.rotate_secret_key(branch, old_id, &n).await });
    let waited = tokio::time::timeout(STILL_BLOCKED, &mut rotating).await;
    assert!(waited.is_err(), "rotation must wait for the DDL transaction");

    ddl.commit().await.unwrap();
    let result = rotating.await.unwrap();
    let live: Vec<uuid::Uuid> = client.list_data_keys(branch).await.unwrap().iter().map(|k| k.id).collect();
    cleanup(&client, id).await;
    assert!(matches!(result, Ok(true)), "{result:?}");
    assert_eq!(live, [new.id], "the old key revoked, the new one live for the same role");
}

#[tokio::test]
#[ignore]
async fn rotate_secret_key_follows_a_rename_made_while_it_waited() {
    let _sealed = sealed_lock().await;
    let client = client().await;
    let (id, ep) = ready_database(&client).await;
    let branch = main_of(id);
    client.insert_api_role(&ep, "app", true, "SCRAM-SHA-256$o", "v1:app", 10).await.unwrap();
    let (old, _) = new_key(branch, DataKeyKind::Secret, "app", "old");
    client.create_secret_key(&old).await.unwrap();
    let mut ddl = client.begin_ddl(&ep).await.unwrap().expect("endpoint");
    assert!(ddl.rename_copy("app", "app2", None).await.unwrap());

    // The caller read the old role name before the rename committed.
    let (new, _) = new_key(branch, DataKeyKind::Secret, "app", "old");
    let c = client.clone();
    let n = new.clone();
    let old_id = old.id;
    let mut rotating = tokio::spawn(async move { c.rotate_secret_key(branch, old_id, &n).await });
    assert!(tokio::time::timeout(STILL_BLOCKED, &mut rotating).await.is_err());
    ddl.commit().await.unwrap();
    let result = rotating.await.unwrap();
    let roles: Vec<String> = client.list_data_keys(branch).await.unwrap().into_iter().map(|k| k.role).collect();
    cleanup(&client, id).await;
    assert!(matches!(result, Ok(true)), "{result:?}");
    assert_eq!(roles, ["app2"], "the new key has the role's current name");
}

#[tokio::test]
#[ignore]
async fn rotate_secret_key_before_a_ddl_drop_is_seen_by_it() {
    let _sealed = sealed_lock().await;
    let client = client().await;
    let (id, ep) = ready_database(&client).await;
    let branch = main_of(id);
    client.insert_api_role(&ep, "app", true, "SCRAM-SHA-256$o", "v1:app", 10).await.unwrap();
    let (old, _) = new_key(branch, DataKeyKind::Secret, "app", "old");
    client.create_secret_key(&old).await.unwrap();
    let (new, _) = new_key(branch, DataKeyKind::Secret, "app", "old");
    let rotated = client.rotate_secret_key(branch, old.id, &new).await.unwrap();
    let mut ddl = client.begin_ddl(&ep).await.unwrap().expect("endpoint");
    let live = ddl.live_secret_keys("app").await.unwrap();
    drop(ddl);
    cleanup(&client, id).await;
    assert!(rotated);
    assert_eq!(live.iter().map(|(id, _)| *id).collect::<Vec<_>>(), [new.id]);
}

// The API's delete drops the role in SQL (forwarding off), then deletes the row: a key stored
// while it dropped the role (after its own key check) is revoked with it.
#[tokio::test]
#[ignore]
async fn delete_role_copy_revokes_late_keys() {
    let _sealed = sealed_lock().await;
    let client = client().await;
    let (id, ep) = ready_database(&client).await;
    let branch = main_of(id);
    client.insert_api_role(&ep, "app", false, "SCRAM-SHA-256$o", "v1:app", 10).await.unwrap();
    let (late, _) = new_key(branch, DataKeyKind::Secret, "app", "late");
    let (other, _) = new_key(branch, DataKeyKind::Secret, "authenticated", "other");
    client.create_secret_key(&late).await.unwrap();
    client.create_secret_key(&other).await.unwrap();
    let deleted = client.delete_role_copy(&ep, "app").await.unwrap();
    let live: Vec<uuid::Uuid> = client.list_data_keys(branch).await.unwrap().iter().map(|k| k.id).collect();
    cleanup(&client, id).await;
    assert!(deleted);
    assert_eq!(live, [other.id], "only the dropped role's key is revoked");
}

#[tokio::test]
#[ignore]
async fn rotate_secret_key_refuses_a_public_key() {
    let _sealed = sealed_lock().await;
    let client = client().await;
    let (id, _) = ready_database(&client).await;
    let branch = main_of(id);
    let (public, _) = new_key(branch, DataKeyKind::Public, "anon", "default");
    client.insert_data_key(&public).await.unwrap();
    let (next, _) = new_key(branch, DataKeyKind::Secret, "anon", "default");
    let rotated = client.rotate_secret_key(branch, public.id, &next).await;
    let live: Vec<uuid::Uuid> = client.list_data_keys(branch).await.unwrap().iter().map(|k| k.id).collect();
    cleanup(&client, id).await;
    assert!(matches!(rotated, Ok(false)), "{rotated:?}");
    assert_eq!(live, [public.id], "the public key is still live, nothing inserted");
}

#[tokio::test]
#[ignore]
async fn ddl_rowless_rename_moves_the_keys() {
    let _sealed = sealed_lock().await;
    let client = client().await;
    let (id, ep) = ready_database(&client).await;
    let branch = main_of(id);
    // No neon_roles row for "authenticated" or "nopw": a role without a password has none.
    let (live, _) = new_key(branch, DataKeyKind::Secret, "nopw", "live");
    let (revoked, _) = new_key(branch, DataKeyKind::Secret, "nopw", "revoked");
    let (other, _) = new_key(branch, DataKeyKind::Secret, "authenticated", "other");
    for key in [&live, &revoked, &other] {
        client.insert_data_key(key).await.unwrap();
    }
    sqlx::query("UPDATE data_api_keys SET revoked_at = now() WHERE id = $1")
        .bind(revoked.id)
        .execute(client.pool())
        .await
        .unwrap();
    let v = version(&client, &ep).await;

    // Two hops, as the receiver does; then a new role takes the old name.
    let mut tx = client.begin_ddl(&ep).await.unwrap().expect("endpoint");
    let row = tx.rename_copy("nopw", "aside", None).await.unwrap();
    let rename = tx.rename_copy("aside", "moved", None).await.unwrap();
    tx.upsert_sql_copy("nopw", SQL_VERIFIER).await.unwrap();
    let inherited = tx.live_secret_keys("nopw").await.unwrap();
    let moved = tx.live_secret_keys("moved").await.unwrap();
    tx.commit().await.unwrap();

    let roles: Vec<(String, String)> =
        sqlx::query_as("SELECT name, role FROM data_api_keys WHERE branch_id = $1 ORDER BY name")
            .bind(branch.as_uuid())
            .fetch_all(client.pool())
            .await
            .unwrap();
    let v_after = version(&client, &ep).await;
    cleanup(&client, id).await;
    assert!(!row && !rename, "no row moved");
    assert!(inherited.is_empty(), "the new role has no keys");
    assert_eq!(moved.iter().map(|(id, _)| *id).collect::<Vec<_>>(), [live.id]);
    assert_eq!(
        roles,
        [
            ("live".to_string(), "moved".to_string()),
            ("other".to_string(), "authenticated".to_string()),
            ("revoked".to_string(), "moved".to_string()),
        ]
    );
    assert!(v_after > v, "moving keys is a write");
}

#[tokio::test]
#[ignore]
async fn ddl_rowless_swap_moves_each_roles_keys() {
    let _sealed = sealed_lock().await;
    let client = client().await;
    let (id, ep) = ready_database(&client).await;
    let branch = main_of(id);
    // "a" has a row, "b" none (mixed); both back a key.
    client.insert_api_role(&ep, "a", true, "SCRAM-SHA-256$a", "v1:a", 10).await.unwrap();
    let (ka, _) = new_key(branch, DataKeyKind::Secret, "a", "key-a");
    let (kb, _) = new_key(branch, DataKeyKind::Secret, "b", "key-b");
    client.insert_data_key(&ka).await.unwrap();
    client.insert_data_key(&kb).await.unwrap();

    // a -> aside1, b -> aside2, then aside1 -> b, aside2 -> a.
    let mut tx = client.begin_ddl(&ep).await.unwrap().expect("endpoint");
    tx.rename_copy("a", "aside1", None).await.unwrap();
    tx.rename_copy("b", "aside2", None).await.unwrap();
    tx.rename_copy("aside1", "b", None).await.unwrap();
    tx.rename_copy("aside2", "a", None).await.unwrap();
    tx.commit().await.unwrap();
    let roles: Vec<(String, String)> =
        sqlx::query_as("SELECT name, role FROM data_api_keys WHERE branch_id = $1 ORDER BY name")
            .bind(branch.as_uuid())
            .fetch_all(client.pool())
            .await
            .unwrap();
    cleanup(&client, id).await;
    assert_eq!(roles, [("key-a".to_string(), "b".to_string()), ("key-b".to_string(), "a".to_string())]);
}

// --- T14 fix batch F2 ---------------------------------------------------------------------

/// Restores `dev` (ready) to `at` on its current timeline; returns the restored timeline.
async fn restore_once(client: &MetadataClient, dev: BranchId, at: &str) -> String {
    let pending = start_reset(client, dev).await;
    let backup_name = format!("dev-before-restore-{}", &hex32()[..8]);
    let switched = client
        .switch_to_restored_timeline(dev, &pending, BranchId::new(), &backup_name, at)
        .await
        .unwrap();
    assert!(switched);
    assert!(client.set_branch_status(dev, &[BranchStatus::Resetting], BranchStatus::Ready).await.unwrap());
    pending
}

// T14 I1: a restore to a point before an earlier restore's walks the restore chain back, at any
// depth, to the timeline whose base was current there, also through a timeline that got no
// copy (its first entry is after the point).
#[tokio::test]
#[ignore]
async fn a_second_restore_before_the_first_walks_the_restore_chain() {
    let _sealed = sealed_lock().await;
    let client = client().await;
    let (db, _ep) = ready_database(&client).await;
    let dev = ready_branch(&client, db, "dev", main_of(db)).await;
    let t0 = branch(&client, dev).await.timeline_id.unwrap();
    let mut create = base_at(dev, "app", BaseSource::Create, &t0, "0/1000");
    create.model = Some(serde_json::json!({"tables": ["t0"]}));
    let create_id = client.append_schema_base(&create).await.unwrap();
    client.append_schema_base(&base_at(dev, "app", BaseSource::Merge, &t0, "0/6000")).await.unwrap();

    // t0 -> t1 at 0/5000: the create's entry is copied onto t1 at 0/5000.
    let t1 = restore_once(&client, dev, "0/5000").await;
    let on_t1 = client.schema_base(dev, "app").await.unwrap().unwrap();
    // t1 -> t2 at 0/3000, before t1's first entry: walks to t0 at 0/3000.
    let t2 = restore_once(&client, dev, "0/3000").await;
    let on_t2 = client.schema_base(dev, "app").await.unwrap().unwrap();
    // t2 -> t3 at 0/2000: t2's copy is at 0/3000, so on to t1 at 0/2000 (none), then t0.
    let t3 = restore_once(&client, dev, "0/2000").await;
    let on_t3 = client.schema_base(dev, "app").await.unwrap().unwrap();
    // t3 -> t4 at 0/800: before every entry anywhere: no copy.
    let t4 = restore_once(&client, dev, "0/800").await;
    let on_t4 = client.schema_base(dev, "app").await.unwrap().unwrap();
    let links: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT timeline_id, from_timeline_id, at_lsn FROM branch_restores
         WHERE branch_id = $1 ORDER BY created_at, timeline_id",
    )
    .bind(dev.as_uuid())
    .fetch_all(client.pool())
    .await
    .unwrap();
    cleanup(&client, db).await;

    let located = |b: &SchemaBase| {
        (b.branch_timeline_id.clone().unwrap(), b.branch_lsn.clone().unwrap(), b.timeline_id.clone(), b.lsn.clone())
    };
    assert_eq!(located(&on_t1), (t1.clone(), "0/5000".into(), Some(t0.clone()), Some("0/1000".into())));
    assert_eq!(located(&on_t2), (t2.clone(), "0/3000".into(), Some(t0.clone()), Some("0/1000".into())));
    assert_eq!(located(&on_t3), (t3.clone(), "0/2000".into(), Some(t0.clone()), Some("0/1000".into())));
    for b in [&on_t1, &on_t2, &on_t3] {
        assert_eq!(b.model, create.model, "the create's model, copied");
        assert_eq!(b.source, BaseSource::Create);
        assert!(b.id > create_id);
    }
    assert_eq!(on_t4.branch_timeline_id.as_deref(), Some(t3.as_str()), "nothing at or before 0/800: no copy");
    assert_eq!(
        links,
        vec![
            (t1.clone(), t0.clone(), "0/5000".into()),
            (t2.clone(), t1.clone(), "0/3000".into()),
            (t3.clone(), t2.clone(), "0/2000".into()),
            (t4, t3, "0/800".into()),
        ]
    );
}

// T14 M7: appending drops the model of an entry superseded on its own timeline by one recorded
// more than the history window ago; the entry stays, with its location and map. Other
// timelines, databases and recent supersessions keep theirs.
#[tokio::test]
#[ignore]
async fn superseded_bases_drop_their_model_after_the_window() {
    let _sealed = sealed_lock().await;
    let client = client().await;
    let (db, _ep) = ready_database(&client).await;
    let main = main_of(db);
    let (t, u) = (hex32(), hex32());
    let with_model = |db_name: &str, timeline: &str, lsn: &str| {
        let mut b = base_at(main, db_name, BaseSource::Merge, timeline, lsn);
        b.model = Some(serde_json::json!({"at": lsn}));
        b.model_version = Some(1);
        b.server_major = Some(17);
        b.identity_map = serde_json::json!({"pairs": [1]});
        b
    };
    let old = client.append_schema_base(&with_model("app", &t, "0/1000")).await.unwrap();
    let superseding = client.append_schema_base(&with_model("app", &t, "0/2000")).await.unwrap();
    let other_db = client.append_schema_base(&with_model("shop", &t, "0/1000")).await.unwrap();
    let other_timeline = client.append_schema_base(&with_model("app", &u, "0/1000")).await.unwrap();
    // The superseding entry was recorded eight days ago (the window is seven).
    sqlx::query("UPDATE branch_schema_bases SET recorded_at = now() - interval '8 days' WHERE id = $1")
        .bind(superseding)
        .execute(client.pool())
        .await
        .unwrap();
    let recent = client.append_schema_base(&with_model("app", &t, "0/3000")).await.unwrap();
    let rows: Vec<(i64, Option<serde_json::Value>, Option<i32>, serde_json::Value)> = sqlx::query_as(
        "SELECT id, model, model_version, identity_map FROM branch_schema_bases WHERE branch_id = $1 ORDER BY id",
    )
    .bind(main.as_uuid())
    .fetch_all(client.pool())
    .await
    .unwrap();
    cleanup(&client, db).await;

    let has_model = |id: i64| rows.iter().find(|r| r.0 == id).map(|r| r.1.is_some() && r.2.is_some()).unwrap();
    assert!(!has_model(old), "superseded on its timeline by an entry older than the window");
    assert_eq!(rows.iter().find(|r| r.0 == old).unwrap().3, serde_json::json!({"pairs": [1]}), "the map stays");
    assert!(has_model(superseding), "superseded only by a recent entry");
    assert!(has_model(other_db) && has_model(other_timeline) && has_model(recent));
}

// T14 I2: turning LOGIN on (a row without a verifier gets one) counts against the cap in the
// same transaction, for an API row's PATCH and for an SQL role's new password; a row that
// already has a verifier isn't counted again.
#[tokio::test]
#[ignore]
async fn login_on_respects_the_cap() {
    let _sealed = sealed_lock().await;
    let client = client().await;
    let (id, ep) = ready_database(&client).await;
    let plain = RoleAttributes::default();
    let nologin = |name| NewApiRole { name, owner_level: false, login: false, attributes: &plain, verifier: None, password_enc: None, change_id: uuid::Uuid::new_v4() };
    client.insert_api_role(&ep, "a1", false, "SCRAM-SHA-256$a", "v1:a", 2).await.unwrap();
    client.insert_api_role_with(&ep, &nologin("g1"), 2).await.unwrap();
    client.insert_api_role_with(&ep, &nologin("g2"), 2).await.unwrap();
    let on = ApiRoleChange {
        owner_level: false,
        login: true,
        attributes: plain.clone(),
        secret: SecretChange::Set("SCRAM-SHA-256$n".into(), "v1:n".into()),
        change_id: uuid::Uuid::new_v4(),
    };
    let first = client.update_api_role(&ep, "g1", &on, 2).await.unwrap();
    let second = client.update_api_role(&ep, "g2", &on, 2).await.unwrap();
    // A row that has a verifier: no new copy, never refused.
    let again = client.update_api_role(&ep, "a1", &on, 2).await.unwrap();
    let sql_role = client.store_role_secret(&ep, "made_in_sql", "SCRAM-SHA-256$s", "v1:s", 2).await.unwrap();
    // An SQL row with a verifier already counts: replacing its password is fine.
    seed_copy(&client, &ep, "counted", "sql", Some(SQL_VERIFIER), None).await;
    let replaced = client.store_role_secret(&ep, "counted", "SCRAM-SHA-256$t", "v1:t", 2).await.unwrap();
    let rows = client.list_roles(&ep).await.unwrap();
    cleanup(&client, id).await;
    assert_eq!(first, RoleWriteOutcome::Done);
    assert_eq!(second, RoleWriteOutcome::CapReached);
    assert_eq!(again, RoleWriteOutcome::Done);
    assert_eq!(sql_role, RoleWriteOutcome::CapReached);
    assert_eq!(replaced, RoleWriteOutcome::Done);
    let g2 = rows.iter().find(|r| r.name == "g2").unwrap();
    assert!(!g2.login, "nothing written at the cap");
    assert!(!rows.iter().any(|r| r.name == "made_in_sql"));
}

// T14 E32: an API create or PATCH marks the row's attributes pending; a confirm or a resync
// write clears it; a reset (`pending` alone) doesn't set it. Rows from before the column start
// false. The resync records an API row's owner level when asked; `None` keeps it.
#[tokio::test]
#[ignore]
async fn attributes_pending_follows_api_writes() {
    let _sealed = sealed_lock().await;
    let client = client().await;
    let (id, ep) = ready_database(&client).await;
    let branch = main_of(id);
    seed_copy(&client, &ep, "legacy", "api", Some(SQL_VERIFIER), None).await;
    let plain = RoleAttributes::default();
    let created = uuid::Uuid::new_v4();
    let app = NewApiRole {
        name: "app",
        owner_level: false,
        login: true,
        attributes: &plain,
        verifier: Some("SCRAM-SHA-256$a"),
        password_enc: Some("v1:a"),
        change_id: created,
    };
    client.insert_api_role_with(&ep, &app, 10).await.unwrap();
    let state = |client: MetadataClient, ep: String| async move {
        let (copies, v) = client.role_copies(&ep).await.unwrap();
        let of = |n: &str| copies.iter().find(|c| c.name == n).map(|c| (c.pending, c.attributes_pending, c.api_owner_level)).unwrap();
        (of("app"), of("legacy"), v)
    };
    let (inserted, legacy, v) = state(client.clone(), ep.clone()).await;
    let confirmed = client.confirm_role(&ep, "app", v, Some(created)).await.unwrap();
    let (after_confirm, _, _) = state(client.clone(), ep.clone()).await;
    client.replace_role_secret(&ep, "app", "SCRAM-SHA-256$b", "v1:b").await.unwrap();
    let (after_reset, _, _) = state(client.clone(), ep.clone()).await;
    let change = ApiRoleChange { owner_level: false, login: true, attributes: RoleAttributes::default(), secret: SecretChange::Keep, change_id: uuid::Uuid::new_v4() };
    client.update_api_role(&ep, "app", &change, 10).await.unwrap();
    let (after_patch, _, v) = state(client.clone(), ep.clone()).await;
    let mut recorded = upsert("app", RoleOrigin::Api, "SCRAM-SHA-256$b");
    recorded.api_owner_level = Some(true);
    recorded.level = RoleLevel::OwnerLevel;
    let mut kept = upsert("legacy", RoleOrigin::Api, SQL_VERIFIER);
    kept.api_owner_level = None;
    let write = ResyncWrite { upserts: vec![recorded, kept], removals: vec![], databases: vec![observed("db", "postgres", false)] };
    client.apply_resync(&ep, branch, v, &write).await.unwrap();
    let (after_resync, legacy_after, _) = state(client.clone(), ep.clone()).await;
    cleanup(&client, id).await;
    assert_eq!(inserted, (true, true, false));
    assert_eq!(legacy, (false, false, false), "an existing row starts false");
    assert!(confirmed);
    assert_eq!(after_confirm, (false, false, false));
    assert_eq!(after_reset, (true, false, false), "a reset leaves the attributes alone");
    assert_eq!(after_patch, (true, true, false));
    assert_eq!(after_resync, (false, false, true), "the resync recorded the catalog's owner level");
    assert_eq!(legacy_after, (false, false, false), "None keeps the row's owner level");
}

// F2 fix round I-1: a confirm clears `attributes_pending` by the change's id even when the
// version moved (a receiver write meanwhile); `pending` keeps the version guard. A confirm
// carrying an older change's id leaves a newer change pending.
#[tokio::test]
#[ignore]
async fn confirm_clears_attributes_by_change_id() {
    let _sealed = sealed_lock().await;
    let client = client().await;
    let (id, ep) = ready_database(&client).await;
    let plain = RoleAttributes::default();
    let first = uuid::Uuid::new_v4();
    let role = NewApiRole { name: "app", owner_level: false, login: false, attributes: &plain, verifier: None, password_enc: None, change_id: first };
    client.insert_api_role_with(&ep, &role, 10).await.unwrap();
    let (_, v) = client.role_copies(&ep).await.unwrap();
    let first_version = v;
    // A receiver write lands between the apply and the confirm.
    seed_copy(&client, &ep, "other", "sql", Some(SQL_VERIFIER), None).await;
    sqlx::query("UPDATE neon_endpoints SET roles_version = roles_version + 1 WHERE id = $1")
        .bind(&ep)
        .execute(client.pool())
        .await
        .unwrap();
    let cleared = client.confirm_role(&ep, "app", v, Some(first)).await.unwrap();
    let flags = |client: MetadataClient, ep: String| async move {
        let (copies, _) = client.role_copies(&ep).await.unwrap();
        let app = copies.into_iter().find(|c| c.name == "app").unwrap();
        (app.pending, app.attributes_pending)
    };
    let after_lost = flags(client.clone(), ep.clone()).await;
    // A newer change, then a confirm of the older one.
    let second = uuid::Uuid::new_v4();
    let change = ApiRoleChange { owner_level: false, login: false, attributes: plain.clone(), secret: SecretChange::Keep, change_id: second };
    client.update_api_role(&ep, "app", &change, 10).await.unwrap();
    let (_, v) = client.role_copies(&ep).await.unwrap();
    // The stale confirm carries what its own (older) write read: the first version and id.
    client.confirm_role(&ep, "app", first_version, Some(first)).await.unwrap();
    let after_stale = flags(client.clone(), ep.clone()).await;
    let confirmed = client.confirm_role(&ep, "app", v, Some(second)).await.unwrap();
    let after_confirm = flags(client.clone(), ep.clone()).await;
    cleanup(&client, id).await;
    assert!(!cleared, "pending isn't cleared past a version bump");
    assert_eq!(after_lost, (true, false), "attributes cleared by id");
    assert_eq!(after_stale, (true, true), "an older version leaves pending, an older id leaves the attributes pending");
    assert!(confirmed);
    assert_eq!(after_confirm, (false, false));
}

// F2 fix round M7: only a secret key can be marked as bypassing RLS.
#[tokio::test]
#[ignore]
async fn only_a_secret_key_bypasses_rls() {
    let _sealed = sealed_lock().await;
    let client = client().await;
    let (id, _) = ready_database(&client).await;
    let branch = main_of(id);
    let (public, _) = new_key(branch, DataKeyKind::Public, "anon", "pub2");
    let stored = client.insert_data_key(&public).await;
    let marked = sqlx::query("UPDATE data_api_keys SET role_bypass_rls = true WHERE branch_id = $1 AND kind = 'public'")
        .bind(branch.as_uuid())
        .execute(client.pool())
        .await;
    cleanup(&client, id).await;
    let _ = stored;
    assert!(marked.is_err(), "the CHECK refuses a public key marked");
}

// T14 E33: a secret key stores whether its role has BYPASSRLS; the list returns it, a rotation
// takes the new check's answer, and the resync's fix updates it only while the OID matches.
#[tokio::test]
#[ignore]
async fn secret_key_bypass_rls_round_trips_and_is_fixed_by_oid() {
    let _sealed = sealed_lock().await;
    let client = client().await;
    let (id, _) = ready_database(&client).await;
    let branch = main_of(id);
    let key = NewDataKey { role_bypass_rls: true, ..bound_on(branch, "svc", "svc", 16390, TL1) };
    client.create_secret_key(&key).await.unwrap();
    let listed = client.list_data_keys(branch).await.unwrap();
    let plain = bound_on(branch, "svc", "svc", 16390, TL1);
    client.rotate_secret_key(branch, key.id, &plain).await.unwrap();
    let rotated = client.list_data_keys(branch).await.unwrap();
    let stale = client
        .fix_key_roles(branch, &[KeyRoleFix::Bypass { id: plain.id, oid: 1, bypass_rls: true }])
        .await
        .unwrap();
    let fixed = client
        .fix_key_roles(branch, &[KeyRoleFix::Bypass { id: plain.id, oid: 16390, bypass_rls: true }])
        .await
        .unwrap();
    let after = client.list_data_keys(branch).await.unwrap();
    // A public key never bypasses, whatever is asked.
    let (public, _) = new_key(branch, DataKeyKind::Public, "anon", "pub");
    let public = NewDataKey { role_bypass_rls: true, ..public };
    let public_stored = client.insert_data_key(&public).await;
    let keys = client.list_data_keys(branch).await.unwrap();
    cleanup(&client, id).await;
    let flag = |keys: &[chelabase_metadata::DataKeyRecord], id| keys.iter().find(|k| k.id == id).map(|k| k.role_bypass_rls);
    assert_eq!(flag(&listed, key.id), Some(true));
    assert_eq!(flag(&rotated, plain.id), Some(false), "the rotation's check decides");
    assert_eq!((stale, fixed), (0, 1));
    assert_eq!(flag(&after, plain.id), Some(true));
    if public_stored.is_ok() {
        assert_eq!(flag(&keys, public.id), Some(false));
    }
}

// T14 E29: a control-plane rename moves the row (resealed) and every key of the old name;
// a taken name (a row, or live keys of a dropped role) moves nothing; a role without a row
// moves its keys only.
#[tokio::test]
#[ignore]
async fn rename_role_copy_moves_the_row_and_keys() {
    let _sealed = sealed_lock().await;
    let client = client().await;
    let (id, ep) = ready_database(&client).await;
    let branch = main_of(id);
    client.insert_api_role(&ep, "app", false, "SCRAM-SHA-256$a", "v1:app", 10).await.unwrap();
    client.insert_api_role(&ep, "other", false, "SCRAM-SHA-256$o", "v1:o", 10).await.unwrap();
    let app_key = bound_on(branch, "app", "app-key", 16390, TL1);
    let rowless_key = bound_on(branch, "group", "group-key", 16391, TL1);
    let dead_key = bound_on(branch, "ghost", "ghost-key", 16392, TL1);
    for key in [&app_key, &rowless_key, &dead_key] {
        client.create_secret_key(key).await.unwrap();
    }
    let v = version(&client, &ep).await;
    let onto_row = client.rename_role_copy(&ep, branch, "app", "other", Some("v1:x"), None).await.unwrap();
    let onto_keys = client.rename_role_copy(&ep, branch, "app", "ghost", Some("v1:x"), None).await.unwrap();
    let renamed = client.rename_role_copy(&ep, branch, "app", "app2", Some("v1:app2"), None).await.unwrap();
    let after = version(&client, &ep).await;
    let rowless = client.rename_role_copy(&ep, branch, "group", "group2", None, None).await.unwrap();
    let unknown = client.rename_role_copy("ep-unknown", branch, "group2", "group3", None, None).await.unwrap();
    // M4: a move that couldn't reseal marks the row; moving it back restores the flag.
    let lost = client.rename_role_copy(&ep, branch, "other", "other2", None, None).await.unwrap();
    let marked = client.list_roles(&ep).await.unwrap().into_iter().find(|r| r.name == "other2").unwrap().password;
    let back = client.rename_role_copy(&ep, branch, "other2", "other", Some("v1:o"), Some(false)).await.unwrap();
    let restored = client.list_roles(&ep).await.unwrap().into_iter().find(|r| r.name == "other").unwrap().password;
    let (copies, _) = client.role_copies(&ep).await.unwrap();
    let keys = client.list_data_keys(branch).await.unwrap();
    cleanup(&client, id).await;
    assert_eq!(unknown, RenameCopyOutcome::NotFound, "an unknown endpoint isn't a taken name");
    assert_eq!(lost, RenameCopyOutcome::Renamed { row: true, was_unreadable: false });
    assert_eq!(marked, PasswordState::Unreadable);
    assert_eq!(back, RenameCopyOutcome::Renamed { row: true, was_unreadable: true });
    assert_eq!(restored, PasswordState::Generated);
    assert_eq!((onto_row, onto_keys), (RenameCopyOutcome::Taken, RenameCopyOutcome::Taken));
    assert_eq!(renamed, RenameCopyOutcome::Renamed { row: true, was_unreadable: false });
    assert_eq!(rowless, RenameCopyOutcome::Renamed { row: false, was_unreadable: false });
    assert_eq!(after, v + 1);
    let app2 = copies.iter().find(|c| c.name == "app2").unwrap();
    assert_eq!((app2.origin, app2.scram_verifier.as_deref(), app2.password_enc.as_deref()), (RoleOrigin::Api, Some("SCRAM-SHA-256$a"), Some("v1:app2")));
    assert!(!copies.iter().any(|c| c.name == "app"));
    let role_of = |id| keys.iter().find(|k| k.id == id).map(|k| (k.role.clone(), k.role_oid, k.role_timeline.clone()));
    assert_eq!(role_of(app_key.id), Some(("app2".into(), Some(16390), Some(TL1.into()))), "OID and timeline kept");
    assert_eq!(role_of(rowless_key.id), Some(("group2".into(), Some(16391), Some(TL1.into()))));
    assert_eq!(role_of(dead_key.id).map(|r| r.0), Some("ghost".into()));
}

// --- Protected branches (PB, T1) -------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn guard_settings_default_on_and_children_copy_them() {
    let _sealed = sealed_lock().await;
    let client = client().await;
    let (id, _ep) = ready_database(&client).await;
    let main = main_of(id);
    let root = branch(&client, main).await;
    assert!(root.guard_logins, "a root branch starts with the switch on");
    assert_eq!(root.review_ack_hash, None);
    assert!(root.review_ack_keys.is_empty());
    assert!(client.list_guard_exemptions(main).await.unwrap().is_empty());
    // By name, idempotent, sorted.
    assert!(client.add_guard_exemption(main, "deploy").await.unwrap());
    assert!(client.add_guard_exemption(main, "Ci Bot").await.unwrap());
    assert!(!client.add_guard_exemption(main, "deploy").await.unwrap());
    assert_eq!(client.list_guard_exemptions(main).await.unwrap(), ["Ci Bot", "deploy"]);
    assert!(client.add_guard_exemption(main, "").await.is_err(), "empty");
    assert!(client.add_guard_exemption(main, &"a".repeat(64)).await.is_err(), "64 bytes");
    let off = client.set_branch_guard_logins(main, false).await.unwrap().expect("a normal branch");
    assert!(!off.guard_logins);
    // A child copies the switch and the exemptions; later changes stay on their branch.
    let dev = ready_branch(&client, id, "dev", main).await;
    assert!(!branch(&client, dev).await.guard_logins);
    assert_eq!(client.list_guard_exemptions(dev).await.unwrap(), ["Ci Bot", "deploy"]);
    assert!(client.remove_guard_exemption(main, "deploy").await.unwrap());
    assert!(!client.remove_guard_exemption(main, "deploy").await.unwrap());
    assert_eq!(client.list_guard_exemptions(main).await.unwrap(), ["Ci Bot"]);
    assert_eq!(client.list_guard_exemptions(dev).await.unwrap(), ["Ci Bot", "deploy"]);
    let on = client.set_branch_guard_logins(dev, true).await.unwrap().expect("a normal branch");
    assert!(on.guard_logins);
    assert!(!branch(&client, main).await.guard_logins);
    assert!(client.set_branch_guard_logins(BranchId::new(), true).await.unwrap().is_none());
    cleanup(&client, id).await;
}

#[tokio::test]
#[ignore]
async fn protect_with_a_review_stores_the_acknowledged_list() {
    let _sealed = sealed_lock().await;
    let client = client().await;
    let (id, _ep) = ready_database(&client).await;
    let dev = ready_branch(&client, id, "dev", main_of(id)).await;
    let keys = vec!["app/function/public.f()".to_string(), "app/rule/public.r(public.t)".to_string()];
    let protected = client.protect_branch(dev, "h1", &keys).await.unwrap().expect("a normal branch");
    assert!(protected.protected);
    assert_eq!(protected.review_ack_hash.as_deref(), Some("h1"));
    assert_eq!(protected.review_ack_keys, keys);
    let acked = client.acknowledge_review(dev, "h2", &[]).await.unwrap().expect("a normal branch");
    assert!(acked.protected, "acknowledging changes nothing else");
    assert_eq!(acked.review_ack_hash.as_deref(), Some("h2"));
    assert!(acked.review_ack_keys.is_empty());
    // A child copies neither the flag nor the acknowledgement (the parent keeps its
    // acknowledgement when unprotected; a child of a protected branch goes through the
    // parent-protection recheck, so it's made from the unprotected parent here).
    client.set_branch_protected(dev, false).await.unwrap().unwrap();
    assert_eq!(branch(&client, dev).await.review_ack_hash.as_deref(), Some("h2"));
    let child = ready_branch(&client, id, "dev-child", dev).await;
    let record = branch(&client, child).await;
    assert!(!record.protected);
    assert_eq!(record.review_ack_hash, None);
    assert!(record.review_ack_keys.is_empty());
    // Not once a delete has begun, nor for an unknown branch.
    let gone = ready_branch(&client, id, "gone", main_of(id)).await;
    client.begin_branch_delete(gone).await.unwrap();
    assert!(client.protect_branch(gone, "h", &[]).await.unwrap().is_none());
    assert!(client.acknowledge_review(gone, "h", &[]).await.unwrap().is_none());
    assert!(client.protect_branch(BranchId::new(), "h", &[]).await.unwrap().is_none());
    client.set_branch_protected(dev, false).await.unwrap().unwrap();
    cleanup(&client, id).await;
}

// PB end-of-step R12: the member-role sweep is the guard's backstop too, so it visits every
// ready normal branch, members or not (a member-less tenant's guard settings still converge).
#[tokio::test]
#[ignore]
async fn the_member_role_sweep_visits_branches_of_member_less_tenants() {
    let _sealed = sealed_lock().await;
    let client = client().await;
    let (id, _ep) = ready_database(&client).await;
    let main = main_of(id);
    let visited = client.member_role_branches().await.unwrap();
    cleanup(&client, id).await;
    assert!(visited.contains(&main.as_uuid()), "{visited:?}");
}

// PB end-of-step Minor: the CHECK refused U+0001 (meant for NUL, which text can't hold).
#[tokio::test]
#[ignore]
async fn an_exemption_name_may_hold_any_character_but_nul() {
    let _sealed = sealed_lock().await;
    let client = client().await;
    let (id, _ep) = ready_database(&client).await;
    let main = main_of(id);
    let added = client.add_guard_exemption(main, "a\u{1}b").await;
    let listed = client.list_guard_exemptions(main).await;
    cleanup(&client, id).await;
    assert!(added.unwrap());
    assert_eq!(listed.unwrap(), ["a\u{1}b"]);
}

/// Object storage m6: an upload token is recorded only for a live key and spent once, before
/// its expiry, while its key is live; two concurrent spends (two replicas) get it once.
#[tokio::test]
#[ignore]
async fn storage_upload_tokens_spend_once() {
    let client = client().await;
    let (id, _) = ready_database(&client).await;
    let branch = main_of(id);
    let (key, _) = new_key(branch, DataKeyKind::Secret, "authenticated", "uploads");
    client.insert_data_key(&key).await.unwrap();
    let live = client.live_data_key(key.id).await.unwrap();
    let unknown = client.live_data_key(uuid::Uuid::new_v4()).await.unwrap();
    let later = chrono::Utc::now() + chrono::Duration::minutes(5);
    let claims = serde_json::json!({ "sub": "u1" });
    let (token, expired, revoked) = (uuid::Uuid::new_v4(), uuid::Uuid::new_v4(), uuid::Uuid::new_v4());
    let recorded = client.insert_upload_token(token, key.id, &claims, later).await.unwrap();
    let again = client.insert_upload_token(token, key.id, &claims, later).await.unwrap();
    let dead_key = client.insert_upload_token(uuid::Uuid::new_v4(), uuid::Uuid::new_v4(), &claims, later).await;
    client
        .insert_upload_token(expired, key.id, &claims, chrono::Utc::now() - chrono::Duration::seconds(1))
        .await
        .unwrap();
    let (a, b) = tokio::join!(client.consume_upload_token(token), client.consume_upload_token(token));
    let spends: Vec<_> = [a.unwrap(), b.unwrap()].into_iter().flatten().collect();
    let after = client.consume_upload_token(token).await.unwrap();
    let past = client.consume_upload_token(expired).await.unwrap();
    client.insert_upload_token(revoked, key.id, &claims, later).await.unwrap();
    client.revoke_data_key(branch, key.id).await.unwrap();
    let of_revoked = client.consume_upload_token(revoked).await.unwrap();
    let live_after = client.live_data_key(key.id).await.unwrap();
    cleanup(&client, id).await;
    assert_eq!(live.map(|k| k.branch_id), Some(branch));
    assert!(unknown.is_none());
    assert!(recorded && !again);
    assert!(matches!(dead_key, Ok(false)), "{dead_key:?}");
    assert_eq!(spends.len(), 1, "{spends:?}");
    assert_eq!(spends[0].0.key_id, key.id);
    assert_eq!(spends[0].1, claims);
    assert!(after.is_none() && past.is_none() && of_revoked.is_none());
    assert!(live_after.is_none());
}
