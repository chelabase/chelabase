//! Member access tokens (#119): EdDSA, pinned `kid`, `iss` = the auth host, `aud` =
//! `chelabase-member`, 30 s leeway. App tokens, the gateway's internal tokens and tokens from
//! another key are refused, and the reverse.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chelabase_auth::app_token::{
    APP_ISSUER, AppClaims, AppTokenSigner, AppTokenVerifier, app_audience,
};
use chelabase_auth::internal::{
    AuthMethod, Identity, InternalSigner, InternalVerifier, generate_keypair_pem,
};
use chelabase_auth::member_token::{
    MEMBER_AUDIENCE, MEMBER_MAX_TTL_SECS, MemberClaims, MemberTokenError, MemberTokenSigner,
    MemberTokenVerifier, member_shaped,
};
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use serde_json::{Value, json};
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

const ISSUER: &str = "http://auth.chelabase.localhost:8000";

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

struct Keys {
    private: String,
    public: String,
    signer: MemberTokenSigner,
    verifier: MemberTokenVerifier,
}

fn keys() -> Keys {
    let (private, public) = generate_keypair_pem();
    let signer =
        MemberTokenSigner::from_pem(private.as_bytes(), public.as_bytes(), ISSUER).unwrap();
    let verifier = MemberTokenVerifier::from_pem(public.as_bytes(), ISSUER).unwrap();
    Keys {
        private,
        public,
        signer,
        verifier,
    }
}

fn claims() -> MemberClaims {
    let iat = now();
    MemberClaims {
        iss: ISSUER.into(),
        aud: MEMBER_AUDIENCE.into(),
        sub: "user_7Hq2".into(),
        tid: Uuid::new_v4(),
        sid: "sess_91".into(),
        email: "ana@example.com".into(),
        name: "Ana Lima".into(),
        iat,
        exp: iat + 300,
    }
}

fn as_value(c: &MemberClaims) -> Value {
    serde_json::to_value(c).unwrap()
}

/// Sign any JSON with `private_pem` under the header's `alg` = EdDSA and the given `kid`.
fn sign_raw(private_pem: &str, kid: Option<&str>, claims: &Value) -> String {
    let mut header = Header::new(Algorithm::EdDSA);
    header.kid = kid.map(str::to_string);
    encode(
        &header,
        claims,
        &EncodingKey::from_ed_pem(private_pem.as_bytes()).unwrap(),
    )
    .unwrap()
}

fn b64(v: &Value) -> String {
    URL_SAFE_NO_PAD.encode(serde_json::to_vec(v).unwrap())
}

// AC 1: a token from the signer verifies to the same claims.
#[test]
fn roundtrip() {
    let k = keys();
    let c = claims();
    let token = k.signer.sign(&c).unwrap();
    assert!(member_shaped(&token));
    assert_eq!(k.verifier.verify(&token).unwrap(), c);
    assert_eq!(k.signer.kid(), k.verifier.kid());
    let header = jsonwebtoken::decode_header(&token).unwrap();
    assert_eq!(header.alg, Algorithm::EdDSA);
    assert_eq!(header.kid.as_deref(), Some(k.verifier.kid()));
}

