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

#[tokio::test]
#[ignore = "needs Valkey (is the workbench stack up?)"]
async fn a_subscriber_gets_the_message_and_is_counted() {
    let client = redis::Client::open(url()).unwrap();
    let mut sub = client
        .get_async_pubsub()
        .await
        .expect("Valkey reachable (is the workbench stack up?)");
    sub.subscribe(CHANNEL).await.unwrap();

    let receivers = ProxyCache::new(&url())
        .unwrap()
        .password_updated("ep-live", "app")
        .await
        .unwrap();
    assert!(receivers >= 1, "at least this test's subscriber");

    let msg = tokio::time::timeout(std::time::Duration::from_secs(5), sub.on_message().next())
        .await
        .expect("a message within 5 s")
        .unwrap();
    let payload: String = msg.get_payload().unwrap();
    assert_eq!(payload, password_updated_message("ep-live", "app"));
}
