# Stack survey for the workbench rework (2026-10-13)

The survey below is of `main` at `4616bcad`, read-only. An agent did it for the stack and worktree rework (roadmap: "the stack and worktree rework"). Paths are relative to the repo root. Redo the parts a later change touches before relying on them.

## 1. Compose (`infra/compose/docker-compose.yml`, the only compose file)

- **Project and network:** the project name is hard-coded as `name: chelabase` (:4), so the network is always `chelabase_default`. There is no `container_name`, and all volumes are project-scoped. The volumes are metadata_pg_data, nats_data, valkey_data, zitadel_pg_data, neon_seaweedfs_data, neon_pageserver_data, neon_safekeeper1_data, neon_storcon_pg_data and neon_proxy_tls.
- **Bind mount:** `../../.chelabase/zitadel:/zitadel-bootstrap` (:129). Through the worktree symlink it resolves to the main checkout's directory.
- **Published ports, all on 127.0.0.1:**

  | Service | Port(s) |
  |---|---|
  | metadata-db | 5433 |
  | nats | 4222, 8222 |
  | valkey | 6379 |
  | zitadel-db | 5434 |
  | zitadel | 8080 |
  | mailpit | 8025 |
  | safekeeper1 | 7676 |
  | storage-controller | 1234 |
  | pageserver | 9898 |
  | proxy | 4432, 4444 |

- **Compose features:** there are no `profiles:`. Every service has a healthcheck except zitadel and mailpit.
- **Zitadel env:** `EXTERNALDOMAIN=localhost`, `EXTERNALPORT=8080` (:100-101). The login v2 base URI and the default login and logout URLs are `http://localhost:3000` (:115-117).
- **Callbacks to the host:** the storcon callback is `http://host.docker.internal:8083/storcon` (:251) and the proxy auth endpoint is `…:8083/proxy/api/v1` (:372). Both are compose literals.
- **Image pins:** the images are pinned through `${NEON_IMAGE:-…}` and `${COMPUTE_NODE_IMAGE:-…}`. `scripts/neon-images.sh` parses those defaults, so the form must stay.
- **No `-p`:** every caller passes `-f infra/compose/docker-compose.yml --env-file infra/compose/.env.example` without `-p`. The callers are `justfile:7-8, 124, 146, 150, 271, 283, 474, 586`, `scripts/zitadel-reset.sh`, `scripts/lib/neon-common.sh:21`, `tools/cli/src/paths.rs:87-95` (which honors `COMPOSE_FILE` and `COMPOSE_ENV`) and `tools/cli/src/dev/tenant.rs:20,147,212`.

## 2. Host app tier (`justfile` `dev` :257-354, `dev-down` :357-379)

- **Processes:** `cargo run` starts the orchestrator (8081, internal listener 8083), the worker (health 8082), the gateway (8000), auth (8084) and rest (8085, internal 8086). `pnpm` starts Studio on 3000; Studio uses `strictPort`, and `CHELABASE_STUDIO_PORT` overrides the port. The docs (3001) and landing (3002) sites run only through `just docs` and `just landing`.
- **`.chelabase/dev/` files:** `pids` blocks a second `just dev`, `worktree` names the owner checkout, and each process has a `*.log`. `dev-down` kills the process groups.
- **Hard-coded port cleanup:** `_cleanup` (:398) kills whatever holds the ports 8000, 8081, 8082, 8084, 8085, 8086 and 3000.
- **Internal address:** `scripts/internal-addr.sh` exports `CHELABASE_INTERNAL_ADDR`/`URL` on port 8083. It uses 127.0.0.1 on Docker Desktop and the bridge gateway on native Linux.
- **Defaults in the service code:**
  - `CHELABASE_STORCON_URL` is :1234, `CHELABASE_VALKEY_URL` is :6379, and `CHELABASE_SAFEKEEPER_HTTP` is `1=http://127.0.0.1:7676`.
  - `CHELABASE_COMPUTE_NETWORK` is `chelabase_default`, and `CHELABASE_CONTROL_PLANE_URI` is `http://host.docker.internal:8083`.
  - The proxy ports are 4432 and 4444, and the proxy domain is `localtest.me` (`services/orchestrator/src/lib.rs:64-106`).
- **Retired per-slot database switch:** `justfile:287-296` can point the metadata URL at another database name. It is the old per-slot mechanism.

