//! Idempotent bootstrap steps: each finds what exists and creates only what's missing.

use crate::dev::requests::*;
use crate::dev::zitadel::ZitadelApi;
use anyhow::{Context, Result, bail};
use reqwest::Method;
use serde_json::{Value, json};
use std::path::Path;
use tracing::info;

const PROJECT: &str = "cheladb";
const DEMO_USER: &str = "cheladb-demo";
const MAILPIT: &str = "mailpit:1025";
const PROJECTS: &str = "zitadel.project.v2.ProjectService";
const APPS: &str = "zitadel.application.v2.ApplicationService";

pub async fn ensure_org(api: &impl ZitadelApi) -> Result<String> {
    let listed = api
        .rest(
            Method::POST,
            "/v2/organizations/_search",
            default_org_query(),
        )
        .await?;
    org_id_from(&listed).with_context(|| format!("no default organization: {listed}"))
}

pub async fn ensure_project(api: &impl ZitadelApi, org_id: &str) -> Result<String> {
    let listed = api
        .connect(PROJECTS, "ListProjects", project_search(PROJECT))
        .await?;
    if let Some(id) = project_id_from(&listed) {
        return Ok(id);
    }
    let created = api
        .connect(PROJECTS, "CreateProject", project_create(org_id, PROJECT))
        .await?;
    info!("created project {PROJECT}");
    project_id_from(&created).with_context(|| format!("CreateProject: {created}"))
}

/// The app called `name` in the project, if any. One without a readable client id is an error:
/// creating another would leave two apps with the same name.
async fn existing_app(api: &impl ZitadelApi, project_id: &str, name: &str) -> Result<Option<App>> {
    let listed = api
        .connect(APPS, "ListApplications", apps_search(project_id))
        .await?;
    if let Some(app) = find_app(&listed, name) {
        return Ok(Some(app));
    }
    if let Some(id) = app_id_named(&listed, name) {
        bail!(
            "Zitadel has an app named {name} (id {id}) without a client id in its listing; \
             delete it in the console (or run `just zitadel-reset`) and re-run bootstrap"
        );
    }
    Ok(None)
}

/// The API app's client id (the gateway accepts it as a token audience).
pub async fn ensure_api_app(api: &impl ZitadelApi, project_id: &str) -> Result<String> {
    if let Some(app) = existing_app(api, project_id, API_APP).await? {
        return Ok(app.client_id);
    }
    let created = api
        .connect(
            APPS,
            "CreateApplication",
            api_app_create(project_id, API_APP),
        )
        .await?;
    info!("created API app {API_APP}");
    created_client_id(&created).with_context(|| format!("CreateApplication: {created}"))
}

/// The Studio app's client id; its redirect settings are corrected if they drifted.
pub async fn ensure_studio_app(
    api: &impl ZitadelApi,
    project_id: &str,
    origin: &str,
) -> Result<String> {
    if let Some(app) = existing_app(api, project_id, STUDIO_APP).await? {
        let drift = studio_app_drift(&app.raw, origin);
        if !drift.is_empty() {
            api.connect(
                APPS,
                "UpdateApplication",
                studio_app_update(project_id, &app.id, origin),
            )
            .await?;
            info!(?drift, "updated the Studio app");
        }
        return Ok(app.client_id);
    }
    let created = api
        .connect(
            APPS,
            "CreateApplication",
            studio_app_create(project_id, origin),
        )
        .await?;
    info!("created Studio app {STUDIO_APP}");
    created_client_id(&created).with_context(|| format!("CreateApplication: {created}"))
}

pub async fn ensure_demo_user(api: &impl ZitadelApi, org_id: &str) -> Result<String> {
    let listed = api
        .rest(Method::POST, "/v2/users", machine_user_search(DEMO_USER))
        .await?;
    if let Some(id) = user_id_from(&listed) {
        return Ok(id);
    }
    let created = api
        .rest(
            Method::POST,
            "/v2/users/new",
            machine_user_create(org_id, DEMO_USER),
        )
        .await?;
    info!("created machine user {DEMO_USER}");
    user_id_from(&created).with_context(|| format!("CreateUser: {created}"))
}

