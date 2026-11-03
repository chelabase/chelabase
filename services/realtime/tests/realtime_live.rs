//! RT1 T7, `realtime_live` part 1 (T8's part 2 is below the part-2 marker), on the plan's stack (S24): realtime runs
//! **in-process** (`app::Deps::connect` + `app::build_with`, ephemeral loopback ports) against
//! the stack's NATS, Valkey and orchestrator with the runner's credentials; only the gateway
//! test uses the compose `realtime` service. The in-process instances verify ticket requests
//! with a key pair of the test's own (the runner has no gateway key); members are the demo
//! tenant's user (`chelabase dev token`, `CHELABASE_CLI`).
//!
//! ```text
//! just workbench test -p realtime --test realtime_live -- --run-ignored only
//! ```
//!
//! Each test that needs a database creates its own through the API and deletes it afterwards,
//! pass or fail.

// Test code: plain clients against local test servers (clippy.toml allows tests only).
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::future::Future;
use std::net::{Ipv4Addr, SocketAddr};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chelabase_auth::internal::{
    AuthMethod, IDENTITY_HEADER, Identity, InternalSigner, InternalVerifier, generate_keypair_pem,
};
use chelabase_auth::member_token::{MEMBER_MAX_TTL_SECS, MemberTokenSigner, MemberTokenVerifier};
use chelabase_nats::rt::AccessEvent;
use futures_util::{SinkExt, StreamExt};
use realtime::app::{Deps, Realtime, build_with};
use realtime::auth::{MemberToken, MemberVerifier, TokenVerifier};
use realtime::config::Config;
use realtime::protocol::SUBPROTOCOL;
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::{self, Message};
use uuid::Uuid;

const DEMO_TENANT_ID: Uuid = Uuid::from_u128(0x0000_0000_0000_4000_8000_0000_0000_0001);
const DEMO_PROJECT_ID: Uuid = Uuid::from_u128(0x0000_0000_0000_4000_8000_0000_0000_0002);

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

/// The gateway's API entry (`CHELABASE_ENTRY_API_URL`: the runner resolves the stack's host
/// names to the gateway).
fn gateway() -> String {
    env("CHELABASE_ENTRY_API_URL")
        .expect("CHELABASE_ENTRY_API_URL (run through just workbench test)")
}

fn workspace_root() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// The `chelabase` CLI: `CHELABASE_CLI` (the runner sets it), else the build next to this test.
fn chelabase_cli() -> std::path::PathBuf {
    if let Some(cli) = env("CHELABASE_CLI") {
        return cli.into();
    }
    let exe = std::env::current_exe().unwrap();
    exe.parent().unwrap().parent().unwrap().join("chelabase")
}

/// An access token of the demo tenant's user that lives [`MEMBER_MAX_TTL_SECS`] from now.
///
/// `chelabase dev token` hands back its kept token while it has more than a minute left (auth's
/// default lifetime is 300 s), and these tests hold one socket for minutes, so a socket opened
/// with it as it comes can meet its `exp` mid-test (4001 "the token expired"). Its claims are
/// signed again with the runner's member key: the same member, tenant and live session (`sid`),
/// only the times are new.
async fn dev_token() -> String {
    let token = cli_dev_token().await;
    let config = Config::from_env().unwrap();
    let issuer = config.issuer.expect("the member tokens' issuer");
    let public = config
        .member_jwt_public_key
        .expect("CHELABASE_MEMBER_JWT_PUBLIC_KEY");
    let keys = env("CHELABASE_KEYS_DIR").expect("CHELABASE_KEYS_DIR (the runner's)");
    let private = std::path::Path::new(&keys).join("auth/member-jwt.key");
    let verifier = MemberTokenVerifier::from_pem_file(std::path::Path::new(&public), &issuer)
        .expect("the member verify key");
    let mut claims = verifier.verify(&token).expect("the dev token verifies");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    claims.iat = now;
    claims.exp = now + MEMBER_MAX_TTL_SECS;
    MemberTokenSigner::from_pem_files(&private, std::path::Path::new(&public), &issuer)
        .expect("the runner's member signing key")
        .sign(&claims)
        .expect("sign the dev token again")
}

