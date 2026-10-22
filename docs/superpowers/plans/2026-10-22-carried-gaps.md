# Carried gaps: storcon's transient refusals and the leaked computes (implementation plan)

**Goal:** the worker rides out the storage controller's few seconds of "no schedulable pageserver" after a pageserver restart (tenant create and delete) and its "tenant Attaching" 503 (timeline create), instead of failing for good; and a compute the worker recreates but that never becomes ready no longer stays behind crash-looping, reconcile notices a Ready branch whose tenant is gone and a compute container that exists but isn't running, and a dropped compute create removes its container.

**Spec:** none (carried gaps from `docs/superpowers/plans/2026-10-21-neon-fork-round-4.md`, Status; the investigation report is summarised in "Pre-flight scan" below; user rulings R1–R5).

**Architecture:** `crates/chelabase-neon` (`storcon.rs`: the retry; `backend.rs`: the drop guard and the non-running listing), `services/worker` (`branch_switch.rs`: the recreate cleanup; `reconcile.rs`: the missing-tenant report and the non-running recreate), the worker's live tests (`tests/branch_live.rs`, `tests/common/mod.rs`).

**Tech:** Rust; `tracing` added to `chelabase-neon` from the workspace (no new crate). No fork change: no image build; the live pass runs on the pinned images.

