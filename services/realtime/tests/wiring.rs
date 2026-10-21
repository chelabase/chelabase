//! RT1 T7 wiring that has no other home (pre-flight §4 "How rest is wired"): the moon project
//! and the example env. Compose, the workbench and the key mounts are checked by
//! `scripts/tests/{workbench-compose,compose-key-visibility}.sh`, `infra/neon/tests/run.sh` and
//! `tools/cli/src/workbench/tests.rs`.

use std::path::{Path, PathBuf};

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn read(rel: &str) -> String {
    std::fs::read_to_string(root().join(rel)).unwrap_or_else(|e| panic!("{rel}: {e}"))
}

#[test]
fn moon_runs_realtime_like_rest() {
    let moon = read("services/realtime/moon.yml");
    assert!(moon.contains("language: \"rust\""), "{moon}");
    for (task, cargo) in [("check", "check"), ("build", "build"), ("run", "run")] {
        assert!(moon.contains(&format!("  {task}:")), "moon task {task}");
        assert!(
            moon.contains(&format!(r#"args: ["{cargo}", "-p", "realtime"]"#)),
            "cargo {cargo} -p realtime"
        );
    }
    assert!(moon.contains("persistent: true"), "run is persistent");
}

#[test]
fn the_example_env_documents_realtimes_settings() {
    let example = read("infra/compose/.env.example");
    for line in [
        "# CHELABASE_REALTIME_ADDR=127.0.0.1:8087",
        "# CHELABASE_REALTIME_INTERNAL_ADDR=127.0.0.1:8088",
        "# CHELABASE_REALTIME_URL=http://127.0.0.1:8087",
        "# CHELABASE_VALKEY_TLS=off",
    ] {
        assert!(
            example.lines().any(|l| l.trim() == line),
            "{line} in .env.example"
        );
    }
}

#[test]
fn the_workspace_builds_realtime_with_axum_websockets() {
    let workspace = read("Cargo.toml");
    assert!(workspace.contains("\"services/realtime\""));
    let manifest = read("services/realtime/Cargo.toml");
    assert!(manifest.contains(r#"axum = { workspace = true, features = ["ws"] }"#));
}

#[test]
fn valkey_tls_is_compiled_in() {
    // S7: the redis crate's rustls TLS (aws-lc-rs through the workspace's rustls), off in the
    // stack (CHELABASE_VALKEY_TLS=off), on in platform mode.
    let workspace = read("Cargo.toml");
    let redis = workspace
        .lines()
        .find(|l| l.starts_with("redis = "))
        .expect("the redis workspace dependency");
    assert!(redis.contains("\"tokio-rustls-comp\""), "{redis}");
}
