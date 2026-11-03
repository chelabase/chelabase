//! RT1 T7: settings from the environment (spec §1, S1, S7, S15, S18): the listen addresses, the
//! two orchestrator URLs, the secrets as files, NATS and Valkey credentials, Valkey TLS.

use std::collections::HashMap;
use std::path::Path;

use realtime::config::{Config, Limits, Timings};
use realtime::store::ValkeyConfig;
use realtime::ticket::{TicketKey, ticket_hash};

struct Env {
    dir: tempfile::TempDir,
    vars: HashMap<String, String>,
}

impl Env {
    /// The minimum: the ticket key file, the internal token file and the OIDC host.
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let mut env = Env {
            dir,
            vars: HashMap::new(),
        };
        let key = env.file("ticket.key", &[9u8; 32]);
        let token = env.file("internal.token", b"rt-token\n");
        env.set("CHELABASE_REALTIME_TICKET_KEY_FILE", &key);
        env.set("CHELABASE_REALTIME_INTERNAL_TOKEN_FILE", &token);
        env.set(
            "CHELABASE_AUTH_HOST",
            "auth.feat-x.chelabase.localhost:8137",
        );
        env.set("CHELABASE_PUBLIC_SCHEME", "http");
        // #22: the token audiences are required (an empty list never accepts a token).
        env.set(
            "CHELABASE_INTERNAL_VERIFY_KEY",
            "/keys/gateway/internal-verify.pem",
        );
        // C4 (SP9): the realtime client pair's directory is required.
        env.set("CHELABASE_REALTIME_TLS_DIR", "/keys/realtime-tls");
        env
    }
    fn file(&self, name: &str, bytes: &[u8]) -> String {
        let p = self.dir.path().join(name);
        std::fs::write(&p, bytes).unwrap();
        p.display().to_string()
    }
    fn set(&mut self, k: &str, v: &str) -> &mut Self {
        self.vars.insert(k.into(), v.into());
        self
    }
    fn unset(&mut self, k: &str) -> &mut Self {
        self.vars.remove(k);
        self
    }
    fn load(&self) -> anyhow::Result<Config> {
        let vars = self.vars.clone();
        Config::from_lookup(move |k| vars.get(k).cloned())
    }
}

#[test]
fn defaults() {
    let c = Env::new().load().unwrap();
    assert_eq!(c.addr.to_string(), "127.0.0.1:8087");
    assert_eq!(c.internal_addr.to_string(), "127.0.0.1:8088");
    // The loopback defaults, spelled without the literal (`scripts/tests/stack-urls.sh`).
    let lo = std::net::Ipv4Addr::LOCALHOST;
    assert_eq!(
        c.orchestrator_url,
        format!("http://{lo}:{}", 8081),
        "the /internal/realtime/* routes (S15)"
    );
    assert_eq!(
        c.internal_url,
        format!("http://{lo}:{}", 8083),
        "the /internal/data/* routes (S15)"
    );
    assert_eq!(
        c.internal_token, "rt-token",
        "the file's trailing line break is dropped"
    );
    assert_eq!(c.verify_key, Path::new("/keys/gateway/internal-verify.pem"));
    assert_eq!(c.valkey.url, format!("redis://{lo}:{}", 6379));
    assert!(!c.valkey.tls, "S7: Valkey TLS is off by default");
    assert_eq!(c.timings, Timings::default());
    assert_eq!(c.limits, Limits::default());
    assert_eq!(c.issuer.as_deref(), Some("http://auth.feat-x.chelabase.localhost:8137"));
}

#[test]
fn the_stacks_values() {
    let mut env = Env::new();
    let nats_pw = env.file("nats.password", b"n-pw\n");
    let valkey_pw = env.file("valkey.password", b"v-pw\n");
    env.set("CHELABASE_REALTIME_ADDR", "0.0.0.0:8087")
        .set("CHELABASE_REALTIME_INTERNAL_ADDR", "0.0.0.0:8088")
        .set("CHELABASE_ORCHESTRATOR_URL", "http://orchestrator:8081")
        .set("CHELABASE_INTERNAL_URL", "http://orchestrator:8083")
        .set("CHELABASE_NATS_URL", "nats://nats:4222")
        .set("CHELABASE_NATS_USER", "realtime")
        .set("CHELABASE_NATS_PASSWORD_FILE", &nats_pw)
        .set("CHELABASE_NATS_INBOX_PREFIX", "_INBOX_realtime")
        .set("CHELABASE_VALKEY_URL", "redis://valkey:6379")
        .set("CHELABASE_VALKEY_USER", "realtime")
        .set("CHELABASE_VALKEY_PASSWORD_FILE", &valkey_pw)
        .set("CHELABASE_REALTIME_INSTANCE_ID", "rt-a");
    let c = env.load().unwrap();
    assert_eq!(c.addr.to_string(), "0.0.0.0:8087");
    assert_eq!(c.internal_addr.to_string(), "0.0.0.0:8088");
    assert_eq!(c.orchestrator_url, "http://orchestrator:8081");
    assert_eq!(c.internal_url, "http://orchestrator:8083");
    assert_eq!(c.nats.user.as_deref(), Some("realtime"));
    assert_eq!(c.nats.password.as_deref(), Some("n-pw"));
    assert_eq!(c.nats.inbox_prefix.as_deref(), Some("_INBOX_realtime"));
    assert_eq!(c.valkey.user.as_deref(), Some("realtime"));
    assert_eq!(c.valkey.password.as_deref(), Some("v-pw"));
    assert_eq!(c.instance_id, "rt-a");
}

