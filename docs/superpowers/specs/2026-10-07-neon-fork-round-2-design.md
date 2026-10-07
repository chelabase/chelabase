# Neon fork round 2: design

**Status:** spec, approved and amended (2026-10-07).
**Branches:** this repo `neon-fork-round-2` (`.worktrees/neon-fork-round-2`); the fork `chelabase/neon` `round-2` (checkout `<neon checkout>`, from `main` at `956332507a7b`).

## Amendments (2026-10-07, during execution)

- **D10 (T3 ruling):** Patch 2 uses a combined record variant `NeonWalRecord::MultixactOffsetCreateWithNext { mid, moff, next_moff }` (enum index 8) instead of the separate `MultixactOffsetSetIfZero` originally planned. `MultixactOffsetCreate` stays byte-for-byte decodable. The spec's "member slot 0 stays unused" is wrong: only the starting offset and the next offset skip slot 0; members of a record straddling the wrap use slot 0.
- **D11 (user, T4 → T5b):** Patch 6 changed: the worker reads `commit_lsn` from the safekeepers' HTTP API (GET `/v1/tenant/{t}/timeline/{tl}`) directly; T4's pageserver-side `TimelineInfo.safekeeper_commit_lsn` field was DROPPED (the field stays in Part 3 prose only as background). The worker still sends no LSN to the pageserver when creating at head (it sends explicit LSNs elsewhere, so retries stay idempotent). Compose publishes safekeeper1 HTTP on `127.0.0.1:7676`; the worker maps storcon ids to URLs via env `CHELABASE_SAFEKEEPER_HTTP`. Security trade-off: the safekeeper HTTP API (incl. delete endpoints) is unauthenticated on loopback, like storcon `:1234` and pageserver `:9898` already are. Platform mode needs safekeeper JWT auth with a read-only scope (fork patch) or storcon-proxied commit LSN.
- **D12 (user, T3b):** Patch 2 gained an upgrade fix (T3b): a CREATE_ID on a missing offsets page writes it as one zeroed image with the entry set (replaces the missing-page bug where older multixacts read offset 0). The first pageserver to handle the record writes the page; an earlier CREATE_ID on the same missing page from an older pageserver may have left it off-disk. **Reverted by D17 (below):** T3b's per-CREATE_ID check made multixact-heavy ingest about 20x slower, so it no longer ships, and the accepted limit that went with it (offset-0 reads on stored images) no longer applies.
- **D13 (user, T8 fix batch):** The dev metadata DB was reset on 2026-10-07 (migrations from `schema-merge` on that DB blocked this branch). The migrator stays strict (never ignore unknown migrations).
- **D17 (user, T3b measurement):** T3b is reverted. Re-ingesting 100k multixacts took about 0.97 s on the base and about 21.9 s with T3b (two extra reads per CREATE_ID); after the revert it is 0.93-0.99 s. The gap T3b closed only affects WAL from computes older than Postgres' 2025-12-03 multixact back-patch (present in all vendored v14-v17 computes). Chelabase runs only its own v17 computes and has no physical import, so it can't reach it. T3's fix stays (its next-page check runs only on page crossings). The gap is re-parked with a trigger: revisit before physical imports or foreign computes.
- **D18 (user, reverses D8):** patches 4 and 5 rejoin the round. Patch 4 is still needed for failed base reads even if 6g stores a raw catalog capture; patch 5 is built as Part 4 describes, with T12a's and T12b's corrections. Built in T11 (patch 4), T12a (fork side of patch 5) and T12b (receiver and worker).
- **D19 (user, T11 review):** leases at kept points. A lease below the GC cutoff is granted only at an exactly retained point, checked and inserted under one `gc_info` write lock; branch create at a retained point takes a short lease there under the same lock. Patch 4's section in Part 3 carries the details (amended in `74051f0`).
- **D20 (user, T12a review):** the DDL handler sends `"recreated": true` on a `set` entry when CREATE finds a pending `Op_Delete` of the same name in the same top-level transaction (it implies `touched`). The receiver runs `DelRole`'s live data-key check for any `recreated` entry on an existing row and answers 409 as for DROP, so a `DROP ROLE app; CREATE USER app …` in one transaction can't carry the old role's secret data keys over to the new one.
- **D21 (user, end-of-step review):** the end batch fixes every open minor (T13a-T13f), not only the Critical and Important findings.
- Part 3's patch 2 interface and D11's patch 6 design and Part 6's `TimelineInfo.safekeeper_commit_lsn` mention are superseded by D10–D11 above. Part 4's "PARKED by D8" and the lists of parked patches below are superseded by D18.

