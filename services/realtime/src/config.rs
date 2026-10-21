//! Settings (spec §1, §8.1, S1, S7, S14, S15, S18). Environment:
//!
//! | Variable | Default |
//! |---|---|
//! | `CHELABASE_REALTIME_ADDR` | `127.0.0.1:8087` (public, behind the gateway) |
//! | `CHELABASE_REALTIME_INTERNAL_ADDR` | `127.0.0.1:8088` (`/health` only) |
//! | `CHELABASE_ORCHESTRATOR_URL` | `http://127.0.0.1:8081` (`/internal/realtime/*`) |
//! | `CHELABASE_INTERNAL_URL` | `http://127.0.0.1:8083` (`/internal/data/*`, T8) |
//! | `CHELABASE_REALTIME_INTERNAL_TOKEN` / `_FILE` | required |
//! | `CHELABASE_REALTIME_TICKET_KEY_FILE` | required, 32 raw bytes |
//! | `CHELABASE_INTERNAL_VERIFY_KEY` | the gateway's public key (identity on ticket requests) |
//! | `CHELABASE_AUTH_HOST`, `CHELABASE_ZITADEL_JWKS_URL`, `CHELABASE_ZITADEL_AUDIENCES` | the gateway's OIDC settings |
//! | `CHELABASE_NATS_*` | `chelabase_nats::NatsConfig` (user `realtime`, inbox `_INBOX_realtime`) |
//! | `CHELABASE_VALKEY_URL`, `_USER`, `_PASSWORD[_FILE]`, `CHELABASE_VALKEY_TLS` | `redis://127.0.0.1:6379`, none, none, `off` |
//! | `CHELABASE_REALTIME_INSTANCE_ID` | a random UUID per start |
//! | `CHELABASE_REALTIME_PG_PASSWORD_FILE` | `chelabase_realtime`'s password (required by `build`) |
//! | `CHELABASE_REALTIME_MAX_SLOTS_PER_COMPUTE` | 10 |
//! | `CHELABASE_COMPUTE_CA_FILE` | the platform CA certificate (required by `build`) |
//! | `CHELABASE_REALTIME_TLS_DIR` | required: the client pair `<dir>/current/{client.crt,client.key,ca.crt}` |

use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, bail};
use chelabase_auth::ZitadelAuthConfig;
use chelabase_nats::NatsConfig;

use crate::limits::SOCKET_REFRESH;
use crate::store::ValkeyConfig;
use crate::ticket::TicketKey;

const LOOPBACK: Ipv4Addr = Ipv4Addr::LOCALHOST;

/// Timings (spec §1, §2, §4, §5; R9, R15, W34). Tests shorten them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Timings {
    /// R9: the first frame of a Studio socket must be `auth` within this.
    pub auth_timeout: Duration,
    pub ping_interval: Duration,
    pub pong_timeout: Duration,
    /// Every joined channel is re-authorized this often.
    pub reauth_interval: Duration,
    /// R15: access events coalesced per member.
    pub access_coalesce: Duration,
    /// W34: project events deduplicated per project.
    pub project_dedup: Duration,
    pub presence_heartbeat: Duration,
    /// Heartbeats an instance may miss before its members are dropped.
    pub presence_missed: u32,
    /// How long a presence sync request collects answers.
    pub presence_sync_wait: Duration,
    /// How often a socket refreshes its `rt:cnt:sock:<sub>` entry (P16).
    pub socket_refresh: Duration,
    /// A frame the client's socket doesn't take within this drops the socket (a client that
    /// stopped reading).
    pub write_timeout: Duration,
}

impl Default for Timings {
    /// 5 s, 25 s, 10 s, 60 s, 1 s, 1 s, 5 s, 3, 500 ms, 30 s, 10 s.
    fn default() -> Self {
        Self {
            auth_timeout: Duration::from_secs(5),
            ping_interval: Duration::from_secs(25),
            pong_timeout: Duration::from_secs(10),
            reauth_interval: Duration::from_secs(60),
            access_coalesce: Duration::from_secs(1),
            project_dedup: Duration::from_secs(1),
            presence_heartbeat: Duration::from_secs(5),
            presence_missed: 3,
            presence_sync_wait: Duration::from_millis(500),
            socket_refresh: SOCKET_REFRESH,
            write_timeout: Duration::from_secs(10),
        }
    }
}

/// Limits (spec §8.1, R15, P16).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Limits {
    pub sockets_per_member: usize,
    pub channels_per_socket: usize,
    pub joins_per_sec: u32,
    pub tickets_per_sec: u64,
    pub presence_per_sec: u32,
    pub members_per_channel: usize,
}

impl Default for Limits {
    /// 20, 50, 5, 10, 20, 1000.
    fn default() -> Self {
        Self {
            sockets_per_member: 20,
            channels_per_socket: 50,
            joins_per_sec: 5,
            tickets_per_sec: 10,
            presence_per_sec: 20,
            members_per_channel: 1000,
        }
    }
}

