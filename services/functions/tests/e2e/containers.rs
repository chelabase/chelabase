//! Edge functions m11 end to end: a container service (go-httpbin, pulled from Docker Hub)
//! deployed through the management API as the dev member (a tenant admin), run by the service
//! runner and called through the gateway with the branch's public key; a rollover to an image
//! that never passes its health check is refused and traffic stays; a healthy one switches;
//! logs; disable. Skipped, saying so, when the stack runs without the `services` profile (the
//! deploy answers 503 `runner_unavailable`).
//!
//! ```text
//! just functions-e2e   # or: chelabase workbench test -p chelabase-functions --test e2e -- --run-ignored only containers::
//! ```
//!
//! `CHELABASE_E2E_CONTAINER_IMAGE` overrides the image (it must serve go-httpbin's `/anything`
//! and `/status/<code>` on port 8080).

use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::Stack;

const IMAGE: &str = "mccutchen/go-httpbin:2.26.0";

impl Stack {
    /// Calls `path` until `done` holds, for at most `within`; the last answer.
    async fn until(
        &self,
        path: &str,
        within: Duration,
        done: impl Fn(u16, &str) -> bool,
    ) -> (u16, String) {
        let start = Instant::now();
        loop {
            let (status, body) = self.call(path, true).await;
            if done(status, &body) || start.elapsed() > within {
                return (status, body);
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }

    /// `box` once its rollover to `deployment` has ended (switched, or failed), for at most
    /// 3 minutes (a pull and the 60 s health wait).
    async fn rolled_over(&self, deployment: &str) -> Value {
        let start = Instant::now();
        loop {
            let st = self.listed("box").await;
            let pending =
                st["rollout"]["deployment_id"] == deployment && st["rollout"]["state"] == "pending";
            if !pending || start.elapsed() > Duration::from_secs(180) {
                return st;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }

    async fn deploy_container(&self, image: &str, health: &str) -> (u16, Value) {
        self.mgmt
            .call(
                reqwest::Method::POST,
                &self.functions("/box/deployments"),
                Some(json!({"spec": {"backend": "container", "image": image, "port": 8080, "health_path": health}})),
            )
            .await
    }
}

async fn flow(s: &Stack, image: &str) {
    s.mgmt
        .ok(
            reqwest::Method::PUT,
            &s.functions("/box"),
            Some(json!({"kind": "service"})),
            &[201],
        )
        .await;
    let (status, first) = s.deploy_container(image, "/status/200").await;
    if status == 503 && first["error"]["code"] == "runner_unavailable" {
        eprintln!(
            "SKIPPED: the service runner isn't running (start the stack with the services profile)"
        );
        return;
    }
    assert_eq!(status, 201, "{first}");
    assert!(
        first["spec"]["image"]
            .as_str()
            .unwrap()
            .contains("@sha256:"),
        "stored by digest: {first}"
    );
    s.patch("box", json!({"enabled": true})).await;

    // AC 1: within 60 s the container answers through the gateway, signed.
    let (status, body) = s
        .until(
            "/services/v1/box/anything/hi",
            Duration::from_secs(60),
            |st, _| st == 200,
        )
        .await;
    assert_eq!(status, 200, "{body}");
    let echo: Value = serde_json::from_str(&body).unwrap();
    assert!(
        echo["url"].as_str().unwrap().ends_with("/anything/hi"),
        "{echo}"
    );
    assert!(echo["headers"]["Chelabase-Signature"].is_array(), "{echo}");

    // AC 2: an image that never passes its health check: the deploy answers 202 at once (the
    // rollover runs in the background), then the rollover fails after the 60 s wait and traffic
    // stays on the old container.
    let started = Instant::now();
    let (status, body) = s.deploy_container(image, "/status/500").await;
    assert_eq!(status, 202, "{body}");
    assert_eq!(body["rollout"], "pending", "{body}");
    let failed = s.rolled_over(body["id"].as_str().unwrap()).await;
    assert_eq!(failed["rollout"]["state"], "failed", "{failed}");
    assert!(
        failed["rollout"]["error"]
            .as_str()
            .unwrap()
            .contains("/status/500"),
        "{failed}"
    );
    assert!(
        started.elapsed() >= Duration::from_secs(55),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(failed["deployment_id"], first["id"]);
    assert!(
        failed["last_error"]
            .as_str()
            .unwrap()
            .contains("wasn't switched"),
        "{failed}"
    );
    let (status, body) = s.call("/services/v1/box/anything/still", true).await;
    assert_eq!(status, 200, "{body}");

    // A healthy new deployment switches once its container passes.
    let (status, second) = s.deploy_container(image, "/status/204").await;
    assert_eq!(status, 202, "{second}");
    let done = s.rolled_over(second["id"].as_str().unwrap()).await;
    assert!(done["rollout"].is_null(), "{done}");
    assert_eq!(done["deployment_id"], second["id"]);
    let (status, body) = s
        .until(
            "/services/v1/box/anything/next",
            Duration::from_secs(20),
            |st, _| st == 200,
        )
        .await;
    assert_eq!(status, 200, "{body}");

    // Logs: the container's own output.
    let (status, logs) = s
        .mgmt
        .call(reqwest::Method::GET, &s.functions("/box/logs"), None)
        .await;
    assert_eq!(status, 200, "{logs}");
    assert!(!logs.as_str().unwrap_or_default().is_empty());

    // AC 3: off → 503 within 2 s (the runner stops the container within its reconcile).
    s.patch("box", json!({"enabled": false})).await;
    let (status, body) = s
        .until(
            "/services/v1/box/anything",
            Duration::from_secs(2),
            |_, b| b.contains("service_disabled"),
        )
        .await;
    assert_eq!(status, 503, "{body}");
}

#[tokio::test]
#[ignore = "needs the workbench stack with the services profile: just functions-e2e"]
async fn a_container_service_end_to_end() {
    let image = std::env::var("CHELABASE_E2E_CONTAINER_IMAGE")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| IMAGE.to_owned());
    let stack = std::sync::Arc::new(Stack::new().await);
    let run = stack.clone();
    let result = tokio::spawn(async move { flow(&run, &image).await }).await;
    stack.cleanup().await;
    if let Err(e) = result {
        std::panic::resume_unwind(e.into_panic());
    }
}
