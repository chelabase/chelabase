//! Which sessions of a locked-down login are void, kept as a durable fact.
//!
//! The bootstrap locks two logins down on every run: the data API's
//! `chelabase_authenticator_elevated` ([`crate::data_api`]) and the storage service's
//! `chelabase_storage` ([`crate::storage_schema`]). Each lockdown sets the login's stored
//! verifier back. Postgres reads a role's verifier when a connection's handshake starts and
//! lists the session under its role only once its start-up has finished, which can be any
//! time later (no setting bounds it), so ending the login's listed sessions around the commit
//! is not enough on its own: the lockdown also records, on the login itself, that its earlier
//! sessions are void.
//!
//! The record is the login's comment (`pg_shdescription`: one row per role, read from every
//! database, written only by a superuser or a role holding `ADMIN` on the login, and the
//! lockdown leaves no such role): [`PENDING`], or `chelabase lockdown <cut-off>` with the
//! cut-off in microseconds since the epoch.
//!
//! # Invariant
//!
//! For each locked-down login, with `V` its stored verifier:
//!
//! 1. **Recorded with the drift.** A lockdown that finds the login drifted writes [`PENDING`]
//!    in the transaction that sets `V` back. Neither is visible without the other.
//! 2. **The cut-off follows the commit.** [`PENDING`] is replaced only by a cut-off read from
//!    the clock after that transaction committed. So every session whose handshake read a
//!    verifier before the commit has `backend_start` at or before the cut-off.
//!    - The bootstrap's advisory lock is per database and the bootstrap runs once per served
//!      database, so it does not order the runs of two databases. The rule holds through the
//!      role's lock instead: `COMMENT ON ROLE` and `ALTER ROLE` take a cluster-wide lock on
//!      the login, and every run sets the same `V`, so a cut-off one run takes before another
//!      run's commit still follows the commit that replaced the verifier.
//!    - Limit: the cut-off and `backend_start` are wall-clock times. If the compute's clock
//!      steps backward between a handshake's start and the cut-off, that session can be left
//!      after the cut-off. The same holds for rule 6's comparison with the postmaster's start.
//! 3. **A void session is drift.** While the record is [`PENDING`] (or unreadable), or any
//!    session of the login has `backend_start` at or before the cut-off, the login's drift
//!    predicate is true ([`void_sessions_sql`] is part of it). The health check then answers
//!    `f`, and the next lockdown ends every session of the login and writes [`PENDING`] again.
//! 4. **No step relies on the worker's session.** The step after the commit
//!    ([`finish_sql`]) reads the record, not a session setting: whichever run reaches it
//!    turns [`PENDING`] into a cut-off and ends the sessions at or before the cut-off.
//! 5. **Current sessions are kept.** A lockdown that finds no drift ends no session of the
//!    login, and a session started after the cut-off is never void.
//! 6. **The record stays while a session it covers can exist.** Its age never clears it. Only
//!    a lockdown that finds no drift clears it, and only when the postmaster started after the
//!    cut-off (`pg_postmaster_start_time()`): a restart ends every session and every handshake
//!    from before it, so nothing the record covers is left. On the same postmaster the record
//!    stays, however old. A record that stays costs nothing, because a session started after
//!    the cut-off is never void (rule 5).
//!
//! Together: once a lockdown has replaced a login's verifier, a session authenticated against
//! the replaced one, whenever it is first listed, is ended by that run, or makes the health
//! check answer `f` and is ended by the bootstrap that follows. The `model` tests below
//! enumerate the interleavings of a handshake of any length, the lockdown's steps, the loss
//! of the worker's session, a postmaster restart, the health check and the bootstraps that
//! follow against these rules.

use crate::data_api::{BOOTSTRAP_LOCK_KEY, LOCK_TIMEOUT_SECS};

/// The record a drifted lockdown commits: the login's sessions are void and no cut-off has
/// been taken yet.
pub const PENDING: &str = "chelabase lockdown pending";

/// The record's cut-off form, as a POSIX regex whose one group is the cut-off.
const CUTOFF_PATTERN: &str = "^chelabase lockdown ([0-9]{1,18})$";

/// How long the step after the commit waits for each ended session to exit, in milliseconds
/// (the health check that follows must not still list it).
const EXIT_WAIT_MS: u32 = 5000;

