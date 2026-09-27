//! Idempotent local Zitadel bootstrap (Management API ensure + demo tenant bind).

use super::dev_email::{
    action_payload, find_action, skip_enabled, trigger_actions_with, ACTION_NAME, ENV_VAR,
    INTERNAL_AUTH_FLOW, PRE_CREATION_TRIGGER,
};
use super::studio::{
    find_smtp_config, render_studio_env, smtp_payload, studio_app_drift, studio_app_payload,
    StudioEnv, GATEWAY_URL, MAILPIT_SMTP_HOST, STUDIO_APP_NAME, STUDIO_ORIGIN,
};
use crate::paths::{
    compose_env, compose_file, demo_tenant_id, gateway_dir, issuer, secrets_dir, studio_env_file,
    zitadel_dir,
};
use anyhow::{bail, Context, Result};
use base64::Engine;
use serde_json::{json, Value};
use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::Duration;
use tracing::{info, warn};

const PROJECT_NAME: &str = "cheladb";
const API_APP_NAME: &str = "cheladb-orchestrator";
const DEMO_USER: &str = "cheladb-demo";
const KEY_EXPIRY: &str = "2099-01-01T00:00:00Z";

struct ApiClient {
    http: reqwest::Client,
    issuer: String,
    admin_pat: String,
    org_id: Option<String>,
}

impl ApiClient {
    async fn request(&self, method: reqwest::Method, path: &str, body: Option<Value>) -> Result<Value> {
        let url = format!("{}{}", self.issuer, path);
        let mut req = self
            .http
            .request(method, &url)
            .header("Authorization", format!("Bearer {}", self.admin_pat))
            .header("Accept", "application/json")
            .header("Content-Type", "application/json");
        if let Some(org) = &self.org_id {
            req = req.header("x-zitadel-orgid", org);
        }
        if let Some(b) = body {
            req = req.json(&b);
        }
        let resp = req.send().await.with_context(|| path.to_string())?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            bail!("{path} -> {status}: {text}");
        }
        if text.trim().is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_str(&text).with_context(|| format!("parse {path}: {text}"))
    }

    async fn get(&self, path: &str) -> Result<Value> {
        self.request(reqwest::Method::GET, path, None).await
    }

    async fn post(&self, path: &str, body: Value) -> Result<Value> {
        self.request(reqwest::Method::POST, path, Some(body)).await
    }
}