/// Keeps `path` if it is a key for `user_id`; otherwise adds a key and writes it (0600).
pub async fn ensure_demo_key(api: &impl ZitadelApi, user_id: &str, path: &Path) -> Result<()> {
    if let Ok(text) = std::fs::read_to_string(path)
        && serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|v| v.get("userId").cloned())
            == Some(json!(user_id))
    {
        return Ok(());
    }
    let created = api
        .rest(
            Method::POST,
            &format!("/v2/users/{user_id}/keys"),
            key_create(),
        )
        .await?;
    let file = key_file_from(&created)?;
    write_private(path, &file)?;
    info!("wrote {}", path.display());
    Ok(())
}

pub async fn ensure_smtp(api: &impl ZitadelApi) -> Result<()> {
    let listed = api
        .rest(Method::POST, "/admin/v1/smtp/_search", json!({}))
        .await?;
    let id = match find_smtp(&listed, MAILPIT) {
        Some((_, true)) => return Ok(()),
        Some((id, false)) => id,
        None => {
            let created = api
                .rest(Method::POST, "/admin/v1/smtp", smtp_payload(MAILPIT))
                .await?;
            created
                .get("id")
                .and_then(Value::as_str)
                .map(str::to_string)
                .with_context(|| format!("create SMTP: {created}"))?
        }
    };
    api.rest(
        Method::POST,
        &format!("/admin/v1/smtp/{id}/_activate"),
        json!({}),
    )
    .await?;
    info!("Mailpit SMTP active");
    Ok(())
}

