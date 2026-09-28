# Zitadel v4 and our own login Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Run a fresh Zitadel v4.19.1 instance with our own login (pages in Studio, a small Rust service `ui-auth` holding the login-client token), and rebuild `cheladb auth bootstrap` from scratch on Zitadel's v2 APIs.

**Architecture:** Zitadel redirects every sign-in to Studio's `/login?authRequest=…`. Studio's pages call `ui-auth` through the Pingora gateway (public routes, credentialed CORS); `ui-auth` talks to Zitadel's Session, User and OIDC v2 APIs with the `IAM_LOGIN_CLIENT` token, keeps step state in an encrypted HttpOnly cookie, and returns Zitadel's callback URL, which Studio follows to finish the ordinary OIDC code flow. The bootstrap is rewritten as small idempotent steps over a two-method client (REST and ConnectRPC).

**Tech Stack:** Rust (edition 2024): axum 0.8.9, reqwest 0.13.5, cheladb-secrets (AES-256-GCM), tokio. Studio: React 19, TanStack Router, Vite 8, vitest 5, TypeScript 7. Zitadel v4.19.1 (REST v2 paths; ConnectRPC JSON for `zitadel.project.v2` and `zitadel.application.v2`, which have no REST paths).

**Spec:** `docs/superpowers/specs/2026-09-28-zitadel-v4-and-own-login-design.md`

## Global Constraints

- Branch `zitadel-v4`. No push, no merge until the user says.
- Commit as `git commit`, message ending with a blank line and:
  ```
  Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
  Claude-Session: https://claude.ai/code/session_01KESSEqEbiuKm7pMwCPe2Wg
  ```
- **Never run `cargo fmt`.** Format only new files, one by one: `rustfmt --edition 2024 <file>`.
- Never run `just sweep-dbs`; never touch `.cheladb/secrets/secrets.key` or the gateway key pair.
- New Rust dependencies come from `[workspace.dependencies]` (`workspace = true`); add a new workspace pin at the **latest** crates.io release only if one is missing.
- Zitadel image: `ghcr.io/zitadel/zitadel:v4.19.1`, pinned by index digest. No `zitadel-login` container.
- Zitadel URLs: login `http://localhost:3000/login?authRequest=`, logout `http://localhost:3000/logout?post_logout_redirect=`, Login V2 base `http://localhost:3000/`.
- `ui-auth` listens on `CHELADB_UI_AUTH_ADDR`, default `127.0.0.1:8084`. The gateway reaches it at `CHELADB_UI_AUTH_URL`, default `http://127.0.0.1:8084`.
- Cookies: `cheladb_login` (`Max-Age=900`) and `cheladb_session` (`Max-Age=2592000`), both `HttpOnly; SameSite=Strict; Path=/auth/v1`, plus `Secure` when `CHELADB_UI_AUTH_SECURE_COOKIES=true`.
- Error bodies, exact strings:
  - `401 {"error":"Email or password is incorrect"}`
  - `400 {"error":"Code is invalid or expired"}`
  - `400 {"error":"Use at least 8 characters, with upper- and lower-case letters, a number and a symbol"}`
  - `410 {"error":"This sign-in has expired, start again"}`
  - `429 {"error":"Too many attempts, try again later"}`
  - `503 {"error":"Sign-in is unavailable, try again"}`
- Rate limits (per window): sign-in 10/min per IP, 5/min per email; sign-up, password-reset, resend-code 20/hour per IP, 5/hour per email; verify-email and password-reset/confirm 10/min per IP. The client IP is the **rightmost** `X-Forwarded-For` entry.
- Sign-in and sign-up answers take at least `CHELADB_UI_AUTH_MIN_RESPONSE_MS` (default 500).
- Never log passwords, codes, tokens or cookie contents; log emails only as `email_tag(email)`.

## Review Focus

1. **Real and decoy sign-up cookies differ in length** → an observer tells taken emails from new ones. Expected: sealed login cookies are padded to the same length. Test in Task 5 (`decoy_and_real_login_cookies_have_the_same_length`).
2. **A client sends its own `X-Forwarded-For: 1.2.3.4`** to dodge rate limits. Expected: the rightmost entry (the gateway's) is used. Test in Task 5 (`client_ip_is_the_rightmost_forwarded_entry`).
3. **Password-reset confirm with a valid code but a weak password** → user told the code is bad and starts over. Expected: the weak-password message. Test in Task 8 (`confirm_with_a_weak_password_says_so`).
4. **`/logout?post_logout_redirect=https://evil.example`** → open redirect. Expected: Studio goes to `/`. Test in Task 10 (`logout_ignores_a_foreign_redirect`).
5. **Verification codes brute-forced** through `verify-email`. Expected: `429` after 10 attempts per minute per IP. Test in Task 7 (`verify_email_is_rate_limited`).

---

### Task 1: Zitadel v4.19.1 in Compose; remove `packages/zitadel`

**Files:**
- Modify: `infra/compose/docker-compose.yml` (the `zitadel` service), `scripts/zitadel-reset.sh`, `justfile` (`zitadel-generate`, `check`), `apps/studio/package.json`, `README.md`, `infra/neon/tests/run.sh`
- Delete: `packages/zitadel/` (whole directory)

**Interfaces:**
- Produces: a fresh v4 instance that writes `.cheladb/zitadel/admin.pat` and `.cheladb/zitadel/login-client.pat`.

- [ ] **Step 1: Write the failing offline check.** In `infra/neon/tests/run.sh`, inside the `if command -v docker` block, after the loopback check, add:

```bash
  zitadel="$("${compose[@]}" config --format json | jq -r '.services.zitadel')"
  check "zitadel is v4.19.1, pinned by digest" \
    grep -Eq '"ghcr\.io/zitadel/zitadel:v4\.19\.1@sha256:[0-9a-f]{64}"' <<<"$zitadel"
  check "zitadel sends sign-ins to Studio's /login" \
    test "$(jq -r '.environment.ZITADEL_OIDC_DEFAULTLOGINURLV2' <<<"$zitadel")" = "http://localhost:3000/login?authRequest="
  check "zitadel writes the login-client token" \
    test "$(jq -r '.environment.ZITADEL_FIRSTINSTANCE_LOGINCLIENTPATPATH' <<<"$zitadel")" = "/zitadel-bootstrap/login-client.pat"
```

- [ ] **Step 2: Run it; expect 3 FAILs.** `just neon-test` → `FAIL zitadel is v4.19.1…`, `FAIL zitadel sends…`, `FAIL zitadel writes…`.

- [ ] **Step 3: Get the digest.** `docker buildx imagetools inspect ghcr.io/zitadel/zitadel:v4.19.1 | awk '/^Digest:/{print $2}'`

- [ ] **Step 4: Update the `zitadel` service.** Replace the image line with `ghcr.io/zitadel/zitadel:v4.19.1@<digest>`; replace the comment above it with `# Zitadel v4: sign-in pages are Studio's own (/login …), served by ui-auth. FirstInstance writes admin.pat and login-client.pat once, on a fresh zitadel_pg_data volume.`; keep every existing environment variable and add:

```yaml
      ZITADEL_FIRSTINSTANCE_LOGINCLIENTPATPATH: /zitadel-bootstrap/login-client.pat
      ZITADEL_FIRSTINSTANCE_ORG_LOGINCLIENT_MACHINE_USERNAME: login-client
      ZITADEL_FIRSTINSTANCE_ORG_LOGINCLIENT_MACHINE_NAME: Cheladb ui-auth
      ZITADEL_FIRSTINSTANCE_ORG_LOGINCLIENT_PAT_EXPIRATIONDATE: "2099-01-01T00:00:00Z"
      ZITADEL_DEFAULTINSTANCE_FEATURES_LOGINV2_REQUIRED: "true"
      ZITADEL_DEFAULTINSTANCE_FEATURES_LOGINV2_BASEURI: http://localhost:3000/
      ZITADEL_OIDC_DEFAULTLOGINURLV2: "http://localhost:3000/login?authRequest="
      ZITADEL_OIDC_DEFAULTLOGOUTURLV2: "http://localhost:3000/logout?post_logout_redirect="
```

- [ ] **Step 5: Reset script.** In `scripts/zitadel-reset.sh`, change the `rm -f` line to also remove the login-client token:

```bash
rm -f "${ZDIR}/admin.pat" "${ZDIR}/login-client.pat" "${ZDIR}/demo.env" "${ZDIR}/demo-sa.json"
```

- [ ] **Step 6: Remove `packages/zitadel`.** `git rm -r packages/zitadel`; in `apps/studio/package.json` delete the `"@cheladb/zitadel": "workspace:*"` dependency; in `justfile` delete the `zitadel-generate` recipe and the `pnpm --filter @cheladb/zitadel typecheck` line of `check`; in `README.md` delete the `zitadel/ … (OpenAPI → orval, pin v2.71.19)` line. Then `pnpm install` (must end `✓ Lockfile passes supply-chain policies`).

- [ ] **Step 7: Checks pass.** `just neon-test` → `all passed`. `pnpm -r typecheck && pnpm --filter @cheladb/studio test` → pass.

- [ ] **Step 8: Live.** `just zitadel-reset`, then wait for readiness and check both tokens:

```bash
for i in $(seq 90); do curl -fsS http://localhost:8080/debug/ready >/dev/null && break; sleep 2; done
test -s .cheladb/zitadel/admin.pat && test -s .cheladb/zitadel/login-client.pat && echo tokens-ok
curl -fsS http://localhost:8080/.well-known/openid-configuration | jq -r .issuer
```
Expected: `tokens-ok` and `http://localhost:8080`.

- [ ] **Step 9: Commit** (`infra`, `scripts`, `justfile`, `apps/studio/package.json`, `pnpm-lock.yaml`, `README.md`, removed `packages/zitadel`): `zitadel: v4.19.1 with Studio login URLs; drop packages/zitadel`.

---

### Task 2: Bootstrap foundation: the Zitadel client and pure request builders

The old bootstrap is removed in Task 3; this task adds its replacement's building blocks.

**Files:**
- Create: `apps/cheladb/src/auth/zitadel.rs` (client), `apps/cheladb/src/auth/requests.rs` (pure builders and parsers)
- Modify: `apps/cheladb/src/auth/mod.rs`, `apps/cheladb/src/auth/secrets_key.rs`

**Interfaces:**
- Produces:
  - `trait ZitadelApi { async fn rest(&self, method: reqwest::Method, path: &str, body: Value) -> Result<Value>; async fn connect(&self, service: &str, method: &str, body: Value) -> Result<Value>; }` (`Send + Sync`; methods return `impl Future + Send`)
  - `struct HttpZitadel { http: reqwest::Client, issuer: String, pat: String }` with `HttpZitadel::new(issuer: &str, pat: &str) -> Self`
  - `requests::{default_org_query, org_id_from, project_search, project_create, project_id_from, apps_search, api_app_create, studio_app_config, studio_app_create, studio_app_update, find_app, studio_app_drift, machine_user_search, machine_user_create, user_id_from, key_create, key_file_from, smtp_payload, find_smtp}` (signatures in Step 3)
  - `secrets_key::ensure_file(dir: &Path, file_name: &str) -> Result<PathBuf>`; `secrets_key::ensure(dir)` becomes `ensure_file(dir, SECRETS_KEY_FILE)`.

- [ ] **Step 1: Write the failing tests** at the bottom of `requests.rs` (create the file with only the test module and `use super::*;` above an empty body so it compiles to "not found" errors):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_default_org_comes_from_a_default_query() {
        assert_eq!(default_org_query(), json!({"queries": [{"defaultQuery": {}}]}));
        assert_eq!(org_id_from(&json!({"result": [{"id": "o1"}]})).as_deref(), Some("o1"));
        assert_eq!(org_id_from(&json!({"result": []})), None);
    }

    #[test]
    fn projects_are_found_by_exact_name() {
        assert_eq!(
            project_search("cheladb"),
            json!({"filters": [{"projectNameFilter": {"projectName": "cheladb", "method": "TEXT_FILTER_METHOD_EQUALS"}}]})
        );
        assert_eq!(project_create("o1", "cheladb"), json!({"organizationId": "o1", "name": "cheladb"}));
        assert_eq!(project_id_from(&json!({"projects": [{"projectId": "p1"}]})).as_deref(), Some("p1"));
        assert_eq!(project_id_from(&json!({"projectId": "p2"})).as_deref(), Some("p2"));
    }

    #[test]
    fn apps_are_found_by_name_with_their_client_id() {
        let listed = json!({"applications": [
            {"applicationId": "a1", "name": "cheladb-orchestrator", "apiConfiguration": {"clientId": "c-api"}},
            {"applicationId": "a2", "name": "cheladb-studio", "oidcConfiguration": {"clientId": "c-spa", "redirectUris": ["x"]}}
        ]});
        let api = find_app(&listed, "cheladb-orchestrator").unwrap();
        assert_eq!((api.id.as_str(), api.client_id.as_str()), ("a1", "c-api"));
        let spa = find_app(&listed, "cheladb-studio").unwrap();
        assert_eq!(spa.client_id, "c-spa");
        assert!(find_app(&listed, "other").is_none());
    }

    #[test]
    fn the_studio_app_is_a_pkce_spa_with_jwt_access_tokens() {
        let c = studio_app_config("http://localhost:3000");
        assert_eq!(c["redirectUris"], json!(["http://localhost:3000/callback"]));
        assert_eq!(c["postLogoutRedirectUris"], json!(["http://localhost:3000/"]));
        assert_eq!(c["applicationType"], "OIDC_APP_TYPE_USER_AGENT");
        assert_eq!(c["authMethodType"], "OIDC_AUTH_METHOD_TYPE_NONE");
        assert_eq!(c["accessTokenType"], "OIDC_TOKEN_TYPE_JWT");
        assert_eq!(c["developmentMode"], true);
        assert_eq!(studio_app_create("p1", "http://localhost:3000")["oidcConfiguration"], c);
        assert_eq!(studio_app_update("p1", "a2", "http://localhost:3000")["applicationId"], "a2");
    }

    #[test]
    fn studio_app_drift_names_what_changed() {
        let good = json!({"oidcConfiguration": studio_app_config("http://localhost:3000")});
        assert!(studio_app_drift(&good, "http://localhost:3000").is_empty());
        let mut bad = good.clone();
        bad["oidcConfiguration"]["redirectUris"] = json!(["http://old/callback"]);
        bad["oidcConfiguration"]["accessTokenType"] = json!("OIDC_TOKEN_TYPE_BEARER");
        assert_eq!(studio_app_drift(&bad, "http://localhost:3000"), vec!["redirectUris", "accessTokenType"]);
    }

    #[test]
    fn the_demo_machine_user_and_its_key() {
        assert_eq!(
            machine_user_search("cheladb-demo"),
            json!({"queries": [{"userNameQuery": {"userName": "cheladb-demo", "method": "TEXT_QUERY_METHOD_EQUALS"}}]})
        );
        let create = machine_user_create("o1", "cheladb-demo");
        assert_eq!(create["username"], "cheladb-demo");
        assert_eq!(create["machine"]["accessTokenType"], "ACCESS_TOKEN_TYPE_JWT");
        assert_eq!(user_id_from(&json!({"result": [{"userId": "u1"}]})).as_deref(), Some("u1"));
        assert_eq!(user_id_from(&json!({"id": "u2"})).as_deref(), Some("u2"));
        assert_eq!(key_create(), json!({"expirationDate": "2099-01-01T00:00:00Z"}));
        use base64::Engine;
        let file = r#"{"type":"serviceaccount","keyId":"k1","key":"PEM","userId":"u1"}"#;
        let content = base64::engine::general_purpose::STANDARD.encode(file);
        assert_eq!(key_file_from(&json!({"keyId": "k1", "keyContent": content})).unwrap(), file);
    }

    #[test]
    fn mailpit_smtp_is_found_by_host() {
        let listed = json!({"result": [
            {"id": "s1", "host": "other:25", "state": "SMTP_CONFIG_ACTIVE"},
            {"id": "s2", "host": "mailpit:1025", "state": "SMTP_CONFIG_INACTIVE"}
        ]});
        assert_eq!(find_smtp(&listed, "mailpit:1025"), Some(("s2".to_string(), false)));
        assert_eq!(smtp_payload("mailpit:1025")["host"], "mailpit:1025");
        assert_eq!(smtp_payload("mailpit:1025")["password"], "cheladb");
    }
}
```

- [ ] **Step 2: Run; expect compile errors** (`cannot find function default_org_query`…): `cargo test -p cheladb requests`.

- [ ] **Step 3: Implement `requests.rs`** above the tests:

```rust
//! Pure request bodies and response parsers for the bootstrap's Zitadel v4 calls.
//! REST v2 for orgs and users; ConnectRPC JSON for projects and applications.

use anyhow::{Context, Result};
use base64::Engine;
use serde_json::{json, Value};

pub const KEY_EXPIRY: &str = "2099-01-01T00:00:00Z";

pub fn default_org_query() -> Value {
    json!({"queries": [{"defaultQuery": {}}]})
}

pub fn org_id_from(listed: &Value) -> Option<String> {
    listed.pointer("/result/0/id")?.as_str().map(str::to_string)
}

pub fn project_search(name: &str) -> Value {
    json!({"filters": [{"projectNameFilter": {"projectName": name, "method": "TEXT_FILTER_METHOD_EQUALS"}}]})
}

pub fn project_create(org_id: &str, name: &str) -> Value {
    json!({"organizationId": org_id, "name": name})
}

/// From a `ListProjects` result or a `CreateProject` response.
pub fn project_id_from(value: &Value) -> Option<String> {
    value
        .pointer("/projects/0/projectId")
        .or_else(|| value.get("projectId"))?
        .as_str()
        .map(str::to_string)
}

pub fn apps_search(project_id: &str) -> Value {
    json!({"filters": [{"projectIdFilter": {"projectId": project_id}}]})
}

pub fn api_app_create(project_id: &str, name: &str) -> Value {
    json!({"projectId": project_id, "name": name,
           "apiConfiguration": {"authMethodType": "API_AUTH_METHOD_TYPE_PRIVATE_KEY_JWT"}})
}

/// Studio: a public SPA using PKCE, issuing JWT access tokens the gateway verifies via JWKS.
/// `developmentMode` allows the plain-http localhost redirects.
pub fn studio_app_config(origin: &str) -> Value {
    json!({
        "redirectUris": [format!("{origin}/callback")],
        "postLogoutRedirectUris": [format!("{origin}/")],
        "responseTypes": ["OIDC_RESPONSE_TYPE_CODE"],
        "grantTypes": ["OIDC_GRANT_TYPE_AUTHORIZATION_CODE", "OIDC_GRANT_TYPE_REFRESH_TOKEN"],
        "applicationType": "OIDC_APP_TYPE_USER_AGENT",
        "authMethodType": "OIDC_AUTH_METHOD_TYPE_NONE",
        "version": "OIDC_VERSION_1_0",
        "developmentMode": true,
        "accessTokenType": "OIDC_TOKEN_TYPE_JWT",
        "idTokenUserinfoAssertion": true
    })
}

pub fn studio_app_create(project_id: &str, origin: &str) -> Value {
    json!({"projectId": project_id, "name": STUDIO_APP, "oidcConfiguration": studio_app_config(origin)})
}

pub fn studio_app_update(project_id: &str, app_id: &str, origin: &str) -> Value {
    json!({"projectId": project_id, "applicationId": app_id, "name": STUDIO_APP,
           "oidcConfiguration": studio_app_config(origin)})
}

pub const STUDIO_APP: &str = "cheladb-studio";
pub const API_APP: &str = "cheladb-orchestrator";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct App {
    pub id: String,
    pub client_id: String,
    pub raw: Value,
}

pub fn find_app(listed: &Value, name: &str) -> Option<App> {
    let app = listed
        .get("applications")?
        .as_array()?
        .iter()
        .find(|a| a.get("name").and_then(Value::as_str) == Some(name))?;
    let client_id = app
        .pointer("/oidcConfiguration/clientId")
        .or_else(|| app.pointer("/apiConfiguration/clientId"))?
        .as_str()?;
    Some(App {
        id: app.get("applicationId")?.as_str()?.to_string(),
        client_id: client_id.to_string(),
        raw: app.clone(),
    })
}

/// Fields of the existing Studio app that differ from what bootstrap wants.
pub fn studio_app_drift(app: &Value, origin: &str) -> Vec<&'static str> {
    let want = studio_app_config(origin);
    ["redirectUris", "postLogoutRedirectUris", "accessTokenType", "developmentMode"]
        .into_iter()
        .filter(|field| app.pointer(&format!("/oidcConfiguration/{field}")) != want.get(*field))
        .collect()
}

/// From a `CreateApplication` response.
pub fn created_client_id(value: &Value) -> Option<String> {
    value
        .pointer("/oidcConfiguration/clientId")
        .or_else(|| value.pointer("/apiConfiguration/clientId"))?
        .as_str()
        .map(str::to_string)
}