/// The CLI's access token of the demo tenant's user (at least a minute left).
async fn cli_dev_token() -> String {
    let checkout = env("CHELABASE_CHECKOUT").map_or_else(workspace_root, Into::into);
    let out = tokio::process::Command::new(chelabase_cli())
        .args(["dev", "token"])
        .current_dir(checkout)
        .stdin(Stdio::null())
        .output()
        .await
        .expect("chelabase dev token");
    assert!(
        out.status.success(),
        "chelabase dev token failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A management API call through the gateway: (status, body).
async fn api(
    token: &str,
    method: reqwest::Method,
    path: &str,
    body: Option<Value>,
) -> (u16, Value) {
    let mut req = reqwest::Client::new()
        .request(method, format!("{}{path}", gateway()))
        .bearer_auth(token)
        .timeout(Duration::from_secs(60));
    if let Some(body) = body {
        req = req.json(&body);
    }
    let resp = req.send().await.expect("the gateway answers");
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(Value::Null))
}

/// A database of the demo project, ready, with a table `rt_items (id int PRIMARY KEY, label
/// text)` holding row 1 in its main Postgres database.
struct Fx {
    token: String,
    id: Uuid,
    pg_database: String,
}

impl Fx {
    fn branch(&self) -> String {
        format!("/v1/databases/{}/branches/{}", self.id, self.id)
    }

    /// A libpq DSN as `postgres` (the owner) through the proxy, and its password.
    async fn dsn(&self, database: &str) -> (String, String) {
        let (status, conn) = api(
            &self.token,
            reqwest::Method::GET,
            &format!("{}/connection", self.branch()),
            None,
        )
        .await;
        assert_eq!(status, 200, "connection: {conn}");
        let hostaddr = env("CHELABASE_PROXY_HOSTADDR").expect("CHELABASE_PROXY_HOSTADDR");
        let port = env("CHELABASE_PROXY_PG_PORT").unwrap_or_else(|| "4432".into());
        let ca = env("CHELABASE_PROXY_CA_FILE").expect("CHELABASE_PROXY_CA_FILE");
        (
            format!(
                "host={} hostaddr={hostaddr} port={port} user=postgres dbname={database} sslmode=verify-full sslrootcert={ca}",
                conn["host"].as_str().unwrap()
            ),
            conn["password"].as_str().unwrap().to_string(),
        )
    }

    async fn psql(&self, database: &str, sql: &str) -> String {
        let (status, conn) = api(
            &self.token,
            reqwest::Method::GET,
            &format!("{}/connection", self.branch()),
            None,
        )
        .await;
        assert_eq!(status, 200, "connection: {conn}");
        let hostaddr = env("CHELABASE_PROXY_HOSTADDR").expect("CHELABASE_PROXY_HOSTADDR");
        let port = env("CHELABASE_PROXY_PG_PORT").unwrap_or_else(|| "4432".into());
        let ca = env("CHELABASE_PROXY_CA_FILE").expect("CHELABASE_PROXY_CA_FILE");
        let dsn = format!(
            "host={} hostaddr={hostaddr} port={port} user=postgres dbname={database} sslmode=verify-full sslrootcert={ca}",
            conn["host"].as_str().unwrap()
        );
        let mut last = String::new();
        for _ in 0..60 {
            let out = tokio::process::Command::new("psql")
                .arg(&dsn)
                .args(["-v", "ON_ERROR_STOP=1", "-qtA", "-c", sql])
                .env("PGPASSWORD", conn["password"].as_str().unwrap())
                .env("PGCONNECT_TIMEOUT", "10")
                .stdin(Stdio::null())
                .output()
                .await
                .expect("psql");
            if out.status.success() {
                return String::from_utf8_lossy(&out.stdout).trim().to_string();
            }
            last = String::from_utf8_lossy(&out.stderr).to_string();
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        panic!("psql in {database}: {last}");
    }

    /// The branch's Postgres database `name`'s `data_api` state, if listed.
    async fn data_api_of(&self, name: &str) -> Option<String> {
        let (_, list) = api(
            &self.token,
            reqwest::Method::GET,
            &format!("{}/databases", self.branch()),
            None,
        )
        .await;
        list.as_array()?
            .iter()
            .find(|d| d["name"] == name)
            .and_then(|d| d["data_api"].as_str().map(str::to_string))
    }
}

/// Creates the database, runs `body`, deletes the database, pass or fail.
async fn with_database<F, Fut>(body: F)
where
    F: FnOnce(Arc<Fx>) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let token = dev_token().await;
    let name = format!("rt_live_{}", &Uuid::new_v4().simple().to_string()[..8]);
    let (status, created) = api(
        &token,
        reqwest::Method::POST,
        "/v1/databases",
        Some(json!({"tenant_id": DEMO_TENANT_ID, "project_id": DEMO_PROJECT_ID, "name": name})),
    )
    .await;
    assert!(
        status == 201 || status == 202,
        "create database: {status} {created}"
    );
    let id: Uuid = created["database_id"].as_str().unwrap().parse().unwrap();
    let ready = async {
        for _ in 0..180 {
            let (_, db) = api(
                &token,
                reqwest::Method::GET,
                &format!("/v1/databases/{id}"),
                None,
            )
            .await;
            match db["status"].as_str() {
                Some("ready") => return Ok(()),
                Some("failed") => return Err(format!("failed: {db}")),
                _ => tokio::time::sleep(Duration::from_secs(1)).await,
            }
        }
        Err("not ready after 180 s".to_string())
    }
    .await;
    let outcome = match ready {
        Ok(()) => {
            let (_, list) = api(
                &token,
                reqwest::Method::GET,
                &format!("/v1/databases/{id}/branches/{id}/databases"),
                None,
            )
            .await;
            let pg_database = list[0]["name"]
                .as_str()
                .expect("the main Postgres database")
                .to_string();
            let fx = Arc::new(Fx {
                token: token.clone(),
                id,
                pg_database,
            });
            // A panic inside the task is caught here, so the cleanup below always runs.
            Some(
                tokio::spawn(async move {
                    fx.psql(&fx.pg_database, "CREATE TABLE rt_items (id int PRIMARY KEY, label text); INSERT INTO rt_items VALUES (1, 'one');")
                        .await;
                    body(fx).await
                })
                .await,
            )
        }
        Err(e) => {
            eprintln!("database {id}: {e}");
            None
        }
    };
    let _ = api(
        &token,
        reqwest::Method::DELETE,
        &format!("/v1/databases/{id}"),
        None,
    )
    .await;
    match outcome {
        Some(Ok(())) => {}
        Some(Err(panic)) => std::panic::resume_unwind(panic.into_panic()),
        None => panic!("database {id} never became ready"),
    }
}

/// An in-process realtime with the stack's dependencies (runner credentials) and the test's
/// own identity signer.
struct Live {
    rt: Realtime,
    signer: Arc<InternalSigner>,
}

async fn live_pair() -> (Live, Live) {
    let (signing_pem, verify_pem) = generate_keypair_pem();
    let signer = Arc::new(InternalSigner::from_pem(signing_pem.as_bytes()).unwrap());
    let mut out = vec![];
    for _ in 0..2 {
        let mut config = Config::from_env().expect("realtime's settings from the runner's env");
        config.addr = SocketAddr::from((Ipv4Addr::LOCALHOST, 0));
        config.internal_addr = SocketAddr::from((Ipv4Addr::LOCALHOST, 0));
        config.instance_id = Uuid::new_v4().to_string();
        let identity = Arc::new(InternalVerifier::from_pem(verify_pem.as_bytes()).unwrap());
        let deps = Deps::connect(&config, identity)
            .await
            .expect("NATS, Valkey and the member-token key with the runner's credentials");
        out.push(Live {
            rt: build_with(config, deps).await.expect("realtime starts"),
            signer: signer.clone(),
        });
    }
    let b = out.pop().unwrap();
    (out.pop().unwrap(), b)
}

async fn live_one() -> Live {
    live_pair().await.0
}

/// The member a token names, verified as realtime verifies it.
async fn member_of(token: &str) -> MemberToken {
    let config = Config::from_env().unwrap();
    let issuer = config.issuer.expect("the member tokens' issuer");
    let key = config.member_jwt_public_key.expect("CHELABASE_MEMBER_JWT_PUBLIC_KEY");
    let member = MemberTokenVerifier::from_pem_file(&key, &issuer)
        .expect("the member verify key");
    MemberVerifier::new(Some(member))
        .verify(token)
        .await
        .expect("the dev token verifies")
}

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

impl Live {
    async fn ticket(&self, member: &MemberToken) -> String {
        let identity = Identity {
            sub: member.sub.clone(),
            auth: AuthMethod::ManagementJwt,
            request_id: format!("live-{}", Uuid::new_v4()),
            data: None,
            email: member.email.clone(),
            name: member.name.clone(),
        };
        let resp = reqwest::Client::new()
            .post(format!("http://{}/v1/realtime/tickets", self.rt.addr()))
            .header(
                IDENTITY_HEADER,
                self.signer
                    .sign_for_tenant("realtime", &identity, member.tenant_id.expect("a tenant"), &member.sid)
                    .unwrap(),
            )
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 201);
        resp.json::<Value>().await.unwrap()["ticket"]
            .as_str()
            .unwrap()
            .to_string()
    }

    fn url(&self, ticket: &str) -> String {
        format!("ws://{}/v1/realtime/socket?ticket={ticket}", self.rt.addr())
    }

    /// A socket authenticated with `token`.
    async fn socket(&self, token: &str) -> Socket {
        let member = member_of(token).await;
        let mut ws = open(&self.url(&self.ticket(&member).await))
            .await
            .expect("the socket opens");
        send(&mut ws, json!({"type": "auth", "token": token})).await;
        ws
    }
}

async fn open(url: &str) -> Result<Socket, u16> {
    let mut req = url.into_client_request().unwrap();
    req.headers_mut().insert(
        "Sec-WebSocket-Protocol",
        HeaderValue::from_static(SUBPROTOCOL),
    );
    match tokio_tungstenite::connect_async(req).await {
        Ok((ws, resp)) => {
            assert_eq!(
                resp.headers().get("sec-websocket-protocol").unwrap(),
                SUBPROTOCOL
            );
            Ok(ws)
        }
        Err(tungstenite::Error::Http(resp)) => Err(resp.status().as_u16()),
        Err(e) => panic!("upgrade: {e}"),
    }
}

async fn send(ws: &mut Socket, v: Value) {
    ws.send(Message::Text(v.to_string().into())).await.unwrap();
}

/// The next frame matching `pred` within `within`; panics on a close or a timeout.
async fn frame_where(ws: &mut Socket, within: Duration, pred: impl Fn(&Value) -> bool) -> Value {
    let deadline = Instant::now() + within;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match tokio::time::timeout(left, ws.next()).await {
            Err(_) => panic!("no matching frame within {within:?}"),
            Ok(Some(Ok(Message::Text(t)))) => {
                let v: Value = serde_json::from_str(t.as_str()).unwrap();
                if pred(&v) {
                    return v;
                }
            }
            Ok(Some(Ok(Message::Close(f)))) => panic!("closed: {f:?}"),
            Ok(Some(Ok(_))) => continue,
            Ok(other) => panic!("socket ended: {other:?}"),
        }
    }
}

async fn reply(ws: &mut Socket, r: &str) -> Value {
    frame_where(ws, Duration::from_secs(30), |v| v["ref"] == r).await
}

fn table_join(r: &str, fx: &Fx, pg_database: &str, table: &str, role: Option<&str>) -> Value {
    json!({"type": "join", "ref": r, "channel": "table", "params": {
        "database_id": fx.id, "branch_id": fx.id, "pg_database": pg_database, "schema": "public", "table": table, "role": role}})
}

fn table_id(fx: &Fx, pg_database: &str, table: &str) -> String {
    format!(
        r#"table:["{}","{}","{pg_database}","public","{table}"]"#,
        fx.id, fx.id
    )
}

#[tokio::test]
#[ignore = "needs the workbench stack (just workbench test -p realtime --test realtime_live -- --run-ignored only)"]
async fn join_and_refusal_against_the_orchestrator() {
    with_database(|fx| async move {
        let live = live_one().await;
        let mut ws = live.socket(&fx.token).await;
        send(
            &mut ws,
            table_join("1", &fx, &fx.pg_database, "rt_items", None),
        )
        .await;
        let joined = reply(&mut ws, "1").await;
        assert_eq!(
            joined["type"], "joined",
            "the tenant admin watches as the owner: {joined}"
        );
        assert_eq!(
            joined["channel"],
            table_id(&fx, &fx.pg_database, "rt_items")
        );
        send(
            &mut ws,
            table_join("2", &fx, "rt_no_such_database", "rt_items", None),
        )
        .await;
        assert_eq!(reply(&mut ws, "2").await["code"], "not_found");
        // RT1 final fix round (test bug, process failure): the same table id on the same
        // socket is `invalid "already joined"` whatever the role (S17), so the owner's channel
        // is left before the same table is joined as another role.
        send(
            &mut ws,
            serde_json::json!({
                "type": "leave",
                "ref": "2b",
                "channel": table_id(&fx, &fx.pg_database, "rt_items"),
            }),
        )
        .await;
        send(
            &mut ws,
            table_join(
                "3",
                &fx,
                &fx.pg_database,
                "rt_items",
                Some("rt_no_such_role"),
            ),
        )
        .await;
        let refused = reply(&mut ws, "3").await;
        assert_eq!(
            (refused["type"].as_str(), refused["code"].as_str()),
            (Some("error"), Some("forbidden")),
            "{refused}"
        );
        live.rt.shutdown();
    })
    .await;
}

