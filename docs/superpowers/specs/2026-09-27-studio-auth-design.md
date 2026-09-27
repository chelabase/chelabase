# Studio Sign-up / Sign-in + Project Landing — Design Spec

**Date:** 2026-09-27  
**Status:** Approved (plan mode)  
**License:** Apache-2.0  
**Depends on:** [2026-09-27-gateway-design.md](./2026-09-27-gateway-design.md)

## Intent

Give Studio a real user flow without building our own auth UI. Users sign up or sign in on **Zitadel's hosted login**, return to Studio, and land on a page that shows their tenant and project, lists the project's databases, and can create one. All API calls go through the gateway on `:8000` with the user's Zitadel JWT.

## Decisions

- **Signups join the bootstrapped demo org** (single-org self-hosted variant). Checked on the running Zitadel: self-registration is enabled (`allowRegister: true`), and the bootstrapped org is the instance default org, which is already bound to the demo tenant.
- **The landing page includes create-database**, not just a read-only view.
- **Keep "one org + tenant per signup" a small change later.** Three seams make that possible:
  - Studio never hard-codes tenant or project IDs.
  - `GET /v1/me` returns `tenant: null` for an unbound org.
  - Pinning to the demo org is one scope driven by one env var.
- **Mailpit catches email in local dev.** Zitadel's hosted signup requires an emailed verification code, and no SMTP is configured.

## Flow

```
Studio :3000 ──PKCE──► Zitadel hosted login :8080 (sign in / register; email code → Mailpit :8025)
      ◄── /callback (code → tokens; JWT access token)
Studio ──Bearer──► gateway :8000 ──x-cheladb-identity──► orchestrator
        GET /v1/me          user + tenant + projects + databases
        POST /v1/databases  create; poll GET /v1/databases/{id}
```

## Components

### Local infra
- A `mailpit` Compose service. SMTP is reached inside the network as `mailpit:1025`; the web UI is on host `:8025`.

### Bootstrap (`cheladb auth bootstrap`, idempotent)
- **Ensure a `cheladb-studio` OIDC app** in the `cheladb` project, found by name on re-runs:
  - user-agent type, PKCE (auth method none), code + refresh-token grants
  - **JWT access tokens**, so the gateway can verify them via JWKS
  - `devMode: true`, allowing plain-http localhost redirects
  - redirect `http://localhost:3000/callback`, post-logout `http://localhost:3000/`
- **Ensure one active SMTP provider** pointing at `mailpit:1025`, never duplicated.
- **Write `apps/studio/.env.local`** (gitignored), containing:
  - `VITE_ZITADEL_ISSUER`
  - `VITE_ZITADEL_CLIENT_ID`
  - `VITE_ZITADEL_PROJECT_ID`
  - `VITE_ZITADEL_ORG_ID`
  - `VITE_API_URL=http://127.0.0.1:8000`

### API: `GET /v1/me`
Declared in `cheladb-routes` as `orchestrator::ME` (`Auth::Platform`, `Cors::Studio`).

```json
{
  "user": { "sub": "…", "org_id": "…" },
  "tenant": {
    "id": "…", "slug": "demo",
    "projects": [
      { "id": "…", "slug": "default",
        "databases": [ { "database_id": "…", "name": "web1", "status": "ready", "host": "127.0.0.1", "port": 49153 } ] }
    ]
  }
}
```

- `tenant` is `null` when the caller's org has no tenant, with a `200` response. Other endpoints keep answering `403` for unbound orgs.
- The response never includes `admin_password` or `container_id`.
- Orchestrator:
  - A `Caller` extractor verifies the internal identity without requiring a tenant. `AuthPrincipal` builds on it, so verification lives in one place.
- Metadata:
  - `tenant_overview(tenant_id) -> Option<TenantOverview>` returns the slug, projects, and their databases, using three simple queries (tenant, projects, databases).
  - A shared row→`DatabaseRecord` mapper is used by both `get_database` and `tenant_overview`.

### Studio
- **Auth:** `oidc-client-ts` + `react-oidc-context`. A pure `buildOidcConfig(env)` sets these scopes:
  - `openid profile email offline_access`
  - `urn:zitadel:iam:user:resourceowner`, which gives the gateway the org claim
  - `urn:zitadel:iam:org:project:id:{projectId}:aud`, so `aud` matches the gateway's configured audiences
  - `urn:zitadel:iam:org:id:{orgId}`, **the demo-org pin**
- **Routes:**
  - `/` is public, with Sign in and Create account (`prompt=create`). An authenticated visitor is sent to `/app`.
  - `/callback` finishes sign-in and goes to `/app`.
  - `/app` is protected and shows:
    - user name and email from the ID token
    - tenant and project
    - a databases table
    - a create form, which uses the IDs from `/v1/me` and polls until `ready` or `failed`
    - sign out
  - When `tenant` is `null`, the page says the organization isn't set up yet.
- **API client:** a typed client attaches the bearer token, surfaces `{"error"}` bodies, and restarts sign-in on `401`. `@cheladb/types` switches to the real snake_case API shapes.
- **Cleanup:** the unused `router.tsx` template and the placeholder store go. `vite.config.ts` gets `strictPort: true`, because the redirect URI is registered for port 3000.

## Moving to one org per signup later (out of scope)
- Drop the org-pin scope and enable Zitadel's register-organization flow.
- Add `POST /v1/tenants`, which creates a tenant and project and binds the caller's org. This is the tenant-lifecycle seam already planned for the managed layer.
- Have the `tenant: null` state call that endpoint.

## Out of scope

Organization onboarding, roles and RBAC in Studio, database deletion, showing connection secrets, production redirect URIs and TLS, and styling beyond clean Tailwind.

## Testing

- **Rust:**
  - Orchestrator `/v1/me`: bound org, unbound org (`tenant: null`), missing identity (`401`), and no secrets in the body.
  - The drift test covers the new route.
  - Bootstrap has pure-function tests for the app payload, SMTP payload and env rendering.
- **Studio (Vitest + Testing Library):**
  - env validation
  - OIDC scopes
  - API client: bearer token, error mapping, `401` handling
  - dashboard states
  - create-form submit and poll
- **End to end:** `just dev`, then sign up in the browser, verify the code from Mailpit, see the dashboard, create a database, and sign out and back in.
  - This is the one check a human must do in a browser: Zitadel's hosted pages aren't scriptable from this repo without adding a browser runner.
  - It also confirms the one Zitadel behavior that can't be checked offline: that a user's JWT access token carries the resourceowner org claim.
