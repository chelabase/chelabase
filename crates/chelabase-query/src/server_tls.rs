//! TLS to a compute, verified against the platform CA (U15: every connection Chelabase opens to
//! a compute uses TLS verified against it). The auth service's app-users sync and rest's
//! `authenticator` pools dial the compute's default-network IP; its server certificate carries
//! one DNS name, `<endpoint_id>.compute.chelabase.internal`, so a connection is
//! `host = <name>` (the name verified), `hostaddr = <ip>` (the address dialled),
//! `sslmode = require`. No client certificate, and no path back to plaintext.

use std::net::IpAddr;
use std::path::Path;
use std::sync::Arc;

use rustls::pki_types::CertificateDer;
use rustls::pki_types::pem::PemObject;
use tokio_postgres::config::SslMode;
use tokio_postgres_rustls_improved::MakeRustlsConnect;

/// Why a [`ServerTls`] couldn't be made or applied.
#[derive(Debug, thiserror::Error)]
pub enum ServerTlsError {
    #[error("reading the compute CA {path}: {source}")]
    Read {
        path: String,
        source: std::io::Error,
    },
    #[error("the compute CA {0}")]
    Ca(String),
    #[error("the compute's address {0:?} isn't an IP address")]
    NotAnIp(String),
    #[error("no certificate name to verify the compute's TLS against")]
    NoName,
}

/// The platform CA as a rustls client config (no client certificate).
#[derive(Clone)]
pub struct ServerTls {
    config: Arc<rustls::ClientConfig>,
}

impl std::fmt::Debug for ServerTls {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ServerTls")
    }
}

impl ServerTls {
    /// From the CA certificate(s) in `pem`; at least one certificate is required.
    pub fn from_ca_pem(pem: &str) -> Result<Self, ServerTlsError> {
        let mut roots = rustls::RootCertStore::empty();
        for cert in CertificateDer::pem_slice_iter(pem.as_bytes()) {
            let cert = cert.map_err(|e| ServerTlsError::Ca(format!("isn't PEM: {e}")))?;
            roots
                .add(cert)
                .map_err(|e| ServerTlsError::Ca(format!("isn't a certificate: {e}")))?;
        }
        if roots.is_empty() {
            return Err(ServerTlsError::Ca("holds no certificate".into()));
        }
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let config = rustls::ClientConfig::builder_with_provider(provider)
            // TLS 1.3 only, like the orchestrator's client: computes all speak it.
            .with_protocol_versions(&[&rustls::version::TLS13])
            .map_err(|e| ServerTlsError::Ca(format!("TLS protocol versions: {e}")))?
            .with_root_certificates(roots)
            .with_no_client_auth();
        Ok(ServerTls {
            config: Arc::new(config),
        })
    }

    /// From the CA certificate file at `path` (`ca.crt`; never the key).
    pub fn from_ca_file(path: &Path) -> Result<Self, ServerTlsError> {
        let pem = std::fs::read_to_string(path).map_err(|source| ServerTlsError::Read {
            path: path.display().to_string(),
            source,
        })?;
        Self::from_ca_pem(&pem).map_err(|e| match e {
            ServerTlsError::Ca(m) => ServerTlsError::Ca(format!("{}: {m}", path.display())),
            other => other,
        })
    }

    /// A tokio-postgres connector verifying against the CA.
    pub fn connector(&self) -> MakeRustlsConnect {
        MakeRustlsConnect::new((*self.config).clone())
    }

    /// Points `config` at `addr` (an IP, dialled) verified as `server_name`, TLS required.
    pub fn configure(
        &self,
        config: &mut tokio_postgres::Config,
        addr: &str,
        server_name: &str,
    ) -> Result<(), ServerTlsError> {
        if server_name.trim().is_empty() {
            return Err(ServerTlsError::NoName);
        }
        let ip: IpAddr = addr
            .parse()
            .map_err(|_| ServerTlsError::NotAnIp(addr.to_owned()))?;
        config
            .host(server_name)
            .hostaddr(ip)
            .ssl_mode(SslMode::Require);
        Ok(())
    }
}
