//! RT1 T4: `console_session` is the sequential path's preamble extracted, not a new one. Its
//! statements are byte-identical to what `schema_for` (the sequential path's transaction:
//! `begin_sql(reset_sql)`, the role or owner preamble) sends, followed by the `COMMIT` or
//! `ROLLBACK` and the trailing reset.
//!
//! A recording fake server stands in for Postgres: it logs every simple query, every
//! `Parse` (the SQL) and every `Bind` (the parameters as text, `None` for NULL), and answers
//! the preambles with one row.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use chelabase_query::engine::{OWNER_PREAMBLE_SQL, PREAMBLE_SQL};
use chelabase_query::{QueryCache, QueryFailure, RunAs, RunSettings, console_session, schema_for};
use http::StatusCode;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use uuid::Uuid;

const RESET: &str = "SET ROLE NONE; RESET ALL";

/// The database the session runs in (RT1 final fix round: `console_session` takes it, so its
/// missing-machinery 503 names the database, as the sequential path's does).
const DB: &str = "app \"db\"";

type Log = Arc<Mutex<Vec<String>>>;

#[derive(Clone)]
struct Fake {
    log: Log,
    /// What the owner preamble answers as `current_user`.
    owner: &'static str,
    mode: Mode,
}

/// How the fake answers a preamble.
#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Ok,
    /// A dropped role: an error.
    Fails,
    /// No `chelabase.schema_version` table: Postgres's 42P01.
    NoTable,
    /// The version read finds no row: a NULL version.
    NullVersion,
}

fn message(kind: u8, body: &[u8]) -> Vec<u8> {
    let mut out = vec![kind];
    out.extend_from_slice(&i32::try_from(body.len() + 4).unwrap().to_be_bytes());
    out.extend_from_slice(body);
    out
}

fn cstr(body: &[u8]) -> (&[u8], &[u8]) {
    let end = body.iter().position(|b| *b == 0).unwrap();
    (&body[..end], &body[end + 1..])
}

/// A `Bind`'s parameters as text.
fn bind_params(body: &[u8]) -> Vec<Option<String>> {
    let (_, rest) = cstr(body); // portal
    let (_, rest) = cstr(rest); // statement
    let n_formats = usize::try_from(i16::from_be_bytes([rest[0], rest[1]])).unwrap();
    let rest = &rest[2 + 2 * n_formats..];
    let n = usize::try_from(i16::from_be_bytes([rest[0], rest[1]])).unwrap();
    let mut rest = &rest[2..];
    let mut out = Vec::new();
    for _ in 0..n {
        let len = i32::from_be_bytes(rest[..4].try_into().unwrap());
        rest = &rest[4..];
        if len < 0 {
            out.push(None);
        } else {
            let len = usize::try_from(len).unwrap();
            out.push(Some(String::from_utf8_lossy(&rest[..len]).into_owned()));
            rest = &rest[len..];
        }
    }
    out
}

/// How many columns the answer to `sql` has (0: no rows).
/// The preambles start `SELECT set_config(…` or, qualified, `SELECT pg_catalog.set_config(…`.
fn columns(sql: &str) -> usize {
    let sql = sql.replacen("SELECT pg_catalog.set_config(", "SELECT set_config(", 1);
    if sql.starts_with("SELECT set_config('role'") {
        5
    } else if sql.starts_with("SELECT set_config('statement_timeout'") {
        4
    } else {
        0
    }
}

fn row_description(n: usize) -> Vec<u8> {
    let mut body = i16::try_from(n).unwrap().to_be_bytes().to_vec();
    for i in 0..n {
        body.extend_from_slice(format!("c{i}\0").as_bytes());
        body.extend_from_slice(&0_i32.to_be_bytes()); // table oid
        body.extend_from_slice(&0_i16.to_be_bytes()); // column
        body.extend_from_slice(&25_i32.to_be_bytes()); // text
        body.extend_from_slice(&(-1_i16).to_be_bytes());
        body.extend_from_slice(&(-1_i32).to_be_bytes());
        body.extend_from_slice(&0_i16.to_be_bytes()); // text format
    }
    message(b'T', &body)
}

