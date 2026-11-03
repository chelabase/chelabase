//! Whether a member token's identity session (`sid`) is still live.
//!
//! A member token outlives the session it was minted from: logout and a password reset end the
//! identity session at once, but the token verifies until `exp` (up to 15 minutes). So every
//! member-token request (the management API and realtime's `/internal/realtime/*` checks) also
//! asks the identity service whether the session is live, and refuses one that has ended.
//!
//! - Answers are cached per session for a short window (`CHELABASE_MEMBER_SESSION_CHECK_SECS`,
//!   15 s by default, at most 30 s; 0 caches nothing), live and ended alike, so a busy page
//!   costs one identity read per window. A token stops working within that window of a logout.
//! - It fails closed: when identity can't answer (or isn't configured) the request is refused
//!   as unavailable (503), never let through.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use chelabase_identity::{Identity, IdentityError, Missing, Pool};
use tracing::warn;

/// The cache window when `CHELABASE_MEMBER_SESSION_CHECK_SECS` is unset.
pub const DEFAULT_WINDOW: Duration = Duration::from_secs(15);
/// The longest window the variable allows.
pub const MAX_WINDOW_SECS: u64 = 30;
/// The longest a request waits for identity's answer before it is refused as unavailable.
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(3);
/// Sessions remembered at once; past it, expired answers go first, then everything.
const MAX_ENTRIES: usize = 10_000;

/// What the identity service says of a member token's session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Liveness {
    /// Live, and the token's subject's.
    Live,
    /// Ended (logout, a reset, a ban, expiry), unknown, someone else's, or not named at all.
    Ended,
    /// Identity can't answer now: refuse, and let the caller retry.
    Unavailable,
}

/// The session check, with its cache. One per orchestrator.
pub struct MemberSessions {
    mode: Mode,
}

enum Mode {
    Identity {
        /// `None` when identity isn't configured: every check is unavailable.
        identity: Option<Identity>,
        window: Duration,
        cache: Mutex<HashMap<String, Entry>>,
    },
    /// Tests that aren't about sessions: every session is live, and none is asked about.
    AssumeLive,
}

struct Entry {
    at: Instant,
    /// The session's user while it is live; `None` once it has ended.
    owner: Option<String>,
}

impl std::fmt::Debug for MemberSessions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.mode {
            Mode::Identity { window, .. } => f
                .debug_struct("MemberSessions")
                .field("window", window)
                .finish_non_exhaustive(),
            Mode::AssumeLive => f.write_str("MemberSessions::AssumeLive"),
        }
    }
}

impl MemberSessions {
    /// Checks against `identity`'s members pool, caching each answer for `window`.
    pub fn new(identity: Option<Identity>, window: Duration) -> Self {
        Self {
            mode: Mode::Identity {
                identity,
                window,
                cache: Mutex::new(HashMap::new()),
            },
        }
    }

    /// For tests that aren't about sessions: everything is live.
    #[doc(hidden)]
    pub fn assume_live() -> Self {
        Self {
            mode: Mode::AssumeLive,
        }
    }

    /// Whether `sid` is a live session of `sub`. A token that names no session is ended.
    pub async fn check(&self, sub: &str, sid: Option<&str>) -> Liveness {
        let Mode::Identity {
            identity,
            window,
            cache,
        } = &self.mode
        else {
            return Liveness::Live;
        };
        let Some(sid) = sid.filter(|s| !s.trim().is_empty()) else {
            return Liveness::Ended;
        };
        let Some(identity) = identity else {
            warn!(
                "a member token's session can't be checked: the identity service isn't configured"
            );
            return Liveness::Unavailable;
        };
        let now = Instant::now();
        if let Some(entry) = cache.lock().unwrap().get(sid)
            && now.duration_since(entry.at) < *window
        {
            return verdict(entry.owner.as_deref(), sub);
        }
        let answer =
            tokio::time::timeout(LOOKUP_TIMEOUT, identity.session_by_id(&Pool::Members, sid)).await;
        let owner = match answer {
            Ok(Ok(found)) if !found.user.banned && found.session.user_id == found.user.id => {
                Some(found.session.user_id)
            }
            Ok(Ok(_)) => None,
            // No such session, or an id that can't name one.
            Ok(Err(IdentityError::NotFound(Missing::Session) | IdentityError::Invalid { .. })) => {
                None
            }
            Ok(Err(err)) => {
                warn!("checking a member token's session: {err}");
                return Liveness::Unavailable;
            }
            Err(_) => {
                warn!("checking a member token's session: no answer within {LOOKUP_TIMEOUT:?}");
                return Liveness::Unavailable;
            }
        };
        let liveness = verdict(owner.as_deref(), sub);
        if !window.is_zero() {
            let mut cache = cache.lock().unwrap();
            if cache.len() >= MAX_ENTRIES {
                cache.retain(|_, e| now.duration_since(e.at) < *window);
                if cache.len() >= MAX_ENTRIES {
                    cache.clear();
                }
            }
            cache.insert(sid.to_string(), Entry { at: now, owner });
        }
        liveness
    }
}

/// Live only when the session's user is the token's subject.
fn verdict(owner: Option<&str>, sub: &str) -> Liveness {
    match owner {
        Some(owner) if owner == sub => Liveness::Live,
        _ => Liveness::Ended,
    }
}

/// `CHELABASE_MEMBER_SESSION_CHECK_SECS`: [`DEFAULT_WINDOW`] when unset or blank, else 0 to
/// [`MAX_WINDOW_SECS`] seconds.
pub fn window(value: Option<&str>) -> Result<Duration, String> {
    const NAME: &str = "CHELABASE_MEMBER_SESSION_CHECK_SECS";
    match value.map(str::trim).filter(|v| !v.is_empty()) {
        None => Ok(DEFAULT_WINDOW),
        Some(v) => v
            .parse::<u64>()
            .ok()
            .filter(|n| *n <= MAX_WINDOW_SECS)
            .map(Duration::from_secs)
            .ok_or_else(|| {
                format!(
                    "{NAME}={v:?}: expected a whole number of seconds from 0 to {MAX_WINDOW_SECS}"
                )
            }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_parses() {
        assert_eq!(window(None), Ok(DEFAULT_WINDOW));
        assert_eq!(window(Some(" ")), Ok(DEFAULT_WINDOW));
        assert_eq!(window(Some("0")), Ok(Duration::ZERO));
        assert_eq!(window(Some("30")), Ok(Duration::from_secs(30)));
        for bad in ["31", "-1", "1.5", "x"] {
            assert!(window(Some(bad)).is_err(), "{bad}");
        }
    }

    #[test]
    fn only_the_subjects_live_session_is_live() {
        assert_eq!(verdict(Some("u1"), "u1"), Liveness::Live);
        assert_eq!(verdict(Some("u2"), "u1"), Liveness::Ended);
        assert_eq!(verdict(None, "u1"), Liveness::Ended);
    }
}
