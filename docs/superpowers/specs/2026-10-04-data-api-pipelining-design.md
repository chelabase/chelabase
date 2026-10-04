# Step 6e: data API on tokio-postgres (pipelining)

**Date:** 2026-10-04
**Status:** Spec, waiting for the user's review.
**Branch:** `data-api-pipelining` (`.worktrees/data-api-pipelining`)
**Roadmap:** `docs/roadmap/2026-09-28-neon-and-data-api.md`, item 6e.

## Goal

Find out, with numbers, whether the data API's compute round trips are worth cutting by pipelining over tokio-postgres. Switch only if the gain clears the bar below. Metadata stays on sqlx in every case.

## What a request costs today

A data API request on a warm cache (`services/rest/src/exec.rs::run`, `crates/chelabase-query/src/engine.rs::run`) does these round trips to the compute, in sequence:

1. sqlx's ping on acquire (`test_before_acquire` defaults to true in sqlx 0.9);
2. `BEGIN`;
3. the preamble (`PREAMBLE_SQL`: role, claims, timeouts and the schema version);
4. each plan query (one per op, several for a batch);
5. `COMMIT`;
6. after release, `SET ROLE NONE; RESET ALL` (`RESET_SQL`), which holds the connection but is off the request's critical path.

That is 5 RTT on the critical path and 6 of connection time for a single op.

## Decisions (user, 2026-10-04)

| # | Decision |
|---|---|
| D1 | **The bar:** a variant passes if, against A, p50 and p99 drop by at least 30% at both 0.5 ms and 2 ms RTT for every op, and its 0 ms throughput is at least 95% of A's for every op. |
| D2 | **Four variants:** A (today), B (every cheap sqlx tweak), C (full pipeline, 1 RTT), D (pipeline, then COMMIT after the result: 2 RTT). |
| D3 | **Latency injection:** `tc netem` in a sidecar sharing the compute's network namespace, delaying only traffic to rest's address. |
| D4 | **Workload:** four ops reported separately: select by key, list 100 rows, insert one row, a batch of 2 reads + 1 insert. |
| D5 | **Scope:** benchmark, pause for the user, then switch every compute-facing path in `chelabase-query` (rest, the orchestrator console, `app_users`) if a variant passes. Metadata stays on sqlx. |
| D6 | **Pool:** `deadpool-postgres` (0.14.2 on crates.io, 2026-10-04), with a `[patch.crates-io]` onto our tokio-postgres fork. |
| D7 | **C or D:** decided by the user at the pause, from the results. Only the chosen shape is built. |

## Phase 1: the benchmark

### Harness

- A new workspace binary, `tools/bench-data-api` (`publish = false`, not in CI).
- It builds each op through `chelabase_query::parse` and the planner, so the SQL and binds are exactly what rest sends.
- It connects as `authenticator` and runs as `authenticated` with a fixed claims JSON, through `PREAMBLE_SQL`. The owner-level check counts as a warm cache hit.
- Each variant has its own pool of the same size. Connection setup is excluded by the warm-up.

| Variant | Driver and pool | Per request |
|---|---|---|
| A | sqlx `PgPool` with rest's exact options: ping on acquire, `after_release` running `RESET_SQL`, statement cache `STATEMENT_CACHE` | ping, `BEGIN`, preamble, plan queries, `COMMIT`; reset after release |
| B | sqlx, `test_before_acquire(false)`, no `after_release`, statement cache | `begin_with("SET ROLE NONE; RESET ALL; BEGIN; SELECT set_config(…)")` with escaped literals (one simple-query round trip), plan queries, `COMMIT`. B doesn't read the schema version; if B alone passed, getting the version would be its own follow-up. |
| C | deadpool-postgres, `RecyclingMethod::Fast` | one pipeline: `batch_execute("SET ROLE NONE; RESET ALL; BEGIN")`, the preamble calling `chelabase.assert_schema_version($n)`, each plan query (`prepare_cached`) wrapped in `chelabase.cap_response`, `COMMIT` |
| D | as C | the same pipeline without `COMMIT`; await the preamble's version, then `COMMIT` |

C and D send their futures in order by creating them in order and joining them with `join!` (tokio-postgres writes a request when its future is first polled).

### Bench database and data

- A throwaway database created on the dev stack through the `chelabase` CLI, and deleted at the end of the recipe.
- The harness seeds `bench_items` with 100,000 rows: `id bigint PRIMARY KEY`, `owner uuid`, `title text`, `n int` (indexed), `created_at timestamptz`. RLS stays off (the default).
- The harness installs `chelabase.assert_schema_version(uuid)` and `chelabase.cap_response(text, bigint)` itself as the owner (the same bodies the "Pipelined execution" section specifies), so nothing changes in the bootstrap before the decision.
- Ops: select one row by a random `id`; list 100 rows where `n = k` (random `k`), ordered by `id`; insert one row; a batch of the select, the list and the insert.
- Inserts commit for real. The table grows by tens of thousands of rows across a run, which doesn't matter.

