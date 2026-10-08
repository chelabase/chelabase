# Step 6g: three-way schema merge

**Date:** 2026-10-06
**Status:** Spec, waiting for the user's review.
**Branch:** `schema-merge` (`.worktrees/schema-merge`)
**Roadmap:** `docs/roadmap/2026-09-28-neon-and-data-api.md`, item 6g.

## Goal

Move schema changes between a branch and its parent, in both directions, with a three-way merge: the base (the schema at the branch point, or at the last merge), the target now, and the source now. Main's data stays canonical; rows never move between branches. Conflicts are flagged the way git flags them, resolved per item, rehearsed on a throwaway copy of the target, then applied in one transaction plus a resumable online phase.

Nothing on the market does this for Postgres: Neon offers branch restore (a data overwrite) and a two-way schema diff view, Supabase's pg-delta is a two-way diff, PlanetScale's three-way deploy requests are MySQL only.

## How we got here

Step 6g started as "branch promotion" (replace the parent's timeline with the branch's state, keeping its connection string). Research on 2026-10-06:

- **Hosted Neon** has no "promote". Its closest feature is branch-to-branch restore: a full overwrite of the target's data, a backup branch `<name>_old_<timestamp>`, the target's children moved onto it, "connections temporarily interrupted" for "a few seconds". It doesn't support merging branches and recommends running migrations against production.
- **A running compute can't change timeline:** `neon.timeline_id` is `PGC_POSTMASTER` (fork `pgxn/neon/libpagestore.c`), compute_ctl's `/promote` only promotes a replica on the same timeline, and LFC prewarm needs endpoint storage we don't run. Any timeline swap restarts Postgres (2–4 s today).

