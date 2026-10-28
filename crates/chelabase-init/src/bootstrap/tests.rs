//! The whole bootstrap against fakes: a stateful Zitadel, metadata database and orchestrator
//! probe that record one shared event log. Nothing here reaches a real service.

use super::outputs;
use super::retry::{Backoff, Check};
use super::tenant::{Bind, Metadata, bind_decision, refused};
use super::zitadel::{ApiError, ZitadelApi};
use super::*;
use anyhow::Result;
use chelabase_types::mode::Mode;
use reqwest::Method;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use uuid::Uuid;

const AUTH_HOST: &str = "auth.chelabase.localhost:8000";
const ORIGIN: &str = "http://studio.chelabase.localhost:8000";
const HUMAN: &str = "zitadel-admin@zitadel.auth.chelabase.localhost";
const TENANT: Uuid = Uuid::from_u128(0x0000_0000_0000_4000_8000_0000_0000_0001);

type Log = Arc<Mutex<Vec<String>>>;

fn push(log: &Log, event: impl Into<String>) {
    log.lock().unwrap().push(event.into());
}

/// Zitadel's objects, as the fake keeps them.
#[derive(Debug, Clone, PartialEq, Default)]
struct ZState {
    projects: BTreeMap<String, String>,
    apps: Vec<Value>,
    login_v2_required: bool,
    allow_register: bool,
    /// id -> (username, human, email, state)
    users: BTreeMap<String, (String, bool, String, String)>,
    keys: Vec<String>,
    smtp: Vec<(String, String, bool)>,
    /// Each SMTP configuration's full settings, the password included (by id).
    smtp_config: BTreeMap<String, Value>,
    /// The instance's IAM members (user ids), and how many more the listing says exist.
    iam: std::collections::BTreeSet<String>,
    iam_unlisted: u32,
    /// IAM members listed without a `userType`.
    iam_untyped: std::collections::BTreeSet<String>,
    targets: Vec<(String, String, String)>,
    execution: Option<Value>,
}

struct FakeZitadel {
    state: Mutex<ZState>,
    next: AtomicU32,
    log: Log,
    /// Probes that answer 503 before the probe answers ready.
    not_ready: AtomicU32,
    /// Errors to answer, per call key, before calling through.
    faults: Mutex<HashMap<String, Vec<ApiError>>>,
}

impl FakeZitadel {
    fn new(log: &Log) -> Self {
        let mut state = ZState {
            login_v2_required: true,
            allow_register: true,
            ..ZState::default()
        };
        // FirstInstance's human and machine users.
        state.users.insert(
            "u-human".into(),
            (HUMAN.into(), true, HUMAN.into(), "USER_STATE_ACTIVE".into()),
        );
        state.users.insert(
            "u-bootstrap".into(),
            (
                "chelabase-bootstrap".into(),
                false,
                String::new(),
                "USER_STATE_ACTIVE".into(),
            ),
        );
        state.iam = ["u-human".to_string(), "u-bootstrap".to_string()]
            .into_iter()
            .collect();
        Self {
            state: Mutex::new(state),
            next: AtomicU32::new(1),
            log: log.clone(),
            not_ready: AtomicU32::new(0),
            faults: Mutex::default(),
        }
    }

    fn fault(&self, key: &str, err: ApiError, times: usize) {
        self.faults
            .lock()
            .unwrap()
            .insert(key.to_string(), vec![err; times]);
    }

    fn state(&self) -> ZState {
        self.state.lock().unwrap().clone()
    }

    fn id(&self, prefix: &str) -> String {
        format!("{prefix}{}", self.next.fetch_add(1, Ordering::SeqCst))
    }

    fn calls(&self, key: &str) -> usize {
        self.log
            .lock()
            .unwrap()
            .iter()
            .filter(|e| *e == &format!("zitadel {key}"))
            .count()
    }

