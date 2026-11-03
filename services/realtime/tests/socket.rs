//! RT1 T7: the ticket route, the upgrade (ticket checked before the 101, subprotocol), the
//! first `auth` frame (R9, S14), renewals (W32), frame limits, pings, sockets per member across
//! instances (P16) and shutdown (1012). In-process realtime with fakes (`common`).

// Test code: plain clients against local test servers (clippy.toml allows tests only).
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

mod common;

use std::time::{Duration, Instant};

use common::*;
use realtime::protocol::{SUBPROTOCOL, close};
use serde_json::json;

// --- The ticket route ----------------------------------------------------------------------

#[tokio::test]
async fn a_ticket_is_201_no_store_and_expires_in_30() {
    let h = Harness::start().await;
    let (status, headers, body) = h.post_ticket(h.rt(), Some(&h.identity("ana", TENANT))).await;
    assert_eq!(status, 201, "{body}");
    assert_eq!(
        headers.get("cache-control").and_then(|v| v.to_str().ok()),
        Some("no-store")
    );
    assert_eq!(body["expires_in"], 30);
    let ticket = body["ticket"].as_str().unwrap();
    assert_eq!(ticket.len(), 43);
    // Stored once, by hash (the store also holds P16/W35's `rt:cnt:*` rate counters).
    let tickets: Vec<String> = h.store.keys().into_iter().filter(|k| k.starts_with("rt:ticket:")).collect();
    let want = realtime::ticket::store_key(&realtime::ticket::ticket_hash(ticket));
    assert_eq!(tickets, vec![want], "stored once, by hash");
}

#[tokio::test]
async fn the_ticket_route_needs_the_gateways_identity_for_realtime() {
    let h = Harness::start().await;
    assert_eq!(h.post_ticket(h.rt(), None).await.0, 401, "no identity");
    assert_eq!(
        h.post_ticket(h.rt(), Some("not-a-token")).await.0,
        401,
        "a bad identity"
    );
    // An identity for another service (here: a token signed by another key pair) is refused.
    let other = Harness::start().await;
    assert_eq!(
        h.post_ticket(h.rt(), Some(&other.identity("ana", TENANT)))
            .await
            .0,
        401,
        "another signer"
    );
    assert!(h.store.keys().is_empty());
}

#[tokio::test]
async fn valkey_down_means_503_for_tickets() {
    let h = Harness::start().await;
    h.store.fail(true);
    assert_eq!(
        h.post_ticket(h.rt(), Some(&h.identity("ana", TENANT))).await.0,
        503
    );
}

#[tokio::test]
async fn tickets_are_rate_limited_per_member() {
    // W35: 10 per member per second (a burst of 25 spans at most two seconds).
    let h = Harness::start().await;
    // At once: each ticket also asks the orchestrator (T8-1), so a sequential burst can spread
    // over more seconds than the window under load.
    let identity = h.identity("ana", TENANT);
    let statuses: Vec<u16> = futures_util::future::join_all(
        (0..25).map(|_| h.post_ticket(h.rt(), Some(&identity))),
    )
    .await
    .into_iter()
    .map(|r| r.0)
    .collect();
    assert!(statuses.contains(&429), "{statuses:?}");
    assert!(
        statuses.iter().filter(|s| **s == 201).count() <= 20,
        "{statuses:?}"
    );
    assert_eq!(
        h.post_ticket(h.rt(), Some(&h.identity("bob", TENANT))).await.0,
        201,
        "per member"
    );
}

// --- The upgrade ---------------------------------------------------------------------------

#[tokio::test]
async fn the_socket_needs_a_ticket_before_the_101() {
    let h = Harness::start().await;
    assert_eq!(
        Harness::try_open(h.rt(), "", Some(SUBPROTOCOL)).await.err(),
        Some(401),
        "no ticket"
    );
    assert_eq!(
        Harness::try_open(h.rt(), &realtime::ticket::new_ticket(), Some(SUBPROTOCOL))
            .await
            .err(),
        Some(401),
        "an unknown ticket"
    );
    let ticket = h.ticket("ana").await;
    let ws = Harness::open(h.rt(), &ticket).await;
    assert_eq!(
        ws.protocol.as_deref(),
        Some(SUBPROTOCOL),
        "the subprotocol is echoed"
    );
    assert_eq!(
        Harness::try_open(h.rt(), &ticket, Some(SUBPROTOCOL))
            .await
            .err(),
        Some(401),
        "a replay"
    );
}