**Research:** `docs/superpowers/specs/2026-10-07-neon-fork-round-2-research.md` (file:line facts for every patch and the build tooling).
**Pauses:** step 6g (`schema-merge`) until this step merges.

## Why

Step 6g's live tests (T13, T13b) found Neon bugs that Chelabase-side workarounds couldn't fully close: detach copies an ancestor's data without waiting for its WAL (a restore silently lost writes, reproduced live), and multi-level detach refuses with `AncestorNotEmpty` (child and grandchild restores fail). A sweep of every phase listed the other fork candidates. The user chose to fix them all in one round ("knock out as much as possible without going overboard", "roll back tangled workarounds"), then trimmed the round to the patches with real user impact (D8).

## Decisions (user, 2026-10-07)

- **D1 (R1): five patches, one image rebuild, one `NEON_REF` bump.** (1) detach waits for the ancestor's WAL; (2) multixact redo; (3) multi-level detach; (4) reads at kept LSNs; (5) DDL forwarding reports role attributes and GRANT.
- **D2 (R3): a sixth patch.** Creating a timeline at head first waits until the pageserver has ingested WAL up to the safekeepers' commit LSN.
- **D3 (R2): order.** 6g pauses; this step runs as its own spec → plan → execution; then 6g resumes.
- **D4 (R4): clean rollback of 6g's workarounds** once the patches land (Part 6).
- **D5 (R5): verify, then ship.** Pageserver Rust tests per patch; Neon's Python regression for detach, branching and multixact; our live suites on the new images; only then the merge into the fork's `main`, the image publish and the bump.
- **D6: execution shape = waves with the dev stack stopped.** One clone per patch (patches 1 and 3 share one), at most 3 container builds at once.
- **D7 (D8 parked it; D18 brought it back): patch 5 is designed as a hybrid** (LOGIN/VALID UNTIL forwarded; other attributes and GRANT trigger a delayed resync); built in T12a and T12b.
- **D8 (user, 2026-10-07, impact review): the round is trimmed to patches 1, 3, 6 and 2.** They fix bugs users hit: a restore losing writes (1), restores failing on branches of branches (3), a new branch missing recent writes (6), and a persistent `DATA_CORRUPTED` "MultiXact … has invalid next offset" error on branches cut during concurrent multixact creation (2; verified against `vendor/postgres-v17/src/backend/access/transam/multixact.c:1638`, which no longer waits). 2's members-wraparound fix (`wrapping_add`, two lines) rides along. **Patch 4 (reads at kept LSNs) and patch 5 (DDL forwarding) went back to the parked list (reversed by D18, which brought both back into the round):** 4 only helps when 6g's stored base snapshot is missing (a fallback for a fallback); 5 only removes Studio's display lag for role attributes, since Postgres already enforces NOLOGIN and VALID UNTIL at the compute.

- **D9 (user, 2026-10-07, while planning): patch 6 becomes "TimelineInfo reports the safekeepers' commit LSN" plus a worker change** (Part 3). The worker resolves head itself with explicit LSNs, so the original no-LSN wait would never trigger. A worker → safekeeper query was rejected on security grounds (a new trust edge; safekeepers have no read-only token scope once JWT auth is on). Sending no LSN from the worker was rejected as more code with an idempotency risk.