**Standing rule:** each task runs the task pipeline (CLAUDE.md "Checkpoints"): test agent (RED commit `tN: RED tests`, run against the base) → coder (GREEN, RED files untouched) → audit (the task's reviewer). T3 is a leaf of test code: one agent, no review.

## Status

- **RESUME HERE (2026-10-22): step tested; paused for the user's go to merge `carried-gaps` into `main`.** Stack `carried-gaps` up (purge after the merge).
- **End of step:** whole-branch review (opus) PASS with I1, I2, M1, M2 + hardening; final fix round F-storcon `00530bf1`/`66de0763` + F-acks `c547cd48` (merged `746f8a36`), F-reconcile `8e248428`/`2f9aeb88` (merged `4d364057`); live pass 1 found the post-restart 404 → T4. **Done: T4** (red-team opus; opus/opus/sonnet): RED `95aaa8ba`, GREEN `026bd544` (coder deviations accepted: the consumer delete gets 1 s past the deadline; test-only `pin_tracing_dispatchers` fixes a tracing-core callsite-cache flake; `allow(large_enum_variant)` on `AttachSource`), audit PASS, no defects (tidy `c009ac03`); merged `3dc4b611`. `just check` 3858/3858. **Live pass 2** (after `up`, which restarted NATS for the new ACL): `attach_events_live` 6/6, `permissions_live` 22/22, `reply_forgery_live` 2/2, `branch_live` 5/5 (incl. `provision_right_after_a_pageserver_restart_succeeds`), `neon_live` 1/1, `storcon_live` 1/1, `neon-e2e` and `branch-e2e` PASS; provisions ~7.4 s, no attach timeouts. From pass 1 (unaffected by T4): `compute_fork_live` 11/11, `docker_backend_live` 3/3 (after `just create-db-demo`).
- **Listed for the user, not done:** T4 audit hardening: (1) a consumer create that times out at the deadline isn't deleted (the server drops it after 30 s); (2) an instant empty pull could spin until the deadline; (3) a stream-not-found or permission-denied consumer create retries for the full 60 s instead of giving up early (e.g. a stack with a stale ACL stalls each provision 60 s, then R8 proceeds); (4) persistent pull failures after one create report `TimedOut` rather than `Unavailable`; (5) the NATS restart in `up` briefly disconnects running apps (they reconnect; in-flight jobs are redelivered); (6) attach events expire after 1 h. Also: Valkey's `users.acl` has the same single-file-bind problem as NATS's `auth.conf` (R9 covers NATS only); per-endpoint backoff for a never-ready compute; P12 (a branch create after a pageserver lost its volume).
- **Done: T1** (sonnet/sonnet/sonnet): RED `217d0157`, GREEN `b6c26313`, audit PASS, no defects (tidy `fe3bc8d8`); merged `60835aaa`.
- **Done: T3** (sonnet, no review): `ba091d88` (coordinator checked the diff); merged `504b3ee4`.
- **Done: T2** (opus/opus/sonnet): RED `b7821650`, GREEN `54107936`, RED test fix `eae6c75f` (haiku), audit PASS, no defects (tidy `6455e328`); merged `c14b3a69`. Coder deviation D2 (accepted, audited): `actions_are_sorted` and the last case of `a_tenant_whose_history_window_drifted_is_patched` updated for R3. **Process failure:** a RED test bug (D1: the 4th case of `a_ready_branch_whose_tenant_is_missing_is_reported_not_recreated` reused another case's `dev`) surfaced only at GREEN, since the test could only fail to compile before.
- **Deferred to the final fix round (T2 audit):** H4 `reconcile.rs:1799` over-long `assert_eq!` line. Listed to the user, no change: a tenant lost mid-provision isn't reported (R3 covers Ready only; the stale-database rules catch it); `RemoveOnDrop`'s spawned removal is lost if the runtime shuts down at once (bounded: R5 recreates the stopped container next pass).
- **Deferred to the final fix round (T1 audit hardening):** H1 `storcon.rs` `retry_transient`: the last attempt can overrun the budget by a whole request timeout (`create_timeline` up to 120 s), beyond the ~240 s the `ACK_WAIT` comment counts: cap the last attempt's timeout at the remaining budget. H2 `delete_until_gone`: when the 409 never clears, the error loses the last 409 text: keep it. H3 `jitter`: redundant `.max(1)`, tiny modulo bias.
- `main` green at the step's start: `just check` 3750/3750 Rust tests (2026-10-22 09:05).
- **R1 (user, 2026-10-22):** retry both storcon "can't schedule" 409s ("No pageserver found matching constraint", "No pageservers found"). Cost if wrong: a provision on a permanently unschedulable stack fails ~30 s later than today.
- **R2 (user, 2026-10-22):** timeline create also retries the 503 "Timed out waiting … for tenant active state. Latest state: Some(Attaching)" with the same backoff and budget. Cost if wrong: same.
- **R3 (user, 2026-10-22):** a Ready branch whose tenant is missing from storcon is report-only (logged each pass, no compute recreate); metadata stays Ready. Cost if wrong: an operator must notice the log.
- **R4 (user, 2026-10-22):** include the cancel-safe compute create (drop guard).
- **R5 (user, 2026-10-22):** include recreating a compute whose container exists but isn't running.
- Branch `carried-gaps`, plan `<sha>`.

## Plan rulings (mechanical, listed for the user)

- **P1** The retry lives in `StorconClient` (below the worker's `Storage` port), so every caller (provision, delete, reconcile's teardown, the test helpers that call storcon) gets it; the worker's own code doesn't change for gap 1. Cost if wrong: none.
- **P2** No live suite runs during the tasks (T1 and T2 are covered by mock and fake tests in `just check`); the plan's one stack comes up in `.worktrees/carried-gaps` at the end-of-step live pass only. Cost if wrong: a defect only a live run shows is found at the end.
- **P3** Fix 1 removes the container only on a readiness failure of this run's own create; a compute that became ready and then failed a later step (bootstrap, resync) is kept (the existing test at `branch_switch.rs:2738-2759` expects that). Cost if wrong: none.

- **P4** No separate red-team or seam pass: the opus investigation's edge-case checklists (in each task below) are the red-team input for T2's concurrency core, and the only cross-task seam is T3 relying on T1's retry (one predicate, pinned). Cost if wrong: a seam defect surfaces at the end-of-step review.

- **P5** (T1 coder's deviation, accepted 2026-10-22) The storcon retry backoff is equal jitter (uniform in [ceiling/2, ceiling]) instead of full jitter: full jitter's near-zero draws made the RED cancellation tests flaky (7/25). Cost if wrong: retries are spread slightly less.

- **P6** (end-of-step review I2) A tenant create must survive storcon's own reconcile wait (`RECONCILE_TIMEOUT` 30 s, `service.rs:116,2622`): only `create_tenant` (idempotent, a lost success included) also retries a request timeout, within a bounded total; the `ACK_WAIT` comment states the real worst case and it stays under 300 s with margin. Cost if wrong: a provision on a slow storcon waits longer before failing.

## Final fix round (end of step 2)

Two batches, run in parallel, each a sonnet test agent (RED) then a sonnet coder; the coordinator verifies each fix diff.
- **F-storcon** (`crates/chelabase-neon/src/storcon.rs`, `tests/storcon_mock.rs`, `crates/chelabase-nats/src/lib.rs`, `services/worker/src/config.rs` if the timeout moves): I2/P6; M1 the `ACK_WAIT` comment's numbers (create_timeline uses `CREATE_TIMELINE_TIMEOUT` 120 s) with H1 (the last attempt never overruns the budget by a whole request timeout); M2 an exhausted error keeps its class for both predicates (the appended note must not break `is_unschedulable`); H2 an exhausted delete keeps the last 409 text; H3 the redundant `.max(1)` in `jitter`; document that `FinishSwitch` timeline creates can lengthen a reconcile pass.
- **F-reconcile** (`services/worker/src/reconcile.rs`, `crates/chelabase-neon/src/backend.rs`): I1 re-check `computes.status(ep)` right before a not-running `RecreateBranchCompute` acts and recreate only if still exited/dead/restarting or absent, fix the "No job is racing it" comment, and test it with a fake that recovers between observe and act; H4 the over-long `assert_eq!` (`reconcile.rs:1799`); comments: the drop guard's uncovered windows (a drop during `create_container` itself or the name-conflict retry), R5 also recreating a compute an operator `docker stop`ped.
- **R6 (user, 2026-10-22):** F-storcon's per-attempt cap cut timeline create from 120 s to 45 s; instead, timeline create gets its own 120 s budget (the old limit; Attaching retries share it), and long-running NATS jobs send in-progress acks so `ACK_WAIT` (300 s, unchanged) bounds a job's silence, not its total time. **Correction (coordinator, same day):** the job runner already sends in-progress acks every `ack_wait / 3` while a handler runs (`crates/chelabase-nats/src/lib.rs:1183-1199`), so no runner change is needed; the coordinator's question to the user wrongly said none were sent. Batch **F-acks** is mechanical (one sonnet agent, coordinator-verified), on `carried-gaps-fs` after F-storcon: `crates/chelabase-neon/src/storcon.rs` (a 120 s default budget for `create_timeline`, with a test) and the `ACK_WAIT` comment (it bounds silence; the in-progress acks keep a long job's delivery). Cost if wrong: none.
- Listed to the user, not done: per-endpoint backoff for a compute that never gets ready (each serial pass can spend up to 60 s per such compute).

## Live pass 1 (2026-10-22) and T4

Live pass 1 on `746f8a36` (stack `carried-gaps`): `neon_live` 1/1, `compute_fork_live` 11/11, `storcon_live` 1/1, `neon-e2e` and `branch-e2e` PASS. `docker_backend_live` 2/3: `admin_sql_on_a_running_compute` needs a running compute and the fresh stack had none (precondition, not a regression; rerun after `just create-db-demo`). `branch_live` 2 passed, 1 failed, 2 cancelled: the new `provision_right_after_a_pageserver_restart_succeeds` got `creating timeline: storage controller rejected the request (404 Not Found): NotFound: 1 (pageserver): NotFound: tenant <id>`: tenant create succeeded, but the pageserver hadn't attached the new tenant yet.

- **R7 (user, 2026-10-22):** fix it in this step, event-driven over **JetStream**, with no stopgap retry of the 404: the storage controller already calls the orchestrator's `PUT /storcon/notify-attach` (`services/orchestrator/src/internal/hooks.rs:42`) when a tenant's shards are attached; the orchestrator publishes that as a JetStream event, and provisioning waits for the new tenant's attach event before creating its timeline.

### T4 spec: the tenant-attached event (draft; the red-team pass turns its open points into rulings)

1. **Publish (orchestrator):** `notify_attach` publishes to JetStream (subject e.g. `chelabase.storage.attached.<tenant_id>`, a stream whose retention keeps events long enough for a provision to read one published before it subscribed) and waits for the publish ack before answering. A failed publish answers 503, which the storage controller retries, so no attach goes unrecorded. The existing compute reconfigure is unchanged.
2. **Wait (worker, provision):** after `create_tenant` succeeds, before `create_timeline`, the worker waits for an attach event for that tenant, reading the stream from the start for that subject (so an event published before the wait began is seen), bounded by a timeout.
3. **Open points for the red-team:** whether the storage controller calls the hook on a fresh tenant create at all, and when relative to its 201 (fork `storage_controller/src/compute_hook.rs`, `service.rs`); what the hook carries (generation, shards) and whether "attached" there means the pageserver will answer a timeline create; duplicate and stale events (redelivery, a re-attach after a later restart); what a timeout does (fail the provision, or create the timeline anyway); the orchestrator down or restarting while the hook fires; NATS down; the stream's retention and limits; who creates the stream (the workbench, the orchestrator, `chelabase-nats`'s stream setup) and its ACL (the NATS users per service: `infra/compose` nats `auth.conf`, `tools/cli/src/workbench/service_auth.rs`); whether branch create (a timeline on an existing tenant) needs it too or keeps T1's Attaching-503 retry; which tests can prove it deterministically (unit fakes, the storcon mock, a live test).

### T4 red-team (opus, 2026-10-22): findings and the design (binding for T4)

**Cause of the live 404** (storcon log): the create's reconcile was cancelled by the restarted pageserver's re-attach upcall (generation bumped to 2, node WarmingUp); `tenant_create` swallows a failed or timed-out reconcile (`storage_controller/src/service.rs:2622-2627`) and answered 201 14 ms later; the timeline create was routed to a pageserver that never got the location. The attach (and its notify) would have come ~4.6 s later, at the next heartbeat.

**Storcon facts:** `notify-attach` is sent once per fresh tenant after its first successful `location_config` (`reconciler.rs:931-934`, `compute_hook.rs:780-807`), normally ~2 ms **before** the 201, so the worker must read history; a re-attach to the same node sends nothing (Noop when unchanged, `compute_hook.rs:478-487`; the body has no generation); a storcon restart re-notifies every tenant; body `{tenant_id, preferred_az, stripe_size, shards:[{node_id, shard_number}]}` (`:327-341`); retries 503/502/504/429 and transport errors ~10× (≈17.5 s), then `reconcile_all` every 20 s indefinitely. A notify follows a successful attach, so the timeline create won't 404 (it may still 503 Attaching: T1's R2 retry).

