//! RT1 T1 (R11, S1, S6, S27): the stack's generated service secrets (created once, never
//! overwritten), the NATS and Valkey auth files rendered from them, and what stack.env and
//! demo.env must not carry.

use super::env::{StackEnvInput, render_stack_env};
use super::service_auth::{render_nats_auth, render_valkey_acl, write_auth_files};
use crate::dev::stack_secrets;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::Path;

/// Every file `stack_secrets::ensure` creates under the keys dir (S1's table, plus S27's bearers).
const SECRET_FILES: &[&str] = &[
    "nats/orchestrator.password",
    "nats/worker.password",
    "nats/realtime.password",
    "nats/runner.password",
    "valkey/proxy.password",
    "valkey/control.password",
    "valkey/realtime.password",
    "valkey/runner.password",
    // Ruling (f): Valkey's own healthcheck logs in as a PING-only user.
    "valkey/health.password",
    "realtime/pg.password",
    "realtime/ticket.key",
    "realtime/internal.token",
    "tokens/rest-internal.token",
    "tokens/data-keys.token",
    "tokens/app-auth.token",
    "tokens/app-sessions.token",
    "tokens/storage-internal.token",
    "tokens/proxy-api.token",
    "tokens/storcon-hook.token",
];

const NATS_USERS: [&str; 5] = ["orchestrator", "worker", "realtime", "runner", "storage"];
const VALKEY_USERS: [&str; 5] = ["proxy", "control", "realtime", "runner", "health"];

fn read_all(keys: &Path) -> BTreeMap<&'static str, Vec<u8>> {
    SECRET_FILES
        .iter()
        .map(|f| {
            (
                *f,
                std::fs::read(keys.join(f)).unwrap_or_else(|e| panic!("{f}: {e}")),
            )
        })
        .collect()
}

#[cfg(unix)]
fn mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

// --- secrets: created once, never overwritten -----------------------------------------------

#[test]
fn every_service_secret_is_created_owner_only_and_strong() {
    let t = tempfile::tempdir().unwrap();
    stack_secrets::ensure(t.path()).unwrap();
    let all = read_all(t.path());
    for (file, bytes) in &all {
        #[cfg(unix)]
        assert_eq!(mode(&t.path().join(file)), 0o600, "{file} is 0600");
        if *file == "realtime/ticket.key" {
            // S1: 32 random bytes, raw (AES-256-GCM, P5).
            assert_eq!(bytes.len(), 32, "{file}");
            continue;
        }
        let text = std::str::from_utf8(bytes).unwrap_or_else(|_| panic!("{file} is text"));
        // Passwords go into URLs (`redis://proxy:<pw>@valkey`, S6) and env files: URL-safe only,
        // no newline, at least 32 characters.
        assert!(text.len() >= 32, "{file}: {} chars", text.len());
        assert!(
            text.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
            "{file} holds only [A-Za-z0-9_-]"
        );
    }
    let mut values: Vec<&Vec<u8>> = all.values().collect();
    values.sort();
    values.dedup();
    assert_eq!(values.len(), SECRET_FILES.len(), "every secret is distinct");
}

#[test]
fn a_second_run_keeps_every_secret() {
    let t = tempfile::tempdir().unwrap();
    stack_secrets::ensure(t.path()).unwrap();
    let first = read_all(t.path());
    stack_secrets::ensure(t.path()).unwrap();
    assert_eq!(read_all(t.path()), first);
}

#[test]
fn a_missing_secret_is_added_and_the_others_kept() {
    let t = tempfile::tempdir().unwrap();
    stack_secrets::ensure(t.path()).unwrap();
    let first = read_all(t.path());
    std::fs::remove_file(t.path().join("valkey/realtime.password")).unwrap();
    stack_secrets::ensure(t.path()).unwrap();
    let second = read_all(t.path());
    for (file, bytes) in &first {
        if *file == "valkey/realtime.password" {
            assert_ne!(&second[file], bytes, "a new value");
        } else {
            assert_eq!(&second[file], bytes, "{file} kept");
        }
    }
}

