//! The bench database's fixture: `bench_items` with 100,000 rows, and the `bench` schema with
//! the two functions C needs (the bench copies of the spec's P2 functions). Run as the
//! database's owner.

use std::net::IpAddr;

use anyhow::Context;
use sqlx::Connection;

/// The table, its rows and its index. `n = i % 1000`.
pub const TABLE_SQL: &str = "\
CREATE TABLE bench_items (
  id bigint PRIMARY KEY,
  owner uuid NOT NULL,
  title text NOT NULL,
  n int NOT NULL,
  created_at timestamptz NOT NULL DEFAULT now()
);
INSERT INTO bench_items (id, owner, title, n)
  SELECT i, gen_random_uuid(), 'item ' || i, i % 1000 FROM generate_series(1, 100000) AS i;
CREATE INDEX ON bench_items (n);
GRANT SELECT, INSERT ON bench_items TO authenticated;";

/// Run after seeding and before each throughput round. Not inside a transaction (VACUUM
/// refuses one, and a multi-statement simple query is one).
pub const VACUUM_SQL: &str = "VACUUM (ANALYZE) bench_items";

/// `VACUUM (ANALYZE) bench_items` as the owner.
pub async fn vacuum(owner: &mut sqlx::PgConnection) -> anyhow::Result<()> {
    sqlx::raw_sql(VACUUM_SQL)
        .execute(&mut *owner)
        .await
        .context("vacuuming bench_items")?;
    Ok(())
}

/// The `bench` schema and its two functions, executable by `authenticated`.
pub const FUNCTIONS_SQL: &str = r#"
CREATE SCHEMA bench;

-- SECURITY DEFINER: the preamble calls it after set_config('role', 'authenticated'), and
-- authenticated has no SELECT on chelabase.schema_version (today's preamble reads it in an
-- initplan, which runs before the role switch, as authenticator).
CREATE FUNCTION bench.assert_schema_version(expected uuid) RETURNS uuid
LANGUAGE plpgsql STABLE SECURITY DEFINER SET search_path = '' AS $f$
DECLARE v uuid;
BEGIN
  SELECT version INTO v FROM chelabase.schema_version;
  IF v IS DISTINCT FROM expected THEN
    RAISE EXCEPTION USING ERRCODE = 'CB001', MESSAGE = 'chelabase: stale schema version';
  END IF;
  RETURN v;
END $f$;

CREATE FUNCTION bench.cap_response(result text, budget bigint) RETURNS text
LANGUAGE plpgsql VOLATILE SET search_path = '' AS $f$
DECLARE total bigint;
BEGIN
  total := coalesce(nullif(pg_catalog.current_setting('chelabase.response_bytes', true), ''), '0')::bigint
           + coalesce(pg_catalog.octet_length(result), 0);
  IF total > budget THEN
    RAISE EXCEPTION USING ERRCODE = 'CB002', MESSAGE = 'chelabase: response too large';
  END IF;
  PERFORM pg_catalog.set_config('chelabase.response_bytes', total::text, true);
  RETURN result;
END $f$;

-- The authenticator too: C prepares its statements before the role switch, as rest's
-- `chelabase` schema allows (worker `data_api.rs` grants it USAGE).
GRANT USAGE ON SCHEMA bench TO authenticator, authenticated;
-- EXECUTE as well: it's checked at executor start, before the preamble's set_config('role').
GRANT EXECUTE ON FUNCTION bench.assert_schema_version(uuid) TO authenticator, authenticated;
GRANT EXECUTE ON FUNCTION bench.cap_response(text, bigint) TO authenticator, authenticated;
"#;

/// Creates the fixture, then returns the schema version (read after every DDL, since DDL
/// bumps it).
pub async fn seed(owner: &mut sqlx::PgConnection) -> anyhow::Result<uuid::Uuid> {
    sqlx::raw_sql(TABLE_SQL)
        .execute(&mut *owner)
        .await
        .context("creating bench_items")?;
    vacuum(&mut *owner).await?;
    sqlx::raw_sql(FUNCTIONS_SQL)
        .execute(&mut *owner)
        .await
        .context("creating the bench schema")?;
    sqlx::query_scalar("SELECT version FROM chelabase.schema_version")
        .fetch_one(&mut *owner)
        .await
        .context("reading the schema version")
}

/// The address the compute sees this machine's connections come from (rest's, for the
/// netem filter): connects as the authenticator and asks.
pub async fn client_addr(info: &rest::conn::ConnInfo) -> anyhow::Result<IpAddr> {
    let options = sqlx::postgres::PgConnectOptions::new()
        .host(&info.host)
        .port(info.port)
        .username(&info.user)
        .password(&info.password)
        .database(&info.database)
        .ssl_mode(sqlx::postgres::PgSslMode::Prefer)
        .application_name("bench-addr");
    let mut conn = sqlx::PgConnection::connect_with(&options)
        .await
        .context("connecting as the authenticator")?;
    let addr: Option<String> = sqlx::query_scalar("SELECT host(inet_client_addr())")
        .fetch_one(&mut conn)
        .await?;
    conn.close().await?;
    let addr = addr.context("inet_client_addr() is NULL: the connection is a Unix socket")?;
    addr.parse()
        .with_context(|| format!("parsing the client address {addr:?}"))
}

