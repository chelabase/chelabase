//! The rules of a member session: its lifetime, the tenant a sign-in lands in, and how many
//! email codes a user can be sent.

use crate::app_users::internal::MemberTenants;
use crate::guard::RateLimiter;
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

/// Codes of one kind a user can be sent per hour. A new code resets the guess counter (5 wrong
/// guesses lock a code), so this is what bounds the guesses: 25 an hour per user and kind.
const CODES_PER_HOUR: u32 = 5;

/// Counts one code of `kind` for `user`: `false` once the hour's are used. Every path that
/// issues a code asks this first, whatever address it came from. Per auth process.
pub fn may_issue_code(limiter: &RateLimiter, kind: CodeKind, user_id: &str) -> bool {
    limiter.allow(
        &format!("code:{}:{user_id}", kind.as_str()),
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
            assert!(may_issue_code(&limiter, CodeKind::Verify, "u1"));
        }
        assert!(!may_issue_code(&limiter, CodeKind::Verify, "u1"));
        assert!(may_issue_code(&limiter, CodeKind::Reset, "u1"));
        assert!(may_issue_code(&limiter, CodeKind::Verify, "u2"));
    }
}