#[tokio::test]
#[ignore = "needs the workbench stack (just workbench test -p realtime --test realtime_live -- --run-ignored only)"]
async fn revocation_within_a_second_of_an_access_event() {
    with_database(|fx| async move {
        // A second Postgres database, served, then switched off while watched.
        fx.psql(&fx.pg_database, "CREATE DATABASE rt_second").await;
        let mut listed = false;
        for _ in 0..30 {
            if fx.data_api_of("rt_second").await.is_some() {
                listed = true;
                break;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        assert!(listed, "rt_second is listed");
        let (status, body) = api(
            &fx.token,
            reqwest::Method::POST,
            &format!("{}/pg-databases/rt_second/data-api", fx.branch()),
            None,
        )
        .await;
        assert_eq!(status, 202, "{body}");
        let mut on = false;
        for _ in 0..60 {
            if fx.data_api_of("rt_second").await.as_deref() == Some("on") {
                on = true;
                break;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        assert!(on, "rt_second is served");
        fx.psql("rt_second", "CREATE TABLE rt_items (id int PRIMARY KEY)")
            .await;

        let live = live_one().await;
        let mut ws = live.socket(&fx.token).await;
        send(&mut ws, table_join("1", &fx, "rt_second", "rt_items", None)).await;
        assert_eq!(reply(&mut ws, "1").await["type"], "joined");
        let (status, body) = api(
            &fx.token,
            reqwest::Method::DELETE,
            &format!("{}/pg-databases/rt_second/data-api", fx.branch()),
            None,
        )
        .await;
        assert_eq!(status, 204, "{body}");
        let member = member_of(&fx.token).await;
        let nats = chelabase_nats::connect(&chelabase_nats::NatsConfig::from_env().unwrap())
            .await
            .unwrap();
        let started = Instant::now();
        chelabase_nats::rt::publish_access(
            &nats,
            &AccessEvent {
                tenant_id: DEMO_TENANT_ID,
                member_sub: Some(member.sub),
            },
        )
        .await
        .unwrap();
        nats.flush().await.unwrap();
        let e = frame_where(&mut ws, Duration::from_secs(5), |v| v["type"] == "error").await;
        let took = started.elapsed();
        assert_eq!(
            (e["code"].as_str(), e["channel"].as_str()),
            (
                Some("forbidden"),
                Some(table_id(&fx, "rt_second", "rt_items").as_str())
            )
        );
        assert!(
            took <= Duration::from_secs(1),
            "revoked {took:?} after the access event"
        );
        live.rt.shutdown();
    })
    .await;
}

#[tokio::test]
#[ignore = "needs the workbench stack (just workbench test -p realtime --test realtime_live -- --run-ignored only)"]
async fn project_events_from_a_real_branch_create() {
    with_database(|fx| async move {
        let live = live_one().await;
        let mut ws = live.socket(&fx.token).await;
        send(
            &mut ws,
            json!({"type": "join", "ref": "1", "channel": format!("project:{DEMO_PROJECT_ID}")}),
        )
        .await;
        assert_eq!(reply(&mut ws, "1").await["type"], "joined");
        let (status, branch) = api(
            &fx.token,
            reqwest::Method::POST,
            &format!("/v1/databases/{}/branches", fx.id),
            Some(json!({"name": "rt-live"})),
        )
        .await;
        assert_eq!(status, 202, "{branch}");
        let branch_id = branch["id"].clone();
        let created = frame_where(&mut ws, Duration::from_secs(60), |v| {
            v["type"] == "project" && v["event"] == "branch.created" && v["branch_id"] == branch_id
        })
        .await;
        assert_eq!(created["channel"], format!("project:{DEMO_PROJECT_ID}"));
        assert_eq!(created["database_id"], json!(fx.id));
        let ready = frame_where(&mut ws, Duration::from_secs(120), |v| {
            v["type"] == "project"
                && v["event"] == "branch.status"
                && v["branch_id"] == branch_id
                && v["status"] == "ready"
        })
        .await;
        assert_eq!(ready["database_id"], json!(fx.id));
        live.rt.shutdown();
    })
    .await;
}

#[tokio::test]
#[ignore = "needs the workbench stack (just workbench test -p realtime --test realtime_live -- --run-ignored only)"]
async fn a_replayed_ticket_is_refused_through_valkey() {
    let live = live_one().await;
    let token = dev_token().await;
    let member = member_of(&token).await;
    let ticket = live.ticket(&member).await;
    let ws = open(&live.url(&ticket)).await;
    assert!(ws.is_ok(), "the first use opens");
    assert_eq!(
        open(&live.url(&ticket)).await.err(),
        Some(401),
        "GETDEL in Valkey: a replay finds nothing"
    );
    assert_eq!(
        open(&live.url(&realtime::ticket::new_ticket())).await.err(),
        Some(401)
    );
    live.rt.shutdown();
}

#[tokio::test]
#[ignore = "needs the workbench stack (just workbench test -p realtime --test realtime_live -- --run-ignored only)"]
async fn two_instances_share_presence() {
    with_database(|fx| async move {
        let (one, two) = live_pair().await;
        // Two tabs of the demo user, one per instance.
        let mut a = one.socket(&fx.token).await;
        send(&mut a, table_join("1", &fx, &fx.pg_database, "rt_items", None)).await;
        assert_eq!(reply(&mut a, "1").await["type"], "joined");
        let mut b = two.socket(&fx.token).await;
        send(&mut b, table_join("1", &fx, &fx.pg_database, "rt_items", None)).await;
        assert_eq!(reply(&mut b, "1").await["type"], "joined");
        let state = frame_where(&mut b, Duration::from_secs(10), |v| v["type"] == "presence_state").await;
        assert_eq!(state["members"].as_array().unwrap().len(), 2, "the other instance's member came by snapshot: {state}");
        frame_where(&mut a, Duration::from_secs(10), |v| v["type"] == "presence_diff" && !v["joins"].as_array().unwrap().is_empty()).await;
        let channel = table_id(&fx, &fx.pg_database, "rt_items");
        send(&mut a, json!({"type": "presence", "channel": channel, "state": {"row": "[\"1\"]", "column": "label", "editing": false}})).await;
        let moved = frame_where(&mut b, Duration::from_secs(10), |v| {
            v["type"] == "presence_diff" && v["joins"].as_array().unwrap().iter().any(|m| m["state"]["row"] == "[\"1\"]")
        })
        .await;
        assert_eq!(moved["channel"], channel);
        a.close(None).await.unwrap();
        frame_where(&mut b, Duration::from_secs(10), |v| v["type"] == "presence_diff" && !v["leaves"].as_array().unwrap().is_empty()).await;
        one.rt.shutdown();
        two.rt.shutdown();
    })
    .await;
}

#[tokio::test]
#[ignore = "needs the workbench stack (just workbench test -p realtime --test realtime_live -- --run-ignored only)"]
async fn the_gateway_route_upgrades_through_the_compose_service() {
    let token = dev_token().await;
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/realtime/tickets", gateway()))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 201);
    assert_eq!(
        resp.headers().get("cache-control").unwrap(),
        "no-store",
        "passed through by the gateway"
    );
    let ticket = resp.json::<Value>().await.unwrap()["ticket"]
        .as_str()
        .unwrap()
        .to_string();
    let ws_base = gateway().replacen("http", "ws", 1);
    let mut ws = open(&format!("{ws_base}/v1/realtime/socket?ticket={ticket}"))
        .await
        .expect("101 through the gateway");
    send(&mut ws, json!({"type": "auth", "token": token})).await;
    send(
        &mut ws,
        json!({"type": "join", "ref": "1", "channel": format!("project:{DEMO_PROJECT_ID}")}),
    )
    .await;
    assert_eq!(reply(&mut ws, "1").await["type"], "joined");
    assert_eq!(
        open(&format!("{ws_base}/v1/realtime/socket?ticket={ticket}"))
            .await
            .err(),
        Some(401),
        "used"
    );
}

// ======================= part 2 (T8): the change source on real computes =======================
//
// Realtime runs in-process (S24) with the real `ReplicationSource` (`Deps::connect`). U16 is
// present on these images: a compute restart or a slot may leave a stale logical-decoding aux
// file on the pageserver; that is expected (the stack is purged) and nothing here checks for it.

/// An in-process realtime whose settings `tune` changes (e.g. the slot cap).
async fn live_with(tune: impl FnOnce(&mut Config)) -> Live {
    let (signing_pem, verify_pem) = generate_keypair_pem();
    let signer = Arc::new(InternalSigner::from_pem(signing_pem.as_bytes()).unwrap());
    let mut config = Config::from_env().expect("realtime's settings from the runner's env");
    config.addr = SocketAddr::from((Ipv4Addr::LOCALHOST, 0));
    config.internal_addr = SocketAddr::from((Ipv4Addr::LOCALHOST, 0));
    config.instance_id = Uuid::new_v4().to_string();
    tune(&mut config);
    let identity = Arc::new(InternalVerifier::from_pem(verify_pem.as_bytes()).unwrap());
    let deps = Deps::connect(&config, identity)
        .await
        .expect("NATS, Valkey, the member-token key and the replication source with the runner's credentials");
    Live {
        rt: build_with(config, deps).await.expect("realtime starts"),
        signer,
    }
}

fn join_in(r: &str, fx: &Fx, pg_database: &str, schema: &str, table: &str, role: Option<&str>) -> Value {
    json!({"type": "join", "ref": r, "channel": "table", "params": {
        "database_id": fx.id, "branch_id": fx.id, "pg_database": pg_database, "schema": schema, "table": table, "role": role}})
}

/// Joins and waits until the channel is live (`joined.info.live`, or a later `status {live:
/// true}`): the channel id.
async fn join_live(ws: &mut Socket, r: &str, join: Value, within: Duration) -> String {
    send(ws, join).await;
    let joined = reply(ws, r).await;
    assert_eq!(joined["type"], "joined", "{joined}");
    let channel = joined["channel"].as_str().unwrap().to_string();
    if joined["info"]["live"] != true {
        let ch = channel.clone();
        frame_where(ws, within, |v| {
            v["type"] == "status" && v["channel"] == ch.as_str() && v["live"] == true
        })
        .await;
    }
    channel
}

/// The keys of each op in every `rows` frame of `channel` for `within`.
#[derive(Debug, Default)]
struct Got {
    inserts: std::collections::BTreeSet<String>,
    updates: std::collections::BTreeSet<String>,
    deletes: std::collections::BTreeSet<String>,
    changed: usize,
    keyless_deletes: usize,
}

async fn collect_rows(ws: &mut Socket, channel: &str, within: Duration) -> Got {
    let deadline = Instant::now() + within;
    let mut got = Got::default();
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return got;
        }
        match tokio::time::timeout(left, ws.next()).await {
            Err(_) => return got,
            Ok(Some(Ok(Message::Text(t)))) => {
                let v: Value = serde_json::from_str(t.as_str()).unwrap();
                if v["type"] != "rows" || v["channel"] != channel {
                    continue;
                }
                for c in v["changes"].as_array().unwrap() {
                    let keys: Vec<String> = c["keys"]
                        .as_array()
                        .map(|k| k.iter().map(|k| k.as_str().unwrap().to_string()).collect())
                        .unwrap_or_default();
                    match (c["op"].as_str().unwrap(), c.get("keys").is_some()) {
                        ("insert", _) => got.inserts.extend(keys),
                        ("update", _) => got.updates.extend(keys),
                        ("delete", true) => got.deletes.extend(keys),
                        ("delete", false) => got.keyless_deletes += 1,
                        ("changed", _) => got.changed += 1,
                        (op, _) => panic!("op {op}"),
                    }
                }
            }
            Ok(Some(Ok(Message::Close(f)))) => panic!("closed: {f:?}"),
            Ok(Some(Ok(_))) => continue,
            Ok(other) => panic!("socket ended: {other:?}"),
        }
    }
}

fn key(texts: &[&str]) -> String {
    chelabase_query::realtime::key_string(&texts.iter().map(|s| s.to_string()).collect::<Vec<_>>())
}

fn keys(list: &[&str]) -> std::collections::BTreeSet<String> {
    list.iter().map(|k| key(&[k])).collect()
}

/// Studio's page read as `role` (`None` = the owner): the key strings of `cols` (`text_of`).
async fn console_keys(fx: &Fx, pg_database: &str, table: &str, role: Option<&str>, cols: &[&str]) -> Vec<String> {
    let select: Vec<Value> = cols
        .iter()
        .enumerate()
        .map(|(i, c)| json!({"text_of": c, "as": format!("$pk{i}")}))
        .collect();
    let (status, body) = api(
        &fx.token,
        reqwest::Method::POST,
        &format!("{}/console/query", fx.branch()),
        Some(json!({"role": role, "database": pg_database, "tree": {"v": 1, "op": "select", "from": table, "select": select}})),
    )
    .await;
    assert_eq!(status, 200, "console read as {role:?}: {body}");
    let rows = body
        .as_array()
        .or_else(|| body["data"].as_array())
        .unwrap_or_else(|| panic!("rows: {body}"))
        .clone();
    rows.iter()
        .map(|r| {
            let texts: Vec<String> = (0..cols.len())
                .map(|i| r[format!("$pk{i}")].as_str().unwrap().to_string())
                .collect();
            chelabase_query::realtime::key_string(&texts)
        })
        .collect()
}

/// The compute's endpoint id, from the branch's connection route (`server_name`).
async fn endpoint_id(fx: &Fx) -> String {
    let config = Config::from_env().unwrap();
    let resp = reqwest::Client::new()
        .get(format!(
            "{}/internal/data/branches/{}/connection",
            config.internal_url, fx.id
        ))
        .bearer_auth(&config.internal_token)
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    body["server_name"]
        .as_str()
        .and_then(|s| s.split('.').next())
        .unwrap_or_else(|| panic!("server_name: {body}"))
        .to_string()
}

async fn docker_restart_compute(fx: &Fx) {
    let project = env("CHELABASE_COMPOSE_PROJECT").expect("CHELABASE_COMPOSE_PROJECT");
    let container = format!("{project}-compute-{}", endpoint_id(fx).await);
    let out = tokio::process::Command::new("docker")
        .args(["restart", &container])
        .output()
        .await
        .expect("docker");
    assert!(out.status.success(), "docker restart {container}: {}", String::from_utf8_lossy(&out.stderr));
}

/// A second Postgres database `name` in the branch, served by the data API, with `rt_items`.
async fn served_database(fx: &Fx, name: &str) {
    served_database_with(fx, name, "").await
}

/// [`served_database`], running `setup` in the main database right after `CREATE DATABASE`
/// (before anything connects to the new one: database-level settings apply to every pool).
async fn served_database_with(fx: &Fx, name: &str, setup: &str) {
    fx.psql(&fx.pg_database, &format!("CREATE DATABASE {name}")).await;
    if !setup.is_empty() {
        fx.psql(&fx.pg_database, setup).await;
    }
    let mut listed = false;
    for _ in 0..30 {
        if fx.data_api_of(name).await.is_some() {
            listed = true;
            break;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    assert!(listed, "{name} is listed");
    let (status, body) = api(
        &fx.token,
        reqwest::Method::POST,
        &format!("{}/pg-databases/{name}/data-api", fx.branch()),
        None,
    )
    .await;
    assert_eq!(status, 202, "{body}");
    let mut on = false;
    for _ in 0..60 {
        if fx.data_api_of(name).await.as_deref() == Some("on") {
            on = true;
            break;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    assert!(on, "{name} is served");
    fx.psql(name, "CREATE TABLE rt_items (id int PRIMARY KEY, label text)").await;
}

/// The `rt_` slots of `database` (as the owner sees `pg_replication_slots`).
async fn rt_slots(fx: &Fx, database: &str) -> usize {
    fx.psql(
        &fx.pg_database,
        &format!("SELECT count(*) FROM pg_replication_slots WHERE slot_name LIKE 'rt\\_%' AND database = '{database}'"),
    )
    .await
    .trim()
    .parse()
    .unwrap()
}

const RLS_SETUP: &str = "CREATE TABLE rt_acl (id int PRIMARY KEY, tag text); \
    ALTER TABLE rt_acl ENABLE ROW LEVEL SECURITY; \
    CREATE POLICY shown ON rt_acl FOR SELECT USING (tag = 'shown'); \
    CREATE ROLE rt_rls NOLOGIN; CREATE ROLE rt_member NOLOGIN BYPASSRLS; \
    GRANT SELECT ON rt_acl TO rt_rls, rt_member;";

#[tokio::test]
#[ignore = "needs the workbench stack (just workbench test -p realtime --test realtime_live -- --run-ignored only)"]
async fn owner_member_and_rls_viewers_get_their_own_keys() {
    with_database(|fx| async move {
        fx.psql(&fx.pg_database, RLS_SETUP).await;
        let live = live_one().await;
        let mut owner = live.socket(&fx.token).await;
        let mut member = live.socket(&fx.token).await;
        let mut rls = live.socket(&fx.token).await;
        let ch = join_live(&mut owner, "1", join_in("1", &fx, &fx.pg_database, "public", "rt_acl", None), Duration::from_secs(60)).await;
        join_live(&mut member, "1", join_in("1", &fx, &fx.pg_database, "public", "rt_acl", Some("rt_member")), Duration::from_secs(60)).await;
        join_live(&mut rls, "1", join_in("1", &fx, &fx.pg_database, "public", "rt_acl", Some("rt_rls")), Duration::from_secs(60)).await;
        fx.psql(&fx.pg_database, "INSERT INTO rt_acl VALUES (10, 'shown'), (11, 'hidden')").await;
        let (o, m, r) = tokio::join!(
            collect_rows(&mut owner, &ch, Duration::from_secs(8)),
            collect_rows(&mut member, &ch, Duration::from_secs(8)),
            collect_rows(&mut rls, &ch, Duration::from_secs(8)),
        );
        assert_eq!(o.inserts, keys(&["10", "11"]), "the owner (bypass): {o:?}");
        assert_eq!(m.inserts, keys(&["10", "11"]), "a BYPASSRLS member role: {m:?}");
        assert_eq!(r.inserts, keys(&["10"]), "the RLS role sees only its rows: {r:?}");
        assert_eq!(r.changed, 0, "{r:?}");
        live.rt.shutdown();
    })
    .await;
}

#[tokio::test]
#[ignore = "needs the workbench stack (just workbench test -p realtime --test realtime_live -- --run-ignored only)"]
async fn keyed_and_keyless_deletes() {
    with_database(|fx| async move {
        fx.psql(&fx.pg_database, RLS_SETUP).await;
        fx.psql(&fx.pg_database, "INSERT INTO rt_acl VALUES (10, 'shown'), (11, 'hidden'), (13, 'shown')").await;
        let live = live_one().await;
        let mut owner = live.socket(&fx.token).await;
        let mut rls = live.socket(&fx.token).await;
        let ch = join_live(&mut owner, "1", join_in("1", &fx, &fx.pg_database, "public", "rt_acl", None), Duration::from_secs(60)).await;
        join_live(&mut rls, "1", join_in("1", &fx, &fx.pg_database, "public", "rt_acl", Some("rt_rls")), Duration::from_secs(60)).await;
        // R19: the RLS viewer's screen shows 10 (13 is visible too, but not on its screen).
        send(&mut rls, json!({"type": "view", "channel": ch, "keys": [key(&["10"])]})).await;
        tokio::time::sleep(Duration::from_millis(500)).await;
        fx.psql(&fx.pg_database, "DELETE FROM rt_acl WHERE id IN (10, 11, 13)").await;
        let (o, r) = tokio::join!(
            collect_rows(&mut owner, &ch, Duration::from_secs(8)),
            collect_rows(&mut rls, &ch, Duration::from_secs(8)),
        );
        assert_eq!(o.deletes, keys(&["10", "11", "13"]), "bypass viewers get keyed deletes: {o:?}");
        assert_eq!(r.deletes, keys(&["10"]), "RLS: keyed only for a key on its screen: {r:?}");

        // Delivered earlier counts too, and W26: an update that hides a viewed row.
        fx.psql(&fx.pg_database, "INSERT INTO rt_acl VALUES (12, 'shown'), (14, 'shown')").await;
        let r = collect_rows(&mut rls, &ch, Duration::from_secs(8)).await;
        assert_eq!(r.inserts, keys(&["12", "14"]), "{r:?}");
        send(&mut rls, json!({"type": "view", "channel": ch, "keys": [key(&["12"])]})).await;
        tokio::time::sleep(Duration::from_millis(500)).await;
        fx.psql(&fx.pg_database, "UPDATE rt_acl SET tag = 'hidden' WHERE id = 12").await;
        let r = collect_rows(&mut rls, &ch, Duration::from_secs(8)).await;
        assert_eq!(r.deletes, keys(&["12"]), "hidden from the viewer: a keyed delete: {r:?}");
        assert!(r.updates.is_empty(), "{r:?}");
        fx.psql(&fx.pg_database, "DELETE FROM rt_acl WHERE id = 14").await;
        let r = collect_rows(&mut rls, &ch, Duration::from_secs(8)).await;
        assert_eq!(r.deletes, keys(&["14"]), "delivered to it before: keyed: {r:?}");
        live.rt.shutdown();
    })
    .await;
}

#[tokio::test]
#[ignore = "needs the workbench stack (just workbench test -p realtime --test realtime_live -- --run-ignored only)"]
async fn slot_lifecycle_and_resync_after_a_compute_restart() {
    with_database(|fx| async move {
        let live = live_one().await;
        let mut ws = live.socket(&fx.token).await;
        let ch = join_live(&mut ws, "1", join_in("1", &fx, &fx.pg_database, "public", "rt_items", None), Duration::from_secs(60)).await;
        assert_eq!(rt_slots(&fx, &fx.pg_database).await, 1, "one temporary rt_ slot");
        docker_restart_compute(&fx).await;
        let c = ch.clone();
        frame_where(&mut ws, Duration::from_secs(180), |v| v["type"] == "resync" && v["channel"] == c.as_str()).await;
        let c = ch.clone();
        frame_where(&mut ws, Duration::from_secs(180), |v| v["type"] == "status" && v["channel"] == c.as_str() && v["live"] == true).await;
        fx.psql(&fx.pg_database, "INSERT INTO rt_items VALUES (2, 'two')").await;
        let got = collect_rows(&mut ws, &ch, Duration::from_secs(8)).await;
        assert_eq!(got.inserts, keys(&["2"]), "live on the new slot: {got:?}");
        // The last viewer leaves: the slot goes 30 s after the last heartbeat.
        send(&mut ws, json!({"type": "leave", "ref": "2", "channel": ch})).await;
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            if rt_slots(&fx, &fx.pg_database).await == 0 {
                break;
            }
            assert!(Instant::now() < deadline, "the slot outlived its watchers");
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        live.rt.shutdown();
    })
    .await;
}

#[tokio::test]
#[ignore = "needs the workbench stack (just workbench test -p realtime --test realtime_live -- --run-ignored only)"]
async fn slot_exhaustion_is_no_slot() {
    with_database(|fx| async move {
        served_database(&fx, "rt_second").await;
        let live = live_with(|c| c.max_slots_per_compute = 1).await;
        let mut ws = live.socket(&fx.token).await;
        join_live(&mut ws, "1", join_in("1", &fx, &fx.pg_database, "public", "rt_items", None), Duration::from_secs(60)).await;
        send(&mut ws, join_in("2", &fx, "rt_second", "public", "rt_items", None)).await;
        let joined = reply(&mut ws, "2").await;
        assert_eq!(joined["type"], "joined", "{joined}");
        let ch = joined["channel"].as_str().unwrap().to_string();
        if joined["info"]["reason"] != "no_slot" {
            frame_where(&mut ws, Duration::from_secs(30), |v| {
                v["type"] == "status" && v["channel"] == ch.as_str() && v["live"] == false && v["reason"] == "no_slot"
            })
            .await;
        }
        assert_eq!(rt_slots(&fx, "rt_second").await, 0, "the cap held");
        live.rt.shutdown();
    })
    .await;
}

#[tokio::test]
#[ignore = "needs the workbench stack (just workbench test -p realtime --test realtime_live -- --run-ignored only)"]
async fn an_open_transaction_blocks_creation_until_commit() {
    with_database(|fx| async move {
        let (dsn, password) = fx.dsn(&fx.pg_database).await;
        let mut child = tokio::process::Command::new("psql")
            .args([dsn.as_str(), "-X", "-qtA"])
            .env("PGPASSWORD", &password)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("psql");
        let mut stdin = child.stdin.take().unwrap();
        use tokio::io::AsyncWriteExt;
        stdin
            .write_all(b"BEGIN; SELECT pg_current_xact_id(); INSERT INTO rt_items VALUES (5, 'open');\n")
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_secs(2)).await;
        let live = live_one().await;
        let mut ws = live.socket(&fx.token).await;
        send(&mut ws, join_in("1", &fx, &fx.pg_database, "public", "rt_items", None)).await;
        let joined = reply(&mut ws, "1").await;
        assert_eq!(joined["type"], "joined", "{joined}");
        let ch = joined["channel"].as_str().unwrap().to_string();
        assert_ne!(joined["info"]["live"], true, "no consistent point while a transaction runs");
        // W7: creation gives up at 10 s and says so. RT1 final fix round (test bug, process
        // failure): statuses are de-duplicated, so when the join already said `waiting` no
        // status frame follows; wait for one only otherwise.
        if joined["info"]["reason"] != "waiting" {
            let c = ch.clone();
            let waiting = frame_where(&mut ws, Duration::from_secs(20), |v| {
                v["type"] == "status" && v["channel"] == c.as_str() && v["live"] == false
            })
            .await;
            assert_eq!(waiting["reason"], "waiting", "{waiting} {joined}");
        } else {
            // Past the 10 s creation timeout, still not live.
            tokio::time::sleep(Duration::from_secs(11)).await;
        }
        stdin.write_all(b"COMMIT;\n").await.unwrap();
        let c = ch.clone();
        frame_where(&mut ws, Duration::from_secs(90), |v| {
            v["type"] == "status" && v["channel"] == c.as_str() && v["live"] == true
        })
        .await;
        drop(stdin);
        let _ = tokio::time::timeout(Duration::from_secs(10), child.wait()).await;
        let _ = child.kill().await;
        live.rt.shutdown();
    })
    .await;
}

#[tokio::test]
#[ignore = "needs the workbench stack (just workbench test -p realtime --test realtime_live -- --run-ignored only)"]
async fn compute_stopping_then_live_again() {
    with_database(|fx| async move {
        let live = live_one().await;
        let mut ws = live.socket(&fx.token).await;
        let ch = join_live(&mut ws, "1", join_in("1", &fx, &fx.pg_database, "public", "rt_items", None), Duration::from_secs(60)).await;
        let nats = chelabase_nats::connect(&chelabase_nats::NatsConfig::from_env().unwrap())
            .await
            .unwrap();
        chelabase_nats::rt::publish_compute_stopping(&nats, fx.id).await.unwrap();
        nats.flush().await.unwrap();
        let c = ch.clone();
        frame_where(&mut ws, Duration::from_secs(5), |v| {
            v["type"] == "status" && v["channel"] == c.as_str() && v["live"] == false && v["reason"] == "compute_stopping"
        })
        .await;
        // The compute didn't actually stop: the connection route answers, so a slot comes back.
        let c = ch.clone();
        frame_where(&mut ws, Duration::from_secs(90), |v| v["type"] == "resync" && v["channel"] == c.as_str()).await;
        let c = ch.clone();
        frame_where(&mut ws, Duration::from_secs(90), |v| v["type"] == "status" && v["channel"] == c.as_str() && v["live"] == true).await;
        live.rt.shutdown();
    })
    .await;
}

#[tokio::test]
#[ignore = "needs the workbench stack (just workbench test -p realtime --test realtime_live -- --run-ignored only)"]
async fn a_quiet_watched_database_still_advances_confirmed_flush_lsn() {
    with_database(|fx| async move {
        fx.psql(&fx.pg_database, "CREATE DATABASE rt_busy").await;
        fx.psql("rt_busy", "CREATE TABLE w (x int)").await;
        let live = live_one().await;
        let mut ws = live.socket(&fx.token).await;
        join_live(&mut ws, "1", join_in("1", &fx, &fx.pg_database, "public", "rt_items", None), Duration::from_secs(60)).await;
        let lsn_sql = format!(
            "SELECT confirmed_flush_lsn FROM pg_replication_slots WHERE slot_name LIKE 'rt\\_%' AND database = '{}'",
            fx.pg_database
        );
        let before = fx.psql(&fx.pg_database, &lsn_sql).await;
        assert!(!before.is_empty(), "the rt_ slot exists");
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            fx.psql("rt_busy", "INSERT INTO w SELECT generate_series(1, 20000)").await;
            let advanced = fx
                .psql(&fx.pg_database, &format!("SELECT pg_wal_lsn_diff(({lsn_sql}), '{before}') > 0"))
                .await;
            if advanced == "t" {
                break;
            }
            assert!(Instant::now() < deadline, "W8: confirmed_flush_lsn stuck at {before}");
            tokio::time::sleep(Duration::from_secs(3)).await;
        }
        live.rt.shutdown();
    })
    .await;
}

#[tokio::test]
#[ignore = "needs the workbench stack (just workbench test -p realtime --test realtime_live -- --run-ignored only)"]
async fn bigint_numeric_and_timestamptz_keys_match_studios_page_keys() {
    with_database(|fx| async move {
        // P6/W4: keys are the console session's own text output. A database-level TimeZone
        // applies to every login (authenticator's included); a role-level one applies only when
        // that role logs in, never under SET ROLE (the console's session), so both sides ignore it.
        fx.psql(
            &fx.pg_database,
            &format!(
                "ALTER DATABASE {} SET TimeZone = 'Asia/Tokyo'; \
                 CREATE TABLE rt_keys (b bigint, n numeric, ts timestamptz, PRIMARY KEY (b, n, ts)); \
                 CREATE ROLE rt_tz NOLOGIN; ALTER ROLE rt_tz SET TimeZone = 'America/Los_Angeles'; \
                 GRANT SELECT ON rt_keys TO rt_tz;",
                fx.pg_database
            ),
        )
        .await;
        let live = live_one().await;
        let mut owner = live.socket(&fx.token).await;
        let mut role = live.socket(&fx.token).await;
        let ch = join_live(&mut owner, "1", join_in("1", &fx, &fx.pg_database, "public", "rt_keys", None), Duration::from_secs(60)).await;
        join_live(&mut role, "1", join_in("1", &fx, &fx.pg_database, "public", "rt_keys", Some("rt_tz")), Duration::from_secs(60)).await;
        fx.psql(&fx.pg_database, "INSERT INTO rt_keys VALUES (9007199254740993, 1.50, '2026-01-02 03:04:05+00')").await;
        let (o, r) = tokio::join!(
            collect_rows(&mut owner, &ch, Duration::from_secs(8)),
            collect_rows(&mut role, &ch, Duration::from_secs(8)),
        );
        let owner_page = console_keys(&fx, &fx.pg_database, "public.rt_keys", None, &["b", "n", "ts"]).await;
        let role_page = console_keys(&fx, &fx.pg_database, "public.rt_keys", Some("rt_tz"), &["b", "n", "ts"]).await;
        assert_eq!(o.inserts.iter().cloned().collect::<Vec<_>>(), owner_page, "the owner's page keys");
        assert_eq!(r.inserts.iter().cloned().collect::<Vec<_>>(), role_page, "the role's page keys");
        let k = &role_page[0];
        assert!(k.contains("9007199254740993") && k.contains("\"1.50\""), "no precision lost: {k}");
        assert!(k.contains("12:04:05+09"), "the session's TimeZone, not the plugin's UTC: {k}");
        live.rt.shutdown();
    })
    .await;
}

#[tokio::test]
#[ignore = "needs the workbench stack (just workbench test -p realtime --test realtime_live -- --run-ignored only)"]
async fn a_big_transaction_streams_through_the_rt_slot() {
    with_database(|fx| async move {
        // The watcher watches `rt_big` itself (spec §3: it gets the big transaction as
        // `changed`), so the table exists before the join.
        fx.psql(&fx.pg_database, "CREATE TABLE rt_big (id int PRIMARY KEY, pad text);").await;
        let live = live_one().await;
        let mut ws = live.socket(&fx.token).await;
        let ch = join_live(&mut ws, "1", join_in("1", &fx, &fx.pg_database, "public", "rt_big", None), Duration::from_secs(60)).await;
        // S21 + K2: ~20+ MB of changes in one transaction; with logical_decoding_work_mem = 8MB
        // for chelabase_realtime the plugin's stream callbacks take it in chunks as it runs
        // (`stream_txns > 0`) and nothing spills (`spill_txns = 0`; the 64 MB default would do
        // neither). Only this database's slots count: `pg_stat_replication_slots` is
        // cluster-wide, and another database's slot on the same compute may spill.
        fx.psql(
            &fx.pg_database,
            "INSERT INTO rt_big SELECT g, repeat('x', 100) FROM generate_series(1, 200000) g;",
        )
        .await;
        let own_slots = "FROM pg_stat_replication_slots s \
                         JOIN pg_replication_slots r ON r.slot_name = s.slot_name \
                         WHERE r.database = current_database() AND r.slot_name LIKE 'rt\\_%'";
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            let streamed = fx
                .psql(
                    &fx.pg_database,
                    &format!("SELECT coalesce(sum(s.stream_txns), 0) > 0 {own_slots}"),
                )
                .await;
            if streamed == "t" {
                break;
            }
            assert!(Instant::now() < deadline, "no stream through the rt_ slot");
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        let spilled = fx
            .psql(
                &fx.pg_database,
                &format!("SELECT coalesce(sum(s.spill_txns), 0) {own_slots}"),
            )
            .await;
        assert_eq!(spilled.trim(), "0", "the rt_ slot streamed; it must not also spill");
        // The streamed transaction reaches the watcher once it commits: far past the plugin's
        // key budget, so `rt_big` comes as `changed` (keyless).
        let c = ch.clone();
        let rows = frame_where(&mut ws, Duration::from_secs(120), |v| {
            v["type"] == "rows"
                && v["channel"] == c.as_str()
                && v["changes"]
                    .as_array()
                    .is_some_and(|changes| changes.iter().any(|x| x["op"] == "changed"))
        })
        .await;
        assert_eq!(rows["channel"], ch.as_str(), "{rows}");
        live.rt.shutdown();
    })
    .await;
}

// No live `no_connect` test (coordinator ruling): the worker's member-roles run re-grants
// `CONNECT` to chelabase_realtime on every database but `postgres` and templates
// (`services/worker/src/member_roles.rs`, `grant_realtime_connect`), and a REVOKE through the
// proxy goes through the DDL hook, which can start such a run at any time, so no ordering
// against the first join can be proven. `slots::classify` (42501, 55000) and the source's
// `slot_errors_are_reported_and_retried` cover `no_connect` on fakes.

/// The owner's keyed delete of a timestamptz key under a database-level non-UTC TimeZone: the
/// key comes through `/visible`'s convert mode in the owner's session and equals the key the
/// owner's page had for that row.
#[tokio::test]
#[ignore = "needs the workbench stack (just workbench test -p realtime --test realtime_live -- --run-ignored only)"]
async fn the_owners_timestamptz_delete_key_matches_its_page_key() {
    with_database(|fx| async move {
        fx.psql(
            &fx.pg_database,
            &format!(
                "ALTER DATABASE {} SET TimeZone = 'Asia/Tokyo'; \
                 CREATE TABLE rt_when (at timestamptz PRIMARY KEY, note text); \
                 INSERT INTO rt_when VALUES ('2026-01-02 03:04:05+00', 'x');",
                fx.pg_database
            ),
        )
        .await;
        let page = console_keys(&fx, &fx.pg_database, "public.rt_when", None, &["at"]).await;
        assert_eq!(page.len(), 1, "{page:?}");
        assert!(page[0].contains("12:04:05+09"), "the owner's session prints Tokyo time: {page:?}");
        let live = live_one().await;
        let mut ws = live.socket(&fx.token).await;
        let ch = join_live(&mut ws, "1", join_in("1", &fx, &fx.pg_database, "public", "rt_when", None), Duration::from_secs(60)).await;
        fx.psql(&fx.pg_database, "DELETE FROM rt_when").await;
        let got = collect_rows(&mut ws, &ch, Duration::from_secs(8)).await;
        assert_eq!(
            got.deletes.into_iter().collect::<Vec<_>>(),
            page,
            "keyed, in the page's form (not the plugin's +00): keyless {}",
            got.keyless_deletes
        );
        live.rt.shutdown();
    })
    .await;
}

/// One role of a generated live case.
#[derive(Debug, Clone)]
struct LiveRole {
    usage: bool,
    /// 0 none, 1 the table, 2 the key column only, 3 another column only.
    select: u8,
    bypassrls: bool,
    /// A role-level `search_path` (`ALTER ROLE … SET`): applies only when the role itself logs
    /// in, never under the console's `SET ROLE`, so both sides must ignore it alike.
    role_path_b: bool,
}

#[derive(Debug, Clone)]
struct LiveCase {
    roles: Vec<LiveRole>,
    rls: bool,
    /// 0 `tag = 1`, 1 `extract(hour FROM ts) < 12` (TimeZone), 2 `m.allowed(id)` (a plpgsql
    /// function reading an unqualified `allow_list` through the session's `search_path`).
    policy: u8,
    /// The database-level `TimeZone` (hours east of UTC) and `search_path` (`a` or `b`).
    db_tz: i8,
    db_path_b: bool,
    rows: Vec<(u32, u8, u8)>,
    /// (0 upsert | 1 update | 2 delete, id, tag, hour UTC)
    changes: Vec<(u8, u32, u8, u8)>,
}

fn live_cases() -> Vec<LiveCase> {
    use proptest::prelude::*;
    use proptest::strategy::ValueTree;
    let role = (any::<bool>(), 0u8..4, any::<bool>(), any::<bool>()).prop_map(
        |(usage, select, bypassrls, role_path_b)| LiveRole {
            usage,
            select,
            bypassrls,
            role_path_b,
        },
    );
    let case = (
        proptest::collection::vec(role, 3),
        any::<bool>(),
        0u8..3,
        -11i8..=12,
        any::<bool>(),
        proptest::collection::vec((0u32..8, 0u8..3, 0u8..24), 0..6),
        proptest::collection::vec((0u8..3, 0u32..8, 0u8..3, 0u8..24), 1..8),
    )
        .prop_map(|(roles, rls, policy, db_tz, db_path_b, rows, changes)| LiveCase {
            roles,
            rls,
            policy,
            db_tz,
            db_path_b,
            rows,
            changes,
        });
    let mut runner = proptest::test_runner::TestRunner::deterministic();
    (0..3)
        .map(|_| case.new_tree(&mut runner).unwrap().current())
        .collect()
}

/// The §4 invariant against real Postgres (the fake run is `tests/model.rs`): for generated
/// schema `USAGE`, table and column grants, `BYPASSRLS`, RLS policies that depend on the
/// session's TimeZone and `search_path` (database- and role-level `SET`s), each role's received
/// keys equal its own console read of the changed rows; a join is refused exactly when that
/// read fails.
#[tokio::test]
#[ignore = "needs the workbench stack (just workbench test -p realtime --test realtime_live -- --run-ignored only)"]
async fn the_invariant_holds_against_postgres() {
    with_database(|fx| async move {
        let live = live_one().await;
        for (n, case) in live_cases().into_iter().enumerate() {
            // A fresh database per case: its database-level settings are in place before any
            // pool (realtime's, rest's) connects to it.
            let db = format!("rt_m{n}");
            served_database_with(
                &fx,
                &db,
                &format!(
                    "ALTER DATABASE {db} SET TimeZone = 'Etc/GMT{tz:+}'; \
                     ALTER DATABASE {db} SET search_path = \"$user\", public, {path};",
                    // POSIX sign: Etc/GMT-9 is UTC+9.
                    tz = -(case.db_tz as i32),
                    path = if case.db_path_b { "b" } else { "a" },
                ),
            )
            .await;
            let mut sql = String::from(
                "CREATE SCHEMA m; CREATE SCHEMA a; CREATE SCHEMA b; \
                 CREATE TABLE a.allow_list (id int); CREATE TABLE b.allow_list (id int); \
                 INSERT INTO a.allow_list VALUES (1), (2), (3); INSERT INTO b.allow_list VALUES (4), (5), (6); \
                 CREATE FUNCTION m.allowed(i int) RETURNS boolean LANGUAGE plpgsql STABLE AS $$ BEGIN RETURN EXISTS (SELECT 1 FROM allow_list l WHERE l.id = i); END $$; \
                 CREATE TABLE m.t (id int PRIMARY KEY, tag int, ts timestamptz); ",
            );
            for (id, tag, hour) in &case.rows {
                sql.push_str(&format!(
                    "INSERT INTO m.t VALUES ({id}, {tag}, '2026-01-02 {hour:02}:00:00+00') ON CONFLICT DO NOTHING; "
                ));
            }
            if case.rls {
                let using = match case.policy {
                    0 => "tag = 1",
                    1 => "extract(hour FROM ts) < 12",
                    _ => "m.allowed(id)",
                };
                sql.push_str(&format!(
                    "ALTER TABLE m.t ENABLE ROW LEVEL SECURITY; CREATE POLICY p ON m.t FOR SELECT USING ({using}); "
                ));
            }
            for (i, r) in case.roles.iter().enumerate() {
                let role = format!("rt_m{n}_r{i}");
                sql.push_str(&format!(
                    "CREATE ROLE {role} NOLOGIN{}; GRANT USAGE ON SCHEMA a, b TO {role}; \
                     GRANT SELECT ON a.allow_list, b.allow_list TO {role}; \
                     ALTER ROLE {role} SET search_path = {}, public; ",
                    if r.bypassrls { " BYPASSRLS" } else { "" },
                    if r.role_path_b { "b" } else { "a" },
                ));
                if r.usage {
                    sql.push_str(&format!("GRANT USAGE ON SCHEMA m TO {role}; "));
                }
                sql.push_str(&match r.select {
                    1 => format!("GRANT SELECT ON m.t TO {role}; "),
                    2 => format!("GRANT SELECT (id) ON m.t TO {role}; "),
                    3 => format!("GRANT SELECT (tag) ON m.t TO {role}; "),
                    _ => String::new(),
                });
            }
            fx.psql(&db, &sql).await;

            // Each role joins, or is refused exactly when its own read fails.
            let mut sockets = vec![];
            for i in 0..case.roles.len() {
                let role = format!("rt_m{n}_r{i}");
                let mut ws = live.socket(&fx.token).await;
                send(&mut ws, join_in("1", &fx, &db, "m", "t", Some(&role))).await;
                let joined = reply(&mut ws, "1").await;
                let (status, read) = api(
                    &fx.token,
                    reqwest::Method::POST,
                    &format!("{}/console/query", fx.branch()),
                    Some(json!({"role": role, "database": db, "tree": {"v": 1, "op": "select", "from": "m.t", "select": [{"text_of": "id", "as": "$pk0"}]}})),
                )
                .await;
                assert_eq!(
                    joined["type"] == "joined",
                    status == 200,
                    "case {n} role {i} {:?}: join {joined} vs read {status} {read}",
                    case.roles[i]
                );
                if joined["type"] == "joined" {
                    let ch = joined["channel"].as_str().unwrap().to_string();
                    if joined["info"]["live"] != true {
                        let c = ch.clone();
                        frame_where(&mut ws, Duration::from_secs(60), |v| {
                            v["type"] == "status" && v["channel"] == c.as_str() && v["live"] == true
                        })
                        .await;
                    }
                    sockets.push((role, ws, ch));
                }
            }
            let mut touched = std::collections::BTreeSet::new();
            let mut script = String::new();
            for (op, id, tag, hour) in &case.changes {
                touched.insert(key(&[&id.to_string()]));
                script.push_str(&match op {
                    0 => format!("INSERT INTO m.t VALUES ({id}, {tag}, '2026-01-02 {hour:02}:00:00+00') ON CONFLICT (id) DO UPDATE SET tag = EXCLUDED.tag, ts = EXCLUDED.ts; "),
                    1 => format!("UPDATE m.t SET tag = {tag}, ts = '2026-01-02 {hour:02}:00:00+00' WHERE id = {id}; "),
                    _ => format!("DELETE FROM m.t WHERE id = {id}; "),
                });
            }
            fx.psql(&db, &script).await;
            for (role, ws, ch) in sockets.iter_mut() {
                let got = collect_rows(ws, ch, Duration::from_secs(6)).await;
                let page: std::collections::BTreeSet<String> =
                    console_keys(&fx, &db, "m.t", Some(role.as_str()), &["id"])
                        .await
                        .into_iter()
                        .collect();
                let received: std::collections::BTreeSet<String> =
                    got.inserts.union(&got.updates).cloned().collect();
                // Sound: nothing outside the role's own read. Complete unless told to refetch.
                assert!(
                    received.is_subset(&page),
                    "case {n} {role}: received {received:?} outside its read {page:?} ({case:?})"
                );
                if got.changed == 0 {
                    let expected: std::collections::BTreeSet<String> =
                        page.intersection(&touched).cloned().collect();
                    assert_eq!(received, expected, "case {n} {role}: {got:?} ({case:?})");
                }
            }
        }
        live.rt.shutdown();
    })
    .await;
}

// ============ RT1 browser fix round (fr): a watcher's status converges to the holder's ============
//
// The browser pass saw every `table:` join answer `{live: false, reason: "waiting"}` and no
// `status` follow while rows streamed. The bound: the per-database status re-publish (every 5 s,
// `realtime::lease::HEARTBEAT`) plus 3 s for the stack (the fakes' `fr_status.rs` uses 1 s).

const CONVERGE: Duration = Duration::from_secs(8);

/// Sends `join` and reads frames for `within`: whether its channel showed live (in
/// `joined.info` or a later `status`), the `joined` frame, and the channel's `status` frames.
async fn told_live_within(ws: &mut Socket, r: &str, join: Value, within: Duration) -> (bool, Value, Vec<Value>) {
    send(ws, join).await;
    let joined = reply(ws, r).await;
    assert_eq!(joined["type"], "joined", "{joined}");
    if joined["info"]["live"] == true {
        return (true, joined, vec![]);
    }
    let channel = joined["channel"].as_str().unwrap().to_string();
    let deadline = Instant::now() + within;
    let mut statuses = vec![];
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return (false, joined, statuses);
        }
        match tokio::time::timeout(left, ws.next()).await {
            Err(_) => return (false, joined, statuses),
            Ok(Some(Ok(Message::Text(t)))) => {
                let v: Value = serde_json::from_str(t.as_str()).unwrap();
                if v["type"] == "status" && v["channel"] == channel.as_str() {
                    let live = v["live"] == true;
                    statuses.push(v);
                    if live {
                        return (true, joined, statuses);
                    }
                }
            }
            Ok(Some(Ok(Message::Close(f)))) => panic!("closed: {f:?}"),
            Ok(Some(Ok(_))) => continue,
            Ok(other) => panic!("socket ended: {other:?}"),
        }
    }
}

#[tokio::test]
#[ignore = "needs the workbench stack (just workbench test -p realtime --test realtime_live -- --run-ignored only)"]
async fn fr_a_viewer_rejoining_a_streaming_table_is_told_live() {
    with_database(|fx| async move {
        let live = live_one().await;
        let mut first = live.socket(&fx.token).await;
        let ch = join_live(&mut first, "1", join_in("1", &fx, &fx.pg_database, "public", "rt_items", None), Duration::from_secs(60)).await;
        send(&mut first, json!({"type": "leave", "ref": "2", "channel": ch})).await;
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert_eq!(rt_slots(&fx, &fx.pg_database).await, 1, "precondition: the slot still streams (30 s idle)");
        let mut second = live.socket(&fx.token).await;
        let (told, joined, statuses) =
            told_live_within(&mut second, "1", join_in("1", &fx, &fx.pg_database, "public", "rt_items", None), CONVERGE).await;
        assert!(
            told,
            "a viewer joining a table whose slot streams is told live within {CONVERGE:?}: joined {joined}, then {statuses:?}"
        );
        live.rt.shutdown();
    })
    .await;
}

#[tokio::test]
#[ignore = "needs the workbench stack (just workbench test -p realtime --test realtime_live -- --run-ignored only)"]
async fn fr_a_second_table_on_a_streaming_database_is_told_live() {
    with_database(|fx| async move {
        fx.psql(&fx.pg_database, "CREATE TABLE rt_other (id int PRIMARY KEY, label text)").await;
        let live = live_one().await;
        let mut ws = live.socket(&fx.token).await;
        let items = join_live(&mut ws, "1", join_in("1", &fx, &fx.pg_database, "public", "rt_items", None), Duration::from_secs(60)).await;
        // While the first table's viewer stays.
        let (told, joined, statuses) =
            told_live_within(&mut ws, "2", join_in("2", &fx, &fx.pg_database, "public", "rt_other", None), CONVERGE).await;
        assert!(told, "a second table while the first watches: joined {joined}, then {statuses:?}");
        let other = joined["channel"].as_str().unwrap().to_string();
        // After both left (the slot keeps streaming for 30 s).
        send(&mut ws, json!({"type": "leave", "ref": "3", "channel": items})).await;
        send(&mut ws, json!({"type": "leave", "ref": "4", "channel": other})).await;
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert_eq!(rt_slots(&fx, &fx.pg_database).await, 1, "precondition: the slot still streams (30 s idle)");
        let mut second = live.socket(&fx.token).await;
        let (told, joined, statuses) =
            told_live_within(&mut second, "1", join_in("1", &fx, &fx.pg_database, "public", "rt_other", None), CONVERGE).await;
        assert!(
            told,
            "a table of a database whose slot streams is told live within {CONVERGE:?}: joined {joined}, then {statuses:?}"
        );
        live.rt.shutdown();
    })
    .await;
}
