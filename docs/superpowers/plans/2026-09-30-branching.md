# Branching: Implementation Plan (roadmap step 5)

> **For agentic workers:** REQUIRED SUB-SKILL: use superpowers:subagent-driven-development to implement this plan task by task. Steps use checkbox (`- [ ]`) syntax for tracking.
>
> **Plan style:** this plan pins interfaces, behavior, tests and gotchas; the implementer writes the code. Literal code appears only where its exact form matters (the migration). Where a signature is given, use it exactly, because other tasks depend on it.

**Goal:** A database has a tree of branches (main included). Each branch is a Neon timeline with its own endpoint, compute and password. Branches can be created now or at a past point, reset to their parent, restored in place, and deleted, through the platform API and Studio.

**Architecture:**
- A `branches` table holds the per-connection state that used to be on `databases`. Main's branch id equals its database id.
- The orchestrator validates and publishes jobs.
- The worker runs every flow through its ports; timeline switches happen by updating metadata and recreating the compute.
- Reconcile covers branches and the history window.

**Tech Stack:** Rust 2024, sqlx 0.9 (Postgres, chrono), axum 0.8, async-nats, reqwest 0.13, bollard 0.21. Neon at `infra/neon/NEON_REF` (`fa504217`). React, vitest and Playwright for Studio. Bash, psql, curl and jq for the scripts.

**Spec:** `docs/superpowers/specs/2026-09-30-branching-design.md` (approved 2026-09-30).

## Status

