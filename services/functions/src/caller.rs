//! Who is calling, from the gateway's identity token, and whether a function's `auth` lets
//! them in (the auth matrix).
//!
//! | `auth`     | public key | secret key | public key + user token | none |
//! |------------|------------|------------|-------------------------|------|
//! | `key`      | yes        | yes        | yes                     | 401  |
//! | `app_user` | 401        | yes        | yes                     | 401  |
//! | `public`   | yes        | yes        | yes                     | yes (`/public/` form only) |
//!
//! The keyless `/public/` form reaches `public` functions only: any other is 404 there, as if
//! it didn't exist. The worker's internal invocations ([`Caller::Internal`]) pass every `auth`.

use chelabase_auth::internal::{AuthMethod, InternalClaims};
use chelabase_metadata::FunctionAuth;
use chelabase_types::data_key::DataKeyKind;
use serde_json::{Value, json};

#[derive(Debug, Clone, PartialEq)]
pub enum Caller {
    PublicKey {
        role: String,
    },
    SecretKey {
        role: String,
    },
    /// A public key with a verified app-user token (role `authenticated`).
    AppUser {
        role: String,
        user_id: Option<String>,
        claims: Value,
    },
    /// The `/public/` form: no credential is checked.
    Anonymous,
    /// The worker (cron, database and storage events).
    Internal,
}

/// Which URL form the call came by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Form {
    /// `/functions/v1/{name}`: the gateway checked a data key.
    Key,
    /// `/functions/v1/public/{branch_id}/{name}`: nothing checked.
    Public,
    /// `/internal/invoke/{branch_id}/{name}`: the worker's bearer.
    Internal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// 404 `function_not_found`.
    NotFound,
    /// 401 `unauthorized`.
    Unauthorized,
}

impl Caller {
    /// The caller an identity names: a data key's, with a user token's claims when it carries
    /// them (the gateway sets role `authenticated` and the claims only after verifying one).
    pub fn from_claims(claims: &InternalClaims) -> Option<Self> {
        let data = match (claims.auth, &claims.data) {
            (AuthMethod::DataKey, Some(data)) => data,
            _ => return None,
        };
        Some(match data.kind {
            DataKeyKind::Secret => Caller::SecretKey {
                role: data.role.clone(),
            },
            DataKeyKind::Public => match data.claims.get("sub") {
                Some(sub) => Caller::AppUser {
                    role: data.role.clone(),
                    user_id: sub.as_str().map(str::to_owned),
                    claims: data.claims.clone(),
                },
                None => Caller::PublicKey {
                    role: data.role.clone(),
                },
            },
        })
    }

    /// `chelabase-caller`: `{kind, role, user_id?, claims?}`.
    pub fn header(&self) -> String {
        let v = match self {
            Caller::PublicKey { role } => json!({"kind": "public_key", "role": role}),
            Caller::SecretKey { role } => json!({"kind": "secret_key", "role": role}),
            Caller::AppUser {
                role,
                user_id,
                claims,
            } => {
                json!({"kind": "app_user", "role": role, "user_id": user_id, "claims": claims})
            }
            Caller::Anonymous => json!({"kind": "anonymous", "role": null}),
            Caller::Internal => json!({"kind": "internal", "role": null}),
        };
        v.to_string()
    }

    /// The caller's key may be handed to the function (`chelabase-caller-key`): a public key
    /// only, never a secret one.
    pub fn may_forward_key(&self) -> bool {
        matches!(self, Caller::PublicKey { .. } | Caller::AppUser { .. })
    }

    pub fn is_app_user(&self) -> bool {
        matches!(self, Caller::AppUser { .. })
    }
}

/// Whether `caller`, by `form`, may call a function whose `auth` is `auth`.
pub fn check(auth: FunctionAuth, form: Form, caller: &Caller) -> Result<(), Refusal> {
    use Caller::*;
    match form {
        Form::Internal => Ok(()),
        Form::Public if auth == FunctionAuth::Public => Ok(()),
        Form::Public => Err(Refusal::NotFound),
        Form::Key => match (auth, caller) {
            (_, Anonymous | Internal) => Err(Refusal::Unauthorized),
            (FunctionAuth::AppUser, PublicKey { .. }) => Err(Refusal::Unauthorized),
            _ => Ok(()),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn callers() -> [(&'static str, Caller); 4] {
        [
            (
                "public key",
                Caller::PublicKey {
                    role: "anon".into(),
                },
            ),
            (
                "secret key",
                Caller::SecretKey {
                    role: "service".into(),
                },
            ),
            (
                "public key + user token",
                Caller::AppUser {
                    role: "authenticated".into(),
                    user_id: Some("u1".into()),
                    claims: json!({"sub": "u1"}),
                },
            ),
            ("none", Caller::Anonymous),
        ]
    }

    /// Every cell of the auth matrix, on both URL forms.
    #[test]
    fn auth_matrix() {
        use FunctionAuth::*;
        use Refusal::*;
        let ok = Ok(());
        // (auth, form, [public key, secret key, app user, none])
        type Row = (FunctionAuth, Form, [Result<(), Refusal>; 4]);
        let table: [Row; 6] = [
            (Key, Form::Key, [ok, ok, ok, Err(Unauthorized)]),
            (
                AppUser,
                Form::Key,
                [Err(Unauthorized), ok, ok, Err(Unauthorized)],
            ),
            (Public, Form::Key, [ok, ok, ok, Err(Unauthorized)]),
            (Key, Form::Public, [Err(NotFound); 4]),
            (AppUser, Form::Public, [Err(NotFound); 4]),
            (Public, Form::Public, [ok; 4]),
        ];
        for (auth, form, expected) in table {
            for ((what, caller), want) in callers().iter().zip(expected) {
                assert_eq!(
                    check(auth, form, caller),
                    want,
                    "{auth:?} by {form:?} with {what}"
                );
            }
        }
        for auth in [Key, AppUser, Public] {
            assert_eq!(check(auth, Form::Internal, &Caller::Internal), Ok(()));
        }
    }

    #[test]
    fn only_a_public_key_is_ever_forwarded() {
        let [(_, public), (_, secret), (_, user), (_, none)] = callers();
        assert!(public.may_forward_key());
        assert!(user.may_forward_key());
        assert!(!secret.may_forward_key());
        assert!(!none.may_forward_key());
        assert!(!Caller::Internal.may_forward_key());
    }

    #[test]
    fn caller_header_is_the_contract() {
        let [(_, public), (_, secret), (_, user), (_, none)] = callers();
        let parse = |c: &Caller| serde_json::from_str::<Value>(&c.header()).unwrap();
        assert_eq!(
            parse(&public),
            json!({"kind": "public_key", "role": "anon"})
        );
        assert_eq!(
            parse(&secret),
            json!({"kind": "secret_key", "role": "service"})
        );
        assert_eq!(
            parse(&user),
            json!({"kind": "app_user", "role": "authenticated", "user_id": "u1", "claims": {"sub": "u1"}})
        );
        assert_eq!(parse(&none), json!({"kind": "anonymous", "role": null}));
    }
}
