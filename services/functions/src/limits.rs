//! Per-function limits on the shared runtime (#295).
//!
//! Every function runs in one workerd process, on one thread, with no CPU limit there
//! (`infra/functions-runtime/watchdog.sh`). Two limits keep one function from taking every
//! other one down with it:
//!
//! - [`Concurrency`]: at most `max` invocations of one function on one branch at once. One past
//!   it answers 429 `too_many_requests` at once; nothing queues. A slot is taken just before the
//!   runtime is called (after the caller is checked and the body read, so a refused caller or a
//!   slow upload never holds one) and freed when the response body ends or is dropped: a
//!   failure, a timeout and a client hang-up all free it.
//! - [`Restarts`]: which function keeps the watchdog restarting the runtime. The supervisor
//!   can't say: the watchdog kills workerd because its one thread is stuck (in the culprit), so
//!   nothing in it answers, and its state dies with it. This service knows what it forwarded.
//!   A kill drops every connection in flight at once. The first such drop (a request sent, then
//!   its connection lost; never a refused connect, which only says the runtime is down) is one
//!   restart, and later drops within `span` are the same one. The restart's suspects are the
//!   invocations in flight at that moment that
//!   - had run for at least `min_stall`: the watchdog kills only after three failed probes 2 s
//!     apart, so a younger request didn't start the stall (and a request lost on a connection
//!     the runtime had just closed, at once, never counts); and
//!   - started no later than the first of them that started after the runtime's last answer:
//!     the culprit holds the one thread, so nothing is answered once it runs, and whatever came
//!     after it only queued behind it.
//!
//!   A function suspected in `threshold` restarts within `window` (with the same bundle) is
//!   disabled. If several reach it in the same restart, none is (each is logged) and a later
//!   restart decides.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use uuid::Uuid;

/// `CHELABASE_FUNCTIONS_MAX_CONCURRENT`'s default: invocations of one function on one branch
/// running at once. Above every internal caller's own bound on one function (the worker's
/// storage triggers run at most 8 per database), so they never meet it on their own.
pub const DEFAULT_MAX_CONCURRENT: usize = 32;

/// Invocations running, by `(branch, function)`.
pub struct Concurrency {
    max: usize,
    running: Mutex<HashMap<(Uuid, Uuid), usize>>,
}

/// One running invocation's slot; dropping it frees the slot.
pub struct Slot {
    owner: Arc<Concurrency>,
    key: (Uuid, Uuid),
}

impl Concurrency {
    pub fn new(max: usize) -> Self {
        Self {
            max,
            running: Mutex::new(HashMap::new()),
        }
    }

    pub fn max(&self) -> usize {
        self.max
    }

    /// A slot for `function` on `branch`, or `None` when `max` are running.
    pub fn try_acquire(self: &Arc<Self>, branch: Uuid, function: Uuid) -> Option<Slot> {
        let key = (branch, function);
        let mut running = self.running.lock().expect("concurrency lock");
        let n = running.entry(key).or_insert(0);
        if *n >= self.max {
            return None;
        }
        *n += 1;
        Some(Slot {
            owner: self.clone(),
            key,
        })
    }

    /// How many invocations of `function` on `branch` are running.
    pub fn running(&self, branch: Uuid, function: Uuid) -> usize {
        let running = self.running.lock().expect("concurrency lock");
        running.get(&(branch, function)).copied().unwrap_or(0)
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        let mut running = self.owner.running.lock().expect("concurrency lock");
        if let Some(n) = running.get_mut(&self.key) {
            *n -= 1;
            if *n == 0 {
                running.remove(&self.key);
            }
        }
    }
}

/// What a restart is attributed to: a function on a branch, running one bundle.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Suspect {
    pub branch: Uuid,
    pub function: Uuid,
    pub bundle_sha256: String,
}

