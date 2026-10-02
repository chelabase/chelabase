# Data API across databases and schemas (roadmap step 6c): design

**Status:** design approved in conversation 2026-10-02, part by part; this written spec awaits the user's review.
**Builds on:** step 6 (data API engine, `docs/superpowers/specs/2026-10-01-data-api-design.md`) and step 6a (login roles, `docs/superpowers/specs/2026-10-01-login-roles-design.md`). Where this spec says nothing, those specs still hold.

## Goal

A branch's data API serves any of its Postgres databases, not only the main one, and Studio can browse and edit rows in any served database and schema, as the owner or as any plain role.

Schemas already work: `"s.t"` names any schema the role has `USAGE` on (step 6). 6c adds databases, per-database `default_schema`, key allow-lists, and Studio's console.

## Decisions (user, 2026-10-02)

| # | Question | Answer |
|---|---|---|
| Q1 | How a request targets a database | Keys are branch-scoped, with an **optional database allow-list**. |
| Q2 | Which databases are served | **Opt-in per database.** Main is on, as today; others are enabled from Studio or the API. |
| Q3 | Where the allow-list applies | **Both key kinds, editable** (the public key's list too). |
| Q4 | Request shape | A **top-level `"database"` body field** (main when omitted). |
| Q5 | Joining results across databases | **Deferred.** Data that is joined belongs in one database under several schemas; a view or a function joins it today. |
| Q6 | Settings | **`default_schema` per database;** `max_rows`, `statement_timeout_ms` and `max_connections` stay project-wide. |
| Q7 | Studio | A database picker plus a **table data browser**. |
| Q8 | Which role the browser uses | **"View as"**, default the owner. |
| A1 | Owner view | **rest stays authenticator-only.** The orchestrator runs owner queries through a shared engine crate, with the owner password it already holds. |
| A2 | Scope of the browser | **Full CRUD in 6c.** |
| A3 | Studio look | **Functional and minimal.** A full UI revamp comes after the roadmap and the fork. |

## Part 1: enabling databases

- **Metadata.** `branch_databases` gains `data_api boolean NOT NULL DEFAULT false` and `default_schema text NOT NULL DEFAULT 'public'` (same name check as today, `^[a-z_][a-z0-9_]{0,62}$`).
  - The migration sets `data_api = true` on every branch's `main` row, copies the project's `databases.default_schema` onto it, then drops `databases.default_schema`.
  - `max_rows`, `statement_timeout_ms` and `max_connections` stay on `databases`.
- **Enable:** `POST /v1/databases/{id}/branches/{branch_id}/pg-databases/{name}/data-api`.
  - The worker runs the existing data API bootstrap in that Postgres database: the `cheladb` schema, the version table and the event trigger, the default privileges, and (on the first bootstrap only) the grants on existing objects.
  - The default privileges and first grants cover the database's `default_schema`, not only `public`.
  - The flag is set only after the bootstrap succeeds. Until then the row reads `enabling`; a failure leaves `data_api = false` and records the error for Studio.
- **Disable:** `DELETE` on the same path.
  - Stops serving at once: the flag goes off, and the orchestrator tells rest to drop its pool and caches (Part 3).
  - Leaves the `cheladb` schema and the grants in place, so re-enabling is instant.
  - **Remove machinery** (`DELETE …/data-api?remove=true`) also drops the `cheladb` schema and its event trigger. Grants made to `anon` and `authenticated` stay; they belong to the user by then.
  - The main database can't be disabled.
- **Changes from SQL (6a's forwarding and resync):**
  - `CREATE DATABASE` arrives with `data_api = false`.
  - A rename keeps the flag and `default_schema`, and every key allow-list naming the old name is updated (as data keys already follow role renames).
  - A drop removes the row and takes the name out of every allow-list.
  - The system `postgres` database (origin `system`) can't be enabled: it's owned by `cloud_admin`, so the owner couldn't manage its grants.
- **Branches, restore and reset:**
  - A child branch copies its parent's `data_api` flags and `default_schema` values (the catalog, machinery included, is a copy).
  - The bootstrap runs in **every enabled database** wherever it runs today: after every compute (re)creation, and when reconcile finds `cheladb.schema_version` missing. A restore that rewinds a database to before it was enabled gets the machinery reinstalled.
- **Cap:** at most 10 enabled databases per branch, main included, set by `CHELADB_BRANCH_DATA_API_DATABASE_CAP` (default 10, range 1–100). Each one costs rest a pool. Over the cap: 409 with a plain refusal text.

## Part 2: keys and the gateway

- **Metadata:** `data_api_keys.databases text[]`. NULL means every enabled database of the branch; a list means only those. An empty list is refused (422).
- **API:**
  - Creating a secret key takes an optional `databases`.
  - `PATCH /v1/databases/{id}/branches/{branch_id}/keys/{key_id}` with `{ "databases": [...] | null }` works on both kinds.
  - Names must be databases the branch has; a disabled one is allowed (Studio marks it "not served").
  - Rotation copies the list to the new key.
  - The keys list returns `databases` for each key.
- **Enforcement:**
  - The gateway still doesn't parse request bodies. Its key lookup (`GET /internal/data/keys/{sha256}`) also returns `databases`, and the gateway puts the list into the internal token it mints for rest.
  - **rest enforces it,** since it parses the body anyway: the request's `database` (main when omitted) must be on the token's list (when there is one) and enabled.
  - The gateway's key cache stays at 30 s, the same window a revoke has today.
- **Errors:** a database that doesn't exist, isn't enabled, or isn't on the key's list answers **404 `unknown_database`** with one text for all three, so a public key can't discover database names (the same rule that hides table names).
- `GET /data/v1/schema?database=` follows the same rules.

## Part 3: the rest engine

- **Connection info:** `GET /internal/data/branches/{id}/connection` returns its current fields plus `databases: [{ name, default_schema }]`, the enabled databases, main first. rest caches it for 60 s (`INFO_TTL`), as today.
- **Pools:**
  - One sqlx pool per (branch, database), created on first use and closed after 5 min idle.
  - The branch's `max_connections` becomes one semaphore shared by all its database pools.
  - The global `CHELADB_REST_MAX_CONNECTIONS` permits are unchanged.
- **Immediate changes:** a new internal `POST /internal/branches/{id}/invalidate` (service token, orchestrator only) drops the branch's cached info, pools and schema entries. The orchestrator calls it after a disable, a `default_schema` change and a narrowed allow-list. A failed call is logged; the 60 s TTL bounds the delay.
- **Requests:**
  - The tree gains an optional top-level `"database"` next to `"v"`. Inside a `batch` only the top level may name it; a batch runs in one database.
  - `"t"` means `<that database's default_schema>.t`.
- **Caches:** the schema cache and the shape cache are keyed by (branch, database, schema version). Each database has its own `cheladb.schema_version`, so DDL in one database doesn't flush another's cache.
- **Errors:** 404 `unknown_database` (Part 2). An enabled database whose machinery is missing gives today's `unavailable` text, naming the database. Everything else is unchanged.

## Part 4: Studio's console route

- **Platform route:** `POST /v1/databases/{id}/branches/{branch_id}/console/query` with `{ "role": "<name>" | null, "database": …, …the data API tree }`. Every operation is allowed, batches included. `role: null` means the owner. The orchestrator checks the caller owns the database.
- **Shared engine crate:** `crates/cheladb-query` takes rest's grammar, validation, schema introspection, schema cache, visibility rules and SQL generation. It is given a connection; it holds none. rest's behaviour and tests are unchanged; rest calls the crate.
  - The introspection gains each table's **primary-key columns** (needed for CRUD by key; today's cache doesn't have them).
- **Owner, in the orchestrator:**
  - The orchestrator opens the sealed owner password (as it does for reveals) and keeps a small owner pool per (branch, database): at most 2 connections, closed after 60 s idle, connecting directly to the compute's published port.
  - Requests run through `cheladb-query` as the owner, with an owner schema cache keyed like rest's.
  - An owner password reset drops that pool.
  - When the owner's password was set in SQL: 409 `owner_password_unknown` ("reset the owner's password to view as owner").
  - **rest never sees an owner password.**
- **Other roles:** the orchestrator forwards to rest's new internal `POST /internal/console/{branch_id}/query` (service token) with `{ role, database, tree }`. rest uses its existing path, `authenticator` plus `SET ROLE`.
  - The first time a role is picked, the orchestrator runs the worker's `check_role` (the RPC key creation uses): it refuses owner-level roles and grants the role to `authenticator`.
- **Behaviour:** one transaction per request; the data API's `max_rows` and statement timeout.
- **Errors (precise, since the caller is the owner):** 409 `data_api_disabled` for a database that isn't enabled (the console needs the machinery's schema version; Studio offers Enable); 404 for a database that doesn't exist; otherwise the data API's codes.
- **Audit:** every console write is logged at `info` with the platform user id, branch, database, role and operation kind, never the values.
- **Groundwork for 6d:** the owner pool is shaped so the SQL editor (roadmap step 6d) can reuse it.

## Part 5: Studio (functional and minimal; A3)

- **Branch tabs:** Overview, Roles, Data API, and a new Tables tab.
- **Data API tab:**
  - A database picker with each database's state (Served / Not served / Enabling… / Error); Enable for the others; Disable and, in a confirm panel, Remove machinery for served ones other than main. `postgres (System)` can't be enabled, with the reason shown.
  - Per database: the `default_schema` setting and the RLS warning.
  - Keys: allow-lists as chips or "All served databases"; Edit databases (multi-select, disabled ones marked "not served") for both kinds; the secret-key form gains the selector.
- **Tables tab:**
  - Toolbar: database (served ones), schema (those the chosen role can reach), View as (Owner by default, `anon`, `authenticated`, plain login roles; owner-level roles listed but disabled with the reason).
  - `owner_password_unknown` shows a notice with a link to reset the owner password.
  - An owner banner: "Viewing as owner: row-level security is bypassed, and edits apply to every row."
  - A list of tables, views and functions, with an RLS badge on tables.
  - A grid: 50 rows a page (capped by `max_rows`), sort by header, a filter builder with the data API's operators, `null` shown distinctly.
  - CRUD: edit a cell (an `update` keyed by the primary key), Insert row (columns with defaults marked "default"), delete selected rows with a count confirm. Tables without a primary key and views are read-only, with the reason shown.
  - Inputs by column type: text, number, boolean, JSON (validated), date and time (ISO text), enum select, a NULL toggle on nullable columns; other types get JSON or text input.
  - Error codes get plain text (for example, a 403 from a grant: "`anon` can't update this table").

## Part 6: SDK, CLI, tests

- **SDK (`@cheladb/client`):** `db.database("analytics")` returns a client bound to that database; every builder sends the top-level `database`. The default client uses main. `Database` types are grouped by database, then schema; `createClient<Database["analytics"]>` types a bound client.
- **CLI:** `cheladb gen types --database <name>` or `--all-databases` writes one `Database` type with a key per reachable database.
- **Management client:** regenerated (enable/disable, `default_schema`, allow-list PATCH, console query).
- **Tests (the plan names each):**
  - metadata: the migration backfill; flags and allow-lists following renames and drops;
  - worker: enable, disable, remove machinery; bootstrap in every enabled database; child branches copying flags;
  - gateway: the allow-list in the minted token;
  - rest: `unknown_database` in all three cases; the shared semaphore; per-database caches; invalidation;
  - engine crate: rest's engine tests move with it unchanged; primary keys in the introspection;
  - orchestrator: the owner console (including `owner_password_unknown`, the pool dropped on reset, the audit line); non-owner roles forwarded to rest; `check_role` on first use;
  - live: `rest_live` with two databases; a new `just console-e2e` (enable a second database, a key allowed only on it, CRUD as the owner and as `anon` under RLS, disable);
  - Studio: the picker, allow-list editing and the grid's CRUD flows (vitest), plus one Playwright pass.

## Later (not in 6c)

- A multi-database request with an SDK join (Q5).
- Embeds and nested reads in the data API.
- The SQL editor (roadmap step 6d), on the owner pool.
- The Studio UI revamp (after the roadmap and the fork).
- **Platform-mode decision:** owner credentials for the console (short-lived credentials, or the console in its own service).
