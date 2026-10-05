# Step 6e: data API pipelining, implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task by task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Measure whether pipelining the data API's compute round trips over tokio-postgres clears the user's bar (Phase 1), and switch only if it does (Phase 2).

**Architecture:** A standalone harness binary (`tools/bench-data-api`) runs the real planner's SQL through four execution variants against a throwaway database on the dev stack, with `tc netem` adding RTT on the compute's traffic to the harness only. The results go to the user at a pause. Phase 2's detailed tasks are written after that pause, for the shape the user picks.

**Tech stack:** Rust 2024; sqlx 0.9.0 (variants A and B); tokio-postgres from the fork `chelabase/rust-postgres` rev `d20c5b3cd15c8fa649757768a788428788844612` (crate version 0.7.18) with `deadpool-postgres` 0.14.2 (variants C and D); clap; `nicolaka/netshoot:v0.16` pinned by index digest `sha256:b09d9b21381f47a79b3cbcb30da25266dc17186ea00ae65e99fdc51396f48e70` (latest on Docker Hub, 2026-10-04).

**Spec:** `docs/superpowers/specs/2026-10-04-data-api-pipelining-design.md` (decisions D1–D7). Read it before your task.

**Worktree:** `<repo root>/.worktrees/data-api-pipelining`, branch `data-api-pipelining`. Parallel implementers use `just wt-add data-api-pipelining-tN data-api-pipelining` and work only in their own worktree, with absolute paths, `git -C <wt>` and `just --justfile <wt>/justfile --working-directory <wt>`.

## Status

- **DONE: merged into `main` on 2026-10-04 (`--no-ff`, the user's go).** The merged `main` builds and passes `cargo test --workspace`.
- **End-of-step review (opus):** "with fixes", with no data-integrity or tenant-isolation defects. One fix wave (`f768126..dced3c8`), re-reviewed clean:
  - **I1:** `authenticator` gets USAGE on the exposed default schema, so non-`public` schemas pipeline too instead of silently falling back. The health check (`schema_check_sql(schema)`) brings existing branches along. Prepares use `join_all` (no leaked named statements), debug logs say which path ran, and the `pipelined_runs` counter backs the live test `a_non_public_default_schema_runs_pipelined`.
  - **I2:** the per-connection statement cache is capped at `STATEMENT_CACHE_CAP` = 500, then cleared.
  - **M1:** a doc fix.
  - **M2:** a dropped app_users `PendingTx` is discarded rather than returned to the pool.
  - **M5:** owner pools use `RecyclingMethod::Fast`.
  - **M6:** the pin text in `Cargo.toml` and `CLAUDE.md` is corrected.
  - Verification after the wave: workspace tests, clippy, `rest_live` 36/36, `engine_live`, both `app_users_live` suites, the orchestrator's ignored tests, the bench's `--tests-only`, and data/console/app-users e2e all green.
- **Accepted as notes (end review):**
  - M3: the owner `current_user` check runs after `COMMIT`; practically unreachable.
  - M4: `octet_length` counts server-encoding bytes and Rust counts UTF-8 bytes; Chelabase databases are UTF-8.
  - M7: the reaper counts idle time from checkout, not return.
  - M8: a `max_connections` change rebuilds the pool rather than calling `resize()`, as before the branch.
  - The cache cap can overshoot by one request's prepares.
  - The app_users pool's statement cache isn't capped; not verified whether app_users uses cached prepares.
  - Reconcile repairing a non-`public` branch wasn't watched live, because no such branch was ready. It's covered by a reconcile unit test and the live test.
- **P8 done** (`c6cbbbe`, `dc6874f`, `4d410d3`): variant R, rest's real `Engine`, is within +1.7% to +4.0% of C's p50 at 0.5 and 2 ms for every op (bar ±10%), and the table is appended to the results doc. `cargo test --workspace` and clippy `-D warnings` are green. The worker's `app_users_live` passed 13/13 and `roles_live` 30/30. Every P7 live suite and the five e2e runs pass again in one pass. No Studio changes. The user approved the Phase 2 plan on 2026-10-04.
- **P3b done** (`279873a`, clean): the pin is `6001659`, and `ROLLBACK` is sent only when `transaction_status()` isn't `Idle` (pipeline, `with_transaction`, rest's owner check, `app_users`).
- **P7 done** (`8fb420f`): six new live tests (stale version, mid-batch failure, oversized write, forged counter, session state, missing guard functions) plus the health-check repair of both functions. All live suites pass: `rest_live` 35/35, `engine_live` 7/7, `app_users_live` 6/6, the orchestrator's ignored tests 6/6, the bench's `--tests-only` 6/6. The data, console, app-users, sql-editor and neon e2e runs pass. The compute log has 0 `no transaction in progress` warnings, and reconcile re-bootstrapped each existing branch exactly once.
- **Wave B done, all merged; the workspace builds:**
  - **P6** (`6ae1749`, clean).
  - **P4** (`fda8377..3499d88`), one fix round: the owner-level check now runs after a reset with `pg_catalog.pg_has_role`, and pending opens count toward the branch cap.
  - **P3** (`b72182c..87580ed`), one fix round:
    - `begin_sql` is `ROLLBACK; BEGIN; <reset>`, so a failing reset aborts instead of autocommitting.
    - A trailing reset runs after `COMMIT`, so a later prepare never parses on a dirty session.
    - The backstop counts NULL as SQL does.
    - Parameter counts are checked before sending.
    - Then the merge step: `chelabase_query::Lease` owns the pooled object and discards it unless the call finished cleanly (covers cancel, timeout and panic). The console passes `RunAs::Owner { user }`.