// AC 2: an app-user token is not a member token, whichever key signed it; and a member token
// is not an app token.
#[test]
fn app_token_refused() {
    let k = keys();
    let project = Uuid::new_v4();
    let app = AppClaims {
        iss: APP_ISSUER.into(),
        aud: app_audience(project),
        sub: Uuid::new_v4(),
        sid: Uuid::new_v4(),
        project,
        email: "a@example.com".into(),
        email_verified: true,
        role: "authenticated".into(),
        iat: now(),
        exp: now() + 300,
    };
    // Signed by the app key (`auth/app-jwt.key`).
    let (app_private, app_public) = generate_keypair_pem();
    let app_signer =
        AppTokenSigner::from_pem(app_private.as_bytes(), app_public.as_bytes()).unwrap();
    let app_token = app_signer.sign(&app).unwrap();
    assert_eq!(
        k.verifier.verify(&app_token),
        Err(MemberTokenError::Invalid)
    );

    // Even an app token signed with the member key itself is refused (audience, issuer,
    // claims).
    let same_key = AppTokenSigner::from_pem(k.private.as_bytes(), k.public.as_bytes()).unwrap();
    assert_eq!(
        k.verifier.verify(&same_key.sign(&app).unwrap()),
        Err(MemberTokenError::Invalid)
    );

    // A member token with an app-token claim (`role`) is refused.
    let mut with_role = as_value(&claims());
    with_role["role"] = json!("authenticated");
    let token = sign_raw(&k.private, Some(k.verifier.kid()), &with_role);
    assert_eq!(k.verifier.verify(&token), Err(MemberTokenError::Invalid));

    // The reverse: a member token where an app JWT is expected.
    let member = k.signer.sign(&claims()).unwrap();
    let app_verifier = AppTokenVerifier::from_pem(app_public.as_bytes()).unwrap();
    assert!(app_verifier.verify(&member, project).is_err());
    // Even if the app verifier held the member key.
    let app_on_member_key = AppTokenVerifier::from_pem(k.public.as_bytes()).unwrap();
    assert!(app_on_member_key.verify(&member, project).is_err());
}

// The gateway's internal identity token is not a member token, and the reverse.
#[test]
fn internal_token_refused() {
    let k = keys();
    let (gw_private, gw_public) = generate_keypair_pem();
    let identity = Identity {
        sub: "user_7Hq2".into(),
        auth: AuthMethod::ManagementJwt,
        request_id: "rid".into(),
        data: None,
        email: None,
        name: None,
    };
    let internal = InternalSigner::from_pem(gw_private.as_bytes()).unwrap();
    let token = internal
        .sign_for_tenant("orchestrator", &identity, Uuid::new_v4(), "sid-1")
        .unwrap();
    assert_eq!(k.verifier.verify(&token), Err(MemberTokenError::Invalid));
    // Even one the member key signed (no `kid`, another issuer and audience).
    let on_member_key = InternalSigner::from_pem(k.private.as_bytes()).unwrap();
    let token = on_member_key
        .sign_for_tenant(MEMBER_AUDIENCE, &identity, Uuid::new_v4(), "sid-1")
        .unwrap();
    assert_eq!(k.verifier.verify(&token), Err(MemberTokenError::Invalid));

    // The reverse: a member token as the internal identity.
    let member = k.signer.sign(&claims()).unwrap();
    let gw_verifier = InternalVerifier::from_pem(gw_public.as_bytes()).unwrap();
    assert!(gw_verifier.verify(&member, "orchestrator").is_err());
    assert!(gw_verifier.verify(&member, MEMBER_AUDIENCE).is_err());
}

// AC 3: `alg: none`, and an HMAC token keyed with the public key (algorithm confusion).
#[test]
fn alg_none_refused() {
    let k = keys();
    let payload = b64(&as_value(&claims()));
    for alg in ["none", "None", "NONE"] {
        let header = b64(&json!({ "alg": alg, "typ": "JWT", "kid": k.verifier.kid() }));
        for sig in ["", "AAAA"] {
            let token = format!("{header}.{payload}.{sig}");
            assert_eq!(
                k.verifier.verify(&token),
                Err(MemberTokenError::Invalid),
                "{alg} {sig:?}"
            );
        }
    }
    let mut header = Header::new(Algorithm::HS256);
    header.kid = Some(k.verifier.kid().to_string());
    let hmac = encode(
        &header,
        &as_value(&claims()),
        &EncodingKey::from_secret(k.public.as_bytes()),
    )
    .unwrap();
    assert!(!member_shaped(&hmac));
    assert_eq!(k.verifier.verify(&hmac), Err(MemberTokenError::Invalid));
}

