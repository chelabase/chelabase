# TE-A: the table API (design)

Branch `table-api`. Roadmap step TE-A (before TE-B, the editor UI). Decisions (1)–(7) are in the roadmap's "TE-A decisions" note; the user approved the whole design on 2026-10-13 without going part by part. Choices made while writing this spec (not asked) are listed as **S-rulings** at the end for review.

## Goal

Typed, tested, documented management API endpoints that let Studio's table editor (TE-B) read and change table structure without hand-written SQL: an object list, a table's structure, a type catalog, enum management, DDL with a dry run that returns the exact SQL, an all-defaults insert and a full-cell fetch. Plus the regenerated `@chelabase/management-client` and docs pages. No fork change, so no image build; the live pass runs on the pinned images.

Non-goals: views/functions/triggers editing (listed read-only), partition management, the editor UI, a "grant all rows" toggle (Members follow-up (b), handled as a starter-policy preset in TE-B if wanted).

## 1. Endpoints and access

All under `/v1/databases/{id}/branches/{branch_id}/`, OpenAPI tag **Tables**. The Postgres database is `?database=` (GET) or `database` in the body (POST), defaulting as the SQL editor does.

| Route | Purpose | Access |
|---|---|---|
| `GET …/objects` | Object list | `Project(Viewer)` |
| `GET …/tables/{schema}/{table}` | One table's structure | `Project(Viewer)` |
| `GET …/types` | Type catalog (incl. enums) | `Project(Viewer)` |
| `POST …/schema-changes` | All DDL and enum changes, dry run or real | `ActAsBranch { unprotected: Developer, protected: Admin }` (new) |
| `POST …/tables/{schema}/{table}/rows/default` | Insert one all-defaults row, return it | `ActAs(Developer)` |
| `POST …/tables/{schema}/{table}/cell` | Full value of one cell by primary key | `ActAs(Viewer)` |

- **`Access::ActAsBranch`** (new, `chelabase-types/src/access.rs`): the `ActAs` checks (the requested role is in the effective set; the owner needs Admin; R16/R21/R22/R23 refusals) **and** the `Branch` minimum by protection (unknown = protected). `decide`'s exhaustive test, `spec_table`, `auth_and_access_agree`, the route walk (`access_walk.rs` `body_of` gets typed bodies for the new POSTs) and the generated `reference/access.mdx` cover it.
- Reads need no role: Postgres catalogs are readable by every role, so structure is not secret from any member who may view the project.
- Tables are addressed by schema and name (URL-encoded path segments), never by OID.

## 2. Read side

