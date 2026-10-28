use chelabase_auth::test_support::{BrokenJwks, MockJwks};
use chelabase_auth::{AuthError, JwtVerifier, VerifiedClaims};
use serde_json::json;
use std::time::{SystemTime, UNIX_EPOCH};

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

/// Assert the verify() call failed with `InvalidToken` whose message contains
/// `needle` — not just any `InvalidToken`. Distinguishing *why* verification
/// failed matters: several failure paths (kid miss, bad signature, expired,
/// wrong issuer) all land on the same enum variant, and a test that only
/// matches the variant would keep passing even if the wrong check tripped.
fn assert_invalid_token_containing(result: Result<VerifiedClaims, AuthError>, needle: &str) {
    match result {
        Ok(claims) => panic!("expected InvalidToken containing {needle:?}, got Ok({claims:?})"),
        Err(AuthError::InvalidToken(msg)) => {
            assert!(
                msg.contains(needle),
                "expected InvalidToken message to contain {needle:?}, got {msg:?}"
            );
        }
        Err(other) => panic!("expected InvalidToken containing {needle:?}, got {other:?}"),
    }
}

#[tokio::test]
async fn valid_token_verifies_successfully() {
    let mock = MockJwks::start().await;
    let verifier = JwtVerifier::new(mock.config(vec![]));
    let token = mock.token_for_org("user-1", "org-1", 300);

    let claims = verifier.verify(&token).await.expect("should verify");
    assert_eq!(claims.sub, "user-1");
    assert_eq!(claims.org_id, "org-1");
}

#[tokio::test]
async fn tampered_signature_is_rejected() {
    let mock = MockJwks::start().await;
    let verifier = JwtVerifier::new(mock.config(vec![]));
    let token = mock.token_for_org("user-1", "org-1", 300);
    let tampered = tamper_signature(&token);

    assert_invalid_token_containing(verifier.verify(&tampered).await, "InvalidSignature");
}

// RT1 S14: expiry is its own error, so realtime can answer `token_expired` (wait for the
// renewal) rather than `forbidden` (leave the channel).
#[tokio::test]
async fn expired_token_is_rejected() {
    let mock = MockJwks::start().await;
    let verifier = JwtVerifier::new(mock.config(vec![]));
    let token = mock.sign(json!({
        "sub": "user-1",
        "iss": mock.issuer(),
        "exp": now() - 3600,
        chelabase_auth::CLAIM_ORG_ID: "org-1",
    }));

    let err = verifier.verify(&token).await.unwrap_err();
    assert!(matches!(err, AuthError::Expired), "got {err:?}");
}

// RT1 S14: the leeway stays 60 s; only a token past it is `Expired`.
#[tokio::test]
async fn a_token_within_the_leeway_still_verifies_and_carries_its_exp() {
    let mock = MockJwks::start().await;
    let verifier = JwtVerifier::new(mock.config(vec![]));
    let exp = now() - 20;
    let token = mock.sign(json!({
        "sub": "user-1",
        "iss": mock.issuer(),
        "exp": exp,
        chelabase_auth::CLAIM_ORG_ID: "org-1",
    }));
    let claims = verifier.verify(&token).await.expect("within the 60 s leeway");
    assert_eq!(claims.exp, exp as u64);
}

// RT1 S14: `VerifiedClaims.exp` is the token's `exp` in unix seconds.
#[tokio::test]
async fn verified_claims_carry_the_tokens_exp() {
    let mock = MockJwks::start().await;
    let verifier = JwtVerifier::new(mock.config(vec![]));
    let exp = now() + 300;
    let token = mock.sign(json!({
        "sub": "user-1",
        "iss": mock.issuer(),
        "exp": exp,
        "email": "a@x.test",
        "name": "A",
        chelabase_auth::CLAIM_ORG_ID: "org-1",
    }));
    let claims = verifier.verify(&token).await.expect("should verify");
    assert_eq!(claims.exp, exp as u64);
    assert_eq!((claims.email.as_deref(), claims.name.as_deref()), (Some("a@x.test"), Some("A")));
}

