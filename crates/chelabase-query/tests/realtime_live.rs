//! RT1 T6: realtime's catalog read, probe and check against Postgres (the dev metadata
//! Postgres, in a scratch database dropped afterwards). Run through the live queue:
//! just workbench test -p chelabase-query --test realtime_live -- --run-ignored only
//!
//! Each check runs in the session a viewer's console read would (here: `SET ROLE` and a
//! `TimeZone` in one transaction), so the texts it answers are that session's (P6, W3, W4).

use chelabase_query::realtime::{self, KeyColumn};
use tokio_postgres::{Client, NoTls};
use uuid::Uuid;

fn url() -> String {
    std::env::var("CHELABASE_METADATA_DATABASE_URL").unwrap_or_else(|_| {
        "postgres://chelabase:chelabase@metadata-db:5432/chelabase_metadata".into()
    })
}

async fn connect_to(database: Option<&str>) -> Client {
    let mut config: tokio_postgres::Config = url().parse().unwrap();
    if let Some(database) = database {
        config.dbname(database);
    }
    let (client, conn) = config
        .connect(NoTls)
        .await
        .expect("the metadata Postgres (is the workbench stack up?)");
    tokio::spawn(conn);
    client
}

/// A scratch database with a viewer role `<db>_viewer` (no rights until a test grants them).
struct Scratch {
    admin: Client,
    database: String,
    viewer: String,
}

impl Scratch {
    async fn new() -> Self {
        let suffix = &Uuid::new_v4().simple().to_string()[..10];
        let database = format!("t6_rt_{suffix}");
        let viewer = format!("{database}_viewer");
        let cluster = connect_to(None).await;
        // Two calls: CREATE DATABASE can't run inside a multi-statement (implicit) transaction.
        cluster
            .batch_execute(&format!("CREATE ROLE {viewer} NOLOGIN"))
            .await
            .unwrap();
        cluster
            .batch_execute(&format!("CREATE DATABASE {database}"))
            .await
            .unwrap();
        let admin = connect_to(Some(&database)).await;
        admin
            .batch_execute(&format!(
                "REVOKE ALL ON SCHEMA public FROM PUBLIC; GRANT USAGE ON SCHEMA public TO {viewer}"
            ))
            .await
            .unwrap();
        Scratch {
            admin,
            database,
            viewer,
        }
    }

    /// A session of `role` with `timezone`, in an open transaction.
    async fn session(&self, role: &str, timezone: &str) -> Client {
        let c = connect_to(Some(&self.database)).await;
        c.batch_execute(&format!(
            "BEGIN; SET LOCAL ROLE {role}; SET LOCAL TimeZone = '{timezone}'"
        ))
        .await
        .unwrap();
        c
    }

    async fn oid(&self, relation: &str) -> u32 {
        self.admin
            .query_one("SELECT $1::text::regclass::oid", &[&relation])
            .await
            .unwrap()
            .get(0)
    }

    async fn drop(self) {
        drop(self.admin);
        let cluster = connect_to(None).await;
        cluster
            .batch_execute(&format!("DROP DATABASE {} WITH (FORCE)", self.database))
            .await
            .unwrap();
        cluster
            .batch_execute(&format!("DROP ROLE {}", self.viewer))
            .await
            .unwrap();
    }
}

fn keys(rows: &[&[&str]]) -> Vec<Vec<String>> {
    rows.iter()
        .map(|r| r.iter().map(|t| t.to_string()).collect())
        .collect()
}

fn key(texts: &[&str]) -> String {
    serde_json::to_string(&texts.iter().map(|t| t.to_string()).collect::<Vec<_>>()).unwrap()
}

fn sorted(mut v: Vec<String>) -> Vec<String> {
    v.sort();
    v
}

