//! Stack identity: a checkout's slug and the names derived from it (W2 spec section 1).

use sha2::{Digest, Sha256};

/// The main checkout's slug (reserved: a worktree directory named `main` gets `main-wt`).
pub const MAIN: &str = "main";

/// Longest slug; a longer one is cut to `CUT` characters and gets `-` plus `HASH_LEN` hex chars.
const MAX_LEN: usize = 40;
const CUT: usize = 33;
const HASH_LEN: usize = 6;

/// The slug of a worktree whose `.worktrees/` directory is `dir_name`: lowercase, `[a-z0-9-]` only,
/// no runs of `-`, no `-` at either end, at most 40 characters.
pub fn slug_for_dir(dir_name: &str) -> anyhow::Result<String> {
    let mut slug = String::with_capacity(dir_name.len());
    for c in dir_name.chars().flat_map(char::to_lowercase) {
        let c = if c.is_ascii_lowercase() || c.is_ascii_digit() {
            c
        } else {
            '-'
        };
        if c == '-' && (slug.is_empty() || slug.ends_with('-')) {
            continue;
        }
        slug.push(c);
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    if slug.is_empty() {
        anyhow::bail!(
            "worktree directory {dir_name:?} has no character usable in a stack name ([a-z0-9-])"
        );
    }
    if slug == MAIN {
        return Ok("main-wt".to_string());
    }
    if slug.len() > MAX_LEN {
        // The slug is ASCII, so byte indices are character indices.
        let mut cut = slug[..CUT].trim_end_matches('-').to_string();
        let hash = hex::encode(Sha256::digest(dir_name.as_bytes()));
        cut.push('-');
        cut.push_str(&hash[..HASH_LEN]);
        slug = cut;
    }
    Ok(slug)
}

/// The Compose project of a stack: `chelabase` for main (today's volumes survive), else
/// `chelabase-<slug>`.
pub fn project_name(slug: &str) -> String {
    if slug == MAIN {
        "chelabase".to_string()
    } else {
        format!("chelabase-{slug}")
    }
}

/// A stack's public hosts, each `name:port`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hosts {
    pub studio: String,
    pub api: String,
    pub auth: String,
    pub mail: String,
    pub docs: String,
    pub landing: String,
}

impl Hosts {
    /// The host names without their port, in the order studio, api, auth, mail, docs, landing.
    pub fn names(&self) -> Vec<String> {
        [
            &self.studio,
            &self.api,
            &self.auth,
            &self.mail,
            &self.docs,
            &self.landing,
        ]
        .into_iter()
        .map(|h| {
            h.rsplit_once(':')
                .map_or(h.as_str(), |(n, _)| n)
                .to_string()
        })
        .collect()
    }
}

/// The public hosts of stack `slug` behind its gateway on `gateway_port`.
pub fn hosts(slug: &str, gateway_port: u16) -> Hosts {
    let domain = if slug == MAIN {
        "chelabase.localhost".to_string()
    } else {
        format!("{slug}.chelabase.localhost")
    };
    let host = |name: &str| format!("{name}.{domain}:{gateway_port}");
    Hosts {
        studio: host("studio"),
        api: host("api"),
        auth: host("auth"),
        mail: host("mail"),
        docs: host("docs"),
        landing: host("landing"),
    }
}
