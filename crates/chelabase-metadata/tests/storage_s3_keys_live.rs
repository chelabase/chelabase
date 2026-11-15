//! s3-endpoint m1: S3 access keys in the metadata. Each test makes its own scratch database
//! (`cm_…`, as `storage_webhooks_live` names and sweeps them) on the workbench stack's metadata
//! Postgres, so the shared metadata database is never migrated.
//!
//! ```text
//! just workbench test -p chelabase-metadata --test storage_s3_keys_live -- --run-ignored only
//! ```

use chelabase_metadata::{
    BranchStatus, DEMO_PROJECT_ID, DEMO_TENANT_ID, DatabaseStatus, MetadataClient, MetadataConfig,
    MetadataError, NewEndpoint, NewS3Key, STORAGE_S3_KEY_CAP,
};
use chelabase_secrets::SecretKey;
use chelabase_types::{BranchId, DatabaseId, ProjectId, TenantId, s3_key};
use chrono::Utc;
use sqlx::postgres::PgPoolOptions;
use sqlx::{AssertSqlSafe, PgPool};
use uuid::Uuid;

fn url() -> String {
    std::env::var("CHELABASE_METADATA_DATABASE_URL").unwrap_or_else(|_| {
        "postgres://chelabase:chelabase@metadata-db:5432/chelabase_metadata".into()
    })
}

fn url_for(name: &str) -> String {
    let base = url();
    let (head, tail) = base.rsplit_once('/').expect("a database in the URL");
    let query = tail
        .split_once('?')
        .map(|(_, q)| format!("?{q}"))
        .unwrap_or_default();
    format!("{head}/{name}{query}")
}

struct Scratch {
    meta: PgPool,
    name: String,
    client: MetadataClient,
}

/// A key as the tests create it: its id, access key id and secret.
struct Made {
    id: Uuid,
    access_key_id: String,
    secret: String,
}

impl Scratch {
    async fn new() -> Scratch {
        let meta = PgPoolOptions::new()
            .max_connections(1)
            .connect(&url())
            .await
            .expect("metadata Postgres reachable (is the workbench stack up?)");
        let secs = Utc::now().timestamp() as u32;
        let name = format!("cm_{secs:08x}{}", &Uuid::new_v4().simple().to_string()[..4]);
        sqlx::raw_sql(AssertSqlSafe(format!("CREATE DATABASE {name}")))
            .execute(&meta)
            .await
            .unwrap();
        let client = MetadataClient::connect(&MetadataConfig {
            database_url: url_for(&name),
        })
        .await
        .unwrap()
        .with_secrets_key(SecretKey::generate());
        client.migrate().await.unwrap();
        Scratch { meta, name, client }
    }

    async fn finish(self) {
        self.client.pool().close().await;
        sqlx::raw_sql(AssertSqlSafe(format!(
            "DROP DATABASE {} WITH (FORCE)",
            self.name
        )))
        .execute(&self.meta)
        .await
        .unwrap();
    }

    /// A database and its main branch (same id).
    async fn branch(&self) -> BranchId {
        let id = DatabaseId::new();
        self.client
            .insert_database_pending(
                id,
                TenantId(DEMO_TENANT_ID),
                ProjectId(DEMO_PROJECT_ID),
                &format!("d-{id}"),
            )
            .await
            .unwrap();
        BranchId(id.as_uuid())
    }

    async fn create(&self, branch: BranchId, name: &str) -> Result<Made, MetadataError> {
        let pair = s3_key::generate();
        let id = Uuid::new_v4();
        self.client
            .create_s3_key(&NewS3Key {
                id,
                branch_id: branch,
                database: "app",
                role: "app_writer",
                name,
                access_key_id: &pair.access_key_id,
                secret: &pair.secret,
                role_oid: 16_384,
                role_timeline: "0123456789abcdef0123456789abcdef",
            })
            .await?;
        Ok(Made {
            id,
            access_key_id: pair.access_key_id,
            secret: pair.secret,
        })
    }

    async fn exec(&self, sql: &str, id: Uuid) -> Result<u64, sqlx::Error> {
        sqlx::query(AssertSqlSafe(sql.to_string()))
            .bind(id)
            .execute(self.client.pool())
            .await
            .map(|r| r.rows_affected())
    }
}

fn check_violation(err: &sqlx::Error) -> Option<String> {
    match err {
        sqlx::Error::Database(e) if e.code().as_deref() == Some("23514") => {
            e.constraint().map(str::to_string)
        }
        _ => None,
    }
}

