use chrono::{DateTime, Utc};
use serde::Deserialize;
use std::fmt;
use uuid::Uuid;

/// A user pool: the members of the platform, or the app users of one project.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Pool {
    Members,
    App(Uuid),
}

impl fmt::Display for Pool {
    /// The name the service's paths use: `members` or `app-<lowercase uuid>`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Pool::Members => f.write_str("members"),
            Pool::App(id) => write!(f, "app-{}", id.as_hyphenated()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct User {
    pub id: String,
    pub email: String,
    pub name: String,
    pub email_verified: bool,
    pub banned: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Default)]
pub struct NewUser<'a> {
    pub email: &'a str,
    /// `None` makes a user that cannot sign in with a password yet (an invitee).
    pub password: Option<&'a str>,
    pub name: Option<&'a str>,
    pub email_verified: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct UserPage {
    pub users: Vec<User>,
    pub total: u64,
    pub limit: u64,
    pub offset: u64,
}

/// A live session; the token is never part of it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct SessionView {
    pub id: String,
    pub user_id: String,
    pub expires_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct SessionInfo {
    pub session: SessionView,
    pub user: User,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct SignedIn {
    pub session_id: String,
    /// The bearer for `session_by_token`; shown once.
    pub session_token: String,
    pub user: User,
}
