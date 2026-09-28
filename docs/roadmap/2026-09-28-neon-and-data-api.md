# Roadmap: Neon integration and the data API

**Date:** 2026-09-28
**Status:** Direction agreed in conversation. Each step still gets its own spec, plan and approval before any build.

## Where we are

On `main`:
- **HTTP gateway** (Pingora, `apps/gateway`).
- **Studio sign-in** through our own pages and `ui-auth` on Zitadel v4.19.1, with a landing page and create-database.
- **Database lifecycle** on plain `postgres:17` containers:
  - two roles
  - encrypted, re-revealable credentials
  - reset password
  - delete by tombstone
  - reconcile
- **Neon storage stack** (step 1, done): opt-in with `just neon-up`. Nothing uses it yet. See "Progress" below.
- **`crates/cheladb-neon`** (step 2, done): the storage-controller client, compute spec, SCRAM, LSNs, compute JWTs and Neon ids. Nothing calls it yet.

Specs are in `docs/superpowers/specs/` and plans in `docs/superpowers/plans/`.

## Resume here (handoff, 2026-09-28)

**State of the repo:**
- `main` has the dependency refresh, Zitadel v4 with our own login (`ui-auth`), `CHELADB_MODE`, `just cleanup`, and every minor finding from the two reviews fixed. There are no open review findings.
- The only other branch is `neon-control-plane-listener`: the approved step 3 spec (2 docs commits, `240288a` and `5ae0f52`), based on the old `main` at `0eef624`. Its plan isn't written yet.
- **Last green run:**
  - `cargo test --workspace`: 312/312, three runs in a row;
  - clippy: clean;
  - `just neon-test`: all passed;
  - Studio: 72/72, plus typecheck and build;
  - `just auth-e2e`: ok, live;
  - a real browser pass (Playwright) of sign-up, the taken-email redirect, sign-in, reset from the Mailpit link, sign-out, and verify-mode sign-up.
- The local state is clean after `just cleanup`. There is one account, `clean-start@example.com` / `Clean-pass1!`, and no tenant databases.

**Next session, in order:**
1. `git checkout neon-control-plane-listener && git rebase main`. It's docs only, so no conflicts are expected.
2. Re-check the spec against what landed since it was written, and amend it:
   - `ui-auth` is on 8084 and the new listener takes 8083;
   - every published port stays on 127.0.0.1 (`just neon-test` enforces this);
   - `CHELADB_MODE` exists, so decide whether the proxy or password mode should follow it.
3. **Decision already made: no long proxy cache.** 10 s is too long, because a user must be able to sign in right after a password reset. Before planning, find in Neon's proxy source at the pinned ref (`infra/neon/NEON_REF`) whether `--project-info-cache` can be turned off, e.g. `size=0`, or given a TTL of about 1 s. Then write the result into the spec. Our control plane answers locally, so skipping the cache is cheap. Also note for step 4: resetting a password must not depend on invalidating the cache, which is a no-op at this ref.
4. Write the step 3 plan (writing-plans), then execute it.

**Findings to carry forward (environment and tooling):**
- **Ephemeral ports run out on this machine.** WSL's `net.ipv4.ip_local_port_range` is `44620 48715`, about 4,000 ports. Many test and e2e runs in a row leave around 10,000 sockets in TIME-WAIT, and then `bind("127.0.0.1:0")` fails in the gateway and `ui-auth` tests. It looks like flakiness but isn't. To fix it:
  - wait a minute or two; or
  - widen the range: `sudo sysctl -w net.ipv4.ip_local_port_range="32768 60999"`. This is the user's machine, so it hasn't been changed.
- **Rate limits in `just auth-e2e`:** sign-in allows 10 per minute per IP, and every run comes from 127.0.0.1, so wait a minute between runs.
- **Playwright MCP:** registered at user scope with proto's pnpm, by absolute path, because `npx` isn't on PATH:

  ```
  claude mcp add playwright --scope user -- ~/.proto/shims/pnpm dlx @playwright/mcp@latest --headless --browser chromium
  ```

  It needs the browser build that matches `@playwright/mcp`, installed with `pnpm dlx @playwright/mcp@latest install-browser chrome-for-testing`. Its output goes to `.playwright-mcp/`, which is git-ignored.
