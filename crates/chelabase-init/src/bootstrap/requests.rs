//! Pure request bodies and response parsers for the bootstrap's Zitadel v4 calls.
//! REST v2 for orgs and users; ConnectRPC JSON for projects and applications.

use anyhow::{Context, Result};
use base64::Engine;
use chelabase_types::mode::Mode;
use chelabase_types::mode_policy::ModePolicy;
use serde_json::{Value, json};

pub const KEY_EXPIRY: &str = "2099-01-01T00:00:00Z";

pub fn default_org_query() -> Value {
    json!({"queries": [{"defaultQuery": {}}]})
}

pub fn org_id_from(listed: &Value) -> Option<String> {
    listed.pointer("/result/0/id")?.as_str().map(str::to_string)
}

/// The name of the first organisation in a search result (`result[0].name`).
pub fn org_name_from(listed: &Value) -> Option<String> {
    listed
        .pointer("/result/0/name")?
        .as_str()
        .map(str::to_string)
        .filter(|n| !n.trim().is_empty())
}

/// The instance login policy path (Zitadel v4.19.1 admin API `GetLoginPolicy` / `UpdateLoginPolicy`).
pub const LOGIN_POLICY: &str = "/admin/v1/policies/login";

/// The org's own login policy path (`GetLoginPolicy` / `UpdateCustomLoginPolicy`): it overrides
/// the instance's when the org has set one (`policy.isDefault` is then false).
pub const ORG_LOGIN_POLICY: &str = "/management/v1/policies/login";

