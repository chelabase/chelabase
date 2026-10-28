//! #22 (selfhost-install m5 fix round): the OIDC audience check is never skipped. An empty
//! audience list is refused at startup and at verify time; a token without an audience is
//! refused; audiences may come late from the bootstrap's files, and until they do every token
//! is refused as "not ready" (503), never let through unchecked.

use chelabase_auth::test_support::MockJwks;
use chelabase_auth::{AuthError, JwtVerifier, ZitadelAuthConfig};
use serde_json::json;
use std::time::{SystemTime, UNIX_EPOCH};

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

fn vars(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
    let map: std::collections::HashMap<String, String> = pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    move |k| map.get(k).cloned()
}

const HOST: (&str, &str) = ("CHELABASE_AUTH_HOST", "auth.chelabase.localhost:8000");

fn token(mock: &MockJwks, aud: serde_json::Value) -> String {
    let mut claims = json!({
        "sub": "user-1",
        "iss": mock.issuer(),
        "exp": now() + 300,
        chelabase_auth::CLAIM_ORG_ID: "org-1",
    });
    if !aud.is_null() {
        claims["aud"] = aud;
    }
    mock.sign_exact(claims)
}

// AC 2: no audience in any form, in either mode, refuses every Zitadel token (the service
// starts, because the workbench starts the gateway before its bootstrap writes demo.env, but
// isn't ready and lets nothing through). It used to skip the check and accept any audience.
#[tokio::test]
async fn no_audience_refuses_every_token() {
    let mock = MockJwks::start().await;
    for mode in ["self-hosted", "platform"] {
        for pairs in [
            vec![HOST, ("CHELABASE_MODE", mode)],
            vec![
                HOST,
                ("CHELABASE_MODE", mode),
                ("CHELABASE_ZITADEL_AUDIENCES", ""),
            ],
            vec![
                HOST,
                ("CHELABASE_MODE", mode),
                ("CHELABASE_ZITADEL_AUDIENCES", " , ,"),
            ],
        ] {
            let mut config = ZitadelAuthConfig::from_lookup(vars(&pairs)).unwrap();
            config.issuer = mock.issuer().into();
            config.jwks_url = mock.jwks_url().into();
            let verifier = JwtVerifier::new(config);
            for aud in [json!("anyone"), json!(null)] {
                let got = verifier.verify(&token(&mock, aud.clone())).await;
                let err = match got {
                    Err(e) => e.to_string(),
                    Ok(c) => {
                        panic!("{pairs:?} aud {aud}: no audience configured, yet accepted: {c:?}")
                    }
                };
                assert!(
                    err.contains("CHELABASE_ZITADEL_PROJECT_ID_FILE"),
                    "{pairs:?}: {err}"
                );
            }
        }
    }
}

// An empty list built in code (no config path makes one) still accepts nothing.
#[tokio::test]
async fn an_empty_audience_list_never_accepts_a_token() {
    let mock = MockJwks::start().await;
    let verifier = JwtVerifier::new(ZitadelAuthConfig {
        issuer: mock.issuer().into(),
        jwks_url: mock.jwks_url().into(),
        audiences: Vec::<String>::new().into(),
    });
    for aud in [json!("anyone"), json!(["a", "b"]), json!(null)] {
        let got = verifier.verify(&token(&mock, aud.clone())).await;
        assert!(
            got.is_err(),
            "aud {aud}: an empty audience list accepted the token: {got:?}"
        );
    }
}

// A token with no `aud` (or an empty one) is refused when audiences are configured: the check
// is never skipped.
#[tokio::test]
async fn a_token_without_an_audience_is_refused() {
    let mock = MockJwks::start().await;
    let verifier = JwtVerifier::new(mock.config(vec!["p1".into()]));
    for aud in [json!(null), json!(""), json!([])] {
        let got = verifier.verify(&token(&mock, aud.clone())).await;
        assert!(
            matches!(got, Err(AuthError::InvalidToken(_))),
            "aud {aud}: {got:?}"
        );
    }
    assert!(verifier.verify(&token(&mock, json!("p1"))).await.is_ok());
}

struct TmpDir(std::path::PathBuf);