/// AC 1: a created key looks up by its access key id with its branch, project, database, role,
/// role OID and the secret it was created with; the list shows it without any secret.
#[tokio::test]
#[ignore]
async fn create_lookup_round_trip() {
    let s = Scratch::new().await;
    let branch = s.branch().await;
    let made = s.create(branch, "backup tool").await.unwrap();

    let got = s
        .client
        .lookup_s3_key(&made.access_key_id)
        .await
        .unwrap()
        .expect("a live key");
    assert_eq!(got.key_id, made.id);
    assert_eq!(got.branch_id, branch);
    assert_eq!(got.database_id, DatabaseId(branch.as_uuid()));
    assert_eq!(got.project_id, ProjectId(DEMO_PROJECT_ID));
    assert_eq!(
        (got.database.as_str(), got.role.as_str()),
        ("app", "app_writer")
    );
    assert_eq!(got.role_oid, Some(16_384));
    assert_eq!(got.role_timeline.as_deref(), Some("0123456789abcdef0123456789abcdef"));
    assert_eq!(got.secret, made.secret);
    assert_eq!(got.branch_status, BranchStatus::Pending);
    assert_eq!(got.database_status, DatabaseStatus::Pending);
    assert!(
        !format!("{got:?}").contains(&made.secret),
        "Debug hides the secret"
    );

    // Stored sealed, never in the clear.
    let stored: String = sqlx::query_scalar("SELECT secret_enc FROM storage_s3_keys WHERE id = $1")
        .bind(made.id)
        .fetch_one(s.client.pool())
        .await
        .unwrap();
    assert!(stored.starts_with("v1:") && !stored.contains(&made.secret));

    let listed = s.client.list_live_s3_keys(branch).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(
        (
            listed[0].id,
            listed[0].access_key_id.as_str(),
            listed[0].name.as_str()
        ),
        (made.id, made.access_key_id.as_str(), "backup tool")
    );
    assert!(listed[0].last_used_at.is_none());
    assert!(
        !format!("{listed:?}").contains("v1:"),
        "no sealed value in a record"
    );

    // Unknown ids find nothing; touching stamps last_used_at (at most once a minute).
    assert!(
        s.client
            .lookup_s3_key("CBAKAAAAAAAAAAAAAAAA")
            .await
            .unwrap()
            .is_none()
    );
    s.client.touch_s3_key_used(made.id).await;
    let first = s.client.list_live_s3_keys(branch).await.unwrap()[0].last_used_at;
    assert!(first.is_some());
    s.client.touch_s3_key_used(made.id).await;
    let second = s.client.list_live_s3_keys(branch).await.unwrap()[0].last_used_at;
    assert_eq!(
        first, second,
        "a second touch within the minute writes nothing"
    );

    // A key whose branch or database is being deleted doesn't look up.
    s.exec(
        "UPDATE branches SET status = 'deleting' WHERE id = $1",
        branch.as_uuid(),
    )
    .await
    .unwrap();
    assert!(
        s.client
            .lookup_s3_key(&made.access_key_id)
            .await
            .unwrap()
            .is_none()
    );
    s.exec(
        "UPDATE branches SET status = 'ready' WHERE id = $1",
        branch.as_uuid(),
    )
    .await
    .unwrap();
    s.exec(
        "UPDATE databases SET status = 'deleting' WHERE id = $1",
        branch.as_uuid(),
    )
    .await
    .unwrap();
    assert!(
        s.client
            .lookup_s3_key(&made.access_key_id)
            .await
            .unwrap()
            .is_none()
    );
    s.finish().await;
}

/// AC 2: a sealed secret copied onto another key's row never opens there: the lookup errors
/// and returns no secret.
#[tokio::test]
#[ignore]
async fn swapped_sealed_value_never_opens() {
    let s = Scratch::new().await;
    let branch = s.branch().await;
    let a = s.create(branch, "a").await.unwrap();
    let b = s.create(branch, "b").await.unwrap();
    sqlx::query(
        "UPDATE storage_s3_keys SET secret_enc = (SELECT secret_enc FROM storage_s3_keys WHERE id = $1)
         WHERE id = $2",
    )
    .bind(a.id)
    .bind(b.id)
    .execute(s.client.pool())
    .await
    .unwrap();
    let swapped = s.client.lookup_s3_key(&b.access_key_id).await;
    assert!(
        matches!(swapped, Err(MetadataError::Secret(_))),
        "{:?}",
        swapped.map(|k| k.map(|k| k.key_id))
    );
    // The access key ids swapped instead: each row's secret still binds its own id.
    sqlx::query("UPDATE storage_s3_keys SET access_key_id = 'CBAKZZZZZZZZZZZZZZZZ' WHERE id = $1")
        .bind(a.id)
        .execute(s.client.pool())
        .await
        .unwrap();
    assert_eq!(
        s.client
            .lookup_s3_key("CBAKZZZZZZZZZZZZZZZZ")
            .await
            .unwrap()
            .unwrap()
            .secret,
        a.secret
    );
    // A client with another secrets key can't open it either.
    let other = MetadataClient::connect(&MetadataConfig {
        database_url: url_for(&s.name),
    })
    .await
    .unwrap()
    .with_secrets_key(SecretKey::generate());
    assert!(matches!(
        other.lookup_s3_key("CBAKZZZZZZZZZZZZZZZZ").await,
        Err(MetadataError::Secret(_))
    ));
    other.pool().close().await;
    s.finish().await;
}

