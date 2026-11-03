//! RT1 T7: re-authorization (spec §4, R15, W19, W28, P13, S13): what each answer does to a
//! channel, three unavailable answers in a row, coalescing per member and the per-cycle cache.

use std::time::{Duration, Instant};

use realtime::orchestrator::{AuthTarget, AuthorizeOutcome, MemberInfo};
use realtime::protocol::ErrorCode;
use realtime::reauth::{Coalescer, CycleCache, MAX_UNAVAILABLE, Offer, ReauthAction, ReauthState};
use uuid::Uuid;

fn allowed() -> AuthorizeOutcome {
    AuthorizeOutcome::Allowed {
        tenant_id: Uuid::from_u128(1),
        member: MemberInfo {
            sub: "ana".into(),
            name: None,
            email: None,
        },
    }
}

#[test]
fn only_forbidden_and_not_found_leave() {
    let mut s = ReauthState::new();
    assert_eq!(s.on_outcome(&allowed()), ReauthAction::Keep);
    assert_eq!(
        s.on_outcome(&AuthorizeOutcome::TokenExpired),
        ReauthAction::AwaitRenewal
    );
    assert_eq!(
        s.on_outcome(&AuthorizeOutcome::Unavailable),
        ReauthAction::Degrade
    );
    assert!(s.degraded());
    assert_eq!(
        s.on_outcome(&AuthorizeOutcome::Forbidden),
        ReauthAction::Leave(ErrorCode::Forbidden)
    );
    let mut s = ReauthState::new();
    assert_eq!(
        s.on_outcome(&AuthorizeOutcome::NotFound),
        ReauthAction::Leave(ErrorCode::NotFound)
    );
}

#[test]
fn the_third_unavailable_in_a_row_leaves_with_unavailable() {
    assert_eq!(MAX_UNAVAILABLE, 3);
    let mut s = ReauthState::new();
    assert_eq!(
        s.on_outcome(&AuthorizeOutcome::Unavailable),
        ReauthAction::Degrade
    );
    assert_eq!(
        s.on_outcome(&AuthorizeOutcome::Unavailable),
        ReauthAction::Degrade
    );
    assert_eq!(
        s.on_outcome(&AuthorizeOutcome::Unavailable),
        ReauthAction::Leave(ErrorCode::Unavailable)
    );
}

#[test]
fn an_allowed_answer_resets_the_run_and_ends_degradation() {
    let mut s = ReauthState::new();
    s.on_outcome(&AuthorizeOutcome::Unavailable);
    s.on_outcome(&AuthorizeOutcome::Unavailable);
    assert_eq!(s.on_outcome(&allowed()), ReauthAction::Keep);
    assert!(!s.degraded());
    assert_eq!(
        s.on_outcome(&AuthorizeOutcome::Unavailable),
        ReauthAction::Degrade,
        "a new run"
    );
    assert_eq!(
        s.on_outcome(&AuthorizeOutcome::Unavailable),
        ReauthAction::Degrade
    );
}

#[test]
fn token_expired_neither_counts_nor_resets_the_run() {
    let mut s = ReauthState::new();
    s.on_outcome(&AuthorizeOutcome::Unavailable);
    s.on_outcome(&AuthorizeOutcome::Unavailable);
    assert_eq!(
        s.on_outcome(&AuthorizeOutcome::TokenExpired),
        ReauthAction::AwaitRenewal
    );
    assert!(s.degraded(), "still degraded");
    assert_eq!(
        s.on_outcome(&AuthorizeOutcome::Unavailable),
        ReauthAction::Leave(ErrorCode::Unavailable)
    );
}

#[test]
fn an_access_event_runs_at_once_then_coalesces_for_a_second() {
    let t0 = Instant::now();
    let ms = Duration::from_millis;
    let mut c = Coalescer::new(Duration::from_secs(1));
    assert_eq!(
        c.offer("ana", t0),
        Offer::RunNow,
        "within 1 s of the event: at once"
    );
    assert_eq!(c.offer("ana", t0 + ms(100)), Offer::Coalesced);
    assert_eq!(c.offer("ana", t0 + ms(200)), Offer::Coalesced);
    assert_eq!(c.offer("bob", t0 + ms(200)), Offer::RunNow, "per member");
    assert!(c.due(t0 + ms(999)).is_empty());
    assert_eq!(
        c.due(t0 + ms(1000)),
        vec!["ana"],
        "one trailing run for the burst"
    );
    assert!(c.due(t0 + ms(1500)).is_empty(), "once");
    // The trailing run started a new window.
    assert_eq!(c.offer("ana", t0 + ms(1500)), Offer::Coalesced);
    assert_eq!(c.due(t0 + ms(2000)), vec!["ana"]);
    assert_eq!(
        c.offer("ana", t0 + ms(3500)),
        Offer::RunNow,
        "a quiet window later"
    );
}

#[test]
fn a_lone_event_has_no_trailing_run() {
    let t0 = Instant::now();
    let mut c = Coalescer::new(Duration::from_secs(1));
    assert_eq!(c.offer(7u32, t0), Offer::RunNow);
    assert!(c.due(t0 + Duration::from_secs(5)).is_empty());
    assert_eq!(c.offer(7u32, t0 + Duration::from_secs(5)), Offer::RunNow);
}

fn target(role: Option<&str>) -> AuthTarget {
    AuthTarget {
        database_id: Uuid::from_u128(1),
        branch_id: Uuid::from_u128(2),
        pg_database: "app".into(),
        role: role.map(str::to_string),
    }
}

#[test]
fn answers_are_cached_per_member_and_target_for_one_cycle() {
    let mut cache = CycleCache::new();
    assert!(cache.is_empty());
    cache.put("ana", "s1", &target(None), 7, allowed());
    assert_eq!(cache.get("ana", "s1", &target(None), 7), Some(allowed()));
    assert_eq!(cache.get("bob", "s1", &target(None), 7), None, "per member");
    // Review fix 1: another session of the member (one that may have logged out) asks itself.
    assert_eq!(cache.get("ana", "s2", &target(None), 7), None, "per session");
    assert_eq!(
        cache.get("ana", "s1", &target(Some("billing")), 7),
        None,
        "per target, role included"
    );
    assert_eq!(cache.get("ana", "s1", &target(None), 8), None, "per cycle");
    cache.put(
        "ana",
        "s1",
        &target(Some("billing")),
        8,
        AuthorizeOutcome::Forbidden,
    );
    assert_eq!(cache.len(), 1, "a new cycle drops the old answers");
    assert_eq!(cache.get("ana", "s1", &target(None), 7), None);
}