// RT1 S14: the JWKS fetch gives up after 3 s, so a hung Zitadel makes an authorize answer
// `unavailable` well inside realtime's 20 s budget instead of hanging it.
#[tokio::test]
async fn a_hung_jwks_endpoint_fails_within_its_timeout() {
    use axum::{routing::get, Router};
    let app = Router::new().route(
        "/oauth/v2/keys",
        get(|| async {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            "never"
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let verifier = JwtVerifier::new(chelabase_auth::ZitadelAuthConfig {
        issuer: format!("http://{addr}"),
        jwks_url: format!("http://{addr}/oauth/v2/keys"),
        jwks_host: None,
        audiences: vec![chelabase_auth::test_support::MOCK_AUDIENCE.to_string()].into(),
    });
    let mock = MockJwks::start().await;
    let token = mock.token_for_org("user-1", "org-1", 300);
    let started = std::time::Instant::now();
    let result = tokio::time::timeout(std::time::Duration::from_secs(8), verifier.verify(&token)).await;
    let waited = started.elapsed();
    let Ok(result) = result else {
        panic!("verify waited more than 8 s on a hung JWKS endpoint (no fetch timeout)");
    };
    assert!(matches!(result, Err(AuthError::Jwks(_))), "got {result:?}");
    assert!(waited >= std::time::Duration::from_millis(2500), "gave up after {waited:?}, before 3 s");
}

#[tokio::test]
async fn wrong_issuer_is_rejected() {
    let mock = MockJwks::start().await;
    let verifier = JwtVerifier::new(mock.config(vec![]));
    let token = mock.sign(json!({
        "sub": "user-1",
        "iss": "http://not-the-configured-issuer",
        "exp": now() + 300,
        chelabase_auth::CLAIM_ORG_ID: "org-1",
    }));

    assert_invalid_token_containing(verifier.verify(&token).await, "InvalidIssuer");
}

#[tokio::test]
async fn unknown_kid_is_rejected() {
    let mock = MockJwks::start().await;
    let verifier = JwtVerifier::new(mock.config(vec![]));

    // Sign with the mock's real key, then swap the header's kid to one the
    // mock's JWKS response doesn't contain.
    let token = mock.token_for_org("user-1", "org-1", 300);
    let tampered = tamper_kid(&token, "no-such-kid");

    assert_invalid_token_containing(verifier.verify(&tampered).await, "no jwk for kid");
}

#[tokio::test]
async fn missing_kid_header_is_rejected() {
    let mock = MockJwks::start().await;
    let verifier = JwtVerifier::new(mock.config(vec![]));
    let tampered = tamper_kid_removed(&mock.token_for_org("user-1", "org-1", 300));

    assert_invalid_token_containing(verifier.verify(&tampered).await, "missing kid");
}

#[tokio::test]
async fn malformed_token_is_rejected() {
    let mock = MockJwks::start().await;
    let verifier = JwtVerifier::new(mock.config(vec![]));

    let err = verifier.verify("not-a-jwt").await.unwrap_err();
    assert!(matches!(err, AuthError::InvalidToken(_)), "got {err:?}");
}

#[tokio::test]
async fn empty_token_is_unauthorized() {
    let mock = MockJwks::start().await;
    let verifier = JwtVerifier::new(mock.config(vec![]));

    let err = verifier.verify("   ").await.unwrap_err();
    assert!(matches!(err, AuthError::Unauthorized), "got {err:?}");
}

#[tokio::test]
async fn audience_mismatch_is_rejected_end_to_end() {
    let mock = MockJwks::start().await;
    let verifier = JwtVerifier::new(mock.config(vec!["expected-aud".into()]));
    let token = mock.sign(json!({
        "sub": "user-1",
        "iss": mock.issuer(),
        "exp": now() + 300,
        "aud": "wrong-aud",
        chelabase_auth::CLAIM_ORG_ID: "org-1",
    }));

    assert_invalid_token_containing(
        verifier.verify(&token).await,
        "audience does not match",
    );
}

#[tokio::test]
async fn jwks_endpoint_unreachable_surfaces_jwks_error() {
    let broken = BrokenJwks::start().await;
    let config = chelabase_auth::ZitadelAuthConfig {
        issuer: "http://irrelevant-issuer".into(),
        jwks_url: broken.jwks_url().to_string(),
        jwks_host: None,
        audiences: vec![chelabase_auth::test_support::MOCK_AUDIENCE.to_string()].into(),
    };
    let verifier = JwtVerifier::new(config);

    // Any syntactically valid header with a kid triggers a JWKS fetch attempt,
    // which fails against `broken` before the issuer above is ever checked.
    let mock = MockJwks::start().await;
    let token = mock.token_for_org("user-1", "org-1", 300);

    let err = verifier.verify(&token).await.unwrap_err();
    assert!(matches!(err, AuthError::Jwks(_)), "got {err:?}");
}

#[tokio::test]
async fn org_claim_falls_back_to_resource_owner_end_to_end() {
    let mock = MockJwks::start().await;
    let verifier = JwtVerifier::new(mock.config(vec![]));
    let token = mock.sign(json!({
        "sub": "user-1",
        "iss": mock.issuer(),
        "exp": now() + 300,
        chelabase_auth::CLAIM_RESOURCE_OWNER_ID: "org-from-resource-owner",
    }));

    let claims = verifier.verify(&token).await.expect("should verify");
    assert_eq!(claims.org_id, "org-from-resource-owner");
}

/// Flip a bit in the signature segment so the token's kid/header/payload stay
/// intact (and thus still resolve to a real, cached key) but the signature no
/// longer matches — isolating "signature verification" from "kid lookup".
fn tamper_signature(token: &str) -> String {
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
    let (head_and_payload, sig_b64) = token.rsplit_once('.').unwrap();
    let mut sig_bytes = URL_SAFE_NO_PAD.decode(sig_b64).unwrap();
    sig_bytes[0] ^= 0xFF;
    let tampered_sig = URL_SAFE_NO_PAD.encode(sig_bytes);
    format!("{head_and_payload}.{tampered_sig}")
}

/// Swaps the base64url header segment's `kid` field for negative tests.
/// jsonwebtoken's decode path resolves the `kid` (via our JWKS lookup) before it
/// validates the signature, so this exercises the "kid not found" branch — the
/// message assertion in `unknown_kid_is_rejected` is what actually proves that,
/// since changing the header also invalidates the signature.
fn tamper_kid(token: &str, new_kid: &str) -> String {
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
    let mut parts = token.splitn(3, '.');
    let header_b64 = parts.next().unwrap();
    let rest: Vec<&str> = parts.collect();
    let mut header: serde_json::Value =
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(header_b64).unwrap()).unwrap();
    header["kid"] = serde_json::Value::String(new_kid.to_string());
    let new_header_b64 = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&header).unwrap());
    format!("{new_header_b64}.{}", rest.join("."))
}

