//! The example overlay as Compose renders it: `example-seed` mounts the setup code and nothing
//! else from the secrets volume, and gets no Zitadel credential.

use serde_json::Value;
use std::path::Path;
use std::process::Command;

#[test]
fn example_seed_mounts_exactly_one_secret() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../infra/compose");
    let rendered = Command::new("docker")
        .args([
            "compose",
            "--project-name",
            "example-render",
            "--env-file",
            "/dev/null",
        ])
        .args([
            "-f",
            "docker-compose.yml",
            "-f",
            "docker-compose.example.yml",
        ])
        .args(["config", "--format", "json", "example-seed"])
        .current_dir(&dir)
        .output();
    let out = match rendered {
        Ok(out) => out,
        // CI has Docker, so there a missing CLI is a failure, not a silent pass.
        Err(e) if std::env::var_os("CI").is_some() => panic!("no docker CLI on CI: {e}"),
        Err(e) => {
            eprintln!(
                "SKIPPED example_seed_mounts_exactly_one_secret: no docker CLI ({e}); \
                 the example-seed mount check did not run"
            );
            return;
        }
    };
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let config: Value = serde_json::from_slice(&out.stdout).unwrap();
    let seed = &config["services"]["example-seed"];
    let volumes = seed["volumes"].as_array().expect("volumes");
    assert_eq!(volumes.len(), 1, "{volumes:#?}");
    let v = &volumes[0];
    assert_eq!(v["source"], "secrets");
    assert_eq!(v["volume"]["subpath"], "setup/setup.code");
    assert_eq!(v["read_only"], true);
    assert_eq!(
        seed["environment"]["CHELABASE_SETUP_CODE_FILE"],
        v["target"]
    );
    let text = seed.to_string();
    for forbidden in ["zitadel-pats", ".pat", "PAT_FILE", "docker.sock"] {
        assert!(
            !text.contains(forbidden),
            "example-seed mentions {forbidden}: {text}"
        );
    }
    assert!(seed["ports"].is_null(), "example-seed publishes nothing");
}
