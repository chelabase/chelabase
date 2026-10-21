//! Against a real Valkey (the workbench stack):
//!
//! ```sh
//! cargo test -p chelabase-neon --test proxy_cache_live -- --ignored
//! ```

use chelabase_neon::proxy_cache::{CHANNEL, ProxyCache, password_updated_message};
use futures_util::StreamExt;

fn url() -> String {
    std::env::var("CHELABASE_VALKEY_URL").unwrap_or_else(|_| "redis://valkey:6379".into())
}

/// Subscribes as the `proxy` ACL user (RT1 T1: only it may subscribe; the runner sees its
/// password file, `$CHELABASE_KEYS_DIR/valkey/proxy.password`).
fn proxy_info() -> redis::ConnectionInfo {
    use redis::IntoConnectionInfo;
    let keys = std::env::var("CHELABASE_KEYS_DIR").expect("CHELABASE_KEYS_DIR (stack.env)");
    let password = std::fs::read_to_string(format!("{keys}/valkey/proxy.password")).unwrap();
    let info = url().into_connection_info().unwrap();
    let settings = info
        .redis_settings()
        .clone()
        .set_username("proxy")
        .set_password(password.trim_end());
    info.set_redis_settings(settings)
}

#[tokio::test]
#[ignore = "needs Valkey (is the workbench stack up?)"]
async fn a_subscriber_gets_the_message_and_is_counted() {
    let client = redis::Client::open(proxy_info()).unwrap();
    let mut sub = client
        .get_async_pubsub()
        .await
        .expect("Valkey reachable (is the workbench stack up?)");
    sub.subscribe(CHANNEL).await.unwrap();

    // As the caller's own ACL user (the runner's: CHELABASE_VALKEY_USER/_PASSWORD_FILE).
    let receivers = ProxyCache::from_lookup(&url(), |n| std::env::var(n).ok())
        .unwrap()
        .password_updated("ep-live", "app")
        .await
        .unwrap();
    assert!(receivers >= 1, "at least this test's subscriber");

    // The channel is shared with other live tests (valkey_acl_live publishes on it): skip
    // foreign messages until ours arrives, within the same 5 s.
    let expected = password_updated_message("ep-live", "app");
    let mut stream = sub.on_message();
    let got = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while let Some(msg) = stream.next().await {
            if msg.get_payload::<String>().ok().as_deref() == Some(expected.as_str()) {
                return true;
            }
        }
        false
    })
    .await
    .expect("the expected message within 5 s");
    assert!(got, "the subscription ended before the expected message");
}
