//! The bootstrap's Zitadel client: REST v2 paths and ConnectRPC JSON, with the admin PAT.
//!
//! Every failure is an [`ApiError`] (inside the `anyhow::Error`), so the retry loop can tell a
//! Zitadel that isn't up yet ([`ApiError::unavailable`]) from a real refusal.

use anyhow::Result;
use reqwest::Method;
use serde_json::Value;
use std::future::Future;

/// One failed Zitadel call: no answer at all (`status` `None`), or a non-2xx answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiError {
    /// The call, as `"POST /v2/users"` or `"Service/Method"`.
    pub what: String,
    pub status: Option<u16>,
    /// The answer's body, or the transport error.
    pub body: String,
}

impl ApiError {
    pub fn new(what: impl Into<String>, status: Option<u16>, body: impl Into<String>) -> Self {
        Self {
            what: what.into(),
            status,
            body: body.into(),
        }
    }

    /// Zitadel (or the way to it) isn't up: no connection, or 502/503/504. Nothing else is,
    /// so a 4xx on a valid request (a 404 included) is never waited on.
    pub fn unavailable(&self) -> bool {
        matches!(self.status, None | Some(502..=504))
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.status {
            Some(status) => write!(f, "{}: {status} {}", self.what, self.body),
            None => write!(f, "{}: no answer ({})", self.what, self.body),
        }
    }
}

impl std::error::Error for ApiError {}

/// The [`ApiError`] in `err`'s chain, if any.
pub fn api_error(err: &anyhow::Error) -> Option<&ApiError> {
    err.chain().find_map(|e| e.downcast_ref::<ApiError>())
}

pub trait ZitadelApi: Send + Sync {
    /// `method` `{base}{path}` with a JSON body; the JSON answer.
    fn rest(
        &self,
        method: Method,
        path: &str,
        body: Value,
    ) -> impl Future<Output = Result<Value>> + Send;
    /// ConnectRPC unary call: `POST {base}/{service}/{method}` with a JSON body.
    fn connect(
        &self,
        service: &str,
        method: &str,
        body: Value,
    ) -> impl Future<Output = Result<Value>> + Send;
    /// Zitadel's readiness probe (`GET /debug/ready`, no credential): `Ok` once it is ready.
    fn ready(&self) -> impl Future<Output = Result<(), ApiError>> + Send;
}

pub struct HttpZitadel {
    http: chelabase_http::Client,
    /// Where requests go (no trailing slash).
    base: String,
    /// The `Host` header, when it differs from `base`'s (Zitadel picks its instance by Host).
    host: Option<String>,
    pat: String,
}

/// Each call's own limit: a Zitadel that accepts the connection but never answers is retried.
const CALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

impl HttpZitadel {
    /// Zitadel at its upstream (`upstream_url`, see `entry::zitadel_upstream`), addressed by the
    /// public auth host: no gateway is needed, so the bootstrap runs before it.
    pub fn via_upstream(upstream_url: &str, auth_host: &str, pat: &str) -> Self {
        Self {
            // Never a redirect (#26): the admin PAT and the forced Host go to the upstream only.
            // A 3xx is an answer, not "not ready": the step fails at once, naming it.
            http: chelabase_http::client_builder()
                .timeout(CALL_TIMEOUT)
                .build()
                .expect("the bootstrap's HTTP client builds"),
            base: upstream_url.trim_end_matches('/').to_string(),
            host: Some(auth_host.to_string()),
            pat: pat.to_string(),
        }
    }

    async fn answer(
        &self,
        mut request: reqwest::RequestBuilder,
        what: &str,
    ) -> Result<(u16, String), ApiError> {
        if let Some(host) = &self.host {
            request = request.header(reqwest::header::HOST, host);
        }
        let response = request
            .send()
            .await
            .map_err(|e| ApiError::new(what, None, transport(&e)))?;
        let status = response.status().as_u16();
        if response.status().is_redirection() {
            let to = response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|l| l.to_str().ok())
                .unwrap_or("nowhere")
                .to_string();
            return Ok((status, format!("a redirect to {to}, not followed")));
        }
        let text = response.text().await.unwrap_or_default();
        Ok((status, text))
    }

    async fn send(&self, request: reqwest::RequestBuilder, what: &str) -> Result<Value> {
        let (status, text) = self.answer(request.bearer_auth(&self.pat), what).await?;
        if !(200..300).contains(&status) {
            return Err(ApiError::new(what, Some(status), text).into());
        }
        if text.trim().is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_str(&text)
            .map_err(|_| anyhow::anyhow!("{what}: the answer is not JSON ({} bytes)", text.len()))
    }
}

/// A transport error with its causes (reqwest's top-level message alone says little).
fn transport(e: &reqwest::Error) -> String {
    let mut text = e.to_string();
    let mut source = std::error::Error::source(e);
    while let Some(s) = source {
        text.push_str(": ");
        text.push_str(&s.to_string());
        source = s.source();
    }
    text
}

impl ZitadelApi for HttpZitadel {
    async fn rest(&self, method: Method, path: &str, body: Value) -> Result<Value> {
        let what = format!("{method} {path}");
        let mut request = self
            .http
            .request(method.clone(), format!("{}{path}", self.base));
        if method != Method::GET && method != Method::DELETE {
            request = request.json(&body);
        }
        self.send(request, &what).await
    }

    async fn connect(&self, service: &str, method: &str, body: Value) -> Result<Value> {
        let request = self
            .http
            .post(format!("{}/{service}/{method}", self.base))
            .header("Connect-Protocol-Version", "1")
            .json(&body);
        self.send(request, &format!("{service}/{method}")).await
    }

