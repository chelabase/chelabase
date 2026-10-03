# SQL editor in Studio (roadmap step 6d): design

**Status:** design approved in conversation 2026-10-03, part by part; written spec approved by the user 2026-10-03.
**Builds on:** step 6c (`docs/superpowers/specs/2026-10-02-data-api-databases-design.md`: the console route, the owner credential path), step 6a (login roles, the protected flag) and step 6b (projects, app users; merged `76acabf`). Where this spec says nothing, those specs still hold.
**Branch:** `sql-editor` (`.worktrees/sql-editor`), from `main` at `cda174e`.

## Goal

The database's owner opens a branch's **SQL** tab in Studio, writes Postgres SQL with schema-aware completion, hover and diagnostics, and runs it unchanged (never translated into the data API's grammar) against any of the branch's Postgres databases, as the owner or as a chosen role. Results, row counts, notices and errors with their position stream back while the run is going; a run can be cancelled. Risky statements need a confirmation, enforced by the server. Each user keeps private saved queries per Cheladb database, and each browser keeps a history of recent runs.

## Decisions (user, 2026-10-03)

| # | Question | Answer |
|---|---|---|
| D1 | Scope | Core runner, **Cancel**, **saved queries**, **autocomplete**, plus a **per-browser run history**. |
| D2 | Driver | The editor runs on **tokio-postgres** (0.7.18): it returns notices, runs multi-statement scripts natively and has a cancel token. sqlx only logs notices. Metadata stays on sqlx. |
| D3 | Data API driver | A **roadmap chore**: benchmark the data API on pipelined tokio-postgres against today's sqlx path; switch rest and `cheladb-query` only if it wins. Not part of 6d. |
| D4 | Execution | **Postgres's own semantics**: the buffer (or selection) is one simple query, an implicit transaction unless the script has its own `BEGIN`/`COMMIT`; lone non-transactional commands (`VACUUM`, `CREATE INDEX CONCURRENTLY`) work. |
| D5 | Limits | **Fixed caps**: 1,000 rows per result set, 10 MiB per response; timeout 60 s by default, up to 15 min per run. Constants, not per-branch settings. |
| D6 | Saved queries | **Private per user, grouped per Cheladb database** (work on every branch of it), stored server-side. (First answered "shared per database", changed to per user during Part 5.) |
| D7 | Editor | **CodeMirror 6** with **postgres-language-server** (pgls). |
| D8 | pgls form | The **WASM build in a web worker** (`@postgres-language-server/wasm` 0.26.0), fed schema JSON by the orchestrator. No server-side language-server process; owner credentials stay in the orchestrator. No `EXPLAIN` type checking (native build only). |
| D9 | Audit | **Metadata and command tags** per run, plus a SHA-256 of the text; **never the SQL text**. |
| D10 | Guards | A **server-side guard** with a `confirm` flag, Studio confirming first. |
| D11 | Guard parser | **pg_query 6.2.1** (libpg_query, Postgres's own parser; the same one pgls uses). Needs clang and protoc at build time. |
| D12 | Guard scope | **Every branch**: risky statements on unprotected branches, any non-read on protected branches. |
| D13 | History | **Per browser**: the last 100 runs per Cheladb database. |
| D14 | Transport | A **streamed NDJSON response** with heartbeats (the gateway's upstream read timeout is 60 s). |

**Rulings (mechanical, listed to the user):**
- **R1:** "Run as a role" sends a session-level `SET ROLE` before the script rather than prepending `SET LOCAL ROLE` to it: a prepended statement would make a lone `VACUUM` multi-statement and fail. Since each run has its own connection (R2), nothing outlives the run.
- **R2:** Each run opens a **fresh connection** and closes it at the end, instead of pooling: no `SET`, temp table, open transaction or role can leak to the next run, and no reset logic is needed. Cost: one connect and SCRAM handshake per run.

Supabase and Neon were references for pitfalls only; nothing here follows their SQL editor APIs.

## Part 1: API and wire format

### Run

`POST /v1/databases/{id}/branches/{branch_id}/sql`. Only the database's owner may call it (6c's console ownership check).

```json
{ "run_id": "<uuid chosen by the client>",
  "database": "app",
  "role": "anon",
  "sql": "…",
  "timeout_ms": 60000,
  "confirm": false }
```

- `database`: a Postgres database of the branch; `null` or absent is the branch's main database.
- `role`: `null` or absent runs as the owner.
- `sql`: 1 byte to 1 MiB.
- `timeout_ms`: 100 to 900,000; default 60,000.
- `confirm`: default false; see the guard (Part 2).

**Errors before streaming starts** are plain JSON in the platform's error shape:

| Status | Code | When |
|---|---|---|
| 422 | the existing validation codes | bad body, bounds, a refused role (system role or superuser) |
| 404 | existing not-found codes | unknown Cheladb database, branch, Postgres database or role |
| 409 | `owner_password_unknown` | the owner's password was set in SQL (6c's text) |
| 409 | `branch_not_ready` | the branch has no endpoint yet |
| 409 | `run_id_in_use` (new) | this user already has a run in flight with this id |
| 409 | `sql_confirm_required` (new) | the guard held statements back; the body lists them (below) |
| 429 | `sql_run_limit` (new) | 4 runs in flight for this user, or 4 for this (branch, database) |
| 503 | the existing unavailable code | the compute isn't running or can't be reached |

`sql_confirm_required`'s body carries `statements: [{ "index": 2, "kind": "DropStmt", "text_preview": "DROP TABLE orders", "reason": "risky" | "protected" }]`, where `kind` is pg_query's statement node name (`unparsed` when the script didn't parse) and `text_preview` is the statement's first 200 characters.

### The stream

Once the connection is ready the answer is `200 application/x-ndjson`: one JSON object per line, in order.

| Event | Shape |
|---|---|
| notice | `{"t":"notice","severity":"NOTICE","code":"00000","message":"…","detail":null,"hint":null,"position":null}` |
| columns | `{"t":"columns","statement":0,"columns":[{"name":"id","type":"int8"}]}` |
| rows | `{"t":"rows","statement":0,"rows":[["1","a",null]]}` (values as text or null, at most 200 rows per event) |
| complete | `{"t":"complete","statement":0,"tag":"SELECT 3","rows":3,"truncated":false}` |
| error | `{"t":"error","statement":2,"code":"42P01","message":"…","detail":null,"hint":null,"position":17}` |
| heartbeat | `{"t":"heartbeat"}` after 15 s without another event |
| done | `{"t":"done","outcome":"ok","duration_ms":12}` |

- `statement` counts statements in the sent text from 0, by completed statements (each `CommandComplete` or `EmptyQueryResponse` advances it).
- `truncated` is true when the statement produced more than 1,000 rows. The statement still runs to its end; the extra rows are dropped.
- `position` is a 1-based character offset into the **sent** text (Postgres's own field). Studio adds the selection's start when a selection ran.
- `type` is the column type's name (from `pg_type`, looked up once per run for the OIDs seen).
- `done` is always the last event. `outcome` is one of `ok`, `error`, `cancelled`, `timeout`, `truncated_response` (the 10 MiB cap was hit and the run was cancelled).

### Cancel

`POST /v1/databases/{id}/branches/{branch_id}/sql/runs/{run_id}/cancel` returns **202** and sends Postgres's cancel request, or **404** when this user has no such run in flight. Closing the stream (the client aborts the fetch) cancels too.

### Gateway

The new routes go into `crates/cheladb-routes` as their own block. The run route streams through the gateway without buffering; the 60 s upstream read timeout stays as it is, because the heartbeat keeps data flowing.

## Part 2: Execution in the orchestrator

**Module:** `services/orchestrator/src/sql_editor/` (routes, the run loop, the cancel registry, the guard, audit).

**Credentials.** `console.rs`'s `owner_pool_for` is split: a new `owner_target(state, branch, row, owner) -> Result<OwnerTarget, ApiError>` (`OwnerTarget { addr, user, password, sealed }`) does the lookup (the sealed owner password, the compute's address) with 6c's errors (`owner_password_unknown`, `branch_not_ready`, `credentials_unavailable`, the 503s). 6c's pool and the editor both call it; 6c's console behaviour and tests are unchanged.

**Connections (R2).** Every run opens a fresh tokio-postgres connection to the compute's published port as the owner, with `application_name = cheladb-sql-editor`, and closes it at the end. TLS follows 6c's pool (`prefer`); the plan's pre-flight checks what the compute offers and picks `tokio-postgres-rustls` (aws-lc-rs) or plain accordingly. In flight at once: at most 4 runs per user and 4 per (branch, Postgres database), otherwise 429 `sql_run_limit`.

**A run, in order:**
1. Validate the body; resolve the database, branch, Postgres database and role (404/422).
2. Run the guard (below); held-back statements give 409 `sql_confirm_required`.
3. Resolve `owner_target`, connect, and register the cancel token under `(user, run_id)` (409 `run_id_in_use` on a duplicate). Errors up to here are JSON; from here on the response is the stream.
4. Send `SET statement_timeout = <timeout_ms>`, then, when a role was chosen, `SET ROLE <quoted identifier>` (R1), each as its own command.
5. Send the SQL as one simple query (`simple_query_raw`). Row descriptions, rows, `CommandComplete` and errors become events. Notices come from the connection's message stream (`AsyncMessage::Notice`), which the run drives itself, and are merged into the event stream as they arrive.
6. A wall-clock deadline of `timeout_ms` sends Postgres's cancel request and ends with `timeout`. `POST …/cancel`, a closed stream and the 10 MiB cap send the same cancel and end with `cancelled` (or `truncated_response`).
7. Unregister, close the connection, emit `done`, write the audit line.

**Roles.** The role must be one of the branch's roles (6a's list) or, when the chosen Postgres database has the data API's machinery, `anon` or `authenticated`. Superusers and system roles (`cloud_admin`, `neon_superuser`, `pg_*` and the other reserved names 6a's checks know) are refused with 422. Owner-level roles are allowed: the connection is already the owner's.

