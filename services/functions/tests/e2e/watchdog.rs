//! #295 on the real runtime: functions that stall the one workerd thread until the watchdog
//! kills it (about 8 s each time). The functions service only reports which function it thinks
//! is the cause, in its log (a warning per restart counted, an error at three within 10
//! minutes); nothing is turned off, whatever stalls the runtime, so no function can get another
//! one disabled. Each test makes the runtime restart three times and checks that every
//! function involved is still on; which one the log names is checked from the service's log
//! (it isn't visible from here).
//!
//! They restart the shared functions runtime, so the e2e binary runs one test at a time
//! (`.config/nextest.toml`).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use serde_json::json;

use super::Stack;

const SPIN: &str = r#"export default { fetch() { while (true) {} } };"#;
const STEADY: &str = r#"export default { fetch() { return new Response("steady") } };"#;
/// Answers, then busy-loops in `waitUntil` once the answer has gone out.
const ANSWER_THEN_SPIN: &str = r#"export default {
  fetch(request, env, ctx) {
    ctx.waitUntil(new Promise((r) => setTimeout(r, 300)).then(() => { while (true) {} }));
    return new Response("answered");
  },
};"#;

/// A watchdog kill and the restart: well under this.
const RESTART: Duration = Duration::from_secs(60);
/// The kills each test makes.
const KILLS: usize = 3;
/// Attempts before giving up on making them.
const MAX_TRIES: usize = 8;

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

/// Every one of `names` is still on, with no watchdog reason anywhere.
async fn all_still_on(s: &Stack, names: &[&str]) {
    for name in names {
        let listed = s.listed(name).await;
        assert_eq!(listed["enabled"], json!(true), "{name}: {listed}");
        let error = listed["last_error"].as_str().unwrap_or_default();
        assert!(!error.contains("watchdog"), "{name}: {listed}");
    }
}

/// A function that loops on every call, with an innocent one called all along (it queues
/// behind each stall and dies with it). The service names the looper (warn, then error at the
/// third); both stay on.
async fn looper_flow(s: &Stack) {
    s.deploy("steady", STEADY).await;
    s.deploy("spin", SPIN).await;
    let (status, body) = s
        .within("/functions/v1/steady", true, |st, _| st == 200)
        .await;
    assert_eq!((status, body.as_str()), (200, "steady"));

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
    for _ in 0..MAX_TRIES {
        if kills == KILLS {
            break;
        }
        until_up(s, "steady").await;
        let started = Instant::now();
        let (status, body) = s.call("/functions/v1/spin", true).await;
        // A kill answers 502 runtime_unavailable after the watchdog's ~6 to 9 s.
        assert_eq!(status, 502, "spin: {body}");
        assert!(body.contains("runtime_unavailable"), "{body}");
        if started.elapsed() >= Duration::from_secs(4) {
            kills += 1;
        }
    }
    stop.store(true, Ordering::Relaxed);
    caller.await.unwrap();
    assert_eq!(kills, KILLS);
    assert!(
        steady_dropped.load(Ordering::Relaxed) >= 1,
        "the innocent function was in flight at a kill ({} calls)",
        steady_calls.load(Ordering::Relaxed)
    );
    all_still_on(s, &["spin", "steady"]).await;
    until_up(s, "steady").await;
}

/// Review B1 on the real runtime: the looper's caller gives up after a second (the runtime
/// keeps looping) and the innocent function's call queues behind it, so at each kill the
/// innocent call is the one in flight. The service still names the looper; both stay on.
async fn abandoned_flow(s: &Stack) {
    s.deploy("queued", STEADY).await;
    s.deploy("spin", SPIN).await;
    let short = reqwest::Client::builder()
        .timeout(Duration::from_secs(1))
        .build()
        .unwrap();
    let mut kills = 0;
    for _ in 0..MAX_TRIES {
        if kills == KILLS {
            break;
        }
        until_up(s, "queued").await;
        let gave_up = short
            .get(format!("{}/functions/v1/spin", s.api))
            .bearer_auth(&s.key)
            .send()
            .await;
        assert!(gave_up.is_err(), "the looper's caller gave up: {gave_up:?}");
        let (status, body) = s.call("/functions/v1/queued", true).await;
        if status == 502 {
            kills += 1;
        } else {
            assert_eq!(status, 200, "{body}");
        }
    }
    assert_eq!(kills, KILLS);
    all_still_on(s, &["spin", "queued"]).await;
    until_up(s, "queued").await;
}

/// The attack warn-only defends against: a function answers, then busy-loops in `waitUntil`,
/// and the caller then calls another function (another tenant's, say), which queues behind the
/// loop and is the only one in flight at the kill. The service can't tell that from a looper
/// on a quiet runtime, so it may name the victim in its log; it turns nothing off.
async fn answer_then_spin_flow(s: &Stack) {
    s.deploy("victim", STEADY).await;
    s.deploy("attacker", ANSWER_THEN_SPIN).await;
    let mut kills = 0;
    for _ in 0..MAX_TRIES {
        if kills == KILLS {
            break;
        }
        until_up(s, "victim").await;
        let (status, body) = s.call("/functions/v1/attacker", true).await;
        assert_eq!((status, body.as_str()), (200, "answered"));
        // Its loop starts 300 ms after the answer: call the victim once it has.
        tokio::time::sleep(Duration::from_millis(600)).await;
        let (status, body) = s.call("/functions/v1/victim", true).await;
        if status == 502 {
            kills += 1;
        } else {
            assert_eq!(status, 200, "{body}");
        }
    }
    assert_eq!(kills, KILLS, "the attacker's loop got the runtime killed");
    all_still_on(s, &["victim", "attacker"]).await;
    until_up(s, "victim").await;
}

/// Runs `flow` on a stack of its own and cleans up, pass or fail.
async fn run<F, Fut>(flow: F)
where
    F: FnOnce(Arc<Stack>) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let stack = Arc::new(Stack::new().await);
    let result = tokio::spawn(flow(stack.clone())).await;
    stack.cleanup().await;
    if let Err(e) = result {
        std::panic::resume_unwind(e.into_panic());
    }
}

#[tokio::test]
#[ignore = "needs the workbench stack: just functions-e2e"]
async fn a_looping_function_is_named_and_nothing_is_turned_off() {
    run(|s| async move { looper_flow(&s).await }).await;
}

#[tokio::test]
#[ignore = "needs the workbench stack: just functions-e2e"]
async fn an_abandoned_looper_is_named_and_nothing_is_turned_off() {
    run(|s| async move { abandoned_flow(&s).await }).await;
}

#[tokio::test]
#[ignore = "needs the workbench stack: just functions-e2e"]
async fn a_function_spinning_after_its_answer_gets_nothing_turned_off() {
    run(|s| async move { answer_then_spin_flow(&s).await }).await;
}