/// AC 3: revoking sets `revoked_at` and clears `secret_enc` in one statement; a revoked key
/// doesn't look up, list, or revoke again.
#[tokio::test]
#[ignore]
async fn revoke_clears_the_secret() {
    let s = Scratch::new().await;
    let (branch, other) = (s.branch().await, s.branch().await);
    let made = s.create(branch, "k").await.unwrap();
    assert!(
        !s.client.revoke_s3_key(other, made.id).await.unwrap(),
        "another branch's"
    );
    assert!(s.client.revoke_s3_key(branch, made.id).await.unwrap());
    let (revoked, sealed): (Option<chrono::DateTime<Utc>>, Option<String>) =
        sqlx::query_as("SELECT revoked_at, secret_enc FROM storage_s3_keys WHERE id = $1")
            .bind(made.id)
            .fetch_one(s.client.pool())
            .await
            .unwrap();
    assert!(revoked.is_some() && sealed.is_none());
    assert!(
        s.client
            .lookup_s3_key(&made.access_key_id)
            .await
            .unwrap()
            .is_none()
    );
    assert!(s.client.list_live_s3_keys(branch).await.unwrap().is_empty());
    assert!(
        !s.client.revoke_s3_key(branch, made.id).await.unwrap(),
        "already revoked"
    );
    s.finish().await;
}

/// AC 3: setting `revoked_at` while keeping the sealed secret violates the CHECK, and so does
/// clearing the secret of a live key.
#[tokio::test]
#[ignore]
async fn revoked_with_secret_violates_check() {
    let s = Scratch::new().await;
    let branch = s.branch().await;
    let made = s.create(branch, "k").await.unwrap();
    for sql in [
        "UPDATE storage_s3_keys SET revoked_at = now() WHERE id = $1",
        "UPDATE storage_s3_keys SET secret_enc = NULL WHERE id = $1",
    ] {
        let err = s.exec(sql, made.id).await.unwrap_err();
        assert_eq!(
            check_violation(&err).as_deref(),
            Some("storage_s3_keys_revoked_unsealed"),
            "{sql}: {err}"
        );
    }
    // A revoked key can't get a secret back.
    s.client.revoke_s3_key(branch, made.id).await.unwrap();
    let err = s
        .exec(
            "UPDATE storage_s3_keys SET secret_enc = 'v1:x' WHERE id = $1",
            made.id,
        )
        .await
        .unwrap_err();
    assert_eq!(
        check_violation(&err).as_deref(),
        Some("storage_s3_keys_revoked_unsealed")
    );
    // Nor can it come back to life without one.
    let err = s
        .exec(
            "UPDATE storage_s3_keys SET revoked_at = NULL WHERE id = $1",
            made.id,
        )
        .await
        .unwrap_err();
    assert_eq!(
        check_violation(&err).as_deref(),
        Some("storage_s3_keys_revoked_unsealed")
    );
    s.finish().await;
}

