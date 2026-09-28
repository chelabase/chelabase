//! Pure request bodies and response parsers for the bootstrap's Zitadel v4 calls.
//! REST v2 for orgs and users; ConnectRPC JSON for projects and applications.

use anyhow::{Context, Result};
use base64::Engine;
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

/// Studio: a public SPA using PKCE, issuing JWT access tokens the gateway verifies via JWKS.
/// `developmentMode` allows the plain-http localhost redirects.
pub fn studio_app_config(origin: &str) -> Value {
    json!({
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
    })
}

pub fn studio_app_create(project_id: &str, origin: &str) -> Value {
    json!({"projectId": project_id, "name": STUDIO_APP, "oidcConfiguration": studio_app_config(origin)})
}

pub fn studio_app_update(project_id: &str, app_id: &str, origin: &str) -> Value {
    json!({"projectId": project_id, "applicationId": app_id, "name": STUDIO_APP,
           "oidcConfiguration": studio_app_config(origin)})
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
pub fn studio_app_drift(app: &Value, origin: &str) -> Vec<&'static str> {
    let want = studio_app_config(origin);
    [
        "redirectUris",
        "postLogoutRedirectUris",
        "accessTokenType",
        "developmentMode",
    ]
    .into_iter()
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

    #[test]
    fn the_studio_app_is_a_pkce_spa_with_jwt_access_tokens() {
        let c = studio_app_config("http://localhost:3000");
        assert_eq!(c["redirectUris"], json!(["http://localhost:3000/callback"]));
        assert_eq!(
            c["postLogoutRedirectUris"],
            json!(["http://localhost:3000/"])
        );
        assert_eq!(c["applicationType"], "OIDC_APP_TYPE_USER_AGENT");
        assert_eq!(c["authMethodType"], "OIDC_AUTH_METHOD_TYPE_NONE");
        assert_eq!(c["accessTokenType"], "OIDC_TOKEN_TYPE_JWT");
        assert_eq!(c["developmentMode"], true);
        assert_eq!(
            studio_app_create("p1", "http://localhost:3000")["oidcConfiguration"],
            c
        );
        assert_eq!(
            studio_app_update("p1", "a2", "http://localhost:3000")["applicationId"],
            "a2"
        );
    }

    #[test]
    fn studio_app_drift_names_what_changed() {
        let good = json!({"oidcConfiguration": studio_app_config("http://localhost:3000")});
        assert!(studio_app_drift(&good, "http://localhost:3000").is_empty());
        let mut bad = good.clone();
        bad["oidcConfiguration"]["redirectUris"] = json!(["http://old/callback"]);
        bad["oidcConfiguration"]["accessTokenType"] = json!("OIDC_TOKEN_TYPE_BEARER");
        assert_eq!(
            studio_app_drift(&bad, "http://localhost:3000"),
            vec!["redirectUris", "accessTokenType"]
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
