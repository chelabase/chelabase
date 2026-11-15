//! s3-endpoint m1: S3 access keys in the metadata. Each test makes its own scratch database
//! (`cm_…`, as `storage_webhooks_live` names and sweeps them) on the workbench stack's metadata
//! Postgres, so the shared metadata database is never migrated.
//!
//! ```text
//! just workbench test -p chelabase-metadata --test storage_s3_keys_live -- --run-ignored only
//! ```

use chelabase_metadata::{
    BranchStatus, DEMO_PROJECT_ID, DEMO_TENANT_ID, DatabaseStatus, MetadataClient, MetadataConfig,
    MetadataError, NewS3Key, STORAGE_S3_KEY_CAP,
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
    assert_eq!(got.role_oid, 16_384);
    assert_eq!(got.role_timeline, "0123456789abcdef0123456789abcdef");
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
    // A zero or negative OID, or an empty timeline, is no identity either.
    assert!(check_violation(&insert(Some(0), Some("t")).await.unwrap_err()).is_some());
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