**Rulings:**
- **R8 (user):** on a wait timeout, log a warning and call `create_timeline` once anyway (the event gates, the timeline create stays the authority; not a 404 retry).
- **R9 (user):** `write_auth_files` reports whether `auth.conf` changed, and `workbench up` restarts NATS when it did (the single-file bind keeps the old inode, `service_auth.rs:410-428`, compose ~:188-192).
- **R10 (user):** only the orchestrator creates and updates the new stream; a missing stream is "unavailable" to the worker (R8 applies).
- **P7** Names: stream `CHELABASE_NEON_EVENTS`, subject `chelabase.neon.attached.<tenant>` (`fn tenant_attached_subject(tenant: &str) -> Result<String, NatsError>`: 32 lowercase hex only), `TenantAttachedEvent { tenant_id: String, shards: Vec<{ node_id: u64, shard_number: u8 }>, stripe_size: Option<u32>, preferred_az: Option<String> }`, all in `chelabase-nats`; never in `control_subjects()`.
- **P8** Stream: Limits retention, File storage, `max_messages_per_subject` 1, `max_age` 1 h, `max_messages` 100 000, `max_bytes` 64 MiB, discard Old, replicas 1, no `Nats-Msg-Id` dedupe. `ensure_neon_events_stream` (orchestrator, after `ensure_control_stream`) creates it and fixes config drift.
- **P9** ACL (least privilege): orchestrator publish `chelabase.neon.attached.*` + `$JS.API.STREAM.{INFO,CREATE,UPDATE}.CHELABASE_NEON_EVENTS`; worker publish `$JS.API.CONSUMER.CREATE.CHELABASE_NEON_EVENTS.*.chelabase.neon.attached.*`, `$JS.API.CONSUMER.MSG.NEXT.CHELABASE_NEON_EVENTS.*`, `$JS.API.CONSUMER.DELETE.CHELABASE_NEON_EVENTS.*` (replies on its `_INBOX_worker.>`), no publish on attached subjects; realtime nothing; runner the union plus deny `$JS.API.STREAM.DELETE.CHELABASE_NEON_EVENTS` (and purge).
- **P10** Publish: an orchestrator port `AttachEvents { fn tenant_attached(&self, TenantAttachedEvent) -> BoxFuture<'_, anyhow::Result<()>> }`, field `attach_events: Arc<dyn AttachEvents>` on `InternalState`, implemented for `NatsPorts`. `notify_attach` parses `tenant_id` as `NeonId` (A3: a bad value is 503, no publish; `shards` and the rest `#[serde(default)]`), then runs the publish (awaiting the JetStream ack) and `reconfigure_all` **concurrently under one deadline** taken at hook entry (A2), publishing regardless of endpoints or metadata errors (A4); 200 only if both succeed, else 503.
- **P11** Wait: `chelabase_nats::wait_tenant_attached(js: &jetstream::Context, tenant: &str, timeout: Duration) -> Result<Option<TenantAttachedEvent>, NatsError>`: a named pull consumer `attached-<tenant>-<8 random hex>` (A5) filtered to that subject (A6), `DeliverPolicy::LastPerSubject`, `AckPolicy::None`, memory storage, `inactive_threshold` 30 s, short repeated fetches (expires ≤ 5 s, 1 message) until the deadline, NATS errors retried within it, then a best-effort delete. Worker port `Storage::wait_attached(&self, tenant: &NeonId, timeout: Duration) -> impl Future<Output = AttachWait> + Send`, `enum AttachWait { Attached, TimedOut, Unavailable(String) }`; `RealStorage` gets `attach: AttachSource { Nats(jetstream::Context) | Disabled }` passed to `build` explicitly (A8: no silent default); `Worker.attach_timeout` 60 s; placed in `provision.rs` right before `create_timeline` (after the settings PATCH), `phase_done("wait_attached")`. Branch create never waits (no event on re-attach).
- **P12** Out of scope, listed for the user: a branch create right after a pageserver lost its local volume could 404 until the next heartbeat.

