//! Mint a short-lived Zitadel access token via JWT profile (machine key).

use crate::paths::{issuer, load_dotenv_file, zitadel_dir};
use anyhow::{bail, Context, Result};
use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use serde::Deserialize;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ServiceAccountKey {
    key_id: String,
    key: String,
    user_id: String,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
}

#[derive(Debug, serde::Serialize)]
struct AssertionClaims {
    iss: String,
    sub: String,
    aud: String,
    iat: u64,
    exp: u64,
}

pub async fn run() -> Result<()> {
    let zdir = zitadel_dir();
    let sa_path = zdir.join("demo-sa.json");
    let env_path = zdir.join("demo.env");

    if !sa_path.is_file() {
        bail!(
            "missing {} — run: just zitadel-bootstrap (or cargo run -p chelabase -- dev bootstrap)",
            sa_path.display()
        );
    }

    let env = load_dotenv_file(&env_path);
    // The issuer comes from the entry, never from demo.env (a copy that can be stale).
    let iss = issuer()?;
    let project_id = env
        .get("CHELABASE_ZITADEL_PROJECT_ID")
        .context("missing CHELABASE_ZITADEL_PROJECT_ID in demo.env — run auth bootstrap")?;

    let key: ServiceAccountKey = serde_json::from_str(
        &std::fs::read_to_string(&sa_path).with_context(|| format!("read {}", sa_path.display()))?,
    )
    .context("parse demo-sa.json")?;

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_secs();

    let mut header = Header::new(Algorithm::RS256);
    header.kid = Some(key.key_id.clone());

    let claims = AssertionClaims {
        iss: key.user_id.clone(),
        sub: key.user_id.clone(),
        aud: iss.clone(),
        iat: now,
        exp: now + 3600,
    };

    let encoding_key = EncodingKey::from_rsa_pem(key.key.as_bytes())
        .context("parse RSA private key from demo-sa.json")?;
    let assertion = encode(&header, &claims, &encoding_key).context("sign JWT assertion")?;

    let scope = format!(
        "openid profile urn:zitadel:iam:user:resourceowner urn:zitadel:iam:org:project:id:{project_id}:aud"
    );

    // Never a redirect (#26): the signed assertion goes to the issuer only.
    let client = chelabase_http::client_builder()
        .build()
        .context("an HTTP client")?;
    let resp = client
        .post(format!("{iss}/oauth/v2/token"))
        .form(&[
            (
                "grant_type",
                "urn:ietf:params:oauth:grant-type:jwt-bearer",
            ),
            ("scope", scope.as_str()),
            ("assertion", assertion.as_str()),
        ])
        .send()
        .await
        .context("token request")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        bail!("token endpoint {status}: {body}");
    }

    let token: TokenResponse = resp.json().await.context("parse token response")?;
    // Token only on stdout so shell capture stays clean; logs go to stderr via tracing.
    print!("{}", token.access_token);
    Ok(())
}
