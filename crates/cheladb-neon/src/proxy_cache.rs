//! Clearing Neon's proxy role-secret cache. The control plane publishes on the channel every
//! proxy subscribes to (neon `proxy/src/redis/notifications.rs` at `infra/neon/NEON_REF`).
//! The proxy drops the role at once, and again 20 s later in case a lookup was in flight.

use std::time::Duration;

use anyhow::Context;
use serde_json::json;

/// The channel the proxy subscribes to.
pub const CHANNEL: &str = "neondb-proxy-ws-updates";

/// `/password_updated` for one role. `data` is itself a JSON string, and `project_id` is the
/// endpoint id our access-control answer gives the proxy.
pub fn password_updated_message(endpoint_id: &str, role: &str) -> String {
    json!({
        "topic": "/password_updated",
        "data": json!({ "project_id": endpoint_id, "role_name": role }).to_string(),
    })
    .to_string()
}

/// Bounds connecting to Valkey and publishing, together.
const PUBLISH_TIMEOUT: Duration = Duration::from_secs(5);

/// Publishes cache-clearing messages to Valkey (any Redis-protocol server).
#[derive(Clone)]
pub struct ProxyCache {
    client: redis::Client,
}

impl ProxyCache {
    /// `redis://host:port`. Nothing is contacted yet.
    pub fn new(url: &str) -> anyhow::Result<Self> {
        Ok(Self {
            client: redis::Client::open(url).context("Valkey URL")?,
        })
    }

    /// Tells every proxy to drop its cached secret for `role`. Returns how many subscribers
    /// got the message; 0 means no proxy is listening, and the old secret stays cached until
    /// its TTL runs out. Connecting and publishing together give up after 5 s.
    pub async fn password_updated(&self, endpoint_id: &str, role: &str) -> anyhow::Result<u64> {
        self.password_updated_within(endpoint_id, role, PUBLISH_TIMEOUT)
            .await
    }

    async fn password_updated_within(
        &self,
        endpoint_id: &str,
        role: &str,
        timeout: Duration,
    ) -> anyhow::Result<u64> {
        tokio::time::timeout(timeout, self.publish(endpoint_id, role))
            .await
            .map_err(|_| anyhow::anyhow!("Valkey did not answer within {timeout:?}"))?
    }

    async fn publish(&self, endpoint_id: &str, role: &str) -> anyhow::Result<u64> {
        let mut conn = self
            .client
            .get_multiplexed_async_connection()
            .await
            .context("connecting to Valkey")?;
        let receivers: u64 = redis::cmd("PUBLISH")
            .arg(CHANNEL)
            .arg(password_updated_message(endpoint_id, role))
            .query_async(&mut conn)
            .await
            .context("PUBLISH to Valkey")?;
        Ok(receivers)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    // The shape neon's `Notification` parses: tag `topic`, and `data` as a JSON string.
    #[test]
    fn the_message_is_what_the_proxy_parses() {
        let v: Value = serde_json::from_str(&password_updated_message("ep-a", "app")).unwrap();
        assert_eq!(v["topic"], "/password_updated");
        let data: Value =
            serde_json::from_str(v["data"].as_str().expect("data is a string")).unwrap();
        assert_eq!(data, json!({ "project_id": "ep-a", "role_name": "app" }));
    }

    #[test]
    fn a_bad_url_is_refused() {
        assert!(ProxyCache::new("not a url").is_err());
    }

    // A server that accepts TCP and never answers must not hang the caller.
    #[tokio::test]
    async fn a_silent_server_times_out() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let cache =
            ProxyCache::new(&format!("redis://{}", listener.local_addr().unwrap())).unwrap();
        let started = std::time::Instant::now();
        let err = cache
            .password_updated_within("ep-a", "app", Duration::from_millis(300))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("did not answer"), "{err:#}");
        assert!(started.elapsed() < Duration::from_secs(3));
        drop(listener);
    }
}
