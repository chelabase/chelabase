//! The encrypted cookies: step state between requests (`chelabase_login`) and the finished
//! sign-in's Zitadel session, kept only to end it at logout (`chelabase_session`).

use chelabase_secrets::{SecretKey, open, seal};
use http::HeaderMap;
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

pub const LOGIN_COOKIE: &str = "chelabase_login";
pub const SESSION_COOKIE: &str = "chelabase_session";
pub const LOGIN_TTL_SECS: u64 = 900;
pub const SESSION_TTL_SECS: u64 = 30 * 24 * 3600;
const LOGIN_AAD: &str = "chelabase:auth:login";
const SESSION_AAD: &str = "chelabase:auth:session";
/// Plaintext is padded to this size so a decoy cookie looks like a real one.
const LOGIN_PLAINTEXT_LEN: usize = 640;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Step {
    Verify,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoginState {
    pub auth_request: String,
    pub user_id: Option<String>,
    pub session_id: Option<String>,
    pub session_token: Option<String>,
    pub step: Step,
    pub decoy: bool,
    pub exp: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionState {
    pub session_id: String,
    pub session_token: String,
    pub exp: u64,
}

pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn seal_login(key: &SecretKey, state: &LoginState) -> String {
    let mut text = serde_json::to_string(state).expect("serializable");
    // JSON ignores trailing whitespace, so padding with spaces keeps it parseable.
    let target = LOGIN_PLAINTEXT_LEN.max(text.len().div_ceil(64) * 64);
    text.extend(std::iter::repeat_n(' ', target - text.len()));
    seal(key, LOGIN_AAD, &text)
}

pub fn open_login(key: &SecretKey, sealed: &str, now: u64) -> Option<LoginState> {
    let state: LoginState =
        serde_json::from_str(open(key, LOGIN_AAD, sealed).ok()?.trim_end()).ok()?;
    (state.exp >= now).then_some(state)
}

pub fn seal_session(key: &SecretKey, state: &SessionState) -> String {
    seal(
        key,
        SESSION_AAD,
        &serde_json::to_string(state).expect("serializable"),
    )
}

pub fn open_session(key: &SecretKey, sealed: &str, now: u64) -> Option<SessionState> {
    let state: SessionState = serde_json::from_str(&open(key, SESSION_AAD, sealed).ok()?).ok()?;
    (state.exp >= now).then_some(state)
}

const TICKET_AAD: &str = "chelabase:auth:accept-ticket";
/// How long an accept ticket lets the remaining steps be retried.
pub const TICKET_TTL_SECS: u64 = 900;

/// Proof that an invite's code was already verified for `user_id`: the steps after verification
/// can be retried with it, because the code is spent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcceptTicket {
    pub user_id: String,
    pub exp: u64,
    /// Random per ticket; a ticket is one-use (see `guard::UsedTickets`).
    pub nonce: String,
}

pub fn seal_ticket(key: &SecretKey, ticket: &AcceptTicket) -> String {
    seal(
        key,
        TICKET_AAD,
        &serde_json::to_string(ticket).expect("serializable"),
    )
}

pub fn open_ticket(key: &SecretKey, sealed: &str, now: u64) -> Option<AcceptTicket> {
    let t: AcceptTicket = serde_json::from_str(&open(key, TICKET_AAD, sealed).ok()?).ok()?;
    (t.exp >= now).then_some(t)
}

pub fn set_cookie(name: &str, value: &str, max_age: u64, secure: bool) -> String {
    let secure = if secure { "; Secure" } else { "" };
    format!("{name}={value}; HttpOnly; SameSite=Strict; Path=/auth/v1; Max-Age={max_age}{secure}")
}

pub fn clear_cookie(name: &str, secure: bool) -> String {
    set_cookie(name, "", 0, secure)
}

pub fn read_cookie<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get_all("cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(n, _)| *n == name)
        .map(|(_, v)| v)
}
#[cfg(test)]
mod tests {
    use super::*;
    use chelabase_secrets::SecretKey;

    fn state(decoy: bool) -> LoginState {
        LoginState {
            auth_request: "V2_1".into(),
            user_id: (!decoy).then(|| "3000000000000000001".into()),
            session_id: (!decoy).then(|| "3000000000000000002".into()),
            session_token: (!decoy).then(|| "x".repeat(90)),
            step: Step::Verify,
            decoy,
            exp: now_secs() + LOGIN_TTL_SECS,
        }
    }

    #[test]
    fn login_state_round_trips_and_is_not_plain_text() {
        let key = SecretKey::generate();
        let sealed = seal_login(&key, &state(false));
        assert!(!sealed.contains("V2_1") && !sealed.contains("xxxx"));
        assert_eq!(open_login(&key, &sealed, now_secs()), Some(state(false)));
    }

    #[test]
    fn tampered_expired_or_foreign_cookies_are_rejected() {
        let key = SecretKey::generate();
        let sealed = seal_login(&key, &state(false));
        let mut tampered = sealed.clone();
        tampered.replace_range(10..11, if &sealed[10..11] == "A" { "B" } else { "A" });
        assert_eq!(open_login(&key, &tampered, now_secs()), None);
        assert_eq!(
            open_login(&SecretKey::generate(), &sealed, now_secs()),
            None
        );
        assert_eq!(
            open_login(&key, &sealed, now_secs() + LOGIN_TTL_SECS + 1),
            None
        );
        // A session cookie can't be replayed as a login cookie.
        let session = seal_session(
            &key,
            &SessionState {
                session_id: "s".into(),
                session_token: "t".into(),
                exp: now_secs() + 60,
            },
        );
        assert_eq!(open_login(&key, &session, now_secs()), None);
    }

    // Review Focus 1.
    #[test]
    fn decoy_and_real_login_cookies_have_the_same_length() {
        let key = SecretKey::generate();
        assert_eq!(
            seal_login(&key, &state(true)).len(),
            seal_login(&key, &state(false)).len()
        );
    }

    #[test]
    fn cookie_attributes() {
        assert_eq!(
            set_cookie(LOGIN_COOKIE, "v", LOGIN_TTL_SECS, false),
            "chelabase_login=v; HttpOnly; SameSite=Strict; Path=/auth/v1; Max-Age=900"
        );
        assert!(
            set_cookie(SESSION_COOKIE, "v", SESSION_TTL_SECS, true)
                .ends_with("; Max-Age=2592000; Secure")
        );
        assert_eq!(
            clear_cookie(LOGIN_COOKIE, false),
            "chelabase_login=; HttpOnly; SameSite=Strict; Path=/auth/v1; Max-Age=0"
        );
    }

    #[test]
    fn reads_one_cookie_among_several() {
        let mut headers = http::HeaderMap::new();
        headers.append("cookie", "a=1; chelabase_login=abc=; b=2".parse().unwrap());
        headers.append("cookie", "chelabase_session=s".parse().unwrap());
        assert_eq!(read_cookie(&headers, LOGIN_COOKIE), Some("abc="));
        assert_eq!(read_cookie(&headers, SESSION_COOKIE), Some("s"));
        assert_eq!(read_cookie(&headers, "missing"), None);
    }
}