/// `at` (a `timestamptz` expression) in microseconds since the epoch, rounded up.
fn micros(at: &str) -> String {
    format!(
        "pg_catalog.ceil(pg_catalog.date_part('epoch', {at}) OPERATOR(pg_catalog.*) 1000000)::pg_catalog.int8"
    )
}

/// The cut-off recorded on `role` (an oid expression): NULL for no record, [`PENDING`] or
/// anything unreadable.
fn cutoff(role: &str) -> String {
    format!(
        "pg_catalog.substring(pg_catalog.shobj_description({role}, 'pg_authid'), '{CUTOFF_PATTERN}')::pg_catalog.int8"
    )
}

/// Whether `role` (an oid expression) has sessions that are void, or may have: its record is
/// [`PENDING`] or unreadable, or one of its sessions started at or before the recorded
/// cut-off. Part of each login's drift predicate (rule 3). Operators are schema-qualified.
pub fn void_sessions_sql(role: &str) -> String {
    format!(
        "(pg_catalog.shobj_description({role}, 'pg_authid') IS NOT NULL
      AND ({cutoff} IS NULL OR EXISTS (SELECT FROM pg_catalog.pg_stat_activity v WHERE v.usesysid OPERATOR(pg_catalog.=) {role}
        AND {started} OPERATOR(pg_catalog.<=) {cutoff})))",
        cutoff = cutoff(role),
        started = micros("v.backend_start"),
    )
}

/// The lockdown's own statements (PL/pgSQL, inside its transaction): when `void` (a boolean
/// expression: the run found the login drifted, or replaces its verifier) the record becomes
/// [`PENDING`] (rule 1); otherwise a cut-off from before the postmaster started is cleared,
/// and any other record is left as it is, whatever its age (rule 6). `name` is the login,
/// `role` its oid expression.
pub fn record_sql(name: &str, role: &str, void: &str) -> String {
    format!(
        "  IF {void} THEN
    COMMENT ON ROLE {name} IS '{PENDING}';
  ELSIF {cutoff} OPERATOR(pg_catalog.<) {postmaster_start} THEN
    COMMENT ON ROLE {name} IS NULL;
  END IF;
",
        cutoff = cutoff(role),
        postmaster_start = micros("pg_catalog.pg_postmaster_start_time()"),
    )
}

