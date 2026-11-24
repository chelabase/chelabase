//! Per-function limits on the shared runtime (#295).
//!
//! Every function runs in one workerd process, on one thread, with no CPU limit there
//! (`infra/functions-runtime/watchdog.sh`). A cap keeps one function from piling into it, and
//! the restarts are attributed so the operator knows which function to turn off:
//!
//! - [`Concurrency`]: at most `max` invocations of one function on one branch at once from
//!   outside callers (schedules, database events and storage triggers are bounded by their own
//!   limits and exempt). One past it answers 429 `too_many_requests` at once; nothing queues.
//!   A slot is taken just before the runtime is called (after the caller is checked and the body read, so a refused caller or a
//!   slow upload never holds one) and freed when the response body ends or is dropped: a
//!   failure, a timeout and a client hang-up all free it.
//! - [`Restarts`]: which function keeps the watchdog restarting the runtime, reported to the
//!   operator (a warning per restart counted, an error at `threshold`), never acted on: see the
//!   last paragraph. The supervisor
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
//!   An invocation whose caller hung up, or that timed out, before the runtime answered is kept
//!   as abandoned: the runtime may still be running it (stuck in it), so it stays a suspect
//!   until the runtime answers anything (the thread isn't stuck in it then), restarts, or
//!   [`ORPHAN_TTL`] passes. Its slot is freed at once all the same.
//!
//!   A restart counts only when it has exactly one suspect (a function on a branch with one
//!   bundle); with none or several it counts for nobody, so an innocent function that happens
//!   to be in flight at extra restarts never pulls ahead of the looper. A function that is the
//!   only suspect of `threshold` restarts within `window` is reported at error level. A looper
//!   that always overlaps a long-running function is never reported.
//!
//!   Why nothing is turned off automatically: a function that answers and then keeps running
//!   (in `ctx.waitUntil`, or after its body ended) has left nothing in flight, and the request
//!   queued behind it, perhaps another tenant's function, looks exactly like a looper on a
//!   quiet runtime. Acting on it would let one tenant get another's function disabled. Only the
//!   runtime could say which request it was running when it got stuck.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use uuid::Uuid;

/// Locks `m`, through a poisoning: the state stays consistent (every update is a few lines
/// that don't panic), and a `Drop` must never panic.
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// How long an abandoned request stays a suspect with no answer or restart since: a runtime
/// stuck in it is killed within about 9 s.
pub const ORPHAN_TTL: Duration = Duration::from_secs(60);

/// `CHELABASE_FUNCTIONS_MAX_CONCURRENT`'s default: invocations of one function on one branch
/// running at once by outside callers (internal ones, bounded by their own limits, don't
/// count). Room for an I/O-bound function at about 100 calls a second.
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
        let mut running = lock(&self.running);
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
        let running = lock(&self.running);
        running.get(&(branch, function)).copied().unwrap_or(0)
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        let mut running = lock(&self.owner.running);
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
    /// The function's name on the branch (for the operator's `chelabase functions disable`).
    pub name: String,
    pub bundle_sha256: String,
}

/// A restart counted against its only suspect.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Credit {
    pub suspect: Suspect,
    /// Restarts counted against it within the window, this one included.
    pub count: usize,
    /// `count` reached the policy's threshold (the count starts over after it).
    pub reached: bool,
}

/// When [`Restarts`] counts a restart and when it reports a function at error level.
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
    /// Every invocation in flight, and each one abandoned without an answer (its caller hung
    /// up, or it timed out) that the runtime may still be running: `(what, started,
    /// abandoned)`.
    flights: HashMap<u64, (Suspect, Instant, Option<Instant>)>,
    last_answer: Option<Instant>,
    last_restart: Option<Instant>,
    suspected: HashMap<Suspect, VecDeque<Instant>>,
}

