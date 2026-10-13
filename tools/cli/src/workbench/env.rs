//! A stack's one generated env file, `env/stack.env`: the example's keys with the stack's names,
//! ports, paths and service-name addresses laid over them (W2 spec sections 1 and 2).

use super::slug::hosts;
use crate::dev::{app_keys, internal_keys, secrets_key};
use std::path::Path;

pub struct StackEnvInput<'a> {
    pub slug: &'a str,
    pub project: &'a str,
    pub checkout: &'a Path,
    /// The main checkout (keys and state live under its `.chelabase`).
    pub main: &'a Path,
    /// The stack's state dir.
    pub state: &'a Path,
    pub gateway_port: u16,
    pub proxy_port: u16,
    pub uid: u32,
    pub gid: u32,
    pub docker_gid: u32,
    pub image: &'a str,
    /// Written last; a key already present is replaced in place.
    pub extra: &'a [(String, String)],
}

/// The app tier's addresses inside the stack: service-name URLs and `0.0.0.0` listen addresses.
const SERVICE_KEYS: &[(&str, &str)] = &[
    (
        "CHELABASE_METADATA_DATABASE_URL",
        "postgres://chelabase:chelabase@metadata-db:5432/chelabase_metadata",
    ),
    ("CHELABASE_NATS_URL", "nats://nats:4222"),
    ("CHELABASE_VALKEY_URL", "redis://valkey:6379"),
    ("CHELABASE_STORCON_URL", "http://storage-controller:1234"),
    ("CHELABASE_SAFEKEEPER_HTTP", "1=http://safekeeper1:7676"),
    ("CHELABASE_INTERNAL_URL", "http://orchestrator:8083"),
    ("CHELABASE_REST_INTERNAL_URL", "http://rest:8086"),
    ("CHELABASE_ORCHESTRATOR_URL", "http://orchestrator:8081"),
    ("CHELABASE_AUTH_URL", "http://auth:8084"),
    ("CHELABASE_REST_URL", "http://rest:8085"),
    ("CHELABASE_STUDIO_UPSTREAM", "http://studio:3000"),
    ("CHELABASE_ZITADEL_UPSTREAM", "http://zitadel:8080"),
    ("CHELABASE_MAIL_UPSTREAM", "http://mailpit:8025"),
    ("CHELABASE_DOCS_UPSTREAM", "http://docs:3001"),
    ("CHELABASE_LANDING_UPSTREAM", "http://landing:3002"),
    ("CHELABASE_ORCHESTRATOR_ADDR", "0.0.0.0:8081"),
    ("CHELABASE_WORKER_HEALTH_ADDR", "0.0.0.0:8082"),
    ("CHELABASE_INTERNAL_ADDR", "0.0.0.0:8083"),
    ("CHELABASE_AUTH_ADDR", "0.0.0.0:8084"),
    ("CHELABASE_REST_ADDR", "0.0.0.0:8085"),
    ("CHELABASE_REST_INTERNAL_ADDR", "0.0.0.0:8086"),
];

/// Keys in file order; setting a known key replaces its value in place.
#[derive(Default)]
struct Lines(Vec<(String, String)>);

impl Lines {
    fn set(&mut self, key: &str, value: impl Into<String>) {
        let value = value.into();
        match self.0.iter_mut().find(|(k, _)| k == key) {
            Some((_, v)) => *v = value,
            None => self.0.push((key.to_string(), value)),
        }
    }

    fn get(&self, key: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }
}

const SCHEME_KEY: &str = "CHELABASE_PUBLIC_SCHEME";

/// The scheme as the entry settings normalize it: lowercased, blanks removed, empty or missing
/// means https. Anything but `http` or `https` is `None`.
fn normalized_scheme(raw: Option<&str>) -> Option<&'static str> {
    let s: String = raw
        .unwrap_or_default()
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect::<String>()
        .to_lowercase();
    match s.as_str() {
        "" | "https" => Some("https"),
        "http" => Some("http"),
        _ => None,
    }
}

/// The example's `KEY=VALUE` lines in file order (a repeated key keeps its first place and its
/// last value).
fn parse_example(example: &str) -> Lines {
    let mut lines = Lines::default();
    for line in example.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            lines.set(key.trim(), value);
        }
    }
    lines
}

/// The example's `CHELABASE_PUBLIC_SCHEME`, normalized: `http` or `https` (blank or missing means
/// https). Any other value is refused, as the entry settings refuse it.
pub fn example_scheme(example: &str) -> anyhow::Result<&'static str> {
    let raw = parse_example(example).get(SCHEME_KEY).map(str::to_string);
    normalized_scheme(raw.as_deref()).ok_or_else(|| {
        anyhow::anyhow!(
            "{SCHEME_KEY}={:?} must be http or https",
            raw.unwrap_or_default()
        )
    })
}

fn path(p: &Path) -> String {
    p.display().to_string()
}

