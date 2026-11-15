//! `chelabase login --url <api>`: an email/password sign-in on an instance, kept per instance URL
//! in `credentials.json` (0600) under `$CHELABASE_CONFIG_DIR`, else `$XDG_CONFIG_HOME/chelabase`,
//! else `~/.config/chelabase`. The flow is `dev token`'s: `POST /auth/v1/sign-in` (as Studio,
//! whose origin `GET /auth/v1/studio-config` names), then `POST /auth/v1/token` with the session.
//! Commands reuse a kept token with more than a minute left, else ask the session for a new one;
//! an ended session asks for a new login. A token is never printed.

use crate::dev::token::{Site, save, seconds_left, session_cookie};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};

pub const CREDENTIALS: &str = "credentials.json";

/// One instance's sign-in.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Login {
    pub email: String,
    /// Studio's origin, which auth checks on sign-in and token calls.
    pub studio_origin: String,
    pub session: String,
    pub access_token: Option<String>,
}

/// Every instance's sign-in, keyed by the API URL.
pub type Credentials = BTreeMap<String, Login>;

pub fn config_dir() -> PathBuf {
    let var = |n| {
        std::env::var_os(n)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    };
    var("CHELABASE_CONFIG_DIR")
        .or_else(|| var("XDG_CONFIG_HOME").map(|d| d.join("chelabase")))
        .or_else(|| var("HOME").map(|h| h.join(".config/chelabase")))
        .unwrap_or_else(|| PathBuf::from(".chelabase-config"))
}

/// An instance URL as credentials are keyed: no trailing slash.
pub fn normalize(url: &str) -> String {
    url.trim().trim_end_matches('/').to_string()
}

