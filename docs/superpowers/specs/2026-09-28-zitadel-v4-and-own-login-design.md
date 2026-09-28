# Zitadel v4 and our own login (`ui-auth`)

**Date:** 2026-09-28
**Status:** Design approved in conversation; spec awaiting review.
**Roadmap:** the "Before step 3: Zitadel v4" item in `docs/roadmap/2026-09-28-neon-and-data-api.md`.

## Goal

Move local identity from Zitadel v2.71.19, which stopped getting releases on 2025-11-12, to
**v4.19.1**, and replace Zitadel's hosted login with **our own**:

- sign-in pages in Studio (Vite)
- a small standalone Rust service, `ui-auth`, that alone holds Zitadel's login-client token

Phase 1 covers what Studio uses today: sign-in with email and password, sign-up with email
verification, password reset and logout.

**Done when** `cargo test`, clippy, the Studio checks and `just auth-e2e` pass, and a manual
browser run through sign-up, sign-in, reset and logout in Studio works.

## Decisions

| Question | Decision |
|---|---|
| Upgrade path | **Fresh v4 instance.** `just zitadel-reset` wipes the local v2 instance once. Zitadel's in-place path (v2 → v3.4.x → wait out advisory A-10017 → v4, then adopt Login V2 by API) isn't worth it for local-only data. Accounts made earlier in Studio sign up again. |
| Login UI | **Our own**, not Zitadel's Login V2 container (a Next.js app). Zitadel supports custom login UIs on its Session and OIDC v2 APIs ("Build your own Login UI" in its docs). We get full control and no Node service to run. |
| Where the pages live | **In Studio**, as routes. Customers' end users will never sign in on a Cheladb site; for them we'll offer component examples to host in their own apps (data API, later). |
| Where the token lives | **`ui-auth`, server side.** The login client's token (`IAM_LOGIN_CLIENT`) can create sessions and finalize any pending sign-in, so it must never reach a browser. |
| State between steps | **An encrypted HttpOnly cookie** set by `ui-auth` (session id and token, pending sign-in, step, expiry). Page scripts can't read it; multi-step flows (email code now, MFA later) work. |
| Revealing accounts | **Reveal nothing.** Sign-in, sign-up and reset answer identically whether or not an account exists; rate limits and response padding back this up. |
| Dev "skip email verification" | **A `ui-auth` flag.** In dev, sign-up creates the user with the email already verified. The Actions v1 hook is deleted: Zitadel's docs say Actions v1 must be reworked into Actions v2 (a webhook we'd host) for Login V2. |
| Existing Zitadel code | **Rebuilt from scratch on v4, not ported.** `cheladb auth bootstrap` is rewritten on Zitadel's v2 APIs (v1 only where v4 has no replacement); `packages/zitadel` is deleted. Kept, because they're standard OIDC/JWT rather than Zitadel-version code: Studio's OIDC client, `cheladb-auth`'s JWKS verification, the gateway and the orchestrator's identity handling. |
| `ui-auth` → Zitadel client | Typed `serde` structs over reqwest, against the v2 REST paths below. No generated client. |

## Components

### Zitadel v4.19.1 (Compose)

`ghcr.io/zitadel/zitadel:v4.19.1`, pinned by index digest; `start-from-init`, with the
current database, masterkey, external-domain and first-instance settings, plus:

| Variable | Value |
|---|---|
| `ZITADEL_DEFAULTINSTANCE_FEATURES_LOGINV2_REQUIRED` | `true` |
| `ZITADEL_DEFAULTINSTANCE_FEATURES_LOGINV2_BASEURI` | `http://localhost:3000/` |
| `ZITADEL_OIDC_DEFAULTLOGINURLV2` | `http://localhost:3000/login?authRequest=` |
| `ZITADEL_OIDC_DEFAULTLOGOUTURLV2` | `http://localhost:3000/logout?post_logout_redirect=` |
| `ZITADEL_FIRSTINSTANCE_LOGINCLIENTPATPATH` | `/zitadel-bootstrap/login-client.pat` |
| `ZITADEL_FIRSTINSTANCE_ORG_LOGINCLIENT_MACHINE_USERNAME` | `login-client` |
| `ZITADEL_FIRSTINSTANCE_ORG_LOGINCLIENT_MACHINE_NAME` | `Cheladb ui-auth` |
| `ZITADEL_FIRSTINSTANCE_ORG_LOGINCLIENT_PAT_EXPIRATIONDATE` | `2099-01-01T00:00:00Z` |

