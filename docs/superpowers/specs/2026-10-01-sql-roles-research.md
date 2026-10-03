# SQL-managed roles through the proxy: research for the 6a re-spec

Date: 2026-10-01. Read-only research. Fork: the ChelaDB-neon checkout at `27c74fe88` (not built or modified). Cheladb: worktree `.worktrees/login-roles` (6a as built, unmerged).

Goal: a project's existing migrations (`CREATE ROLE x LOGIN PASSWORD '…'`, `ALTER ROLE x PASSWORD`, `ALTER ROLE x RENAME TO y`, `DROP ROLE x`, `GRANT a TO b`, `CREATE DATABASE`) should just work, even though every client connection goes through Neon's proxy, which gets role secrets from Cheladb's control plane rather than from the compute's `pg_authid`.

---

## 0. The facts that shape every design

1. **The proxy passes SCRAM keys through to the compute, and the compute checks them again.**
   - The proxy runs SCRAM against the verifier the control plane gave it. It then logs in to the compute with the derived `ClientKey`/`ServerKey`, not with the password (`proxy/src/auth/backend/classic.rs:33-55`, `proxy/src/compute/mod.rs:175-204`, `libs/proxy/postgres-protocol2/src/authentication/sasl.rs:229-272`).
   - So the compute's `pg_authid` verifier must be **byte-identical** to the control plane's (same salt, iterations and keys). PgBouncer documents the same constraint for its SCRAM pass-through: "the SCRAM secrets are identical in PgBouncer and the PostgreSQL server" (https://www.pgbouncer.org/config.html).
   - **Consequence (safety):** a stale or wrong secret on the proxy side can only *refuse* a correct password. It can never let a wrong one in, because the compute re-verifies. Staleness costs availability, not security.
   - **Consequence (today's bug):** if the owner runs `ALTER ROLE postgres PASSWORD '…'` in SQL today, `pg_authid` and `neon_roles` diverge. The new password is refused at the proxy and the old one is refused at the compute, so logins break until the next reconfigure rewrites the old verifier.
2. **The proxy accepts only SCRAM secrets.**
   - `ServerSecret::parse` fails on anything else, which is `BadSecret` (`proxy/src/control_plane/client/cplane_proxy_v1.rs:180-185`).
   - A 404 from the control plane is treated as "no secret" (`:165-176`).
   - MD5 can't work through the proxy at all.
3. **Proxy cache.**
   - Cheladb runs `--project-info-cache=size=10000,ttl=1m,max_roles=10` (`infra/compose/docker-compose.yml:362`). Note `max_roles=10` per endpoint: more SQL roles than that will churn the cache.
   - Invalidation is the `/password_updated` Valkey message (`proxy/src/redis/notifications.rs:58`), which Cheladb already publishes (`crates/cheladb-neon/src/proxy_cache.rs:15-67`).
4. **`compute_ctl` and roles.**
   - It touches only roles listed in `spec.cluster.roles` plus the `delta_operations`; any other role in `pg_authid` is left alone (`compute_tools/src/spec_apply.rs:786-828`, `:1217-1229`).
   - For a listed role, it runs `ALTER ROLE … LOGIN PASSWORD <spec verifier>` when the verifier differs; Neon's own comment warns it "can be run on /every/ role" (`spec_apply.rs:796-801`).
   - It creates a missing role with `IN ROLE neon_superuser` unless `privileged == Some(false)` (the fork's Task 11b, `spec_apply.rs:1294-1312`; field at `libs/compute_api/src/spec.rs:553`).
   - **Every spec role is forced to `LOGIN`**, and a non-`SCRAM-SHA-256` value is prefixed with `md5` (`compute_tools/src/pg_helpers.rs:146-167`). So passwordless group roles (`NOLOGIN`) and MD5 values must never enter the spec.
5. **PG16+ ADMIN option.**
   - A non-superuser `CREATEROLE` creator automatically gets ADMIN OPTION on the role it creates (`vendor/postgres-v17/src/backend/commands/user.c:538-560`).
   - So roles that `postgres` creates in SQL are fully manageable by `postgres` (rename, drop, grant, password).
   - Roles that `compute_ctl` creates (as `cloud_admin`) are not. This is the 6a pause note.

---

## 1. Neon's DDL forwarding (`pgxn/neon/neon_ddl_handler.c`)

### What it is and when it sends
- It is a `ProcessUtility_hook` (`:1283-1352`, installed at `:1384-1394`) that records role and database changes into per-subtransaction hash tables (`:94-105`, merge/pop at `:391-509`).
- It sends one JSON delta per top-level transaction **at `XACT_EVENT_PRE_COMMIT`** (`:532-541`).
- The header comment admits that the transaction "may abort AFTER changes have already been forwarded, and that case is not handled" (`:10-11`).
  - In PG17 the pre-commit callbacks run at `xact.c:2231`. `PreCommit_Notify` (`:2279`), the serializable check (`:2289`) and `RecordTransactionCommit` (`:2315`) all come after, so a phantom forward is possible but rare.
- **Recording happens before the statement runs** (the hook records, then calls `standard_ProcessUtility`).
  - A statement that then errors aborts the transaction, so nothing is sent. Good.
  - `DROP ROLE IF EXISTS ghost` still sends a `del` for a role that never existed, so the receiver must be idempotent.
- `PREPARE TRANSACTION` isn't handled (the callback clears the tables on any event). Irrelevant while `max_prepared_transactions = 0`.

### What it intercepts (`:1296-1327`)

| Statement | Recorded |
|---|---|
| `CREATE ROLE/USER` (`HandleCreateRole`, `:678-708`) | `set name`, plus `password` if a `PASSWORD` option has a non-NULL arg |
| `ALTER ROLE … PASSWORD` (`HandleAlterRole`, `:710-754`) | **only when a PASSWORD option is present** (`:735-741`). `PASSWORD NULL` gives a `set` with no password. `LOGIN`/`NOLOGIN`, `VALID UNTIL` and attributes are **not** forwarded. It refuses to ALTER `neon_superuser` unless superuser (`:722-723`). |
| `ALTER ROLE … RENAME` (`HandleRoleRename`, `:756-795`) | `set new_name`, `old_name` (chains collapse within a transaction) |
| `DROP ROLE` (`:797-820`) | `del` for each name |
| `CREATE DATABASE` (`:552-591`) | `set name owner`. Refuses `OWNER neon_superuser` (`:582-583`). |
| `ALTER DATABASE … OWNER` (`:593-619`) | `set name owner`. Refuses `neon_superuser` (`:614-615`). |
| `ALTER DATABASE … RENAME` (`:621-658`) | `set new old_name` |
| `DROP DATABASE` (`:660-676`) | `del` |
| `CREATE TABLESPACE` | refused outright (`:1319-1326`) |
| `GRANT a TO b`, `ALTER ROLE … SET`, `ALTER ROLE … [NO]LOGIN`, attributes | **not intercepted** |

Utility statements inside PL/pgSQL (`DO`, functions) go through `ProcessUtility` too, so they are captured.

### The payload (`ConstructDeltaMessage`, `:125-221`)

```json
PATCH <neon.console_url>
Content-Type: application/json
Authorization: Bearer $NEON_CONTROL_PLANE_TOKEN
{"roles":[{"op":"set|del","name":"x","password":"<as typed>","encrypted_password":"<pg_authid.rolpassword>","old_name":"y"}],
 "dbs":[{"op":"set|del","name":"d","owner":"r","old_name":"e"}]}
```

- **`password` is the literal from the statement** (`:190`).
  - That is plaintext for `PASSWORD 'secret'`.
  - It is the **verifier itself** when the client sent a pre-hashed one. That happens with `psql`'s `\password`, pgAdmin and `pg_dumpall` output (`PASSWORD 'SCRAM-SHA-256$4096:…'`) and MD5 strings.
- **`encrypted_password`** is read inside the transaction with `get_role_password()` (`:191-198`). With Cheladb's `password_encryption=scram-sha-256` (`crates/cheladb-neon/src/spec.rs:172`), it is the exact SCRAM verifier that is stored: the one the proxy needs.
- **Failure modes:**
  - `get_role_password` returns NULL for a missing, empty or **expired** password (`vendor/postgres-v17/src/backend/libpq/crypt.c:36-80`). The handler then `elog(ERROR)`s, so `CREATE ROLE x PASSWORD 'p' VALID UNTIL '2000-01-01'` and `PASSWORD ''` (Postgres clears it with a NOTICE) both fail at commit with "Failed to get encrypted password".
  - An MD5 literal produces an MD5 `encrypted_password`, which the receiver should refuse.

### Transport and auth
- It uses curl, method `PATCH`, a 3 s timeout (`:286-291`), and up to 5 retries with a 1 s sleep, **retrying only on transport errors and timeouts** (`:295-308`).
  - A request that timed out may already have been applied, so the receiver must be idempotent.
- **Any HTTP status other than 200 is `ERROR "Received HTTP code %ld from control plane: <body ≤1 KiB>"`, which aborts the user's transaction** (`:316-334`). A receiver can therefore refuse a DDL with a readable message.
- For `DROP DATABASE` (not transactional) a refusal leaves the database marked invalid (`datconnlimit = -2`). The fork's `test_ddl_forwarding.py:241-262` checks exactly this, and a repeated drop then succeeds. `compute_ctl` has a `DropInvalidDatabases` phase.
- The bearer token is `getenv("NEON_CONTROL_PLANE_TOKEN")` at load (`:1444-1448`). **Cheladb already sets this env var to the per-compute token** (`crates/cheladb-neon/src/compute.rs:17`), whose SHA-256 is in `neon_endpoints.compute_token_sha256` and authenticates the spec fetch (`services/orchestrator/src/internal/spec.rs:39-70`).

### GUCs (`:1396-1442`)
- `neon.console_url`: `PGC_POSTMASTER`, default NULL. When unset, the handler logs "ConsoleURL not set, skipping forwarding" (`:254-258`). **This is Cheladb today: the hook runs but nothing is sent.**
  - It can be set per endpoint through `spec.cluster.settings`, which `compute_ctl` writes into `postgresql.conf`.
  - It is one fixed URL, so the endpoint id goes in the path, e.g. `http://<internal>/compute/api/v2/computes/<ep>/ddl`.
- `neon.forward_ddl`: bool, default on, **`PGC_SUSET`**. The owner (`postgres`) can't turn it off; superusers can.
  - `compute_ctl` sets it off in its own sessions "because control plane already knows" (`compute_tools/src/compute.rs:1938-1944`, `:1963-1968`).
  - **Cheladb's `admin_sql` sessions (as `cloud_admin`) must also set it off.** Otherwise the worker's own `DROP ROLE`, the bootstrap `CREATE ROLE anon/authenticated/authenticator/cheladb_admin`, and so on would be forwarded back. A worker holding the endpoint lock while its `DROP ROLE` commit waits on a PATCH that wants the same lock is a 3 s × 5 stall that then errors.
- `neon.regress_test_mode`, `neon.event_triggers`: unrelated.

### What Neon's control plane is expected to do
- The reference behaviour is `test_runner/regress/test_ddl_forwarding.py`:
  - apply renames first: move the row and re-point database owners (`:37-48`);
  - store the password on `set` with a password, asserting that `encrypted_password` is present (`:45-47`);
  - delete on `del`;
  - answer 200;
  - a 500 makes the statement fail (`:233-240`).
- Its public docs (below) show it also enforces password rules on the plaintext. That is why the plaintext is sent at all.
- Afterwards the role is in the control plane's list, so the next spec lists it with the same `encrypted_password`. `compute_ctl` sees equal verifiers and does nothing (`spec_apply.rs:795-813`), so **it doesn't fight forwarded state**.
- The fight appears only when `pg_authid` and the control plane diverge (restore, phantom forward, a forward that was off). Then the control plane wins: the spec rewrites the verifier, or re-creates the role, and re-creates it owner-level unless `privileged:false`.

### `neon_superuser` / `privileged`
- SQL-created roles are **not** put in `neon_superuser` by the hook; nothing in the handler grants anything.
- Neon's docs confirm SQL-created roles get "only … the basic public schema privileges" (https://neon.com/docs/manage/roles).
- Only console, API and spec roles go through `create_role_query`'s `IN ROLE neon_superuser` (`spec_apply.rs:1296-1312`). Cheladb's fork field `privileged: Some(false)` gives the plain form (fork test `test_compute_plain_roles.py`).
- `is_privileged_role()` (in `vendor/postgres-v17/src/backend/utils/adt/acl.c:136-152`) is only used for the event-trigger sudo path and for the `neon.event_triggers` GUC.

### Also in the fork: `compute_ctl` `GET /dbs_and_roles`
- It returns every `pg_authid` row as `{name, encrypted_password}` plus the databases (`compute_tools/src/catalog.rs:19-36`, `pg_helpers.rs:269-280`).
- It is on the external port behind the compute JWT (`compute_tools/src/http/server.rs:100-125`), which the worker already signs for `/configure`.
- This is the natural "read the timeline's truth" call for a resync.

---

## 2. Neon's proxy: are there fallbacks that avoid a control-plane copy?

Auth backends (`proxy/src/binary/proxy.rs:89`, `:744-930`):

- **`control-plane`** (Cheladb's): `get_endpoint_access_control` → SCRAM secret (`cplane_proxy_v1.rs:132-200`).
  - There is no fallback and no "refresh on failure". The cached secret (Cheladb: 1 min) is served until TTL or `/password_updated`.
- **`postgres`** (the "mock", `proxy/src/control_plane/client/mock.rs:69-131`): runs `select rolpassword from pg_catalog.pg_authid where rolname = $1` (`:85`) against **one fixed Postgres URL** and wakes to that same URL.
  - It is an `auth_query` in all but name, but single-instance and dev-only: it logs the secret (`:89`), uses no TLS, and isn't multi-tenant.
  - Useful as a reference for an auth_query mode, not usable as-is.
- **`console-redirect`** ("link auth"): the user authenticates in a browser and the console hands the proxy credentials. Not for apps or migrations.
- **`local`** (`local_proxy`, JWT/`pg_session_jwt`): for compute-side HTTP. Not password auth.
- **Password hack** (`proxy/src/auth/password_hack.rs`): only carries the endpoint id inside the password for clients without SNI. It doesn't change where the secret comes from.

**Conclusion:** stock Neon has no mode where the proxy trusts `pg_authid` for a multi-tenant control-plane deployment. Either the control plane holds a copy (Neon's design, kept in sync by DDL forwarding), or Cheladb's **internal listener** (which the proxy already calls) looks the secret up in the compute. The second needs no proxy change at all.

---

## 3. How others do it

| Provider | SQL-created roles through the pooler/proxy | Notes |
|---|---|---|
| **Neon** | Yes, through DDL forwarding to the console | SQL roles are not in `neon_superuser`. Passwords need **≥ 60 bits of entropy (~12+ chars, mixed classes)**, and "Passwords must be supplied in plain text … Hashed passwords are not supported" (https://neon.com/docs/manage/roles). Known failures: `Received HTTP code 400 from control plane: Neon does not support roles with empty passwords` (https://github.com/logto-io/logto/issues/4535); refusals of SCRAM/MD5-shaped values, with a project reporting a 32-char rule (https://github.com/SebastianFrazier26/SafeAscent/pull/6, second-hand). Child branches duplicate roles (https://neon.com/docs/manage/database-access); branches of **protected** branches get **new passwords**, preserved on later reset/restore since 2024-09-06 (https://neon.com/docs/guides/protected-branches). Restore docs say data and schema are replaced but are silent on roles (https://neon.com/docs/introduction/branch-restore.md). Historic pain: owners couldn't create or grant roles (https://github.com/neondatabase/neon/issues/2088). |
| **Databricks Lakebase** (Neon-derived) | Yes, `CREATE ROLE … LOGIN PASSWORD` | SQL roles **appear in the UI**, and passwords are "validated at creation time to verify 60-bit entropy" (https://docs.databricks.com/aws/en/oltp/projects/postgres-roles). This is DDL forwarding plus the entropy check, as in Neon. |
| **Supabase / Supavisor** | Yes, through `auth_query` against the tenant DB, using a manager user | Credentials live in an in-memory cache, so right after a password change new logins can be rejected; the pool refreshes "within roughly 15 seconds" or after a reconnect (https://supabase.com/docs/guides/troubleshooting/supavisor-error-password-authentication-failed-after-password-rotation). PR #1213 (merged 2026-09-28): on auth failure, refetch secrets. Cleartext retries succeed; **SCRAM can't be retried in the same exchange ("used the old salt")**, so the client gets 28P01 with a hint to reconnect (https://github.com/supabase/supavisor/pull/1213). |
| **PgBouncer** | Yes, `auth_user` + `auth_query` | Default `SELECT rolname, CASE WHEN rolvaliduntil < now() THEN NULL ELSE rolpassword END FROM pg_authid WHERE rolname=$1 AND rolcanlogin`. SCRAM pass-through needs identical secrets (https://www.pgbouncer.org/config.html). Best practice is a `SECURITY DEFINER` lookup function, not raw `pg_authid` access (https://www.enterprisedb.com/postgres-tutorials/pgbouncer-authquery-and-authuser-pro-tips). |
| **Crunchy Bridge** | Yes. `CREATE EXTENSION crunchy_pooler` creates a `crunchy_pooler` user that can call one `user_lookup` function | **Superuser and replication roles can't connect through PgBouncer.** API roles are optional; SQL `CREATE ROLE` works (https://docs.crunchybridge.com/how-to/pgbouncer). |
| **PlanetScale Postgres** | Yes, as `user.<branch_id>` for routing | SQL roles "will not display on your database settings". On restore or PITR, user-defined roles are copied and "their passwords are reset" (https://planetscale.com/docs/postgres/connecting/roles). |
| **Aiven** | PgBouncer pools per user and database | Couldn't confirm details in docs (https://aiven.io/blog/aiven-postgresql-connection-pooling). |
| **Xata, Nile** | No public documentation found on SQL roles through their proxies | Xata's SQL proxy authenticates through its own API (https://xata.io/blog/sql-over-http). |
| **AWS RDS Proxy** (counter-example) | **No**: every DB user needs its own Secrets Manager secret (or end-to-end IAM) | A `CREATE ROLE` in SQL is invisible to the proxy until someone adds a secret (https://docs.aws.amazon.com/AmazonRDS/latest/UserGuide/rds-proxy-iam-setup.html). |

**Pitfalls to design against:**
1. Stale credential caches after a password change (Supavisor's 15 s; Cheladb's 1 min TTL).
2. Hashed passwords from tools: Neon refuses them, which breaks `\password` and `pg_dumpall`.
3. Password-strength refusals breaking existing migrations (Neon's 60-bit rule).
4. Empty-password and group roles (Neon refuses "roles with empty passwords", which breaks `CREATE ROLE x NOLOGIN` group roles in seeds; see the logto issue).
5. A failing forward makes the DDL fail.
6. Resurrecting roles on restore.

---

## 4. Approaches for Cheladb

Common ground for all three:
- the proxy contract is unchanged;
- grants (`GRANT a TO b`) need no control-plane involvement, since they live in `pg_auth_members` on the timeline and Postgres enforces them;
- `CREATE DATABASE` already works through the proxy (the proxy doesn't check database names; `postgres` has `CREATEDB`).

The data API stays safe in every option because:
- `check_data_role` checks the live catalog for superuser, system attributes and `neon_superuser` membership at any depth (`services/worker/src/data_api.rs:251`, `:481`);
- `GRANT <role> TO authenticator` is issued by the worker as `cloud_admin` and re-issued after restore (`data_api.rs:318`).

### A. DDL forwarding into `neon_roles` (Neon's own design)

**Fork:**
- Nothing is required: the hook is compiled in, and `neon.console_url` comes from the spec.
- Recommended small fork changes (one task, sonnet):
  - add `"login": rolcanlogin` (and maybe `valid_until`) to each role entry;
  - forward `ALTER ROLE … [NO]LOGIN` too. Today only PASSWORD triggers a forward (`:735-741`), and `compute_ctl` forces `LOGIN` on every spec role (`pg_helpers.rs:149`).
  - optional: `CURLOPT_TIMEOUT` as a GUC, or an idempotency key.

**Cheladb:**
- **Internal route** `PATCH /compute/api/v2/computes/{ep}/ddl`. Bearer = that compute's token (same check as the spec route, `internal/spec.rs:39-70`). It scopes every write to `{ep}` only, is idempotent, answers 200 or a 4xx/409 with a human message, and takes a **short** lock (`lock_timeout` < 3 s, or 503).
- **Spec:** add `setting("neon.console_url", "<internal>/compute/api/v2/computes/<ep>/ddl", "string")` to `config_response` (`crates/cheladb-neon/src/spec.rs:160-173`). It is `PGC_POSTMASTER`, so it takes effect at the next compute start (`recreate_compute`).
- **`admin_sql`**: `SET neon.forward_ddl = off` in every worker session.
- **Metadata:**
  - a role origin, e.g. `kind IN ('owner','login','sql')`, or an `origin` column;
  - `password_enc` nullable for SQL rows (sealed when a plaintext arrived, NULL when the SQL supplied a verifier);
  - `scram_verifier` nullable (group or passwordless roles);
  - the spec emits only rows with a SCRAM verifier and login, always `privileged:false` for SQL rows.
- **`enforce_plain_roles`** (`services/worker/src/roles.rs:525`) **must skip SQL-origin rows**. Otherwise a migration's `CREATE ROLE migrator CREATEDB …` or `GRANT neon_superuser TO x` gets demoted by reconcile, which breaks "just works".
- **Proxy cache:** publish `/password_updated` for each touched role (new and old name).
- **Resync** (timeline → metadata, for SQL-origin rows) from `compute_ctl GET /dbs_and_roles`:
  - at rollout (existing branches already hold un-forwarded SQL roles);
  - after restore, reset-to-parent and branch-create;
  - in the reconcile round-robin, to repair phantom forwards.

**Each statement:**

| Statement | Behaviour |
|---|---|
| `CREATE ROLE x LOGIN PASSWORD 'pw'` | Row inserted, verifier stored, plaintext sealed (revealable), cache published. Works at once. Optional strength check (refusal = 4xx = the statement fails). |
| …`PASSWORD 'SCRAM-SHA-256$…'` | Accept: verifier stored, no reveal. Better than Neon. |
| …`PASSWORD 'md5…'` | Refuse, 4xx: "MD5 passwords can't log in through Cheladb's proxy; use SCRAM". |
| `CREATE ROLE g NOLOGIN` / no password | Stored as a non-login SQL row, or ignored. Never in the spec. |
| `ALTER ROLE x PASSWORD` | Verifier updated, published. On API rows: allowed if `postgres` has ADMIN (see open questions). Today it is refused by Postgres. |
| `ALTER ROLE x RENAME TO y` | Row moved (SCRAM doesn't depend on the name; MD5 is cleared by Postgres anyway). Refuse 4xx for reserved names or a role backing a data API key. |
| `DROP ROLE x` | Row deleted, published. **Refuse 409** while a secret data key uses it (parity with the API delete). |
| `GRANT a TO b` | Not forwarded, works natively. |
| `CREATE DATABASE` | Either record it in a SQL-database list for Studio (keep it **out of the spec** so a restore doesn't resurrect it empty), or answer 200 and ignore. |
| `ALTER ROLE postgres PASSWORD` by the owner | Owner row updated, fixing today's lockout. |

**Branches and restore:**
- The control-plane copy and the timeline diverge by design, so SQL-origin rows need a rule:
  - "**timeline wins for SQL rows**": resync from `/dbs_and_roles` after restore/reset/branch-create (delete rows missing from `pg_authid`, take the timeline's verifier);
  - "**control plane wins for API rows**" (6a as built).
- Without the resync, a restore to before a migration resurrects its SQL role through the spec. Re-running that migration then fails with `role "x" already exists`, a real "existing migrations" break.
- For branch-create of SQL roles: keep the parent's passwords (the timeline copy; Neon's default for unprotected branches) or give fresh ones (6a's API-role rule; Neon's protected branches; PlanetScale's restores). This is a product question.

**Risks:**
- A phantom forward when the commit fails after pre-commit (rare; healed by resync).
- DDL latency (an HTTP round trip inside commit) and DDL failing while the orchestrator is down. Migrations then fail with "Failed to perform curl request", which is the same failure mode as Neon's.
- Lock interplay with worker operations (needs `forward_ddl=off` plus a short lock).
- The plaintext travels compute → orchestrator over the internal network, unencrypted HTTP in dev.
- Cache churn above `max_roles=10`.

**Size:** medium. About one 6a-sized step:
- the route, metadata migration, spec, `admin_sql` and resync job in the worker;
- Studio badges;
- a live test matrix (create, re-password, rename, drop, savepoint, refusal, restore);
- an optional small fork task.

### B. The listener answers from the compute's `pg_authid` (auth_query style, no forwarding)

**Fork:** none. Optional: a proxy change to invalidate the cached secret on auth failure (Supavisor PR #1213's idea), so a fresh SQL password works on the second try instead of after TTL.

**Cheladb:**
- In `access_control` (`services/orchestrator/src/internal/proxy.rs:61`), when `neon_roles` has no row for the role, fetch it from the compute: `compute_ctl /dbs_and_roles` with a JWT, or a dedicated `SECURITY DEFINER` lookup. Filter as PgBouncer does (`rolcanlogin`, not expired, SCRAM only). **Refuse** superusers, `cloud_admin`, `cheladb_admin`, `authenticator`, `neon_superuser` itself and the `pg_*` roles.
- No spec rows or reconcile for SQL roles: `compute_ctl` never touches them.

**Each statement:**
- `CREATE` and `DROP` work once the proxy cache misses (a negative cache of up to 1 min).
- `ALTER … PASSWORD` works after up to 1 min TTL, since there is no event; the old password keeps passing the proxy but fails at the compute.
- `RENAME` works.
- MD5 is refused at login with a confusing "password authentication failed".
- No way to refuse a DDL with a helpful message (e.g. dropping a key-backed role).
- No reveal, no Studio visibility unless Studio also queries the compute.

**Branches and restore:** perfectly consistent: the timeline is the truth, and child branches keep the parent's passwords.

**Risks:**
- The auth path depends on the compute being up and reachable. Fine while computes are always-on (`suspend_timeout_seconds: -1`); a problem with scale-to-zero later, since `get_endpoint_access_control` runs before `wake_compute`.
- An extra query per cache miss.
- The 1-min stale window after SQL password changes. Shortening the TTL raises the load.
- The 6a spec already rejected "a query on the proxy's auth path" for exactly these reasons.

**Size:** small to medium.

### C. Hybrid (recommended): A for events and refusals, plus B-style timeline resync

- **A's forwarding** for immediacy (cache invalidation, reveal, refusals with messages, Studio visibility).
- **The timeline as source of truth for SQL-origin roles:** resync from `/dbs_and_roles` at rollout, after restore/reset/branch-create, and in reconcile.
- **Optionally a listener fallback to `pg_authid` on a miss**, as a safety net for roles not yet synced (pre-rollout roles, phantom aborts, a forward that failed open). The fallback can wait for a later step if computes stay always-on.
- **API-made rows keep 6a's control-plane-wins rule.** SQL-made rows are always plain in the spec (`privileged:false`), never demoted, and never resurrected against the timeline.

**Why C over A or B:**
- it is the only option where all six statements behave like plain Postgres;
- a restore is correct;
- the data API keeps refusals (key-backed roles can't be dropped or renamed out from under it);
- passwords work the moment the transaction commits.

**Size:** A plus the resync (about A × 1.2).

### Plain or owner-level by default for SQL-created roles
- Neon's choice is plain: SQL roles get only `public` privileges.
- Migrations expect plain Postgres semantics: the role gets exactly what the SQL says (`CREATEDB`, `IN ROLE …`, grants).
- So Cheladb should **record SQL roles as `privileged:false`** (so `compute_ctl` never adds `neon_superuser`) and **exempt them from the plain-role demotion**. Whatever the owner granted in SQL stands.
- The data API's own check still refuses owner-level roles for secret keys, at any depth.

---

## 5. Open questions for the product owner

1. **Source-of-truth split.** Are you OK with "the timeline wins for SQL-made roles, the control plane wins for API-made roles", including on restore? The alternative is control-plane-wins for all, which resurrects rolled-back SQL roles and breaks re-running migrations.
2. **Password rules for SQL passwords.** Should there be a Neon-like strength check (60-bit entropy or 12+ chars)? Per CLAUDE.md the recommendation is off by default in self-hosted mode, with an opt-in. What should platform mode do?
3. **Pre-hashed passwords.** Accept SCRAM verifiers from `\password` and `pg_dumpall` (recommended; Neon refuses them), knowing such roles can't be revealed in Studio? Refuse MD5 with a clear message?
4. **Reveal.** Should SQL-set plaintext passwords be sealed and revealable in Studio like API roles, or only verifiers stored (no plaintext kept)? Should the fork stop sending `password` at all if no strength check is wanted?
5. **API roles in SQL.** Give `postgres` ADMIN on API-made login roles (so `ALTER ROLE … PASSWORD`, `RENAME`, `DROP` and `GRANT x TO y` work in SQL and are forwarded), or keep them API-owned?
6. **Branch create for SQL roles.** Keep the parent's passwords (timeline copy; Neon's default) or fresh passwords like 6a's API roles?
7. **Cap.** Do SQL-created login roles count toward `CHELADB_BRANCH_ROLE_CAP` (10)? A refusal would fail the migration's `CREATE ROLE`. Also note the proxy's `max_roles=10` cache sizing.
8. **SQL-created databases.** Track them in metadata for Studio but keep them out of the spec (recommended), ignore them, or put them in the spec as Neon does?
9. **Group or passwordless roles** (`CREATE ROLE g NOLOGIN`, common in Supabase-style migrations). Recommendation: accept them silently, never in the spec, listed in Studio as "group". Neon refuses empty-password roles.
10. **Studio.** Show SQL roles with a "SQL" badge and allow API reset and delete on them?
11. **Orchestrator down.** During an outage, should DDL fail (Neon's behaviour, consistent) or fail open (`forward_ddl` can't be changed by the owner; fail-open needs the listener fallback from C)?
12. **Small fork task.** Forward `login` / `[NO]LOGIN` (and maybe `VALID UNTIL`) in the DDL payload: yes or no?
