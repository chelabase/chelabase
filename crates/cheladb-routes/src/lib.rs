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
}

impl Service {
    /// Stable service name; also the `aud` of internal identity tokens.
    pub const fn name(self) -> &'static str {
        match self {
            Service::Orchestrator => "orchestrator",
            Service::Auth => "auth",
        }
    }
}

/// Which family of hostnames an endpoint is served on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Scope {
    Platform,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Auth {
    /// No credential required.
    Public,
    /// Zitadel-issued platform JWT (developers / operators).
    Platform,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cors {
    None,
    /// Callable from the Studio origins (`CHELADB_STUDIO_ORIGINS`).
    Studio,
    /// Callable from the Studio origins with cookies (credentials); never for a "*" setting.
    StudioCredentials,
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
        scope: Scope::Platform,
        service: Service::Orchestrator,
        policy: Policy {
            auth: Auth::Public,
            cors: Cors::None,
        },
    };

    pub const CREATE_DATABASE: Endpoint = Endpoint {
        name: "orchestrator.create_database",
        method: Method::Post,
        path: "/v1/databases",
        scope: Scope::Platform,
        service: Service::Orchestrator,
        policy: Policy {
            auth: Auth::Platform,
            cors: Cors::Studio,
        },
    };

    pub const GET_DATABASE: Endpoint = Endpoint {
        name: "orchestrator.get_database",
        method: Method::Get,
        path: "/v1/databases/{id}",
        scope: Scope::Platform,
        service: Service::Orchestrator,
        policy: Policy {
            auth: Auth::Platform,
            cors: Cors::Studio,
        },
    };

    /// Owner credentials of a ready database (re-revealable).
    pub const DATABASE_CONNECTION: Endpoint = Endpoint {
        name: "orchestrator.database_connection",
        method: Method::Get,
        path: "/v1/databases/{id}/connection",
        scope: Scope::Platform,
        service: Service::Orchestrator,
        policy: Policy {
            auth: Auth::Platform,
            cors: Cors::Studio,
        },
    };

    pub const RESET_PASSWORD: Endpoint = Endpoint {
        name: "orchestrator.reset_password",
        method: Method::Post,
        path: "/v1/databases/{id}/reset-password",
        scope: Scope::Platform,
        service: Service::Orchestrator,
        policy: Policy {
            auth: Auth::Platform,
            cors: Cors::Studio,
        },
    };

    pub const DELETE_DATABASE: Endpoint = Endpoint {
        name: "orchestrator.delete_database",
        method: Method::Delete,
        path: "/v1/databases/{id}",
        scope: Scope::Platform,
        service: Service::Orchestrator,
        policy: Policy {
            auth: Auth::Platform,
            cors: Cors::Studio,
        },
    };

    /// The caller, their tenant, projects and databases (Studio landing page).
    pub const ME: Endpoint = Endpoint {
        name: "orchestrator.me",
        method: Method::Get,
        path: "/v1/me",
        scope: Scope::Platform,
        service: Service::Orchestrator,
        policy: Policy {
            auth: Auth::Platform,
            cors: Cors::Studio,
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
            scope: Scope::Platform,
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
    orchestrator::CREATE_DATABASE,
    orchestrator::GET_DATABASE,
    orchestrator::ME,
    orchestrator::DATABASE_CONNECTION,
    orchestrator::RESET_PASSWORD,
    orchestrator::DELETE_DATABASE,
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
    fn literal_path_is_found() {
        assert_eq!(
            match_endpoint(Scope::Platform, "POST", "/v1/databases"),
            Match::Found(&orchestrator::CREATE_DATABASE)
        );
    }

    #[test]
    fn param_segment_matches_any_non_empty_value() {
        assert_eq!(
            match_endpoint(
                Scope::Platform,
                "GET",
                "/v1/databases/6c0f1c2e-2f7e-4c52-9a51-0d8f2f1d7c11"
            ),
            Match::Found(&orchestrator::GET_DATABASE)
        );
    }

    #[test]
    fn known_path_with_wrong_method_lists_allowed_methods() {
        assert_eq!(
            match_endpoint(Scope::Platform, "DELETE", "/v1/databases"),
            Match::MethodNotAllowed(vec![Method::Post])
        );
    }

    #[test]
    fn undeclarable_method_token_on_known_path_is_method_not_allowed() {
        assert_eq!(
            match_endpoint(Scope::Platform, "OPTIONS", "/v1/databases"),
            Match::MethodNotAllowed(vec![Method::Post])
        );
    }

    #[test]
    fn unknown_path_is_not_found() {
        assert_eq!(
            match_endpoint(Scope::Platform, "GET", "/v1/other"),
            Match::NotFound
        );
    }

    #[test]
    fn trailing_slash_is_not_found() {
        assert_eq!(
            match_endpoint(Scope::Platform, "POST", "/v1/databases/"),
            Match::NotFound
        );
    }

    #[test]
    fn empty_param_segment_is_not_found() {
        assert_eq!(
            match_endpoint(Scope::Platform, "GET", "/v1/databases//"),
            Match::NotFound
        );
    }

    #[test]
    fn prefix_of_a_path_is_not_found() {
        assert_eq!(match_endpoint(Scope::Platform, "GET", "/v1"), Match::NotFound);
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
        match match_in(LITERAL_VS_PARAM, Scope::Platform, "GET", "/things/search") {
            Match::Found(ep) => assert_eq!(ep.name, "test.search"),
            other => panic!("expected Found, got {other:?}"),
        }
        match match_in(LITERAL_VS_PARAM, Scope::Platform, "GET", "/things/42") {
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

    #[test]
    fn auth_endpoints_are_public_posts_with_credentialed_cors() {
        for ep in ENDPOINTS.iter().filter(|e| e.service == Service::Auth) {
            assert_eq!(ep.method, Method::Post, "{}", ep.path);
            assert_eq!(ep.policy.auth, Auth::Public, "{}", ep.path);
            assert_eq!(ep.policy.cors, Cors::StudioCredentials, "{}", ep.path);
            assert!(ep.path.starts_with("/auth/v1/"), "{}", ep.path);
        }
        assert_eq!(ENDPOINTS.iter().filter(|e| e.service == Service::Auth).count(), 8);
        assert!(matches!(match_endpoint(Scope::Platform, "POST", "/auth/v1/password-reset/confirm"),
            Match::Found(ep) if ep.name == "auth.password_reset_confirm"));
    }
}
