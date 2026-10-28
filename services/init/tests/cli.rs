//! The `chelabase-init` binary end to end: what it prints, and that it prints no secret.

use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::process::{Command, Output};

fn run(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_chelabase-init"))
        .args(args)
        .output()
        .expect("runs")
}

/// An owners file giving every entry to the test user (a non-root writer can't chgrp to the
/// images' groups).
fn owners(dir: &Path) -> String {
    let meta = std::fs::metadata(dir).unwrap();
    let path = dir.join("owners.json");
    std::fs::write(
        &path,
        format!(
            r#"{{"all": {{"uid": {}, "gid": {}}}}}"#,
            meta.uid(),
            meta.gid()
        ),
    )
    .unwrap();
    path.display().to_string()
}

fn files(root: &Path, out: &mut Vec<Vec<u8>>) {
    for entry in std::fs::read_dir(root).unwrap() {
        let p = entry.unwrap().path();
        let meta = std::fs::symlink_metadata(&p).unwrap();
        if meta.is_dir() {
            files(&p, out);
        } else if meta.is_file() {
            out.push(std::fs::read(&p).unwrap());
        }
    }
}

#[test]
fn secrets_prints_the_setup_code_once_and_no_secret() {
    let t = tempfile::tempdir().unwrap();
    let owners = owners(t.path());
    let dir = t.path().join("secrets");
    let d = dir.to_str().unwrap();

    let first = run(&["secrets", "--dir", d, "--owners", &owners]);
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let stdout = String::from_utf8(first.stdout.clone()).unwrap();
    let code = stdout
        .strip_prefix("Setup code: ")
        .and_then(|s| s.strip_suffix('\n'))
        .unwrap_or_else(|| panic!("{stdout:?}"))
        .to_string();
    assert_eq!(code.len(), 24);

    let again = run(&["secrets", "--dir", d, "--owners", &owners]);
    assert!(again.status.success());
    assert!(again.stdout.is_empty(), "no setup code on a rerun");

    let reprint = run(&["setup-code", "--dir", d]);
    assert!(reprint.status.success());
    assert_eq!(
        String::from_utf8(reprint.stdout).unwrap(),
        format!("{code}\n")
    );
    assert!(reprint.stderr.is_empty());

    // A failing run (a loose secret) names the file, and still prints nothing secret.
    let token = dir.join("tokens/app-auth.token");
    std::fs::write(&token, "").unwrap();
    let failed = run(&["secrets", "--dir", d, "--owners", &owners]);
    assert!(!failed.status.success());
    let failed_err = String::from_utf8_lossy(&failed.stderr).into_owned();
    assert!(failed_err.contains("app-auth.token"), "{failed_err}");

    let mut printed = String::from_utf8_lossy(&first.stderr).into_owned();
    for o in [&again, &failed] {
        printed.push_str(&String::from_utf8_lossy(&o.stdout));
        printed.push_str(&String::from_utf8_lossy(&o.stderr));
    }
    assert!(
        !printed.contains(&code),
        "the code only on the creating run's stdout"
    );
    let mut contents = Vec::new();
    files(&dir, &mut contents);
    let mut checked = 0;
    for bytes in contents {
        let text = String::from_utf8_lossy(&bytes);
        for line in text.lines().map(str::trim) {
            if line.len() >= 8 && !line.starts_with("-----") && !line.starts_with('#') {
                assert!(!printed.contains(line), "printed a secret: {printed}");
                checked += 1;
            }
        }
    }
    assert!(checked > 40);
}

#[test]
fn a_missing_owners_file_or_dir_fails_cleanly() {
    let t = tempfile::tempdir().unwrap();
    let d = t.path().join("s");
    let out = run(&[
        "secrets",
        "--dir",
        d.to_str().unwrap(),
        "--owners",
        "/nonexistent.json",
    ]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("nonexistent.json"));
    let out = run(&["setup-code", "--dir", d.to_str().unwrap()]);
    assert!(!out.status.success());
    assert!(out.stdout.is_empty());
}

