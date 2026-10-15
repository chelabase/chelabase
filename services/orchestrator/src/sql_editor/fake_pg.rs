//! Real `tokio_postgres::Error`s for the connect-failure classification tests (T10 I1/I2,
//! U9): each [`Case`] runs a scripted server on an in-memory stream (or a closed port, or the
//! timeout constructor) and returns what `connect` fails with. [`TABLE`] says what each one
//! must classify as, for a compute connection (`RunTls::Compute`) and a plain one.
//!
//! The invariant (fork round 3, final fix round): a compute connection failure is
//! `compute_tls_failed` iff the TLS layer or the post-password certificate check refused it;
//! a wrong password or a stale role is never `compute_tls_failed`, and a TLS failure never
//! triggers a reconcile.

use std::fmt;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::SystemTime;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use chelabase_neon::compute_tls::{ClientTls, ComputeCa, ORCHESTRATOR_CN, write_client_pair};
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, DuplexStream, ReadBuf};
use tokio_postgres::config::{ChannelBinding as ChannelBindingMode, SslMode};
use tokio_postgres::tls::{ChannelBinding, TlsConnect, TlsStream};
use tokio_postgres::{Config, NoTls};

use super::run::RunTls;

const PASSWORD: &str = "pencil";
const SALT: &[u8] = b"salt-for-tests";
const ITERATIONS: u32 = 4096;
const SSL_REQUEST_CODE: i32 = 80_877_103;
const SCRAM_PLUS: &str = "SCRAM-SHA-256-PLUS";
/// What the TLS stand-in reports as the server certificate's hash.
const END_POINT: &[u8] = b"server-cert-hash";

/// One way a connection to a compute fails.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Case {
    /// Nothing listens (`Kind::Connect`).
    Refused,
    /// The connect timeout (`Kind::Timeout`).
    Timeout,
    /// The server answers the SSLRequest with `N` (`Kind::Tls`, no I/O error under it).
    TlsRefusedByServer,
    /// The handshake fails on the server's certificate (`Kind::Tls` over an I/O error
    /// holding a `rustls::Error`).
    HandshakeUnknownIssuer,
    /// The same, the `rustls::Error` two levels under the I/O error.
    HandshakeNested,
    /// The connection is reset during the handshake (`Kind::Tls` over a plain I/O error).
    HandshakeReset,
    /// TLS 1.3: the server refuses our client certificate after the handshake, so its
    /// `unknown_ca` alert arrives on the first read (`Kind::Io` holding a `rustls::Error`).
    AlertAfterHandshake,
    /// A plain reset on the first read after the handshake (`Kind::Io`).
    ResetAfterHandshake,
    /// The server closes during the startup (`Kind::Closed`).
    Closed,
    /// A message out of order (`Kind::UnexpectedMessage`).
    UnexpectedMessage,
    /// An error response without its code (`Kind::Parse`).
    Malformed,
    /// SCRAM asked for but no password configured (`Kind::Config`).
    NoPassword,
    /// The server authenticates without channel binding while it's required
    /// (`Kind::Authentication`).
    ChannelBindingRefused,
    /// The server's SCRAM signature doesn't verify (`Kind::Authentication`).
    BadServerSignature,
    /// 28P01 instead of the SCRAM final message: the password was wrong (`Kind::Db`).
    WrongPassword,
    /// 28000 from `pg_hba.conf` before authentication (`Kind::Db`).
    HbaRejects,
    /// U9: 28P01 after a SCRAM final message that verified, as Postgres reports a
    /// `clientcert=verify-full` CN mismatch (`Kind::Db`).
    VerifiedThen28P01,
    /// 28000 after authentication: the role may not log in (`Kind::Db`).
    VerifiedThenNotPermitted,
    /// 42501 after authentication: no CONNECT on the database (`Kind::Db`).
    VerifiedThenNoConnect,
    /// 53300 after authentication (`Kind::Db`).
    VerifiedThenTooMany,
    /// 3D000 after authentication (`Kind::Db`).
    VerifiedThenNoDatabase,
}

/// What `connect_error` must make of a case.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Want {
    Tls,
    Unreachable,
    Rejected(&'static str),
}

