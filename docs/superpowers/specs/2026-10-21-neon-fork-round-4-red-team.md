# Neon fork round 4: red-team of the design spec

Attacks the spec `2026-10-21-neon-fork-round-4-design.md` (`6d4231f0`) and the subsystems it
touches. Evidence read on `chelabase-neon` at `629166cee` (Postgres via `vendor/postgres-v17`
at `76ce76278c5`) and the worktree at `6d4231f0`. Findings are RT4-N with severity, the concrete
scenario, verified evidence, and a proposed ruling. A per-area implementer checklist is at the end.
Items needing a user decision are marked **USER**.

Severity tally: 2 critical, 7 important, 6 minor.

---

## Settled open item: do the trusted extensions let a non-superuser read local files or hit localhost?

The spec left this for the plan's pre-flight (§4.1, Open questions). Settled here:

- **Image reality.** Chelabase ships the **dev / `EXTENSIONS=minimal`** image
  (`publish-dev.sh:228` `--build-arg EXTENSIONS=minimal`; `infra/neon/README.md:49`). `extensions-minimal`
  copies only **pgrag, timescaledb, pg_cron, pg_partman** (`compute-node.Dockerfile:1653-1658`).
  **pg_duckdb, pg_mooncake, anon are NOT in the dev image** — they are only in `extensions-all`
  (`:1702,1705,1706`), the platform variant. So **on what self-hosters run today there is no
  file-read extension at all.**
- **pgrag (in the dev image).** Its HTTP callers target hardcoded hosts (`api.openai.com`, etc.;
  `exts/rag/src/openai.rs`), key-gated; no arbitrary URL, so no SSRF to `127.0.0.1:3080/3081`. Safe.
- **anon (platform only).** `anon.load_csv`/`anon.init` use `COPY ... FROM`, are `SECURITY INVOKER`
  and labelled `UNTRUSTED` (`sql/init.sql`); `COPY FROM` needs `pg_read_server_files`, which
  `neon_superuser` does not hold. Safe.
- **pg_duckdb v0.3.1 (platform only).** Safe **by default only**: `duckdb.postgres_role` defaults to
  `""` = superusers only, and `duckdb.disabled_filesystems` defaults to `LocalFileSystem` for
  non-superusers (both superuser-only GUCs; `docs/settings.md`). Chelabase sets neither, so a
  non-superuser cannot run DuckDB at all. If anything ever sets `duckdb.postgres_role`, a member of
  that role reads any file postgres can (see `test/.../non_superuser.out`: `read_csv` is blocked
  only by `disabled_filesystems`).
- **pg_mooncake v0.1.2 (platform only) — the hole.** It vendors pg_duckdb but its guards are
  **commented out**: `IsDuckdbExecutionAllowed()` in `GetConnection`/`CreateConnection` and the
  `disabled_filesystems` block in `RefreshConnectionState` are all `//`-commented
  (`src/pgduckdb/pgduckdb_duckdb.cpp`). So **any role** can run `SELECT mooncake.read_csv('/abs/path')`
  / `read_parquet` and read arbitrary local files. It is marked `trusted`
  (`compute-node.Dockerfile:1502`) and is in `neon.unstable_extensions` (`compute/manifest.yaml:76`),
  but `neon.allow_unstable_extensions` is `PGC_USERSET` (`pgxn/neon/unstable_extensions.c:113`), so a
  non-superuser owner runs `SET neon.allow_unstable_extensions='true'; CREATE EXTENSION pg_mooncake;`
  and then reads any file postgres can — including the two round-4 token files. See RT4-1.

Conclusion: the spec's reliance on SQL-level file-read privileges (`pg_read_server_files` etc.) is
necessary but **not sufficient** against a trusted C extension that itself opens files. On the dev
image this is moot; on the platform image pg_mooncake defeats it outright.

---

## RT4-1 (critical) — pg_mooncake reads the compute_ctl internal token → `/grants` as `cloud_admin`

