# Storage leak on delete: plan (2026-10-03)

Branch `storage-leak`, worktree `.worktrees/storage-leak` (created without a stack slot). A bugfix, decided with the user in chat: no separate spec.

## Status

- **Current:** done; merged into `main` on 2026-10-03 (user's D1 answer: merge now, fork fix after). Follow-up: the storcon-row fix below.
- **Decided by the user (2026-10-03):**
  - **Approach A:** add `--timelines-onto-safekeepers --timeline-safekeeper-count=1` to the storage controller. No worker code change.
  - **Landing:** after `live-queue` (which replaces the Compose files with one file and resets the shared Neon volumes, its L8) merges into `main`; the live test runs through `just live`.
  - Rejected: B (the worker deletes on each safekeeper: more code, safekeeper port 7676 not published, duplicates the controller); A+C (a reconcile GC pass: a race with creates; kept as a possible later safety net).
- **Done:**
  - research (opus, read only): see "Root cause".
  - Task 1 (sonnet implementer, sonnet review): `f101bdf` (flags, `scripts/storage-leak-e2e.sh` + `just storage-leak-e2e`, READMEs), review fixes `277acf3`. RED on the old flags (a reset-to-parent timeline stayed on safekeeper1 after 60 s); GREEN 35 s. Gate: `just test` 1075/1075; `live` neon-e2e, branch-e2e, data-e2e, role-e2e passed.
  - Review (Approved with fixes): Important, the S3 check was vacuous (partial-segment upload waits 15 min), fixed with an S3 marker per timeline, asserted listed before each delete and gone after (or still there for kept timelines). Minors 2, 3, 5 fixed in the same batch.
  - **Rulings:** restore keeps the old timeline as the backup, so reset-to-parent is the superseded-timeline case (restore checks that both stay). *Cost if wrong: none, both paths are covered.* The minors were fixed with the Important (one task, same files) rather than after the end-of-step review. *Cost if wrong: none.* The S3 marker check is proven only on the flag-on stack (a RED needs reverting the flags); the delete path is cited in the script (fork `safekeeper/src/http/routes.rs:74-94,222-240`, `safekeeper_reconciler.rs:403,417`). *Cost if wrong: an S3 leak could go unnoticed by the test.*
  - End-of-step review (opus): ready to merge with fixes. I1: the flags expose a ChelaDB/neon bug: `tenant_delete_safekeepers` (`storage_controller/src/service/safekeeper_service.rs:683`) schedules the in-memory request with generation 1 but persists the pending op with `i32::MAX`, so `remove_pending_op` never matches; each database delete leaves its `timelines` rows (deleted_at set) and a tenant `Delete` op in storcon-db. A controller restart cleans up once (ops reload with `i32::MAX`); new deletes leave rows again. Safekeeper and S3 data are removed. Minors M1–M4 fixed in `0a8c097` (e2e GREEN 36 s); M5 (a `deps-up` from a branch without the flags turns them off until it merges main) told to the user.
  - **User's D1 (2026-10-03):** merge now; fork fix after as a follow-up.
- **Follow-up (next):** one line in ChelaDB/neon (`generation: i32::MAX as u32` in that `ScheduleRequest`, matching upstream 6a35a3e9f's timeline-path fix), bump `NEON_REF`, rebuild the dev images, and add a storcon-db check to `storage-leak-e2e` (no `timelines` rows and no pending ops left for the tenant after the database delete; RED on the current image).
- **Deferred:**
  - Pre-flag timelines left on this machine's safekeeper1 and S3 (created between the L8 reset and the flag change); the README says how to reclaim them (L8).
  - Optional hardening, matching neon_local: put `safekeepers_generation` and the safekeeper connstrings from `GET /debug/v1/tenant/{t}/timeline/{tl}/locate` into the compute spec (`crates/cheladb-neon/src/spec.rs:113`). When implemented, fall back to the plain safekeeper list when `locate` returns 404 (pre-flag timelines), else computes wait forever for a configuration. Its benefit: a missing safekeeper timeline fails loudly instead of being created implicitly and leaking. Deferring is safe (walproposer adopts the safekeeper's generation-1 config, `walproposer.c:660-666, 943-977`).
  - Optional: SeaweedFS `-master.garbageThreshold=0.1` for faster reclaim in dev (auto-vacuum already works at the default 0.3).
  - **Platform-mode decision for the user:** the controller-managed safekeeper path with auth needs `--safekeeper-jwt-token` and a safekeeper public key (strict mode requires it, `storage_controller/src/main.rs:376-385`).
- **Running:** nothing.

## Root cause (research, 2026-10-03)

- The storage controller runs with only `--dev --listen --control-plane-url --control-plane-jwt-token --database-url` (`infra/compose/shared.yml:202-218` before live-queue). `--timelines-onto-safekeepers` defaults to false (fork `storage_controller/src/main.rs:187-190`), so the compute's walproposer creates safekeeper timelines implicitly and the controller's `timelines` table stays empty.
- Tenant and timeline deletes on the controller call `tenant_delete_safekeepers` / `tenant_timeline_delete_safekeepers` (`service.rs:3759-3762`, `5355`), which return early when the table has no row (`service/safekeeper_service.rs:523-536`, `598-610`; log: "Skipping tenant delete as the timeline doesn't exist in db").
- So every deleted database, branch and superseded (restore/reset) timeline leaves its safekeeper copy: about 16 MB of local WAL (`.partial` segment) plus its WAL backup in S3 (`/buckets/neon/safekeeper/`, 18.65 GB observed; the pageserver prefix was 0 bytes, so the pageserver does clean up; SeaweedFS auto-vacuum works).
- With the flags, the controller creates each safekeeper timeline (generation 1, members [sk1], `safekeeper_service.rs:280-345`) and, on tenant/timeline delete, queues per-safekeeper deletes as durable pending ops, retried after restarts (`safekeeper_reconciler.rs:95-100`, `411-439`); the safekeeper DELETE without `only_local` removes the local WAL and the S3 backup (`safekeeper/src/http/routes.rs:74-94`, `222-240`). `--timeline-safekeeper-count=1` is required (default 3 needs three safekeepers; below 3 is allowed only with `--dev`, `main.rs:398`).
- Compatibility (checked): the create response gains a flattened `safekeepers` field (`TimelineCreateResponseStorcon`, `libs/pageserver_api/src/models.rs:370-375`); our `TimelineInfo` doesn't deny unknown fields. Walproposer adopts the safekeeper's membership config (`pgxn/neon/walproposer.c:943-977`). Worker delete paths (`services/worker/src/delete.rs:57-58, 92-97`, `branch_delete.rs:117-138`, `branch_switch.rs:453, 526, 895, 959, 969, 988`) all go through these controller calls.

## Task 1: controller flags and the live test (sonnet implementer, sonnet review)

**Files:** the storage-controller service in the Compose file live-queue leaves (one file); a live test (prefer `services/worker/tests/neon_live.rs` or a new `scripts/storage-leak-e2e.sh` + `just` recipe, whichever fits the post-live-queue layout); `infra/neon/README.md` (one line on the flags).

**Behaviour:** the storage controller starts with `--timelines-onto-safekeepers --timeline-safekeeper-count=1`. Nothing else changes.

**Test (RED first, on the stack without the flags):**
1. Create a database through the API; assert safekeeper1 lists its tenant/timeline (`GET /v1/tenant/timeline` on port 7676, via `docker compose exec` since 7676 isn't published).
2. Delete it; poll (bounded, e.g. 60 s) until safekeeper1 no longer lists it, `/data/<tenant>` is gone in the safekeeper container, and `fs.ls /buckets/neon/safekeeper/<tenant>` (SeaweedFS `weed shell`) is empty. RED: on the current flags this times out (the safekeeper keeps it).
3. The same for a branch delete (its timeline only) and for the timeline a restore supersedes.
4. GREEN after adding the flags (restart the storage controller through `just live`).

**Gate:** the new live test GREEN; `just test`; the existing `neon-e2e`, `branch-e2e`, `data-e2e`, `role-e2e` pass (creates now go through the controller-managed safekeeper path). All live runs go through `just live <command>` (the queue live-queue adds).

**Gotchas:** new creates take a little longer (the controller waits for the safekeeper); a safekeeper that's down makes creates fail with 500 (the worker retries). Timelines created before the flag still leak: live-queue's storage reset clears them on this machine; the other computer needs the same reset (note it in the README).