/// Every case: what it classifies as for a compute connection, then for a plain one.
pub(crate) const TABLE: &[(Case, Want, Want)] = {
    use Case::*;
    use Want::{Rejected, Tls, Unreachable};
    &[
        (Refused, Unreachable, Unreachable),
        (Timeout, Unreachable, Unreachable),
        (TlsRefusedByServer, Tls, Unreachable),
        (HandshakeUnknownIssuer, Tls, Unreachable),
        (HandshakeNested, Tls, Unreachable),
        (HandshakeReset, Unreachable, Unreachable),
        (AlertAfterHandshake, Tls, Unreachable),
        (ResetAfterHandshake, Unreachable, Unreachable),
        (Closed, Unreachable, Unreachable),
        (UnexpectedMessage, Unreachable, Unreachable),
        (Malformed, Unreachable, Unreachable),
        (NoPassword, Unreachable, Unreachable),
        (ChannelBindingRefused, Tls, Unreachable),
        (BadServerSignature, Unreachable, Unreachable),
        (WrongPassword, Rejected("28P01"), Rejected("28P01")),
        (HbaRejects, Rejected("28000"), Rejected("28000")),
        (VerifiedThen28P01, Tls, Rejected("28P01")),
        (
            VerifiedThenNotPermitted,
            Rejected("28000"),
            Rejected("28000"),
        ),
        (VerifiedThenNoConnect, Rejected("42501"), Rejected("42501")),
        (VerifiedThenTooMany, Rejected("53300"), Rejected("53300")),
        (VerifiedThenNoDatabase, Rejected("3D000"), Rejected("3D000")),
    ]
};

/// The reason a TLS case's message must carry (it goes to the log), if it has a typed one.
pub(crate) fn tls_reason(case: Case) -> Option<&'static str> {
    match case {
        Case::HandshakeUnknownIssuer | Case::HandshakeNested => Some("UnknownIssuer"),
        Case::AlertAfterHandshake => Some("UnknownCA"),
        _ => None,
    }
}

impl Case {
    /// What connecting fails with in this case.
    pub(crate) async fn error(self) -> tokio_postgres::Error {
        use Handshake::{Fail, Ok, ReadFails};
        match self {
            Case::Refused => refused().await,
            Case::Timeout => tokio_postgres::Error::__private_api_timeout(),
            Case::TlsRefusedByServer => {
                scripted(Ok, |mut s| async move { s.ssl_request(b'N').await }).await
            }
            Case::HandshakeUnknownIssuer => scripted(Fail(unknown_issuer), handshake_only).await,
            Case::HandshakeNested => scripted(Fail(nested_unknown_issuer), handshake_only).await,
            Case::HandshakeReset => scripted(Fail(reset), handshake_only).await,
            Case::AlertAfterHandshake => scripted(ReadFails(unknown_ca_alert), handshake_only).await,
            Case::ResetAfterHandshake => scripted(ReadFails(reset), handshake_only).await,
            Case::Closed => scripted(Ok, |mut s| async move { s.startup().await }).await,
            Case::UnexpectedMessage => {
                scripted(Ok, |mut s| async move {
                    s.startup().await;
                    s.send(b'Z', b"I").await;
                    s.drain().await;
                })
                .await
            }
            Case::Malformed => {
                scripted(Ok, |mut s| async move {
                    s.startup().await;
                    s.send(b'E', b"Mno code\0\0").await;
                    s.drain().await;
                })
                .await
            }
            Case::NoPassword => {
                let mut config = Config::new();
                config
                    .user("cb_m_ab")
                    .dbname("app")
                    .ssl_mode(SslMode::Require)
                    .channel_binding(ChannelBindingMode::Require);
                scripted_with(config, Ok, |mut s| async move {
                    s.startup().await;
                    s.auth(10, b"SCRAM-SHA-256-PLUS\0SCRAM-SHA-256\0\0").await;
                    s.drain().await;
                })
                .await
            }
            Case::ChannelBindingRefused => {
                scripted(Ok, |mut s| async move {
                    s.startup().await;
                    s.auth(0, b"").await;
                    s.drain().await;
                })
                .await
            }
            Case::BadServerSignature => {
                scripted(Ok, |mut s| async move {
                    s.startup().await;
                    s.scram_until_final().await;
                    s.scram_final(&[0; 32]).await;
                    s.drain().await;
                })
                .await
            }
            Case::WrongPassword => {
                scripted(Ok, |mut s| async move {
                    s.startup().await;
                    s.scram_until_final().await;
                    s.error("28P01", "password authentication failed for user \"cb_m_ab\"")
                        .await;
                })
                .await
            }
            Case::HbaRejects => {
                scripted(Ok, |mut s| async move {
                    s.startup().await;
                    s.error(
                        "28000",
                        "pg_hba.conf rejects connection for host \"10.213.4.12\", user \"cb_m_ab\", database \"app\", SSL encryption",
                    )
                    .await;
                })
                .await
            }
            Case::VerifiedThen28P01 => {
                scripted(Ok, |mut s| async move {
                    s.startup().await;
                    s.verified_scram().await;
                    s.error("28P01", "password authentication failed for user \"cb_m_ab\"")
                        .await;
                })
                .await
            }
            Case::VerifiedThenNotPermitted => {
                after_auth("28000", "role \"cb_m_ab\" is not permitted to log in").await
            }
            Case::VerifiedThenNoConnect => {
                after_auth("42501", "permission denied for database \"app\"").await
            }
            Case::VerifiedThenTooMany => {
                after_auth("53300", "sorry, too many clients already").await
            }
            Case::VerifiedThenNoDatabase => {
                after_auth("3D000", "database \"app\" does not exist").await
            }
        }
    }
}

