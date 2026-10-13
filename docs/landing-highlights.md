# Landing-page highlights

Raw notes for the landing page: what each piece of work solves, what it unlocks, and the evidence behind it. Added to as steps run (user, 2026-10-07). Not copy: wording for the page comes later, in the voice set by `PRODUCT.md`.

Rules for entries:
- Facts only. Each claim names its evidence (a test, a reproduced bug, a measurement). No invented numbers, customers or benchmarks (`PRODUCT.md`, "Evidence on Hand").
- Say what's upstream and what's ours precisely. "Upstream" means the open-source Neon storage code our fork starts from, not Neon's hosted product, which we haven't tested.
- Mark the status: **shipped** (merged into `main`), **in progress**, or **parked**.

## Landing split (2026-10-08, shipped)

Merged into `main` 2026-10-08. Spec `docs/superpowers/specs/2026-10-08-landing-split-design.md`; plan `docs/superpowers/plans/2026-10-08-landing-split.md`.

### The landing page as its own static app on the shared lagoon

- **Problem:** the landing page lived in Studio (on an unmerged branch), with its own copy of the palette and fonts, and Studio and the landing each had a copy of the crab marks. Self-hosted must never ship marketing pages.
- **What we built:** `apps/landing`, a prerendered TanStack Start app (static HTML, no stack needed), rewritten on Tailwind with every colour, font and the crab marks coming from `@chelabase/lagoon`. Its buttons link to Studio, where `/?signup` goes straight to account creation. The hero now ends with "staging merged into main" on every screen size, and the comparison has a PlanetScale column (not surveyed yet).
- **What it unlocks:** the polish, audit and mobile passes run on the final layout; the landing can be hosted anywhere static, separately from self-hosted Studio.
- **Evidence:** a Playwright pixel check at 1440, 1024 and 390 px matched the Tailwind rewrite against screenshots of the verbatim port (≤ 954 differing pixels, antialiasing only); tests fail if the app writes a colour literal or redeclares a lagoon token; build tests check the prerendered HTML (dark, copy, Studio links, short-screen CSS); Studio's sign-up redirect fires once, never after an error, and replaces the history entry.

## Docs in the lagoon style (2026-10-08, shipped)

Merged into `main` 2026-10-08. Spec `docs/superpowers/specs/2026-10-07-css-design.md`; plan `docs/superpowers/plans/2026-10-08-docs-lagoon.md`.

### One look across Studio and the docs

- **Problem:** the docs site used Fumadocs' stock neutral theme with light/dark switching, while Studio and its API page use the lagoon palette; method colours differed between the two.
- **What we built:** one workspace package (`@chelabase/lagoon`) holds the palette, fonts, the Tailwind preset and the Scalar and Fumadocs skins; Studio and the docs import it. The docs are dark only, every HTTP method badge has the same tone as on Studio's API page, and callouts follow the same colour meanings (coral = hazard, seafoam = yes, sand = attention).
- **What it unlocks:** the landing page joins the same package next, so all three surfaces share one source of colour and type.
- **Evidence:** package tests pin every token against Studio's previous values and check contrast (text and method tones ≥ 4.5:1 on ink, deep and channel; tones ≥ 5.4:1 on their 10% badge tint); a test fails if any app declares its own fonts; Studio's built CSS compared against `main`: no custom property changed, only three Spectral faces added; docs build-output tests (dark without JavaScript, badge tones on every badge and nothing else).

## Step 6g: three-way schema merge (2026-10-08, shipped)

Merged into `main` 2026-10-08. Spec `docs/superpowers/specs/2026-10-06-schema-merge-design.md`; plan and full history `docs/superpowers/plans/2026-10-06-schema-merge.md`.

### Merge schema changes between live Postgres branches, like git

- **Problem:** branching a database is easy; getting a branch's schema changes back into main is not. Other tools reset the branch from its parent (overwriting it), show a two-way diff, or merge migration files in git; the only three-way schema merge with conflict detection we found (PlanetScale deploy requests) is MySQL only (roadmap research, 2026-10-06).
- **What we built:** a three-way merge of the live catalogs (base = the branch point, ours = the parent now, theirs = the branch now) for tables, columns, constraints, indexes, enums, sequences, RLS, policies, grants and roles, in both directions (merge into the parent, update from the parent). Conflicts are flagged per object like git, with keep main / take branch / your own SQL. Renames are followed by Postgres object identity (OID, attnum for columns), not guessed from names. Main's data stays canonical: only schema moves.
- **What it unlocks:** schema work on a branch with real data, then a reviewed, conflict-checked merge into production, without hand-written migrations.
- **Evidence:** `merge_live` (26 live scenarios on the Neon stack: merges both ways, conflicts, renames, restores before and after a merge, bases behind GC) passes 26/26; `just merge-e2e` passes; the schema crate's analysis and planner unit tests.

### Every merge is rehearsed on a throwaway copy first

