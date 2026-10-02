use axum::Router;
use axum::body::Bytes;
use axum::http::{HeaderMap, Method, StatusCode};
use axum::routing::any;
use cheladb_hook_relay::{RelayConfig, app, read_slots};
use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
struct Seen {
    method: Method,
    path: String,
    auth: Option<String>,
    custom: Option<String>,
    body: Vec<u8>,
}

/// A fake slot internal listener: answers `status` to everything and records what it saw.
async fn upstream(status: u16, hits: Arc<Mutex<Vec<Seen>>>) -> u16 {
    let router = Router::new().fallback(any(
        move |method: Method, uri: axum::http::Uri, h: HeaderMap, body: Bytes| {
            let hits = hits.clone();
            async move {
                hits.lock().unwrap().push(Seen {
                    method,
                    path: uri.path().to_string(),
                    auth: h.get("authorization").map(|v| v.to_str().unwrap().into()),
                    custom: h.get("x-custom").map(|v| v.to_str().unwrap().into()),
                    body: body.to_vec(),
                });
                (
                    StatusCode::from_u16(status).unwrap(),
                    [("x-from", "slot")],
                    "reply",
                )
            }
        },
    ));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(l, router).await.unwrap() });
    port
}

fn slot_file(dir: &tempfile::TempDir, n: u32, port: u16) {
    let mut f = std::fs::File::create(dir.path().join(format!("{n}.env"))).unwrap();
    writeln!(
        f,
        "# comment\nCHELADB_SLOT={n}\nCHELADB_INTERNAL_PORT={port}\nOTHER=x"
    )
    .unwrap();
}

fn cfg(dir: &tempfile::TempDir) -> RelayConfig {
    RelayConfig {
        slots_dir: dir.path().into(),
        upstream_host: "127.0.0.1".into(),
        slot0_port: None,
        slot_timeout: Duration::from_secs(5),
        deadline: Duration::from_secs(10),
    }
}

async fn serve(cfg: RelayConfig) -> String {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app(cfg)).await.unwrap() });
    format!("http://{addr}")
}

fn hits() -> Arc<Mutex<Vec<Seen>>> {
    Arc::default()
}

