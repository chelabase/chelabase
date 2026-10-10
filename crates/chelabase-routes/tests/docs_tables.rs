//! The generated tables in `apps/docs`, and their staleness checks.
//!
//! - `reference/access.mdx`: every route with its auth and access level, from `ENDPOINTS`.
//! - `using/self-hosted-vs-platform.mdx`: every mode rule with its value per mode, from
//!   `ModePolicy`.
//! - `reference/errors.mdx`: every `ErrorCode` must have a row (`ErrorCode::ALL`).
//!
//! Each generated table sits between `{/* BEGIN GENERATED <name> */}` and `{/* END GENERATED
//! <name> */}` lines. The tests fail when the file differs from what the code says; rewrite the
//! tables with `CHELABASE_BLESS=1 cargo test -p chelabase-routes --test docs_tables`.

use std::fmt::Write as _;
use std::path::PathBuf;

use chelabase_routes::{Access, Auth, Cors, ENDPOINTS, Endpoint, PlatformRole};
use chelabase_types::api_error::ErrorCode;
use chelabase_types::mode::Mode;
use chelabase_types::mode_policy::ModePolicy;

fn docs_file(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../apps/docs/content/docs").join(rel)
}

fn markers(name: &str) -> (String, String) {
    (format!("{{/* BEGIN GENERATED {name} */}}"), format!("{{/* END GENERATED {name} */}}"))
}

/// Replaces the block between the markers (blessing) or compares it (checking).
fn check_block(rel: &str, name: &str, generated: &str) {
    let path = docs_file(rel);
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let (begin, end) = markers(name);
    let b = text.find(&begin).unwrap_or_else(|| panic!("{rel}: missing `{begin}`"));
    let e = text.find(&end).unwrap_or_else(|| panic!("{rel}: missing `{end}`"));
    assert!(b < e, "{rel}: markers out of order");
    let want = format!("{begin}\n\n{generated}\n{end}");
    let have = &text[b..e + end.len()];
    if have == want {
        return;
    }
    if std::env::var_os("CHELABASE_BLESS").is_some() {
        let new = format!("{}{want}{}", &text[..b], &text[e + end.len()..]);
        std::fs::write(&path, new).unwrap();
        return;
    }
    panic!("{rel} is stale ({name}): run `CHELABASE_BLESS=1 cargo test -p chelabase-routes --test docs_tables` and commit");
}

fn role(r: PlatformRole) -> &'static str {
    match r {
        PlatformRole::Viewer => "Viewer",
        PlatformRole::Developer => "Developer",
        PlatformRole::Admin => "Admin",
    }
}

fn access_text(a: Access) -> String {
    match a {
        Access::Public => "Public".into(),
        Access::DataKey => "Data API key".into(),
        Access::SignedIn => "Signed in".into(),
        Access::Instance => "Instance admin".into(),
        Access::TenantMember => "Tenant member".into(),
        Access::TenantAdmin => "Tenant admin".into(),
        Access::Project(r) => format!("Project {}", role(r)),
        Access::Branch { unprotected, protected } => {
            format!("Branch: {} unprotected, {} protected", role(unprotected), role(protected))
        }
        Access::ActAs(r) => format!("Act as: {} and the role in your set", role(r)),
    }
}

fn auth_text(a: Auth) -> &'static str {
    match a {
        Auth::Public => "none",
        Auth::Management => "Zitadel token",
        Auth::DataKey => "any data key",
        Auth::SecretDataKey => "secret data key",
        Auth::PublicDataKey => "public data key",
    }
}

fn access_table() -> String {
    let mut sorted: Vec<&Endpoint> = ENDPOINTS.iter().collect();
    sorted.sort_by(|a, b| (a.path, a.method.as_str()).cmp(&(b.path, b.method.as_str())));
    let mut out = String::from("| Method | Path | Credential | Access |\n|---|---|---|---|\n");
    for ep in sorted {
        let _ = Cors::None; // the CORS policy is not part of the table
        let _ = writeln!(
            out,
            "| {} | `{}` | {} | {} |",
            ep.method.as_str(),
            ep.path.replace('{', "\\{").replace('}', "\\}"),
            auth_text(ep.policy.auth),
            access_text(ep.policy.access)
        );
    }
    out
}

fn mode_table() -> String {
    let sh = ModePolicy::new(Mode::SelfHosted);
    let pf = ModePolicy::new(Mode::Platform);
    let mut out = String::from("| Rule | What it decides | Self-hosted | Platform |\n|---|---|---|---|\n");
    for (((name, description), (_, a)), (_, b)) in
        ModePolicy::RULES.iter().zip(sh.describe()).zip(pf.describe())
    {
        let _ = writeln!(out, "| `{name}` | {description} | {a} | {b} |");
    }
    out
}

#[test]
fn access_table_is_current() {
    check_block("reference/access.mdx", "access-table", &access_table());
}

#[test]
fn mode_table_is_current() {
    check_block("using/self-hosted-vs-platform.mdx", "mode-table", &mode_table());
}

/// Every code a handler can answer has a row in the error reference.
#[test]
fn every_error_code_is_documented() {
    let text = std::fs::read_to_string(docs_file("reference/errors.mdx")).unwrap();
    let missing: Vec<&str> =
        ErrorCode::ALL.iter().map(|c| c.as_str()).filter(|c| !text.contains(&format!("`{c}`"))).collect();
    assert!(missing.is_empty(), "reference/errors.mdx has no row for: {missing:?}");
}

#[test]
fn error_codes_are_listed_once() {
    let mut names: Vec<&str> = ErrorCode::ALL.iter().map(|c| c.as_str()).collect();
    let n = names.len();
    names.sort_unstable();
    names.dedup();
    assert_eq!(names.len(), n, "a code is listed twice in ErrorCode::ALL");
}