**Edge-case checklist** (test and coder agents cover each): chelabase-nats live (runner, random tenants): an event published before the wait returns at once, one published 500 ms in returns, another tenant's event times out at ~the timeout, two concurrent waiters both see it, the consumer count returns to baseline, two publishes leave one message, ensure is idempotent, fixes drift and leaves `CHELABASE_CONTROL` alone; subject fn rejects upper case, `.`, `*`, `>`, empty, 31/33 chars (unit). `permissions_live`: orchestrator publish acked, worker and realtime publishes refused; worker may create a consumer filtered to `attached.<t>` but not an unfiltered or other-filtered one; orchestrator and realtime can't create consumers; nobody can delete or purge the stream. CLI `service_auth` tests: the exact new entries; the worker's publish list has no attached subject; the runner denies the stream delete. Orchestrator (paused clock): one publish per call with tenant and shards; 200 only after the ack; publish failure → 503 even with no endpoints, reconfigures still run; a hung publish → 503 within 10 s without delaying reconfigures; invalid `tenant_id` → 503, no publish; failed metadata lookup still publishes then 503; the token check still precedes the body. Worker (fakes): journal order `create_tenant` < `wait_attached` < `create_timeline`; no wait when `create_tenant` fails; `TimedOut` and `Unavailable` → warning, timeline tried once (R8); `create_branch_timeline` paths never wait; EndpointGone and delete-during-wait unchanged. Workbench: a changed `auth.conf` restarts NATS, an unchanged one doesn't.

