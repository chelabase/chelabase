//! Review fix 1 (auth-replacement whole-epic review): a member's sockets from different
//! identity sessions never share a re-authorization answer, so the socket of a session that
//! logged out is refused at its next check even while another live session of the same member
//! is on the instance.

mod common;

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use common::*;
use serde_json::{Value, json};

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

/// Reads `ws` until `want` holds for a frame; false on a close or at `deadline`.
async fn arrives(ws: &mut Ws, deadline: Instant, want: impl Fn(&Value) -> bool) -> bool {
    loop {
        match ws
            .read(deadline.saturating_duration_since(Instant::now()))
            .await
        {
            Read::Frame(v) if want(&v) => return true,
            Read::Frame(_) => continue,
            Read::Closed(_) | Read::Timeout => return false,
        }
    }
}

fn is_forbidden(v: &Value) -> bool {
    v["type"] == "error" && v["code"] == "forbidden"
}

/// A socket of `sub` authenticated with a token of identity session `sid`.
async fn authed_session(h: &Harness, sub: &str, sid: &str) -> Ws {
    let ticket = h.ticket_on(h.rt(), sub, TENANT).await;
    let mut ws = Harness::open(h.rt(), &ticket).await;
    ws.send(json!({"type": "auth", "token": token_session(sub, TENANT, sid, 3600)}))
        .await;
    ws
}

/// Ana has two sessions on one instance, each with a socket on one table. The `dead` session
/// logs out (the orchestrator now refuses its token); the live session's socket runs its
/// periodic check first in each cycle. The dead socket must still get its own answer and leave
/// the channel, instead of reusing the live session's cached `allowed`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_ended_sessions_socket_never_reuses_a_live_sessions_answer() {
    const CYCLE_MS: u64 = 3000;
    let h =
        Harness::start_with(|c| c.timings.reauth_interval = Duration::from_millis(CYCLE_MS)).await;
    // Start just after a cycle boundary, so the live socket's periodic run comes first in
    // every cycle.
    let aligned = Instant::now() + Duration::from_secs(4);
    while !(100..300).contains(&(unix_ms() % CYCLE_MS)) {
        assert!(Instant::now() < aligned, "no cycle boundary found");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let mut live = authed_session(&h, "ana", "live").await;
    assert_eq!(live.join_table("1", "items", None).await["type"], "joined");
    tokio::time::sleep(Duration::from_millis(800)).await;
    let mut dead = authed_session(&h, "ana", "dead").await;
    assert_eq!(dead.join_table("1", "items", None).await["type"], "joined");
    // The `dead` session logs out: the orchestrator refuses its token from now on.
    h.orch.on_authorize(
        |body| match body["token"].as_str().and_then(sid_of).as_deref() {
            Some("live") => allowed("ana"),
            _ => refused("forbidden"),
        },
    );
    assert!(
        arrives(
            &mut dead,
            Instant::now() + Duration::from_millis(2 * CYCLE_MS + 1500),
            is_forbidden
        )
        .await,
        "the ended session's socket kept the live session's answer"
    );
    assert!(
        !arrives(
            &mut live,
            Instant::now() + Duration::from_millis(200),
            is_forbidden
        )
        .await,
        "the live session's socket was refused"
    );
}
