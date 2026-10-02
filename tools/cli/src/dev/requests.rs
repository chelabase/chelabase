//! Pure request bodies and response parsers for the bootstrap's Zitadel v4 calls.
//! REST v2 for orgs and users; ConnectRPC JSON for projects and applications.

use anyhow::{Context, Result};
use base64::Engine;
use cheladb_types::mode::Mode;
use serde_json::{Value, json};

pub const KEY_EXPIRY: &str = "2099-01-01T00:00:00Z";

pub fn default_org_query() -> Value {
    json!({"queries": [{"defaultQuery": {}}]})
}

pub fn org_id_from(listed: &Value) -> Option<String> {
    listed.pointer("/result/0/id")?.as_str().map(str::to_string)
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

/// Worktree slot `slot`'s Studio origin: port 3000 + 100 × slot (slot 0 is the main checkout).
pub fn studio_origin(slot: u16) -> String {
    format!("http://localhost:{}", 3000 + 100 * slot)
}

/// Slot 0's Studio app keeps its name; slot N's is `cheladb-studio-s<N>`.
pub fn studio_app_name(slot: u16) -> String {
    if slot == 0 {
        STUDIO_APP.to_string()
    } else {
        format!("{STUDIO_APP}-s{slot}")
    }
}

/// Studio: a public SPA using PKCE, issuing JWT access tokens the gateway verifies via JWKS.
/// `developmentMode` allows the plain-http localhost redirects. Each worktree slot has its own
/// app that sends sign-ins to its own Studio's /login (login v2 `baseUri`). In self-hosted mode
/// slot 0's names it too: the instance no longer forces its own login URL there (W8), and an app
/// without a base URI would get Zitadel's v1 login. Platform mode keeps slot 0's app on the
/// instance default.
pub fn studio_app_config(slot: u16, mode: Mode) -> Value {
    let origin = studio_origin(slot);
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
    if slot > 0 || mode == Mode::SelfHosted {
        config["loginVersion"] = json!({"loginV2": {"baseUri": format!("{origin}/")}});
    }
    config
}

pub fn studio_app_create(project_id: &str, slot: u16, mode: Mode) -> Value {
    json!({"projectId": project_id, "name": studio_app_name(slot),
           "oidcConfiguration": studio_app_config(slot, mode)})
}

/// No `name`: the app is found by it, and Zitadel refuses an unchanged name (COMMAND-2m8vx).
pub fn studio_app_update(project_id: &str, app_id: &str, slot: u16, mode: Mode) -> Value {
    json!({"projectId": project_id, "applicationId": app_id,
           "oidcConfiguration": studio_app_config(slot, mode)})
}

/// The worktree slot a Studio app serves, from its name (`cheladb-studio` is slot 0,
/// `cheladb-studio-s<N>` slot N, 1-9); `None` for any other app.
pub fn studio_slot_from_name(name: &str) -> Option<u16> {
    if name == STUDIO_APP {
        return Some(0);
    }
    let digit = name.strip_prefix(STUDIO_APP)?.strip_prefix("-s")?;
    match digit.as_bytes() {
        [d @ b'1'..=b'9'] => Some(u16::from(d - b'0')),
        _ => None,
    }
}

/// Names of the Studio apps in a `ListApplications` result without a login v2 base URI.
pub fn studio_apps_without_login(listed: &Value) -> Vec<String> {
    let Some(apps) = listed.get("applications").and_then(Value::as_array) else {
        return Vec::new();
    };
    apps.iter()
        .filter_map(|a| a.get("name").and_then(Value::as_str))
        .zip(apps.iter())
        .filter(|(name, _)| studio_slot_from_name(name).is_some())
        .filter(|(_, app)| {
            app.pointer("/oidcConfiguration/loginVersion/loginV2/baseUri")
                .and_then(Value::as_str)
                .is_none_or(str::is_empty)
        })
        .map(|(name, _)| name.to_string())
        .collect()
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

pub const STUDIO_APP: &str = "cheladb-studio";
pub const API_APP: &str = "cheladb-orchestrator";

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
pub fn studio_app_drift(app: &Value, slot: u16, mode: Mode) -> Vec<&'static str> {
    let want = studio_app_config(slot, mode);
    [
        "redirectUris",
        "postLogoutRedirectUris",
        "accessTokenType",
        "developmentMode",
        "loginVersion",
    ]
    .into_iter()
    // Platform mode: slot 0's app leaves the login to the instance default, as before slots.
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

pub fn machine_user_create(org_id: &str, username: &str) -> Value {
    json!({"organizationId": org_id, "username": username,
           "machine": {"name": "Cheladb Demo", "accessTokenType": "ACCESS_TOKEN_TYPE_JWT"}})
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
    json!({"senderAddress": "noreply@localhost", "senderName": "Cheladb (local)", "tls": false,
           "host": host, "user": "cheladb", "password": "cheladb", "description": "cheladb local mailpit"})
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
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
    fn projects_are_found_by_exact_name() {
        assert_eq!(
            project_search("cheladb"),
            json!({"filters": [{"projectNameFilter": {"projectName": "cheladb", "method": "TEXT_FILTER_METHOD_EQUALS"}}]})
        );
        assert_eq!(
            project_create("o1", "cheladb"),
            json!({"organizationId": "o1", "name": "cheladb"})
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
            {"applicationId": "a1", "name": "cheladb-orchestrator", "apiConfiguration": {"clientId": "c-api"}},
            {"applicationId": "a2", "name": "cheladb-studio", "oidcConfiguration": {"clientId": "c-spa", "redirectUris": ["x"]}}
        ]});
        let api = find_app(&listed, "cheladb-orchestrator").unwrap();
        assert_eq!((api.id.as_str(), api.client_id.as_str()), ("a1", "c-api"));
        let spa = find_app(&listed, "cheladb-studio").unwrap();
        assert_eq!(spa.client_id, "c-spa");
        assert!(find_app(&listed, "other").is_none());
    }

    // The app's shape; platform mode keeps slot 0's login on the instance default, as before.
    #[test]
    fn the_studio_app_is_a_pkce_spa_with_jwt_access_tokens() {
        assert_eq!(studio_app_name(0), "cheladb-studio");
        let c = studio_app_config(0, Mode::Platform);
        assert_eq!(c["redirectUris"], json!(["http://localhost:3000/callback"]));
        assert_eq!(
            c["postLogoutRedirectUris"],
            json!(["http://localhost:3000/"])
        );
        assert_eq!(c["applicationType"], "OIDC_APP_TYPE_USER_AGENT");
        assert_eq!(c["authMethodType"], "OIDC_AUTH_METHOD_TYPE_NONE");
        assert_eq!(c["accessTokenType"], "OIDC_TOKEN_TYPE_JWT");
        assert_eq!(c["developmentMode"], true);
        assert!(c.get("loginVersion").is_none(), "{c}");
        let create = studio_app_create("p1", 0, Mode::Platform);
        assert_eq!(create["oidcConfiguration"], c);
        assert_eq!(create["name"], "cheladb-studio");
        assert_eq!(
            studio_app_update("p1", "a2", 0, Mode::Platform)["applicationId"],
            "a2"
        );
    }

    // W8: self-hosted mode stops forcing the instance's login URL, so every Studio app,
    // slot 0's included, names its own login (otherwise it falls back to Zitadel's v1 login).
    #[test]
    fn every_self_hosted_studio_app_names_its_own_login() {
        let c = studio_app_config(0, Mode::SelfHosted);
        assert_eq!(
            c["loginVersion"],
            json!({"loginV2": {"baseUri": "http://localhost:3000/"}})
        );
        assert_eq!(
            studio_app_create("p1", 0, Mode::SelfHosted)["oidcConfiguration"],
            c
        );
    }

    // Worktree slot N (W7): its own app, redirects and login UI on Studio's port 3000 + 100N.
    #[test]
    fn a_slot_studio_app_has_its_own_uris_and_login() {
        assert_eq!(studio_app_name(3), "cheladb-studio-s3");
        for mode in [Mode::SelfHosted, Mode::Platform] {
            let c = studio_app_config(3, mode);
            assert_eq!(c["redirectUris"], json!(["http://localhost:3300/callback"]));
            assert_eq!(
                c["postLogoutRedirectUris"],
                json!(["http://localhost:3300/"])
            );
            assert_eq!(
                c["loginVersion"],
                json!({"loginV2": {"baseUri": "http://localhost:3300/"}})
            );
            assert_eq!(c["accessTokenType"], "OIDC_TOKEN_TYPE_JWT");
            let update = studio_app_update("p1", "a9", 3, mode);
            assert_eq!(update["oidcConfiguration"], c);
            // Zitadel refuses an UpdateApplication whose name hasn't changed
            // (COMMAND-2m8vx "No changes") before it looks at the OIDC settings.
            assert!(update.get("name").is_none(), "{update}");
        }
    }

    #[test]
    fn studio_app_drift_names_what_changed() {
        let good = json!({"oidcConfiguration": studio_app_config(0, Mode::Platform)});
        assert!(studio_app_drift(&good, 0, Mode::Platform).is_empty());
        let mut bad = good.clone();
        bad["oidcConfiguration"]["redirectUris"] = json!(["http://old/callback"]);
        bad["oidcConfiguration"]["accessTokenType"] = json!("OIDC_TOKEN_TYPE_BEARER");
        assert_eq!(
            studio_app_drift(&bad, 0, Mode::Platform),
            vec!["redirectUris", "accessTokenType"]
        );
        let slot = json!({"oidcConfiguration": studio_app_config(2, Mode::SelfHosted)});
        assert!(studio_app_drift(&slot, 2, Mode::SelfHosted).is_empty());
        let mut no_login = slot.clone();
        no_login["oidcConfiguration"]
            .as_object_mut()
            .unwrap()
            .remove("loginVersion");
        assert_eq!(
            studio_app_drift(&no_login, 2, Mode::SelfHosted),
            vec!["loginVersion"]
        );
    }

    // W8: slot 0's app from before W8 (no loginVersion) is updated in self-hosted mode only.
    #[test]
    fn slot_0_login_drift_counts_only_in_self_hosted_mode() {
        let before = json!({"oidcConfiguration": studio_app_config(0, Mode::Platform)});
        assert_eq!(
            studio_app_drift(&before, 0, Mode::SelfHosted),
            vec!["loginVersion"]
        );
        assert!(studio_app_drift(&before, 0, Mode::Platform).is_empty());
        let after = json!({"oidcConfiguration": studio_app_config(0, Mode::SelfHosted)});
        assert!(studio_app_drift(&after, 0, Mode::SelfHosted).is_empty());
    }

    #[test]
    fn studio_slots_come_from_app_names() {
        assert_eq!(studio_slot_from_name("cheladb-studio"), Some(0));
        assert_eq!(studio_slot_from_name("cheladb-studio-s4"), Some(4));
        assert_eq!(studio_slot_from_name("cheladb-studio-s9"), Some(9));
        for other in [
            "cheladb-studio-s0",
            "cheladb-studio-s10",
            "cheladb-studio-sx",
            "cheladb-studio-",
            "cheladb-orchestrator",
        ] {
            assert_eq!(studio_slot_from_name(other), None, "{other}");
        }
    }

    // An app without a base URI would fall back to Zitadel's v1 login once the instance
    // no longer forces login v2: bootstrap names each one.
    #[test]
    fn studio_apps_without_a_login_base_are_named() {
        let login = |uri: &str| json!({"loginV2": {"baseUri": uri}});
        let listed = json!({"applications": [
            {"applicationId": "a1", "name": "cheladb-orchestrator", "apiConfiguration": {"clientId": "c-api"}},
            {"applicationId": "a2", "name": "cheladb-studio", "oidcConfiguration": {"clientId": "c0"}},
            {"applicationId": "a3", "name": "cheladb-studio-s1", "oidcConfiguration": {"clientId": "c1", "loginVersion": login("http://localhost:3100/")}},
            {"applicationId": "a4", "name": "cheladb-studio-s2", "oidcConfiguration": {"clientId": "c2", "loginVersion": login("")}},
            {"applicationId": "a5", "name": "cheladb-studio-s3", "oidcConfiguration": {"clientId": "c3", "loginVersion": {"loginV1": {}}}}
        ]});
        assert_eq!(
            studio_apps_without_login(&listed),
            vec!["cheladb-studio", "cheladb-studio-s2", "cheladb-studio-s3"]
        );
        assert!(studio_apps_without_login(&json!({})).is_empty());
    }

    // zitadel.feature.v2.FeatureService: the effective value (inheritance), and protojson's
    // omitted `required: false`.
    #[test]
    fn the_login_v2_feature_is_read_with_inheritance() {
        assert_eq!(instance_features_get(), json!({"inheritance": true}));
        assert_eq!(
            login_v2_from(&json!({"loginV2": {"required": true,
                "baseUri": "http://localhost:3000/", "source": "SOURCE_INSTANCE"}})),
            (true, Some("http://localhost:3000/".to_string()))
        );
        assert_eq!(
            login_v2_from(&json!({"loginV2": {"baseUri": "http://localhost:3000/"}})),
            (false, Some("http://localhost:3000/".to_string()))
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
            login_v2_optional("http://localhost:3000/"),
            json!({"loginV2": {"required": false, "baseUri": "http://localhost:3000/"}})
        );
    }

    #[test]
    fn the_demo_machine_user_and_its_key() {
        assert_eq!(
            machine_user_search("cheladb-demo"),
            json!({"queries": [{"userNameQuery": {"userName": "cheladb-demo", "method": "TEXT_QUERY_METHOD_EQUALS"}}]})
        );
        let create = machine_user_create("o1", "cheladb-demo");
        assert_eq!(create["username"], "cheladb-demo");
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
        assert_eq!(smtp_payload("mailpit:1025")["password"], "cheladb");
    }
}
