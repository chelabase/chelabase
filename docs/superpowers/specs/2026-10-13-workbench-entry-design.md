# W1: the entry layout (design)

Branch `workbench-entry`. This is the first of the two stack-rework steps; W2, isolation, follows. The decisions are in the roadmap's "stack and worktree rework" note: Q1–Q7, the housekeeping answers and the approach. The user approved the whole design on 2026-10-13 without going part by part. Choices made while writing this spec that were not asked are listed as **S-rulings** at the end, for review. The facts behind the design are in `docs/superpowers/notes/2026-10-13-stack-survey.md`.

## Goal

All browser and API HTTP reaches Chelabase through our Pingora gateway, on one port, routed by host name. This is the self-hosted production layout, and the workbench runs it the same way:

| Host (self-hosted example) | Workbench (main) | Serves |
|---|---|---|
| `studio.acme.com` | `studio.chelabase.localhost:8000` | Studio, including its sign-in pages |
| `api.acme.com` | `api.chelabase.localhost:8000` | `/v1` management, `/auth/v1` app-user auth, `/data/v1` data API |
| `auth.acme.com` | `auth.chelabase.localhost:8000` | all of Zitadel: OIDC, console, APIs |
| (workbench only) | `docs.chelabase.localhost:8000`, `landing.chelabase.localhost:8000`, `mail.chelabase.localhost:8000` | docs site, landing page, Mailpit |

Postgres wire traffic stays on Neon's proxy and its own port. Internal callbacks (storcon, proxy, compute_ctl and the Zitadel Actions calling the internal listener) are unchanged. The app tier stays on the host in W1; W2 containerizes it. There is no fork change, so no image build.

Non-goals: per-branch stacks, the port registry, containerizing anything, the `just workbench` rename and retiring the `just live` queue (all W2), and serving Studio's built files in a self-hosted package (packaging).

## 1. Hosts and configuration

- **Host settings:**
  - Three required settings (S5): `CHELABASE_STUDIO_HOST`, `CHELABASE_API_HOST` and `CHELABASE_AUTH_HOST`. Each is a host with an optional `:port`, checked the way `CHELABASE_API_HOSTS` entries are today.
  - Three optional settings that only the workbench sets: `CHELABASE_DOCS_HOST`, `CHELABASE_LANDING_HOST` and `CHELABASE_MAIL_HOST`.
  - `CHELABASE_PUBLIC_SCHEME` is `http` or `https` and defaults to `https`; the workbench sets `http`.
- **Workbench defaults:** they are the ones in the table above. They live in `infra/compose/.env.example`, which documents every host setting, and in the `just dev` exports.
- **Derived URLs (one function per crate, never configured twice):**
  - Studio's origin `<scheme>://<studio host>` is used for gateway CORS, the auth service's email links and the orchestrator's invite links. It replaces `CHELABASE_STUDIO_ORIGINS` and `CHELABASE_STUDIO_URL` (S6).
  - The issuer `<scheme>://<auth host>` is what Studio, the orchestrator, the worker and the CLI use. `CHELABASE_ZITADEL_ISSUER` becomes derived; if it is set and differs, startup refuses.
  - The API base `<scheme>://<api host>` is Studio's `VITE_API_URL`.
- **One site (user, 2026-10-13):** Studio's sign-in pages call `/auth/v1` on the API host with `SameSite=Strict` cookies, so the Studio and API hosts must be the same site, which means sharing a registrable domain. `studio.acme.com` and `api.acme.com` do. Bare `studio.localhost` and `api.localhost` don't, because `localhost` counts as a top-level domain. The workbench therefore uses the parent `chelabase.localhost`: main is `studio.chelabase.localhost:8000`, and in W2 branch `x` is `studio.x.chelabase.localhost:<port>`. Cookies stay host-only, so stacks still never share sessions. At startup the gateway refuses Studio and API hosts that don't share a parent domain (the hosts minus their first label differ), and it names both.
- **Listeners:** a new `CHELABASE_GATEWAY_LISTEN` takes comma-separated addresses. The workbench uses `127.0.0.1:8000,[::1]:8000`, because `*.localhost` resolves to `::1` on this machine (`getent hosts auth.chelabase.localhost` gives `::1`).
- **Removed:** `CHELABASE_API_HOSTS`, `CHELABASE_STUDIO_ORIGINS` and `CHELABASE_STUDIO_URL`. Setting any of them refuses startup with a message that names its replacement.

## 2. Gateway routing

