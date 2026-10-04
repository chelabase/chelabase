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
    pub auth_token_file: &'a str,
    pub auth_cookie_key: &'a str,
    pub compute_jwt_dir: &'a str,
    pub admin_pat_file: &'a str,
    pub app_jwt_private_key: &'a str,
    pub app_jwt_public_key: &'a str,
}

pub fn render_demo_env(e: &DemoEnv<'_>) -> String {
    format!(
        "CHELABASE_ZITADEL_ISSUER={}\nCHELABASE_ZITADEL_JWKS_URL={}/oauth/v2/keys\nCHELABASE_ZITADEL_AUDIENCES={}\n\
CHELABASE_ZITADEL_ORG_ID={}\nCHELABASE_ZITADEL_PROJECT_ID={}\nCHELABASE_ZITADEL_API_CLIENT_ID={}\n\
CHELABASE_DEMO_USER_ID={}\nCHELABASE_DEMO_TENANT_ID={}\nCHELABASE_INTERNAL_SIGNING_KEY={}\n\
CHELABASE_INTERNAL_VERIFY_KEY={}\nCHELABASE_SECRETS_KEY={}\nCHELABASE_AUTH_TOKEN_FILE={}\n\
CHELABASE_AUTH_COOKIE_KEY={}\nCHELABASE_COMPUTE_JWT_DIR={}\n\
CHELABASE_DATA_KEYS_TOKEN=chelabase-dev-data-keys\nCHELABASE_REST_INTERNAL_TOKEN=chelabase-dev-rest-internal\n\
CHELABASE_ZITADEL_ADMIN_PAT_FILE={}\nCHELABASE_APP_JWT_PRIVATE_KEY={}\nCHELABASE_APP_JWT_PUBLIC_KEY={}\n\
CHELABASE_APP_AUTH_INTERNAL_TOKEN=chelabase-dev-app-auth\nCHELABASE_APP_SESSIONS_INTERNAL_TOKEN=chelabase-dev-app-sessions\n",
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
        e.auth_token_file,
        e.auth_cookie_key,
        e.compute_jwt_dir,
        e.admin_pat_file,
        e.app_jwt_private_key,
        e.app_jwt_public_key
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
        "# Written by `chelabase dev bootstrap`; regenerate instead of editing.\n\
VITE_ZITADEL_ISSUER={issuer}\nVITE_ZITADEL_CLIENT_ID={client_id}\nVITE_ZITADEL_PROJECT_ID={project_id}\n\
VITE_ZITADEL_ORG_ID={org_id}\nVITE_API_URL={api_url}\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn demo_env_names_the_auth_files() {
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
            auth_token_file: "/z/login-client.pat",
            auth_cookie_key: "/u/cookie.key",
            compute_jwt_dir: "/abs/neon",
            admin_pat_file: "/z/admin.pat",
            app_jwt_private_key: "/a/k",
            app_jwt_public_key: "/a/p",
        });
        assert!(text.contains("CHELABASE_AUTH_TOKEN_FILE=/z/login-client.pat\n"));
        assert!(text.contains("CHELABASE_AUTH_COOKIE_KEY=/u/cookie.key\n"));
        assert!(text.contains("CHELABASE_ZITADEL_JWKS_URL=http://localhost:8080/oauth/v2/keys\n"));
    }

    #[test]
    fn demo_env_has_the_data_api_wiring() {
        let text = render_demo_env(&DemoEnv {
            issuer: "http://localhost:8080",
            audiences: "p1,c1",
            org_id: "o",
            project_id: "p1",
            api_client_id: "c1",
            demo_user_id: "u",
            demo_tenant: "t",
            signing_key: "/s",
            verify_key: "/v",
            secrets_key: "/k",
            auth_token_file: "/tok",
            auth_cookie_key: "/cookie",
            compute_jwt_dir: "/abs/neon",
            admin_pat_file: "/z/admin.pat",
            app_jwt_private_key: "/a/k",
            app_jwt_public_key: "/a/p",
        });
        for line in [
            "CHELABASE_DATA_KEYS_TOKEN=chelabase-dev-data-keys\n",
            "CHELABASE_REST_INTERNAL_TOKEN=chelabase-dev-rest-internal\n",
        ] {
            assert!(text.contains(line), "{line} in {text}");
        }
        // No ports in demo.env: the apps default to the dev stack's.
        for key in [
            "CHELABASE_REST_URL=",
            "CHELABASE_REST_INTERNAL_URL=",
            "CHELABASE_INTERNAL_URL=",
        ] {
            assert!(!text.contains(key), "{key} in {text}");
        }
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

    #[test]
    fn demo_env_names_the_compute_jwt_dir() {
        let text = render_demo_env(&DemoEnv {
            issuer: "http://localhost:8080",
            audiences: "p1,c1",
            org_id: "o",
            project_id: "p1",
            api_client_id: "c1",
            demo_user_id: "u",
            demo_tenant: "t",
            signing_key: "/s",
            verify_key: "/v",
            secrets_key: "/k",
            auth_token_file: "/tok",
            auth_cookie_key: "/cookie",
            compute_jwt_dir: "/abs/neon",
            admin_pat_file: "/z/admin.pat",
            app_jwt_private_key: "/a/k",
            app_jwt_public_key: "/a/p",
        });
        assert!(
            text.contains("\nCHELABASE_COMPUTE_JWT_DIR=/abs/neon\n"),
            "{text}"
        );
    }

    #[test]
    fn demo_env_has_app_settings() {
        let text = render_demo_env(&DemoEnv {
            issuer: "http://localhost:8080",
            audiences: "p1,c1",
            org_id: "o",
            project_id: "p1",
            api_client_id: "c1",
            demo_user_id: "u",
            demo_tenant: "t",
            signing_key: "/s",
            verify_key: "/v",
            secrets_key: "/k",
            auth_token_file: "/tok",
            auth_cookie_key: "/cookie",
            compute_jwt_dir: "/abs/neon",
            admin_pat_file: "/z/admin.pat",
            app_jwt_private_key: "/a/app-jwt.key",
            app_jwt_public_key: "/a/app-jwt.pub",
        });
        for line in [
            "CHELABASE_ZITADEL_ADMIN_PAT_FILE=/z/admin.pat\n",
            "CHELABASE_APP_JWT_PRIVATE_KEY=/a/app-jwt.key\n",
            "CHELABASE_APP_JWT_PUBLIC_KEY=/a/app-jwt.pub\n",
            "CHELABASE_APP_AUTH_INTERNAL_TOKEN=chelabase-dev-app-auth\n",
            "CHELABASE_APP_SESSIONS_INTERNAL_TOKEN=chelabase-dev-app-sessions\n",
        ] {
            assert!(text.contains(line), "{line} in {text}");
        }
    }
}
