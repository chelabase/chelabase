//! The bootstrap's Zitadel client: REST v2 paths and ConnectRPC JSON, with the admin PAT.

use anyhow::{Context, Result, bail};
use reqwest::Method;
use serde_json::Value;
use std::future::Future;

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
}

pub struct HttpZitadel {
    http: reqwest::Client,
    /// Where requests go (no trailing slash).
    base: String,
    /// The `Host` header, when it differs from `base`'s (Zitadel picks its instance by Host).
    host: Option<String>,
    pat: String,
}

impl HttpZitadel {
    /// Zitadel at its upstream (`upstream_url`, see `entry::zitadel_upstream`), addressed by the
    /// public auth host: no gateway is needed, so the bootstrap runs before it.
    pub fn via_upstream(upstream_url: &str, auth_host: &str, pat: &str) -> Self {
        Self {
            http: reqwest::Client::new(),
            base: upstream_url.trim_end_matches('/').to_string(),
            host: Some(auth_host.to_string()),
            pat: pat.to_string(),
        }
    }

    async fn send(&self, mut request: reqwest::RequestBuilder, what: &str) -> Result<Value> {
        if let Some(host) = &self.host {
            request = request.header(reqwest::header::HOST, host);
        }
        let response = request
            .bearer_auth(&self.pat)
            .send()
            .await
            .with_context(|| what.to_string())?;
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        if !status.is_success() {
            bail!("{what}: {status} {text}");
        }
        if text.trim().is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_str(&text).with_context(|| format!("{what}: not JSON: {text}"))
    }
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
}

#[cfg(test)]
mod upstream_tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    // End-of-step review, critical 1: the bootstrap reaches Zitadel at its upstream with the
    // auth host as Host (Zitadel picks its instance by Host), so it needs no gateway running.
    #[tokio::test]
    async fn the_bootstrap_client_calls_the_upstream_with_the_auth_host() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 8192];
            let n = sock.read(&mut buf).await.unwrap();
            sock.write_all(
                b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}",
            )
            .await
            .unwrap();
            String::from_utf8_lossy(&buf[..n]).to_string()
        });
        let api = HttpZitadel::via_upstream(&upstream, "auth.chelabase.localhost:8000", "pat");
        api.rest(Method::GET, "/management/v1/orgs/me", serde_json::json!({})).await.unwrap();
        let head = server.await.unwrap().to_ascii_lowercase();
        assert!(head.starts_with("get /management/v1/orgs/me http/1.1\r\n"), "{head}");
        assert!(head.contains("\r\nhost: auth.chelabase.localhost:8000\r\n"), "{head}");
        assert!(head.contains("\r\nauthorization: bearer pat\r\n"), "{head}");
    }
}
