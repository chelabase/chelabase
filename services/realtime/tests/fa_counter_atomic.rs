//! RT1 final fix round, batch A (T7 hardening): `ValkeyStore::incr` sends `INCR`, then
//! `EXPIRE` when the count is 1, as two commands. A connection that drops (or a command that
//! times out, or an instance that dies) between them leaves `rt:cnt:*` with a count and no TTL:
//! the key never goes, and a socket counter that never expires keeps refusing the member.
//!
//! Pinned: the count and its TTL are set in one `MULTI`/`EXEC` transaction (`INCR key` and
//! `EXPIRE key ttl NX`, or `EXPIRE` when the count is 1, inside it), so either both apply or
//! neither does. Realtime's and the runner's Valkey ACL users gain `+multi +exec` (see
//! `tools/cli/src/workbench/r11_tests.rs`).
//!
//! The Valkey here is a fake RESP2 server with the few commands the store sends; it can drop
//! the connection right after answering an `INCR` sent outside a transaction.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use realtime::store::{CounterStore, ValkeyConfig, ValkeyStore};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

#[derive(Default, Debug, Clone)]
struct Key {
    count: i64,
    ttl_secs: Option<i64>,
}

#[derive(Default)]
struct Fake {
    keys: Mutex<HashMap<String, Key>>,
    /// Close the connection right after answering an `INCR` sent outside `MULTI`.
    drop_after_bare_incr: bool,
    /// Every command, upper-cased name first, as received (`EXEC` included).
    log: Mutex<Vec<Vec<String>>>,
}

enum Reply {
    Simple(&'static str),
    Int(i64),
    Err(String),
    Array(Vec<Reply>),
}

impl Reply {
    fn encode(&self, out: &mut Vec<u8>) {
        match self {
            Reply::Simple(s) => out.extend_from_slice(format!("+{s}\r\n").as_bytes()),
            Reply::Int(n) => out.extend_from_slice(format!(":{n}\r\n").as_bytes()),
            Reply::Err(e) => out.extend_from_slice(format!("-{e}\r\n").as_bytes()),
            Reply::Array(items) => {
                out.extend_from_slice(format!("*{}\r\n", items.len()).as_bytes());
                for i in items {
                    i.encode(out);
                }
            }
        }
    }
}

impl Fake {
    fn apply(&self, cmd: &[String]) -> Reply {
        let mut keys = self.keys.lock().unwrap();
        match cmd[0].as_str() {
            "INCR" => {
                let k = keys.entry(cmd[1].clone()).or_default();
                k.count += 1;
                Reply::Int(k.count)
            }
            "EXPIRE" => {
                let secs: i64 = cmd[2].parse().unwrap();
                let nx = cmd.get(3).is_some_and(|f| f.eq_ignore_ascii_case("NX"));
                match keys.get_mut(&cmd[1]) {
                    None => Reply::Int(0),
                    Some(k) if nx && k.ttl_secs.is_some() => Reply::Int(0),
                    Some(k) => {
                        k.ttl_secs = Some(secs);
                        Reply::Int(1)
                    }
                }
            }
            "PING" => Reply::Simple("PONG"),
            "CLIENT" => Reply::Simple("OK"),
            other => Reply::Err(format!("ERR unknown command '{other}' (the fake)")),
        }
    }
}

/// One RESP2 command (`*n` of bulk strings) from the front of `buf`, if complete.
fn parse(buf: &[u8]) -> Option<(Vec<String>, usize)> {
    fn line(buf: &[u8], at: usize) -> Option<(&[u8], usize)> {
        let end = buf[at..].windows(2).position(|w| w == b"\r\n")? + at;
        Some((&buf[at..end], end + 2))
    }
    let (head, mut at) = line(buf, 0)?;
    assert_eq!(head.first(), Some(&b'*'), "the fake takes arrays only");
    let n: usize = std::str::from_utf8(&head[1..]).unwrap().parse().unwrap();
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        let (len, next) = line(buf, at)?;
        assert_eq!(len.first(), Some(&b'$'));
        let len: usize = std::str::from_utf8(&len[1..]).unwrap().parse().unwrap();
        if buf.len() < next + len + 2 {
            return None;
        }
        out.push(String::from_utf8_lossy(&buf[next..next + len]).into_owned());
        at = next + len + 2;
    }
    out[0] = out[0].to_ascii_uppercase();
    Some((out, at))
}

