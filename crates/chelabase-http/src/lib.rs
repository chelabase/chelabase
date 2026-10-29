//! The one place a `reqwest` client starts (#26, #32).
//!
//! Every client a service or tool builds sends a credential (a PAT, a bearer, an internal
//! token, a NeonJWT, a data key) or a forced Host, so none of them may follow a redirect: it
//! would take the credential, or the answer the caller trusts, to wherever the upstream points.
//! `reqwest` follows up to 10 redirects by default.
//!
//! [`Client`] wraps a `reqwest::Client` that refuses redirects, and [`client_builder`] is the
//! only way to make one. The wrapper has no `Default`, no `Deref`, and no conversion to or from
//! a `reqwest::Client`, so the inner client never leaves this crate. [`ClientBuilder`] exposes
//! only `timeout`, `default_headers` and `build`: nothing that changes the redirect policy.
//!
//! The workspace `clippy.toml` disallows the `reqwest::Client` and `reqwest::ClientBuilder`
//! types themselves (so an inferred `Default::default()` field fails too), every other
//! constructor, `reqwest::get` and `ClientBuilder::redirect`. This crate is the only production
//! code allowed to name them; `scripts/tests/http-no-redirects.sh` fails any other allow.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::time::Duration;

pub use reqwest::{IntoUrl, Method, RequestBuilder};

/// A `reqwest` client that never follows a redirect: a 3xx comes back to the caller as the
/// response. Cheap to clone (the connection pool is shared). Built only by [`client_builder`].
#[derive(Clone, Debug)]
pub struct Client(reqwest::Client);

impl Client {
    /// A `GET` to `url`.
    pub fn get<U: IntoUrl>(&self, url: U) -> RequestBuilder {
        self.0.get(url)
    }

    /// A `POST` to `url`.
    pub fn post<U: IntoUrl>(&self, url: U) -> RequestBuilder {
        self.0.post(url)
    }

    /// A `PUT` to `url`.
    pub fn put<U: IntoUrl>(&self, url: U) -> RequestBuilder {
        self.0.put(url)
    }

    /// A `PATCH` to `url`.
    pub fn patch<U: IntoUrl>(&self, url: U) -> RequestBuilder {
        self.0.patch(url)
    }

    /// A `DELETE` to `url`.
    pub fn delete<U: IntoUrl>(&self, url: U) -> RequestBuilder {
        self.0.delete(url)
    }

    /// A request with `method` to `url`.
    pub fn request<U: IntoUrl>(&self, method: Method, url: U) -> RequestBuilder {
        self.0.request(method, url)
    }
}

/// Builds a [`Client`]. The redirect policy is fixed at `Policy::none()`; only the settings in
/// use are exposed, and none of them touches redirects.
#[derive(Debug)]
#[must_use]
pub struct ClientBuilder(reqwest::ClientBuilder);

impl ClientBuilder {
    /// The total timeout for each request.
    pub fn timeout(self, timeout: Duration) -> Self {
        Self(self.0.timeout(timeout))
    }

    /// Headers sent on every request.
    pub fn default_headers(self, headers: reqwest::header::HeaderMap) -> Self {
        Self(self.0.default_headers(headers))
    }

    /// The client.
    pub fn build(self) -> reqwest::Result<Client> {
        self.0.build().map(Client)
    }
}

/// A [`ClientBuilder`] that never follows a redirect. Set a timeout or headers on it, then
/// `build()`.
pub fn client_builder() -> ClientBuilder {
    ClientBuilder(reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()))
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// A server that answers every request with `status` and a `location` back to itself, and
    /// counts the requests it gets.
    async fn redirecting(
        status: &'static str,
    ) -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = hits.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf).await;
                let reply = format!(
                    "HTTP/1.1 {status}\r\nlocation: http://{addr}/elsewhere\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                );
                let _ = sock.write_all(reply.as_bytes()).await;
            }
        });
        (format!("http://{addr}/start"), hits)
    }

    /// `req` came back as the `status` response itself, after one request.
    async fn unfollowed(
        req: reqwest::RequestBuilder,
        status: &str,
        hits: &std::sync::atomic::AtomicUsize,
        what: &str,
    ) {
        let resp = req.send().await.unwrap();
        assert_eq!(resp.status().as_str(), &status[..3], "{what}");
        assert_eq!(
            resp.url().path(),
            "/start",
            "{what}: the client followed the redirect"
        );
        assert_eq!(
            hits.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "{what}: one request only"
        );
    }

    #[tokio::test]
    async fn a_redirect_comes_back_as_the_response() {
        for status in [
            "301 Moved Permanently",
            "302 Found",
            "303 See Other",
            "307 Temporary Redirect",
            "308 Permanent Redirect",
        ] {
            let (url, hits) = redirecting(status).await;
            let http: super::Client = super::client_builder().build().unwrap();
            unfollowed(http.get(&url), status, &hits, status).await;
        }
    }

    #[tokio::test]
    async fn forwarded_methods_keep_the_policy() {
        let http = super::client_builder()
            .timeout(std::time::Duration::from_secs(10))
            .default_headers(reqwest::header::HeaderMap::new())
            .build()
            .unwrap();
        type Forwarder = fn(&super::Client, &str) -> reqwest::RequestBuilder;
        let forwarders: [(&str, Forwarder); 6] = [
            ("get", |c, u| c.get(u)),
            ("post", |c, u| c.post(u)),
            ("put", |c, u| c.put(u)),
            ("patch", |c, u| c.patch(u)),
            ("delete", |c, u| c.delete(u)),
            ("request", |c, u| c.request(super::Method::OPTIONS, u)),
        ];
        for (name, forward) in forwarders {
            let (url, hits) = redirecting("302 Found").await;
            // A clone shares the inner client, policy included.
            unfollowed(forward(&http.clone(), &url), "302 Found", &hits, name).await;
        }
    }
}
