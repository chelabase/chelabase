# Chore: generated API client and TanStack Query in Studio — design

Status: **written 2026-10-01; awaiting the user's review.** Brainstormed with the user the same day, before step 6a (login roles), which waits for it. Branch `api-client` (`.worktrees/api-client`).

## Goal

Studio's calls to our services become fully typed from the Rust code, so a changed route or field is a TypeScript compile error instead of a runtime bug. The Rust handlers describe themselves as OpenAPI documents; a typed client with TanStack Query hooks is generated from them; every Studio screen moves to TanStack Query; and every error our platform, login and gateway layers write takes one shape with a stable code. The platform document and client are shaped so they can be published to third parties later.

## Decisions (from the brainstorm)

| Topic | Decision |
|---|---|
| Purpose | A fully typed Studio client (contributors first), with a public-ready OpenAPI document of the platform API for third parties later. |
| Audience now | Studio only. The client is a private workspace package, publishable later (npm name, versioning, docs wait for platform mode). |
| Scope | The platform API (orchestrator `/v1`, including the data API schema route) and the login API (auth `/auth/v1`, Studio-internal document). The gateway's own errors on those routes follow the error shape. Out of scope: the data API (`POST /data/v1/query`, its SDK and `cheladb gen types`), internal listeners. |
| Errors | One shape everywhere: `{"error": {"code", "message", …}}` with a documented code per refusal. |
| Approach | utoipa 6.0.0 + utoipa-axum 0.3.0 → committed OpenAPI JSON → orval 8.x (`react-query` client, fetch) with one hand-written fetch mutator per client. Rejected: openapi-typescript + openapi-fetch + openapi-react-query (path-string call sites, three 0.x packages), Hey API (pre-1.0). |
| TanStack Query | All of Studio moves to it in this chore (`@tanstack/react-query` 5.104.0). |
| Another tenant's database | 404 `not_found`, indistinguishable from a missing one (user, 2026-10-01). |
| Org without a tenant | 403 `tenant_not_bound` (user, 2026-10-01). |

## Part 1: the OpenAPI documents

- **Files (committed):** `openapi/platform.json` (orchestrator `/v1`; public later) and `openapi/login.json` (auth `/auth/v1/*`; Studio-internal).
- **Rust:** request and response structs derive `utoipa::ToSchema`; each handler has `#[utoipa::path(…)]` with method, path, parameters, body, every response status (errors with the `ApiError` schema, the description naming the codes) and an explicit `operation_id`. The orchestrator and auth each expose `pub fn openapi() -> utoipa::openapi::OpenApi`.
- **Freshness:** a test per service compares `openapi()` with the committed JSON; `CHELADB_BLESS=1` rewrites it (as `cheladb gen types`' golden files do).
- **Registry match:** a test per service asserts the document's set of `(method, path)` equals the `cheladb-routes` endpoints of that service, minus `orchestrator.health`. (`utoipa::path` needs literal paths, so the two can't share constants; this test keeps them equal.)
- **Operation ids** (camelCase, verb + noun, unique; checked by a test) become the hook names (`useGetDatabase`, `useCreateBranch`) and, later, the public SDK's names:

  | Route | operation_id |
  |---|---|
  | `GET /v1/me` | `getMe` |
  | `POST /v1/databases` | `createDatabase` |
  | `GET /v1/databases/{id}` | `getDatabase` |
  | `PATCH /v1/databases/{id}` | `updateDatabase` |
  | `DELETE /v1/databases/{id}` | `deleteDatabase` |
  | `GET /v1/databases/{id}/connection` | `getDatabaseConnection` |
  | `POST /v1/databases/{id}/reset-password` | `resetDatabasePassword` |
  | `GET /v1/databases/{id}/branches` | `listBranches` |
  | `POST /v1/databases/{id}/branches` | `createBranch` |
  | `DELETE /v1/databases/{id}/branches/{branch_id}` | `deleteBranch` |
  | `GET …/branches/{branch_id}/connection` | `getBranchConnection` |
  | `POST …/branches/{branch_id}/reset-password` | `resetBranchPassword` |
  | `POST …/branches/{branch_id}/reset-to-parent` | `resetBranchToParent` |
  | `POST …/branches/{branch_id}/restore` | `restoreBranch` |
  | `GET …/branches/{branch_id}/keys` | `listKeys` |
  | `POST …/branches/{branch_id}/keys` | `createKey` |
  | `POST …/keys/{key_id}/rotate` | `rotateKey` |
  | `DELETE …/keys/{key_id}` | `revokeKey` |
  | `GET …/branches/{branch_id}/data-api/schema` | `getDataApiSchema` |
  | `POST /auth/v1/auth-request` | `getAuthRequest` |
  | `POST /auth/v1/sign-in` | `signIn` |
  | `POST /auth/v1/sign-up` | `signUp` |
  | `POST /auth/v1/verify-email` | `verifyEmail` |
  | `POST /auth/v1/resend-code` | `resendCode` |
  | `POST /auth/v1/password-reset` | `requestPasswordReset` |
  | `POST /auth/v1/password-reset/confirm` | `confirmPasswordReset` |
  | `POST /auth/v1/logout` | `logout` |