/// One invocation in flight. Dropped unanswered (cancelled, timed out), it is abandoned:
/// the runtime may still be running it, so it stays a suspect until the runtime answers
/// anything, it restarts, or [`ORPHAN_TTL`] passes.
pub struct Flight {
    owner: Arc<Restarts>,
    id: u64,
    answered: bool,
    done: bool,
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
        let mut st = lock(&self.state);
        let id = st.next;
        st.next += 1;
        prune_orphans(&mut st, now);
        st.flights.insert(id, (suspect, now, None));
        Flight {
            owner: self.clone(),
            id,
            answered: false,
            done: false,
        }
    }

    /// The runtime answered a request (its response head) at `now`.
    pub fn answered(&self, now: Instant) {
        let mut st = lock(&self.state);
        if st.last_answer.is_none_or(|t| t < now) {
            st.last_answer = Some(now);
        }
        // An answer proves the thread isn't stuck in what was abandoned before it.
        st.flights
            .retain(|_, (_, _, abandoned)| abandoned.is_none_or(|a| a >= now));
    }

    /// The runtime dropped a request in flight at `now`: the credit, when this restart had
    /// exactly one suspect, with the count of restarts counted against it within `window`
    /// (`reached` at `threshold`, after which its count starts over). A restart
    /// with no suspect, or with several, counts for nobody (a long-running innocent in flight
    /// at an extra restart can then never pull ahead of the looper).
    pub fn dropped(&self, now: Instant) -> Option<Credit> {
        let p = self.policy;
        let mut st = lock(&self.state);
        if st
            .last_restart
            .is_some_and(|t| now.saturating_duration_since(t) < p.span)
        {
            return None;
        }
        st.last_restart = Some(now);
        prune_orphans(&mut st, now);
        let stalled: Vec<(&Suspect, Instant)> = st
            .flights
            .values()
            .filter(|(_, started, _)| now.saturating_duration_since(*started) >= p.min_stall)
            .map(|(s, started, _)| (s, *started))
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
        // The restart consumed whatever was abandoned: the runtime runs none of it now.
        st.flights
            .retain(|_, (_, _, abandoned)| abandoned.is_none());
        let since = now.checked_sub(p.window);
        st.suspected.retain(|_, seen| {
            seen.back()
                .is_some_and(|t| since.is_none_or(|since| *t >= since))
        });
        if suspects.len() != 1 {
            if suspects.len() > 1 {
                tracing::info!(
                    suspects = suspects.len(),
                    "the functions runtime restarted with several functions that could have stalled it; none is counted"
                );
            }
            return None;
        }
        let s = suspects.into_iter().next().expect("one");
        let seen = st.suspected.entry(s.clone()).or_default();
        seen.push_back(now);
        while seen
            .front()
            .is_some_and(|t| since.is_some_and(|since| *t < since))
        {
            seen.pop_front();
        }
        let count = seen.len();
        let reached = count >= p.threshold;
        if reached {
            st.suspected.remove(&s);
        }
        Some(Credit {
            suspect: s,
            count,
            reached,
        })
    }
}

/// Forgets what was abandoned more than [`ORPHAN_TTL`] before `now`.
fn prune_orphans(st: &mut RestartState, now: Instant) {
    st.flights.retain(|_, (_, _, abandoned)| {
        abandoned.is_none_or(|a| now.saturating_duration_since(a) < ORPHAN_TTL)
    });
}

impl Flight {
    /// The runtime answered this request: when it ends, it is simply gone.
    pub fn set_answered(&mut self) {
        self.answered = true;
    }

    /// It ended with the runtime no longer running it (the runtime dropped it, or it never
    /// reached it).
    pub fn end(mut self) {
        self.done = true;
        lock(&self.owner.state).flights.remove(&self.id);
    }

    /// It ended at `now` without an answer while the runtime may still run it.
    pub fn abandon(mut self, now: Instant) {
        self.done = true;
        self.mark_abandoned(now);
    }

    fn mark_abandoned(&self, now: Instant) {
        let mut st = lock(&self.owner.state);
        if self.answered {
            st.flights.remove(&self.id);
        } else if let Some(flight) = st.flights.get_mut(&self.id) {
            flight.2 = Some(now);
        }
    }
}