pub fn load(dir: &Path) -> Result<Credentials> {
    match std::fs::read(dir.join(CREDENTIALS)) {
        Ok(b) => serde_json::from_slice(&b)
            .with_context(|| format!("parsing {}", dir.join(CREDENTIALS).display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Credentials::new()),
        Err(e) => Err(e).with_context(|| format!("reading {}", dir.join(CREDENTIALS).display())),
    }
}

/// Writes the credentials owner-only (the directory 0700, the file 0600).
pub fn store(dir: &Path, creds: &Credentials) -> Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .with_context(|| format!("creating {}", dir.display()))?;
    let file = dir.join(CREDENTIALS);
    save(&file, creds)?;
    // `save` creates 0600, but umask or an older file never widens it.
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

/// One line of input: from the terminal (not echoed when `secret`) or stdin.
pub fn prompt(label: &str, secret: bool, input: &mut dyn BufRead) -> Result<String> {
    let tty = std::io::stdin().is_terminal();
    if tty {
        eprint!("{label}: ");
        std::io::stderr().flush()?;
    }
    let echo_off = tty && secret && stty("-echo");
    let mut line = String::new();
    let read = input.read_line(&mut line);
    if echo_off {
        stty("echo");
        eprintln!();
    }
    read.context("reading stdin")?;
    let line = line.strip_suffix('\n').unwrap_or(&line);
    Ok(line.strip_suffix('\r').unwrap_or(line).to_string())
}

fn stty(arg: &str) -> bool {
    std::process::Command::new("stty")
        .arg(arg)
        .stdin(std::process::Stdio::inherit())
        .status()
        .is_ok_and(|s| s.success())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct StudioConfig {
    studio_host: String,
}

pub async fn run(url: &str, email: Option<String>) -> Result<()> {
    let url = normalize(url);
    let mut stdin = std::io::stdin().lock();
    let email = match email {
        Some(e) => e,
        None => prompt("Email", false, &mut stdin)?,
    };
    let password = prompt("Password", true, &mut stdin)?;
    let http = chelabase_http::client_builder()
        .build()
        .context("an HTTP client")?;
    let config: StudioConfig = http
        .get(format!("{url}/auth/v1/studio-config"))
        .send()
        .await
        .with_context(|| format!("reaching {url}"))?
        .error_for_status()
        .with_context(|| format!("{url} doesn't look like a Chelabase API URL"))?
        .json()
        .await
        .context("parsing /auth/v1/studio-config")?;
    let scheme = url.split_once("://").map(|(s, _)| s).unwrap_or("https");
    let site = Site {
        api: url.clone(),
        studio: format!("{scheme}://{}", config.studio_host),
    };
    let resp = site
        .post(&http, "/auth/v1/sign-in")
        .json(&serde_json::json!({ "email": email, "password": password }))
        .send()
        .await
        .context("POST /auth/v1/sign-in")?;
    let status = resp.status().as_u16();
    let session = session_cookie(resp.headers().get_all(reqwest::header::SET_COOKIE).iter());
    let body: serde_json::Value = resp.json().await.unwrap_or_default();
    let session = match (status, body["next"].as_str(), session) {
        (200, Some("studio"), Some(s)) => s,
        (200, Some("verify"), _) => {
            bail!("this account signs in with a code step, which the CLI doesn't support yet")
        }
        (200, Some("pending"), _) => bail!("this account is waiting for an admin's approval"),
        (429, ..) => bail!("auth rate-limited the sign-in: try again in a minute"),
        (401 | 403, ..) => bail!("the email or password is wrong"),
        _ => bail!("the sign-in answered {status} {body}"),
    };
    let token = site.token(&http, &session).await?;
    let dir = config_dir();
    let mut creds = load(&dir)?;
    creds.insert(
        url.clone(),
        Login {
            email,
            studio_origin: site.studio,
            session,
            access_token: token,
        },
    );
    store(&dir, &creds)?;
    eprintln!("Signed in to {url}.");
    Ok(())
}

/// The instance a command talks to: `explicit` (--url, CHELABASE_URL), else the project's, else
/// the only instance signed in to.
pub fn instance(explicit: Option<&str>, project: Option<&str>) -> Result<String> {
    if let Some(u) = explicit.or(project) {
        return Ok(normalize(u));
    }
    let creds = load(&config_dir())?;
    let mut urls = creds.keys();
    match (urls.next(), urls.next()) {
        (Some(u), None) => Ok(u.clone()),
        (None, _) => bail!("not signed in: run `chelabase login --url <api url>`"),
        _ => bail!(
            "signed in to several instances: pass --url or set [project] url in chelabase.toml"
        ),
    }
}

/// A member access token for `url`, from the kept sign-in.
pub async fn bearer(http: &chelabase_http::Client, url: &str) -> Result<String> {
    let dir = config_dir();
    let mut creds = load(&dir)?;
    let relogin = || format!("run `chelabase login --url {url}`");
    let Some(login) = creds.get_mut(url) else {
        bail!("not signed in to {url}: {}", relogin());
    };
    if let Some(t) = &login.access_token
        && seconds_left(t, now()) > 60
    {
        return Ok(t.clone());
    }
    let site = Site {
        api: url.to_string(),
        studio: login.studio_origin.clone(),
    };
    let Some(token) = site.token(http, &login.session).await? else {
        bail!("the session for {url} has ended: {}", relogin());
    };
    login.access_token = Some(token.clone());
    store(&dir, &creds)?;
    Ok(token)
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn credentials_are_owner_only_and_keyed_by_instance() {
        let t = tempfile::tempdir().unwrap();
        let dir = t.path().join("chelabase");
        let mut creds = Credentials::new();
        creds.insert(
            normalize("http://api.localhost:8000/"),
            Login {
                session: "s".into(),
                ..Default::default()
            },
        );
        store(&dir, &creds).unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir.join(CREDENTIALS)), 0o600);
        assert_eq!(mode(&dir), 0o700);
        // An existing wider file is narrowed on the next write.
        std::fs::set_permissions(
            dir.join(CREDENTIALS),
            std::fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        store(&dir, &creds).unwrap();
        assert_eq!(mode(&dir.join(CREDENTIALS)), 0o600);
        let back = load(&dir).unwrap();
        assert_eq!(back["http://api.localhost:8000"].session, "s");
        assert!(load(&t.path().join("none")).unwrap().is_empty());
    }

    #[test]
    fn a_prompt_reads_one_line_from_stdin() {
        let mut input: &[u8] = b"s3cret value\r\nnext\n";
        assert_eq!(prompt("Value", true, &mut input).unwrap(), "s3cret value");
        assert_eq!(prompt("Value", true, &mut input).unwrap(), "next");
        assert_eq!(prompt("Value", true, &mut input).unwrap(), "");
    }
}