- **Current:** paused 2026-09-30 for a `/clear` (the user's request). The end-of-step review returned With fixes. The fix batch is done: `47aefc2` (I1: backups as parents), `4e194a1` (I2: the oldest point names a time, or says the point predates the branch), `53f55c6` (I3: the claim clears the last error), `a9082a9` (I4: README), `c4a9784` (the minors: `INTERRUPTED` wording and test, the `live_db.rs` sweep, `branch-e2e` without the retry loop, comments), `1e008c3` (M3: transient storage-controller failures retry), `5db5991` (M5: create and detach get their own timeouts; a running detach is never undone), `770755a` (M7: visible, `aria-describedby`-linked reasons for disabled actions).
  - **Full run after the last fix commit (it counts as the step's verification run):** workspace 578, clippy clean, live metadata 40, `branch_live` 1, NATS live 2, Studio 136 plus typecheck and build, `branch-e2e` 44 s, `neon-e2e` 14 s, `auth-e2e` 19 s, Playwright green. The `branch-e2e` retries were never needed (6 reproduction rounds, no 5xx) and are gone.
  - **Re-review (sonnet, 2026-10-01) of `f78f668..770755a`: With fixes.** Every earlier finding is addressed. Two new Important issues: I-1, a stuck detach retries forever with nothing recorded (every redelivery renews the claim, so reconcile never rescues it); I-2, Studio bounds the pickers by `created_at`, which blocks a backup's older history and past-point children. Minors: the stale-error prefix, a retry window of about 20 s, the too broad `live_db` sweep, the "was created" wording and the spec text. Detach idempotence and the in-progress acks were judged fine.
  - **Decisions by the user (2026-10-01):** I-1: record every retry's cause and fail after 1200 s (2 × the detach timeout), deleting nothing. I-2: a new `branches.history_start`, returned by the API, bounds the pickers. Wording: "before the history of <name> begins (<time>)" for a backup, and "before <name>'s history begins (<time>); branch from <parent> instead" for a child. Update the spec's "Resolve a point" to match.
  - **Second fix batch done (opus):** `9f0e415` (I-1: record every unresolved detach's cause; fail after 1200 s, deleting nothing; migration `20261001000001_branch_pending_timeline_at`), `5bd1ea7` (I-2: `history_start`; migration `20261001000002_branch_history_start`), `b591d71` (the "history begins" wording and the spec), `8e7e2b2` ("Last attempt:"), `43ea6e1` (a branch job nak delay of 30 s × 5 deliveries), `c043d1b` (the live sweep takes only its own databases). `just test` 589, clippy clean, live metadata 42, Studio 142 plus typecheck and build. `branch_live` and `branch-e2e` weren't run (they need `just dev`). The fixer's concerns went to the re-review: a restored branch's `history_start` may be too strict; deleting a stuck-failed branch removes its pending timeline, possibly mid-detach; a stuck-failed main leaves the database `ready`.
  - **Re-review (sonnet) of `37c866f..c043d1b`: Approved**, with Minors only. A stuck-failed main leaving `databases.status` at `ready` is consistent with ruling 4. Deleting a stuck-failed branch can at worst leak a timeline, since reconcile retries; accepted.
  - **Last minors done (`3b7a36c`):** checked live that a restored branch reaches back before the restore point (a branch created from restored main at a time before the restore became ready and held the older row), so the restored branch now keeps its old `history_start`. `ReportStuckRestore` logs at `info`. The stuck-restore failure ends "; delete the branch to remove it". The README says a failed main shows on the branch.
  - **Step verification run after `3b7a36c` (2026-10-01):** `just test` 589, clippy clean, live metadata 42 plus `neon_live` 9, NATS live 2, Studio 142 plus typecheck and build, `branch_live` 1 (27.9 s), `branch-e2e` 40 s, `neon-e2e` 13 s, `auth-e2e` 19 s. Playwright wasn't rerun because no Studio code changed after the last pass. The dev apps are stopped and the deps stack is running.
  - **Current:** merged into local `main` on 2026-10-01 (user's go-ahead), not pushed. Step 5 is done.
  - **Decisions by the user (2026-09-30):** `INTERRUPTED` reads "The last reset or restore was interrupted before it switched timelines; the branch is unchanged. Try again."; M3, M5 and M7 were fixed before the merge rather than deferred.
  - Before migrating the other computer: `SELECT id FROM databases WHERE status = 'deleted' AND endpoint_id IS NOT NULL;` must return nothing.
- **Done:**
  - Task 1 (`6833a81`, fix `0a7b0ae`): `lsn_by_timestamp` (with lease), `timestamp_of_lsn`, `detach_ancestor`, `patch_tenant_config`, `tenant_pitr_interval` (`Option`: `None` when never set; a follow-up change, because every existing tenant is unset), `list_timelines`. Mock tests 30, and clippy is clean. Added `humantime 2.4.0` and `urlencoding 2.1.3`. `get_timestamp_of_lsn` answers 412 (not 404) when no timestamp is known; both map to `None`.
- **Rulings (made while planning; cost if wrong):**
  1. **Main's branch id equals its database id.** Every sealed owner password's AAD is the branch id, and for main that is the database id, so the backfilled passwords stay valid and the aliases need no lookup. *Cost if wrong: small; add a lookup.*
  2. **The new Neon tenant column is `databases.neon_tenant_id`**, not `tenant_id` as the spec wrote, because `DatabaseRecord.tenant_id` is already the Cheladb tenant. *Cost: none.*
  3. **The backfill covers every non-deleted database, not only those with an endpoint**, so an in-flight provision still has a main branch to finish. *Cost: none; this is a wider reading of the spec.*
  4. **`databases.status` stays the database's lifecycle.** Provisioning sets main's status and the database's status in one transaction. Deleting/deleted is driven from the database. Other branch statuses don't touch the database. *Cost: small.*
  5. **The point-in-time API takes RFC 3339 timestamps or `X/Y` hex LSNs.** Studio sends timestamps only. *Cost: none.*
- **Pre-flight scan (2026-09-30):** checked every interface one task produces and another consumes (Task 1 → 4/6, Task 2 → 3/4/5/6, Task 3 → 4/5/7/9, Task 5 → 6, Task 6 → 7), and each task's tests against its own code. Two gaps were fixed in the plan: `BranchJob::Restore` now carries `backup_name`, and Task 2 adds `set_branch_error` for the back-to-ready path in Task 5. No other conflicts were found.
  - Task 2 (`3f5688a`, fix `977436b`): the migration (byte-identical to the plan; applied to the local metadata DB); `BranchId`; the branch queries in `branches.rs`; main as a branch everywhere (reveal/reset/delete/reconcile/provision unchanged for users). Review (opus) found two Important issues, fixed in round 1:
    - a database that reconcile had failed could become deleted when `set_branch_ready` refused. It now goes through `abandon_gone` and stays failed.
    - lock-order inversions between `databases` and `branches` could deadlock. **Rule (top of `branches.rs`): databases row first, then branches, then `neon_*`; `databases` rows are locked `FOR NO KEY UPDATE`.** The switch also fixed a second deadlock (create racing restore).
    - `mirror_on_main` only overwrites a pending, provisioning or failed main.
    - Workspace 442 passed; live metadata 29 + 9; clippy clean.
  - Task 3 (`47e63fd`): routes (the gateway already handled two path params and PATCH), the orchestrator's `branches.rs` with every refusal from the spec, `BranchJob`/`PointInTime`/settings jobs, the reset RPC by `BranchId`, and `/v1/me` folded into one query (`main_branches_with_counts`). Choices: the list is a bare array; `branch_count` counts live `normal` branches **including main**, excluding backups; `history_window` is in seconds in the PATCH; restore returns `backup_branch_id` and `backup_name`. "LSN ahead" and "outside the window" are reported on the branch by the worker, because the spec's "Resolve a point" says the API never calls Neon. Review (sonnet): approved, minors only. It was stopped once by a rate limit and resumed.
  - Task 4 (`4d055be`, fix `fa87b04`): `point.rs`, `branch_create.rs`, `branch_delete.rs`; database delete tears down every branch (spec order: computes, tenant, endpoint rows); `begin_branch_delete` locks the database row first (a real 40P01 reproduced, then fixed) and returns `BranchDeleteOutcome { Started, NotMoved, HasChildren { backup, children } }`. Review (opus) found three Important issues, all fixed in round 1:
    - deleting a branch whose restore backup had live children hung forever. The API, the metadata and the worker now refuse it, naming the children.
    - teardown order is now pending → current → superseded, children before ancestors.
    - the fakes now model Neon's 412 on deleting an ancestor and reject an LSN ahead of the head.
    - Also fixed outside the brief: reconcile's `retry_teardown` could delete the whole tenant, main included, for a failed branch's endpoint; it now deletes the tenant only when the tenant holds no other timeline (Task 6 must keep this). Workspace 508 passed; live metadata 32 + 9; clippy clean.
  - Task 5 (`d99fd2f`, fix `5de5cce`): `branch_switch.rs` (reset-to-parent, restore, `recreate_compute`, which reconcile now uses and which waits for readiness); a backup-name clash is `MetadataError::BranchNameTaken` and the branch goes back to ready; redelivery restarts or finishes a half-done attempt. Review (opus) found one Important issue, caused by the planning ruling that the worker could start from `ready`: a redelivered reset reset a branch twice and lost writes. **Ruling amended: the worker acts only on `resetting` (the API always claims first); a stale job is a no-op.** Hardening: both switch queries take `expected_pending` and lock the databases row first. Worker 96, live metadata 33 + 9, clippy clean.
  - Task 6 (`58ced3e`, fix `5d79bf9`): reconcile gains branches, tenant timelines and pitr; actions `FailStaleBranch`, `FinishSwitch`, `RetryBranchDelete`, `RecreateBranchCompute` (replaces `RecreateCompute`), `ReportUnknownTimeline`, `PatchHistoryWindow`; `settings.rs` `handle_database_settings`, also called by provision after `create_tenant`. New queries: `swap_pending_timeline` (CAS; re-records the id if the delete fails), `fail_branch_if_stale`, `claim_stale_reset`. Review (opus) found one Important issue, fixed in round 1: `finish_switch`'s claim was status-only, so two runs could both recreate the compute; it now claims only a resetting branch older than 600 s. Worker 126, live metadata 37, clippy clean.
    - **Ruling:** a live switch job running >600 s past its last write can still overlap a reconcile `FinishSwitch`; accepted, since a job that slow is stuck. *Cost: a compute whose token hash mismatches until the next reset/reconcile.*
    - **For the user:** a crashed switch that recorded no error shows the new text `INTERRUPTED` (beyond the spec's "mark it ready").
  - Task 7 (`35d00d3`): `Worker::handle_branch_job` (`branch_job.rs`) dispatches `BranchJob`; `main.rs` starts the branch and settings consumers; `neon_live` helpers moved to `tests/common/mod.rs`; `branch_live.rs` passes (27.6 s). Timings: provision 7.3 s, create at a time 2.2 s, reset to parent 3.9 s, delete branch 1.8 s, restore 3.8 s, create from backup 1.9 s, out-of-window create 24 ms, delete database 1.4 s. Workspace 558 passed; clippy clean.
    - The implementer deleted a leftover `live-test-*` database (and its branches) from the dev metadata DB: `live_db.rs` residue sealed with another key, which made `just dev` refuse to start. `secrets.key` untouched.
  - Task 8 (`066f9b6`): `scripts/branch-e2e.sh` and `just branch-e2e` (the Task 7 story through the gateway API and the proxy); the `api` helper moved into `scripts/lib/neon-common.sh`; `neon-test` gains `branch-e2e recipe exists` (RED→GREEN). Live: `branch-e2e` 41 s, `neon-e2e` 15 s, `auth-e2e` 18 s.
  - Task 9 (`094e886`): the branch tree, branch actions, the new-branch form and database settings in Studio. Studio 125 tests; types and Studio typecheck, test and build pass; Playwright pass done.
    - **Ruling:** the plan's Playwright order restored main while a child existed, which the children rule refuses; the pass ran reset → delete the branch → restore main → delete the backup. *Cost: none.*
  - **Carry into Task 6:** a crash between a claim and its publish leaves a branch `resetting` (no pending timeline) or `deleting`; reconcile must recover both. `drop_leftover_pending` (Task 5) and `FinishSwitch` must clear `pending_timeline_id` by CAS on the expected id **before** deleting the timeline (closes a drop-before-clear race with an overlapping run; a crash leaves a reported, never-deleted timeline). Reconcile's `retry_teardown` keeps Task 4's rule: delete the tenant only when it holds no other timeline.
  - **Carry into Task 9:** the cap display is `branch_count - 1` against `max_branches`.
  - **Carry into Task 4:** `begin_branch_delete` must lock the database row first, or it can deadlock with `begin_delete` over branch rows. Any new transaction in Tasks 4–6 follows the lock-order rule (`lock_database_of_main`, or take the database row first).
- **Deferred minors:**
  - Task 2: `/v1/me` makes one `get_branch` query per database (fold it into the overview when Task 3 adds `branch_count`); a backup-name clash in `switch_to_restored_timeline` surfaces as a raw sqlx error (Task 5 should map it or pre-check); the `sweep-dbs` counts omit branches; the `BranchId` test and two API tests were mutation-checked rather than written red-first; the I2 RED tests sleep 300 ms instead of polling `pg_stat_activity`.
  - Task 3: reset-to-parent doesn't check the parent is ready (moot through the claim rule; no test); `BranchSummary` maps to `DatabaseStatus` to reuse `proxy_address`; the PATCH "window changed" check uses the pre-update record (reconcile heals it); backup-name truncation can leave a trailing `-` or `_`; no test for a backup as a parent.
  - Task 4: metadata errors while recording the endpoint or at the re-check fail the branch instead of retrying (the cause is lost); an `{lsn}` older than the window fails at `creating timeline:` with Neon's text rather than the "oldest available point" message; a `set_branch_ready` error after a finished delete leaks the timeline (same trade-off as provision); the refusal groups children by parent name, not id.
  - Task 5: `switch_refused`'s text says another run owns the flow even when the API released the branch mid-flow (reconcile restores the compute); a misplaced doc comment on `lock_database_of_branch`/`move_endpoint`; a late restore job that arrives after the user's retry runs with the first job's backup id and name.
  - Task 6: `INTERRUPTED` path untested (and needs the user's OK on the wording); a `target_of` error isn't recorded on the branch (stays resetting, retried every 10 min silently); the branch is read before the claim (superseded/error could be stale); `a_failed_timeline_listing_reports_nothing_and_carries_on` overpromises (add a plan case: a tenant with no listing gives no report); a deleting branch with children warns every 60 s; provision tests count `get_database` calls (fragile); `reconcile.rs` is ~1,900 lines (split the branch tests out).
  - Task 7: on a young tenant the out-of-window error says "the oldest available point is unknown" (no commit at the cutoff), not an RFC 3339 time; `live_db.rs` doesn't clean up its `live-test-*` databases; two orphan `neon_endpoints` rows (`ep-6f283f366f39`, `ep-02de350414cd`).
  - Task 8: `branch-e2e` retries the too-old create up to 6 times under new names because the 1 h window reaches Neon asynchronously (failed branch rows remain until the database is deleted); `KEEP=1` checked only on a passing run; no shellcheck on this machine (`bash -n` only).
  - Task 9 (triage at the end review; possibly Important): a failed restore leaves its error on the branch after it returns to ready, so Studio shows a stale error on a ready main; the restore success notice stays until the next action; the cap is counted from the branch list rather than `/v1/me`; the disabled-action tooltips are `title` attributes.
  - Before migrating the other computer: check for `deleted` databases that still have an `endpoint_id` (there should be none; such a row would get no branch).
  - Task 1: the new test code in `storcon_mock.rs` isn't rustfmt-clean (existing file); the first commit's trailer names Sonnet.
- **Left running:** the deps stack (Neon included). The dev stack is stopped.

## Global Constraints

- **Git:** branch `branching`, worktree `<repo root>/.worktrees/branching`. Commit with `git commit`, ending with the `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>` and `Claude-Session: https://claude.ai/code/session_01SUiNQSh3VyeaGC5pkLP934` trailers. Don't merge or push.
- **Formatting:** never run `cargo fmt` on the workspace; run `rustfmt --edition 2024 <file>` only on files you create.
- **Secrets:** never overwrite `.cheladb/secrets/secrets.key`, the gateway key pair, the cookie key or the compute-JWT key pair.
- **Strict TDD:** a failing test run (RED) comes before any implementation in every task, and the report shows the RED and GREEN commands and output.
- **Ids:**
  - branch id is a uuid (`BranchId`), and main's equals the database id;
  - timelines are `NeonId` (32 hex);
  - an endpoint is `ep-` + 12 lowercase hex;
  - a compute token is 32 random bytes in hex, of which only the SHA-256 is stored;
  - a branch password is `credentials::generate_password()`, sealed with `Purpose::Owner`, AAD = branch id.
- **Branch names:** 1–63 characters, `^[a-z0-9][a-z0-9_-]*$`. `main` is reserved for the root. Backups are `<name>-before-restore-<yyyymmddThhmmssZ>`; when too long, `<name>` is truncated so the whole name fits in 63.
- **Statuses:** `pending | provisioning | ready | resetting | failed | deleting | deleted`. Kinds: `normal | backup`.
- **Limits:**
  - `history_window` default `7 days`, allowed from 1 hour to 30 days, stored as `interval`, and sent to Neon as `pitr_interval` in seconds with a unit (for example `604800s`);
  - `max_branches` default 5, allowed 0–20, counting live `normal` branches other than main.
- **Children rule:** delete, reset-to-parent and restore are refused while the branch has live `normal` children. Backups never count as children. Deleting a branch deletes its backups.
- **Errors:** worker failures are stored with the full cause chain (`{e:#}`), prefixed by the step: `resolving the point:`, `creating timeline:`, `detaching:`, `starting compute:`, `waiting for the compute:`, `bootstrapping roles:`, `switching timeline:`, `removing compute:`, `deleting timeline:`, `deleting endpoint:`.
- **Point resolution:**
  - `kind=present` → the returned LSN;
  - `future` → the source's `last_record_lsn`;
  - `past` → `outside the history window; the oldest available point is <rfc3339>`;
  - `nodata` → `no data at that time`;
  - an LSN ahead of the head → `the LSN <lsn> is ahead of the branch (at <head>)`.
- **Ports:** the proxy is at `<ep>.<CHELADB_PROXY_DOMAIN>` on 4432 and 4444. The reveal format is unchanged from step 4.
- **WSL:** the ephemeral port range is small. If tests fail binding `127.0.0.1:0`, wait a minute and rerun.

## Review Focus

1. **A delete lands while a branch create, reset or restore is in flight.** Everything the flow created is removed, and the branch ends `deleted`, never `ready` or `failed`. Tested in Tasks 4 and 5.
2. **A crash between creating a timeline and switching to it**, and between switching and deleting the old one. Reconcile finishes the flow using `pending_timeline_id` and `superseded_timeline_id`. No timeline leaks, and the branch never points at a missing timeline. Tested in Tasks 5 and 6.
3. **Two creates race for the last slot under the cap.** Exactly one wins. Tested live in Task 2.
4. **The migration runs on the other computer's metadata.** It holds pending, ready, failed and deleted databases, a pre-step-4 tombstone, and possibly a leftover step-3 demo endpoint with no database. Each live database gets exactly one main branch whose sealed password still opens, and nothing else changes. Tested live in Task 2.
5. **A branch point at the edge of the window, or before it.** Studio and the API show the "oldest available point" message, and no branch is left half-made. Tested in Task 4 with fakes and in Task 7 live.

---

## File Structure

| File | Responsibility |
|---|---|
| `crates/cheladb-types/src/lib.rs` (modify) | `BranchId` |
| `crates/cheladb-neon/src/storcon.rs` (modify) | `lsn_by_timestamp`, `timestamp_of_lsn`, `detach_ancestor`, `patch_tenant_config`, `tenant_pitr_interval` |
| `crates/cheladb-metadata/migrations/20260930000001_branches.sql` (create) | the migration |
| `crates/cheladb-metadata/src/branches.rs` (create) | branch types and queries |
| `crates/cheladb-metadata/src/{lib.rs,neon.rs}` (modify) | database record and settings, step 4 queries moved to branches |
| `crates/cheladb-nats/src/lib.rs` (modify) | branch jobs and database settings job |
| `crates/cheladb-routes/src/lib.rs` (modify) | new endpoints |
| `services/orchestrator/src/{lib.rs,branches.rs}` | the API |
| `services/worker/src/{ports.rs,fakes.rs,point.rs,branch_create.rs,branch_delete.rs,branch_switch.rs,settings.rs,reconcile.rs,provision.rs,delete.rs,reset.rs,main.rs}` | the worker |
| `services/worker/tests/branch_live.rs` (create) | live story |
| `scripts/branch-e2e.sh`, `justfile`, `infra/neon/tests/run.sh` | e2e |
| `packages/types/src/index.ts`, `apps/studio/src/lib/api.ts`, `apps/studio/src/components/{branch-tree,branch-actions,new-branch-form,database-settings}.tsx` and tests | Studio |
| `README.md`, roadmap | docs |

**Models:** the implementer and per-task reviewer for each task. A task gets its own review only when later tasks build on it, or when nothing else would catch a mistake.

| Task | Implementer | Reviewer |
|---|---|---|
| 1 Neon client additions | sonnet | none (mock-server tests; exercised live in Task 7) |
| 2 Migration, metadata, main as a branch | opus | opus (irreversible migration; everything builds on it) |
| 3 API, routes, jobs | sonnet | sonnet (Tasks 4–9 consume it) |
| 4 Worker: point resolution, create, delete | opus | opus |
| 5 Worker: reset to parent, restore | opus | opus |
| 6 Worker: reconcile and history window | opus | opus |
| 7 Wiring and live test | sonnet | none (live test) |
| 8 `branch-e2e` and just | sonnet | none (e2e runs) |
| 9 Studio | sonnet | none (unit tests, Playwright) |
| 10 Docs | haiku | none |
| End-of-step whole-branch review | | opus |

---

### Task 1: Neon client additions (`cheladb-neon`)

**Files:** `crates/cheladb-neon/src/storcon.rs` and its mock tests (`tests/storcon_mock.rs`).

**Produces (exact):**

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LsnKind { Present, Future, Past, Nodata }
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LsnAtTime { pub lsn: Lsn, pub kind: LsnKind }
impl StorconClient {
    /// GET /v1/tenant/{t}/timeline/{tl}/get_lsn_by_timestamp?timestamp=<rfc3339>&with_lease=true
    pub async fn lsn_by_timestamp(&self, tenant: &NeonId, timeline: &NeonId, at: chrono::DateTime<chrono::Utc>) -> Result<LsnAtTime, StorconError>;
    /// GET …/get_timestamp_of_lsn?lsn=<lsn>; None on 404 (no timestamp known).
    pub async fn timestamp_of_lsn(&self, tenant: &NeonId, timeline: &NeonId, lsn: Lsn) -> Result<Option<chrono::DateTime<chrono::Utc>>, StorconError>;
    /// PUT /v1/tenant/{t}/timeline/{tl}/detach_ancestor?detach_behavior=multi_level_and_no_reparent
    pub async fn detach_ancestor(&self, tenant: &NeonId, timeline: &NeonId) -> Result<(), StorconError>;
    /// PATCH /v1/tenant/config {"tenant_id": t, "pitr_interval": "<secs>s"}
    pub async fn patch_tenant_config(&self, tenant: &NeonId, pitr_interval: std::time::Duration) -> Result<(), StorconError>;
    /// The tenant's effective pitr_interval (GET /v1/tenant/{t}/config passthrough; `effective_config.pitr_interval`).
    /// None when the tenant never had pitr_interval set (every tenant created before step 5).
    pub async fn tenant_pitr_interval(&self, tenant: &NeonId) -> Result<Option<std::time::Duration>, StorconError>;
    /// GET /v1/tenant/{t}/timeline (passthrough): every timeline id in the tenant.
    pub async fn list_timelines(&self, tenant: &NeonId) -> Result<Vec<NeonId>, StorconError>;
}
```

**Gotchas:**
- The pageserver's response shapes are at `NEON_REF`. Read them from GitHub raw (`pageserver/src/http/routes.rs`, `get_lsn_by_timestamp_handler` and `get_timestamp_of_lsn_handler`; `libs/pageserver_api/src/models.rs`, `TenantConfigPatch` and the tenant config response). Mirror the existing client methods' error mapping, and use `urlencoding` for the timestamp.
- Durations come back in humantime form (`"7days"`, `"604800s"`); parse with `humantime` (add it if missing). Check the effective config's key name against the source before relying on it.
- `detach_ancestor` may answer 202 or 200 with a body of reparented timelines. Treat any 2xx as success.

**Tests** (mock server, as the existing `storcon_mock.rs` does):
- `lsn_by_timestamp_parses_each_kind`: four cases; check that `with_lease=true` and the timestamp are in the query.
- `timestamp_of_lsn_none_on_404`.
- `detach_ancestor_sends_multi_level_no_reparent`.
- `patch_tenant_config_sends_seconds`.
- `tenant_pitr_interval_parses_humantime`.
- `list_timelines_returns_ids`.
- An error case: a 500 maps to `StorconError` with the body.

- [ ] RED, implement, GREEN: `cargo test -p cheladb-neon`; clippy clean.
- [ ] Commit `cheladb-neon: timestamp lookups, detach_ancestor and tenant pitr config`.

---

### Task 2: Migration, metadata, and main as a branch

**Files:**
- create `crates/cheladb-metadata/migrations/20260930000001_branches.sql` and `crates/cheladb-metadata/src/branches.rs`;
- modify `crates/cheladb-types/src/lib.rs`, `crates/cheladb-metadata/src/{lib.rs,neon.rs}`, `tests/{live_db.rs,neon_live.rs}`;
- modify every caller that uses the moved fields: `services/orchestrator/src/{lib.rs,internal/*}`, `services/worker/src/{ports.rs,fakes.rs,provision.rs,delete.rs,reset.rs,reconcile.rs}` and their tests.

**The migration (exact):**

```sql
-- Roadmap step 5: branches. Main is a branch whose id equals its database id, so sealed
-- owner passwords (AAD = that id) stay valid. Per-connection state moves off databases.
CREATE TABLE branches (
    id                     uuid PRIMARY KEY,
    database_id            uuid NOT NULL REFERENCES databases (id),
    name                   text NOT NULL,
    parent_branch_id       uuid REFERENCES branches (id),
    kind                   text NOT NULL DEFAULT 'normal' CHECK (kind IN ('normal', 'backup')),
    status                 text NOT NULL,
    timeline_id            text,
    pending_timeline_id    text,
    superseded_timeline_id text,
    endpoint_id            text REFERENCES neon_endpoints (id) ON DELETE SET NULL,
    owner_password_enc     text,
    branched_at_time       timestamptz,
    branched_at_lsn        text,
    error                  text,
    created_at             timestamptz NOT NULL DEFAULT now(),
    updated_at             timestamptz NOT NULL DEFAULT now(),
    deleted_at             timestamptz
);
CREATE UNIQUE INDEX branches_live_name ON branches (database_id, name) WHERE status <> 'deleted';
CREATE UNIQUE INDEX branches_live_main ON branches (database_id)
    WHERE parent_branch_id IS NULL AND status <> 'deleted';
CREATE UNIQUE INDEX branches_endpoint ON branches (endpoint_id) WHERE endpoint_id IS NOT NULL;
CREATE INDEX branches_parent ON branches (parent_branch_id);
CREATE INDEX branches_status ON branches (status);

ALTER TABLE databases
    ADD COLUMN neon_tenant_id text,
    ADD COLUMN history_window interval NOT NULL DEFAULT interval '7 days',
    ADD COLUMN max_branches int NOT NULL DEFAULT 5;

INSERT INTO branches (id, database_id, name, parent_branch_id, kind, status, timeline_id,
                      endpoint_id, owner_password_enc, error, created_at, updated_at)
SELECT d.id, d.id, 'main', NULL, 'normal', d.status, e.timeline_id,
       d.endpoint_id, d.owner_password_enc, d.error, d.created_at, d.updated_at
  FROM databases d
  LEFT JOIN neon_endpoints e ON e.id = d.endpoint_id
 WHERE d.status <> 'deleted';

UPDATE databases d SET neon_tenant_id = e.tenant_id
  FROM neon_endpoints e WHERE e.id = d.endpoint_id;

ALTER TABLE databases
    DROP COLUMN endpoint_id,
    DROP COLUMN owner_password_enc;
```

**Produces (exact; `cheladb_types` and `cheladb_metadata`):**

```rust
id_newtype!(BranchId, "Stable identifier for a branch of a database; main's equals its database id.");
impl From<DatabaseId> for BranchId  // main's id

pub enum BranchStatus { Pending, Provisioning, Ready, Resetting, Failed, Deleting, Deleted } // as_str/parse like DatabaseStatus
pub enum BranchKind { Normal, Backup }
pub struct BranchRecord {
    pub id: BranchId, pub database_id: DatabaseId, pub name: String, pub parent_branch_id: Option<BranchId>,
    pub kind: BranchKind, pub status: BranchStatus, pub timeline_id: Option<String>,
    pub pending_timeline_id: Option<String>, pub superseded_timeline_id: Option<String>,
    pub endpoint_id: Option<String>, pub owner_password_enc: Option<String>,
    pub branched_at_time: Option<DateTime<Utc>>, pub branched_at_lsn: Option<String>,
    pub error: Option<String>, pub created_at: DateTime<Utc>, pub updated_at: DateTime<Utc>,
}
pub struct NewBranch<'a> { pub id: BranchId, pub database_id: DatabaseId, pub name: &'a str,
    pub parent_branch_id: BranchId, pub branched_at_time: Option<DateTime<Utc>>, pub branched_at_lsn: Option<&'a str> }
pub enum CreateBranchOutcome { Created, NameTaken, CapReached { max: i32 }, ParentNotReady, DatabaseNotLive }
pub struct DatabaseSettings { pub history_window: std::time::Duration, pub max_branches: i32 }
pub struct LiveBranch { pub id: BranchId, pub database_id: DatabaseId, pub kind: BranchKind, pub status: BranchStatus,
    pub endpoint_id: Option<String>, pub timeline_id: Option<String>, pub pending_timeline_id: Option<String>,
    pub superseded_timeline_id: Option<String>, pub age_secs: i64 }

impl MetadataClient {
    pub async fn get_branch(&self, id: BranchId) -> Result<Option<BranchRecord>, MetadataError>;
    pub async fn list_branches(&self, db: DatabaseId) -> Result<Vec<BranchRecord>, MetadataError>; // non-deleted, created_at order
    /// One transaction: lock the database row FOR UPDATE; refuse unless it's live (not deleting/deleted);
    /// the parent must be a live branch of this database with status ready; count live normal non-main
    /// branches against max_branches; insert status 'pending'. A unique-name violation → NameTaken.
    pub async fn create_branch(&self, b: &NewBranch<'_>) -> Result<CreateBranchOutcome, MetadataError>;
    /// Live `normal` children only (backups excluded).
    pub async fn live_children(&self, id: BranchId) -> Result<Vec<BranchRecord>, MetadataError>;
    pub async fn live_backups_of(&self, id: BranchId) -> Result<Vec<BranchRecord>, MetadataError>;
    /// Guarded status change: `from` must match; never out of deleting/deleted. True when changed.
    pub async fn set_branch_status(&self, id: BranchId, from: &[BranchStatus], to: BranchStatus) -> Result<bool, MetadataError>;
    pub async fn set_branch_failed(&self, id: BranchId, error: &str) -> Result<(), MetadataError>; // not if deleting/deleted
    /// Records (or clears, with None) the last error without changing the status; used when a reset or restore
    /// fails before its switch and the branch goes back to ready.
    pub async fn set_branch_error(&self, id: BranchId, error: Option<&str>) -> Result<(), MetadataError>;
    /// → deleting unless already deleted; also the branch's backups. True when this call moved it.
    pub async fn begin_branch_delete(&self, id: BranchId) -> Result<bool, MetadataError>;
    pub async fn mark_branch_deleted(&self, id: BranchId) -> Result<(), MetadataError>; // clears owner_password_enc, sets deleted_at
    /// One transaction, like create_endpoint_for_database: endpoint + role + database rows, link to the branch,
    /// and record timeline_id. False (nothing written) if the branch is deleting/deleted or already linked.
    pub async fn create_endpoint_for_branch(&self, id: BranchId, timeline_id: &str, ep: &NewEndpoint<'_>) -> Result<bool, MetadataError>;
    /// ready + sealed password + branched_at_lsn, only from provisioning; for main also the database row (ruling 4).
    pub async fn set_branch_ready(&self, id: BranchId, sealed_password: &str, branched_at_lsn: Option<&str>) -> Result<bool, MetadataError>;
    pub async fn set_pending_timeline(&self, id: BranchId, timeline_id: Option<&str>) -> Result<bool, MetadataError>;
    /// One transaction (reset): branch.timeline_id and its endpoint's timeline_id := pending; superseded := old; pending := NULL.
    /// Only while resetting. False otherwise.
    pub async fn switch_to_pending_timeline(&self, id: BranchId) -> Result<bool, MetadataError>;
    /// One transaction (restore): insert the backup (kind backup, status ready, no endpoint, old timeline,
    /// parent = id, name given); switch as above but without superseded. Only while resetting.
    pub async fn switch_to_restored_timeline(&self, id: BranchId, backup: BranchId, backup_name: &str) -> Result<bool, MetadataError>;
    pub async fn clear_superseded_timeline(&self, id: BranchId) -> Result<(), MetadataError>;
    pub async fn database_settings(&self, db: DatabaseId) -> Result<Option<DatabaseSettings>, MetadataError>;
    pub async fn update_database_settings(&self, db: DatabaseId, history_window: Option<std::time::Duration>, max_branches: Option<i32>) -> Result<bool, MetadataError>;
    pub async fn set_neon_tenant(&self, db: DatabaseId, tenant: &str) -> Result<bool, MetadataError>;
    pub async fn list_live_branches_for_reconcile(&self) -> Result<Vec<LiveBranch>, MetadataError>;
}
// DatabaseRecord: remove endpoint_id and owner_password_enc; add neon_tenant_id: Option<String>, history_window, max_branches.
// replace_owner_secret now takes a BranchId and seals on branches (guard: ready, linked to endpoint_id, kind normal).
// EndpointState: database_id/database_status become branch_id/branch_status (from branches).
// ensure_key_check counts sealed rows in branches.
```

**Main as a branch (behavior unchanged for users):**
- **Provision** records main's endpoint with `create_endpoint_for_branch(BranchId::from(db), …)`, calls `set_neon_tenant`, and finishes with `set_branch_ready` (which also sets the database ready). A new database's main row is inserted with the database, in `insert_database_pending`: same id, name `main`, status pending.
- **Reveal and reset on the old routes** go through main's branch row, and the old API tests pass unchanged.
- **Delete of a database** marks main (and every branch) deleting, keeps step 4's teardown, then `mark_branch_deleted` for each branch and `mark_deleted` for the database.
- **Reconcile's endpoint rows** now come from branches. Its existing tests keep passing with the fakes updated.
- **`apply_pending`, the per-endpoint lock and the verifier-guarded clear** are unchanged.

**Tests:**
- **Live** (`live_db.rs`, `neon_live.rs`, ignored; run them with `--test-threads=1`):
  - `migration_backfills_main_for_live_databases_only`: rows shaped like before the migration (pending with no endpoint, ready with an endpoint and a sealed password, failed, deleted, plus an endpoint with no database). Because the migration has already run, apply the INSERT/UPDATE by hand to rows in a scratch schema or re-created tables, as step 4's tombstone test did. Assert one main per live database, id = database id, the sealed password equal to the source, the timeline from the endpoint, and nothing for deleted rows.
  - `create_branch_enforces_the_cap_under_a_race` (Review Focus 3): cap 1, two concurrent `create_branch`; exactly one `Created`, one `CapReached { max: 1 }`.
  - `create_branch_refuses_taken_names_and_unready_parents`.
  - `backups_do_not_count_as_children_or_toward_the_cap`.
  - `switch_to_pending_timeline_moves_endpoint_and_records_superseded`, and that it's refused unless the branch is resetting.
  - `switch_to_restored_timeline_inserts_the_backup_atomically`.
  - `begin_branch_delete_takes_backups_along`.
  - `set_branch_status_never_leaves_deleting`.
  - `main_set_ready_sets_the_database_ready_too`.
  - The existing step 4 metadata tests, adapted to branches.
- **Orchestrator and worker:** every existing test passes after the refactor. Add `reveal_uses_main_branch_row` (api).

- [ ] RED (the new live tests against stubs), implement, GREEN. `cargo test --workspace`, the ignored metadata tests and clippy all pass.
- [ ] Commit `metadata: branches table; main is a branch; per-connection state moves to branches`.

---

### Task 3: API, routes and jobs

**Files:** `crates/cheladb-routes/src/lib.rs`, `crates/cheladb-nats/src/lib.rs`, `services/orchestrator/src/{lib.rs,branches.rs}`, `services/orchestrator/tests/api.rs`, `packages/types/src/index.ts`.

**Produces:**
- **Routes** (all `Auth::Platform`, `Cors::Studio`):
  - `LIST_BRANCHES` `GET /v1/databases/{id}/branches`;
  - `CREATE_BRANCH` `POST /v1/databases/{id}/branches`;
  - `BRANCH_CONNECTION` `GET /v1/databases/{id}/branches/{branch_id}/connection`;
  - `BRANCH_RESET_PASSWORD` `POST …/{branch_id}/reset-password`;
  - `BRANCH_RESET_TO_PARENT` `POST …/{branch_id}/reset-to-parent`;
  - `BRANCH_RESTORE` `POST …/{branch_id}/restore`;
  - `DELETE_BRANCH` `DELETE …/{branch_id}`;
  - `UPDATE_DATABASE` `PATCH /v1/databases/{id}`.
  - Add them all to `ENDPOINTS`. **Check** that the gateway matcher handles two path parameters and `Method::Patch`, and add either if missing, with a routing test.
- **Jobs (`cheladb-nats`):**
  - `SUBJECT_BRANCH = "cheladb.branch"`, consumer `cheladb-worker-branch`;
  - `enum BranchJob { Create { branch_id, at: Option<PointInTime> }, ResetToParent { branch_id }, Restore { branch_id, backup_id, backup_name: String, at: PointInTime }, Delete { branch_id } }`, where `PointInTime` is `#[serde(rename_all = "snake_case")] enum { Timestamp(DateTime<Utc>), Lsn(String) }`;
  - `SUBJECT_DATABASE_SETTINGS = "cheladb.database_settings"` carrying `{ database_id }`;
  - `publish_branch`, `publish_database_settings`, `run_branch_consumer` and `run_settings_consumer`, following the existing helpers' shape;
  - both subjects added to `control_subjects()`;
  - the password reset RPC request gains `branch_id: BranchId` (the orchestrator sends main's for the old route). In the same task, `Worker::reset_password` takes `BranchId` instead of `DatabaseId`, `main.rs` passes it through, and the worker's reset tests are adapted, so the workspace builds.
- **Orchestrator:**
  - the handlers in `branches.rs`, using the metadata queries from Task 2;
  - the refusals and status codes exactly as in the spec's API section;
  - `restore` generates the backup id and name in the orchestrator and returns both;
  - `PATCH` validates the ranges and publishes the settings job when `history_window` changed;
  - `/v1/me` and `GET /v1/databases/{id}` gain `branch_count`.
  - `BranchSummary` JSON: `id, name, parent_branch_id, kind, status, host, port, branched_at_time, branched_at_lsn, created_at, error`. `host`/`port` are set only for a ready `normal` branch with an endpoint.
- **Types (TS):** `BranchSummary`, `CreateBranchRequest`, `RestoreRequest`, `DatabaseSettings`, and `branch_count` on database summaries.

**Tests** (`api.rs`, with the fakes extended):
- Each route's happy path.
- Each refusal: 404, the 409 cases (name taken, cap reached with its message, delete/reset/restore with children naming them, main refused for delete and reset-to-parent, parent/branch not ready, backup refused for reveal/reset-password/reset/restore), and the 422 cases (bad name, `main` as a name, a bad LSN, a bad timestamp, a history window or cap out of range).
- `old_routes_are_aliases_for_main`.
- `restore_returns_the_backup_name`.
- `patch_publishes_settings_only_when_the_window_changed`.
- The routing tests for the new endpoints.

- [ ] RED, implement, GREEN; `cargo test --workspace`; clippy.
- [ ] Commit `orchestrator: branch API, database settings and branch jobs`.

---

### Task 4: Worker: point resolution, create and delete

**Files:** create `services/worker/src/{point.rs,branch_create.rs,branch_delete.rs}`; modify `ports.rs`, `fakes.rs` and `lib.rs`.

**Produces:**
- **Ports:**
  - `Storage` gains `create_branch_timeline(tenant, new, parent, lsn)`, `delete_timeline`, `get_timeline` (head LSN), `lsn_by_timestamp`, `timestamp_of_lsn`, `detach_ancestor`, `patch_tenant_config`, `tenant_pitr_interval` and `list_timelines`, forwarding to Task 1.
  - `WorkerMetadata` gains the Task 2 branch methods, again calling by full path.
- **`point.rs`:** `pub async fn resolve_point(&self, tenant: &NeonId, source_timeline: &NeonId, at: Option<&PointInTime>) -> Result<Lsn, String>`, with the Global Constraints' messages, prefixed `resolving the point:`.
- **`branch_create.rs`:** `pub async fn handle_branch_create(&self, branch_id: BranchId, at: Option<PointInTime>) -> Result<(), String>`. `Err` only for metadata trouble, which retries the job.
  1. Claim with pending → provisioning, skipping other states as provision does.
  2. Resolve the point against the parent's current timeline.
  3. Record the endpoint and timeline id (`create_endpoint_for_branch`). The owner verifier comes from a new password, and the database name from main's `neon_databases`.
  4. `create_branch_timeline`.
  5. `computes.create`.
  6. Readiness (the database name, `SELECT 1`).
  7. Bootstrap SQL.
  8. Re-check, seal (AAD = branch id), then `set_branch_ready`.
  - Teardown and races follow `provision.rs`: `abandon` / `abandon_gone` / the tenant-leak recheck equivalent, which here checks that the endpoint row still exists after `create_branch_timeline`.
  - The teardown for a branch deletes the **timeline**, never the tenant.
- **`branch_delete.rs`:** `pub async fn handle_branch_delete(&self, branch_id: BranchId) -> Result<(), String>`.
  1. `begin_branch_delete`, which is fine if it's already deleting.
  2. For each backup and then the branch: compute (if an endpoint exists), `delete_timeline` for `timeline_id`, `pending_timeline_id` and `superseded_timeline_id` (404 is done), then the endpoint row.
  3. `mark_branch_deleted`.
  - A teardown error returns `Err`, and the row stays deleting.
- **Database delete (`delete.rs`)** now tears down every live branch (backups included) before `delete_tenant`:
  1. each branch's compute and endpoint row;
  2. `delete_tenant`;
  3. `mark_branch_deleted` for each branch;
  4. `mark_deleted` for the database.
  - Test: `database_delete_removes_every_branch_compute_first`.
- **Fakes:**
  - `FakeStorage` gains timelines per tenant, with an ancestor and a head LSN, and `lsn_by_timestamp` answers scripted per test.
  - `FakeMetadata` mirrors the Task 2 guards: the cap, the children and backup rules, and the switch transactions.

**Tests:**
- `create_branch_happy_path`: the order is endpoint < timeline(ancestor = the parent's timeline, the resolved LSN) < compute.create < `SELECT 1` < `DO $$` < set_branch_ready. The sealed password opens with AAD = branch id, and the endpoint verifier matches it.
- `create_at_each_point_kind`: present, future (head), past (the message has the oldest point), nodata, and an LSN ahead of the head.
- `a_failure_at_each_step_cleans_up_and_names_the_step`: the timeline is deleted, never the tenant.
- Review Focus 1: `a_delete_seen_at_the_recheck_tears_down`, `a_delete_racing_set_ready_tears_down`, and `a_delete_before_the_timeline_exists_leaves_no_timeline`.
- `delete_removes_backups_then_the_branch`, `delete_also_removes_pending_and_superseded_timelines`, and `a_failed_teardown_keeps_deleting`.

- [ ] RED, implement, GREEN: `cargo test -p worker`; clippy.
- [ ] Commit `worker: branch create and delete with point-in-time resolution`.

---

### Task 5: Worker: reset to parent and restore

**Files:** create `services/worker/src/branch_switch.rs`; modify `fakes.rs`.

**Produces:**
- `pub async fn handle_branch_reset_to_parent(&self, branch_id: BranchId) -> Result<(), String>`
- `pub async fn handle_branch_restore(&self, branch_id: BranchId, backup_id: BranchId, backup_name: &str, at: PointInTime) -> Result<(), String>` (`backup_name` comes from the job; Task 3 generates it)
- `pub(crate) async fn recreate_compute(&self, ep: &str) -> Result<(), String>`: a new token hash is written before `computes.create`, then readiness. Reconcile reuses it.

**Behavior:** follow the spec's two flows exactly.
- **Claim:** ready → resetting. The branch must be `normal`; reset-to-parent also requires not main. Re-check `live_children` in the worker; if there are any, fail with the refusal message and return to ready.
- **Reset-to-parent order:** `set_pending_timeline` < `create_branch_timeline(parent head)` < `computes.delete` < `switch_to_pending_timeline` < `recreate_compute` < `delete_timeline(superseded)` < `clear_superseded_timeline` < resetting → ready.
- **Restore order:** resolve the point against the branch's own timeline < `set_pending_timeline` < `create_branch_timeline(own, lsn)` < `detach_ancestor(new)` < `computes.delete` < `switch_to_restored_timeline(backup)` < `recreate_compute` < resetting → ready.
- **Failure before the switch:** delete the pending timeline, clear it, and go back to ready on the old timeline (the compute is recreated if it was already stopped). The error is stored, prefixed by the step, and the branch status is `ready`, so the branch is still usable. Store the failure with `set_branch_error` without failing the branch. A later successful reset or restore clears it with `set_branch_error(id, None)`.
- **Failure after the switch:** leave `resetting`, and reconcile finishes the flow (Task 6).
- **A delete landing mid-flow:** each metadata write is guarded, so a false result means "deleted meanwhile". Tear down the pending timeline and stop. The delete job owns the rest.

**Tests:**
- Each flow's journal order.
- `restore_keeps_the_endpoint_host_and_password`.
- `restore_creates_a_backup_that_does_not_count`.
- `reset_refused_for_main_backup_and_with_children`.
- `a_failure_before_the_switch_returns_to_ready_on_the_old_timeline` (one case per step).
- `a_failure_after_the_switch_leaves_resetting_for_reconcile` (Review Focus 2).
- `a_delete_during_reset_or_restore_leaves_no_pending_timeline` (Review Focus 1).

- [ ] RED, implement, GREEN; commit `worker: branch reset-to-parent and restore`.

---

### Task 6: Worker: reconcile and the history window

**Files:** `services/worker/src/{reconcile.rs,settings.rs}` and `fakes.rs`.

**Produces:**
- **`settings.rs`:** `pub async fn handle_database_settings(&self, db: DatabaseId) -> Result<(), String>` → `patch_tenant_config(neon_tenant_id, history_window)`, which is a no-op when the database has no tenant yet. Provision also calls it after `create_tenant`.
- **`reconcile.rs`:** `Snapshot` gains `branches: Vec<LiveBranch>`, `tenant_timelines: HashMap<String, HashSet<String>>` (from the `Storage` port's `list_timelines`, which forwards to Task 1) and `pitr: HashMap<String, Duration>` with its settings.
- **New `Action`s:**
  - `FailStaleBranch(BranchId)`;
  - `FinishSwitch(BranchId)`, for resetting older than 600 s: delete any pending timeline, `recreate_compute` if the compute is missing, delete any superseded timeline, then resetting → ready;
  - `RetryBranchDelete(BranchId)`;
  - `RecreateBranchCompute(String)`, merged with the existing `RecreateCompute`;
  - `ReportUnknownTimeline(String, String)`;
  - `PatchHistoryWindow(DatabaseId)`.
- The observe order is unchanged, with branches read alongside databases. A timeline that only a pending or superseded field names is known, not unknown.

**Tests:**
- One `plan` test per new row.
- `a_fresh_branch_is_left_alone`.
- `backups_have_no_compute_and_need_nothing`.
- `unknown_timelines_are_reported_never_deleted`.
- `finish_switch_after_a_crash_between_switch_and_delete` (Review Focus 2).
- `history_window_drift_is_patched` (including a tenant whose `pitr_interval` is unset, i.e. `None`).
- `settings_job_patches_the_tenant`.

- [ ] RED, implement, GREEN; commit `worker: reconcile branches and the history window`.

---

### Task 7: Wiring and live test

**Files:** `services/worker/src/main.rs`, `services/worker/tests/branch_live.rs`.

- **`main.rs`:** start `run_branch_consumer`, dispatching on `BranchJob` to the Task 4 and 5 handlers, and `run_settings_consumer`. The reset RPC uses `branch_id`.
- **Live test** (`#[ignore]`; needs `just deps-up` and `just dev`; run with `--test-threads=1 --nocapture`):
  1. Provision; write row A; note the time (sleep 2 s either side); write row B.
  2. Create a branch at the noted time: it has A and not B.
  3. Write C on main, then reset the branch to its parent: it has A, B and C.
  4. Restore main to the noted time: main has A and not B/C. The backup exists, has no compute, and a branch made from it has B and C.
  5. Try a point older than the history window, with the window patched to 1 h and a timestamp 2 h back: the error has the "oldest available point" message (Review Focus 5).
  6. Delete everything: no compute containers, no timelines (`get_timeline` 404), no endpoint rows, and every branch `deleted`.
  - Print each operation's time.
  - Clean up on panic.

- [ ] Implement, then run the live test. `cargo test --workspace`; clippy. Record the timings in the Status.
- [ ] Commit `worker: branch jobs wired; live branching story`.

---

### Task 8: `branch-e2e` and just

**Files:** create `scripts/branch-e2e.sh`; modify `justfile` and `infra/neon/tests/run.sh`.

- **`just branch-e2e`** runs the Task 7 story through the gateway API (token from `cheladb dev token`, as `neon-e2e.sh` gets it) and psql through the proxy, with `hostaddr=127.0.0.1` and `sslrootcert`.
  - Reuse `neon-e2e.sh`'s helpers (move shared ones into `scripts/lib/neon-common.sh` if needed).
  - A trap deletes the database unless `KEEP=1`.
- **`neon-test`** gains `branch-e2e recipe exists`.
- **`sweep-dbs`** also clears `branches`: `DELETE FROM branches` before `databases`. Its timeline cleanup is covered by tenant deletion.

- [ ] Write the new `neon-test` check first (RED), implement, GREEN. Live: `just dev`, then `just branch-e2e`, `just neon-e2e` and `just auth-e2e`.
- [ ] Commit `scripts: branch-e2e through the API and the proxy`.

---

### Task 9: Studio

**Files:**
- `apps/studio/src/lib/api.ts` (branch calls);
- `apps/studio/src/components/{branch-tree,branch-actions,new-branch-form,database-settings}.tsx` and their tests;
- `dashboard.tsx` (rows expand into the tree), `database-actions.tsx` (becomes per branch, or is reused by `branch-actions`).

**Behavior:**
- **The tree:** main first, children indented, backups marked. It shows status, host and "from `<parent>` at `<time or LSN>`", and polls while any branch is in a transitional state.
- **Branch actions:**
  - connection details and reset password (today's panel, called per branch);
  - reset to parent;
  - restore, with a `datetime-local` input limited to `now - history_window` .. now, converted to UTC;
  - delete, with a confirm.
- **Disabled actions** show a tooltip with the reason: "main can't be reset to a parent", "has child branches: …", or "restore backup: create a branch from it to connect".
- **The new-branch form:** name (validated as in the Global Constraints), a parent select (default main), and a point (now or a time).
- **Settings:** history window (hours or days) and cap, with the note "Shrinking the window lets older history be removed."
- **Errors** show the API's message.

**Tests:**
- vitest for:
  - tree ordering and indentation;
  - each action's disabled reason;
  - the form's validation;
  - the restore picker's bounds;
  - settings validation;
  - the API calls' paths and bodies.
- Playwright against `just dev`:
  1. sign up;
  2. create a database;
  3. create a branch;
  4. reveal it (a different host from main);
  5. restore main to a minute ago (the backup appears);
  6. reset the branch to its parent;
  7. delete the branch and then the backup.
  - Take a screenshot per step into the scratchpad.

- [ ] RED, implement, GREEN. Run `pnpm --filter @cheladb/types typecheck`, `pnpm --filter @cheladb/studio typecheck`, `test` and `build`, then the Playwright pass.
- [ ] Commit `studio: branch tree, actions and database settings`.

---

### Task 10: Docs

- **README:** a "Branches" section covering what a branch is, its own password and compute, the cap, the history window, restore (the backups and the unchanged host), reset to parent, the children rule, and `just branch-e2e`.
- **Roadmap:** step 5 progress, "Where we are", and "Resume here".
- The full verification run happens after the end-of-step fix batch, as in step 4.

- [ ] Commit `docs: step 5 — branching`.

Then comes the end-of-step review (opus, whole branch), one fix batch with a scoped re-review, the full run, and a pause for the merge go-ahead.