/// Renders `stack.env`: every `KEY=VALUE` line of `example` in its order (overlaid keys get the
/// stack's value), then the overlaid keys the example lacks in their pinned order, then `extra`.
/// Values are written literally. Callers check the scheme with [`example_scheme`] first; an
/// invalid one renders as https.
pub fn render_stack_env(example: &str, input: &StackEnvInput) -> String {
    let mut lines = parse_example(example);

    let scheme = normalized_scheme(lines.get(SCHEME_KEY)).unwrap_or("https");
    let secure = scheme == "https";
    let h = hosts(input.slug, input.gateway_port);
    let names = h.names();
    let keys_dir = input.main.join(".chelabase");
    let gw = input.gateway_port.to_string();

    lines.set("CHELABASE_STACK", input.slug);
    lines.set("CHELABASE_COMPOSE_PROJECT", input.project);
    lines.set("CHELABASE_STACK_DIR", path(input.state));
    lines.set("CHELABASE_CHECKOUT", path(input.checkout));
    lines.set("CHELABASE_MAIN_CHECKOUT", path(input.main));
    lines.set("CHELABASE_KEYS_DIR", path(&keys_dir));
    lines.set("CHELABASE_UID", input.uid.to_string());
    lines.set("CHELABASE_GID", input.gid.to_string());
    lines.set("CHELABASE_DOCKER_GID", input.docker_gid.to_string());
    lines.set("CHELABASE_WORKBENCH_IMAGE", input.image);
    lines.set("CHELABASE_GATEWAY_PORT", gw.as_str());
    lines.set("CHELABASE_PROXY_PORT", input.proxy_port.to_string());
    lines.set(SCHEME_KEY, scheme);
    let site_hosts = [
        ("STUDIO", &h.studio),
        ("API", &h.api),
        ("AUTH", &h.auth),
        ("MAIL", &h.mail),
        ("DOCS", &h.docs),
        ("LANDING", &h.landing),
    ];
    for (site, host) in site_hosts {
        lines.set(&format!("CHELABASE_{site}_HOST"), host.as_str());
    }
    lines.set("CHELABASE_GATEWAY_LISTEN", format!("0.0.0.0:{gw}"));
    // names() is in the order studio, api, auth, mail, docs, landing.
    let [studio, api, auth, mail, docs, landing] = &names[..] else {
        unreachable!("Hosts::names returns six names");
    };
    for (site, name) in [
        ("AUTH", auth),
        ("STUDIO", studio),
        ("API", api),
        ("MAIL", mail),
        ("DOCS", docs),
        ("LANDING", landing),
    ] {
        lines.set(&format!("CHELABASE_{site}_HOST_NAME"), name.as_str());
    }
    lines.set("CHELABASE_PUBLIC_PORT", gw.as_str());
    lines.set(
        "CHELABASE_PUBLIC_SECURE",
        if secure { "true" } else { "false" },
    );
    lines.set(
        "CHELABASE_ZITADEL_TLS_MODE",
        if secure { "external" } else { "disabled" },
    );
    for (site, host) in site_hosts {
        lines.set(
            &format!("CHELABASE_ENTRY_{site}_URL"),
            format!("{scheme}://{host}"),
        );
    }
    for (key, value) in SERVICE_KEYS {
        lines.set(key, *value);
    }
    lines.set("CHELABASE_AUTH_TRUSTED_PROXY", "true");
    lines.set("CHELABASE_CONTROL_PLANE_URI", "http://orchestrator:8083");
    lines.set(
        "CHELABASE_COMPUTE_NETWORK",
        format!("{}_default", input.project),
    );
    lines.set(
        "CHELABASE_ZITADEL_ACTIONS_ENDPOINT",
        "http://orchestrator:8083/internal/zitadel/events",
    );
    lines.set("CHELABASE_ZITADEL_DIR", path(&input.state.join("zitadel")));
    for (key, dir) in [
        ("CHELABASE_SECRETS_DIR", "secrets"),
        ("CHELABASE_AUTH_DIR", "auth"),
        ("CHELABASE_NEON_DIR", "neon"),
        ("CHELABASE_GATEWAY_DIR", "gateway"),
    ] {
        lines.set(key, path(&keys_dir.join(dir)));
    }
    lines.set("CHELABASE_COMPUTE_JWT_DIR", path(&keys_dir.join("neon")));
    // The machine keys every app reads at startup. The gateway starts before the first bootstrap
    // (bootstrap reaches Zitadel through it), so these can't wait for demo.env.
    let gateway_dir = keys_dir.join("gateway");
    let auth_dir = keys_dir.join("auth");
    for (key, file) in [
        (
            "CHELABASE_INTERNAL_SIGNING_KEY",
            gateway_dir.join(internal_keys::SIGNING_KEY_FILE),
        ),
        (
            "CHELABASE_INTERNAL_VERIFY_KEY",
            gateway_dir.join(internal_keys::VERIFY_KEY_FILE),
        ),
        (
            "CHELABASE_SECRETS_KEY",
            keys_dir.join("secrets").join(secrets_key::SECRETS_KEY_FILE),
        ),
        (
            "CHELABASE_AUTH_COOKIE_KEY",
            auth_dir.join(secrets_key::COOKIE_KEY_FILE),
        ),
        (
            "CHELABASE_APP_JWT_PRIVATE_KEY",
            auth_dir.join(app_keys::PRIVATE_FILE),
        ),
        (
            "CHELABASE_APP_JWT_PUBLIC_KEY",
            auth_dir.join(app_keys::PUBLIC_FILE),
        ),
    ] {
        lines.set(key, path(&file));
    }
    lines.set(
        "CHELABASE_ZITADEL_JWKS_URL",
        format!("{scheme}://{}/oauth/v2/keys", h.auth),
    );
    lines.set(
        "CHELABASE_PROXY_CA_FILE",
        path(&input.state.join("neon").join("proxy-ca.crt")),
    );
    for key in [
        "CHELABASE_STUDIO_BIND",
        "CHELABASE_DOCS_BIND",
        "CHELABASE_LANDING_BIND",
    ] {
        lines.set(key, "0.0.0.0");
    }
    lines.set(
        "CHELABASE_CLI",
        path(
            &input
                .checkout
                .join("target")
                .join("debug")
                .join("chelabase"),
        ),
    );

    for (key, value) in input.extra {
        lines.set(key, value.as_str());
    }

    let mut out = format!(
        "# Generated by chelabase workbench for {}; do not edit.\n",
        input.slug
    );
    for (key, value) in &lines.0 {
        out.push_str(key);
        out.push('=');
        out.push_str(value);
        out.push('\n');
    }
    out
}