/// Whether the org has no login policy of its own (the instance's applies).
pub fn policy_is_default(answer: &Value) -> bool {
    answer
        .pointer("/policy/isDefault")
        .or_else(|| answer.get("isDefault"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// Whether Zitadel's own sign-up (`allowRegister`) is on in a `GET` login-policy answer.
pub fn registration_allowed(answer: &Value) -> bool {
    answer
        .pointer("/policy/allowRegister")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// The `PUT` body that turns `allowRegister` off and keeps the rest of the policy as read.
/// `UpdateLoginPolicy` replaces the whole policy (a field left out becomes `false`), so every
/// updatable field of the answer is sent back; the read-only ones (`details`, `isDefault`, the
/// factor and IdP lists, which have their own calls) are not.
pub fn login_policy_without_registration(answer: &Value) -> Value {
    const FIELDS: [&str; 16] = [
        "allowUsernamePassword",
        "allowExternalIdp",
        "forceMfa",
        "passwordlessType",
        "hidePasswordReset",
        "ignoreUnknownUsernames",
        "defaultRedirectUri",
        "passwordCheckLifetime",
        "externalLoginCheckLifetime",
        "mfaInitSkipLifetime",
        "secondFactorCheckLifetime",
        "multiFactorCheckLifetime",
        "allowDomainDiscovery",
        "disableLoginWithEmail",
        "disableLoginWithPhone",
        "forceMfaLocalOnly",
    ];
    let mut body = serde_json::Map::new();
    for field in FIELDS {
        if let Some(v) = answer.pointer(&format!("/policy/{field}")) {
            body.insert(field.to_string(), v.clone());
        }
    }
    body.insert("allowRegister".into(), json!(false));
    Value::Object(body)
}

pub fn project_search(name: &str) -> Value {
    json!({"filters": [{"projectNameFilter": {"projectName": name, "method": "TEXT_FILTER_METHOD_EQUALS"}}]})
}

pub fn project_create(org_id: &str, name: &str) -> Value {
    json!({"organizationId": org_id, "name": name})
}

/// From a `ListProjects` result or a `CreateProject` response.
pub fn project_id_from(value: &Value) -> Option<String> {
    value
        .pointer("/projects/0/projectId")
        .or_else(|| value.get("projectId"))?
        .as_str()
        .map(str::to_string)
}

pub fn apps_search(project_id: &str) -> Value {
    json!({"filters": [{"projectIdFilter": {"projectId": project_id}}]})
}

pub fn api_app_create(project_id: &str, name: &str) -> Value {
    json!({"projectId": project_id, "name": name,
           "apiConfiguration": {"authMethodType": "API_AUTH_METHOD_TYPE_PRIVATE_KEY_JWT"}})
}

/// Studio: a public SPA using PKCE, issuing JWT access tokens the gateway verifies via JWKS.
/// `developmentMode` allows the plain-http localhost redirects. In self-hosted mode the app sends
/// sign-ins to Studio's /login itself (login v2 `baseUri`): the instance doesn't force its own
/// login URL there (W8), and an app without a base URI would get Zitadel's v1 login. Platform
/// mode leaves the login to the instance default.
pub fn studio_app_config(mode: Mode, origin: &str) -> Value {
    let mut config = json!({
        "redirectUris": [format!("{origin}/callback")],
        "postLogoutRedirectUris": [format!("{origin}/")],
        "responseTypes": ["OIDC_RESPONSE_TYPE_CODE"],
        "grantTypes": ["OIDC_GRANT_TYPE_AUTHORIZATION_CODE", "OIDC_GRANT_TYPE_REFRESH_TOKEN"],
        "applicationType": "OIDC_APP_TYPE_USER_AGENT",
        "authMethodType": "OIDC_AUTH_METHOD_TYPE_NONE",
        "version": "OIDC_VERSION_1_0",
        "developmentMode": true,
        "accessTokenType": "OIDC_TOKEN_TYPE_JWT",
        "idTokenUserinfoAssertion": true
    });
    if ModePolicy::new(mode).cli_manages_studio_login() {
        config["loginVersion"] = json!({"loginV2": {"baseUri": format!("{origin}/")}});
    }
    config
}

pub fn studio_app_create(project_id: &str, mode: Mode, origin: &str) -> Value {
    json!({"projectId": project_id, "name": STUDIO_APP,
           "oidcConfiguration": studio_app_config(mode, origin)})
}

/// No `name`: the app is found by it, and Zitadel refuses an unchanged name (COMMAND-2m8vx).
pub fn studio_app_update(project_id: &str, app_id: &str, mode: Mode, origin: &str) -> Value {
    json!({"projectId": project_id, "applicationId": app_id,
           "oidcConfiguration": studio_app_config(mode, origin)})
}

/// Whether a `ListApplications` result has the Studio app without a login v2 base URI.
pub fn studio_app_lacks_login(listed: &Value) -> bool {
    listed
        .get("applications")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|a| a.get("name").and_then(Value::as_str) == Some(STUDIO_APP))
        .any(|app| {
            app.pointer("/oidcConfiguration/loginVersion/loginV2/baseUri")
                .and_then(Value::as_str)
                .is_none_or(str::is_empty)
        })
}

/// `GetInstanceFeatures` body: the effective values, inherited from the system level too.
pub fn instance_features_get() -> Value {
    json!({"inheritance": true})
}

/// `(required, base_uri)` of the instance's login v2 feature. ConnectRPC JSON leaves out
/// `required: false`, so a missing flag is false.
pub fn login_v2_from(features: &Value) -> (bool, Option<String>) {
    let required = features
        .pointer("/loginV2/required")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let base_uri = features
        .pointer("/loginV2/baseUri")
        .and_then(Value::as_str)
        .filter(|uri| !uri.is_empty())
        .map(str::to_string);
    (required, base_uri)
}

/// `SetInstanceFeatures` body that stops forcing every app onto the instance's login URL,
/// keeping its base URI.
pub fn login_v2_optional(base_uri: &str) -> Value {
    json!({"loginV2": {"required": false, "baseUri": base_uri}})
}

pub const STUDIO_APP: &str = "chelabase-studio";
pub const API_APP: &str = "chelabase-orchestrator";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct App {
    pub id: String,
    pub client_id: String,
    pub raw: Value,
}

pub fn find_app(listed: &Value, name: &str) -> Option<App> {
    let app = listed
        .get("applications")?
        .as_array()?
        .iter()
        .find(|a| a.get("name").and_then(Value::as_str) == Some(name))?;
    let client_id = app
        .pointer("/oidcConfiguration/clientId")
        .or_else(|| app.pointer("/apiConfiguration/clientId"))?
        .as_str()?;
    Some(App {
        id: app.get("applicationId")?.as_str()?.to_string(),
        client_id: client_id.to_string(),
        raw: app.clone(),
    })
}

/// The id of an app called `name`, whether or not its client id is readable.
pub fn app_id_named(listed: &Value, name: &str) -> Option<String> {
    listed
        .get("applications")?
        .as_array()?
        .iter()
        .find(|a| a.get("name").and_then(Value::as_str) == Some(name))?
        .get("applicationId")?
        .as_str()
        .map(str::to_string)
}

/// Fields of the existing Studio app that differ from what bootstrap wants.
pub fn studio_app_drift(app: &Value, mode: Mode, origin: &str) -> Vec<&'static str> {
    let want = studio_app_config(mode, origin);
    [
        "redirectUris",
        "postLogoutRedirectUris",
        "accessTokenType",
        "developmentMode",
        "loginVersion",
    ]
    .into_iter()
    // Platform mode: the app leaves the login to the instance default.
    .filter(|field| *field != "loginVersion" || want.get("loginVersion").is_some())
    .filter(|field| app.pointer(&format!("/oidcConfiguration/{field}")) != want.get(*field))
    .collect()
}

/// From a `CreateApplication` response.
pub fn created_client_id(value: &Value) -> Option<String> {
    value
        .pointer("/oidcConfiguration/clientId")
        .or_else(|| value.pointer("/apiConfiguration/clientId"))?
        .as_str()
        .map(str::to_string)
}

pub fn machine_user_search(username: &str) -> Value {
    json!({"queries": [{"userNameQuery": {"userName": username, "method": "TEXT_QUERY_METHOD_EQUALS"}}]})
}

/// The members of an IAM member listing (`POST /admin/v1/members/_search`) that may be human:
/// all but an explicit `TYPE_MACHINE` (a missing or unknown type is checked, not skipped: the
/// caller reads the user to tell), how many members it listed, and how many it says exist.
pub fn iam_humans_from(listed: &Value) -> (Vec<String>, u64, u64) {
    let members: Vec<&Value> = listed
        .get("result")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .collect();
    let humans = members
        .iter()
        .filter(|m| m.get("userType").and_then(Value::as_str) != Some("TYPE_MACHINE"))
        .filter_map(|m| m.get("userId").and_then(Value::as_str).map(str::to_string))
        .collect();
    let count = members.len() as u64;
    let total = listed
        .pointer("/details/totalResult")
        .and_then(|v| {
            v.as_str()
                .and_then(|s| s.parse().ok())
                .or_else(|| v.as_u64())
        })
        .unwrap_or(count);
    (humans, count, total)
}

/// A deactivated user's state in a `ListUsers` result.
pub const USER_INACTIVE: &str = "USER_STATE_INACTIVE";

/// Users of one org with this email, compared case-insensitively.
pub fn user_email_search(org_id: &str, email: &str) -> Value {
    json!({"queries": [
        {"organizationIdQuery": {"organizationId": org_id}},
        {"emailQuery": {"emailAddress": email, "method": "TEXT_QUERY_METHOD_EQUALS_IGNORE_CASE"}}]})
}

pub fn machine_user_create(org_id: &str, username: &str) -> Value {
    json!({"organizationId": org_id, "username": username,
           "machine": {"name": "Chelabase Demo", "accessTokenType": "ACCESS_TOKEN_TYPE_JWT"}})
}

/// From a `ListUsers` result or a `CreateUser` response.
pub fn user_id_from(value: &Value) -> Option<String> {
    value
        .pointer("/result/0/userId")
        .or_else(|| value.get("id"))?
        .as_str()
        .map(str::to_string)
}

pub fn key_create() -> Value {
    json!({"expirationDate": KEY_EXPIRY})
}

/// The key file (`demo-sa.json`): `keyContent` is base64 of Zitadel's JSON key file.
pub fn key_file_from(created: &Value) -> Result<String> {
    let content = created
        .get("keyContent")
        .and_then(Value::as_str)
        .with_context(|| format!("AddKey response without keyContent: {created}"))?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(content)
        .context("keyContent is not base64")?;
    String::from_utf8(bytes).context("keyContent is not UTF-8")
}

/// Zitadel needs SMTP credentials even though Mailpit accepts any.
pub fn smtp_payload(host: &str) -> Value {
    json!({"senderAddress": "noreply@localhost", "senderName": "Chelabase (local)", "tls": false,
           "host": host, "user": "chelabase", "password": "chelabase", "description": "chelabase local mailpit"})
}

/// `(id, active)` of the SMTP configuration for `host`, from `/admin/v1/smtp/_search`.
pub fn find_smtp(listed: &Value, host: &str) -> Option<(String, bool)> {
    let config = listed
        .get("result")?
        .as_array()?
        .iter()
        .find(|c| c.get("host").and_then(Value::as_str) == Some(host))?;
    let active = config.get("state").and_then(Value::as_str) == Some("SMTP_CONFIG_ACTIVE");
    Some((config.get("id")?.as_str()?.to_string(), active))
}
pub fn actions_target_search(name: &str) -> Value {
    json!({"filters": [{"targetNameFilter": {"targetName": name, "method": "TEXT_FILTER_METHOD_EQUALS"}}]})
}

/// The ids of the listed targets called `name`.
pub fn actions_target_ids(listed: &Value, name: &str) -> Vec<String> {
    listed
        .get("targets")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|t| t.get("name").and_then(Value::as_str) == Some(name))
        .filter_map(|t| t.get("id")?.as_str().map(str::to_string))
        .collect()
}

/// An asynchronous REST target: Zitadel doesn't wait for (or use) the answer.
pub fn actions_target_create(name: &str, endpoint: &str) -> Value {
    json!({"name": name, "restAsync": {}, "endpoint": endpoint,
           "timeout": "10s", "payloadType": "PAYLOAD_TYPE_JSON"})
}

/// The endpoint of listed target `id`, when the listing names one.
pub fn actions_target_endpoint(listed: &Value, id: &str) -> Option<String> {
    listed
        .get("targets")
        .and_then(Value::as_array)?
        .iter()
        .find(|t| t.get("id").and_then(Value::as_str) == Some(id))?
        .get("endpoint")?
        .as_str()
        .map(str::to_string)
}

/// UpdateTarget's body: only the endpoint changes (the signing key is kept).
pub fn actions_target_update(endpoint: &str) -> Value {
    json!({"endpoint": endpoint})
}

/// `(id, signingKey)` of a created target.
pub fn actions_target_created(created: &Value) -> Result<(String, String)> {
    let field = |k: &str| {
        created
            .get(k)
            .and_then(Value::as_str)
            .map(str::to_string)
            // Never the answer itself: it carries the signing key.
            .with_context(|| format!("CreateTarget: no {k} in the answer"))
    };
    Ok((field("id")?, field("signingKey")?))
}

/// Routes every user event to the target.
pub fn actions_execution_set(target_id: &str) -> Value {
    json!({"condition": {"event": {"group": "user"}}, "targets": [target_id]})
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const ORIGIN: &str = "http://studio.chelabase.localhost:8000";

    #[test]
    fn the_default_org_comes_from_a_default_query() {
        assert_eq!(
            default_org_query(),
            json!({"queries": [{"defaultQuery": {}}]})
        );
        assert_eq!(
            org_id_from(&json!({"result": [{"id": "o1"}]})).as_deref(),
            Some("o1")
        );
        assert_eq!(org_id_from(&json!({"result": []})), None);
    }

    #[test]
    fn the_org_name_comes_from_the_same_result() {
        let listed = json!({"result": [{"id": "o1", "name": "ZITADEL"}]});
        assert_eq!(org_name_from(&listed).as_deref(), Some("ZITADEL"));
        assert_eq!(org_name_from(&json!({"result": [{"id": "o1"}]})), None);
        assert_eq!(
            org_name_from(&json!({"result": [{"id": "o1", "name": "  "}]})),
            None
        );
        assert_eq!(org_name_from(&json!({"result": []})), None);
    }

    #[test]
    fn projects_are_found_by_exact_name() {
        assert_eq!(
            project_search("chelabase"),
            json!({"filters": [{"projectNameFilter": {"projectName": "chelabase", "method": "TEXT_FILTER_METHOD_EQUALS"}}]})
        );
        assert_eq!(
            project_create("o1", "chelabase"),
            json!({"organizationId": "o1", "name": "chelabase"})
        );
        assert_eq!(
            project_id_from(&json!({"projects": [{"projectId": "p1"}]})).as_deref(),
            Some("p1")
        );
        assert_eq!(
            project_id_from(&json!({"projectId": "p2"})).as_deref(),
            Some("p2")
        );
    }

    #[test]
    fn apps_are_found_by_name_with_their_client_id() {
        let listed = json!({"applications": [
            {"applicationId": "a1", "name": "chelabase-orchestrator", "apiConfiguration": {"clientId": "c-api"}},
            {"applicationId": "a2", "name": "chelabase-studio", "oidcConfiguration": {"clientId": "c-spa", "redirectUris": ["x"]}}
        ]});
        let api = find_app(&listed, "chelabase-orchestrator").unwrap();
        assert_eq!((api.id.as_str(), api.client_id.as_str()), ("a1", "c-api"));
        let spa = find_app(&listed, "chelabase-studio").unwrap();
        assert_eq!(spa.client_id, "c-spa");
        assert!(find_app(&listed, "other").is_none());
    }

    // The app's shape; platform mode keeps the login on the instance default.
    #[test]
    fn the_studio_app_is_a_pkce_spa_with_jwt_access_tokens() {
        let c = studio_app_config(Mode::Platform, ORIGIN);
        assert_eq!(
            c["redirectUris"],
            json!(["http://studio.chelabase.localhost:8000/callback"])
        );
        assert_eq!(
            c["postLogoutRedirectUris"],
            json!(["http://studio.chelabase.localhost:8000/"])
        );
        assert_eq!(c["applicationType"], "OIDC_APP_TYPE_USER_AGENT");
        assert_eq!(c["authMethodType"], "OIDC_AUTH_METHOD_TYPE_NONE");
        assert_eq!(c["accessTokenType"], "OIDC_TOKEN_TYPE_JWT");
        assert_eq!(c["developmentMode"], true);
        assert!(c.get("loginVersion").is_none(), "{c}");
        let create = studio_app_create("p1", Mode::Platform, ORIGIN);
        assert_eq!(create["oidcConfiguration"], c);
        assert_eq!(create["name"], "chelabase-studio");
        for mode in [Mode::SelfHosted, Mode::Platform] {
            let update = studio_app_update("p1", "a2", mode, ORIGIN);
            assert_eq!(update["applicationId"], "a2");
            assert_eq!(update["oidcConfiguration"], studio_app_config(mode, ORIGIN));
            // Zitadel refuses an UpdateApplication whose name hasn't changed
            // (COMMAND-2m8vx "No changes") before it looks at the OIDC settings.
            assert!(update.get("name").is_none(), "{update}");
        }
    }

    // W8: self-hosted mode doesn't force the instance's login URL, so the Studio app names its
    // own login (otherwise it falls back to Zitadel's v1 login).
    #[test]
    fn the_self_hosted_studio_app_names_its_own_login() {
        let c = studio_app_config(Mode::SelfHosted, ORIGIN);
        assert_eq!(
            c["loginVersion"],
            json!({"loginV2": {"baseUri": "http://studio.chelabase.localhost:8000/"}})
        );
        assert_eq!(
            studio_app_create("p1", Mode::SelfHosted, ORIGIN)["oidcConfiguration"],
            c
        );
    }

    #[test]
    fn studio_app_drift_names_what_changed() {
        let good = json!({"oidcConfiguration": studio_app_config(Mode::Platform, ORIGIN)});
        assert!(studio_app_drift(&good, Mode::Platform, ORIGIN).is_empty());
        let mut bad = good.clone();
        bad["oidcConfiguration"]["redirectUris"] = json!(["http://old/callback"]);
        bad["oidcConfiguration"]["accessTokenType"] = json!("OIDC_TOKEN_TYPE_BEARER");
        assert_eq!(
            studio_app_drift(&bad, Mode::Platform, ORIGIN),
            vec!["redirectUris", "accessTokenType"]
        );
    }

    // W8: an app from before W8 (no loginVersion) is updated in self-hosted mode only.
    #[test]
    fn login_drift_counts_only_in_self_hosted_mode() {
        let before = json!({"oidcConfiguration": studio_app_config(Mode::Platform, ORIGIN)});
        assert_eq!(
            studio_app_drift(&before, Mode::SelfHosted, ORIGIN),
            vec!["loginVersion"]
        );
        assert!(studio_app_drift(&before, Mode::Platform, ORIGIN).is_empty());
        let after = json!({"oidcConfiguration": studio_app_config(Mode::SelfHosted, ORIGIN)});
        assert!(studio_app_drift(&after, Mode::SelfHosted, ORIGIN).is_empty());
    }

    // The app would fall back to Zitadel's v1 login once the instance no longer forces login v2.
    #[test]
    fn a_studio_app_without_a_login_base_is_found() {
        let login = |uri: &str| json!({"loginV2": {"baseUri": uri}});
        let listed = |config: Value| {
            json!({"applications": [
                {"applicationId": "a1", "name": "chelabase-orchestrator", "apiConfiguration": {"clientId": "c-api"}},
                {"applicationId": "a2", "name": "chelabase-studio", "oidcConfiguration": config}
            ]})
        };
        assert!(studio_app_lacks_login(&listed(json!({"clientId": "c0"}))));
        assert!(studio_app_lacks_login(&listed(
            json!({"clientId": "c0", "loginVersion": login("")})
        )));
        assert!(studio_app_lacks_login(&listed(
            json!({"clientId": "c0", "loginVersion": {"loginV1": {}}})
        )));
        assert!(!studio_app_lacks_login(&listed(json!({
            "clientId": "c0",
            "loginVersion": login("http://studio.chelabase.localhost:8000/")
        }))));
        // No Studio app at all: nothing lacks a login (ensure_studio_app creates it first).
        assert!(!studio_app_lacks_login(&json!({})));
    }

    // zitadel.feature.v2.FeatureService: the effective value (inheritance), and protojson's
    // omitted `required: false`.
    #[test]
    fn the_login_v2_feature_is_read_with_inheritance() {
        assert_eq!(instance_features_get(), json!({"inheritance": true}));
        assert_eq!(
            login_v2_from(&json!({"loginV2": {
                "required": true,
                "baseUri": "http://studio.chelabase.localhost:8000/",
                "source": "SOURCE_INSTANCE"
            }})),
            (
                true,
                Some("http://studio.chelabase.localhost:8000/".to_string())
            )
        );
        assert_eq!(
            login_v2_from(&json!({"loginV2": {
                "baseUri": "http://studio.chelabase.localhost:8000/"
            }})),
            (
                false,
                Some("http://studio.chelabase.localhost:8000/".to_string())
            )
        );
        assert_eq!(
            login_v2_from(&json!({"loginV2": {"source": "SOURCE_SYSTEM"}})),
            (false, None)
        );
        assert_eq!(login_v2_from(&json!({})), (false, None));
    }

    #[test]
    fn turning_login_v2_required_off_keeps_its_base_uri() {
        assert_eq!(
            login_v2_optional("http://studio.chelabase.localhost:8000/"),
            json!({"loginV2": {
                "required": false,
                "baseUri": "http://studio.chelabase.localhost:8000/"
            }})
        );
    }

    #[test]
    fn the_demo_machine_user_and_its_key() {
        assert_eq!(
            machine_user_search("chelabase-demo"),
            json!({"queries": [{"userNameQuery": {"userName": "chelabase-demo", "method": "TEXT_QUERY_METHOD_EQUALS"}}]})
        );
        let create = machine_user_create("o1", "chelabase-demo");
        assert_eq!(create["username"], "chelabase-demo");
        assert_eq!(
            create["machine"]["accessTokenType"],
            "ACCESS_TOKEN_TYPE_JWT"
        );
        assert_eq!(
            user_id_from(&json!({"result": [{"userId": "u1"}]})).as_deref(),
            Some("u1")
        );
        assert_eq!(user_id_from(&json!({"id": "u2"})).as_deref(), Some("u2"));
        assert_eq!(
            key_create(),
            json!({"expirationDate": "2099-01-01T00:00:00Z"})
        );
        use base64::Engine;
        let file = r#"{"type":"serviceaccount","keyId":"k1","key":"PEM","userId":"u1"}"#;
        let content = base64::engine::general_purpose::STANDARD.encode(file);
        assert_eq!(
            key_file_from(&json!({"keyId": "k1", "keyContent": content})).unwrap(),
            file
        );
    }

    #[test]
    fn mailpit_smtp_is_found_by_host() {
        let listed = json!({"result": [
            {"id": "s1", "host": "other:25", "state": "SMTP_CONFIG_ACTIVE"},
            {"id": "s2", "host": "mailpit:1025", "state": "SMTP_CONFIG_INACTIVE"}
        ]});
        assert_eq!(
            find_smtp(&listed, "mailpit:1025"),
            Some(("s2".to_string(), false))
        );
        assert_eq!(smtp_payload("mailpit:1025")["host"], "mailpit:1025");
        assert_eq!(smtp_payload("mailpit:1025")["password"], "chelabase");
    }

    #[test]
    fn the_studio_app_follows_the_studio_origin() {
        let origin = "https://studio.acme.com";
        let c = studio_app_config(Mode::SelfHosted, origin);
        assert_eq!(
            c["redirectUris"],
            json!(["https://studio.acme.com/callback"])
        );
        assert_eq!(
            c["postLogoutRedirectUris"],
            json!(["https://studio.acme.com/"])
        );
        assert_eq!(
            c["loginVersion"]["loginV2"]["baseUri"],
            "https://studio.acme.com/"
        );
        assert_eq!(
            studio_app_create("p1", Mode::SelfHosted, origin)["oidcConfiguration"],
            c
        );
        assert_eq!(
            studio_app_update("p1", "a2", Mode::SelfHosted, origin)["oidcConfiguration"],
            c
        );
        // An app registered for another Studio origin drifts, so bootstrap updates it.
        let old = json!({"oidcConfiguration": studio_app_config(Mode::SelfHosted, "http://old-studio.example")});
        let drift = studio_app_drift(&old, Mode::SelfHosted, origin);
        assert!(
            drift.contains(&"redirectUris") && drift.contains(&"postLogoutRedirectUris"),
            "{drift:?}"
        );
        let same = json!({"oidcConfiguration": c});
        assert!(studio_app_drift(&same, Mode::SelfHosted, origin).is_empty());
    }
}
