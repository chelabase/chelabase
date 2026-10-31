# Contributing

How to set up a machine, run the development stack, test, release images and commit.

**The workbench is the development loop; the Compose install is what users run.** `just workbench up` builds the Rust services on your machine and runs them, with dev-only extras (Mailpit, a demo admin, a test runner), in an isolated stack per checkout. Self-hosters never use it: they follow the [Install](apps/docs/content/docs/self-hosting/install.mdx) page, which runs the release images from `infra/compose`. The [README](README.md) is for them.

Agent workflow (epics, issues, white-room audits): see [AGENTS.md](AGENTS.md).

## Repository layout

```
apps/studio            Studio, the web console
apps/docs              the documentation site (content in apps/docs/content/docs)
apps/landing           the landing page
services/              gateway, orchestrator, worker, rest, auth, realtime, init (chelabase-init)
crates/                shared Rust libraries: auth, http, init, metadata, nats, neon, query,
                       routes, schema, secrets, types, zitadel
packages/              TypeScript packages: client, management-client, design (design system), neon-e2e
openapi/               generated OpenAPI documents (management, login, data)
infra/compose/         the self-hosted install (docker-compose.yml, .env.example,
                       docker-compose.build.yml) and the workbench overlay (docker-compose.workbench.yml)
infra/images/          the release images: server.Dockerfile, studio.Dockerfile, docker-bake.hcl
infra/neon/            Neon image pins and build notes
infra/workbench/       the workbench image
tools/cli              the `chelabase` binary (workbench, dev bootstrap, type generation)
tools/bench-data-api   the data API benchmark harness
scripts/               e2e, stack, image and codegen scripts; scripts/tests holds the offline checks
docs/                  security notes and benchmarks; roadmap/ and superpowers/ are archive only
```

## Prerequisites

