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
    /// The stack's control-network slot (`Entry::control_slot`).
    pub control_slot: u8,
}

/// One stack's control network: the subnet Compose creates, the range computes get addresses
/// from, and the static addresses of the orchestrator, the test runner and realtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControlNet {
    pub subnet: String,
    pub compute_range: String,
    pub orchestrator_ip: String,
    pub runner_ip: String,
    pub realtime_ip: String,
}

/// Slot `k` (0..=63): subnet `10.213.{4k}.0/22`, computes from the upper `/23`
/// (`10.213.{4k+2}.0/23`), the orchestrator at `.10`, the runner at `.11` and realtime at `.12` of the first `/24`.
pub fn control_net(slot: u8) -> ControlNet {
    let base = u16::from(slot) * 4;
    ControlNet {
        subnet: format!("10.213.{base}.0/22"),
        compute_range: format!("10.213.{}.0/23", base + 2),
        orchestrator_ip: format!("10.213.{base}.10"),
        runner_ip: format!("10.213.{base}.11"),
        realtime_ip: format!("10.213.{base}.12"),
    }
}

/// One Docker network and the subnets of its `IPAM.Config`, as `docker network inspect` says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkSubnets {
    pub name: String,
    pub subnets: Vec<String>,
}

/// Parses the output of `docker network inspect <all networks>` (a JSON array). `IPAM.Config`
/// may be null or missing, and an entry may lack `Subnet`: those add no subnet.
pub fn parse_network_subnets(json: &str) -> anyhow::Result<Vec<NetworkSubnets>> {
    let value: serde_json::Value =
        serde_json::from_str(json).map_err(|e| anyhow::anyhow!("docker network inspect: {e}"))?;
    let networks = value
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("docker network inspect: expected a JSON array"))?;
    Ok(networks
        .iter()
        .map(|n| NetworkSubnets {
            name: n["Name"].as_str().unwrap_or_default().to_string(),
            subnets: n["IPAM"]["Config"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|c| c["Subnet"].as_str().map(str::to_string))
                .collect(),
        })
        .collect())
}

fn parse_cidr_v4(cidr: &str) -> Option<(u32, u32)> {
    let (addr, prefix) = cidr.split_once('/')?;
    let addr: std::net::Ipv4Addr = addr.parse().ok()?;
    let prefix: u32 = prefix.parse().ok().filter(|p| *p <= 32)?;
    Some((u32::from(addr), prefix))
}

/// Whether two IPv4 CIDRs share an address. Host bits are ignored; IPv6 and unparsable input
/// never overlap.
pub fn cidr_overlaps(a: &str, b: &str) -> bool {
    let (Some((a, pa)), Some((b, pb))) = (parse_cidr_v4(a), parse_cidr_v4(b)) else {
        return false;
    };
    let prefix = pa.min(pb);
    let mask = if prefix == 0 { 0 } else { u32::MAX << (32 - prefix) };
    (a ^ b) & mask == 0
}

/// Refuses a start when a network other than the stack's own (`own_network`) holds an IPv4 subnet
/// overlapping the slot's control subnet: Docker would fail the stack's network with "Pool
/// overlaps" after the images and builds, and the message would not name the slot.
pub fn check_control_subnet(
    slug: &str,
    slot: u8,
    own_network: &str,
    networks: &[NetworkSubnets],
) -> anyhow::Result<()> {
    let wanted = control_net(slot).subnet;
    for net in networks.iter().filter(|n| n.name != own_network) {
        if let Some(subnet) = net.subnets.iter().find(|s| cidr_overlaps(s, &wanted)) {
            anyhow::bail!(
                "stack {slug} (control slot {slot}) needs the subnet {wanted}, but the Docker network {} holds {subnet}; remove that network (`docker network rm {}`) or free the slot",
                net.name,
                net.name
            );
        }
    }
    Ok(())
}

/// `docker network inspect <id>` for one network: a successful run is parsed; a network removed
/// since `docker network ls` ("No such network", "network … not found") yields none; any other
/// failure is an error naming the network.
pub fn parse_network_inspect(
    id: &str,
    ok: bool,
    stdout: &str,
    stderr: &str,
) -> anyhow::Result<Vec<NetworkSubnets>> {
    if ok {
        return parse_network_subnets(stdout);
    }
    let lower = stderr.to_lowercase();
    if lower.contains("no such network") || lower.contains("not found") {
        return Ok(Vec::new());
    }
    anyhow::bail!("docker network inspect {id} failed: {}", stderr.trim())
}