pub async fn run() -> Result<()> {
    let iss = issuer();
    let zdir = zitadel_dir();
    fs::create_dir_all(&zdir).with_context(|| format!("mkdir {}", zdir.display()))?;

    // Independent of Zitadel: generate first so it exists even if IAM steps fail.
    let internal_keys = super::internal_keys::ensure(&gateway_dir())?;
    let secrets_key = super::secrets_key::ensure(&secrets_dir())?;

    wait_jwks(&iss).await?;

    let admin_pat_path = zdir.join("admin.pat");
    wait_admin_pat(&admin_pat_path).await?;
    let admin_pat = fs::read_to_string(&admin_pat_path)
        .with_context(|| format!("read {}", admin_pat_path.display()))?
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect::<String>();
    if admin_pat.is_empty() {
        bail!("admin.pat is empty");
    }

    let mut api = ApiClient {
        http: reqwest::Client::new(),
        issuer: iss.clone(),
        admin_pat,
        org_id: None,
    };

    info!("waiting for Zitadel API…");
    wait_api_ready(
        &format!("{iss}/auth/v1/users/me"),
        &api.admin_pat,
        60,
        Duration::from_secs(1),
    )
    .await?;

    info!("loading bootstrap caller identity…");
    let me = api.get("/auth/v1/users/me").await?;
    let org_id = me
        .pointer("/user/details/resourceOwner")
        .or_else(|| me.pointer("/details/resourceOwner"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .with_context(|| format!("could not resolve org id from /auth/v1/users/me: {me}"))?;
    info!(%org_id, "resolved org");
    api.org_id = Some(org_id.clone());

    // --- Project ---
    let projects = api
        .post(
            "/management/v1/projects/_search",
            json!({"query":{"offset":"0","limit":100,"asc":true}}),
        )
        .await?;
    let mut project_id = find_named_id(&projects, PROJECT_NAME);
    if project_id.is_none() {
        info!("creating project {PROJECT_NAME}…");
        let created = api
            .post("/management/v1/projects", json!({"name": PROJECT_NAME}))
            .await?;
        project_id = created.get("id").and_then(|v| v.as_str()).map(str::to_string);
        if project_id.is_none() {
            bail!("create project failed: {created}");
        }
    } else {
        info!(
            "project {PROJECT_NAME} exists ({})",
            project_id.as_deref().unwrap()
        );
    }
    let project_id = project_id.unwrap();

    // --- API application ---
    let apps = api
        .post(
            &format!("/management/v1/projects/{project_id}/apps/_search"),
            json!({"query":{"offset":"0","limit":100,"asc":true}}),
        )
        .await?;
    let (mut app_id, mut client_id) = find_api_app(&apps, API_APP_NAME);
    if client_id.is_none() {
        info!("creating API app {API_APP_NAME}…");
        let created = api
            .post(
                &format!("/management/v1/projects/{project_id}/apps/api"),
                json!({
                    "name": API_APP_NAME,
                    "authMethodType": "API_AUTH_METHOD_TYPE_PRIVATE_KEY_JWT"
                }),
            )
            .await?;
        client_id = created
            .get("clientId")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        app_id = created
            .get("appId")
            .or_else(|| created.get("id"))
            .and_then(|v| v.as_str())
            .map(str::to_string);
        if client_id.is_none() {
            bail!("create API app failed: {created}");
        }
    } else {
        info!(
            "API app {API_APP_NAME} exists (client_id={})",
            client_id.as_deref().unwrap()
        );
    }
    let client_id = client_id.unwrap();

    // --- Studio SPA app (OIDC, PKCE, JWT access tokens) ---
    let (_, mut studio_client_id) = find_api_app(&apps, STUDIO_APP_NAME);
    if studio_client_id.is_none() {
        info!("creating OIDC app {STUDIO_APP_NAME}…");
        let created = api
            .post(
                &format!("/management/v1/projects/{project_id}/apps/oidc"),
                studio_app_payload(STUDIO_ORIGIN),
            )
            .await?;
        studio_client_id = created
            .get("clientId")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        if studio_client_id.is_none() {
            bail!("create OIDC app failed: {created}");
        }
    } else {
        info!(
            "OIDC app {STUDIO_APP_NAME} exists (client_id={})",
            studio_client_id.as_deref().unwrap()
        );
        // Bootstrap never edits an existing app; say so if it has drifted.
        if let Some(app) = find_named(&apps, STUDIO_APP_NAME) {
            let drift = studio_app_drift(app, STUDIO_ORIGIN);
            for line in &drift {
                warn!("{STUDIO_APP_NAME} differs from what bootstrap would create — {line}");
            }
            if !drift.is_empty() {
                warn!(
                    "fix it in the Zitadel console, or delete the app there and re-run \
                     `just zitadel-bootstrap` (it will be recreated)"
                );
            }
        }
    }
    let studio_client_id = studio_client_id.unwrap();

    // --- SMTP → Mailpit (signup verification emails in local dev) ---
    ensure_mailpit_smtp(&api).await?;

    // --- Optional dev switch: skip email verification for self-signups ---
    ensure_email_verification_mode(&api, skip_enabled(std::env::var(ENV_VAR).ok().as_deref()))
        .await?;

    // --- Demo machine user ---
    let users = api
        .post(
            "/management/v1/users/_search",
            json!({
                "query":{"offset":"0","limit":100,"asc":true},
                "queries":[{
                    "userNameQuery":{
                        "userName": DEMO_USER,
                        "method":"TEXT_QUERY_METHOD_EQUALS"
                    }
                }]
            }),
        )
        .await?;
    let mut demo_user_id = find_user_id(&users, DEMO_USER);
    if demo_user_id.is_none() {
        info!("creating machine user {DEMO_USER}…");
        let created = api
            .post(
                "/management/v1/users/machine",
                json!({
                    "userName": DEMO_USER,
                    "name": "Cheladb Demo",
                    "description": "Local demo API caller",
                    "accessTokenType": "ACCESS_TOKEN_TYPE_JWT"
                }),
            )
            .await?;
        demo_user_id = created
            .get("userId")
            .or_else(|| created.get("id"))
            .and_then(|v| v.as_str())
            .map(str::to_string);
        if demo_user_id.is_none() {
            bail!("create machine user failed: {created}");
        }
    } else {
        info!(
            "machine user {DEMO_USER} exists ({})",
            demo_user_id.as_deref().unwrap()
        );
    }
    let demo_user_id = demo_user_id.unwrap();

    let sa_path = zdir.join("demo-sa.json");
    if sa_path.is_file() {
        info!("reusing machine key {}", sa_path.display());
    } else {
        info!("creating JWT profile machine key for {DEMO_USER}…");
        let key_json = api
            .post(
                &format!("/management/v1/users/{demo_user_id}/keys"),
                json!({"type":"KEY_TYPE_JSON","expirationDate": KEY_EXPIRY}),
            )
            .await?;
        let key_b64 = key_json
            .get("keyDetails")
            .and_then(|v| v.as_str())
            .context(format!("create machine key failed: {key_json}"))?;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(key_b64)
            .context("decode keyDetails")?;
        fs::write(&sa_path, &bytes).with_context(|| format!("write {}", sa_path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&sa_path)?.permissions();
            perms.set_mode(0o600);
            fs::set_permissions(&sa_path, perms)?;
        }
        info!("wrote {}", sa_path.display());
    }

    let audience = format!("{project_id},{client_id}");
    let demo_tenant = demo_tenant_id();
    bind_demo_tenant(&org_id, &demo_tenant)?;

    let demo_env = zdir.join("demo.env");
    let app_id_s = app_id.unwrap_or_default();
    let contents = format!(
        "CHELADB_ZITADEL_ISSUER={iss}\n\
CHELADB_ZITADEL_JWKS_URL={iss}/oauth/v2/keys\n\
CHELADB_ZITADEL_AUDIENCES={audience}\n\
CHELADB_ZITADEL_ORG_ID={org_id}\n\
CHELADB_ZITADEL_PROJECT_ID={project_id}\n\
CHELADB_ZITADEL_API_APP_ID={app_id_s}\n\
CHELADB_ZITADEL_API_CLIENT_ID={client_id}\n\
CHELADB_DEMO_USER_ID={demo_user_id}\n\
CHELADB_DEMO_TENANT_ID={demo_tenant}\n\
CHELADB_INTERNAL_SIGNING_KEY={signing_key}\n\
CHELADB_INTERNAL_VERIFY_KEY={verify_key}\n\
CHELADB_SECRETS_KEY={secrets_key}\n",
        signing_key = internal_keys.signing.display(),
        verify_key = internal_keys.verify.display(),
        secrets_key = secrets_key.display(),
    );
    fs::write(&demo_env, contents).with_context(|| format!("write {}", demo_env.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&demo_env)?.permissions();
        perms.set_mode(0o600);
        fs::set_permissions(&demo_env, perms)?;
    }

    info!("wrote {}", demo_env.display());

    let studio_env = studio_env_file();
    let studio_contents = render_studio_env(&StudioEnv {
        issuer: &iss,
        client_id: &studio_client_id,
        project_id: &project_id,
        org_id: &org_id,
        api_url: GATEWAY_URL,
    });
    fs::write(&studio_env, studio_contents)
        .with_context(|| format!("write {}", studio_env.display()))?;
    info!("wrote {}", studio_env.display());
    info!("mint a demo access token with: cargo run -q -p cheladb -- auth token");
    info!("bootstrap complete (idempotent)");
    Ok(())
}

async fn wait_jwks(issuer: &str) -> Result<()> {
    info!("waiting for Zitadel JWKS at {issuer}/oauth/v2/keys …");
    let client = reqwest::Client::new();
    for _ in 0..60 {
        if client
            .get(format!("{issuer}/oauth/v2/keys"))
            .send()
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false)
        {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    bail!("Zitadel not ready at {issuer}");
}

/// Point Zitadel's email at Mailpit (create or activate; never duplicate).
async fn ensure_mailpit_smtp(api: &ApiClient) -> Result<()> {
    let search = api.post("/admin/v1/smtp/_search", json!({})).await?;
    let existing = find_smtp_config(&search, MAILPIT_SMTP_HOST);
    let id = match &existing {
        Some(c) if c.has_credentials && c.active => {
            info!("SMTP → {MAILPIT_SMTP_HOST} already active");
            return Ok(());
        }
        Some(c) if c.has_credentials => c.id.clone(),
        Some(c) => {
            warn!(
                "SMTP config {} has no credentials, so Zitadel never sends mail with it; replacing it",
                c.id
            );
            create_mailpit_smtp(api).await?
        }
        None => create_mailpit_smtp(api).await?,
    };
    api.post(&format!("/admin/v1/smtp/{id}/_activate"), json!({}))
        .await?;
    if let Some(stale) = existing.filter(|c| !c.has_credentials) {
        api.request(reqwest::Method::DELETE, &format!("/admin/v1/smtp/{}", stale.id), None)
            .await?;
    }
    info!("activated SMTP → {MAILPIT_SMTP_HOST} (read mail at http://localhost:8025)");
    Ok(())
}

/// Install/activate the skip-verification Action when `skip` is set; otherwise
/// deactivate it if an earlier run installed it.
async fn ensure_email_verification_mode(api: &ApiClient, skip: bool) -> Result<()> {
    let search = api.post("/management/v1/actions/_search", json!({})).await?;
    let existing = find_action(&search, ACTION_NAME);

    if !skip {
        if let Some((id, true)) = existing {
            api.post(&format!("/management/v1/actions/{id}/_deactivate"), json!({}))
                .await?;
            info!("email verification required again (deactivated {ACTION_NAME})");
        }
        return Ok(());
    }

    let id = match existing {
        Some((id, active)) => {
            if !active {
                api.post(&format!("/management/v1/actions/{id}/_reactivate"), json!({}))
                    .await?;
            }
            id
        }
        None => {
            let created = api.post("/management/v1/actions", action_payload()).await?;
            created
                .get("id")
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .with_context(|| format!("create action failed: {created}"))?
        }
    };
    let flow = api
        .get(&format!("/management/v1/flows/{INTERNAL_AUTH_FLOW}"))
        .await?;
    let action_ids = trigger_actions_with(&flow, PRE_CREATION_TRIGGER, &id);
    api.post(
        &format!("/management/v1/flows/{INTERNAL_AUTH_FLOW}/trigger/{PRE_CREATION_TRIGGER}"),
        json!({ "actionIds": action_ids }),
    )
    .await?;
    warn!("{ENV_VAR} is on: self-signups skip email verification (local dev only)");
    Ok(())
}

async fn create_mailpit_smtp(api: &ApiClient) -> Result<String> {
    let created = api
        .post("/admin/v1/smtp", smtp_payload(MAILPIT_SMTP_HOST))
        .await?;
    created
        .get("id")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .with_context(|| format!("create SMTP config failed: {created}"))
}

/// The entry named `name` in a Zitadel `_search` result.
fn find_named<'a>(search: &'a Value, name: &str) -> Option<&'a Value> {
    search
        .get("result")?
        .as_array()?
        .iter()
        .find(|v| v.get("name").and_then(|n| n.as_str()) == Some(name))
}

/// Poll `url` (authenticated with `bearer`) until Zitadel's API answers
/// without a 5xx. Zitadel serves JWKS a moment before its REST gateway can
/// reach its own gRPC backend, so a JWKS 200 alone doesn't mean the
/// Management/Auth APIs are ready.
async fn wait_api_ready(url: &str, bearer: &str, attempts: u32, delay: Duration) -> Result<()> {
    let client = reqwest::Client::new();
    let mut last = String::from("no response");
    for attempt in 0..attempts {
        match client.get(url).bearer_auth(bearer).send().await {
            Ok(resp) if !resp.status().is_server_error() => return Ok(()),
            Ok(resp) => last = resp.status().to_string(),
            Err(err) => last = err.to_string(),
        }
        if attempt + 1 < attempts {
            tokio::time::sleep(delay).await;
        }
    }
    bail!("Zitadel API not ready at {url} after {attempts} attempts (last: {last})")
}

async fn wait_admin_pat(path: &Path) -> Result<()> {
    if path.is_file() && path.metadata()?.len() > 0 {
        return Ok(());
    }
    info!("waiting for FirstInstance admin.pat at {}…", path.display());
    for _ in 0..45 {
        if path.is_file() && path.metadata().map(|m| m.len() > 0).unwrap_or(false) {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    bail!(
        "missing {}. FirstInstance PAT is only written on a fresh Zitadel volume. Run: just zitadel-reset",
        path.display()
    );
}

fn find_named_id(search: &Value, name: &str) -> Option<String> {
    search
        .get("result")?
        .as_array()?
        .iter()
        .find(|p| p.get("name").and_then(|n| n.as_str()) == Some(name))
        .and_then(|p| p.get("id").and_then(|id| id.as_str()).map(str::to_string))
}

fn find_api_app(search: &Value, name: &str) -> (Option<String>, Option<String>) {
    let Some(arr) = search.get("result").and_then(|r| r.as_array()) else {
        return (None, None);
    };
    for app in arr {
        if app.get("name").and_then(|n| n.as_str()) != Some(name) {
            continue;
        }
        let id = app.get("id").and_then(|v| v.as_str()).map(str::to_string);
        let client_id = app
            .pointer("/apiConfig/clientId")
            .or_else(|| app.pointer("/oidcConfig/clientId"))
            .and_then(|v| v.as_str())
            .map(str::to_string);
        return (id, client_id);
    }
    (None, None)
}

fn find_user_id(search: &Value, username: &str) -> Option<String> {
    search
        .get("result")?
        .as_array()?
        .iter()
        .find(|u| u.get("userName").and_then(|n| n.as_str()) == Some(username))
        .and_then(|u| u.get("id").and_then(|id| id.as_str()).map(str::to_string))
}

fn bind_demo_tenant(org_id: &str, demo_tenant: &str) -> Result<()> {
    let compose = compose_file();
    let env_file = compose_env();

    let ready = Command::new("docker")
        .args([
            "compose",
            "-f",
            compose.to_str().unwrap(),
            "--env-file",
            env_file.to_str().unwrap(),
            "exec",
            "-T",
            "metadata-db",
            "pg_isready",
            "-U",
            "cheladb",
            "-d",
            "cheladb_metadata",
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    if !ready {
        warn!("metadata Postgres not ready — skipped tenant bind (re-run after deps-up)");
        return Ok(());
    }

    let col = Command::new("docker")
        .args([
            "compose",
            "-f",
            compose.to_str().unwrap(),
            "--env-file",
            env_file.to_str().unwrap(),
            "exec",
            "-T",
            "metadata-db",
            "psql",
            "-U",
            "cheladb",
            "-d",
            "cheladb_metadata",
            "-tAc",
            "SELECT 1 FROM information_schema.columns WHERE table_name='tenants' AND column_name='zitadel_org_id'",
        ])
        .output()
        .context("check zitadel_org_id column")?;

    let col_exists = String::from_utf8_lossy(&col.stdout).trim() == "1";
    if !col_exists {
        warn!("tenants.zitadel_org_id missing — start orchestrator once (migrate), then re-run bootstrap");
        return Ok(());
    }

    let sql = format!(
        "UPDATE tenants SET zitadel_org_id = '{org_id}' WHERE id = '{demo_tenant}';"
    );
    let status = Command::new("docker")
        .args([
            "compose",
            "-f",
            compose.to_str().unwrap(),
            "--env-file",
            env_file.to_str().unwrap(),
            "exec",
            "-T",
            "metadata-db",
            "psql",
            "-U",
            "cheladb",
            "-d",
            "cheladb_metadata",
            "-v",
            "ON_ERROR_STOP=1",
            "-c",
            &sql,
        ])
        .stdout(std::process::Stdio::null())
        .status()
        .context("bind demo tenant")?;

    if !status.success() {
        bail!("failed to UPDATE tenants.zitadel_org_id");
    }
    info!("bound demo tenant {demo_tenant} → zitadel_org_id={org_id}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::StatusCode;
    use axum::routing::get;
    use axum::Router;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// Serves `/api` with 503 for the first `failures` calls, then 200.
    async fn flaky_api(failures: usize) -> (String, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let app = Router::new().route(
            "/api",
            get(move || {
                let counter = counter.clone();
                async move {
                    if counter.fetch_add(1, Ordering::SeqCst) < failures {
                        StatusCode::SERVICE_UNAVAILABLE
                    } else {
                        StatusCode::OK
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{addr}/api"), calls)
    }

    #[tokio::test]
    async fn waits_through_transient_503s_until_the_api_answers() {
        let (url, calls) = flaky_api(2).await;
        wait_api_ready(&url, "pat", 10, Duration::from_millis(10))
            .await
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn gives_up_when_the_api_never_recovers() {
        let (url, calls) = flaky_api(usize::MAX).await;
        let err = wait_api_ready(&url, "pat", 3, Duration::from_millis(10))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not ready"), "{err}");
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn retries_while_nothing_is_listening_yet() {
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let url = format!("http://127.0.0.1:{port}/api");
        assert!(wait_api_ready(&url, "pat", 2, Duration::from_millis(10))
            .await
            .is_err());
    }
}
