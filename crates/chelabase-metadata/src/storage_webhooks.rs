//! Object storage m18: a database's webhook endpoints for object events, and their recent
//! delivery attempts. The sealed secret is read only by the worker ([`DeliveryTarget`]); no
//! API type carries it.

use chrono::{DateTime, TimeDelta, Utc};
use sqlx::Row;
use sqlx::postgres::PgRow;
use uuid::Uuid;

use crate::{MetadataClient, MetadataError};

/// Endpoints one database may have.
pub const STORAGE_WEBHOOK_CAP: i64 = 20;
/// Attempts kept per endpoint (the newest).
pub const STORAGE_WEBHOOK_ATTEMPTS_KEPT: i64 = 100;
/// How long the worker retries an event before dropping it. While an endpoint keeps failing
/// some event is always retrying, so attempts are never further apart than this: a longer
/// quiet spell ends a run of failures.
pub const STORAGE_WEBHOOK_RETRY_WINDOW: TimeDelta = TimeDelta::hours(24);

/// An endpoint's run of failed attempts: when it started (`None`: not failing) and how many.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FailureRun {
    pub failing_since: Option<DateTime<Utc>>,
    pub consecutive_failures: i32,
}

impl FailureRun {
    /// The run after an attempt at `at` (`delivered` or not), given this run and the time of
    /// the endpoint's last attempt.
    pub fn after(
        self,
        last_attempt_at: Option<DateTime<Utc>>,
        delivered: bool,
        at: DateTime<Utc>,
    ) -> FailureRun {
        if delivered {
            return FailureRun {
                failing_since: None,
                consecutive_failures: 0,
            };
        }
        if last_attempt_at.is_none_or(|last| at - last > STORAGE_WEBHOOK_RETRY_WINDOW) {
            return FailureRun {
                failing_since: Some(at),
                consecutive_failures: 1,
            };
        }
        FailureRun {
            failing_since: Some(self.failing_since.unwrap_or(at)),
            consecutive_failures: self.consecutive_failures.saturating_add(1),
        }
    }
}

macro_rules! columns {
    () => {
        "id, database_id, url, events, bucket, prefix, enabled, enabled_since, disabled_reason,
         failing_since, consecutive_failures, last_attempt_at, created_at, updated_at"
    };
}

/// An endpoint, as the API shows it (never its secret).
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct StorageWebhook {
    pub id: Uuid,
    pub database_id: Uuid,
    pub url: String,
    /// `object.created`, `object.updated`, `object.deleted`, `object.moved`.
    pub events: Vec<String>,
    /// Only objects of this bucket (by name); `null`: every bucket.
    pub bucket: Option<String>,
    /// Only object names starting with this; `null`: every name.
    pub prefix: Option<String>,
    pub enabled: bool,
    /// When delivery last (re)started: events from then on are delivered.
    pub enabled_since: DateTime<Utc>,
    /// Why it is disabled: `member` (through the API) or `failing` (three days of failed
    /// attempts); `null` while enabled.
    pub disabled_reason: Option<String>,
    /// The start of the current run of failed attempts; `null` after a success.
    pub failing_since: Option<DateTime<Utc>>,
    pub consecutive_failures: i32,
    pub last_attempt_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// A new endpoint; `secret_enc` is its sealed signing secret.
#[derive(Debug, Clone)]
pub struct NewStorageWebhook<'a> {
    pub id: Uuid,
    pub database_id: Uuid,
    pub url: &'a str,
    pub events: &'a [String],
    pub bucket: Option<&'a str>,
    pub prefix: Option<&'a str>,
    pub secret_enc: &'a str,
}

/// Fields to change: `None` leaves one, `Some(None)` clears a filter. Enabling again restarts
/// delivery from now and clears the failure state.
#[derive(Debug, Clone, Default)]
pub struct StorageWebhookPatch {
    pub url: Option<String>,
    pub events: Option<Vec<String>>,
    pub bucket: Option<Option<String>>,
    pub prefix: Option<Option<String>>,
    pub enabled: Option<bool>,
}

/// An endpoint with its sealed secret: what the worker delivers to.
#[derive(Debug, Clone, PartialEq)]
pub struct DeliveryTarget {
    pub webhook: StorageWebhook,
    pub secret_enc: String,
}