## 3. Gateway (`services/gateway`)

- **Listener and routes:** it listens on 127.0.0.1:8000 and sends `/v1/*` to the orchestrator, `/auth/v1/*` to auth and `/data/v1/*` to rest (`crates/chelabase-routes`). `CHELABASE_API_HOSTS` sets the host match; empty matches any host.
- **CORS and Studio URLs:** `CHELABASE_STUDIO_ORIGINS` sets CORS and defaults to `http://localhost:3000`. Auth uses its first value for email links. The orchestrator uses `CHELABASE_STUDIO_URL` for invite links.
- **What bypasses it:** Studio :3000, Zitadel :8080, docs :3001, landing :3002, Mailpit :8025, the proxy (4432/4444) and the internal listener :8083.
- **Studio's API URL:** Studio gets `VITE_API_URL` from `apps/studio/.env.local`, written by bootstrap (`GATEWAY_URL = "http://localhost:8000"`, `tools/cli/src/dev/bootstrap.rs:17`). For cookies, localhost and 127.0.0.1 are different sites (`bootstrap.rs:277`).

## 4. Computes (`crates/chelabase-neon/src/{backend,compute}.rs`)

- **Container setup:** computes are Docker API containers on `settings.network`, with `extra_hosts host.docker.internal`. Ports 3080 and 5432 are bound to 127.0.0.1 on random host ports. Each carries the label `chelabase.dev/endpoint-id` and the name `chelabase-compute-<ep>`. Neither is scoped to a stack.
- **Cross-stack deletion:** `DockerBackend::list` (`backend.rs:434-448`) lists every labelled container on the daemon. The worker's orphan removal (`reconcile.rs:731`, `CHELABASE_WORKER_REMOVE_ORPHANS`, default true) would therefore delete another stack's computes. `just sweep-dbs` and `deps-down` remove all computes machine-wide.
- **Callbacks:** compute_ctl, storcon and the proxy call back on `host.docker.internal:8083`. So do the Zitadel Actions (`tools/cli/src/dev/steps.rs:252`, `/internal/zitadel/events`).
- **Reaching a compute:** the host reaches a compute through its random loopback ports. The proxy reaches it by IP on `chelabase_default`. Unit tests pin `chelabase_default`.

## 5. Zitadel bootstrap (`tools/cli/src/dev/*`, `tools/cli/src/paths.rs`)

- **Paths:** everything sits under one `.chelabase/`: `zitadel`, `secrets`, `auth`, `neon` and `gateway`, each with env overrides. `repo_root()` is the nearest ancestor that has a `justfile`.
- **Issuer and Studio origin:** the issuer is `http://localhost:8080`. `STUDIO_ORIGIN = "http://localhost:3000"` (`requests.rs:110`) gives the redirect URIs `…:3000/callback` and post-logout `…:3000/`, and sets `developmentMode: true`. Tests pin 3000.
- **What bootstrap writes:**
  - `.chelabase/zitadel/demo.env`, which also holds the internal signing keys, the secrets key, the compute-JWT dir and the dev tokens;
  - `actions.env`;
  - `demo-sa.json`;
  - `apps/studio/.env.local` (VITE_ZITADEL_*, VITE_API_URL).
- **PAT files:** Compose writes `admin.pat` and `login-client.pat` into the bind mount, on a fresh volume only.

## 6. Live tests and e2e

- **`just live` queue:** `just live` runs through `scripts/live.sh`, a machine-wide FIFO queue and flock under `<main>/.chelabase/live`. `_live-check` refuses callers that don't hold the lock. `_live-swap-dev` (`justfile:74-105`) restarts the host apps in the caller's worktree, then polls :8000/health and :3000.
- **Hard-coded URLs:**
  - `scripts/auth-e2e.sh` and `members-e2e.sh` use :8000, Mailpit :8025 and Studio :3000.
  - `scripts/neon-e2e.sh` uses storcon 1234, the gateway 8000 and the proxy 4432/4444, and reads `.chelabase/neon/proxy-ca.crt`.
  - `neon-smoke.sh` uses 1234 and 9898.
  - `scripts/lib/neon-common.sh` runs `compose exec` into metadata-db.
  - Every e2e script sources `.chelabase/zitadel/demo.env`.
