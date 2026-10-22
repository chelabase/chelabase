# Hardening after the carried gaps (implementation plan)

**Goal:** close what the carried-gaps step listed for the user (`docs/superpowers/plans/2026-10-22-carried-gaps.md`, Status "Listed for the user, not done"): the attach wait's edges, Valkey's stale ACL after a change, a never-ready compute stalling every reconcile pass, and a branch create's 404 after a pageserver lost its volume.

**Spec:** none (follow-ups; rulings below).

**Tech:** Rust. No fork change: no image build; the live pass runs on the pinned images.

**Standing rule (user, 2026-10-22, lean mechanical changes):** each area is one sonnet agent working test-first (a failing test shown before the change), coordinator-verified diff, no separate audit; one `just check`, one stack, the affected live suites once.

## Status

- **RESUME HERE (2026-10-22): step tested; paused for the user's go to merge `hardening` into `main`.** Stack `hardening` up (purge after the merge).
- **Done:** A `ec868114` (merged `38d20331`): create cut off at the deadline → delete by name; pause after an empty pull; give up after 2 permanent create refusals (stream not found, JetStream off; a permissions denial is indistinguishable from a slow server, so it still waits out the timeout); rule 4 (a wait whose pulls never answered or whose last 3 calls failed is `Err`); P2 comments. B `fe735be8` (merged `c67d1fa2`): `write_auth_files` reports NATS and Valkey changes separately; `up` restarts Valkey on a changed `users.acl` (Valkey is a `--save ""` cache: a restart loses nothing the stack relies on). C `59c48bd9` (merged `85f65c62`): per-endpoint recreate backoff (30 s doubling to 10 min, reset on success or when the branch leaves Ready); branch jobs retry the pageserver-forwarded tenant 404. Coordinator verified each diff.
- **Live pass 1 found 2 failures**, fixed by an opus debug agent in `b4b06946`: (1) A's rule 4 regressed a no-event wait with a single pull (the pull's `expires` reached the deadline, so the server's empty answer was cut off): pulls now expire 250 ms before the deadline (`attach_pull_expires`, exhaustive test); (2) `restore_a_grandchild_branch` raced the stack worker because the live tests deleted databases without `begin_delete` (branches stayed Ready, reconcile legitimately recreated the compute): tests now claim first (`common::delete_database`), and a **product race** found with it is fixed: `execute(RecreateBranchCompute)` re-reads the branch and acts only if it is still Ready with that endpoint (a milliseconds window between re-read and create remains). Pre-existing, not from areas A–C.
- **Gate and live:** `just check` 3874/3874 (after `b4b06946`). Live on `b4b06946`: `permissions_live` 22/22, `attach_events_live` 7/7, `branch_live` 5/5, `neon_live` 1/1, `realtime_role_live` 4/4; from the first pass (code they exercise unchanged since): `valkey_acl_live` 8/8, `neon-e2e` PASS.
- **R1 (user, 2026-10-22):** fix every listed item before `/clear`.
- **R2 (user, 2026-10-22):** item 9: the branch job classes the pageserver-forwarded `NotFound: tenant` 404 as transient (existing NATS redelivery: tear down, redeliver, bounded attempts); storcon's own 404 stays permanent.
- Branch `hardening`.

## Plan rulings (mechanical, listed for the user)

- **P1** Item 8: per-endpoint backoff for a compute whose recreate never got ready, in memory in the worker: skip it for 1 pass after the first failure, doubling per further failure, capped at 10 min; reset when the compute gets ready (or the branch leaves Ready, or the worker restarts). Cost if wrong: a compute that would have come back stays down up to 10 min longer.
- **P2** Items 5 and 6 are by design (the NATS restart briefly disconnects running apps, which reconnect and redeliver in-flight jobs; attach events expire after 1 h, after which a retried provision waits 60 s then proceeds under R8): a comment each, no behaviour change.
- **P3** Item 3: a consumer create refused for a reason that won't change within the wait (stream not found, permission denied) ends the wait early as `Err`/`Unavailable` after at most 2 tries, instead of retrying for the full timeout.

## Areas (binding)

| Area | Scope | Agent | Review | Files |
|---|---|---|---|---|
| A | items 1–6 | sonnet | coordinator | `crates/chelabase-nats/src/lib.rs` (+ its tests), `tools/cli/src/workbench/commands.rs` comment only for item 5 |
| B | item 7 | sonnet | coordinator | `tools/cli/src/workbench/{service_auth.rs,commands.rs}` + tests |
| C | items 8, 9 | sonnet | coordinator | `services/worker/src/{reconcile.rs,branch_switch.rs,branch_job.rs}` (+ tests) |

A's one comment in `commands.rs` (item 5, next to the NATS restart) may conflict trivially with B; B owns `commands.rs`: A adds its comment only if B's diff doesn't touch those lines, else the coordinator adds it at merge.

## End of step

1. Coordinator verifies each diff; merges A, B, C into `hardening`.
2. `just check`, once.
3. Stack up in `.worktrees/hardening` (exercises R9 and item 7's restart on a fresh then a re-rendered stack), then once each: `attach_events_live`, `permissions_live`, `valkey_acl_live`, `branch_live`, `neon-e2e`.
4. Update this Status, the roadmap's "Resume here" and `docs/landing-highlights.md`; pause for the user before merging.
