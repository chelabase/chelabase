//! The identity service's routes (`services/identity/openapi.json`) as methods.

use crate::error::{IdentityError, classify_status};
use crate::ops::{self, Op};
use crate::types::{NewUser, Pool, SessionInfo, SignedIn, User, UserDetail, UserPage};
use chelabase_http::Method;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::path::Path;
use std::time::Duration;

/// A caller of the identity service. Cheap to clone.
///
/// The token is this caller's own (`auth`, `orchestrator` or `worker`): the service limits each
/// route to the callers its `x-callers` lists, and answers `Forbidden` to the rest.
#[derive(Clone)]
pub struct Identity {
    http: chelabase_http::Client,
    base: String,
    token: String,
}

impl std::fmt::Debug for Identity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Identity")
            .field("base", &self.base)
            .finish_non_exhaustive()
    }
}

#[derive(Deserialize)]
struct UserBody {
    user: User,
}

#[derive(Deserialize)]
struct UserDetailBody {
    user: UserDetail,
}

impl Identity {
    pub fn new(base: &str, token: &str) -> Self {
        Self {
            // Never a redirect (chelabase-http): the bearer must not follow one.
            http: chelabase_http::client_builder()
                .timeout(Duration::from_secs(10))
                .build()
                .expect("reqwest client"),
            base: base.trim_end_matches('/').to_string(),
            token: token.to_string(),
        }
    }

    /// The caller's token read from a file (trailing whitespace dropped).
    pub fn from_token_file(base: &str, token_file: &Path) -> Result<Self, IdentityError> {
        let token = std::fs::read_to_string(token_file)
            .map_err(|e| IdentityError::Config(format!("{}: {e}", token_file.display())))?;
        let token = token.trim();
        if token.is_empty() {
            return Err(IdentityError::Config(format!(
                "{} is empty",
                token_file.display()
            )));
        }
        Ok(Self::new(base, token))
    }

    /// From `CHELABASE_IDENTITY_URL` and `CHELABASE_IDENTITY_TOKEN_FILE`.
    pub fn from_env() -> Result<Self, IdentityError> {
        let var = |name: &str| {
            std::env::var(name)
                .ok()
                .filter(|v| !v.trim().is_empty())
                .ok_or_else(|| IdentityError::Config(format!("{name} is unset")))
        };
        Self::from_token_file(
            &var("CHELABASE_IDENTITY_URL")?,
            Path::new(&var("CHELABASE_IDENTITY_TOKEN_FILE")?),
        )
    }

    /// One call: the op's path with `{pool}` and `{id}` filled (percent-encoded), the answer's
    /// status and text. No answer is `Unavailable`; a refusal is its typed error.
    async fn send(
        &self,
        op: &Op,
        pool: Option<&Pool>,
        id: Option<&str>,
        query: &[(&str, String)],
        body: Option<Value>,
    ) -> Result<String, IdentityError> {
        let mut path = op.path.to_string();
        if let Some(pool) = pool {
            path = path.replace("{pool}", &pool.to_string());
        }
        if let Some(id) = id {
            path = path.replace("{id}", &urlencoding::encode(id));
        }
        let method = Method::from_bytes(op.method.as_bytes()).expect("a method name");
        let mut url = format!("{}{path}", self.base);
        for (n, (key, value)) in query.iter().enumerate() {
            url.push(if n == 0 { '?' } else { '&' });
            url.push_str(&format!(
                "{}={}",
                urlencoding::encode(key),
                urlencoding::encode(value)
            ));
        }
        let mut request = self.http.request(method, url).bearer_auth(&self.token);
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request
            .send()
            .await
            .map_err(|e| IdentityError::Unavailable(e.to_string()))?;
        let status = response.status().as_u16();
        let text = response
            .text()
            .await
            .map_err(|e| IdentityError::Unavailable(e.to_string()))?;
        match classify_status(status, &text) {
            Some(err) => Err(err),
            None => Ok(text),
        }
    }

