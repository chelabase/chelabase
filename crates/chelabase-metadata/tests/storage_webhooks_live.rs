//! Object storage m18: storage webhook endpoints and their attempts in the metadata. Each test
//! makes its own scratch database (`cm_…`, as `members_live` names and sweeps them) on the
//! workbench stack's metadata Postgres, so the shared metadata database is never migrated.
//!
//! ```text
//! just workbench test -p chelabase-metadata --test storage_webhooks_live -- --run-ignored only
//! ```

use chelabase_metadata::{
    DEMO_PROJECT_ID, DEMO_TENANT_ID, MetadataClient, MetadataConfig, NewStorageWebhook,
    STORAGE_WEBHOOK_ATTEMPTS_KEPT, STORAGE_WEBHOOK_CAP, StorageWebhookAttempt, StorageWebhookPatch,
};
use chelabase_types::{DatabaseId, ProjectId, TenantId};
use chrono::{Duration, Utc};
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
        .unwrap();
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

    async fn database(&self) -> Uuid {
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
        id.as_uuid()
    }

    async fn webhook(&self, db: Uuid, url: &str) -> Uuid {
        let id = Uuid::new_v4();
        let events = vec!["object.created".to_string()];
        self.client
            .create_storage_webhook(&NewStorageWebhook {
                id,
                database_id: db,
                url,
                events: &events,
                bucket: Some("avatars"),
                prefix: None,
                secret_enc: "v1:sealed",
            })
            .await
            .unwrap()
            .expect("under the cap");
        id
    }
}

fn attempt(outcome: &str, at: chrono::DateTime<Utc>) -> StorageWebhookAttempt {
    StorageWebhookAttempt {
        event_id: Uuid::new_v4(),
        event_type: "object.created".into(),
        attempt: 1,
        outcome: outcome.into(),
        status: (outcome == "delivered").then_some(200),
        duration_ms: 12,
        attempted_at: at,
    }
}

/// Create, read, patch, rotate and delete stay inside their database; the API type has no
/// secret; only the worker's target carries it; a disabled endpoint or a deleting database's is
/// no target.
#[tokio::test]
#[ignore]
async fn storage_webhooks_round_trip_within_their_database() {
    let s = Scratch::new().await;
    let (db, other) = (s.database().await, s.database().await);
    let id = s.webhook(db, "https://example.com/hook").await;

    let got = s.client.get_storage_webhook(db, id).await.unwrap().unwrap();
    assert_eq!(
        (got.url.as_str(), got.bucket.as_deref(), got.enabled),
        ("https://example.com/hook", Some("avatars"), true)
    );
    assert!(
        !serde_json::to_string(&got).unwrap().contains("sealed"),
        "the API type has no secret"
    );
    assert!(
        s.client
            .get_storage_webhook(other, id)
            .await
            .unwrap()
            .is_none(),
        "another database's"
    );
    assert_eq!(s.client.list_storage_webhooks(db).await.unwrap().len(), 1);
    assert!(
        s.client
            .list_storage_webhooks(other)
            .await
            .unwrap()
            .is_empty()
    );

    let targets = s.client.storage_webhook_targets().await.unwrap();
    assert_eq!(targets.len(), 1);
    assert_eq!(targets[0].secret_enc, "v1:sealed");
    assert!(
        s.client
            .storage_webhook_target(other, id)
            .await
            .unwrap()
            .is_none()
    );

    let patched = s
        .client
        .update_storage_webhook(
            db,
            id,
            &StorageWebhookPatch {
                bucket: Some(None),
                prefix: Some(Some("img/".into())),
                events: Some(vec!["object.deleted".into(), "object.moved".into()]),
                ..Default::default()
            },
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (patched.bucket, patched.prefix.as_deref()),
        (None, Some("img/"))
    );
    assert_eq!(patched.events, ["object.deleted", "object.moved"]);
    assert!(
        s.client
            .update_storage_webhook(other, id, &StorageWebhookPatch::default())
            .await
            .unwrap()
            .is_none()
    );

    // Disabled by a member: no target; enabled again: delivery restarts now, failures cleared.
    let off = StorageWebhookPatch {
        enabled: Some(false),
        ..Default::default()
    };
    let disabled = s
        .client
        .update_storage_webhook(db, id, &off)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (disabled.enabled, disabled.disabled_reason.as_deref()),
        (false, Some("member"))
    );
    assert!(s.client.storage_webhook_targets().await.unwrap().is_empty());
    assert!(
        s.client
            .storage_webhook_target(db, id)
            .await
            .unwrap()
            .is_some(),
        "a test event still finds it"
    );
    let on = StorageWebhookPatch {
        enabled: Some(true),
        ..Default::default()
    };
    let enabled = s
        .client
        .update_storage_webhook(db, id, &on)
        .await
        .unwrap()
        .unwrap();
    assert!(enabled.enabled && enabled.disabled_reason.is_none());
    assert!(enabled.enabled_since > got.enabled_since);

    let rotated = s
        .client
        .rotate_storage_webhook_secret(db, id, "v1:new")
        .await
        .unwrap();
    assert!(rotated.is_some());
    assert!(
        s.client
            .rotate_storage_webhook_secret(other, id, "v1:x")
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        s.client
            .storage_webhook_target(db, id)
            .await
            .unwrap()
            .unwrap()
            .secret_enc,
        "v1:new"
    );

    sqlx::query("UPDATE databases SET status = 'deleting' WHERE id = $1")
        .bind(db)
        .execute(s.client.pool())
        .await
        .unwrap();
    assert!(
        s.client.storage_webhook_targets().await.unwrap().is_empty(),
        "a deleting database's"
    );

    assert!(!s.client.delete_storage_webhook(other, id).await.unwrap());
    assert!(s.client.delete_storage_webhook(db, id).await.unwrap());
    assert!(
        s.client
            .get_storage_webhook(db, id)
            .await
            .unwrap()
            .is_none()
    );
    s.finish().await;
}

