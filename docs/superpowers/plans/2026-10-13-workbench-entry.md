# W1: the entry layout (implementation plan)

**Goal:** all browser and API HTTP goes through the Pingora gateway on one port, routed by host name. The hosts are `studio.`, `api.` and `auth.`, plus `docs.`, `landing.` and `mail.` in the workbench, all under `chelabase.localhost:8000` there. Zitadel moves to `auth.chelabase.localhost:8000`. Every caller (Studio, scripts, live tests, Playwright, the CLI bootstrap) uses the entry hosts, and an offline guard keeps it that way.

**Spec:** `docs/superpowers/specs/2026-10-13-workbench-entry-design.md` (approved 2026-10-13, amended with `chelabase.localhost`). Each implementer reads the spec section its task names.

**Architecture:**
- `chelabase_types::entry` is the single source for host settings and derived URLs. Every service and the CLI read it.
- The gateway gains a host → target registry (`Api`, or a pass-through `Site`) and per-site upstreams.
- The services derive Zitadel's issuer and Studio's origin from `Entry`.
- The CLI bootstrap takes Studio's origin and the issuer from `Entry`.
- Compose, the justfile, the Vite configs and the scripts move to the entry hosts. The guard `scripts/tests/entry-urls.sh` runs in `just check`.

**Tech:** Rust (pingora 0.x as pinned, axum and reqwest in tests), Vite configs (TypeScript), bash. There are no new dependencies. There is no fork change, so no image build; the live pass runs on the pinned images.

**Standing rule:** the coordinator writes and commits each task's RED tests (below) before the brief, runs them RED and records the result in the Status. Implementers make them pass without editing, weakening, skipping or deleting them. Reviewers check that `git diff <red-commit>..HEAD` on the RED test code is empty; for inline `#[cfg(test)] mod tests`, that means the test module. When a RED commit needs a module to exist so that its tests compile, the coordinator adds only the empty module declaration. **P9** sets out how existing tests outside the RED commits may change.

## Status

