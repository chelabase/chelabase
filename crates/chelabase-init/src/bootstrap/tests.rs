//! The bootstrap against fakes: a metadata database and orchestrator probe that record one
//! shared event log, and a stand-in for auth's sign-up. Nothing here reaches a real service.

use super::dev_member::{DevMember, DevMemberConfig};
use super::outputs;
use super::retry::{Backoff, Check};
use super::*;
use anyhow::Result;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

type Log = Arc<Mutex<Vec<String>>>;

fn push(log: &Log, event: impl Into<String>) {
    log.lock().unwrap().push(event.into());
}

struct FakeMetadata {
    log: Log,
    /// `migrated` answers "not yet" this many times first.
    unmigrated: AtomicU32,
}

impl Metadata for FakeMetadata {
    async fn migrated(&self) -> Result<Check<()>> {
        push(&self.log, "metadata migrated?");
        let left = self.unmigrated.load(Ordering::SeqCst);
        if left > 0 {
            self.unmigrated.store(left - 1, Ordering::SeqCst);
            return Ok(Check::Wait("migrations left".into()));
        }
        Ok(Check::Ready(()))
    }
}

/// The orchestrator: unreachable for `down` probes, then up.
struct FakeOrchestrator {
    log: Log,
    down: AtomicU32,
}

impl Probe for FakeOrchestrator {
    fn name(&self) -> String {
        "the orchestrator at http://orchestrator:8083/health".into()
    }
    async fn check(&self) -> Result<Check<()>> {
        push(&self.log, "orchestrator ready?");
        let left = self.down.load(Ordering::SeqCst);
        if left > 0 {
            self.down.store(left - 1, Ordering::SeqCst);
            return Ok(Check::Wait("connection refused".into()));
        }
        Ok(Check::Ready(()))
    }
}