    fn handle(&self, key: &str, body: &Value) -> Result<Value> {
        push(&self.log, format!("zitadel {key}"));
        if let Some(errs) = self.faults.lock().unwrap().get_mut(key)
            && !errs.is_empty()
        {
            return Err(errs.remove(0).into());
        }
        let mut s = self.state.lock().unwrap();
        let answer = match key {
            "POST /v2/organizations/_search" => {
                json!({"result": [{"id": "org-1", "name": "Acme"}]})
            }
            "ProjectService/ListProjects" => {
                let name = body
                    .pointer("/filters/0/projectNameFilter/projectName")
                    .unwrap();
                let found: Vec<Value> = s
                    .projects
                    .iter()
                    .filter(|(_, n)| *n == name)
                    .map(|(id, _)| json!({"projectId": id}))
                    .collect();
                json!({"projects": found})
            }
            "ProjectService/CreateProject" => {
                let id = self.id("p");
                s.projects
                    .insert(id.clone(), body["name"].as_str().unwrap().into());
                json!({"projectId": id})
            }
            "ApplicationService/ListApplications" => json!({"applications": s.apps}),
            "ApplicationService/CreateApplication" => {
                let (id, client) = (self.id("a"), self.id("c"));
                let mut app = json!({"applicationId": id, "name": body["name"]});
                if let Some(oidc) = body.get("oidcConfiguration") {
                    let mut c = oidc.clone();
                    c["clientId"] = json!(client);
                    app["oidcConfiguration"] = c;
                } else {
                    app["apiConfiguration"] = json!({"clientId": client});
                }
                s.apps.push(app.clone());
                app
            }
            "ApplicationService/UpdateApplication" => json!({}),
            "FeatureService/GetInstanceFeatures" => {
                json!({"loginV2": {"required": s.login_v2_required, "baseUri": format!("{ORIGIN}/")}})
            }
            "FeatureService/SetInstanceFeatures" => {
                s.login_v2_required = body.pointer("/loginV2/required") == Some(&json!(true));
                json!({})
            }
            "GET /admin/v1/policies/login" => {
                json!({"policy": {"allowRegister": s.allow_register, "isDefault": true}})
            }
            "PUT /admin/v1/policies/login" => {
                s.allow_register = body["allowRegister"] == json!(true);
                json!({})
            }
            "GET /management/v1/policies/login" => {
                json!({"policy": {"allowRegister": s.allow_register, "isDefault": true}})
            }
            "POST /v2/users" => {
                let q = &body["queries"];
                let username = q
                    .pointer("/0/userNameQuery/userName")
                    .and_then(Value::as_str);
                let email = q
                    .pointer("/1/emailQuery/emailAddress")
                    .and_then(Value::as_str);
                let human_only = q.pointer("/1/typeQuery/type") == Some(&json!("TYPE_HUMAN"));
                let found: Vec<Value> = s
                    .users
                    .iter()
                    .filter(|(_, (u, h, e, _))| {
                        username.is_none_or(|n| n == u)
                            && email.is_none_or(|m| m.eq_ignore_ascii_case(e))
                            && (!human_only || *h)
                    })
                    .map(|(id, (_, _, _, st))| json!({"userId": id, "state": st}))
                    .collect();
                json!({"result": found})
            }
            "POST /v2/users/new" => {
                let id = self.id("u");
                s.users.insert(
                    id.clone(),
                    (
                        body["username"].as_str().unwrap().into(),
                        false,
                        String::new(),
                        "USER_STATE_ACTIVE".into(),
                    ),
                );
                json!({"id": id})
            }
            "POST /admin/v1/smtp/_search" => {
                json!({"result": s.smtp.iter().map(|(id, host, active)| {
                    // As Zitadel lists it: no password, and `tls: false` left out.
                    let mut c = s.smtp_config.get(id).cloned().unwrap_or_else(|| json!({}));
                    let o = c.as_object_mut().unwrap();
                    o.remove("password");
                    if o.get("tls") == Some(&json!(false)) {
                        o.remove("tls");
                    }
                    o.insert("id".into(), json!(id));
                    o.insert("host".into(), json!(host));
                    o.insert("state".into(), json!(if *active { "SMTP_CONFIG_ACTIVE" } else { "SMTP_CONFIG_INACTIVE" }));
                    c
                }).collect::<Vec<_>>()})
            }
            "POST /admin/v1/smtp" => {
                let id = self.id("s");
                s.smtp
                    .push((id.clone(), body["host"].as_str().unwrap().into(), false));
                s.smtp_config.insert(id.clone(), body.clone());
                json!({"id": id})
            }
            "POST /admin/v1/members/_search" => {
                let result: Vec<Value> = s
                    .iam
                    .iter()
                    .map(|id| {
                        let human = s.users[id].1;
                        let mut m = json!({"userId": id, "roles": ["IAM_OWNER"],
                            "userType": if human { "TYPE_HUMAN" } else { "TYPE_MACHINE" }});
                        if s.iam_untyped.contains(id) {
                            m.as_object_mut().unwrap().remove("userType");
                        }
                        m
                    })
                    .collect();
                let total = result.len() as u32 + s.iam_unlisted;
                json!({"details": {"totalResult": total.to_string()}, "result": result})
            }
            "POST /v2/actions/targets/search" => {
                json!({"targets": s.targets.iter().map(|(id, name, ep)| json!({
                "id": id, "name": name, "endpoint": ep})).collect::<Vec<_>>()})
            }
            "POST /v2/actions/targets" => {
                let id = self.id("t");
                s.targets.push((
                    id.clone(),
                    body["name"].as_str().unwrap().into(),
                    body["endpoint"].as_str().unwrap().into(),
                ));
                json!({"id": id, "signingKey": format!("sk-{id}")})
            }
            "PUT /v2/actions/executions" => {
                s.execution = Some(body.clone());
                json!({})
            }
            other => {
                if let Some(rest) = other.strip_prefix("POST /v2/users/") {
                    let (id, what) = rest.split_once('/').unwrap();
                    let Some(user) = s.users.get_mut(id) else {
                        return Err(ApiError::new(other, Some(404), "user not found").into());
                    };
                    match what {
                        "deactivate" => {
                            assert_ne!(
                                user.3, "USER_STATE_INACTIVE",
                                "Zitadel refuses deactivating twice"
                            );
                            user.3 = "USER_STATE_INACTIVE".into();
                            json!({})
                        }
                        "keys" => {
                            use base64::Engine;
                            let key = self.id("k");
                            s.keys.push(key.clone());
                            let file = json!({"type": "serviceaccount", "keyId": key, "key": "PEM", "userId": id});
                            json!({"keyId": key, "keyContent": base64::engine::general_purpose::STANDARD.encode(file.to_string())})
                        }
                        _ => panic!("unexpected call {other}"),
                    }
                } else if let Some(id) = other
                    .strip_prefix("POST /admin/v1/smtp/")
                    .and_then(|r| r.strip_suffix("/_activate"))
                {
                    for c in &mut s.smtp {
                        c.2 = c.0 == id;
                    }
                    json!({})
                } else if let Some(id) = other
                    .strip_prefix("PUT /admin/v1/smtp/")
                    .and_then(|r| r.strip_suffix("/password"))
                {
                    s.smtp_config.get_mut(id).unwrap()["password"] = body["password"].clone();
                    json!({})
                } else if let Some(id) = other.strip_prefix("PUT /admin/v1/smtp/") {
                    let c = s.smtp_config.get_mut(id).unwrap();
                    for (k, v) in body.as_object().unwrap() {
                        if k != "password" {
                            c[k] = v.clone();
                        }
                    }
                    json!({})
                } else if let Some(id) = other.strip_prefix("GET /v2/users/") {
                    match s.users.get(id) {
                        Some(u) if u.1 => {
                            json!({"user": {"userId": id, "state": u.3, "human": {}}})
                        }
                        Some(u) => json!({"user": {"userId": id, "state": u.3, "machine": {}}}),
                        None => {
                            return Err(
                                ApiError::new(other, Some(404), "Errors.User.NotFound").into()
                            );
                        }
                    }
                } else if let Some(id) = other.strip_prefix("DELETE /v2/actions/targets/") {
                    s.targets.retain(|t| t.0 != id);
                    json!({})
                } else {
                    panic!("unexpected call {other}")
                }
            }
        };
        Ok(answer)
    }
}