/// The role-identity CHECK: a live key always has a role OID and timeline, on insert and on
/// update; a revoked key may lose either.
#[tokio::test]
#[ignore]
async fn live_key_needs_role_identity() {
    let s = Scratch::new().await;
    let branch = s.branch().await;
    let insert = |oid: Option<i64>, timeline: Option<&'static str>| {
        let pool = s.client.pool().clone();
        async move {
            sqlx::query(
                "INSERT INTO storage_s3_keys
                     (id, branch_id, database, role, name, access_key_id, secret_enc, role_oid, role_timeline)
                 VALUES (gen_random_uuid(), $1, 'app', 'r', 'n', $2, 'v1:x', $3, $4)",
            )
            .bind(branch.as_uuid())
            .bind(s3_key::generate().access_key_id)
            .bind(oid)
            .bind(timeline)
            .execute(&pool)
            .await
        }
    };
    for (oid, timeline) in [(None, Some("t")), (Some(16_384), None), (None, None)] {
        let err = insert(oid, timeline).await.unwrap_err();
        assert_eq!(
            check_violation(&err).as_deref(),
            Some("storage_s3_keys_live_role_identity"),
            "{oid:?} {timeline:?}: {err}"
        );
    }
    // A zero, negative or past-u32 OID, or an empty timeline, is no identity either.
    assert!(check_violation(&insert(Some(0), Some("t")).await.unwrap_err()).is_some());
    assert!(check_violation(&insert(Some(1 << 32), Some("t")).await.unwrap_err()).is_some());
    insert(Some(u32::MAX.into()), Some("t")).await.unwrap();
    assert!(check_violation(&insert(Some(16_384), Some("")).await.unwrap_err()).is_some());

    let made = s.create(branch, "k").await.unwrap();
    for sql in [
        "UPDATE storage_s3_keys SET role_oid = NULL WHERE id = $1",
        "UPDATE storage_s3_keys SET role_timeline = NULL WHERE id = $1",
    ] {
        let err = s.exec(sql, made.id).await.unwrap_err();
        assert_eq!(
            check_violation(&err).as_deref(),
            Some("storage_s3_keys_live_role_identity"),
            "{sql}: {err}"
        );
    }
    // Revoked: either may go null.
    s.client.revoke_s3_key(branch, made.id).await.unwrap();
    s.exec(
        "UPDATE storage_s3_keys SET role_timeline = NULL WHERE id = $1",
        made.id,
    )
    .await
    .unwrap();
    s.exec(
        "UPDATE storage_s3_keys SET role_oid = NULL WHERE id = $1",
        made.id,
    )
    .await
    .unwrap();
    s.finish().await;
}

/// AC 4: a branch holds at most [`STORAGE_S3_KEY_CAP`] live keys; the next is a conflict and
/// inserts nothing. Revoked keys don't count, and another branch has its own budget.
#[tokio::test]
#[ignore]
async fn the_live_key_cap() {
    let s = Scratch::new().await;
    let (branch, other) = (s.branch().await, s.branch().await);
    assert_eq!(STORAGE_S3_KEY_CAP, 100);
    let mut first = None;
    for i in 0..STORAGE_S3_KEY_CAP {
        let made = s.create(branch, &format!("k{i}")).await.unwrap();
        first.get_or_insert(made.id);
    }
    let over = s.create(branch, "over").await;
    assert!(
        matches!(over, Err(MetadataError::Conflict)),
        "{:?}",
        over.map(|m| m.id)
    );
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM storage_s3_keys WHERE branch_id = $1")
            .bind(branch.as_uuid())
            .fetch_one(s.client.pool())
            .await
            .unwrap();
    assert_eq!(count, STORAGE_S3_KEY_CAP, "nothing inserted");
    s.create(other, "elsewhere").await.unwrap();

    assert!(
        s.client
            .revoke_s3_key(branch, first.unwrap())
            .await
            .unwrap()
    );
    s.create(branch, "after revoke").await.unwrap();
    assert!(matches!(
        s.create(branch, "over again").await,
        Err(MetadataError::Conflict)
    ));
    assert_eq!(
        s.client.list_live_s3_keys(branch).await.unwrap().len() as i64,
        STORAGE_S3_KEY_CAP
    );
    s.finish().await;
}

/// AC 5: deleting the branch row takes its keys with it (live and revoked).
#[tokio::test]
#[ignore]
async fn branch_delete_cascades() {
    let s = Scratch::new().await;
    let (branch, other) = (s.branch().await, s.branch().await);
    let live = s.create(branch, "live").await.unwrap();
    let revoked = s.create(branch, "revoked").await.unwrap();
    s.client.revoke_s3_key(branch, revoked.id).await.unwrap();
    let kept = s.create(other, "kept").await.unwrap();
    s.exec("DELETE FROM branches WHERE id = $1", branch.as_uuid())
        .await
        .unwrap();
    let left: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM storage_s3_keys ORDER BY id")
        .fetch_all(s.client.pool())
        .await
        .unwrap();
    assert_eq!(left, vec![kept.id]);
    assert!(
        s.client
            .lookup_s3_key(&live.access_key_id)
            .await
            .unwrap()
            .is_none()
    );
    s.finish().await;
}

/// Links a fresh endpoint to `branch`, as provisioning does; returns its id.
async fn link_endpoint(s: &Scratch, branch: BranchId) -> String {
    let hex = || Uuid::new_v4().simple().to_string();
    let ep = format!("ep-{}", &hex()[..12]);
    let (tenant, timeline) = (hex(), hex());
    let endpoint = NewEndpoint {
        id: &ep,
        tenant_id: &tenant,
        timeline_id: &timeline,
        compute_token_sha256: "cd",
        owner_role: "postgres",
        owner_verifier: "SCRAM-SHA-256$v1",
        database_name: "app",
        role_copies: &[],
        routable: true,
        static_lsn: None,
    };
    assert!(
        s.client
            .create_endpoint_for_branch(branch, &timeline, &endpoint, false)
            .await
            .unwrap()
    );
    ep
}