/// The catalog read: the key in index order, each column's type schema-qualified, domains
/// (over domains) resolved to their base, enums marked.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn the_relation_is_read_by_oid_with_its_key_in_order() {
    let s = Scratch::new().await;
    s.admin
        .batch_execute(
            r#"CREATE SCHEMA "Odd ""s";
               CREATE TYPE "Odd ""s".mood AS ENUM ('ok', 'meh');
               CREATE DOMAIN "Odd ""s".email AS text;
               CREATE DOMAIN "Odd ""s".work_email AS "Odd ""s".email;
               CREATE TABLE "Odd ""s"."T ""1" (
                 b text, a bigint, m "Odd ""s".mood, e "Odd ""s".work_email, extra int,
                 PRIMARY KEY (a, m, e, b));"#,
        )
        .await
        .unwrap();
    let oid = s.oid(r#""Odd ""s"."T ""1""#).await;
    let rel = realtime::load_relation(&s.admin, oid)
        .await
        .unwrap()
        .expect("the relation");
    assert_eq!(
        (rel.oid, rel.schema.as_str(), rel.name.as_str()),
        (oid, r#"Odd "s"#, r#"T "1"#)
    );
    let names: Vec<&str> = rel.key.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(
        names,
        ["a", "m", "e", "b"],
        "primary key order, not column order"
    );
    assert_eq!(
        rel.key[0],
        KeyColumn {
            name: "a".into(),
            type_schema: "pg_catalog".into(),
            type_name: "int8".into(),
            base_schema: "pg_catalog".into(),
            base_name: "int8".into(),
            base_typtype: 'b',
        }
    );
    assert_eq!(
        (
            rel.key[1].type_schema.as_str(),
            rel.key[1].type_name.as_str(),
            rel.key[1].base_typtype
        ),
        (r#"Odd "s"#, "mood", 'e')
    );
    let e = &rel.key[2];
    assert_eq!(
        (e.type_schema.as_str(), e.type_name.as_str()),
        (r#"Odd "s"#, "work_email")
    );
    assert_eq!(
        (e.base_schema.as_str(), e.base_name.as_str(), e.base_typtype),
        ("pg_catalog", "text", 'b')
    );
    assert!(realtime::keys_supported(&rel));
    // No relation, or one without a primary key.
    assert_eq!(
        realtime::load_relation(&s.admin, 4_000_000_000)
            .await
            .unwrap(),
        None
    );
    s.admin
        .batch_execute("CREATE TABLE public.nokey (x int)")
        .await
        .unwrap();
    let nokey = s.oid("public.nokey").await;
    let read = realtime::load_relation(&s.admin, nokey).await.unwrap();
    assert!(
        read.as_ref()
            .is_none_or(|r| r.key.is_empty() && !realtime::keys_supported(r)),
        "{read:?}"
    );
    s.drop().await;
}

/// The check answers the session's own texts of the rows it sees: lossless bigint and numeric,
/// a timestamptz in the session's TimeZone, quotes and backslashes; missing keys are left out.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn the_check_answers_the_sessions_texts_of_existing_rows() {
    let s = Scratch::new().await;
    let v = &s.viewer;
    s.admin
        .batch_execute(&format!(
            "CREATE TABLE public.k (id bigint, amount numeric, at timestamptz, tag text, PRIMARY KEY (id, amount, at, tag));
             INSERT INTO public.k VALUES (9007199254740993, 1.50, '2026-10-16 12:00:00+00', 'he said \"hi\" \\ ok');
             INSERT INTO public.k VALUES (1, 2, '2026-01-01 00:00:00+00', 'x');
             GRANT SELECT ON public.k TO {v};"
        ))
        .await
        .unwrap();
    let oid = s.oid("public.k").await;
    let viewer = s.session(v, "Asia/Tokyo").await;
    let rel = realtime::load_relation(&viewer, oid)
        .await
        .unwrap()
        .unwrap();
    // The decoder's ISO texts (UTC offsets), parsed by plain casts.
    let wanted = keys(&[
        &[
            "9007199254740993",
            "1.50",
            "2026-10-16 12:00:00+00",
            "he said \"hi\" \\ ok",
        ],
        &["2", "2", "2026-01-01 00:00:00+00", "x"],
    ]);
    let checked = realtime::check(&viewer, &rel, None, &wanted).await.unwrap();
    assert!(checked.visible, "no xid: visible");
    // The session's output: Tokyo time, the numeric's scale kept.
    assert_eq!(
        checked.keys,
        vec![key(&[
            "9007199254740993",
            "1.50",
            "2026-10-16 21:00:00+09",
            "he said \"hi\" \\ ok"
        ])]
    );
    // An empty key list is a valid (empty) check.
    let none = realtime::check(&viewer, &rel, None, &[]).await.unwrap();
    assert_eq!((none.visible, none.keys.len()), (true, 0));
    s.drop().await;
}

/// RLS applies: an RLS viewer's check answers only the rows its policy lets it read, and the
/// probe says so (`bypass: false`); the table's owner without FORCE bypasses.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn row_security_filters_the_check_and_shows_in_the_probe() {
    let s = Scratch::new().await;
    let v = &s.viewer;
    s.admin
        .batch_execute(&format!(
            "CREATE TABLE public.notes (id int PRIMARY KEY, owner text NOT NULL);
             INSERT INTO public.notes VALUES (1, '{v}'), (2, 'someone else');
             ALTER TABLE public.notes ENABLE ROW LEVEL SECURITY;
             CREATE POLICY mine ON public.notes FOR SELECT USING (owner = current_user);
             GRANT SELECT ON public.notes TO {v};"
        ))
        .await
        .unwrap();
    let oid = s.oid("public.notes").await;
    let viewer = s.session(v, "UTC").await;
    let rel = realtime::load_relation(&viewer, oid)
        .await
        .unwrap()
        .unwrap();
    assert!(
        !realtime::probe(&viewer, &rel).await.unwrap().bypass,
        "an RLS viewer"
    );
    let checked = realtime::check(&viewer, &rel, None, &keys(&[&["1"], &["2"]]))
        .await
        .unwrap();
    assert_eq!(checked.keys, vec![key(&["1"])]);
    // The admin (the owner here, superuser) bypasses.
    assert!(
        realtime::probe(&s.admin, &rel).await.unwrap().bypass,
        "the superuser bypasses"
    );
    let all = realtime::check(&s.admin, &rel, None, &keys(&[&["1"], &["2"]]))
        .await
        .unwrap();
    assert_eq!(sorted(all.keys), vec![key(&["1"]), key(&["2"])]);
    s.drop().await;
}

/// W2: the probe is a real read, so a role with SELECT on the table but no USAGE on its schema
/// fails it (and so does one without SELECT on a key column).
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn the_probe_fails_without_schema_usage_or_column_select() {
    let s = Scratch::new().await;
    let v = &s.viewer;
    s.admin
        .batch_execute(&format!(
            "CREATE SCHEMA private;
             CREATE TABLE private.t (id int PRIMARY KEY);
             GRANT SELECT ON private.t TO {v};
             CREATE TABLE public.cols (id int PRIMARY KEY, body text);
             GRANT SELECT (body) ON public.cols TO {v};"
        ))
        .await
        .unwrap();
    let private = s.oid("private.t").await;
    let cols = s.oid("public.cols").await;
    // Described by the admin: realtime's catalog read may run elsewhere than the probe.
    let private_rel = realtime::load_relation(&s.admin, private)
        .await
        .unwrap()
        .unwrap();
    let cols_rel = realtime::load_relation(&s.admin, cols)
        .await
        .unwrap()
        .unwrap();
    let viewer = s.session(v, "UTC").await;
    assert!(
        realtime::probe(&viewer, &private_rel).await.is_err(),
        "no USAGE on the schema"
    );
    viewer.batch_execute("ROLLBACK").await.unwrap();
    let viewer = s.session(v, "UTC").await;
    assert!(
        realtime::probe(&viewer, &cols_rel).await.is_err(),
        "no SELECT on the key column"
    );
    viewer.batch_execute("ROLLBACK").await.unwrap();
    // With USAGE, the same probe passes.
    s.admin
        .batch_execute(&format!("GRANT USAGE ON SCHEMA private TO {v}"))
        .await
        .unwrap();
    let viewer = s.session(v, "UTC").await;
    assert!(
        realtime::probe(&viewer, &private_rel).await.unwrap().bypass,
        "RLS is off"
    );
    s.drop().await;
}

/// W27: a key type the session can't use (an enum in a schema without USAGE) is an error
/// (`changed`), never a fallback to text comparison.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn a_key_type_the_session_cannot_use_is_an_error() {
    let s = Scratch::new().await;
    let v = &s.viewer;
    s.admin
        .batch_execute(&format!(
            "CREATE SCHEMA hidden;
             CREATE TYPE hidden.mood AS ENUM ('ok', 'meh');
             CREATE TABLE public.m (m hidden.mood PRIMARY KEY);
             INSERT INTO public.m VALUES ('ok');
             GRANT SELECT ON public.m TO {v};"
        ))
        .await
        .unwrap();
    let rel = realtime::load_relation(&s.admin, s.oid("public.m").await)
        .await
        .unwrap()
        .unwrap();
    let viewer = s.session(v, "UTC").await;
    assert!(
        realtime::check(&viewer, &rel, None, &keys(&[&["ok"]]))
            .await
            .is_err()
    );
    s.drop().await;
}

