//! Public HTTP endpoints of Cheladb services.
//!
//! One declaration per endpoint, shared by the gateway (routing + policy) and
//! the owning service (axum route paths), so the two cannot drift. Path
//! templates use axum's `{param}` syntax.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Method {
    Get,
    Post,
    Put,
    Patch,
    Delete,
}

impl Method {
    pub const fn as_str(self) -> &'static str {
        match self {
            Method::Get => "GET",
            Method::Post => "POST",
            Method::Put => "PUT",
            Method::Patch => "PATCH",
            Method::Delete => "DELETE",
        }
    }

    /// Parse an HTTP method token. Tokens no endpoint can declare (`OPTIONS`,
    /// `HEAD`, lowercase variants, …) return `None` and never match.
    pub fn parse(token: &str) -> Option<Method> {
        match token {
            "GET" => Some(Method::Get),
            "POST" => Some(Method::Post),
            "PUT" => Some(Method::Put),
            "PATCH" => Some(Method::Patch),
            "DELETE" => Some(Method::Delete),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Service {
    Orchestrator,
    Auth,
    Rest,
}

impl Service {
    /// Stable service name; also the `aud` of internal identity tokens.
    pub const fn name(self) -> &'static str {
        match self {
            Service::Orchestrator => "orchestrator",
            Service::Auth => "auth",
            Service::Rest => "rest",
        }
    }
}

/// Which family of hostnames an endpoint is served on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Scope {
    Management,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Auth {
    /// No credential required.
    Public,
    /// Zitadel-issued management token (developers / operators).
    Management,
    /// A data API key of either kind (`chk_pub_` or `chk_sec_`).
    DataKey,
    /// Only a secret data API key.
    SecretDataKey,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cors {
    None,
    /// Callable from the Studio origins (`CHELADB_STUDIO_ORIGINS`).
    Studio,
    /// Callable from the Studio origins with cookies (credentials); never for a "*" setting.
    StudioCredentials,
    /// Any origin: `Access-Control-Allow-Origin: *`, never with credentials.
    Any,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Policy {
    pub auth: Auth,
    pub cors: Cors,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Endpoint {
    /// Stable name used in logs.
    pub name: &'static str,
    pub method: Method,
    pub path: &'static str,
    pub scope: Scope,
    pub service: Service,
    pub policy: Policy,
}

pub mod orchestrator {
    use super::{Auth, Cors, Endpoint, Method, Policy, Scope, Service};

    pub const HEALTH: Endpoint = Endpoint {
        name: "orchestrator.health",
        method: Method::Get,
        path: "/health",
        scope: Scope::Management,
        service: Service::Orchestrator,
        policy: Policy {
            auth: Auth::Public,
            cors: Cors::None,
        },
    };

    /// Interactive API docs (Scalar). Not part of the OpenAPI document.
    pub const API_DOCS: Endpoint = public_get("orchestrator.api_docs", "/v1/docs");

    /// The embedded Scalar bundle the docs page loads (self-hosted, no CDN).
    pub const API_DOCS_SCRIPT: Endpoint = public_get("orchestrator.api_docs_script", "/v1/docs/scalar.js");

    /// The Management API's OpenAPI document.
    pub const OPENAPI_JSON: Endpoint = public_get("orchestrator.openapi_json", "/v1/openapi.json");

    const fn public_get(name: &'static str, path: &'static str) -> Endpoint {
        Endpoint {
            name,
            method: Method::Get,
            path,
            scope: Scope::Management,
            service: Service::Orchestrator,
            policy: Policy {
                auth: Auth::Public,
                cors: Cors::None,
            },
        }
    }

    pub const CREATE_DATABASE: Endpoint = Endpoint {
        name: "orchestrator.create_database",
        method: Method::Post,
        path: "/v1/databases",
        scope: Scope::Management,
        service: Service::Orchestrator,
        policy: Policy {
            auth: Auth::Management,
            cors: Cors::Studio,
        },
    };

    pub const GET_DATABASE: Endpoint = Endpoint {
        name: "orchestrator.get_database",
        method: Method::Get,
        path: "/v1/databases/{id}",
        scope: Scope::Management,
        service: Service::Orchestrator,
        policy: Policy {
            auth: Auth::Management,
            cors: Cors::Studio,
        },
    };

    /// Owner credentials of a ready database (re-revealable).
    pub const DATABASE_CONNECTION: Endpoint = Endpoint {
        name: "orchestrator.database_connection",
        method: Method::Get,
        path: "/v1/databases/{id}/connection",
        scope: Scope::Management,
        service: Service::Orchestrator,
        policy: Policy {
            auth: Auth::Management,
            cors: Cors::Studio,
        },
    };

    pub const RESET_PASSWORD: Endpoint = Endpoint {
        name: "orchestrator.reset_password",
        method: Method::Post,
        path: "/v1/databases/{id}/reset-password",
        scope: Scope::Management,
        service: Service::Orchestrator,
        policy: Policy {
            auth: Auth::Management,
            cors: Cors::Studio,
        },
    };

    pub const DELETE_DATABASE: Endpoint = Endpoint {
        name: "orchestrator.delete_database",
        method: Method::Delete,
        path: "/v1/databases/{id}",
        scope: Scope::Management,
        service: Service::Orchestrator,
        policy: Policy {
            auth: Auth::Management,
            cors: Cors::Studio,
        },
    };

    /// The database's branches (a flat list; `parent_branch_id` makes the tree).
    pub const LIST_BRANCHES: Endpoint = Endpoint {
        name: "orchestrator.list_branches",
        method: Method::Get,
        path: "/v1/databases/{id}/branches",
        scope: Scope::Management,
        service: Service::Orchestrator,
        policy: Policy {
            auth: Auth::Management,
            cors: Cors::Studio,
        },
    };

    /// A branch of an existing branch, at a point in its history.
    pub const CREATE_BRANCH: Endpoint = Endpoint {
        name: "orchestrator.create_branch",
        method: Method::Post,
        path: "/v1/databases/{id}/branches",
        scope: Scope::Management,
        service: Service::Orchestrator,
        policy: Policy {
            auth: Auth::Management,
            cors: Cors::Studio,
        },
    };

    /// Owner credentials of a ready branch.
    pub const BRANCH_CONNECTION: Endpoint = Endpoint {
        name: "orchestrator.branch_connection",
        method: Method::Get,
        path: "/v1/databases/{id}/branches/{branch_id}/connection",
        scope: Scope::Management,
        service: Service::Orchestrator,
        policy: Policy {
            auth: Auth::Management,
            cors: Cors::Studio,
        },
    };

    pub const BRANCH_RESET_PASSWORD: Endpoint = Endpoint {
        name: "orchestrator.branch_reset_password",
        method: Method::Post,
        path: "/v1/databases/{id}/branches/{branch_id}/reset-password",
        scope: Scope::Management,
        service: Service::Orchestrator,
        policy: Policy {
            auth: Auth::Management,
            cors: Cors::Studio,
        },
    };

    pub const BRANCH_RESET_TO_PARENT: Endpoint = Endpoint {
        name: "orchestrator.branch_reset_to_parent",
        method: Method::Post,
        path: "/v1/databases/{id}/branches/{branch_id}/reset-to-parent",
        scope: Scope::Management,
        service: Service::Orchestrator,
        policy: Policy {
            auth: Auth::Management,
            cors: Cors::Studio,
        },
    };

    /// Rewinds a branch to a point, keeping the old timeline as a backup branch.
    pub const BRANCH_RESTORE: Endpoint = Endpoint {
        name: "orchestrator.branch_restore",
        method: Method::Post,
        path: "/v1/databases/{id}/branches/{branch_id}/restore",
        scope: Scope::Management,
        service: Service::Orchestrator,
        policy: Policy {
            auth: Auth::Management,
            cors: Cors::Studio,
        },
    };

    pub const DELETE_BRANCH: Endpoint = Endpoint {
        name: "orchestrator.delete_branch",
        method: Method::Delete,
        path: "/v1/databases/{id}/branches/{branch_id}",
        scope: Scope::Management,
        service: Service::Orchestrator,
        policy: Policy {
            auth: Auth::Management,
            cors: Cors::Studio,
        },
    };

    /// Database settings: the history window and the branch cap.
    pub const UPDATE_DATABASE: Endpoint = Endpoint {
        name: "orchestrator.update_database",
        method: Method::Patch,
        path: "/v1/databases/{id}",
        scope: Scope::Management,
        service: Service::Orchestrator,
        policy: Policy {
            auth: Auth::Management,
            cors: Cors::Studio,
        },
    };

    /// The caller, their tenant, projects and databases (Studio landing page).
    pub const ME: Endpoint = Endpoint {
        name: "orchestrator.me",
        method: Method::Get,
        path: "/v1/me",
        scope: Scope::Management,
        service: Service::Orchestrator,
        policy: Policy {
            auth: Auth::Management,
            cors: Cors::Studio,
        },
    };

    /// The branch's live data API keys (the public key in full).
    pub const LIST_KEYS: Endpoint = key_endpoint(
        "orchestrator.list_keys",
        Method::Get,
        "/v1/databases/{id}/branches/{branch_id}/keys",
    );

    /// A new secret key, shown once.
    pub const CREATE_KEY: Endpoint = key_endpoint(
        "orchestrator.create_key",
        Method::Post,
        "/v1/databases/{id}/branches/{branch_id}/keys",
    );

    pub const ROTATE_KEY: Endpoint = key_endpoint(
        "orchestrator.rotate_key",
        Method::Post,
        "/v1/databases/{id}/branches/{branch_id}/keys/{key_id}/rotate",
    );

    pub const REVOKE_KEY: Endpoint = key_endpoint(
        "orchestrator.revoke_key",
        Method::Delete,
        "/v1/databases/{id}/branches/{branch_id}/keys/{key_id}",
    );

    /// The branch's schema as the data API sees it, with each table's role privileges.
    pub const DATA_API_SCHEMA: Endpoint = key_endpoint(
        "orchestrator.data_api_schema",
        Method::Get,
        "/v1/databases/{id}/branches/{branch_id}/data-api/schema",
    );

    const fn key_endpoint(name: &'static str, method: Method, path: &'static str) -> Endpoint {
        Endpoint {
            name,
            method,
            path,
            scope: Scope::Management,
            service: Service::Orchestrator,
            policy: Policy {
                auth: Auth::Management,
                cors: Cors::Studio,
            },
        }
    }
}

/// The data API, served by `rest`.
pub mod rest {
    use super::{Auth, Cors, Endpoint, Method, Policy, Scope, Service};

    pub const QUERY: Endpoint = Endpoint {
        name: "rest.query",
        method: Method::Post,
        path: "/data/v1/query",
        scope: Scope::Management,
        service: Service::Rest,
        policy: Policy {
            auth: Auth::DataKey,
            cors: Cors::Any,
        },
    };

    pub const SCHEMA: Endpoint = Endpoint {
        name: "rest.schema",
        method: Method::Get,
        path: "/data/v1/schema",
        scope: Scope::Management,
        service: Service::Rest,
        policy: Policy {
            auth: Auth::SecretDataKey,
            cors: Cors::Any,
        },
    };
}

/// Studio's sign-in pages, served by `auth`.
pub mod auth {
    use super::{Auth, Cors, Endpoint, Method, Policy, Scope, Service};

    const POLICY: Policy = Policy {
        auth: Auth::Public,
        cors: Cors::StudioCredentials,
    };

    const fn post(name: &'static str, path: &'static str) -> Endpoint {
        Endpoint {
            name,
            method: Method::Post,
            path,
            scope: Scope::Management,
            service: Service::Auth,
            policy: POLICY,
        }
    }

    pub const AUTH_REQUEST: Endpoint = post("auth.auth_request", "/auth/v1/auth-request");
    pub const SIGN_IN: Endpoint = post("auth.sign_in", "/auth/v1/sign-in");
    pub const SIGN_UP: Endpoint = post("auth.sign_up", "/auth/v1/sign-up");
    pub const VERIFY_EMAIL: Endpoint = post("auth.verify_email", "/auth/v1/verify-email");
    pub const RESEND_CODE: Endpoint = post("auth.resend_code", "/auth/v1/resend-code");
    pub const PASSWORD_RESET: Endpoint = post("auth.password_reset", "/auth/v1/password-reset");
    pub const PASSWORD_RESET_CONFIRM: Endpoint =
        post("auth.password_reset_confirm", "/auth/v1/password-reset/confirm");
    pub const LOGOUT: Endpoint = post("auth.logout", "/auth/v1/logout");
}

/// Every public endpoint the gateway exposes.
pub const ENDPOINTS: &[Endpoint] = &[
    orchestrator::HEALTH,
    orchestrator::API_DOCS,
    orchestrator::API_DOCS_SCRIPT,
    orchestrator::OPENAPI_JSON,
    orchestrator::CREATE_DATABASE,
    orchestrator::GET_DATABASE,
    orchestrator::ME,
    orchestrator::DATABASE_CONNECTION,
    orchestrator::RESET_PASSWORD,
    orchestrator::DELETE_DATABASE,
    orchestrator::UPDATE_DATABASE,
    orchestrator::LIST_BRANCHES,
    orchestrator::CREATE_BRANCH,
    orchestrator::BRANCH_CONNECTION,
    orchestrator::BRANCH_RESET_PASSWORD,
    orchestrator::BRANCH_RESET_TO_PARENT,
    orchestrator::BRANCH_RESTORE,
    orchestrator::DELETE_BRANCH,
    orchestrator::LIST_KEYS,
    orchestrator::CREATE_KEY,
    orchestrator::ROTATE_KEY,
    orchestrator::REVOKE_KEY,
    orchestrator::DATA_API_SCHEMA,
    rest::QUERY,
    rest::SCHEMA,
    auth::AUTH_REQUEST,
    auth::SIGN_IN,
    auth::SIGN_UP,
    auth::VERIFY_EMAIL,
    auth::RESEND_CODE,
    auth::PASSWORD_RESET,
    auth::PASSWORD_RESET_CONFIRM,
    auth::LOGOUT,
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Match {
    Found(&'static Endpoint),
    /// The path exists in this scope, but not for the requested method.
    MethodNotAllowed(Vec<Method>),
    NotFound,
}

/// Find the endpoint for `method` + `path` (no query string) within `scope`.
pub fn match_endpoint(scope: Scope, method: &str, path: &str) -> Match {
    match_in(ENDPOINTS, scope, method, path)
}

fn match_in(endpoints: &'static [Endpoint], scope: Scope, method: &str, path: &str) -> Match {
    let method = Method::parse(method);
    let mut best: Option<(&'static Endpoint, Vec<bool>)> = None;
    let mut allowed = Vec::new();

    for ep in endpoints.iter().filter(|ep| ep.scope == scope) {
        let Some(specificity) = path_specificity(ep.path, path) else {
            continue;
        };
        if Some(ep.method) == method {
            let better = best
                .as_ref()
                .is_none_or(|(_, current)| specificity > *current);
            if better {
                best = Some((ep, specificity));
            }
        } else if !allowed.contains(&ep.method) {
            allowed.push(ep.method);
        }
    }

    match best {
        Some((ep, _)) => Match::Found(ep),
        None if !allowed.is_empty() => Match::MethodNotAllowed(allowed),
        None => Match::NotFound,
    }
}

/// If `template` matches `path`, return one flag per segment (`true` = literal
/// segment), so literal matches compare greater than param matches.
fn path_specificity(template: &str, path: &str) -> Option<Vec<bool>> {
    let template = template.strip_prefix('/')?;
    let path = path.strip_prefix('/')?;
    let template_segments: Vec<&str> = template.split('/').collect();
    let path_segments: Vec<&str> = path.split('/').collect();
    if template_segments.len() != path_segments.len() {
        return None;
    }
    template_segments
        .iter()
        .zip(&path_segments)
        .map(|(t, p)| {
            if t.starts_with('{') && t.ends_with('}') {
                (!p.is_empty()).then_some(false)
            } else {
                (t == p).then_some(true)
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn docs_routes_are_public() {
        for (path, name) in [
            ("/v1/docs", "orchestrator.api_docs"),
            ("/v1/docs/scalar.js", "orchestrator.api_docs_script"),
            ("/v1/openapi.json", "orchestrator.openapi_json"),
        ] {
            match match_endpoint(Scope::Management, "GET", path) {
                Match::Found(ep) => {
                    assert_eq!(ep.name, name);
                    assert_eq!(ep.policy.auth, Auth::Public, "{path}");
                    assert_eq!(ep.service, Service::Orchestrator);
                }
                other => panic!("{path}: expected Found, got {other:?}"),
            }
        }
    }

    #[test]
    fn literal_path_is_found() {
        assert_eq!(
            match_endpoint(Scope::Management, "POST", "/v1/databases"),
            Match::Found(&orchestrator::CREATE_DATABASE)
        );
    }

    #[test]
    fn param_segment_matches_any_non_empty_value() {
        assert_eq!(
            match_endpoint(
                Scope::Management,
                "GET",
                "/v1/databases/6c0f1c2e-2f7e-4c52-9a51-0d8f2f1d7c11"
            ),
            Match::Found(&orchestrator::GET_DATABASE)
        );
    }

    #[test]
    fn known_path_with_wrong_method_lists_allowed_methods() {
        assert_eq!(
            match_endpoint(Scope::Management, "DELETE", "/v1/databases"),
            Match::MethodNotAllowed(vec![Method::Post])
        );
    }

    #[test]
    fn undeclarable_method_token_on_known_path_is_method_not_allowed() {
        assert_eq!(
            match_endpoint(Scope::Management, "OPTIONS", "/v1/databases"),
            Match::MethodNotAllowed(vec![Method::Post])
        );
    }

    #[test]
    fn unknown_path_is_not_found() {
        assert_eq!(
            match_endpoint(Scope::Management, "GET", "/v1/other"),
            Match::NotFound
        );
    }

    #[test]
    fn trailing_slash_is_not_found() {
        assert_eq!(
            match_endpoint(Scope::Management, "POST", "/v1/databases/"),
            Match::NotFound
        );
    }

    #[test]
    fn empty_param_segment_is_not_found() {
        assert_eq!(
            match_endpoint(Scope::Management, "GET", "/v1/databases//"),
            Match::NotFound
        );
    }

    #[test]
    fn prefix_of_a_path_is_not_found() {
        assert_eq!(match_endpoint(Scope::Management, "GET", "/v1"), Match::NotFound);
    }

    const LITERAL_VS_PARAM: &[Endpoint] = &[
        Endpoint {
            name: "test.by_id",
            method: Method::Get,
            path: "/things/{id}",
            ..orchestrator::HEALTH
        },
        Endpoint {
            name: "test.search",
            method: Method::Get,
            path: "/things/search",
            ..orchestrator::HEALTH
        },
    ];

    #[test]
    fn literal_segment_beats_param_segment() {
        match match_in(LITERAL_VS_PARAM, Scope::Management, "GET", "/things/search") {
            Match::Found(ep) => assert_eq!(ep.name, "test.search"),
            other => panic!("expected Found, got {other:?}"),
        }
        match match_in(LITERAL_VS_PARAM, Scope::Management, "GET", "/things/42") {
            Match::Found(ep) => assert_eq!(ep.name, "test.by_id"),
            other => panic!("expected Found, got {other:?}"),
        }
    }

    #[test]
    fn declared_endpoints_are_unique_and_well_formed() {
        let mut seen = std::collections::HashSet::new();
        for ep in ENDPOINTS {
            assert!(
                seen.insert((ep.scope, ep.method, ep.path)),
                "duplicate endpoint {} {}",
                ep.method.as_str(),
                ep.path
            );
            assert!(ep.path.starts_with('/'), "{} must start with /", ep.path);
            assert!(
                ep.path == "/" || !ep.path.ends_with('/'),
                "{} must not end with /",
                ep.path
            );
            for segment in ep.path.split('/').skip(1) {
                assert!(!segment.is_empty(), "{} has an empty segment", ep.path);
                let opens = segment.starts_with('{');
                let closes = segment.ends_with('}');
                assert_eq!(opens, closes, "{} has a malformed param segment", ep.path);
            }
        }
    }

    const BRANCHES: &str = "/v1/databases/6c0f1c2e-2f7e-4c52-9a51-0d8f2f1d7c11/branches";
    const BRANCH: &str =
        "/v1/databases/6c0f1c2e-2f7e-4c52-9a51-0d8f2f1d7c11/branches/0d8f2f1d-7c11-4c52-9a51-6c0f1c2e2f7e";

    fn found(method: &str, path: &str) -> &'static str {
        match match_endpoint(Scope::Management, method, path) {
            Match::Found(ep) => ep.name,
            other => panic!("{method} {path}: expected Found, got {other:?}"),
        }
    }

    #[test]
    fn branch_endpoints_route_with_two_path_params() {
        assert_eq!(found("GET", BRANCHES), "orchestrator.list_branches");
        assert_eq!(found("POST", BRANCHES), "orchestrator.create_branch");
        assert_eq!(found("DELETE", BRANCH), "orchestrator.delete_branch");
        for (method, suffix, name) in [
            ("GET", "connection", "orchestrator.branch_connection"),
            ("POST", "reset-password", "orchestrator.branch_reset_password"),
            ("POST", "reset-to-parent", "orchestrator.branch_reset_to_parent"),
            ("POST", "restore", "orchestrator.branch_restore"),
        ] {
            assert_eq!(found(method, &format!("{BRANCH}/{suffix}")), name);
        }
    }

    #[test]
    fn patch_database_routes_and_is_distinct_from_get_and_delete() {
        let db = "/v1/databases/6c0f1c2e-2f7e-4c52-9a51-0d8f2f1d7c11";
        assert_eq!(found("PATCH", db), "orchestrator.update_database");
        assert_eq!(found("GET", db), "orchestrator.get_database");
        assert_eq!(found("DELETE", db), "orchestrator.delete_database");
        assert_eq!(
            match_endpoint(Scope::Management, "PUT", db),
            Match::MethodNotAllowed(vec![Method::Get, Method::Delete, Method::Patch])
        );
    }

    #[test]
    fn branch_paths_reject_wrong_methods_and_extra_segments() {
        assert_eq!(
            match_endpoint(Scope::Management, "PUT", BRANCH),
            Match::MethodNotAllowed(vec![Method::Delete])
        );
        assert_eq!(
            match_endpoint(Scope::Management, "GET", &format!("{BRANCH}/restore")),
            Match::MethodNotAllowed(vec![Method::Post])
        );
        assert_eq!(
            match_endpoint(Scope::Management, "GET", &format!("{BRANCH}/nope")),
            Match::NotFound
        );
    }

    #[test]
    fn branch_endpoints_are_management_auth_with_studio_cors() {
        for name in [
            "list_branches",
            "create_branch",
            "branch_connection",
            "branch_reset_password",
            "branch_reset_to_parent",
            "branch_restore",
            "delete_branch",
            "update_database",
        ] {
            let ep = ENDPOINTS
                .iter()
                .find(|e| e.name == format!("orchestrator.{name}"))
                .unwrap_or_else(|| panic!("{name} is not in ENDPOINTS"));
            assert_eq!(ep.policy, Policy { auth: Auth::Management, cors: Cors::Studio }, "{name}");
            assert_eq!(ep.service, Service::Orchestrator, "{name}");
        }
    }

    #[test]
    fn auth_endpoints_are_public_posts_with_credentialed_cors() {
        for ep in ENDPOINTS.iter().filter(|e| e.service == Service::Auth) {
            assert_eq!(ep.method, Method::Post, "{}", ep.path);
            assert_eq!(ep.policy.auth, Auth::Public, "{}", ep.path);
            assert_eq!(ep.policy.cors, Cors::StudioCredentials, "{}", ep.path);
            assert!(ep.path.starts_with("/auth/v1/"), "{}", ep.path);
        }
        assert_eq!(ENDPOINTS.iter().filter(|e| e.service == Service::Auth).count(), 8);
        assert!(matches!(match_endpoint(Scope::Management, "POST", "/auth/v1/password-reset/confirm"),
            Match::Found(ep) if ep.name == "auth.password_reset_confirm"));
    }

    const KEYS: &str = "/v1/databases/6c0f1c2e-2f7e-4c52-9a51-0d8f2f1d7c11/branches/0d8f2f1d-7c11-4c52-9a51-6c0f1c2e2f7e/keys";

    #[test]
    fn rest_endpoints_match_and_are_listed() {
        assert_eq!(found("POST", "/data/v1/query"), "rest.query");
        assert_eq!(found("GET", "/data/v1/schema"), "rest.schema");
        assert_eq!(rest::QUERY.policy, Policy { auth: Auth::DataKey, cors: Cors::Any });
        assert_eq!(rest::SCHEMA.policy, Policy { auth: Auth::SecretDataKey, cors: Cors::Any });
        for ep in [rest::QUERY, rest::SCHEMA] {
            assert_eq!(ep.service, Service::Rest);
            assert_eq!(ep.scope, Scope::Management);
            assert!(ENDPOINTS.contains(&ep));
        }
        assert_eq!(found("GET", KEYS), "orchestrator.list_keys");
        assert_eq!(found("POST", KEYS), "orchestrator.create_key");
        assert_eq!(found("DELETE", &format!("{KEYS}/abc")), "orchestrator.revoke_key");
        assert_eq!(found("POST", &format!("{KEYS}/abc/rotate")), "orchestrator.rotate_key");
        assert_eq!(
            found("GET", "/v1/databases/6c0f1c2e-2f7e-4c52-9a51-0d8f2f1d7c11/branches/0d8f2f1d-7c11-4c52-9a51-6c0f1c2e2f7e/data-api/schema"),
            "orchestrator.data_api_schema"
        );
        for name in ["list_keys", "create_key", "rotate_key", "revoke_key", "data_api_schema"] {
            let ep = ENDPOINTS
                .iter()
                .find(|e| e.name == format!("orchestrator.{name}"))
                .unwrap_or_else(|| panic!("{name} is not in ENDPOINTS"));
            assert_eq!(ep.policy, Policy { auth: Auth::Management, cors: Cors::Studio }, "{name}");
        }
    }

    #[test]
    fn literal_keys_rotate_beats_param() {
        let id = "6c0f1c2e-2f7e-4c52-9a51-0d8f2f1d7c11";
        assert_eq!(found("POST", &format!("{KEYS}/{id}/rotate")), "orchestrator.rotate_key");
        assert_eq!(found("DELETE", &format!("{KEYS}/{id}")), "orchestrator.revoke_key");
        // `rotate` in the key-id position is a key id, not the rotate action.
        assert_eq!(
            match_endpoint(Scope::Management, "POST", &format!("{KEYS}/rotate")),
            Match::MethodNotAllowed(vec![Method::Delete])
        );
        assert_eq!(
            match_endpoint(Scope::Management, "GET", &format!("{KEYS}/{id}/rotate")),
            Match::MethodNotAllowed(vec![Method::Post])
        );
    }

    #[test]
    fn service_rest_name_is_rest() {
        assert_eq!(Service::Rest.name(), "rest");
    }
}