// m12 (#23): `--seeds` end to end, through the binary.
#[test]
fn secrets_with_seeds() {
    use std::os::unix::fs::PermissionsExt;
    let t = tempfile::tempdir().unwrap();
    let owners = owners(t.path());
    let dir = t.path().join("secrets");
    let d = dir.to_str().unwrap();
    let (metadata, nats, masterkey) = (
        "Sntl9Q-binary-metadata-0001",
        "Sntl9Q-binary-nats-runner-0001",
        "Sntl9QBinaryMasterkey00000000001",
    );
    let env = t.path().join(".env");
    let write_env = |text: &str, mode: u32| {
        std::fs::write(&env, text).unwrap();
        std::fs::set_permissions(&env, std::fs::Permissions::from_mode(mode)).unwrap();
    };
    write_env(
        &format!(
            "# compose settings\nCHELABASE_KEYS_DIR=/srv/keys\n\
             CHELABASE_SEED_METADATA_DB_PASSWORD={metadata}\n\
             CHELABASE_SEED_NATS_RUNNER_PASSWORD='{nats}'\r\n\
             CHELABASE_SEED_ZITADEL_MASTERKEY=\"{masterkey}\"\n"
        ),
        0o600,
    );
    let e = env.to_str().unwrap();
    let first = run(&["secrets", "--dir", d, "--owners", &owners, "--seeds", e]);
    let first_err = String::from_utf8_lossy(&first.stderr).into_owned();
    assert!(first.status.success(), "{first_err}");
    let read = |p: &str| std::fs::read_to_string(dir.join(p)).unwrap();
    assert_eq!(read("db/metadata.password"), metadata);
    assert_eq!(read("nats/runner.password"), nats);
    assert_eq!(read("zitadel/masterkey"), masterkey);
    assert!(first_err.contains("seeded") && first_err.contains("db/metadata.password"));
    assert!(first_err.contains("3 seeded"), "{first_err}");
    assert!(!first_err.contains("warning"), "{first_err}");
    let stdout = String::from_utf8(first.stdout.clone()).unwrap();
    assert!(stdout.starts_with("Setup code: ") && stdout.lines().count() == 1);

    // Same seeds again: nothing to do. A loose .env warns.
    write_env(&std::fs::read_to_string(&env).unwrap(), 0o644);
    let again = run(&["secrets", "--dir", d, "--owners", &owners, "--seeds", e]);
    let again_err = String::from_utf8_lossy(&again.stderr).into_owned();
    assert!(again.status.success(), "{again_err}");
    assert!(again.stdout.is_empty());
    assert!(again_err.contains("chmod 600"), "{again_err}");
    assert!(again_err.contains("0 created, 0 seeded"), "{again_err}");

    // A changed seed fails, naming the key and file.
    write_env(
        "CHELABASE_SEED_METADATA_DB_PASSWORD=Sntl9Q-binary-changed-0001\n",
        0o600,
    );
    let changed = run(&["secrets", "--dir", d, "--owners", &owners, "--seeds", e]);
    let changed_err = String::from_utf8_lossy(&changed.stderr).into_owned();
    assert!(!changed.status.success());
    assert!(
        changed_err.contains("CHELABASE_SEED_METADATA_DB_PASSWORD")
            && changed_err.contains("db/metadata.password"),
        "{changed_err}"
    );
    assert_eq!(read("db/metadata.password"), metadata);

    // An invalid seed fails before anything is written, even the root.
    write_env("CHELABASE_SEED_ZITADEL_MASTERKEY=Sntl9Q-too-short\n", 0o600);
    let fresh = t.path().join("fresh");
    let invalid = run(&[
        "secrets",
        "--dir",
        fresh.to_str().unwrap(),
        "--owners",
        &owners,
        "--seeds",
        e,
    ]);
    assert!(!invalid.status.success());
    assert!(String::from_utf8_lossy(&invalid.stderr).contains("CHELABASE_SEED_ZITADEL_MASTERKEY"));
    assert!(!fresh.exists());

    // A symlinked seeds file is refused.
    let link = t.path().join("link.env");
    std::os::unix::fs::symlink(&env, &link).unwrap();
    let linked = run(&[
        "secrets",
        "--dir",
        d,
        "--owners",
        &owners,
        "--seeds",
        link.to_str().unwrap(),
    ]);
    assert!(!linked.status.success());
    assert!(String::from_utf8_lossy(&linked.stderr).contains("symlink"));

    // --help names the flag; no run printed a seed value.
    let help = run(&["secrets", "--help"]);
    assert!(String::from_utf8_lossy(&help.stdout).contains("--seeds"));
    let mut printed = String::new();
    for o in [&first, &again, &changed, &invalid, &linked, &help] {
        printed.push_str(&String::from_utf8_lossy(&o.stderr));
        printed.push_str(&String::from_utf8_lossy(&o.stdout));
    }
    assert!(
        !printed.contains("Sntl9Q"),
        "a seed value was printed: {printed}"
    );
}
