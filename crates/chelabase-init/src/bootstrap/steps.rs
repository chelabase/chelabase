//! Idempotent bootstrap steps: each finds what exists and creates only what's missing.

use super::outputs::{self, Outputs};
use super::requests::*;
use super::zitadel::ZitadelApi;
use anyhow::{Context, Result, bail};
use chelabase_types::mode::Mode;
use chelabase_types::mode_policy::ModePolicy;
use reqwest::Method;
use serde_json::{Value, json};
use tracing::info;

const PROJECT: &str = "chelabase";
pub const DEMO_USER: &str = "chelabase-demo";
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
        api.rest(
            Method::PUT,
            LOGIN_POLICY,
            login_policy_without_registration(&policy),
        )
        .await?;
        info!("Zitadel's own registration is off (instance policy)");
    }
    let org = api.rest(Method::GET, ORG_LOGIN_POLICY, Value::Null).await?;
    if !policy_is_default(&org) && registration_allowed(&org) {
        api.rest(
            Method::PUT,
            ORG_LOGIN_POLICY,
            login_policy_without_registration(&org),
        )
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

/// Keeps the output dir's `demo-sa.json` if it is a key for `user_id`; otherwise adds a key
/// and writes it (0600). A symlink there is refused.
pub async fn ensure_demo_key(api: &impl ZitadelApi, user_id: &str, out: &Outputs) -> Result<()> {
    if let Some(bytes) = out.read(outputs::DEMO_KEY)?
        && serde_json::from_slice::<Value>(&bytes)
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
    out.write_secret(outputs::DEMO_KEY, file.as_bytes())?;
    info!("wrote {}", out.path().join(outputs::DEMO_KEY).display());
    Ok(())
}

/// The FirstInstance human's possible user names (Zitadel names it after the instance's
/// domain): `zitadel-admin@zitadel.<auth host name>`, the pre-W1 `zitadel.localhost` form, and
/// Zitadel's plain default.
pub fn first_human_usernames(auth_host_name: &str) -> Vec<String> {
    let mut names = vec![
        format!("zitadel-admin@zitadel.{auth_host_name}"),
        "zitadel-admin@zitadel.localhost".to_string(),
        "zitadel-admin".to_string(),
    ];
    names.dedup();
    names
}

/// Deactivates every human among the instance's IAM members: Zitadel's FirstInstance human
/// (an IAM owner whose random password is only in the steps file), however it is named, and
/// any other human given an instance role. The machine members (the admin PAT's user, the login
/// client) stay. A human already inactive is left alone, so a rerun changes nothing; no human
/// member at all is the state this wants. A listing that doesn't hold every member fails:
/// it can't show that no human is left. Returns the humans deactivated by this call.
pub async fn deactivate_human_admins(api: &impl ZitadelApi) -> Result<Vec<String>> {
    let listed = api
        .rest(Method::POST, "/admin/v1/members/_search", json!({}))
        .await?;
    let (humans, listed_count, total) = iam_humans_from(&listed);
    if total > listed_count {
        bail!(
            "the IAM member listing listed {listed_count} of {total} members: can't check that \
             no human admin is left"
        );
    }
    let mut deactivated = Vec::new();
    for id in humans {
        let user = api
            .rest(Method::GET, &format!("/v2/users/{id}"), Value::Null)
            .await?;
        // A member whose listing gave no type: its user record says. Only a machine is kept.
        if user.pointer("/user/machine").is_some() {
            continue;
        }
        let state = user.pointer("/user/state").and_then(Value::as_str);
        if state == Some(USER_INACTIVE) {
            continue;
        }
        api.rest(
            Method::POST,
            &format!("/v2/users/{id}/deactivate"),
            json!({}),
        )
        .await?;
        info!("deactivated Zitadel human admin {id} (an IAM member)");
        deactivated.push(id);
    }
    Ok(deactivated)
}

/// An SMTP configuration to keep active: Zitadel's sender for its own mail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Smtp {
    /// `host:port`, as Zitadel lists it (the configuration is found by it).
    pub host: String,
    pub payload: Value,
}

impl Smtp {
    /// The workbench's Mailpit (`--demo`).
    pub fn mailpit() -> Self {
        Self {
            host: MAILPIT.to_string(),
            payload: smtp_payload(MAILPIT),
        }
    }
}

const MAILPIT: &str = "mailpit:1025";

/// The settings an SMTP listing shows, compared with what is wanted (the password is never
/// listed: see [`smtp_password_record`]).
const SMTP_FIELDS: [&str; 5] = ["senderAddress", "senderName", "user", "tls", "description"];