- **Scopes:** `registry.rs`'s `Scope` gains `Studio`, `Auth`, `Docs`, `Landing` and `Mail` next to `Api`. `scope_for(host)` normalizes the host as it does today and returns the scope whose configured host matches. A host that matches nothing gets a 404 with an empty body. A request with no `Host` gets a 400.
- **Behaviour per scope:**

  | Scope | Behaviour |
  |---|---|
  | `Api` | Today's route table, CORS, data keys and user tokens, unchanged. CORS allows exactly Studio's origin. |
  | `Studio` | Everything goes to the Studio upstream (`CHELABASE_STUDIO_UPSTREAM`, default `http://127.0.0.1:3000`). The websocket upgrade passes through (Vite's hot reload). The gateway adds no CORS. |
  | `Auth` | Everything goes to the Zitadel upstream (`CHELABASE_ZITADEL_UPSTREAM`, default `http://127.0.0.1:8080`) with `Host` unchanged, because Zitadel picks its instance by host. HTTP/1.1 to the upstream (S4). The gateway adds no CORS; Zitadel answers its own. |
  | `Docs`, `Landing`, `Mail` | Each goes to its upstream (`CHELABASE_DOCS_UPSTREAM` :3001, `CHELABASE_LANDING_UPSTREAM` :3002, `CHELABASE_MAIL_UPSTREAM` :8025). Websockets pass through. A scope exists only when its host is set. |

- **Forwarded headers (S7):** for every scope the gateway sets `X-Forwarded-Proto`, `X-Forwarded-Host` and `X-Forwarded-For`, replacing any the client sent.
- **Limits:** today's request limits and timeouts stay on `Api`. The other scopes get the proxy's defaults, except that a websocket gets no idle timeout below 1 h.
- **Errors:** an upstream that is down returns 502 with a plain-text body on non-API scopes. `Api` keeps its JSON error shape.

## 3. Zitadel and the bootstrap

- **Compose:** Zitadel's `ZITADEL_EXTERNALDOMAIN` becomes the auth host's name (`auth.chelabase.localhost`), `EXTERNALPORT` becomes `8000`, and `EXTERNALSECURE` comes from the scheme. `LOGINV2_BASEURI` and the default login and logout URLs move to Studio's origin (`http://studio.chelabase.localhost:8000/…`). All of these values come from the compose env file, not literals.
- **Moving the existing instance (S1):** Zitadel runs `start-from-init`, so its setup phase runs on every start and picks up the new external domain. The live pass checks that the existing instance answers on `auth.chelabase.localhost:8000`. If it doesn't, `just zitadel-reset` followed by the bootstrap is the documented move; the bootstrap is idempotent and rebinds the demo tenant.
- **Bootstrap (`tools/cli/src/dev`):**
  - `GATEWAY_URL`, `STUDIO_ORIGIN` and `issuer()` come from the host settings, not constants.
  - The Studio app's redirect URIs become `<studio origin>/callback`, its post-logout URI `<studio origin>/`, and the login v2 base URI `<studio origin>/`.
  - `ensure_*` updates an existing app whose URIs differ, so a workbench that already exists moves over.
  - `apps/studio/.env.local` gets `VITE_API_URL`, `VITE_ZITADEL_ISSUER` and a new `VITE_STUDIO_HOST`. `demo.env` gets the issuer.
- **Internal callers:** the orchestrator, the worker and the CLI call Zitadel at the issuer (through the gateway) so the `Host` matches the instance. In W1 they run on the host, where `auth.chelabase.localhost` resolves. W2 revisits this inside the stack network.

## 4. Studio, docs, landing and Mailpit behind the gateway

- **Studio (`apps/studio/vite.config.ts`):** `server.allowedHosts` is the Studio host. `server.hmr` uses `clientPort` = the gateway's port and `host` = the Studio host, so hot reload goes through the gateway. The dev server still binds `127.0.0.1:3000` (`strictPort`) as the gateway's upstream. Studio's own links to docs or Mailpit come from env.
- **Docs and landing (`apps/docs`, `apps/landing`):** the same `allowedHosts` and HMR settings, from `CHELABASE_DOCS_HOST` and `CHELABASE_LANDING_HOST`. `just docs` and `just landing` stay host commands and are reached at their hosts.
- **Mailpit:** served at the root of the mail host, with its UI websocket going through the gateway.
- **What nobody addresses directly any more (S2):** Studio :3000, Zitadel :8080, Mailpit :8025, docs :3001 and landing :3002. They stay bound to 127.0.0.1 in W1 only so that the host-side gateway can reach them. W2 unpublishes them. An offline guard test (`scripts/tests/entry-urls.sh`, run by `just check`) fails if any script, e2e test, Rust live test, Playwright config or doc outside the gateway's own upstream defaults names `localhost:3000`, `:8080`, `:8025`, `:3001` or `:3002`.

## 5. Callers moved to the new URLs

- **Scripts:** `scripts/auth-e2e.sh`, `members-e2e.sh`, `neon-e2e.sh`, `scripts/lib/neon-common.sh`, the `_cleanup` Mailpit call and `create-db-demo` use the API host, the Studio host and the mail host. Each URL comes from one shared shell helper, `scripts/lib/entry.sh`, which reads the host settings.
- **Rust live tests:** the `GATEWAY_URL` defaults in `app_users_live`, `roles_live`, `merge_live` and the rest become `http://api.chelabase.localhost:8000`. The other defaults (metadata DB, storcon, valkey) are unchanged in W1.
- **Other callers:** the Playwright configs in the e2e runs and the browser-pass instructions use `http://studio.chelabase.localhost:8000`, as do `packages/client/scripts/e2e.mjs` and `tools/bench-data-api`.
- **Docs:** the `CONTRIBUTING.md` URLs and port table, the `.env.example` comments, and the self-hosting docs page for hosts and DNS (`apps/docs`, "Self-hosting → Hosts"). The page explains the three host names, one wildcard or three DNS records, TLS at the gateway or a front proxy, and why Zitadel needs its own host. It also explains that Studio and the API must share a registrable domain that isn't a public suffix (`acme.com`, not `acme.pages.dev`).

## 6. Tests

- **Gateway (unit):**
  - a table of host → scope, covering case, a trailing dot, a port, IPv6 and no match → 404;
  - the startup refusals (a missing host, a removed setting, an issuer that conflicts);
  - forwarded headers replaced;
  - CORS on `Api` only, for Studio's origin only.
- **Gateway (proxy tests with a local upstream):** a Studio-scope websocket upgrade round-trips, and `Host` reaches the Auth upstream unchanged.
- **Config derivation in each service:** auth, orchestrator, worker and the CLI derive the same URLs from the same settings.
- **Bootstrap:** request tests for the redirect, post-logout and login URIs, and an update of an existing app whose URIs differ.
- **Offline guard:** `scripts/tests/entry-urls.sh`.
- **Live pass:** `just check`; the orchestrator, rest, worker and catalog live suites; the auth, members, sql-editor, console, merge, role and branch e2e runs. The browser pass covers:
  - sign-in at `studio.chelabase.localhost:8000`, through Zitadel at `auth.chelabase.localhost:8000`;
  - Zitadel's console;
  - a Studio hot reload;
  - Mailpit at `mail.chelabase.localhost:8000` during a sign-up with verification on;
  - docs and landing at their hosts.

## S-rulings (made while writing; for review)

- **S1 Zitadel domain move:** rely on `start-from-init` re-running setup, and fall back to `just zitadel-reset` plus the bootstrap. Cost if wrong: Zitadel users created by hand in the workbench are lost on the fallback (the demo tenant and admin come back).
- **S2 Loopback upstream ports stay in W1:** the gateway is a host process until W2, so it still reaches Studio, Zitadel, Mailpit, docs and landing on 127.0.0.1. The guard test makes sure nothing else uses those ports. Cost: none beyond W2 unpublishing them.
- **S3 Studio is an upstream in both modes:** the gateway proxies Studio, whether that is a Vite dev server or a static file server. How self-hosted serves Studio's built files is packaging work. Cost: none now.
- **S4 HTTP/1.1 from the gateway to Zitadel:** Zitadel's console and our bootstrap use REST, gRPC-web or Connect, which work over HTTP/1.1. Native gRPC (h2c) clients aren't supported through the gateway. Cost if wrong: a self-hoster using a native-gRPC Zitadel SDK has to reach Zitadel some other way, or we add h2c upstream support later.
- **S5 Strict hosts:** an unknown host gets a 404, and `CHELABASE_API_HOSTS` is removed. This is a breaking config change, but no self-hosters exist yet. Cost: none.
- **S6 One source per URL:** Studio's origin, the issuer and the API base are derived from the host settings. The old variables refuse startup with a pointer to their replacements.
- **S7 Forwarded headers:** the gateway replaces `X-Forwarded-*` on every scope (never appends a client's), so Zitadel and Studio see the public scheme and host.