    pub(crate) async fn call<T: DeserializeOwned>(
        &self,
        op: &Op,
        pool: Option<&Pool>,
        id: Option<&str>,
        query: &[(&str, String)],
        body: Option<Value>,
    ) -> Result<T, IdentityError> {
        let text = self.send(op, pool, id, query, body).await?;
        serde_json::from_str(&text).map_err(|e| {
            IdentityError::Unavailable(format!("unreadable answer from {}: {e}", op.operation_id))
        })
    }

    pub(crate) async fn call_empty(
        &self,
        op: &Op,
        pool: Option<&Pool>,
        id: Option<&str>,
        body: Option<Value>,
    ) -> Result<(), IdentityError> {
        self.send(op, pool, id, &[], body).await.map(|_| ())
    }

    // Pools (worker only).

    /// Creates the pool's schema and runs its migrations. Idempotent.
    pub async fn create_pool(&self, pool: &Pool) -> Result<(), IdentityError> {
        self.call_empty(&ops::CREATE_POOL, Some(pool), None, None)
            .await
    }

    /// Drops the pool and everything in it.
    pub async fn drop_pool(&self, pool: &Pool) -> Result<(), IdentityError> {
        self.call_empty(&ops::DROP_POOL, Some(pool), None, None)
            .await
    }

    // Users.

    pub async fn create_user(
        &self,
        pool: &Pool,
        user: &NewUser<'_>,
    ) -> Result<User, IdentityError> {
        let mut body = json!({ "email": user.email, "email_verified": user.email_verified });
        if let Some(password) = user.password {
            body["password"] = json!(password);
        }
        if let Some(name) = user.name {
            body["name"] = json!(name);
        }
        let out: UserBody = self
            .call(&ops::CREATE_USER, Some(pool), None, &[], Some(body))
            .await?;
        Ok(out.user)
    }

    /// Oldest first. `limit` is 1 to 200.
    pub async fn list_users(
        &self,
        pool: &Pool,
        limit: u32,
        offset: u32,
    ) -> Result<UserPage, IdentityError> {
        self.users_page(pool, None, limit, offset).await
    }

    /// The users whose email contains `search`, ignoring case (`%` and `_` match themselves),
    /// oldest first. `total` counts the matches. `search` is at most 320 characters.
    pub async fn search_users(
        &self,
        pool: &Pool,
        search: &str,
        limit: u32,
        offset: u32,
    ) -> Result<UserPage, IdentityError> {
        self.users_page(pool, Some(search), limit, offset).await
    }

    /// The user whose email is exactly `email`, ignoring case, or `None`. One indexed read on
    /// identity's unique email column, however many other addresses contain it. `email` is at
    /// most 320 characters.
    pub async fn find_user_by_email(
        &self,
        pool: &Pool,
        email: &str,
    ) -> Result<Option<User>, IdentityError> {
        let page = self
            .list_call(pool, vec![("email", email.to_string())])
            .await?;
        Ok(page.users.into_iter().next())
    }

    async fn users_page(
        &self,
        pool: &Pool,
        search: Option<&str>,
        limit: u32,
        offset: u32,
    ) -> Result<UserPage, IdentityError> {
        let mut query = vec![("limit", limit.to_string()), ("offset", offset.to_string())];
        if let Some(search) = search.filter(|s| !s.is_empty()) {
            query.push(("search", search.to_string()));
        }
        self.list_call(pool, query).await
    }

    async fn list_call(
        &self,
        pool: &Pool,
        query: Vec<(&str, String)>,
    ) -> Result<UserPage, IdentityError> {
        self.call(&ops::LIST_USERS, Some(pool), None, &query, None)
            .await
    }

    pub async fn get_user(&self, pool: &Pool, user_id: &str) -> Result<User, IdentityError> {
        Ok(self.user_detail(pool, user_id).await?.user)
    }