fn private_dir() -> tempfile::TempDir {
    let t = tempfile::tempdir().unwrap();
    std::fs::set_permissions(t.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    t
}

fn config(out: &Path) -> Config {
    Config {
        out_dir: out.to_path_buf(),
        studio_origin: "http://studio.chelabase.localhost:8000".into(),
        mode: chelabase_types::mode::Mode::SelfHosted,
        metadata_url: "postgres://unused".into(),
        orchestrator_health: None,
        auth_url: "http://unused".into(),
        setup_code_file: None,
        admin_email: None,
        timeout: Duration::from_secs(10),
        backoff: Backoff { first: Duration::from_millis(5), max: Duration::from_millis(20) },
    }
}

// The run waits for the migrations, then the orchestrator, sharing one deadline.
#[tokio::test]
async fn retries_until_migrated_and_orchestrator_ready() {
    let out = private_dir();
    let log: Log = Arc::default();
    let metadata = FakeMetadata { log: log.clone(), unmigrated: AtomicU32::new(2) };
    let orchestrator = FakeOrchestrator { log: log.clone(), down: AtomicU32::new(2) };
    let locked = run_with(&config(out.path()), &metadata, &orchestrator).await.unwrap();
    assert_eq!(locked.outputs.path(), out.path());
    assert_eq!(
        *log.lock().unwrap(),
        [
            "metadata migrated?",
            "metadata migrated?",
            "metadata migrated?",
            "orchestrator ready?",
            "orchestrator ready?",
            "orchestrator ready?",
        ]
    );
    // Nothing is written without `--demo`: only the lock.
    let names: Vec<String> = std::fs::read_dir(out.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(names, [outputs::LOCK]);
}

#[tokio::test]
async fn deadline_names_the_dependency() {
    let out = private_dir();
    let log: Log = Arc::default();
    let metadata = FakeMetadata { log: log.clone(), unmigrated: AtomicU32::new(0) };
    let orchestrator = FakeOrchestrator { log, down: AtomicU32::new(u32::MAX) };
    let mut cfg = config(out.path());
    cfg.timeout = Duration::from_millis(100);
    let err = format!("{:#}", run_with(&cfg, &metadata, &orchestrator).await.err().unwrap());
    assert!(err.contains("the orchestrator") && err.contains("connection refused"), "{err}");
}

// N3: a second run behind a held output-dir lock waits only until the deadline, naming it.
#[tokio::test]
async fn the_output_lock_waits_only_until_the_deadline() {
    let out = private_dir();
    let log: Log = Arc::default();
    let metadata = FakeMetadata { log: log.clone(), unmigrated: AtomicU32::new(0) };
    let orchestrator = FakeOrchestrator { log, down: AtomicU32::new(0) };
    let mut cfg = config(out.path());
    cfg.timeout = Duration::from_millis(200);
    let held = std::fs::File::create(out.path().join(outputs::LOCK)).unwrap();
    held.lock().unwrap();
    let result = tokio::time::timeout(Duration::from_secs(5), run_with(&cfg, &metadata, &orchestrator)).await;
    let err = format!("{:#}", result.expect("bounded by --timeout").err().unwrap());
    assert!(err.contains(outputs::LOCK) && err.contains("another bootstrap"), "{err}");
}

// A symlink at an output name fails the run before anything is written.
#[tokio::test]
async fn outputs_refuse_symlinks() {
    let out = private_dir();
    let elsewhere = private_dir();
    std::os::unix::fs::symlink(elsewhere.path().join("x"), out.path().join(outputs::DEV_MEMBER)).unwrap();
    let log: Log = Arc::default();
    let metadata = FakeMetadata { log: log.clone(), unmigrated: AtomicU32::new(0) };
    let orchestrator = FakeOrchestrator { log: log.clone(), down: AtomicU32::new(0) };
    let err = format!("{:#}", run_with(&config(out.path()), &metadata, &orchestrator).await.err().unwrap());
    assert!(err.contains("symlink"), "{err}");
    assert!(log.lock().unwrap().is_empty(), "nothing waited on");
}

// The metadata password comes from its file into the URL's user info, percent-encoded, through
// the shared helper (so it has its password-query, symlink and mode checks).
fn metadata_config(url: &str, pw: &Path) -> Result<Config> {
    let env: HashMap<String, String> = [
        ("CHELABASE_STUDIO_HOST", "studio.example.com".to_string()),
        ("CHELABASE_API_HOST", "api.example.com".to_string()),
        ("CHELABASE_AUTH_HOST", "auth.example.com".to_string()),
        ("CHELABASE_METADATA_DATABASE_URL", url.to_string()),
        ("CHELABASE_METADATA_DATABASE_PASSWORD_FILE", pw.display().to_string()),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect();
    Config::from_lookup(move |k| env.get(k).cloned(), PathBuf::from("/out"))
}

fn password_file(dir: &Path, mode: u32, text: &str) -> PathBuf {
    let path = dir.join("metadata.password");
    std::fs::write(&path, text).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
    path
}

#[test]
fn the_metadata_password_file_fills_the_url() {
    let t = tempfile::tempdir().unwrap();
    // 0640: what self-host's layout gives it (owner and the postgres group).
    let pw = password_file(t.path(), 0o640, "p@ss/w:rd\n");
    let cfg = metadata_config("postgres://chelabase@metadata-db:5432/m?sslmode=disable", &pw).unwrap();
    assert_eq!(
        cfg.metadata_url,
        "postgres://chelabase:p%40ss%2Fw%3Ard@metadata-db:5432/m?sslmode=disable"
    );
    assert!(metadata_config("postgres://chelabase:x@db/m", &pw).is_err());
    assert!(metadata_config("postgres://db/m", &pw).is_err());
}

// m5's checks on the shared helper reach the bootstrap too: a `password=` query beside the
// file, a symlinked file and a world-readable file are all refused, never echoing the password.
#[test]
fn a_password_query_beside_the_metadata_password_file_is_refused() {
    let t = tempfile::tempdir().unwrap();
    let pw = password_file(t.path(), 0o640, "hunter2-secret\n");
    let err = format!(
        "{:#}",
        metadata_config("postgres://chelabase@db/m?password=other", &pw).err().unwrap()
    );
    assert!(err.contains("carries a password"), "{err}");
}

#[test]
fn a_symlinked_metadata_password_file_is_refused() {
    let t = tempfile::tempdir().unwrap();
    let pw = password_file(t.path(), 0o640, "hunter2-secret\n");
    let link = t.path().join("link.password");
    std::os::unix::fs::symlink(&pw, &link).unwrap();
    let err = format!("{:#}", metadata_config("postgres://chelabase@db/m", &link).err().unwrap());
    assert!(err.contains("symlink"), "{err}");
}

#[test]
fn a_world_readable_metadata_password_file_is_refused() {
    let t = tempfile::tempdir().unwrap();
    let pw = password_file(t.path(), 0o644, "hunter2-secret\n");
    let err = format!("{:#}", metadata_config("postgres://chelabase@db/m", &pw).err().unwrap());
    assert!(err.contains("CHELABASE_METADATA_DATABASE_PASSWORD_FILE"), "{err}");
    assert!(!err.contains("hunter2"), "{err}");
}

fn demo_env(code_file: &Path, extra: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
    let mut env: HashMap<String, String> = [
        ("CHELABASE_PUBLIC_SCHEME", "http"),
        ("CHELABASE_STUDIO_HOST", "studio.chelabase.localhost:8000"),
        ("CHELABASE_API_HOST", "api.chelabase.localhost:8000"),
        ("CHELABASE_AUTH_HOST", "auth.chelabase.localhost:8000"),
        ("CHELABASE_METADATA_DATABASE_URL", "postgres://chelabase:pw@metadata-db/m"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    env.insert("CHELABASE_SETUP_CODE_FILE".into(), code_file.display().to_string());
    for (k, v) in extra {
        env.insert(k.to_string(), v.to_string());
    }
    move |k| env.get(k).cloned()
}

fn code_file(dir: &Path) -> std::path::PathBuf {
    let path = dir.join("setup.code");
    std::fs::write(&path, "ABCD-EFGH-JKMN-PQRS-TUVW\n").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    path
}

// `--demo`: the dev member signs up at auth (`CHELABASE_AUTH_URL`, `http://auth:8084` unless
// set) with the stack's setup code, as Studio; the email is `--admin-email`, lowercased, or
// the default.
#[test]
fn demo_reads_the_setup_code_and_the_email() {
    let t = private_dir();
    let file = code_file(t.path());
    let mut cfg = Config::from_lookup(demo_env(&file, &[]), t.path().to_path_buf()).unwrap();
    let dev = cfg.dev_member().unwrap();
    assert_eq!(
        (dev.auth_url.as_str(), dev.studio_origin.as_str(), dev.setup_code.as_str(), dev.email.as_str()),
        ("http://auth:8084", "http://studio.chelabase.localhost:8000", "ABCD-EFGH-JKMN-PQRS-TUVW", dev_member::DEFAULT_EMAIL)
    );
    cfg.admin_email = Some(" Ana@Example.COM ".into());
    assert_eq!(cfg.dev_member().unwrap().email, "ana@example.com");
    let cfg = Config::from_lookup(demo_env(&file, &[("CHELABASE_AUTH_URL", "http://a:1/")]), t.path().to_path_buf()).unwrap();
    assert_eq!(cfg.dev_member().unwrap().auth_url, "http://a:1");
    // Without the code file, or in platform mode, `--demo` refuses.
    let mut none = Config::from_lookup(demo_env(&file, &[]), t.path().to_path_buf()).unwrap();
    none.setup_code_file = None;
    assert!(format!("{:#}", none.dev_member().err().unwrap()).contains("CHELABASE_SETUP_CODE_FILE"));
    let mut platform = Config::from_lookup(demo_env(&file, &[]), t.path().to_path_buf()).unwrap();
    platform.mode = chelabase_types::mode::Mode::Platform;
    assert!(platform.dev_member().is_err());
    // A world-readable code file is refused, never echoed.
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
    let err = format!("{:#}", Config::from_lookup(demo_env(&file, &[]), t.path().to_path_buf()).unwrap().dev_member().err().unwrap());
    assert!(!err.contains("ABCD"), "{err}");
}

/// A stand-in for auth: `/health` answers 200, `/auth/v1/sign-up` records each request (its
/// headers and body) and answers `reply`'s next answer (the last one repeats).
struct FakeAuth {
    url: String,
    seen: Arc<Mutex<Vec<(HashMap<String, String>, Value)>>>,
}

async fn fake_auth(replies: Vec<(u16, Value)>) -> FakeAuth {
    use axum::routing::{get, post};
    let seen: Arc<Mutex<Vec<(HashMap<String, String>, Value)>>> = Arc::default();
    let replies = Arc::new(Mutex::new(replies));
    let s = seen.clone();
    let app = axum::Router::new()
        .route("/health", get(|| async { "ok" }))
        .route(
            "/auth/v1/sign-up",
            post(move |headers: axum::http::HeaderMap, axum::Json(body): axum::Json<Value>| {
                let (s, replies) = (s.clone(), replies.clone());
                async move {
                    let h = headers
                        .iter()
                        .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
                        .collect();
                    s.lock().unwrap().push((h, body));
                    let mut r = replies.lock().unwrap();
                    let (status, answer) = if r.len() > 1 { r.remove(0) } else { r[0].clone() };
                    (axum::http::StatusCode::from_u16(status).unwrap(), axum::Json(answer))
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    FakeAuth { url, seen }
}

fn dev_cfg(auth: &FakeAuth) -> DevMemberConfig {
    DevMemberConfig {
        auth_url: auth.url.clone(),
        studio_origin: "http://studio.chelabase.localhost:8000".into(),
        setup_code: "ABCD-EFGH-JKMN-PQRS-TUVW".into(),
        email: "dev@chelabase.localhost".into(),
    }
}

fn waiter() -> retry::Waiter {
    retry::Waiter::new(Duration::from_secs(10), Backoff { first: Duration::from_millis(5), max: Duration::from_millis(20) })
}

// #126, Gap 2: the dev member signs up once, as Studio, with the setup code; its password is
// random, passes the rule and is kept owner-only; a rerun asks auth nothing.
#[tokio::test]
async fn the_dev_member_signs_up_once_with_the_setup_code() {
    let out_dir = private_dir();
    let out = outputs::Outputs::open(out_dir.path()).unwrap();
    // auth isn't answering sign-ups yet (503), then it is.
    let auth = fake_auth(vec![(503, json!({"error": {"code": "unavailable"}})), (200, json!({"next": "studio"}))]).await;
    let member = dev_member::ensure(&waiter(), &out, &dev_cfg(&auth)).await.unwrap();
    let seen = auth.seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 2, "retried past the 503");
    let (headers, body) = &seen[1];
    assert_eq!(headers["origin"], "http://studio.chelabase.localhost:8000");
    assert_eq!(headers["x-chelabase-studio"], "1");
    assert_eq!(
        *body,
        json!({"email": "dev@chelabase.localhost", "password": member.password, "givenName": "Dev",
            "familyName": "Member", "setupCode": "ABCD-EFGH-JKMN-PQRS-TUVW"})
    );
    assert!(chelabase_identity::rules::strong_password(&member.password));
    let path = out_dir.path().join(outputs::DEV_MEMBER);
    assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    let stored: DevMember = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(stored, member);
    let again = dev_member::ensure(&waiter(), &out, &dev_cfg(&auth)).await.unwrap();
    assert_eq!(again, member);
    assert_eq!(auth.seen.lock().unwrap().len(), 2, "a rerun asks nothing");
}

// A tenant already claimed (a pending answer, closed sign-up, a wrong code, a taken email) or
// one that wants the email verified fails the step, names the way out, and leaves no file.
#[tokio::test]
async fn a_claimed_tenant_fails_the_dev_member_and_keeps_no_password() {
    for (status, answer, says) in [
        (200, json!({"next": "pending"}), "already claimed"),
        (403, json!({"error": {"code": "sign_up_closed"}}), "already claimed"),
        (403, json!({"error": {"code": "setup_code_invalid"}}), "already claimed"),
        (409, json!({"error": {"code": "account_exists"}}), "already claimed"),
        (200, json!({"next": "verify"}), "CHELABASE_REQUIRE_EMAIL_VERIFICATION"),
        (400, json!({"error": {"code": "weak_password"}}), "400"),
    ] {
        let out_dir = private_dir();
        let out = outputs::Outputs::open(out_dir.path()).unwrap();
        let auth = fake_auth(vec![(status, answer.clone())]).await;
        let err = format!("{:#}", dev_member::ensure(&waiter(), &out, &dev_cfg(&auth)).await.err().unwrap());
        assert!(err.contains(says), "{answer}: {err}");
        assert!(!err.contains("ABCD"), "the setup code isn't echoed: {err}");
        assert!(!out_dir.path().join(outputs::DEV_MEMBER).exists(), "{answer}");
    }
}

// N2: the orchestrator's probe waits on no answer, a 404 and 502/503/504; a 500 fails at once.
#[tokio::test]
async fn the_orchestrator_probe_fails_on_a_500() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    async fn answering(status: &'static str) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/health", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let _ = sock.read(&mut buf).await;
            let _ = sock
                .write_all(
                    format!("HTTP/1.1 {status}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
                        .as_bytes(),
                )
                .await;
        });
        url
    }
    let probe = HttpProbe::new(
        "the orchestrator",
        &answering("500 Internal Server Error").await,
    );
    assert!(
        probe.check().await.is_err(),
        "a 500 is an answer, not a wait"
    );
    for status in [
        "503 Service Unavailable",
        "502 Bad Gateway",
        "504 Gateway Timeout",
        "404 Not Found",
    ] {
        let probe = HttpProbe::new("the orchestrator", &answering(status).await);
        assert!(
            matches!(probe.check().await.unwrap(), Check::Wait(_)),
            "{status}"
        );
    }
    // #26: a 3xx is an answer too, and is never followed: here it points at a ready server,
    // so a followed redirect would read as ready. It fails at once, naming the status.
    async fn redirecting(status: &'static str, location: String) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/health", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let _ = sock.read(&mut buf).await;
            let _ = sock
                .write_all(
                    format!("HTTP/1.1 {status}\r\nlocation: {location}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
                        .as_bytes(),
                )
                .await;
        });
        url
    }
    for status in [
        "301 Moved Permanently",
        "302 Found",
        "303 See Other",
        "307 Temporary Redirect",
        "308 Permanent Redirect",
    ] {
        let ready = answering("200 OK").await;
        let probe = HttpProbe::new("the orchestrator", &redirecting(status, ready).await);
        let err = probe.check().await.err().unwrap_or_else(|| panic!("a {status} is an answer, not a wait or ready"));
        assert!(err.to_string().contains(&status[..3]), "{status}: {err}");
    }
}

