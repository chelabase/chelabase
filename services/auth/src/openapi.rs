//! The login API's OpenAPI document (`openapi/login.json`), generated from the handlers'
//! `#[utoipa::path]` annotations.

use utoipa::openapi::{InfoBuilder, LicenseBuilder, OpenApi};

/// The document for `/auth/v1/*`: the sign-in pages' API (tag `login`) and app users' flows
/// (tag `app-users`).
pub fn openapi() -> OpenApi {
    let (_, mut doc) = crate::app::api_router().split_for_parts();
    doc.info = InfoBuilder::new()
        .title("Chelabase login API")
        .version("1")
        .license(Some(LicenseBuilder::new().name("Apache-2.0").build()))
        .build();
    doc
}

#[cfg(test)]
mod tests {
    use super::openapi;
    use chelabase_routes::{ENDPOINTS, Service};
    use std::collections::BTreeSet;
    use std::path::PathBuf;

    fn json_path() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../openapi/login.json")
    }

    fn operations() -> Vec<(String, String, utoipa::openapi::path::Operation)> {
        let doc = openapi();
        let mut out = Vec::new();
        for (path, item) in doc.paths.paths {
            for (m, op) in [
                ("GET", item.get),
                ("POST", item.post),
                ("PUT", item.put),
                ("PATCH", item.patch),
                ("DELETE", item.delete),
            ] {
                if let Some(op) = op {
                    out.push((m.to_string(), path.clone(), op));
                }
            }
        }
        out
    }

    #[test]
    fn document_is_current() {
        let mut text = serde_json::to_string_pretty(&openapi()).unwrap();
        text.push('\n');
        let path = json_path();
        if std::env::var_os("CHELABASE_BLESS").is_some() {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, &text).unwrap();
        }
        let committed = std::fs::read_to_string(&path).unwrap_or_default();
        assert!(
            committed == text,
            "openapi/login.json is stale; run `just api-gen` (CHELABASE_BLESS=1)"
        );
    }

    #[test]
    fn document_matches_the_registry() {
        let documented: BTreeSet<(String, String)> =
            operations().into_iter().map(|(m, p, _)| (m, p)).collect();
        let registry: BTreeSet<(String, String)> = ENDPOINTS
            .iter()
            .filter(|e| e.service == Service::Auth && !e.name.ends_with(".health"))
            .map(|e| (e.method.as_str().to_string(), e.path.to_string()))
            .collect();
        assert!(!registry.is_empty());
        assert_eq!(documented, registry);
    }

    #[test]
    fn operation_ids_are_the_contract() {
        let mut got: Vec<(String, String, String)> = operations()
            .into_iter()
            .map(|(m, p, op)| (m, p, op.operation_id.unwrap_or_default()))
            .collect();
        got.sort();
        let mut want: Vec<(String, String, String)> = [
            ("/auth/v1/auth-request", "getAuthRequest"),
            ("/auth/v1/sign-in", "signIn"),
            ("/auth/v1/sign-up", "signUp"),
            ("/auth/v1/accept-invite", "acceptInvite"),
            ("/auth/v1/invite", "describeInvite"),
            ("/auth/v1/verify-email", "verifyEmail"),
            ("/auth/v1/resend-code", "resendCode"),
            ("/auth/v1/password-reset", "requestPasswordReset"),
            ("/auth/v1/password-reset/confirm", "confirmPasswordReset"),
            ("/auth/v1/logout", "logout"),
        ]
        .into_iter()
        .map(|(p, id)| ("POST".to_string(), p.to_string(), id.to_string()))
        .collect();
        want.extend(
            [
                ("GET", "/auth/v1/instance", "getInstance"),
                ("POST", "/auth/v1/app/sign-up", "appSignUp"),
                ("POST", "/auth/v1/app/sign-in", "appSignIn"),
                ("POST", "/auth/v1/app/refresh", "appRefresh"),
                ("POST", "/auth/v1/app/sign-out", "appSignOut"),
                ("GET", "/auth/v1/app/user", "appGetUser"),
                ("POST", "/auth/v1/app/verify-email", "appVerifyEmail"),
                ("POST", "/auth/v1/app/resend-code", "appResendCode"),
                ("POST", "/auth/v1/app/password-reset", "appRequestPasswordReset"),
                ("POST", "/auth/v1/app/password-reset/confirm", "appConfirmPasswordReset"),
                ("POST", "/auth/v1/app/change-email", "appChangeEmail"),
                ("POST", "/auth/v1/app/change-password", "appChangePassword"),
                ("GET", "/auth/v1/app/jwks", "appJwks"),
            ]
            .map(|(m, p, id)| (m.to_string(), p.to_string(), id.to_string())),
        );
        want.sort();
        assert_eq!(got, want);
        for (_, path, op) in operations() {
            let tag = if path.starts_with("/auth/v1/app/") { "app-users" } else { "login" };
            assert_eq!(op.tags.as_deref(), Some(&[tag.to_string()][..]), "{path}");
        }
    }

    #[test]
    fn every_response_has_a_description_and_errors_use_the_envelope() {
        let ops = operations();
        assert!(!ops.is_empty());
        for (m, p, op) in ops {
            for (status, resp) in &op.responses.responses {
                let resp = match resp {
                    utoipa::openapi::RefOr::T(r) => r,
                    utoipa::openapi::RefOr::Ref(_) => continue,
                };
                assert!(
                    !resp.description.trim().is_empty(),
                    "{m} {p} {status}: empty description"
                );
                if status.starts_with('4') || status.starts_with('5') {
                    let json = serde_json::to_value(resp).unwrap();
                    let r = json["content"]["application/json"]["schema"]["$ref"]
                        .as_str()
                        .unwrap_or_default();
                    assert_eq!(
                        r, "#/components/schemas/ErrorEnvelope",
                        "{m} {p} {status}: error must use ErrorEnvelope"
                    );
                }
            }
        }
    }

    #[test]
    fn schema_names_are_specific_and_unique() {
        let doc = serde_json::to_value(openapi()).unwrap();
        let names: Vec<&String> = doc["components"]["schemas"].as_object().unwrap().keys().collect();
        let set: BTreeSet<&&String> = names.iter().collect();
        assert_eq!(set.len(), names.len(), "repeated schema name");
        for n in names {
            assert!(
                !["Empty", "Body", "Object"].contains(&n.as_str()),
                "generic schema name {n}"
            );
        }
    }
}