**Files:** `crates/chelabase-nats/src/lib.rs`, `crates/chelabase-nats/tests/{permissions_live.rs, attach_events_live.rs (new)}`; `services/orchestrator/src/internal/{hooks.rs,mod.rs}`, `services/orchestrator/src/main.rs`, `services/orchestrator/tests/internal.rs`; `services/worker/src/{ports.rs,fakes.rs,provision.rs,lib.rs,config.rs,main.rs}` and the test modules that call `build` (`rt1_tests/mod.rs`, `rt1_tests/config.rs`, `f4_tests/config_ca.rs`, `rt_events/mod.rs`), `services/worker/tests/common/mod.rs`, `services/rest/tests/rest_live.rs`; `tools/cli/src/workbench/{service_auth.rs,r11_tests.rs,r11_edge_tests.rs,commands.rs}`; the `ACK_WAIT` comment.

**Live runs:** the coordinator refreshes the stack first (`workbench up` from `.worktrees/carried-gaps` after T4 merges: rebuilt apps, NATS restarted by R9). Then: `attach_events_live`, `permissions_live`, `reply_forgery_live`, whole `branch_live`, `neon_live`, `storcon_live`, `neon-e2e`, `branch-e2e`, and `just check` once before them.

## Task table (binding)

| Task | Scope | Test agent | Coder | Reviewer (audit) | Wave | Depends |
|---|---|---|---|---|---|---|
| T1 | storcon retry (R1, R2) | sonnet | sonnet | sonnet | 1 | — |
| T2 | compute leak: recreate cleanup, missing tenant, non-running recreate, drop guard (R3–R5) | opus | opus | sonnet | 1 | — |
| T3 | live tests: drop the test-side 409 loop, cleanup on failure, a provision right after a restart | — | sonnet | none (end-of-step review) | 2 | T1 |
| T4 | the tenant-attached JetStream event (R7); red-team (opus) first | opus | opus | sonnet | 3 | T1–T3 |

