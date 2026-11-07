//! Edge functions m9 end to end: an external service (the stub of
//! `infra/compose/fixtures/stub-service.yml`, go-httpbin) registered through the management
//! API as the dev member (a tenant admin) and called through the gateway with the branch's
//! public key. Skipped, saying so, when the stack runs without that overlay
//! (`CHELABASE_STUB_SERVICE_URL` unset).

use std::time::{Duration, Instant};

use axum::http::{HeaderMap, HeaderName, HeaderValue};
use functions::services::signature::Verifier;
use serde_json::{Value, json};

use super::Stack;

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

impl Stack {
    /// Makes service `name` at `url` (health at `health`), enabled; its signing secret.
    async fn service(&self, name: &str, url: &str, health: &str) -> String {
        let made = self
            .mgmt
            .ok(
                reqwest::Method::PUT,
                &self.functions(&format!("/{name}")),
                Some(json!({"kind": "service"})),
                &[201],
            )
            .await;
        let secret = made["signing_secret"]
            .as_str()
            .expect("the secret")
            .to_owned();
        self.mgmt
            .ok(
                reqwest::Method::POST,
                &self.functions(&format!("/{name}/deployments")),
                Some(json!({"spec": {"backend": "external", "url": url, "health_path": health}})),
                &[201],
            )
            .await;
        self.patch(name, json!({"enabled": true})).await;
        secret
    }
}

/// go-httpbin's echo: the request's headers (`{Name: [values]}`) as a map.
fn echoed_headers(echo: &Value) -> HeaderMap {
    let mut h = HeaderMap::new();
    for (name, values) in echo["headers"].as_object().expect("headers") {
        for v in values.as_array().into_iter().flatten() {
            h.append(
                HeaderName::from_bytes(name.to_ascii_lowercase().as_bytes()).unwrap(),
                HeaderValue::from_str(v.as_str().unwrap()).unwrap(),
            );
        }
    }
    h
}

async fn flow(s: &Stack, stub: &str) {
    // AC 1: the stub gets the rest and the query under the base path, signed.
    let secret = s
        .service("stub", &format!("{stub}/anything"), "/status/200")
        .await;
    let (status, body) = s
        .within("/services/v1/stub/a/b?x=1", true, |st, _| st == 200)
        .await;
    assert_eq!(status, 200, "{body}");
    let echo: Value = serde_json::from_str(&body).unwrap();
    let url = echo["url"].as_str().unwrap();
    let target = &url[url.find("/anything").expect("the base path")..];
    assert_eq!(target, "/anything/a/b?x=1");
    let headers = echoed_headers(&echo);
    assert_eq!(
        headers.get("chelabase-branch").unwrap(),
        s.branch.to_string().as_str()
    );
    assert_eq!(headers.get("chelabase-role").unwrap(), "anon");
    assert!(
        headers.get("authorization").is_none(),
        "the key stays at the gateway"
    );
    let v = Verifier::new(&secret);
    assert_eq!(v.verify("GET", target, &headers, b"", now()), Ok(()));
    assert!(
        v.verify("GET", target, &headers, b"", now()).is_err(),
        "a replay"
    );

    // A client's identity headers never reach the service.
    let resp = s
        .http
        .get(format!("{}/services/v1/stub/x", s.api))
        .bearer_auth(&s.key)
        .header("chelabase-user", "admin")
        .send()
        .await
        .unwrap();
    let echo: Value = resp.json().await.unwrap();
    assert_eq!(echoed_headers(&echo).get("chelabase-user").unwrap(), "");

    // AC 5: a redirect to an internal address comes back as it is. `root` is the stub's root.
    s.service("root", stub, "/status/200").await;
    let redirect = "/services/v1/root/redirect-to?url=http%3A%2F%2Fmetadata-db%3A5432%2F&status_code=302";
    let start = Instant::now();
    let resp = loop {
        let r = s
            .http
            .get(format!("{}{redirect}", s.api))
            .bearer_auth(&s.key)
            .send()
            .await
            .unwrap();
        if r.status() != 404 || start.elapsed() > Duration::from_secs(2) {
            break r;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert_eq!(resp.status(), 302);
    assert_eq!(resp.headers()["location"], "http://metadata-db:5432/");

    // AC 6: failing health checks (every 15 s, 3 in a row) make calls answer 503. Meanwhile
    // AC 11: a response streaming for 45 s.
    s.service("sick", &format!("{stub}/status"), "/503").await;
    let drip = {
        let (http, url, key) = (
            s.http.clone(),
            format!("{}/services/v1/root/drip", s.api),
            s.key.clone(),
        );
        tokio::spawn(async move {
            let started = Instant::now();
            let resp = http
                .get(format!("{url}?duration=45&numbytes=45&delay=0"))
                .bearer_auth(key)
                .send()
                .await
                .unwrap();
            let status = resp.status();
            let body = resp.bytes().await.unwrap();
            (status, body.len(), started.elapsed())
        })
    };
    let (status, body) = {
        let start = Instant::now();
        loop {
            let (status, body) = s.call("/services/v1/sick", true).await;
            if body.contains("service_unhealthy") || start.elapsed() > Duration::from_secs(75) {
                break (status, body);
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    };
    assert_eq!(status, 503, "{body}");
    assert!(body.contains("service_unhealthy"), "{body}");
    assert_eq!(s.listed("sick").await["health"], "unhealthy");
    assert_eq!(s.listed("stub").await["health"], "healthy");

    // AC 11: it arrived whole.
    let (status, len, took) = drip.await.unwrap();
    assert_eq!((status.as_u16(), len), (200, 45), "after {took:?}");
    assert!(took >= Duration::from_secs(40), "{took:?}");

    // Disabled: 503 service_disabled.
    s.patch("stub", json!({"enabled": false})).await;
    let (status, body) = s
        .within("/services/v1/stub/a", true, |_, b| {
            b.contains("service_disabled")
        })
        .await;
    assert_eq!(status, 503, "{body}");
}

#[tokio::test]
#[ignore = "needs the workbench stack with infra/compose/fixtures/stub-service.yml: just functions-e2e"]
async fn an_external_service_end_to_end() {
    let Some(stub) = std::env::var("CHELABASE_STUB_SERVICE_URL")
        .ok()
        .filter(|v| !v.trim().is_empty())
    else {
        eprintln!(
            "SKIPPED: CHELABASE_STUB_SERVICE_URL is unset (start the stack with infra/compose/fixtures/stub-service.yml)"
        );
        return;
    };
    let stack = std::sync::Arc::new(Stack::new().await);
    let run = stack.clone();
    let result = tokio::spawn(async move { flow(&run, stub.trim_end_matches('/')).await }).await;
    stack.cleanup().await;
    if let Err(e) = result {
        std::panic::resume_unwind(e.into_panic());
    }
}
