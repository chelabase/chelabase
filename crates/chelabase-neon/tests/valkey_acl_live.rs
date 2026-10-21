//! RT1 T1 (R11, W15, P5, S6): each Valkey ACL user can do exactly what `infra/compose/valkey`'s
//! users file grants. Live: run in the stack's runner (`just workbench test -p chelabase-neon
//! --test valkey_acl_live -- --run-ignored only`). Reads `CHELABASE_VALKEY_URL` (no credentials
//! in it, S1) and each user's password from `$CHELABASE_KEYS_DIR/valkey/<user>.password` (the
//! runner sees every Valkey password file, it is the test harness).
//!
//! S6's file: `default` off; `proxy` = `~cancel:*`, `&neondb-proxy-ws-updates`, subscribe ping set
//! get hget expire client|setinfo; `control` = `&neondb-proxy-ws-updates`, publish ping
//! client|setinfo; `realtime` = `~rt:*`, set getdel incr decr expire zadd zrem zremrangebyscore
//! zcard ping client|setinfo; `runner` = realtime's and control's rules.

use chelabase_neon::proxy_cache::CHANNEL;
use redis::aio::MultiplexedConnection;
use redis::{RedisError, Value};
use uuid::Uuid;

fn base_url() -> String {
    std::env::var("CHELABASE_VALKEY_URL").unwrap_or_else(|_| "redis://valkey:6379".into())
}

fn password(user: &str) -> String {
    let keys = std::env::var("CHELABASE_KEYS_DIR").expect("CHELABASE_KEYS_DIR (stack.env)");
    let path = std::path::Path::new(&keys)
        .join("valkey")
        .join(format!("{user}.password"));
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{}: {e}", path.display()))
        .trim_end()
        .to_string()
}

fn info(user: Option<&str>) -> redis::ConnectionInfo {
    use redis::IntoConnectionInfo;
    let mut info = base_url().into_connection_info().unwrap();
    assert!(
        info.redis_settings().username().is_none(),
        "CHELABASE_VALKEY_URL carries no credentials (S1)"
    );
    if let Some(user) = user {
        let settings = info
            .redis_settings()
            .clone()
            .set_username(user)
            .set_password(password(user));
        info = info.set_redis_settings(settings);
    }
    info
}

async fn conn(user: &str) -> MultiplexedConnection {
    redis::Client::open(info(Some(user)))
        .unwrap()
        .get_multiplexed_async_connection()
        .await
        .unwrap_or_else(|e| panic!("connect as {user}: {e}"))
}

async fn run(c: &mut MultiplexedConnection, args: &[&str]) -> Result<Value, RedisError> {
    let mut cmd = redis::cmd(args[0]);
    for a in &args[1..] {
        cmd.arg(*a);
    }
    cmd.query_async(c).await
}

async fn allowed(user: &str, c: &mut MultiplexedConnection, args: &[&str]) -> Value {
    run(c, args)
        .await
        .unwrap_or_else(|e| panic!("{user}: {args:?} refused: {e}"))
}

/// redis-rs renders the server's `-NOPERM …` as `NoPerm: …`.
fn is_noperm(e: &RedisError) -> bool {
    e.code() == Some("NOPERM") || e.to_string().to_uppercase().contains("NOPERM")
}

async fn refused(user: &str, c: &mut MultiplexedConnection, args: &[&str]) {
    match run(c, args).await {
        Err(e) if is_noperm(&e) => {}
        other => panic!("{user}: {args:?} must be refused with NOPERM, got {other:?}"),
    }
}

fn key(prefix: &str) -> String {
    format!("{prefix}t1-{}", Uuid::new_v4())
}

#[tokio::test]
#[ignore]
async fn the_default_user_is_off() {
    let mut c = redis::Client::open(info(None))
        .unwrap()
        .get_multiplexed_async_connection()
        .await
        .unwrap();
    let err = run(&mut c, &["PING"])
        .await
        .expect_err("an unauthenticated PING is refused");
    assert!(
        err.code() == Some("NOAUTH") || err.to_string().to_uppercase().contains("NOAUTH"),
        "{err}"
    );
    let err = run(&mut c, &["AUTH", "default", "anything"])
        .await
        .expect_err("default can't log in");
    assert!(
        err.to_string().to_uppercase().contains("WRONGPASS")
            || err.to_string().contains("disabled"),
        "{err}"
    );
}

#[tokio::test]
#[ignore]
async fn a_wrong_password_is_refused() {
    let mut info = info(None);
    let settings = info
        .redis_settings()
        .clone()
        .set_username("realtime")
        .set_password("not-it");
    info = info.set_redis_settings(settings);
    let result = redis::Client::open(info)
        .unwrap()
        .get_multiplexed_async_connection()
        .await;
    assert!(result.is_err(), "realtime with a wrong password connected");
}

