# Neon fork round 3: keys plugin, realtime restriction, member logins, born-closed databases (implementation plan)

**Goal:** One fork round and one image build that ship: the `chelabase_keys` logical decoding output plugin (D1); the `chelabase_realtime` walsender restriction (D2); member roles that only a superuser changes and that log in only from the orchestrator, by address and TLS client certificate (D3); `REASSIGN OWNED` firing the DDL event triggers (D4); databases born closed on a protected branch (D5); compute_ctl migrations before `running` and a worker that waits for it (D6). Chelabase gets compute TLS (a platform CA, per-compute server certificates, the orchestrator's client certificate), a `control` network, and the reconcile that guards and opens born-closed databases.

**Spec:** `docs/superpowers/specs/2026-10-15-neon-fork-round-3-design.md` (`674ebf8d`; decisions D1–D6, user rulings U1–U3, red-team rulings RT3-1 to RT3-15). Implementers read the spec section their task names.

**Architecture:**
- Postgres fork (`chelabase/postgres`, v17 only): `contrib/chelabase_keys/` (T2); `walsender.c`, `postinit.c`, `slot.c`, new `src/include/replication/chelabase.h` (T3); `utility.c`, `cmdtaglist.h`, `hba.c`, `event_trigger` regression, `event-trigger.sgml` (T4).
- Neon fork (`chelabase/neon`): `.github/scripts/ci-local.sh` (T1); `pgxn/neon/neon_ddl_handler.c` (T5); `compute_tools/` and `libs/compute_api/` (T6); submodule bump, `vendor/revisions.json`, `postgres.mk` (T7).
- Chelabase: `crates/chelabase-neon` (`compute_tls`, `spec`, `backend`, `reconfigure`; T8), `services/worker` (T9), `services/orchestrator` and `crates/chelabase-types` (T10), `tools/cli`, `infra/compose`, `scripts/tests` (T11), docs (T12).

**Tech:** C (Postgres 17.11 fork), Rust (compute_ctl; Chelabase workspace on toolchain 1.99.0), Python (Neon's pytest suite, psycopg2 `^2.9.10`). New Chelabase dependencies, latest on crates.io (checked 2026-10-15): `rcgen` 0.14.10 (`default-features = false`, `features = ["aws_lc_rs", "pem"]`), `x509-parser` 0.18.1, `tokio-postgres-rustls-improved` 0.17.1 (`default-features = false`, `features = ["aws-lc-rs"]`; rustls 0.23 like the workspace's 0.23.45, aws-lc-rs 1.18.1), `tar` 0.4.46; already in the workspace: `arc-swap` 1.9.2, `tokio-rustls` 0.26.5, `rustls-pki-types` 1.15.1. **Fork change: yes**, so one fork gate, one image build, one `NEON_REF` bump (End of step 4).

**Standing rule:** the coordinator writes and commits each task's RED tests (below) before the brief, runs them RED and records the result in the Status. Implementers make the tests pass without editing, weakening, skipping or deleting them. Reviewers check that `git diff <red-commit>..HEAD -- <test files>` is empty. When a RED commit needs a module to exist so that its tests compile, the coordinator adds only the empty module declaration.

## Status

- **RESUME HERE (2026-10-15): PAUSED for the user's merge decision (End of step 6 done).** Everything is done and green: tasks T0b–T12, the end-of-step review, the final fix round (F1–F6, F5b/U15), `just check` (on `98dff0af`), the fork gate, the one image build (`629166ceef60-dev`, local, not pushed), and the live pass (every live file and e2e suite green on the plan's stack, after the live-pass fixes merged up to `75701b34`; `member_roles_live` 30/30, `compute_fork_live` 6/6, `docker_backend_live` 3/3). Landing highlights updated on `main` (`bb056d47`). Heads: Chelabase `neon-fork-round-3` (this branch), Neon `round-3` `629166ceef600938559ff39a1a95a7217e2cc0d2`, Postgres `round-3-v17` `76ce76278c5`, rust-postgres `chelabase/sasl-verified` `9a1b048b` (the workspace pin). The plan's stack `neon-fork-round-3` is still up. **Next, after the user's go: End of step 7 (ship):** fast-forward `chelabase/postgres` `REL_17_STABLE_chelabase` → `76ce76278c5` and `chelabase/neon` `main` → `629166cee` (direct push, same SHAs); push the build-tools image `ghcr.io/chelabase/neon-build-tools:328cc68f1c26` (local, U12); `publish-dev.sh --push-only` (prints the digests); pin both Compose digests (`infra/compose/docker-compose.yml`), the README Round 3 tag, commit `neon: bump to fork round 3`, push; `just neon-test`; merge into `main` (`--no-ff`), push; purge the stack (`just wt-remove neon-fork-round-3`), delete the branch and the fork task branches (`round-3-f1`…`-f3`, `round-3-v17-f2`, `round-3-t*`, Chelabase `neon-fork-round-3-f4`…`-f6`, `-lp`, `-t*`) on origin and locally; decide whether `chelabase/rust-postgres` `chelabase/sasl-verified` becomes the long-lived branch. Deferred to the next fork round (U16): the temporary-slot aux-file leak (roadmap). History of the final fix round and end of step follows. **History (2026-10-15):** F1 test agent (sonnet) in clone `<neon checkout>-f1` (branch `round-3-f1`, volumes `-f1`), F3 test agent (opus) in `<neon checkout>-f3` (`round-3-f3`, volumes `-f3`); briefs follow the "Final fix round" table. F1 RED `67ede8eeb` (shell 36 FAIL on the missing helpers; Rust: 4 structural startup-order tests + fast_import timeout fail on their first assertion, `crypto_provider.rs`/`s3_timeout.rs` fail to compile only on the items under test; runs `.ci-local/20261015-114032-v17-release`, `-114740-`); F1 coder (opus) running. **U12 (user, 2026-10-15):** after F1's coder, a sonnet step bumps `build-tools/Dockerfile` pins to the registry's latest (cargo-nextest 0.9.148, cargo-hakari 0.9.39, cargo-chef 0.1.78, diesel_cli 2.3.14, cargo-auditable 0.7.7, cargo-hack 0.6.45; deny, bindgen-cli, rustfilt already latest) and builds the build-tools image once, before F1's audit; F2/F3 then use it. Cost if wrong: one image build. **U13 (user, 2026-10-15):** fewer compiles (preamble): coders run the RED selection once at the end, audits don't rerun, fork pipelines build v17 only. F1 GREEN `d5bc0dfd2` (19/19 Rust `.ci-local/20261015-115742-v17-release`; pg-check core 225/225 + chelabase_keys 3/3 with neon_rmgr `-120459-`; coder deviation: hyper0 workspace dep declares `http1,http2,runtime,server,stream`, since the regenerated hakari dropped them); test-agent fix `57de8442d` (process failure: 2 RED Rust files not rustfmt-clean; plus 7 mypy errors in T2/T4/T6 Python tests; lint clean). F1 tool pins `7f743f3c8` (image `ghcr.io/chelabase/neon-build-tools:328cc68f1c26`, local id `1f58e2aeafd5`, **not pushed**: push at ship; hakari 0.9.39 regen); F1 audit (sonnet): approve, tidy `936f5327c`; Important: proxy/local_proxy/pg_sni_router never installed the provider (`redis` 0.29.2 `rediss://` panics); Minor: the 10 s overwrote a user connect timeout; Minor (kept, U-none): hakari now enables rustls `prefer-post-quantum` (X25519MLKEM768 first): watch the live pass for handshake failures. Invariant (second finding in the area): RED `5ab53cead` (every bin target installs the provider first; 11 named), fix `7bded0e2c` (coordinator verified the diff; coder deviation accepted: fast_import keeps the loader-level 10 s, since aws-config 1.12's timeout default reads no env/profile). **F1 done: `round-3-f1` at `7bded0e2c`.** F3 RED `680102c8a` + `7506a3238` (U14 rework: 18 Python FAIL / 10 controls pass, Rust compile RED on `branch_protected`); F3 GREEN `d33e34766` (Rust 62/62 `-130422-`; Python 117 passed/5 skipped `-125230-`, `ddl_forwarding_model` deselected: 40 shards, hours) + `1b46e3b62`. **R-F3b (coordinator, 2026-10-15):** compute_ctl's migrations run with `SET neon.forward_ddl = false` first (as `apply_config` does): `test_ddl_forwarding_invalid_db` failed because migration 1 was forwarded to an unreachable console and U3 made it fatal. Cost if wrong: none (migrations are compute_ctl's own changes). F3 audit (opus): approve, 0 Critical/Important, tidy `6a5461d46`; 2 Minor (reconfigure not all-or-nothing; a watcher panic stopped reloads) + hardening taken (digest key+cert+CA, cert dates, forwarding off as a startup option, aws-lc-rs not ring in compute_tools) + **R-F3c (coordinator):** a non-superuser can't rename a protected database or rename another to a protected name (`RENAME` then `DROP` bypassed the drop refusal). RED `df649a2a3`, fix `0a4d5f7cb`, test-agent fix `d379c28b7` (**process failure:** 2 RED tests wrong: a no-TLS rewrite expecting the TLS line; a `ring::` needle matching `Ordering::`), merge of `round-3-f1` `89f5d81df`, `f98e598ae` (aws-lc-rs 1.18.2 workspace dep; hakari clean; lint clean; Rust 86/86 `-135831-`). **F3 done: `round-3-f3` at `f98e598ae` (contains F1).** F2 RED Postgres `4e1c1267412`, Neon `67d0f0cf7` (chelabase_keys checks run with `--encoding=UTF8`: the build image has only C/POSIX locales; realtime tests connect without `options`); F2 GREEN Postgres `70c8d49ddcb`, Neon `314d9f510`, `ab3cc5c11` (pg-check core 225/225 + chelabase_keys 5/5 `-135912-`; Python 91 passed, 1 RED test bug: **process failure** `test_realtime_may_not_set_parameters_through_libpq`'s `rt_dsn()` dropped `options`); `chelabase_realtime` may send no `options` at all; 48 bytes per key in the budget. **U7b (user, 2026-10-15; narrows U7):** U7 applies only when the neon walproposer runs in the server (a core global set by the neon extension in the postmaster when it registers the walproposer, no GUC); plain Postgres keeps stock sync replication (so PostgreSQL's TAP tests 007/009/012/010 stay valid, and a future "neon off" self-hosted mode gets plain Postgres). Cost if wrong: a bug that leaves the global unset on a compute fails open (tested on a compute). F2 U7b: test fix `134fc857f` (libpq options helper; U7b tests), Postgres `55216d886d9` (`chelabase_walproposer_active` in `chelabase.h`, defined in `syncrep.c`, set by `walprop_register_bgworker()`), Neon `26909e948` (submodule → `55216d886d9`); pg-check 225/225 + 5/5, Python 85/85 `-160108-`. F2 audit (opus): approve, no defects; tidy Postgres `76ce76278c5` (comments); hardening tests taken, Neon `b4f4008f9` (startup-option cases; submodule → `76ce76278c5`; 12 passed `-161738-`). **F2 done: Postgres `round-3-v17-f2` at `76ce76278c5`, Neon `round-3-f2` at `b4f4008f9`.** Note for RT1's client docs (audit hardening 2): a libpq client with `PGTZ`/`PGDATESTYLE` set sends `timezone`/`datestyle` and is refused as `chelabase_realtime`; tokio-postgres sends neither. **F6 done: `neon-fork-round-3-f6` at `b450045d`:** RED `4bef8785` (per-service key table `scripts/tests/compose-key-visibility.sh`, in `just check`; R-T9a in `workbench-compose.sh`), GREEN `81b7907a` (per-file ro key mounts, `.chelabase` masked, CA reissue/0700/race, subnet preflight, docs), audit (sonnet) **Critical**: in a worktree `<checkout>/.chelabase` is a symlink to the keys dir, Docker follows it and orders mounts by unresolved depth, so the tmpfs mask tied with and hid every key mount (proved with an alpine probe); **process failure:** the table fixture had no symlink. Fix: RED `9dc353f7` (real-worktree case, depth check, new Docker probe `scripts/tests/compose-mask-probe.sh` in `just check`, read-only mask, 2 Minors), GREEN `680ba458` (read-only tmpfs on `${CHELABASE_KEYS_DIR}`, second mask via `CHELABASE_CHECKOUT_MASK` only when `<checkout>/.chelabase` resolves elsewhere; per-network inspect; `client.key` must match `client.crt`), test fix `b450045d` (**process failure:** depth check compared unrelated subtrees). Coordinator verified: the rendered config has one ro tmpfs at the keys dir; the probe passes. F5 RED rust-postgres `7a4bca59` (`chelabase/sasl-verified`), Chelabase `76029186`; GREEN rust-postgres `4cae4473`, `905de1bb` (pin `905de1bb500ec3dba4f3baece9524d41d2fd842f`, CLAUDE.md updated), Chelabase `894b9789` (orchestrator 123/424/109, query 47, neon 74). The coder found two more plaintext compute paths on `chelabase_default`: the auth service's app-users sync (`services/auth/src/app_users/mod.rs:71`, `chelabase_auth_sync`, app-user rows with emails; target from `internal/app.rs:252-296`) and rest's `authenticator` (`internal/data.rs:113`). **U15 (user, 2026-10-15):** both use TLS in this round, verifying the compute's server certificate against the platform CA (only `compute-ca/ca.crt` mounted ro into auth and rest, never `ca.key`; F6's key table extended), the server name passed with the target, no plaintext fallback; an F5 follow-up pipeline (sonnet test agent, opus coder) before F5's audit. Cost if wrong: a CA cert mount and TLS setup per connection pool. Also in it: rust-postgres `clippy -D warnings` fails on the base (`generic_client.rs` async_trait `double_must_use`): fix (our fork). Fork merges done: Postgres `round-3-v17` ff to `76ce76278c5`; Neon `round-3` = merges of `round-3-f3` (with F1) `89aa62ca9` and `round-3-f2`, `revisions.json` `629166cee`. Chelabase: F4, F6 merged into the step branch (`8a52d36f`, `25df75c0`; compile-checked). F5b: RED `ac916eb0` (cert carries only `<endpoint_id>.compute.chelabase.internal`: dial the IP via `hostaddr`, verify the name), GREEN `d0a31df5`, test fixes `3df7b7e1` (**process failure:** `err_expect` in a RED file; rust-postgres test clippy; pin `9a1b048b`), F5 audit (opus) pass, tidy `99819e13`; ruled: the coder's `server_name: None` in `app_users_live.rs` sanctioned; hardening taken: `ServerTls` TLS 1.3 only (RED `479c4530`, fix `bdb09719`); recorded, no change: a CA rotation needs auth/rest restarted; a no-alert handshake drop is Unreachable (m4). **F5 done**, merged `cb08a7e2`. **All six batches done.** End of step 3: `just check` first failed in `stack-urls.sh` (**process failures:** `"127.0.0.1"` literals in 4 new test fakes; T9's `compute_fork_live.rs` ran docker without `CHELABASE_COMPOSE_PROJECT`), fixed `5a78571c`; then a Studio flake under load (`structure.test.tsx` read the dry-run answer before its 400 ms debounce; `vi.waitFor`'s 1 s default), fixed `98dff0af` (21 runs green, 10 under load); **`just check` green on `98dff0af`** (Rust 2784, Studio 1388, all packages). End of step 4, fork gate (Neon `round-3` `629166cee`, Postgres `76ce76278c5`, suffix f3): default run green (lint, build, Rust 989/989, `.ci-local/20261015-172357-v17-release`); pg-check green (core 225/225, chelabase_keys 5/5, `-173417-`); regress selection (plan's list + this round's files, minus `ddl_forwarding_model`): 235 passed, 6 skipped, 1 error + 6 reruns, **all disk pressure** (pageserver's `disk_usage_eviction` WARN at >80% host disk, flagged at teardown; present in every failed attempt, in no passing one), `-173448-`; disk freed (batch worktrees F4–F6, clones `-f1`…`-f3`, F2 volumes removed: 61% used), `test_large_records` rerun green. **Fork gate green.** Lesson: fork regress runs need the host disk under 80% used. Pending fork changes before the image build: none (Neon `round-3` `629166cee`, Postgres `round-3-v17` `76ce76278c5`, both pushed). Images built once: `ghcr.io/chelabase/neon-storage:629166ceef60-dev` (`75794bed0c2f`), `neon-compute-v17:629166ceef60-dev` (`281c47327e04`), variant test ok, re-tagged `chelabase/neon-*:629166ceef60-dev`; **not pushed** (with the build-tools image `328cc68f1c26`: push at ship). `NEON_REF` → `629166ceef600938559ff39a1a95a7217e2cc0d2` (`f40ebdfd`). The plan's stack `neon-fork-round-3` is up (gateway 8100, proxy 4500; first `up` with F6's per-file key mounts in a worktree: healthy). Live pass (End of step 5's list plus `chelabase-query --test app_users_live` and `app-users-e2e`, which U15 touched), one suite at a time: **green:** roles_live, branch_live, neon_live, rest_live, orchestrator api, metadata live_db, app_users_live, every e2e (neon, data, sql-editor, role, branch, console, storage-leak, members, app-users); **failed (each stopped its file):** (1) `compute_fork_live` `cert_renewal_keeps_the_compute_running`: the test fakes time (`renew_certs_once(now + 70 d)`) and the sweep issued at that time, so the cert wasn't valid yet and F3's (correct) validity check refused it; ruling R-LP1: the sweep judges at the given time, issues at the wall clock (production unaffected: it passes the real clock); (2) `member_roles_live` hand-over test: stale since D3a (the member's own `ALTER ROLE CURRENT_USER SET` is refused per spec D3, C correct since T5; the T9 run predates T5's images); test-only fix, plus the same stale "reserved by Chelabase" expectations at ~`:1405-1414`; (3) `docker_backend_live` `admin_sql_on_a_running_compute`: needs a hand-exported `CHELABASE_LIVE_ENDPOINT` since 2026-10-04; ruling R-LP2: it discovers a running compute of its stack when unset (clear failure if none) and the live pass runs `just create-db-demo` first. No fork change, no image rebuild. Fix worktree `neon-fork-round-3-lp`: RED/test fixes `1b0290e8`, sweep fix `fe4662dc` (worker 689), merged `cbb88083`; rerun: docker_backend_live green (3/3, after `just create-db-demo`), compute_fork_live's cert test green; two more stale/wrong tests surfaced (they hadn't run): (4) `compute_fork_live` `realtime_slot_goes_with_a_compute_restart` counted `wal_proposer_slot` (unfiltered count; the realtime slot was gone, Postgres really restarted); (5) `member_roles_live` `no_guarded_session_escapes_the_guard` predates U1 (expected a template1 database guarded at once; U1 makes it born closed until the reconcile); test fixes `290242d8` (plus a scan of `member_roles_live`/`roles_live`/`branch_live` against U1/U14/D3a/R-F3c/F2: no other stale expectation), merged `8e06f525`; rerun of both files running. **Process failures:** stale live tests (3 in `member_roles_live`, 1 in `compute_fork_live`, the env-dependent `docker_backend_live`) were never traced against U1/D3a by the earlier test agents. **U16 (user, 2026-10-15):** the debug of (4) found a real fork bug: a temporary slot's pageserver aux-file copy outlives a compute restart (the restore deletes the slot without logging the drop), so basebackups keep shipping a stale `restart.lsn`; **deferred to the next fork round as its first item** (roadmap entry; must land before RT1 runs realtime on real computes). Cost if wrong: stale slot entries accumulate on computes that crash with `rt_` slots open, none before RT1. **F4 done: `neon-fork-round-3-f4` at `e9699fc1`:** GREEN `83e32ad0` (worker 678, orchestrator 116/112/422), test-agent fix `0bbb904e` (**process failure:** 2 clippy warnings in RED files), audit (opus) pass, tidy `bf0acef9`, 5 Minor + 2 hardening taken (sticky ready-cap stretch; unreadable flag pushes off; `starts_with` not `LIKE`; push bounded by `DOCKER_TIMEOUT`, one spec builder; `WorkerMetadata::endpoint_branch` port method, a deleting branch = unlinked; DDL retry only on protected branches; stale `current.tmp*` pruned) RED `ac12de7c`, fix `e9699fc1` (worker 688, chelabase-neon 90; coordinator verified the diff). Audit note, no change: a compute restart during unprotect may push the flag off before guards are removed; the guards keep the session protected until the reconcile removes them. F5 (worktree `neon-fork-round-3-f5`, rust-postgres branch `chelabase/sasl-verified`) test agent (opus) running. Earlier: F4 RED `efebd39e` (chelabase-neon 12, worker 29, orchestrator 1 failing for the intended reason; 15 controls); F4 coder (sonnet) running. **R-F4a (coordinator):** the orchestrator's spec pull (`internal/spec.rs` → `stored_endpoint_config`) reads the branch's protection (fail closed), so a restarted compute starts with the flag right. F5 after F2. Disk at 94%: the t6 clone and volumes removed (as planned; nothing unpushed); then the F1 volumes too (62 GB; pageserver disk-pressure warnings caused test reruns at 83%): run the End-of-step fork gate with `CHELA_NEON_VOLUME_SUFFIX=f3` (warm). F2 test agent (opus) running (clone `-f2`, Neon `round-3-f2` from `round-3-f1`, Postgres `round-3-v17-f2`). **U14 (user, 2026-10-15; supersedes the per-database E-I2 rule):** the branch's protection reaches the compute as a flag: `compute_api` `ComputeSpec.branch_protected: bool` (`#[serde(default)]`, false), compute_ctl writes `neon.branch_protected = on|off` into `postgresql.conf` at start and on reconfigure (then reloads); the extension's GUC `neon.branch_protected` (bool, `PGC_SIGHUP`, so no session can `SET` it); "protected" in `neon_ddl_handler.c` = the flag **or** the session database carries the guard (fails closed); every non-superuser `CREATE DATABASE` born closed and `ALTER DATABASE … ALLOW_CONNECTIONS/IS_TEMPLATE` refused while protected, whichever database the session is in. F4: `crates/chelabase-neon` spec sends `branch_protected`; the worker sets it from the branch and reconfigures the compute on protect (flag on **before** installing guards) and unprotect (guards removed, then flag off). Cost if wrong: a spec field and a reconfigure per protect/unprotect. Ruling R-F3a: F3 runs alongside F1 instead of after it (it doesn't use pg-check; 2 pipelines, 2 ci-local containers). Cost if wrong: none (disjoint files). Live-pass fix round 3: `reconcile_converges`'s model still let an owner `GRANT … TO cb_m_…` (**process failure**, missed by the scan): it now asserts D3's refusal (`42efff75`, merged `75701b34`); `member_roles_live` 30/30. Warm fork volume left: `chela-neon-{target,cargo}-f3` (remove after ship). Disk cleanup chore parked on the roadmap.
- **Done: T1b** (sonnet/sonnet): RED `51bdaeccb` (12 FAIL), implementation `1d6a4a6ab`; review: approve with fixes, 2 Important fixed at once (later fork tasks build on it): fix-round RED `8198f1bcf` (8 FAIL: the cache mounted at `/sccache-cache` outside `$HOME`, a damaged installed binary never trusted), fix `d55c66156` (verified by the coordinator: test file unchanged, all checks pass, binary sha pinned and checked each run, flock + same-filesystem rename). Note: debug builds are incremental, so sccache helps release builds only.
- **Done: T8** (opus/opus): RED `6b1cc0ae`, implementation `c904e2e4`; review: approve, 0 Critical, 1 Important + 6 Minor, all deferred to the final fix round (none blocks T9–T11): (I1) `compute_tls.rs:318-322` the tar writes `server.crt` before `server.key`, so a renewal can be seen half-done and compute_ctl's digest poll never retries: write the key first, and T6's watcher retries a failed reload; (m1) `compute_tls.rs:373-399` fsync the `v<secs>/` dir before the rename; (m2) `:361-397` concurrent writers (CLI and sweep) collide on `exists()`/`create` and the fixed `current.tmp`: loop on AlreadyExists, unique tmp name, prune old `v*/`; (m3) `backend.rs:502-523` a failed connect/upload/start leaves a stopped container: best-effort remove; (m4) `tests/docker_backend_live.rs:45-46` give it its own control network so `connect_network` is exercised; (m5) `compute_tls.rs:181-201` `from_pem` should check CA validity and keyCertSign; `:469` TLS 1.3 only. For later tasks: T9 needs `upload_tls`/`read_file` beyond the trait (an `impl Worker<…, DockerBackend, …>` block or new trait methods), its own `Arc<ComputeCa>`, and a timeout on the status client; T10 must wire `client_cns` (the placeholder sends none, which RT3-12 makes fail every start); T11 mounts the `orchestrator-tls` directory itself.
- **Done: T4** (opus/opus): RED Postgres `596f163f2eb` (core make check: `event_trigger` fails, "event triggers are not supported for REASSIGN OWNED"), Neon `78ae4d643`; implementation Postgres `1765e4a90e6` on `round-3-v17-t4`; GREEN: core make check 225/225 (release), the 3 hba tests (on a probe identical to the fixed file). Review: approve, 0 Critical/Important; 4 Minor: (m1–m3) deferred to the final fix round: add `clientcert=verify-ca` + `map` refused to `test_map_without_verify_full_stays_refused`; add the orchestrator cert with a wrong password ("password authentication failed") to the certificate/address test; assert `pg_event_trigger_ddl_commands()` is empty for REASSIGN OWNED in `event_trigger.sql` (the guard, `schema_guard.rs:258-261`, relies on it); (m4) the Python block vs compute_ctl's output: covered by T6's `the_block_allows_each_cidr_then_rejects`, which pins the identical text. Not proven on a cassert build (the reviewer reasons the `event_trigger.c:661-681` cross-check holds); the End-of-step fork gate's debug run covers it.
- **Done: T2** (opus/opus): RED Postgres `c4c3374d8e7` (by construction: no module source; the coordinator's run saw the in-progress file: "may not be used as an output plugin"), Neon `f4575bebf`; implementation Postgres `85755f31b1d` on `round-3-v17-t2` (plugin + R-T2a); GREEN: `make check` 3/3 with a `neon_rmgr` TEMP_CONFIG, Python 2/2 (`.ci-local/20261015-092232-v17-release`). Review: approve with fixes, 0 Critical, 2 Important + 4 Minor, all deferred to the final fix round (T3 depends on none): (I1) `chelabase_keys.c:604-605`, `:672-674` in a composite PK an unchanged out-of-line column makes the whole new key `KEY_TOAST`, so a change to another PK column is emitted as `u` with the old key and the new key is lost: build the new key per column, taking unchanged TOAST columns from the old key, then compare texts; regression test `(a text STORAGE EXTERNAL, b int)`, `UPDATE … SET b = b + 1` → `d` old + `i` new; (I2) see U4; (m1) `:180-184` strict option grammar (no leading space/`+`, refuse duplicates); (m2) `:113`, `:507`, `:521` memory ≈10× the output budget: count a per-key overhead in `size` (or state it in the spec); (m3) key-equal but text-different updates (`1.0`→`1.00`, citext) become `d`+`i` per RT3-9: recorded for RT1's consumer (a `d` whose `i` names the same row is not "row gone"); (m4) tests: control characters in a text key, `TRUNCATE a, b` and `… CASCADE`, a DEFERRABLE PK → `c`, and I1's case.
- **U4 (user, 2026-10-15):** the plugin converts every emitted string (keys, schema, table names) to UTF-8 with `pg_server_to_any(…, PG_UTF8)`; in a SQL_ASCII database `startup_cb` refuses with an error naming the encoding (realtime reports the database not live). Regression tests: a LATIN1 database's non-ASCII key arrives as UTF-8; SQL_ASCII refused. Fixed in the final fix round (T2 I2). Cost if wrong: a conversion per string.
- **Done: T3** (opus/opus): RED Neon `51ed019af` (13 failed, 2 controls passed, on T2's binaries), implementation Postgres `be98c07dfca` on `round-3-v17-t3` (base T2 `82a24073f13`); GREEN 15/15 regress, core make check 225/225, chelabase_keys 3/3 by hand. Review: restriction correct as specified, 0 Critical, 2 Important + 2 Minor + 1 note, all deferred to the final fix round (T7 depends on none): (I1) `walsender.c:2855-2978` hot-standby feedback ('h') on a logical slot sets its xmin (and can clear catalog_xmin): see U6; test: a raw 'h' reply leaves `pg_replication_slots.xmin` NULL; (I2) `syncrep.c:860-896` + `walsender.c:2758` a client with `application_name=walproposer` can become the sync standby and ack unstored WAL: see U7; test: such a client has `sync_priority` 0; (m1) rule 6 gap: `chelabase_realtime` may set user GUCs in its startup packet (`logical_decoding_work_mem`, `wal_sender_timeout=0`): allow only `client_encoding`, `application_name`, `replication` (check what RT1's client sends) and refuse the rest; (m2) tests: `CREATE_REPLICATION_SLOT rt_x TEMPORARY LOGICAL chelabase_keys` with no SNAPSHOT option (export, RT1's path) allowed; a closed non-`template0` template database stays closed to a superuser; (note) owner can terminate the realtime walsender: U8.
- **Done: T5** (opus/opus): RED Neon `c4300268c` (5 FAIL), implementation `7db5dda42` on `round-3-t5`, coordinator test fix `1f714554c` (owner needs CREATE on `public`, PG15+); GREEN: the Verify selection 120 + 1 passed (v17 release); compiles on v17 and v14. Review: approve, 0 Critical/Important, 4 Minor deferred to the final fix round (T7, T9 depend on none): (m1) `SessionDatabaseGuarded` can be faked by an owner trigger reassigned `OWNER TO cloud_admin` (fails closed): also require the trigger function's `proowner` to be a superuser; (m2) `REASSIGN OWNED BY x TO neon_superuser` moves databases to the privileged role, which `HandleCreateDb`/`HandleAlterOwner` refuse: refuse it in `HandleReassignOwned`; (m3) misleading refusals: `IS_TEMPLATE DEFAULT` refused (defGetBoolean of NULL is true), born-closed message for a nonexistent database; (m4) tests: a refusal forwards nothing (no PATCH), `CREATE ROLE … IN ROLE cb_m_*`, `ALTER GROUP … DROP USER`, `ALTER ROLE … IN DATABASE` by the member itself; and v15/v16 not compiled: the End-of-step fork gate compiles `pgxn/neon` for v14–v16 (`ci-local.sh --pg vNN --only build` builds only that major plus v17).
- **Done: T10** (sonnet/opus): RED `3b078376` (compile errors on `ComputeTlsFailed`, `ConnectError::Tls`), implementation `72d0f28b` on `neon-fork-round-3-t10`; GREEN: orchestrator 115 lib tests, chelabase-types, clippy, `sql_run_live` compiles. Ruling R-T10a: a missing client pair at startup isn't fatal (warning, retried every 60 s, compute connections 503 `compute_tls_failed` until loaded), since the worker writes the first pair. Review: approve with fixes, 0 Critical, 3 Important + 4 Minor, deferred to the final fix round (T11 depends on none): (I1) a CN mismatch reaches the client only as 28P01 (`auth.c:294-298`, `:2789-2800`), so it takes the stale-role path; my RED test's third message (`certificate validation … CN mismatch`) can never arrive (coordinator test bug: replace it with the U9 case); see U9; (I2) `run.rs:763-766` text matching misses post-handshake certificate alerts (TLS 1.3 `received fatal alert: UnknownCA` as `Kind::Io`) and "server did not use channel binding": classify by type for `RunTls::Compute` (`Kind::Tls`, a `rustls::Error` in the chain, the channel-binding refusal); (I3) `app_user_admin.rs:461-478` the app-users sync connection (`PgUsersDb`, `chelabase-query/src/app_users.rs:212`, `NoTls`) now reaches the control address in plaintext: give it TLS like the others; (m1) `console.rs:643-646` owner-pool TLS failures go through `QueryFailure::from_pg`, not `compute_tls_failed`; (m2) `routes.rs:219,227`, `run.rs:626` logs lack the endpoint id (RT3-7); (m3) `console.rs:610-631` take the connector and fingerprint from one snapshot; (m4) `run.rs:765` "certificate"/"tls" text also catches a reset during handshake (fixed by I2).
- **U9 (user, 2026-10-15):** CN-mismatch detection in our rust-postgres fork: it exposes that the server's SASL final message verified before the 28P01, so the orchestrator knows the password was right and the refusal came after it (the certificate check): 503 `compute_tls_failed`, no reconcile. A rust-postgres commit on `chelabase/transaction-status` (or a new branch from it), the workspace pin bumped (and CLAUDE.md's pin line). Final fix round. Cost if wrong: a small fork change and a pin bump.
- **Done: T11** (sonnet/sonnet): RED `5fe8571a`, implementation `e79dd9bc` on `neon-fork-round-3-t11`; GREEN: `cargo test -p chelabase` 203, clippy, `workbench-compose.sh`, `just neon-test`. Review: approve with fixes, 0 Critical, 2 Important + 4 Minor, deferred to the final fix round: (I1) see U10; (I2) `compute_ca.rs:~80` `ensure_client_pair` renews on expiry only: also reissue when the pair isn't signed by the current CA (a regenerated CA) or `client.key`/`ca.crt` is missing; (m1) no preflight for a `10.213.x.0/22` overlap with an existing Docker network: detect it (or catch Docker's "Pool overlaps") and name the slot/stack; (m2) `write_pair` race between two `ensure` calls: retry/re-check after `AlreadyExists`; (m3) create `compute-ca/` 0700; (m4) computes created before the CA have no server cert and aren't on `control`: the worker recreates (or reconfigures) them on start (note for T9's final-round fix).
- **Done: T9** (sonnet/opus): RED `5956ddd2`, implementation `7447c3ec` on `neon-fork-round-3-t9`; GREEN: worker 629 unit tests, clippy, both live files compile. Review: approve with findings, 4 Important + 6 Minor, deferred to the final fix round: (I1) see R-T9a; (I2) `member_roles.rs:1203-1206`, `:1738` the RT3-11 retry fires on the wrong condition (`saw_closed` counts owner-closed and marked-open databases; should be "no `born_closed && !allow_connections`") and for every enqueued run: a `from_ddl` flag on `MemberRolesReconcileRequest`, set by `services/orchestrator/src/internal/ddl.rs`; (I3) `certs.rs:84-109` the sweep judges renewal from the uploaded `server.crt`, not what Postgres serves: check the `pgdata` copy (with T8 I1's key-first tar); (I4) `member_roles.rs:496` open user templates (`datistemplate AND datallowconn`, not `template1`) are neither guarded nor listed: they join the guard list (RT3-5's invariant); (m1) on an unprotected branch, closed non-template databases join the unguard loop, and unguard runs before open; (m2) `:1400` `ResetMarker` on a protected branch requires the guard installed in this run; (m3) `:513-515` match the marker as a Postgres bool (`on`/`true`/…), as the C side does; (m4) `certs.rs:116-117` a corrupt `client.crt` is reissued, path from a constant; (m5) unit tests for the reconcile wiring (guard before open, failed install keeps closed, retry once) and the sweep; (m6) `config.rs:199` the missing-CA error names the command that creates it; `READY_TIMEOUT` (60 s) vs RT3-14's 2-min migration timeout: raise it to cover migrations.
- **Done: T12** (haiku/none): `d558f181` on `neon-fork-round-3-t12` (branches.mdx, table-api.mdx, security.mdx, infra/neon/README.md "Round 3"); covered by the End-of-step review.
- **Done: T0b** (opus/sonnet, review running): RED `363fd9fcc`; commits on Neon `round-3`: `41dde8943` (1.99.0 pins, LLVM 23), `ccec75978` (bindgen 0.73.2, rustls 0.23.46), `f056052a2`, `46ca4065a` (AWS SDK on hyper 1 + rustls 0.23; behavior version 2026-01-12; default checksums, U5), `611602bff` (machine clippy fixes), `d5080181e` (async-trait 0.1.92), `b124ea3fc` (hand fixes, narrow allows; found 3 real test bugs in the walproposer/safekeeper mocks), `b9caf56be` (deny.toml: our policy wording, 3 stale ignores removed, foldhash Zlib exception). GREEN: default ci-local run (lint, build, rust-tests 927/927) `.ci-local/20261015-101700-v17-release`. Image `ghcr.io/chelabase/neon-build-tools:cc9963bf9030` (`sha256:9014c2a36d0aa207e495e926f29149ac21ef8e50b29c56c82984795d5971eb10`), pushed. S3 note: no Rust test hits real S3 (`test_real_s3.rs` needs AWS); SeaweedFS is proven by the End-of-step live pass (U5) and the coordinator's CLI probe (all PUT/GET/multipart/streaming checksum cases pass on seaweedfs 4.47 with the stack's credentials).
- **T0b review** (sonnet): not ready, **1 Critical** + 5 Minor, all to the final fix round (T6 depends on none): (**C1**) rustls 0.23.46 now has both providers (ring via existing deps, aws-lc-rs via the AWS SDK's `rustls-aws-lc`); `rustls::ServerConfig::builder()` with no process default panics at `pageserver/src/bin/pageserver.rs:500`, `safekeeper/src/bin/safekeeper.rs:602`, `storage_controller/src/main.rs:528`, `storage_broker/src/bin/storage_broker.rs:723` when TLS is on. Ruling R-T0b-a (product rule: aws-lc-rs over ring): each binary installs `rustls::crypto::aws_lc_rs::default_provider()` as the process default at start (before any TLS use), and a unit test builds a `ServerConfig` with both providers compiled in; revisit the stale comment at `storage_broker/src/lib.rs:58`; the live pass starts pageserver/safekeeper with TLS where the stack enables it. (m2) regenerate `workspace_hack` (`cargo hakari generate`; it still lists rustls with ring only). (m3) ruling R-T0b-b: behavior version 2026-01-12's default 3.1 s connect timeout is set explicitly to 10 s in `remote_storage` and `fast_import` (S3-compatible stores on slow links), with the `s3_bucket.rs` comment updated. (m5) check the Dockerfile's tool pins (cargo-deny 0.20.2, nextest 0.9.94, hakari 0.9.36, chef 0.1.71, diesel_cli 2.2.9, auditable 0.7.0) and bindgen against the registry for newer releases. (m6) note: the safekeeper simulation mock's `update_donor` also sets `shmem.donor_lsn` (`walproposer_api.rs:258`); both mock fixes make previously no-op tests exercise donor updates (good; recorded).
- **Done: T6** (opus/opus): RED `6b7f5ac6a` (Rust: 29 compile errors on the items under test; Python: 4 FAIL), merge of round-3 `0857be082`, implementation `4bdbec667` on `round-3-t6`, coordinator test fix `168cde8b9` (`test_compute_migrations_e2e`'s parametrize lost in my RED edit, restored; migration 1's case runs all migrations since failing 0002 now fails the start); GREEN: compute_tools/compute_api 41/41, the 4 migration tests, e2e 7 passed + 5 skipped (SKIP migrations), clippy clean. Accepted extras: neon_local stop without `postmaster.pid`; pgbouncer/local_proxy TLS refresh after a reload; lakebase `apply_config` options appended, not replaced. Review: approve, 6 Minor + 1 note, deferred to the final fix round: (m1) `tls.rs:65-74` `reload_tls_files` also parses `ca_path` and checks the server cert chains to it (a half-written `ca.crt` otherwise leaves the old cert in service with no retry); (m2) `compute.rs:2005-2015` `pg_reload_conf` checks pg_ctl's exit status and returns an error instead of `expect`; (m3) `tls.rs:96-113` unique temp names or a mutex (watcher vs reconfigure); (m4) `migration.rs:50-71` drop "next: migration 1" from a pre-migration failure; (m5) extract the watcher's step function and test it; (m6) `spec.rs` `member_hba_block` below v17: write the reject lines only (fails closed) or a clear startup error; (note) the watcher runs without `tls_experimental` too: gate it on the feature.
- **Done: T7** (sonnet/none): Postgres `round-3-v17`: `4cea4339ead` (T2+T3), `7002e9c9a85` (T4) = submodule `7002e9c9a852b74d95c14190552a2f8bfdf05829` (17.11); Neon `round-3`: merges `8c5076744`, `fb002c367`, `763b5fd2f`, `60628902a`, `d715910f5`, then `30ec3022e` (submodule, `revisions.json`, `postgres.mk` installs chelabase_keys for v17 only). RED `017065ea7` (1 FAIL; coordinator test fix vs the plan: the recipe is printed with `MAKE=echo` and `-o` on its prerequisites, since plain `make -n` can't resolve it), GREEN: `ci-local_test.sh` all checks pass. No conflicts. Task clones `-t2`…`-t5` removed (nothing unpushed); `-t6` kept for its warm volume.
- **End-of-step review** (opus, done): no new Critical; new findings for the final fix round: (E-I1) see U11 (`provision.rs:295`); (E-I2) `neon_ddl_handler.c:1907` `SessionDatabaseGuarded` and `:2058` `CheckAlterDatabase` look only at the session's database: from an open user template (T9 I4, now Important) an owner's `CREATE DATABASE` isn't born closed and `ALTER DATABASE <owner-closed> ALLOW_CONNECTIONS true` passes: T9 I4's fix plus C-side tests for both cases; (E-m3) `compute_tools/src/spec.rs:252` `update_pg_hba` skips a file that has the header, and `reconfigure` never rewrites pg_hba/pg_ident: replace the text between the markers and re-apply on reconfigure with a reload; (E-m4) = T6 m6 (v14–v16 guard); (E-m5) docs: `security.mdx:14` describes Kubernetes/NetworkPolicy (not shipped): say an empty `CHELABASE_MEMBER_LOGIN_CIDRS` turns the address rule off and workbench stacks also admit the runner's address; `branches.mdx:76` add that a non-superuser's `CREATE`/`ALTER DATABASE … ALLOW_CONNECTIONS`/`IS_TEMPLATE` is refused on a protected branch (42501, `chelabase:branch_protected`); (E-m6) Neon `FORK.md` "Compute extension changes": round-3 entries (managed member roles and `chelabase_realtime`, born closed and `neon.born_closed`, REASSIGN owners in DDL forwarding, compute_ctl `member_logins`/`ca_path`/TLS reload/migrations before `running`). Implementation notes for deferred fixes: T9 m6: wait while compute_ctl reports a live non-failed state, capped at ~5 min (≥ the 2-min migration timeout); U7: key sync priority on `MyBackendType == B_BG_WORKER` (walproposer), client walsenders (`B_WAL_SENDER`) get 0, no name check; U6: guard only `ProcessStandbyHSFeedbackMessage` with `SlotIsLogical(MyReplicationSlot)`, leave `ProcessStandbyHSFeedback` (walproposer uses it for `wal_proposer_slot`, `walproposer_pg.c:2223`); T5 m2: refuse only for non-superusers (compute_ctl's `DeleteDBRoleReferences` runs `REASSIGN OWNED` as `cloud_admin`); R-T9a + U10 need T11's compose check changed with them.
- **U11 (user, 2026-10-15):** the worker's wait ends at once only on compute_ctl's fatal-migration error ("migration N failed" / migrations could not run, U3); any other `failed` keeps polling to the deadline (the container's `unless-stopped` restart recovers transient start errors) and the last error is reported. Supersedes spec §7's "Failed ends the wait". Cost if wrong: a non-recoverable start failure takes the full deadline to report.
- **U10 (user, 2026-10-15):** every Compose service that mounts a checkout gets `.chelabase` masked (an empty tmpfs or empty read-only dir over `${CHELABASE_KEYS_DIR}` inside the checkout mount), then only the narrow key mounts it needs (CA: worker, and the runner per R-T9a; `orchestrator-tls`: worker rw, orchestrator/runner ro; `secrets.key`, gateway pair, compute-JWT, cookie key: only the services that read them today). Closes today's exposure of every machine key on the `main` stack. Final fix round, with a compose check per service. Cost if wrong: a service missing a key it reads (caught by the live pass).
- **R-T9a (coordinator, 2026-10-15):** the runner (workbench stacks only) mounts `<keys>/compute-ca` read-only, because worker live tests build a real worker there (`tests/common/mod.rs:56-76` → `config::build` → `ComputeCa::load`); amends S14 and T11's compose check ("only the worker mounts the CA" → worker rw, runner ro; coordinator test fix in the final round). Cost if wrong: the test runner holds the CA key on dev stacks.
- **U6 (user, 2026-10-15):** hot-standby feedback is ignored on every logical slot, for every role. Cost if wrong: a logical client that sends feedback gains nothing (it never needed it).
- **U7 (user, 2026-10-15):** only the in-process walproposer can be a synchronous standby; client walsenders get sync priority 0 whatever `application_name` they send (by process, not name). Cost if wrong: customers can't attach their own synchronous standby to a compute.
- **U8 (user, 2026-10-15):** the owner may terminate the realtime walsender on their own compute (pg_signal_backend); accepted, realtime recovers. No change.
- **U5 (user, 2026-10-15):** T0b upgrades the AWS SDK crates (aws-config 1.12.0, aws-sdk-s3 1.152.0, …; `default-https-client`, hyper 1 + rustls 0.23 on aws-lc-rs) so rustls 0.21 / hyper-rustls 0.24 leave the tree (RUSTSEC-2026-0346, -0349); no `deny.toml` ignores. The S3 client keeps the SDK's **default flexible checksums** (CRC on requests, validation on responses). The End-of-step live pass must prove pageserver/safekeeper uploads and downloads against our SeaweedFS (`chrislusf/seaweedfs:4.47`) with them. Cost if wrong: S3-compatible stores without flexible-checksum support reject requests.
- **Deferred T1 finding (m4, found by T2):** pg-check of `contrib/chelabase_keys` needs `neon_rmgr` preloaded (heap DML logs under `RM_NEON_ID`): pg-check passes a `TEMP_CONFIG` with `shared_preload_libraries = 'neon_rmgr'` and `dynamic_library_path = '$libdir:/work/pg_install/v17/lib/postgresql'` for that directory, and runs after the build step. Must land on `round-3` before the fork gate (End of step 4). T2 proves GREEN meanwhile with the same `make check TEMP_CONFIG=…` by hand.
- **R-T2a (coordinator, 2026-10-15):** `output_plugin_libraries` (PostgreSQL 17.11's allow-list, default `pgoutput, test_decoding`) gains `chelabase_keys` in its default (`guc_tables.c`, `postgresql.conf.sample`, `config.sgml`), since we ship the plugin. Cost if wrong: compute_ctl would set it instead.
- **Coordinator test fixes (listed per the rules):** T4: `setup()` renamed `setup_hba()` (pytest's module-setup hook) and the hba rows found by line number, since `pg_hba_file_rules` nulls the fields of an error line (`559943cfe`); T2: `CREATE OR REPLACE FUNCTION keys` (the three files share one database, `82a24073f13`) and the restart test counts logical slots only (every primary has `wal_proposer_slot`, `60912d6e4`). T8: three `endpoint_config` calls in `spec.rs` tests lacked the new trailing argument (the implementer added `&logins()`); T2: the TOAST test's key 3000 → 2500 bytes (PG17's `SET STORAGE EXTERNAL` reaches the PK index, `tablecmds.c:8936`, which then can't compress, `indextuple.c:121-123`); T2 and T4 Python tests ruff-formatted and T4's `Path` import moved under `TYPE_CHECKING` (`765bab5e6`, `27386eb47`). T2's own RED run is contaminated (its uncommitted `chelabase_keys.c` was on disk in the bind-mounted checkout); T2's RED is by construction at `c4c3374d8e7` (no module source).
- **T2 RED** Postgres `c4c3374d8e7`, Neon `f4575bebf` (+`765bab5e6`). **T4 RED** Postgres `596f163f2eb`, Neon `78ae4d643` (+`27386eb47`). **T5 RED** Neon `c4300268c`. **T6 RED** Neon `6b7f5ac6a`. Python and Rust RED for T5/T6 are proved by the implementer's first run (P14c).
- **Done: T1** (sonnet/sonnet): RED `ecd742a75` (5 FAIL; the plan said 6, but "pg-check is not in the default run" already passes, it only guards), implementation `9a71be63e` on Neon fork `round-3`; review: approve, 0 Critical/Important, 3 Minor deferred to the final fix round: (m1) `ci-local.sh:391-394` `find "$dir" -name regression.diffs` on `build/v17` also prints the contrib dir's diffs under the core heading; (m2) same lines: stale `regression.diffs` from an earlier run in the persistent volume can be printed as current (fix both: delete `regression.diffs` in each dir before its `make check`, and limit the find's depth); (m3) `ci-local.sh:384` `make check` without `-j`.
- **T8 RED** `6b1cc0ae` on `neon-fork-round-3-t8`: lib tests fail to compile on `MemberLogins`/`member_logins` (12 errors); `compute_tls` and `reconfigure_mock` fail only on the missing items under test (no errors in the rustls, x509-parser or tar calls).
- **U1 (user, 2026-10-15):** every non-superuser `CREATE DATABASE` on a protected branch is born closed, whatever its template. Cost if wrong: SQL-created databases from `template1` wait one reconcile before taking connections.
- **U2 (user, 2026-10-15):** the reconcile connects to any closed non-template database on a protected branch as superuser and installs the guard there, but opens only databases carrying `neon.born_closed`; an owner's close is respected. Cost if wrong: an owner-closed database stays closed until unprotect.
- **U3 (user, 2026-10-15, amended):** only fail-open compute_ctl migrations are fatal: 0002 and future privilege/attribute removals, tagged in compute_ctl; a failure that leaves a fail-open migration unapplied (0001 before 0002) is fatal too; grant migrations fail closed and are logged. Cost if wrong: computes start with fewer owner rights after a grant migration's bug.
- **S12 (user, 2026-10-15, stays in this round):** `REASSIGN OWNED` reports the database owners it moves to DDL forwarding. Cost if wrong: a few lines in the handler.
- Branch `neon-fork-round-3`, spec `674ebf8d`, plan `<this commit>`. Fork branches: `chelabase/neon` `round-3` (to be cut from `main` `c8d1bb68b`), `chelabase/postgres` `round-3-v17` (to be cut from `REL_17_STABLE_chelabase` `1915cc9bba56`).

## Plan rulings (mechanical, listed for the user)

- **P1** Postgres work happens in each Neon task clone's submodule checkout `vendor/postgres-v17`, initialised with `git submodule update --init --reference <postgres checkout> vendor/postgres-v17` (a local-object clone of the Postgres fork) and switched to a task branch `round-3-v17-tN` (origin `https://github.com/chelabase/postgres.git`), because `ci-local.sh` builds exactly that directory. T7 merges the task branches into `round-3-v17`. Cost if wrong: a separate Postgres clone plus a fetch into the submodule per test run.
- **P2** The `pg_regress` expected files are the contract for data lines. If an implementer's run differs from an `.out` only in psql framing (echo, blank lines, column padding), it reports a deviation and the coordinator fixes the `.out` (a coordinator test bug, listed here). Cost if wrong: one extra coordinator commit.
- **P3** The worker uploads the compute's TLS files root-owned, mode 0644 (the key too), into `/var/db/chelabase/tls/`: no uid pinning (the image creates `postgres` without a fixed uid, `compute/compute-node.Dockerfile:1987`). Every process in a compute runs as `postgres` or root, and compute_ctl copies the key into `pgdata` with mode 0600 for Postgres (`compute_tools/src/tls.rs:63-87`). Cost if wrong: a Dockerfile line pinning the uid and a 0600 upload.
- **P4** RT3-1 (a certificate change reloads Postgres and never touches the status) is tested by a compute_ctl unit test of the reload function plus the Chelabase live test `cert_renewal_keeps_the_compute_running`. neon_local can't run a TLS compute (`control_plane/src/endpoint.rs:214-216` hard-codes `tls: None`). Cost if wrong: a neon_local TLS option for a Python test.
- **P5** The member `pg_hba.conf` block is tested twice: its exact text (compute_ctl test, T6) and its behaviour (Python `VanillaPostgres` with the same literal block, T4). Cost if wrong: none.
- **P6** The plan's one stack comes up at End of step 4, after T11 (the Compose file changes) and the image build, not at plan time: no task runs a live suite, because every new behaviour needs the new images. Cost if wrong: none.
- **P7** The worker's certificate sweep is tested live only (`cert_renewal_keeps_the_compute_running`); its renewal window is the pure `compute_tls::needs_renewal`. Cost if wrong: a fake-backed unit test later.
- **P8** No new orchestrator live test: the TLS member path is exercised by `member_roles_live` (the runner holds the orchestrator's identity), orchestrator `api` live and `sql-editor-e2e` in End of step 5. Cost if wrong: a failure surfaces in the e2e rather than in a named test.
- **P9** The fork gate (`ci-local.sh` default run, `--only pg-check`, the targeted `--regress`) runs once, at End of step 4; tasks run only the `ci-local.sh` steps their Verify lists. Cost if wrong: a cross-task fork failure found at the end, fixed in the final round.
- **P10** The closed-database reconcile is a pure planner (`plan_closed_databases`) plus SQL builders, unit-tested; its wiring into the reconcile is covered by review and the live tests. Cost if wrong: a wiring bug found live.
- **P11** The new `ErrorCode::ComputeTlsFailed` (`compute_tls_failed`) regenerates `openapi/*.json` and both orval clients with `just api-gen` in T10. Cost if wrong: none.
- **P13** "Unset `CHELABASE_MEMBER_LOGIN_CIDRS` is a startup error" (spec §4.2) is enforced where the services are built (`worker::config::build`, the orchestrator's `main`), not in `from_lookup`, whose existing tests rely on every other setting having a default (`services/worker/src/config.rs:304-310`, `services/orchestrator/src/internal/config.rs:211`). Cost if wrong: none to behaviour.
- **P14 (user, 2026-10-15: "Seed + sccache")** Fork build speed: (a) T1b adds a shared sccache volume to `ci-local.sh`; (b) the coordinator seeds every later fork task's volumes from a warm task's (`docker run --rm -v chela-neon-target-<warm>:/a -v chela-neon-target-<new>:/b <build-tools image> cp -a /a/. /b`, same for `chela-neon-cargo-*`) before its first run, so the stamp wipes only what differs; (c) a coordinator RED proof for C tests uses `--only pg-check` (Postgres only), and Python RED tests are proved by the implementer's first run, briefed at once, never waiting on a coordinator-only full build. Cost if wrong: ~20–30 GB disk per seeded volume.
- **P15 (user, 2026-10-15: "Bump now")** The Neon fork moves to Rust 1.99.0 in this round (T0b). T2, T4 and T5 (C and Python) stay on their 1.88 bases until T7 merges them into `round-3`; T6 (compute_ctl, Rust) starts after T0b; the final fork gate and the images use 1.99.0. Cost if wrong: one build-tools image build and the lint fallout.
- **P12** The plugin's text messages are pinned here (option errors, the restriction's refusals) so tests can assert them. Cost if wrong: a string in two places.

## Task table (binding)

| Task | Scope | Implementer | Reviewer | Wave | Depends |
|---|---|---|---|---|---|
| T1 | Neon fork: `ci-local.sh` `pg-check` step | sonnet | sonnet | 0 | — |
| T0b | Neon fork: Rust toolchain 1.88.0 → 1.99.0 (`rust-toolchain.toml`, `build-tools/Dockerfile`, new build-tools image, new lints fixed) (user, 2026-10-15) | opus | sonnet | 1 | T1 |
| T1b | Neon fork: `ci-local.sh` shared `chela-neon-sccache` volume + static sccache wrapper (user, 2026-10-15) | sonnet | sonnet | 1 | T1 |
| T2 | Postgres fork: `contrib/chelabase_keys` (D1) | opus | opus | 1 | T1 |
| T4 | Postgres fork: `REASSIGN OWNED` event triggers (D4); `hba.c` `map` with `clientcert=verify-full` (D3b) | opus | opus | 1 | T1 |
| T8 | Chelabase `crates/chelabase-neon`: `compute_tls`, spec fields, backend create (network, upload), compute_ctl status, deps | opus | opus | 1 | — |
| T3 | Postgres fork: `chelabase_realtime` restriction, `rt_` slot names, superuser connect to closed databases (D2, RT3-15, D5) | opus | opus | 2 | T2 (reviewed) |
| T5 | Neon fork: `neon_ddl_handler.c` member-role statements (D3a), born closed (D5, U1, U2, RT3-4, RT3-5), REASSIGN database owners (S12) | opus | opus | 2 | T1 |
| T6 | Neon fork: compute_ctl member hba/ident, TLS (`ca_path`, PKCS#8, reload), migrations before `running` with kinds and timeouts (D3b, D6, U3, RT3-1, RT3-12, RT3-14) | opus | opus | 2 | T1 |
| T7 | Neon fork integration: Postgres branches into `round-3-v17`, submodule + `vendor/revisions.json`, `postgres.mk` v17 line (RT3-6), merges into `round-3` | sonnet | none (End of step 1 review, and the fork gate in step 4) | 3 | T1–T6 |
| T9 | Worker: config, `wait_until_ready`, closed/born-closed databases, certificate sweep; the live RED tests | sonnet | opus | 3 | T8 |
| T10 | Orchestrator: TLS connections, client-config reload, control locator, `compute_tls_failed`, `just api-gen` | sonnet | opus | 3 | T8 |
| T11 | CLI/workbench and Compose: CA and client pair, env, registry slot, `control` network, mounts | sonnet | sonnet | 4 | T8 |
| T12 | Docs: `branches.mdx`, `table-api.mdx`, `security.mdx`, `infra/neon/README.md` Round 3 draft | haiku | none (leaf; End of step 1 review) | 4 | — |

Waves run in parallel when their files are disjoint (pre-flight scan), each task in its own worktree (`just wt-add neon-fork-round-3-tN neon-fork-round-3`) or fork clone, with at most 3 builds or test suites at once machine-wide (each `ci-local.sh` run and each `cargo test` counts as one): W1 = T2, T4 (fork) + T8 (cargo); W2 = T3, T5, T6 (fork); W3 = T7 (shell tests only) + T9, T10 (cargo); W4 = T11 (cargo) + T12 (no build). One stack serves the whole plan, from End of step 4 (P6); no task runs a live suite.

## Shared interfaces (pinned)

**Names and constants.**
- Roles: `chelabase_realtime` (exact); member roles: prefix `cb_m_`, matched case-insensitively by the fork (`pg_strncasecmp(name, "cb_m_", 5) == 0`), platform form `cb_m_` + 32 lowercase hex.
- Plugin: `chelabase_keys`; realtime slot prefix `rt_`.
- `src/include/replication/chelabase.h`: `#define CHELABASE_REALTIME_ROLE "chelabase_realtime"`, `#define CHELABASE_KEYS_PLUGIN "chelabase_keys"`, `#define CHELABASE_REALTIME_SLOT_PREFIX "rt_"`.
- Marker GUC (neon extension): `neon.born_closed`, `PGC_SUSET` bool, default off, no runtime effect; set per database as `pg_db_role_setting` row `(setdatabase = <db oid>, setrole = 0)` with element `neon.born_closed=on`.
- Guard detection (C): event trigger named `chelabase_schema_guard_ddl` in the current database, `superuser_arg(evtowner)`, `evtenabled = 'A'`.
- TLS: `TLS_DIR = /var/db/chelabase/tls`; files `server.key`, `server.crt`, `ca.crt`; upload root `/var/db` (tar entries `chelabase/`, `chelabase/tls/`, `chelabase/tls/{server.crt,server.key,ca.crt}`, files 0644, dirs 0755, uid/gid 0). Server name `<endpoint_id>.compute.chelabase.internal`. Orchestrator CN `chelabase-orchestrator`. Validity: server 90 days (renew under 30), client 365 days (renew under 60), CA 3650 days. Keys ECDSA P-256 PKCS#8 PEM; signatures ecdsa-with-SHA256.
- Key directories (under the machine-key dir `<keys>` = `.chelabase`): `<keys>/compute-ca/{ca.crt,ca.key}` (ca.key 0600, never overwritten); `<keys>/orchestrator-tls/current` → symlink to `v<unix-secs>/` holding `client.crt`, `client.key` (0600), `ca.crt`; the swap is: write `v<secs>/`, create `current.tmp` symlink, `rename(current.tmp, current)`.
- Env: `CHELABASE_COMPUTE_CONTROL_NETWORK` (worker, orchestrator, runner), `CHELABASE_MEMBER_LOGIN_CIDRS` (worker, orchestrator; comma list; unset = startup error; empty = no address rule), `CHELABASE_COMPUTE_CA_DIR` (worker), `CHELABASE_COMPUTE_TLS_DIR` (worker writes, orchestrator and runner read), `CHELABASE_CONTROL_SUBNET`, `CHELABASE_CONTROL_COMPUTE_RANGE`, `CHELABASE_ORCHESTRATOR_CONTROL_IP`, `CHELABASE_RUNNER_CONTROL_IP` (Compose).
- Control subnet for registry slot `k` (0..=63; `main` = 0): subnet `10.213.{4k}.0/22`, compute range `10.213.{4k+2}.0/23`, orchestrator `10.213.{4k}.10`, runner `10.213.{4k}.11`, CIDRs `10.213.{4k}.10/32,10.213.{4k}.11/32`.

**Plugin output and options (T2).** One compact JSON message per committed transaction with row changes, written in `commit_cb`: `{"v":1,"lsn":"<X/Y of txn->end_lsn>","xid":<full xid as integer>,"changes":[…]}` or `{"v":1,"lsn":…,"xid":…,"all":true}`; a change is `{"rel":<oid>,"s":"<schema>","t":"<table>","op":"i"|"u"|"d","k":[[<text>,…],…]}` or `{"rel":…,"s":…,"t":…,"op":"c"}`. Relations in first-change order; ops `i`, `u`, `d`, `c` in that order; keys in first-seen order, deduplicated per (relation, op). Options `max_keys_per_table` (1–10000, default 1000), `max_tables` (1–1000, default 256), `max_message_bytes` (65536–16777216, default 1048576). Errors (P12, `22023`): `chelabase_keys: unknown option "<name>"`; `chelabase_keys: option "<name>" must be an integer between <lo> and <hi>`.

**Restriction messages (T3, `42501`).** Login: `FATAL: role "chelabase_realtime" may only open logical replication connections`. SQL in the walsender: `role "chelabase_realtime" can't run SQL in a replication connection`. A refused command: `permission denied for replication command "<TAG>"` (TAG as Postgres names it: `CREATE_REPLICATION_SLOT`, `START_REPLICATION`, `DROP_REPLICATION_SLOT`, `ALTER_REPLICATION_SLOT`, `READ_REPLICATION_SLOT`, `BASE_BACKUP`, `TIMELINE_HISTORY`, `SHOW`, `UPLOAD_MANIFEST`). `rt_` names: `replication slot names starting with "rt_" are reserved for chelabase_realtime`.

**Neon-extension messages (T5, `42501`).** `member role "<name>" is managed by Chelabase`; born closed: `on a protected branch, ALLOW_CONNECTIONS and IS_TEMPLATE are set by Chelabase` with hint `chelabase:branch_protected`.

**compute_api (T6).**
```rust
// libs/compute_api/src/spec.rs
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct MemberLogins { pub allowed_cidrs: Vec<String>, pub client_cns: Vec<String> }
// ComputeSpec gains:  #[serde(default)] pub member_logins: Option<MemberLogins>,
// libs/compute_api/src/responses.rs  TlsConfig gains:  #[serde(default)] pub ca_path: Option<String>,
```
compute_ctl (pub, for the integration tests): `compute_tools::spec::{member_hba_block, member_ident_lines, update_pg_hba, migration_kinds}`, `compute_tools::migration::{MigrationKind, fatal_failure, MIGRATION_CONN_OPTIONS}`, `compute_tools::config::tls_conf_lines`, `compute_tools::tls::{verify_key_cert, reload_tls_files}`:
```rust
pub fn member_hba_block(member_logins: Option<&MemberLogins>, tls: bool) -> anyhow::Result<String>;
pub fn member_ident_lines(member_logins: Option<&MemberLogins>) -> anyhow::Result<String>;
pub fn update_pg_hba(pgdata_path: &Path, databricks_pg_hba: Option<&String>, member_logins: Option<&MemberLogins>, tls: bool) -> Result<()>;
pub fn migration_kinds() -> Vec<MigrationKind>;            // one per entry of handle_migrations' list
pub enum MigrationKind { FailOpen, FailClosed, Skip }     // derive Debug, Clone, Copy, PartialEq, Eq
pub fn fatal_failure(kinds: &[MigrationKind], failed: usize) -> bool; // failed = 0-based index
pub const MIGRATION_CONN_OPTIONS: &str = "-c lock_timeout=5s -c statement_timeout=2min";
pub fn tls_conf_lines(tls: &TlsConfig) -> String;
pub fn verify_key_cert(key: &str, cert: &str) -> anyhow::Result<()>;   // was private
pub fn reload_tls_files(pgdata: &Path, tls: &TlsConfig, reload: impl FnOnce() -> anyhow::Result<()>) -> anyhow::Result<()>;
```
The block (tabs between fields; one `hostssl` line per CIDR, `all` when the list is empty; only the header, the four `reject` lines and the footer when `member_logins` is absent or `tls` is false):
```
# chelabase: member logins (cb_m_*) only from the orchestrator
hostssl	all	/^cb_m_[0-9a-f]{32}$	<cidr>	scram-sha-256	clientcert=verify-full	map=chelabase_member
host	all	/^[Cc][Bb]_[Mm]_	all	reject
host	replication	/^[Cc][Bb]_[Mm]_	all	reject
local	all	/^[Cc][Bb]_[Mm]_	reject
local	replication	/^[Cc][Bb]_[Mm]_	reject
# chelabase: end of member logins
```
`pg_ident.conf`, one line per CN: `chelabase_member\t<cn>\t/^cb_m_[0-9a-f]{32}$`. A CIDR must parse as `<ipv4|ipv6>/<prefix>`; a CN must match `^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$`; `client_cns` must not be empty.

**chelabase-neon (T8).**
```rust
// crates/chelabase-neon/src/compute_tls.rs
pub const TLS_DIR: &str = "/var/db/chelabase/tls";
pub const TLS_UPLOAD_ROOT: &str = "/var/db";
pub const SERVER_KEY_PATH: &str = "/var/db/chelabase/tls/server.key";
pub const SERVER_CERT_PATH: &str = "/var/db/chelabase/tls/server.crt";
pub const CA_CERT_PATH: &str = "/var/db/chelabase/tls/ca.crt";
pub const SERVER_NAME_SUFFIX: &str = "compute.chelabase.internal";
pub const ORCHESTRATOR_CN: &str = "chelabase-orchestrator";
pub const SERVER_VALIDITY: Duration;  // 90 days
pub const CLIENT_VALIDITY: Duration;  // 365 days
pub const CA_VALIDITY: Duration;      // 3650 days
pub const SERVER_RENEW_BEFORE: Duration; // 30 days
pub const CLIENT_RENEW_BEFORE: Duration; // 60 days
pub fn server_name(endpoint_id: &str) -> String;
pub struct IssuedCert { pub cert_pem: String, pub key_pem: String, pub not_after: SystemTime }
pub struct ComputeCa { /* cert_pem, key_pem */ }
impl ComputeCa {
    pub fn generate(now: SystemTime) -> anyhow::Result<Self>;
    pub fn from_pem(cert_pem: &str, key_pem: &str) -> anyhow::Result<Self>;
    pub fn load(dir: &Path) -> anyhow::Result<Self>;            // <dir>/ca.crt, <dir>/ca.key
    pub fn cert_pem(&self) -> &str;
    pub fn key_pem(&self) -> &str;
    pub fn issue_server(&self, endpoint_id: &str, now: SystemTime) -> anyhow::Result<IssuedCert>;
    pub fn issue_client(&self, cn: &str, now: SystemTime) -> anyhow::Result<IssuedCert>;
}
pub fn not_after(cert_pem: &str) -> anyhow::Result<SystemTime>;
pub fn needs_renewal(not_after: SystemTime, now: SystemTime, before: Duration) -> bool;
pub fn tls_files_tar(server: &IssuedCert, ca_pem: &str) -> anyhow::Result<Vec<u8>>;
pub fn write_client_pair(dir: &Path, client: &IssuedCert, ca_pem: &str, now: SystemTime) -> anyhow::Result<PathBuf>; // returns the new v<secs> dir
pub struct ClientTls { /* ArcSwap of the rustls ClientConfig, the loaded pair's fingerprint */ }
impl ClientTls {
    pub fn load(dir: &Path) -> anyhow::Result<Self>;            // <dir>/current/*
    pub fn fingerprint(&self) -> [u8; 32];                       // SHA-256 of client.crt bytes
    pub fn reload_if_changed(&self) -> anyhow::Result<bool>;
    pub fn connector(&self) -> tokio_postgres_rustls_improved::MakeRustlsConnect;
}
// crates/chelabase-neon/src/spec.rs
pub struct MemberLogins { pub allowed_cidrs: Vec<String>, pub client_cns: Vec<String> }
pub fn parse_member_login_cidrs(raw: Option<&str>) -> anyhow::Result<Vec<String>>;
// SpecInputs gains `pub member_logins: MemberLogins`; endpoint_config and stored_endpoint_config gain
// a trailing `member_logins: &MemberLogins` parameter. config_response always adds
// spec.features = ["tls_experimental"], spec.member_logins, and
// compute_ctl_config.tls = {key_path: SERVER_KEY_PATH, cert_path: SERVER_CERT_PATH, ca_path: CA_CERT_PATH}.
// crates/chelabase-neon/src/reconfigure.rs
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ComputeCtlStatus { Running, Failed(String), Other(String) }
pub async fn status(http: &reqwest::Client, control_addr: SocketAddr, signer: &ComputeSigner, endpoint_id: &str) -> anyhow::Result<ComputeCtlStatus>;
// crates/chelabase-neon/src/backend.rs
// ComputeSettings gains `pub control_network: String` and `pub ca: Option<Arc<ComputeCa>>` (None in the
// orchestrator, which never creates computes); DockerBackend::create = create → connect control network →
// upload tls_files_tar(issue_server(..)) → start. New: `async fn upload_tls(&self, endpoint_id, server: &IssuedCert)`
// and `async fn read_file(&self, endpoint_id, path) -> anyhow::Result<Vec<u8>>` on DockerBackend (sweep).
```

**Worker (T9).** `SpecPusher` gains `fn status(&self, control_addr: SocketAddr, endpoint_id: &str) -> impl Future<Output = anyhow::Result<ComputeCtlStatus>> + Send;` `FakePusher` gains `pub fn set_status(&self, endpoint_id: &str, status: ComputeCtlStatus)` (default `Running`). `WorkerConfig` gains `compute_control_network: String` (default `chelabase_control`), `compute_ca_dir: PathBuf` (default `.chelabase/compute-ca`), `compute_tls_dir: PathBuf` (default `.chelabase/orchestrator-tls`), `member_login_cidrs: Option<Vec<String>>` (`None` when unset; read with `get` directly, not `non_blank`, because an empty value means "no address rule"; a malformed value fails `from_lookup`); `config::build` refuses to build a worker when it is `None` (the startup error), so the many existing `from_lookup` tests keep their defaults (P13). The orchestrator's `InternalConfig` gains the same `member_login_cidrs`, `compute_control_network` and `compute_tls_dir`, with the same rule in its `main`. `Worker::renew_certs_once(&self, now: SystemTime) -> (usize, usize)` (renewed, failed), run daily. In `member_roles.rs`:
```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClosedDatabase { pub name: String, pub allow_connections: bool, pub template: bool, pub born_closed: bool }
// MemberCatalog gains `pub closed_databases: Vec<ClosedDatabase>` (sorted by name), from catalog lines
// `c <hex name> <datallowconn t|f> <datistemplate t|f> <marked t|f>` for every database that is closed or marked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClosedStep { Guard(String), Open(String), ResetMarker(String), Skip(String) }
pub fn plan_closed_databases(closed: &[ClosedDatabase], protected: bool) -> Vec<ClosedStep>;
pub fn open_born_closed_sql(database: &str) -> String;   // ALTER … ALLOW_CONNECTIONS true; ALTER … RESET neon.born_closed;
pub fn reset_born_closed_sql(database: &str) -> String;  // ALTER … RESET neon.born_closed;
```
Planner rules: protected → closed non-template: `Guard`, then `Open` if marked; marked and open: `ResetMarker`; closed template: `Skip` (only listed when marked or closed). Unprotected → marked closed non-template: `Open`; marked and open: `ResetMarker`; marked template: `Skip`; unmarked closed: nothing. Order of the output: by database name, each database's steps in the order above.

**Orchestrator (T10).** `ErrorCode::ComputeTlsFailed` (`"compute_tls_failed"`, 503, `retry_after`). `ConnectError` gains `Tls(String)` (a failure before the server answered: handshake, certificate check, no client pair loaded). `member_login_refusal`: `Tls(_)`, and `Rejected { code: "28000", message }` whose message contains `pg_hba.conf rejects connection` or `certificate` → `Refused(compute_tls_failed())`; other `28000`/`28P01` → `Retry` as today. `RunTarget` gains `pub tls: RunTls` with `pub enum RunTls { Plain, Compute { server_name: String, client: Arc<ClientTls> } }`; compute connections set `host` = server name, `hostaddr` = control address, `ssl_mode(Require)`, `channel_binding(Require)`. Owner-pool key gains the client fingerprint.

## Pre-flight scan (2026-10-15)

**Files per wave (disjointness).**
- W1: T2 `contrib/chelabase_keys/**`, `contrib/Makefile`, `contrib/meson.build` (Postgres, clone `-t2`); T4 `src/backend/tcop/utility.c`, `src/include/tcop/cmdtaglist.h`, `src/backend/libpq/hba.c`, `src/test/regress/{sql,expected}/event_trigger.*`, `doc/src/sgml/event-trigger.sgml` (Postgres, clone `-t4`) and `test_runner/regress/test_member_logins_hba.py` (Neon, clone `-t4`); T8 `crates/chelabase-neon/**`, workspace `Cargo.toml`/`Cargo.lock`. Disjoint.
- W2: T3 `src/backend/replication/walsender.c`, `src/backend/utils/init/postinit.c`, `src/backend/replication/slot.c`, `src/include/replication/chelabase.h` (Postgres, branch from T2's), Neon `test_runner/regress/test_chelabase_realtime_role.py`, `test_closed_database_connect.py`; T5 `pgxn/neon/neon_ddl_handler.c`, `test_runner/regress/test_member_role_guard.py`, `test_born_closed_database.py`, `test_ddl_forwarding.py`; T6 `compute_tools/**`, `libs/compute_api/**`, `control_plane/src/endpoint.rs` (one literal), `test_runner/regress/test_compute_migrations.py`. Disjoint.
- W3: T7 `vendor/postgres-v17` pointer, `vendor/revisions.json`, `postgres.mk`, `.github/scripts/ci-local_test.sh`; T9 `services/worker/**`; T10 `services/orchestrator/**`, `crates/chelabase-types/src/api_error.rs`, `openapi/*.json`, both generated clients, `apps/docs/content/docs/reference/errors.mdx`. Disjoint (T9 and T10 both read but don't change `crates/chelabase-neon`).
- W4: T11 `tools/cli/**`, `infra/compose/docker-compose.yml`, `scripts/tests/workbench-compose.sh`, `scripts/tests/fixtures/workbench-stack.env`, `.env.example` if it lists keys; T12 `apps/docs/content/docs/{using/branches.mdx,architecture/table-api.mdx,architecture/security.mdx}`, `infra/neon/README.md`. Disjoint.

**Match sites a new field or variant breaks.**
- `SpecInputs` literals: `crates/chelabase-neon/src/spec.rs` tests (`:253`, `:391`, `:435`, `:453`, `:475`, `:497`, `:577`); `endpoint_config`/`stored_endpoint_config` callers `services/worker/src/reset.rs:124`, `services/orchestrator/src/internal/spec.rs:29`; `primary_spec_unchanged`'s key list (`spec.rs:589-606`) gains `features` and `member_logins`.
- `ComputeSettings` literals: `crates/chelabase-neon/src/backend.rs:780` (test `settings()`), `services/worker/src/config.rs:260`.
- `ComputeSpec` literal: `control_plane/src/endpoint.rs` (Neon); `TlsConfig` literal: `control_plane/src/endpoint.rs:216` (it is `None`, unchanged).
- `ErrorCode`: `crates/chelabase-types/src/api_error.rs:109` (enum), `:224` (list), `:324` (`as_str`), `:438` (`ALL`, 96 → 97 entries); generated `errorCode.ts` in `packages/management-client` and `apps/studio/src/lib/login-client`; `apps/docs/content/docs/reference/errors.mdx`.
- `ConnectError` matches: `services/orchestrator/src/sql_editor/routes.rs:215-229` (`member_login_refusal`), `:523-` (`connect_failure`), `run.rs:182-191` (`Display`).
- `RunTarget` literals: `routes.rs:491`, `:511`; `services/orchestrator/tests/sql_run_live.rs:653` (metadata database: `RunTls::Plain`).
- `SpecPusher` impls: `services/worker/src/ports.rs:1668` (`HttpPusher`), `services/worker/src/fakes.rs:3547` (`FakePusher`).
- `MemberCatalog` literal/`parse_member_catalog` (`member_roles.rs:520-707`): new `c` line kind; fakes answering the catalog needle must keep parsing (no `c` lines = no closed databases).
- `MachineKeys` / `ensure_machine_keys` (`tools/cli/src/dev/bootstrap.rs:207-231`) and its callers; registry `Entry` (`tools/cli/src/workbench/registry.rs:24-33`, serde default for old registries); `StackEnvInput` literals (`tools/cli/src/workbench/tests.rs:412-427` and lifecycle tests).

**Helpers to reuse.** `chelabase_neon::reconfigure::push`'s auth (`reconfigure.rs:22-41`); `DockerLocator` for the control network (`compute.rs:139-201`); `compute_jwt::ensure`'s never-overwrite publishing (`tools/cli/src/neon/compute_jwt.rs:16-80`) for the CA; postgres_fdw's `set_transmission_modes` (`contrib/postgres_fdw/postgres_fdw.c:3910-3950`); neon's `switch_to_superuser`/`switch_to_original_role` (`neon_ddl_handler.c:1471-1496`); `get_event_trigger_oid` (`event_trigger.c:579`); `FullTransactionIdFromAllowableAt` (`transam.h:381`); test_decoding as the plugin skeleton (`contrib/test_decoding/test_decoding.c`, its `Makefile` and `logical.conf`); Neon test fixtures `VanillaPostgres` (`test_runner/fixtures/neon_fixtures.py:3491-3580`), `run_only_on_postgres` (`fixtures/utils.py:706`), the DDL mock pattern (`test_runner/regress/test_ddl_protected_database.py:30-60`).

**Traps.**
- `ci-local.sh` refuses `git worktree` checkouts; every fork task uses a clone. The build stamp keys on the submodule commit, so committing in `vendor/postgres-v17` wipes and rebuilds `build/v17` (a few minutes); uncommitted edits rebuild incrementally.
- `datallowconn = false` refuses superusers too until T3 lands (`postinit.c:352-356`); T5's tests don't connect to born-closed databases.
- `pg_regress` runs psql from stdin with `-a -q`; the contrib tests start with `\set ECHO none`, `\set VERBOSITY terse`, `\pset tuples_only on`, `\pset format unaligned` so the expected files hold data lines only (P2).
- `REASSIGN OWNED` in a hand-over runs as `cloud_admin`: the guard returns at the superuser check (`services/worker/src/schema_guard.rs:220`).
- compute_ctl's migration failpoint `compute-migration=return(<id>)` fails migration `<id>` at its id update, SKIP ones included (`compute_tools/src/migration.rs:40-61`).
- A failed neon_local start (`control_plane/src/endpoint.rs:956-962`) leaves the fixture's `_running` unset, so `endpoint.stop()` is a no-op; the tests stop through `env.neon_cli.endpoint_stop(…, check_return_code=False, mode="immediate")`.
- `admin_sql` is `cloud_admin` with forwarding off; `as_owner` is `SET ROLE postgres` in that session, which is a non-superuser for the fork's checks unless the stack runs the owner-superuser opt-in (it doesn't).
- The runner already mounts the Docker socket and runs `docker restart` (`services/worker/tests/branch_live.rs:749-757`).
- libpq refuses a client key readable by group/others: `client.key` is written 0600 for the host uid the runner runs as.

---

## T1: `ci-local.sh` `pg-check` step (sonnet / sonnet)

**Checkout:** `<neon checkout>` after the coordinator cuts `round-3` from `main` (`git -C … switch -c round-3 origin/main`, push `-u`). T1 commits there directly (as round 2's T0 did).

**Files:** `.github/scripts/ci-local.sh`, `.github/scripts/ci-local_test.sh` (coordinator), `FORK.md` (the "Checks" paragraph).

**Behaviour:** a step `pg-check`, selectable only through `--only` (never in the default run), placed in pipeline order after `rust-tests` and before `regress`. Inside the container (user `nonroot`, `ci-local.sh:230`) it builds Postgres for `$PG` if needed (`make postgres-$PG`) and runs `make -C <dir> check` for each directory `pg_check_dirs $PG` prints: `build/<pg>` (core regression) and, for v17 only, `build/v17/contrib/chelabase_keys`. A failing directory fails the step; the run's log keeps each directory's `regression.diffs`.

**RED tests (coordinator):** appended to `.github/scripts/ci-local_test.sh` before its final `exit` (it already defines `expect` and `steps`, `:15-23`, `:142-147`):
```bash
# pg-check (round 3): only through --only, after rust-tests, before regress.
expect "--only pg-check runs only pg-check" "pg-check regress=0" "$(steps --only pg-check)"
expect "--only pg-check,build runs in pipeline order" "build,pg-check regress=0" "$(steps --only pg-check,build)"
expect "--only regress,pg-check,rust-tests runs in pipeline order" "rust-tests,pg-check,regress regress=1" "$(steps --only regress,pg-check,rust-tests)"
expect "pg-check is not in the default run" "lint,build,rust-tests regress=0" "$(steps)"
expect "pg-check directories for v17" "build/v17 build/v17/contrib/chelabase_keys" "$(pg_check_dirs v17 | paste -sd' ' -)"
expect "pg-check directories for v16: core only" "build/v16" "$(pg_check_dirs v16 | paste -sd' ' -)"
if steps --only pg-check --test-filter 'test(x)' >/dev/null 2>&1; then
    echo "FAIL: --test-filter without rust-tests must still be refused with pg-check"
    failures=$((failures + 1))
else
    echo "ok: --test-filter needs rust-tests even with pg-check"
fi
```
RED: 6 `FAIL` lines (`--only pg-check` is refused today, `ci-local.sh:109-113`; `pg_check_dirs` is undefined). Assumptions checked: `steps` echoes `selected_steps` joined by commas plus `regress=` (`ci-local_test.sh:142-147`); `--only` validation is the `case` at `ci-local.sh:110-113`.

**Gotchas:** the `--only` refusal message lists the steps; update it. `selected_steps`' loop (`ci-local.sh:155`) gains `pg-check` between `rust-tests` and `regress`.

**Verify:** `bash .github/scripts/ci-local_test.sh`; `shellcheck .github/scripts/ci-local.sh .github/scripts/ci-local_test.sh`. No container run.

**Live runs:** none.

---

## T2: `contrib/chelabase_keys` (opus / opus)

**Setup (coordinator):** `git clone <neon checkout> <neon checkout>-t2`; `git -C …-t2 remote set-url origin https://github.com/chelabase/neon.git`; `git -C …-t2 fetch origin`; `git -C …-t2 switch -c round-3-t2 origin/round-3`; `git -C …-t2 submodule update --init --reference <postgres checkout> vendor/postgres-v17`; `git -C …-t2/vendor/postgres-v17 fetch origin`; `git -C …-t2/vendor/postgres-v17 switch -c round-3-v17-t2 origin/round-3-v17` (the coordinator cut `round-3-v17` from `REL_17_STABLE_chelabase` and pushed it). Runs use `CHELA_NEON_VOLUME_SUFFIX=t2`. Postgres commits go to `round-3-v17-t2` (pushed to `chelabase/postgres`); Python tests go to the Neon branch `round-3-t2`.

**Files:** `vendor/postgres-v17/contrib/chelabase_keys/{chelabase_keys.c,Makefile,meson.build,logical.conf}`, `contrib/Makefile` (SUBDIRS), `contrib/meson.build`; tests (coordinator): `contrib/chelabase_keys/sql/{keys,kinds,limits}.sql`, `expected/{keys,kinds,limits}.out`, Neon `test_runner/regress/test_chelabase_keys.py`. The coordinator's RED commit also adds the `Makefile` and `logical.conf` (test plumbing, so `make check` runs):
```make
# contrib/chelabase_keys/Makefile
MODULES = chelabase_keys
PGFILEDESC = "chelabase_keys - primary keys of changed rows, for Chelabase realtime"
REGRESS = keys kinds limits
REGRESS_OPTS = --temp-config $(top_srcdir)/contrib/chelabase_keys/logical.conf
NO_INSTALLCHECK = 1
ifdef USE_PGXS
PG_CONFIG = pg_config
PGXS := $(shell $(PG_CONFIG) --pgxs)
include $(PGXS)
else
subdir = contrib/chelabase_keys
top_builddir = ../..
include $(top_builddir)/src/Makefile.global
include $(top_srcdir)/contrib/contrib-global.mk
endif
```
```
# contrib/chelabase_keys/logical.conf
wal_level = logical
max_replication_slots = 4
max_wal_senders = 4
max_prepared_transactions = 2
logical_decoding_work_mem = 64kB
```

**Behaviour:** spec §2 with RT3-2, RT3-3, RT3-8, RT3-9; the message format and options as pinned.

**RED tests (coordinator).** Every `.sql` starts with this prologue (in the `.out` only `\set ECHO none` is echoed):
```sql
\set ECHO none
\set VERBOSITY terse
\pset tuples_only on
\pset format unaligned
SET client_min_messages = warning;
CREATE FUNCTION keys(slot text, VARIADIC opts text[] DEFAULT '{}') RETURNS SETOF text LANGUAGE sql AS $$
  SELECT regexp_replace(regexp_replace(data,
           '"lsn":"[0-9A-F]+/[0-9A-F]+","xid":[0-9]+', '"lsn":"L","xid":X'),
         '"rel":[0-9]+', '"rel":R', 'g')
  FROM pg_logical_slot_get_changes(slot, NULL, NULL, VARIADIC opts) $$;
```

`sql/keys.sql` (after the prologue):
```sql
SELECT 'created' FROM pg_create_logical_replication_slot('k', 'chelabase_keys');
-- basic: one message per transaction; ops grouped; a key once per (relation, op)
CREATE TABLE orders (id int PRIMARY KEY, note text);
BEGIN;
INSERT INTO orders VALUES (1, 'a'), (2, 'b');
UPDATE orders SET note = 'c' WHERE id = 1;
UPDATE orders SET note = 'd' WHERE id = 1;
DELETE FROM orders WHERE id = 2;
COMMIT;
INSERT INTO orders VALUES (3, 'e');
SELECT * FROM keys('k');
-- a PK update is d + i; setting the PK to its own value is u
CREATE TABLE pku (id int PRIMARY KEY, v int);
INSERT INTO pku VALUES (1, 0);
UPDATE pku SET id = 2 WHERE id = 1;
UPDATE pku SET id = 2, v = 1 WHERE id = 2;
SELECT * FROM keys('k');
-- an unchanged out-of-line PK in an update of another column: u with the full key
CREATE TABLE tk (id text PRIMARY KEY, v int);
ALTER TABLE tk ALTER COLUMN id SET STORAGE EXTERNAL;
INSERT INTO tk VALUES (repeat('x', 3000), 0);
UPDATE tk SET v = 1;
SELECT replace(k, repeat('x', 3000), 'x*3000') FROM keys('k') k;
-- TRUNCATE is c
CREATE TABLE tr (id int PRIMARY KEY);
INSERT INTO tr VALUES (1);
TRUNCATE tr;
SELECT * FROM keys('k');
-- replica identity: keys only for the PK
CREATE TABLE nopk (v int);
CREATE TABLE rfull (id int PRIMARY KEY);
ALTER TABLE rfull REPLICA IDENTITY FULL;
CREATE TABLE rnothing (id int PRIMARY KEY);
ALTER TABLE rnothing REPLICA IDENTITY NOTHING;
CREATE TABLE ridx (id int PRIMARY KEY, u int NOT NULL UNIQUE);
ALTER TABLE ridx REPLICA IDENTITY USING INDEX ridx_pkey;
CREATE TABLE ridx2 (id int PRIMARY KEY, u int NOT NULL UNIQUE);
ALTER TABLE ridx2 REPLICA IDENTITY USING INDEX ridx2_u_key;
INSERT INTO nopk VALUES (1);
INSERT INTO rfull VALUES (1);
INSERT INTO rnothing VALUES (1);
INSERT INTO ridx VALUES (1, 1);
INSERT INTO ridx2 VALUES (1, 1);
SELECT * FROM keys('k');
-- a partition's change carries the partition
CREATE TABLE pt (id int PRIMARY KEY) PARTITION BY RANGE (id);
CREATE TABLE pt1 PARTITION OF pt FOR VALUES FROM (0) TO (100);
INSERT INTO pt VALUES (5);
SELECT (data::json->'changes'->0->>'rel')::oid = 'pt1'::regclass, data::json->'changes'->0->>'t'
  FROM pg_logical_slot_get_changes('k', NULL, NULL);
-- nothing for DDL alone or a rollback; DDL beside rows shows only the rows
CREATE TABLE e1 (id int PRIMARY KEY);
BEGIN;
INSERT INTO e1 VALUES (1);
ROLLBACK;
BEGIN;
ALTER TABLE e1 ADD COLUMN v int;
INSERT INTO e1 VALUES (2, 0);
COMMIT;
SELECT * FROM keys('k');
-- a prepared transaction is decoded at COMMIT PREPARED
BEGIN;
INSERT INTO e1 VALUES (3, 0);
PREPARE TRANSACTION 'p1';
SELECT * FROM keys('k');
COMMIT PREPARED 'p1';
SELECT * FROM keys('k');
-- xid is the full transaction id; lsn is the message's position
CREATE TABLE xl (id int PRIMARY KEY);
SELECT 'drained' FROM (SELECT count(*) FROM pg_logical_slot_get_changes('k', NULL, NULL)) d;
BEGIN;
INSERT INTO xl VALUES (1);
SELECT pg_current_xact_id() AS x \gset
COMMIT;
SELECT (data::json->>'xid')::xid8 = :'x'::xid8, (data::json->>'lsn')::pg_lsn = lsn
  FROM pg_logical_slot_get_changes('k', NULL, NULL);
SELECT 'dropped' FROM (SELECT pg_drop_replication_slot('k')) d;
```
`expected/keys.out`:
```
\set ECHO none
created
{"v":1,"lsn":"L","xid":X,"changes":[{"rel":R,"s":"public","t":"orders","op":"i","k":[["1"],["2"]]},{"rel":R,"s":"public","t":"orders","op":"u","k":[["1"]]},{"rel":R,"s":"public","t":"orders","op":"d","k":[["2"]]}]}
{"v":1,"lsn":"L","xid":X,"changes":[{"rel":R,"s":"public","t":"orders","op":"i","k":[["3"]]}]}
{"v":1,"lsn":"L","xid":X,"changes":[{"rel":R,"s":"public","t":"pku","op":"i","k":[["1"]]}]}
{"v":1,"lsn":"L","xid":X,"changes":[{"rel":R,"s":"public","t":"pku","op":"i","k":[["2"]]},{"rel":R,"s":"public","t":"pku","op":"d","k":[["1"]]}]}
{"v":1,"lsn":"L","xid":X,"changes":[{"rel":R,"s":"public","t":"pku","op":"u","k":[["2"]]}]}
{"v":1,"lsn":"L","xid":X,"changes":[{"rel":R,"s":"public","t":"tk","op":"i","k":[["x*3000"]]}]}
{"v":1,"lsn":"L","xid":X,"changes":[{"rel":R,"s":"public","t":"tk","op":"u","k":[["x*3000"]]}]}
{"v":1,"lsn":"L","xid":X,"changes":[{"rel":R,"s":"public","t":"tr","op":"i","k":[["1"]]}]}
{"v":1,"lsn":"L","xid":X,"changes":[{"rel":R,"s":"public","t":"tr","op":"c"}]}
{"v":1,"lsn":"L","xid":X,"changes":[{"rel":R,"s":"public","t":"nopk","op":"c"}]}
{"v":1,"lsn":"L","xid":X,"changes":[{"rel":R,"s":"public","t":"rfull","op":"c"}]}
{"v":1,"lsn":"L","xid":X,"changes":[{"rel":R,"s":"public","t":"rnothing","op":"c"}]}
{"v":1,"lsn":"L","xid":X,"changes":[{"rel":R,"s":"public","t":"ridx","op":"i","k":[["1"]]}]}
{"v":1,"lsn":"L","xid":X,"changes":[{"rel":R,"s":"public","t":"ridx2","op":"c"}]}
t|pt1
{"v":1,"lsn":"L","xid":X,"changes":[{"rel":R,"s":"public","t":"e1","op":"i","k":[["2"]]}]}
{"v":1,"lsn":"L","xid":X,"changes":[{"rel":R,"s":"public","t":"e1","op":"i","k":[["3"]]}]}
drained
t|t
dropped
```

`sql/kinds.sql` (after the prologue; `pg_regress` sets `PGTZ=America/Los_Angeles` and `PGDATESTYLE=Postgres, MDY`, so the session differs from R6's settings):
```sql
SELECT 'created' FROM pg_create_logical_replication_slot('kk', 'chelabase_keys');
SET IntervalStyle = 'postgres_verbose';
SET extra_float_digits = 0;
SET bytea_output = 'escape';
SET search_path = public, pg_catalog;
CREATE TYPE mood AS ENUM ('sad', 'happy');
CREATE TABLE target (id int);
CREATE TABLE k_big (id bigint PRIMARY KEY);
CREATE TABLE k_num (id numeric PRIMARY KEY);
CREATE TABLE k_txt (id text PRIMARY KEY);
CREATE TABLE k_uuid (id uuid PRIMARY KEY);
CREATE TABLE k_ts (id timestamptz PRIMARY KEY);
CREATE TABLE k_iv (id interval PRIMARY KEY);
CREATE TABLE k_fl (id float8 PRIMARY KEY);
CREATE TABLE k_by (id bytea PRIMARY KEY);
CREATE TABLE k_en (id mood PRIMARY KEY);
CREATE TABLE k_mo (id money PRIMARY KEY);
CREATE TABLE k_rc (id regclass PRIMARY KEY);
CREATE TABLE k_comp (b text, a int, note text, PRIMARY KEY (a, b));
INSERT INTO k_big VALUES (9007199254740993);
INSERT INTO k_num VALUES (1.50);
INSERT INTO k_txt VALUES (E'q"uo\\te é');
INSERT INTO k_uuid VALUES ('A0EEBC99-9C0B-4EF8-BB6D-6BB9BD380A11');
INSERT INTO k_ts VALUES ('2026-01-02 03:04:05+00');
INSERT INTO k_iv VALUES ('1 day 2 hours');
INSERT INTO k_fl VALUES (0.1);
INSERT INTO k_by VALUES ('\x0102');
INSERT INTO k_en VALUES ('happy');
INSERT INTO k_mo VALUES (12.34);
INSERT INTO k_rc VALUES ('target'::regclass);
INSERT INTO k_comp VALUES ('x', 1, 'n');
SELECT * FROM keys('kk');
SELECT 'dropped' FROM (SELECT pg_drop_replication_slot('kk')) d;
```
`expected/kinds.out`:
```
\set ECHO none
created
{"v":1,"lsn":"L","xid":X,"changes":[{"rel":R,"s":"public","t":"k_big","op":"i","k":[["9007199254740993"]]}]}
{"v":1,"lsn":"L","xid":X,"changes":[{"rel":R,"s":"public","t":"k_num","op":"i","k":[["1.50"]]}]}
{"v":1,"lsn":"L","xid":X,"changes":[{"rel":R,"s":"public","t":"k_txt","op":"i","k":[["q\"uo\\te é"]]}]}
{"v":1,"lsn":"L","xid":X,"changes":[{"rel":R,"s":"public","t":"k_uuid","op":"i","k":[["a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11"]]}]}
{"v":1,"lsn":"L","xid":X,"changes":[{"rel":R,"s":"public","t":"k_ts","op":"i","k":[["2026-01-02 03:04:05+00"]]}]}
{"v":1,"lsn":"L","xid":X,"changes":[{"rel":R,"s":"public","t":"k_iv","op":"i","k":[["P1DT2H"]]}]}
{"v":1,"lsn":"L","xid":X,"changes":[{"rel":R,"s":"public","t":"k_fl","op":"i","k":[["0.1"]]}]}
{"v":1,"lsn":"L","xid":X,"changes":[{"rel":R,"s":"public","t":"k_by","op":"i","k":[["\\x0102"]]}]}
{"v":1,"lsn":"L","xid":X,"changes":[{"rel":R,"s":"public","t":"k_en","op":"i","k":[["happy"]]}]}
{"v":1,"lsn":"L","xid":X,"changes":[{"rel":R,"s":"public","t":"k_mo","op":"i","k":[["$12.34"]]}]}
{"v":1,"lsn":"L","xid":X,"changes":[{"rel":R,"s":"public","t":"k_rc","op":"i","k":[["public.target"]]}]}
{"v":1,"lsn":"L","xid":X,"changes":[{"rel":R,"s":"public","t":"k_comp","op":"i","k":[["1","x"]]}]}
dropped
```

`sql/limits.sql` (after the prologue):
```sql
SELECT 'created' FROM pg_create_logical_replication_slot('l', 'chelabase_keys');
SELECT * FROM keys('l', 'nope', '1');
SELECT * FROM keys('l', 'max_tables', 'x');
SELECT * FROM keys('l', 'max_tables', '0');
SELECT * FROM keys('l', 'max_tables', '1001');
SELECT * FROM keys('l', 'max_keys_per_table', '0');
SELECT * FROM keys('l', 'max_keys_per_table', '10001');
SELECT * FROM keys('l', 'max_message_bytes', '65535');
SELECT * FROM keys('l', 'max_message_bytes', '16777217');
CREATE TABLE lk (id int PRIMARY KEY);
INSERT INTO lk SELECT generate_series(1, 2);
SELECT * FROM keys('l', 'max_keys_per_table', '2');
INSERT INTO lk SELECT generate_series(3, 5);
SELECT * FROM keys('l', 'max_keys_per_table', '2');
CREATE TABLE la (id int PRIMARY KEY);
CREATE TABLE lb (id int PRIMARY KEY);
CREATE TABLE lc (id int PRIMARY KEY);
BEGIN;
INSERT INTO la VALUES (1);
INSERT INTO lb VALUES (1);
INSERT INTO lc VALUES (1);
COMMIT;
SELECT * FROM keys('l', 'max_tables', '2');
BEGIN;
INSERT INTO la VALUES (2);
INSERT INTO lb VALUES (2);
COMMIT;
SELECT * FROM keys('l', 'max_tables', '2');
CREATE TABLE lt (id text PRIMARY KEY);
INSERT INTO lt SELECT repeat(chr(97 + i % 26), 1000) || i FROM generate_series(1, 100) i;
SELECT * FROM keys('l', 'max_message_bytes', '65536');
SELECT 'dropped' FROM (SELECT pg_drop_replication_slot('l')) d;
```
`expected/limits.out`:
```
\set ECHO none
created
ERROR:  chelabase_keys: unknown option "nope"
ERROR:  chelabase_keys: option "max_tables" must be an integer between 1 and 1000
ERROR:  chelabase_keys: option "max_tables" must be an integer between 1 and 1000
ERROR:  chelabase_keys: option "max_tables" must be an integer between 1 and 1000
ERROR:  chelabase_keys: option "max_keys_per_table" must be an integer between 1 and 10000
ERROR:  chelabase_keys: option "max_keys_per_table" must be an integer between 1 and 10000
ERROR:  chelabase_keys: option "max_message_bytes" must be an integer between 65536 and 16777216
ERROR:  chelabase_keys: option "max_message_bytes" must be an integer between 65536 and 16777216
{"v":1,"lsn":"L","xid":X,"changes":[{"rel":R,"s":"public","t":"lk","op":"i","k":[["1"],["2"]]}]}
{"v":1,"lsn":"L","xid":X,"changes":[{"rel":R,"s":"public","t":"lk","op":"c"}]}
{"v":1,"lsn":"L","xid":X,"all":true}
{"v":1,"lsn":"L","xid":X,"changes":[{"rel":R,"s":"public","t":"la","op":"i","k":[["2"]]},{"rel":R,"s":"public","t":"lb","op":"i","k":[["2"]]}]}
{"v":1,"lsn":"L","xid":X,"all":true}
dropped
```

`test_runner/regress/test_chelabase_keys.py` (Neon):
```python
from __future__ import annotations

import json
import select
import time
from typing import TYPE_CHECKING

import psycopg2
from fixtures.pg_version import PgVersion
from fixtures.utils import run_only_on_postgres
from psycopg2.extras import LogicalReplicationConnection

if TYPE_CHECKING:
    from fixtures.neon_fixtures import NeonEnv, VanillaPostgres


def read_one(conn, cur, timeout: float = 20.0) -> dict:
    deadline = time.time() + timeout
    while time.time() < deadline:
        msg = cur.read_message()
        if msg is not None:
            return json.loads(msg.payload)
        select.select([conn], [], [], 0.5)
    raise AssertionError("no message from chelabase_keys")


@run_only_on_postgres([PgVersion.V17], "chelabase_keys is in the v17 fork only")
def test_keys_over_replication_and_restart(neon_simple_env: NeonEnv):
    env = neon_simple_env
    endpoint = env.endpoints.create_start("main")
    endpoint.safe_psql("CREATE ROLE chelabase_realtime LOGIN REPLICATION NOBYPASSRLS NOINHERIT")
    endpoint.safe_psql("CREATE TABLE t (id int PRIMARY KEY, v int)")
    conn = psycopg2.connect(
        endpoint.connstr(user="chelabase_realtime"), connection_factory=LogicalReplicationConnection
    )
    cur = conn.cursor()
    cur.execute("CREATE_REPLICATION_SLOT rt_t TEMPORARY LOGICAL chelabase_keys (SNAPSHOT 'nothing')")
    cur.fetchall()
    cur.start_replication_expert("START_REPLICATION SLOT rt_t LOGICAL 0/0", decode=True)
    endpoint.safe_psql("INSERT INTO t VALUES (1, 0), (2, 0)")
    msg = read_one(conn, cur)
    assert msg["v"] == 1
    assert msg["changes"] == [
        {"rel": msg["changes"][0]["rel"], "s": "public", "t": "t", "op": "i", "k": [["1"], ["2"]]}
    ]
    assert endpoint.safe_psql("SELECT count(*) FROM pg_replication_slots WHERE slot_name = 'rt_t'")[0][0] == 1
    endpoint.stop()
    endpoint.start()
    assert endpoint.safe_psql("SELECT count(*) FROM pg_replication_slots")[0][0] == 0
    try:
        cur.read_message()
        conn.poll()
        broken = False
    except psycopg2.Error:
        broken = True
    assert broken or conn.closed, "the replication connection must end with the compute"


@run_only_on_postgres([PgVersion.V17], "chelabase_keys is in the v17 fork only")
def test_xid_is_the_full_transaction_id(vanilla_pg: VanillaPostgres):
    vanilla_pg.configure(["wal_level = logical", "max_replication_slots = 4"])
    vanilla_pg.pg_bin.run_capture(["pg_resetwal", "-e", "1", "-D", str(vanilla_pg.pgdatadir)])
    vanilla_pg.start()
    vanilla_pg.safe_psql("CREATE TABLE xt (id int PRIMARY KEY)")
    vanilla_pg.safe_psql("SELECT pg_create_logical_replication_slot('epoch', 'chelabase_keys')")
    with vanilla_pg.cursor(autocommit=False) as cur:
        cur.execute("INSERT INTO xt VALUES (1)")
        cur.execute("SELECT pg_current_xact_id()::text")
        xid = int(cur.fetchone()[0])
        cur.connection.commit()
    assert xid >= 2**32, f"the epoch must be above 0: {xid}"
    rows = vanilla_pg.safe_psql("SELECT data FROM pg_logical_slot_get_changes('epoch', NULL, NULL)")
    assert [json.loads(r[0])["xid"] for r in rows] == [xid]
```
RED: `make check` fails (no `chelabase_keys.c`, so the module doesn't build; each test fails at slot creation: `could not access file "chelabase_keys"`); the Python tests fail at slot creation. Assumptions checked: `pg_logical_slot_get_changes(slot_name, upto_lsn, upto_nchanges, VARIADIC options)` (Postgres 17 catalog); TOAST with `STORAGE EXTERNAL` stores 3000 bytes out of line and the btree entry is compressed, `repeat('x', 3000)` being compressible; `heapam.c:9589-9590` logs the old key for an external key column; neon_local computes run `wal_level = logical` (logical slots are made there without settings, `test_runner/regress/test_logical_replication.py:218-224`); compute loopback and local are `trust` (`libs/postgres_ffi/samples/pg_hba.conf`); the fixture `vanilla_pg` is initialised but not started (`neon_fixtures.py:3583-3594`); `pg_resetwal -e` sets the epoch on a cleanly shut down cluster.

**Gotchas:** the forced settings must cover `regclass` output (`search_path = ''`) and `money` (`lc_monetary = 'C'`); `escape_json` for every string; no stream or prepare callbacks; `startup_cb` sees `NIL` options at slot creation (`is_init`); `xid` via `FullTransactionIdFromAllowableAt` in `commit_cb`.

**Verify:** `CHELA_NEON_VOLUME_SUFFIX=t2 .github/scripts/ci-local.sh --only build,pg-check` (in `-t2`); `CHELA_NEON_VOLUME_SUFFIX=t2 .github/scripts/ci-local.sh --only regress -k "test_chelabase_keys"` (one container at a time). `postgres.mk` doesn't install the module into `pg_install` until T7: for the Python run here the implementer installs it into the clone's volume with `make -C build/v17/contrib/chelabase_keys install` inside the container (the run's `inside` shell); T7 makes it permanent.

**Live runs:** none.

---

## T4: `REASSIGN OWNED` event triggers; `hba.c` `map` with `clientcert=verify-full` (opus / opus)

**Setup (coordinator):** as T2 with suffix `t4`, Neon branch `round-3-t4`, Postgres branch `round-3-v17-t4` from `round-3-v17`.

**Files:** `src/backend/tcop/utility.c`, `src/include/tcop/cmdtaglist.h`, `src/backend/libpq/hba.c`, `doc/src/sgml/event-trigger.sgml`, `doc/src/sgml/client-auth.sgml` (the `map` sentence); tests (coordinator): `src/test/regress/sql/event_trigger.sql`, `src/test/regress/expected/event_trigger.out` (appended at the end), Neon `test_runner/regress/test_member_logins_hba.py`.

**Behaviour:** spec §5; spec §4.2's `hba.c` paragraph: `map` is accepted on a line with `clientcert=verify-full` whatever the auth method and option order (validated once the line's options are parsed), and governs only the certificate's CN check.

**RED tests (coordinator).** Appended to `src/test/regress/sql/event_trigger.sql` (the file ends with `event_triggers = 'off'`, `:last`):
```sql
-- REASSIGN OWNED fires the DDL event triggers (Chelabase)
SET event_triggers = 'on';
CREATE ROLE regress_reassign_a;
CREATE ROLE regress_reassign_b;
CREATE TABLE reassign_t (id int);
ALTER TABLE reassign_t OWNER TO regress_reassign_a;
CREATE FUNCTION reassign_report() RETURNS event_trigger LANGUAGE plpgsql AS $$
BEGIN
  RAISE NOTICE '% %', tg_event, tg_tag;
END $$;
CREATE EVENT TRIGGER reassign_start ON ddl_command_start
  WHEN TAG IN ('REASSIGN OWNED') EXECUTE FUNCTION reassign_report();
CREATE EVENT TRIGGER reassign_end ON ddl_command_end
  WHEN TAG IN ('REASSIGN OWNED') EXECUTE FUNCTION reassign_report();
REASSIGN OWNED BY regress_reassign_a TO regress_reassign_b;
SELECT pg_get_userbyid(relowner) = 'regress_reassign_b' AS moved FROM pg_class WHERE relname = 'reassign_t';
CREATE FUNCTION reassign_refuse() RETURNS event_trigger LANGUAGE plpgsql AS $$
BEGIN
  RAISE EXCEPTION 'refused %', tg_tag;
END $$;
CREATE EVENT TRIGGER reassign_refuse ON ddl_command_end
  WHEN TAG IN ('REASSIGN OWNED') EXECUTE FUNCTION reassign_refuse();
REASSIGN OWNED BY regress_reassign_b TO regress_reassign_a;
SELECT pg_get_userbyid(relowner) = 'regress_reassign_b' AS kept FROM pg_class WHERE relname = 'reassign_t';
DROP EVENT TRIGGER reassign_refuse;
DROP EVENT TRIGGER reassign_start;
DROP EVENT TRIGGER reassign_end;
DROP FUNCTION reassign_report(), reassign_refuse();
DROP TABLE reassign_t;
DROP ROLE regress_reassign_a, regress_reassign_b;
SET event_triggers = 'off';
```
Appended to `expected/event_trigger.out` (echo of every line, notices, results; blank source lines aren't echoed):
```
-- REASSIGN OWNED fires the DDL event triggers (Chelabase)
SET event_triggers = 'on';
CREATE ROLE regress_reassign_a;
CREATE ROLE regress_reassign_b;
CREATE TABLE reassign_t (id int);
ALTER TABLE reassign_t OWNER TO regress_reassign_a;
CREATE FUNCTION reassign_report() RETURNS event_trigger LANGUAGE plpgsql AS $$
BEGIN
  RAISE NOTICE '% %', tg_event, tg_tag;
END $$;
CREATE EVENT TRIGGER reassign_start ON ddl_command_start
  WHEN TAG IN ('REASSIGN OWNED') EXECUTE FUNCTION reassign_report();
CREATE EVENT TRIGGER reassign_end ON ddl_command_end
  WHEN TAG IN ('REASSIGN OWNED') EXECUTE FUNCTION reassign_report();
REASSIGN OWNED BY regress_reassign_a TO regress_reassign_b;
NOTICE:  ddl_command_start REASSIGN OWNED
NOTICE:  ddl_command_end REASSIGN OWNED
SELECT pg_get_userbyid(relowner) = 'regress_reassign_b' AS moved FROM pg_class WHERE relname = 'reassign_t';
 moved 
-------
 t
(1 row)

CREATE FUNCTION reassign_refuse() RETURNS event_trigger LANGUAGE plpgsql AS $$
BEGIN
  RAISE EXCEPTION 'refused %', tg_tag;
END $$;
CREATE EVENT TRIGGER reassign_refuse ON ddl_command_end
  WHEN TAG IN ('REASSIGN OWNED') EXECUTE FUNCTION reassign_refuse();
REASSIGN OWNED BY regress_reassign_b TO regress_reassign_a;
NOTICE:  ddl_command_start REASSIGN OWNED
NOTICE:  ddl_command_end REASSIGN OWNED
ERROR:  refused REASSIGN OWNED
CONTEXT:  PL/pgSQL function reassign_refuse() line 3 at RAISE
SELECT pg_get_userbyid(relowner) = 'regress_reassign_b' AS kept FROM pg_class WHERE relname = 'reassign_t';
 kept 
------
 t
(1 row)

DROP EVENT TRIGGER reassign_refuse;
DROP EVENT TRIGGER reassign_start;
DROP EVENT TRIGGER reassign_end;
DROP FUNCTION reassign_report(), reassign_refuse();
DROP TABLE reassign_t;
DROP ROLE regress_reassign_a, regress_reassign_b;
SET event_triggers = 'off';
```
(Two `ddl_command_end` triggers fire in name order: `reassign_end` before `reassign_refuse`.)

`test_runner/regress/test_member_logins_hba.py` (Neon; `VanillaPostgres` with TLS and the member block from the pinned interface):
```python
from __future__ import annotations

import os
import subprocess
from pathlib import Path
from typing import TYPE_CHECKING

import psycopg2
import pytest
from fixtures.pg_version import PgVersion
from fixtures.utils import run_only_on_postgres

if TYPE_CHECKING:
    from fixtures.neon_fixtures import VanillaPostgres

MEMBER = "cb_m_0123456789abcdef0123456789abcdef"
REJECT = [
    "host\tall\t/^[Cc][Bb]_[Mm]_\tall\treject",
    "host\treplication\t/^[Cc][Bb]_[Mm]_\tall\treject",
    "local\tall\t/^[Cc][Bb]_[Mm]_\treject",
    "local\treplication\t/^[Cc][Bb]_[Mm]_\treject",
]


def block(cidr: str) -> list[str]:
    return [
        "# chelabase: member logins (cb_m_*) only from the orchestrator",
        f"hostssl\tall\t/^cb_m_[0-9a-f]{{32}}$\t{cidr}\tscram-sha-256\tclientcert=verify-full\tmap=chelabase_member",
        *REJECT,
        "# chelabase: end of member logins",
    ]


def openssl(*args: str | Path):
    subprocess.run(["openssl", *[str(a) for a in args]], check=True, capture_output=True)


def issue(d: Path, name: str, cn: str):
    key, csr, crt = d / f"{name}.key", d / f"{name}.csr", d / f"{name}.crt"
    openssl("req", "-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:P-256", "-nodes",
            "-keyout", key, "-out", csr, "-subj", f"/CN={cn}")
    openssl("x509", "-req", "-in", csr, "-CA", d / "ca.crt", "-CAkey", d / "ca.key",
            "-CAcreateserial", "-days", "2", "-out", crt)
    os.chmod(key, 0o600)


@pytest.fixture
def certs(tmp_path: Path) -> Path:
    openssl("req", "-x509", "-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:P-256", "-nodes",
            "-keyout", tmp_path / "ca.key", "-out", tmp_path / "ca.crt", "-days", "2",
            "-subj", "/CN=test compute CA")
    issue(tmp_path, "server", "localhost")
    issue(tmp_path, "orchestrator", "chelabase-orchestrator")
    issue(tmp_path, "compute", "ep-x.compute.chelabase.internal")
    return tmp_path


def setup(pg: VanillaPostgres, certs: Path, cidr: str):
    pg.configure([
        "ssl = on",
        f"ssl_cert_file = '{certs / 'server.crt'}'",
        f"ssl_key_file = '{certs / 'server.key'}'",
        f"ssl_ca_file = '{certs / 'ca.crt'}'",
        "password_encryption = 'scram-sha-256'",
    ])
    hba = pg.pgdatadir / "pg_hba.conf"
    original = hba.read_text()
    hba.write_text("\n".join(block(cidr)) + "\n" + original)
    with open(pg.pgdatadir / "pg_ident.conf", "a") as f:
        f.write("chelabase_member\tchelabase-orchestrator\t/^cb_m_[0-9a-f]{32}$\n")


def member(pg: VanillaPostgres, certs: Path, cert: str | None, **kw):
    opts = dict(user=MEMBER, password="pw", host="127.0.0.1", sslmode="require",
                sslrootcert=str(certs / "ca.crt"))
    if cert:
        opts.update(sslcert=str(certs / f"{cert}.crt"), sslkey=str(certs / f"{cert}.key"))
    opts.update(kw)
    with pg.cursor(**opts) as cur:
        cur.execute("SELECT current_user")
        return cur.fetchone()[0]


@run_only_on_postgres([PgVersion.V17], "the hba map change is v17 only")
def test_member_logins_need_the_certificate_and_the_address(vanilla_pg: VanillaPostgres, certs: Path):
    setup(vanilla_pg, certs, "127.0.0.1/32")
    vanilla_pg.start()
    vanilla_pg.safe_psql(f"CREATE ROLE {MEMBER} LOGIN PASSWORD 'pw'")
    vanilla_pg.safe_psql("CREATE ROLE plain LOGIN PASSWORD 'pw'")
    assert member(vanilla_pg, certs, "orchestrator") == MEMBER
    for cert, kw, needle in [
        (None, {}, "connection requires a valid client certificate"),
        # A CN the map doesn't name fails after the password check; the client sees only the
        # generic auth failure (auth.c:645-653, :2785-2805 logs the CN mismatch server-side).
        ("compute", {}, "password authentication failed"),
        ("orchestrator", {"sslmode": "disable", "sslrootcert": None}, "pg_hba.conf rejects connection"),
        (None, {"host": "/tmp", "sslmode": "disable", "sslrootcert": None}, "pg_hba.conf rejects connection"),
    ]:
        with pytest.raises(psycopg2.OperationalError) as e:
            member(vanilla_pg, certs, cert, **kw)
        assert needle in str(e.value), (cert, kw, str(e.value))
    with vanilla_pg.cursor(user="plain", password="pw", host="127.0.0.1", sslmode="disable") as cur:
        cur.execute("SELECT 1")
    vanilla_pg.stop()
    hba = vanilla_pg.pgdatadir / "pg_hba.conf"
    hba.write_text(hba.read_text().replace("127.0.0.1/32\tscram", "10.255.255.254/32\tscram"))
    vanilla_pg.start()
    with pytest.raises(psycopg2.OperationalError) as e:
        member(vanilla_pg, certs, "orchestrator")
    assert "pg_hba.conf rejects connection" in str(e.value)


@run_only_on_postgres([PgVersion.V17], "the hba map change is v17 only")
def test_an_empty_list_is_no_address_rule(vanilla_pg: VanillaPostgres, certs: Path):
    setup(vanilla_pg, certs, "all")
    vanilla_pg.start()
    vanilla_pg.safe_psql(f"CREATE ROLE {MEMBER} LOGIN PASSWORD 'pw'")
    assert member(vanilla_pg, certs, "orchestrator") == MEMBER


@run_only_on_postgres([PgVersion.V17], "the hba map change is v17 only")
def test_map_without_verify_full_stays_refused(vanilla_pg: VanillaPostgres, certs: Path):
    setup(vanilla_pg, certs, "127.0.0.1/32")
    vanilla_pg.start()
    with open(vanilla_pg.pgdatadir / "pg_hba.conf", "a") as f:
        f.write("hostssl\tall\tbad_map\t127.0.0.1/32\tscram-sha-256\tmap=chelabase_member\n")
        f.write("hostssl\tall\tgood_map\t127.0.0.1/32\tscram-sha-256\tmap=chelabase_member\tclientcert=verify-full\n")
    rows = dict(vanilla_pg.safe_psql(
        "SELECT user_name[1], coalesce(error, '') FROM pg_hba_file_rules WHERE user_name[1] IN ('bad_map', 'good_map')"
    ))
    assert 'authentication option "map" is only valid' in rows["bad_map"]
    assert rows["good_map"] == ""
```
RED: the event-trigger block fails (`REASSIGN OWNED` can't be a `WHEN TAG` value today, `cmdtaglist.h:195`; no notices); the hba tests fail at server start (`map` refused on a scram line, `hba.c:2059-2066`, so `pg_hba.conf` doesn't load) and in `pg_hba_file_rules` for `good_map`. Assumptions checked: event triggers on the same event fire in name order (`EventCacheLookup` sorts by name); `pg_hba_file_rules` reads the file on disk (Postgres 17); `VanillaPostgres` connects to `localhost` by default (`neon_fixtures.py:3493`) and its socket is in `/tmp` (compiled default); initdb's default `pg_hba.conf` trusts loopback and the socket; with `sslrootcert` set, `sslmode=require` verifies the CA (libpq).

**Gotchas:** `REASSIGN OWNED` moves into `ProcessUtilitySlow` exactly like `DROP OWNED` (`utility.c:1827-1831`: `commandCollected = true`); the assertion cross-check at `event_trigger.c:661-681` must pass in cassert builds (ci-local builds `--enable-cassert` for debug, `Makefile:40`).

**Verify:** `CHELA_NEON_VOLUME_SUFFIX=t4 .github/scripts/ci-local.sh --only build,pg-check`; `… --only regress -k "test_member_logins_hba"`.

**Live runs:** none.

---

## T8: `crates/chelabase-neon`: compute TLS, spec, backend, status (opus / opus)

**Worktree:** `just wt-add neon-fork-round-3-t8 neon-fork-round-3`.

**Files:** `crates/chelabase-neon/src/{compute_tls.rs (new), lib.rs, spec.rs, backend.rs, reconfigure.rs}`, `crates/chelabase-neon/Cargo.toml`, workspace `Cargo.toml` (`[workspace.dependencies]`: `rcgen`, `x509-parser`, `tokio-postgres-rustls-improved`, `tar`; `chelabase-neon` gains `tokio-postgres`, `rustls`, `arc-swap`); callers updated to compile: `services/worker/src/reset.rs:124`, `services/worker/src/config.rs:260`, `services/orchestrator/src/internal/spec.rs:29` (pass the config's `MemberLogins`; T9/T10 wire the env); tests (coordinator): `crates/chelabase-neon/tests/compute_tls.rs`, additions to `crates/chelabase-neon/src/spec.rs`'s test module, `crates/chelabase-neon/tests/reconfigure_mock.rs`.

**Behaviour:** spec §4.2 (certificates, server name), §7 (status); the pinned interfaces. `DockerBackend::create`: create → `connect_network(control_network)` → `upload_to_container(TLS_UPLOAD_ROOT, tls_files_tar(issue_server(..), ca))` → start. `read_file` downloads one file from a container (for the sweep).

**RED tests (coordinator).** The RED commit adds `pub mod compute_tls;` with an empty `compute_tls.rs`, and the four dependencies to the manifests.

`crates/chelabase-neon/tests/compute_tls.rs`:
```rust
//! Round 3: the platform CA, compute server and orchestrator client certificates, the upload
//! tar and the client pair on disk.

use std::io::Read;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use chelabase_neon::compute_tls::*;
use rustls::client::danger::ServerCertVerifier;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};

const DAY: Duration = Duration::from_secs(86_400);

fn der(pem: &str) -> CertificateDer<'static> {
    CertificateDer::from_pem_slice(pem.as_bytes()).expect("a PEM certificate")
}

fn roots(ca: &ComputeCa) -> Arc<rustls::RootCertStore> {
    let mut roots = rustls::RootCertStore::empty();
    roots.add(der(ca.cert_pem())).unwrap();
    Arc::new(roots)
}

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::aws_lc_rs::default_provider())
}

fn verify_server(ca: &ComputeCa, cert_pem: &str, name: &str, now: SystemTime) -> Result<(), rustls::Error> {
    let verifier = rustls::client::WebPkiServerVerifier::builder_with_provider(roots(ca), provider())
        .build()
        .unwrap();
    let name = ServerName::try_from(name.to_string()).unwrap();
    verifier
        .verify_server_cert(&der(cert_pem), &[], &name, &[], UnixTime::since_unix_epoch(now.duration_since(SystemTime::UNIX_EPOCH).unwrap()))
        .map(|_| ())
}

fn parsed(pem: &str) -> (String, bool, bool, SystemTime, String, String) {
    let (_, p) = x509_parser::pem::parse_x509_pem(pem.as_bytes()).unwrap();
    let cert = p.parse_x509().unwrap();
    let cn = cert.subject().iter_common_name().next().unwrap().as_str().unwrap().to_string();
    let eku = cert.extended_key_usage().unwrap().map(|e| e.value.clone());
    let (server, client) = eku.map_or((false, false), |e| (e.server_auth, e.client_auth));
    let not_after = SystemTime::UNIX_EPOCH + Duration::from_secs(cert.validity().not_after.timestamp() as u64);
    let sig = cert.signature_algorithm.algorithm.to_id_string();
    let key_alg = cert.public_key().algorithm.algorithm.to_id_string();
    (cn, server, client, not_after, sig, key_alg)
}

fn near(a: SystemTime, b: SystemTime) -> bool {
    let d = a.duration_since(b).unwrap_or_else(|e| e.duration());
    d < DAY
}

#[test]
fn the_server_name_is_the_endpoint_under_the_suffix() {
    assert_eq!(server_name("ep-abc123"), "ep-abc123.compute.chelabase.internal");
    assert_eq!(SERVER_NAME_SUFFIX, "compute.chelabase.internal");
    assert_eq!(ORCHESTRATOR_CN, "chelabase-orchestrator");
    assert_eq!((TLS_DIR, TLS_UPLOAD_ROOT), ("/var/db/chelabase/tls", "/var/db"));
    assert_eq!(
        (SERVER_KEY_PATH, SERVER_CERT_PATH, CA_CERT_PATH),
        ("/var/db/chelabase/tls/server.key", "/var/db/chelabase/tls/server.crt", "/var/db/chelabase/tls/ca.crt")
    );
}

#[test]
fn a_server_certificate_verifies_for_its_endpoint_only() {
    let now = SystemTime::now();
    let ca = ComputeCa::generate(now).unwrap();
    let server = ca.issue_server("ep-a", now).unwrap();
    verify_server(&ca, &server.cert_pem, &server_name("ep-a"), now).unwrap();
    assert!(verify_server(&ca, &server.cert_pem, &server_name("ep-b"), now).is_err());
    let other = ComputeCa::generate(now).unwrap();
    assert!(verify_server(&other, &server.cert_pem, &server_name("ep-a"), now).is_err(), "another CA");
    let (_, server_auth, client_auth, not_after, sig, key_alg) = parsed(&server.cert_pem);
    assert!(server_auth && !client_auth, "EKU serverAuth only");
    assert!(near(not_after, now + SERVER_VALIDITY) && near(server.not_after, not_after));
    assert_eq!(SERVER_VALIDITY, 90 * DAY);
    assert_eq!(sig, "1.2.840.10045.4.3.2", "ecdsa-with-SHA256, the only kind compute_ctl reads");
    assert_eq!(key_alg, "1.2.840.10045.2.1", "an EC key");
    assert!(server.key_pem.starts_with("-----BEGIN PRIVATE KEY-----"), "PKCS#8");
}

#[test]
fn a_client_certificate_carries_the_cn_and_client_auth() {
    let now = SystemTime::now();
    let ca = ComputeCa::generate(now).unwrap();
    let client = ca.issue_client(ORCHESTRATOR_CN, now).unwrap();
    let (cn, server_auth, client_auth, not_after, _, _) = parsed(&client.cert_pem);
    assert_eq!(cn, "chelabase-orchestrator");
    assert!(client_auth && !server_auth, "EKU clientAuth only");
    assert_eq!(CLIENT_VALIDITY, 365 * DAY);
    assert!(near(not_after, now + CLIENT_VALIDITY));
    let verifier = rustls::server::WebPkiClientVerifier::builder_with_provider(roots(&ca), provider())
        .build()
        .unwrap();
    verifier
        .verify_client_cert(&der(&client.cert_pem), &[], UnixTime::since_unix_epoch(now.duration_since(SystemTime::UNIX_EPOCH).unwrap()))
        .unwrap();
}

#[test]
fn the_ca_round_trips_through_pem_and_lasts_ten_years() {
    let now = SystemTime::now();
    let ca = ComputeCa::generate(now).unwrap();
    let again = ComputeCa::from_pem(ca.cert_pem(), ca.key_pem()).unwrap();
    let server = again.issue_server("ep-a", now).unwrap();
    verify_server(&ca, &server.cert_pem, &server_name("ep-a"), now).unwrap();
    assert_eq!(CA_VALIDITY, 3650 * DAY);
    assert!(near(not_after(ca.cert_pem()).unwrap(), now + CA_VALIDITY));
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("ca.crt"), ca.cert_pem()).unwrap();
    std::fs::write(dir.path().join("ca.key"), ca.key_pem()).unwrap();
    assert_eq!(ComputeCa::load(dir.path()).unwrap().cert_pem(), ca.cert_pem());
    assert!(ComputeCa::from_pem(ca.cert_pem(), "not a key").is_err());
}

#[test]
fn renewal_windows() {
    let now = SystemTime::now();
    assert_eq!((SERVER_RENEW_BEFORE, CLIENT_RENEW_BEFORE), (30 * DAY, 60 * DAY));
    assert!(needs_renewal(now + 29 * DAY, now, SERVER_RENEW_BEFORE));
    assert!(!needs_renewal(now + 31 * DAY, now, SERVER_RENEW_BEFORE));
    assert!(needs_renewal(now - DAY, now, SERVER_RENEW_BEFORE), "expired");
    assert!(needs_renewal(now + 59 * DAY, now, CLIENT_RENEW_BEFORE));
}

#[test]
fn the_upload_tar_holds_the_three_files_under_var_db() {
    let now = SystemTime::now();
    let ca = ComputeCa::generate(now).unwrap();
    let server = ca.issue_server("ep-a", now).unwrap();
    let tar = tls_files_tar(&server, ca.cert_pem()).unwrap();
    let mut archive = tar::Archive::new(tar.as_slice());
    let mut seen = Vec::new();
    for entry in archive.entries().unwrap() {
        let mut entry = entry.unwrap();
        let path = entry.path().unwrap().to_string_lossy().trim_end_matches('/').to_string();
        let header = entry.header();
        let (mode, uid, gid) = (header.mode().unwrap(), header.uid().unwrap(), header.gid().unwrap());
        let mut body = String::new();
        entry.read_to_string(&mut body).unwrap();
        seen.push((path, mode, uid, gid, body));
    }
    let find = |p: &str| seen.iter().find(|s| s.0 == p).unwrap_or_else(|| panic!("{p} in {seen:?}")).clone();
    assert_eq!(find("chelabase").1, 0o755);
    assert_eq!(find("chelabase/tls").1, 0o755);
    for (p, body) in [
        ("chelabase/tls/server.crt", server.cert_pem.as_str()),
        ("chelabase/tls/server.key", server.key_pem.as_str()),
        ("chelabase/tls/ca.crt", ca.cert_pem()),
    ] {
        let (_, mode, uid, gid, got) = find(p);
        assert_eq!((mode, uid, gid), (0o644, 0, 0), "{p}");
        assert_eq!(got, body, "{p}");
    }
    assert_eq!(seen.len(), 5);
}

#[test]
fn the_client_pair_is_swapped_whole_and_reloaded() {
    let now = SystemTime::now();
    let ca = ComputeCa::generate(now).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let first = ca.issue_client(ORCHESTRATOR_CN, now).unwrap();
    let v1 = write_client_pair(dir.path(), &first, ca.cert_pem(), now).unwrap();
    assert_eq!(std::fs::read_link(dir.path().join("current")).unwrap().file_name(), v1.file_name());
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(v1.join("client.key")).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    let tls = ClientTls::load(dir.path()).unwrap();
    let f1 = tls.fingerprint();
    assert!(!tls.reload_if_changed().unwrap(), "nothing changed");
    let second = ca.issue_client(ORCHESTRATOR_CN, now + Duration::from_secs(1)).unwrap();
    write_client_pair(dir.path(), &second, ca.cert_pem(), now + Duration::from_secs(1)).unwrap();
    assert!(tls.reload_if_changed().unwrap());
    assert_ne!(tls.fingerprint(), f1);
    assert!(!tls.reload_if_changed().unwrap());
    assert!(ClientTls::load(tempfile::tempdir().unwrap().path()).is_err(), "no pair");
}

#[test]
fn member_login_cidrs_parse() {
    use chelabase_neon::spec::parse_member_login_cidrs;
    assert!(parse_member_login_cidrs(None).is_err(), "unset is a startup error");
    assert_eq!(parse_member_login_cidrs(Some("")).unwrap(), Vec::<String>::new());
    assert_eq!(
        parse_member_login_cidrs(Some(" 10.213.4.10/32, 10.213.4.11/32 ")).unwrap(),
        vec!["10.213.4.10/32".to_string(), "10.213.4.11/32".to_string()]
    );
    assert_eq!(parse_member_login_cidrs(Some("fd00::1/128")).unwrap(), vec!["fd00::1/128".to_string()]);
    for bad in ["x", "10.0.0.1", "10.0.0.1/33", "10.0.0.1/32,", "10.0.0.1/32 # c"] {
        assert!(parse_member_login_cidrs(Some(bad)).is_err(), "{bad:?}");
    }
}
```
Added to `crates/chelabase-neon/src/spec.rs`'s test module (and every existing `SpecInputs` literal there gets `member_logins: logins()`; `primary_spec_unchanged`'s key list gains `"features"` and `"member_logins"` in sorted position):
```rust
    fn logins() -> MemberLogins {
        MemberLogins {
            allowed_cidrs: vec!["10.213.4.10/32".into(), "10.213.4.11/32".into()],
            client_cns: vec!["chelabase-orchestrator".into()],
        }
    }

    #[test]
    fn every_compute_gets_tls_and_the_member_login_rules() {
        let tenant = NeonId::from(Uuid::from_u128(1));
        let timeline = NeonId::from(Uuid::from_u128(2));
        let v = config_response(&SpecInputs {
            endpoint_id: "ep-a",
            tenant_id: &tenant,
            timeline_id: &timeline,
            pageserver_connstring: "postgresql://no_user@ps:6400".into(),
            safekeeper_connstrings: vec![],
            roles: vec![],
            databases: vec![],
            jwks: json!({}),
            console_url: None,
            mode: ComputeMode::Primary,
            member_logins: logins(),
        });
        assert_eq!(v["spec"]["features"], json!(["tls_experimental"]));
        assert_eq!(
            v["compute_ctl_config"]["tls"],
            json!({
                "key_path": "/var/db/chelabase/tls/server.key",
                "cert_path": "/var/db/chelabase/tls/server.crt",
                "ca_path": "/var/db/chelabase/tls/ca.crt",
            })
        );
        assert_eq!(
            v["spec"]["member_logins"],
            json!({
                "allowed_cidrs": ["10.213.4.10/32", "10.213.4.11/32"],
                "client_cns": ["chelabase-orchestrator"],
            })
        );
        // An empty list is "no address rule" (Kubernetes), still sent as an empty list.
        let mut empty = logins();
        empty.allowed_cidrs.clear();
        let v = config_response(&SpecInputs { member_logins: empty, ..inputs_for_tests(&tenant, &timeline) });
        assert_eq!(v["spec"]["member_logins"]["allowed_cidrs"], json!([]));
    }
```
(`inputs_for_tests(&tenant, &timeline) -> SpecInputs` is a small test helper the RED commit adds beside `logins()`, building the literal above.)

Added to `crates/chelabase-neon/tests/reconfigure_mock.rs`:
```rust
async fn status_compute(body: Value) -> (SocketAddr, Arc<Seen>) {
    let seen = Arc::new(Seen::default());
    let app = Router::new()
        .route(
            "/status",
            axum::routing::get(move |State(seen): State<Arc<Seen>>, headers: HeaderMap| {
                let body = body.clone();
                async move {
                    *seen.auth.lock().unwrap() =
                        headers.get("authorization").map(|v| v.to_str().unwrap().to_string());
                    (StatusCode::OK, axum::Json(body))
                }
            }),
        )
        .with_state(seen.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (addr, seen)
}

#[tokio::test]
async fn status_reads_compute_ctl_with_its_token() {
    use chelabase_neon::reconfigure::{ComputeCtlStatus, status};
    let (pem, x) = generate_keypair();
    let signer = ComputeSigner::new(&pem, &x).unwrap();
    let base = json!({"start_time": "2026-10-15T00:00:00Z", "tenant": null, "timeline": null,
                      "last_active": null, "error": null});
    let with = |s: &str, e: Option<&str>| {
        let mut b = base.clone();
        b["status"] = json!(s);
        b["error"] = json!(e);
        b
    };
    for (body, want) in [
        (with("running", None), ComputeCtlStatus::Running),
        (with("failed", Some("migration 2 failed")), ComputeCtlStatus::Failed("migration 2 failed".into())),
        (with("init", None), ComputeCtlStatus::Other("init".into())),
        (with("configuration_pending", None), ComputeCtlStatus::Other("configuration_pending".into())),
    ] {
        let (addr, seen) = status_compute(body).await;
        let got = status(&reqwest::Client::new(), addr, &signer, "ep-a").await.unwrap();
        assert_eq!(got, want);
        assert!(seen.auth.lock().unwrap().clone().unwrap().starts_with("Bearer "));
    }
}
```
RED: compile errors on the missing items of `compute_tls`, `MemberLogins`, `parse_member_login_cidrs`, `status`, `ComputeCtlStatus` (the code under test). Assumptions checked: compute_ctl's `/status` body is `ComputeStatusResponse` with `status` snake_case (`libs/compute_api/src/responses.rs:141-151`, `:172-175`) and needs the JWT (`compute_tools/src/http/server.rs:101-126`); rustls 0.23 has `WebPkiServerVerifier::builder_with_provider`, `WebPkiClientVerifier::builder_with_provider`, `pki_types::pem::PemObject` (pki-types 1.15.1); `tempfile` is already a dev-dependency (`crates/chelabase-neon/Cargo.toml`).

**Gotchas:** compute_ctl reads only ECDSA P-256 with SHA-256 signatures (`compute_tools/src/tls.rs:91-118`); the orchestrator must not need the CA key (`ComputeSettings.ca` is `None` there); `connect_network` before `start` (Docker accepts it on a created container).

**Verify:** `cargo test -p chelabase-neon`; `cargo clippy -p chelabase-neon --all-targets`; `cargo check -p worker -p orchestrator` (the updated callers compile); `cargo deny check` for the four new crates.

**Live runs:** none (`docker_backend_live` and the rest run at End of step 5).

---

## T3: `chelabase_realtime` restriction, `rt_` names, superuser connect to closed databases (opus / opus)

**Setup (coordinator), after T2's review:** clone `-t3` as T2, Neon branch `round-3-t3` from `round-3`; Postgres branch `round-3-v17-t3` from `origin/round-3-v17-t2` (it needs the plugin); suffix `t3`.

**Files:** `src/backend/replication/walsender.c`, `src/backend/utils/init/postinit.c`, `src/backend/replication/slot.c`, `src/include/replication/chelabase.h`; tests (coordinator): Neon `test_runner/regress/test_chelabase_realtime_role.py`, `test_runner/regress/test_closed_database_connect.py`.

**Behaviour:** spec §3 rules 1–7 (D2, S6–S8, RT3-15) and §6's `postinit.c` rule (a superuser connects to a closed non-template database); the pinned messages.

**RED tests (coordinator).** `test_runner/regress/test_chelabase_realtime_role.py`:
```python
from __future__ import annotations

import json
import select
import time
from typing import TYPE_CHECKING

import psycopg2
import pytest
from fixtures.pg_version import PgVersion
from fixtures.utils import run_only_on_postgres
from psycopg2.errors import InsufficientPrivilege
from psycopg2.extras import LogicalReplicationConnection, PhysicalReplicationConnection

if TYPE_CHECKING:
    from fixtures.neon_fixtures import VanillaPostgres

pytestmark = run_only_on_postgres([PgVersion.V17], "the restriction is in the v17 fork only")
RT = "chelabase_realtime"


@pytest.fixture
def pg(vanilla_pg: VanillaPostgres) -> VanillaPostgres:
    vanilla_pg.configure(["wal_level = logical", "max_replication_slots = 16", "max_wal_senders = 16"])
    vanilla_pg.start()
    vanilla_pg.safe_psql(f"CREATE ROLE {RT} LOGIN REPLICATION NOBYPASSRLS NOINHERIT")
    vanilla_pg.safe_psql("CREATE ROLE repl LOGIN REPLICATION")
    vanilla_pg.safe_psql("CREATE TABLE t (id int PRIMARY KEY)")
    return vanilla_pg


def logical(pg: VanillaPostgres, user: str = RT):
    conn = psycopg2.connect(pg.connstr(user=user), connection_factory=LogicalReplicationConnection)
    return conn, conn.cursor()


def refused(cur, command: str, needle: str):
    with pytest.raises(InsufficientPrivilege) as e:
        cur.execute(command)
    assert needle in str(e.value), (command, str(e.value))


def test_normal_and_physical_logins_are_refused(pg: VanillaPostgres):
    for factory in [None, PhysicalReplicationConnection]:
        with pytest.raises(psycopg2.OperationalError) as e:
            if factory:
                psycopg2.connect(pg.connstr(user=RT), connection_factory=factory)
            else:
                psycopg2.connect(pg.connstr(user=RT))
        assert 'role "chelabase_realtime" may only open logical replication connections' in str(e.value)


def test_no_sql_in_the_walsender(pg: VanillaPostgres):
    _, cur = logical(pg)
    for sql in ["SELECT 1", "SELECT pg_create_logical_replication_slot('x', 'test_decoding')", "BEGIN"]:
        refused(cur, sql, "can't run SQL in a replication connection")


def test_other_commands_are_refused(pg: VanillaPostgres):
    _, cur = logical(pg)
    for command, tag in [
        ("DROP_REPLICATION_SLOT rt_none", "DROP_REPLICATION_SLOT"),
        ("ALTER_REPLICATION_SLOT rt_none (FAILOVER false)", "ALTER_REPLICATION_SLOT"),
        ("READ_REPLICATION_SLOT rt_none", "READ_REPLICATION_SLOT"),
        ("BASE_BACKUP", "BASE_BACKUP"),
        ("TIMELINE_HISTORY 1", "TIMELINE_HISTORY"),
        ("SHOW wal_level", "SHOW"),
    ]:
        refused(cur, command, f'permission denied for replication command "{tag}"')


@pytest.mark.parametrize(
    "command",
    [
        "CREATE_REPLICATION_SLOT rt_g LOGICAL chelabase_keys",
        "CREATE_REPLICATION_SLOT rt_f TEMPORARY PHYSICAL",
        "CREATE_REPLICATION_SLOT rt_e TEMPORARY LOGICAL test_decoding",
        "CREATE_REPLICATION_SLOT x_h TEMPORARY LOGICAL chelabase_keys",
        "CREATE_REPLICATION_SLOT rt_i TEMPORARY LOGICAL chelabase_keys (TWO_PHASE)",
        "CREATE_REPLICATION_SLOT rt_j TEMPORARY LOGICAL chelabase_keys (FAILOVER)",
        "CREATE_REPLICATION_SLOT rt_k TEMPORARY LOGICAL chelabase_keys (SNAPSHOT 'use')",
    ],
)
def test_only_temporary_chelabase_keys_rt_slots(pg: VanillaPostgres, command: str):
    _, cur = logical(pg)
    refused(cur, command, 'permission denied for replication command "CREATE_REPLICATION_SLOT"')


def test_one_slot_per_session_and_only_its_own(pg: VanillaPostgres):
    pg.safe_psql("SELECT pg_create_logical_replication_slot('cust', 'test_decoding')")
    _, other = logical(pg)
    other.execute("CREATE_REPLICATION_SLOT rt_b TEMPORARY LOGICAL chelabase_keys (SNAPSHOT 'nothing')")
    other.fetchall()
    _, cur = logical(pg)
    cur.execute("CREATE_REPLICATION_SLOT rt_c TEMPORARY LOGICAL chelabase_keys (SNAPSHOT 'nothing')")
    cur.fetchall()
    refused(cur, "CREATE_REPLICATION_SLOT rt_d TEMPORARY LOGICAL chelabase_keys", "CREATE_REPLICATION_SLOT")
    for command in [
        "START_REPLICATION SLOT cust LOGICAL 0/0",
        "START_REPLICATION SLOT rt_b LOGICAL 0/0",
        "START_REPLICATION SLOT rt_c PHYSICAL 0/0",
    ]:
        with pytest.raises(InsufficientPrivilege) as e:
            cur.start_replication_expert(command)
        assert 'permission denied for replication command "START_REPLICATION"' in str(e.value), command
    assert pg.safe_psql("SELECT confirmed_flush_lsn IS NOT NULL FROM pg_replication_slots WHERE slot_name = 'cust'")[0][0]


def test_the_allowed_path(pg: VanillaPostgres):
    conn, cur = logical(pg)
    cur.execute("IDENTIFY_SYSTEM")
    assert len(cur.fetchall()) == 1
    cur.execute("CREATE_REPLICATION_SLOT rt_a TEMPORARY LOGICAL chelabase_keys (SNAPSHOT 'nothing')")
    cur.fetchall()
    cur.start_replication_expert("START_REPLICATION SLOT rt_a LOGICAL 0/0 (max_tables '10')", decode=True)
    pg.safe_psql("INSERT INTO t VALUES (7)")
    deadline = time.time() + 20
    while time.time() < deadline:
        msg = cur.read_message()
        if msg is not None:
            assert json.loads(msg.payload)["changes"][0]["k"] == [["7"]]
            break
        select.select([conn], [], [], 0.5)
    else:
        raise AssertionError("no message")


def test_another_replication_role_is_not_restricted(pg: VanillaPostgres):
    _, cur = logical(pg, user="repl")
    cur.execute("IDENTIFY_SYSTEM")
    cur.fetchall()
    cur.execute("CREATE_REPLICATION_SLOT plain_slot TEMPORARY LOGICAL test_decoding")
    cur.fetchall()
    cur.execute("SHOW wal_level")
    assert cur.fetchall()[0][0] == "logical"


def test_rt_names_are_realtimes_alone(pg: VanillaPostgres):
    needle = 'replication slot names starting with "rt_" are reserved for chelabase_realtime'
    with pg.cursor() as su:
        for sql in [
            "SELECT pg_create_logical_replication_slot('rt_x', 'test_decoding', true)",
            "SELECT pg_create_logical_replication_slot('rt_y', 'test_decoding')",
            "SELECT pg_create_physical_replication_slot('rt_z')",
        ]:
            with pytest.raises(InsufficientPrivilege) as e:
                su.execute(sql)
            assert needle in str(e.value), sql
        su.execute("SELECT pg_create_logical_replication_slot('src', 'test_decoding')")
        with pytest.raises(InsufficientPrivilege) as e:
            su.execute("SELECT pg_copy_logical_replication_slot('src', 'rt_copy')")
        assert needle in str(e.value)
        su.execute("SELECT pg_create_logical_replication_slot('fine', 'test_decoding')")
    _, cur = logical(pg, user="repl")
    with pytest.raises(InsufficientPrivilege) as e:
        cur.execute("CREATE_REPLICATION_SLOT rt_w TEMPORARY LOGICAL test_decoding")
    assert needle in str(e.value)
```
`test_runner/regress/test_closed_database_connect.py`:
```python
from __future__ import annotations

from typing import TYPE_CHECKING

import psycopg2
import pytest
from fixtures.pg_version import PgVersion
from fixtures.utils import run_only_on_postgres

if TYPE_CHECKING:
    from fixtures.neon_fixtures import VanillaPostgres


@run_only_on_postgres([PgVersion.V17], "the connect rule is in the v17 fork only")
def test_a_superuser_connects_to_a_closed_non_template_database(vanilla_pg: VanillaPostgres):
    vanilla_pg.start()
    vanilla_pg.safe_psql("CREATE DATABASE c1")
    vanilla_pg.safe_psql("ALTER DATABASE c1 ALLOW_CONNECTIONS false")
    vanilla_pg.safe_psql("CREATE ROLE plain LOGIN")
    assert vanilla_pg.safe_psql("SELECT current_database()", dbname="c1")[0][0] == "c1"
    for kw in [{"dbname": "template0"}, {"dbname": "c1", "user": "plain"}]:
        with pytest.raises(psycopg2.OperationalError) as e:
            vanilla_pg.safe_psql("SELECT 1", **kw)
        assert "is not currently accepting connections" in str(e.value), kw
```
RED: every refusal test fails (nothing refuses today; the normal login works; `rt_` names are accepted); the allowed path passes already (a control); the closed-database test fails at the superuser connect. Assumptions checked: `exec_replication_command`'s fallthrough and tags (`walsender.c:2086-2101`, `:2152-2252`); a temporary slot keeps its creator's `active_pid` (`slot.c:729-746`); `pg_copy_logical_replication_slot` creates through `ReplicationSlotCreate` (`slotfuncs.c:139`); `test_decoding` is in `pg_install` (`postgres.mk:118`) and `chelabase_keys` must be installed there by the implementer for the run as in T2 (T7 makes it permanent); `VanillaPostgres`' default user is the superuser running initdb; `safe_psql` passes `dbname`/`user` through `connstr` (`neon_fixtures.py:241-270`).

**Gotchas:** the identity check uses the authenticated user once (`GetAuthenticatedUserId`), stored in a backend-local flag; `IDENTIFY_SYSTEM` stays allowed (S6); the `rt_` check in `ReplicationSlotCreate` exempts `IsSyncingReplicationSlots()`; `CheckMyDatabase` gets `am_superuser` already (`postinit.c:313`, `:352`).

**Verify:** `CHELA_NEON_VOLUME_SUFFIX=t3 .github/scripts/ci-local.sh --only build,pg-check` (core regression must stay green); `… --only regress -k "test_chelabase_realtime_role or test_closed_database_connect or test_chelabase_keys"`.

**Live runs:** none.

---

## T5: neon extension: member-role statements, born closed, REASSIGN database owners (opus / opus)

**Setup (coordinator):** Neon clone `-t5`, branch `round-3-t5` from `round-3`, suffix `t5` (no Postgres change).

**Files:** `pgxn/neon/neon_ddl_handler.c`; tests (coordinator): `test_runner/regress/test_member_role_guard.py`, `test_runner/regress/test_born_closed_database.py`, an added test in `test_runner/regress/test_ddl_forwarding.py`.

**Behaviour:** spec §4.1 (D3a, S9, RT3-13), §6's neon-extension rules (D5, U1, U2, RT3-4, RT3-5) and S12, with the pinned messages and marker. Compiles on v14–v17.

**RED tests (coordinator).** A shared fixture in each new file (the DDL mock accepts every forward, as `test_ddl_protected_database.py:30-60` does):
```python
@pytest.fixture
def neon_pg(httpserver: HTTPServer, vanilla_pg: VanillaPostgres, httpserver_listen_address: ListenAddress):
    (host, port) = httpserver_listen_address
    endpoint = "/test/roles_and_databases"
    httpserver.expect_request(endpoint, method="PATCH").respond_with_handler(lambda request: Response(status=200))
    vanilla_pg.configure([f"neon.console_url=http://{host}:{port}{endpoint}", "shared_preload_libraries = 'neon'"])
    vanilla_pg.start()
    vanilla_pg.safe_psql("CREATE ROLE neon_superuser NOLOGIN CREATEDB CREATEROLE")
    return vanilla_pg
```
(imports as `test_ddl_protected_database.py:1-12`, plus `psycopg2`, `pytest`, `InsufficientPrivilege`.)

`test_runner/regress/test_member_role_guard.py` (fixture above, then):
```python
MEMBER = "cb_m_0123456789abcdef0123456789abcdef"


def setup_roles(pg: VanillaPostgres):
    pg.safe_psql(f"CREATE ROLE {MEMBER} LOGIN PASSWORD 'pw'")
    pg.safe_psql("CREATE ROLE grp NOLOGIN")
    pg.safe_psql("CREATE ROLE chelabase_realtime LOGIN REPLICATION")
    pg.safe_psql("CREATE ROLE owner LOGIN CREATEROLE PASSWORD 'pw' IN ROLE neon_superuser")
    pg.safe_psql("GRANT grp TO owner WITH ADMIN OPTION")
    # Postgres alone would let owner (CREATEROLE + ADMIN) change the member role: the refusals
    # below must come from the hook.
    pg.safe_psql(f"GRANT {MEMBER} TO owner WITH ADMIN OPTION")


def refused(pg: VanillaPostgres, user: str, sql: str, name: str):
    with pytest.raises(InsufficientPrivilege) as e:
        with pg.cursor(user=user, password="pw") as cur:
            cur.execute(sql)
    assert f'member role "{name}" is managed by Chelabase' in str(e.value), sql


AS_OWNER = [
    (f"ALTER ROLE {MEMBER} PASSWORD 'x'", MEMBER),
    (f"ALTER ROLE {MEMBER} SET work_mem = '1MB'", MEMBER),
    (f"ALTER ROLE {MEMBER} IN DATABASE postgres SET work_mem = '1MB'", MEMBER),
    (f"ALTER ROLE {MEMBER} RENAME TO not_a_member", MEMBER),
    ("ALTER ROLE grp RENAME TO cb_m_ffffffffffffffffffffffffffffffff", "cb_m_ffffffffffffffffffffffffffffffff"),
    (f"DROP ROLE {MEMBER}", MEMBER),
    ("CREATE ROLE cb_m_ffffffffffffffffffffffffffffffff", "cb_m_ffffffffffffffffffffffffffffffff"),
    ('CREATE ROLE "CB_M_X"', "CB_M_X"),
    (f"CREATE ROLE g2 ROLE {MEMBER}", MEMBER),
    (f"CREATE ROLE g3 ADMIN {MEMBER}", MEMBER),
    (f"GRANT grp TO {MEMBER}", MEMBER),
    (f"REVOKE grp FROM {MEMBER}", MEMBER),
    (f"ALTER GROUP grp ADD USER {MEMBER}", MEMBER),
    ("ALTER ROLE chelabase_realtime NOLOGIN", "chelabase_realtime"),
    ("DROP ROLE chelabase_realtime", "chelabase_realtime"),
    ("GRANT grp TO chelabase_realtime", "chelabase_realtime"),
    ("CREATE ROLE chelabase_realtime", "chelabase_realtime"),
    (f"DO $$BEGIN EXECUTE 'ALTER ROLE {MEMBER} PASSWORD ''x'''; END$$", MEMBER),
]
AS_MEMBER = [
    "ALTER ROLE CURRENT_USER PASSWORD 'x'",
    "ALTER ROLE CURRENT_ROLE PASSWORD 'x'",
    "ALTER USER CURRENT_USER PASSWORD 'y'",
    "ALTER ROLE SESSION_USER SET work_mem = '1MB'",
    "ALTER ROLE CURRENT_USER SET role = 'grp'",
]


def test_only_a_superuser_changes_member_roles(neon_pg: VanillaPostgres):
    setup_roles(neon_pg)
    for sql, name in AS_OWNER:
        refused(neon_pg, "owner", sql, name)
    for sql in AS_MEMBER:
        refused(neon_pg, MEMBER, sql, MEMBER)
    # From PL/pgSQL (SPI) too.
    neon_pg.safe_psql(
        "CREATE FUNCTION set_member_pw() RETURNS void LANGUAGE plpgsql AS "
        f"$$BEGIN EXECUTE 'ALTER ROLE {MEMBER} PASSWORD ''z'''; END$$"
    )
    neon_pg.safe_psql("GRANT EXECUTE ON FUNCTION set_member_pw() TO owner")
    refused(neon_pg, "owner", "SELECT set_member_pw()", MEMBER)
    # A superuser may.
    for sql in [
        f"ALTER ROLE {MEMBER} PASSWORD 'pw2'",
        f"ALTER ROLE {MEMBER} SET work_mem = '1MB'",
        f"GRANT grp TO {MEMBER}",
        f"REVOKE grp FROM {MEMBER}",
        "ALTER ROLE chelabase_realtime NOLOGIN",
    ]:
        neon_pg.safe_psql(sql)
    # Ordinary roles are untouched.
    with neon_pg.cursor(user="owner", password="pw") as cur:
        cur.execute("CREATE ROLE ordinary; ALTER ROLE ordinary PASSWORD 'x'; DROP ROLE ordinary")
```
`test_runner/regress/test_born_closed_database.py` (fixture above, then):
```python
GUARD = (
    "CREATE FUNCTION guard_fn() RETURNS event_trigger LANGUAGE plpgsql AS $$BEGIN END$$;"
    "CREATE EVENT TRIGGER chelabase_schema_guard_ddl ON ddl_command_end EXECUTE FUNCTION guard_fn();"
    "ALTER EVENT TRIGGER chelabase_schema_guard_ddl ENABLE ALWAYS;"
)
REFUSAL = "on a protected branch, ALLOW_CONNECTIONS and IS_TEMPLATE are set by Chelabase"


def as_owner(pg: VanillaPostgres, sql: str, dbname: str = "postgres"):
    with pg.cursor(user="owner", password="pw", dbname=dbname) as cur:
        cur.execute(sql)


def owner_refused(pg: VanillaPostgres, sql: str, needle: str = REFUSAL, dbname: str = "postgres"):
    with pytest.raises(psycopg2.Error) as e:
        as_owner(pg, sql, dbname)
    assert needle in str(e.value), (sql, str(e.value))


def state(pg: VanillaPostgres, db: str) -> tuple[bool, list[str] | None]:
    allow = pg.safe_psql(f"SELECT datallowconn FROM pg_database WHERE datname = '{db}'")[0][0]
    marker = pg.safe_psql(
        "SELECT s.setconfig FROM pg_db_role_setting s JOIN pg_database d ON d.oid = s.setdatabase "
        f"WHERE d.datname = '{db}' AND s.setrole = 0"
    )
    return allow, (marker[0][0] if marker else None)


def setup_owner(pg: VanillaPostgres):
    pg.safe_psql("CREATE ROLE owner LOGIN CREATEDB PASSWORD 'pw' IN ROLE neon_superuser")


def test_born_closed_on_a_guarded_database(neon_pg: VanillaPostgres):
    setup_owner(neon_pg)
    neon_pg.safe_psql(GUARD)
    as_owner(neon_pg, "CREATE DATABASE b1 TEMPLATE template0")
    as_owner(neon_pg, "CREATE DATABASE b2")
    for db in ["b1", "b2"]:
        assert state(neon_pg, db) == (False, ["neon.born_closed=on"]), db
    for sql in [
        "CREATE DATABASE b3 WITH ALLOW_CONNECTIONS true",
        "CREATE DATABASE b4 WITH ALLOW_CONNECTIONS false",
        "CREATE DATABASE b5 WITH IS_TEMPLATE true",
        "ALTER DATABASE b1 ALLOW_CONNECTIONS true",
        "ALTER DATABASE b1 IS_TEMPLATE true",
    ]:
        owner_refused(neon_pg, sql)
    owner_refused(neon_pg, "ALTER DATABASE b1 RESET neon.born_closed", "permission denied")
    as_owner(neon_pg, "ALTER DATABASE b1 RESET ALL")
    assert state(neon_pg, "b1") == (False, ["neon.born_closed=on"]), "RESET ALL keeps the marker"
    assert not neon_pg.safe_psql("SELECT has_table_privilege('owner', 'pg_catalog.pg_database', 'UPDATE')")[0][0]
    owner_refused(neon_pg, "UPDATE pg_catalog.pg_database SET datallowconn = true WHERE datname = 'b1'", "permission denied")
    # A superuser is untouched.
    neon_pg.safe_psql("CREATE DATABASE s1")
    assert state(neon_pg, "s1") == (True, None)
    neon_pg.safe_psql("ALTER DATABASE b2 ALLOW_CONNECTIONS true")
    assert state(neon_pg, "b2")[0] is True


def test_the_targets_marker_decides_from_an_unguarded_database(neon_pg: VanillaPostgres):
    setup_owner(neon_pg)
    neon_pg.safe_psql(GUARD)
    as_owner(neon_pg, "CREATE DATABASE b1")
    neon_pg.safe_psql("CREATE DATABASE plain")
    neon_pg.safe_psql("GRANT CONNECT ON DATABASE plain TO owner")
    for sql in ["ALTER DATABASE b1 ALLOW_CONNECTIONS true", "ALTER DATABASE b1 IS_TEMPLATE true"]:
        owner_refused(neon_pg, sql, dbname="plain")
    # U2: an unmarked closed database is the owner's to reopen only outside a guarded session.
    neon_pg.safe_psql("CREATE DATABASE shut OWNER owner")
    neon_pg.safe_psql("ALTER DATABASE shut ALLOW_CONNECTIONS false")
    owner_refused(neon_pg, "ALTER DATABASE shut ALLOW_CONNECTIONS true")
    as_owner(neon_pg, "ALTER DATABASE shut ALLOW_CONNECTIONS true", dbname="plain")
    assert state(neon_pg, "shut") == (True, None)


def test_no_guard_no_change(neon_pg: VanillaPostgres):
    setup_owner(neon_pg)
    # Not ENABLE ALWAYS: not the guard.
    neon_pg.safe_psql(
        "CREATE FUNCTION g0() RETURNS event_trigger LANGUAGE plpgsql AS $$BEGIN END$$;"
        "CREATE EVENT TRIGGER chelabase_schema_guard_ddl ON ddl_command_end EXECUTE FUNCTION g0();"
    )
    as_owner(neon_pg, "CREATE DATABASE n1")
    assert state(neon_pg, "n1") == (True, None)
    neon_pg.safe_psql("DROP EVENT TRIGGER chelabase_schema_guard_ddl")
    # Owned by a non-superuser: not the guard.
    as_owner(
        neon_pg,
        "CREATE FUNCTION g1() RETURNS event_trigger LANGUAGE plpgsql AS $$BEGIN END$$;"
        "CREATE EVENT TRIGGER chelabase_schema_guard_ddl ON ddl_command_end EXECUTE FUNCTION g1();"
        "ALTER EVENT TRIGGER chelabase_schema_guard_ddl ENABLE ALWAYS;",
    )
    as_owner(neon_pg, "CREATE DATABASE n2")
    assert state(neon_pg, "n2") == (True, None)
    as_owner(neon_pg, "ALTER DATABASE n2 ALLOW_CONNECTIONS false")
    as_owner(neon_pg, "ALTER DATABASE n2 ALLOW_CONNECTIONS true")
```
Added to `test_runner/regress/test_ddl_forwarding.py` (uses its `ddl` fixture, `:124-130`):
```python
def test_reassign_owned_reports_database_owner(ddl: DdlForwardingContext):
    ddl.send_and_wait("CREATE ROLE ra WITH PASSWORD 'a'")
    ddl.send_and_wait("CREATE ROLE rb WITH PASSWORD 'b'")
    ddl.send_and_wait("CREATE DATABASE rdb OWNER ra")
    assert ddl.dbs == {"rdb": "ra"}
    ddl.send_and_wait("REASSIGN OWNED BY ra TO rb")
    assert ddl.dbs == {"rdb": "rb"}
    ddl.send_and_wait("DROP DATABASE rdb")
    ddl.send_and_wait("DROP ROLE ra")
    ddl.send_and_wait("DROP ROLE rb")
    assert ddl.dbs == {} and ddl.roles == {}
```
RED: each refusal is accepted today; databases are born open; `REASSIGN OWNED` sends no owner. Assumptions checked: the neon hook sees nested statements (`ProcessUtility_hook`); `ProcessCreateEventTrigger` leaves a non-superuser's event trigger owned by its creator (`neon_ddl_handler.c:1675-1690`) and a superuser's owned by the superuser; `owner` is `IN ROLE neon_superuser` so it may create event triggers (`:1605`); `ALTER DATABASE … RESET <SUSET name>` by a non-superuser fails with "permission denied to set parameter" once the GUC is defined; `GUCArrayReset` keeps entries the user may not reset; `REASSIGN OWNED` reassigns databases (`pg_shdepend.c:1714-1719`).

**Gotchas:** born closed appends `allow_connections false` to a `copyObject` of the statement (the tree may be read-only); the marker write happens after `standard_ProcessUtility` in the same transaction, as superuser; the `ALTER DATABASE` target's marker is read from `pg_db_role_setting` (shared) by database OID; every `RoleSpec` kind is resolved (`get_rolespec_name` handles `CURRENT_ROLE`, `CURRENT_USER`, `SESSION_USER`).

**Verify:** `CHELA_NEON_VOLUME_SUFFIX=t5 .github/scripts/ci-local.sh --only build`; `… --only regress -k "test_member_role_guard or test_born_closed_database or test_ddl_forwarding or test_ddl_protected_database or test_neon_superuser or test_event_trigger"`.

**Live runs:** none.

---

## T6: compute_ctl: member logins, TLS, migrations before `running` (opus / opus)

**Setup (coordinator):** Neon clone `-t6`, branch `round-3-t6` from `round-3`, suffix `t6`.

**Files:** `libs/compute_api/src/{spec.rs,responses.rs}`, `compute_tools/src/{spec.rs,config.rs,tls.rs,compute.rs,migration.rs}`, `control_plane/src/endpoint.rs` (`member_logins: None` in its `ComputeSpec` literal); tests (coordinator): `compute_tools/tests/chelabase_round3.rs`, `test_runner/regress/test_compute_migrations.py` (changed and added tests).

**Behaviour:** spec §4.2 (compute_ctl block, ident, `ssl_ca_file`, PKCS#8, RT3-1 reload, RT3-12), §7 (D6, U3, RT3-14); pinned interfaces. `update_pg_hba` puts the block at the top (once: a block already present isn't added again) and keeps today's `md5` line last. The cert watcher (`compute.rs:2115-2166`) calls `reload_tls_files(pgdata, tls, || self.pg_reload_conf())` and never sets the status. `apply_config` runs the migrations inline (`Handle::block_on`) on a connection with `MIGRATION_CONN_OPTIONS`; a failure for which `fatal_failure` is true fails the start with `migration <id> failed: <error>`.

**RED tests (coordinator).** `compute_tools/tests/chelabase_round3.rs`:
```rust
//! Round 3: the member pg_hba block and pg_ident lines, TLS lines and keys, migration kinds.

use std::path::Path;

use compute_api::responses::TlsConfig;
use compute_api::spec::MemberLogins;
use compute_tools::config::tls_conf_lines;
use compute_tools::migration::{MIGRATION_CONN_OPTIONS, MigrationKind, fatal_failure};
use compute_tools::spec::{member_hba_block, member_ident_lines, migration_kinds, update_pg_hba};
use compute_tools::tls::{reload_tls_files, verify_key_cert};

const HEAD: &str = "# chelabase: member logins (cb_m_*) only from the orchestrator\n";
const REJECT: &str = "host\tall\t/^[Cc][Bb]_[Mm]_\tall\treject\n\
host\treplication\t/^[Cc][Bb]_[Mm]_\tall\treject\n\
local\tall\t/^[Cc][Bb]_[Mm]_\treject\n\
local\treplication\t/^[Cc][Bb]_[Mm]_\treject\n";
const TAIL: &str = "# chelabase: end of member logins\n";

fn allow(cidr: &str) -> String {
    format!("hostssl\tall\t/^cb_m_[0-9a-f]{{32}}$\t{cidr}\tscram-sha-256\tclientcert=verify-full\tmap=chelabase_member\n")
}

fn logins(cidrs: &[&str], cns: &[&str]) -> MemberLogins {
    MemberLogins {
        allowed_cidrs: cidrs.iter().map(|s| s.to_string()).collect(),
        client_cns: cns.iter().map(|s| s.to_string()).collect(),
    }
}

#[test]
fn the_block_allows_each_cidr_then_rejects() {
    let ml = logins(&["10.213.4.10/32", "10.213.4.11/32"], &["chelabase-orchestrator"]);
    assert_eq!(
        member_hba_block(Some(&ml), true).unwrap(),
        format!("{HEAD}{}{}{REJECT}{TAIL}", allow("10.213.4.10/32"), allow("10.213.4.11/32"))
    );
    let empty = logins(&[], &["chelabase-orchestrator"]);
    assert_eq!(member_hba_block(Some(&empty), true).unwrap(), format!("{HEAD}{}{REJECT}{TAIL}", allow("all")));
    assert_eq!(member_hba_block(None, true).unwrap(), format!("{HEAD}{REJECT}{TAIL}"), "no spec: refused");
    assert_eq!(member_hba_block(Some(&ml), false).unwrap(), format!("{HEAD}{REJECT}{TAIL}"), "no TLS: refused");
    assert!(member_hba_block(Some(&logins(&["fd00::10/128"], &["chelabase-orchestrator"])), true).unwrap().contains("\tfd00::10/128\t"));
}

#[test]
fn bad_cidrs_and_cns_fail() {
    for cidr in ["x", "10.0.0.1", "10.0.0.1/33", "10.0.0.1/32 trust", "all"] {
        assert!(member_hba_block(Some(&logins(&[cidr], &["chelabase-orchestrator"])), true).is_err(), "{cidr}");
    }
    for cns in [vec![], vec!["a b"], vec!["a#b"], vec![""], vec!["-x"], vec!["x/y"]] {
        assert!(member_ident_lines(Some(&logins(&[], &cns))).is_err(), "{cns:?}");
    }
}

#[test]
fn ident_lines_map_each_cn_to_the_member_roles() {
    let ml = logins(&[], &["chelabase-orchestrator", "chelabase-realtime"]);
    assert_eq!(
        member_ident_lines(Some(&ml)).unwrap(),
        "chelabase_member\tchelabase-orchestrator\t/^cb_m_[0-9a-f]{32}$\n\
chelabase_member\tchelabase-realtime\t/^cb_m_[0-9a-f]{32}$\n"
    );
    assert_eq!(member_ident_lines(None).unwrap(), "");
}

#[test]
fn update_pg_hba_puts_the_block_first_and_md5_last() {
    let dir = Path::new("./tests/tmp/round3_hba");
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir).unwrap();
    let sample = "local   all             all                                     trust\n\
host    all             all             127.0.0.1/32            trust\n";
    std::fs::write(dir.join("pg_hba.conf"), sample).unwrap();
    let ml = logins(&["10.213.4.10/32"], &["chelabase-orchestrator"]);
    update_pg_hba(dir, None, Some(&ml), true).unwrap();
    update_pg_hba(dir, None, Some(&ml), true).unwrap();
    let text = std::fs::read_to_string(dir.join("pg_hba.conf")).unwrap();
    let block = member_hba_block(Some(&ml), true).unwrap();
    assert!(text.starts_with(&block), "{text}");
    assert_eq!(text.matches(HEAD.trim_end()).count(), 1, "once");
    assert!(text[block.len()..].contains("127.0.0.1/32            trust"));
    assert!(text.trim_end().ends_with("host\tall\t\tall\t\tall\t\tmd5"), "{text}");
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn tls_lines_carry_the_ca_whenever_tls_is_on() {
    let tls = |ca: Option<&str>| TlsConfig {
        key_path: "/var/db/chelabase/tls/server.key".into(),
        cert_path: "/var/db/chelabase/tls/server.crt".into(),
        ca_path: ca.map(str::to_string),
    };
    assert_eq!(
        tls_conf_lines(&tls(Some("/var/db/chelabase/tls/ca.crt"))),
        "ssl = on\nssl_cert_file = 'server.crt'\nssl_key_file = 'server.key'\nssl_ca_file = '/var/db/chelabase/tls/ca.crt'\n"
    );
    assert_eq!(tls_conf_lines(&tls(None)), "ssl = on\nssl_cert_file = 'server.crt'\nssl_key_file = 'server.key'\n");
}

fn openssl_pair(dir: &Path) -> (String, String, String) {
    let run = |args: &[&str]| {
        let out = std::process::Command::new("openssl").args(args).current_dir(dir).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    };
    run(&["req", "-x509", "-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:P-256", "-nodes", "-keyout", "k.pem", "-out", "c.pem", "-days", "1", "-subj", "/CN=t"]);
    run(&["ec", "-in", "k.pem", "-out", "k_sec1.pem"]);
    let read = |f: &str| std::fs::read_to_string(dir.join(f)).unwrap();
    (read("k.pem"), read("k_sec1.pem"), read("c.pem"))
}

#[test]
fn both_key_forms_are_read_and_a_wrong_key_is_not() {
    let dir = Path::new("./tests/tmp/round3_keys");
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir).unwrap();
    let (pkcs8, sec1, cert) = openssl_pair(dir);
    assert!(pkcs8.starts_with("-----BEGIN PRIVATE KEY-----"));
    verify_key_cert(&pkcs8, &cert).unwrap();
    verify_key_cert(&sec1, &cert).unwrap();
    std::fs::remove_dir_all(dir).unwrap();
    std::fs::create_dir_all(dir).unwrap();
    let (other, _, _) = openssl_pair(dir);
    assert!(verify_key_cert(&other, &cert).is_err());
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn a_certificate_change_rewrites_the_files_and_reloads_once() {
    let dir = Path::new("./tests/tmp/round3_reload");
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir.join("pgdata")).unwrap();
    let (key, _, cert) = openssl_pair(dir);
    std::fs::write(dir.join("server.key"), &key).unwrap();
    std::fs::write(dir.join("server.crt"), &cert).unwrap();
    let tls = TlsConfig {
        key_path: dir.join("server.key").to_string_lossy().into(),
        cert_path: dir.join("server.crt").to_string_lossy().into(),
        ca_path: None,
    };
    let mut reloads = 0;
    reload_tls_files(&dir.join("pgdata"), &tls, || {
        reloads += 1;
        Ok(())
    })
    .unwrap();
    assert_eq!(reloads, 1);
    assert_eq!(std::fs::read_to_string(dir.join("pgdata/server.crt")).unwrap(), cert);
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(dir.join("pgdata/server.key")).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn migration_kinds_follow_the_files() {
    use MigrationKind::*;
    let kinds = migration_kinds();
    assert_eq!(kinds, vec![FailClosed, FailOpen, FailClosed, FailClosed, Skip, Skip, Skip, Skip, Skip, FailClosed, FailClosed, FailClosed]);
    let files = [
        include_str!("../src/migrations/0001-add_bypass_rls_to_privileged_role.sql"),
        include_str!("../src/migrations/0002-alter_roles.sql"),
        include_str!("../src/migrations/0003-grant_pg_create_subscription_to_privileged_role.sql"),
        include_str!("../src/migrations/0004-grant_pg_monitor_to_privileged_role.sql"),
        include_str!("../src/migrations/0005-grant_all_on_tables_to_privileged_role.sql"),
        include_str!("../src/migrations/0006-grant_all_on_sequences_to_privileged_role.sql"),
        include_str!("../src/migrations/0007-grant_all_on_tables_with_grant_option_to_privileged_role.sql"),
        include_str!("../src/migrations/0008-grant_all_on_sequences_with_grant_option_to_privileged_role.sql"),
        include_str!("../src/migrations/0009-revoke_replication_for_previously_allowed_roles.sql"),
        include_str!("../src/migrations/0010-grant_snapshot_synchronization_funcs_to_privileged_role.sql"),
        include_str!("../src/migrations/0011-grant_pg_show_replication_origin_status_to_privileged_role.sql"),
        include_str!("../src/migrations/0012-grant_pg_signal_backend_to_privileged_role.sql"),
    ];
    for (i, (file, kind)) in files.iter().zip(&kinds).enumerate() {
        assert_eq!(file.starts_with("-- SKIP"), *kind == Skip, "migration {}", i + 1);
    }
}

#[test]
fn only_a_failure_that_leaves_a_fail_open_migration_unapplied_is_fatal() {
    let kinds = migration_kinds();
    assert!(fatal_failure(&kinds, 0), "0001 fails: 0002 stays unapplied");
    assert!(fatal_failure(&kinds, 1), "0002 itself");
    for i in 2..kinds.len() {
        assert!(!fatal_failure(&kinds, i), "migration {}", i + 1);
    }
    use MigrationKind::*;
    assert!(!fatal_failure(&[FailClosed, Skip], 0));
    assert!(fatal_failure(&[Skip, FailClosed, FailOpen], 0));
    assert_eq!(MIGRATION_CONN_OPTIONS, "-c lock_timeout=5s -c statement_timeout=2min");
}
```
`test_runner/regress/test_compute_migrations.py`: `test_compute_migrations_retry` is replaced by:
```python
def test_compute_migrations_retry(neon_simple_env: NeonEnv, compute_migrations_dir: Path):
    """
    U3: a failure that leaves a fail-open migration unapplied (0001, 0002) fails the start;
    a later one (fail-closed or skip) is logged and the compute starts. Either way the next
    start applies the rest.
    """
    env = neon_simple_env
    endpoint = env.endpoints.create("main")
    endpoint.respec(skip_pg_catalog_updates=False)

    for i in range(1, NUM_COMPUTE_MIGRATIONS + 1):
        failpoint = {"FAILPOINTS": f"compute-migration=return({i})"}
        if i <= 2:
            with pytest.raises(Exception, match="compute startup failed"):
                endpoint.start(env=failpoint)
            assert endpoint.log_contains(f"migration {i} was configured to fail")
            env.neon_cli.endpoint_stop(endpoint.endpoint_id, check_return_code=False, mode="immediate")
            continue
        endpoint.start(env=failpoint)
        endpoint.wait_for_migrations(wait_for=i - 1)
        with endpoint.cursor() as cur:
            cur.execute("SELECT id FROM neon_migration.migration_id")
            assert cast("int", cur.fetchall()[0][0]) == i - 1
        endpoint.stop()

    endpoint.start()
    with endpoint.cursor() as cur:
        cur.execute("SELECT id FROM neon_migration.migration_id")
        assert cast("int", cur.fetchall()[0][0]) == NUM_COMPUTE_MIGRATIONS
```
and added:
```python
def test_migrations_done_when_running(neon_simple_env: NeonEnv):
    env = neon_simple_env
    endpoint = env.endpoints.create("main")
    endpoint.respec(skip_pg_catalog_updates=False)
    endpoint.start()
    with endpoint.cursor() as cur:
        cur.execute("SELECT id FROM neon_migration.migration_id")
        assert cast("int", cur.fetchall()[0][0]) == NUM_COMPUTE_MIGRATIONS


def test_fail_open_migration_failure_fails_the_start(neon_simple_env: NeonEnv):
    env = neon_simple_env
    endpoint = env.endpoints.create("main")
    endpoint.respec(skip_pg_catalog_updates=False)
    with pytest.raises(Exception, match="migration 2"):
        endpoint.start(env={"FAILPOINTS": "compute-migration=return(2)"})
    env.neon_cli.endpoint_stop(endpoint.endpoint_id, check_return_code=False, mode="immediate")
    endpoint.start()
    with endpoint.cursor() as cur:
        cur.execute("SELECT id FROM neon_migration.migration_id")
        assert cast("int", cur.fetchall()[0][0]) == NUM_COMPUTE_MIGRATIONS


def test_fail_closed_migration_failure_starts(neon_simple_env: NeonEnv):
    env = neon_simple_env
    endpoint = env.endpoints.create("main")
    endpoint.respec(skip_pg_catalog_updates=False)
    endpoint.start(env={"FAILPOINTS": f"compute-migration=return({NUM_COMPUTE_MIGRATIONS})"})
    with endpoint.cursor() as cur:
        cur.execute("SELECT id FROM neon_migration.migration_id")
        assert cast("int", cur.fetchall()[0][0]) == NUM_COMPUTE_MIGRATIONS - 1
```
RED: the Rust file doesn't compile (the items under test are missing); the Python tests fail (migrations run after `running`, so `migration_id` is still 0 right after `start()`; failures never fail the start). Assumptions checked: `NUM_COMPUTE_MIGRATIONS` and `cast`, `pytest` are imported by the file (`test_compute_migrations.py:1-14`; `pytest` is); neon_local reports a `Failed` compute as `compute startup failed: <error>` (`control_plane/src/endpoint.rs:956-962`); the failpoint message (`compute_tools/src/migration.rs:57-59`); `compute_tools/tests/tmp` is the tests' scratch directory (`compute_tools/tests/config_test.rs:25`); `openssl` is in the build-tools image (Neon's fixtures call it, `neon_fixtures.py:3504-3521`); the last migration (0012) is fail-closed.

**Gotchas:** the `testing` feature gates the failpoint (`migration.rs:47`); keep `handle_migrations`' order (the comment at `spec.rs:250-252`); `reload_tls_files` reuses `try_update_key_path_blocking`'s writes (`tls.rs:63-87`); `write_postgres_conf` uses `tls_conf_lines`.

**Verify:** `CHELA_NEON_VOLUME_SUFFIX=t6 .github/scripts/ci-local.sh --only build,rust-tests --test-filter 'package(compute_tools) or package(compute_api)'`; `… --only regress -k "test_compute_migrations"`.

**Live runs:** none.

---

## T7: fork integration (sonnet / none)

**Checkout:** `<neon checkout>` on `round-3`; its `vendor/postgres-v17` on `round-3-v17`.

**Steps:**
1. In `vendor/postgres-v17`: `git fetch origin`; merge `--no-ff` `origin/round-3-v17-t2`, `-t3` (contains `-t2`), `-t4` into `round-3-v17` (as <maintainer>, trailers); push.
2. In the Neon repo: merge `--no-ff` `origin/round-3-t2`, `-t3`, `-t4`, `-t5`, `-t6` (their Python tests and compute_ctl changes; T2/T3/T4's submodule pointers are superseded); set the submodule to `round-3-v17`'s head; `vendor/revisions.json` `"v17": ["17.11", "<that sha>"]`; `postgres.mk` installs `contrib/chelabase_keys` next to `test_decoding` (`:118`) for v17 only; commit `vendor: postgres v17 round 3; install chelabase_keys for tests`; push.
3. Remove the task clones and their volumes (`docker volume rm chela-neon-cargo-tN chela-neon-target-tN`) once pushed.

**RED tests (coordinator):** appended to `.github/scripts/ci-local_test.sh`:
```bash
# RT3-6: chelabase_keys is installed into pg_install for v17 only.
mk="$root/postgres.mk"
if make -n -f "$mk" ROOT_PROJECT_DIR="$root" BUILD_DIR=/x POSTGRES_INSTALL_DIR=/y postgres-install-v17 2>/dev/null | grep -q 'contrib/chelabase_keys install'; then
    echo "ok: v17 installs chelabase_keys"
else
    echo "FAIL: v17 must install chelabase_keys"; failures=$((failures + 1))
fi
for v in v14 v15 v16; do
    if make -n -f "$mk" ROOT_PROJECT_DIR="$root" BUILD_DIR=/x POSTGRES_INSTALL_DIR=/y "postgres-install-$v" 2>/dev/null | grep -q 'chelabase_keys'; then
        echo "FAIL: $v must not install chelabase_keys"; failures=$((failures + 1))
    else
        echo "ok: $v skips chelabase_keys"
    fi
done
expect "revisions.json pins the submodule" "$(git -C "$root" rev-parse HEAD:vendor/postgres-v17)" "$(jq -r '.v17[1]' "$root/vendor/revisions.json")"
```
RED: the v17 check fails. Assumption to confirm at RED time: `make -n` on `postgres.mk` alone resolves the `postgres-install-%` recipe with those variables (`postgres.mk:107-118`); if it needs the top `Makefile`'s variables, the coordinator runs it through `make -n -C "$root" postgres-install-v17` instead and records the change (P2-style test fix).

**Verify:** `bash .github/scripts/ci-local_test.sh`. The heavy runs are End of step 4's.

**Live runs:** none.

---

## T9: worker (sonnet / opus)

**Worktree:** `just wt-add neon-fork-round-3-t9 neon-fork-round-3` (after T8 merged into the branch).

**Files:** `services/worker/src/{config.rs,lib.rs,provision.rs,ports.rs,fakes.rs,member_roles.rs,main.rs}`, a new `services/worker/src/certs.rs` (the sweep); tests (coordinator): additions to `services/worker/src/provision.rs`'s and `services/worker/src/member_roles/tests.rs`' test modules and `services/worker/src/config.rs`' tests; live: new `services/worker/tests/compute_fork_live.rs`, additions to `services/worker/tests/member_roles_live.rs` (including the `direct`/`direct_in` helpers over TLS).

**Behaviour:** spec §6 (Chelabase), §7 (worker), §4.2 (certificate sweep for computes and the orchestrator pair, RT3-7; the worker writes `<keys>/orchestrator-tls` with `write_client_pair`); the pinned interfaces. `wait_until_ready`: ready when `status` is `Running` and `SELECT 1` answers; `Failed(e)` ends the wait at once with `e`; a status error keeps polling. Reconcile: `member_catalog_sql` gains the `c` lines; on a run, `plan_closed_databases` steps run after the guard loop (a `Guard` step joins the guard databases; `Open` runs only after that database's install succeeded); a run requested by the DDL receiver that finds no closed database retries once after `MEMBER_ROLES_ENQUEUE_DELAY` (RT3-11). `renew_certs_once(now)` daily: each running compute whose `server.crt` (`read_file(SERVER_CERT_PATH)`) needs renewal gets a new one uploaded; the orchestrator pair is reissued under 60 days. The doc comment at `main.rs:676` says every ready normal branch.

**RED tests (coordinator).** Added to `provision.rs`' test module:
```rust
    // D6: ready means compute_ctl says running AND the database answers.
    #[tokio::test]
    async fn ready_needs_running_as_well_as_select_1() {
        let (mut w, _journal) = worker();
        w.ready_timeout = Duration::from_millis(60);
        w.computes.create("ep-s", "t").await.unwrap();
        w.pusher.set_status("ep-s", chelabase_neon::reconfigure::ComputeCtlStatus::Other("init".into()));
        let err = w.wait_until_ready("ep-s", "app").await.unwrap_err();
        assert!(err.contains("init"), "{err}");
        w.pusher.set_status("ep-s", chelabase_neon::reconfigure::ComputeCtlStatus::Running);
        w.wait_until_ready("ep-s", "app").await.unwrap();
    }

    #[tokio::test]
    async fn a_failed_compute_ctl_ends_the_wait_at_once() {
        let (mut w, _journal) = worker();
        w.ready_timeout = Duration::from_secs(5);
        w.computes.create("ep-f", "t").await.unwrap();
        w.pusher.set_status(
            "ep-f",
            chelabase_neon::reconfigure::ComputeCtlStatus::Failed("migration 2 failed: boom".into()),
        );
        let started = Instant::now();
        let err = w.wait_until_ready("ep-f", "app").await.unwrap_err();
        assert!(err.contains("migration 2 failed: boom"), "{err}");
        assert!(started.elapsed() < Duration::from_millis(500), "{:?}", started.elapsed());
    }
```
Added to `member_roles/tests.rs`:
```rust
fn closed(name: &str, allow: bool, template: bool, born: bool) -> ClosedDatabase {
    ClosedDatabase { name: name.into(), allow_connections: allow, template, born_closed: born }
}

#[test]
fn the_catalog_lists_closed_and_marked_databases() {
    let sql = member_catalog_sql();
    assert!(sql.contains("neon.born_closed=on"), "{sql}");
    assert!(sql.contains("NOT d.datallowconn"), "{sql}");
    let h = |s: &str| hex::encode(s.as_bytes());
    let out = format!("c {} f f t\nc {} f f f\nc {} t f t\nc {} f t f\n", h("born"), h("shut"), h("opened"), h("template0"));
    let catalog = parse_member_catalog(&out).unwrap();
    assert_eq!(
        catalog.closed_databases,
        vec![
            closed("born", false, false, true),
            closed("opened", true, false, true),
            closed("shut", false, false, false),
            closed("template0", false, true, false),
        ]
    );
    assert!(parse_member_catalog(&format!("c {} f f", h("x"))).is_err(), "a short line");
}

#[test]
fn closed_databases_on_a_protected_branch_are_guarded_and_only_marked_ones_opened() {
    use ClosedStep::*;
    let dbs = [
        closed("born", false, false, true),
        closed("opened", true, false, true),
        closed("shut", false, false, false),
        closed("template0", false, true, false),
        closed("tmark", false, true, true),
    ];
    assert_eq!(
        plan_closed_databases(&dbs, true),
        vec![
            Guard("born".into()),
            Open("born".into()),
            ResetMarker("opened".into()),
            Guard("shut".into()),
            Skip("template0".into()),
            Skip("tmark".into()),
        ]
    );
    assert_eq!(
        plan_closed_databases(&dbs, false),
        vec![Open("born".into()), ResetMarker("opened".into()), Skip("tmark".into())]
    );
    assert!(plan_closed_databases(&[], true).is_empty());
}

#[test]
fn opening_resets_the_marker_with_quoted_names() {
    let sql = open_born_closed_sql("we\"ird");
    let open = sql.find("ALTER DATABASE \"we\"\"ird\" ALLOW_CONNECTIONS true;").expect(&sql);
    let reset = sql.find("ALTER DATABASE \"we\"\"ird\" RESET neon.born_closed;").expect(&sql);
    assert!(open < reset, "{sql}");
    assert_eq!(reset_born_closed_sql("db"), "ALTER DATABASE \"db\" RESET neon.born_closed;");
}
```
Added to `config.rs`' tests (its `lookup` helper and `IMAGE`, `config.rs:294-302`):
```rust
    #[test]
    fn config_reads_the_round_3_settings() {
        let d = WorkerConfig::from_lookup(lookup(&[IMAGE])).unwrap();
        assert_eq!(d.compute_control_network, "chelabase_control");
        assert_eq!(d.compute_ca_dir, PathBuf::from(".chelabase/compute-ca"));
        assert_eq!(d.compute_tls_dir, PathBuf::from(".chelabase/orchestrator-tls"));
        assert_eq!(d.member_login_cidrs, None, "unset: build() refuses to start");
        let c = WorkerConfig::from_lookup(lookup(&[
            IMAGE,
            ("CHELABASE_COMPUTE_CONTROL_NETWORK", "chelabase-feat-x_control"),
            ("CHELABASE_COMPUTE_CA_DIR", "/m/.chelabase/compute-ca"),
            ("CHELABASE_COMPUTE_TLS_DIR", "/m/.chelabase/orchestrator-tls"),
            ("CHELABASE_MEMBER_LOGIN_CIDRS", "10.213.4.10/32,10.213.4.11/32"),
        ]))
        .unwrap();
        assert_eq!(c.compute_control_network, "chelabase-feat-x_control");
        assert_eq!(c.compute_ca_dir, PathBuf::from("/m/.chelabase/compute-ca"));
        assert_eq!(c.compute_tls_dir, PathBuf::from("/m/.chelabase/orchestrator-tls"));
        assert_eq!(c.member_login_cidrs, Some(vec!["10.213.4.10/32".to_string(), "10.213.4.11/32".to_string()]));
        let empty = WorkerConfig::from_lookup(lookup(&[IMAGE, ("CHELABASE_MEMBER_LOGIN_CIDRS", "")])).unwrap();
        assert_eq!(empty.member_login_cidrs, Some(vec![]), "empty: no address rule");
        assert!(WorkerConfig::from_lookup(lookup(&[IMAGE, ("CHELABASE_MEMBER_LOGIN_CIDRS", "nope")])).is_err());
    }
```

`services/worker/tests/compute_fork_live.rs` (new):
```rust
//! Fork round 3 on a real compute: the keys plugin, the realtime restriction, migrations before
//! `running`, the compute's TLS files and networks, certificate renewal. Needs the round-3 images.
//!
//! ```text
//! just workbench test -p worker --test compute_fork_live -- --run-ignored only
//! ```

use std::future::Future;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use chelabase_metadata::{DEMO_PROJECT_ID, DEMO_TENANT_ID, DatabaseStatus, MetadataClient};
use chelabase_neon::compute::{ComputeLocator, container_name};
use chelabase_neon::reconfigure::ComputeCtlStatus;
use chelabase_neon::ComputeBackend;
use chelabase_types::{BranchId, DatabaseId};
use tokio::io::AsyncWriteExt;
use worker::config::RealWorker;
use worker::ports::SpecPusher;

mod common;
use common::{cleanup, setup};

struct Db {
    worker: RealWorker,
    metadata: MetadataClient,
    id: DatabaseId,
    ep: String,
    database: String,
    stack: String,
    control_network: String,
}

impl Db {
    async fn sql(&self, database: &str, sql: &str) -> String {
        self.worker
            .computes
            .admin_sql(&self.ep, database, sql)
            .await
            .unwrap_or_else(|e| panic!("admin_sql {sql:?}: {e:#}"))
    }

    fn container(&self) -> String {
        container_name(&self.stack, &self.ep)
    }

    async fn conninfo(&self, user: &str, password: &str, replication: bool) -> String {
        let addr = self.worker.computes.postgres_addr(&self.ep).await.unwrap().expect("address");
        format!(
            "host={} port={} dbname={} user={user} password={password} sslmode=prefer{}",
            addr.ip(),
            addr.port(),
            self.database,
            if replication { " replication=database" } else { "" }
        )
    }

    async fn psql(&self, conninfo: &str, script: &str) -> Result<String, String> {
        let mut child = tokio::process::Command::new("psql")
            .args([conninfo, "-X", "-qtA", "-v", "ON_ERROR_STOP=1", "-f", "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("psql");
        child.stdin.take().unwrap().write_all(script.as_bytes()).await.unwrap();
        let out = child.wait_with_output().await.unwrap();
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        } else {
            Err(String::from_utf8_lossy(&out.stderr).into_owned())
        }
    }

    async fn ready_again(&self) {
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            if self.worker.computes.admin_sql(&self.ep, "postgres", "SELECT 1").await.is_ok() {
                return;
            }
            assert!(Instant::now() < deadline, "the compute didn't come back");
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }

    async fn docker(&self, args: &[&str]) -> String {
        let out = tokio::process::Command::new("docker").args(args).output().await.expect("docker");
        assert!(out.status.success(), "docker {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    async fn ctl_status(&self) -> ComputeCtlStatus {
        let addr = self.worker.computes.control_addr(&self.ep).await.unwrap().expect("control address");
        self.worker.pusher.status(addr, &self.ep).await.expect("compute_ctl status")
    }
}

async fn with_db<F, Fut>(body: F)
where
    F: FnOnce(Arc<Db>) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let (worker, metadata, config) = setup().await;
    let id = DatabaseId::new();
    let name = format!("live-{}", &id.to_string()[..8]);
    metadata
        .insert_database_pending(id, DEMO_TENANT_ID.into(), DEMO_PROJECT_ID.into(), &name)
        .await
        .expect("insert pending database");
    let provisioned = tokio::time::timeout(Duration::from_secs(90), worker.handle_provision(id, &name)).await;
    let record = metadata.get_database(id).await.ok().flatten();
    let Some(record) = record.filter(|r| r.status == DatabaseStatus::Ready) else {
        cleanup(&worker, &metadata, id).await;
        panic!("provision failed: {provisioned:?}");
    };
    let branch = metadata.get_branch(BranchId::from(id)).await.unwrap().expect("main branch");
    let db = Arc::new(Db {
        ep: branch.endpoint_id.expect("endpoint"),
        database: record.database_name.expect("database name"),
        stack: config.stack.clone(),
        control_network: config.compute_control_network.clone(),
        worker,
        metadata,
        id,
    });
    let outcome = tokio::spawn(body(db.clone())).await;
    cleanup(&db.worker, &db.metadata, db.id).await;
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic.into_panic());
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the workbench stack on the round-3 images"]
async fn chelabase_keys_ships_and_decodes() {
    with_db(|db| async move {
        db.sql(&db.database, "CREATE TABLE k (id int PRIMARY KEY, v int); CREATE TABLE kt (ts timestamptz PRIMARY KEY);").await;
        let out = db
            .sql(
                &db.database,
                "SELECT 'slot' FROM pg_create_logical_replication_slot('live_keys', 'chelabase_keys', true);\n\
                 SET TimeZone = 'Asia/Tokyo';\n\
                 INSERT INTO k VALUES (1, 0);\n\
                 UPDATE k SET id = 2 WHERE id = 1;\n\
                 TRUNCATE k;\n\
                 INSERT INTO kt VALUES ('2026-01-02 03:04:05+00');\n\
                 SELECT data FROM pg_logical_slot_get_changes('live_keys', NULL, NULL);",
            )
            .await;
        let lines: Vec<&str> = out.lines().filter(|l| l.starts_with('{')).collect();
        assert_eq!(lines.len(), 4, "{out}");
        assert!(lines[0].contains(r#""t":"k","op":"i","k":[["1"]]"#), "{}", lines[0]);
        assert!(lines[1].contains(r#""op":"i","k":[["2"]]"#) && lines[1].contains(r#""op":"d","k":[["1"]]"#), "{}", lines[1]);
        assert!(lines[2].contains(r#""t":"k","op":"c""#), "{}", lines[2]);
        assert!(lines[3].contains(r#""k":[["2026-01-02 03:04:05+00"]]"#), "{}", lines[3]);
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the workbench stack on the round-3 images"]
async fn realtime_role_is_restricted() {
    with_db(|db| async move {
        db.sql("postgres", "CREATE ROLE chelabase_realtime LOGIN REPLICATION NOBYPASSRLS NOINHERIT PASSWORD 'rt-live-pw';").await;
        let normal = db.conninfo("chelabase_realtime", "rt-live-pw", false).await;
        let repl = db.conninfo("chelabase_realtime", "rt-live-pw", true).await;
        let err = db.psql(&normal, "SELECT 1;").await.unwrap_err();
        assert!(err.contains("may only open logical replication connections"), "{err}");
        let err = db.psql(&repl, "SELECT 1;").await.unwrap_err();
        assert!(err.contains("can't run SQL in a replication connection"), "{err}");
        let err = db.psql(&repl, "DROP_REPLICATION_SLOT rt_none;").await.unwrap_err();
        assert!(err.contains(r#"permission denied for replication command "DROP_REPLICATION_SLOT""#), "{err}");
        let ok = db
            .psql(&repl, "CREATE_REPLICATION_SLOT rt_live TEMPORARY LOGICAL chelabase_keys (SNAPSHOT 'nothing');")
            .await
            .unwrap();
        assert!(ok.contains("rt_live"), "{ok}");
        let err = db.sql_result("SELECT pg_create_logical_replication_slot('rt_su', 'test_decoding', true);").await.unwrap_err();
        assert!(err.contains(r#"reserved for chelabase_realtime"#), "{err}");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the workbench stack on the round-3 images"]
async fn realtime_slot_goes_with_a_compute_restart() {
    with_db(|db| async move {
        db.sql("postgres", "CREATE ROLE chelabase_realtime LOGIN REPLICATION NOBYPASSRLS NOINHERIT PASSWORD 'rt-live-pw';").await;
        let repl = db.conninfo("chelabase_realtime", "rt-live-pw", true).await;
        let mut child = tokio::process::Command::new("psql")
            .args([repl.as_str(), "-X", "-qtA"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("psql");
        let mut stdin = child.stdin.take().unwrap();
        stdin
            .write_all(b"CREATE_REPLICATION_SLOT rt_held TEMPORARY LOGICAL chelabase_keys (SNAPSHOT 'nothing');\n")
            .await
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let n = db.sql("postgres", "SELECT count(*) FROM pg_replication_slots WHERE slot_name = 'rt_held';").await;
            if n.trim() == "1" {
                break;
            }
            assert!(Instant::now() < deadline, "the slot never appeared");
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        db.docker(&["restart", &db.container()]).await;
        db.ready_again().await;
        let n = db.sql("postgres", "SELECT count(*) FROM pg_replication_slots;").await;
        assert_eq!(n.trim(), "0", "the temporary slot is gone");
        drop(stdin);
        let _ = tokio::time::timeout(Duration::from_secs(10), child.wait()).await;
        let _ = child.kill().await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the workbench stack on the round-3 images"]
async fn migrations_done_when_provisioned() {
    with_db(|db| async move {
        let id = db.sql("postgres", "SELECT id FROM neon_migration.migration_id;").await;
        assert_eq!(id.trim(), "12", "every compute_ctl migration ran before running");
        let bypass = db.sql("postgres", "SELECT rolbypassrls FROM pg_roles WHERE rolname = 'authenticator';").await;
        assert_eq!(bypass.trim(), "f");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the workbench stack on the round-3 images"]
async fn compute_has_tls_and_both_networks() {
    with_db(|db| async move {
        assert_eq!(db.sql("postgres", "SHOW ssl;").await.trim(), "on");
        assert_eq!(db.sql("postgres", "SHOW ssl_ca_file;").await.trim(), "/var/db/chelabase/tls/ca.crt");
        let networks = db.docker(&["inspect", "-f", "{{json .NetworkSettings.Networks}}", &db.container()]).await;
        assert!(networks.contains(&format!("\"{}\"", db.control_network)), "{networks}");
        let modes = db.docker(&["exec", &db.container(), "stat", "-c", "%a %U %n", "/var/db/chelabase/tls/server.key", "/var/db/chelabase/tls/server.crt", "/var/db/chelabase/tls/ca.crt"]).await;
        for line in modes.lines() {
            assert!(line.starts_with("644 root "), "{modes}");
        }
        assert_eq!(db.ctl_status().await, ComputeCtlStatus::Running);
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the workbench stack on the round-3 images"]
async fn cert_renewal_keeps_the_compute_running() {
    with_db(|db| async move {
        let before = db.docker(&["exec", &db.container(), "cat", "/var/db/postgres/compute/server.crt"]).await;
        let (renewed, failed) = db.worker.renew_certs_once(SystemTime::now() + Duration::from_secs(70 * 86_400)).await;
        assert!(renewed >= 1 && failed == 0, "{renewed} renewed, {failed} failed");
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            assert_eq!(db.ctl_status().await, ComputeCtlStatus::Running, "the status never leaves running");
            let now = db.docker(&["exec", &db.container(), "cat", "/var/db/postgres/compute/server.crt"]).await;
            if now != before {
                break;
            }
            assert!(Instant::now() < deadline, "compute_ctl never picked up the new certificate");
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        assert_eq!(db.sql("postgres", "SELECT 1;").await.trim(), "1");
    })
    .await;
}
```
(`Db::sql_result(&self, sql) -> Result<String, String>` is `admin_sql` on `postgres` mapped to `Err(format!("{e:#}"))`; the RED commit includes it.)

`services/worker/tests/member_roles_live.rs`: the `direct_in` body becomes (and `direct` calls `direct_in` with `self.database`):
```rust
        let control = std::env::var("CHELABASE_COMPUTE_CONTROL_NETWORK").expect("CHELABASE_COMPUTE_CONTROL_NETWORK");
        let stack = std::env::var("CHELABASE_STACK").unwrap_or_else(|_| "main".into());
        let addr = chelabase_neon::DockerLocator::connect(&control, &stack)
            .unwrap()
            .postgres_addr(ep)
            .await
            .expect("the compute's control address")
            .expect("on the control network");
        let tls = std::env::var("CHELABASE_COMPUTE_TLS_DIR").expect("CHELABASE_COMPUTE_TLS_DIR");
        let conn = format!(
            "host={} hostaddr={} port={} dbname={database} sslmode=verify-full channel_binding=require \
             sslrootcert={tls}/current/ca.crt sslcert={tls}/current/client.crt sslkey={tls}/current/client.key",
            chelabase_neon::compute_tls::server_name(ep),
            addr.ip(),
            addr.port()
        );
        psql(&conn, user, password, script).await
```
plus a helper and the new tests:
```rust
impl Fx {
    /// The old way in: straight to the compute on the default network, no certificate.
    async fn direct_plain(&self, ep: &str, user: &str, password: &str, script: &str) -> Result<String, String> {
        let addr = self.worker.computes.postgres_addr(ep).await.unwrap().expect("address");
        let conn = format!("host={} port={} dbname={} sslmode=prefer", addr.ip(), addr.port(), self.database);
        psql(&conn, user, password, script).await
    }

    async fn try_owner(&self, database: &str, sql: &str) -> Result<String, String> {
        self.worker
            .computes
            .admin_sql(&self.ep, database, &format!("SET ROLE postgres;\n{sql}"))
            .await
            .map_err(|e| format!("{e:#}"))
    }

    async fn allow_conn(&self, db: &str) -> (String, String) {
        let allow = self.sql(&format!("SELECT datallowconn FROM pg_database WHERE datname = '{db}';")).await;
        let marker = self
            .sql(&format!(
                "SELECT coalesce((SELECT array_to_string(s.setconfig, ',') FROM pg_db_role_setting s \
                 JOIN pg_database d ON d.oid = s.setdatabase WHERE d.datname = '{db}' AND s.setrole = 0), '-');"
            ))
            .await;
        (allow.trim().to_string(), marker.trim().to_string())
    }

    async fn guard_triggers(&self, db: &str) -> String {
        self.sql_on(&self.ep, db, "SELECT count(*) FROM pg_event_trigger WHERE evtname LIKE 'chelabase_schema_guard%';")
            .await
            .trim()
            .to_string()
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the workbench stack on the round-3 images"]
async fn member_roles_cannot_be_changed_from_sql() {
    live(|fx| async move {
        let m = fx.members[0].id;
        fx.grant(m, PlatformRole::Developer, &["billing"]).await;
        fx.reconcile(fx.main).await;
        let role = member_role_name(m);
        for sql in ["ALTER ROLE CURRENT_USER PASSWORD 'x';", "ALTER ROLE CURRENT_USER SET work_mem = '1MB';"] {
            let err = fx.as_member(m, sql).await.unwrap_err();
            assert!(err.contains("is managed by Chelabase"), "{sql}: {err}");
        }
        for sql in [format!("ALTER ROLE {role} RENAME TO cb_x;"), format!("GRANT sales TO {role};")] {
            let err = fx.try_owner("postgres", &sql).await.unwrap_err();
            assert!(err.contains("is managed by Chelabase"), "{sql}: {err}");
        }
        fx.as_member(m, "SELECT 1;").await.unwrap();
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the workbench stack on the round-3 images"]
async fn member_logins_need_the_orchestrator_identity() {
    live(|fx| async move {
        let m = fx.members[0].id;
        fx.grant(m, PlatformRole::Developer, &["billing"]).await;
        fx.reconcile(fx.main).await;
        let role = member_role_name(m);
        let password = fx.password(fx.main, &role).await;
        fx.direct(&fx.ep, &role, &password, "SELECT 1;").await.unwrap();
        let plain = fx.direct_plain(&fx.ep, &role, &password, "SELECT 1;").await.unwrap_err();
        assert!(plain.contains("pg_hba.conf rejects connection"), "{plain}");
        let local = fx
            .worker
            .computes
            .admin_sql_with(&fx.ep, "postgres", "SELECT 1;", &chelabase_neon::SqlOpts { user: Some(role.clone()), ..chelabase_neon::SqlOpts::admin_default() })
            .await
            .unwrap_err();
        assert!(format!("{local:#}").contains("pg_hba.conf rejects connection"), "{local:#}");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the workbench stack on the round-3 images"]
async fn reassign_owned_is_guarded_on_a_protected_branch() {
    live(|fx| async move {
        let dev = fx.members[0].id;
        fx.sql_on(&fx.ep, &fx.database, "GRANT sales TO billing; CREATE TABLE public.ro (id int); ALTER TABLE public.ro OWNER TO billing;").await;
        fx.grant(dev, PlatformRole::Developer, &["billing"]).await;
        fx.reconcile(fx.main).await;
        fx.metadata.set_branch_protected(fx.main, true).await.unwrap();
        fx.reconcile(fx.main).await;
        let err = fx.as_member(dev, "SET ROLE billing; REASSIGN OWNED BY billing TO sales;").await.unwrap_err();
        assert!(err.contains("42501") && err.contains("chelabase:branch_protected"), "{err}");
        let owner = fx.sql_on(&fx.ep, &fx.database, "SELECT pg_get_userbyid(relowner) FROM pg_class WHERE relname = 'ro';").await;
        assert_eq!(owner.trim(), "billing");
        fx.metadata.set_branch_protected(fx.main, false).await.unwrap();
        fx.reconcile(fx.main).await;
        fx.as_member(dev, "SET ROLE billing; REASSIGN OWNED BY billing TO sales;").await.unwrap();
        fx.sql_on(&fx.ep, &fx.database, "DROP TABLE public.ro; REVOKE sales FROM billing;").await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the workbench stack on the round-3 images"]
async fn a_database_from_template0_is_born_closed_then_guarded() {
    live(|fx| async move {
        let dev = fx.members[0].id;
        fx.grant(dev, PlatformRole::Developer, &["billing"]).await;
        fx.metadata.set_branch_protected(fx.main, true).await.unwrap();
        fx.reconcile(fx.main).await;
        fx.try_owner("postgres", "CREATE DATABASE bc1 TEMPLATE template0;").await.unwrap();
        assert_eq!(fx.allow_conn("bc1").await, ("f".into(), "neon.born_closed=on".into()));
        fx.reconcile(fx.main).await;
        assert_eq!(fx.allow_conn("bc1").await, ("t".into(), "-".into()));
        assert_eq!(fx.guard_triggers("bc1").await, "2");
        fx.sql_on(&fx.ep, "bc1", "GRANT CREATE ON DATABASE bc1 TO billing;").await;
        let role = member_role_name(dev);
        let password = fx.password(fx.main, &role).await;
        let err = fx.direct_in(&fx.ep, "bc1", &role, &password, "SET ROLE billing; CREATE SCHEMA s1;").await.unwrap_err();
        assert!(err.contains("chelabase:branch_protected"), "{err}");
        fx.metadata.set_branch_protected(fx.main, false).await.unwrap();
        fx.reconcile(fx.main).await;
        fx.sql("DROP DATABASE bc1;").await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the workbench stack on the round-3 images"]
async fn a_closed_database_is_guarded_but_stays_closed() {
    live(|fx| async move {
        fx.sql("CREATE DATABASE shut OWNER postgres;").await;
        fx.sql("ALTER DATABASE shut ALLOW_CONNECTIONS false;").await;
        fx.metadata.set_branch_protected(fx.main, true).await.unwrap();
        fx.reconcile(fx.main).await;
        assert_eq!(fx.guard_triggers("shut").await, "2");
        assert_eq!(fx.allow_conn("shut").await, ("f".into(), "-".into()));
        let err = fx.try_owner("postgres", "ALTER DATABASE shut ALLOW_CONNECTIONS true;").await.unwrap_err();
        assert!(err.contains("set by Chelabase"), "{err}");
        fx.metadata.set_branch_protected(fx.main, false).await.unwrap();
        fx.reconcile(fx.main).await;
        fx.try_owner("postgres", "ALTER DATABASE shut ALLOW_CONNECTIONS true;").await.unwrap();
        fx.sql("DROP DATABASE shut;").await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the workbench stack on the round-3 images"]
async fn born_closed_opens_without_a_guard_when_unprotected() {
    live(|fx| async move {
        fx.metadata.set_branch_protected(fx.main, true).await.unwrap();
        fx.reconcile(fx.main).await;
        fx.try_owner("postgres", "CREATE DATABASE bc2;").await.unwrap();
        assert_eq!(fx.allow_conn("bc2").await.0, "f");
        fx.metadata.set_branch_protected(fx.main, false).await.unwrap();
        fx.reconcile(fx.main).await;
        assert_eq!(fx.allow_conn("bc2").await, ("t".into(), "-".into()));
        assert_eq!(fx.guard_triggers("bc2").await, "0");
        fx.sql("DROP DATABASE bc2;").await;
    })
    .await;
}
```
RED: the unit tests don't compile (`set_status`, `ClosedDatabase`, `plan_closed_databases`, the config fields: the code under test). The live tests compile once the units exist and are **expected RED until the image build**: the coordinator proves they compile with `cargo test -p worker --test compute_fork_live --test member_roles_live --no-run` at T9's RED commit (after adding the empty `pub fn`/`pub struct` declarations only where the live files need them to compile, per the standing rule), and that they fail for the right reason by reading each test's first assertion against the pinned images' behaviour (e.g. `SHOW ssl` is `off` today; `migration_id` read right after provisioning; `CREATE ROLE chelabase_realtime` then a normal login succeeds). They run in End of step 5 only. Assumptions checked: `Worker`'s `computes` and `pusher` fields are public (fakes construct them, `fakes.rs:3576-3633`); `admin_sql_with`'s `user` uses `127.0.0.1` (`backend.rs:212-224`); `set_branch_protected` and `password` exist (`member_roles_live.rs:1914`, `:435`); `DEMO_TENANT_ID`/`DEMO_PROJECT_ID` provisioning (`neon_live.rs:31-33`); the runner has `psql` and `docker` (`member_roles_live.rs:526-560`, `branch_live.rs:749-757`); `/var/db/postgres/compute` is `pgdata` (`crates/chelabase-neon/src/compute.rs:70`); compute_ctl writes `server.crt` there (`compute_tools/src/tls.rs:63-87`); 12 migrations (`compute_tools/src/spec.rs:255-309`).

**Gotchas:** the guard loop's `guard_databases` lists `(db, guard_only)` from the catalog (`member_roles.rs:1080`); closed databases join it without `guard_only`; an install failure for a born-closed database leaves it closed and counts as a failure of the run.

**Verify:** `cargo test -p worker` (unit), `cargo clippy -p worker --all-targets`, `cargo test -p worker --test compute_fork_live --test member_roles_live --no-run`.

**Live runs:** none here; `compute_fork_live` and `member_roles_live` run in End of step 5.

---

## T10: orchestrator (sonnet / opus)

**Worktree:** `just wt-add neon-fork-round-3-t10 neon-fork-round-3` (after T8 merged).

**Files:** `services/orchestrator/src/{main.rs,internal/config.rs,internal/spec.rs,console.rs,sql_editor/run.rs,sql_editor/routes.rs}`, a small reload task (`services/orchestrator/src/compute_tls.rs`), `crates/chelabase-types/src/api_error.rs`, `openapi/*.json` and both generated clients (`just api-gen`), `apps/docs/content/docs/reference/errors.mdx`, `services/orchestrator/tests/sql_run_live.rs` (`RunTls::Plain` in its `RunTarget` literal); tests (coordinator): `api_error.rs`' `ALL` list, `routes.rs`' test module.

**Behaviour:** spec §4.2 (orchestrator connections, RT3-7); pinned interfaces. `AppState.locator` uses `CHELABASE_COMPUTE_CONTROL_NETWORK` (`main.rs:110-113`); `InternalState.locator` stays on the default network (`main.rs:70`). The reload task runs `ClientTls::reload_if_changed` every 60 s.

**RED tests (coordinator).** In `crates/chelabase-types/src/api_error.rs`' test module: `ALL` becomes `[(ErrorCode, &str); 97]` with `(ErrorCode::ComputeTlsFailed, "compute_tls_failed"),` appended. In `sql_editor/routes.rs`' test module:
```rust
    // RT3-7: a TLS or certificate problem is a typed 503 without the forced reconcile (it
    // can't help); any other refused login keeps today's path.
    #[test]
    fn tls_and_hba_refusals_are_compute_tls_failed() {
        let branch = BranchId::new();
        let tls = |e: ConnectError| match member_login_refusal(branch, e) {
            MemberLogin::Refused(api) => (api.status, api.code, api.retry_after.is_some()),
            MemberLogin::Retry => panic!("a TLS problem isn't a stale role"),
        };
        assert_eq!(
            tls(ConnectError::Tls("invalid peer certificate: UnknownIssuer".into())),
            (503, ErrorCode::ComputeTlsFailed, true)
        );
        for message in [
            "pg_hba.conf rejects connection for host \"10.213.4.12\", user \"cb_m_ab\", database \"app\", SSL encryption",
            "connection requires a valid client certificate",
            "certificate validation (clientcert=verify-full) failed for user \"cb_m_ab\": CN mismatch",
        ] {
            let rejected = ConnectError::Rejected { code: "28000".into(), message: message.into() };
            assert_eq!(tls(rejected), (503, ErrorCode::ComputeTlsFailed, true), "{message}");
        }
        let stale = ConnectError::Rejected { code: "28000".into(), message: "role \"cb_m_ab\" is not permitted to log in".into() };
        assert!(matches!(member_login_refusal(branch, stale), MemberLogin::Retry));
    }
```
RED: compile errors on `ComputeTlsFailed` and `ConnectError::Tls` (the code under test). Assumptions checked: `ApiError.retry_after` (`api_error.rs:360-366`); the existing `member_login_refusals_map` keeps passing (its `28000` message is `"no"`, `routes.rs:1107`).

**Gotchas:** `RunTarget` is also built for metadata-database tests (`sql_run_live.rs:653`); the console's pool key fingerprint includes the client pair (`console.rs:106-127`); `cancel_query` (`run.rs:368`) uses the same connector.

**Verify:** `cargo test -p orchestrator`, `cargo test -p chelabase-types --features axum`, `cargo clippy -p orchestrator --all-targets`, `just api-gen` then `git status` clean apart from the regenerated files committed.

**Live runs:** none (End of step 5).

---

## T11: CLI/workbench and Compose (sonnet / sonnet)

**Worktree:** `just wt-add neon-fork-round-3-t11 neon-fork-round-3` (after T8 merged).

**Files:** `tools/cli/src/neon/compute_ca.rs` (new) and `tools/cli/src/neon/mod.rs`, `tools/cli/src/dev/bootstrap.rs` (`MachineKeys`, `ensure_machine_keys`), `tools/cli/src/workbench/{env.rs,registry.rs}` and their callers, `infra/compose/docker-compose.yml`, `.env.example` if it lists the compute keys; tests (coordinator): `tools/cli/src/neon/compute_ca.rs`' test module, `tools/cli/src/workbench/tests.rs`, `scripts/tests/workbench-compose.sh`, `scripts/tests/fixtures/workbench-stack.env`.

**Behaviour:** spec §4.2 (key directories, mounts, `control` network); pinned names. `compute_ca::ensure(dir) -> Result<ComputeCaPaths>` never overwrites (refuses a half pair, like `compute_jwt::ensure`); `compute_ca::ensure_client_pair(ca_dir, tls_dir, now) -> Result<bool>` issues the first pair (and a due one) with `write_client_pair`. `MachineKeys` gains `compute_ca` and `orchestrator_tls`. Registry `Entry` gains `#[serde(default)] control_slot: Option<u8>`, filled by `ensure` (first free 0..=63; `main` gets 0); `StackEnvInput` gains `control_slot: u8`; `env.rs` exposes `pub fn control_net(slot: u8) -> ControlNet { subnet, compute_range, orchestrator_ip, runner_ip }`.

**RED tests (coordinator).** In `tools/cli/src/neon/compute_ca.rs` (the RED commit adds `pub mod compute_ca;` and the file with only this test module):
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::time::{Duration, SystemTime};

    #[test]
    fn the_ca_is_made_once_and_never_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ensure(dir.path()).unwrap();
        let (crt, key) = (std::fs::read(&paths.cert).unwrap(), std::fs::read(&paths.key).unwrap());
        assert_eq!(std::fs::metadata(&paths.key).unwrap().permissions().mode() & 0o777, 0o600);
        let again = ensure(dir.path()).unwrap();
        assert_eq!((std::fs::read(&again.cert).unwrap(), std::fs::read(&again.key).unwrap()), (crt, key));
        std::fs::remove_file(&paths.key).unwrap();
        assert!(ensure(dir.path()).is_err(), "a half pair is refused");
    }

    #[test]
    fn the_client_pair_is_issued_then_renewed_only_when_due() {
        let ca = tempfile::tempdir().unwrap();
        let tls = tempfile::tempdir().unwrap();
        ensure(ca.path()).unwrap();
        let now = SystemTime::now();
        assert!(ensure_client_pair(ca.path(), tls.path(), now).unwrap(), "first issue");
        let current = tls.path().join("current");
        let first = std::fs::read_link(&current).unwrap();
        assert!(!ensure_client_pair(ca.path(), tls.path(), now + Duration::from_secs(86_400)).unwrap());
        assert_eq!(std::fs::read_link(&current).unwrap(), first);
        assert!(ensure_client_pair(ca.path(), tls.path(), now + Duration::from_secs(310 * 86_400)).unwrap(), "under 60 days left");
        assert_ne!(std::fs::read_link(&current).unwrap(), first);
        for f in ["client.crt", "client.key", "ca.crt"] {
            assert!(current.join(f).is_file(), "{f}");
        }
    }
}
```
In `tools/cli/src/workbench/tests.rs` (the `input()` helper gains `control_slot: 1`):
```rust
#[test]
fn the_stack_env_carries_the_control_network() {
    let env = parse(&render_stack_env(EXAMPLE, &input()));
    for (k, v) in [
        ("CHELABASE_COMPUTE_CONTROL_NETWORK", "chelabase-feat-x_control"),
        ("CHELABASE_CONTROL_SUBNET", "10.213.4.0/22"),
        ("CHELABASE_CONTROL_COMPUTE_RANGE", "10.213.6.0/23"),
        ("CHELABASE_ORCHESTRATOR_CONTROL_IP", "10.213.4.10"),
        ("CHELABASE_RUNNER_CONTROL_IP", "10.213.4.11"),
        ("CHELABASE_MEMBER_LOGIN_CIDRS", "10.213.4.10/32,10.213.4.11/32"),
        ("CHELABASE_COMPUTE_CA_DIR", "/m/.chelabase/compute-ca"),
        ("CHELABASE_COMPUTE_TLS_DIR", "/m/.chelabase/orchestrator-tls"),
    ] {
        assert_eq!(env.get(k).map(String::as_str), Some(v), "{k}");
    }
    let main = control_net(0);
    assert_eq!(
        (main.subnet.as_str(), main.compute_range.as_str(), main.orchestrator_ip.as_str(), main.runner_ip.as_str()),
        ("10.213.0.0/22", "10.213.2.0/23", "10.213.0.10", "10.213.0.11")
    );
    assert_eq!(control_net(63).subnet, "10.213.252.0/22");
}

#[test]
fn every_registered_stack_gets_its_own_control_slot() {
    let mut r = Registry::default();
    let main = r.ensure("main", Path::new("/m"), 1, |_| true).unwrap();
    assert_eq!(main.control_slot, Some(0));
    let a = r.ensure("feat-a", Path::new("/m/.worktrees/feat-a"), 2, |_| true).unwrap();
    let b = r.ensure("feat-b", Path::new("/m/.worktrees/feat-b"), 3, |_| true).unwrap();
    let slots = [main.control_slot, a.control_slot, b.control_slot];
    assert!(slots.iter().all(Option::is_some));
    let mut unique = slots.to_vec();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), 3, "{slots:?}");
    let old: Registry = serde_json::from_str(r#"{"stacks":[{"slug":"feat-c","checkout":"/m/.worktrees/feat-c","gateway_port":8140,"proxy_port":4540,"created_at":1,"running_since":null}]}"#).unwrap();
    let mut old = old;
    let c = old.ensure("feat-c", Path::new("/m/.worktrees/feat-c"), 4, |_| true).unwrap();
    assert!(c.control_slot.is_some(), "an old registry entry gets a slot");
}
```
In `scripts/tests/fixtures/workbench-stack.env`, appended:
```
CHELABASE_COMPUTE_CONTROL_NETWORK=chelabase-feat-x_control
CHELABASE_CONTROL_SUBNET=10.213.4.0/22
CHELABASE_CONTROL_COMPUTE_RANGE=10.213.6.0/23
CHELABASE_ORCHESTRATOR_CONTROL_IP=10.213.4.10
CHELABASE_RUNNER_CONTROL_IP=10.213.4.11
CHELABASE_MEMBER_LOGIN_CIDRS=10.213.4.10/32,10.213.4.11/32
CHELABASE_COMPUTE_CA_DIR=/m/.chelabase/compute-ca
CHELABASE_COMPUTE_TLS_DIR=/m/.chelabase/orchestrator-tls
```
In `scripts/tests/workbench-compose.sh`, before its final success line:
```bash
[[ "$(q '.networks.control.ipam.config[0].subnet')" == 10.213.4.0/22 ]] || fail "the control network has the stack's subnet"
[[ "$(q '.networks.control.ipam.config[0].ip_range')" == 10.213.6.0/23 ]] || fail "computes get addresses from the upper /23"
[[ "$(q '.services.orchestrator.networks.control.ipv4_address')" == 10.213.4.10 ]] || fail "the orchestrator's static control address"
[[ "$(q '.services.runner.networks.control.ipv4_address')" == 10.213.4.11 ]] || fail "the runner's static control address"
[[ "$(q '.services.orchestrator.networks | keys | join(" ")')" == "control default" ]] || fail "the orchestrator is on both networks"
[[ "$(q '.services.worker.networks | keys | join(" ")')" == "default" ]] || fail "the worker isn't on the control network"
ca_mounts="$(q '[.services | to_entries[] | select(any(.value.volumes[]?; .source == "/m/.chelabase/compute-ca")) | .key] | sort | join(" ")')"
[[ "$ca_mounts" == worker ]] || fail "only the worker mounts the CA; got: $ca_mounts"
tls_mounts="$(q '[.services | to_entries[] | select(any(.value.volumes[]?; .source == "/m/.chelabase/orchestrator-tls")) | "\(.key):\(any(.value.volumes[]; .source == "/m/.chelabase/orchestrator-tls" and .read_only == true))"] | sort | join(" ")')"
[[ "$tls_mounts" == "orchestrator:true runner:true worker:false" ]] || fail "orchestrator-tls: rw for the worker, ro for the orchestrator and runner; got: $tls_mounts"
```
RED: the Rust tests don't compile (`ensure`, `ensure_client_pair`, `control_net`, `control_slot`: the code under test); the shell checks fail (no `control` network). Assumptions checked: `Registry::ensure(slug, checkout, now, free)` (`registry.rs:55-92`); the fixture is the `--env-file` and interpolation resolves its values (`workbench-compose.sh:11-13`); `<keys>` is `/m/.chelabase` in the fixture (`CHELABASE_KEYS_DIR`); `tempfile` is a dev-dependency of the CLI (its compute_jwt tests use it, `compute_jwt.rs:150`).

**Gotchas:** an existing registry has no `control_slot` (serde default, then filled on `ensure`); computes join `control` from the worker (T8), not from Compose; `ensure_machine_keys`' callers pass the two new directories; the runner gets `CHELABASE_COMPUTE_CONTROL_NETWORK` and `CHELABASE_COMPUTE_TLS_DIR` through `stack.env`.

**Verify:** `cargo test -p chelabase` (the CLI package), `cargo clippy -p chelabase --all-targets`, `bash scripts/tests/workbench-compose.sh`, `just neon-test`.

**Live runs:** none.

---

## T12: docs (haiku / none)

**Worktree:** `just wt-add neon-fork-round-3-t12 neon-fork-round-3`.

**Files and text:** `apps/docs/content/docs/using/branches.mdx:76` (spec §6's replacement sentences, both: born closed timing, and owner-closed databases staying closed while protected); `apps/docs/content/docs/architecture/table-api.mdx:71` (`REASSIGN OWNED` now fires the DDL event triggers, so the protected-branch guard covers ownership moves); `apps/docs/content/docs/architecture/security.mdx` (a short "Member logins" paragraph: only from the orchestrator's address with its client certificate and the member's password; Kubernetes relies on certificates and a NetworkPolicy); `infra/neon/README.md` (a "Round 3" paragraph after `:9`, listing D1–D6 in one sentence each, with `<sha12>` left for End of step 6). No tests (leaf).

**Verify:** `pnpm --filter @chelabase/docs build` if the docs build is part of `just check`; otherwise the end-of-step review reads the diff.

**Live runs:** none.

---

## Final fix round (pipelines; user, 2026-10-15)

Each batch is one pipeline: the coordinator's brief (the findings below with their Status entries, the checklist "probe edges" from the preamble) → **test agent** (RED tests, run against the base, committed `fN: RED tests`) → **coder agent** (GREEN) → **audit agent** (review + tidy, RED diff empty, defects vs hardening). No agent spawns agents; at most 2 pipelines run at once (3 agents total); at most 2 fork `ci-local` containers. Fork checkouts are fresh clones `<neon checkout>-fN` (from the local clone, origin reset to GitHub), Neon branch `round-3-fN` from `round-3`, Postgres branch `round-3-v17-fN` from `round-3-v17`; their volumes are seeded from `chela-neon-*-t6` (Rust 1.99 image). Chelabase batches use worktrees `just wt-add neon-fork-round-3-fN neon-fork-round-3`. Every finding below is described in the Status block (cite it in the brief).

| Batch | Checkout | Findings | Test / coder / audit | Order |
|---|---|---|---|---|
| F1 Neon tooling + Rust deps | Neon `round-3-f1` | T1 m1–m4 (pg-check: per-dir `regression.diffs` cleanup and depth, `-j`, **`neon_rmgr` TEMP_CONFIG** for `chelabase_keys`); T0b **C1** (aws-lc-rs process default in pageserver, safekeeper, storage_controller, storage_broker + a both-providers test), m2 (hakari), m3 (10 s S3 connect timeout, R-T0b-b), m5 (tool pins vs registry) | sonnet / opus / sonnet | 1st (pg-check fix is needed by F2's checks) |
| F2 Postgres C | Postgres `round-3-v17-f2` in clone `-f2` | T2 I1, I2/**U4**, m1, m2, m4; T3 I1/**U6**, I2/**U7** (by `MyBackendType`, per the end-of-step notes), m1, m2; T4 m1–m3 | opus / opus / opus | after F1 (rebases its ci-local) |
| F3 neon extension + compute_ctl | Neon `round-3-f3` | T5 m1–m4 (m2: non-superusers only); **E-I2** C side (guard/born-closed decided for open user templates and the target, not only the session database); T6 m1–m6 + note; E-m3 (rewrite the hba/ident block between markers on reconfigure + reload); E-m6 (`FORK.md`) | opus / opus / opus | parallel with F2 |
| F4 worker + chelabase-neon | Chelabase `neon-fork-round-3-f4` | T8 I1 (key first in the tar), m1–m5; T9 I2 (`from_ddl` flag; the orchestrator's `ddl.rs` sets it), I3 (judge the pgdata copy), **I4** (open user templates guarded), m1–m6; **U11** | sonnet / sonnet / opus | after F2/F3 (fork containers free) |
| F5 orchestrator + rust-postgres | Chelabase `neon-fork-round-3-f5` + `<rust-postgres checkout>` | T10 I1/**U9** (rust-postgres: expose a verified SASL final before the error; pin bump, CLAUDE.md pin line; replace the RED test's third message with the U9 case), I2 (typed TLS classification), I3 (app-users sync over TLS), m1–m4 | opus / opus / opus | parallel with F4 |
| F6 CLI, Compose, docs | Chelabase `neon-fork-round-3-f6` | T11 I1/**U10** (mask `.chelabase`, narrow mounts per service), I2 (reissue on CA change or missing files), m1–m3; **R-T9a** (runner mounts the CA ro; compose check changed); T11 m4 handled in F4 (worker recreates pre-CA computes); E-m5 (docs) | sonnet / sonnet / sonnet | after F4/F5 |

Not tested (coordinator reads the diff): docs, `FORK.md`, comments, log wording, tool-pin checks; the T2 m3 note goes to RT1's plan.

After all six: merge `round-3-v17-f2` into `round-3-v17`; `round-3-f1`, `-f3` into `round-3` with the submodule moved to the new `round-3-v17` head and `revisions.json` updated; the Chelabase batches into `neon-fork-round-3` (`--no-ff`); remove the batch clones and worktrees; then End of step 3–5.

---

## End of step

0. Done before this plan: the spec's red-team pass (opus, RT3-1 to RT3-15). At the step's start `main` is green (`fix-main-clippy` merged, `da199073`).
1. Whole-branch review (opus) on `main..neon-fork-round-3` and the fork diffs `chelabase/neon` `main..round-3` and `chelabase/postgres` `REL_17_STABLE_chelabase..round-3-v17`; it also covers T7 and T12.
2. The one final fix round: the review's findings plus every task finding deferred to it (listed in the Status); the coordinator writes a failing test for each finding first. Fork fixes land on the round branches in a clone; Chelabase fixes in a fix worktree.
3. `just --justfile <wt>/justfile --working-directory <wt> check`, once.
4. Fork gate and the one image build. First list the pending fork changes in the Status (there must be none). In `<neon checkout>` on `round-3` (submodule on `round-3-v17`):
   - `.github/scripts/ci-local.sh` (default: lint, build, Rust tests; background);
   - `.github/scripts/ci-local.sh --only pg-check`;
   - `.github/scripts/ci-local.sh --only regress -k "test_chelabase_keys or test_chelabase_realtime_role or test_closed_database_connect or test_member_logins_hba or test_member_role_guard or test_born_closed_database or test_ddl_forwarding or test_ddl_protected_database or test_compute_migrations or test_neon_superuser or test_event_trigger or test_logical_replication"` (check the selection with `--collect-only` first);
   - `.github/scripts/publish-dev.sh --no-push --allow-unpushed` (builds `ghcr.io/chelabase/neon-storage:<sha12>-dev` and `neon-compute-v17:<sha12>-dev`); re-tag both as `chelabase/neon-*:<sha12>-dev` (`infra/neon/README.md:73-76`);
   - on `neon-fork-round-3`: `infra/neon/NEON_REF` = `round-3`'s head (full sha), commit `neon: pin round-3 head for the live pass`, push; then the plan's one stack: `just --justfile <wt>/justfile --working-directory <wt> workbench up` (it picks the local `<sha12>-dev` images, `scripts/neon-images.sh`), Zitadel bootstrap as the roadmap's working rules say.
5. Live pass on that stack, one suite at a time (at most 3 at once machine-wide), whole files, once:
   - `just workbench test -p worker --test compute_fork_live -- --run-ignored only`
   - `just workbench test -p worker --test member_roles_live -- --run-ignored only`
   - `just workbench test -p worker --test roles_live -- --run-ignored only`
   - `just workbench test -p worker --test branch_live -- --run-ignored only`
   - `just workbench test -p worker --test neon_live -- --run-ignored only`
   - `just workbench test -p rest --test rest_live -- --run-ignored only`
   - `just workbench test -p orchestrator --test api -- --run-ignored only`
   - `just workbench test -p chelabase-metadata --test live_db -- --run-ignored only`
   - `just workbench test -p chelabase-neon --test docker_backend_live -- --run-ignored only`
   - e2e: `just neon-e2e`, `just data-e2e`, `just sql-editor-e2e`, `just role-e2e`, `just branch-e2e`, `just console-e2e`, `just storage-leak-e2e`, `just members-e2e`.
   A failure goes to a debug subagent (sonnet; opus for fork, TLS or concurrency causes); a fix means a new fork commit → the gate items it touches, a new image build, and the pass again from the failed suite (listed in the Status).
6. `docs/landing-highlights.md` on `main`: what round 3 solves (keys-only realtime decoding with a locked-down replication login; member logins bound to the orchestrator by address and client certificate; ownership moves and new databases covered by the protected-branch guard; no migration race at compute start), what it unlocks (RT1; per-Postgres-database member overrides), evidence (the suites above), the original Neon base vs ours. Update the plan Status and the roadmap's "Resume here", then **pause for the user**.
7. Ship, after the user's go: fast-forward `chelabase/postgres` `REL_17_STABLE_chelabase` and `chelabase/neon` `main` to the tested commits (direct push, as round 2's T9 did, so the SHAs stay); `.github/scripts/publish-dev.sh --push-only` (prints the digests); on `neon-fork-round-3`: both Compose digests (`infra/compose/docker-compose.yml:17-19`), `infra/neon/README.md` (the Round 3 paragraph's tag), commit `neon: bump to fork round 3`, push; `just neon-test`; merge into `main` (`--no-ff`), push; purge the stack (`just wt-remove neon-fork-round-3`), delete the branch on origin and locally; delete the fork task branches on origin.
