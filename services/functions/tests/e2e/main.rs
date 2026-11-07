//! Edge functions end to end on a workbench stack (#181): a function deployed on a branch,
//! invoked through the gateway with the branch's public key, disabled, redeployed, and a
//! public function called on the keyless form.
//!
//! ```text
//! just functions-e2e
//! ```
//!
//! It runs in the stack's runner (stack.env and `chelabase dev token`). Each run makes its own
//! database rows (a pending database: no compute is needed, the gateway only resolves the key)
//! and removes them at the end, pass or fail. Functions are created, deployed, toggled and
//! given secrets and auth through the management API (#165) as the workbench's dev member, so
//! the orchestrator uploads the bundle and publishes `chelabase.fn.changed` itself.

// Test code: a plain client against the stack (clippy.toml allows tests only).
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use chelabase_metadata::{
    DEMO_PROJECT_ID, DEMO_TENANT_ID, MetadataClient, MetadataConfig, NewDataKey,
};
use chelabase_secrets::SecretKey;
use chelabase_types::data_key::{DataKeyKind, generate};
use chelabase_types::{BranchId, DatabaseId};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;

mod app_users;

/// How soon a change must apply (NATS drops the cached resolution).
const WITHIN: Duration = Duration::from_secs(2);

fn env(name: &str) -> String {
    std::env::var(name)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| panic!("{name} is unset (run through `just functions-e2e`)"))
}

fn secrets_key() -> SecretKey {
    SecretKey::from_env().expect("CHELABASE_SECRETS_KEY")
}

struct Stack {
    metadata: MetadataClient,
    http: reqwest::Client,
    /// The gateway's API base.
    api: String,
    /// The management API, as the dev member.
    mgmt: Api,
    db: DatabaseId,
    branch: BranchId,
    key: String,
}

/// The management API as the workbench's dev member (`chelabase dev token`, per request: it is
/// short-lived).
pub(crate) struct Api {
    pub(crate) http: reqwest::Client,
    pub(crate) base: String,
}

impl Api {
    pub(crate) fn new() -> Self {
        Api {
            http: reqwest::Client::new(),
            base: env("CHELABASE_ENTRY_API_URL")
                .trim_end_matches('/')
                .to_owned(),
        }
    }

