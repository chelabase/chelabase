# Landing-page highlights

Raw notes for the landing page: what each piece of work solves, what it unlocks, and the evidence behind it. Added to as steps run (user, 2026-10-07). Not copy: wording for the page comes later, in the voice set by `PRODUCT.md`.

Rules for entries:
- Facts only. Each claim names its evidence (a test, a reproduced bug, a measurement). No invented numbers, customers or benchmarks (`PRODUCT.md`, "Evidence on Hand").
- Say what's upstream and what's ours precisely. "Upstream" means the open-source Neon storage code our fork starts from, not Neon's hosted product, which we haven't tested.
- Mark the status: **shipped** (merged into `main`), **in progress**, or **parked**.

## Neon fork round 2 (2026-10-07, in progress)

Branch `neon-fork-round-2`; fork branch `round-2` on `chelabase/neon`. Spec `docs/superpowers/specs/2026-10-07-neon-fork-round-2-design.md`.

### A restore never silently loses recent writes

- **Problem:** restoring a branch detaches it from its parent's history. Upstream copied the parent's data without first waiting for the parent's latest WAL to arrive. Right after a storage restart, a restore could silently drop the last writes. We reproduced it live while building schema merge (step 6g).
- **What we changed:** detach waits for every ancestor to catch up to the branch point (bounded at 30 s, then a retryable 503 with nothing left half-done).
- **What it unlocks:** point-in-time restore you can trust at any moment, including just after a restart or deploy.
- **Evidence:** a new regression test reproduces the lost writes on unpatched code (the child misses even its `CREATE TABLE`) and passes with the fix.

### Restore works on branches of branches, at any depth

- **Problem:** upstream refuses to detach a branch whose parent is itself a branch (`AncestorNotEmpty`). Restoring a branch of a branch (feature → staging → main) failed.
- **What we changed:** multi-level detach copies each ancestor's data up to its own branch point, and leaves sibling branches untouched.
- **What it unlocks:** deep branch trees (per-feature, per-PR, per-developer branches off a staging branch) with the same restore guarantees as top-level branches.
- **Evidence:** new regression tests for two and three levels, an equal-LSN level and siblings; all failed with "ancestor is not empty" before the change and pass after.

### Branches of busy apps don't break on row locks

- **Problem:** Postgres records shared row locks (`SELECT … FOR SHARE`, and the locks foreign-key checks take under concurrency) as "multixacts". Upstream rebuilt their bookkeeping differently from Postgres. A branch or restart could then fail with `MultiXact N has invalid next offset`, a persistent error on reads of the affected rows. Upstream's own `test_multixact` fails on the unpatched base.
- **What we changed:** the storage layer writes the next multixact's offset exactly like Postgres, handles wraparound, and keeps already-stored data readable unchanged.
- **What it unlocks:** branching and restoring databases with concurrent foreign-key traffic, without a data error showing up later.
- **Evidence:** upstream's failing `test_multixact` now passes; a new test reproduces "invalid next offset" on unpatched code and passes after; Rust tests pin page wraparound, out-of-order logging and old-format compatibility.

### A new branch always includes the last committed write

- **Problem:** creating a branch "now" took the storage server's view of the head. After a storage reload (every restore and restart causes one), that view can lag behind what's committed, so a branch made right after a commit could miss it.
- **What we're changing:** Chelabase asks the safekeepers (the WAL quorum) for the committed position directly. The branch point is never behind a committed write. The storage server waits for the WAL before branching.
- **What it unlocks:** "branch from now" means now: CI that commits fixtures and immediately branches per test, or a preview branch created right after a migration.
- **Evidence:** worker tests (max over safekeepers, fallback when none answers), and the live test `branch_right_after_a_pageserver_restart_sees_the_last_write`: it commits a write, restarts the storage server, and requires the stale window to be real before branching (it fails rather than passing vacuously). First run on the round-2 images: after the restart the storage server's head was at LSN 33016216 while the safekeepers had committed 33016328; the branch created at that moment had every write. The old head logic would have branched at 33016216 and missed the last write.
