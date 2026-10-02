//! Idempotent bootstrap steps: each finds what exists and creates only what's missing.

use crate::dev::requests::*;
use crate::dev::zitadel::ZitadelApi;
use anyhow::{Context, Result, bail};
use cheladb_types::mode::Mode;
use reqwest::Method;
use serde_json::{Value, json};
use std::path::Path;
use tracing::info;

const PROJECT: &str = "cheladb";
const DEMO_USER: &str = "cheladb-demo";
const MAILPIT: &str = "mailpit:1025";
const PROJECTS: &str = "zitadel.project.v2.ProjectService";
const APPS: &str = "zitadel.application.v2.ApplicationService";
const FEATURES: &str = "zitadel.feature.v2.FeatureService";

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

/// Worktree slot `slot`'s Studio app (slot 0's is `cheladb-studio`): its client id; its
/// redirect and login settings are corrected if they drifted.
pub async fn ensure_studio_app(
    api: &impl ZitadelApi,
    project_id: &str,
    slot: u16,
    mode: Mode,
) -> Result<String> {
    let name = studio_app_name(slot);
    if let Some(app) = existing_app(api, project_id, &name).await? {
        let drift = studio_app_drift(&app.raw, slot, mode);
        if !drift.is_empty() {
            api.connect(
                APPS,
                "UpdateApplication",
                studio_app_update(project_id, &app.id, slot, mode),
            )
            .await?;
            info!(?drift, "updated the Studio app {name}");
        }
        return Ok(app.client_id);
    }
    let created = api
        .connect(
            APPS,
            "CreateApplication",
            studio_app_create(project_id, slot, mode),
        )
        .await?;
    info!("created Studio app {name}");
    created_client_id(&created).with_context(|| format!("CreateApplication: {created}"))
}

