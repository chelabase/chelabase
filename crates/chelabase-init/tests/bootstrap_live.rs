//! The bootstrap's metadata side (`SqlMetadata`) against a real Postgres: a scratch database
//! on the workbench stack's metadata server, dropped at the end. Run with
//! `just workbench test --stack <stack> -p chelabase-init -- --run-ignored only -E 'test(bootstrap_live)'`.

use chelabase_init::bootstrap::SEEDED_TENANT;
use chelabase_init::bootstrap::retry::Check;
use chelabase_init::bootstrap::tenant::{Metadata, SqlMetadata};
use chelabase_metadata::{MetadataClient, MetadataConfig};
use sqlx::AssertSqlSafe;
use sqlx::postgres::PgPoolOptions;

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

async fn org(pool: &sqlx::PgPool) -> Option<String> {
    sqlx::query_scalar("SELECT zitadel_org_id FROM tenants WHERE id = $1")
        .bind(SEEDED_TENANT)
        .fetch_one(pool)
        .await
        .unwrap()
}

// S4: unmigrated waits; then the bind sets, keeps and refuses A -> B naming both; two
// concurrent binds from NULL: exactly one wins and the other is refused or kept.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn bootstrap_live_sql_bind() {
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&url())
        .await
        .expect("metadata Postgres reachable");
    let name = format!("cm_boot_{}", uuid::Uuid::new_v4().simple());
    sqlx::raw_sql(AssertSqlSafe(format!("CREATE DATABASE {name}")))
        .execute(&admin)
        .await
        .unwrap();
    let scratch = name.clone();
    let result = async move {
        let meta = SqlMetadata::new(&url_for(&scratch)).unwrap();
        assert!(
            matches!(meta.migrated().await.unwrap(), Check::Wait(_)),
            "an empty database is not migrated"
        );
        let client = MetadataClient::connect(&MetadataConfig {
            database_url: url_for(&scratch),
        })
        .await
        .unwrap();
        client.migrate().await.unwrap();
        assert_eq!(meta.migrated().await.unwrap(), Check::Ready(()));
        let pool = client.pool();

        assert!(
            meta.bind_org(SEEDED_TENANT, "org-A").await.unwrap(),
            "NULL: bound"
        );
        assert!(
            !meta.bind_org(SEEDED_TENANT, "org-A").await.unwrap(),
            "equal: kept"
        );
        let err = meta
            .bind_org(SEEDED_TENANT, "org-B")
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("org-A") && err.contains("org-B"), "{err}");
        assert_eq!(
            org(pool).await.as_deref(),
            Some("org-A"),
            "a refusal writes nothing"
        );

        // Named once, after the org; a second name is ignored.
        assert!(meta.fill_display_name(SEEDED_TENANT, "Acme").await.unwrap());
        assert!(
            !meta
                .fill_display_name(SEEDED_TENANT, "Other")
                .await
                .unwrap()
        );

        sqlx::query("UPDATE tenants SET zitadel_org_id = NULL WHERE id = $1")
            .bind(SEEDED_TENANT)
            .execute(pool)
            .await
            .unwrap();
        let other = SqlMetadata::new(&url_for(&scratch)).unwrap();
        let (a, b) = tokio::join!(
            meta.bind_org(SEEDED_TENANT, "org-X"),
            other.bind_org(SEEDED_TENANT, "org-Y")
        );
        let wins = [&a, &b].iter().filter(|r| matches!(r, Ok(true))).count();
        assert_eq!(wins, 1, "{a:?} {b:?}");
        let bound = org(pool).await.unwrap();
        let loser = if matches!(a, Ok(true)) { &b } else { &a };
        let msg = loser.as_ref().unwrap_err().to_string();
        assert!(
            msg.contains(&bound),
            "the loser names the winner's org: {msg}"
        );
        pool.close().await;
    };
    let outcome = tokio::spawn(result).await;
    sqlx::raw_sql(AssertSqlSafe(format!(
        "DROP DATABASE IF EXISTS {name} WITH (FORCE)"
    )))
    .execute(&admin)
    .await
    .unwrap();
    outcome.unwrap();
}
