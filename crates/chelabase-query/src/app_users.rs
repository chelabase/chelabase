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
use sqlx::postgres::{PgConnectOptions, PgPoolOptions, PgSslMode};
use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

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
    /// Best effort: dropping the transaction rolls back anyway.
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

fn map_error(e: sqlx::Error) -> UserRowError {
    if let sqlx::Error::Database(db) = &e {
        let code = db.code().map(|c| c.into_owned()).unwrap_or_default();
        let constraint = db.constraint().map(str::to_owned);
        if code == UNIQUE_VIOLATION && constraint.as_deref() == Some(EMAIL_KEY) {
            return UserRowError::EmailTaken;
        }
        if code.starts_with("23") || code.starts_with("22") || code == "42501" {
            return UserRowError::Constraint {
                message: db.message().to_owned(),
                constraint,
            };
        }
    }
    UserRowError::Unavailable(e.to_string())
}

/// Postgres-backed [`UsersDb`], with a small pool per branch endpoint and credentials.
pub struct PgUsersDb {
    pools: moka::future::Cache<(Uuid, [u8; 32]), PgPool>,
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
    async fn pool(&self, t: &SyncTarget) -> PgPool {
        let mut h = Sha256::new();
        h.update(t.password.as_bytes());
        h.update(format!("{}:{}", t.host, t.port).as_bytes());
        h.update(t.database.as_bytes());
        let key = (t.branch_id, <[u8; 32]>::from(h.finalize()));
        self.pools
            .get_with(key, async {
                let options = PgConnectOptions::new()
                    .host(&t.host)
                    .port(t.port)
                    .username(&t.user)
                    .password(&t.password)
                    .database(&t.database)
                    .ssl_mode(PgSslMode::Prefer)
                    .application_name("chelabase-auth");
                PgPoolOptions::new()
                    .max_connections(2)
                    .min_connections(0)
                    .idle_timeout(Duration::from_secs(60))
                    .connect_lazy_with(options)
            })
            .await
    }
}

struct PendingTx(Transaction<'static, Postgres>);

#[async_trait::async_trait]
impl PendingInsert for PendingTx {
    async fn commit(self: Box<Self>) -> Result<(), UserRowError> {
        self.0.commit().await.map_err(map_error)
    }

    async fn rollback(self: Box<Self>) {
        let _ = self.0.rollback().await;
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
        let mut tx = pool
            .begin()
            .await
            .map_err(|e| UserRowError::Unavailable(e.to_string()))?;
        sqlx::query("SET LOCAL statement_timeout = '10s'")
            .execute(&mut *tx)
            .await
            .map_err(map_error)?;

        let rel = format!("{}.{}", quote(&target.schema), quote(&target.table));
        let insertable: Vec<String> = sqlx::query_scalar(INSERTABLE_COLUMNS_SQL)
            .bind(&rel)
            .bind(MANAGED_COLUMNS.as_slice())
            .fetch_all(&mut *tx)
            .await
            .map_err(map_error)?;

        // `Map` iterates in key order (no `preserve_order`), so the first unknown is the
        // alphabetically first.
        let mut keys: Vec<String> = row.data.keys().cloned().collect();
        keys.sort();
        if let Some(bad) = keys.iter().find(|k| !insertable.contains(k)) {
            let _ = tx.rollback().await;
            return Err(UserRowError::UnknownColumn(bad.clone()));
        }

        let sql = insert_sql(&target.schema, &target.table, &keys);
        let q = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(row.id)
            .bind(&row.email)
            .bind(row.email_verified);
        let q = if keys.is_empty() {
            q
        } else {
            q.bind(serde_json::Value::Object(row.data.clone()).to_string())
        };
        if let Err(e) = q.execute(&mut *tx).await {
            let _ = tx.rollback().await;
            return Err(map_error(e));
        }
        Ok(Box::new(PendingTx(tx)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