    pub(crate) fn token() -> String {
        let out = std::process::Command::new(env("CHELABASE_CLI"))
            .args(["dev", "token"])
            .output()
            .expect("chelabase dev token");
        assert!(
            out.status.success(),
            "chelabase dev token: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    }

    /// A call with `body` sent as it is (JSON); the status and the answer (JSON, else text).
    pub(crate) async fn raw(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<String>,
    ) -> (u16, Value) {
        let mut req = self
            .http
            .request(method, format!("{}{path}", self.base))
            .bearer_auth(Self::token());
        if let Some(b) = body {
            req = req.header("content-type", "application/json").body(b);
        }
        let resp = req.send().await.expect("the gateway");
        let status = resp.status().as_u16();
        let text = resp.text().await.unwrap_or_default();
        (
            status,
            serde_json::from_str(&text).unwrap_or(Value::String(text)),
        )
    }

    pub(crate) async fn call(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
    ) -> (u16, Value) {
        self.raw(method, path, body.map(|b| b.to_string())).await
    }

    /// [`Self::call`], asserting the status is one of `want`; the answer.
    pub(crate) async fn ok(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
        want: &[u16],
    ) -> Value {
        let (status, body) = self.call(method.clone(), path, body).await;
        assert!(want.contains(&status), "{method} {path}: {status} {body}");
        body
    }
}

async fn metadata() -> MetadataClient {
    MetadataClient::connect(&MetadataConfig {
        database_url: env("CHELABASE_METADATA_DATABASE_URL"),
    })
    .await
    .expect("metadata database")
    .with_secrets_key(secrets_key())
}

impl Stack {
    /// A pending database of its own (no compute: the gateway only resolves the key) with a
    /// public key on its main branch.
    async fn new() -> Self {
        let metadata = metadata().await;
        let db = DatabaseId::new();
        metadata
            .insert_database_pending(
                db,
                DEMO_TENANT_ID.into(),
                DEMO_PROJECT_ID.into(),
                &format!("functions-e2e-{}", &db.to_string()[..8]),
            )
            .await
            .expect("a pending database");
        // The main branch has the database's id.
        let branch = BranchId(db.as_uuid());
        let generated = generate(DataKeyKind::Public);
        let key_id = Uuid::new_v4();
        metadata
            .insert_data_key(&NewDataKey {
                id: key_id,
                branch_id: branch,
                kind: DataKeyKind::Public,
                role: "anon".into(),
                name: "functions-e2e".into(),
                prefix: generated.prefix.clone(),
                sha256: generated.sha256,
                // A public key is stored sealed for display, as the orchestrator stores it.
                sealed: Some(chelabase_secrets::seal_credential(
                    &secrets_key(),
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
        Self::with(metadata, db, branch, generated.key)
    }

    /// The stack's clients for functions on `branch` of `db`, called with the public `key`.
    fn with(metadata: MetadataClient, db: DatabaseId, branch: BranchId, key: String) -> Self {
        Stack {
            metadata,
            http: reqwest::Client::new(),
            api: env("CHELABASE_ENTRY_API_URL")
                .trim_end_matches('/')
                .to_owned(),
            mgmt: Api::new(),
            db,
            branch,
            key,
        }
    }

    /// The management API's path of the branch's functions, then `tail`.
    fn functions(&self, tail: &str) -> String {
        format!(
            "/v1/databases/{}/branches/{}/functions{tail}",
            self.db, self.branch
        )
    }

    /// A PATCH of the function on the branch through the management API.
    async fn patch(&self, name: &str, body: Value) -> Value {
        self.mgmt
            .ok(
                reqwest::Method::PATCH,
                &self.functions(&format!("/{name}")),
                Some(body),
                &[200],
            )
            .await
    }

    /// Sets a secret of the function on the branch through the management API.
    async fn secret(&self, name: &str, secret: &str, value: &str) {
        self.mgmt
            .ok(
                reqwest::Method::PUT,
                &self.functions(&format!("/{name}/secrets/{secret}")),
                Some(json!({ "value": value })),
                &[204],
            )
            .await;
    }

    /// Creates the function if needed, deploys `source` as a one-module bundle (made active on
    /// the branch) and enables it, all through the management API.
    async fn deploy(&self, name: &str, source: &str) {
        let manifest = serde_json::to_string(&json!({
            "compatibilityDate": "2026-10-01",
            "compatibilityFlags": [],
            "mainModule": "main.js",
            "modules": [{"name": "main.js", "type": "js", "base64": STANDARD.encode(source)}],
        }))
        .unwrap();
        let sha = hex::encode(Sha256::digest(manifest.as_bytes()));
        self.mgmt
            .ok(
                reqwest::Method::PUT,
                &self.functions(&format!("/{name}")),
                Some(json!({})),
                &[200, 201],
            )
            .await;
        // The manifest's bytes as hashed: the orchestrator forwards them unchanged.
        let (status, body) = self
            .mgmt
            .raw(
                reqwest::Method::POST,
                &self.functions(&format!("/{name}/deployments")),
                Some(format!(
                    "{{\"manifest\": {manifest}, \"sha256\": \"{sha}\"}}"
                )),
            )
            .await;
        assert_eq!(status, 201, "deploy {name}: {body}");
        self.patch(name, json!({ "enabled": true })).await;
    }

    async fn call(&self, path: &str, key: bool) -> (u16, String) {
        let mut req = self.http.get(format!("{}{path}", self.api));
        if key {
            req = req.bearer_auth(&self.key);
        }
        let resp = req.send().await.expect("the gateway");
        (
            resp.status().as_u16(),
            resp.text().await.unwrap_or_default(),
        )
    }

    /// Calls `path` until `done` holds, for at most [`WITHIN`]; the last answer.
    async fn within(
        &self,
        path: &str,
        key: bool,
        done: impl Fn(u16, &str) -> bool,
    ) -> (u16, String) {
        let start = Instant::now();
        loop {
            let (status, body) = self.call(path, key).await;
            if done(status, &body) || start.elapsed() > WITHIN {
                return (status, body);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
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
        let id = self.db.as_uuid();
        for sql in [
            "DELETE FROM data_api_keys WHERE branch_id = $1",
            "DELETE FROM branches WHERE database_id = $1",
            "DELETE FROM databases WHERE id = $1",
        ] {
            let _ = sqlx::query(sql)
                .bind(id)
                .execute(self.metadata.pool())
                .await;
        }
    }
}

const HELLO: &str = r#"export default {
  async fetch(request, env) {
    return Response.json({ v: "VERSION", branch: env.CHELABASE_BRANCH_ID, greeting: env.GREETING ?? null,
                           path: new URL(request.url).pathname });
  },
};"#;

async fn flow(s: &Stack) {
    // 1. Deployed and enabled: the branch's public key runs it, with its secret and branch id.
    s.deploy("hello", &HELLO.replace("VERSION", "v1")).await;
    s.secret("hello", "GREETING", "hi").await;
    let (status, body) = s
        .within("/functions/v1/hello/sub", true, |st, b| {
            st == 200 && b.contains("\"hi\"")
        })
        .await;
    assert_eq!(status, 200, "{body}");
    let got: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        got,
        json!({"v": "v1", "branch": s.branch.to_string(), "greeting": "hi", "path": "/hello/sub"})
    );
    let (status, _) = s.call("/functions/v1/hello", false).await;
    assert_eq!(status, 401, "no key");

    // 2. Disabled: 404 within 2 s.
    s.patch("hello", json!({ "enabled": false })).await;
    let (status, body) = s
        .within("/functions/v1/hello", true, |st, _| st == 404)
        .await;
    assert_eq!(status, 404, "disabled: {body}");

    // 3. Enabled again with a new deployment: the new code answers within 2 s.
    s.deploy("hello", &HELLO.replace("VERSION", "v2")).await;
    let (status, body) = s
        .within("/functions/v1/hello", true, |st, b| {
            st == 200 && b.contains("\"v2\"")
        })
        .await;
    assert_eq!(
        (status, body.contains("\"v2\"")),
        (200, true),
        "redeployed: {body}"
    );

    // 4. A public function on the keyless form; a key-only one isn't reachable there.
    s.deploy(
        "open",
        r#"export default { fetch() { return new Response("open") } }"#,
    )
    .await;
    s.patch("open", json!({ "auth": "public" })).await;
    let path = format!("/functions/v1/public/{}/open", s.branch);
    let (status, body) = s.within(&path, false, |st, _| st == 200).await;
    assert_eq!((status, body.as_str()), (200, "open"));
    let (status, _) = s
        .call(&format!("/functions/v1/public/{}/hello", s.branch), false)
        .await;
    assert_eq!(status, 404, "a key-only function on the keyless form");
}

#[tokio::test]
#[ignore = "needs the workbench stack: just functions-e2e"]
async fn deploy_invoke_toggle_redeploy_and_public() {
    let stack = std::sync::Arc::new(Stack::new().await);
    let run = stack.clone();
    let result = tokio::spawn(async move { flow(&run).await }).await;
    stack.cleanup().await;
    if let Err(e) = result {
        std::panic::resume_unwind(e.into_panic());
    }
}
