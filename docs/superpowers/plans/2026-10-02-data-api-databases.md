# Data API across databases and schemas: Implementation Plan (roadmap step 6c)

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:subagent-driven-development. Strict TDD (a RED run before code). Work only in your task's worktree, with absolute paths. Read `CLAUDE.md`.

**Goal:** a branch's data API serves any enabled Postgres database of the branch, keys carry an optional database allow-list, and Studio browses and edits rows in any served database and schema, as the owner or as a plain role.

**Architecture:**
- `branch_databases` carries each database's data API state and `default_schema`; the worker bootstraps a database when it's enabled, and in every enabled database wherever it bootstraps today.
- rest's engine moves into `crates/cheladb-query`. rest keeps `authenticator` only, gains a pool per (branch, database) under one per-branch semaphore, and enforces allow-lists carried in the gateway's internal token.
- The orchestrator runs **owner** console queries itself through `cheladb-query`, with the owner password it already holds; other roles go to rest's internal console route.

**Tech stack:** as step 6/6a (Rust 2024, axum 0.8.9, sqlx 0.9.0, moka, utoipa 6.0.0, orval 8.38.0, React 19 + TanStack Query/Router, vitest). **No new dependencies.**

**Spec:** `docs/superpowers/specs/2026-10-02-data-api-databases-design.md` (approved 2026-10-02). The spec wins on behaviour; this plan wins on names, interfaces and the rulings below.

## Status

- **Current (2026-10-02): plan approved by the user. T2 merged. The 6a follow-up chore merged into `main` and `main` was merged in (`58302f5`; workspace 969/969, generated files current). All tasks done (T1–T10, T4b). `main` (with the per-worktree stacks) merged in at `814549f`; this worktree runs in stack slot 2. **MERGED into `main` (`5089819`) at the user's word on 2026-10-03; `main` has 1096/1096 tests passing after the merge.** **Playwright pass (2026-10-03, slot 2, after the user installed the browser's system libraries): all 12 checks passed** (Data API view states, enable, default schema with the grant checkbox off by default, system database refused, key allow-lists, Tables with database/schema switch, owner banner, anon/authenticated under RLS, insert/edit/delete, no-PK read-only, disable). Deferred UX minors for the Studio revamp: the database picker needs a reload to show a database created in SQL; a not-served database shows a default-schema field with a hint for a checkbox it doesn't show; an insert refused by a WITH CHECK policy reads like a missing privilege; Enter doesn't save a cell edit; Insert starts nullable columns as NULL with their inputs disabled; unticking "All served databases" preselects every database. A throwaway Zitadel user `pw-6c@example.com` was left in the dev Zitadel. Branch `data-api-databases` (`.worktrees/data-api-databases`), from `main` `9f99d86`. `main` is merged into it (no rebase: the branch is pushed and force-pushes are off) after the 6a follow-up chore merges (that chore touches the same metadata, worker and orchestrator files).
- **Pre-flight scan (2026-10-02, static, sonnet), corrections to the spec's premises:**
  - Studio has **no branch page or tabs**. The dashboard expands a database row into "Branches" or "Data API" (`dashboard.tsx` `open.view`), and the Data API view picks a branch with a `<select>`.
  - A child branch's `branch_databases` gets only its `main` row at creation; other rows come from the first resync (`set_databases`).
  - Reconcile's schema health check and `check_data_role` look only at the main database.
  - The orchestrator's public `AppState` has no `ComputeLocator` (only `InternalState`), so the owner console needs one.
  - The schema cache has no primary keys (`INTROSPECT_SQL`, `Table`, `TableDesc`).
  - The preamble reads `cheladb.schema_version`, which today only `authenticator` may read.
  - No Playwright tooling exists in the repo; 6a's pass ran through the Playwright MCP over CDP.
  - The latest migration is `20261005000001_role_followups.sql` (on the chore branch).