/// The DDL receiver's transaction holds the endpoint row: a key creation waits for it (as a
/// secret data key's does), so a `DROP ROLE` forwarded first runs first and one forwarded
/// after the store sees the key.
#[tokio::test]
#[ignore]
async fn create_waits_for_the_ddl_transaction() {
    let s = Scratch::new().await;
    let branch = s.branch().await;
    let ep = link_endpoint(&s, branch).await;
    let ddl = s.client.begin_ddl(&ep).await.unwrap().expect("endpoint");

    let client = s.client.clone();
    let pair = s3_key::generate();
    let mut creating = tokio::spawn(async move {
        client
            .create_s3_key(&NewS3Key {
                id: Uuid::new_v4(),
                branch_id: branch,
                database: "app",
                role: "app_writer",
                name: "late",
                access_key_id: &pair.access_key_id,
                secret: &pair.secret,
                role_oid: 16_384,
                role_timeline: "0123456789abcdef0123456789abcdef",
            })
            .await
    });
    let waited = tokio::time::timeout(std::time::Duration::from_millis(400), &mut creating).await;
    assert!(
        waited.is_err(),
        "key creation must wait for the DDL transaction"
    );
    let during = s.client.list_live_s3_keys(branch).await.unwrap().len();
    ddl.commit().await.unwrap();
    let result = creating.await.unwrap();
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(during, 0);
    assert_eq!(s.client.list_live_s3_keys(branch).await.unwrap().len(), 1);
    s.finish().await;
}

/// The endpoint row itself is in the lock order (`FOR SHARE`, after the branch row), as for a
/// secret data key: a creation waits for any transaction holding the endpoint `FOR UPDATE`,
/// not only for one that also holds the branch row.
#[tokio::test]
#[ignore]
async fn create_waits_for_the_endpoint_row() {
    let s = Scratch::new().await;
    let branch = s.branch().await;
    let ep = link_endpoint(&s, branch).await;
    let mut holder = s.client.pool().begin().await.unwrap();
    sqlx::query("SELECT id FROM neon_endpoints WHERE id = $1 FOR UPDATE")
        .bind(&ep)
        .execute(&mut *holder)
        .await
        .unwrap();

    let client = s.client.clone();
    let pair = s3_key::generate();
    let mut creating = tokio::spawn(async move {
        client
            .create_s3_key(&NewS3Key {
                id: Uuid::new_v4(),
                branch_id: branch,
                database: "app",
                role: "app_writer",
                name: "held",
                access_key_id: &pair.access_key_id,
                secret: &pair.secret,
                role_oid: 16_384,
                role_timeline: "0123456789abcdef0123456789abcdef",
            })
            .await
    });
    let waited = tokio::time::timeout(std::time::Duration::from_millis(400), &mut creating).await;
    assert!(
        waited.is_err(),
        "key creation must wait for the endpoint row"
    );
    holder.commit().await.unwrap();
    let result = creating.await.unwrap();
    assert!(result.is_ok(), "{result:?}");
    s.finish().await;
}

/// A `backup` or `rehearsal` branch takes no key (lookup never serves one): `RowNotFound`,
/// nothing written.
#[tokio::test]
#[ignore]
async fn create_refuses_backup_and_rehearsal_branches() {
    let s = Scratch::new().await;
    for kind in ["backup", "rehearsal"] {
        let branch = s.branch().await;
        sqlx::query("UPDATE branches SET kind = $2 WHERE id = $1")
            .bind(branch.as_uuid())
            .bind(kind)
            .execute(s.client.pool())
            .await
            .unwrap();
        let made = s.create(branch, "refused").await;
        assert!(
            matches!(made, Err(MetadataError::Database(sqlx::Error::RowNotFound))),
            "{kind}: {:?}",
            made.map(|m| m.id)
        );
        let rows: i64 =
            sqlx::query_scalar("SELECT count(*) FROM storage_s3_keys WHERE branch_id = $1")
                .bind(branch.as_uuid())
                .fetch_one(s.client.pool())
                .await
                .unwrap();
        assert_eq!(rows, 0, "{kind}");
    }
    s.finish().await;
}

// ---- m6: S3 keys follow or are revoked with their role and database ----

const TIMELINE: &str = "0123456789abcdef0123456789abcdef";