fn data_row(values: &[Option<&str>]) -> Vec<u8> {
    let mut body = i16::try_from(values.len()).unwrap().to_be_bytes().to_vec();
    for v in values {
        match v {
            Some(v) => {
                body.extend_from_slice(&i32::try_from(v.len()).unwrap().to_be_bytes());
                body.extend_from_slice(v.as_bytes());
            }
            None => body.extend_from_slice(&(-1_i32).to_be_bytes()),
        }
    }
    message(b'D', &body)
}

fn error(code: &str, text: &str) -> Vec<u8> {
    let mut body = Vec::new();
    for (tag, value) in [(b'S', "ERROR"), (b'V', "ERROR"), (b'C', code), (b'M', text)] {
        body.push(tag);
        body.extend_from_slice(value.as_bytes());
        body.push(0);
    }
    body.push(0);
    message(b'E', &body)
}

async fn serve(fake: Fake, mut socket: TcpStream) {
    let Ok(len) = socket.read_i32().await else {
        return;
    };
    let mut startup = vec![0; usize::try_from(len).unwrap() - 4];
    if socket.read_exact(&mut startup).await.is_err() {
        return;
    }
    let ready = message(b'Z', b"I");
    let mut hello = message(b'R', &0_i32.to_be_bytes());
    hello.extend_from_slice(&ready);
    if socket.write_all(&hello).await.is_err() {
        return;
    }
    let mut pending = Vec::new();
    let mut query = String::new();
    let mut oids: Vec<u8> = vec![0, 0];
    let mut failed = false;
    loop {
        let Ok(kind) = socket.read_u8().await else {
            return;
        };
        let Ok(len) = socket.read_i32().await else {
            return;
        };
        let mut body = vec![0; usize::try_from(len).unwrap() - 4];
        if socket.read_exact(&mut body).await.is_err() {
            return;
        }
        match kind {
            b'Q' => {
                let (sql, _) = cstr(&body);
                fake.log
                    .lock()
                    .unwrap()
                    .push(format!("Q {}", String::from_utf8_lossy(sql)));
                let mut out = message(b'C', b"OK\0");
                out.extend_from_slice(&ready);
                if socket.write_all(&out).await.is_err() {
                    return;
                }
            }
            b'P' => {
                let (_, rest) = cstr(&body);
                let (sql, rest) = cstr(rest);
                query = String::from_utf8_lossy(sql).into_owned();
                let n = usize::try_from(i16::from_be_bytes([rest[0], rest[1]])).unwrap();
                oids = rest[..2 + 4 * n].to_vec();
                fake.log.lock().unwrap().push(format!("P {query}"));
                pending.extend(message(b'1', &[]));
            }
            b'B' => {
                fake.log
                    .lock()
                    .unwrap()
                    .push(format!("B {:?}", bind_params(&body)));
                pending.extend(message(b'2', &[]));
            }
            b'D' => {
                pending.extend(message(b't', &oids));
                match columns(&query) {
                    0 => pending.extend(message(b'n', &[])),
                    n => pending.extend(row_description(n)),
                }
            }
            b'E' => {
                let version = (fake.mode != Mode::NullVersion).then_some("v1");
                if columns(&query) > 0 && fake.mode == Mode::Fails {
                    pending.extend(error("42704", "role \"editor\" does not exist"));
                    failed = true;
                } else if columns(&query) > 0 && fake.mode == Mode::NoTable {
                    pending.extend(error(
                        "42P01",
                        "relation \"chelabase.schema_version\" does not exist",
                    ));
                    failed = true;
                } else {
                    let (a, b, c) = (Some("2000"), Some("2000"), Some(fake.owner));
                    match columns(&query) {
                        5 => pending.extend(data_row(&[Some("editor"), Some("{}"), a, b, version])),
                        4 => pending.extend(data_row(&[a, b, c, version])),
                        _ => {}
                    }
                    pending.extend(message(b'C', b"SELECT 1\0"));
                }
            }
            b'C' => pending.extend(message(b'3', &[])),
            b'S' => {
                pending.extend(&ready);
                let _ = failed;
                failed = false;
                if socket.write_all(&pending).await.is_err() {
                    return;
                }
                pending.clear();
            }
            b'X' => return,
            _ => {}
        }
    }
}

