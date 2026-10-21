//! `demo.env` (host processes) and Studio's `.env.local`.
//!
//! demo.env carries no internal bearer (RT1 T1, S27): every app reads it, so the stack mounts each
//! generated bearer into the services that use it and sets `<NAME>_FILE` per service.

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
CHELABASE_ZITADEL_ADMIN_PAT_FILE={}\nCHELABASE_APP_JWT_PRIVATE_KEY={}\nCHELABASE_APP_JWT_PUBLIC_KEY={}\n",
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

/// The internal bearers a demo.env written before RT1 T1 carried (their public development
/// values). The stack now sets `<NAME>_FILE` per service, and a service refuses both forms.
pub const LEGACY_BEARER_KEYS: [&str; 4] = [
    "CHELABASE_DATA_KEYS_TOKEN",
    "CHELABASE_REST_INTERNAL_TOKEN",
    "CHELABASE_APP_AUTH_INTERNAL_TOKEN",
    "CHELABASE_APP_SESSIONS_INTERNAL_TOKEN",
];

/// `text` without its [`LEGACY_BEARER_KEYS`] lines, or `None` when it has none. `up` scrubs an
/// existing demo.env before any container is created from it (the bootstrap rewrites it later).
pub fn without_legacy_bearers(text: &str) -> Option<String> {
    let is_legacy = |line: &str| {
        line.split_once('=')
            .is_some_and(|(k, _)| LEGACY_BEARER_KEYS.contains(&k.trim()))
    };
    if !text.lines().any(is_legacy) {
        return None;
    }
    let mut out = String::with_capacity(text.len());
    for line in text.lines().filter(|l| !is_legacy(l)) {
        out.push_str(line);
        out.push('\n');
    }
    Some(out)
}

pub fn render_studio_env(
    issuer: &str,
    client_id: &str,
    project_id: &str,
    org_id: &str,
    api_url: &str,
    studio_host: &str,
) -> String {
    format!(
        "# Written by `chelabase dev bootstrap`; regenerate instead of editing.\n\
VITE_ZITADEL_ISSUER={issuer}\nVITE_ZITADEL_CLIENT_ID={client_id}\nVITE_ZITADEL_PROJECT_ID={project_id}\n\
VITE_ZITADEL_ORG_ID={org_id}\nVITE_API_URL={api_url}\nVITE_STUDIO_HOST={studio_host}\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn demo_env_names_the_auth_files() {
        let text = render_demo_env(&DemoEnv {
            issuer: "http://auth.chelabase.localhost:8000",
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
        assert!(text.contains(
            "CHELABASE_ZITADEL_JWKS_URL=http://auth.chelabase.localhost:8000/oauth/v2/keys\n"
        ));
    }

    #[test]
    fn demo_env_has_the_data_api_wiring() {
        let text = render_demo_env(&DemoEnv {
            issuer: "http://auth.chelabase.localhost:8000",
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
        // RT1 T1 (S27): the internal bearers are generated secrets mounted per service, never in
        // demo.env (`workbench::r11_tests::demo_env_holds_no_internal_bearer`).
        // No ports in demo.env: the apps default to the dev stack's.
        for key in [
            "CHELABASE_REST_URL=",
            "CHELABASE_REST_INTERNAL_URL=",
            "CHELABASE_INTERNAL_URL=",
        ] {
            assert!(!text.contains(key), "{key} in {text}");
        }
    }

    // RT1 T1: an older demo.env's public bearers are scrubbed; everything else is kept.
    #[test]
    fn legacy_bearers_are_scrubbed_from_an_older_demo_env() {
        let old = "CHELABASE_ZITADEL_ISSUER=http://a\nCHELABASE_DATA_KEYS_TOKEN=chelabase-dev-data-keys\n\
CHELABASE_REST_INTERNAL_TOKEN=chelabase-dev-rest-internal\nCHELABASE_SECRETS_KEY=/k\n\
CHELABASE_APP_AUTH_INTERNAL_TOKEN=chelabase-dev-app-auth\nCHELABASE_APP_SESSIONS_INTERNAL_TOKEN=x\n";
        let new = without_legacy_bearers(old).expect("changed");
        assert_eq!(new, "CHELABASE_ZITADEL_ISSUER=http://a\nCHELABASE_SECRETS_KEY=/k\n");
        assert_eq!(without_legacy_bearers(&new), None, "nothing left to scrub");
        // A key that merely starts with one of the names is kept.
        assert_eq!(without_legacy_bearers("CHELABASE_DATA_KEYS_TOKEN_FILE=/f\n"), None);
    }

    #[test]
    fn studio_env_has_every_key_studio_reads() {
        let text = render_studio_env(
            "http://auth.chelabase.localhost:8000",
            "c",
            "p",
            "o",
            "http://api.chelabase.localhost:8000",
            "studio.chelabase.localhost:8000",
        );
        for key in [
            "VITE_ZITADEL_ISSUER=http://auth.chelabase.localhost:8000\n",
            "VITE_ZITADEL_CLIENT_ID=c\n",
            "VITE_ZITADEL_PROJECT_ID=p\n",
            "VITE_ZITADEL_ORG_ID=o\n",
            "VITE_API_URL=http://api.chelabase.localhost:8000\n",
            "VITE_STUDIO_HOST=studio.chelabase.localhost:8000\n",
        ] {
            assert!(text.contains(key), "{key}");
        }
    }

    #[test]
    fn demo_env_names_the_compute_jwt_dir() {
        let text = render_demo_env(&DemoEnv {
            issuer: "http://auth.chelabase.localhost:8000",
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
            issuer: "http://auth.chelabase.localhost:8000",
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
        ] {
            assert!(text.contains(line), "{line} in {text}");
        }
    }
}
