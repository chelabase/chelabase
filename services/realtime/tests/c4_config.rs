//! Fork round 4, C4 (SP9): `CHELABASE_REALTIME_TLS_DIR` reaches `Config.realtime_tls_dir`
//! (that it is required is `config.rs::the_realtime_tls_dir_is_required`).

use std::collections::HashMap;
use std::path::Path;

use realtime::config::Config;

#[test]
fn the_realtime_tls_dir_comes_from_the_environment() {
    let dir = tempfile::tempdir().unwrap();
    let write = |name: &str, bytes: &[u8]| {
        let p = dir.path().join(name);
        std::fs::write(&p, bytes).unwrap();
        p.display().to_string()
    };
    let vars: HashMap<&str, String> = HashMap::from([
        (
            "CHELABASE_REALTIME_TICKET_KEY_FILE",
            write("ticket.key", &[9u8; 32]),
        ),
        (
            "CHELABASE_REALTIME_INTERNAL_TOKEN_FILE",
            write("internal.token", b"rt-token\n"),
        ),
        (
            "CHELABASE_AUTH_HOST",
            "auth.feat-x.chelabase.localhost:8137".into(),
        ),
        ("CHELABASE_PUBLIC_SCHEME", "http".into()),
        (
            "CHELABASE_INTERNAL_VERIFY_KEY",
            "/keys/gateway/internal-verify.pem".into(),
        ),
        ("CHELABASE_REALTIME_TLS_DIR", "/keys/realtime-tls".into()),
    ]);
    let c = Config::from_lookup(|k| vars.get(k).cloned()).unwrap();
    assert_eq!(c.realtime_tls_dir, Path::new("/keys/realtime-tls"));
}
