//! Opt-in tests against a real metadata Postgres (`just deps-up` first):
//!
//! ```sh
//! cargo test -p cheladb-metadata --test live_db -- --ignored --test-threads=1
//! ```
//!
//! They use the seeded demo tenant/project and delete the rows they create.

use cheladb_metadata::{
    DatabaseStatus, MetadataClient, MetadataConfig, NewEndpoint, ReadyDatabase, DEMO_PROJECT_ID,
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

/// Deletes the database row and any endpoint still linked to it.
async fn cleanup(client: &MetadataClient, id: DatabaseId) {
    sqlx::query("DELETE FROM neon_endpoints WHERE id = (SELECT endpoint_id FROM databases WHERE id = $1)")
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

/// Creates and links an endpoint; returns its id.
async fn link_endpoint(client: &MetadataClient, id: DatabaseId) -> String {
    let ep = ep_id();
    let (tenant, timeline) = (hex32(), hex32());
    assert!(client
        .create_endpoint_for_database(id, &new_endpoint(&ep, &tenant, &timeline))
        .await
        .unwrap());
    ep
}

async fn endpoint_exists(client: &MetadataClient, ep: &str) -> bool {
    sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM neon_endpoints WHERE id = $1)")
        .bind(ep)
        .fetch_one(client.pool())
        .await
        .unwrap()
}

fn ready() -> ReadyDatabase<'static> {
    ReadyDatabase {
        database_name: "db",
        owner_role: "postgres",
        owner_password_enc: "v1:owner",
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
    let ready_applied = client.set_ready(id, "ep-000000000000", ready()).await.unwrap();
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

    let ep = link_endpoint(&client, id).await;
    let status_applied = client.set_status(id, DatabaseStatus::Provisioning).await.unwrap();
    let ready_applied = client.set_ready(id, &ep, ready()).await.unwrap();

    let row = client.get_database(id).await.unwrap().unwrap();
    cleanup(&client, id).await;
    assert!(status_applied);
    assert!(ready_applied);
    assert_eq!(row.status, DatabaseStatus::Ready);
    assert_eq!(row.endpoint_id.as_deref(), Some(ep.as_str()));
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
        "SELECT count(*) FROM databases WHERE owner_password_enc IS NOT NULL",
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
        .replace_owner_secret(not_ready, &ep_not_ready, "postgres", "SCRAM-SHA-256$v2", "v1:new")
        .await
        .unwrap();
    let untouched = verifier(&ep_not_ready).await;
    let untouched_row = client.get_database(not_ready).await.unwrap().unwrap();

    // Ready and linked: everything changes together.
    let ok = insert_pending(&client).await;
    let ep_ok = link_endpoint(&client, ok).await;
    assert!(client.set_ready(ok, &ep_ok, ready()).await.unwrap());
    let stored = client
        .replace_owner_secret(ok, &ep_ok, "postgres", "SCRAM-SHA-256$v2", "v1:new")
        .await
        .unwrap();
    let changed = verifier(&ep_ok).await;
    let changed_row = client.get_database(ok).await.unwrap().unwrap();

    // Wrong endpoint, and an unknown role: refused, nothing changes.
    let wrong_ep = client
        .replace_owner_secret(ok, "ep-ffffffffffff", "postgres", "SCRAM-SHA-256$v3", "v1:x")
        .await
        .unwrap();
    let unknown_role = client
        .replace_owner_secret(ok, &ep_ok, "nobody", "SCRAM-SHA-256$v3", "v1:x")
        .await
        .unwrap();
    let after = client.get_database(ok).await.unwrap().unwrap();
    let after_verifier = verifier(&ep_ok).await;

    // Deleted: refused because the status is no longer ready.
    let gone = insert_pending(&client).await;
    let ep_gone = link_endpoint(&client, gone).await;
    assert!(client.set_ready(gone, &ep_gone, ready()).await.unwrap());
    assert!(client.begin_delete(gone).await.unwrap());
    client.mark_deleted(gone).await.unwrap();
    let on_tombstone = client
        .replace_owner_secret(gone, &ep_gone, "postgres", "SCRAM-SHA-256$v2", "v1:new")
        .await
        .unwrap();
    let tombstone = client.get_database(gone).await.unwrap().unwrap();

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

// The migration already ran (client() applies it); re-run its UPDATE on a legacy-shaped row.
#[tokio::test]
#[ignore]
async fn migration_tombstones_rows_without_an_endpoint() {
    let client = client().await;
    let legacy = insert_pending(&client).await;
    sqlx::query("UPDATE databases SET status = 'ready', owner_password_enc = 'v1:old' WHERE id = $1")
        .bind(legacy.as_uuid())
        .execute(client.pool())
        .await
        .unwrap();
    let linked = insert_pending(&client).await;
    let ep = link_endpoint(&client, linked).await;
    assert!(client.set_ready(linked, &ep, ready()).await.unwrap());

    sqlx::query(
        "UPDATE databases
            SET status = 'deleted', deleted_at = now(), updated_at = now(), owner_password_enc = NULL
          WHERE endpoint_id IS NULL AND status <> 'deleted'",
    )
    .execute(client.pool())
    .await
    .unwrap();

    let legacy_row = client.get_database(legacy).await.unwrap().unwrap();
    let linked_row = client.get_database(linked).await.unwrap().unwrap();
    let deleted_at_set: bool =
        sqlx::query_scalar("SELECT deleted_at IS NOT NULL FROM databases WHERE id = $1")
            .bind(legacy.as_uuid())
            .fetch_one(client.pool())
            .await
            .unwrap();
    cleanup(&client, legacy).await;
    cleanup(&client, linked).await;
    assert_eq!(legacy_row.status, DatabaseStatus::Deleted);
    assert_eq!(legacy_row.owner_password_enc, None);
    assert!(deleted_at_set);
    assert_eq!(linked_row.status, DatabaseStatus::Ready);
    assert_eq!(linked_row.owner_password_enc.as_deref(), Some("v1:owner"));
}

#[tokio::test]
#[ignore]
async fn create_endpoint_links_and_refuses_a_deleting_database() {
    let client = client().await;
    let id = insert_pending(&client).await;
    let ep = ep_id();
    let (tenant, timeline) = (hex32(), hex32());
    let created = client
        .create_endpoint_for_database(id, &new_endpoint(&ep, &tenant, &timeline))
        .await
        .unwrap();
    let row = client.get_database(id).await.unwrap().unwrap();
    let roles = client.neon_roles(&ep).await.unwrap();
    let dbs = client.neon_databases(&ep).await.unwrap();
    let endpoint = client.neon_endpoint(&ep).await.unwrap().unwrap();

    // Already linked: refused, and the second endpoint isn't written.
    let ep2 = ep_id();
    let again = client
        .create_endpoint_for_database(id, &new_endpoint(&ep2, &tenant, &timeline))
        .await
        .unwrap();
    let ep2_written = endpoint_exists(&client, &ep2).await;

    // Deleting, deleted and missing databases are refused without writing anything.
    let deleting = insert_pending(&client).await;
    assert!(client.begin_delete(deleting).await.unwrap());
    let deleted = insert_pending(&client).await;
    assert!(client.begin_delete(deleted).await.unwrap());
    client.mark_deleted(deleted).await.unwrap();
    let mut refused = Vec::new();
    for target in [deleting, deleted, DatabaseId::new()] {
        let ep = ep_id();
        let ok = client
            .create_endpoint_for_database(target, &new_endpoint(&ep, &tenant, &timeline))
            .await
            .unwrap();
        refused.push((ok, endpoint_exists(&client, &ep).await));
    }
    for id in [id, deleting, deleted] {
        cleanup(&client, id).await;
    }

    assert!(created);
    assert_eq!(row.endpoint_id.as_deref(), Some(ep.as_str()));
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
    let id = insert_pending(&client).await;
    let ep = link_endpoint(&client, id).await;
    assert!(client.set_ready(id, &ep, ready()).await.unwrap());

    client.delete_endpoint(&ep).await.unwrap();
    client.delete_endpoint(&ep).await.unwrap(); // missing is fine

    let row = client.get_database(id).await.unwrap().unwrap();
    let roles = client.neon_roles(&ep).await.unwrap();
    let dbs = client.neon_databases(&ep).await.unwrap();
    let exists = endpoint_exists(&client, &ep).await;
    cleanup(&client, id).await;
    assert!(!exists);
    assert!(roles.is_empty() && dbs.is_empty());
    assert_eq!(row.endpoint_id, None);
    assert_eq!(row.status, DatabaseStatus::Ready);
}

#[tokio::test]
#[ignore]
async fn list_endpoints_for_reconcile_joins_status() {
    let client = client().await;
    let linked = insert_pending(&client).await;
    let ep_linked = link_endpoint(&client, linked).await;
    assert!(client.set_ready(linked, &ep_linked, ready()).await.unwrap());
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
        .replace_owner_secret(linked, &ep_linked, "postgres", "SCRAM-SHA-256$v2", "v1:n")
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

    assert_eq!(a.database_id, Some(linked));
    assert_eq!(a.database_status, Some(DatabaseStatus::Ready));
    assert!(a.needs_reconfigure);
    assert_eq!(b.database_id, None);
    assert_eq!(b.database_status, None);
    assert_eq!(b.tenant_id, tenant);
    assert!(!b.needs_reconfigure);
    assert_eq!(c.database_status, Some(DatabaseStatus::Deleting));
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