impl Drop for Flight {
    fn drop(&mut self) {
        if !self.done {
            self.mark_abandoned(Instant::now());
        }
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
            name: format!("f{n}"),
            bundle_sha256: format!("{n:064x}"),
        }
    }

    /// The suspect whose count reached the threshold, if any.
    fn reached(c: Option<Credit>) -> Option<Suspect> {
        c.filter(|c| c.reached).map(|c| c.suspect)
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
        let verdict = reached(r.dropped(at));
        for i in 1..flights.len() {
            assert_eq!(r.dropped(at + Duration::from_millis(i as u64)), None);
        }
        for f in flights {
            f.end();
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
    fn repeated_restarts_with_one_bundle_reach_the_threshold() {
        let r = Arc::new(Restarts::new(policy()));
        let base = Instant::now();
        let hung = suspect(1);
        assert_eq!(restart(&r, base + secs(60), &hung, &[]), None);
        assert_eq!(restart(&r, base + secs(120), &hung, &[]), None);
        assert_eq!(
            restart(&r, base + secs(180), &hung, &[]),
            Some(hung.clone()),
            "the third restart within the window reaches the threshold"
        );
        // Counted afresh after the threshold.
        assert_eq!(restart(&r, base + secs(240), &hung, &[]), None);
    }

    #[test]
    fn restarts_without_a_common_bundle_reach_nothing() {
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
    fn restarts_outside_the_window_reach_nothing() {
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
    /// suspect too, and a restart with two suspects is credited to neither (review B2): only
    /// restarts with the looper alone count.
    #[test]
    fn a_restart_with_two_suspects_counts_for_neither() {
        let r = Arc::new(Restarts::new(policy()));
        let base = Instant::now();
        let (hung, waiting) = (suspect(1), suspect(2));
        // `waiting` started before the runtime's last answer (at -9 s): the runtime ran it.
        let others = [(waiting.clone(), secs(20))];
        for i in 1..=4u64 {
            assert_eq!(
                restart(&r, base + secs(60 * i), &hung, &others),
                None,
                "restart {i}"
            );
        }
        assert_eq!(restart(&r, base + secs(300), &hung, &[]), None);
        assert_eq!(restart(&r, base + secs(360), &hung, &[]), None);
        assert_eq!(restart(&r, base + secs(420), &hung, &[]), Some(hung));
    }

    /// Warn-only (#295): every restart counted against its only suspect is reported with the
    /// count in the window, so the operator sees it build up; the threshold is flagged, and the
    /// count starts over after it.
    #[test]
    fn each_counted_restart_reports_its_count() {
        let r = Arc::new(Restarts::new(policy()));
        let base = Instant::now();
        let hung = suspect(1);
        let mut credits = Vec::new();
        for i in 1..=4u64 {
            r.answered(base + secs(60 * i) - secs(9));
            let f = r.start(hung.clone(), base + secs(60 * i) - secs(8));
            credits.push(r.dropped(base + secs(60 * i)).map(|c| (c.count, c.reached)));
            f.end();
        }
        assert_eq!(
            credits,
            vec![
                Some((1, false)),
                Some((2, false)),
                Some((3, true)),
                Some((1, false))
            ]
        );
        // A restart with two suspects reports nothing.
        r.answered(base + secs(400) - secs(9));
        let a = r.start(suspect(1), base + secs(400) - secs(8));
        let b = r.start(suspect(2), base + secs(400) - secs(20));
        assert_eq!(r.dropped(base + secs(400)), None);
        a.end();
        b.end();
    }

    /// Review B2's example: a runtime death with a long-running innocent in flight alone (an
    /// OOM kill another function caused) and two looper restarts reach nothing: the
    /// innocent's restart doesn't add to the looper's, and the looper's (where the innocent
    /// was also in flight) don't add to the innocent's. Three restarts with the looper alone do.
    #[test]
    fn an_innocent_in_an_extra_restart_never_pulls_ahead() {
        let r = Arc::new(Restarts::new(policy()));
        let base = Instant::now();
        let (looper, innocent) = (suspect(1), suspect(2));
        // The OOM-like death: the runtime was answering until just now; only the innocent has
        // run long enough.
        let at = base + secs(60);
        r.answered(at - Duration::from_millis(300));
        let i = r.start(innocent.clone(), at - secs(20));
        assert_eq!(reached(r.dropped(at)), None);
        i.end();
        // Two looper restarts, the innocent in flight (since before the last answer) in both.
        let others = [(innocent.clone(), secs(25))];
        assert_eq!(restart(&r, base + secs(120), &looper, &others), None);
        assert_eq!(restart(&r, base + secs(180), &looper, &others), None);
        // Then the looper alone, three times.
        assert_eq!(restart(&r, base + secs(240), &looper, &[]), None);
        assert_eq!(restart(&r, base + secs(300), &looper, &[]), None);
        assert_eq!(restart(&r, base + secs(360), &looper, &[]), Some(looper));
    }

    /// Review B1: the looper's caller hangs up (or times out) a second in, and a request
    /// queues behind it; at the kill the looper, though no longer in flight here, is the
    /// suspect and the queued request isn't.
    #[test]
    fn an_abandoned_looper_stays_the_suspect() {
        let r = Arc::new(Restarts::new(policy()));
        let base = Instant::now();
        let (looper, queued) = (suspect(1), suspect(2));
        for i in 1..=3u64 {
            let at = base + secs(60 * i);
            r.answered(at - secs(9));
            let l = r.start(looper.clone(), at - secs(8));
            l.abandon(at - secs(7));
            let q = r.start(queued.clone(), at - Duration::from_millis(6500));
            let verdict = reached(r.dropped(at));
            q.end();
            let want = (i == 3).then(|| looper.clone());
            assert_eq!(
                verdict, want,
                "restart {i}: the looper, not the queued request"
            );
        }
        assert!(
            r.state.lock().unwrap().flights.is_empty(),
            "a restart consumes orphans"
        );
    }

    /// An abandoned request the runtime answered something after is no longer a suspect: an
    /// answer proves the thread wasn't stuck in it.
    #[test]
    fn an_answer_clears_abandoned_requests() {
        let r = Arc::new(Restarts::new(policy()));
        let base = Instant::now();
        let (gone, looper) = (suspect(1), suspect(2));
        for i in 1..=3u64 {
            let at = base + secs(60 * i);
            let g = r.start(gone.clone(), at - secs(30));
            g.abandon(at - secs(20));
            r.answered(at - secs(9));
            let l = r.start(looper.clone(), at - secs(8));
            let verdict = reached(r.dropped(at));
            l.end();
            assert_eq!(verdict, (i == 3).then(|| looper.clone()), "restart {i}");
        }
    }

    /// An abandoned request is forgotten after [`ORPHAN_TTL`] even with no answer since (a
    /// runtime stuck in it would have been restarted long before).
    #[test]
    fn abandoned_requests_expire() {
        let r = Arc::new(Restarts::new(policy()));
        let base = Instant::now();
        let g = r.start(suspect(1), base);
        g.abandon(base + secs(1));
        r.start(suspect(2), base + ORPHAN_TTL + secs(5)).end();
        assert!(r.state.lock().unwrap().flights.is_empty());
    }

    /// The attribution model (review B2): over random runs of restarts (a looper in flight or
    /// whose caller gave up, long-running innocents, requests queued behind it, young ones,
    /// abandoned ones, and runtime deaths with no looper while it was answering), a function is
    /// reported at the threshold exactly when it is the sole suspect of its `threshold`th restart within the
    /// window, the sole suspect being worked out here from what each request was, not from the
    /// timings.
    mod model {
        use super::*;
        use proptest::prelude::*;

        #[derive(Clone, Copy, Debug)]
        enum Kind {
            Looper,
            AbandonedLooper,
            NoLooper,
        }

        #[derive(Clone, Copy, Debug)]
        enum Role {
            /// Started before the last answer (awaiting I/O), still in flight.
            LongRunning,
            /// Started after the looper (with no looper: before the last answer), 4 s+ old.
            Queued,
            /// Under `min_stall` old.
            Young,
            /// Abandoned long before the last answer.
            AbandonedEarly,
            /// Started after the looper and abandoned before the kill.
            QueuedAbandoned,
        }

        fn kind() -> impl Strategy<Value = Kind> {
            prop_oneof![
                Just(Kind::Looper),
                Just(Kind::AbandonedLooper),
                Just(Kind::NoLooper)
            ]
        }

        fn role() -> impl Strategy<Value = Role> {
            prop_oneof![
                Just(Role::LongRunning),
                Just(Role::Queued),
                Just(Role::Young),
                Just(Role::AbandonedEarly),
                Just(Role::QueuedAbandoned),
            ]
        }

        type Run = (u64, Kind, Vec<(u8, Role)>);

        /// `(gap since the last restart in s, kind, [(innocent function 2..=4, role)])`.
        fn runs() -> impl Strategy<Value = Vec<Run>> {
            prop::collection::vec(
                (
                    20u64..400,
                    kind(),
                    prop::collection::vec((2u8..=4, role()), 0..4),
                ),
                1..14,
            )
        }

        /// Something that happens this long before the kill.
        enum Ev {
            Start(usize, Suspect, Duration),
            Abandon(usize, Duration),
            Answer(Duration),
        }

        impl Ev {
            fn ago(&self) -> Duration {
                match self {
                    Ev::Start(_, _, d) | Ev::Abandon(_, d) | Ev::Answer(d) => *d,
                }
            }
        }

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(512))]
            #[test]
            fn reaches_exactly_the_sole_suspect_of_threshold_restarts(runs in runs()) {
                let p = policy();
                let r = Arc::new(Restarts::new(p));
                let mut at = Instant::now() + secs(3600);
                let mut counted: HashMap<Suspect, VecDeque<Instant>> = HashMap::new();
                for (gap, kind, innocents) in runs {
                    at += secs(gap);
                    let looper = suspect(1);
                    let answer = match kind {
                        Kind::NoLooper => Duration::from_millis(500),
                        _ => secs(9),
                    };
                    let mut evs = vec![Ev::Answer(answer)];
                    let mut expected: HashSet<Suspect> = HashSet::new();
                    if !matches!(kind, Kind::NoLooper) {
                        evs.push(Ev::Start(0, looper.clone(), secs(8)));
                        if matches!(kind, Kind::AbandonedLooper) {
                            evs.push(Ev::Abandon(0, secs(7)));
                        }
                        expected.insert(looper.clone());
                    }
                    for (i, (f, role)) in innocents.iter().enumerate() {
                        let (id, s) = (i + 1, suspect(*f));
                        match role {
                            Role::LongRunning => {
                                evs.push(Ev::Start(id, s.clone(), secs(20)));
                                expected.insert(s);
                            }
                            Role::Queued => {
                                evs.push(Ev::Start(id, s.clone(), Duration::from_millis(6000)));
                                if matches!(kind, Kind::NoLooper) {
                                    expected.insert(s);
                                }
                            }
                            Role::Young => evs.push(Ev::Start(id, s, secs(1))),
                            Role::AbandonedEarly => {
                                evs.push(Ev::Start(id, s, secs(40)));
                                evs.push(Ev::Abandon(id, secs(30)));
                            }
                            Role::QueuedAbandoned => {
                                evs.push(Ev::Start(id, s, Duration::from_millis(6500)));
                                evs.push(Ev::Abandon(id, Duration::from_millis(5000)));
                            }
                        }
                    }
                    evs.sort_by_key(|e| std::cmp::Reverse(e.ago()));
                    let mut flights: HashMap<usize, Flight> = HashMap::new();
                    for e in evs {
                        match e {
                            Ev::Start(id, s, d) => {
                                flights.insert(id, r.start(s, at - d));
                            }
                            Ev::Abandon(id, d) => {
                                flights.remove(&id).expect("started").abandon(at - d);
                            }
                            Ev::Answer(d) => r.answered(at - d),
                        }
                    }
                    let got = reached(r.dropped(at));
                    for (_, f) in flights {
                        f.end();
                    }
                    // The oracle: only a restart with one suspect counts, for that one.
                    let mut want = None;
                    if expected.len() == 1 {
                        let s = expected.into_iter().next().expect("one");
                        let seen = counted.entry(s.clone()).or_default();
                        seen.push_back(at);
                        while seen.front().is_some_and(|t| *t < at - p.window) {
                            seen.pop_front();
                        }
                        if seen.len() >= p.threshold {
                            counted.remove(&s);
                            want = Some(s);
                        }
                    }
                    prop_assert_eq!(got, want);
                }
            }
        }
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
            assert_eq!(reached(r.dropped(at)), None);
        }
    }

    #[test]
    fn ended_flights_are_forgotten() {
        let r = Arc::new(Restarts::new(policy()));
        let base = Instant::now();
        let f = suspect(1);
        for i in 1..=5u64 {
            let at = base + secs(60 * i);
            let mut flight = r.start(f.clone(), at - secs(8));
            flight.set_answered();
            drop(flight);
            assert_eq!(reached(r.dropped(at)), None);
        }
        assert!(r.state.lock().unwrap().flights.is_empty());
    }

    #[test]
    fn human_windows() {
        assert_eq!(human(secs(600)), "10 minutes");
        assert_eq!(human(secs(90)), "90 seconds");
    }
}