- **Rust live tests:** they default to 127.0.0.1 ports: metadata 5433, gateway 8000, internal 8083, orchestrator 8081, storcon 1234 and valkey 6379. They read env overrides in some places.
- **Offline checks:** `scripts/tests/live.sh`, `scripts/tests/internal-addr.sh`, `scripts/tests/stack-reset-slots.sh` and `infra/neon/tests/run.sh`.
- **Earlier attempt:** the retired per-worktree slots are in `docs/superpowers/plans/2026-10-02-worktree-stacks.md` and `scripts/stack-reset-slots.sh`.

## 7. Worktree tooling

- **`just wt-add`:** creates `.worktrees/<branch with / as ->`, symlinks `.chelabase` and `apps/studio/.env.local` from the main checkout, then runs `pnpm install --frozen-lockfile`.
- **`just wt-remove`:** keeps the branch and stops no stack.
- **`.claude/settings.json`:** `worktree.bgIsolation: "none"` and the impeccable plugin.
- **Text to update for the rename:**
  - `CLAUDE.md` ("One machine, one dev stack");
  - `CONTRIBUTING.md:66-71, 139-170`;
  - the preamble's stack bullet;
  - the "dev stack" strings in `justfile` and `scripts/live.sh`;
  - `.chelabase/dev/` and `just dev`/`dev-down`;
  - the CLI `chelabase dev bootstrap`, `DEV_USER_EMAIL` and `LIVE_SWAP_DEV`.

## 8. Hardest per-stack points

1. **Fixed host ports everywhere.** With only the gateway published, the host apps move into the stack (Compose DNS) or get per-stack ports.
2. **The 8083 callback through `host.docker.internal`.** It appears in Compose literals, the scripts, the service defaults and the Zitadel Actions target.
3. **Zitadel's issuer and Studio's origin.** Both are baked into Zitadel's env, the app's redirect URIs, `demo.env` and tokens.
4. **The project name `chelabase` and the network `chelabase_default`.** The unscoped compute label, the compute name and the orphan removal all act across stacks.
5. **The shared `.chelabase/` through the symlink.** It holds the PATs, `demo.env`, keys, the proxy CA, `dev/` and the `live/` lock. `apps/studio/.env.local` is shared too.
6. **`live.sh` and `_live-swap-dev`.** Both model one stack with a global lock.
7. **The CLI's bootstrap.** It assumes fixed ports and the global compose file, with no `-p`.
8. **The proxy.** It uses the fixed 4432/4444, `<ep>.localtest.me` and the single shared CA copy.
9. **Fixed-port side services.** The e2e scripts read Mailpit :8025, and docs and landing use `strictPort`.

## 9. Claude Code worktree behaviour (docs and changelog, read 2026-10-13)

- **Pinning:** a session or subagent that enters a worktree (`--worktree`, `EnterWorktree`, `isolation: "worktree"`) is still pinned to it: no Edit/Write or Bash in the main checkout, and its subagents inherit the pin. From a pinned session, `EnterWorktree` takes only paths under `.claude/worktrees/`, so `.worktrees/<branch>` can't be reached that way. Subagent worktrees branch from the default branch. The Agent tool has no path, branch or base argument.
- **Settings:**
  - `worktree.baseRef` is `fresh` or `head`; it can't be a branch name.
  - `worktree.bgIsolation` is `worktree` (the default) or `none` (ours).
  - Also available: `worktree.symlinkDirectories` and `worktree.sparsePaths`.
- **Hooks:** a `WorktreeCreate` hook receives a `name`, may create the worktree anywhere and run any setup (such as a Compose project), and prints the path. A `WorktreeRemove` hook receives `worktree_path`. `WorktreeRemove` doesn't run when a subagent finishes, and a Bash `git worktree add` doesn't trigger either hook.
- **No other isolation:** ports, env and services are never isolated. `.worktreeinclude` copies gitignored files, except when a `WorktreeCreate` hook is used.
- **Changes since 2.1.222:**
  - 2.1.251: background sessions and subagents can edit a worktree made with `git worktree add`.
  - 2.1.290: background subagents no longer lose write access when the main session switches worktrees.
  - 2.1.295: a subagent's git status shows its own worktree.
- **Conclusion:** our hand-made `.worktrees/<branch>` stay, as do "never `EnterWorktree`" and `bgIsolation: none`. Per-branch stacks are our own tooling (`just`), not Claude Code's. Unverified: whether a plain `claude` started inside a hand-made worktree is pinned.