A role choice is a **preview of that role's view, not a security boundary**: the script runs on the owner's connection and can `RESET ROLE`. Studio says so (Part 5).

**Pre-flight check (live):** whether the owner can `SET ROLE` to an API-made role, a SQL-made role, and `anon`/`authenticated`. Postgres 16 and later don't give a `CREATEROLE` creator SET on the roles it creates by default, and roles created by `compute_ctl` or the worker have another creator. Where the owner can't, a worker RPC runs `GRANT <role> TO <owner> WITH INHERIT FALSE, SET TRUE` through `admin_sql` on first use, cached like 6c's `check_role`. With `INHERIT FALSE` the owner gains no privileges implicitly, and the owner is already the most privileged role on the branch.

**Guard (pg_query 6.2.1, every branch).** Unless the body says `confirm: true`, the server splits and parses the script before connecting:
- **Unprotected branch:** *risky* statements are held back: `DROP …` (every `DropStmt` and the other drop nodes: database, role, owned, subscription, tablespace), `TRUNCATE`, `ALTER … DROP …` (column, constraint, attribute, value), `DELETE` without `WHERE`, `UPDATE` without `WHERE`, and anything that doesn't parse (kind `unparsed`; fail closed).
- **Protected branch:** every statement that isn't a **read** is held back. Reads: `SELECT` without `INTO`, without a data-modifying CTE and without `FOR UPDATE`/`FOR SHARE` (and their `NO KEY`/`KEY` forms); `VALUES`; `TABLE`; `SHOW`; `EXPLAIN` without `ANALYZE`. Unparsed counts as a write.
- **Known limit (documented):** a `SELECT` that calls a function which writes passes as a read. A server-side guard can't see into function bodies.
- `confirm: true` skips the guard. The audit line records it.