### Latency injection

- The recipe runs a sidecar: `docker run --rm --net container:<compute> --cap-add NET_ADMIN <netshoot image>`, with the image pinned by digest (latest release checked when the plan is written).
- It installs a `prio` qdisc on the compute's `eth0`, `netem delay <rtt>` on one band, and a `u32` filter steering packets whose destination is rest's address into that band. Delaying the compute's egress by the full RTT adds that RTT; storage traffic (pageserver, safekeepers) is untouched.
- **rest's address:** the harness reads `client_addr` from `pg_stat_activity` for its own backend, so the filter fits Docker Desktop and native Docker alike.
- **Sanity check:** before each RTT setting, 200 `SELECT 1` calls; the setting is refused unless the measured p50 is at least the configured RTT.
- **Teardown:** `tc qdisc del dev eth0 root` on exit, failures included (an `EXIT` trap in the recipe). The qdisc lives in the compute's namespace, so the sidecar's `--rm` doesn't remove it.

### Runs

- **Entry point:** `just bench-data-api`, always queued as `just live just bench-data-api`. `--quick` runs one RTT with 50 requests per cell.
- **Latency:** RTT 0, 0.5 and 2 ms; concurrency 1; 200 warm-up requests, then 2,000 measured per cell (variant × op). Variants run interleaved in rounds, so drift hits all of them equally. Each cell repeats 3 times and reports the median of the runs' p50, p95 and p99.
- **Throughput:** RTT 0; concurrency 32 over a 32-connection pool; 20 s per cell; requests per second.
- **Output:** raw JSON in `target/bench-data-api/` (git-ignored), and a committed results table in `docs/benchmarks/<date>-data-api-pipelining.md`. It records the setup: image digests, `NEON_REF`, RTTs, pool size and request counts, but never local hardware details.

### Decision pause

- I bring the user the results table, which variants pass D1, and a recommendation. Nothing switches on a rule alone.
- **C vs D:** if both pass, I recommend C only when it beats D's p50 by at least 20% at 0.5 ms on the read ops; otherwise D, which is simpler and needs no new database functions.
- **Only B passes:** reported. B gets its own short design if the user wants it.
- **Nothing passes:** step 6e ends. The results doc is committed, the roadmap records that sqlx stays (with the numbers), and the harness stays for later re-runs (for example once rest and computes are on different hosts).
- Committed before the pause: the harness, the recipe, the results doc, and the plan's Status naming the decision as pending.

## Phase 2: the switch (only if a variant passes)

### Driver and pool