- **Starting apps outside `just dev`:** anything started by hand isn't in `.cheladb/dev/pids`, so `just stop` doesn't kill it, and the next `just dev` fails with "Address already in use". `just cleanup` also stops anything on the dev ports.
- **Studio and the gateway must be the same site** (`localhost`, not `127.0.0.1`). Otherwise browsers drop `ui-auth`'s `SameSite=Strict` cookies. Bootstrap writes `VITE_API_URL=http://localhost:8000`, and a test pins this.
- **TanStack Router JSON-parses search params by default.** Studio replaces the parser (`src/lib/search.ts`) so that all-digit Zitadel ids and codes stay strings.
- **Zitadel v4 behaviours:**
  - A new user is invisible to `POST /v2/sessions` for a moment (404), and `ui-auth` retries for up to 1 s.
  - Usernames are unique, emails aren't.
  - Error messages are translated text, not ids.
  - `end_session` with `id_token_hint` ends the session itself and skips our `/logout` page.
  - Password hashing makes an existing account about 0.25 s slower, so sign-in is padded to 1.5 s and sign-up to 2.5 s.

## Decisions made

### Which Neon parts to keep and which to re-engineer

| Neon component | Decision | Why |
|---|---|---|
| pageserver, safekeepers, storage broker, storage controller | **Adopt unchanged** | This is the storage engine behind branching and point-in-time restore. Rewriting it would be the whole project. |
| compute image (Postgres 17 + neon extension) and `compute_ctl` | **Adopt unchanged** | Plain Postgres can't read from the pageserver. `compute_ctl` starts Postgres from a JSON spec served by the control plane. |
| **proxy** (Postgres wire) | **Adopt instead of writing our own pgproxy** | It already does SCRAM, routing by SNI (`<endpoint>.<domain>`), caching of wake results, retries while a compute boots, and rate limits. Its dependency on the control plane is only three calls. |
| Control plane | **Build it ourselves** | Neon's is closed source. |
| NeonVM, autoscaler-agent, vm-monitor | **Drop** | They're tied to Kubernetes and KVM. We replace them with our own **Docker-native compute runtime**, where Cheladb can actually be better than Neon. |
| Neon's Data API | **Not available; build our own** | See the data API section below. |

### What Neon's proxy needs from us

`--auth-backend=control-plane` calls our control plane with a Bearer token. It needs these three calls (from `proxy/src/control_plane/client/cplane_proxy_v1.rs`):

| Call | What we return |
|---|---|
| `GET get_endpoint_access_control?endpointish=&role=` | `role_secret` (a SCRAM-SHA-256 verifier), `allowed_ips`, `project_id`, … |
| `GET wake_compute?endpointish=` | `address` (host:port of the compute), `server_name` |
| `GET /endpoints/{id}/jwks` | JWKS; only needed for the proxy's JWT path |

- The proxy's SQL-over-HTTP, WebSocket and REST features stay **off**. HTTP traffic goes through our gateway only.
- That gives two front doors, one per protocol:
  - **Postgres wire** goes through Neon's proxy.
  - **HTTP** goes through the Pingora gateway.

### Upstream Neon is effectively frozen

- The last release was July 2025, and `main` has had almost no code commits since March 2026. NeonVM's last code commit was September 2025.
- Pinning a commit therefore costs nothing.
- If we ever need to change Neon (for example, adding an extension to the compute image), the change goes in as a `patches/` file applied on top of the pinned commit. We don't fork.

### How to get the Neon images

We build them from source at a pinned commit, with pulling as a shortcut:
- Port a trimmed `infra/neon/` from the old repo: `NEON_REF` plus a build script for the `neon` and `compute-node` images only, with no NeonVM.
- `just neon-build` shallow-clones upstream at `NEON_REF` and builds. The first build needs about 40 GB of free disk and about 40 minutes.
- `just neon-up` (until step 4; then `just dev`) uses local images when they exist, and otherwise pulls the matching `ghcr.io/cheladb/{neon,compute-node-v17}` images pinned by digest.
- We don't vendor or fork the Neon source.

### Reuse the old repo (`<repo root>`)

It's your own public Apache-2.0 code, and it already implements a working Neon control plane on Docker.

**Port**, adapting as needed:

| Old repo path | Use in the scaffold |
|---|---|
| `core/docker/docker-compose.yml`: seaweedfs + create-bucket, storage-broker, safekeeper1, storcon-db, storage-controller, pageserver, register-safekeepers, docker-socket-proxy, proxy-tls, proxy | Add to `infra/compose/docker-compose.yml` |
| `infra/neon/build.sh`, `NEON_REF`, `lib/tags.sh` | Build the Neon images, trimmed to `neon` + `compute-node` |
| `services/controlplane/src/storcon.rs` | Storage-controller client (tenants, timelines, pageserver, safekeepers) |
| `…/src/compute.rs`, `…/src/api/compute_spec.rs` | The compute spec `compute_ctl` pulls (`/compute/api/v2/computes/{id}/spec`) |
| `…/src/api/proxy_api.rs`, `…/src/scram.rs` | The proxy's control-plane calls; SCRAM verifiers |
| `…/src/api/hooks.rs` | Storage-controller callbacks (`notify-attach`, `notify-safekeepers`) that trigger a reconfigure |
| `…/src/backend.rs`, `…/src/docker.rs` | The `ComputeBackend` trait and its Docker implementation. **Only the Docker backend comes over; `k8s.rs`/NeonVM stays behind.** |
| `…/src/jwt.rs`, `…/src/lsn.rs` | Compute JWKS; parsing Postgres log sequence numbers |
| `…/migrations/0001–0002`, `api/{branches,roles,endpoints}.rs` | Reference for the data model and logic; adapt, don't copy |
| `core/tests/e2e.sh`, `smoke.sh` | End-to-end check with `psql` through the proxy |

**Don't port:**
- identity (better-auth); we use Zitadel
- the gateway; we have our own
- NeonVM/k8s
- the Copybara, lanes and mirror setup

**Friction to expect:**
- Dependency versions differ:

  | | Old repo | Scaffold |
  |---|---|---|
  | sqlx | 0.9 | 0.8 |
  | bollard | 0.21 | 0.18 |
  | jsonwebtoken | 11 | 9 |

- Neon's services call the control plane machine-to-machine: the proxy, `compute_ctl` and the storage controller. That traffic needs an **internal-only listener with its own bearer tokens**, not a gateway route.

### Mapping our model to Neon's

- **Our model:** tenant → project → database, where one database is one container.
- **Neon's model:** tenant → timeline (branch) → endpoint (compute) → roles and databases.
- **The mapping:** one Cheladb database becomes one Neon tenant, with a `main` branch, one endpoint and the `postgres` owner role. A branch is a later addition: another timeline under the same database.

### What changes in the scaffold when provisioning moves to Neon

| Area | Change |
|---|---|
| Worker | The plain `postgres:17` + named-volume provisioning goes away. Computes run through `ComputeBackend` (Docker). |
| `cheladb_platform` role | Removed. `compute_ctl` manages its own admin role. |
| Reveal | Still works. The sealed owner password stays, and we derive a SCRAM verifier from it for the proxy. |
| Reset password | New verifier in the spec, then reconfigure the compute, instead of `ALTER ROLE`. |
| Delete | Also deletes the Neon tenant. |
| Carries over | Tombstones, conditional writes, reconcile and the NATS jobs. |

## Next steps, in order

1. **Neon build and data plane in Compose.** ✅ Done 2026-09-28; see "Progress".
   - Port the trimmed `infra/neon` build and pin.
   - Add the storage services to Compose.
   - First check: is there about 40 GB free for a source build, and can we pull the `ghcr.io/cheladb/*` images as a fallback?
   - Done when the storage stack comes up healthy next to the existing services.
2. **`crates/cheladb-neon`.** ✅ Done 2026-09-28; see "Progress".
   - A straight port with its tests: storcon client, compute-spec builder, `scram`, `lsn`, compute JWKS.
3. **Internal control-plane listener and Neon's proxy.**
   - Endpoints: `proxy_api`, `compute_spec`, storage-controller hooks.
   - Replace the `storcon-hook` stand-in: point the storage controller's `--control-plane-url` at the listener (`host.docker.internal:8083`, with `extra_hosts: host-gateway`) and delete the stub service.
   - Add the proxy and its TLS setup (local CA, `*.localtest.me` certificate) to Compose.
   - Done when `psql` connects through `<id>.localtest.me:4432` to a hand-made compute.
