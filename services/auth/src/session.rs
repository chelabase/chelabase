//! The rules of a member session: its lifetime, the tenant a sign-in lands in, and how many
//! email codes a user can be sent.

use crate::app_users::internal::MemberTenants;
use crate::guard::{RateLimiter, Slot};
use chelabase_identity::CodeKind;
use chrono::{DateTime, Utc};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;
use uuid::Uuid;

/// The wall clock, which tests can move forward. A session's age is the one thing it measures.
#[derive(Clone, Default)]
pub struct Clock(Arc<AtomicI64>);

impl Clock {
    pub fn now(&self) -> DateTime<Utc> {
        Utc::now() + chrono::Duration::seconds(self.0.load(Ordering::Relaxed))
    }

    pub fn advance(&self, by: Duration) {
        self.0.fetch_add(by.as_secs() as i64, Ordering::Relaxed);
    }
}

/// How long a member session can mint tokens, counted from its `created_at`. Identity's own
/// expiry says the same (`session.expiresIn`); this holds even if a read there ever slid it.
pub const ABSOLUTE_LIFETIME: chrono::Duration = chrono::Duration::days(30);

/// Whether a session created at `created_at` is past [`ABSOLUTE_LIFETIME`] at `now`. Strict:
/// exactly 30 days old is still valid; a `created_at` after `now` is valid too.
pub fn past_absolute_lifetime(now: DateTime<Utc>, created_at: DateTime<Utc>) -> bool {
    now - created_at > ABSOLUTE_LIFETIME
}

/// Codes of one kind a user can be sent per hour. Identity bounds the guesses itself (a stored
/// failure budget per user and kind, and no new code within a minute of a live one); this bounds
/// the mail.
const CODES_PER_HOUR: u32 = 5;

/// Reserves one code of `kind` for `user`: `None` once the hour's are used. Every path that
/// issues a code asks this first, whatever address it came from. The slot counts unless it is
/// refunded, which only identity's `CodeRecentlySent` does (nothing was sent). Per auth process.
pub fn reserve_code_issue<'a>(limiter: &'a RateLimiter, kind: CodeKind, user_id: &str) -> Option<Slot<'a>> {
    Slot::reserve(
        limiter,
        format!("code:{}:{user_id}", kind.as_str()),
        CODES_PER_HOUR,
        Duration::from_secs(3600),
    )
}

/// Where a right password leads, from the person's memberships and the sign-in's `?tenant=`.
#[derive(Debug, PartialEq, Eq)]
pub enum Choice {
    Tenant(Uuid),
    /// Nothing active, and waiting for an admin (in the tenant asked for, if one was).
    Pending,
    /// Several active tenants and none asked for.
    Several(Vec<Uuid>),
    /// The generic refusal.
    Nowhere,
}

pub fn choose(tenants: &MemberTenants, wanted: Option<&str>) -> Choice {
    if let Some(raw) = wanted {
        return match Uuid::parse_str(raw) {
            Ok(t) if tenants.active.contains(&t) => Choice::Tenant(t),
            Ok(t) if tenants.pending.contains(&t) => Choice::Pending,
            _ => Choice::Nowhere,
        };
    }
    match tenants.active.as_slice() {
        [only] => Choice::Tenant(*only),
        [] if tenants.pending.is_empty() => Choice::Nowhere,
        [] => Choice::Pending,
        several => Choice::Several(several.to_vec()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_clock_offset_adds_whole_seconds() {
        let clock = Clock::default();
        clock.advance(Duration::from_secs(90));
        let before = Utc::now();
        let now = clock.now();
        let after = Utc::now();
        let ahead = chrono::Duration::seconds(90);
        assert!(before + ahead <= now && now <= after + ahead, "{before} {now} {after}");
        assert_eq!(ABSOLUTE_LIFETIME.num_seconds(), 2_592_000);
    }

    #[test]
    fn absolute_lifetime_edges_are_unchanged() {
        let now = Utc::now();
        let age = |secs: i64| now - chrono::Duration::seconds(secs);
        assert!(!past_absolute_lifetime(now, age(2_592_000)), "exactly 30 days is valid");
        assert!(past_absolute_lifetime(now, age(2_592_001)), "30 days + 1 s is refused");
        assert!(!past_absolute_lifetime(now, age(-5)), "created after now is valid");
        assert!(!past_absolute_lifetime(now, age(0)));
    }

    const A: Uuid = Uuid::from_u128(1);
    const B: Uuid = Uuid::from_u128(2);

    fn m(active: &[Uuid], pending: &[Uuid]) -> MemberTenants {
        MemberTenants {
            active: active.to_vec(),
            pending: pending.to_vec(),
        }
    }

    // Every shape of memberships against every shape of ask.
    #[test]
    fn choose_is_exhaustive() {
        let a = A.to_string();
        let b = B.to_string();
        let cases: [(MemberTenants, Option<&str>, Choice); 11] = [
            (m(&[], &[]), None, Choice::Nowhere),
            (m(&[], &[]), Some(&a), Choice::Nowhere),
            (m(&[A], &[]), None, Choice::Tenant(A)),
            (m(&[A], &[]), Some(&a), Choice::Tenant(A)),
            (m(&[A], &[]), Some(&b), Choice::Nowhere),
            (m(&[A], &[]), Some("junk"), Choice::Nowhere),
            (m(&[A], &[B]), None, Choice::Tenant(A)),
            (m(&[A], &[B]), Some(&b), Choice::Pending),
            (m(&[], &[B]), None, Choice::Pending),
            (m(&[], &[B]), Some(&a), Choice::Nowhere),
            (m(&[A, B], &[]), None, Choice::Several(vec![A, B])),
        ];
        for (tenants, wanted, want) in cases {
            assert_eq!(choose(&tenants, wanted), want, "{tenants:?} {wanted:?}");
        }
    }

    #[test]
    fn code_limits_are_per_user_and_kind() {
        let limiter = RateLimiter::default();
        for _ in 0..5 {
            assert!(reserve_code_issue(&limiter, CodeKind::Verify, "u1").is_some());
        }
        assert!(reserve_code_issue(&limiter, CodeKind::Verify, "u1").is_none());
        assert!(reserve_code_issue(&limiter, CodeKind::Reset, "u1").is_some());
        assert!(reserve_code_issue(&limiter, CodeKind::Verify, "u2").is_some());
    }

    // m9b AC 3: an issue identity turned away (a code sent under a minute ago) is given back.
    #[test]
    fn a_refunded_issue_spends_no_hourly_budget() {
        let limiter = RateLimiter::default();
        for _ in 0..20 {
            reserve_code_issue(&limiter, CodeKind::Reset, "u1").expect("a refunded issue frees its slot").refund();
        }
        for _ in 0..5 {
            assert!(reserve_code_issue(&limiter, CodeKind::Reset, "u1").is_some());
        }
        assert!(reserve_code_issue(&limiter, CodeKind::Reset, "u1").is_none());
    }
}