/// The path to mask in the containers besides the keys dir: `<checkout>/.chelabase` when it exists
/// and is not the keys dir (a worktree's symlink to it resolves there, and the keys dir is masked
/// anyway). `None` when it is the keys dir, or absent.
pub fn checkout_mask(checkout: &Path, keys_dir: &Path) -> Option<std::path::PathBuf> {
    let link = checkout.join(".chelabase");
    let resolved = link.canonicalize().ok()?;
    let keys = keys_dir.canonicalize().unwrap_or_else(|_| keys_dir.to_path_buf());
    (resolved != keys && link != keys_dir).then_some(link)
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
    // Three safekeepers: the storage controller runs without --dev (RT1 T1, user).
    (
        "CHELABASE_SAFEKEEPER_HTTP",
        "1=http://safekeeper1:7676,2=http://safekeeper2:7676,3=http://safekeeper3:7676",
    ),
    ("CHELABASE_INTERNAL_URL", "http://orchestrator:8083"),
    ("CHELABASE_REST_INTERNAL_URL", "http://rest:8086"),
    ("CHELABASE_ORCHESTRATOR_URL", "http://orchestrator:8081"),
    ("CHELABASE_AUTH_URL", "http://auth:8084"),
    ("CHELABASE_REST_URL", "http://rest:8085"),
    // RT1 T7: the gateway's realtime upstream (the ticket and socket routes).
    ("CHELABASE_REALTIME_URL", "http://realtime:8087"),
    // m5: the gateway's storage upstream.
    ("CHELABASE_STORAGE_URL", "http://storage:8089"),
    // m9: the orchestrator's way to storage's console listener (Studio's storage routes).
    ("CHELABASE_STORAGE_INTERNAL_URL", "http://storage:8090"),
    ("CHELABASE_STUDIO_UPSTREAM", "http://studio:3000"),
    ("CHELABASE_MAIL_UPSTREAM", "http://mailpit:8025"),
    ("CHELABASE_DOCS_UPSTREAM", "http://docs:3001"),
    ("CHELABASE_LANDING_UPSTREAM", "http://landing:3002"),
    ("CHELABASE_ORCHESTRATOR_ADDR", "0.0.0.0:8081"),
    ("CHELABASE_WORKER_HEALTH_ADDR", "0.0.0.0:8082"),
    ("CHELABASE_INTERNAL_ADDR", "0.0.0.0:8083"),
    ("CHELABASE_AUTH_ADDR", "0.0.0.0:8084"),
    ("CHELABASE_REST_ADDR", "0.0.0.0:8085"),
    ("CHELABASE_REST_INTERNAL_ADDR", "0.0.0.0:8086"),
    ("CHELABASE_REALTIME_ADDR", "0.0.0.0:8087"),
    ("CHELABASE_REALTIME_INTERNAL_ADDR", "0.0.0.0:8088"),
    ("CHELABASE_STORAGE_ADDR", "0.0.0.0:8089"),
    ("CHELABASE_STORAGE_INTERNAL_ADDR", "0.0.0.0:8090"),
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

/// The Docker network a stack's computes run on (`CHELABASE_COMPUTE_NETWORK`, P4).
pub fn compute_network(project: &str) -> String {
    format!("{project}_compute")
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
    for (site, host) in site_hosts {
        lines.set(
            &format!("CHELABASE_ENTRY_{site}_URL"),
            format!("{scheme}://{host}"),
        );
    }
    for (key, value) in SERVICE_KEYS {
        lines.set(key, *value);
    }
    // Ruling (e): only the gateway's X-Forwarded-For counts (computes reach auth directly).
    lines.set("CHELABASE_AUTH_TRUSTED_PROXY", "gateway");
    lines.set("CHELABASE_CONTROL_PLANE_URI", "http://orchestrator:8083");
    // P4/W14: computes get the stack's own network, without NATS, Valkey or any database.
    lines.set("CHELABASE_COMPUTE_NETWORK", compute_network(input.project));
    let control = control_net(input.control_slot);
    lines.set(
        "CHELABASE_COMPUTE_CONTROL_NETWORK",
        format!("{}_control", input.project),
    );
    lines.set("CHELABASE_CONTROL_SUBNET", control.subnet.as_str());
    lines.set(
        "CHELABASE_CONTROL_COMPUTE_RANGE",
        control.compute_range.as_str(),
    );
    lines.set(
        "CHELABASE_ORCHESTRATOR_CONTROL_IP",
        control.orchestrator_ip.as_str(),
    );
    lines.set("CHELABASE_RUNNER_CONTROL_IP", control.runner_ip.as_str());
    lines.set(
        "CHELABASE_MEMBER_LOGIN_CIDRS",
        format!("{}/32,{}/32", control.orchestrator_ip, control.runner_ip),
    );
    // Realtime's own address on `control` (outside the computes' range) and the two addresses the
    // `chelabase_realtime` login is allowed from: realtime and the runner (it runs realtime).
    lines.set("CHELABASE_REALTIME_CONTROL_IP", control.realtime_ip.as_str());
    lines.set(
        "CHELABASE_REALTIME_LOGIN_CIDRS",
        format!("{}/32,{}/32", control.realtime_ip, control.runner_ip),
    );
    lines.set("CHELABASE_BOOTSTRAP_DIR", path(&input.state.join("bootstrap")));
    for (key, dir) in [
        ("CHELABASE_SECRETS_DIR", "secrets"),
        ("CHELABASE_AUTH_DIR", "auth"),
        ("CHELABASE_NEON_DIR", "neon"),
        ("CHELABASE_GATEWAY_DIR", "gateway"),
    ] {
        lines.set(key, path(&keys_dir.join(dir)));
    }
    if let Some(mask) = checkout_mask(input.checkout, &keys_dir) {
        lines.set("CHELABASE_CHECKOUT_MASK", path(&mask));
    }
    lines.set("CHELABASE_COMPUTE_JWT_DIR", path(&keys_dir.join("neon")));
    lines.set("CHELABASE_COMPUTE_CA_DIR", path(&keys_dir.join("compute-ca")));
    // U15: auth and rest verify computes against the CA certificate alone (only it is mounted
    // into them, never ca.key).
    lines.set(
        "CHELABASE_COMPUTE_CA_FILE",
        path(&keys_dir.join("compute-ca").join("ca.crt")),
    );
    lines.set(
        "CHELABASE_COMPUTE_TLS_DIR",
        path(&keys_dir.join("orchestrator-tls")),
    );
    lines.set(
        "CHELABASE_REALTIME_TLS_DIR",
        path(&keys_dir.join("realtime-tls")),
    );
    // The machine keys every app reads at startup, all in stack.env: no app waits on the
    // bootstrap.
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
        // The member JWT pair (auth-replacement m5): auth signs, the others verify.
        ("CHELABASE_MEMBER_JWT_PRIVATE_KEY", auth_dir.join("member-jwt.key")),
        ("CHELABASE_MEMBER_JWT_PUBLIC_KEY", auth_dir.join("member-jwt.pub")),
    ] {
        lines.set(key, path(&file));
    }
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

#[cfg(test)]
mod mask_tests {
    use super::checkout_mask;

    #[test]
    fn the_second_mask_only_exists_for_a_different_directory() {
        let t = tempfile::tempdir().unwrap();
        let main = t.path().join("m");
        let keys = main.join(".chelabase");
        let wt = main.join(".worktrees/x");
        std::fs::create_dir_all(&keys).unwrap();
        std::fs::create_dir_all(&wt).unwrap();
        assert_eq!(checkout_mask(&wt, &keys), None, "no .chelabase in the checkout");
        std::os::unix::fs::symlink(&keys, wt.join(".chelabase")).unwrap();
        assert_eq!(checkout_mask(&wt, &keys), None, "a symlink to the keys dir");
        assert_eq!(checkout_mask(&main, &keys), None, "the checkout holds the keys dir");
        let other = t.path().join("o");
        std::fs::create_dir_all(other.join(".chelabase")).unwrap();
        assert_eq!(checkout_mask(&other, &keys), Some(other.join(".chelabase")));
    }
}