4. **Switch provisioning to Neon.**
   - Make the storage stack part of `just deps-up`/`just dev`: remove the `profiles: ["neon"]` lines.
   - Apply the mapping above and port the `ComputeBackend` Docker implementation.
   - Adapt reveal, reset, delete and reconcile.
   - Studio shows the proxy connection string.
   - Also create the data API roles (`anon`, `authenticated`, `authenticator`) in the compute spec now, so step 6 doesn't need a migration.
5. **Branching.** Create a branch from `main`, optionally at a point in time. Each branch gets its own endpoint and appears in Studio.
6. **Data API (the PostgREST equivalent).** See below.
7. **Compute runtime work.** This is where we improve on NeonVM:
   - scale to zero: stop idle computes, and `wake_compute` starts them again (the proxy already retries using `retry_ms`)
   - resize running containers (the old repo has a fixed 1 CPU / 2 GiB)
   - later, a Kubernetes pod backend using the same trait

Steps 5, 6 and 7 are independent once step 4 lands, so they can be reordered. Branching comes first because it's why we adopted Neon.

**Can be done at any point:** NATS auth, TLS, restart policies, running more than one worker, CI.

**Dependency follow-up:** ✅ Done 2026-09-28 by the dependency refresh (see "Progress"). Every crate uses workspace pins at the latest releases.

**Before step 3: Zitadel v4 (done 2026-09-28, branch `zitadel-v4`).** Zitadel v2 stopped getting releases after v2.71.19 (2025-11-12); v4 is current. Moving touches Studio's hosted login, the generated client in `packages/zitadel` and `cheladb auth bootstrap`, so it gets its own spec and branch, done between the dependency refresh and step 3. Spec: `docs/superpowers/specs/2026-09-28-zitadel-v4-and-own-login-design.md`. It also replaces Zitadel's hosted login with our own: pages in Studio and a small Rust service, `ui-auth`.

**Security follow-ups found during the refresh:**
- ✅ Every published port is bound to `127.0.0.1` (metadata-db, zitadel-db, NATS, Zitadel and Mailpit were on all interfaces); `just neon-test` checks it.
- No TLS to the metadata database or NATS yet (the "TLS" item above). sqlx has `tls-rustls-aws-lc-rs`, so turning it on is a server-side and connection-string change.

## Progress

### Step 1: Neon data plane in Compose (done 2026-09-28)

Spec: `docs/superpowers/specs/2026-09-28-neon-data-plane-design.md`. Plan: `docs/superpowers/plans/2026-09-28-neon-data-plane.md`.

**What exists now:**
- `infra/neon/`: `NEON_REF` (`fa504217c61b`), a trimmed `build.sh` (`neon` and `compute-node` only), `lib/tags.sh`, READMEs, and offline tests (`just neon-test`).
- In `infra/compose/docker-compose.yml`, under the `neon` profile:
  - seaweedfs and `create-bucket`
  - the storage broker and `safekeeper1`
  - `storcon-db` and the storage controller
  - the pageserver
  - `register-safekeepers`
  - `storcon-hook`
- Images come from `ghcr.io/cheladb/neon:0.1.0` and `compute-node-v17:0.1.0`, pinned by digest as the `${NEON_IMAGE:-…}` / `${COMPUTE_NODE_IMAGE:-…}` defaults. The compute image is pulled but not run yet.
- Recipes: `neon-build`, `neon-up`, `neon-down`, `neon-ps`, `neon-smoke` and `neon-test`. `just down` also stops the storage stack.
- `just neon-smoke` checks that:
  - safekeeper 1 is Active and the pageserver answers;
  - a tenant can be created, and its compute notification isn't left pending;
  - a `main` timeline can be created and read back;
  - the timeline and tenant can be deleted.
- Host ports, loopback only: storage controller `127.0.0.1:1234`, pageserver HTTP `127.0.0.1:9898`. Step 2's storcon client and step 4's provisioning run on the host and use these.

