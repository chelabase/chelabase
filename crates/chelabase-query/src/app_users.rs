//! Mirroring an app user into the branch's app users table: one insert in a transaction the
//! caller commits or rolls back, run as the `chelabase_auth_sync` role (which holds
//! `INSERT` on the table and nothing else).
//!
//! The user's `data` keys name the table's own columns. Only columns the `authenticated`
//! role could insert into are accepted, so a user can't be given a column their own
//! session couldn't write. Names in SQL are always quoted; values are bind parameters.

use std::fmt;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use deadpool_postgres::{Manager, ManagerConfig, Object, Pool, RecyclingMethod, Runtime};
use tokio_postgres::{NoTls, TransactionStatus};
use tokio_postgres::types::Type;
use uuid::Uuid;

use crate::error::DbFields;

/// The role the sync connects as.
pub const SYNC_ROLE: &str = "chelabase_auth_sync";

/// Columns Chelabase owns: `data` can't name them.
pub const MANAGED_COLUMNS: [&str; 5] = ["id", "email", "email_verified", "created_at", "deleted_at"];

/// The `SQLSTATE` unique-violation code and the email index's name (the branch's migration).
const UNIQUE_VIOLATION: &str = "23505";
const EMAIL_KEY: &str = "chelabase_users_email_key";

/// Where and how to write: the branch's endpoint, the sync role's credentials and the table.
#[derive(Clone, Serialize, Deserialize, PartialEq)]
pub struct SyncTarget {
    pub branch_id: Uuid,
    pub host: String,
    pub port: u16,
    pub database: String,
    pub user: String,
    pub password: String,
    pub schema: String,
    pub table: String,
}

impl fmt::Debug for SyncTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SyncTarget")
            .field("branch_id", &self.branch_id)
            .field("host", &self.host)
            .field("port", &self.port)
            .field("database", &self.database)
            .field("user", &self.user)
            .field("password", &"<hidden>")
            .field("schema", &self.schema)
            .field("table", &self.table)
            .finish()
    }
}

/// The row to insert: the managed columns, and `data` for the table's own.
#[derive(Clone, Debug, PartialEq)]
pub struct NewUserRow {
    pub id: Uuid,
    pub email: String,
    pub email_verified: bool,
    pub data: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, PartialEq)]
pub enum UserRowError {
    /// A `data` key that isn't an insertable column (unknown, managed, or one
    /// `authenticated` can't insert into).
    UnknownColumn(String),
    /// A check, not-null, type or privilege failure of the table, named when it has a name.
    Constraint {
        message: String,
        constraint: Option<String>,
    },
    EmailTaken,
    Unavailable(String),
}

impl fmt::Display for UserRowError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            UserRowError::UnknownColumn(c) => write!(f, "unknown column {c}"),
            UserRowError::Constraint { message, .. } => f.write_str(message),
            UserRowError::EmailTaken => f.write_str("email already taken"),
            UserRowError::Unavailable(m) => write!(f, "database unavailable: {m}"),
        }
    }
}

impl std::error::Error for UserRowError {}

#[async_trait::async_trait]
pub trait UsersDb: Send + Sync {
    /// Inserts `row` in a new transaction and returns it uncommitted.
    async fn begin_insert(
        &self,
        target: &SyncTarget,
        row: &NewUserRow,
    ) -> Result<Box<dyn PendingInsert>, UserRowError>;
}

#[async_trait::async_trait]
pub trait PendingInsert: Send {
    async fn commit(self: Box<Self>) -> Result<(), UserRowError>;
    /// Best effort: when the `ROLLBACK` fails, the connection is closed instead (which ends
    /// the transaction).
    async fn rollback(self: Box<Self>);
}

/// The insertable columns of the table (`$1`, a quoted `schema.table`), minus `$2` (the
/// managed ones): not generated, not `GENERATED ALWAYS`, and `authenticated` may insert.
pub const INSERTABLE_COLUMNS_SQL: &str = "SELECT a.attname::text FROM pg_catalog.pg_attribute a
 WHERE a.attrelid OPERATOR(pg_catalog.=) $1::pg_catalog.regclass AND a.attnum OPERATOR(pg_catalog.>) 0
   AND NOT a.attisdropped AND a.attgenerated OPERATOR(pg_catalog.=) '' AND a.attidentity OPERATOR(pg_catalog.<>) 'a'
   AND a.attname OPERATOR(pg_catalog.<>) ALL ($2::pg_catalog.name[])
   AND pg_catalog.has_column_privilege('authenticated', a.attrelid, a.attname::pg_catalog.text, 'INSERT')";