/// The fields of `listed` (one listed configuration) that differ from `payload`. Zitadel
/// leaves `tls: false` and empty strings out of a listing.
pub fn smtp_drift(listed: &Value, payload: &Value) -> Vec<&'static str> {
    let norm = |v: Option<&Value>, field: &str| match v {
        None | Some(Value::Null) if field == "tls" => json!(false),
        None | Some(Value::Null) => json!(""),
        Some(v) => v.clone(),
    };
    SMTP_FIELDS
        .into_iter()
        .filter(|f| norm(listed.get(*f), f) != norm(payload.get(*f), f))
        .collect()
}

/// What the output dir keeps of the SMTP password last sent to configuration `id`: never the
/// password, but `<salt>:<SHA-256 of salt, id and password>`, with a random 32-byte salt per
/// install, made when the file is first written and kept in the same 0600 file. A salt (rather
/// than an HMAC keyed by an m1 secret) because the bootstrap reads no m1 secret and this needs
/// none: the record only tells "same password as last time". It still lets whoever reads the
/// file test guesses against a weak password, one install at a time.
pub fn smtp_password_record(salt: &str, id: &str, password: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = hex::encode(Sha256::digest(format!(
        "chelabase-smtp\0{salt}\0{id}\0{password}"
    )));
    format!("{salt}:{digest}")
}

/// The record for `password` with this install's salt: the stored one's, or a new one.
fn smtp_record_for(out: &Outputs, id: &str, password: &str) -> Result<(String, Option<String>)> {
    let stored = out.read_value(outputs::SMTP_PASSWORD_DIGEST)?;
    let salt = stored
        .as_deref()
        .and_then(|r| r.split_once(':'))
        .map(|(salt, _)| salt.to_string())
        .filter(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()))
        .unwrap_or_else(|| hex::encode(crate::random::bytes::<32>()));
    Ok((smtp_password_record(&salt, id, password), stored))
}

/// Keeps `smtp` Zitadel's active SMTP configuration: created when no configuration has its
/// host; its listed settings updated when they differ; its password sent again when it differs
/// from the one last sent (a digest in the output dir); activated when it isn't. A rerun with the
/// same settings writes nothing.
pub async fn ensure_smtp(api: &impl ZitadelApi, smtp: &Smtp, out: &Outputs) -> Result<()> {
    let listed = api
        .rest(Method::POST, "/admin/v1/smtp/_search", json!({}))
        .await?;
    let password = smtp
        .payload
        .get("password")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let existing = listed
        .get("result")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find(|c| c.get("host").and_then(Value::as_str) == Some(smtp.host.as_str()))
        .cloned();
    let (id, active) = match existing {
        Some(config) => {
            let id = config
                .get("id")
                .and_then(Value::as_str)
                .context("SMTP listing: a configuration without an id")?
                .to_string();
            let drift = smtp_drift(&config, &smtp.payload);
            if !drift.is_empty() {
                let mut body = smtp.payload.clone();
                if let Some(o) = body.as_object_mut() {
                    o.remove("password");
                }
                api.rest(Method::PUT, &format!("/admin/v1/smtp/{id}"), body)
                    .await?;
                info!(?drift, "updated SMTP {}", smtp.host);
            }
            let (digest, stored) = smtp_record_for(out, &id, &password)?;
            if stored.as_deref() != Some(digest.as_str()) {
                api.rest(
                    Method::PUT,
                    &format!("/admin/v1/smtp/{id}/password"),
                    json!({"password": password}),
                )
                .await?;
                out.write_secret(
                    outputs::SMTP_PASSWORD_DIGEST,
                    format!("{digest}\n").as_bytes(),
                )?;
                info!("sent SMTP {}'s password", smtp.host);
            }
            let active = config.get("state").and_then(Value::as_str) == Some("SMTP_CONFIG_ACTIVE");
            (id, active)
        }
        None => {
            let created = api
                .rest(Method::POST, "/admin/v1/smtp", smtp.payload.clone())
                .await?;
            let id = created
                .get("id")
                .and_then(Value::as_str)
                .map(str::to_string)
                .context("create SMTP: no id in the answer")?;
            let (digest, _) = smtp_record_for(out, &id, &password)?;
            out.write_secret(
                outputs::SMTP_PASSWORD_DIGEST,
                format!("{digest}\n").as_bytes(),
            )?;
            (id, false)
        }
    };
    if !active {
        api.rest(
            Method::POST,
            &format!("/admin/v1/smtp/{id}/_activate"),
            json!({}),
        )
        .await?;
        info!("SMTP {} active", smtp.host);
    }
    Ok(())
}

