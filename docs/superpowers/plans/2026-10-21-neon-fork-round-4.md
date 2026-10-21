# Neon fork round 4: token files, storage and compute_ctl auth, slot and streaming fixes, personal-login marker, compute extensions (implementation plan)

**Goal:** One fork round and one image build that ship: temporary slots never reach the pageserver and stale copies are cleaned at start (U16); `chelabase_keys` streams large transactions (W9); compute_ctl's internal API on loopback behind a per-start secret, and token files (D2, U1) carrying that secret and a refreshable tenant storage token that expires (item 7, RT4-2/3); `:3080` answering 401 with no bypass (item 4); no panic on a tenant token without `tenant_id` (item 5); JWT on `/metrics` and the profiles with a read-only `metrics` scope, compute_ctl's metrics included (D3, U2, U4); the proxy's Valkey password off its argv (item 8); `chelabase_realtime` logins only from realtime with its certificate (W31); storcon backoff (item 10); the personal-login marker (D1); and the compute extension set bumped, with `pg_mooncake`, `pgrag`, `pgjwt` and `pg_tiktoken` removed and one pgrx layer (§17, RT4-1). Chelabase gets the spec fields, expiring tokens for capable computes and their refresh loop, the metrics token, realtime's client certificate and control-network address, the personal-login helpers, and the Compose and script changes.

**Spec:** `docs/superpowers/specs/2026-10-21-neon-fork-round-4-design.md` (`b4c32da0`; decisions D1–D3, answers U1–U4, red-team rulings RT4-1..13 in §15, per-area checklist §16, extensions §17) and the red-team `docs/superpowers/specs/2026-10-21-neon-fork-round-4-red-team.md` (`ebb8148b`). Where a section and §15 disagree, §15 wins. Implementers read the spec sections their task names.

**Architecture:**
- Postgres fork (`chelabase/postgres`, v17 only): `slot.c`, `slot.h`, `xlog.c` (K1); `contrib/chelabase_keys/` (K2).
- Neon fork (`chelabase/neon`): `pageserver/src/basebackup.rs`, `libs/postgres_ffi` (K1); `compute_tools/`, `libs/compute_api/`, `control_plane/src/endpoint.rs` (K3, K4); `pgxn/neon/` token reader and callers (K5), `neon_ddl_handler.c` (K6); `libs/utils/src/auth.rs`, pageserver/safekeeper/storcon auth and routes (K7); `proxy/` (K8); `storage_controller/src/persistence.rs` (K9); `compute/compute-node.Dockerfile`, `compute/manifest.yaml`, `compute/patches/`, `docker-compose/`, `build-tools/Dockerfile` (X0–X6); integration (K10).
- Chelabase: `crates/chelabase-neon` (C1), `services/worker` (C2, C3), `crates/chelabase-types`, `crates/chelabase-metadata` (C3), `services/orchestrator`, `services/realtime` (C4), `tools/cli`, `infra/compose`, `scripts/` (C5), docs (C6).