// An empty file (a crash between create and write, or a hand edit) is neither used as an empty
// password nor silently replaced (another service may already hold the old value).
#[test]
fn an_existing_empty_secret_is_refused_not_overwritten() {
    let t = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(t.path().join("nats")).unwrap();
    std::fs::write(t.path().join("nats/worker.password"), "").unwrap();
    let err = stack_secrets::ensure(t.path()).unwrap_err();
    assert!(format!("{err:#}").contains("worker.password"), "{err:#}");
    assert_eq!(
        std::fs::read(t.path().join("nats/worker.password")).unwrap(),
        b""
    );
}

#[test]
fn the_machine_keys_beside_them_are_left_alone() {
    let t = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(t.path().join("secrets")).unwrap();
    std::fs::write(t.path().join("secrets/secrets.key"), "keep-me").unwrap();
    stack_secrets::ensure(t.path()).unwrap();
    assert_eq!(
        std::fs::read(t.path().join("secrets/secrets.key")).unwrap(),
        b"keep-me"
    );
}

// --- the rendered auth files (S1, S6) -------------------------------------------------------

fn passwords(users: &[&str]) -> BTreeMap<String, String> {
    users
        .iter()
        .map(|u| {
            (
                u.to_string(),
                format!("pw-of-{u}-0123456789abcdef0123456789"),
            )
        })
        .collect()
}

fn sha256_hex(s: &str) -> String {
    hex::encode(Sha256::digest(s.as_bytes()))
}

// S6's ACL file, verbatim, with each password as `#<sha256 hex>`.
#[test]
fn the_valkey_acl_is_s6s_file_with_hashed_passwords() {
    let pw = passwords(&VALKEY_USERS);
    let text = render_valkey_acl(&pw).unwrap();
    let lines: Vec<&str> = text
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
        .collect();
    let h = |u: &str| sha256_hex(&pw[u]);
    assert_eq!(lines.len(), 6, "{text}");
    // Ruling (f): the healthcheck's user may only PING.
    assert_eq!(
        lines[5],
        format!("user health on #{} resetchannels -@all +ping", h("health"))
    );
    assert_eq!(lines[0], "user default off");
    assert_eq!(
        lines[1],
        format!(
            "user proxy on #{} resetchannels &neondb-proxy-ws-updates ~cancel:* -@all +subscribe +ping +set +get +hget +expire +client|setinfo",
            h("proxy")
        )
    );
    assert_eq!(
        lines[2],
        format!(
            "user control on #{} resetchannels &neondb-proxy-ws-updates -@all +publish +ping +client|setinfo",
            h("control")
        )
    );
    assert_eq!(
        lines[3],
        format!(
            "user realtime on #{} resetchannels ~rt:* -@all +set +getdel +incr +decr +expire +zadd +zrem +zremrangebyscore +zcard +multi +exec +ping +client|setinfo",
            h("realtime")
        )
    );
    // runner = realtime's ∪ control's rules (order free).
    let runner: std::collections::BTreeSet<&str> = lines[4].split_whitespace().collect();
    let want: std::collections::BTreeSet<String> = [
        "user",
        "runner",
        "on",
        "resetchannels",
        "&neondb-proxy-ws-updates",
        "~rt:*",
        "-@all",
        "+set",
        "+getdel",
        "+incr",
        "+decr",
        "+expire",
        "+zadd",
        "+zrem",
        "+zremrangebyscore",
        "+zcard",
        "+multi",
        "+exec",
        "+publish",
        "+ping",
        "+client|setinfo",
    ]
    .iter()
    .map(|s| s.to_string())
    .chain([format!("#{}", h("runner"))])
    .collect();
    assert_eq!(
        runner,
        want.iter().map(String::as_str).collect(),
        "{}",
        lines[4]
    );
    assert!(
        lines[4].find("-@all").unwrap() < lines[4].find("+set").unwrap(),
        "-@all before the grants"
    );
    for (u, p) in &pw {
        assert!(
            !text.contains(p.as_str()),
            "{u}'s plaintext password is in the ACL file"
        );
    }
}

#[test]
fn the_valkey_acl_needs_every_users_password() {
    let mut pw = passwords(&VALKEY_USERS);
    pw.remove("proxy");
    assert!(render_valkey_acl(&pw).is_err());
    let mut pw = passwords(&VALKEY_USERS);
    pw.insert("control".into(), String::new());
    assert!(
        render_valkey_acl(&pw).is_err(),
        "an empty password is refused"
    );
}