/// P7/W6: a transaction still in progress for the statement's snapshot is not visible (and no
/// key is answered); once committed, the same check sees it.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn an_xid_still_in_progress_is_not_visible_then_is() {
    let s = Scratch::new().await;
    let v = &s.viewer;
    s.admin
        .batch_execute(&format!(
            "CREATE TABLE public.v (id int PRIMARY KEY); GRANT SELECT ON public.v TO {v};"
        ))
        .await
        .unwrap();
    let oid = s.oid("public.v").await;
    let writer = connect_to(Some(&s.database)).await;
    writer
        .batch_execute("BEGIN; INSERT INTO public.v VALUES (7)")
        .await
        .unwrap();
    let xid: String = writer
        .query_one("SELECT pg_current_xact_id()::text", &[])
        .await
        .unwrap()
        .get(0);
    let xid: u64 = xid.parse().unwrap();

    // A fresh statement per check (READ COMMITTED): a new snapshot each time.
    let reader = connect_to(Some(&s.database)).await;
    reader
        .batch_execute(&format!("SET ROLE {v}"))
        .await
        .unwrap();
    let rel = realtime::load_relation(&reader, oid)
        .await
        .unwrap()
        .unwrap();
    let before = realtime::check(&reader, &rel, Some(xid), &keys(&[&["7"]]))
        .await
        .unwrap();
    assert_eq!(
        (before.visible, before.keys.len()),
        (false, 0),
        "{before:?}"
    );
    writer.batch_execute("COMMIT").await.unwrap();
    let after = realtime::check(&reader, &rel, Some(xid), &keys(&[&["7"]]))
        .await
        .unwrap();
    assert_eq!((after.visible, after.keys), (true, vec![key(&["7"])]));
    // An xid long committed is visible.
    let old = realtime::check(&reader, &rel, Some(3), &keys(&[&["7"]]))
        .await
        .unwrap();
    assert!(old.visible);
    s.drop().await;
}