/// One delivery attempt, as recorded.
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct StorageWebhookAttempt {
    pub event_id: Uuid,
    pub event_type: String,
    /// 1 for the first delivery of the event, then one more per retry.
    pub attempt: i32,
    /// `delivered`, `http_error`, `timeout`, `connect_error` or `refused`.
    pub outcome: String,
    /// The HTTP status, when there was an answer.
    pub status: Option<i32>,
    pub duration_ms: i32,
    pub attempted_at: DateTime<Utc>,
}

fn webhook_from_row(r: &PgRow) -> StorageWebhook {
    StorageWebhook {
        id: r.get("id"),
        database_id: r.get("database_id"),
        url: r.get("url"),
        events: r.get("events"),
        bucket: r.get("bucket"),
        prefix: r.get("prefix"),
        enabled: r.get("enabled"),
        enabled_since: r.get("enabled_since"),
        disabled_reason: r.get("disabled_reason"),
        failing_since: r.get("failing_since"),
        consecutive_failures: r.get("consecutive_failures"),
        last_attempt_at: r.get("last_attempt_at"),
        created_at: r.get("created_at"),
        updated_at: r.get("updated_at"),
    }
}

fn target_from_row(r: &PgRow) -> DeliveryTarget {
    DeliveryTarget {
        webhook: webhook_from_row(r),
        secret_enc: r.get("secret_enc"),
    }
}

