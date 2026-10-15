//! `PgUsersDb` against the dev metadata Postgres as a stand-in for a branch (no guard
//! trigger). Run through the live queue:
//! just workbench test -p chelabase-query --test app_users_live -- --run-ignored only
//!
//! Each test works in its own scratch schema, dropped afterwards even when the test fails.
//! The metadata cluster gains a `NOLOGIN` role `authenticated` (made once, kept: harmless,
//! it owns nothing and the metadata database grants it nothing outside the scratch schemas).

use chelabase_query::app_users::{NewUserRow, PgUsersDb, SyncTarget, UserRowError, UsersDb};
use serde_json::{Value, json};
use sqlx::{Connection, PgConnection};
use uuid::Uuid;

/// The metadata database the live recipes use (`CHELABASE_METADATA_DATABASE_URL`, as the justfile
/// exports it), else the shared dev one.
fn url() -> String {
    std::env::var("CHELABASE_METADATA_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://chelabase:chelabase@metadata-db:5432/chelabase_metadata".into())
}

/// The database name in `url()` (its last path segment, without a query string).
fn database_name() -> String {
    let url = url();
    let path = url.rsplit('/').next().unwrap_or_default();
    path.split('?').next().unwrap_or_default().to_string()
}

struct Scratch {
    admin: PgConnection,
    target: SyncTarget,
    db: PgUsersDb,
    /// Set by `finish`; otherwise `Drop` (a failed assertion) drops the schema.
    finished: bool,
}

impl Scratch {
    async fn new() -> Scratch {
        let mut admin = PgConnection::connect(&url())
            .await
            .expect("metadata Postgres (is the workbench stack up?)");
        let run = Uuid::new_v4().simple().to_string();
        let schema = format!("cq_app_users_{}", &run[..12]);
        for sql in [
            // The role the real migration provides; a stand-in here.
            // Tests run in parallel: a concurrent CREATE ROLE loses with unique_violation.
            "DO $$ BEGIN CREATE ROLE authenticated NOLOGIN; EXCEPTION WHEN duplicate_object OR unique_violation THEN NULL; END $$",
            &format!("CREATE SCHEMA {schema}"),
            &format!(
                "CREATE TABLE {schema}.users (
                   id uuid PRIMARY KEY, email text NOT NULL, email_verified boolean NOT NULL,
                   created_at timestamptz NOT NULL, deleted_at timestamptz,
                   nick text, age int CONSTRAINT age_positive CHECK (age > 0), secret_col text)"
            ),
            &format!(
                "CREATE UNIQUE INDEX chelabase_users_email_key ON {schema}.users (lower(email)) WHERE deleted_at IS NULL"
            ),
            &format!("GRANT USAGE ON SCHEMA {schema} TO authenticated"),
            // Sign-up data is opt-in per column (P34): no table-level INSERT.
            &format!("GRANT INSERT (nick, age, secret_col) ON {schema}.users TO authenticated"),
        ] {
            sqlx::query(sqlx::AssertSqlSafe(sql.to_string()))
                .execute(&mut admin)
                .await
                .unwrap_or_else(|e| panic!("{sql}: {e}"));
        }
        // The metadata database as the URL names it (a service name inside the stack).
        let options: sqlx::postgres::PgConnectOptions = url().parse().unwrap();
        let target = SyncTarget {
            branch_id: Uuid::new_v4(),
            host: options.get_host().to_string(),
            port: options.get_port(),
            database: database_name(),
            user: "chelabase".into(),
            password: "chelabase".into(),
            schema,
            table: "users".into(),
            // The metadata database, not a compute: plain.
            server_name: None,
            tls: None,
        };
        Scratch {
            admin,
            target,
            db: PgUsersDb::new(),
            finished: false,
        }
    }

    async fn count(&mut self) -> i64 {
        sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT count(*) FROM {}.users",
            self.target.schema
        )))
        .fetch_one(&mut self.admin)
        .await
        .unwrap()
    }

    async fn finish(mut self) {
        let _ = sqlx::query(sqlx::AssertSqlSafe(format!(
            "DROP SCHEMA {} CASCADE",
            self.target.schema
        )))
        .execute(&mut self.admin)
        .await;
        self.finished = true;
    }
}