#[tokio::test]
async fn the_subprotocol_is_required() {
    let h = Harness::start().await;
    let ticket = h.ticket("ana").await;
    assert_eq!(
        Harness::try_open(h.rt(), &ticket, None).await.err(),
        Some(400),
        "none"
    );
    let ticket = h.ticket("ana").await;
    assert_eq!(
        Harness::try_open(h.rt(), &ticket, Some("chelabase.realtime.v2"))
            .await
            .err(),
        Some(400),
        "an unknown version"
    );
    let ticket = h.ticket("ana").await;
    let ws = Harness::try_open(
        h.rt(),
        &ticket,
        Some("chelabase.realtime.v2, chelabase.realtime.v1"),
    )
    .await;
    assert_eq!(
        ws.expect("v1 among several").protocol.as_deref(),
        Some(SUBPROTOCOL)
    );
}

#[tokio::test]
async fn a_tampered_ticket_value_is_refused() {
    let h = Harness::start().await;
    let ticket = h.ticket("ana").await;
    // W15: a value that doesn't open under this ticket (the TTL is the ticket unit tests').
    for k in h.store.keys().into_iter().filter(|k| k.starts_with("rt:ticket:")) {
        h.store.set_raw(&k, b"gone".to_vec());
    }
    assert_eq!(
        Harness::try_open(h.rt(), &ticket, Some(SUBPROTOCOL))
            .await
            .err(),
        Some(401)
    );
}

#[tokio::test]
async fn valkey_down_means_503_for_the_upgrade() {
    let h = Harness::start().await;
    let ticket = h.ticket("ana").await;
    h.store.fail(true);
    assert_eq!(
        Harness::try_open(h.rt(), &ticket, Some(SUBPROTOCOL))
            .await
            .err(),
        Some(503)
    );
}

// #22: the internal health check is readiness too: 503 while the OIDC verifier can't check
// tokens (its audiences aren't written yet), 200 once it can.
#[tokio::test]
async fn health_waits_for_the_token_audiences() {
    let h = Harness::start().await;
    let url = format!("http://{}/health", h.rt().internal_addr());
    h.verifier.set_not_ready(true);
    assert_eq!(reqwest::get(&url).await.unwrap().status().as_u16(), 503);
    h.verifier.set_not_ready(false);
    assert_eq!(reqwest::get(&url).await.unwrap().status().as_u16(), 200);
}