The user then dropped data promotion (main's data must stay canonical: it takes real writes after a branch is cut) and chose a three-way schema merge, as value other products don't offer.

**Parked (not 6g):** cutting the restart gap of restore and reset-to-parent: a blue/green compute switch, a read-only freeze before the switch (writes accepted during a switch land in the backup today), and a `wake_compute` gated on readiness (today the proxy can route a client to a compute before its role resync).

## Decisions (user, 2026-10-06)

| # | Decision |
|---|---|
| D1 | **No data promotion.** Only schema moves between branches; data stays where it is. |
| D2 | **Three-way merge**: base, target, source. |
| D3 | **Object scope (whole feature):** schemas, tables, columns, constraints, indexes, enums, sequences, RLS, policies, grants, roles, views, materialized views, functions, procedures, triggers. Anything unmodelled is reported, never dropped. |
| D4 | **Base:** a snapshot stored when the branch is created, reset or merged-and-kept, with a live fallback (a read-only compute at the recorded point). |
| D5 | **Conflicts are order-sensitive** (PlanetScale's rule): the same sub-object changed differently on both sides, or both sides adding columns to one table. Identical changes are overlaps, not conflicts. Conflicts are per sub-object, not per property. |
| D6 | **Resolution per conflict:** keep target, take source, or own SQL. |
| D7 | **Identity:** objects that existed at the base are matched by OID (columns by table OID + attnum), so renames are exact; newer objects by name. |
| D8 | **Every merge is rehearsed** on a throwaway branch of the target, and the result is checked against the expected model. |
| D9 | **Apply:** one transaction with a lock timeout, then a resumable online phase (concurrent index builds, `NOT VALID` + `VALIDATE`). |
| D10 | **User SQL steps** can be inserted into the plan; the engine suggests backfills (suggestions ship in 6h). |
| D11 | **After merge:** keep (default; the base advances), reset to parent, or delete. |
| D12 | **Roles** the source added are created on the target through the control plane with a new password shown once; passwords are never copied. |
| D13 | **Directions:** merge into parent, and update from parent. |
| D14 | **A stored merge request** with states and history; stale if the target or source changes after rehearsal. |
| D15 | **Surfaces:** API, Studio merge page, `.sql` export, CLI, docs. |
| D16 | **One Postgres database per merge request.** |
| D17 | **Engine:** a Rust crate reading the live catalog (not pg-delta, not pg_dump + pg_query). |
| D18 | **Vertical slices:** 6g = the full flow for the table-level objects (Part 1); 6h = views, materialized views, functions, procedures, triggers and backfill suggestions; 6i = CLI commands. |

## Rulings (Claude, mechanical; listed for the user)

| # | Ruling | Cost if wrong |
|---|---|---|
| R1 | Merging into a protected branch needs an explicit confirm, as the SQL editor does. | One extra click; easy to drop. |
| R2 | Phase 1's `lock_timeout` defaults to 5 s, adjustable per merge. | A default change. |
| R3 | Reconcile deletes `rehearsal` branches older than 30 minutes whose merge isn't rehearsing, and static base computes older than 10 minutes. | A constant. |
| R4 | Bootstrap-owned objects are recognised by a fixed list in the crate, next to the bootstrap SQL. | A missed object shows up as a change to merge; the rehearsal or review catches it. |
| R5 | Phase 2's online transforms apply to tables that existed on the target before the merge, whatever their size; new tables get everything in phase 1. | Small tables take a few extra steps. |
| R6 | A failed base read at branch create doesn't fail the create; the live fallback covers it later. | A slower first merge. |

## Part 1: scope and the schema model

**Crate `crates/chelabase-schema`:** reads one Postgres database's catalog into a normalized model; three-way diff, conflict rules, integrity check and planner; no HTTP, no metadata access. The worker uses it.

**Merged in 6g:**
- schemas (owner, comment);
- tables (owner, comment, RLS enabled/forced);
- columns (type with typmod, collation, default expression, NOT NULL, identity, generated expression, comment);
- constraints: primary key, unique, foreign key, check, exclusion (definition from `pg_get_constraintdef`, deferrable, `NOT VALID` state);
- indexes (from `pg_get_indexdef`, normalized: method, keys or expressions, INCLUDE, predicate, uniqueness);
- enums (values and order);
- sequences (standalone, owned by a column, identity), with their parameters;
- policies (command, permissive/restrictive, roles, USING, WITH CHECK);
- grants on schemas, tables, columns and sequences, and default privileges;
- roles (attributes and memberships, never passwords).

**Reported, not merged in 6g:** views, materialized views, functions, procedures and triggers (6h); domains, composite types, partitioning, extensions, rules, casts, operators, publications, foreign tables and user event triggers. The preview lists each as "changed on the source, apply by hand". When a merged object depends on a reported one (a default or policy calling a function the source added), the plan says so, and the rehearsal fails clearly if it's missing.

**Always excluded:** `pg_catalog`, `information_schema`, `pg_toast`, the `chelabase` schema; objects that belong to an extension (`pg_depend` type `e`); objects Chelabase's bootstrap owns (data API event triggers, the app users table's guard trigger and its fixed columns, `authenticator` and the data API roles; R4). Columns users add to the users table do merge.

**Identity:**
- Every object records its catalog class and OID (columns: the table's OID + `attnum`) beside its qualified name.
- An object on the target or source whose class and OID (through the identity map, Part 2) exist in the base is that base object. A rename or `SET SCHEMA` is a change of name, not a drop and a create.
- Objects created after the base are matched by qualified name; their OIDs are never used, since each side allocates OIDs independently.
- A drop and re-create under one name gets a new OID: a drop plus a create, with a data-loss flag.

**Model version:** an integer carried by every model. A stored snapshot of another version isn't trusted (Part 2).

**Reading:** as the owner, in one `REPEATABLE READ READ ONLY` transaction, using `pg_get_*def`, `pg_get_expr` and `format_type`, so expressions come back in Postgres's normalized text and compare stably.

## Part 2: base snapshots

**Table `branch_schema_bases`**, one row per branch and Postgres database name: the model (`jsonb`) and its version; `source` (`create` | `reset` | `merge` | `update`); the live location (timeline id + LSN); `taken_at`.

**Identity map** (stored apart from the model, so it survives a model upgrade): per base object, its OID on the parent side and on the branch side. Initially equal. After a merge creates objects on the target, the worker reads the target, finds them by name and records their OIDs.

**When a base is written:**
- **Create:** after the branch's compute is ready and bootstrapped, before it's marked ready, the worker reads every database on the new branch (exactly the parent at the branch point). A failed read leaves the base missing; the branch still becomes ready (R6).
- **Reset to parent:** a new base from the parent's head, and `branched_at_lsn` is updated to the reset point (today it stays stale).
- **Merge or update with keep:** the base becomes the source's schema at the moment the merge read it (git's merge base); live location = the source's timeline at that LSN.
- **Restore of the branch** (amended 2026-10-06, E18): bases are a **history**, not one row. Every write appends an entry that also records the branch's own position (its timeline and LSN) where the base became valid. A restore to LSN X on timeline T appends a copy of the newest entry on T at or before X, positioned at the new timeline's start. So after a restore to before a merge with keep, the next merge compares against the ancestor that was true at that point. The latest entry is the current base. Entries go when the branch is deleted (about 10–25 KB per entry per database).

**Live fallback** (base missing, or another model version): a temporary read-only compute in Neon's static mode (`ComputeSpec.mode = Static(lsn)`), pinned to the recorded timeline and LSN, with its own transient endpoint record that the proxy never routes. The worker reads the model, stores it as the new snapshot, and removes the compute and record. Reconcile removes leftovers (R3).

**Retention** (E17, corrected 2026-10-06, narrowed 2026-10-08 after Neon fork round 2's patch 4): Neon's GC keeps a branch point's data on the parent's timeline while the branch exists (`retain_lsns`), and the pageserver serves reads at such a retained point (or a leased LSN) below the timeline's applied GC cutoff. Within the history window (7 days by default) the live fallback reads the recorded location as is. Behind the GC cutoff, a location at its timeline's branch point (a create or reset base) is read on the ancestor timeline at that point, which retains it while the timeline exists. Re-baseline (below) is left for a base that is truly unavailable: no stored snapshot, and a point that is neither within the window nor retained (for example a keep merge's base, recorded on the source's timeline at the merge, or a timeline that is gone). The merge says why. This matters only when the stored base is missing (a failed read at create), from an older model version, or from a branch made before 6g.

**No base available:** the merge says so and offers **re-baseline**: the target's current schema becomes the base (a two-way diff). Every difference then counts as a source change, and the preview warns that the target's own recent changes will show as things to undo.

## Part 3: three-way diff, conflicts and resolution

**Terms:** source = where changes come from; target = what receives them. Into parent: source = branch, target = parent. From parent: the reverse. B = base, S = source now, T = target now.

**Changes:** `diff(B,S)` and `diff(B,T)`, one entry per sub-object (schema, table, column, constraint, index, enum, sequence, policy, grant, role): create, drop, or alter (properties, name included).

| Source | Target | Result |
|---|---|---|
| changed | unchanged | apply |
| unchanged | changed | nothing |
| same change | same change | overlap (nothing) |
| changed | changed differently | conflict |
| dropped | altered (or the reverse) | conflict (drop vs edit) |
| created | created under the same name, different definition | conflict |
| adds columns to a table | adds columns to the same table | order conflict |

**Integrity check:** the merged model `M` must have every reference resolve (a foreign key to a column the target dropped; a policy naming a role that won't exist). Each failure is a **dependency conflict** naming both items. The rehearsal is the final check.

**Resolutions** (every conflict needs one before rehearsal):
- **keep target:** the item stays out of the plan;
- **take source:** the plan moves the target's version to the source's; data loss is flagged;
- **own SQL:** replaces the item in the plan. After the rehearsal, the preview shows what it actually produced, for the user to confirm; checks compare against that result, not the model;
- **order conflicts:** "target's columns first, then the source's", or own SQL (Postgres can't reorder columns without a rebuild, so "take source" isn't offered).

**Drop acknowledgement:** every step that loses data (a dropped table or column, a type change that rewrites values) needs a per-item acknowledgement before apply, conflict or not.

**Output:** `M = T + applied changes + resolutions`; the plan is the two-way diff `T → M` through the identity map.

**Roles:** matched by OID in `pg_authid`; attributes and memberships compared; passwords never.

## Part 4: planner and two-phase apply

**Plan:** an ordered list of steps; each step has a stable id, kind (`generated` | `user`), phase, SQL, the items it implements, and flags (data loss, table rewrite, locks taken). User steps go where they're inserted and can move within their phase.

**Order:** renames first; then drops in reverse dependency order (a policy before the column it uses); then schemas → enums → sequences → tables → columns → primary keys, unique and check constraints → foreign keys → indexes → RLS flags → policies → grants and default privileges.

**Phases:**
- **Phase 0, roles, through the control plane:** create the roles the source added (caps, reserved names and protected-branch rules apply; a new password); attribute changes to API-managed roles also go through it (resync would undo SQL changes to them). Role drops are reported, never run.
- **Phase 1a, only when needed:** an enum value added by the plan and used later in it (`ALTER TYPE … ADD VALUE` can't be used in the transaction that adds it) gets its own committed transaction first; harmless if phase 1 then fails.
- **Phase 1, one transaction:** as the owner on the target's compute, DDL forwarding on; `SET LOCAL lock_timeout` (R2) and `statement_timeout`; it first re-checks that the target's schema fingerprint equals the rehearsed one (otherwise: stale, rollback, nothing changed). It also writes `chelabase.merge_applied(merge_id)` for recovery (Part 5). All or nothing.
- **Phase 2, online, for tables that existed on the target before the merge (R5):**
  - new index: `CREATE INDEX CONCURRENTLY`;
  - changed index: a replacement under a temporary name built concurrently, the old one dropped concurrently, then renamed;
  - new foreign key or check: added `NOT VALID` in phase 1, `VALIDATE CONSTRAINT` here;
  - new unique constraint: a concurrent unique index, then `ADD CONSTRAINT … USING INDEX`;
  - new NOT NULL: `CHECK (col IS NOT NULL) NOT VALID` in phase 1, validated here, then `SET NOT NULL` (no scan, thanks to the check), then the check dropped.

**Phase 2 resumes:** each step checks state first (a valid index is skipped; an invalid one is dropped concurrently and rebuilt; a validated constraint is skipped); results go to the merge log, and a retry continues where it stopped. A `NOT VALID` constraint already holds for new rows.

**After apply:** read the target and compare with `M` (a mismatch: status `applied_with_drift`, difference shown); record the new OIDs in the identity map; write the new base if kept; run the after-merge choice (its usual refusals apply).

**Protected target:** an explicit confirm (R1). **New role passwords** are sealed in metadata and revealed once from the merge page.

## Part 5: merge requests, rehearsal, jobs and recovery

**Metadata migration:**
- `merges`: id, database, source and target branch, direction (`into_parent` | `from_parent`), Postgres database name, status, base/source/target fingerprints (SHA-256 of the normalized models), classification, resolutions, user steps, drop acknowledgements, options (lock timeout, after-merge choice), plan, rehearsal result, `version` (optimistic concurrency), error, created_by, timestamps.
- `merge_events`: an append-only log of state changes and step results with timings.
- `branch_schema_bases` and the identity map (Part 2).
- `BranchKind::Rehearsal`: not counted toward the cap, hidden from the branch tree, never routed by the proxy.

**States:**

```
computing → draft → rehearsing → rehearsed → applying → online → finishing → done
                ↑________ any edit _______|
failed (with its phase) · stale · cancelled · applied_with_drift
```

- An edit in `draft` or `rehearsed` returns to `draft`.
- Stale: the source or target fingerprint changed by the apply request (or at phase 1's re-check). "Recompute" makes a new draft that keeps the choices still relevant.
- At most one open merge per (source, target, Postgres database); at most one applying merge per target.
- Reset, restore and delete of the source or target are refused while a merge is rehearsing or applying; deleting a branch cancels its draft merges.

**Rehearsal:** a `rehearsal` branch from the target's head (real data, so failing constraints show); all phases run, including phase 2 (phase 0 there creates roles directly as `NOLOGIN`, no password, no metadata); the worker reads its schema and compares it with `M` (own SQL compared with what it produced); per-step timings and lock waits are recorded; the rehearsal branch is deleted. A failure returns the merge to `draft` with the step and the Postgres error.

**Jobs:** subject `chelabase.merge`, jobs `compute`, `rehearse`, `apply`, `resume`, `cancel`; the branch consumer's pattern (claim, keep the claim alive, redeliver on a crash).

**Recovery:**
- phase 1 exactly once: after a crash, the `chelabase.merge_applied` marker decides (present: go on to phase 2; absent: back to `rehearsed`, "interrupted, nothing applied");
- phase 0 idempotent: a role already in metadata is skipped;
- phase 2 resumes from the log;
- reconcile: delete old `rehearsal` branches and static base computes (R3); re-queue `rehearsing` / `applying` / `online` / `finishing` merges whose claim went silent.

## Part 6: surfaces

**API** (orchestrator, utoipa, the existing error shape and codes):

| Route | Purpose |
|---|---|
| `POST …/branches/{bid}/merges` `{ direction, database? }` | 202, a new merge (`computing`); `database` defaults to the main one. |
| `GET …/databases/{id}/merges?branch=` | History and open merges. |
| `GET …/merges/{mid}` | Classification, conflicts (base/target/source), plan, rehearsal, log, `version`. |
| `PATCH …/merges/{mid}` `{ version, resolutions?, steps?, acknowledgements?, options? }` | Edit a draft; a stale `version` is 409. |
| `POST …/merges/{mid}/rehearse`, `/apply` `{ confirm? }`, `/recompute`, `/retry`, `/cancel` | 202. Apply refusals: `merge_conflicts_open`, `merge_drops_unacknowledged`, `merge_not_rehearsed`, `merge_stale`, `merge_busy`, `confirm_required`. |
| `GET …/merges/{mid}/plan.sql` | Export: the final plan as SQL, phase headers as comments, phase 2 marked "run outside a transaction", roles as comments (no passwords). |
| `POST …/merges/{mid}/role-passwords/reveal` | New roles' passwords, once. |

**Studio:**
- branch actions "Merge into ‹parent›" and "Update from ‹parent›", disabled with reasons (main, not ready, a merge already open, with a link);
- merge page `…/databases/$d/merges/$mid`: header (direction, status, base, re-baseline warning); Changes (by object type; apply / overlap / conflict; base/target/source side by side); Conflicts (keep target / take source / own SQL in the 6d SQL editor with completion); Plan (steps by phase; insert, move, delete own steps; drop acknowledgements; lock timeout); Rehearsal (timings, lock waits, failure, own-SQL outcomes to confirm); Apply (progress by phase, polling while not terminal; the log); Finish (after-merge choice, password reveal, export);
- a merges list per database (history).

**Docs** (`apps/docs`): Using, "Merging schema between branches"; Reference, the generated OpenAPI pages; Architecture, "The schema merge engine" (identity, three-way rules, phases, recovery).

**Outside 6g:** the landing page's last hero label becomes "staging merged into main" (branch `landing-page`). CLI commands are 6i.

## Part 7: testing

- **Engine unit tests (model fixtures):** each classification row, every conflict kind, overlap and order conflicts; identity (rename and `SET SCHEMA` as renames; drop and re-create as drop + create with data loss; newer objects by name; the identity map after a merge); the integrity check; planner order and the phase 1/1a/2 transforms; user steps kept within their phase; the `.sql` export.
- **Introspection round trip (live, `just live`):** a DDL fixture covering every 6g object type → apply to a scratch database → read → generate "create everything" from an empty model → apply to a second database → read; the models are equal. Also: Chelabase's objects excluded, unmodelled objects reported, two reads identical.
- **Worker with fakes (journal-order tests, like `branch_switch.rs`):** base written on create, reset and keep; the static-compute fallback; re-baseline; the state machine (edits back to draft, stale via fingerprint and phase 1's re-check, one-open and one-applying); branch operations refused while busy; delete cancelling drafts; recovery (a crash before and after phase 1 commits, phase 0 idempotence, phase 2 resume with an invalid index rebuilt, rehearsal and static-compute cleanup).
- **Live scenarios (`services/worker/tests/merge_live.rs`, ignored by default):** into parent and from parent on real branches; a rename on the branch with a new column on main; each resolution; a NOT NULL failing in rehearsal on main's rows and passing after an inserted backfill step; a role created through the control plane with its password revealed once; a protected target needing confirm; keep, reset and delete after merge; a second merge carrying only newer changes.
- **Metadata and API:** live tests for the `merges` and `branch_schema_bases` queries; orchestrator API tests for every route and refusal, the 409 on a stale `version`, and the export.
- **Studio:** unit tests for the merge page, resolution controls, plan editor, and actions with their disabled reasons; a Playwright pass (branch, change both sides with one conflict, merge, resolve, rehearse, apply, check main).
- **End to end:** `just merge-e2e` through `just live`; `openapi/data.json` regenerated and checked; the docs build.

## Out of scope (6g)

- Views, materialized views, functions, procedures, triggers, and backfill suggestions (6h); CLI commands (6i).
- Moving data between branches; merging between siblings or across more than one level.
- A review/approval gate before apply (needs platform-mode project roles).
- Downtime work for restore and reset-to-parent (parked; see "How we got here").
