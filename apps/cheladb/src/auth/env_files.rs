//! `demo.env` (host processes) and Studio's `.env.local`.

pub struct DemoEnv<'a> {
    pub issuer: &'a str,
    pub audiences: &'a str,
    pub org_id: &'a str,
    pub project_id: &'a str,
    pub api_client_id: &'a str,
    pub demo_user_id: &'a str,
    pub demo_tenant: &'a str,
    pub signing_key: &'a str,
    pub verify_key: &'a str,
    pub secrets_key: &'a str,
    pub ui_auth_token_file: &'a str,
    pub ui_auth_cookie_key: &'a str,
}

pub fn render_demo_env(e: &DemoEnv<'_>) -> String {
    format!(
        "CHELADB_ZITADEL_ISSUER={}\nCHELADB_ZITADEL_JWKS_URL={}/oauth/v2/keys\nCHELADB_ZITADEL_AUDIENCES={}\n\
CHELADB_ZITADEL_ORG_ID={}\nCHELADB_ZITADEL_PROJECT_ID={}\nCHELADB_ZITADEL_API_CLIENT_ID={}\n\
CHELADB_DEMO_USER_ID={}\nCHELADB_DEMO_TENANT_ID={}\nCHELADB_INTERNAL_SIGNING_KEY={}\n\
CHELADB_INTERNAL_VERIFY_KEY={}\nCHELADB_SECRETS_KEY={}\nCHELADB_UI_AUTH_TOKEN_FILE={}\n\
CHELADB_UI_AUTH_COOKIE_KEY={}\n",
        e.issuer,
        e.issuer,
        e.audiences,
        e.org_id,
        e.project_id,
        e.api_client_id,
        e.demo_user_id,
        e.demo_tenant,
        e.signing_key,
        e.verify_key,
        e.secrets_key,
        e.ui_auth_token_file,
        e.ui_auth_cookie_key
    )
}

pub fn render_studio_env(
    issuer: &str,
    client_id: &str,
    project_id: &str,
    org_id: &str,
    api_url: &str,
) -> String {
    format!(
        "# Written by `cheladb auth bootstrap`; regenerate instead of editing.\n\
VITE_ZITADEL_ISSUER={issuer}\nVITE_ZITADEL_CLIENT_ID={client_id}\nVITE_ZITADEL_PROJECT_ID={project_id}\n\
VITE_ZITADEL_ORG_ID={org_id}\nVITE_API_URL={api_url}\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn demo_env_names_the_ui_auth_files() {
        let text = render_demo_env(&DemoEnv {
            issuer: "http://localhost:8080",
            audiences: "p1,c1",
            org_id: "o1",
            project_id: "p1",
            api_client_id: "c1",
            demo_user_id: "u1",
            demo_tenant: "t1",
            signing_key: "/s",
            verify_key: "/v",
            secrets_key: "/k",
            ui_auth_token_file: "/z/login-client.pat",
            ui_auth_cookie_key: "/u/cookie.key",
        });
        assert!(text.contains("CHELADB_UI_AUTH_TOKEN_FILE=/z/login-client.pat\n"));
        assert!(text.contains("CHELADB_UI_AUTH_COOKIE_KEY=/u/cookie.key\n"));
        assert!(text.contains("CHELADB_ZITADEL_JWKS_URL=http://localhost:8080/oauth/v2/keys\n"));
    }

    #[test]
    fn studio_env_has_every_key_studio_reads() {
        let text = render_studio_env(
            "http://localhost:8080",
            "c",
            "p",
            "o",
            "http://127.0.0.1:8000",
        );
        for key in [
            "VITE_ZITADEL_ISSUER=",
            "VITE_ZITADEL_CLIENT_ID=c",
            "VITE_ZITADEL_PROJECT_ID=p",
            "VITE_ZITADEL_ORG_ID=o",
            "VITE_API_URL=http://127.0.0.1:8000",
        ] {
            assert!(text.contains(key), "{key}");
        }
    }
}
