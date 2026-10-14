# PB: protected branches, login-role guard and the review (implementation plan)

**Goal:** close TE-A's two guard gaps. (1) A per-branch "guard login roles" switch with per-branch exemptions by role name: on a protected branch with the switch on, every login role except the owner, member logins and the exempt is refused schema changes by the event triggers, from its first statement. (2) A review of code owned by non-Admin roles that can run in someone else's session, across every database of a branch: protecting needs its hash, items can be reassigned to the owner or dropped in place, and a live panel flags what's new since the last acknowledgement. API, generated client, Studio panels and docs.

**Spec:** `docs/superpowers/specs/2026-10-14-protected-branches-design.md` (user decisions PB-Q1 to PB-Q7 and the approach, 2026-10-14; the user asked to build without spec or plan pauses). Implementers read the spec section their task names.

**Architecture:** metadata (`crates/chelabase-metadata`: columns on `branches`, table `branch_guard_exemptions`); shared types (`crates/chelabase-types/src/protection.rs`); the scan, its hash and the fix SQL (`crates/chelabase-schema/src/review.rs`, pure); the guard with settings and its removal (`services/worker/src/schema_guard.rs`, `member_roles.rs`); the API (`services/orchestrator/src/protection.rs` plus the branch PATCH in `branches.rs` and `guard_exempt` in `roles.rs`); Studio (`apps/studio/src/components/branch-actions.tsx`, `branch-roles.tsx`, a new `branch-review.tsx`); docs (`apps/docs`).

**Tech:** Rust (existing crates only: `sha2`, `hex`, `serde_json`, `utoipa` are already workspace dependencies), React/TanStack Query with the orval-generated client. No new dependency. **No fork change: no image build; the live pass runs on the pinned images.**

**Standing rule:** the coordinator writes and commits each task's RED tests (below) before the brief, runs them RED and records the result in the Status. Implementers make the tests pass without editing, weakening, skipping or deleting them. Reviewers check that `git diff <red-commit>..HEAD -- <test files>` is empty for the coordinator's tests. When a RED commit needs a module to exist so that its tests compile, the coordinator adds only the empty module declaration.

## Status

