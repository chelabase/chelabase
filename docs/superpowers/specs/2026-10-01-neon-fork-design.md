# Hard fork of Neon (first spec)

**Date:** 2026-10-01
**Status:** Approved (amended).
**Amended by the plan's Status block** (`docs/superpowers/plans/2026-10-01-neon-fork.md`): image names `neon-storage` / `neon-compute-v17`, local dev-image publishing, no Neon regression gate, public packages restored by the user on 2026-10-03, platform-image check deferred.
**Roadmap:** `docs/roadmap/2026-09-28-neon-and-data-api.md`, "Hard fork of Neon", items 1–3 and the cloud-feature decisions.
**Branch:** `neon-fork` (worktree `.worktrees/neon-fork`), for the cheladb-v1 side. Most of the work happens in two new repos, `chelabase/neon` and `chelabase/postgres`.

## Goal

Neon upstream is frozen, so we own it: `chelabase/neon` and `chelabase/postgres`, with a test suite running in our CI before any real change, the Postgres security minors merged, published images in a slim dev variant and a full platform variant, and a decision for each of Neon's cloud features. cheladb-v1 then builds and runs from the fork.

**Done when:**
- Both repos exist (public, detached, full history). `chelabase/neon`'s PR CI is green on v17, and the nightly is green on v14–v16 plus a v17 debug build. Every test quarantined in `test_runner/known_failures.txt` has a reason.
- The compute runs **17.11** (16.15, 15.19 and 14.24 in the platform images), built from `REL_1x_STABLE_cheladb`.
- The fork's CI publishes the dev and platform images to public GHCR with SBOMs and provenance.
- `vm_monitor` and the VM specs are gone, and push-only feature flags work, with tests.
- cheladb-v1's `NEON_REF` points at `chelabase/neon`, and Compose pulls the digest-pinned dev images. The full verification run passes, including a database created on 17.5 starting on 17.11.

**Not in this spec:** Postgres 18 and 19 (their own spec, on a proven CI), the worker's flag push, the console redirect through Studio, and any engine rewrite.

## Decisions

These were made in brainstorming on 2026-10-01.