/// When [`Restarts`] disables a function.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RestartPolicy {
    /// Restarts a function must be suspected in.
    pub threshold: usize,
    /// ... within this long.
    pub window: Duration,
    /// How long a request must have run when the runtime dropped it to be a suspect.
    pub min_stall: Duration,
    /// Drops this close to a restart's first are the same restart.
    pub span: Duration,
}

impl Default for RestartPolicy {
    /// 3 restarts in 10 minutes. The watchdog needs about 8 s to kill a stuck runtime and the
    /// restart a few more, so a function that loops on every call reaches it within a minute
    /// of being called three times; one rare input that loops once never does.
    fn default() -> Self {
        Self {
            threshold: 3,
            window: Duration::from_secs(600),
            min_stall: Duration::from_secs(4),
            span: Duration::from_secs(2),
        }
    }
}

/// The invocations in flight in the runtime, and the restarts each was suspected in.
pub struct Restarts {
    policy: RestartPolicy,
    state: Mutex<RestartState>,
}

#[derive(Default)]
struct RestartState {
    next: u64,
    flights: HashMap<u64, (Suspect, Instant)>,
    last_answer: Option<Instant>,
    last_restart: Option<Instant>,
    suspected: HashMap<Suspect, VecDeque<Instant>>,
}

/// One invocation in flight; dropping it ends it.
pub struct Flight {
    owner: Arc<Restarts>,
    id: u64,
}

impl Restarts {
    pub fn new(policy: RestartPolicy) -> Self {
        Self {
            policy,
            state: Mutex::new(RestartState::default()),
        }
    }

    pub fn policy(&self) -> RestartPolicy {
        self.policy
    }

    /// `suspect` was sent to the runtime at `now`.
    pub fn start(self: &Arc<Self>, suspect: Suspect, now: Instant) -> Flight {
        let mut st = self.state.lock().expect("restarts lock");
        let id = st.next;
        st.next += 1;
        st.flights.insert(id, (suspect, now));
        Flight {
            owner: self.clone(),
            id,
        }
    }

    /// The runtime answered a request (its response head) at `now`.
    pub fn answered(&self, now: Instant) {
        let mut st = self.state.lock().expect("restarts lock");
        if st.last_answer.is_none_or(|t| t < now) {
            st.last_answer = Some(now);
        }
    }

    /// The runtime dropped a request in flight at `now`: the function to disable, if this
    /// restart is the `threshold`th one within `window` with the same single suspect.
    pub fn dropped(&self, now: Instant) -> Option<Suspect> {
        let p = self.policy;
        let mut st = self.state.lock().expect("restarts lock");
        if st
            .last_restart
            .is_some_and(|t| now.saturating_duration_since(t) < p.span)
        {
            return None;
        }
        st.last_restart = Some(now);
        let stalled: Vec<(&Suspect, Instant)> = st
            .flights
            .values()
            .filter(|(_, started)| now.saturating_duration_since(*started) >= p.min_stall)
            .map(|(s, started)| (s, *started))
            .collect();
        // The first request started after the runtime's last answer; none: every one started
        // before it, and each may be the one that got stuck.
        let last_answer = st.last_answer;
        let first_after = stalled
            .iter()
            .map(|(_, started)| *started)
            .filter(|started| last_answer.is_none_or(|a| *started > a))
            .min();
        let suspects: HashSet<Suspect> = stalled
            .into_iter()
            .filter(|(_, started)| first_after.is_none_or(|t| *started <= t))
            .map(|(s, _)| s.clone())
            .collect();
        let since = now.checked_sub(p.window);
        let mut reached = Vec::new();
        for s in suspects {
            let seen = st.suspected.entry(s.clone()).or_default();
            seen.push_back(now);
            while seen
                .front()
                .is_some_and(|t| since.is_some_and(|since| *t < since))
            {
                seen.pop_front();
            }
            if seen.len() >= p.threshold {
                reached.push(s);
            }
        }
        st.suspected.retain(|_, seen| {
            seen.back()
                .is_some_and(|t| since.is_none_or(|since| *t >= since))
        });
        match reached.len() {
            0 => None,
            1 => {
                let s = reached.pop().expect("one");
                st.suspected.remove(&s);
                Some(s)
            }
            _ => {
                for s in &reached {
                    tracing::warn!(
                        branch = %s.branch, function = %s.function, bundle = %s.bundle_sha256,
                        "the functions runtime keeps restarting while this function runs, but so does another; none is disabled yet"
                    );
                }
                None
            }
        }
    }
}