**Audit.** One `info` line per run when it ends: platform user id, Cheladb database id, branch id, Postgres database, role (or owner), `run_id`, statement count, the command tags (the first 50), duration, outcome, `sql_sha256`, `protected`, `confirmed`. Never the SQL text and never values. A 409 `sql_confirm_required` is logged the same way, with outcome `confirm_required` and the held-back kinds.

## Part 3: Schema for the language server

**Pin.** `@postgres-language-server/wasm` is pinned at **0.26.0** exactly. The orchestrator has `PGLS_VERSION = "0.26.0"`; a test reads Studio's `package.json` and fails when they differ.

**Introspection.** The orchestrator embeds **pgls's own introspection SQL from tag 0.26.0** (`crates/pgls_schema_cache/src/queries/*.sql`, MIT, with an attribution header) and assembles the JSON that the WASM build's `setSchema` takes. Upgrading pgls means re-copying those files at the new tag and updating the fixture test; the module docs describe the steps.

**Route.** `GET /v1/databases/{id}/branches/{branch_id}/sql/schema?database=<name>`, owner only.
- It uses a fresh owner connection like a run (same `owner_target`, same errors), always **as the owner**, so completion covers the whole database. Roles matter only when running.
- `200 application/json` with the pgls schema JSON, at most 8 MiB; beyond that 422 `schema_too_large` (new), and Studio falls back to keyword completion.
- No server-side cache. Studio caches per (branch, Postgres database) with TanStack Query and refetches when a run's command tags include DDL (`CREATE`, `ALTER`, `DROP`, `COMMENT`, `GRANT`, `REVOKE`; not `TRUNCATE`), when the database picker changes, and on Refresh schema.

