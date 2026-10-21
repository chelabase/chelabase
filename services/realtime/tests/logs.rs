//! RT1 T7 (R22, W16 realtime side): realtime's logs never carry the socket URL's query string
//! (the ticket) or an `auth` frame's contents (the member's token), at any level, on the happy
//! path or the failing ones. Every log line of the process is captured at TRACE.

mod common;

use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::*;
use realtime::protocol::SUBPROTOCOL;
use serde_json::json;

#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
    type Writer = Captured;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

// The one test of this binary: a global subscriber (with `log` records bridged, as `main`'s
// `init()` does), so every task and every library logs into the capture. The test's own
// WebSocket client (tungstenite logs its request line at TRACE) is the only target left out.
#[tokio::test]
async fn tickets_and_tokens_never_reach_the_logs() {
    use tracing_subscriber::util::SubscriberInitExt;
    let captured = Captured::default();
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new(
            "trace,tungstenite=off,tokio_tungstenite=off",
        ))
        .with_writer(captured.clone())
        .with_ansi(false)
        .finish()
        .try_init()
        .expect("the only subscriber of this test binary");

    let h = Harness::start().await;
    let mut secrets = vec![];

    // A good ticket and token, a join, a renewal, a presence update.
    let ticket = h.ticket("ana").await;
    secrets.push(ticket.clone());
    let mut ws = Harness::open(h.rt(), &ticket).await;
    let tok = token("ana", 3600);
    secrets.push(tok.clone());
    ws.send(json!({"type": "auth", "token": tok})).await;
    assert_eq!(ws.join_table("1", "items", None).await["type"], "joined");
    let renewed = token("ana", 7200);
    secrets.push(renewed.clone());
    ws.send(json!({"type": "auth", "token": renewed})).await;
    ws.send(json!({"type": "presence", "channel": table_id("items"), "state": {"row": "[\"1\"]", "column": null, "editing": false}}))
        .await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    // A replayed ticket and an unknown one (401 paths).
    assert!(
        Harness::try_open(h.rt(), &ticket, Some(SUBPROTOCOL))
            .await
            .is_err()
    );
    let unknown = realtime::ticket::new_ticket();
    secrets.push(unknown.clone());
    assert!(
        Harness::try_open(h.rt(), &unknown, Some(SUBPROTOCOL))
            .await
            .is_err()
    );

    // A token for another person (4003) and a token that doesn't verify (4001).
    let mut other = Harness::open(h.rt(), &h.ticket("ana").await).await;
    let bob = token("bob", 3600);
    secrets.push(bob.clone());
    other.send(json!({"type": "auth", "token": bob})).await;
    other.closed().await;
    let mut bad = Harness::open(h.rt(), &h.ticket("ana").await).await;
    let garbage = "eyJhbGciOiJSUzI1NiJ9.SECRET-PAYLOAD-MARKER.signature".to_string();
    secrets.push(garbage.clone());
    bad.send(json!({"type": "auth", "token": garbage})).await;
    bad.closed().await;

    // An orchestrator failure while the token is in hand.
    h.orch.on_authorize(|_| (503, json!({})));
    let reply = ws.join_table("2", "other", None).await;
    assert_eq!(reply["code"], "unavailable");
    tokio::time::sleep(Duration::from_millis(200)).await;

    let logs = String::from_utf8(captured.0.lock().unwrap().clone()).unwrap();
    assert!(
        !logs.is_empty(),
        "the capture works (realtime logs something at TRACE)"
    );
    assert!(!logs.contains("ticket="), "no query string in the logs");
    for s in &secrets {
        assert!(
            !logs.contains(s.as_str()),
            "a secret reached the logs: {}…",
            &s[..12.min(s.len())]
        );
    }
    assert!(!logs.contains("SECRET-PAYLOAD-MARKER"));
}
