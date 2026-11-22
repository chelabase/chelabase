//! The service's settings, from the environment.

use std::net::SocketAddr;

use anyhow::Context;
use chelabase_auth::member_token::MemberTokenVerifier;
use chelabase_types::entry::Entry;

use crate::app::Settings;

pub struct Config {
    /// `CHELABASE_MCP_LISTEN` (default `127.0.0.1:8095`).
    pub listen: SocketAddr,
    pub settings: Settings,
    /// `CHELABASE_MEMBER_JWT_PUBLIC_KEY` with the issuer from `CHELABASE_AUTH_HOST`. Required:
    /// without it no request could be authenticated.
    pub verifier: MemberTokenVerifier,
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    /// Blank values count as unset.
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> anyhow::Result<Self> {
        let non_blank = |name: &str| {
            get(name)
                .map(|v| v.trim().to_owned())
                .filter(|v| !v.is_empty())
        };
        let listen = non_blank("CHELABASE_MCP_LISTEN")
            .unwrap_or_else(|| "127.0.0.1:8095".into())
            .parse()
            .context("CHELABASE_MCP_LISTEN must be an ip:port address")?;
        // The same entry the gateway reads, so the challenge it sends and this one agree.
        let entry = Entry::from_lookup(&get).map_err(|e| anyhow::anyhow!(e.0))?;
        let verifier = MemberTokenVerifier::from_lookup(&get)
            .map_err(|e| anyhow::anyhow!("{e}"))?
            .context("CHELABASE_MEMBER_JWT_PUBLIC_KEY is required: the MCP endpoint takes member tokens only")?;
        let allowed_origins = non_blank("CHELABASE_MCP_ALLOWED_ORIGINS")
            .map(|v| {
                v.split(',')
                    .map(|o| o.trim().to_owned())
                    .filter(|o| !o.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        let log_code = match non_blank("CHELABASE_MCP_LOG_CODE").as_deref() {
            None | Some("0") => false,
            Some("1") => true,
            Some(other) => anyhow::bail!("CHELABASE_MCP_LOG_CODE must be 0 or 1, not {other:?}"),
        };
        Ok(Self {
            listen,
            settings: Settings {
                api_base: entry.api_base(),
                allowed_origins,
                log_code,
            },
            verifier,
        })
    }
}
