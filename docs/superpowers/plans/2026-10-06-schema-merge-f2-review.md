# Step 6g T14, F2 review and its fix round

F2 = roles, keys and bases fixes, commit `33d28ad` on branch `schema-merge-f2` (base `5b6f6b7`). Reviewed by opus on 2026-10-08: **Needs fixes**, 2 Important, 7 Minor, nothing Critical. `cargo test --workspace` and clippy `-D warnings` passed at `33d28ad`.

The fix round below was in progress when the session hit its usage limit. Its partial work is the unverified WIP commit `4483e1e` on `schema-merge-f2`. **On resume:** read `git diff 33d28ad 4483e1e`, finish every item below, run `just api-gen` (the generated management client was restored to `33d28ad`'s mid-regeneration), build, test and clippy, then the coordinator checks the diff `33d28ad..` and merges F2 into `schema-merge`.

## What the review confirmed (no change needed)

- A SQL `DROP ROLE` of an API role sticks: the receiver's `DelRole` calls `remove_copy`, which deletes rows of any non-owner origin (`metadata/src/ddl.rs:249`), so the resync has no row to bring back.
- A SQL `ALTER ROLE … PASSWORD` on an API role sticks: `upsert_sql_copy` updates the verifier and keeps the origin.
- The first resync after an upgrade revokes nothing (`attributes_pending` defaults to false).
- SCRAM verifiers survive a rename (Postgres clears only MD5, which those rows store as NULL); OID and timeline are kept (E26); both proxy names are invalidated.
- `role_cap_reached` as 409 on updateRole matches createRole: the deviation is fine.
- The restore-chain CTE (I1) is correct; M7, M8 and the three migrations are correct and idempotent.

## Fix round: every item, nothing deferred

**I-1 (E32 race).** `services/worker/src/roles.rs:1073-1080`, `crates/chelabase-metadata/src/neon.rs:1452`: `confirm_role` clears `attributes_pending` only while `roles_version` is unchanged. A receiver write during a PATCH or create (e.g. a migration's `CREATE ROLE x PASSWORD …`) leaves it true with no resync scheduled. Later, a SQL `GRANT readers TO app` triggers a touched resync that drives the stale stored state and REVOKEs `readers`.
- Fix: a per-row token (`attributes_change_id uuid`) written by insert and update; confirm clears `attributes_pending` `WHERE attributes_change_id = $token`. Keep the version guard for `pending` (the password). Also publish `SUBJECT_RESYNC_BRANCH` when a confirm is lost.
- Test: confirm lost to a version bump, then a catalog change, then a resync: the resync records the change, never reverts it.

**I-2 (E29 rename rollback).** `services/worker/src/roles.rs:1030-1040`: if `admin_sql` errors after the `ALTER ROLE … RENAME` committed (timeout, dropped connection), the row and keys move back to `from`, the next resync re-creates `from` with the same password, and `to` becomes an unmanaged SQL role with the keys following the OID.
- Fix: on error, read the catalog for `from` and `to`. `to` exists and `from` doesn't: success. Move back only when `from` still exists. If the read fails, return a not-applied outcome (`NotReady`) and leave the move as is.
- Test: the fake fails after applying the rename.

**M1.** `services/worker/src/branch_switch.rs:480`: M9's window uses `STUCK_DETACH_SECS`, but an attempt may start just inside the bound and run `DETACH_TIMEOUT` longer. Use `STUCK_DETACH_SECS + DETACH_TIMEOUT` and fix the comment ("past the bound any detach has ended" is wrong).

**M2.** The E32 exhaustive test (`services/worker/src/resync.rs:2230`) sets `pending` equal to `attributes_pending`. Decouple them (a password reset in flight, `pending` true and `attributes_pending` false, drives only the password) and add the role-missing and password-differs dimensions.

**M3 → F4.** Studio "View as" (`apps/studio/.../tables-view.tsx:140-141`) still says owner-level roles "can bypass row-level security"; a BYPASSRLS role is now selectable with no "RLS bypassed, edits apply to every row" warning like the Owner option has. F4 fixes it.

**M4.** `rename_role_copy` (`neon.rs`, the rename UPDATE) forces `password_unreadable = false` whenever a sealed password is passed, so a rollback loses the flag. Pass the original flag through.

**M5.** `rename_role_copy` returns `Taken` for an unknown endpoint (`neon.rs:1542`), which `rename_role` reports as `role_exists`. Return a not-found outcome, mapped to `NotFound`/`NotReady`.

**M6.** `services/orchestrator/src/branches.rs:271`: the `valid_lsn` doc line ("Postgres text form…") sits above `lsn_value`. Move it back.

**M7.**
- Add `CHECK (kind = 'secret' OR NOT role_bypass_rls)` on `data_api_keys`, in F2's own migration (not an older one).
- rest's per-request gate (`services/rest/src/exec.rs:37`, `OWNER_LEVEL_SQL`) must refuse exactly what the create-time check refuses, including membership in a superuser role and the `pg_*` system roles the create check refuses: one shared definition, or a test pinning both to the same set. (This gap predates E33.)
- Restores made before the upgrade have no `branch_restores` link, so their chains still re-baseline: accepted (recorded in the Status); add a one-line doc note on the CTE.

**E33a (user, 2026-10-08).** Keys whose role gained BYPASSRLS in SQL work after the upgrade (E33's intent). At worker start, run the key-role sync (`sync_key_roles`, which sets `role_bypass_rls`) once for every ready branch with live secret keys, so the "bypasses RLS" marks are right immediately (bounded concurrency; failures logged and left to the normal resync). Test with fakes. Add one line to `apps/docs/content/docs/using/data-api/keys.mdx`: keys on roles that gained BYPASSRLS in SQL work from this release and are marked.

## Verification for the fix round

`cargo build --workspace --all-targets`, `cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D warnings`, `just api-gen` plus the openapi drift tests, the Studio typecheck if clients changed, and the docs typecheck and build. Report: a checklist of every item above with its test name, RED evidence, and any interface change for F3.
