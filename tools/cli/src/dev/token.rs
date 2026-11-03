//! `chelabase dev token`: a member access token of the workbench's dev member (#126) on stdout.
//!
//! The dev member is the one `chelabase-init bootstrap --demo` signed up (its email and password
//! are in the stack's bootstrap dir, `dev-member.json`). The token comes the way Studio gets one:
//! `POST /auth/v1/sign-in`, then `POST /auth/v1/token` with the session cookie, both on the API
//! host as Studio sends them. The session cookie and the last token are kept beside the password
//! (`dev-token.json`, 0600), so most calls cost no sign-in (auth allows ten a minute per address):
//! a kept token with more than a minute left is printed as is, else the kept session asks for a
//! new one, and only an ended session signs in again.

use crate::paths::{bootstrap_dir, entry};
use anyhow::{Context, Result, bail};
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// The bootstrap's file: the dev member's email and password.
pub const DEV_MEMBER: &str = "dev-member.json";
/// What this command keeps between calls.
pub const DEV_TOKEN: &str = "dev-token.json";
/// auth's session cookie.
const SESSION_COOKIE: &str = "chelabase_session";
/// A kept token is reused while it has more than this left (seconds).
const MIN_LEFT: u64 = 60;

#[derive(Deserialize)]
struct DevMember {
    email: String,
    password: String,
}

#[derive(Default, Serialize, Deserialize)]
struct Kept {
    /// The session cookie's value.
    session: Option<String>,
    access_token: Option<String>,
}

#[derive(Deserialize)]
struct TokenAnswer {
    access_token: String,
}

pub async fn run() -> Result<()> {
    let token = token(&bootstrap_dir()).await?;
    // Token only on stdout so shell capture stays clean; logs go to stderr via tracing.
    print!("{token}");
    Ok(())
}

/// A fresh token for the dev member whose files are in `dir`.
pub async fn token(dir: &Path) -> Result<String> {
    let member_file = dir.join(DEV_MEMBER);
    let member: DevMember = serde_json::from_slice(
        &std::fs::read(&member_file)
            .with_context(|| format!("missing {}: run `just workbench up`", member_file.display()))?,
    )
    .with_context(|| format!("parse {}", member_file.display()))?;
    let kept_file = dir.join(DEV_TOKEN);
    let mut kept: Kept = std::fs::read(&kept_file)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    if let Some(t) = &kept.access_token
        && seconds_left(t, now()) > MIN_LEFT
    {
        return Ok(t.clone());
    }
    let entry = entry()?;
    let site = Site { api: entry.api_base(), studio: entry.studio_origin() };
    let http = chelabase_http::client_builder().build().context("an HTTP client")?;
    if let Some(session) = kept.session.clone()
        && let Some(t) = site.token(&http, &session).await?
    {
        kept.access_token = Some(t.clone());
        save(&kept_file, &kept)?;
        return Ok(t);
    }
    let session = site.sign_in(&http, &member).await?;
    kept.session = Some(session.clone());
    let t = site
        .token(&http, &session)
        .await?
        .context("auth refused a token for the session it just made")?;
    kept.access_token = Some(t.clone());
    save(&kept_file, &kept)?;
    Ok(t)
}

struct Site {
    api: String,
    studio: String,
}

impl Site {
    fn post(&self, http: &chelabase_http::Client, path: &str) -> reqwest::RequestBuilder {
        http.post(format!("{}{path}", self.api))
            .header("origin", &self.studio)
            .header("x-chelabase-studio", "1")
    }

