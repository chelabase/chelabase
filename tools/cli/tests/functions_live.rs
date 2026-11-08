//! `chelabase login` and `chelabase functions …` against a workbench stack (#167): a deploy
//! answering through the gateway with the branch's public key, a core `.wasm` import, and
//! `dev --watch` putting an edit live within 2 s while a syntax error keeps the previous
//! deployment.
//!
//! ```text
//! just workbench test -p chelabase --test functions_live -- --run-ignored only
//! ```
//!
//! Shared state: it makes its own pending database (no compute: the gateway only resolves the
//! key) with a public key, signs in as the workbench's dev member, and removes the rows at the
//! end, pass or fail. esbuild comes from the checkout's `node_modules` (via vite).

// Test code: a plain client against the stack (clippy.toml allows tests only).
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use chelabase_metadata::{
    DEMO_PROJECT_ID, DEMO_TENANT_ID, MetadataClient, MetadataConfig, NewDataKey,
};
use chelabase_secrets::SecretKey;
use chelabase_types::data_key::{DataKeyKind, generate};
use chelabase_types::{BranchId, DatabaseId};
use uuid::Uuid;

const WITHIN: Duration = Duration::from_secs(2);

/// `(module (func (export "add") (param i32 i32) (result i32) local.get 0 local.get 1 i32.add))`
const ADD_WASM: &[u8] = &[
    0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00, 0x01, 0x07, 0x01, 0x60, 0x02, 0x7f, 0x7f, 0x01,
    0x7f, 0x03, 0x02, 0x01, 0x00, 0x07, 0x07, 0x01, 0x03, 0x61, 0x64, 0x64, 0x00, 0x00, 0x0a, 0x09,
    0x01, 0x07, 0x00, 0x20, 0x00, 0x20, 0x01, 0x6a, 0x0b,
];

fn env(name: &str) -> String {
    std::env::var(name)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| panic!("{name} is unset (run through `just workbench test`)"))
}

fn hello(version: &str) -> String {
    format!(
        "import {{ tag }} from \"./tag\";\n\
         export default {{ async fetch() {{ return new Response(tag(\"{version}\")); }} }};\n"
    )
}

/// The directory holding an esbuild: the checkout's `node_modules/.bin` (or a package's).
fn esbuild_dir() -> PathBuf {
    let start = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let start = std::env::var("CARGO_MANIFEST_DIR")
        .map(PathBuf::from)
        .unwrap_or(start);
    start
        .ancestors()
        .flat_map(|d| {
            [
                d.join("node_modules/.bin"),
                d.join("packages/client/node_modules/.bin"),
            ]
        })
        .find(|d| d.join("esbuild").is_file())
        .expect("an esbuild in the checkout's node_modules (pnpm install)")
}

struct Stack {
    metadata: MetadataClient,
    db: DatabaseId,
    key: String,
    api: String,
    http: reqwest::Client,
    project: tempfile::TempDir,
    config: tempfile::TempDir,
}

impl Stack {
    async fn new() -> Self {
        let secrets = SecretKey::from_env().expect("CHELABASE_SECRETS_KEY");
        let metadata = MetadataClient::connect(&MetadataConfig {
            database_url: env("CHELABASE_METADATA_DATABASE_URL"),
        })
        .await
        .expect("metadata database")
        .with_secrets_key(secrets.clone());
        let db = DatabaseId::new();
        metadata
            .insert_database_pending(
                db,
                DEMO_TENANT_ID.into(),
                DEMO_PROJECT_ID.into(),
                &format!("cli-functions-{}", &db.to_string()[..8]),
            )
            .await
            .expect("a pending database");
        let generated = generate(DataKeyKind::Public);
        let key_id = Uuid::new_v4();
        metadata
            .insert_data_key(&NewDataKey {
                id: key_id,
                branch_id: BranchId(db.as_uuid()),
                kind: DataKeyKind::Public,
                role: "anon".into(),
                name: "cli-functions".into(),
                prefix: generated.prefix.clone(),
                sha256: generated.sha256,
                sealed: Some(chelabase_secrets::seal_credential(
                    &secrets,
                    &key_id.to_string(),
                    chelabase_secrets::Purpose::DataKey,
                    &generated.key,
                )),
                databases: None,
                role_oid: None,
                role_timeline: None,
                role_bypass_rls: false,
            })
            .await
            .expect("the branch's public key");
        Stack {
            metadata,
            db,
            key: generated.key,
            api: env("CHELABASE_ENTRY_API_URL")
                .trim_end_matches('/')
                .to_owned(),
            http: reqwest::Client::new(),
            project: tempfile::tempdir().unwrap(),
            config: tempfile::tempdir().unwrap(),
        }
    }

