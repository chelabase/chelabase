//! Health checks of external services: every [`INTERVAL`], a `GET` of each enabled service's
//! base URL with its `health_path`, through the services' guarded client. A 2xx within
//! [`CHECK_TIMEOUT`] passes. [`FAILURES`] failures in a row make a service `unhealthy` (calls
//! answer 503 `service_unhealthy`); one pass makes it `healthy`. A service not checked yet is
//! `unknown` and is called. Each change is recorded in the metadata, for the API.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chelabase_http::outbound::{PublicClient, Url};
use uuid::Uuid;

use super::target::upstream;
use crate::directory::Directory;

pub const INTERVAL: Duration = Duration::from_secs(15);
pub const CHECK_TIMEOUT: Duration = Duration::from_secs(5);
pub const FAILURES: u32 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Unknown,
    Healthy,
    Unhealthy,
}

#[derive(Debug, Clone, Copy)]
struct Entry {
    status: Status,
    failures: u32,
}

/// Each deployment's health, as this process last saw it.
#[derive(Default)]
pub struct Health {
    entries: Mutex<HashMap<Uuid, Entry>>,
}

impl Health {
    pub fn status(&self, deployment: Uuid) -> Status {
        self.entries
            .lock()
            .expect("not poisoned")
            .get(&deployment)
            .map_or(Status::Unknown, |e| e.status)
    }

    /// Records a check; the new status when it changed.
    pub fn observe(&self, deployment: Uuid, passed: bool) -> Option<Status> {
        let mut entries = self.entries.lock().expect("not poisoned");
        let e = entries.entry(deployment).or_insert(Entry {
            status: Status::Unknown,
            failures: 0,
        });
        let before = e.status;
        if passed {
            e.failures = 0;
            e.status = Status::Healthy;
        } else {
            e.failures = e.failures.saturating_add(1);
            if e.failures >= FAILURES {
                e.status = Status::Unhealthy;
            }
        }
        (e.status != before).then_some(e.status)
    }

    /// Forgets every deployment not in `keep`.
    fn retain(&self, keep: &HashSet<Uuid>) {
        self.entries
            .lock()
            .expect("not poisoned")
            .retain(|d, _| keep.contains(d));
    }
}

/// The health URL of an external spec (`{backend: "external", url, health_path}`).
pub fn health_url(spec: &serde_json::Value) -> Option<Url> {
    let base = Url::parse(spec.get("url")?.as_str()?).ok()?;
    let path = spec.get("health_path")?.as_str()?;
    let (path, query) = match path.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (path, None),
    };
    Some(upstream(&base, path, query)?.url)
}

async fn passes(client: &PublicClient, url: &Url) -> bool {
    let Ok(req) = client.request(reqwest::Method::GET, url.as_str()) else {
        return false;
    };
    match PublicClient::send_within(req.timeout(CHECK_TIMEOUT), CHECK_TIMEOUT).await {
        Some(Ok(resp)) => resp.status().is_success(),
        _ => false,
    }
}

/// One round: every target checked at once, each change recorded.
pub async fn check_all(directory: &dyn Directory, client: &PublicClient, health: &Health) {
    let targets = match directory.service_targets().await {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!("list services to check: {e}");
            return;
        }
    };
    health.retain(&targets.iter().map(|t| t.deployment_id).collect());
    let checks = targets.iter().map(|t| async move {
        let passed = match health_url(&t.spec) {
            Some(url) => passes(client, &url).await,
            None => false,
        };
        (t.deployment_id, passed)
    });
    for (deployment, passed) in futures_util::future::join_all(checks).await {
        let Some(status) = health.observe(deployment, passed) else {
            continue;
        };
        tracing::info!(%deployment, ?status, "service health changed");
        if let Err(e) = directory
            .record_health(deployment, status == Status::Healthy)
            .await
        {
            tracing::warn!(%deployment, "record service health: {e}");
        }
    }
}

/// Checks every [`INTERVAL`] for as long as the service runs.
pub fn spawn(directory: Arc<dyn Directory>, client: PublicClient, health: Arc<Health>) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(INTERVAL);
        loop {
            tick.tick().await;
            check_all(directory.as_ref(), &client, &health).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn three_failures_in_a_row_make_a_service_unhealthy() {
        let h = Health::default();
        let d = Uuid::new_v4();
        assert_eq!(h.status(d), Status::Unknown);
        assert_eq!(h.observe(d, false), None);
        assert_eq!(h.observe(d, false), None);
        assert_eq!(h.status(d), Status::Unknown, "two failures aren't enough");
        assert_eq!(h.observe(d, false), Some(Status::Unhealthy));
        assert_eq!(h.observe(d, false), None);
        assert_eq!(
            h.observe(d, true),
            Some(Status::Healthy),
            "one pass recovers"
        );
        assert_eq!(h.observe(d, false), None);
        assert_eq!(h.observe(d, false), None);
        assert_eq!(h.observe(d, true), None, "the run of failures restarts");
        assert_eq!(h.observe(d, false), None);
        assert_eq!(h.observe(d, false), None);
        assert_eq!(h.status(d), Status::Healthy);
        assert_eq!(h.observe(d, false), Some(Status::Unhealthy));
        h.retain(&HashSet::new());
        assert_eq!(
            h.status(d),
            Status::Unknown,
            "a service no longer served is forgotten"
        );
    }

    #[test]
    fn the_health_url_sits_under_the_base_path() {
        let spec = |url: &str, path: &str| serde_json::json!({"backend": "external", "url": url, "health_path": path});
        let got = |s| health_url(&s).map(|u| u.to_string());
        assert_eq!(
            got(spec("https://svc.example/api", "/healthz")).as_deref(),
            Some("https://svc.example/api/healthz")
        );
        assert_eq!(
            got(spec("http://svc:8080", "/status/200?x=1")).as_deref(),
            Some("http://svc:8080/status/200?x=1")
        );
        assert_eq!(
            got(spec("https://svc.example/api", "/../admin")).as_deref(),
            Some("https://svc.example/api/admin")
        );
        assert_eq!(
            health_url(&serde_json::json!({"backend": "external"})),
            None
        );
    }
}