**Pre-flight checks (plan):**
1. Read `pgls_wasm` at 0.26.0 and pin the exact `setSchema` JSON and the `/workspace` API (`complete`, `lint`, `hover`, `parse`/`splitStatements`, their offset units and result shapes).
2. Run the copied SQL on Postgres 14, 15, 16 and 17 computes (a branch can be any of them); a query that uses a catalog column missing on an older version gets a version-gated variant.
3. Check that the owner has the privileges the SQL needs.

## Part 4: Saved queries

**Migration `20261009000001_saved_queries.sql`** (after 6b's `20261008100001_app_users.sql`):

```sql
CREATE TABLE saved_queries (
    id UUID PRIMARY KEY,
    database_id UUID NOT NULL REFERENCES databases (id) ON DELETE CASCADE,
    user_id TEXT NOT NULL,
    name TEXT NOT NULL CHECK (char_length(name) BETWEEN 1 AND 120),
    sql TEXT NOT NULL CHECK (octet_length(sql) <= 1048576),
    pg_database TEXT,
    role TEXT,
    version INT NOT NULL DEFAULT 1,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE UNIQUE INDEX saved_queries_name ON saved_queries (database_id, user_id, lower(name));
```

- `user_id` is the platform user id (the authenticated principal's).
- `pg_database` and `role` remember the last Postgres database and role used (`null`: main, owner).
- A tombstoned Cheladb database hides its queries; a hard delete cascades.

**Routes.** Anyone who can open the Cheladb database (the existing access check), and every route is filtered by the caller's user id:
- `GET /v1/databases/{id}/sql-queries`: the caller's queries without the SQL text (`id, name, pg_database, role, version, updated_at`), sorted by name.
- `POST …/sql-queries` `{ name, sql, pg_database?, role? }` → 201 with the row.
- `GET …/sql-queries/{query_id}` → the row with its SQL.
- `PATCH …/sql-queries/{query_id}` `{ version, name?, sql?, pg_database?, role? }` → 200 with the new version.
- `DELETE …/sql-queries/{query_id}` → 204.

**Rules and errors (new codes):**
- 409 `saved_query_name_taken`: the caller already has that name (case-insensitive) in this database.
- 409 `saved_query_conflict`: `version` is stale (the same user saved from another tab or computer); the body carries the current row.
- 409 `saved_query_cap`: 500 queries per user per database.
- 404 for an unknown query, another user's query (its existence isn't revealed) or a query of another database.
- `pg_database` and `role` are hints: on a branch where they don't exist, Studio falls back to main and the owner, with a note.
- Saving runs nothing; the guard applies when a query runs.

## Part 5: Studio

**Placement.** A new branch tab, **SQL**, after Tables, routed like the other branch tabs (`routeTree.gen.ts` regenerated, never hand-merged). The editor code and the pgls worker are lazy-loaded; other pages don't grow.

**Toolbar.** Database (the branch's Postgres databases, main first), Run as (Owner by default, then the branch's roles; system roles not listed), Timeout (60 s, 5 min, 15 min), **Run** (Ctrl/Cmd+Enter; the selection if there is one, else the buffer), **Cancel** while running, Refresh schema.

**Banners:**
- Owner: "Running as owner: row-level security is bypassed."
- A role: "Running as `anon` (SET ROLE). This previews that role's view; it isn't a security boundary, since a script can RESET ROLE."
- Protected branch: "Protected branch: every change asks for confirmation."
- `owner_password_unknown`: 6c's notice with the reset link.

**Left panel**, two tabs:
- **Saved:** the caller's queries for this Cheladb database, searchable by name; each row shows the name and when it was last saved; open, rename, delete (with a confirm).
- **History:** this browser's last 100 runs for this Cheladb database (SQL text, branch, Postgres database, role, time, outcome, duration), with Open, Save as query and Clear history.

**Editor and results:**
- The editor on top; below it, one result tab per statement that returned columns, labelled with its tag (`SELECT 3`), and a **Messages** tab with notices, each statement's tag and row count, and the error.
- Grid values are text; `NULL` is shown distinctly. A banner says "Showing the first 1,000 rows" when `truncated`. The status line shows the duration and outcome.
- An error's `position` is underlined in the editor (shifted by the selection's start when a selection ran), with "Go to error".