    fn cli(&self) -> Command {
        let exe = std::env::var("NEXTEST_BIN_EXE_chelabase")
            .unwrap_or_else(|_| env!("CARGO_BIN_EXE_chelabase").to_owned());
        let mut cmd = Command::new(exe);
        let path = std::env::join_paths(std::iter::once(esbuild_dir()).chain(
            std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
        ))
        .unwrap();
        cmd.current_dir(self.project.path())
            .env("CHELABASE_CONFIG_DIR", self.config.path())
            .env("PATH", path)
            .env_remove("CHELABASE_URL")
            .env_remove("CHELABASE_DATABASE")
            .env_remove("CHELABASE_BRANCH");
        cmd
    }

    fn run(&self, args: &[&str], stdin: &str) -> (bool, String, String) {
        let mut child = self
            .cli()
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("the CLI");
        child
            .stdin
            .take()
            .unwrap()
            .write_all(stdin.as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        let s = |b: Vec<u8>| String::from_utf8_lossy(&b).into_owned();
        (out.status.success(), s(out.stdout), s(out.stderr))
    }

    fn write(&self, rel: &str, bytes: &[u8]) {
        let p = self.project.path().join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, bytes).unwrap();
    }

    async fn call(&self, name: &str) -> (u16, String) {
        self.get(&format!("/functions/v1/{name}")).await
    }

    /// A GET of `path` through the gateway with the branch's public key.
    async fn get(&self, path: &str) -> (u16, String) {
        let resp = self
            .http
            .get(format!("{}{path}", self.api))
            .bearer_auth(&self.key)
            .send()
            .await
            .expect("the gateway");
        (
            resp.status().as_u16(),
            resp.text().await.unwrap_or_default(),
        )
    }

    /// Calls `name` until it answers 200 `want`, for at most `within` from `since`.
    async fn until(
        &self,
        name: &str,
        want: &str,
        since: Instant,
        within: Duration,
    ) -> (u16, String) {
        loop {
            let got = self.call(name).await;
            if (got.0 == 200 && got.1 == want) || since.elapsed() > within {
                return got;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    async fn cleanup(&self) {
        for f in self
            .metadata
            .list_functions(self.db)
            .await
            .unwrap_or_default()
        {
            let _ = sqlx::query("DELETE FROM functions WHERE id = $1")
                .bind(f.id)
                .execute(self.metadata.pool())
                .await;
        }
        for sql in [
            "DELETE FROM data_api_keys WHERE branch_id = $1",
            "DELETE FROM branches WHERE database_id = $1",
            "DELETE FROM databases WHERE id = $1",
        ] {
            let _ = sqlx::query(sql)
                .bind(self.db.as_uuid())
                .execute(self.metadata.pool())
                .await;
        }
    }
}

fn dev_member() -> (String, String) {
    let dir = std::env::var("CHELABASE_BOOTSTRAP_DIR")
        .ok()
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(&env("CHELABASE_STACK_DIR")).join("bootstrap"));
    let v: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.join("dev-member.json")).unwrap()).unwrap();
    (
        v["email"].as_str().unwrap().to_owned(),
        v["password"].as_str().unwrap().to_owned(),
    )
}

/// Lines of a child's stream, as they come.
fn lines(stream: impl std::io::Read + Send + 'static) -> mpsc::Receiver<String> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stream).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    rx
}

fn wait_for(rx: &mpsc::Receiver<String>, what: &str, timeout: Duration) -> String {
    let start = Instant::now();
    let mut seen = Vec::new();
    while let Some(left) = timeout.checked_sub(start.elapsed()) {
        match rx.recv_timeout(left) {
            Ok(l) if l.contains(what) => return l,
            Ok(l) => seen.push(l),
            Err(_) => break,
        }
    }
    panic!("no line with {what:?} within {timeout:?}; saw {seen:#?}");
}

