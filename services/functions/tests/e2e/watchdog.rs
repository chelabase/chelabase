//! #295 on the real runtime: a function that busy-loops on every call stalls the one workerd
//! thread until the watchdog kills it (about 8 s). After three such restarts with it in flight
//! each time the functions service turns it off, with a reason the management API shows, while
//! an innocent function called all along stays on. Deploying another bundle turns it back on.
//!
//! It restarts the shared functions runtime, so the e2e binary runs one test at a time
//! (`.config/nextest.toml`).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use chelabase_metadata::WATCHDOG_DISABLED_PREFIX;
use serde_json::json;

use super::Stack;

const SPIN: &str = r#"export default { fetch() { while (true) {} } };"#;
const STEADY: &str = r#"export default { fetch() { return new Response("steady") } };"#;
const FIXED: &str = r#"export default { fetch() { return new Response("fixed") } };"#;

/// A watchdog kill and the restart: well under this.
const RESTART: Duration = Duration::from_secs(60);
/// Kills tried before giving up (3 are needed; the rest absorb a restart the service didn't
/// attribute, which the design allows).
const MAX_KILLS: usize = 6;

/// Calls `name` until it answers 200, for at most [`RESTART`]: the runtime is back.
async fn until_up(s: &Stack, name: &str) {
    let start = Instant::now();
    loop {
        let (status, _) = s.call(&format!("/functions/v1/{name}"), true).await;
        if status == 200 {
            return;
        }
        assert!(
            start.elapsed() < RESTART,
            "{name} didn't answer again: {status}"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

async fn flow(s: &Stack) {
    s.deploy("steady", STEADY).await;
    s.deploy("spin", SPIN).await;
    let (status, body) = s
        .within("/functions/v1/steady", true, |st, _| st == 200)
        .await;
    assert_eq!((status, body.as_str()), (200, "steady"));

    // The innocent function, called all along (it queues behind each stall and dies with it).
    let stop = Arc::new(AtomicBool::new(false));
    let steady_calls = Arc::new(AtomicUsize::new(0));
    let steady_dropped = Arc::new(AtomicUsize::new(0));
    let caller = {
        let (stop, calls, dropped) = (stop.clone(), steady_calls.clone(), steady_dropped.clone());
        let s = Stack::with(super::metadata().await, s.db, s.branch, s.key.clone());
        tokio::spawn(async move {
            while !stop.load(Ordering::Relaxed) {
                let (status, body) = s.call("/functions/v1/steady", true).await;
                calls.fetch_add(1, Ordering::Relaxed);
                if status == 502 && body.contains("runtime_unavailable") {
                    dropped.fetch_add(1, Ordering::Relaxed);
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        })
    };

    let mut kills = 0;
    let mut disabled = false;
    while kills < MAX_KILLS && !disabled {
        until_up(s, "steady").await;
        let started = Instant::now();
        let (status, body) = s.call("/functions/v1/spin", true).await;
        let held = started.elapsed();
        if status == 404 {
            break;
        }
        // A kill answers 502 runtime_unavailable after the watchdog's ~6 to 9 s.
        assert_eq!(status, 502, "spin: {body}");
        assert!(body.contains("runtime_unavailable"), "{body}");
        if held >= Duration::from_secs(4) {
            kills += 1;
        }
        let listed = s.listed("spin").await;
        disabled = listed["enabled"] == json!(false);
    }
    stop.store(true, Ordering::Relaxed);
    caller.await.unwrap();
    assert!(
        steady_calls.load(Ordering::Relaxed) >= kills,
        "the innocent function was called throughout"
    );
    assert!(
        steady_dropped.load(Ordering::Relaxed) >= 1,
        "the innocent function was in flight at a kill"
    );

    // (b) Off, with the watchdog's reason, and its calls answer as a disabled function's.
    let start = Instant::now();
    let listed = loop {
        let listed = s.listed("spin").await;
        if listed["enabled"] == json!(false) || start.elapsed() > Duration::from_secs(5) {
            break listed;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    assert_eq!(
        listed["enabled"],
        json!(false),
        "after {kills} kills: {listed}"
    );
    assert!(
        (3..=MAX_KILLS).contains(&kills),
        "turned off after {kills} kills, 3 are needed"
    );
    let reason = listed["last_error"].as_str().unwrap_or_default();
    assert!(reason.starts_with(WATCHDOG_DISABLED_PREFIX), "{listed}");
    let (status, _) = s
        .within("/functions/v1/spin", true, |st, _| st == 404)
        .await;
    assert_eq!(status, 404);

    // (c) The innocent function, in flight at every kill, is still on.
    let steady = s.listed("steady").await;
    assert_eq!(steady["enabled"], json!(true), "{steady}");
    assert!(
        !steady["last_error"]
            .as_str()
            .unwrap_or_default()
            .starts_with(WATCHDOG_DISABLED_PREFIX),
        "{steady}"
    );
    until_up(s, "steady").await;

    // (d) A redeploy of another bundle (activated, not enabled by hand) turns it back on and
    // clears the reason.
    s.deploy_only("spin", FIXED).await;
    let listed = s.listed("spin").await;
    assert_eq!(
        (&listed["enabled"], &listed["last_error"]),
        (&json!(true), &json!(null)),
        "after the redeploy: {listed}"
    );
    let (status, body) = s
        .within("/functions/v1/spin", true, |st, _| st == 200)
        .await;
    assert_eq!((status, body.as_str()), (200, "fixed"));
}

/// Review B1 on the real runtime: the looper's caller gives up after a second (the runtime
/// keeps looping) and the innocent function's call queues behind it, so at each kill the
/// innocent call is the one in flight. The looper is still the one turned off.
async fn abandoned_flow(s: &Stack) {
    s.deploy("queued", STEADY).await;
    s.deploy("spin", SPIN).await;
    let short = reqwest::Client::builder()
        .timeout(Duration::from_secs(1))
        .build()
        .unwrap();
    let mut kills = 0;
    while kills < MAX_KILLS {
        until_up(s, "queued").await;
        let gave_up = short
            .get(format!("{}/functions/v1/spin", s.api))
            .bearer_auth(&s.key)
            .send()
            .await;
        if let Ok(resp) = &gave_up
            && resp.status() == 404
        {
            break;
        }
        assert!(gave_up.is_err(), "the looper's caller gave up: {gave_up:?}");
        let (status, body) = s.call("/functions/v1/queued", true).await;
        if status == 502 {
            kills += 1;
        } else {
            assert_eq!(status, 200, "{body}");
        }
        if s.listed("spin").await["enabled"] == json!(false) {
            break;
        }
    }
    let start = Instant::now();
    let listed = loop {
        let listed = s.listed("spin").await;
        if listed["enabled"] == json!(false) || start.elapsed() > Duration::from_secs(5) {
            break listed;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    assert_eq!(
        listed["enabled"],
        json!(false),
        "after {kills} kills: {listed}"
    );
    assert!(
        listed["last_error"]
            .as_str()
            .unwrap_or_default()
            .starts_with(WATCHDOG_DISABLED_PREFIX),
        "{listed}"
    );
    let queued = s.listed("queued").await;
    assert_eq!(
        queued["enabled"],
        json!(true),
        "the queued function stays on: {queued}"
    );
    until_up(s, "queued").await;
}

#[tokio::test]
#[ignore = "needs the workbench stack: just functions-e2e"]
async fn an_abandoned_looper_is_turned_off_not_the_call_queued_behind_it() {
    let stack = Arc::new(Stack::new().await);
    let run = stack.clone();
    let result = tokio::spawn(async move { abandoned_flow(&run).await }).await;
    stack.cleanup().await;
    if let Err(e) = result {
        std::panic::resume_unwind(e.into_panic());
    }
}

#[tokio::test]
#[ignore = "needs the workbench stack: just functions-e2e"]
async fn a_function_that_keeps_stalling_the_runtime_is_turned_off() {
    let stack = Arc::new(Stack::new().await);
    let run = stack.clone();
    let result = tokio::spawn(async move { flow(&run).await }).await;
    stack.cleanup().await;
    if let Err(e) = result {
        std::panic::resume_unwind(e.into_panic());
    }
}
