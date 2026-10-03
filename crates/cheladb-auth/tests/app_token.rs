use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine;
use cheladb_auth::app_token::{
    app_audience, kid_for, AppClaims, AppTokenError, AppTokenSigner, AppTokenVerifier, APP_ISSUER,
};
use cheladb_auth::internal::generate_keypair_pem;
use sha2::{Digest, Sha256};
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()
}

fn pair() -> (AppTokenSigner, AppTokenVerifier, String) {
    let (private_pem, public_pem) = generate_keypair_pem();
    (
        AppTokenSigner::from_pem(private_pem.as_bytes(), public_pem.as_bytes()).unwrap(),
        AppTokenVerifier::from_pem(public_pem.as_bytes()).unwrap(),
        public_pem,
    )
}

fn claims(project: Uuid, exp: u64) -> AppClaims {
    AppClaims {
        iss: APP_ISSUER.into(),
        aud: app_audience(project),
        sub: Uuid::new_v4(),
        sid: Uuid::new_v4(),
        project,
        email: "a@example.com".into(),
        email_verified: true,
        role: "authenticated".into(),
        iat: now(),
        exp,
    }
}

fn raw_of(public_pem: &str) -> [u8; 32] {
    let body: String = public_pem.lines().filter(|l| !l.starts_with("-----")).collect();
    let der = STANDARD.decode(body).unwrap();
    assert_eq!(der.len(), 44);
    der[12..].try_into().unwrap()
}

#[test]
fn app_token_round_trip() {
    let (signer, verifier, _) = pair();
    let project = Uuid::new_v4();
    let c = claims(project, now() + 900);
    let token = signer.sign(&c).unwrap();
    assert_eq!(verifier.verify(&token, project).unwrap(), c);
    assert_eq!(app_audience(project), format!("cheladb-project:{project}"));
}

#[test]
fn app_token_wrong_audience_is_invalid() {
    let (signer, verifier, _) = pair();
    let token = signer.sign(&claims(Uuid::new_v4(), now() + 900)).unwrap();
    assert_eq!(verifier.verify(&token, Uuid::new_v4()), Err(AppTokenError::Invalid));
}

#[test]
fn app_token_expired_beyond_leeway_is_expired_but_within_leeway_passes() {
    let (signer, verifier, _) = pair();
    let project = Uuid::new_v4();
    let within = signer.sign(&claims(project, now() - 10)).unwrap();
    assert!(verifier.verify(&within, project).is_ok());
    let beyond = signer.sign(&claims(project, now() - 60)).unwrap();
    assert_eq!(verifier.verify(&beyond, project), Err(AppTokenError::Expired));
}

#[test]
fn app_token_kid_is_sha256_prefix_of_raw_key() {
    let (signer, _, public_pem) = pair();
    let raw = raw_of(&public_pem);
    let expected = &hex::encode(Sha256::digest(raw))[..16];
    assert_eq!(kid_for(&raw), expected);
    assert_eq!(signer.kid(), expected);
    let token = signer.sign(&claims(Uuid::new_v4(), now() + 900)).unwrap();
    let header = token.split('.').next().unwrap();
    let header: serde_json::Value =
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(header).unwrap()).unwrap();
    assert_eq!(header["kid"], expected);
}

#[test]
fn jwks_has_the_okp_key() {
    let (signer, _, public_pem) = pair();
    let raw = raw_of(&public_pem);
    assert_eq!(
        signer.jwks(),
        serde_json::json!({ "keys": [{
            "kty": "OKP", "crv": "Ed25519",
            "x": URL_SAFE_NO_PAD.encode(raw),
            "kid": signer.kid(), "alg": "EdDSA", "use": "sig",
        }]})
    );
}

#[test]
fn app_token_other_key_is_invalid() {
    let (signer, _, _) = pair();
    let (_, other, _) = pair();
    let project = Uuid::new_v4();
    let token = signer.sign(&claims(project, now() + 900)).unwrap();
    assert_eq!(other.verify(&token, project), Err(AppTokenError::Invalid));
}
