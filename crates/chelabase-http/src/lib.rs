//! The one place a `reqwest` client starts (#26).
//!
//! Every client a service or tool builds sends a credential (a PAT, a bearer, an internal
//! token, a NeonJWT, a data key) or a forced Host, so none of them may follow a redirect: it
//! would take the credential, or the answer the caller trusts, to wherever the upstream points.
//! `reqwest` follows up to 10 redirects by default.
//!
//! [`client_builder`] returns a builder with `redirect(Policy::none())` already set. The
//! workspace `clippy.toml` disallows every other way to make a client (`Client::new`,
//! `Client::builder`, `Client::default`, `ClientBuilder::new`, `reqwest::get`, the blocking
//! constructors) and `ClientBuilder::redirect` itself, so a caller can't build a client that
//! follows redirects or turn following back on. Only this function is allowed to call them.

/// A `reqwest` client builder that never follows a redirect: a 3xx comes back to the caller as
/// the response. Set timeouts, headers and the like on it, then `build()`.
#[allow(clippy::disallowed_methods)]
pub fn client_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder().redirect(reqwest::redirect::Policy::none())
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
            let http = super::client_builder().build().unwrap();
            let resp = http.get(&url).send().await.unwrap();
            assert_eq!(resp.status().as_str(), &status[..3], "{status}");
            assert_eq!(
                resp.url().path(),
                "/start",
                "{status}: the client followed the redirect"
            );
            assert_eq!(
                hits.load(std::sync::atomic::Ordering::SeqCst),
                1,
                "{status}: one request only"
            );
        }
    }
}