fn tamper_kid_removed(token: &str) -> String {
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
    let mut parts = token.splitn(3, '.');
    let header_b64 = parts.next().unwrap();
    let rest: Vec<&str> = parts.collect();
    let mut header: serde_json::Value =
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(header_b64).unwrap()).unwrap();
    header.as_object_mut().unwrap().remove("kid");
    let new_header_b64 = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&header).unwrap());
    format!("{new_header_b64}.{}", rest.join("."))
}

// #24: behind TLS with no port in the hosts, the public issuer (`https://auth.example.com`)
// isn't reachable inside the network, so the keys are fetched at Zitadel's in-network address
// (CHELABASE_ZITADEL_INTERNAL_URL) with the auth host as `Host`. The issuer checked stays the
// public one.

const PUBLIC_AUTH_HOST: &str = "auth.example.com";
const PUBLIC_ISSUER: &str = "https://auth.example.com";

/// A stand-in for Zitadel at its in-network address: like Zitadel, it picks its instance by
/// `Host`, so it serves the keys (the mock's) only to a request for the public auth host and
/// answers 404 to any other. Returns its origin, `http://127.0.0.1:<port>`.
async fn zitadel_in_network(mock: &MockJwks) -> String {
    use axum::{http::HeaderMap, http::StatusCode, response::IntoResponse, routing::get, Router};
    let keys: serde_json::Value = reqwest::get(mock.jwks_url()).await.unwrap().json().await.unwrap();
    let app = Router::new().route(
        "/oauth/v2/keys",
        get(move |headers: HeaderMap| {
            let keys = keys.clone();
            async move {
                match headers.get("host").and_then(|h| h.to_str().ok()) {
                    Some(PUBLIC_AUTH_HOST) => axum::Json(keys).into_response(),
                    _ => (StatusCode::NOT_FOUND, "unable to set instance").into_response(),
                }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

fn behind_tls(internal_url: &str) -> chelabase_auth::ZitadelAuthConfig {
    let pairs = [
        ("CHELABASE_PUBLIC_SCHEME", "https".to_string()),
        ("CHELABASE_AUTH_HOST", PUBLIC_AUTH_HOST.to_string()),
        ("CHELABASE_ZITADEL_INTERNAL_URL", internal_url.to_string()),
        ("CHELABASE_ZITADEL_AUDIENCES", chelabase_auth::test_support::MOCK_AUDIENCE.to_string()),
    ];
    chelabase_auth::ZitadelAuthConfig::from_lookup(|k| {
        pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| v.clone())
    })
    .expect("config")
}

fn claims_from(iss: &str) -> serde_json::Value {
    json!({
        "sub": "user-1",
        "iss": iss,
        "exp": now() + 300,
        chelabase_auth::CLAIM_ORG_ID: "org-1",
    })
}

#[tokio::test]
async fn behind_tls_the_keys_come_from_the_in_network_url_and_the_public_issuer_verifies() {
    let mock = MockJwks::start().await;
    let internal = zitadel_in_network(&mock).await;
    let verifier = JwtVerifier::new(behind_tls(&internal));
    assert_eq!(verifier.config().issuer, PUBLIC_ISSUER);

    let claims = verifier
        .verify(&mock.sign(claims_from(PUBLIC_ISSUER)))
        .await
        .expect("a token from the public issuer verifies with keys fetched in-network");
    assert_eq!(claims.sub, "user-1");
}

#[tokio::test]
async fn behind_tls_a_token_not_from_the_public_issuer_is_refused() {
    let mock = MockJwks::start().await;
    let internal = zitadel_in_network(&mock).await;
    let verifier = JwtVerifier::new(behind_tls(&internal));
    // Signed with the very keys the in-network fetch returns: only `iss` is wrong. Neither the
    // in-network address the keys came from, nor the public host over plain http, nor a
    // different host passes.
    for iss in [
        internal.as_str(),
        "http://auth.example.com",
        "https://auth.example.com:443",
        "https://auth.example.com/",
        "https://evil.example.com",
        "",
    ] {
        assert_invalid_token_containing(verifier.verify(&mock.sign(claims_from(iss))).await, "InvalidIssuer");
    }
    let mut no_iss = claims_from(PUBLIC_ISSUER);
    no_iss.as_object_mut().unwrap().remove("iss");
    assert!(verifier.verify(&mock.sign(no_iss)).await.is_err(), "a token without iss passed");
}

#[tokio::test]
async fn behind_tls_a_failed_key_fetch_passes_no_token() {
    let mock = MockJwks::start().await;
    let token = mock.sign(claims_from(PUBLIC_ISSUER));
    // A failing Zitadel.
    let broken = BrokenJwks::start().await;
    let origin = broken.jwks_url().trim_end_matches("/oauth/v2/keys").to_string();
    let err = JwtVerifier::new(behind_tls(&origin)).verify(&token).await.unwrap_err();
    assert!(matches!(err, AuthError::Jwks(_)), "got {err:?}");
    // Nothing listening (a just-bound and dropped port).
    let closed = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        format!("http://{}", l.local_addr().unwrap())
    };
    let err = JwtVerifier::new(behind_tls(&closed)).verify(&token).await.unwrap_err();
    assert!(matches!(err, AuthError::Jwks(_)), "got {err:?}");
}

// The in-network fetch names the public auth host: a Zitadel asked for another host answers
// 404 (its "unable to set instance"), and that is a failed fetch, never a pass.
#[tokio::test]
async fn behind_tls_the_fetch_carries_the_auth_host() {
    let mock = MockJwks::start().await;
    let internal = zitadel_in_network(&mock).await;
    let config = behind_tls(&internal);
    assert_eq!(config.jwks_host.as_deref(), Some(PUBLIC_AUTH_HOST));
    let token = mock.sign(claims_from(PUBLIC_ISSUER));
    for host in [Some("auth.other.example"), None] {
        let mut config = config.clone();
        config.jwks_host = host.map(str::to_string);
        let err = JwtVerifier::new(config).verify(&token).await.unwrap_err();
        assert!(matches!(err, AuthError::Jwks(_)), "{host:?}: got {err:?}");
    }
}

// #24 follow-up: every claim the Zitadel verifier checks is required. `iss` and `exp` through
// jsonwebtoken's required claims, `aud` by the audience check (validate_aud is off because the
// audiences are a runtime list): a token missing one, signed with the right key, is refused.
#[tokio::test]
async fn a_zitadel_token_without_iss_aud_or_exp_is_refused() {
    let mock = MockJwks::start().await;
    let verifier = JwtVerifier::new(mock.config(vec![]));
    let full = json!({
        "sub": "user-1",
        "iss": mock.issuer(),
        "aud": chelabase_auth::test_support::MOCK_AUDIENCE,
        "exp": now() + 300,
        chelabase_auth::CLAIM_ORG_ID: "org-1",
    });
    verifier.verify(&mock.sign_exact(full.clone())).await.expect("the full claims pass");
    for (missing, needle) in [("iss", "Missing required claim: iss"), ("exp", "Missing required claim: exp"), ("aud", "names no audience")] {
        let mut c = full.clone();
        c.as_object_mut().unwrap().remove(missing);
        assert_invalid_token_containing(verifier.verify(&mock.sign_exact(c)).await, needle);
    }
}

// m13 audit B1: `iss` is one string (RFC 7519 4.1.1). jsonwebtoken's issuer check also takes a
// JSON array and passes it when any entry matches; a token naming the issuer among others, or
// as a one-entry list, isn't from the issuer and is refused.
#[tokio::test]
async fn an_iss_list_is_refused_even_when_it_holds_the_issuer() {
    let mock = MockJwks::start().await;
    let verifier = JwtVerifier::new(mock.config(vec![]));
    for iss in [
        json!([mock.issuer(), "https://evil.example.com"]),
        json!(["https://evil.example.com", mock.issuer()]),
        json!([mock.issuer()]),
        json!([mock.issuer(), mock.issuer()]),
        json!([]),
    ] {
        let token = mock.sign(json!({
            "sub": "user-1",
            "iss": iss,
            "exp": now() + 300,
            chelabase_auth::CLAIM_ORG_ID: "org-1",
        }));
        assert_invalid_token_containing(verifier.verify(&token).await, "InvalidIssuer");
    }
}

// m13 audit B1, the audience side (#22): `aud` may be a string or a list (RFC 7519 4.1.3;
// Zitadel's tokens name the project and the client). A list passes when one entry is one of our
// audiences, whatever else it names; a list naming none of them, an empty list, or a non-string
// `aud` is refused. Non-string entries in a list never match.
#[tokio::test]
async fn an_aud_list_passes_only_when_it_names_one_of_our_audiences() {
    let mock = MockJwks::start().await;
    let verifier = JwtVerifier::new(mock.config(vec!["our-project".into(), "our-client".into()]));
    let token = |aud: serde_json::Value| {
        mock.sign_exact(json!({
            "sub": "user-1",
            "iss": mock.issuer(),
            "aud": aud,
            "exp": now() + 300,
            chelabase_auth::CLAIM_ORG_ID: "org-1",
        }))
    };
    for aud in [
        json!("our-project"),
        json!(["our-client"]),
        json!(["someone-else", "our-project"]),
        json!(["our-project", "our-client", "another-app"]),
    ] {
        verifier.verify(&token(aud.clone())).await.unwrap_or_else(|e| panic!("{aud}: {e:?}"));
    }
    for (aud, needle) in [
        (json!(["someone-else", "another-app"]), "does not match"),
        (json!("someone-else"), "does not match"),
        (json!(["Our-Project"]), "does not match"),
        (json!([]), "names no audience"),
        (json!(""), "names no audience"),
        (json!([""]), "names no audience"),
        (json!(42), "names no audience"),
        (json!({"aud": "our-project"}), "names no audience"),
        (json!([42, null, {"x": "our-project"}]), "names no audience"),
    ] {
        assert_invalid_token_containing(verifier.verify(&token(aud.clone())).await, needle);
    }
}