- **RESUME HERE (2026-10-13): DONE, paused for the user's merge decision.** Every task, the end-of-step review and its fixes, `just check` and the live pass are green on `workbench-entry` (`bc6a64f6` + this checkpoint). The stack runs from the main checkout again (on the old addresses; Zitadel's setup moved its domain both ways without a reset).
- **Live pass (2026-10-13), from `.worktrees/workbench-entry`:** `just dev` started cold with the new entry (the bootstrap reached Zitadel directly, W-R3); S1 held: the existing Zitadel instance answered at `auth.chelabase.localhost:8000` with the new issuer, no reset; hosts routed (api/studio/mail 200, an unknown host 404). Orchestrator ignored-only 49/49; worker `app_users_live` 13/13, `roles_live` 42/42, `merge_live` 26/26, `member_roles_live` 20/20; `rest_live` 38/38; `catalog_live` 12/12 (single-threaded, as in TE-A; its parallel run collides in the harness, not this branch); e2e: auth, members, app-users, sql-editor, console, merge, role, branch, data, neon all green. Browser pass: Studio sign-in at `studio.chelabase.localhost:8000` through Zitadel at the auth host, Studio's API calls (projects list), Vite hot reload through the gateway (text updated, page state kept), Zitadel's console sign-in (its v1 login form posts through the gateway with CSRF intact; Playwright needed `form.submit()`, the page's own button script, not the gateway), Mailpit at the mail host, docs (new Hosts page) and landing at theirs (its Studio links point at the Studio host). Noted, not W1: no app ships a favicon (404 on every site).
- **End-of-step review (opus):** not ready: 2 critical (`just dev`'s bootstrap reached Zitadel through the gateway before it started; `_cleanup` emptied Mailpit through the gateway after stopping it), 3 important (the Hosts page's upstream table failed the guard; Secure cookies by mode, not scheme; Zitadel's `--tlsMode disabled` would force an http issuer under https), 6 minor (`zitadel-reset.sh` without `entry.sh` and compose defaults as a second copy; `entry.sh` vs `Entry` parity; recipes/scripts/token.rs/Vite with their own URL copies; the landing host; docs mismatches). **User rulings:** Secure cookies follow the scheme (override kept, warned when off under https), platform refuses an http entry; landing at `landing.chelabase.localhost`. **W-R3 (coordinator):** the CLI bootstrap reaches Zitadel's upstream (`CHELABASE_ZITADEL_UPSTREAM`) with the auth host as `Host`, so it needs no gateway; cost if wrong: a deploy where the CLI can't reach Zitadel directly needs the override. Fix-round RED `e8f99487` (Rust: upstream defaults, `check_mode`, `secure_cookies(scheme, …)`, `via_upstream`; shell: `entry-sh.sh` parity matrix and compose checks, new `dev-recipes.sh`; the guard allow-lists `entry.rs` and the Hosts page instead of the gateway config), fix `4f5bd8fb`, `0e4f3941`, `ccb3e923`, `694c5360` (verified by the coordinator; 1710 passed; `just check` green). Accepted deviations: compose requires its entry values (`${VAR:?}`), so every compose call sources `entry.sh`; the gateway reads `CHELABASE_MODE`; `entry.sh` returns 1 on a bad scheme; `_cleanup` notes, not fails, when Mailpit can't be reached. **Noted for later:** the landing page's runtime fallback Studio URL (`apps/landing/src/studio-url.ts`) is a literal (the build gets `VITE_STUDIO_URL`).
- **Done: T6** (sonnet, no review): `8fee7b48` (Self-hosting → Hosts page, CONTRIBUTING, stale variable names, `.env.example` comments); its Secure-cookies claim was wrong and fed the end-of-step review.
- **Done: T5** (sonnet/sonnet): RED `a8593bef` (`entry-urls.sh`, 80 hits across 30 files, more than the plan listed: Studio/landing tests and source, docs pages, README, crate tests; all fixed under P9), implementation `ae3ba532` (accepted: `scripts/lib/entry.sh` reads `.env.example` as the one place for defaults; **W-R2 (coordinator): Studio's `origin.ts` 127.0.0.1→localhost:3000 redirect deleted**, dead under the entry layout; cost: a visit to the loopback port 3000 in W1 serves Studio but its sign-in fails, until W2 unpublishes the port), review: not ready, 3 important (the guard failed on a new comment, so `just check` was red; `CHELABASE_AUTH_HOST_NAME`/`PUBLIC_PORT`/`PUBLIC_SECURE` configured twice with an unsound check; Studio's Vite read only `.env.local`'s host) + 3 minor (hand-written `VITE_*` in `.env.example`; `origin.ts` removal, kept per W-R2; literal `.env.example` parsing); fix-round RED `d2c08160` (`scripts/tests/entry-sh.sh`: invariant "derived entry values are never configured", exhaustive over scheme × auth-host form), fix `066484d1` (verified by the coordinator; both shell tests pass; `just check` green).
- **Done: T2** (opus/opus): RED `e55e2099`, implementation `c808bf17`, coordinator RED-test fix `f371e6b6` (missing `Outcome::Site` arms; the miss-flood test sent no Host), review: pass, 2 minor (a repeated listen address; every site request got the 3600 s timeout, spec §2 says websockets only) + a T1 gap (a host with its scheme's default port could never match); fix-round RED `874501b4`, fix `41b512d2` (accepted: `issuer_from_lookup` refuses the default port too); merged `a687b518`; 1706/1706 across the touched crates on the merged branch. Accepted choices: `Gateway::new` takes the scheme; X-Forwarded-For/Host removed when absent; a malformed Host → 404; sites don't get `x-request-id` upstream; `*_UPSTREAM` ignored without its host.
- **Done: T3** (sonnet/sonnet): RED `6e3516f5`, implementation `aee9d1da` (P9: `app_auth_settings_are_read` setup moved to the auth host), review: approve, 3 minor (stale operator messages naming `CHELABASE_ZITADEL_ISSUER`/`CHELABASE_STUDIO_URL`; the orchestrator parsed the entry three times; its `zitadel_admin` "auth host unset" branch was unreachable since the orchestrator requires the whole entry, intended per spec §1); fix `a5041782` (wording; the entry read once in `main`), verified by the coordinator by reading (**W-R1:** wording and `main.rs` wiring aren't observable from tests; cost: none); merged `330de802`.
- **Done: T4** (sonnet/sonnet): RED `bed69501` (7 signature compile errors), implementation `df8e8a2d` (accepted: `steps::ensure_studio_app`/`ensure_studio_login` take the origin; `paths::issuer()` returns a Result), review: approve, 2 minor (over-long lines, a const between `use` lines); fix `e0bf8107` (formatting only, verified whitespace-insensitive by the coordinator; no RED possible, as R1 in TE-A); merged `6f534da5`.
- **Done: T1** (sonnet/sonnet): RED `d6495f66` (26 compile errors), implementation `e2252df6`, review: pass, 4 minor (empty labels and malformed names accepted, port 0 and `+80`, dotted-numeric non-IPs as names, case-sensitive scheme/issuer); a second case in one area, so the fix round stated the host-setting invariant and tested it against a model (proptest, 4000 cases) plus the same-site parent and case rules: RED `4534e13a`, fix `cb036173` (verified by the coordinator), coordinator test tidy and seed `932d4ca1`; 72/72. Accepted: the identical-host check covers all six host settings; IP literals compared as parsed addresses.

## Plan rulings (mechanical, listed for the user)

- **P1 Host matching.** A configured host with a port (`auth.chelabase.localhost:8000`) matches only requests whose `Host` has that port. A host without a port (`studio.acme.com`) matches any port. An exact host:port match wins over a name-only one. Two settings that are identical after normalization are refused at startup. Cost if wrong: a request whose `Host` omits a configured port gets a 404.
- **P2 Same-site check.** Studio and the API must be the same site. If either host is an IP literal, both must be the same IP. Otherwise both are DNS names with the same parent (the host minus its first label), and that parent has at least two labels: `chelabase.localhost` and `acme.com` pass, `localhost` doesn't. There is no public-suffix list, so `studio.co.uk` / `api.co.uk` would pass. Cost if wrong: a deploy under a multi-label public suffix (e.g. `x.pages.dev`) isn't caught at startup.
- **P3 Issuer.** `CHELABASE_ZITADEL_ISSUER` is derived from `CHELABASE_AUTH_HOST` and `CHELABASE_PUBLIC_SCHEME`. Setting it alone, without `CHELABASE_AUTH_HOST`, is refused. Setting it with a different value is refused; an equal value (a trailing `/` aside) is accepted. `ZitadelAuthConfig` no longer defaults to `http://localhost:8080`. Cost: any env that sets only the issuer must add the auth host.
- **P4 Forwarded headers.** `X-Forwarded-Proto` is `CHELABASE_PUBLIC_SCHEME`, which replaces "keep an operator proxy's value". `X-Forwarded-For` is the client IP only (no append). `X-Forwarded-Host` is the request's `Host`. All three are replaced on every target. Cost if wrong: a self-hoster behind a TLS proxy must set `CHELABASE_PUBLIC_SCHEME=https`, which the spec already requires.
- **P5** `CHELABASE_GATEWAY_LISTEN` (a list) replaces `CHELABASE_GATEWAY_ADDR`. Setting the old variable is refused with a pointer to the new one.
- **P6** `Site` and `Target` live in the gateway (`registry.rs`). `chelabase_routes::Scope` stays `Api`-only, since route tables exist only for the API. The spec's "`Scope` gains…" is realized as `Target::Site(Site)`.
- **P7** The bootstrap test `studio_and_the_gateway_are_the_same_site` is deleted: P2's startup check in `Entry` now holds that invariant (T1 tests it).
- **P8** CORS allows exactly Studio's origin. The `"*"` origin setting and its test `a_wildcard_origin_setting_never_allows_credentials` go away, since the setting no longer exists (S6). `Cors::Any` endpoints still answer `*`.
- **P9 Existing tests.** Tests outside the RED commits may change only call signatures and URL or host literals, to follow the pinned interfaces and the entry hosts. Their assertions must not be weakened, and the reviewer checks this. The tests that P7 and P8 delete are the exceptions.

## Task table (binding)

| Task | Scope | Implementer | Reviewer | Wave | Depends |
|---|---|---|---|---|---|
| T1 | `chelabase_types::entry`: host settings, validation, P1/P2/P3 rules, derived URLs | sonnet | sonnet | 1 | — |
| T2 | gateway: config from `Entry`, the `Registry`/`Target`/`Site` routing, per-site upstreams, pass-through proxying (websockets, `Host` kept, no auth or CORS), P4 headers, list listeners, plain-text 502 on sites | opus | opus | 2 | T1 |
| T3 | services: auth, orchestrator, worker and `chelabase-auth` read Studio's origin and the issuer from `Entry` | sonnet | sonnet | 2 | T1 |
| T4 | CLI bootstrap: Studio's origin, the API base and the issuer from `Entry`; Studio app URIs and drift; `apps/studio/.env.local` keys | sonnet | sonnet | 2 | T1 |
| T5 | compose, `.env.example`, justfile `dev` exports and `check`, Vite configs (Studio, docs, landing), scripts and `scripts/lib/entry.sh`, Rust live-test defaults, Playwright, client e2e, bench; the guard passes | sonnet | sonnet | 3 | T2, T3, T4 |
| T6 | docs: `CONTRIBUTING.md`, a new "Self-hosting → Hosts" docs page, `.env.example` comments | sonnet | none (the end-of-step review covers it) | 4 | T5 |

Wave 2 runs in parallel, each task in its own worktree (`just wt-add workbench-entry-tN workbench-entry`); the files are disjoint (pre-flight scan). No more than 3 builds at once.

## Shared interfaces (pinned)

`crates/chelabase-types/src/entry.rs` (T1; `pub mod entry;` in `lib.rs`):

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scheme { Http, Https }

/// The public entry: host settings and every URL derived from them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry { /* private */ }

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryError(pub String);
impl std::fmt::Display for EntryError { /* the message */ }
impl std::error::Error for EntryError {}

impl Entry {
    /// Reads CHELABASE_PUBLIC_SCHEME (default https), CHELABASE_{STUDIO,API,AUTH}_HOST (required),
    /// CHELABASE_{DOCS,LANDING,MAIL}_HOST (optional; blank = unset). Refuses: a missing or invalid
    /// host, a bad scheme, two identical hosts (P1), Studio/API not one site (P2), the removed
    /// CHELABASE_API_HOSTS / CHELABASE_STUDIO_ORIGINS / CHELABASE_STUDIO_URL when non-blank (the
    /// message names the replacement), and CHELABASE_ZITADEL_ISSUER that differs from the derived
    /// issuer (P3). Messages name the variable.
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Self, EntryError>;
    pub fn from_env() -> Result<Self, EntryError>;
    pub fn scheme(&self) -> Scheme;
    /// Normalized (lowercase, no trailing dot), port kept as configured.
    pub fn studio_host(&self) -> &str;
    pub fn api_host(&self) -> &str;
    pub fn auth_host(&self) -> &str;
    pub fn docs_host(&self) -> Option<&str>;
    pub fn landing_host(&self) -> Option<&str>;
    pub fn mail_host(&self) -> Option<&str>;
    /// `<scheme>://<studio host>` (no trailing slash); likewise the API base and the issuer.
    pub fn studio_origin(&self) -> String;
    pub fn api_base(&self) -> String;
    pub fn issuer(&self) -> String;
}

/// P3 for services that need only the issuer: None when neither CHELABASE_AUTH_HOST nor
/// CHELABASE_ZITADEL_ISSUER is set; the derived issuer when the auth host is set (a different
/// explicit issuer refused); an error naming CHELABASE_AUTH_HOST when only the issuer is set.
pub fn issuer_from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Option<String>, EntryError>;

/// Validation of one host setting (moved from the gateway's registry): a hostname or IPv4
/// address, or a bracketed IPv6 address, optionally with `:port`.
pub fn validate_host(entry: &str) -> Result<(), String>;
/// Lowercase, drop any port and a single trailing dot (`[::1]:8000` → `[::1]`).
pub fn normalize_host(raw: &str) -> String;
/// The port of a host (`a.b:8000` → Some(8000), `[::1]` → None).
pub fn host_port(raw: &str) -> Option<u16>;
```

`services/gateway/src/registry.rs` (T2):

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Site { Studio, Auth, Docs, Landing, Mail }
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target { Api, Site(Site) }
impl Registry {
    pub fn new<I, S>(api_hosts: I) -> Self where I: IntoIterator<Item = S>, S: AsRef<str>;
    pub fn with_site(self, host: &str, site: Site) -> Self;
    pub fn from_entry(entry: &Entry) -> Self;
    /// P1. None for no Host or no match.
    pub fn target_for(&self, host: Option<&str>) -> Option<Target>;
}
```

`services/gateway` (T2):
- `pipeline::Outcome::Site(Site)`, a new variant.
- `Pipeline::new(registry: Arc<SharedRegistry>, studio_origin: String, verifier, signer, keys, users)`.
- `Upstreams::with_site(self, site: Site, url: &str) -> Result<Self, String>` and `Upstreams::site_addr(&self, site: Site) -> Option<&str>`.
- `GatewayConfig { listen: Vec<String>, entry: Entry, upstreams, … }` with `GatewayConfig::from_lookup(get) -> anyhow::Result<Self>`. Its site upstreams are `CHELABASE_{STUDIO,ZITADEL,DOCS,LANDING,MAIL}_UPSTREAM`, defaulting to `http://127.0.0.1:{3000,8080,3001,3002,8025}`. Docs, landing and mail upstreams exist only when the matching host is set.

`crates/chelabase-auth` (T3): `ZitadelAuthConfig::from_lookup(get) -> Result<Self, AuthError>`. `from_env` calls it.

`tools/cli` (T4):
- `studio_app_config(mode, origin: &str)`, `studio_app_create(project_id, mode, origin)`, `studio_app_update(project_id, app_id, mode, origin)` and `studio_app_drift(app, mode, origin)`.
- `render_studio_env(issuer, client_id, project_id, org_id, api_url, studio_host)` adds `VITE_STUDIO_HOST=`.
- `paths::entry() -> anyhow::Result<Entry>`, and `paths::issuer()` returns `entry()?.issuer()`.
- The constants `STUDIO_ORIGIN` and `GATEWAY_URL` are removed.

## Pre-flight scan (2026-10-13)

- **Wave 2 file sets are disjoint:**
  - T2 has `services/gateway/**` only.
  - T3 has `services/{auth,orchestrator,worker}/src/**` (config and main only), `services/{orchestrator,worker}/src/**` tests modules, and `crates/chelabase-auth/{src/lib.rs,tests/zitadel_config.rs}`.
  - T4 has `tools/cli/**` only.
  - `Cargo.lock` is untouched (no new deps).
- **Readers of the entry variables (code):**
  - `services/gateway/src/config.rs:53-54` (`API_HOSTS`, `STUDIO_ORIGINS`);
  - `services/auth/src/config.rs:116` (`STUDIO_ORIGINS` → email links) and `:132` (issuer);
  - `services/orchestrator/src/main.rs:114` (`STUDIO_URL` → invite links) and `:162` (issuer for the admin PAT);
  - `services/orchestrator/src/internal/config.rs:82`;
  - `services/worker/src/config.rs:108`;
  - `crates/chelabase-auth/src/lib.rs:40` (issuer, default `localhost:8080`);
  - `tools/cli/src/paths.rs:99`, `tools/cli/src/dev/token.rs:45` (from `demo.env`), `tools/cli/src/dev/requests.rs:110` (`STUDIO_ORIGIN`) and `tools/cli/src/dev/bootstrap.rs:17` (`GATEWAY_URL`).
- **Gateway:**
  - `Pipeline` holds `studio_origins: Vec<String>` with a `"*"` wildcard (`pipeline.rs:287-295`; test `:992`), which P8 removes.
  - The pipeline tests call `harness(&[])` with no host in 22 places; they're rewritten in T2's RED commit.
  - The e2e harness (`tests/gateway.rs:112-140`) sets `CHELABASE_GATEWAY_ADDR`, `API_HOSTS` and `STUDIO_ORIGINS`; it's rewritten in T2's RED commit.
  - `incoming_forwarded_proto_is_preserved` (`:520-534`) contradicts P4 and is replaced.
  - `X-Forwarded-For` is appended today (`proxy.rs:160-175`).
- **Zitadel:**
  - Compose sets `EXTERNALDOMAIN=localhost`, `EXTERNALPORT=8080` and `LOGINV2_BASEURI`/`DEFAULTLOGIN…` = `localhost:3000` (`docker-compose.yml:100-117`).
  - `start-from-init` reruns setup at each start (S1).
- **Studio:**
  - The login client fetches `/auth/v1` with `credentials: "include"` (`apps/studio/src/lib/login-client/fetch.ts:21`).
  - The auth cookies are `SameSite=Strict; Path=/auth/v1` (`services/auth/src/cookie.rs:105`). Hence `chelabase.localhost` (P2).
- **Name resolution:** `*.localhost` resolves to `::1` only on this machine (`getent hosts studio.chelabase.localhost`), so the gateway listens on `[::1]:8000` too. Rust clients connecting to `api.chelabase.localhost:8000` reach `::1`.
- **URL literals for T5** (the survey, §6): `scripts/{auth,members,neon}-e2e.sh`, `scripts/neon-smoke.sh`, `scripts/lib/neon-common.sh`, `justfile` (`_live-swap-dev` polls `:3000`, `_cleanup` uses Mailpit `:8025`, `create-db-demo`), Rust live tests (`GATEWAY_URL` defaults in `app_users_live.rs`, `roles_live.rs`, `merge_live.rs`; `rest_live.rs`), `packages/client/scripts/e2e.mjs`, `tools/bench-data-api`, the Playwright configs, `infra/compose/.env.example:17,123,127`.

---

## T1: `chelabase_types::entry` (sonnet / sonnet)

**Files:** `crates/chelabase-types/src/{lib.rs,entry.rs}` and `crates/chelabase-types/src/entry/tests.rs` (coordinator).

**Behaviour:** spec §1 plus P1, P2 and P3, with the interfaces above. `validate_host` and `normalize_host` move here from `services/gateway/src/registry.rs`, unchanged in behaviour. The gateway's copies are removed in T2.

**RED tests (coordinator):** `entry.rs` holds only `#[cfg(test)] mod tests;`. `entry/tests.rs`:

```rust
use super::*;
use std::collections::HashMap;

const WORKBENCH: &[(&str, &str)] = &[
    ("CHELABASE_PUBLIC_SCHEME", "http"),
    ("CHELABASE_STUDIO_HOST", "studio.chelabase.localhost:8000"),
    ("CHELABASE_API_HOST", "api.chelabase.localhost:8000"),
    ("CHELABASE_AUTH_HOST", "auth.chelabase.localhost:8000"),
];

/// WORKBENCH with `extra` on top (an `extra` entry overrides; value "-" removes the name).
fn env(extra: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
    let mut map: HashMap<String, String> =
        WORKBENCH.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    for (k, v) in extra {
        if *v == "-" {
            map.remove(*k);
        } else {
            map.insert(k.to_string(), v.to_string());
        }
    }
    move |name: &str| map.get(name).cloned()
}

fn err(extra: &[(&str, &str)]) -> String {
    Entry::from_lookup(env(extra)).unwrap_err().to_string()
}

#[test]
fn workbench_urls_are_derived_from_the_hosts() {
    let e = Entry::from_lookup(env(&[])).unwrap();
    assert_eq!(e.scheme(), Scheme::Http);
    assert_eq!(e.studio_host(), "studio.chelabase.localhost:8000");
    assert_eq!(e.studio_origin(), "http://studio.chelabase.localhost:8000");
    assert_eq!(e.api_base(), "http://api.chelabase.localhost:8000");
    assert_eq!(e.issuer(), "http://auth.chelabase.localhost:8000");
    assert_eq!((e.docs_host(), e.landing_host(), e.mail_host()), (None, None, None));
}

#[test]
fn the_scheme_defaults_to_https_and_is_checked() {
    let e = Entry::from_lookup(env(&[
        ("CHELABASE_PUBLIC_SCHEME", "-"),
        ("CHELABASE_STUDIO_HOST", "studio.acme.com"),
        ("CHELABASE_API_HOST", "api.acme.com"),
        ("CHELABASE_AUTH_HOST", "auth.acme.com"),
    ]))
    .unwrap();
    assert_eq!(e.scheme(), Scheme::Https);
    assert_eq!(e.studio_origin(), "https://studio.acme.com");
    assert_eq!(e.issuer(), "https://auth.acme.com");
    let msg = err(&[("CHELABASE_PUBLIC_SCHEME", "ftp")]);
    assert!(msg.contains("CHELABASE_PUBLIC_SCHEME"), "{msg}");
}

#[test]
fn each_required_host_is_named_when_missing_or_blank() {
    for name in ["CHELABASE_STUDIO_HOST", "CHELABASE_API_HOST", "CHELABASE_AUTH_HOST"] {
        for value in ["-", "  "] {
            let msg = err(&[(name, value)]);
            assert!(msg.contains(name), "{name}={value:?}: {msg}");
        }
    }
}

#[test]
fn invalid_hosts_are_refused_with_their_variable() {
    for bad in ["http://studio.chelabase.localhost", "studio chelabase.localhost", "studio/x", "::1", "[::1", "studio.chelabase.localhost:http"] {
        let msg = err(&[("CHELABASE_STUDIO_HOST", bad)]);
        assert!(msg.contains("CHELABASE_STUDIO_HOST"), "{bad}: {msg}");
    }
}

#[test]
fn hosts_are_normalized_and_keep_their_port() {
    let e = Entry::from_lookup(env(&[
        ("CHELABASE_STUDIO_HOST", "Studio.Chelabase.LocalHost.:8000"),
        ("CHELABASE_DOCS_HOST", "docs.chelabase.localhost:8000"),
        ("CHELABASE_LANDING_HOST", " "),
        ("CHELABASE_MAIL_HOST", "mail.chelabase.localhost:8000"),
    ]))
    .unwrap();
    assert_eq!(e.studio_host(), "studio.chelabase.localhost:8000");
    assert_eq!(e.studio_origin(), "http://studio.chelabase.localhost:8000");
    assert_eq!(e.docs_host(), Some("docs.chelabase.localhost:8000"));
    assert_eq!(e.landing_host(), None);
    assert_eq!(e.mail_host(), Some("mail.chelabase.localhost:8000"));
}

#[test]
fn removed_variables_are_refused_with_their_replacement() {
    for (old, new) in [
        ("CHELABASE_API_HOSTS", "CHELABASE_API_HOST"),
        ("CHELABASE_STUDIO_ORIGINS", "CHELABASE_STUDIO_HOST"),
        ("CHELABASE_STUDIO_URL", "CHELABASE_STUDIO_HOST"),
    ] {
        let msg = err(&[(old, "http://old.example")]);
        assert!(msg.contains(old) && msg.contains(new), "{old}: {msg}");
        // Blank counts as unset.
        assert!(Entry::from_lookup(env(&[(old, " ")])).is_ok(), "{old} blank");
    }
}

#[test]
fn an_explicit_issuer_must_equal_the_derived_one() {
    assert!(Entry::from_lookup(env(&[("CHELABASE_ZITADEL_ISSUER", "http://auth.chelabase.localhost:8000")])).is_ok());
    assert!(Entry::from_lookup(env(&[("CHELABASE_ZITADEL_ISSUER", "http://auth.chelabase.localhost:8000/")])).is_ok());
    let msg = err(&[("CHELABASE_ZITADEL_ISSUER", "http://old-issuer.example")]);
    assert!(msg.contains("CHELABASE_ZITADEL_ISSUER") && msg.contains("CHELABASE_AUTH_HOST"), "{msg}");
}

#[test]
fn identical_hosts_are_refused() {
    let msg = err(&[("CHELABASE_AUTH_HOST", "API.chelabase.localhost:8000")]);
    assert!(msg.contains("CHELABASE_AUTH_HOST") && msg.contains("CHELABASE_API_HOST"), "{msg}");
    // A different port is a different host (P1).
    assert!(Entry::from_lookup(env(&[("CHELABASE_AUTH_HOST", "api.chelabase.localhost:9000")])).is_ok());
}

#[test]
fn studio_and_the_api_must_be_one_site() {
    // Bare *.localhost names are each their own site: sign-in cookies (SameSite=Strict) would drop.
    let msg = err(&[("CHELABASE_STUDIO_HOST", "studio.localhost:8000"), ("CHELABASE_API_HOST", "api.localhost:8000")]);
    assert!(msg.contains("CHELABASE_STUDIO_HOST") && msg.contains("CHELABASE_API_HOST"), "{msg}");
    let msg = err(&[("CHELABASE_API_HOST", "api.other.localhost:8000")]);
    assert!(msg.contains("CHELABASE_API_HOST"), "{msg}");
    // Production shape, and IP literals that are the same address.
    assert!(Entry::from_lookup(env(&[("CHELABASE_STUDIO_HOST", "studio.acme.com"), ("CHELABASE_API_HOST", "api.acme.com")])).is_ok());
    assert!(Entry::from_lookup(env(&[("CHELABASE_STUDIO_HOST", "127.0.0.1:3999"), ("CHELABASE_API_HOST", "127.0.0.1")])).is_ok());
    assert!(Entry::from_lookup(env(&[("CHELABASE_STUDIO_HOST", "127.0.0.2"), ("CHELABASE_API_HOST", "127.0.0.1")])).is_err());
    assert!(Entry::from_lookup(env(&[("CHELABASE_STUDIO_HOST", "studio.acme.com"), ("CHELABASE_API_HOST", "127.0.0.1")])).is_err());
}

#[test]
fn issuer_from_lookup_follows_p3() {
    let get = |pairs: &'static [(&'static str, &'static str)]| {
        move |k: &str| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| v.to_string())
    };
    assert_eq!(issuer_from_lookup(get(&[])).unwrap(), None);
    assert_eq!(
        issuer_from_lookup(get(&[("CHELABASE_PUBLIC_SCHEME", "http"), ("CHELABASE_AUTH_HOST", "auth.chelabase.localhost:8000")])).unwrap(),
        Some("http://auth.chelabase.localhost:8000".to_string())
    );
    let msg = issuer_from_lookup(get(&[("CHELABASE_ZITADEL_ISSUER", "http://old-issuer.example")])).unwrap_err().to_string();
    assert!(msg.contains("CHELABASE_AUTH_HOST"), "{msg}");
    let msg = issuer_from_lookup(get(&[
        ("CHELABASE_AUTH_HOST", "auth.acme.com"),
        ("CHELABASE_ZITADEL_ISSUER", "http://old-issuer.example"),
    ]))
    .unwrap_err()
    .to_string();
    assert!(msg.contains("CHELABASE_ZITADEL_ISSUER"), "{msg}");
}

#[test]
fn host_helpers_keep_the_gateway_rules() {
    assert_eq!(normalize_host("[::1]:8000"), "[::1]");
    assert_eq!(normalize_host("API.Example.com.:8000"), "api.example.com");
    assert_eq!(host_port("a.b:8000"), Some(8000));
    assert_eq!(host_port("[::1]:8000"), Some(8000));
    assert_eq!(host_port("[::1]"), None);
    assert_eq!(host_port("a.b"), None);
    for good in ["api.example.com", "API.example.com:443", "[::1]", "127.0.0.1"] {
        assert!(validate_host(good).is_ok(), "{good}");
    }
    let msg = validate_host("::1").unwrap_err();
    assert!(msg.contains("bracket"), "{msg}");
}
```

**Gotchas:**
- The P2 parent comparison uses the normalized names without ports.
- IP literals are IPv4 or bracketed IPv6.
- Comparing the issuer ignores one trailing `/`.

**Verify:** `cargo nextest run -p chelabase-types` and `cargo clippy -p chelabase-types --all-targets`.

---

## T2: the gateway (opus / opus)

**Files:**
- implementation: `services/gateway/src/{config.rs,registry.rs,upstreams.rs,pipeline.rs,proxy.rs,main.rs}`, plus `Cargo.toml` if `chelabase-types` needs no change (it's already a dependency);
- tests (coordinator): the test modules of `config.rs`, `registry.rs`, `upstreams.rs` and `pipeline.rs`, and `tests/gateway.rs`.

**Behaviour:**
- Spec §2 with P1, P4, P5, P6 and P8.
- `main.rs` builds `Registry::from_entry(&config.entry)`, passes `entry.studio_origin()` to `Pipeline::new` and adds one TCP listener for each `config.listen` entry.
- `Pipeline::route`:
  - the health check comes first, as today;
  - no `Host` → 400 with an empty body;
  - an unknown host → 404 with an empty body (`body: None`);
  - `Target::Site(s)` → `Outcome::Site(s)` before any route, CORS or auth logic;
  - `Target::Api` → today's path.
- `proxy.rs`:
  - `Outcome::Site` sets a ctx target. `upstream_peer` uses `upstreams.site_addr(site)`; a site with no upstream gets a 502.
  - `upstream_request_filter` on a site leaves `Host` and every client header unchanged (including `Authorization`, cookies and `Upgrade`/`Connection`) and sets the P4 headers.
  - Websocket upgrades pass through.
  - A site's read timeout is 3600 s; `Api` keeps `upstream_timeout`.
  - `fail_to_proxy` on a site writes a `text/plain` body (`502 bad gateway\n`, or `504`). `Api` keeps JSON.
- `config.rs`:
  - `from_lookup`, with `from_env` calling it;
  - `listen` from `CHELABASE_GATEWAY_LISTEN` (comma list, default `127.0.0.1:8000`);
  - `entry` from `Entry::from_lookup`;
  - site upstreams per the interfaces;
  - `CHELABASE_GATEWAY_ADDR` refused.
- `registry.rs` loses `from_config` and the validation, which moved to `Entry`. `SharedRegistry` stays.

**RED tests (coordinator):**

1. `registry.rs`: the whole test module is replaced:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use chelabase_types::entry::Entry;

    #[test]
    fn an_empty_registry_matches_no_host() {
        let r = Registry::new(Vec::<String>::new());
        assert_eq!(r.target_for(Some("anything:8000")), None);
        assert_eq!(r.target_for(None), None);
    }

    #[test]
    fn a_host_without_a_port_matches_any_port_and_case() {
        let r = Registry::new(["api.example.com"]);
        assert_eq!(r.target_for(Some("API.Example.com:8000")), Some(Target::Api));
        assert_eq!(r.target_for(Some("api.example.com.")), Some(Target::Api));
        assert_eq!(r.target_for(Some("evil.example.com")), None);
        assert_eq!(r.target_for(None), None);
    }

    #[test]
    fn a_host_with_a_port_matches_only_that_port_and_wins() {
        let r = Registry::new(["127.0.0.1"]).with_site("127.0.0.1:3999", Site::Studio);
        assert_eq!(r.target_for(Some("127.0.0.1:3999")), Some(Target::Site(Site::Studio)));
        assert_eq!(r.target_for(Some("127.0.0.1:8000")), Some(Target::Api));
        let r = Registry::new(["api.chelabase.localhost:8000"]);
        assert_eq!(r.target_for(Some("api.chelabase.localhost:9000")), None);
        assert_eq!(r.target_for(Some("api.chelabase.localhost")), None);
    }

    #[test]
    fn ipv6_literals_keep_their_brackets() {
        let r = Registry::new(["[::1]"]);
        assert_eq!(r.target_for(Some("[::1]:8000")), Some(Target::Api));
    }

    #[test]
    fn from_entry_maps_every_configured_host() {
        let vars = [
            ("CHELABASE_PUBLIC_SCHEME", "http"),
            ("CHELABASE_STUDIO_HOST", "studio.chelabase.localhost:8000"),
            ("CHELABASE_API_HOST", "api.chelabase.localhost:8000"),
            ("CHELABASE_AUTH_HOST", "auth.chelabase.localhost:8000"),
            ("CHELABASE_DOCS_HOST", "docs.chelabase.localhost:8000"),
            ("CHELABASE_MAIL_HOST", "mail.chelabase.localhost:8000"),
        ];
        let entry = Entry::from_lookup(|k| vars.iter().find(|(n, _)| *n == k).map(|(_, v)| v.to_string())).unwrap();
        let r = Registry::from_entry(&entry);
        for (host, target) in [
            ("studio.chelabase.localhost:8000", Some(Target::Site(Site::Studio))),
            ("api.chelabase.localhost:8000", Some(Target::Api)),
            ("auth.chelabase.localhost:8000", Some(Target::Site(Site::Auth))),
            ("docs.chelabase.localhost:8000", Some(Target::Site(Site::Docs))),
            ("mail.chelabase.localhost:8000", Some(Target::Site(Site::Mail))),
            ("landing.chelabase.localhost:8000", None),
        ] {
            assert_eq!(r.target_for(Some(host)), target, "{host}");
        }
    }

    #[test]
    fn replace_takes_effect_for_the_next_load() {
        let shared = SharedRegistry::new(Registry::new(["a.example.com"]));
        assert_eq!(shared.load().target_for(Some("b.example.com")), None);
        shared.replace(Registry::new(["b.example.com"]));
        assert_eq!(shared.load().target_for(Some("b.example.com")), Some(Target::Api));
    }
}
```

2. `pipeline.rs` test module edits:
   - add `const API: &str = "api.test";`;
   - `STUDIO` becomes `"http://studio.test"`;
   - every `harness(&[])` becomes `harness(&[API])`;
   - every `run(&h.pipeline, <m>, <p>, None, …)` becomes `Some(API)`, except in `gateway_health_is_answered_before_host_checks`;
   - `pipeline_with` delegates to a new `pipeline_with_registry(registry: Registry, verifier: JwtVerifier)`, whose `Pipeline::new` takes `STUDIO.to_string()`;
   - delete `a_wildcard_origin_setting_never_allows_credentials` (P8);
   - `unknown_host_is_404_when_api_hosts_are_configured` is replaced by the first test below.

   New tests:

```rust
    #[tokio::test]
    async fn an_unknown_host_is_404_with_no_body() {
        let h = harness(&[API]).await;
        let resp = expect_response(run(&h.pipeline, "GET", "/health", Some("other.example.com"), &[]).await.outcome);
        assert_eq!(resp.status, 404);
        assert_eq!(resp.body, None);
    }

    #[tokio::test]
    async fn a_request_without_a_host_is_400() {
        let h = harness(&[API]).await;
        let resp = expect_response(run(&h.pipeline, "GET", "/health", None, &[]).await.outcome);
        assert_eq!(resp.status, 400);
    }

    async fn site_harness() -> Harness {
        let jwks = MockJwks::start().await;
        let registry = Registry::new([API])
            .with_site("studio.test", Site::Studio)
            .with_site("auth.test:8000", Site::Auth);
        let (pipeline, verify) = pipeline_with_registry(registry, JwtVerifier::new(jwks.config(vec![])));
        Harness { pipeline, jwks, verify }
    }

    #[tokio::test]
    async fn site_hosts_pass_through_before_routes_auth_and_cors() {
        let h = site_harness().await;
        for (host, site) in [("studio.test", Site::Studio), ("studio.test:8000", Site::Studio), ("auth.test:8000", Site::Auth)] {
            for method in ["GET", "POST", "OPTIONS"] {
                let handled = run(
                    &h.pipeline,
                    method,
                    "/v1/no-such-route",
                    Some(host),
                    &[("authorization", "Bearer not-a-jwt"), ("origin", STUDIO), ("access-control-request-method", "GET")],
                )
                .await;
                assert!(matches!(handled.outcome, Outcome::Site(s) if s == site), "{host} {method}");
            }
        }
        let resp = expect_response(run(&h.pipeline, "GET", "/", Some("auth.test:9000"), &[]).await.outcome);
        assert_eq!(resp.status, 404);
    }
```

3. `config.rs`: the test module is replaced (`timeout_secs_*` and `split_csv_*` kept as they are):

```rust
    use crate::registry::Site;

    const ENTRY: &[(&str, &str)] = &[
        ("CHELABASE_PUBLIC_SCHEME", "http"),
        ("CHELABASE_STUDIO_HOST", "studio.chelabase.localhost:8000"),
        ("CHELABASE_API_HOST", "api.chelabase.localhost:8000"),
        ("CHELABASE_AUTH_HOST", "auth.chelabase.localhost:8000"),
    ];

    fn with(extra: &[(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        let all: Vec<(&str, &str)> = ENTRY.iter().chain(extra.iter()).copied().collect();
        move |k: &str| all.iter().rev().find(|(n, _)| *n == k).map(|(_, v)| v.to_string())
    }

    #[test]
    fn debug_hides_the_tokens() {
        let config = GatewayConfig::from_lookup(with(&[
            ("CHELABASE_DATA_KEYS_TOKEN", "super-secret-token"),
            ("CHELABASE_APP_SESSIONS_INTERNAL_TOKEN", "sessions-secret"),
        ]))
        .unwrap();
        let shown = format!("{config:?}");
        assert!(!shown.contains("super-secret-token"), "{shown}");
        assert!(!shown.contains("sessions-secret"), "{shown}");
        assert!(shown.contains("data_keys_token"), "{shown}");
    }

    #[test]
    fn listeners_default_and_take_a_list() {
        assert_eq!(GatewayConfig::from_lookup(with(&[])).unwrap().listen, vec!["127.0.0.1:8000"]);
        let c = GatewayConfig::from_lookup(with(&[("CHELABASE_GATEWAY_LISTEN", "127.0.0.1:8000, [::1]:8000")])).unwrap();
        assert_eq!(c.listen, vec!["127.0.0.1:8000", "[::1]:8000"]);
    }

    #[test]
    fn removed_variables_are_refused_with_their_replacement() {
        for (old, new) in [
            ("CHELABASE_GATEWAY_ADDR", "CHELABASE_GATEWAY_LISTEN"),
            ("CHELABASE_API_HOSTS", "CHELABASE_API_HOST"),
            ("CHELABASE_STUDIO_ORIGINS", "CHELABASE_STUDIO_HOST"),
        ] {
            let msg = GatewayConfig::from_lookup(with(&[(old, "x")])).unwrap_err().to_string();
            assert!(msg.contains(new), "{old}: {msg}");
        }
    }

    #[test]
    fn site_upstreams_default_to_loopback_and_exist_only_with_their_host() {
        let c = GatewayConfig::from_lookup(with(&[])).unwrap();
        assert_eq!(c.upstreams.site_addr(Site::Studio), Some("127.0.0.1:3000"));
        assert_eq!(c.upstreams.site_addr(Site::Auth), Some("127.0.0.1:8080"));
        assert_eq!(c.upstreams.site_addr(Site::Docs), None);
        assert_eq!(c.upstreams.site_addr(Site::Mail), None);
        let c = GatewayConfig::from_lookup(with(&[
            ("CHELABASE_DOCS_HOST", "docs.chelabase.localhost:8000"),
            ("CHELABASE_MAIL_HOST", "mail.chelabase.localhost:8000"),
            ("CHELABASE_MAIL_UPSTREAM", "http://mailpit:8025"),
        ]))
        .unwrap();
        assert_eq!(c.upstreams.site_addr(Site::Docs), Some("127.0.0.1:3001"));
        assert_eq!(c.upstreams.site_addr(Site::Mail), Some("mailpit:8025"));
        assert_eq!(c.upstreams.site_addr(Site::Landing), None);
        assert_eq!(c.entry.studio_origin(), "http://studio.chelabase.localhost:8000");
    }
```

4. `tests/gateway.rs`:
   - `STUDIO` becomes `"http://127.0.0.1:3999"`, and a new `const STUDIO_HOST: &str = "127.0.0.1:3999";`.
   - In `spawn_gateway_with`:
     - `CHELABASE_GATEWAY_ADDR` → `CHELABASE_GATEWAY_LISTEN`;
     - `CHELABASE_API_HOSTS` and `CHELABASE_STUDIO_ORIGINS` are removed;
     - added: `.env("CHELABASE_PUBLIC_SCHEME", "http")`, `.env("CHELABASE_API_HOST", "127.0.0.1")`, `.env("CHELABASE_STUDIO_HOST", STUDIO_HOST)` and `.env("CHELABASE_AUTH_HOST", jwks.issuer().trim_start_matches("http://"))`;
     - `command.envs(extra_env…)` moves after the fixed `.env` calls, so a test can override them.
   - `incoming_forwarded_proto_is_preserved` is replaced by the first test below.
   - `use tokio::io::{AsyncReadExt, AsyncWriteExt};` is added.

   New tests:

```rust
// P4: forwarded headers are the gateway's own, never the client's.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forwarded_headers_are_replaced_not_trusted() {
    let stack = start_stack().await;
    let resp = reqwest::Client::new()
        .get(format!("{}/health", stack.base))
        .header("x-forwarded-proto", "https")
        .header("x-forwarded-for", "203.0.113.9")
        .header("x-forwarded-host", "evil.example")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let seen = stack.upstream_saw().unwrap();
    assert_eq!(seen["x-forwarded-proto"], "http");
    assert_eq!(seen["x-forwarded-for"], "127.0.0.1");
    assert_eq!(seen["x-forwarded-host"], stack.base.trim_start_matches("http://"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unknown_host_is_404_with_an_empty_body() {
    let stack = start_stack().await;
    let resp = reqwest::Client::new()
        .get(format!("{}/health", stack.base))
        .header(reqwest::header::HOST, "nope.example")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
    assert_eq!(resp.text().await.unwrap(), "");
    assert!(stack.upstream_saw().is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_auth_host_reaches_zitadel_with_host_and_bearer_unchanged() {
    let jwks = MockJwks::start().await;
    let (api, _) = spawn_upstream().await;
    let (zitadel, seen) = spawn_upstream().await;
    let (base, _verify, _gateway) =
        spawn_gateway_with(&api, &jwks, &[("CHELABASE_ZITADEL_UPSTREAM", zitadel.as_str())]).await;
    let auth_host = jwks.issuer().trim_start_matches("http://").to_string();
    let resp = reqwest::Client::new()
        .get(format!("{base}/management/v1/orgs/me"))
        .header(reqwest::header::HOST, &auth_host)
        .header("authorization", "Bearer zitadel-pat")
        .header("origin", STUDIO)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert!(resp.headers().get("access-control-allow-origin").is_none());
    let seen = seen.lock().unwrap().clone().expect("the Zitadel upstream was reached");
    assert_eq!(seen["host"], auth_host);
    assert_eq!(seen["authorization"], "Bearer zitadel-pat");
    assert_eq!(seen[":uri"], "/management/v1/orgs/me");
    assert!(!seen.contains_key(IDENTITY_HEADER));
    assert_eq!(seen["x-forwarded-proto"], "http");
    assert_eq!(seen["x-forwarded-host"], auth_host);
}

async fn read_head(sock: &mut tokio::net::TcpStream) -> String {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        sock.read_exact(&mut byte).await.unwrap();
        head.push(byte[0]);
    }
    String::from_utf8(head).unwrap()
}

/// A raw upstream that answers one websocket upgrade with 101, then echoes 4 bytes.
async fn spawn_upgrade_upstream() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let head = read_head(&mut sock).await.to_ascii_lowercase();
        assert!(head.contains("upgrade: websocket"), "{head}");
        sock.write_all(b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n")
            .await
            .unwrap();
        let mut buf = [0u8; 4];
        sock.read_exact(&mut buf).await.unwrap();
        sock.write_all(&buf).await.unwrap();
        tokio::time::sleep(Duration::from_secs(5)).await;
    });
    format!("http://{addr}")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_studio_host_passes_websocket_upgrades() {
    let jwks = MockJwks::start().await;
    let (api, _) = spawn_upstream().await;
    let studio = spawn_upgrade_upstream().await;
    let (base, _verify, _gateway) =
        spawn_gateway_with(&api, &jwks, &[("CHELABASE_STUDIO_UPSTREAM", studio.as_str())]).await;
    let port: u16 = base.rsplit(':').next().unwrap().parse().unwrap();
    let mut sock = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let request = format!(
        "GET /@vite/client HTTP/1.1\r\nHost: {STUDIO_HOST}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
         Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n"
    );
    sock.write_all(request.as_bytes()).await.unwrap();
    let head = tokio::time::timeout(Duration::from_secs(10), read_head(&mut sock)).await.unwrap();
    assert!(head.starts_with("HTTP/1.1 101"), "{head}");
    sock.write_all(b"ping").await.unwrap();
    let mut echo = [0u8; 4];
    tokio::time::timeout(Duration::from_secs(10), sock.read_exact(&mut echo)).await.unwrap().unwrap();
    assert_eq!(&echo, b"ping");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_site_upstream_down_is_502_plain_text() {
    let jwks = MockJwks::start().await;
    let (api, _) = spawn_upstream().await;
    let (down, _socket) = refused_upstream();
    let (base, _verify, _gateway) =
        spawn_gateway_with(&api, &jwks, &[("CHELABASE_STUDIO_UPSTREAM", down.as_str())]).await;
    let resp = reqwest::Client::new()
        .get(format!("{base}/"))
        .header(reqwest::header::HOST, STUDIO_HOST)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 502);
    let content_type = resp.headers()["content-type"].to_str().unwrap().to_string();
    assert!(content_type.starts_with("text/plain"), "{content_type}");
}
```

**Gotchas:**
- Pingora passes upgrades through when the response is 101. Don't strip `Upgrade` or `Connection` on sites.
- The `x-forwarded-*` writes must use `insert_header` (replace), not append.
- `ctx.host` is the raw `Host`.
- `wait_ready` in the e2e harness probes `/_gateway/health` with `Host: 127.0.0.1:<port>`, which matches `Api` (any port).

**Verify:** `cargo nextest run -p gateway` (unit and e2e) and `cargo clippy -p gateway --all-targets`.

---

## T3: services read `Entry` (sonnet / sonnet)

**Files:**
- `services/auth/src/config.rs`: Studio's URL comes from `Entry::from_env()?.studio_origin()`, and the issuer from `entry.issuer()`.
- `services/orchestrator/src/main.rs`: `studio_url` comes from `Entry`, and the admin-PAT issuer from `issuer_from_lookup`.
- `services/orchestrator/src/internal/config.rs`: `zitadel_issuer` comes from `issuer_from_lookup`.
- `services/worker/src/config.rs`: the same.
- `crates/chelabase-auth/src/lib.rs`: `ZitadelAuthConfig::from_lookup`, with the issuer from `issuer_from_lookup` (a missing issuer is a `Config` error naming `CHELABASE_AUTH_HOST`) and the JWKS URL from `CHELABASE_ZITADEL_JWKS_URL` or `{issuer}/oauth/v2/keys`.
- Tests (coordinator): `crates/chelabase-auth/tests/zitadel_config.rs`, plus the test modules of `services/worker/src/config.rs` and `services/orchestrator/src/internal/config.rs`.

**Behaviour:** spec §1 (derived URLs) and P3. Every `Entry`/`EntryError` failure stops startup with its message.

**RED tests (coordinator):**

`crates/chelabase-auth/tests/zitadel_config.rs`:

```rust
use chelabase_auth::ZitadelAuthConfig;

fn get(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
    move |k: &str| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| v.to_string())
}

#[test]
fn the_issuer_comes_from_the_auth_host() {
    let c = ZitadelAuthConfig::from_lookup(get(&[
        ("CHELABASE_PUBLIC_SCHEME", "http"),
        ("CHELABASE_AUTH_HOST", "auth.chelabase.localhost:8000"),
    ]))
    .unwrap();
    assert_eq!(c.issuer, "http://auth.chelabase.localhost:8000");
    assert_eq!(c.jwks_url, "http://auth.chelabase.localhost:8000/oauth/v2/keys");
    let c = ZitadelAuthConfig::from_lookup(get(&[
        ("CHELABASE_AUTH_HOST", "auth.acme.com"),
        ("CHELABASE_ZITADEL_JWKS_URL", "http://zitadel:8080/oauth/v2/keys"),
    ]))
    .unwrap();
    assert_eq!(c.issuer, "https://auth.acme.com");
    assert_eq!(c.jwks_url, "http://zitadel:8080/oauth/v2/keys");
}

#[test]
fn no_auth_host_or_a_lone_issuer_is_refused() {
    assert!(ZitadelAuthConfig::from_lookup(get(&[])).is_err());
    assert!(ZitadelAuthConfig::from_lookup(get(&[("CHELABASE_ZITADEL_ISSUER", "http://old-issuer.example")])).is_err());
}
```

`services/worker/src/config.rs` test module:
- In `config_reads_reconcile_interval`, the pair `("CHELABASE_ZITADEL_ISSUER", "http://localhost:8080")` becomes the two pairs `("CHELABASE_PUBLIC_SCHEME", "http"), ("CHELABASE_AUTH_HOST", "auth.chelabase.localhost:8000")`, and the assertion expects `Some("http://auth.chelabase.localhost:8000")`.
- In line :546, the literal becomes `"http://auth.chelabase.localhost:8000"`.
- New test:

```rust
    #[test]
    fn a_lone_zitadel_issuer_is_refused() {
        let err = WorkerConfig::from_lookup(lookup(&[IMAGE, ("CHELABASE_ZITADEL_ISSUER", "http://old-issuer.example")]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("CHELABASE_AUTH_HOST"), "{err}");
    }
```

`services/orchestrator/src/internal/config.rs` test module, new test:

```rust
    #[test]
    fn the_zitadel_issuer_comes_from_the_auth_host() {
        let c = InternalConfig::from_lookup(|name| match name {
            "CHELABASE_PUBLIC_SCHEME" => Some("http".into()),
            "CHELABASE_AUTH_HOST" => Some("auth.chelabase.localhost:8000".into()),
            _ => None,
        })
        .unwrap();
        assert_eq!(c.zitadel_issuer.as_deref(), Some("http://auth.chelabase.localhost:8000"));
        assert_eq!(InternalConfig::from_lookup(|_| None).unwrap().zitadel_issuer, None);
        let err = InternalConfig::from_lookup(|name| {
            (name == "CHELABASE_ZITADEL_ISSUER").then(|| "http://old-issuer.example".to_string())
        })
        .unwrap_err()
        .to_string();
        assert!(err.contains("CHELABASE_AUTH_HOST"), "{err}");
    }
```

**Gotchas:**
- The auth service and the orchestrator call `Entry::from_env()` once at startup. Tests that build their configs directly are unaffected.
- `JwtVerifier::from_env` goes through `ZitadelAuthConfig::from_env`.

**Verify:** `cargo nextest run -p chelabase-auth -p auth -p orchestrator -p worker` (non-ignored) and `cargo clippy -p chelabase-auth -p auth -p orchestrator -p worker --all-targets`. Then compile the live tests: `cargo nextest run -p orchestrator -p worker --no-run`.

---

## T4: CLI bootstrap (sonnet / sonnet)

**Files:**
- `tools/cli/src/{paths.rs,dev/bootstrap.rs,dev/requests.rs,dev/steps.rs,dev/env_files.rs,dev/token.rs}`;
- tests (coordinator): the new tests below, in the `requests.rs` and `env_files.rs` test modules.

**Behaviour:**
- Spec §3 (bootstrap).
- `paths::entry()` reads `Entry::from_env()`, and the CLI's `just dev` environment carries the host variables (T5).
- Studio's origin, the API base and the issuer all come from it.
- `ensure_studio_app`, through `studio_app_drift`, updates an app registered for another origin.
- The login v2 base URI is `<studio origin>/`.
- `render_studio_env` gains `VITE_STUDIO_HOST=<studio host>`, and `VITE_API_URL` is `entry.api_base()`.
- `token.rs` reads the issuer from `demo.env` as today, and `demo.env`'s issuer is `entry.issuer()`.
- Delete `bootstrap.rs`'s `studio_and_the_gateway_are_the_same_site` (P7).
- The existing tests follow P9: signatures, and `http://localhost:3000` / `:8080` / `127.0.0.1:8000` literals → the `chelabase.localhost` hosts.

**RED tests (coordinator):**

In `requests.rs` tests:

```rust
    #[test]
    fn the_studio_app_follows_the_studio_origin() {
        let origin = "https://studio.acme.com";
        let c = studio_app_config(Mode::SelfHosted, origin);
        assert_eq!(c["redirectUris"], json!(["https://studio.acme.com/callback"]));
        assert_eq!(c["postLogoutRedirectUris"], json!(["https://studio.acme.com/"]));
        assert_eq!(c["loginVersion"]["loginV2"]["baseUri"], "https://studio.acme.com/");
        assert_eq!(studio_app_create("p1", Mode::SelfHosted, origin)["oidcConfiguration"], c);
        assert_eq!(studio_app_update("p1", "a2", Mode::SelfHosted, origin)["oidcConfiguration"], c);
        // An app registered for another Studio origin drifts, so bootstrap updates it.
        let old = json!({"oidcConfiguration": studio_app_config(Mode::SelfHosted, "http://old-studio.example")});
        let drift = studio_app_drift(&old, Mode::SelfHosted, origin);
        assert!(drift.contains(&"redirectUris") && drift.contains(&"postLogoutRedirectUris"), "{drift:?}");
        let same = json!({"oidcConfiguration": c});
        assert!(studio_app_drift(&same, Mode::SelfHosted, origin).is_empty());
    }
```

In `env_files.rs` tests, `studio_env_has_every_key_studio_reads` is replaced by:

```rust
    #[test]
    fn studio_env_has_every_key_studio_reads() {
        let text = render_studio_env(
            "http://auth.chelabase.localhost:8000",
            "c",
            "p",
            "o",
            "http://api.chelabase.localhost:8000",
            "studio.chelabase.localhost:8000",
        );
        for key in [
            "VITE_ZITADEL_ISSUER=http://auth.chelabase.localhost:8000\n",
            "VITE_ZITADEL_CLIENT_ID=c\n",
            "VITE_ZITADEL_PROJECT_ID=p\n",
            "VITE_ZITADEL_ORG_ID=o\n",
            "VITE_API_URL=http://api.chelabase.localhost:8000\n",
            "VITE_STUDIO_HOST=studio.chelabase.localhost:8000\n",
        ] {
            assert!(text.contains(key), "{key}");
        }
    }
```

**Verify:** `cargo nextest run -p chelabase` (the CLI package) and `cargo clippy -p chelabase --all-targets`.

---

## T5: workbench wiring and the guard (sonnet / sonnet)

**Files:**
- `infra/compose/docker-compose.yml`: Zitadel's env from variables: `ZITADEL_EXTERNALDOMAIN=${CHELABASE_AUTH_HOST_NAME}`, `EXTERNALPORT`, `EXTERNALSECURE`, `LOGINV2_BASEURI` and the default login and logout URLs from Studio's origin;
- `infra/compose/.env.example`: the host settings and their workbench values, with the removed variables gone;
- `justfile`:
  - `dev` exports the host settings and `CHELABASE_GATEWAY_LISTEN=127.0.0.1:8000,[::1]:8000`;
  - `_live-swap-dev` polls `http://api.chelabase.localhost:8000/_gateway/health` and `http://studio.chelabase.localhost:8000/`;
  - `_cleanup` uses Mailpit through the mail host;
  - `create-db-demo` uses the API host;
  - `check` runs `scripts/tests/entry-urls.sh`;
- `apps/studio/vite.config.ts`, `apps/docs/vite.config.*` and `apps/landing/vite.config.*`: `server.allowedHosts` and `server.hmr { host, clientPort: 8000 }` from env, with the dev servers still bound to loopback;
- scripts:
  - a new `scripts/lib/entry.sh` that exports `CHELABASE_ENTRY_{STUDIO,API,AUTH,MAIL}_URL` from the host settings, with the workbench defaults;
  - `scripts/{auth,members,neon}-e2e.sh`, `scripts/neon-smoke.sh` and `scripts/lib/neon-common.sh` use it;
- Rust live-test defaults: `GATEWAY_URL` is `http://api.chelabase.localhost:8000` in `services/*/tests/*_live.rs`;
- the Playwright configs and specs, `packages/client/scripts/e2e.mjs` and `tools/bench-data-api`;
- `scripts/tests/entry-urls.sh` (coordinator).

**Behaviour:** spec §3 (compose), §4 and §5. Compose needs the auth host's name without its port: `just dev` and `.env.example` give `CHELABASE_AUTH_HOST_NAME` and `CHELABASE_PUBLIC_PORT` separately, and `just dev` checks that they agree with `CHELABASE_AUTH_HOST`. Mailpit's UI works at the mail host's root.

**RED test (coordinator):** `scripts/tests/entry-urls.sh`. It is RED today, because of the many old literals:

```bash
#!/usr/bin/env bash
# W1 guard (spec §4, S2): nothing but the gateway addresses Studio, Zitadel, Mailpit, docs or
# landing by their loopback ports; everything else uses the entry hosts (CHELABASE_*_HOST).
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"
pattern='(localhost|127\.0\.0\.1|\[::1\]):(3000|3001|3002|8080|8025)([^0-9]|$)'
allowed=(
  ':!services/gateway/src/config.rs'     # the gateway's upstream defaults
  ':!infra/compose/docker-compose.yml'   # loopback-bound ports the host gateway reaches (W2 unpublishes them)
  ':!docs/superpowers/**'                # specs, plans and notes: history
  ':!docs/roadmap/**'
  ':!docs/landing-highlights.md'
  ':!scripts/tests/entry-urls.sh'
)
if hits=$(git grep -nE "$pattern" -- . "${allowed[@]}"); then
  echo "entry-urls: use the entry hosts (CHELABASE_*_HOST), not loopback ports:" >&2
  echo "$hits" >&2
  exit 1
fi
echo "entry-urls: ok"
```

**Gotchas:**
- `git grep` respects tracked files only, so `.chelabase/` and `apps/studio/.env.local` aren't scanned.
- Vite's HMR `clientPort` must be the gateway's port, or hot reload connects to the dev server's own port.
- The Playwright `baseURL` and `CHELABASE_STUDIO_URL`-style env in the e2e scripts become `CHELABASE_ENTRY_STUDIO_URL`.

**Verify:**
- `scripts/tests/entry-urls.sh`;
- `just check`;
- `cargo nextest run -p orchestrator -p worker -p rest --no-run`;
- `pnpm --filter @chelabase/studio build` and `pnpm --filter @chelabase/studio test`.

The live runs are the coordinator's (End of step).

---

## T6: docs (sonnet, no review)

**Files:**
- `CONTRIBUTING.md`: the URLs, the port table and the workbench hosts;
- a new docs section `apps/docs/content/docs/self-hosting/` (`meta.json`, plus an entry in `apps/docs/content/docs/meta.json`) with `hosts.mdx`, which covers the three hosts, one wildcard or three DNS records, TLS at the gateway or a front proxy (`CHELABASE_PUBLIC_SCHEME`), why Zitadel needs its own host, and that Studio and the API must share a registrable domain that isn't a public suffix (P2);
- `infra/compose/.env.example` comments.

**Verify:** `pnpm --filter @chelabase/docs build`, and `scripts/tests/entry-urls.sh` still passes.

---

## End of step

1. Whole-branch review (opus) on `main..workbench-entry`.
2. Fix round: the coordinator writes a failing test for each finding first.
3. `just check`.
4. No fork change, so no gate and no image build.
5. Live pass, from `.worktrees/workbench-entry` through `just live` (then move the stack back):
   1. Restart the stack's apps with the new env. Zitadel restarts, its setup moves the external domain (S1), and `chelabase dev bootstrap` updates the Studio app.
   2. Check that `http://auth.chelabase.localhost:8000/.well-known/openid-configuration` gives the new issuer. If it doesn't, run `just zitadel-reset` and the bootstrap, and record that in the Status.
   3. Orchestrator `--run-ignored ignored-only`, `rest_live`, `just live worker-live`, `catalog_live`.
   4. The e2e runs: `auth-e2e`, `members-e2e`, `sql-editor-e2e`, `console-e2e`, `merge-e2e`, `role-e2e`, `branch-e2e`.
   5. A browser pass:
      - sign-in at `studio.chelabase.localhost:8000` through `auth.chelabase.localhost:8000`;
      - Zitadel's console;
      - Studio hot reload after editing a component;
      - Mailpit at `mail.chelabase.localhost:8000` during a sign-up with verification on;
      - docs and landing at their hosts.
6. Update `docs/landing-highlights.md` on `main`, the plan Status and the roadmap's "Resume here", then pause for the user.