pub fn created_app_id(value: &Value) -> Option<String> {
    value.get("applicationId")?.as_str().map(str::to_string)
}

pub fn machine_user_search(username: &str) -> Value {
    json!({"queries": [{"userNameQuery": {"userName": username, "method": "TEXT_QUERY_METHOD_EQUALS"}}]})
}

pub fn machine_user_create(org_id: &str, username: &str) -> Value {
    json!({"organizationId": org_id, "username": username,
           "machine": {"name": "Cheladb Demo", "accessTokenType": "ACCESS_TOKEN_TYPE_JWT"}})
}

/// From a `ListUsers` result or a `CreateUser` response.
pub fn user_id_from(value: &Value) -> Option<String> {
    value
        .pointer("/result/0/userId")
        .or_else(|| value.get("id"))?
        .as_str()
        .map(str::to_string)
}

pub fn key_create() -> Value {
    json!({"expirationDate": KEY_EXPIRY})
}

/// The key file (`demo-sa.json`): `keyContent` is base64 of Zitadel's JSON key file.
pub fn key_file_from(created: &Value) -> Result<String> {
    let content = created
        .get("keyContent")
        .and_then(Value::as_str)
        .with_context(|| format!("AddKey response without keyContent: {created}"))?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(content)
        .context("keyContent is not base64")?;
    String::from_utf8(bytes).context("keyContent is not UTF-8")
}

/// Zitadel needs SMTP credentials even though Mailpit accepts any.
pub fn smtp_payload(host: &str) -> Value {
    json!({"senderAddress": "noreply@localhost", "senderName": "Cheladb (local)", "tls": false,
           "host": host, "user": "cheladb", "password": "cheladb", "description": "cheladb local mailpit"})
}

/// `(id, active)` of the SMTP configuration for `host`, from `/admin/v1/smtp/_search`.
pub fn find_smtp(listed: &Value, host: &str) -> Option<(String, bool)> {
    let config = listed
        .get("result")?
        .as_array()?
        .iter()
        .find(|c| c.get("host").and_then(Value::as_str) == Some(host))?;
    let active = config.get("state").and_then(Value::as_str) == Some("SMTP_CONFIG_ACTIVE");
    Some((config.get("id")?.as_str()?.to_string(), active))
}
```

- [ ] **Step 4: Implement `zitadel.rs`:**

```rust
//! The bootstrap's Zitadel client: REST v2 paths and ConnectRPC JSON, with the admin PAT.

use anyhow::{bail, Context, Result};
use reqwest::Method;
use serde_json::Value;
use std::future::Future;

pub trait ZitadelApi: Send + Sync {
    /// `method` `{issuer}{path}` with a JSON body; the JSON answer.
    fn rest(&self, method: Method, path: &str, body: Value) -> impl Future<Output = Result<Value>> + Send;
    /// ConnectRPC unary call: `POST {issuer}/{service}/{method}` with a JSON body.
    fn connect(&self, service: &str, method: &str, body: Value) -> impl Future<Output = Result<Value>> + Send;
}

pub struct HttpZitadel {
    http: reqwest::Client,
    issuer: String,
    pat: String,
}

impl HttpZitadel {
    pub fn new(issuer: &str, pat: &str) -> Self {
        Self {
            http: reqwest::Client::new(),
            issuer: issuer.trim_end_matches('/').to_string(),
            pat: pat.to_string(),
        }
    }

    async fn send(&self, request: reqwest::RequestBuilder, what: &str) -> Result<Value> {
        let response = request.bearer_auth(&self.pat).send().await.with_context(|| what.to_string())?;
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        if !status.is_success() {
            bail!("{what}: {status} {text}");
        }
        if text.trim().is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_str(&text).with_context(|| format!("{what}: not JSON: {text}"))
    }
}

impl ZitadelApi for HttpZitadel {
    async fn rest(&self, method: Method, path: &str, body: Value) -> Result<Value> {
        let what = format!("{method} {path}");
        let mut request = self.http.request(method.clone(), format!("{}{path}", self.issuer));
        if method != Method::GET {
            request = request.json(&body);
        }
        self.send(request, &what).await
    }

    async fn connect(&self, service: &str, method: &str, body: Value) -> Result<Value> {
        let request = self
            .http
            .post(format!("{}/{service}/{method}", self.issuer))
            .header("Connect-Protocol-Version", "1")
            .json(&body);
        self.send(request, &format!("{service}/{method}")).await
    }
}
```

- [ ] **Step 5: Generalize the key helper.** In `secrets_key.rs`, rename the body of `ensure` into `pub fn ensure_file(dir: &Path, file_name: &str) -> Result<PathBuf>` (same code, `join(file_name)`, log messages naming the file), and make `pub fn ensure(dir: &Path) -> Result<PathBuf> { ensure_file(dir, SECRETS_KEY_FILE) }`. Add a test:

```rust
    #[test]
    fn ensure_file_creates_a_named_key_once() {
        let dir = tempfile::tempdir().unwrap();
        let first = ensure_file(dir.path(), "cookie.key").unwrap();
        assert!(first.ends_with("cookie.key"));
        let before = std::fs::read(&first).unwrap();
        ensure_file(dir.path(), "cookie.key").unwrap();
        assert_eq!(std::fs::read(&first).unwrap(), before);
        assert!(cheladb_secrets::SecretKey::from_file(&first).is_ok());
    }
```

- [ ] **Step 6: Wire modules and run.** Add `pub mod requests; pub mod zitadel;` to `auth/mod.rs`. `cargo test -p cheladb` → all pass. `cargo clippy -p cheladb --all-targets` → clean (allow dead code only via real use in Task 3; if clippy flags unused items now, add `#[allow(dead_code)]` on the module declarations with a comment `// used by the rebuilt bootstrap (next commit)` and remove it in Task 3).

- [ ] **Step 7: Commit:** `cheladb: v4 bootstrap building blocks (client, request builders)`. Format the two new files with `rustfmt --edition 2024`.

---

### Task 3: Rebuild `cheladb auth bootstrap`

**Files:**
- Delete: `apps/cheladb/src/auth/bootstrap.rs` (old), `apps/cheladb/src/auth/dev_email.rs`, `apps/cheladb/src/auth/studio.rs`
- Create: `apps/cheladb/src/auth/bootstrap.rs` (new), `apps/cheladb/src/auth/steps.rs`, `apps/cheladb/src/auth/env_files.rs`, `apps/cheladb/src/auth/tenant.rs`
- Modify: `apps/cheladb/src/auth/mod.rs`, `apps/cheladb/src/paths.rs`, `infra/compose/.env.example`

**Interfaces:**
- Consumes: Task 2's `ZitadelApi`, `HttpZitadel`, `requests::*`, `secrets_key::ensure_file`.
- Produces: `steps::{ensure_org, ensure_project, ensure_api_app, ensure_studio_app, ensure_demo_user, ensure_demo_key, ensure_smtp}`; `paths::ui_auth_dir() -> PathBuf` (`CHELADB_UI_AUTH_DIR`, default `.cheladb/ui-auth`); `demo.env` gains `CHELADB_UI_AUTH_TOKEN_FILE` and `CHELADB_UI_AUTH_COOKIE_KEY`.

- [ ] **Step 1: Write the failing idempotency tests** in `steps.rs` (tests module; the fake records every call):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::requests::studio_app_config;
    use reqwest::Method;
    use serde_json::{json, Value};
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// Answers by call key (`"POST /v2/organizations/_search"` or `"Service/Method"`), records calls.
    #[derive(Default)]
    struct Fake {
        answers: HashMap<String, Value>,
        calls: Mutex<Vec<String>>,
    }

    impl Fake {
        fn with(mut self, key: &str, answer: Value) -> Self {
            self.answers.insert(key.to_string(), answer);
            self
        }
        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
        fn answer(&self, key: String) -> anyhow::Result<Value> {
            self.calls.lock().unwrap().push(key.clone());
            self.answers.get(&key).cloned().ok_or_else(|| anyhow::anyhow!("unexpected call {key}"))
        }
    }

    impl ZitadelApi for Fake {
        async fn rest(&self, method: Method, path: &str, _body: Value) -> anyhow::Result<Value> {
            self.answer(format!("{method} {path}"))
        }
        async fn connect(&self, service: &str, method: &str, _body: Value) -> anyhow::Result<Value> {
            self.answer(format!("{}/{method}", service.rsplit('.').next().unwrap()))
        }
    }

    const ORIGIN: &str = "http://localhost:3000";

    fn existing() -> Fake {
        Fake::default()
            .with("POST /v2/organizations/_search", json!({"result": [{"id": "o1"}]}))
            .with("ProjectService/ListProjects", json!({"projects": [{"projectId": "p1"}]}))
            .with("ApplicationService/ListApplications", json!({"applications": [
                {"applicationId": "a1", "name": "cheladb-orchestrator", "apiConfiguration": {"clientId": "c-api"}},
                {"applicationId": "a2", "name": "cheladb-studio", "oidcConfiguration": {
                    "clientId": "c-spa", "redirectUris": studio_app_config(ORIGIN)["redirectUris"],
                    "postLogoutRedirectUris": studio_app_config(ORIGIN)["postLogoutRedirectUris"],
                    "accessTokenType": "OIDC_TOKEN_TYPE_JWT", "developmentMode": true}}]}))
            .with("POST /v2/users", json!({"result": [{"userId": "u1"}]}))
            .with("POST /admin/v1/smtp/_search", json!({"result": [{"id": "s1", "host": "mailpit:1025", "state": "SMTP_CONFIG_ACTIVE"}]}))
    }

    #[tokio::test]
    async fn a_second_run_creates_nothing() {
        let fake = existing();
        assert_eq!(ensure_org(&fake).await.unwrap(), "o1");
        assert_eq!(ensure_project(&fake, "o1").await.unwrap(), "p1");
        assert_eq!(ensure_api_app(&fake, "p1").await.unwrap(), "c-api");
        assert_eq!(ensure_studio_app(&fake, "p1", ORIGIN).await.unwrap(), "c-spa");
        assert_eq!(ensure_demo_user(&fake, "o1").await.unwrap(), "u1");
        ensure_smtp(&fake).await.unwrap();
        let writes: Vec<String> = fake
            .calls()
            .into_iter()
            .filter(|c| !c.contains("_search") && !c.contains("/List") && c != "POST /v2/users")
            .collect();
        assert!(writes.is_empty(), "unexpected writes: {writes:?}");
    }

    #[tokio::test]
    async fn a_first_run_creates_everything() {
        let fake = Fake::default()
            .with("POST /v2/organizations/_search", json!({"result": [{"id": "o1"}]}))
            .with("ProjectService/ListProjects", json!({"projects": []}))
            .with("ProjectService/CreateProject", json!({"projectId": "p1"}))
            .with("ApplicationService/ListApplications", json!({"applications": []}))
            .with("ApplicationService/CreateApplication", json!({"applicationId": "a9", "apiConfiguration": {"clientId": "c-new"}}))
            .with("POST /v2/users", json!({"result": []}))
            .with("POST /v2/users/new", json!({"id": "u9"}))
            .with("POST /admin/v1/smtp/_search", json!({"result": []}))
            .with("POST /admin/v1/smtp", json!({"id": "s9"}))
            .with("POST /admin/v1/smtp/s9/_activate", json!({}));
        assert_eq!(ensure_project(&fake, "o1").await.unwrap(), "p1");
        assert_eq!(ensure_api_app(&fake, "p1").await.unwrap(), "c-new");
        assert_eq!(ensure_demo_user(&fake, "o1").await.unwrap(), "u9");
        ensure_smtp(&fake).await.unwrap();
        let calls = fake.calls();
        for created in ["ProjectService/CreateProject", "ApplicationService/CreateApplication", "POST /v2/users/new", "POST /admin/v1/smtp", "POST /admin/v1/smtp/s9/_activate"] {
            assert!(calls.iter().any(|c| c == created), "missing {created} in {calls:?}");
        }
    }

    #[tokio::test]
    async fn a_drifted_studio_app_is_updated_in_place() {
        let fake = existing()
            .with("ApplicationService/ListApplications", json!({"applications": [
                {"applicationId": "a2", "name": "cheladb-studio", "oidcConfiguration": {"clientId": "c-spa", "redirectUris": ["http://old/callback"]}}]}))
            .with("ApplicationService/UpdateApplication", json!({}));
        assert_eq!(ensure_studio_app(&fake, "p1", ORIGIN).await.unwrap(), "c-spa");
        assert!(fake.calls().contains(&"ApplicationService/UpdateApplication".to_string()));
    }

    #[tokio::test]
    async fn the_demo_key_is_kept_when_its_file_matches_the_user() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("demo-sa.json");
        std::fs::write(&path, r#"{"keyId":"k1","key":"PEM","userId":"u1"}"#).unwrap();
        let fake = Fake::default();
        ensure_demo_key(&fake, "u1", &path).await.unwrap();
        assert!(fake.calls().is_empty());
    }

    #[tokio::test]
    async fn a_missing_or_foreign_demo_key_is_replaced() {
        use base64::Engine;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("demo-sa.json");
        std::fs::write(&path, r#"{"keyId":"k0","key":"PEM","userId":"someone-else"}"#).unwrap();
        let file = r#"{"type":"serviceaccount","keyId":"k1","key":"PEM","userId":"u1"}"#;
        let fake = Fake::default().with(
            "POST /v2/users/u1/keys",
            json!({"keyId": "k1", "keyContent": base64::engine::general_purpose::STANDARD.encode(file)}),
        );
        ensure_demo_key(&fake, "u1", &path).await.unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), file);
    }
}
```

- [ ] **Step 2: Run; expect compile errors** (`cannot find function ensure_org`): `cargo test -p cheladb steps`.

- [ ] **Step 3: Implement `steps.rs`:**

```rust
//! Idempotent bootstrap steps: each finds what exists and creates only what's missing.

use crate::auth::requests::*;
use crate::auth::zitadel::ZitadelApi;
use anyhow::{Context, Result};
use reqwest::Method;
use serde_json::{json, Value};
use std::path::Path;
use tracing::info;

const PROJECT: &str = "cheladb";
const DEMO_USER: &str = "cheladb-demo";
const MAILPIT: &str = "mailpit:1025";
const PROJECTS: &str = "zitadel.project.v2.ProjectService";
const APPS: &str = "zitadel.application.v2.ApplicationService";

pub async fn ensure_org(api: &impl ZitadelApi) -> Result<String> {
    let listed = api.rest(Method::POST, "/v2/organizations/_search", default_org_query()).await?;
    org_id_from(&listed).with_context(|| format!("no default organization: {listed}"))
}

pub async fn ensure_project(api: &impl ZitadelApi, org_id: &str) -> Result<String> {
    let listed = api.connect(PROJECTS, "ListProjects", project_search(PROJECT)).await?;
    if let Some(id) = project_id_from(&listed) {
        return Ok(id);
    }
    let created = api.connect(PROJECTS, "CreateProject", project_create(org_id, PROJECT)).await?;
    info!("created project {PROJECT}");
    project_id_from(&created).with_context(|| format!("CreateProject: {created}"))
}

async fn list_apps(api: &impl ZitadelApi, project_id: &str) -> Result<Value> {
    api.connect(APPS, "ListApplications", apps_search(project_id)).await
}

/// The API app's client id (the gateway accepts it as a token audience).
pub async fn ensure_api_app(api: &impl ZitadelApi, project_id: &str) -> Result<String> {
    if let Some(app) = find_app(&list_apps(api, project_id).await?, API_APP) {
        return Ok(app.client_id);
    }
    let created = api.connect(APPS, "CreateApplication", api_app_create(project_id, API_APP)).await?;
    info!("created API app {API_APP}");
    created_client_id(&created).with_context(|| format!("CreateApplication: {created}"))
}

/// The Studio app's client id; its redirect settings are corrected if they drifted.
pub async fn ensure_studio_app(api: &impl ZitadelApi, project_id: &str, origin: &str) -> Result<String> {
    if let Some(app) = find_app(&list_apps(api, project_id).await?, STUDIO_APP) {
        let drift = studio_app_drift(&app.raw, origin);
        if !drift.is_empty() {
            api.connect(APPS, "UpdateApplication", studio_app_update(project_id, &app.id, origin)).await?;
            info!(?drift, "updated the Studio app");
        }
        return Ok(app.client_id);
    }
    let created = api.connect(APPS, "CreateApplication", studio_app_create(project_id, origin)).await?;
    info!("created Studio app {STUDIO_APP}");
    created_client_id(&created).with_context(|| format!("CreateApplication: {created}"))
}

pub async fn ensure_demo_user(api: &impl ZitadelApi, org_id: &str) -> Result<String> {
    let listed = api.rest(Method::POST, "/v2/users", machine_user_search(DEMO_USER)).await?;
    if let Some(id) = user_id_from(&listed) {
        return Ok(id);
    }
    let created = api.rest(Method::POST, "/v2/users/new", machine_user_create(org_id, DEMO_USER)).await?;
    info!("created machine user {DEMO_USER}");
    user_id_from(&created).with_context(|| format!("CreateUser: {created}"))
}

/// Keeps `path` if it is a key for `user_id`; otherwise adds a key and writes it (0600).
pub async fn ensure_demo_key(api: &impl ZitadelApi, user_id: &str, path: &Path) -> Result<()> {
    if let Ok(text) = std::fs::read_to_string(path)
        && serde_json::from_str::<Value>(&text).ok().and_then(|v| v.get("userId").cloned()) == Some(json!(user_id))
    {
        return Ok(());
    }
    let created = api.rest(Method::POST, &format!("/v2/users/{user_id}/keys"), key_create()).await?;
    let file = key_file_from(&created)?;
    write_private(path, &file)?;
    info!("wrote {}", path.display());
    Ok(())
}

pub async fn ensure_smtp(api: &impl ZitadelApi) -> Result<()> {
    let listed = api.rest(Method::POST, "/admin/v1/smtp/_search", json!({})).await?;
    let id = match find_smtp(&listed, MAILPIT) {
        Some((_, true)) => return Ok(()),
        Some((id, false)) => id,
        None => {
            let created = api.rest(Method::POST, "/admin/v1/smtp", smtp_payload(MAILPIT)).await?;
            created.get("id").and_then(Value::as_str).map(str::to_string)
                .with_context(|| format!("create SMTP: {created}"))?
        }
    };
    api.rest(Method::POST, &format!("/admin/v1/smtp/{id}/_activate"), json!({})).await?;
    info!("Mailpit SMTP active");
    Ok(())
}

