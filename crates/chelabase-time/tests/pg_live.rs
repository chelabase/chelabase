//! Live: values written through the binds and read back in the extended (binary) and the
//! simple (text) protocol, in UTC and non-UTC sessions. Against the metadata Postgres, in a
//! scratch schema dropped afterwards (not shared state). Run through the live queue:
//! just workbench test -p chelabase-time --test pg_live -- --run-ignored only

use chelabase_time::{Date as JDate, DateExt, Timestamp as JTs, TimestampExt};
use sqlx::postgres::{PgConnection, PgRow};
use sqlx::{AssertSqlSafe, Connection, Row};

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

/// One row as read; `None` where a column didn't decode.
#[derive(Debug, PartialEq)]
struct Read {
    ts: Option<Option<i128>>,
    tsa: Option<Option<Vec<Option<i128>>>>,
    d: Option<Option<String>>,
}

fn read(row: &PgRow) -> Read {
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

/// Writes every instant and date (and a row of NULLs), reads them back in both protocols, and
/// checks they equal the input as Postgres keeps it. An offset with seconds in the text
/// protocol (local mean time before 1920 in Asia/Kathmandu, before 1884 in America/St_Johns),
/// which chrono's text decode refused, must decode.
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
        let j = ts.map(|s| JTs::parse_rfc3339(s).unwrap());
        sqlx::query(AssertSqlSafe(insert.clone()))
            .bind(i as i32 * 2)
            .bind(j)
            .bind(j.map(|j| vec![Some(j), None]))
            .bind(d.map(|d| JDate::parse_iso(d).unwrap()))
            .execute(&mut s.conn)
            .await
            .unwrap();
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
            let j = read(row);
            let what = format!("{time_zone} row {i}");
            // Postgres keeps microseconds; sqlx's encode cuts toward 2000 (as chrono's did),
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

/// "Postgres round trip": an instant [`JTs::round_trips_through_pg`] holds for, written as its
/// `to_rfc3339_micros` literal and read back as `extract(epoch …)` microseconds (as the worker
/// does with a role's `VALID UNTIL`), is itself truncated, in a UTC and a far-east session; the
/// literal of an instant before [`JTs::PG_ROUND_TRIP_MIN`] is refused.
#[tokio::test]
#[ignore = "live: needs the workbench metadata Postgres"]
async fn the_round_trip_range_is_what_postgres_stores_and_reads_back() {
    let fits = [
        JTs::PG_ROUND_TRIP_MIN,
        JTs::PG_ROUND_TRIP_MIN + std::time::Duration::from_nanos(1_500),
        "1969-12-31T23:59:59.999999999Z".parse().unwrap(),
        "9999-12-30T22:00:00Z".parse().unwrap(),
        "9999-12-30T22:00:00.5Z".parse().unwrap(),
        JTs::PG_ROUND_TRIP_MAX,
        JTs::MAX,
    ];
    let refused: [JTs; 3] = [
        JTs::PG_ROUND_TRIP_MIN - std::time::Duration::from_micros(1),
        "-4713-11-24T00:00:00Z".parse().unwrap(),
        JTs::MIN,
    ];
    for time_zone in ["UTC", "Etc/GMT-14"] {
        let mut s = Scratch::new(time_zone).await;
        for t in fits {
            assert!(t.round_trips_through_pg(), "{t:?}");
            let sql = format!(
                "SELECT (extract(epoch FROM '{}'::timestamptz) * 1000000)::bigint",
                t.to_rfc3339_micros()
            );
            let micros: i64 = sqlx::query_scalar(AssertSqlSafe(sql))
                .fetch_one(&mut s.conn)
                .await
                .unwrap();
            assert_eq!(
                JTs::from_unix_micros(micros),
                Some(t.trunc_micros()),
                "{t:?} in {time_zone}"
            );
        }
        for t in refused {
            assert!(!t.round_trips_through_pg(), "{t:?}");
            let sql = format!("SELECT '{}'::timestamptz", t.to_rfc3339_micros());
            let err = sqlx::raw_sql(AssertSqlSafe(sql)).execute(&mut s.conn).await;
            assert!(err.is_err(), "{t:?} in {time_zone}");
        }
        s.drop_schema().await;
    }
}
