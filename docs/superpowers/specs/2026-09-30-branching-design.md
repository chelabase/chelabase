# Branching (roadmap step 5)

**Date:** 2026-09-30
**Status:** Draft, for the user's review.
**Roadmap:** `docs/roadmap/2026-09-28-neon-and-data-api.md`, step 5.
**Branch:** `branching` (worktree `.worktrees/branching`).

## Goal

A database can have branches: copies of another branch's data, made instantly by Neon at the latest state or at a past point in time. Each branch has its own compute, its own connection details and its own password, and shows up in Studio. A branch can be reset to its parent's latest state, and any branch, main included, can be restored to a past point without changing its connection string.

What branches are for (the user, 2026-09-30):
- dev and preview copies of production data;
- recovery from mistakes (point in time);
- trying a migration before running it on main;
- staging environments for the data API (step 6).

**Done when:**
- Create at "now" and at a past time works end to end through the platform API, the proxy and Studio.
- Reset to parent, restore (main included), reveal, reset password and delete also work end to end.
- The live story passes:
  1. write row A and note the time, then write row B;
  2. a branch at that time has A but not B;
  3. reset to parent brings B;
  4. restoring main to that time removes B from main, while the backup branch keeps it;
  5. deleting everything leaves no compute, timeline or row.
- The existing per-database routes still work as aliases for main.

## Decisions

These were made in brainstorming on 2026-09-30.

| Question | Decision | Why |
|---|---|---|
| Scope of recovery | Branch at a timestamp or LSN, **restore** a branch to a point, and **reset to parent**. | The user wants all three purposes: recovery, dev copies that refresh, and safe migrations. |
| Credentials | **Each branch gets its own `postgres` password.** | A dev or preview credential that leaks can't open main. Apps switch the whole connection string between branches anyway, because the host differs. |
| Computes | **Always on**, like main, with a per-database cap `max_branches` (default 5, not counting main). | Scale to zero is step 7. The cap keeps preview branches from exhausting a single server. |
| History | **`history_window` per database, default 7 days**, allowed from 1 hour to 30 days. It becomes Neon's `pitr_interval`. | Neon's usual default. Longer windows cost pageserver and S3 storage. |
| Shape | **A tree**: a branch can be created from any branch. | It matches Neon's model at no extra storage cost. |
| Data model | **A `branches` table, where main is a branch.** Per-connection state moves off `databases`. | There is one code path for main and for branches, and restore and reset are the same operation for both. Rejected alternatives: a branch as another `databases` row (it overloads "database" and special-cases the cap, settings and cascades); a branches table for non-main only (two code paths forever). |
| Children and restore/reset | **Restore and reset-to-parent are refused while the branch has `normal` children.** Restore backups don't count as children. Deleting a branch also deletes its backups. | Neon's `detach_ancestor` re-parents only some children, which is confusing, so v1 stays strict; this can be relaxed later. A restored timeline is detached from its backup, so the backup doesn't block anything. It's the branch's own history, so it goes with the branch. |

### Findings (Neon at `NEON_REF` `fa504217`)

- **Timestamp → LSN:**
  - `GET /v1/tenant/{t}/timeline/{tl}/get_lsn_by_timestamp?timestamp=<rfc3339>&with_lease=true` reaches the pageserver through the storage controller's GET passthrough (`storage_controller/src/http.rs`, the `/v1/tenant/:tenant_id/*` route).
  - It answers `{ lsn, kind }` with `kind` one of `present`, `past`, `future` or `nodata` (`pageserver/src/http/routes.rs`).
  - `with_lease=true` pins the LSN against garbage collection for `lsn_lease_length_for_ts`. That closes the race between resolving a point at the edge of the window and creating the timeline.
- **Detach:** `PUT /v1/tenant/{t}/timeline/{tl}/detach_ancestor` is a storage-controller route. Its `detach_behavior=multi_level_and_no_reparent` detaches through every level of ancestors without re-parenting anything (`libs/pageserver_api/src/models.rs`, `DetachBehavior`). That fits the no-children rule.
- **History window:** `PATCH /v1/tenant/config` with `{ tenant_id, pitr_interval }` (`TenantConfigPatch.pitr_interval`, a duration string).
- **Already in `cheladb-neon`:**
  - `StorconClient::create_timeline(tenant, timeline, pg_version, Some((ancestor, Some(lsn))))`;
  - `get_timeline` (with `last_record_lsn` and `ancestor_lsn`);
  - `delete_timeline`.
