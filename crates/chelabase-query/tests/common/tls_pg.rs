//! A fake Postgres that offers TLS with a given server certificate (or refuses it), and
//! records what a client did: whether it asked for TLS, whether the handshake finished, whether
//! a startup packet (which carries the user, and so is where a password exchange would begin)
//! ever arrived, and whether it came in plaintext. Used by the U15 tests (f5b): every
//! connection Chelabase opens to a compute uses TLS verified against the platform CA.
//!
//! Included by `#[path]` from the auth-sync tests here and from rest's.
#![allow(dead_code)]

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use chelabase_neon::compute_tls::ComputeCa;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// The client's first packet was an SSLRequest.
    SslRequest,
    /// The client's first packet was a plaintext startup message (a TLS downgrade).
    PlaintextStartup,
    /// The TLS handshake finished.
    HandshakeOk,
    /// The TLS handshake failed (the client refused the certificate, or hung up).
    HandshakeFailed,
    /// A startup message arrived inside the finished TLS session.
    TlsStartup,
    /// The connection ended.
    Done,
}

pub struct TlsPg {
    pub port: u16,
    events: Arc<Mutex<Vec<Event>>>,
}

/// A platform CA and a server certificate it issued for `endpoint_id`
/// (`<endpoint_id>.compute.chelabase.internal`, the only name the real ones carry).
pub struct Issued {
    pub ca_pem: String,
    pub cert_pem: String,
    pub key_pem: String,
}

pub fn issue(endpoint_id: &str) -> Issued {
    let now = SystemTime::now();
    let ca = ComputeCa::generate(now).unwrap();
    let server = ca.issue_server(endpoint_id, now).unwrap();
    Issued {
        ca_pem: ca.cert_pem().to_owned(),
        cert_pem: server.cert_pem,
        key_pem: server.key_pem,
    }
}

/// A server certificate for `endpoint_id` from the CA of `ca`.
pub fn issue_from(ca_pem: &str, ca_key_pem: &str, endpoint_id: &str) -> Issued {
    let ca = ComputeCa::from_pem(ca_pem, ca_key_pem).unwrap();
    let server = ca.issue_server(endpoint_id, SystemTime::now()).unwrap();
    Issued {
        ca_pem: ca_pem.to_owned(),
        cert_pem: server.cert_pem,
        key_pem: server.key_pem,
    }
}

impl TlsPg {
    /// Offers TLS with `server` (`Some((cert_pem, key_pem))`), or answers `N` to an SSLRequest.
    pub fn start(server: Option<(&str, &str)>) -> TlsPg {
        Self::start_with(server, rustls::ALL_VERSIONS)
    }

    /// Offers TLS 1.2 only (a server below the floor Chelabase's clients require).
    pub fn start_tls12_only(cert: &str, key: &str) -> TlsPg {
        static ONLY_12: [&rustls::SupportedProtocolVersion; 1] = [&rustls::version::TLS12];
        Self::start_with(Some((cert, key)), &ONLY_12)
    }

    fn start_with(
        server: Option<(&str, &str)>,
        versions: &'static [&'static rustls::SupportedProtocolVersion],
    ) -> TlsPg {
        let config = server.map(|(cert, key)| {
            let chain: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(cert.as_bytes())
                .collect::<Result<_, _>>()
                .unwrap();
            let key = PrivateKeyDer::from_pem_slice(key.as_bytes()).unwrap();
            let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
            Arc::new(
                rustls::ServerConfig::builder_with_provider(provider)
                    .with_protocol_versions(versions)
                    .unwrap()
                    .with_no_client_auth()
                    .with_single_cert(chain, key)
                    .unwrap(),
            )
        });
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let events = Arc::new(Mutex::new(Vec::new()));
        let log = events.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let (config, log) = (config.clone(), log.clone());
                std::thread::spawn(move || {
                    let _ = serve(stream, config, &log);
                    log.lock().unwrap().push(Event::Done);
                });
            }
        });
        TlsPg { port, events }
    }

    pub fn events(&self) -> Vec<Event> {
        self.events.lock().unwrap().clone()
    }

    /// Waits until `n` connections have ended; returns every event.
    pub fn after_connections(&self, n: usize) -> Vec<Event> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let events = self.events();
            let done = events.iter().filter(|e| **e == Event::Done).count();
            if done >= n || Instant::now() > deadline {
                return events;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// How many connections have been accepted and have sent their first packet.
    pub fn first_packets(&self) -> usize {
        self.events()
            .iter()
            .filter(|e| matches!(e, Event::SslRequest | Event::PlaintextStartup))
            .count()
    }
}

const AUTH_OK_READY: [u8; 15] = [b'R', 0, 0, 0, 8, 0, 0, 0, 0, b'Z', 0, 0, 0, 5, b'I'];
const SSL_REQUEST: [u8; 8] = [0, 0, 0, 8, 0x04, 0xd2, 0x16, 0x2f];

/// The startup message after the first 8 bytes (its length is in them).
fn read_startup_rest<R: Read>(r: &mut R, first: &[u8; 8]) -> std::io::Result<()> {
    let len = u32::from_be_bytes([first[0], first[1], first[2], first[3]]) as usize;
    let mut rest = vec![0u8; len.saturating_sub(8)];
    r.read_exact(&mut rest)
}

fn serve(
    mut tcp: std::net::TcpStream,
    config: Option<Arc<rustls::ServerConfig>>,
    log: &Mutex<Vec<Event>>,
) -> std::io::Result<()> {
    tcp.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut first = [0u8; 8];
    tcp.read_exact(&mut first)?;
    if first != SSL_REQUEST {
        log.lock().unwrap().push(Event::PlaintextStartup);
        let _ = read_startup_rest(&mut tcp, &first);
        let _ = tcp.write_all(&AUTH_OK_READY);
        return Ok(());
    }
    log.lock().unwrap().push(Event::SslRequest);
    let Some(config) = config else {
        tcp.write_all(b"N")?;
        // A client that falls back sends its startup message in plaintext.
        let mut next = [0u8; 8];
        if tcp.read_exact(&mut next).is_ok() {
            log.lock().unwrap().push(Event::PlaintextStartup);
        }
        return Ok(());
    };
    tcp.write_all(b"S")?;
    let mut conn = rustls::ServerConnection::new(config).map_err(std::io::Error::other)?;
    while conn.is_handshaking() {
        if let Err(e) = conn.complete_io(&mut tcp) {
            log.lock().unwrap().push(Event::HandshakeFailed);
            return Err(e);
        }
    }
    log.lock().unwrap().push(Event::HandshakeOk);
    let mut tls = rustls::StreamOwned::new(conn, tcp);
    let mut first = [0u8; 8];
    tls.read_exact(&mut first)?;
    log.lock().unwrap().push(Event::TlsStartup);
    let _ = read_startup_rest(&mut tls, &first);
    // Authenticated and idle, then gone: the client's first statement fails as "closed".
    let _ = tls.write_all(&AUTH_OK_READY);
    let _ = tls.flush();
    Ok(())
}