// W15: realtime's user reaches only `rt:*`, with only the commands tickets and counters use.
#[tokio::test]
#[ignore]
async fn realtime_sets_and_takes_rt_keys_only() {
    let mut c = conn("realtime").await;
    let k = key("rt:ticket:");
    assert_eq!(
        allowed("realtime", &mut c, &["SET", &k, "sealed", "EX", "30"]).await,
        Value::Okay
    );
    let got = allowed("realtime", &mut c, &["GETDEL", &k]).await;
    assert_eq!(got, Value::BulkString(b"sealed".to_vec()));
    assert_eq!(
        allowed("realtime", &mut c, &["GETDEL", &k]).await,
        Value::Nil,
        "GETDEL takes it once"
    );
    let n = key("rt:cnt:t:");
    allowed("realtime", &mut c, &["INCR", &n]).await;
    allowed("realtime", &mut c, &["DECR", &n]).await;
    allowed("realtime", &mut c, &["EXPIRE", &n, "30"]).await;
    let z = key("rt:cnt:sock:");
    allowed("realtime", &mut c, &["ZADD", &z, "100", "sock-1"]).await;
    allowed("realtime", &mut c, &["ZREMRANGEBYSCORE", &z, "0", "50"]).await;
    allowed("realtime", &mut c, &["ZCARD", &z]).await;
    allowed("realtime", &mut c, &["ZREM", &z, "sock-1"]).await;
    allowed("realtime", &mut c, &["PING"]).await;

    let cancel = key("cancel:");
    for args in [
        &["GET", k.as_str()][..],
        &["SET", cancel.as_str(), "x"][..],
        &["GETDEL", cancel.as_str()][..],
        &["GET", cancel.as_str()][..],
        &["HGET", cancel.as_str(), "f"][..],
        &["PUBLISH", CHANNEL, "x"][..],
        &["SUBSCRIBE", CHANNEL][..],
        &["KEYS", "*"][..],
        &["SCAN", "0"][..],
        &["DEL", k.as_str()][..],
        &["FLUSHALL"][..],
        &["CONFIG", "GET", "*"][..],
        &["ACL", "LIST"][..],
        &["EVAL", "return 1", "0"][..],
    ] {
        refused("realtime", &mut c, args).await;
    }
}

// S6: the proxy keeps its cancellation keys and its subscription, nothing else.
#[tokio::test]
#[ignore]
async fn the_proxy_keeps_cancel_keys_and_its_channel_only() {
    let mut c = conn("proxy").await;
    let k = key("cancel:");
    assert_eq!(
        allowed("proxy", &mut c, &["SET", &k, "v", "EX", "10"]).await,
        Value::Okay
    );
    assert_eq!(
        allowed("proxy", &mut c, &["GET", &k]).await,
        Value::BulkString(b"v".to_vec())
    );
    allowed("proxy", &mut c, &["EXPIRE", &k, "5"]).await;
    allowed("proxy", &mut c, &["HGET", &key("cancel:"), "f"]).await;
    allowed("proxy", &mut c, &["PING"]).await;
    let rt_key = key("rt:ticket:");
    for args in [
        &["GET", rt_key.as_str()][..],
        &["SET", rt_key.as_str(), "x"][..],
        &["GETDEL", k.as_str()][..],
        &["DEL", k.as_str()][..],
        &["PUBLISH", CHANNEL, "x"][..],
        &["KEYS", "*"][..],
        &["FLUSHALL"][..],
    ] {
        refused("proxy", &mut c, args).await;
    }
    // Subscriptions: its own channel only.
    let mut pubsub = redis::Client::open(info(Some("proxy")))
        .unwrap()
        .get_async_pubsub()
        .await
        .unwrap();
    pubsub
        .subscribe(CHANNEL)
        .await
        .expect("the proxy subscribes to its channel");
    // (redis-rs's async PubSub doesn't surface a refused SUBSCRIBE; a plain command does.)
    let mut other = conn("proxy").await;
    refused("proxy", &mut other, &["SUBSCRIBE", "some-other-channel"]).await;
}