#[tokio::test]
#[ignore]
async fn a_database_keeps_at_most_the_cap() {
    let s = Scratch::new().await;
    let db = s.database().await;
    for i in 0..STORAGE_WEBHOOK_CAP {
        s.webhook(db, &format!("https://example.com/{i}")).await;
    }
    let events = vec!["object.created".to_string()];
    let over = s
        .client
        .create_storage_webhook(&NewStorageWebhook {
            id: Uuid::new_v4(),
            database_id: db,
            url: "https://example.com/over",
            events: &events,
            bucket: None,
            prefix: None,
            secret_enc: "v1:x",
        })
        .await
        .unwrap();
    assert!(over.is_none());
    // The table refuses an unknown event type and an empty list.
    for bad in [vec!["object.read".to_string()], vec![]] {
        let other = s.database().await;
        assert!(
            s.client
                .create_storage_webhook(&NewStorageWebhook {
                    id: Uuid::new_v4(),
                    database_id: other,
                    url: "https://example.com/",
                    events: &bad,
                    bucket: None,
                    prefix: None,
                    secret_enc: "v1:x",
                })
                .await
                .is_err(),
            "{bad:?}"
        );
    }
    s.finish().await;
}

/// Attempts: a failure starts a run (`failing_since` set once), more failures keep its start, a
/// success ends it; the log keeps the newest attempts only; disabling for failure needs the
/// same run still going.
#[tokio::test]
#[ignore]
async fn attempts_track_the_failure_run_and_stay_bounded() {
    let s = Scratch::new().await;
    let db = s.database().await;
    let id = s.webhook(db, "https://example.com/hook").await;
    let t0 = Utc::now() - Duration::days(4);

    let first = s
        .client
        .record_storage_webhook_attempt(id, &attempt("http_error", t0))
        .await
        .unwrap();
    assert_eq!(
        first.map(|t| t.timestamp_micros()),
        Some(t0.timestamp_micros())
    );
    let again = s
        .client
        .record_storage_webhook_attempt(id, &attempt("timeout", t0 + Duration::hours(1)))
        .await
        .unwrap();
    assert_eq!(again, first, "the run keeps its start");
    let w = s.client.get_storage_webhook(db, id).await.unwrap().unwrap();
    assert_eq!(w.consecutive_failures, 2);

    let ok = s
        .client
        .record_storage_webhook_attempt(id, &attempt("delivered", t0 + Duration::hours(2)))
        .await
        .unwrap();
    assert_eq!(ok, None);
    let w = s.client.get_storage_webhook(db, id).await.unwrap().unwrap();
    assert_eq!((w.consecutive_failures, w.failing_since), (0, None));
    // A stale run doesn't disable it.
    assert!(
        !s.client
            .disable_failing_storage_webhook(id, first.unwrap())
            .await
            .unwrap()
    );

    let run = s
        .client
        .record_storage_webhook_attempt(id, &attempt("connect_error", t0))
        .await
        .unwrap()
        .unwrap();
    assert!(
        s.client
            .disable_failing_storage_webhook(id, run)
            .await
            .unwrap()
    );
    let w = s.client.get_storage_webhook(db, id).await.unwrap().unwrap();
    assert_eq!(
        (w.enabled, w.disabled_reason.as_deref()),
        (false, Some("failing"))
    );
    assert!(w.failing_since.is_some(), "the failure state stays visible");
    assert!(
        !s.client
            .disable_failing_storage_webhook(id, run)
            .await
            .unwrap(),
        "once"
    );

    for _ in 0..STORAGE_WEBHOOK_ATTEMPTS_KEPT + 5 {
        s.client
            .record_storage_webhook_attempt(id, &attempt("http_error", Utc::now()))
            .await
            .unwrap();
    }
    let kept = s
        .client
        .list_storage_webhook_attempts(db, id, 1000)
        .await
        .unwrap();
    assert_eq!(kept.len() as i64, STORAGE_WEBHOOK_ATTEMPTS_KEPT);
    assert!(
        s.client
            .list_storage_webhook_attempts(Uuid::new_v4(), id, 10)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        s.client
            .list_storage_webhook_attempts(db, id, 3)
            .await
            .unwrap()
            .len(),
        3
    );

    // An attempt for a deleted endpoint records nothing.
    assert!(s.client.delete_storage_webhook(db, id).await.unwrap());
    assert_eq!(
        s.client
            .record_storage_webhook_attempt(id, &attempt("delivered", Utc::now()))
            .await
            .unwrap(),
        None
    );
    s.finish().await;
}