- **Not yet in `cheladb-neon`:** `lsn_by_timestamp`, `detach_ancestor` and `patch_tenant_config` need adding.
- **`compute_ctl` can't move a running compute to another timeline.** The timeline is fixed when the compute starts from its spec. Restore and reset therefore recreate the compute container, keeping the same endpoint id, host and password.

## Architecture

The same layout as step 4. The orchestrator answers the platform API and publishes jobs. The worker owns every lifecycle flow through its ports: metadata, storage, `ComputeBackend`, proxy cache and spec pusher. The internal listener keeps serving specs, reading an endpoint's timeline from `neon_endpoints`, so a timeline switch needs only a metadata update and a compute restart.

New pieces:
- **`cheladb-neon`:** `StorconClient::lsn_by_timestamp`, `detach_ancestor` and `patch_tenant_config`.
- **`cheladb-metadata`:** the migration below, and branch queries with the same conditional-write style as step 4 (claim, set_ready, mark_deleted, each guarded on status).
- **`cheladb-nats`:** jobs `branch.create`, `branch.reset_to_parent`, `branch.restore` and `branch.delete`, and a `database.settings` job for the history window. Each job carries the branch id and, where needed, the resolved request.
- **Worker:** `branch_create.rs`, `branch_reset.rs`, `branch_restore.rs` and `branch_delete.rs`, plus extensions to `provision.rs`, `delete.rs`, `reset.rs` and `reconcile.rs`, which become branch-aware.
- **Orchestrator:** the branch routes, the settings route, and the aliases.
- **Studio:** the branch tree, actions and settings.

## Data model

One migration. It can't be undone, so it gets a per-task review.

```sql
CREATE TABLE branches (
    id                 uuid PRIMARY KEY,
    database_id        uuid NOT NULL REFERENCES databases (id),
    name               text NOT NULL,
    parent_branch_id   uuid REFERENCES branches (id),          -- NULL only for main
    kind               text NOT NULL DEFAULT 'normal',         -- 'normal' | 'backup'
    status             text NOT NULL,                          -- pending | provisioning | ready | resetting | failed | deleting | deleted
    timeline_id        text,                                   -- 32 hex; NULL until created
    pending_timeline_id    text,                               -- created by reset/restore, not yet switched to
    pending_timeline_at    timestamptz,                        -- when it was recorded
    superseded_timeline_id text,                               -- switched away from by reset, not yet deleted
    endpoint_id        text REFERENCES neon_endpoints (id) ON DELETE SET NULL,  -- NULL for backups
    owner_password_enc text,
    branched_at_time   timestamptz,                            -- what the user asked for, if a time
    branched_at_lsn    text,                                   -- what it resolved to
    error              text,
    created_at         timestamptz NOT NULL DEFAULT now(),
    updated_at         timestamptz NOT NULL DEFAULT now(),
    deleted_at         timestamptz,
    history_start      timestamptz                             -- where the history begins; NULL = unknown
);
CREATE UNIQUE INDEX branches_live_name ON branches (database_id, name) WHERE status <> 'deleted';
CREATE UNIQUE INDEX branches_live_main ON branches (database_id) WHERE parent_branch_id IS NULL AND status <> 'deleted';
CREATE UNIQUE INDEX branches_endpoint ON branches (endpoint_id) WHERE endpoint_id IS NOT NULL;
CREATE INDEX ON branches (parent_branch_id);

ALTER TABLE databases
    ADD COLUMN tenant_id text,
    ADD COLUMN history_window interval NOT NULL DEFAULT interval '7 days',
    ADD COLUMN max_branches int NOT NULL DEFAULT 5;
```

- **Backfill:**
  - Every non-deleted database with an `endpoint_id` gets a `main` branch. The branch takes the database's status and `owner_password_enc`, its `endpoint_id`, and its `timeline_id` from `neon_endpoints`.
  - `databases.tenant_id` is filled from that endpoint.
  - Then `databases.endpoint_id` and `databases.owner_password_enc` are dropped.
  - Deleted databases get no branch.