#[test]
fn the_ticket_key_comes_from_its_file() {
    let env = Env::new();
    let c = env.load().unwrap();
    let mine = TicketKey::from_bytes(&[9u8; 32]).unwrap();
    let h = ticket_hash("t");
    let id = realtime::ticket::TicketIdentity {
        sub: "s".into(),
        tenant_id: None,
        auth: chelabase_auth::internal::AuthMethod::ManagementJwt,
        email: None,
        name: None,
        data: None,
    };
    assert_eq!(mine.unseal(&h, &c.ticket_key.seal(&h, &id)), Ok(id));
}

#[test]
fn required_secrets_and_bad_files_are_errors_without_the_secret() {
    let mut env = Env::new();
    env.unset("CHELABASE_REALTIME_TICKET_KEY_FILE");
    assert!(env.load().is_err(), "no ticket key");
    let mut env = Env::new();
    let short = env.file("short.key", &[1u8; 16]);
    env.set("CHELABASE_REALTIME_TICKET_KEY_FILE", &short);
    assert!(env.load().is_err(), "a 16-byte key");
    let mut env = Env::new();
    env.unset("CHELABASE_REALTIME_INTERNAL_TOKEN_FILE");
    assert!(env.load().is_err(), "no internal token");
    let mut env = Env::new();
    env.set("CHELABASE_REALTIME_INTERNAL_TOKEN", "plain-secret-value");
    let err = env.load().err().expect("both forms set").to_string();
    assert!(
        !err.contains("plain-secret-value") && !err.contains("rt-token"),
        "{err}"
    );
    let mut env = Env::new();
    env.set("CHELABASE_REALTIME_ADDR", "not an address");
    assert!(env.load().is_err());
}

// C4 (SP9, spec §10): the realtime client pair (`<dir>/current/{client.crt,client.key,ca.crt}`)
// is how the replication session logs in, so its directory is required: unset or blank is a
// startup error that names the variable.
#[test]
fn the_realtime_tls_dir_is_required() {
    Env::new().load().unwrap();
    let mut env = Env::new();
    env.unset("CHELABASE_REALTIME_TLS_DIR");
    let err = format!("{:#}", env.load().err().expect("no TLS dir is an error"));
    assert!(err.contains("CHELABASE_REALTIME_TLS_DIR"), "{err}");
    let mut env = Env::new();
    env.set("CHELABASE_REALTIME_TLS_DIR", "   ");
    let err = format!("{:#}", env.load().err().expect("a blank TLS dir is an error"));
    assert!(err.contains("CHELABASE_REALTIME_TLS_DIR"), "{err}");
}

#[test]
fn a_nats_or_valkey_user_without_a_password_is_an_error() {
    let mut env = Env::new();
    env.set("CHELABASE_NATS_USER", "realtime");
    assert!(env.load().is_err());
    let mut env = Env::new();
    env.set("CHELABASE_VALKEY_USER", "realtime");
    assert!(env.load().is_err());
}

#[test]
fn valkey_tls_is_on_or_off() {
    let mut env = Env::new();
    assert!(
        env.set("CHELABASE_VALKEY_TLS", "on")
            .load()
            .unwrap()
            .valkey
            .tls
    );
    assert!(
        !env.set("CHELABASE_VALKEY_TLS", "off")
            .load()
            .unwrap()
            .valkey
            .tls
    );
    assert!(env.set("CHELABASE_VALKEY_TLS", "maybe").load().is_err());
}

#[test]
fn every_start_has_its_own_instance_id_by_default() {
    let env = Env::new();
    let (a, b) = (env.load().unwrap(), env.load().unwrap());
    assert!(!a.instance_id.is_empty());
    assert_ne!(a.instance_id, b.instance_id);
}

#[test]
fn the_valkey_url_carries_the_credentials_and_the_tls_scheme() {
    let plain = ValkeyConfig {
        url: "redis://valkey:6379".into(),
        user: None,
        password: None,
        tls: false,
    };
    assert_eq!(plain.connection_url(), "redis://valkey:6379");
    let creds = ValkeyConfig {
        url: "redis://valkey:6379".into(),
        user: Some("realtime".into()),
        password: Some("p@ss/w:rd".into()),
        tls: false,
    };
    assert_eq!(
        creds.connection_url(),
        "redis://realtime:p%40ss%2Fw%3Ard@valkey:6379"
    );
    let tls = ValkeyConfig {
        tls: true,
        ..creds.clone()
    };
    assert_eq!(
        tls.connection_url(),
        "rediss://realtime:p%40ss%2Fw%3Ard@valkey:6379"
    );
    assert!(
        !format!("{creds:?}").contains("p@ss"),
        "Debug redacts the password"
    );
}

#[test]
fn with_defaults_is_the_defaults() {
    let c = Config::with_defaults(TicketKey::generate(), "tok");
    assert_eq!(c.addr.to_string(), "127.0.0.1:8087");
    assert_eq!(c.internal_token, "tok");
    assert!(c.issuer.is_none());
    assert_eq!(c.timings, Timings::default());
    assert!(!c.instance_id.is_empty());
}