/// The catalog read, the probe and the check run as the viewer's role, under its
/// `search_path`. A role that puts its own schema before `pg_catalog` and shadows
/// `row_security_active` (always false), `unnest` (nothing), `=` on its key type (always
/// true), `pg_visible_in_snapshot` (always true) and the `text` type (a domain refusing every
/// value) changes nothing: it's still an RLS viewer, sees only its own requested rows, and an
/// in-progress xid is still not visible.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn a_viewers_search_path_cannot_shadow_the_catalog() {
    let s = Scratch::new().await;
    let v = &s.viewer;
    s.admin
        .batch_execute(&format!(
            "CREATE TABLE public.notes (id int PRIMARY KEY, owner text NOT NULL);
             INSERT INTO public.notes VALUES (1, '{v}'), (2, 'someone else'), (3, '{v}');
             ALTER TABLE public.notes ENABLE ROW LEVEL SECURITY;
             CREATE POLICY mine ON public.notes FOR SELECT USING (owner = current_user);
             GRANT SELECT ON public.notes TO {v};
             CREATE SCHEMA evil;
             GRANT USAGE ON SCHEMA evil TO {v};
             CREATE FUNCTION evil.row_security_active(pg_catalog.oid) RETURNS pg_catalog.bool
               LANGUAGE sql AS 'SELECT false';
             CREATE FUNCTION evil.row_security_active(pg_catalog.text) RETURNS pg_catalog.bool
               LANGUAGE sql AS 'SELECT false';
             CREATE FUNCTION evil.unnest(anyarray) RETURNS SETOF anyelement
               LANGUAGE sql AS 'SELECT x FROM pg_catalog.unnest($1) AS x WHERE false';
             CREATE FUNCTION evil.always(pg_catalog.int4, pg_catalog.int4) RETURNS pg_catalog.bool
               LANGUAGE sql AS 'SELECT true';
             CREATE OPERATOR evil.= (LEFTARG = pg_catalog.int4, RIGHTARG = pg_catalog.int4, FUNCTION = evil.always);
             CREATE FUNCTION evil.pg_visible_in_snapshot(pg_catalog.xid8, pg_catalog.pg_snapshot)
               RETURNS pg_catalog.bool LANGUAGE sql AS 'SELECT true';
             CREATE DOMAIN evil.text AS pg_catalog.text CHECK (VALUE IS NULL);"
        ))
        .await
        .unwrap();
    let oid = s.oid("public.notes").await;
    let writer = connect_to(Some(&s.database)).await;
    writer
        .batch_execute(&format!(
            "BEGIN; INSERT INTO public.notes VALUES (7, '{v}')"
        ))
        .await
        .unwrap();
    let xid: String = writer
        .query_one("SELECT pg_current_xact_id()::text", &[])
        .await
        .unwrap()
        .get(0);
    let xid: u64 = xid.parse().unwrap();

    let viewer = s.session(v, "UTC").await;
    viewer
        .batch_execute("SET LOCAL search_path = evil, pg_catalog")
        .await
        .unwrap();
    // The shadowing is real in this session.
    let shadowed: bool = viewer
        .query_one("SELECT row_security_active('public.notes'::regclass)", &[])
        .await
        .unwrap()
        .get(0);
    assert!(
        !shadowed,
        "evil.row_security_active answers in this session"
    );

    let rel = realtime::load_relation(&viewer, oid)
        .await
        .unwrap()
        .expect("the relation");
    let names: Vec<&str> = rel.key.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(
        names,
        ["id"],
        "the catalog read isn't fooled by evil.unnest: {rel:?}"
    );
    assert_eq!(
        (
            rel.key[0].base_schema.as_str(),
            rel.key[0].base_name.as_str()
        ),
        ("pg_catalog", "int4")
    );
    let probe = realtime::probe(&viewer, &rel).await.unwrap();
    assert!(
        !probe.bypass,
        "still an RLS viewer under evil.row_security_active"
    );
    let checked = realtime::check(&viewer, &rel, None, &keys(&[&["1"], &["2"]]))
        .await
        .unwrap();
    assert_eq!(
        checked.keys,
        vec![key(&["1"])],
        "only the requested row it may see (not row 3 through evil.=, not none through evil.unnest)"
    );
    let pending = realtime::check(&viewer, &rel, Some(xid), &keys(&[&["7"]]))
        .await
        .unwrap();
    assert_eq!(
        (pending.visible, pending.keys.len()),
        (false, 0),
        "evil.pg_visible_in_snapshot doesn't make an in-progress xid visible: {pending:?}"
    );
    writer.batch_execute("ROLLBACK").await.unwrap();
    drop(viewer);
    drop(writer);
    s.drop().await;
}

