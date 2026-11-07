//! Edge functions end to end on a workbench stack (#181): a function deployed on a branch,
//! invoked through the gateway with the branch's public key, disabled, redeployed, and a
//! public function called on the keyless form.
//!
//! ```text
//! just functions-e2e
//! ```
//!
//! It runs in the stack's runner (stack.env, the runner's NATS user and the orchestrator's
//! functions bearer). Each run makes its own database rows (a pending database: no compute is
//! needed, the gateway only resolves the key) and removes them at the end, pass or fail.
//!
//! TODO(#165): deploy, toggle and set auth through the management API once its functions
//! endpoints land. Until then the test writes the metadata directly, uploads the bundle with
//! the orchestrator's bearer as the orchestrator does, and publishes `chelabase.fn.changed`
//! itself.

// Test code: a plain client against the stack (clippy.toml allows tests only).
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use chelabase_metadata::{
    DEMO_PROJECT_ID, DEMO_TENANT_ID, FunctionAuth, FunctionKind, MetadataClient, MetadataConfig,
    NewDataKey, NewDeployment,
};
use chelabase_secrets::SecretKey;
use chelabase_types::data_key::{DataKeyKind, generate};
use chelabase_types::{BranchId, DatabaseId};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;

/// How soon a change must apply (NATS drops the cached resolution).
const WITHIN: Duration = Duration::from_secs(2);

fn env(name: &str) -> String {
    std::env::var(name)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| panic!("{name} is unset (run through `just functions-e2e`)"))
}

struct Stack {
    metadata: MetadataClient,
    http: reqwest::Client,
    nats: chelabase_nats::NatsConfig,
    api: String,
    internal: String,
    orchestrator_token: String,
    db: DatabaseId,
    branch: BranchId,
    key: String,
}

impl Stack {
    async fn new() -> Self {
        let metadata = MetadataClient::connect(&MetadataConfig {
            database_url: env("CHELABASE_METADATA_DATABASE_URL"),
        })
        .await
        .expect("metadata database")
        .with_secrets_key(SecretKey::from_env().expect("CHELABASE_SECRETS_KEY"));
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
        metadata
            .insert_data_key(&NewDataKey {
                id: Uuid::new_v4(),
                branch_id: branch,
                kind: DataKeyKind::Public,
                role: "anon".into(),
                name: "functions-e2e".into(),
                prefix: generated.prefix.clone(),
                sha256: generated.sha256,
                sealed: None,
                databases: None,
                role_oid: None,
                role_timeline: None,
                role_bypass_rls: false,
            })
            .await
            .expect("the branch's public key");
        Stack {
            metadata,
            http: reqwest::Client::new(),
            nats: chelabase_nats::NatsConfig::from_env().expect("the runner's NATS settings"),
            api: env("CHELABASE_ENTRY_API_URL")
                .trim_end_matches('/')
                .to_owned(),
            internal: env("CHELABASE_FUNCTIONS_INTERNAL_URL")
                .trim_end_matches('/')
                .to_owned(),
            orchestrator_token: chelabase_types::secret_env::env_secret(
                "CHELABASE_ORCHESTRATOR_FUNCTIONS_TOKEN",
            )
            .expect("CHELABASE_ORCHESTRATOR_FUNCTIONS_TOKEN_FILE")
            .expect("the orchestrator's functions bearer"),
            db,
            branch,
            key: generated.key,
        }
    }

    /// Uploads `source` as a one-module bundle and makes it the function's active, enabled
    /// deployment on the branch; returns the function's id.
    async fn deploy(&self, name: &str, source: &str) -> Uuid {
        let manifest = serde_json::to_vec(&json!({
            "compatibilityDate": "2026-10-01",
            "compatibilityFlags": [],
            "mainModule": "main.js",
            "modules": [{"name": "main.js", "type": "js", "base64": STANDARD.encode(source)}],
        }))
        .unwrap();
        let sha = hex::encode(Sha256::digest(&manifest));
        let resp = self
            .http
            .put(format!(
                "{}/internal/bundles/{}/{sha}",
                self.internal, self.db
            ))
            .bearer_auth(&self.orchestrator_token)
            .body(manifest.clone())
            .send()
            .await
            .expect("the functions service's internal listener");
        assert_eq!(
            resp.status(),
            204,
            "bundle upload: {}",
            resp.text().await.unwrap_or_default()
        );
        let function = match self
            .metadata
            .list_functions(self.db)
            .await
            .unwrap()
            .into_iter()
            .find(|f| f.name == name)
        {
            Some(f) => f.id,
            None => {
                self.metadata
                    .create_function(self.db, name, FunctionKind::Function)
                    .await
                    .unwrap()
                    .id
            }
        };
        let deployment = self
            .metadata
            .create_deployment(&NewDeployment {
                id: Uuid::new_v4(),
                function_id: function,
                bundle_sha256: Some(&sha),
                bundle_size: Some(manifest.len() as i64),
                spec: &json!({}),
                compatibility_date: sqlx::types::chrono::NaiveDate::from_ymd_opt(2026, 10, 1)
                    .unwrap(),
                created_by: "functions-e2e",
                note: None,
            })
            .await
            .unwrap();
        self.metadata
            .set_active_deployment(self.branch, function, deployment.id)
            .await
            .unwrap();
        assert!(
            self.metadata
                .set_enabled(self.branch, function, true)
                .await
                .unwrap()
        );
        self.changed(name).await;
        function
    }

    /// What the management API publishes after each write (TODO(#165)).
    async fn changed(&self, name: &str) {
        let client = chelabase_nats::connect(&self.nats).await.expect("NATS");
        chelabase_nats::fn_events::publish_changed(&client, self.branch.as_uuid(), name)
            .await
            .expect("publish");
        client.flush().await.expect("flush");
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
    let hello = s.deploy("hello", &HELLO.replace("VERSION", "v1")).await;
    s.metadata
        .put_secret(s.branch, hello, "GREETING", "hi")
        .await
        .unwrap();
    s.changed("hello").await;
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
    s.metadata
        .set_enabled(s.branch, hello, false)
        .await
        .unwrap();
    s.changed("hello").await;
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
    let open = s
        .deploy(
            "open",
            r#"export default { fetch() { return new Response("open") } }"#,
        )
        .await;
    assert!(
        s.metadata
            .set_function_auth(s.branch, open, FunctionAuth::Public)
            .await
            .unwrap()
    );
    s.changed("open").await;
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