/// A failure after a quiet spell longer than the retry window starts a new run: the old run's
/// start doesn't carry over, so the endpoint isn't disabled at its first failure back.
#[tokio::test]
#[ignore]
async fn a_failure_after_a_quiet_spell_starts_a_new_run() {
    let s = Scratch::new().await;
    let db = s.database().await;
    let id = s.webhook(db, "https://example.com/hook").await;
    let t0 = Utc::now() - Duration::days(5);
    for h in 0..=24 {
        s.client
            .record_storage_webhook_attempt(id, &attempt("http_error", t0 + Duration::hours(h)))
            .await
            .unwrap();
    }
    let back = t0 + Duration::days(4);
    let since = s
        .client
        .record_storage_webhook_attempt(id, &attempt("http_error", back))
        .await
        .unwrap();
    assert_eq!(
        since.map(|t| t.timestamp_micros()),
        Some(back.timestamp_micros()),
        "a new run"
    );
    let w = s.client.get_storage_webhook(db, id).await.unwrap().unwrap();
    assert_eq!(w.consecutive_failures, 1);
    // Within the window the run goes on.
    let next = s
        .client
        .record_storage_webhook_attempt(id, &attempt("timeout", back + Duration::hours(1)))
        .await
        .unwrap();
    assert_eq!(next, since);
    let w = s.client.get_storage_webhook(db, id).await.unwrap().unwrap();
    assert_eq!(w.consecutive_failures, 2);
    s.finish().await;
}

/// An attempt recorded out of order (older than the last one) never moves `last_attempt_at`
/// back: it stays the newest attempt's time, the base of the next window check.
#[tokio::test]
#[ignore]
async fn an_out_of_order_attempt_never_moves_the_last_attempt_back() {
    let s = Scratch::new().await;
    let db = s.database().await;
    let id = s.webhook(db, "https://example.com/hook").await;
    let newer = Utc::now() - Duration::minutes(1);
    s.client
        .record_storage_webhook_attempt(id, &attempt("http_error", newer))
        .await
        .unwrap();
    s.client
        .record_storage_webhook_attempt(id, &attempt("timeout", newer - Duration::seconds(30)))
        .await
        .unwrap();
    let w = s.client.get_storage_webhook(db, id).await.unwrap().unwrap();
    assert_eq!(
        w.last_attempt_at.map(|t| t.timestamp_micros()),
        Some(newer.timestamp_micros())
    );
    assert_eq!(w.consecutive_failures, 2);
    s.finish().await;
}