**Findings to carry forward:**
- **The GHCR packages are private.** `docker login ghcr.io` is needed before `neon-up`, or build locally. The first pull took about 4 minutes.
- **The storage controller calls its hook on every tenant create, not only on rare moves.** With nothing answering, each create retried for about 15 s and left the tenant's reconcile in error (`is_pending_compute_notification: true`). The `storcon-hook` stand-in (`hashicorp/http-echo`, pinned) answers 200 until step 3. Step 3's real `notify-attach` must answer quickly and with 2xx, or tenant creates stall.
- **`docker compose up --wait` fails on a one-shot that has exited, even with status 0.** `neon-up` waits on the long-running services, then runs `docker compose run --rm register-safekeepers`. Re-registering is an upsert, so `neon-up` can be re-run safely.
- **Neon ids are 32 hex chars** (no dashes), unlike our UUIDs. Step 4's mapping has to convert.
- **Tenant describe** (`GET /control/v1/tenant/{id}`) reports `is_pending_compute_notification`, `is_reconciling` and `last_error` per shard. That's useful for reconcile in step 4.
- **Deletes are asynchronous:** `DELETE` answers 202 until the object is gone, then 200 or 404.
- The Compose project's default network is `cheladb_default`. The existing demo databases are on `bridge` and aren't affected. Step 4's computes join `cheladb_default`.
- The hook token is `CHELADB_STORCON_HOOK_TOKEN` (default `cheladb-dev-storcon-hook`), which Compose reads. Step 3's listener checks it.

### Step 2: `crates/cheladb-neon` (done 2026-09-28)

Spec: `docs/superpowers/specs/2026-09-28-cheladb-neon-crate-design.md`. Plan: `docs/superpowers/plans/2026-09-28-cheladb-neon-crate.md`.

**What exists now:**
- The workspace is on edition 2024 (resolver 3). No rustfmt run; existing files keep their formatting.
- `crates/cheladb-neon`, at the latest crate versions, declared in its own `Cargo.toml`:
  - `NeonId`: 32 lowercase hex; `NeonId::from(uuid)` maps one of our ids to its tenant id.
  - `Lsn`, with serde as the text form.
  - `scram::verifier`, with the RFC 7677 vector. It applies SASLprep first, as Postgres does, and hashes the raw password when SASLprep rejects it.
  - `spec::config_response` for compute_ctl.
  - `ComputeSigner`: Ed25519 tokens and the JWKS for `compute_ctl_config`.
  - `StorconClient`: tenants (create, describe, delete), timelines (create, branch, get, delete), locate, active safekeepers. 30 s timeout; deletes poll 120 × 500 ms, bounded overall by that budget plus one request timeout. Plain HTTP only: no TLS feature, so no aws-lc-rs.
- Tests: unit tests, `tests/storcon_mock.rs` (in-process mock), and `tests/storcon_live.rs`, run by `just neon-test-live` after `just neon-up`.

**Findings to carry forward:**
- The old `jwt.rs` and `scram.rs` compile unchanged against jsonwebtoken 11 and the 0.11/0.13 RustCrypto crates.
- The edition move needed one `cargo fix` edit (a macro fragment) and five let-chain rewrites for clippy's `collapsible_if`. Its 12 tail-expression drop-order warnings only move awaited temporaries ahead of locals, which is harmless here.
- The live storage controller matched the mock's response shapes: the round trip, including a branch at an LSN, passed first time. A fresh tenant's shard reports no pending compute notification and no `last_error` while `storcon-hook` answers.
- The storage controller lists tenants at `GET /control/v1/tenant`; `/v1/tenant` is 404.

### Dependency refresh (done 2026-09-28)

Branch `deps-refresh`, one commit per group; tests, clippy and the Studio checks passed after each.

- **Rust:** reqwest 0.13.5, jsonwebtoken 11.1.0, base64 0.23.1, ed25519-dalek 3.0.0, rand 0.10.3, aes-gcm 0.11.1, sqlx 0.9.0, async-nats 0.50.0, bollard 0.21.1, tower-http 0.7.1. Every workspace pin now names its exact current version, and `cheladb-neon` uses them.
- **TLS and JWT crypto is aws-lc-rs everywhere** (reqwest `rustls`, jsonwebtoken `aws_lc_rs`, async-nats `aws-lc-rs`): it has post-quantum key exchange and FIPS mode, which ring lacks. It needs a C toolchain.
- **pnpm:** vite 8.3.1, @vitejs/plugin-react 6.1.1, orval 8.38.0, lucide-react 1.48.0, TypeScript 7.0.2, @types/node 24 (we run Node 24). The TanStack packages that were on `"latest"` are pinned.
- **Docker:** every image pinned by index digest. Postgres 17.11 (not 18: existing volumes would need a dump and restore), NATS 2.15.0, Zitadel v2.71.19.

