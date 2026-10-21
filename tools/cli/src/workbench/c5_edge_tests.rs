//! C5 edge tests beyond the RED set: the metrics token through the CLI's directory entry point,
//! and the keys a re-mint must leave alone.

use super::service_auth::{METRICS_TOKEN_FILE, mint_metrics_token, mint_metrics_token_in, write_storage_auth};

fn keys_with_pair() -> tempfile::TempDir {
    let t = tempfile::tempdir().unwrap();
    crate::neon::compute_jwt::ensure(&t.path().join("neon")).unwrap();
    t
}

// `chelabase neon mint-metrics-token` finds the directory through CHELABASE_NEON_DIR, so the
// directory entry point must write the same file the keys-dir one does.
#[test]
fn the_neon_dir_entry_point_writes_the_same_file() {
    let keys = keys_with_pair();
    let neon = keys.path().join("neon");
    let a = mint_metrics_token_in(&neon, 1_900_000_000).unwrap();
    let b = mint_metrics_token(keys.path(), 1_900_000_000).unwrap();
    assert_eq!(a, b);
    assert_eq!(a, neon.join("storage-tokens").join(METRICS_TOKEN_FILE));
}

// A re-mint touches the metrics token only; the deterministic service tokens and the key pair stay.
#[test]
fn a_re_mint_leaves_the_other_tokens_and_the_key_pair_alone() {
    let keys = keys_with_pair();
    let neon = keys.path().join("neon");
    write_storage_auth(keys.path()).unwrap();
    let read = |p: std::path::PathBuf| std::fs::read(p).unwrap();
    let names = ["pageserver-api.jwt", "safekeeper-data.jwt", "generations-api.jwt", "storcon-admin.jwt"];
    let before: Vec<_> = names.iter().map(|n| read(neon.join("storage-tokens").join(n))).collect();
    let pem = read(neon.join("compute-jwt.pem"));
    mint_metrics_token(keys.path(), 1_900_000_000).unwrap();
    let after: Vec<_> = names.iter().map(|n| read(neon.join("storage-tokens").join(n))).collect();
    assert_eq!(before, after);
    assert_eq!(pem, read(neon.join("compute-jwt.pem")));
}

// A whitespace-only token file is not empty: it is kept (the operator wrote it), like any other
// non-empty file; only a zero-length file is minted again.
#[test]
fn a_zero_length_file_is_the_only_one_reminted() {
    let keys = keys_with_pair();
    write_storage_auth(keys.path()).unwrap();
    let path = keys.path().join("neon/storage-tokens").join(METRICS_TOKEN_FILE);
    std::fs::write(&path, "\n").unwrap();
    write_storage_auth(keys.path()).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), b"\n");
}
