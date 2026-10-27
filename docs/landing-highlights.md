# Landing-page highlights

Raw notes for the landing page: what each piece of work solves, what it unlocks, and the evidence behind it. Added to as steps run (user, 2026-10-07). Not copy: wording for the page comes later, in the voice set by `PRODUCT.md`.

Rules for entries:
- Facts only. Each claim names its evidence (a test, a reproduced bug, a measurement). No invented numbers, customers or benchmarks (`PRODUCT.md`, "Evidence on Hand").
- Say what's upstream and what's ours precisely. "Upstream" means the open-source Neon storage code our fork starts from, not Neon's hosted product, which we haven't tested.
- Mark the status: **shipped** (merged into `main`), **in progress**, or **parked**.

## Members hub, step 2: teams with live-linked access (2026-10-27, in progress, end-of-step pending)

Branch `members-hub-teams`; spec `docs/superpowers/specs/2026-10-23-members-hub-design.md` (D1-D6); plan `docs/superpowers/plans/2026-10-27-members-hub-step-2.md`. No fork change.

### Teams that grant access, live
- **Problem:** access was granted one person per project. Giving ten people the same access meant ten grants, and changing it meant ten edits.
- **What we built:**
  - **Teams** hold per-project grants (platform role plus Postgres roles). Members get them live: effective access is their own grant plus their teams' grants, narrowed afterwards by their database overrides. Nothing is copied.
  - **Per-role levels:** each Postgres role keeps the highest level among the sources naming it, so a Viewer-level team grant can't lift a role to Developer.
  - **Every team write is checked on the union** for every affected member and database, under the tenant lock then the members' locks in id order. A personal login that stops qualifying is revoked in the same transaction, and the same side effects as a grant write fire (realtime, proxy cache, reconcile).
  - **Invites and approvals can attach teams** (`team_ids`); the project invite refuses them.
  - Studio gains a Teams tab and team page.
- **What it unlocks:** onboarding and offboarding by team membership, with a downgrade reaching the SQL editor role and personal logins at once.
- **Evidence (suites written for the step):**
  - metadata: `members_live` extended for teams, including the SQL/Rust agreement test and an interleaving test of the lock order;
  - orchestrator `tests/teams.rs`: `a_team_grant_is_checked_on_the_union_for_every_member_and_database`, `every_write_sequence_keeps_the_invariant` (proptest), `every_team_write_fires_realtime_cache_and_reconcile`;
  - `tests/api.rs`: `project_invite_refuses_team_ids`, `tenant_invite_with_team_ids_adds_memberships_on_every_outcome`, one `team_only_member_*` test per caller, the T23 audit's two union-check race tests;
  - live (end-of-step pass, not yet run): `just teams-e2e`.

### SQL editor role holds only Developer-level roles
- **Problem:** the hidden member role behind the SQL editor held every role the member could act as, Viewer-level ones included, so a role held at Viewer could be set from a Developer-only editor.
- **What we built:** the hidden role is granted only roles held at Developer or above, and a downgrade revokes the role at once. The table editor's full-cell read as a role held below Developer goes through the data API's connection (`authenticator` + `SET ROLE`), answering 409 `data_api_disabled` when the data API is off.
- **Evidence:** `member_roles_live` and `sql_run_live::the_sql_editor_cannot_set_a_viewer_level_role` (compiled, live run pending); `rest_live::console_cell_reads_as_the_role_and_refuses_a_column_it_lacks` (live run pending).
- **Status:** in progress, end-of-step pending.

## Studio shell: phone navigation, breadcrumbs, no clipped content (2026-10-27, shipped: merged `5b4ab478`)

Branch `studio-shell`; plan `docs/superpowers/plans/2026-10-27-studio-shell.md`. Studio only; no fork change, no image build.

### Studio on a phone
- **Problem:** below the rail breakpoint Studio had no way to navigate (the rail was a fixed strip), no sense of where you were, and some pages hid content: the project's Branches table and forms were clipped (KI-1) and the invite dialog's role chips spilled out of their borders (KI-2).
- **What we built:**
  - A menu button in the header that opens the navigation as a drawer with labels. Escape, the backdrop or following a link closes it, and focus returns to the button.
  - A header breadcrumb on every page (projects, project, database, merge, tables, members, member, role), with names supplied by the pages from data they already loaded. On a phone a trail longer than two collapses its middle into a "…" menu of links.
  - Branches and project forms scroll inside their own region instead of clipping; role chips truncate inside their border.