**Attack (platform image).** A database owner (a `neon_superuser` member, not a PG superuser) runs
`SET neon.allow_unstable_extensions='true'; CREATE EXTENSION pg_mooncake;` then
`SELECT mooncake.read_csv('/var/db/postgres/compute_ctl_internal.token')` (or `storage_auth.token`).
pg_mooncake's non-superuser/filesystem guards are commented out (evidence above), so the read
succeeds and returns the bearer secret. With `compute_ctl_internal.token` the owner calls
`http://127.0.0.1:3081/grants`, which runs `GRANT … ON SCHEMA …` on compute_ctl's **`cloud_admin`**
connection (`compute_tools/src/http/routes/grants.rs:14-47`) — arbitrary privilege grants as the
superuser. `/extensions` and `/refresh_configuration` are also reachable. The loopback bind + bearer
of §4 do **not** stop this: the caller is inside the compute and holds the secret.

**Evidence.** `src/pgduckdb/pgduckdb_duckdb.cpp` (commented guards), `compute-node.Dockerfile:1502,1705`,
`compute/manifest.yaml:76`, `pgxn/neon/unstable_extensions.c:108-125`, `grants.rs:14-47`, spec §4.1.

**Ruling.** (a) Keep the dev image as the shipped image (no change needed there; state it). (b) For
the platform image, this round must either **drop pg_mooncake** or **patch it** to restore the
`IsDuckdbExecutionAllowed()` and `disabled_filesystems='LocalFileSystem'` guards for non-superusers
(a compute patch like `pg_duckdb_v031.patch`), and defensively pin `duckdb.postgres_role` empty +
`duckdb.disabled_filesystems=LocalFileSystem` in `postgresql.conf`. (c) Independently, treat
`compute_ctl_internal.token` as **`cloud_admin`-equivalent** (RT4-6): no SQL-reachable boundary can
be assumed to protect it. **USER:** is platform-mode in scope this round, or is the ruling "dev image
only ships safe extensions; platform fix tracked separately"? Options: *(Recommended)* patch/drop
pg_mooncake + pin duckdb GUCs this round; or defer platform fix and document the exposure.

## RT4-2 (important) — mixed-version deploy: new worker's 24 h tokens vs an old compute with no refresh

**Attack / failure.** The worker begins minting tenant tokens with `exp = now+24h` (§8, S9) before
the new compute image is universally deployed, or the compute image is rolled back after the worker
upgrades. An **old** compute_ctl ignores `--storage-auth-token-file`, still reads
`spec.storage_auth_token` once into `NEON_AUTH_TOKEN` (`libpagestore.c:1642-1644`), and has no
refresh path. 24 h after its last spec push every new pageserver/safekeeper connection fails auth and
the compute silently loses storage access (open connections keep working, masking it).

**Evidence.** `compute_tools/src/compute.rs:1752-1776` (env token today), spec §8 "A compute that
misses refreshes", `infra/neon/README.md` rollback hazard note (round 2 precedent).

**Ruling.** The plan pins deploy order: **compute image first, then the worker starts minting
`exp` tokens**; and a rollback of the compute image below round-4 after the worker ships `exp` tokens
is a named hazard in `infra/neon/README.md`. Consider a worker env gate (`CHELABASE_STORAGE_TOKEN_TTL`
= unset/none until computes are new) so the TTL can be enabled only after the image bump. **USER**
confirmation of the gate vs. "bump-both-together only" is worth one line.

## RT4-3 (important) — storage-token refresh rides the full `reconfigure`, which fails the compute on any error

**Attack / failure.** The refresh loop pushes a spec with `skip_pg_catalog_updates:true` via
`/configure` (§8). `/configure` sets `ConfigurationPending`; the configurator runs
`compute.reconfigure()`, and **any** error there (pgbouncer tune, local_proxy configure, rsyslog,
`rewrite_config_files`, reload) sets the compute to **`Failed`** and the configurator then exits
(`configurator.rs:46-57,188-189`). So an hourly token refresh can take a *healthy running* compute
down over an unrelated reconfigure fault, and `/configure` returns 500.