#[tokio::test]
async fn the_internal_listener_serves_health_only() {
    let h = Harness::start().await;
    let base = format!("http://{}", h.rt().internal_addr());
    let health = reqwest::get(format!("{base}/health")).await.unwrap();
    assert_eq!(health.status().as_u16(), 200);
    let tickets = reqwest::Client::new()
        .post(format!("{base}/v1/realtime/tickets"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        tickets.status().as_u16(),
        404,
        "the ticket route is the public listener's"
    );
}

// --- The first frame -----------------------------------------------------------------------

#[tokio::test]
async fn the_first_frame_must_be_auth_within_the_timeout() {
    let h = Harness::start_with(|c| c.timings.auth_timeout = Duration::from_millis(300)).await;
    let mut ws = Harness::open(h.rt(), &h.ticket("ana").await).await;
    let started = Instant::now();
    assert_eq!(ws.closed().await, close::TOKEN_EXPIRED);
    assert!(
        started.elapsed() >= Duration::from_millis(250),
        "{:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn a_join_before_auth_closes_4001() {
    let h = Harness::start().await;
    let mut ws = Harness::open(h.rt(), &h.ticket("ana").await).await;
    ws.send(json!({"type": "join", "ref": "1", "channel": project_id(PROJECT)}))
        .await;
    assert_eq!(ws.closed().await, close::TOKEN_EXPIRED);
}

#[tokio::test]
async fn an_auth_frame_for_another_person_or_tenant_closes_4003() {
    let h = Harness::start().await;
    let mut ws = Harness::open(h.rt(), &h.ticket("ana").await).await;
    ws.send(json!({"type": "auth", "token": token("bob", 3600)}))
        .await;
    assert_eq!(ws.closed().await, close::REVOKED);
    let mut ws = Harness::open(h.rt(), &h.ticket("ana").await).await;
    ws.send(json!({"type": "auth", "token": token_in("ana", uuid::Uuid::from_u128(2), 3600)}))
        .await;
    assert_eq!(ws.closed().await, close::REVOKED, "same sub, other tenant");
}

#[tokio::test]
async fn an_expired_or_invalid_first_token_closes_4001() {
    let h = Harness::start().await;
    let mut ws = Harness::open(h.rt(), &h.ticket("ana").await).await;
    ws.send(json!({"type": "auth", "token": token("ana", -5)}))
        .await;
    assert_eq!(
        ws.closed().await,
        close::TOKEN_EXPIRED,
        "exp in the past (no leeway)"
    );
    let mut ws = Harness::open(h.rt(), &h.ticket("ana").await).await;
    ws.send(json!({"type": "auth", "token": token("ana", 0)}))
        .await;
    assert_eq!(ws.closed().await, close::TOKEN_EXPIRED, "exp = now");
    let mut ws = Harness::open(h.rt(), &h.ticket("ana").await).await;
    ws.send(json!({"type": "auth", "token": "garbage"})).await;
    assert_eq!(ws.closed().await, close::TOKEN_EXPIRED);
}

#[tokio::test]
async fn a_good_auth_frame_lets_the_socket_work() {
    let h = Harness::start().await;
    let mut ws = h.authed("ana").await;
    let joined = ws.join_project("1", PROJECT).await;
    assert_eq!(joined["type"], "joined", "{joined}");
}

// --- Renewal and exp -----------------------------------------------------------------------

#[tokio::test]
async fn the_socket_closes_4001_at_exp_without_a_renewal() {
    let h = Harness::start().await;
    let mut ws = Harness::open(h.rt(), &h.ticket("ana").await).await;
    ws.send(json!({"type": "auth", "token": token("ana", 2)}))
        .await;
    let started = Instant::now();
    assert_eq!(
        ws.closed_within(Duration::from_secs(6)).await,
        close::TOKEN_EXPIRED
    );
    let took = started.elapsed();
    assert!(
        took >= Duration::from_millis(900) && took <= Duration::from_millis(3500),
        "{took:?}"
    );
}

#[tokio::test]
async fn a_renewal_moves_the_deadline() {
    let h = Harness::start().await;
    let mut ws = Harness::open(h.rt(), &h.ticket("ana").await).await;
    ws.send(json!({"type": "auth", "token": token("ana", 2)}))
        .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    ws.send(json!({"type": "auth", "token": token("ana", 3600)}))
        .await;
    tokio::time::sleep(Duration::from_millis(3000)).await;
    let joined = ws.join_project("1", PROJECT).await;
    assert_eq!(
        joined["type"], "joined",
        "still open past the first exp: {joined}"
    );
}

#[tokio::test]
async fn a_renewal_for_another_person_closes_4003() {
    let h = Harness::start().await;
    let mut ws = h.authed("ana").await;
    ws.send(json!({"type": "auth", "token": token("bob", 3600)}))
        .await;
    assert_eq!(ws.closed().await, close::REVOKED);
}

#[tokio::test]
async fn a_renewal_that_fails_verification_closes_4001() {
    // W32 (server side): a failed renewal ends the socket at once.
    let h = Harness::start().await;
    let mut ws = h.authed("ana").await;
    assert_eq!(ws.join_project("1", PROJECT).await["type"], "joined");
    ws.send(json!({"type": "auth", "token": "garbage"})).await;
    assert_eq!(ws.closed().await, close::TOKEN_EXPIRED);
}

#[tokio::test]
async fn an_expired_renewal_closes_4001() {
    let h = Harness::start().await;
    let mut ws = h.authed("ana").await;
    ws.send(json!({"type": "auth", "token": token("ana", -1)}))
        .await;
    assert_eq!(ws.closed().await, close::TOKEN_EXPIRED);
}

// --- App tickets (R9) ----------------------------------------------------------------------

#[tokio::test]
async fn an_app_ticket_sending_auth_is_refused() {
    let h = Harness::start().await;
    let mut ws = Harness::open(h.rt(), &h.app_ticket(uuid::Uuid::new_v4()).await).await;
    ws.send(json!({"type": "auth", "token": token("ana", 3600)}))
        .await;
    assert_eq!(ws.closed().await, close::REVOKED);
}

#[tokio::test]
async fn an_app_ticket_may_not_join_table_or_project_channels() {
    let h = Harness::start().await;
    let mut ws = Harness::open(h.rt(), &h.app_ticket(uuid::Uuid::new_v4()).await).await;
    let reply = ws.join_table("1", "items", None).await;
    assert_eq!(
        (reply["type"].as_str(), reply["code"].as_str()),
        (Some("error"), Some("forbidden")),
        "{reply}"
    );
    let reply = ws.join_project("2", PROJECT).await;
    assert_eq!(reply["code"], "forbidden", "{reply}");
    assert!(
        h.orch.authorize_calls().is_empty() && h.orch.projects_calls().is_empty(),
        "refused before any call"
    );
}

#[tokio::test]
async fn a_studio_ticket_may_not_join_app_channels() {
    let h = Harness::start().await;
    let mut ws = h.authed("ana").await;
    ws.send(json!({"type": "join", "ref": "1", "channel": "app:room"}))
        .await;
    let reply = ws.reply("1").await;
    assert_eq!(
        (reply["type"].as_str(), reply["code"].as_str()),
        (Some("error"), Some("forbidden")),
        "{reply}"
    );
}

// --- Frames --------------------------------------------------------------------------------

#[tokio::test]
async fn bad_frames_get_error_invalid_and_the_socket_stays() {
    let h = Harness::start().await;
    let mut ws = h.authed("ana").await;
    for bad in [
        json!({"type": "nope"}),
        json!({"type": "join", "ref": 1, "channel": "x"}),
        json!([1]),
    ] {
        ws.send(bad.clone()).await;
        let e = ws.frame_of("error").await;
        assert_eq!(e["code"], "invalid", "{bad} → {e}");
    }
    ws.send_text("not json".into()).await;
    assert_eq!(ws.frame_of("error").await["code"], "invalid");
    ws.send_binary(vec![1, 2, 3]).await;
    assert_eq!(
        ws.frame_of("error").await["code"],
        "invalid",
        "binary frames aren't the protocol"
    );
    assert_eq!(
        ws.join_project("9", PROJECT).await["type"],
        "joined",
        "still open"
    );
}

#[tokio::test]
async fn a_join_with_a_bad_channel_is_invalid_with_its_ref() {
    let h = Harness::start().await;
    let mut ws = h.authed("ana").await;
    ws.send(json!({"type": "join", "ref": "7", "channel": "bogus:x"}))
        .await;
    let e = ws.reply("7").await;
    assert_eq!(
        (e["type"].as_str(), e["code"].as_str()),
        (Some("error"), Some("invalid")),
        "{e}"
    );
}

#[tokio::test]
async fn a_frame_over_64_kb_closes_1009() {
    let h = Harness::start().await;
    let mut ws = h.authed("ana").await;
    let big = json!({"type": "view", "channel": table_id("items"), "keys": ["x".repeat(70_000)]});
    ws.send(big).await;
    assert_eq!(ws.closed().await, close::TOO_BIG);
}

#[tokio::test]
async fn no_pong_closes_4008() {
    let h = Harness::start_with(|c| {
        c.timings.ping_interval = Duration::from_millis(200);
        c.timings.pong_timeout = Duration::from_millis(200);
    })
    .await;
    let mut ws = h.authed("ana").await;
    // Not reading means not answering the pings (the client answers only while it reads).
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert_eq!(ws.closed().await, close::PONG_TIMEOUT);
}

#[tokio::test]
async fn answered_pings_keep_the_socket() {
    let h = Harness::start_with(|c| {
        c.timings.ping_interval = Duration::from_millis(100);
        c.timings.pong_timeout = Duration::from_millis(300);
    })
    .await;
    let mut ws = h.authed("ana").await;
    assert!(
        ws.frames_within(Duration::from_millis(1200))
            .await
            .is_empty(),
        "reading answers pings"
    );
    assert_eq!(ws.join_project("1", PROJECT).await["type"], "joined");
}

// --- Sockets per member (P16) --------------------------------------------------------------

#[tokio::test]
async fn sockets_over_the_members_limit_close_4029_across_instances() {
    let h = Harness::start_with(|c| c.limits.sockets_per_member = 2).await;
    let second = h.instance(|_| {}).await;
    let mut a = h.authed_on(h.rt(), "ana").await;
    let _b = h.authed_on(&second, "ana").await;
    assert_eq!(a.join_project("1", PROJECT).await["type"], "joined");
    let mut c = Harness::open(h.rt(), &h.ticket("ana").await).await;
    assert_eq!(
        c.closed().await,
        close::LIMIT,
        "the third socket, counted with the other instance's"
    );
    let _bob = h.authed("bob").await;
    a.close().await;
    // A closed socket's entry goes at once (not after its 90 s expiry).
    tokio::time::sleep(Duration::from_millis(500)).await;
    let mut d = h.authed("ana").await;
    assert_eq!(
        d.join_project("2", PROJECT).await["type"],
        "joined",
        "a freed slot"
    );
}

// --- Shutdown ------------------------------------------------------------------------------

#[tokio::test]
async fn shutdown_closes_every_socket_with_1012() {
    let mut h = Harness::start().await;
    let mut a = h.authed("ana").await;
    let mut b = Harness::open(h.rt(), &h.ticket("bob").await).await;
    assert_eq!(a.join_table("1", "items", None).await["type"], "joined");
    let rt = h.take_rt();
    rt.shutdown();
    assert_eq!(a.closed().await, close::RESTARTING);
    assert_eq!(
        b.closed().await,
        close::RESTARTING,
        "a socket still waiting for its auth frame too"
    );
    tokio::time::timeout(Duration::from_secs(5), rt.wait())
        .await
        .expect("stops within the grace period")
        .unwrap();
    assert_eq!(h.source.watchers("items"), 0, "watches are dropped");
}

#[test]
fn the_drain_fits_composes_grace_period() {
    assert!(
        realtime::shutdown::DRAIN <= Duration::from_secs(8),
        "compose stops after 10 s"
    );
}