**Editor component:**
- CodeMirror 6 used directly (no React wrapper): `@codemirror/state`, `view`, `commands`, `language`, `autocomplete`, `lint`, `search` and `lang-sql` with its PostgreSQL dialect (highlighting and the keyword fallback), each at its latest release when the plan is written.
- **pgls worker:** `@postgres-language-server/wasm` 0.26.0 in a module web worker, started when the SQL tab opens; the wasm file is a Vite asset. A thin adapter calls `setSchema` (Part 3) and maps `complete` to completions, `lint` to `@codemirror/lint` diagnostics, `hover` to tooltips, and `parse`/`splitStatements` to the guard hint, converting between UTF-16 offsets and the WASM API's units.
- Until the worker is ready, or when it fails to load, the editor uses lang-sql's keyword completion and shows a small "Basic completion" note.

**Guard flow.** Before sending, Studio applies Part 2's rule to pgls's parse; held-back statements open a confirm dialog listing kind, preview and reason (risky or protected). Confirming sends `confirm: true`. A 409 `sql_confirm_required` from the server opens the same dialog (for when Studio's check missed something or the worker isn't loaded).

**Transport.** The run uses `fetch` with the session cookie and reads the body as a stream: a `TextDecoder` and an NDJSON line reader feed a reducer that builds the result tabs. Non-200 answers go through the existing error handling. Cancel uses the generated client. Leaving the page or starting a new run aborts the fetch, which cancels the run on the server. In OpenAPI the run route is documented as `application/x-ndjson`; Studio doesn't use a generated hook for it.