**Tech:** C (PostgreSQL 17.11 fork), Rust (Neon fork on 1.99.0; Chelabase workspace), Python (Neon's pytest suite), Docker (compute image, extension-test harness). Extension versions and checksums: spec §17.2 (verified 2026-10-21); additions verified for this plan: CGAL **v6.2.1** (latest release, 2026-09-04; SFCGAL 2.3.0 needs CGAL ≥ 5.6, pre-flight 12), pg_duckdb 1.1.1's GUC names and contexts (pre-flight 13). No new Chelabase crate dependency. **Fork change: yes**, so one fork gate, one image build (minimal storage and compute through `publish-dev.sh`, plus `extensions-all` for the harness, S19), one build-tools image (X5), one `NEON_REF` bump (End of step 4).

**Standing rule (task pipeline, CLAUDE.md "Task pipeline"; replaces the template's coordinator-written RED tests):** the coordinator writes each task's brief from this plan (behaviour, files, pinned interfaces, the edge-case checklist, the RED list below); then, in sequence, a **test agent** writes and commits the RED tests (`<id>: RED tests`), compiles every target (live ones with `--no-run`), runs every runnable one against the base and shows each failing on its first assertion for the intended reason (live tests needing the new images are traced by reading); a **coder agent** makes them GREEN without editing, weakening, skipping or deleting them; an **audit agent** reviews and tidies, confirms `git diff <red-commit>..HEAD` on the RED files is empty, and reports defects and hardening separately. The audit stage is the task's one review; a leaf task has none (the coordinator checks the RED diff itself; the end-of-step review covers it). Every brief starts with "Read `<repo root>/docs/agent-preamble.md` and follow it", says no agent may spawn agents, and carries the §16 checklist of its area (copied below per task).

## Status

- **PAUSED FOR THE USER'S GO TO MERGE (2026-10-22 ~04:50).** Everything below is done and green.
  - **Branches:** Chelabase `neon-fork-round-4` `222ab6ac`; fork `round-4` `0741b56de`; Postgres `round-4-v17` `7812d66fe85`.
  - **Gates:** `just check` passed (3748 tests). Fork gate: default gate (lint, build, Rust), pg-check, and the regress selection. Of the regress failures, the disk-pressure ones were rerun green; the two compute_termination tests were stale and fixed in `c1dd851c9`.
  - **Images:** built locally, not pushed: `0741b56deb67-dev`, plus build-tools `f3f0cc340753`. The minimal harness passed.
  - **Live pass:** every suite and e2e is green. `branch_live` was fixed in `5608feb8` and is 4/4.
  - **Landing highlights:** on main, `f8a6a67e`.
  - **Left for the user:**
    1. The go to ship: End of step 7, using the rewritten removals list at the end of this plan.
    2. Two compute containers on the step stack keep restarting (`…-compute-ep-97d2975e09c4`, `…-compute-ep-7211a718fa1b`). The full-disk run left them behind. The debug agent's `docker rm -f` was denied by the permission check, so the coordinator didn't retry it.
  - **Next step (user rulings):**
    - the storcon 409 retry;
    - the worker leaving computes behind after a failed create (debug agent finding, possible bug).
  - **Queued after this round:**
    - the curated replay into a new repo, which takes in the process trim, ChelaDB's lanes and per-service images;
    - upstream PR candidates;
    - the compute-extensions step.

- **After this step, before the Members page (user, 2026-10-21): trim the process** in CLAUDE.md, `docs/agent-preamble.md`, the plan template and the roadmap's working rules. The user asked for:
  - **Status block:** archive finished steps; the Status keeps current state and open items, and a finished task's line is appended to an archive/log at the end.
  - **Mechanical tasks:** no full test → coder → audit pipeline.
  - **Lint fixes:** coders may fix lint, format and typing in RED files themselves (no assertion changes), without handing them back to the test agent.
  - **Other trims** (coordinator proposals, to confirm with the user then):
    - a short "Resume here";
    - Status commits only at task boundaries and pauses;
    - compile-only RED tests proven by a compile, not a full run;
    - briefs that point to plan sections instead of restating them.
  - **Refined (user, 2026-10-21): model the process on ChelaDB's lanes** (`chelabase/cheladb`). It has:
    - the `.claude/{commands,agents,lanes,skills}/lane-*` files;
    - the `scripts/lanes` CLI;
    - `lanes/labels.yml` (with `4-status` triage → ready → in-progress → in-audit → audited/changes-requested → integrated);
    - its PR template.
    
    In that model, each step is an epic (milestone issues, an `epic/<slug>` branch, a draft PR) and its state lives in GitHub, not a Status block. Mechanical tasks go through `/lane-quick`.
    - **Keep:** the spec doc, red team, pre-flight, seam pass, opus test-first for security work, one local gate, build once, one stack per plan.
    - **Drop:** Copybara, the leak rules, moon, and GitHub checks for now. `chelabase/neon` keeps only its scheduled advisories job.
    - **Fork work:** tracked in public issues in the fork repos themselves, as `neon-pre-replay` did (user, 2026-10-21). `chelabase/chelabase` goes public soon too.
    
    It runs as its own small step after this one; the Members page is its first epic.
  - **Superseded (user, 2026-10-21): a curated replay into a new repo.** The process trim and the lanes port become part of this replay step, which runs after fork round 4 merges and before the Members page. Steps:
    1. An inventory of PII and host references across every file and commit (emails, home paths, hostnames, IPs, machine details, credential rules, private repo names). The user approves the replacements.
    2. Consolidate the roadmap, specs and plans into a coherent `docs/history/` of per-step decision records.
    3. **Curated replay (user's choice over a `filter-repo` scrub or a single snapshot):** rebuild the history as a smaller set of meaningful, cleaned commits, roughly one per roadmap step, into a new **private** repo for the user's review before it goes public.
    4. ChelaDB's lane setup comes in: rules, labels, PR template, `scripts/lanes`; Copybara/moon only where a piece really builds standalone. Chelabase is one Cargo workspace with shared crates.
    5. Verify: gitleaks/trufflehog over the whole history, a grep for every inventory pattern, `just check`, the image pins and `NEON_REF` resolve.
    
    The current repo is then archived privately. Fork repos get the same PII grep.
  - **Layout (user, 2026-10-21): stay a monorepo, no per-service mirror repos or Copybara.** Publish one image per service (`ghcr.io/chelabase/{gateway,rest,realtime,auth,orchestrator,worker}`, maybe studio/cli): a shared cached build stage, small non-root runtime images, one version tag across all images, a local publish script first. Self-host Compose uses the images; the workbench keeps checkout binaries via a dev override. Per-service deploy docs. A step of its own after the replay (~6–10 h). Decoupling a service so it's usable outside Chelabase (rest reads the metadata DB; the others ask the orchestrator) is later and optional, one service at a time.
- **Now (2026-10-21, evening):** **K4 DONE**, audit pass, 0 defects (tidy `d732c9608`; coder deviations accepted: stray unmarked `chelabase_realtime` ident lines removed on first write, `HEAD` on metrics routes, `Authorize::new` without `instance_id`); **merged into Neon `round-4` `1b6fb9785`**; clone `-k4` removed. **K4 deferred to the final fix round (hardening):** (a) `config.rs:146-148` writes postgresql.conf/pg_hba/pg_ident as three renames, so a failed ident write leaves new hba with old ident (write ident before hba, or roll back); (b) `plan_pg_hba` replaces a hand-moved realtime block in place, never moving it back after the member block; (c) `authorize.rs` `verify` logs one `warn!` per JWKS key on every failed token (unauthenticated log spam; pre-existing). **W4 running:** K6 test agent (opus; clone `-k6`, `round-4-k6` at `1b6fb9785`, volumes `-k3`), K8 test agent (sonnet; clone `-k8`, `round-4-k8` at `7912a1e3e`, volumes `-k8` seeded from `-k3`). **K8 RED** `96455345c` (compile-RED; proxy tests in `compute_ctl/tests.rs`, `binary/local_proxy/token_tests.rs`, `binary/proxy/secret_tests.rs`; pinned `ComputeCtlApi::token_file`, `LocalBackend::new(…, Option<Utf8PathBuf>)`, `plain_redis_connection_info(url, password_file)`; run `.ci-local/20261021-190604-v17-release`); **K8 implementation** `08ac3586d` (proxy 184/185, `.ci-local/20261021-193358-v17-release`; `%` pre-escaped as `%25` before `Url::set_password` since url doesn't encode it and redis decodes userinfo; `configure_redis` no longer panics on a bad URL; coder made lint-only RED edits, allowed by the user's new rule). **Process failure: K8 RED test bug**: `configure_redis_fails_closed` writes every case to the same file, so "empty" reads the good password; fixed by the test agent `7393d8efa` (185/185, `.ci-local/20261021-193718-v17-release`). **K8 audit:** 1 important defect, **fixed now** (X0's image builds compile the proxy with default features): the `camino` import is gated on `testing` but ungated code uses it, so the proxy and local_proxy fail to compile without `--features testing` (`proxy/src/binary/proxy.rs:12-13`); `ci-local` builds only with `testing`, so it can't catch this. Fixed in `6730b03ef` (coordinator verified the one-line diff; default-feature `cargo check` and clippy clean both ways). **K8 DONE, merged into Neon `round-4` `e3f48874e`.** **K9 RED** `1e3880c56` (`persistence.rs` `backoff_tests`, compile-RED on `retry_delay_cap`/`retry_delay`/`retry_on_serialization_failure`; Python RED only on the missing histogram, since the burst doesn't fail deterministically on the base; runs `.ci-local/20261021-19{5833,5939}-v17-release`). Pinned: `MAX_RETRIES = 20` (from 128), metric `storage_controller_database_query_retries`. **K9 coder (sonnet) running** in clone `-k8`, `round-4-k9`, volumes `-k8`. **Deferred to the final fix round (K8 hardening):**
  - **Token file:** `read_token` must open with `O_NONBLOCK` and check it's a regular file. A FIFO at the path blocks a pool thread.
  - **Token file:** a rate-limited `warn!` on an invalid file, never printing its content.
  - **Redis URL:** restrict `plain_redis_connection_info` to `redis`/`rediss`. With `unix:`/`redis+unix:` the file's password is silently dropped.
  - **Password file:** reject control characters (e.g. a trailing `\r`).
  - **Auth type:** error when `--redis-plain-password-file` is given without `--redis-auth-type=plain`.
  - **Gate:** add a default-feature `cargo check` of the proxy to the fork gate. **K6 RED** `d0a2db81c` (`test_member_role_guard.py` +436, 10 RED on the base, round 3's 2 pass; `.ci-local/20261021-190934-v17-release`). **Coordinator ruling (wording, listed to the user):** a non-superuser's change of the marker itself (`ALTER ROLE … SET/RESET`, `GRANT/REVOKE SET|ALTER SYSTEM ON PARAMETER`) gets `42501` `permission denied to change parameter "chelabase.managed_role"`; statements on a marked role keep the pinned `role "%s" is managed by Chelabase`. K6 test-agent note: `ALTER ROLE ALL IN DATABASE … SET` needs only database ownership (round 3's comment at `neon_ddl_handler.c:~1878` is wrong; coder corrects it; a database-wide row marks nothing). **K6 implementation** `2eceb6c5f` (126 passed, `.ci-local/20261021-193141-v17-release`; edge file `test_member_role_guard_edges.py`; `DbRoleSettingHasEntry` shared with `DatabaseBornClosed`). Coder deviations under audit: early return before `superuser()` for unguarded statements (aborted-transaction syscache hazard); `ALTER SYSTEM SET/RESET` and `ALTER ROLE ALL … SET/RESET` of the marker refused for non-superusers; `ALTER DATABASE … SET` of the marker and `GRANTED BY <marked>` unguarded. **K6 DONE**: the audit passed with 0 defects; all 4 deviations were accepted; tidy `12eb8dfc8` (comments only). **Merged into Neon `round-4` `af2f8a3d1`**; clone `-k6` removed (it used the `-k3` volumes, which are kept).
  - **K6 hardening to the final fix round:**
    - H3: change `default: break;` in the guard's second switch to `Assert(false)`.
    - H4: version-skip the `ON PARAMETER`/ADMIN tests below PG16.
  - **For C3's brief (Members):**
    - H2: `DROP OWNED BY X` / `REVOKE … CASCADE` can remove a marked role's memberships if X was recorded as their grantor. Grant memberships as a superuser only.
    - H5: the worker's drop of a personal login runs `REASSIGN OWNED`/`DROP OWNED` first, because a `GRANTED BY` dependency blocks `DROP ROLE`.
  - **User ruling (2026-10-21) on H1, final fix round:** the guard refuses `COMMENT ON ROLE` and `SECURITY LABEL ON ROLE` on a marked or managed role for non-superusers, on all versions. On PG14/15 a CREATEROLE non-superuser can run both; PG16+ blocks them natively, and with `anon` a label changes masking. Dropping v14/v15 was considered and rejected as not worth the cost.
  - **X0 RED** `6679e2edf` (`docker-compose/tests/chelabase-ext-test_test.sh`, 23 cases, 20 RED). Interface pinned by the test agent (header of the test): flags, `REPOSITORY=chelabase-local`, `TAG`, image names, build args, `run-tests.sh`/`docker_compose_test.sh` `ONLY`, SKIP `pg_jsonschema-src,rdkit-src,timescaledb-src`; `manifest.yaml` `unstable_extensions` reduced to `anon`.
  - **Coordinator rulings (mechanical):**
    - No `--no-build` flag: later runs rebuild from their checkout on the layer cache.
    - Every harness run passes `--storage-image`. The wrapper warns on stderr when it's absent; the tests allow that.
  - **X0 implementation** `390f236df` (shell test 23/23; removal grep empty).
    - The base run failed after 66 s: the h3-pg URL (`zachasme/h3-pg`) is dead, because the repo moved to `postgis/h3-pg`. The sha256 is identical.
    - **Ruling:** X0 commits the URL swap. It replaces the separate base run with one whole-harness run on its final commit (`--keep-images round4-base`), which becomes X1–X6's base.
    - Other dead URLs are fixed only when the sha256 is unchanged; otherwise the coder stops and reports.
  - **Memory incident (2026-10-21 20:26):** the OOM killer killed the user's editor, and K9's host cargo build died with exit 137. On 30 GB RAM, three things were running at once: the `extensions-all` build (BuildKit parallel C++ stages: V8, DuckDB, rdkit), a host cargo build at 16 jobs, and the main checkout's running stack. **Rule from here: one heavy build at a time on this machine** (a compute image build, a `ci-local.sh` run, or a host cargo build). Host cargo is capped at `-j 8`. A future `extensions-all` build caps BuildKit `max-parallelism`.
  - **Scope change (user, 2026-10-21): this round only needs the minimal compute.** `extensions-all` work is split out into a later "compute extensions" step.
    - **Moved to that step:** X1 (pgrx group), X2 (PostGIS stack), X3 (rdkit, plv8, pg_duckdb with the RT4-1 patches and grants), X6 (the RT4-1 image test), X4's `extensions-all` rows, the whole-harness run, and the `extensions-all` image build at End of step.
    - **Stays (it changes what the minimal image ships):** X0's removals (pgrag left minimal; the others are `all`-only, already done and cheap), X4 narrowed to timescaledb, pg_cron and pg_partman, and X5 (pgbouncer and the exporters ship in every image).
    - K3's DuckDB pins in `postgresql.conf` stay (harmless without pg_duckdb).
    - **X0 verify is now** the shell test plus one `EXTENSIONS=minimal` compute build (`round4-minimal`). The `extensions-all` wrapper run was stopped.
  - **X0 implemented** `390f236df` + `941ccf640` (h3-pg URL). Shell test 23/23. Minimal compute builds in 243 s (1.13 GB, `chelabase-local/compute-node-v17:round4-minimal`). **X0 audit (sonnet) running.**
  - **For X4 (coder's finding):**
    - `extension-tests` is `FROM pg-build`, independent of `EXTENSIONS`, but it always builds PostGIS.
    - The wrapper hard-codes `EXTENSIONS=all`.
    - timescaledb has no `ext-src` dir and its `COPY` is commented out.
    
    So X4 needs an `--extensions` option and those additions to run the minimal suites.
  - **Disk:** 84 % after X0's builds. The user approved a build-cache prune (21.5 GB), and the disk is now 77 %.
  - **X0 DONE.** The audit passed with 0 defects; tidy `d03e88a59`. **Merged into Neon `round-4` `b9b829b74`.** Clone `-x0` kept for X4/X5.
  - **X0 hardening, deferred to the final fix round:**
    - `run-tests.sh` `ONLY` entries: allow only `^[A-Za-z0-9._-]+$` and reject `.` and `..`.
    - The wrapper's `cleanup` runs a best-effort `docker compose down`.
    - `--help` notes that `chelabase-local/neon:<tag>` is retagged and removed.
  - **X4 needs** (X0 audit):
    - an `--extensions` option or image reuse in the wrapper;
    - a slim `extension-tests` target (it always builds PostGIS today);
    - a timescaledb test dir plus its uncommented `COPY`;
    - a harness compute config with `shared_preload_libraries` and `cron.database_name` for pg_cron/pg_partman.
  - **C1 RED** `a9252ec0` (compile-RED across lib, `reconfigure_mock`, `storage_auth*`, `compute_tls`). **C1 coder (sonnet) running.**
  - **New pins from the C1 test agent:**
    - `compute_tls::renew_client_pair(dir, ca, cn, now) -> Result<bool>`. The worker's `renew_orchestrator_pair` and the CLI's `ensure_client_pair`/`pair_is_current` move onto it in **C2/C5**. `write_client_pair` is unchanged: the certificate already carries the CN.
    - `storage_token(Metrics)` returns an error.
    - `tenant_token` returns an error on overflow.
  - **Plan note:** `SpecInputs.storage_token_expiring` has no effect, because `config_response` takes a pre-minted token. It is kept as pinned, and the end-of-step review decides whether to drop it.
  - **K9 implementation** `87d65e4a3`: storcon 55/55 (`.ci-local/20261021-203756-v17-release`); the burst test passes (`…-203938-…`); histogram buckets `[0,1,2,3,5,8,13,20]`.
    - **Process failure (test design):** the pinned `AsyncFn` driver signature can't be called from `with_conn` (callers' futures hit "`Send` is not general enough"). The coder routed every DB call's connection through an mpsc/oneshot hop to fit it.
    - **Coordinator ruling:** re-pin the driver as `retry_on_serialization_failure(attempt: impl FnMut() -> impl Future<Output = DatabaseResult<T>>)`. Each attempt owns its connection, which drops before the sleep. The test agent rewrites the driver tests after a compile probe on the real `with_conn`; then the coder simplifies `with_conn` and removes the channel hop. Not merged until then. Re-pinned in `0edf74987` (a throwaway probe compiled the direct `with_conn` call, and 17/17 backoff tests passed). **K9 DONE** (`db17868ad`: direct `with_conn`, 60 lines less; storcon 55/55 `.ci-local/20261021-204451-v17-release`, burst test green `…-204758-…`; coordinator checked the diff). **Merged into Neon `round-4` `3085f9ef3`.** Clone `-k8` and the `-k3` volumes are removed; the `-k8` volumes are kept warm for K10 and the gate. Disk 62 %.
  - **C1 implementation** `b1f8a25b`:
    - `chelabase-neon` all green (109 lib unit tests plus the integration targets); `--features metadata` compiles; clippy clean; worker and orchestrator build.
    - New edge file `tests/c1_edges.rs` (5 tests).
    - The interim callers carry an `Interim (SP4)` comment.
    
    **C1 DONE.** The audit found 0 defects; tidy `b3f26d4d`. Merged into `neon-fork-round-4` `7efe0db3`; worktree and branch removed.
  - **C1 hardening, deferred to the final fix round:**
    - cap compute_ctl's error body in our error text at 512 bytes (`reconfigure.rs:114`, `:156`);
    - treat a symlink loop under `current` as absent in `renew_client_pair` (`compute_tls.rs:499-509`);
    - tolerate `capabilities: null` (`reconfigure.rs:62`);
    - drop the inert `SpecInputs.storage_token_expiring`. This edits RED tests, so it goes to the test agent.
  - **For C5:** mint the metrics token with `metrics_token(now)`, never via `STORAGE_TOKENS`.
  - **C2 test agent (sonnet) running** in `.worktrees/neon-fork-round-4-c2`.
  - **X4 RED** `d7a399158`:
    - The shell test has 10 cases RED.
    - Version tests in `ext-src/{timescaledb,pg_partman,pg_cron}-src/neon-test.sh` were run against `round4-minimal`: 2.17.1 and 5.1.0 fail as intended.
    - pg_cron's `extversion` is `1.6` in both versions, so its test also requires the v1.6.8-only GUC `cron.match_dom_and_dow`.
    - New pins: `--extensions minimal|all`, and the stage `extension-tests-minimal` (exactly the three dirs).
    - Harness `config.json`: `cron.database` → `cron.database_name`. `timescaledb-src` is removed from SKIP.
    
    **X4 implemented** `ed542f733`:
    - Shell test 38/38; `pg_cron.patch` applies to v1.6.8; the pgaudit ordering holds (`config.rs:367-377`).
    - Both minimal builds pass in about 7 min.
    - The harness couldn't pull `quay.io/minio/minio`/`minio/mc` (401; gone from Docker Hub), so the suites didn't run.
    - **Ruling:** X4 replaces MinIO in the fork's harness compose with SeaweedFS, the same pinned image Chelabase runs (`chrislusf/seaweedfs:4.47@sha256:ce9e796f…`), then reruns the three suites. **X4 DONE** (leaf, no audit). The coordinator checked the RED diff: only the new no-minio case was added and the ext-src tests are unchanged.
    - Commits: `46c313084` (SeaweedFS), `e26602735` (`pgx_ulid` preloaded only when present), `c3d7972e2` (harness data prep skipped under `ONLY` when the slim image lacks it).
    - The wrapper run passed: timescaledb, pg_partman and pg_cron suites all green on the minimal compute. Shell test 39/39.
    - **Merged into Neon `round-4` `ea3354dfb`.** Images kept: `chelabase-local/*:round4-x4`. Disk 70 %.
  - **X5 RED** `8ba376181`: `docker-compose/tests/x5-binaries_test.sh` (versions, shas, the tag commit, build-tools ENV; fails 20 checks on the base) and `test_compute_metrics.py::{test_sql_exporter_pins_agree,test_sql_exporter_binary_version}` with a `SQL_EXPORTER_VERSION` constant.
    - **SP21 settled:** the builder is `default` on the `docker` driver, so no switch is needed.
    - **X5 stays in this round (user, 2026-10-21).** The coordinator stopped it briefly by mistake. pgbouncer and the exporters ship in the minimal image, and pgbouncer 1.25.x carries security fixes. **X5 coder resumed.** No other X work this round.
  - **C2 RED** `1bfba458`: unit tests in `services/worker/src/c2_tests/`, plus the live changes in `compute_fork_live.rs`. It is compile-RED only.
    - **New pins:** `SpecPusher::report`/`push_storage_token`, the `FakePusher` knobs, `token_refresh::{RefreshPass, storage_token_refresh_loop}`, `Worker::refresh_storage_tokens_once`, and the new `Worker`/`WorkerConfig`/`CertSweep` fields. The refresh interval is `0 < s < 86400`.
    - **Plan correction:** `list_aux_files` is a `testing` route and the images build without `testing`. `a_crashed_compute_leaves_no_slot_aux_file` uses a pageserver `basebackup` over libpq piped to `tar -t` instead. This is unverified until the live pass.
    - **For C5:** the runner needs `realtime_tls_dir` mounted with the pair the worker's sweep writes.
    - `build_and_push_spec` also runs from `member_roles.rs:1654`, so it gets the `report()` too.
    
    **C2 implemented** `a72e56bc`:
    - Worker lib 814/814, clippy clean, live targets compile.
    - `renew_orchestrator_pair` is deleted; the sweep uses `renew_client_pair` for both CNs.
    - No `Interim` value is left in the worker. The orchestrator's interim value is C4's.
    - Deviation, accepted by the coordinator: one fixture line, `realtime_login_cidrs = Some(vec![])`, in `f4_tests/config_ca.rs:22` and `rt1_tests/config.rs:68`, because `build()` now refuses the variable when unset.
    
    **C2 DONE.** The audit found 1 important and 1 minor defect and accepted the deviation; tidy `3b18ddac` (`report_bounded`). **Merged into `neon-fork-round-4` `a6d04f71`**, worktree and branch removed.
    - **Important, fixed by C3's test agent** (it edits the same file, P13): `an_expired_storage_token_is_refreshed_without_a_restart` asserts the whole pass, `failed == 0`. It flakes when a sibling compute is restarting. The fix is to assert on the test's own endpoint.
    - **Final fix round:**
      - (minor) prune the `storage_token_failures` entries of endpoints gone from a pass;
      - (hardening) each loop pass survives a panic;
      - (hardening) skip a capable compute whose status isn't `running`/`configuration_pending`;
      - (hardening) refuse `CHELABASE_REALTIME_TLS_DIR` == `CHELABASE_COMPUTE_TLS_DIR`;
      - (hardening) take `now` per endpoint;
      - (hardening) cap the refresh interval at TTL/2;
      - (hardening) put a timeout on the live test's `docker exec`.
    - **Note:** a stack worker built from C2/C3 fails at `build()` until C5 renders `CHELABASE_REALTIME_LOGIN_CIDRS`, so don't rebuild the stack before C5 (consistent with P6).
  - **C3 RED** `de13681d` and C2 live-test fix `aa809d11` (now asserts on its own endpoint). Compile-RED. A throwaway stub run showed 7 of 10 worker tests failing as intended; 3 are guards.
    - **Pins:** `create_personal_login` runs on `SYSTEM_DATABASE`, takes no lock, writes no metadata row, and rejects reserved names and non-SCRAM verifiers before any SQL. Resync recreates a personal login without `SET ROLE` or an ADMIN grant, ending with the marker.
    - **Plan corrections:** K6 H5 is already true: `delete_role` does `REASSIGN OWNED`/`DROP OWNED` first, and a guard test pins it. There are no exhaustive `RoleOrigin` matches in `roles.rs`/`ddl.rs`.
    - **C3 implemented** `614f9fbd`.
      - Tests: types 92, metadata 32, worker 826, orchestrator lib 132 + api 427 + internal 121, all green. Clippy clean.
      - `just api-gen` updated `openapi/management.json` and `roleOrigin.ts`.
      - `create_personal_login` wraps its script in `BEGIN…COMMIT`.
      - The coordinator checked that the RED files are unchanged.
      - **C3 DONE.** The audit found no defect in this round's paths and made no tidy commit. **Merged into `neon-fork-round-4` `16e0b6b9`**; the worktree and branch are removed. Personal logins can't be created through any API yet, so neither defect below is exposed today.
        - **Important, latent: final fix round.** Resync can adopt an unmarked role as `Personal` (`resync.rs:886-890`). Example: an owner-made `alice` on a point-in-time child gets recorded as alice's personal login, with the owner's verifier.
          - Fix: give `ObservedRole` a `marked` field (SP22 in `RESYNC_SQL`). Never adopt an unmarked role as `Personal`; record it as `Sql` and log.
          - This needs the test agent, because RED `a_present_personal_login_is_left_alone_and_keeps_its_origin` builds the role without a marker.
        - **Important, latent: user ruling 2026-10-21, decide in the Members step.** The owner-facing API acts on personal rows as `cloud_admin` and bypasses the marker: `reset_role` plus `replace_role_secret` (reset, then reveal the password), and `delete_role`. The Members step spec decides who may reset or delete, and adds the platform removal path. Until then the paths stay as they are.
        - **Minor: final fix round.** Resync's recreate filter uses `!has_reserved_prefix`. Use `is_reserved_target(name, owner)` instead, so `anon`, `authenticated`, `authenticator` and `pg_*` personal rows are never recreated.
        - **Hardening, final fix round:** parse the full SCRAM verifier structure, not just its prefix.
        - **For the Members step:**
          - a caller checks `is_marked_sql` before retrying after a lost `COMMIT`;
          - create the role and write its row under `lock_endpoint`;
          - the live test's "nothing is forwarded" comment proves nothing (`forward_ddl=off`); K6's Python tests cover forwarding.
  - **X5 DONE** (leaf; the coordinator checked that the RED diff is empty). Commit `bf806aab7`: `x5-binaries_test.sh` passes, `test_compute_metrics` 8/8 (`.ci-local/20261021-214533-v17-release`). Build-tools image `ghcr.io/chelabase/neon-build-tools:f3f0cc340753` was built locally in 13 min and **not pushed**. No sql_exporter config changes were needed for 0.24. **Merged into Neon `round-4` `aea60d6ef`.** Obsolete local images were removed; disk 77 %.
  - **C4 RED** `e1367305`: compile-RED except `config.rs::the_realtime_tls_dir_is_required`, which is runnable. A throwaway scaffold showed 12 orchestrator RED tests failing as intended, with 2 guards.
    - **Pins:**
      - `InternalState.{control_locator, realtime_logins}` and `InternalConfig.{realtime_login_cidrs, realtime_logins()}`;
      - realtime `ConnInfo.replication_host`, `Config.realtime_tls_dir`, a new `client_tls::ClientPair` (reloaded every 60 s);
      - `PgReplicator::new(lookup, client, password)`;
      - `chelabase-neon` becomes a realtime dependency.
    - **Plan correction:** neither realtime live file logs in directly as `chelabase_realtime` (only the replicator does), so SP9 has nothing to convert there. The runner's config now needs `CHELABASE_REALTIME_TLS_DIR` (C5).
    - **C4 implemented** `26497b62` + merge of C3 `451b09e8` (clean).
      - Tests: orchestrator + realtime 1174 passed, 0 failed; clippy clean; the live targets compile.
      - New edge file `c4_edges.rs`: a non-IP `replication_host` is `Unavailable`. A guard in `slots.rs` stops tokio-postgres resolving it as a hostname.
      - One lint-only RED edit in `c4_realtime_logins.rs`.
      - **C4 DONE.** The audit passed with 0 defects and accepted the lint edit. Tidy `0857668e`. **Merged into `neon-fork-round-4` `e530811c`.** Worktree and branch removed.
      - **Final fix round:**
        - (minor, test agent) the streaming live test also checks that the watcher gets `rt_big`;
        - (minor, test agent) `realtime_logs_in_only_with_its_certificate` matches the refusal text, not just `is_err()`;
        - (hardening) invalidate `ConnLookup` after `Unavailable` or a connect failure (`slots.rs:~310`);
        - (hardening) bound the hook's `status_report`/`push` calls below storcon's 10 s (`hooks.rs:116-126`);
        - (hardening) the hook checks the compute's identity, or takes `lock_endpoint`, between status and push;
        - (hardening) `ReplicationTarget`'s `Debug` must not print the password (`decoder.rs`, pre-existing).
  - **C5 RED** `e04c96de`:
    - CLI unit tests: 4 runnable RED, the rest compile-RED.
    - Three shell tests RED on today's files.
    - `compute-network-e2e.sh` is strict: `known_gaps=()`, exact 401s, the `:3081` case gone.
    - `metrics_scope_reads_metrics_only` is compiled. It needs `CHELABASE_LIVE_COMPUTE_CTL=<host>:3080`, which the plan didn't have, and the live pass must set it.
    - **Pins:** `mint_metrics_token(keys, now)`, `METRICS_TOKEN_FILE`, `chelabase neon mint-metrics-token`, `ensure_machine_keys` with a 7th argument, `MachineKeys.realtime_tls`, `ControlNet.realtime_ip`.
    - **Found:** `scripts/tests/stack-urls.sh` fails because C4's `realtime_role_live.rs` runs docker without `CHELABASE_COMPOSE_PROJECT`. The C5 coder fixes the harness call without changing any assertion.
  - **C5 implemented** `cac2fd5a`.
    - CLI 272/272, clippy clean, chelabase-neon green.
    - Four shell tests ok; `compute-network-e2e.sh` passes `bash -n`.
    - `realtime_role_live` now derives its stack from `CHELABASE_COMPOSE_PROJECT`.
    - Coder deviations under audit:
      - `stack-urls.sh` exempts C4's in-process `127.0.0.1` test lines;
      - `mint_metrics_token` is test-only, and the CLI uses `mint_metrics_token_in`.
    - **C5 DONE.** The audit found 0 defects and accepted the deviations. Its tidy `7f247b26` narrowed the `stack-urls.sh` exemption to the `"127.0.0.1"` rule only, and the script fails if a `c4_*` test turns live. **Merged into `neon-fork-round-4` `5746b5a0`.**
    - **C5 hardening, final fix round:**
      - (fork) `NEON_PROXY_TO_CONTROLPLANE_TOKEN` is still in the proxy's environment. Add a fork flag `--control-plane-token-file`.
      - A kept `metrics.token` isn't checked against the current key after a key rotation. Document this, or verify the token on `up`. A runner single-file mount keeps the old inode after a re-mint.
      - `mint-metrics-token` follows `CHELABASE_NEON_DIR`. Document this for stacks.
      - One `tests/common` helper for the stack name, instead of a copy in each test.
  - **ALL TASKS DONE.** **End-of-step whole-branch review (opus) running** over Chelabase `main...neon-fork-round-4`, Neon `main...round-4` and Postgres `REL_17_STABLE_chelabase...round-4-v17`. It also rewords the End of step and the removals list for the minimal-only scope.
  - **K10 DONE.**
    - **Postgres:** `round-4-v17` = `aa1551d611c` (merges K1 + K2).
    - **Neon `round-4`:** `5c6626adb` moves the submodule and `revisions.json` (v17 → `aa1551d611c`, version 17.11). `bfdb6aaf1` updates `FORK.md`: new "Postgres changes" and "Compute image" sections, and four extension entries. Head is `bfdb6aaf1`.
    - **Checks:** `ci-local --only build` passed (`<neon checkout>/.ci-local/20261021-221811-v17-release`), and `--only pg-check` passed (`…-222125-…`).
    - **Pending fork changes:** none. Every K/X task of this round is in `round-4`.
  - **C6 DONE:** merged into `neon-fork-round-4` at `8276951b`. The end-of-step review checks its spec-derived phrasings against the shipped code.
  - **Disk** was at 84 %. I pruned the unused build cache again (33 GB, the same action the user approved earlier), and it is now 73 %.
  - **Then:** C5 after C4 merges; then End of step.
  - **X4 (narrowed: timescaledb, pg_partman, pg_cron) test agent (sonnet) running** in clone `-x0`, branch `round-4-x4` at `3085f9ef3`.
  - **W5 started:** X0 test agent (sonnet), clone `-x0`, `round-4-x0` at `af2f8a3d1`. C1 starts when K9 finishes (at most 2 pipelines). Disk: K8's seeded volumes took it to 81 %; freed to 77 % (build cache prune, merged clones `-k3`/`-k5` removed; volumes `-k3` kept for K6). After K8 merges: K9 in clone `-k8`. **User ruling (2026-10-21), final fix round, K7 area:** no tenant token reaches any pageserver control-plane route. The eight routes the fix opened to the tenant's own token (shard_split, heatmap_upload, secondary/download, secondary/status, wait_lsn, location_config/:id, scan_remote_storage, break) **and** tenant delete and location_config PUT take the global scope (`PageServerApi`) only. Check first that nothing in neon_local, storcon or the Python tests calls them with a tenant token; update `test_storage_auth_edges.py` accordingly.
- **Resumed (2026-10-21, after the usage-window pause):** running (fresh agents, briefs from this plan): **K7 fix-round test agent (opus)** in clone `-k7` (`round-4-k7` at `d95e16d8c`, volumes `-k7`), and **K4 test agent (opus)** in clone `-k4` (`round-4-k4` at `96e7628d7`, volumes `-k3`). Next per lane: K7 fix coder (opus) → coordinator verifies → merge `round-4-k7`; K4 coder (opus) → audit (opus). **K7 fix RED** `d5a9678a1` + lint `a500bcd94` (`test_route_scopes.py`: route list parsed from each `make_router`, fail-closed parser, live probes; pageserver 42/156 and storcon 4/146 violations, safekeeper clean (guard); `http-utils/tests/auth_middleware_metrics.rs`; libpq `auth_tests` in pageserver and safekeeper; `utils/tests/auth_nbf.rs`; run `.ci-local/20261021-181412-v17-release`). Ruling for the coder: authorize before reading body or query. **K7 fix coder (opus) running.** **K4 RED** `c84d46e96` (`compute_tools/tests/chelabase_k4_{auth,realtime}.rs`, `compute_api/tests/chelabase_k4_api.rs`, Python `test_metrics_need_a_token`, `test_realtime_logins`; runs `.ci-local/20261021-18{1432,2930,3353}-v17-release`). **K4 plan corrections (test agent):** `update_pg_hba(pgdata, databricks, member_logins, realtime_logins, tls)`, `update_pg_ident(pgdata, databricks, member_logins, realtime_logins)`, `realtime_hba_block(Option<&MemberLogins>, bool)`, `realtime_ident_lines(Option<&MemberLogins>)`; ident realtime block right after the member map; every 401 carries the challenge; `Server::External.instance_id` kept, ignored by `Authorize`; round-3 layout tests (`chelabase_round3.rs`, `chelabase_f3_edges.rs`) and `test_chelabase_keys.py::test_keys_over_replication_and_restart` (adds test-only loopback `trust` lines for `chelabase_realtime`) adapted in the RED commit. **K4 implementation** `290df22e8` (Rust 1086/1086 with lint, `.ci-local/20261021-184804-v17-release`; regress 19 pass, 5 fail = K2's streaming tests in `test_chelabase_keys*.py`, `stream_txns == 0`, because the submodule stays at `76ce76278c5` until K10, not K4's; `.ci-local/20261021-185253-v17-release`). Coder deviations under audit: unmarked `chelabase_realtime` ident lines removed on first write; `HEAD` on the metrics routes opens with a metrics token; `Authorize::new` drops `instance_id`. **K4 audit (opus) running.** **K7 DONE, merged into Neon `round-4` `7912a1e3e`** (fix `cf558c0fc`, `e0d53801c`; coordinator verified the diff: `auth_middleware` refuses `metrics` off exactly `/metrics`; pageserver/safekeeper `api_handler` authorizes per route before the body/query (`RouteAuth::Path`/`TenantInBody`); storcon `check_route_scope` guard (exhaustive match) plus Admin on failpoints and PageServerApi on secondary download; libpq `Unauthorized`; `validate_nbf`; Rust 1032/1032, regress 38/38 `.ci-local/20261021-185100-v17-release`). Clone `-k7` and its volumes removed; disk 66 %. **Open question to the user:** tenant-named routes that had no check now take the tenant's own token (shard_split, heatmap_upload, secondary download/status, wait_lsn, location_config/:id, scan_remote_storage, break) vs global scope only. **Environment note:** one regress teardown failed on the pageserver's disk-pressure WARN at ~80 % disk (passed on rerun); disk now 66 %.
- **Latest (2026-10-21, later):** **K3 + K5 DONE and merged together (P3)** into Neon `round-4`: `775be987a` (K3), `96e7628d7` (K5; final run `.ci-local/20261021-171644-v17-release` green). K5 audit (tidy `4348ad826`), deferred to the final fix round: a transient `open`/`read` error (EMFILE/ENFILE/ENOMEM/EIO) is cached under the file identity and never retried until compute_ctl replaces the file (`token_file.c:280`, `:319`; keep the identity only for EACCES/EPERM); `curl_easy_escape` the extension name in the bearer-carrying URL (`extension_server.c:48`); `S_ISREG` on the `stat()` result before `open` (`token_file.c:273`); stale `compute/manifest.yaml:69` comment on `allow_unstable_extensions` (X0 or K10). `-k5` alias volumes removed. **K4 started** (test agent opus; clone `-k4`, `round-4-k4` from `round-4` `96e7628d7`, reusing the `-k3` volumes via `CHELA_NEON_VOLUME_SUFFIX=k3`). **PAUSED (user, 2026-10-21, until the usage window resets). Nothing is running; no ci-local container; clones `-k4` and `-k7` are clean (no uncommitted work).** To resume, restart from scratch (fresh agents, same briefs as the stopped ones): (1) **K7 fix round, test agent (opus)** in clone `-k7`, `round-4-k7` at `d95e16d8c`, `CHELA_NEON_VOLUME_SUFFIX=k7`: the scope-gap invariant (every route × {metrics token, foreign tenant token} → 403 except `/metrics`, route list proven complete; middleware refuses `metrics` off `/metrics`; libpq `expect`s → `Unauthorized`; `validate_nbf = true`), then its coder (opus), coordinator verifies, then merge `round-4-k7`; (2) **K4 test agent (opus)** in clone `-k4`, `round-4-k4` at `96e7628d7`, `CHELA_NEON_VOLUME_SUFFIX=k3` (reusing K3's volumes); then K4 coder → audit. Then W4: K6 ‖ K8 → K9; W5 onwards as the task table. Volumes on disk: `-k3` (used by K4), `-k7`, `chela-neon-sccache`; disk 76%.
- **Earlier (2026-10-21):** **K2 DONE** (audit 0 defects, tidy `d33f8bf75`; merged into Neon `round-4` `a763b5778`; Postgres `round-4-v17-k2` `83d34302dda` waits for K10). K2 deferred to the final fix round: a `max_message_bytes` flip-point test streamed vs not (`stream_edges.sql:96-115`); the wrong "as RT1 sets it" docstring in RED file `test_chelabase_keys.py:257` (test agent); if the 8192-byte memory bound flakes, measure only the `chelabase_keys stream context` subtree. **For C4's brief:** `a_big_transaction_spills_through_the_rt_slot` (`realtime_live.rs:1189`) becomes `…_streams_…` and its `spill_txns = 0` check must be limited to its own slot (`pg_replication_slots` joined on `database = current_database()`), or other tests' spills fail it. **CDLA exception** on `round-4` `5897dc18e` (webpki-roots only; deny licenses + advisories ok). **C9 done** (pingora `chelabase/advisories` `5811b62d`, clone `<pingora checkout>-c9`; Chelabase `0712c6b0`: tokio 1.54.0, h2 0.4.20, `aws-lc-rs` workspace dep; pingora 584/78/59 pass; Chelabase 1353 pass), audit 0 defects (tidy `5934ed2`, comments only). **C7+C8+C9 merged** into `neon-fork-round-4` `0518cdd8`; worktree `-c7` and clone `pingora-c8` removed (clone `pingora-c9` kept for the final fix round). Deferred to the final fix round (pingora, then a pin bump): `default_h2_options` sets `max_local_error_reset_streams(Some(MAX_MALFORMED_STREAMS_PER_CONN))` to keep the old 32-per-connection bound (`v2/server.rs:59-74`); `WriteLayer::resize` with pending bytes ignores an unbuffer request silently (`stream.rs:458`, `debug!` or flush first); dead `buffer_write == false` branches (`stream.rs:647`, `:982/1008/1019`). **K5 started** (test agent opus; clone `-k5`, `round-4-k5` from `round-4-k3` `9923d3bbd`). **K5 RED** `59e11b466` (`test_token_files.py` 8, `test_storage_token_reload.py` 6, `test_unstable_extensions.py` +1, helper `fixtures/token_files.py`; plus `test_remote_extensions[sql_only|with_lib]`); K5 implementation `c22242ac1` (after merging `origin/round-4`; `pgxn/neon/token_file.{c,h}`, `PGC_POSTMASTER` path settings, per-process cache keyed on dev/ino/size/mtim/ctim, fail-closed rules, readers at every connect, `allow_unstable_extensions` `PGC_SUSET`, `NEON_AUTH_TOKEN` gone; symlinks followed, as the plan and RED test require). **Process failure: 3 K5 RED test bugs** (pageserver auth-error allow-list misses the real line; `CREATE TABLE` in `public` refused for the owner on PG15+; a conf-load-time probe opens a new session under the expired token), fixed by the test agent in `4c49ab224` (19/19, `.ci-local/20261021-171355-v17-release`); K5 coder fixing a mypy error in its own edge file, then one final run of K5 + K3 tests and lints. **K7 implementation** `c72a6ff3c` (`Scope::Metrics`; total `check_permission`; `check_metrics_permission`/`check_profile_permission`; `PUBLIC_ROUTES` replace the allowlists; `JwtAuth::decode` refuses non-integer `exp`/`nbf`, `validate_exp = true`; fixtures; Rust 1018/1018, regress 40/40); **K7 audit** (tidy `d95e16d8c`): **1 important defect (pre-existing, security):** 13 pageserver HTTP handlers (`routes.rs` :1405, :2085, :2107, :2696, :2847, :2921, :2935, :3020, :3072, :3129; `testing`: failpoints, `handle_tenant_break` :2200, `post_tracing_event_handler` :3157) and storcon `handle_tenant_secondary_download` (`http.rs:396`) never check scope, so a `metrics` token or another tenant's token reaches them (cross-tenant). **Fixed now** (C5 and the compute-network e2e build on it), as an invariant (second scope gap in this area): every registered route × {metrics token, foreign tenant token} → 403 except `/metrics`, route list proven complete; plus the middleware refusing `metrics` off `/metrics`; folded into the same round: the two libpq `expect`s become `Unauthorized` (`page_service.rs:2812`, `safekeeper/src/handler.rs:395`) and `validate_nbf = true` (`libs/utils/src/auth.rs:150`). K7 fix test agent (opus) running. K2's `test_chelabase_keys_stream.py` fails `ruff format` on `round-4`: the K2 coder formats it. K5 test-agent notes: `--sync-safekeepers` isn't exercised at start when safekeepers are in sync (`compute.rs:1764-1771`), so the test runs it by hand; no reconfigure failpoint exists, so "refresh never fails a running compute" checks status, `pg_conf_load_time` and the postmaster PID. **Disk incident:** disk reached 88% (> the pageserver's 80% eviction WARN, which fails every regress teardown); freed to 73% by removing the merged K1/K2 volumes and pingora-c9's `target/`. **Rule from here:** a task's `chela-neon-{target,cargo}-<id>` volumes are removed as soon as its branch merges (P2); the `-k5` volumes are bind aliases of `-k3`'s. **K7 RED** `f6c966bdd` (Rust scope tables per server, `utils` Metrics serde, Python `test_auth.py`; new pinned names `check_metrics_permission`, `check_profile_permission`, `PUBLIC_ROUTES`); K7 coder (opus) running. **Plan correction (K7 test agent):** an expired or badly signed token gets 403, only a missing header 401 (`libs/http-utils/src/error.rs:101-107`). **K7 started early** (W3 lane B; it depends only on W0): test agent opus; clone `-k7`, `round-4-k7` from `round-4` `5897dc18e`, volumes seeded from `-k2`.
- **RESUME HERE (2026-10-21): W0 done; K1 DONE (merged into Neon `round-4` `a649d311e`; Postgres `round-4-v17-k1` `eaee50d10bc` waits for K10); K3 lint fix done: merge `c3fa4040c` (`round-4` into `round-4-k3`, no conflicts), mypy fix `9923d3bbd` (**process failure: a RED file failing lint**; no assertion changed); ruff format of K1's edge tests `b6132aee1` on `round-4`. **User ruling (2026-10-21): allow CDLA-Permissive-2.0 (webpki-roots 1.x, Mozilla's CA data) as a scoped `deny.toml` exception, as long as it doesn't break the Apache-2.0 posture** (it doesn't: a permissive data licence, no copyleft, its text kept in notices); a haiku agent adds it on `round-4`. Running: K3 fix round DONE (RED `988a81374`, fix `99f29e17a`: bind-before-launch, `[::1]` only EADDRNOTAVAIL/EAFNOSUPPORT tolerated, `with_internal_auth` route layer + kept extractor sharing one `check`; Rust 1043/1043, regress 11/11, `.ci-local/20261021-155534-v17-release`; coordinator verified the diff), K2 test agent fixing a RED expectation (**process failure:** `stream.sql` line 5 expected a rolled-back subtransaction's relation, which the SQL decoding API never delivers, `reorderbuffer.c:2683-2692`; the keep rule holds only for a subtransaction streamed while it ran, now a walsender test); K2 implementation Postgres `d663e4f7953`, Neon `b9818bb3e` (stream callbacks, per-xid state under `ctx->context`; regress 7/7, isolation 2/2, Python 10/10 with the probe line, `.ci-local/20261021-155740-v17-release`), C7+C8 audit (opus).** C8 done: pingora fork `chelabase/advisories` `12fc469b` (`derivative` → hand-written impls, `serde_yaml` → `serde_norway` 0.9.42), Chelabase `e31acd8e` on `neon-fork-round-4-c7` (rev bump); `just deny` ok with no ignores; `gateway` 170/170. **C7+C8 audit: 0 defects.** The 41 pingora `--lib` failures: 39 are **tokio 1.54.0** panicking on zero-capacity `BufReader`/`BufStream`, which pingora's l4 `Stream` uses for every accepted connection (`pingora-core/src/protocols/l4/stream.rs:549/572/596`, `listener.rs:88`; same in crates.io 0.9.0); Chelabase's `tokio = "1.53.1"` would take 1.54 on the next `cargo update` and panic every gateway connection; 2 are h2 0.4.20 rejecting duplicate `content-length` itself (outdated tests). **Added (coordinator ruling, obvious bug fix): C9** (opus; coordinator verifies): fix the fork's l4 `Stream` for tokio 1.54 preserving its unbuffered-until-`set_buffer` semantics, take tokio 1.54.x and h2 0.4.20, update the two h2 tests, destructure-all-fields in the hand-written impls (audit hardening 1), then Chelabase's pingora rev + tokio/h2 bump and `aws-lc-rs` as a workspace dependency (hardening 2), on `neon-fork-round-4-c7`. K2 RED: Postgres `round-4-v17-k2` `70d39aa2939`, Neon `round-4-k2` `51f2a7db6`. **K2 test-agent corrections:** the spec's two-session interleave case is an isolation spec (`specs/stream_interleave.spec`), not `stream.sql`; `limits.sql` pins `logical_decoding_work_mem = '64MB'` (pre-flight 3 missed that `logical.conf`'s 64kB would make it stream); the realtime walsender refuses `SHOW` and reserves `rt_` slots, so the control login uses slot `ctl_s`; a `get_changes` between PREPARE and COMMIT PREPARED moves `start_decoding_at`, so that test peeks. **K3 audit (0 defects; tidy `8706f9e89`).** Fixed now (K5 builds on them): `[::1]` bind tolerates only EADDRNOTAVAIL/EAFNOSUPPORT (`server.rs:216`); bind before launch, error from `run()` (`server.rs:213`); plus a `route_layer(from_fn_with_state(...))` on the served internal router so a route without the extractor fails closed (the audit accepted the extractor deviation with this layer added). **Deferred to the final fix round:** remove every `<name>.tmp.*` at start (`token_file.rs:72-86`); the `ALTER EXTENSION UPDATE` trusted check covers every version on the update path (`compute.rs:2867-2875`); a redacting wrapper for `ParsedSpec.endpoint_storage_token`; stale `docs/authentication.md:114-166` (`NEON_AUTH_TOKEN`). **C7 done** (`5d9b3fc8` on `neon-fork-round-4-c7`, not merged yet): `deny.toml` (advisories only, no ignores, `yanked = "deny"`), `just deny` in `just check`, `just deny-install` (cargo-deny 0.20.2); no `.github` in this repo, so no CI job; `yoke-derive` 0.8.4; `rsa` removed (test JWKS keys via `aws-lc-rs`). **Added (user, 2026-10-21): C8**, our pingora fork drops `derivative` and `serde_yaml` (→ `serde_norway`) on new branch `chelabase/advisories` from `210aa72` (clone `<pingora checkout>-c8`), then Chelabase's pingora rev bump on `neon-fork-round-4-c7`; C7+C8 merge together once `just deny` passes. K3 implementation `3713ad437` on `round-4-k3` (Rust 125/125; Python 13/15, the 2 failures are `test_download_extensions` `[sql_only]`/`[with_lib]`, waiting for K5's bearer; `.ci-local/20261021-153023-v17-release`). **K3 deviation under audit:** internal-server auth is a per-handler `InternalAuth` extractor instead of P5's router layer. **K11 + C0 DONE (coordinator checked the lockfile diffs):** Neon `round-4` `600e2edc7` (merge of `4c7b29f1e`; `cargo deny check advisories` ok, `ci-local --only build` green); Chelabase `neon-fork-round-4` `285aa5c3` (merge of `f9d5bdf8`, `4a23f250`; also `webpki-root-certs` 1.0.10 for RUSTSEC-2026-0383; `cargo check --workspace --locked` green). Clone `-k11`, its volumes and the `-c0` worktree removed. **Added (user, 2026-10-21): C7** (sonnet, one agent, no review; worktree `.worktrees/neon-fork-round-4-c7`): a Chelabase `deny.toml` with the fork's shrink-only ignore policy, `just deny` in `just check` plus a scheduled CI job, and fixes for the current findings (an `rsa` ignore only as a proposal for the user's approval); running. Chelabase had no `deny.toml`/advisory check; a default-config `cargo deny` flagged RUSTSEC-2024-0388 (`derivative`, unmaintained), RUSTSEC-2023-0071 (`rsa`, Marvin, no fix), RUSTSEC-2024-0449 (`unsafe-libyaml`, unmaintained), a yanked `yoke-derive`. **Added (user, 2026-10-21): K11 + C0**, `chelabase/neon`'s scheduled `advisories` run failed (RUSTSEC-2026-0381 `webpki-roots`, RUSTSEC-2026-0382 `rustls-webpki`, CVE-2026-111943); Chelabase's lockfile is affected too. K11: Neon clone `-k11`, branch `round-4-k11`; C0: worktree `.worktrees/neon-fork-round-4-c0`, branch `neon-fork-round-4-c0`; `rustls-webpki` 0.103.16, `webpki-roots` 1.0.10 (0.26.11 shim); no ignore entries; the coordinator merges both and checks the diff (mechanical, no review). **Deferred to the final fix round (K1 audit, 0 defects):** (a) the basebackup skips a malformed slot aux entry with a warning (shorter than 16 bytes, wrong magic/version, `16 + length != len`, CRC32C mismatch) instead of shipping it into a compute PANIC (`basebackup.rs:493`; pre-existing; a corrupt entry today blocks every primary start of the branch); (b) after a postmaster crash-restart, record discarded names only on a basebackup (`neon.signal`) start, so never-copied local temp slots get no spurious deletes (`slot.c:2466`; low, `restart_after_crash` is off); (c) tests: a replica start + promote asserting no "removing non-existing aux file", and a persisted slot copied exactly once. K3 RED: Neon `round-4-k3` `34f44ccfc` (Python internal-API tests; Rust `chelabase_k3_*` in compute_api, compute_tools, control_plane); warm volumes `chela-neon-{target,cargo}-k3` (P2). **Plan corrections (K3 test agent):** `test_download_extensions` can't pass in K3 (its `:3081` call gets the bearer only in K5) → it moves to K5's verify; `test_role_grants.py` now creates its schema as a non-superuser (RT4-6). K1 implementation: Postgres `round-4-v17-k1` `f8dfd7c9f53`, Neon `round-4-k1` `b5bf528e1` (Rust 337/337, Python 5/5, `.ci-local/20261021-145016-v17-release`). **Open (K1 coder):** a pageserver slot aux entry shorter than 200 bytes is still shipped and PANICs the compute at start (pre-existing); the audit assesses, fix due in the final fix round unless it says otherwise. K1 RED: Neon `round-4-k1` `ae58900ec` (Rust `slot_state_tests` in `basebackup.rs`; Python `test_temp_slot_aux_files.py`, 3 RED + 1 guard). **Correction (K1 test agent):** spec §2 (`:64`, `:80`) and K1's case-4 gotcha say a stale aux entry is inherited by child branches; it isn't (aux files are in `NON_INHERITED_RANGE`, `libs/pageserver_api/src/key.rs:818-833`), so case 4 is a passing guard; the live test `compute_fork_live::a_crashed_compute_leaves_no_slot_aux_file` (C-tasks) must not assume inheritance. The user gave the go on the plan (2026-10-21, asked explicitly); P3, P6, P9 and SP13 listed to the user. W0: `chelabase/neon` `round-4` = `629166cee` and `chelabase/postgres` `round-4-v17` = `76ce76278c5` pushed; clones `<neon checkout>-k3` (`round-4-k3`) and `<neon checkout>-k1` (`round-4-k1`) made, origin reset; submodules not yet initialised (each task does it per P1). No stack is up (P6).
- **Pending fork changes:** none yet (every K/X task adds its branch here until K10 merges it).
- **Deferred findings (final fix round):** none yet.
- Branch `neon-fork-round-4`, spec `b4c32da0`, plan `<this commit>`. Fork branches (to be cut in W0): `chelabase/neon` `round-4` from `main` `629166cee`; `chelabase/postgres` `round-4-v17` from `REL_17_STABLE_chelabase` `76ce76278c5`.

## Final fix round (2026-10-21)

**User ruling (2026-10-21):** fix **everything listed**, polish included. **Time it.** The clock started at **22:54:13 +07**. It is paused only while the coordinator waits on a user answer; record each pause here.

**Pauses:** none (every user question was answered while work continued).

**`just check` GREEN (02:23, after the Studio `personal` label fix `634b5ede`; the first run only failed Studio typecheck: 3748 Rust tests passed). Fork gate default run 1: lint failed on clippy `unusual_byte_groupings` in F3's `basebackup.rs` (F3 never ran lint — process failure); fixed `bb553083c`; build and Rust tests passed. Run 2: lint failed on ruff in F3's `test_f3_coder_edges.py`; fixed `016fc32a3` (ruff, format, mypy clean repo-wide); Rust 927/927. Regress selection run 1 (`.ci-local/20261022-023159-v17-release`): 551 passed, 2 failed, 21 errors, nearly all from the pageserver disk-pressure WARN at 83 % disk. Freed to 76 % (removed merged clones `-f1,-f3,-f6,-x0,-k1,-k2`, obsolete images, dangling anonymous volumes). **Default gate PASS on `016fc32a3`** (lint, build, Rust; `<neon checkout>/.ci-local/20261022-030307-v17-release`). The 23 failed/errored tests rerun at 77 % disk: 19 passed, 2 still fail consistently (`test_compute_termination::test_compute_terminate_empty`, `::test_compute_empty_spec_during_refresh_configuration`, `…-030926-…`). Debug: the test was stale; K4 removed the `INSTANCE_ID` bypass on purpose, and the test polled `/status` and `/terminate` without a bearer. Fixed in the test `c1dd851c9`, which now signs a compute token and adds a 401 check; 28/28 pass (`…-031620-…`). **Fork gate DONE: `round-4` = `0741b56de`.** Pending fork changes: none. **Images built** (local, not pushed; ~13 min): `ghcr.io/chelabase/neon-{storage,compute-v17}:0741b56deb67-dev`, retagged `chelabase/neon-*`. **Minimal extension harness PASS** (timescaledb, pg_partman, pg_cron). `NEON_REF` = `0741b56de…` (`808010cd`). Stack `neon-fork-round-4` up (1 m 25 s) on the new images; `create-db-demo` done.**

**LIVE PASS (03:47–04:24, logs in the coordinator's scratch, summary below):** compute-network-e2e PASS; compute_fork_live 11/11; realtime_role_live 4/4; realtime_live 19/19; storage_auth_edges 1/1 (ignored live) + storcon 1/1, proxy_cache 1/1, valkey_acl 8/8, docker_backend 3/3; member_roles_live 30/30; roles_live 42/42; neon_live 1/1; rest_live 38/38; orchestrator api 30/30; metadata live_db 191/191; e2e neon, data, sql-editor, role, branch, console, storage-leak all PASS. `storage_auth` has no ignored tests (its 9 run in `just check`; the plan's `--run-ignored only` line for it is wrong). **One failure:** `branch_live::branch_right_after_a_pageserver_restart_sees_the_last_write` could not produce its stale-head precondition (`branch_live.rs:870`), and the other 3 `branch_live` tests were cancelled. Debug agent (opus) running in `.worktrees/neon-fork-round-4-dbg3`.
- **Cause:** the precondition was impossible. A SIGTERM restart freeze-and-flushes the pageserver, so its LSN always equals the safekeepers' commit LSN.
- **Fix `5608feb8`:**
  - The test stops the storage broker and restarts the pageserver, so w2 commits without reaching it.
  - It reads `commit_lsn` from each safekeeper directly (new `SafekeeperSet::client`) and fails loudly on a refused read.
  - Tenant delete retries the post-restart 409.
- **Disk incident:** the host filled to 100 %, which broke the stack's SeaweedFS mid-run. Freed to 75 % by pruning the build cache (21 GB) and removing the merged-task `chela-neon-*-k8` volumes. The rerun then failed all 4 tests with "tenant Attaching" 503s; the debug agent is checking stack recovery.
- **User ruling (2026-10-22), next step:** right after a pageserver restart, storcon answers tenant create/delete with 409 "No pageserver found matching constraint" for ~5 s, and the worker treats it as final. The worker should retry that 409 with backoff. This is a known gap carried to the next step.

**FINAL FIX ROUND DONE at 02:09:45 +07: 3 h 15 min** (the coordinator first estimated 8–12 h, then revised to 1.5–2.5 h more at 00:06; actual from 00:06 was 2 h 4 min). Integration: Postgres `round-4-v17` = `7812d66fe85`; Neon `round-4` = `b30afbf81` (merges f1 `883dc3519`, dbg `1a6a54b92`, f6 `ad49f242e`, f3 `849ecb702`; pin `25aabf02e`; FORK.md `b30afbf81`); `ci-local --only build` PASS (`<neon checkout>/.ci-local/20261022-020409-v17-release`), `pg-check` PASS (core 225/225, chelabase_keys regress 7/7, isolation 2/2; `…-020911-…`). Chelabase step branch `5b3ebd78`. **Pending fork changes: none.** Next: `just check` once, fork gate, minimal image build, `NEON_REF`, stack, live pass.

**F1 RED** `e084fbc22` (12 global-only routes RED 12/12; compute_ctl, proxy and gate RED; no caller uses a tenant token on the ruled routes; pins `RouteAuth::{Path, Global}`, `remove_stale_temp_files`, `--control-plane-token-file`, `lint_proxy_default_features`).

**F1 implemented** `e8049c549`:
- **Results:** gate 1202/1202 (`.ci-local/20261021-235134-v17-release`); regress 52 passed; `ci-local_test.sh` passes.
- **Changes:** a new redacting type `EndpointStorageToken`; the proxy gains `libc`.
- **Left for the audit to decide:**
  - `ComputeSpec`'s Debug prints `cluster.settings`;
  - `postgresql.conf` is written before ident/hba, with no rollback;
  - the safekeeper `TenantInBody` route.

**Running:**
- F1 audit (opus);
- F3 test agent (opus), clone `-f3`, `round-4-f3` from `bfdb6aaf1`;
- **F1 audit:** 1 important defect, all deviations accepted, tidy `69060f29b` (a doc comment).
  - **Defect:** `CREATE EXTENSION … VERSION` checks only the target's `trusted`, while PG17 runs the closest install script plus the update chain as superuser. Fix as an invariant: every script that CREATE or ALTER runs must be under a trusted control file.
  - **Fix now, auditor's decisions:**
    - A spec carrying `neon.endpoint_storage_token` in `cluster.settings` is refused. It would become a placeholder GUC every role can `SHOW`.
    - Restore the old `postgresql.conf` when the ident/hba write fails. The TLS watcher reloads on its own.
    - **User ruling (2026-10-22):** the safekeeper's `POST /v1/tenant/timeline` is global scope only.
  - **Hardening:** `--control-plane-token-file` with an auth backend that doesn't use it is an error.
  - **F1 round 2 DONE** `cfe27b483`. The coordinator checked the report and the diff summary.
    - **Results:** default gate 1208/1208 (`.ci-local/20261022-001923-v17-release`). Every round-2 RED test passes.
    - **Changes:**
      - `extension_paths.rs` `ScriptGraph` checks the whole install and update chain for trust.
      - The setting is refused.
      - `postgresql.conf` is rolled back on a failed write.
      - A `config_files_lock` is shared with the TLS watcher.
      - The safekeeper route is global-only.
    - **Open, debug agent (opus) running:**
      - (A) **DONE, on branch `round-4-dbg`** (from `round-4-f1`). The failures were already on fork main `629166cee` (`.ci-local/20261022-010829-v17-release`); round 4 didn't cause them.
        - Cause: since `46ca4065a` (aws-sdk-s3 1.152 default checksums), streamed PutObject is sent in multiple aws-chunked chunks. moto 5.0.6 decodes only the first chunk, so mock-S3 WAL segments come back corrupt.
        - Fix `d49a1e721`: moto 5.2.4 (latest). The relock also dropped the unused jsondiff and setuptools.
        - All 6 cases pass (`…-012753-…`). The gate runs the remaining mock_s3 tests.
      - (B) **DONE:** no real flow broke, but `/extensions` refused a remote extension whose files weren't downloaded yet.
        - Fix: RED `8ad6b4872`; fix `2196e699e` downloads first when the spec lists the extension as remote.
        - Result: 16/16 pass (`…-013752-…`).
        - Control files that set `directory` are still refused (fails closed; none exist).
      - (earlier) (A) `test_wal_acceptor::test_timeline_copy[100000|500000]` and `test_s3_wal_replay[mock_s3]` fail identically on `round-4-f1` and on the round-4 base `bfdb6aaf1`.
      - (B) Do remote extensions not yet downloaded now get refused by `ScriptGraph`? Trace the flow and fix if needed.
  - (earlier) **F1 round 2:** RED `423a94d24` (the trusted-scripts invariant fails on exactly 2 CREATE cases; the setting, rollback, token-file and safekeeper tests are RED; no caller of the safekeeper route uses a tenant token). Coordinator ruling: the TLS watcher reload and the config writes share one config-files lock. Coder running.
  - **Clock pause:** the safekeeper question (work continued while it was open, so this pause is ≈0).
- **F2 implemented** `b4794c83`.
  - **Tests:** 2528 passed and clippy is clean.
  - **Hook:** it reconfigures endpoints concurrently, each capped at 8 s. The control lookup has a 3 s cap.
  - **Docs:** the metrics token behaviour after a key rotation, and `CHELABASE_NEON_DIR`, are documented in the Compose comment and the CLI help.
  - **Deviation:** `apply_resync` turns `personal` into `sql` when the resync says `sql`. `live_db.rs` was changed to match, so the live pass must run it.
  - **Process failure (test agent):** RED `f2_edges.rs` failed `stack-urls.sh` because it used `127.0.0.1`. The coder fixed it with a lint edit.
  - **F2 audit:** pass, 0 defects. All 5 deviations accepted. Tidy `13295d52`.
  - **F2 round 2 (coordinator ruling: the coder does it with TDD, no separate test agent for these small items):**
    1. The resync DELETE excludes `personal`.
    2. A reserved-name personal row without SCRAM is recorded as `Sql`.
    3. A `PoolTimedOut` lock wait counts as skipped.
    4. `libc::ELOOP`.
    5. Escape control characters in the error body.
    6. Cap the hook's concurrency.
    7. Run the hook tests on paused time.
    8. `same_dir` compares canonicalised paths.
  - **F2 DONE.** Round 2 `17713b8a`: all 8 items. 2566 tests passed; `internal.rs` hook tests run on paused time (3.8 s, down from 8.9 s); the hook caps concurrency at 16 with a 9 s deadline. The coordinator checked the diff, including the personal-row exclusion in the DELETE. **Merged into `neon-fork-round-4` `cc61fed8`.**
    - **Live pass must run:** `live_db.rs::a_personal_origin_round_trips…` (edited) and `a_resync_never_removes_a_personal_row` (new).
  - **F4 DONE.**
    - pingora `chelabase/advisories` `c199b4ed`: the h2 local error-reset bound is back to 32, `resize` flushes before unbuffering, and the dead branches are gone. pingora-core 586 passed.
    - Chelabase pin bump `5d98d2dd`: the lock changes only pingora lines (checked by the coordinator). gateway 170 passed, clippy clean, `just deny` ok.
    - **Merged at `988c0ff7`.**
  - **F5 DONE**, merged at `6af8cba0`.
    - Docs accuracy fixes.
    - Compose passes `--control-plane-token-file`, so the proxy's token is out of its environment.
    - `compose-secret-argv.sh` fails on any entrypoint exporting a `/run` secret. Storcon and pageserver were allowlisted until F6.
    - Two stale `infra/neon/tests/run.sh` checks fixed. The hook-token check was already failing on `main` since `e48e0d7f`; the check was wrong, not the Compose file.
  - **F6 (user ruling 2026-10-22): no secret in any service's environment.**
    - Storcon gets `--{pageserver,safekeeper,control-plane}-jwt-token-file`, and the pageserver gets a token-file flag for `NEON_AUTH_TOKEN`. Both use F1's reader, moved into a shared crate.
    - Compose drops the exports, and the allowlist is removed.
    - Lane: fork clone `-f6` from `round-4-f1` `cfe27b483`, worktree `neon-fork-round-4-f6`. RED: fork `a4f157609`, Chelabase `e8855aa1`. Pins: `utils::secret_file::read_secret_file`, storcon `--{pageserver,safekeeper,control-plane}-jwt-token-file`, pageserver `--auth-token-file`. The Compose check now also catches the split `NAME=$(cat /run/…); export NAME` form. The valkey healthcheck's `VALKEYCLI_AUTH` prefix is accepted (a healthcheck process, not a service). Coder (opus) running.
    - The user's questions took no pause: work continued while they were open.
    - **F6 implemented:** fork `00bfeea4e`, Chelabase `67e50470` (**merged at `5b3ebd78`**).
      - Tests: utils 108 + `secret_file` 10, storcon 9, pageserver 8, proxy 30; clippy clean; 3 shell tests pass. The coordinator checked the Compose diff.
      - **Follow-ups DONE** `feb772c00` (storcon 10/10; pageserver and storcon clippy clean with and without `testing`): a non-UTF-8 storcon `*_JWT_TOKEN` next to its file flag counts as a conflict; the pre-existing `pageserver --all-targets` clippy failure without `testing` (`tenant.rs:9707` `GcInfo`).
  - **F3 RED** `26a9232a0` (run `.ci-local/20261022-002547-v17-release`).
    - K1 malformed-entry tests, K5 transient/FIFO/escape tests, K6 H1/H3 tests, the X0 cases, the D1/D6 scripts: all RED. K2 flip-point and the other guards pass. H4 skips on PG15.
    - **Correction:** `neon.signal` is never unlinked, so K1(b) can't use it. **Ruling:** discriminate on the control file state at start. Untestable here (`restart_after_crash=off`); reasoned in a comment.
    - **Process failure (cross-task):** K10's FORK.md broke X0's removal grep. The test now excludes FORK.md.
    - **F3 implemented.** Neon `57adbaa4f` + `ea658a52f`; Postgres `round-4-v17-f3` `14ed3e95281`.
      - Checks: regress 56/56 (`.ci-local/20261022-012212-v17-release`), pageserver Rust 346/346, pg-check passing, shell tests 43/7/20. All three scripts are now in `ci-local`'s lint self-tests.
      - K1(b): `StartupReplicationSlots(from_shutdown_state)` reads the control file's state before recovery.
      - **Process failure (test agent):** the directory case of the non-regular-file test never ran at RED. `clear_buffers` restored the LFC size before any backend shrank it, and the coder fixed the fixture (`neon_fixtures.py`).
      - **FORK.md describes F1's code** (`cfe27b483`), so it's correct only once F1 merges.
      - **F3 audit:** pass, 0 defects, all deviations accepted. Tidy `8788424ee` (FORK.md wording).
      - **F3 round 2 (the coder does it test-first):**
        - `compose down --remove-orphans`;
        - validate `--only` before any build;
        - the no-cursor `clear_buffers` path gets the same wait, plus the string-vs-int fix;
        - a real curl handle;
        - the clean-shutdown killed-walsender case in `slot.c`.
      - **F3 round 2 DONE:** Neon `d67fdaafe`, Postgres `1a1870f5221`; run `.ci-local/20261022-015418-v17-release` all pass; shell tests 45/45.
        - **K1(b) exact:** a basebackup start = `DB_SHUTDOWNED` with `checkPoint == 0`. Shown RED by a clean-shutdown restart test.
        - **Process note:** the shared k8 target volume reused another clone's build once. Integration and the gate touch sources first. For the process trim: one volume per clone, or a clean build for the gate.
      - **Integration agent (sonnet) running:** merges f1, dbg, f6 and f3 into `round-4`; Postgres `round-4-v17-f3` into `round-4-v17`; the pin and `revisions.json`; FORK.md for F6 and the debug fixes; then `ci-local --only build`, `pg-check`.
      - (earlier) **Pending fork changes for integration:**
        - Postgres `round-4-v17-f3` (+ round 2) goes into `round-4-v17`, then the Neon pin and `revisions.json`;
        - `round-4-f1`, `round-4-f3`, `round-4-f6` and the debug branch go into `round-4`.
  - **Process note:** F3's and F1's `ci-local` runs overlapped once, because two agents passed the `pgrep` check together. Memory was fine. For the process trim: one lock file for heavy builds, and prove shell or doc-only checks by running them directly, not through `ci-local`.
  - **For the Members step:** a role made by `create_personal_login` before its row exists becomes `sql` at the next resync. Members must write the row first, or under `lock_endpoint`.
  - **Clock:** 1 h 12 min elapsed at 00:06. The coordinator's 8–12 h estimate was badly off: it summed padded per-task numbers serially. Revised to 1.5–2.5 h more for the fix round.
- (earlier) F2 coder (opus), after F2 RED `067fb3d9` (live fixes) + `60272356` (RED); rulings: the storcon hook reconfigures endpoints concurrently to answer within 10 s overall; a same-address restart is accepted as undetected (the refresh loop corrects within its interval). F3 starts after F1, F4/F5 after F2.

The end-of-step review (opus, 2026-10-21) found no critical defects. Pipelines run as test agent → coder → audit, with models by area. Lanes:

**F1: fork Rust (opus / opus / opus).** Clone `-f1`, branch `round-4-f1` from `round-4` `bfdb6aaf1`, volumes `-k8`.

1. **Storage auth (user rulings).** These pageserver routes take global scope (`PageServerApi`) only, never a tenant token:
   - `shard_split`, `heatmap_upload`, `secondary/download`, `secondary/status`, `wait_lsn`, `location_config/:id`, `scan_remote_storage`, `break`;
   - tenant delete, `location_config` PUT;
   - `/v1/tenant/config` PUT/PATCH.
   
   First check that nothing in neon_local, storcon or the Python tests calls them with a tenant token. Update `test_storage_auth_edges.py`.
2. **compute_ctl:**
   - Remove every `<name>.tmp.*` at start (`token_file.rs:72-86`).
   - The `ALTER EXTENSION UPDATE` trusted check covers every version on the update path (`compute.rs:2867-2875`).
   - Add a redacting wrapper for `ParsedSpec.endpoint_storage_token`.
   - Fix stale `docs/authentication.md:114-166` (`NEON_AUTH_TOKEN`).
   - Write `pg_ident` before `pg_hba`, or roll back, so a failed ident write never leaves new hba with old ident (`config.rs:146-148`).
   - `plan_pg_hba` moves a hand-moved realtime block back after the member block.
   - Stop logging one `warn!` per JWKS key on every failed token (`authorize.rs` `verify`).
   - Advertise `storage_token_refresh` only when the compute holds a storage token (SP6).
3. **Proxy:**
   - Add `--control-plane-token-file`, so `NEON_PROXY_TO_CONTROLPLANE_TOKEN` is off the environment.
   - `read_token`: open with `O_NONBLOCK` and require a regular file.
   - A rate-limited `warn!` on an invalid token file.
   - `plain_redis_connection_info`: `redis`/`rediss` only.
   - Reject control characters in the password.
   - Error when `--redis-plain-password-file` is given without `plain` auth.
   - Add a default-feature `cargo check -p proxy` to the fork gate.

**F2: Chelabase (sonnet test / sonnet coder / opus audit; resync security parts get an opus test agent).** Worktree `neon-fork-round-4-f2`.

1. **Live-test defects (review D2–D4):**
   - `realtime_role_live`: keep only the replication positive; the negatives match the hba/certificate refusal text.
   - `compute_fork_live` `a_crashed_compute…`: filter slots on `rt_aux`.
   - `metrics_scope_reads_metrics_only` finds a running compute itself.
2. **Worker:**
   - Prune `storage_token_failures` entries for gone endpoints.
   - Each loop pass survives a panic.
   - Skip a capable compute that isn't `running`/`configuration_pending`.
   - Refuse `CHELABASE_REALTIME_TLS_DIR` == `CHELABASE_COMPUTE_TLS_DIR`.
   - Take `now` per endpoint.
   - Cap the refresh interval at TTL/2.
   - Put a timeout on the live test's `docker exec`.
   - Bound the refresh pass's `lock_endpoint` wait: skip and count.
   - **Resync (opus test):**
     - `ObservedRole.marked`; never adopt an unmarked role as `Personal` (`resync.rs:886-890`).
     - The recreate filter uses `is_reserved_target`.
     - Parse the full SCRAM verifier.
3. **Orchestrator/realtime:**
   - The streaming live test also checks that the watcher gets `rt_big`.
   - `realtime_logs_in_only…` matches refusal text.
   - Invalidate `ConnLookup` after `Unavailable` or a connect failure.
   - Bound the hook's `status_report`/`push` below 10 s.
   - The hook checks compute identity, or takes `lock_endpoint`, between status and push.
   - `ReplicationTarget` `Debug` never prints the password.
   - Add a timeout to `data.rs:135`'s Docker lookup, or compute it only when asked.
4. **chelabase-neon/CLI:**
   - Cap compute_ctl's error body at 512 bytes in our errors.
   - Treat ELOOP as absent in `renew_client_pair`.
   - Tolerate `capabilities: null`.
   - Drop `SpecInputs.storage_token_expiring` (test agent).
   - Check a kept `metrics.token` against the current key on `up` (and recreate the runner after a re-mint), or document it.
   - Document that `mint-metrics-token` follows `CHELABASE_NEON_DIR`.
   - One `tests/common` helper for the stack name.

**F3: fork C, harness, FORK.md (opus / opus / opus).** Clone `-f3`, after F1 (one heavy build at a time).

1. **K1:**
   - The basebackup skips a malformed slot aux entry with a warning (`basebackup.rs:493`).
   - After a crash-restart, record discarded names only on a `neon.signal` start (`slot.c:2466`).
   - Tests: replica start + promote; a persisted slot copied once.
2. **K5:**
   - Keep the file identity only for EACCES/EPERM; transient errors are retried (`token_file.c:280`, `:319`).
   - `curl_easy_escape` the extension name (`extension_server.c:48`).
   - `S_ISREG` before `open` (`token_file.c:273`).
   - Fix the stale `manifest.yaml:69` comment.
3. **K6:**
   - **(H1, user ruling)** Refuse `COMMENT ON ROLE`/`SECURITY LABEL ON ROLE` on a marked or managed role for non-superusers.
   - **(H3)** `default: break;` → `Assert(false)`.
   - **(H4)** Version-skip the `ON PARAMETER`/ADMIN tests below PG16.
4. **K2:**
   - A `max_message_bytes` flip-point test.
   - Fix the wrong docstring (`test_chelabase_keys.py:257`).
   - If the 8192-byte memory bound flakes, measure only the `chelabase_keys stream context` subtree.
5. **X0:**
   - `ONLY` entries match `^[A-Za-z0-9._-]+$`, rejecting `.` and `..`.
   - The wrapper's `cleanup` runs `docker compose down`.
   - Add a `--help` note on the `neon:<tag>` retag.
6. **Review D1:** remove the committed `docker-compose/pageserver_config/tenants/…` runtime state and gitignore it.
7. **Review D6:** complete `FORK.md` and correct its two wrong statements.

**F4: pingora (sonnet; coordinator verifies).** Clone `pingora-c9`, then the pin bump on the step branch:
- `default_h2_options` sets `max_local_error_reset_streams(Some(MAX_MALFORMED_STREAMS_PER_CONN))`;
- `WriteLayer::resize` flushes first or logs with `debug!`;
- remove the dead `buffer_write == false` branches.

**F5: docs (haiku; leaf):**
- Review D7: update `realtime.mdx` "Known gaps" and `neon.mdx:70`, `:78`, and `security.mdx:38`.
- Review D8: the `security.mdx` phrasings, `infra/neon/README.md`, and the `compute-network-e2e.sh` comment.

**Plan (coordinator):** review D5 (gate `-k`) and the End of step / removals rewording, applied below.

**Upstream PR candidates (user asked 2026-10-22; follow-up after this round, nothing opened without the user's go):**
- **Pingora:**
  - C9's l4 `Stream` fix: tokio 1.54 panics on zero-capacity `BufReader`/`BufStream`, which every accepted connection uses (also in crates.io 0.9.0). Strong candidate.
  - F4's `max_local_error_reset_streams` bound under h2 0.4.20, together with the two h2 test updates.
  - C8's `derivative` removal. Offer the `serde_yaml` → `serde_norway` switch as a separate PR.
  - Optional cleanup: the `WriteLayer::resize` and dead-branch tidy.
- **rust-postgres** (pinned `chelabase/replication`):
  - the transaction-status fix;
  - the typed error accessors;
  - CopyBoth/replication, after checking upstream's open proposals.
- **Process:** check upstream first, then cut one fix per branch from upstream's base.

**Recorded for later steps:**
- `extensions-all` must not be published before X3/X6 (it still ships pg_duckdb 0.3.1 with the privileged-role workaround).
- The expired-token live test doesn't show session recovery; the fork's Python tests cover it.

## Plan rulings (mechanical, listed for the user)

- **P1** Fork task checkouts are clones, never worktrees (`ci-local.sh` refuses `git worktree`): `<neon checkout>-<id>` (from the local clone, origin reset to `https://github.com/chelabase/neon.git`), Neon branch `round-4-<id>`, `CHELA_NEON_VOLUME_SUFFIX=<id>`. Postgres work happens in that clone's `vendor/postgres-v17` (initialised with `--reference <postgres checkout>`), on branch `round-4-v17-<id>`. **Neon task branches never commit the submodule pointer** (it stays at `76ce76278c5`; the clone's checkout differs locally); K10 moves it once. Cost if wrong: submodule conflicts at every merge.
- **P2** Volumes: only `chela-neon-sccache` exists. The first fork task of W1 (K3) builds cold; every later clone's `chela-neon-{target,cargo}-<id>` is seeded from the warmest one before its first run (round 3 P14b: `docker run --rm -v chela-neon-target-<warm>:/a -v chela-neon-target-<new>:/b <build-tools image> cp -a /a/. /b`). A clone and its volumes are removed after its branch is merged and pushed (nothing unpushed). Host disk stays under 80 % for fork regress runs (round 3's lesson: pageserver disk-pressure warnings fail tests). Cost if wrong: ~25 GB per seeded volume.
- **P3** K3 and K5 merge into `round-4` together: K5 branches from `round-4-k3` after K3's review fixes; both land after K5's audit. Between them `round-4` would start computes without a storage token (K3 drops `NEON_AUTH_TOKEN`, K5 reads the file), breaking every auth-enabled Python test. Cost if wrong: K4/K7 RED runs fail for the wrong reason.
- **P4** Token-file paths when the compute has no token: compute_ctl writes `neon.storage_auth_token_file = ''` (empty = no token configured, no file, no warning) when the start spec has no `storage_auth_token`; `neon.compute_ctl_token_file` always names the file (the secret always exists). The reader treats an empty path as "no token" silently and a configured path that fails as fail-closed with one `WARNING` per state (§4.1). Cost if wrong: one warning line per backend on auth-off test setups.
- **P5** compute_ctl's internal server binds `127.0.0.1` and best-effort `::1` with **two listeners** (axum serves one listener per task); the secret check is one middleware layer on the internal router; `/refresh_configuration` and `/failpoints` are behind it too. Cost if wrong: none.
- **P6** The plan's one stack comes up at End of step 4, after the image build and the `NEON_REF` pin, not at plan time: every live test this round adds or changes needs the new images (round 3 P6). No task runs a live suite; live RED tests are compiled with `--no-run` and traced by reading. Cost if wrong: none.
- **P7** The extension-test harness gets an `ONLY=<dir,…>` selection (X0) and a wrapper `docker-compose/chelabase-ext-test.sh` that builds `extensions-all` images from a checkout and runs the harness on them, because `publish-dev.sh` hard-codes `EXTENSIONS=minimal` (`publish-dev.sh:228`) and the harness expects `${REPOSITORY}/neon:${TAG}` and `neon-test-extensions-v17` images (`docker-compose/docker-compose.yml:32`, `:200`). Extension tasks run only their own directories through it; the End-of-step build runs it whole, with the storage image `publish-dev.sh` built from the same commit. Cost if wrong: a second storage build.
- **P8** Dockerfile ownership: X0 does every removal (§17.1 and `pg_mooncake`) and every edit to the shared regions (the `extensions-minimal`/`extensions-all` `COPY` blocks, the `extension-tests` stage's `COPY` lines, the final stage's runtime packages except SFCGAL's, `manifest.yaml` entries of removed extensions, `docker_compose_test.sh`, `test_extensions_upgrade.sh`, `ext-src/README.md`). X1–X5 then edit only their own stages, their own `manifest.yaml` entries, their own patches and `ext-src` dirs, and each merges into `round-4` after X0; a conflict in a shared region goes back to the later task. X2 owns the final stage's `libsfcgal1` line (pre-flight 12). Cost if wrong: merge conflicts resolved by hand.
- **P9** The harness skips `timescaledb-src`, `rdkit-src`, `pg_jsonschema-src` today (`docker_compose_test.sh` `SKIP=`; also stale `kq_imcx-src`, `wal2json_2_5-src`). The spec requires each bumped extension's own suite to pass, so X0 removes the stale names and X1/X3/X4 un-skip theirs; a suite that can't run on a Neon compute is reported by the task as a deviation, and the coordinator asks the user (never skipped silently). Cost if wrong: an extension shipped unverified.
- **P10** Metadata records personal logins as `neon_roles.origin = 'personal'` (`RoleOrigin::Personal`): a migration widens `neon_roles_origin` (`20261004000001_sql_roles.sql:24`) and every `match` on `RoleOrigin` gets an arm (pre-flight 16). The spec left this to the plan ("if the plan finds no existing field"). Cost if wrong: a rename.
- **P11** compute_ctl's `/status` capability read in Chelabase is a new `reconfigure::status_report` (status plus capabilities); the existing `status()` stays for its callers. Cost if wrong: one extra method.
- **P12** The spec builder's growing argument list becomes one `SpecPolicy` struct (pinned below). Cost if wrong: a signature churn in two callers.
- **P13** The personal-login live tests live in `compute_fork_live.rs` (the spec's names), so C3 runs after C2 (which owns that file this round). Cost if wrong: none.
- **P14** Leaf tasks (no later task builds on them and the gate or harness catches a broken result): K9, K10, X1, X2, X4, X5, X6, C6. They get no audit agent: the coordinator checks the RED diff and the end-of-step review covers them. X3 (pg_duckdb's security pins) and K8 (a secret's handling) are reviewed although nothing builds on them. Cost if wrong: a defect found at the end-of-step review instead.
- **P15** The extension tasks' per-task image builds are test builds (like `ci-local.sh` runs), not "the image build": nothing they build is tagged for publishing. At most 2 heavy Docker builds or `ci-local.sh` containers at once machine-wide. Cost if wrong: none.
- **P16** Fork gate selection (End of step 4) is pinned with `--collect-only` before the run; K7's test agent lists the auth-enabled tests outside `test_auth.py` that read metrics through the fixtures (`test_wal_acceptor.py`, `test_pageserver_api.py`, `test_safekeeper_deletion.py`, auth tests in `test_storage_controller.py`), and the coordinator adds them to the selection in the Status. Cost if wrong: a fixture regression found by the gate.

## Seam pass (opus, 2026-10-21)

Every pinned interface read from both sides on Neon `629166cee` and Chelabase `c206b87a`. Each ruling: the seam, both sides, the defect or gap, the ruling, the briefs it changes (already corrected in place above and below). Cost if wrong is one task's rework unless stated.

- **SP1 A second spec push the plan missed.** Producer C1 (`SpecPolicy.storage_token_expiring`); consumers: the spec hook (`services/orchestrator/src/internal/spec.rs:97-113`) and the worker (`services/worker/src/reset.rs:118-154`) are planned, but the storcon notify hook also pushes specs, `hooks.rs:100-124` (`reconfigure_endpoint` → the shared `endpoint_config`, `spec.rs:24-46`), with no capability information. Ruling: that path calls `reconfigure::status_report` on the compute's control address before building the spec and passes the same `SpecPolicy` (capability → expiring; realtime logins from the orchestrator's config); a failed report fails that reconfigure like a failed push (storcon retries the hook, `hooks.rs:86-95`). Briefs: C4 (adds `hooks.rs`).
- **SP2 Report and refresh calls, argument order and failure.** Plan pinned `status_report(http, control_addr, endpoint_id, signer)`; the existing `push`/`status` take `(http, control_addr, signer, endpoint_id)` (`crates/chelabase-neon/src/reconfigure.rs:22-28`, `:64-69`). Ruling: the new functions follow the existing order (pinned above). In the worker, an error from `report()` before `build_and_push_spec` fails that push (retried as a push failure is today), never a silent fallback to a non-expiring token for a capable compute; `report()` and `push_storage_token` are bounded by `DOCKER_TIMEOUT` (`reset.rs:98-113` pattern). Briefs: C1, C2.
- **SP3 The clock for `tenant_token`.** `tenant_token(tenant, now, expiring)` needs `now`; `endpoint_config`'s pinned signature has none (`spec.rs:212-245` mints inside). Ruling: `endpoint_config` reads `SystemTime::now()`; its unit tests assert `exp - iat == STORAGE_TOKEN_TTL` and the claim set, not a fixed token. `primary_spec_unchanged` (`spec.rs:732-772`) compares keys: it gains `realtime_logins` and keeps `storage_auth_token`. Briefs: C1.
- **SP4 C1's interim realtime logins.** C1 changes the callers only to compile (worker `reset.rs:131-146`, orchestrator `spec.rs:31-44`) but `realtime_logins` is not optional on the Chelabase side, and an empty list means "any address" with the certificate (spec §10). Ruling: the interim value is `MemberLogins::realtime(Vec::new())`; C2 (worker) and C4 (orchestrator `spec.rs` and `hooks.rs`) replace it with the parsed `CHELABASE_REALTIME_LOGIN_CIDRS`; C2's and C4's audits grep that no interim value is left. Nothing runs live in between (P6). Briefs: C1, C2, C4.
- **SP5 A refreshed token must reach compute_ctl's own callers.** `POST /storage_token` was pinned to write the file only, but compute_ctl reads `pspec.storage_auth_token` for its whole life: the LSN-lease loop of static and replica computes re-reads it on every attempt (`compute_tools/src/lsn_lease.rs:80-89`; Chelabase runs static computes), and the basebackup/sync/gRPC paths read it from the parsed spec (`compute.rs:1233`, `:1271-1273`, `:1388-1389`). After 24 h an unrefreshed in-memory copy would lose the static compute's lease. Ruling: `/storage_token` writes the file, then replaces the token in `state.pspec` (and `pspec.spec`) under the state lock; no status change. `StorageAuthToken` gets `as_str()` (pinned); `lsn_lease.rs` is K3's. Briefs: K3.
- **SP6 A compute that started without a token (P4).** P4 writes `neon.storage_auth_token_file = ''` (a `PGC_POSTMASTER` GUC, K5) when the start spec has no token, so a token arriving later through `/configure` or `/storage_token` could never be read until a restart, and a rewritten path line would make every reload log "cannot be changed without restarting". Ruling: such a compute answers `400` ("compute started without storage auth; restart to enable it") to `/storage_token` and to a `/configure` that adds a token, nothing changed (the mirror of RT4-7); compute_ctl decides both path lines once at start and every `postgresql.conf` rewrite writes them identically. Chelabase always sends a token. Briefs: K3.
- **SP7 `/storage_token` states, rejections and error text.** compute_ctl's `Json` extractor keeps axum's rejection status (415/422, `compute_tools/src/http/extract/json.rs:20-29`); Chelabase treats every non-2xx as an error and logs compute_ctl's body (`reconfigure.rs:36-40`). Ruling: the route maps every body rejection to `400`; error texts name the reason, never the token; outside `running`/`configuration_pending` (e.g. `init` while the start path is still writing the file) it answers `412` and writes nothing, which the worker loop counts and retries. Briefs: K3, C2.
- **SP8 `exp` leeway.** The storage servers validate with `Validation::default()` (60 s leeway; `libs/utils/src/auth.rs:139-143`); compute_ctl's `Authorize` too (`authorize.rs:29-49`). Ruling: tests that need an expired token mint `exp` at least 120 s in the past or wait `exp + 61 s`. The runner mints its short-lived token with the pinned API, `tenant_token(tenant, real_now - STORAGE_TOKEN_TTL + 30, true)` (no extra TTL parameter). Briefs: K5, C2.
- **SP9 Existing logins as `chelabase_realtime` break under W31.** K4's block admits the role only over `hostssl` from the CIDR list with the realtime certificate; the list is realtime `.12` and the runner `.11` on `control` (C5). Round 3's `compute_fork_live` tests log in as the role over the compute network with a password only (`services/worker/tests/compute_fork_live.rs:65`, `:186-210`), as will C2's new aux-file test; realtime's in-process tests use the replicator. Ruling: every live login as `chelabase_realtime` dials the compute's `control` address (`DockerLocator::connect(<control network>, stack)`, as the orchestrator's console locator, `services/orchestrator/src/main.rs:133-136`) with the realtime pair from `realtime-tls/current`; C2 updates `compute_fork_live`, C4 `realtime_role_live` and `realtime_live`; both test agents grep every live file for `chelabase_realtime`/`decoder::ROLE` logins. Briefs: C2, C4.
- **SP10 The worker's realtime-pair directory and the compose address.** The sweep renews in `sweep.tls_dir` from `CHELABASE_COMPUTE_TLS_DIR` (`services/worker/src/certs.rs:110-130`, `config.rs:82`, `:167`); the plan pinned `CHELABASE_REALTIME_TLS_DIR` for realtime only, and no name for realtime's static IP. Ruling: the worker reads `CHELABASE_REALTIME_TLS_DIR` too (default `.chelabase/realtime-tls`); the sweep writes a missing pair, as for the orchestrator's (`certs.rs:121`); Compose takes realtime's address from `CHELABASE_REALTIME_CONTROL_IP` (as `CHELABASE_ORCHESTRATOR_CONTROL_IP`/`CHELABASE_RUNNER_CONTROL_IP`, `docker-compose.yml:971`, `:1304`); env.rs renders all three settings (pinned above). Briefs: C2, C5.
- **SP11 `replication_host` on a shared route.** The connection route (`services/orchestrator/src/internal/data.rs:113-154`) also serves rest (`"host"`, `"server_name"`); a control-locator failure must not turn rest's answer into `409`. Ruling: `replication_host` is `null` when the control locator finds nothing or fails (logged), the rest of the answer unchanged; realtime's type takes it `#[serde(default)]`; `null` = `SlotError::Unavailable`. Briefs: C4.
- **SP12 No `curl` in the compute image.** The final stage installs `libcurl4` but no `curl` binary (`compute/compute-node.Dockerfile:1957-1982`); `iproute2` is there. Ruling: `compute_ctl_internal_api_is_loopback_and_needs_the_token` probes from inside the compute with bash's `/dev/tcp` (an HTTP request by hand) and lists sockets with `ss -ltnp`; from the runner, a TCP connect to the compute's `:3081` must fail. Briefs: C2.
- **SP13 `RoleOrigin::Personal` is in the public API.** `BranchRole.origin: RoleOrigin` is serialised by the management API and in its OpenAPI document (`services/orchestrator/src/roles.rs:61-66`, `openapi.rs:1445`), generated into `packages/management-client/src/generated/model/roleOrigin.ts`; `just check` fails on stale generated code (`justfile:120`). Pre-flight 16 missed `roles.rs`. The spec roles are already owner-only (`crates/chelabase-metadata/src/neon.rs:737-752`), so personal rows never reach a spec without a builder change. Ruling: C3 adds the variant everywhere, runs `just api-gen` and commits the documents and clients; its test pins that `neon_roles`/`neon_roles_versioned` exclude personal rows; the merge pause lists the new enum value. Briefs: C3.
- **SP14 X0 named the wrong pgx_ulid stage.** Spec §17.1 removes the v14–v16 `ulid` 0.1.5 build `pgx_ulid-src`/`pgx_ulid-build` (`compute-node.Dockerfile:1291-1314`); the plan's X0 listed `pgx_ulid-pgrx12`, which is the v17 build (`:1323-1347`) X1 bumps to 0.2.3. Ruling: X0 removes `pgx_ulid-src`/`pgx_ulid-build` and its `COPY`; `pgx_ulid-pgrx12-*` is X1's. Briefs: X0, X1. Cost if wrong: v17's `pgx_ulid` deleted.
- **SP15 The single pgrx layer's name.** `pg_repack-build` and `pg_mooncake-build` build `FROM rust-extensions-build` (`:1497`, `:1546`), which is the pgrx 0.11 layer that exits early on v17. Ruling: X1's one layer is named `rust-extensions-build` and every pgrx stage moves onto it, so `pg_repack-build` keeps building between X1 (W6) and X4 (W7), which moves it to `pg-build`. X1's `local_conn_pool.rs` change is Rust in the proxy: X1 compiles it (`ci-local.sh --pg v17 --only build`). Briefs: X1.
- **SP16 X6 needs K3, K5 and both token files.** X6 checks K3's DuckDB pins and K5's `PGC_SUSET`, but the task table listed only X0, X3; and the harness compute runs without storage auth (`docker-compose/compute_wrapper/shell/compute.sh:95-100`), so P4 would leave no storage token file and a "refused" read could just be a missing file. Ruling: X6 depends on K3+K5 (table corrected; already its checkout base); X6 adds a dummy `storage_auth_token` to the harness spec (`compute_wrapper/var/db/postgres/configs/config.json`; the harness pageserver has no auth) and first asserts as `cloud_admin` that both files exist. Briefs: X6.
- **SP17 compute_ctl metrics tokens in the Python fixtures.** The fixture's scope enum has only `ADMIN` (`test_runner/fixtures/endpoint/http.py:27-28`); `Endpoint.generate_jwt(scope)` (`neon_fixtures.py:4884`) runs `neon_local endpoint generate-jwt --scope`, which builds `ComputeClaims` with this compute's `compute_id` for any non-admin scope (`control_plane/src/endpoint.rs:685-697`) and parses the scope with `FromStr`. Ruling: K4 adds `ComputeClaimsScope.METRICS = "metrics"` to `http.py`; `test_metrics_need_a_token` covers both a `generate_jwt(METRICS)` token (carries `compute_id`: RT4-9's case) and a PyJWT-minted one without `compute_id`; K4 doesn't edit `neon_fixtures.py` (K7's in W3). Briefs: K4.
- **SP18 K3's fixture interface for K5.** K5's tests read the token files and push refreshes through K3's fixtures. Ruling (names pinned): `Endpoint.internal_api_token_file -> Path`, `Endpoint.storage_auth_token_file -> Path` (in `neon_fixtures.py`), `EndpointHttpClient` reads the internal secret from the endpoint's file and sends it as the bearer on internal-port routes only (the external JWT stays on `:external`), dials `127.0.0.1` instead of `localhost` for the internal port (`http.py:167-208`), and gains `storage_token(token: str) -> None` (raises on non-2xx). Briefs: K3, K5.
- **SP19 local_proxy's token file at startup.** The `LocalProxy` fixture starts `local_proxy` with `--disable-pg-session-jwt` and no compute_ctl (`neon_fixtures.py:4216-4241`); the default `--compute-ctl-token-file` doesn't exist there. Ruling: the file is read per call only; a missing file at startup is not an error (a call made without it goes out without a header and is refused). Briefs: K8.
- **SP20 C3 and C4 share orchestrator test files in W9.** C3 adds `RoleOrigin::Personal` arms in `services/orchestrator/tests/{internal.rs,common/fakes.rs,api.rs,role_rules_agree.rs}`; C4 adds `InternalState` fields (control locator, realtime logins), and `tests/internal.rs` builds `InternalState` literally. Ruling: they stay parallel; C3 merges first, C4 merges second and its coder resolves the conflict (as P8). Briefs: C3, C4; waves table annotated.
- **SP21 The local build-tools image at the image build.** X5 changes `build-tools/Dockerfile`, so its tag changes; `ci-local.sh` uses a local image first (`.github/scripts/ci-local.sh:627-635`), and `publish-dev.sh`'s storage build is `FROM ghcr.io/chelabase/neon-build-tools:<sha12>` (`publish-dev.sh:214-222`), which buildx finds locally only with the `docker` driver; the image isn't pushed until ship. Ruling: X5 reports the active builder's driver; if it isn't `docker`, the coordinator switches the builder (`docker buildx use default`) before End of step 4, never pushes the image early. Briefs: X5. Cost if wrong: a failed storage build at the end.
- **SP22 One marker predicate.** K3's `/grants` refusal (RT4-6), K6's guard (S15) and C3's `is_marked_sql` must agree on "marked". Ruling: all three use exactly the pinned predicate: a `pg_db_role_setting` row with `setdatabase = 0` and `setrole = <role oid>` whose `setconfig` has an entry starting `chelabase.managed_role=`. Briefs: K3 (K6 and C3 already say so).
- **SP23 Reading `Compute-Capabilities`.** compute_ctl sends one header (`do_control_plane_request`, `compute_tools/src/spec.rs:25-31`); the spec hook has the `HeaderMap` (`internal/spec.rs:97-101`). Ruling: the reader checks every value of the header (`get_all`), splits on `,`, trims, compares exactly (case-sensitive); a value that isn't UTF-8 counts as "not capable". `advertises_storage_token_refresh` takes the joined values. Briefs: C1, C4.

Checked and consistent (no ruling): spec-role list (owner rows only); `ComputeSpec.realtime_logins`/`member_logins` key names and `MemberLogins { allowed_cidrs, client_cns }` on both sides (`libs/compute_api/src/spec.rs:218-235`, `crates/chelabase-neon/src/spec.rs:63-66`); `chelabase-realtime` passes compute_ctl's CN check (`compute_tools/src/spec.rs:166-176`); the metrics token's claims decode on all four servers (`utils::auth::Claims` has no `deny_unknown_fields`, `auth.rs:74-88`; `ComputeClaims` takes `scope` only, `requests.rs:37-57`; `validate_aud = false` and no required claims at compute_ctl, `authorize.rs:42-49`) and are signed by the key whose public half every server holds (`jwt.rs:17-23`, `:104-114`); Chelabase's control tokens are `{compute_id, exp}` (`jwt.rs:117-126`), so "this compute's control token" on `/storage_token` and the metrics routes is the existing `token_for`; token paths: Chelabase's pgdata is `/var/db/postgres/compute` (`crates/chelabase-neon/src/compute.rs:67-76`) whose parent is postgres-owned (`compute-node.Dockerfile:1987-1993`), as local_proxy's default; the worker's admin SQL runs with `neon.forward_ddl=off` (`backend.rs:274-293`), so the personal-login marker isn't forwarded; nothing in Chelabase or the extension harness reads `/metrics`; the storage servers' `Scope` has `TenantEndpoint`, `Scrubber`, `ControllerPeer` too (`auth.rs:23-58`): K7's exhaustive table covers every variant.

## Task table (binding)

Models per pipeline stage. "Audit" is the task's one review; "none (leaf)" per P14.

| Task | Scope | Test | Coder | Audit (review) | Wave | Depends |
|---|---|---|---|---|---|---|
| K1 | U16: slots copied/dropped only when persistent, stale copies cleaned at start (Postgres fork); basebackup reads persistency safely | opus | opus | opus | 1 | W0 |
| K3 | compute_ctl: internal server on loopback + secret, token-file writer, storage token file and `POST /storage_token`, capability, RT4-6/7/8/10/11, DuckDB pins; neon_local and fixtures | opus | opus | opus | 1 | W0 |
| K2 | W9: `chelabase_keys` stream callbacks (Postgres fork) | opus | opus | opus | 2 | W0 |
| K5 | neon extension: `token_file.c`, every caller reads the files, `127.0.0.1` + bearer, `allow_unstable_extensions` SUSET | opus | opus | opus | 2 | K3 (reviewed, fixes in) |
| K4 | compute_ctl external auth (401, no `INSTANCE_ID`, no token logged, `kid`), `Metrics` scope on `:3080` (U4, RT4-9), W31 `realtime_logins` hba/ident | opus | opus | opus | 3 | K3+K5 merged |
| K7 | Storage auth: `Scope::Metrics`, total `check_permission` (item 5), `/metrics` and profiles behind JWT on pageserver, safekeeper, storcon (RT4-5); fixtures | opus | opus | opus | 3 | W0 |
| K6 | neon extension: `chelabase.managed_role` marker guard (D1) | opus | opus | opus | 4 | W0 |
| K8 | proxy: `--redis-plain-password-file`; `--compute-ctl-token-file` for local_proxy and the testing `Local` backend | sonnet | sonnet | sonnet | 4 | W0 |
| K9 | storcon: full-jitter backoff, retry histogram, dead migration arm removed | sonnet | sonnet | none (leaf) | 4 (after K8) | W0 |
| X0 | Extension removals (§17.1, `pg_mooncake`), shared Dockerfile regions, harness `ONLY` + `chelabase-ext-test.sh`, SKIP list | sonnet | sonnet | sonnet | 5 | W0 |
| C1 | `crates/chelabase-neon`: `tenant_token`, `metrics_token`, `StorageScope::Metrics`, `SpecPolicy`, `realtime_logins`, `storage_token_expiring`, `pg_partman_bgw.role`, `push_storage_token`, `status_report`, `REALTIME_CN` | sonnet | sonnet | opus | 5 | — |
| X1 | pgrx group: one 0.19.2 layer; pg_graphql, pg_jsonschema, pg_session_jwt (+ local_proxy `EXT_VERSION`), pgx_ulid, pg_anon 3.2.3 | sonnet | opus | none (leaf) | 6 | X0 |
| X2 | PostGIS stack: CGAL 6.2.1, SFCGAL 2.3.0, PostGIS 3.6.4, pgrouting 4.0.2, h3 + h3-pg 4.5.0 | sonnet | sonnet | none (leaf) | 6 | X0 |
| X3 | rdkit, plv8 (patch rework), pg_duckdb 1.1.1 (RT4-1 patches and grants) | sonnet | opus | opus | 7 | X0 |
| X4 | C extensions: timescaledb, pg_partman, pg_cron; §17.2 rows 4–5, 18–32 | sonnet | sonnet | none (leaf) | 7 | X0 |
| X5 | pgbouncer, the three exporters, build-tools `sql_exporter` (image built once, local), `test_compute_metrics.py` | sonnet | sonnet | none (leaf) | 8 | X0 |
| C2 | worker: storage-token refresh loop, capability read, settings, realtime CIDRs, generalised cert sweep; live RED tests in `compute_fork_live` | sonnet | sonnet | opus | 8 | C1 |
| C3 | personal logins: `role_name` helpers, `RoleOrigin::Personal` + migration, `create_personal_login`, resync re-marking; live RED tests | sonnet | sonnet | opus | 9 | C1, C2 merged |
| C4 | orchestrator (`Compute-Capabilities` header, the storcon-hook reconfigure's capability read (SP1), `replication_host`, control locator) and realtime (client cert, `replication_host`) | sonnet | sonnet | opus | 9 | C1; merges after C3 (SP20) |
| C5 | CLI/workbench, Compose, scripts: metrics token + mint command, realtime pair, `.12` and CIDRs, proxy flags, mounts, `compute-network-e2e.sh`, `compose-secret-argv.sh` | sonnet | sonnet | sonnet | 10 | C1–C4 merged |
| X6 | RT4-1 image test on `extensions-all` (test only; GREEN is End of step 4's harness run) | opus | — | none (leaf) | 10 | X0, X3, K3+K5 merged (SP16) |
| C6 | Docs: `security.mdx`, `infra/neon/README.md` Round 4 draft | — | haiku | none (leaf) | 10 | — |
| K10 | Fork integration: Postgres branches into `round-4-v17`, submodule + `revisions.json`, remaining merges into `round-4`, `FORK.md` | — | sonnet | none (leaf) | 11 | all K, X |

### Waves (at most 2 pipelines, 3 agents, 2 `ci-local.sh` containers or heavy Docker builds at once)

| Wave | Lane A | Lane B | Claude time (agents + machine) |
|---|---|---|---|
| W0 | coordinator: cut `round-4`, `round-4-v17`, push; clones `-k3`, `-k1` | — | 0.3 h |
| W1 | K3 (cold build first) | K1 | 5 h |
| W2 | K5 (from `round-4-k3`) | K2 | 4 h |
| W3 | K4 | K7 | 4 h |
| W4 | K6 | K8, then K9 | 4 h |
| W5 | X0 (base `extensions-all` build ~2.5 h, then post-removal) | C1 | 4.5 h |
| W6 | X1 | X2 | 5 h |
| W7 | X3 | X4 | 5 h |
| W8 | X5 (build-tools image) | C2 | 3.5 h |
| W9 | C3 (merges first) | C4 (merges second, resolves the orchestrator test-file conflicts, SP20) | 3.5 h |
| W10 | C5 | X6 test agent, then C6 | 3 h |
| W11 | K10 | — | 1 h |
| End | review 1.5 h; final fix round 4 h; `just check` 0.5 h; fork gate 2.5 h; image build (minimal 1 h, `extensions-all` + harness 1.5 h); live pass 3.5 h | | ≈ 14.5 h |

Every task, Chelabase-side ones included, runs and merges before the image build; only the live tests (written and compiled by C2–C5, traced by reading) and the `extensions-all` harness run come after it (P6, End of step 4–5). Total ≈ 57 h of Claude time. A fix due right after a review (K3's, X0's, C1's, whose findings later tasks build on) is part of its wave's time.

## Shared interfaces (pinned)

**Token files (K3 writes, K5/K8/fixtures read; §4.1).**
- Internal secret: 32 bytes from the OS RNG, lowercase hex (64 chars). File `--internal-api-token-file`, default `<pgdata parent>/compute_ctl_internal.token` (Chelabase: `/var/db/postgres/compute_ctl_internal.token`; neon_local: `<endpoint dir>/compute_ctl_internal.token`).
- Storage token: `pspec.storage_auth_token`. File `--storage-auth-token-file`, default `<pgdata parent>/storage_auth.token`.
- Both: mode 0600, owned by compute_ctl's user, content `<token>\n`, written via `<file>.tmp.<pid>` (`O_CREAT|O_EXCL`, 0600) → `fsync` → `rename` → directory `fsync`. Never logged.
- `postgresql.conf` lines, after the spec's settings and the Databricks block, next to `neon.branch_protected`: `neon.compute_ctl_token_file='<path>'`, `neon.storage_auth_token_file='<path>'` (or `''`, P4). Then, same place: `duckdb.disabled_filesystems='LocalFileSystem'`, `duckdb.autoinstall_known_extensions=off`, `duckdb.autoload_known_extensions=off` (RT4-1, S22; names and `PGC_SUSET` contexts verified, pre-flight 13).
- Reader alphabet: one line of `[A-Za-z0-9._~+/=-]`, 1–8191 bytes, one trailing `\n` stripped, whole file ≤ 8 KiB.

**neon extension (K5).** GUCs `neon.compute_ctl_token_file`, `neon.storage_auth_token_file`: string, `PGC_POSTMASTER`, default `''`. `pgxn/neon/token_file.h`:
- `const char *neon_storage_auth_token(void);` and `const char *neon_compute_ctl_token(void);`: the current token, or `NULL` (no path, or fail closed); the pointer is valid until the next call in the process. One `stat()` per call; identity `{st_dev, st_ino, st_size, st_mtim}` taken from `fstat` of the opened descriptor.
- `neon_auth_token` (`neon.h:16`) and `getenv("NEON_AUTH_TOKEN")` are removed.

**compute_ctl HTTP (K3, K4; Chelabase C1/C2/C4 read).**
- `GET /status` (`ComputeStatusResponse`): new `capabilities: Vec<String>`, `#[serde(default)]`, value `["storage_token_refresh"]`.
- Spec fetch request header `Compute-Capabilities: storage_token_refresh` (comma-separated list; readers trim and ignore unknown entries).
- `POST /storage_token` on `:3080`, body `{"token":"<jwt>"}` (`compute_api::requests::StorageTokenRequest { token: StorageAuthToken }`); auth: this compute's control token or `compute_ctl:admin`; `204` written (file **and** the in-memory `pspec`/`spec` token, SP5); `400` empty/invalid/any body rejection, or a compute that started without a storage token (SP6, SP7; file kept, error text never contains the token); `401` no or bad bearer; `412` in any status but `running`/`configuration_pending` (SP7); `500` write failure (old file kept). Never changes the status.
- `/configure` on a compute that holds a storage token with a spec whose `storage_auth_token` is absent or empty → `400`, nothing changed (RT4-7).
- Internal routes: `Authorization: Bearer <secret>`; missing/other scheme/wrong → `401` + `WWW-Authenticate: Bearer`, JSON `{"error": …}` never echoing the header.
- External auth (K4): missing/malformed/non-Bearer → `401` + `WWW-Authenticate: Bearer`. `ComputeClaimsScope::Metrics`, serde and `FromStr` `"metrics"`; a `Metrics` token opens only `GET /metrics` and `GET /autoscaling_metrics` (no `aud` needed, any or no `compute_id`); both routes also accept `compute_ctl:admin` and this compute's control token (S18); every other route answers a `Metrics` token `403`.
- `compute_api::spec::ComputeSpec.storage_auth_token: Option<StorageAuthToken>` where `StorageAuthToken(String)` is `#[serde(transparent)]` with a hand-written `Debug` printing `StorageAuthToken(<redacted>)` (RT4-8) and `pub fn as_str(&self) -> &str` (no `Display`, no `Deref`); `ParsedSpec.storage_auth_token` uses the same type, and every reader (`compute.rs` basebackup/sync/gRPC, `lsn_lease.rs`, `control_plane/src/endpoint.rs`) goes through `as_str()` (SP5).
- `ComputeSpec.realtime_logins: Option<MemberLogins>`, `#[serde(default)]` (K4). Blocks: `# chelabase: realtime logins` … `# chelabase: end of realtime logins` in `pg_hba.conf` (right after the member block, before the basebackup's `trust` lines; lines as spec §10); `# chelabase: realtime certificate map (chelabase_realtime)` … `# chelabase: end of realtime certificate map` in `pg_ident.conf`, one `chelabase_realtime <cn> chelabase_realtime` line per CN.

**Storage auth (K7; C1/C5 mint).** `utils::auth::Scope::Metrics` (serde `"metrics"`). `/metrics` accepts `{Metrics, Admin, own global}`; profiles `{Admin, own global}`; own global: pageserver `PageServerApi`, safekeeper `SafekeeperData`, storcon `Admin`. Public: pageserver `/v1/status`, `/v1/doc`, `/swagger.yml`; safekeeper `/v1/status`; storcon `/status`, `/live`, `/ready`. A `Tenant` token without `tenant_id` → `AuthError("tenant scope without a tenant id")` (403).

**Postgres / pageserver (K1).** `void LogDiscardedSlotFiles(void)` in `slot.h`. `pg_constants::REPL_SLOT_ON_DISK_OFFSETOF_PERSISTENCY: usize = 4*4 + 64 + 4` (84). `basebackup.rs`: `fn persistent_slot_restart_lsn(content: &[u8]) -> Option<Lsn>` (None for non-persistent or shorter than `RESTART_LSN offset + 8` = 104 bytes, with a warning for short).

**Role marker (K6, C3).** GUC `chelabase.managed_role`: string, `PGC_SUSET`, default `''`, `GUC_NOT_IN_SAMPLE`. Refusal `42501`, `role "%s" is managed by Chelabase`. Chelabase `chelabase_types::role_name`: `pub const PERSONAL_LOGIN_MARKER: &str = "chelabase.managed_role"`, `pub fn mark_personal_login_sql(role: &str) -> String` (`ALTER ROLE <quote_ident(role)> SET chelabase.managed_role = 'personal';`), `pub fn is_marked_sql(role: &str) -> String` (one boolean row: a `pg_db_role_setting` row with `setdatabase = 0` for the role whose `setconfig` has an entry starting `chelabase.managed_role=`). Worker `Worker::create_personal_login(&self, endpoint: &str, name: &str, scram_verifier: &str) -> anyhow::Result<()>`. Metadata `RoleOrigin::Personal` ↔ `'personal'`.

**Proxy (K8; C5).** `proxy --redis-plain-password-file <path>` (with `--redis-auth-type=plain`); `local_proxy --compute-ctl-token-file <path>` (default `/var/db/postgres/compute_ctl_internal.token`); `proxy --compute-ctl-token-file <path>` for the testing `Local` backend (no default; absent = no header). Compose: `--redis-plain=redis://proxy@valkey:6379 --redis-plain-password-file=/run/valkey-proxy.password`.

**Chelabase (C1, read by C2–C5).**
- `jwt.rs`: `pub const STORAGE_TOKEN_TTL: Duration = Duration::from_secs(86_400)`; `StorageScope::Metrics` (`"metrics"`); `ComputeSigner::tenant_token(&self, tenant: &NeonId, now: u64, expiring: bool) -> anyhow::Result<String>` (`expiring`: `{"tenant_id","scope":"tenant","iat":now,"exp":now+TTL}`; else today's `{"tenant_id","scope":"tenant"}`); `ComputeSigner::metrics_token(&self, now: u64) -> anyhow::Result<String>` (`{"scope":"metrics","iat":now}`). `storage_token` keeps its deterministic output for the service scopes.
- `spec.rs`: `pub struct SpecPolicy<'a> { pub member_logins: &'a MemberLogins, pub realtime_logins: &'a MemberLogins, pub branch_protected: bool, pub storage_token_expiring: bool }`; `endpoint_config(placement, endpoint, roles, databases, jwks, console_url, policy: &SpecPolicy<'_>, signer)` and `stored_endpoint_config(…, policy: &SpecPolicy<'_>, signer)`; `endpoint_config` takes `now` from `SystemTime::now()` itself (no clock parameter, SP3); `SpecInputs` gains `realtime_logins: MemberLogins`, `storage_token_expiring: bool`; JSON key `"realtime_logins"`; `MemberLogins::realtime(allowed_cidrs) -> Self` (`client_cns: ["chelabase-realtime"]`); `parse_realtime_login_cidrs(raw: Option<&str>)` (as `parse_member_login_cidrs`, naming `CHELABASE_REALTIME_LOGIN_CIDRS`); settings gain `setting("pg_partman_bgw.role", "postgres", "string")` (S21); `primary_spec_unchanged`'s key list gains `realtime_logins` (`storage_auth_token` stays in the list: the test compares keys; value comparisons of the token, which now changes per mint, go, SP3).
- `reconfigure.rs`: `pub struct ComputeCtlReport { pub status: ComputeCtlStatus, pub storage_token_refresh: bool }`; `pub async fn status_report(http, control_addr, signer, endpoint_id) -> anyhow::Result<ComputeCtlReport>`; `pub async fn push_storage_token(http, control_addr, signer, endpoint_id, token: &str) -> anyhow::Result<()>` (argument order as the existing `push`/`status`, SP2; `StatusBody` gains `#[serde(default)] capabilities: Vec<String>`; a non-2xx answer is an error carrying the status and compute_ctl's body); `pub const CAPABILITIES_HEADER: &str = "Compute-Capabilities"`, `pub const STORAGE_TOKEN_REFRESH: &str = "storage_token_refresh"`, `pub fn advertises_storage_token_refresh(header: Option<&str>) -> bool`.
- `compute_tls.rs`: `pub const REALTIME_CN: &str = "chelabase-realtime"`; the client-pair writer and renewal take `(dir, cn)`.
- Worker `SpecPusher` gains `report(control_addr, endpoint_id) -> Result<ComputeCtlReport>` and `push_storage_token(control_addr, endpoint_id, token: &str) -> Result<()>`.
- Settings: `CHELABASE_STORAGE_TOKEN_REFRESH_SECS` (worker, default 3600), `CHELABASE_REALTIME_LOGIN_CIDRS` (worker and orchestrator; unset = startup error where the services are built, as round 3 P13; empty = no address rule), `CHELABASE_REALTIME_TLS_DIR` (realtime, required; the worker reads it too, default `.chelabase/realtime-tls`, as `CHELABASE_COMPUTE_TLS_DIR` for the orchestrator pair, SP10), `CHELABASE_REALTIME_CONTROL_IP` (stack env, realtime's `ipv4_address` on `control`, SP10).
- Files: `<keys>/neon/storage-tokens/metrics.token` (0600, kept across runs; `chelabase neon mint-metrics-token` re-mints); `<keys>/realtime-tls/` (`current` layout, as `orchestrator-tls`).
- Orchestrator connection route: new `"replication_host"`: the compute's `control`-network IP as a string, or `null` when the control locator finds no address or fails (logged; the route's answer is otherwise unchanged, since rest reads the same route, SP11); realtime's lookup type gains `#[serde(default)] replication_host: Option<String>`; the replicator dials it (a missing value is `SlotError::Unavailable`, never a fallback to `host`).
- Workbench `ControlNet` gains `realtime_ip` = `10.213.<4k>.12`.

## Pre-flight scan (2026-10-21)

Read on Neon `629166cee`, Postgres `76ce76278c5` and Chelabase `b4c32da0`. Items marked **(contradicts the spec)** change what the spec says; the rest confirm or add.

1. **U16 offsets (contradicts the spec):** `ReplicationSlotOnDisk` puts `persistency` at 84 (header 16 + `NameData` 64 + `database` 4, `slot.h:63-74`) and `restart_lsn` at 96 (`pg_constants.rs:231`); the current slice reads 8 bytes from 96, so it panics on states shorter than **104** bytes, not 96. K1's unit test covers 0, 83, 95, 103 and full length.
2. U16 sites confirmed: save `slot.c:2145-2156` (no persistency check), drop `:948-955`, restore `:2391` deletes non-persistent slots, `StartupReplicationSlots` `xlog.c:5679`, `LocalSetXLogInsertAllowed` `:6237`, `XLogReportParameters` `:6264`, all in the startup process. Trap: `LogLogicalMessage(…, transactional=false, flush=true)` runs there before `SharedRecoveryState` is done; `XLogFlush` must take the `XLogInsertAllowed()` path (local inserts allowed) — K1's test proves the record reaches the pageserver.
3. `chelabase_keys` sets no stream callbacks (`chelabase_keys.c:183-191`); its `REGRESS` lists live in both `Makefile:4` and `meson.build` (`stream` goes in both); `logical.conf` already has `max_prepared_transactions = 2` and `logical_decoding_work_mem = 64kB`.
4. compute_ctl binds both servers to `[::]` (`http/server.rs:192-199`); the internal router is `server.rs:65-88`; `/metrics` and `/autoscaling_metrics` sit on the unauthenticated external router (`:93-99`).
5. `authorize.rs`: missing header → 400 (`:80-86`), `INSTANCE_ID` bypass (`:67-79`), `debug!("verifying token {}")` (`:154`), `key_id.unwrap()` twice (`:162`, `:175`). Non-admin scopes fall to the `compute_id` check (`:119-133`): RT4-9's explicit arm is needed.
6. Token env: `start_postgres` `compute.rs:1752-1776`, `sync_safekeepers` `:1459-1470`; spec `Debug` logs at `compute_promote.rs:155` **and `spec_apply.rs:47`** (`debug!("Config: {:?}", spec)`; covered by the `StorageAuthToken` type).
7. **D2 file list (contradicts the spec, adds a file):** neon_local's `control_plane/src/endpoint.rs` calls `:3081` `/refresh_configuration` without a bearer (`:1185-1210`) and passes `NEON_AUTH_TOKEN` to `pg_ctl` (`:620-622`); `control_plane/src/pageserver.rs:354` passes it to the pageserver's own process (unrelated, kept). K3 owns `endpoint.rs`: bearer from the endpoint's token file, the `pg_ctl` variable dropped. neon_local's internal address is already `127.0.0.1` (`:225-228`).
8. **local_proxy (adds):** nothing in the Docker compute launches local_proxy (only the image ships it, `compute-node.Dockerfile:2008-2010`), so `--compute-ctl-token-file` defaults to the Chelabase path (pinned). `local_conn_pool.rs:45` pins `EXT_VERSION = "0.3.1"` of `pg_session_jwt` and calls `auth.jwt_session_init` (`:274`): X1's bump to 0.5.0 updates that constant and checks the function (0.5.0 ships `sql/pg_session_jwt--0.4.0--0.5.0.sql`).
9. **Storage auth fixtures (contradicts the spec's scope):** the fixture teardown reads pageserver metrics with an unauthenticated client (`neon_fixtures.py:1480-1490` → `assert_no_metric_errors`/`snapshot_final_metrics`, `:3149-3176`; `http_client()` defaults to no token, `:3130-3138`), so every auth-enabled test would fail at teardown after D3; storcon's `get_metrics` sends no scope (`:2032-2034`). K7 changes both (a `pageserverapi` token when auth is on; storcon `admin`). Safekeeper clients already default to a `safekeeperdata` token (`:5470-5482`).
10. **Fixture token helper (adds):** `AuthKeys.generate_token` stringifies every claim (`fixtures/auth_tokens.py:17-19`), so it can't mint a numeric `exp`/`iat`; K7 adds numeric `exp`/`iat` keyword arguments and `TokenScope.METRICS`.
11. Storcon's allowlist also has `/live` (`storage_controller/src/http.rs:77-84`); it stays public. Storcon's candidate-leader route list (`:1840-1850`) keeps `/metrics` and profiles reachable while a candidate, now behind auth.
12. **SFCGAL (settles a spec unknown):** SFCGAL 2.3.0 requires `CGAL 5.6` (`CMakeLists.txt:193`); bookworm's `libcgal-dev` is 5.5 (`compute-node.Dockerfile:278`). X2 builds CGAL **v6.2.1** (latest release; header-only, C++17; Boost 1.74 from bookworm suffices) from its release tarball with a pinned sha256 in `postgis-src`. The final stage installs bookworm's `libsfcgal1` (1.4) runtime (`:1972`) while the build copies the source-built SFCGAL to `/` (`:1670`): X2 drops `libsfcgal1` so only 2.3.0's library ships.
13. **pg_duckdb 1.1.1 (settles S22):** `src/pgduckdb_guc.cpp` at `v1.1.1` defines `duckdb.disabled_filesystems`, `duckdb.autoinstall_known_extensions`, `duckdb.autoload_known_extensions` (all `PGC_SUSET`, settable only before DuckDB initialises) and `duckdb.postgres_role` (`PGC_POSTMASTER`, `GUC_SUPERUSER_ONLY`, default empty = superusers only). An owner can't change any of them in a session.
14. **Removals (contradicts the spec's list):** besides the dirs §17.1 names, `docker-compose/ext-src/pgjwt-src` and `docker-compose/ext-src/pgrag-src` exist and go too. The harness `SKIP` holds stale names (P9). `compute/patches/plv8_v3.1.10.patch` is unused by the v17 build; X3 checks and removes it if no stage applies it.
15. **No DuckDB pins today (corrects the spec's table):** §1 lists "`compute_tools/src/config.rs` (the duckdb pins)" as existing; there are none. K3 adds them (pinned above).
16. **Personal origin (settles RT4-4's open item):** `RoleOrigin` is `Owner | Sql | Api` (`crates/chelabase-metadata/src/neon.rs:102-124`), constrained by `neon_roles_origin` (`migrations/20261004000001_sql_roles.sql:24`); match sites in `services/worker/src/{merge/phases.rs,merge/apply.rs,roles.rs,resync.rs,branch_create.rs,fakes.rs}`, `services/orchestrator/src/internal/ddl.rs`, orchestrator tests (`common/fakes.rs`, `internal.rs`, `role_rules_agree.rs`, `api.rs`), `services/worker/tests/roles_live.rs`, `crates/chelabase-metadata/tests/live_db.rs` (P10).
17. `SpecPusher::status` returns only `ComputeCtlStatus` (`services/worker/src/ports.rs:1688-1692`; users `provision.rs`, `f4_tests/{wait_loop,audit}.rs`, `fakes.rs`, `tests/compute_fork_live.rs`, `crates/chelabase-neon/tests/reconfigure_mock.rs`) (P11).
18. Chelabase spec builder: `endpoint_config`/`stored_endpoint_config` take `member_logins`, `branch_protected` positionally (`crates/chelabase-neon/src/spec.rs:166-240`); callers `services/worker/src/reset.rs` and `services/orchestrator/src/internal/spec.rs:24-46` (P12). The storage token is minted in `endpoint_config` (`:227-229`).
19. Realtime logs in as `chelabase_realtime` only in `PgReplicator::start` (`services/realtime/src/slots.rs:297-345`, `ReplicationTarget` `decoder.rs:196-231`); the lookup's `host` comes from the orchestrator route (`services/orchestrator/src/internal/data.rs:110-142`), which already carries `server_name`. Workbench addresses: `.10` orchestrator, `.11` runner (`tools/cli/src/workbench/env.rs:38-48`); `.12` is free.
20. Compose: compute digest pin `infra/compose/docker-compose.yml:21` (storage pin beside it); proxy entrypoint `:884-914`; realtime service `:1176`; `orchestrator-tls` mount anchors `:319-327`. The safekeeper's own `NEON_AUTH_TOKEN` (`:776-777`) is the safekeeper's peer token, unrelated to compute_ctl, and stays.
21. `compute-network-e2e.sh`: `known_gaps=(/profile/cpu /profile/heap /metrics)` (`:110`), `compute:3080` `no_cred_also=400` (`:186`), the `compute:3081` known gap (`:187-189`).
22. `test_unstable_extensions.py` sets `neon.allow_unstable_extensions` as `cloud_admin` (`:48`), so it keeps passing under `PGC_SUSET`; K5 adds the owner's refusal.
23. Disk: 45 % used (153 GB free); only `chela-neon-sccache` exists (P2). 16 cores: one cold fork build ≈ 1 h, one cold `extensions-all` build ≈ 2–3 h (to be measured by X0).

---

## K1: U16, temporary slots and the pageserver (opus / opus / opus)

**Checkout:** clone `<neon checkout>-k1`, Neon `round-4-k1`, Postgres `round-4-v17-k1` (P1).
**Files:** Postgres `src/backend/replication/slot.c`, `src/include/replication/slot.h`, `src/backend/access/transam/xlog.c`; Neon `pageserver/src/basebackup.rs`, `libs/postgres_ffi/src/pg_constants.rs`, new `test_runner/regress/test_temp_slot_aux_files.py`.
**Behaviour:** spec §2 rules 1–5.
**Checklist (§16 "Slots and basebackup"):** `neon-file:` copy and drop only for `RS_PERSISTENT`; `LogDiscardedSlotFiles()` after `XLogReportParameters()`, primary only, idempotent. Basebackup counts only persistent, long-enough states for `restart.lsn`; a short state is skipped with a warning, no panic; `REPL_SLOT_ON_DISK_OFFSETOF_PERSISTENCY`. Plus: pre-flight 1 (104 bytes), 2 (flush in the startup process); an ephemeral slot that fails creation logs nothing; a slot persisted after creation (`ReplicationSlotPersist`) is copied once; the discarded-name list freed after logging.
**RED tests (test agent):** Python `test_temp_slot_aux_files.py` (spec §2's four cases; run on today's fork build with `ci-local.sh --only build,regress -k test_temp_slot_aux_files`: each fails on its first assertion, the aux file is present); Rust unit in `basebackup.rs` for `persistent_slot_restart_lsn` (persistent, temporary, ephemeral, 0, 83, 95, 103 bytes, full): RED as a compile error on the missing helper only.
**Gotchas:** every primary has `wal_proposer_slot` (physical; untouched); `list_aux_files` takes an LSN (use the endpoint's current flush LSN after a checkpoint); a child branch for case 4 is created from an LSN before the cleanup.
**Verify:** `ci-local.sh --pg v17 --only build,rust-tests` (pageserver unit), `--only build,regress -k test_temp_slot_aux_files` (check `--collect-only` first).
**Live runs:** none (P6); `compute_fork_live::a_crashed_compute_leaves_no_slot_aux_file` is C2's, run at End of step 5.

## K2: W9, `chelabase_keys` streams (opus / opus / opus)

**Checkout:** clone `-k2`, Neon `round-4-k2`, Postgres `round-4-v17-k2`.
**Files:** `contrib/chelabase_keys/chelabase_keys.c`, `sql/stream.sql`, `expected/stream.out`, `Makefile` and `meson.build` (`stream`); Neon `test_runner/regress/test_chelabase_keys.py`.
**Behaviour:** spec §3 (contract additions; output format unchanged; RT4-13).
**Checklist (§16 "Logical decoding plugin"):** `stream_{start,stop,abort,commit,change,truncate}_cb`; no message or prepare callback. Per-relation `c` up to `max_tables`, then `all`; message at `txn->end_lsn` with the full xid; top-level abort, implicit crash abort, `ROLLBACK PREPARED`, DDL-only → nothing; subtransaction abort keeps the entry; `COMMIT PREPARED` of a streamed transaction writes the message; the entry freed on every end (RT4-13). Plus: a relation dropped mid-transaction (historic catalog), a TOAST-only change, two streamed transactions interleaved, the hash's memory context flat over a long session.
**RED tests (test agent):** C regress `stream.sql` (spec §3's cases; RED with `ci-local.sh --only pg-check`: streamed cases print keys, not `c`); Python `test_role_setting_streams_in_the_walsender` and the crash and long-session cases (spec §3), RED on today's build (`stream_txns = 0`).
**Gotchas:** `pg_regress` framing as round 3 P2 (`\set ECHO none`, `\pset tuples_only on`, `\pset format unaligned`); the regress runs with `--encoding=UTF8` and needs `neon_rmgr` (pg-check's `TEMP_CONFIG`); `debug_logical_replication_streaming = immediate` is a session setting; round 3's `chelabase_realtime` startup restriction refuses `options`, so the walsender test sets the role setting with `ALTER ROLE`, never `-c`.
**Verify:** `ci-local.sh --only pg-check`; `--only build,regress -k test_chelabase_keys`.
**Live runs:** none; `realtime_live::a_big_transaction_streams_through_the_rt_slot` is C4's (End of step 5).

## K3: compute_ctl internal server, token files, storage token refresh (opus / opus / opus)

**Checkout:** clone `-k3`, Neon `round-4-k3` (cold build first, P2).
**Files:** `compute_tools/src/http/server.rs`, new `http/middleware/internal_auth.rs`, `http/routes/{grants,extensions,status,configure}.rs`, new `http/routes/storage_token.rs`, `compute.rs`, `config.rs`, `spec.rs` (`do_control_plane_request` header only), `compute_promote.rs`, `spec_apply.rs`, `lsn_lease.rs` (SP5), `bin/compute_ctl.rs`, new `compute_tools/src/token_file.rs`; `libs/compute_api/src/{requests,responses,spec}.rs`; `control_plane/src/endpoint.rs`; `test_runner/fixtures/endpoint/http.py`, `neon_fixtures.py` (endpoint token path helper); `test_runner/regress/test_compute_http.py`.
**Behaviour:** spec §4 (bind, secret, middleware, delivery, §4.1 writer side, RT4-6, RT4-10), §8 fork rules (file at start and first in every reconfigure; no `NEON_AUTH_TOKEN`; `POST /storage_token`; capability; RT4-7, RT4-8, RT4-11), RT4-1's DuckDB pins, P4, P5, pre-flight 6, 7.
**Checklist (§16 "compute_ctl"):** Internal server: `127.0.0.1` (+ `::1` best effort), bearer required, constant time, no bypass, never logged (RT4-10); `/grants` and `/extensions` narrowed (RT4-6). Token files: 0600, postgres-owned, `O_EXCL` temp + `fsync` + rename + directory `fsync`, outside pgdata; paths in `postgresql.conf` after the spec's settings; storage token written at start, first in every reconfigure, and by `POST /storage_token` (RT4-3); none of it changes the status. No `NEON_AUTH_TOKEN` for the postmaster or `--sync-safekeepers`; config and token file before the sync (RT4-11). RT4-7: a spec without a token on an authenticated compute → 400, nothing changed. RT4-8: spec `Debug` redacts the token. `/status` and the spec fetch advertise `storage_token_refresh` (RT4-2). RT4-1: `duckdb.disabled_filesystems = 'LocalFileSystem'` pinned in `postgresql.conf`. Plus: a stale `<file>.tmp.*` from a crash is ignored and replaced; the token directory is pgdata's parent, never pgdata; `/grants` on a role named like `cb_m_*`, `chelabase_realtime` or carrying D1's marker refused (the marker predicate exactly as pinned under "Role marker", SP22); `/extensions` with a version string or a schema doesn't bypass the trusted check. Seam rulings: SP5 (a refresh also replaces the in-memory token `lsn_lease.rs` and compute_ctl's own pageserver calls read), SP6 (a compute started without a token refuses one later; the path lines are fixed at start), SP7 (`/storage_token` states, body rejections, error text), SP18 (the fixture interface K5 uses).
**RED tests (test agent):** compute_ctl Rust unit (spec §4 and §8 lists, `oneshot` on the routers); Python `test_compute_http.py::test_internal_api_needs_the_token`, `::test_internal_api_listens_on_loopback_only` (RED on today's build: 3081 answers without a bearer and listens on `::`). Rust RED compiles only up to the items under test (`StorageAuthToken`, `capabilities`, the flags).
**Gotchas:** K5 is not in this branch yet: Python tests with an auth-enabled compute can't read pages here (P3), so K3's Python RED/GREEN uses auth-off environments; the existing `/extensions`, `/grants`, `/failpoints` fixture users (`test_role_grants.py`, `test_extensions.py`, `test_download_extensions.py`, failpoint tests) must keep passing with the bearer from the file (run `test_role_grants`, `test_download_extensions`, `test_compute_http` once at the end). `cfg!(feature = "testing")` gates `/failpoints`.
**Verify:** `ci-local.sh --pg v17 --only build,rust-tests` (compute_tools, compute_api); `--only build,regress -k "test_compute_http or test_role_grants or test_download_extensions or test_extensions"` (collect first).
**Review fixes:** right after the review (K4, K5 build on it).
**Live runs:** none; `compute_fork_live::compute_ctl_internal_api_is_loopback_and_needs_the_token` is C2's.

## K5: neon extension token reader (opus / opus / opus)

**Checkout:** clone `-k5`, Neon `round-4-k5` from `round-4-k3` (P3).
**Files:** new `pgxn/neon/token_file.{c,h}`, `libpagestore.c`, `walproposer_pg.c`, `neon_walreader.c` (through `libpqwp_connect_start`), `extension_server.c`, `neon.c` (GUC registration), `neon.h`, `unstable_extensions.c`, `pgxn/neon/Makefile`; Python new `test_token_files.py`, `test_storage_token_reload.py`, `test_unstable_extensions.py` (owner refusal).
**Behaviour:** spec §4 callers, §4.1 reader side (RT4-12), §8 "every new connection reads the current file", RT4-1 (`PGC_SUSET`), P4.
**Checklist (§16 "neon extension"):** One `stat()` per connect or `:3081` call; identity from `fstat` of the open descriptor (RT4-12); content validated; fail closed on missing/empty/oversized/invalid, cached value dropped; one `WARNING` per state change, never the content. `127.0.0.1` URLs with the bearer; `neon.allow_unstable_extensions` `PGC_SUSET` (RT4-1). Plus: `--sync-safekeepers` (no shared memory, `CallExtMain`) reads the file from `postgresql.conf`; a reconnect inside a long statement takes the new file; `curl_slist` freed on every path; a symlinked token file is read like a file (compute_ctl never makes one).
**RED tests (test agent):** `test_token_files.py` and `test_storage_token_reload.py` (spec §4 and §8 Python lists, RT4-3/8/11/12 cases), on the `round-4-k3` build: they fail on their first assertion (the extension still reads no file); the owner's `SET neon.allow_unstable_extensions` case fails (it succeeds today). The `extensions-all` part of `test_token_files.py` is X6's, not here.
**Gotchas:** the test's token minting needs K7's numeric `exp` helper, which isn't in this branch: mint with PyJWT directly in the test (`jwt.encode({...,"exp": int}, env.auth_keys.priv, algorithm="EdDSA")`); the storage servers allow 60 s of `exp` leeway (`Validation::default()`), so an "expired" token is minted with `exp` at least 120 s in the past, or the test waits `exp + 61 s` (SP8); file paths and the refresh call come from K3's fixture names (SP18); a page read only reaches the pageserver with shared buffers and the LFC cleared (`neon_lfc` off, or `SELECT pg_prewarm` avoided, restart of the endpoint's backends).
**Verify:** `ci-local.sh --pg v17 --only build`; `--only build,regress -k "test_token_files or test_storage_token_reload or test_unstable_extensions or test_auth or test_compute_http"`.
**Then:** the coordinator merges `round-4-k3` and `round-4-k5` into `round-4` (`--no-ff`) and pushes.
**Live runs:** none; `an_expired_storage_token_is_refreshed_without_a_restart` is C2's.

## K4: compute_ctl external auth, metrics scope, realtime hba (opus / opus / opus)

**Checkout:** clone `-k4`, Neon `round-4-k4` from `round-4` after K3+K5.
**Files:** `compute_tools/src/http/middleware/authorize.rs`, `http/server.rs` (metrics routes behind `Authorize`), `bin/compute_ctl.rs` (`INSTANCE_ID`), `compute_tools/src/spec.rs` (realtime block), `config.rs`/`compute.rs` (rewrite on reconfigure), `libs/compute_api/src/{requests,spec}.rs`; `test_runner/fixtures/endpoint/http.py` (metrics with a token; `ComputeClaimsScope.METRICS = "metrics"`, SP17), `test_compute_http.py::test_metrics_need_a_token`, `test_member_logins_hba.py::test_realtime_logins`. Not `neon_fixtures.py` (K7 edits it in the same wave, SP17).
**Behaviour:** spec §5, §7 compute_ctl part (U4, S18, RT4-9), §10 fork rules.
**Checklist (§16 "compute_ctl", external auth part):** 401 for missing/malformed/non-Bearer, `INSTANCE_ID` bypass gone, no token logged, no `kid` unwrap; `Metrics` arm only for `/metrics` and `/autoscaling_metrics` (RT4-9). From §16 Chelabase W31: `clientcert=verify-full`, reject lines elsewhere, CN `chelabase-realtime`. Plus: realtime block absent / no TLS / below 17 → reject lines only; the realtime and member blocks each rewritten between their own markers on reconfigure; a `Metrics` token with an `exp` in the past → 401.
**RED tests (test agent):** compute_ctl Rust unit (spec §5, §7, §10 lists); Python `test_metrics_need_a_token` (RED: 200 without a token today) and `test_realtime_logins` (`VanillaPostgres` with TLS and the literal block; RED: the role logs in from anywhere through the catch-all).
**Verify:** `ci-local.sh --pg v17 --only build,rust-tests`; `--only build,regress -k "test_compute_http or test_member_logins_hba"`.
**Live runs:** none.

## K6: the personal-login marker guard (opus / opus / opus)

**Checkout:** clone `-k6`, Neon `round-4-k6`.
**Files:** `pgxn/neon/neon_ddl_handler.c` (marker GUC defined in the handler's init), `test_runner/regress/test_member_role_guard.py` (`test_marked_role_*`).
**Behaviour:** spec §12 fork rules (S15, S16).
**Checklist (§16 "Role guard C"):** Marker: a `pg_db_role_setting` row `(setdatabase = 0, setrole = oid)` holding `chelabase.managed_role=`, any value (S15); a database-scoped row doesn't mark. Every round-3 §4.1 statement form on a marked role: `CREATE ROLE` IN ROLE/ROLE/ADMIN, `ALTER ROLE/USER` (incl. `IN DATABASE … SET/RESET`), `ALTER GROUP ADD/DROP USER`, `RENAME`, `DROP ROLE`, `GRANT/REVOKE` grantees and granted roles; `CURRENT_USER`/`CURRENT_ROLE`/`SESSION_USER` resolved. Non-superuser `ALTER ROLE … SET/RESET chelabase.managed_role` and `GRANT/REVOKE SET|ALTER SYSTEM ON PARAMETER chelabase.managed_role` refused for any role; `ALTER ROLE ALL` already needs a superuser (assert). Nested statements (DO, PL/pgSQL, SPI) guarded; superusers untouched; nothing forwarded on a refusal. Plus: `RESET ALL` on a marked role refused, on an unmarked role unaffected; a role name that doesn't exist yet isn't looked up as marked; the lookup runs only for non-superusers.
**RED tests (test agent):** the spec §12 Python list (`VanillaPostgres` with the neon extension, as round 3's file); RED on today's build (the statements succeed).
**Verify:** `ci-local.sh --pg v17 --only build`; `--only build,regress -k "test_member_role_guard or test_ddl_forwarding"` (forwarding must not see refusals).
**Live runs:** none; the live tests are C3's.

## K7: storage auth (opus / opus / opus)

**Checkout:** clone `-k7`, Neon `round-4-k7`.
**Files:** `libs/utils/src/auth.rs`, `pageserver/src/auth.rs`, `pageserver/src/http/routes.rs`, `safekeeper/src/auth.rs`, `safekeeper/src/http/routes.rs`, `storage_controller/src/{auth,http}.rs`, the shared handlers' callers (`libs/http-utils` if the metrics/profile handlers need a permission hook); `test_runner/fixtures/auth_tokens.py`, `neon_fixtures.py` (pageserver teardown metrics, storcon `get_metrics`), `test_auth.py`.
**Behaviour:** spec §6, §7 (storage part, S7, S8, RT4-5), pre-flight 9–11.
**Checklist (§16 "Pageserver / safekeeper / storcon auth"):** `Scope::Metrics`; `check_permission` total (no `tenant_id` unwrap), exhaustively tested. `/metrics`: set `{Metrics, Admin, own global}`; profiles `{Admin, own global}`; `Metrics` refused everywhere else, libpq and the safekeeper WAL service included (RT4-5). `/v1/status`, `/status`, `/ready`, `/v1/doc`, `/swagger.yml` stay public (and storcon `/live`); auth off unchanged.
**RED tests (test agent):** Rust unit: the exhaustive `Scope × tenant_id × route tenant` table per crate (invariant: total, no panic) and a route table per server; Python `test_auth.py::test_tenant_token_without_tenant_id`, `::test_metrics_and_profiles_need_a_token` (RED: 200 without a token; a panic on the tenant route). The agent lists P16's auth-enabled metric readers in its report.
**Verify:** `ci-local.sh --pg v17 --only build,rust-tests`; `--only build,regress -k "test_auth or test_pageserver_api or test_safekeeper_deletion"` plus the P16 list's tests that its fixture change touches.
**Live runs:** none; `storage_auth_edges::metrics_scope_reads_metrics_only` is C5's.

## K8: proxy secrets (sonnet / sonnet / sonnet)

**Checkout:** clone `-k8`, Neon `round-4-k8`.
**Files:** `proxy/src/binary/proxy.rs`, `proxy/src/binary/local_proxy.rs`, `proxy/src/compute_ctl/mod.rs`, the `LocalBackend` constructor.
**Behaviour:** spec §9 (S11), §4 local_proxy and the testing backend, pinned flags.
**Checklist (§16 "Proxy"):** `--redis-plain-password-file` fails closed (empty, missing, URL with a password); no password in errors, logs or `Debug` (RT4-13). `local_proxy` and the testing `Local` backend read `--compute-ctl-token-file` before each call; missing/invalid → no header → refused; a missing file at startup is not a startup error (SP19). Plus: a password with URL-reserved characters (`@`, `:`, `/`, `%`) is percent-encoded into the connection info, never spliced as text.
**RED tests (test agent):** proxy Rust unit (spec §9 list; the token file read per call). RED as compile errors on the new flags only.
**Verify:** `ci-local.sh --pg v17 --only build,rust-tests` (proxy).
**Live runs:** none; `proxy_cache_live`, `valkey_acl_live` at End of step 5.

## K9: storcon backoff (sonnet / sonnet / none, leaf)

**Checkout:** clone `-k8` reused after K8 merges (same volumes), Neon `round-4-k9`.
**Files:** `storage_controller/src/persistence.rs`, its metrics module; `test_storage_controller.py::test_serialization_failures_back_off`.
**Behaviour:** spec §11 (S14).
**Checklist (§16 "Storage controller persistence"):** Full-jitter backoff (S14), connection returned before the sleep, 20 retries, retry histogram; the migration loop's dead arm removed.
**RED tests (test agent):** Rust unit (delay bounds per `n`, cap, seeded spread; a fake failing `k` times with a paused clock); the Python burst test (RED: some requests fail today; if it can't be made to fail deterministically on the base, the agent says so and the unit tests carry RED).
**Verify:** `ci-local.sh --pg v17 --only build,rust-tests`; `--only build,regress -k test_serialization_failures_back_off`.
**Live runs:** none; `rest_live` at End of step 5.

## X0: removals, shared Dockerfile regions, harness (sonnet / sonnet / sonnet)

**Checkout:** clone `-x0`, Neon `round-4-x0`.
**Files:** `compute/compute-node.Dockerfile` (P8's shared regions; the `pgjwt`, `pgrag`+onnxruntime, `pg_tiktoken`, `pgx_ulid-src`/`pgx_ulid-build` (the v14–v16 0.1.5 build, `:1291-1314`; **not** `pgx_ulid-pgrx12-*`, which is the v17 build X1 bumps, SP14), `pg_mooncake` stages), `compute/manifest.yaml`, `compute/patches/{onnxruntime,duckdb_v113}.patch`, `docker-compose/{docker_compose_test.sh,test_extensions_upgrade.sh,run-tests.sh}`, `docker-compose/ext-src/{README.md,pgjwt-src,pgrag-src,pg_tiktoken-src,rag_bge_small_en_v15-src,rag_jina_reranker_v1_tiny_en-src}`, new `docker-compose/chelabase-ext-test.sh`, new `docker-compose/tests/chelabase-ext-test_test.sh`.
**Behaviour:** spec §17.1 (with pre-flight 14), P7, P8, P9. The wrapper: `chelabase-ext-test.sh [--storage-image <ref>] [--only <dir,…>] [--keep-images <tag>]` builds the compute (`EXTENSIONS=all`) and `extension-tests` targets from its checkout, tags what `docker-compose/docker-compose.yml` expects, and runs `docker_compose_test.sh` with `TEST_VERSION_ONLY=17` and `ONLY`.
**Checklist (§16 "Compute extensions"):** Removals complete in `compute-node.Dockerfile`, `compute/manifest.yaml`, `docker-compose/` (ext-src dirs, `docker_compose_test.sh`, `test_extensions_upgrade.sh`, `ext-src/README.md`), patches, the build-tools image's `sql_exporter` pin (X5) and `test_compute_metrics.py` (X5). Plus: `grep -rn "mooncake\|pgrag\|pgjwt\|tiktoken\|rag_bge\|rag_jina\|onnx"` over the fork (excluding `vendor/`, `target/`) finds nothing; the `extensions-minimal` stage is timescaledb, pg_cron, pg_partman.
**RED tests (test agent):** shell `chelabase-ext-test_test.sh` (the wrapper's argument handling, the `ONLY` selection and the SKIP list, with `docker` and `docker compose` stubbed; the removal grep). RED: the scripts don't exist / the grep finds the names.
**First, as the base for X6 and the cache:** the coder runs the wrapper once on the base commit (`--keep-images round4-base`), recording build time and the harness result in its report; then on its own commit.
**Verify:** the shell test; one wrapper run on the final commit (`--only` the remaining dirs' smallest suite, e.g. `pg_uuidv7-src`, to prove the plumbing; the whole harness runs at End of step 4).
**Review fixes:** right after the review (X1–X6 build on it).
**Live runs:** none.

## X1–X5: extension bumps (models in the table)

Common to every X task: clone `-x<n>`, Neon `round-4-x<n>` from `round-4` after X0; files limited by P8; exact tag and sha256 (or tag commit, asserted after the clone) from spec §17.2; a patch that no longer applies is reworked, never dropped silently; the test agent first runs the task's suites on the base images (`round4-base`) through the wrapper with `--only` (RED: the version check fails, or the suite is skipped per P9), then writes a version assertion per extension in the task's `ext-src` dir (`SELECT extversion`) as its RED; the coder makes the build and the suites green; verification is `chelabase-ext-test.sh --only <the task's dirs>`. **Checklist (§16 "Compute extensions"):** Each bump: exact tag, the sha256 from §17.2 (or the tag's commit for a `git clone`), the extension's own regression green on `extensions-all`; a patch that no longer applies is reworked, never dropped silently. One pgrx layer at 0.19.2 (S20); patched `Cargo.toml`s pin `pgrx`/`pgrx-tests` `=0.19.2`; no `sed` hacks left; `pg_repack` built from `pg-build`.

- **X1 pgrx (sonnet / opus / none):** one layer named `rust-extensions-build` (`cargo install --locked --version 0.19.2 cargo-pgrx`; every pgrx stage, `pgx_ulid-pgrx12-build` and `pg-anon-pg-build` included, builds `FROM` it; `pg_repack-build` keeps building on it until X4 moves it to `pg-build`, SP15), the 0.12/0.14 layers and every `sed` deleted; the proxy change compiles (`ci-local.sh --pg v17 --only build`, SP15); pg_graphql 1.6.2, pg_jsonschema 0.3.4, pg_session_jwt 0.5.0 (its `pgrx-tests` member too), pgx_ulid 0.2.3, pg_anon 3.2.3 (`anon_v2.patch` reworked or dropped with the reason); `proxy/src/serverless/local_conn_pool.rs` `EXT_VERSION` and `jwt_session_init` check (pre-flight 8); un-skip `pg_jsonschema-src` (P9). If a newer upstream release already asks for 0.19.x, take it instead of a patch (§17.3).
- **X2 PostGIS (sonnet / sonnet / none):** CGAL v6.2.1 tarball with sha256 (pre-flight 12), SFCGAL 2.3.0, PostGIS 3.6.4, pgrouting 4.0.2, h3 + h3-pg 4.5.0 from `postgis/h3-pg`; `libsfcgal1` dropped from the final stage; the `extension-tests` stage's `libgdal32`/`libproj25` pins re-checked.
- **X3 heavy (sonnet / opus / opus):** rdkit Release_2026_09_1 (CMake flags re-checked; un-skip `rdkit-src`, P9), plv8 v3.2.5 (`plv8_v3.2.5.patch` reworked; `plv8_v3.1.10.patch` removed if unused), pg_duckdb v1.1.1 (tag commit `7b0db3737a1ab2dfb182b742322426e3c4b500af` asserted; `duckdb.postgres_role` unset; the privileged-role grant workaround `compute-node.Dockerfile:1515-1517` and `pg_duckdb_v031.patch`/`duckdb_v120.patch` reworked or dropped, each with its reason). Review: opus, RT4-1 focus (no path lets a non-superuser run DuckDB or reach a file or URL).
- **X4 C extensions (sonnet / sonnet / none):** timescaledb 2.31.0 (un-skip `timescaledb-src`, P9; pgaudit after it in `shared_preload_libraries` holds), pg_partman v5.5.0, pg_cron v1.6.8 (`pg_cron.patch` re-checked), and rows 4–5, 18–32 (pgvector's `hnswbuild.c` hunk reworked and the `vector--0.7.4.sql` trick re-checked; pg_repack built `FROM pg-build`).
- **X5 binaries (sonnet / sonnet / none):** pgbouncer 1.26.1 (tag commit asserted), postgres_exporter v0.20.1, pgbouncer_exporter v0.12.1, sql_exporter 0.24.11 (binary sha256 per arch from §17.2) in the compute Dockerfile, `build-tools/Dockerfile` and `test_runner/regress/test_compute_metrics.py`; the compute's sql_exporter query config re-checked against 0.24; the build-tools image built once locally (`ghcr.io/chelabase/neon-build-tools:<sha12 of build-tools/Dockerfile>`, not pushed until ship); `ci-local.sh --only build,regress -k test_compute_metrics` on it; the report says whether the active buildx builder uses the `docker` driver, so `publish-dev.sh`'s storage build finds the local tag (SP21).

## X6: the RT4-1 image test (opus test agent; leaf)

**Checkout:** clone `-x6`, Neon `round-4-x6` from `round-4` after X0, X3, K3+K5.
**Files:** a harness test dir `docker-compose/ext-src/chelabase-trusted-ext-src/` (its `neon-test.sh` and SQL/shell) run by the wrapper; `docker-compose/compute_wrapper/var/db/postgres/configs/config.json` (a dummy `storage_auth_token`, so the harness compute writes both token files; the harness pageserver runs without auth and ignores it, SP16). The test first asserts, as `cloud_admin`, that both files exist (a refusal must be a privilege refusal, not a missing file).
**Behaviour:** spec §4 Tests (the `extensions-all` paragraph) and §17.5: as the spec's owner role (a `pg_monitor` member, non-superuser), enumerate `pg_available_extensions` whose control file is `trusted`; create each; point its file readers and HTTP clients at both token files and at `127.0.0.1:3080/3081`: all refused; `pg_mooncake` absent; `SET neon.allow_unstable_extensions` refused; `SHOW duckdb.disabled_filesystems` includes `LocalFileSystem` and the two auto-load/install settings are `off`.
**RED:** run on `round4-base` images through the wrapper (`--only chelabase-trusted-ext-src`): fails on its first assertion (`pg_mooncake` present). GREEN is End of step 4's harness run on the final build.

## C1: `crates/chelabase-neon` (sonnet / sonnet / opus)

**Worktree:** `just wt-add neon-fork-round-4-c1 neon-fork-round-4`.
**Files:** `crates/chelabase-neon/src/{jwt,spec,reconfigure,compute_tls}.rs`, `crates/chelabase-neon/tests/{storage_auth,storage_auth_edges,reconfigure_mock}.rs` (the byte-for-byte token comparisons; the `metrics_scope_reads_metrics_only` live test is C5's), callers' compile fixes only (`services/worker/src/reset.rs`, `services/orchestrator/src/internal/spec.rs`: pass a `SpecPolicy` with today's values and `storage_token_expiring: false`).
**Behaviour:** pinned interfaces above; spec §7 Chelabase (U2, S17), §8 Chelabase (`tenant_token`, RT4-2 header), §10 Chelabase (`REALTIME_CN`), §17.4 (S21).
**Checklist (§16 "Chelabase", C1's part):** `tenant_token` expiring only for capable computes (the caller decides; C1 provides the switch); metrics token `scope` + `iat`, no `exp`, no `aud` (the storage servers' `Validation` has no audience and would refuse one), two mints differ; one token for storage servers and compute_ctl (S17); W31 CN `chelabase-realtime`. Callers' interim `SpecPolicy.realtime_logins` is `MemberLogins::realtime(Vec::new())`, replaced by C2 (worker) and C4 (orchestrator) (SP4). Seam rulings: SP2, SP3, SP23. Plus: `advertises_storage_token_refresh` on absent, empty, `" storage_token_refresh , x"`, and a near-miss name.
**RED tests (test agent):** unit tests for every pinned item (spec §7, §8, §10 Chelabase unit lists); RED as compile errors on the items under test.
**Verify:** `cargo test -p chelabase-neon`, `cargo clippy -p chelabase-neon --all-targets -- -D warnings`, `cargo build -p worker -p orchestrator`.
**Review fixes:** right after the review (C2–C5 build on it).
**Live runs:** none.

## C2: worker storage tokens, realtime settings, certificate sweep (sonnet / sonnet / opus)

**Worktree:** `neon-fork-round-4-c2` from the branch after C1.
**Files:** `services/worker/src/{config.rs,reset.rs,certs.rs,ports.rs,fakes.rs,main.rs,lib.rs}`, new `services/worker/src/token_refresh.rs`, `services/worker/tests/compute_fork_live.rs` (new RED live tests: `a_crashed_compute_leaves_no_slot_aux_file`, `compute_ctl_internal_api_is_loopback_and_needs_the_token`, `an_expired_storage_token_is_refreshed_without_a_restart`; the deterministic-token assumptions there updated).
**Behaviour:** spec §8 Chelabase (RT4-2, RT4-3, S9), §10 Chelabase (sweep renews both pairs; `CHELABASE_REALTIME_LOGIN_CIDRS` feeds `SpecPolicy.realtime_logins`).
**Checklist (§16 "Chelabase worker…"):** `tenant_token` expiring only for capable computes (worker `/status` via `report()` before each `build_and_push_spec`); the refresh loop through `POST /storage_token` under `lock_endpoint`, failures retried, never a spec push (RT4-3). Plus: a compute that stops advertising (image rolled back) gets a non-expiring token at its next push; the loop skips stopped computes and those without the capability; consecutive failures counted per endpoint; the loop's interval read once at startup. Seam rulings: SP2 (a failed `report()` fails that push, never a silent non-expiring token; `report()` and `push_storage_token` bounded by `DOCKER_TIMEOUT`), SP4 (replace C1's interim realtime logins), SP7 (`412` is a counted, retried failure), SP8 (the live expiry test mints with `tenant_token(tenant, real_now - STORAGE_TOKEN_TTL + 30, true)` and waits `exp + 61 s`), SP9 (every `compute_fork_live` login as `chelabase_realtime`, the round-3 ones at `:186-210` included, dials the compute's control address with the realtime pair), SP10 (worker reads `CHELABASE_REALTIME_TLS_DIR`; the sweep writes a missing pair), SP12 (no `curl` in the compute image).
**RED tests (test agent):** unit with fakes (spec §8 Chelabase list), config tests for both settings; live tests compiled `--no-run` and traced by reading (P6).
**Verify:** `cargo test -p worker`, clippy, `cargo test -p worker --test compute_fork_live --no-run`.
**Live runs:** none (P6); its live tests run at End of step 5.

## C3: personal logins (sonnet / sonnet / opus)

**Worktree:** `neon-fork-round-4-c3` from the branch after C2 (P13).
**Files:** `crates/chelabase-types/src/role_name.rs`; `crates/chelabase-metadata/migrations/20261021000001_personal_logins.sql`, `crates/chelabase-metadata/src/neon.rs` and every `RoleOrigin` match site (pre-flight 16, plus `services/orchestrator/src/roles.rs`, whose `BranchRole.origin` is in the public API, SP13); `openapi/*.json` and the generated clients (`just api-gen`, committed; `just check` refuses stale ones, SP13); `services/worker/src/{roles.rs,resync.rs}` (`create_personal_login`, re-marking on recreate); `services/worker/tests/compute_fork_live.rs` (`a_personal_login_is_guarded`, `a_recreated_personal_login_is_marked_again`).
**Behaviour:** spec §12 Chelabase, RT4-4, P10.
**Checklist (§16 "Role guard C", RT4-4 line, and "Chelabase", personal logins):** every create/recreate of a personal login marks it in the same transaction; resync re-marks; never a spec role (personal rows excluded from `neon_roles` → spec roles). The SQL helpers, `create_personal_login`, the resync's re-marking. Plus: a name with `"` and with upper case quoted; `CREATE ROLE` fails on an existing name (never adopts); the resync's recreate script for a non-personal row is unchanged.
**RED tests (test agent):** unit (helpers, resync script for personal vs other rows), metadata `live_db` origin round-trip and "`neon_roles`/`neon_roles_versioned` never return a personal row" (they already select `origin = 'owner'` only, `neon.rs:744-752`, so the spec builder needs no change; the test pins it, SP13), compiled `--no-run`; live tests `--no-run`.
**Verify:** `cargo test -p chelabase-types -p chelabase-metadata -p worker -p orchestrator`, clippy, live targets `--no-run`.
**Live runs:** none (P6).

## C4: orchestrator and realtime (sonnet / sonnet / opus)

**Worktree:** `neon-fork-round-4-c4` from the branch after C1.
**Files:** `services/orchestrator/src/internal/{spec.rs,hooks.rs,data.rs,mod.rs,config.rs}` (`hooks.rs`: the storcon-notify reconfigure, SP1), `services/orchestrator/src/main.rs`, `services/orchestrator/tests/internal.rs` (constructs `InternalState`; C3 edits it too, SP20); `services/realtime/src/{config.rs,app.rs,slots.rs,decoder.rs,orchestrator.rs,pool.rs}`; `services/realtime/tests/realtime_live.rs` (`a_big_transaction_streams_through_the_rt_slot`, renamed from `…_spills_…`); `services/worker/tests/realtime_role_live.rs` (`realtime_logs_in_only_with_its_certificate`).
**Behaviour:** spec §8 RT4-2 (the spec hook reads `Compute-Capabilities`), §10 Chelabase (S13), §3 Chelabase.
**Checklist (§16 "Chelabase"):** W31: CN `chelabase-realtime`, `clientcert=verify-full`, CIDRs realtime + runner, reject lines elsewhere; realtime on `control` at `.12` dialing `replication_host` for replication only; pair renewed by the worker sweep (C2), reloaded by realtime (every 60 s). Plus: the spec hook treats a missing header as "not capable"; `replication_host` absent → the slot isn't opened (no fallback); realtime's check pools keep `host`. Seam rulings: SP1 (the hook's reconfigure reads `status_report` first and builds the same `SpecPolicy`), SP4, SP9 (`realtime_role_live`/`realtime_live` log in as `chelabase_realtime` via the control address with the realtime pair; realtime's unit tests that build its config from env set `CHELABASE_REALTIME_TLS_DIR`), SP11, SP20, SP23.
**RED tests (test agent):** unit (header parsing in the hook, the route's new field, realtime config requiring `CHELABASE_REALTIME_TLS_DIR`, the replicator dialing `replication_host` with the client cert); live tests `--no-run`.
**Verify:** `cargo test -p orchestrator -p realtime`, clippy, live targets `--no-run`.
**Live runs:** none (P6).

## C5: CLI/workbench, Compose, scripts (sonnet / sonnet / sonnet)

**Worktree:** `neon-fork-round-4-c5` from the branch after C1–C4.
**Files:** `tools/cli/src/workbench/{service_auth.rs,env.rs,paths.rs}`, the `chelabase neon mint-metrics-token` command (`tools/cli/src/neon/`), `tools/cli/src/dev/bootstrap.rs` (`ensure_machine_keys` issues the realtime pair), `infra/compose/docker-compose.yml` (proxy entrypoint and command, realtime on `control` at `.12` with `realtime-tls` ro, worker `realtime-tls` rw, runner `realtime-tls` ro and `metrics.token` ro, `CHELABASE_REALTIME_*`, `CHELABASE_STORAGE_TOKEN_REFRESH_SECS`), `scripts/tests/{compose-secret-argv.sh,workbench-compose.sh,compose-key-visibility.sh}`, `scripts/compute-network-e2e.sh`, `scripts/tests/fixtures/workbench-stack.env`, `crates/chelabase-neon/tests/storage_auth_edges.rs` (`metrics_scope_reads_metrics_only`).
**Behaviour:** spec §7 Chelabase (U2), §9 Chelabase, §10 Chelabase (addresses, mounts), §13.4 rows for D2, item 4, D3, item 8, W31.
**Checklist (§16 "Chelabase", C5's part):** metrics token kept across runs, re-minted by the CLI command; `compute-network-e2e.sh`: no `:3081` listener, 401 on `:3080` (`/status`, `/metrics`, `/autoscaling_metrics`), `known_gaps` empty; `compose-secret-argv.sh` without the proxy gap (and fails if `--redis-plain=` carries `:…@`). Plus: the key-visibility table lists `realtime-tls` and `metrics.token` per service; `.12` outside the compute range for every slot; env.rs renders `CHELABASE_REALTIME_CONTROL_IP`, `CHELABASE_REALTIME_TLS_DIR` (worker, realtime, runner) and `CHELABASE_REALTIME_LOGIN_CIDRS` = `<realtime>/32,<runner>/32` (worker, orchestrator) (SP10).
**RED tests (test agent):** CLI unit (env rendering, `write_storage_auth` keeps `metrics.token`, the mint command replaces it), shell tests (RED on today's files); `storage_auth_edges` live test `--no-run`.
**Verify:** `cargo test -p chelabase`, clippy, `scripts/tests/compose-secret-argv.sh`, `scripts/tests/workbench-compose.sh`, `scripts/tests/compose-key-visibility.sh`, live target `--no-run`.
**Live runs:** none (P6).

## C6: docs (haiku; leaf)

**Worktree:** `neon-fork-round-4-c6`. **Files and text:** `apps/docs/content/docs/architecture/security.mdx` (compute_ctl's internal API on loopback behind a per-start secret; storage tokens expire after 24 h and are refreshed hourly for capable computes; the read-only `metrics` scope and its token file, which never expires and is rotated by re-minting, revoked only by rotating the compute-JWT key pair; realtime logins only from realtime's address with its certificate); `infra/neon/README.md` "Round 4" paragraph (one sentence per item, the removed extensions named, `<sha12>` left for ship). **Verify:** the end-of-step review reads the diff.

## K10: fork integration (sonnet; leaf)

**Checkout:** `<neon checkout>` on `round-4`. Merge `round-4-v17-k1` and `-k2` into `round-4-v17` (push); move `vendor/postgres-v17` to its head and set `vendor/revisions.json`'s v17 sha (version `17.11`); merge every Neon task branch not yet merged (`--no-ff`); `FORK.md`: "Postgres changes" gains U16; "Compute extension changes" gains token files, the marker guard, `allow_unstable_extensions` SUSET, the stream callbacks; a "Compute image" entry lists the bumps and removals. **Verify:** `ci-local.sh --only build` compiles; the gate is End of step 4's.

---

## End of step

0. Done before this plan: the spec's red-team (opus, RT4-1..13). At the step's start `main` is green (RT1 merged `87d2d633`; `just check` green there). Before W1: the seam pass (opus) and the user's approval of this plan.
1. Whole-branch review (opus) on `main..neon-fork-round-4` and the fork diffs `chelabase/neon` `main..round-4`, `chelabase/postgres` `REL_17_STABLE_chelabase..round-4-v17`; it covers the leaf tasks (P14).
2. The one final fix round: the review's findings plus every deferred task finding (Status), batched by area into pipelines (test agent → coder → audit, models by area as in the task table).
3. `just --justfile <wt>/justfile --working-directory <wt> check`, once.
4. Fork gate and the one image build. First list the pending fork changes in the Status (must be none: every task branch merged, `round-4` and `round-4-v17` pushed). In `<neon checkout>` on `round-4`:
   - `.github/scripts/ci-local.sh` (default: lint, build, Rust tests, every major; background);
   - `.github/scripts/ci-local.sh --only pg-check` (core `make check`, `chelabase_keys` with `stream`);
   - `.github/scripts/ci-local.sh --only regress -k "test_temp_slot_aux_files or test_chelabase_keys or test_chelabase_realtime_role or test_compute_http or test_token_files or test_storage_token_reload or test_auth or test_member_logins_hba or test_member_role_guard or test_serialization_failures_back_off or test_download_extensions or test_compute_migrations or test_role_grants or test_extensions or test_unstable_extensions or test_compute_metrics or test_replica_promotes or test_compute_reconfigure or test_rest_broker or test_ddl_forwarding or test_logical_replication or test_route_scopes or test_storage_auth_edges or test_compute_termination"` plus P16's additions (check with `--collect-only` first; deselect `test_ddl_forwarding_model`, hours);
   - `.github/scripts/publish-dev.sh --no-push --allow-unpushed` (storage and minimal compute, `ghcr.io/chelabase/neon-{storage,compute-v17}:<sha12>-dev`, variant test); re-tag as `chelabase/neon-*:<sha12>-dev`;
   - `docker-compose/chelabase-ext-test.sh --extensions minimal --storage-image ghcr.io/chelabase/neon-storage:<sha12>-dev --only timescaledb-src,pg_partman-src,pg_cron-src` (the minimal compute and `extension-tests-minimal` images from the same commit; a test build, never pushed, P15). The `extensions-all` build and X6's RT4-1 image test belong to the compute-extensions step; `extensions-all` is not published or used until then (it still ships pg_duckdb 0.3.1).
   - on `neon-fork-round-4`: `infra/neon/NEON_REF` = `round-4`'s full sha, commit `neon: pin round-4 head for the live pass`, push; then the plan's one stack: `just --justfile <wt>/justfile --working-directory <wt> workbench up` (P6; local `<sha12>-dev` images, `scripts/neon-images.sh`), Zitadel bootstrap per the roadmap's working rules, `just create-db-demo`.
5. Live pass on that stack, one suite at a time, whole files, once, in this order (no two overlap; nothing restarts an app; the stack was created on the new flags, mounts and networks):
   1. `just compute-network-e2e` (first, alone: it audits every compute on the network);
   2. `just workbench test -p worker --test compute_fork_live -- --run-ignored only` (own branches: crash, token expiry, personal logins, internal API);
   3. `just workbench test -p worker --test realtime_role_live -- --run-ignored only`;
   4. `just workbench test -p realtime --test realtime_live -- --run-ignored only`;
   5. `just workbench test -p chelabase-neon --test storage_auth -- --run-ignored only`, then `--test storage_auth_edges`, `--test storcon_live`, `--test proxy_cache_live`, `--test valkey_acl_live`, `--test docker_backend_live`;
   6. `just workbench test -p worker --test member_roles_live -- --run-ignored only`, then `--test roles_live`, `--test branch_live`, `--test neon_live`;
   7. `just workbench test -p rest --test rest_live -- --run-ignored only`; `-p orchestrator --test api`; `-p chelabase-metadata --test live_db`;
   8. e2e: `just neon-e2e`, `just data-e2e`, `just sql-editor-e2e`, `just role-e2e`, `just branch-e2e`, `just console-e2e`, `just storage-leak-e2e`.
   A failure goes to a debug agent (sonnet; opus for fork, auth, TLS or concurrency causes); a fork fix means a new fork commit → the gate items it touches, a new image build, and the pass again from the failed suite (Status).
6. `docs/landing-highlights.md` on `main`: what round 4 solves (no secret on a compute's command line, environment or SQL-visible setting; tokens that expire and refresh without restarts; metrics and profiles behind a read-only scope; realtime logins bound to realtime; large realtime transactions streamed; personal logins guarded; pgrag removed; timescaledb, pg_partman, pg_cron, pgbouncer and the exporters on current, patched releases), what it unlocks (the Members page's personal logins; scraping metrics safely), evidence (the suites above), the original Neon base vs ours. Update the plan Status and the roadmap's "Resume here", then **pause for the user**, naming the user-facing removals and changes below.
7. Ship, after the user's go: fast-forward `chelabase/postgres` `REL_17_STABLE_chelabase` and `chelabase/neon` `main` to the tested commits (direct push, same SHAs); push X5's build-tools image; `publish-dev.sh --push-only` (prints the digests); on `neon-fork-round-4`: both Compose digests (`infra/compose/docker-compose.yml`), `infra/neon/README.md` Round 4 tag, commit `neon: bump to fork round 4`, push; `just neon-test`; merge into `main` (`--no-ff`), push; purge the stack (`just wt-remove neon-fork-round-4`), delete the branch and every task branch (fork `round-4-*`, `round-4-v17-*`, Chelabase `neon-fork-round-4-*`) on origin and locally; remove the task clones and their volumes; prune the extension build cache.

### User-facing removals and changes (for the merge pause; rewritten for the minimal-only scope, end-of-step review 2026-10-21)

- **Removed extensions:** `rag`, `rag_bge_small_en_v15`, `rag_jina_reranker_v1_tiny_en` (pgrag; the only removal from the minimal image Chelabase runs). The unpublished `extensions-all` variant also loses `pg_mooncake`, `pgjwt`, `pg_tiktoken` and the v14–v16 `ulid` 0.1.5 build (no Chelabase compute ran them).
- **Bumped (minimal):** timescaledb 2.17.1 → 2.31.0 (an existing install needs `ALTER EXTENSION … UPDATE`; the 2.17.1 library is gone); pg_partman 5.1.0 → 5.5.0 (`pg_partman_bgw.role = postgres`, worker stays off); pg_cron 1.6.4 → 1.6.8 (`extversion` stays 1.6). Binaries: pgbouncer 1.26.1, postgres_exporter 0.20.1, pgbouncer_exporter 0.12.1, sql_exporter 0.24.11.
- **Owners can no longer `SET neon.allow_unstable_extensions`.**
- **Metrics need a bearer:** `/metrics` and `/profile/*` on the pageserver, safekeepers and storcon, and compute_ctl's `/metrics` and `/autoscaling_metrics`. The `metrics` token is `.chelabase/neon/storage-tokens/metrics.token`; `chelabase neon mint-metrics-token` re-mints it; the old token stays valid until the compute-JWT key rotates.
- **compute_ctl:** `:3081` loopback-only behind the per-start secret; `/grants` and `/extensions` narrowed; no `INSTANCE_ID` bypass; `:3080` answers 401 without a bearer; computes no longer read `NEON_AUTH_TOKEN`.
- **Storage tokens expire** after 24 h on the new image and the worker refreshes them (`CHELABASE_STORAGE_TOKEN_REFRESH_SECS`, default 3600); a worker outage of ~23 h or more makes new storage connections fail.
- **No tenant token on pageserver control-plane routes** (user ruling): shard split, heatmap upload, secondary download/status, wait_lsn, location_config (GET/PUT), scan_remote_storage, break, tenant delete, tenant config.
- **`chelabase_realtime` logins** only from realtime's control address (and the runner's) with its client certificate; new settings `CHELABASE_REALTIME_LOGIN_CIDRS` (worker, orchestrator; unset = startup error), `CHELABASE_REALTIME_TLS_DIR` (realtime, required), `CHELABASE_REALTIME_CONTROL_IP` (Compose); realtime joins `control` at `.12`; new key dir `.chelabase/realtime-tls/`.
- The proxy takes `--redis-plain-password-file` (and `--control-plane-token-file` after F1).
- Storcon retries serialization failures 20 times with backoff (was 128).
- New: `chelabase neon mint-metrics-token`; the management API's role `origin` gains `personal` (no rows yet).