**Evidence.** `compute_tools/src/configurator.rs:41-57`, `compute.rs:2048-2137` (reconfigure does
pgbouncer/local_proxy/rsyslog + file rewrite + optional SQL), `http/routes/configure.rs:35-37` (412
only for non-Empty/Running).

**Ruling.** The refresh must not be able to fail a running compute: either (a) a dedicated lightweight
path that only rewrites the two token files + `pg_reload_conf()` (no pgbouncer/local_proxy/rsyslog,
no status change), or (b) on a refresh-origin reconfigure error, log + keep `Running` with the old
files intact rather than `Failed`. The worker's failure counter (§8) then reflects transient pushes,
not compute death. Add a RED test: a refresh whose reconfigure errors leaves the compute `Running`.

## RT4-4 (important) — the personal-login marker is not carried to child branches created *after* marking

**Attack.** The marker lives in `pg_db_role_setting (setdatabase=0, setrole=oid)` (spec §12, S15),
inherited by a child **only via basebackup** (catalog copy at branch LSN). The worker's DDL
resync/forward path records role **attributes and memberships** but not role **settings**
(`services/worker/src/resync.rs` header + the `$OWNER` query at `:377-442` carry no `pg_db_role_setting`).
A personal login created on a parent *after* a child already exists is forwarded to the child by
resync as a plain `CREATE ROLE` **without** the `ALTER ROLE … SET chelabase.managed_role`. On that
child the role exists **unmarked** → `CheckManagedRoleStatement` does not guard it → the database
owner can `ALTER ROLE … PASSWORD` / add LOGIN options to a role the platform believes it owns.

**Evidence.** `resync.rs:1-9,377-442` (attributes/memberships only), spec §12 "a child branch's
compute guards it too" (only basebackup-born children tested), `neon_ddl_handler.c:1180-1232`
(forward path touches attributes, not settings).

**Ruling.** The marker must travel on every path that recreates a personal login: either the
resync/forward emits the `ALTER ROLE … SET chelabase.managed_role='personal'` for a role the control
plane knows is a personal login, or the receiver re-applies the marker from control-plane state. Add a
live assertion: personal login created on the parent **after** a child branch exists is guarded on the
child. (Today there is no caller yet — §12 — but the marker-travel contract must be pinned before the
Members step builds on it.)

## RT4-5 (important) — storcon `/metrics` must accept Metrics *and* Admin, but storcon auth is equality-only

**Bug.** `storage_controller/src/auth.rs:check_permission` requires `claims.scope == required_scope`
(spec §7 relies on this to make `Metrics` ineligible elsewhere). But §7 also requires `/metrics` to
accept `metrics`, `admin`, **and** the server's own global scope. An equality check against a single
required scope can admit only one of them, so a naive `check_permission(claims, Scope::Metrics)` on
`/metrics` would **403 a legitimate `admin` scraper** (and vice-versa).

**Evidence.** `storage_controller/src/auth.rs:4-10`, spec §7 bullet "`/metrics` accepts `metrics`,
`admin` and the server's own global scope".

**Ruling.** The metrics/profile handlers check **membership in an allowed set** (`{Metrics, Admin}` for
`/metrics`; `{Admin}` for profiles on storcon), not a single-scope equality, on all three servers.
The §6/§7 exhaustive table must include `admin` → 200 on `/metrics`, and `metrics` → 403 on every
non-metrics route, for storcon as well as pageserver/safekeeper.

## RT4-6 (important) — the `:3081` internal token is `cloud_admin`-equivalent; its file is the whole boundary

**Issue.** `/grants` executes as compute_ctl's `cloud_admin` connection and `/extensions` /
`/refresh_configuration` are powerful. The only thing standing between a non-superuser backend and
`cloud_admin`-level action is that the backend cannot read `compute_ctl_internal.token`. §4.1 argues
no SQL path reads it, but any in-process C extension (RT4-1) or future `COPY PROGRAM`/`pg_read_server_files`
grant defeats that. The spec treats this token like the storage token, but it is strictly more
dangerous (storage token is tenant-scoped data the owner already reaches; the internal token reaches
`cloud_admin`).