T1 and T2 touch disjoint files except `crates/chelabase-neon/Cargo.toml`, where both may add the identical `tracing.workspace = true` line (merges cleanly). Each runs in its own worktree (`just wt-add carried-gaps-tN carried-gaps`).

## Shared interfaces (pinned)

- `StorconError::is_unschedulable(&self) -> bool`: `Rejected { status: 409, message }` where `message` starts with `"Conflict: "` and ends with `": No pageserver found matching constraint"` or `": No pageservers found"`. Every other 409 is final.
- `StorconError::is_tenant_attaching(&self) -> bool`: the 503 whose message contains `"tenant active state"` and `"Attaching"`. Today a 5xx becomes `StorconError::Other`; T1 may add a variant carrying a 5xx's status and message, but every existing caller's behaviour for any other 5xx stays the same (22 `StorconError::Other` match sites).
- `StorconClient::with_schedule_retry(self, base: Duration, cap: Duration, budget: Duration) -> Self`, in the style of `with_delete_poll`; default base 250 ms, cap 2 s, budget 30 s; full jitter (`getrandom`, already a dependency), the shape of the fork's K9 `retry_on_serialization_failure`.
- `ComputeBackend::list_not_running(&self) -> impl Future<Output = anyhow::Result<Vec<String>>> + Send`: endpoint ids whose compute container exists in Docker state `exited`, `dead` or `restarting` (never `created`, which an in-flight create passes through). `list()` is unchanged.
- `reconcile` action `ReportMissingTenant { database: <id type used by reconcile> }`: report-only.

## Pre-flight scan (2026-10-22, from the opus investigation)