#[derive(Clone)]
pub struct Config {
    pub addr: SocketAddr,
    pub internal_addr: SocketAddr,
    pub orchestrator_url: String,
    pub internal_url: String,
    pub internal_token: String,
    pub ticket_key: TicketKey,
    /// The gateway's identity verify key (read by [`crate::app::build`]).
    pub verify_key: PathBuf,
    /// The gateway's OIDC settings (`None` until [`Config::from_lookup`] reads them).
    pub oidc: Option<ZitadelAuthConfig>,
    pub nats: NatsConfig,
    pub valkey: ValkeyConfig,
    pub instance_id: String,
    pub timings: Timings,
    pub limits: Limits,
    /// `chelabase_realtime`'s password (`CHELABASE_REALTIME_PG_PASSWORD_FILE`, T8); required by
    /// [`crate::app::build`], never logged.
    pub replication_password: Option<String>,
    /// `CHELABASE_REALTIME_MAX_SLOTS_PER_COMPUTE` (default 10, T8).
    pub max_slots_per_compute: u32,
    /// The platform CA's certificate (`CHELABASE_COMPUTE_CA_FILE`, T8): the check pools'
    /// connections are TLS verified against it (U15); required by [`crate::app::Deps::connect`].
    pub compute_ca: Option<PathBuf>,
    /// `CHELABASE_REALTIME_TLS_DIR` (C4, W31): the client pair (CN `chelabase-realtime`) the
    /// replication session logs in with, and the `ca.crt` it verifies the compute against; the
    /// worker writes and renews it.
    pub realtime_tls_dir: PathBuf,
}

fn default_addr() -> SocketAddr {
    SocketAddr::from((LOOPBACK, 8087))
}

fn default_internal_addr() -> SocketAddr {
    SocketAddr::from((LOOPBACK, 8088))
}

fn new_instance_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// A secret file's text without its trailing line break; empty or unreadable is an error that
/// names the variable and the path, never the contents.
fn read_secret_text(var: &str, path: &str) -> anyhow::Result<String> {
    let text = std::fs::read_to_string(path).with_context(|| format!("{var}={path}"))?;
    let value = text.trim_end_matches(['\n', '\r']);
    if value.is_empty() {
        bail!("{var}={path} is empty");
    }
    Ok(value.to_string())
}

impl Config {
    /// The defaults of the table above with this key and token (no OIDC settings, anonymous
    /// NATS and Valkey); tests set the rest.
    pub fn with_defaults(ticket_key: TicketKey, internal_token: &str) -> Self {
        Self {
            addr: default_addr(),
            internal_addr: default_internal_addr(),
            orchestrator_url: format!("http://{LOOPBACK}:8081"),
            internal_url: format!("http://{LOOPBACK}:8083"),
            internal_token: internal_token.to_string(),
            ticket_key,
            verify_key: PathBuf::new(),
            oidc: None,
            nats: NatsConfig {
                url: chelabase_nats::DEFAULT_NATS_URL.to_string(),
                user: None,
                password: None,
                inbox_prefix: None,
            },
            valkey: ValkeyConfig {
                url: format!("redis://{LOOPBACK}:6379"),
                user: None,
                password: None,
                tls: false,
            },
            instance_id: new_instance_id(),
            timings: Timings::default(),
            limits: Limits::default(),
            replication_password: None,
            max_slots_per_compute: 10,
            compute_ca: None,
            realtime_tls_dir: PathBuf::new(),
        }
    }