impl Scratch {
    /// A key bound to `role` (OID `oid`) and `database` on `branch`.
    async fn create_for(&self, branch: BranchId, name: &str, role: &str, database: &str, oid: i64) -> Uuid {
        let pair = s3_key::generate();
        let id = Uuid::new_v4();
        self.client
            .create_s3_key(&NewS3Key {
                id,
                branch_id: branch,
                database,
                role,
                name,
                access_key_id: &pair.access_key_id,
                secret: &pair.secret,
                role_oid: oid,
                role_timeline: TIMELINE,
            })
            .await
            .unwrap();
        id
    }

    /// `(role, database, revoked, secret cleared)` of a key, read from the row.
    async fn row(&self, id: Uuid) -> (String, String, bool, bool) {
        sqlx::query_as(
            "SELECT role, database, revoked_at IS NOT NULL, secret_enc IS NULL
             FROM storage_s3_keys WHERE id = $1",
        )
        .bind(id)
        .fetch_one(self.client.pool())
        .await
        .unwrap()
    }

    /// An `sql` row named `name` on the endpoint.
    async fn sql_row(&self, ep: &str, name: &str) {
        sqlx::query(
            "INSERT INTO neon_roles (endpoint_id, name, scram_verifier, origin)
             VALUES ($1, $2, 'SCRAM-SHA-256$v', 'sql')",
        )
        .bind(ep)
        .bind(name)
        .execute(self.client.pool())
        .await
        .unwrap();
    }
}

/// AC 1 (metadata side): the DDL transaction's `live_s3_keys` lists the branch's live S3 keys
/// bound to the role, by name, and nothing revoked or of another role.
#[tokio::test]
#[ignore]
async fn ddl_lists_live_s3_keys_of_a_role() {
    let s = Scratch::new().await;
    let branch = s.branch().await;
    let ep = link_endpoint(&s, branch).await;
    let live = s.create_for(branch, "ci", "writer", "app", 16_400).await;
    let gone = s.create_for(branch, "old", "writer", "app", 16_400).await;
    s.create_for(branch, "other", "reader", "app", 16_401).await;
    assert!(s.client.revoke_s3_key(branch, gone).await.unwrap());
    let mut ddl = s.client.begin_ddl(&ep).await.unwrap().expect("endpoint");
    assert_eq!(ddl.live_s3_keys("writer").await.unwrap(), vec![(live, "ci".to_string())]);
    assert!(ddl.live_s3_keys("nobody").await.unwrap().is_empty());
    drop(ddl);
    s.finish().await;
}

/// AC 2: deleting a role's row through the roles API revokes the branch's live S3 keys bound to
/// the name in the same transaction (secret cleared) and returns their ids for publishing.
/// Other roles' keys and other branches' keys stay.
#[tokio::test]
#[ignore]
async fn role_delete_revokes_s3_keys() {
    let s = Scratch::new().await;
    let branch = s.branch().await;
    let ep = link_endpoint(&s, branch).await;
    let other_branch = s.branch().await;
    link_endpoint(&s, other_branch).await;
    s.sql_row(&ep, "writer").await;
    let bound = s.create_for(branch, "ci", "writer", "app", 16_400).await;
    let other_role = s.create_for(branch, "r", "reader", "app", 16_401).await;
    let other_branch_key = s.create_for(other_branch, "ci", "writer", "app", 16_400).await;

    let deleted = s.client.delete_role_copy_revoking(&ep, "writer").await.unwrap();
    assert!(deleted.row);
    assert_eq!(deleted.revoked_s3_keys, vec![bound]);
    assert_eq!(s.row(bound).await, ("writer".into(), "app".into(), true, true));
    assert!(!s.row(other_role).await.2);
    assert!(!s.row(other_branch_key).await.2);

    // No row: nothing deleted, nothing revoked (as for data keys).
    let again = s.client.delete_role_copy_revoking(&ep, "writer").await.unwrap();
    assert!(!again.row);
    assert!(again.revoked_s3_keys.is_empty());
    s.finish().await;
}