fn write_private(path: &Path, contents: &str) -> Result<()> {
    std::fs::write(path, contents).with_context(|| format!("write {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}
```

- [ ] **Step 4: Run the step tests.** `cargo test -p cheladb steps` → 5 pass.

- [ ] **Step 5: Move the tenant binding.** Create `auth/tenant.rs` containing the old `bind_demo_tenant(org_id: &str, demo_tenant: &str) -> Result<()>` **unchanged** (copy it verbatim from the old `bootstrap.rs` with its `use` lines: `std::process::Command`, `anyhow::{Context, Result}`, `tracing::{info, warn}`, `crate::paths::{compose_env, compose_file}`), made `pub`.

- [ ] **Step 6: Env files, with tests.** Create `auth/env_files.rs`:

```rust
//! `demo.env` (host processes) and Studio's `.env.local`.

pub struct DemoEnv<'a> {
    pub issuer: &'a str,
    pub audiences: &'a str,
    pub org_id: &'a str,
    pub project_id: &'a str,
    pub api_client_id: &'a str,
    pub demo_user_id: &'a str,
    pub demo_tenant: &'a str,
    pub signing_key: &'a str,
    pub verify_key: &'a str,
    pub secrets_key: &'a str,
    pub ui_auth_token_file: &'a str,
    pub ui_auth_cookie_key: &'a str,
}

pub fn render_demo_env(e: &DemoEnv<'_>) -> String {
    format!(
        "CHELADB_ZITADEL_ISSUER={}\nCHELADB_ZITADEL_JWKS_URL={}/oauth/v2/keys\nCHELADB_ZITADEL_AUDIENCES={}\n\
CHELADB_ZITADEL_ORG_ID={}\nCHELADB_ZITADEL_PROJECT_ID={}\nCHELADB_ZITADEL_API_CLIENT_ID={}\n\
CHELADB_DEMO_USER_ID={}\nCHELADB_DEMO_TENANT_ID={}\nCHELADB_INTERNAL_SIGNING_KEY={}\n\
CHELADB_INTERNAL_VERIFY_KEY={}\nCHELADB_SECRETS_KEY={}\nCHELADB_UI_AUTH_TOKEN_FILE={}\n\
CHELADB_UI_AUTH_COOKIE_KEY={}\n",
        e.issuer, e.issuer, e.audiences, e.org_id, e.project_id, e.api_client_id, e.demo_user_id,
        e.demo_tenant, e.signing_key, e.verify_key, e.secrets_key, e.ui_auth_token_file, e.ui_auth_cookie_key
    )
}

pub fn render_studio_env(issuer: &str, client_id: &str, project_id: &str, org_id: &str, api_url: &str) -> String {
    format!(
        "# Written by `cheladb auth bootstrap`; regenerate instead of editing.\n\
VITE_ZITADEL_ISSUER={issuer}\nVITE_ZITADEL_CLIENT_ID={client_id}\nVITE_ZITADEL_PROJECT_ID={project_id}\n\
VITE_ZITADEL_ORG_ID={org_id}\nVITE_API_URL={api_url}\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn demo_env_names_the_ui_auth_files() {
        let text = render_demo_env(&DemoEnv {
            issuer: "http://localhost:8080", audiences: "p1,c1", org_id: "o1", project_id: "p1",
            api_client_id: "c1", demo_user_id: "u1", demo_tenant: "t1", signing_key: "/s",
            verify_key: "/v", secrets_key: "/k", ui_auth_token_file: "/z/login-client.pat",
            ui_auth_cookie_key: "/u/cookie.key",
        });
        assert!(text.contains("CHELADB_UI_AUTH_TOKEN_FILE=/z/login-client.pat\n"));
        assert!(text.contains("CHELADB_UI_AUTH_COOKIE_KEY=/u/cookie.key\n"));
        assert!(text.contains("CHELADB_ZITADEL_JWKS_URL=http://localhost:8080/oauth/v2/keys\n"));
    }

    #[test]
    fn studio_env_has_every_key_studio_reads() {
        let text = render_studio_env("http://localhost:8080", "c", "p", "o", "http://127.0.0.1:8000");
        for key in ["VITE_ZITADEL_ISSUER=", "VITE_ZITADEL_CLIENT_ID=c", "VITE_ZITADEL_PROJECT_ID=p",
                    "VITE_ZITADEL_ORG_ID=o", "VITE_API_URL=http://127.0.0.1:8000"] {
            assert!(text.contains(key), "{key}");
        }
    }
}
```

- [ ] **Step 7: The new `bootstrap.rs`:**

```rust
//! `cheladb auth bootstrap`: idempotent local IAM setup on a fresh Zitadel v4 instance.

use crate::auth::env_files::{render_demo_env, render_studio_env, DemoEnv};
use crate::auth::zitadel::HttpZitadel;
use crate::auth::{internal_keys, secrets_key, steps, tenant};
use crate::paths::{demo_tenant_id, gateway_dir, issuer, secrets_dir, studio_env_file, ui_auth_dir, zitadel_dir};
use anyhow::{bail, Context, Result};
use std::fs;
use std::path::Path;
use std::time::Duration;
use tracing::info;

const STUDIO_ORIGIN: &str = "http://localhost:3000";
const GATEWAY_URL: &str = "http://127.0.0.1:8000";

pub async fn run() -> Result<()> {
    let iss = issuer();
    let zdir = zitadel_dir();
    fs::create_dir_all(&zdir).with_context(|| format!("mkdir {}", zdir.display()))?;

    // Local files first, so they exist even if Zitadel isn't up.
    let internal = internal_keys::ensure(&gateway_dir())?;
    let secrets = secrets_key::ensure(&secrets_dir())?;
    let cookie_key = secrets_key::ensure_file(&ui_auth_dir(), "cookie.key")?;

    let admin_pat = wait_for_file(&zdir.join("admin.pat"), "admin.pat").await?;
    let login_client = zdir.join("login-client.pat");
    if !login_client.is_file() {
        bail!("missing {}: Zitadel writes it only on a fresh instance. Run: just zitadel-reset", login_client.display());
    }

    let api = HttpZitadel::new(&iss, &admin_pat);
    let org_id = wait_ready(|| steps::ensure_org(&api)).await?;
    let project_id = steps::ensure_project(&api, &org_id).await?;
    let api_client_id = steps::ensure_api_app(&api, &project_id).await?;
    let studio_client_id = steps::ensure_studio_app(&api, &project_id, STUDIO_ORIGIN).await?;
    let demo_user_id = steps::ensure_demo_user(&api, &org_id).await?;
    steps::ensure_demo_key(&api, &demo_user_id, &zdir.join("demo-sa.json")).await?;
    steps::ensure_smtp(&api).await?;

    let demo_tenant = demo_tenant_id();
    tenant::bind_demo_tenant(&org_id, &demo_tenant)?;

    let path = |p: &Path| p.display().to_string();
    let demo_env = zdir.join("demo.env");
    fs::write(&demo_env, render_demo_env(&DemoEnv {
        issuer: &iss,
        audiences: &format!("{project_id},{api_client_id}"),
        org_id: &org_id,
        project_id: &project_id,
        api_client_id: &api_client_id,
        demo_user_id: &demo_user_id,
        demo_tenant: &demo_tenant,
        signing_key: &path(&internal.signing),
        verify_key: &path(&internal.verify),
        secrets_key: &path(&secrets),
        ui_auth_token_file: &path(&login_client.canonicalize()?),
        ui_auth_cookie_key: &path(&cookie_key),
    }))
    .with_context(|| format!("write {}", demo_env.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&demo_env, fs::Permissions::from_mode(0o600))?;
    }
    fs::write(studio_env_file(), render_studio_env(&iss, &studio_client_id, &project_id, &org_id, GATEWAY_URL))?;
    info!("bootstrap complete (idempotent)");
    Ok(())
}

async fn wait_for_file(path: &Path, what: &str) -> Result<String> {
    for _ in 0..45 {
        if let Ok(text) = fs::read_to_string(path) {
            let token: String = text.chars().filter(|c| !c.is_whitespace()).collect();
            if !token.is_empty() {
                return Ok(token);
            }
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    bail!("missing {} ({what}): written only on a fresh Zitadel volume. Run: just zitadel-reset", path.display())
}

/// Zitadel answers JWKS before its APIs are ready; retry the first real call.
async fn wait_ready<F, Fut>(call: F) -> Result<String>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Result<String>>,
{
    let mut last = None;
    for _ in 0..60 {
        match call().await {
            Ok(value) => return Ok(value),
            Err(err) => last = Some(err),
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    Err(last.unwrap()).context("Zitadel API not ready")
}
```

- [ ] **Step 8: Paths, modules, cleanup.** In `paths.rs` add:

```rust
/// Where `cheladb auth bootstrap` keeps ui-auth's cookie key.
pub fn ui_auth_dir() -> PathBuf {
    if let Ok(dir) = env::var("CHELADB_UI_AUTH_DIR") {
        return PathBuf::from(dir);
    }
    repo_root().join(".cheladb/ui-auth")
}
```
`git rm apps/cheladb/src/auth/dev_email.rs apps/cheladb/src/auth/studio.rs`. Set `auth/mod.rs` to:

```rust
pub mod bootstrap;
pub mod env_files;
pub mod internal_keys;
pub mod requests;
pub mod secrets_key;
pub mod steps;
pub mod tenant;
pub mod token;
pub mod zitadel;
```
In `infra/compose/.env.example`, change the comment above `# CHELADB_DEV_SKIP_EMAIL_VERIFICATION=false` to say it is read by `ui-auth` (local dev only). Remove any `#[allow(dead_code)]` added in Task 2.

- [ ] **Step 9: Tests and clippy.** `cargo test -p cheladb` → pass; `cargo clippy --workspace --all-targets` → clean.

- [ ] **Step 10: Live, twice.** With the fresh instance from Task 1 and `just deps-up` running:

```bash
cargo run -q -p cheladb -- auth bootstrap 2>&1 | tail -3
cargo run -q -p cheladb -- auth bootstrap 2>&1 | grep -E "created|updated|wrote .*demo-sa" || echo "second run changed nothing"
grep -c "CHELADB_UI_AUTH_" .cheladb/zitadel/demo.env
tok=$(cargo run -q -p cheladb -- auth token); echo "token ${#tok} chars"
```
Expected: first run ends `bootstrap complete`; second prints `second run changed nothing`; `2`; a token of several hundred chars. If `/admin/v1/smtp` answers 404 on v4.19.1, stop and report (the spec allows v1 only where v2 is missing).

- [ ] **Step 11: Commit** (`apps/cheladb`, `infra/compose/.env.example`): `cheladb: rebuild auth bootstrap on Zitadel v4 (v2 APIs; no Actions)`.

---

### Task 4: Routes and gateway: the `UiAuth` service and credentialed CORS

**Files:**
- Modify: `crates/cheladb-routes/src/lib.rs`, `apps/gateway/src/pipeline.rs`, `apps/gateway/src/upstreams.rs`, `apps/gateway/src/config.rs`, `apps/gateway/src/main.rs` (only if it builds `Upstreams`)

**Interfaces:**
- Produces: `Service::UiAuth` (name `"ui-auth"`); `Cors::StudioCredentials`; `cheladb_routes::ui_auth::{AUTH_REQUEST, SIGN_IN, SIGN_UP, VERIFY_EMAIL, RESEND_CODE, PASSWORD_RESET, PASSWORD_RESET_CONFIRM, LOGOUT}` (all `POST`, `Auth::Public`, `Cors::StudioCredentials`, paths `/auth/v1/auth-request`, `/auth/v1/sign-in`, `/auth/v1/sign-up`, `/auth/v1/verify-email`, `/auth/v1/resend-code`, `/auth/v1/password-reset`, `/auth/v1/password-reset/confirm`, `/auth/v1/logout`); `Upstreams::new(orchestrator_url, ui_auth_url)`.

- [ ] **Step 1: Failing tests.** In `cheladb-routes` tests:

```rust
    #[test]
    fn ui_auth_endpoints_are_public_posts_with_credentialed_cors() {
        for ep in ENDPOINTS.iter().filter(|e| e.service == Service::UiAuth) {
            assert_eq!(ep.method, Method::Post, "{}", ep.path);
            assert_eq!(ep.policy.auth, Auth::Public, "{}", ep.path);
            assert_eq!(ep.policy.cors, Cors::StudioCredentials, "{}", ep.path);
            assert!(ep.path.starts_with("/auth/v1/"), "{}", ep.path);
        }
        assert_eq!(ENDPOINTS.iter().filter(|e| e.service == Service::UiAuth).count(), 8);
        assert!(matches!(match_endpoint(Scope::Platform, "POST", "/auth/v1/password-reset/confirm"),
            Match::Found(ep) if ep.name == "ui_auth.password_reset_confirm"));
    }
```
In gateway `pipeline.rs` tests:

```rust
    #[tokio::test]
    async fn ui_auth_allows_credentials_for_studio_only() {
        let h = harness(&[]).await;
        let plan = expect_plan(run(&h.pipeline, "POST", "/auth/v1/sign-in", None, &[("origin", STUDIO)]).await.outcome);
        assert_eq!(plan.endpoint.service, Service::UiAuth);
        assert_eq!(plan.identity, None);
        assert_eq!(header(&plan.response_headers, "access-control-allow-origin"), Some(STUDIO));
        assert_eq!(header(&plan.response_headers, "access-control-allow-credentials"), Some("true"));

        let other = expect_plan(run(&h.pipeline, "POST", "/auth/v1/sign-in", None, &[("origin", "http://evil.example")]).await.outcome);
        assert_eq!(header(&other.response_headers, "access-control-allow-origin"), None);
        assert_eq!(header(&other.response_headers, "access-control-allow-credentials"), None);
    }

    #[tokio::test]
    async fn ui_auth_preflight_allows_credentials() {
        let h = harness(&[]).await;
        let resp = expect_response(run(&h.pipeline, "OPTIONS", "/auth/v1/sign-in", None,
            &[("origin", STUDIO), ("access-control-request-method", "POST")]).await.outcome);
        assert_eq!(resp.status, 204);
        assert_eq!(header(&resp.headers, "access-control-allow-credentials"), Some("true"));
    }

    #[tokio::test]
    async fn a_wildcard_origin_setting_never_allows_credentials() {
        let jwks = MockJwks::start().await;
        let (private_pem, _) = generate_keypair_pem();
        let pipeline = Pipeline::new(
            Arc::new(SharedRegistry::new(Registry::new(&[]))),
            vec!["*".to_string()],
            JwtVerifier::new(jwks.config(vec![])),
            InternalSigner::from_pem(private_pem.as_bytes()).unwrap(),
        );
        let plan = expect_plan(run(&pipeline, "POST", "/auth/v1/sign-in", None, &[("origin", STUDIO)]).await.outcome);
        assert_eq!(header(&plan.response_headers, "access-control-allow-origin"), None);
        assert_eq!(header(&plan.response_headers, "access-control-allow-credentials"), None);
    }
```
In `upstreams.rs` tests:

```rust
    #[test]
    fn ui_auth_has_its_own_upstream() {
        let upstreams = Upstreams::new("http://127.0.0.1:8081", "http://127.0.0.1:8084").unwrap();
        assert_eq!(upstreams.addr(Service::UiAuth), "127.0.0.1:8084");
    }
```
(Update the existing `Upstreams::new(x)` calls in these tests to `Upstreams::new(x, "http://127.0.0.1:8084")`.)

- [ ] **Step 2: Run; expect compile errors** (`no variant UiAuth`): `cargo test -p cheladb-routes -p gateway`.

- [ ] **Step 3: Routes.** Add `UiAuth` to `Service` (`name()` → `"ui-auth"`); add `StudioCredentials` to `Cors` with doc `/// Callable from the Studio origins with cookies (credentials); never for a "*" setting.`; add a `pub mod ui_auth` with the eight endpoints, e.g.:

```rust
pub mod ui_auth {
    use super::{Auth, Cors, Endpoint, Method, Policy, Scope, Service};

    const POLICY: Policy = Policy { auth: Auth::Public, cors: Cors::StudioCredentials };

    const fn post(name: &'static str, path: &'static str) -> Endpoint {
        Endpoint { name, method: Method::Post, path, scope: Scope::Platform, service: Service::UiAuth, policy: POLICY }
    }

    pub const AUTH_REQUEST: Endpoint = post("ui_auth.auth_request", "/auth/v1/auth-request");
    pub const SIGN_IN: Endpoint = post("ui_auth.sign_in", "/auth/v1/sign-in");
    pub const SIGN_UP: Endpoint = post("ui_auth.sign_up", "/auth/v1/sign-up");
    pub const VERIFY_EMAIL: Endpoint = post("ui_auth.verify_email", "/auth/v1/verify-email");
    pub const RESEND_CODE: Endpoint = post("ui_auth.resend_code", "/auth/v1/resend-code");
    pub const PASSWORD_RESET: Endpoint = post("ui_auth.password_reset", "/auth/v1/password-reset");
    pub const PASSWORD_RESET_CONFIRM: Endpoint = post("ui_auth.password_reset_confirm", "/auth/v1/password-reset/confirm");
    pub const LOGOUT: Endpoint = post("ui_auth.logout", "/auth/v1/logout");
}
```
and append all eight to `ENDPOINTS`.

- [ ] **Step 4: Gateway CORS.** In `pipeline.rs`: in `preflight`, accept `Cors::Studio | Cors::StudioCredentials`; in both `preflight` and `cors_headers`, compute `let credentials = endpoint.policy.cors == Cors::StudioCredentials;` and allow the origin only if `self.origin_allowed(origin) && !(credentials && self.wildcard())`, where:

```rust
    /// Credentialed CORS needs an explicit origin list; a "*" entry disables it.
    fn wildcard(&self) -> bool {
        self.studio_origins.iter().any(|o| o == "*")
    }
```
When allowed and `credentials`, also push `("access-control-allow-credentials", "true".to_string())`. `cors_headers` returns early for `Cors::None` only.

- [ ] **Step 5: Upstreams and config.** `Upstreams { orchestrator: String, ui_auth: String }`; `new(orchestrator_url: &str, ui_auth_url: &str)`; `addr` matches `Service::UiAuth => &self.ui_auth`. In `config.rs`: `Upstreams::new(&env_or("CHELADB_ORCHESTRATOR_URL", "http://127.0.0.1:8081"), &env_or("CHELADB_UI_AUTH_URL", "http://127.0.0.1:8084"))`. Fix any other `Upstreams::new` call sites (`grep -rn "Upstreams::new" apps/gateway`).

- [ ] **Step 6: Run.** `cargo test -p cheladb-routes -p gateway` → pass (including the existing `declared_endpoints_are_unique_and_well_formed`). `cargo clippy --workspace --all-targets` → clean.

- [ ] **Step 7: Commit:** `routes, gateway: ui-auth service with credentialed Studio CORS`.

---

### Task 5: `apps/ui-auth`: crate, config, cookies and safeguards

**Files:**
- Create: `apps/ui-auth/Cargo.toml`, `apps/ui-auth/src/lib.rs`, `apps/ui-auth/src/config.rs`, `apps/ui-auth/src/cookie.rs`, `apps/ui-auth/src/guard.rs`
- Modify: root `Cargo.toml` (`members`)

**Interfaces:**
- Produces:
  - `config::Config { listen: String, issuer: String, token: String, cookie_key: SecretKey, org_id: String, studio_url: String, skip_email_verification: bool, secure_cookies: bool, min_response: Duration }`, `Config::from_env() -> anyhow::Result<Config>`
  - `cookie::{LOGIN_COOKIE, SESSION_COOKIE, LoginState, Step, SessionState, seal_login, open_login, seal_session, open_session, set_cookie, clear_cookie, read_cookie, now_secs, LOGIN_TTL_SECS, SESSION_TTL_SECS}`
  - `guard::{RateLimiter, client_ip, normalize_email, email_tag, pad}`

- [ ] **Step 1: Crate skeleton.** `apps/ui-auth/Cargo.toml`:

```toml
[package]
name = "ui-auth"
version.workspace = true
edition.workspace = true
license.workspace = true
description = "Cheladb's login backend: holds Zitadel's login-client token for Studio's sign-in pages"

[lib]
name = "ui_auth"
path = "src/lib.rs"

[[bin]]
name = "ui-auth"
path = "src/main.rs"

[dependencies]
anyhow = { workspace = true }
axum = { workspace = true }
cheladb-routes = { workspace = true }
cheladb-secrets = { workspace = true }
dotenvy = { workspace = true }
hex = { workspace = true }
http = { workspace = true }
reqwest = { workspace = true }
serde = { workspace = true }
serde_json = { workspace = true }
sha2 = { workspace = true }
thiserror = { workspace = true }
tokio = { workspace = true }
tracing = { workspace = true }
tracing-subscriber = { workspace = true }

[dev-dependencies]
tempfile = { workspace = true }
```
Add `"apps/ui-auth"` to workspace `members`. `src/lib.rs`: `pub mod config; pub mod cookie; pub mod guard;` (more modules come in Tasks 6–8). `src/main.rs`: `fn main() {}` for now (Task 8 replaces it).

- [ ] **Step 2: Failing tests for `cookie.rs`:**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use cheladb_secrets::SecretKey;

    fn state(decoy: bool) -> LoginState {
        LoginState {
            auth_request: "V2_1".into(),
            user_id: (!decoy).then(|| "3000000000000000001".into()),
            session_id: (!decoy).then(|| "3000000000000000002".into()),
            session_token: (!decoy).then(|| "x".repeat(90)),
            step: Step::Verify,
            decoy,
            exp: now_secs() + LOGIN_TTL_SECS,
        }
    }

    #[test]
    fn login_state_round_trips_and_is_not_plain_text() {
        let key = SecretKey::generate();
        let sealed = seal_login(&key, &state(false));
        assert!(!sealed.contains("V2_1") && !sealed.contains("xxxx"));
        assert_eq!(open_login(&key, &sealed, now_secs()), Some(state(false)));
    }

    #[test]
    fn tampered_expired_or_foreign_cookies_are_rejected() {
        let key = SecretKey::generate();
        let sealed = seal_login(&key, &state(false));
        let mut tampered = sealed.clone();
        tampered.replace_range(10..11, if &sealed[10..11] == "A" { "B" } else { "A" });
        assert_eq!(open_login(&key, &tampered, now_secs()), None);
        assert_eq!(open_login(&SecretKey::generate(), &sealed, now_secs()), None);
        assert_eq!(open_login(&key, &sealed, now_secs() + LOGIN_TTL_SECS + 1), None);
        // A session cookie can't be replayed as a login cookie.
        let session = seal_session(&key, &SessionState { session_id: "s".into(), session_token: "t".into(), exp: now_secs() + 60 });
        assert_eq!(open_login(&key, &session, now_secs()), None);
    }

    // Review Focus 1.
    #[test]
    fn decoy_and_real_login_cookies_have_the_same_length() {
        let key = SecretKey::generate();
        assert_eq!(seal_login(&key, &state(true)).len(), seal_login(&key, &state(false)).len());
    }

    #[test]
    fn cookie_attributes() {
        assert_eq!(
            set_cookie(LOGIN_COOKIE, "v", LOGIN_TTL_SECS, false),
            "cheladb_login=v; HttpOnly; SameSite=Strict; Path=/auth/v1; Max-Age=900"
        );
        assert!(set_cookie(SESSION_COOKIE, "v", SESSION_TTL_SECS, true).ends_with("; Max-Age=2592000; Secure"));
        assert_eq!(clear_cookie(LOGIN_COOKIE, false), "cheladb_login=; HttpOnly; SameSite=Strict; Path=/auth/v1; Max-Age=0");
    }

    #[test]
    fn reads_one_cookie_among_several() {
        let mut headers = http::HeaderMap::new();
        headers.append("cookie", "a=1; cheladb_login=abc=; b=2".parse().unwrap());
        headers.append("cookie", "cheladb_session=s".parse().unwrap());
        assert_eq!(read_cookie(&headers, LOGIN_COOKIE), Some("abc="));
        assert_eq!(read_cookie(&headers, SESSION_COOKIE), Some("s"));
        assert_eq!(read_cookie(&headers, "missing"), None);
    }
}
```

- [ ] **Step 3: Failing tests for `guard.rs`:**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    // Review Focus 2.
    #[test]
    fn client_ip_is_the_rightmost_forwarded_entry() {
        let mut headers = http::HeaderMap::new();
        headers.insert("x-forwarded-for", "1.2.3.4, 10.0.0.9".parse().unwrap());
        assert_eq!(client_ip(&headers), "10.0.0.9");
        assert_eq!(client_ip(&http::HeaderMap::new()), "unknown");
    }

    #[test]
    fn emails_are_trimmed_and_lowercased() {
        assert_eq!(normalize_email("  Ada@Example.COM "), "ada@example.com");
        assert_eq!(email_tag("ada@example.com").len(), 12);
        assert_ne!(email_tag("ada@example.com"), email_tag("bob@example.com"));
    }

    #[test]
    fn the_limiter_allows_up_to_the_limit_per_window() {
        let limiter = RateLimiter::default();
        for _ in 0..5 {
            assert!(limiter.allow("sign-in:email:a", 5, Duration::from_secs(60)));
        }
        assert!(!limiter.allow("sign-in:email:a", 5, Duration::from_secs(60)));
        assert!(limiter.allow("sign-in:email:b", 5, Duration::from_secs(60)));
        assert!(limiter.allow("short", 1, Duration::from_millis(1)));
        std::thread::sleep(Duration::from_millis(5));
        assert!(limiter.allow("short", 1, Duration::from_millis(1)), "a new window starts over");
    }

    #[tokio::test]
    async fn pad_waits_for_the_minimum() {
        let start = Instant::now();
        pad(start, Duration::from_millis(80)).await;
        assert!(start.elapsed() >= Duration::from_millis(80));
    }
}
```

- [ ] **Step 4: Run; expect compile errors:** `cargo test -p ui-auth`.

- [ ] **Step 5: Implement `cookie.rs`:**

```rust
//! The encrypted cookies: step state between requests (`cheladb_login`) and the finished
//! sign-in's Zitadel session, kept only to end it at logout (`cheladb_session`).

use cheladb_secrets::{open, seal, SecretKey};
use http::HeaderMap;
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

pub const LOGIN_COOKIE: &str = "cheladb_login";
pub const SESSION_COOKIE: &str = "cheladb_session";
pub const LOGIN_TTL_SECS: u64 = 900;
pub const SESSION_TTL_SECS: u64 = 30 * 24 * 3600;
const LOGIN_AAD: &str = "cheladb:ui-auth:login";
const SESSION_AAD: &str = "cheladb:ui-auth:session";
/// Plaintext is padded to this size so a decoy cookie looks like a real one.
const LOGIN_PLAINTEXT_LEN: usize = 640;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Step {
    Verify,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoginState {
    pub auth_request: String,
    pub user_id: Option<String>,
    pub session_id: Option<String>,
    pub session_token: Option<String>,
    pub step: Step,
    pub decoy: bool,
    pub exp: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionState {
    pub session_id: String,
    pub session_token: String,
    pub exp: u64,
}

pub fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

pub fn seal_login(key: &SecretKey, state: &LoginState) -> String {
    let mut text = serde_json::to_string(state).expect("serializable");
    // JSON ignores trailing whitespace, so padding with spaces keeps it parseable.
    let target = LOGIN_PLAINTEXT_LEN.max(text.len().div_ceil(64) * 64);
    text.extend(std::iter::repeat_n(' ', target - text.len()));
    seal(key, LOGIN_AAD, &text)
}

pub fn open_login(key: &SecretKey, sealed: &str, now: u64) -> Option<LoginState> {
    let state: LoginState = serde_json::from_str(open(key, LOGIN_AAD, sealed).ok()?.trim_end()).ok()?;
    (state.exp >= now).then_some(state)
}

pub fn seal_session(key: &SecretKey, state: &SessionState) -> String {
    seal(key, SESSION_AAD, &serde_json::to_string(state).expect("serializable"))
}

pub fn open_session(key: &SecretKey, sealed: &str, now: u64) -> Option<SessionState> {
    let state: SessionState = serde_json::from_str(&open(key, SESSION_AAD, sealed).ok()?).ok()?;
    (state.exp >= now).then_some(state)
}

pub fn set_cookie(name: &str, value: &str, max_age: u64, secure: bool) -> String {
    let secure = if secure { "; Secure" } else { "" };
    format!("{name}={value}; HttpOnly; SameSite=Strict; Path=/auth/v1; Max-Age={max_age}{secure}")
}

pub fn clear_cookie(name: &str, secure: bool) -> String {
    set_cookie(name, "", 0, secure)
}

pub fn read_cookie<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get_all("cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(n, _)| *n == name)
        .map(|(_, v)| v)
}
```

- [ ] **Step 6: Implement `guard.rs`:**

```rust
//! Rate limits, client IP, email normalization and response padding.

use http::HeaderMap;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Fixed-window counters in memory (one ui-auth process).
#[derive(Default)]
pub struct RateLimiter {
    windows: Mutex<HashMap<String, (Instant, u32)>>,
}

impl RateLimiter {
    /// Counts this attempt; false once `key` has used `limit` attempts in the current window.
    pub fn allow(&self, key: &str, limit: u32, window: Duration) -> bool {
        let now = Instant::now();
        let mut windows = self.windows.lock().expect("limiter lock");
        if windows.len() > 100_000 {
            windows.retain(|_, (start, _)| now.duration_since(*start) < Duration::from_secs(3600));
        }
        let entry = windows.entry(key.to_string()).or_insert((now, 0));
        if now.duration_since(entry.0) >= window {
            *entry = (now, 0);
        }
        entry.1 += 1;
        entry.1 <= limit
    }
}

/// The rightmost `X-Forwarded-For` entry: the one the gateway appended from the TCP peer.
/// Entries to its left come from the client and can be forged.
pub fn client_ip(headers: &HeaderMap) -> String {
    headers
        .get_all("x-forwarded-for")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .last()
        .unwrap_or("unknown")
        .to_string()
}

pub fn normalize_email(email: &str) -> String {
    email.trim().to_lowercase()
}

/// A short, stable tag for logs, so emails never appear in them.
pub fn email_tag(email: &str) -> String {
    hex::encode(&Sha256::digest(email.as_bytes())[..6])
}

/// Sleep until `min` has passed since `start`.
pub async fn pad(start: Instant, min: Duration) {
    tokio::time::sleep_until((start + min).into()).await;
}
```

- [ ] **Step 7: Implement `config.rs`:**

```rust
//! ui-auth settings from the environment (`demo.env` in local dev).

use anyhow::{Context, Result};
use cheladb_secrets::SecretKey;
use std::time::Duration;

#[derive(Clone)]
pub struct Config {
    pub listen: String,
    pub issuer: String,
    pub token: String,
    pub cookie_key: SecretKey,
    pub org_id: String,
    /// The first Studio origin; email links point at its pages.
    pub studio_url: String,
    pub skip_email_verification: bool,
    pub secure_cookies: bool,
    pub min_response: Duration,
}

fn var(name: &str) -> Result<String> {
    std::env::var(name).with_context(|| format!("{name} is not set; run `just zitadel-bootstrap` and load .cheladb/zitadel/demo.env"))
}

fn flag(name: &str) -> bool {
    matches!(std::env::var(name).map(|v| v.trim().to_ascii_lowercase()).as_deref(), Ok("1" | "true" | "yes" | "on"))
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let token_file = var("CHELADB_UI_AUTH_TOKEN_FILE")?;
        let token = std::fs::read_to_string(&token_file).with_context(|| format!("read {token_file}"))?;
        let cookie_key = SecretKey::from_file(std::path::Path::new(&var("CHELADB_UI_AUTH_COOKIE_KEY")?))
            .map_err(|e| anyhow::anyhow!("cookie key: {e}"))?;
        let studio_url = std::env::var("CHELADB_STUDIO_ORIGINS")
            .unwrap_or_else(|_| "http://localhost:3000".into())
            .split(',')
            .map(str::trim)
            .find(|o| !o.is_empty() && *o != "*")
            .unwrap_or("http://localhost:3000")
            .to_string();
        let min_ms: u64 = std::env::var("CHELADB_UI_AUTH_MIN_RESPONSE_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(500);
        Ok(Self {
            listen: std::env::var("CHELADB_UI_AUTH_ADDR").unwrap_or_else(|_| "127.0.0.1:8084".into()),
            issuer: var("CHELADB_ZITADEL_ISSUER")?,
            token: token.trim().to_string(),
            cookie_key,
            org_id: var("CHELADB_ZITADEL_ORG_ID")?,
            studio_url,
            skip_email_verification: flag("CHELADB_DEV_SKIP_EMAIL_VERIFICATION"),
            secure_cookies: flag("CHELADB_UI_AUTH_SECURE_COOKIES"),
            min_response: Duration::from_millis(min_ms),
        })
    }
}
```

- [ ] **Step 8: Run.** `cargo test -p ui-auth` → all pass. `cargo clippy -p ui-auth --all-targets` → clean. Format the new files.

- [ ] **Step 9: Commit:** `ui-auth: crate with encrypted cookies, rate limits and padding`.

---

### Task 6: `ui-auth`: the Zitadel client and a stateful mock Zitadel

**Files:**
- Create: `apps/ui-auth/src/zitadel.rs`, `apps/ui-auth/tests/support/mod.rs`, `apps/ui-auth/tests/zitadel_client.rs`
- Modify: `apps/ui-auth/src/lib.rs` (`pub mod zitadel;`)

**Interfaces:**
- Produces:
  - `zitadel::{Zitadel, ZError, Session, NewUser}`; `ZError::{Rejected { status: u16, message: String }, Unavailable(String)}`
  - `Zitadel::new(base: &str, token: &str) -> Zitadel`
  - `auth_request(&self, id: &str) -> Result<bool /*sign_up*/, ZError>`
  - `find_user_by_email(&self, org_id: &str, email: &str) -> Result<Option<String>, ZError>`
  - `email_verified(&self, user_id: &str) -> Result<bool, ZError>`
  - `create_session(&self, user_id: &str, password: &str) -> Result<Session, ZError>`
  - `delete_session(&self, session: &Session) -> Result<(), ZError>`
  - `finish(&self, auth_request: &str, session: &Session) -> Result<String /*callback url*/, ZError>`
  - `create_user(&self, user: &NewUser<'_>) -> Result<String, ZError>`
  - `verify_email(&self, user_id: &str, code: &str) -> Result<(), ZError>`
  - `resend_code(&self, user_id: &str, url_template: &str) -> Result<(), ZError>`
  - `send_password_reset(&self, user_id: &str, url_template: &str) -> Result<(), ZError>`
  - `set_password(&self, user_id: &str, code: &str, password: &str) -> Result<(), ZError>`
  - test support: `MockZitadel::start() -> MockZitadel` with `base`, `add_user(email, password, verified) -> String`, `lock(user_id)`, `add_auth_request(sign_up: bool) -> String`, `emails() -> Vec<(String user_id, String kind)>`, `set_down(bool)`, `session_count() -> usize`; codes: email verification `"123456"`, password reset `"654321"`; token `"test-login-client"`.

- [ ] **Step 1: The mock** (`tests/support/mod.rs`). It checks the bearer token, keeps users, sessions and auth requests in memory, and mimics Zitadel's status codes (409 duplicate email, 400 bad password or code with Zitadel's message ids, 412 locked, 404 unknown):

```rust
#![allow(dead_code)]
//! A stateful stand-in for Zitadel v4's Session, User and OIDC v2 REST APIs.

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

pub const TOKEN: &str = "test-login-client";
pub const EMAIL_CODE: &str = "123456";
pub const RESET_CODE: &str = "654321";

#[derive(Clone, Debug)]
pub struct User { pub id: String, pub email: String, pub password: String, pub verified: bool, pub locked: bool }

#[derive(Default)]
pub struct Inner {
    pub users: Vec<User>,
    pub sessions: HashMap<String, (String, String)>,
    pub auth_requests: HashMap<String, bool>,
    pub emails: Vec<(String, String)>,
    pub next: u64,
    pub down: bool,
}

#[derive(Clone)]
pub struct MockZitadel { pub base: String, pub inner: Arc<Mutex<Inner>> }

type Reply = (StatusCode, Json<Value>);
fn ok(v: Value) -> Reply { (StatusCode::OK, Json(v)) }
fn err(status: StatusCode, message: &str) -> Reply { (status, Json(json!({"code": status.as_u16(), "message": message}))) }

impl MockZitadel {
    pub async fn start() -> Self {
        let inner = Arc::new(Mutex::new(Inner::default()));
        let app = Router::new()
            .route("/v2/oidc/auth_requests/{id}", get(get_auth_request).post(create_callback))
            .route("/v2/users", post(list_users))
            .route("/v2/users/new", post(create_user))
            .route("/v2/users/{id}", get(get_user))
            .route("/v2/users/{id}/email/verify", post(verify_email))
            .route("/v2/users/{id}/email/resend", post(resend))
            .route("/v2/users/{id}/password_reset", post(password_reset))
            .route("/v2/users/{id}/password", post(set_password))
            .route("/v2/sessions", post(create_session))
            .route("/v2/sessions/{id}", delete(delete_session))
            .with_state(inner.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self { base: format!("http://{addr}"), inner }
    }

    fn id(inner: &mut Inner, prefix: &str) -> String { inner.next += 1; format!("{prefix}{}", inner.next) }

    pub fn add_user(&self, email: &str, password: &str, verified: bool) -> String {
        let mut inner = self.inner.lock().unwrap();
        let id = Self::id(&mut inner, "u");
        inner.users.push(User { id: id.clone(), email: email.into(), password: password.into(), verified, locked: false });
        id
    }
    pub fn lock(&self, user_id: &str) { self.inner.lock().unwrap().users.iter_mut().filter(|u| u.id == user_id).for_each(|u| u.locked = true); }
    pub fn add_auth_request(&self, sign_up: bool) -> String {
        let mut inner = self.inner.lock().unwrap();
        let id = Self::id(&mut inner, "V2_");
        inner.auth_requests.insert(id.clone(), sign_up);
        id
    }
    pub fn emails(&self) -> Vec<(String, String)> { self.inner.lock().unwrap().emails.clone() }
    pub fn set_down(&self, down: bool) { self.inner.lock().unwrap().down = down; }
    pub fn session_count(&self) -> usize { self.inner.lock().unwrap().sessions.len() }
    pub fn user(&self, email: &str) -> Option<User> { self.inner.lock().unwrap().users.iter().find(|u| u.email == email).cloned() }
}

type S = State<Arc<Mutex<Inner>>>;

fn guard(headers: &HeaderMap, inner: &Inner) -> Option<Reply> {
    if inner.down { return Some(err(StatusCode::SERVICE_UNAVAILABLE, "down")); }
    let auth = headers.get("authorization").and_then(|v| v.to_str().ok());
    (auth != Some(&format!("Bearer {TOKEN}"))).then(|| err(StatusCode::UNAUTHORIZED, "no token"))
}

async fn get_auth_request(State(s): S, headers: HeaderMap, Path(id): Path<String>) -> Reply {
    let inner = s.lock().unwrap();
    if let Some(e) = guard(&headers, &inner) { return e; }
    match inner.auth_requests.get(&id) {
        Some(sign_up) => ok(json!({"authRequest": {"id": id, "prompt": if *sign_up { json!(["PROMPT_CREATE"]) } else { json!([]) }}})),
        None => err(StatusCode::NOT_FOUND, "Errors.AuthRequest.NotFound"),
    }
}

async fn create_callback(State(s): S, headers: HeaderMap, Path(id): Path<String>, Json(b): Json<Value>) -> Reply {
    let mut inner = s.lock().unwrap();
    if let Some(e) = guard(&headers, &inner) { return e; }
    let sid = b.pointer("/session/sessionId").and_then(Value::as_str).unwrap_or("").to_string();
    let tok = b.pointer("/session/sessionToken").and_then(Value::as_str).unwrap_or("");
    let valid = inner.sessions.get(&sid).is_some_and(|(_, t)| t == tok);
    if !inner.auth_requests.contains_key(&id) || !valid { return err(StatusCode::NOT_FOUND, "Errors.AuthRequest.NotFound"); }
    inner.auth_requests.remove(&id);
    ok(json!({"callbackUrl": format!("http://localhost:3000/callback?code=code-{id}&state=s")}))
}

async fn list_users(State(s): S, headers: HeaderMap, Json(b): Json<Value>) -> Reply {
    let inner = s.lock().unwrap();
    if let Some(e) = guard(&headers, &inner) { return e; }
    let email = b.pointer("/queries/0/emailQuery/emailAddress").and_then(Value::as_str).unwrap_or("").to_lowercase();
    let result: Vec<Value> = inner.users.iter().filter(|u| u.email.to_lowercase() == email).map(|u| json!({"userId": u.id})).collect();
    ok(json!({"result": result}))
}

async fn get_user(State(s): S, headers: HeaderMap, Path(id): Path<String>) -> Reply {
    let inner = s.lock().unwrap();
    if let Some(e) = guard(&headers, &inner) { return e; }
    match inner.users.iter().find(|u| u.id == id) {
        Some(u) => ok(json!({"user": {"userId": u.id, "human": {"email": {"email": u.email, "isVerified": u.verified}}}})),
        None => err(StatusCode::NOT_FOUND, "Errors.User.NotFound"),
    }
}

async fn create_user(State(s): S, headers: HeaderMap, Json(b): Json<Value>) -> Reply {
    let mut inner = s.lock().unwrap();
    if let Some(e) = guard(&headers, &inner) { return e; }
    let email = b.pointer("/human/email/email").and_then(Value::as_str).unwrap_or("").to_string();
    let password = b.pointer("/human/password/password").and_then(Value::as_str).unwrap_or("").to_string();
    if inner.users.iter().any(|u| u.email.eq_ignore_ascii_case(&email)) { return err(StatusCode::CONFLICT, "Errors.User.AlreadyExists"); }
    if password.len() < 8 { return err(StatusCode::BAD_REQUEST, "Errors.User.PasswordComplexityPolicy.MinLength"); }
    let verified = b.pointer("/human/email/isVerified").and_then(Value::as_bool).unwrap_or(false);
    let id = MockZitadel::id(&mut inner, "u");
    if b.pointer("/human/email/sendCode").is_some() { inner.emails.push((id.clone(), "verify".into())); }
    inner.users.push(User { id: id.clone(), email, password, verified, locked: false });
    ok(json!({"id": id}))
}

async fn create_session(State(s): S, headers: HeaderMap, Json(b): Json<Value>) -> Reply {
    let mut inner = s.lock().unwrap();
    if let Some(e) = guard(&headers, &inner) { return e; }
    let uid = b.pointer("/checks/user/userId").and_then(Value::as_str).unwrap_or("");
    let pw = b.pointer("/checks/password/password").and_then(Value::as_str).unwrap_or("");
    let Some(user) = inner.users.iter().find(|u| u.id == uid).cloned() else { return err(StatusCode::NOT_FOUND, "Errors.User.NotFound"); };
    if user.locked { return err(StatusCode::PRECONDITION_FAILED, "Errors.User.Locked"); }
    if user.password != pw { return err(StatusCode::BAD_REQUEST, "Errors.User.Password.Invalid"); }
    let sid = MockZitadel::id(&mut inner, "s");
    let tok = format!("tok-{sid}");
    inner.sessions.insert(sid.clone(), (uid.to_string(), tok.clone()));
    ok(json!({"sessionId": sid, "sessionToken": tok}))
}

async fn delete_session(State(s): S, headers: HeaderMap, Path(id): Path<String>) -> Reply {
    let mut inner = s.lock().unwrap();
    if let Some(e) = guard(&headers, &inner) { return e; }
    inner.sessions.remove(&id);
    ok(json!({}))
}

async fn verify_email(State(s): S, headers: HeaderMap, Path(id): Path<String>, Json(b): Json<Value>) -> Reply {
    let mut inner = s.lock().unwrap();
    if let Some(e) = guard(&headers, &inner) { return e; }
    if b.get("verificationCode").and_then(Value::as_str) != Some(EMAIL_CODE) { return err(StatusCode::BAD_REQUEST, "Errors.User.Code.Invalid"); }
    inner.users.iter_mut().filter(|u| u.id == id).for_each(|u| u.verified = true);
    ok(json!({}))
}

async fn resend(State(s): S, headers: HeaderMap, Path(id): Path<String>) -> Reply {
    let mut inner = s.lock().unwrap();
    if let Some(e) = guard(&headers, &inner) { return e; }
    inner.emails.push((id, "verify".into()));
    ok(json!({}))
}

async fn password_reset(State(s): S, headers: HeaderMap, Path(id): Path<String>) -> Reply {
    let mut inner = s.lock().unwrap();
    if let Some(e) = guard(&headers, &inner) { return e; }
    inner.emails.push((id, "reset".into()));
    ok(json!({}))
}

async fn set_password(State(s): S, headers: HeaderMap, Path(id): Path<String>, Json(b): Json<Value>) -> Reply {
    let mut inner = s.lock().unwrap();
    if let Some(e) = guard(&headers, &inner) { return e; }
    if b.get("verificationCode").and_then(Value::as_str) != Some(RESET_CODE) { return err(StatusCode::BAD_REQUEST, "Errors.User.Code.Invalid"); }
    let pw = b.pointer("/newPassword/password").and_then(Value::as_str).unwrap_or("").to_string();
    if pw.len() < 8 { return err(StatusCode::BAD_REQUEST, "Errors.User.PasswordComplexityPolicy.MinLength"); }
    inner.users.iter_mut().filter(|u| u.id == id).for_each(|u| u.password = pw.clone());
    ok(json!({}))
}
```

- [ ] **Step 2: Failing client tests** (`tests/zitadel_client.rs`):

```rust
mod support;

use support::*;
use ui_auth::zitadel::{NewUser, ZError, Zitadel};

async fn setup() -> (MockZitadel, Zitadel) {
    let mock = MockZitadel::start().await;
    let client = Zitadel::new(&mock.base, TOKEN);
    (mock, client)
}

#[tokio::test]
async fn reads_whether_a_sign_in_asked_for_sign_up() {
    let (mock, z) = setup().await;
    assert!(z.auth_request(&mock.add_auth_request(true)).await.unwrap());
    assert!(!z.auth_request(&mock.add_auth_request(false)).await.unwrap());
    assert!(matches!(z.auth_request("V2_missing").await, Err(ZError::Rejected { status: 404, .. })));
}

#[tokio::test]
async fn signs_in_and_finishes() {
    let (mock, z) = setup().await;
    let user = mock.add_user("ada@example.com", "Secret-pass1", true);
    assert_eq!(z.find_user_by_email("o1", "ADA@example.com").await.unwrap(), Some(user.clone()));
    assert_eq!(z.find_user_by_email("o1", "nobody@example.com").await.unwrap(), None);
    assert!(z.email_verified(&user).await.unwrap());
    let session = z.create_session(&user, "Secret-pass1").await.unwrap();
    let ar = mock.add_auth_request(false);
    assert!(z.finish(&ar, &session).await.unwrap().starts_with("http://localhost:3000/callback?code="));
    assert!(matches!(z.create_session(&user, "wrong").await, Err(ZError::Rejected { status: 400, .. })));
}

#[tokio::test]
async fn creates_and_verifies_a_user() {
    let (mock, z) = setup().await;
    let new = NewUser { org_id: "o1", email: "bob@example.com", given_name: "Bob", family_name: "B", password: "Secret-pass1", verified: false, url_template: "http://localhost:3000/verify?code={{.Code}}" };
    let id = z.create_user(&new).await.unwrap();
    assert_eq!(mock.emails(), vec![(id.clone(), "verify".to_string())]);
    assert!(matches!(z.create_user(&new).await, Err(ZError::Rejected { status: 409, .. })));
    assert!(matches!(z.verify_email(&id, "000000").await, Err(ZError::Rejected { status: 400, .. })));
    z.verify_email(&id, EMAIL_CODE).await.unwrap();
    assert!(z.email_verified(&id).await.unwrap());
}

#[tokio::test]
async fn resets_a_password() {
    let (mock, z) = setup().await;
    let id = mock.add_user("cy@example.com", "Old-pass-1", true);
    z.send_password_reset(&id, "http://localhost:3000/password-reset?userId={{.UserID}}&code={{.Code}}").await.unwrap();
    assert_eq!(mock.emails(), vec![(id.clone(), "reset".to_string())]);
    match z.set_password(&id, RESET_CODE, "short").await {
        Err(ZError::Rejected { status: 400, message }) => assert!(message.contains("Password")),
        other => panic!("{other:?}"),
    }
    z.set_password(&id, RESET_CODE, "New-pass-12").await.unwrap();
}

#[tokio::test]
async fn an_unreachable_or_failing_zitadel_is_unavailable() {
    let (mock, z) = setup().await;
    mock.set_down(true);
    assert!(matches!(z.auth_request("V2_1").await, Err(ZError::Unavailable(_))));
    let nothing = Zitadel::new("http://127.0.0.1:9", TOKEN);
    assert!(matches!(nothing.auth_request("V2_1").await, Err(ZError::Unavailable(_))));
}
```

- [ ] **Step 3: Run; expect compile errors:** `cargo test -p ui-auth --test zitadel_client`.

- [ ] **Step 4: Implement `zitadel.rs`:**

```rust
//! Zitadel v4's Session, User and OIDC v2 REST APIs, with the login-client token.

use reqwest::Method;
use serde_json::{json, Value};
use std::time::Duration;

#[derive(Debug, thiserror::Error)]
pub enum ZError {
    /// Zitadel understood and refused (4xx). `message` is Zitadel's error id, e.g.
    /// `Errors.User.Code.Invalid`; never shown to users.
    #[error("zitadel rejected the request ({status}): {message}")]
    Rejected { status: u16, message: String },
    /// No answer, a timeout, a 5xx or an unreadable answer.
    #[error("zitadel unavailable: {0}")]
    Unavailable(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub id: String,
    pub token: String,
}

pub struct NewUser<'a> {
    pub org_id: &'a str,
    pub email: &'a str,
    pub given_name: &'a str,
    pub family_name: &'a str,
    pub password: &'a str,
    /// Dev only (`CHELADB_DEV_SKIP_EMAIL_VERIFICATION`): create the email already verified.
    pub verified: bool,
    pub url_template: &'a str,
}

pub struct Zitadel {
    http: reqwest::Client,
    base: String,
    token: String,
}

fn str_at(value: &Value, pointer: &str) -> Result<String, ZError> {
    value
        .pointer(pointer)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| ZError::Unavailable(format!("answer without {pointer}")))
}

impl Zitadel {
    pub fn new(base: &str, token: &str) -> Self {
        Self {
            http: reqwest::Client::builder().timeout(Duration::from_secs(10)).build().expect("reqwest client"),
            base: base.trim_end_matches('/').to_string(),
            token: token.to_string(),
        }
    }

    async fn call(&self, method: Method, path: &str, body: Option<Value>) -> Result<Value, ZError> {
        let mut request = self.http.request(method, format!("{}{path}", self.base)).bearer_auth(&self.token);
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.map_err(|e| ZError::Unavailable(e.to_string()))?;
        let status = response.status();
        let text = response.text().await.map_err(|e| ZError::Unavailable(e.to_string()))?;
        if status.is_server_error() {
            return Err(ZError::Unavailable(format!("{status}")));
        }
        let value: Value = if text.trim().is_empty() { Value::Null } else {
            serde_json::from_str(&text).map_err(|_| ZError::Unavailable(format!("{status}: not JSON")))?
        };
        if status.is_client_error() {
            let message = value.get("message").and_then(Value::as_str).unwrap_or("").to_string();
            return Err(ZError::Rejected { status: status.as_u16(), message });
        }
        Ok(value)
    }

    /// Whether the pending sign-in asked for sign-up (`prompt=create`).
    pub async fn auth_request(&self, id: &str) -> Result<bool, ZError> {
        let answer = self.call(Method::GET, &format!("/v2/oidc/auth_requests/{id}"), None).await?;
        Ok(answer
            .pointer("/authRequest/prompt")
            .and_then(Value::as_array)
            .is_some_and(|p| p.iter().any(|v| v == "PROMPT_CREATE")))
    }

    pub async fn find_user_by_email(&self, org_id: &str, email: &str) -> Result<Option<String>, ZError> {
        let body = json!({"queries": [
            {"emailQuery": {"emailAddress": email, "method": "TEXT_QUERY_METHOD_EQUALS_IGNORE_CASE"}},
            {"organizationIdQuery": {"organizationId": org_id}}
        ]});
        let answer = self.call(Method::POST, "/v2/users", Some(body)).await?;
        let result = answer.get("result").and_then(Value::as_array).cloned().unwrap_or_default();
        Ok(match result.as_slice() {
            [one] => one.get("userId").and_then(Value::as_str).map(str::to_string),
            _ => None,
        })
    }

    pub async fn email_verified(&self, user_id: &str) -> Result<bool, ZError> {
        let answer = self.call(Method::GET, &format!("/v2/users/{user_id}"), None).await?;
        Ok(answer.pointer("/user/human/email/isVerified").and_then(Value::as_bool).unwrap_or(false))
    }

    pub async fn create_session(&self, user_id: &str, password: &str) -> Result<Session, ZError> {
        let body = json!({"checks": {"user": {"userId": user_id}, "password": {"password": password}}});
        let answer = self.call(Method::POST, "/v2/sessions", Some(body)).await?;
        Ok(Session { id: str_at(&answer, "/sessionId")?, token: str_at(&answer, "/sessionToken")? })
    }

    pub async fn delete_session(&self, session: &Session) -> Result<(), ZError> {
        self.call(Method::DELETE, &format!("/v2/sessions/{}", session.id), Some(json!({"sessionToken": session.token}))).await?;
        Ok(())
    }

    /// Finalizes the pending sign-in; the URL to send the browser to.
    pub async fn finish(&self, auth_request: &str, session: &Session) -> Result<String, ZError> {
        let body = json!({"session": {"sessionId": session.id, "sessionToken": session.token}});
        let answer = self.call(Method::POST, &format!("/v2/oidc/auth_requests/{auth_request}"), Some(body)).await?;
        str_at(&answer, "/callbackUrl")
    }

    pub async fn create_user(&self, user: &NewUser<'_>) -> Result<String, ZError> {
        let email = if user.verified {
            json!({"email": user.email, "isVerified": true})
        } else {
            json!({"email": user.email, "sendCode": {"urlTemplate": user.url_template}})
        };
        let body = json!({"organizationId": user.org_id, "human": {
            "profile": {"givenName": user.given_name, "familyName": user.family_name},
            "email": email,
            "password": {"password": user.password, "changeRequired": false}
        }});
        let answer = self.call(Method::POST, "/v2/users/new", Some(body)).await?;
        str_at(&answer, "/id")
    }

    pub async fn verify_email(&self, user_id: &str, code: &str) -> Result<(), ZError> {
        self.call(Method::POST, &format!("/v2/users/{user_id}/email/verify"), Some(json!({"verificationCode": code}))).await?;
        Ok(())
    }

    pub async fn resend_code(&self, user_id: &str, url_template: &str) -> Result<(), ZError> {
        self.call(Method::POST, &format!("/v2/users/{user_id}/email/resend"), Some(json!({"sendCode": {"urlTemplate": url_template}}))).await?;
        Ok(())
    }

    pub async fn send_password_reset(&self, user_id: &str, url_template: &str) -> Result<(), ZError> {
        let body = json!({"sendLink": {"notificationType": "NOTIFICATION_TYPE_Email", "urlTemplate": url_template}});
        self.call(Method::POST, &format!("/v2/users/{user_id}/password_reset"), Some(body)).await?;
        Ok(())
    }

    pub async fn set_password(&self, user_id: &str, code: &str, password: &str) -> Result<(), ZError> {
        let body = json!({"newPassword": {"password": password, "changeRequired": false}, "verificationCode": code});
        self.call(Method::POST, &format!("/v2/users/{user_id}/password"), Some(body)).await?;
        Ok(())
    }
}
```

- [ ] **Step 5: Run.** `cargo test -p ui-auth` → pass. Clippy clean. Format new files.

- [ ] **Step 6: Commit:** `ui-auth: Zitadel v2 client, tested against a stateful mock`.

---

### Task 7: `ui-auth` handlers: auth-request, sign-in, sign-up, verify, resend

**Files:**
- Create: `apps/ui-auth/src/app.rs`, `apps/ui-auth/tests/flows.rs`
- Modify: `apps/ui-auth/src/lib.rs` (`pub mod app;`)

**Interfaces:**
- Consumes: Tasks 4–6 (`cheladb_routes::ui_auth::*`, `cookie::*`, `guard::*`, `zitadel::*`, `config::Config`).
- Produces: `app::{AppState, router, messages}`; `AppState { zitadel: Zitadel, config: Config, limiter: RateLimiter }`; `router(state: Arc<AppState>) -> axum::Router`; `messages::{BAD_CREDENTIALS, BAD_CODE, WEAK_PASSWORD, GONE, TOO_MANY, UNAVAILABLE}` (the exact strings from Global Constraints).

- [ ] **Step 1: Failing flow tests** (`tests/flows.rs`), driving the real router over HTTP against the mock:

```rust
mod support;

use cheladb_secrets::SecretKey;
use reqwest::StatusCode;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::{Duration, Instant};
use support::*;
use ui_auth::app::{messages, router, AppState};
use ui_auth::config::Config;
use ui_auth::guard::RateLimiter;
use ui_auth::zitadel::Zitadel;

pub struct Harness { pub mock: MockZitadel, pub base: String, pub http: reqwest::Client }

pub async fn harness_with(skip: bool, min_ms: u64) -> Harness {
    let mock = MockZitadel::start().await;
    let config = Config {
        listen: String::new(), issuer: mock.base.clone(), token: TOKEN.into(), cookie_key: SecretKey::generate(),
        org_id: "o1".into(), studio_url: "http://localhost:3000".into(), skip_email_verification: skip,
        secure_cookies: false, min_response: Duration::from_millis(min_ms),
    };
    let state = Arc::new(AppState { zitadel: Zitadel::new(&mock.base, TOKEN), config, limiter: RateLimiter::default() });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router(state)).await.unwrap() });
    Harness { mock, base: format!("http://{addr}"), http: reqwest::Client::new() }
}

pub async fn harness() -> Harness { harness_with(false, 1).await }

pub struct Answer { pub status: StatusCode, pub body: Value, pub cookies: Vec<String> }

impl Answer {
    /// `name=value` of a cookie this answer set, for sending back.
    pub fn cookie(&self, name: &str) -> Option<String> {
        self.cookies.iter().find(|c| c.starts_with(&format!("{name}="))).map(|c| c.split(';').next().unwrap().to_string())
    }
}

impl Harness {
    pub async fn post(&self, path: &str, body: Value, cookie: Option<&str>, ip: &str) -> Answer {
        let mut req = self.http.post(format!("{}{path}", self.base)).json(&body).header("x-forwarded-for", format!("9.9.9.9, {ip}"));
        if let Some(c) = cookie { req = req.header("cookie", c); }
        let resp = req.send().await.unwrap();
        let status = resp.status();
        let cookies = resp.headers().get_all("set-cookie").iter().map(|v| v.to_str().unwrap().to_string()).collect();
        let text = resp.text().await.unwrap();
        Answer { status, body: serde_json::from_str(&text).unwrap_or(Value::Null), cookies }
    }
}

fn signin(ar: &str, email: &str, pw: &str) -> Value { json!({"authRequest": ar, "email": email, "password": pw}) }
fn signup(ar: &str, email: &str, pw: &str) -> Value { json!({"authRequest": ar, "email": email, "password": pw, "givenName": "A", "familyName": "B"}) }

#[tokio::test]
async fn auth_request_says_whether_to_show_sign_up() {
    let h = harness().await;
    let yes = h.post("/auth/v1/auth-request", json!({"authRequest": h.mock.add_auth_request(true)}), None, "1.1.1.1").await;
    assert_eq!((yes.status, yes.body.clone()), (StatusCode::OK, json!({"signUp": true})));
    let gone = h.post("/auth/v1/auth-request", json!({"authRequest": "V2_nope"}), None, "1.1.1.1").await;
    assert_eq!((gone.status, gone.body), (StatusCode::GONE, json!({"error": messages::GONE})));
}

#[tokio::test]
async fn sign_in_finishes_and_sets_the_session_cookie() {
    let h = harness().await;
    h.mock.add_user("ada@example.com", "Secret-pass1", true);
    let ar = h.mock.add_auth_request(false);
    let a = h.post("/auth/v1/sign-in", signin(&ar, " ADA@example.com ", "Secret-pass1"), None, "1.1.1.1").await;
    assert_eq!(a.status, StatusCode::OK);
    assert_eq!(a.body["next"], "done");
    assert!(a.body["redirect"].as_str().unwrap().starts_with("http://localhost:3000/callback?code="));
    assert!(a.cookie("cheladb_session").is_some());
    assert!(a.cookies.iter().any(|c| c.starts_with("cheladb_session=") && c.contains("HttpOnly; SameSite=Strict; Path=/auth/v1")));
}

#[tokio::test]
async fn unknown_user_wrong_password_and_locked_give_identical_401s() {
    let h = harness().await;
    h.mock.add_user("ada@example.com", "Secret-pass1", true);
    let locked = h.mock.add_user("lee@example.com", "Secret-pass1", true);
    h.mock.lock(&locked);
    let mut bodies = Vec::new();
    for (email, pw) in [("nobody@example.com", "x"), ("ada@example.com", "wrong"), ("lee@example.com", "Secret-pass1")] {
        let a = h.post("/auth/v1/sign-in", signin(&h.mock.add_auth_request(false), email, pw), None, "1.1.1.1").await;
        assert_eq!(a.status, StatusCode::UNAUTHORIZED, "{email}");
        assert!(a.cookies.is_empty(), "{email}");
        bodies.push(serde_json::to_string(&a.body).unwrap());
    }
    assert!(bodies.iter().all(|b| *b == format!(r#"{{"error":"{}"}}"#, messages::BAD_CREDENTIALS)), "{bodies:?}");
}

#[tokio::test]
async fn sign_in_with_an_unverified_email_asks_for_the_code() {
    let h = harness().await;
    let id = h.mock.add_user("ada@example.com", "Secret-pass1", false);
    let ar = h.mock.add_auth_request(false);
    let a = h.post("/auth/v1/sign-in", signin(&ar, "ada@example.com", "Secret-pass1"), None, "1.1.1.1").await;
    assert_eq!((a.status, a.body.clone()), (StatusCode::OK, json!({"next": "verify"})));
    assert_eq!(h.mock.emails(), vec![(id, "verify".to_string())]);
    let login = a.cookie("cheladb_login").unwrap();
    let done = h.post("/auth/v1/verify-email", json!({"code": EMAIL_CODE}), Some(&login), "1.1.1.1").await;
    assert_eq!(done.body["next"], "done");
    assert!(done.cookies.iter().any(|c| c.starts_with("cheladb_login=;") && c.contains("Max-Age=0")));
}

#[tokio::test]
async fn sign_up_then_verify_finishes() {
    let h = harness().await;
    let ar = h.mock.add_auth_request(true);
    let a = h.post("/auth/v1/sign-up", signup(&ar, "new@example.com", "Secret-pass1"), None, "1.1.1.1").await;
    assert_eq!((a.status, a.body.clone()), (StatusCode::OK, json!({"next": "verify"})));
    let login = a.cookie("cheladb_login").unwrap();
    let bad = h.post("/auth/v1/verify-email", json!({"code": "000000"}), Some(&login), "1.1.1.1").await;
    assert_eq!((bad.status, bad.body), (StatusCode::BAD_REQUEST, json!({"error": messages::BAD_CODE})));
    let done = h.post("/auth/v1/verify-email", json!({"code": EMAIL_CODE}), Some(&login), "1.1.1.1").await;
    assert_eq!(done.body["next"], "done");
    assert!(h.mock.user("new@example.com").unwrap().verified);
}

#[tokio::test]
async fn a_taken_email_looks_the_same_and_its_code_never_works() {
    let h = harness().await;
    h.mock.add_user("ada@example.com", "Secret-pass1", true);
    let fresh = h.post("/auth/v1/sign-up", signup(&h.mock.add_auth_request(true), "new@example.com", "Secret-pass1"), None, "1.1.1.1").await;
    let taken = h.post("/auth/v1/sign-up", signup(&h.mock.add_auth_request(true), "ada@example.com", "Secret-pass1"), None, "2.2.2.2").await;
    assert_eq!((taken.status, taken.body.clone()), (fresh.status, fresh.body.clone()));
    let decoy = taken.cookie("cheladb_login").unwrap();
    assert_eq!(decoy.len(), fresh.cookie("cheladb_login").unwrap().len());
    let a = h.post("/auth/v1/verify-email", json!({"code": EMAIL_CODE}), Some(&decoy), "2.2.2.2").await;
    assert_eq!((a.status, a.body), (StatusCode::BAD_REQUEST, json!({"error": messages::BAD_CODE})));
}

#[tokio::test]
async fn a_weak_password_at_sign_up_says_so() {
    let h = harness().await;
    let a = h.post("/auth/v1/sign-up", signup(&h.mock.add_auth_request(true), "new@example.com", "short"), None, "1.1.1.1").await;
    assert_eq!((a.status, a.body), (StatusCode::BAD_REQUEST, json!({"error": messages::WEAK_PASSWORD})));
}

#[tokio::test]
async fn the_dev_flag_signs_up_already_verified() {
    let h = harness_with(true, 1).await;
    let a = h.post("/auth/v1/sign-up", signup(&h.mock.add_auth_request(true), "dev@example.com", "Secret-pass1"), None, "1.1.1.1").await;
    assert_eq!(a.body["next"], "done");
    assert!(h.mock.user("dev@example.com").unwrap().verified);
    assert!(h.mock.emails().is_empty());
}

#[tokio::test]
async fn resend_always_answers_ok() {
    let h = harness().await;
    let a = h.post("/auth/v1/resend-code", json!({}), None, "1.1.1.1").await;
    assert_eq!((a.status, a.body), (StatusCode::OK, json!({})));
}

#[tokio::test]
async fn sign_in_answers_take_the_minimum_time() {
    let h = harness_with(false, 300).await;
    let start = Instant::now();
    h.post("/auth/v1/sign-in", signin(&h.mock.add_auth_request(false), "nobody@example.com", "x"), None, "1.1.1.1").await;
    assert!(start.elapsed() >= Duration::from_millis(300));
}

#[tokio::test]
async fn the_sixth_sign_in_per_email_per_minute_is_refused_even_from_new_ips() {
    let h = harness().await;
    for i in 0..5 {
        let a = h.post("/auth/v1/sign-in", signin(&h.mock.add_auth_request(false), "ada@example.com", "x"), None, &format!("1.1.1.{i}")).await;
        assert_eq!(a.status, StatusCode::UNAUTHORIZED);
    }
    let a = h.post("/auth/v1/sign-in", signin(&h.mock.add_auth_request(false), "ada@example.com", "x"), None, "7.7.7.7").await;
    assert_eq!((a.status, a.body), (StatusCode::TOO_MANY_REQUESTS, json!({"error": messages::TOO_MANY})));
}

// Review Focus 5.
#[tokio::test]
async fn verify_email_is_rate_limited() {
    let h = harness().await;
    let a = h.post("/auth/v1/sign-up", signup(&h.mock.add_auth_request(true), "new@example.com", "Secret-pass1"), None, "3.3.3.3").await;
    let login = a.cookie("cheladb_login").unwrap();
    for _ in 0..10 {
        h.post("/auth/v1/verify-email", json!({"code": "000000"}), Some(&login), "3.3.3.3").await;
    }
    let a = h.post("/auth/v1/verify-email", json!({"code": EMAIL_CODE}), Some(&login), "3.3.3.3").await;
    assert_eq!(a.status, StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn zitadel_down_is_503_and_an_expired_sign_in_is_410() {
    let h = harness().await;
    let a = h.post("/auth/v1/sign-in", signin("V2_nope", "ada@example.com", "x"), None, "1.1.1.1").await;
    assert_eq!((a.status, a.body), (StatusCode::GONE, json!({"error": messages::GONE})));
    h.mock.set_down(true);
    let a = h.post("/auth/v1/sign-in", signin("V2_1", "ada@example.com", "x"), None, "1.1.1.1").await;
    assert_eq!((a.status, a.body), (StatusCode::SERVICE_UNAVAILABLE, json!({"error": messages::UNAVAILABLE})));
}

#[tokio::test]
async fn a_non_json_request_is_refused() {
    let h = harness().await;
    let resp = h.http.post(format!("{}/auth/v1/sign-in", h.base)).body("email=a").header("content-type", "application/x-www-form-urlencoded").send().await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
}
```

- [ ] **Step 2: Run; expect compile errors:** `cargo test -p ui-auth --test flows`.

- [ ] **Step 3: Implement `app.rs`** (this task's handlers; Task 8 adds the rest to the same router):

```rust
//! The HTTP API Studio's sign-in pages call (through the gateway).

use crate::config::Config;
use crate::cookie::*;
use crate::guard::{client_ip, email_tag, normalize_email, pad, RateLimiter};
use crate::zitadel::{NewUser, Session, ZError, Zitadel};
use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use cheladb_routes::ui_auth as routes;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{info, warn};

pub mod messages {
    pub const BAD_CREDENTIALS: &str = "Email or password is incorrect";
    pub const BAD_CODE: &str = "Code is invalid or expired";
    pub const WEAK_PASSWORD: &str = "Use at least 8 characters, with upper- and lower-case letters, a number and a symbol";
    pub const GONE: &str = "This sign-in has expired, start again";
    pub const TOO_MANY: &str = "Too many attempts, try again later";
    pub const UNAVAILABLE: &str = "Sign-in is unavailable, try again";
}
use messages::*;

pub struct AppState {
    pub zitadel: Zitadel,
    pub config: Config,
    pub limiter: RateLimiter,
}

type S = State<Arc<AppState>>;

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route(routes::AUTH_REQUEST.path, post(auth_request))
        .route(routes::SIGN_IN.path, post(sign_in))
        .route(routes::SIGN_UP.path, post(sign_up))
        .route(routes::VERIFY_EMAIL.path, post(verify_email))
        .route(routes::RESEND_CODE.path, post(resend_code))
        .with_state(state)
}

const MINUTE: Duration = Duration::from_secs(60);
const HOUR: Duration = Duration::from_secs(3600);

/// Counts one attempt against every key; false if any is over its limit.
fn allowed(s: &AppState, checks: &[(String, u32, Duration)]) -> bool {
    checks.iter().fold(true, |ok, (key, limit, window)| s.limiter.allow(key, *limit, *window) && ok)
}

fn reply(status: StatusCode, body: Value, cookies: Vec<String>) -> Response {
    let mut response = (status, Json(body)).into_response();
    for c in cookies {
        response.headers_mut().append(header::SET_COOKIE, c.parse().expect("cookie header"));
    }
    response
}

fn error(status: StatusCode, message: &str) -> Response {
    reply(status, json!({ "error": message }), Vec::new())
}

fn unavailable(e: &ZError) -> Response {
    warn!(error = %e, "zitadel call failed");
    error(StatusCode::SERVICE_UNAVAILABLE, UNAVAILABLE)
}

/// A pending sign-in that Zitadel no longer knows is gone; anything else is unavailability.
fn pending_sign_in_error(e: ZError) -> Response {
    match e {
        ZError::Rejected { .. } => error(StatusCode::GONE, GONE),
        e => unavailable(&e),
    }
}

fn verify_template(s: &AppState) -> String {
    format!("{}/verify?code={{{{.Code}}}}", s.config.studio_url)
}

fn login_cookie(s: &AppState, state: &LoginState) -> String {
    set_cookie(LOGIN_COOKIE, &seal_login(&s.config.cookie_key, state), LOGIN_TTL_SECS, s.config.secure_cookies)
}

/// Finalize the pending sign-in: `done` with Zitadel's redirect, the session cookie set and
/// the step cookie cleared.
async fn finish(s: &AppState, auth_request: &str, session: Session) -> Response {
    match s.zitadel.finish(auth_request, &session).await {
        Ok(redirect) => {
            let kept = SessionState { session_id: session.id, session_token: session.token, exp: now_secs() + SESSION_TTL_SECS };
            let secure = s.config.secure_cookies;
            reply(StatusCode::OK, json!({"next": "done", "redirect": redirect}), vec![
                set_cookie(SESSION_COOKIE, &seal_session(&s.config.cookie_key, &kept), SESSION_TTL_SECS, secure),
                clear_cookie(LOGIN_COOKIE, secure),
            ])
        }
        Err(e) => pending_sign_in_error(e),
    }
}

async fn verify_step(s: &AppState, auth_request: &str, user_id: String, session: Session) -> Response {
    let state = LoginState {
        auth_request: auth_request.to_string(),
        user_id: Some(user_id),
        session_id: Some(session.id),
        session_token: Some(session.token),
        step: Step::Verify,
        decoy: false,
        exp: now_secs() + LOGIN_TTL_SECS,
    };
    reply(StatusCode::OK, json!({"next": "verify"}), vec![login_cookie(s, &state)])
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AuthRequestBody { auth_request: String }

async fn auth_request(State(s): S, Json(b): Json<AuthRequestBody>) -> Response {
    match s.zitadel.auth_request(&b.auth_request).await {
        Ok(sign_up) => reply(StatusCode::OK, json!({"signUp": sign_up}), Vec::new()),
        Err(e) => pending_sign_in_error(e),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SignInBody { auth_request: String, email: String, password: String }

async fn sign_in(State(s): S, headers: HeaderMap, Json(b): Json<SignInBody>) -> Response {
    let start = Instant::now();
    let answer = sign_in_inner(&s, &headers, b).await;
    pad(start, s.config.min_response).await;
    answer
}

async fn sign_in_inner(s: &AppState, headers: &HeaderMap, b: SignInBody) -> Response {
    let email = normalize_email(&b.email);
    let ip = client_ip(headers);
    if !allowed(s, &[(format!("sign-in:ip:{ip}"), 10, MINUTE), (format!("sign-in:email:{email}"), 5, MINUTE)]) {
        return error(StatusCode::TOO_MANY_REQUESTS, TOO_MANY);
    }
    if let Err(e) = s.zitadel.auth_request(&b.auth_request).await {
        return pending_sign_in_error(e);
    }
    let user_id = match s.zitadel.find_user_by_email(&s.config.org_id, &email).await {
        Ok(Some(id)) => id,
        Ok(None) => return error(StatusCode::UNAUTHORIZED, BAD_CREDENTIALS),
        Err(e) => return unavailable(&e),
    };
    let session = match s.zitadel.create_session(&user_id, &b.password).await {
        Ok(session) => session,
        Err(ZError::Rejected { .. }) => {
            info!(email = %email_tag(&email), "sign-in refused");
            return error(StatusCode::UNAUTHORIZED, BAD_CREDENTIALS);
        }
        Err(e) => return unavailable(&e),
    };
    match s.zitadel.email_verified(&user_id).await {
        Ok(true) => finish(s, &b.auth_request, session).await,
        Ok(false) => {
            if let Err(e) = s.zitadel.resend_code(&user_id, &verify_template(s)).await {
                warn!(error = %e, "resending the verification code failed");
            }
            verify_step(s, &b.auth_request, user_id, session).await
        }
        Err(e) => unavailable(&e),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SignUpBody { auth_request: String, email: String, password: String, given_name: String, family_name: String }

async fn sign_up(State(s): S, headers: HeaderMap, Json(b): Json<SignUpBody>) -> Response {
    let start = Instant::now();
    let answer = sign_up_inner(&s, &headers, b).await;
    pad(start, s.config.min_response).await;
    answer
}

async fn sign_up_inner(s: &AppState, headers: &HeaderMap, b: SignUpBody) -> Response {
    let email = normalize_email(&b.email);
    let ip = client_ip(headers);
    if !allowed(s, &[(format!("sign-up:ip:{ip}"), 20, HOUR), (format!("sign-up:email:{email}"), 5, HOUR)]) {
        return error(StatusCode::TOO_MANY_REQUESTS, TOO_MANY);
    }
    if let Err(e) = s.zitadel.auth_request(&b.auth_request).await {
        return pending_sign_in_error(e);
    }
    let template = verify_template(s);
    let new = NewUser {
        org_id: &s.config.org_id, email: &email, given_name: b.given_name.trim(), family_name: b.family_name.trim(),
        password: &b.password, verified: s.config.skip_email_verification, url_template: &template,
    };
    let user_id = match s.zitadel.create_user(&new).await {
        Ok(id) => id,
        Err(ZError::Rejected { status: 409, .. }) => {
            info!(email = %email_tag(&email), "sign-up for a taken email (decoy)");
            let decoy = LoginState {
                auth_request: b.auth_request, user_id: None, session_id: None, session_token: None,
                step: Step::Verify, decoy: true, exp: now_secs() + LOGIN_TTL_SECS,
            };
            return reply(StatusCode::OK, json!({"next": "verify"}), vec![login_cookie(s, &decoy)]);
        }
        Err(ZError::Rejected { status: 400, .. }) => return error(StatusCode::BAD_REQUEST, WEAK_PASSWORD),
        Err(e) => return unavailable(&e),
    };
    let session = match s.zitadel.create_session(&user_id, &b.password).await {
        Ok(session) => session,
        Err(e) => return unavailable(&e),
    };
    if s.config.skip_email_verification {
        return finish(s, &b.auth_request, session).await;
    }
    verify_step(s, &b.auth_request, user_id, session).await
}

fn current_login(s: &AppState, headers: &HeaderMap) -> Option<LoginState> {
    read_cookie(headers, LOGIN_COOKIE).and_then(|raw| open_login(&s.config.cookie_key, raw, now_secs()))
}

#[derive(Deserialize)]
struct CodeBody { code: String }

async fn verify_email(State(s): S, headers: HeaderMap, Json(b): Json<CodeBody>) -> Response {
    if !allowed(&s, &[(format!("verify:ip:{}", client_ip(&headers)), 10, MINUTE)]) {
        return error(StatusCode::TOO_MANY_REQUESTS, TOO_MANY);
    }
    let Some(state) = current_login(&s, &headers).filter(|st| st.step == Step::Verify) else {
        return error(StatusCode::BAD_REQUEST, BAD_CODE);
    };
    let (Some(user_id), Some(session_id), Some(session_token), false) =
        (state.user_id, state.session_id, state.session_token, state.decoy)
    else {
        return error(StatusCode::BAD_REQUEST, BAD_CODE);
    };
    match s.zitadel.verify_email(&user_id, b.code.trim()).await {
        Ok(()) => finish(&s, &state.auth_request, Session { id: session_id, token: session_token }).await,
        Err(ZError::Rejected { .. }) => error(StatusCode::BAD_REQUEST, BAD_CODE),
        Err(e) => unavailable(&e),
    }
}

#[derive(Deserialize)]
struct Empty {}

async fn resend_code(State(s): S, headers: HeaderMap, Json(_): Json<Empty>) -> Response {
    let ip = client_ip(&headers);
    let state = current_login(&s, &headers);
    let user = state.as_ref().and_then(|st| st.user_id.clone()).unwrap_or_default();
    if !allowed(&s, &[(format!("resend:ip:{ip}"), 20, HOUR), (format!("resend:user:{user}"), 5, HOUR)]) {
        return error(StatusCode::TOO_MANY_REQUESTS, TOO_MANY);
    }
    if !user.is_empty()
        && let Err(e) = s.zitadel.resend_code(&user, &verify_template(&s)).await
    {
        warn!(error = %e, "resending the verification code failed");
    }
    reply(StatusCode::OK, json!({}), Vec::new())
}
```

- [ ] **Step 4: Run.** `cargo test -p ui-auth` → all pass. Clippy clean. Format `app.rs` and `tests/flows.rs`.

- [ ] **Step 5: Commit:** `ui-auth: sign-in, sign-up, email verification`.

---

### Task 8: `ui-auth` handlers: password reset, logout; the binary; `just`

**Files:**
- Modify: `apps/ui-auth/src/app.rs`, `apps/ui-auth/src/main.rs`, `apps/ui-auth/tests/flows.rs`, `justfile`

**Interfaces:**
- Consumes: Task 7's `app.rs` helpers (`allowed`, `reply`, `error`, `unavailable`, `current_login`, `Empty`, `MINUTE`, `HOUR`).
- Produces: `POST /auth/v1/password-reset {email}`, `POST /auth/v1/password-reset/confirm {userId, code, password}`, `POST /auth/v1/logout {}`; binary `ui-auth`; recipe `just ui-auth`; `just dev` starts `ui-auth`.

- [ ] **Step 1: Failing tests** (append to `tests/flows.rs`):

```rust
#[tokio::test]
async fn password_reset_answers_the_same_whether_or_not_the_user_exists() {
    let h = harness().await;
    let id = h.mock.add_user("ada@example.com", "Old-pass-1", true);
    let known = h.post("/auth/v1/password-reset", json!({"email": "ADA@example.com"}), None, "1.1.1.1").await;
    let unknown = h.post("/auth/v1/password-reset", json!({"email": "nobody@example.com"}), None, "1.1.1.1").await;
    assert_eq!((known.status, known.body.clone()), (StatusCode::OK, json!({})));
    assert_eq!((unknown.status, unknown.body), (known.status, known.body));
    // The email goes out in the background.
    for _ in 0..50 {
        if !h.mock.emails().is_empty() { break; }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(h.mock.emails(), vec![(id, "reset".to_string())]);
}

#[tokio::test]
async fn confirm_sets_the_new_password() {
    let h = harness().await;
    let id = h.mock.add_user("ada@example.com", "Old-pass-1", true);
    let bad = h.post("/auth/v1/password-reset/confirm", json!({"userId": id, "code": "000000", "password": "New-pass-12"}), None, "1.1.1.1").await;
    assert_eq!((bad.status, bad.body), (StatusCode::BAD_REQUEST, json!({"error": messages::BAD_CODE})));
    let ok = h.post("/auth/v1/password-reset/confirm", json!({"userId": id, "code": RESET_CODE, "password": "New-pass-12"}), None, "1.1.1.1").await;
    assert_eq!((ok.status, ok.body), (StatusCode::OK, json!({})));
    assert_eq!(h.mock.user("ada@example.com").unwrap().password, "New-pass-12");
}

// Review Focus 3.
#[tokio::test]
async fn confirm_with_a_weak_password_says_so() {
    let h = harness().await;
    let id = h.mock.add_user("ada@example.com", "Old-pass-1", true);
    let a = h.post("/auth/v1/password-reset/confirm", json!({"userId": id, "code": RESET_CODE, "password": "short"}), None, "1.1.1.1").await;
    assert_eq!((a.status, a.body), (StatusCode::BAD_REQUEST, json!({"error": messages::WEAK_PASSWORD})));
}

#[tokio::test]
async fn logout_ends_the_session_and_clears_both_cookies() {
    let h = harness().await;
    h.mock.add_user("ada@example.com", "Secret-pass1", true);
    let a = h.post("/auth/v1/sign-in", signin(&h.mock.add_auth_request(false), "ada@example.com", "Secret-pass1"), None, "1.1.1.1").await;
    assert_eq!(h.mock.session_count(), 1);
    let out = h.post("/auth/v1/logout", json!({}), Some(&a.cookie("cheladb_session").unwrap()), "1.1.1.1").await;
    assert_eq!((out.status, out.body), (StatusCode::OK, json!({})));
    assert_eq!(h.mock.session_count(), 0);
    for name in ["cheladb_login=;", "cheladb_session=;"] {
        assert!(out.cookies.iter().any(|c| c.starts_with(name) && c.contains("Max-Age=0")), "{name}");
    }
    let again = h.post("/auth/v1/logout", json!({}), None, "1.1.1.1").await;
    assert_eq!(again.status, StatusCode::OK);
}
```

- [ ] **Step 2: Run; expect 404s** from the missing routes: `cargo test -p ui-auth --test flows`.

- [ ] **Step 3: Implement.** Add to `router`: `.route(routes::PASSWORD_RESET.path, post(password_reset)).route(routes::PASSWORD_RESET_CONFIRM.path, post(password_reset_confirm)).route(routes::LOGOUT.path, post(logout))`, and:

```rust
#[derive(Deserialize)]
struct EmailBody { email: String }

async fn password_reset(State(s): S, headers: HeaderMap, Json(b): Json<EmailBody>) -> Response {
    let email = normalize_email(&b.email);
    let ip = client_ip(&headers);
    if !allowed(&s, &[(format!("reset:ip:{ip}"), 20, HOUR), (format!("reset:email:{email}"), 5, HOUR)]) {
        return error(StatusCode::TOO_MANY_REQUESTS, TOO_MANY);
    }
    // In the background, so the answer time doesn't reveal whether the user exists.
    let s2 = s.clone();
    tokio::spawn(async move {
        let template = format!("{}/password-reset?userId={{{{.UserID}}}}&code={{{{.Code}}}}", s2.config.studio_url);
        match s2.zitadel.find_user_by_email(&s2.config.org_id, &email).await {
            Ok(Some(user_id)) => {
                if let Err(e) = s2.zitadel.send_password_reset(&user_id, &template).await {
                    warn!(error = %e, "sending the password reset failed");
                }
            }
            Ok(None) => info!(email = %email_tag(&email), "password reset for an unknown email"),
            Err(e) => warn!(error = %e, "password reset lookup failed"),
        }
    });
    reply(StatusCode::OK, json!({}), Vec::new())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ConfirmBody { user_id: String, code: String, password: String }

async fn password_reset_confirm(State(s): S, headers: HeaderMap, Json(b): Json<ConfirmBody>) -> Response {
    if !allowed(&s, &[(format!("confirm:ip:{}", client_ip(&headers)), 10, MINUTE)]) {
        return error(StatusCode::TOO_MANY_REQUESTS, TOO_MANY);
    }
    match s.zitadel.set_password(b.user_id.trim(), b.code.trim(), &b.password).await {
        Ok(()) => reply(StatusCode::OK, json!({}), Vec::new()),
        // Zitadel's error ids tell a bad password (…PasswordComplexityPolicy…) from a bad code.
        Err(ZError::Rejected { message, .. }) if message.contains("Password") => error(StatusCode::BAD_REQUEST, WEAK_PASSWORD),
        Err(ZError::Rejected { .. }) => error(StatusCode::BAD_REQUEST, BAD_CODE),
        Err(e) => unavailable(&e),
    }
}

async fn logout(State(s): S, headers: HeaderMap, Json(_): Json<Empty>) -> Response {
    if let Some(kept) = read_cookie(&headers, SESSION_COOKIE).and_then(|raw| open_session(&s.config.cookie_key, raw, now_secs()))
        && let Err(e) = s.zitadel.delete_session(&Session { id: kept.session_id, token: kept.session_token }).await
    {
        warn!(error = %e, "ending the Zitadel session failed");
    }
    let secure = s.config.secure_cookies;
    reply(StatusCode::OK, json!({}), vec![clear_cookie(LOGIN_COOKIE, secure), clear_cookie(SESSION_COOKIE, secure)])
}
```
(`s.clone()` clones the `Arc`.)

- [ ] **Step 4: The binary** (`main.rs`):

```rust
use std::sync::Arc;
use tracing::info;
use ui_auth::app::{router, AppState};
use ui_auth::config::Config;
use ui_auth::guard::RateLimiter;
use ui_auth::zitadel::Zitadel;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "ui_auth=info".into()))
        .init();
    let config = Config::from_env()?;
    if config.skip_email_verification {
        tracing::warn!("CHELADB_DEV_SKIP_EMAIL_VERIFICATION is on: sign-ups skip email verification (local dev only)");
    }
    let listener = tokio::net::TcpListener::bind(&config.listen).await?;
    info!("ui-auth listening on {}", config.listen);
    let state = Arc::new(AppState { zitadel: Zitadel::new(&config.issuer, &config.token), config, limiter: RateLimiter::default() });
    axum::serve(listener, router(state)).await?;
    Ok(())
}
```

- [ ] **Step 5: `just`.** Add a recipe after `gateway:`, in the same shape (source `demo.env`, then `cargo run -p ui-auth`):

```just
ui-auth:
    #!/usr/bin/env bash
    set -euo pipefail
    if [[ -f .cheladb/zitadel/demo.env ]]; then
      set -a
      # shellcheck disable=SC1091
      source .cheladb/zitadel/demo.env
      set +a
    fi
    cargo run -p ui-auth
```
In `dev`: add `-p ui-auth` to the `cargo build` line; add `nohup setsid cargo run -p ui-auth >"{{dev_dir}}/ui-auth.log" 2>&1 & echo $! >>"{{dev_dir}}/pids"` after the gateway line; add `echo "ui-auth:         http://127.0.0.1:8084  (sign-in backend; via the gateway at /auth/v1)"` to the summary.

- [ ] **Step 6: Run.** `cargo test -p ui-auth` → all pass. `cargo clippy --workspace --all-targets` → clean.

- [ ] **Step 7: Commit:** `ui-auth: password reset, logout, binary and just recipes`.

---

### Task 9: Studio: login API client, `/login`, `/register`, `/verify`

**Files:**
- Create: `apps/studio/src/lib/login-api.ts`, `apps/studio/src/lib/login-api.test.ts`, `apps/studio/src/components/login/sign-in-form.tsx`, `apps/studio/src/components/login/sign-up-form.tsx`, `apps/studio/src/components/login/verify-email-form.tsx`, `apps/studio/src/components/login/login-forms.test.tsx`, `apps/studio/src/routes/login.tsx`, `apps/studio/src/routes/register.tsx`, `apps/studio/src/routes/verify.tsx`
- Modify: `apps/studio/src/routeTree.gen.ts` (regenerated)

**Interfaces:**
- Produces:
  - `type LoginNext = { next: "done"; redirect: string } | { next: "verify" }`
  - `class LoginApiError extends Error { status: number }`
  - `interface LoginApi { authRequest(authRequest: string): Promise<{ signUp: boolean }>; signIn(i: { authRequest: string; email: string; password: string }): Promise<LoginNext>; signUp(i: { authRequest: string; email: string; password: string; givenName: string; familyName: string }): Promise<LoginNext>; verifyEmail(code: string): Promise<LoginNext>; resendCode(): Promise<void>; requestPasswordReset(email: string): Promise<void>; confirmPasswordReset(i: { userId: string; code: string; password: string }): Promise<void>; logout(): Promise<void> }`
  - `createLoginApi(baseUrl: string, fetchImpl?: typeof fetch): LoginApi`
  - components `SignInForm`, `SignUpForm`, `VerifyEmailForm` with props `{ api: LoginApi; authRequest?: string; initialCode?: string; go: (url: string) => void; toVerify: () => void; toRegister?: () => void }`

- [ ] **Step 1: Failing API tests** (`login-api.test.ts`):

```ts
import { describe, expect, it, vi } from "vitest";
import { LoginApiError, createLoginApi } from "./login-api";

function fakeFetch(status: number, body: unknown) {
  return vi.fn().mockResolvedValue(new Response(JSON.stringify(body), { status, headers: { "Content-Type": "application/json" } }));
}

describe("createLoginApi", () => {
  it("posts JSON with credentials and returns the answer", async () => {
    const fetchImpl = fakeFetch(200, { next: "done", redirect: "http://localhost:3000/callback?code=c" });
    const api = createLoginApi("http://127.0.0.1:8000", fetchImpl);
    await expect(api.signIn({ authRequest: "V2_1", email: "a@b.c", password: "p" })).resolves.toEqual({
      next: "done",
      redirect: "http://localhost:3000/callback?code=c",
    });
    const [url, init] = fetchImpl.mock.calls[0];
    expect(url).toBe("http://127.0.0.1:8000/auth/v1/sign-in");
    expect(init).toMatchObject({ method: "POST", credentials: "include" });
    expect(init.headers["Content-Type"]).toBe("application/json");
    expect(JSON.parse(init.body)).toEqual({ authRequest: "V2_1", email: "a@b.c", password: "p" });
  });

  it("turns an error answer into LoginApiError with ui-auth's message", async () => {
    const api = createLoginApi("http://x", fakeFetch(401, { error: "Email or password is incorrect" }));
    const err = await api.signIn({ authRequest: "V2_1", email: "a", password: "b" }).catch((e) => e);
    expect(err).toBeInstanceOf(LoginApiError);
    expect(err.status).toBe(401);
    expect(err.message).toBe("Email or password is incorrect");
  });

  it("sends an empty JSON object for body-less calls", async () => {
    const fetchImpl = fakeFetch(200, {});
    await createLoginApi("http://x", fetchImpl).logout();
    expect(fetchImpl.mock.calls[0][0]).toBe("http://x/auth/v1/logout");
    expect(fetchImpl.mock.calls[0][1].body).toBe("{}");
  });
});
```

- [ ] **Step 2: Run; expect failure** (`Cannot find module './login-api'`): `pnpm --filter @cheladb/studio test`.

- [ ] **Step 3: Implement `login-api.ts`:**

```ts
/** Studio's sign-in pages talk to ui-auth through the gateway, with its HttpOnly cookies. */

export type LoginNext = { next: "done"; redirect: string } | { next: "verify" };

export class LoginApiError extends Error {
  readonly status: number;
  constructor(status: number, message: string) {
    super(message);
    this.name = "LoginApiError";
    this.status = status;
  }
}

export interface LoginApi {
  authRequest(authRequest: string): Promise<{ signUp: boolean }>;
  signIn(input: { authRequest: string; email: string; password: string }): Promise<LoginNext>;
  signUp(input: { authRequest: string; email: string; password: string; givenName: string; familyName: string }): Promise<LoginNext>;
  verifyEmail(code: string): Promise<LoginNext>;
  resendCode(): Promise<void>;
  requestPasswordReset(email: string): Promise<void>;
  confirmPasswordReset(input: { userId: string; code: string; password: string }): Promise<void>;
  logout(): Promise<void>;
}

const FALLBACK = "Sign-in is unavailable, try again";

export function createLoginApi(baseUrl: string, fetchImpl: typeof fetch = globalThis.fetch.bind(globalThis)): LoginApi {
  async function post<T>(path: string, body: unknown = {}): Promise<T> {
    let response: Response;
    try {
      response = await fetchImpl(`${baseUrl}/auth/v1/${path}`, {
        method: "POST",
        credentials: "include",
        headers: { "Content-Type": "application/json", Accept: "application/json" },
        body: JSON.stringify(body),
      });
    } catch {
      throw new LoginApiError(0, FALLBACK);
    }
    const text = await response.text().catch(() => "");
    let parsed: unknown = undefined;
    try {
      parsed = text ? JSON.parse(text) : undefined;
    } catch {
      parsed = undefined;
    }
    if (!response.ok) {
      const message = (parsed as { error?: unknown } | undefined)?.error;
      throw new LoginApiError(response.status, typeof message === "string" ? message : FALLBACK);
    }
    return parsed as T;
  }

  return {
    authRequest: (authRequest) => post("auth-request", { authRequest }),
    signIn: (input) => post("sign-in", input),
    signUp: (input) => post("sign-up", input),
    verifyEmail: (code) => post("verify-email", { code }),
    resendCode: async () => void (await post("resend-code")),
    requestPasswordReset: async (email) => void (await post("password-reset", { email })),
    confirmPasswordReset: async (input) => void (await post("password-reset/confirm", input)),
    logout: async () => void (await post("logout")),
  };
}
```

- [ ] **Step 4: Failing component tests** (`components/login/login-forms.test.tsx`):

```tsx
import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import { LoginApiError, type LoginApi } from "@/lib/login-api";
import { SignInForm } from "./sign-in-form";
import { SignUpForm } from "./sign-up-form";
import { VerifyEmailForm } from "./verify-email-form";

function api(overrides: Partial<LoginApi>): LoginApi {
  return {
    authRequest: vi.fn().mockResolvedValue({ signUp: false }),
    signIn: vi.fn(), signUp: vi.fn(), verifyEmail: vi.fn(), resendCode: vi.fn().mockResolvedValue(undefined),
    requestPasswordReset: vi.fn(), confirmPasswordReset: vi.fn(), logout: vi.fn(),
    ...overrides,
  };
}

describe("SignInForm", () => {
  it("signs in and follows ui-auth's redirect", async () => {
    const go = vi.fn();
    const signIn = vi.fn().mockResolvedValue({ next: "done", redirect: "http://localhost:3000/callback?code=c" });
    render(<SignInForm api={api({ signIn })} authRequest="V2_1" go={go} toVerify={vi.fn()} toRegister={vi.fn()} />);
    await userEvent.type(await screen.findByLabelText("Email"), "ada@example.com");
    await userEvent.type(screen.getByLabelText("Password"), "Secret-pass1");
    await userEvent.click(screen.getByRole("button", { name: "Sign in" }));
    await waitFor(() => expect(go).toHaveBeenCalledWith("http://localhost:3000/callback?code=c"));
    expect(signIn).toHaveBeenCalledWith({ authRequest: "V2_1", email: "ada@example.com", password: "Secret-pass1" });
  });

  it("shows ui-auth's message on failure and stays put", async () => {
    const go = vi.fn();
    const signIn = vi.fn().mockRejectedValue(new LoginApiError(401, "Email or password is incorrect"));
    render(<SignInForm api={api({ signIn })} authRequest="V2_1" go={go} toVerify={vi.fn()} toRegister={vi.fn()} />);
    await userEvent.type(await screen.findByLabelText("Email"), "a@b.c");
    await userEvent.type(screen.getByLabelText("Password"), "x");
    await userEvent.click(screen.getByRole("button", { name: "Sign in" }));
    expect(await screen.findByRole("alert")).toHaveTextContent("Email or password is incorrect");
    expect(go).not.toHaveBeenCalled();
  });

  it("forwards to sign-up when the pending sign-in asked for it", async () => {
    const toRegister = vi.fn();
    render(<SignInForm api={api({ authRequest: vi.fn().mockResolvedValue({ signUp: true }) })} authRequest="V2_1" go={vi.fn()} toVerify={vi.fn()} toRegister={toRegister} />);
    await waitFor(() => expect(toRegister).toHaveBeenCalled());
  });

  it("goes to the code step when the email isn't verified", async () => {
    const toVerify = vi.fn();
    render(<SignInForm api={api({ signIn: vi.fn().mockResolvedValue({ next: "verify" }) })} authRequest="V2_1" go={vi.fn()} toVerify={toVerify} toRegister={vi.fn()} />);
    await userEvent.type(await screen.findByLabelText("Email"), "a@b.c");
    await userEvent.type(screen.getByLabelText("Password"), "x");
    await userEvent.click(screen.getByRole("button", { name: "Sign in" }));
    await waitFor(() => expect(toVerify).toHaveBeenCalled());
  });

  it("offers to start again when the sign-in expired", async () => {
    render(<SignInForm api={api({ authRequest: vi.fn().mockRejectedValue(new LoginApiError(410, "This sign-in has expired, start again")) })} authRequest="V2_1" go={vi.fn()} toVerify={vi.fn()} toRegister={vi.fn()} />);
    expect(await screen.findByRole("alert")).toHaveTextContent("This sign-in has expired, start again");
    expect(screen.getByRole("link", { name: "Start again" })).toHaveAttribute("href", "/");
  });

  it("needs an authRequest", () => {
    render(<SignInForm api={api({})} authRequest="" go={vi.fn()} toVerify={vi.fn()} toRegister={vi.fn()} />);
    expect(screen.getByRole("alert")).toHaveTextContent("This sign-in has expired, start again");
  });
});

describe("SignUpForm", () => {
  it("creates the account and goes to the code step", async () => {
    const toVerify = vi.fn();
    const signUp = vi.fn().mockResolvedValue({ next: "verify" });
    render(<SignUpForm api={api({ signUp })} authRequest="V2_1" go={vi.fn()} toVerify={toVerify} />);
    await userEvent.type(screen.getByLabelText("First name"), "Ada");
    await userEvent.type(screen.getByLabelText("Last name"), "Lovelace");
    await userEvent.type(screen.getByLabelText("Email"), "ada@example.com");
    await userEvent.type(screen.getByLabelText("Password"), "Secret-pass1");
    await userEvent.click(screen.getByRole("button", { name: "Create account" }));
    await waitFor(() => expect(toVerify).toHaveBeenCalled());
    expect(signUp).toHaveBeenCalledWith({ authRequest: "V2_1", email: "ada@example.com", password: "Secret-pass1", givenName: "Ada", familyName: "Lovelace" });
  });
});

describe("VerifyEmailForm", () => {
  it("prefills the code from the email link, verifies and follows the redirect", async () => {
    const go = vi.fn();
    const verifyEmail = vi.fn().mockResolvedValue({ next: "done", redirect: "http://localhost:3000/callback?code=c" });
    render(<VerifyEmailForm api={api({ verifyEmail })} initialCode="123456" go={go} toVerify={vi.fn()} />);
    expect(screen.getByLabelText("Code")).toHaveValue("123456");
    await userEvent.click(screen.getByRole("button", { name: "Verify" }));
    await waitFor(() => expect(go).toHaveBeenCalledWith("http://localhost:3000/callback?code=c"));
  });

  it("resends the code", async () => {
    const resendCode = vi.fn().mockResolvedValue(undefined);
    render(<VerifyEmailForm api={api({ resendCode })} go={vi.fn()} toVerify={vi.fn()} />);
    await userEvent.click(screen.getByRole("button", { name: "Send a new code" }));
    expect(await screen.findByText("If your account needs it, a new code is on its way.")).toBeInTheDocument();
    expect(resendCode).toHaveBeenCalled();
  });
});
```

- [ ] **Step 5: Run; expect failure** (missing modules).

- [ ] **Step 6: Implement the forms.** A shared hook and layout live in `sign-in-form.tsx` and are exported for the other forms:

```tsx
// sign-in-form.tsx
import { useEffect, useState, type FormEvent, type ReactNode } from "react";
import { Button } from "@/components/ui/button";
import { LoginApiError, type LoginApi, type LoginNext } from "@/lib/login-api";

export const EXPIRED = "This sign-in has expired, start again";

export interface FormProps {
  api: LoginApi;
  authRequest?: string;
  initialCode?: string;
  go: (url: string) => void;
  toVerify: () => void;
  toRegister?: () => void;
}

export function messageOf(err: unknown): string {
  return err instanceof LoginApiError ? err.message : "Sign-in is unavailable, try again";
}

export function follow(next: LoginNext, go: (url: string) => void, toVerify: () => void) {
  if (next.next === "done") go(next.redirect);
  else toVerify();
}

export function LoginLayout({ title, children }: { title: string; children: ReactNode }) {
  return (
    <main className="flex min-h-screen items-center justify-center bg-background p-8 text-foreground">
      <div className="w-full max-w-sm space-y-6">
        <h1 className="text-2xl font-semibold tracking-tight">{title}</h1>
        {children}
      </div>
    </main>
  );
}

export function Field({ label, type = "text", value, onChange, autoComplete }: {
  label: string; type?: string; value: string; onChange: (v: string) => void; autoComplete?: string;
}) {
  const id = label.toLowerCase().replace(/\s+/g, "-");
  return (
    <div className="space-y-1">
      <label htmlFor={id} className="text-sm font-medium">{label}</label>
      <input id={id} type={type} value={value} autoComplete={autoComplete} required
        onChange={(e) => onChange(e.target.value)}
        className="w-full rounded-md border border-input bg-background px-3 py-2 text-sm" />
    </div>
  );
}

export function Problem({ message, restart }: { message: string; restart?: boolean }) {
  return (
    <div role="alert" className="text-sm text-destructive">
      {message}
      {restart && (<> <a href="/" className="underline">Start again</a></>)}
    </div>
  );
}

export function SignInForm({ api, authRequest = "", go, toVerify, toRegister }: FormProps) {
  const [email, setEmail] = useState("");
  const [password, setPassword] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(authRequest ? null : EXPIRED);

  useEffect(() => {
    if (!authRequest) return;
    api.authRequest(authRequest).then(
      ({ signUp }) => { if (signUp) toRegister?.(); },
      (err) => setError(messageOf(err)),
    );
  }, [api, authRequest, toRegister]);

  async function submit(event: FormEvent) {
    event.preventDefault();
    if (busy) return;
    setBusy(true);
    setError(null);
    try {
      follow(await api.signIn({ authRequest, email, password }), go, toVerify);
    } catch (err) {
      setError(messageOf(err));
      setBusy(false);
    }
  }

  return (
    <LoginLayout title="Sign in to Cheladb">
      <form onSubmit={submit} className="space-y-4">
        <Field label="Email" type="email" value={email} onChange={setEmail} autoComplete="username" />
        <Field label="Password" type="password" value={password} onChange={setPassword} autoComplete="current-password" />
        {error && <Problem message={error} restart={error === EXPIRED} />}
        <Button type="submit" disabled={busy || !authRequest} className="w-full">Sign in</Button>
      </form>
      <p className="text-sm text-muted-foreground">
        <a href="/password-reset" className="underline">Forgot your password?</a>
      </p>
    </LoginLayout>
  );
}
```

```tsx
// sign-up-form.tsx
import { useState, type FormEvent } from "react";
import { Button } from "@/components/ui/button";
import { EXPIRED, Field, LoginLayout, Problem, follow, messageOf, type FormProps } from "./sign-in-form";

export function SignUpForm({ api, authRequest = "", go, toVerify }: FormProps) {
  const [givenName, setGivenName] = useState("");
  const [familyName, setFamilyName] = useState("");
  const [email, setEmail] = useState("");
  const [password, setPassword] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(authRequest ? null : EXPIRED);

  async function submit(event: FormEvent) {
    event.preventDefault();
    if (busy) return;
    setBusy(true);
    setError(null);
    try {
      follow(await api.signUp({ authRequest, email, password, givenName, familyName }), go, toVerify);
    } catch (err) {
      setError(messageOf(err));
      setBusy(false);
    }
  }

  return (
    <LoginLayout title="Create your Cheladb account">
      <form onSubmit={submit} className="space-y-4">
        <Field label="First name" value={givenName} onChange={setGivenName} autoComplete="given-name" />
        <Field label="Last name" value={familyName} onChange={setFamilyName} autoComplete="family-name" />
        <Field label="Email" type="email" value={email} onChange={setEmail} autoComplete="username" />
        <Field label="Password" type="password" value={password} onChange={setPassword} autoComplete="new-password" />
        {error && <Problem message={error} restart={error === EXPIRED} />}
        <Button type="submit" disabled={busy || !authRequest} className="w-full">Create account</Button>
      </form>
    </LoginLayout>
  );
}
```

```tsx
// verify-email-form.tsx
import { useState, type FormEvent } from "react";
import { Button } from "@/components/ui/button";
import { Field, LoginLayout, Problem, follow, messageOf, type FormProps } from "./sign-in-form";

export function VerifyEmailForm({ api, initialCode = "", go, toVerify }: FormProps) {
  const [code, setCode] = useState(initialCode);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [resent, setResent] = useState(false);

  async function submit(event: FormEvent) {
    event.preventDefault();
    if (busy) return;
    setBusy(true);
    setError(null);
    try {
      follow(await api.verifyEmail(code.trim()), go, toVerify);
    } catch (err) {
      setError(messageOf(err));
      setBusy(false);
    }
  }

  async function resend() {
    await api.resendCode().catch(() => undefined);
    setResent(true);
  }

  return (
    <LoginLayout title="Check your email">
      <p className="text-sm text-muted-foreground">Enter the code we sent to your email.</p>
      <form onSubmit={submit} className="space-y-4">
        <Field label="Code" value={code} onChange={setCode} autoComplete="one-time-code" />
        {error && <Problem message={error} />}
        <Button type="submit" disabled={busy} className="w-full">Verify</Button>
      </form>
      <Button variant="outline" onClick={() => void resend()} className="w-full">Send a new code</Button>
      {resent && <p className="text-sm text-muted-foreground">If your account needs it, a new code is on its way.</p>}
    </LoginLayout>
  );
}
```

- [ ] **Step 7: Routes.** A shared hook gives each page its API client:

```tsx
// routes/login.tsx
import { createFileRoute, useNavigate } from "@tanstack/react-router";
import { useMemo } from "react";
import { SignInForm } from "@/components/login/sign-in-form";
import { useStudioEnv } from "@/lib/env-context";
import { createLoginApi } from "@/lib/login-api";

export const Route = createFileRoute("/login")({
  validateSearch: (search: Record<string, unknown>) => ({ authRequest: typeof search.authRequest === "string" ? search.authRequest : "" }),
  component: LoginPage,
});

function LoginPage() {
  const { authRequest } = Route.useSearch();
  const env = useStudioEnv();
  const navigate = useNavigate();
  const api = useMemo(() => createLoginApi(env.apiUrl), [env.apiUrl]);
  return (
    <SignInForm api={api} authRequest={authRequest}
      go={(url) => window.location.assign(url)}
      toVerify={() => void navigate({ to: "/verify", search: { code: "" } })}
      toRegister={() => void navigate({ to: "/register", search: { authRequest }, replace: true })} />
  );
}
```
`routes/register.tsx`: the same shape with `createFileRoute("/register")`, the same `validateSearch`, rendering `<SignUpForm api={api} authRequest={authRequest} go=… toVerify=… />`. `routes/verify.tsx`: `validateSearch: (s) => ({ code: typeof s.code === "string" ? s.code : "" })`, rendering `<VerifyEmailForm api={api} initialCode={code} go=… toVerify={() => undefined} />`.

- [ ] **Step 8: Regenerate routes and run.** `pnpm --filter @cheladb/studio generate-routes`; `pnpm --filter @cheladb/studio test` → all pass; `pnpm -r typecheck` → pass; `pnpm --filter @cheladb/studio build` → built.

- [ ] **Step 9: Commit:** `studio: sign-in, sign-up and email verification pages`.

---

### Task 10: Studio: `/password-reset` and `/logout`

**Files:**
- Create: `apps/studio/src/lib/safe-redirect.ts`, `apps/studio/src/lib/safe-redirect.test.ts`, `apps/studio/src/components/login/password-reset-form.tsx`, `apps/studio/src/components/login/password-reset-form.test.tsx`, `apps/studio/src/routes/password-reset.tsx`, `apps/studio/src/routes/logout.tsx`
- Modify: `apps/studio/src/routeTree.gen.ts` (regenerated)

**Interfaces:**
- Produces: `safeRedirect(target: string, origin: string): string` (the target if it parses and has exactly `origin`, else `"/"`); `PasswordResetForm` with props `{ api: LoginApi; userId?: string; code?: string }`.

- [ ] **Step 1: Failing tests.**

```ts
// safe-redirect.test.ts
import { describe, expect, it } from "vitest";
import { safeRedirect } from "./safe-redirect";

describe("safeRedirect", () => {
  it("allows Studio's own origin", () => {
    expect(safeRedirect("http://localhost:3000/", "http://localhost:3000")).toBe("http://localhost:3000/");
  });
  // Review Focus 4.
  it("logout_ignores_a_foreign_redirect", () => {
    for (const bad of ["https://evil.example/", "//evil.example", "javascript:alert(1)", "not a url", ""]) {
      expect(safeRedirect(bad, "http://localhost:3000")).toBe("/");
    }
  });
});
```

```tsx
// password-reset-form.test.tsx
import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import { LoginApiError, type LoginApi } from "@/lib/login-api";
import { PasswordResetForm } from "./password-reset-form";

const base = (o: Partial<LoginApi>): LoginApi => ({
  authRequest: vi.fn(), signIn: vi.fn(), signUp: vi.fn(), verifyEmail: vi.fn(), resendCode: vi.fn(),
  requestPasswordReset: vi.fn().mockResolvedValue(undefined), confirmPasswordReset: vi.fn().mockResolvedValue(undefined), logout: vi.fn(), ...o,
});

describe("PasswordResetForm", () => {
  it("requests a reset and always says the same thing", async () => {
    const api = base({});
    render(<PasswordResetForm api={api} />);
    await userEvent.type(screen.getByLabelText("Email"), "ada@example.com");
    await userEvent.click(screen.getByRole("button", { name: "Send reset link" }));
    expect(await screen.findByText("If an account exists for that email, we've sent a reset link.")).toBeInTheDocument();
    expect(api.requestPasswordReset).toHaveBeenCalledWith("ada@example.com");
  });

  it("sets the new password from the email link", async () => {
    const api = base({});
    render(<PasswordResetForm api={api} userId="u1" code="654321" />);
    await userEvent.type(screen.getByLabelText("New password"), "New-pass-12");
    await userEvent.click(screen.getByRole("button", { name: "Set password" }));
    expect(await screen.findByText("Your password is set. Sign in from Cheladb Studio.")).toBeInTheDocument();
    expect(api.confirmPasswordReset).toHaveBeenCalledWith({ userId: "u1", code: "654321", password: "New-pass-12" });
  });

  it("shows ui-auth's message when the code is bad", async () => {
    const api = base({ confirmPasswordReset: vi.fn().mockRejectedValue(new LoginApiError(400, "Code is invalid or expired")) });
    render(<PasswordResetForm api={api} userId="u1" code="bad" />);
    await userEvent.type(screen.getByLabelText("New password"), "New-pass-12");
    await userEvent.click(screen.getByRole("button", { name: "Set password" }));
    expect(await screen.findByRole("alert")).toHaveTextContent("Code is invalid or expired");
  });
});
```

- [ ] **Step 2: Run; expect failure** (missing modules).

- [ ] **Step 3: Implement.**

```ts
// safe-redirect.ts
/** `target` only if it is an absolute URL on `origin`; otherwise Studio's home. */
export function safeRedirect(target: string, origin: string): string {
  try {
    const url = new URL(target);
    return url.origin === origin ? url.toString() : "/";
  } catch {
    return "/";
  }
}
```

```tsx
// password-reset-form.tsx
import { useState, type FormEvent } from "react";
import { Button } from "@/components/ui/button";
import type { LoginApi } from "@/lib/login-api";
import { Field, LoginLayout, Problem, messageOf } from "./sign-in-form";

export function PasswordResetForm({ api, userId = "", code = "" }: { api: LoginApi; userId?: string; code?: string }) {
  const confirming = userId !== "" && code !== "";
  const [email, setEmail] = useState("");
  const [password, setPassword] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [done, setDone] = useState(false);

  async function submit(event: FormEvent) {
    event.preventDefault();
    if (busy) return;
    setBusy(true);
    setError(null);
    try {
      if (confirming) await api.confirmPasswordReset({ userId, code, password });
      else await api.requestPasswordReset(email);
      setDone(true);
    } catch (err) {
      setError(messageOf(err));
    } finally {
      setBusy(false);
    }
  }

  if (done) {
    return (
      <LoginLayout title="Reset your password">
        <p className="text-sm">
          {confirming ? "Your password is set. Sign in from Cheladb Studio." : "If an account exists for that email, we've sent a reset link."}
        </p>
        <a href="/" className="text-sm underline">Back to Cheladb Studio</a>
      </LoginLayout>
    );
  }
  return (
    <LoginLayout title="Reset your password">
      <form onSubmit={submit} className="space-y-4">
        {confirming
          ? <Field label="New password" type="password" value={password} onChange={setPassword} autoComplete="new-password" />
          : <Field label="Email" type="email" value={email} onChange={setEmail} autoComplete="username" />}
        {error && <Problem message={error} />}
        <Button type="submit" disabled={busy} className="w-full">{confirming ? "Set password" : "Send reset link"}</Button>
      </form>
    </LoginLayout>
  );
}
```

```tsx
// routes/password-reset.tsx
import { createFileRoute } from "@tanstack/react-router";
import { useMemo } from "react";
import { PasswordResetForm } from "@/components/login/password-reset-form";
import { useStudioEnv } from "@/lib/env-context";
import { createLoginApi } from "@/lib/login-api";

const str = (v: unknown) => (typeof v === "string" ? v : "");

export const Route = createFileRoute("/password-reset")({
  validateSearch: (s: Record<string, unknown>) => ({ userId: str(s.userId), code: str(s.code) }),
  component: PasswordResetPage,
});

function PasswordResetPage() {
  const { userId, code } = Route.useSearch();
  const env = useStudioEnv();
  const api = useMemo(() => createLoginApi(env.apiUrl), [env.apiUrl]);
  return <PasswordResetForm api={api} userId={userId} code={code} />;
}
```

```tsx
// routes/logout.tsx
import { createFileRoute } from "@tanstack/react-router";
import { useEffect } from "react";
import { useStudioEnv } from "@/lib/env-context";
import { createLoginApi } from "@/lib/login-api";
import { safeRedirect } from "@/lib/safe-redirect";

export const Route = createFileRoute("/logout")({
  validateSearch: (s: Record<string, unknown>) => ({
    post_logout_redirect: typeof s.post_logout_redirect === "string" ? s.post_logout_redirect : "",
  }),
  component: LogoutPage,
});

function LogoutPage() {
  const { post_logout_redirect } = Route.useSearch();
  const env = useStudioEnv();
  useEffect(() => {
    const target = safeRedirect(post_logout_redirect, window.location.origin);
    // Leave even if ui-auth is unreachable: the Zitadel tokens are already ended.
    void createLoginApi(env.apiUrl).logout().catch(() => undefined).finally(() => window.location.replace(target));
  }, [env.apiUrl, post_logout_redirect]);
  return <main className="p-8 text-muted-foreground">Signing you out…</main>;
}
```

- [ ] **Step 4: Regenerate routes and run.** `pnpm --filter @cheladb/studio generate-routes && pnpm --filter @cheladb/studio test && pnpm -r typecheck && pnpm --filter @cheladb/studio build` → all pass.

- [ ] **Step 5: Commit:** `studio: password reset and logout pages`.

---

### Task 11: Live end to end, docs, review

**Files:**
- Create: `scripts/auth-e2e.sh`
- Modify: `justfile` (`auth-e2e`), `docs/roadmap/2026-09-28-neon-and-data-api.md`, `README.md`

- [ ] **Step 1: The script** (`scripts/auth-e2e.sh`, `chmod +x`). It drives the real OIDC flow with curl through the gateway, reading codes from Mailpit:

```bash
#!/usr/bin/env bash
# Live check of Zitadel v4 + ui-auth + the gateway (needs `just dev`): sign-up with a code
# from Mailpit, token exchange, /v1/me, sign-in, identical failures, password reset, logout.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
set -a; source "$ROOT/.cheladb/zitadel/demo.env"; set +a
CLIENT="$(sed -n 's/^VITE_ZITADEL_CLIENT_ID=//p' "$ROOT/apps/studio/.env.local")"
ISS="$CHELADB_ZITADEL_ISSUER"
GW="${GATEWAY_URL:-http://127.0.0.1:8000}"
MAILPIT="${MAILPIT_URL:-http://localhost:8025}"
STUDIO="http://localhost:3000"
JAR="$(mktemp)"; trap 'rm -f "$JAR"' EXIT
EMAIL="e2e-$(date +%s)-$RANDOM@example.com"
PASSWORD="E2e-pass-$(openssl rand -hex 4)A!"

say() { echo "[auth-e2e] $*"; }
die() { echo "[auth-e2e] FAIL: $*" >&2; exit 1; }

b64url() { openssl base64 -A | tr '+/' '-_' | tr -d '='; }
VERIFIER="$(openssl rand -hex 32)"
CHALLENGE="$(printf %s "$VERIFIER" | openssl dgst -sha256 -binary | b64url)"

# authorize [prompt] → the authRequest id from Zitadel's redirect to Studio's /login
authorize() {
  local scope="openid profile email urn:zitadel:iam:user:resourceowner urn:zitadel:iam:org:project:id:${CHELADB_ZITADEL_PROJECT_ID}:aud"
  local loc
  loc="$(curl -sS -o /dev/null -w '%{redirect_url}' -G "$ISS/oauth/v2/authorize" \
    --data-urlencode "client_id=$CLIENT" --data-urlencode "redirect_uri=$STUDIO/callback" \
    --data-urlencode "response_type=code" --data-urlencode "scope=$scope" \
    --data-urlencode "code_challenge=$CHALLENGE" --data-urlencode "code_challenge_method=S256" \
    ${1:+--data-urlencode "prompt=$1"})"
  [[ "$loc" == "$STUDIO/login?authRequest="* ]] || die "authorize redirected to: $loc"
  echo "${loc#*authRequest=}"
}

# ui <path> <json> → body; status in $STATUS
ui() {
  local out
  out="$(curl -sS -b "$JAR" -c "$JAR" -H "Origin: $STUDIO" -H 'Content-Type: application/json' \
    -w '\n%{http_code}' -X POST "$GW/auth/v1/$1" -d "$2")"
  STATUS="${out##*$'\n'}"; printf '%s' "${out%$'\n'*}"
}

# mail_link <to> <kind: verify|password-reset> → the link in the newest such email
mail_link() {
  local id
  for _ in $(seq 30); do
    id="$(curl -sS -G "$MAILPIT/api/v1/search" --data-urlencode "query=to:$1" | jq -r '.messages[0].ID // empty')"
    if [[ -n "$id" ]]; then
      curl -sS "$MAILPIT/api/v1/message/$id" | jq -r '.Text' | grep -oE "$STUDIO/$2\?[^ \"<>]+" | head -n1 && return 0
    fi
    sleep 1
  done
  die "no $2 email for $1"
}

# exchange <callback url> → access token
exchange() {
  local code; code="$(sed -E 's/.*[?&]code=([^&]+).*/\1/' <<<"$1")"
  curl -sS -X POST "$ISS/oauth/v2/token" -d grant_type=authorization_code -d "code=$code" \
    -d "redirect_uri=$STUDIO/callback" -d "client_id=$CLIENT" -d "code_verifier=$VERIFIER" | jq -r .access_token
}

ar="$(authorize create)"
[[ "$(ui auth-request "{\"authRequest\":\"$ar\"}")" == '{"signUp":true}' ]] || die "prompt=create not reported"
ui sign-up "{\"authRequest\":\"$ar\",\"email\":\"$EMAIL\",\"password\":\"$PASSWORD\",\"givenName\":\"E\",\"familyName\":\"Two\"}" >/dev/null
[[ "$STATUS" == 200 ]] || die "sign-up: $STATUS"
code="$(mail_link "$EMAIL" verify | sed -E 's/.*code=([^&]+).*/\1/')"
done_json="$(ui verify-email "{\"code\":\"$code\"}")"
redirect="$(jq -r .redirect <<<"$done_json")"
[[ "$redirect" == "$STUDIO/callback?"* ]] || die "verify-email: $done_json"
token="$(exchange "$redirect")"
[[ -n "$token" && "$token" != null ]] || die "no access token"
me="$(curl -sS -H "Authorization: Bearer $token" "$GW/v1/me")"
jq -e '.tenant != null' <<<"$me" >/dev/null || die "/v1/me: $me"
say "signed up, verified, exchanged the code and resolved the tenant"

ar="$(authorize)"
signed="$(ui sign-in "{\"authRequest\":\"$ar\",\"email\":\"$EMAIL\",\"password\":\"$PASSWORD\"}")"
[[ "$(jq -r .next <<<"$signed")" == done ]] || die "sign-in: $signed"
say "signed in with the password"

wrong="$(ui sign-in "{\"authRequest\":\"$(authorize)\",\"email\":\"$EMAIL\",\"password\":\"nope\"}")"; w_status="$STATUS"
unknown="$(ui sign-in "{\"authRequest\":\"$(authorize)\",\"email\":\"nobody-$RANDOM@example.com\",\"password\":\"nope\"}")"
[[ "$w_status" == 401 && "$STATUS" == 401 && "$wrong" == "$unknown" ]] || die "failures differ: $wrong vs $unknown"
say "a wrong password and an unknown email look identical"

ui password-reset "{\"email\":\"$EMAIL\"}" >/dev/null
link="$(mail_link "$EMAIL" password-reset)"
uid="$(sed -E 's/.*userId=([^&]+).*/\1/' <<<"$link")"; rcode="$(sed -E 's/.*code=([^&]+).*/\1/' <<<"$link")"
NEWPW="E2e-new-$(openssl rand -hex 4)B!"
ui password-reset/confirm "{\"userId\":\"$uid\",\"code\":\"$rcode\",\"password\":\"$NEWPW\"}" >/dev/null
[[ "$STATUS" == 200 ]] || die "confirm: $STATUS"
again="$(ui sign-in "{\"authRequest\":\"$(authorize)\",\"email\":\"$EMAIL\",\"password\":\"$NEWPW\"}")"
[[ "$(jq -r .next <<<"$again")" == done ]] || die "sign-in with the new password: $again"
say "reset the password and signed in with the new one"

out_headers="$(curl -sS -D - -o /dev/null -b "$JAR" -c "$JAR" -H "Origin: $STUDIO" -H 'Content-Type: application/json' -X POST "$GW/auth/v1/logout" -d '{}')"
grep -qi '^set-cookie: cheladb_session=;.*Max-Age=0' <<<"$out_headers" || die "logout didn't clear the session cookie"
say "logged out"
say "ok"
```
Add to `justfile`, after `neon-test-live`:

```just
# Live sign-up/sign-in/reset/logout through the gateway and ui-auth (needs just dev)
auth-e2e:
    scripts/auth-e2e.sh
```

- [ ] **Step 2: Run it.** `just stop` (if running), `just zitadel-reset`, `just dev`, then `just auth-e2e` → ends `[auth-e2e] ok`. If a step fails, fix the cause in the owning task's code (not the script), re-run the unit tests, and commit the fix separately.

- [ ] **Step 3: Manual browser pass.** In a browser at `http://localhost:3000`: Create account → verify with the code from Mailpit (`http://localhost:8025`) → dashboard; sign out → back at `/`; Sign in → dashboard; "Forgot your password?" → reset from the Mailpit link → sign in with the new password. Record the result in the commit message.

- [ ] **Step 4: Review against Zitadel's own login.** Compare `apps/ui-auth/src/app.rs` with Zitadel's `apps/login` at v4.19.1 (session creation, `CreateCallback`, the email-verification gate) and note any behaviour we lack in the roadmap's findings. Do not add features beyond the spec.

- [ ] **Step 5: Docs.** Roadmap: mark "Before step 3: Zitadel v4" done (2026-09-28) with a short "Progress" entry: what exists (Zitadel v4.19.1, `ui-auth`, Studio pages, the rebuilt bootstrap, `just auth-e2e`) and findings (anything surprising from Steps 2–4, e.g. differences between the mock and real Zitadel). README: add `ui-auth` to the service list and `just auth-e2e` to the commands.

- [ ] **Step 6: Final checks.** `cargo test --workspace`, `cargo clippy --workspace --all-targets`, `just neon-test`, `pnpm -r typecheck`, `pnpm --filter @cheladb/studio test`, `pnpm --filter @cheladb/studio build` → all pass.

- [ ] **Step 7: Commit:** `auth: live end-to-end check; roadmap and README`.