// The orchestrator and worker (`control`) publish cache clearing and nothing else; the message
// reaches a subscribed proxy.
#[tokio::test]
#[ignore]
async fn control_publishes_cache_clearing_only() {
    use futures_util::StreamExt;
    let mut proxy = redis::Client::open(info(Some("proxy")))
        .unwrap()
        .get_async_pubsub()
        .await
        .unwrap();
    proxy.subscribe(CHANNEL).await.unwrap();
    let mut c = conn("control").await;
    let receivers = allowed("control", &mut c, &["PUBLISH", CHANNEL, "t1-probe"]).await;
    assert!(
        matches!(receivers, Value::Int(n) if n >= 1),
        "{receivers:?}"
    );
    // The channel is shared with other live tests (proxy_cache_live publishes on it): skip
    // foreign messages until ours arrives, within the same 3 s.
    let mut stream = proxy.on_message();
    let got = tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while let Some(msg) = stream.next().await {
            if msg.get_payload::<String>().ok().as_deref() == Some("t1-probe") {
                return true;
            }
        }
        false
    })
    .await
    .expect("the proxy got it");
    assert!(got, "the subscription ended before t1-probe");
    drop(stream);
    let (cancel, rt_key) = (key("cancel:"), key("rt:ticket:"));
    for args in [
        &["PUBLISH", "some-other-channel", "x"][..],
        &["GET", cancel.as_str()][..],
        &["SET", cancel.as_str(), "x"][..],
        &["SET", rt_key.as_str(), "x"][..],
        &["GETDEL", rt_key.as_str()][..],
        &["SUBSCRIBE", CHANNEL][..],
        &["KEYS", "*"][..],
    ] {
        refused("control", &mut c, args).await;
    }
}

// S6: runner = realtime's and control's rules (tests run realtime and the control plane
// in-process), and still not the proxy's keys.
#[tokio::test]
#[ignore]
async fn the_runner_has_realtimes_and_controls_rules() {
    let mut c = conn("runner").await;
    let k = key("rt:ticket:");
    allowed("runner", &mut c, &["SET", &k, "v", "EX", "30"]).await;
    allowed("runner", &mut c, &["GETDEL", &k]).await;
    allowed("runner", &mut c, &["PUBLISH", CHANNEL, "t1-probe"]).await;
    let cancel = key("cancel:");
    for args in [
        &["GET", cancel.as_str()][..],
        &["SET", cancel.as_str(), "x"][..],
        &["FLUSHALL"][..],
        &["KEYS", "*"][..],
    ] {
        refused("runner", &mut c, args).await;
    }
}

// Ruling (f): the healthcheck's user answers PING and nothing else.
#[tokio::test]
#[ignore]
async fn the_health_user_may_only_ping() {
    let mut c = conn("health").await;
    assert_eq!(
        allowed("health", &mut c, &["PING"]).await,
        Value::SimpleString("PONG".into())
    );
    let (rt_key, cancel) = (key("rt:ticket:"), key("cancel:"));
    for args in [
        &["GET", rt_key.as_str()][..],
        &["SET", rt_key.as_str(), "x"][..],
        &["GET", cancel.as_str()][..],
        &["PUBLISH", CHANNEL, "x"][..],
        &["SUBSCRIBE", CHANNEL][..],
        &["KEYS", "*"][..],
    ] {
        refused("health", &mut c, args).await;
    }
}

// RT1 final fix round (batch A): realtime's counters set their count and TTL in one
// MULTI/EXEC (INCR then EXPIRE … NX), so realtime and the runner may run a transaction; its
// commands are still checked against `~rt:*` when queued.
#[tokio::test]
#[ignore]
async fn realtime_and_the_runner_count_in_one_transaction() {
    for user in ["realtime", "runner"] {
        let mut c = conn(user).await;
        let n = key("rt:cnt:tix:");
        let (count, set): (u64, i64) = redis::pipe()
            .atomic()
            .cmd("INCR")
            .arg(&n)
            .cmd("EXPIRE")
            .arg(&n)
            .arg(30)
            .arg("NX")
            .query_async(&mut c)
            .await
            .unwrap_or_else(|e| panic!("{user}: MULTI INCR EXPIRE NX EXEC refused: {e}"));
        assert_eq!((count, set), (1, 1), "{user}: the first count sets the TTL");
        let (count, set): (u64, i64) = redis::pipe()
            .atomic()
            .cmd("INCR")
            .arg(&n)
            .cmd("EXPIRE")
            .arg(&n)
            .arg(30)
            .arg("NX")
            .query_async(&mut c)
            .await
            .unwrap();
        assert_eq!((count, set), (2, 0), "{user}: NX keeps the window's TTL");
        // Outside `~rt:*` the queued command is refused, and EXEC runs nothing.
        let outside = key("cancel:");
        let result: Result<(u64,), RedisError> = redis::pipe()
            .atomic()
            .cmd("INCR")
            .arg(&outside)
            .query_async(&mut c)
            .await;
        match result {
            Err(e) if is_noperm(&e) || e.to_string().to_uppercase().contains("EXECABORT") => {}
            got => panic!("{user}: a transaction on {outside} must be refused, got {got:?}"),
        }
    }
}
