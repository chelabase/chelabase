# Protected branches: login-role guard and the review (design)

**Status:** approved by the user's decisions PB-Q1 to PB-Q7 and the approach (2026-10-14, roadmap "Resume here"); the user asked to build without a design walk or spec pause. S-rulings below are the coordinator's mechanical choices, listed for the user.

**Why:** TE-A's DDL guard left two gaps (TE-A plan, open points 1 and 2):
1. it covers member logins (`cb_m_*`) only: anyone logging in directly as another Postgres login role can still change the schema of a protected branch;
2. code planted while a branch was unprotected (a trigger, a rule, a SECURITY DEFINER function, a function called from a policy or a default…) runs during an Admin's session, which the guard never locks, so it can change the schema anyway.

This step closes (1) with a per-branch switch and (2) with a review an Admin must acknowledge to protect, plus fixes in place. No fork change, no image build. REASSIGN OWNED (R5) stays in the next fork round.

## 1. Per-branch settings (PB-Q2, PB-Q3)

- `branches.guard_logins boolean NOT NULL DEFAULT true`: the "guard login roles" switch.
- `branch_guard_exemptions (branch_id, role_name)`: login roles exempt from that switch on that branch, stored **by name** (a role dropped and re-created from SQL keeps its setting; a renamed role loses it, which fails closed). Any valid role name may be listed, existing or not.
- A new branch copies its parent's switch and exemptions (dormant until it is protected); a branch with no parent starts with the switch on and no exemptions. Nothing is migrated for existing branches (dummy data; the column default applies).
- The switch is **active** when the branch is protected and `guard_logins` is on. Only an Admin changes the switch or exemptions (the branch's own Access rule applies on top: a protected branch's settings are Admin-only anyway).

## 2. The guard in Postgres (approach "settings inside the guard")

On a branch where the switch is active, both event triggers (`chelabase_schema_guard_ddl`, `chelabase_schema_guard_drop`) refuse a schema change when `session_user`:
- is a LOGIN role (`rolcanlogin`), and
- isn't a superuser, the owner (`postgres`), a member login (`cb_m_*`, which keeps TE-A's marker rule), nor in the branch's exemptions.

So a role created from SQL is guarded from its first statement; there is no reconcile window. The member-login rule (marker `chelabase_schema_locked`) is unchanged. Temporary objects, `REFRESH MATERIALIZED VIEW` and `REINDEX` stay allowed for everyone, as today. The refusal is TE-A's: `42501`, hint `chelabase:branch_protected`.

**S1 (coordinator): the settings are rendered into the guard functions' bodies**, not a table: the functions aren't SECURITY DEFINER (on purpose: a definer function would run with the superuser's rights inside every guarded session), so they run as the session's role and couldn't read a superuser-only table. The bodies carry `guard_logins` (active or not) and the exemptions as literals; the stamp and the check (which already compares bodies) cover them, so a settings change makes the check answer `f` and the reconciler reinstalls. Cost if wrong: none to correctness; a settings change rewrites two functions per database.

**S2 (coordinator): an unprotected branch carries no guard.** Today a branch that was protected keeps its event triggers after unprotect (harmless while nobody holds the marker). With a login rule baked in, a child copied from a protected branch, or an unprotected branch, would refuse login roles. The reconciler therefore removes the guard (both event triggers, then schema `chelabase_guard` with what it holds) from every database of an unprotected branch where it finds it. Until that run, a child copied from a protected parent refuses login roles' DDL (fails closed). Cost if wrong: a short window after branch create where login roles' DDL is refused on the child.

**S3 (coordinator): the reconciler installs the guard on a protected branch whatever the switch**, with `guard_logins` rendered from the switch, so toggling only rewrites the bodies.

## 3. The review (PB-Q4 to PB-Q6)

A catalog scan, in every database of the branch, of code owned by a **non-Admin role** that can run in someone else's session. A role is Admin-level when it is a superuser, the owner, or a member (any chain) of the owner, `neon_superuser` or `cloud_admin` (TE-A/Members' owner-level rule); every other role is non-Admin. Objects that belong to an extension (`pg_depend.deptype = 'e'`) and objects in `pg_catalog`, `information_schema`, `pg_toast`, `chelabase_guard` and the platform's reserved schemas are skipped.

