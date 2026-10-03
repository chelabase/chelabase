# Login Roles, revision 2: Implementation Plan (roadmap step 6a)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** projects run their existing migrations unchanged through Cheladb's proxy. `CREATE ROLE … LOGIN PASSWORD`, `ALTER ROLE … PASSWORD/RENAME`, `DROP ROLE`, `GRANT`, `CREATE DATABASE` and friends work. Studio and the API manage the same roles, and a protected-branch flag exists.

**Architecture:**
- **The branch's `pg_authid` is the source of truth** for every login role except the owner. The compute spec lists only the owner again.
- **`neon_roles` is the proxy's copy** of each role's SCRAM hash, plus bookkeeping (`origin`, `login`, `level`, an optional sealed password).
- Copies are kept current in three ways:
  - **Neon's DDL forwarding:** `neon.console_url` → the orchestrator's internal listener, `PATCH /compute/api/v2/computes/{id}/ddl`. It runs one metadata transaction per user transaction, and its refusals fail the user's transaction.
  - **A resync from the catalog**, run by the worker under the endpoint lock after restore, reset-to-parent, branch create and compute re-creation, and periodically in reconcile.
  - **The worker's own API writes** (create, reset, delete). These run through `admin_sql` with forwarding off.
- **`rest`** refuses a secret key whose role has become owner-level.

**Tech stack:** as revision 1. Rust 2024 (axum 0.8.9, sqlx 0.9.0, tokio, async-nats 0.50.0, moka), Postgres 17.5 on the fork's compute image, utoipa 6.0.0 + utoipa-axum 0.3.0, orval 8.38.0 (`@cheladb/management-client`), React Studio on TanStack Query 5.104.0 (vitest). **No new dependencies.**

**Spec:** `docs/superpowers/specs/2026-10-01-login-roles-design.md` (revision 2, approved 2026-10-02). Brainstorm: `2026-10-01-login-roles-v2-brainstorm.md`. Research: `2026-10-01-sql-roles-research.md`.
- The spec wins on behaviour. This plan wins on names and interfaces, and on the rulings under Status.
- Revision 1's plan (`2026-10-01-login-roles.md`) is superseded. Its Status records what was built.

## Status

- **Current (2026-10-02): MERGED into `main` (`c43a56d`) at the user's word. After the merge, `main` has 940/940 tests passing, clippy clean, Studio typecheck and 298/298. Step 6a is done.** The open follow-ups (below) go to a small chore branch later. Tasks 1–12, the Playwright pass, the end-of-step review, its fix batch and one follow-up fix are all done and re-reviewed clean. Final verification:
  - `cargo nextest run --workspace` 940/940, clippy (`-D warnings`) clean, `just check`, Studio build and tests, `live_db` 80/80;
  - live: `roles_live` 29/29 (plus a 4-test rerun after the last fix), `rest_live` 24/24, and the `role-e2e`, `data-e2e`, `neon-e2e` and `branch-e2e` runs.
  - The deps stack and `just dev` are still running from `.worktrees/login-roles`.
  - After the merge, the fork session needs the Compose stack once (it will ask for a slot first).
- **Pre-flight scan (2026-10-02):**
  - **Static (all confirmed):**
    - The ddl route fits the internal listener next to the spec route (`services/orchestrator/src/internal/mod.rs`, `build_internal_router`).
    - Compute-token auth can be reused from `internal/spec.rs` (`compute_spec`: SHA-256 of the bearer compared in constant time with `neon_endpoints.compute_token_sha256`, with a dummy digest for unknown ids).
    - The spec's settings are a `settings` list in `config_response` (`crates/cheladb-neon/src/spec.rs`). `neon.console_url` is one more `setting(…, "string")`. Its callers are `internal/spec.rs::endpoint_config` (boot and `hooks.rs`) and `services/worker/src/reset.rs` (`/configure` push).
    - `admin_sql` runs `psql` over `docker exec` with an `env` list (`PGCONNECT_TIMEOUT=5`, `crates/cheladb-neon/src/backend.rs` `run_exec`), so `PGOPTIONS=-c neon.forward_ddl=off` goes there. `run_exec` has no other caller.
    - The proxy flag is at `infra/compose/docker-compose.yml:362`, and `infra/neon/tests/run.sh:87` asserts its exact text.
    - In the fork, `max_roles` only multiplies the role cache's total capacity (`size × max_roles`, `proxy/src/cache/project_info.rs:86-105`). Raising it is just the flag.
    - `rest` switches roles in `PREAMBLE_SQL` (`services/rest/src/exec.rs:78`), called from `Engine::query` and `Engine::role_schema`. The owner-level check goes right before `preamble`, in the same transaction. `moka` is already a dependency there.
    - The orchestrator has no Valkey client and no `CHELADB_CONTROL_PLANE_URI` today: only the worker reads them (`services/worker/src/config.rs`). `cheladb-neon` (with `ProxyCache`) is already an orchestrator dependency.
    - `just wt-add <branch> [base]` defaults the base to `main`, so step branches pass `login-roles` as the base.
  - **Live, on the fork image `compute-node-v17:fa6917ea6ba6-dev` (Postgres 17.5), with a throwaway database (deleted afterwards; no compute left):**
    - `neon.forward_ddl` exists (`on`; context `superuser`). `neon.console_url` exists, context **`postmaster`**, empty by default.
    - `NEON_CONTROL_PLANE_TOKEN` is in the postmaster's environment.
    - **Forwarding works end to end.** A copy of the compute, booted with its real spec plus `neon.console_url` pointing at a local Python listener, sent one `PATCH` per transaction to `/compute/api/v2/computes/<ep>/ddl` with `Authorization: Bearer <the compute token>` and `Content-Type: application/json`. Round trip about 0.17 s. Payloads seen:
      - `CREATE ROLE app_rw LOGIN PASSWORD '…'` → `{"roles":[{"op":"set","name":"app_rw","password":"plain-secret-123","encrypted_password":"SCRAM-SHA-256$4096:…"}]}`
      - `CREATE ROLE grp NOLOGIN` → `{"roles":[{"op":"set","name":"grp"}]}` (no password keys)
      - `BEGIN; CREATE ROLE app_a …; ALTER ROLE app_a RENAME TO app_b; COMMIT` → one entry `{"op":"set","name":"app_b","old_name":"app_a","password":"x1","encrypted_password":"SCRAM…"}`
      - a SCRAM literal → `password` and `encrypted_password` are both the literal
      - an MD5 literal → both `md5…`
      - `DROP ROLE IF EXISTS ghost` → `{"roles":[{"op":"del","name":"ghost"}]}`
      - `CREATE DATABASE extra_db` → `{"dbs":[{"op":"set","name":"extra_db","owner":"postgres"}]}`
      - **`ALTER ROLE … NOLOGIN`, `… CREATEDB`, `… BYPASSRLS` and `GRANT grp TO app_b` each send `{"roles":[]}`**
      - `ALTER ROLE postgres PASSWORD …` → a `set` for `postgres` with both passwords
      - `compute_ctl`'s own start-up sent nothing.
    - **A 409 with a plain-text body fails the transaction:** `ERROR: Received HTTP code 409 from control plane: <body>`, and the role isn't created.
    - **A refused `DROP DATABASE` leaves the database invalid** (`datconnlimit = -2`). Postgres removes the files before pre-commit (`dbcommands.c:1805-1848`), and `compute_ctl` drops invalid databases at start. See the user's answer Q1 (the drop is accepted and mirrored).
    - **Control plane unreachable:** the DDL fails after about 5 s with `Failed to perform curl request: Failed to connect to host.docker.internal port … : Couldn't connect to server`, and nothing is created. Non-role DDL is unaffected. See the user's answer Q2.
    - **PG17 ADMIN:** a role `postgres` creates gets a `pg_auth_members` row (member `postgres`, grantor `cloud_admin`) with `admin_option = t`, **`inherit_option = f`, `set_option = f`**. So the owner can alter, rename, drop and grant it, but can't `SET ROLE` to it or inherit its privileges (Postgres's default `createrole_self_grant = ''`).
    - **`GRANT neon_superuser TO x` as `postgres` is refused** ("Only roles with the ADMIN option on role "neon_superuser" may grant this role"). As `postgres`, these are also refused: `ALTER ROLE … SUPERUSER`, `ALTER ROLE cheladb_admin PASSWORD` and `DROP ROLE authenticator`. `ALTER ROLE x CREATEDB` and `ALTER ROLE x BYPASSRLS` succeed.
    - **The resync query** (Task 1 section) was syntax- and format-checked on the metadata Postgres 17 (with `pg_monitor` standing in for `neon_superuser`). It prints `r|<hex name>|t|<hex hash>|f`; an empty hash prints as nothing.
    - **On a compute:** the system `postgres` database is owned by `cloud_admin` (oid 10). The branch's database is owned by `postgres`. The `postgres` role has `CREATEROLE`, `CREATEDB`, `BYPASSRLS` and `REPLICATION`, so it reads as owner-level.
    - **The new migration** (Task 1, as written below) applied cleanly on this machine's dev metadata database (which has revision 1's migration) inside `BEGIN … ROLLBACK`. There were no role rows to map then, so Task 1's `migration_maps_revision_one_rows` covers the mapping.
    - **Not checked live:** a refused `CREATE DATABASE` (the database cap) leaving nothing behind. Task 11 tests it.
- **Rulings (made while planning; mechanical, each with its cost if wrong):**
  - **R1. Route path:** `PATCH /compute/api/v2/computes/{id}/ddl`, as the spec's Part 2 says. The brainstorm's `/internal/v1/…` was an earlier draft. *Cost: none.*
  - **R2. Forwarding off through `PGOPTIONS=-c neon.forward_ddl=off`** in `run_exec`'s environment, not a `SET` prepended to the SQL. It covers every statement of every `admin_sql` call, and the fakes' SQL journals stay unchanged. `cloud_admin` is a superuser, so a `superuser`-context setting is allowed at connection start. *Cost: none; Task 11 asserts `SHOW neon.forward_ddl` = `off` through `admin_sql`.*
  - **R3. The receiver's lock:** `SET LOCAL lock_timeout = '2s'`, then `SELECT … FROM neon_endpoints WHERE id = $1 FOR UPDATE`; a timeout answers 503. The compute's curl timeout is 3 s, and its retries count on an idempotent receiver. *Cost: a DDL during a long metadata write gets a "try again".*
  - **R4. Empty deltas** (`{"roles":[]}`, or no `roles`/`dbs` keys), seen live for `ALTER ROLE … [NO]LOGIN/attributes` and `GRANT`, answer 200 at once, with no transaction. *Cost: none.*
  - **R5. The cap counts copies** (rows of origin `sql` or `api` with a SCRAM verifier), whether or not the role has `LOGIN`. The receiver can't see `LOGIN` (Neon's payload has no field for it). *Cost: a `NOLOGIN` role with a password counts.*
  - **R6. A new copy records `login = true`** until a resync observes otherwise. *Cost: Studio may show login for a `CREATE ROLE x PASSWORD …` (no LOGIN) until the next resync.*
  - **R7. Resync races:**
    - The resync reads `roles_version` before the catalog, and writes only if it's unchanged (otherwise it re-reads, at most 3 times).
    - It also leaves alone rows the receiver touched in the last 10 s (`forwarded_at`). The receiver answers before the user's commit is visible, so a catalog read in that gap would wrongly drop a fresh copy.
    - *Cost: a phantom forward is repaired one resync later.*
  - **R8. What the resync and `branch_databases` ignore:**
    - roles with oid < 16384, `pg_*`, the system roles, and the endpoint's owner (spec-managed: `compute_ctl` applies its copy);
    - template databases and invalid ones (`datconnlimit = -2`). The system `postgres` database (owned by a bootstrap role, `datdba < 16384`) is listed with origin `system` (user's answer Q8).
    - *Cost: none.*
  - **R9. MD5 roles are kept as rows with a NULL verifier,** so Studio can list them as `md5_unusable`. The proxy lookup treats a NULL verifier like an unknown role. They come only from the resync, since the receiver refuses MD5. *Cost: one nullable column.*
  - **R10. Metadata shape:**
    - `neon_roles.kind` is replaced by `origin` (`owner|sql|api`);
    - `privileged` is renamed `api_owner_level` (only meaningful for `api` rows: re-create with `neon_superuser`);
    - `scram_verifier` becomes nullable (R9);
    - a new `forwarded_at` column (R7).
    - *Cost: every revision-1 query on `kind` is rewritten (Task 1).*
  - **R11. Rollout re-creation:** `neon_endpoints.compute_settings_version` (default 0) against the worker constant `COMPUTE_SETTINGS_VERSION = 1`. Reconcile plans `UpgradeCompute(branch, ep)` for ready `normal` branches below it. `recreate_compute` stores the constant. *Cost: one column; one restart per compute at rollout.*
  - **R12. Valkey publish failures** in the receiver are logged, and the reply is still 200. A stale proxy cache only refuses logins and never lets a wrong password in; that was checked in step 3. *Cost: up to 1 min of refused logins if Valkey is down.*
  - **R13. The resync gives `postgres` ADMIN on every `api` role that lacks it.** Revision-1 roles were created by `compute_ctl` as `cloud_admin`, without ADMIN for the owner. The grant is `GRANT "<r>" TO <owner> WITH ADMIN OPTION, INHERIT FALSE, SET FALSE`, as `cloud_admin`. *Cost: one column in the resync query.*
  - **R14. Role names on reset, delete and reveal accept any Postgres name** (1–63 bytes, no NUL), since SQL can make `"My Role"`. Create keeps revision 1's strict rule. Every SQL use quotes identifiers by doubling `"`, and `uri` percent-encodes the user and the database. *Cost: none.*
  - **R15. Strength estimate:** bits = (length in chars) × log2(pool). The pool adds 26 for any lowercase letter, 26 for uppercase, 10 for digits, and 33 for anything else (non-ASCII included). A password needs at least 12 chars and at least 60 bits. *Cost: a different estimator is a one-function change.*
  - **R16. Live tests make roles owner-level without `GRANT neon_superuser`,** because `postgres` can't grant it (live). They use `ALTER ROLE x BYPASSRLS`, and an indirect membership via an API owner-level role (`GRANT api_admin TO x`, which `postgres` may do with its ADMIN). *Cost: none.*
  - **R17. The protected flag:**
    - `PATCH` works only on `normal` branches (a backup → 409 `backup_branch`), in any status but `deleting`/`deleted`.
    - Delete and reset-to-parent check it atomically: the claim's `UPDATE` gains `AND NOT protected`, and a zero-row result is re-read to tell `branch_protected` from `branch_not_ready`.
    - Deleting a database checks it atomically in `begin_delete`.
    - The parent's flag is read when the worker records the child's endpoint.
    - *Cost: none.*
  - **R18. The orchestrator reads `CHELADB_CONTROL_PLANE_URI`** (same default and check as the worker) to build `neon.console_url` in the specs it serves. It also reads `CHELADB_VALKEY_URL` for the receiver's cache messages. *Cost: none.*
  - **R19. Data keys follow a rename:** every `data_api_keys` row of the branch whose `role` is the old name, revoked ones included. *Cost: none.*
  - **R20. `max_roles=64`** (50 copies + the owner + headroom). It only scales capacity. *Cost: none.*
  - **R21. Studio's "Blocked" comes from `listRoles`' `blocked_keys`;** the data keys API doesn't change. *Cost: Studio's keys tab loads the roles list too.*
  - **R22. Proxy re-invalidation about 1 s later** is a spawned task (`tokio::spawn` + `sleep(1 s)`) in the receiver and in the worker. A failure is logged. *Cost: none.*
  - **R23. The system `postgres` database doesn't count toward `CHELADB_BRANCH_DATABASE_CAP`** (the owner can't drop it, so counting it would only shrink the cap). The cap counts `branch_databases` rows of origin `main` or `sql`. *Cost: none.*
  - **R24. The reveal with no `database` while the main database is missing** still answers the recorded main database's name (the connection then fails at Postgres), so a client can see what to re-create. *Cost: a connection string that doesn't work until `CREATE DATABASE <main>`.*