impl ZitadelApi for FakeZitadel {
    async fn rest(&self, method: Method, path: &str, body: Value) -> Result<Value> {
        self.handle(&format!("{method} {path}"), &body)
    }
    async fn connect(&self, service: &str, method: &str, body: Value) -> Result<Value> {
        self.handle(
            &format!("{}/{method}", service.rsplit('.').next().unwrap()),
            &body,
        )
    }
    async fn ready(&self) -> Result<(), ApiError> {
        push(&self.log, "zitadel ready?");
        let left = self.not_ready.load(Ordering::SeqCst);
        if left > 0 {
            self.not_ready.store(left - 1, Ordering::SeqCst);
            return Err(ApiError::new("GET /debug/ready", Some(503), ""));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
struct MState {
    org: Option<String>,
    name: Option<String>,
    admins: Vec<(String, Option<String>)>,
}

struct FakeMetadata {
    state: Mutex<MState>,
    log: Log,
    /// `migrated` answers "not yet" this many times first.
    unmigrated: AtomicU32,
}

impl FakeMetadata {
    fn new(log: &Log) -> Self {
        Self {
            state: Mutex::default(),
            log: log.clone(),
            unmigrated: AtomicU32::new(0),
        }
    }
    fn state(&self) -> MState {
        self.state.lock().unwrap().clone()
    }
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
    async fn bind_org(&self, tenant: Uuid, org: &str) -> Result<bool> {
        push(&self.log, format!("metadata bind {org}"));
        let mut s = self.state.lock().unwrap();
        match bind_decision(s.org.as_deref(), org) {
            Bind::Set => {
                s.org = Some(org.into());
                Ok(true)
            }
            Bind::Keep => Ok(false),
            Bind::Refuse { bound } => Err(refused(tenant, &bound, org)),
        }
    }
    async fn fill_display_name(&self, _: Uuid, name: &str) -> Result<bool> {
        let mut s = self.state.lock().unwrap();
        if s.name.is_some() {
            return Ok(false);
        }
        s.name = Some(name.into());
        Ok(true)
    }
    async fn bootstrap_admin(&self, _: Uuid, zuid: &str, email: Option<&str>) -> Result<()> {
        push(&self.log, format!("metadata admin {zuid}"));
        let mut s = self.state.lock().unwrap();
        let entry = (zuid.to_string(), email.map(str::to_string));
        if !s.admins.contains(&entry) {
            s.admins.push(entry);
        }
        Ok(())
    }
}

/// The orchestrator: unreachable for `down` probes, then up.
struct FakeOrchestrator {
    log: Log,
    down: AtomicU32,
    /// When set, never comes up.
    never: bool,
}

impl FakeOrchestrator {
    fn new(log: &Log, down: u32) -> Self {
        Self {
            log: log.clone(),
            down: AtomicU32::new(down),
            never: false,
        }
    }
}

impl Probe for FakeOrchestrator {
    fn name(&self) -> String {
        "the orchestrator at http://orchestrator:8083/health".into()
    }
    async fn check(&self) -> Result<Check<()>> {
        push(&self.log, "orchestrator ready?");
        let left = self.down.load(Ordering::SeqCst);
        if self.never || left > 0 {
            self.down.store(left.saturating_sub(1), Ordering::SeqCst);
            return Ok(Check::Wait("connection refused".into()));
        }
        Ok(Check::Ready(()))
    }
}

/// A test world: the fakes, a private output dir, and a config pointing at them.
struct World {
    log: Log,
    zitadel: FakeZitadel,
    metadata: FakeMetadata,
    orchestrator: FakeOrchestrator,
    root: tempfile::TempDir,
    cfg: Config,
}

impl World {
    fn new() -> Self {
        let log: Log = Arc::default();
        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let out = root.path().join("out");
        std::fs::create_dir(&out).unwrap();
        std::fs::set_permissions(&out, std::fs::Permissions::from_mode(0o700)).unwrap();
        let cfg = Config {
            out_dir: out,
            zitadel_upstream: "http://zitadel:8080".into(),
            auth_host: AUTH_HOST.into(),
            studio_origin: ORIGIN.into(),
            mode: Mode::SelfHosted,
            admin_pat_file: root.path().join("admin.pat"),
            metadata_url: "postgres://chelabase@metadata-db/chelabase_metadata".into(),
            orchestrator_health: Some("http://orchestrator:8083/health".into()),
            actions_endpoint: "http://orchestrator:8083/internal/zitadel/events".into(),
            smtp: None,
            admin_email: None,
            tenant: TENANT,
            timeout: Duration::from_secs(10),
            backoff: Backoff {
                first: Duration::from_millis(1),
                max: Duration::from_millis(2),
            },
        };
        World {
            zitadel: FakeZitadel::new(&log),
            metadata: FakeMetadata::new(&log),
            orchestrator: FakeOrchestrator::new(&log, 0),
            log,
            root,
            cfg,
        }
    }

    async fn run(&self, demo: Demo) -> Result<()> {
        run_with(
            &self.cfg,
            demo,
            &self.zitadel,
            &self.metadata,
            &self.orchestrator,
        )
        .await
    }

    fn out(&self, name: &str) -> Option<String> {
        std::fs::read_to_string(self.cfg.out_dir.join(name)).ok()
    }

    fn log(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }

    fn first(&self, prefix: &str) -> Option<usize> {
        self.log().iter().position(|e| e.starts_with(prefix))
    }

    /// Every file under the root: path -> (bytes, mode).
    fn files(&self) -> BTreeMap<std::path::PathBuf, (Vec<u8>, u32)> {
        let mut out = BTreeMap::new();
        let mut dirs = vec![self.root.path().to_path_buf()];
        while let Some(d) = dirs.pop() {
            for e in std::fs::read_dir(&d).unwrap() {
                let p = e.unwrap().path();
                let meta = std::fs::symlink_metadata(&p).unwrap();
                if meta.is_dir() {
                    dirs.push(p);
                } else if !p.ends_with(outputs::LOCK) {
                    let bytes = std::fs::read(&p).unwrap_or_default();
                    out.insert(p, (bytes, meta.permissions().mode() & 0o7777));
                }
            }
        }
        out
    }
}

fn human_state(w: &World) -> String {
    w.zitadel.state().users["u-human"].3.clone()
}

// AC 1: started before the orchestrator, it keeps waiting (the metadata migrations, the
// orchestrator's probe, the Actions target's host) and finishes once it is up, in one run.
#[tokio::test]
async fn retries_until_orchestrator_ready() {
    let w = World::new();
    w.zitadel.not_ready.store(2, Ordering::SeqCst);
    w.metadata.unmigrated.store(3, Ordering::SeqCst);
    w.orchestrator.down.store(4, Ordering::SeqCst);
    // Zitadel refuses a target whose host doesn't resolve until the orchestrator exists.
    w.zitadel.fault(
        "POST /v2/actions/targets",
        ApiError::new(
            "POST /v2/actions/targets",
            Some(400),
            r#"{"code":3,"message":"Errors.Target.DeniedURL"}"#,
        ),
        2,
    );
    w.run(Demo::Off).await.unwrap();
    let log = w.log();
    assert_eq!(
        log.iter().filter(|e| *e == "orchestrator ready?").count(),
        5,
        "{log:?}"
    );
    assert_eq!(
        log.iter().filter(|e| *e == "metadata migrated?").count(),
        4,
        "{log:?}"
    );
    assert_eq!(w.zitadel.calls("POST /v2/actions/targets"), 3);
    assert_eq!(w.metadata.state().org.as_deref(), Some("org-1"));
    let target = &w.zitadel.state().targets[0].0;
    assert_eq!(
        w.out(outputs::ACTIONS_SIGNING_KEY),
        Some(format!("sk-{target}\n"))
    );
    assert_eq!(w.out(outputs::ORG_ID).as_deref(), Some("org-1\n"));
}

/// Whether a logged Zitadel call writes (the execution's idempotent PUT aside: it sets the
/// same value again, and the state comparison covers it).
fn is_write(e: &str) -> bool {
    let Some(call) = e.strip_prefix("zitadel ") else {
        // The metadata side is compared by state: `bootstrap_admin` writes nothing on a repeat.
        return false;
    };
    [
        "Create",
        "Update",
        "SetInstance",
        "/new",
        "/keys",
        "/deactivate",
        "DELETE ",
        "PUT /admin",
        "PUT /management",
        "_activate",
    ]
    .iter()
    .any(|w| call.contains(w))
        || call == "POST /admin/v1/smtp"
        || (call.starts_with("POST /v2/actions/targets") && !call.ends_with("/search"))
}

// AC 2: a finished bootstrap run again changes nothing in Zitadel, metadata or the output dir.
#[tokio::test]
async fn rerun_is_noop() {
    for demo in [Demo::Off, Demo::On] {
        let w = World::new();
        w.run(demo).await.unwrap();
        let (z, m, files) = (w.zitadel.state(), w.metadata.state(), w.files());
        let before = w.log().len();
        w.run(demo).await.unwrap();
        assert_eq!(w.zitadel.state(), z, "{demo:?}: Zitadel changed");
        assert_eq!(w.metadata.state(), m, "{demo:?}: metadata changed");
        assert_eq!(w.files(), files, "{demo:?}: the output dir changed");
        let writes: Vec<String> = w.log()[before..]
            .iter()
            .filter(|e| is_write(e))
            .cloned()
            .collect();
        assert!(writes.is_empty(), "{demo:?}: writes on a rerun: {writes:?}");
    }
}

// AC 3: no `--demo`: no demo machine user, no demo-sa.json, no SMTP, no tenant admin.
#[tokio::test]
async fn no_demo_creates_no_demo_objects() {
    let w = World::new();
    w.run(Demo::Off).await.unwrap();
    let z = w.zitadel.state();
    assert!(
        !z.users.values().any(|u| u.0 == steps::DEMO_USER),
        "{:?}",
        z.users
    );
    assert!(z.keys.is_empty(), "no machine key: {:?}", z.keys);
    assert!(z.smtp.is_empty(), "no SMTP: {:?}", z.smtp);
    assert!(
        w.metadata.state().admins.is_empty(),
        "the claim decides the admin"
    );
    for name in [outputs::DEMO_KEY, outputs::DEMO_USER_ID] {
        assert!(w.out(name).is_none(), "{name} written");
    }
    // The product's objects are all there.
    assert_eq!(z.projects.len(), 1);
    assert_eq!(z.apps.len(), 2);
    assert!(!z.allow_register && !z.login_v2_required);
    assert_eq!(z.targets.len(), 1);
    for name in [
        outputs::ORG_ID,
        outputs::PROJECT_ID,
        outputs::API_CLIENT_ID,
        outputs::STUDIO_CLIENT_ID,
        outputs::ACTIONS_SIGNING_KEY,
    ] {
        assert!(w.out(name).is_some(), "{name} missing");
    }

    // `--demo` adds today's workbench objects: the demo user and its key, Mailpit, the admins.
    let d = World::new();
    d.run(Demo::On).await.unwrap();
    let z = d.zitadel.state();
    let demo_id = d.out(outputs::DEMO_USER_ID).unwrap().trim().to_string();
    assert_eq!(z.users[&demo_id].0, steps::DEMO_USER);
    assert_eq!(z.keys.len(), 1);
    assert_eq!(z.smtp.len(), 1);
    assert_eq!((z.smtp[0].1.as_str(), z.smtp[0].2), ("mailpit:1025", true));
    let key: Value = serde_json::from_str(&d.out(outputs::DEMO_KEY).unwrap()).unwrap();
    assert_eq!(key["userId"], json!(demo_id));
    assert_eq!(
        d.metadata.state().admins,
        [
            ("u-human".to_string(), Some(HUMAN.to_string())),
            (demo_id, None)
        ]
    );
}

// AC 3: no `--demo`: the FirstInstance human (an IAM owner) can't sign in; `--demo` keeps it,
// the workbench's sign-in.
#[tokio::test]
async fn first_human_locked_without_demo() {
    let w = World::new();
    w.run(Demo::Off).await.unwrap();
    assert_eq!(human_state(&w), "USER_STATE_INACTIVE");
    // A rerun leaves it inactive and doesn't deactivate it again.
    w.run(Demo::Off).await.unwrap();
    assert_eq!(w.zitadel.calls("POST /v2/users/u-human/deactivate"), 1);
    assert_eq!(human_state(&w), "USER_STATE_INACTIVE");
    // The machine users stay active (the admin PAT is one of them).
    assert_eq!(
        w.zitadel.state().users["u-bootstrap"].3,
        "USER_STATE_ACTIVE"
    );

    let d = World::new();
    d.run(Demo::On).await.unwrap();
    assert_eq!(human_state(&d), "USER_STATE_ACTIVE");
}

// The window before the human is deactivated is only Zitadel's own start: it is the first
// thing done, before any wait on the metadata database or the orchestrator.
#[tokio::test]
async fn first_human_deactivated_before_orchestrator_wait() {
    let w = World::new();
    w.orchestrator.down.store(3, Ordering::SeqCst);
    w.metadata.unmigrated.store(3, Ordering::SeqCst);
    w.run(Demo::Off).await.unwrap();
    let deactivated = w
        .first("zitadel POST /v2/users/u-human/deactivate")
        .unwrap();
    for wait in [
        "orchestrator ready?",
        "metadata migrated?",
        "zitadel ProjectService/",
    ] {
        assert!(
            deactivated < w.first(wait).unwrap(),
            "{wait}: {:?}",
            w.log()
        );
    }
}

// AC 5: a real refusal fails at once, naming the step: no retry until the deadline.
#[tokio::test]
async fn non_retryable_fails_fast() {
    let mut w = World::new();
    w.cfg.timeout = Duration::from_secs(600);
    w.zitadel.fault(
        "ProjectService/CreateProject",
        ApiError::new(
            "zitadel.project.v2.ProjectService/CreateProject",
            Some(409),
            "Errors.Project.AlreadyExists",
        ),
        100,
    );
    let started = std::time::Instant::now();
    let err = format!("{:#}", w.run(Demo::Off).await.unwrap_err());
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(w.zitadel.calls("ProjectService/CreateProject"), 1);
    assert!(err.contains("the project") && err.contains("409"), "{err}");
    // The bind comes first now; nothing after the failed step ran.
    assert!(
        w.zitadel.state().apps.is_empty() && w.out(outputs::ORG_ID).is_none(),
        "nothing after the failed step ran"
    );
}

// AC 6: past the deadline it fails, naming what it waited for.
#[tokio::test]
async fn deadline_names_dependency() {
    let mut w = World::new();
    w.cfg.timeout = Duration::from_millis(100);
    w.orchestrator.never = true;
    let err = format!("{:#}", w.run(Demo::Off).await.unwrap_err());
    assert!(
        err.contains("waiting for the orchestrator at http://orchestrator:8083/health")
            && err.contains("connection refused"),
        "{err}"
    );
    // And Zitadel that never comes up.
    let mut w = World::new();
    w.cfg.timeout = Duration::from_millis(100);
    w.zitadel.not_ready.store(u32::MAX, Ordering::SeqCst);
    let err = format!("{:#}", w.run(Demo::Off).await.unwrap_err());
    assert!(
        err.contains("waiting for Zitadel") && err.contains("503"),
        "{err}"
    );
}

// AC 7: the tenant is bound through the metadata client (the `Metadata` trait, whose real
// implementation is `chelabase_metadata::MetadataClient` over the URL), with no docker binary
// and no workbench stack dir: the config has neither, and no bootstrap module starts a process.
#[tokio::test]
async fn tenant_bind_without_docker() {
    let w = World::new();
    w.run(Demo::Off).await.unwrap();
    assert!(w.first("metadata bind org-1").is_some(), "{:?}", w.log());
    assert_eq!(w.metadata.state().org.as_deref(), Some("org-1"));
    assert_eq!(w.metadata.state().name.as_deref(), Some("Acme"));
    for (file, src) in SOURCES {
        for banned in [
            "std::process",
            "Command::new",
            "\"docker\"",
            "\"psql\"",
            "CHELABASE_STACK_DIR",
        ] {
            assert!(!src.contains(banned), "{file} uses {banned}");
        }
    }
    // The config comes from the environment alone: no stack dir is asked for.
    let env: HashMap<&str, &str> = [
        ("CHELABASE_AUTH_HOST", "auth.example.com"),
        ("CHELABASE_STUDIO_HOST", "studio.example.com"),
        ("CHELABASE_API_HOST", "api.example.com"),
        ("CHELABASE_ZITADEL_ADMIN_PAT_FILE", "/run/zitadel/admin.pat"),
        (
            "CHELABASE_METADATA_DATABASE_URL",
            "postgres://chelabase@metadata-db:5432/chelabase_metadata",
        ),
    ]
    .into_iter()
    .collect();
    let cfg = Config::from_lookup(|k| env.get(k).map(|v| v.to_string()), "/out".into()).unwrap();
    assert_eq!(cfg.auth_host, "auth.example.com");
    assert_eq!(cfg.studio_origin, "https://studio.example.com");
}

/// Every source file of the bootstrap, for [`tenant_bind_without_docker`].
const SOURCES: [(&str, &str); 7] = [
    ("mod.rs", include_str!("mod.rs")),
    ("steps.rs", include_str!("steps.rs")),
    ("tenant.rs", include_str!("tenant.rs")),
    ("zitadel.rs", include_str!("zitadel.rs")),
    ("requests.rs", include_str!("requests.rs")),
    ("retry.rs", include_str!("retry.rs")),
    ("outputs.rs", include_str!("outputs.rs")),
];

// AC 7: it creates no key: with a read-only secrets dir next to it, only the output dir is
// written, and nothing it writes is a key file.
#[tokio::test]
async fn bootstrap_creates_no_keys() {
    let w = World::new();
    let secrets = w.root.path().join("secrets");
    for d in ["", "gateway", "auth", "neon"] {
        std::fs::create_dir_all(secrets.join(d)).unwrap();
    }
    std::fs::write(secrets.join("gateway/internal-signing.pem"), "PEM").unwrap();
    for d in ["gateway", "auth", "neon", ""] {
        std::fs::set_permissions(secrets.join(d), std::fs::Permissions::from_mode(0o500)).unwrap();
    }
    std::fs::write(&w.cfg.admin_pat_file, "pat").unwrap();
    let before = w.files();
    w.run(Demo::On).await.unwrap();
    let after = w.files();
    let changed: Vec<_> = after
        .iter()
        .filter(|(p, v)| before.get(*p) != Some(v))
        .map(|(p, _)| p.clone())
        .collect();
    assert!(!changed.is_empty());
    for p in &changed {
        assert!(
            p.starts_with(&w.cfg.out_dir),
            "written outside the output dir: {}",
            p.display()
        );
        let name = p.strip_prefix(&w.cfg.out_dir).unwrap().to_str().unwrap();
        assert!(outputs::ALL.contains(&name), "not an output: {name}");
        assert!(!name.ends_with(".pem") && !name.ends_with(".key"), "{name}");
    }
    assert!(
        before.keys().all(|p| after.contains_key(p)),
        "nothing removed"
    );
    for d in ["gateway", "auth", "neon", ""] {
        std::fs::set_permissions(secrets.join(d), std::fs::Permissions::from_mode(0o700)).unwrap();
    }
}

// A tenant claimed by org A is never rebound to this Zitadel's org B (a lost Zitadel volume):
// the run fails naming both, and metadata and the output ids stay as they were.
#[tokio::test]
async fn rebind_to_other_org_refused() {
    let w = World::new();
    w.metadata.state.lock().unwrap().org = Some("org-A".into());
    let err = format!("{:#}", w.run(Demo::On).await.unwrap_err());
    assert!(err.contains("org-A") && err.contains("org-1"), "{err}");
    let m = w.metadata.state();
    assert_eq!(m.org.as_deref(), Some("org-A"));
    assert!(m.name.is_none() && m.admins.is_empty(), "{m:?}");
    assert!(
        w.out(outputs::ORG_ID).is_none(),
        "no org id for the other org"
    );
    assert!(
        w.zitadel.state().targets.is_empty(),
        "the Actions target comes after the bind"
    );
}

// A 404 on an object (a project or an app that isn't there) is an answer: the step creates the
// object (an empty listing) or fails at once (a 404), never waiting on it.
#[tokio::test]
async fn object_404_not_retried() {
    let mut w = World::new();
    w.cfg.timeout = Duration::from_secs(600);
    w.zitadel.fault(
        "ApplicationService/ListApplications",
        ApiError::new(
            "zitadel.application.v2.ApplicationService/ListApplications",
            Some(404),
            "Errors.Project.NotFound",
        ),
        100,
    );
    let started = std::time::Instant::now();
    let err = format!("{:#}", w.run(Demo::Off).await.unwrap_err());
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(
        w.zitadel.calls("ApplicationService/ListApplications"),
        1,
        "{err}"
    );
    assert!(err.contains("404"), "{err}");
    // A missing project in an empty listing is created.
    let w = World::new();
    w.run(Demo::Off).await.unwrap();
    assert_eq!(w.zitadel.calls("ProjectService/CreateProject"), 1);
}

// Outputs are never written through a symlink: one in the output dir fails the run before
// anything is written, and its target is untouched.
#[tokio::test]
async fn outputs_refuse_symlinks() {
    for name in outputs::ALL {
        let w = World::new();
        let target = w.root.path().join("elsewhere");
        std::fs::write(&target, "keep").unwrap();
        let link = w.cfg.out_dir.join(name);
        let parent = link.parent().unwrap();
        std::fs::create_dir_all(parent).unwrap();
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::os::unix::fs::symlink(&target, link).unwrap();
        let err = format!("{:#}", w.run(Demo::On).await.unwrap_err());
        assert!(
            err.contains("symlink") && err.contains(name),
            "{name}: {err}"
        );
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "keep");
        assert!(
            w.log().is_empty(),
            "{name}: called before the check: {:?}",
            w.log()
        );
    }
    // The output dir itself as a symlink is refused too.
    let w = World::new();
    let real = w.root.path().join("real");
    std::fs::create_dir(&real).unwrap();
    std::fs::remove_dir(&w.cfg.out_dir).unwrap();
    std::os::unix::fs::symlink(&real, &w.cfg.out_dir).unwrap();
    let err = format!("{:#}", w.run(Demo::Off).await.unwrap_err());
    assert!(err.contains("symlink"), "{err}");
    assert_eq!(std::fs::read_dir(&real).unwrap().count(), 0);
}

// The metadata password comes from its file into the URL's user info, percent-encoded; a URL
// that already has one is refused rather than silently overridden.
#[test]
fn the_metadata_password_file_fills_the_url() {
    assert_eq!(
        with_password(
            "postgres://chelabase@metadata-db:5432/m?sslmode=disable",
            "p@ss/w:rd"
        )
        .unwrap(),
        "postgres://chelabase:p%40ss%2Fw%3Ard@metadata-db:5432/m?sslmode=disable"
    );
    assert!(with_password("postgres://chelabase:x@db/m", "p").is_err());
    assert!(with_password("postgres://db/m", "p").is_err());
}

// SMTP comes from CHELABASE_SMTP_* with the password from a file only; none when unset.
#[test]
fn smtp_comes_from_the_env_with_a_password_file() {
    let t = tempfile::tempdir().unwrap();
    let pw = t.path().join("smtp.password");
    std::fs::write(&pw, "secret\n").unwrap();
    let pw = pw.display().to_string();
    let env = |pairs: &[(&str, &str)]| -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    };
    let lookup = |m: &HashMap<String, String>| {
        let m = m.clone();
        move |k: &str| m.get(k).cloned()
    };
    assert_eq!(smtp_from(&lookup(&env(&[]))).unwrap(), None);
    let full = env(&[
        ("CHELABASE_SMTP_HOST", "smtp.example.com"),
        ("CHELABASE_SMTP_USER", "u"),
        ("CHELABASE_SMTP_PASSWORD_FILE", &pw),
        ("CHELABASE_SMTP_FROM", "noreply@example.com"),
    ]);
    let smtp = smtp_from(&lookup(&full)).unwrap().unwrap();
    assert_eq!(smtp.host, "smtp.example.com:587");
    assert_eq!(smtp.payload["password"], "secret");
    assert_eq!(smtp.payload["tls"], true);
    let mut plain = full.clone();
    plain.insert("CHELABASE_SMTP_PASSWORD".into(), "x".into());
    assert!(
        smtp_from(&lookup(&plain)).is_err(),
        "a password in the env is refused"
    );
    let mut no_from = full.clone();
    no_from.remove("CHELABASE_SMTP_FROM");
    assert!(smtp_from(&lookup(&no_from)).is_err());
}

#[test]
fn the_first_human_is_looked_for_by_zitadels_names() {
    assert_eq!(
        steps::first_human_usernames("auth.feat-x.chelabase.localhost"),
        [
            "zitadel-admin@zitadel.auth.feat-x.chelabase.localhost",
            "zitadel-admin@zitadel.localhost",
            "zitadel-admin"
        ]
    );
    assert_eq!(
        steps::first_human_usernames("localhost"),
        ["zitadel-admin@zitadel.localhost", "zitadel-admin"]
    );
}

// --- m4 fix round (audit of #11) ---

// B1: the FirstInstance human is found by what it is (a human IAM member), not by a guessed
// name: renamed, it is still deactivated.
#[tokio::test]
async fn first_human_with_other_name_is_deactivated() {
    let w = World::new();
    w.zitadel
        .state
        .lock()
        .unwrap()
        .users
        .get_mut("u-human")
        .unwrap()
        .0 = "zitadel-admin@chelabase.auth.chelabase.localhost".into();
    w.run(Demo::Off).await.unwrap();
    assert_eq!(human_state(&w), "USER_STATE_INACTIVE");
}

// B1: every active human IAM member is deactivated; the machine owners (the admin PAT's user)
// stay; a rerun deactivates nothing again.
#[tokio::test]
async fn every_human_iam_member_is_deactivated_and_machines_kept() {
    let w = World::new();
    {
        let mut s = w.zitadel.state.lock().unwrap();
        s.users.insert(
            "u-ops".into(),
            (
                "ops@example.com".into(),
                true,
                "ops@example.com".into(),
                "USER_STATE_ACTIVE".into(),
            ),
        );
        s.iam.insert("u-ops".into());
    }
    w.run(Demo::Off).await.unwrap();
    let users = w.zitadel.state().users;
    assert_eq!(users["u-human"].3, "USER_STATE_INACTIVE");
    assert_eq!(users["u-ops"].3, "USER_STATE_INACTIVE");
    assert_eq!(users["u-bootstrap"].3, "USER_STATE_ACTIVE");
    let before = w.log().len();
    w.run(Demo::Off).await.unwrap();
    assert!(
        !w.log()[before..].iter().any(|e| e.contains("/deactivate")),
        "{:?}",
        &w.log()[before..]
    );
}

// B1: a human IAM owner it can't deactivate fails the step, naming it (never "nothing to do").
#[tokio::test]
async fn a_human_owner_left_active_fails_the_step() {
    let w = World::new();
    w.zitadel.fault(
        "POST /v2/users/u-human/deactivate",
        ApiError::new(
            "POST /v2/users/u-human/deactivate",
            Some(412),
            "Errors.User.NotFound",
        ),
        100,
    );
    let err = format!("{:#}", w.run(Demo::Off).await.unwrap_err());
    assert!(
        err.contains("Zitadel's human admins") && err.contains("412"),
        "{err}"
    );
    // An IAM listing that doesn't hold every member can't prove none is left: it fails too.
    let w = World::new();
    w.zitadel.state.lock().unwrap().iam_unlisted = 1;
    let err = format!("{:#}", w.run(Demo::Off).await.unwrap_err());
    assert!(
        err.contains("Zitadel's human admins") && err.contains("listed 2 of 3"),
        "{err}"
    );
    assert_eq!(
        human_state(&w),
        "USER_STATE_ACTIVE",
        "nothing deactivated from a partial list"
    );
}

// B1: no human IAM member at all (removed by the operator, or demoted) is the state this step
// wants: success, and nothing is called but the listing.
#[tokio::test]
async fn no_human_iam_member_is_success() {
    let w = World::new();
    w.zitadel.state.lock().unwrap().iam.remove("u-human");
    w.run(Demo::Off).await.unwrap();
    assert_eq!(w.zitadel.calls("POST /v2/users/u-human/deactivate"), 0);
    assert_eq!(
        human_state(&w),
        "USER_STATE_ACTIVE",
        "not an IAM member: not touched"
    );
}

// S1 (AC 9): a refused rebind changes nothing: no output file, no Zitadel object. Without
// `--demo` the one change left is the FirstInstance human's deactivation, which AC 8 puts
// before any wait on the orchestrator (and so before the bind).
#[tokio::test]
async fn refused_rebind_changes_nothing() {
    for demo in [Demo::On, Demo::Off] {
        let w = World::new();
        w.metadata.state.lock().unwrap().org = Some("org-A".into());
        let (files, mut zitadel) = (w.files(), w.zitadel.state());
        w.run(demo).await.unwrap_err();
        assert_eq!(w.files(), files, "{demo:?}: the output dir changed");
        if demo == Demo::Off {
            zitadel.users.get_mut("u-human").unwrap().3 = "USER_STATE_INACTIVE".into();
        }
        assert_eq!(w.zitadel.state(), zitadel, "{demo:?}: Zitadel changed");
    }
}

fn smtp_world(password: &str, from: &str) -> World {
    let mut w = World::new();
    w.cfg.smtp = Some(steps::Smtp {
        host: "smtp.example.com:587".into(),
        payload: json!({"senderAddress": from, "senderName": "Chelabase", "tls": true,
            "host": "smtp.example.com:587", "user": "u", "password": password,
            "description": "chelabase"}),
    });
    w
}

// S2: a changed SMTP password or sender is applied on the next run; an unchanged one isn't
// rewritten.
#[tokio::test]
async fn a_changed_smtp_config_is_applied() {
    let mut w = smtp_world("old", "a@example.com");
    w.run(Demo::Off).await.unwrap();
    let id = w.zitadel.state().smtp[0].0.clone();
    let config = |w: &World| w.zitadel.state().smtp_config[&id].clone();
    assert_eq!(config(&w)["password"], "old");

    // Unchanged: no SMTP write.
    let before = w.log().len();
    w.run(Demo::Off).await.unwrap();
    let smtp_writes = |w: &World, from: usize| -> Vec<String> {
        w.log()[from..]
            .iter()
            .filter(|e| e.contains("/admin/v1/smtp") && !e.ends_with("_search"))
            .cloned()
            .collect()
    };
    assert!(
        smtp_writes(&w, before).is_empty(),
        "{:?}",
        smtp_writes(&w, before)
    );

    // A new password.
    w.cfg.smtp = smtp_world("new", "a@example.com").cfg.smtp;
    w.run(Demo::Off).await.unwrap();
    assert_eq!(config(&w)["password"], "new");
    // A new sender.
    w.cfg.smtp = smtp_world("new", "b@example.com").cfg.smtp;
    let before = w.log().len();
    w.run(Demo::Off).await.unwrap();
    assert_eq!(config(&w)["senderAddress"], "b@example.com");
    assert_eq!(config(&w)["password"], "new");
    assert!(
        !smtp_writes(&w, before)
            .iter()
            .any(|e| e.ends_with("/password")),
        "the password is unchanged: not resent"
    );
    assert_eq!(
        w.zitadel.state().smtp.len(),
        1,
        "updated in place, not duplicated"
    );
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

// N3: a second run behind a held output-dir lock waits only until the deadline, naming it.
#[tokio::test]
async fn the_output_lock_waits_only_until_the_deadline() {
    let mut w = World::new();
    w.cfg.timeout = Duration::from_millis(200);
    let held = std::fs::File::create(w.cfg.out_dir.join(outputs::LOCK)).unwrap();
    held.lock().unwrap();
    let result = tokio::time::timeout(Duration::from_secs(5), w.run(Demo::Off)).await;
    let err = format!("{:#}", result.expect("bounded by --timeout").unwrap_err());
    assert!(
        err.contains(outputs::LOCK) && err.contains("another bootstrap"),
        "{err}"
    );
}

// S3: the SQL the bootstrap runs is the one the metadata crate's live test checks: no copy is
// left in the CLI.
#[test]
fn the_tenant_name_sql_has_one_home() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    for orphan in [
        "tools/cli/src/dev/tenant_name.sql",
        "tools/cli/src/dev/tenant_admin.sql",
    ] {
        assert!(!root.join(orphan).exists(), "{orphan} is an orphaned copy");
    }
    let live =
        std::fs::read_to_string(root.join("crates/chelabase-metadata/tests/members_live.rs"))
            .unwrap();
    assert!(live.contains("chelabase-init/src/bootstrap/tenant_name.sql"));
    assert!(!live.contains("tools/cli/src/dev/tenant_"));
}

// N4: a member listed without a type is checked, not skipped: a human is deactivated, a
// machine (from its user record) stays.
#[tokio::test]
async fn an_untyped_iam_member_is_checked_not_skipped() {
    let w = World::new();
    {
        let mut s = w.zitadel.state.lock().unwrap();
        s.iam_untyped.insert("u-human".into());
        s.iam_untyped.insert("u-bootstrap".into());
    }
    w.run(Demo::Off).await.unwrap();
    assert_eq!(human_state(&w), "USER_STATE_INACTIVE");
    assert_eq!(
        w.zitadel.state().users["u-bootstrap"].3,
        "USER_STATE_ACTIVE"
    );
}