/// AC 3: a dropped database revokes the S3 keys pinned to it (DDL receiver and resync alike);
/// a renamed one takes them along. Each returns the ids it changed.
#[tokio::test]
#[ignore]
async fn database_drop_and_rename() {
    let s = Scratch::new().await;
    let branch = s.branch().await;
    let ep = link_endpoint(&s, branch).await;
    let mut ddl = s.client.begin_ddl(&ep).await.unwrap().expect("endpoint");
    ddl.upsert_database("d", "postgres").await.unwrap();
    ddl.upsert_database("gone", "postgres").await.unwrap();
    ddl.commit().await.unwrap();
    let on_d = s.create_for(branch, "d-key", "writer", "d", 16_400).await;
    let on_app = s.create_for(branch, "app-key", "writer", "app", 16_400).await;
    let on_gone = s.create_for(branch, "gone-key", "writer", "gone", 16_400).await;

    // Rename d -> e: the key follows.
    let mut ddl = s.client.begin_ddl(&ep).await.unwrap().expect("endpoint");
    assert!(ddl.rename_database("d", "e").await.unwrap());
    assert_eq!(ddl.s3_keys_changed(), vec![on_d]);
    ddl.commit().await.unwrap();
    assert_eq!(s.row(on_d).await, ("writer".into(), "e".into(), false, false));

    // Drop e: revoked, secret cleared.
    let mut ddl = s.client.begin_ddl(&ep).await.unwrap().expect("endpoint");
    assert!(ddl.remove_database("e").await.unwrap());
    assert_eq!(ddl.s3_keys_changed(), vec![on_d]);
    ddl.commit().await.unwrap();
    assert_eq!(s.row(on_d).await, ("writer".into(), "e".into(), true, true));
    assert!(!s.row(on_app).await.2);

    // The resync sees `gone` missing from the catalog: revoked there too.
    sqlx::query("UPDATE neon_endpoints SET dbs_forwarded_at = NULL WHERE id = $1")
        .bind(&ep)
        .execute(s.client.pool())
        .await
        .unwrap();
    let (_, version) = s.client.role_copies(&ep).await.unwrap();
    let write = chelabase_metadata::ResyncWrite {
        upserts: vec![],
        removals: vec![],
        databases: vec![chelabase_metadata::ObservedDatabase {
            name: "app".into(),
            owner: "postgres".into(),
            system: false,
        }],
    };
    let applied = s.client.apply_resync(&ep, branch, version, &write).await.unwrap();
    let chelabase_metadata::ResyncApplied::Applied { s3_keys, .. } = applied else {
        panic!("{applied:?}")
    };
    assert_eq!(s3_keys, vec![on_gone]);
    assert_eq!(s.row(on_gone).await, ("writer".into(), "gone".into(), true, true));
    assert!(!s.row(on_app).await.2);
    s.finish().await;
}

/// Renames of a role move its S3 keys, as they move its data keys: through the DDL receiver
/// (`rename_copy`, row or no row) and the control plane (`rename_role_copy`, which refuses a
/// new name that still backs live S3 keys).
#[tokio::test]
#[ignore]
async fn role_rename_moves_s3_keys() {
    let s = Scratch::new().await;
    let branch = s.branch().await;
    let ep = link_endpoint(&s, branch).await;
    let key = s.create_for(branch, "ci", "writer", "app", 16_400).await;

    let mut ddl = s.client.begin_ddl(&ep).await.unwrap().expect("endpoint");
    assert!(!ddl.rename_copy("writer", "w2", None).await.unwrap(), "no row moved");
    assert_eq!(ddl.s3_keys_changed(), vec![key]);
    ddl.commit().await.unwrap();
    assert_eq!(s.row(key).await.0, "w2");

    let renamed = s
        .client
        .rename_role_copy(&ep, branch, "w2", "w3", None, None)
        .await
        .unwrap();
    let chelabase_metadata::RenameCopyOutcome::Renamed { s3_keys, .. } = renamed else {
        panic!("{renamed:?}")
    };
    assert_eq!(s3_keys, vec![key]);
    assert_eq!(s.row(key).await.0, "w3");

    // A name a live S3 key still names is taken.
    s.create_for(branch, "other", "reader", "app", 16_401).await;
    let taken = s
        .client
        .rename_role_copy(&ep, branch, "w3", "reader", None, None)
        .await
        .unwrap();
    assert_eq!(taken, chelabase_metadata::RenameCopyOutcome::Taken);
    assert_eq!(s.row(key).await.0, "w3");
    s.finish().await;
}