impl Drop for Scratch {
    /// A test that panicked before `finish`: the schema is dropped on a runtime of its own
    /// (the test's runtime is unwinding).
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        let schema = self.target.schema.clone();
        let _ = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .ok()?;
            rt.block_on(async {
                let mut admin = PgConnection::connect(&url()).await.ok()?;
                // An insert transaction the failed test left open would block the drop
                // forever (this thread holds up the test's runtime): end its session first.
                let _ = sqlx::query(
                    "SELECT pg_catalog.pg_terminate_backend(l.pid) FROM pg_catalog.pg_locks l
                     JOIN pg_catalog.pg_class c ON c.oid = l.relation
                     JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
                     WHERE n.nspname = $1 AND l.pid <> pg_catalog.pg_backend_pid()",
                )
                .bind(&schema)
                .execute(&mut admin)
                .await;
                sqlx::query(sqlx::AssertSqlSafe(format!(
                    "DROP SCHEMA IF EXISTS {schema} CASCADE"
                )))
                .execute(&mut admin)
                .await
                .ok()
            })
        })
        .join();
    }
}

fn row(email: &str, data: Value) -> NewUserRow {
    let Value::Object(data): Value = data else {
        panic!()
    };
    NewUserRow {
        id: Uuid::new_v4(),
        email: email.into(),
        email_verified: false,
        data,
    }
}

#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn unknown_key_rolls_back() {
    let mut s = Scratch::new().await;
    for key in ["nope", "email"] {
        let r =
            s.db.begin_insert(&s.target, &row("a@x.io", json!({ key: "v" })))
                .await;
        assert_eq!(r.err(), Some(UserRowError::UnknownColumn(key.into())));
    }
    assert_eq!(s.count().await, 0);
    s.finish().await;
}

#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn check_violation_is_constraint_with_name() {
    let mut s = Scratch::new().await;
    let r =
        s.db.begin_insert(&s.target, &row("a@x.io", json!({"age": -1})))
            .await;
    match r.err() {
        Some(UserRowError::Constraint { constraint, .. }) => {
            assert_eq!(constraint.as_deref(), Some("age_positive"))
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(s.count().await, 0);
    s.finish().await;
}

#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn duplicate_email_is_email_taken() {
    let mut s = Scratch::new().await;
    s.db.begin_insert(&s.target, &row("a@x.io", json!({})))
        .await
        .unwrap()
        .commit()
        .await
        .unwrap();
    let r =
        s.db.begin_insert(&s.target, &row("A@x.io", json!({"nick": "n"})))
            .await;
    assert_eq!(r.err(), Some(UserRowError::EmailTaken));
    assert_eq!(s.count().await, 1);
    s.finish().await;
}

#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn commit_then_visible() {
    let mut s = Scratch::new().await;
    let p =
        s.db.begin_insert(&s.target, &row("a@x.io", json!({"nick": "ann", "age": 7})))
            .await
            .unwrap();
    assert_eq!(s.count().await, 0, "not visible before commit");
    p.commit().await.unwrap();
    assert_eq!(s.count().await, 1);
    let nick: String = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT nick FROM {}.users",
        s.target.schema
    )))
    .fetch_one(&mut s.admin)
    .await
    .unwrap();
    assert_eq!(nick, "ann");
    s.finish().await;
}

#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn rollback_leaves_nothing() {
    let mut s = Scratch::new().await;
    let p =
        s.db.begin_insert(&s.target, &row("a@x.io", json!({"nick": "ann"})))
            .await
            .unwrap();
    p.rollback().await;
    assert_eq!(s.count().await, 0);
    s.finish().await;
}

#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn column_without_authenticated_insert_is_unknown() {
    let mut s = Scratch::new().await;
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "REVOKE INSERT (secret_col) ON {}.users FROM authenticated",
        s.target.schema
    )))
    .execute(&mut s.admin)
    .await
    .unwrap();
    let r =
        s.db.begin_insert(&s.target, &row("a@x.io", json!({"secret_col": "x"})))
            .await;
    assert_eq!(
        r.err(),
        Some(UserRowError::UnknownColumn("secret_col".into()))
    );
    assert_eq!(s.count().await, 0);
    s.finish().await;
}