struct Kill(Child);
impl Drop for Kill {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

async fn flow(s: &Stack) {
    use std::os::unix::fs::PermissionsExt;
    // Sign in: the password from stdin, the credentials owner-only, the token never printed.
    let (email, password) = dev_member();
    let (ok, out, err) = s.run(
        &["login", "--url", &s.api, "--email", &email],
        &format!("{password}\n"),
    );
    assert!(ok, "login: {out}{err}");
    let creds = s.config.path().join("credentials.json");
    assert_eq!(
        std::fs::metadata(&creds).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let kept: serde_json::Value = serde_json::from_slice(&std::fs::read(&creds).unwrap()).unwrap();
    let token = kept[&s.api]["access_token"]
        .as_str()
        .expect("a kept token")
        .to_owned();
    assert!(
        !out.contains(&token) && !err.contains(&token),
        "the token is never printed"
    );

    s.write(
        "chelabase.toml",
        format!(
            "[project]\nurl = \"{}\"\ndatabase = \"{}\"\n\n\
             [functions.hello]\nentry = \"functions/hello/index.ts\"\n\n\
             [functions.adder]\nentry = \"functions/adder/index.ts\"\n",
            s.api, s.db
        )
        .as_bytes(),
    );
    s.write(
        "functions/hello/tag.ts",
        b"export const tag = (v: string): string => `hello ${v}`;\n",
    );
    s.write("functions/hello/index.ts", hello("v1").as_bytes());
    s.write("functions/adder/add.wasm", ADD_WASM);
    s.write(
        "functions/adder/index.ts",
        b"import add from \"./add.wasm\";\n\
          const { exports } = new WebAssembly.Instance(add as WebAssembly.Module);\n\
          export default { fetch() { return new Response(String((exports.add as Function)(2, 3))); } };\n",
    );

    // 1. Deploy: the id is printed, and the new code answers through the gateway.
    let (ok, out, err) = s.run(&["functions", "deploy", "hello"], "");
    assert!(ok, "deploy: {out}{err}");
    let v1 = out
        .trim()
        .strip_prefix("hello: deployed ")
        .expect("the deployment id")
        .to_owned();
    let (status, body) = s.until("hello", "hello v1", Instant::now(), WITHIN).await;
    assert_eq!((status, body.as_str()), (200, "hello v1"));

    // 2. A core .wasm import runs as a wasm module.
    let (ok, out, err) = s.run(&["functions", "deploy", "adder"], "");
    assert!(ok, "deploy adder: {out}{err}");
    let (status, body) = s.until("adder", "5", Instant::now(), WITHIN).await;
    assert_eq!((status, body.as_str()), (200, "5"));

    let (ok, out, _) = s.run(&["functions", "list"], "");
    assert!(
        ok && out.contains("hello\tenabled") && out.contains("adder\tenabled"),
        "{out}"
    );

    // 3. dev --watch: an edit is live within 2 s; a syntax error keeps the previous deployment.
    let mut child = s
        .cli()
        .args(["functions", "dev", "--watch", "hello"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("dev --watch");
    let (out, err) = (
        lines(child.stdout.take().unwrap()),
        lines(child.stderr.take().unwrap()),
    );
    let _child = Kill(child);
    wait_for(&out, "hello: deployed", Duration::from_secs(30));
    let saved = Instant::now();
    s.write("functions/hello/index.ts", hello("v2").as_bytes());
    let (status, body) = s.until("hello", "hello v2", saved, WITHIN).await;
    assert_eq!(
        (status, body.as_str()),
        (200, "hello v2"),
        "live {:?} after the save",
        saved.elapsed()
    );

    s.write(
        "functions/hello/index.ts",
        b"export default { fetch() { return new Response( } };\n",
    );
    let line = wait_for(&err, "not deployed", Duration::from_secs(10));
    assert!(line.contains("previous deployment stays live"), "{line}");
    let msg = wait_for(&err, "ERROR", Duration::from_secs(2));
    assert!(msg.contains("ERROR"), "esbuild's message: {msg}");
    assert_eq!(
        s.call("hello").await,
        (200, "hello v2".to_owned()),
        "the previous deployment stays"
    );

    // A change to a shared input redeploys too.
    s.write("functions/hello/index.ts", hello("v3").as_bytes());
    s.write(
        "functions/hello/tag.ts",
        b"export const tag = (v: string): string => `hi ${v}`;\n",
    );
    let (status, body) = s.until("hello", "hi v3", Instant::now(), WITHIN).await;
    assert_eq!((status, body.as_str()), (200, "hi v3"));

    // 4. Rollback, disable, secrets.
    let (ok, out, _) = s.run(&["functions", "list", "hello"], "");
    assert!(ok && out.contains(&v1), "the deployments: {out}");
    let (ok, _, err) = s.run(&["functions", "rollback", "hello", &v1], "");
    assert!(ok, "rollback: {err}");
    let (status, body) = s.until("hello", "hello v1", Instant::now(), WITHIN).await;
    assert_eq!((status, body.as_str()), (200, "hello v1"), "rolled back");
    let (ok, _, err) = s.run(
        &["functions", "secrets", "set", "hello", "GREETING"],
        "hi there\n",
    );
    assert!(ok, "secrets set: {err}");
    let (ok, out, _) = s.run(&["functions", "secrets", "list", "hello"], "");
    assert!(ok && out.trim() == "GREETING", "{out}");
    let (ok, _, _) = s.run(&["functions", "secrets", "unset", "hello", "GREETING"], "");
    assert!(ok);
    schedules_and_triggers(s);
    let (ok, _, err) = s.run(&["functions", "disable", "hello"], "");
    assert!(ok, "disable: {err}");
    let (status, _) = s.until("hello", "-", Instant::now(), WITHIN).await;
    assert_eq!(status, 404, "disabled");

    // 5. An ended session asks for a new login and prints no token.
    let mut kept: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&creds).unwrap()).unwrap();
    kept[&s.api]["access_token"] = serde_json::Value::Null;
    kept[&s.api]["session"] = "ended".into();
    std::fs::write(&creds, kept.to_string()).unwrap();
    let (ok, out, err) = s.run(&["functions", "list"], "");
    assert!(!ok && err.contains("chelabase login --url"), "{out}{err}");
}

/// m11: `functions schedule` and `functions trigger` add, list and rm (`hello` is deployed).
fn schedules_and_triggers(s: &Stack) {
    let (ok, out, err) = s.run(
        &[
            "functions",
            "schedule",
            "add",
            "hello",
            "--cron",
            "*/15 * * * *",
            "--timezone",
            "Europe/London",
        ],
        "",
    );
    assert!(ok, "schedule add: {out}{err}");
    let schedule = out.trim().to_owned();
    let (ok, out, err) = s.run(&["functions", "schedule", "list", "hello"], "");
    assert!(ok, "schedule list: {err}");
    let line = out
        .lines()
        .find(|l| l.starts_with(&schedule))
        .unwrap_or_else(|| panic!("{out}"));
    assert!(
        line.contains("*/15 * * * *\tEurope/London\tenabled\tnext=2"),
        "{line}"
    );
    let (ok, _, err) = s.run(&["functions", "schedule", "rm", "hello", &schedule], "");
    assert!(ok, "schedule rm: {err}");
    let (_, out, _) = s.run(&["functions", "schedule", "list", "hello"], "");
    assert!(!out.contains(&schedule), "removed: {out}");

    let add = |args: &[&str]| {
        let mut argv = vec!["functions", "trigger", "add", "hello"];
        argv.extend_from_slice(args);
        let (ok, out, err) = s.run(&argv, "");
        assert!(ok, "trigger add {args:?}: {out}{err}");
        out.trim().to_owned()
    };
    let table = add(&[
        "--table",
        "public.orders",
        "--pg-database",
        "app",
        "--events",
        "insert,update",
    ]);
    let storage = add(&[
        "--storage",
        "--pg-database",
        "app",
        "--bucket",
        "avatars",
        "--prefix",
        "in/",
        "--events",
        "object.created",
    ]);
    let (ok, out, err) = s.run(&["functions", "trigger", "list", "hello"], "");
    assert!(ok, "trigger list: {err}");
    let line = |id: &str| {
        out.lines()
            .find(|l| l.starts_with(id))
            .unwrap_or_else(|| panic!("{id}: {out}"))
            .to_owned()
    };
    let t = line(&table);
    assert!(t.contains("\ttable public.orders\tinsert,update\t"), "{t}");
    assert!(t.contains("\tdead=0\tdropped=0\tdelivered=-"), "{t}");
    let st = line(&storage);
    assert!(
        st.contains("\tstorage avatars/in/\tobject.created\t"),
        "{st}"
    );
    for id in [&table, &storage] {
        let (ok, _, err) = s.run(&["functions", "trigger", "rm", "hello", id], "");
        assert!(ok, "trigger rm: {err}");
    }
    let (_, out, _) = s.run(&["functions", "trigger", "list", "hello"], "");
    assert!(out.trim().is_empty(), "removed: {out}");
}

/// go-httpbin, as the functions e2e's container test uses it (`CHELABASE_E2E_CONTAINER_IMAGE`
/// overrides it).
const IMAGE: &str = "mccutchen/go-httpbin:2.26.0";

/// m11: `services deploy` (created and enabled), a refused unhealthy rollover, `list`, `logs`
/// and `disable`. Needs the stack's `services` profile; without the runner it reports itself
/// skipped.
async fn services_flow(s: &Stack) {
    let (email, password) = dev_member();
    let (ok, out, err) = s.run(
        &["login", "--url", &s.api, "--email", &email],
        &format!("{password}\n"),
    );
    assert!(ok, "login: {out}{err}");
    let image = std::env::var("CHELABASE_E2E_CONTAINER_IMAGE")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| IMAGE.to_owned());
    let db = s.db.to_string();
    let svc = |args: &[&str]| {
        let mut argv = vec!["services", "--database", db.as_str()];
        argv.extend_from_slice(args);
        s.run(&argv, "")
    };
    let (ok, out, err) = svc(&[
        "deploy",
        "box",
        "--image",
        &image,
        "--port",
        "8080",
        "--health",
        "/status/200",
    ]);
    // No runner (the `services` profile is off), or a stack older than container services.
    if !ok && (err.contains("runner") || err.contains("unknown field `image`")) {
        eprintln!("SKIPPED: the stack can't run container services: {err}");
        return;
    }
    assert!(ok, "services deploy: {out}{err}");
    assert!(
        out.contains("box: signing secret "),
        "the secret is shown once: {out}"
    );
    let first = out
        .lines()
        .find_map(|l| l.strip_prefix("box: deployed "))
        .expect("the id")
        .to_owned();

    // Created and enabled: answers within 60 s, signed.
    let since = Instant::now();
    let (status, body) = loop {
        let got = s.get("/services/v1/box/anything/hi").await;
        if got.0 == 200 || since.elapsed() > Duration::from_secs(60) {
            break got;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    };
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("Chelabase-Signature"), "{body}");
    let (_, out, _) = svc(&["list"]);
    assert!(
        out.contains("box\tenabled\t") && out.contains(&first),
        "{out}"
    );

    // A rollover that never passes health: non-zero, the reason and the new id; traffic stays.
    let (ok, out, err) = svc(&[
        "deploy",
        "box",
        "--image",
        &image,
        "--port",
        "8080",
        "--health",
        "/status/500",
    ]);
    assert!(!ok, "an unhealthy rollover fails: {out}{err}");
    assert!(
        err.contains("never passed its health check")
            && err.contains("previous deployment stays live"),
        "{err}"
    );
    let (_, out, _) = svc(&["list"]);
    assert!(
        out.contains(&first),
        "the old deployment stays active: {out}"
    );
    assert_eq!(s.get("/services/v1/box/anything/still").await.0, 200);

    let (ok, out, err) = svc(&["logs", "box"]);
    assert!(ok && !out.trim().is_empty(), "logs: {out}{err}");

    let (ok, _, err) = svc(&["disable", "box"]);
    assert!(ok, "disable: {err}");
    let since = Instant::now();
    let (status, body) = loop {
        let got = s.get("/services/v1/box/anything").await;
        if got.1.contains("service_disabled") || since.elapsed() > WITHIN {
            break got;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert_eq!(status, 503, "{body}");
}

#[tokio::test]
#[ignore = "needs the workbench stack with the services profile"]
async fn services_deploy_rollover_logs_disable() {
    let stack = std::sync::Arc::new(Stack::new().await);
    let run = stack.clone();
    let result = tokio::spawn(async move { services_flow(&run).await }).await;
    stack.cleanup().await;
    if let Err(e) = result {
        std::panic::resume_unwind(e.into_panic());
    }
}

#[tokio::test]
#[ignore = "needs the workbench stack: just workbench test -p chelabase --test functions_live"]
async fn login_deploy_wasm_watch_and_manage() {
    let stack = std::sync::Arc::new(Stack::new().await);
    let run = stack.clone();
    let result = tokio::spawn(async move { flow(&run).await }).await;
    stack.cleanup().await;
    if let Err(e) = result {
        std::panic::resume_unwind(e.into_panic());
    }
}