async fn connect(owner: &'static str, mode: Mode) -> (tokio_postgres::Client, Log) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    // The fake's own address (never a literal: stack-urls.sh).
    let addr = listener.local_addr().unwrap();
    let fake = Fake {
        log: Log::default(),
        owner,
        mode,
    };
    let log = fake.log.clone();
    tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            tokio::spawn(serve(fake.clone(), socket));
        }
    });
    let (client, connection) = tokio_postgres::Config::new()
        .host(addr.ip().to_string())
        .port(addr.port())
        .user("x")
        .dbname("x")
        .connect(tokio_postgres::NoTls)
        .await
        .unwrap();
    tokio::spawn(connection);
    (client, log)
}

fn settings() -> RunSettings {
    RunSettings {
        max_rows: 1000,
        statement_timeout_ms: 2000,
        default_schema: "public".into(),
        reset_sql: RESET,
    }
}

fn begin() -> String {
    format!("Q BEGIN; {RESET}")
}

fn snapshot(log: &Log) -> Vec<String> {
    log.lock().unwrap().clone()
}

/// The statements `schema_for` sends before it needs rows the fake doesn't hold: the
/// sequential path's start.
async fn sequential_start(owner: &'static str, run_as: RunAs<'_>) -> Vec<String> {
    let (client, log) = connect(owner, Mode::Ok).await;
    let _ = schema_for(
        &client,
        &QueryCache::default(),
        (Uuid::nil(), "main"),
        run_as,
        &settings(),
        "rid-seq",
    )
    .await;
    snapshot(&log)
}

#[tokio::test]
async fn a_role_session_sends_the_sequential_paths_statements() {
    let claims = json!({});
    let run_as = || RunAs::Role {
        role: "editor",
        claims: &claims,
        oid: None,
    };
    let (client, log) = connect("owner", Mode::Ok).await;
    let seen_version = Arc::new(Mutex::new(None));
    let out = console_session(
        &client,
        DB,
        run_as(),
        &settings(),
        "rid-1",
        async |_c, version| {
            *seen_version.lock().unwrap() = Some(version.to_owned());
            Ok(7)
        },
    )
    .await;
    assert_eq!(out.unwrap(), 7);
    assert_eq!(
        snapshot(&log),
        vec![
            begin(),
            format!("P {PREAMBLE_SQL}"),
            r#"B [Some("editor"), Some("{}"), Some("2000"), None]"#.to_owned(),
            "Q COMMIT".to_owned(),
            format!("Q {RESET}"),
        ]
    );
    assert_eq!(seen_version.lock().unwrap().as_deref(), Some("v1"));

    // The very start of the sequential path: the same transaction opening and preamble.
    let sequential = sequential_start("owner", run_as()).await;
    assert_eq!(snapshot(&log)[..3], sequential[..3], "{sequential:?}");
}

#[tokio::test]
async fn an_owner_session_sends_the_sequential_paths_statements() {
    let run_as = || RunAs::Owner { user: "owner" };
    let (client, log) = connect("owner", Mode::Ok).await;
    let seen_version = Arc::new(Mutex::new(None));
    let out = console_session(
        &client,
        DB,
        run_as(),
        &settings(),
        "rid-2",
        async |_c, version| {
            *seen_version.lock().unwrap() = Some(version.to_owned());
            Ok("done")
        },
    )
    .await;
    assert_eq!(out.unwrap(), "done");
    assert_eq!(
        snapshot(&log),
        vec![
            begin(),
            format!("P {OWNER_PREAMBLE_SQL}"),
            r#"B [Some("2000")]"#.to_owned(),
            "Q COMMIT".to_owned(),
            format!("Q {RESET}"),
        ]
    );
    assert_eq!(seen_version.lock().unwrap().as_deref(), Some("v1"));

    let sequential = sequential_start("owner", run_as()).await;
    assert_eq!(snapshot(&log)[..3], sequential[..3], "{sequential:?}");
}

#[tokio::test]
async fn the_claims_and_the_key_role_identity_reach_the_preamble_as_given() {
    // `oid` binds a key to its role's identity (the preamble refuses another role under the
    // name); claims are sent as their JSON text.
    let claims = json!({"sub": "u-1", "q": "it's \"x\""});
    let (client, log) = connect("owner", Mode::Ok).await;
    let run_as = RunAs::Role {
        role: "editor",
        claims: &claims,
        oid: Some(16401),
    };
    console_session(&client, DB,run_as, &settings(), "rid-3", async |_c, _| Ok(()))
        .await
        .unwrap();
    let log = snapshot(&log);
    assert_eq!(log[1], format!("P {PREAMBLE_SQL}"));
    // Four parameters; the last (an int8, binary on the wire) is the oid, not NULL.
    let head = format!(
        "B [Some({:?}), Some({:?}), Some(\"2000\"), Some(",
        "editor",
        claims.to_string()
    );
    assert!(log[2].starts_with(&head), "{}", log[2]);
}