impl Drop for Flight {
    fn drop(&mut self) {
        let mut st = self.owner.state.lock().expect("restarts lock");
        st.flights.remove(&self.id);
    }
}

/// `window` for a reason: whole minutes, else seconds.
pub fn human(window: Duration) -> String {
    let secs = window.as_secs();
    if secs >= 60 && secs.is_multiple_of(60) {
        format!("{} minutes", secs / 60)
    } else {
        format!("{secs} seconds")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn suspect(n: u8) -> Suspect {
        Suspect {
            branch: Uuid::from_u128(1),
            function: Uuid::from_u128(u128::from(n)),
            bundle_sha256: format!("{n:064x}"),
        }
    }

    fn policy() -> RestartPolicy {
        RestartPolicy::default()
    }

    fn secs(s: u64) -> Duration {
        Duration::from_secs(s)
    }

    /// One restart at `at`: `hung` started at `at - 8 s` after the runtime's last answer and is
    /// still in flight with `others` (started at the offsets before `at` given), then the kill
    /// drops them all. What [`Restarts::dropped`] decided.
    fn restart(
        r: &Arc<Restarts>,
        at: Instant,
        hung: &Suspect,
        others: &[(Suspect, Duration)],
    ) -> Option<Suspect> {
        r.answered(at - secs(9));
        let mut flights = vec![r.start(hung.clone(), at - secs(8))];
        for (s, ago) in others {
            flights.push(r.start(s.clone(), at - *ago));
        }
        // Every flight is dropped; the first drop decides, the rest are the same restart.
        let verdict = r.dropped(at);
        for i in 1..flights.len() {
            assert_eq!(r.dropped(at + Duration::from_millis(i as u64)), None);
        }
        verdict
    }

    #[test]
    fn concurrency_caps_each_function_alone_and_frees_on_drop() {
        let c = Arc::new(Concurrency::new(2));
        let (branch, f, g) = (Uuid::from_u128(1), Uuid::from_u128(2), Uuid::from_u128(3));
        let a = c.try_acquire(branch, f).expect("first");
        let b = c.try_acquire(branch, f).expect("second");
        assert!(c.try_acquire(branch, f).is_none(), "a third is refused");
        assert!(c.try_acquire(branch, g).is_some(), "another function isn't");
        assert!(
            c.try_acquire(Uuid::from_u128(9), f).is_some(),
            "the same function on another branch isn't"
        );
        assert_eq!(c.running(branch, f), 2);
        drop(a);
        assert_eq!(c.running(branch, f), 1);
        let again = c.try_acquire(branch, f).expect("a freed slot is reused");
        drop((b, again));
        assert_eq!(c.running(branch, f), 0);
        assert!(
            c.running.lock().unwrap().is_empty(),
            "no entry is left behind"
        );
    }

    #[test]
    fn repeated_restarts_with_one_bundle_disable_it() {
        let r = Arc::new(Restarts::new(policy()));
        let base = Instant::now();
        let hung = suspect(1);
        assert_eq!(restart(&r, base + secs(60), &hung, &[]), None);
        assert_eq!(restart(&r, base + secs(120), &hung, &[]), None);
        assert_eq!(
            restart(&r, base + secs(180), &hung, &[]),
            Some(hung.clone()),
            "the third restart within the window disables it"
        );
        // Counted afresh after a disable.
        assert_eq!(restart(&r, base + secs(240), &hung, &[]), None);
    }

    #[test]
    fn restarts_without_a_common_bundle_disable_nothing() {
        let r = Arc::new(Restarts::new(policy()));
        let base = Instant::now();
        for (i, n) in [1u8, 2, 3, 4, 5, 6].into_iter().enumerate() {
            let at = base + secs(60 * (i as u64 + 1));
            assert_eq!(restart(&r, at, &suspect(n), &[]), None, "restart {i}");
        }
        // The same function with another bundle (a redeploy) starts over.
        let mut redeployed = suspect(7);
        assert_eq!(restart(&r, base + secs(500), &redeployed, &[]), None);
        assert_eq!(restart(&r, base + secs(520), &redeployed, &[]), None);
        redeployed.bundle_sha256 = "f".repeat(64);
        assert_eq!(restart(&r, base + secs(540), &redeployed, &[]), None);
    }

    #[test]
    fn restarts_outside_the_window_disable_nothing() {
        let r = Arc::new(Restarts::new(policy()));
        let base = Instant::now();
        let hung = suspect(1);
        for i in 0..5u64 {
            assert_eq!(
                restart(&r, base + secs(301 * (i + 1)), &hung, &[]),
                None,
                "restart {i}"
            );
        }
    }

    /// Requests that arrived while the runtime was stuck only queued behind the culprit: a busy
    /// function that is in flight at every kill is never a suspect for it, nor is one that ran
    /// for less than `min_stall`.
    #[test]
    fn requests_queued_behind_the_stall_are_not_suspects() {
        let r = Arc::new(Restarts::new(policy()));
        let base = Instant::now();
        let (hung, busy, young) = (suspect(1), suspect(2), suspect(3));
        let others = [(busy.clone(), secs(7)), (young.clone(), secs(1))];
        assert_eq!(restart(&r, base + secs(60), &hung, &others), None);
        assert_eq!(restart(&r, base + secs(120), &hung, &others), None);
        assert_eq!(restart(&r, base + secs(180), &hung, &others), Some(hung));
    }

    /// A request the runtime was already running (awaiting I/O) when the culprit started is a
    /// suspect too; when two functions reach the threshold in one restart neither is disabled,
    /// and a restart without one of them decides.
    #[test]
    fn two_functions_common_to_every_restart_wait_for_one_that_tells_them_apart() {
        let r = Arc::new(Restarts::new(policy()));
        let base = Instant::now();
        let (hung, waiting) = (suspect(1), suspect(2));
        // `waiting` started before the runtime's last answer (at -9 s): the runtime ran it.
        let others = [(waiting.clone(), secs(20))];
        assert_eq!(restart(&r, base + secs(60), &hung, &others), None);
        assert_eq!(restart(&r, base + secs(120), &hung, &others), None);
        assert_eq!(
            restart(&r, base + secs(180), &hung, &others),
            None,
            "two at the threshold"
        );
        assert_eq!(restart(&r, base + secs(240), &hung, &[]), Some(hung));
    }

    /// A drop of a request that had just been sent (a keep-alive connection the runtime closed
    /// as it was reused) is not a stall: nothing in flight long enough is suspected.
    #[test]
    fn drops_of_young_requests_suspect_nothing() {
        let r = Arc::new(Restarts::new(policy()));
        let base = Instant::now();
        let f = suspect(1);
        for i in 1..=5u64 {
            let at = base + secs(60 * i);
            r.answered(at - secs(1));
            let _flight = r.start(f.clone(), at - Duration::from_millis(5));
            assert_eq!(r.dropped(at), None);
        }
    }

    #[test]
    fn ended_flights_are_forgotten() {
        let r = Arc::new(Restarts::new(policy()));
        let base = Instant::now();
        let f = suspect(1);
        for i in 1..=5u64 {
            let at = base + secs(60 * i);
            drop(r.start(f.clone(), at - secs(8)));
            assert_eq!(r.dropped(at), None);
        }
        assert!(r.state.lock().unwrap().flights.is_empty());
    }

    #[test]
    fn human_windows() {
        assert_eq!(human(secs(600)), "10 minutes");
        assert_eq!(human(secs(90)), "90 seconds");
    }
}
