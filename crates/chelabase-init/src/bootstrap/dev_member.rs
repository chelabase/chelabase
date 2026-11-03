//! `--demo`: the workbench's dev member (#126). The bootstrap signs it up through auth with the
//! stack's setup code, the path a fresh install's first admin takes, so the dev member is the
//! seeded tenant's admin and holds a real identity account. Its email and a random password are
//! kept in the output dir (`dev-member.json`, 0600) for `chelabase dev token`. Nothing here
//! needs a privilege the product doesn't already give a first sign-up.
//!
//! A rerun with the file present changes nothing. A tenant that is already claimed (someone
//! signed up first, or the stack's metadata is from an older install) is a clear error.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::time::Duration;
use tracing::info;

use super::outputs::{DEV_MEMBER, Outputs};
use super::retry::{Check, Waiter};
use super::{HttpProbe, Probe};

/// The dev member's email unless `--admin-email` names another.
pub const DEFAULT_EMAIL: &str = "dev@chelabase.localhost";

/// What `dev-member.json` holds.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DevMember {
    pub email: String,
    pub password: String,
}

impl std::fmt::Debug for DevMember {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DevMember")
            .field("email", &self.email)
            .field("password", &"<redacted>")
            .finish()
    }
}

/// Where and how the dev member signs up. No `Debug`: it holds the setup code.
pub struct DevMemberConfig {
    /// auth's own address (`CHELABASE_AUTH_URL`, e.g. `http://auth:8084`), no trailing `/`.
    pub auth_url: String,
    /// Studio's origin: sign-up answers only Studio's pages.
    pub studio_origin: String,
    /// The stack's setup code (`CHELABASE_SETUP_CODE_FILE`).
    pub setup_code: String,
    pub email: String,
}

/// A random password that passes the complexity rule: upper, lower, digit and symbol, 31 bytes.
pub fn new_password() -> String {
    let mut bytes = [0u8; 12];
    getrandom::fill(&mut bytes).expect("OS random number generator");
    format!("Dev-{}-a1!", hex::encode(bytes))
}

/// The dev member, made once: `dev-member.json` when it is there, else a sign-up with the setup
/// code. The file is written before the sign-up (so a password that worked is never lost) and
/// replaced by nothing on a refusal: it is removed again.
pub async fn ensure(w: &Waiter, out: &Outputs, cfg: &DevMemberConfig) -> Result<DevMember> {
    if let Some(bytes) = out.read(DEV_MEMBER)? {
        let member: DevMember = serde_json::from_slice(&bytes).with_context(|| {
            format!(
                "{} isn't the dev member's file",
                out.path().join(DEV_MEMBER).display()
            )
        })?;
        info!(email = %member.email, "the dev member exists");
        return Ok(member);
    }
    let member = DevMember {
        email: cfg.email.clone(),
        password: new_password(),
    };
    let health = HttpProbe::new("auth", &format!("{}/health", cfg.auth_url));
    w.until(&health.name(), || health.check()).await?;
    out.write_secret(DEV_MEMBER, &serde_json::to_vec_pretty(&member)?)?;
    let http = chelabase_http::client_builder()
        .timeout(Duration::from_secs(60))
        .build()
        .context("the auth client")?;
    let signed_up = w
        .until("auth (the dev member's sign-up)", || {
            sign_up(&http, cfg, &member)
        })
        .await;
    if let Err(e) = signed_up {
        let _ = std::fs::remove_file(out.path().join(DEV_MEMBER));
        return Err(e.context("bootstrap step: the dev member"));
    }
    info!(email = %member.email, "the dev member signed up and holds the tenant's admin claim");
    Ok(member)
}

/// One `POST /auth/v1/sign-up` as Studio would send it: `Ready` once auth answers `studio`
/// (signed in: the claim is bound), `Wait` while auth isn't answering, an error otherwise.
pub async fn sign_up(
    http: &chelabase_http::Client,
    cfg: &DevMemberConfig,
    member: &DevMember,
) -> Result<Check<()>> {
    let url = format!("{}/auth/v1/sign-up", cfg.auth_url);
    let body = json!({
        "email": member.email,
        "password": member.password,
        "givenName": "Dev",
        "familyName": "Member",
        "setupCode": cfg.setup_code,
    });
    let answer = http
        .post(&url)
        .header("origin", &cfg.studio_origin)
        .header("x-chelabase-studio", "1")
        .json(&body)
        .send()
        .await;
    let r = match answer {
        Err(e) => return Ok(Check::Wait(format!("no answer ({e})"))),
        Ok(r) if matches!(r.status().as_u16(), 502..=504) => {
            return Ok(Check::Wait(format!("it answers {}", r.status())));
        }
        Ok(r) => r,
    };
    let status = r.status();
    let body: Value = r.json().await.unwrap_or(Value::Null);
    let code = body["error"]["code"].as_str().unwrap_or("");
    match (status.as_u16(), body["next"].as_str(), code) {
        (200, Some("studio"), _) => Ok(Check::Ready(())),
        (200, Some("verify"), _) => bail!(
            "auth wants the dev member's email verified: this stack requires email verification \
             (unset CHELABASE_REQUIRE_EMAIL_VERIFICATION for the workbench)"
        ),
        (200, Some("pending"), _)
        | (403, _, "sign_up_closed" | "setup_code_invalid")
        | (409, _, _) => bail!(
            "the stack's tenant is already claimed ({}{}), so the dev member can't claim it: \
             sign in with the account that did, or start from a fresh stack (`just workbench purge`, \
             then `just workbench up`)",
            status.as_u16(),
            if code.is_empty() {
                String::new()
            } else {
                format!(" {code}")
            }
        ),
        (503, _, _) => Ok(Check::Wait(format!("auth answers 503 {code}"))),
        _ => bail!("auth answers {status} to POST {url}: {code}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passwords_pass_the_rule_and_differ() {
        let (a, b) = (new_password(), new_password());
        assert_ne!(a, b);
        for p in [&a, &b] {
            assert!(chelabase_identity::rules::strong_password(p), "{p}");
            assert!(
                p.len() <= chelabase_identity::rules::PASSWORD_MAX_BYTES,
                "{p}"
            );
        }
    }

    #[test]
    fn debug_hides_the_password() {
        let m = DevMember {
            email: "dev@x".into(),
            password: "Secret-1!".into(),
        };
        assert!(!format!("{m:?}").contains("Secret-1!"));
    }
}