/// The step after the lockdown's `COMMIT`, its own transaction under the bootstrap lock
/// (rules 2 and 4): a record that isn't a cut-off becomes one, read from the clock now; then
/// every session of `name` that started at or before the cut-off is ended. With no record it
/// does nothing. It reads only the record, so it completes a lockdown whose own run never
/// reached it.
pub fn finish_sql(name: &str) -> String {
    format!(
        "DO $$ DECLARE
  locked pg_catalog.oid := (SELECT oid FROM pg_catalog.pg_roles WHERE rolname OPERATOR(pg_catalog.=) '{name}');
  cutoff pg_catalog.int8;
BEGIN
  SET LOCAL lock_timeout = '{LOCK_TIMEOUT_SECS}s';
  PERFORM pg_catalog.pg_advisory_xact_lock({BOOTSTRAP_LOCK_KEY});
  IF pg_catalog.shobj_description(locked, 'pg_authid') IS NULL THEN RETURN; END IF;
  cutoff := {recorded};
  IF cutoff IS NULL THEN
    cutoff := {now} OPERATOR(pg_catalog.+) 1;
    EXECUTE pg_catalog.format('COMMENT ON ROLE {name} IS %L', 'chelabase lockdown ' OPERATOR(pg_catalog.||) cutoff::pg_catalog.text);
  END IF;
  PERFORM pg_catalog.pg_terminate_backend(v.pid, {EXIT_WAIT_MS}) FROM pg_catalog.pg_stat_activity v
    WHERE v.usesysid OPERATOR(pg_catalog.=) locked AND {started} OPERATOR(pg_catalog.<=) cutoff
      AND v.pid OPERATOR(pg_catalog.<>) pg_catalog.pg_backend_pid();
END $$;
",
        recorded = cutoff("locked"),
        now = micros("pg_catalog.clock_timestamp()"),
        started = micros("v.backend_start"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROLE: &str =
        "(SELECT oid FROM pg_catalog.pg_roles WHERE rolname OPERATOR(pg_catalog.=) 'chelabase_x')";

    /// Rule 3: a record that isn't a cut-off, or a session at or before the cut-off.
    #[test]
    fn a_pending_record_or_a_session_before_the_cutoff_is_void() {
        let sql = void_sessions_sql(ROLE);
        assert!(
            sql.contains(&format!(
                "pg_catalog.shobj_description({ROLE}, 'pg_authid') IS NOT NULL"
            )),
            "{sql}"
        );
        // PENDING and anything unreadable have no cut-off.
        assert!(
            sql.contains(&format!(
                "'{CUTOFF_PATTERN}')::pg_catalog.int8 IS NULL OR EXISTS"
            )),
            "{sql}"
        );
        assert!(
            sql.contains(&format!("v.usesysid OPERATOR(pg_catalog.=) {ROLE}")),
            "{sql}"
        );
        assert!(
            sql.contains("pg_catalog.date_part('epoch', v.backend_start)"),
            "{sql}"
        );
        assert!(
            sql.contains("OPERATOR(pg_catalog.<=) pg_catalog.substring("),
            "{sql}"
        );
        // The two forms the worker writes are told apart by the pattern.
        assert!(
            !PENDING
                .strip_prefix("chelabase lockdown ")
                .unwrap()
                .bytes()
                .all(|b| b.is_ascii_digit())
        );
    }

    /// Rules 1 and 6: PENDING when void; otherwise cleared only when the postmaster started
    /// after the cut-off, with no comparison against the record's age.
    #[test]
    fn the_lockdown_records_pending_and_clears_only_after_a_postmaster_restart() {
        let sql = record_sql("chelabase_x", "role", "ended");
        assert_eq!(
            sql,
            format!(
                "  IF ended THEN
    COMMENT ON ROLE chelabase_x IS 'chelabase lockdown pending';
  ELSIF pg_catalog.substring(pg_catalog.shobj_description(role, 'pg_authid'), '{CUTOFF_PATTERN}')::pg_catalog.int8 OPERATOR(pg_catalog.<) pg_catalog.ceil(pg_catalog.date_part('epoch', pg_catalog.pg_postmaster_start_time()) OPERATOR(pg_catalog.*) 1000000)::pg_catalog.int8 THEN
    COMMENT ON ROLE chelabase_x IS NULL;
  END IF;
"
            )
        );
        // Nothing reads the clock, so no age decides it.
        for clock in ["clock_timestamp", "now()", "current_timestamp", "interval"] {
            assert!(!sql.contains(clock), "{clock}: {sql}");
        }
    }

    /// Rules 2 and 4: under the bootstrap lock, from the record alone.
    #[test]
    fn the_step_after_the_commit_reads_only_the_record() {
        let sql = finish_sql("chelabase_x");
        assert!(
            sql.starts_with("DO $$ DECLARE\n") && sql.ends_with("END $$;\n"),
            "{sql}"
        );
        assert!(
            !sql.contains("current_setting"),
            "no session setting decides it: {sql}"
        );
        let lock = sql
            .find(&format!(
                "pg_catalog.pg_advisory_xact_lock({BOOTSTRAP_LOCK_KEY})"
            ))
            .expect("the bootstrap lock");
        let read = sql
            .find("IF pg_catalog.shobj_description(locked, 'pg_authid') IS NULL THEN RETURN;")
            .expect("no record, nothing to do");
        let clock = sql
            .find("pg_catalog.clock_timestamp()")
            .expect("the cut-off is read from the clock");
        let write = sql.find("EXECUTE pg_catalog.format('COMMENT ON ROLE chelabase_x IS %L', 'chelabase lockdown '").expect("the cut-off is recorded");
        let end = sql
            .find("pg_catalog.pg_terminate_backend(v.pid, 5000)")
            .expect("sessions ended");
        assert!(
            lock < read && read < clock && clock < write && write < end,
            "{sql}"
        );
        assert!(
            sql[end..].contains("OPERATOR(pg_catalog.<=) cutoff"),
            "{sql}"
        );
    }
}

/// The invariant at the top of this file, as a model: the record, the catalog's verifier, the
/// postmaster and the login's sessions, with every interleaving of the steps that touch them.
/// A handshake has no time limit: once started it can stay unlisted for any number of steps
/// and finish at any step.
#[cfg(test)]
mod model {
    /// Ticks after which [`Design::ClearedByAge`] clears a cut-off.
    const RETENTION: u64 = 100;

    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    enum Verifier {
        /// Set on the login by someone other than the worker.
        Replaced,
        /// The branch's stored verifier, which the lockdown sets back.
        Stored,
    }

    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    enum Record {
        None,
        Pending,
        Cutoff(u64),
    }

    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    enum Phase {
        Idle,
        /// The handshake began: `backend_start` is fixed, the session isn't listed yet.
        Started,
        /// The handshake read the catalog's verifier.
        Read(Verifier),
        /// Listed under the login, authenticated against this verifier.
        Open(Verifier),
        Refused,
        /// Ended by a lockdown.
        Ended,
        /// Its postmaster restarted.
        Gone,
    }

    #[derive(Clone, Copy, Debug)]
    struct Client {
        /// The verifier its password matches.
        knows: Verifier,
        backend_start: u64,
        phase: Phase,
    }

    /// Which lockdown is modelled.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    enum Design {
        /// "This run found drift" lives in the worker's session only.
        SessionSetting,
        /// The record, cleared by a lockdown without drift once its cut-off is older than
        /// [`RETENTION`].
        ClearedByAge,
        /// The rules at the top of this file.
        DurableRecord,
    }

    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    enum Step {
        /// The lockdown block: reads the drift, ends the login's sessions when drifted, and
        /// leaves the verifier and the record uncommitted.
        Lockdown,
        Commit,
        /// The termination after the commit that reads the session setting.
        EndAgain,
        /// [`super::finish_sql`].
        Finish,
        /// The worker's session is lost: what it hadn't committed is gone, as is its setting.
        Lose,
        HealthCheck,
        HandshakeStart(usize),
        VerifierRead(usize),
        HandshakeFinish(usize),
        /// The postmaster restarts: every session and every handshake in flight is gone, the
        /// worker's included.
        PostmasterRestart,
        /// More than [`RETENTION`] passes on the same postmaster. Nothing else changes:
        /// handshakes in flight stay in flight.
        Wait,
    }

    #[derive(Clone, Copy, Debug)]
    struct World {
        design: Design,
        now: u64,
        /// When the running postmaster started.
        postmaster_start: u64,
        verifier: Verifier,
        record: Record,
        clients: [Client; 2],
        /// The open transaction's verifier and record.
        uncommitted: Option<Record>,
        /// The worker session's "this run found drift".
        setting: bool,
        /// Sessions under the stored verifier that a lockdown ended.
        current_ended: u32,
        /// Whether any health check answered `f`.
        unhealthy_seen: bool,
        /// Whether a lockdown cleared a cut-off that was not from before the postmaster's
        /// start: one that could still cover a session.
        cleared_on_its_postmaster: bool,
    }

    impl World {
        fn void_sessions(&self) -> bool {
            match self.record {
                Record::None => false,
                Record::Pending => true,
                Record::Cutoff(cutoff) => self
                    .clients
                    .iter()
                    .any(|c| matches!(c.phase, Phase::Open(_)) && c.backend_start <= cutoff),
            }
        }

        /// The login's drift predicate: the catalog's part, and rule 3.
        fn drifted(&self) -> bool {
            self.verifier != Verifier::Stored
                || (self.design != Design::SessionSetting && self.void_sessions())
        }

        fn healthy(&self) -> bool {
            !self.drifted()
        }

        /// Whether a lockdown that finds no drift clears a record with this cut-off (rule 6).
        fn clears(&self, cutoff: u64) -> bool {
            match self.design {
                Design::SessionSetting => false,
                Design::ClearedByAge => cutoff + RETENTION < self.now,
                Design::DurableRecord => cutoff < self.postmaster_start,
            }
        }

        fn end(&mut self, which: impl Fn(&Client) -> bool) {
            for i in 0..self.clients.len() {
                let c = self.clients[i];
                if let Phase::Open(under) = c.phase
                    && which(&c)
                {
                    if under == Verifier::Stored {
                        self.current_ended += 1;
                    }
                    self.clients[i].phase = Phase::Ended;
                }
            }
        }

        fn apply(&mut self, step: Step) {
            self.now += 1;
            match step {
                Step::Lockdown => {
                    let drifted = self.drifted();
                    self.setting = drifted;
                    let mut record = self.record;
                    if drifted {
                        self.end(|_| true);
                        record = Record::Pending;
                    } else if let Record::Cutoff(cutoff) = record
                        && self.clears(cutoff)
                    {
                        self.cleared_on_its_postmaster |= cutoff >= self.postmaster_start;
                        record = Record::None;
                    }
                    if self.design == Design::SessionSetting {
                        record = self.record;
                    }
                    self.uncommitted = Some(record);
                }
                Step::Commit => {
                    if let Some(record) = self.uncommitted.take() {
                        self.verifier = Verifier::Stored;
                        self.record = record;
                    }
                }
                Step::EndAgain => {
                    if self.setting {
                        self.end(|_| true);
                    }
                }
                Step::Finish => {
                    if self.design != Design::SessionSetting && self.record != Record::None {
                        let cutoff = match self.record {
                            Record::Cutoff(cutoff) => cutoff,
                            _ => self.now,
                        };
                        self.record = Record::Cutoff(cutoff);
                        self.end(|c| c.backend_start <= cutoff);
                    }
                }
                Step::Lose => {
                    self.uncommitted = None;
                    self.setting = false;
                }
                Step::HealthCheck => self.unhealthy_seen |= !self.healthy(),
                Step::HandshakeStart(i) => {
                    self.clients[i].backend_start = self.now;
                    self.clients[i].phase = Phase::Started;
                }
                Step::VerifierRead(i) => {
                    if self.clients[i].phase == Phase::Started {
                        self.clients[i].phase = Phase::Read(self.verifier);
                    }
                }
                Step::HandshakeFinish(i) => {
                    if let Phase::Read(read) = self.clients[i].phase {
                        self.clients[i].phase = if read == self.clients[i].knows {
                            Phase::Open(read)
                        } else {
                            Phase::Refused
                        };
                    }
                }
                Step::PostmasterRestart => {
                    self.postmaster_start = self.now;
                    for c in &mut self.clients {
                        if matches!(c.phase, Phase::Started | Phase::Read(_) | Phase::Open(_)) {
                            c.phase = Phase::Gone;
                        }
                    }
                    self.uncommitted = None;
                    self.setting = false;
                }
                Step::Wait => self.now += RETENTION + 1,
            }
        }

        fn bootstrap(&mut self) {
            for step in BOOTSTRAP {
                self.apply(step);
            }
        }

        fn open_under_a_replaced_verifier(&self) -> bool {
            self.clients
                .iter()
                .any(|c| matches!(c.phase, Phase::Open(v) if v != self.verifier))
        }
    }

    const BOOTSTRAP: [Step; 4] = [Step::Lockdown, Step::Commit, Step::EndAgain, Step::Finish];

    /// The worker's programs: a first bootstrap that runs whole or loses its session after
    /// any of its steps, then a health check, then (or not) a next bootstrap.
    fn worker_programs() -> Vec<Vec<Step>> {
        let mut programs = Vec::new();
        for reached in 1..=BOOTSTRAP.len() {
            for next_bootstrap in [false, true] {
                let mut program = BOOTSTRAP[..reached].to_vec();
                if reached < BOOTSTRAP.len() {
                    program.push(Step::Lose);
                }
                program.push(Step::HealthCheck);
                if next_bootstrap {
                    program.extend(BOOTSTRAP);
                }
                programs.push(program);
            }
        }
        programs
    }

    fn handshake(i: usize) -> Vec<Step> {
        vec![
            Step::HandshakeStart(i),
            Step::VerifierRead(i),
            Step::HandshakeFinish(i),
        ]
    }

    /// Runs `check` on the world after every interleaving of `threads` (each thread's steps
    /// stay in order); the number of interleavings, or the first one `check` refuses.
    fn interleavings(
        start: World,
        threads: &[Vec<Step>],
        check: &impl Fn(World) -> Result<(), String>,
    ) -> Result<u64, String> {
        fn go(
            world: World,
            threads: &[Vec<Step>],
            at: &mut Vec<usize>,
            trace: &mut Vec<Step>,
            check: &impl Fn(World) -> Result<(), String>,
        ) -> Result<u64, String> {
            let mut count = 0;
            let mut any = false;
            for t in 0..threads.len() {
                if at[t] == threads[t].len() {
                    continue;
                }
                any = true;
                let step = threads[t][at[t]];
                let mut next = world;
                next.apply(step);
                at[t] += 1;
                trace.push(step);
                count += go(next, threads, at, trace, check)?;
                trace.pop();
                at[t] -= 1;
            }
            if !any {
                check(world).map_err(|e| format!("{e} after {trace:?}"))?;
                count = 1;
            }
            Ok(count)
        }
        go(
            start,
            threads,
            &mut vec![0; threads.len()],
            &mut Vec::new(),
            check,
        )
    }

    /// A world at tick 1000 whose postmaster started at `postmaster_start`.
    fn world(
        design: Design,
        verifier: Verifier,
        record: Record,
        postmaster_start: u64,
        clients: [Client; 2],
    ) -> World {
        World {
            design,
            now: 1000,
            postmaster_start,
            verifier,
            record,
            clients,
            uncommitted: None,
            setting: false,
            current_ended: 0,
            unhealthy_seen: false,
            cleared_on_its_postmaster: false,
        }
    }

    fn idle(knows: Verifier) -> Client {
        Client {
            knows,
            backend_start: 0,
            phase: Phase::Idle,
        }
    }

    /// The guarantee, checked once every step of an interleaving has run: a health check,
    /// and the bootstrap it asks for, leave no session open under a replaced verifier and a
    /// healthy login. Then a new session under the stored verifier stays open through a
    /// routine bootstrap, which leaves the record as it is however old; and after a
    /// postmaster restart a routine bootstrap clears the record.
    fn settles(mut w: World) -> Result<(), String> {
        if !w.healthy() {
            w.bootstrap();
        }
        if w.open_under_a_replaced_verifier() {
            return Err(format!(
                "a session under a replaced verifier is open: {w:?}"
            ));
        }
        if !w.healthy() {
            return Err(format!("unhealthy after the bootstrap: {w:?}"));
        }
        if w.design != Design::DurableRecord {
            return Ok(());
        }
        w.clients[1] = idle(Verifier::Stored);
        for step in handshake(1) {
            w.apply(step);
        }
        // A routine bootstrap clears only a cut-off from before the postmaster started.
        let kept = match w.record {
            Record::Cutoff(cutoff) if cutoff < w.postmaster_start => Record::None,
            record => record,
        };
        let ended = w.current_ended;
        w.apply(Step::Wait);
        w.bootstrap();
        if w.clients[1].phase != Phase::Open(Verifier::Stored)
            || w.current_ended != ended
            || !w.healthy()
        {
            return Err(format!(
                "a routine bootstrap ended a current session: {w:?}"
            ));
        }
        if w.record != kept {
            return Err(format!(
                "a routine bootstrap on the same postmaster changed the record: {w:?}"
            ));
        }
        w.apply(Step::PostmasterRestart);
        w.bootstrap();
        if w.record != Record::None || !w.healthy() {
            return Err(format!(
                "the record did not clear after a postmaster restart: {w:?}"
            ));
        }
        Ok(())
    }

    /// Every interleaving of: a handshake under the replaced verifier, a handshake under the
    /// stored one, a postmaster restart, and each worker program, from a login whose verifier
    /// was replaced (with no record, or a cut-off from long before on the same postmaster).
    fn replaced_verifier_interleavings(design: Design) -> Result<u64, String> {
        let mut total = 0;
        for record in [Record::None, Record::Cutoff(1)] {
            let start = world(
                design,
                Verifier::Replaced,
                record,
                0,
                [idle(Verifier::Replaced), idle(Verifier::Stored)],
            );
            for program in worker_programs() {
                let threads = [
                    program,
                    handshake(0),
                    handshake(1),
                    vec![Step::PostmasterRestart],
                ];
                total += interleavings(start, &threads, &settles)?;
            }
        }
        Ok(total)
    }

    /// Every interleaving of: a handshake under the replaced verifier that can finish at any
    /// step, a long wait on the same postmaster, and each worker program followed by up to
    /// three more routine bootstraps.
    fn long_handshake_interleavings(design: Design) -> Result<u64, String> {
        let start = world(
            design,
            Verifier::Replaced,
            Record::None,
            0,
            [idle(Verifier::Replaced), idle(Verifier::Stored)],
        );
        let mut total = 0;
        for program in worker_programs() {
            for routine in 0..=3 {
                let mut program = program.clone();
                for _ in 0..routine {
                    program.extend(BOOTSTRAP);
                }
                let threads = [program, handshake(0), vec![Step::Wait]];
                total += interleavings(start, &threads, &settles)?;
            }
        }
        Ok(total)
    }

    /// Rules 1 to 4 and 6: whatever the order of the handshake's start, its verifier read
    /// and its finish, the lockdown's record, commit and terminations, the loss of the
    /// worker's session, a postmaster restart, the health check and the next bootstrap, no
    /// session authenticated against a replaced verifier is open after a health check and
    /// the bootstrap it asks for.
    #[test]
    fn no_interleaving_keeps_a_session_under_a_replaced_verifier() {
        let checked = replaced_verifier_interleavings(Design::DurableRecord)
            .unwrap_or_else(|e| panic!("{e}"));
        println!("replaced verifier: {checked} interleavings");
        assert_eq!(checked, 9_738_680);
    }

    /// Rule 6: a handshake that read the replaced verifier stays covered however long it
    /// stays unlisted and however many routine bootstraps run before it finishes.
    #[test]
    fn a_handshake_that_finishes_after_any_number_of_routine_bootstraps_is_still_covered() {
        let checked =
            long_handshake_interleavings(Design::DurableRecord).unwrap_or_else(|e| panic!("{e}"));
        println!("long handshake: {checked} interleavings");
        assert_eq!(checked, LONG_HANDSHAKE_INTERLEAVINGS);
    }

    const LONG_HANDSHAKE_INTERLEAVINGS: u64 = 427_680;

    /// The model tells the designs apart: with the drift kept only in the worker's session,
    /// some interleaving leaves such a session open with a healthy answer.
    #[test]
    fn the_model_refuses_a_lockdown_that_keeps_the_drift_in_its_session() {
        let refused = replaced_verifier_interleavings(Design::SessionSetting)
            .expect_err("an interleaving it refuses");
        assert!(
            refused.contains("a session under a replaced verifier is open"),
            "{refused}"
        );
    }

    /// The model tells the clearing rules apart: with the record cleared by its age, a
    /// handshake that finishes after the record cleared leaves such a session open with a
    /// healthy answer.
    #[test]
    fn the_model_refuses_a_record_cleared_by_its_age() {
        let refused = long_handshake_interleavings(Design::ClearedByAge)
            .expect_err("an interleaving it refuses");
        assert!(
            refused.contains("a session under a replaced verifier is open")
                && refused.contains("record: None"),
            "{refused}"
        );
    }

    /// Rules 5 and 6: with the stored verifier in the catalog and no void session, no
    /// interleaving of a new handshake, a long wait, a postmaster restart and the worker's
    /// programs has a lockdown end a session or a health check answer `f`, and the record
    /// changes only when a lockdown clears a cut-off from before the postmaster started. Checked with no
    /// record, a cut-off from long before, and one just before the open session, on the same
    /// postmaster and (the cut-off from long before) on a later one.
    #[test]
    fn without_drift_no_interleaving_ends_a_session() {
        let mut total = 0;
        for (record, postmaster_start) in [
            (Record::None, 0),
            (Record::Cutoff(1), 0),
            (Record::Cutoff(998), 0),
            (Record::Cutoff(1), 500),
        ] {
            let open = Client {
                knows: Verifier::Stored,
                backend_start: 999,
                phase: Phase::Open(Verifier::Stored),
            };
            let start = world(
                Design::DurableRecord,
                Verifier::Stored,
                record,
                postmaster_start,
                [open, idle(Verifier::Stored)],
            );
            for program in worker_programs() {
                let threads = [
                    program,
                    handshake(1),
                    vec![Step::Wait],
                    vec![Step::PostmasterRestart],
                ];
                total += interleavings(start, &threads, &|w: World| {
                    let none_ended = w
                        .clients
                        .iter()
                        .all(|c| matches!(c.phase, Phase::Open(Verifier::Stored) | Phase::Gone));
                    if w.current_ended != 0 || w.unhealthy_seen || !w.healthy() || !none_ended {
                        return Err(format!(
                            "a session ended or the check answered f without drift: {w:?}"
                        ));
                    }
                    let cleared = w.record == Record::None && !w.cleared_on_its_postmaster;
                    if w.record != record && !cleared {
                        return Err(format!(
                            "the record changed on the same postmaster: {w:?}"
                        ));
                    }
                    Ok(())
                })
                .unwrap_or_else(|e| panic!("{e}"));
            }
        }
        println!("no drift: {total} interleavings");
        assert_eq!(total, NO_DRIFT_INTERLEAVINGS);
    }

    const NO_DRIFT_INTERLEAVINGS: u64 = 541_520;
}