- **Problem:** a schema migration that works on a dev database can still fail or do something else on production's actual schema.
- **What we built:** each merge is first applied to a throwaway branch of the target, and the result is checked against the expected merged schema; the apply refuses if the target changed since (a digest guard inside the transaction). The apply is one transaction with a lock timeout, then a resumable online phase (`CREATE INDEX CONCURRENTLY`, `NOT VALID` + `VALIDATE`). A half-applied merge holds both branches until it is retried or abandoned, and Abandon lists exactly what was left behind.
- **What it unlocks:** applying schema changes to production with a dress rehearsal on its real schema, and a clear recovery path if something stops halfway.
- **Evidence:** an exhaustive state-machine test over 16 merge states × 9 actions and × reset/restore/delete on both branches, plus its live SQL counterpart (`merge_state_sql_matches_the_table`, `every_branch_path_follows_the_hold_table`); live `a_failed_online_merge_holds_its_target_and_abandon_lists_leftovers`, `recovery_reads_the_marker`.

### Restore stays an undo, even across merges

- **Problem:** after restoring a branch, a later merge has to compare against the true common ancestor, or it re-applies or loses changes.
- **What we built:** each branch stores a base snapshot when it is created or reset, with a history of bases across restores, and a fallback that reads the branch point from storage (fork round 2's reads at kept LSNs) when the snapshot is missing.
- **What it unlocks:** restore as the undo for a merge on any branch, at any depth.
- **Evidence:** live `restore_before_a_keep_merge_uses_the_older_base`, `a_restored_branch_reads_its_base_until_the_backup_is_deleted`, `a_branch_point_behind_gc_is_read_on_the_parent`, `a_grandchild_branch_point_behind_gc_is_read_on_its_parent`.

## Neon fork round 2 (2026-10-07, shipped 2026-10-08)

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

### Team access mapped onto Postgres roles (Members and roles)

- **Problem:** a project had one owner login. There was no way to let a department edit only its own tables (say, `billing`) without handing out the owner password.
- **What we built:** members from the tenant's Zitadel org, invites (by email, or a one-time link when there is no SMTP), and Viewer, Developer and Admin per project, narrowable per database. Each grant lists the Postgres group roles a person may act as. Every member gets their own `cb_m_*` login role per branch, holding only the groups they were granted (`SET` only, `INHERIT FALSE`). The SQL editor, console and data keys act through it, so Postgres enforces the boundary. Owner-level, LOGIN, reserved and ADMIN-holding roles, and chains reaching them, are Admin-only. After every run, objects a member created as themselves are handed to the role they acted as. Self-hosted sign-up is off unless an admin opens it; sign-ups wait for approval. Every access change is logged.
- **What it unlocks:** an owner creates a `billing` group that owns schema `billing`, grants it to the billing team, and they edit it freely while everything else refuses them. No shared passwords, and revoking someone takes effect at once.
- **Evidence:** property tests that the orchestrator, the worker and Studio give identical role decisions on random role graphs; an exhaustive live test that a member role holds nothing after any run ending (success, error, cancel, timeout, disconnect); `members-e2e` (invite, grant, act-as `billing`, refused elsewhere, revoke ends open sessions, pending approval) plus the live suites and a browser pass, all green. Status: merged to `main` 2026-10-10.

### A table API that shows the exact SQL before it runs (TE-A)

- **Problem:** Studio's table editor needs to change table structure safely for people who don't write DDL, and a project's protected branch must not take schema changes from anyone but Admins, whichever tool they use.
- **What we built:** typed management endpoints for the object list, a table's structure, the type catalog, and one `schema-changes` endpoint for every DDL and enum change (tables, columns, constraints, indexes, row-level security, policies, grants), run as the person's chosen Postgres role. A dry run executes the same statements in a rolled-back transaction and returns them; statements that would rewrite a large table are shown but not executed. Drops are RESTRICT unless the caller confirms a cascade with the hash of the exact dependents it was shown, and the real drop is then checked against Postgres's own cascade report. `CONCURRENTLY` indexes clean up only their own failed build. On protected branches an event-trigger guard in every database refuses schema changes from Developers' member logins, in the SQL editor too, while rows, temporary tables and data maintenance stay normal. Structure reads follow Postgres's own login rule: a member sees a database's structure only if their own login may connect to it, so the table API never shows more than the SQL editor would. Plus an all-defaults insert and a full-cell fetch capped server-side.
- **What it unlocks:** TE-B, Studio's table editor, built on a reviewed API; Developers work freely on branches while protected `main` takes schema changes only through Admins or merges.
- **Evidence:** the renderer is shared with schema merges (an invariant test checks both render the same object identically); property tests over hostile names (every name reaches Postgres as given) and over random request sequences (no stale catalog knowledge); live tests for dry runs, cascades, concurrent indexes, lock timeouts, permissions and the guard (planted objects, maintenance allowed), and the full live pass (catalog, orchestrator, rest, worker suites, six e2e runs) green. Status: on branch `table-api`, awaiting the merge decision (2026-10-13).