- **The exact SQL, including status handling during the backfill**, is pinned in the plan and tested live against a copy of pre-migration rows.
- **`neon_endpoints.timeline_id`** stays one row per branch compute. Restore and reset update it.
- **`databases.status`** is the lifecycle of the whole database. Branch statuses are independent, except that a deleting database moves all its branches to deleting.
- **Names:**
  - A branch name is 1–63 characters from `[a-z0-9_-]`, starting with a letter or digit. `main` is reserved for the root.
  - Backup names are `<name>-before-restore-<yyyymmddThhmmssZ>`, truncated to fit.

## API (gateway → orchestrator)

| Route | Result |
|---|---|
| `GET /v1/databases/{id}/branches` | The tree: `id`, `name`, `parent_branch_id`, `kind`, `status`, `host`/`port` (ready branches with a compute only), `branched_at_time`, `branched_at_lsn`, `created_at`, `history_start` (null when unknown), `error`. |
| `POST /v1/databases/{id}/branches` `{ name, parent_branch_id?, at?: { timestamp } \| { lsn } }` | 202 and the branch. The parent defaults to main, and `at` to now. |
| `GET …/branches/{bid}/connection` | The same shape as today's reveal. |
| `POST …/branches/{bid}/reset-password` | Step 4's reset, per branch. |
| `POST …/branches/{bid}/reset-to-parent` | 202. |
| `POST …/branches/{bid}/restore` `{ at: { timestamp } \| { lsn } }` | 202, with the backup branch's id. |
| `DELETE …/branches/{bid}` | 202. |
| `PATCH /v1/databases/{id}` `{ history_window?, max_branches? }` | 200. The history window is pushed to the tenant by a worker job. |
| `GET /v1/databases/{id}/connection` and `POST …/reset-password` | Aliases for main, unchanged. |
| `/v1/me`, `GET /v1/databases/{id}` | Gain `branch_count`. `host`/`port` are main's. |

**Refusals** at the API, checked in the same transaction as the write where a race is possible:
- 404: unknown branch.
- 409:
  - a live branch already has that name;
  - the cap is reached (the message names the cap and the `PATCH` that raises it);
  - delete, reset or restore of a branch that has `normal` children (the children are named). Its backups don't block a delete; they are deleted with it;
  - delete or reset-to-parent of main;
  - a parent or branch that isn't ready;
  - reveal, reset-password, reset-to-parent or restore on a backup ("this is a restore backup with no compute; create a branch from it to connect"). A backup can be deleted, or used as a parent.
- 422:
  - a bad name;
  - a malformed LSN or timestamp;
  - an LSN ahead of the parent's head;
  - a point outside the history window.
- 503: the worker isn't running, as today.

**The cap** counts live `normal` branches other than main. The insert takes the database row `FOR UPDATE`, counts, and inserts, so two concurrent creates can't both pass.

## Flows (worker)

Every flow claims its branch row with a conditional write, as provisioning does. It records what it creates before creating it, so a failure or a delete can always tear it down. Its failures land in `branches.error` with the full cause chain (`{e:#}`).

### Resolve a point

- **`{ timestamp }`:** `lsn_by_timestamp(tenant, source timeline, ts, with_lease)`.
  - `present` → use the returned LSN.
  - `future` → use the source's head (`get_timeline().last_record_lsn`).
  - `past`, with the returned LSN where the source's timeline starts on its ancestor's → the point is before the source's history:
    - a backup → "before the history of `<name>` begins (`<time>`)";
    - a child → "before `<name>`'s history begins (`<time>`); branch from `<parent>` instead";
    - `<time>` is the source's `history_start`, else `get_timestamp_of_lsn` there, else its `created_at`.
  - any other `past` → "outside the history window; the oldest available point is `<time>`", where the time is from `get_timestamp_of_lsn` on the returned LSN, or the source's `created_at` when there's no commit there (a tenant younger than its window).
  - `nodata` → fail with "no data at that time".
- **`{ lsn }`:** parse it, and refuse it if it's ahead of the source's `last_record_lsn`.
- **No `at`:** the source's head.
- The orchestrator validates the shapes, and the worker resolves the point, so the platform API never calls Neon.
- Resolution failures fail the branch with a 422-class message. The API returns 202 before resolution, so the result shows in the branch's `status`/`error`, and Studio shows it.

### Where a history begins