- **RESUME HERE (2026-10-14): DONE, paused for the user's merge decision.** End-of-step fixes merged (`b5304dcc` via the fix branch: R9, R12–R17); `just check` green on the merged branch; live pass on this worktree's stack: `live_db` 189/189, `review_live` 2/2, `member_roles_live` 24/24, `roles_live` 42/42, orchestrator `api` live 26/26, `sql-editor-e2e` all passed. Browser pass blocked: Studio on this branch stack didn't react to input in "Create database" (form untouched by this step; even after `workbench restart studio`), so the new panels weren't seen in a browser (they're covered by 1130 vitest tests); queued with the post-merge `--stack` chore. The stack stays up until the merge, then is purged (one stack per plan). On merge: `git merge --no-ff`, push `main`, `just wt-remove protected-branches`, delete the branch on origin and locally.
- **End-of-step fix round (opus, `.worktrees/protected-branches-fix`):** `cc208c5c` (a one-character compile fix in the coordinator's `review_live.rs`), `1a3062af` (R9, R12–R16), `8ecf523c` (MDX); nextest 2575/2575, Studio 1130, `live_db` 189/189, `review_live` 2/2, `member_roles_live` 24/24 (incl. `no_guarded_session_escapes_the_guard`), `just check` green. Accepted deviations: two older worker tests follow R12/R13 (the sweep counts member-less tenants; exemptions in the guard test exist as roles); the compute-start hook is `recreate_compute_inner` (reset/restore already ask in `finish_ready`). Refused: a SQL comment naming `neon_superuser`/`cloud_admin` to satisfy an old needle (the coordinator's test now requires neither name). **R17 (coordinator):** `template1` is touched only in a run that installs or removes the guard in another database (a session there makes a concurrent `CREATE DATABASE` fail; only a superuser can change its guard). Follow-up RED `6b8363dc` (`template1_is_touched_only_when_another_database_changed` and the scan-names test FAIL); follow-up running.
- **End-of-step review (opus):** I1 the member-role sweep skips member-less tenants and a database made from SQL gets no reconcile, so a lost or failed reconcile leaves stale guard settings (fails open); I2 a guarded CREATEDB/CREATEROLE login escapes (CREATE DATABASE: templates carry no guard; CREATE ROLE under an exempt name nobody holds), and the review trusts `neon_superuser` members whom the guard doesn't; I3 the review hash ignores code bodies (a `CREATE OR REPLACE` keeps the hash); Minors: switch written then a failed protect enqueues nothing; the migration's `strpos(role_name, chr(1))` refuses U+0001, not NUL; the badge rescans on every window focus and hides a 503; docs wording (member logins "whatever the switch"; badge and toggle names). Second finding in the guard's area (after R11), so the round states the invariant and tests it exhaustively.
- **Invariant (end-of-step):** on a protected branch with the switch on, no session of a guarded role (any non-superuser other than the owner, an Admin's member login or an honored exemption) changes a permanent schema object in any database of the branch, including databases and roles it makes itself. Tested by `no_guarded_session_escapes_the_guard` (live, every known escape) plus `a_guarded_session_stays_guarded_after_its_role_loses_login`.
- **R12 (coordinator):** the sweep (`member_role_branches`) visits every ready normal branch of a live database, members or not; the DDL receiver asks for a member reconcile on any accepted delta that makes a database; the guard is also installed in `template1` (so `CREATE DATABASE` copies it). Residual: a database made `TEMPLATE template0` is unguarded until the reconcile its creation asks for (seconds); closing it needs the fork (refuse or guard at `CREATE DATABASE`), added to the next fork round with REASSIGN OWNED (R5). Also: if the worker has a hook where a branch's compute starts, it asks for a reconcile there (implementer reports). Cost if wrong: sweep load proportional to all branches (each converged run is one catalog read plus one check per database).
- **R13 (coordinator):** an exemption is honored only for a role that exists and that no guarded role administers (ADMIN on it, directly or through a chain); the reconciler renders only honored names (warning for the others). Keeps PB-Q2's by-name storage. Cost if wrong: an exemption silently not in effect (logged; a later Studio hint could show it).
- **R14 (coordinator):** the review trusts only superusers and the owner role itself (members of the owner or of `neon_superuser` are listed like anyone), so the review and the guard agree. Cost if wrong: longer lists.
- **R15 (coordinator):** each review item carries `digest` (md5 hex of `pg_get_functiondef`, `pg_get_triggerdef` or `pg_get_ruledef`; `prosrc` where those don't apply; `#[serde(default)]`), the hash line's fourth field. Cost if wrong: none.
- **R16 (coordinator):** a PATCH that wrote the switch enqueues the reconcile even if the protect then fails; the migration drops the `strpos` clause (text can't hold NUL; the API refuses it); the badge's review query uses a 5-minute stale time and no focus refetch, and shows "Review unavailable" on error; docs wording fixed; R9's check accepts a recipe that passes its arguments quoted as `"$@"` or `"$1"` (I2's intent: never re-split, never interpolated).
- **End-of-step fix-round RED (`.worktrees/protected-branches-fix`):** `review.rs` unit tests fail to compile (no `digest`); `ddl_role_changes_ask_for_a_member_reconcile` FAILS (now expects a reconcile for a new database); Studio `a_badge_review_that_cannot_load_says_so` FAILS; `workbench-recipes.sh` FAILS (R9); live (`#[ignore]`): `the_member_role_sweep_visits_branches_of_member_less_tenants`, `an_exemption_name_may_hold_any_character_but_nul`, `review_live` (f_boss listed, digests, a replaced body changes the hash), `no_guarded_session_escapes_the_guard`. No coordinator test for R16's enqueue-after-failed-protect (the fake can't fail `protect_branch`) or the badge's stale time.
- **Done: T5** (sonnet/sonnet): RED `641f58db` (8 vitest FAIL), implementation `a08cd958` (deviations accepted: badge from the review query's own hashes; Roles switches disabled for non-Admins; Reassign/Drop for every kind); review: 1 Important (`review_changed` from fix/acknowledge left the list stale), 3 Minor (no retry on a failed load; badge Admin-only → ruled no change; confirm focus); fix-round RED `bb546347` (4 FAIL), fix `04ebda6e` (verified by the coordinator: only `branch-actions.tsx`, `branch-review.tsx`); Studio 1129 tests, typecheck clean; merged.
- **Done: T6** (haiku, no review of its own): `bd7e3e8a` (`using/branches.mdx`, `architecture/security.mdx`, `using/login-roles.mdx`); docs build green; merged.
- **Done: T4** (opus/opus): RED `8b60ed6f`, implementation `67891a69`; review: 1 Important, 3 Minor (+ implementer deviation 1) → R8/R10; fix-round RED `31d9db5f`, fix `c9c29c36` (verified by the coordinator: test files untouched; worker `review.rs` lists `pg_database` live, re-checks the hash, fixes under the endpoint lock with `lock_timeout`); nextest 1323/1325 then the 2 OpenAPI tests fixed by `api-gen`; `member_roles_live` 23/23; merged into `protected-branches`.
- **Done: T3** (opus/opus): RED `38b82d90`, implementation `d8ac203f`; review: 1 Important (R11), 1 Minor (no change); fix-round RED `459ff5fb`, fix `664d7084` (verified by the coordinator: only `schema_guard.rs`, the rule now `session_user <> owner`, not `cb_m_`, not exempt); unit 608, `member_roles_live` 22/22; merged into `protected-branches`.
- **T4 fix-round interfaces (R8/R10):** `chelabase-nats` `SUBJECT_REVIEW = "chelabase.rpc.review"`, `ReviewRequest { branch_id, fix: Option<ReviewFixOrder { key, action, hash }> }`, `ReviewReply` (`status`-tagged: `Scanned{items}`, `Fixed{sql, items}`, `Changed{items}`, `NotFound{items}`, `FixFailed{sqlstate, message, items}`, `Failed{message}`); worker `review::REVIEW_DATABASES_MARK`, `Worker::review`; `JobPort::review`; mapping `Failed` 503 `unavailable`, `Changed` 409 `review_changed`, `NotFound` 404, `FixFailed` 2BP01 409 `has_dependents` / 55P03 409 `lock_timeout` / else 400 `schema_change_failed`. The scratch-compute test `protect_needs_the_review_hash_and_fixes_apply` is replaced by fake-harness tests and the worker live test. T4 review Minor 4 (`create_role` reads exemptions after creating) has no coordinator test (the fake can't fail that read).
- **T3:** implementation `d8ac203f`; unit 608 passed; `member_roles_live` 21/21, `roles_live` 42/42. Deviations (accepted): four older TE-A call sites in `member_roles/tests.rs` exclude the read-only presence check (`changing_scripts`); TE-A's text tests in `schema_guard.rs` adapted (listed in the report); the fake restore copies exemptions too.
- **T4:** implementation `67891a69`; 766 passed; all 6 RED fake-harness tests pass; the live test fails at the reassign fix (see R8). Deviations (accepted): `guard_exempt` added to one older test's field list; the scan opens the owner password itself (503 without a compute); `new` only after a first acknowledgement; Studio fixtures lack the new fields (T5).
- **R8 (coordinator, 2026-10-14):** review fixes run in the worker as `cloud_admin` (NATS RPC carrying branch, database, key and action only; the worker re-scans that database, finds the item by key, renders `fix_sql` with the branch owner, runs it with `lock_timeout`, and answers the SQL or a typed error). Why: Postgres lets the owner `ALTER … OWNER`/`DROP` only objects of roles it is a member of, so an owner-run fix fails on exactly the planted objects it exists for. Only items of the worker's own scan can be touched; no SQL crosses NATS. Cost if wrong: an Admin's fix acts with superuser rights on a listed item (that is the point).
- **T3 review (opus): 1 Important** (reproduced live: `rolcanlogin` asks who may log in now, so `ALTER ROLE x NOLOGIN` from another session un-guards x's open session), 1 Minor (exemptions over 63 bytes can't match: already refused by T4's API and the metadata CHECK; no change). Everything else checked live and holding (`cb_m_` names reserved by the fork's DDL handler, case, `SET ROLE` to the owner, hostile exemption names, CREATEROLE logins, the unprotected child). **R11 (coordinator):** drop `rolcanlogin`; a non-superuser `session_user` logged in by definition. Fix-round RED `459ff5fb` (the text test asserts no `rolcanlogin`: FAIL; live `a_guarded_session_stays_guarded_after_its_role_loses_login`).
- **T4 review (opus): 1 Important** (the scan uses the metadata's database list and skips 3D000, so a database renamed or created from SQL hides planted code until a resync), 3 Minor (owner refused CONNECT → bare 500; a PATCH writing the switch then failing to protect doesn't enqueue; `create_role` reads exemptions after creating the role). **R10 (coordinator), extending R8:** the whole review (scan and fixes) runs in the worker as `cloud_admin`: databases listed live from `pg_database` (not templates, `datallowconn`), each scanned, any failure fails the request; a fix carries the list hash, which the worker re-checks against its own scan before acting. Covers the Important and Minor 1. T4's fix round runs after T3 merges (it touches the worker).
- **R9 (coordinator, 2026-10-14):** `scripts/tests/workbench-recipes.sh` fails on `main` too ("zitadel-bootstrap passes its arguments with \"$@\""; the recipe now takes `admin_email=""`, W2 `931b61c2`); fixed in this step's end-of-step fixes so the gate passes. Cost if wrong: none.
- **Done: T2** (opus/opus): RED `724ddc7b`, implementation `8f5eb510`, coordinator test fix `a72c0a13`; review: 4 Important, 5 Minor (rulings R2–R7 below); fix-round RED `af9af073` (`ReviewReasonKind::Type` missing: both test files fail to compile), fix `173e1000` (verified by the coordinator: only `review.rs` and `protection.rs` changed; tests untouched); unit 332 passed, `review_live` 2/2; merged into `protected-branches`.
- **T4 RED `8b60ed6f`:** the 6 fake-harness tests FAIL (`update_branch_protected`, `guard_changes_reconcile_member_roles`, `a_new_branch_copies_its_parents_guard_switch` — T1 review finding 2 — `the_switch_and_exemptions_round_trip`, `guard_writes_need_admin_and_reads_need_developer`, `review_requests_validate_before_any_compute`); `protect_needs_the_review_hash_and_fixes_apply` is `#[ignore]` (scratch compute).
- **T3 RED `38b82d90`:** worker lib tests fail to compile, 23 errors (no `GuardSettings`, `schema_guard_present_sql`, `schema_unguard_sql`, the marks, `FakeState.guard_exemptions`; zero-argument call sites). Also `a_restore_backup_copies_guard_logins` (T1 review finding 1): written after T1's fix, so it passes from the start.
- **Done: T1** (sonnet/sonnet): RED `605b8582`, implementation `0846e928`, coordinator test fix `92d079b5` (the RED test made a child of a protected branch, which the parent-protection recheck refuses; the child is now made from the unprotected parent, which keeps its acknowledgement); review: approve, 4 Minor (fakes hardcoding `guard_logins: true` ×2, a snapshot comment, indentation); fix `17906f46` (verified by the coordinator; tests for the two fakes come with T3/T4, whose views show them); live `live_db` 187/187 (reviewer); merged into `protected-branches`.
- **P12 (coordinator, 2026-10-14):** clippy `-D warnings` is checked on the lines a task changes only; the workspace already has clippy 1.99 lints (`chelabase-routes/src/lib.rs:882`, `chelabase-metadata/src/members.rs:1365,2071`, `orchestrator/tests/common/fakes.rs:2503,2663,3076`) and `just check` doesn't run clippy. Cost if wrong: none (lints only).
- **T2 review (opus): 4 Important, 5 Minor**, rulings (coordinator, 2026-10-14, toward "close the gap fully"): **R2** follow operators (`pg_operator.oprcode`) wherever a function edge is followed; **R3** a domain's default is a root (`default`, via `schema.domain`); **R4** every SECURITY DEFINER function, any owner, is a starting point of the reach (`called_by`); Admin-owned definers stay unlisted; **R5** range types' functions are roots with a new reason `type` (via `schema.type`); trigger clones (`tgparentid <> 0`) skipped; **R6** a trigger whose function is Admin-owned but whose relation isn't is listed with `function: null` and the relation's owner, and its reassign is `ALTER TABLE {identity} OWNER TO`; **R7** keys are `{ident(database)}/{kind}/{ident(schema)}.{ident(name)}({identity})` and the hash line uses `ident(owner)` (so names can't collide or forge lines); finding 9 (superuser-only objects) no change. Cost if wrong: longer lists; key strings with quotes for unusual names.
- **T2:** implementation `8f5eb510` (opus). Deviation (correct): `CREATE DATABASE`/`DROP DATABASE` shared a simple query (implicit transaction, 25001); coordinator test fix `a72c0a13`. Implementer: unit 328 passed; the live test passed with the same split; `catalog_live` 12/12.
- **T1 RED `605b8582`:** `live_db` fails to compile, 27 errors (missing `set_branch_guard_logins`, `list_guard_exemptions`, `add_guard_exemption`, `remove_guard_exemption`, `protect_branch`, `acknowledge_review`; no `guard_logins`, `review_ack_hash`, `review_ack_keys` on `BranchRecord`).
- **T2 RED `724ddc7b`:** `review` and `review_live` fail to compile (unresolved `chelabase_schema::review`, `chelabase_types::protection`).
- **R1 (user, 2026-10-14):** build without a design walk, spec pause or plan pause (as W2); the pause before merging stays.
- Branch `protected-branches` (`.worktrees/protected-branches`), spec `90cdc10c`.

## Plan rulings (mechanical, listed for the user)

- **P1** The guard's settings are rendered into the two function bodies (spec S1). Exempt names go in as lowercase hex of their UTF-8 bytes and are compared with `encode(convert_to(session_user, 'UTF8'), 'hex')`, so no name can break out of the `$f$` body or the literal. The owner is the worker's `OWNER_ROLE`. Cost if wrong: none to safety; the bodies are harder to read in `psql`.
- **P2** The exemption list is rendered as stored even when the switch is off, so toggling flips one flag. Cost if wrong: none.
- **P3** Reads (`GET …/review`, `GET …/guard-exemptions`) need Developer on the project; every write (switch, exemptions, protect, acknowledge, fix) needs Admin (`Access::Project(Admin)`, like `UPDATE_BRANCH`). Cost if wrong: Developers see the planted-code list (they could read the catalogs anyway).
- **P4** Protecting runs the scan, so it needs a running compute. In `api.rs`'s fake harness (no compute) protect answers `503 unavailable` and changes nothing; the successful paths are tested on the scratch compute (`#[ignore]`, workbench stack). Unprotect, the switch and exemptions need no compute. Cost if wrong: protecting a suspended branch wakes its compute (as the SQL editor does).
- **P5** A PATCH needs at least one of `protected`, `guard_logins`; `review_hash` is only accepted with `protected: true`. Otherwise `422 invalid_request`. Cost if wrong: none.
- **P6** Item key: `<database>/<kind>/<schema>.<name>(<identity>)` for every kind. A function's identity is `pg_get_function_identity_arguments` (argument names included); a trigger's or rule's is its relation, `%I.%I`. Cost if wrong: keys change shape, so acknowledged lists from before a change show items as new once.
- **P7** Fixes run `ALTER ROUTINE`/`DROP ROUTINE` on a function's `function` text (`%I.%I(<identity args>)`, built from the catalog), `ALTER FUNCTION … OWNER TO` on a trigger's function, `ALTER TABLE <relation> OWNER TO` for a rule's relation (Postgres accepts it for views), and `DROP TRIGGER|RULE … ON <relation> RESTRICT`. Each runs as the owner in its own transaction with `lock_timeout = 5s`. Cost if wrong: a fix fails with a typed error instead of running.
- **P8** `review_changed` is 409. Its error body carries the current review as `error.review`, so a client can show the new list without another round trip.
- **P9** Operation ids: `getBranchReview`, `acknowledgeBranchReview`, `fixBranchReviewItem`, `listGuardExemptions`, `putGuardExemption`, `deleteGuardExemption`. Routes: `GET|POST /v1/databases/{id}/branches/{branch_id}/review[/acknowledge|/fix]`, `GET /v1/databases/{id}/branches/{branch_id}/guard-exemptions`, `PUT|DELETE …/guard-exemptions/{role}`.
- **P10** An exemption name is 1–63 bytes and has no NUL (the metadata CHECK enforces the length too); it need not exist as a role.
- **P11** An unprotected branch's reconcile runs a cheap read-only presence check in every database and removes the guard only where the check answers `t` (spec S2), so a normal reconcile runs no DDL.

## Task table (binding)

| Task | Scope | Implementer | Reviewer | Wave | Depends |
|---|---|---|---|---|---|
| T1 | Metadata: migration, `BranchRecord` fields, exemptions, protect-with-review, acknowledge, copy on create; update every `BranchRecord` literal | sonnet | sonnet | 1 | — |
| T2 | `chelabase-types::protection` types; `chelabase-schema::review` (scan SQL, parse, hash, fix SQL) | opus | opus | 1 | — |
| T3 | Worker: guard with settings, presence check and removal, reconciler wiring, `WorkerMetadata::guard_exemptions` | opus | opus | 2 | T1 |
| T4 | Orchestrator: PATCH fields, exemptions routes, review/acknowledge/fix routes, `guard_exempt` on roles, `review_changed`, routes table, OpenAPI, `just api-gen`, docs reference tables (errors, access) | opus | opus | 2 | T1, T2 |
| T5 | Studio: Protect panel review, Protection panel, badge, Roles exemption toggle | sonnet | sonnet | 3 | T4 |
| T6 | Docs: `using/branches.mdx` "Protected branches", `architecture/security.mdx`, `using/login-roles.mdx` note | haiku | none (the end-of-step review covers it) | 3 | T4 |

Waves run in parallel when their files are disjoint (pre-flight scan), each task in its own worktree (`just wt-add protected-branches-tN protected-branches`), with at most 3 builds or test suites at once.

## Shared interfaces (pinned)

**Metadata (T1)**, on the metadata client (the type that has `set_branch_protected`):

```rust
// BranchRecord gains (after `protected`):
pub guard_logins: bool,
pub review_ack_hash: Option<String>,
pub review_ack_keys: Vec<String>,

pub async fn set_branch_guard_logins(&self, id: BranchId, on: bool) -> Result<Option<BranchRecord>, MetadataError>;
pub async fn list_guard_exemptions(&self, id: BranchId) -> Result<Vec<String>, MetadataError>; // sorted (byte order)
pub async fn add_guard_exemption(&self, id: BranchId, role: &str) -> Result<bool, MetadataError>; // true: inserted
pub async fn remove_guard_exemption(&self, id: BranchId, role: &str) -> Result<bool, MetadataError>; // true: removed
/// protected = true and the acknowledged list, in one UPDATE (same row guards as set_branch_protected).
pub async fn protect_branch(&self, id: BranchId, hash: &str, keys: &[String]) -> Result<Option<BranchRecord>, MetadataError>;
pub async fn acknowledge_review(&self, id: BranchId, hash: &str, keys: &[String]) -> Result<Option<BranchRecord>, MetadataError>;
```

Migration `crates/chelabase-metadata/migrations/20261014000009_branch_guard.sql` (exact):

```sql
-- Protected branches: the login-role guard switch, its exemptions, the acknowledged review.
ALTER TABLE branches
    ADD COLUMN guard_logins    boolean NOT NULL DEFAULT true,
    ADD COLUMN review_ack_hash text,
    ADD COLUMN review_ack_keys text[] NOT NULL DEFAULT '{}';
CREATE TABLE branch_guard_exemptions (
    branch_id  uuid NOT NULL REFERENCES branches (id) ON DELETE CASCADE,
    role_name  text NOT NULL CHECK (octet_length(role_name) BETWEEN 1 AND 63 AND strpos(role_name, chr(1)) = 0),
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (branch_id, role_name)
);
```

`create_branch` copies the parent's `guard_logins` and exemptions in its transaction (not `protected`, not the acknowledgement). `create_rehearsal_branch` and the restore backup insert copy them the same way. A root branch takes the defaults.

**Types (T2)**, `crates/chelabase-types/src/protection.rs` (`pub mod protection;` in `lib.rs`), all `#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]` plus `#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]`, enums `#[serde(rename_all = "snake_case")]` with `Copy, PartialOrd, Ord, Hash` too:

```rust
pub const REVIEW_SCAN_MARK: &str = "-- chelabase: protection review";
pub enum ReviewItemKind { Function, Rule, Trigger }
pub enum ReviewReasonKind { SecurityDefiner, Trigger, Rule, Policy, Default, Generated, Check, Index, View, CalledBy }
pub struct ReviewReason { pub kind: ReviewReasonKind, pub via: Option<String> } // also PartialOrd, Ord (kind, then via)
pub struct ReviewItem {
    pub key: String, pub database: String, pub kind: ReviewItemKind,
    pub schema: String, pub name: String, pub identity: String,
    /// The function a fix targets, `%I.%I(<identity args>)`: the function itself, or a trigger's function; None for a rule.
    pub function: Option<String>,
    pub owner: String, pub reasons: Vec<ReviewReason>, pub opaque_body: bool,
    /// Not in the list last acknowledged (set by the orchestrator).
    #[serde(default)] pub new: bool,
}
pub struct Review { pub items: Vec<ReviewItem>, pub hash: String, pub acknowledged_hash: Option<String> }
pub enum FixAction { Reassign, Drop }
#[serde(deny_unknown_fields)] pub struct ReviewFixRequest { pub key: String, pub action: FixAction, pub hash: String }
pub struct ReviewFixResponse { pub sql: String, pub review: Review }
#[serde(deny_unknown_fields)] pub struct AcknowledgeReviewRequest { pub hash: String }
pub struct GuardExemptions { pub exemptions: Vec<String> }
```

**Scan (T2)**, `crates/chelabase-schema/src/review.rs` (`pub mod review;`):

```rust
/// One read-only SELECT (starts with REVIEW_SCAN_MARK, one row, one JSON text column: an array).
pub fn review_sql(owner: &str) -> String;
/// The scan's document for `database`: keys set, reasons sorted and deduplicated, items sorted by key, `new` false.
pub fn parse_review(database: &str, json: &str) -> Result<Vec<ReviewItem>, SchemaError>;
/// SHA-256, lowercase hex, of the items sorted by key, one line each: `key \t owner \t reasons`, reasons sorted,
/// each `kind` or `kind:via` (snake_case kind), joined by `,`; lines joined by `\n`. `new` and `opaque_body` aren't hashed.
pub fn review_hash(items: &[ReviewItem]) -> String;
pub fn fix_sql(item: &ReviewItem, action: FixAction, owner: &str) -> Result<String, SchemaError>;
```

The scan document: an array of `{"kind","schema","name","identity","function","owner","reasons":[{"kind","via"}],"opaque_body"}` (`function` and `via` may be null). `opaque_body` is `prosqlbody IS NULL` for a function item; false for triggers and rules. `via` names: policy `schema.table.policy`, default/generated `schema.table.column`, check `schema.table.constraint` (a domain's check: `schema.domain.constraint`), index `schema.index`, view `schema.view`, trigger `schema.table.trigger`, rule `schema.table.rule`, called_by the caller's `function` text; `security_definer` has no `via`. A role is Admin-level when it is a superuser, `owner`, or a member (any chain) of `owner`, `neon_superuser` or `cloud_admin` (each only when it exists). Skipped: extension members (`pg_depend.deptype = 'e'`), schemas `pg_catalog`, `information_schema`, `pg_toast`, `pg_temp_*`, `pg_toast_temp_*`, `chelabase_guard`. Reachability follows `pg_depend` from policies, attribute defaults, constraints, index relations, rewrite rules (a view's `_RETURN` counts as `view`, any other rule as `rule`), triggers, and transitively from reached functions (any owner) to the functions they reference; only non-Admin-owned functions are listed. A trigger is listed when its function or its relation is non-Admin-owned (owner shown: the function's owner when non-Admin, else the relation's); a rule (not `_RETURN`) when its relation is non-Admin-owned.

Fix SQL (P7), `owner` through `sql::ident`: function `ALTER ROUTINE {function} OWNER TO {owner};` / `DROP ROUTINE {function} RESTRICT;`; trigger `ALTER FUNCTION {function} OWNER TO {owner};` / `DROP TRIGGER {ident(name)} ON {identity} RESTRICT;`; rule `ALTER TABLE {identity} OWNER TO {owner};` / `DROP RULE {ident(name)} ON {identity} RESTRICT;`. A function or trigger item without `function` is `SchemaError::Parse`.

**Guard (T3)**, `services/worker/src/schema_guard.rs`:

```rust
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct GuardSettings { pub guard_logins: bool, pub exempt: BTreeSet<String> }
pub fn schema_guard_sql(settings: &GuardSettings) -> String;
pub fn schema_guard_check_sql(settings: &GuardSettings) -> String;
pub const GUARD_PRESENT_MARK: &str = "-- chelabase: schema guard present";
/// Read-only: `t` when either event trigger or schema `chelabase_guard` exists.
pub fn schema_guard_present_sql() -> String; // starts with GUARD_PRESENT_MARK
pub const UNGUARD_MARK: &str = "-- chelabase: schema unguard";
/// One transaction: both event triggers dropped, then `DROP SCHEMA IF EXISTS chelabase_guard CASCADE`.
pub fn schema_unguard_sql() -> String; // starts with UNGUARD_MARK
```

Both bodies carry one settings line, exactly `-- settings: guard_logins=<on|off> exempt=[<hex>, <hex>]` (hex per P1, in `exempt`'s order, `, `-separated, `[]` when empty). The stamp hashes the rendered script (settings included). Body order: superuser `session_user` returns; maintenance returns; then "locked" is the marker rule (TE-A, unchanged) **or**, when `guard_logins` is on, `session_user` isn't `OWNER_ROLE`, doesn't start `cb_m_`, and isn't exempt (no `rolcanlogin` test: R11); not locked returns; then TE-A's refusals unchanged. Functions stay SECURITY INVOKER with the pinned `search_path`, every catalog function and operator qualified.

`WorkerMetadata` gains `fn guard_exemptions(&self, branch: BranchId) -> impl Future<Output = Result<Vec<String>, MetadataError>> + Send;` and the fake `FakeState` gains `pub guard_exemptions: HashMap<BranchId, Vec<String>>` (missing key: none).

**API (T4):** `UpdateBranchRequest { protected: Option<bool>, guard_logins: Option<bool>, review_hash: Option<String> }` (deny unknown fields; P5). The `Branch` view gains `guard_logins: bool` and `review_acknowledged_hash: Option<String>`. `BranchRole` gains `guard_exempt: bool`. Routes and Access per P3/P9; `ErrorCode::ReviewChanged` (`review_changed`, 409). Each write that changes the switch or exemptions enqueues the member-roles reconcile (`enqueue_member_roles`), as protect does. Protect: scan → compare with `review_hash` (required when the list isn't empty; an empty list needs none) → `protect_branch(hash, keys)` → enqueue. Review GET: scan every branch database as the owner (`connect_as_owner`), `parse_review`, set `new` from `review_ack_keys`, hash. Acknowledge and fix compare the hash first; fix finds the item by key (missing: `404 not_found`), runs `fix_sql` as the owner (P7), and answers with the SQL and the new review.

## Pre-flight scan (2026-10-14)

- Wave 1: T1 touches `crates/chelabase-metadata/**`, every `BranchRecord { … }` literal (`services/worker/src/fakes.rs:505,578,1898,2883`, `services/worker/src/app_users.rs:1727`, `services/orchestrator/tests/common/fakes.rs:180,332,606`, `services/orchestrator/tests/internal.rs:3616`) and the worker fake's `create_branch` (`fakes.rs:546`, copy rule). T2 touches `crates/chelabase-types/src/{lib.rs,protection.rs}` and `crates/chelabase-schema/{src/lib.rs,src/review.rs,tests/review.rs,tests/review_live.rs}`. Disjoint.
- Wave 2: T3 is `services/worker/**` only; T4 is `services/orchestrator/**`, `crates/chelabase-routes`, `crates/chelabase-types/src/api_error.rs`, `crates/chelabase-nats` (none expected), `packages/management-client`, `openapi/`, `apps/docs/content/docs/reference/{errors,access}.mdx`. Disjoint.
- The guard runs today only `if record.protected` (`member_roles.rs:1095-1116`), and its functions return early unless the session holds the marker (`schema_guard.rs:119-125`). TE-A's text tests in `schema_guard.rs` (`guard_text`, `tests`, `stamp`) call the zero-argument functions and assert the old body order. **T3 may adapt those TE-A tests** to the new signatures and order (not this plan's RED tests), keeping each property they assert; the reviewer checks that.
- `an_unprotected_branch_runs_no_guard` (`member_roles/tests.rs:1791`) is rewritten in T3's RED commit (the presence check runs there now).
- `update_branch_protected` (`api.rs:3617`) and `protecting_a_branch_reconciles_its_member_roles` (`api.rs:~11652`) protect without a compute; T4's RED commit rewrites both for P4.
- `api.rs`'s `ScratchCompute` / `changes_app(password)` give a branch whose owner connection reaches a scratch database on the metadata Postgres; the review endpoint tests use it (`#[ignore]`).
- Studio tests run with `ALL_CAPS` (Admin) by default; `mk()` in `branch-actions.test.tsx` and `role()` in `branch-roles.test.tsx` list every field, so T5's RED commit adds the new fields there.
- `openapi.rs:1394` lists schema names; `crates/chelabase-routes` `ENDPOINTS`, `access_walk.rs`, `role_rules_agree.rs` and `docs_tables.rs` must learn every new route; `ErrorCode::ALL` grows to 96.

---

## T1: metadata (sonnet / sonnet)

**Files:** `crates/chelabase-metadata/migrations/20261014000009_branch_guard.sql`, `crates/chelabase-metadata/src/branches.rs` (+ `lib.rs` root insert untouched: defaults apply), every `BranchRecord` literal listed above, `services/worker/src/fakes.rs` `create_branch` (copy `guard_logins` from the parent record; exemptions are T3's), `crates/chelabase-metadata/tests/live_db.rs` (coordinator).

**Behaviour:** spec §1; Shared interfaces "Metadata". Same row guards as `set_branch_protected` for `set_branch_guard_logins`, `protect_branch`, `acknowledge_review` (`kind = 'normal'`, not deleting/deleted). Exemption calls on an unknown branch fail with the FK error (callers check the branch first).

**RED tests (coordinator)**, appended to `crates/chelabase-metadata/tests/live_db.rs`:

```rust
// --- Protected branches (PB, T1) -------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn guard_settings_default_on_and_children_copy_them() {
    let _sealed = sealed_lock().await;
    let client = client().await;
    let (id, _ep) = ready_database(&client).await;
    let main = main_of(id);
    let root = branch(&client, main).await;
    assert!(root.guard_logins, "a root branch starts with the switch on");
    assert_eq!(root.review_ack_hash, None);
    assert!(root.review_ack_keys.is_empty());
    assert!(client.list_guard_exemptions(main).await.unwrap().is_empty());
    // By name, idempotent, sorted.
    assert!(client.add_guard_exemption(main, "deploy").await.unwrap());
    assert!(client.add_guard_exemption(main, "Ci Bot").await.unwrap());
    assert!(!client.add_guard_exemption(main, "deploy").await.unwrap());
    assert_eq!(client.list_guard_exemptions(main).await.unwrap(), ["Ci Bot", "deploy"]);
    assert!(client.add_guard_exemption(main, "").await.is_err(), "empty");
    assert!(client.add_guard_exemption(main, &"a".repeat(64)).await.is_err(), "64 bytes");
    let off = client.set_branch_guard_logins(main, false).await.unwrap().expect("a normal branch");
    assert!(!off.guard_logins);
    // A child copies the switch and the exemptions; later changes stay on their branch.
    let dev = ready_branch(&client, id, "dev", main).await;
    assert!(!branch(&client, dev).await.guard_logins);
    assert_eq!(client.list_guard_exemptions(dev).await.unwrap(), ["Ci Bot", "deploy"]);
    assert!(client.remove_guard_exemption(main, "deploy").await.unwrap());
    assert!(!client.remove_guard_exemption(main, "deploy").await.unwrap());
    assert_eq!(client.list_guard_exemptions(main).await.unwrap(), ["Ci Bot"]);
    assert_eq!(client.list_guard_exemptions(dev).await.unwrap(), ["Ci Bot", "deploy"]);
    let on = client.set_branch_guard_logins(dev, true).await.unwrap().expect("a normal branch");
    assert!(on.guard_logins);
    assert!(!branch(&client, main).await.guard_logins);
    assert!(client.set_branch_guard_logins(BranchId::new(), true).await.unwrap().is_none());
    cleanup(&client, id).await;
}

#[tokio::test]
#[ignore]
async fn protect_with_a_review_stores_the_acknowledged_list() {
    let _sealed = sealed_lock().await;
    let client = client().await;
    let (id, _ep) = ready_database(&client).await;
    let dev = ready_branch(&client, id, "dev", main_of(id)).await;
    let keys = vec!["app/function/public.f()".to_string(), "app/rule/public.r(public.t)".to_string()];
    let protected = client.protect_branch(dev, "h1", &keys).await.unwrap().expect("a normal branch");
    assert!(protected.protected);
    assert_eq!(protected.review_ack_hash.as_deref(), Some("h1"));
    assert_eq!(protected.review_ack_keys, keys);
    let acked = client.acknowledge_review(dev, "h2", &[]).await.unwrap().expect("a normal branch");
    assert!(acked.protected, "acknowledging changes nothing else");
    assert_eq!(acked.review_ack_hash.as_deref(), Some("h2"));
    assert!(acked.review_ack_keys.is_empty());
    // A child copies neither the flag nor the acknowledgement (the parent keeps its
    // acknowledgement when unprotected; a child of a protected branch goes through the
    // parent-protection recheck, so it's made from the unprotected parent here).
    client.set_branch_protected(dev, false).await.unwrap().unwrap();
    assert_eq!(branch(&client, dev).await.review_ack_hash.as_deref(), Some("h2"));
    let child = ready_branch(&client, id, "dev-child", dev).await;
    let record = branch(&client, child).await;
    assert!(!record.protected);
    assert_eq!(record.review_ack_hash, None);
    assert!(record.review_ack_keys.is_empty());
    // Not once a delete has begun, nor for an unknown branch.
    let gone = ready_branch(&client, id, "gone", main_of(id)).await;
    client.begin_branch_delete(gone).await.unwrap();
    assert!(client.protect_branch(gone, "h", &[]).await.unwrap().is_none());
    assert!(client.acknowledge_review(gone, "h", &[]).await.unwrap().is_none());
    assert!(client.protect_branch(BranchId::new(), "h", &[]).await.unwrap().is_none());
    client.set_branch_protected(dev, false).await.unwrap().unwrap();
    cleanup(&client, id).await;
}
```

**Gotchas:** `BRANCH_COLUMNS` and `branch_from_row` both need the new columns. `ready_branch` goes through `create_branch`, so it proves the copy. The exemption copy is an `INSERT … SELECT` from the parent's rows in the same transaction as the branch insert.

**Verify:** `cargo nextest run --manifest-path <wt>/Cargo.toml --workspace --no-run`; `cargo clippy --manifest-path <wt>/Cargo.toml -p chelabase-metadata -p worker -p orchestrator --all-targets -- -D warnings`; live: `just --justfile <wt>/justfile --working-directory <wt> workbench test -p chelabase-metadata --test live_db -- --run-ignored only guard_settings_default_on_and_children_copy_them protect_with_a_review_stores_the_acknowledged_list` (plus the whole `live_db` file once, for regressions).

---

## T2: types and the review scan (opus / opus)

**Files:** `crates/chelabase-types/src/{lib.rs,protection.rs}`, `crates/chelabase-schema/src/{lib.rs,review.rs}`, `crates/chelabase-schema/tests/review.rs` (coordinator), `crates/chelabase-schema/tests/review_live.rs` (coordinator).

**Behaviour:** spec §3; Shared interfaces "Types" and "Scan". The SQL is one statement, read-only, every catalog function and operator schema-qualified (`OPERATOR(pg_catalog.=)` style, as `schema_guard.rs`), `owner` passed through `sql::literal`.

**RED tests (coordinator)**, `crates/chelabase-schema/tests/review.rs`:

```rust
//! The review's parsing, hash and fix SQL (PB, T2).

use chelabase_schema::review::{fix_sql, parse_review, review_hash, review_sql};
use chelabase_types::protection::*;

const DOC: &str = r#"[
  {"kind":"trigger","schema":"public","name":"tg","identity":"public.t","function":"public.tf()","owner":"dev","reasons":[{"kind":"trigger","via":"public.t.tg"}],"opaque_body":false},
  {"kind":"function","schema":"public","name":"f","identity":"a integer","function":"public.f(a integer)","owner":"dev",
   "reasons":[{"kind":"security_definer","via":null},{"kind":"policy","via":"public.t.p"},{"kind":"policy","via":"public.t.p"}],"opaque_body":true},
  {"kind":"rule","schema":"public","name":"r","identity":"public.t","function":null,"owner":"dev","reasons":[{"kind":"rule","via":"public.t.r"}],"opaque_body":false}
]"#;

fn items() -> Vec<ReviewItem> {
    parse_review("app", DOC).unwrap()
}

#[test]
fn parse_sets_keys_sorts_items_and_dedups_reasons() {
    let items = items();
    let keys: Vec<&str> = items.iter().map(|i| i.key.as_str()).collect();
    assert_eq!(keys, ["app/function/public.f(a integer)", "app/rule/public.r(public.t)", "app/trigger/public.tg(public.t)"]);
    let f = &items[0];
    assert_eq!((f.database.as_str(), f.kind, f.owner.as_str(), f.opaque_body, f.new), ("app", ReviewItemKind::Function, "dev", true, false));
    assert_eq!(f.function.as_deref(), Some("public.f(a integer)"));
    assert_eq!(
        f.reasons,
        [
            ReviewReason { kind: ReviewReasonKind::SecurityDefiner, via: None },
            ReviewReason { kind: ReviewReasonKind::Policy, via: Some("public.t.p".into()) },
        ]
    );
    assert_eq!(items[1].function, None);
}

#[test]
fn parse_refuses_what_it_does_not_know() {
    assert!(parse_review("app", "{}").is_err());
    assert!(parse_review("app", r#"[{"kind":"view","schema":"s","name":"n","identity":"","function":null,"owner":"o","reasons":[],"opaque_body":false}]"#).is_err());
    assert_eq!(parse_review("app", "[]").unwrap(), vec![]);
}

#[test]
fn the_hash_is_order_free_and_covers_keys_owners_and_reasons() {
    let base = items();
    let mut reversed = base.clone();
    reversed.reverse();
    assert_eq!(review_hash(&base), review_hash(&reversed));
    assert_eq!(review_hash(&base).len(), 64);
    let mut marked = base.clone();
    marked[0].new = true;
    marked[0].opaque_body = false;
    assert_eq!(review_hash(&base), review_hash(&marked), "new and opaque_body aren't hashed");
    let mut owner = base.clone();
    owner[0].owner = "other".into();
    assert_ne!(review_hash(&base), review_hash(&owner));
    let mut reasons = base.clone();
    reasons[0].reasons.pop();
    assert_ne!(review_hash(&base), review_hash(&reasons));
    assert_ne!(review_hash(&base), review_hash(&base[1..]));
    // The empty list: SHA-256 of "".
    assert_eq!(review_hash(&[]), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
    // One line per item: `key \t owner \t reasons`.
    let one = parse_review("db", r#"[{"kind":"function","schema":"s","name":"n","identity":"","function":"s.n()","owner":"o","reasons":[{"kind":"view","via":"s.v"},{"kind":"security_definer","via":null}],"opaque_body":false}]"#).unwrap();
    use sha2::{Digest, Sha256};
    let expected = hex::encode(Sha256::digest(b"db/function/s.n()\to\tsecurity_definer,view:s.v"));
    assert_eq!(review_hash(&one), expected);
}

#[test]
fn fixes_render_per_kind_with_a_quoted_owner() {
    let items = items();
    let (f, r, t) = (&items[0], &items[1], &items[2]);
    assert_eq!(fix_sql(f, FixAction::Reassign, "postgres").unwrap(), "ALTER ROUTINE public.f(a integer) OWNER TO postgres;");
    assert_eq!(fix_sql(f, FixAction::Drop, "postgres").unwrap(), "DROP ROUTINE public.f(a integer) RESTRICT;");
    assert_eq!(fix_sql(t, FixAction::Reassign, "Own Er").unwrap(), "ALTER FUNCTION public.tf() OWNER TO \"Own Er\";");
    assert_eq!(fix_sql(t, FixAction::Drop, "postgres").unwrap(), "DROP TRIGGER tg ON public.t RESTRICT;");
    assert_eq!(fix_sql(r, FixAction::Reassign, "postgres").unwrap(), "ALTER TABLE public.t OWNER TO postgres;");
    assert_eq!(fix_sql(r, FixAction::Drop, "postgres").unwrap(), "DROP RULE r ON public.t RESTRICT;");
    let mut broken = f.clone();
    broken.function = None;
    assert!(fix_sql(&broken, FixAction::Drop, "postgres").is_err());
}

#[test]
fn the_scan_is_one_read_only_select_for_the_owner() {
    let sql = review_sql("own'er");
    assert!(sql.starts_with(REVIEW_SCAN_MARK), "{sql}");
    assert!(sql.contains("'own''er'"), "the owner as a literal: {sql}");
    for word in ["INSERT ", "UPDATE ", "DELETE ", "ALTER ", "DROP ", "CREATE ", "GRANT ", "SET "] {
        assert!(!sql.to_uppercase().contains(word), "{word}: {sql}");
    }
    for needle in ["pg_catalog.pg_depend", "prosecdef", "neon_superuser", "cloud_admin", "chelabase_guard"] {
        assert!(sql.contains(needle), "{needle}");
    }
}
```

(`sha2` and `hex` become dev-dependencies of `chelabase-schema` if they aren't dependencies already; the implementer adds them, workspace versions.)

`crates/chelabase-schema/tests/review_live.rs`:

```rust
//! The review scan on a scratch database of the metadata Postgres (PB, T2). Names: `rv_<12 hex>`.

use chelabase_schema::review::{fix_sql, parse_review, review_hash, review_sql};
use chelabase_types::protection::*;
use tokio_postgres::{Client, NoTls, SimpleQueryMessage};

fn url() -> String {
    std::env::var("CHELABASE_METADATA_DATABASE_URL").unwrap_or_else(|_| "postgres://chelabase:chelabase@metadata-db:5432/chelabase_metadata".into())
}

fn url_for(name: &str) -> String {
    let base = url();
    let (head, tail) = base.rsplit_once('/').expect("a database in the URL");
    let query = tail.split_once('?').map(|(_, q)| format!("?{q}")).unwrap_or_default();
    format!("{head}/{name}{query}")
}

async fn connect(url: &str) -> Client {
    let (client, connection) = tokio_postgres::connect(url, NoTls).await.expect("metadata Postgres (is the workbench stack up?)");
    tokio::spawn(connection);
    client
}

async fn one(client: &Client, sql: &str) -> String {
    let rows: Vec<String> = client
        .simple_query(sql)
        .await
        .unwrap()
        .into_iter()
        .filter_map(|m| match m {
            SimpleQueryMessage::Row(row) => Some(row.get(0).unwrap_or_default().to_string()),
            _ => None,
        })
        .collect();
    assert_eq!(rows.len(), 1, "one row: {rows:?}");
    rows.into_iter().next().unwrap()
}

async fn scan(client: &Client, owner: &str) -> Vec<ReviewItem> {
    parse_review("app", &one(client, &review_sql(owner)).await).unwrap()
}

#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn the_scan_finds_every_implicit_call_site_and_fixes_apply() {
    let meta = connect(&url()).await;
    let mut random = [0u8; 6];
    getrandom::fill(&mut random).unwrap();
    let name = format!("rv_{}", random.iter().map(|b| format!("{b:02x}")).collect::<String>());
    let (owner, dev, boss) = (format!("{name}_owner"), format!("{name}_dev"), format!("{name}_boss"));
    meta.batch_execute(&format!("CREATE ROLE {owner} LOGIN; CREATE ROLE {dev}; CREATE ROLE {boss} IN ROLE {owner};")).await.unwrap();
    // CREATE DATABASE can't share a simple query with other statements (implicit transaction).
    meta.batch_execute(&format!("CREATE DATABASE {name} OWNER {owner}")).await.unwrap();
    let db = connect(&url_for(&name)).await;
    db.batch_execute(&format!(
        "GRANT CREATE ON SCHEMA public TO {dev}, {boss};
         SET ROLE {dev};
         CREATE FUNCTION public.f_default() RETURNS int LANGUAGE sql AS 'SELECT 1';
         CREATE FUNCTION public.f_gen(int) RETURNS int LANGUAGE sql IMMUTABLE AS 'SELECT $1';
         CREATE FUNCTION public.f_check(int) RETURNS boolean LANGUAGE sql IMMUTABLE AS 'SELECT true';
         CREATE FUNCTION public.f_index(int) RETURNS int LANGUAGE sql IMMUTABLE AS 'SELECT $1';
         CREATE FUNCTION public.f_policy() RETURNS boolean LANGUAGE sql STABLE AS 'SELECT true';
         CREATE FUNCTION public.f_view() RETURNS int LANGUAGE sql STABLE AS 'SELECT 1';
         CREATE FUNCTION public.f_inner() RETURNS int LANGUAGE sql STABLE RETURN 1;
         CREATE FUNCTION public.f_outer() RETURNS int LANGUAGE sql STABLE BEGIN ATOMIC SELECT public.f_inner(); END;
         CREATE FUNCTION public.f_definer() RETURNS int LANGUAGE sql SECURITY DEFINER AS 'SELECT 1';
         CREATE FUNCTION public.f_trigger() RETURNS trigger LANGUAGE plpgsql AS 'BEGIN RETURN NEW; END';
         CREATE FUNCTION public.f_unused() RETURNS int LANGUAGE sql AS 'SELECT 1';
         CREATE TABLE public.t (id int, c int DEFAULT public.f_default() CONSTRAINT t_c_check CHECK (public.f_check(c)), g int GENERATED ALWAYS AS (public.f_gen(id)) STORED);
         CREATE INDEX t_idx ON public.t (public.f_index(id));
         ALTER TABLE public.t ENABLE ROW LEVEL SECURITY;
         CREATE POLICY p ON public.t USING (public.f_policy());
         CREATE VIEW public.v AS SELECT public.f_view() AS x, public.f_outer() AS y;
         CREATE TRIGGER tg BEFORE INSERT ON public.t FOR EACH ROW EXECUTE FUNCTION public.f_trigger();
         CREATE RULE r AS ON UPDATE TO public.t DO ALSO NOTIFY t;
         RESET ROLE;
         SET ROLE {boss};
         CREATE FUNCTION public.f_boss() RETURNS int LANGUAGE sql SECURITY DEFINER AS 'SELECT 1';
         RESET ROLE;"
    ))
    .await
    .unwrap();

    let items = scan(&db, &owner).await;
    let keys: Vec<&str> = items.iter().map(|i| i.key.as_str()).collect();
    assert_eq!(
        keys,
        [
            "app/function/public.f_check(integer)",
            "app/function/public.f_default()",
            "app/function/public.f_definer()",
            "app/function/public.f_gen(integer)",
            "app/function/public.f_index(integer)",
            "app/function/public.f_inner()",
            "app/function/public.f_outer()",
            "app/function/public.f_policy()",
            "app/function/public.f_trigger()",
            "app/function/public.f_view()",
            "app/rule/public.r(public.t)",
            "app/trigger/public.tg(public.t)",
        ],
        "not f_unused (unreachable), not f_boss (Admin-level owner)"
    );
    assert!(items.iter().all(|i| i.owner == dev), "{items:#?}");
    let reasons = |key: &str| -> Vec<(ReviewReasonKind, Option<String>)> {
        items.iter().find(|i| i.key == key).unwrap().reasons.iter().map(|r| (r.kind, r.via.clone())).collect()
    };
    use ReviewReasonKind::*;
    let s = |v: &str| Some(v.to_string());
    assert_eq!(reasons("app/function/public.f_check(integer)"), [(Check, s("public.t.t_c_check"))]);
    assert_eq!(reasons("app/function/public.f_default()"), [(Default, s("public.t.c"))]);
    assert_eq!(reasons("app/function/public.f_definer()"), [(SecurityDefiner, None)]);
    assert_eq!(reasons("app/function/public.f_gen(integer)"), [(Generated, s("public.t.g"))]);
    assert_eq!(reasons("app/function/public.f_index(integer)"), [(Index, s("public.t_idx"))]);
    assert_eq!(reasons("app/function/public.f_inner()"), [(CalledBy, s("public.f_outer()"))]);
    assert_eq!(reasons("app/function/public.f_outer()"), [(View, s("public.v"))]);
    assert_eq!(reasons("app/function/public.f_policy()"), [(Policy, s("public.t.p"))]);
    assert_eq!(reasons("app/function/public.f_trigger()"), [(Trigger, s("public.t.tg"))]);
    assert_eq!(reasons("app/function/public.f_view()"), [(View, s("public.v"))]);
    assert_eq!(reasons("app/rule/public.r(public.t)"), [(Rule, s("public.t.r"))]);
    assert_eq!(reasons("app/trigger/public.tg(public.t)"), [(Trigger, s("public.t.tg"))]);
    let item = |key: &str| items.iter().find(|i| i.key == key).unwrap().clone();
    assert!(item("app/function/public.f_definer()").opaque_body);
    assert!(!item("app/function/public.f_inner()").opaque_body, "a RETURN body is followed");
    assert_eq!(item("app/trigger/public.tg(public.t)").function.as_deref(), Some("public.f_trigger()"));

    // Fixes, run as the superuser here: reassigning the definer and dropping the trigger take
    // them (and the trigger's function, now unreachable) off the list; the hash moves.
    let before = review_hash(&items);
    db.batch_execute(&fix_sql(&item("app/function/public.f_definer()"), FixAction::Reassign, &owner).unwrap()).await.unwrap();
    db.batch_execute(&fix_sql(&item("app/trigger/public.tg(public.t)"), FixAction::Drop, &owner).unwrap()).await.unwrap();
    let after = scan(&db, &owner).await;
    let keys: Vec<&str> = after.iter().map(|i| i.key.as_str()).collect();
    assert!(!keys.contains(&"app/function/public.f_definer()"));
    assert!(!keys.contains(&"app/trigger/public.tg(public.t)"));
    assert!(!keys.contains(&"app/function/public.f_trigger()"));
    assert_eq!(keys.len(), 9);
    assert_ne!(review_hash(&after), before);
    // A drop that has dependents fails as Postgres says (RESTRICT).
    let err = db.batch_execute(&fix_sql(&item("app/function/public.f_policy()"), FixAction::Drop, &owner).unwrap()).await.unwrap_err();
    assert_eq!(err.as_db_error().map(|e| e.code().code()), Some("2BP01"), "{err}");

    drop(db);
    meta.batch_execute(&format!("DROP DATABASE {name} WITH (FORCE)")).await.unwrap();
    meta.batch_execute(&format!("DROP ROLE {boss}; DROP ROLE {dev}; DROP ROLE {owner};")).await.unwrap();
}
```

**Gotchas:** a view's dependency on a function goes through its `_RETURN` rule (`pg_rewrite`), not the view's `pg_class` row; an index expression's through the index's `pg_class` row; a default's and a generated column's through `pg_attrdef` (tell them apart by `attgenerated`). `pg_get_function_identity_arguments` includes argument names. The CHECK constraint's dependency is `pg_constraint` (`contype = 'c'`), a domain's too. Keep the recursion bounded by a `UNION` (not `UNION ALL`) on function oids.

**Verify:** `cargo nextest run --manifest-path <wt>/Cargo.toml -p chelabase-schema -p chelabase-types`; clippy on both crates with `--all-targets -- -D warnings`; live: `… workbench test -p chelabase-schema --test review_live -- --run-ignored only`.

---

## T3: worker guard with settings (opus / opus)

**Files:** `services/worker/src/schema_guard.rs`, `services/worker/src/member_roles.rs`, `services/worker/src/ports.rs`, the real port impl (where `member_branch_roles` is implemented), `services/worker/src/fakes.rs` (`FakeState.guard_exemptions`, the port method, copying exemptions in the fake `create_branch`), `services/worker/src/member_roles/tests.rs` (coordinator section + the rewritten `an_unprotected_branch_runs_no_guard`), `services/worker/tests/member_roles_live.rs` (coordinator).

**Behaviour:** spec §2, S2, S3; Shared interfaces "Guard"; P1, P2, P11. Protected: settings from `record.guard_logins` and `guard_exemptions(branch)`, check then install as today. Unprotected: the presence check in every database, `schema_unguard_sql` where it answers `t`; a failed removal fails the run like a failed install (`failures`), roles still applied.

**RED tests (coordinator)**, a new module at the end of `services/worker/src/schema_guard.rs`:

```rust
// PB, T3: the branch's settings in the bodies.
#[cfg(test)]
mod settings_text {
    use super::*;

    fn s(on: bool, exempt: &[&str]) -> GuardSettings {
        GuardSettings { guard_logins: on, exempt: exempt.iter().map(|e| e.to_string()).collect() }
    }

    #[test]
    fn the_settings_line_is_in_both_bodies() {
        let sql = schema_guard_sql(&s(true, &["deploy", "Ci Bot"]));
        // "Ci Bot" and "deploy", hex of their UTF-8 bytes, in the set's order.
        assert_eq!(sql.matches("-- settings: guard_logins=on exempt=[436920426f74, 6465706c6f79]").count(), 2, "{sql}");
        let off = schema_guard_sql(&s(false, &[]));
        assert_eq!(off.matches("-- settings: guard_logins=off exempt=[]").count(), 2, "{off}");
    }

    #[test]
    fn a_settings_change_changes_the_check_and_the_stamp() {
        let (a, b, c) = (s(true, &["deploy"]), s(true, &[]), s(false, &["deploy"]));
        assert_ne!(schema_guard_check_sql(&a), schema_guard_check_sql(&b));
        assert_ne!(schema_guard_check_sql(&a), schema_guard_check_sql(&c));
        assert_eq!(schema_guard_check_sql(&a), schema_guard_check_sql(&s(true, &["deploy"])));
        let stamp = |x: &GuardSettings| {
            let sql = schema_guard_sql(x);
            let at = sql.find("IS 'chelabase schema guard ").unwrap();
            sql[at..at + 44].to_string()
        };
        assert_ne!(stamp(&a), stamp(&b));
        assert_ne!(stamp(&a), stamp(&c));
    }

    #[test]
    fn the_login_rule_reads_the_catalog_and_stays_invoker() {
        let sql = schema_guard_sql(&s(true, &["deploy"]));
        for part in ["rolcanlogin", "session_user", "'cb_m_'", "rolsuper", "pg_catalog.encode(pg_catalog.convert_to(session_user, 'UTF8'), 'hex')"] {
            assert!(sql.contains(part), "{part}: {sql}");
        }
        assert!(!sql.contains("SECURITY DEFINER"));
    }

    #[test]
    fn hostile_names_never_reach_the_text() {
        let name = "x$f$y'; DROP SCHEMA public; --";
        let sql = schema_guard_sql(&s(true, &[name]));
        assert!(!sql.contains("x$f$y") && !sql.contains("DROP SCHEMA public"), "{sql}");
        assert!(sql.contains(&hex::encode(name.as_bytes())), "{sql}");
    }

    #[test]
    fn presence_and_removal_scripts() {
        let present = schema_guard_present_sql();
        assert!(present.starts_with(GUARD_PRESENT_MARK), "{present}");
        for word in ["DROP", "CREATE", "ALTER", "COMMENT"] {
            assert!(!present.contains(word), "{word}: {present}");
        }
        let unguard = schema_unguard_sql();
        assert!(unguard.starts_with(UNGUARD_MARK), "{unguard}");
        let ddl = unguard.find("DROP EVENT TRIGGER IF EXISTS chelabase_schema_guard_ddl;").unwrap();
        let drop = unguard.find("DROP EVENT TRIGGER IF EXISTS chelabase_schema_guard_drop;").unwrap();
        let schema = unguard.find("DROP SCHEMA IF EXISTS chelabase_guard CASCADE;").unwrap();
        assert!(ddl < schema && drop < schema, "{unguard}");
        assert!(unguard.contains("BEGIN;") && unguard.trim_end().ends_with("COMMIT;"), "{unguard}");
    }
}
```

(`hex` becomes a worker dependency if it isn't one; workspace version.)

`services/worker/src/member_roles/tests.rs`: replace `an_unprotected_branch_runs_no_guard` with:

```rust
#[tokio::test]
async fn an_unprotected_branch_runs_no_guard() {
    let (w, _journal) = worker();
    let (branch, ep, _member) = setup(&w, &["billing"]).await;
    w.computes.answer_at(&ep, "chelabase: member roles", &cat(&["!postgres", "billing"]));
    assert!(done(&w.reconcile_member_roles(branch, None).await));
    // Only the read-only presence check (PB T3), which answers nothing here: no install, no removal.
    let all = all_scripts(&w);
    assert!(all.iter().filter(|(_, s)| !s.starts_with(crate::schema_guard::GUARD_PRESENT_MARK)).all(|(_, s)| !s.contains("chelabase_schema_guard") && !s.contains("chelabase_schema_locked")), "{all:?}");
}
```

and append:

```rust
// --- Protected branches: the login-role guard (PB, T3) ---------------------------------------

use crate::schema_guard::{
    GUARD_PRESENT_MARK, GuardSettings, UNGUARD_MARK, schema_unguard_sql,
};

fn guard_settings(on: bool, exempt: &[&str]) -> GuardSettings {
    GuardSettings { guard_logins: on, exempt: exempt.iter().map(|e| e.to_string()).collect() }
}

#[tokio::test]
async fn a_protected_branch_guards_with_its_settings() {
    for on in [true, false] {
        let (w, _journal) = worker();
        let (branch, ep, _member) = setup(&w, &["billing"]).await;
        {
            let mut state = w.metadata.state();
            let record = state.branches.get_mut(&branch).unwrap();
            record.protected = true;
            record.guard_logins = on;
            state.guard_exemptions.insert(branch, vec!["deploy".into(), "Ci Bot".into()]);
        }
        let answer = Cat { roles: &["!postgres", "billing"], memberships: &[], edges: &[], passwords: &[], databases: &[("app", "postgres")] }.answer();
        w.computes.answer_at(&ep, "chelabase: member roles", &answer);
        assert!(done(&w.reconcile_member_roles(branch, None).await));
        let settings = guard_settings(on, &["Ci Bot", "deploy"]);
        let all = all_scripts(&w);
        assert!(all.iter().any(|(db, s)| db == "app" && *s == schema_guard_check_sql(&settings)), "{on}: {all:?}");
        assert!(all.iter().any(|(db, s)| db == "app" && *s == schema_guard_sql(&settings)), "{on}: {all:?}");
        assert!(!all.iter().any(|(_, s)| s.starts_with(GUARD_PRESENT_MARK) || s.starts_with(UNGUARD_MARK)));
    }
}

#[tokio::test]
async fn an_unprotected_branch_removes_a_guard_where_it_finds_one() {
    let (w, _journal) = worker();
    let (branch, ep, _member) = setup(&w, &["billing"]).await;
    let answer = Cat { roles: &["!postgres", "billing"], memberships: &[], edges: &[], passwords: &[], databases: &[("app", "postgres"), ("analytics", "postgres")] }.answer();
    w.computes.answer_at(&ep, "chelabase: member roles", &answer);
    w.computes.answer_in("app", GUARD_PRESENT_MARK, "t");
    w.computes.answer_in("analytics", GUARD_PRESENT_MARK, "f");
    assert!(done(&w.reconcile_member_roles(branch, None).await));
    let all = all_scripts(&w);
    let checked: Vec<&str> = all.iter().filter(|(_, s)| s.starts_with(GUARD_PRESENT_MARK)).map(|(db, _)| db.as_str()).collect();
    assert_eq!(checked, ["analytics", "app"]);
    let removed: Vec<&str> = all.iter().filter(|(_, s)| *s == schema_unguard_sql()).map(|(db, _)| db.as_str()).collect();
    assert_eq!(removed, ["app"]);
    assert!(!all.iter().any(|(_, s)| s.contains("CREATE EVENT TRIGGER")));
}

#[tokio::test]
async fn a_failed_removal_fails_the_run_but_the_roles_still_apply() {
    let (w, _journal) = worker();
    let (branch, ep, member) = setup(&w, &["billing"]).await;
    w.computes.answer_at(&ep, "chelabase: member roles", &cat(&["!postgres", "billing"]));
    w.computes.answer(GUARD_PRESENT_MARK, "t");
    w.computes.fail_when(UNGUARD_MARK);
    let reply = w.reconcile_member_roles(branch, None).await;
    assert!(matches!(&reply, MemberRolesReply::Failed { message } if message.contains("unguarding")), "{reply:?}");
    let role = member_role_name(member);
    assert!(texts(&w).iter().any(|s| s.contains(&format!("CREATE ROLE {role}"))), "{:?}", texts(&w));
}
```

(The failure message for a removal starts `unguarding <db>: `, mirroring `guarding <db>: `.)

`services/worker/tests/member_roles_live.rs`, appended:

```rust
// PB, T3: login roles on a protected branch, the switch, exemptions, a role made later from
// SQL, and an unprotected child copied from a protected parent.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the workbench stack"]
async fn a_protected_branch_guards_login_roles_unless_exempt() {
    live(|fx| async move {
        fx.sql_on(&fx.ep, &fx.database, "CREATE ROLE pb_ci LOGIN PASSWORD 'pb-ci-pass'; CREATE ROLE pb_deploy LOGIN PASSWORD 'pb-deploy-pass'; GRANT CREATE ON SCHEMA public TO pb_ci, pb_deploy;").await;
        let refused = |out: &Result<String, String>| out.as_ref().is_err_and(|e| e.contains("42501") && e.contains("branch is protected") && e.contains("chelabase:branch_protected"));
        // Unprotected: login roles change the schema freely.
        fx.direct(&fx.ep, "pb_ci", "pb-ci-pass", "CREATE TABLE public.pb_t1 (id int);").await.unwrap();
        // Protected, switch on (the default), pb_deploy exempt.
        fx.metadata.add_guard_exemption(fx.main, "pb_deploy").await.unwrap();
        fx.metadata.set_branch_protected(fx.main, true).await.unwrap();
        fx.reconcile(fx.main).await;
        for ddl in ["CREATE TABLE public.pb_t2 (id int);", "DROP TABLE public.pb_t1;", "ALTER TABLE public.pb_t1 ADD COLUMN x int;"] {
            let out = fx.direct(&fx.ep, "pb_ci", "pb-ci-pass", ddl).await;
            assert!(refused(&out), "{ddl}: {out:?}");
        }
        fx.direct(&fx.ep, "pb_ci", "pb-ci-pass", "CREATE TEMP TABLE pb_scratch (id int); DROP TABLE pb_scratch;").await.unwrap();
        fx.direct(&fx.ep, "pb_deploy", "pb-deploy-pass", "CREATE TABLE public.pb_t3 (id int);").await.unwrap();
        // A login role made from SQL afterwards is guarded at once, with no reconcile.
        fx.sql_on(&fx.ep, &fx.database, "CREATE ROLE pb_late LOGIN PASSWORD 'pb-late-pass'; GRANT CREATE ON SCHEMA public TO pb_late;").await;
        let out = fx.direct(&fx.ep, "pb_late", "pb-late-pass", "CREATE TABLE public.pb_t4 (id int);").await;
        assert!(refused(&out), "{out:?}");
        // Switch off: only member logins stay guarded.
        fx.metadata.set_branch_guard_logins(fx.main, false).await.unwrap();
        fx.reconcile(fx.main).await;
        fx.direct(&fx.ep, "pb_ci", "pb-ci-pass", "CREATE TABLE public.pb_t2 (id int);").await.unwrap();
        // Switch on again; an unprotected child of the protected branch carries no guard.
        fx.metadata.set_branch_guard_logins(fx.main, true).await.unwrap();
        fx.reconcile(fx.main).await;
        let child = fx.child("pb-child").await;
        let child_ep = fx.endpoint_of(child).await;
        fx.reconcile(child).await;
        let triggers = fx.sql_on(&child_ep, &fx.database, "SELECT count(*) FROM pg_event_trigger WHERE evtname LIKE 'chelabase_schema_guard%'").await;
        assert_eq!(triggers.trim(), "0");
        let schema = fx.sql_on(&child_ep, &fx.database, "SELECT count(*) FROM pg_namespace WHERE nspname = 'chelabase_guard'").await;
        assert_eq!(schema.trim(), "0");
        // The child gave pb_ci a fresh password (protected parent): set a known one.
        fx.sql_on(&child_ep, "postgres", "ALTER ROLE pb_ci PASSWORD 'pb-ci-child';").await;
        fx.direct(&child_ep, "pb_ci", "pb-ci-child", "CREATE TABLE public.pb_t5 (id int);").await.unwrap();
        // Clean up main.
        fx.metadata.set_branch_protected(fx.main, false).await.unwrap();
        fx.reconcile(fx.main).await;
        fx.sql_on(&fx.ep, &fx.database, "DROP TABLE IF EXISTS public.pb_t1, public.pb_t2, public.pb_t3; REVOKE CREATE ON SCHEMA public FROM pb_ci, pb_deploy, pb_late; DROP ROLE pb_ci; DROP ROLE pb_deploy; DROP ROLE pb_late;").await;
        fx.metadata.remove_guard_exemption(fx.main, "pb_deploy").await.unwrap();
    })
    .await;
}
```

**Gotchas:** `session_user` is a `name`; convert with `convert_to(session_user, 'UTF8')` (the implicit cast to text). A guard installed under TE-A's script (no settings line) fails the new check and is reinstalled: no migration needed. The FakeBackend's default answer is empty, so the presence check answers "not `t`" in every older test. `child()` in the live fixture copies roles with fresh passwords because the parent is protected (6a). If `fx.direct` against the child can't reach it, run its DDL check through `sql_on` with `SET SESSION AUTHORIZATION pb_ci` instead and report it as a deviation.

**Verify:** `cargo nextest run --manifest-path <wt>/Cargo.toml -p worker`; `cargo clippy --manifest-path <wt>/Cargo.toml -p worker --all-targets -- -D warnings`; live: `… workbench test -p worker --test member_roles_live -- --run-ignored only` (the whole file: TE-A's guard tests must stay green).

---

## T4: orchestrator API (opus / opus)

**Files:** `services/orchestrator/src/{branches.rs,roles.rs,lib.rs,openapi.rs}`, new `services/orchestrator/src/protection.rs`, `services/orchestrator/tests/common/fakes.rs`, `services/orchestrator/tests/{access_walk.rs,role_rules_agree.rs}`, `services/orchestrator/tests/api.rs` (coordinator section + the two rewritten tests), `crates/chelabase-routes/src/lib.rs`, `crates/chelabase-types/src/api_error.rs`, `apps/docs/content/docs/reference/{errors.mdx,access.mdx}`, `openapi/*.json` and `packages/management-client/src/generated` (via `just api-gen`).

**Behaviour:** spec §3, §4; Shared interfaces "API"; P3–P9. The scan runs per branch database as the owner (`connect_as_owner`, `owner_name`), `simple_text(review_sql(owner))`, `parse_review(database_name, …)`. A database the owner can't connect to fails the request (`503 unavailable` or the connect error as today), never a partial list.

**RED tests (coordinator)**: in `services/orchestrator/tests/api.rs`, replace `update_branch_protected` with:

```rust
#[tokio::test]
async fn update_branch_protected() {
    let t = branching().await;
    let dev = t.ready("dev");
    // Protecting runs the review scan: the fake has no compute (P4), so 503 and no change.
    let err = error_of(t.send(Method::PATCH, &t.branch(dev), Some(json!({ "protected": true }))).await, 503).await;
    assert_eq!(code_of(&err), "unavailable");
    assert!(!t.metadata.branch(dev).protected);
    // Unprotecting needs no compute.
    t.metadata.branches.lock().unwrap().get_mut(&dev).unwrap().protected = true;
    let body: Value = t.send(Method::PATCH, &t.branch(dev), Some(json!({ "protected": false }))).await.json().await.unwrap();
    assert_eq!(body["protected"], false);
    assert_eq!(body["guard_logins"], true);
    assert!(body["review_acknowledged_hash"].is_null());
    // A backup: 409; unknown: 404; a deleting branch: 409 branch_not_ready (before any scan).
    let backup = t.backup(dev);
    let err = error_of(t.send(Method::PATCH, &t.branch(backup), Some(json!({ "protected": true }))).await, 409).await;
    assert_eq!(code_of(&err), "backup_branch");
    assert_eq!(t.send(Method::PATCH, &t.branch(BranchId::new()), Some(json!({ "protected": true }))).await.status(), 404);
    let deleting = t.add("gone", t.main(), BranchKind::Normal, BranchStatus::Deleting);
    let err = error_of(t.send(Method::PATCH, &t.branch(deleting), Some(json!({ "protected": true }))).await, 409).await;
    assert_eq!(code_of(&err), "branch_not_ready");
    // Unknown or missing field, a hash without protect: 422 (P5).
    for body in [
        json!({ "protected": true, "name": "x" }),
        json!({}),
        json!({ "protected": "yes" }),
        json!({ "review_hash": "abc" }),
        json!({ "protected": false, "review_hash": "abc" }),
        json!({ "guard_logins": "on" }),
    ] {
        let err = error_of(t.send(Method::PATCH, &t.branch(dev), Some(body.clone())).await, 422).await;
        assert_eq!(code_of(&err), "invalid_request", "{body}");
    }
    // Malformed JSON: 400.
    let resp = reqwest::Client::new()
        .patch(format!("{}{}", t.app.base, t.branch(dev)))
        .header(IDENTITY_HEADER, t.app.token_for_org("org-1"))
        .header("content-type", "application/json")
        .body("{\"protected\": tru")
        .send()
        .await
        .unwrap();
    let err = error_of(resp, 400).await;
    assert_eq!(code_of(&err), "invalid_request");
    assert!(!t.metadata.branch(dev).protected);
    // Needs the management credential.
    let resp = reqwest::Client::new()
        .patch(format!("{}{}", t.app.base, t.branch(dev)))
        .json(&json!({ "protected": false }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
}
```

(The coordinator keeps the original test's tail after `.unwrap();` if it asserts more than the 401; the RED commit shows the exact text.)

Replace `protecting_a_branch_reconciles_its_member_roles` with:

```rust
#[tokio::test]
async fn guard_changes_reconcile_member_roles() {
    let t = branching().await;
    let main = t.main();
    t.metadata.branches.lock().unwrap().get_mut(&main).unwrap().protected = true;
    assert_eq!(t.send(Method::PATCH, &t.branch(main), Some(json!({ "protected": false }))).await.status(), 200);
    assert_eq!(t.send(Method::PATCH, &t.branch(main), Some(json!({ "guard_logins": false }))).await.status(), 200);
    assert_eq!(t.send(Method::PUT, &format!("{}/guard-exemptions/deploy", t.branch(main)), None).await.status(), 200);
    assert_eq!(t.send(Method::DELETE, &format!("{}/guard-exemptions/deploy", t.branch(main)), None).await.status(), 200);
    assert_eq!(*t.jobs.member_role_enqueues.lock().unwrap(), vec![main, main, main, main]);
}
```

Append:

```rust
// --- Protected branches: switch, exemptions, review (PB, T4) -------------------------------

#[tokio::test]
async fn the_switch_and_exemptions_round_trip() {
    let t = sql_app().await;
    let main = t.main();
    let body: Value = t.send(Method::PATCH, &t.branch(main), Some(json!({ "guard_logins": false }))).await.json().await.unwrap();
    assert_eq!(body["guard_logins"], false);
    assert!(!t.metadata.branch(main).guard_logins);
    let list = |resp: reqwest::Response| async move { resp.json::<Value>().await.unwrap()["exemptions"].clone() };
    let exemptions = format!("{}/guard-exemptions", t.branch(main));
    assert_eq!(list(t.send(Method::GET, &exemptions, None).await).await, json!([]));
    assert_eq!(list(t.send(Method::PUT, &format!("{exemptions}/editor"), None).await).await, json!(["editor"]));
    assert_eq!(list(t.send(Method::PUT, &format!("{exemptions}/editor"), None).await).await, json!(["editor"]), "idempotent");
    assert_eq!(list(t.send(Method::PUT, &format!("{exemptions}/Ci%20Bot"), None).await).await, json!(["Ci Bot", "editor"]));
    // The role list says which login roles are exempt.
    let roles: Value = t.send(Method::GET, &format!("{}/roles", t.branch(main)), None).await.json().await.unwrap();
    let exempt = |name: &str| roles.as_array().unwrap().iter().find(|r| r["name"] == name).unwrap()["guard_exempt"].clone();
    assert_eq!((exempt("editor"), exempt("postgres")), (json!(true), json!(false)));
    assert_eq!(list(t.send(Method::DELETE, &format!("{exemptions}/editor"), None).await).await, json!(["Ci Bot"]));
    assert_eq!(list(t.send(Method::DELETE, &format!("{exemptions}/editor"), None).await).await, json!(["Ci Bot"]), "idempotent");
    // A name of 64 bytes: 422 (P10).
    let err = error_of(t.send(Method::PUT, &format!("{exemptions}/{}", "a".repeat(64)), None).await, 422).await;
    assert_eq!(code_of(&err), "invalid_request");
    // Unknown branch: 404.
    assert_eq!(t.send(Method::GET, &format!("{}/guard-exemptions", t.branch(BranchId::new())), None).await.status(), 404);
}

#[tokio::test]
async fn guard_writes_need_admin_and_reads_need_developer() {
    let (t, _dev) = act_as_app().await;
    let main = t.main();
    let exemptions = format!("{}/guard-exemptions", t.branch(main));
    for (method, path, body) in [
        (Method::PATCH, t.branch(main), Some(json!({ "guard_logins": false }))),
        (Method::PUT, format!("{exemptions}/deploy"), None),
        (Method::DELETE, format!("{exemptions}/deploy"), None),
        (Method::POST, format!("{}/review/acknowledge", t.branch(main)), Some(json!({ "hash": "h" }))),
        (Method::POST, format!("{}/review/fix", t.branch(main)), Some(json!({ "key": "k", "action": "drop", "hash": "h" }))),
    ] {
        let err = error_of(t.send_as("dev", method.clone(), &path, body).await, 403).await;
        assert_eq!((code_of(&err), err["error"]["required"].as_str()), ("forbidden", Some("admin")), "{method} {path}");
    }
    assert_eq!(t.send_as("dev", Method::GET, &exemptions, None).await.status(), 200);
    assert!(t.metadata.branch(main).guard_logins, "unchanged");
    // The review read is allowed for a Developer (it needs a compute: the fake has none).
    let err = error_of(t.send_as("dev", Method::GET, &format!("{}/review", t.branch(main)), None).await, 503).await;
    assert_eq!(code_of(&err), "unavailable");
}

#[tokio::test]
async fn review_requests_validate_before_any_compute() {
    let t = sql_app().await;
    let main = t.main();
    for (path, body) in [
        ("review/acknowledge", json!({})),
        ("review/acknowledge", json!({ "hash": "h", "x": 1 })),
        ("review/fix", json!({ "key": "k", "action": "burn", "hash": "h" })),
        ("review/fix", json!({ "key": "k", "action": "drop" })),
    ] {
        let err = error_of(t.send(Method::POST, &format!("{}/{path}", t.branch(main)), Some(body.clone())).await, 422).await;
        assert_eq!(code_of(&err), "invalid_request", "{path} {body}");
    }
    assert_eq!(t.compute_lookups(), 0);
}

/// A non-owner role on the scratch compute, and a SECURITY DEFINER function it owns.
async fn plant(scratch: &ScratchCompute, function: &str) -> String {
    let dev = format!("{}_dev", scratch.owner);
    scratch
        .exec_in(&format!(
            "DO $$BEGIN IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname = '{dev}') THEN CREATE ROLE {dev}; END IF; END$$; \
             CREATE FUNCTION public.{function}() RETURNS int LANGUAGE sql SECURITY DEFINER AS 'SELECT 1'; \
             ALTER FUNCTION public.{function}() OWNER TO {dev};"
        ))
        .await;
    dev
}

#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn protect_needs_the_review_hash_and_fixes_apply() {
    let (scratch, t) = changes_app("rev-pass").await;
    let main = t.main();
    let review_path = format!("{}/review", t.branch(main));
    let dev = plant(&scratch, "planted").await;
    let review: Value = t.send(Method::GET, &review_path, None).await.json().await.unwrap();
    let key = format!("{}/function/public.planted()", scratch.database);
    let items = review["items"].as_array().unwrap();
    assert!(items.iter().any(|i| i["key"] == key.as_str() && i["owner"] == dev.as_str() && i["new"] == false), "{review}");
    let hash = review["hash"].as_str().unwrap().to_string();
    assert!(review["acknowledged_hash"].is_null());
    // No hash, or a stale one: 409 review_changed with the current review.
    for body in [json!({ "protected": true }), json!({ "protected": true, "review_hash": "0".repeat(64) })] {
        let err = error_of(t.send(Method::PATCH, &t.branch(main), Some(body.clone())).await, 409).await;
        assert_eq!(code_of(&err), "review_changed", "{body}");
        assert_eq!(err["error"]["review"]["hash"], hash.as_str());
        assert!(!t.metadata.branch(main).protected);
    }
    let body: Value = t.send(Method::PATCH, &t.branch(main), Some(json!({ "protected": true, "review_hash": hash }))).await.json().await.unwrap();
    assert_eq!((body["protected"].as_bool(), body["review_acknowledged_hash"].as_str()), (Some(true), Some(hash.as_str())));
    // Something planted later is flagged new.
    plant(&scratch, "planted_later").await;
    let review: Value = t.send(Method::GET, &review_path, None).await.json().await.unwrap();
    let new_of = |name: &str| review["items"].as_array().unwrap().iter().find(|i| i["name"] == name).unwrap()["new"].clone();
    assert_eq!((new_of("planted"), new_of("planted_later")), (json!(false), json!(true)));
    assert_eq!(review["acknowledged_hash"], hash.as_str());
    let current = review["hash"].as_str().unwrap().to_string();
    assert_ne!(current, hash);
    // Acknowledge: stale 409, current 200.
    let err = error_of(t.send(Method::POST, &format!("{review_path}/acknowledge"), Some(json!({ "hash": hash }))).await, 409).await;
    assert_eq!(code_of(&err), "review_changed");
    assert_eq!(t.send(Method::POST, &format!("{review_path}/acknowledge"), Some(json!({ "hash": current }))).await.status(), 200);
    assert_eq!(t.metadata.branch(main).review_ack_hash.as_deref(), Some(current.as_str()));
    // Fixes: stale 409, unknown key 404, reassign and drop as the owner.
    let fix = |key: String, action: &str, hash: &str| json!({ "key": key, "action": action, "hash": hash });
    let err = error_of(t.send(Method::POST, &format!("{review_path}/fix"), Some(fix(key.clone(), "reassign", &hash))).await, 409).await;
    assert_eq!(code_of(&err), "review_changed");
    let err = error_of(t.send(Method::POST, &format!("{review_path}/fix"), Some(fix("nope".into(), "drop", &current))).await, 404).await;
    assert_eq!(code_of(&err), "not_found");
    let resp: Value = t.send(Method::POST, &format!("{review_path}/fix"), Some(fix(key.clone(), "reassign", &current))).await.json().await.unwrap();
    assert_eq!(resp["sql"], format!("ALTER ROUTINE public.planted() OWNER TO {};", scratch.owner));
    assert!(resp["review"]["items"].as_array().unwrap().iter().all(|i| i["name"] != "planted"), "{resp}");
    let later = format!("{}/function/public.planted_later()", scratch.database);
    let after = resp["review"]["hash"].as_str().unwrap().to_string();
    let resp: Value = t.send(Method::POST, &format!("{review_path}/fix"), Some(fix(later, "drop", &after))).await.json().await.unwrap();
    assert_eq!(resp["sql"], "DROP ROUTINE public.planted_later() RESTRICT;");
    assert!(!scratch.yes("SELECT to_regprocedure('public.planted_later()') IS NOT NULL").await);
    assert_eq!(scratch.yes(&format!("SELECT pg_get_userbyid(proowner) = '{}' FROM pg_proc WHERE oid = 'public.planted()'::regprocedure", scratch.owner)).await, true);
    // Clean up.
    t.metadata.branches.lock().unwrap().get_mut(&main).unwrap().protected = false;
    wait_no_editor_sessions(&scratch).await;
    scratch.exec_in(&format!("DROP FUNCTION public.planted(); DROP ROLE {dev};")).await;
    scratch.drop().await;
}
```

**Gotchas:** `UPDATE_BRANCH` stays `Access::Project(Admin)`; the new reads use `Access::Project(Developer)`. Every new route goes in `ENDPOINTS`, `access_walk.rs`, `role_rules_agree.rs` and the generated access table; every new schema in `openapi.rs`'s list; `review_changed` in `ErrorCode`, `ALL` (96), `as_str`, the test table and `errors.mdx`. `review_changed`'s body: build it like other typed errors that carry fields (`op_index`, `required`). Run `just api-gen` and commit the regenerated client. The fix runs `SET LOCAL lock_timeout = '5s'` in a transaction, then the statement (P7); a Postgres error maps through `classify_failure` like a schema change (`has_dependents`-style 409 for `2BP01` if that mapping exists, else `schema_change_failed` 400 with the SQLSTATE).

**Verify:** `cargo nextest run --manifest-path <wt>/Cargo.toml -p orchestrator -p chelabase-routes -p chelabase-types`; clippy on those with `--all-targets -- -D warnings`; `just --justfile <wt>/justfile --working-directory <wt> check` once at the end of the task (it covers the docs tables and the client); live: `… workbench test -p orchestrator --test api -- --run-ignored only protect_needs_the_review_hash_and_fixes_apply`.

---

## T5: Studio (sonnet / sonnet)

**Files:** `apps/studio/src/components/branch-actions.tsx`, new `apps/studio/src/components/branch-review.tsx` (the list, reasons, per-item Reassign/Drop with confirm; used by both panels), `apps/studio/src/components/branch-roles.tsx`, tests `branch-actions.test.tsx`, `branch-roles.test.tsx` (coordinator: new cases, plus the new fields in `mk()` and `role()`).

**Behaviour:** spec §5; labels pinned here. Item label: a function `schema.name(identity)`, a trigger or rule `name on identity`. A reason shows its kind with `_` as a space, then `via` when present (`policy public.t.p`). The Protect panel loads the review on open, shows the switch "Guard login roles" (a `role="switch"`), the list, and "Confirm protect" (sends `{ protected: true, review_hash }`, plus `guard_logins` only when changed). On `review_changed` it shows "The review changed since you loaded it. Check the list again." and shows `error.review`. Each item's buttons are labelled `Reassign <label>` and `Drop <label>` and open a confirm ("Confirm reassign" / "Confirm drop"; the reassign confirm says "The owner takes this code over. A SECURITY DEFINER function then runs as the owner."). The Protection panel (button "Protection", protected branches, Admin) holds the switch, the review and "Acknowledge" (sends `{ hash }`). A protected branch whose review hash differs from `acknowledged_hash` shows the badge "Review changed" on its row (the review query runs only for protected branches and Admins). In the Roles panel each login role other than the owner gets a `role="switch"` labelled `Exempt <name> from the login guard`, checked from `guard_exempt`, sending `PUT`/`DELETE …/guard-exemptions/<name>`.

**RED tests (coordinator)**: in `branch-actions.test.tsx`, `mk()` gains `guard_logins: true, review_acknowledged_hash: null,`; add near the top:

```tsx
const REVIEW = "GET /v1/databases/db/branches/d/review";
const ACK = "POST /v1/databases/db/branches/d/review/acknowledge";
const FIX = "POST /v1/databases/db/branches/d/review/fix";
const planted = {
  key: "web1/function/public.planted()",
  database: "web1",
  kind: "function",
  schema: "public",
  name: "planted",
  identity: "",
  function: "public.planted()",
  owner: "dev",
  reasons: [{ kind: "security_definer", via: null }, { kind: "policy", via: "public.t.p" }],
  opaque_body: true,
  new: false,
};
const review = (over: object = {}) => ({ items: [planted], hash: "h1", acknowledged_hash: null, ...over });
```

and a `describe("protected branches (PB)")` block:

```tsx
describe("protected branches (PB)", () => {
  it("protect_shows_the_review_and_sends_its_hash", async () => {
    const patch = vi.fn(() => jsonResponse({ ...dev, protected: true, review_acknowledged_hash: "h1" }));
    const { fetch } = setup(dev, [main, dev], { [REVIEW]: () => jsonResponse(review()), [PATCH]: patch });
    await userEvent.click(btn("Protect"));
    expect(await screen.findByText("public.planted()")).toBeInTheDocument();
    expect(screen.getByText("security definer")).toBeInTheDocument();
    expect(screen.getByText("policy public.t.p")).toBeInTheDocument();
    expect(screen.getByRole("switch", { name: "Guard login roles" })).toBeChecked();
    await userEvent.click(btn("Confirm protect"));
    await vi.waitFor(() => expect(patch).toHaveBeenCalled());
    const call = fetch.calls.find((r) => r.method === "PATCH")!;
    expect(await call.clone().json()).toEqual({ protected: true, review_hash: "h1" });
  });

  it("protect_sends_the_switch_when_changed", async () => {
    const patch = vi.fn(() => jsonResponse({ ...dev, protected: true, guard_logins: false }));
    const { fetch } = setup(dev, [main, dev], { [REVIEW]: () => jsonResponse(review({ items: [], hash: "e0" })), [PATCH]: patch });
    await userEvent.click(btn("Protect"));
    await userEvent.click(await screen.findByRole("switch", { name: "Guard login roles" }));
    await userEvent.click(btn("Confirm protect"));
    await vi.waitFor(() => expect(patch).toHaveBeenCalled());
    const call = fetch.calls.find((r) => r.method === "PATCH")!;
    expect(await call.clone().json()).toEqual({ protected: true, review_hash: "e0", guard_logins: false });
  });

  it("protect_shows_the_changed_review_on_review_changed", async () => {
    const later = { ...planted, key: "web1/function/public.later()", name: "later", function: "public.later()", new: true };
    const patch = vi.fn(() =>
      jsonResponse({ error: { code: "review_changed", message: "changed", review: review({ items: [planted, later], hash: "h2" }) } }, 409),
    );
    setup(dev, [main, dev], { [REVIEW]: () => jsonResponse(review()), [PATCH]: patch });
    await userEvent.click(btn("Protect"));
    await screen.findByText("public.planted()");
    await userEvent.click(btn("Confirm protect"));
    expect(await screen.findByText("The review changed since you loaded it. Check the list again.")).toBeInTheDocument();
    expect(await screen.findByText("public.later()")).toBeInTheDocument();
  });

  it("reassign_confirms_then_posts_the_fix", async () => {
    const fix = vi.fn(() => jsonResponse({ sql: "ALTER ROUTINE public.planted() OWNER TO postgres;", review: review({ items: [], hash: "e0" }) }));
    const { fetch } = setup(dev, [main, dev], { [REVIEW]: () => jsonResponse(review()), [FIX]: fix });
    await userEvent.click(btn("Protect"));
    await userEvent.click(await screen.findByRole("button", { name: "Reassign public.planted()" }));
    expect(screen.getByText("The owner takes this code over. A SECURITY DEFINER function then runs as the owner.")).toBeInTheDocument();
    expect(fix).not.toHaveBeenCalled();
    await userEvent.click(btn("Confirm reassign"));
    await vi.waitFor(() => expect(fix).toHaveBeenCalled());
    const call = fetch.calls.find((r) => r.method === "POST" && r.url.endsWith("/review/fix"))!;
    expect(await call.clone().json()).toEqual({ key: planted.key, action: "reassign", hash: "h1" });
  });

  it("protection_panel_toggles_and_acknowledges", async () => {
    const guarded = { ...dev, protected: true, review_acknowledged_hash: "h0" };
    const patch = vi.fn(() => jsonResponse({ ...guarded, guard_logins: false }));
    const ack = vi.fn(() => jsonResponse(review({ acknowledged_hash: "h1" })));
    const { fetch } = setup(guarded, [main, guarded], {
      [REVIEW]: () => jsonResponse(review({ acknowledged_hash: "h0", items: [{ ...planted, new: true }] })),
      [PATCH]: patch,
      [ACK]: ack,
    });
    expect(await screen.findByText("Review changed")).toBeInTheDocument();
    await userEvent.click(btn("Protection"));
    await userEvent.click(await screen.findByRole("switch", { name: "Guard login roles" }));
    await vi.waitFor(() => expect(patch).toHaveBeenCalled());
    expect(await fetch.calls.find((r) => r.method === "PATCH")!.clone().json()).toEqual({ guard_logins: false });
    await userEvent.click(btn("Acknowledge"));
    await vi.waitFor(() => expect(ack).toHaveBeenCalled());
    expect(await fetch.calls.find((r) => r.url.endsWith("/review/acknowledge"))!.clone().json()).toEqual({ hash: "h1" });
  });

  it("no_badge_when_the_review_is_acknowledged", async () => {
    const guarded = { ...dev, protected: true, review_acknowledged_hash: "h1" };
    const { fetch } = setup(guarded, [main, guarded], { [REVIEW]: () => jsonResponse(review({ acknowledged_hash: "h1" })) });
    await vi.waitFor(() => expect(sent(fetch)).toContain("GET /v1/databases/db/branches/d/review"));
    expect(screen.queryByText("Review changed")).toBeNull();
  });

  it("an_unprotected_branch_loads_no_review_until_protect_opens", () => {
    const { fetch } = setup(dev, [main, dev], { [REVIEW]: () => jsonResponse(review()) });
    expect(sent(fetch)).not.toContain("GET /v1/databases/db/branches/d/review");
  });
});
```

The existing `protect_toggle_confirms_and_patches` gains `[REVIEW]: () => jsonResponse(review({ items: [], hash: "e0" }))` in its routes and expects the body `{ protected: true, review_hash: "e0" }` (the coordinator edits it in the RED commit).

In `branch-roles.test.tsx`, `role()` gains `guard_exempt: false,`; add:

```tsx
describe("login guard exemptions (PB)", () => {
  const EXEMPT = (name: string) => `/v1/databases/db/branches/br/guard-exemptions/${name}`;

  it("exempt_toggle_puts_and_deletes", async () => {
    const exempt = { ...reader, guard_exempt: true };
    const put = vi.fn(() => jsonResponse({ exemptions: ["analyst"] }));
    const del = vi.fn(() => jsonResponse({ exemptions: [] }));
    renderWithQuery(<BranchRoles databaseId="db" branchId="br" />, {
      fetch: fakeFetch({
        [`GET ${ROLES}`]: () => jsonResponse([owner, exempt, sqlRole]),
        [`PUT ${EXEMPT("analyst")}`]: put,
        [`DELETE ${EXEMPT("reporting")}`]: del,
      }),
    });
    const on = await screen.findByRole("switch", { name: "Exempt reporting from the login guard" });
    expect(on).toBeChecked();
    const off = screen.getByRole("switch", { name: "Exempt analyst from the login guard" });
    expect(off).not.toBeChecked();
    expect(screen.queryByRole("switch", { name: `Exempt ${owner.name} from the login guard` })).toBeNull();
    await userEvent.click(off);
    await vi.waitFor(() => expect(put).toHaveBeenCalled());
    await userEvent.click(on);
    await vi.waitFor(() => expect(del).toHaveBeenCalled());
  });
});
```

**Gotchas:** the generated client's names come from P9's operation ids (`useGetBranchReview`, `useAcknowledgeBranchReview`, `useFixBranchReviewItem`, `usePutGuardExemption`, `useDeleteGuardExemption`); mutations invalidate the branch list and the review query. `BranchRoles` already renders inside an Admin-only area; the switches follow `useProjectCaps().atLeast("admin")`. Non-login roles get no switch.

**Verify:** `pnpm --dir <wt> --filter @chelabase/studio test -- branch-actions branch-roles`; `pnpm --dir <wt> --filter @chelabase/studio typecheck`; browser pass by the coordinator in the live pass.

---

## T6: docs (haiku / none)

**Files:** `apps/docs/content/docs/using/branches.mdx` ("Protected branches"), `apps/docs/content/docs/architecture/security.mdx` (the guard paragraph), `apps/docs/content/docs/using/login-roles.mdx` (one note on exemptions).

**Behaviour:** describe, from the spec: what protect blocks for members and (with the switch on) for login roles; exemptions per branch by name; children copy the switch and exemptions; the review (what it lists, why, the hash on protect, reassign/drop, "new since review"); the API routes by path. Plain sentences, no marketing. No RED tests (prose); `just check` builds the docs.

**Verify:** `pnpm --dir <wt> --filter @chelabase/docs build`.

---

## End of step

1. Whole-branch review (opus) on `main..protected-branches`.
2. Fix round: the coordinator writes a failing test for each finding first.
3. `just check`.
4. No fork changed: no gate, no image build (pending fork changes: none).
5. Live pass on this worktree's stack (`just workbench up`): `workbench test -p chelabase-metadata --test live_db -- --run-ignored only`; `-p chelabase-schema --test review_live -- --run-ignored only`; `-p worker --test member_roles_live -- --run-ignored only`; `-p worker --test roles_live -- --run-ignored only`; `-p orchestrator --test api -- --run-ignored only`; `just sql-editor-e2e`; a browser pass (protect with a planted function, fix, acknowledge, exemption toggle, badge).
6. Update `docs/landing-highlights.md` on `main`, the plan Status and the roadmap's "Resume here", then pause for the user (merge decision).