- Workspace dependencies: `deadpool-postgres` 0.14.2 (feature `rt_tokio_1`, or whatever the latest release names it), plus a `[patch.crates-io]` that points `tokio-postgres`, and any sibling crate from the same repo that deadpool pulls in, at the fork's pinned rev. `cargo tree -d` must show exactly one `tokio-postgres`.
- **TLS:** `NoTls`, as in the SQL editor's ruling R6. Today's `PgSslMode::Prefer` already falls back to plaintext, because computes don't offer TLS. TLS to computes is a platform-mode decision.
- **`chelabase-query`:** `&mut PgConnection` in `engine.rs` and `schema.rs` becomes `&tokio_postgres::Client` (deadpool's `Object` derefs to it). `app_users::PgUsersDb` moves to deadpool pools and `client.transaction()`.
- **Errors:** `QueryFailure::from_sqlx` becomes `QueryFailure::from_pg(&tokio_postgres::Error, request_id)`, and rest's `ApiError::from_sqlx` becomes `ApiError::from_pg`. Both keep today's SQLSTATE → response mapping (code, message, detail, hint, position); a closed connection maps to today's I/O failure.
- **rest `conn.rs`:** `DbPool` wraps a deadpool `Pool`.
  - Size and idle counts come from `status()`; resizing uses `resize()`.
  - `idle_to_close` closes idle connections through `retain()` on the chosen pool, oldest `last_used` first.
  - The idle timeout (`CONNECTION_IDLE`) becomes a reaper task that calls `retain()` every 10 s; deadpool has no idle timeout of its own.
  - The acquire deadline (`ACQUIRE_TIMEOUT`) becomes `timeouts.wait`.
  - Recycling is `RecyclingMethod::Fast`: no ping. A broken connection is caught by `is_closed()` and by the existing retry on connect errors.
  - `RESET_SQL` moves from after-release to the front of each request's pipeline.
  - The branch slots, the global semaphore and their tests stay as they are.
- **Orchestrator console:** its owner pools (`services/orchestrator/src/console.rs`) move to deadpool the same way.
- **Unchanged:** metadata access everywhere, and the SQL editor (already tokio-postgres, a fresh connection per run).

### Pipelined execution

In `chelabase_query::run`, used by rest and the console.

**Common to C and D:**
- **Last-known version:** `QueryCache` keeps the last-seen schema version per (branch, database), and a request plans against it. With no known version (cold start, after `invalidate`), the request runs the unpipelined path once: preamble, then plan, then queries, then `COMMIT`.
- **Separate round trips that stay:** a cache miss on the owner-level check, the schema or the role's view costs one before the pipeline, as today.
- **Pipeline order:** `batch_execute("SET ROLE NONE; RESET ALL; BEGIN")`, the preamble, each plan query (`prepare_cached` + execute), then `COMMIT` (C only).
- **Errors:** the first real error wins; the `25P02` errors after it are expected and ignored. Nothing after a failure commits, so a failing batch behaves as today's stop-at-first-error.
- **Stale version:** roll back, reload the schema and its version, re-plan, and retry once on the unpipelined path. A second mismatch returns today's schema-changed failure.
- **Console:** `OWNER_PREAMBLE_SQL` gets the same treatment. The plan cache's role, today read from `current_user` in the preamble, comes from the pool's login user, so it's known before the pipeline.

**D (2 RTT):** the plan queries go out with the preamble. The client awaits it, compares versions and applies the response budget in Rust (as today), then sends `COMMIT`, or `ROLLBACK` and the retry. No database changes.

**C (1 RTT):** `COMMIT` is already on the wire, so two checks move into SQL:
- `chelabase.assert_schema_version(expected uuid)` raises SQLSTATE `CB001` when `chelabase.schema_version` differs. The preamble calls it. It is `SECURITY DEFINER` (plan ruling P4): it runs after the role switch, and the API roles can't read the version table.
- `chelabase.cap_response(result text, budget bigint) RETURNS text` adds `octet_length(result)` to a transaction-local counter (`set_config('chelabase.response_bytes', …, true)`) and raises `CB002` once the total passes `budget`. Every plan's outer select wraps its result in it, so an oversized response rolls back instead of committing writes and then failing. `CB002` maps to today's response-too-large failure; Rust no longer applies the budget.
- Both functions go into the worker's bootstrap SQL: owned by `chelabase_admin`, `REVOKE ALL … FROM PUBLIC`, `EXECUTE` granted to `authenticator` and the owner role, and added to the allow-list arrays of `chelabase` functions. Reconcile reruns the bootstrap, so existing databases get them.
- **Rollout:** a database missing the functions (`42883` on either) is marked, and rest uses D for it until the next schema reload.

## Testing and verification

**Phase 1:**
- Harness unit tests: B's literal escaping (quotes, backslashes, claims JSON containing `'`), the netem command builder, and the stats code (percentiles, median of runs).
- A live check of B's escaping: claims set through B round-trip through `set_config` unchanged.
- `just live just bench-data-api --quick`: every variant returns byte-identical JSON for each op, and the qdisc is gone afterwards.

**Phase 2:**
- Unit tests:
  - `from_pg` gives the same responses as today's `from_sqlx` for 23505, 42501, 23503, 57014, 25P02 and a closed connection;
  - the existing `conn.rs` tests pass on deadpool, plus a reaper test and a `retain` make-room test;
  - pipeline outcome handling on a fake client: first error wins, `25P02` ignored, retry once.
- Live tests in `services/rest/tests/rest_live.rs`:
  - a stale version (DDL between two requests) re-plans and succeeds, without committing twice;
  - a mid-batch failure leaves no rows;
  - a write whose response passes the budget leaves no row (C) or never commits (D);
  - session state set by an RPC (`set_config(…, false)`, `SET ROLE`) doesn't reach the next request;
  - C only: a database without the guard functions falls back to D;
  - C only: the worker's bootstrap tests pin both functions and their grants.
- The full verification run before the merge pause: `cargo test --workspace`, clippy with `-D warnings`, the rest, console and `app_users` live tests, then through `just live`: `data-e2e`, `app-users-e2e`, `neon-e2e`, the Studio console and SQL editor tests, and a short Playwright pass on the data and console pages.
- A final benchmark run through the real rest code confirms the shipped path is within ±10% of the harness's winning variant; the numbers are appended to the results doc.

## Out of scope

- Metadata access (stays on sqlx).
- TLS to computes, CancelRequest on dropped requests, and the other platform-mode items deferred from step 6.
- Variant B as a shipped change (only if the user asks after the pause).
- Moving tokio-postgres back to crates.io (waits for rust-postgres/rust-postgres#1391).