    async fn ready(&self) -> Result<(), ApiError> {
        let what = "GET /debug/ready";
        let (status, text) = self
            .answer(self.http.get(format!("{}/debug/ready", self.base)), what)
            .await?;
        if (200..300).contains(&status) {
            Ok(())
        } else {
            Err(ApiError::new(what, Some(status), text))
        }
    }
}

#[cfg(test)]
mod upstream_tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// One connection's request head, answered with `answer`.
    async fn serve_once(answer: &'static [u8]) -> (String, tokio::task::JoinHandle<String>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 8192];
            let n = sock.read(&mut buf).await.unwrap();
            sock.write_all(answer).await.unwrap();
            String::from_utf8_lossy(&buf[..n]).to_string()
        });
        (upstream, server)
    }

    // End-of-step review, critical 1: the bootstrap reaches Zitadel at its upstream with the
    // auth host as Host (Zitadel picks its instance by Host), so it needs no gateway running.
    #[tokio::test]
    async fn the_bootstrap_client_calls_the_upstream_with_the_auth_host() {
        let (upstream, server) = serve_once(
            b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}",
        )
        .await;
        let api = HttpZitadel::via_upstream(&upstream, "auth.chelabase.localhost:8000", "pat");
        api.rest(Method::GET, "/management/v1/orgs/me", serde_json::json!({}))
            .await
            .unwrap();
        let head = server.await.unwrap().to_ascii_lowercase();
        assert!(
            head.starts_with("get /management/v1/orgs/me http/1.1\r\n"),
            "{head}"
        );
        assert!(
            head.contains("\r\nhost: auth.chelabase.localhost:8000\r\n"),
            "{head}"
        );
        assert!(head.contains("\r\nauthorization: bearer pat\r\n"), "{head}");
    }

    // The readiness probe sends no credential.
    #[tokio::test]
    async fn the_readiness_probe_sends_no_pat() {
        let (upstream, server) =
            serve_once(b"HTTP/1.1 200 OK\r\ncontent-length: 4\r\nconnection: close\r\n\r\n\"ok\"")
                .await;
        let api = HttpZitadel::via_upstream(&upstream, "auth.chelabase.localhost:8000", "pat");
        api.ready().await.unwrap();
        let head = server.await.unwrap().to_ascii_lowercase();
        assert!(head.starts_with("get /debug/ready http/1.1\r\n"), "{head}");
        assert!(!head.contains("authorization"), "{head}");
    }

    // A refused connection and a 503 are "not up yet"; a 404 or a 400 is an answer.
    #[tokio::test]
    async fn only_no_answer_and_5xx_gateways_are_unavailable() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let closed = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        let api = HttpZitadel::via_upstream(&closed, "auth.x", "pat");
        let err = api.rest(Method::GET, "/x", Value::Null).await.unwrap_err();
        assert!(api_error(&err).unwrap().unavailable(), "{err:#}");
        for (status, unavailable) in [
            (503, true),
            (502, true),
            (504, true),
            (404, false),
            (400, false),
            (409, false),
            (500, false),
            (401, false),
        ] {
            assert_eq!(
                ApiError::new("x", Some(status), "").unavailable(),
                unavailable,
                "{status}"
            );
        }
    }

    /// A server answering every request with `head` (no body), counting its connections.
    async fn serve_all(head: String) -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = hits.clone();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let head = head.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 8192];
                    let _ = sock.read(&mut buf).await;
                    let _ = sock.write_all(head.as_bytes()).await;
                });
            }
        });
        (base, hits)
    }

    // #26 (m13 follow-up): the bootstrap carries the admin PAT and the forced Host, so it never
    // follows a redirect. A 3xx is an answer, not "Zitadel isn't up": the step fails at once,
    // named, and the redirect's target (another origin with a working answer) is never asked.
    #[tokio::test]
    async fn a_redirect_is_never_followed_and_fails_the_step() {
        let ok = "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}";
        let (target, target_hits) = serve_all(ok.to_string()).await;
        for status in ["301 Moved Permanently", "302 Found", "303 See Other", "307 Temporary Redirect", "308 Permanent Redirect"] {
            let head = format!(
                "HTTP/1.1 {status}\r\nlocation: {target}/v2/users\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
            );
            let (upstream, _) = serve_all(head).await;
            let z = HttpZitadel::via_upstream(&upstream, "auth.example.com", "the-admin-pat");
            let code: u16 = status[..3].parse().unwrap();
            for err in [
                z.rest(Method::POST, "/v2/users", serde_json::json!({})).await.unwrap_err(),
                z.connect("zitadel.org.v2.OrganizationService", "ListOrganizations", serde_json::json!({}))
                    .await
                    .unwrap_err(),
            ] {
                let api = api_error(&err).unwrap_or_else(|| panic!("{status}: not an ApiError: {err:#}"));
                assert_eq!(api.status, Some(code), "{status}: {api}");
                assert!(!api.unavailable(), "{status}: a redirect would be waited on as not ready");
                assert!(!api.what.is_empty(), "{status}: the error names no call");
            }
            let ready = z.ready().await.unwrap_err();
            assert_eq!(ready.status, Some(code), "{status}: {ready}");
            // wait_for_zitadel waits only on unavailable() or a 404.
            assert!(!ready.unavailable() && ready.status != Some(404), "{status}: readiness would wait on it");
        }
        assert_eq!(target_hits.load(std::sync::atomic::Ordering::SeqCst), 0, "the redirect's target was asked");
    }
}