const ACTIONS_TARGET: &str = "chelabase-app-users";
const ACTIONS_ENDPOINT: &str = "http://orchestrator:8083/internal/zitadel/events";

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
/// target's signing key only when it is created, so a target without its key file in the
/// output dir is deleted and created again; a kept target whose endpoint differs is updated in
/// place (`POST /v2/actions/targets/{id}`), which keeps its signing key.
pub async fn ensure_actions_target(
    api: &impl ZitadelApi,
    out: &Outputs,
    endpoint: &str,
) -> Result<()> {
    let have_key = out.read_value(outputs::ACTIONS_SIGNING_KEY)?.is_some();
    let listed = api
        .rest(
            Method::POST,
            "/v2/actions/targets/search",
            actions_target_search(ACTIONS_TARGET),
        )
        .await?;
    let existing = actions_target_ids(&listed, ACTIONS_TARGET);
    let id = match existing.first() {
        Some(id) if have_key => {
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
            out.write_secret(outputs::ACTIONS_SIGNING_KEY, format!("{key}\n").as_bytes())?;
            info!(
                "created Actions target {ACTIONS_TARGET}; wrote {}",
                out.path().join(outputs::ACTIONS_SIGNING_KEY).display()
            );
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

#[cfg(test)]
mod tests {
    use super::super::requests::studio_app_config;
    use super::*;
    use chelabase_types::mode::Mode;
    use reqwest::Method;
    use serde_json::{Value, json};
    use std::collections::HashMap;
    use std::sync::Mutex;

    const ORIGIN: &str = "http://studio.chelabase.localhost:8000";

    /// A private temporary output dir.
    fn out_dir() -> (tempfile::TempDir, Outputs) {
        use std::os::unix::fs::PermissionsExt;
        let t = tempfile::tempdir().unwrap();
        std::fs::set_permissions(t.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let out = Outputs::open(t.path()).unwrap();
        (t, out)
    }

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
        async fn ready(&self) -> Result<(), crate::bootstrap::zitadel::ApiError> {
            Ok(())
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
            assert!(
                body.get(read_only).is_none(),
                "{read_only} is not sent back"
            );
        }
    }

    #[tokio::test]
    async fn zitadel_registration_already_off_writes_nothing() {
        let fake = Fake::default()
            .with(GET_POLICY, login_policy(false))
            .with(GET_ORG_POLICY, org_policy(true, false));
        ensure_zitadel_registration_off(&fake).await.unwrap();
        assert_eq!(
            fake.calls(),
            vec![GET_POLICY.to_string(), GET_ORG_POLICY.to_string()]
        );
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
        assert_eq!(
            fake.calls(),
            vec![GET_POLICY, GET_ORG_POLICY, PUT_ORG_POLICY]
        );
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
            .with("POST /admin/v1/smtp/_search", json!({"result": [{"id": "s1", "host": "mailpit:1025", "state": "SMTP_CONFIG_ACTIVE",
                "senderAddress": "noreply@localhost", "senderName": "Chelabase (local)",
                "user": "chelabase", "description": "chelabase local mailpit"}]}))
    }

    #[tokio::test]
    async fn a_user_is_found_by_email_within_the_org() {
        let fake = existing();
        assert_eq!(
            find_user_by_email(&fake, "o1", "a@b.co").await.unwrap(),
            Some("u1".to_string())
        );
        let body = &fake.bodies("POST /v2/users")[0];
        assert_eq!(
            body["queries"][0]["organizationIdQuery"]["organizationId"],
            "o1"
        );
        assert_eq!(body["queries"][1]["emailQuery"]["emailAddress"], "a@b.co");
        let none = Fake::default().with("POST /v2/users", json!({"result": []}));
        assert_eq!(
            find_user_by_email(&none, "o1", "a@b.co").await.unwrap(),
            None
        );
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
        let (_dir, out) = out_dir();
        out.write_secret(
            outputs::SMTP_PASSWORD_DIGEST,
            format!("{}\n", smtp_password_record(&"a".repeat(64), "s1", "chelabase")).as_bytes(),
        )
        .unwrap();
        ensure_smtp(&fake, &Smtp::mailpit(), &out).await.unwrap();
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
        let (_dir, out) = out_dir();
        ensure_smtp(&fake, &Smtp::mailpit(), &out).await.unwrap();
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

    // N5: the stored SMTP password digest is salted per install: the same password and
    // configuration id give different files, and neither is the bare SHA-256.
    #[tokio::test]
    async fn the_smtp_password_digest_is_salted_per_install() {
        use sha2::{Digest, Sha256};
        let mut files = Vec::new();
        for _ in 0..2 {
            let fake = Fake::default()
                .with("POST /admin/v1/smtp/_search", json!({"result": []}))
                .with("POST /admin/v1/smtp", json!({"id": "s9"}))
                .with("POST /admin/v1/smtp/s9/_activate", json!({}));
            let (dir, out) = out_dir();
            ensure_smtp(&fake, &Smtp::mailpit(), &out).await.unwrap();
            files.push(std::fs::read_to_string(dir.path().join(outputs::SMTP_PASSWORD_DIGEST)).unwrap());
        }
        assert_ne!(files[0], files[1], "unsalted: {files:?}");
        let bare = hex::encode(Sha256::digest("chelabase-smtp\0s9\0chelabase"));
        assert!(!files.iter().any(|f| f.contains(&bare)), "{files:?}");
    }

    #[tokio::test]
    async fn the_demo_key_is_kept_when_its_file_matches_the_user() {
        let (dir, out) = out_dir();
        let path = dir.path().join("demo-sa.json");
        std::fs::write(&path, r#"{"keyId":"k1","key":"PEM","userId":"u1"}"#).unwrap();
        let fake = Fake::default();
        ensure_demo_key(&fake, "u1", &out).await.unwrap();
        assert!(fake.calls().is_empty());
    }

    #[tokio::test]
    async fn a_missing_or_foreign_demo_key_is_replaced() {
        use base64::Engine;
        let (dir, out) = out_dir();
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
        ensure_demo_key(&fake, "u1", &out).await.unwrap();
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
            .with(
                TARGETS_CREATE,
                json!({"id": "t-new", "signingKey": "sk-new"}),
            )
            .with(EXECUTIONS, json!({}));
        for id in existing {
            fake = fake.with(&format!("DELETE /v2/actions/targets/{id}"), json!({}));
        }
        fake
    }

    #[tokio::test]
    async fn actions_target_created_and_key_written() {
        let (dir, out) = out_dir();
        let file = dir.path().join("actions-signing-key");
        let fake = actions_fake(&[]);
        ensure_actions_target(&fake, &out, ENDPOINT).await.unwrap();
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
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "sk-new\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&file).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[tokio::test]
    async fn actions_target_reused_when_key_file_exists() {
        let (dir, out) = out_dir();
        let file = dir.path().join("actions-signing-key");
        std::fs::write(&file, "old\n").unwrap();
        let fake = actions_fake(&["t-old"]);
        ensure_actions_target(&fake, &out, ENDPOINT).await.unwrap();
        // Kept: no delete, no create; the execution is still PUT (idempotent) with the old id.
        assert_eq!(fake.calls(), vec![TARGETS_SEARCH, EXECUTIONS]);
        assert_eq!(fake.bodies(EXECUTIONS)[0]["targets"], json!(["t-old"]));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "old\n");
    }

    #[tokio::test]
    async fn actions_target_recreated_when_key_file_missing() {
        let (dir, out) = out_dir();
        let file = dir.path().join("actions-signing-key");
        let fake = actions_fake(&["t-old"]);
        ensure_actions_target(&fake, &out, ENDPOINT).await.unwrap();
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
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "sk-new\n");
    }

    #[tokio::test]
    async fn actions_target_moved_to_a_new_endpoint_keeps_its_key() {
        let (dir, out) = out_dir();
        let file = dir.path().join("actions-signing-key");
        std::fs::write(&file, "old\n").unwrap();
        let fake = Fake::default()
            .with(
                TARGETS_SEARCH,
                json!({"targets": [{"id": "t-old", "name": "chelabase-app-users",
                    "endpoint": "http://host.docker.internal:8083/internal/zitadel/events"}]}),
            )
            .with("POST /v2/actions/targets/t-old", json!({}))
            .with(EXECUTIONS, json!({}));
        ensure_actions_target(&fake, &out, ENDPOINT).await.unwrap();
        assert_eq!(
            fake.calls(),
            vec![TARGETS_SEARCH, "POST /v2/actions/targets/t-old", EXECUTIONS]
        );
        assert_eq!(
            fake.bodies("POST /v2/actions/targets/t-old")[0]["endpoint"],
            ENDPOINT
        );
        assert_eq!(fake.bodies(EXECUTIONS)[0]["targets"], json!(["t-old"]));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "old\n");
    }

    #[test]
    fn the_actions_endpoint_comes_from_the_env() {
        assert_eq!(actions_endpoint_from(None), ENDPOINT);
        assert_eq!(actions_endpoint_from(Some("http://x:1/e")), "http://x:1/e");
        assert_eq!(actions_endpoint_from(Some("  ")), ENDPOINT);
    }
}