fn quote(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

/// The insert: `$1` id, `$2` email, `$3` email_verified, `$4` the `data` object as JSON
/// (read with `json_populate_record`, so each column gets its own type's input rules).
pub fn insert_sql(schema: &str, table: &str, data_columns: &[String]) -> String {
    let rel = format!("{}.{}", quote(schema), quote(table));
    if data_columns.is_empty() {
        return format!(
            "INSERT INTO {rel} (id, email, email_verified, created_at) \
             VALUES ($1, $2, $3, pg_catalog.now())"
        );
    }
    let cols: Vec<String> = data_columns.iter().map(|c| quote(c)).collect();
    let from_r: Vec<String> = cols.iter().map(|c| format!("r.{c}")).collect();
    format!(
        "INSERT INTO {rel} (id, email, email_verified, created_at, {}) \
         SELECT $1, $2, $3, pg_catalog.now(), {} \
         FROM pg_catalog.json_populate_record(NULL::{rel}, $4::json) r",
        cols.join(", "),
        from_r.join(", ")
    )
}

/// The row error a Postgres error maps to, or `None` when it isn't one the caller can act on.
fn map_db(db: &dyn DbFields) -> Option<UserRowError> {
    let code = db.code();
    let constraint = db.constraint().map(str::to_owned);
    if code == UNIQUE_VIOLATION && constraint.as_deref() == Some(EMAIL_KEY) {
        return Some(UserRowError::EmailTaken);
    }
    if code.starts_with("23") || code.starts_with("22") || code == "42501" {
        return Some(UserRowError::Constraint {
            message: db.message().to_owned(),
            constraint,
        });
    }
    None
}

fn map_error(e: &tokio_postgres::Error) -> UserRowError {
    e.as_db_error()
        .and_then(|db| map_db(db))
        .unwrap_or_else(|| UserRowError::Unavailable(e.to_string()))
}

/// Longest wait for a connection (waiting for a free one, or connecting).
const ACQUIRE_TIMEOUT: Duration = Duration::from_secs(30);

/// Postgres-backed [`UsersDb`], with a small pool per branch endpoint and credentials. An
/// idle pool is dropped by the cache's time-to-idle (two connections don't need a reaper).
pub struct PgUsersDb {
    pools: moka::future::Cache<(Uuid, [u8; 32]), Pool>,
}

impl Default for PgUsersDb {
    fn default() -> Self {
        Self::new()
    }
}

impl PgUsersDb {
    pub fn new() -> Self {
        PgUsersDb {
            pools: moka::future::Cache::builder()
                .max_capacity(256)
                .time_to_idle(Duration::from_secs(600))
                .build(),
        }
    }

    /// The branch's pool; a changed password, address or database makes a new one.
    async fn pool(&self, t: &SyncTarget) -> Pool {
        let mut h = Sha256::new();
        h.update(t.password.as_bytes());
        h.update(format!("{}:{}", t.host, t.port).as_bytes());
        h.update(t.database.as_bytes());
        let key = (t.branch_id, <[u8; 32]>::from(h.finalize()));
        self.pools
            .get_with(key, async {
                let mut config = tokio_postgres::Config::new();
                config
                    .host(&t.host)
                    .port(t.port)
                    .user(&t.user)
                    .password(&t.password)
                    .dbname(&t.database)
                    .application_name("chelabase-auth");
                let manager = Manager::from_config(
                    config,
                    NoTls,
                    ManagerConfig {
                        recycling_method: RecyclingMethod::Fast,
                    },
                );
                Pool::builder(manager)
                    .max_size(2)
                    .runtime(Runtime::Tokio1)
                    .wait_timeout(Some(ACQUIRE_TIMEOUT))
                    .create_timeout(Some(ACQUIRE_TIMEOUT))
                    .build()
                    .expect("a pool with a runtime set always builds")
            })
            .await
    }
}

/// A pooled connection inside an open transaction. It goes back to the pool only once its
/// `COMMIT` or `ROLLBACK` succeeded; dropped before that (a cancelled sync, an error, a
/// panic) it's taken out of the pool and closed, which ends the transaction (and frees the
/// email's unique-index lock) at once rather than when the connection is next used.
struct PendingTx {
    object: Option<Object>,
    finished: bool,
}

impl PendingTx {
    fn new(object: Object) -> Self {
        PendingTx {
            object: Some(object),
            finished: false,
        }
    }

    fn conn(&self) -> &Object {
        self.object
            .as_ref()
            .expect("a pending insert holds its connection until dropped")
    }

    /// Ends the transaction with `sql`; the connection is kept only when that worked.
    async fn end(&mut self, sql: &str) -> Result<(), tokio_postgres::Error> {
        self.conn().batch_execute(sql).await?;
        self.finished = true;
        Ok(())
    }
}

impl Drop for PendingTx {
    fn drop(&mut self) {
        if let Some(object) = self.object.take()
            && !self.finished
        {
            drop(Object::take(object));
        }
    }
}

#[async_trait::async_trait]
impl PendingInsert for PendingTx {
    async fn commit(mut self: Box<Self>) -> Result<(), UserRowError> {
        self.end("COMMIT").await.map_err(|e| map_error(&e))
    }

    async fn rollback(mut self: Box<Self>) {
        let _ = self.end("ROLLBACK").await;
    }
}

#[async_trait::async_trait]
impl UsersDb for PgUsersDb {
    async fn begin_insert(
        &self,
        target: &SyncTarget,
        row: &NewUserRow,
    ) -> Result<Box<dyn PendingInsert>, UserRowError> {
        let pool = self.pool(target).await;
        let conn = pool
            .get()
            .await
            .map_err(|e| UserRowError::Unavailable(e.to_string()))?;
        // Held from here: a failure or a cancel before the end discards the connection.
        let tx = PendingTx::new(conn);
        tx.conn()
            .batch_execute(&begin_sql(tx.conn().transaction_status()))
            .await
            .map_err(|e| map_error(&e))?;

        let rel = format!("{}.{}", quote(&target.schema), quote(&target.table));
        let insertable = match insertable_columns(tx.conn(), &rel).await {
            Ok(columns) => columns,
            Err(e) => {
                Box::new(tx).rollback().await;
                return Err(map_error(&e));
            }
        };

        // `Map` iterates in key order (no `preserve_order`), so the first unknown is the
        // alphabetically first.
        let mut keys: Vec<String> = row.data.keys().cloned().collect();
        keys.sort();
        if let Some(bad) = keys.iter().find(|k| !insertable.contains(k)) {
            Box::new(tx).rollback().await;
            return Err(UserRowError::UnknownColumn(bad.clone()));
        }

        if let Err(e) = insert_row(tx.conn(), target, &keys, row).await {
            Box::new(tx).rollback().await;
            return Err(map_error(&e));
        }
        Ok(Box::new(tx))
    }
}

/// Opens the transaction and caps its statements. The leading `ROLLBACK` ends whatever a
/// connection dropped mid-transaction left open; it's sent unless the connection is idle
/// (Postgres logs a warning when there is nothing to end).
fn begin_sql(status: TransactionStatus) -> String {
    let begin = "BEGIN; SET LOCAL statement_timeout = '10s'";
    if status == TransactionStatus::Idle {
        begin.to_owned()
    } else {
        format!("ROLLBACK; {begin}")
    }
}

/// The columns of `rel` (a quoted `schema.table`) that app user data may name.
async fn insertable_columns(
    conn: &Object,
    rel: &str,
) -> Result<Vec<String>, tokio_postgres::Error> {
    // `$1` is text (cast to `regclass` in the SQL), `$2` is `text[]` (cast to `name[]`).
    let stmt = conn
        .prepare_typed_cached(INSERTABLE_COLUMNS_SQL, &[Type::TEXT, Type::TEXT_ARRAY])
        .await?;
    let managed: &[&str] = &MANAGED_COLUMNS;
    let rows = conn.query(&stmt, &[&rel, &managed]).await?;
    rows.iter().map(|r| r.try_get(0)).collect()
}

/// The insert of `row`, with `keys` (its sorted data keys) as the table's own columns.
async fn insert_row(
    conn: &Object,
    target: &SyncTarget,
    keys: &[String],
    row: &NewUserRow,
) -> Result<(), tokio_postgres::Error> {
    let sql = insert_sql(&target.schema, &target.table, keys);
    if keys.is_empty() {
        let stmt = conn
            .prepare_typed(&sql, &[Type::UUID, Type::TEXT, Type::BOOL])
            .await?;
        conn.execute(&stmt, &[&row.id, &row.email, &row.email_verified])
            .await?;
    } else {
        let stmt = conn
            .prepare_typed(&sql, &[Type::UUID, Type::TEXT, Type::BOOL, Type::TEXT])
            .await?;
        let json = serde_json::Value::Object(row.data.clone()).to_string();
        conn.execute(&stmt, &[&row.id, &row.email, &row.email_verified, &json])
            .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fake {
        code: &'static str,
        constraint: Option<&'static str>,
    }

    impl DbFields for Fake {
        fn code(&self) -> &str {
            self.code
        }
        fn message(&self) -> &str {
            "boom"
        }
        fn detail(&self) -> Option<&str> {
            None
        }
        fn hint(&self) -> Option<&str> {
            None
        }
        fn constraint(&self) -> Option<&str> {
            self.constraint
        }
    }

    #[test]
    fn the_app_users_begin_rolls_back_only_when_not_idle() {
        use tokio_postgres::TransactionStatus;
        assert_eq!(
            begin_sql(TransactionStatus::Idle),
            "BEGIN; SET LOCAL statement_timeout = '10s'"
        );
        for status in [
            TransactionStatus::InTransaction,
            TransactionStatus::Failed,
            TransactionStatus::Unknown,
        ] {
            assert_eq!(
                begin_sql(status),
                "ROLLBACK; BEGIN; SET LOCAL statement_timeout = '10s'",
                "{status:?}"
            );
        }
    }

    #[test]
    fn the_email_index_violation_is_email_taken() {
        let f = Fake { code: "23505", constraint: Some("chelabase_users_email_key") };
        assert_eq!(map_db(&f), Some(UserRowError::EmailTaken));
        // Another unique index is a plain constraint failure.
        let f = Fake { code: "23505", constraint: Some("users_nick_key") };
        assert_eq!(
            map_db(&f),
            Some(UserRowError::Constraint {
                message: "boom".into(),
                constraint: Some("users_nick_key".into())
            })
        );
    }

    #[test]
    fn data_errors_and_privilege_errors_are_constraints_and_the_rest_is_unmapped() {
        for code in ["22P02", "23502", "23514", "42501"] {
            let f = Fake { code, constraint: None };
            assert_eq!(
                map_db(&f),
                Some(UserRowError::Constraint { message: "boom".into(), constraint: None }),
                "{code}"
            );
        }
        for code in ["57014", "08006", "42P01"] {
            assert_eq!(map_db(&Fake { code, constraint: None }), None, "{code}");
        }
    }

    #[test]
    fn insert_sql_lists_columns_in_order() {
        let sql = insert_sql("s", "t", &["c1".into(), "we\"ird".into()]);
        assert_eq!(
            sql,
            "INSERT INTO \"s\".\"t\" (id, email, email_verified, created_at, \"c1\", \"we\"\"ird\") \
             SELECT $1, $2, $3, pg_catalog.now(), r.\"c1\", r.\"we\"\"ird\" \
             FROM pg_catalog.json_populate_record(NULL::\"s\".\"t\", $4::json) r"
        );
    }

    #[test]
    fn insert_sql_without_data_uses_values() {
        assert_eq!(
            insert_sql("s", "t", &[]),
            "INSERT INTO \"s\".\"t\" (id, email, email_verified, created_at) \
             VALUES ($1, $2, $3, pg_catalog.now())"
        );
    }

    /// A pending insert goes back to the pool only once committed or rolled back; dropped
    /// before that (a cancelled sync), its connection is taken out of the pool and closed,
    /// ending the transaction and its lock on the email.
    #[tokio::test]
    async fn a_pending_insert_dropped_unfinished_is_discarded() {
        let pool = crate::fake_pg::pool(2).await;
        drop(PendingTx::new(pool.get().await.unwrap()));
        assert_eq!(pool.status().size, 0, "dropped: taken");

        let tx: Box<dyn PendingInsert> = Box::new(PendingTx::new(pool.get().await.unwrap()));
        tx.commit().await.unwrap();
        assert_eq!(pool.status().size, 1, "committed: kept");
        let tx: Box<dyn PendingInsert> = Box::new(PendingTx::new(pool.get().await.unwrap()));
        tx.rollback().await;
        assert_eq!(pool.status().size, 1, "rolled back: kept");

        // The commit's future dropped before it ran (the caller cancelled).
        let tx: Box<dyn PendingInsert> = Box::new(PendingTx::new(pool.get().await.unwrap()));
        drop(tx.commit());
        assert_eq!(pool.status().size, 0, "cancelled commit: taken");
    }

    #[test]
    fn sync_target_debug_hides_the_password() {
        let t = SyncTarget {
            branch_id: uuid::Uuid::nil(),
            host: "h".into(),
            port: 5432,
            database: "d".into(),
            user: "u".into(),
            password: "hunter2".into(),
            schema: "s".into(),
            table: "t".into(),
        };
        let d = format!("{t:?}");
        assert!(!d.contains("hunter2"), "{d}");
        assert!(d.contains("host") && d.contains("\"h\""), "{d}");
    }
}
