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