`FIRSTINSTANCE_*` and `DEFAULTINSTANCE_*` apply only when the instance is created, hence the
reset. The port stays `127.0.0.1:8080`. There is no `zitadel-login` container.

### `apps/ui-auth` (new Rust service)

- axum on `CHELADB_UI_AUTH_ADDR`, default `127.0.0.1:8084`.
- A second `Service` in `cheladb-routes` (`UiAuth`), so the gateway routes it and the service
  mounts the same path declarations. Every endpoint is `Auth::Public` with a new
  `Cors::StudioCredentials` policy.
- Configuration: the Zitadel issuer (`CHELADB_ZITADEL_ISSUER`), the login-client token file
  (`CHELADB_UI_AUTH_TOKEN_FILE`, default `.cheladb/zitadel/login-client.pat`), the cookie key
  file (`CHELADB_UI_AUTH_COOKIE_KEY`, default `.cheladb/ui-auth/cookie.key`), the org to sign
  users up into (`CHELADB_ZITADEL_ORG_ID`, from `demo.env`), the Studio origins
  (`CHELADB_STUDIO_ORIGINS`), and `CHELADB_DEV_SKIP_EMAIL_VERIFICATION`.
- Zitadel calls (v4.19.1 REST paths, checked against its protos):

  | Purpose | Call |
  |---|---|
  | Read a pending sign-in | `GET /v2/oidc/auth_requests/{id}` |
  | Finalize it | `POST /v2/oidc/auth_requests/{id}` (`CreateCallback`) |
  | Session with user and password checks | `POST /v2/sessions`, `PATCH /v2/sessions/{id}` |
  | End a session | `DELETE /v2/sessions/{id}` |
  | Sign-up | `POST /v2/users/new` (`CreateUser`; `AddHumanUser` is deprecated) |
  | Email code | `POST /v2/users/{id}/email/verify`, `POST /v2/users/{id}/email/resend` |
  | Password reset | `POST /v2/users/{id}/password_reset`, `POST /v2/users/{id}/password` |
  | Find a user by email | `POST /v2/users` (`ListUsers`) |
  | Is the email verified | `GET /v2/users/{id}` |

- Its endpoints, all `POST` with JSON bodies:

  | Path | Body | Answer |
  |---|---|---|
  | `/auth/v1/auth-request` | `authRequest` | `{signUp: bool}` (the pending sign-in asked for `prompt=create`), or `410` |
  | `/auth/v1/sign-in` | `authRequest`, `email`, `password` | `{next: "done", redirect}` or `{next: "verify"}` |
  | `/auth/v1/sign-up` | `authRequest`, `email`, `password`, `givenName`, `familyName` | `{next: "verify"}` (dev flag: `done`) |
  | `/auth/v1/verify-email` | `code` | `{next: "done", redirect}` |
  | `/auth/v1/resend-code` | none | `{}` |
  | `/auth/v1/password-reset` | `email` | `{}` |
  | `/auth/v1/password-reset/confirm` | `userId`, `code`, `password` | `{}` |
  | `/auth/v1/logout` | none | `{}` |

### Studio

- New routes: `/login`, `/register`, `/verify`, `/password-reset`, `/logout`. They read
  `authRequest` (or `post_logout_redirect`) from the URL, call `ui-auth` through the gateway
  with `credentials: "include"`, and follow the `redirect` it returns.
- The landing page's "Sign in" and "Create account" still start the OIDC flow
  (`signinRedirect`, `prompt: "create"`); Zitadel then redirects to `/login` or `/register`.
  `/login` asks `/auth/v1/auth-request` first and forwards to `/register` when the pending
  sign-in asked for sign-up.
- The OIDC client (PKCE, `/callback`, scopes, token renewal) is unchanged.

### Gateway

- Routes `UiAuth` endpoints to `ui-auth` (`CHELADB_UI_AUTH_UPSTREAM`, default
  `127.0.0.1:8084`).
- `Cors::StudioCredentials`: echoes an allowed Studio origin and sends
  `Access-Control-Allow-Credentials: true`; never for other origins, never with `*`.