/// W8, self-hosted mode only: every Studio app (any slot's) gets its own login base URI,
/// bootstrap checks that none is left without one, then switches off the instance's
/// "login v2 required", which otherwise replaces each app's base URI with the instance's
/// (slot 0's Studio). Platform mode leaves the instance and the other slots' apps alone.
pub async fn ensure_studio_logins(
    api: &impl ZitadelApi,
    project_id: &str,
    mode: Mode,
) -> Result<()> {
    if mode == Mode::Platform {
        return Ok(());
    }
    let listed = api
        .connect(APPS, "ListApplications", apps_search(project_id))
        .await?;
    let mut slots: Vec<u16> = listed
        .get("applications")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|a| a.get("name").and_then(Value::as_str))
        .filter_map(studio_slot_from_name)
        .collect();
    slots.sort_unstable();
    slots.dedup();
    for slot in slots {
        ensure_studio_app(api, project_id, slot, mode).await?;
    }
    // Checked against what Zitadel now stores, before the instance stops forcing its login.
    let listed = api
        .connect(APPS, "ListApplications", apps_search(project_id))
        .await?;
    let missing = studio_apps_without_login(&listed);
    if !missing.is_empty() {
        bail!(
            "Studio apps without a login v2 base URI: {}. They would fall back to Zitadel's \
             v1 login; fix them in the console (Login version: v2, base URI \
             http://localhost:3N00/) or run `just zitadel-reset`, then re-run bootstrap",
            missing.join(", ")
        );
    }
    let features = api
        .connect(FEATURES, "GetInstanceFeatures", instance_features_get())
        .await?;
    let (required, base_uri) = login_v2_from(&features);
    if required {
        let base_uri = base_uri.unwrap_or_else(|| format!("{}/", studio_origin(0)));
        api.connect(
            FEATURES,
            "SetInstanceFeatures",
            login_v2_optional(&base_uri),
        )
        .await?;
        info!("login v2 is no longer required instance-wide: each Studio app uses its own login");
    }
    Ok(())
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
    use cheladb_types::mode::Mode;
    use reqwest::Method;
    use serde_json::{Value, json};
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// Answers by call key (`"POST /v2/organizations/_search"` or `"Service/Method"`), records
    /// calls and their bodies. A key with several answers gives them in order, then repeats
    /// the last.
    #[derive(Default)]
    struct Fake {
        answers: Mutex<HashMap<String, Vec<Value>>>,
        calls: Mutex<Vec<(String, Value)>>,
    }

    impl Fake {
        fn with(self, key: &str, answer: Value) -> Self {
            self.answers
                .lock()
                .unwrap()
                .insert(key.to_string(), vec![answer]);
            self
        }
        fn then(self, key: &str, answer: Value) -> Self {
            self.answers
                .lock()
                .unwrap()
                .entry(key.to_string())
                .or_default()
                .push(answer);
            self
        }
        fn calls(&self) -> Vec<String> {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .map(|(k, _)| k.clone())
                .collect()
        }
        fn bodies(&self, key: &str) -> Vec<Value> {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .filter(|(k, _)| k == key)
                .map(|(_, b)| b.clone())
                .collect()
        }
        fn answer(&self, key: String, body: Value) -> anyhow::Result<Value> {
            self.calls.lock().unwrap().push((key.clone(), body));
            let mut answers = self.answers.lock().unwrap();
            let queue = answers
                .get_mut(&key)
                .ok_or_else(|| anyhow::anyhow!("unexpected call {key}"))?;
            Ok(if queue.len() > 1 {
                queue.remove(0)
            } else {
                queue[0].clone()
            })
        }
    }

    impl ZitadelApi for Fake {
        async fn rest(&self, method: Method, path: &str, body: Value) -> anyhow::Result<Value> {
            self.answer(format!("{method} {path}"), body)
        }
        async fn connect(&self, service: &str, method: &str, body: Value) -> anyhow::Result<Value> {
            self.answer(
                format!("{}/{method}", service.rsplit('.').next().unwrap()),
                body,
            )
        }
    }

    const LIST: &str = "ApplicationService/ListApplications";
    const UPDATE: &str = "ApplicationService/UpdateApplication";
    const GET_FEATURES: &str = "FeatureService/GetInstanceFeatures";
    const SET_FEATURES: &str = "FeatureService/SetInstanceFeatures";

    fn studio(id: &str, slot: u16, mode: Mode) -> Value {
        let mut config = studio_app_config(slot, mode);
        config["clientId"] = json!(format!("c-{slot}"));
        json!({"applicationId": id, "name": crate::dev::requests::studio_app_name(slot),
               "oidcConfiguration": config})
    }

    fn existing() -> Fake {
        Fake::default()
            .with("POST /v2/organizations/_search", json!({"result": [{"id": "o1"}]}))
            .with("ProjectService/ListProjects", json!({"projects": [{"projectId": "p1"}]}))
            .with("ApplicationService/ListApplications", json!({"applications": [
                {"applicationId": "a1", "name": "cheladb-orchestrator", "apiConfiguration": {"clientId": "c-api"}},
                {"applicationId": "a2", "name": "cheladb-studio", "oidcConfiguration": {
                    "clientId": "c-spa", "redirectUris": studio_app_config(0, Mode::SelfHosted)["redirectUris"],
                    "postLogoutRedirectUris": studio_app_config(0, Mode::SelfHosted)["postLogoutRedirectUris"],
                    "accessTokenType": "OIDC_TOKEN_TYPE_JWT", "developmentMode": true,
                    "loginVersion": {"loginV2": {"baseUri": "http://localhost:3000/"}}}}]}))
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
            ensure_studio_app(&fake, "p1", 0, Mode::SelfHosted)
                .await
                .unwrap(),
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

    // W7: slot N's Studio gets its own app beside slot 0's.
    #[tokio::test]
    async fn a_slot_gets_its_own_studio_app() {
        let fake = existing().with(
            "ApplicationService/CreateApplication",
            json!({"applicationId": "a7", "oidcConfiguration": {"clientId": "c-s1"}}),
        );
        assert_eq!(
            ensure_studio_app(&fake, "p1", 1, Mode::SelfHosted)
                .await
                .unwrap(),
            "c-s1"
        );
        assert!(
            fake.calls()
                .contains(&"ApplicationService/CreateApplication".to_string()),
            "{:?}",
            fake.calls()
        );
        assert_eq!(
            ensure_studio_app(&fake, "p1", 0, Mode::SelfHosted)
                .await
                .unwrap(),
            "c-spa"
        );
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
            ensure_studio_app(&fake, "p1", 0, Mode::SelfHosted)
                .await
                .unwrap(),
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
        let err = ensure_studio_app(&fake, "p1", 0, Mode::SelfHosted)
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

    // W8: in self-hosted mode a Studio app without a login base URI (slot 0's from before W8)
    // gets one, apps that have theirs are left alone, then the instance stops forcing its
    // own login URL (keeping its base URI).
    #[tokio::test]
    async fn self_hosted_gives_every_studio_app_its_login_then_stops_forcing_the_instances() {
        let before = json!({"applications": [
            {"applicationId": "a1", "name": "cheladb-orchestrator", "apiConfiguration": {"clientId": "c-api"}},
            studio("a2", 0, Mode::Platform),
            studio("a3", 2, Mode::SelfHosted)]});
        let after = json!({"applications": [
            studio("a2", 0, Mode::SelfHosted),
            studio("a3", 2, Mode::SelfHosted)]});
        let fake = Fake::default()
            .with(LIST, before.clone())
            .then(LIST, before)
            .then(LIST, after)
            .with(UPDATE, json!({}))
            .with(
                GET_FEATURES,
                json!({"loginV2": {"required": true,
                "baseUri": "http://localhost:3000/", "source": "SOURCE_INSTANCE"}}),
            )
            .with(SET_FEATURES, json!({}));
        ensure_studio_logins(&fake, "p1", Mode::SelfHosted)
            .await
            .unwrap();
        let updates = fake.bodies(UPDATE);
        assert_eq!(updates.len(), 1, "{:?}", fake.calls());
        assert_eq!(updates[0]["applicationId"], "a2");
        assert_eq!(
            updates[0]["oidcConfiguration"]["loginVersion"],
            json!({"loginV2": {"baseUri": "http://localhost:3000/"}})
        );
        assert_eq!(
            fake.bodies(GET_FEATURES),
            vec![json!({"inheritance": true})]
        );
        assert_eq!(
            fake.bodies(SET_FEATURES),
            vec![json!({"loginV2": {"required": false, "baseUri": "http://localhost:3000/"}})]
        );
        // The base URIs are in place before the instance stops forcing its own.
        let calls = fake.calls();
        let set = calls.iter().position(|c| c == SET_FEATURES).unwrap();
        let update = calls.iter().position(|c| c == UPDATE).unwrap();
        assert!(update < set, "{calls:?}");
    }

    // A second run writes nothing.
    #[tokio::test]
    async fn self_hosted_logins_are_idempotent() {
        let fake = Fake::default()
            .with(
                LIST,
                json!({"applications": [
                studio("a2", 0, Mode::SelfHosted), studio("a3", 1, Mode::SelfHosted)]}),
            )
            .with(
                GET_FEATURES,
                json!({"loginV2": {"baseUri": "http://localhost:3000/",
                "source": "SOURCE_INSTANCE"}}),
            );
        ensure_studio_logins(&fake, "p1", Mode::SelfHosted)
            .await
            .unwrap();
        let calls = fake.calls();
        assert!(calls.contains(&GET_FEATURES.to_string()), "{calls:?}");
        assert!(
            !calls.iter().any(|c| c == UPDATE || c == SET_FEATURES),
            "{calls:?}"
        );
    }

    // No base URI on the instance either: the instance keeps slot 0's Studio as its default.
    #[tokio::test]
    async fn required_off_without_a_base_uri_names_slot_0s_studio() {
        let fake = Fake::default()
            .with(
                LIST,
                json!({"applications": [studio("a2", 0, Mode::SelfHosted)]}),
            )
            .with(
                GET_FEATURES,
                json!({"loginV2": {"required": true, "source": "SOURCE_SYSTEM"}}),
            )
            .with(SET_FEATURES, json!({}));
        ensure_studio_logins(&fake, "p1", Mode::SelfHosted)
            .await
            .unwrap();
        assert_eq!(
            fake.bodies(SET_FEATURES),
            vec![json!({"loginV2": {"required": false, "baseUri": "http://localhost:3000/"}})]
        );
    }

    // If Zitadel still lists a Studio app without a base URI after the updates, bootstrap stops
    // before switching the instance off (that app would fall back to the v1 login).
    #[tokio::test]
    async fn a_studio_app_left_without_a_login_stops_bootstrap() {
        let missing = json!({"applications": [studio("a2", 0, Mode::Platform)]});
        let fake = Fake::default()
            .with(LIST, missing)
            .with(UPDATE, json!({}))
            .with(GET_FEATURES, json!({"loginV2": {"required": true}}))
            .with(SET_FEATURES, json!({}));
        let err = ensure_studio_logins(&fake, "p1", Mode::SelfHosted)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("cheladb-studio") && err.contains("v1 login"),
            "{err}"
        );
        assert!(
            !fake.calls().contains(&SET_FEATURES.to_string()),
            "{:?}",
            fake.calls()
        );
    }

    // Platform mode: the instance setting and the apps' login are not touched.
    #[tokio::test]
    async fn platform_mode_leaves_the_logins_alone() {
        let fake = Fake::default();
        ensure_studio_logins(&fake, "p1", Mode::Platform)
            .await
            .unwrap();
        assert!(fake.calls().is_empty(), "{:?}", fake.calls());
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