- **Where the 409 comes from:** storcon marks a re-attached pageserver `WarmingUp` until its next heartbeat (5 s; at most 300 s) (`chelabase-neon/storage_controller/src/service.rs:2518-2519`, `node.rs:232`, `service.rs:137,140`). Bodies: create `{"msg":"Conflict: Failed to schedule shard(s): No pageserver found matching constraint"}` (`service.rs:2773-2776`); delete `{"msg":"Conflict: Scheduling error: No pageserver found matching constraint"}` (`:3680`).
- **Calls that can hit it:** tenant create (`services/worker/src/provision.rs:250-253`) and every tenant delete through `delete.rs:80-88` (handle_delete, provision's teardown, reconcile's `retry_teardown` `reconcile.rs:848-879`, `RetryDelete` `:694`), plus test helpers `services/worker/tests/common/mod.rs:128`, `services/rest/tests/rest_live.rs:122`. Timeline calls don't schedule; timeline create gets the Attaching 503 instead.
- **Today:** `check()` → `StorconError::Rejected` (`crates/chelabase-neon/src/storcon.rs:509-525`); `create_tenant` (`:196-207`) and `delete_until_gone` (`:481-506`, `_ => check(resp).await?` at `:500-502`) return it at once. `backend.rs:511`'s 409 is Docker's name conflict, unrelated.
- **Idempotency:** tenant create is idempotent on retry, a lost success included (`service.rs:2690-2734`, `:2817-2834`); the worker always sends the same body. Delete is idempotent (`:3648-3683`); a gone tenant is 404 = success (`storcon.rs:498`). Both hold a per-tenant lock.
- **The leak:** `recreate_compute_inner` (`services/worker/src/branch_switch.rs:367-424`) creates the container, `wait_until_ready` polls a `restarting` container for `READY_TIMEOUT` (60 s, `provision.rs:20`; it stops early only on `exited`/`dead`, `provision.rs:329-344`) and returns `Err` without removing it (`branch_switch.rs:390`). Reconcile recreates only an absent container (`reconcile.rs:242-245`), the restart policy is `unless-stopped` (`backend.rs:184-187`), and the orphan sweep only removes computes with no endpoint row (`reconcile.rs:317-322`, `:736-748`). Comments that promise a retry: `branch_switch.rs:1224-1225`, `:1236-1238`, `:536-537`; `reconcile.rs:699-700`. Reconcile never checks a Ready database's tenant against `s.tenants`.
- **Handled already:** provision and branch-create failures tear down first (`provision.rs:176-179`, `delete.rs:92-102`, `branch_create.rs:186-197`, `:528-547`); `DockerBackend::create` removes its container when a later step fails (`backend.rs:520-549`, test `crates/chelabase-neon/tests/docker_backend_cleanup_mock.rs:99`) but not when its future is dropped (R4).
- **Tests' own leak:** `services/worker/tests/branch_live.rs:760-776` loops on the 409 around `handle_delete` and panics outside the spawned task, skipping `cleanup()`; `common::cleanup` (`tests/common/mod.rs:118-133`) follows only `branch.endpoint_id`, and `branches.endpoint_id` is `ON DELETE SET NULL`, so unlinked `neon_endpoints` rows remain.

---

## T1: storcon retry (sonnet / sonnet / sonnet)

**Files:** `crates/chelabase-neon/src/storcon.rs`, `crates/chelabase-neon/Cargo.toml` (`tracing`), `crates/chelabase-neon/tests/storcon_mock.rs` (test agent), `crates/chelabase-nats/src/lib.rs` (`ACK_WAIT` doc comment, `:1046-1058`: add the 30 s create budget to its worst case).

**Behaviour:** R1, R2 and the pinned interfaces. `create_tenant` retries an unschedulable 409 with full-jitter backoff within the budget, each attempt under the existing request timeout. `delete_until_gone` treats an unschedulable 409 like a 202 (sleep the poll interval, ask again) inside its existing deadline. `create_timeline` retries the Attaching 503 within the same budget. Each retry logs `info!` (tenant, attempt, elapsed, message); exhaustion logs `warn!` and the returned error says "after N attempts in Xs" and keeps the original message.

**Edge cases (test agent: cover each):** 409×k→201; lost success (409 then 201 on the same tenant); delete 409×k→404 and →200; "No pageservers found" retried; a non-scheduling 409 (`"tenant exists"`, `"has children"`, "policy Detached") never retried; a permanent unschedulable 409 fails after the budget with the original text; the delete deadline bounds a slow 409 answer (shape of `storcon_mock.rs:329`); delete answers alternating 202/409 share one deadline; the Attaching 503 retried and any other 503 not; cancellation during a sleep leaves nothing half-done (each attempt is one request).

**Verify:** `cargo test -p chelabase-neon --test storcon_mock`, `cargo clippy -p chelabase-neon --all-targets -- -D warnings`.

**Live runs:** none (P2).

---

## T2: the compute leak (opus / opus / sonnet)

**Files:** `services/worker/src/branch_switch.rs`, `services/worker/src/reconcile.rs`, `services/worker/src/fakes.rs` (if the fakes need a non-running state), `crates/chelabase-neon/src/backend.rs` (`list_not_running` on `DockerBackend` and `FakeBackend`, the drop guard), `crates/chelabase-neon/tests/docker_backend_cleanup_mock.rs` (test agent); worker unit tests in the files' test modules (test agent).