- `X-Forwarded-For` is unchanged: the gateway already **appends** the client connection's
  address (so an operator's proxy keeps its entries). `ui-auth` uses the **rightmost** entry,
  the one the gateway added, which a client can't forge.

### Bootstrap (`cheladb auth bootstrap`), rebuilt

The current bootstrap (v1 management API, Actions v1) is deleted and rewritten. The new one
is idempotent and does, in order:

1. Create `.cheladb/gateway` keys, the secrets key and `.cheladb/ui-auth/cookie.key`, each
   once and never overwritten (unchanged helpers, generalized for the cookie key).
2. Wait for Zitadel, authenticated with the first instance's admin PAT.
3. Resolve the admin's org.
4. Ensure the project, the API app and the Studio OIDC app (redirect URIs `/callback` and
   `/`, PKCE, access tokens as JWT).
5. Ensure the demo machine user and its key (`demo-sa.json`).
6. Ensure the Mailpit SMTP configuration is the active one.
7. Check that Zitadel wrote `login-client.pat`; if not, say to run `just zitadel-reset`.
8. Bind the demo tenant to the org, and write `demo.env` (adding `CHELADB_UI_AUTH_TOKEN_FILE`
   and `CHELADB_UI_AUTH_COOKIE_KEY`) and Studio's `.env.local`.

Every call uses a v2 API where Zitadel v4.19.1 has one (users, keys, projects, apps,
settings), and v1 only where it doesn't. The plan lists which. No Actions of any version.

### `packages/zitadel`, deleted

Studio doesn't import it. Studio reads the user's profile from standard OIDC userinfo
(`react-oidc-context` already loads it) and everything else from our `/v1/me`. The package,
its vendored OpenAPI spec, orval and `just zitadel-generate` are removed.

### `just`

- `just dev` also builds and starts `ui-auth`.
- `just ui-auth` runs it in the foreground.
- `just auth-e2e` runs the live check below.

## Behaviour

### The cookies

- `cheladb_login`: the record `{session_id, session_token, auth_request, user_id?, step,
  decoy, exp}`, sealed with AES-256-GCM through `cheladb-secrets` (its own purpose string,
  its own key). `HttpOnly; SameSite=Strict; Path=/auth/v1; Max-Age=900`, plus `Secure`
  outside local dev. Cleared when a flow finishes.
- `cheladb_session`: `{session_id, session_token}` of the finished sign-in, sealed the same
  way, 30 days, same attributes. Only used to end the session at logout.
- A cookie that fails to open, has expired or is from another step is treated as absent.

**Request forgery** is blocked by `SameSite=Strict`, the credentialed CORS allowing only the
Studio origins, and every endpoint requiring `Content-Type: application/json` (which forces a
preflight).

### Sign-in

1. `GET` the pending sign-in; missing or expired → `410`.
2. `POST /v2/sessions` with a user check (login name = email) and a password check.
3. If the user's email is unverified: resend the code, set `cheladb_login` at step
   `verify`, answer `{next: "verify"}`.
4. Otherwise `CreateCallback` with the session, set `cheladb_session`, answer
   `{next: "done", redirect}`.

Unknown user, wrong password or locked account → `401 {"error": "Email or password is
incorrect"}`, byte-identical in every case.

### Sign-up

1. `CreateUser` in the configured org with the password and an email code whose link is
   `http://localhost:3000/verify?code={{.Code}}`. With the dev flag, the email is created
   verified.
2. Create a session (user and password checks), set `cheladb_login` at step `verify`, answer
   `{next: "verify"}`. With the dev flag, finalize at once and answer `done`.
3. If the email is already taken, answer exactly as in step 2, with a cookie marked
   `decoy: true` and no session.

### Verify email, resend

- `verify-email`: needs `cheladb_login` at step `verify`. `VerifyEmail(user, code)`, then
  `CreateCallback`, set `cheladb_session`, clear `cheladb_login`, answer `done`. A wrong or
  expired code, or a decoy cookie → `400 {"error": "Code is invalid or expired"}`.
- `resend-code`: resends when the cookie has a real user; always `200 {}`.

### Password reset

- `password-reset`: always `200 {}` straight away. In the background: `ListUsers` by email in
  the org; if found, `PasswordReset` with a link to
  `http://localhost:3000/password-reset?userId={{.UserID}}&code={{.Code}}`.
- `password-reset/confirm`: `SetPassword` with the code. Wrong or expired →
  `400 {"error": "Code is invalid or expired"}`. On success the user signs in normally.

### Logout

Zitadel's end-session sends the browser to `/logout?post_logout_redirect=…`. Studio calls
`/auth/v1/logout`, which deletes the session in `cheladb_session` and clears both cookies.
Studio then follows `post_logout_redirect` only if its origin is one of the Studio origins
(otherwise it goes to `/`), so the page can't be used as an open redirect.

### Safeguards

- **Rate limits**, in memory (one `ui-auth` process), keyed on the client IP (the
  rightmost `X-Forwarded-For` entry, added by the gateway) and on the normalized email (trimmed, lowercased):

  | Endpoint | Per IP | Per email |
  |---|---|---|
  | sign-in | 10 / minute | 5 / minute |
  | sign-up, password-reset, resend-code | 20 / hour | 5 / hour |

  Over the limit → `429 {"error": "Too many attempts, try again later"}`, whatever the account
  status.
- **Timing:** sign-in and sign-up answers are padded to at least 500 ms (Zitadel answers
  faster for unknown users: no password hash). Reset emails go out in the background.
- **Logging:** never passwords, codes, tokens or cookie contents; emails only as a hash.

### Zitadel failures

Unreachable, timeout or 5xx → `503 {"error": "Sign-in is unavailable, try again"}`. A missing
or expired pending sign-in → `410`; Studio restarts sign-in from the landing page.

## Testing

- **`ui-auth`** (`cargo test`), against an in-process mock of Zitadel's v2 API (axum, like
  `storcon_mock.rs`):
  - sign-in: success finalizes and returns Zitadel's redirect; unverified email → `verify`
    and a code is sent; unknown user, wrong password and locked account give byte-identical
    `401`s; answers take at least 500 ms
  - sign-up: success → `verify`; a taken email → identical answer with a decoy cookie, whose
    verify fails exactly like a wrong code; dev flag → `done`, user created verified
  - cookies: tampered, expired, wrong-key or wrong-step cookies are rejected; attributes as
    specified; contents never readable as plain text
  - reset: identical answer whether or not the user exists; the email is sent in the
    background
  - rate limits: the 6th sign-in per email per minute → `429`; keyed on the rightmost
    `X-Forwarded-For` entry, so a client-supplied leftmost entry can't dodge them
  - logout: deletes the session and clears both cookies
  - Zitadel down → `503`; missing pending sign-in → `410`
