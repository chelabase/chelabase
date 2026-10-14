# W2: workbench isolation (implementation plan)

**Goal:** every checkout runs its own isolated stack (own Compose project, volumes, network, computes, Zitadel instance, state and host names) through `chelabase workbench …` / `just workbench …`; up to 4 at once with a FIFO wait; the app tier, Studio and the test runner run inside the stack; the `just live` queue, stack swapping and the host app tier are retired; main migrates in place keeping its data.

**Spec:** `docs/superpowers/specs/2026-10-13-workbench-isolation-design.md` (approved 2026-10-13 as a whole: "Approved all, just build it"). Implementers read the spec section their task names.

**Architecture:**
- `crates/chelabase-neon` (compute labels, names, filters, network addresses), read by the worker and the orchestrator (`CHELABASE_STACK`).
- `tools/cli/src/workbench/`: `slug`, `registry`, `slots`, `env`, `argv` (T3, pure and file-level), `commands` and `migrate` (T5, the lifecycle). `main.rs` gets the `Workbench` subcommand (T5).
- `infra/compose/docker-compose.yml` (parametrized project, app tier, runner, sites), `infra/workbench/Dockerfile` (T4).
- `justfile`, `scripts/` (T6). Docs and rules (T7).

**Tech:** Rust 1.99 (`std::fs::File::lock` for flock), clap 4 (workspace), `sha2` 0.11 and `hex` 0.4 (workspace, new to the CLI), serde/serde_json (workspace). Docker 29.9 / Compose v5.6 (`env_file` long syntax with `required: false`). Image `ubuntu:26.04@sha256:f144425ff09be612d6d9ad965196e9cdc23dae1f42110a8a11a3e9a8198759f7` (checked 2026-10-13). Node 24.21.0 (the host's), pnpm 12.6.0 via corepack (the repo's `packageManager`). cargo-nextest 0.9.148 (host, mounted into the runner). **No fork change: no image build of the Neon images; the live pass runs on the pinned images.**

**Standing rule:** the coordinator writes and commits each task's RED tests (below) before the brief, runs them RED and records the result in the Status. Implementers make the tests pass without editing, weakening, skipping or deleting them. Reviewers check that `git diff <red-commit>..HEAD -- <test files>` is empty. When a RED commit needs a module to exist so that its tests compile, the coordinator adds only the empty module declaration.

## Status

