//! The local-only gate: the seed claims an instance with a well-known password, so it runs only
//! on a stack nobody else can reach. Checked before any request.
//!
//! - The Studio, API and auth hosts, without their ports, are `localhost` or `*.localhost`.
//! - `CHELABASE_HTTP_ADDR` and `CHELABASE_PROXY_ADDR` (where the host publishes the gateway and
//!   the Postgres proxy) are loopback addresses.
//! - `CHELABASE_REQUIRE_EMAIL_VERIFICATION` is blank or false: the seed has no inbox.
//! - The gateway URL is plain http to a loopback address or a single-label name (a service on
//!   the compose network), so the setup code goes nowhere else.

use super::Config;
use anyhow::{Result, bail};
use chelabase_types::entry::normalize_host;
use std::net::IpAddr;

/// `Ok` only for a local stack; the error names the setting.
pub fn check(cfg: &Config) -> Result<()> {
    for (name, host) in [
        ("CHELABASE_STUDIO_HOST", cfg.entry.studio_host()),
        ("CHELABASE_API_HOST", cfg.entry.api_host()),
        ("CHELABASE_AUTH_HOST", cfg.entry.auth_host()),
    ] {
        let bare = normalize_host(host);
        if bare != "localhost" && !bare.ends_with(".localhost") {
            bail!(
                "{name}={host} is not a localhost name: the demo seed runs only on a local stack"
            );
        }
    }
    for (name, addr) in [
        ("CHELABASE_HTTP_ADDR", &cfg.http_addr),
        ("CHELABASE_PROXY_ADDR", &cfg.proxy_addr),
    ] {
        let addr = addr.as_deref().unwrap_or("").trim();
        let ip = addr
            .strip_prefix('[')
            .and_then(|a| a.strip_suffix(']'))
            .unwrap_or(addr);
        if !ip.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback()) {
            bail!(
                "{name}={addr:?} is not a loopback address: the demo seed runs only on a local stack"
            );
        }
    }
    let verification = cfg
        .require_email_verification
        .as_deref()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if !matches!(verification.as_str(), "" | "0" | "false" | "no" | "off") {
        bail!("CHELABASE_REQUIRE_EMAIL_VERIFICATION is on: the demo seed can't verify emails");
    }
    let gateway_ok = reqwest::Url::parse(&cfg.gateway).is_ok_and(|u| {
        u.scheme() == "http"
            && u.username().is_empty()
            && u.password().is_none()
            && u.host_str().is_some_and(|h| {
                let bare = h.trim_start_matches('[').trim_end_matches(']');
                match bare.parse::<IpAddr>() {
                    Ok(ip) => ip.is_loopback(),
                    Err(_) => !h.is_empty() && !h.contains('.'),
                }
            })
    });
    if !gateway_ok {
        bail!(
            "CHELABASE_EXAMPLE_GATEWAY_URL must be http:// to a compose service or a loopback address"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::example::fake;

    fn with_all(set: &[(&'static str, &'static str)]) -> Result<()> {
        let cfg = Config::from_lookup(|name| match set.iter().find(|(k, _)| *k == name) {
            Some((_, v)) => Some(v.to_string()),
            None => fake::env(name),
        })?;
        check(&cfg)
    }

    fn with(k: &'static str, v: &'static str) -> Result<()> {
        with_all(&[(k, v)])
    }

    #[test]
    fn a_local_stack_passes() {
        with("CHELABASE_HTTP_ADDR", "127.0.0.1").unwrap();
        with("CHELABASE_HTTP_ADDR", "::1").unwrap();
        with("CHELABASE_PROXY_ADDR", "[::1]").unwrap();
        with_all(&[
            ("CHELABASE_STUDIO_HOST", "studio.acme.localhost:9443"),
            ("CHELABASE_API_HOST", "api.acme.localhost:9443"),
            ("CHELABASE_AUTH_HOST", "localhost:9443"),
        ])
        .unwrap();
        with("CHELABASE_REQUIRE_EMAIL_VERIFICATION", "false").unwrap();
        with("CHELABASE_REQUIRE_EMAIL_VERIFICATION", "").unwrap();
        with("CHELABASE_EXAMPLE_GATEWAY_URL", "http://127.0.0.1:8000").unwrap();
    }

    #[test]
    fn hostile_hosts_are_refused() {
        let cases: [&[(&'static str, &'static str)]; 4] = [
            &[
                ("CHELABASE_STUDIO_HOST", "studio.localhost.evil.com"),
                ("CHELABASE_API_HOST", "api.localhost.evil.com"),
            ],
            &[("CHELABASE_AUTH_HOST", "localhost.evil.com:8000")],
            &[("CHELABASE_AUTH_HOST", "127.0.0.1.nip.io")],
            &[("CHELABASE_AUTH_HOST", "127.0.0.1:8000")],
        ];
        for set in cases {
            let e = with_all(set).unwrap_err().to_string();
            assert!(e.contains("is not a localhost name"), "{set:?}: {e}");
        }
    }

    #[test]
    fn non_loopback_binds_are_refused() {
        for (k, v) in [
            ("CHELABASE_HTTP_ADDR", "0.0.0.0"),
            ("CHELABASE_HTTP_ADDR", "192.168.1.5"),
            ("CHELABASE_HTTP_ADDR", "localhost"),
            ("CHELABASE_HTTP_ADDR", ""),
            ("CHELABASE_PROXY_ADDR", "::"),
        ] {
            let e = with(k, v).unwrap_err();
            assert!(e.to_string().contains(k), "{k}={v}: {e}");
        }
    }

    #[test]
    fn required_email_verification_is_refused() {
        for v in ["true", "1", "yes", "ON", "maybe"] {
            let e = with("CHELABASE_REQUIRE_EMAIL_VERIFICATION", v).unwrap_err();
            assert!(
                e.to_string()
                    .contains("CHELABASE_REQUIRE_EMAIL_VERIFICATION"),
                "{v}: {e}"
            );
        }
    }

    #[test]
    fn the_gateway_must_be_a_service_on_this_network() {
        for v in [
            "http://gateway.evil.com:8000",
            "https://gateway:8000",
            "http://user@gateway:8000",
            "http://10.0.0.1:8000",
        ] {
            let e = with("CHELABASE_EXAMPLE_GATEWAY_URL", v).unwrap_err();
            assert!(
                e.to_string().contains("CHELABASE_EXAMPLE_GATEWAY_URL"),
                "{v}: {e}"
            );
        }
    }
}
