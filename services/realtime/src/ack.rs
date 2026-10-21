//! Acknowledgements (spec §3.3 step 3, W8, P14): the flush LSN realtime confirms in its standby
//! status updates.
//!
//! - A transaction is confirmed only once the window holding it is published, and in order: the
//!   confirmed LSN never passes a transaction that isn't published yet.
//! - When nothing is in flight, a keepalive's `wal_end` is confirmed (a quiet watched database
//!   still advances `confirmed_flush_lsn` while other databases write, W8).
//! - The confirmed LSN never goes back.
//! - The lag watchdog: something in flight while `wal_end` is more than [`LAG_MAX_BYTES`] past
//!   the oldest in-flight transaction, or that transaction older than [`LAG_MAX_AGE`] → drop the
//!   slot and recreate it (watchers get `resync`).

use std::collections::VecDeque;
use std::time::Duration;

use tokio::time::Instant;
use tokio_postgres::types::PgLsn;

pub const LAG_MAX_BYTES: u64 = 64 << 20;
pub const LAG_MAX_AGE: Duration = Duration::from_secs(60);

pub struct Acks {
    confirmed: PgLsn,
    /// Decoded, not yet published: (end LSN, when decoded), in decode (commit) order.
    in_flight: VecDeque<(PgLsn, Instant)>,
}

impl Acks {
    /// Starting at the slot's consistent point.
    pub fn new(start: PgLsn) -> Self {
        Self {
            confirmed: start,
            in_flight: VecDeque::new(),
        }
    }
    /// A transaction ending at `lsn` was decoded at `at` (in flight until published).
    pub fn received(&mut self, lsn: PgLsn, at: Instant) {
        self.in_flight.push_back((lsn, at));
    }
    /// Every transaction ending at or before `upto` is published.
    pub fn published(&mut self, upto: PgLsn) {
        while self.in_flight.front().is_some_and(|(lsn, _)| *lsn <= upto) {
            self.in_flight.pop_front();
        }
        if upto > self.confirmed {
            self.confirmed = upto;
        }
    }
    /// A keepalive's `wal_end`.
    pub fn keepalive(&mut self, wal_end: PgLsn) {
        if self.in_flight.is_empty() && wal_end > self.confirmed {
            self.confirmed = wal_end;
        }
    }
    /// The flush LSN to confirm.
    pub fn confirmed(&self) -> PgLsn {
        self.confirmed
    }
    /// Transactions decoded and not yet published.
    pub fn in_flight(&self) -> usize {
        self.in_flight.len()
    }
    /// The watchdog: whether the slot must be dropped and recreated.
    pub fn lagging(&self, wal_end: PgLsn, now: Instant) -> bool {
        let Some((oldest, at)) = self.in_flight.front() else {
            return false;
        };
        u64::from(wal_end).saturating_sub(u64::from(*oldest)) > LAG_MAX_BYTES
            || now.saturating_duration_since(*at) > LAG_MAX_AGE
    }
}