- **RESUME HERE (2026-10-14): W2 DONE, merged to `main` (`664f01b9`) on the user's go, branch deleted.** The user chose to restart main fresh instead of migrating it (R7): W1's host apps stopped, project `chelabase` brought down with its volumes, `.chelabase/{zitadel,dev,live}`, the proxy CA copy and `apps/studio/.env.local` removed (machine keys kept); then main's first `just workbench up` came up seeded in 59 s and a Studio sign-in as `zitadel-admin@zitadel.auth.chelabase.localhost` worked. The user left the landing page's `just dev` copy as is. Main's migration path (`migrate_main_layout`) is unit-tested but has not run on real data.
- **R7 (user, 2026-10-14):** merge and restart main fresh with a full volume wipe, instead of the in-place migration. Cost: main's previous databases, Zitadel users and members are gone (the user's choice).
- **Live pass (2026-10-14, R5/R6):**
  - Branch stack `workbench-isolation`: first `up` failed at bootstrap (Zitadel refuses an Actions target whose host doesn't resolve yet); fixed by the live agent with 4 more W2 defects: `efdfa7a5` (Actions target only after the apps), `88a68719` (a test parsed `metadata-db:5432` as an IP), `c348ed15` (worker live tests one at a time: nextest test group, guarded), `b6d8d784` (`catalog_live` one at a time), `9972874e` (runner passes `CHELABASE_LIVE_ENDPOINT`); the coordinator found a sixth (fresh stacks got no tenant admin: Zitadel names its admin after the external domain): RED `0e3c3cd5`, fix `931b61c2`.
  - Suites in the runner: orchestrator 49/49, rest 38/38 (2 flaked once: the known compute_ctl-migration race, roadmap), worker 105/106, chelabase-schema 12/12, chelabase-metadata 228/228, chelabase-query 13/13, chelabase-neon 4/4. All 12 e2e runs pass (`members-e2e` once needed a rerun: a Zitadel reactivate lag). `restart worker` rebuilt and recreated only the worker; a second `up` took 8.3 s and recreated nothing.
  - A fresh scratch stack (`workbench-isolation-live2`) came up seeded in 1m 34s from a cold worktree, with the demo tenant's admin bound; `neon-e2e` on both stacks at once: 11/11 each, each stack's compute created and deleted by its own worker only.
  - Slots: with `CHELABASE_WORKBENCH_SLOTS=2`, a third worktree's `up` waited, reported, and timed out after 60 s naming both stacks with uptimes. `just wt-remove` purged both scratch stacks: no container, volume, network, state dir or registry entry left.
  - Browser pass on the branch stack (`studio.workbench-isolation.chelabase.localhost:8100`): sign-in through Zitadel (OIDC round trip on `auth.…:8100`, Studio's own login), the app shell with API calls across hosts, Zitadel's console, a Studio hot reload through the gateway (`[vite] hot updated`), Mailpit, docs and landing after `up --with docs,landing`. Only console errors: missing favicons (pre-existing).
  - Not W2, reported for the user: `merge_live a_recompute_after_a_committed_phase1_stays_half_applied` fails every run (its 200 ms poll can't catch phase 2's `Online` on a tiny table; a test-design change); the `rest_live` `tuple concurrently updated` race (already in the roadmap); a `members-e2e` Zitadel reactivate lag (and `auth/src/app.rs:789` ignores a 400/412 from reactivate).
- **Done: end-of-step review** (opus): 1 Critical (fresh stacks: the gateway restart-looped before the first bootstrap, the machine-key paths were only in `demo.env`), 2 Important (neon-e2e compared URIs with the in-network proxy port; host-side `zitadel-bootstrap` couldn't resolve `zitadel`), 4 Minor (stale references; CONTRIBUTING's purge line; spec W-8 vs R4; an extra recreate on a fresh `up`); fix-round RED `e71726a5` (with the new `retired-names.sh` guard; the coordinator amended W-8), fix `add9da93`; `just check` green.
- **R5 (coordinator, 2026-10-14):** main's migration can't run before the merge (main's stack runs main's code: its compose file and binaries are W1's until `main` has W2), so the pre-merge live pass runs on branch stacks: this worktree's and a scratch worktree's (isolation and slots); main's first `just workbench up` (the migration) runs right after the merge, with the user's go. W1's host apps are stopped for the pass because W1's worker removes computes it doesn't know (it lists by endpoint label only). Cost if wrong: main's migration is first exercised after the merge; a failure there is fixed on `main` with main's data intact (the migration moves files and keeps volumes).
- **R6 (coordinator, 2026-10-14):** live-pass failures are debugged and fixed by one opus live agent in the branch worktree, which writes its own regression test (RED then GREEN) where a fix is unit-testable; the coordinator reviews each fix commit. A RED round trip through the coordinator per live failure would serialize the pass. Cost if wrong: those tests are the implementer's own, checked by the coordinator and the end-of-step diff, not written independently.
- **Done: T6** (sonnet/sonnet): RED `0adc6776` (+ the retired offline tests deleted), implementation `241872e5`; the coordinator's test scanned itself (git's `*` crosses `/`), corrected in `32da3082` together with a new check (wt-remove must not recompute the slug). Review: 1 Critical (neon-e2e's SQL-over-HTTP curl resolves `*.localtest.me` to loopback in the runner), 3 Important (wt-remove slug in bash; `{{args}}` re-split by the shell; `check` conflict with T2), 5 Minor; fix-round RED `4a38d7c6` (coordinator also anchored `entry-sh.sh`'s recipe check), fix `22791948` (`--connect-to` to the proxy, `set positional-arguments` and `"$@"`, wt-remove runs the CLI from the worktree, studio.env checked, bench binaries from cargo's JSON); verified by the coordinator; merged `9eab6a05`.
- **Done: T7** (sonnet/no review; end-of-step review covers it): `88b4213c` (CLAUDE.md, preamble, roadmap working rules, CONTRIBUTING.md, `.env.example` comments, 9 docs pages); merged `433f1706`. Open for the user at the pause: the landing page (`apps/landing/.../landing-page.tsx:252` and its test and baselines) still says `just dev`.
- **T5** (opus/opus): RED `e5a1c0cc`, implementation `6df5c8d6` (one RED `use` line added, accepted); step branch merged in `ba7d51d0`. Review: not ready, 5 Important (I1 main's `up` would remove a `compute-*` worktree's whole stack by a name substring; I2 `up` on a running stack keeps stale binaries; I3 legacy dev processes not waited for before computes are removed; I4 `wt-remove` on a broken worktree could purge main; I5 the shell's env beats `stack.env` in compose interpolation) and 5 Minor (Zitadel dir not 0777; a missing nextest breaks `run`; half-up stacks taken as running; liveness needs the other stack's compose file and a long build outlives the grace; `run` eats the script's `--help`, `compose_base(None)` without `-p`, bootstrap outside a stack recreates `.chelabase/zitadel`). Fix-round RED `2b1efa4b` (coordinator also changed its own `compose_base(None)` expectation to `-p chelabase`); fix running.
- **Done: T1** (sonnet/opus): RED `5f6b9af7`, implementation `bfc659fd`, review: approve after 1 Important (I1: `CHELABASE_STACK` trimmed in one service only and never validated) and 2 Minors (fixtures still on `host.docker.internal`); fix-round RED `5d5deac8` (coordinator also moved the three fixtures), fix `d5bcda79` (`chelabase_neon::check_stack`, verified by the coordinator); merged `51900e33`. Implementer deviation accepted: an older orchestrator test updated to the P1 default; `DEFAULT_STACK` constant added. The review's note for T5 (remove legacy computes before the apps start) is pinned in T5's section.
- **Done: T3** (opus/opus): RED `99b52009`, implementation `b4e7f13d`, review: approve after 3 Important (I1 a waiter's ticket leaks when its stack starts elsewhere or the wait errors; I2 reap grace during `up`'s builds; I3 two checkouts sharing a slug) and 5 Minor (pid reuse, clock step back, unchecked scheme, no queue API for T5, no dir fsync); I2 pinned in T5's section (15-minute grace in `alive`); fix-round RED `89384609`, fix `7d58f160` (ticket drop guard, checkout clash and takeover, `<pid> <starttime>` tickets, monotonic ticket names, `example_scheme`, `queued`/`drop_tickets`, dir fsync; verified by the coordinator); merged `c9f5d332`.
- **Done: T4** (opus/sonnet): RED `4b87771c`, implementation `328067a5`; the coordinator's RED test was wrong (Compose v5.6's `config` drops `env_file` unless `--no-env-resolution`), corrected in `bcf75d8f`. Review: approve; deviations accepted (`stack.env` `required: false`; auth/rest healthchecks accept any HTTP answer, no health route; `ZITADEL_TLS_ENABLED=false`; orchestrator `CHELABASE_PROXY_PG_PORT=${CHELABASE_PROXY_PORT}`); I1 (curl resolves `*.localhost` to loopback inside the runner), M2 (no cargo in the runner) and M3 (`env_file` read at create) pinned in T5's section with a new RED test (`curlrc`); M4/M5 fix-round RED `832253bf`, fix `982d2f8d` (node checksums gpg-verified against pinned release keys, Docker key fingerprint, architecture mapping; verified by the coordinator); merged `06b223d5`.
- **Done: T2** (sonnet/sonnet): RED `bc57b2dd`, implementation `729a6291`; review: not ready, 5 Important (orchestrator restarts via dev pids and `cargo run`; CA path; a bind address dialled; hard-coded `chelabase-pageserver-1`; CLI lookup) and 4 Minor. Second finding in the area, so the fix round stated an invariant ("a live test depends on nothing that only exists on the host") and guarded it across every test file: fix-round RED `03f08c5a` (18 failures), fix `35c167b7` (orchestrator recreated through Compose with pass-through `CHELABASE_PASSWORD_STRENGTH`/`CHELABASE_BRANCH_DATABASE_CAP`; Compose plugin in the image; `proxy_ca_path`, `pageserver_container`, `CHELABASE_CLI`, `proxy` hostaddr default; loopback literals as `Ipv4Addr::LOCALHOST`); merged `d405b5e5`.
- **R4 (coordinator, 2026-10-14):** W-8 reversed: the runner image has the Compose plugin (the restart tests recreate the orchestrator with a test's settings); scripts still use `docker exec` where that is enough. Cost if wrong: ~60 MB in the image.
- **R3 (coordinator, 2026-10-14):** `cargo clippy -D warnings` fails on `main` already (`chelabase-metadata/members.rs`, `chelabase-routes`, `orchestrator/members.rs`, test fakes); tasks are held to "no warning in a file the task touches". Cost if wrong: none; the pre-existing warnings are left for a later cleanup the user may ask for.
- **Wave 1 RED (coordinator, 2026-10-14):** T1 `5f6b9af7` (compile RED: no `stack` field, `list_filters`, `STACK_LABEL`, `*_addr_on_network`, two-arg `container_name`); T3 `99b52009` (compile RED: every `workbench::*` item unresolved); T4 `4b87771c` (`workbench-compose.sh`: 12 extra published ports; `run.sh`: 6 FAIL: services, no `name:`, published ports, storcon and proxy callbacks, valkey unpublished).
- **R1 (user, 2026-10-13):** design approved as a whole; the spec and plan pauses are skipped ("just build it"); the end-of-step pause before merging stays. Cost if wrong: a design point the user would have changed surfaces at the end-of-step pause instead.
- **R2 (user, 2026-10-13):** paths in docs start at the repo root (`CLAUDE.md`, preamble; `9b8826f7` on `main`).
- Branch `workbench-isolation` (`.worktrees/workbench-isolation`); spec and plan: the branch's first two commits.

## Plan rulings (mechanical, listed for the user)

- **P1 Control-plane default:** the worker's and orchestrator's `CHELABASE_CONTROL_PLANE_URI` default becomes `http://orchestrator:8083` (host.docker.internal goes, spec 3). Cost if wrong: a non-Compose self-host sets it explicitly (it must today too).
- **P2 `stack` default `main`** for `CHELABASE_STACK` in the worker and orchestrator. Cost: none.
- **P3 Slug cut:** cut to 33, trailing `-` trimmed, then `-` + 6 hex of SHA-256 (so a cut never yields `--`). Cost: none.
- **P4 Uptime format** in slot messages: `{h}h {m}m` from one hour, else `{m}m`. Cost: none.
- **P5 `workbench env`** (extra command, not in the spec's table): prints the stack's `stack.env` path, so host-side recipes (`zitadel-reset`) can load it. Cost: none.
- **P6 Bootstrap reaches Zitadel through the gateway:** host-side bootstrap runs with `CHELABASE_ZITADEL_UPSTREAM=http://127.0.0.1:<gw>` (the stack's gateway; `Host` stays the auth host, as W1's `via_upstream` sends it). Cost: none.
- **P7 Zitadel's UpdateTarget is `POST /v2/actions/targets/{id}`** (action service v2). The implementer checks it against Zitadel v4.19.1's API docs; if it differs, it reports a deviation (the RED test pins the path). Cost if wrong: one test line changes.
- **P8 Retired offline tests:** `scripts/tests/{live,internal-addr,stack-reset-slots,dev-recipes}.sh` and the live/dev blocks of `infra/neon/tests/run.sh` are deleted with what they test (T6 RED commit); the compose block of `run.sh` is rewritten for a parametrized project (T4 RED commit).

## Task table (binding)

| Task | Scope | Implementer | Reviewer | Wave | Depends |
|---|---|---|---|---|---|
| T1 | Computes scoped to a stack (spec 3): `chelabase-neon`, worker and orchestrator config, `docker_backend_live.rs`, the `DockerLocator::connect` call sites | sonnet | opus | 1 | — |
| T3 | CLI workbench core (spec 1): `slug`, `registry`, `slots`, `env`, `argv` | opus | opus | 1 | — |
| T4 | Compose and image (spec 2): compose rewrite, `infra/workbench/Dockerfile`, Vite bind settings, `run.sh` compose block, `workbench-compose.sh` in `just check` | opus | sonnet | 1 | — |
| T2 | Rust live tests on service names (spec 5): fallbacks, proxy `hostaddr`, doc comments | sonnet | sonnet | 2 | T1 |
| T5 | CLI workbench commands (spec 4, 6): clap, lifecycle, bootstrap per stack, Actions endpoint, tenant `-p`, migration | opus | opus | 2 | T3, T4 |
| T6 | Recipes and scripts (spec 4, 5): justfile, e2e scripts in the runner, retirements, worktree hooks | sonnet | sonnet | 2 | T4 (names); T5's CLI surface is pinned below |
| T7 | Rules and docs (spec 6): `CLAUDE.md`, preamble, roadmap working rules, `CONTRIBUTING.md`, `.env.example` comments, docs site's self-hosting note if it names `just dev` | sonnet | none (end-of-step review) | 3 | T5, T6 |

Waves run in parallel when their files are disjoint (pre-flight scan), each task in its own worktree (`just wt-add workbench-isolation-tN workbench-isolation`), with at most 3 builds or test suites at once. Until T5 and T6 land, worktrees still use today's tooling (`just wt-add` as is); no task runs a stack. The coordinator runs the end-of-step live pass.

## Shared interfaces (pinned)

**T1, `crates/chelabase-neon`:**
- `compute.rs`: `pub const STACK_LABEL: &str = "chelabase.dev/stack";` · `pub fn container_name(stack: &str, endpoint_id: &str) -> String` (`chelabase-{stack}-compute-{endpoint_id}`) · `pub fn list_filters(stack: &str) -> std::collections::HashMap<String, Vec<String>>` (`{"label": [ENDPOINT_LABEL, "chelabase.dev/stack=<stack>"]}`) · `pub fn control_addr_on_network(inspect: &ContainerInspectResponse, network: &str) -> Option<SocketAddr>` (ip:3080) · `pub fn postgres_addr_on_network(…) -> Option<SocketAddr>` (ip:5432). `published_control_addr` / `published_postgres_addr` are removed. `DockerLocator::connect(network: &str, stack: &str)`.
- `backend.rs`: `ComputeSettings { image, network, control_plane_uri, stack: String }`; `create_body` sets both labels, no port bindings, no exposed ports, no `extra_hosts`.
- Worker `WorkerConfig.stack: String` and orchestrator `InternalConfig.stack: String`, from `CHELABASE_STACK` (default `main`); both `control_plane_uri` defaults `http://orchestrator:8083`.

**T3, `tools/cli/src/workbench/` (all `pub`):**
- `slug`: `pub const MAIN: &str = "main";` · `fn slug_for_dir(dir_name: &str) -> anyhow::Result<String>` · `fn project_name(slug: &str) -> String` · `struct Hosts { pub studio, pub api, pub auth, pub mail, pub docs, pub landing: String }` (each `name:port`) with `fn names(&self) -> Vec<String>` (no ports; order studio, api, auth, mail, docs, landing) · `fn hosts(slug: &str, gateway_port: u16) -> Hosts`.
- `registry`: `#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)] struct Entry { pub slug: String, pub checkout: PathBuf, pub gateway_port: u16, pub proxy_port: u16, pub created_at: u64, pub running_since: Option<u64> }` · `#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)] struct Registry { pub stacks: Vec<Entry> }` with `fn get(&self, slug: &str) -> Option<&Entry>`, `fn get_mut(&mut self, slug: &str) -> Option<&mut Entry>`, `fn ensure(&mut self, slug: &str, checkout: &Path, now: u64, free: impl Fn(u16) -> bool) -> anyhow::Result<Entry>`, `fn remove(&mut self, slug: &str) -> Option<Entry>`, `fn running(&self) -> impl Iterator<Item = &Entry>` · `const MAIN_GATEWAY_PORT: u16 = 8000; const MAIN_PROXY_PORT: u16 = 4432; const GATEWAY_PORTS: RangeInclusive<u16> = 8100..=8199; const PROXY_PORTS: RangeInclusive<u16> = 4500..=4599;` · `fn with_registry<T>(stacks_dir: &Path, f: impl FnOnce(&mut Registry) -> anyhow::Result<T>) -> anyhow::Result<T>` (creates the dir; exclusive lock on `registry.lock`; reads `registry.json`, missing = empty; writes back atomically via a temp file and rename, only when `f` returns `Ok`) · `fn port_is_free(port: u16) -> bool` (binds `127.0.0.1:port` and `[::1]:port`).
- `slots`: `struct SlotOptions { pub cap: usize, pub timeout: Duration, pub poll: Duration, pub status_every: Duration }` · `fn ticket_name(nanos: u128, slug: &str) -> String` (`{nanos:039}-{slug}`, sorts by time) · `fn reap(registry: &mut Registry, alive: impl Fn(&Entry) -> bool) -> Vec<String>` · `fn acquire_slot(stacks_dir: &Path, slug: &str, opts: &SlotOptions, alive: &dyn Fn(&Entry) -> bool, now: &dyn Fn() -> u64, report: &mut dyn FnMut(&str)) -> anyhow::Result<()>` · `fn release_slot(stacks_dir: &Path, slug: &str) -> anyhow::Result<()>` · `fn cap_from_env(v: Option<&str>) -> anyhow::Result<usize>` · `fn wait_from_env(v: Option<&str>) -> anyhow::Result<Duration>`. Queue dir `<stacks_dir>/queue`, ticket file content: the waiting process's pid.
- `env`: `struct StackEnvInput<'a> { pub slug: &'a str, pub project: &'a str, pub checkout: &'a Path, pub main: &'a Path, pub state: &'a Path, pub gateway_port: u16, pub proxy_port: u16, pub uid: u32, pub gid: u32, pub docker_gid: u32, pub image: &'a str, pub extra: &'a [(String, String)] }` · `fn render_stack_env(example: &str, input: &StackEnvInput) -> String`.
- `argv`: `struct Compose<'a> { pub project: &'a str, pub file: &'a Path, pub env_file: &'a Path }` with `fn args(&self, rest: &[&str]) -> Vec<String>` · `fn nextest_archive_args(selection: &[String], archive: &Path) -> Vec<String>` · `fn nextest_run_args(run: &[String], archive: &Path, checkout: &Path) -> Vec<String>`.

**The `stack.env` keys** (T3 renders them; T4's compose and T6's scripts read them): the example's keys, then (overriding) `CHELABASE_STACK`, `CHELABASE_COMPOSE_PROJECT`, `CHELABASE_STACK_DIR`, `CHELABASE_CHECKOUT`, `CHELABASE_MAIN_CHECKOUT`, `CHELABASE_KEYS_DIR` (`<main>/.chelabase`), `CHELABASE_UID`, `CHELABASE_GID`, `CHELABASE_DOCKER_GID`, `CHELABASE_WORKBENCH_IMAGE`, `CHELABASE_GATEWAY_PORT`, `CHELABASE_PROXY_PORT`, `CHELABASE_PUBLIC_SCHEME`, `CHELABASE_{STUDIO,API,AUTH,MAIL,DOCS,LANDING}_HOST`, `CHELABASE_GATEWAY_LISTEN=0.0.0.0:<gw>`, `CHELABASE_{AUTH,STUDIO,API,MAIL,DOCS,LANDING}_HOST_NAME` (hosts without their port), `CHELABASE_PUBLIC_PORT`, `CHELABASE_PUBLIC_SECURE`, `CHELABASE_ZITADEL_TLS_MODE`, `CHELABASE_ENTRY_{STUDIO,API,AUTH,MAIL,DOCS,LANDING}_URL`, the service-name URLs and `0.0.0.0` listen addresses of T3's test `the_stack_env_keeps_the_example_and_overlays_the_stack`, `CHELABASE_AUTH_TRUSTED_PROXY=true`, `CHELABASE_CONTROL_PLANE_URI`, `CHELABASE_COMPUTE_NETWORK=<project>_default`, `CHELABASE_ZITADEL_ACTIONS_ENDPOINT`, `CHELABASE_ZITADEL_DIR`, `CHELABASE_{SECRETS,AUTH,NEON,GATEWAY}_DIR`, `CHELABASE_COMPUTE_JWT_DIR`, `CHELABASE_PROXY_CA_FILE`, `CHELABASE_{STUDIO,DOCS,LANDING}_BIND=0.0.0.0`, `CHELABASE_CLI=<checkout>/target/debug/chelabase`; then `extra` (T5 passes `NEON_IMAGE`, `COMPUTE_NODE_IMAGE`, `CHELABASE_COMPUTE_IMAGE` from `scripts/neon-images.sh`).

**T5, the CLI surface** (T6 calls it; T5's RED tests pin the parsing):
- `chelabase workbench up [--with docs,landing]` · `restart [app…]` · `down [--stack s]` · `purge [--stack s] [--yes]` · `status` · `logs [-f] [service…]` · `psql [db]` · `exec <service> <cmd…>` · `test [selection…] [-- run-args…]` · `run [--env K=V]… <script> [args…]` · `env` (prints the `stack.env` path).
- In `main.rs`: `Commands::Workbench { command: WorkbenchCommands }` with variants `Up { with: Vec<Site> }`, `Restart { apps: Vec<String> }`, `Down { stack: Option<String> }`, `Purge { stack: Option<String>, yes: bool }`, `Status`, `Logs { follow: bool, services: Vec<String> }`, `Psql { db: Option<String> }`, `Exec { service: String, cmd: Vec<String> }`, `Test { archive: Vec<String>, run: Vec<String> }`, `Run { env: Vec<String>, script: String, args: Vec<String> }`, `Env`. `workbench::commands::Site` is a `clap::ValueEnum` with `Docs`, `Landing`, imported into `main.rs`.
- `workbench::migrate::migrate_main_layout(main: &Path) -> anyhow::Result<Vec<String>>` · `workbench::commands::stale_apps(recorded: &HashMap<String, u64>, current: &HashMap<String, u64>) -> Vec<String>` (sorted) · `workbench::commands::curlrc(hosts: &Hosts, gateway_port: u16) -> String` · `paths::studio_env_file_in(root: &Path, stack_dir: Option<&Path>) -> PathBuf` · `dev::steps::ensure_actions_target(api, env_file: &Path, endpoint: &str)` · `dev::steps::actions_endpoint_from(v: Option<&str>) -> String` · `dev::tenant::compose_base(project: Option<&str>, file: &Path, env: &Path) -> Vec<String>`.
- Runner env inside `test`/`run`: `stack.env`, `demo.env`, plus `GATEWAY_URL=<api url>`, `CHELABASE_PROXY_HOSTADDR=<proxy container IP on the stack network>`, `CHELABASE_PROXY_PG_PORT=4432`, `CHELABASE_PROXY_HTTP_PORT=4444`, `STORCON_URL=http://storage-controller:1234`, `PAGESERVER_URL=http://pageserver:9898`, and the `--env` pairs. The runner runs as `<uid>:<gid>` with the docker group, the checkout mounted read-write at its path (scripts write temp files under it), the key dirs read-only, the state dir read-write, `cargo-nextest` at `/usr/local/bin/cargo-nextest`, working directory the checkout.

## Pre-flight scan (2026-10-13)

- **Files per wave (disjoint):** T1 `crates/chelabase-neon/**`, `services/worker/src/config.rs`, `services/orchestrator/src/{main.rs,internal/config.rs}`, `services/worker/tests/app_users_live.rs` (the one `DockerLocator::connect` call), `.env.example`'s orphan-switch comment is T7's. T3 `tools/cli/src/workbench/{mod,slug,registry,slots,env,argv,tests}.rs`, `tools/cli/Cargo.toml`, one `mod workbench;` line in `tools/cli/src/main.rs`. T4 `infra/compose/docker-compose.yml`, `infra/workbench/**`, `apps/{studio,docs,landing}/vite.config.ts`, `infra/neon/tests/run.sh` (compose block), `scripts/tests/workbench-compose.sh` + fixture, the `check` recipe's one new line in `justfile`. Wave 2: T2 Rust test files under `services/*/tests`, `crates/*/tests`, `tools/*/tests`, `services/rest/src/conn.rs` tests, `scripts/tests/stack-urls.sh`; T5 `tools/cli/src/**` (not T3's files except `mod.rs` declarations), `tools/cli/Cargo.toml`; T6 `justfile`, `scripts/**` (not T2's or T4's test files), `packages/{client,neon-e2e}/**` scripts, `infra/neon/tests/run.sh` (live/dev blocks). T6 and T4 share `justfile` and `run.sh` across waves only.
- **Facts checked:** publishing on `[::1]` works with this daemon and `*.chelabase.localhost` reaches it from the host (probe 2026-10-13); the Rust binaries need only glibc, libm and libgcc_s (ldd); pnpm's store for a checkout is under that checkout's `node_modules/.pnpm`; docker group gid is 973 here (read at run time, never pinned); Zitadel's HTTP-client deny-list blocks loopback only, so `orchestrator:8083` (a private address) is allowed; `std::fs::File::lock` is `flock`, which excludes separate opens in one process (the registry thread test relies on it).
- **Call sites a signature change breaks:** `DockerLocator::connect` (`services/orchestrator/src/main.rs:70,110`, `services/worker/tests/app_users_live.rs:209`, `backend.rs:279`), `container_name` (`compute.rs:143`, `backend.rs:308,389,418`), `ComputeSettings {…}` (`services/worker/src/config.rs:252`, `crates/chelabase-neon/tests/docker_backend_live.rs:37`), `ensure_actions_target` (`tools/cli/src/dev/bootstrap.rs`).
- **Traps:** the existing compute tests pin published ports and `host.docker.internal` (rewritten in T1's RED commit); `run.sh`'s compose block pins project `chelabase`, published Neon ports and `host.docker.internal` (rewritten in T4's RED commit); a test binary's command line contains `target/debug/`, so no test may exercise the dev-process stopping in `migrate`.

---

## T1: computes scoped to a stack (sonnet / opus)

**Files:** `crates/chelabase-neon/src/{compute.rs,backend.rs,lib.rs}`, `crates/chelabase-neon/tests/docker_backend_live.rs`, `services/worker/src/config.rs`, `services/worker/src/reconcile.rs` (only if the listing call needs it), `services/orchestrator/src/{main.rs,internal/config.rs}`, `services/worker/tests/app_users_live.rs` (the `DockerLocator::connect` line only). Test edits marked (coordinator) are in the RED commit.

**Behaviour:** spec section 3 and the pinned interfaces. `list()` uses `list_filters(&self.settings.stack)`. The locator's `control_addr` / `postgres_addr` use the network address. `docker_backend_live.rs`: `ComputeSettings` gets `stack: "livetest"`, the admin-SQL test's network and control-plane URI come from `CHELABASE_COMPUTE_NETWORK` (default `chelabase_default`) and `CHELABASE_CONTROL_PLANE_URI` (default `http://orchestrator:8083`), stack from `CHELABASE_STACK` (default `main`); its doc comment says it runs with `just workbench test -p chelabase-neon --test docker_backend_live -- --run-ignored only`. The orphan-switch's doc in `config.rs` stops calling it a cross-stack guard.

**RED tests (coordinator):**

In `crates/chelabase-neon/src/compute.rs` `mod tests`: replace `containers_are_named_after_their_endpoint`, `a_running_compute_publishes_compute_ctl_on_loopback`, `postgres_addr_reads_the_published_port` and `a_binding_on_another_interface_is_not_used` with the tests below; in `a_stopped_compute_has_neither` replace `published_control_addr(&exited)` with `control_addr_on_network(&exited, "chelabase_default")` and add `assert_eq!(postgres_addr_on_network(&exited, "chelabase_default"), None);`.

```rust
    #[test]
    fn containers_are_named_after_their_stack_and_endpoint() {
        assert_eq!(
            container_name("main", "ep-abc123"),
            "chelabase-main-compute-ep-abc123"
        );
        assert_eq!(
            container_name("feat-x", "ep-abc123"),
            "chelabase-feat-x-compute-ep-abc123"
        );
    }

    #[test]
    fn listing_selects_only_this_stacks_computes() {
        let filters = list_filters("feat-x");
        assert_eq!(filters.len(), 1, "{filters:?}");
        let mut labels = filters["label"].clone();
        labels.sort();
        assert_eq!(
            labels,
            ["chelabase.dev/endpoint-id", "chelabase.dev/stack=feat-x"]
        );
        assert_eq!(STACK_LABEL, "chelabase.dev/stack");
    }

    #[test]
    fn a_running_compute_is_reached_on_the_stack_network() {
        let running = inspect(
            ContainerStateStatusEnum::RUNNING,
            "chelabase-feat-x_default",
            "172.20.0.9",
        );
        assert_eq!(
            control_addr_on_network(&running, "chelabase-feat-x_default"),
            Some("172.20.0.9:3080".parse().unwrap())
        );
        assert_eq!(
            postgres_addr_on_network(&running, "chelabase-feat-x_default"),
            Some("172.20.0.9:5432".parse().unwrap())
        );
        assert_eq!(
            control_addr_on_network(&running, "chelabase_default"),
            None,
            "only the stack's own network"
        );
    }
```

In `crates/chelabase-neon/src/backend.rs` `mod tests`: `settings()` becomes

```rust
    fn settings() -> ComputeSettings {
        ComputeSettings {
            image: "neon/compute-node-v17:test".to_string(),
            network: "chelabase-feat-x_default".to_string(),
            control_plane_uri: "http://orchestrator:8083".to_string(),
            stack: "feat-x".to_string(),
        }
    }
```

In `container_body_runs_compute_ctl_with_the_token_in_env` the URI becomes `http://orchestrator:8083`. In `container_body_limits_restarts_and_networks` the network assertion becomes `Some("chelabase-feat-x_default")` and the `extra_hosts` assertion becomes `assert_eq!(host.extra_hosts, None);`. `compute_ctl_is_published_on_loopback_only` and `create_body_publishes_postgres_on_loopback` are replaced by:

```rust
    #[test]
    fn computes_publish_nothing_on_the_host() {
        let body = create_body("ep-abc123", "t", &settings());
        assert_eq!(body.exposed_ports, None);
        let host = body.host_config.unwrap();
        assert!(host.port_bindings.as_ref().is_none_or(|b| b.is_empty()));
        assert_ne!(host.publish_all_ports, Some(true));
    }

    #[test]
    fn computes_carry_their_stack_label() {
        let labels = create_body("ep-abc123", "t", &settings()).labels.unwrap();
        assert_eq!(labels.get("chelabase.dev/stack"), Some(&"feat-x".to_string()));
        assert_eq!(
            labels.get("chelabase.dev/endpoint-id"),
            Some(&"ep-abc123".to_string())
        );
        assert_eq!(labels.len(), 2);
    }
```

In `services/worker/src/config.rs` `defaults_are_for_local_dev`: the control-plane line becomes `assert_eq!(c.control_plane_uri, "http://orchestrator:8083");` and add `assert_eq!(c.stack, "main");`. New test:

```rust
    #[test]
    fn the_stack_comes_from_the_env() {
        let c = WorkerConfig::from_lookup(lookup(&[IMAGE, ("CHELABASE_STACK", "feat-x")])).unwrap();
        assert_eq!(c.stack, "feat-x");
        let blank = WorkerConfig::from_lookup(lookup(&[IMAGE, ("CHELABASE_STACK", " ")])).unwrap();
        assert_eq!(blank.stack, "main");
    }
```

In `services/orchestrator/src/internal/config.rs` `defaults_are_for_local_dev` add `assert_eq!(c.stack, "main");` and `assert_eq!(c.control_plane_uri, "http://orchestrator:8083");` (the field's existing type; compare with `.as_str()` if it is a `String`). New test:

```rust
    #[test]
    fn the_stack_comes_from_the_env() {
        let c = InternalConfig::from_lookup(|k| (k == "CHELABASE_STACK").then(|| "feat-x".to_string())).unwrap();
        assert_eq!(c.stack, "feat-x");
    }
```

**Gotchas:** `ComputeLocator` impls in test fakes (`services/orchestrator/tests/common/fakes.rs`) don't change. The worker's reconcile tests use `FakeBackend`; `list()` semantics there stay "this backend's computes". A container created before this change (`chelabase-compute-<ep>`, no stack label) is invisible to the new listing: T5's migration removes those.

**Verify:** `cargo nextest run -p chelabase-neon -p worker -p orchestrator`; `cargo clippy -p chelabase-neon -p worker -p orchestrator --all-targets -- -D warnings`; `cargo test -p chelabase-neon --test docker_backend_live --no-run`; `cargo test -p worker --test app_users_live --no-run`.

---

## T3: CLI workbench core (opus / opus)

**Files:** `tools/cli/src/workbench/{mod.rs,slug.rs,registry.rs,slots.rs,env.rs,argv.rs}`, `tools/cli/Cargo.toml` (`sha2`, `hex`, `serde` derive if not already). `tools/cli/src/workbench/tests.rs` (coordinator). The RED commit adds `mod workbench;` to `main.rs` and `mod.rs` with `pub mod argv; pub mod env; pub mod registry; pub mod slots; pub mod slug; #[cfg(test)] mod tests;` and empty files for the five modules.

**Behaviour:** spec section 1 and the pinned interfaces.
- `slug_for_dir`: lowercase; any char outside `[a-z0-9-]` → `-`; collapse runs of `-`; trim `-` at both ends; empty → error naming the input; `main` → `main-wt`; longer than 40 → first 33 chars, trailing `-` trimmed, then `-` + first 6 hex chars of SHA-256 of the **original** name.
- `hosts(MAIN, p)` → `studio.chelabase.localhost:p` etc.; otherwise `studio.<slug>.chelabase.localhost:p`. `project_name(MAIN)` = `chelabase`, else `chelabase-<slug>`.
- `Registry::ensure`: an existing entry is returned unchanged (no probe). New `main` gets 8000/4432 without a probe. A new branch entry takes the lowest gateway port in `GATEWAY_PORTS` and, independently, the lowest proxy port in `PROXY_PORTS` that no entry uses and `free(port)` accepts; none left → error naming the range (`8100–8199` or `4500–4599`, both bounds as numbers). `created_at = now`, `running_since = None`.
- `with_registry`: the lock file is opened (created) and locked exclusively for the whole read-modify-write; a JSON parse error is an error naming the file (never silently reset).
- `acquire_slot` loop, each round under the registry lock: reap with `alive`; if the entry for `slug` is already running → done (unchanged); prune queue tickets whose pid (file content) has no `/proc/<pid>`; ensure our own ticket exists (written once, with our pid); if `running < cap` and our ticket is the oldest → set `running_since = now()`, delete our ticket, done. Otherwise every `status_every` call `report` with a line naming the running stacks and the tickets ahead; sleep `poll` (outside the lock); after `timeout` delete our ticket and fail with a message naming each running stack with its uptime (P4, from `now() - running_since`), the tickets ahead, the waited time, and `just workbench down --stack <slug>`. The slug must have a registry entry (error otherwise).
- `release_slot`: `running_since = None` (missing entry → Ok).
- `cap_from_env`: default 4, integer ≥ 1. `wait_from_env`: default 45 min; `<secs>` or `<n>m`.
- `render_stack_env`: every non-comment `KEY=VALUE` line of the example in its order, with the value replaced when the key is overlaid; then the overlaid keys not in the example in the pinned order; then `extra` (an `extra` key already written is replaced in place, never duplicated). Header comment line `# Generated by chelabase workbench for <slug>; do not edit.`. Values are written literally (no quotes). The public scheme is the example's (normalized as `entry.sh` does: blank → https); `CHELABASE_PUBLIC_PORT` is the gateway port; `CHELABASE_PUBLIC_SECURE` / `CHELABASE_ZITADEL_TLS_MODE` follow the scheme (`true`/`external` for https, `false`/`disabled` for http).
- `nextest_archive_args`: `["nextest","archive","--archive-file",<archive>]` + the selection, or `--workspace` when the selection is empty. `nextest_run_args`: `["nextest","run","--archive-file",<archive>,"--workspace-remap",<checkout>]` + run args.

**RED tests (coordinator):** `tools/cli/src/workbench/tests.rs`:

```rust
//! W2 core: slug, registry, slots, stack env and argv (spec section 1).

use super::argv::{Compose, nextest_archive_args, nextest_run_args};
use super::env::{StackEnvInput, render_stack_env};
use super::registry::{Entry, MAIN_GATEWAY_PORT, MAIN_PROXY_PORT, Registry, with_registry};
use super::slots::{
    SlotOptions, acquire_slot, cap_from_env, reap, release_slot, ticket_name, wait_from_env,
};
use super::slug::{MAIN, hosts, project_name, slug_for_dir};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

// --- slug -----------------------------------------------------------------------------------

#[test]
fn a_worktree_dir_becomes_its_slug() {
    assert_eq!(slug_for_dir("workbench-isolation").unwrap(), "workbench-isolation");
    assert_eq!(slug_for_dir("Feat_X.Y").unwrap(), "feat-x-y");
    assert_eq!(slug_for_dir("--a__b--").unwrap(), "a-b");
    assert_eq!(slug_for_dir("main").unwrap(), "main-wt", "main is the main checkout's");
    assert!(slug_for_dir("___").is_err());
    assert!(slug_for_dir("").is_err());
}

#[test]
fn a_long_dir_is_cut_with_a_hash_of_its_full_name() {
    let name = "members-and-roles-with-a-very-long-branch-name-t3";
    let slug = slug_for_dir(name).unwrap();
    let hash = hex::encode(Sha256::digest(name.as_bytes()));
    assert_eq!(slug, format!("members-and-roles-with-a-very-lon-{}", &hash[..6]));
    assert_eq!(slug.len(), 40);
    let other = slug_for_dir("members-and-roles-with-a-very-long-branch-name-t4").unwrap();
    assert_ne!(slug, other);
    assert_eq!(slug_for_dir(name).unwrap(), slug, "stable");
}

#[test]
fn a_cut_never_leaves_a_double_dash() {
    // the 33rd character is a dash
    let name = "abcdefghij-abcdefghij-abcdefghij-abcdefghij";
    let slug = slug_for_dir(name).unwrap();
    assert!(!slug.contains("--"), "{slug}");
    assert!(slug.starts_with("abcdefghij-abcdefghij-abcdefghij-"));
    assert!(slug.len() <= 40);
}

#[test]
fn slugs_are_dns_labels() {
    for name in ["A b c", "x/y/z", "ÜBER-feature", "1-2-3", "t3"] {
        let slug = slug_for_dir(name).unwrap();
        assert!(
            slug.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
            "{name} -> {slug}"
        );
        assert!(!slug.starts_with('-') && !slug.ends_with('-'), "{name} -> {slug}");
        assert!(slug.len() <= 40);
    }
}

#[test]
fn main_keeps_todays_names_and_ports() {
    assert_eq!(project_name(MAIN), "chelabase");
    let h = hosts(MAIN, 8000);
    assert_eq!(h.studio, "studio.chelabase.localhost:8000");
    assert_eq!(h.api, "api.chelabase.localhost:8000");
    assert_eq!(h.auth, "auth.chelabase.localhost:8000");
    assert_eq!(h.mail, "mail.chelabase.localhost:8000");
    assert_eq!(h.docs, "docs.chelabase.localhost:8000");
    assert_eq!(h.landing, "landing.chelabase.localhost:8000");
}

#[test]
fn a_branch_stack_puts_its_slug_in_every_host() {
    assert_eq!(project_name("feat-x"), "chelabase-feat-x");
    let h = hosts("feat-x", 8137);
    assert_eq!(h.studio, "studio.feat-x.chelabase.localhost:8137");
    assert_eq!(h.api, "api.feat-x.chelabase.localhost:8137");
    assert_eq!(h.auth, "auth.feat-x.chelabase.localhost:8137");
    assert_eq!(
        h.names(),
        [
            "studio.feat-x.chelabase.localhost",
            "api.feat-x.chelabase.localhost",
            "auth.feat-x.chelabase.localhost",
            "mail.feat-x.chelabase.localhost",
            "docs.feat-x.chelabase.localhost",
            "landing.feat-x.chelabase.localhost",
        ]
    );
}

// --- registry -------------------------------------------------------------------------------

fn entry(slug: &str, gw: u16, pg: u16, running: Option<u64>) -> Entry {
    Entry {
        slug: slug.to_string(),
        checkout: PathBuf::from(format!("/w/{slug}")),
        gateway_port: gw,
        proxy_port: pg,
        created_at: 1,
        running_since: running,
    }
}

#[test]
fn main_gets_the_fixed_ports_without_a_probe() {
    assert_eq!((MAIN_GATEWAY_PORT, MAIN_PROXY_PORT), (8000, 4432));
    let mut r = Registry::default();
    let e = r.ensure(MAIN, Path::new("/m"), 10, |_| false).unwrap();
    assert_eq!((e.gateway_port, e.proxy_port), (8000, 4432));
    assert_eq!(e.checkout, PathBuf::from("/m"));
}

#[test]
fn a_branch_gets_the_lowest_free_ports_and_keeps_them() {
    let mut r = Registry::default();
    r.stacks.push(entry("a", 8100, 4500, None));
    let busy_on_host = [8101u16, 4501];
    let e = r
        .ensure("b", Path::new("/w/b"), 10, |p| !busy_on_host.contains(&p))
        .unwrap();
    assert_eq!((e.gateway_port, e.proxy_port), (8102, 4502));
    assert_eq!(e.created_at, 10);
    assert_eq!(e.running_since, None);
    let again = r.ensure("b", Path::new("/w/b"), 99, |_| false).unwrap();
    assert_eq!(again, e, "kept, and no probe for a known stack (it may be running)");
    assert_eq!(r.stacks.len(), 2);
}

#[test]
fn gateway_and_proxy_ports_are_chosen_independently() {
    let mut r = Registry::default();
    let e = r.ensure("b", Path::new("/w/b"), 1, |p| p != 8100).unwrap();
    assert_eq!((e.gateway_port, e.proxy_port), (8101, 4500));
}

#[test]
fn ports_run_out_with_a_clear_error() {
    let mut r = Registry::default();
    let err = r
        .ensure("b", Path::new("/w/b"), 1, |_| false)
        .unwrap_err()
        .to_string();
    assert!(err.contains("8100") && err.contains("8199"), "{err}");
    assert!(r.stacks.is_empty(), "nothing registered on failure");
}

#[test]
fn the_registry_file_round_trips_under_its_lock() {
    let dir = tempfile::tempdir().unwrap();
    with_registry(dir.path(), |r| {
        r.stacks.push(entry("a", 8100, 4500, Some(5)));
        Ok(())
    })
    .unwrap();
    let read = with_registry(dir.path(), |r| Ok(r.clone())).unwrap();
    assert_eq!(read.stacks, vec![entry("a", 8100, 4500, Some(5))]);
    assert!(dir.path().join("registry.json").is_file());
    assert!(dir.path().join("registry.lock").is_file());
}

#[test]
fn a_failed_update_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    with_registry(dir.path(), |r| {
        r.stacks.push(entry("a", 8100, 4500, None));
        Ok(())
    })
    .unwrap();
    let res: anyhow::Result<()> = with_registry(dir.path(), |r| {
        r.stacks.clear();
        anyhow::bail!("no")
    });
    assert!(res.is_err());
    assert_eq!(with_registry(dir.path(), |r| Ok(r.stacks.len())).unwrap(), 1);
}

#[test]
fn a_corrupt_registry_is_an_error_not_a_reset() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("registry.json"), "{not json").unwrap();
    let err = with_registry(dir.path(), |r| Ok(r.stacks.len()))
        .unwrap_err()
        .to_string();
    assert!(err.contains("registry.json"), "{err}");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("registry.json")).unwrap(),
        "{not json"
    );
}

#[test]
fn concurrent_writers_never_lose_an_update() {
    let dir = tempfile::tempdir().unwrap();
    let threads: Vec<_> = (0..8)
        .map(|i| {
            let d = dir.path().to_path_buf();
            std::thread::spawn(move || {
                for j in 0..10 {
                    with_registry(&d, |r| {
                        r.stacks.push(entry(&format!("s{i}-{j}"), 0, 0, None));
                        Ok(())
                    })
                    .unwrap();
                }
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
    assert_eq!(with_registry(dir.path(), |r| Ok(r.stacks.len())).unwrap(), 80);
}

// --- slots ----------------------------------------------------------------------------------

fn opts(cap: usize, timeout_ms: u64) -> SlotOptions {
    SlotOptions {
        cap,
        timeout: Duration::from_millis(timeout_ms),
        poll: Duration::from_millis(20),
        status_every: Duration::from_millis(50),
    }
}

fn seed(dir: &Path, entries: Vec<Entry>) {
    with_registry(dir, |r| {
        r.stacks = entries;
        Ok(())
    })
    .unwrap();
}

fn running_slugs(dir: &Path) -> Vec<String> {
    with_registry(dir, |r| Ok(r.running().map(|e| e.slug.clone()).collect())).unwrap()
}

fn queue_len(dir: &Path) -> usize {
    std::fs::read_dir(dir.join("queue")).map(|d| d.count()).unwrap_or(0)
}

#[test]
fn a_free_slot_is_taken_at_once() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path(), vec![entry("main", 8000, 4432, Some(1)), entry("b", 8100, 4500, None)]);
    acquire_slot(dir.path(), "b", &opts(4, 1000), &|_| true, &|| 50, &mut |_| {}).unwrap();
    assert_eq!(running_slugs(dir.path()), ["main", "b"]);
    let b = with_registry(dir.path(), |r| Ok(r.get("b").cloned())).unwrap().unwrap();
    assert_eq!(b.running_since, Some(50));
    assert_eq!(queue_len(dir.path()), 0, "no ticket left behind");
}

#[test]
fn an_already_running_stack_takes_no_second_slot() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path(), vec![entry("a", 8100, 4500, Some(7))]);
    acquire_slot(dir.path(), "a", &opts(1, 100), &|_| true, &|| 50, &mut |_| {}).unwrap();
    let a = with_registry(dir.path(), |r| Ok(r.get("a").cloned())).unwrap().unwrap();
    assert_eq!(a.running_since, Some(7), "unchanged");
}

#[test]
fn a_full_house_times_out_naming_the_running_stacks() {
    let dir = tempfile::tempdir().unwrap();
    seed(
        dir.path(),
        vec![
            entry("main", 8000, 4432, Some(0)),
            entry("alpha", 8100, 4500, Some(0)),
            entry("b", 8101, 4501, None),
        ],
    );
    let mut lines = Vec::new();
    let err = acquire_slot(dir.path(), "b", &opts(2, 200), &|_| true, &|| 3_700, &mut |l| {
        lines.push(l.to_string())
    })
    .unwrap_err()
    .to_string();
    assert!(err.contains("main") && err.contains("alpha"), "{err}");
    assert!(err.contains("1h 1m"), "each stack's uptime: {err}");
    assert!(err.contains("just workbench down --stack"), "{err}");
    assert!(!lines.is_empty(), "status lines while waiting");
    assert_eq!(running_slugs(dir.path()), ["main", "alpha"]);
    assert_eq!(queue_len(dir.path()), 0, "the ticket goes on timeout");
}

#[test]
fn a_slot_freed_while_waiting_is_taken() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path(), vec![entry("a", 8100, 4500, Some(0)), entry("b", 8101, 4501, None)]);
    let d = dir.path().to_path_buf();
    let freer = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(150));
        release_slot(&d, "a").unwrap();
    });
    acquire_slot(dir.path(), "b", &opts(1, 5000), &|_| true, &|| 9, &mut |_| {}).unwrap();
    freer.join().unwrap();
    assert_eq!(running_slugs(dir.path()), ["b"]);
}

#[test]
fn the_oldest_live_ticket_goes_first() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path(), vec![entry("b", 8101, 4501, None), entry("c", 8102, 4502, None)]);
    std::fs::create_dir_all(dir.path().join("queue")).unwrap();
    // b queued earlier, by a live process (this one)
    std::fs::write(
        dir.path().join("queue").join(ticket_name(1, "b")),
        std::process::id().to_string(),
    )
    .unwrap();
    let err = acquire_slot(dir.path(), "c", &opts(1, 150), &|_| true, &|| 9, &mut |_| {})
        .unwrap_err()
        .to_string();
    assert!(err.contains('b'), "names who is ahead: {err}");
    assert!(running_slugs(dir.path()).is_empty(), "c never jumped the queue");
    assert!(dir.path().join("queue").join(ticket_name(1, "b")).exists());
}

#[test]
fn a_ticket_of_a_dead_process_is_dropped() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path(), vec![entry("b", 8101, 4501, None), entry("c", 8102, 4502, None)]);
    std::fs::create_dir_all(dir.path().join("queue")).unwrap();
    let dead = dir.path().join("queue").join(ticket_name(1, "b"));
    std::fs::write(&dead, "999999999").unwrap();
    acquire_slot(dir.path(), "c", &opts(1, 1000), &|_| true, &|| 9, &mut |_| {}).unwrap();
    assert_eq!(running_slugs(dir.path()), ["c"]);
    assert!(!dead.exists());
}

#[test]
fn concurrent_waiters_never_exceed_the_cap() {
    let dir = tempfile::tempdir().unwrap();
    let names: Vec<String> = (0..6).map(|i| format!("s{i}")).collect();
    seed(
        dir.path(),
        names.iter().enumerate().map(|(i, n)| entry(n, 8100 + i as u16, 4500 + i as u16, None)).collect(),
    );
    let threads: Vec<_> = names
        .iter()
        .map(|n| {
            let d = dir.path().to_path_buf();
            let n = n.clone();
            std::thread::spawn(move || {
                acquire_slot(&d, &n, &opts(2, 400), &|_| true, &|| 9, &mut |_| {}).is_ok()
            })
        })
        .collect();
    let taken = threads.into_iter().map(|t| t.join().unwrap()).filter(|ok| *ok).count();
    assert_eq!(taken, 2);
    assert_eq!(running_slugs(dir.path()).len(), 2);
    assert_eq!(queue_len(dir.path()), 0);
}

#[test]
fn dead_stacks_give_their_slot_back() {
    let mut r = Registry::default();
    r.stacks = vec![entry("main", 8000, 4432, Some(1)), entry("gone", 8100, 4500, Some(1))];
    assert_eq!(reap(&mut r, |e| e.slug != "gone"), ["gone"]);
    assert_eq!(r.get("gone").unwrap().running_since, None);
    assert_eq!(r.get("main").unwrap().running_since, Some(1));
    assert!(reap(&mut r, |e| e.slug != "gone").is_empty(), "a stopped stack isn't reaped twice");
}

#[test]
fn acquire_reaps_before_counting() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path(), vec![entry("gone", 8100, 4500, Some(1)), entry("b", 8101, 4501, None)]);
    acquire_slot(dir.path(), "b", &opts(1, 500), &|e| e.slug != "gone", &|| 9, &mut |_| {})
        .unwrap();
    assert_eq!(running_slugs(dir.path()), ["b"]);
}

#[test]
fn an_unregistered_stack_cannot_take_a_slot() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path(), vec![]);
    assert!(acquire_slot(dir.path(), "x", &opts(4, 100), &|_| true, &|| 9, &mut |_| {}).is_err());
}

#[test]
fn slot_settings_from_the_env() {
    assert_eq!(cap_from_env(None).unwrap(), 4);
    assert_eq!(cap_from_env(Some("2")).unwrap(), 2);
    assert!(cap_from_env(Some("0")).is_err());
    assert!(cap_from_env(Some("x")).is_err());
    assert_eq!(wait_from_env(None).unwrap(), Duration::from_secs(45 * 60));
    assert_eq!(wait_from_env(Some("300")).unwrap(), Duration::from_secs(300));
    assert_eq!(wait_from_env(Some("10m")).unwrap(), Duration::from_secs(600));
    assert!(wait_from_env(Some("soon")).is_err());
}

#[test]
fn tickets_sort_by_time() {
    assert!(ticket_name(9, "z") < ticket_name(10, "a"));
    assert!(ticket_name(1, "b").ends_with("-b"));
}

// --- stack env ------------------------------------------------------------------------------

const EXAMPLE: &str = "# The workbench defaults\n\
CHELABASE_METADATA_DATABASE_URL=postgres://chelabase:chelabase@127.0.0.1:5433/chelabase_metadata\n\
CHELABASE_MODE=self-hosted\n\
CHELABASE_PUBLIC_SCHEME=http\n\
CHELABASE_STUDIO_HOST=studio.chelabase.localhost:8000\n\
CHELABASE_GATEWAY_LISTEN=127.0.0.1:8000,[::1]:8000\n\
CHELABASE_APP_AUTH_INTERNAL_TOKEN=chelabase-dev-app-auth\n";

fn input<'a>() -> StackEnvInput<'a> {
    StackEnvInput {
        slug: "feat-x",
        project: "chelabase-feat-x",
        checkout: Path::new("/m/.worktrees/feat-x"),
        main: Path::new("/m"),
        state: Path::new("/m/.chelabase/stacks/feat-x"),
        gateway_port: 8137,
        proxy_port: 4537,
        uid: 1000,
        gid: 1001,
        docker_gid: 973,
        image: "chelabase-workbench:abc123def456",
        extra: &[],
    }
}

fn parse(text: &str) -> HashMap<String, String> {
    text.lines()
        .filter(|l| !l.starts_with('#') && l.contains('='))
        .map(|l| {
            let (k, v) = l.split_once('=').unwrap();
            (k.to_string(), v.to_string())
        })
        .collect()
}

#[test]
fn the_stack_env_keeps_the_example_and_overlays_the_stack() {
    let env = parse(&render_stack_env(EXAMPLE, &input()));
    assert_eq!(env["CHELABASE_MODE"], "self-hosted");
    assert_eq!(env["CHELABASE_APP_AUTH_INTERNAL_TOKEN"], "chelabase-dev-app-auth");
    let expect = [
        ("CHELABASE_STACK", "feat-x"),
        ("CHELABASE_COMPOSE_PROJECT", "chelabase-feat-x"),
        ("CHELABASE_STACK_DIR", "/m/.chelabase/stacks/feat-x"),
        ("CHELABASE_CHECKOUT", "/m/.worktrees/feat-x"),
        ("CHELABASE_MAIN_CHECKOUT", "/m"),
        ("CHELABASE_KEYS_DIR", "/m/.chelabase"),
        ("CHELABASE_UID", "1000"),
        ("CHELABASE_GID", "1001"),
        ("CHELABASE_DOCKER_GID", "973"),
        ("CHELABASE_WORKBENCH_IMAGE", "chelabase-workbench:abc123def456"),
        ("CHELABASE_GATEWAY_PORT", "8137"),
        ("CHELABASE_PROXY_PORT", "4537"),
        ("CHELABASE_PUBLIC_SCHEME", "http"),
        ("CHELABASE_STUDIO_HOST", "studio.feat-x.chelabase.localhost:8137"),
        ("CHELABASE_API_HOST", "api.feat-x.chelabase.localhost:8137"),
        ("CHELABASE_AUTH_HOST", "auth.feat-x.chelabase.localhost:8137"),
        ("CHELABASE_MAIL_HOST", "mail.feat-x.chelabase.localhost:8137"),
        ("CHELABASE_DOCS_HOST", "docs.feat-x.chelabase.localhost:8137"),
        ("CHELABASE_LANDING_HOST", "landing.feat-x.chelabase.localhost:8137"),
        ("CHELABASE_GATEWAY_LISTEN", "0.0.0.0:8137"),
        ("CHELABASE_AUTH_HOST_NAME", "auth.feat-x.chelabase.localhost"),
        ("CHELABASE_STUDIO_HOST_NAME", "studio.feat-x.chelabase.localhost"),
        ("CHELABASE_API_HOST_NAME", "api.feat-x.chelabase.localhost"),
        ("CHELABASE_MAIL_HOST_NAME", "mail.feat-x.chelabase.localhost"),
        ("CHELABASE_DOCS_HOST_NAME", "docs.feat-x.chelabase.localhost"),
        ("CHELABASE_LANDING_HOST_NAME", "landing.feat-x.chelabase.localhost"),
        ("CHELABASE_PUBLIC_PORT", "8137"),
        ("CHELABASE_PUBLIC_SECURE", "false"),
        ("CHELABASE_ZITADEL_TLS_MODE", "disabled"),
        ("CHELABASE_ENTRY_STUDIO_URL", "http://studio.feat-x.chelabase.localhost:8137"),
        ("CHELABASE_ENTRY_API_URL", "http://api.feat-x.chelabase.localhost:8137"),
        ("CHELABASE_ENTRY_AUTH_URL", "http://auth.feat-x.chelabase.localhost:8137"),
        ("CHELABASE_ENTRY_MAIL_URL", "http://mail.feat-x.chelabase.localhost:8137"),
        ("CHELABASE_ENTRY_DOCS_URL", "http://docs.feat-x.chelabase.localhost:8137"),
        ("CHELABASE_ENTRY_LANDING_URL", "http://landing.feat-x.chelabase.localhost:8137"),
        (
            "CHELABASE_METADATA_DATABASE_URL",
            "postgres://chelabase:chelabase@metadata-db:5432/chelabase_metadata",
        ),
        ("CHELABASE_NATS_URL", "nats://nats:4222"),
        ("CHELABASE_VALKEY_URL", "redis://valkey:6379"),
        ("CHELABASE_STORCON_URL", "http://storage-controller:1234"),
        ("CHELABASE_SAFEKEEPER_HTTP", "1=http://safekeeper1:7676"),
        ("CHELABASE_INTERNAL_URL", "http://orchestrator:8083"),
        ("CHELABASE_REST_INTERNAL_URL", "http://rest:8086"),
        ("CHELABASE_ORCHESTRATOR_URL", "http://orchestrator:8081"),
        ("CHELABASE_AUTH_URL", "http://auth:8084"),
        ("CHELABASE_REST_URL", "http://rest:8085"),
        ("CHELABASE_STUDIO_UPSTREAM", "http://studio:3000"),
        ("CHELABASE_ZITADEL_UPSTREAM", "http://zitadel:8080"),
        ("CHELABASE_MAIL_UPSTREAM", "http://mailpit:8025"),
        ("CHELABASE_DOCS_UPSTREAM", "http://docs:3001"),
        ("CHELABASE_LANDING_UPSTREAM", "http://landing:3002"),
        ("CHELABASE_ORCHESTRATOR_ADDR", "0.0.0.0:8081"),
        ("CHELABASE_WORKER_HEALTH_ADDR", "0.0.0.0:8082"),
        ("CHELABASE_INTERNAL_ADDR", "0.0.0.0:8083"),
        ("CHELABASE_AUTH_ADDR", "0.0.0.0:8084"),
        ("CHELABASE_REST_ADDR", "0.0.0.0:8085"),
        ("CHELABASE_REST_INTERNAL_ADDR", "0.0.0.0:8086"),
        ("CHELABASE_AUTH_TRUSTED_PROXY", "true"),
        ("CHELABASE_CONTROL_PLANE_URI", "http://orchestrator:8083"),
        ("CHELABASE_COMPUTE_NETWORK", "chelabase-feat-x_default"),
        (
            "CHELABASE_ZITADEL_ACTIONS_ENDPOINT",
            "http://orchestrator:8083/internal/zitadel/events",
        ),
        ("CHELABASE_ZITADEL_DIR", "/m/.chelabase/stacks/feat-x/zitadel"),
        ("CHELABASE_SECRETS_DIR", "/m/.chelabase/secrets"),
        ("CHELABASE_AUTH_DIR", "/m/.chelabase/auth"),
        ("CHELABASE_NEON_DIR", "/m/.chelabase/neon"),
        ("CHELABASE_GATEWAY_DIR", "/m/.chelabase/gateway"),
        ("CHELABASE_COMPUTE_JWT_DIR", "/m/.chelabase/neon"),
        ("CHELABASE_PROXY_CA_FILE", "/m/.chelabase/stacks/feat-x/neon/proxy-ca.crt"),
        ("CHELABASE_STUDIO_BIND", "0.0.0.0"),
        ("CHELABASE_DOCS_BIND", "0.0.0.0"),
        ("CHELABASE_LANDING_BIND", "0.0.0.0"),
        ("CHELABASE_CLI", "/m/.worktrees/feat-x/target/debug/chelabase"),
    ];
    for (k, v) in expect {
        assert_eq!(env.get(k).map(String::as_str), Some(v), "{k}");
    }
}

#[test]
fn every_key_appears_once_and_extra_pairs_win() {
    let extra = [
        ("COMPUTE_NODE_IMAGE".to_string(), "img:1".to_string()),
        ("CHELABASE_MODE".to_string(), "platform".to_string()),
    ];
    let mut i = input();
    i.extra = &extra;
    let text = render_stack_env(EXAMPLE, &i);
    let keys: Vec<&str> = text
        .lines()
        .filter(|l| !l.starts_with('#') && l.contains('='))
        .map(|l| l.split_once('=').unwrap().0)
        .collect();
    let mut unique = keys.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(keys.len(), unique.len(), "no duplicate keys");
    let env = parse(&text);
    assert_eq!(env["COMPUTE_NODE_IMAGE"], "img:1");
    assert_eq!(env["CHELABASE_MODE"], "platform");
    assert!(!text.contains("127.0.0.1:5433"));
    assert!(text.starts_with("# Generated by chelabase workbench for feat-x"));
}

#[test]
fn the_example_order_is_kept() {
    let text = render_stack_env(EXAMPLE, &input());
    let pos = |k: &str| text.find(&format!("\n{k}=")).unwrap_or_else(|| panic!("{k}"));
    assert!(pos("CHELABASE_METADATA_DATABASE_URL") < pos("CHELABASE_MODE"));
    assert!(pos("CHELABASE_MODE") < pos("CHELABASE_APP_AUTH_INTERNAL_TOKEN"));
    assert!(pos("CHELABASE_APP_AUTH_INTERNAL_TOKEN") < pos("CHELABASE_STACK"));
}

#[test]
fn main_renders_todays_hosts_and_network() {
    let mut i = input();
    i.slug = "main";
    i.project = "chelabase";
    i.gateway_port = 8000;
    i.proxy_port = 4432;
    i.checkout = Path::new("/m");
    i.state = Path::new("/m/.chelabase/stacks/main");
    let env = parse(&render_stack_env(EXAMPLE, &i));
    assert_eq!(env["CHELABASE_STUDIO_HOST"], "studio.chelabase.localhost:8000");
    assert_eq!(env["CHELABASE_COMPUTE_NETWORK"], "chelabase_default");
    assert_eq!(env["CHELABASE_PROXY_PORT"], "4432");
    assert_eq!(env["CHELABASE_CLI"], "/m/target/debug/chelabase");
}

#[test]
fn an_https_example_makes_a_secure_stack() {
    let example = EXAMPLE.replace("CHELABASE_PUBLIC_SCHEME=http", "CHELABASE_PUBLIC_SCHEME=");
    let env = parse(&render_stack_env(&example, &input()));
    assert_eq!(env["CHELABASE_PUBLIC_SCHEME"], "https");
    assert_eq!(env["CHELABASE_PUBLIC_SECURE"], "true");
    assert_eq!(env["CHELABASE_ZITADEL_TLS_MODE"], "external");
    assert_eq!(
        env["CHELABASE_ENTRY_API_URL"],
        "https://api.feat-x.chelabase.localhost:8137"
    );
}

#[test]
fn values_are_written_literally() {
    let text = render_stack_env(EXAMPLE, &input());
    assert!(text.lines().all(|l| l.starts_with('#') || !l.contains('"')));
}

// --- argv -----------------------------------------------------------------------------------

#[test]
fn compose_always_names_its_project_file_and_env() {
    let c = Compose {
        project: "chelabase-feat-x",
        file: Path::new("/w/infra/compose/docker-compose.yml"),
        env_file: Path::new("/s/env/stack.env"),
    };
    assert_eq!(
        c.args(&["ps", "-q"]),
        [
            "compose",
            "-p",
            "chelabase-feat-x",
            "-f",
            "/w/infra/compose/docker-compose.yml",
            "--env-file",
            "/s/env/stack.env",
            "ps",
            "-q"
        ]
    );
}

#[test]
fn nextest_archives_on_the_host_and_runs_in_the_runner() {
    let archive = Path::new("/s/test/archive.tar.zst");
    assert_eq!(
        nextest_archive_args(&["-p".into(), "rest".into()], archive),
        ["nextest", "archive", "--archive-file", "/s/test/archive.tar.zst", "-p", "rest"]
    );
    assert_eq!(
        nextest_run_args(&["--run-ignored".into(), "only".into()], archive, Path::new("/w")),
        [
            "nextest",
            "run",
            "--archive-file",
            "/s/test/archive.tar.zst",
            "--workspace-remap",
            "/w",
            "--run-ignored",
            "only"
        ]
    );
}

#[test]
fn an_empty_selection_archives_the_workspace() {
    assert_eq!(
        nextest_archive_args(&[], Path::new("/a.tar.zst")),
        ["nextest", "archive", "--archive-file", "/a.tar.zst", "--workspace"]
    );
}
```

**Gotchas:** the slot loop must never sleep while holding the lock. Ticket content is read as a pid; unreadable or non-numeric content counts as dead. `running()` iterates in registry order (the tests compare ordered lists). No `unsafe`, no new crates beyond `sha2`/`hex`.

**Verify:** `cargo nextest run -p chelabase workbench`; `cargo clippy -p chelabase --all-targets -- -D warnings`; rustfmt only the new files.

---

## T4: Compose and image (opus / sonnet)

**Files:** `infra/compose/docker-compose.yml`, `infra/workbench/Dockerfile`, `apps/studio/vite.config.ts`, `apps/docs/vite.config.ts`, `apps/landing/vite.config.ts`, `justfile` (one line in `check`: `scripts/tests/workbench-compose.sh` after `dev-recipes.sh`). Coordinator: `scripts/tests/workbench-compose.sh`, `scripts/tests/fixtures/workbench-stack.env`, and the rewritten compose block of `infra/neon/tests/run.sh`.

**Behaviour:** spec section 2 with the `stack.env` keys pinned above.
- No `name:`. Infra services as today minus every `ports:` except the proxy's Postgres listener (`127.0.0.1:${CHELABASE_PROXY_PORT}:4432` and `[::1]:${CHELABASE_PROXY_PORT}:4432`), minus every `extra_hosts`. Callbacks by service name (spec 2). Zitadel's bind mount `${CHELABASE_STACK_DIR}/zitadel:/zitadel-bootstrap`; healthchecks for Zitadel (`/debug/ready`) and Mailpit (`/livez`) if their images carry a client (Zitadel's image has none: use `["CMD", "/app/zitadel", "ready"]` per Zitadel's docs; Mailpit: `["CMD", "/mailpit", "readyz"]`); the implementer checks both against the pinned images and reports what it used.
- App services `orchestrator`, `worker`, `gateway`, `auth`, `rest`: `image: ${CHELABASE_WORKBENCH_IMAGE}`, `user: "${CHELABASE_UID}:${CHELABASE_GID}"`, `working_dir: ${CHELABASE_CHECKOUT}`, `command: ["${CHELABASE_CHECKOUT}/target/debug/<app>"]`, `env_file:` long form (`${CHELABASE_STACK_DIR}/env/stack.env` required, `${CHELABASE_STACK_DIR}/zitadel/demo.env` and for the orchestrator `${CHELABASE_STACK_DIR}/zitadel/actions.env` with `required: false`), volumes (bind, long form): checkout at its path read-only, `${CHELABASE_KEYS_DIR}/{secrets,gateway,auth,neon}` each at its path read-only, `${CHELABASE_STACK_DIR}` at its path read-write; worker and orchestrator add `/var/run/docker.sock` and `group_add: ["${CHELABASE_DOCKER_GID}"]`; each has a healthcheck with `curl -fsS` on its health endpoint; `restart: unless-stopped`; `depends_on` with the infra they need (healthy). The gateway also publishes `127.0.0.1:${CHELABASE_GATEWAY_PORT}:${CHELABASE_GATEWAY_PORT}` and `[::1]:…`, carries `networks: default: aliases:` with the six `CHELABASE_*_HOST_NAME` values (Compose can't strip a port, so `stack.env` carries the names), and depends only on infra (not on the apps).
- `studio`: same image/user/working dir, checkout read-write, `env_file` stack.env plus `${CHELABASE_STACK_DIR}/studio.env` (`required: false`), `command: ["pnpm", "--filter", "@chelabase/studio", "dev"]`, `environment: CHELABASE_STUDIO_PORT: "3000"`. `docs` and `landing`: profile `sites`, `pnpm --filter @chelabase/docs dev` / `@chelabase/landing dev`. `runner`: profile `runner`, image/user/working dir as above, checkout read-write, key dirs read-only, state dir read-write, docker socket and group, `${CHELABASE_NEXTEST_BIN:-/usr/local/bin/cargo-nextest}` bind-mounted read-only at `/usr/local/bin/cargo-nextest` (the CLI sets `CHELABASE_NEXTEST_BIN` to the host's path), `env_file` stack.env + demo.env (optional), `entrypoint: []`, no command.
- Vite configs: `server.host` reads `CHELABASE_STUDIO_BIND` / `CHELABASE_DOCS_BIND` / `CHELABASE_LANDING_BIND` (default `127.0.0.1`), through the existing `entrySetting` helper's environment branch (process env first; never `.env.example`).
- `infra/workbench/Dockerfile`: `FROM ubuntu:26.04@sha256:f144425ff09be612d6d9ad965196e9cdc23dae1f42110a8a11a3e9a8198759f7`; apt `ca-certificates curl jq postgresql-client openssl iproute2 xz-utils`; the Docker CLI from Docker's apt repo (`docker-ce-cli` only); node 24.21.0 from nodejs.org's `linux-x64` tarball verified against its `SHASUMS256.txt`; `corepack enable` and `corepack prepare pnpm@12.6.0 --activate`, with `COREPACK_HOME` readable by any uid; no `USER` line (compose sets it); `HOME=/tmp` so any uid can write caches.

**RED tests (coordinator):**

`scripts/tests/fixtures/workbench-stack.env` (what `render_stack_env` produces for stack `feat-x`, checkout `/w/feat-x`, main `/m`; the parts compose reads):

```
# Fixture: a rendered stack.env for stack feat-x (W2 T4 offline check).
CHELABASE_STACK=feat-x
CHELABASE_COMPOSE_PROJECT=chelabase-feat-x
CHELABASE_STACK_DIR=/m/.chelabase/stacks/feat-x
CHELABASE_CHECKOUT=/w/feat-x
CHELABASE_MAIN_CHECKOUT=/m
CHELABASE_KEYS_DIR=/m/.chelabase
CHELABASE_UID=1000
CHELABASE_GID=1000
CHELABASE_DOCKER_GID=973
CHELABASE_WORKBENCH_IMAGE=chelabase-workbench:fixture000000
CHELABASE_GATEWAY_PORT=8137
CHELABASE_PROXY_PORT=4537
CHELABASE_PUBLIC_SCHEME=http
CHELABASE_STUDIO_HOST=studio.feat-x.chelabase.localhost:8137
CHELABASE_API_HOST=api.feat-x.chelabase.localhost:8137
CHELABASE_AUTH_HOST=auth.feat-x.chelabase.localhost:8137
CHELABASE_MAIL_HOST=mail.feat-x.chelabase.localhost:8137
CHELABASE_DOCS_HOST=docs.feat-x.chelabase.localhost:8137
CHELABASE_LANDING_HOST=landing.feat-x.chelabase.localhost:8137
CHELABASE_STUDIO_HOST_NAME=studio.feat-x.chelabase.localhost
CHELABASE_API_HOST_NAME=api.feat-x.chelabase.localhost
CHELABASE_AUTH_HOST_NAME=auth.feat-x.chelabase.localhost
CHELABASE_MAIL_HOST_NAME=mail.feat-x.chelabase.localhost
CHELABASE_DOCS_HOST_NAME=docs.feat-x.chelabase.localhost
CHELABASE_LANDING_HOST_NAME=landing.feat-x.chelabase.localhost
CHELABASE_GATEWAY_LISTEN=0.0.0.0:8137
CHELABASE_PUBLIC_PORT=8137
CHELABASE_PUBLIC_SECURE=false
CHELABASE_ZITADEL_TLS_MODE=disabled
CHELABASE_CONTROL_PLANE_URI=http://orchestrator:8083
CHELABASE_COMPUTE_NETWORK=chelabase-feat-x_default
CHELABASE_ZITADEL_ACTIONS_ENDPOINT=http://orchestrator:8083/internal/zitadel/events
CHELABASE_NEXTEST_BIN=/usr/local/bin/cargo-nextest
```

`scripts/tests/workbench-compose.sh` (mode 755):

```bash
#!/usr/bin/env bash
# Offline check of the workbench Compose shape (W2 spec section 2): one stack's rendered config,
# from a fixture stack.env (stack feat-x, gateway 8137, proxy 4537).
set -euo pipefail
root="$(cd "$(dirname "$0")/../.." && pwd)"
fixture="$root/scripts/tests/fixtures/workbench-stack.env"
fail() {
  echo "workbench-compose: $*" >&2
  exit 1
}
config="$(env -u NEON_IMAGE -u COMPUTE_NODE_IMAGE docker compose -p chelabase-feat-x \
  -f "$root/infra/compose/docker-compose.yml" --env-file "$fixture" \
  --profile sites --profile runner config --format json)" || fail "compose config failed"
q() { jq -r "$1" <<<"$config"; }

[[ "$(q .name)" == chelabase-feat-x ]] || fail "the project name comes from -p"

published="$(q '[.services | to_entries[] | select(.value.ports) | .key as $s
  | .value.ports[] | "\($s) \(.host_ip) \(.published) \(.target)"] | sort | .[]')"
expected="$(printf '%s\n' "gateway 127.0.0.1 8137 8137" "gateway ::1 8137 8137" \
  "proxy 127.0.0.1 4537 4432" "proxy ::1 4537 4432" | sort)"
[[ "$published" == "$expected" ]] || fail "published ports must be the gateway's and the proxy's Postgres port only; got:
$published"

! grep -q 'host.docker.internal' <<<"$config" || fail "host.docker.internal is gone"
! grep -q 'host-gateway' <<<"$config" || fail "no extra_hosts host-gateway"

for name in studio api auth mail docs landing; do
  q '.services.gateway.networks.default.aliases[]?' | grep -qx "$name.feat-x.chelabase.localhost" ||
    fail "the gateway answers $name.feat-x.chelabase.localhost inside the network"
done
[[ "$(q '.services.gateway.environment.CHELABASE_GATEWAY_LISTEN // empty')" == "" ||
  "$(q '.services.gateway.environment.CHELABASE_GATEWAY_LISTEN')" == "0.0.0.0:8137" ]] ||
  fail "the gateway listens on its published port inside"

q '.services["storage-controller"].command[]' | grep -qx -- '--control-plane-url=http://orchestrator:8083/storcon' ||
  fail "storcon's hook calls the orchestrator by service name"
q '.services.proxy.command[]' | grep -qx -- '--auth-endpoint=http://orchestrator:8083/proxy/api/v1' ||
  fail "the proxy's auth endpoint is the orchestrator by service name"

for s in orchestrator worker gateway auth rest studio docs landing runner; do
  [[ "$(q ".services[\"$s\"].image // empty")" == chelabase-workbench:fixture000000 ]] || fail "$s runs the workbench image"
  [[ "$(q ".services[\"$s\"].user // empty")" == 1000:1000 ]] || fail "$s runs as the host user"
  [[ "$(q ".services[\"$s\"].working_dir // empty")" == /w/feat-x ]] || fail "$s works in the checkout"
done
for s in docs landing; do
  [[ "$(q ".services[\"$s\"].profiles // [] | index(\"sites\")")" != null ]] || fail "$s is in the sites profile"
done
[[ "$(q '.services.runner.profiles // [] | index("runner")')" != null ]] || fail "the runner is in the runner profile"
for s in orchestrator worker gateway auth rest studio; do
  [[ "$(q ".services[\"$s\"].profiles // [] | length")" == 0 ]] || fail "$s starts by default"
done

for s in orchestrator worker gateway auth rest; do
  [[ "$(q ".services[\"$s\"].command | if type == \"array\" then .[0] else . end")" == "/w/feat-x/target/debug/$s" ]] ||
    fail "$s runs the checkout's debug binary"
  [[ "$(q ".services[\"$s\"].volumes[] | select(.source == \"/w/feat-x\") | .read_only // false")" == true ]] ||
    fail "$s mounts the checkout read-only"
done
[[ "$(q '.services.studio.volumes[] | select(.source == "/w/feat-x") | .read_only // false')" == false ]] ||
  fail "studio mounts the checkout read-write (Vite's cache)"

for s in orchestrator worker runner; do
  q ".services[\"$s\"].volumes[] | select(.target == \"/var/run/docker.sock\") | .source" | grep -qx /var/run/docker.sock ||
    fail "$s gets the Docker socket"
  q ".services[\"$s\"].group_add[]?" | grep -qx 973 || fail "$s joins the docker group"
done
for s in gateway auth rest studio zitadel mailpit metadata-db; do
  [[ -z "$(q ".services[\"$s\"].volumes[]? | select(.source == \"/var/run/docker.sock\") | .source")" ]] ||
    fail "$s must not get the Docker socket"
done

for s in orchestrator worker gateway auth rest runner; do
  for k in secrets gateway auth neon; do
    [[ "$(q ".services[\"$s\"].volumes[] | select(.source == \"/m/.chelabase/$k\") | \"\(.target) \(.read_only // false)\"")" == "/m/.chelabase/$k true" ]] ||
      fail "$s mounts the $k keys read-only at their own path"
  done
  [[ "$(q ".services[\"$s\"].volumes[] | select(.source == \"/m/.chelabase/stacks/feat-x\") | \"\(.target) \(.read_only // false)\"")" == "/m/.chelabase/stacks/feat-x false" ]] ||
    fail "$s mounts the state dir read-write at its own path"
done

[[ "$(q '.services.zitadel.volumes[] | select(.target == "/zitadel-bootstrap") | .source')" == /m/.chelabase/stacks/feat-x/zitadel ]] ||
  fail "zitadel's PATs go to the stack's state dir"
[[ "$(q '.services.zitadel.environment.ZITADEL_EXTERNALDOMAIN')" == auth.feat-x.chelabase.localhost ]] ||
  fail "zitadel's domain is the stack's auth host"
[[ "$(q '.services.zitadel.environment.ZITADEL_EXTERNALPORT')" == 8137 ]] ||
  fail "zitadel's port is the stack's gateway port"
[[ "$(q '.services.zitadel.environment.ZITADEL_OIDC_DEFAULTLOGINURLV2')" == "http://studio.feat-x.chelabase.localhost:8137/login?authRequest=" ]] ||
  fail "zitadel sends sign-ins to the stack's Studio"
for s in zitadel mailpit; do
  [[ -n "$(q ".services[\"$s\"].healthcheck.test // empty")" ]] || fail "$s has a healthcheck"
done
for s in orchestrator worker gateway auth rest; do
  [[ -n "$(q ".services[\"$s\"].healthcheck.test // empty")" ]] || fail "$s has a healthcheck"
done

for s in orchestrator worker gateway auth rest runner; do
  q ".services[\"$s\"].env_file[]? | if type == \"object\" then .path else . end" | grep -qx /m/.chelabase/stacks/feat-x/env/stack.env ||
    fail "$s reads the stack env"
done
q '.services.runner.volumes[] | select(.target == "/usr/local/bin/cargo-nextest") | .source' | grep -qx /usr/local/bin/cargo-nextest ||
  fail "the runner gets the host's cargo-nextest"

df="$root/infra/workbench/Dockerfile"
[[ -f "$df" ]] || fail "infra/workbench/Dockerfile exists"
grep -Eq '^FROM ubuntu:26\.04@sha256:[0-9a-f]{64}' "$df" || fail "the image starts from ubuntu:26.04 by digest"
grep -q '24\.21\.0' "$df" || fail "node 24.21.0"
grep -q 'SHASUMS256' "$df" || fail "node's tarball is verified"
grep -q 'pnpm@12\.6\.0' "$df" || fail "pnpm 12.6.0 through corepack"
for tool in ca-certificates curl jq postgresql-client docker-ce-cli; do
  grep -q "$tool" "$df" || fail "the image has $tool"
done
echo "workbench-compose: ok"
```

**Shared with T3:** `render_stack_env` also emits `CHELABASE_{STUDIO,API,MAIL,DOCS,LANDING}_HOST_NAME` (host without port), after `CHELABASE_AUTH_HOST_NAME`; T3's RED test pins them.

`infra/neon/tests/run.sh`: the block from `if command -v docker >/dev/null; then` through the proxy-token check is replaced by (the static checks above it stay):

```bash
if command -v docker >/dev/null; then
  # A parametrized project (W2): the checks render one stack from the workbench fixture.
  fixture="$root/scripts/tests/fixtures/workbench-stack.env"
  compose=(env -u NEON_IMAGE -u COMPUTE_NODE_IMAGE docker compose -p chelabase-feat-x -f "$compose_file" --env-file "$fixture")
  services() { "${compose[@]}" "$@" config --services | sort | tr '\n' ' '; }
  check "the stack has every service (infra, app tier, no hook relay)" \
    test "$(services)" = "auth create-bucket gateway mailpit metadata-db nats orchestrator pageserver proxy proxy-tls register-safekeepers rest safekeeper1 seaweedfs storage-broker storage-controller storcon-db studio valkey worker zitadel zitadel-db "
  check "the project name comes from -p" \
    test "$("${compose[@]}" config --format json | jq -r '.name')" = "chelabase-feat-x"
  check "the compose file names no project" bash -c '! grep -qE "^name:" "$1"' _ "$compose_file"
  neon_json="$("${compose[@]}" config --format json)"
  published="$(jq <<<"$neon_json" -r '
    [.services | to_entries[]
      | select(.key | IN("seaweedfs","storage-broker","safekeeper1","storcon-db","storage-controller","pageserver","proxy"))
      | .value.ports[]?
      | "\(.host_ip):\(.published)"] | sort | join(" ")')"
  check "neon publishes only the proxy's Postgres port, on loopback" \
    test "$published" = "127.0.0.1:4537 ::1:4537"
  zitadel="$(jq -r '.services.zitadel' <<<"$neon_json")"
  check "zitadel is v4.19.1, pinned by digest" \
    grep -Eq '"ghcr\.io/zitadel/zitadel:v4\.19\.1@sha256:[0-9a-f]{64}"' <<<"$zitadel"
  check "zitadel sends sign-ins to Studio's /login" \
    test "$(jq -r '.environment.ZITADEL_OIDC_DEFAULTLOGINURLV2' <<<"$zitadel")" = "http://studio.feat-x.chelabase.localhost:8137/login?authRequest="
  check "zitadel writes the login-client token" \
    test "$(jq -r '.environment.ZITADEL_FIRSTINSTANCE_LOGINCLIENTPATPATH' <<<"$zitadel")" = "/zitadel-bootstrap/login-client.pat"
  token="$(CHELABASE_STORCON_HOOK_TOKEN=from-env "${compose[@]}" config --format json |
    jq -r '.services["storage-controller"].command[]' | grep -- '--control-plane-jwt-token=' || true)"
  check "the hook token comes from CHELABASE_STORCON_HOOK_TOKEN" \
    test "$token" = "--control-plane-jwt-token=from-env"
  storcon="$(jq -r '.services["storage-controller"]' <<<"$neon_json")"
  check "the storage controller calls the orchestrator's internal listener" \
    grep -qx -- '--control-plane-url=http://orchestrator:8083/storcon' <(jq -r '.command[]' <<<"$storcon")
  proxy="$(jq -r '.services.proxy' <<<"$neon_json")"
  proxy_args="$(jq -r '.command[]' <<<"$proxy")"
  check "the proxy asks the orchestrator's internal listener" \
    grep -qx -- '--auth-endpoint=http://orchestrator:8083/proxy/api/v1' <<<"$proxy_args"
  check "the proxy serves Postgres wire and serverless" \
    bash -c "grep -qx -- '--proxy=0.0.0.0:4432' <<<\"\$1\" && grep -qx -- '--wss=0.0.0.0:4444' <<<\"\$1\"" _ "$proxy_args"
  check "the proxy caches role secrets for at most a minute" \
    grep -qx -- '--project-info-cache=size=10000,ttl=1m,max_roles=64,gc_interval=60m' <<<"$proxy_args"
  check "the proxy uses Valkey (cache clearing, query cancel)" \
    bash -c "grep -qx -- '--redis-auth-type=plain' <<<\"\$1\" && grep -qx -- '--redis-plain=redis://valkey:6379' <<<\"\$1\"" _ "$proxy_args"
  valkey="$(jq -r '.services.valkey' <<<"$neon_json")"
  check "valkey is 9.1.2, pinned by digest" \
    grep -Eq '"valkey/valkey:9\.1\.2-alpine@sha256:[0-9a-f]{64}"' <<<"$valkey"
  check "valkey is not published" \
    test "$(jq -r '.ports // [] | length' <<<"$valkey")" = "0"
  check "the proxy waits for a healthy valkey" \
    test "$(jq -r '.depends_on.valkey.condition' <<<"$proxy")" = "service_healthy"
  check "valkey's healthcheck has an explicit timeout" \
    test "$(jq -r '.healthcheck.timeout // empty' <<<"$valkey")" != ""
```

(The two existing checks after this block, "the proxy waits for its TLS certificate" and "the proxy's token comes from CHELABASE_PROXY_API_TOKEN", stay; the coordinator points their `compose` use at the same array.)

**Gotchas:** `docker compose config` normalizes `ports` to objects with `host_ip`, `published` (a string) and `target`; IPv6 host IPs print without brackets. Compose interpolates `${VAR}` in `command` and volume paths; a list-form `command` keeps each item. `env_file` entries print as objects with `path` and `required`. The proxy's HTTP port 4444 stays inside. Docs and landing Vite servers today pass `--port … --strictPort` in `package.json`; their binds come from the new env settings.

**Verify:** `scripts/tests/workbench-compose.sh`; `infra/neon/tests/run.sh`; `docker build -f infra/workbench/Dockerfile infra/workbench -t chelabase-workbench:t4check` (builds the image once; this is the task's only build and is allowed); `docker run --rm chelabase-workbench:t4check sh -c 'node --version && pnpm --version && psql --version && docker --version && jq --version'`; `pnpm --filter @chelabase/studio typecheck`.

---

## T2: Rust live tests on service names (sonnet / sonnet)

**Files:** every file `scripts/tests/stack-urls.sh` scans (`services/*/tests/**`, `crates/*/tests/**`, `tools/*/tests/**`, `services/rest/src/conn.rs`'s tests); `scripts/tests/stack-urls.sh` (coordinator); one line in `justfile`'s `check` (after `workbench-compose.sh`).

**Behaviour:** spec section 5. Fallbacks: metadata `postgres://chelabase:chelabase@metadata-db:5432/chelabase_metadata` (and `sql_schema_live.rs`'s `BASE` reads `CHELABASE_METADATA_DATABASE_URL`'s base, else `postgres://chelabase:chelabase@metadata-db:5432`), storcon `http://storage-controller:1234`, valkey `redis://valkey:6379`, internal `orchestrator:8083` / `http://orchestrator:8083`, orchestrator `orchestrator:8081`, NATS `nats://nats:4222`. Every test that connects to the proxy by `<ep>.<CHELABASE_PROXY_DOMAIN>` adds `hostaddr=<CHELABASE_PROXY_HOSTADDR>` to its connection when that variable is set (tokio-postgres `Config::hostaddr`, or the conninfo key). Doc comments that say how to run a test say `just workbench test -p <pkg> --test <file> -- --run-ignored only` instead of `just dev`, `just live` or `just deps-up`.

**RED tests (coordinator):** `scripts/tests/stack-urls.sh` (mode 755):

```bash
#!/usr/bin/env bash
# W2 spec section 5: Rust live tests reach their stack by service name (they run in the stack's
# runner), and every test that dials the proxy by its domain can be pointed at the proxy's address.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"
mapfile -t files < <(git ls-files 'services/*/tests/*.rs' 'services/*/tests/**/*.rs' \
  'crates/*/tests/*.rs' 'crates/*/tests/**/*.rs' 'tools/*/tests/*.rs' 'tools/*/tests/**/*.rs' \
  services/rest/src/conn.rs)
bad=0
for pat in '127\.0\.0\.1:5433' '127\.0\.0\.1:1234' '127\.0\.0\.1:6379' '127\.0\.0\.1:8081' \
  '127\.0\.0\.1:8083' '127\.0\.0\.1:4222' '127\.0\.0\.1:7676' 'host\.docker\.internal' \
  'just dev\b' 'just live\b' 'just deps-up' 'internal-addr\.sh'; do
  if hits="$(grep -nE "$pat" "${files[@]}")"; then
    echo "stack-urls: $pat" >&2
    echo "$hits" >&2
    bad=1
  fi
done
while IFS= read -r f; do
  grep -q 'CHELABASE_PROXY_HOSTADDR' "$f" ||
    { echo "stack-urls: $f reads CHELABASE_PROXY_DOMAIN but not CHELABASE_PROXY_HOSTADDR" >&2; bad=1; }
done < <(grep -lE 'CHELABASE_PROXY_DOMAIN' "${files[@]}" || true)
grep -qE 'metadata-db:5432' "${files[@]}" ||
  { echo "stack-urls: no live test falls back to metadata-db:5432" >&2; bad=1; }
[[ $bad -eq 0 ]] && echo "stack-urls: ok"
exit $bad
```

**Gotchas:** `grep -E` has no `\b` in every implementation; GNU grep (this machine) supports it. `services/rest/src/conn.rs` is production code with a live test inside: change only the test's fallback. Don't touch `docker_backend_live.rs` beyond what T1 did unless the guard flags it.

**Verify:** `scripts/tests/stack-urls.sh`; `cargo nextest run --workspace --no-run` (compiles every test); `cargo clippy --workspace --all-targets -- -D warnings`.

---

## T5: CLI workbench commands (opus / opus)

**Files:** `tools/cli/src/main.rs`, `tools/cli/src/workbench/{mod.rs,commands.rs,migrate.rs}`, `tools/cli/src/paths.rs`, `tools/cli/src/dev/{bootstrap.rs,steps.rs,tenant.rs}`, `tools/cli/Cargo.toml`. Coordinator: tests in `main.rs`, `workbench/lifecycle_tests.rs` (declared `#[cfg(test)] mod lifecycle_tests;` in `mod.rs`), `paths.rs`, `dev/steps.rs`, `dev/tenant.rs`.

**Behaviour:** spec sections 4 and 6, the pinned CLI surface, P5–P7.
- Resolve the checkout: `git rev-parse --show-toplevel` from the cwd; `<main>` from `--git-common-dir`; slug `main` when checkout == main, else `slug_for_dir(<checkout's dir name>)` (a checkout outside `<main>/.worktrees/` is an error naming the path). State dir `<main>/.chelabase/stacks/<slug>`.
- `up`: on main run `migrate_main_layout` first (print each action), stop legacy dev processes listed in `.chelabase/dev/pids` (dev-down's command-line check), remove legacy computes (`chelabase-compute-*` without a stack label; via `docker ps`/`docker rm -f`, the CLI shells out to `docker` like `tenant.rs`) **before any app container starts** (T1 review: main's worker no longer sees unlabelled computes and would start a second compute on the same timeline). Then: `example_scheme` on `.env.example` (refuse a bad scheme), registry `ensure` (probe `port_is_free`), `acquire_slot` (alive = checkout exists **and** (`now - running_since < 15 min` **or** `docker compose -p <project> ps -q --status running` is non-empty): the grace covers the image and cargo builds before any container runs (T3 review I2); report to stderr), `status` lists the queue with `slots::queued`, `purge` calls `slots::drop_tickets`, image (`docker image inspect chelabase-workbench:<hash>` else `docker build -t … -f <checkout>/infra/workbench/Dockerfile <checkout>/infra/workbench`), images (`scripts/neon-images.sh` output), render `stack.env` (from `<checkout>/infra/compose/.env.example`, `uid`/`gid` from `id -u`/`id -g`, docker gid from the socket's group via `std::os::unix::fs::MetadataExt::gid` on `/var/run/docker.sock`), `CHELABASE_NEXTEST_BIN` in `extra` (the host's `cargo-nextest` from `PATH`), `cargo build -p orchestrator -p worker -p gateway -p rest -p auth -p chelabase` in the checkout, the key steps (the existing `dev::bootstrap` keys-first functions, run with the stack env), `compose up -d --wait` of every infra service plus `zitadel`, `mailpit` and `gateway`, wait for `<state>/zitadel/{admin,login-client}.pat`, bootstrap (`run_bootstrap` with the stack env and P6), `compose up -d --wait orchestrator worker auth rest studio` (plus `docs landing` with `--profile sites` when asked), bootstrap again (as `just dev` did: the Actions target after the orchestrator), copy `proxy:/tls/ca.crt` to `<state>/neon/proxy-ca.crt` (`docker compose cp`), record binary mtimes in `<state>/binaries.json`, print the URLs (Studio, API, auth, mail, Postgres `localhost:<pg>`). Any failure after taking the slot leaves the slot held (the stack is partly up; `down` releases it) and says so.
- **The runner's curl (T4 review I1):** curl 8.18 resolves every `*.localhost` name to loopback itself and never asks Docker's DNS, so the gateway aliases don't reach curl. `up` writes `<state>/runner/.curlrc` from `commands::curlrc(&hosts, gateway_port)`: one line `connect-to = "<name>:<gw>:gateway:<gw>"` per host name; the runner gets `CURL_HOME=<state>/runner`. Rust, Node and Go clients use the aliases as designed.
- **Recreate, don't restart (T4 review M3):** `env_file` is read when a container is created, so after the second bootstrap (new `actions.env`/`demo.env`) and in `restart`, apps are brought up with `compose up -d --force-recreate --no-deps <apps>`, never `compose restart`.
- **`test` runs an archive only (T4 review M2):** the runner has no `cargo`; `test` always archives on the host and runs `cargo-nextest nextest run --archive-file …` in the runner.
- `restart`, `down`, `purge`, `status`, `logs`, `psql`, `exec`, `test`, `run`, `env`: as the spec's table and the pinned runner env. `down`/`purge` with `--stack` look the entry up in the registry (unknown slug → error listing the known ones); `purge` without `--yes` on `main` refuses. `test`/`run` call `up` when the stack isn't running, else `cargo build` the apps and restart `stale_apps`. Runner: `compose run --rm --no-deps -e K=V… --profile runner runner <cmd>` (the proxy IP from `docker inspect <project>-proxy-1` on `<project>_default`).
- `paths.rs`: `studio_env_file()` = `studio_env_file_in(&repo_root(), env CHELABASE_STACK_DIR)`.
- `dev/steps.rs`: `ensure_actions_target(api, env_file, endpoint)`: a listed target whose `endpoint` differs is updated with `POST /v2/actions/targets/{id}` and body `{"endpoint": <endpoint>}` (keeping its key file); `actions_endpoint_from(v)` (blank or unset → `http://orchestrator:8083/internal/zitadel/events`); `bootstrap.rs` passes `actions_endpoint_from(env CHELABASE_ZITADEL_ACTIONS_ENDPOINT)`.
- `dev/tenant.rs`: `compose_base(project, file, env)`; `bind_demo_tenant` uses it with `CHELABASE_COMPOSE_PROJECT` from the env.

**RED tests (coordinator):**

`tools/cli/src/main.rs` `mod tests` (append):

```rust
    use crate::workbench::commands::Site;

    fn workbench(args: &[&str]) -> WorkbenchCommands {
        let mut argv = vec!["chelabase", "workbench"];
        argv.extend_from_slice(args);
        match Cli::try_parse_from(argv).unwrap().command {
            Commands::Workbench { command } => command,
            other => panic!("not a workbench command: {other:?}"),
        }
    }

    #[test]
    fn workbench_up_takes_sites() {
        let WorkbenchCommands::Up { with } = workbench(&["up", "--with", "docs,landing"]) else {
            panic!()
        };
        assert_eq!(with, vec![Site::Docs, Site::Landing]);
        let WorkbenchCommands::Up { with } = workbench(&["up"]) else { panic!() };
        assert!(with.is_empty());
        assert!(Cli::try_parse_from(["chelabase", "workbench", "up", "--with", "blog"]).is_err());
    }

    #[test]
    fn workbench_test_splits_selection_and_run_args() {
        let WorkbenchCommands::Test { archive, run } = workbench(&[
            "test", "-p", "rest", "--test", "rest_live", "--", "--run-ignored", "only",
        ]) else {
            panic!()
        };
        assert_eq!(archive, ["-p", "rest", "--test", "rest_live"]);
        assert_eq!(run, ["--run-ignored", "only"]);
        let WorkbenchCommands::Test { archive, run } = workbench(&["test"]) else { panic!() };
        assert!(archive.is_empty() && run.is_empty());
    }

    #[test]
    fn workbench_run_takes_env_then_the_script_and_its_args() {
        let WorkbenchCommands::Run { env, script, args } = workbench(&[
            "run", "--env", "A=1", "--env", "B=2", "scripts/neon-e2e.sh", "--fast", "x",
        ]) else {
            panic!()
        };
        assert_eq!(env, ["A=1", "B=2"]);
        assert_eq!(script, "scripts/neon-e2e.sh");
        assert_eq!(args, ["--fast", "x"]);
    }

    #[test]
    fn workbench_exec_passes_the_command_through() {
        let WorkbenchCommands::Exec { service, cmd } =
            workbench(&["exec", "metadata-db", "psql", "-U", "chelabase"])
        else {
            panic!()
        };
        assert_eq!(service, "metadata-db");
        assert_eq!(cmd, ["psql", "-U", "chelabase"]);
    }

    #[test]
    fn workbench_stack_commands() {
        let WorkbenchCommands::Purge { stack, yes } =
            workbench(&["purge", "--stack", "feat-x", "--yes"])
        else {
            panic!()
        };
        assert_eq!(stack.as_deref(), Some("feat-x"));
        assert!(yes);
        assert!(matches!(workbench(&["down"]), WorkbenchCommands::Down { stack: None }));
        let WorkbenchCommands::Restart { apps } = workbench(&["restart", "worker", "gateway"]) else {
            panic!()
        };
        assert_eq!(apps, ["worker", "gateway"]);
        let WorkbenchCommands::Logs { follow, services } = workbench(&["logs", "-f", "worker"]) else {
            panic!()
        };
        assert!(follow);
        assert_eq!(services, ["worker"]);
        assert!(matches!(workbench(&["status"]), WorkbenchCommands::Status));
        assert!(matches!(workbench(&["env"]), WorkbenchCommands::Env));
        let WorkbenchCommands::Psql { db } = workbench(&["psql"]) else { panic!() };
        assert_eq!(db, None);
    }
```

`tools/cli/src/workbench/lifecycle_tests.rs`:

```rust
//! W2 spec sections 4 and 6: main's one-time move, and which apps restart.

use super::commands::{curlrc, stale_apps};
use super::migrate::migrate_main_layout;
use super::slug::hosts;
use std::collections::HashMap;
use std::path::Path;

fn touch(p: &Path, s: &str) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, s).unwrap();
}

fn read(p: &Path) -> String {
    std::fs::read_to_string(p).unwrap()
}

#[test]
fn the_old_layout_moves_into_main_stack_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let m = tmp.path();
    touch(&m.join(".chelabase/zitadel/admin.pat"), "pat");
    touch(&m.join(".chelabase/zitadel/demo.env"), "X=1");
    touch(&m.join(".chelabase/neon/proxy-ca.crt"), "ca");
    touch(&m.join(".chelabase/neon/compute-jwt.key"), "k");
    touch(&m.join(".chelabase/secrets/secrets.key"), "s");
    touch(&m.join(".chelabase/dev/pids"), "");
    touch(&m.join(".chelabase/dev/orchestrator.log"), "log");
    touch(&m.join(".chelabase/live/lock"), "");
    touch(&m.join("apps/studio/.env.local"), "VITE_API_URL=x");
    let linked = m.join(".worktrees/feat-x/apps/studio");
    std::fs::create_dir_all(&linked).unwrap();
    std::os::unix::fs::symlink(m.join("apps/studio/.env.local"), linked.join(".env.local")).unwrap();
    touch(&m.join(".worktrees/other/apps/studio/.env.local"), "a real file");

    let done = migrate_main_layout(m).unwrap();
    assert!(!done.is_empty(), "it says what it did");

    let s = m.join(".chelabase/stacks/main");
    assert_eq!(read(&s.join("zitadel/admin.pat")), "pat");
    assert_eq!(read(&s.join("zitadel/demo.env")), "X=1");
    assert_eq!(read(&s.join("neon/proxy-ca.crt")), "ca");
    assert!(!m.join(".chelabase/zitadel").exists());
    assert!(!m.join(".chelabase/neon/proxy-ca.crt").exists());
    assert_eq!(read(&m.join(".chelabase/neon/compute-jwt.key")), "k", "machine keys stay");
    assert_eq!(read(&m.join(".chelabase/secrets/secrets.key")), "s", "machine keys stay");
    assert!(!m.join(".chelabase/dev").exists());
    assert!(!m.join(".chelabase/live").exists());
    assert!(!m.join("apps/studio/.env.local").exists());
    assert!(
        std::fs::symlink_metadata(linked.join(".env.local")).is_err(),
        "the worktree's link to it is gone"
    );
    assert_eq!(
        read(&m.join(".worktrees/other/apps/studio/.env.local")),
        "a real file",
        "a real file is not ours to delete"
    );
}

#[test]
fn a_second_run_does_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let m = tmp.path();
    touch(&m.join(".chelabase/zitadel/admin.pat"), "pat");
    migrate_main_layout(m).unwrap();
    assert!(migrate_main_layout(m).unwrap().is_empty());
    assert_eq!(read(&m.join(".chelabase/stacks/main/zitadel/admin.pat")), "pat");
}

#[test]
fn a_fresh_checkout_has_nothing_to_move() {
    let tmp = tempfile::tempdir().unwrap();
    assert!(migrate_main_layout(tmp.path()).unwrap().is_empty());
}

#[test]
fn both_layouts_present_is_an_error_and_nothing_moves() {
    let tmp = tempfile::tempdir().unwrap();
    let m = tmp.path();
    touch(&m.join(".chelabase/zitadel/admin.pat"), "old");
    touch(&m.join(".chelabase/stacks/main/zitadel/admin.pat"), "new");
    let err = migrate_main_layout(m).unwrap_err().to_string();
    assert!(err.contains(".chelabase/zitadel"), "{err}");
    assert_eq!(read(&m.join(".chelabase/zitadel/admin.pat")), "old");
    assert_eq!(read(&m.join(".chelabase/stacks/main/zitadel/admin.pat")), "new");
}

#[test]
fn only_apps_whose_binary_changed_restart() {
    let recorded = HashMap::from([("worker".to_string(), 1u64), ("gateway".to_string(), 2)]);
    let current = HashMap::from([
        ("worker".to_string(), 1u64),
        ("gateway".to_string(), 3),
        ("rest".to_string(), 5),
    ]);
    assert_eq!(stale_apps(&recorded, &current), ["gateway", "rest"]);
    assert!(stale_apps(&current, &current).is_empty());
}

#[test]
fn the_runners_curl_reaches_the_gateway_for_every_stack_host() {
    let h = hosts("feat-x", 8137);
    let rc = curlrc(&h, 8137);
    for name in h.names() {
        assert!(
            rc.lines().any(|l| l == format!("connect-to = \"{name}:8137:gateway:8137\"")),
            "{name}: {rc}"
        );
    }
    assert_eq!(rc.lines().filter(|l| l.starts_with("connect-to")).count(), 6);
}
```

`tools/cli/src/paths.rs` `mod tests` (append):

```rust
    #[test]
    fn studio_env_goes_to_the_stack_dir_in_a_workbench() {
        use std::path::Path;
        assert_eq!(
            studio_env_file_in(Path::new("/w"), Some(Path::new("/s"))),
            PathBuf::from("/s/studio.env")
        );
        assert_eq!(
            studio_env_file_in(Path::new("/w"), None),
            PathBuf::from("/w/apps/studio/.env.local")
        );
    }
```

`tools/cli/src/dev/steps.rs` `mod tests`: add `const ENDPOINT: &str = "http://orchestrator:8083/internal/zitadel/events";`; `target_listing` becomes

```rust
    fn target_listing(ids: &[&str]) -> Value {
        json!({"targets": ids.iter().map(|id| json!({"id": id, "name": "chelabase-app-users", "endpoint": ENDPOINT})).collect::<Vec<_>>()})
    }
```

every `ensure_actions_target(&fake, &file)` becomes `ensure_actions_target(&fake, &file, ENDPOINT)`, the endpoint assertion in `actions_target_created_and_key_written` becomes `assert_eq!(body["endpoint"], ENDPOINT);`, and append:

```rust
    #[tokio::test]
    async fn actions_target_moved_to_a_new_endpoint_keeps_its_key() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("actions.env");
        std::fs::write(&file, "CHELABASE_ZITADEL_ACTIONS_SIGNING_KEY=old\n").unwrap();
        let fake = Fake::default()
            .with(
                TARGETS_SEARCH,
                json!({"targets": [{"id": "t-old", "name": "chelabase-app-users",
                    "endpoint": "http://host.docker.internal:8083/internal/zitadel/events"}]}),
            )
            .with("POST /v2/actions/targets/t-old", json!({}))
            .with(EXECUTIONS, json!({}));
        ensure_actions_target(&fake, &file, ENDPOINT).await.unwrap();
        assert_eq!(
            fake.calls(),
            vec![TARGETS_SEARCH, "POST /v2/actions/targets/t-old", EXECUTIONS]
        );
        assert_eq!(fake.bodies("POST /v2/actions/targets/t-old")[0]["endpoint"], ENDPOINT);
        assert_eq!(fake.bodies(EXECUTIONS)[0]["targets"], json!(["t-old"]));
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "CHELABASE_ZITADEL_ACTIONS_SIGNING_KEY=old\n"
        );
    }

    #[test]
    fn the_actions_endpoint_comes_from_the_env() {
        assert_eq!(actions_endpoint_from(None), ENDPOINT);
        assert_eq!(actions_endpoint_from(Some("http://x:1/e")), "http://x:1/e");
        assert_eq!(actions_endpoint_from(Some("  ")), ENDPOINT);
    }
```

`tools/cli/src/dev/tenant.rs` `mod tests` (append; create the module if absent):

```rust
    #[test]
    fn tenant_bind_runs_in_the_stacks_project() {
        use std::path::Path;
        assert_eq!(
            compose_base(Some("chelabase-feat-x"), Path::new("/w/c.yml"), Path::new("/s/stack.env")),
            ["compose", "-p", "chelabase-feat-x", "-f", "/w/c.yml", "--env-file", "/s/stack.env"]
        );
        assert_eq!(
            compose_base(None, Path::new("/w/c.yml"), Path::new("/e")),
            ["compose", "-f", "/w/c.yml", "--env-file", "/e"]
        );
    }
```

**Gotchas:** `up` runs long; print each phase on stderr with its duration. Never write into `<main>/.chelabase/{secrets,gateway,auth,neon}` except through the existing keys-first steps (which create missing keys only). The CLI is itself built by `up`'s `cargo build`; when run through `just workbench` it's the freshly built one only from the second call on, which is fine. `compose up --wait` fails on an unhealthy service: show `compose ps` and the last 50 log lines of the failing service. Nothing here may call `docker compose` without `-p`.

**Verify:** `cargo nextest run -p chelabase`; `cargo clippy -p chelabase --all-targets -- -D warnings`; `cargo run -q -p chelabase -- workbench --help`; `cargo run -q -p chelabase -- workbench status` (reads the registry; must work with none). No `up` in this task (the coordinator's live pass does it).

---

## T6: recipes and scripts (sonnet / sonnet)

**Files:** `justfile`, `scripts/**` except `scripts/tests/{stack-urls,workbench-compose}.sh` and fixtures, `scripts/sweep-dbs.sh` (new), `packages/client/scripts/e2e.mjs`, `packages/neon-e2e/check.mjs`. Coordinator: `scripts/tests/workbench-recipes.sh`, the deletions of `scripts/tests/{live,internal-addr,stack-reset-slots,dev-recipes}.sh`, the deletion of `infra/neon/tests/run.sh`'s blocks "CHELABASE_INTERNAL_ADDR validation", "justfile" and "one stack: …" through the end of the live checks (the summary lines at the file's end stay), and the `check` recipe's lines for the deleted tests.

**Behaviour:** spec sections 4 and 5, the pinned CLI surface.
- New recipe `workbench *args` → `cargo run -q -p chelabase -- workbench {{args}}`. Retired recipes (spec 4) are deleted. `create-db-demo`, `token`, `zitadel-bootstrap` and `zitadel-reset` load the stack env first (`set -a; . "$(cargo run -q -p chelabase -- workbench env)"; set +a`); `zitadel-reset.sh` uses `docker compose -p "$CHELABASE_COMPOSE_PROJECT" --env-file "$CHELABASE_STACK_DIR/env/stack.env"`, the volume `${CHELABASE_COMPOSE_PROJECT}_zitadel_pg_data` by exact name, and `$CHELABASE_ZITADEL_DIR`.
- Each e2e recipe (`neon-e2e`, `branch-e2e`, `storage-leak-e2e`, `app-users-e2e`, `data-e2e`, `console-e2e`, `sql-editor-e2e`, `merge-e2e`, `role-e2e`, `members-e2e`, `auth-e2e`, `neon-smoke`) is `cargo run -q -p chelabase -- workbench run scripts/<name>.sh`. `bench-data-api` builds `bench-data-api`'s `variants_live` test binary on the host (`cargo test -p bench-data-api --test variants_live --no-run --message-format=json`, the executable from the JSON) and passes it with `workbench run --env CHELABASE_BENCH_TEST_BIN=<path> scripts/bench-data-api.sh`. `sweep-dbs` is `workbench run scripts/sweep-dbs.sh`: it removes this stack's computes (by `chelabase.dev/stack=$CHELABASE_STACK`), deletes their storcon tenants and the `neon_endpoints`, `branches` and `databases` rows (today's recipe body, moved).
- `wt-add`: no `.env.local` link. `wt-remove`: `cargo run -q -p chelabase -- workbench purge --stack <slug> --yes` (run from the worktree, so the slug resolves; a missing stack isn't an error) before `git worktree remove`.
- Scripts run in the runner: `scripts/lib/neon-common.sh` drops `compose`, `check_internal_addr`, and uses `psql "$CHELABASE_METADATA_DATABASE_URL"` for `metadata_sql`, `"$CHELABASE_CLI" dev token` for the token, `STORCON_URL` default `http://storage-controller:1234`. The scripts: `"$CHELABASE_CLI"` for `cargo run -q -p chelabase --`; `hostaddr=${CHELABASE_PROXY_HOSTADDR}`; CA `$CHELABASE_PROXY_CA_FILE`; `demo.env` from `$CHELABASE_ZITADEL_DIR`; compute checks `docker ps -aq --filter label=chelabase.dev/stack=$CHELABASE_STACK --filter label=chelabase.dev/endpoint-id=$ep`; storage-leak `docker exec "$CHELABASE_COMPOSE_PROJECT-<service>-1"`; `neon-smoke.sh` defaults `http://storage-controller:1234` / `http://pageserver:9898`. `packages/neon-e2e/check.mjs` and `packages/client/scripts/e2e.mjs` take the proxy address from `CHELABASE_PROXY_HOSTADDR` when set (TLS servername stays the endpoint host).
- `check` recipe: drops the deleted tests' lines, adds `scripts/tests/workbench-recipes.sh`.

**RED tests (coordinator):** `scripts/tests/workbench-recipes.sh` (mode 755):

```bash
#!/usr/bin/env bash
# W2 spec sections 4 and 5: recipes and scripts run against the current checkout's stack through
# `chelabase workbench`; the one-stack machinery is gone.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"
fail() {
  echo "workbench-recipes: $*" >&2
  exit 1
}
recipes=" $(just --summary) "
for gone in dev dev-down stop deps-up deps-down deps-ps neon-ps orchestrator worker gateway rest \
  auth studio cleanup _cleanup live live-status live-test _live-check _live-swap-dev \
  stack-reset-slots neon-test-live worker-live docs landing; do
  [[ "$recipes" != *" $gone "* ]] || fail "recipe $gone is retired"
done
[[ "$recipes" == *" workbench "* ]] || fail "there is a workbench recipe"
just --show workbench | grep -q -- '-p chelabase -- workbench' || fail "just workbench forwards to the CLI"
for e2e in neon-e2e branch-e2e storage-leak-e2e app-users-e2e data-e2e console-e2e sql-editor-e2e \
  merge-e2e role-e2e members-e2e auth-e2e neon-smoke; do
  just --show "$e2e" | grep -q "workbench run scripts/$e2e.sh" || fail "$e2e runs in the stack's runner"
done
just --show bench-data-api | grep -q 'workbench run --env CHELABASE_BENCH_TEST_BIN=' ||
  fail "the benchmark passes its prebuilt test binary"
just --show sweep-dbs | grep -q 'workbench run scripts/sweep-dbs.sh' || fail "sweep-dbs runs in the runner"
just --show wt-remove | grep -q 'workbench purge --stack' || fail "wt-remove purges the worktree's stack"
! just --show wt-add | grep -q '\.env\.local' || fail "wt-add no longer links Studio's env"
for r in create-db-demo zitadel-reset; do
  just --show "$r" | grep -q 'workbench env' || fail "$r loads the stack's env"
done
for f in scripts/live.sh scripts/internal-addr.sh scripts/stack-reset-slots.sh \
  scripts/tests/live.sh scripts/tests/internal-addr.sh scripts/tests/stack-reset-slots.sh \
  scripts/tests/dev-recipes.sh; do
  [[ ! -e "$f" ]] || fail "$f is retired"
done
[[ -x scripts/sweep-dbs.sh ]] || fail "scripts/sweep-dbs.sh exists"

mapfile -t scripts < <(git ls-files 'scripts/*.sh' 'scripts/lib/*.sh')
for pat in 'compose exec' 'cargo run' 'cargo test' \
  '127\.0\.0\.1:(5433|1234|6379|8081|8083|4222|7676|9898)' 'host\.docker\.internal' \
  'internal-addr\.sh' 'name=\^chelabase-compute-' '\.chelabase/neon/proxy-ca\.crt' \
  '\.chelabase/zitadel' 'hostaddr=127\.0\.0\.1' 'just live' 'just dev\b' 'just deps-up'; do
  hits="$(grep -nE "$pat" "${scripts[@]}" || true)"
  [[ -z "$hits" ]] || fail "scripts still use $pat:
$hits"
done
for f in scripts/zitadel-reset.sh; do
  grep -q -- '-p "\$CHELABASE_COMPOSE_PROJECT"' "$f" || fail "$f acts on the stack's project"
done
for f in scripts/neon-e2e.sh scripts/branch-e2e.sh; do
  grep -q 'chelabase.dev/stack' "$f" || fail "$f selects computes by the stack's label"
done
grep -q 'CHELABASE_PROXY_HOSTADDR' scripts/neon-e2e.sh || fail "neon-e2e dials the proxy's address"
for f in packages/neon-e2e/check.mjs packages/client/scripts/e2e.mjs; do
  grep -q 'CHELABASE_PROXY_HOSTADDR' "$f" || fail "$f dials the proxy's address"
done
just --show check | grep -q 'scripts/tests/workbench-recipes.sh' || fail "just check runs this file"
echo "workbench-recipes: ok"
```

**Gotchas:** `just --summary` prints recipe names separated by spaces (private `_` recipes are omitted, which is fine). The scripts are run by the runner as the host uid with `HOME=/tmp`; temp files go under `mktemp -d`. `scripts/neon-images.sh` stays a host script (the CLI runs it). `entry.sh` still works inside the runner (the env already holds the hosts). `infra/neon/tests/run.sh`'s neon-smoke offline checks set `STORCON_URL` themselves; keep them passing.

**Verify:** `scripts/tests/workbench-recipes.sh`; `infra/neon/tests/run.sh`; `scripts/tests/entry-urls.sh`; `scripts/tests/entry-sh.sh`; `shellcheck scripts/*.sh scripts/lib/*.sh` (if installed); `just --summary`.

---

## T7: rules and docs (sonnet / no review of its own)

**Files:** `CLAUDE.md`, `docs/agent-preamble.md`, `docs/roadmap/2026-09-28-neon-and-data-api.md` (the "Working rules" section only), `CONTRIBUTING.md`, `infra/compose/.env.example` (comments only), `apps/docs` pages that name `just dev`, `just live` or the host ports (if any).

**Behaviour:** spec section 6's rules rewrite. `CLAUDE.md`'s last Git bullet becomes: one stack per checkout (`just workbench up|test|run|down`), at most 4 running (FIFO wait), a worktree's stack is purged by `just wt-remove`; the `.env.local` symlink sentence goes. The preamble's stack rule becomes: run your own worktree's stack with `just workbench …` (from your worktree, `just --justfile <wt>/justfile --working-directory <wt> workbench …`), never another checkout's; `just workbench down` when your task ends unless the brief says otherwise; live Rust tests through `just workbench test`, e2e through the e2e recipes; at most 3 builds or test suites at once still holds. The roadmap's "Live tests and e2e runs stay one at a time (one Compose stack per machine)" goes, replaced by the per-checkout rule. `CONTRIBUTING.md`: quickstart (`just workbench up`), recipe table, URL/port table (only the gateway and proxy ports are on the host), the worktrees section, the e2e note. No RED tests (docs only); `scripts/tests/workbench-recipes.sh` and `stack-urls.sh` already guard the scripts.

**Verify:** `scripts/tests/entry-urls.sh`; `grep -rnE 'just (dev|live|deps-up)\b' CLAUDE.md CONTRIBUTING.md docs/agent-preamble.md apps/docs/src` prints nothing.

---

## End of step

1. Whole-branch review (opus) on `main..workbench-isolation`.
2. Fix round: the coordinator writes a failing test for each finding first.
3. `just check`.
4. No fork change: no gate, no image build of the Neon images (list "pending fork changes: none" in the Status).
5. Live pass (coordinator), in this order:
   1. Main: `just workbench up` in the main checkout (migration, the `chelabase` project keeps its volumes); Studio sign-in at `studio.chelabase.localhost:8000` with the existing admin; an existing database still connects through `localhost:4432`.
   2. Branch: `just workbench up` in `.worktrees/workbench-isolation` (fresh, seeded; note the time); `just workbench status` lists both.
   3. On the branch stack: `just workbench test -- --run-ignored only` for the orchestrator, rest, worker and catalog live suites (as W1's live pass, one package at a time); `just neon-e2e`, `branch-e2e`, `role-e2e`, `merge-e2e`, `console-e2e`, `sql-editor-e2e`, `auth-e2e`, `members-e2e`, `app-users-e2e`, `data-e2e`.
   4. Isolation: `just neon-e2e` on main and on the branch at the same time; both pass; `docker ps --filter label=chelabase.dev/stack=…` shows each stack's computes untouched by the other's worker.
   5. Slots: with `CHELABASE_WORKBENCH_SLOTS=2` and main + branch running, a third worktree's `CHELABASE_WORKBENCH_WAIT=60 just workbench up` waits, prints status lines, and times out naming both; then `just wt-remove` of that worktree purges its stack (volumes and state dir gone, registry entry gone).
   6. Browser pass on the branch stack (`studio.workbench-isolation.chelabase.localhost:<port>`): sign-in through Zitadel, Zitadel's console, a Studio hot reload, Mailpit during a sign-up with verification on, docs and landing after `just workbench up --with docs,landing`.
6. Update `docs/landing-highlights.md` on `main`, the plan Status and the roadmap's "Resume here", then pause for the user.