- **Shared types:** the data API schema's wire types (`Table`, `Column`, `Function` and what they contain, today in `services/rest/src/schema.rs`) move to `crates/cheladb-types`, so the orchestrator's document can describe the body it proxies; rest uses them from there.
- Ids (`DatabaseId`, `BranchId`, key ids) are `string` with format `uuid`; timestamps are `string` with format `date-time`. Studio's branded id types are dropped.
- Both documents declare their security: platform = bearer token (Zitadel JWT, through the gateway); login = cookies.

## Part 2: one error shape

```json
{ "error": { "code": "branch_not_ready", "message": "branch is provisioning; try again once it's ready" } }
```

- `code` is stable snake_case; clients switch on it. `message` is a sentence safe to show to a user, never matched by code. Extra fields only where a code documents them (6a's `role_in_use` adds `keys`). 503s keep `Retry-After` where they have it today.
- The data API already nests its errors this way; its own extras (`detail`, `hint`, `sqlstate`) and codes stay data-API-only and unchanged.
- **Rust:** `cheladb_types::api_error::{ApiError, ErrorCode}`. `ApiError { status: StatusCode, code: ErrorCode, message: String, extra: serde_json::Map }` implements `IntoResponse` (behind an `axum` feature) and `ToSchema`; `ErrorCode` is one `ToSchema` enum serialising to the codes below. The orchestrator's `json_error`, `unauthorized`, `internal_error` and auth's `error`/`unavailable` are replaced by it. The gateway writes the same body through its own small helper.
- **Extractor rejections become JSON:** both services' JSON and path extractors answer `invalid_request` (400, 415, 422) or `too_large` (413) in this shape instead of axum's plain text (custom rejection types or `WithRejection`). The orchestrator's `Bytes`-body routes already parse by hand; they keep doing so with the new error.
- **The gateway** answers its own errors on platform, login **and data** routes in this shape: unknown host or no route → 404 `not_found`; 405 `method_not_allowed` (with `Allow`); 401 `unauthorized`; JWKS trouble → 503 `unavailable`; minting failure → 500 `internal`; proxy failures → 502/503 `unavailable`, 504/408 `timeout`, 400 `invalid_request`, 500 `internal` (a 413 → `too_large`). The data routes' existing nested errors keep their codes (`invalid_query` for a data 404/405 is the data API's contract).