- [proto](https://moonrepo.dev/docs/proto), which installs Rust, moon, node, pnpm, just and sccache at the versions in `.prototools`.
- Docker with the Compose plugin (2.30 or newer) and buildx. Every stack's services run in containers, and the worker starts each database's compute container.
- cmake (Pingora builds zlib-ng): `sudo apt-get install -y cmake`
- clang and protoc (`pg_query` builds Postgres's parser for the SQL editor's guard): `sudo apt-get install -y clang protobuf-compiler`
- psql (PostgreSQL client): `sudo apt-get install -y postgresql-client`
- [gh](https://cli.github.com) 2.83 or newer, from [GitHub's own apt repo](https://github.com/cli/cli/blob/trunk/docs/install_linux.md). Distro packages lag (Ubuntu's is 2.46), and before 2.73 `gh pr edit` fails on the Projects (classic) deprecation.
- [cargo-nextest](https://nexte.st), the test runner behind `just test`: `cargo install cargo-nextest --locked`
- sccache as cargo's compiler wrapper: proto installs it, but registering it is one-time machine setup (below).

## Machine setup (one time)

Install cargo-sweep (build cleanup):

```bash
cargo install cargo-sweep --locked
```

Cargo runs rustc from outside the repo, where the proto shim can't see this repo's `.prototools`, so register sccache globally once:

```bash
proto plugin add sccache https://raw.githubusercontent.com/appthrust/proto-toml-plugins/main/sccache/plugin.toml --to global
proto pin sccache 0.18.0 --to global
```

Then make sccache cargo's wrapper in `~/.cargo/config.toml` (create it if it doesn't exist; cargo comes from rustup, not a proto shim, so `.prototools` can't set it):

```toml
[build]
rustc-wrapper = "sccache"
```

This is per machine, not per repo. Every worktree shares the cache. Without cargo-sweep, `just sweep-builds` prints a notice and skips.

## Quickstart (the workbench)

```bash
git clone https://github.com/chelabase/chelabase.git
cd chelabase
proto install
pnpm install
just workbench up
```

`just workbench up` builds the Rust services on the host, starts the Compose services, runs the bootstrap, then runs the services and Studio in containers, and prints the URLs. The first run builds a lot and takes a while.

Open http://studio.chelabase.localhost:8000 and choose **Sign in** as the bootstrap admin (see [Bootstrap admin and seed tenant](#bootstrap-admin-and-seed-tenant)). Self-sign-up is off: invite everyone else (or switch open sign-up on and approve requests). Emails such as invites and password resets land in Mailpit at http://mail.chelabase.localhost:8000.

The docs site runs in the stack too, reached through the gateway:

```bash
just workbench up --with docs     # then open http://docs.chelabase.localhost:8000/docs
```

Stop the stack with `just workbench down` (volumes and state stay). `just` lists every recipe.

## Recipes (`just`)

Run `just` to list them all. Each checkout (the main one and every worktree) has its own stack, managed with `just workbench …` (see [Worktrees and their stacks](#worktrees-and-their-stacks)).

### The workbench

| Command | What it does |
| --- | --- |
| `just workbench up [--with docs,landing]` | Start this checkout's stack: the Compose services (metadata Postgres, NATS, Valkey, Zitadel, the Neon storage stack and proxy, Mailpit), the bootstrap, then the apps (orchestrator, worker, gateway, rest, auth, realtime, Studio) in containers built from your host-built binaries. Waits in line when the stack cap is reached. Prints the URLs |
| `just workbench restart [app…]` | Rebuild and recreate the named apps (all six Rust apps when none); `studio`, `docs` and `landing` can be named too |
| `just workbench down [--stack <slug>]` | Stop the stack and remove its computes; volumes and state stay |
| `just workbench purge [--stack <slug>] [--yes]` | `down`, then delete the volumes, state directory and registry entry. Main is purged only when named and confirmed: `just workbench purge --stack main --yes` |
| `just workbench status` | Every stack: checkout, ports, running or stopped, URLs; then the queue |
| `just workbench logs [-f] [service…]` | The stack's Compose logs |
| `just workbench psql [db]` | `psql` in the metadata database |
| `just workbench exec <service> <cmd…>` | A command in one of the stack's containers |
| `just workbench test [--stack <slug>] [selection…] [-- run-args…]` | Live Rust tests in the stack's runner container (see [Live Rust tests](#live-rust-tests)) |
| `just workbench run [--stack <slug>] [--env K=V] <script> [args…]` | A repo script in the runner, on the stack's network (the e2e recipes use it) |
| `just workbench env` | Print the path of this stack's `stack.env` |
| `just zitadel-bootstrap [admin_email]` | Rerun this stack's bootstrap, as `workbench up` does (idempotent; workbench only) |
| `just zitadel-reset` | Wipe only this stack's Zitadel volume so FirstInstance rewrites `admin.pat` |
| `just auth-token` | Mint a short-lived demo JWT access token (stdout) |
| `just create-db-demo [name]` | Authenticated `POST /v1/databases` through this stack's gateway, in the seed project; `name` defaults to `demo1` |
| `just sweep-dbs` | Dev reset for this stack: remove its Neon computes, its storage-controller tenants and its metadata rows for databases, branches and endpoints |

### Checks, code generation and builds

| Command | What it does |
| --- | --- |
| `just check` | The offline script checks in `scripts/tests/` (entry URLs, the compose files and their secrets, the image pins, retired names, …), `just deny`, `cargo check`, `cargo clippy -D warnings`, `cargo nextest run`, the generated-code freshness check (`just api-gen` if it fails), and typecheck plus tests for the TypeScript packages, Studio, the docs site and the landing page (the sites are built too) |
| `just test` | Rust tests with nextest, then doctests |
| `just api-gen` | Regenerate `openapi/*.json` and the generated clients. Never edit those by hand |
| `just deny` / `just deny-install` | RustSec advisories and yanked crates against `Cargo.lock`; install the pinned cargo-deny |
| `just neon-test` | Offline checks of the Neon build scripts, Compose services and neon scripts |
| `just neon-build` | Build the Neon images from source at `infra/neon/NEON_REF` (about 40 minutes and 40 GB of disk) |
| `just images` | Build every release image (the per-app images, `chelabase/server` and `chelabase/studio`) from one cargo build, tagged `chelabase/<image>:latest`. Separate from the workbench. Smoke check: `scripts/tests/images-smoke.sh` |
| `just landing-visual [--update-snapshots]` | Pixel check of the landing page against committed baselines (not part of `just check`) |
| `just sweep-builds [days=N]` | Delete build artifacts older than 14 days (or `N`) from the main and every worktree's `target/` |
| `just wt-add <branch> [base]` / `just wt-remove <branch>` | Create a worktree under `.worktrees/`, or purge its stack and remove it (see [Worktrees and their stacks](#worktrees-and-their-stacks)) |

The end-to-end recipes are in [Tests and e2e runs](#tests-and-e2e-runs).

### Smoke test: create a database

```bash
just workbench up
just create-db-demo demo1        # 202 and a database_id; positional (name=demo1 would be the literal name)
TOKEN=$(just auth-token)
curl -sS -H "Authorization: Bearer $TOKEN" \
  http://api.chelabase.localhost:8000/v1/databases/<database_id>   # poll until status is ready
```

`GET <api url>/v1/databases/<id>/connection` (same token) returns the owner credentials and a `postgresql://postgres:…@<ep>.localtest.me:<proxy port>/<db>?sslmode=verify-full` URI (the proxy port is 4432 on main; `just workbench status` shows it). Connect with the stack's proxy CA: `psql "<uri>&sslrootcert=.chelabase/stacks/<slug>/neon/proxy-ca.crt"`, adding `&hostaddr=127.0.0.1` if `localtest.me` resolves to `::1` first. The response's `ca_file` field names the CA file (self-hosted mode; `null` in platform mode). `CHELABASE_PROXY_CA_FILE` only sets the path reported in `ca_file`; it doesn't change which CA the proxy uses.

`DELETE <api url>/v1/databases/<id>` removes it (or use Studio). `just sweep-dbs` is the dev-only "remove everything" reset for the stack.

## Bootstrap admin and seed tenant

The bootstrap is `chelabase-init bootstrap`. A self-hosted install runs it as the `init-bootstrap` one-shot, with no admin: the first sign-up with the setup code claims the instance. A workbench stack runs it with `--demo` instead (`just workbench up` does, and `just zitadel-bootstrap` reruns it), which adds the dev-only objects:

- a tenant admin: Zitadel's first human user, `zitadel-admin@zitadel.<auth host name>` (on main, `zitadel-admin@zitadel.auth.chelabase.localhost`), or the user you pass as `admin_email` (it must exist in Zitadel). Sign in to Studio as that user;
- the demo machine user and its key, behind `just auth-token` and `just create-db-demo`;
- Mailpit as Zitadel's SMTP provider, when no SMTP is configured.

Without `--demo`, every run deactivates Zitadel's human instance admins; with it, the first human user stays active.

If bootstrap fails with a missing `admin.pat` (an old Zitadel volume from before FirstInstance):

```bash
just zitadel-reset && just zitadel-bootstrap
```

A Zitadel reset leaves no account but the bootstrap admin: sign in as that user and invite the rest.

The metadata migrations seed one tenant and project, in every install, workbench or not. The bootstrap binds the tenant to its Zitadel org (`tenants.zitadel_org_id`). The fixed IDs are what `just create-db-demo` posts:

| Entity | Slug | UUID |
| --- | --- | --- |
| Tenant | `demo` | `00000000-0000-4000-8000-000000000001` |
| Project | `default` | `00000000-0000-4000-8000-000000000002` |

IAM artifacts are in the stack's state directory, `.chelabase/stacks/<slug>/` (gitignored; `<slug>` is `main` for the main checkout); the apps' env files are in its `zitadel/` (`demo.env`) after bootstrap. Zitadel PATs are opaque, so demo calls use JWT-profile access tokens from `chelabase dev token` (`just auth-token`).

## Workbench URLs

Everything a browser or a script reaches goes through the stack's gateway, by host name. The main checkout's stack uses port 8000 and the host names below; a worktree's stack `x` puts its slug in the host (`studio.x.chelabase.localhost:<port>`) and uses its own port. `just workbench status` prints the URLs of every stack.

| URL (main) | What |
| --- | --- |
| http://studio.chelabase.localhost:8000 | Studio |
| http://api.chelabase.localhost:8000 | The management API, the data API and the app-user sign-in routes |
| http://auth.chelabase.localhost:8000 | Zitadel (console at `/ui/console`) |
| http://docs.chelabase.localhost:8000 | The docs site (`just workbench up --with docs`) |
| http://landing.chelabase.localhost:8000 | The landing page (`just workbench up --with landing`) |
| http://mail.chelabase.localhost:8000 | Mailpit (workbench only) |

`chelabase.localhost` is deliberate: `*.localhost` resolves to loopback without any setup, and a stack's hosts share one registrable parent, so Studio and the API count as the same site and the `SameSite=Strict` sign-in cookies work. (Studio on `127.0.0.1` and the API on `localhost` would be cross-site.) Different stacks have different hosts, so cookies and Zitadel sessions never mix between them. The hosts come from `CHELABASE_{STUDIO,API,AUTH,DOCS,LANDING,MAIL}_HOST`, `CHELABASE_PUBLIC_SCHEME` and `CHELABASE_GATEWAY_LISTEN`, which `just workbench up` renders per stack; the rules are on the docs page [Hosts](apps/docs/content/docs/self-hosting/hosts.mdx).

`scripts/lib/entry.sh` is the one place scripts and recipes get these URLs: `source` it and use `CHELABASE_ENTRY_{STUDIO,API,AUTH,DOCS,LANDING,MAIL}_URL` (and the derived `CHELABASE_AUTH_HOST_NAME`, `CHELABASE_PUBLIC_PORT`, `CHELABASE_PUBLIC_SECURE`, `CHELABASE_ZITADEL_TLS_MODE`). Never write a URL literal in a script; `scripts/tests/entry-urls.sh` (run by `just check`) fails on them.

## Ports

On the host, a workbench stack publishes only two ports, on loopback (`127.0.0.1` and `::1`). Every other service is reachable only inside the stack's network, by service name (`metadata-db:5432`, `nats:4222`, `orchestrator:8081`, and so on); tests and scripts run there, in the runner container (`just workbench test|run`), and `just workbench psql|exec` reach in.

| Port | main | worktree stacks | What |
| --- | --- | --- | --- |
| Gateway | 8000 | 8100–8199 | HTTP front door (Pingora); the only port to open in a browser |
| Neon proxy (Postgres wire) | 4432 | 4500–4599 | `<ep>.localtest.me:<port>` for `psql` and drivers |

A worktree's ports are allocated on its first `up` (the lowest free in the range) and kept until `purge`. The registry is `.chelabase/stacks/registry.json`, and `just workbench status` lists it. What a self-hosted install publishes is on the [Install](apps/docs/content/docs/self-hosting/install.mdx) page.

Docker's Compose project is `chelabase` for main and `chelabase-<slug>` for a worktree. Computes are labelled `chelabase.dev/stack=<slug>`, so a stack's worker never touches another's.

Configuration: [`infra/compose/.env.example`](infra/compose/.env.example) holds the documented defaults. A workbench stack runs `docker-compose.yml` with the `docker-compose.workbench.yml` overlay, and `just workbench up` renders its `.chelabase/stacks/<slug>/env/stack.env` from `.env.example` plus the per-stack values (print its path with `just workbench env`). You don't copy `.env.example` to a `.env` for the workbench; only a self-hoster does, in `infra/compose`.

## Worktrees and their stacks

Every checkout runs its own stack: its own Compose project, volumes, network, computes, Zitadel instance, state directory (`.chelabase/stacks/<slug>/`) and host names. Up to 4 stacks run at once by default (`CHELABASE_WORKBENCH_SLOTS` sets the cap); when the cap is reached, `just workbench up` waits in a FIFO queue (it gives up after 45 minutes; `CHELABASE_WORKBENCH_WAIT` changes that). The main checkout stays on `main`.

```bash
just wt-add my-feature                # .worktrees/my-feature, branched from main
just wt-add epic/my-epic              # .worktrees/epic-my-epic: a / in the branch becomes -
just wt-add task-branch epic/my-epic  # branched from the epic; uses the epic's stack
just workbench up                     # (inside a main-based worktree) its own stack, on its own ports
just wt-remove my-feature             # purges its stack, removes the worktree; keeps the branch
```

`just wt-add <branch> [base]` makes `.worktrees/<branch>` (with `/` as `-`), creating the branch from `base` (default `main`) if it doesn't exist and reusing a local one if it does. It symlinks `.chelabase` from the main checkout and runs `pnpm install --frozen-lockfile`. It starts no stack:

- based on `main` (or `origin/main`), the worktree gets its own stack from `just workbench up`;
- based on another branch, it builds the CLI and records that branch's stack in `.workbench-stack`, so `just workbench test` and `run` use it. If the base has no stack, the file is empty and it says so: bring the base's stack up there, or pass `--stack`.

For a branch that exists only on origin, run `git fetch && git branch <branch> origin/<branch>` first.

`just wt-remove <branch>` purges the worktree's own stack (containers, volumes, state directory, registry entry), then removes the worktree (refusing if it has changes) and its `target/`. Delete the branch yourself with `git branch -d <branch>`.

Each worktree:

- has its own `target/` directory, and compilation is shared machine-wide through sccache;
- symlinks `.chelabase` from the main checkout, which holds the machine's keys once (secrets key, gateway and cookie keys, compute-JWT keys; mounted read-only into every stack) and every stack's state;
- runs its own stack from its own binaries: run `just workbench …` from inside the worktree. Another checkout's stack is reached with `--stack <slug>` (`down`, `purge`, `test`, `run`); `status` lists them all.

`just workbench down` when you're done frees the slot. Build artifacts pile up: `just sweep-builds` drops those older than 14 days from every `target/`.

## Tests and e2e runs

Offline checks need no stack:

- `just test`: Rust tests with nextest (one process per test), then doctests, which nextest doesn't run.
- `just check`: everything in the table above.
- `just neon-test`: the Neon build scripts, Compose services and neon scripts.

### Live recipes on the workbench

These run against your checkout's own stack: each is `just workbench run scripts/<name>.sh` in the stack's runner container, so they start (or update) the stack first. Run them from the checkout you are working in; two checkouts can run theirs at the same time. Most scripts honour `KEEP=1`, which leaves the database in place.

| Recipe | What it runs |
| --- | --- |
| `neon-smoke` | Checks the running Neon storage stack with a throwaway tenant |
| `neon-e2e` | Create a database, then psql, SQL over HTTP and the serverless driver through the proxy, query cancel, a password reset that works at once, delete |
| `branch-e2e` | Branch at a time, reset to parent, restore with a backup, a point outside the history window, delete |
| `storage-leak-e2e` | Deleting a database, a branch or a superseded timeline reclaims the safekeeper's WAL and S3 backup |
| `compute-network-e2e` | What a compute can reach: no NATS, Valkey or databases; every listener on the compute network needs a credential or is public by design |
| `auth-e2e` | Closed sign-up, a link invite and accepting it, sign-in, reset and logout through the gateway and auth |
| `role-e2e` | Login roles: create a plain role, connect, back a secret key, refused delete, reset, revoke, delete |
| `members-e2e` | An email invite (link from Mailpit) and a link invite, a grant, the SQL editor as a group role, revoke, open sign-up and approval |
| `personal-logins-e2e` | Personal Postgres logins: connect through the proxy, a child branch, reset, revocation on grant loss, suspend, revoke-now, Zitadel offboarding, expiry |
| `teams-e2e` | A team-only member runs SQL as a Developer team role, a downgrade to Viewer refuses it, a Viewer team role reads through the data API, removal and delete |
| `data-e2e` | Data API story (below) |
| `console-e2e` | Data API across databases and Studio's console (below) |
| `app-users-e2e` | App users story (below) |
| `sql-editor-e2e` | SQL editor story (below) |
| `merge-e2e` | Schema merge: a branch and main both change, one conflict resolved, rehearse, a protected main needs confirm, apply, main checked with psql |
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

### Self-hosted install e2e

These test the Compose install the way a self-hoster runs it. They need no workbench and touch no other project: each makes a throwaway Compose project of its own (unique name, free port and control subnet, its own `.env`) and removes it on exit, pass or fail.

| Recipe | What it runs |
| --- | --- |
| `selfhost-e2e` | A fresh `up --wait`, every service healthy, the setup code in the logs, sign-up refused without the code, the claim, a project and a database through the API and the proxy, the Zitadel webhook, no secret in `docker inspect`, each service reading its own secrets, a rerun that changes nothing, the documented uninstall leaving nothing behind, and a failing install (SMTP host with no sender) |
| `selfhost-tls-e2e` | The install behind nginx terminating TLS with a throwaway CA and no port in the hosts: an https issuer, the claim and sign-in, the gateway and realtime verifying tokens, in-network calls to Zitadel, and `down -v` leaving nothing |

Images:

- `selfhost-e2e` builds them from your checkout by default (with `docker-compose.build.yml`, sharing the cache with `just images`). With `SELFHOST_E2E_IMAGES=local` it runs the base compose file on images already built by `just images` instead (`CHELABASE_IMAGE_REPO` and `CHELABASE_IMAGE_TAG`, default `chelabase` and `latest`; nothing is pulled).
- `selfhost-tls-e2e` always runs on local images: `just images` first (or set `CHELABASE_IMAGE_REPO` and `CHELABASE_IMAGE_TAG`). It never builds an image, and it needs node for its socket client.

```bash
just images
SELFHOST_E2E_IMAGES=local just selfhost-e2e
just selfhost-tls-e2e
```

`SELFHOST_E2E_KEEP=1` and `SELFHOST_TLS_E2E_KEEP=1` leave the project up; tear it down with the Install page's uninstall (with `-p <name>`), or `docker compose -p <name> down -v` for the TLS one.

### Live Rust tests

Some Rust tests are `#[ignore]`d because they need the stack. Run them on your checkout's stack with `just workbench test -p <pkg> --test <file> -- --run-ignored only`: it makes sure the stack is up and current, archives the tests on the host and runs them in the runner container with the stack's environment (service names, the demo identity, the compute image). Arguments before `--` select what is archived; those after it go to nextest.

- `rest_live` (`just workbench test -p rest --test rest_live -- --run-ignored only`): two databases with their own default schemas and caches, allow-lists (and stored lists following a SQL rename and drop), a disable with and without rest's invalidate call, and a child branch serving its parent's second database.
- `app_users_live` (`just workbench test -p worker --test app_users_live -- --run-ignored only`): the bootstrap is idempotent; the guard and the protect triggers on a real compute; DDL elsewhere works even with the users table broken; the helpers and the built-in policies under the data API; a custom role's key reads with and without app users; the data API's rebootstrap keeps the objects and its remove drops the helpers (or is refused by a dependent policy); sign-up data and marked deletes; and the worker's enable, sync fan-out, branch hook, resync and disable with the real Zitadel.
- `storcon_live` (`just workbench test -p chelabase-neon --test storcon_live -- --run-ignored only`): a round trip through the Rust storage-controller client. Other live files live in `services/worker/tests/` (`branch_live`, `neon_live`, `roles_live`).

## CI (GitHub Actions)

Two workflows in `.github/workflows/`. Until the repo is public (Actions minutes are billed on a private repo) both run **by hand only** (`workflow_dispatch`). Neither uses `pull_request_target` or any secret, and both have `permissions: contents: read`. Actions are pinned to commit SHAs.

| Workflow | What it runs |
| --- | --- |
| `ci.yml` | On ubuntu-latest: the toolchain from `.prototools` (via `moonrepo/setup-toolchain`), the apt packages from Prerequisites, `cargo-nextest` and `cargo-deny` 0.20.2, then `just check` and the doctests (`cargo test --workspace --doc`, all `just test` adds) |
| `live.yml` | `just workbench up`, every recipe in *Live recipes on the workbench* except `bench-data-api`; the self-hosted e2e and the live Rust tests (`--run-ignored`) are not run (a suite failure doesn't stop the rest; the run fails at the end), then `just workbench down`; the status and Compose logs are uploaded as the `live-logs` artifact |

**At launch**, enable the triggers (one change per workflow, each marked `# enable at launch`):

- `ci.yml`: uncomment `pull_request:` and `push: { branches: [main] }`.
- `live.yml`: uncomment the nightly `schedule:`. For a `run-live` PR label, uncomment the `pull_request:` block and the job's `if:` line too.

**Checking a workflow without Actions minutes:** `actionlint` for syntax and expressions, and `act` to run it in Docker (both are single binaries from their GitHub releases; install them outside the repo):

```bash
actionlint
act workflow_dispatch -W .github/workflows/ci.yml
act workflow_dispatch -W .github/workflows/live.yml --container-daemon-socket /var/run/docker.sock --artifact-server-path /tmp/act-artifacts   # brings up a workbench stack
```

`act` needs a runner image (`-P ubuntu-latest=catthehacker/ubuntu:act-24.04`; the `full` image is about 75 GB) and a GitHub token (`-s GITHUB_TOKEN=…`, a fine-grained token with no access is enough): `proto` resolves the Rust, just and moon versions through the GitHub API, and without a token it fails with "Failed to resolve … to a valid supported version". Don't pass `--bind`: the runner's user (uid 1001) takes ownership of the bound checkout. Neither workflow has been run under `act` yet.

`act` can't prove: that its runner image matches ubuntu-latest (the packages, the Docker and Compose versions: the workbench needs Compose 2.30 or newer), how `actions/cache` and `rust-cache` behave on GitHub's cache service, timing and disk limits on the hosted runner, and the `workflow_dispatch` UI. The first real run at launch checks these.

## Releasing images

The install pulls the per-app images and Studio from `ghcr.io/chelabase`, each pinned by digest in `infra/compose/docker-compose.yml`. Each pin has its own commit, so a release ships only the images it names. Build once, test those exact images, push them as they are. Never rebuild between the test and the push.

Every `publish-images.sh` run selects its images:

- `--service <image>` (repeatable) takes exactly those images, even one whose inputs are unchanged since its pin (it prints a note).
- `--all` takes the images whose inputs changed since their pins. If none did, it says so and builds nothing.
- With neither, it refuses and prints the changed set. Naming every image with `--service` rebuilds them all.

`scripts/image-inputs.sh` works out each image's inputs from the build graphs: for a Rust image, its closure of workspace crates from `cargo metadata` plus the manifests of every crate in the shared cargo build, `Cargo.toml`, `Cargo.lock` and `rust-toolchain*`; for Studio, `apps/studio` and its `workspace:` packages, the pnpm lockfile and workspace files, `openapi/` and `.env.example` (outside its pin block). `server.Dockerfile`, `docker-bake.hcl`, `.dockerignore` and the release scripts are inputs of every image. A pin whose commit isn't in your history counts as changed. `just check` fails when a Dockerfile `COPY` brings in a path outside its images' inputs.

```bash
# 1. Build and tag ghcr.io/chelabase/<image>:<sha12> locally, at a clean HEAD. Nothing is pushed.
scripts/publish-images.sh --all               # or: --service studio --service gateway

# 2. Test the set that will ship: the new images at <sha12>, the rest at their pins.
SELFHOST_E2E_IMAGES=local CHELABASE_IMAGE_REPO=ghcr.io/chelabase CHELABASE_IMAGE_TAG_STUDIO=<sha12> \
  just selfhost-e2e                           # one CHELABASE_IMAGE_TAG_<IMAGE> per built image

# 3. Push them without rebuilding, with the same selection. It refuses, before any network call,
#    unless the tree is clean and each selected image's revision label is HEAD. It prints one
#    `pin <image> <sha12> sha256:<digest>` line per pushed image.
scripts/publish-images.sh --push-only --all | tee digests.txt

# 4. Write those pins into docker-compose.yml and .env.example (the other pins stay), check them,
#    and commit.
scripts/pin-images.sh digests.txt
scripts/tests/compose-image-pins.sh digests.txt
```

`scripts/publish-images.sh --push` builds and pushes in one go, with no test in between: it is not the release path. `PUBLISH_IMAGES_REPO` replaces `ghcr.io/chelabase` (a local registry, a test prefix). The all-in-one `chelabase/server` image stays local. The Neon images are built and published from the `chelabase/neon` fork; see [`infra/neon/README.md`](infra/neon/README.md).

## Commits and branches

- Work on a feature branch in its own worktree. Never switch branches in the main checkout; it stays on `main`.
- Commit subjects use `type(area): summary` (for example `fix(studio): keep the query string on redirect`). Stage explicit paths only.
- Changes land on `main` through pull requests, which a maintainer squash-merges. Without push access, fork the repo and open the PR from your fork.
- After your PR merges, remove its worktree (`just wt-remove <branch>`) and delete the branch.
- Before building on a branch, `git fetch` and check whether origin has newer commits. Never force-push; if a push is rejected, fetch and integrate first.
- Never run `cargo fmt` on the workspace; rustfmt only the files you create.
- Never overwrite `.chelabase/secrets/secrets.key`, the gateway key pair, the cookie key or the compute-JWT key pair.
- Before adding a dependency, check the registry for the latest release rather than trusting an older pin. Prefer the security-stronger option when two are equivalent (aws-lc-rs over ring). pnpm's minimum release age applies.
- `tokio-postgres` is pinned to the `chelabase/rust-postgres` fork by `rev` in the workspace `Cargo.toml`; return to crates.io once the upstream change is released (https://github.com/rust-postgres/rust-postgres/pull/1391).