**Evidence.** `grants.rs:14-47`, `server.rs:65-88`, spec §4 (secret) and §4.1 (file).

**Ruling.** (a) Record in the spec/plan that the internal token is a `cloud_admin`-grade secret. (b)
Narrow `/grants` to the exact grant shapes compute_ctl needs (reject arbitrary role/privilege/object)
so a leaked secret is less than full `cloud_admin`. (c) The §4.1 "no SQL path reads them" test must
run **on the image's actual extension set** (and, if platform is in scope, with pg_mooncake installed)
— not only core functions.

## RT4-7 (important) — a refresh push that omits `storage_auth_token` must fail closed, not silently drop a running compute's auth

**Failure.** §8 says "No token file (… `storage_auth_token` absent from the spec) means no password,
today's behaviour without auth." For a **running, authenticated** compute, a refresh push that (by a
bug or a spec-builder regression) omits `storage_auth_token` would, under that rule, make compute_ctl
write no file / an empty file → the extension fails closed → every new storage connection breaks,
while the control plane thinks it pushed a refresh. The "absent = auth off" rule is correct only at
first start with auth globally off, not on a refresh of an auth-on compute.

**Evidence.** spec §8 "Rule (fork)" bullets 1 and 3; `configure.rs` path reuses the same spec apply.

**Ruling.** Distinguish *auth globally off* (never had a token) from *token missing on a refresh of an
auth-on compute*. On the latter, refuse the reconfigure (or keep the existing file) rather than
dropping auth; the worker treats an omitted token on a refresh as a bug, not a downgrade. Add a RED
test.

## RT4-8 (important) — full compute spec (incl. `storage_auth_token`) is logged at debug by compute_promote

**Leak.** §4.1/§8 state the tokens are "never logged." But `compute_promote.rs:155`
`tracing::debug!("applied spec: {:#?}", new_pspec.spec)` serialises the whole spec, which still
carries `storage_auth_token` (U1 removes the *env var*, not the spec field — compute_ctl reads the
field to write the file). Promotion of a replica thus writes the live tenant token to the compute log
at debug.

**Evidence.** `compute_tools/src/compute_promote.rs:155`, `compute.rs:301` (`storage_auth_token` on
the spec), spec §8 tests ("token never appears in the compute … logs").

**Ruling.** Redact `storage_auth_token` (and any future secret spec field) from all `Debug`/`{:#?}`
spec logging, or drop that debug line. The "no token in logs" RED test must exercise the promote path,
not only start/reconfigure.

## RT4-9 (minor) — `Metrics` scope must be refused on compute_ctl's other authenticated routes by an explicit arm

The §7 (U4) change makes a `metrics` token decode on `:3080` (adds `ComputeClaimsScope::Metrics`).
Today non-Admin scopes fall through to the `compute_id` check (`authorize.rs:119-133`), so a `metrics`
token that happens to carry this compute's `compute_id` would pass **every** authenticated route, not
only `/metrics`. Ruling: the authorizer must add an explicit `Metrics` arm that admits **only**
`/metrics` and `/autoscaling_metrics` and 403s elsewhere, independent of `compute_id`. Covered by the
spec's test list — pin it as a hard requirement, not a `compute_id` side effect.

## RT4-10 (minor) — `INSTANCE_ID` / Hadron auth bypass and bearer-at-debug (item 4/5) confirmed; include the internal server

`authorize.rs:76-78` returns `Ok` (no auth) whenever `instance_id` is set; `:154` logs the bearer at
debug; `:162,:175` `unwrap()` a JWK `kid`. Item 4/§5 removes these for the external server. Ensure the
**new internal-server middleware** (§4) does not reintroduce an env/instance bypass and does not log
its bearer. Ruling: the internal middleware has no bypass path at all and never logs the secret; add a
unit test `INSTANCE_ID` set → internal routes still 401 without the bearer.

## RT4-11 (minor) — `--sync-safekeepers` must find the token-file path GUC before the first safekeeper connect