/// Checks that every bench session (`(application_name, host(client_addr))` rows) comes from
/// `dst`, the address netem delays, and that each app in `expect` has one. Returns the number
/// of sessions checked; the error names the offending apps.
pub fn check_client_addrs(
    sessions: &[(String, Option<String>)],
    dst: IpAddr,
    expect: &[&str],
) -> anyhow::Result<usize> {
    let want = dst.to_string();
    let mut bad: Vec<String> = sessions
        .iter()
        .filter(|(_, addr)| addr.as_deref() != Some(want.as_str()))
        .map(|(app, addr)| format!("{app} from {}", addr.as_deref().unwrap_or("NULL")))
        .collect();
    bad.extend(
        expect
            .iter()
            .filter(|app| !sessions.iter().any(|(a, _)| a == *app))
            .map(|app| format!("{app} has no session")),
    );
    anyhow::ensure!(
        bad.is_empty(),
        "bench sessions not from {dst} (netem would miss them): {}",
        bad.join(", ")
    );
    Ok(sessions.len())
}

/// The bench sessions' client addresses, as the compute sees them.
pub async fn bench_client_addrs(
    owner: &mut sqlx::PgConnection,
) -> anyhow::Result<Vec<(String, Option<String>)>> {
    sqlx::query_as(
        "SELECT application_name, host(client_addr) FROM pg_stat_activity \
         WHERE application_name LIKE 'bench-%' \
            OR (application_name = 'chelabase-rest' AND datname = current_database()) \
         ORDER BY 1",
    )
    .fetch_all(owner)
    .await
    .context("reading the bench sessions' client addresses")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// C prepares its statements before the role switch, as the authenticator (as rest
    /// does with `chelabase`, which grants it USAGE).
    #[test]
    fn bench_schema_is_usable_by_the_authenticator() {
        assert!(
            FUNCTIONS_SQL.contains("GRANT USAGE ON SCHEMA bench TO authenticator, authenticated;"),
            "{FUNCTIONS_SQL}"
        );
        // EXECUTE is checked at executor start, before the preamble's set_config('role').
        for f in ["assert_schema_version(uuid)", "cap_response(text, bigint)"] {
            let grant =
                format!("GRANT EXECUTE ON FUNCTION bench.{f} TO authenticator, authenticated;");
            assert!(FUNCTIONS_SQL.contains(&grant), "{grant}");
        }
    }

    fn s(app: &str, addr: Option<&str>) -> (String, Option<String>) {
        (app.into(), addr.map(Into::into))
    }

    const APPS: [&str; 4] = ["bench-a", "bench-b", "bench-c", "bench-d"];

    #[test]
    fn client_addrs_all_match() {
        let dst: IpAddr = "172.19.0.1".parse().unwrap();
        let rows: Vec<_> = APPS.iter().map(|a| s(a, Some("172.19.0.1"))).collect();
        assert_eq!(check_client_addrs(&rows, dst, &APPS).unwrap(), 4);
    }

    #[test]
    fn client_addrs_mismatch_names_the_apps() {
        let dst: IpAddr = "172.19.0.1".parse().unwrap();
        let rows = vec![
            s("bench-a", Some("172.19.0.1")),
            s("bench-b", Some("10.0.0.9")),
            s("bench-c", None),
            s("bench-d", Some("172.19.0.1")),
        ];
        let e = check_client_addrs(&rows, dst, &APPS)
            .unwrap_err()
            .to_string();
        assert!(e.contains("bench-b") && e.contains("10.0.0.9"), "{e}");
        assert!(e.contains("bench-c"), "{e}");
        assert!(!e.contains("bench-a"), "{e}");
    }

    #[test]
    fn client_addrs_missing_variant_fails() {
        let dst: IpAddr = "172.19.0.1".parse().unwrap();
        let rows = vec![s("bench-a", Some("172.19.0.1"))];
        let e = check_client_addrs(&rows, dst, &APPS)
            .unwrap_err()
            .to_string();
        assert!(e.contains("bench-d"), "{e}");
    }

    /// The fixture ends vacuumed and analyzed. VACUUM can't run in the table script's
    /// implicit transaction, so it's its own statement.
    #[test]
    fn seed_vacuums_and_analyzes_separately() {
        assert_eq!(VACUUM_SQL, "VACUUM (ANALYZE) bench_items");
        assert!(!TABLE_SQL.contains("ANALYZE"), "{TABLE_SQL}");
    }
}