- **User's answers (2026-10-02):**
  - **Q1. Dropping the main database: accept now, fork later.**
    - The drop is accepted and mirrored: the main row leaves `branch_databases`.
    - Studio shows "main database missing" on the branch, and the data API answers its existing unavailable error until a database with that name exists again. The resync and the receiver then restore the `main` row.
    - Renaming main stays refused (renames are transactional, so the refusal is clean).
  - **Q2. The control-plane-down text: accept now, fork later.** 6a keeps Neon's curl text and explains it in the README.
  - **Q3. One wide owner-level definition everywhere:** superuser, CREATEROLE, CREATEDB, BYPASSRLS, REPLICATION, or `neon_superuser` at any depth. It applies to key creation, the Level badge, Blocked, and `rest`'s check.
  - **Q4. The database cap counts every tracked database including main.** Default 20, allowed 1..=1000. The system `postgres` database isn't counted (R23).
  - **Q5. The reveal** answers `password: null`, a new `password_state: "generated" | "set_in_sql" | "md5_unusable"`, and a URI without a password.
  - **Q6. Strength on plus a pre-hashed SCRAM password in self-hosted mode → accepted.** Strength applies to plain passwords only.
  - **Q7. The 11 refusal texts are approved as listed in Task 5,** except text 10, which becomes `the branch's main database "<d>" can't be renamed` (a drop is now allowed).
  - **Q8. The system `postgres` database is listed with origin `system`.**
    - It's offered in the connection picker (Studio and `GET …/databases`) and isn't counted in the cap.
    - The receiver refuses nothing new for it; Postgres already stops the owner from dropping or renaming it.
  - **Q9. `createrole_self_grant = 'set, inherit'` is set in every compute's settings** (Task 3). Task 11 tests that `ALTER TABLE t OWNER TO app` and `SET ROLE app` work as `postgres` right after `CREATE ROLE app`.
- **Fork tasks to queue for the fork session (6a doesn't wait for them):**
  - **F1. Refuse a drop of a protected database before it runs.** Neon's DDL handler checks a `DROP DATABASE` of a protected name (the branch's main database) before Postgres runs it, so the drop is truly refused with no data loss.
  - **F2. Our own control-plane-down message.** Replace the curl error with "role changes are unavailable right now; try again", without the internal address.
- **Done:**
  - Task 2: `1bf8ede` (review clean, haiku), merged `c324500`.
  - Task 3: `5bf94d9` (no review, per plan), merged `225fe47`.
  - Task 1: `a572277` (review clean, sonnet), merged `4f84f17`. The `spec.rs` conflict with Task 3 was resolved in favour of Task 3's `RoleSpec` without `privileged`.
  - Task 7: `b6ca40b` (review clean, haiku), merged `98fa02e`. Started early, since it needs only Task 2.
- **More done (wave 2):**
  - Task 6: `6989e65` (review clean, sonnet), merged `035db5e`.
  - Task 5: `0dea3dd` (review clean, sonnet), merged `52d90bf`.
  - Task 10: `6294283`..`7235d04` (one fix round: it had edited "Resume here" and invented scope for 6c; no review, per plan), merged `c0174dd`.
  - Task 4: `0aa3a71` (review clean, sonnet), merged `98e6428`.
  - Task 9: `c29eff6` (no review, per plan), merged `643acbe`.
  - Task 11: `af885b8` (bug fix: provision runs a resync, so a new database lists the system `postgres` database, Q8), `f85e7e1`, `704dc30`. Runs, one at a time:
    - `roles_live` 27/27 (214 s), `rest_live` 24/24 (137 s), `neon_live` 1/1, `branch_live` 1/1;
    - `just role-e2e` (53 s), `neon-e2e` (10 s), `data-e2e` (35 s) and `branch-e2e` (32 s) all passed.
    - Timings: provision about 3.8 s, `CREATE ROLE` 51 ms, `ALTER ROLE … PASSWORD` 39 ms, `CREATE DATABASE` 0.49 s, resync 54 ms, a child branch about 2 s, a restore about 2.9 s, and the 403 for a role that became owner-level about 30 s.
  - **User's answer N1 (2026-10-02): an owner-level API role matches the owner.** It's created with CREATEROLE, CREATEDB, BYPASSRLS and REPLICATION, granted `neon_superuser`, and repaired by the resync. Before this, live tests showed it got only `neon_superuser`, so it couldn't create roles or databases and didn't bypass RLS.
  - **Fork F1/F2 are published (fork Task 11d), and 6a picks them up in Task 12.** Dropping the main database is now refused at Postgres. Q1's "accept the drop" no longer applies, though the receiver keeps its mirror of a drop.
  - Task 12 (added): `48dfad7` (N1: owner-level attributes, also repaired by the resync), `17fe5ec` (F1: `neon.protected_databases`), `05895f2` (E8 text), `81ad1f7` (image pin), `8f391ed` (live tests), `bb32c04` (README and spec). Unit tests 925/925, clippy clean, `roles_live` 27/27 (335 s), and the role, neon, data and branch e2e runs passed on the new image. `COMPUTE_SETTINGS_VERSION` 2 rolled out live: reconcile re-created an old compute about 12 s later.
  - Playwright pass (sonnet, over CDP with locally extracted libraries): 6/6 checks passed with no console errors, and the test database was deleted. Screenshots are in the job's scratch folder.
  - End-of-step review (opus): "with fixes", with 0 Critical, 5 Important and 7 Minor findings:
    - I1: role names weren't encoded in URL paths;
    - I2: key creation didn't refuse every owner-level role;
    - I3: protected children only covered the parent's copies;
    - I4: psql error excerpts could carry verifiers;
    - I5: docs and `.env.example` were wrong.
  - The fix batch (`d9003b0`..`8f33788`) fixed I1–I5, M1, M2, M4–M7 and the cheap triage items:
    - M2 is built as two-hop renames through a temporary name, which keeps swaps safe;
    - M7 found that `PASSWORD ''` fails inside the compute, so nothing is forwarded.
  - The scoped re-review cleared all of them and found one new Important: a drop plus a rename onto the dropped name bypassed `role_in_use`. It's fixed in `fca0934` (refused with the `role_in_use` text; swaps still pass) and re-reviewed clean.
  - Task 8: `d15e0d0`..`3631eb5` (the review found one Important issue, fixed in round 1; the re-review is clean), merged `0627a40`.
