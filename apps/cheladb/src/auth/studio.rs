//! Studio (browser SPA) wiring for local dev: the Zitadel OIDC app, the
//! Mailpit SMTP provider for signup emails, and Studio's `.env.local`.

use serde_json::{json, Value};

pub const STUDIO_APP_NAME: &str = "cheladb-studio";
pub const STUDIO_ORIGIN: &str = "http://localhost:3000";
pub const GATEWAY_URL: &str = "http://127.0.0.1:8000";
/// Mailpit's SMTP listener as Zitadel sees it on the Compose network.
pub const MAILPIT_SMTP_HOST: &str = "mailpit:1025";

/// Zitadel OIDC app for Studio: a public SPA using PKCE, issuing JWT access
/// tokens so the gateway can verify them via JWKS. `devMode` allows the
/// plain-http localhost redirects.
pub fn studio_app_payload(origin: &str) -> Value {
    json!({
        "name": STUDIO_APP_NAME,
        "redirectUris": [format!("{origin}/callback")],
        "postLogoutRedirectUris": [format!("{origin}/")],
        "responseTypes": ["OIDC_RESPONSE_TYPE_CODE"],
        "grantTypes": ["OIDC_GRANT_TYPE_AUTHORIZATION_CODE", "OIDC_GRANT_TYPE_REFRESH_TOKEN"],
        "appType": "OIDC_APP_TYPE_USER_AGENT",
        "authMethodType": "OIDC_AUTH_METHOD_TYPE_NONE",
        "version": "OIDC_VERSION_1_0",
        "accessTokenType": "OIDC_TOKEN_TYPE_JWT",
        "devMode": true,
        "idTokenUserinfoAssertion": true
    })
}

/// Zitadel SMTP provider for Mailpit. Credentials are required even though
/// Mailpit accepts any: Zitadel 2.71 treats a config without a password as
/// missing and never sends mail.
pub fn smtp_payload(host: &str) -> Value {
    json!({
        "senderAddress": "noreply@localhost",
        "senderName": "Cheladb (local)",
        "tls": false,
        "host": host,
        "user": "cheladb",
        "password": "cheladb",
        "description": "cheladb local mailpit"
    })
}

/// An existing Zitadel SMTP provider, as listed by `/admin/v1/smtp/_search`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SmtpConfig {
    pub id: String,
    pub active: bool,
    /// Whether a user is set. Zitadel doesn't list passwords, but configs this
    /// bootstrap creates always set both; one without a user predates that and
    /// can never send mail.
    pub has_credentials: bool,
}

/// The SMTP config already pointing at `host`.
pub fn find_smtp_config(search: &Value, host: &str) -> Option<SmtpConfig> {
    let config = search
        .get("result")?
        .as_array()?
        .iter()
        .find(|c| c.get("host").and_then(Value::as_str) == Some(host))?;
    Some(SmtpConfig {
        id: config.get("id")?.as_str()?.to_string(),
        active: config.get("state").and_then(Value::as_str) == Some("SMTP_CONFIG_ACTIVE"),
        has_credentials: config
            .get("user")
            .and_then(Value::as_str)
            .is_some_and(|u| !u.is_empty()),
    })
}

/// Settings of an existing Studio app (an entry of the project apps search)
/// that differ from what `studio_app_payload` would create, as
/// `"field: have X, want Y"` lines.
pub fn studio_app_drift(app: &Value, origin: &str) -> Vec<String> {
    let want = studio_app_payload(origin);
    let have = app.get("oidcConfig").cloned().unwrap_or(Value::Null);
    [
        "redirectUris",
        "postLogoutRedirectUris",
        "responseTypes",
        "grantTypes",
        "appType",
        "authMethodType",
        "accessTokenType",
        "devMode",
    ]
    .into_iter()
    .filter(|field| have.get(field) != want.get(field))
    .map(|field| {
        format!(
            "{field}: have {}, want {}",
            have.get(field).unwrap_or(&Value::Null),
            want[field]
        )
    })
    .collect()
}

pub struct StudioEnv<'a> {
    pub issuer: &'a str,
    pub client_id: &'a str,
    pub project_id: &'a str,
    pub org_id: &'a str,
    pub api_url: &'a str,
}