#[tokio::test]
async fn a_failing_body_rolls_back_and_a_failing_preamble_never_runs_it() {
    let claims = json!({});
    let run_as = || RunAs::Role {
        role: "editor",
        claims: &claims,
        oid: None,
    };
    let (client, log) = connect("owner", Mode::Ok).await;
    let out: Result<(), QueryFailure> =
        console_session(&client, DB,run_as(), &settings(), "rid-4", async |_c, _| {
            Err(QueryFailure::invalid_query("boom"))
        })
        .await;
    assert_eq!(out.unwrap_err().message, "boom");
    let log = snapshot(&log);
    assert_eq!(log[0], begin());
    assert_eq!(
        log[3..],
        ["Q ROLLBACK".to_owned(), format!("Q {RESET}")],
        "{log:?}"
    );

    // The preamble fails (a dropped role): the body does not run, and nothing commits.
    let (client, log) = connect("owner", Mode::Fails).await;
    let ran = AtomicBool::new(false);
    let out: Result<(), QueryFailure> =
        console_session(&client, DB,run_as(), &settings(), "rid-5", async |_c, _| {
            ran.store(true, Ordering::SeqCst);
            Ok(())
        })
        .await;
    assert!(
        !ran.load(Ordering::SeqCst),
        "the body ran after a failed preamble"
    );
    assert_eq!(out.unwrap_err().status, StatusCode::SERVICE_UNAVAILABLE);
    let log = snapshot(&log);
    assert_eq!(log[0], begin());
    assert_eq!(log[1], format!("P {PREAMBLE_SQL}"));
    assert!(!log.contains(&"Q COMMIT".to_owned()), "{log:?}");
    assert!(log.contains(&"Q ROLLBACK".to_owned()), "{log:?}");
}

#[tokio::test]
async fn an_owner_connection_running_as_someone_else_is_refused() {
    // The preamble checks the connection runs as the user the request names (a 500).
    let (client, log) = connect("somebody-else", Mode::Ok).await;
    let ran = AtomicBool::new(false);
    let out: Result<(), QueryFailure> = console_session(
        &client,
        DB,
        RunAs::Owner { user: "owner" },
        &settings(),
        "rid-6",
        async |_c, _| {
            ran.store(true, Ordering::SeqCst);
            Ok(())
        },
    )
    .await;
    assert!(!ran.load(Ordering::SeqCst));
    assert_eq!(out.unwrap_err().status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(!snapshot(&log).contains(&"Q COMMIT".to_owned()));
}

/// No `chelabase.schema_version` (the machinery is missing: a 42P01, or no row so a NULL
/// version): a 503 `unavailable` for both kinds of session, the body never runs, nothing commits.
#[tokio::test]
async fn a_missing_schema_version_is_unavailable_and_the_body_never_runs() {
    let claims = json!({});
    for mode in [Mode::NoTable, Mode::NullVersion] {
        for owner in [false, true] {
            let run_as = if owner {
                RunAs::Owner { user: "owner" }
            } else {
                RunAs::Role {
                    role: "editor",
                    claims: &claims,
                    oid: None,
                }
            };
            let (client, log) = connect("owner", mode).await;
            let ran = AtomicBool::new(false);
            let out: Result<(), QueryFailure> =
                console_session(&client, DB,run_as, &settings(), "rid-7", async |_c, _v| {
                    ran.store(true, Ordering::SeqCst);
                    Ok(())
                })
                .await;
            let case = format!("owner={owner}, {}", mode == Mode::NoTable);
            assert!(!ran.load(Ordering::SeqCst), "{case}: the body ran");
            let e = out.unwrap_err();
            assert_eq!(e.status, StatusCode::SERVICE_UNAVAILABLE, "{case}");
            assert_eq!(
                e.message,
                chelabase_query::error::schema_missing_in(DB),
                "{case}: the 503 names the database"
            );
            let log = snapshot(&log);
            assert!(!log.contains(&"Q COMMIT".to_owned()), "{case}: {log:?}");
            assert!(log.contains(&"Q ROLLBACK".to_owned()), "{case}: {log:?}");
        }
    }
}