| Question | Decision | Why |
|---|---|---|
| Visibility | **Public** repos. cheladb-v1 stays private. | GitHub's free plan gives public repos unlimited 4-vCPU/16 GB runners. Private repos get 2,000 minutes on 2 vCPUs, which doesn't cover one full build plus the regression suite. Apache-2.0 and the PostgreSQL licence allow it. |
| Fork shape | Detached repos (not in GitHub's fork network) with mirrored history. Base `fa504217`, which is still upstream `main`'s HEAD. | PRs never default to targeting upstream, and blame and history are kept. |
| CI scope | Our own workflows. **PR:** lint, clippy, Rust tests, and a sharded v17 release-build regression run. **Nightly:** v14–v16 plus a v17 debug build. | Upstream's CI needs Neon's self-hosted runners (`large`, `large-metal`), AWS, Azure and a Hetzner cache. The full matrix on every PR would take hours. |
| Spec scope | Items 1–3 plus the cloud-feature decisions. PG 18 later. | PG 18's port (AIO against the smgr patches) is best designed on a proven CI. |
| Minors | **Our own `REL_1x_STABLE_cheladb` branches** from the pinned submodule commits. We merge `postgres/postgres`'s minor tags and cherry-pick only vetted, self-contained Neon fixes. | `neondatabase/postgres`'s branch heads (17.8 etc.) carry hundreds of commits that need `neon`-extension changes that were never published (see Findings). |
| v14 | Merge 14.24 now and 14.25 (2026-11-12), then **freeze**: it stays in the platform images and the nightly, but can't be chosen for new databases. | It honours "keep every major" without promising security fixes that won't exist after end of life. |
| Release watch | A daily workflow in `chelabase/postgres` opens one de-duplicated issue per new minor. | It's free on a public repo, nobody has to remember, and the issue is the to-do item. |
| Images | The fork's CI publishes to public GHCR. cheladb-v1 pins by digest, with the dev variant as Compose's default. `just neon-build` stays as the offline fallback. | Nobody waits 40 minutes on a laptop, and published artifacts are reproducible. |
| Dev compute extensions | Upstream's `minimal` as is (pgrag, timescaledb, pg_cron, pg_partman). | The user's choice. |
| Auth broker, `local_proxy`, `pg_session_jwt` | Keep in source, off by default. `pg_session_jwt` ships in the platform images. | The data API (step 6) doesn't use them, and keeping them costs no merge work. |
| Feature flags | **Push-only:** a fork patch lets `POST /v1/feature_flag_spec` work without PostHog. The cheladb-v1 worker pushes the spec (later step). | No outbound PostHog calls, and the evaluator stays unchanged. |
| Code changes in this step | Remove `vm_monitor` and the VM specs; add the flag patch. Everything else is recorded with a home step. | The fork branch stays self-contained, with no collision with step 6. |
| Order | Safety net first: repos + baseline CI → minors → images → drops and the flag patch → cheladb-v1. | The roadmap's rule: tests run before any real change. |

### Findings (2026-10-01)

- `neondatabase/neon` `main` = `fa504217` (last push 2026-08-31). `neondatabase/postgres` was last pushed 2026-04-09.
- Pinned Postgres (`vendor/revisions.json`): 17.5 `1e01fcea`, 16.9 `a42351fc`, 15.13 `2aaab3bb`, 14.18 `2155cb16`. Current releases: 17.11, 16.15, 15.19, 14.24 (18.6 for later).
- `REL_1x_STABLE_neon` heads are at 17.8 / 16.12 / 15.16 / 14.21, but they are 475 / 387 / 332 / 289 commits ahead of our pins. Among those commits, "Move the neon_storage_token GUC definition to the neon extension", `get_pin_limit_hook` and "Move LastWrittenLsnLock to neon extension" need extension code that our `pgxn/neon` lacks: `neon_storage_token` and `get_pin_limit_hook` appear nowhere in it. Neon's own minor merges are on v17 at `f702f46f37` (17.6), `178558d2ba` (17.7) and `1c0ec25d63` (17.8).
- The test runner already has `pytest-xdist`, `pytest-split` and `pytest-rerunfailures`. There are 151 regression test files. All jobs run inside the `build-tools` image.
- The storage `Dockerfile` hard-codes v14–v17 (vendor `COPY`, `pg_install` copies, `postgres_install.tar.gz`). The Makefile has `POSTGRES_VERSIONS`. The compute Dockerfile takes `PG_VERSION` and `EXTENSIONS` (`none` / `minimal` / `all`).
- `libs/vm_monitor` is a workspace crate that `compute_tools` depends on. The VM specs are `compute/vm-image-spec-{bookworm,bullseye}.yaml`.
- `FeatureResolver::update` is a no-op unless `posthog_config` is set (`new_disabled` has no inner store). `POST /v1/feature_flag_spec` exists.

## Part 1: Repos and history

**`chelabase/postgres`** (public)
- Created empty, then `git push --mirror` from a full clone of `neondatabase/postgres`, which copies every branch and tag (the v18 branches included).
- New branches `REL_17_STABLE_cheladb`, `REL_16_…`, `REL_15_…` and `REL_14_STABLE_cheladb`, at the pinned commits above. These are protected (PR, CI, no force-push), and `REL_17_STABLE_cheladb` is the default branch. The `_neon` branches stay as unprotected, read-only references.
- Maintainer remotes: `postgres` (`postgres/postgres`, the source of tags) and `upstream-neon` (frozen reference).

**`chelabase/neon`** (public)
- Created empty, then a push of `neondatabase/neon`'s `main` and release tags only, without the thousands of stale branches.
- The first commit, "fork: ChelaDB":
  - `.gitmodules` points at `https://github.com/chelabase/postgres.git`, still at the pinned commits;
  - removes upstream's `.github/workflows/*`, `actionlint.yml`, `CODEOWNERS` and Dependabot config;
  - adds `FORK.md` (base commit, Apache-2.0 with NOTICE kept, "never rewrite the engine", how minors flow);
  - retitles the README.
- `main` is protected: PR, the `ci-ok` check, no force-push, no squash merges.
- No renaming of crates or binaries: upstream patches stay readable and Compose stays the same.

**Access:** both repos are in the `chelabase` org. GHCR packages are linked to `chelabase/neon` and published with `GITHUB_TOKEN`. Creating the repos and the first push need the user's `gh` account with org-owner rights, and wait for the user's explicit go-ahead (public and permanent).

## Part 2: CI

**`build-tools.yml`:** builds upstream's `build-tools/Dockerfile` into `ghcr.io/chelabase/neon-build-tools:<hash>` when the file changes. Every job runs inside it. We never pull `neondatabase/build-tools`.

**`pr.yml`** (PRs to `main` and pushes to `main`):
1. `lint`: rustfmt check, clippy `-D warnings` with upstream's flags, `cargo deny`, ruff and mypy, actionlint.
2. `build-v17`: release build of the Rust binaries plus Postgres v17. The Postgres install is cached by submodule commit and the cargo target by `Cargo.lock`. Output: an artifact.
3. `rust-tests`: `cargo nextest` on that build.
4. `regress-v17`: N shards (start at 6, tuned from timings). Each runs `./scripts/pytest -n 3 --dist=loadgroup --splits N --group i --reruns 2` with `DEFAULT_PG_VERSION=17` and `BUILD_TYPE=release`, balanced by a committed `.test_durations`.
5. `ci-ok`: the one required status, which aggregates the jobs above.

**`nightly.yml`** (03:00 UTC, also manual): the same stages for v14, v15 and v16, plus a v17 debug build with its regression run. On failure it opens or updates one `nightly-failure` issue. A frozen major (`FROZEN`) still runs nightly.

**Baseline:** the first green run is on unchanged code (only `.github` and `.gitmodules` differ). A test that fails on unchanged code on our runners goes into `test_runner/known_failures.txt` with a reason (needs real AWS or Azure, timing on 4 vCPUs, and so on). The list may only shrink, except when an upstream-merge PR adds to it with a justification. Real-cloud tests stay skipped through their env gates, and `mock_s3` tests run.

**`chelabase/postgres`:**
- `minor-watch.yml` (daily) compares the newest `REL_<major>_<minor>` tag on `postgres/postgres` with `configure.ac` on each `_cheladb` branch, and opens `Merge PostgreSQL <x.y>` (the title de-duplicates it), labelled `security`. It skips majors listed in `FROZEN`.
- `pr.yml` builds the touched branch's major and runs `make check`.

**Gotcha:** a 4-vCPU runner has about 14 GB of free disk. The first CI task measures a full release build. Fallbacks: free space up front (remove the preinstalled SDKs), or use the larger disk on `ubuntu-24.04` runners.

## Part 3: Postgres minors

**Per major, oldest minor first, one PR per minor** (the plan may batch two or three per PR once a major has shown it merges cleanly):
1. `git merge REL_<x>_<y>` from `postgres/postgres` as a real merge commit.
2. Resolve conflicts against Neon's matching merge on the `_neon` branch where one exists (17.6–17.8, 16.10–16.12, 15.14–15.16, 14.19–14.21); after that, ourselves.
3. `chelabase/postgres` CI (`make check`).
4. A `chelabase/neon` PR bumps the submodule and `vendor/revisions.json`. Its CI (PR for v17, a manual nightly run for v14–v16) must pass before the next minor of that major.

Targets: 17.11, 16.15, 15.19, 14.24, then 14.25 in November, after which `FROZEN` lists `14`.

**Cherry-picks:** every non-merge commit on each `_neon` branch since the pin is classified in `CHERRY-PICKS.md` (in `chelabase/postgres`), one row per commit with its reason:
- **pick:** self-contained correctness or security fixes with no `pgxn/neon` dependency. Candidates are the multixid offset fixes, the async-notify ERROR→FATAL and xid fixes, "Fix UC permissions check after CVE-2025-8713 fix", btree split stability, and "Do not emit WAL for unlogged BRIN indexes";
- **skip (unpublished ext):** for example the storage token, pin-limit hook and LastWrittenLsnLock move;
- **skip (Databricks/feature):** `BRC-`/`LKB-`, pgbench additions, prewarm.

Picks land after the minors, one PR per major, gated the same way.

**Gotcha:** a minor can change WAL or catalog details that the Rust side decodes (`postgres_ffi`, walingest). The full regression run on the `neon` bump PR is what catches this.

## Part 4: Images and cloud features

**Build args** (fork commit "images: variant build args"): the storage `Dockerfile` gains `ARG PG_VERSIONS="v14 v15 v16 v17"`, which drives the vendor copies, `make POSTGRES_VERSIONS`, the `pg_install` copies and the tarball. Its build-tools defaults point at `ghcr.io/chelabase/neon-build-tools`. The compute Dockerfile is unchanged (`PG_VERSION`, `EXTENSIONS`).

**`images.yml`**: dev images on every push to `main` and on `v*` tags; platform images on tags and a weekly cron. GHCR registry cache, amd64 only (arm64 later through the same args), `provenance: true, sbom: true`.

| Image | Variant | Args | Tags |
|---|---|---|---|
| `ghcr.io/chelabase/neon` | platform | `PG_VERSIONS` = all | `<sha12>`, `<tag>` |
| `ghcr.io/chelabase/neon` | dev | `PG_VERSIONS=v17` | `<sha12>-dev`, `<tag>-dev` |
| `ghcr.io/chelabase/compute-node-v17` | platform | `EXTENSIONS=all` | `<sha12>`, `<tag>` |
| `ghcr.io/chelabase/compute-node-v17` | dev | `EXTENSIONS=minimal` | `<sha12>-dev`, `<tag>-dev` |
| `ghcr.io/chelabase/compute-node-v1{4,5,6}` | platform | `EXTENSIONS=all` | `<sha12>`, `<tag>` |

**Remove `vm_monitor` and the VM specs** (one PR):
- delete `libs/vm_monitor` and `compute/vm-image-spec-*.yaml`, plus the workspace and dependency entries;
- remove `compute_ctl`'s vm-monitor launch path and its flags (`--vm-monitor-addr`, `--cgroup`, `--filecache-connstr`), after checking that cheladb-v1's launcher (`crates/cheladb-neon`) never passes them.

Gate: CI green, and `known_failures.txt` doesn't grow.

**Push-only feature flags** (one PR):
- new pageserver config `feature_flags_push_only: bool` (default `false`).
- When `posthog_config` is absent and it is `true`, `FeatureResolver::spawn` builds an inner store with no PostHog background loop and no `CaptureEvent`. `update()` writes into it, and the evaluators read it.
- With PostHog configured, behaviour is unchanged. With neither set, it stays `new_disabled`.
- The push path is the existing `POST /v1/feature_flag_spec`, behind the management API's auth.

Tests in `pageserver/src/feature_resolver.rs`:
- `push_only_update_then_evaluate_boolean`: after a pushed spec, a matching flag evaluates `Ok`;
- `push_only_without_spec_is_not_available`: before any push, evaluation returns `NotAvailable`, so defaults apply;
- `disabled_resolver_ignores_update`: without the setting, `update` is still a no-op.

**Cloud-feature decisions** (no code in this step):

| Feature | Decision | Home |
|---|---|---|
| `consumption_metrics`, `tenant_size_model` | keep | usage/billing step |
| `fast_import`, `timeline_import` | keep | import step |
| `endpoint_storage`, `basebackup_cache` | keep | step 7 (scale to zero) |
| shard splitting, warm secondaries, storcon HA | keep, off | multi-node storage step (likely alongside the Kubernetes backend) |
| `disk_usage_eviction` | keep; can be turned on single-host | alpha sizing (it bounds the pageserver's local disk) |
| proxy rate limiter, Valkey cancellation | keep (in use) | — |
| `storage_scrubber` | keep | operations step |
| `console_redirect_proxy` | keep, rework to log in through Studio | its own step |
| auth broker, `local_proxy`, `pg_session_jwt` | keep, off by default | — |
| worker's flag push | build | after step 6 |
| `vm_monitor`, VM specs | removed here | — |

## Part 5: cheladb-v1 integration

On branch `neon-fork`, after the fork's images are published:
- `infra/neon/NEON_REF` is set to the `chelabase/neon` commit with parts 1–4.
- `infra/neon/build.sh` clones `chelabase/neon`. `VARIANT=dev|platform` (default `dev`) selects `PG_VERSIONS=v17` / `EXTENSIONS=minimal`, or all of them.
- `infra/compose/docker-compose.yml`: `NEON_IMAGE` and `COMPUTE_NODE_IMAGE` default to the digest-pinned `-dev` images. The "packages are private" comments go.
- `scripts/neon-images.sh`: the local tag matches the `-dev` suffix, and the pull-failure hint drops "private".
- `infra/neon/tests/run.sh` adds three checks:
  - `NEON_REF` resolves on `chelabase/neon`;
  - the Compose defaults have a digest and the `-dev` tag for `NEON_REF`'s sha12;
  - `build.sh` never references `neondatabase/neon`.
- `infra/neon/patches/` is deleted, and `infra/neon/README.md` is rewritten around the fork.
- Roadmap: strike the "we don't fork" lines, point the hard-fork section at this spec, and note the v14 "not for new databases" rule for the `pg_version` work.
- `@neondatabase/serverless` in `packages/neon-e2e` stays (it's the client driver).

## Testing and verification

- **Fork:** PR CI (`ci-ok`) on every fork PR; the nightly green before part 5 starts; the three feature-flag tests; `known_failures.txt` reviewed at the end-of-step review.
- **cheladb-v1 full run** (one Compose stack at a time; it takes turns with step 6's live tests):
  - `cargo test --workspace` and clippy `-D warnings`;
  - `just neon-test`, then `just deps-up` with the pulled GHCR dev images, then `just neon-smoke`, `just neon-e2e` and `just auth-e2e`;
  - the ignored live tests `neon_live`, `proxy_cache_live` and `storcon_live`;
  - `SELECT version()` on a new compute reports 17.11;
  - a database created on the 17.5 compute before the switch starts on 17.11 and reads its data;
  - the platform `compute-node-v17:<sha12>`, pulled and run, accepts `CREATE EXTENSION pg_session_jwt, vector, postgis` in a throwaway database.

## Risks and gotchas

- **Runner disk** (part 2). Measured first, with known fallbacks.
- **Regression time on 4 vCPUs.** The shard count is tuned from timings, and the PR target is about an hour.
- **Conflicts from 17.9 on** have no Neon reference merge, so each is reviewed against the smgr and WAL patches.
- **Minor-release WAL or catalog changes** are caught by the `neon` bump PR's regression run, not by `make check`.
- **Public, permanent actions** (creating the repos, the first GHCR publish) wait for the user's go-ahead.
- **Parallel step 6:** the roadmap file is edited on both branches, so whichever merges second resolves a docs-only conflict. One Compose stack per machine.

## Out of scope

- Postgres 18 and 19 (next fork spec).
- The worker's flag push; the console redirect through Studio; enabling any "keep, off" feature.
- Renaming crates, binaries or images beyond the `ghcr.io/chelabase` namespace.
- arm64 images.
- Performance work (waits for profiling evidence).