- **Rulings (made while planning; each with its cost if wrong):**
  - **P1. Studio placement:** no new branch page. The Data API view (already per branch) gains the database picker, states, `default_schema` and allow-lists; the dashboard's `open.view` gains `"tables"` (a third row button "Tables") for the console, with its own branch `<select>`. Fits A3 (functional, minimal; revamp later). *Cost: none; the revamp replaces it.*
  - **P2. State column instead of a boolean:** `branch_databases.data_api text NOT NULL DEFAULT 'off' CHECK IN ('off','enabling','on','error')` plus `data_api_error text`. The spec's `data_api = true` reads `data_api = 'on'`. *Cost: none.*
  - **P3. Child branches copy flags after their first resync:** `MetadataClient::copy_parent_database_flags(child: BranchId)` copies `data_api` and `default_schema` from the parent's rows with the same name (`on` stays `on`; `enabling`/`error` become `off`), called by the worker right after `sync_child_roles`' resync and before the child's data API bootstrap. *Cost: one statement.*
  - **P4. Owner pool keyed by a fingerprint** (SHA-256 of the sealed owner password plus the compute's `host:port`) instead of reset hooks: a reset, a SQL `ALTER ROLE … PASSWORD` (which NULLs the sealed value) or a moved compute all change the key, and the old pool idles out. *Cost: none; covers more paths than the spec's "reset drops the pool".*
  - **P5. The owner may read the version table:** the bootstrap also runs `GRANT USAGE ON SCHEMA cheladb TO <owner>` and `GRANT SELECT ON cheladb.schema_version TO <owner>`. The owner's preamble skips `set_config('role', …)` and claims. *Cost: two grants.*
  - **P6. `gen types --all-databases`:** `GET /data/v1/schema` answers a new top-level `databases: [names]` (the enabled databases the key may reach); the CLI calls the schema endpoint once per name. *Cost: one field.*
  - **P7. Remove machinery:** `DELETE …/data-api?remove=true` runs `DROP EVENT TRIGGER IF EXISTS` for the three triggers and `DROP SCHEMA IF EXISTS cheladb CASCADE` through `admin_sql`, then sets `off`. The `cheladb_admin` role stays (cluster-wide). *Cost: none.*
  - **P8. Allow-list names:** each name 1–63 bytes, at most 20 names, no duplicates; must be a row of the branch's `branch_databases` (any state) at create/PATCH time; origin `system` refused. 422 `invalid_request` otherwise. *Cost: none.*
  - **P9. Enable/disable are worker jobs over NATS** like reset (`JobPort::enable_data_api(branch, name)`, `disable_data_api(branch, name, remove)`), answering a reply with `ok`, `refused(text)`, `not_ready` or `failed(text)`. The HTTP route answers 202 after setting `enabling` and publishing; Studio polls the databases list. *Cost: none.*
  - **P10. `default_schema` change** re-runs the enable bootstrap for that database (so default privileges cover the new schema), then invalidates rest. *Cost: one bootstrap per change.*
  - **P11. Console errors:** 409 `data_api_disabled`, 409 `owner_password_unknown`, 404 `not_found` for an unknown database; new `ErrorCode` variants `DataApiDisabled`, `OwnerPasswordUnknown`; rest's `unknown_database` is a new data API error code (404).
  - **P12. Console page size** is 50 and the console uses the data API grammar unchanged; primary-key columns come from the schema DTO's new `primary_key: Vec<String>` on each table.
  - **P14. Dropping a key's last allowed database revokes the key** (the list can't be empty, and NULL would mean "every database"). *Cost: the user creates a new key after re-creating the database.*
  - **P13. Migration** `20261006000001_data_api_databases.sql` (sorts after the chore's). Merging `main` in after the chore keeps the order.
- **Done:**
  - T1: `e2ff925` (review clean, sonnet), merged. Implementer rulings: main rows are inserted `on` at creation (a child's main takes its parent's `default_schema`); `finish_enable_data_api` only moves a row still `enabling` (a disable mid-enable wins); re-enabling an `enabling`/`error` row starts again. *Cost: none.*
  - Coordinator `9bdce90`: NATS subjects `cheladb.rpc.data_api.{enable,disable,rebootstrap}` and `DataApiDatabaseRequest` (enable is a publish with no reply; disable and rebootstrap answer `CheckDataRoleReply`).
  - **User's answer U1 (2026-10-02):** when a database's `default_schema` changes, existing objects in the new schema are granted **only if the user ticks "Also grant existing objects in <schema>"** (off by default). This adds **Task 4b**.
  - T3 `ee42a61`, T4 `6cf26e2`+`7159358` (one fix round: a missing `default_schema` no longer fails the bootstrap; it skips the schema steps with a warning — ruling X4, *cost: unqualified names in that database don't resolve until the schema exists*), T5 `899349a`, T7 `10d70d1`: all reviewed clean and merged.
  - **User's answer U2 (2026-10-02):** the opt-in grant on a schema change adds grants only; it never revokes (first enable still revokes `PUBLIC` EXECUTE on the schema's functions).
  - T6 `da1bbf6` (review clean, opus) and T4b `18c199f`..`59166b4` (one fix round for U2; it also added `?database=` to the platform schema route, a plan gap T9 found) merged.
  - T8 `8f6382d`..`dd8bd83` (review clean) and T9 `2a1c28c`..`ad3470e` (one fix round: console reads are lossless — int8/numeric beyond the double's range stay strings — and inputs are validated; every write is keyed by the full primary key, with guards) merged.
  - T10 `451a7a8`..`c637a5d` (live, slot 2): `rest_live` 28/28 (four new: two databases with their own schemas, allow-lists incl. rename/drop/revoke, disable answers 404 at once, child branch serves the parent's second database), the owner-pool tests 2/2, `roles_live` 30/30, `live_db` 103/103, the new `just console-e2e` (owner CRUD in two databases, anon under RLS, plain and owner-level roles, `owner_password_unknown` then reset, disable/remove), `data-e2e`, `neon-e2e`, `role-e2e`, `branch-e2e`: all passed. The proxy CA is one shared volume for every slot.
  - **End-of-step review (two opus reviewers, backend and frontend):** With fixes; 7 Important (owner pool never reset session state; enabling didn't reach rest for up to 60 s; remove machinery could CASCADE-drop a user's schema named `cheladb`; per-database pools could exceed `max_connections`; View as lacked anon/authenticated; the Tables schema list followed anon/authenticated even as owner; `gen types --all-databases` duplicated main). **User's answers U3** (fix the schema list now: a role-aware `GET …/console/schema?database=&role=`) and **U4** (main keeps the generated key "main").
  - **Fix batch:** backend `bdc85be`..`75d4cbb` (opus) and Studio/SDK/CLI/docs `c1f89d9`..`df9a88c` + `22342ee` (sonnet), merged at `60c0d60`/`22342ee`; both scoped re-reviews clean. Rulings: **X8.** New migration `20261007000001` adds `branch_databases.data_api_bootstrapped`, so remove is refused (409) on a database never enabled. *Cost: a database disabled before the migration can't be removed until enabled once.* **X9.** rest's invalidate resizes the branch semaphore in place. *Cost: none.* **X10.** The connection bound closes idle connections of the least-recently-used other pool (best-effort under concurrency, +1 transient). *Cost: a brief overshoot.*
  - **Final verification (slot 2, 60c0d60; then 22342ee Studio-only):** `rest_live` 29/29, owner tests 3/3, connection-bound live 1/1, `live_db` 104/104, `roles_live` 30/30, `console-e2e`, `data-e2e`, `neon-e2e`, `role-e2e`, `branch-e2e`; nextest 1096/1096 (one first-run timing flake in `worker provision::tests::waits_for_readiness`, passing on rerun), clippy clean, `just check` (Studio 366 tests).
  - **Deferred minors kept (triaged by the reviewers):** see the ledger triage; the notable ones: nested jsonb big numbers become strings on edit (revamp); an exact-50 table shows an empty next page; rest's 503 for up to 30 s after a role is dropped; `role_refused` wording also covers a missing role; the semaphore shrink-then-grow can leave fewer permits.
  - **Ruling X11 (2026-10-03; the user asked me to decide):** a non-main database's own owner role that isn't owner-level stays selectable in View as and usable for secret keys; it bypasses RLS only on tables it owns, and the console says when RLS is bypassed. *Cost: an app could use such a role and see all rows of its own tables.*
  - **Rulings during execution:** X7 (superseded by U3). The Tables schema picker isn't filtered by the chosen role (the platform schema route reports what anon/authenticated reach). *Cost: an "unknown table" text when the role can't see a table.* X5. A database whose first enable ran while its schema was missing gets its full first grant (with the revoke) once the schema exists, flag or not ("first enable completing late"). *Cost: one revoke the user didn't tick for.* X6. An unchanged schema with `grant_existing` on a database that isn't served answers 409. *Cost: none.* X1. rest's `ConnInfo` accepts both connection JSON shapes, so every merge point works. *Cost: one compat branch.* X2. Reconcile re-runs enables left `enabling` over 60 s (a lost publish heals). *Cost: one reconcile action.* X3. T1 review minors 2 and 3 (a resync drop leaves stale names in key lists; a row turning `main` stays `off`) are fixed in T4. *Cost: none.*
  - T2: `143911e` (pure move) and `6659804` (features) (review clean, sonnet), merged. `TableDesc.primary_key` is omitted from JSON when empty (TS `primary_key?: string[]`; T9 uses `?? []`).
- **Deferred minors:**
  - T2: `schema_for`/`run`/rest's `schemas_for` repeat the preamble + key + cache sequence (extract a helper when T3/T6 touch it); `role.to_owned()` per request; the `message.contains("cheladb")` match for `schema_missing` is loose (prefer SQLSTATE + "schema_version"); `shape_cache_keys_apart_databases` doesn't go through `run`; a mis-sorted import in `tests/schema.rs`; the `arbitrary_precision` comment in `crates/cheladb-query/Cargo.toml` (T6 re-checks `untagged`/`flatten` with `Value` in the orchestrator).
  - **For T10:** cover `RunAs::Owner` (with P5's grants) and a two-database `run` live.

## Global Constraints

- Never mirror Supabase (no PostgREST URL grammar, no postgres-meta raw SQL path). rest holds only the `authenticator` credential; **no owner password ever reaches rest**.
- Errors use `cheladb_types::api_error::{ApiError, ErrorCode}` on the platform API and rest's `ApiError` on the data API; a database that doesn't exist, isn't enabled or isn't on the key's list answers 404 `unknown_database` with one text: "the database doesn't exist or isn't served by this key".
- `CHELADB_BRANCH_DATA_API_DATABASE_CAP` default 10, range 1–100, main included; over the cap 409 `conflict` "this branch already serves <n> databases (the cap is <n>)".
- `default_schema` check `^[a-z_][a-z0-9_]{0,62}$`.
- Every new platform route goes in `crates/cheladb-routes` `ENDPOINTS` and in the orchestrator's utoipa routes (`document_matches_the_registry`); run `just api-gen` and commit the regenerated files.
- Every new port method gets its `Doc` stub (`openapi.rs`) and test fakes (`tests/api.rs`, `tests/internal.rs`, worker `fakes.rs`).
- Never run `cargo fmt` on the workspace (rustfmt only new files). Never edit an applied migration.
- Commits: `git commit`, ending with the `Co-Authored-By` and `Claude-Session` trailers. Implementers don't push or merge.
- Live tests and e2e scripts run one at a time (one Compose stack); the fork session may ask for a 45–60 min slot.

## Review Focus

1. A batch that names `database` inside an inner op must be refused (only the top level may name it); a batch must never span databases.
2. A key whose allow-list names a database that was later renamed or dropped: rename follows, drop removes the name; a list left empty by drops makes the key reach nothing (404), not everything.
3. A console request as the owner while the owner password is `set_in_sql`, then after a reset: 409 first, then success without restarting anything (P4).
4. Disabling a database while rest holds a warm pool and cached shapes for it: the next request is 404 at once (invalidate), and within 60 s even if the invalidate call failed.
5. A child branch of a parent with a served second database: the child serves it after creation (P3), and the bootstrap ran there.

Each item has its test in the owning task (names below).

## Models and schedule

| Task | What | Implementer | Review |
|---|---|---|---|
| T1 | Metadata: migration, state, allow-lists, connection rows | sonnet | sonnet |
| T2 | `crates/cheladb-query`: engine extraction, top-level `database`, primary keys | opus | sonnet |
| T3 | rest: per-database pools, allow-list enforcement, invalidate, console route, schema `databases`; auth token + gateway | opus | sonnet |
| T4 | Worker: enable/disable/remove jobs, per-database bootstrap and health, child flag copy | sonnet | sonnet |
| T5 | Orchestrator management: enable/disable/default_schema routes, allow-lists, connection/lookup routes, invalidate calls, cap | sonnet | sonnet |
| T6 | Orchestrator console: owner pool + `cheladb-query`, forwarding to rest, `check_role`, audit | opus | opus |
| T7 | SDK `db.database()`, per-database types, CLI `--database`/`--all-databases` | sonnet | haiku |
| T8 | Studio Data API view: picker, states, default_schema, allow-lists | sonnet | haiku |
| T9 | Studio Tables console: grid, filters, sort, view-as, CRUD | sonnet | sonnet |
| T10 | Live: `rest_live` two-database tests, `just console-e2e`, README | opus | none (end-of-step review) |
| — | Playwright MCP pass | sonnet | — |
| — | End-of-step whole-branch review | — | opus |

- **Wave 1:** T1 ∥ T2 (disjoint: metadata vs rest/new crate).
- **Wave 2 (after wave 1):** T3 ∥ T4 ∥ T5 (rest+auth+gateway / worker / orchestrator management).
- **Wave 3 (after T3 and T5):** T6 ∥ T7.
- **Wave 4 (after T6; needs the regenerated client):** T8 ∥ T9 only if T9 doesn't touch `data-api-panel.tsx`/`data-api-keys.tsx` and T8 doesn't touch `dashboard.tsx`'s row buttons beyond its own view; T8 owns `dashboard.tsx` edits, T9 adds its own `tables-view.tsx` and gets one line in `dashboard.tsx` after T8 merges.
- **Wave 5:** T10 alone on the Compose stack, then the Playwright pass, the verification run, the end-of-step review and its fixes, and the user's merge pause.

## Task 1: metadata

**Files:** `crates/cheladb-metadata/migrations/20261006000001_data_api_databases.sql` (new), `crates/cheladb-metadata/src/{lib.rs,neon.rs,ddl.rs,data_api.rs,branches.rs}`, `crates/cheladb-metadata/tests/live_db.rs`; minimal compile fixes at call sites in `services/{orchestrator,worker}` (no behaviour change there).

**Migration:**
```sql
ALTER TABLE branch_databases
    ADD COLUMN data_api text NOT NULL DEFAULT 'off' CHECK (data_api IN ('off','enabling','on','error')),
    ADD COLUMN data_api_error text,
    ADD COLUMN default_schema text NOT NULL DEFAULT 'public' CHECK (default_schema ~ '^[a-z_][a-z0-9_]{0,62}$'),
    ADD CONSTRAINT branch_databases_system_off CHECK (origin <> 'system' OR data_api = 'off');
UPDATE branch_databases bd SET data_api = 'on', default_schema = d.default_schema
  FROM branches b JOIN databases d ON d.id = b.database_id
 WHERE bd.branch_id = b.id AND bd.origin = 'main';
ALTER TABLE databases DROP COLUMN default_schema;
ALTER TABLE data_api_keys ADD COLUMN databases text[]
    CHECK (databases IS NULL OR (cardinality(databases) BETWEEN 1 AND 20));
```

**Interfaces (produces):**
- `enum DataApiState { Off, Enabling, On, Error }` (serde snake_case, `utoipa::ToSchema`); `BranchDatabase` gains `data_api: DataApiState`, `data_api_error: Option<String>`, `default_schema: String`.
- `MetadataClient::begin_enable_data_api(branch, name, cap: u32) -> Result<EnableStart, MetadataError>` where `enum EnableStart { Started, AlreadyOn, Unknown, System, OverCap(u32) }` (locks the branch row; counts `on`+`enabling`).
- `finish_enable_data_api(branch, name, Result<(), String>)` → `on`, or `error` with the text.
- `set_data_api_off(branch, name) -> Result<bool, MetadataError>` (false for main/unknown).
- `set_database_default_schema(branch, name, schema) -> Result<bool, MetadataError>`.
- `served_databases(branch) -> Vec<ServedDatabase { name, default_schema }>` (state `on`, main first).
- `copy_parent_database_flags(child) -> Result<(), MetadataError>` (P3).
- `BranchConnectionRow` gains `databases: Vec<ServedDatabase>`; `DataApiSettings` loses `default_schema`; `DatabaseRecord`/`SettingsPatch` lose `default_schema`.
- `NewDataKey.databases: Option<Vec<String>>`, `DataKeyRecord.databases`, `DataKeyLookup.databases`; `set_key_databases(branch, key_id, Option<Vec<String>>) -> Result<bool, MetadataError>`; `rotate_data_key` copies the old key's list.
- `DdlTx::rename_database` replaces the old name in every key list of the branch (`array_replace`). `DdlTx::remove_database` removes it (`array_remove`); a key whose list would become empty is **revoked** instead (P14).

**Tests (live_db):** `migration_backfills_main_as_on` (main rows `on` with the project's schema); `enable_respects_the_cap_and_refuses_system`; `enable_finish_records_errors`; `served_databases_lists_on_only_main_first`; `default_schema_check`; `child_copies_parent_flags` (on→on, enabling→off); `key_lists_follow_renames`; `dropping_the_last_allowed_database_revokes_the_key`; `rotation_keeps_the_list`; `lookup_returns_the_list`; `branch_connection_lists_served_databases`.

**Verify:** metadata tests + `live_db`, `cargo check --workspace`, clippy.

## Task 2: `crates/cheladb-query`

**Files:** new `crates/cheladb-query/{Cargo.toml,src/lib.rs,…}` (moved from `services/rest/src/{query.rs,shape.rs,sql.rs,schema.rs}` and the sqlx half of `error.rs`), root `Cargo.toml` (member + workspace dependency), `services/rest/{Cargo.toml,src/*}` (use the crate), moved tests (`tests/{sql,parse,schema,shape,fixtures}.rs`, `tests/common/mod.rs`), `crates/cheladb-types/src/data_api_schema.rs` (`TableDesc.primary_key`).

**Interfaces (produces):**
- Re-exports `Op`, `parse(body) -> Result<Parsed, QueryError>` where `Parsed { database: Option<String>, op: Op }` (top-level `"database"` string, 1–63 bytes; inside a batch op → `QueryError` `invalid_query` "only the top level may name a database").
- `Schema`, `RoleView`, `introspect`, `visibility`, `role_view`, `plan`, `shape_hash`, `SchemaKey { branch: Uuid, database: String, version: String }`.
- `pub struct QueryCache` (schemas keyed by `SchemaKey`, shapes keyed `(SchemaKey, role, default_schema, shape_hash)`), `QueryCache::invalidate_branch(branch)`.
- `pub enum RunAs<'a> { Role { role: &'a str, claims: &'a serde_json::Value }, Owner }`.
- `pub async fn run(conn: &mut PgConnection, cache: &QueryCache, key: (Uuid, &str), run_as: RunAs<'_>, settings: &RunSettings, budget: usize, op: &Op, rid: &str) -> Result<String, QueryFailure>` (inside a transaction the caller began; preamble per `RunAs`, P5), and `pub async fn schema_for(conn, cache, key, run_as, settings: &RunSettings, rid) -> Result<(Arc<Schema>, Arc<RoleView>), QueryFailure>` (as built in T2: the preamble needs the timeout, and `Schema::to_json` needs the `Schema`).
- `RunSettings { max_rows: i64, statement_timeout_ms: i64, default_schema: String }`; `QueryFailure` maps to a data API error (code, status, message, sqlstate) without axum; rest wraps it with `IntoResponse`.
- Primary keys: `INTROSPECT_SQL` relations gain the PK column names; `Table.primary_key: Vec<String>`; `TableDesc.primary_key` (serde default empty).

**Behaviour:** rest's behaviour and every moved test unchanged (same names, same assertions). rest's `Engine::query` becomes: acquire, begin, owner-level check, `cheladb_query::run`, commit.

**Tests:** all moved tests green under the crate; new `parse_reads_a_top_level_database`, `a_database_inside_a_batch_op_is_refused`, `introspection_reports_primary_keys` (fixture), `shape_cache_keys_apart_databases`, `owner_preamble_sets_no_role` (SQL text pinned).

**Verify:** `cargo nextest run -p cheladb-query -p rest`, clippy, `cargo check --workspace`.

## Task 3: rest, auth token, gateway

**Files:** `services/rest/src/{conn.rs,exec.rs,app.rs,error.rs}`, `services/rest/tests/app.rs`, `crates/cheladb-auth/src/internal.rs` (+ its tests), `services/gateway/src/{data_keys.rs,pipeline.rs}`, struct-literal sites listed by the scan.

**Interfaces:**
- Consumes T2's crate; the orchestrator's connection JSON gains `databases: [{name, default_schema}]` and loses `settings.default_schema` (T5 produces it; pin with this shape).
- `DataIdentity.databases: Option<Vec<String>>` (`#[serde(default, skip_serializing_if = "Option::is_none")]`); `ResolvedKey.databases`; the gateway copies it into the minted identity.
- rest internal routes (service bearer, one shared helper): `POST /internal/branches/{id}/invalidate` → 204; `POST /internal/console/{branch_id}/query` with `{ "role": string, "database": string|null, "tree": object }` → the data API's response/error bodies (role must not be owner-level: same `check_owner_level` as secret keys).
- `GET /data/v1/schema?database=` and the response's new `databases: [names]` (P6).
- Error `unknown_database` (404) with the Global Constraints text.

**Behaviour:** pools keyed `(branch, database)`; `slots` become one semaphore per branch (moka cache keyed by branch, sized by `max_connections`); allow-list check = name on the token's list (when present) and in `ConnInfo.databases`; invalidate drops infos, pools, semaphores and `QueryCache::invalidate_branch`.

**Tests:** `unknown_database_in_all_three_cases_has_one_text`; `a_batch_cannot_name_a_database_inside` (Review Focus 1, through the HTTP edge); `pools_are_per_database_and_share_the_branch_slots`; `invalidate_drops_pools_and_caches_at_once` (Review Focus 4); `invalidate_needs_the_bearer`; `console_route_runs_as_the_role_and_refuses_owner_level`; `schema_lists_reachable_databases`; auth `old_tokens_without_databases_parse`; gateway `the_allow_list_is_carried_in_the_minted_token`.

**Verify:** `cargo nextest run -p rest -p gateway -p cheladb-auth`, clippy.

## Task 4: worker

**Files:** `services/worker/src/{data_api.rs,reconcile.rs,branch_create.rs,branch_switch.rs,provision.rs,ports.rs,fakes.rs,main.rs}`, `crates/cheladb-nats` (job subjects, if jobs are declared there; follow `reset_password`'s pattern).

**Interfaces:**
- `data_api_sql(owner_role, authenticator_password, schema)` (default privileges and first grants in `quote_ident(schema)`; plus P5's two grants). The pinned test `data_api_sql_is_exact` is updated.
- Jobs: `enable_data_api { branch_id, name }`, `disable_data_api { branch_id, name, remove }`, replies `DataApiReply { ok, refused: Option<String>, not_ready: bool, failed: Option<String> }` (P9). Enable runs `bootstrap_data_api_on(branch, ep, name, schema)` then `finish_enable_data_api`. Remove runs P7's SQL.
- Bootstrap and health in every served database: `recreate_compute`, provision, branch create and `reconcile_data_api` loop `served_databases`; `data_api_healthy` checks each; `Action::BootstrapDataApi` carries `(branch_id, database)`.
- Branch create: `copy_parent_database_flags(child)` after the first resync, before the bootstrap loop (P3).
- `WorkerMetadata` gains `served_databases`, `begin_enable_data_api`/`finish_enable_data_api`/`set_data_api_off`, `copy_parent_database_flags` (+ fakes).

**Tests:** `enable_bootstraps_that_database_with_its_schema`; `enable_failure_records_error`; `disable_with_remove_drops_the_machinery`; `recreate_bootstraps_every_served_database`; `reconcile_checks_and_heals_each_served_database`; `child_create_copies_flags_then_bootstraps` (Review Focus 5); `bootstrap_grants_the_owner_the_version_table`.

**Verify:** `cargo nextest run -p worker`, clippy.

## Task 5: orchestrator management

**Files:** `services/orchestrator/src/{data_api_databases.rs (new),data_keys.rs,branches.rs,lib.rs,main.rs,openapi.rs,internal/data.rs,internal/ddl.rs}`, `crates/cheladb-routes/src/lib.rs`, `crates/cheladb-types/src/api_error.rs` (P11 codes), `services/orchestrator/tests/{api.rs,internal.rs}`, `openapi/*.json` and the generated client (`just api-gen`).

**Routes (all `Auth::Management`, Studio CORS):**
- `GET /v1/databases/{id}/branches/{branch_id}/pg-databases` gains `data_api`, `data_api_error`, `default_schema` (existing `list_branch_databases`).
- `POST …/pg-databases/{name}/data-api` → 202 (P9); 409 over cap / not ready; 422 for `system`; 404 unknown.
- `DELETE …/pg-databases/{name}/data-api[?remove=true]` → 204 after the job; 409 for main.
- `PATCH …/pg-databases/{name}` `{ "default_schema": "…" }` → 200 (P10).
- `PATCH …/keys/{key_id}` `{ "databases": [...] | null }` → 200 with the key; create takes `databases`; list returns it.
- `PATCH /v1/databases/{id}` loses `default_schema` (breaking; Studio follows in T8).
- Internal: `connection` returns `databases`; `key_lookup` returns `databases`.

**Behaviour:** after disable, a `default_schema` change, a narrowed allow-list, and a forwarded `DROP DATABASE`/rename, call rest's invalidate (`RestInternal::invalidate(branch)`, logged on failure). `CHELADB_BRANCH_DATA_API_DATABASE_CAP` read in `main.rs` (default 10, 1–100).

**Tests:** `enable_answers_202_and_publishes`; `enable_refusals` (system 422, over cap 409, unknown 404); `disable_main_is_refused`; `default_schema_patch_validates_and_invalidates`; `key_allow_list_validation` (P8); `patch_key_databases_invalidates`; `connection_lists_served_databases`; `key_lookup_returns_databases`; `ddl_drop_invalidates_rest`; `update_database_no_longer_takes_default_schema`.

**Verify:** `cargo nextest run -p orchestrator`, clippy, `just api-gen` committed, `just check` (Studio typecheck may break until T8; report).

## Task 6: orchestrator console

**Files:** `services/orchestrator/src/{console.rs (new),lib.rs,main.rs,openapi.rs}`, `crates/cheladb-routes/src/lib.rs`, `services/orchestrator/{Cargo.toml (cheladb-query, sqlx),tests/api.rs}`, regenerated client.

**Interfaces:**
- Route `POST /v1/databases/{id}/branches/{branch_id}/console/query`, body `ConsoleRequest { role: Option<String>, database: Option<String>, tree: serde_json::Value }` (`#[schema(value_type = Object)]`), response the data API's JSON text.
- `AppState` gains `locator: Arc<dyn PostgresAddr>` (a port with `postgres_addr(ep) -> Option<SocketAddr>`, implemented by `DockerLocator`) and `console: Console { pools: moka Cache<OwnerPoolKey, PgPool>, cache: QueryCache }` where `OwnerPoolKey { branch, database, fingerprint }` (P4; max 2 connections, 60 s idle).
- Non-owner roles: `JobPort::check_data_role` on first use per (branch, role) (cached 5 min), then `RestInternal::console(branch, role, database, tree)`.

**Behaviour:** owner path: `owner_password_enc` missing → 409 `owner_password_unknown`; database not `on` → 409 `data_api_disabled`; unknown → 404; else begin, `cheladb_query::run(…, RunAs::Owner, …)`, commit. Audit `info!(user, branch, database, role, op)` for writes, no values.

**Tests:** `owner_console_runs_through_the_owner_pool` (a fake executor seam or a sqlx test against the dev metadata Postgres as a stand-in compute — implementer picks and states it); `owner_password_unknown_then_works_after_reset` (Review Focus 3); `a_disabled_database_is_409`; `other_roles_check_once_then_forward_to_rest`; `owner_level_role_is_refused`; `console_writes_are_audited_without_values`; `the_owner_password_never_goes_to_rest` (the rest fake never receives a body or header containing it).

**Verify:** `cargo nextest run -p orchestrator`, clippy, `just api-gen`, `just check`.

## Task 7: SDK and CLI

**Files:** `packages/client/src/{index.ts,query.ts,types.ts}` and tests, `tools/cli/src/{main.rs,generate/mod.rs}` and tests, `packages/client/fixtures/gen/*` goldens.

**Interfaces:** `db.database<DB2 extends DatabaseShape>(name, types?)` returns a `Client<DB2>` whose runner adds the top-level `database` (batches: top level only). The generated file exports `export type Databases = { main: {…}, analytics: {…} }` (each value a `DatabaseShape`) and a `types` map keyed by database; with one database it still exports `Database` (= the main one) for compatibility. CLI: `--database <name>` adds `?database=`; `--all-databases` reads `databases` (P6) and generates all.

**Tests:** `database_binds_the_top_level_field`, `batch_carries_database_only_at_top_level`, type tests for `createClient<Databases["analytics"]>`, CLI golden for two databases, `--database` query string.

**Verify:** client `test` + typecheck; `cargo nextest run -p cheladb` (CLI package name per the scan).

## Task 8: Studio Data API view

**Files:** `apps/studio/src/components/{database-data-api.tsx,data-api-panel.tsx,data-api-keys.tsx,database-settings.tsx,data-api-databases.tsx (new)}` and tests, `apps/studio/src/lib/branches.ts`.

**Behaviour:** per P1 and spec Part 5's Data API bullets: database list with states and Enable/Disable/Remove machinery (confirm), polling while `enabling`; `default_schema` per database; RLS warning for the picked database; key allow-list chips and a multi-select editor for both kinds; secret-key form selector; `default_schema` removed from the settings form.

**Tests:** `databases_list_shows_states_and_enable`; `enabling_polls_until_on`; `system_database_cannot_be_enabled`; `disable_and_remove_confirm`; `default_schema_saves_per_database`; `allow_list_edit_for_public_key`; `create_secret_key_with_databases`; settings form no longer shows default schema.

**Verify:** Studio `test`, `typecheck`, `build`.

## Task 9: Studio Tables console

**Files:** `apps/studio/src/components/{tables-view.tsx,console-grid.tsx,console-filters.tsx,row-editor.tsx} (new)` and tests, `apps/studio/src/lib/console.ts` (tree builders), one line in `dashboard.tsx` (after T8 merges).

**Behaviour:** spec Part 5's Tables bullets, minimal styling: toolbar (branch, database, schema, view-as), object list with RLS badge, grid (50/page, sort, filters, null display), CRUD by primary key (P12), read-only reason for views/no-PK tables, type-aware inputs with a NULL toggle, owner banner, `owner_password_unknown` notice, plain error texts.

**Tests:** `view_as_defaults_to_owner_with_banner`; `paging_and_sort_build_the_right_tree`; `filters_build_and_or_trees`; `edit_cell_sends_update_by_primary_key`; `insert_marks_defaults`; `delete_confirms_count`; `no_primary_key_is_read_only`; `owner_password_unknown_shows_reset_link`; `error_codes_have_plain_text`.

**Verify:** Studio `test`, `typecheck`, `build`.

## Task 4b: grant existing objects on a schema change (U1)

**Files:** `crates/cheladb-nats/src/lib.rs` (`DataApiDatabaseRequest.grant_existing: bool`, serde default false), `services/worker/src/data_api.rs` (rebootstrap with `grant_existing` runs the first-grants statements for that schema, regardless of `cheladb.bootstrap`), `services/orchestrator/src/data_api_databases.rs` (`PATCH …/pg-databases/{name}` takes `grant_existing: bool`, default false, passed to rebootstrap), tests, `just api-gen`. Also a receiver test from the fork's F3 notes: one transaction `CREATE ROLE b; ALTER ROLE b RENAME TO c; ALTER ROLE c PASSWORD NULL` arrives as `{set, name c, old_name b, password null}` → 200 and no `neon_roles` row.

**Tests:** `rebootstrap_grants_existing_only_when_asked`; `patch_default_schema_passes_grant_existing`; `ddl_rename_of_an_unknown_role_with_null_password_leaves_no_row`.

## Task 10: live tests, e2e, docs

**Files:** `services/rest/tests/rest_live.rs` (a `database` parameter on `sql_on`, a public worker wrapper for enabling), `scripts/console-e2e.sh` (new, modelled on `data-e2e.sh`), `justfile` (`console-e2e`), `README.md`, `.env.example`.

**Live tests:** `two_databases_serve_separately_with_their_own_schema`; `allow_list_limits_a_key_to_one_database`; `disable_answers_unknown_database_at_once`; `child_branch_serves_the_parent_second_database`.
**`just console-e2e`:** create a database; `CREATE DATABASE analytics` via psql; enable it; a key allowed only on `analytics`; CRUD through the console as the owner and as `anon` under an RLS policy; disable; delete.
**Docs:** README sections for serving more databases, allow-lists, the console, `CHELADB_BRANCH_DATA_API_DATABASE_CAP`.

## Verification run (one at a time)

`cargo nextest run --workspace`, clippy (`-D warnings`), `just check`, Studio test/typecheck/build, `live_db`, `rest_live`, `roles_live`, `just data-e2e`, `role-e2e`, `branch-e2e`, `neon-e2e`, `console-e2e`, and the Playwright MCP pass. Touch sources before verifying (shared target dir).