**Behaviour:**
1. `recreate_compute_inner`: when `wait_until_ready` fails for this run's own create, remove the container (best effort, logged), so the next reconcile pass's `RecreateBranchCompute` really retries (P3). Fix the four misleading comments.
2. Reconcile: a Ready branch whose database's `neon_tenant_id` isn't in `s.tenants` (the listing succeeded) plans `ReportMissingTenant` (R3) instead of `RecreateBranchCompute`/`UpgradeCompute`; it logs an error naming the database, tenant and branch each pass and touches nothing.
3. Reconcile: a Ready branch whose compute is in `list_not_running` (R5) is recreated like an absent one (the existing remove-then-create path), unless a job for that branch is in flight (follow how reconcile already avoids racing in-flight creates).
4. `DockerBackend::create`: a drop guard that, if the future is dropped after `create_container` and before `Ok`, spawns the container's removal (R4).

**Edge cases (test agent: cover each):** cleanup never removes a container another job owns (only after this run's own create; a racing recreate of the same endpoint swaps the token first, check the token is still ours if the code allows); a compute that reports a live state before `READY_CAP` is not removed (`provision.rs:345-354`); a delete beginning meanwhile (`gone_meanwhile`, `branch_switch.rs:1201-1215`) removes once, not twice; a missing tenant when the storcon listing failed is never reported (`observe` errors, `reconcile.rs:489-493`); with `CHELABASE_WORKER_REMOVE_ORPHANS=off`, fixes 1 and 3 still apply (they remove the worker's own compute, not orphans): state that rule in a comment; static computes (`schema_base.rs:462`) and rehearsals keep their own rules; a container in Docker state `created` is never treated as not running; the drop guard does nothing after `Ok` and after the existing error-path cleanup already ran.

**Deterministic reproduction (test agent):** worker fakes (`services/worker/src/fakes.rs`, `FakeBackend` at `backend.rs:615-967`): a Ready branch, its compute removed, `computes.sql_failures_left = u32::MAX`, short ready timeout → `reconcile_once` → the container is gone (today it stays listed); a second pass plans `RecreateBranchCompute` again; a database whose tenant isn't in the fake storage → `ReportMissingTenant`, no `compute.create` in the journal; a non-running container → recreated. Drop guard: `docker_backend_cleanup_mock.rs` with a start that hangs, the `create` future dropped by a timeout, the engine sees `DELETE /containers/{id}`.

**Verify:** `cargo test -p chelabase-worker` (confirm the package name), `cargo test -p chelabase-neon --test docker_backend_cleanup_mock`, clippy on both with `-D warnings`; `cargo test -p chelabase-neon --test docker_backend_live --no-run`.

**Live runs:** none during the task (P2); `docker_backend_live` runs in the end-of-step live pass.

---

## T3: the live tests (sonnet, no review)

**Files:** `services/worker/tests/branch_live.rs`, `services/worker/tests/common/mod.rs`.

**Behaviour:** `branch_live.rs:760-776`'s 409 loop becomes a plain `handle_delete(id).await.expect(..)` (T1's retry covers it); every failure path in that test runs `cleanup` before panicking; `common::cleanup` removes the computes and `neon_endpoints` rows of every endpoint whose tenant is the database's (not only `branch.endpoint_id`). Add one live test in `branch_live.rs`: a provision (tenant create) right after a pageserver restart succeeds (T1's create retry), using the file's existing restart helpers.

**Verify:** `cargo test -p chelabase-worker --test branch_live --no-run`, clippy.

**Live runs:** `branch_live` in the end-of-step live pass only.

---

## End of step

0. `main` green at the start (done).
1. Whole-branch review (opus) on `main..carried-gaps`; it also covers T3.
2. The one final fix round: the review's findings plus any task finding deferred to it.
3. `just check`, once.
4. No fork changed: no integration gate, no image build.
5. Live pass on the plan's one stack (`just workbench up` in `.worktrees/carried-gaps`): `just workbench test` for `branch_live`, `neon_live`, `storcon_live`, `docker_backend_live`, `compute_fork_live`; `just neon-e2e`, `just branch-e2e`.
6. Update `docs/landing-highlights.md` on `main`, this Status and the roadmap's "Resume here", then pause for the user before merging.