// S1: the NATS server config holds bcrypt hashes, never the plaintext.
#[test]
fn the_nats_auth_holds_one_bcrypt_hash_per_user_and_no_plaintext() {
    let pw = passwords(&NATS_USERS);
    let text = render_nats_auth(&pw).unwrap();
    let hashes: Vec<&str> = text
        .split(|c: char| c == '"' || c == '\'' || c.is_whitespace() || c == ',')
        .filter(|w| {
            (w.starts_with("$2a$") || w.starts_with("$2b$") || w.starts_with("$2y$"))
                && w.len() == 60
                && w[4..6].parse::<u32>().is_ok_and(|cost| cost >= 10)
        })
        .collect();
    assert_eq!(
        hashes.len(),
        NATS_USERS.len(),
        "one bcrypt hash (cost >= 10) per user:\n{text}"
    );
    let mut unique = hashes.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), hashes.len(), "distinct hashes");
    // Ruling (b): each hash is the bcrypt of that user's password (nats-server checks with it).
    for u in NATS_USERS {
        let matching: Vec<&&str> = hashes
            .iter()
            .filter(|h| bcrypt::verify(&pw[u], h).unwrap_or(false))
            .collect();
        assert_eq!(matching.len(), 1, "{u}'s password matches exactly one hash");
        // The hash sits in that user's own entry: the nearest `user` key before it names `u`.
        let at = text.find(matching[0]).unwrap();
        let key = text[..at]
            .rfind("user")
            .expect("a user key before the hash");
        let name: String = text[key + 4..]
            .trim_start_matches(|c: char| c == ':' || c == '=' || c == '"' || c.is_whitespace())
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
            .collect();
        assert_eq!(name, u, "{u}'s hash is in {name}'s entry:\n{text}");
    }
    for u in NATS_USERS {
        assert!(text.contains(u), "{u} in:\n{text}");
    }
    for (u, p) in &pw {
        assert!(
            !text.contains(p.as_str()),
            "{u}'s plaintext password is in the NATS config"
        );
    }
    // Anonymous access stays off (W13): no user without a password, no default user.
    assert!(!text.contains("no_auth_user"), "{text}");
}

#[test]
fn the_nats_auth_needs_every_users_password() {
    let mut pw = passwords(&NATS_USERS);
    pw.remove("realtime");
    assert!(render_nats_auth(&pw).is_err());
}

#[test]
fn the_auth_files_are_rendered_owner_only_from_the_secret_files() {
    let t = tempfile::tempdir().unwrap();
    let keys = t.path().join("keys");
    let state = t.path().join("state");
    stack_secrets::ensure(&keys).unwrap();
    // A file left by an older render with a loose mode is refused, not silently tightened
    // (selfhost-install m1); a stale one at the right mode is rewritten.
    std::fs::create_dir_all(state.join("valkey")).unwrap();
    std::fs::write(state.join("valkey/users.acl"), "stale").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            state.join("valkey/users.acl"),
            std::fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        assert!(write_auth_files(&keys, &state).is_err(), "a loose users.acl is refused");
        assert_eq!(std::fs::read_to_string(state.join("valkey/users.acl")).unwrap(), "stale");
        std::fs::set_permissions(
            state.join("valkey/users.acl"),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
    }
    write_auth_files(&keys, &state).unwrap();
    let nats = std::fs::read_to_string(state.join("nats/auth.conf")).unwrap();
    let acl = std::fs::read_to_string(state.join("valkey/users.acl")).unwrap();
    #[cfg(unix)]
    {
        assert_eq!(mode(&state.join("nats/auth.conf")), 0o600);
        assert_eq!(mode(&state.join("valkey/users.acl")), 0o600);
    }
    for u in VALKEY_USERS {
        let pw = std::fs::read_to_string(keys.join(format!("valkey/{u}.password"))).unwrap();
        assert!(
            acl.contains(&format!("user {u} on #{} ", sha256_hex(&pw))),
            "{u}'s hash in:\n{acl}"
        );
    }
    for f in SECRET_FILES.iter().filter(|f| **f != "realtime/ticket.key") {
        let secret = std::fs::read_to_string(keys.join(f)).unwrap();
        assert!(
            !nats.contains(&secret) && !acl.contains(&secret),
            "{f}'s plaintext in a rendered file"
        );
    }
    // Rendering again gives the same Valkey file (no churn, no restart for nothing).
    write_auth_files(&keys, &state).unwrap();
    assert_eq!(
        std::fs::read_to_string(state.join("valkey/users.acl")).unwrap(),
        acl
    );
}