**Codes** (from the audit of every error site; ★ = new to the brainstorm's list):

| code | status | meaning |
|---|---|---|
| `unauthorized` | 401 | no credential, or a bad token or internal identity |
| `forbidden` | 403 | authenticated but not allowed (tenant id mismatch on create) |
| `tenant_not_bound` ★ | 403 | the caller's Zitadel org isn't bound to a Cheladb tenant |
| `not_found` | 404 | unknown host or route; a missing database (or another tenant's), branch, parent branch, key, tenant or project |
| `method_not_allowed` | 405 | the route exists, not for this method |
| `invalid_request` | 400, 415, 422 | a malformed or invalid body, path, name, point in time or setting |
| `too_large` ★ | 413 | the body is over the limit (8 KiB login, 2 MB platform) |
| `rate_limited` | 429 | too many attempts (login) or unknown-key lookups (gateway) |
| `unavailable` | 502, 503 | a dependency is down (an upstream service, JWKS, Zitadel, the data API) |
| `timeout` | 504, 408 | an upstream, the worker or the database didn't answer in time (408: the client was too slow) |
| `internal` | 500 | unexpected; details only in logs |
| `database_not_ready` | 409 | the database isn't ready (pending, failed, deleting) for this |
| `branch_not_ready` | 409 | the branch or its parent isn't ready, or is busy |
| `credentials_unavailable` | 409, 500 | owner credentials are missing (409) or can't be opened with the current secrets key (500) |
| `backup_branch` | 409 | a restore backup has no compute; branch from it instead |
| `has_children` | 409 | can't reset, restore or delete while it (or its backup) has child branches |
| `branch_cap_reached` | 409 | the database has `max_branches` |
| `main_branch` ★ | 409 | not possible on main (no parent to reset to; delete the database instead) |
| `name_taken` ★ | 409 | a database or branch with this name exists |
| `already_deleting` ★ | 409 | the database or branch is already being deleted |
| `public_key_exists` | 409 | the branch already has a public key |
| `public_key_not_revocable` ★ | 409 | a public key can only be rotated |
| `role_refused` | 422 | the worker refused the secret key's role; the message says why |
| `not_applied` | 503 | the new password is stored but not applied yet; it will be retried |
| `worker_unavailable` | 503 | the worker or job queue can't be reached |
| `invalid_credentials` | 401 | email or password is incorrect (identical body for both) |
| `email_taken` | 409 | an account with this email exists (self-hosted sign-up) |
| `invalid_code` | 400 | the verification or reset code is invalid or expired |
| `weak_password` ★ | 400 | the password fails the policy |
| `sign_in_expired` ★ | 410 | the pending sign-in is gone; start again |

Not codes yet: `out_of_history_window` (it only appears in `branch.error` today) and `verification_required` (sign-in answers 200 `{"next":"verify"}`).

**Fixes that come with the move (rulings):**
- Another tenant's database → 404 `not_found` (user's decision).
- `POST …/branches`' parent lookup no longer turns a metadata error into 404; it's 500 `internal`.
- A failed NATS publish is 503 `worker_unavailable` (502 today), like a failed request.
- The schema route's 409 passthrough (rest's `{"status": …}` body, no error key) becomes 409 `branch_not_ready`; rest's 404 `unknown_branch` becomes `not_found` at the orchestrator.
- A backup branch on `POST …/keys` gets `backup_branch` (it gets `branch_not_ready` today).
- Unchanged on purpose: auth maps any Zitadel 400 on sign-up to `weak_password` and any Zitadel 4xx on a pending sign-in to `sign_in_expired`; the messages stay as today.

## Part 3: the generated client

- **`packages/platform-client`** (`@cheladb/platform-client`, `private: true`): orval output from `openapi/platform.json` in `src/generated/` (committed, never edited), plus the hand-written `src/fetch.ts` (the mutator) and `src/index.ts`.
- **The login client:** orval output from `openapi/login.json` in `apps/studio/src/lib/login-client/generated/`, plus its mutator.
- **`packages/types` is deleted** (Studio is its only user).
- **orval settings:** `client: "react-query"`, `httpClient: "fetch"`, `mode: "split"` (hooks and functions in one file, schemas in another), no zod, no MSW. Per operation it emits a plain async function (`getDatabase(id)`), a key helper (`getGetDatabaseQueryKey(id)`), a hook (`useGetDatabase`, `useCreateBranch`) and the types.
- **Mutators:**
  - Platform: `configurePlatformClient({ baseUrl, getToken, onUnauthorized, fetchImpl? })`, called once. Adds `Authorization: Bearer`, JSON headers; a 401 calls `onUnauthorized()` (true = sign-in restarting); a non-2xx throws `ApiError`.
  - Login: `configureLoginClient({ baseUrl, fetchImpl? })`, `credentials: "include"`, no token; non-2xx throws `ApiError`.
  - `ApiError extends Error { status: number; code: ErrorCode | "network"; message; extra; signInRestarted: boolean }`, read from Part 2's body; a non-JSON body or a network failure gives `code: "unavailable"` or `"network"` with a fallback message. `ErrorCode` is the generated union.
- **Generation:** `just api-gen` = `CHELADB_BLESS=1 cargo test -p orchestrator -p auth openapi_document` then orval for both. `just check` runs orval and fails on a `git diff` in the generated folders (and the document tests fail on stale JSON).
- **Versions:** orval = the newest 8.x pnpm's minimum release age allows (8.39.0 is one day old at writing); `@tanstack/react-query` 5.104.0 (peer of the client, dependency of Studio). Checked again at plan time.

## Part 4: Studio on TanStack Query

