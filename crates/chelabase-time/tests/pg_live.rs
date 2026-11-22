//! Live: values written through one backing and read through the other, in the extended
//! (binary) and the simple (text) protocol, in UTC and non-UTC sessions. Against the metadata
//! Postgres, in a scratch schema dropped afterwards (not shared state). Run through the live
//! queue: just workbench test -p chelabase-time --test pg_live -- --run-ignored only

use chelabase_time::jiff_backed::{Date as JDate, Timestamp as JTs};
use chelabase_time::{DateExt, TimestampExt};
use chrono::{DateTime, NaiveDate, Utc};
use sqlx::postgres::{PgConnection, PgRow};
use sqlx::{AssertSqlSafe, Connection, Row};

type CTs = DateTime<Utc>;

fn url() -> String {
    std::env::var("CHELABASE_METADATA_DATABASE_URL").unwrap_or_else(|_| {
        "postgres://chelabase:chelabase@metadata-db:5432/chelabase_metadata".into()
    })
}

/// A connection with its own scratch schema; `drop_schema` removes it.
struct Scratch {
    conn: PgConnection,
    schema: String,
}

impl Scratch {
    async fn new(time_zone: &str) -> Scratch {
        let mut conn = PgConnection::connect(&url())
            .await
            .expect("the metadata Postgres (is the workbench stack up?)");
        let schema = format!(
            "chelabase_time_live_{}_{}",
            std::process::id(),
            JTs::now().unix_nanos()
        );
        sqlx::raw_sql(AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
            .execute(&mut conn)
            .await
            .unwrap();
        sqlx::raw_sql(AssertSqlSafe(format!(
            "CREATE TABLE {schema}.t (id int PRIMARY KEY, ts timestamptz, tsa timestamptz[], d date)"
        )))
        .execute(&mut conn)
        .await
        .unwrap();
        sqlx::raw_sql(AssertSqlSafe(format!("SET TimeZone = '{time_zone}'")))
            .execute(&mut conn)
            .await
            .unwrap();
        Scratch { conn, schema }
    }

    async fn drop_schema(mut self) {
        sqlx::raw_sql(AssertSqlSafe(format!(
            "DROP SCHEMA {} CASCADE",
            self.schema
        )))
        .execute(&mut self.conn)
        .await
        .unwrap();
    }
}

/// Instants at nanosecond precision, including sub-microsecond digits either side of 2000.
const INSTANTS: &[&str] = &[
    "2026-09-30T12:00:00Z",
    "2026-09-30T12:00:00.5Z",
    "2026-09-30T12:00:00.123456789Z",
    "1999-12-31T23:59:59.9999995Z",
    "2000-01-01T00:00:00.0000005Z",
    "1969-12-31T23:59:59.999999999Z",
    "1970-01-01T00:00:00Z",
    "1921-03-04T05:06:07.000001Z",
    "2199-12-31T23:59:59.999999Z",
    "0999-01-02T03:04:05Z",
];

const DATES: &[&str] = &[
    "2026-09-30",
    "1999-12-31",
    "2000-01-01",
    "0999-01-02",
    "9999-12-31",
];

/// One row as a backing reads it; `None` where the backing couldn't decode a column.
#[derive(Debug, PartialEq)]
struct Read {
    ts: Option<Option<i128>>,
    tsa: Option<Option<Vec<Option<i128>>>>,
    d: Option<Option<String>>,
}

fn read_jiff(row: &PgRow) -> Read {
    Read {
        ts: row
            .try_get::<Option<JTs>, _>("ts")
            .ok()
            .map(|t| t.map(|t| t.unix_nanos())),
        tsa: row
            .try_get::<Option<Vec<Option<JTs>>>, _>("tsa")
            .ok()
            .map(|v| v.map(|v| v.into_iter().map(|t| t.map(|t| t.unix_nanos())).collect())),
        d: row
            .try_get::<Option<JDate>, _>("d")
            .ok()
            .map(|d| d.map(|d| d.to_string())),
    }
}

fn read_chrono(row: &PgRow) -> Read {
    Read {
        ts: row
            .try_get::<Option<CTs>, _>("ts")
            .ok()
            .map(|t| t.map(|t| t.unix_nanos())),
        tsa: row
            .try_get::<Option<Vec<Option<CTs>>>, _>("tsa")
            .ok()
            .map(|v| v.map(|v| v.into_iter().map(|t| t.map(|t| t.unix_nanos())).collect())),
        d: row
            .try_get::<Option<NaiveDate>, _>("d")
            .ok()
            .map(|d| d.map(|d| d.to_string())),
    }
}

/// chrono's text decode can't read an offset with seconds (local mean time before 1920 in
/// Asia/Kathmandu, before 1884 in America/St_Johns), so a column it fails on is skipped; the
/// jiff backing must read every one.
fn agree<T: PartialEq + std::fmt::Debug>(j: &Option<T>, c: &Option<T>, what: &str) {
    let j = j
        .as_ref()
        .unwrap_or_else(|| panic!("jiff couldn't decode {what}"));
    if let Some(c) = c {
        assert_eq!(j, c, "{what}");
    }
}

/// Writes every instant and date (and a row of NULLs) through one backing, reads them back
/// through both, in both protocols, and checks they agree and equal the truncated input.
async fn round_trip(time_zone: &str) {
    let mut s = Scratch::new(time_zone).await;
    let insert = format!(
        "INSERT INTO {}.t (id, ts, tsa, d) VALUES ($1, $2, $3, $4)",
        s.schema
    );
    let n = INSTANTS.len().max(DATES.len());
    for i in 0..n {
        let ts = INSTANTS.get(i).copied();
        let d = DATES.get(i).copied();
        // Even rows go in through chrono, odd rows through jiff.
        let q = sqlx::query(AssertSqlSafe(insert.clone())).bind(i as i32 * 2);
        let q = if i % 2 == 0 {
            let c = ts.map(|s| CTs::parse_rfc3339(s).unwrap());
            q.bind(c)
                .bind(c.map(|c| vec![Some(c), None]))
                .bind(d.map(|d| NaiveDate::parse_iso(d).unwrap()))
        } else {
            let j = ts.map(|s| JTs::parse_rfc3339(s).unwrap());
            q.bind(j)
                .bind(j.map(|j| vec![Some(j), None]))
                .bind(d.map(|d| JDate::parse_iso(d).unwrap()))
        };
        q.execute(&mut s.conn).await.unwrap();
    }
    sqlx::query(AssertSqlSafe(insert.clone()))
        .bind(1_000)
        .bind(None::<JTs>)
        .bind(None::<Vec<JTs>>)
        .bind(None::<JDate>)
        .execute(&mut s.conn)
        .await
        .unwrap();

    let select = format!("SELECT ts, tsa, d FROM {}.t ORDER BY id", s.schema);
    // Extended protocol (binary results), then simple protocol (text results).
    let binary = sqlx::query(AssertSqlSafe(select.clone()))
        .fetch_all(&mut s.conn)
        .await
        .unwrap();
    let text = sqlx::raw_sql(AssertSqlSafe(select.clone()))
        .fetch_all(&mut s.conn)
        .await
        .unwrap();
    for rows in [&binary, &text] {
        assert_eq!(rows.len(), n + 1);
        for (i, row) in rows.iter().enumerate() {
            let (j, c) = (read_jiff(row), read_chrono(row));
            let what = format!("{time_zone} row {i}");
            agree(&j.ts, &c.ts, &what);
            agree(&j.tsa, &c.tsa, &what);
            agree(&j.d, &c.d, &what);
            // Postgres keeps microseconds; sqlx's encode cuts toward 2000 (for both backings),
            // which differs from `trunc_micros` only for sub-microsecond digits before 2000.
            const Y2K: i128 = 946_684_800 * 1_000_000_000;
            let cut = INSTANTS.get(i).map(|s| {
                let n = JTs::parse_rfc3339(s).unwrap().unix_nanos();
                (n - Y2K) / 1_000 * 1_000 + Y2K
            });
            assert_eq!(j.ts, Some(cut), "{what}");
            assert_eq!(j.tsa, Some(cut.map(|c| vec![Some(c), None])), "{what}");
            assert_eq!(j.d, Some(DATES.get(i).map(|d| d.to_string())), "{what}");
        }
    }
    s.drop_schema().await;
}

#[tokio::test]
#[ignore = "live: needs the workbench metadata Postgres"]
async fn round_trips_across_implementations() {
    round_trip("UTC").await;
}

#[tokio::test]
#[ignore = "live: needs the workbench metadata Postgres"]
async fn round_trips_under_non_utc_sessions() {
    // A `timestamp`-typed bind or a local-offset text decode would shift these.
    round_trip("Asia/Kathmandu").await;
    round_trip("America/St_Johns").await;
}

#[tokio::test]
#[ignore = "live: needs the workbench metadata Postgres"]
async fn binds_type_as_timestamptz() {
    let mut s = Scratch::new("UTC").await;
    let t = JTs::parse_rfc3339("2026-09-30T12:00:00Z").unwrap();
    // `$1::timestamptz IS NULL` with a NULL and a value, and `unnest` with NULLs.
    let row =
        sqlx::query("SELECT $1::timestamptz IS NULL, $2::timestamptz IS NULL, pg_typeof($2)::text")
            .bind(None::<JTs>)
            .bind(Some(t))
            .fetch_one(&mut s.conn)
            .await
            .unwrap();
    assert!(row.get::<bool, _>(0));
    assert!(!row.get::<bool, _>(1));
    assert_eq!(row.get::<String, _>(2), "timestamp with time zone");
    let rows = sqlx::query("SELECT x FROM unnest($1::timestamptz[]) AS x")
        .bind(vec![Some(t), None])
        .fetch_all(&mut s.conn)
        .await
        .unwrap();
    let got: Vec<Option<JTs>> = rows.iter().map(|r| r.get(0)).collect();
    assert_eq!(got, vec![Some(t), None]);
    s.drop_schema().await;
}

#[tokio::test]
#[ignore = "live: needs the workbench metadata Postgres"]
async fn infinity_is_an_error() {
    let mut s = Scratch::new("UTC").await;
    for v in ["infinity", "-infinity"] {
        let sql = format!("SELECT '{v}'::timestamptz AS ts, '{v}'::date AS d");
        let binary = sqlx::query(AssertSqlSafe(sql.clone()))
            .fetch_one(&mut s.conn)
            .await
            .unwrap();
        let text = sqlx::raw_sql(AssertSqlSafe(sql.clone()))
            .fetch_one(&mut s.conn)
            .await
            .unwrap();
        for row in [&binary, &text] {
            assert!(row.try_get::<JTs, _>("ts").is_err(), "{v} timestamptz");
            assert!(row.try_get::<JDate, _>("d").is_err(), "{v} date");
        }
    }
    s.drop_schema().await;
}