- **Rulings during execution:**
  - **E1.** The worker test `roles::tests::create_runs_in_order` stays red on `login-roles` until Task 4 rewrites `roles.rs` (it asserts revision-1 spec rows that no longer exist). *Cost: one known-red test during wave 2.*
  - **E2.** Task 1's `begin_ddl` locks the branch row before the endpoint row, as the repo's lock order does (`replace_owner_secret`). The 2 s endpoint lock timeout is kept. *Cost: none; reviewed.*
  - **E3.** The receiver refuses reserved names on every forwarded role `set`, not only on sets that carry a password (spec Part 2). *Cost: a legitimate forwarded entry naming a reserved role would fail; Task 11 checks this.*
  - **E4.** `DdlTx::has_database` was added to `cheladb-metadata` so the database cap can tell a create from an owner change. *Cost: none.*
  - **E5.** The receiver's first proxy publish is spawned rather than awaited, so a slow Valkey can't outlast the compute's 3 s curl timeout. *Cost: none.*
  - **E6.** The resync reads the catalog through the system `postgres` database. The main database may be dropped (Q1), and `pg_authid`/`pg_database` are cluster-wide. *Cost: none.*
  - **E9.** Task 8 records `compute_settings_version` right after the bootstrap, before the resync, which overrides the plan's order. Otherwise a resync that kept failing would delete and re-create the compute on every pass, and R11 means one re-creation. *Cost: a compute whose resync fails counts as upgraded and is retried only by a plain `Resync`.*
  - **E10.** A protected parent's child gets a fresh, revealable password for every non-owner role with a password in the child's own catalog (MD5 and passworded `NOLOGIN` included), not only for the parent's copies. *Cost: one more catalog read per protected child.*
  - **E11.** The regression the fix batch introduced (drop plus a rename onto a key's role) got one targeted fix round instead of being carried to the merge, because it touches security. *Cost: one short extra round.*
  - **E12.** New refusal text for key creation: "the role `<r>` is owner-level (owner-level roles can't back a secret key; use a plain role)". *Cost: wording.*
  - **E7.** The worktrees share a target directory, so another worktree's build of a workspace crate can look "Fresh" here. Touch the sources before verifying. *Cost: none.*
- **Wording and choices: approved by the user 2026-10-02 ("those look good"); the user asked not to pause until the merge.** The owner `PASSWORD NULL` case gets a dedicated text in the final fix batch: "the branch owner's password can't be removed; reset it instead" (coordinator's ruling E8).
  - Receiver: an owner `PASSWORD NULL` answers "the branch owner "postgres" can't be dropped or renamed", which is the wrong text for that action. A dedicated text is needed.
  - Studio (Task 9) wording:
    - column headers "Origin", "Login" and "Password", with Login values "Yes"/"No";
    - the connection panel's database picker is labelled "Database", and the system database appears as "postgres (System)";
    - buttons "Protect", "Unprotect" and "Confirm protect".
  - Studio (Task 9) behaviour:
    - Unprotect has no confirm; only Protect does;
    - picking the main database sends no `database` parameter;
    - a protected main branch keeps main's own disabled reasons rather than "Unprotect the branch first".
- **Deferred minors:**
  - T1: `DdlTx::rename_database` keeps the row's origin, so a `sql` database renamed to the main name stays `sql` until the next resync.
  - T1: `upsert_sql_copy` on the owner's name is a no-op, but it still bumps `roles_version`.
  - T1: the lookup of the recorded main database is written twice (`begin_ddl` and `set_databases`).
  - T3: `spec.rs` has two near-duplicate tests (`plain_roles_carry_no_privileged_key_legacy`, `roles_carry_no_privileged_key`).
  - Pre-existing (also fails on `edb6689`): the `just neon-test` check "smoke passes on curl's own error".
  - T6: the non-ready path of `delete_branch` reads `protected` from an earlier load, and `begin_branch_delete` has no protected guard (a small window).
  - T6: `list_branches` makes one `branch_databases` query per ready branch; a reveal of a `Generated` row with no sealed password answers `password: null` with `generated`; there's a weak password check in one api test, and a comment mentions an untested 400 case.
  - T5:
    - a rename plus `PASSWORD NULL` in one transaction looks like a plain rename (Neon's payload), and the resync repairs it;
    - `reseal_password` reads the old row a second time;
    - the ddl route reads the body before auth, so it needs a `DefaultBodyLimit`;
    - some tests are missing (owner publish on a re-password; an unopenable sealed password on a rename over HTTP; a rename then a drop over HTTP), and the imports in `internal/mod.rs` are out of order.
  - T4:
    - after a failed reset of an `sql` row, the next resync silently reverts it, although `not_applied` promised a retry;
    - one failing repair aborts the whole resync;
    - psql stderr (its `LINE n:` context, which can contain a verifier) can reach the logs and branch errors;
    - a partial owner-level create (the `GRANT neon_superuser` failed) is never repaired;
    - small tidy-ups.
  - T9: `DatabaseActions` lists branches once per ready database row, and `DataApiKeys` makes an extra roles request.
  - T8:
    - `admin_sql` stderr (its `LINE` context) can carry a verifier into `branch.error`;
    - the protected flag is read in a separate query from the endpoint insert;
    - MD5 rows of a protected parent keep the parent's MD5 hash in the child;
    - a sealed password that won't open is dropped with a WARN, and the role then shows `set_in_sql`.
  - T11:
    - a role created less than 10 s before a restore stays listed until the next periodic resync (the `forwarded_at` guard);
    - for the README: Q9's `ALTER TABLE … OWNER TO` also needs `GRANT CREATE ON SCHEMA public`, and a raw `pg_dumpall` `NOSUPERUSER` line for the owner is refused.
  - T12: provisioning is slower on the new image (3.9–8.2 s, against 3.7–4.1 s), not investigated. The live rename check ends the data API's pooled session before it renames.
  - The pre-existing `neon-test` failure has a known cause: port 1 times out on this machine instead of refusing, so curl reports error 28, not 7.
  - **Follow-ups that stay open after the end-of-step triage** (the full table is in the review; not fixed in 6a):
    - one failing repair aborts the whole resync;
    - `list_branches` makes one query per branch, and Studio makes extra requests per row;
    - a role created less than 10 s before a restore stays listed until the next periodic resync;
    - the protected flag is read in a separate query from the endpoint insert;
    - a rename plus `PASSWORD NULL` in one transaction looks like a plain rename;
    - an unopenable sealed password shows `set_in_sql`;
    - `branch_databases` has no `forwarded_at` guard (a window of milliseconds; repaired automatically);
    - provisioning is slower on the new image;
    - the pre-existing `neon-test` curl check;
    - the revealed URI's `sslmode=verify-full` needs the dev CA in psql.
  - These follow-ups are handled by the follow-up chore `docs/superpowers/plans/2026-10-02-login-roles-followups.md` (branch `login-roles-followups`).
- **Note:** Task 1's live tests applied migration `20261004000001` to this machine's dev metadata database.
- **Running:** `just dev` from `.worktrees/login-roles` (stop with `just dev-down` there). Deps stack up.

## Global Constraints

(From the spec, verbatim where it states them.)

- **Git:**
  - Branch `login-roles`, worktree `<repo root>/.worktrees/login-roles`.
  - Parallel tasks use `just wt-add login-roles-<suffix> login-roles`, are merged back `--no-ff` and are removed.
  - Commit with `git commit`. Messages end with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>` and `Claude-Session: https://claude.ai/code/session_01A7WRiGPQKSBhJcqDaiZoUL`.
  - Subagents don't merge or push. The coordinator pushes after each checkpoint.
- **Formatting:** never run `cargo fmt` on the workspace; use `rustfmt --edition 2024 <file>` only on files you create.
- **Secrets:** never overwrite `.cheladb/secrets/secrets.key`, the gateway key pair, the cookie key or the compute-JWT key pair.
- **Strict TDD:** a failing test run (RED) before any implementation, in every task. Reports show the RED and GREEN commands and output. A compile error counts as RED only when the missing item is the code under test.
- **Dependencies:** none new.
- **Compute image:** planned unchanged (`compute-node-v17:fa6917ea6ba6-dev`). Task 12 pinned the fork's F1/F2 image `ghcr.io/chelabase/neon-compute-v17:b2402faa2394-dev@sha256:7816c6b0543d9ea83a9c925ba5da733d39a38e6845cae6888ecd297123859e43`.
- **Caps and settings.** The orchestrator (receiver) and the worker (API creates) both read these, through one parser in `cheladb-types`:
  - `CHELADB_BRANCH_ROLE_CAP`: **50 login roles per branch** by default, allowed **0..=500**. Group roles without a password don't count. A refusal names the setting.
  - `CHELADB_BRANCH_DATABASE_CAP`: default **20**, allowed **1..=1000**, a per-branch limit on Postgres databases. It counts every tracked database including main, but not the system `postgres` database (Q4, R23). The existing branch cap is unchanged.
  - `CHELADB_PASSWORD_STRENGTH`: **off in self-hosted mode** (opt-in `on`). **Platform mode forces strength on and refuses pre-hashed passwords whatever the setting.**
- **Password rules per mode:**
  - `encrypted_password` must be SCRAM. **MD5 is refused in both modes**: "MD5 passwords can't be used through Cheladb; set a plain password".
  - A pre-hashed `password` (the literal is a SCRAM verifier) is **accepted in self-hosted mode (even with strength on: strength applies to plain passwords only, Q6) and refused in platform mode**.
  - Strength (platform, or self-hosted with `CHELADB_PASSWORD_STRENGTH=on`): **at least 12 characters and an estimated 60 bits of entropy** (character classes × length, R15). The error names the rule.
  - No password (a group role) → no copy. `PASSWORD NULL` removes an existing copy.
- **Reserved names** (refused by the receiver; `RESERVED_ROLE_NAMES` in `crates/cheladb-types/src/role_name.rs`): `postgres` (except as the endpoint's own owner), `cloud_admin`, `neon_superuser`, `authenticator`, `anon`, `authenticated`, `cheladb_admin`, `public`, `none`, and any `pg_*` name. **API create** keeps the strict name rule: 1–63 bytes, `^[a-z_][a-z0-9_]*$`.
- **The owner rule:** the owner `postgres` gets **its own password on every branch** (step 5's rule, unchanged). Every other copied role keeps the parent's password unless the parent is **protected**, whose children get a fresh generated, revealable password for every copied login role.
- **Error codes:**
  - New `ErrorCode::BranchProtected` = `branch_protected` (409).
  - Revision 1's role codes stay: `invalid_role_name`, `reserved_role_name` (422), `role_exists`, `role_cap_reached`, `role_in_use` with `keys` (409), `role_privilege_mismatch` (500, now unused by new code but kept in the enum).
  - The data API's new error is `role_not_allowed` (403): "the role `<name>` of this key can bypass row-level security; make it plain or use another role".
- **Routes:**
  - Internal: `PATCH /compute/api/v2/computes/{id}/ddl` (compute-token auth). 200 accepts; a refusal is a 4xx with a one-line `text/plain` body.
  - Platform API (`Auth::Management`, `Cors::Studio`, `Scope::Api`), all under `/v1/databases/{id}/branches/{branch_id}`:
    - `GET …/roles`, `POST …/roles`, `POST …/roles/{role}/reset-password` (204), `DELETE …/roles/{role}` (204);
    - `GET …/connection?role=&database=`;
    - **new** `GET …/databases` (`listBranchDatabases`) and **new** `PATCH …` (`updateBranch`, body `{ "protected": bool }`).
  - Branch responses carry `protected`. No database create or delete through the API in 6a.
- **Reveal shape (Q5):** `password: null` when not revealable, `password_state: "generated" | "set_in_sql" | "md5_unusable"`, and `uri` without a password.
- **Postgres databases (Q1, Q8):** `branch_databases.origin` is `main | sql | system`. Dropping main is accepted and mirrored (its row leaves; it comes back when a database of that name exists again). Renaming main is refused. The system `postgres` database is listed and pickable.
- **Compute settings:** `neon.console_url` and `createrole_self_grant = 'set, inherit'` (Q9) on every compute.
- **Owner-level, one definition everywhere** (resync `level`, the Level badge, Blocked, key creation, `rest`; Q3): superuser, `CREATEROLE`, `CREATEDB`, `BYPASSRLS`, `REPLICATION`, or `neon_superuser` membership at any depth.
- **Compose is one stack per machine:** live tests and e2e scripts run one at a time. If tests fail binding `127.0.0.1:0` (the small WSL port range), wait a minute and rerun.

## What changes from revision 1

| Revision 1 (built) | Revision 2 |
|---|---|
| Login roles are spec rows; `compute_ctl` creates them; `privileged:false` in the spec | **Removed.** The spec lists only the owner. `RoleSpec.privileged` and `NeonRole.privileged` are gone. The worker creates roles in SQL as the owner. |
| `enforce_plain_roles`, `Action::EnforcePlainRoles`, the demotion check, `role_privilege_mismatch` replies | **Removed** (privileges given in SQL stick). Replaced by the resync (`Action::Resync`) and `rest`'s check. |
| Fresh passwords for every login role of a child | **Removed.** Children keep the parent's passwords, except under a protected parent. |
| `privileged` in `POST …/roles` and `RoleCreateRequest` | Renamed **`owner_level`**. |
| `neon_roles.kind` (`owner|login`), the sealed and privileged constraints, the strict name constraint | Replaced by `origin` (`owner|sql|api`), `login`, `level`, `api_owner_level`, a nullable verifier and `forwarded_at` (Task 1 migration). |
| `CHELADB_BRANCH_ROLE_CAP` 10 (0..=100), worker-only | 50 (0..=500), shared parser, read by orchestrator and worker. |
| `apply_pending` for login roles | Kept only for the owner (spec push). Login-role writes run SQL directly with forwarding off. |
| **Kept** | name rules and reserved names for API create; sealed generated passwords (`Purpose::Role`, AAD `"{branch_id}:role:{name}"`); routes, RPC subjects, 201/204 replies; `pending` and `roles_version`; delete across every connectable database (quoted conninfo, checked before `NOLOGIN`); error codes; the Studio Roles panel and role picker; the live-test harness. |

---

## File Structure

| File | Task | Responsibility |
|---|---|---|
| `crates/cheladb-metadata/migrations/20261004000001_sql_roles.sql` (create) | 1 | the revision-2 metadata (below, in full) |
| `crates/cheladb-metadata/src/neon.rs` (rewrite of the role parts) | 1 | `RoleOrigin`, `RoleLevel`, `PasswordState`, `RoleCopy`, `RoleRow`, owner-only `neon_roles`, API role writes, resync read and apply |
| `crates/cheladb-metadata/src/ddl.rs` (create) | 1 | `DdlTx`: the receiver's one transaction |
| `crates/cheladb-metadata/src/branches.rs`, `lib.rs`, `data_api.rs` (modify) | 1 | `protected`, claim outcome, `begin_delete` outcome, `branch_databases`, branch copies, keys following a rename |
| `crates/cheladb-nats/src/lib.rs` (modify) | 1 | `RoleCreateRequest.owner_level` |
| `services/worker/src/{ports.rs,fakes.rs,…}`, `services/orchestrator/src/…` (compile sites only) | 1 | keep the workspace building |
| `crates/cheladb-types/src/{role_name.rs,role_password.rs (create),role_settings.rs (create),api_error.rs,lib.rs}` | 2 | name checks, password rules, settings, `BranchProtected` |
| `crates/cheladb-neon/src/{spec.rs,backend.rs}` | 3 | `neon.console_url`, owner-only `RoleSpec`, `PGOPTIONS` |
| `infra/compose/docker-compose.yml`, `infra/neon/tests/run.sh`, `infra/compose/.env.example` | 3 | `max_roles=64`; the three settings |
| `services/worker/src/{roles.rs (rewrite),resync.rs (create),config.rs,lib.rs,ports.rs,fakes.rs,reset.rs}` | 4 | API role ops in SQL, the resync, settings |
| `services/orchestrator/src/internal/{ddl.rs (create),mod.rs,spec.rs,config.rs}`, `src/main.rs`, `tests/internal.rs` | 5 | the receiver, compute auth shared, console URL, Valkey |
| `services/orchestrator/src/{roles.rs,branches.rs,lib.rs,openapi.rs}`, `tests/api.rs`, `crates/cheladb-routes/src/lib.rs`, `openapi/*.json`, `packages/*/src/generated/**` | 6 | public API, protected, databases list, reveal by role and database |
| `services/rest/src/{exec.rs,error.rs}` (+ its tests) | 7 | the owner-level check |
| `services/worker/src/{branch_create.rs,branch_switch.rs,reconcile.rs,main.rs,provision.rs}` | 8 | resync hooks, protected children, rollout upgrade, RPC wiring |
| `apps/studio/src/components/{branch-roles.tsx,branch-actions.tsx,database-actions.tsx,data-api-keys.tsx}` (+ tests) | 9 | Studio |
| `README.md`, `docs/roadmap/2026-09-28-neon-and-data-api.md` | 10 | docs |
| `services/worker/tests/roles_live.rs` (rewrite), `scripts/role-e2e.sh` (rewrite), `scripts/fixtures/role-migration.sql` (create), `justfile` | 11 | live tests and e2e |

## Models and schedule

| Task | Implementer | Reviewer | Needs | Branch |
|---|---|---|---|---|
| 1 Migration, metadata, NATS rename | sonnet | sonnet (everything builds on it) | — | `login-roles-t1` |
| 2 Shared rules: names, passwords, settings, `BranchProtected` | sonnet | haiku (small, but the receiver and the worker both depend on it) | — | `login-roles-t2` |
| 3 Compute wiring: console URL, `PGOPTIONS`, owner-only spec, proxy flag | haiku | none (Task 11's live tests and the end review catch it; mechanical) | — | `login-roles-t3` |
| 4 Worker A: API role ops in SQL, the resync | opus | sonnet (Task 8 builds on it; security-relevant SQL) | 1, 2, 3 | `login-roles-worker` |
| 5 Receiver (orchestrator internal) | opus | sonnet (security boundary; nothing else covers its rules) | 1, 2, 3 | `login-roles-t5` |
| 6 Public API, protected, OpenAPI, generated client | sonnet | sonnet (Studio and e2e consume it) | 1, 2 | `login-roles-t6` |
| 7 `rest`'s owner-level check | sonnet | haiku (small; security-relevant) | 2 | `login-roles-t7` |
| 8 Worker B: hooks, branch copies, protected children, rollout, RPCs | opus | sonnet (branch semantics; only live tests would catch mistakes) | 4 | `login-roles-worker` (continues) |
| 9 Studio | sonnet | none (vitest and a Playwright pass in Task 11) | 6 | `login-roles-t9` |
| 10 Docs | haiku | none (end-of-step review) | 6 | `login-roles-t10` |
| 11 Live tests, `just role-e2e`, Playwright pass | opus | none (they are the check) | 4–10 merged | `login-roles` |
| End-of-step whole-branch review | | opus | all | |

```
Wave 1:  T1 ─┐   T2 ─┐   T3 ─┐                  (disjoint crates; merged in order 2, 3, 1)
Wave 2:      ├─ T4 (worker) ─────── T8 (worker) ─┐
             ├─ T5 (orchestrator/internal) ──────┤
             ├─ T6 (orchestrator public) ─┬─ T9 ─┤
             │                            └─ T10 ┤
             └─ T7 (rest) ───────────────────────┴─ T11 (live, one at a time) ─ end-of-step review
```
- **Wave 1** runs T1, T2 and T3 side by side:
  - T1: `crates/cheladb-metadata`, `crates/cheladb-nats`, plus compile sites in `services/*`.
  - T2: `crates/cheladb-types` only.
  - T3: `crates/cheladb-neon`, `infra/`, plus the two `stored_endpoint_config(…)` call expressions.
  - T1 and T3 both touch `services/worker/src/reset.rs` and `services/orchestrator/src/internal/spec.rs`. T3 edits only the call expression; T1 only the `neon_roles…` line. Merge T2, then T3, then T1, and resolve by hand if needed.
- **Wave 2** starts when wave 1 is merged into `login-roles`. T4, T5, T6 and T7 run side by side:
  - T4: `services/worker`;
  - T5: `services/orchestrator/src/internal/**`, `src/main.rs`, `tests/internal.rs`;
  - T6: `services/orchestrator/src/{roles,branches,lib,openapi}.rs`, `tests/api.rs`, `crates/cheladb-routes`, `openapi/`, generated clients;
  - T7: `services/rest`.
- **T8** continues T4's branch after T4's review. **T9** and **T10** start when T6 is merged.
- **T11** runs on `login-roles` after everything is merged, one live suite at a time. The stack and `just dev` are restarted first, so the orchestrator serves the new spec.
- Shared files: `Cargo.lock` doesn't change (no new crates). `infra/compose/.env.example` belongs to T3 only. README and roadmap belong to T10 only.

---

### Task 1: Migration, metadata, NATS rename

**Files:**
- Create: `crates/cheladb-metadata/migrations/20261004000001_sql_roles.sql`, `crates/cheladb-metadata/src/ddl.rs`.
- Modify: `crates/cheladb-metadata/src/{neon.rs,branches.rs,lib.rs,data_api.rs}`, `crates/cheladb-nats/src/lib.rs`.
- Fix compile sites only, no behaviour work, in `services/worker/src/**` and `services/orchestrator/src/**`. Keep revision-1 worker code compiling by mapping to the new calls; Task 4 replaces it.
- Test: `crates/cheladb-metadata/tests/live_db.rs` (live) and unit tests.
- **Never edit** `20261003000001_login_roles.sql`.

**Migration (exact):**
```sql
-- Step 6a revision 2: login roles and Postgres databases managed from SQL.
-- On top of 20261003000001_login_roles.sql (already applied on dev machines; never edit it).

-- neon_roles: the proxy's copy of each role's SCRAM hash, plus bookkeeping.
DROP INDEX neon_roles_one_owner;
ALTER TABLE neon_roles
    DROP CONSTRAINT neon_roles_login_sealed,
    DROP CONSTRAINT neon_roles_owner_privileged,
    DROP CONSTRAINT neon_roles_login_name,
    ADD COLUMN origin       text,
    ADD COLUMN login        boolean NOT NULL DEFAULT true,
    ADD COLUMN level        text NOT NULL DEFAULT 'plain',
    ADD COLUMN forwarded_at timestamptz,
    ALTER COLUMN scram_verifier DROP NOT NULL;
UPDATE neon_roles SET
    origin = CASE kind WHEN 'owner' THEN 'owner' ELSE 'api' END,
    level  = CASE WHEN kind = 'owner' OR privileged THEN 'owner_level' ELSE 'plain' END;
ALTER TABLE neon_roles RENAME COLUMN privileged TO api_owner_level;
UPDATE neon_roles SET api_owner_level = false WHERE origin <> 'api';
ALTER TABLE neon_roles
    ALTER COLUMN origin SET NOT NULL,
    ALTER COLUMN api_owner_level SET DEFAULT false,
    DROP COLUMN kind,
    ADD CONSTRAINT neon_roles_origin CHECK (origin IN ('owner', 'sql', 'api')),
    ADD CONSTRAINT neon_roles_level CHECK (level IN ('plain', 'owner_level')),
    ADD CONSTRAINT neon_roles_owner_verifier CHECK (origin <> 'owner' OR scram_verifier IS NOT NULL),
    ADD CONSTRAINT neon_roles_owner_unsealed CHECK (origin <> 'owner' OR password_enc IS NULL),
    ADD CONSTRAINT neon_roles_api_owner_level CHECK (origin = 'api' OR NOT api_owner_level),
    ADD CONSTRAINT neon_roles_name_length CHECK (octet_length(name) BETWEEN 1 AND 63);
CREATE UNIQUE INDEX neon_roles_one_owner ON neon_roles (endpoint_id) WHERE origin = 'owner';

-- Rollout: computes below the current settings version are re-created once (neon.console_url).
ALTER TABLE neon_endpoints ADD COLUMN compute_settings_version integer NOT NULL DEFAULT 0;

ALTER TABLE branches ADD COLUMN protected boolean NOT NULL DEFAULT false;

-- The Postgres databases of each branch: its main one, those made in SQL, and the system `postgres`
-- database (filled by the first resync).
CREATE TABLE branch_databases (
    branch_id  uuid NOT NULL REFERENCES branches (id) ON DELETE CASCADE,
    name       text NOT NULL CHECK (octet_length(name) BETWEEN 1 AND 63),
    owner      text NOT NULL,
    origin     text NOT NULL CHECK (origin IN ('main', 'sql', 'system')),
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (branch_id, name)
);
CREATE UNIQUE INDEX branch_databases_one_main ON branch_databases (branch_id) WHERE origin = 'main';
INSERT INTO branch_databases (branch_id, name, owner, origin)
SELECT b.id, d.name, d.owner, 'main'
FROM branches b
JOIN neon_databases d ON d.endpoint_id = b.endpoint_id
WHERE b.endpoint_id IS NOT NULL AND b.status <> 'deleted'
ON CONFLICT DO NOTHING;
```
- Postgres `text` can't hold NUL, so the length check is the whole name rule for SQL-made roles.

**The resync's catalog query (exact; run by Task 4 through `admin_sql` on the branch's main database; `$OWNER` is the owner's name as a quoted SQL literal):**
```sql
SET search_path = pg_catalog, pg_temp;
SELECT 'r',
       encode(convert_to(a.rolname, 'UTF8'), 'hex'),
       a.rolcanlogin,
       coalesce(encode(convert_to(a.rolpassword, 'UTF8'), 'hex'), ''),
       (a.rolsuper OR a.rolcreaterole OR a.rolcreatedb OR a.rolbypassrls OR a.rolreplication
        OR pg_has_role(a.oid, 'neon_superuser', 'MEMBER')),
       EXISTS (SELECT 1 FROM pg_auth_members m
               WHERE m.roleid = a.oid AND m.admin_option
                 AND m.member = (SELECT oid FROM pg_roles WHERE rolname = $OWNER))
FROM pg_authid a
WHERE a.oid >= 16384
  AND a.rolname <> $OWNER
  AND a.rolname NOT IN ('cloud_admin', 'neon_superuser', 'anon', 'authenticated',
                        'authenticator', 'cheladb_admin')
  AND a.rolname !~ '^pg_'
UNION ALL
SELECT 'd',
       encode(convert_to(d.datname, 'UTF8'), 'hex'),
       NULL,
       encode(convert_to(pg_get_userbyid(d.datdba), 'UTF8'), 'hex'),
       d.datdba < 16384,
       NULL
FROM pg_database d
WHERE NOT d.datistemplate AND d.datconnlimit <> -2
ORDER BY 1, 2;
```
- Output lines: `r|<hex name>|<t|f login>|<hex rolpassword or empty>|<t|f owner_level>|<t|f owner has ADMIN>` and `d|<hex name>||<hex owner>|<t|f system>|`. `system` is true only for the bootstrap-owned `postgres` database.

**Interfaces (produces):**
```rust
// cheladb-metadata::neon
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)] #[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub enum RoleOrigin { Owner, Sql, Api }
#[derive(…same…)] pub enum RoleLevel { Plain, OwnerLevel }
#[derive(…same…)] pub enum PasswordState { Generated, SetInSql, Md5Unusable }
// RoleKind is removed.

/// One `neon_roles` row with its secrets. Debug redacts the verifier and the sealed password.
pub struct RoleCopy { pub name: String, pub origin: RoleOrigin, pub scram_verifier: Option<String>,
    pub password_enc: Option<String>, pub login: bool, pub level: RoleLevel, pub api_owner_level: bool,
    pub pending: bool, pub created_at: DateTime<Utc> }
/// As listed (no secrets). `password` for the owner comes from `branches.owner_password_enc`
/// (NULL → SetInSql); for others: verifier NULL → Md5Unusable, password_enc set → Generated, else SetInSql.
pub struct RoleRow { pub name: String, pub origin: RoleOrigin, pub level: RoleLevel, pub login: bool,
    pub password: PasswordState, pub pending: bool, pub created_at: DateTime<Utc> }
pub struct NeonRole { pub name: String, pub scram_verifier: String }   // `privileged` removed; Debug redacts
pub struct NewRoleCopy<'a> { pub name: &'a str, pub origin: RoleOrigin /* Sql | Api */,
    pub verifier: Option<&'a str>, pub password_enc: Option<&'a str>, pub login: bool,
    pub level: RoleLevel, pub api_owner_level: bool, pub pending: bool }
// NewEndpoint: `login_roles` is replaced by `role_copies: &'a [NewRoleCopy<'a>]`; the main
// database's `branch_databases` row (origin main) is written in the same transaction.
pub enum InsertRoleOutcome { Inserted, Exists, CapReached }   // unchanged
#[derive(…same…)] pub enum DatabaseOrigin { Main, Sql, System }
pub struct BranchDatabase { pub name: String, pub owner: String, pub origin: DatabaseOrigin }

impl MetadataClient {
    /// CHANGED: only the owner row (origin owner) — what the compute spec lists.
    pub async fn neon_roles(&self, ep: &str) -> Result<Vec<NeonRole>, MetadataError>;
    pub async fn neon_roles_versioned(&self, ep: &str) -> Result<(Vec<NeonRole>, i64), MetadataError>; // owner only
    /// `None` unknown endpoint; `Some(None)` no row or NULL verifier (MD5).  (semantics as today)
    pub async fn neon_role_secret(&self, ep: &str, role: &str) -> Result<Option<Option<String>>, MetadataError>;
    /// Owner first, then by name COLLATE "C".
    pub async fn list_roles(&self, ep: &str) -> Result<Vec<RoleRow>, MetadataError>;
    /// Every row with secrets, and roles_version, read in one statement.
    pub async fn role_copies(&self, ep: &str) -> Result<(Vec<RoleCopy>, i64), MetadataError>;
    /// The sealed password of a non-owner row; None when there's no row or nothing sealed.
    pub async fn role_password(&self, ep: &str, name: &str) -> Result<Option<String>, MetadataError>;
    /// API create: locks the endpoint row; copies (origin sql|api with a verifier) ≥ cap → CapReached;
    /// insert origin api, pending = true, level from `owner_level`; roles_version += 1.
    pub async fn insert_api_role(&self, ep: &str, name: &str, owner_level: bool, verifier: &str,
        password_enc: &str, cap: u32) -> Result<InsertRoleOutcome, MetadataError>;
    /// Reset of any non-owner row: verifier, sealed password, pending = true, version += 1. False if no row.
    pub async fn replace_role_secret(&self, ep: &str, name: &str, verifier: &str, password_enc: &str)
        -> Result<bool, MetadataError>;
    /// Clears `pending` of one row while roles_version = `version`. True when cleared.
    pub async fn confirm_role(&self, ep: &str, name: &str, version: i64) -> Result<bool, MetadataError>;
    /// Deletes a non-owner row, version += 1. False if none.
    pub async fn delete_role_copy(&self, ep: &str, name: &str) -> Result<bool, MetadataError>;
    /// One transaction: lock the endpoint row; if roles_version ≠ read_version → VersionMoved (nothing written).
    /// Otherwise apply `write`, skipping (in SQL) every row whose forwarded_at > now() - 10 s, and set
    /// branch_databases of `branch` to `write.databases` exactly (Q1: a missing main row is removed).
    /// Origin: `system` when observed as system; `main` for the branch's recorded main database name
    /// (neon_databases of `ep`); otherwise `sql`. version += 1 if anything changed.
    pub async fn apply_resync(&self, ep: &str, branch: BranchId, read_version: i64, write: &ResyncWrite)
        -> Result<ResyncApplied, MetadataError>;
    pub async fn branch_databases(&self, branch: BranchId) -> Result<Vec<BranchDatabase>, MetadataError>; // main first, then by name
    pub async fn set_compute_settings_version(&self, ep: &str, v: i32) -> Result<(), MetadataError>;
    // replace_owner_secret: as today (owner row by origin = 'owner').
    // clear_needs_reconfigure: as today (also clears pending of the owner row only).
    // Removed: insert_login_role, replace_login_role_secret, delete_login_role, login_role_password,
    //          login_roles_of (callers move to the calls above).
}
pub struct ResyncWrite { pub upserts: Vec<CopyUpsert>, pub removals: Vec<String>, pub databases: Vec<ObservedDatabase> }
pub struct ObservedDatabase { pub name: String, pub owner: String, pub system: bool }
/// Insert-or-update one row. `origin` is used only on insert; an existing row keeps its origin.
/// `verifier: None` stores NULL (MD5). `password_enc`: Keep | Clear | Set(String).
pub struct CopyUpsert { pub name: String, pub origin: RoleOrigin, pub verifier: Option<String>,
    pub password_enc: SealedChange, pub login: bool, pub level: RoleLevel, pub pending: bool }
pub enum SealedChange { Keep, Clear, Set(String) }
pub enum ResyncApplied { Applied { changed: Vec<String> }, VersionMoved }

// cheladb-metadata::ddl — the receiver's one transaction.
pub struct DdlTx { /* Transaction<'static, Postgres>, endpoint id, branch id, owner name, main database */ }
impl MetadataClient {
    /// `SET LOCAL lock_timeout = '2s'`, `SELECT … FROM neon_endpoints WHERE id = $1 FOR UPDATE`, then the
    /// owner row's name, the linked branch (`branches.endpoint_id = $1`, not deleted) and its main
    /// database. None = unknown endpoint. A lock timeout → MetadataError::EndpointLockTimeout.
    pub async fn begin_ddl(&self, ep: &str) -> Result<Option<DdlTx>, MetadataError>;
}
impl DdlTx {
    pub fn branch_id(&self) -> Option<BranchId>;
    pub fn owner(&self) -> &str;
    pub fn main_database(&self) -> Option<&str>;
    pub async fn role(&mut self, name: &str) -> Result<Option<RoleCopy>, MetadataError>;
    pub async fn copy_count(&mut self) -> Result<u32, MetadataError>;                 // R5
    pub async fn live_secret_keys(&mut self, role: &str) -> Result<Vec<(Uuid, String)>, MetadataError>;
    /// Insert (origin sql, login true, level plain) or update: verifier, password_enc NULL; forwarded_at = now().
    pub async fn upsert_sql_copy(&mut self, name: &str, verifier: &str) -> Result<(), MetadataError>;
    /// The owner row's verifier and forwarded_at; `branches.owner_password_enc = NULL` for the branch.
    pub async fn set_owner_verifier(&mut self, verifier: &str) -> Result<(), MetadataError>;
    pub async fn remove_copy(&mut self, name: &str) -> Result<bool, MetadataError>;
    /// Moves the row (keeping origin and level), replaces password_enc with `resealed` (None keeps NULL),
    /// forwarded_at = now(); data_api_keys of the branch with role = old follow (R19).
    pub async fn rename_copy(&mut self, old: &str, new: &str, resealed: Option<&str>) -> Result<bool, MetadataError>;
    /// Rows of origin main or sql (R23: the system database isn't counted).
    pub async fn database_count(&mut self) -> Result<u32, MetadataError>;
    /// New rows: origin `main` when `name` is the recorded main database name (it came back, Q1), else `sql`.
    pub async fn upsert_database(&mut self, name: &str, owner: &str) -> Result<(), MetadataError>;
    pub async fn rename_database(&mut self, old: &str, new: &str) -> Result<bool, MetadataError>;
    /// Any row, main included (Q1).
    pub async fn remove_database(&mut self, name: &str) -> Result<bool, MetadataError>;
    /// roles_version += 1 when anything was written; commit.
    pub async fn commit(self) -> Result<(), MetadataError>;
}

// cheladb-metadata::branches
// BranchRecord gains `pub protected: bool`.
pub enum ClaimOutcome { Claimed, NotReady, Protected }
/// `allow_protected = false` adds `AND NOT protected` (delete, reset-to-parent); restore passes true.
pub async fn claim_branch_for_switch(&self, id: BranchId, allow_protected: bool) -> Result<ClaimOutcome, MetadataError>;
/// Normal branches not deleting/deleted. None = no such branch; Some(record) after the update.
pub async fn set_branch_protected(&self, id: BranchId, protected: bool) -> Result<Option<BranchRecord>, MetadataError>;
// lib.rs: begin_delete returns `BeginDelete { Started, NotReady, Protected }` (protected = any non-deleted branch with protected).

// cheladb-nats
pub struct RoleCreateRequest { pub branch_id: BranchId, pub name: String, pub owner_level: bool }   // was `privileged`
```
- The callers of the removed functions and of `claim_branch_for_switch`/`begin_delete` are mapped mechanically in this task: pass `allow_protected` as the old behaviour (true), and map `Protected` to the existing not-ready path. Tasks 6 and 8 give them their real behaviour.

**Tests to write** (live in `live_db.rs` unless marked):
- `migration_maps_revision_one_rows`: seed rows the 20261003 shape had (an owner, a plain login, a privileged login) before the new migration runs (use the existing migration-test pattern, or insert, then run the new file's statements by hand). Expect: origin `owner`/`api`/`api`; level `owner_level`/`plain`/`owner_level`; `api_owner_level` false/false/true.
- `owner_constraints`: an owner with a NULL verifier fails; an owner with `password_enc` fails; a second owner per endpoint fails.
- `sql_names_may_be_anything_postgres_allows`: `My Role`, `a"b`, `É`, 63 bytes are inserted; 64 bytes fails.
- `neon_roles_lists_only_the_owner`.
- `role_secret_of_an_md5_row_is_none`.
- `list_roles_password_states`: generated, set_in_sql, md5_unusable, and the owner's state from `owner_password_enc`.
- `api_insert_respects_the_cap_under_concurrency`: cap 2, five concurrent → exactly two Inserted. MD5 rows don't count; sql rows do.
- `resync_skips_recently_forwarded_rows`: a removal of a row forwarded 1 s ago is skipped; one forwarded 11 s ago is applied (set `forwarded_at` by hand).
- `resync_version_guard`: a `DdlTx` commit between `role_copies` and `apply_resync` → `VersionMoved`, nothing written.
- `resync_sets_databases_with_origins`: system / main / sql origins; a missing main row is removed, and it comes back as `main` when observed again.
- `database_count_ignores_system`.
- `ddl_main_database_drop_and_recreate`: `remove_database(main)`, then `upsert_database(main)` → origin `main` again.
- `ddl_rename_moves_copy_and_keys`: two keys (one revoked) follow; the copy keeps its origin.
- `ddl_owner_verifier_clears_the_sealed_owner_password`.
- `ddl_lock_times_out`: hold `FOR UPDATE` in another transaction for 3 s → `EndpointLockTimeout` in about 2 s.
- `branch_copy_writes_role_copies_and_main_database`.
- `claim_refuses_a_protected_branch_unless_allowed`; `begin_delete_refuses_with_a_protected_branch`.
- Unit (`neon.rs`): `debug_redacts_role_copy`.
- `cheladb-nats`: `role_create_request_uses_owner_level` (serde: the key is `owner_level`).

**Verify:** `just --justfile <wt>/justfile --working-directory <wt> test -p cheladb-metadata -p cheladb-nats`, the live metadata tests as `live_db.rs` documents, `cargo build --workspace --all-targets`, and `cargo clippy --workspace --all-targets -- -D warnings`.

---

### Task 2: Shared rules: names, passwords, settings, `BranchProtected`

**Files:** `crates/cheladb-types/src/{role_name.rs,role_password.rs (create),role_settings.rs (create),api_error.rs,lib.rs}`. Nothing outside `cheladb-types`.

**Interfaces (produces):**
```rust
// role_name.rs (kept: RESERVED_ROLE_NAMES, RoleNameError, check_login_role_name)
/// Any name Postgres can hold: 1..=63 bytes, no NUL. (reset, delete, reveal — R14)
pub fn check_any_role_name(name: &str) -> Result<(), RoleNameError>;
/// RESERVED_ROLE_NAMES or a `pg_` prefix (any case), except `owner` itself.
pub fn is_reserved_for(name: &str, owner: &str) -> bool;
/// `"` doubled, wrapped in quotes; and a single-quoted SQL literal ('' doubled; no backslash escapes).
pub fn quote_ident(name: &str) -> String;
pub fn quote_literal(s: &str) -> String;

// role_password.rs
pub enum PasswordKind { Scram, Md5, Other }
pub fn hash_kind(s: &str) -> PasswordKind;            // "SCRAM-SHA-256$" prefix → Scram; "md5" + 32 hex → Md5
pub fn entropy_bits(password: &str) -> f64;           // R15
#[derive(Debug, PartialEq, Eq)]
pub enum PasswordRefusal { Md5, NotScram, Prehashed, Weak }
impl PasswordRefusal { pub fn message(&self) -> &'static str; }   // the approved texts in Task 5 (Q7)
/// The receiver's check of one forwarded `set` with a password.
pub fn check_forwarded_password(password: &str, encrypted: &str, rules: &RoleSettings)
    -> Result<(), PasswordRefusal>;
//   order: encrypted MD5 → Md5; encrypted not SCRAM → NotScram; `password` itself a hash (Scram or Md5 kind):
//   platform → Prehashed, self-hosted → Ok even with strength on (Q6); else strength when `rules.strength` and
//   (chars < 12 or bits < 60) → Weak.

// role_settings.rs
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RoleSettings { pub role_cap: u32, pub database_cap: u32, pub strength: bool, pub prehashed_allowed: bool }
impl RoleSettings {
    /// CHELADB_BRANCH_ROLE_CAP (default 50, 0..=500), CHELADB_BRANCH_DATABASE_CAP (default 20, 1..=1000),
    /// CHELADB_PASSWORD_STRENGTH (on|off|true|false|1|0, case-insensitive, default off), with `mode`:
    /// platform forces strength = true and prehashed_allowed = false. Blank = unset. Errors name the variable.
    pub fn from_lookup(mode: Mode, get: impl Fn(&str) -> Option<String>) -> Result<Self, String>;
}

// api_error.rs: ErrorCode::BranchProtected  // "branch_protected", 409 (every-code test updated)
```

**Tests to write:**
- `role_name`: `any_name_accepts_quoted_shapes` (`My Role`, `a"b`, `É`, 63 bytes; refuses empty, 64 bytes, NUL); `reserved_for_the_owner` (`postgres` is reserved unless it's the owner; `PG_x` reserved; `pg_x` reserved even as owner); `quoting` (`a"b` → `"a""b"`; `it's` → `'it''s'`).
- `role_password`:
  - `hash_kinds`: a real SCRAM verifier, `md5`+32 hex, `md5short`, plaintext;
  - `entropy_examples`: 12 lowercase ≈ 56.4 → weak; 13 lowercase ≥ 60; `Tr0ub4dor&3x` ≥ 60;
  - `forwarded_password_table`: a table over (password, encrypted, mode, strength) → expected refusal. It covers MD5 in both modes, pre-hashed self-hosted Ok (also with strength on) / platform Prehashed, weak with strength on and off, and an 11-char strong-looking password (Weak: the length rule).
- `role_settings`: `defaults`, `ranges` (501 roles, 0 databases, 1001 databases refused), `strength_values`, `platform_forces_strength_and_refuses_prehashed`.
- `api_error`: the every-code test includes `branch_protected` → 409.

**Verify:** `test -p cheladb-types`, clippy.

---

### Task 3: Compute wiring: console URL, `createrole_self_grant`, `PGOPTIONS`, owner-only spec, proxy flag

**Files:**
- `crates/cheladb-neon/src/{spec.rs,backend.rs}`;
- `infra/compose/docker-compose.yml` (line 362), `infra/neon/tests/run.sh` (line 87), `infra/compose/.env.example`;
- only the `stored_endpoint_config(…)` call expressions in `services/orchestrator/src/internal/spec.rs` and `services/worker/src/reset.rs`, which pass the new argument as `None` (Tasks 5 and 4 pass the real value).

**Interfaces (produces):**
```rust
pub struct RoleSpec { pub name: String, pub scram_verifier: String }      // `privileged` removed
// SpecInputs gains: pub console_url: Option<String>   (Some → setting("neon.console_url", url, "string"))
// settings always gain setting("createrole_self_grant", "set, inherit", "string")   (Q9)
pub fn ddl_url(control_plane_uri: &str, endpoint_id: &str) -> String;   // "{uri without trailing /}/compute/api/v2/computes/{ep}/ddl"
pub async fn stored_endpoint_config(placement, endpoint, roles: Vec<NeonRole>, databases, jwks,
    console_url: Option<String>) -> anyhow::Result<Value>;              // endpoint_config gains the same last argument
// backend.rs run_exec env: ["PGCONNECT_TIMEOUT=5", "PGOPTIONS=-c neon.forward_ddl=off"]  (R2)
```
- `NeonRole.privileged` disappears in Task 1. If Task 1 hasn't merged yet, `stored_endpoint_config` maps only `name` and `scram_verifier`, which compiles against both shapes.
- Compose: `--project-info-cache=size=10000,ttl=1m,max_roles=64,gc_interval=60m` (R20); `run.sh` asserts the new text.
- `.env.example`: `CHELADB_BRANCH_ROLE_CAP=50`, `CHELADB_BRANCH_DATABASE_CAP=20` and `CHELADB_PASSWORD_STRENGTH=off`, each with a one-line comment. They replace revision 1's role-cap line.

**Tests to write:**
- `spec.rs`: `console_url_is_a_string_setting` (present when Some, absent when None); `createrole_self_grant_is_set_inherit`; `ddl_url_joins_without_double_slash`; `roles_carry_no_privileged_key`.
- `backend.rs`: `admin_exec_turns_forwarding_off` (the `CreateExecOptions` env built by a small extracted fn `admin_exec_env()` contains `PGOPTIONS=-c neon.forward_ddl=off`).

**Verify:** `test -p cheladb-neon`, `just neon-test` (static checks), `cargo build --workspace`, clippy.

---

### Task 4: Worker A: API role operations in SQL, and the resync

**Files:**
- Create: `services/worker/src/resync.rs`.
- Rewrite: `services/worker/src/roles.rs`. Delete `enforce_plain_roles`, `plain_check_sql`, `still_member_sql`, `indirect_member_sql` and their tests.
- Modify: `services/worker/src/{config.rs,lib.rs,ports.rs,fakes.rs,reset.rs}`. In `reset.rs`, pass `console_url: Some(ddl_url(&self.control_plane_uri, ep))` to `stored_endpoint_config`.

**Interfaces:**
- **Consumes:** Task 1's metadata calls and `RoleCreateRequest.owner_level`; Task 2's `RoleSettings`, `check_login_role_name`, `check_any_role_name`, `is_reserved_for`, `quote_ident`, `quote_literal`, `hash_kind`; Task 3's `ddl_url`.
- **Produces:**
```rust
// config.rs: WorkerConfig gains `roles: RoleSettings` (replaces `role_cap`); Worker gains
// `roles: RoleSettings` and `control_plane_uri: String`.
// roles.rs (RoleOutcome kept; `Mismatch` removed; role_reply unchanged otherwise)
pub async fn create_role(&self, branch: BranchId, name: &str, owner_level: bool) -> RoleOutcome;
pub async fn reset_role(&self, branch: BranchId, name: &str) -> RoleOutcome;
pub async fn delete_role(&self, branch: BranchId, name: &str) -> RoleOutcome;
// resync.rs
pub struct ObservedRole { pub name: String, pub login: bool, pub password: Option<String>, pub owner_level: bool, pub owner_admin: bool }
pub struct Observed { pub roles: Vec<ObservedRole>, pub databases: Vec<ObservedDatabase> }   // ObservedDatabase from Task 1
pub fn resync_sql(owner: &str) -> String;                         // the Task 1 query with $OWNER = quote_literal(owner)
pub fn parse_observed(out: &str) -> anyhow::Result<Observed>;      // hex-decoded; any malformed line is an error
/// SQL repairs for api rows (as cloud_admin), and the metadata write.
pub fn plan_resync(owner: &str, copies: &[RoleCopy], observed: &Observed) -> (Vec<String> /*SQL statements*/, ResyncWrite);
/// Caller holds the endpoint lock. Reads copies+version, the catalog, runs the repairs, applies; on
/// VersionMoved starts over (at most 3 tries, then Err). Publishes password_updated for every changed
/// name now and again ~1 s later (R22). Errors are prefixed "syncing login roles: ".
pub(crate) async fn resync_locked(&self, branch: BranchId, ep: &str, database: &str) -> Result<(), String>;
pub async fn resync(&self, branch: BranchId) -> Result<(), String>;   // takes the lock, then resync_locked
```

**Behaviour:**
- **Create:**
  1. `check_login_role_name`.
  2. The branch is ready and `normal`, with an endpoint.
  3. Take the lock.
  4. `admin_sql` `SELECT 1 FROM pg_roles WHERE rolname = <literal>`. A hit → `role_exists`, adopting nothing.
  5. Generate the password, its verifier and `seal_role_password`.
  6. `insert_api_role(…, roles.role_cap)` (`Exists`/`CapReached` as before).
  7. One `admin_sql` (forwarding is off through `PGOPTIONS`):
     ```sql
     SET search_path = pg_catalog, pg_temp;
     SET ROLE <quote_ident(owner)>;
     CREATE ROLE <quote_ident(name)> LOGIN PASSWORD <quote_literal(verifier)>;
     RESET ROLE;
     GRANT neon_superuser TO <quote_ident(name)>;   -- only when owner_level
     ```
  8. `confirm_role(ep, name, version)`.
  9. Publish `password_updated`, now and again about 1 s later.
  - An `admin_sql` failure in step 7 → `NotApplied`; the row stays `pending`, and the next resync re-creates it. Revision 1's `apply_pending_locked` is no longer used for login roles.
- **Reset:**
  - The owner name → the existing owner reset.
  - Otherwise: `check_any_role_name`; lock; `replace_role_secret` (false → `NotFound`); `ALTER ROLE <ident> PASSWORD <literal>` as `cloud_admin` (a missing role → `NotFound` after re-checking `pg_roles`); `confirm_role`; publish twice.
  - It works for `sql` rows too: they become revealable.
- **Delete:** revision 1's SQL order (NOLOGIN, terminate, then for every connectable database `REASSIGN OWNED`/`DROP OWNED`, then `DROP ROLE IF EXISTS`), with these changes:
  - `check_any_role_name`;
  - any non-owner row (`is_reserved_for` → `reserved_role_name`);
  - the live-secret-key refusal as before;
  - then `delete_role_copy` and publish twice.
- **Resync, `plan_resync` rules (spec Part 3).** For each observed role with a hash:
  - SCRAM, no row → upsert `origin sql`, `Clear`.
  - SCRAM, `sql` row with a different hash → upsert the new hash, `Clear` the sealed password.
  - `sql` row, same hash → refresh `login`/`level` only.
  - MD5 → a row with `verifier None` (R9).
  - An observed role without a hash, whose row is `sql` → removal. Its row is `api` → repair: `ALTER ROLE … PASSWORD <stored>`.
- **Resync, `api` rows:**
  - missing in the catalog → repair `SET ROLE <owner>; CREATE ROLE … LOGIN PASSWORD <stored>; RESET ROLE` (plus `GRANT neon_superuser` when `api_owner_level`), and the row's `pending` cleared;
  - a different hash → repair `ALTER ROLE … PASSWORD <stored>`;
  - `owner_admin = false` → repair `GRANT <r> TO <owner> WITH ADMIN OPTION, INHERIT FALSE, SET FALSE` (R13).
- **Resync, removals and the rest:**
  - `sql` rows not observed → removal.
  - Every row → `login` and `level` refreshed from the catalog.
  - The owner row is never touched (R8). Nothing is removed for exceeding the cap.
  - `databases` = the observed list, with the system flag (Q8); a missing main database simply isn't in it (Q1).
  - An `api` row whose stored verifier is NULL can't happen (create always has one); if it does, log ERROR and skip it.
- **Order inside `resync_locked`:** `role_copies` (version v) → `admin_sql(resync_sql)` → `parse_observed` → `plan_resync` → run the repairs in one `admin_sql` (if there are any) → `apply_resync(v)` → publish the changed names plus every name repaired.

**Tests to write** (fakes; the fake `admin_sql` answers scripted outputs):
- `create_runs_as_the_owner_in_one_script` (the SQL contains `SET ROLE "postgres"` before `CREATE ROLE`, the verifier as a literal, no `GRANT` for plain); `create_owner_level_grants_neon_superuser`.
- `create_refuses_bad_and_reserved_names_without_sql`; `create_conflicts_on_an_existing_sql_role`; `create_at_the_cap_is_refused` (cap from `RoleSettings`).
- `create_sql_failure_is_not_applied_and_pending`; `create_publishes_twice`.
- `reset_of_an_sql_role_makes_it_revealable` (the row's `password_enc` opens with the role's AAD).
- `delete_accepts_quoted_names` (`My "Role"` → `"My ""Role"""` everywhere in the SQL; the database names in the conninfo are quoted as before); `delete_refuses_the_owner`; `delete_refused_while_a_live_secret_key_uses_the_role`.
- `parse_observed_decodes_hex_and_refuses_garbage` (`|` and a newline inside a name round-trip via hex; the `d` line's system flag).
- `plan_resync_sql_rows` (a table: added, hash changed, removed, MD5, no-hash removal, login/level refresh, nothing for over-cap).
- `plan_resync_api_rows` (re-create with `GRANT` when `api_owner_level`; set-back; the ADMIN grant; owner never touched).
- `resync_retries_when_the_version_moves` (the fake `apply_resync` answers `VersionMoved` twice, then Applied → 3 calls; four times → `Err` starting with `syncing login roles:`).
- `resync_publishes_changed_names`.
- `config_reads_role_settings` (`WorkerConfig` with the three variables).

**Verify:** `test -p worker`, clippy.

---

### Task 5: The DDL receiver (orchestrator internal listener)

**Files:**
- Create: `services/orchestrator/src/internal/ddl.rs`.
- Modify: `services/orchestrator/src/internal/{mod.rs,spec.rs,config.rs}`, `src/main.rs`, `tests/internal.rs`.

**Interfaces:**
- **Consumes:** Task 1's `begin_ddl`/`DdlTx`, Task 2's `RoleSettings`, `check_forwarded_password`, `is_reserved_for`, `hash_kind`; Task 3's `ddl_url`; `open_role_password`/`seal_role_password`; `ProxyCache`.
- **Produces:**
```rust
// mod.rs: route  .route("/compute/api/v2/computes/{id}/ddl", patch(ddl::receive::<M, L>))
// NeonMetadataPort gains: fn begin_ddl(&self, ep) -> impl Future<Output = Result<Option<Self::Ddl>, MetadataError>>
//   with an associated `type Ddl: DdlPort` — DdlPort mirrors DdlTx's methods (so tests use a fake).
// InternalState gains: pub roles: RoleSettings, pub cache: ProxyCache (behind a small `CachePort` trait for fakes),
//   pub control_plane_uri: String.
// InternalConfig gains: control_plane_uri (CHELADB_CONTROL_PLANE_URI, default http://host.docker.internal:8083,
//   same check as the worker's), valkey_url (CHELADB_VALKEY_URL, default redis://127.0.0.1:6379); main.rs reads
//   RoleSettings::from_lookup(Mode::from_env()?, env).
// spec.rs: `authorize_compute(state, id, headers) -> Result<NeonEndpoint, Response>` extracted from compute_spec and
//   shared; endpoint_config passes Some(ddl_url(&state.control_plane_uri, &endpoint.id)).
#[derive(Deserialize)] struct Delta { #[serde(default)] roles: Vec<RoleOp>, #[serde(default)] dbs: Vec<DbOp> }
#[derive(Deserialize)] struct RoleOp { op: Op, name: String, old_name: Option<String>, password: Option<String>, encrypted_password: Option<String> }
#[derive(Deserialize)] struct DbOp { op: Op, name: String, owner: Option<String>, old_name: Option<String> }
#[derive(Deserialize)] #[serde(rename_all = "lowercase")] enum Op { Set, Del }
/// Pure: the decision for one delta against the rows read so far (table-tested).
fn decide(...)  // implementer's shape; the rules below are the contract
```

**Behaviour:**
- **Auth:** `authorize_compute` (401 with the body `compute token refused`). A malformed body → 400 `malformed request from the compute`. An empty delta → 200 at once (R4).
- **Order:** one `begin_ddl` transaction (unknown endpoint → 404 `unknown endpoint`; lock timeout → 503).
  - Role renames first: entries with `old_name` are processed as a rename, then as a `set` if they carry a password.
  - Then the other sets, then deletes, then databases (renames, then sets, then deletes).
  - The first refusal rolls back and answers 409 with its message. Success commits, then publishes (below), then 200.
- **Role `set` with a password:**
  - The endpoint's owner (`DdlTx::owner()`): `check_forwarded_password`, then `set_owner_verifier`.
  - Reserved names (`is_reserved_for`) → 409.
  - `check_forwarded_password` refusal → 409 with its message.
  - A new row with `copy_count() >= roles.role_cap` → 409 naming `CHELADB_BRANCH_ROLE_CAP`.
  - Otherwise `upsert_sql_copy` (an existing row keeps its origin; its sealed password is cleared).
- **Role `set` without a password:** a group role or `PASSWORD NULL`. `remove_copy` if a row exists (the owner: 409). Otherwise nothing.
- **Rename:**
  - `old_name` or the new name reserved, or the owner → 409.
  - A row is moved with `rename_copy`. An `api` row with a sealed password is opened under the old AAD and re-sealed under the new one.
  - No row → nothing.
- **Role `del`:**
  - The owner or a reserved name → 409.
  - `live_secret_keys(name)` non-empty → 409 naming the keys.
  - Else `remove_copy` (a missing row is fine: idempotent).
- **Databases:**
  - `set` (create or owner change): a new name with `database_count() >= roles.database_cap` → 409 naming `CHELADB_BRANCH_DATABASE_CAP`; else `upsert_database`.
  - A rename of the main database → 409. Otherwise `rename_database`.
  - `del` of any database, main included → `remove_database` (Q1: the drop has already removed the files; refusing would only leave the database invalid). Log WARN for a main-database drop. Nothing new for the system `postgres` database (Postgres stops the owner; Q8).
  - A `set` of the recorded main database name after a drop restores the `main` row (`upsert_database`), within the cap.
  - A branch-less endpoint (no `branch_id`) skips database bookkeeping.
- **Idempotence:** applying the same delta twice gives the same rows and 200 both times.
- **Publishing:** `password_updated` for every changed role name (old and new on a rename), at once and again about 1 s later (R22). A failure is logged WARN (R12).
- **Errors:** a metadata error → 503 `role changes are unavailable right now; try again` (WARN log with the endpoint). Bodies are one line of `text/plain`, at most 200 bytes, and never echo a password.
- **Refusal texts (approved by the user, Q7):**
  - reserved: `the role name "<n>" is reserved by Cheladb`
  - owner drop/rename: `the branch owner "<n>" can't be dropped or renamed`
  - MD5: `MD5 passwords can't be used through Cheladb; set a plain password`
  - not SCRAM: `this password format can't be used through Cheladb; set a plain password`
  - pre-hashed (platform): `pre-hashed passwords aren't accepted here; set a plain password`
  - weak: `password too weak: use at least 12 characters and mix letters, digits and symbols (about 60 bits)`
  - role cap: `this branch has reached its limit of <cap> login roles (CHELADB_BRANCH_ROLE_CAP)`
  - in use: `role "<n>" backs data API key(s) <k1>, <k2>; revoke them first`
  - database cap: `this branch has reached its limit of <cap> databases (CHELADB_BRANCH_DATABASE_CAP)`
  - main database: `the branch's main database "<d>" can't be renamed`
  - busy: `role changes are busy; try again`

**Tests to write** (`tests/internal.rs` with fakes; `ddl.rs` unit tests for `decide`):
- `ddl_needs_the_compute_token` (no token, another compute's token, an unknown id → 401, and the same timing path as the spec route).
- `ddl_empty_delta_is_200_without_a_transaction`.
- `ddl_rules_table`: one row per rule above → status and body. It covers a pre-hashed password per mode, strength on and off, the cap exactly at and below, MD5 refused, the owner re-password, `PASSWORD NULL`, a group role, a reserved rename target, a key-backed drop, the main database rename (409, the approved text), the main database drop (200, row removed), its re-creation (200, `main` again), pre-hashed with strength on in self-hosted (200), and the cap not counting the system database.
- `ddl_is_idempotent` (the same delta twice; `DROP ROLE IF EXISTS ghost` → 200).
- `ddl_rename_reseals_an_api_password` (opens under the new AAD only).
- `ddl_first_refusal_rolls_back_everything` (a delta with an ok `set` then a refused `del` → nothing written).
- `ddl_publishes_twice_and_survives_valkey_down`.
- `ddl_lock_timeout_is_503`.
- `ddl_body_never_contains_the_password` (a weak plaintext refused: the body doesn't contain it).
- `spec_route_sets_console_url` (the served spec has `neon.console_url` = `ddl_url(...)`).
- `config_reads_control_plane_uri_and_valkey`.

**Verify:** `test -p orchestrator`, clippy.

---

### Task 6: Public API, protected flag, OpenAPI, generated client

**Files:**
- Modify: `services/orchestrator/src/{roles.rs,branches.rs,lib.rs,openapi.rs}`, `services/orchestrator/tests/api.rs`, `crates/cheladb-routes/src/lib.rs`.
- Regenerate with `just api-gen` (never edit by hand): `openapi/*.json`, `packages/*/src/generated/**`.

**Interfaces:**
```rust
// cheladb-routes: LIST_BRANCH_DATABASES GET /v1/databases/{id}/branches/{branch_id}/databases (listBranchDatabases)
//                 UPDATE_BRANCH         PATCH /v1/databases/{id}/branches/{branch_id}          (updateBranch)
// Schemas:
struct BranchRole { name: String, origin: RoleOrigin, level: RoleLevel, login: bool, password: PasswordState,
                    blocked_keys: Vec<RoleInUseKey>, pending: bool, created_at: DateTime<Utc> }   // `kind`/`privileged` removed
#[serde(deny_unknown_fields)] struct CreateRoleRequest { name: String, #[serde(default)] owner_level: bool }
struct BranchDatabaseView { name: String, owner: String, origin: DatabaseOrigin /* main|sql|system */, main: bool }
// Branch responses also gain `main_database_missing: bool` (no `main` row in branch_databases; Q1).
#[serde(deny_unknown_fields)] struct UpdateBranchRequest { protected: bool }
// Branch responses (list/create/get) gain `protected: bool`.
// ConnectionDetails: `password: Option<String>` and `password_state: PasswordState` (Q5); `uri` without a
//   password when None; user and database percent-encoded in `uri`.
// MetadataPort gains: list_roles, role_password, branch_databases, set_branch_protected, list_data_keys (existing).
// JobPort: create_role(RoleCreateRequest { owner_level }) — unchanged otherwise.
```

**Behaviour:**
- **List roles:** `list_roles(ep)`. `blocked_keys` holds the live secret keys of the branch whose role is that row's name, for a row whose `level` is `owner_level` (Q3: the one wide definition). The owner comes first.
- **Create:** body `{ name, owner_level? }` (`privileged` is now an unknown field → 422); otherwise as revision 1 (201 `BranchRole`).
- **Reset and delete:** `check_any_role_name` (R14) instead of the strict check. The owner's name: reset uses the owner path (204); delete → 422 `reserved_role_name`.
- **Reveal** `GET …/connection?role=&database=`:
  - `role` absent or the owner → the owner, whose password is `None` when `owner_password_enc` is NULL (`set_in_sql`).
  - Otherwise `check_any_role_name` (422) → the row (none → 404) → its sealed password, if any.
  - `database` absent → the recorded main database, even while it's missing (R24). Otherwise it must be in `branch_databases(branch)`, the system `postgres` database included (else 404 `not_found`).
  - `password_state` from the row. `Cache-Control: no-store` as today.
- **`GET …/databases`:** `branch_databases(branch)` → 200 `BranchDatabaseView[]` (main first, then by name; the system `postgres` database with origin `system`). A backup branch → 409 `backup_branch`.
- **`PATCH …/branches/{branch_id}`:** `{ "protected": bool }` → `set_branch_protected`. 200 with the branch. 404 for an unknown branch, 409 `backup_branch` for a backup, 409 `branch_not_ready` for a deleting one. INFO log with the new value.
- **Protected refusals (R17):**
  - delete branch and reset-to-parent use `claim_branch_for_switch(id, false)`: `Protected` → 409 `branch_protected` "this branch is protected; unprotect it first";
  - a non-ready branch's delete checks `branch.protected` first;
  - restore passes `true`;
  - database delete: `BeginDelete::Protected` → 409 `branch_protected` "a branch of this database is protected; unprotect it first".
- **Annotations:** every new or changed handler has `#[utoipa::path]`, every status with a `description`, and the codes named. The two new ids join `operation_ids_are_the_contract`. Run `api-gen` and commit the regenerated files with the annotations.

**Tests to write:**
- `list_roles_shapes_and_blocked_keys`; `create_role_takes_owner_level_and_refuses_privileged`.
- `reset_and_delete_accept_sql_names` (`My%20Role` decoded; NUL → 422).
- `connection_by_role_and_database` (an `sql` role without a sealed password → `password: null`, `password_state: "set_in_sql"`, a `uri` with no password; an MD5 row → `md5_unusable`; an unknown database 404; a percent-encoded user in `uri`).
- `owner_set_in_sql_reveals_no_password`.
- `list_branch_databases` (origins main/sql/system); `main_database_missing_is_reported` (no main row → the branch response says so; the reveal still names it).
- `update_branch_protected` (200, backup 409, unknown 404, unknown field 422).
- `protected_branch_refuses_delete_and_reset_but_not_restore`; `database_delete_refused_with_a_protected_branch`.
- `branch_responses_carry_protected`.
- The `openapi.rs` contract tests: RED first by adding the ids.
- `cheladb-routes`: the two endpoints with their policies.

**Verify:** `test -p orchestrator -p cheladb-routes -p gateway`, clippy, `just check` (document and client freshness; Studio typecheck/tests still green with the regenerated client).

---

### Task 7: `rest`'s owner-level check

**Files:** `services/rest/src/{exec.rs,error.rs}` (+ unit tests; the `rest_live` addition goes to Task 11).

**Interfaces:**
```rust
// error.rs
pub fn role_not_allowed(role: &str) -> ApiError;   // 403, code "role_not_allowed",
// "the role `<name>` of this key can bypass row-level security; make it plain or use another role"
// exec.rs
pub const OWNER_LEVEL_SQL: &str = "SELECT rolsuper OR rolbypassrls OR rolcreaterole OR rolcreatedb OR rolreplication \
    OR pg_has_role(oid, 'neon_superuser', 'MEMBER') FROM pg_catalog.pg_roles WHERE rolname = $1";  // the one wide definition (Q3)
// Engine gains `owner_level: moka::future::Cache<(Uuid, String), bool>` (time_to_live 30 s).
```

**Behaviour:** in `Engine::query` and `Engine::role_schema`, for a **secret** key only, the check runs before `preamble`, in the same transaction:
- a cached `true` → `role_not_allowed`;
- a miss → `OWNER_LEVEL_SQL`. No row → `false` (the preamble then reports the missing role as today). Cache the answer.
- The error message keeps the spec's text ("can bypass row-level security") for every owner-level reason.

**Tests to write:** `owner_level_sql_is_pinned` (string test: all five attributes and `neon_superuser`); `owner_level_check_is_cached_per_branch_and_role` (a fake that counts queries: two requests, one query; another role, a second query); `publishable_keys_skip_the_check`; `role_not_allowed_body` (403, code, message).

**Verify:** `test -p rest`, clippy.

---

### Task 8: Worker B: resync hooks, branch copies, protected children, rollout, RPCs

**Files:** `services/worker/src/{branch_create.rs,branch_switch.rs,reconcile.rs,main.rs,provision.rs,data_api.rs}`.

**Interfaces:**
- **Consumes:** Task 4's `resync_locked`/`resync`, `create_role`/`reset_role`/`delete_role`; Task 1's `NewRoleCopy`, `set_compute_settings_version`, `BranchRecord.protected`.
- **Produces:**
```rust
pub const COMPUTE_SETTINGS_VERSION: i32 = 1;   // lib.rs
// reconcile: Action::EnforcePlainRoles(String) → Action::Resync(BranchId, String /*ep*/) (same rank),
//            new Action::UpgradeCompute(BranchId, String) (rank right after RecreateBranchCompute).
// EndpointState gains `compute_settings_version: i32` (Task 1 adds the column; this task reads it).
```

**Behaviour:**
- **Branch create** (`record_branch_endpoint`):
  - The parent's rows come from `role_copies(parent_ep)`. Each non-owner row becomes a `NewRoleCopy` with the same origin, level, login and `api_owner_level`.
  - Unprotected parent: the same verifier, and the sealed password (if any) re-opened with the parent's AAD and re-sealed for the child.
  - Protected parent (read now, R17): a fresh password, verifier and sealed password per row, `pending = true`.
  - MD5 rows are copied as they are.
- **After the child's compute is up and bootstrapped** (where `enforce_plain_roles` ran):
  - for a protected parent, one `admin_sql` `ALTER ROLE <ident> PASSWORD <literal>` for each copied row that exists in `pg_roles` (missing ones are left to the resync);
  - then `resync_locked`;
  - then the branch is ready. Errors fail the create with `syncing login roles: …`.
  - A child from a past point gets that time's SQL roles through the resync's "database wins" rule. API rows are re-created by its "control plane wins" rule.
- **`recreate_compute`** (restore, reset-to-parent, a recreated compute): `resync_locked` after `bootstrap_data_api_on` (it replaces `enforce_plain_roles`), with the same error prefix. Then `set_compute_settings_version(ep, COMPUTE_SETTINGS_VERSION)`.
- **Provision** writes `compute_settings_version = COMPUTE_SETTINGS_VERSION` for new endpoints. The main `branch_databases` row comes from `NewEndpoint`.
- **Reconcile:**
  - the health-check batch (the `schema_check_cursor` round robin) plans `Resync(branch, ep)` for every branch in the batch with a compute; executing it runs `resync` and logs failures (WARN), retried next pass;
  - `UpgradeCompute` for ready `normal` branches whose endpoint's version is below the constant. Executing it removes the container and runs `recreate_compute` (R11), serially like `RecreateBranchCompute`.
- **RPC wiring** (`main.rs`): `RoleCreateRequest.owner_level` → `create_role`. The `role_privilege_mismatch` reply is no longer produced.
- **`data_api.rs`:** the secret-key refusal hint reads `(owner-level roles can't back a secret key; use a plain role)`.

**Tests to write:**
- `child_copies_keep_parent_passwords` (same verifiers; sealed passwords open with the child's AAD only).
- `protected_parent_gives_fresh_passwords_and_alters_them` (different verifiers; the `ALTER ROLE` SQL before the resync SQL in the journal).
- `child_create_runs_the_resync_before_ready` and `child_create_fails_with_syncing_prefix`.
- `recreate_compute_resyncs_and_records_the_settings_version`.
- `reconcile_plans_resync_in_the_health_batch`; `reconcile_upgrades_old_computes_once` (version 0 → `UpgradeCompute`; after it, no action).
- `role_rpcs_reply_with_owner_level`.

**Verify:** `test -p worker`, clippy, `cargo test --workspace` after merging into `login-roles`.

---

### Task 9: Studio

**Files:** `apps/studio/src/components/{branch-roles.tsx,branch-actions.tsx,database-actions.tsx,data-api-keys.tsx}` and their tests.

**Behaviour (spec Part 6; generated hooks only; passwords never in the query cache, as revision 1):**
- **Roles panel:**
  - columns Name, Origin badge (*SQL* / *Studio* / *Owner*), Level badge (*Plain* / *Owner-level*), Login, Password, Created, Actions;
  - Password shows "Generated", "Password set in SQL — reset to get a revealable one", or an MD5 warning ("MD5 password: can't log in through Cheladb; reset it");
  - blocked keys: "Data API keys blocked: <names>";
  - create sends `owner_level`;
  - reset on every role; delete on every role but the owner;
  - Reveal hides the password field when `password` is null and shows the state text.
- **Connection panel:** a role picker (as before) plus a database picker from `useListBranchDatabases` (main first, default main; the system `postgres` database offered with a *System* label); each change re-fetches `getBranchConnection(…, { role, database })`. When `password` is null, show the `password_state` text instead of a password.
- **Main database missing (Q1):** when the branch's `main_database_missing` is true, the branch shows "Main database missing: create a database named `<d>` to bring it back".
- **Branch actions:**
  - a **Protect** toggle (normal branches) with a confirm: "Children of a protected branch get new passwords for every role. It can't be deleted or reset to its parent until you unprotect it. Restore stays allowed.";
  - when protected, Delete and Reset-to-parent are disabled with "Unprotect the branch first";
  - the database's Delete is disabled while any branch is protected ("A branch is protected").
- **Data API keys:** a **Blocked** badge, with "the role `<r>` can bypass row-level security", for keys listed in their role's `blocked_keys` (R21).
- New Studio wording is listed in the Status as rulings at the checkpoint (Q7 covered the receiver's texts only).

**Tests to write:** `roles_show_origin_level_and_password_state`, `sql_role_reset_offers_revealable_password`, `md5_warning`, `owner_has_no_delete`, `create_sends_owner_level`, `database_picker_refetches` (system database offered), `main_database_missing_notice`, `protect_toggle_confirms_and_patches`, `protected_branch_disables_delete_and_reset`, `database_delete_disabled_with_protected_branch`, `blocked_key_badge`, `passwords_never_enter_the_query_cache`.

**Verify:** `pnpm --filter @cheladb/studio test`, `typecheck`, `build`, `just check`. The Playwright pass runs in Task 11.

---

### Task 10: Docs

**Files:** `README.md`, `docs/roadmap/2026-09-28-neon-and-data-api.md` (the 6a entry only; the coordinator owns "Resume here").

- **README, "Roles and migrations":**
  - what works in SQL (create, re-password, rename, drop, grant, create database);
  - password rules per mode;
  - pre-hashed passwords and MD5;
  - the caps and their settings;
  - the branch password rule (owner per branch; others keep the parent's unless it's protected);
  - protected branches;
  - the control-plane-down error: Neon's curl text, what it means, and that it will be replaced (Q2, fork task F2);
  - dropping the main database is accepted (the data is gone, and the data API is unavailable until a database of that name is created again); renaming it is refused (Q1, fork task F1 will refuse the drop);
  - the owner can `SET ROLE` to, and inherit, roles it creates (`createrole_self_grant = 'set, inherit'`, Q9);
  - the system `postgres` database is listed, and isn't counted in the cap;
  - roles made in SQL before this change can log in after the first resync.
  - Replace revision 1's "a role created in SQL can't log in" text.
- **Roadmap:**
  - the 6a entry's outcome;
  - step **6c** (data API across databases and schemas, with database/schema switching in Studio) after 6a and before 6b;
  - the per-project database cap note;
  - a note that the documentation site must highlight the branch password rule.

**Verify:** links resolve; no machine-specific details (CPU, RAM) anywhere.

---

### Task 11: Live tests, `just role-e2e`, Playwright pass

**Files:** `services/worker/tests/roles_live.rs` (rewrite; revision 1's header and harness), `scripts/role-e2e.sh` (rewrite), `scripts/fixtures/role-migration.sql` (create), `justfile` (`role-e2e` unchanged name).

**Setup:**
- Restart the deps stack and `just dev` from this worktree first, so the orchestrator serves `neon.console_url` and `max_roles=64`.
- Each test provisions its own database and removes it, pass or fail.
- Logins go through the proxy on `:4432` (`hostaddr=127.0.0.1`, `sslmode=verify-full`).

**Live tests:**
- `forwarding_is_off_in_admin_sql` (`SHOW neon.forward_ddl` → `off`).
- `migration_script_runs_through_the_proxy`, as `postgres` via psql:
  - `CREATE ROLE app LOGIN PASSWORD`; log in as `app` at once;
  - `ALTER ROLE app PASSWORD` (the old one is refused and the new one works at once);
  - `ALTER ROLE app RENAME TO app2` (a rename clears an MD5 hash only; SCRAM keeps working);
  - `GRANT`; `CREATE DATABASE extra` and connect to it as `app2`;
  - `DROP ROLE app2`, then login is refused.
- `quoted_names_work`: `CREATE ROLE "My ""Role""" LOGIN PASSWORD …`, log in with that user, reset and delete it through the worker.
- `savepoint_and_multi_statement_transactions`: a role created then `ROLLBACK TO SAVEPOINT` has no copy; create + rename + drop in one transaction leaves nothing.
- `restore_before_the_migration_and_rerun`: after a restore, the SQL roles are gone (no `already exists` on re-run); an API role made after the restore point still logs in.
- `child_keeps_passwords_owner_differs`; `protected_child_gets_fresh_passwords` (the parent's passwords refused on the child; the revealed ones work).
- `md5_refused` (the error text in the psql output); `pg_dumpall_style_scram_accepted` (self-hosted).
- `weak_password_with_strength_on` (the orchestrator restarted with `CHELADB_PASSWORD_STRENGTH=on`, or a unit-level substitute if restarting is impractical; say which).
- `key_role_drop_refused` (`role_in_use` text in the DDL error).
- `control_plane_down_fails_the_ddl` (stop the orchestrator's internal listener: `just dev` app stop/start for the orchestrator only; the DDL fails and no role exists).
- `forwarding_off_then_resync_picks_up` (create a role via `admin_sql` with forwarding off; `resync` → a copy; login works).
- `owner_level_by_sql_blocks_the_key`: `ALTER ROLE r BYPASSRLS` (R16) → the data API answers 403 `role_not_allowed` within 30 s; the same for `ALTER ROLE r CREATEDB` (Q3); and an indirect membership via an API owner-level role.
- `database_cap_refusal_leaves_no_database` (the cap set low by env; `CREATE DATABASE` past it fails and the catalog has no such database; the system `postgres` database isn't counted).
- `owner_can_use_roles_it_creates` (Q9): as `postgres`, `CREATE ROLE app …` then at once `ALTER TABLE t OWNER TO app` and `SET ROLE app` succeed.
- `main_database_drop_is_mirrored` (Q1): as `postgres` from another database, `DROP DATABASE <main>` succeeds; `GET …/databases` has no main row and the branch shows `main_database_missing`; the data API answers its unavailable error; `CREATE DATABASE <main>` brings the `main` row back. `ALTER DATABASE <main> RENAME` is refused with the approved text.
- `system_database_listed_and_connectable` (Q8): it's in `GET …/databases` with origin `system`, and the reveal with `?database=postgres` connects.
- `owner_password_set_in_sql` (`ALTER ROLE postgres PASSWORD` → login with the new password works through the proxy; the reveal says `set_in_sql`).
- `rest_live`: add `secret_key_of_an_owner_level_role_is_refused`.

**`just role-e2e`** (through the gateway and the proxy):
- `scripts/fixtures/role-migration.sql` holds a realistic migration: two roles, a group role, grants, a database.
- Run it with psql as the owner; list roles (origins `sql`, password `set_in_sql`); reveal `?role=` (no password).
- Reset one role (revealable now) and log in; create a secret key on it; `ALTER ROLE … BYPASSRLS` in SQL → the key gets 403 `role_not_allowed`, and `listRoles` shows `blocked_keys`.
- Protect the branch: delete → 409 `branch_protected`; unprotect.
- Re-run the migration after a restore to before it (it succeeds).
- `KEEP=1` keeps the database. Prints `ok …` lines with timings.

**Also run, one at a time:** `just neon-e2e`, `just data-e2e`, `just branch-e2e`, `rest_live`, `neon_live`, `branch_live`, `roles_live`.

**Playwright pass** (sonnet subagent with the Playwright MCP, or CDP as in revision 1): roles panel badges, reset of an SQL role, the database picker, the Protect toggle and the disabled actions, a Blocked key. Delete the test database afterwards.

**Verify:** all of the above green; the test databases removed; timings recorded in the Status.

---

## Review Focus

These are the input classes most likely to bite users that the tasks' ordinary tests wouldn't catch. Each now has a test in its owning task.

1. **Role names Postgres allows but ours didn't** (`"My ""Role"""`, uppercase, spaces, `|`, non-ASCII, 63 bytes) through the receiver, the resync's parsing, the API path parameter, the reveal URI and the worker's SQL. Tests: T2 `any_name_accepts_quoted_shapes`, `quoting`; T4 `delete_accepts_quoted_names`, `parse_observed_decodes_hex_and_refuses_garbage`; T6 `reset_and_delete_accept_sql_names`, `connection_by_role_and_database` (percent-encoding); T11 `quoted_names_work`.
2. **Transactions that don't commit as written:** savepoint rollbacks, create-rename-drop in one transaction, `DROP ROLE IF EXISTS` of a missing role, and a compute retrying a PATCH that timed out. Tests: T5 `ddl_is_idempotent`, `ddl_first_refusal_rolls_back_everything`; T11 `savepoint_and_multi_statement_transactions`.
3. **The resync racing a forward:** a role created between the resync's catalog read and its write must not lose its copy. Tests: T1 `resync_skips_recently_forwarded_rows`, `resync_version_guard`; T4 `resync_retries_when_the_version_moves`.
4. **Password shapes from tools:** `\password`/`pg_dumpall` SCRAM literals, MD5 literals, an empty password, `PASSWORD NULL` on a role with a copy, and an 11-character "strong" password. Tests: T2 `forwarded_password_table`; T5 `ddl_rules_table`, `ddl_body_never_contains_the_password`; T11 `md5_refused`, `pg_dumpall_style_scram_accepted`.
5. **Branches from the past and protected parents:** a child from before a migration gets that time's SQL roles plus today's API roles; a protected parent's child refuses every parent password. Tests: T8 `protected_parent_gives_fresh_passwords_and_alters_them`, `child_copies_keep_parent_passwords`; T11 `restore_before_the_migration_and_rerun`, `protected_child_gets_fresh_passwords`.

## Self-review notes

- **Spec coverage:**
  - Part 1 → Task 1 (+3 for the owner-only spec);
  - Part 2 → Tasks 2, 3, 5;
  - Part 3 → Tasks 1 (query), 4, 8;
  - Part 4 → Tasks 1, 6, 8;
  - Part 5 → Tasks 4 (key check kept), 7;
  - Part 6 → Tasks 6, 9;
  - Part 7 → Tasks 1, 8 (rollout), 10, 11.
- **Deviations:** all listed under Status (R1–R24). The user's answers Q1–Q9 are applied in Tasks 1–11. Fork tasks F1–F2 are queued for the fork session; 6a doesn't wait for them.