Left parked (the sweep's reasons stand; patches 4 and 5 are built, by D18; the multixact minor-upgrade gap is re-parked by D17): every detach reloads the whole tenant; in-place timeline switch; the proxy's anti-enumeration "password authentication failed"; compute_ctl's role hook; deleting timelines with children; the proxy's Valkey dependency; the static compute's 60 s refused-basebackup wait; the extension-function grant trigger; `wake_compute` readiness (Chelabase side, with the downtime work). Already done in the fork, not redone: 11a, 11b, 11d (F1–F3), the storage-leak generation fix.

## Part 1: fork branches, clones and build tooling

- **Round branch.** `round-2` is cut in the fork from `main`; it reaches the fork's `main` through one PR at the end (branch protection requires a PR). This repo's work (spec, plan, patch 6's worker side, the bump, docs) is on `neon-fork-round-2`.
- **Clones, not worktrees.** `ci-local.sh` refuses `git worktree` checkouts (it mounts `.git`). Each patch gets a local clone beside the fork: `<neon checkout>-p13`, `-p2`, `-p6`, each on branch `round-2-pN` off `round-2`, with `origin` set to `https://github.com/chelabase/neon.git`. A clone and its Docker volumes are removed after its branch merges (`--no-ff`) into `round-2`.
- **Task 0, tooling (lands first on `round-2`).** `.github/scripts/ci-local.sh` gains:
  - `CHELA_NEON_VOLUME_SUFFIX` (env): when set, the volumes are `chela-neon-cargo-<suffix>` and `chela-neon-target-<suffix>`, so parallel clones never share build state; unset keeps today's names;
  - `--only build|rust-tests` (runs that one step, skipping lint) and `--test-filter <nextest filter expression>` (passed to `cargo nextest run -E`; needs `--only rust-tests`), for RED/GREEN in minutes.
  - Its self-tests (the script's `lint` step runs them) cover both.
- **Baseline.** The full default gate once on `round-2` (proves the image and toolchain on this machine; the volumes don't exist yet), then the targeted regression selection of Part 5, recorded in the plan's Status as the baseline.
- **Concurrency.** At most 3 container builds or test runs at once; the dev stack is stopped for the patch phase (`just live` is unavailable then). Every patch's last check before merging is the full default gate in its clone.
- **Host.** No native build: the host lacks bison, flex and pkg-config. Everything runs in `ghcr.io/chelabase/neon-build-tools`.

## Part 2: patches 1 and 3 (detach; one clone, `-p13`)

File: `pageserver/src/tenant/timeline/detach_ancestor.rs` (plus the error mapping and `libs/pageserver_api` where noted).

**Patch 1: detach waits for each ancestor's WAL.**
- In `prepare`, after the ancestor-chain walk and before `start_new_attempt`, so a timeout holds no gc block and no attempt lock: for every ancestor in the chain, `ancestor.wait_lsn(cut_lsn, WaitLsnWaiter::Timeline(detached), WaitLsnTimeout::Custom(Duration::from_secs(30)), ctx)`, where `cut_lsn` is the LSN its child branched at.
- Errors: `WaitLsnError::Timeout` or `BadState` (not cancelling) → new `Error::AncestorLsnTimeout`, mapped to `ApiError::ResourceUnavailable` (503). `Shutdown` / `is_cancel()` → `Error::ShuttingDown`. The storage controller passes the 503 through; the plan confirms Chelabase's restore call site retries it with backoff.

**Patch 3: multi-level detach.**
- Only under `DetachBehavior::MultiLevelAndNoReparent`; the default behavior keeps refusing with `TooManyAncestors`. Chelabase's restore sends the multi-level behavior at every depth (the plan checks what it sends today).
- The chain is `[(parent, child.ancestor_lsn), (grandparent, parent.ancestor_lsn), …]` to the root. The `AncestorNotEmpty` refusal for unequal LSNs is removed; a level whose cut equals its own branch point contributes no layers, so today's equal-LSN case is a special case.
- Per level: wait (patch 1); `freeze_and_flush` when the cut is at or past its disk-consistent LSN; `partition_work(cut, layers)` (straddling deltas rewritten up to the cut by `copy_lsn_prefix`, the rest copied by reference). All levels' layers go into one `PreparedTimelineDetach`, published atomically (one index upload, then the reset). Shadowing across levels relies on the layer map's LSN order, as the single-level case does.
- Tombstones: one tombstone image at the child's branch LSN over the union of every level's non-inherited sparse keyspace (aux keys never descend into ancestors). The "tombstone already present" skip check looks at every level.
- Layer name collisions (names carry no timeline id): if two levels yield the same layer name, `prepare` refuses with a new `Error::LayerNameCollision` (500, nothing copied). Renaming layers was rejected as riskier.
- No sibling reparenting for multi-level (unchanged). `Lineage` keeps recording the root at the child's LSN (upstream behavior). The repeated walk in `detach_and_reparent` (~1008-1029) drops the equal-LSN requirement too.

**Tests (Rust, `TenantHarness`):** `detach_waits_for_lagging_ancestor`, `detach_ancestor_lsn_timeout_is_503` (no attempt, no gc block left), `multi_level_detach_copies_each_level` (a detached grandchild reads the grandparent's writes below the parent's branch point and the parent's writes below its own, and none of the later ones), `multi_level_detach_tombstones_all_levels`, `multi_level_layer_name_collision_refused`; the existing single-level tests stay green. Python: a grandchild case in `test_runner/regress/test_timeline_detach_ancestor.py`.

## Part 3: patches 2 and 6 (clones `-p2`, `-p6`)

**Patch 2: multixact redo** (`pageserver/src/walingest.rs`, `pageserver/src/walredo/apply_neon.rs`, `libs/wal_decoder/src/models/record.rs`).
- New record variant `NeonWalRecord::MultixactOffsetSetIfZero { mid: MultiXactId, moff: MultiXactOffset }`; `MultixactOffsetCreate` stays byte-for-byte unchanged so stored layers still decode.
- `ingest_multixact_create`, after `mid`'s offset record, emits it for the next entry as Postgres' `RecordNewMultiXact` does: `next = mid + 1` (0 → `FirstMultiXactId`), `next_off = moff + nmembers` (0 → 1). Redo writes the entry only if it is still 0. When the next entry is on a page that doesn't exist yet, walingest first creates that page zeroed (as `ZERO_OFF_PAGE` does).
- Wraparound: walingest caps `page_remain` at the wrap point (the last members page holds at most 1036 members); apply_neon uses `wrapping_add` for member offsets; member slot 0 stays unused.
- Tests: Rust (new `#[cfg(test)]` in apply_neon, plus walingest) `offset_create_writes_next_entry`, `next_entry_across_page_boundary_creates_page`, `next_entry_not_overwritten_when_set`, `members_wrap_at_last_page`, `ingest_emits_next_offset_record`. Python: new `test_multixact_next_offset_after_restart` (multixacts across an offsets-page boundary; restart the compute so the pages come from the pageserver; reading the locked rows raises no "invalid next offset"); existing `test_multixact`, `test_multixid_wraparound_import`.

**Patch 4: reads at kept LSNs. Parked by D8, then added back by D18 (user, 2026-10-07) and built in T11** (`pageserver/src/tenant/timeline.rs`, `pageserver/src/page_service.rs`, `pageserver/src/tenant.rs`).
- `GcInfo::lsn_is_retained(&self, lsn: Lsn) -> bool`: an exact match with a `retain_lsns` entry or a lease key: the LSN equals the point, or normalizes (`normalize_lsn`) to the same LSN and lies at or after it (amended 2026-10-07, T11 review I1: never below the point, since gc-compaction keeps only the image at the exact point and drops the history below). No ranges.
- `check_lsn_is_in_scope` allows an LSN below the applied cutoff when it is retained; it takes `gc_info`'s std RwLock briefly, never across an `.await`. The same rule at all four scope checks: basebackup libpq (~2711), basebackup gRPC (~3761), get_page `effective_request_lsn`, branch create (~5033).
- **D19 (user, 2026-10-07), replacing "out of scope: a new lease at an old retained LSN":** a lease below the cutoff is granted only at an exactly retained point, checked and inserted under one `gc_info` write lock. Every lease is keyed at the exact requested LSN, never normalized (upstream normalized it, so a static compute at a WAL page start lost its point once the cutoff passed it; T13a); reads at the normalized LSN still match. Branch create at a retained point below the cutoff takes a short lease there under that lock, so a child dropped meanwhile can't let GC (legacy `gc_timeline` or gc-compaction) drop the point before the new branch registers its own. A static compute at a kept point therefore keeps its data through compute_ctl's lease renewal even if the child is deleted.
- Accepted risk (corrected 2026-10-07, T11 review I2; the earlier "fails with an ordinary retryable error" was wrong): a reader holding no lease (its lease request failed or was never made) loses the point when the last child that kept it is deleted. On a root timeline its reads then fail; on a branch's parent a page whose history is gone falls through to the grandparent and returns stale data silently (legacy GC works from a snapshot of the retain points taken before the new cutoff is applied). Reads at a point kept by an offloaded child may hit cold layers (performance only).
- Tests: Rust `retained_lsn_readable_below_cutoff`, `unretained_lsn_below_cutoff_refused`, `lease_exemption_unchanged`, `lsn_is_retained_matches_exact_points_only`, `gc_compaction_keeps_only_the_exact_retained_point`, `lease_below_cutoff_only_at_retained_point`, `branch_at_retained_point_takes_a_lease`. Python: a static compute at a child's branch point after GC moved the cutoff past it, still reading there after both children are deleted and GC and gc-compaction ran (its lease holds the point).

**Patch 6 (revised by D9): `TimelineInfo` reports the safekeepers' commit LSN; the worker's head uses it** (fork: `pageserver/src/tenant/timeline/walreceiver/connection_manager.rs`, `pageserver/src/tenant/timeline.rs`, `pageserver/src/http/routes.rs`, `libs/pageserver_api/src/models.rs`; Chelabase: `crates/chelabase-neon/src/storcon.rs`, `services/worker/src/point.rs`, `services/worker/src/ports.rs`).
- *Why revised:* the worker never branches "at head" on the pageserver. `resolve_point`'s `head()` (`services/worker/src/point.rs:31-38`) reads the parent's `last_record_lsn` and always creates with that explicit LSN (`ports.rs:1072`), so a pageserver-side wait for no-LSN creates would never trigger, and the explicit-LSN wait is already satisfied by the stale value.
- Fork: `ConnectionManagerStatus::max_commit_lsn(&self) -> Option<Lsn>` (the highest of the live connection's commit LSN and the broker candidates' valid ones); `Timeline::safekeeper_commit_lsn(&self) -> Option<Lsn>` wraps it; `TimelineInfo` gains `#[serde(default)] pub safekeeper_commit_lsn: Option<Lsn>`, filled by the timeline-info builder in `routes.rs`. The storage controller passes timeline GETs through to shard zero unchanged (`storage_controller/src/http.rs` ~2664-2679), so the field reaches the worker with no storcon change.
- Chelabase: `chelabase_neon::storcon::TimelineInfo` gains `#[serde(default)] pub safekeeper_commit_lsn: Option<Lsn>`; the pure helper `head_lsn(last_record: Lsn, safekeeper_commit: Option<Lsn>) -> Lsn` returns the max; `Storage::get_timeline` (still "the timeline's head") returns `head_lsn(...)`. Creation keeps explicit LSNs, so retries stay idempotent; the pageserver's explicit-LSN path (`tenant.rs` ~2766-2779, `wait_lsn` with the 300 s default) does the catch-up. The worker's create timeout (`CREATE_TIMELINE_TIMEOUT`, 120 s) is shorter; a longer catch-up times out the request and the existing retry recreates with the same LSN (idempotent).
- Covered at once: branch create with no point, a future timestamp (`LsnKind::Future`), reset-to-parent (`branch_switch.rs:144`), and 6g's merge rehearsals, which all go through `head()`.
- Security: no new trust edge (the worker still talks only to the storage controller; the pageserver reads the broker it's already subscribed to).
- Accepted residual: for about a second after a tenant reload, before the walreceiver hears from the broker, the field can be `None` and the head stays stale (the safekeeper holding unuploaded WAL stays "active" and publishes quickly).
- Tests: fork Rust `max_commit_lsn` (no connection and no candidates → None; the connection's and the candidates' max; INVALID ignored) and a `TimelineInfo` serde round-trip with and without the field; Chelabase unit tests `head_lsn` (None → last; lower → last; higher → commit) and `get_timeline_parses_safekeeper_commit_lsn` (present and absent); Python (fork): restart the pageserver right after a committed write, read `TimelineInfo` at once, and branch at `max(last_record_lsn, safekeeper_commit_lsn)`; the branch sees the row. Live (Chelabase): `branch_live` gains `branch_right_after_a_restore_sees_the_last_write`.

**Shared:** the new detach wait is 30 s; every new timeout error is a retryable 503.

## Part 4: patch 5, DDL forwarding (hybrid). Parked by D8, built by D18 in T12a (fork) and T12b (receiver, worker)

The design below is the original; T12a and T12b changed it as follows.

**Corrections from T12a (fork `pgxn/neon/neon_ddl_handler.c`):**
- CREATE without PASSWORD sends an explicit `"password": null` beside the attribute keys (a bare `set` still means PASSWORD NULL). Without it the entry arrived attribute-only and bypassed the receiver's reserved-name refusal.
- CREATE always sends `login` (CREATE USER true; CREATE ROLE and CREATE GROUP false).
- A `created` flag marks a savepoint CREATE, so a rollback of the savepoint clears the parent's attributes for that role.
- A `password_set` merge flag keeps a password-only statement distinct from an attribute-only one.
- `touched` also covers superuser and membership options, not only the attributes listed below.
- DROP clears attributes, and renames carry them to the new name.
- GRANT and the other attributes (`createdb`, `createrole`, `inherit`, `isreplication`, `bypassrls`, `connectionlimit`) trigger the delayed per-branch resync; only LOGIN and the password are applied by the receiver at once.
- compute_ctl's own migrations (ALTER ROLE and GRANT on `neon_superuser`) keep forwarding (M3 rejected by the user): every such body is touched-only, so the receiver answers 200 and schedules a resync.
- D20's `recreated` flag was added later, in T13b.

**Facts from T12b (receiver and worker):**
- `login` is applied by a separate `Step::SetLogin`, not inside `SetRole`.
- An attribute-only entry (no password key) produces no `SetRole`. VALID UNTIL counts as attribute-only (T13d's fix of a re-review finding; before it, `ALTER ROLE app VALID UNTIL …` removed the copy and its sealed API password).
- The resync's `forwarded_at` guard (10 s) now protects only the receiver-owned columns `scram_verifier`, `password_enc` and `login`, so the delayed resync still updates the other columns of a recently forwarded row.
- The NATS payload on `chelabase.resync.branch` is `{"branch_id": …}`.
- The `login` column is a display value: the compute's Postgres enforces NOLOGIN and VALID UNTIL, even through the proxy.

**Original design (as approved):**

**Fork (`pgxn/neon/neon_ddl_handler.c`).**
- `RoleEntry` gains `login` (tri-state: unset / true / false), `valid_until` (the timestamp text as written) and `touched` (bool).
- `HandleCreateRole` / `HandleAlterRole` parse `canlogin` → `login`, `validUntil` → `valid_until`, and set `touched` for `createdb`, `createrole`, `inherit`, `isreplication`, `bypassrls`, `connectionlimit`. The password-only early return becomes "return when nothing tracked is present".
- New `T_GrantRoleStmt` case in `NeonProcessUtility` (GRANT/REVOKE role): marks every grantee and every granted role touched. No grants table (the resync reads memberships from the catalog).
- `MergeTable` carries the new fields (later values win; `touched` is OR-ed); every entry initializer resets them.
- `ConstructDeltaMessage` adds optional keys to the role object: `"login": true|false`, `"valid_until": "<text>"`, `"touched": true`, with small bool helpers beside `PushKeyValue`. A touched-only entry sends `{"op":"set","name":…,"touched":true}`. Password-only bodies are unchanged.

**Receiver (`services/orchestrator/src/internal/ddl.rs`).**
- `RoleOp` gains `#[serde(default)] login: Option<bool>`, `valid_until: Option<String>`, `touched: bool`; old payloads still parse.
- `login` is applied in the same locked transaction as passwords: an existing row's `login` is updated and `forwarded_at` stamped (the existing 10 s guard; an aborted transaction is repaired by the periodic resync). A new copy takes the forwarded `login` instead of R6's hard-coded `true`.
- `valid_until` is accepted and ignored, with no metadata column: the compute's Postgres enforces it, and the proxy doesn't need it. (Ruling S1.)
- When any role in a delta is touched, the receiver publishes the new NATS subject `chelabase.resync.branch` (payload: the branch id) after its transaction commits, following the `SUBJECT_DATA_API_ENABLE` pattern. Touched-only entries don't stamp `forwarded_at`.
- **Worker (`services/worker/src/main.rs`, `resync.rs`):** queue-subscribes to `chelabase.resync.branch`, waits 3 s (the forward arrives at the compute's pre-commit), then runs the existing per-branch `resync_locked`. One pending resync per branch (coalesced). A commit slower than 3 s is caught by the periodic resync, which stays.

**Tests.** Fork: Python `test_ddl_forwarding.py` (it captures PATCH bodies with a mock server): `alter_role_nologin_forwards_login_false`, `valid_until_forwarded`, `createdb_marks_touched`, `grant_role_marks_grantee_touched`, `savepoint_rollback_drops_attributes`, `password_only_body_unchanged`. Chelabase: `steps()` unit tests (login updates the existing row; a new copy uses the forwarded login; touched publishes once; an old payload parses); worker tests for the delayed, coalesced resync; `roles_live`: `ALTER ROLE x NOLOGIN` blocks the proxy login at once, `GRANT grp TO app` shows the derived flag within about 3 s.

**Docs.** `apps/docs/content/docs/using/login-roles.mdx` ~18, 22, 46: LOGIN takes effect at once; attributes and grants within seconds.

## Part 5: verification, publishing and the bump

- **Per patch (in its clone):** RED/GREEN with `--only rust-tests --test-filter …` → review → the full default gate → merge `--no-ff` into `round-2` → delete the clone and its volumes.
- **On `round-2`:** the full gate, plus `--regress -k "detach or branch or multixact or next_xid"` including the new Python tests, compared with the baseline. `test_runner/known_failures*.txt` may only shrink; any new failure blocks.
- **Local images:** `just neon-build` with `NEON_REF` at `round-2`'s head; `just deps-up` uses the local `<sha12>-dev` images. Nothing is pushed to ghcr.io yet.
- **Live suites** on those images (stack started again; each through `just live`, one at a time): `branch_live`, `roles_live`, `live_db`, `neon-e2e`, `data-e2e`, `storage-leak-e2e`, `app-users-e2e`. `merge_live` and the grandchild restore run when 6g resumes.
- **Ship, only when all green:** PR `round-2` → the fork's `main` (merge commit, <maintainer>); `.github/scripts/publish-dev.sh` from the merge commit; on `neon-fork-round-2` bump `NEON_REF`, the compose digests and `infra/neon/README.md`; `just deps-up` pulls the published digests; one last `neon-e2e`. Then the end-of-step whole-branch review (opus) over both repos' diffs, the fix batch, and a pause before merging into `main`.
- **Failures** go back to the patch they point at; a new local image is built and the suites rerun; nothing is published in between.

## Part 6: the R4 rollback in 6g, and the docs

**Order.** This step merges into `main` first; 6g resumes on `schema-merge` by merging `main` (the bump and the digests). The rollback is 6g's resume work, because the restore code and its plan exist only on `schema-merge`.

**This step changes in Chelabase:** patch 6's worker side (Part 3: `TimelineInfo.safekeeper_commit_lsn`, `head_lsn`, `Storage::get_timeline`, the `branch_live` case); the bump (`NEON_REF`, digests, `infra/neon/README.md`); docs (`apps/docs/content/docs/using/login-roles.mdx` line ~48's "isn't pinned yet" claim about the `PASSWORD NULL` rename fix, corrected if it is pinned; the role-lag notes go, since patch 5 is built: `login-roles.mdx` now says LOGIN takes effect at once and other attributes and grants are read back in a resync about 3 s after the commit); the roadmap (patches 3, 4 and 5 move from parked to done, and the multixact item leaves the parked list except the minor-upgrade gap re-parked by D17); the user's memory note on parked multixact redo is retired.

**6g's resume list** (written into 6g's plan Status during this step; carried out when 6g resumes):
1. Merge `main` into `schema-merge` (the bump; conflicts limited to `NEON_REF`, compose digests and `infra/neon/README.md`, if any).
2. Discard T13b: carry `restore_a_child_branch` over (the test only), then delete `schema-merge-t13b` (`d8ea1fa`) and its worktree.
3. Restore becomes one detach with the multi-level behavior at any depth (no double copy, no `wait_for_catch_up`, no two-detach recovery states); add live `restore_a_grandchild_branch`.
4. Strike E24, E25 and the grandchild refusal from the plan ("superseded by fork round 2").
5. E17 stays as decided (re-baseline outside the history window when the stored snapshot is missing); patch 4 is built (D18), so reads at kept points can also serve a missing base read, and 6g decides whether to use that.
6. T13b concern (2) is closed by patch 6; (3)–(5) stay for T14.
7. Then T14 and T15 as planned. (The role-lag notes are already gone, since patch 5 is built. 6g also has to reconcile `InternalState.resync`/`ResyncPort` and the new NATS subject `chelabase.resync.branch` with its own resync code.)

**Safety nets kept:** the periodic role resync; T7's GC-cutoff pre-check.

## Rulings

- **S1 (Claude, 2026-10-07; applies to patch 5, built by D18): `valid_until` is not stored.** *Cost if wrong: a metadata column and a migration later if Studio should show it.*
- **S2 (Claude, 2026-10-07): patch and round branches are pushed to `chelabase/neon` as work proceeds;** R5's "only then" applies to the merge into the fork's `main` and the image publish. *Cost if wrong: unverified work is visible on public fork branches (never on `main` or ghcr.io).*
- **S3 (Claude, 2026-10-07): layer name collisions refuse instead of renaming.** *Cost if wrong: a rare multi-level detach fails with a clear error until a rename path exists.*