§8 relies on `postgres --sync-safekeepers` reading `neon.storage_auth_token_file` from
`postgresql.conf` via `CallExtMain` (`main.c:57-76`), and on `prepare_pgdata` writing the config
before the sync (`compute.rs:1564-1587`). Verify ordering holds for both the start path and the
pre-start sync, and that the walproposer in `--sync-safekeepers` mode reads the file (not an env var).
Ruling: a RED test asserts `--sync-safekeepers` runs with **no `NEON_AUTH_TOKEN`** in its environment
(`/proc/<pid>/environ`) and still authenticates — already in the spec; keep it mandatory and add the
config-ordering assertion.

## RT4-12 (minor) — token_file cache invalidation and refresh/connect race

§4.1's cache key is `{st_dev, st_ino, st_size, st_mtim}`; the writer always renames (new inode), so a
refresh is detected. Residual edges to test: (a) two refreshes within one `mtime` tick — a new inode
from the rename still forces a re-read, so OK, but assert it; (b) `fstat` on the **open** fd (not the
pre-open `stat`) records the inode actually read (spec says so — pin it, else a stat-old/open-new race
caches the wrong identity); (c) a read that returns an empty/oversized/invalid file drops the cached
value (no stale token survives). All are in the spec's `test_token_files.py`; keep them and add the
same-tick case.

## RT4-13 (minor) — proxy Valkey password file and streaming/W9 edges

- **Item 8:** `--redis-plain-password-file` must fail closed at startup on empty/missing file and on a
  URL that already carries a password; the error text must not contain the password
  (`proxy/src/binary/proxy.rs:176-178`). Straightforward; pin the no-leak assertion.
- **W9 streaming:** with `stream_commit_cb` using `txn->end_lsn` and no `stream_prepare_cb`, a streamed
  then `PREPARE`d transaction decodes at `COMMIT PREPARED` — correct, but add the explicit test that a
  `ROLLBACK PREPARED` of a streamed transaction writes nothing, and that an implicit abort after crash
  (`ReorderBufferAbortOld` → `stream_abort(…, InvalidXLogRecPtr)`, `reorderbuffer.c:3052-3053`) is
  treated as a top-level abort (entry dropped, nothing written). The spec's S4 per-xid hash must be
  freed on both abort and commit to avoid a leak across a long walsender session.

---

## Per-area checklist for implementer briefs

**Role guard C (`pgxn/neon/neon_ddl_handler.c`, D1 marker)**
- Marker lookup via `pg_db_role_setting (setdatabase=0, setrole=oid)`, any value marks (S15); a
  database-scoped row does not mark.
- Guard every §4.1-round-3 statement form on a marked role: `CREATE ROLE` IN ROLE/ROLE/ADMIN,
  `ALTER ROLE/USER` (incl. `… IN DATABASE … SET/RESET`), `ALTER GROUP ADD/DROP USER`, `RENAME`,
  `DROP ROLE`, `GRANT/REVOKE` grantees+granted; `CURRENT_USER`/`SESSION_USER` resolution.
- Refuse non-superuser `ALTER ROLE … SET/RESET chelabase.managed_role` and
  `GRANT/REVOKE SET|ALTER SYSTEM ON PARAMETER chelabase.managed_role` for any role.
- `ALTER ROLE ALL` (role NULL) is a no-op for the marker and already needs superuser — assert.
- Runs for DO blocks / PL/pgSQL / SPI (ProcessUtility_hook). Superuser (`cloud_admin`, migrations)
  untouched.
- **RT4-4:** the marker must reach child branches created *after* marking (resync/forward carries no
  role settings today) — pin the marker-travel contract and test on a child.

**compute_ctl (`http/server.rs`, `middleware/*`, `compute.rs`, `config.rs`, `configurator.rs`)**
- Internal server binds `127.0.0.1` (+ `::1` best-effort); bearer required, constant-time, no bypass
  (RT4-10), never logs the secret.
- Token files 0600, postgres-owned, `O_EXCL` temp + rename + dir fsync; paths written to
  `postgresql.conf` after spec `postgresql_conf`/`settings` (RT4-11).
