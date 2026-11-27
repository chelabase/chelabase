//! #426 AC 1: two callers at different addresses invoking the same function, which calls the
//! data API with the caller's public key and no user token through its caller-bound binding
//! (`ctx.props.CHELABASE`), each draw on their own bucket (`(key, original caller)`), not on
//! one bucket for the whole app (`(key, runtime address)`).
//!
//! The runner has one address, so the second caller is the functions runtime itself: a relay
//! function calls the burst function through `env.CHELABASE` (no original caller), so the
//! gateway sees the runtime's address as that call's client. The two callers are then the
//! runner's address and the runtime's.
//!
//! The burst function falls back to `env.CHELABASE` when it has no `ctx.props.CHELABASE` (a
//! runtime from before #426), so on such a stack both callers' calls share the runtime's
//! bucket and the second caller is refused: the test fails there.
//!
//! It relies on the stack's default data API budget (burst 300, 100 a second): the first
//! caller's burst must run its bucket dry, and the second caller's must fit in a fresh one.

use serde_json::Value;

use super::Stack;

/// Calls the data API `?n=` times through the caller-bound binding with the caller's public
/// key, 50 at a time; the count of each status.
const BURST: &str = r#"export default {
  async fetch(request, env, ctx) {
    const n = Number(new URL(request.url).searchParams.get("n") ?? "1");
    const binding = ctx.props?.CHELABASE ?? env.CHELABASE;
    const headers = { "content-type": "application/json", authorization: `Bearer ${request.headers.get("chelabase-caller-key") ?? "none"}` };
    const body = JSON.stringify({ v: 1, op: "select", from: "notes" });
    const statuses = {};
    for (let i = 0; i < n; i += 50) {
      const batch = Array.from({ length: Math.min(50, n - i) }, () =>
        binding.fetch(new URL("/data/v1/query", env.CHELABASE_URL), { method: "POST", headers, body }).then(async (r) => {
          await r.arrayBuffer();
          return r.status;
        }));
      for (const s of await Promise.all(batch)) statuses[s] = (statuses[s] ?? 0) + 1;
    }
    return Response.json({ callerBound: Boolean(ctx.props?.CHELABASE), statuses });
  },
};"#;

/// Calls `burst` with the same query and the caller's key through `env.CHELABASE`: the
/// runtime is then `burst`'s caller.
const RELAY: &str = r#"export default {
  async fetch(request, env) {
    const res = await env.CHELABASE.fetch(new URL(`/functions/v1/burst${new URL(request.url).search}`, env.CHELABASE_URL), {
      headers: { authorization: `Bearer ${request.headers.get("chelabase-caller-key") ?? "none"}` },
    });
    return new Response(await res.text(), { status: res.status });
  },
};"#;

/// The first caller's calls: well past the burst, so its bucket runs dry.
const FIRST: usize = 600;
/// The second caller's: inside a fresh burst, past what a second's refill gives a dry bucket.
const SECOND: usize = 200;

/// The status counts of one invocation of `path`.
async fn invoke(s: &Stack, path: &str) -> (bool, Value) {
    let (status, body) = s.call(path, true).await;
    assert_eq!(status, 200, "{path}: {body}");
    let got: Value = serde_json::from_str(&body).unwrap_or_else(|_| panic!("{path}: {body}"));
    (got["callerBound"] == true, got["statuses"].clone())
}

fn count(statuses: &Value, status: u16) -> u64 {
    statuses[status.to_string()].as_u64().unwrap_or(0)
}

async fn caller_flow(s: &Stack) {
    s.deploy("burst", BURST).await;
    s.deploy("relay", RELAY).await;
    // Both functions answer before the bursts start (a deploy applies within 2 s).
    for name in ["burst", "relay"] {
        let path = format!("/functions/v1/{name}?n=1");
        let (status, body) = s.within(&path, true, |st, _| st == 200).await;
        assert_eq!(status, 200, "{name}: {body}");
    }

    // The two callers' bursts at once: the runner directly, the runtime through the relay.
    let first = format!("/functions/v1/burst?n={FIRST}");
    let second = format!("/functions/v1/relay?n={SECOND}");
    let ((bound, a), (_, b)) = tokio::join!(invoke(s, &first), invoke(s, &second));
    eprintln!("first caller: {a}; second caller: {b}");
    assert!(
        bound,
        "the function got no caller-bound binding (ctx.props.CHELABASE)"
    );
    assert!(
        count(&a, 429) > 0,
        "the first caller's {FIRST} calls should run its bucket dry: {a}"
    );
    assert_eq!(
        count(&b, 429),
        0,
        "the second caller shares the first one's bucket: first {a}, second {b}"
    );
}

#[tokio::test]
#[ignore = "needs the workbench stack: just functions-e2e"]
async fn two_callers_of_one_function_get_their_own_data_api_buckets() {
    let stack = std::sync::Arc::new(Stack::new().await);
    let run = stack.clone();
    let result = tokio::spawn(async move { caller_flow(&run).await }).await;
    stack.cleanup().await;
    if let Err(e) = result {
        std::panic::resume_unwind(e.into_panic());
    }
}