- `branches.history_start` is set with the branch's timeline: the commit time (`get_timestamp_of_lsn`) at the timeline's branch point on its source.
- Main (no branch point), or no commit there: `created_at`. After a reset or restore with no commit there: when the new timeline was made.
- A restore's backup takes over the old timeline, so it keeps the branch's old `history_start`; the branch keeps it too, because Neon copies the ancestor's layers into the restored timeline and points before the restore point stay reachable (checked live: a branch of a restored main, at a time before the restore point, has the old rows).
- Studio bounds its pickers by `max(now − window, history_start)`, and by the window alone when it's null.

### Create a branch

1. Claim: pending → provisioning.
2. Resolve the point against the parent's current timeline.
3. Record a new endpoint row (`ep-…`, compute token hash, the owner role with a new verifier, and the database name copied from main's `neon_databases`) and link it to the branch in one transaction. Record the new `timeline_id` on the branch.
4. `create_timeline(tenant, new, 17, Some((parent timeline, Some(lsn))))`.
5. `computes.create(ep, token)`, then wait for readiness (`admin_sql(ep, db, "SELECT 1")`).
6. Run the idempotent role bootstrap (step 4's `bootstrap_sql`). It only matters when the branch point predates the roles.
7. Re-check the row. If it's being deleted, tear down and `mark_deleted`. Otherwise seal the password (`Purpose::Owner`, AAD the branch id) and `set_ready`. If that returns false, tear down and `mark_deleted`.

The compute's spec carries the branch's verifier. The timeline's `pg_authid` still has the parent's, so `compute_ctl` runs `ALTER ROLE postgres … PASSWORD` at start. The branch gets its own password without extra work.

**Teardown:** the compute, then `delete_timeline` (404 counts as done), then the endpoint row. It stops at the first error, leaving the rest for reconcile. Delete races and teardown failures follow step 4's rules, which were Review Focus 2 and 3.

### Reset to parent (not main; no children; not a backup)

1. Claim: ready → resetting.
2. Resolve the parent's head.
3. Record the new id as `pending_timeline_id` on the branch.
4. Create the timeline from the parent at that LSN.
5. Stop the compute (`computes.delete(ep)`).
6. In one transaction: point `neon_endpoints.timeline_id` and `branches.timeline_id` at the new timeline, move the old id to `superseded_timeline_id`, and clear `pending_timeline_id`.
7. `computes.create(ep, fresh token)`, with the token hash replaced first, as reconcile does. Wait for readiness.
8. `delete_timeline(superseded)`, then clear `superseded_timeline_id`.
9. resetting → ready.
- A failure before step 6 deletes the pending timeline, clears `pending_timeline_id`, and returns the branch to ready on the old one.
- A failure after step 6 leaves the branch on the new timeline. Reconcile recreates the compute and deletes the superseded timeline.

### Restore (any `normal` branch, main included; no children)

1. Claim: ready → resetting.
2. Resolve the point against the branch's own timeline.
3. Record the new id as `pending_timeline_id`, then create the timeline from the branch's timeline at that LSN.
4. `detach_ancestor(new, multi_level_and_no_reparent)`. The restored timeline then no longer depends on the old one or on the branch's parents.
5. Insert the backup branch in one transaction: kind `backup`, `status ready`, no endpoint, the old `timeline_id`, `parent_branch_id` = the branch. In the same transaction, switch the branch's and its endpoint's `timeline_id` to the new one and clear `pending_timeline_id`.
6. Recreate the compute as in reset, and wait for readiness.
7. resetting → ready.
- The backup doesn't count toward the cap, and it can be deleted like any branch.
- A failure before step 5 deletes the pending timeline, clears `pending_timeline_id`, and returns the branch to ready.
- A detach whose outcome is unknown (a timeout, or the timeline can't be read) is never undone: its cause is recorded, the branch stays resetting, and the job is retried. Once `pending_timeline_at` is more than 20 minutes old (2 × the detach timeout), the branch fails with the cause, keeping its old timeline and the pending one. Reconcile reports it and leaves its endpoint alone.
- A failure after step 5 leaves the branch on the restored timeline. Reconcile recreates the compute.

### Delete a branch (not main; no `normal` children)

1. Claim the branch and its backups: → deleting.
2. Tear down each backup's timeline, then the branch's: the compute if any, then `delete_timeline` (also any pending or superseded timeline), then the endpoint row if any.
3. `mark_deleted` for the backups and the branch.
- A teardown error returns `Err`, so the job is retried and the row stays deleting.

### Delete a database

1. Mark the database and every live branch deleting.
2. Remove every branch's compute.
3. `delete_tenant`, which removes every timeline.
4. Delete the endpoint rows.
5. Mark everything deleted.

### History window

- `database.settings` → `patch_tenant_config(tenant, pitr_interval = history_window)`.
- Provision applies it when the tenant is created.
- Reconcile compares each tenant's effective `pitr_interval` (from `describe`/config) with the database's setting and patches any drift. That is how existing databases pick up the default after the migration.
- Shrinking the window lets Neon collect older history. The API docs and Studio say so.

### Passwords

- Reveal and reset per branch reuse step 4's code unchanged: metadata → push → publish → clear, under the per-endpoint lock.
- Restore and reset-to-parent keep the password: the recreated compute reads the same verifier from its spec.

### Reconcile, extended

It adds these rows to `plan`, with the same observe order: computes, tenants, databases and branches, then endpoints.

| Condition | Action |
|---|---|
| branch pending/provisioning, age > 600 s | fail it stale, then tear down |
| branch resetting, age > 600 s | delete any `pending_timeline_id` timeline, recreate its compute on its current timeline, delete any `superseded_timeline_id` timeline, then mark it ready |
| branch deleting, age > 60 s | retry the delete |
| ready `normal` branch with an endpoint but no compute | recreate the compute |
| endpoint whose branch is failed or deleted | tear down |
| timeline in a live tenant that no branch records | report only (never delete) |
| tenant `pitr_interval` ≠ database `history_window` | patch it |

## Studio

- **Branch tree.** A database row expands into its branch tree: main first, children indented, backups marked. Each branch shows its status, host (when it has a compute) and "from `<parent>` at `<time or LSN>`".
- **Per-branch actions:**
  - connection details and reset password (today's panel, per branch);
  - reset to parent;
  - restore, with a date-time picker limited to the history window and to the branch's `history_start` (the window alone when it's null);
  - delete.
- **Disabled actions** show their reason as a tooltip: main, has children, or backup.
- **New branch** form: name, parent (default main), and point (now, or a date-time within the window).
- **Settings** on the database: history window and branch cap, with the note about shrinking the window.
- **Status** comes from polling while any branch is pending, provisioning, resetting or deleting, as the database list does today.

## Error handling

- API refusals are listed above. Worker failures are stored on the branch with their cause chain, and Studio shows them.
- **Timeouts.**
  - Branch jobs use the same NATS consumer settings as provisioning (`ACK_WAIT` 300 s).
  - A restore or reset does at most:
    - two storage-controller calls;
    - a compute recreate;
    - a readiness wait of 60 s.
  - That fits well inside the ack wait.
- **Idempotency.** A job redelivered after a crash finds its recorded timeline or endpoint ids. It either finishes or tears down, and it never creates a second timeline for the same step.

## Testing

Strict TDD in every task, as in step 4.

- **`cheladb-neon`:** mock-server tests for `lsn_by_timestamp` (each `kind`, with `with_lease`), `detach_ancestor` and `patch_tenant_config`.
- **Metadata, live:**
  - the migration backfill against pre-migration-shaped rows: a main for each live database, none for deleted ones, the old columns gone;
  - the cap under two concurrent creates;
  - the conditional writes for each branch status;
  - the name and main uniqueness indexes.
- **Worker, with fakes:**
  - each flow's happy path and step order (journal);
  - a failure at each step, cleaning up and naming the step;
  - a delete landing during create, reset or restore;
  - the refusals for children, main and backups;
  - point resolution for each `kind`;
  - the reconcile rows above.
- **Orchestrator:**
  - every route and refusal;
  - the aliases for main;
  - the cap race through the API.
- **Worker live test**, against the real stack: the "done when" story above. Record the timings.
- **`just branch-e2e`:** the same story through the gateway API and the proxy (psql).
- **Studio:** unit tests for the tree, the actions and their disabled reasons, the forms and the settings; then a Playwright pass that creates a branch, connects, restores and deletes.

## Out of scope

- Scale to zero, and stopping a branch's compute by hand (step 7).
- Merging branches, and schema diffs.
- Restore or reset on a branch that has children.
- Branching across databases or projects.
- Syncing the data API's users table to every branch. Step 6 designs that; this step gives it the list of branches to walk.
- Per-branch compute sizes (step 7's resize).