- **F1 done:** `chelabase/rust-postgres` `chelabase/transaction-status` `60016594a76606abf661581a48b4d1d42ed29d2d`, which adds `Client::transaction_status()` with in-flight tracking (`Unknown` while requests are outstanding). The fork's suite passes (116), and the opus review approved it.
- **Wave A done:** P1 (`01303c1..fbddaa5`, one fix round: `with_transaction(client, reset_sql, request_id, f)` sends `ROLLBACK; <reset>; BEGIN`), P2 (`cd065b4`, clean), P5 (`51eae8a..384608b`, one fix round: the plan's `pg_catalog.nullif` doesn't exist, so it's bare `nullif`). All merged.
- **Wave B rulings (R-B1..R-B4):** callers pass a pooled connection as `&*object`; P3 owns the `RunAs::Owner { user }` change and updates the console call sites after P4 and P6 merge; P4 restores the bench build; each task owns its own files.
- **Phase 2 rulings during execution:** P2's `param_types()` plus `capped_param_types()` (budget last) are accepted; every request starts with `ROLLBACK; <reset>; BEGIN` (P1 review), so the compute logs one `WARNING: there is no transaction in progress` per request. **User's decision (2026-10-04): patch the driver fork instead of filtering logs.** **F1** adds `Client::transaction_status()` to `chelabase/rust-postgres` (new branch `chelabase/transaction-status` on `d20c5b3`; it records the ReadyForQuery status byte). **P3b**, after P3 merges, bumps the pin and sends `ROLLBACK` only when the status isn't idle, so the warning (and the extra statement) disappears in the normal case; deadpool `Timeout`/`Closed` map to the old 503 (P4). Pre-flight scan (2026-10-04): deadpool 0.13.1 (under deadpool-postgres 0.14.2) has `Pool::retain(FnMut(&T, Metrics) -> bool)`, `status()`, `resize()`, `close()` and `Metrics::last_used() -> Duration`; deadpool-postgres has `RecyclingMethod::Custom(String)` and `ClientWrapper::prepare_typed_cached`; tokio-postgres's `DbError` has no public constructor (hence Q8); the worker's `SCHEMA_CHECK_SQL` ignores functions (hence Q1); the workspace tokio-postgres pin has only `runtime` (P1 adds `with-serde_json-1` and `with-uuid-1`). **User's decision (2026-10-04): C** (1-RTT pipeline), after seeing the results against sqlx. Phase 1 was done before that decision. Results: `docs/benchmarks/2026-10-04-data-api-pipelining.md` (harness `be4aa94`). D1 verdict: **C PASS, D PASS, B FAIL** (B misses 30% on Insert and Batch). C vs D rule: C's p50 is 45.5% (ByKey) and 39.8% (List100) lower than D's at 0.5 ms, so the rule recommends **C**. At 0 ms throughput, D edges C on List100, Insert and Batch (C pays for the plpgsql guard and cap calls). Plan approved by the user 2026-10-04.
- **T5 (done):** wiring `f9876b7`; `--seed-only`/`--tests-only` `3c2927b`; authenticator `USAGE`/`EXECUTE` on the bench functions `fbb4470` (C prepares before the role switch; **Phase 2: the `chelabase` guard functions need `EXECUTE` for `authenticator` too**); pools closed after throughput `05bc3af` (the compute ran out of slots). Quick run green, 6/6 live tests, five fairness checks pass (no variant on TLS, `RESET ALL` clears state, guard works, byte-identical results, no netem left). Harness review (opus): with fixes; I1 (throughput: 3 rotated rounds, warm-up, a fresh pool per cell, `VACUUM` per round), M1–M5 and M7 fixed in `b136a92..be4aa94` and re-reviewed clean. Full run 2,462 s, green.
- **P11:** inserts use `n = -1` (the plan said `0`), so inserted rows never fall into List100's key range. Cost if wrong: none.
- **P12:** netem isn't calibrated. The results doc gives the measured round trip (0.21 / 0.83 / 2.43 ms at the 0 / 0.5 / 2 ms settings), so the "0.5 ms" verdict was judged at about 0.8 ms. Cost if wrong: the 0.5 ms judgment slightly favors pipelining.
- **P13:** the recipe's `--tests-only` seeds the fixture, then runs `variants_live --ignored --test-threads=1` inside the recipe's cleanup trap.
- **Done:** spec (`053e3bd`); plan (`a82909a`); T1 (`a82909a..489c6ab`, review clean); T2 (`cd29f78..e4f6f28`, one fix round); T3 (`cd29f78..6e7d55a`, one fix round); T4 (`56a38db`, no review of its own). All merged into `data-api-pipelining` (42 unit tests green).
- **P8 (T2 review):** the plan's `cap_response` wrap (`… FROM (<plan.sql>) AS q(r)`) is invalid for the planner's write plans (`WITH r AS (INSERT …)` must stay top level). The bench splices instead: `WITH r AS (<dml>), q(r) AS (SELECT <tail>) SELECT bench.cap_response(q.r, $N) FROM q`, using a quote-aware paren scan, with forms cached per plan set outside the measured path. **Phase 2 (if C is chosen) needs a planner-level hook in `chelabase-query` instead of text splicing.** Cost if wrong: C's insert/batch numbers fail loudly in T5.
- **P9 (T3 review):** T3's first RED was produced after its implementation. Remedy: the fix round's new code went strict RED→GREEN, and mutation checks (verdict threshold, interleave order, guard drop) proved the existing tests catch breakage. Cost if wrong: weaker evidence for those tests.
- **P10 (T3 review):** throughput workers are spawned tokio tasks (`throughput(variant: Arc<E>, op, concurrency, secs, next: Arc<dyn Fn(OpKind) -> BenchOp + Send + Sync>)`), so a single client task can't cap the faster variants; latency loops repeat-outer, op-inner, per the brief.
- **Checks for T5's live run:** whether A/B use TLS (`Prefer`) while C/D use `NoTls` (check `pg_stat_ssl` for `bench-%` and equalize if needed); `RESET ALL` clears custom settings; the `SECURITY DEFINER` guard reads `chelabase.schema_version`.
- **P6 (pre-flight, execution):** the crate is lib + bin. T1 declared all nine modules in `src/lib.rs`, with stubs for later tasks, so parallel tasks fill only their own files and `tests/` can import the crate. Cost if wrong: a module move.
- **P7 (pre-flight, execution):** T1 wrote the shared types (`OpKind`, `ALL_OPS`, `BenchOp`, `VariantKind`, `Exec`) into `workload.rs` and `variants.rs`, so T3 compiles against them while T2 runs. Cost if wrong: a small T1 scope increase.
- **Rulings:**
  - **P1:** Phase 2's detailed tasks are written after the decision pause, for the chosen shape (C or D) only. The outline below pins its files and order. Cost if wrong: one more plan pause.
  - **P2:** the guard functions for the bench live in a `bench` schema that the harness creates as the owner, not in `chelabase`: the owner has no `CREATE` on `chelabase`. Same bodies as the spec. Cost if wrong: none for the measurement.
  - **P3:** the harness, not the recipe, applies and removes netem between RTT settings, because it owns the RTT loop. The recipe's `EXIT` trap removes the qdisc again as a backstop.
- **P4 (pre-flight):** the C guard must be `SECURITY DEFINER`. It runs after the preamble's `set_config('role', …)`, and the API roles can't read `chelabase.schema_version`; today's preamble reads it in an initplan, before the switch. Phase 2's `chelabase.assert_schema_version` gets the same. Cost if wrong: C fails with 42501 on every request.
- **P5 (pre-flight):** the harness and the owner connect straight to the compute's published port (the `ConnInfo` address, as rest does), not through the proxy. Cost if wrong: none for the measurement, since rest doesn't use the proxy either.
- **Pre-flight scan (2026-10-04), checked against the code:** `chelabase_query::{plan, sql::{Plan, resolve_bind, BindValue}, PREAMBLE_SQL}` are already public; `rest::conn::{InternalSource, ConnInfo, RESET_SQL, STATEMENT_CACHE, ACQUIRE_TIMEOUT, CONNECTION_IDLE}` are public (`rest` has a lib target); `begin_with` accepts multi-statement simple queries; the fork's tokio-postgres is 0.7.18 with `query_typed`, matching deadpool-postgres 0.14.2's `tokio-postgres` 0.7 requirement; sqlx 0.9's `test_before_acquire` defaults to true; netshoot `v0.16` is the latest tag.
- **Deferred minors:**
  - T1: `Cargo.lock` churn from the re-resolve (`windows-sys` 0.61.2 and `getrandom` 0.4.3 in unrelated crates).
  - T2: an encode error in C can still let `COMMIT` go out (Phase 2: check `stmt.params()` against the values before building the pipeline); D can return a connection still in a transaction if its ROLLBACK fails (detach the object); C counts `octet_length` while A/B/D count `len + 1` per result; `variants.rs` is ~1,000 lines (split A/B from C/D); `join_in_order`'s test wasn't RED-first; C and D build their pipelines with duplicated code.
  - T3: `next()` is drawn per variant per request, so the variants read different keys; a weak `contains("A")` assert; no success-path tests for `neon_ref`, `write_outputs` or `compute_image`; NaN when A's baseline is zero; a blocking `docker` command inside async code; a tight window in the throughput timing test; a failing throughput worker doesn't abort the others (`JoinSet::abort_all`); `throughput_uses_parallel_tasks` needs 2–3 free cores.
  - T4: passing `--results-md` through the recipe duplicates the flag; shellcheck isn't installed.
  - T5: after the first RTT step, Ctrl-C is swallowed (`tokio::signal::ctrl_c` replaces SIGINT for the whole process).
- **Running:** nothing.

## Global constraints

- Metadata access stays on sqlx in both phases.
- Never mirror PostgREST or Supabase.
- Live runs only through `just live …`; implementer worktrees never run the stack.
- Never `cargo fmt` the workspace; rustfmt only files you create (`rustfmt --edition 2024 <file>`).
- One simple command per Bash call; `Write` for files, not heredocs.
- Commit as `git commit`, ending with the `Co-Authored-By` and `Claude-Session` trailers; push after every commit.
- Strict TDD: show a RED run before code, then GREEN.
- Results docs never contain local hardware details (CPU, RAM, threads).
- The bar (D1): a variant passes if, against A, p50 and p99 drop by ≥30% at both 0.5 ms and 2 ms RTT for every op, and its 0 ms throughput is ≥95% of A's for every op.

## Review focus

1. **Pipeline order:** in C and D, a statement that isn't prepared yet must never let `COMMIT` reach the server before the plan query. Prepare everything (preamble and plans, via `prepare_cached`) *before* building the pipeline. Test `c_pipeline_prepares_before_sending` (T2).
2. **Fairness of A:** A must use rest's real pool options (ping on acquire, `after_release` `RESET_SQL`, `STATEMENT_CACHE`, `ACQUIRE_TIMEOUT`, `CONNECTION_IDLE`), taken from `rest::conn`'s constants, not copied numbers. Test `variant_a_uses_rest_pool_options` (T2).
3. **Literal escaping in B:** claims JSON with `'`, `\`, and non-ASCII must round-trip through `set_config` unchanged. Test `quote_literal_*` (T1) and the live check `b_claims_round_trip` (T2).
4. **netem left behind:** a failing or interrupted run must not leave the qdisc on the compute. Tests: `netem_del_cmd_*` (T1); the recipe's trap and the `--quick` check (T4, T5).
5. **Equal answers:** all four variants must return byte-identical JSON for the same op and values, or the timing comparison is meaningless. Test `variants_agree` (T2) and the `--quick` run (T5).

---

## Phase 1: the benchmark

### Dependencies and waves

```
Wave 1:  T1 (crate, workspace deps, pure modules)
Wave 2:  T2 (variants + workload)  ||  T3 (runner + report)  ||  T4 (recipe)
Wave 3:  T5 (quick live run, harness review, full run, results doc) -> PAUSE
```

T2, T3 and T4 touch disjoint files and run in parallel worktrees (`-t2`, `-t3`, `-t4`), each merged back with `--no-ff`.

### Models

| Task | Implementer | Reviewer |
|---|---|---|
| T1 crate + pure modules | sonnet | sonnet (later tasks build on it) |
| T2 variants + workload | opus (pipelining order is subtle) | opus |
| T3 runner + report | sonnet | sonnet |
| T4 recipe | sonnet | no own review (e2e-gated; covered by T5 and the end review) |
| T5 live runs + results | opus (live debugging) | harness review before the full run: opus |

### Task 1: the crate and its pure modules

**Files:**
- Modify: `Cargo.toml` (workspace): add member `tools/bench-data-api`; add `deadpool-postgres = { version = "0.14.2", default-features = false, features = ["rt_tokio_1"] }` to `[workspace.dependencies]`; add `[patch.crates-io] tokio-postgres = { git = "https://github.com/chelabase/rust-postgres", rev = "d20c5b3cd15c8fa649757768a788428788844612" }`, plus `postgres-types` and `postgres-protocol` from the same repo and rev **only if** `cargo tree -d -i tokio-postgres` shows a duplicate without them.
- Create: `tools/bench-data-api/Cargo.toml` (package `bench-data-api`, `publish = false`, bin `bench-data-api`; deps: `chelabase-query`, `rest` (path `../../services/rest`), `sqlx`, `tokio-postgres`, `deadpool-postgres`, `tokio`, `clap`, `anyhow`, `serde`, `serde_json`, `rand`, `futures-util`, `uuid`, all `workspace = true` where the workspace has them).
- Create: `tools/bench-data-api/src/main.rs` (clap args, `mod` lines; `main` may just parse args and print them in this task).
- Create: `tools/bench-data-api/src/stats.rs`, `src/literal.rs`, `src/netem.rs`.

**Interfaces (produced):**

```rust
// stats.rs: latencies in microseconds
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
pub struct Summary { pub n: usize, pub p50_us: f64, pub p95_us: f64, pub p99_us: f64 }
pub fn summarize(samples_us: &mut [u64]) -> Summary;      // nearest-rank percentiles; panics on empty
pub fn median_of(runs: &[Summary]) -> Summary;            // field-wise median; n = sum of n

// literal.rs
pub fn quote_literal(s: &str) -> anyhow::Result<String>;  // '…' with ' doubled; Err on NUL

// netem.rs: argv vectors for std::process::Command (no shell)
pub const NETSHOOT: &str = "nicolaka/netshoot:v0.16@sha256:b09d9b21381f47a79b3cbcb30da25266dc17186ea00ae65e99fdc51396f48e70";
pub fn add_cmds(container: &str, rtt_us: u32, dst: std::net::IpAddr) -> Vec<Vec<String>>;
pub fn del_cmd(container: &str) -> Vec<String>;
```

`add_cmds` returns, in order, `docker run --rm --net container:<c> --cap-add NET_ADMIN <NETSHOOT> tc …` argv for:
1. `tc qdisc add dev eth0 root handle 1: prio bands 3 priomap 1 1 1 1 1 1 1 1 1 1 1 1 1 1 1 1` (everything to band 2 by default; check `tc-prio(8)`: the 16-entry priomap maps every priority to band 1, i.e. class `1:2`);
2. `tc qdisc add dev eth0 parent 1:3 handle 30: netem delay <rtt_us>us`;
3. `tc filter add dev eth0 parent 1:0 protocol ip prio 1 u32 match ip dst <ip>/32 flowid 1:3` (IPv6: `protocol ipv6`, `match ip6 dst <ip>/128`).

`del_cmd` is the same `docker run` prefix with `tc qdisc del dev eth0 root`. `rtt_us == 0` makes `add_cmds` return an empty list.

**CLI (pinned, used by T4):** `bench-data-api --owner-url <url> --branch <uuid> --internal-url <url> --internal-token <tok> --compute-container <name> [--rtts-ms 0,0.5,2] [--quick] [--out target/bench-data-api] [--results-md <path>]`. `--internal-url` and `--internal-token` also read `CHELABASE_INTERNAL_URL` and `CHELABASE_REST_INTERNAL_TOKEN`.

**Tests to write first (unit, in each module):**
- `summarize_nearest_rank`: 1..=100 gives p50 50, p95 95, p99 99; one sample gives that value for all.
- `median_of_three_runs`: field-wise median; `n` summed.
- `quote_literal_doubles_quotes`: `it's` gives `'it''s'`.
- `quote_literal_keeps_backslashes_and_unicode`: `a\b ü {"x":"'"}` round-trips as `'a\b ü {"x":"''"}'` (standard_conforming_strings is on).
- `quote_literal_refuses_nul`.
- `netem_add_cmds_ipv4`: exact argv for container `c1`, 500 µs, `172.17.0.1`.
- `netem_add_cmds_ipv6`: the filter uses `protocol ipv6` and `ip6 dst …/128`.
- `netem_zero_rtt_adds_nothing`.
- `netem_del_cmd_removes_root`.

**Verify:** `cargo test -p bench-data-api`; `cargo clippy -p bench-data-api -- -D warnings`; `cargo tree -d -i tokio-postgres` shows one version.

### Task 2: the workload and the four variants

**Files:**
- Create: `tools/bench-data-api/src/workload.rs`, `src/variants.rs`, `src/setup.rs`.
- Test: `tools/bench-data-api/tests/variants_live.rs` (`#[ignore]`, run only through `just live`).

**Interfaces:**
- Consumes (T1): `literal::quote_literal`.
- Uses from `chelabase_query`: `parse`, `Op`, `plan`, `sql::{Plan, resolve_bind, BindValue}`, `schema_for` / `introspect` / `role_view`, `engine::PREAMBLE_SQL`. Read `crates/chelabase-query/src/query.rs` and its tests for the request tree grammar (select with filter, order and limit; insert; batch).
- Uses from `rest::conn`: `InternalSource`, `ConnSource`, `ConnInfo`, `RESET_SQL`, `STATEMENT_CACHE`, `ACQUIRE_TIMEOUT`, `CONNECTION_IDLE`.
- Produces (used by T3):

```rust
// workload.rs
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize)]
pub enum OpKind { ByKey, List100, Insert, Batch }
pub const ALL_OPS: [OpKind; 4];
pub struct BenchOp { pub kind: OpKind, pub op: chelabase_query::Op, pub plans: std::sync::Arc<Vec<chelabase_query::sql::Plan>> }
pub struct Workload { /* plans per OpKind, built once from the role's view */ }
impl Workload {
    pub async fn build(owner: &mut sqlx::PgConnection, default_schema: &str) -> anyhow::Result<Workload>;
    pub fn next(&self, kind: OpKind, rng: &mut impl rand::Rng) -> BenchOp;   // fresh random values, same plans
}

// variants.rs
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize)]
pub enum VariantKind { A, B, C, D }
pub trait Exec: Send + Sync {
    fn kind(&self) -> VariantKind;
    fn exec(&self, op: &BenchOp) -> impl std::future::Future<Output = anyhow::Result<String>> + Send;
}
pub enum AnyVariant { A(VariantA), B(VariantB), C(VariantC), D(VariantD) }   // implements Exec by delegating
pub struct Variants { pub all: Vec<AnyVariant> }                               // in order A, B, C, D
impl Variants {
    pub fn c(&self) -> &VariantC;                                              // for the sanity ping
    pub async fn ping(&self) -> anyhow::Result<std::time::Duration>;           // one `SELECT 1` on C's pool, timed
    pub async fn connect(info: &rest::conn::ConnInfo, pool_size: u32, claims: &serde_json::Value,
                         expected_version: uuid::Uuid, max_rows: i64, timeout_ms: i64) -> anyhow::Result<Variants>;
}

// setup.rs
pub async fn seed(owner: &mut sqlx::PgConnection) -> anyhow::Result<uuid::Uuid>;  // returns the schema version after setup
pub async fn client_addr(info: &rest::conn::ConnInfo) -> anyhow::Result<std::net::IpAddr>;
```

**Behavior rules:**
- **Every variant** runs as role `authenticated` with the fixed claims `{"sub":"00000000-0000-0000-0000-000000000001","role":"authenticated"}`, `statement_timeout`/`transaction_timeout` = `info.settings.statement_timeout_ms`, and binds through `resolve_bind(op, bind, max_rows)`. Each returns the plan's text result, joined as `chelabase_query::run` does: `[r1,r2,…]` for a batch, else the single result.
- **A:** an sqlx `PgPool` built exactly like `rest::conn`'s (`ssl_mode(Prefer)`, `application_name("bench-a")`, `statement_cache_capacity(STATEMENT_CACHE)`, `idle_timeout(CONNECTION_IDLE)`, `acquire_timeout(ACQUIRE_TIMEOUT)`, default `test_before_acquire`, `after_release` running `RESET_SQL`). Per request: `begin()`, `PREAMBLE_SQL` (read the version column), each plan (`query_scalar` + binds), `commit()`.
- **B:** an sqlx pool with `test_before_acquire(false)` and no `after_release`, same statement cache. Per request: `begin_with(format!("SET ROLE NONE; RESET ALL; BEGIN; SELECT set_config('role', {r}, true), set_config('request.jwt.claims', {c}, true), set_config('statement_timeout', {t}, true), set_config('transaction_timeout', {t}, true)"))`, with every value through `quote_literal`; then each plan; then `commit()`. Pre-flight: `begin_with` takes an `impl SqlSafeStr` (wrap the `String` in `sqlx::AssertSqlSafe`) and queues it as one simple query, so several statements are fine; it refuses only inside an existing transaction.
- **C:** a `deadpool_postgres::Pool` (`NoTls`, `RecyclingMethod::Fast`, `application_name` `bench-c`). Per request:
  1. prepare the preamble and every plan with `client.prepare_cached` (`try_join_all`; no I/O when cached);
  2. then build the pipeline futures **in order** and await them with `futures_util::future::join_all` or `tokio::join!` (both poll in order on the first poll, and tokio-postgres writes a request on its first poll): `batch_execute("SET ROLE NONE; RESET ALL; BEGIN")`; the preamble, `SELECT …same four set_config…, bench.assert_schema_version($4)`; each plan as `query_one(&stmt, &binds)` with its outer result wrapped as `bench.cap_response(<plan sql>, $budget)`; then `batch_execute("COMMIT")`;
  3. the first error wins; later `25P02` errors are ignored.
  Never `tokio::spawn` the pieces, and never use `FuturesUnordered`.
- **Wrapping a plan for `cap_response`:** `SELECT bench.cap_response(q.r, $N) FROM (<plan.sql>) AS q(r)` where `$N` is the next parameter index (`i64`, `chelabase_query::engine::DEFAULT_MAX_RESPONSE_BYTES`). Check that every plan returns exactly one `text` column (it does today: `run_plan` reads `Option<String>`).
- **D:** the same pool kind as C (its own pool, `application_name` `bench-d`). The same pipeline without `COMMIT` and without `cap_response`. The preamble returns `(SELECT version FROM chelabase.schema_version)` instead of asserting. After the joined futures resolve, compare the version with `expected_version`, then `batch_execute("COMMIT")`, or `ROLLBACK` and an error on a mismatch.
- **`seed`** (as the owner):
  - `CREATE TABLE bench_items (id bigint PRIMARY KEY, owner uuid NOT NULL, title text NOT NULL, n int NOT NULL, created_at timestamptz NOT NULL DEFAULT now())`, 100,000 rows from `generate_series` (`n = i % 1000`), `CREATE INDEX ON bench_items (n)`, `ANALYZE`;
  - `CREATE SCHEMA bench`, the two functions below, `GRANT USAGE ON SCHEMA bench TO authenticated`, and `GRANT EXECUTE` on both to `authenticated`;
  - then return `SELECT version FROM chelabase.schema_version` (read **after** all DDL, since DDL bumps it).
- **Op values:**
  - `ByKey`: a random `id` in `1..=100000`;
  - `List100`: `n = k`, a random `k` in `0..1000`, ordered by `id`, limit 100;
  - `Insert`: `id` from an atomic counter starting at `1_000_000 + rand(0..1e9)` (no collisions across runs), random `owner`, title `"bench"`, `n = 0`;
  - `Batch`: ByKey, then List100, then Insert, in one batch op.
- **`client_addr`:** connect as the authenticator and run `SELECT inet_client_addr()`. Error if NULL (a Unix socket).

```sql
-- setup.rs: the bench copies of the spec's functions (P2)
-- SECURITY DEFINER: the preamble calls it after set_config('role', 'authenticated'), and
-- authenticated has no SELECT on chelabase.schema_version (today's preamble reads it in an
-- initplan, which runs before the role switch, as authenticator).
CREATE FUNCTION bench.assert_schema_version(expected uuid) RETURNS uuid
LANGUAGE plpgsql STABLE SECURITY DEFINER SET search_path = '' AS $f$
DECLARE v uuid;
BEGIN
  SELECT version INTO v FROM chelabase.schema_version;
  IF v IS DISTINCT FROM expected THEN
    RAISE EXCEPTION USING ERRCODE = 'CB001', MESSAGE = 'chelabase: stale schema version';
  END IF;
  RETURN v;
END $f$;

CREATE FUNCTION bench.cap_response(result text, budget bigint) RETURNS text
LANGUAGE plpgsql VOLATILE SET search_path = '' AS $f$
DECLARE total bigint;
BEGIN
  total := coalesce(nullif(pg_catalog.current_setting('chelabase.response_bytes', true), ''), '0')::bigint
           + coalesce(pg_catalog.octet_length(result), 0);
  IF total > budget THEN
    RAISE EXCEPTION USING ERRCODE = 'CB002', MESSAGE = 'chelabase: response too large';
  END IF;
  PERFORM pg_catalog.set_config('chelabase.response_bytes', total::text, true);
  RETURN result;
END $f$;
```

**Tests:**
- Unit (`variants.rs`): `variant_a_uses_rest_pool_options` asserts the A pool's options (`get_max_connections`, `get_idle_timeout`, `get_acquire_timeout`, `get_test_before_acquire() == true`) equal the `rest::conn` constants. `b_begin_statement_quotes_every_value` checks the built `begin_with` string for a role and claims containing `'`. `cap_wrap_uses_next_param_index`: a plan with 3 binds wraps with `$4`.
- Unit (`workload.rs`): `insert_ids_never_repeat` (10,000 `next(Insert)` calls give distinct ids); `batch_has_three_ops_in_order`.
- Live (`tests/variants_live.rs`, `#[ignore]`; reads `BENCH_OWNER_URL`, `BENCH_BRANCH`, `CHELABASE_INTERNAL_URL`, `CHELABASE_REST_INTERNAL_TOKEN`):
  - `variants_agree`: for 20 random ops of each kind, generated with one seeded RNG per variant so values match, all four return identical strings (except `Insert`, whose ids differ: compare with the id field masked);
  - `b_claims_round_trip`: B with claims containing `'`, `\` and `ü`; `current_setting('request.jwt.claims')` (run as an extra statement inside B's transaction in a test-only helper) equals the input;
  - `c_pipeline_prepares_before_sending`: on a fresh C pool (empty statement cache), the first `Insert` commits the row (count +1), proving `COMMIT` didn't overtake an unprepared query;
  - `c_stale_version_rolls_back`: C built with a wrong `expected_version` fails with SQLSTATE `CB001`, and an `Insert` leaves no row;
  - `d_stale_version_rolls_back`: the same for D;
  - `session_state_doesnt_leak`: run `SELECT set_config('x.y','1',false)` through a C connection's pipeline, then the next request on that connection sees `current_setting('x.y', true)` empty. Same for B.

**Verify:** `cargo test -p bench-data-api` (unit); clippy clean. The live tests run in T5.

### Task 3: runner and report

**Files:**
- Create: `tools/bench-data-api/src/runner.rs`, `src/report.rs`.
- Create: `tools/bench-data-api/src/rtt.rs`: the RTT loop as a function generic over `Exec`, plus the netem cleanup guard. Its signature is below. `main.rs` calls it in T5, because `main.rs`'s wiring needs T2's concrete types.

**Interfaces:**
- Consumes: T1 `stats`, `netem`; T2 `Exec`, `VariantKind`, `OpKind`, `ALL_OPS`, `BenchOp`, `Workload`, `Variants`, `setup::{seed, client_addr}`. Until T2 merges, build against a test double implementing `Exec` (sleeps a set duration and returns `"x"`).
- Produces:

```rust
// runner.rs
pub struct LatencyPlan { pub warmup: usize, pub measured: usize, pub repeats: usize }   // full: 200/2000/3; quick: 10/50/1
pub async fn latency<E: Exec>(variants: &[&E], ops: &[OpKind], plan: &LatencyPlan,
    next: impl FnMut(OpKind) -> BenchOp) -> anyhow::Result<Vec<Cell>>;
pub async fn throughput<E: Exec>(variant: &E, op: OpKind, concurrency: usize, secs: u64,
    next: impl Fn(OpKind) -> BenchOp + Sync) -> anyhow::Result<f64>;            // requests/s
#[derive(serde::Serialize)]
pub struct Cell { pub variant: VariantKind, pub op: OpKind, pub summary: stats::Summary }

// rtt.rs: one RTT step. Applies netem (unless rtt 0), runs the sanity check with `ping`
// (a closure timing one `SELECT 1`), runs `latency`, and removes netem on every exit path.
pub async fn at_rtt<E: Exec>(container: &str, dst: std::net::IpAddr, rtt_ms: f64, sanity_n: usize,
    ping: impl FnMut() -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<std::time::Duration>> + Send>>,
    run: impl std::future::Future<Output = anyhow::Result<Vec<Cell>>>) -> anyhow::Result<Vec<Cell>>;
pub struct NetemGuard { /* runs netem::del_cmd on Drop */ }

// report.rs
#[derive(serde::Serialize)]
pub struct Results { pub setup: Setup, pub latency: Vec<(f64 /* rtt ms */, Vec<Cell>)>, pub throughput: Vec<(VariantKind, OpKind, f64)> }
#[derive(serde::Serialize)]
pub struct Setup { pub neon_ref: String, pub compute_image: String, pub netshoot: String, pub pool_size: u32, pub plan: String }
pub fn verdict(r: &Results) -> Vec<(VariantKind, bool, Vec<String> /* reasons it failed */)>;   // D1, against A
pub fn markdown(r: &Results) -> String;
```

**Behavior rules:**
- **Interleaving:** for each repeat, for each op, run the variants round-robin, one request each, until every variant has `warmup + measured` requests. Only the measured ones are timed. Any error aborts the run with the variant, op and error.
- **Timing:** `Instant` around `exec` only; microseconds.
- **Throughput:** `concurrency` tasks, each looping `exec` until the deadline; total completed / elapsed. Run at RTT 0 only, concurrency 32, 20 s (quick: 2 s), pool size 32.
- **The RTT loop (`rtt.rs`; `main.rs` drives it in T5):**
  - for each RTT, apply `netem::add_cmds` (with the address from `client_addr`);
  - run the sanity check: 200 `SELECT 1` through the C pool (quick: 20); require a p50 of at least the RTT, or abort;
  - run `latency`, then always run `del_cmd`;
  - a drop guard and a Ctrl-C handler also run `del_cmd`.
  - A failed `docker` command aborts with its stderr.
- **`verdict`:** D1 exactly as written in Global constraints. B, C and D are each judged against A. An RTT missing from the results is a failing reason, not a pass.
- **`markdown`:** a setup block, one table per RTT (rows: ops; columns: each variant's p50/p95/p99 in ms with 2 decimals), a throughput table, and the verdict list. No hardware details. `Setup.neon_ref` comes from `infra/neon/NEON_REF` (or wherever `just` reads it; check `infra/neon/README.md`), and `compute_image` from `docker inspect --format '{{.Config.Image}}' <container>`.
- **Output:** write `<out>/results-<unix ts>.json`, and when `--results-md` is set, write the markdown there.

**Tests (unit, with the test double):**
- `interleaves_variants_round_robin`: the double records the call order; for 2 variants and 3 requests it's A B A B A B per op.
- `warmup_is_not_measured`: `Summary.n == measured`.
- `latency_aborts_on_error`: a double that fails on call 5 makes `latency` return `Err` naming the variant and op.
- `throughput_counts_completions`: a 10 ms double at concurrency 4 for 1 s gives 300–420 req/s.
- `verdict_passes_at_exactly_30_and_95`, `verdict_fails_on_p99_only`, `verdict_fails_on_missing_rtt`, `verdict_fails_on_throughput_94`.
- `markdown_has_no_hardware_words`: the output contains none of `CPU`, `cores`, `threads`, `RAM`, `GiB`.

**Verify:** `cargo test -p bench-data-api`; clippy clean.

### Task 4: the recipe

**Files:**
- Create: `scripts/bench-data-api.sh` (executable).
- Modify: `justfile`: add next to `data-e2e`:

  ```
  # Data API round-trip benchmark (step 6e): four execution variants under netem RTTs; results in docs/benchmarks (needs just dev)
  bench-data-api *args: _live-check
      scripts/bench-data-api.sh {{args}}
  ```

**Behavior rules:**
- Copy `scripts/data-e2e.sh`'s helpers (`api`, `ok`, `wait_db`, `reveal`, `die`, `pass`, `hex`, demo.env sourcing, the proxy CA). Don't source that script.
- Create the database `bench_<hex 2>`, wait until ready, and `reveal` (owner password and database name).
- **The compute:** `curl` the internal listener's `/internal/data/branches/<db_id>/connection` (bearer `CHELABASE_REST_INTERNAL_TOKEN`, default `chelabase-dev-rest-internal`; URL `CHELABASE_INTERNAL_URL`, default `http://127.0.0.1:8083`) and take `.host` and `.port`. That's the compute's published Postgres port (`chelabase_neon::compute::published_postgres_addr`). The container is `docker ps --filter label=chelabase.dev/endpoint-id --filter publish=<port> --format '{{.Names}}'` (named `chelabase-compute-<endpoint>`); die unless exactly one matches.
- **`--owner-url`:** `postgresql://postgres:<pw>@<host>:<port>/<db>`, straight to the compute (pre-flight: the same address rest uses, no proxy; URL-encode the password with `jq -rn --arg p "$bpw" '$p|@uri'`).
- `cargo build -q --release -p bench-data-api`, then run it with all the args. Pass `--results-md docs/benchmarks/$(date +%F)-data-api-pipelining.md` unless `--quick` is among the args.
- **`trap cleanup EXIT`:** `docker run --rm --net container:<c> --cap-add NET_ADMIN <NETSHOOT> tc qdisc del dev eth0 root` (ignore "No such file"), then delete the database (unless `KEEP=1`).
- With `--quick`, after the run, check `docker run … tc qdisc show dev eth0` contains no `netem`, or fail.

**Verify (offline):** `bash -n scripts/bench-data-api.sh`; `shellcheck scripts/bench-data-api.sh` if installed; `just --list` shows the recipe. The live run is T5.

### Task 5: live runs, harness review, results

**Steps:**
- [ ] Merge T2, T3 and T4 into `data-api-pipelining` (`--no-ff`), and remove their worktrees.
- [ ] Wire `main.rs` (TDD where it has logic): fetch `ConnInfo` through `InternalSource`, then `seed`, `client_addr`, `Workload::build`, `Variants::connect` (pool 32), then `rtt::at_rtt` for each `--rtts-ms` value with `runner::latency` over `variants.all` (`AnyVariant`, so one `E`), then `runner::throughput` at RTT 0 for each variant and op, then `report`. Ctrl-C (`tokio::signal::ctrl_c`) drops the `NetemGuard`.
- [ ] `just live just bench-data-api --quick`. Fix what fails; harness bugs go back to the owning task's area, with RED/GREEN as usual.
- [ ] Live tests: `just live cargo test -p bench-data-api --test variants_live -- --ignored` (with the env from a `KEEP=1` quick run's database, or a mode of the recipe that runs them: add `--tests-only` to the recipe if that is simpler, and note it as a ruling).
- [ ] **Harness review (opus)** of the whole Phase 1 diff, focused on fairness (Review focus 1–5): does any variant do more or less work than its definition? Fix Critical/Important findings before the full run.
- [ ] `just live just bench-data-api`: the full run. Commit `docs/benchmarks/<date>-data-api-pipelining.md`.
- [ ] Update this Status (results summary, verdict, recommendation per the spec's "C vs D" rule) and the roadmap's "Resume here"; commit; push.
- [ ] **PAUSE:** bring the user the table, the verdict and the recommendation.

---

## Phase 2: the switch to shape C (user's decision, 2026-10-04)

Every compute-facing path in `chelabase-query` (rest's data requests and schema endpoints, the orchestrator's owner console, `app_users`) moves to tokio-postgres + deadpool-postgres; data requests run as one pipeline (shape C). Metadata stays on sqlx; live tests keep sqlx for their own admin sessions (dev-dependency).

### Phase 2 rulings (made while planning; listed for the user at the plan pause)

- **Q1, functions reach existing branches:** the worker's health check (`SCHEMA_CHECK_SQL`) doesn't look at functions, so a bootstrap change never reaches healthy branches. Extend it to require `chelabase.assert_schema_version(uuid)` and `chelabase.cap_response(text, bigint)`; reconcile then re-bootstraps each branch once. Cost if wrong: one extra bootstrap per branch.
- **Q2, grants:** `EXECUTE` on both functions goes to `PUBLIC`, like the claim helpers. `cap_response` runs after the switch to whatever role a key uses (`anon`, `authenticated`, custom roles), and `assert_schema_version` runs before it as `authenticator`. `chelabase` is a reserved schema, so neither is exposed as an RPC; `assert_schema_version` is `SECURITY DEFINER` (P4) and reveals only whether a guessed uuid matches. Cost if wrong: a role can call two harmless functions.
- **Q3, the cap can't be lifted by user code:** an RPC can `set_config('chelabase.response_bytes', …)` to forge the cumulative counter. So `cap_response` also refuses any single result over the budget on its own (that check doesn't use the counter), and Rust re-counts what it reads and fails the request past the budget. Forging the counter can then only make the user's own request fail after its writes committed. Cost if wrong: a forged counter wastes at most `plans × budget` bytes of rest memory per request.
- **Q4, one fallback path:** the spec's rollout fallback ("use D for a database missing the functions") becomes the **sequential path** that already exists for cold starts and stale-version retries. That's one code path instead of two, slower only until reconcile installs the functions. Cost if wrong: rollout-window requests take today's ~4 RTT.
- **Q5, the cap belongs to the planner:** `Plan` gains `capped_sql` (built once at plan time, so cached with the plan): write plans become `WITH r AS (<dml>), q(r) AS (SELECT <tail>) SELECT chelabase.cap_response(q.r, $N) FROM q`, and every other plan `SELECT chelabase.cap_response(q.r, $N) FROM (<sql>) AS q(r)`. The planner builds this structurally; the bench's text splice (P8) stays bench-only. Cost if wrong: none at run time.
- **Q6, bind types:** tokio-postgres lets the server infer untyped parameters (`CAST($n AS bigint)` would become `int8` while we bind text), so every statement is prepared with explicit types: text, jsonb, int8, as sqlx sends. `Plan::param_types()` gives them as a crate-local `ParamType` enum (no driver type in `sql.rs`).
- **Q7, the owner-level check** moves out of the request's transaction: on a cache miss it's one plain query before the pipeline (rest already caches it per branch and role).
- **Q8, errors stay testable:** tokio-postgres's `DbError` has no public constructor, so the mapping takes a small trait `DbFields { code, message, detail, hint, constraint }` implemented for `tokio_postgres::error::DbError` and for a test fake (replacing sqlx's `FakeDb`).
- **Q9, `run` owns its transaction:** `chelabase_query::run` sends `BEGIN`/`COMMIT` itself (callers stop opening transactions), so callers are written once and the pipeline can include both.

### Global constraints (Phase 2)
- Every Phase 1 global constraint still applies.
- tokio-postgres from the fork (`d20c5b3…`, already patched for crates.io); deadpool-postgres 0.14.2 (check crates.io for a newer release first); `NoTls` (SQL editor ruling R6).
- The HTTP responses of the data API, the console and the schema endpoints don't change: same statuses, codes, messages and bodies. The existing tests that pin them must pass unchanged, apart from the type they're built from.
- `RESET_SQL` / `OWNER_RESET_SQL` content is unchanged. It moves from after-release to the front of each request (rest), or stays a recycle step where there's no pipeline (console schema, app_users).

### Waves and models

```
Wave A:  P1 (query crate: driver, errors, schema, sequential run)  ||  P2 (planner: capped_sql, param types)  ||  P5 (worker bootstrap)
Wave B:  P3 (pipelined run)  ||  P4 (rest pools + exec)  ||  P6 (console + app_users)
Wave C:  P7 (live tests + live debugging)
Wave D:  P8 (rest-path bench variant, full verification) -> end-of-step review (opus) -> fixes -> MERGE PAUSE
```

| Task | Implementer | Reviewer |
|---|---|---|
| P1 query crate core | opus | opus |
| P2 planner hook | sonnet | sonnet |
| P3 pipelined run | opus | opus |
| P4 rest pools + exec | opus | opus |
| P5 worker bootstrap | sonnet | sonnet |
| P6 console + app_users | sonnet | sonnet |
| P7 live tests | opus | no review of its own (end-of-step review) |
| P8 bench variant + verification | sonnet | no review of its own (end-of-step review) |
| End of step | | opus, whole branch |

P1 ∥ P2 ∥ P5 and P3 ∥ P4 ∥ P6 touch disjoint files (P1 owns `chelabase-query/Cargo.toml`, `lib.rs`, `error.rs`, `schema.rs`, `engine.rs`; P2 only `sql.rs` and `tests/sql.rs`; P3 only `engine.rs`; P4 only `services/rest/src/*`; P6 only `console.rs` and `app_users.rs`), each in a `-tN` worktree, merged `--no-ff`.

### P1: query crate on tokio-postgres (sequential)

**Files:** `crates/chelabase-query/{Cargo.toml, src/lib.rs, src/error.rs, src/schema.rs, src/engine.rs}`; `tools/bench-data-api/src/workload.rs` (introspect callers move to a tokio-postgres client); the workspace `Cargo.toml` only if a dependency is missing.

**Interfaces (produced):**
```rust
// error.rs
pub trait DbFields { fn code(&self) -> &str; fn message(&self) -> &str; fn detail(&self) -> Option<&str>; fn hint(&self) -> Option<&str>; fn constraint(&self) -> Option<&str>; }
impl DbFields for tokio_postgres::error::DbError { /* code().code() etc. */ }
impl QueryFailure {
    pub fn from_db(err: &dyn DbFields, request_id: &str) -> QueryFailure;          // was from_database
    pub fn from_pg(err: &tokio_postgres::Error, request_id: &str) -> QueryFailure;  // was from_sqlx
}
// schema.rs
pub enum IntrospectError { Db(#[from] tokio_postgres::Error), Decode(#[from] serde_json::Error) }
pub async fn introspect(client: &tokio_postgres::Client, version: String) -> Result<Schema, IntrospectError>;
pub async fn visibility(client: &tokio_postgres::Client, role: &str) -> Result<Visibility, IntrospectError>;
pub async fn role_view(client: &tokio_postgres::Client, schema: &Schema, role: &str) -> Result<Arc<RoleView>, IntrospectError>;
// engine.rs: every `conn: &mut PgConnection` becomes `client: &tokio_postgres::Client`
pub fn preamble_failure(db: &dyn DbFields, role: &str, request_id: &str) -> QueryFailure;
pub async fn run(client: &Client, cache: &QueryCache, key: (Uuid, &str), run_as: RunAs<'_>,
                 settings: &RunSettings, budget: usize, op: &Op, request_id: &str) -> Result<String, QueryFailure>;  // owns BEGIN..COMMIT (Q9)
pub async fn schema_for(client: &Client, …same…) -> Result<(Arc<Schema>, Arc<RoleView>), QueryFailure>;  // owns BEGIN..COMMIT
pub async fn load_schema(client: &Client, …same…);  // runs inside a transaction the caller of `run`/`schema_for` opened; stays pub for rest's schemas_for, which now calls `with_transaction`
pub async fn with_transaction<T>(client: &Client, request_id: &str, f: impl AsyncFnOnce(&Client) -> Result<T, QueryFailure>) -> Result<T, QueryFailure>;  // BEGIN, f, COMMIT; ROLLBACK on error (best effort)
pub(crate) async fn run_sequential(client: &Client, …) -> Result<String, QueryFailure>;  // today's logic: preamble, plan, each plan, budget; used by P3 for cold/retry/fallback
```
**Behavior rules:**
- Same mapping table as today (`classify`); non-DB errors map like `Io` did: 503 `unreachable` when `err.is_closed()` or the error has no `as_db_error()`, else 500 internal.
- Statements are prepared with explicit types: the preamble (text×3 / text×1), `INTROSPECT_SQL` (`text[]`), `VISIBILITY_SQL` (text, `text[]`), plans (via `Plan::param_types()` from P2; until P2 merges, a local helper with the same semantics, deleted at merge). Use `client.prepare_typed` and pass values in that order; `BindValue::Json` binds `serde_json::Value` as jsonb (`with-serde_json-1`).
- `run_sequential` keeps today's semantics exactly: `ResponseBudget` (`len + 1`), batch joins, `null` for NULL.
- Workspace dependency features: tokio-postgres needs `with-serde_json-1` and `with-uuid-1` for the query crate; add at workspace level if missing.
- The bench harness keeps compiling (`workload.rs` gets a tokio-postgres client for introspection; variants A/B stay on sqlx).

**Tests:** port `error.rs` tests to `FakeDb: DbFields` (same names, same assertions: `sqlstate_mapping_table`, `unavailable_bodies_are_fixed`; `a_closed_pool_is_unavailable` becomes `a_closed_connection_is_unavailable`, built from a real closed client: connect to a listener that drops the socket); `engine.rs` tests keep their names (`connection_and_role_failures_are_fixed_messages`, `schema_missing_needs_the_sqlstate_and_the_name`, …) on `FakeDb`. New: `with_transaction_rolls_back_on_error` (fake client is hard: make it an `#[ignore]` live test in `crates/chelabase-query/tests/engine_live.rs` against the metadata Postgres, like `app_users_live.rs`), `prepared_types_match_sqlx` (unit: the type list for a select with a `CAST($n AS bigint)` filter, a LIMIT and a jsonb insert is `[Text, Int8, Int8]` / `[Jsonb, Int8]`).

**Gotcha:** callers (rest, console) won't compile until P4/P6. P1 compiles `-p chelabase-query -p bench-data-api` only; the workspace build goes green at the end of Wave B.

### P2: planner hook (`capped_sql`, `param_types`)

**Files:** `crates/chelabase-query/src/sql.rs`, `crates/chelabase-query/tests/sql.rs`.

**Interfaces:**
```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)] pub enum ParamType { Text, Jsonb, Int8 }
pub struct Plan { pub sql: String, pub binds: Vec<Bind>, pub capped_sql: String }
impl Plan { pub fn param_types(&self) -> Vec<ParamType>; }  // Text(..)->Text, Json(..)->Jsonb, Limit/Offset->Int8; capped_sql has one more Int8 at the end
```
**Behavior:** `capped_sql` per Q5; the budget parameter is `$(binds.len() + 1)`. The write-plan builders (sql.rs ~:467 and ~:484) produce both forms from their parts (CTE body, tail), never by parsing `sql`. `sql` is unchanged (the sequential path uses it).
**Tests:** `capped_select_wraps_in_a_subquery`, `capped_write_keeps_the_cte_top_level` (insert, update, delete, with and without RETURNING), `capped_call_wraps` (scalar and set-returning), `capped_param_is_last`, `param_types_follow_binds`. Exact-string assertions.

### P3: the pipelined `run` (shape C)

**Files:** `crates/chelabase-query/src/engine.rs` (and a new `src/pipeline.rs` if it keeps `engine.rs` readable; declare it in `lib.rs` in the same commit).

**Interfaces:** consumes P1 (`run_sequential`, `with_transaction`, `DbFields`, `from_pg`) and P2 (`capped_sql`, `param_types`). `run`'s signature is unchanged from P1. `QueryCache` gains `versions: moka::sync::Cache<(Uuid, String), String>` (last-seen version per branch and database; capacity like `SCHEMA_CACHE`) and `no_guard: moka::sync::Cache<(Uuid, String), ()>` (databases missing the functions, TTL 60 s); `invalidate_branch` clears both.

**Behavior (`run`):**
1. No known version, or `no_guard` hit: `run_sequential` (records the version it reads).
2. Otherwise, plan against the known version's schema (`cached_schema`; a schema-cache miss means: sequential path).
3. Prepare (typed, cached per connection: `deadpool_postgres::Client::prepare_typed_cached` when the client is a deadpool client; take `&deadpool_postgres::ClientWrapper` or a small trait so the sequential path works on a plain `Client` in tests) the preamble and each plan's `capped_sql` **before** building the pipeline.
4. Pipeline, in order, polled in order with the bench's `join_in_order` pattern (port it with its test): `batch_execute("ROLLBACK; <reset_sql>; BEGIN")` (P1 ruling: clears a transaction a dropped request left open) (the reset SQL is a `RunSettings` field `reset_sql: &'static str`, so rest and the console pass their own), the preamble `… , chelabase.assert_schema_version($n)` (owner variant: no role, no claims), each capped plan with `budget` as its last parameter, `batch_execute("COMMIT")`.
5. The first real error wins, the `25P02`s after it are ignored, and the trailing `COMMIT` turns into a rollback. `CB001`: drop the cached version, run `run_sequential` once (a second mismatch there is today's schema-changed failure). `CB002`: today's 413 `too_large`. `42883` naming `chelabase.assert_schema_version` or `chelabase.cap_response`: insert into `no_guard`, run `run_sequential`.
6. Rust re-counts the results (`ResponseBudget`, Q3); past the budget it's the same 413, even though it already committed (only reachable by forging the counter).
7. Before building the pipeline, check every plan's bind values encode for its types (the T2 deferred minor). An encode error never sends anything.
8. Session state: the reset runs at the front of every request, so the pool needs no after-release step for rest.

**Tests (unit, with a fake pipeline step runner where possible; else live in P7):** `first_error_wins_and_aborted_steps_are_ignored`, `stale_version_falls_back_to_sequential_once`, `missing_guard_marks_the_database_and_runs_sequentially`, `cap_error_is_413`, `rust_backstop_refuses_past_the_budget`, `encode_error_sends_nothing`, `join_in_order_polls_in_push_order`. A test-only trait over "prepare / execute step" is acceptable if it keeps the production path the same code.

### P4: rest on deadpool

**Files:** `services/rest/{Cargo.toml, src/conn.rs, src/exec.rs, src/error.rs}`.

**Interfaces:** `Conn { pub client: deadpool_postgres::Object, pub info: Arc<ConnInfo>, _permits }`; `AcquireError::Db(DbAcquireError)` where `DbAcquireError` wraps `deadpool_postgres::PoolError` (keep `AcquireError`'s other variants); `ApiError::from_db(&dyn DbFields, rid)` and `ApiError::from_pg(&tokio_postgres::Error, rid)` replace the sqlx wrappers.

**Behavior:**
- `DbPool` wraps `deadpool_postgres::Pool`: manager from a `tokio_postgres::Config` (host, port, user, password, dbname, `application_name("chelabase-rest")`, connect timeout 5 s), `NoTls`, `RecyclingMethod::Fast`, `max_size = max_connections.max(1)`, `timeouts.wait = ACQUIRE_TIMEOUT`, statement cache via `prepare_typed_cached` (deadpool's per-connection cache; `STATEMENT_CACHE` becomes unused, so delete it and its uses).
- `PoolLoad` from `pool.status()` (`size`, `available`); `pool_at_size` from `status().size >= max_size`.
- `idle_to_close` stays pure; `make_room` closes idle connections of other pools with `pool.retain(|_, metrics| …)`, oldest `last_used` first, up to the count `idle_to_close` returns.
- Idle timeout: a reaper task per `Pools` (spawned in `Pools::new`, every 10 s) calls `retain` on every cached pool, dropping connections idle longer than `CONNECTION_IDLE`; it holds a `Weak` and ends when `Pools` drops.
- `is_connect_error` becomes `fn is_connect_error(e: &PoolError, at_size: bool) -> bool`: `Timeout(Wait)` → `!at_size`; `Timeout(Create)`, `Backend(e)` with `e.is_closed()` or no DB error, or a DB error whose code starts with `28`, `08` or `57P` → true; `Closed` → true; else false. Same retry-once behavior.
- `exec.rs`: `fetch_owner_level` runs on the client before `run` (Q7); `Engine::run` calls `chelabase_query::run(&conn.client, …)` with `reset_sql: RESET_SQL` and no `begin`/`commit` of its own; `schemas_for` uses `with_transaction` + `load_schema` + `view`.
- `Pools::invalidate` closes pools with `pool.close()`.
- **Pool-error parity (P1 review):** a deadpool `PoolError::Timeout(_)` or `PoolError::Closed` reaching a response maps to `QueryFailure::unreachable` (503, the fixed body), as sqlx's `PoolTimedOut`/`PoolClosed` did; `PoolError::Backend(e)` goes through `from_pg`. Test: `pool_errors_map_like_the_old_sqlx_ones`.
- **Every request starts clean (P1 ruling):** `with_transaction(client, reset_sql, request_id, f)` sends `ROLLBACK; <reset_sql>; BEGIN`; `schemas_for` passes `RESET_SQL`.

**Tests:** every existing `conn.rs`/`exec.rs`/`error.rs` unit test keeps its name and assertion, ported to the new types (`a_busy_pool_timing_out_is_not_a_moved_compute` becomes a table over `PoolError` variants; `pools_are_per_database_and_share_the_branch_slots` reads the database from the manager config). New: `reaper_closes_connections_idle_past_the_limit` (unit, with a paused tokio clock where deadpool allows it; else in P7 live), `make_room_closes_the_oldest_idle_first` (pure over `PoolLoad`s if `retain`'s selection is factored out). Rewrite the live test `open_connections_stay_within_the_branch_cap_live` for `status()`.

### P5: worker bootstrap

**Files:** `services/worker/src/data_api.rs` (and `app_users_sql.rs` only if the claim-helper text has to move).

**Behavior:** in `data_api_script`, after `bump_schema_version`, create the two functions (exact SQL below), `ALTER FUNCTION … OWNER TO CURRENT_USER`, `REVOKE ALL … FROM PUBLIC`, then `GRANT EXECUTE … TO PUBLIC` (Q2). Add `'assert_schema_version', 'cap_response'` to the pronames allow-list (both copies: script and the `data_api_sql_is_exact` expected text). `REMOVE_DATA_API_SQL` drops both before the schema. `SCHEMA_CHECK_SQL` also requires both to exist with the right signatures (`to_regprocedure('chelabase.assert_schema_version(uuid)') IS NOT NULL AND to_regprocedure('chelabase.cap_response(text,bigint)') IS NOT NULL`) (Q1).

```sql
CREATE OR REPLACE FUNCTION chelabase.assert_schema_version(expected uuid) RETURNS uuid
LANGUAGE plpgsql STABLE SECURITY DEFINER SET search_path = '' AS
  $f$ DECLARE v uuid; BEGIN
    SELECT version INTO v FROM chelabase.schema_version;
    IF v IS DISTINCT FROM expected THEN
      RAISE EXCEPTION USING ERRCODE = 'CB001', MESSAGE = 'chelabase: stale schema version';
    END IF;
    RETURN v; END $f$;
CREATE OR REPLACE FUNCTION chelabase.cap_response(result text, budget bigint) RETURNS text
LANGUAGE plpgsql VOLATILE SET search_path = '' AS
  $f$ DECLARE n bigint := coalesce(pg_catalog.octet_length(result), 0); total bigint; BEGIN
    IF n OPERATOR(pg_catalog.>) budget THEN
      RAISE EXCEPTION USING ERRCODE = 'CB002', MESSAGE = 'chelabase: response too large';
    END IF;
    total := coalesce(nullif(pg_catalog.current_setting('chelabase.response_bytes', true), '')::bigint, 0) OPERATOR(pg_catalog.+) n;
    IF total OPERATOR(pg_catalog.>) budget THEN
      RAISE EXCEPTION USING ERRCODE = 'CB002', MESSAGE = 'chelabase: response too large';
    END IF;
    PERFORM pg_catalog.set_config('chelabase.response_bytes', total::text, true);
    RETURN result; END $f$;
```
(Follow the file's existing quoting and operator-qualification style exactly. If `OPERATOR(pg_catalog.…)` isn't used inside plpgsql bodies there, match what `bump_schema_version` does.)

**Tests:** update `data_api_sql_is_exact`, `remove_sql_drops_only_the_machinery_of_our_schema`, `remove_data_api_sql_drops_helpers_dependents_first`, `tamper_check_allows_app_users_objects` (now also both names), `the_health_check_covers_the_login_grants_and_triggers` (now also both functions), `event_trigger_functions_are_owned_by_the_bootstrap_superuser` (pattern for ownership of the two new ones). New: `guard_functions_are_granted_to_public_and_owned_by_the_bootstrap_user`.

### P6: console + app_users

**Files:** `services/orchestrator/src/console.rs`, `crates/chelabase-query/src/app_users.rs`, `services/orchestrator/Cargo.toml` (deadpool), and the orchestrator test `api.rs` only where a type changed.

**Behavior:**
- **Console:** `pools: moka::future::Cache<OwnerPoolKey, deadpool_postgres::Pool>`; the eviction listener calls `pool.close()`. Pool: `max_size = OWNER_POOL_CONNECTIONS`, `timeouts.wait = OWNER_ACQUIRE_TIMEOUT`, `application_name = APPLICATION_NAME`, `NoTls`, `RecyclingMethod::Custom(OWNER_RESET_SQL)` (kept as a recycle step: the console's schema route has no pipeline). `run_as_owner` calls `chelabase_query::run` with `RunAs::Owner`, `reset_sql: OWNER_RESET_SQL`, no `begin`/`commit`. `owner_schema` calls `schema_for`.
- **Owner plan-cache role (spec):** the role comes from the pool's login user, not `current_user` from the preamble. `RunAs::Owner` gains `Owner { user: &str }`. The owner preamble keeps returning `current_user`; a mismatch is a 500 internal.
- **app_users:** `PgUsersDb` pools become deadpool pools (`max_size 2`, idle reaping the same way as rest's reaper or `RecyclingMethod::Fast` plus a 60 s `retain` on access); `PendingTx` holds the `Object` and runs `COMMIT`/`ROLLBACK` with `batch_execute`; `map_error` takes `&tokio_postgres::Error` via `DbFields`. Statements are typed (`$1::regclass` text, `$2::name[]` as `text[]`; insert `uuid, text, bool, text`).

**Tests:** existing names keep their assertions (`a_closed_owner_pool_is_503_with_retry_after` builds from a closed `PoolError`/`tokio_postgres` error); the `app_users_live.rs` and console live tests in `services/orchestrator/tests/api.rs` must pass unchanged in P7.

### P7: live tests and live debugging

- New tests in `services/rest/tests/rest_live.rs` (the spec's list): `stale_version_replans_and_commits_once`, `mid_batch_failure_leaves_no_rows` (keep `batch_failure_rolls_back_everything` too), `oversized_write_response_leaves_no_row`, `forged_response_counter_cannot_return_more_than_the_budget` (an RPC that sets `chelabase.response_bytes` to a large negative, then a big result: still 413), `session_state_set_by_an_rpc_does_not_reach_the_next_request` (`set_config(…, false)` and `SET ROLE`; keep `a_session_level_set_role_does_not_leak_to_the_next_request`), `a_database_without_the_guard_functions_runs_sequentially`, and in the worker: `bootstrap_installs_the_guard_functions_live` if the worker has a live suite (else covered by `the_health_check_sees_what_the_bootstrap_repairs`, extended to drop one function and see it repaired).
- Run every live suite that touches the changed paths, through `just live`: `rest_live`, `chelabase-query` `app_users_live` and `engine_live`, the orchestrator console live tests, `bench-data-api --tests-only`, `data-e2e`, `console-e2e`, `app-users-e2e`, `sql-editor-e2e`, `neon-e2e`. Fix what fails, by root cause, RED/GREEN.

### P8: rest-path benchmark and full verification

- Add a variant `R` to the harness (`tools/bench-data-api`): it calls `rest::exec::Engine` directly (built on `InternalSource`, like `rest_live.rs`), so the shipped code is measured. Run it in a short mode (`--rtts-ms 0.5,2 --variants C,R`, 2 repeats). Pass: R's p50 within ±10% of C's at both RTTs for every op, recorded in the results doc's appended section.
- The full verification run: `cargo test --workspace`, clippy `-D warnings` on the workspace, Studio unaffected (no Studio changes expected; skip unless touched), and the live list from P7 green in one pass.
- Then the end-of-step review (opus, whole branch, with the deferred minors), one fix wave, re-review, and the **merge pause**.