Listed items:
- **trigger** (not internal) whose function is owned by a non-Admin role, or that sits on a relation owned by one;
- **rule** (not a view's `_RETURN`) on a relation owned by a non-Admin role;
- **function** (any kind: function, procedure, aggregate's support function) owned by a non-Admin role when it is SECURITY DEFINER, or reachable implicitly: through `pg_depend` from a policy, a column default or generated expression, a CHECK constraint, an index expression or predicate, a view or materialized view, a trigger, a rule, or another listed function's SQL-standard body (`BEGIN ATOMIC`) or default. A PL/pgSQL or dynamic body can't be followed; such functions are listed by their reachability alone and flagged `opaque_body`.

Each item carries: `database`, `kind` (`trigger | rule | function`), `schema`, `name`, `identity` (a function's argument types; a trigger's or rule's relation), `owner`, `reasons` (`security_definer`, `trigger`, `rule`, `policy`, `default`, `generated`, `check`, `index`, `view`, `called_by`, each with the referencing object's name), `opaque_body`, and a stable `key` (`<database>/<kind>/<schema>.<name>[(<identity>)]`).

**Hash:** SHA-256 (hex) over the items sorted by key, each rendered as key, owner and sorted reasons; the empty list has a fixed hash. TE-A's dependents hash pattern.

**Protecting:** `protected: true` must carry `review_hash` when the list isn't empty; a missing or stale hash is refused with `409 review_changed` (the response carries the current list and hash). A successful protect stores the hash as acknowledged.

**Live panel:** on any branch, `GET …/review` returns the list, its hash, the acknowledged hash, and per item `new` (not part of the list last acknowledged; S4: the acknowledged keys are stored with the hash, so `new` is exact). `POST …/review/acknowledge {hash}` stores the current list as acknowledged (`409 review_changed` when stale). Studio badges a protected branch whose current hash differs from the acknowledged one.

**Fixes** (Admin, as the owner, one item per request, carrying the item's `key` and the list `hash`; stale → `409 review_changed`):
- `reassign`: the function (for a trigger, its function; for a rule, its relation) is given to the owner (`ALTER … OWNER TO`). Studio warns that the Admin then answers for the code, and that a SECURITY DEFINER function then runs as the owner.
- `drop`: `DROP TRIGGER`/`DROP RULE`/`DROP FUNCTION|PROCEDURE … RESTRICT`; a failure for dependents returns TE-A's typed error naming them (no cascade from here).
Rendered with TE-A's quoting; each fix returns the SQL it ran, and the response carries the new list and hash.

## 4. API

All under the existing branch routes; Admin for every write; reads follow the branch's read rule.
- `PATCH` branch: new optional `guard_logins: bool`, `review_hash: string` (with `protected: true`).
- `GET …/guard-exemptions`, `PUT …/guard-exemptions/{role}`, `DELETE …/guard-exemptions/{role}` (idempotent).
- The branch's role list carries `guard_exempt: bool` per login role; the branch carries `guard_logins` and `review_acknowledged_hash`.
- `GET …/review`, `POST …/review/acknowledge`, `POST …/review/fix`.
- New error codes: `review_changed`. Each settings write triggers the member-roles reconcile (synchronous, as protect does today).

## 5. Studio (PB-Q7)

In `branch-actions.tsx`:
- **Protect panel:** the review list (grouped by database, with reasons and the `new` flag), per-item Reassign/Drop with a confirm, the "guard login roles" switch, and Confirm protect (sends the hash; on `review_changed` it reloads the list and says so).
- **Protection panel** (protected branches, Admin): the switch, the live review with its fixes, and Acknowledge.
- **Roles panel:** per login role, an "Exempt from guard" toggle (Admin).
- **Branch row:** a badge "review" when the current hash differs from the acknowledged one.

## 6. Tests and live pass

Unit: guard SQL text and stamps (settings rendered and compared), reconciler planning (install with settings, removal on unprotected), metadata (copy on create, exemptions CRUD), scan parsing and hashing, the orchestrator handlers (hash gate, Admin checks), Studio components. Live: a login role refused on a protected branch with the switch on, allowed when exempt, when the switch is off, and on an unprotected child; a role created from SQL refused at once; the scan finding each reason kind in a seeded database; protect refused without the hash and with a stale one; reassign and drop fixes; acknowledge and `new`.