- **Gateway**: `ui-auth` routes are public; credentialed CORS for Studio origins only; no
  `Access-Control-Allow-Credentials` for other origins or for a `*` origin setting.
- **Studio** (vitest): each page's form, pending and error states; `authRequest` carried
  through; the redirect followed only when `ui-auth` returns one; `/login` forwarding to
  `/register` for `prompt=create`; logout's origin check.
- **Bootstrap** (rebuilt): pure request builders and response parsers unit-tested; each step
  idempotent against a mock Zitadel (a second run changes nothing); the login-client token
  check and the cookie key's create-once behaviour tested; live run on the fresh instance.
- **Live: `just auth-e2e`** (needs `just dev`), a script with no browser:
  1. start an authorize request with PKCE; Zitadel redirects to `/login?authRequest=…`
  2. sign up through `ui-auth`, read the code from Mailpit's API, verify, follow the
     redirect, exchange the code at the token endpoint, call the gateway's `/v1/me`
  3. sign in again; a wrong password and an unknown email give identical `401`s
  4. reset the password with the code from Mailpit; sign in with the new one
  5. log out; the session is gone and a new sign-in is required

## Out of scope (later phases)

Passkeys and security keys, MFA, external identity providers, the account picker (reusing a
session), device-code login, branding and translations, and component examples for customers'
end users.

## Risks

- **Any v1 call left in the bootstrap is deprecated.** The plan keeps them to the steps v4 has
  no v2 replacement for; a future Zitadel major may remove them.
- **We own security-critical code.** The safeguards above are specified and tested, but a
  review against Zitadel's own login (`apps/login` in its repo) before merging is part of the
  plan.
- **Single `ui-auth` process.** Rate limits live in memory; running more than one needs a
  shared store (the roadmap's "more than one worker" item applies here too).
