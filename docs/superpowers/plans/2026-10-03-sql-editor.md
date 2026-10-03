# SQL editor (roadmap step 6d) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking. This repo's `CLAUDE.md` overrides the skill where they differ: lean plans, strict TDD with RED evidence, parallel waves in their own worktrees, `just live` for anything touching the stack.

**Goal:** a Studio SQL view that runs raw Postgres SQL on a branch's databases as the owner or a chosen role, streaming results, notices and errors, with cancel, a server-side guard, private saved queries, a per-browser history, and pgls-powered completion.

**Architecture:** a new orchestrator module `sql_editor` opens a fresh tokio-postgres connection per run (owner credentials from 6c's path) and streams NDJSON events through the gateway; pg_query classifies statements for the guard; saved queries live in a new metadata table. Studio adds a fourth database view ("SQL") with CodeMirror 6 and `@postgres-language-server/wasm` in a web worker, fed schema JSON by the orchestrator.

**Tech Stack:** Rust (axum 0.8, utoipa, tokio-postgres 0.7.18 `NoTls`, pg_query 6.2.1, tokio-util 0.7.19, sqlx 0.9 for metadata), NATS RPC to the worker, React 19 + TanStack Query, CodeMirror 6, `@postgres-language-server/wasm` 0.26.0, vitest.

**Spec:** `docs/superpowers/specs/2026-10-03-sql-editor-design.md` (approved 2026-10-03). Read it with this plan; the plan's rulings (below) override it where they differ.

## Status

- **Current (2026-10-03): MERGED into `main` at the user's word (`060f1a6`); the scratch metadata DB `cheladb_metadata_6d` is dropped.** All tasks are merged into `sql-editor`. The end-of-step review (opus) returned *With fixes* (I1: audit target not enabled by the default log filter; I2: memory not bounded by the response cap). The single fix batch `3d09e76`..`e2f834c` fixed both plus the fix-before-merge minors, the README numbers and two Playwright UX items, and its re-review (opus) found everything addressed. Then:
  - **Verification after the fixes:** `just check` and `just test` pass (nextest 1,384), Studio 477 tests, typecheck and build; `just live` re-run: `sql-editor-e2e` 92 s (including closing a stream through the gateway), audit lines present with no SQL text (17 lines; 2 `confirm_required`), ignored orchestrator live tests 25/25, `console-e2e` 76 s, `data-e2e` 36 s; metadata `live_db` 133/133 earlier; Playwright pass 10/10.
  - **Merge checklist done:** the temporary separate-metadata-DB block is reverted (`87350a8`); `main` is merged in (`229de76`). After the merge, the first `just dev` from `main` applies `20261009000001_saved_queries.sql` to the shared metadata DB.
  - **Leftovers:** the scratch metadata DB `cheladb_metadata_6d` (drop after the merge), the Zitadel user `pw-6d@example.com`.
  - **Second computer:** `sudo apt-get install -y clang protobuf-compiler` before building (pg_query).
  - **Upstream:** https://github.com/rust-postgres/rust-postgres/pull/1391 (when released, move `tokio-postgres` back to crates.io).
- **Pre-flight scan (2026-10-03):** codebase map (sonnet, static), pgls 0.26.0 API pin (sonnet; tag commit `653ecab1`), live checks (opus, through `just live`, Postgres 17.11). Findings are folded into the rulings and tasks below.
- **Rulings (made while planning, listed to the user; each with its cost if wrong):**
  - **R3. No client-side guard hint.** pgls WASM's `parse` returns only error strings (no AST), so Studio can't classify statements. Studio relies on the server's 409 `sql_confirm_required` (checked before connecting: one quick round trip) and shows the confirm dialog from it. *Cost: none visible; the server was authoritative anyway.*
  - **R4. Placement.** Studio has no per-branch tabs: each database row has view buttons (`project-page.tsx` `DatabasesTable`, `view: "branches" | "data-api" | "tables"`). The SQL editor is a fourth view `"sql"` (button "SQL") with its own branch `<select>`, like `TablesView`. *Cost: none; the UI revamp replaces it.*
  - **R5. Bearer auth for the stream.** Studio authenticates with a Bearer token (`configureManagementClient`), not a cookie. The run uses a new `streamRequest` export in `packages/management-client/src/fetch.ts` that reuses the client's base URL, token, 401 handling and `readApiError`. *Cost: none.*
  - **R6. No TLS to the compute.** The compute port has `ssl = off` (live: `sslmode=require` refused; 6c's `prefer` always falls back). The editor connects with `tokio_postgres::NoTls` on the loopback port; `tokio-postgres-rustls` isn't added. *Cost: revisit in platform mode (computes on other hosts).*
  - **R7. Postgres 17 only.** The stack creates every timeline with `PG_VERSION = 17` (`services/worker/src/ports.rs`), and only `neon-compute-v17` exists. The spec's 14–16 checks are dropped; pgls's SQL uses nothing newer than PG 11 (static check). *Cost: a version-gated variant if other majors arrive.*
  - **R8. Roles.** Neon sets `createrole_self_grant = 'set, inherit'`, so the owner can already `SET ROLE` to API roles, SQL-made roles and owner-level roles (live). Only `anon` and `authenticated` (created by `cloud_admin`) need a grant, done **on first use by a worker RPC** (user's answer, 2026-10-03): `GRANT <role> TO <owner> WITH INHERIT FALSE, SET TRUE` through `admin_sql`, cached 5 min per (branch, role). Live: with INHERIT FALSE the owner gains nothing until `SET ROLE`.
  - **R9. Schema introspection wraps pgls's SQL.** Each of the 12 pgls queries runs as `SELECT coalesce(json_agg(q), '[]')::text FROM (<file without trailing ;>) q` (`versions.sql`: `row_to_json`), and Rust post-processes the JSON (strip the `!` suffix from keys, map char codes to enum names, wrap `enums`/`attributes`, decode the trigger bitmask). No per-column Rust structs. *Cost: none; one fixture test pins the shape.*
  - **R10. Notices count toward the 10 MiB cap** (a `VACUUM` as a plain role emits one WARNING per relation, 60+ on a fresh database). No separate notice cap. *Cost: a huge notice flood ends the run as `truncated_response`.*
  - **R11. Cancel signal.** The run registry holds a `tokio_util::sync::CancellationToken` per run; the run task itself sends Postgres's cancel request when the token fires, the deadline passes, the cap is hit or the client disconnects. *Cost: none.*
- **Done:**
  - T3 `5d4141d`..`00927cd` (fork dependency `c106c1f`; one fix round: a stalled client no longer pins its slot); T6a `3fec1ff`..`6ed63d4` (one fix round: the owner travels in the request); T6b `bc898fa`..`2b69b0f` (one fix round: R17); T9 `4536f47` (review clean); T10 `8d730d2` (no own review). API clients regenerated `7dedfba`; `just check` passed. Schema JSON live: 2,593,381 bytes (214 tables, 3,335 functions, system catalogs included).
  - T2 `28d8f58` (review clean), T5 `1a8f31b`..`050bf87` (one fix round), T4 `06a0f68` (review clean), T8 `b6225cf` (review clean); merged (`5e4989a`, `11410e8`, `18660c0`, `a3de5ef`). T5 live: the schema JSON is 2,586,338 bytes on a fresh database (cap 8 MiB); the fixture loads in pgls 0.26.0's `setSchema`.
  - T1 `43558c3` (no own review, per plan). Two orchestrator tests fail on the stale OpenAPI document until api-gen; cheladb-types' error-code tests need `--features axum`; the first pg_query build took ~25 min.
  - T7 `269aaa6` (review clean), merged `a9638c9`.
- **Rulings during execution:**
  - R17: connect failures map by SQLSTATE (28P01 → 409 `owner_password_unknown`, 3D000 → 404, 57P03/class 53 → 503, else 500), overriding the plan's blanket 409. *Cost: none.*
  - **User decisions (2026-10-03):** commit as `the maintainer's GitHub noreply identity` from now on (CLAUDE.md on `main`, `ad65f33`); no history rewrite of the public forks (superseded 2026-10-04: the forks were replayed too, see `CLAUDE.md`; they keep their older commits under the previous identity; this repo's history was later replayed into `chelabase/chelabase` with every commit as `<maintainer>`, 2026-10-03); the upstream PR is opened as `<maintainer>` from `chelabase/rust-postgres` `upstream/simple-query-tags` (`8bf6bc1`), while Cheladb pins `cheladb/simple-query-tags` (`d20c5b3`).
  - **User decision D15 (2026-10-03): fork tokio-postgres.** tokio-postgres 0.7.18's simple query drops the command tag, reports an empty query as `CommandComplete(0)` and gives no column types. The user chose a fork, `chelabase/rust-postgres` (public, created 2026-10-03), with an additive patch, over an own driver on postgres-protocol, because TLS with channel binding comes free later and the data API chore (6e) could share it. The upstream PR is approved, to be opened after our live tests pass.
  - R14: T6 split into T6a (worker grant RPC, now) and T6b (routes, audit, after T3). *Cost: one extra merge.*
  - R15: T2 and T5 showed RED after the fact (a stub; a self-generated fixture checked by the real pgls `setSchema`). Accepted; reviews checked the rules and mappings independently. *Cost: tests written alongside code may mirror its assumptions.*
  - R16: empty-string saved-query hints will be normalised to null (end batch). *Cost: none.*
  - R12: `run::Connected::client()` accessor for the schema route. *Cost: none.*
  - R13: the plan's `errorOffset` example used position 15; `nope` is at code point 13 (test uses 13). *Cost: none.*
- **Deferred minors (left after the fix batch; can wait):** the row-size estimate ignores the `rows` event wrapper (a batch near the cap can be dropped whole; outcome still `truncated_response`); the schema route doesn't drain notices; Studio offers `anon`/`authenticated` only when the data API is `on` (the API lacks `data_api_bootstrapped`); the cancel route doesn't check the run's branch against the URL; `role="owner"` in audit lines is ambiguous for a role literally named `owner`; the schema JSON is ~2.5 MB per load (system catalogs); SQL view needs a scroll at 720 px. Earlier items below were fixed in the batch unless listed here.
  - T2: `kind_name` formats the whole node via Debug; FOR UPDATE in subqueries and writing functions in SELECT pass as reads (document); tests for MERGE, CALL, `EXPLAIN (ANALYSE)`, multibyte after other statements.
  - T4: `stale()` `unwrap_or_default`; fake sort tie-break; empty PATCH bumps the version.
  - T5: the live test's `has` helper unwraps `as_array`.
  - T8: unstyled hover class; a worker error after `ready` kills the client; build prints four `node:*` externalized warnings.
  - T7: `readNdjson` casts `JSON.parse` unchecked; a non-object line would TypeError in the reducer.
- **Left running:** nothing.

## Global Constraints

- Latest versions, checked 2026-10-03: tokio-postgres **0.7.18**, pg_query **6.2.1**, tokio-util **0.7.19** (already in the lock), `@postgres-language-server/wasm` **0.26.0 exactly** (no caret), `@codemirror/state` 6.7.6, `@codemirror/view` 6.43.13, `@codemirror/commands` 6.11.1, `@codemirror/language` 6.12.4, `@codemirror/autocomplete` 6.20.3, `@codemirror/lint` 6.9.7, `@codemirror/search` 6.7.2, `@codemirror/lang-sql` 6.10.0. Re-check with the registry at task start; respect pnpm's minimum release age (don't keep exemptions it writes).
- New Rust deps go in root `[workspace.dependencies]`, members use `{ workspace = true }`. One TLS backend (aws-lc-rs) for the workspace; tokio-postgres is used with default features off plus `runtime` only if needed (no `with-*` features required; no TLS crate).
- pg_query needs `clang` and `protoc` at build time (installed on this machine 2026-10-03).
- Limits (constants, `services/orchestrator/src/sql_editor/mod.rs`): SQL 1 B–1 MiB; `timeout_ms` 100–900,000, default 60,000; 1,000 rows per statement; 200 rows per `rows` event; 10 MiB per response (all event bytes, notices included); heartbeat after 15 s without an event; 4 runs in flight per user and 4 per (branch, Postgres database); schema JSON ≤ 8 MiB; 500 saved queries per user per Cheladb database; saved-query name 1–120 chars.
- `application_name` for editor connections: `cheladb-sql-editor`.
- Error codes (new, `snake_case` in JSON): `run_id_in_use` (409), `sql_confirm_required` (409), `sql_run_limit` (429), `schema_too_large` (422), `saved_query_name_taken` (409), `saved_query_conflict` (409), `saved_query_cap` (409).
- Studio copy (verbatim):
  - "Running as owner: row-level security is bypassed."
  - "Running as `<role>` (SET ROLE). This previews that role's view; it isn't a security boundary, since a script can RESET ROLE."
  - "Protected branch: every change asks for confirmation."
  - "Showing the first 1,000 rows"
  - "Saved queries are private to you but stored on the server. Don't save secrets."
  - "Basic completion"
- The audit line never contains SQL text or values.
- Never run `cargo fmt` on the workspace (rustfmt only files you create). Never hand-edit generated files (`openapi/*.json`, `packages/management-client/src/generated/**`, `apps/studio/src/lib/login-client/generated/**`, `routeTree.gen.ts`); only the coordinator runs `just api-gen`, once after each wave.
- Implementer worktrees never run the stack. Ignored live tests and e2e runs go through `just live …`, run by the coordinator (or a live-run subagent it dispatches), one at a time, holding the stack ≤ 15 minutes per chunk.
- Commits: `git commit`, message ending with the `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>` and `Claude-Session: https://claude.ai/code/session_01SKtYKifyzRBi8hgU1Ve6WD` trailers.

## Review Focus

1. **A client that disconnects mid-run** (tab closed, network drop): the query must be cancelled on the compute, the run unregistered and the audit line written; no orphan backend. → T3 test `client_drop_cancels_the_backend` (live), T6 test `closing_the_stream_cancels_the_run` (live).
2. **Multibyte SQL** (é, emoji, CJK): Postgres's `position` counts characters (code points), pgls uses UTF-8 bytes, CodeMirror uses UTF-16; a wrong conversion underlines the wrong token or completes the wrong word. → T7 `positions_count_code_points`, T8 `offsets_round_trip_multibyte`.
3. **A script whose first statement errors or whose `SET ROLE` fails** (role dropped since the picker loaded): the stream must still end with `error` then `done` (`outcome: error`), and Studio must show it, not hang. → T3 `set_role_failure_is_an_error_event`, T7 reducer test `error_then_done_stops_running`.
4. **Two runs racing for the same `run_id`, or a fifth concurrent run**: 409/429 before any connection is opened, and the in-flight runs unaffected. → T3 `reserve_refuses_duplicates_and_caps`, T6 `fifth_run_is_429`.
5. **The guard on statements with odd boundaries** (comments only, trailing semicolons, `DO $$ … $$` bodies containing `DROP`, a `WITH … DELETE` CTE inside a `SELECT`): dollar-quoted bodies must not trigger "risky", data-modifying CTEs must count as writes on protected branches. → T2 `dollar_quoted_drop_is_not_risky`, `data_modifying_cte_is_a_write`, `comment_only_script_holds_nothing`.

---

## Task graph, models and waves

| Task | What | Implementer | Reviewer | Wave | Depends on |
|---|---|---|---|---|---|
| T1 | Foundations: deps, error codes, `sql_editor` module skeleton, `AppState` field | sonnet | none (compile-checked by every later task; covered by end-of-step review) | 1 | — |
| T2 | Guard (`guard.rs`, pg_query) | sonnet | sonnet | 2 | T1 |
| T3 | Run engine (`events.rs`, `registry.rs`, `run.rs`) + `owner_target` refactor | opus | opus | 2 | T1 |
| T4 | Saved queries (migration, metadata, routes) | sonnet | sonnet | 2 | T1 |
| T5 | Schema introspection (`schema.rs`, pgls SQL) | sonnet | sonnet | 2 | T1 |
| T7 | Studio stream client + NDJSON reducer | sonnet | sonnet | 2 | — |
| T8 | pgls worker + CodeMirror adapter | opus | sonnet | 2 | — |
| T6 | Routes: run, cancel, schema; worker grant RPC; audit | opus | opus | 3 | T2, T3, T5 (T4 merged first: shared files) |
| — | Coordinator: `just api-gen`, `just check` | coordinator | — | after 3 | T4, T6 |
| T9 | Studio SQL view UI | sonnet | sonnet | 4 | T7, T8, api-gen |
| T10 | `sql-editor-e2e`, README, roadmap | sonnet | none (e2e-gated; end-of-step review) | 4 | api-gen |
| — | Live verification: ignored tests, `sql-editor-e2e`, Playwright pass | coordinator via `just live` (sonnet subagent for the browser pass) | — | 5 | all |
| — | End-of-step whole-branch review | — | opus | 6 | all |

**Parallel worktrees:** wave 2 runs T2, T3, T4, T5, T7, T8 side by side, each in `just wt-add sql-editor-tN sql-editor`, reviewed there, merged back `--no-ff` in this order: T2, T5, T3, T4, T7, T8 (T4 touches `lib.rs`/`openapi.rs`/`tests/api.rs`; T3 touches `console.rs`; the rest own disjoint files). Wave 4 runs T9 and T10 side by side.

**File ownership in wave 2 (disjoint):** T2 `sql_editor/guard.rs`; T3 `sql_editor/{events,registry,run}.rs`, `console.rs`, `tests/sql_run_live.rs`; T4 migration, `cheladb-metadata/src/saved_queries.rs` (+ `lib.rs` re-export, `tests/live_db.rs` sweep), orchestrator `lib.rs` (MetadataPort), `openapi.rs` (Doc), `tests/api.rs` (FakeMetadata + saved-query tests), `sql_editor/saved.rs`, `cheladb-routes` saved-query block; T5 `sql_editor/schema.rs`, `sql_editor/pgls/*.sql`; T7 `packages/management-client/src/fetch.ts` (+ test), `apps/studio/src/lib/sql-run.ts` (+ test); T8 `apps/studio/src/lib/pgls/**`, `apps/studio/package.json`, `pnpm-lock.yaml`, `apps/studio/vite.config.ts`.

---

### Task 1: Foundations

**Files:**
- Modify: `Cargo.toml` (workspace deps), `services/orchestrator/Cargo.toml`, `Cargo.lock`
- Modify: `crates/cheladb-types/src/api_error.rs` (enum, `as_str`, the `ALL` test array; bump its length)
- Check: `services/gateway/src/pipeline.rs` for a code table that lists every `ErrorCode` (extend it if it exists)
- Create: `services/orchestrator/src/sql_editor/mod.rs`, empty-skeleton `guard.rs`, `events.rs`, `registry.rs`, `run.rs`, `schema.rs`, `saved.rs`, `routes.rs`
- Modify: `services/orchestrator/src/lib.rs` (`pub mod sql_editor;`, `AppState.sql_editor`, `.merge(sql_editor::routes())` in `api_router`), `services/orchestrator/src/main.rs`, `services/orchestrator/tests/api.rs` (`spawn_app_parts` builds the field)

**Interfaces (produced):**
- Workspace deps: `tokio-postgres = { version = "0.7.18", default-features = false, features = ["runtime"] }`, `pg_query = "6.2.1"`, `tokio-util = "0.7.19"` (features as needed: none for `CancellationToken`), `futures-util` (already a workspace dep) added to the orchestrator.
- `ErrorCode::{RunIdInUse, SqlConfirmRequired, SqlRunLimit, SchemaTooLarge, SavedQueryNameTaken, SavedQueryConflict, SavedQueryCap}`, appended after `UserTokenWithSecretKey`.
- `sql_editor/mod.rs`:
  ```rust
  pub mod events; pub mod guard; pub mod registry; pub mod run; pub mod saved; pub mod schema;
  mod routes;
  pub const MAX_SQL_BYTES: usize = 1_048_576;
  pub const MIN_TIMEOUT_MS: u64 = 100;
  pub const MAX_TIMEOUT_MS: u64 = 900_000;
  pub const DEFAULT_TIMEOUT_MS: u64 = 60_000;
  pub const MAX_ROWS_PER_STATEMENT: usize = 1_000;
  pub const ROWS_PER_EVENT: usize = 200;
  pub const MAX_RESPONSE_BYTES: usize = 10 * 1_048_576;
  pub const HEARTBEAT: std::time::Duration = std::time::Duration::from_secs(15);
  pub const RUNS_PER_USER: usize = 4;
  pub const RUNS_PER_TARGET: usize = 4;
  pub const APPLICATION_NAME: &str = "cheladb-sql-editor";
  pub const PGLS_VERSION: &str = "0.26.0";
  pub const MAX_SCHEMA_BYTES: usize = 8 * 1_048_576;
  pub const SAVED_QUERY_CAP: i64 = 500;
  pub const DATA_ROLE_GRANT_TTL: std::time::Duration = std::time::Duration::from_secs(300);

  /// The editor's state on `AppState`.
  pub struct SqlEditor {
      pub(crate) runs: registry::RunRegistry,
      pub(crate) granted: moka::sync::Cache<(cheladb_types::BranchId, String), ()>,
  }
  impl Default for SqlEditor { /* granted: max_capacity 10_000, time_to_live DATA_ROLE_GRANT_TTL */ }

  pub(crate) fn routes<M: MetadataPort, J: JobPort>() -> OpenApiRouter<Arc<AppState<M, J>>> {
      routes::routes::<M, J>().merge(saved::routes::<M, J>())
  }
  ```
- `registry.rs` skeleton: `#[derive(Default)] pub struct RunRegistry {}` (T3 replaces it). `routes.rs`/`saved.rs` skeletons: `pub(crate) fn routes<M, J>() -> OpenApiRouter<Arc<AppState<M, J>>> { OpenApiRouter::new() }` with the same bounds as other modules. Other files: a module doc comment only.
- `AppState { …, pub sql_editor: sql_editor::SqlEditor }`.

**Steps:**
- [ ] RED: extend the `ALL` array in `api_error.rs` with the seven `(ErrorCode::X, "x")` pairs; run `cargo test -p cheladb-types api_error` → fails to compile (variants missing: the code under test).
- [ ] GREEN: add the variants and `as_str` arms; tests pass.
- [ ] Add the deps, the module skeleton, the `AppState` field (main.rs, tests' `spawn_app_parts`), and the router merge. `cargo build -p orchestrator` (this compiles pg_query's C build: expect a few minutes the first time) and `cargo nextest run -p orchestrator -p cheladb-types` stay green.
- [ ] Commit `sql-editor T1: deps, error codes, sql_editor skeleton`.

**Gotchas:** pg_query must compile here before wave 2 starts (it proves clang/protoc work). Don't add routes to `cheladb-routes` in T1: the drift test (`every_declared_orchestrator_endpoint_is_routed`) would fail until the handlers exist; T4 and T6 add their own entries. `openapi/*.json` and generated `errorCode.ts` go stale until the coordinator's api-gen; `just check` is expected to fail on them until then.

**Verification:** `cargo nextest run -p cheladb-types -p orchestrator`; `cargo clippy -p orchestrator -p cheladb-types --all-targets -- -D warnings`.

---

### Task 2: Guard (pg_query)

**Files:** `services/orchestrator/src/sql_editor/guard.rs` (unit tests inline).

**Interfaces (produced):**
```rust
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, utoipa::ToSchema)]
pub struct HeldStatement {
    pub index: usize,          // 0-based, script order
    pub kind: String,          // pg_query node name ("DropStmt", "TruncateStmt", …) or "unparsed"
    pub text_preview: String,  // the statement's first 200 chars (trimmed)
    pub reason: HoldReason,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum HoldReason { Risky, Protected }

/// The statements `sql` must not run without `confirm`. Empty means run.
pub fn check(sql: &str, protected: bool) -> Vec<HeldStatement>;
```

**Behavior rules:**
- Parse with `pg_query::parse(sql)`; statements are `protobuf.stmts` in order; each `RawStmt`'s text is `sql[stmt_location .. stmt_location + stmt_len]` (byte offsets; `stmt_len == 0` means "to the end"). Empty statements (comments only) are skipped and don't take an index.
- Parse failure → one `HeldStatement { index: 0, kind: "unparsed", text_preview: <first 200 chars of sql>, reason: Protected if protected else Risky }`.
- **Risky** (any branch): `DropStmt`, `DropdbStmt`, `DropRoleStmt`, `DropOwnedStmt`, `DropSubscriptionStmt`, `DropTableSpaceStmt`, `TruncateStmt`, `DeleteStmt` with `where_clause == None`, `UpdateStmt` with `where_clause == None`, `AlterTableStmt` with any `AlterTableCmd.subtype` in {`AtDropColumn`, `AtDropConstraint`} (compare with `pg_query::protobuf::AlterTableType` variants, not raw integers), `AlterTypeStmt`/`AlterEnumStmt` don't count. Also `DropStmt` nested anywhere is NOT inspected (only top-level statements; `DO` bodies are strings).
- **Read** (protected branches): `SelectStmt` with `into_clause == None`, empty `locking_clause`, and no CTE (in its `with_clause`, recursively through set-operation `larg`/`rarg`) whose `ctequery` is `InsertStmt`/`UpdateStmt`/`DeleteStmt`/`MergeStmt`; `VariableShowStmt`; `ExplainStmt` whose `options` has no `DefElem` named `analyze` (case-insensitive) with a missing arg or a true value, and whose inner query is itself a read. `VALUES` and `TABLE x` parse as `SelectStmt` and follow its rule.
- On a protected branch, every non-read statement is held with reason `Protected` (a risky one too: reason `Protected`); on an unprotected branch only risky ones, reason `Risky`.
- `text_preview`: trim whitespace, cut to 200 chars (chars, not bytes).

**Tests (names → assertion):**
- `drop_table_is_risky` → `DROP TABLE t` on unprotected: one held, kind `DropStmt`, reason `Risky`.
- `delete_without_where_is_risky_with_where_is_not` → two statements, only index 0 held.
- `update_without_where_is_risky`
- `truncate_is_risky`
- `alter_table_drop_column_is_risky_add_column_is_not`
- `create_and_insert_pass_on_unprotected` → empty.
- `dollar_quoted_drop_is_not_risky` → `DO $$ BEGIN DROP TABLE t; END $$` on unprotected: empty.
- `comment_only_script_holds_nothing` → `-- hi\n/* x */` → empty on both.
- `protected_holds_every_write` → `SELECT 1; INSERT …; CREATE TABLE …` protected: indexes 1 and 2, reason `Protected`.
- `protected_allows_reads` → `SELECT …; VALUES (1); TABLE t; SHOW search_path; EXPLAIN SELECT 1` → empty.
- `explain_analyze_is_a_write` → protected, held.
- `select_into_and_for_update_are_writes`
- `data_modifying_cte_is_a_write` → `WITH d AS (DELETE FROM t RETURNING *) SELECT * FROM d` protected: held; unprotected: not held (DELETE in a CTE isn't top-level risky).
- `unparsed_fails_closed` → `SELEC 1` → one `unparsed`, reason per branch.
- `indexes_skip_empty_statements` → `;; DROP TABLE t` → index 0.
- `preview_is_200_chars_and_trimmed` (multibyte input: cut on a char boundary).

**Verification:** `cargo nextest run -p orchestrator guard`; clippy as T1.

---

### Task 3: Run engine and `owner_target`

**Files:**
- Modify: `services/orchestrator/src/console.rs` (split `owner_pool_for`)
- Implement: `services/orchestrator/src/sql_editor/{events,registry,run}.rs` (unit tests inline)
- Create: `services/orchestrator/tests/sql_run_live.rs` (`#[ignore = "needs the metadata Postgres (just deps-up)"]` tests against a scratch database on the dev metadata Postgres, like `ScratchCompute` in `tests/api.rs`; copy the small helper rather than sharing it)

**Interfaces (produced):**
```rust
// console.rs
pub(crate) struct OwnerTarget { pub addr: SocketAddr, pub user: String, pub password: String, pub sealed: String }
/// 6c's lookup: 409 owner_password_unknown, 409 branch_not_ready, 500 credentials_unavailable, 503s.
pub(crate) async fn owner_target<M: MetadataPort, J: JobPort>(
    state: &AppState<M, J>, branch: &BranchRecord, owner: &str,
) -> Result<OwnerTarget, ApiError>;
// owner_pool_for keeps its signature and calls owner_target; console behaviour unchanged.
pub(crate) fn unavailable(message: &str) -> ApiError; // was private

// events.rs
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum RunEvent {
    Notice { severity: String, code: String, message: String, detail: Option<String>, hint: Option<String>, position: Option<u32> },
    Columns { statement: usize, columns: Vec<ColumnInfo> },
    Rows { statement: usize, rows: Vec<Vec<Option<String>>> },
    Complete { statement: usize, tag: String, rows: u64, truncated: bool },
    Error { statement: usize, code: String, message: String, detail: Option<String>, hint: Option<String>, position: Option<u32> },
    Heartbeat,
    Done { outcome: Outcome, duration_ms: u64 },
}
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct ColumnInfo { pub name: String, #[serde(rename = "type")] pub type_name: String }
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome { Ok, Error, Cancelled, Timeout, TruncatedResponse }
impl RunEvent { pub fn to_line(&self) -> bytes::Bytes; } // JSON + "\n"

// registry.rs
pub struct RunRegistry { /* Mutex<HashMap<(String, Uuid), Entry>>; Entry { branch: BranchId, database: String, cancel: CancellationToken } */ }
pub enum ReserveError { InUse, UserLimit, TargetLimit }
pub struct RunGuard { /* removes its entry on drop */ }
impl RunRegistry {
    pub fn reserve(&self, user: &str, run_id: Uuid, branch: BranchId, database: &str) -> Result<RunGuard, ReserveError>;
    /// Fires the run's token; false when this user has no such run.
    pub fn cancel(&self, user: &str, run_id: Uuid) -> bool;
}
impl RunGuard { pub fn token(&self) -> CancellationToken; }

// run.rs
pub struct RunTarget { pub addr: SocketAddr, pub user: String, pub password: String, pub database: String }
pub struct RunSpec { pub sql: String, pub role: Option<String>, pub timeout: Duration }
pub struct Connected { /* client, the connection task's notice receiver, cancel token */ }
#[derive(Debug, thiserror::Error)] // or a plain enum; thiserror only if already a workspace dep
pub enum ConnectError { Unreachable(String), Rejected(String) }
/// A fresh NoTls connection, application_name = APPLICATION_NAME, connect timeout 5 s.
pub async fn connect(target: &RunTarget) -> Result<Connected, ConnectError>;
/// Runs `spec` and yields its events; always ends with `Done`. Dropping the stream cancels the run.
pub fn run(connected: Connected, spec: RunSpec, guard: RunGuard) -> impl futures_util::Stream<Item = RunEvent> + Send + 'static;
```

**Behavior rules:**
- `connect` builds a `tokio_postgres::Config` (host = `addr.ip()`, port, user, password, dbname, `application_name`, `connect_timeout` 5 s), `NoTls`. The connection future is spawned; it is polled with `poll_message` so `AsyncMessage::Notice` values go into an unbounded mpsc that `run` drains (notices interleave by arrival).
- `run` spawns one task that writes events into a bounded mpsc (capacity 64); the returned stream reads it. If a send fails (receiver dropped = client gone) the task cancels the query (`client.cancel_token().cancel_query(NoTls)`) and stops.
- Order inside the task: `SET statement_timeout = <ms>` (simple query), then `SET ROLE <quote_ident(role)>` when set (a failure → `Error { statement: 0, … }` + `Done { outcome: Error }`), then `client.simple_query_raw(&spec.sql)` (or `simple_query` streamed; use the raw stream so rows flow before the end).
- Messages: `RowDescription` → `Columns`. Column `type` rule: if tokio-postgres 0.7.18's simple-query row description exposes each column's type OID, map OIDs to names with a static table of the built-in types (`int2/int4/int8/numeric/float4/float8/bool/text/varchar/bpchar/name/uuid/date/time/timetz/timestamp/timestamptz/interval/json/jsonb/bytea/oid` and their array forms `_int4`…) and send `""` for any other OID; if it doesn't expose OIDs, send `""` for every column. No extra queries mid-run (the connection is busy streaming). Record which case applied in the report; Studio shows the type only when non-empty.
- `DataRow` → buffered into `Rows` events of ≤ 200; after 1,000 rows in a statement, further rows are dropped and `truncated = true` on its `Complete`.
- `CommandComplete` → `Complete { tag, rows: <trailing integer of the tag or 0> }`, statement index += 1. An empty query response advances the index without an event.
- `DbError` → `Error { statement: <current index>, code, message, detail, hint, position: Some(n) only for ErrorPosition::Original }`, then `Done { outcome: Error }`.
- Deadline `spec.timeout` (wall clock from the first `SET`) → cancel query, emit `Done { outcome: Timeout }` (the cancelled query's error is swallowed). Token fired → `Done { outcome: Cancelled }`. Byte count (sum of `to_line` lengths) would exceed `MAX_RESPONSE_BYTES` → don't send that event, cancel, `Done { outcome: TruncatedResponse }`.
- Heartbeat: when 15 s pass with no event sent, send `Heartbeat`.
- The connection is dropped at the end in every path; the `RunGuard` drops with the task (unregisters).
- `reserve`: `InUse` when (user, run_id) exists; `UserLimit` when the user has `RUNS_PER_USER` entries; `TargetLimit` when (branch, database) has `RUNS_PER_TARGET`. Checks and insert under one lock.

**Tests:**
- unit `events_serialize_to_the_spec_shapes` → each variant's `to_line` equals the spec's JSON (`{"t":"complete","statement":0,"tag":"SELECT 3","rows":3,"truncated":false}\n` etc.).
- unit `reserve_refuses_duplicates_and_caps` → same id `InUse`; 5th for a user `UserLimit`; 5th for a target from two users `TargetLimit`; dropping a guard frees a slot.
- unit `cancel_fires_only_the_callers_run` → another user's cancel returns false and leaves the token unfired.
- unit `tag_row_counts` → `SELECT 3`→3, `INSERT 0 5`→5, `CREATE TABLE`→0.
- console's existing tests pass unchanged (owner_target refactor).
- live (`tests/sql_run_live.rs`, ignored): `multi_statement_results_and_tags`, `notice_arrives_before_done` (`DO $$ BEGIN RAISE NOTICE 'hi'; PERFORM pg_sleep(1); END $$; SELECT 1` → the notice event precedes `complete`), `error_position_and_statement_index` (`SELECT 1; SELECT nope` → error statement 1, position 8), `explicit_transaction_is_honoured` (`BEGIN; CREATE TABLE …; COMMIT` then the table exists on a new connection), `lone_vacuum_runs`, `set_role_failure_is_an_error_event`, `row_cap_truncates_but_completes` (`SELECT generate_series(1,5000)` → 1,000 rows, `rows: 5000`, `truncated: true`), `deadline_ends_with_timeout` (`pg_sleep(5)`, timeout 500 ms; `pg_stat_activity` shows no backend left), `token_cancels_the_backend`, `client_drop_cancels_the_backend` (drop the stream mid-`pg_sleep(30)`; within 2 s no `cheladb-sql-editor` backend remains), `response_cap_ends_with_truncated_response` (wide rows), `nothing_leaks_between_runs` (temp table and `SET` gone on the next run).

**Gotchas:** the item about column type names is a known unknown: tokio-postgres's simple-query `RowDescription` exposes names via `SimpleColumn`; check whether 0.7.18 exposes the type OID (`SimpleColumn::type_oid` or the raw `RowDescriptionBody`). If it doesn't, ship `type: ""` and record it as a deferred minor; don't fall back to the extended protocol (it can't run multi-statement scripts). Rows in the simple protocol are text: `SimpleQueryRow::get(i) -> Option<&str>`.

**Verification:** `cargo nextest run -p orchestrator`; clippy; live by the coordinator: `just live cargo nextest run -p orchestrator --test sql_run_live --run-ignored ignored-only`.

---

### Task 4: Saved queries

**Files:**
- Create: `crates/cheladb-metadata/migrations/20261009000001_saved_queries.sql`, `crates/cheladb-metadata/src/saved_queries.rs`
- Modify: `crates/cheladb-metadata/src/lib.rs` (mod + re-exports), `crates/cheladb-metadata/tests/live_db.rs` (tests + `sweep_leftovers` DELETE list)
- Modify: orchestrator `src/lib.rs` (`MetadataPort` + `impl for MetadataClient`), `src/openapi.rs` (`Doc` stubs), `tests/api.rs` (`FakeMetadata` + route tests), `src/sql_editor/saved.rs`
- Modify: `crates/cheladb-routes/src/lib.rs` (a saved-query block in `ENDPOINTS` + a route test)

**Migration (exact):**
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

**Interfaces (produced, `cheladb_metadata`):**
```rust
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct SavedQuery { pub id: Uuid, pub name: String, pub sql: String, pub pg_database: Option<String>, pub role: Option<String>, pub version: i32, pub created_at: DateTime<Utc>, pub updated_at: DateTime<Utc> }
#[derive(…same…)]
pub struct SavedQuerySummary { pub id: Uuid, pub name: String, pub pg_database: Option<String>, pub role: Option<String>, pub version: i32, pub updated_at: DateTime<Utc> }
#[derive(Debug, Clone, Default)]
pub struct SavedQueryPatch { pub name: Option<String>, pub sql: Option<String>, pub pg_database: Option<Option<String>>, pub role: Option<Option<String>> }
pub enum SavedQueryWrite { Saved(SavedQuery), NameTaken, CapReached, Stale(SavedQuery), NotFound }

impl MetadataClient {
    pub async fn list_saved_queries(&self, database: Uuid, user: &str) -> Result<Vec<SavedQuerySummary>, MetadataError>; // by lower(name)
    pub async fn get_saved_query(&self, database: Uuid, user: &str, id: Uuid) -> Result<Option<SavedQuery>, MetadataError>;
    pub async fn create_saved_query(&self, database: Uuid, user: &str, id: Uuid, name: &str, sql: &str, pg_database: Option<&str>, role: Option<&str>) -> Result<SavedQueryWrite, MetadataError>; // Saved | NameTaken | CapReached
    pub async fn update_saved_query(&self, database: Uuid, user: &str, id: Uuid, version: i32, patch: &SavedQueryPatch) -> Result<SavedQueryWrite, MetadataError>; // Saved | NameTaken | Stale | NotFound
    pub async fn delete_saved_query(&self, database: Uuid, user: &str, id: Uuid) -> Result<bool, MetadataError>;
}
```
`MetadataPort` gains the same five methods (same names and argument types, `impl Future<…> + Send`), delegated one line each for `MetadataClient`, `match *self {}` for `Doc`, and an in-memory implementation in `FakeMetadata`.

- Create takes `pg_advisory_xact_lock(hashtextextended(database::text || '/' || user, 0))` in a transaction, counts, refuses at `SAVED_QUERY_CAP` (500; pass it in or define it in the metadata crate and re-use from the orchestrator constant: **define `pub const SAVED_QUERY_CAP: i64 = 500` in `cheladb_metadata` and have the orchestrator's T1 constant re-export it** — T4 changes `sql_editor/mod.rs`'s line to `pub use cheladb_metadata::SAVED_QUERY_CAP;`).
- Update: `UPDATE … SET …, version = version + 1, updated_at = now() WHERE id = $1 AND database_id = $2 AND user_id = $3 AND version = $4 RETURNING *`; zero rows → re-read: missing → `NotFound`, else `Stale(current)`. Unique violation (`constraint() == Some("saved_queries_name")`) → `NameTaken`.
- Queries of a database whose status is `deleted` are hidden by the route (the route loads the database with `load_owned` first, which already 404s deleted ones).

**Routes (`sql_editor/saved.rs`, tag `sql-editor`, all `Auth::Management`/`Cors::Studio` via `key_endpoint`):**
| Method | Path | operation_id | Body | Success |
|---|---|---|---|---|
| GET | `/v1/databases/{id}/sql-queries` | `listSavedQueries` | — | 200 `[SavedQuerySummary]` |
| POST | `/v1/databases/{id}/sql-queries` | `createSavedQuery` | `{ name, sql, pg_database?, role? }` | 201 `SavedQuery` |
| GET | `/v1/databases/{id}/sql-queries/{query_id}` | `getSavedQuery` | — | 200 `SavedQuery` |
| PATCH | `/v1/databases/{id}/sql-queries/{query_id}` | `updateSavedQuery` | `{ version, name?, sql?, pg_database?, role? }` (`null` clears the two hints; absent leaves them) | 200 `SavedQuery` |
| DELETE | `/v1/databases/{id}/sql-queries/{query_id}` | `deleteSavedQuery` | — | 204 |

- Access: `load_owned(state, &principal, id)`; user = `principal.sub`. Bodies `#[serde(deny_unknown_fields)]`; validation (422 `invalid_request`): name trimmed 1–120 chars, sql 1 B–1 MiB, hints ≤ 63 bytes when present.
- Errors: 409 `saved_query_name_taken`, 409 `saved_query_cap`, 409 `saved_query_conflict` with `.with_extra("current", <SavedQuery JSON>)`, 404 `not_found` ("saved query not found") for missing / other user / other database.
- `cheladb-routes`: entries `LIST_SAVED_QUERIES`, `CREATE_SAVED_QUERY`, `GET_SAVED_QUERY`, `UPDATE_SAVED_QUERY`, `DELETE_SAVED_QUERY` (names `orchestrator.list_saved_queries`, …), added to `ENDPOINTS` as one block, with a test `saved_query_routes_are_management_auth_with_studio_cors`.

**Tests:**
- metadata live (`tests/live_db.rs`, ignored): `saved_query_round_trip`, `saved_query_names_are_unique_per_user_case_insensitive` (same name for another user is fine), `saved_query_update_detects_stale_versions`, `saved_query_cap_is_enforced` (insert 500 then `CapReached`; use a smaller cap? no: insert 500 in one statement with a helper SQL, then call create), `saved_queries_cascade_with_their_database`.
- orchestrator (`tests/api.rs`, fakes): `saved_queries_list_create_get_update_delete`, `another_users_saved_query_is_404` (identity sub `user-2` via a second token), `saved_query_conflict_carries_the_current_row`, `saved_query_name_taken_is_409`, `saved_query_validation_is_422`, `saved_queries_of_a_deleted_database_are_404`.

**Verification:** `cargo nextest run -p orchestrator -p cheladb-metadata -p cheladb-routes`; clippy; live by the coordinator: `just live cargo test -p cheladb-metadata --test live_db -- --ignored --test-threads=1 saved_quer` (the separate-metadata-DB rule below applies).

**Gotcha — the migration on the shared metadata DB:** once this migration runs against the shared `cheladb_metadata`, binaries built from `main` refuse to start. Before any live run on this branch, the coordinator adds 6b's temporary exports at the top of the justfile (a separate metadata database for this branch) and reverts them before the merge; T4's implementer doesn't run anything live.

---

### Task 5: Schema introspection

**Files:**
- Create: `services/orchestrator/src/sql_editor/pgls/{columns,extensions,functions,indexes,policies,roles,schemas,sequences,tables,triggers,types,versions}.sql` copied byte-for-byte from `github.com/supabase-community/postgres-language-server` tag `0.26.0` (commit `653ecab1`), `crates/pgls_schema_cache/src/queries/` (a scratch copy may exist at `~/.claude/jobs/356ade48/tmp/pgls-0.26.0-queries/`; otherwise `gh api repos/supabase-community/postgres-language-server/contents/crates/pgls_schema_cache/src/queries/<f>?ref=0.26.0 --jq .content | base64 -d`), plus `services/orchestrator/src/sql_editor/pgls/LICENSE` (the repo's MIT text, "Copyright (c) 2023 Philipp Steinrötter") and a `README.md` (source, tag, the upgrade steps).
- Implement: `services/orchestrator/src/sql_editor/schema.rs`
- Create: `services/orchestrator/tests/fixtures/pgls_schema_0.26.0.json` (a small expected document for the unit test)

**Interfaces (produced):**
```rust
#[derive(Debug)]
pub enum SchemaError { Query(String), TooLarge(usize) }
/// Runs pgls 0.26.0's introspection on `client` (as whoever it's connected as) and returns
/// the JSON `Workspace.setSchema` takes, serialized; TooLarge beyond MAX_SCHEMA_BYTES.
pub async fn introspect(client: &tokio_postgres::Client) -> Result<String, SchemaError>;
/// The pure part, tested without Postgres: per-query JSON arrays in, setSchema JSON out.
pub fn assemble(parts: &RawParts) -> serde_json::Value;
pub struct RawParts { pub versions: serde_json::Value, pub schemas: Value, pub tables: Value, pub columns: Value, pub functions: Value, pub types: Value, pub policies: Value, pub extensions: Value, pub triggers: Value, pub roles: Value, pub indexes: Value, pub sequences: Value }
```

**Behavior rules (R9):** for each file, strip trailing whitespace and `;`, run `SELECT coalesce(json_agg(q), '[]')::text FROM (<sql>) q` (versions: `SELECT row_to_json(q)::text FROM (<sql>) q`) with `client.simple_query`, parse the text as JSON. `assemble`:
- every object key ending in `!` loses the `!`;
- `columns[].class_kind`: `r→OrdinaryTable, v→View, m→MaterializedView, f→ForeignTable, p→PartitionedTable`;
- `tables[].table_kind`: `r→Ordinary, v→View, m→MaterializedView, p→Partitioned`; `replica_identity`: `DEFAULT→Default, INDEX→Index, FULL→Full, NOTHING→Nothing`;
- `functions[].kind`: `f→Function, a→Aggregate, w→Window, p→Procedure`; `behavior`: `IMMUTABLE→Immutable, STABLE→Stable, VOLATILE|null→Volatile`; `args` stays `{ "args": [...] }` (wrap the array when the SQL returns a bare array);
- `types[]`: `enums` → `{ "values": [...] }`, `attributes` → `{ "attrs": [...] }` (null → empty);
- `policies[].command`: `SELECT→Select, INSERT→Insert, UPDATE→Update, DELETE→Delete, ALL→All`; `is_permissive`: `"PERMISSIVE"→true` else false;
- `triggers[]`: decode `details_bitmask` into `affected` (bit 1 → `Row` else `Statement`), `timing` (64 → `Instead`, else 2 → `Before`, else `After`), `events` (4 Insert, 8 Delete, 16 Update, 32 Truncate, in that order) and drop the bitmask;
- the result: `{ schemas, tables, functions, types, version, columns, policies, extensions, triggers, roles, indexes, sequences }`. Any field mapping not listed passes through unchanged.
- Read each `.sql` before coding: if a file returns a column whose mapping differs from this list (the pin came from the struct definitions at `crates/pgls_schema_cache/src/*.rs`), follow the Rust `From`/mapping code at the tag and note the difference in the report.

**Tests:**
- unit `assemble_maps_codes_and_wraps_lists` → a hand-written `RawParts` (one row per kind) equals `tests/fixtures/pgls_schema_0.26.0.json`.
- unit `trigger_bitmask_decodes` → 1|2|4|16 → Row, Before, [Insert, Update]; 64|8 → Statement, Instead, [Delete].
- unit `bang_suffix_is_stripped_everywhere`.
- unit `pgls_sql_files_are_embedded` → all 12 `include_str!`s non-empty and contain no `$1`.
- live (ignored, in `tests/sql_run_live.rs`'s neighbour `tests/sql_schema_live.rs`): `introspection_runs_and_fits` → against a scratch database with one table, an enum, a function, a policy and a trigger: every top-level key present, the table appears, size < `MAX_SCHEMA_BYTES`; **print the byte size** (the coordinator records it in the Status: `functions.sql` covers `pg_catalog`).

**Verification:** `cargo nextest run -p orchestrator schema`; clippy; live by the coordinator.

---

### Task 6: Routes: run, cancel, schema; grant RPC; audit

**Files:**
- Implement: `services/orchestrator/src/sql_editor/routes.rs`
- Modify: `crates/cheladb-nats/src/lib.rs` (subject + request + subject test), `services/orchestrator/src/lib.rs` (`JobPort::grant_data_role_to_owner` + `NatsPorts` impl), `src/openapi.rs` (`Doc` stub), `tests/api.rs` (`FakeJobs` + route tests), `services/worker/src/main.rs` (`serve_grant_data_role_to_owner`, spawned next to `serve_check_data_role`), `services/worker/src/data_api.rs` (`Worker::grant_data_role_to_owner_reply` + unit tests with the compute fakes), `crates/cheladb-routes/src/lib.rs` (a run/cancel/schema block + test)

**Interfaces:**
```rust
// cheladb-nats
pub const SUBJECT_GRANT_DATA_ROLE_TO_OWNER: &str = "cheladb.rpc.grant_data_role_to_owner";
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct GrantDataRoleRequest { pub branch_id: Uuid, pub role: String }
// reply: the existing CheckDataRoleReply (ok | refused(msg) | not_ready | failed(msg))

// orchestrator JobPort
fn grant_data_role_to_owner(&self, branch: BranchId, role: &str) -> impl Future<Output = Result<CheckDataRoleReply, ResetError>> + Send;

// worker
impl Worker { pub async fn grant_data_role_to_owner_reply(&self, req: &GrantDataRoleRequest) -> CheckDataRoleReply; }
```
- Worker: `role` must be `anon` or `authenticated` (else `refused`); `branch_connection(branch)` must be ready (else `not_ready`); the owner name is the branch's owner role (the same source `check_data_role_reply` uses for the main database's owner); runs `GRANT <quote_ident(role)> TO <quote_ident(owner)> WITH INHERIT FALSE, SET TRUE` via `admin_sql` in the main database; a missing role (`role "anon" does not exist`) → `refused("the data API isn't set up on this branch")`. Deadline 10 s like `CHECK_ROLE_DEADLINE`.

**Routes (tag `sql-editor`; `cheladb-routes` entries `RUN_SQL` `orchestrator.run_sql` POST `/v1/databases/{id}/branches/{branch_id}/sql`, `CANCEL_SQL_RUN` `orchestrator.cancel_sql_run` POST `…/sql/runs/{run_id}/cancel`, `GET_SQL_SCHEMA` `orchestrator.get_sql_schema` GET `…/sql/schema`; all `key_endpoint`; test `sql_editor_routes_are_management_auth_with_studio_cors`):**
- `runSql` body (`#[serde(deny_unknown_fields)]`, ToSchema): `{ run_id: Uuid, database: Option<String>, role: Option<String>, sql: String, timeout_ms: Option<u64>, confirm: Option<bool> }`. Documented response: 200 `application/x-ndjson` (describe the events in the description; a `RunEvent` schema is not required), and the 4xx/5xx `ErrorEnvelope`s from the spec's table.
- Order: `load_owned` → `load_branch` → backup branch 409 / not-ready 409 (as console) → validate body (422) → `pick_database` including non-data-API databases but refusing `System` (404 when unknown) → role resolution: `None` = owner; else it must be in `list_roles(endpoint)` (any origin/level) or be `anon`/`authenticated` with the database's `data_api_bootstrapped`; names in a reserved set (`cloud_admin`, `neon_superuser`, `authenticator`, `cheladb_admin`, anything starting `pg_`) → 422; unknown → 404 `not_found` ("role not found") → guard: `guard::check(sql, branch.protected)` unless `confirm == Some(true)`; non-empty → 409 `sql_confirm_required` `.with_extra("statements", …)` and an audit line with outcome `confirm_required` → for `anon`/`authenticated`: grant cache miss → `js.grant_data_role_to_owner` (reply mapping as `check_role`: refused → 403 `role_refused`, not_ready → 409 `branch_not_ready`, timeout → 504, transport → 503 `worker_unavailable`) → `runs.reserve` (409 `run_id_in_use`, 429 `sql_run_limit` with `retry_after(2)`) → `owner_target` → `run::connect` (Unreachable → 503 `unavailable` "the branch's compute can't be reached right now; try again"; Rejected → 409 `owner_password_unknown`, since a SQL-changed password is the only cause) → 200 with `Body::from_stream(run(...).map(|e| Ok::<_, Infallible>(e.to_line())))`, headers `content-type: application/x-ndjson`, `cache-control: no-store`, `x-accel-buffering: no`.
- Audit: wrap the stream to observe events; when `Done` passes, log at `info` (target `cheladb::sql_editor::audit`): `user`, `database_id`, `branch_id`, `pg_database`, `role` (or `"owner"`), `run_id`, `statements` (count of `Complete`), `tags` (first 50, joined `,`), `duration_ms`, `outcome`, `sql_sha256` (hex), `protected`, `confirmed`. If the stream is dropped before `Done`, log with outcome `cancelled` from the wrapper's `Drop`.
- `cancelSqlRun`: `load_owned` + `load_branch`, then `runs.cancel(principal.sub, run_id)` → 202 `{}` or 404 `not_found` ("run not found").
- `getSqlSchema` (`?database=`): same preamble and database pick; `owner_target` → `run::connect` → `schema::introspect(&client)` → 200 `application/json` (the string as the body) or 422 `schema_too_large`.
- `PGLS_VERSION` test: `pgls_version_matches_studio` reads `../../apps/studio/package.json` (relative to `CARGO_MANIFEST_DIR`) and asserts `dependencies["@postgres-language-server/wasm"] == PGLS_VERSION`.

**Tests (fakes, `tests/api.rs`):** `run_validation_is_422` (bounds, unknown fields), `run_unknown_database_or_role_is_404`, `run_reserved_role_is_422`, `risky_statement_needs_confirm` (409 body lists `DropStmt`, reason `risky`; no connection attempted: `FakeLocator.asked` stays 0), `protected_branch_holds_inserts`, `confirm_skips_the_guard`, `anon_is_granted_once_then_cached` (FakeJobs counts 1 call over two runs), `grant_refusal_is_403`, `fifth_run_is_429` (hold four via the registry directly), `duplicate_run_id_is_409`, `cancel_unknown_run_is_404`, `owner_password_unknown_is_409` (no sealed password), `sql_confirm_required_is_audited_without_sql` (Logs capture: no SQL text, has `sql_sha256`), `every_declared_orchestrator_endpoint_is_routed` still passes. Worker unit tests: `grant_refuses_other_roles`, `grant_runs_the_grant_sql` (exact SQL string), `grant_on_missing_role_is_refused`. Live (`tests/api.rs`, ignored, on `ScratchCompute`): `run_streams_events_over_http` (reqwest streaming: `columns`, `rows`, `complete`, `done` in order), `closing_the_stream_cancels_the_run`, `schema_route_returns_pgls_json`.

**Verification:** `cargo nextest run -p orchestrator -p worker -p cheladb-nats -p cheladb-routes`; clippy for those; live by the coordinator.

---

### Coordinator step after wave 3: generated code

- [ ] `just api-gen` (blesses `openapi/*.json`, regenerates the management client and Studio's login client, scalar sync), then `just check`. Commit `sql-editor: regenerate API clients`. Push.

---

### Task 7: Studio stream client and NDJSON reducer

**Files:** `packages/management-client/src/fetch.ts` (+ its test file next to existing ones), `apps/studio/src/lib/sql-run.ts`, `apps/studio/src/lib/sql-run.test.ts`.

**Interfaces (produced):**
```ts
// management-client fetch.ts
/** POSTs `body` as JSON and returns the response body stream; throws ApiError (via readApiError) on non-2xx, calls onUnauthorized on 401. */
export async function streamRequest(path: string, body: unknown, signal: AbortSignal): Promise<ReadableStream<Uint8Array>>;

// studio lib/sql-run.ts
export type RunEvent =
  | { t: "notice"; severity: string; code: string; message: string; detail: string | null; hint: string | null; position: number | null }
  | { t: "columns"; statement: number; columns: { name: string; type: string }[] }
  | { t: "rows"; statement: number; rows: (string | null)[][] }
  | { t: "complete"; statement: number; tag: string; rows: number; truncated: boolean }
  | { t: "error"; statement: number; code: string; message: string; detail: string | null; hint: string | null; position: number | null }
  | { t: "heartbeat" }
  | { t: "done"; outcome: "ok" | "error" | "cancelled" | "timeout" | "truncated_response"; duration_ms: number };
export type StatementResult = { index: number; columns: { name: string; type: string }[]; rows: (string | null)[][]; tag?: string; rowCount?: number; truncated: boolean };
export type RunState = { running: boolean; statements: StatementResult[]; notices: Extract<RunEvent, { t: "notice" }>[]; error?: Extract<RunEvent, { t: "error" }>; done?: Extract<RunEvent, { t: "done" }> };
export const initialRunState: RunState; // running: true, empty lists
export function runReducer(state: RunState, event: RunEvent): RunState;
export async function* readNdjson(stream: ReadableStream<Uint8Array>): AsyncGenerator<RunEvent>;
export type RunBody = { run_id: string; database: string | null; role: string | null; sql: string; timeout_ms: number; confirm: boolean };
export function startRun(databaseId: string, branchId: string, body: RunBody, signal: AbortSignal): Promise<AsyncGenerator<RunEvent>>;
/** Postgres's 1-based code-point position in the sent text → 0-based UTF-16 index in the editor document. */
export function errorOffset(sent: string, position: number, sentFrom: number): number;
/** True when any tag is DDL that should refresh the schema (CREATE, ALTER, DROP, COMMENT, GRANT, REVOKE). */
export function changesSchema(tags: string[]): boolean;
```

**Behavior rules:** `readNdjson` decodes with `TextDecoder` (`stream: true`), splits on `\n`, skips empty lines and `heartbeat`s are yielded (the reducer ignores them), throws on invalid JSON with the line. The reducer: `columns` creates a `StatementResult` for its index, `rows` appends, `complete` sets tag/rowCount/truncated (creating the result when no `columns` came: DDL), `notice` appends, `error` sets `error`, `done` sets `done` and `running: false`. Unknown `t` values are ignored (forward compatible).

**Tests:** `streamRequest_throws_ApiError_on_409` (fakeFetch; `code` and `extra.statements` kept), `streamRequest_calls_onUnauthorized_on_401`, `readNdjson_handles_lines_split_across_chunks` (a line split mid-UTF-8 sequence too), `reducer_builds_statements_in_order`, `complete_without_columns_creates_a_ddl_result`, `error_then_done_stops_running`, `unknown_events_are_ignored`, `positions_count_code_points` (`errorOffset("SELECT '😀', nope", 15, 0)` points at `nope` in UTF-16), `selection_offset_is_added`, `changesSchema_detects_ddl_only` (`CREATE TABLE` true, `INSERT 0 1` false, `TRUNCATE TABLE` false).

**Verification:** `pnpm --filter @cheladb/management-client test typecheck`; `pnpm --filter @cheladb/studio test typecheck`.

---

### Task 8: pgls worker and CodeMirror adapter

**Files:** `apps/studio/package.json` (deps, exact pins), `pnpm-lock.yaml`, `apps/studio/vite.config.ts`, `apps/studio/src/lib/pgls/{worker.ts,client.ts,offsets.ts,codemirror.ts}` and tests `offsets.test.ts`, `codemirror.test.ts`, `client.test.ts`.

**Interfaces (produced):**
```ts
// offsets.ts
export function utf16ToByte(text: string, index: number): number;
export function byteToUtf16(text: string, byte: number): number;

// client.ts — a promise RPC over the module worker
export type PglsCompletion = { label: string; kind: "table" | "column" | "function" | "schema" | "policy" | "role" | "keyword"; detail?: string };
export type PglsDiagnostic = { start: number; end: number; message: string; severity: "error" | "warning" | "info" | "hint" }; // UTF-16 offsets after mapping
export interface PglsClient {
  ready: Promise<void>;                 // rejects when the wasm fails to load
  setSchema(json: string): Promise<void>;
  update(text: string): Promise<void>;  // insertFile("/query.sql", text)
  complete(offset: number): Promise<PglsCompletion[]>;   // UTF-16 offset in, mapped inside
  lint(): Promise<PglsDiagnostic[]>;
  hover(offset: number): Promise<string | null>;
  dispose(): void;
}
export function createPglsClient(): PglsClient; // new Worker(new URL("./worker.ts", import.meta.url), { type: "module" })

// codemirror.ts
export function sqlExtensions(client: PglsClient | null): Extension[]; // lang-sql PostgreSQL highlighting always; pgls completion + lint + hover when client is non-null, else lang-sql keyword completion
```

**Behavior rules:**
- The worker imports `createWorkspace` from `@postgres-language-server/wasm/workspace`, creates it once, and handles messages `{ id, op: "setSchema" | "update" | "complete" | "lint" | "hover", … }`, replying `{ id, ok, value | error }`. Offsets cross the worker boundary as UTF-8 bytes; the client converts using the last text it sent.
- Completion source: replace range is the word at the cursor (`/[\w$]*/` back from the cursor, plus a `schema.` prefix rule: after a `.` complete from the dot), `options` map `kind` to CodeMirror types (`table→"class"`, `column→"property"`, `function→"function"`, `schema→"namespace"`, `keyword→"keyword"`, others `"variable"`), `detail` passes through.
- Lint: debounce 300 ms after edits (`@codemirror/lint` `linter(..., { delay: 300 })`); severity `hint` maps to `info`.
- Hover: `hoverTooltip` with the markdown shown as plain preformatted text (no markdown renderer dependency).
- Vite: `worker: { format: "es" }`, `optimizeDeps: { exclude: ["@postgres-language-server/wasm"] }`, `build.target: "esnext"` if top-level await requires it. The `.wasm` must be emitted as an asset (check `dist/` after `build`).

**Tests:** `offsets_round_trip_multibyte` (ASCII, `é`, `😀`, CJK; every UTF-16 boundary round-trips), `completion_replaces_the_word_at_the_cursor` (fake client), `completion_after_a_dot_starts_at_the_dot`, `lint_maps_byte_ranges_to_utf16` (fake worker answering byte offsets on multibyte text), `client_rejects_ready_when_the_worker_errors`, `null_client_uses_keyword_completion`. A build check: `pnpm --filter @cheladb/studio build` succeeds and `dist/assets` contains a `.wasm` file and a worker chunk (assert in the report; not a unit test).

**Gotchas:** the wasm is 12.3 MB; it must load only when the SQL view opens (dynamic import of the client module from T9's component). jsdom has no `Worker`: unit tests inject a fake worker into the client (export a `createPglsClientWith(worker: Worker-like)` for tests). Don't import the package from code that Studio's main bundle includes.

**Verification:** `pnpm --filter @cheladb/studio test typecheck build`.

---

### Task 9: Studio SQL view

**Files:**
- Create: `apps/studio/src/components/sql-editor/{sql-editor.tsx, sql-toolbar.tsx, sql-results.tsx, sql-confirm-dialog.tsx, saved-queries-panel.tsx, run-history.ts}` and tests `sql-editor.test.tsx`, `run-history.test.ts`, `saved-queries-panel.test.tsx`
- Create: `apps/studio/src/components/owner-password-notice.tsx` (extracted from `tables-view.tsx`; `tables-view.tsx` uses it, its tests stay green)
- Modify: `apps/studio/src/components/project-page.tsx` (view union gains `"sql"`, a "SQL" button, renders `<SqlEditor databaseId=…>` lazily via `React.lazy`)

**Interfaces (consumed):** T7's `startRun`, `runReducer`, `readNdjson`, `errorOffset`, `changesSchema`; T8's `createPglsClient`, `sqlExtensions`; generated hooks after api-gen: `useListBranches`, `useListBranchDatabases`, `useListRoles`, `useGetSqlSchema`, `cancelSqlRun`, `useListSavedQueries`, `useCreateSavedQuery`, `useGetSavedQuery`, `useUpdateSavedQuery`, `useDeleteSavedQuery` (exact names from `packages/management-client/src/generated/endpoints` after the coordinator's api-gen).

**Behavior rules (the spec's Part 5 with R3/R4):**
- Branch `<select>` (normal, ready branches), Database (all non-`system` branch databases, main first), Run as (Owner, then `list_roles` minus the owner, plus `anon`/`authenticated` when the database's `data_api_bootstrapped`), Timeout (60 s / 5 min / 15 min, stored per browser), Run (Ctrl/Cmd+Enter; selection or buffer), Cancel (calls `cancelSqlRun` and aborts the fetch), Refresh schema.
- Each run gets `crypto.randomUUID()`; a new run aborts the previous fetch.
- 409 `sql_confirm_required` → `SqlConfirmDialog` lists `statements` (kind, preview, reason); Confirm re-sends with `confirm: true` and a new `run_id`.
- Banners and notices use the Global Constraints copy verbatim; `owner_password_unknown` shows `OwnerPasswordNotice`.
- Results: a tab per statement with columns (label = tag), a Messages tab; `NULL` rendered as a muted `NULL` badge; truncation banner; status line with outcome and duration; error underline via a CodeMirror decoration at `errorOffset(...)` and a "Go to error" button.
- Schema: `useGetSqlSchema` per (branch, database), `staleTime: Infinity`; refetch when `changesSchema(tags)` after a run, on database change, and on Refresh; on `schema_too_large` or worker failure show "Basic completion" and pass `null` to `sqlExtensions`.
- Saved panel: list (name, updated time), search, open (loads buffer, remembers id + version, applies hints with fallback note), Save (PATCH with version), Save as (POST), rename, delete with confirm, conflict dialog ("Overwrite" re-PATCHes with the current version from `extra.current`; "Load saved version" replaces the buffer). The save dialog shows the "private to you… Don't save secrets." line.
- History (`run-history.ts`): `localStorage` key `cheladb.sql.history.<userSub>.<databaseId>`, newest first, max 100, entries `{ sql, branchId, database, role, at, outcome, durationMs }`; buffer key `cheladb.sql.buffer.<userSub>.<databaseId>`; timeout key `cheladb.sql.timeout`. Every access in try/catch; works with storage throwing.

**Tests:** `run_streams_results_into_tabs` (fake fetch streaming NDJSON), `confirm_dialog_resends_with_confirm`, `cancel_calls_the_route_and_aborts`, `error_position_is_underlined`, `owner_banner_and_role_banner_copy`, `protected_banner_shows_on_protected_branches`, `anon_listed_only_when_bootstrapped`, `history_keeps_100_newest`, `history_survives_storage_throwing`, `saved_conflict_offers_overwrite_and_load`, `schema_refetches_after_ddl`, `basic_completion_note_on_schema_too_large`, plus `tables-view` tests still pass with the extracted notice.

**Verification:** `pnpm --filter @cheladb/studio test typecheck build`; regenerate `routeTree.gen.ts` only if routes change (they shouldn't: R4).

---

### Task 10: e2e, README, roadmap

**Files:** `scripts/sql-editor-e2e.sh`, `justfile` (recipe `sql-editor-e2e: _live-check`, with a comment, next to `console-e2e`), `README.md` (a `## SQL editor` section between "App users" and "Neon"; Prerequisites gain clang and protoc), `docs/roadmap/2026-09-28-neon-and-data-api.md` (6d progress line).

**e2e (`scripts/sql-editor-e2e.sh`, modelled on `scripts/console-e2e.sh`, sourcing `scripts/lib/neon-common.sh`):** create a database (cleanup trap unless `KEEP=1`), wait ready, then through the gateway with `curl -sN` and a helper `run_sql <json>` that saves the NDJSON and asserts with `jq -s`:
1. `CREATE TABLE notes(id int primary key, body text); INSERT …; SELECT …` as owner → `complete` tags `CREATE TABLE`, `INSERT 0 2`, `SELECT 2`, `done.outcome == "ok"`.
2. Enable RLS with a policy, then `SELECT count(*) FROM notes` as `anon` → the policy's count (and the grant RPC ran: second anon run is fast).
3. `DO $$ BEGIN RAISE NOTICE 'hello'; END $$` → a `notice` event.
4. `SELECT nope` → `error` with `position`.
5. `SELECT pg_sleep(30)` in the background; `POST …/sql/runs/<id>/cancel` → 202; the stream ends `cancelled`.
6. `SELECT pg_sleep(75)` with `timeout_ms: 120000` → the stream stays open past 60 s (heartbeats) and ends `ok` (proves gateway pass-through).
7. `DROP TABLE notes` → 409 `sql_confirm_required`; again with `confirm: true` → ok.
8. `PATCH …/branches/<main>` `{protected: true}`; `INSERT …` → 409 (reason protected); unprotect.
9. Saved queries: create, list, get, PATCH with a stale version → 409, delete.
10. `GET …/sql/schema` → JSON with `tables` and `functions`; print its size.

**README section content:** what the editor is, execution semantics (one simple query; implicit transaction; `BEGIN/COMMIT` honoured; `VACUUM`/`CREATE INDEX CONCURRENTLY`/`CREATE DATABASE` only alone), limits, the guard (risky vs protected, `confirm`, the known limit), Run as (preview, not a security boundary; `anon`/`authenticated` granted to the owner with INHERIT FALSE on first use), saved queries and history, pgls attribution (MIT) and the upgrade note, and `just live sql-editor-e2e`.

**Verification:** `bash -n scripts/sql-editor-e2e.sh`; the run itself is in wave 5.

---

### Wave 5: live verification (coordinator)

- [ ] Separate metadata DB exports in place (6b's trick), then, one at a time: `just live cargo nextest run -p orchestrator --run-ignored ignored-only -E 'test(/sql|schema|run_streams|closing_the_stream|owner_console/)'`, `just live cargo test -p cheladb-metadata --test live_db -- --ignored --test-threads=1`, `just live sql-editor-e2e`, then the existing `just live console-e2e` and `just live data-e2e` (6c's paths touched by `owner_target`).
- [ ] Playwright pass (sonnet subagent, Playwright MCP, `just live` held ≤ 15 min per chunk): open SQL view, completion shows a table from pgls, run a multi-statement script, result tabs, an error underline, Cancel on `pg_sleep`, confirm dialog on DROP, saved query save/reopen, history entry, anon banner.
- [ ] Full checks: `just test`, `just check`, clippy for touched crates. Record counts in the Status.

### End-of-step review

- [ ] Whole-branch review (opus) against the spec and this plan; fix batch (Critical/Important inside, minors batched); re-run affected checks; revert the separate-metadata-DB exports; Status + roadmap "Resume here"; pause for the user's merge decision.