**Saved-query editing.** Opening a saved query loads it into the buffer and tracks unsaved changes. **Save** sends a `PATCH` with `version`; **Save as** sends a `POST`. On `saved_query_conflict` Studio offers "Overwrite" or "Load saved version". The save dialog says: "Saved queries are private to you but stored on the server. Don't save secrets."

**Browser storage** (per user and Cheladb database): the open buffer, the history (at most 100 entries; SQL text and metadata, never results) and the chosen timeout. Every read and write is wrapped in try/catch, and the page works without storage.

## Part 6: Tests, rollout and the roadmap chore

**Tests (the plan names each one):**
- **Orchestrator, unit:**
  - the guard's rule table per statement kind, on unprotected and protected branches; `unparsed` fails closed; `confirm` skips;
  - body validation bounds;
  - the NDJSON event encoding;
  - the 1,000-row cap and the 10 MiB cap;
  - the cancel registry (`run_id_in_use`, another user's run is 404, the concurrency caps);
  - the audit line never contains the SQL text;
  - `owner_target` (6c's console tests stay green);
  - the saved-query routes, including another user's query answering 404, conflicts, the cap and name uniqueness.
- **Orchestrator, live** (through `just live`; Postgres 17, plus one run of the schema SQL on 14):
  - a multi-statement script with result sets and tags;
  - `RAISE NOTICE` arriving as an event before the run ends;
  - an error with its position and statement index;
  - an explicit `BEGIN … COMMIT` honoured; a lone `VACUUM`;
  - `SET ROLE` as `anon`, an API-made role and a SQL-made role;
  - cancelling `pg_sleep(60)` both through the route and by closing the stream; the deadline ending with `timeout`;
  - a heartbeat carrying a run past 60 s through the gateway;
  - nothing leaking between runs (a temp table, a `SET`);
  - the pgls schema JSON loading.
- **Metadata:** the migration; name uniqueness per user and database; the cascade.
- **Studio (vitest):** the NDJSON reducer, offset mapping, the guard hint, the confirm dialog on 409, history with and without storage, the saved-query conflict flow, the error-position underline.
- **`just sql-e2e`:** create a table, insert and select as the owner, select as `anon` under RLS, cancel a sleep, a refused `DROP` then a confirmed one, a protected branch refusing an `INSERT`, save, reopen and delete a query.
- **One Playwright pass:** completion from pgls, a run, result tabs, the error highlight, Cancel.

**Rollout:**
- Branch `sql-editor`, worktree `.worktrees/sql-editor`.
- **Machine setup:** pg_query needs clang and protoc. `sudo apt-get install -y clang protobuf-compiler` is added to the roadmap's new-computer checklist. Installed on this machine (2026-10-03); the second computer still needs it.
- While the migration exists only on this branch, live runs use 6b's separate-metadata-database exports at the top of the justfile, reverted before the merge, so `main`'s binaries keep working against the shared metadata database.
- Only the coordinator runs `just api-gen` (OpenAPI documents, the management client, Studio's clients), once after each wave.
- README: a "SQL editor" section covering the execution semantics, the limits, the guard and its known limit, and the role-preview caveat.

**Roadmap chore (D3).** A new item, "Data API on tokio-postgres": benchmark today's sqlx path (`BEGIN`, preamble, query, `COMMIT` as four sequential round trips) against a pipelined tokio-postgres one, measuring latency with `tc netem` delay on the compute network and throughput under concurrent load on one machine. rest and `cheladb-query` switch only if it wins. Metadata stays on sqlx.

## Later (not in 6d)

- `EXPLAIN`-based type checking (pgls's native build, or our own `EXPLAIN` pass).
- Shared or team saved queries.
- Exporting results (CSV, JSON).
- Multiple editor tabs.
- **Platform-mode decision:** the owner credentials behind the editor (as for 6c's console).