    pub fn from_env() -> anyhow::Result<Self> {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    /// The table above. Errors name variables and paths, never a secret.
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> anyhow::Result<Self> {
        let get = |name: &str| {
            lookup(name)
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };
        let addr_of = |name: &str, default: SocketAddr| -> anyhow::Result<SocketAddr> {
            match get(name) {
                None => Ok(default),
                Some(v) => v
                    .parse()
                    .with_context(|| format!("{name}={v:?} is not an address (host:port)")),
            }
        };
        let addr = addr_of("CHELABASE_REALTIME_ADDR", default_addr())?;
        let internal_addr = addr_of("CHELABASE_REALTIME_INTERNAL_ADDR", default_internal_addr())?;
        let url_of = |name: &str, port: u16| {
            get(name)
                .map(|v| v.trim_end_matches('/').to_string())
                .unwrap_or_else(|| format!("http://{LOOPBACK}:{port}"))
        };
        let orchestrator_url = url_of("CHELABASE_ORCHESTRATOR_URL", 8081);
        let internal_url = url_of("CHELABASE_INTERNAL_URL", 8083);

        let internal_token = match (
            get("CHELABASE_REALTIME_INTERNAL_TOKEN"),
            get("CHELABASE_REALTIME_INTERNAL_TOKEN_FILE"),
        ) {
            (Some(_), Some(_)) => bail!(
                "set CHELABASE_REALTIME_INTERNAL_TOKEN or CHELABASE_REALTIME_INTERNAL_TOKEN_FILE, not both"
            ),
            (Some(plain), None) => plain,
            (None, Some(path)) => {
                read_secret_text("CHELABASE_REALTIME_INTERNAL_TOKEN_FILE", &path)?
            }
            (None, None) => bail!(
                "CHELABASE_REALTIME_INTERNAL_TOKEN_FILE (or CHELABASE_REALTIME_INTERNAL_TOKEN) is required"
            ),
        };

        let Some(key_path) = get("CHELABASE_REALTIME_TICKET_KEY_FILE") else {
            bail!("CHELABASE_REALTIME_TICKET_KEY_FILE is required (32 random bytes)");
        };
        let ticket_key = TicketKey::from_file(std::path::Path::new(&key_path))
            .map_err(|e| anyhow::anyhow!("CHELABASE_REALTIME_TICKET_KEY_FILE={key_path}: {e}"))?;

        let Some(verify_key) = get("CHELABASE_INTERNAL_VERIFY_KEY") else {
            bail!("CHELABASE_INTERNAL_VERIFY_KEY is required (the gateway's identity verify key)");
        };

        let oidc = ZitadelAuthConfig::from_lookup(get)
            .map_err(|e| anyhow::anyhow!("the gateway's OIDC settings: {e}"))?;

        let nats = NatsConfig::from_lookup(get).map_err(|e| anyhow::anyhow!(e))?;
        let valkey = valkey_from_lookup(&get)?;

        let instance_id = get("CHELABASE_REALTIME_INSTANCE_ID").unwrap_or_else(new_instance_id);

        let replication_password = get("CHELABASE_REALTIME_PG_PASSWORD_FILE")
            .map(|path| read_secret_text("CHELABASE_REALTIME_PG_PASSWORD_FILE", &path))
            .transpose()?;
        let max_slots_per_compute = match get("CHELABASE_REALTIME_MAX_SLOTS_PER_COMPUTE") {
            None => 10,
            Some(v) => v.parse::<u32>().with_context(|| {
                format!("CHELABASE_REALTIME_MAX_SLOTS_PER_COMPUTE={v:?} is not a whole number")
            })?,
        };

        let Some(realtime_tls_dir) = get("CHELABASE_REALTIME_TLS_DIR") else {
            bail!("CHELABASE_REALTIME_TLS_DIR is required (the realtime client certificate pair)");
        };

        Ok(Self {
            addr,
            internal_addr,
            orchestrator_url,
            internal_url,
            internal_token,
            ticket_key,
            verify_key: PathBuf::from(verify_key),
            oidc: Some(oidc),
            nats,
            valkey,
            instance_id,
            timings: Timings::default(),
            limits: Limits::default(),
            replication_password,
            max_slots_per_compute,
            compute_ca: get("CHELABASE_COMPUTE_CA_FILE").map(PathBuf::from),
            realtime_tls_dir: PathBuf::from(realtime_tls_dir),
        })
    }
}

fn valkey_from_lookup(get: &impl Fn(&str) -> Option<String>) -> anyhow::Result<ValkeyConfig> {
    let url = get("CHELABASE_VALKEY_URL").unwrap_or_else(|| format!("redis://{LOOPBACK}:6379"));
    if url.contains('@') {
        bail!(
            "CHELABASE_VALKEY_URL carries credentials; set CHELABASE_VALKEY_USER and CHELABASE_VALKEY_PASSWORD_FILE"
        );
    }
    let user = get("CHELABASE_VALKEY_USER");
    let password = match (
        get("CHELABASE_VALKEY_PASSWORD"),
        get("CHELABASE_VALKEY_PASSWORD_FILE"),
    ) {
        (Some(_), Some(_)) => {
            bail!("set CHELABASE_VALKEY_PASSWORD or CHELABASE_VALKEY_PASSWORD_FILE, not both")
        }
        (Some(plain), None) => Some(plain),
        (None, Some(path)) => Some(read_secret_text("CHELABASE_VALKEY_PASSWORD_FILE", &path)?),
        (None, None) => None,
    };
    match (&user, &password) {
        (Some(_), None) => bail!(
            "CHELABASE_VALKEY_USER is set without a password (CHELABASE_VALKEY_PASSWORD_FILE)"
        ),
        (None, Some(_)) => bail!("a Valkey password is set without CHELABASE_VALKEY_USER"),
        _ => {}
    }
    let tls = match get("CHELABASE_VALKEY_TLS").as_deref() {
        None | Some("off") => false,
        Some("on") => true,
        Some(other) => bail!("CHELABASE_VALKEY_TLS={other:?}: on or off"),
    };
    Ok(ValkeyConfig {
        url,
        user,
        password,
        tls,
    })
}
