//! Re-authorization (spec §4, R15, W19, W28, P13, S13): every joined channel is re-authorized
//! on each `auth` renewal, on an access event for its member or tenant (coalesced per member
//! on the instance: one run at once, then at most one trailing run per 1 s window; each run is
//! a burst whose answers the sockets of each of the member's sessions share, one `/authorize`
//! per target), and every 60 s (answers cached per (member, session, target) for the cycle and
//! shared by the instance's sockets of that session, so an ended session never borrows a live
//! one's answer; a fresh answer — a renewal's, a join's, a burst's — replaces the cycle's). Only
//! `forbidden` and `not_found` leave a channel; `token_expired` waits for the renewal;
//! `unavailable` degrades it, and the third in a row leaves it with `unavailable`.

use std::collections::HashMap;
use std::hash::Hash;
use std::time::{Duration, Instant};

use futures_util::FutureExt;
use futures_util::future::{BoxFuture, Shared};

use crate::orchestrator::{AuthTarget, AuthorizeOutcome};
use crate::protocol::ErrorCode;

/// Unavailable answers in a row that leave a channel.
pub const MAX_UNAVAILABLE: u32 = 3;

/// What a channel does with one re-authorization answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReauthAction {
    /// Allowed: stays (and leaves the degraded state).
    Keep,
    /// `error {channel, code}`, then the channel is left.
    Leave(ErrorCode),
    /// `token_expired`: nothing changes; the socket's own `exp` close decides.
    AwaitRenewal,
    /// `unavailable`: degraded (no presence, R20) until an allowed answer.
    Degrade,
}

/// One channel's re-authorization state.
#[derive(Debug, Clone, Default)]
pub struct ReauthState {
    unavailable_run: u32,
    degraded: bool,
}

impl ReauthState {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn on_outcome(&mut self, outcome: &AuthorizeOutcome) -> ReauthAction {
        match outcome {
            AuthorizeOutcome::Allowed { .. } => {
                self.unavailable_run = 0;
                self.degraded = false;
                ReauthAction::Keep
            }
            AuthorizeOutcome::Forbidden => ReauthAction::Leave(ErrorCode::Forbidden),
            AuthorizeOutcome::NotFound => ReauthAction::Leave(ErrorCode::NotFound),
            AuthorizeOutcome::TokenExpired => ReauthAction::AwaitRenewal,
            AuthorizeOutcome::Unavailable => {
                self.unavailable_run += 1;
                if self.unavailable_run >= MAX_UNAVAILABLE {
                    ReauthAction::Leave(ErrorCode::Unavailable)
                } else {
                    self.degraded = true;
                    ReauthAction::Degrade
                }
            }
        }
    }
    pub fn degraded(&self) -> bool {
        self.degraded
    }
}

/// What [`Coalescer::offer`] says about an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Offer {
    /// Run now; the window starts.
    RunNow,
    /// Folded into the window's one trailing run.
    Coalesced,
}

#[derive(Debug)]
struct Window {
    start: Instant,
    pending: bool,
}

/// R15: one run per key at once, then one trailing run at the end of the window if more
/// events came. A trailing run starts a new window.
#[derive(Debug)]
pub struct Coalescer<K> {
    window: Duration,
    keys: HashMap<K, Window>,
}

impl<K: Hash + Eq + Clone> Coalescer<K> {
    pub fn new(window: Duration) -> Self {
        Self {
            window,
            keys: HashMap::new(),
        }
    }
    pub fn offer(&mut self, key: K, now: Instant) -> Offer {
        match self.keys.get_mut(&key) {
            Some(w) if w.pending || now < w.start + self.window => {
                w.pending = true;
                Offer::Coalesced
            }
            Some(w) => {
                w.start = now;
                Offer::RunNow
            }
            None => {
                self.keys.insert(
                    key,
                    Window {
                        start: now,
                        pending: false,
                    },
                );
                Offer::RunNow
            }
        }
    }
    /// The keys whose trailing run is due at `now`, each once.
    pub fn due(&mut self, now: Instant) -> Vec<K> {
        let window = self.window;
        let mut due = vec![];
        self.keys.retain(|k, w| {
            let ended = now >= w.start + window;
            if w.pending && ended {
                due.push(k.clone());
                w.pending = false;
                w.start = now;
                true
            } else {
                // A quiet window that ended holds nothing back any more.
                !ended
            }
        });
        due
    }
    /// When the next trailing run is due, if any is pending.
    pub fn next_due(&self) -> Option<Instant> {
        self.keys
            .values()
            .filter(|w| w.pending)
            .map(|w| w.start + self.window)
            .min()
    }
}

/// One access burst's `/authorize`, shared by the member's sockets.
pub type BurstAnswer = Shared<BoxFuture<'static, AuthorizeOutcome>>;

/// W28: `/authorize` answers per (member sub, identity session, target) for one 60 s cycle,
/// and each target's answer of the member's latest access burst (one call, however many of the
/// session's sockets ask). Answers are never shared across sessions: one that has ended (a
/// logout, a reset) is refused even while another session of the member is allowed.
#[derive(Default)]
pub struct CycleCache {
    cycle: u64,
    answers: HashMap<(String, String, AuthTarget), AuthorizeOutcome>,
    bursts: HashMap<(String, String, AuthTarget), (u64, BurstAnswer)>,
}

impl std::fmt::Debug for CycleCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CycleCache")
            .field("cycle", &self.cycle)
            .field("answers", &self.answers.len())
            .field("bursts", &self.bursts.len())
            .finish()
    }
}

impl CycleCache {
    pub fn new() -> Self {
        Self::default()
    }
    /// The answer stored for this cycle, `None` for another cycle's.
    pub fn get(
        &self,
        sub: &str,
        sid: &str,
        target: &AuthTarget,
        cycle: u64,
    ) -> Option<AuthorizeOutcome> {
        if cycle != self.cycle {
            return None;
        }
        self.answers
            .get(&(sub.to_string(), sid.to_string(), target.clone()))
            .cloned()
    }
    /// Stores an answer; a new cycle drops every older one.
    pub fn put(
        &mut self,
        sub: &str,
        sid: &str,
        target: &AuthTarget,
        cycle: u64,
        outcome: AuthorizeOutcome,
    ) {
        if cycle < self.cycle {
            return;
        }
        if cycle > self.cycle {
            self.answers.clear();
            self.cycle = cycle;
        }
        self.answers
            .insert((sub.to_string(), sid.to_string(), target.clone()), outcome);
    }
    /// The answer of access burst `burst` for (sub, sid, target): the one in flight or given,
    /// or `ask`'s (started now, replacing an older burst's). Every socket of the member's
    /// session awaits the same call.
    pub fn burst(
        &mut self,
        sub: &str,
        sid: &str,
        target: &AuthTarget,
        burst: u64,
        ask: impl FnOnce() -> BoxFuture<'static, AuthorizeOutcome>,
    ) -> BurstAnswer {
        let key = (sub.to_string(), sid.to_string(), target.clone());
        match self.bursts.get(&key) {
            Some((b, answer)) if *b == burst => answer.clone(),
            _ => {
                let answer = ask().shared();
                self.bursts.insert(key, (burst, answer.clone()));
                answer
            }
        }
    }
    /// Forgets the burst answers of `sub` older than `burst` (a member whose sockets are gone
    /// keeps nothing for long: called with each new burst).
    pub fn prune_bursts(&mut self, sub: &str, burst: u64) {
        self.bursts.retain(|(s, _, _), (b, _)| s != sub || *b >= burst);
    }
    pub fn len(&self) -> usize {
        self.answers.len()
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}
