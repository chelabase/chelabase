//! Idempotent bootstrap steps: each finds what exists and creates only what's missing.

use crate::dev::requests::*;
use crate::dev::zitadel::ZitadelApi;
use anyhow::{Context, Result, bail};
use chelabase_types::mode::Mode;
use chelabase_types::mode_policy::ModePolicy;
use reqwest::Method;
use serde_json::{Value, json};
use std::path::Path;
use tracing::info;

const PROJECT: &str = "chelabase";
const DEMO_USER: &str = "chelabase-demo";
const MAILPIT: &str = "mailpit:1025";
const PROJECTS: &str = "zitadel.project.v2.ProjectService";
const APPS: &str = "zitadel.application.v2.ApplicationService";
const FEATURES: &str = "zitadel.feature.v2.FeatureService";

/// The default organisation's id and name.
pub async fn ensure_org(api: &impl ZitadelApi) -> Result<(String, Option<String>)> {
    let listed = api
        .rest(
            Method::POST,
            "/v2/organizations/_search",
            default_org_query(),
        )
        .await?;
    let id = org_id_from(&listed).with_context(|| format!("no default organization: {listed}"))?;
    Ok((id, org_name_from(&listed)))
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

/// The Studio app's client id; its redirect and login settings are corrected if they drifted.
pub async fn ensure_studio_app(
    api: &impl ZitadelApi,
    project_id: &str,
    mode: Mode,
    origin: &str,
) -> Result<String> {
    if let Some(app) = existing_app(api, project_id, STUDIO_APP).await? {
        let drift = studio_app_drift(&app.raw, mode, origin);
        if !drift.is_empty() {
            api.connect(
                APPS,
                "UpdateApplication",
                studio_app_update(project_id, &app.id, mode, origin),
            )
            .await?;
            info!(?drift, "updated the Studio app {STUDIO_APP}");
        }
        return Ok(app.client_id);
    }
    let created = api
        .connect(
            APPS,
            "CreateApplication",
            studio_app_create(project_id, mode, origin),
        )
        .await?;
    info!("created Studio app {STUDIO_APP}");
    created_client_id(&created).with_context(|| format!("CreateApplication: {created}"))
}

/// W8, self-hosted mode only, after [`ensure_studio_app`]: checks the Studio app has its own
/// login base URI, then keeps the instance's "login v2 required" off (it would replace the
/// app's base URI with the instance's). Platform mode leaves the instance alone.
pub async fn ensure_studio_login(
    api: &impl ZitadelApi,
    project_id: &str,
    mode: Mode,
    origin: &str,
) -> Result<()> {
    if !ModePolicy::new(mode).cli_manages_studio_login() {
        return Ok(());
    }
    // Checked against what Zitadel stores, before the instance stops forcing its login.
    let listed = api
        .connect(APPS, "ListApplications", apps_search(project_id))
        .await?;
    if studio_app_lacks_login(&listed) {
        bail!(
            "the Studio app {STUDIO_APP} has no login v2 base URI. It would fall back to \
             Zitadel's v1 login; fix it in the console (Login version: v2, base URI \
             {origin}/) or run `just zitadel-reset`, then re-run bootstrap"
        );
    }
    let features = api
        .connect(FEATURES, "GetInstanceFeatures", instance_features_get())
        .await?;
    let (required, base_uri) = login_v2_from(&features);
    if required {
        let base_uri = base_uri.unwrap_or_else(|| format!("{origin}/"));
        api.connect(
            FEATURES,
            "SetInstanceFeatures",
            login_v2_optional(&base_uri),
        )
        .await?;
        info!("login v2 is no longer required instance-wide: the Studio app uses its own login");
    }
    Ok(())
}

/// Zitadel's own registration page is off (R38): sign-up goes only through Chelabase's own
/// rules (the tenant's open sign-up, invites), never around them. Reads the instance login
/// policy and writes it back only when `allowRegister` is on; the same for the org's own login
/// policy when it has one (it overrides the instance's).
pub async fn ensure_zitadel_registration_off(api: &impl ZitadelApi) -> Result<()> {
    let policy = api.rest(Method::GET, LOGIN_POLICY, Value::Null).await?;
    if registration_allowed(&policy) {
        api.rest(Method::PUT, LOGIN_POLICY, login_policy_without_registration(&policy))
            .await?;
        info!("Zitadel's own registration is off (instance policy)");
    }
    let org = api.rest(Method::GET, ORG_LOGIN_POLICY, Value::Null).await?;
    if !policy_is_default(&org) && registration_allowed(&org) {
        api.rest(Method::PUT, ORG_LOGIN_POLICY, login_policy_without_registration(&org))
            .await?;
        info!("Zitadel's own registration is off (the org's policy)");
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

/// The id of the org's user with this email, if any.
pub async fn find_user_by_email(
    api: &impl ZitadelApi,
    org_id: &str,
    email: &str,
) -> Result<Option<String>> {
    let listed = api
        .rest(Method::POST, "/v2/users", user_email_search(org_id, email))
        .await?;
    Ok(user_id_from(&listed))
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

const ACTIONS_TARGET: &str = "chelabase-app-users";
const ACTIONS_ENDPOINT: &str = "http://orchestrator:8083/internal/zitadel/events";
const ACTIONS_KEY_VAR: &str = "CHELABASE_ZITADEL_ACTIONS_SIGNING_KEY";

/// The Actions endpoint: `CHELABASE_ZITADEL_ACTIONS_ENDPOINT`'s value `v`, or the orchestrator's
/// internal listener by service name when it is unset or blank.
pub fn actions_endpoint_from(v: Option<&str>) -> String {
    v.map(str::trim)
        .filter(|v| !v.is_empty())
        .unwrap_or(ACTIONS_ENDPOINT)
        .to_string()
}

/// The Actions target that posts user events to `endpoint` (the orchestrator's internal
/// listener), and the execution that routes the `user` event group to it. Zitadel shows a
/// target's signing key only when it is created, so a target without its `actions.env` is
/// deleted and created again; a kept target whose endpoint differs is updated in place
/// (`POST /v2/actions/targets/{id}`), which keeps its signing key.
pub async fn ensure_actions_target(
    api: &impl ZitadelApi,
    env_file: &Path,
    endpoint: &str,
) -> Result<()> {
    let listed = api
        .rest(
            Method::POST,
            "/v2/actions/targets/search",
            actions_target_search(ACTIONS_TARGET),
        )
        .await?;
    let existing = actions_target_ids(&listed, ACTIONS_TARGET);
    let id = match existing.first() {
        Some(id) if env_file.is_file() => {
            if actions_target_endpoint(&listed, id).as_deref() != Some(endpoint) {
                api.rest(
                    Method::POST,
                    &format!("/v2/actions/targets/{id}"),
                    actions_target_update(endpoint),
                )
                .await?;
                info!("moved Actions target {ACTIONS_TARGET} to {endpoint}");
            }
            id.clone()
        }
        _ => {
            for id in &existing {
                api.rest(
                    Method::DELETE,
                    &format!("/v2/actions/targets/{id}"),
                    Value::Null,
                )
                .await?;
            }
            let created = api
                .rest(
                    Method::POST,
                    "/v2/actions/targets",
                    actions_target_create(ACTIONS_TARGET, endpoint),
                )
                .await?;
            let (id, key) = actions_target_created(&created)?;
            write_private(env_file, &format!("{ACTIONS_KEY_VAR}={key}\n"))?;
            info!("created Actions target {ACTIONS_TARGET}; wrote {}", env_file.display());
            id
        }
    };
    api.rest(
        Method::PUT,
        "/v2/actions/executions",
        actions_execution_set(&id),
    )
    .await?;
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
    use chelabase_types::mode::Mode;
    use reqwest::Method;
    use serde_json::{Value, json};
    use std::collections::HashMap;
    use std::sync::Mutex;

    const ORIGIN: &str = "http://studio.chelabase.localhost:8000";

    /// Answers by call key (`"POST /v2/organizations/_search"` or `"Service/Method"`), records
    /// calls and their bodies.
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

    const GET_POLICY: &str = "GET /admin/v1/policies/login";
    const PUT_POLICY: &str = "PUT /admin/v1/policies/login";

    const GET_ORG_POLICY: &str = "GET /management/v1/policies/login";
    const PUT_ORG_POLICY: &str = "PUT /management/v1/policies/login";

    fn login_policy(allow_register: bool) -> Value {
        json!({"policy": {
            "details": {"sequence": "3"},
            "allowUsernamePassword": true,
            "allowRegister": allow_register,
            "allowExternalIdp": true,
            "forceMfa": false,
            "passwordlessType": "PASSWORDLESS_TYPE_ALLOWED",
            "hidePasswordReset": false,
            "isDefault": true,
            "passwordCheckLifetime": "864000s",
            "secondFactors": ["SECOND_FACTOR_TYPE_OTP"],
            "idps": [],
            "disableLoginWithEmail": true,
        }})
    }

    // R38: Zitadel's own registration page would let anyone in around Chelabase's sign-up rules.
    // The PUT replaces the whole policy, so everything else is sent back as it was read.
    #[tokio::test]
    async fn zitadel_registration_is_turned_off_keeping_the_rest_of_the_policy() {
        let fake = Fake::default()
            .with(GET_POLICY, login_policy(true))
            .with(PUT_POLICY, json!({}))
            .with(GET_ORG_POLICY, org_policy(true, true));
        ensure_zitadel_registration_off(&fake).await.unwrap();
        let put = fake.bodies(PUT_POLICY);
        assert_eq!(put.len(), 1);
        let body = &put[0];
        assert_eq!(body["allowRegister"], false);
        assert_eq!(body["allowUsernamePassword"], true);
        assert_eq!(body["allowExternalIdp"], true);
        assert_eq!(body["passwordlessType"], "PASSWORDLESS_TYPE_ALLOWED");
        assert_eq!(body["passwordCheckLifetime"], "864000s");
        assert_eq!(body["disableLoginWithEmail"], true);
        for read_only in ["details", "isDefault", "secondFactors", "idps"] {
            assert!(body.get(read_only).is_none(), "{read_only} is not sent back");
        }
    }

    #[tokio::test]
    async fn zitadel_registration_already_off_writes_nothing() {
        let fake = Fake::default()
            .with(GET_POLICY, login_policy(false))
            .with(GET_ORG_POLICY, org_policy(true, false));
        ensure_zitadel_registration_off(&fake).await.unwrap();
        assert_eq!(fake.calls(), vec![GET_POLICY.to_string(), GET_ORG_POLICY.to_string()]);
    }

    /// The org's login policy as `GET /management/v1/policies/login` answers it.
    fn org_policy(is_default: bool, allow_register: bool) -> Value {
        let mut answer = login_policy(allow_register);
        answer["policy"]["isDefault"] = json!(is_default);
        answer
    }

    // An org policy of its own overrides the instance's, so it needs `allowRegister: false` too.
    #[tokio::test]
    async fn an_org_login_policy_with_registration_on_is_turned_off_too() {
        let fake = Fake::default()
            .with(GET_POLICY, login_policy(false))
            .with(GET_ORG_POLICY, org_policy(false, true))
            .with(PUT_ORG_POLICY, json!({}));
        ensure_zitadel_registration_off(&fake).await.unwrap();
        assert_eq!(fake.calls(), vec![GET_POLICY, GET_ORG_POLICY, PUT_ORG_POLICY]);
        let body = &fake.bodies(PUT_ORG_POLICY)[0];
        assert_eq!(body["allowRegister"], false);
        assert_eq!(body["allowUsernamePassword"], true);
        assert!(body.get("isDefault").is_none() && body.get("details").is_none());
    }

    // The default policy shown for the org is the instance's, already handled: no org write.
    #[tokio::test]
    async fn an_org_without_its_own_login_policy_is_left_alone() {
        let fake = Fake::default()
            .with(GET_POLICY, login_policy(false))
            .with(GET_ORG_POLICY, org_policy(true, true));
        ensure_zitadel_registration_off(&fake).await.unwrap();
        assert!(fake.bodies(PUT_ORG_POLICY).is_empty());
    }

    const LIST: &str = "ApplicationService/ListApplications";
    const UPDATE: &str = "ApplicationService/UpdateApplication";
    const GET_FEATURES: &str = "FeatureService/GetInstanceFeatures";
    const SET_FEATURES: &str = "FeatureService/SetInstanceFeatures";

    fn studio(id: &str, mode: Mode) -> Value {
        let mut config = studio_app_config(mode, ORIGIN);
        config["clientId"] = json!("c-spa");
        json!({"applicationId": id, "name": "chelabase-studio", "oidcConfiguration": config})
    }

    fn existing() -> Fake {
        Fake::default()
            .with("POST /v2/organizations/_search", json!({"result": [{"id": "o1"}]}))
            .with("ProjectService/ListProjects", json!({"projects": [{"projectId": "p1"}]}))
            .with("ApplicationService/ListApplications", json!({"applications": [
                {"applicationId": "a1", "name": "chelabase-orchestrator", "apiConfiguration": {"clientId": "c-api"}},
                {"applicationId": "a2", "name": "chelabase-studio", "oidcConfiguration": {
                    "clientId": "c-spa", "redirectUris": studio_app_config(Mode::SelfHosted, ORIGIN)["redirectUris"],
                    "postLogoutRedirectUris": studio_app_config(Mode::SelfHosted, ORIGIN)["postLogoutRedirectUris"],
                    "accessTokenType": "OIDC_TOKEN_TYPE_JWT", "developmentMode": true,
                    "loginVersion": {"loginV2": {
                        "baseUri": "http://studio.chelabase.localhost:8000/"}}}}]}))
            .with("POST /v2/users", json!({"result": [{"userId": "u1"}]}))
            .with("POST /admin/v1/smtp/_search", json!({"result": [{"id": "s1", "host": "mailpit:1025", "state": "SMTP_CONFIG_ACTIVE"}]}))
    }

    #[tokio::test]
    async fn a_user_is_found_by_email_within_the_org() {
        let fake = existing();
        assert_eq!(
            find_user_by_email(&fake, "o1", "a@b.co").await.unwrap(),
            Some("u1".to_string())
        );
        let body = &fake.bodies("POST /v2/users")[0];
        assert_eq!(body["queries"][0]["organizationIdQuery"]["organizationId"], "o1");
        assert_eq!(body["queries"][1]["emailQuery"]["emailAddress"], "a@b.co");
        let none = Fake::default().with("POST /v2/users", json!({"result": []}));
        assert_eq!(find_user_by_email(&none, "o1", "a@b.co").await.unwrap(), None);
    }

    #[tokio::test]
    async fn a_second_run_creates_nothing() {
        let fake = existing();
        assert_eq!(ensure_org(&fake).await.unwrap().0, "o1");
        assert_eq!(ensure_project(&fake, "o1").await.unwrap(), "p1");
        assert_eq!(ensure_api_app(&fake, "p1").await.unwrap(), "c-api");
        assert_eq!(
            ensure_studio_app(&fake, "p1", Mode::SelfHosted, ORIGIN)
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
                {"applicationId": "a2", "name": "chelabase-studio", "oidcConfiguration": {"clientId": "c-spa", "redirectUris": ["http://old/callback"]}}]}))
            .with("ApplicationService/UpdateApplication", json!({}));
        assert_eq!(
            ensure_studio_app(&fake, "p1", Mode::SelfHosted, ORIGIN)
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
            json!({"applications": [{"applicationId": "a2", "name": "chelabase-studio"}]}),
        );
        let err = ensure_studio_app(&fake, "p1", Mode::SelfHosted, ORIGIN)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("chelabase-studio") && err.contains("a2"),
            "{err}"
        );
        assert!(
            !fake.calls().iter().any(|c| c.contains("Create")),
            "{:?}",
            fake.calls()
        );
    }

    // W8: in self-hosted mode, once the Studio app has its own login base URI, the instance
    // stops forcing its login URL (keeping its base URI).
    #[tokio::test]
    async fn self_hosted_stops_forcing_the_instances_login() {
        let fake = Fake::default()
            .with(
                LIST,
                json!({"applications": [studio("a2", Mode::SelfHosted)]}),
            )
            .with(
                GET_FEATURES,
                json!({"loginV2": {"required": true,
                "baseUri": "http://studio.chelabase.localhost:8000/", "source": "SOURCE_INSTANCE"}}),
            )
            .with(SET_FEATURES, json!({}));
        ensure_studio_login(&fake, "p1", Mode::SelfHosted, ORIGIN)
            .await
            .unwrap();
        assert_eq!(
            fake.bodies(GET_FEATURES),
            vec![json!({"inheritance": true})]
        );
        assert_eq!(
            fake.bodies(SET_FEATURES),
            vec![json!({"loginV2": {
                "required": false,
                "baseUri": "http://studio.chelabase.localhost:8000/"
            }})]
        );
    }

    // A second run writes nothing.
    #[tokio::test]
    async fn the_self_hosted_login_is_idempotent() {
        let fake = Fake::default()
            .with(
                LIST,
                json!({"applications": [studio("a2", Mode::SelfHosted)]}),
            )
            .with(
                GET_FEATURES,
                json!({"loginV2": {"baseUri": "http://studio.chelabase.localhost:8000/",
                "source": "SOURCE_INSTANCE"}}),
            );
        ensure_studio_login(&fake, "p1", Mode::SelfHosted, ORIGIN)
            .await
            .unwrap();
        let calls = fake.calls();
        assert!(calls.contains(&GET_FEATURES.to_string()), "{calls:?}");
        assert!(
            !calls.iter().any(|c| c == UPDATE || c == SET_FEATURES),
            "{calls:?}"
        );
    }

    // No base URI on the instance either: the instance keeps Studio as its default.
    #[tokio::test]
    async fn required_off_without_a_base_uri_names_studio() {
        let fake = Fake::default()
            .with(
                LIST,
                json!({"applications": [studio("a2", Mode::SelfHosted)]}),
            )
            .with(
                GET_FEATURES,
                json!({"loginV2": {"required": true, "source": "SOURCE_SYSTEM"}}),
            )
            .with(SET_FEATURES, json!({}));
        ensure_studio_login(&fake, "p1", Mode::SelfHosted, ORIGIN)
            .await
            .unwrap();
        assert_eq!(
            fake.bodies(SET_FEATURES),
            vec![json!({"loginV2": {
                "required": false,
                "baseUri": "http://studio.chelabase.localhost:8000/"
            }})]
        );
    }

    // If Zitadel still lists the Studio app without a base URI, bootstrap stops before switching
    // the instance off (the app would fall back to the v1 login).
    #[tokio::test]
    async fn a_studio_app_left_without_a_login_stops_bootstrap() {
        let missing = json!({"applications": [studio("a2", Mode::Platform)]});
        let fake = Fake::default()
            .with(LIST, missing)
            .with(GET_FEATURES, json!({"loginV2": {"required": true}}))
            .with(SET_FEATURES, json!({}));
        let err = ensure_studio_login(&fake, "p1", Mode::SelfHosted, ORIGIN)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("chelabase-studio") && err.contains("v1 login"),
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
        ensure_studio_login(&fake, "p1", Mode::Platform, ORIGIN)
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

    const ENDPOINT: &str = "http://orchestrator:8083/internal/zitadel/events";
    const TARGETS_SEARCH: &str = "POST /v2/actions/targets/search";
    const TARGETS_CREATE: &str = "POST /v2/actions/targets";
    const EXECUTIONS: &str = "PUT /v2/actions/executions";

    fn target_listing(ids: &[&str]) -> Value {
        json!({"targets": ids.iter().map(|id| json!({"id": id, "name": "chelabase-app-users", "endpoint": ENDPOINT})).collect::<Vec<_>>()})
    }

    fn actions_fake(existing: &[&str]) -> Fake {
        let mut fake = Fake::default()
            .with(TARGETS_SEARCH, target_listing(existing))
            .with(TARGETS_CREATE, json!({"id": "t-new", "signingKey": "sk-new"}))
            .with(EXECUTIONS, json!({}));
        for id in existing {
            fake = fake.with(&format!("DELETE /v2/actions/targets/{id}"), json!({}));
        }
        fake
    }

    #[tokio::test]
    async fn actions_target_created_and_key_written() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("actions.env");
        let fake = actions_fake(&[]);
        ensure_actions_target(&fake, &file, ENDPOINT).await.unwrap();
        assert_eq!(
            fake.calls(),
            vec![TARGETS_SEARCH, TARGETS_CREATE, EXECUTIONS]
        );
        let body = &fake.bodies(TARGETS_CREATE)[0];
        assert_eq!(body["name"], "chelabase-app-users");
        assert_eq!(body["restAsync"], json!({}));
        assert_eq!(body["endpoint"], ENDPOINT);
        assert_eq!(body["payloadType"], "PAYLOAD_TYPE_JSON");
        assert_eq!(
            fake.bodies(EXECUTIONS)[0],
            json!({"condition": {"event": {"group": "user"}}, "targets": ["t-new"]})
        );
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "CHELABASE_ZITADEL_ACTIONS_SIGNING_KEY=sk-new\n"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&file).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[tokio::test]
    async fn actions_target_reused_when_key_file_exists() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("actions.env");
        std::fs::write(&file, "CHELABASE_ZITADEL_ACTIONS_SIGNING_KEY=old\n").unwrap();
        let fake = actions_fake(&["t-old"]);
        ensure_actions_target(&fake, &file, ENDPOINT).await.unwrap();
        // Kept: no delete, no create; the execution is still PUT (idempotent) with the old id.
        assert_eq!(fake.calls(), vec![TARGETS_SEARCH, EXECUTIONS]);
        assert_eq!(fake.bodies(EXECUTIONS)[0]["targets"], json!(["t-old"]));
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "CHELABASE_ZITADEL_ACTIONS_SIGNING_KEY=old\n"
        );
    }

    #[tokio::test]
    async fn actions_target_recreated_when_key_file_missing() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("actions.env");
        let fake = actions_fake(&["t-old"]);
        ensure_actions_target(&fake, &file, ENDPOINT).await.unwrap();
        assert_eq!(
            fake.calls(),
            vec![
                TARGETS_SEARCH,
                "DELETE /v2/actions/targets/t-old",
                TARGETS_CREATE,
                EXECUTIONS
            ]
        );
        assert_eq!(fake.bodies(EXECUTIONS)[0]["targets"], json!(["t-new"]));
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "CHELABASE_ZITADEL_ACTIONS_SIGNING_KEY=sk-new\n"
        );
    }

    #[tokio::test]
    async fn actions_target_moved_to_a_new_endpoint_keeps_its_key() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("actions.env");
        std::fs::write(&file, "CHELABASE_ZITADEL_ACTIONS_SIGNING_KEY=old\n").unwrap();
        let fake = Fake::default()
            .with(
                TARGETS_SEARCH,
                json!({"targets": [{"id": "t-old", "name": "chelabase-app-users",
                    "endpoint": "http://host.docker.internal:8083/internal/zitadel/events"}]}),
            )
            .with("POST /v2/actions/targets/t-old", json!({}))
            .with(EXECUTIONS, json!({}));
        ensure_actions_target(&fake, &file, ENDPOINT).await.unwrap();
        assert_eq!(
            fake.calls(),
            vec![TARGETS_SEARCH, "POST /v2/actions/targets/t-old", EXECUTIONS]
        );
        assert_eq!(fake.bodies("POST /v2/actions/targets/t-old")[0]["endpoint"], ENDPOINT);
        assert_eq!(fake.bodies(EXECUTIONS)[0]["targets"], json!(["t-old"]));
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "CHELABASE_ZITADEL_ACTIONS_SIGNING_KEY=old\n"
        );
    }

    #[test]
    fn the_actions_endpoint_comes_from_the_env() {
        assert_eq!(actions_endpoint_from(None), ENDPOINT);
        assert_eq!(actions_endpoint_from(Some("http://x:1/e")), "http://x:1/e");
        assert_eq!(actions_endpoint_from(Some("  ")), ENDPOINT);
    }
}