// --- stack.env and demo.env carry no per-service credential (S1, S27) -----------------------

const PER_SERVICE_KEYS: &[&str] = &[
    "CHELABASE_NATS_USER",
    "CHELABASE_NATS_PASSWORD",
    "CHELABASE_NATS_PASSWORD_FILE",
    "CHELABASE_NATS_INBOX_PREFIX",
    "CHELABASE_VALKEY_USER",
    "CHELABASE_VALKEY_PASSWORD_FILE",
    "CHELABASE_REALTIME_PG_PASSWORD_FILE",
    "CHELABASE_REALTIME_TICKET_KEY_FILE",
    "CHELABASE_REALTIME_INTERNAL_TOKEN",
    "CHELABASE_REALTIME_INTERNAL_TOKEN_FILE",
    "CHELABASE_REST_INTERNAL_TOKEN",
    "CHELABASE_DATA_KEYS_TOKEN",
    "CHELABASE_APP_AUTH_INTERNAL_TOKEN",
    "CHELABASE_APP_SESSIONS_INTERNAL_TOKEN",
    "CHELABASE_PROXY_API_TOKEN",
    "CHELABASE_STORCON_HOOK_TOKEN",
];

const ENV_EXAMPLE: &str = include_str!("../../../../infra/compose/.env.example");

fn stack_input<'a>() -> StackEnvInput<'a> {
    StackEnvInput {
        slug: "feat-x",
        project: "chelabase-feat-x",
        checkout: Path::new("/m/.worktrees/feat-x"),
        main: Path::new("/m"),
        state: Path::new("/m/.chelabase/stacks/feat-x"),
        gateway_port: 8137,
        proxy_port: 4537,
        uid: 1000,
        gid: 1001,
        docker_gid: 973,
        image: "chelabase-workbench:abc123def456",
        extra: &[],
        control_slot: 1,
    }
}

fn keys_of(text: &str) -> Vec<(String, String)> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.starts_with('#') && l.contains('='))
        .map(|l| {
            let (k, v) = l.split_once('=').unwrap();
            (k.trim().to_string(), v.to_string())
        })
        .collect()
}

// Every app container reads stack.env (and demo.env): a user, password or bearer there is every
// service's. They go per service in compose `environment:` with narrow file mounts.
#[test]
fn the_stack_env_from_the_real_example_holds_no_service_credential() {
    let text = render_stack_env(ENV_EXAMPLE, &stack_input());
    for (k, v) in keys_of(&text) {
        assert!(
            !PER_SERVICE_KEYS.contains(&k.as_str()),
            "{k} is in stack.env"
        );
        assert!(
            !v.contains("chelabase-dev-"),
            "{k}={v}: a public development bearer in stack.env"
        );
    }
    let env: BTreeMap<String, String> = keys_of(&text).into_iter().collect();
    assert!(
        !env["CHELABASE_VALKEY_URL"].contains('@'),
        "the Valkey URL carries no credentials (S1)"
    );
    assert!(
        !env["CHELABASE_NATS_URL"].contains('@'),
        "the NATS URL carries no credentials"
    );
}

#[test]
fn the_example_sets_no_development_bearer() {
    for (k, v) in keys_of(ENV_EXAMPLE) {
        assert!(!v.contains("chelabase-dev-"), ".env.example sets {k}={v}");
        assert!(
            !PER_SERVICE_KEYS.contains(&k.as_str()),
            ".env.example sets {k}"
        );
    }
}

// P4/W14: computes are created on the stack's own `compute` network (plus `control`), never on
// `_default` with NATS, Valkey and every database.
#[test]
fn computes_join_the_stacks_compute_network() {
    let env: BTreeMap<String, String> = keys_of(&render_stack_env(ENV_EXAMPLE, &stack_input()))
        .into_iter()
        .collect();
    assert_eq!(env["CHELABASE_COMPUTE_NETWORK"], "chelabase-feat-x_compute");
    assert_eq!(
        env["CHELABASE_COMPUTE_CONTROL_NETWORK"],
        "chelabase-feat-x_control"
    );
}