**Findings to carry forward:**
- **jsonwebtoken 11 panics at runtime if one binary enables both `aws_lc_rs` and `rust_crypto`.** Every crate must use the workspace pin.
- **pnpm 12 enforces a minimum release age.** Asking for a version published hours ago makes it write exemptions into `pnpm-workspace.yaml`; don't keep those. Let it resolve the newest version that passes.
- TypeScript 7 removed `baseUrl`; Studio's `paths` already start with `./`.
- reqwest 0.13 puts `.form()` and `.query()` behind features of those names.
- Remaining duplicate crates in `Cargo.lock` come from pingora 0.9 and rsa 0.9 (latest stable) and from reqwest's own tower-http 0.6. We can't remove them.
- The worker's provisioning path on bollard 0.21 is live-tested (2026-09-28). A database created through the gateway was ready with `postgres:17.11` on 127.0.0.1 only and answered queries. Password reset rotated the password and the old one was refused. Delete left no container or volume behind. The volume handling is also covered by `apps/worker/tests/docker_live.rs`.

### Zitadel v4 and our own login (done 2026-09-28)

Branch `zitadel-v4`. Plan: `docs/superpowers/plans/2026-09-28-zitadel-v4-and-own-login.md`.

- **Zitadel v4.19.1**, pinned by digest, on a fresh instance. It sends every sign-in to Studio's `/login?authRequest=…` and writes `login-client.pat` on first start. `packages/zitadel` is gone.
- **`cheladb auth bootstrap`, rebuilt:** idempotent steps over v2 REST (orgs, users, keys) and ConnectRPC JSON (projects, apps). The one exception is SMTP, which v4.19.1 still has only in the v1 admin API. A second run changes nothing. It also creates `.cheladb/ui-auth/cookie.key`.
- **`apps/ui-auth`** (`:8084`, routed by the gateway at `/auth/v1/*` with credentialed CORS for Studio only): sign-in, sign-up with email code, resend, password reset and logout, with state in AES-GCM cookies.
- **Studio pages:** `/login`, `/register`, `/verify`, `/password-reset`, `/logout`.
- **`just auth-e2e`** runs the whole flow live with curl and Mailpit, including a timing check.

