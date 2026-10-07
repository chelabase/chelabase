# Neon fork round 2: research notes (2026-10-07)

Read-only findings gathered during the brainstorm, kept so the spec and plan don't depend on chat. Paths are relative to the fork checkout `<neon checkout>` at `956332507a7b` unless noted.

## Build tooling

- No native fork build on this machine: the host lacks bison, flex and pkg-config, so Postgres can't be built without sudo. All builds go through `.github/scripts/ci-local.sh` in `ghcr.io/chelabase/neon-build-tools`.
- `ci-local.sh` needs a normal clone (it mounts `.git`), not a `git worktree`; its lock is per checkout (`.ci-local/lock`) but the Docker volumes `chela-neon-cargo` / `chela-neon-target` are shared by name, so parallel clones would collide. It always runs lint + build + all Rust tests (no targeted mode). The volumes don't exist yet here (never run on this machine).

## Patch 1: detach waits for the ancestor's WAL

- `prepare` (`pageserver/src/tenant/timeline/detach_ancestor.rs:284`) never calls `wait_lsn`. After a tenant reload the ancestor restarts at `disk_consistent_lsn` (`timeline.rs:3237-3241`); until it re-streams WAL, freeze_and_flush (394-436) and `partition_work` (440-456, 726-758) miss (last_record_lsn, ancestor_lsn].
- Helper: `Timeline::wait_lsn(lsn, WaitLsnWaiter, WaitLsnTimeout, ctx) -> Result<(), WaitLsnError{Shutdown, BadState, Timeout}>` (`timeline.rs:1782`, 974, 1098, 1104). Closest analogue: `get_ready_ancestor_timeline` (`timeline.rs:4798-4812`).
- Placement: after the chain walk (~374), before `start_new_attempt` (~383), so a timeout holds no gc block or attempt. Timeout → new `Error` variant mapped to 503 (not `Prepare`, which is 500). Error mapping at 101-118.

## Patch 3: multi-level detach

- `DetachBehavior::MultiLevelAndNoReparent` already exists (`libs/pageserver_api/src/models.rs:193-208`); it only skips intermediate ancestors branched at the same LSN and otherwise refuses `AncestorNotEmpty` (350-374); the same walk repeats in `detach_and_reparent` (1008-1029).
- Single level today: layers with start > L skipped; straddling deltas rewritten by `copy_lsn_prefix` to [start, L+1); the rest `remote_copy`'d (851); tombstone image for the non-inherited sparse keyspace (181-281).
- Multi-level sketch: chain of (ancestor, cut LSN = its child's ancestor_lsn); per level wait_lsn + freeze_and_flush + partition_work into the detached timeline; publish atomically.
- Gotchas: per-level tombstones (aux / `NON_INHERITED_RANGE` keys don't descend, `timeline.rs:4533-4539`; skip check 203-231 only looks at the top level); layer file names carry no timeline id (collision guard); `Lineage::record_detaching` (`remote_timeline_client/index.rs:339`) stores one `original_ancestor`; gc block only on the detached timeline (610-628); keep no-reparent for multi-level.

## Patch 2: multixact redo

- Bug 1 (real): Postgres' `RecordNewMultiXact` (v17 `multixact.c:955`, 1083-1115; current v14-v17 minors) also writes `offsets[multi+1]` and zero-inits the next page across a boundary; `GetMultiXactIdMembers` (1597-1644) errors "invalid next offset" when it is 0. Neon's `MultixactOffsetCreate` (`walredo/apply_neon.rs:185-211`, produced at `walingest.rs:1015-1033`) writes only `mid`'s entry. Fix with a new record variant (don't change a stored record's serialized form; `libs/wal_decoder/src/models/record.rs:31`), write next entry only if 0, create-if-missing for the next page.
- Bug 2: members wraparound on the last members page (`MAX_MEMBERS_IN_LAST_MEMBERS_PAGE` 1036): `moff + i` (`apply_neon.rs:220`) overflows; walingest's `page_remain` (1038-1071) must cap at the wrap; slot 0 unused.
- Tests: none for apply_neon; Python `test_multixact.py`, `test_next_xid.py::test_multixid_wraparound_import` (287).

## Patch 4: reads at kept LSNs

- `check_lsn_is_in_scope` (`timeline.rs:1920`) is a bare `lsn >= cutoff`. `GcInfo` (484): `retain_lsns`, `leases`, `lsn_covered_by_lease` (543, exact match).
- GC keeps data at exact retain_lsns in both legacy GC (`gc_timeline` 6867, ~6970) and gc-compaction (`compaction.rs:3346-3355`), same as leases.
- Call sites: get_page `effective_request_lsn` (`page_service.rs:2240`, already exempts leases at 2268-2277); basebackup libpq 2711 and gRPC 3761 (no exemption); branch create (`tenant.rs:5033-5055`, exempts leases); `make_lsn_lease` refuses vacant LSNs below the cutoff (`timeline.rs:1996`).
- Proposal: `GcInfo::lsn_is_retained(lsn)` (exact match, retain_lsns or leases) used in all four places. Risk: a child deleted mid-read drops its retain_lsn; don't hold the std RwLock across `.await`.

## Patch 6: create timeline at head waits

- `tenant.rs:4997` uses `get_last_record_lsn()` with no wait; the explicit-LSN path (2749-2779) already waits.
- Safekeeper commit_lsn: `Timeline.walreceiver` → `WalReceiver::status()` (`walreceiver.rs:133`) → `ConnectionManagerStatus` (`connection_manager.rs:397`, private fields `existing_connection.commit_lsn`, `wal_stream_candidates[..].timeline.commit_lsn`); needs a `max_commit_lsn()` accessor. Candidates pruned after `lagging_wal_timeout` (10 s).
- Idle timelines publish nothing to the broker (`safekeeper/src/timeline_manager.rs:504`) → no commit_lsn known → no wait. Wait before `gc_cs` (4994) with a bounded custom timeout; timeout → 503 (storcon retries 503; `routes.rs:677-709`, `storage_controller/src/service.rs:277-293`). Storcon propagates shard 0's LSN (`service.rs:3949`).

## Patch 5: DDL forwarding for role attributes and GRANT

- Fork `pgxn/neon/neon_ddl_handler.c`: `HandleAlterRole` (798-846) and `HandleCreateRole` capture only the password (early return otherwise); `RoleEntry` {name, old_name, password, password_null, type}; per-subxact `DdlHashTable` merged in `MergeTable` (~520-590, field-by-field); `ConstructDeltaMessage` (~150-250) emits `{dbs, roles}`; `SendDeltasToControlPlane` PATCHes `neon.console_url` at pre-commit, non-200 fails the transaction. No `T_GrantRoleStmt` case in `NeonProcessUtility` (~1470-1495).
- Receiver (this repo): `services/orchestrator/src/internal/ddl.rs` (`PATCH /compute/api/v2/computes/{id}/ddl`), `Delta{roles, dbs}` without `deny_unknown_fields` (backward compatible both ways); blind to LOGIN, attributes and grants by design (`docs/superpowers/plans/2026-10-02-login-roles-v2.md:51`, R5, R6). Periodic resync `services/worker/src/resync.rs` repairs from the catalog (skips rows forwarded in the last 10 s).
- Two designs: forward attribute fields + a `grants` array, or mark roles touched and trigger an immediate per-branch resync.
- Docs to update: `apps/docs/content/docs/using/login-roles.mdx` lines ~18, 22, 46 (no literal "role changes lag" string exists).
