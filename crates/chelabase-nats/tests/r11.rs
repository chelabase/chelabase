//! RT1 T1 (R11): the crate's credentials, inbox prefix and the `rt` module, without a server.
//! A fake NATS server (INFO, PONG, and a record of CONNECT/PUB/SUB) stands in where a client
//! must talk to one.

use chelabase_nats::rt::{self, AccessEvent, ComputeStopping, ProjectEvent};
use chelabase_nats::{NatsConfig, connect};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;
use uuid::Uuid;

// --- a fake server ----------------------------------------------------------------------------

#[derive(Debug)]
enum Frame {
    Connect(Value),
    Pub { subject: String, payload: Vec<u8> },
    Sub(String),
}

/// Listens on 127.0.0.1, answers every client's handshake and records what it sends.
async fn fake_server() -> (String, mpsc::UnboundedReceiver<Frame>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("nats://{}", listener.local_addr().unwrap());
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        loop {
            let Ok((socket, _)) = listener.accept().await else {
                return;
            };
            let tx = tx.clone();
            tokio::spawn(async move {
                let (read, mut write) = socket.into_split();
                let info = json!({
                    "server_id": "fake", "server_name": "fake", "version": "2.15.0",
                    "proto": 1, "headers": true, "max_payload": 1_048_576,
                    "host": std::net::Ipv4Addr::LOCALHOST.to_string(), "port": 4222
                });
                if write
                    .write_all(format!("INFO {info}\r\n").as_bytes())
                    .await
                    .is_err()
                {
                    return;
                }
                let mut read = BufReader::new(read);
                let mut line = String::new();
                loop {
                    line.clear();
                    if read.read_line(&mut line).await.unwrap_or(0) == 0 {
                        return;
                    }
                    let l = line.trim_end();
                    let mut words = l.split_whitespace();
                    match words.next().unwrap_or("").to_ascii_uppercase().as_str() {
                        "CONNECT" => {
                            let body = l.split_once(' ').map(|(_, b)| b).unwrap_or("{}");
                            let _ = tx.send(Frame::Connect(
                                serde_json::from_str(body).unwrap_or(Value::Null),
                            ));
                        }
                        "PING" => {
                            let _ = write.write_all(b"PONG\r\n").await;
                        }
                        "SUB" => {
                            let _ = tx.send(Frame::Sub(words.next().unwrap_or("").to_string()));
                        }
                        op @ ("PUB" | "HPUB") => {
                            let parts: Vec<&str> = words.collect();
                            let subject = parts[0].to_string();
                            let total: usize = parts.last().unwrap().parse().unwrap();
                            let headers: usize = if op == "HPUB" {
                                parts[parts.len() - 2].parse().unwrap()
                            } else {
                                0
                            };
                            let mut buf = vec![0u8; total + 2];
                            if read.read_exact(&mut buf).await.is_err() {
                                return;
                            }
                            buf.truncate(total);
                            let _ = tx.send(Frame::Pub {
                                subject,
                                payload: buf[headers..].to_vec(),
                            });
                        }
                        _ => {}
                    }
                }
            });
        }
    });
    (url, rx)
}

async fn next_frame(
    rx: &mut mpsc::UnboundedReceiver<Frame>,
    want: impl Fn(&Frame) -> bool,
) -> Frame {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let f = rx.recv().await.expect("fake server closed");
            if want(&f) {
                return f;
            }
        }
    })
    .await
    .expect("the client never sent the expected frame")
}

fn config(
    url: &str,
    user: Option<&str>,
    password: Option<&str>,
    inbox: Option<&str>,
) -> NatsConfig {
    NatsConfig {
        url: url.to_string(),
        user: user.map(str::to_string),
        password: password.map(str::to_string),
        inbox_prefix: inbox.map(str::to_string),
    }
}

// --- NatsConfig::from_lookup (S1) -----------------------------------------------------------

fn lookup(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
    let map: HashMap<String, String> = vars
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    move |k| map.get(k).cloned()
}

