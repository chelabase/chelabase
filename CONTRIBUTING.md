# Contributing

How to set up a machine, run the stack, test, and commit. For what the system is and how it works, run `just docs` and open http://docs.chelabase.localhost:8000/docs/architecture (with the stack up: the docs host is reached through the gateway, which `just dev` starts).

## Prerequisites

- [proto](https://moonrepo.dev/docs/proto)
- Docker (Compose dependencies and the Docker socket for the worker)
- Rust toolchain (the workspace uses edition 2024)
- cmake (Pingora builds zlib-ng): `sudo apt-get install -y cmake`
- clang and protoc (`pg_query` builds Postgres's parser for the SQL editor's guard): `sudo apt-get install -y clang protobuf-compiler`

```bash
proto install   # rust, moon, node, pnpm, just, sccache from .prototools
pnpm install
```

`just test` also needs [cargo-nextest](https://nexte.st): `cargo install cargo-nextest --locked`.

## Machine setup (one time)

`proto install` provides sccache (build cache). Install cargo-sweep (cleanup):

```bash
cargo install cargo-sweep --locked
```

Cargo runs rustc from outside the repo, where the proto shim can't see this repo's `.prototools`, so register sccache globally once:

```bash
proto plugin add sccache https://raw.githubusercontent.com/appthrust/proto-toml-plugins/main/sccache/plugin.toml --to global
proto pin sccache 0.18.0 --to global
```

Then make sccache cargo's wrapper by adding this to `~/.cargo/config.toml` (create it if it doesn't exist; cargo comes from rustup, not a proto shim, so `.prototools` can't set it):

```toml
[build]
rustc-wrapper = "sccache"
```

This is per machine, not per repo. Every worktree shares the cache. Without cargo-sweep, `just sweep-builds` prints a notice and skips.

## Recipes (`just`)

Run `just` to list them all.

| Command | What it does |
| --- | --- |
| `just deps-up` / `up` | Compose up `-d` (metadata Postgres, NATS, Zitadel, Valkey, the Neon storage stack, Mailpit), then copy the proxy's CA to `.chelabase/neon/proxy-ca.crt` |
| `just deps-down` / `down` | Compose down, and remove the compute containers. Host apps keep running |
| `just deps-ps` | Compose service status |
| `just zitadel-bootstrap` | `cargo run -p chelabase -- dev bootstrap` (idempotent) |
| `just auth-token` | Mint a demo JWT access token (stdout) |
| `just zitadel-reset` | Wipe only the Zitadel volume so FirstInstance rewrites `admin.pat` |
| `just dev` | Compose up, bootstrap, then detached apps (orchestrator, worker, gateway, rest, auth, Studio); prints the seed IDs |
| `just dev-down` | Stop the host processes from `just dev` (Compose stays up) |
| `just stop` | `dev-down`, then `down` |
| `just create-db-demo [name]` | Authenticated `POST /v1/databases` through the gateway (:8000); `name` defaults to `demo1` |
| `just sweep-dbs` | Dev reset: remove every worker-provisioned container and data volume, every Neon compute, and (with deps up) the storage-controller tenants and all metadata rows for databases, branches and endpoints |
| `just cleanup` | Fresh start: stops the dev apps, deletes tenant databases, every Zitadel account and all Mailpit email, then re-bootstraps and sweeps old builds. Keeps the keys and the seeded tenant. Asks first |
| `just check` | Rust check, the OpenAPI freshness checks (`just api-gen` if they fail), and typecheck plus tests for the TypeScript packages; it also typechecks, builds and tests the docs site |
| `just test` | Rust tests with nextest, then doctests |
| `just api-gen` | Regenerate `openapi/*.json` and the generated clients. Never edit those by hand |
| `just docs` | The docs site on http://docs.chelabase.localhost:8000 (reached through the gateway: run `just dev` first) |
| `just sweep-builds [days=N]` | Delete build artifacts older than 14 days (or `N`) from the main and every worktree's `target/` |
| `just wt-add <branch> [base]` / `wt-remove <branch>` | Create or remove a worktree under `.worktrees/` (see below) |
| `just live <recipe or command>` | Queue your command on the machine's one stack; run when it's your turn |
| `just live-status` | Who holds the stack, and who waits |
| `just live-test` | The queue's offline tests (`scripts/tests/live.sh`) |
| `just stack-reset-slots [--yes] [--dry-run]` | One-time cleanup of data left by the retired per-worktree slots: stops the slot projects, deletes their files and databases, then runs `just deps-up`. Asks first; idempotent |
| `just orchestrator` / `worker` / `gateway` / `rest` / `auth` / `studio` | Run one process in the foreground |

The end-to-end recipes are in [Tests and e2e runs](#tests-and-e2e-runs).

```bash
just dev                 # deps, IAM bootstrap, background processes
just create-db-demo      # 202 and a database_id
# poll with a minted token:
#   TOKEN=$(cargo run -q -p chelabase -- dev token)
#   curl -sS -H "Authorization: Bearer $TOKEN" \
#     http://api.chelabase.localhost:8000/v1/databases/<database_id>
just stop
```

If bootstrap fails with a missing `admin.pat` (an old Zitadel volume from before FirstInstance):

```bash
just zitadel-reset && just zitadel-bootstrap
```

`zitadel-bootstrap` binds the demo tenant to the Zitadel org only when the metadata database is up, so run `just deps-up` first, or rerun the bootstrap afterwards (`just dev` also rebinds). A Zitadel reset leaves no account in the tenant but the bootstrap admin that `zitadel-bootstrap` sets (pass `admin_email` to pick the user): sign in as that user. Self-sign-up is off by default, so everyone else is invited.

Logs for detached processes: `.chelabase/dev/*.log`. IAM artifacts: `.chelabase/zitadel/` (gitignored). Zitadel PATs are opaque, so demo calls use JWT-profile access tokens from `chelabase dev token`.

### Smoke test: create a database

1. `just dev` (needs Docker; the worker starts each database's compute container).
2. `just create-db-demo demo1` (positional; `name=demo1` would become the literal name).
3. Poll `GET http://api.chelabase.localhost:8000/v1/databases/<id>` with a minted Bearer token until `status` is `ready`.
4. `GET http://api.chelabase.localhost:8000/v1/databases/<id>/connection` (same token) returns the owner credentials and a `postgresql://postgres:…@<ep>.localtest.me:4432/<db>?sslmode=verify-full` URI. Connect with the proxy's CA: `psql "<uri>&sslrootcert=.chelabase/neon/proxy-ca.crt"`, adding `&hostaddr=127.0.0.1` if `localtest.me` resolves to `::1` first. The response's `ca_file` field names the CA file (self-hosted mode; `null` in platform mode). `CHELABASE_PROXY_CA_FILE` (default `.chelabase/neon/proxy-ca.crt`) only sets the path reported in `ca_file`; it doesn't change which CA the proxy uses.
5. `DELETE http://api.chelabase.localhost:8000/v1/databases/<id>` removes it (or use Studio). `just sweep-dbs` is the dev-only "remove everything" reset.

## Seed tenant and project

| Entity | Slug | UUID |
| --- | --- | --- |
| Tenant | `demo` | `00000000-0000-4000-8000-000000000001` |
| Project | `default` | `00000000-0000-4000-8000-000000000002` |

Bootstrap sets `tenants.zitadel_org_id` for the demo tenant from the local Zitadel org.

## Workbench URLs

Everything a browser or a script reaches goes through the gateway on port 8000, by host name:

| URL | What |
| --- | --- |
| http://studio.chelabase.localhost:8000 | Studio |
| http://api.chelabase.localhost:8000 | The management API, the data API and the app-user sign-in routes |
| http://auth.chelabase.localhost:8000 | Zitadel (console at `/ui/console`) |
| http://docs.chelabase.localhost:8000 | The docs site (`just docs`) |
| http://landing.chelabase.localhost:8000 | The landing page (`just landing`) |
| http://mail.chelabase.localhost:8000 | Mailpit |

`chelabase.localhost` is deliberate: `*.localhost` resolves to loopback without any setup, and the hosts share one registrable parent, so Studio and the API count as the same site and the `SameSite=Strict` sign-in cookies work. (Studio on `127.0.0.1` and the API on `localhost` would be cross-site.) The hosts come from `CHELABASE_{STUDIO,API,AUTH,DOCS,LANDING,MAIL}_HOST`, `CHELABASE_PUBLIC_SCHEME` and `CHELABASE_GATEWAY_LISTEN`, whose workbench values live in [`infra/compose/.env.example`](infra/compose/.env.example); the rules are in the docs page "Self-hosting → Hosts".

`scripts/lib/entry.sh` is the one place scripts and recipes get these URLs: `source` it and use `CHELABASE_ENTRY_{STUDIO,API,AUTH,DOCS,LANDING,MAIL}_URL` (and the derived `CHELABASE_AUTH_HOST_NAME`, `CHELABASE_PUBLIC_PORT`, `CHELABASE_PUBLIC_SECURE`, `CHELABASE_ZITADEL_TLS_MODE`, which Compose requires: every recipe that runs `docker compose` sources it). Never write a URL literal in a script; `scripts/tests/entry-urls.sh` (run by `just check`) fails on them.

## Local dependencies

The upstream ports below are loopback only. Reach the apps through the gateway at the URLs above; a port is for the gateway itself, for debugging, or for services that aren't web apps.

| Service | Host port | Notes |
| --- | --- | --- |
| metadata Postgres | 5433 | Control-plane metadata |
| NATS (+ JetStream) | 4222 (monitor 8222) | Provision queue |
| Zitadel | 8080 | Upstream of the auth host (`CHELABASE_ZITADEL_UPSTREAM`) |
| Zitadel Postgres | 5434 | Zitadel's own database |
| Mailpit | 8025 | Upstream of the mail host (`CHELABASE_MAIL_UPSTREAM`) |
| Gateway | 8000 | HTTP front door (Pingora); the only port to open in a browser |
| Orchestrator | 8081 | Internal HTTP API (reached through the gateway) |
| Worker health | 8082 | `/health` |
| Orchestrator internal | 8083 | Internal listener (proxy, compute_ctl, storage controller) |
| auth | 8084 | Sign-in backend (through the gateway at `/auth/v1`) |
| rest | 8085 | Data API engine (through the gateway at `/data/v1`) |
| rest internal | 8086 | The orchestrator's schema calls to rest |
| Studio | 3000 | Upstream of the Studio host; its sign-in fails on this port, use the Studio URL |
| Docs site | 3001 | Upstream of the docs host (`just docs`) |
| Landing page | 3002 | Upstream of the landing host |
| Neon storage controller | 1234 | Tenants and timelines |
| Neon proxy | 4432, 4444 | Postgres wire; WebSocket and SQL over HTTP |
| Valkey | 6379 | Proxy cache invalidation and query cancel |

Copy [`infra/compose/.env.example`](infra/compose/.env.example) to a root `.env` for dotenv auto-load. Prefer the values in `.chelabase/zitadel/demo.env` after bootstrap.

## Worktrees and the live queue

Each machine has one development stack. Every branch lives in its own worktree under `.worktrees/`, builds into its own `target/`, and takes turns on the shared stack through `just live`, a FIFO queue. The main checkout stays on `main`.

```bash
just wt-add my-feature          # creates .worktrees/my-feature from main, with its own build dir
just wt-add my-branch other     # branches from `other` instead
just wt-remove my-feature       # removes the worktree and its target/, keeps the branch
```

`just wt-add <branch> [base]` makes `.worktrees/<branch>` (a `/` in the branch name becomes `-`), creating the branch from `base` (default `main`) if it doesn't exist and reusing it if it does. It symlinks `.chelabase` and `apps/studio/.env.local` from the main checkout and runs `pnpm install --frozen-lockfile`. `just wt-remove` refuses if the worktree has changes; delete the branch yourself with `git branch -d <branch>`.

`just wt-add` only reuses a local branch. For a branch that exists only on origin, run `git fetch && git branch <branch> origin/<branch>` first, then `just wt-add <branch>`.

Each worktree:

- has its own `target/` directory (cargo's default; no shared `.cargo/config.toml`), and compilation is shared machine-wide through sccache;
- symlinks `.chelabase` and `apps/studio/.env.local` from the main checkout;
- shares the one stack: Compose is machine-wide, and `just dev`'s apps run from whichever worktree last took the stack (`just live` moves them).

A worktree never runs the stack on its own. Stack recipes (`deps-up`, `dev`, `sweep-dbs`, the e2e recipes and the like) refuse while another worktree holds the stack and tell you to wait in line instead. Live tests, e2e runs and browser passes go through the queue and run one at a time:

```bash
just dev                      # start the stack: Compose up, bootstrap, background apps
just live just neon-e2e       # queue a test run; it starts when its turn comes
just live just data-e2e       # queue another; it waits
just live cargo nextest run   # queue any command
just live -- deps-up          # apply new image pins, through the queue
just live-status              # who holds the stack and who waits
just dev-down                 # stop the background apps (Compose stays up)
just stop                     # dev-down, then deps-down (full shutdown)
```

Press Ctrl-C to cancel your run. Build artifacts pile up: `just sweep-builds` drops those older than 14 days from the main `target/` and every worktree's (`days=N` to change); `just cleanup` runs it too.

## Tests and e2e runs

Offline checks need no stack:

- `just test`: Rust tests with nextest (one process per test), then doctests, which nextest doesn't run. Dev builds carry line tables only (no full debug info), which keeps test binaries small.
- `just check`: the typechecks, generated-code freshness and TypeScript tests, plus the docs site.
- `just live-test`: the live queue's own tests.
- `just neon-test`: the Neon build scripts, Compose services and neon scripts.
- `just rename-test`: the old-name rename script.

The live recipes need `just deps-up` and `just dev` (several say so in their description) and run through the queue as `just live <recipe>`. Most scripts honour `KEEP=1`, which leaves the database in place.

| Recipe | What it runs |
| --- | --- |
| `neon-smoke` | Checks the running Neon storage stack with a throwaway tenant |
| `neon-e2e` | Create a database, then psql, SQL over HTTP and the serverless driver through the proxy, query cancel, a password reset that works at once, delete |
| `branch-e2e` | Branch at a time, reset to parent, restore with a backup, a point outside the history window, delete |
| `storage-leak-e2e` | Deleting a database, a branch or a superseded timeline reclaims the safekeeper's WAL and S3 backup |
| `auth-e2e` | Closed sign-up, a link invite and accepting it, sign-in, reset and logout through the gateway and auth |
| `role-e2e` | Login roles: create a plain role, connect, back a secret key, refused delete, reset, revoke, delete |
| `data-e2e` | Data API story (below) |
| `console-e2e` | Data API across databases and Studio's console (below) |
| `app-users-e2e` | App users story (below) |
| `sql-editor-e2e` | SQL editor story (below) |
| `bench-data-api` | Data API round-trip benchmark: four execution variants under netem RTTs |

`data-e2e` runs a live story through the gateway and the proxy:

1. Provision a database, list its public key, create a secret key (role `authenticated`) and a table.
2. Through the SDK: the secret key inserts, the public key reads, the public key's insert is refused (403), and a batch rolls back on error.
3. Generate types with `chelabase gen types` and typecheck a sample: a valid call compiles, a wrong column doesn't.
4. Add a row-level security policy: it hides a row from the public key (`anon`) but not from the secret key's role.
5. Revoke the secret key and check it gets 401 within 35 seconds.

`console-e2e` checks databases and the console:

1. `CREATE DATABASE analytics` in SQL is listed, not served; it gets the default schema `app` and is enabled through the API (`postgres` is 422, an unknown name 404).
2. A secret key allowed only on `analytics` reads it through the gateway, main answers 404 `unknown_database`, and the schema lists `analytics` only; the public key's list is narrowed (effective within the gateway's 30 seconds) and reset.
3. The console as the owner reads main and `analytics` and inserts, updates, deletes and batches.
4. As `anon` under row-level security policies: it reads, inserts, updates and deletes only the rows the policies allow.
5. A plain role reads under its policy; an owner-level role and an unknown role are refused (403); a role dropped after its check answers 404 `unknown_role` once `rest`'s 30-second check cache expires.
6. An owner password set in SQL answers 409 `owner_password_unknown`; after a reset the owner console works at once.
7. Main can't be switched off; disabling `analytics` answers 409 `data_api_disabled` in the console and 404 to the key at once; `?remove=true` drops the `chelabase` schema; the database is deleted.

`app-users-e2e`: a project and a database with app users enabled; an SDK sign-up refused by a `NOT NULL` column (422 `constraint_violation`), then accepted with `data` once the column is granted; each user reads only their own row (and the public key alone nothing), on the users table and on a table with a `chelabase.user_id()` policy; a new branch gets the rows; an email changed in Zitadel reaches the row through the webhook; refresh rotation, the grace answer and `refresh_token_reused` after 11 s, then `user_session_revoked` for the session's access token; a user deleted through the API loses their row on both branches; deleting the project deletes its database and its Zitadel org.

`sql-editor-e2e`: a script as owner and as `anon` under RLS, a notice, an error with its position, a cancel, closing the stream through the gateway (the run's slot is freed), a 75 s `pg_sleep` through the gateway (heartbeats), the guard (`DROP` needs `confirm`; a protected branch holds writes), saved queries (including a stale version) and the schema document.

### Live Rust tests

Some Rust tests are `#[ignore]`d because they need the stack. Each file's header gives its exact command (it sources `.chelabase/zitadel/demo.env` and `scripts/neon-images.sh` first, and takes the compute image from `COMPUTE_NODE_IMAGE`); run it through `just live bash -c '…'`.

- `rest_live` (`cargo test -p rest --test rest_live -- --ignored --test-threads=1`): two databases with their own default schemas and caches, allow-lists (and stored lists following a SQL rename and drop), a disable with and without rest's invalidate call, and a child branch serving its parent's second database.
- `app_users_live` (`cargo test -p worker --test app_users_live -- --ignored --test-threads=1`): the bootstrap is idempotent; the guard and the protect triggers on a real compute; DDL elsewhere works even with the users table broken; the helpers and the built-in policies under the data API; a custom role's key reads with and without app users; the data API's rebootstrap keeps the objects and its remove drops the helpers (or is refused by a dependent policy); sign-up data and marked deletes; and the worker's enable, sync fan-out, branch hook, resync and disable with the real Zitadel.
- `just neon-test-live` (`storcon_live`): a round trip through the Rust storage-controller client. Other live files live in `services/worker/tests/` (`branch_live`, `neon_live`, `roles_live`).

## Commits and branches

- Work on a feature branch in its own worktree. Never switch branches in the main checkout; it stays on `main`.
- Commit identity. There is no global git identity on this machine; commit with
  `git commit`
  (the GitHub account `<maintainer>`). The same applies to the public forks (`chelabase/neon`, `chelabase/postgres`, `chelabase/rust-postgres`).
- Merge into `main` only when the maintainer says so, with `git merge --no-ff`, so the branch's history stays in `main`.
- After a merge and a push of `main`, delete the branch on origin (`git push origin --delete <branch>`) and locally (`just wt-remove <branch>`, then `git branch -d <branch>`).
- Before building on a branch, `git fetch` and check whether origin has newer commits. Push a branch after each commit (`git push -u origin <branch>` the first time) and `main` after a merge. Never force-push; if a push is rejected, fetch and integrate first.
- Never run `cargo fmt` on the workspace; rustfmt only the files you create.
- Never overwrite `.chelabase/secrets/secrets.key`, the gateway key pair, the cookie key or the compute-JWT key pair.
- Before adding a dependency, check the registry for the latest release rather than trusting an older pin. Prefer the security-stronger option when two are equivalent (aws-lc-rs over ring). pnpm's minimum release age applies.
- `tokio-postgres` is pinned to the `chelabase/rust-postgres` fork by `rev` in the workspace `Cargo.toml`; return to crates.io once the upstream change is released (https://github.com/rust-postgres/rust-postgres/pull/1391).