fn write_private(path: &Path, contents: &str) -> Result<()> {
    std::fs::write(path, contents).with_context(|| format!("write {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::dev::requests::studio_app_config;
    use reqwest::Method;
    use serde_json::{Value, json};
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// Answers by call key (`"POST /v2/organizations/_search"` or `"Service/Method"`), records calls.
    #[derive(Default)]
    struct Fake {
        answers: HashMap<String, Value>,
        calls: Mutex<Vec<String>>,
    }

    impl Fake {
        fn with(mut self, key: &str, answer: Value) -> Self {
            self.answers.insert(key.to_string(), answer);
            self
        }
        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
        fn answer(&self, key: String) -> anyhow::Result<Value> {
            self.calls.lock().unwrap().push(key.clone());
            self.answers
                .get(&key)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("unexpected call {key}"))
        }
    }

    impl ZitadelApi for Fake {
        async fn rest(&self, method: Method, path: &str, _body: Value) -> anyhow::Result<Value> {
            self.answer(format!("{method} {path}"))
        }
        async fn connect(
            &self,
            service: &str,
            method: &str,
            _body: Value,
        ) -> anyhow::Result<Value> {
            self.answer(format!("{}/{method}", service.rsplit('.').next().unwrap()))
        }
    }

    const ORIGIN: &str = "http://localhost:3000";

    fn existing() -> Fake {
        Fake::default()
            .with("POST /v2/organizations/_search", json!({"result": [{"id": "o1"}]}))
            .with("ProjectService/ListProjects", json!({"projects": [{"projectId": "p1"}]}))
            .with("ApplicationService/ListApplications", json!({"applications": [
                {"applicationId": "a1", "name": "cheladb-orchestrator", "apiConfiguration": {"clientId": "c-api"}},
                {"applicationId": "a2", "name": "cheladb-studio", "oidcConfiguration": {
                    "clientId": "c-spa", "redirectUris": studio_app_config(ORIGIN)["redirectUris"],
                    "postLogoutRedirectUris": studio_app_config(ORIGIN)["postLogoutRedirectUris"],
                    "accessTokenType": "OIDC_TOKEN_TYPE_JWT", "developmentMode": true}}]}))
            .with("POST /v2/users", json!({"result": [{"userId": "u1"}]}))
            .with("POST /admin/v1/smtp/_search", json!({"result": [{"id": "s1", "host": "mailpit:1025", "state": "SMTP_CONFIG_ACTIVE"}]}))
    }

    #[tokio::test]
    async fn a_second_run_creates_nothing() {
        let fake = existing();
        assert_eq!(ensure_org(&fake).await.unwrap(), "o1");
        assert_eq!(ensure_project(&fake, "o1").await.unwrap(), "p1");
        assert_eq!(ensure_api_app(&fake, "p1").await.unwrap(), "c-api");
        assert_eq!(
            ensure_studio_app(&fake, "p1", ORIGIN).await.unwrap(),
            "c-spa"
        );
        assert_eq!(ensure_demo_user(&fake, "o1").await.unwrap(), "u1");
        ensure_smtp(&fake).await.unwrap();
        let writes: Vec<String> = fake
            .calls()
            .into_iter()
            .filter(|c| !c.contains("_search") && !c.contains("/List") && c != "POST /v2/users")
            .collect();
        assert!(writes.is_empty(), "unexpected writes: {writes:?}");
    }

    #[tokio::test]
    async fn a_first_run_creates_everything() {
        let fake = Fake::default()
            .with(
                "POST /v2/organizations/_search",
                json!({"result": [{"id": "o1"}]}),
            )
            .with("ProjectService/ListProjects", json!({"projects": []}))
            .with("ProjectService/CreateProject", json!({"projectId": "p1"}))
            .with(
                "ApplicationService/ListApplications",
                json!({"applications": []}),
            )
            .with(
                "ApplicationService/CreateApplication",
                json!({"applicationId": "a9", "apiConfiguration": {"clientId": "c-new"}}),
            )
            .with("POST /v2/users", json!({"result": []}))
            .with("POST /v2/users/new", json!({"id": "u9"}))
            .with("POST /admin/v1/smtp/_search", json!({"result": []}))
            .with("POST /admin/v1/smtp", json!({"id": "s9"}))
            .with("POST /admin/v1/smtp/s9/_activate", json!({}));
        assert_eq!(ensure_project(&fake, "o1").await.unwrap(), "p1");
        assert_eq!(ensure_api_app(&fake, "p1").await.unwrap(), "c-new");
        assert_eq!(ensure_demo_user(&fake, "o1").await.unwrap(), "u9");
        ensure_smtp(&fake).await.unwrap();
        let calls = fake.calls();
        for created in [
            "ProjectService/CreateProject",
            "ApplicationService/CreateApplication",
            "POST /v2/users/new",
            "POST /admin/v1/smtp",
            "POST /admin/v1/smtp/s9/_activate",
        ] {
            assert!(
                calls.iter().any(|c| c == created),
                "missing {created} in {calls:?}"
            );
        }
    }

    #[tokio::test]
    async fn a_drifted_studio_app_is_updated_in_place() {
        let fake = existing()
            .with("ApplicationService/ListApplications", json!({"applications": [
                {"applicationId": "a2", "name": "cheladb-studio", "oidcConfiguration": {"clientId": "c-spa", "redirectUris": ["http://old/callback"]}}]}))
            .with("ApplicationService/UpdateApplication", json!({}));
        assert_eq!(
            ensure_studio_app(&fake, "p1", ORIGIN).await.unwrap(),
            "c-spa"
        );
        assert!(
            fake.calls()
                .contains(&"ApplicationService/UpdateApplication".to_string())
        );
    }

    // An app we can't read a client id from must not be "created" again next to itself.
    #[tokio::test]
    async fn a_named_app_without_a_client_id_is_an_error_not_a_duplicate() {
        let fake = Fake::default().with(
            "ApplicationService/ListApplications",
            json!({"applications": [{"applicationId": "a2", "name": "cheladb-studio"}]}),
        );
        let err = ensure_studio_app(&fake, "p1", ORIGIN)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("cheladb-studio") && err.contains("a2"),
            "{err}"
        );
        assert!(
            !fake.calls().iter().any(|c| c.contains("Create")),
            "{:?}",
            fake.calls()
        );
    }

    #[tokio::test]
    async fn the_demo_key_is_kept_when_its_file_matches_the_user() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("demo-sa.json");
        std::fs::write(&path, r#"{"keyId":"k1","key":"PEM","userId":"u1"}"#).unwrap();
        let fake = Fake::default();
        ensure_demo_key(&fake, "u1", &path).await.unwrap();
        assert!(fake.calls().is_empty());
    }

    #[tokio::test]
    async fn a_missing_or_foreign_demo_key_is_replaced() {
        use base64::Engine;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("demo-sa.json");
        std::fs::write(
            &path,
            r#"{"keyId":"k0","key":"PEM","userId":"someone-else"}"#,
        )
        .unwrap();
        let file = r#"{"type":"serviceaccount","keyId":"k1","key":"PEM","userId":"u1"}"#;
        let fake = Fake::default().with(
            "POST /v2/users/u1/keys",
            json!({"keyId": "k1", "keyContent": base64::engine::general_purpose::STANDARD.encode(file)}),
        );
        ensure_demo_key(&fake, "u1", &path).await.unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), file);
    }
}