// AC 3: another Ed25519 key, whatever `kid` it names; and no `kid`.
#[test]
fn foreign_key_refused() {
    let k = keys();
    let other = keys();
    let c = as_value(&claims());
    // Under the real key's kid.
    let forged = sign_raw(&other.private, Some(k.verifier.kid()), &c);
    assert!(
        member_shaped(&forged),
        "an EdDSA token is member-shaped: it never falls through"
    );
    assert_eq!(k.verifier.verify(&forged), Err(MemberTokenError::Invalid));
    // Under its own kid.
    let own = other.signer.sign(&claims()).unwrap();
    assert_eq!(k.verifier.verify(&own), Err(MemberTokenError::Invalid));
    // The right key, but no kid or another kid.
    assert_eq!(
        k.verifier.verify(&sign_raw(&k.private, None, &c)),
        Err(MemberTokenError::Invalid)
    );
    assert_eq!(
        k.verifier
            .verify(&sign_raw(&k.private, Some("0000000000000000"), &c)),
        Err(MemberTokenError::Invalid)
    );
    // A tampered payload under a valid signature.
    let good = k.signer.sign(&claims()).unwrap();
    let parts: Vec<&str> = good.split('.').collect();
    let mut evil = as_value(&claims());
    evil["tid"] = json!(Uuid::new_v4());
    let tampered = format!("{}.{}.{}", parts[0], b64(&evil), parts[2]);
    assert_eq!(k.verifier.verify(&tampered), Err(MemberTokenError::Invalid));
}

// AC 3: the right key, `aud` or `iss` changed (including lists that name the right one).
#[test]
fn aud_iss_mismatch_refused() {
    let k = keys();
    let kid = k.verifier.kid().to_string();
    let cases: Vec<(&str, Value)> = vec![
        ("aud", json!(app_audience(Uuid::new_v4()))),
        ("aud", json!("chelabase-member ")),
        ("aud", json!([MEMBER_AUDIENCE, "other"])),
        ("aud", json!([MEMBER_AUDIENCE])),
        ("aud", json!("")),
        ("iss", json!("http://evil.example")),
        ("iss", json!(APP_ISSUER)),
        ("iss", json!("chelabase-gateway")),
        ("iss", json!([ISSUER])),
        ("iss", json!(format!("{ISSUER}/"))),
    ];
    for (claim, value) in cases {
        let mut c = as_value(&claims());
        c[claim] = value.clone();
        let token = sign_raw(&k.private, Some(&kid), &c);
        assert_eq!(
            k.verifier.verify(&token),
            Err(MemberTokenError::Invalid),
            "{claim} = {value}"
        );
    }
    for missing in ["aud", "iss", "exp", "iat", "sub", "tid", "sid"] {
        let mut c = as_value(&claims());
        c.as_object_mut().unwrap().remove(missing);
        let token = sign_raw(&k.private, Some(&kid), &c);
        assert_eq!(
            k.verifier.verify(&token),
            Err(MemberTokenError::Invalid),
            "no {missing}"
        );
    }
    // A verifier for another issuer refuses a good token.
    let elsewhere =
        MemberTokenVerifier::from_pem(k.public.as_bytes(), "http://auth.other:8000").unwrap();
    assert_eq!(
        elsewhere.verify(&k.signer.sign(&claims()).unwrap()),
        Err(MemberTokenError::Invalid)
    );
}

// Only the tenant id names the tenant: a nil one or an empty subject is refused.
#[test]
fn tenant_and_subject_required() {
    let k = keys();
    let mut nil = claims();
    nil.tid = Uuid::nil();
    assert_eq!(
        k.verifier.verify(&k.signer.sign(&nil).unwrap()),
        Err(MemberTokenError::Invalid)
    );
    let mut no_sub = claims();
    no_sub.sub = String::new();
    assert_eq!(
        k.verifier.verify(&k.signer.sign(&no_sub).unwrap()),
        Err(MemberTokenError::Invalid)
    );
    // Review fix 1: the session is what logout and a reset end; a token naming none is refused.
    for sid in ["", " "] {
        let mut no_sid = claims();
        no_sid.sid = sid.into();
        assert_eq!(
            k.verifier.verify(&k.signer.sign(&no_sid).unwrap()),
            Err(MemberTokenError::Invalid),
            "sid {sid:?}"
        );
    }
}