async fn serve(fake: Arc<Fake>, mut socket: TcpStream) {
    let mut buf = Vec::new();
    let mut queued: Option<Vec<Vec<String>>> = None;
    let mut chunk = [0u8; 4096];
    loop {
        while let Some((cmd, used)) = parse(&buf) {
            buf.drain(..used);
            fake.log.lock().unwrap().push(cmd.clone());
            let mut out = Vec::new();
            let mut close = false;
            match (cmd[0].as_str(), queued.as_mut()) {
                ("MULTI", None) => {
                    queued = Some(Vec::new());
                    Reply::Simple("OK").encode(&mut out);
                }
                ("EXEC", Some(_)) => {
                    let cmds = queued.take().unwrap();
                    // All of them under one lock acquisition each, with nothing in between.
                    Reply::Array(cmds.iter().map(|c| fake.apply(c)).collect()).encode(&mut out);
                }
                ("DISCARD", Some(_)) => {
                    queued = None;
                    Reply::Simple("OK").encode(&mut out);
                }
                (_, Some(q)) => {
                    q.push(cmd.clone());
                    Reply::Simple("QUEUED").encode(&mut out);
                }
                (name, None) => {
                    fake.apply(&cmd).encode(&mut out);
                    close = name == "INCR" && fake.drop_after_bare_incr;
                }
            }
            if socket.write_all(&out).await.is_err() {
                return;
            }
            if close {
                // The instance died, or the connection broke, right after the INCR.
                let _ = socket.shutdown().await;
                return;
            }
        }
        match socket.read(&mut chunk).await {
            Ok(0) | Err(_) => return,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    }
}

async fn start(drop_after_bare_incr: bool) -> (Arc<Fake>, ValkeyStore) {
    let fake = Arc::new(Fake {
        drop_after_bare_incr,
        ..Fake::default()
    });
    let listener = TcpListener::bind(("localhost", 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let f = fake.clone();
    tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            tokio::spawn(serve(f.clone(), socket));
        }
    });
    let store = ValkeyStore::connect(&ValkeyConfig {
        url: format!("redis://{addr}"),
        user: None,
        password: None,
        tls: false,
    })
    .await
    .expect("the store connects to the fake");
    (fake, store)
}

#[tokio::test]
async fn a_counter_never_outlives_its_ttl_when_the_connection_drops_after_incr() {
    let (fake, store) = start(true).await;
    let key = "rt:cnt:tix:member-1:1760000000";
    // Whatever the call answers (it may fail: the connection broke), the key must not be left
    // counting with no TTL.
    let _ = store.incr(key, Duration::from_secs(2)).await;
    let state = fake.keys.lock().unwrap().get(key).cloned();
    if let Some(k) = state {
        assert!(
            k.ttl_secs.is_some(),
            "{key} was left with count {} and no TTL (INCR and EXPIRE are separate commands); \
             commands: {:?}",
            k.count,
            fake.log.lock().unwrap()
        );
    }
}

#[tokio::test]
async fn the_count_and_its_ttl_go_in_one_transaction() {
    let (fake, store) = start(false).await;
    let key = "rt:cnt:tix:member-2:1760000000";
    assert_eq!(store.incr(key, Duration::from_secs(2)).await.unwrap(), 1);
    let log = fake.log.lock().unwrap().clone();
    let incr = log
        .iter()
        .position(|c| c[0] == "INCR")
        .unwrap_or_else(|| panic!("no INCR sent: {log:?}"));
    let multi = log[..incr].iter().rposition(|c| c[0] == "MULTI");
    let exec = log[incr..]
        .iter()
        .position(|c| c[0] == "EXEC")
        .map(|i| i + incr);
    let expire = log[incr..]
        .iter()
        .position(|c| c[0] == "EXPIRE")
        .map(|i| i + incr);
    assert!(
        multi.is_some() && exec.is_some() && expire.is_some_and(|e| e < exec.unwrap()),
        "INCR and EXPIRE must be queued in one MULTI/EXEC: {log:?}"
    );
    assert_eq!(fake.keys.lock().unwrap()[key].ttl_secs, Some(2));
}

// Guard: the window's TTL is set when the key is created and the count keeps growing in it.
#[tokio::test]
async fn later_increments_count_up_and_keep_a_ttl() {
    let (fake, store) = start(false).await;
    let key = "rt:cnt:tix:member-3:1760000000";
    for want in 1..=3 {
        assert_eq!(
            store.incr(key, Duration::from_millis(1500)).await.unwrap(),
            want
        );
    }
    let k = fake.keys.lock().unwrap()[key].clone();
    assert_eq!(k.count, 3);
    // A sub-second remainder rounds up (never 0).
    assert_eq!(k.ttl_secs, Some(2));
}