- **Setup:** one `QueryClient` + provider in `routes/app.tsx` (signed-in area), where `configurePlatformClient` is called with the OIDC token getter and the re-auth guard. Sign-out runs `queryClient.clear()`. Defaults: `staleTime` 10 s; no retry on 4xx; up to 2 retries on network errors and 5xx; refetch on focus. The Query devtools plug into the existing `@tanstack/react-devtools`, dev builds only.
- **Components** call the generated hooks; the `api: ApiClient` prop, `createApiClient`, `ApiClient` and `fakeApi` go.
- **Polling:** a query's `refetchInterval` is 2 s while anything it shows is `pending`, `provisioning`, `resetting` or `deleting`, else `false`. It replaces the dashboard's, the branches', the settings' and the create form's timer loops; the create form becomes a mutation, then the dashboard polls.
- **Invalidation** (each mutation, on success, through the key helpers):

  | Mutation | Invalidates |
  |---|---|
  | `createDatabase`, `deleteDatabase` | `getMe` |
  | `updateDatabase` | `getDatabase(id)` |
  | `createBranch`, `deleteBranch`, `resetBranchToParent`, `restoreBranch` | `listBranches(id)`, `getDatabase(id)` |
  | `resetDatabasePassword`, `resetBranchPassword` | nothing cached (secrets aren't cached) |
  | `createKey`, `rotateKey`, `revokeKey` | `listKeys(id, branch)` |

- **Secrets never enter the cache:** connection details and created or rotated keys come from the plain generated functions or mutations, never `useQuery`; they live in component state as today (not refetched on focus, not in devtools).
- **Errors:** `errorText(err)` gives the message (or a fallback for `network`). Behaviour that depends on an error switches on `code`: `not_applied` (the reset notice; replaces today's exact-text match), `sign_in_expired` (the restart link; replaces `EXPIRED` text matching), `email_taken` (go to sign-in), and `signInRestarted` on 401.
- **Login pages** use the login client's mutation hooks under their own `QueryClientProvider` in the public routes; flows and wording unchanged.

## Part 5: tests, checks, and 6a

**Rust:** per document: matches the committed JSON; registry match; unique camelCase operation ids; every route documents its error statuses. `ApiError`: body shape, `Retry-After`, every `ErrorCode`'s serialised name; extractor rejections answer JSON. The orchestrator's (`tests/api.rs`), auth's (`tests/flows.rs`) and the gateway's tests move to `body["error"]["code"]`; message checks stay only where wording matters (auth's identical 401 bodies stay byte-identical). New tests for each ruling above (another tenant's database 404, `tenant_not_bound`, the parent-lookup 500, 503 on publish failure, the schema 409, a backup branch on key create).

**Studio:** a `fakeFetch` helper (method + path → response, records calls) passed as `fetchImpl`; every test renders in a fresh `QueryClient` (`retry: false`). Existing component tests keep their user-facing assertions. New: polling starts and stops with the status; each mutation invalidates per the table; connection details and new keys never appear in the query cache; a 401 runs re-auth; `code` reaches `ApiError`; the three code-switched behaviours.

**Scripts:** `scripts/auth-e2e.sh` (identical 401 bodies), `branch-e2e.sh` and `data-e2e.sh` statuses keep passing; any body reads move to `.error.code`/`.error.message`.

**Checks:** `just check` (clippy, tests, both document tests, orval with no diff, Studio typecheck/tests/build); `just neon-e2e`, `branch-e2e`, `data-e2e`, `auth-e2e`; a Playwright pass over every screen (sign-in, sign-up, verify, dashboard, create database polling to ready, branches: create, reset, restore, delete; connection reveal; settings; Data API keys and schema).

**Step 6a, when it resumes (rebased onto this):** its Task 5 annotates the four role routes, adds `invalid_role_name`, `reserved_role_name`, `role_exists`, `role_cap_reached`, `role_in_use` (extra `keys`), `role_privilege_mismatch` to `ErrorCode`, and runs `just api-gen`; ruling R6 is dropped; its Studio task uses the generated hooks (`useListRoles`; create and reset as mutations so passwords stay out of the cache). The 6a plan is updated and re-approved.

**Models (proposed; final in the plan):** error type and Rust error migration on sonnet; OpenAPI annotations on sonnet; client packages and mutators on sonnet; Studio's Query migration on opus (many screens, polling and invalidation design); e2e and Playwright on sonnet; docs on haiku. Reviews: sonnet for the error type and the client; opus for the Studio migration; the end-of-step whole-branch review on opus.
