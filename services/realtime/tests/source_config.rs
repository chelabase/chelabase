//! RT1 T8: the change source's settings and wiring: `chelabase_realtime`'s password file (S1),
//! the per-compute slot cap (spec §3.2), and `app::Deps::connect` using the replication source,
//! never T7's fake (whose `visible` answers `true` for every key).

use std::collections::HashMap;
use std::path::Path;

use realtime::config::Config;

fn root() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

struct Env {
    dir: tempfile::TempDir,
    vars: HashMap<String, String>,
}

impl Env {
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
        env.set("CHELABASE_ZITADEL_AUDIENCES", "p1,c-api");
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
    fn load(&self) -> anyhow::Result<Config> {
        let vars = self.vars.clone();
        Config::from_lookup(move |k| vars.get(k).cloned())
    }
}

#[test]
fn the_replication_password_comes_from_its_file() {
    let mut env = Env::new();
    let pw = env.file("pg.password", b"rt-pg-secret\n");
    env.set("CHELABASE_REALTIME_PG_PASSWORD_FILE", &pw);
    let c = env.load().unwrap();
    assert_eq!(c.replication_password.as_deref(), Some("rt-pg-secret"));

    let mut env = Env::new();
    env.set(
        "CHELABASE_REALTIME_PG_PASSWORD_FILE",
        "/nonexistent/pg.password",
    );
    let err = env.load().err().map(|e| format!("{e:#}"));
    assert!(
        err.as_deref()
            .is_some_and(|e| e.contains("CHELABASE_REALTIME_PG_PASSWORD_FILE")),
        "{err:?}"
    );
    let mut env = Env::new();
    let empty = env.file("empty", b"\n");
    env.set("CHELABASE_REALTIME_PG_PASSWORD_FILE", &empty);
    assert!(env.load().is_err(), "an empty password file is refused");
}

#[test]
fn a_password_is_never_in_an_error() {
    let mut env = Env::new();
    let pw = env.file("pg.password", b"rt-pg-secret\n");
    env.set("CHELABASE_REALTIME_PG_PASSWORD_FILE", &pw);
    env.set("CHELABASE_REALTIME_MAX_SLOTS_PER_COMPUTE", "lots");
    let err = env.load().err().map(|e| format!("{e:#}"));
    assert!(
        err.as_deref()
            .is_some_and(|e| e.contains("CHELABASE_REALTIME_MAX_SLOTS_PER_COMPUTE")
                && !e.contains("rt-pg-secret")),
        "{err:?}"
    );
}

#[test]
fn the_slot_cap_setting() {
    let mut env = Env::new();
    env.set("CHELABASE_REALTIME_MAX_SLOTS_PER_COMPUTE", "3");
    assert_eq!(env.load().unwrap().max_slots_per_compute, 3);
    assert_eq!(
        Env::new().load().unwrap().max_slots_per_compute,
        10,
        "spec §3.2: the customer keeps at least 10 of 20"
    );
    assert_eq!(Env::new().load().unwrap().replication_password, None);
}

#[test]
fn deps_connect_uses_the_replication_source() {
    let app = std::fs::read_to_string(root().join("services/realtime/src/app.rs")).unwrap();
    let connect = app
        .split("pub async fn connect")
        .nth(1)
        .and_then(|rest| rest.split("\n    }\n").next())
        .expect("Deps::connect in app.rs");
    assert!(
        !connect.contains("FakeChangeSource"),
        "the fake answers visible = true for every key: it must not serve production"
    );
    assert!(
        connect.contains("ReplicationSource"),
        "Deps::connect builds the replication source"
    );
}
