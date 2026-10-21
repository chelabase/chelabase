//! Fork round 4, final fix round (F2): the names the coder adds.
//!
//! - `chelabase_neon::reconfigure::ERROR_BODY_LIMIT: usize = 512`: the most bytes of
//!   compute_ctl's answer an error of `push`, `status_report` or `push_storage_token` carries
//!   (cut on a character boundary, never more; `f2_edges.rs` tests the behaviour);
//! - `chelabase_neon::scram::is_verifier(&str) -> bool`: the whole
//!   `SCRAM-SHA-256$<iterations>:<salt>$<StoredKey>:<ServerKey>` structure, as Postgres'
//!   `parse_scram_secret` reads it (iterations a positive integer, salt non-empty standard
//!   base64, both keys standard base64 of exactly 32 bytes, nothing else). The worker's
//!   `create_personal_login` and its resync's recreate take only such a verifier (C3's
//!   hardening finding: the prefix alone was enough).

#[test]
fn the_error_body_limit_is_512_bytes() {
    assert_eq!(chelabase_neon::reconfigure::ERROR_BODY_LIMIT, 512);
}

// ---- SCRAM verifiers, whole -----------------------------------------------------------------

/// RFC 7677's example (user "user", password "pencil"), as Postgres stores it.
const PENCIL: &str = "SCRAM-SHA-256$4096:W22ZaJ0SNY7soEsUEjb6gQ==$WG5d8oPm3OtcPnkdi4Uo7BkeZkBFzpcXkuLmtbsT4qY=:wfPLwcE6nTWhTAmQ7tl2KeoiWGPlZqQxSrmfPwDl2dU=";

#[test]
fn a_whole_scram_verifier_is_one() {
    use chelabase_neon::scram::{is_verifier, verifier, verifier_with_salt};
    assert!(is_verifier(PENCIL));
    assert!(is_verifier(&verifier("anything")));
    assert!(is_verifier(&verifier("")));
    assert!(is_verifier(&verifier_with_salt("pw", b"s", 1)));
    assert!(is_verifier(&verifier_with_salt("pw", &[0u8; 64], 10_000)));
}

#[test]
fn anything_else_is_not_a_scram_verifier() {
    use chelabase_neon::scram::is_verifier;
    let (head, keys) = PENCIL.rsplit_once('$').unwrap();
    let (stored, server) = keys.split_once(':').unwrap();
    let key31 = base64_of(&[7u8; 31]);
    let key33 = base64_of(&[7u8; 33]);
    let bad = [
        String::new(),
        "SCRAM-SHA-256$".into(),
        // The prefix alone was enough before (C3's hardening finding).
        "SCRAM-SHA-256$4096:c2FsdA==$a:b".into(),
        "SCRAM-SHA-256$x'; DROP ROLE y; --".into(),
        "SCRAM-SHA-256$4096:W22ZaJ0SNY7soEsUEjb6gQ==".into(),
        format!("{head}$"),
        format!("{head}${stored}"),
        format!("{head}${stored}:"),
        format!("{head}$:{server}"),
        // Iterations: not a positive integer.
        PENCIL.replace("$4096:", "$0:"),
        PENCIL.replace("$4096:", "$-1:"),
        PENCIL.replace("$4096:", "$abc:"),
        PENCIL.replace("$4096:", "$:"),
        PENCIL.replace("$4096:", "$40 96:"),
        // Salt: empty or not base64.
        PENCIL.replace("W22ZaJ0SNY7soEsUEjb6gQ==", ""),
        PENCIL.replace("W22ZaJ0SNY7soEsUEjb6gQ==", "not base64!"),
        // Keys: not base64, or not 32 bytes.
        format!("{head}$!!!!:{server}"),
        format!("{head}${stored}:!!!!"),
        format!("{head}${key31}:{server}"),
        format!("{head}${stored}:{key33}"),
        format!("{head}${key31}:{key31}"),
        // Extra parts, padding or whitespace.
        format!("{PENCIL}$x"),
        format!("{PENCIL}:x"),
        format!("{PENCIL} "),
        format!("{PENCIL}\n"),
        format!(" {PENCIL}"),
        // Not SCRAM-SHA-256 at all.
        PENCIL.to_lowercase(),
        PENCIL.replace("SCRAM-SHA-256$", "SCRAM-SHA-1$"),
        "md5000102030405060708090a0b0c0d0e0f".into(),
        "plaintext".into(),
    ];
    for b in &bad {
        assert!(!is_verifier(b), "{b:?}");
    }
}

fn base64_of(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}