// AC 6: expired more than 30 s ago is refused; within the leeway passes. `iat` in the future
// beyond the leeway, or a lifetime past the cap, is refused.
#[test]
fn expired_beyond_leeway() {
    let k = keys();
    let mut c = claims();
    c.iat = now() - 400;
    c.exp = now() - 31;
    assert_eq!(
        k.verifier.verify(&k.signer.sign(&c).unwrap()),
        Err(MemberTokenError::Expired)
    );
    c.exp = now() - 10;
    assert!(
        k.verifier.verify(&k.signer.sign(&c).unwrap()).is_ok(),
        "inside the 30 s leeway"
    );

    let mut future = claims();
    future.iat = now() + 120;
    future.exp = future.iat + 300;
    assert_eq!(
        k.verifier.verify(&k.signer.sign(&future).unwrap()),
        Err(MemberTokenError::Invalid)
    );

    let mut long = claims();
    long.exp = long.iat + MEMBER_MAX_TTL_SECS + 1;
    assert_eq!(
        k.verifier.verify(&k.signer.sign(&long).unwrap()),
        Err(MemberTokenError::Invalid)
    );
    long.exp = long.iat + MEMBER_MAX_TTL_SECS;
    assert!(k.verifier.verify(&k.signer.sign(&long).unwrap()).is_ok());
}

// The member-shaped rule the gateway and realtime route on: EdDSA in a parsable header.
#[test]
fn member_shape_is_the_header_algorithm() {
    assert!(!member_shaped(""));
    assert!(!member_shaped("not.a.jwt"));
    let mut rs = Header::new(Algorithm::RS256);
    rs.kid = Some("k".into());
    let header = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&rs).unwrap());
    assert!(!member_shaped(&format!("{header}.e30.sig")));
    let ed = URL_SAFE_NO_PAD.encode(br#"{"alg":"EdDSA","typ":"JWT"}"#);
    assert!(member_shaped(&format!("{ed}.e30.sig")));
}

// The internal identity never names the nil tenant: the signer refuses to mint one and the
// verifier refuses one minted by hand.
#[test]
fn internal_identity_never_names_the_nil_tenant() {
    use chelabase_auth::internal::InternalClaims;
    let (private, public) = generate_keypair_pem();
    let signer = InternalSigner::from_pem(private.as_bytes()).unwrap();
    let verifier = InternalVerifier::from_pem(public.as_bytes()).unwrap();
    let identity = Identity {
        sub: "u".into(),
        auth: AuthMethod::ManagementJwt,
        request_id: "r".into(),
        data: None,
        email: None,
        name: None,
    };
    assert!(
        signer
            .sign_for_tenant("orchestrator", &identity, Uuid::nil(), "sid-1")
            .is_err()
    );
    let tid = Uuid::new_v4();
    // Review fix 1: a member identity names its session, which the orchestrator checks.
    assert!(
        signer
            .sign_for_tenant("orchestrator", &identity, tid, " ")
            .is_err()
    );
    let token = signer
        .sign_for_tenant("orchestrator", &identity, tid, "sid-1")
        .unwrap();
    let claims = verifier.verify(&token, "orchestrator").unwrap();
    assert_eq!(claims.tenant_id, Some(tid));
    assert_eq!(claims.sid.as_deref(), Some("sid-1"));
    // A data key's identity names neither.
    let token = signer.sign("orchestrator", &identity).unwrap();
    assert_eq!(verifier.verify(&token, "orchestrator").unwrap().sid, None);

    let claims = InternalClaims {
        iss: "chelabase-gateway".into(),
        aud: "orchestrator".into(),
        iat: now(),
        exp: now() + 60,
        sub: "u".into(),
        tenant_id: Some(Uuid::nil()),
        auth: AuthMethod::ManagementJwt,
        rid: "r".into(),
        data: None,
        email: None,
        name: None,
    sid: Some("sid-1".into()),
    };
    let token = encode(
        &Header::new(Algorithm::EdDSA),
        &claims,
        &EncodingKey::from_ed_pem(private.as_bytes()).unwrap(),
    )
    .unwrap();
    assert!(verifier.verify(&token, "orchestrator").is_err());
}