/// RT1 T8: the conversion prints the decoder's texts as the session does, for keys whose rows
/// are gone (owner deletes), in the request's order; a text the type can't read fails.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn the_conversion_prints_deleted_keys_in_the_sessions_form() {
    let s = Scratch::new().await;
    s.admin
        .batch_execute(
            "CREATE TABLE public.ev (at timestamptz, amount numeric, id bigint, PRIMARY KEY (at, amount, id));",
        )
        .await
        .unwrap();
    let oid = s.oid("public.ev").await;
    // The owner's own session (no SET ROLE), Tokyo time; no row exists.
    let owner = connect_to(Some(&s.database)).await;
    owner
        .batch_execute("BEGIN; SET LOCAL TimeZone = 'Asia/Tokyo'")
        .await
        .unwrap();
    let rel = realtime::load_relation(&owner, oid).await.unwrap().unwrap();
    let sql = realtime::convert_sql(&rel);
    assert!(!sql.is_empty(), "no SQL built");
    let col = |i: usize, texts: &[&str]| -> Vec<Option<String>> {
        let _ = i;
        texts.iter().map(|t| Some(t.to_string())).collect()
    };
    let at = col(0, &["2026-10-16 12:00:00+00", "2026-01-01 00:00:00+00"]);
    let amount = col(1, &["1.50", "2"]);
    let id = col(2, &["9007199254740993", "1"]);
    let rows = owner.query(&sql, &[&at, &amount, &id]).await.unwrap();
    let got: Vec<String> = rows
        .iter()
        .map(|r| {
            let texts: Vec<String> = (0..3)
                .map(|i| r.get::<_, String>(r.len() - 3 + i))
                .collect();
            realtime::key_string(&texts)
        })
        .collect();
    assert_eq!(
        got,
        vec![
            key(&["2026-10-16 21:00:00+09", "1.50", "9007199254740993"]),
            key(&["2026-01-01 09:00:00+09", "2", "1"]),
        ],
        "the session's TimeZone, the numeric's scale, in order, without the rows"
    );
    owner.batch_execute("ROLLBACK").await.unwrap();
    // A text the type can't read is an error, never a guess.
    owner.batch_execute("BEGIN").await.unwrap();
    let bad = col(0, &["not a time"]);
    let one = col(1, &["1"]);
    assert!(owner.query(&sql, &[&bad, &one, &one]).await.is_err());
    owner.batch_execute("ROLLBACK").await.unwrap();
    drop(owner);
    s.drop().await;
}