- Storage token written at start and **first** on every reconfigure; no `NEON_AUTH_TOKEN` env for
  postmaster or `--sync-safekeepers`.
- **RT4-3:** refresh path must not be able to push a running compute to `Failed`.
- **RT4-7:** omitted `storage_auth_token` on a refresh of an auth-on compute fails closed, not
  downgrade.
- **RT4-8:** redact `storage_auth_token` from all `{:#?}` spec logging (`compute_promote.rs:155`).
- **RT4-9/§5:** explicit `Metrics` arm admits only `/metrics` + `/autoscaling_metrics`; missing/
  malformed/non-Bearer → 401; `INSTANCE_ID` bypass removed.

**Pageserver / safekeeper / storcon auth (`libs/utils/src/auth.rs`, each `auth.rs`, routes)**
- `Scope::Metrics` added; `check_permission` total (no `unwrap` on `tenant_id`, item 5).
- `/metrics` accepts **set** {Metrics, Admin, own global}; profiles accept {Admin, own global};
  `Metrics` refused everywhere else incl. libpq and safekeeper WAL service (RT4-5 — membership, not
  equality, esp. storcon).
- `/v1/status`, `/status`, `/ready`, `/v1/doc`, `/swagger.yml` stay public; auth-off unchanged.

**Logical decoding plugin (`contrib/chelabase_keys`, W9)**
- Set stream_{start,stop,abort,commit,change,truncate}_cb; no message/prepare cb.
- Streamed txn records per-relation `c` up to `max_tables` then `all`; `stream_commit` writes at
  `txn->end_lsn`; top-level abort/`ROLLBACK PREPARED`/DDL-only → nothing; subtxn abort keeps entry.
- Free the per-xid hash on both commit and abort (RT4-13); `COMMIT PREPARED` of a streamed txn emits
  the message.

**Slots / basebackup (`slot.c`, `xlog.c`, `basebackup.rs`, U16)**
- `SaveSlotToPath`/`ReorderBufferDropPtr` emit `neon-file:` only for `RS_PERSISTENT`;
  `LogDiscardedSlotFiles` after `XLogReportParameters`, primary-only, idempotent.
- basebackup counts only persistent, long-enough slot states toward `restart.lsn`; short states
  skipped with a warning, no panic (`basebackup.rs:491-493`); new `OFFSETOF_PERSISTENCY` constant.

**Proxy (`binary/proxy.rs`, `local_proxy.rs`, `compute_ctl/mod.rs`, item 8 + §4 callers)**
- `--redis-plain-password-file` fails closed on empty/missing/double-password; no password in logs
  or `Debug` (RT4-13).
- `local_proxy`/testing `Local` backend read `--compute-ctl-token-file` before each call; missing/
  invalid → no header → refused.

**Chelabase worker / realtime / compose (`services/*`, `crates/*`, `infra/compose`, `tools/cli`)**
- Storage-token refresh loop: per running compute under `lock_endpoint`, `skip_pg_catalog_updates`,
  412 retried next pass, failures counted (RT4-3 lightweight path), deploy order vs 24 h TTL (RT4-2).
- `tenant_token` 24 h exp + iat; metrics token no-exp + iat (S17); `write_storage_auth` keeps an
  existing metrics token, the mint command rotates.
- W31: realtime CN `chelabase-realtime`, `clientcert=verify-full`, CIDR = realtime+runner, reject
  lines everywhere else; realtime dials `replication_host` (control `.12`) for the replication session
  only, check pools stay on the compute network; cert renewed by the worker sweep.
- `compute-network-e2e.sh`: no `:3081` listener, 401 on `:3080` `/metrics`+`/autoscaling_metrics`,
  `known_gaps` emptied.
- **RT4-1 (platform):** drop or patch pg_mooncake (restore the commented-out non-superuser /
  `disabled_filesystems` guards) and pin `duckdb.postgres_role`/`duckdb.disabled_filesystems`; confirm
  the dev image ships none of pg_duckdb/pg_mooncake/anon. **USER** scope decision.