pub fn render_studio_env(env: &StudioEnv<'_>) -> String {
    format!(
        "# Written by `cheladb auth bootstrap`; regenerate instead of editing.\n\
VITE_ZITADEL_ISSUER={}\nVITE_ZITADEL_CLIENT_ID={}\nVITE_ZITADEL_PROJECT_ID={}\n\
VITE_ZITADEL_ORG_ID={}\nVITE_API_URL={}\n",
        env.issuer, env.client_id, env.project_id, env.org_id, env.api_url
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn studio_app_is_a_pkce_spa_issuing_jwt_access_tokens() {
        let p = studio_app_payload("http://localhost:3000");
        assert_eq!(p["name"], STUDIO_APP_NAME);
        assert_eq!(p["appType"], "OIDC_APP_TYPE_USER_AGENT");
        assert_eq!(p["authMethodType"], "OIDC_AUTH_METHOD_TYPE_NONE");
        assert_eq!(p["accessTokenType"], "OIDC_TOKEN_TYPE_JWT");
        assert_eq!(p["devMode"], true);
        assert_eq!(p["redirectUris"], json!(["http://localhost:3000/callback"]));
        assert_eq!(p["postLogoutRedirectUris"], json!(["http://localhost:3000/"]));
        assert_eq!(p["responseTypes"], json!(["OIDC_RESPONSE_TYPE_CODE"]));
        assert_eq!(
            p["grantTypes"],
            json!(["OIDC_GRANT_TYPE_AUTHORIZATION_CODE", "OIDC_GRANT_TYPE_REFRESH_TOKEN"])
        );
    }

    #[test]
    fn smtp_payload_targets_mailpit_without_tls() {
        let p = smtp_payload(MAILPIT_SMTP_HOST);
        assert_eq!(p["host"], "mailpit:1025");
        assert_eq!(p["tls"], false);
        assert!(p["senderAddress"].as_str().unwrap().contains('@'));
    }

    // Zitadel 2.71 treats an SMTP config without a password as missing
    // (`QUERY-Wrs3gw` in GetActiveEmailConfig), so no email is ever sent.
    #[test]
    fn smtp_payload_always_carries_credentials() {
        let p = smtp_payload(MAILPIT_SMTP_HOST);
        assert!(!p["user"].as_str().unwrap().is_empty());
        assert!(!p["password"].as_str().unwrap().is_empty());
    }

    #[test]
    fn find_smtp_config_reports_id_state_and_credentials_for_the_host() {
        let search = json!({ "result": [
            { "id": "other", "host": "smtp.example.com:587", "state": "SMTP_CONFIG_ACTIVE", "user": "x" },
            { "id": "m1", "host": "mailpit:1025", "state": "SMTP_CONFIG_INACTIVE", "user": "cheladb" }
        ]});
        assert_eq!(
            find_smtp_config(&search, "mailpit:1025"),
            Some(SmtpConfig { id: "m1".into(), active: false, has_credentials: true })
        );
        let active = json!({ "result": [
            { "id": "m1", "host": "mailpit:1025", "state": "SMTP_CONFIG_ACTIVE", "user": "cheladb" }
        ]});
        assert!(find_smtp_config(&active, "mailpit:1025").unwrap().active);
        assert_eq!(find_smtp_config(&json!({}), "mailpit:1025"), None);
    }

    // A config created before credentials were required can never send mail
    // (Zitadel treats a password-less config as missing); it must be replaced.
    #[test]
    fn find_smtp_config_flags_a_config_without_credentials() {
        let legacy = json!({ "result": [
            { "id": "old", "host": "mailpit:1025", "state": "SMTP_CONFIG_ACTIVE" }
        ]});
        assert!(!find_smtp_config(&legacy, "mailpit:1025").unwrap().has_credentials);
    }

    fn registered_app(origin: &str) -> Value {
        let p = studio_app_payload(origin);
        json!({ "name": STUDIO_APP_NAME, "oidcConfig": {
            "redirectUris": p["redirectUris"],
            "postLogoutRedirectUris": p["postLogoutRedirectUris"],
            "responseTypes": p["responseTypes"],
            "grantTypes": p["grantTypes"],
            "appType": p["appType"],
            "authMethodType": p["authMethodType"],
            "accessTokenType": p["accessTokenType"],
            "devMode": p["devMode"],
            "clientId": "c1"
        }})
    }

    #[test]
    fn matching_studio_app_has_no_drift() {
        assert!(studio_app_drift(&registered_app(STUDIO_ORIGIN), STUDIO_ORIGIN).is_empty());
    }

    #[test]
    fn studio_app_drift_names_each_changed_setting() {
        let mut app = registered_app(STUDIO_ORIGIN);
        app["oidcConfig"]["accessTokenType"] = json!("OIDC_TOKEN_TYPE_BEARER");
        app["oidcConfig"]["redirectUris"] = json!(["http://localhost:4000/callback"]);
        let drift = studio_app_drift(&app, STUDIO_ORIGIN);
        assert_eq!(drift.len(), 2, "{drift:?}");
        assert!(drift.iter().any(|d| d.contains("accessTokenType")));
        assert!(drift.iter().any(|d| d.contains("redirectUris")));
    }

    #[test]
    fn studio_env_lists_every_vite_setting() {
        let text = render_studio_env(&StudioEnv {
            issuer: "http://localhost:8080",
            client_id: "c1",
            project_id: "p1",
            org_id: "o1",
            api_url: GATEWAY_URL,
        });
        for line in [
            "VITE_ZITADEL_ISSUER=http://localhost:8080",
            "VITE_ZITADEL_CLIENT_ID=c1",
            "VITE_ZITADEL_PROJECT_ID=p1",
            "VITE_ZITADEL_ORG_ID=o1",
            "VITE_API_URL=http://127.0.0.1:8000",
        ] {
            assert!(text.lines().any(|l| l == line), "missing {line} in:\n{text}");
        }
    }
}