    /// The user with what only a single read tells: whether they have a password, and why
    /// they are banned.
    pub async fn user_detail(
        &self,
        pool: &Pool,
        user_id: &str,
    ) -> Result<UserDetail, IdentityError> {
        let out: UserDetailBody = self
            .call(&ops::GET_USER, Some(pool), Some(user_id), &[], None)
            .await?;
        Ok(out.user)
    }

    pub async fn delete_user(&self, pool: &Pool, user_id: &str) -> Result<(), IdentityError> {
        self.call_empty(&ops::DELETE_USER, Some(pool), Some(user_id), None)
            .await
    }

    /// `email_verified` is what the new address counts as; `false` unless the caller knows better.
    pub async fn set_email(
        &self,
        pool: &Pool,
        user_id: &str,
        email: &str,
        email_verified: bool,
    ) -> Result<User, IdentityError> {
        let body = json!({ "email": email, "email_verified": email_verified });
        let out: UserBody = self
            .call(&ops::SET_EMAIL, Some(pool), Some(user_id), &[], Some(body))
            .await?;
        Ok(out.user)
    }

    pub async fn set_password(
        &self,
        pool: &Pool,
        user_id: &str,
        password: &str,
    ) -> Result<(), IdentityError> {
        self.call_empty(
            &ops::SET_PASSWORD,
            Some(pool),
            Some(user_id),
            Some(json!({ "password": password })),
        )
        .await
    }

    pub async fn mark_email_verified(
        &self,
        pool: &Pool,
        user_id: &str,
    ) -> Result<User, IdentityError> {
        let out: UserBody = self
            .call(
                &ops::MARK_EMAIL_VERIFIED,
                Some(pool),
                Some(user_id),
                &[],
                None,
            )
            .await?;
        Ok(out.user)
    }

    /// Refuses sign-in and revokes every session of the user.
    pub async fn ban(
        &self,
        pool: &Pool,
        user_id: &str,
        reason: Option<&str>,
    ) -> Result<User, IdentityError> {
        let body = reason.map_or_else(|| json!({}), |r| json!({ "reason": r }));
        let out: UserBody = self
            .call(&ops::BAN_USER, Some(pool), Some(user_id), &[], Some(body))
            .await?;
        Ok(out.user)
    }

    pub async fn unban(&self, pool: &Pool, user_id: &str) -> Result<User, IdentityError> {
        let out: UserBody = self
            .call(&ops::UNBAN_USER, Some(pool), Some(user_id), &[], None)
            .await?;
        Ok(out.user)
    }

    pub async fn revoke_user_sessions(
        &self,
        pool: &Pool,
        user_id: &str,
    ) -> Result<(), IdentityError> {
        self.call_empty(&ops::REVOKE_USER_SESSIONS, Some(pool), Some(user_id), None)
            .await
    }

    // Sessions.

    /// Password sign-in: `BadCredentials` for a wrong email or password, `Banned` for a banned user.
    pub async fn sign_in(
        &self,
        pool: &Pool,
        email: &str,
        password: &str,
    ) -> Result<SignedIn, IdentityError> {
        let body = json!({ "email": email, "password": password });
        self.call(&ops::SIGN_IN, Some(pool), None, &[], Some(body))
            .await
    }

    pub async fn session_by_token(
        &self,
        pool: &Pool,
        token: &str,
    ) -> Result<SessionInfo, IdentityError> {
        self.call(
            &ops::SESSION_BY_TOKEN,
            Some(pool),
            None,
            &[],
            Some(json!({ "token": token })),
        )
        .await
    }

    pub async fn session_by_id(
        &self,
        pool: &Pool,
        session_id: &str,
    ) -> Result<SessionInfo, IdentityError> {
        self.call(&ops::SESSION_BY_ID, Some(pool), Some(session_id), &[], None)
            .await
    }

    pub async fn revoke_session(&self, pool: &Pool, session_id: &str) -> Result<(), IdentityError> {
        self.call_empty(&ops::REVOKE_SESSION, Some(pool), Some(session_id), None)
            .await
    }
}