/// `RunTls::Compute` with a freshly issued client pair (the classification reads only which
/// variant it is; the scripted connections use their own TLS stand-in).
pub(crate) fn compute_tls() -> RunTls {
    let now = SystemTime::now();
    let ca = ComputeCa::generate(now).unwrap();
    let pair = ca.issue_client(ORCHESTRATOR_CN, now).unwrap();
    let dir = tempfile::tempdir().unwrap();
    write_client_pair(dir.path(), &pair, ca.cert_pem(), now).unwrap();
    RunTls::Compute {
        server_name: chelabase_neon::compute_tls::server_name("ep-test"),
        client: Arc::new(ClientTls::load(dir.path()).unwrap()),
    }
}

// ---- Error causes -----------------------------------------------------------------------

fn unknown_issuer() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        rustls::Error::InvalidCertificate(rustls::CertificateError::UnknownIssuer),
    )
}

/// A wrapper between the I/O error and rustls' (a connector that adds its own context).
#[derive(Debug)]
struct SessionFailed(rustls::Error);

impl fmt::Display for SessionFailed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the TLS session failed")
    }
}

impl std::error::Error for SessionFailed {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.0)
    }
}

fn nested_unknown_issuer() -> io::Error {
    io::Error::other(SessionFailed(rustls::Error::InvalidCertificate(
        rustls::CertificateError::UnknownIssuer,
    )))
}

fn unknown_ca_alert() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        rustls::Error::AlertReceived(rustls::AlertDescription::UnknownCA),
    )
}

fn reset() -> io::Error {
    io::Error::from(io::ErrorKind::ConnectionReset)
}

// ---- Running a script -------------------------------------------------------------------

/// How a compute connection is configured: TLS and channel binding required.
fn compute_config() -> Config {
    let mut config = Config::new();
    config
        .user("cb_m_ab")
        .password(PASSWORD)
        .dbname("app")
        .ssl_mode(SslMode::Require)
        .channel_binding(ChannelBindingMode::Require);
    config
}

async fn scripted<F, Fut>(handshake: Handshake, script: F) -> tokio_postgres::Error
where
    F: FnOnce(Server) -> Fut,
    Fut: Future<Output = ()> + Send + 'static,
{
    scripted_with(compute_config(), handshake, script).await
}

async fn scripted_with<F, Fut>(
    config: Config,
    handshake: Handshake,
    script: F,
) -> tokio_postgres::Error
where
    F: FnOnce(Server) -> Fut,
    Fut: Future<Output = ()> + Send + 'static,
{
    let (client_end, server_end) = tokio::io::duplex(64 * 1024);
    let server = tokio::spawn(script(Server { s: server_end }));
    let result = config
        .connect_raw(client_end, FakeTls(handshake))
        .await
        .map(|_| ());
    if let Err(panic) = server.await {
        panic!("the scripted server failed ({panic:?}); the client got {result:?}");
    }
    result.expect_err("the scripted connection should fail")
}

/// The SSLRequest answered, then whatever the client sends until it goes.
async fn handshake_only(mut s: Server) {
    s.ssl_request(b'S').await;
    s.drain().await;
}