// Ruling (e): auth trusts `X-Forwarded-For` only from the gateway (a compute reaches auth on the
// compute network). The stack names the gateway, not a blanket `true`.
#[test]
fn auth_trusts_forwarded_headers_from_the_gateway_only() {
    let env: BTreeMap<String, String> = keys_of(&render_stack_env(ENV_EXAMPLE, &stack_input()))
        .into_iter()
        .collect();
    assert_eq!(env["CHELABASE_AUTH_TRUSTED_PROXY"], "gateway");
}

// --- storage auth (user, 2026-10-16): NeonJWT from the existing compute-JWT key pair --------

const STORAGE_TOKENS: [(&str, &str, Option<&str>); 4] = [
    // file, scope, what holds it
    (
        "pageserver-api",
        "pageserverapi",
        Some("storcon -> pageserver"),
    ),
    (
        "safekeeper-data",
        "safekeeperdata",
        Some("storcon/pageserver/safekeeper -> safekeeper"),
    ),
    (
        "generations-api",
        "generations_api",
        Some("pageserver -> storcon upcalls"),
    ),
    (
        "storcon-admin",
        "admin",
        Some("register-safekeepers, the runner -> storcon"),
    ),
];

fn decode_with_pem(pem: &str, token: &str) -> serde_json::Value {
    use jsonwebtoken::{Algorithm, DecodingKey, Validation};
    let key = DecodingKey::from_ed_pem(pem.as_bytes()).expect("an SPKI PEM Ed25519 key");
    let mut v = Validation::new(Algorithm::EdDSA);
    v.required_spec_claims = Default::default();
    jsonwebtoken::decode::<serde_json::Value>(token, &key, &v)
        .unwrap_or_else(|e| panic!("Neon rejects the token: {e}"))
        .claims
}

#[test]
fn the_storage_auth_files_are_derived_from_the_compute_jwt_pair() {
    let t = tempfile::tempdir().unwrap();
    let neon = t.path().join("neon");
    crate::neon::compute_jwt::ensure(&neon).unwrap();
    let pair = (
        std::fs::read(neon.join("compute-jwt.pem")).unwrap(),
        std::fs::read(neon.join("compute-jwt.pub")).unwrap(),
    );
    super::service_auth::write_storage_auth(t.path()).unwrap();
    let pem = std::fs::read_to_string(neon.join("compute-jwt.pub.pem")).unwrap();
    assert!(pem.starts_with("-----BEGIN PUBLIC KEY-----"), "{pem}");
    for (file, scope, _) in STORAGE_TOKENS {
        let path = neon.join("storage-tokens").join(format!("{file}.jwt"));
        #[cfg(unix)]
        assert_eq!(mode(&path), 0o600, "{file}");
        let token = std::fs::read_to_string(&path).unwrap();
        let claims = decode_with_pem(&pem, token.trim());
        assert_eq!(claims["scope"], scope, "{file}: {claims}");
        assert!(
            claims["tenant_id"].is_null(),
            "{file}: no tenant on a service token"
        );
        assert!(claims.get("exp").is_none(), "{file}");
    }
    // The pair itself is never regenerated or rewritten.
    assert_eq!(
        (
            std::fs::read(neon.join("compute-jwt.pem")).unwrap(),
            std::fs::read(neon.join("compute-jwt.pub")).unwrap()
        ),
        pair
    );
    // A second run writes the same files (deterministic Ed25519 signatures, no exp).
    let before = std::fs::read(neon.join("storage-tokens/storcon-admin.jwt")).unwrap();
    super::service_auth::write_storage_auth(t.path()).unwrap();
    assert_eq!(
        std::fs::read(neon.join("storage-tokens/storcon-admin.jwt")).unwrap(),
        before
    );
}

#[test]
fn storage_auth_needs_the_existing_pair_and_never_makes_one() {
    let t = tempfile::tempdir().unwrap();
    assert!(super::service_auth::write_storage_auth(t.path()).is_err());
    assert!(
        !t.path().join("neon/compute-jwt.pem").exists(),
        "no key pair is generated here"
    );
}