    /// The session cookie's value after a sign-in with the dev member's password.
    async fn sign_in(&self, http: &chelabase_http::Client, m: &DevMember) -> Result<String> {
        let resp = self
            .post(http, "/auth/v1/sign-in")
            .json(&serde_json::json!({ "email": m.email, "password": m.password }))
            .send()
            .await
            .context("POST /auth/v1/sign-in")?;
        let status = resp.status();
        let session = session_cookie(resp.headers().get_all(reqwest::header::SET_COOKIE).iter());
        let body: serde_json::Value = resp.json().await.unwrap_or_default();
        match (status.as_u16(), body["next"].as_str(), session) {
            (200, Some("studio"), Some(session)) => Ok(session),
            (429, ..) => bail!("auth rate-limited the dev member's sign-in: try again in a minute"),
            _ => bail!(
                "the dev member's sign-in answered {status} {body}: rerun `just workbench up`, or \
                 start from a fresh stack (`just workbench purge`, then `just workbench up`)"
            ),
        }
    }

    /// A token for the session, or `None` once the session has ended (401).
    async fn token(&self, http: &chelabase_http::Client, session: &str) -> Result<Option<String>> {
        let resp = self
            .post(http, "/auth/v1/token")
            .header(reqwest::header::COOKIE, format!("{SESSION_COOKIE}={session}"))
            .send()
            .await
            .context("POST /auth/v1/token")?;
        match resp.status().as_u16() {
            200 => Ok(Some(resp.json::<TokenAnswer>().await.context("parse the token answer")?.access_token)),
            401 => Ok(None),
            _ => {
                let status = resp.status();
                bail!("POST /auth/v1/token answered {status}: {}", resp.text().await.unwrap_or_default())
            }
        }
    }
}

/// The session cookie's value among `Set-Cookie` headers.
fn session_cookie<'a>(headers: impl Iterator<Item = &'a reqwest::header::HeaderValue>) -> Option<String> {
    headers.filter_map(|h| h.to_str().ok()).find_map(|h| {
        let (pair, _) = h.split_once(';').unwrap_or((h, ""));
        let (name, value) = pair.split_once('=')?;
        (name.trim() == SESSION_COOKIE && !value.is_empty()).then(|| value.trim().to_string())
    })
}

/// Seconds until the JWT's `exp` (0 when it has passed or can't be read). Unverified: it only
/// decides whether to ask for a new one.
fn seconds_left(jwt: &str, now: u64) -> u64 {
    let exp = jwt
        .split('.')
        .nth(1)
        .and_then(|p| base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(p).ok())
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
        .and_then(|v| v["exp"].as_u64())
        .unwrap_or(0);
    exp.saturating_sub(now)
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Writes `kept` owner-only: a temporary file made `O_EXCL` at 0600, then renamed into place.
fn save(path: &Path, kept: &Kept) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let tmp: PathBuf = path.with_extension(format!("tmp-{}", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)
        .with_context(|| format!("creating {}", tmp.display()))?;
    file.write_all(&serde_json::to_vec(kept)?)?;
    std::fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jwt(exp: u64) -> String {
        let enc = |v: serde_json::Value| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(v.to_string());
        format!("{}.{}.sig", enc(serde_json::json!({"alg": "EdDSA"})), enc(serde_json::json!({"exp": exp})))
    }

    #[test]
    fn a_kept_token_is_reused_only_with_a_minute_left() {
        assert_eq!(seconds_left(&jwt(1_300), 1_000), 300);
        assert_eq!(seconds_left(&jwt(900), 1_000), 0);
        assert_eq!(seconds_left("garbage", 1_000), 0);
    }

    #[test]
    fn the_session_cookie_is_found_among_set_cookies() {
        let values = [
            reqwest::header::HeaderValue::from_static("chelabase_login=; Max-Age=0; Path=/auth/v1"),
            reqwest::header::HeaderValue::from_static("chelabase_session=abc.def; HttpOnly; Path=/auth/v1"),
        ];
        assert_eq!(session_cookie(values.iter()).as_deref(), Some("abc.def"));
        assert_eq!(session_cookie(values[..1].iter()), None);
    }

    #[test]
    fn kept_files_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let t = tempfile::tempdir().unwrap();
        let path = t.path().join(DEV_TOKEN);
        save(&path, &Kept { session: Some("s".into()), access_token: None }).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    }
}
