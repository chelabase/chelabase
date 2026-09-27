//! Opt-in tests against a real metadata Postgres (`just deps-up` first):
//!
//! ```sh
//! cargo test -p cheladb-metadata --test live_db -- --ignored --test-threads=1
//! ```
//!
//! They use the seeded demo tenant/project and delete the rows they create.

use cheladb_metadata::{
    DatabaseStatus, MetadataClient, MetadataConfig, ReadyDatabase, DEMO_PROJECT_ID,
    DEMO_TENANT_ID,
};
use cheladb_types::{DatabaseId, ProjectId, TenantId};

async fn client() -> MetadataClient {
    let database_url = std::env::var("CHELADB_METADATA_DATABASE_URL").unwrap_or_else(|_| {
        "postgres://cheladb:cheladb@127.0.0.1:5433/cheladb_metadata".to_string()
    });
    let client = MetadataClient::connect(&MetadataConfig { database_url })
        .await
        .expect("metadata Postgres reachable (just deps-up)");
    client.migrate().await.unwrap();
    client
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

async fn cleanup(client: &MetadataClient, id: DatabaseId) {
    sqlx::query("DELETE FROM databases WHERE id = $1")
        .bind(id.as_uuid())
        .execute(client.pool())
        .await
        .unwrap();
}

fn ready() -> ReadyDatabase<'static> {
    ReadyDatabase {
        container_id: "c",
        host: "127.0.0.1",
        port: 5432,
        database_name: "db",
        owner_role: "postgres",
        owner_password_enc: "v1:owner",
        platform_password_enc: "v1:platform",
    }
}

// Review Focus 1: a delete that lands mid-provision must stick.
#[tokio::test]
#[ignore]
async fn provisioning_writes_never_resurrect_a_tombstone() {
    let client = client().await;
    let id = insert_pending(&client).await;
    assert!(client.begin_delete(id).await.unwrap());
    client.mark_deleted(id).await.unwrap();

    let status_applied = client.set_status(id, DatabaseStatus::Provisioning).await.unwrap();
    let ready_applied = client.set_ready(id, ready()).await.unwrap();
    client.set_failed(id, "boom").await.unwrap();

    let row = client.get_database(id).await.unwrap().unwrap();
    cleanup(&client, id).await;
    assert!(!status_applied, "set_status overwrote a tombstone");
    assert!(!ready_applied, "set_ready overwrote a tombstone");
    assert_eq!(row.status, DatabaseStatus::Deleted);
    assert_eq!(row.owner_password_enc, None);
    assert_eq!(row.error, None);
}

#[tokio::test]
#[ignore]
async fn provisioning_writes_still_apply_while_deleting_has_not_started() {
    let client = client().await;
    let id = insert_pending(&client).await;

    let status_applied = client.set_status(id, DatabaseStatus::Provisioning).await.unwrap();
    let ready_applied = client.set_ready(id, ready()).await.unwrap();

    let row = client.get_database(id).await.unwrap().unwrap();
    cleanup(&client, id).await;
    assert!(status_applied);
    assert!(ready_applied);
    assert_eq!(row.status, DatabaseStatus::Ready);
    assert_eq!(row.owner_password_enc.as_deref(), Some("v1:owner"));
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
}

#[tokio::test]
#[ignore]
async fn deleted_among_returns_only_tombstones() {
    let client = client().await;
    let live = insert_pending(&client).await;
    let tombstone = insert_pending(&client).await;
    assert!(client.begin_delete(tombstone).await.unwrap());
    client.mark_deleted(tombstone).await.unwrap();
    let unknown = DatabaseId::new();

    let deleted = client.deleted_among(&[live, tombstone, unknown]).await.unwrap();
    cleanup(&client, live).await;
    cleanup(&client, tombstone).await;
    assert_eq!(deleted, [tombstone].into());
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

    // Stored credentials exist: a different key must be refused.
    let id = insert_pending(&client).await;
    sqlx::query("UPDATE databases SET owner_password_enc = 'v1:x' WHERE id = $1")
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
        "SELECT count(*) FROM databases WHERE owner_password_enc IS NOT NULL OR platform_password_enc IS NOT NULL",
    )
    .fetch_one(client.pool())
    .await
    .unwrap();
    if sealed > 0 {
        eprintln!("skipping: {sealed} databases hold encrypted credentials in this environment");
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

// A reset finishing after a delete must not write a password back onto the tombstone.
#[tokio::test]
#[ignore]
async fn owner_password_is_only_stored_for_ready_databases() {
    let client = client().await;
    let ready_id = insert_pending(&client).await;
    client.set_ready(ready_id, ready()).await.unwrap();
    let deleted_id = insert_pending(&client).await;
    client.set_ready(deleted_id, ready()).await.unwrap();
    assert!(client.begin_delete(deleted_id).await.unwrap());
    client.mark_deleted(deleted_id).await.unwrap();

    let stored_ready = client.set_owner_password_enc(ready_id, "v1:new").await.unwrap();
    let stored_deleted = client.set_owner_password_enc(deleted_id, "v1:new").await.unwrap();
    let tombstone = client.get_database(deleted_id).await.unwrap().unwrap();
    cleanup(&client, ready_id).await;
    cleanup(&client, deleted_id).await;
    assert!(stored_ready);
    assert!(!stored_deleted);
    assert_eq!(tombstone.owner_password_enc, None);
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