impl TmpDir {
    fn new() -> Self {
        use std::os::unix::fs::PermissionsExt;
        let dir =
            std::env::temp_dir().join(format!("chelabase-audiences-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        Self(dir)
    }
    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn write_id(path: &std::path::Path, value: &str, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::remove_file(path);
    std::fs::write(path, value).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

fn file_config(mock: &MockJwks, dir: &std::path::Path) -> ZitadelAuthConfig {
    let p = dir.join("project-id").to_str().unwrap().to_string();
    let c = dir.join("api-client-id").to_str().unwrap().to_string();
    let mut config = ZitadelAuthConfig::from_lookup(vars(&[
        HOST,
        ("CHELABASE_ZITADEL_PROJECT_ID_FILE", &p),
        ("CHELABASE_ZITADEL_API_CLIENT_ID_FILE", &c),
    ]))
    .expect("the _FILE forms configure the audiences, written or not");
    config.issuer = mock.issuer().into();
    config.jwks_url = mock.jwks_url().into();
    config
}

// #22 AC 1 failure path: the audience files absent, or present but refused (loose mode, a
// symlink), and every token is refused as not ready, whatever its audience.
#[tokio::test]
async fn audience_not_yet_refuses() {
    let mock = MockJwks::start().await;
    let dir = TmpDir::new();
    let verifier = JwtVerifier::new(file_config(&mock, dir.path()));
    assert!(!verifier.config().audiences.is_ready());
    let check = |label: &'static str| {
        let v = verifier.clone();
        let mock = &mock;
        async move {
            for aud in [json!("p1"), json!("anything"), json!(null)] {
                let got = v.verify(&token(mock, aud.clone())).await;
                assert!(
                    matches!(got, Err(AuthError::NotReady(_))),
                    "{label} aud {aud}: {got:?}"
                );
            }
        }
    };
    check("absent").await;
    write_id(&dir.path().join("project-id"), "p1\n", 0o644);
    check("one of two").await;
    write_id(&dir.path().join("api-client-id"), "c-api\n", 0o666);
    check("group- and world-writable").await;
    std::fs::remove_file(dir.path().join("api-client-id")).unwrap();
    write_id(&dir.path().join("real"), "c-api\n", 0o644);
    std::os::unix::fs::symlink(dir.path().join("real"), dir.path().join("api-client-id")).unwrap();
    check("a symlink").await;
    let msg = verifier
        .verify(&token(&mock, json!("p1")))
        .await
        .unwrap_err()
        .to_string();
    assert!(
        msg.contains("CHELABASE_ZITADEL_API_CLIENT_ID_FILE"),
        "{msg}"
    );
    assert!(!verifier.config().audiences.is_ready());
}

// #22 AC 1: once both files are there, the right audience passes without a restart and
// another is refused.
#[tokio::test]
async fn late_audience_file_is_picked_up() {
    let mock = MockJwks::start().await;
    let dir = TmpDir::new();
    let verifier = JwtVerifier::new(file_config(&mock, dir.path()));
    assert!(matches!(
        verifier.verify(&token(&mock, json!("p1"))).await,
        Err(AuthError::NotReady(_))
    ));
    write_id(&dir.path().join("project-id"), "p1\n", 0o644);
    write_id(&dir.path().join("api-client-id"), "c-api\n", 0o644);
    assert!(verifier.config().audiences.is_ready());
    assert!(verifier.verify(&token(&mock, json!("p1"))).await.is_ok());
    assert!(
        verifier
            .verify(&token(&mock, json!(["x", "c-api"])))
            .await
            .is_ok()
    );
    let got = verifier.verify(&token(&mock, json!("someone-else"))).await;
    assert!(matches!(got, Err(AuthError::InvalidToken(_))), "{got:?}");
}

// #12 AC 3 for the audiences: a variable and its `_FILE` (or the list and a `_FILE`) is
// refused, naming the variable. The workbench's plain forms keep working.
#[test]
fn config_refuses_env_and_file_both() {
    for (pairs, name) in [
        (
            vec![
                HOST,
                ("CHELABASE_ZITADEL_PROJECT_ID", "p1"),
                ("CHELABASE_ZITADEL_PROJECT_ID_FILE", "/o/p"),
            ],
            "CHELABASE_ZITADEL_PROJECT_ID ",
        ),
        (
            vec![
                HOST,
                ("CHELABASE_ZITADEL_API_CLIENT_ID", "c"),
                ("CHELABASE_ZITADEL_API_CLIENT_ID_FILE", "/o/c"),
            ],
            "CHELABASE_ZITADEL_API_CLIENT_ID ",
        ),
        (
            vec![
                HOST,
                ("CHELABASE_ZITADEL_AUDIENCES", "p1,c"),
                ("CHELABASE_ZITADEL_PROJECT_ID_FILE", "/o/p"),
            ],
            "CHELABASE_ZITADEL_AUDIENCES",
        ),
    ] {
        let err = ZitadelAuthConfig::from_lookup(vars(&pairs))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains(name) && err.contains("not both"),
            "{pairs:?}: {err}"
        );
    }
    // The workbench's demo.env: the list wins over the plain ids it repeats.
    let c = ZitadelAuthConfig::from_lookup(vars(&[
        HOST,
        ("CHELABASE_ZITADEL_AUDIENCES", "p1, c-api"),
        ("CHELABASE_ZITADEL_PROJECT_ID", "p1"),
        ("CHELABASE_ZITADEL_API_CLIENT_ID", "c-api"),
    ]))
    .unwrap();
    assert_eq!(
        c.audiences.current().unwrap(),
        vec!["p1".to_string(), "c-api".to_string()]
    );
    let c = ZitadelAuthConfig::from_lookup(vars(&[HOST, ("CHELABASE_ZITADEL_PROJECT_ID", "p1")]))
        .unwrap();
    assert_eq!(c.audiences.current().unwrap(), vec!["p1".to_string()]);
}
