use cheladb_auth::internal::{
    generate_keypair_pem, AuthMethod, Identity, InternalSigner, InternalVerifier,
    INTERNAL_ISSUER, INTERNAL_TOKEN_TTL_SECS,
};
use cheladb_auth::AuthError;
use std::time::{SystemTime, UNIX_EPOCH};

fn keypair() -> (InternalSigner, InternalVerifier) {
    let (private_pem, public_pem) = generate_keypair_pem();
    (
        InternalSigner::from_pem(private_pem.as_bytes()).unwrap(),
        InternalVerifier::from_pem(public_pem.as_bytes()).unwrap(),
    )
}

fn identity() -> Identity {
    Identity {
        sub: "user-1".into(),
        org_id: "org-1".into(),
        auth: AuthMethod::PlatformJwt,
        request_id: "rid-1".into(),
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

#[test]
fn signed_token_verifies_and_carries_identity() {
    let (signer, verifier) = keypair();
    let token = signer.sign("orchestrator", &identity()).unwrap();

    let claims = verifier.verify(&token, "orchestrator").unwrap();
    assert_eq!(claims.iss, INTERNAL_ISSUER);
    assert_eq!(claims.aud, "orchestrator");
    assert_eq!(claims.sub, "user-1");
    assert_eq!(claims.org_id, "org-1");
    assert_eq!(claims.auth, AuthMethod::PlatformJwt);
    assert_eq!(claims.rid, "rid-1");
    assert_eq!(claims.exp - claims.iat, INTERNAL_TOKEN_TTL_SECS);
}

#[test]
fn token_for_another_service_is_rejected() {
    let (signer, verifier) = keypair();
    let token = signer.sign("some-other-service", &identity()).unwrap();
    let err = verifier.verify(&token, "orchestrator").unwrap_err();
    assert!(matches!(err, AuthError::InvalidToken(_)), "{err:?}");
}

#[test]
fn expired_token_is_rejected() {
    let (signer, verifier) = keypair();
    let token = signer
        .sign_at("orchestrator", &identity(), now() - 3600)
        .unwrap();
    let err = verifier.verify(&token, "orchestrator").unwrap_err();
    assert!(matches!(err, AuthError::InvalidToken(_)), "{err:?}");
}

#[test]
fn token_from_another_key_is_rejected() {
    let (signer, _) = keypair();
    let (_, other_verifier) = keypair();
    let token = signer.sign("orchestrator", &identity()).unwrap();
    let err = other_verifier.verify(&token, "orchestrator").unwrap_err();
    assert!(matches!(err, AuthError::InvalidToken(_)), "{err:?}");
}

#[test]
fn tampered_token_is_rejected() {
    let (signer, verifier) = keypair();
    let token = signer.sign("orchestrator", &identity()).unwrap();
    let mut parts: Vec<String> = token.split('.').map(str::to_string).collect();
    // Flip the last payload character to a different base64url character.
    let last = parts[1].pop().unwrap();
    parts[1].push(if last == 'A' { 'B' } else { 'A' });
    let tampered = parts.join(".");
    let err = verifier.verify(&tampered, "orchestrator").unwrap_err();
    assert!(matches!(err, AuthError::InvalidToken(_)), "{err:?}");
}

#[test]
fn empty_token_is_unauthorized() {
    let (_, verifier) = keypair();
    let err = verifier.verify("  ", "orchestrator").unwrap_err();
    assert!(matches!(err, AuthError::Unauthorized), "{err:?}");
}

#[test]
fn invalid_pem_is_a_config_error() {
    assert!(matches!(
        InternalSigner::from_pem(b"not a pem"),
        Err(AuthError::Config(_))
    ));
    assert!(matches!(
        InternalVerifier::from_pem(b"not a pem"),
        Err(AuthError::Config(_))
    ));
}

#[test]
fn missing_key_file_is_a_config_error() {
    let err = InternalSigner::from_pem_file(std::path::Path::new("/nonexistent/key.pem"));
    assert!(matches!(err, Err(AuthError::Config(_))));
}

#[test]
fn token_with_another_issuer_is_rejected() {
    let (private_pem, public_pem) = generate_keypair_pem();
    let verifier = InternalVerifier::from_pem(public_pem.as_bytes()).unwrap();
    let key = jsonwebtoken::EncodingKey::from_ed_pem(private_pem.as_bytes()).unwrap();
    let claims = serde_json::json!({
        "iss": "not-the-gateway",
        "aud": "orchestrator",
        "iat": now(),
        "exp": now() + 60,
        "sub": "user-1",
        "org_id": "org-1",
        "auth": "platform_jwt",
        "rid": "rid-1",
    });
    let token = jsonwebtoken::encode(
        &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::EdDSA),
        &claims,
        &key,
    )
    .unwrap();
    let err = verifier.verify(&token, "orchestrator").unwrap_err();
    assert!(matches!(err, AuthError::InvalidToken(_)), "{err:?}");
}