/// SCRAM-SHA-256-PLUS that verifies, AuthenticationOk, then `code`.
async fn after_auth(code: &'static str, message: &'static str) -> tokio_postgres::Error {
    scripted(Handshake::Ok, move |mut s| async move {
        s.startup().await;
        s.verified_scram().await;
        s.auth(0, b"").await;
        s.error(code, message).await;
    })
    .await
}

async fn refused() -> tokio_postgres::Error {
    let port = {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        listener.local_addr().unwrap().port()
    };
    let mut config = Config::new();
    config.host("127.0.0.1").port(port).user("u").dbname("app");
    config.connect(NoTls).await.map(|_| ()).unwrap_err()
}

// ---- The scripted server ----------------------------------------------------------------

pub(crate) struct Server {
    s: DuplexStream,
}

impl Server {
    async fn ssl_request(&mut self, answer: u8) {
        let len = self.s.read_i32().await.unwrap();
        let code = self.s.read_i32().await.unwrap();
        assert_eq!((len, code), (8, SSL_REQUEST_CODE), "an SSLRequest first");
        self.s.write_all(&[answer]).await.unwrap();
    }

    /// The SSLRequest (answered `S`), then the startup message.
    async fn startup(&mut self) {
        self.ssl_request(b'S').await;
        let len = self.s.read_i32().await.unwrap();
        let mut body = vec![0; usize::try_from(len).unwrap() - 4];
        self.s.read_exact(&mut body).await.unwrap();
    }

    async fn read_msg(&mut self) -> (u8, Vec<u8>) {
        let tag = self.s.read_u8().await.unwrap();
        let len = self.s.read_i32().await.unwrap();
        let mut body = vec![0; usize::try_from(len).unwrap() - 4];
        self.s.read_exact(&mut body).await.unwrap();
        (tag, body)
    }

    async fn send(&mut self, tag: u8, body: &[u8]) {
        let mut out = vec![tag];
        out.extend_from_slice(&i32::try_from(body.len() + 4).unwrap().to_be_bytes());
        out.extend_from_slice(body);
        self.s.write_all(&out).await.unwrap();
    }

    async fn auth(&mut self, code: i32, data: &[u8]) {
        let mut body = code.to_be_bytes().to_vec();
        body.extend_from_slice(data);
        self.send(b'R', &body).await;
    }

    async fn error(&mut self, code: &str, message: &str) {
        let mut body = Vec::new();
        for (field, value) in [
            (b'S', "FATAL"),
            (b'V', "FATAL"),
            (b'C', code),
            (b'M', message),
        ] {
            body.push(field);
            body.extend_from_slice(value.as_bytes());
            body.push(0);
        }
        body.push(0);
        self.send(b'E', &body).await;
    }

    /// SCRAM-SHA-256-PLUS up to the server's final message; returns the server signature a
    /// real server would send.
    async fn scram_until_final(&mut self) -> [u8; 32] {
        let mut offer = SCRAM_PLUS.as_bytes().to_vec();
        offer.extend_from_slice(b"\0\0");
        self.auth(10, &offer).await;

        let (tag, body) = self.read_msg().await;
        assert_eq!(tag, b'p', "SASLInitialResponse");
        let nul = body.iter().position(|b| *b == 0).unwrap();
        assert_eq!(&body[..nul], SCRAM_PLUS.as_bytes());
        let client_first = String::from_utf8(body[nul + 5..].to_vec()).unwrap();
        let bare = &client_first[client_first.find(",,").unwrap() + 2..];
        let client_nonce = bare.split_once(",r=").unwrap().1;
        let server_first = format!(
            "r={client_nonce}server-nonce,s={},i={ITERATIONS}",
            STANDARD.encode(SALT)
        );
        self.auth(11, server_first.as_bytes()).await;

        let (tag, body) = self.read_msg().await;
        assert_eq!(tag, b'p', "SASLResponse");
        let client_final = String::from_utf8(body).unwrap();
        let without_proof = &client_final[..client_final.find(",p=").unwrap()];
        let auth_message = format!("{bare},{server_first},{without_proof}");
        let salted = hi(PASSWORD.as_bytes(), SALT, ITERATIONS);
        let server_key = hmac(&salted, b"Server Key");
        hmac(&server_key, auth_message.as_bytes())
    }

    async fn scram_final(&mut self, signature: &[u8; 32]) {
        let body = format!("v={}", STANDARD.encode(signature));
        self.auth(12, body.as_bytes()).await;
    }