impl MetadataClient {
    /// Creates an endpoint, or `None` when the database already has [`STORAGE_WEBHOOK_CAP`]
    /// (counted under a lock on the database row, so two creates can't both pass it).
    pub async fn create_storage_webhook(
        &self,
        new: &NewStorageWebhook<'_>,
    ) -> Result<Option<StorageWebhook>, MetadataError> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("SELECT 1 FROM databases WHERE id = $1 FOR UPDATE")
            .bind(new.database_id)
            .fetch_optional(&mut *tx)
            .await?;
        let count: i64 =
            sqlx::query_scalar("SELECT count(*) FROM storage_webhooks WHERE database_id = $1")
                .bind(new.database_id)
                .fetch_one(&mut *tx)
                .await?;
        if count >= STORAGE_WEBHOOK_CAP {
            return Ok(None);
        }
        let row = sqlx::query(concat!(
            "INSERT INTO storage_webhooks (id, database_id, url, events, bucket, prefix, secret_enc)
             VALUES ($1, $2, $3, $4, $5, $6, $7) RETURNING ",
            columns!()
        ))
        .bind(new.id)
        .bind(new.database_id)
        .bind(new.url)
        .bind(new.events)
        .bind(new.bucket)
        .bind(new.prefix)
        .bind(new.secret_enc)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(Some(webhook_from_row(&row)))
    }

    /// The database's endpoints, oldest first.
    pub async fn list_storage_webhooks(
        &self,
        database: Uuid,
    ) -> Result<Vec<StorageWebhook>, MetadataError> {
        let rows = sqlx::query(concat!(
            "SELECT ",
            columns!(),
            " FROM storage_webhooks WHERE database_id = $1 ORDER BY created_at, id"
        ))
        .bind(database)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(webhook_from_row).collect())
    }

    /// One endpoint of the database.
    pub async fn get_storage_webhook(
        &self,
        database: Uuid,
        id: Uuid,
    ) -> Result<Option<StorageWebhook>, MetadataError> {
        let row = sqlx::query(concat!(
            "SELECT ",
            columns!(),
            " FROM storage_webhooks WHERE database_id = $1 AND id = $2"
        ))
        .bind(database)
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.as_ref().map(webhook_from_row))
    }

    /// Applies `patch` to one endpoint of the database; `None` when there is none.
    pub async fn update_storage_webhook(
        &self,
        database: Uuid,
        id: Uuid,
        patch: &StorageWebhookPatch,
    ) -> Result<Option<StorageWebhook>, MetadataError> {
        let row = sqlx::query(concat!(
            "UPDATE storage_webhooks SET
                url = COALESCE($3, url),
                events = COALESCE($4, events),
                bucket = CASE WHEN $5 THEN $6 ELSE bucket END,
                prefix = CASE WHEN $7 THEN $8 ELSE prefix END,
                enabled_since = CASE WHEN $9 AND NOT enabled THEN now() ELSE enabled_since END,
                failing_since = CASE WHEN $9 AND NOT enabled THEN NULL ELSE failing_since END,
                consecutive_failures = CASE WHEN $9 AND NOT enabled THEN 0 ELSE consecutive_failures END,
                disabled_reason = CASE WHEN $9 IS NULL THEN disabled_reason
                                       WHEN $9 THEN NULL
                                       WHEN enabled THEN 'member'
                                       ELSE disabled_reason END,
                enabled = COALESCE($9, enabled),
                updated_at = now()
             WHERE database_id = $1 AND id = $2 RETURNING ",
            columns!()
        ))
        .bind(database)
        .bind(id)
        .bind(patch.url.as_deref())
        .bind(patch.events.as_deref())
        .bind(patch.bucket.is_some())
        .bind(patch.bucket.clone().flatten())
        .bind(patch.prefix.is_some())
        .bind(patch.prefix.clone().flatten())
        .bind(patch.enabled)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.as_ref().map(webhook_from_row))
    }

    /// Deletes one endpoint of the database (its attempts go with it).
    pub async fn delete_storage_webhook(
        &self,
        database: Uuid,
        id: Uuid,
    ) -> Result<bool, MetadataError> {
        let done = sqlx::query("DELETE FROM storage_webhooks WHERE database_id = $1 AND id = $2")
            .bind(database)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(done.rows_affected() == 1)
    }

    /// Replaces one endpoint's sealed secret.
    pub async fn rotate_storage_webhook_secret(
        &self,
        database: Uuid,
        id: Uuid,
        secret_enc: &str,
    ) -> Result<Option<StorageWebhook>, MetadataError> {
        let row = sqlx::query(concat!(
            "UPDATE storage_webhooks SET secret_enc = $3, updated_at = now()
             WHERE database_id = $1 AND id = $2 RETURNING ",
            columns!()
        ))
        .bind(database)
        .bind(id)
        .bind(secret_enc)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.as_ref().map(webhook_from_row))
    }

    /// Every enabled endpoint of a database that isn't being deleted: what the worker delivers
    /// to.
    pub async fn storage_webhook_targets(&self) -> Result<Vec<DeliveryTarget>, MetadataError> {
        let rows = sqlx::query(concat!(
            "SELECT w.secret_enc, ",
            "w.id, w.database_id, w.url, w.events, w.bucket, w.prefix, w.enabled, w.enabled_since,
             w.disabled_reason, w.failing_since, w.consecutive_failures, w.last_attempt_at,
             w.created_at, w.updated_at
             FROM storage_webhooks w JOIN databases d ON d.id = w.database_id
             WHERE w.enabled AND d.status NOT IN ('deleting', 'deleted')
             ORDER BY w.id"
        ))
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(target_from_row).collect())
    }

    /// One endpoint of the database with its sealed secret, enabled or not (a test event).
    pub async fn storage_webhook_target(
        &self,
        database: Uuid,
        id: Uuid,
    ) -> Result<Option<DeliveryTarget>, MetadataError> {
        let row = sqlx::query(concat!(
            "SELECT secret_enc, ",
            columns!(),
            " FROM storage_webhooks WHERE database_id = $1 AND id = $2"
        ))
        .bind(database)
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.as_ref().map(target_from_row))
    }

    /// Records an attempt and keeps only the newest [`STORAGE_WEBHOOK_ATTEMPTS_KEPT`]. `delivered`
    /// ends a run of failures; any other outcome starts one (`failing_since` set once) or
    /// continues it, unless the last attempt is older than the retry window: then it starts a
    /// new one ([`FailureRun::after`]). Returns the endpoint's `failing_since` after the attempt
    /// (`None`: the endpoint is gone, or not failing).
    pub async fn record_storage_webhook_attempt(
        &self,
        webhook: Uuid,
        attempt: &StorageWebhookAttempt,
    ) -> Result<Option<DateTime<Utc>>, MetadataError> {
        let delivered = attempt.outcome == "delivered";
        let mut tx = self.pool.begin().await?;
        let Some(row) = sqlx::query(
            "SELECT failing_since, consecutive_failures, last_attempt_at
             FROM storage_webhooks WHERE id = $1 FOR UPDATE",
        )
        .bind(webhook)
        .fetch_optional(&mut *tx)
        .await?
        else {
            return Ok(None);
        };
        let run = FailureRun {
            failing_since: row.get("failing_since"),
            consecutive_failures: row.get("consecutive_failures"),
        }
        .after(row.get("last_attempt_at"), delivered, attempt.attempted_at);
        // As stored (microseconds): the value a later disable compares with. An attempt older
        // than the last one (recorded out of order) never moves `last_attempt_at` back.
        let failing_since: Option<DateTime<Utc>> = sqlx::query_scalar(
            "UPDATE storage_webhooks
             SET failing_since = $2, consecutive_failures = $3,
                 last_attempt_at = GREATEST(last_attempt_at, $4)
             WHERE id = $1 RETURNING failing_since",
        )
        .bind(webhook)
        .bind(run.failing_since)
        .bind(run.consecutive_failures)
        .bind(attempt.attempted_at)
        .fetch_one(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO storage_webhook_attempts
                (webhook_id, event_id, event_type, attempt, outcome, status, duration_ms, attempted_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind(webhook)
        .bind(attempt.event_id)
        .bind(&attempt.event_type)
        .bind(attempt.attempt)
        .bind(&attempt.outcome)
        .bind(attempt.status)
        .bind(attempt.duration_ms)
        .bind(attempt.attempted_at)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "DELETE FROM storage_webhook_attempts WHERE webhook_id = $1 AND id < (
                SELECT id FROM storage_webhook_attempts WHERE webhook_id = $1
                ORDER BY id DESC OFFSET $2 - 1 LIMIT 1)",
        )
        .bind(webhook)
        .bind(STORAGE_WEBHOOK_ATTEMPTS_KEPT)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(failing_since)
    }

    /// Disables an endpoint that has been failing since `failing_since` (and still is: a
    /// success in between, or a member's change, leaves it). True when it was disabled.
    pub async fn disable_failing_storage_webhook(
        &self,
        webhook: Uuid,
        failing_since: DateTime<Utc>,
    ) -> Result<bool, MetadataError> {
        let done = sqlx::query(
            "UPDATE storage_webhooks SET enabled = false, disabled_reason = 'failing', updated_at = now()
             WHERE id = $1 AND enabled AND failing_since = $2",
        )
        .bind(webhook)
        .bind(failing_since)
        .execute(&self.pool)
        .await?;
        Ok(done.rows_affected() == 1)
    }

    /// One endpoint's recent attempts, newest first (at most `limit`).
    pub async fn list_storage_webhook_attempts(
        &self,
        database: Uuid,
        id: Uuid,
        limit: i64,
    ) -> Result<Vec<StorageWebhookAttempt>, MetadataError> {
        let rows = sqlx::query(
            "SELECT a.event_id, a.event_type, a.attempt, a.outcome, a.status, a.duration_ms, a.attempted_at
             FROM storage_webhook_attempts a JOIN storage_webhooks w ON w.id = a.webhook_id
             WHERE w.database_id = $1 AND w.id = $2 ORDER BY a.id DESC LIMIT $3",
        )
        .bind(database)
        .bind(id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .map(|r| StorageWebhookAttempt {
                event_id: r.get("event_id"),
                event_type: r.get("event_type"),
                attempt: r.get("attempt"),
                outcome: r.get("outcome"),
                status: r.get("status"),
                duration_ms: r.get("duration_ms"),
                attempted_at: r.get("attempted_at"),
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(hours: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_700_000_000, 0).unwrap() + TimeDelta::hours(hours)
    }

    fn run(since: Option<i64>, n: i32) -> FailureRun {
        FailureRun {
            failing_since: since.map(at),
            consecutive_failures: n,
        }
    }

    /// A failure starts a run or continues it; a success ends it; a failure after a quiet
    /// spell longer than the retry window starts a new run instead of continuing the old one
    /// (which would disable a healthy endpoint at its first failure).
    #[test]
    fn a_quiet_spell_ends_a_failure_run() {
        assert_eq!(run(None, 0).after(None, false, at(0)), run(Some(0), 1));
        assert_eq!(
            run(Some(0), 1).after(Some(at(0)), false, at(1)),
            run(Some(0), 2)
        );
        assert_eq!(
            run(Some(0), 2).after(Some(at(1)), true, at(2)),
            run(None, 0)
        );
        // Failed for a day (its event dropped), then three quiet days: a new run.
        assert_eq!(
            run(Some(0), 30).after(Some(at(24)), false, at(96)),
            run(Some(96), 1)
        );
        // Exactly the window apart still continues; a second more starts again.
        let w = STORAGE_WEBHOOK_RETRY_WINDOW;
        let last = at(10);
        assert_eq!(
            run(Some(0), 5).after(Some(last), false, last + w),
            run(Some(0), 6)
        );
        let later = last + w + TimeDelta::seconds(1);
        assert_eq!(
            run(Some(0), 5).after(Some(last), false, later),
            FailureRun {
                failing_since: Some(later),
                consecutive_failures: 1
            }
        );
        // An attempt recorded out of order (concurrent deliveries) continues the run.
        assert_eq!(
            run(Some(0), 5).after(Some(at(10)), false, at(9)),
            run(Some(0), 6)
        );
    }
}