/// A file in the system temp dir (no tempfile dependency: S7, T1 changes no Cargo.toml).
fn temp_file(name: &str, contents: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("chelabase-nats-r11-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    std::fs::write(&path, contents).unwrap();
    path
}

#[test]
fn neither_user_nor_password_is_anonymous_with_the_default_url() {
    let c = NatsConfig::from_lookup(lookup(&[])).unwrap();
    // The crate's default for a host process (scripts/tests/stack-urls.sh keeps stack addresses
    // out of test literals).
    assert_eq!(c.url, format!("nats://{}:4222", std::net::Ipv4Addr::LOCALHOST));
    assert_eq!((c.user, c.password, c.inbox_prefix), (None, None, None));
}

#[test]
fn the_password_comes_from_its_file_and_the_prefix_from_the_env() {
    let file = temp_file("realtime.password", "s3cr3t-Value_1");
    let c = NatsConfig::from_lookup(lookup(&[
        ("CHELABASE_NATS_URL", "nats://nats:4222"),
        ("CHELABASE_NATS_USER", "realtime"),
        ("CHELABASE_NATS_PASSWORD_FILE", file.to_str().unwrap()),
        ("CHELABASE_NATS_INBOX_PREFIX", "_INBOX_realtime"),
    ]))
    .unwrap();
    assert_eq!(c.url, "nats://nats:4222");
    assert_eq!(c.user.as_deref(), Some("realtime"));
    assert_eq!(c.password.as_deref(), Some("s3cr3t-Value_1"));
    assert_eq!(c.inbox_prefix.as_deref(), Some("_INBOX_realtime"));
}

#[test]
fn a_trailing_newline_in_the_password_file_is_not_part_of_the_password() {
    let file = temp_file("worker.password", "pw-123\n");
    let c = NatsConfig::from_lookup(lookup(&[
        ("CHELABASE_NATS_USER", "worker"),
        ("CHELABASE_NATS_PASSWORD_FILE", file.to_str().unwrap()),
    ]))
    .unwrap();
    assert_eq!(c.password.as_deref(), Some("pw-123"));
}

#[test]
fn the_plain_password_works_without_a_file() {
    let c = NatsConfig::from_lookup(lookup(&[
        ("CHELABASE_NATS_USER", "runner"),
        ("CHELABASE_NATS_PASSWORD", "plain-pw"),
    ]))
    .unwrap();
    assert_eq!(c.password.as_deref(), Some("plain-pw"));
}

#[test]
fn both_password_forms_at_once_are_refused_without_echoing_either() {
    let file = temp_file("orchestrator.password", "file-secret-xyz");
    let err = NatsConfig::from_lookup(lookup(&[
        ("CHELABASE_NATS_USER", "orchestrator"),
        ("CHELABASE_NATS_PASSWORD", "env-secret-abc"),
        ("CHELABASE_NATS_PASSWORD_FILE", file.to_str().unwrap()),
    ]))
    .unwrap_err();
    assert!(err.contains("CHELABASE_NATS_PASSWORD"), "{err}");
    assert!(
        !err.contains("env-secret-abc") && !err.contains("file-secret-xyz"),
        "{err}"
    );
}

#[test]
fn a_user_without_a_password_is_refused() {
    let err = NatsConfig::from_lookup(lookup(&[("CHELABASE_NATS_USER", "worker")])).unwrap_err();
    assert!(err.contains("CHELABASE_NATS_USER"), "{err}");
}

#[test]
fn a_password_without_a_user_is_refused() {
    assert!(NatsConfig::from_lookup(lookup(&[("CHELABASE_NATS_PASSWORD", "pw")])).is_err());
}

#[test]
fn a_missing_or_empty_password_file_is_refused() {
    let missing =
        std::env::temp_dir().join(format!("chelabase-nats-r11-missing-{}", Uuid::new_v4()));
    let err = NatsConfig::from_lookup(lookup(&[
        ("CHELABASE_NATS_USER", "worker"),
        ("CHELABASE_NATS_PASSWORD_FILE", missing.to_str().unwrap()),
    ]))
    .unwrap_err();
    assert!(err.contains("CHELABASE_NATS_PASSWORD_FILE"), "{err}");
    let empty = temp_file("empty.password", "\n");
    assert!(
        NatsConfig::from_lookup(lookup(&[
            ("CHELABASE_NATS_USER", "worker"),
            ("CHELABASE_NATS_PASSWORD_FILE", empty.to_str().unwrap()),
        ]))
        .is_err()
    );
}

// --- connect: credentials and the inbox prefix ----------------------------------------------

#[tokio::test]
async fn connect_sends_the_user_and_password() {
    let (url, mut rx) = fake_server().await;
    let _client = connect(&config(
        &url,
        Some("realtime"),
        Some("pw-r"),
        Some("_INBOX_realtime"),
    ))
    .await
    .unwrap();
    let Frame::Connect(info) = next_frame(&mut rx, |f| matches!(f, Frame::Connect(_))).await else {
        unreachable!()
    };
    assert_eq!(info["user"], "realtime", "{info}");
    assert_eq!(info["pass"], "pw-r", "{info}");
}

#[tokio::test]
async fn an_anonymous_config_sends_no_credentials() {
    let (url, mut rx) = fake_server().await;
    let _client = connect(&config(&url, None, None, None)).await.unwrap();
    let Frame::Connect(info) = next_frame(&mut rx, |f| matches!(f, Frame::Connect(_))).await else {
        unreachable!()
    };
    assert!(info.get("user").is_none_or(Value::is_null), "{info}");
    assert!(info.get("pass").is_none_or(Value::is_null), "{info}");
}

// W13: every reply inbox is under the service's own prefix, never the shared `_INBOX`.
#[tokio::test]
async fn connect_uses_the_service_inbox_prefix_for_replies() {
    let (url, mut rx) = fake_server().await;
    let client = connect(&config(
        &url,
        Some("worker"),
        Some("pw-w"),
        Some("_INBOX_worker"),
    ))
    .await
    .unwrap();
    assert!(
        client.new_inbox().starts_with("_INBOX_worker."),
        "{}",
        client.new_inbox()
    );
    // A request subscribes its reply inbox under the prefix too.
    let _pending = tokio::spawn({
        let client = client.clone();
        async move { client.request("chelabase.rpc.review", "{}".into()).await }
    });
    let Frame::Sub(subject) = next_frame(&mut rx, |f| matches!(f, Frame::Sub(_))).await else {
        unreachable!()
    };
    assert!(subject.starts_with("_INBOX_worker."), "{subject}");
}

// --- rt: subjects and payloads (pins; S28) --------------------------------------------------

#[test]
fn the_rt_subjects_and_buckets_are_the_pinned_names() {
    assert_eq!(rt::ACCESS, "chelabase.rt.access");
    assert_eq!(rt::PROJECT, "chelabase.rt.project");
    assert_eq!(rt::COMPUTE_STOPPING, "chelabase.rt.compute.stopping");
    assert_eq!(rt::WATCH, "chelabase.rt.watch");
    assert_eq!(rt::ROWS, "chelabase.rt.rows");
    assert_eq!(rt::PRESENCE, "chelabase.rt.presence");
    assert_eq!(rt::LEASES_BUCKET, "CHELABASE_RT_LEASES");
    assert_eq!(rt::SLOTS_BUCKET, "CHELABASE_RT_SLOTS");
}

fn ids() -> (Uuid, Uuid) {
    (
        Uuid::parse_str("0b9f6c3e-1d2a-4c5b-8e7f-a1b2c3d4e5f6").unwrap(),
        Uuid::parse_str("FFEEDDCC-BBAA-4999-8888-777766665555").unwrap(),
    )
}

#[test]
fn project_events_are_tagged_by_event_with_the_pinned_names() {
    let (d, b) = ids();
    let cases = [
        (
            ProjectEvent::BranchStatus {
                database_id: d,
                branch_id: b,
                status: "ready".into(),
            },
            json!({"event": "branch.status", "database_id": d, "branch_id": b, "status": "ready"}),
        ),
        (
            ProjectEvent::BranchCreated {
                database_id: d,
                branch_id: b,
            },
            json!({"event": "branch.created", "database_id": d, "branch_id": b}),
        ),
        (
            ProjectEvent::BranchDeleted {
                database_id: d,
                branch_id: b,
            },
            json!({"event": "branch.deleted", "database_id": d, "branch_id": b}),
        ),
        (
            ProjectEvent::DatabaseCreated { database_id: d },
            json!({"event": "database.created", "database_id": d}),
        ),
        (
            ProjectEvent::DatabaseDeleted { database_id: d },
            json!({"event": "database.deleted", "database_id": d}),
        ),
        (
            ProjectEvent::PgDatabaseCreated {
                database_id: d,
                branch_id: b,
                name: "app".into(),
            },
            json!({"event": "pg_database.created", "database_id": d, "branch_id": b, "name": "app"}),
        ),
        (
            ProjectEvent::PgDatabaseDropped {
                database_id: d,
                branch_id: b,
                name: "app".into(),
            },
            json!({"event": "pg_database.dropped", "database_id": d, "branch_id": b, "name": "app"}),
        ),
    ];
    for (ev, want) in cases {
        let got = serde_json::to_value(&ev).unwrap();
        assert_eq!(got, want);
        // Round trip: the reader (realtime) parses what the writers send.
        let back: ProjectEvent = serde_json::from_value(want.clone()).unwrap();
        assert_eq!(serde_json::to_value(&back).unwrap(), want);
    }
    // UUIDs go out in the canonical lowercase hyphenated form.
    let text = serde_json::to_string(&ProjectEvent::DatabaseCreated { database_id: b }).unwrap();
    assert!(
        text.contains("ffeeddcc-bbaa-4999-8888-777766665555"),
        "{text}"
    );
}

#[test]
fn an_unknown_project_event_is_refused() {
    let (d, _) = ids();
    assert!(
        serde_json::from_value::<ProjectEvent>(
            json!({"event": "branch.renamed", "database_id": d})
        )
        .is_err()
    );
    assert!(serde_json::from_value::<ProjectEvent>(json!({"database_id": d})).is_err());
}

// S28: a writer that doesn't know the member publishes `member_sub: null` (whole tenant).
#[test]
fn access_events_carry_member_sub_null_for_the_whole_tenant() {
    let (t, _) = ids();
    assert_eq!(
        serde_json::to_value(AccessEvent {
            tenant_id: t,
            member_sub: None
        })
        .unwrap(),
        json!({"tenant_id": t, "member_sub": null})
    );
    assert_eq!(
        serde_json::to_value(AccessEvent {
            tenant_id: t,
            member_sub: Some("u-1".into())
        })
        .unwrap(),
        json!({"tenant_id": t, "member_sub": "u-1"})
    );
    let back: AccessEvent =
        serde_json::from_value(json!({"tenant_id": t, "member_sub": null})).unwrap();
    assert_eq!((back.tenant_id, back.member_sub), (t, None));
}

#[test]
fn compute_stopping_carries_only_the_branch_id() {
    let (_, b) = ids();
    assert_eq!(
        serde_json::to_value(ComputeStopping { branch_id: b }).unwrap(),
        json!({"branch_id": b})
    );
}

async fn published(rx: &mut mpsc::UnboundedReceiver<Frame>) -> (String, Value) {
    let Frame::Pub { subject, payload } = next_frame(rx, |f| matches!(f, Frame::Pub { .. })).await
    else {
        unreachable!()
    };
    (subject, serde_json::from_slice(&payload).unwrap())
}

#[tokio::test]
async fn publish_project_goes_to_the_project_subject() {
    let (url, mut rx) = fake_server().await;
    let client = connect(&config(&url, None, None, None)).await.unwrap();
    let (d, b) = ids();
    let project = Uuid::parse_str("ABCDEF01-2345-4678-9ABC-DEF012345678").unwrap();
    let ev = ProjectEvent::BranchCreated {
        database_id: d,
        branch_id: b,
    };
    let result: Result<(), chelabase_nats::NatsError> =
        rt::publish_project(&client, project, &ev).await;
    result.unwrap();
    let (subject, body) = published(&mut rx).await;
    assert_eq!(
        subject,
        "chelabase.rt.project.abcdef01-2345-4678-9abc-def012345678"
    );
    assert_eq!(body, serde_json::to_value(&ev).unwrap());
}

#[tokio::test]
async fn publish_access_goes_to_the_tenant_subject() {
    let (url, mut rx) = fake_server().await;
    let client = connect(&config(&url, None, None, None)).await.unwrap();
    let (t, _) = ids();
    let ev = AccessEvent {
        tenant_id: t,
        member_sub: Some("sub-9".into()),
    };
    rt::publish_access(&client, &ev).await.unwrap();
    let (subject, body) = published(&mut rx).await;
    assert_eq!(subject, format!("chelabase.rt.access.{t}"));
    assert_eq!(body, json!({"tenant_id": t, "member_sub": "sub-9"}));
}

#[tokio::test]
async fn publish_compute_stopping_goes_to_the_branch_subject() {
    let (url, mut rx) = fake_server().await;
    let client = connect(&config(&url, None, None, None)).await.unwrap();
    let (_, b) = ids();
    rt::publish_compute_stopping(&client, b).await.unwrap();
    let (subject, body) = published(&mut rx).await;
    assert_eq!(
        subject,
        "chelabase.rt.compute.stopping.ffeeddcc-bbaa-4999-8888-777766665555"
    );
    assert_eq!(body, json!({"branch_id": b}));
}