    async fn verified_scram(&mut self) {
        let signature = self.scram_until_final().await;
        self.scram_final(&signature).await;
    }

    /// Holds the stream open until the client goes.
    async fn drain(&mut self) {
        let mut buf = [0; 1024];
        while matches!(self.s.read(&mut buf).await, Ok(n) if n > 0) {}
    }
}

fn hmac(key: &[u8], data: &[u8]) -> [u8; 32] {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).unwrap();
    mac.update(data);
    mac.finalize().into_bytes().into()
}

fn hi(password: &[u8], salt: &[u8], iterations: u32) -> [u8; 32] {
    let mut first = salt.to_vec();
    first.extend_from_slice(&[0, 0, 0, 1]);
    let mut prev = hmac(password, &first);
    let mut out = prev;
    for _ in 1..iterations {
        prev = hmac(password, &prev);
        for (o, p) in out.iter_mut().zip(prev) {
            *o ^= p;
        }
    }
    out
}

// ---- A TLS stand-in ---------------------------------------------------------------------

#[derive(Clone, Copy)]
enum Handshake {
    /// A pass-through session with channel-binding data.
    Ok,
    /// The handshake fails with this error.
    Fail(fn() -> io::Error),
    /// The handshake succeeds; the first read fails with this error.
    ReadFails(fn() -> io::Error),
}

struct FakeTls(Handshake);

struct FakeTlsStream {
    inner: DuplexStream,
    read_error: Option<io::Error>,
}

impl TlsConnect<DuplexStream> for FakeTls {
    type Stream = FakeTlsStream;
    type Error = io::Error;
    type Future = Pin<Box<dyn Future<Output = io::Result<FakeTlsStream>> + Send>>;

    fn connect(self, inner: DuplexStream) -> Self::Future {
        let handshake = self.0;
        Box::pin(async move {
            match handshake {
                Handshake::Fail(fail) => Err(fail()),
                Handshake::Ok => Ok(FakeTlsStream {
                    inner,
                    read_error: None,
                }),
                Handshake::ReadFails(fail) => Ok(FakeTlsStream {
                    inner,
                    read_error: Some(fail()),
                }),
            }
        })
    }
}

impl AsyncRead for FakeTlsStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if let Some(err) = self.read_error.take() {
            return Poll::Ready(Err(err));
        }
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for FakeTlsStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

impl TlsStream for FakeTlsStream {
    fn channel_binding(&self) -> ChannelBinding {
        ChannelBinding::tls_server_end_point(END_POINT.to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The fixtures themselves: each case fails with the error kind it names, and the cases
    /// cover every kind `connect` can return (`Encode`, `ConfigParse` and the row and
    /// parameter kinds never come from connecting).
    #[tokio::test]
    async fn every_case_fails_with_the_kind_it_names() {
        use Case::*;
        let mut kinds = std::collections::BTreeSet::new();
        for (case, _, _) in TABLE {
            let kind = match case {
                Refused => "Connect",
                Timeout => "Timeout",
                TlsRefusedByServer | HandshakeUnknownIssuer | HandshakeNested | HandshakeReset => {
                    "Tls"
                }
                AlertAfterHandshake | ResetAfterHandshake => "Io",
                Closed => "Closed",
                UnexpectedMessage => "UnexpectedMessage",
                Malformed => "Parse",
                NoPassword => "Config",
                ChannelBindingRefused | BadServerSignature => "Authentication",
                WrongPassword
                | HbaRejects
                | VerifiedThen28P01
                | VerifiedThenNotPermitted
                | VerifiedThenNoConnect
                | VerifiedThenTooMany
                | VerifiedThenNoDatabase => "Db",
            };
            let err = case.error().await;
            let debug = format!("{err:?}");
            assert!(
                debug.starts_with(&format!("Error {{ kind: {kind},")),
                "{case:?}: {debug}"
            );
            kinds.insert(kind);
        }
        let all: std::collections::BTreeSet<_> = [
            "Connect",
            "Timeout",
            "Tls",
            "Io",
            "Closed",
            "UnexpectedMessage",
            "Parse",
            "Config",
            "Authentication",
            "Db",
        ]
        .into_iter()
        .collect();
        assert_eq!(kinds, all);
        let cases: std::collections::BTreeSet<_> =
            TABLE.iter().map(|(c, _, _)| format!("{c:?}")).collect();
        assert_eq!(cases.len(), TABLE.len(), "each case once");
    }
}