- **Unlocks:** the same Studio is usable at 390 px, which the members hub and later pages build on.
- **Evidence:** full Studio vitest and typecheck green on the branch; captures at 390 and 1440 in `docs/superpowers/designs/2026-10-27-studio-shell/png/`; browser pass on the stack confirmed the drawer (open, Escape, backdrop, link; focus back on the button), the "…" menu, a 52 px header on every page, no horizontal page scroll on 7 of 9 top-level pages at 390, and chips truncating inside the chip. The pass also found defects (Tables page and merge page at 390, Tables context bar at 1440), listed in the plan's Status.
- **Status:** awaiting merge.

## Members hub, step 1: one place for people, roles and privileges (2026-10-24, shipped: merged into `main` `6e3c8df3`)

Branch `members-hub`; spec `docs/superpowers/specs/2026-10-23-members-hub-design.md`; plan `docs/superpowers/plans/2026-10-23-members-hub-step-1.md`. No fork change (it builds on fork round 4's personal-login marker).

### One place for access
- **Problem:** Studio's Members page could only make someone a tenant admin or remove them. Grants lived in each project's Access tab, Postgres roles in each branch's panel, and there was nowhere to define NOLOGIN roles and what they may touch.
- **What we built:**
  - A members hub. The roster is grouped by each person's highest visible level, with a project filter and card or list views.
  - A page per member: grants per project with database narrowing, invite actions, and Suspend / Reinstate / "Revoke Postgres access now".
  - A Roles tab: create roles, a staged privilege editor (tables and columns, schemas, sequences, functions, default privileges, "all rows" for RLS), and role delete with a chosen successor.
  - Multi-email invites that report each address's outcome.
- **Evidence:** Studio suite 2472/2472; three Impeccable finish reviews against the user-approved comps; a browser pass as a tenant admin and as a Developer.

### Personal Postgres logins that die the instant access does
- **Problem:** to use `psql` or a GUI client, people shared the app's password or the owner's.
- **What we built:**
  - **One login per person per database**, present on every branch where they have Developer access or more. It holds no privileges of its own and acts only through its groups, and it can start in a chosen group (`options=-c role=…`).
  - **Revocation reaches the proxy in the same transaction** as any loss of access, before any compute work: grant or override changes, removal, suspension, revoke-now, the IdP webhook, a 10-minute IdP sweep, and a 90-day expiry. An offboarded member is refused everywhere at once.
  - **Objects a removed login owned go to a powerless quarantine role, never to the owner**, so a planted SECURITY DEFINER can't escalate. Role deletes ask for a successor and preview any exposure before confirming.
- **Evidence:**
  - `personal_logins_live` 7/7 and `personal-logins-e2e` 10 checks, covering a proxy refusal while the compute is suspended and the role option;
  - `members_live` 65/65, with deterministic race tests of a worker write against a reset or revoke;
  - `member_roles_live` 30/30, `roles_live` 47/47;
  - a 48-case exhaustive model of login convergence.

### Privileges that are checked, not assumed
- **Problem:** Postgres answers a GRANT or REVOKE the caller can't perform with a warning, not an error. So an editor can report success while the privilege is unchanged.
- **What we built:**
  - Every privilege apply re-reads the ACLs in the same transaction and rolls back with a named mismatch if anything differs.
  - Edits on objects owned by group roles run as their owner through a transient SET that leaves the owner's memberships exactly as they were.
  - "All rows" is refused for any role the data API reaches. That check runs in the worker and fails closed.
- **Evidence:** `privileges_live` 10/10, `catalog_live` 15/15, the orchestrator `api` live privilege tests, and T1's PG 17.11 feasibility probe.

### Also fixed on the way
- Four `merge_live` tests broken on `main` since 2026-10-16: they sent no storage-controller token.
- Two timing flakes in the test harness: a paused clock racing loopback on WSL2, and SIGPIPE in lint scripts.
- Advisories RUSTSEC-2026-0384 (`tokio`) and RUSTSEC-2026-0385 (`ctutils`).

## Carried gaps: provisioning through a pageserver restart, no leaked computes (2026-10-22, shipped: merged into `main` `06758d3f`)

Branch `carried-gaps`; plan `docs/superpowers/plans/2026-10-22-carried-gaps.md`. No fork change.

### A database created right after a pageserver restart just works

- **Problem:** in the first seconds after a pageserver restart, the upstream storage controller refuses tenant create and delete with a 409 ("No pageserver found matching constraint"), answers a timeline create with a 503 ("tenant … Attaching"), and can answer a tenant create with 201 before the tenant is attached (the attach was cancelled by the pageserver's re-attach), so the next timeline create gets a 404. The worker took each of these as final, so the database failed.
- **What we built:**
  - The worker's storage-controller client retries only those transient answers, with bounded, jittered backoff (tenant create 45 s, above the controller's own 30 s reconcile wait; timeline create 120 s); every other refusal stays final.
  - Event-driven attach: the storage controller's attach notification is published on a JetStream stream, and provisioning waits for the new tenant's attach event before creating its timeline (no polling). If the event doesn't come, it warns and tries once.
  - Least-privilege NATS rights for the new stream (the worker can read only one tenant's subject; nobody can delete it), and `workbench up` restarts NATS when its permissions change.
- **Evidence:** `storcon_mock` (68 tests: retries, budgets, cancellation); `attach_events_live` 6/6, `permissions_live` 22/22; `branch_live` 5/5, including `provision_right_after_a_pageserver_restart_succeeds`, which reproduced the 404 before the attach event existed.

### Computes the worker can't make ready don't linger

- **Problem:** a compute the worker recreated but that never became ready stayed behind and restarted forever (seen in fork round 4's live pass); reconcile only recreated missing computes, not stopped ones.
- **What we built:** the worker removes its own never-ready compute (only while the stored token is still its own); reconcile recreates a compute that exists but isn't running (re-checked just before acting, so one Docker brought back is left alone); a Ready branch whose tenant is gone from the storage controller is reported, never churned; an abandoned compute create removes its container.
- **Evidence:** worker unit tests over the fakes (888), `docker_backend_cleanup_mock`, `docker_backend_live` 3/3, `compute_fork_live` 11/11.
- **Follow-up (hardening step, 2026-10-22, shipped: merged into `main` `0aa8f002`):** a compute that keeps failing to get ready is backed off per endpoint (30 s doubling to 10 min) instead of costing every reconcile pass a minute; reconcile never recreates a compute for a branch a delete or reset has claimed since it looked (re-checked just before acting); branch jobs retry the pageserver's "tenant not found" while it re-attaches; `workbench up` restarts Valkey, like NATS, when its permissions change. Evidence: worker unit tests (906), `branch_live` 5/5, `realtime_role_live` 4/4, `valkey_acl_live` 8/8.

## Neon fork round 4 (2026-10-22, shipped: merged into `main` `fafb3275`)

Branch `neon-fork-round-4`; fork `chelabase/neon` `round-4` `0741b56de`, `chelabase/postgres` `round-4-v17` `7812d66fe85`; plan and full history `docs/superpowers/plans/2026-10-21-neon-fork-round-4.md`. The upstream Neon base passes secrets through the environment and leaves several storage routes open; these are ours.

### No secret in a service's command line or environment

- **Problem:** upstream, compute_ctl puts the storage token in Postgres' environment, and the storage controller, pageserver and proxy read their tokens and passwords from environment variables or flags. Anything that can read a process's environment or arguments sees them.
- **What we built:**
  - Every token and password is read from a file, through one reader: regular file only, non-blocking, size-capped, never echoed.
  - compute_ctl's internal API listens on loopback only, behind a per-start secret.
  - The storage token expires after 24 h, and the worker refreshes it without restarting the compute.
- **Evidence:**
  - `compose-secret-argv.sh` fails on any secret exported from a `/run` file. The only exception is a public key.
  - `compute_fork_live` 11/11, including the expired-token refresh and the loopback-only internal API.
  - The fork's token-file Python suites.

### Tokens that can only do their job

- **Problem:** upstream, a metrics scraper needs an admin token. Some pageserver routes checked no scope, so another tenant's token reached them (found by our audit; an exhaustive route test counted 42 gaps on the pageserver and 4 on the storage controller).
- **What we built:**
  - A read-only `metrics` scope.
  - Every route authorised before its handler reads the request.
  - Control-plane routes refuse tenant tokens.
  - `nbf` validated.
  - compute_ctl answers 401 with no bypass.
- **Evidence:** `test_route_scopes.py`, whose route list is parsed from each router and fails if incomplete; `storage_auth_edges`; `compute-network-e2e.sh` with no known gaps.

### Realtime logins bound to realtime, large transactions streamed

- **What we built:**
  - `chelabase_realtime` logs in only from realtime's address, with its client certificate.
  - The `chelabase_keys` decoder streams large transactions instead of spilling them to disk.
- **Evidence:**
  - `realtime_role_live` 4/4 (wrong certificate, no certificate and compute-network logins refused).
  - `realtime_live` 19/19, including a big transaction streamed through its own slot.

### Personal Postgres logins the project owner can't take over

- **What we built:** a marker the neon extension guards. A non-superuser can't alter, drop, grant, comment on or label a personal login, or change the marker; it holds on every Postgres version.
- **Evidence:** `test_member_role_guard*` suites; `compute_fork_live` personal-login tests.
- **Unlocks:** personal logins on the Members page.

### Also fixed on the way

- **Temporary slots:** they never reach the pageserver. A malformed slot entry is skipped instead of crashing the compute.
- **Extension trust:** extension installs check trust for every script Postgres runs.
- **Mock-S3 test corruption:** moto 5.2.4 fixes it; it was breaking WAL tests on fork `main`.
- **Storage-controller retries:** with backoff (20, was 128).
- **Minimal-image bumps:** timescaledb 2.31.0, pg_partman 5.5.0, pg_cron 1.6.8, pgbouncer 1.26.1 (security fixes) and the exporters. pgrag is removed.
- **Evidence:**
  - fork gate: lint, build, Rust tests, pg-check (core 225/225), regress selection;
  - `just check` 3748 tests;
  - live pass: every suite and e2e green.

## TE-RT RT1: realtime table editor (2026-10-21, shipped)

Branch `table-realtime`, merged `87d2d633`; spec `docs/superpowers/specs/2026-10-14-realtime-design.md`; plan and full history `docs/superpowers/plans/2026-10-16-realtime-rt1.md`.

### Live rows that respect each viewer's row-level security

- **Problem:** a shared table editor needs to show other people's changes as they happen, but streaming row contents to every viewer leaks rows that Postgres RLS hides from them.
- **What we built:** the stream carries only primary keys (our `chelabase_keys` decoder, fork round 3), and each key reaches a viewer only after a check run as that viewer's own role says they can read the row; Studio then re-reads the row as that role. Deletes reach a viewer only for keys they saw. One temporary slot per watched database is shared by all viewers and dropped when nobody watches.
- **Evidence:** `realtime_live` 17/17 including `the_invariant_holds_against_postgres` (a viewer receives a key only if their own read returns the row), an RLS viewer in three Playwright passes never received a hidden row's key in its WebSocket frames; splices reached the other member in 110–140 ms.

### Presence and safe concurrent edits

- **What we built:** each person's selected cell in their own colour (nested rings when several share a cell, your own outermost), an "editing" fill, hover tags with the acting-as role; saves carry the original values, so a concurrent change opens "Changed by someone else" (overwrite or take theirs) instead of silently winning; the row panel marks fields a live update changed and asks before discarding an unsaved draft on every path (incl. browser Back and closing the tab).
- **Evidence:** Studio suite 2030/2030; three Playwright passes with two to three signed-in members (51 regression scenarios in the second); presence colours tested for contrast and distinctness on both themes.

### Every watcher sees the true live state

- **Problem (found in the browser pass):** a viewer joining after the slot started streaming was told "waiting" forever.
- **What we built:** the holder seeds new watchers with its state and re-tells it every 5 s in every state; a fenced holder never claims live.
- **Evidence:** a property test over join/leave/holder sequences across two instances (`fr_status`), live rejoin and second-table tests.

## Neon fork round 3 (2026-10-15, shipped)

Branch `neon-fork-round-3`; spec `docs/superpowers/specs/2026-10-15-neon-fork-round-3-design.md`; plan and full history `docs/superpowers/plans/2026-10-15-neon-fork-round-3.md`. Fork heads: `chelabase/neon` `round-3` `629166cee`, `chelabase/postgres` `round-3-v17` `76ce76278c5`, `chelabase/rust-postgres` `chelabase/sasl-verified` `9a1b048b`.

### Realtime decoding that only ever sees primary keys

- **Problem:** realtime (RT1) needs to know which rows changed without reading row contents or creating publications in the customer's database; upstream offers `pgoutput`/`test_decoding`, which emit whole rows.
- **What we built:** `chelabase_keys`, a logical decoding output plugin in our Postgres fork that emits only the primary-key values of inserted, updated and deleted rows (as UTF-8 whatever the database encoding; SQL_ASCII refused), with a memory budget, and a locked-down `chelabase_realtime` replication login: temporary `chelabase_keys` slots it created only, no SQL, no startup parameters, hot-standby feedback ignored on logical slots.
- **What it unlocks:** RT1's row-change stream filtered by each viewer's RLS, with nothing installed in the customer's schema.
- **Evidence:** `chelabase_keys` regression suite (keys, kinds, limits, edges, encoding: 5/5), Python suites `test_chelabase_keys`, `test_chelabase_realtime_role` (including an invariant test over every replication command and message type the login can send), `test_chelabase_f2_edges`; live `compute_fork_live` (the realtime slot goes with a compute restart).

### Commits can't be acknowledged by an impostor standby

- **Problem:** in the original Neon base, any replication client could name itself `walproposer` (or match `*`) and become the synchronous standby, acknowledging WAL the safekeepers never stored.
- **What we built:** while the walproposer runs, only it can be a synchronous standby; client walsenders always get priority 0. Plain Postgres (no walproposer) keeps stock synchronous replication.
- **Evidence:** Python tests on a compute (a client named `walproposer` stays async, the walproposer stays sync) and on plain Postgres (stock behaviour); the flag is a C global, not a setting (tests: `SHOW`/`SET` fail).

### Team member logins locked to the platform

- **Problem:** per-member Postgres logins (`cb_m_*`) could be altered by a database owner or the member itself (a password, a granted role), and could log in from anywhere on the compute network; the reconcile only undid it later.
- **What we built:** only a superuser can change a member role (every `ALTER ROLE`/`GRANT`/`CREATE ROLE … IN ROLE` form, nested SQL included); member logins are accepted only from the orchestrator's addresses **and** with a client certificate from a platform CA. Computes serve TLS with per-compute certificates; the orchestrator, auth and the data API all connect over verified TLS 1.3, and a certificate refusal is told apart from a wrong password.
- **Evidence:** `test_member_role_guard`, `test_member_logins_hba`, `member_roles_live` (30/30, including a model test over random owner and admin actions), rust-postgres `sasl_verified` (12 + 2), orchestrator error-classification table tests, `app-users-e2e`, `rest_live`.

### New databases on a protected branch are born closed

- **Problem:** a database created from SQL on a protected branch was open, without the schema guard, until the next reconcile.
- **What we built:** while the branch is protected (a flag set by the platform, not a setting a session can change), every non-superuser `CREATE DATABASE` is born closed; Chelabase guards it, then opens it. Opening or templating a database is refused; renaming the branch's main database is refused too.
- **Evidence:** exhaustive Python matrices (flag on/off × session database kind × template × role), `member_roles_live` born-closed tests, the worker's exhaustive reconcile table test.

### No migration race at compute start

- **Problem:** compute_ctl ran its migrations after reporting `running`, so the data API setup could collide with them (`tuple concurrently updated`, seen in 6g and TE-B1).
- **What we built:** migrations run before `running`, never forwarded to the control plane, and a failure that leaves a privilege-removing migration unapplied stops the start; the worker waits for `running`.
- **Evidence:** `test_compute_migrations`, `test_ddl_forwarding_invalid_db`, the worker's wait-loop tests; the live pass's provisioning suites.

### Deferred

- A temporary slot's pageserver copy outlives a compute restart (found by this round's live pass): the first item of the next fork round, before RT1 runs on real computes.

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

### One front door, routed by host name (W1)

- **Problem:** Studio, the API and Zitadel each had their own port (3000, 8000, 8080), so a self-hosted install published several origins with CORS between them. Every script, test and config spelled out `localhost` ports, and the workbench couldn't look like production.
- **What we built:** all browser and API HTTP now goes through our Pingora gateway on one port, routed by host: `studio.`, `api.` and `auth.` (plus `docs.`, `landing.` and `mail.` in the workbench, under `chelabase.localhost`). The host settings are the single source: Studio's origin, the OIDC issuer, the API base, the Secure-cookie default and Zitadel's TLS mode are all derived from them. Startup refuses settings that could never work (unknown or identical hosts, a default port in a host, Studio and the API on different sites, a stale issuer, platform on plain http). Site hosts pass through untouched, websockets included (Vite's hot reload works through the gateway). Forwarded headers are always the gateway's own. Offline guards keep loopback-port literals and second copies of derived values out of the repo.
- **What it unlocks:** a production-shaped self-hosted entry (one port, three DNS names, TLS at the gateway or a front proxy) and W2's per-branch isolated stacks, where only the gateway port is published.
- **Evidence:** a model-based property test of the host-setting rules; gateway end-to-end tests (websocket upgrade, `Host` and bearer kept on the auth host, plain-text errors on sites, unknown host 404); shell tests of the derived values over scheme × host forms; and a full live pass through the new hosts: every live suite, ten e2e runs, and a browser pass covering sign-in, hot reload, Zitadel's console, Mailpit, docs and landing. The existing Zitadel instance moved to its new domain without a reset. Status: merged to `main` 2026-10-13.

### One isolated stack per checkout (W2)

- **Problem:** one machine had one dev stack. Every live test, e2e run and browser pass queued through a machine-wide lock, and moving the stack to another worktree restarted the whole app tier. Agents couldn't verify their own work live. Worktrees shared Zitadel's tokens, Studio's env and the proxy CA, and a worker's orphan removal saw every compute on the Docker daemon, so two stacks would have deleted each other's databases.
- **What we built:** `just workbench up|test|run|down|purge|status`, a Rust CLI over one parametrized Compose file. Each checkout gets its own project, volumes, network, Zitadel instance, state dir and host names (`studio.<branch>.chelabase.localhost:<port>`). Only the gateway port and the proxy's Postgres port are published, from a registry (8100–8199 / 4500–4599). Up to 4 stacks run at once and a fifth waits in a FIFO with a timeout; the registry and queue are flock-guarded and tested under concurrency. The app tier, Studio and a test runner run inside the stack from host-built binaries, so URLs are the same inside and outside. Computes carry a stack label and are listed, located and removed only within their stack. Live tests are archived with nextest on the host and run in the runner. `just wt-remove` purges a worktree's stack, and main migrates in place keeping its volumes.
- **What it unlocks:** every implementer and reviewer runs its own worktree's stack, so a task's live check happens inside the task, in parallel. No more stack swapping, and main stays usable during a branch's live pass.
- **Evidence:**
  - CLI unit tests (184), covering slugs, the registry and FIFO slots (stress-tested 20×), the stack env, process stop, purge safety and the migration.
  - Offline guards for the compose shape, the recipes, runner-only live tests and retired names.
  - A live pass on branch stacks: every live suite, all 12 e2e runs, and `neon-e2e` on two stacks at once with neither losing a compute. The slot cap made a third `up` wait and time out naming both stacks, and `wt-remove` purges left nothing behind. A fresh stack comes up seeded in about 1.5 minutes from a cold worktree. A browser pass covered sign-in, Zitadel's console, hot reload, Mailpit, docs and landing.
- **Status:** merged to `main` 2026-10-14; main runs on the workbench.


### Protected branches that can't be bypassed (PB)

- **Problem:** TE-A's guard stopped Developers' member logins from changing a protected branch's schema, but anyone logging in directly with another Postgres login role could still do it. And code planted while the branch was unprotected (a trigger, a rule, a SECURITY DEFINER function, a function called from a policy or a default) runs inside an Admin's session later, where no guard applies.
- **What we built:**
  - **A per-branch "guard login roles" switch.** With it on, every login role except the owner, Admins' member logins and per-branch exemptions is refused schema changes. The rule is enforced inside Postgres by the guard's event triggers, from the role's first statement, roles created later from SQL included. It follows who logged in, so removing a role's LOGIN mid-session doesn't free its open sessions.
  - **Exemptions,** stored per branch by name. One is honored only for a role no guarded role can administer, so a guarded login can't create its own exempt role.
  - **New databases carry the guard:** it lives in `template1`, and creating a database asks for a reconcile.
  - **Inheritance and recovery:** child branches copy the parent's switch and exemptions, and a periodic sweep over every branch puts back any setting a lost update left stale.
  - **A review**, run by the worker with platform rights over every live database. It lists code owned by anyone but the owner that can run in someone else's session: triggers, rules, SECURITY DEFINER functions, and functions reachable from policies, defaults, checks, index expressions, generated columns, views, operators, domains, range types or any SECURITY DEFINER caller.
  - **Protect and acknowledge need the review's current hash.** The hash includes each object's definition, so a body replaced in place shows up as changed.
  - **Fixes in place:** reassign to the owner, or drop, re-checked against the worker's own scan first.
  - **Studio:** the review sits in the Protect and Protection panels, with a "Review changed" badge and exemption switches in the Roles panel.
- **What it unlocks:** a production branch whose schema only Admins and merges can change, through any client, with a reviewed answer to "what code here can run as me?" before protecting it.
- **Evidence:**
  - A live test that tries every known escape on a real compute:
    - databases made from `template1` and `template0`;
    - a role created under an exempt name;
    - an honored exemption still allowed.
  - A live NOLOGIN-flip test, and the scan's live tests (every reason kind, fixes, body digests).
  - Hostile exemption names, which reach the guard only as hex.
  - The full live pass on the merged branch: `live_db` 189, `review_live` 2, `member_roles_live` 24, `roles_live` 42, the orchestrator's live API tests 26, and `sql-editor-e2e`.
  - `just check` green.
  - Not yet seen in a browser: Studio's create form didn't respond on the branch stack, an issue outside this step.
- **Known residual:** a database a guarded login makes from `template0` is unguarded until the reconcile its creation asks for (seconds). Closing it needs the fork and is queued for the next fork round, with REASSIGN OWNED.
- **Status:** merged to `main` 2026-10-14.

### A table editor that shows the SQL before it touches your schema (TE-B1)

- **Problem:** Studio's old Tables console was a flat grid: no structure editing, no review of what a change would run, and no way to see what a role can actually read.
- **What we built:**
  - **Staged structure changes.** Columns, constraints (PK, unique, FK with NOT VALID + validate, check), table rename/move/comment/drop, new tables and enums stack up as pending edits. Every change re-runs a dry run on the branch and shows the SQL, lock level, table rewrites and dependents (with a cascade checkbox) before one transactional Apply.
  - **A changeset model that stays correct under edits:** folding, removing an edit and its dependents, and addressing every operation to the table's name at its position (after pending renames/moves), checked by property tests.
  - **A row grid** (virtualized, inline type-aware editing, keyboard navigation) with a row panel for inserts, long values and NULL/default toggles; select a page or every matching row, with an exact `count` from the query engine (a new `count` op, also for apps).
  - **Act as a role.** Reads and writes run as one of your granted Postgres roles; a project Admin holds every role of their project (narrowable per database), and the data API's `anon`/`authenticated` only when a grant names them.
  - **Honest defaults:** new tables offer "Turn RLS on" and say plainly what the data API exposes while RLS is off.
- **Evidence:** Studio 1 370+ unit and property tests; `just check` green; live: the orchestrator API suite 449/449, `rest_live` 37/38 (the one failure is the known, parked compute_ctl migration race during provisioning), `sql-editor-e2e` and `console-e2e` green with the new act-as rules; browser passes as an Admin and a Developer.
- **Status:** merged to `main` 2026-10-14.
