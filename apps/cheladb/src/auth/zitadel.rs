//! The bootstrap's Zitadel client: REST v2 paths and ConnectRPC JSON, with the admin PAT.

use anyhow::{Context, Result, bail};
use reqwest::Method;
use serde_json::Value;
use std::future::Future;

pub trait ZitadelApi: Send + Sync {
    /// `method` `{issuer}{path}` with a JSON body; the JSON answer.
    fn rest(
        &self,
        method: Method,
        path: &str,
        body: Value,
    ) -> impl Future<Output = Result<Value>> + Send;
    /// ConnectRPC unary call: `POST {issuer}/{service}/{method}` with a JSON body.
    fn connect(
        &self,
        service: &str,
        method: &str,
        body: Value,
    ) -> impl Future<Output = Result<Value>> + Send;
}

pub struct HttpZitadel {
    http: reqwest::Client,
    issuer: String,
    pat: String,
}

impl HttpZitadel {
    pub fn new(issuer: &str, pat: &str) -> Self {
        Self {
            http: reqwest::Client::new(),
            issuer: issuer.trim_end_matches('/').to_string(),
            pat: pat.to_string(),
        }
    }

    async fn send(&self, request: reqwest::RequestBuilder, what: &str) -> Result<Value> {
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
            .request(method.clone(), format!("{}{path}", self.issuer));
        if method != Method::GET {
            request = request.json(&body);
        }
        self.send(request, &what).await
    }

    async fn connect(&self, service: &str, method: &str, body: Value) -> Result<Value> {
        let request = self
            .http
            .post(format!("{}/{service}/{method}", self.issuer))
            .header("Connect-Protocol-Version", "1")
            .json(&body);
        self.send(request, &format!("{service}/{method}")).await
    }
}