**Findings to carry forward:**
- **Zitadel's password hashing is slow enough to reveal accounts by timing.** At a 500 ms floor, sign-in for an existing email took about 0.75 s and for an unknown email 0.50 s. The default floor is now 1500 ms (`CHELADB_UI_AUTH_MIN_RESPONSE_MS`), and `just auth-e2e` fails if the two differ by 150 ms or more. Revisit the floor if Zitadel's hasher settings change.
- **Right after `CreateUser`, `POST /v2/sessions` can answer 404 "User could not be found"** until Zitadel's projections catch up. ui-auth retries for up to 2 s.
- **Zitadel enforces unique usernames, not unique emails.** Sign-up looks the email up before `CreateUser` and gives an existing email the decoy answer.
- **Zitadel's messages are translated text, not error ids.** ui-auth never branches on them. It checks password complexity itself, and every other rejection at reset confirm means a bad code.
- **Ids go into Zitadel URL paths,** so ui-auth only passes on `V2_<digits>` (pending sign-ins) and digits (users). The security review found that `../` in an id reached other Zitadel endpoints.
- **Compared with Zitadel's own login (`apps/login` at v4.19.1),** ours doesn't read:
  - MFA enforcement (`checkMFAFactors`);
  - lockout settings (it shows remaining attempts; we deliberately don't);
  - `ignoreUnknownUsernames` (we always hide unknown users);
  - forced password change (`changeRequired`).
  
  Its email-verification gate is optional (`EMAIL_VERIFICATION`); ours always applies. MFA and forced password change belong to the later phases in the spec.
- **Modes:** `CHELADB_MODE` is `self-hosted` (the default, the dev mode) or `platform` (the hosted cloud, not built yet). In self-hosted mode:
  - email verification is off;
  - a sign-up for a taken email says so and goes to sign in;
  - cookies aren't `Secure`.
  
  Platform mode:
  - requires verification;
  - answers a taken email with a decoy (and emails the owner a reset link);
  - sets `Secure` cookies.
  
  `CHELADB_REQUIRE_EMAIL_VERIFICATION` and `CHELADB_UI_AUTH_SECURE_COOKIES` override the mode.
- **`just cleanup`** wipes local state (tenant databases, every Zitadel account, Mailpit) and re-bootstraps.
- **The reviews' minor findings are fixed (2026-09-28):**
  - Only failed sign-ins count against an email.
  - A non-loopback `ui-auth` needs `CHELADB_UI_AUTH_TRUSTED_PROXY=true`.
  - Studio sends no Referer.
  - `ui-auth` warns about token and cookie-key files other users can read.
  - `auth-request` is rate-limited.
  - An expired `/verify` offers Start again.
  - Sign-up is padded 1 s beyond sign-in, since it waits for Zitadel's projections.
  - Bootstrap waits for `login-client.pat`, and gives a clear error for an app without a client id.
  - Sign-out also clears the `cheladb_session` cookie.
  - Empty URL parameters are dropped.
  - Zitadel escapes `_` and `%` in email queries (checked live), so ui-auth needs no escaping.
- **Studio's sign-out ends the Zitadel session itself:** `end_session` with `id_token_hint` returns straight to Studio. Zitadel only goes through our `/logout` page when it has no hint.

## The data API (PostgREST equivalent)

### Where it stands

Nothing exists anywhere yet:
- **Neon's Data API is closed.** It's a Rust rewrite of PostgREST inside their proxy (`proxy/src/serverless/rest.rs`, feature `rest_broker`). Its query engine, `subzero-core`, is a **stub** in the public repo; the real crate isn't public or on crates.io.
- **The old repo deferred it.**
- **The scaffold has nothing either.**

### The pattern to copy from Neon

```
client ──HTTPS + JWT──► data API
   1. validate the JWT against the project's JWKS
   2. load the schema and config from a per-database cache; on a miss, introspect the database
   3. parse the PostgREST URL grammar and generate SQL
   4. wake the compute, then connect as `authenticator`
   5. run in one transaction:
        set_config('role', <jwt role | anon role>, true)
        set_config('request.jwt.claims', <claims>, true)
        <generated SQL>
   RLS does the authorization.
```

Settings, read from the database and cached: `db_schemas`, `db_anon_role`, `db_max_rows`, `role_claim_key`. With no anonymous role, unauthenticated requests are refused.

### Proposed design (still to be approved)

- A multi-tenant Rust service, `cheladb-rest`, that serves every database from one process. It is not one PostgREST per database.
- It sits **behind the Pingora gateway**, because it's HTTP. That's where Neon differs: they put it in the Postgres proxy.
- It asks our control plane for the compute's address and keeps its own connection pool.
- We write the parser and SQL generator ourselves, test-first against PostgREST's documented behaviour.

### Open decisions (to settle before its spec)

1. **End-user identity.** Early on the plan was for Zitadel to handle end users as well as platform users. The data API needs a JWT issuer or JWKS for each project's app users, separate from the Studio/platform tokens. Do we allow bring-your-own JWKS (Auth0, Clerk, …) from the start?
2. **How claims reach SQL.**
   - `set_config('request.jwt.claims', …)`: PostgREST-style, works on any Postgres, and the default.
   - The `pg_session_jwt` extension (neondatabase/pg_session_jwt, Apache-2.0). We haven't checked whether our `EXTENSIONS=minimal` compute image includes it.
3. **First subset of the grammar:**
   - reads with filters, `select`, order and limit
   - single-table insert, update and delete
   - `rpc`

   Resource embedding (joins) and upsert come in a second pass.

## Working rules (still apply)

- Feature branches only. Merge only when the user says. There's no remote; don't push.
- Commit as `git commit`, with `Co-Authored-By` and `Claude-Session` trailers.
- Never run `just sweep-dbs` or touch the user's databases (demo1/demo2/demo3).
- Never run `cargo fmt` across the workspace.
- Never overwrite `.cheladb/secrets/secrets.key` or the gateway key pair.
- The skip-email-verification switch and Mailpit's insecure SMTP auth are for local dev only.