/// The resync's repairs on S3 keys: each applies only while the row still looks as the plan
/// saw it, and the ids it changed come back. A backfill (never planned for an S3 key, whose
/// OID is always set) revokes.
#[tokio::test]
#[ignore]
async fn fix_s3_key_roles_applies_guarded_repairs() {
    use chelabase_metadata::KeyRoleFix;
    let s = Scratch::new().await;
    let branch = s.branch().await;
    let gone = s.create_for(branch, "gone", "dropped", "app", 16_400).await;
    let moved = s.create_for(branch, "moved", "old", "app", 16_401).await;
    let restamped = s.create_for(branch, "re", "app", "app", 16_402).await;
    let stale = s.create_for(branch, "stale", "app", "app", 16_403).await;
    let odd = s.create_for(branch, "odd", "x", "app", 16_404).await;
    let next = "fedcba9876543210fedcba9876543210";
    let fixes = vec![
        KeyRoleFix::Revoke { id: gone, role: "dropped".into(), oid: Some(16_400) },
        KeyRoleFix::Follow { id: moved, from: "old".into(), to: "new".into(), oid: 16_401, timeline: TIMELINE.into() },
        KeyRoleFix::Restamp { id: restamped, role: "app".into(), oid: 16_402, from: Some(TIMELINE.into()), timeline: next.into() },
        // Saw another OID: left alone.
        KeyRoleFix::Revoke { id: stale, role: "app".into(), oid: Some(1) },
        KeyRoleFix::Backfill { id: odd, role: "x".into(), oid: 16_404, timeline: TIMELINE.into() },
        KeyRoleFix::Bypass { id: stale, oid: 16_403, bypass_rls: true },
    ];
    let mut changed = s.client.fix_s3_key_roles(branch, &fixes).await.unwrap();
    changed.sort();
    let mut want = vec![gone, moved, restamped, odd];
    want.sort();
    assert_eq!(changed, want);
    assert_eq!(s.row(gone).await, ("dropped".into(), "app".into(), true, true));
    assert_eq!(s.row(moved).await, ("new".into(), "app".into(), false, false));
    let timeline: String = sqlx::query_scalar("SELECT role_timeline FROM storage_s3_keys WHERE id = $1")
        .bind(restamped)
        .fetch_one(s.client.pool())
        .await
        .unwrap();
    assert_eq!(timeline, next);
    assert!(!s.row(stale).await.2);
    assert!(s.row(odd).await.2);
    // Applied again: the rows no longer match, nothing changes.
    assert!(s.client.fix_s3_key_roles(branch, &fixes[..3]).await.unwrap().is_empty());
    s.finish().await;
}

/// AC 7: a role delete and a database drop on one branch, each revoking data keys and S3 keys,
/// run concurrently many times without a deadlock (data keys first, then S3 keys, everywhere,
/// after the endpoint row).
#[tokio::test]
#[ignore]
async fn concurrent_role_delete_and_db_drop() {
    let s = Scratch::new().await;
    let branch = s.branch().await;
    let ep = link_endpoint(&s, branch).await;
    for round in 0..20 {
        let db = format!("db{round}");
        let role = format!("r{round}");
        let mut ddl = s.client.begin_ddl(&ep).await.unwrap().expect("endpoint");
        ddl.upsert_database(&db, "postgres").await.unwrap();
        ddl.commit().await.unwrap();
        s.sql_row(&ep, &role).await;
        // One key on the role and the database, so both transactions touch it.
        let key = s.create_for(branch, "both", &role, &db, 16_400).await;
        let mut sha256 = [0u8; 32];
        sha256[..16].copy_from_slice(Uuid::new_v4().as_bytes());
        let data_key = Uuid::new_v4();
        s.client
            .create_secret_key(&chelabase_metadata::NewDataKey {
                id: data_key,
                branch_id: branch,
                kind: chelabase_types::data_key::DataKeyKind::Secret,
                role: role.clone(),
                name: "dk".into(),
                prefix: "cb_sk_x".into(),
                sha256,
                sealed: None,
                databases: Some(vec![db.clone()]),
                role_oid: Some(16_400),
                role_timeline: Some(TIMELINE.into()),
                role_bypass_rls: false,
            })
            .await
            .unwrap();
        let (a, b) = (s.client.clone(), s.client.clone());
        let (ep_a, ep_b, role_a, db_b) = (ep.clone(), ep.clone(), role.clone(), db.clone());
        let deleting = tokio::spawn(async move { a.delete_role_copy_revoking(&ep_a, &role_a).await });
        let dropping = tokio::spawn(async move {
            let mut ddl = b.begin_ddl(&ep_b).await?.expect("endpoint");
            ddl.remove_database(&db_b).await?;
            let changed = ddl.s3_keys_changed();
            ddl.commit().await?;
            Ok::<_, MetadataError>(changed)
        });
        let deleted = deleting.await.unwrap().expect("role delete, no deadlock");
        let dropped = dropping.await.unwrap().expect("database drop, no deadlock");
        // Exactly one of them revoked the key.
        assert_eq!(
            deleted.revoked_s3_keys.len() + dropped.len(),
            1,
            "round {round}: {deleted:?} {dropped:?}"
        );
        assert!(s.row(key).await.2);
        let data_revoked: bool =
            sqlx::query_scalar("SELECT revoked_at IS NOT NULL FROM data_api_keys WHERE id = $1")
                .bind(data_key)
                .fetch_one(s.client.pool())
                .await
                .unwrap();
        assert!(data_revoked, "round {round}");
    }
    s.finish().await;
}