**Where reads run.** The orchestrator connects as the database owner (the pooled owner connection the console uses) and runs a new read-only catalog query, in `REPEATABLE READ READ ONLY`, with `statement_timeout` 15 s. It does not go through the worker (latency) and does not reuse `CATALOG_SQL` as is (it reads the whole database and touches `chelabase.app_users_table`, which the owner can't read). (S1)

**Shared model.** `chelabase-schema` gains a scoped reader: `catalog::table_sql()` / `parse_table(json) -> Result<Table, SchemaError>` returning the existing `model::Table` (columns, constraints, indexes, policies, grants, RLS, owner, comment) for one table, from the same SQL fragments as `CATALOG_SQL` (the fragments are factored so both reads share them; a test asserts a table read through both paths is equal). The app-users hidden columns/policies and the `cb_m_*`/`chelabase_*` ACL hiding apply identically. New fields follow the serde contract (`#[serde(default)]`, externally tagged enums).

**Object list** (`ObjectSummary`): `schema, name, kind (table | partitioned_table | view | materialized_view | foreign_table), oid, owner, comment, rls_enabled, rls_forced, data_api_exposed, row_estimate (reltuples, null when never analyzed), total_bytes, editable` (`editable` = ordinary table, not extension-owned, not a partition child). Excluded schemas: the `exclusions.rs` list.

**Table structure** response: the `model::Table` plus `editable` and, per column, `type_ref` (schema-qualified type name and its category for the editor).

**Type catalog** (`TypeCatalog`): `base` (built-ins by `typcategory`, with `takes_length/precision`, `is_preferred`), `enums` (with labels in order), `domains` (base type, not null, checks), `composites`, `ranges`; arrays as a flag on each entry. Pseudo-types and internal types are excluded.

## 3. Changes

**Request** `SchemaChangeRequest { database?, role? (as the SQL editor's; null = owner), dry_run: bool, operations: Vec<Operation>, cascade?: { dependents_hash }, lock_timeout_ms? (default 5000, max 60000) }`. `Operation` is externally tagged, one variant per change:

- tables: `create_table { schema, name, columns, primary_key?, constraints, rls?, policies, grants, comment?, owner? }`, `rename_table`, `set_table_schema`, `drop_table`, `set_table_comment`, `set_table_owner`;
- columns: `add_column`, `alter_column { type?, using?, default? (set|drop), not_null?, identity? (add|set|drop), collation?, comment? }`, `rename_column`, `drop_column`;
- constraints: `add_constraint { primary_key | unique | foreign_key | check | exclusion, not_valid? }`, `validate_constraint`, `rename_constraint`, `drop_constraint`;
- indexes: `create_index { …, concurrently? }`, `drop_index { concurrently? }`, `rename_index`;
- RLS: `set_rls { enabled, forced }`; policies: `create_policy`, `alter_policy`, `rename_policy`, `drop_policy`;
- grants: `grant`, `revoke` (table- or column-level), `set_data_api_exposure { exposed }` (the existing exposure mechanism);
- enums: `create_enum`, `add_enum_value { before|after? }`, `rename_enum_value`, `rename_enum`, `drop_enum`.

Expressions (defaults, checks, policy `USING`/`WITH CHECK`, index predicates, `USING` casts) are SQL text the caller supplies; Postgres parses them as the chosen role, so they run with that role's rights. Everything else (names, types, roles) is quoted by the renderer.

**Renderer** (`chelabase_schema::ddl`, pure): `render(&[Operation], &RenderContext) -> Result<Vec<RenderedStatement>, DdlError>`, where `RenderedStatement { sql, op_index, rewrites_table, lock: LockLevel, concurrently }`. It reuses `sql::{ident, qualified, literal}` and the plan renderers in `plan/ddl.rs`, which become `pub(crate)` and shared (`column_def`, `identity_clause`, `create_index`, `create_policy`, `grant_sql`, `revoke_sql`, `comment`, `sequence_options`), so merges and the editor produce the same SQL for the same object. `chelabase_types::role_name::{quote_ident, quote_literal}` duplicates are not touched in this step. `rewrites_table` is decided from the operation and the catalog (type change not binary-coercible, `add_column` with a volatile default, `SET LOGGED/UNLOGGED`); unknown counts as rewriting. Validation errors (`DdlError`) map to `invalid_request` with the operation index.

**Running.** The orchestrator opens the session exactly as `run_sql` does (`open_session`: owner connection, or the member's `cb_m_*` login + `SET ROLE`, with its reconcile/retry and hand-over), then in one transaction: `SET LOCAL lock_timeout`, `SET LOCAL statement_timeout` (real: 60 s; dry run: 15 s), `SET LOCAL ROLE`, the cascade check (part 4), each statement in order. Real run commits and returns `{ statements: [{sql, notices}], committed: true }`. A Postgres error rolls back and returns a typed error with `op_index`, `sqlstate`, `message`, `detail`, `hint`, `position`.

**Dry run** = the same rendered statements, executed in the same way, always `ROLLBACK`. Statements with `rewrites_table` on a table whose `pg_total_relation_size` is over the threshold (default 256 MiB, setting `CHELABASE_TABLE_API_DRY_RUN_MAX_BYTES`) are rendered but not executed, and every statement after the first such one too (its effects can't be simulated); the response marks each statement `executed | rendered_only` with the reason. `concurrently` statements are always `rendered_only` in a dry run, after a catalog validation (table exists, columns exist, name free). Response: `{ statements: [{sql, executed, notices, rewrites_table, lock}], dependents?: {objects, hash}, would_succeed: bool | null }` (`null` when anything was rendered only).

**Errors** (new `ErrorCode`s): `permission_denied` (403, SQLSTATE 42501, with `op_index`), `dependents_changed` (409), `index_build_failed` (409, the concurrent build failed and its INVALID index was dropped), `lock_timeout` (409, SQLSTATE 55P03, retryable), `schema_change_failed` (400, any other SQLSTATE, carrying its fields). Existing ones reused: `invalid_request`, `not_found`, `forbidden`, `branch_protected` (from the guard, part 5), `timeout`.

## 4. Drops, cascade and `CONCURRENTLY`

- `drop_table`, `drop_column`, `drop_constraint`, `drop_index`, `drop_policy`, `drop_enum` render `RESTRICT` unless the request has `cascade`.
- A dry run of a request containing drops always returns `dependents`: every object `DROP … CASCADE` would remove, read from `pg_depend` recursively (kind, schema, name, parent), sorted, and `hash` = SHA-256 of their canonical JSON. A request with `cascade: { dependents_hash }` renders `CASCADE`; inside the real run's transaction, after `SET LOCAL lock_timeout` and before any drop, the same dependents query runs after taking the drop targets' locks (`LOCK TABLE … IN ACCESS EXCLUSIVE MODE` for tables; the target table for columns/constraints/indexes/policies), and a different hash aborts with `dependents_changed` (with the new list). `cascade` without a hash is `invalid_request`.
- `concurrently: true` is allowed only on `create_index`/`drop_index` and only as the request's single operation (else `invalid_request`). It runs outside a transaction, as the chosen role, with `lock_timeout`; on failure the orchestrator (as the owner) drops the index if it exists and `indisvalid` is false, then returns `index_build_failed`.

## 5. The DDL guard on protected branches

- **Marker role.** A cluster-wide `NOLOGIN` role `chelabase_schema_locked` (reserved prefix, hidden from role lists and ACL output). On a protected branch, the Members reconciler grants it to every `cb_m_*` login whose member is not Admin on the project (and revokes it otherwise / on unprotected branches). Role membership is per cluster, so per branch, and a branch created from a protected branch is reconciled like any other branch (the reconciler already runs on branch create via the periodic sweep and grant changes). The reconciler learns to own this membership (no drift `Reshape` for it); it doesn't use per-role GUCs, which it treats as drift. (S2)
- **Event trigger.** `chelabase_schema_guard` on `ddl_command_start`, a `SECURITY DEFINER` function owned by `chelabase_admin`: if `pg_has_role(session_user, 'chelabase_schema_locked', 'MEMBER')`, raise `42501` with message `branch is protected: schema changes need Admin` and `HINT 'chelabase:branch_protected'`; the SQL editor's NDJSON error carries it, and the table API maps the hint to `branch_protected`. Superuser sessions (`cloud_admin`, migrations, merges) and the owner connection are unaffected (not `cb_m_*`).
- **Installed in every Postgres database of every branch** (not only data-API databases): a new idempotent `schema_guard_sql()` in the worker, run by the reconciler per database alongside its existing per-database work, and checked by the reconcile round-robin's health check (missing or disabled trigger → reinstall). It's independent of the data API bootstrap and survives `REMOVE_DATA_API_SQL`.
- **Triggers for reconcile.** Protect/unprotect (`update_branch`) now publishes `chelabase.member_roles.reconcile` for the branch; until it lands, the table API's up-front `ActAsBranch` check already refuses, and the SQL editor's existing `sql_confirm_required` guard still applies.

## 6. Rows, client, docs, tests

- **All-defaults insert:** `INSERT INTO t DEFAULT VALUES RETURNING *` as the chosen role (session as in part 3), returned as JSON (the data API's row encoding). Errors map as the data API's (`constraint_violation`, `permission_denied`…).
- **Full-cell fetch:** body `{ database?, role?, key: {column: value…} (the primary key), column }`; `SELECT col::text, pg_column_size(col) FROM t WHERE pk = $…` (values bound as parameters, names quoted), returns `{ value, bytes, type }`, capped at 64 MiB (`too_large` beyond). A table without a primary key: `invalid_request` (`no_primary_key`). Returns `not_found` when no row matches.
- **Client and docs:** `just api-gen` regenerates `openapi/management.json` and `@chelabase/management-client`; `docs_tables` regenerates access and error tables; a new hand-written page `apps/docs/content/docs/using/table-api.mdx` (dry run, cascade, `concurrently`, protected branches) and `architecture/table-api.mdx` (renderer shared with merges, the guard). The OpenAPI tag whitelist gets **Tables**.
- **Tests (the coordinator writes them RED per the standing rule):**
  - renderer: an example test per operation (exact SQL), plus a property test that every rendered statement round-trips through `pg_query` parsing and contains no unquoted user identifier (names drawn from hostile strings: quotes, keywords, Unicode, uppercase);
  - shared-renderer invariant: for each object kind, `ddl` rendering of an object equals the merge planner's rendering;
  - `rewrites_table` classification table test;
  - access: `decide` exhaustive test extended for `ActAsBranch`; route walk; `spec_table`;
  - catalog: table read via the scoped reader equals the same table from `CATALOG_SQL`;
  - live (`just live`): create a table with columns/RLS/policies/grants as a non-owner role and read it back; dry run leaves no trace; `permission_denied` typed; cascade hash mismatch → `dependents_changed`; concurrent index failure drops the INVALID index; the guard refuses a Developer's DDL on a protected branch via both the table API and the SQL editor while row changes succeed, and allows it after unprotect; Admin unaffected; all-defaults insert; full-cell fetch of a large value.

## S-rulings (chosen while writing; cost if wrong)

- **S1** reads run on the orchestrator's owner connection with a scoped catalog query, not the worker's `CATALOG_SQL`. Cost if wrong: one more read path to keep equal to the merge reader (covered by the equality test).
- **S2** the guard flag is membership in a marker role `chelabase_schema_locked`, not a per-role GUC (the reconciler wipes GUCs as drift) nor a table (would be per database, not per branch). Cost if wrong: a reconciler change to undo.
- **S3** dry-run size threshold 256 MiB, dry-run `statement_timeout` 15 s, real 60 s, default `lock_timeout` 5 s. Cost: tuning only.
- **S4** expressions are caller-supplied SQL text parsed by Postgres as the chosen role (no expression builder in the API). Cost: TE-B shows Postgres's errors for bad expressions.
- **S5** guard refusals use SQLSTATE 42501 with a `chelabase:branch_protected` hint, so plain Postgres clients see a normal permission error.
- **S6** `role_name::{quote_ident, quote_literal}` duplicates are left alone in this step.