#[tokio::test]
async fn forwards_to_the_slot_that_knows_the_tenant() {
    let (h1, h2) = (hits(), hits());
    let p1 = upstream(404, h1.clone()).await;
    let p2 = upstream(200, h2.clone()).await;
    let dir = tempfile::tempdir().unwrap();
    slot_file(&dir, 1, p1);
    slot_file(&dir, 2, p2);
    let base = serve(cfg(&dir)).await;
    let r = reqwest::Client::new()
        .put(format!("{base}/storcon/notify-attach"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["x-from"], "slot");
    assert_eq!(r.text().await.unwrap(), "reply");
    assert_eq!(h1.lock().unwrap().len(), 1, "slot 1 asked first, said 404");
    assert_eq!(h2.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn all_404_is_404() {
    let p1 = upstream(404, hits()).await;
    let p2 = upstream(404, hits()).await;
    let dir = tempfile::tempdir().unwrap();
    slot_file(&dir, 1, p1);
    slot_file(&dir, 2, p2);
    let base = serve(cfg(&dir)).await;
    let r = reqwest::Client::new()
        .put(format!("{base}/storcon/notify-attach"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
    // No slots at all is 404 too.
    let empty = tempfile::tempdir().unwrap();
    let base = serve(cfg(&empty)).await;
    let r = reqwest::Client::new()
        .put(format!("{base}/storcon/x"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
}

#[tokio::test]
async fn an_unreachable_slot_is_skipped() {
    let dead = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let h = hits();
    let p = upstream(200, h.clone()).await;
    let dir = tempfile::tempdir().unwrap();
    slot_file(&dir, 1, dead);
    slot_file(&dir, 2, p);
    let base = serve(cfg(&dir)).await;
    let r = reqwest::Client::new()
        .put(format!("{base}/storcon/notify-attach"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(h.lock().unwrap().len(), 1);
    // Only an unreachable slot: 404.
    let only = tempfile::tempdir().unwrap();
    slot_file(&only, 1, dead);
    let base = serve(cfg(&only)).await;
    let r = reqwest::Client::new()
        .put(format!("{base}/storcon/notify-attach"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
}

#[tokio::test]
async fn headers_body_and_method_are_forwarded() {
    let h = hits();
    let p = upstream(200, h.clone()).await;
    let dir = tempfile::tempdir().unwrap();
    slot_file(&dir, 1, p);
    let base = serve(cfg(&dir)).await;
    reqwest::Client::new()
        .put(format!("{base}/storcon/notify-safekeepers?a=1"))
        .bearer_auth("tok")
        .header("x-custom", "v")
        .body(r#"{"tenant_id":"t"}"#)
        .send()
        .await
        .unwrap();
    let seen = h.lock().unwrap()[0].clone();
    assert_eq!(seen.method, Method::PUT);
    assert_eq!(seen.path, "/storcon/notify-safekeepers");
    assert_eq!(seen.auth.as_deref(), Some("Bearer tok"));
    assert_eq!(seen.custom.as_deref(), Some("v"));
    assert_eq!(seen.body, br#"{"tenant_id":"t"}"#);
}

#[tokio::test]
async fn a_404_slot_gets_the_same_body_as_the_next_one() {
    let (h1, h2) = (hits(), hits());
    let p1 = upstream(404, h1.clone()).await;
    let p2 = upstream(200, h2.clone()).await;
    let dir = tempfile::tempdir().unwrap();
    slot_file(&dir, 1, p1);
    slot_file(&dir, 2, p2);
    let base = serve(cfg(&dir)).await;
    reqwest::Client::new()
        .post(format!("{base}/storcon/x"))
        .body("abc")
        .send()
        .await
        .unwrap();
    assert_eq!(h1.lock().unwrap()[0].body, b"abc");
    assert_eq!(h2.lock().unwrap()[0].body, b"abc");
}

#[tokio::test]
async fn slot_order_is_stable() {
    let dir = tempfile::tempdir().unwrap();
    // Created out of order, and 10 sorts after 2 numerically (not lexically).
    slot_file(&dir, 10, 8100);
    slot_file(&dir, 2, 8200);
    slot_file(&dir, 1, 8300);
    std::fs::write(dir.path().join("notes.txt"), "CHELADB_SLOT=7\n").unwrap();
    std::fs::write(dir.path().join("bad.env"), "garbage").unwrap();
    let mut c = cfg(&dir);
    c.slot0_port = Some(8083);
    let got: Vec<(u32, u16)> = read_slots(&c).iter().map(|s| (s.slot, s.port)).collect();
    assert_eq!(got, vec![(0, 8083), (1, 8300), (2, 8200), (10, 8100)]);
    assert_eq!(
        got,
        read_slots(&c)
            .iter()
            .map(|s| (s.slot, s.port))
            .collect::<Vec<_>>()
    );
    // A registry file for slot 0 wins over the implicit one; a missing port derives from the slot.
    std::fs::write(
        dir.path().join("0.env"),
        "CHELADB_SLOT=0\nCHELADB_INTERNAL_PORT=9999\n",
    )
    .unwrap();
    std::fs::write(dir.path().join("3.env"), "CHELADB_SLOT=3\n").unwrap();
    let got: Vec<(u32, u16)> = read_slots(&c).iter().map(|s| (s.slot, s.port)).collect();
    assert_eq!(
        got,
        vec![(0, 9999), (1, 8300), (2, 8200), (3, 8383), (10, 8100)]
    );
}

#[derive(Clone, Default)]
struct Buf(Arc<Mutex<Vec<u8>>>);
impl Write for Buf {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Buf {
    type Writer = Buf;
    fn make_writer(&'a self) -> Buf {
        self.clone()
    }
}

// Relies on the single-threaded `#[tokio::test]` runtime: the relay's tasks run on this
// thread, so the thread-local `set_default` subscriber sees their events. Do not switch
// this test to `flavor = "multi_thread"`.
#[tokio::test]
async fn bodies_are_never_logged() {
    let buf = Buf::default();
    let sub = tracing_subscriber::fmt()
        .with_writer(buf.clone())
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .finish();
    let _g = tracing::subscriber::set_default(sub);
    let p1 = upstream(404, hits()).await;
    let p2 = upstream(200, hits()).await;
    let dir = tempfile::tempdir().unwrap();
    slot_file(&dir, 1, p1);
    slot_file(&dir, 2, p2);
    let base = serve(cfg(&dir)).await;
    reqwest::Client::new()
        .put(format!("{base}/storcon/notify-attach"))
        .bearer_auth("SECRET-TOKEN")
        .body("SECRET-BODY")
        .send()
        .await
        .unwrap();
    let logs = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
    assert!(
        logs.contains("/storcon/notify-attach"),
        "relay should log the request: {logs:?}"
    );
    assert!(!logs.contains("SECRET-BODY"), "{logs}");
    assert!(!logs.contains("SECRET-TOKEN"), "{logs}");
}

/// Accepts connections and never answers.
async fn hung_upstream() -> u16 {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((s, _)) = l.accept().await {
            held.push(s);
        }
    });
    port
}

#[tokio::test]
async fn a_hung_slot_answers_504_and_is_not_skipped() {
    let hung = hung_upstream().await;
    let h = hits();
    let good = upstream(200, h.clone()).await;
    let dir = tempfile::tempdir().unwrap();
    slot_file(&dir, 1, hung);
    slot_file(&dir, 2, good);
    let mut c = cfg(&dir);
    c.slot_timeout = Duration::from_millis(300);
    c.deadline = Duration::from_secs(5);
    let base = serve(c.clone()).await;
    let t = Instant::now();
    let r = reqwest::Client::new()
        .put(format!("{base}/storcon/notify-attach"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        r.status(),
        504,
        "a slot that accepted the request and timed out may own the tenant: the controller retries"
    );
    assert!(t.elapsed() < Duration::from_secs(3), "{:?}", t.elapsed());
    assert_eq!(h.lock().unwrap().len(), 0, "the next slot is not asked");

    // Deadline shorter than the per-slot timeout: 504, promptly.
    c.slot_timeout = Duration::from_secs(5);
    c.deadline = Duration::from_millis(300);
    let base = serve(c).await;
    let t = Instant::now();
    let r = reqwest::Client::new()
        .put(format!("{base}/storcon/notify-attach"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 504);
    assert!(t.elapsed() < Duration::from_secs(3), "{:?}", t.elapsed());
}

#[tokio::test]
async fn only_storcon_paths_are_forwarded() {
    let h = hits();
    let p = upstream(200, h.clone()).await;
    let dir = tempfile::tempdir().unwrap();
    slot_file(&dir, 1, p);
    let base = serve(cfg(&dir)).await;
    for path in ["/", "/admin/secret", "/storcon", "/storconx/notify-attach"] {
        let r = reqwest::Client::new()
            .put(format!("{base}{path}"))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 404, "{path}");
    }
    assert_eq!(h.lock().unwrap().len(), 0, "no slot was contacted");
    let r = reqwest::Client::new()
        .put(format!("{base}/storcon/notify-attach"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
}

#[test]
fn out_of_range_ports_are_skipped() {
    let dir = tempfile::tempdir().unwrap();
    slot_file(&dir, 1, 8300);
    slot_file(&dir, 2, 80); // privileged
    slot_file(&dir, 3, 0);
    std::fs::write(dir.path().join("700.env"), "CHELADB_SLOT=700\n").unwrap(); // 8083+70000 overflows
    let got: Vec<u32> = read_slots(&cfg(&dir)).iter().map(|s| s.slot).collect();
    assert_eq!(got, vec![1]);
}

/// The relay image has no CA bundle, and the relay only speaks plain HTTP to the slots, so
/// building the client must not need the system's trust store.
#[test]
fn the_client_needs_no_system_ca_certificates() {
    let dir = tempfile::tempdir().unwrap();
    let empty_file = dir.path().join("no-certs.pem");
    std::fs::write(&empty_file, b"").unwrap();
    let empty_dir = dir.path().join("no-certs");
    std::fs::create_dir(&empty_dir).unwrap();
    // nextest runs each test in its own process, so this doesn't leak into other tests.
    unsafe {
        std::env::set_var("SSL_CERT_FILE", &empty_file);
        std::env::set_var("SSL_CERT_DIR", &empty_dir);
    }
    let _ = app(cfg(&dir));
}
