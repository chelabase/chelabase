# TE-B1: the table editor (implementation plan)

**Goal:** Studio's top-level "Table editor": per-level context pickers and a deep URL, a virtualized row grid with inline editing, a row panel, selection with copy/delete, a column strip and Structure view, columns, PK/unique/FK/check constraints, table create/rename/move/comment/drop, enums, the RLS toggle, and a changeset panel that shows TE-A's dry run and applies every staged structure change in one transaction. It replaces the project page's inline Tables console.

**Spec:** `docs/superpowers/specs/2026-10-14-table-editor-design.md` (approved 2026-10-14). Surface brief and direction contract: `apps/studio/.impeccable/surfaces/src-components-table-editor-table-editor-tsx.md`; sketches `apps/studio/.impeccable/mocks/decision/{switch-pickers,add-column,row-panel,grid-edges}.html`. Implementers read the spec section their task names and, for UI tasks, the surface brief and the four sketches.

**Architecture:** a pure model in `apps/studio/src/lib/table-editor/` (changeset fold/projection, column form, selection, dry-run mapping, context resolution, access); a zustand vanilla store (`store.ts`) provided through React context; thin components in `apps/studio/src/components/table-editor/`; one route file wiring URL params to `<TableEditor params navigate>`. Server data through the generated `@chelabase/management-client` hooks (TanStack Query) and `consoleQueryExact`.

**Tech:** React 19, TanStack Router 1.171 (optional path params `{-$x}`), TanStack Query 5, zustand 5, Base UI. New: `@tanstack/react-table` **9.3.1** (latest, 2026-10-12), `@tanstack/react-virtual` **3.14.19** (latest, 2026-10-13; if pnpm's minimum-release-age check refuses it at install, the newest it accepts, 3.14.18), `fast-check` **4.10.2** (dev; latest, 2026-09-19). Checked against the npm registry 2026-10-14. No fork change: no image build; the browser pass runs on the pinned images. No backend change: no Rust or live tests.

**Standing rule:** the coordinator writes and commits each task's RED tests (below) before the brief, runs them RED and records the result in the Status. Implementers make the tests pass without editing, weakening, skipping or deleting them. Reviewers check that `git diff <red-commit>..HEAD -- <test files>` is empty. When a RED commit needs a module to exist so that its tests compile, the coordinator adds only the empty module declaration.

## Status

- **RESUME HERE (2026-10-14): PAUSED for the user's merge decision.** Everything is done on `table-editor`: tasks, the whole-branch review and every fix batch (FA, FB, FC, FD, FE/FE2), the Impeccable finish and documenter, `just check` green (exit 0, 2611 tests), live: orchestrator `api` 449/449, `rest_live` 37/38 (the one failure is the parked compute_ctl migration race, user: keep parked), `sql-editor-e2e` and `console-e2e` green (with R18's checks), browser rechecks done (C1 as dev1 on a fresh branch, I1, I2, R18 Act as, the constraint popup). After the merge: `just wt-remove table-editor` (purges the stack), delete the branch locally and on origin. Open minor for later (not fixed, user to decide): the Branch picker doesn't refresh when a new branch becomes ready (needs a reload).
- **Done: FE + FE2** (opus): RED `36673aa1`, coordinator test fix `3b49ae14` (dev3's member-role row; worker RED), FE `dd11c5ab`, FE2 `c7c61d08`, merged `761d27b6`; coordinator lint fix (member-sign-in's studio.env check form). Migration `20261014000010_admin_grant_every_role.sql`. Verified in code: tenant admins are decided from the flag at `access.rs:323`, `lib.rs:2395` (/v1/me), worker `member_roles.rs:1094`; `RoleSet::may_act_as` keeps anon/authenticated out of `All`. **Process note:** a `just check` run reported "exit 0" from its wrapper while the gate had failed (FE2's lint); from then on the gate's own exit code is recorded in its log.
- **Done: FD** (opus, browser-pass fixes): RED `3dcbec0a`, fix `e78085e3`, merged `f1ac1c0e`; verified RED untouched; C1 via a shared `reconcile_member_role` helper (deviation accepted: the reconcile triggers on a login missing on the compute, not on a missing metadata row), new 503 `member_role_reconcile_failed` on the three reads (`openapi/management.json` regenerated); I1/I2/M2–M7 done; M7's popup placement still to see in a browser. `just check` green after FD. **R17 (user, 2026-10-14):** a project Admin's grant is every role on their project (`RoleSet::All`), narrowable per database; existing Admin grants migrate to All; the tenant-admin bypass is decided from the flag, not the grant's shape. Admin scope otherwise unchanged (user: keep it; project Admins already invite into their project via `createProjectInvite`, so no invites follow-up step). **R18 (user, 2026-10-14):** Act as and the SQL editor's Run as list only granted roles and never anon/authenticated unless a grant names them (R15 and FD's Admin anon rule undone). **TE-B2 note (user):** a read-only **View as** listing anon and authenticated for everyone plus the person's own granted roles (e.g. granted `billing_admin` → billing_admin, anon, authenticated), to see rows as that role would. **Coordinator test changes:** `admins_act_as_the_data_api_roles` rewritten; `sql-editor.test` `anon_listed_only_when_bootstrapped` → `admins_dont_run_as_anon_without_a_named_grant`; FD's R15 Studio tests → R18; `meAs("admin")` holds `"all"`.
- **Done: End of step 6, the browser pass** (opus agent, findings only): all admin and Developer checks pass except C1 (a Developer can't open the editor on a branch until the SQL editor reconciled their `cb_m_*` login there: TE-A's read-path CONNECT check never reconciles; on `main` too), I1 (the column form's preview addresses the old name after a pending rename), I2 (picking a branch/Postgres db keeps the old schema/table in the URL). Minors: M1 Act as lacks anon/authenticated (→ R15), M2 system db `postgres` offered, M3 filtered delete dialog has no count and says "Delete all rows", M4 docs footer text, M5 Types view has no Applied line, M6 a stale rows query for the old name after a rename, M7 Add constraint dialog name / popup covers its buttons. **R15 (user, 2026-10-14):** Act as offers anon and authenticated to anyone their grant allows (Admins always, as the SQL editor; members when their role set includes them; `act_as_reserved` already exempts both). **R16 (coordinator):** C1 is fixed in the orchestrator (not the parked fork work: round 2's patch 5 forwards role DDL, it doesn't create member logins): a table-API read whose subject is a member login missing on the branch asks the worker to reconcile that member once, then checks CONNECT; an existing login without CONNECT is refused as before (R12). Cost if wrong: one reconcile request per first read. **Coordinator test change:** `rows-view.test` `delete_all_matching_with_a_filter_uses_it` expects "Delete matching rows" (M3).
- **Done: End of steps 3–5:** FC (opus) RED `1b6e54af`, fix `ef4d3084`, merged `5842ad1e`; verified RED untouched; accepted: `rows-view.test` `estimate_unknown_reads_of_many` rewritten to `…full_page_reads_of_more` (it contradicted RED 4), the exact count applies only without an estimate, the resize grip sits beside the header (not inside), the column form uses the one-line dry-run text. Documenter `0ff4609c` (DESIGN.md + design.json: grids, strip, panel, checkbox, tab radio, micro 11px and note 13px steps, Sand Never Warns, One Panel); coordinator fixed the stale primary-hover line. Gate `just check` green once. No stack restart for the live pass: FB/FC changed no Rust source after the restart at `380f5206`. Live: `count_respects_rls_and_filters` PASS (ran twice: the coordinator's first run's output was filtered away), whole `rest_live` 38/38.
- **Done: End of step 2** (final fix round, R9): FA (opus) RED `4ae807d6`, coordinator `f5da0313` (20 s test timeout), fix `fcb15eac`, coordinator `b42f0798` (Testing Library `asyncUtilTimeout` 5 s: `findBy` under load), merged `380f5206`; FB (opus) RED `73517061`, fix `f61a229c`, merged into `table-editor` (clean). Verified: RED files untouched on both. FA deviation accepted: the dry run does answer `409 has_dependents` (`changes.rs:675`); the brief's "out of date" note was wrong; FA corrected the TE-A text that said a dry run always returns dependents. FB deviations accepted: T3 M7 help via `aria-describedby`; E-M6 reason "Loading roles…"; T2 M10 `historyWindow: number | undefined`; extra empty states. **E-M8 note:** the Types and New table views aren't deep-linkable; a reload or a schema switch returns to the table view. **Leftovers for the second fix batch (End of step 3):** (a) Apply in `changes-panel.tsx` also invalidates `[COUNT_QUERY, databaseId, branchId]` (T8 M2); (b) a column header's accessible name reads "memo Resize memo" (the named grip sits inside it; name the header from its label only); (c) T2 M7: context-bar captions still show below `sm` (needs `picker.tsx`).
- **Done (End of step 3, part 1):** stack apps restarted from the merged branch; live checks: R8 checkbox and copy, create → navigate with the Applied line kept (E-I2/E-M3), add column with dry run, untouched insert via insert-with-defaults (E-M1). Finish review: 16 items; item 5 (identity editable) not reproduced (RED `5_…` green: the field is read-only); the API serializes `identity.generation: "always"` correctly. `impeccable detect` isn't installed on this machine; the reviewer's detector run: 0 anti-patterns, 14 `text-[11px]` advisories → DESIGN.md micro step. **R14 (coordinator):** the second fix batch runs on opus like the final round (R9); one implementer, the coordinator verifies. Cost if wrong: model cost only.
- **Done: End of step 1** (opus whole-branch review on `b8d5b748`): Changes needed, 0C/3I/13M. E-I1 ops after a pending table rename/move (or after removing one) name the wrong table, including T6's own comment fold (fix: one `addressOps` normalization in `fold`/`foldAll`/`withoutEdit` plus a property test); E-I2 applying a rename/move/drop of the open table leaves "Table not found" (navigate to the new place or the schema); E-I3 the new-table copy says "not exposed" while the data API's default grants expose it (user ruling R8). Minors E-M1 empty insert sends `values:[{}]`; E-M2 sort/filter survive a rename/drop Apply; E-M3 create flashes "Table not found"; E-M4 a staged table drop leaves structure controls live; E-M5 enum edits not modelled by `withoutEdit`; E-M6 dry runs go out as the owner before roles load; E-M7 FK picker queries per keystroke; E-M8 Types/New table view on schema switch; E-M9 docs leftovers and label drift; E-M10 T4/T6 tests thin; E-M11 Move/New table can't target an empty schema; E-M12 typed constraint names checked only within the table; E-M13 old-console test misses two deleted files. Note 3b ruling: keep the dry-run pause while an edit dialog is open (S1 reads "400 ms after the last edit, and not while an edit dialog is open"), test it, and make `new_enum_and_add_value_after` wait for the 2-op body.
- **R8 (user, 2026-10-14):** the New table form gets a "Turn RLS on" checkbox, **unticked by default**; ticked, Create sends `create_table` with RLS on. Unticked, the form says plainly that in a data-API schema anon reads and authenticated writes every row (default grants), and the coral banner shows. The spec line "not exposed to the data API" and the docs are corrected. Cost if wrong: a default flip later.
- **R9 (user, 2026-10-14):** the final fix round runs as two opus implementers in parallel worktrees with disjoint files: **FA** (model + structure + Apply + types + new table: E-I1, E-I2, E-I3/R8, E-M3, E-M4, E-M5, E-M10 (T6 part), E-M11, E-M12, 3b, T1 note, T5 M1–M8, TE-A spec text, the fold property test's timeout) and **FB** (shell, rows, row panel, count, docs: T2 I1/M1–M4/M6–M10, T3 M1–M3/M5/M7/M8, T4 items, T7 item, T8 M1–M4, E-M1, E-M2, E-M6, E-M7, E-M8, E-M9, E-M10 (T4 part), E-M13, UI test timeouts). No second review (coordinator verifies the diffs) unless a fix changes design.
- **RED lines, final round:** FA `changeset-address.test.ts` 7/7 fail (ops after a rename/move keep the old table; removed rename leaves `bills`; label `Change comment on invoices`); `final-fixround-model.test.ts` 7 fail/2 pass (40P01 not retryable, enum ops kept by `withoutEdit`, names not truncated, typed name vs relation not checked); `final-fixround-fa.test.tsx` 13 fail/1 pass (no navigate after rename/move/drop Apply; "Table not found" at create navigate; no "Turn RLS on"; staged drop leaves controls live; no `Use "archive"`; add_column after rename names `invoices`; hidden check sent; changeset discarded before re-read; no persistent live region; focus request not consumed). FB `final-fixround-fb-model.test.ts` 4 fail/3 pass; `final-fixround-fb.test.tsx` 19 fail/5 pass (the 5 are E-M10/T7/E-M8 coverage, green already).
- **Coordinator test changes (final round):** `types-view.test.tsx` `new_enum_and_add_value_after` waits for the 2-op body (3b); `old-console-removed.test.ts` glob adds `console-filters.tsx`, `tables-view.test.tsx` (E-M13); `row-panel.test.tsx` / `rows-view.test.tsx` key deletes expect `returning: ["id"]` (T3 M8); `openapi_fidelity.rs` count reject/accept cases (T8 M3, green: closes M3); `use-dry-run-paused.test.tsx` (3b, green); Studio `vitest.config.ts` `testTimeout: 20_000` (the load flake; `f5da0313`).
- **R10 (coordinator):** Move to schema and New table's schema field accept a typed name (`Use "<name>"` option) since no endpoint lists empty schemas (E-M11). Cost if wrong: a schema-list source later.
- **R11 (coordinator):** generated constraint names model Postgres's 63-byte truncation and the numbered label within the table and changeset; schema-wide collisions with other relations are left to the dry run (T1 note). Cost if wrong: a wrong projected name until the dry run.
- **R12 (coordinator):** the non-drag resize (T3 M7, WCAG 2.5.7): the grip is a focusable separator; a click widens one step, Shift+click narrows; headers describe Alt+Left/Right. Cost if wrong: a different affordance later.
- **R13 (coordinator):** key deletes ask for `returning` of the key columns to report deletes narrowed by RLS (T3 M8); delete by filter / all stays without. Cost if wrong: one request shape.
- **Ruling 3b (reviewer, adopted):** the dry run waits while an edit dialog is open; S1 reads "400 ms after the last edit, and not while an edit dialog is open". Cost if wrong: none.
- **Done: T6** (sonnet, leaf): RED `485d8208`, implementation `65d328ef`; coordinator test fix (after a pending table rename, the comment label names the new table); merged `9365781d`. Implementer notes for the end-of-step review: T5's column form addresses ops with the base table ref, so column ops staged after a pending rename/move of the table name the old table (real bug); T6 added a dry-run pause while an edit dialog is open (`store.dialogs`, `useDryRun({ paused })`) — judge it.
- **Done: T4** (sonnet, leaf): RED `4cc4227b`, implementation `3ed877ac`; merged `1fcf04ae`.
- **Done: T5** (opus/opus): RED `4713c66d`, implementation `3528333f`; coordinator test fix `9e09e215` (plain fake timers); review: spec ❌, Changes needed (I1 empty `has_dependents` blocks silently, I2 staging during Apply, I3 acknowledgement keyed on ops only, I4 no re-read on Apply, I5 pending drop breaks the form preview; M1–M8); fix-round RED `5f420b42` (the Apply invariant), fix `d628e4c6` (verified: RED untouched, 372/372); merged `33dcc3dc` (coordinator resolved `panel.tsx`/`table-editor.tsx` conflicts with T4).
- **Done: T7** (sonnet, leaf): RED `d5504966`, implementation `a745485e`; merged `2c045f9f`.
- **Done: T8** (opus/opus): RED `2f631808`, implementation `eaf1ccd5`; coordinator test fix `b2094dcf` (the `writes` helper counted the count read); review: ✅ Approved (M1–M4); merged `dc4b562f`. Its live test runs in End of step 5.
- **Done: T1** (opus/opus): RED `f983ebae`, implementation `6a079cdc`; coordinator test fix `a427e38a` (the fingerprint test's allowlist replacer dropped nested keys); review: spec ✅, Changes needed (I1 generated constraint names not frozen at add time, I2 self-referencing FK targets not rewritten, M1–M4); fix-round RED `c4b75a75` (I1, I2, M1, M3, M4 and the invariant as property tests over constraints and draft tables), fix `5bc69146` (verified by the coordinator: RED files untouched, 57/57, typecheck clean); merged `7ce298ff`.
- **Done: T2** (sonnet/sonnet): RED `64430a09`, implementation `8fbc21ba`; coordinator fixture fix `2508824d` (the real API serves databases on `/v1/me` `tenant.projects`, `/v1/projects` returns bare `ProjectInfo`); review: spec ✅, Approved (I1, M1–M10); merged `6bdb1393`. Merged branch: 20 files, 154 tests, typecheck clean.
- **Done: T3** (opus/opus): RED `1a8df85f`, implementation `2df9d48f`; coordinator test fixes `defc6d3a` (paging waited for a row virtualization never mounts; unused helper; `capabilities.test.tsx` still looked for a Tables button after T2); review: spec ❌, Changes needed (C1 editor keyed by row index, I1 no strip hook, I2 stale rows across a role change, M1–M8); fix-round RED `aa89abf2` (C1, I1, I2, M4, D3), fix `c34b2692` (verified: RED untouched, full Studio 104 files/1265 green, typecheck clean); merged `4c709888`.
- **R6 (user, 2026-10-14):** filtered "select all matching" shows an exact count; the count lives in the query engine as a `count` op (apps get it too). New task T8 (opus/opus, wave 3); spec Addendum and plan T8 in `b8ec77bd`. Its live test runs in the end-of-step live pass.
- **R7 (coordinator):** T3 M4 (Rows view reset by the Structure tab) and D3 (spec §3 editors: date/time inputs, JSON in CodeMirror) fixed in T3's round because T4/T5 build on them; `GridLayout`/`above` pinned for T5. Cost if wrong: none.
- **R1 (coordinator):** T2's `store.ts` consumes T1's model; T1 was merged into T2 before T2's review. Cost if wrong: none (done).
- **R2 (coordinator):** `@tanstack/react-virtual` is 3.14.17 (pnpm 12 silently exempts younger releases from the release-age rule; reverted). Cost if wrong: a patch bump later.
- **R3 (coordinator):** T2 M5 (removing an edit leaves later edits that depend on it) is handled in T5 with a coordinator RED test. Cost if wrong: a store signature change in T5.
- **R4 (coordinator):** T1 M2 accepted: fold may reorder edits to free a name. Cost if wrong: the changes panel's numbering can shift on a fold.
- **R5 (coordinator):** `ProjectedConstraint` gains `references: { schema; table; columns } | null` (T1 fix round). Cost if wrong: an interface addition.
- Branch `table-editor`, spec `05ac7739`, plan `d43c8216`.

## Plan rulings (mechanical, listed for the user)

- **P1** A changeset `Edit` holds `ops: Operation[]` (one user action = one numbered edit; a column form with Unique/FK/Check yields `add_column` + `add_constraint`s). The spec's "an edit is a generated `Operation`" becomes "a list of them". Cost if wrong: renumbering only.
- **P2** Value defaults are quoted by the client (`sqlLiteral`: `'…'` with `'` doubled; Postgres casts the literal to the column type). TE-A takes default text as SQL, so the spec's "quoted by the renderer" was wrong about where. Cost if wrong: none for correctness; a typed literal style later.
- **P3** The table fingerprint is the canonical JSON (keys sorted) of the `getTable` response, compared as a string, not a SHA-256 of it (same equality, no async). Cost if wrong: none.
- **P4** "Delete selected" sends one console `batch` of `delete`s, each by `whereByKey` (works for composite keys; the old console's proven shape), not one `delete … in`. Cost if wrong: one request shape.
- **P5** Updates send `returning: "*"`, so the edited row is refreshed from the write itself.
- **P6** The rail link goes to `/app/tables`; the "last context" is applied by the route's resolution (`resolveParams` with `readLast`), not by the link. Cost: none.
- **P7** Projected marks are state-based (a column is `changed` when its projected name, not-null, default or comment differs from the base, or a type change is pending), so folding never changes what the strip shows. A pending type change is never folded away by equality (catalog names and `format_type` text differ).
- **P8** Pending changes for enums use the changeset key `…/{schema}.#types`; a draft new table `…/{schema}.#new`.
- **P9** `main` Postgres databases are addressed with `database: null` in every request (as the SQL editor and the old console do); others by name.
- **P10** The header context bar is portalled into the shell header's context slot (`#app-header-context`) by `HeaderContext`; without the slot (tests, other layouts) it renders inline.

## Task table (binding)

| Task | Scope | Implementer | Reviewer | Wave | Depends |
|---|---|---|---|---|---|
| T1 | Pure model: `changeset.ts`, `column-form.ts`, `selection.ts`, `dry-run.ts` + deps | opus | opus | 1 | — |
| T2 | `context.ts`, `access.ts`, `store.ts`, route, `TableEditor` shell, context bar, table list, header slot, rail item, project-page link | sonnet | sonnet | 1 | — |
| T3 | Rows view: grid (Table 9 + Virtual), paging, sort, filter, resize, keyboard, inline edit, selection bar and actions, data-API gate, page-size halving | opus | opus | 2 | T1, T2 |
| T4 | Row panel: fields, NULL, Now, FK Pick/Open, insert, insert with all defaults, delete, oversized rows and "Load value", field filter | sonnet | none (leaf; end-of-step review) | 3 | T3 |
| T5 | Column strip, column form, Structure view, `useDryRun`, changes panel and Apply, RLS banner, panel collapse | opus | opus | 3 | T1, T3 |
| T6 | Constraints, table operations, new table, Types/Enums view | sonnet | none (leaf; end-of-step review) | 4 | T5 |
| T7 | Delete the old console; docs page `using/table-editor.mdx`, `using/studio.mdx` update | sonnet | none (leaf; end-of-step review) | 4 | T2 |
| T8 | The `count` op (spec addendum, user R6): `chelabase-query`, console `write_kind`, OpenAPI + `openapi/data.json`, typed client, docs, the editor's filtered select-all | opus | opus | 3 | T3 |

Waves run in parallel when their files are disjoint (pre-flight scan), each task in its own worktree (`just wt-add table-editor-tN table-editor`), with at most 3 builds or test suites at once. One stack serves the whole plan (`.worktrees/table-editor`, up when the plan is written, purged after the final merge). No task runs live suites; the stack is used only by the end-of-step browser pass.

## Shared interfaces (pinned)

All paths below are under `apps/studio/src/`. Implementers write these names and types exactly.

### `lib/table-editor/changeset.ts` (T1)

```ts
import type { Operation, Table, TableRef, TypeSpec } from "@chelabase/management-client";

export interface Edit { id: string; ops: Operation[]; label: string }
export interface Changeset { key: string; base: string | null; edits: Edit[] }
export type Mark = "added" | "changed" | "dropped";
export interface ProjectedColumn {
  name: string;            // current name (a dropped base column keeps its base name)
  original: string | null; // base name; null for a column added in this changeset
  type_oid: number | null; // base type oid; null when added or when a type change is pending
  type_spec: TypeSpec | null; // the pending/added type; null when unchanged
  data_type: string;       // base data_type, or typeSpecText(type_spec)
  not_null: boolean;
  default: string | null;
  comment: string | null;
  mark: Mark | null;
}
export interface ProjectedConstraint {
  name: string;
  kind: "primary_key" | "unique" | "foreign_key" | "check" | "exclusion";
  columns: string[];       // column names
  definition: string | null; // base definition; null for a pending one
  validated: boolean;
  mark: Mark | null;
}
export interface ProjectedTable {
  schema: string; name: string; comment: string | null; rls_enabled: boolean; dropped: boolean;
  columns: ProjectedColumn[];      // base columns in attnum order (dropped stay in place), then added ones in add order
  constraints: ProjectedConstraint[];
}
export function changesetKey(c: { databaseId: string; branchId: string; pgDatabase: string; schema: string; table: string }): string; // `${databaseId}/${branchId}/${pgDatabase}/${schema}.${table}`
export function typeSpecText(t: TypeSpec): string;       // "int8", "public.invoice_status", "text[]" (only pg_catalog is omitted)
export function editLabel(op: Operation): string;
export function newEdit(ops: Operation[]): Edit;          // id from crypto.randomUUID(), label = editLabel(ops[0])
export function opsOf(edits: Edit[]): Operation[];
export function editIndexOfOp(edits: Edit[], opIndex: number): number; // -1 when out of range
export function fold(base: Table | null, edits: Edit[], next: Edit): Edit[];
export function foldAll(base: Table | null, edits: Edit[]): Edit[];
export function project(base: Table | null, edits: Edit[]): ProjectedTable; // base null: the first create_table op gives the table
export function tableFingerprint(t: Table): string;       // canonical JSON, keys sorted recursively
```

Fold rules (spec §2, P7): F1 an op on a column added in this changeset rewrites its `add_column` (alter fields into the spec, rename into `spec.name`, comment into `spec.comment`, default `set`/`drop` into `spec.default`); F2 dropping such a column removes its `add_column` and every pending op naming it (including `add_constraint`s whose columns include it); F3 `alter_column`s on one base column merge (later fields win); a merged change with no `data_type` whose not-null, default and comment all equal the base disappears; F4 renames chain (`a→b`, `b→c` = `a→c`; back to `a` = nothing); ops after a rename name the new name; F5 dropping a base column removes its pending alters and rename and emits `drop_column` under the base name; F6 with a pending `create_table`, column ops rewrite its `columns` (and `primary_key`); edits left with no ops are removed; a rewritten edit keeps its position and gets `label = editLabel(ops[0])`.

Labels: `add_column` → `Add column ${name} ${typeSpecText}`; `alter_column` → `Change column ${column}`; `rename_column` → `Rename column ${column} → ${to}`; `drop_column` → `Drop column ${column}`; `add_constraint` → `Add ${kind} on ${table.name}` with kind `primary key | unique constraint | foreign key | check constraint | exclusion constraint`; `validate_constraint` → `Validate ${name}`; `rename_constraint` → `Rename constraint ${name} → ${to}`; `drop_constraint` → `Drop constraint ${name}`; `set_rls` → `Turn RLS on` / `Turn RLS off`; `create_table` → `Create table ${schema}.${name}`; `rename_table` → `Rename table ${name} → ${to}`; `set_table_schema` → `Move table ${name} to ${schema}`; `set_table_comment` → `Change comment on ${name}`; `drop_table` → `Drop table ${schema}.${name}`; `create_enum` → `Create enum ${schema}.${name}`; `add_enum_value` → `Add value ${value} to ${name}`; `rename_enum_value` → `Rename value ${from} → ${to} in ${name}`; `rename_enum` → `Rename enum ${name} → ${to}`; `drop_enum` → `Drop enum ${schema}.${name}`; anything else → the op's key.

### `lib/table-editor/column-form.ts` (T1)

```ts
import type { FkAction, Operation, TableRef, TypeEntry } from "@chelabase/management-client";
import type { ProjectedColumn, ProjectedTable } from "./changeset";
export interface ColumnForm {
  name: string;
  type: { schema: string; name: string } | null;
  array: boolean;
  defaultKind: "none" | "value" | "expression";
  defaultText: string;
  notNull: boolean;
  unique: boolean;
  primaryKey: boolean;
  references: { schema: string; table: string; column: string; onDelete: FkAction | null } | null;
  check: string;
  comment: string;
}
export interface FieldError { field: "name" | "type" | "defaultText" | "check" | "references"; message: string }
export function emptyColumnForm(): ColumnForm; // defaultKind "none", everything empty/false/null
export function formFromColumn(col: ProjectedColumn, table: ProjectedTable, catalog: TypeEntry[]): ColumnForm;
export function validateColumnForm(form: ColumnForm, ctx: { table: ProjectedTable; editing: string | null; catalog: TypeEntry[] }): FieldError[];
export function columnFormOps(form: ColumnForm, ctx: { table: TableRef; editing: ProjectedColumn | null; projected: ProjectedTable }): Operation[];
export function sqlLiteral(text: string): string;
```

Rules: a new column → `add_column` then, in order, `add_constraint` primary key, unique, foreign key, check. An existing column → one `alter_column` with only changed fields (`data_type` when type/array changed, `not_null`, `default` `{set}`/`"drop"`, `comment` `{set}`/`"clear"`), then `rename_column` when the name changed, then `add_constraint`/`drop_constraint` for Unique/Primary key toggles against existing single-column constraints. Default: none → no default (`"drop"` when editing one that had a default); value → `sqlLiteral(defaultText)`; expression → `defaultText` as is. `formFromColumn` finds the type by `type_oid` in the catalog (an entry whose `array_oid` matches means `array: true`) or from `type_spec`; a base default is shown as an expression. Validation messages: name empty → `Name the column`; over 63 bytes (UTF-8) → `Postgres names are at most 63 bytes`; taken → `${name} is already a column`; no type → `Pick a type`; expression default empty → `Write the default expression`; references incomplete → `Pick the referenced table and column`.

### `lib/table-editor/selection.ts` (T1)

```ts
export type Selection = { kind: "none" } | { kind: "rows"; keys: string[] } | { kind: "all_matching"; estimate: number | null };
export interface SelectionState { view: string; selection: Selection }
export type SelectionAction =
  | { type: "view"; view: string }          // a different view resets to none
  | { type: "toggle"; key: string }         // in all_matching: back to none
  | { type: "page"; keys: string[] }        // all on page; when all already selected: none
  | { type: "all_matching"; estimate: number | null }
  | { type: "clear" };
export const NO_SELECTION: SelectionState; // { view: "", selection: { kind: "none" } }
export function reduceSelection(s: SelectionState, a: SelectionAction): SelectionState;
export function rowKey(pk: string[], row: Record<string, unknown>): string; // JSON.stringify(pk.map((c) => row[c] ?? null))
export function viewKey(c: { changeset: string; filter: unknown; sort: unknown }): string; // JSON.stringify([changeset, filter ?? null, sort ?? null])
```

### `lib/table-editor/dry-run.ts` (T1)

```ts
import type { ApiError, Dependents, LockLevel, SchemaChangeResponse, SkipReason } from "@chelabase/management-client";
import type { Edit } from "./changeset";
export type EditStatus =
  | { kind: "pending" } | { kind: "ok" } | { kind: "rendered_only"; reason: SkipReason }
  | { kind: "error"; message: string; detail: string | null; hint: string | null };
export interface StatementView { sql: string; edit: number; lock: LockLevel; executed: boolean; rewrites: boolean; skipped: SkipReason | null; notices: string[] }
export interface DryRunView { statements: StatementView[]; perEdit: EditStatus[]; warnings: string[]; dependents: Dependents | null; apply: "ready" | "needs_ack" | "blocked" }
export interface ChangeFailure { edit: number | null; status: EditStatus; code: string; retryable: boolean; dependents: Dependents | null }
export function mapDryRun(edits: Edit[], res: SchemaChangeResponse, ctx: { table: string; totalBytes: number | null }): DryRunView;
export function mapChangeError(edits: Edit[], err: ApiError): ChangeFailure;
export function lockText(l: LockLevel): string;
export function skipText(r: SkipReason): string;
export function formatBytes(n: number): string;
```

### `lib/table-editor/context.ts` and `access.ts` (T2)

```ts
// context.ts
import type { Branch, BranchDatabaseView, ObjectSummary, Project } from "@chelabase/management-client";
export interface EditorParams { projectId?: string; databaseId?: string; branchId?: string; pgDatabase?: string; schema?: string; table?: string }
export const LEVELS: readonly (keyof EditorParams)[]; // ["projectId","databaseId","branchId","pgDatabase","schema","table"]
export const LAST_KEY = "chelabase.studio.tables.last";
export function editorPath(p: EditorParams): string; // "/app/tables" + each present level up to the first missing one, encodeURIComponent'd
export function readLast(storage: Storage | null): EditorParams | null;
export function writeLast(storage: Storage | null, p: EditorParams): void;
export interface Choices { projects?: Project[]; branches?: Branch[]; pgDatabases?: BranchDatabaseView[]; objects?: ObjectSummary[] }
export type Resolution =
  | { kind: "loading" }
  | { kind: "redirect"; params: EditorParams }
  | { kind: "ready"; params: EditorParams }
  | { kind: "not_found"; level: "project" | "database" | "branch" | "pgDatabase" | "schema" | "table" };
export function resolveParams(p: EditorParams, last: EditorParams | null, c: Choices): Resolution;
export function branchTree(branches: Branch[]): { branch: Branch; depth: number }[];
export function schemasOf(objects: ObjectSummary[]): string[]; // distinct, sorted
// access.ts
import type { Branch, BranchDatabaseView } from "@chelabase/management-client";
import type { ProjectCaps } from "@/lib/capabilities";
export type StructureAccess = { allowed: true } | { allowed: false; reason: string };
export function structureAccess(caps: ProjectCaps, branch: Branch): StructureAccess;
export interface RowAccess { read: boolean; write: boolean; reason: string | null }
export function rowAccess(caps: ProjectCaps, actAs: { owner: boolean; roles: string[] }, db: BranchDatabaseView): RowAccess;
```

`resolveParams` fills missing levels in order while it has the data for that level, then stops: project = `last.projectId` when listed, else the first project; database = the project's first `databases[]`; branch = the root (`parent_branch_id === null`, `kind: "normal"`, `status: "ready"`); pgDatabase = the `main` one; schema = `public` when `schemasOf(objects)` has it, else the first. Never a table. A given id missing from loaded data → `not_found` (checked level by level, before filling). Filled anything → `redirect`; needs data it lacks and filled nothing → `loading`; else `ready`. `last` is only used when `projectId` is missing. `branchTree`: `kind: "normal"` and `status: "ready"` only, depth-first from roots, siblings by name. Reasons: protected and below Admin → `${branch.name} is protected: schema changes need Admin`; Viewer → `Viewers can't change the schema`; no role → `You don't have access to this project`; data API off → `Rows need the data API on ${db.name}`; no role to act as → `You can't act as any Postgres role here.`

### `lib/table-editor/store.ts` (T2)

```ts
import type { StoreApi } from "zustand";
import type { Table } from "@chelabase/management-client";
import type { Changeset, Edit } from "./changeset";
import type { SelectionAction, SelectionState } from "./selection";
export type PanelKind = "closed" | "changes" | "column" | "row";
export interface PanelState { kind: PanelKind; collapsed: boolean; column: string | null; row: string | null }
export interface TableEditorState {
  changesets: Record<string, Changeset>;
  panel: PanelState;
  actAs: Record<string, string | null>; // key `${databaseId}/${branchId}`; null = the owner
  selection: SelectionState;
  addEdit(key: string, base: Table | null, edit: Edit): void;  // creates the changeset with base = tableFingerprint(base); folds; drops it when empty
  removeEdit(key: string, id: string): void;
  discard(key: string): void;
  rebase(key: string, base: Table): void;                     // after the user reviewed a changed table
  openPanel(kind: PanelKind, target?: { column?: string | null; row?: string | null }): void; // also un-collapses
  setCollapsed(collapsed: boolean): void;
  setActAs(branchKey: string, role: string | null): void;
  select(action: SelectionAction): void;
}
export const DRAFTS_KEY = "chelabase.studio.tables.drafts";
export function createTableEditorStore(storage: Storage | null): StoreApi<TableEditorState>; // loads drafts; saves non-empty changesets on every change; storage errors ignored
export const TableEditorStoreContext: React.Context<StoreApi<TableEditorState> | null>;
export function useTableEditor<T>(selector: (s: TableEditorState) => T): T;
```

### Components (T2 → T6)

- `components/table-editor/table-editor.tsx` (T2): `export function TableEditor(props: { params: EditorParams; navigate: (p: EditorParams, opts?: { replace?: boolean }) => void }): JSX.Element`. Creates the store once (`sessionStorage`), resolves params (navigates `{ replace: true }` on `redirect`), writes the last context when ready, renders `HeaderContext` > `ContextBar`, `TableList`, and the main area (T3 `RowsView`, T5 strip/Structure/panel, T6 new-table/Types views).
- `components/app-shell/header-context.tsx` (T2): `export function HeaderContext({ children }: { children: ReactNode })`; `app-header.tsx`'s slot gets `id="app-header-context"`.
- `routes/app.tables.{-$projectId}.{-$databaseId}.{-$branchId}.{-$pgDatabase}.{-$schema}.{-$table}.tsx` (T2): reads params, renders `<TableEditor params navigate>`; `navigate` calls the router with `editorPath(p)`.
- Accessible names and copy the tests use are written in each task's RED tests; implementers keep them verbatim.

## Pre-flight scan (2026-10-14)

- **Files per wave (disjoint):** wave 1: T1 `lib/table-editor/{changeset,column-form,selection,dry-run}.ts` + `package.json`/`pnpm-lock.yaml` (fast-check, table, virtual); T2 `lib/table-editor/{context,access,store}.ts`, `components/table-editor/{table-editor,context-bar,table-list}.tsx`, `components/app-shell/{header-context.tsx,app-header.tsx,rail-items.ts}`, the route file, `routeTree.gen.ts`, `components/project-page.tsx`, `test/table-editor-fixtures.tsx`. T2 imports only T1's types → T2 compiles against T1's files only after the wave merges; T2's tests don't import T1 modules. **Both touch nothing in common** (T1 owns `package.json`; T2 adds no dependency).
- Wave 3: T4 `components/table-editor/row-panel*.tsx`, `lib/table-editor/oversized.ts`; T5 `components/table-editor/{column-strip,column-form,structure-view,changes-panel,rls-banner,panel}.tsx`, `use-dry-run.ts`. Both mount into the main area: T3 leaves two mount points in `table-editor.tsx` (`{panel}` slot and `{structure}` slot, see T3 Files) so T4 and T5 don't edit the same lines; the coordinator resolves the merge of `table-editor.tsx` if both touch it.
- Wave 4: T6 `components/table-editor/{constraints-section,table-actions,new-table,types-view}.tsx`; T7 deletes `components/{tables-view,console-grid,row-editor}.tsx(+tests)`, edits `apps/docs/content/docs/using/{studio.mdx,meta.json}`, adds `table-editor.mdx`. Disjoint.
- **Existing tests changed by the coordinator in T2's RED commit:** `components/app-shell/rail-items.test.ts` (entries: Projects, Table editor, API reference, Members; no placeholder), `components/app-shell/app-rail.test.tsx` ("placeholders are aria-disabled…" becomes "no placeholder is left"), `components/project-page.test.tsx` ("opens the Tables console" becomes a link check). `console-filters.tsx` is used only by `tables-view.tsx`; T3 decides whether to reuse it (it is deleted in T7 if unused).
- **Reused helpers:** `lib/console` (`buildSelect`, `whereByKey`, `buildFilterTree`, `inputKind`, `parseValue`, `pageSizeFor`, `PAGE_SIZE = 50`, `consoleErrorText`), `lib/capabilities` (`projectCaps`, `actAsRoles`), `components/ui/{combobox,menu,dialog,confirm-dialog,switch,tabs,button}`, `rls-warning.tsx`, `new-branch-form.tsx`, `useEnableDataApi` (`POST …/pg-databases/{name}/data-api`).
- **Traps:** jsdom has no layout: `@tanstack/react-virtual` renders no rows unless sizes are stubbed; the fixture's `stubLayout()` stubs `getBoundingClientRect`, `offsetWidth/Height` and `ResizeObserver` (T3's implementer reports a deviation if the virtualizer still renders nothing, and the coordinator fixes the helper). The console's 413 code is `too_large`. TE-A errors carry `extra.op_index`, `extra.detail`, `extra.hint`; `dependents_changed` carries `extra.dependents`. `BranchKind` has `normal | backup | rehearsal`. `getTable` returns `Table` (no `editable`/`type_ref` fields; editability comes from the object list's `editable`).

## Review focus (no RED test pins these; reviewers and the browser pass check them)

- After Apply, changed columns and rows keep a seafoam edge-mark until the user moves past them (spec §2).
- Column widths are remembered per table in `localStorage` (`chelabase.studio.tables.widths`) and survive a reload; a broken storage falls back to defaults.
- Above 40 columns the grid virtualizes columns too, and the strip stays aligned with the grid while scrolling sideways.
- With any non-empty changeset, leaving the page asks first (`beforeunload`).
- FK `Open {column}` goes to the referenced table; Ctrl/Cmd+C on selected cells copies TSV.

---

## T1: the pure model (opus / opus)

**Files:** create `apps/studio/src/lib/table-editor/{changeset,column-form,selection,dry-run}.ts`; tests `apps/studio/src/lib/table-editor/{changeset,column-form,selection,dry-run}.test.ts` (coordinator). Add `fast-check@4.10.2` (dev), `@tanstack/react-table@9.3.1` and `@tanstack/react-virtual@3.14.19` (deps) to `apps/studio/package.json` with `pnpm --filter @chelabase/studio add`.

**Behaviour:** spec §2; the pinned interfaces and rules above.

**RED tests (coordinator):**

`apps/studio/src/lib/table-editor/test-tables.ts` (shared by T1 tests; coordinator):

```ts
import type { Table } from "@chelabase/management-client";

const col = (attnum: number, name: string, data_type: string, type_oid: number, over: Record<string, unknown> = {}) => ({
  attnum, name, data_type, type_oid, not_null: false, default: null, comment: null, collation: null, generated: null, identity: null, grants: [], ...over,
});

/** `public.invoices`: id uuid PK, customer_id FK, status enum, amount_cents, memo. */
export function invoices(over: Partial<Table> = {}): Table {
  return {
    schema: "public", name: "invoices", oid: 16600, owner: { name: "app_owner", oid: 10 }, comment: null,
    rls_enabled: true, rls_forced: false, unlogged: false, options: [], grants: [], indexes: [], policies: [],
    columns: [
      col(1, "id", "uuid", 2950, { not_null: true, default: "gen_random_uuid()" }),
      col(2, "customer_id", "uuid", 2950, { not_null: true }),
      col(3, "status", "invoice_status", 16500, { not_null: true, default: "'open'::invoice_status" }),
      col(4, "amount_cents", "bigint", 20, { not_null: true }),
      col(5, "memo", "text", 25),
    ],
    constraints: [
      { name: "invoices_pkey", kind: "primary_key", columns: [1], definition: "PRIMARY KEY (id)", deferrable: false, initially_deferred: false, oid: 1, references: null, validated: true },
      { name: "invoices_customer_id_fkey", kind: "foreign_key", columns: [2], definition: "FOREIGN KEY (customer_id) REFERENCES customers(id)", deferrable: false, initially_deferred: false, oid: 2, references: { schema: "public", table: "customers", table_oid: 16610, columns: [1] }, validated: true },
    ],
    ...over,
  } as unknown as Table;
}

export const CATALOG = [
  { oid: 20, array_oid: 1016, schema: "pg_catalog", name: "int8", kind: "base", category: "N", preferred: false, takes_modifiers: false },
  { oid: 25, array_oid: 1009, schema: "pg_catalog", name: "text", kind: "base", category: "S", preferred: true, takes_modifiers: false },
  { oid: 16, array_oid: 1000, schema: "pg_catalog", name: "bool", kind: "base", category: "B", preferred: true, takes_modifiers: false },
  { oid: 2950, array_oid: 2951, schema: "pg_catalog", name: "uuid", kind: "base", category: "U", preferred: false, takes_modifiers: false },
  { oid: 1184, array_oid: 1185, schema: "pg_catalog", name: "timestamptz", kind: "base", category: "D", preferred: true, takes_modifiers: true },
  { oid: 16500, array_oid: 16499, schema: "public", name: "invoice_status", kind: "enum", category: "E", preferred: false, takes_modifiers: false, labels: ["open", "paid", "void"] },
] as const;
```

`apps/studio/src/lib/table-editor/changeset.test.ts`:

```ts
import { describe, expect, it } from "vitest";
import fc from "fast-check";
import type { Operation, TypeSpec } from "@chelabase/management-client";
import {
  changesetKey, editIndexOfOp, editLabel, fold, foldAll, newEdit, opsOf, project, tableFingerprint, typeSpecText,
  type Edit, type ProjectedTable,
} from "./changeset";
import { invoices } from "./test-tables";

const T = { schema: "public", name: "invoices" };
const INT8: TypeSpec = { schema: "pg_catalog", name: "int8" };
const TEXT: TypeSpec = { schema: "pg_catalog", name: "text" };
const add = (name: string, data_type: TypeSpec = INT8, over: Record<string, unknown> = {}): Operation => ({ add_column: { table: T, column: { name, data_type, ...over } } });
const alter = (column: string, change: Record<string, unknown>): Operation => ({ alter_column: { table: T, column, change } });
const rename = (column: string, to: string): Operation => ({ rename_column: { table: T, column, to } });
const drop = (column: string): Operation => ({ drop_column: { table: T, column } });
const e = (...ops: Operation[]): Edit => newEdit(ops);
const opsAfter = (...steps: Operation[][]) => opsOf(steps.reduce<Edit[]>((acc, ops) => fold(invoices(), acc, e(...ops)), []));
const col = (p: ProjectedTable, name: string) => p.columns.find((c) => c.name === name);

describe("labels and keys", () => {
  it("changeset_key_joins_the_context", () => {
    expect(changesetKey({ databaseId: "d1", branchId: "b1", pgDatabase: "app", schema: "public", table: "invoices" })).toBe("d1/b1/app/public.invoices");
  });
  it("type_spec_text_omits_only_pg_catalog", () => {
    expect(typeSpecText(INT8)).toBe("int8");
    expect(typeSpecText({ schema: "public", name: "invoice_status" })).toBe("public.invoice_status");
    expect(typeSpecText({ ...TEXT, array: true })).toBe("text[]");
  });
  it("edit_labels", () => {
    expect(editLabel(add("discount_cents"))).toBe("Add column discount_cents int8");
    expect(editLabel(alter("status", { not_null: false }))).toBe("Change column status");
    expect(editLabel(rename("memo", "note"))).toBe("Rename column memo → note");
    expect(editLabel(drop("memo"))).toBe("Drop column memo");
    expect(editLabel({ set_rls: { table: T, enabled: true, forced: false } })).toBe("Turn RLS on");
    expect(editLabel({ add_constraint: { table: T, constraint: { kind: { unique: { columns: ["memo"] } } } } })).toBe("Add unique constraint on invoices");
    expect(editLabel({ drop_table: { table: T } })).toBe("Drop table public.invoices");
    expect(editLabel({ create_enum: { schema: "public", name: "tier", values: ["a"] } })).toBe("Create enum public.tier");
    expect(newEdit([add("x"), { add_constraint: { table: T, constraint: { kind: { unique: { columns: ["x"] } } } } }]).label).toBe("Add column x int8");
  });
  it("edit_index_of_op_maps_flattened_indexes", () => {
    const edits = [e(add("a"), add("b")), e(drop("memo"))];
    expect(opsOf(edits)).toHaveLength(3);
    expect(editIndexOfOp(edits, 0)).toBe(0);
    expect(editIndexOfOp(edits, 1)).toBe(0);
    expect(editIndexOfOp(edits, 2)).toBe(1);
    expect(editIndexOfOp(edits, 3)).toBe(-1);
  });
  it("fingerprint_ignores_key_order", () => {
    const a = invoices();
    const b = JSON.parse(JSON.stringify(a, Object.keys(a).reverse()));
    expect(tableFingerprint(b)).toBe(tableFingerprint(a));
    expect(tableFingerprint(invoices({ comment: "x" }))).not.toBe(tableFingerprint(a));
  });
});

describe("fold examples", () => {
  it("F1_edits_to_an_added_column_rewrite_its_add", () => {
    expect(opsAfter([add("d")], [alter("d", { not_null: true, default: { set: "0" } })], [rename("d", "discount")])).toEqual([
      add("discount", INT8, { not_null: true, default: "0" }),
    ]);
  });
  it("F2_dropping_an_added_column_removes_it_and_its_constraints", () => {
    const uniq: Operation = { add_constraint: { table: T, constraint: { kind: { unique: { columns: ["d"] } } } } };
    expect(opsAfter([add("d"), uniq], [drop("d")])).toEqual([]);
  });
  it("F3_alters_merge_and_vanish_when_back_to_base", () => {
    expect(opsAfter([alter("memo", { not_null: true })], [alter("memo", { comment: { set: "c" } })])).toEqual([
      alter("memo", { not_null: true, comment: { set: "c" } }),
    ]);
    expect(opsAfter([alter("memo", { not_null: true })], [alter("memo", { not_null: false })])).toEqual([]);
  });
  it("F3_a_type_change_is_never_folded_away", () => {
    expect(opsAfter([alter("memo", { data_type: INT8 })], [alter("memo", { data_type: TEXT })])).toEqual([alter("memo", { data_type: TEXT })]);
  });
  it("F4_renames_chain_and_cancel", () => {
    expect(opsAfter([rename("memo", "note")], [rename("note", "remark")])).toEqual([rename("memo", "remark")]);
    expect(opsAfter([rename("memo", "note")], [rename("note", "memo")])).toEqual([]);
  });
  it("F5_dropping_a_renamed_base_column_drops_it_by_its_base_name", () => {
    expect(opsAfter([rename("memo", "note")], [alter("note", { not_null: true })], [drop("note")])).toEqual([drop("memo")]);
  });
  it("F6_column_ops_rewrite_a_pending_create_table", () => {
    const create: Operation = { create_table: { table: { schema: "public", name: "payments" }, columns: [{ name: "id", data_type: INT8, not_null: true }], primary_key: ["id"] } };
    const P = { schema: "public", name: "payments" };
    const steps = [e(create), e({ add_column: { table: P, column: { name: "note", data_type: TEXT } } }), e({ rename_column: { table: P, column: "note", to: "memo" } })];
    const folded = steps.reduce<Edit[]>((acc, s) => fold(null, acc, s), []);
    expect(opsOf(folded)).toEqual([
      { create_table: { table: P, columns: [{ name: "id", data_type: INT8, not_null: true }, { name: "memo", data_type: TEXT }], primary_key: ["id"] } },
    ]);
    expect(folded[0].label).toBe("Create table public.payments");
  });
  it("a_rewritten_edit_keeps_its_position", () => {
    const folded = [e(drop("memo")), e(add("d")), e(alter("amount_cents", { not_null: false }))].reduce<Edit[]>((acc, x) => fold(invoices(), acc, x), []);
    const again = fold(invoices(), folded, e(rename("d", "discount")));
    expect(again.map((x) => x.label)).toEqual(["Drop column memo", "Add column discount int8", "Change column amount_cents"]);
  });
});

describe("project", () => {
  it("marks_added_changed_dropped_and_keeps_order", () => {
    const p = project(invoices(), [e(add("discount_cents")), e(rename("memo", "note")), e(drop("customer_id"))]);
    expect(p.columns.map((c) => [c.name, c.mark])).toEqual([
      ["id", null], ["customer_id", "dropped"], ["status", null], ["amount_cents", null], ["note", "changed"], ["discount_cents", "added"],
    ]);
    expect(col(p, "note")?.original).toBe("memo");
    expect(col(p, "discount_cents")).toMatchObject({ original: null, data_type: "int8", type_spec: INT8, type_oid: null });
  });
  it("constraints_carry_names_and_pending_marks", () => {
    const p = project(invoices(), [e({ add_constraint: { table: T, constraint: { name: "memo_key", kind: { unique: { columns: ["memo"] } } } } }), e({ drop_constraint: { table: T, name: "invoices_customer_id_fkey" } })]);
    expect(p.constraints.map((c) => [c.name, c.kind, c.columns, c.mark])).toEqual([
      ["invoices_pkey", "primary_key", ["id"], null],
      ["invoices_customer_id_fkey", "foreign_key", ["customer_id"], "dropped"],
      ["memo_key", "unique", ["memo"], "added"],
    ]);
  });
  it("rls_and_draft_tables", () => {
    expect(project(invoices({ rls_enabled: false }), [e({ set_rls: { table: T, enabled: true, forced: false } })]).rls_enabled).toBe(true);
    const draft = project(null, [e({ create_table: { table: { schema: "public", name: "payments" }, columns: [{ name: "id", data_type: INT8 }] } })]);
    expect(draft).toMatchObject({ schema: "public", name: "payments" });
    expect(draft.columns.map((c) => [c.name, c.mark])).toEqual([["id", "added"]]);
  });
});

// ---- invariants (property tests) ----
const POOL = ["a", "b", "c", "x", "id", "memo", "status"];
const DEFAULTS = [null, "0", "'x'"];
const COMMENTS = [null, "c1", "c2"];
const TYPES = [INT8, TEXT, { schema: "pg_catalog", name: "bool" }];
type Intent =
  | { k: "add"; n: number } | { k: "drop"; i: number } | { k: "rename"; i: number; to: number }
  | { k: "notnull"; i: number; v: boolean } | { k: "default"; i: number; v: number } | { k: "comment"; i: number; v: number } | { k: "type"; i: number; t: number };
const intent: fc.Arbitrary<Intent> = fc.oneof(
  fc.record({ k: fc.constant("add" as const), n: fc.nat(6) }),
  fc.record({ k: fc.constant("drop" as const), i: fc.nat(9) }),
  fc.record({ k: fc.constant("rename" as const), i: fc.nat(9), to: fc.nat(6) }),
  fc.record({ k: fc.constant("notnull" as const), i: fc.nat(9), v: fc.boolean() }),
  fc.record({ k: fc.constant("default" as const), i: fc.nat(9), v: fc.nat(2) }),
  fc.record({ k: fc.constant("comment" as const), i: fc.nat(9), v: fc.nat(2) }),
  fc.record({ k: fc.constant("type" as const), i: fc.nat(9), t: fc.nat(2) }),
);
/** Turns intents into valid raw edits against the table as it stands after the previous ones. */
function rawEdits(intents: Intent[]): Edit[] {
  const base = invoices();
  const out: Edit[] = [];
  for (const it of intents) {
    const live = project(base, out).columns.filter((c) => c.mark !== "dropped");
    const names = new Set(live.map((c) => c.name));
    const target = live.length ? live[("i" in it ? it.i : 0) % live.length].name : null;
    let op: Operation | null = null;
    if (it.k === "add") op = names.has(POOL[it.n]) ? null : add(POOL[it.n]);
    else if (!target) op = null;
    else if (it.k === "drop") op = drop(target);
    else if (it.k === "rename") op = names.has(POOL[it.to]) ? null : rename(target, POOL[it.to]);
    else if (it.k === "notnull") op = alter(target, { not_null: it.v });
    else if (it.k === "default") op = alter(target, { default: DEFAULTS[it.v] === null ? "drop" : { set: DEFAULTS[it.v] } });
    else if (it.k === "comment") op = alter(target, { comment: COMMENTS[it.v] === null ? "clear" : { set: COMMENTS[it.v] } });
    else op = alter(target, { data_type: TYPES[it.t] });
    if (op) out.push(e(op));
  }
  return out;
}
const RUNS = { numRuns: 400 };

describe("fold invariants", () => {
  it("folded_has_the_same_effect_as_raw", () => {
    fc.assert(fc.property(fc.array(intent, { maxLength: 14 }), (intents) => {
      const raw = rawEdits(intents);
      expect(project(invoices(), foldAll(invoices(), raw))).toEqual(project(invoices(), raw));
    }), RUNS);
  });
  it("fold_is_idempotent", () => {
    fc.assert(fc.property(fc.array(intent, { maxLength: 14 }), (intents) => {
      const once = foldAll(invoices(), rawEdits(intents));
      expect(opsOf(foldAll(invoices(), once))).toEqual(opsOf(once));
    }), RUNS);
  });
  it("folded_is_minimal", () => {
    fc.assert(fc.property(fc.array(intent, { maxLength: 14 }), (intents) => {
      const ops = opsOf(foldAll(invoices(), rawEdits(intents)));
      const added = new Set<string>();
      const altered = new Set<string>();
      const renamed = new Set<string>();
      for (const op of ops) {
        if ("add_column" in op) added.add(op.add_column.column.name);
        const c = "alter_column" in op ? op.alter_column.column : "rename_column" in op ? op.rename_column.column : "drop_column" in op ? op.drop_column.column : null;
        if (c !== null) expect(added.has(c)).toBe(false);
        if ("alter_column" in op) { expect(altered.has(c!)).toBe(false); altered.add(c!); }
        if ("rename_column" in op) { expect(renamed.has(c!)).toBe(false); renamed.add(c!); }
      }
    }), RUNS);
  });
});
```

`apps/studio/src/lib/table-editor/column-form.test.ts`:

```ts
import { describe, expect, it } from "vitest";
import type { TypeEntry } from "@chelabase/management-client";
import { newEdit, project } from "./changeset";
import { columnFormOps, emptyColumnForm, formFromColumn, sqlLiteral, validateColumnForm, type ColumnForm } from "./column-form";
import { CATALOG, invoices } from "./test-tables";

const catalog = CATALOG as unknown as TypeEntry[];
const T = { schema: "public", name: "invoices" };
const p = project(invoices(), []);
const form = (over: Partial<ColumnForm>): ColumnForm => ({ ...emptyColumnForm(), ...over });
const col = (name: string) => p.columns.find((c) => c.name === name)!;

describe("column form", () => {
  it("sql_literal_doubles_quotes", () => {
    expect(sqlLiteral("it's")).toBe("'it''s'");
    expect(sqlLiteral("0")).toBe("'0'");
  });
  it("validation_messages", () => {
    const v = (f: Partial<ColumnForm>, editing: string | null = null) => validateColumnForm(form(f), { table: p, editing, catalog }).map((x) => [x.field, x.message]);
    expect(v({})).toEqual([["name", "Name the column"], ["type", "Pick a type"]]);
    expect(v({ name: "x".repeat(64), type: { schema: "pg_catalog", name: "int8" } })).toEqual([["name", "Postgres names are at most 63 bytes"]]);
    expect(v({ name: "é".repeat(32), type: { schema: "pg_catalog", name: "int8" } })).toEqual([["name", "Postgres names are at most 63 bytes"]]);
    expect(v({ name: "memo", type: { schema: "pg_catalog", name: "text" } })).toEqual([["name", "memo is already a column"]]);
    expect(v({ name: "memo", type: { schema: "pg_catalog", name: "text" } }, "memo")).toEqual([]);
    expect(v({ name: "d", type: { schema: "pg_catalog", name: "int8" }, defaultKind: "expression", defaultText: "" })).toEqual([["defaultText", "Write the default expression"]]);
    expect(v({ name: "d", type: { schema: "pg_catalog", name: "int8" }, references: { schema: "public", table: "", column: "", onDelete: null } })).toEqual([["references", "Pick the referenced table and column"]]);
  });
  it("new_column_emits_add_then_constraints_in_order", () => {
    const ops = columnFormOps(
      form({ name: "code", type: { schema: "pg_catalog", name: "text" }, defaultKind: "value", defaultText: "it's", notNull: true, unique: true, check: "length(code) > 2", references: { schema: "public", table: "customers", column: "code", onDelete: "cascade" } }),
      { table: T, editing: null, projected: p },
    );
    expect(ops).toEqual([
      { add_column: { table: T, column: { name: "code", data_type: { schema: "pg_catalog", name: "text", array: false }, not_null: true, default: "'it''s'", comment: null } } },
      { add_constraint: { table: T, constraint: { kind: { unique: { columns: ["code"] } } } } },
      { add_constraint: { table: T, constraint: { kind: { foreign_key: { columns: ["code"], references: { schema: "public", name: "customers" }, referenced_columns: ["code"], on_delete: "cascade" } } } } },
      { add_constraint: { table: T, constraint: { kind: { check: { expression: "length(code) > 2" } } } } },
    ]);
  });
  it("existing_column_emits_only_changed_fields_then_rename", () => {
    const f = formFromColumn(col("memo"), p, catalog);
    expect(f).toMatchObject({ name: "memo", type: { schema: "pg_catalog", name: "text" }, array: false, defaultKind: "none", notNull: false, unique: false, primaryKey: false });
    expect(columnFormOps({ ...f, notNull: true, name: "note", comment: "free text" }, { table: T, editing: col("memo"), projected: p })).toEqual([
      { alter_column: { table: T, column: "memo", change: { not_null: true, comment: { set: "free text" } } } },
      { rename_column: { table: T, column: "memo", to: "note" } },
    ]);
    expect(columnFormOps(f, { table: T, editing: col("memo"), projected: p })).toEqual([]);
  });
  it("existing_default_shows_as_expression_and_can_be_dropped", () => {
    const f = formFromColumn(col("status"), p, catalog);
    expect(f).toMatchObject({ type: { schema: "public", name: "invoice_status" }, defaultKind: "expression", defaultText: "'open'::invoice_status", notNull: true });
    expect(columnFormOps({ ...f, defaultKind: "none" }, { table: T, editing: col("status"), projected: p })).toEqual([
      { alter_column: { table: T, column: "status", change: { default: "drop" } } },
    ]);
  });
  it("type_and_unique_toggles_on_an_existing_column", () => {
    const f = formFromColumn(col("amount_cents"), p, catalog);
    expect(f.type).toEqual({ schema: "pg_catalog", name: "int8" });
    expect(columnFormOps({ ...f, type: { schema: "pg_catalog", name: "text" }, unique: true }, { table: T, editing: col("amount_cents"), projected: p })).toEqual([
      { alter_column: { table: T, column: "amount_cents", change: { data_type: { schema: "pg_catalog", name: "text", array: false } } } },
      { add_constraint: { table: T, constraint: { kind: { unique: { columns: ["amount_cents"] } } } } },
    ]);
    const withKey = project(invoices(), [newEdit([{ add_constraint: { table: T, constraint: { name: "memo_key", kind: { unique: { columns: ["memo"] } } } } }])]);
    const memo = withKey.columns.find((c) => c.name === "memo")!;
    const g = formFromColumn(memo, withKey, catalog);
    expect(g.unique).toBe(true);
    expect(columnFormOps({ ...g, unique: false }, { table: T, editing: memo, projected: withKey })).toEqual([
      { drop_constraint: { table: T, name: "memo_key" } },
    ]);
  });
  it("array_types_come_from_array_oid", () => {
    const arr = { ...col("memo"), type_oid: 1009, data_type: "text[]" };
    expect(formFromColumn(arr, p, catalog)).toMatchObject({ type: { schema: "pg_catalog", name: "text" }, array: true });
  });
});
```

`apps/studio/src/lib/table-editor/selection.test.ts`:

```ts
import { describe, expect, it } from "vitest";
import { NO_SELECTION, reduceSelection, rowKey, viewKey, type SelectionState } from "./selection";

const run = (...actions: Parameters<typeof reduceSelection>[1][]): SelectionState => actions.reduce(reduceSelection, NO_SELECTION);

describe("selection", () => {
  it("row_key_is_the_pk_tuple", () => {
    expect(rowKey(["id"], { id: 7, x: 1 })).toBe("[7]");
    expect(rowKey(["a", "b"], { a: "x", b: null })).toBe('["x",null]');
  });
  it("view_key_changes_with_filter_and_sort", () => {
    expect(viewKey({ changeset: "k", filter: null, sort: null })).not.toBe(viewKey({ changeset: "k", filter: { col: "a", eq: 1 }, sort: null }));
  });
  it("toggle_page_all_matching_clear", () => {
    expect(run({ type: "view", view: "v" }, { type: "toggle", key: "[1]" }, { type: "toggle", key: "[2]" }, { type: "toggle", key: "[1]" }).selection).toEqual({ kind: "rows", keys: ["[2]"] });
    expect(run({ type: "page", keys: ["[1]", "[2]"] }).selection).toEqual({ kind: "rows", keys: ["[1]", "[2]"] });
    expect(run({ type: "page", keys: ["[1]", "[2]"] }, { type: "page", keys: ["[1]", "[2]"] }).selection).toEqual({ kind: "none" });
    expect(run({ type: "page", keys: ["[1]"] }, { type: "all_matching", estimate: 48210 }).selection).toEqual({ kind: "all_matching", estimate: 48210 });
    expect(run({ type: "all_matching", estimate: 5 }, { type: "toggle", key: "[1]" }).selection).toEqual({ kind: "none" });
    expect(run({ type: "toggle", key: "[1]" }, { type: "toggle", key: "[1]" }).selection).toEqual({ kind: "none" });
    expect(run({ type: "page", keys: ["[1]"] }, { type: "clear" }).selection).toEqual({ kind: "none" });
  });
  it("a_new_view_resets", () => {
    const s = run({ type: "view", view: "v1" }, { type: "toggle", key: "[1]" });
    expect(reduceSelection(s, { type: "view", view: "v1" }).selection).toEqual({ kind: "rows", keys: ["[1]"] });
    expect(reduceSelection(s, { type: "view", view: "v2" })).toEqual({ view: "v2", selection: { kind: "none" } });
  });
});
```

`apps/studio/src/lib/table-editor/dry-run.test.ts`:

```ts
import { describe, expect, it } from "vitest";
import { ApiError, type SchemaChangeResponse } from "@chelabase/management-client";
import { newEdit } from "./changeset";
import { formatBytes, lockText, mapChangeError, mapDryRun, skipText } from "./dry-run";

const T = { schema: "public", name: "invoices" };
const edits = [
  newEdit([{ add_column: { table: T, column: { name: "d", data_type: { schema: "pg_catalog", name: "int8" } } } }, { add_constraint: { table: T, constraint: { kind: { unique: { columns: ["d"] } } } } }]),
  newEdit([{ alter_column: { table: T, column: "memo", change: { data_type: { schema: "pg_catalog", name: "int8" } } } }]),
];
const stmt = (op_index: number, over: Record<string, unknown> = {}) => ({ sql: `SQL ${op_index}`, op_index, executed: true, lock: "access_exclusive", notices: [], rewrites_table: false, skipped: null, ...over });

describe("dry run mapping", () => {
  it("statements_map_to_edits_and_all_ok_is_ready", () => {
    const res = { dry_run: true, committed: false, would_succeed: true, statements: [stmt(0), stmt(1), stmt(2)] } as SchemaChangeResponse;
    const v = mapDryRun(edits, res, { table: "public.invoices", totalBytes: 12582912 });
    expect(v.statements.map((s) => [s.sql, s.edit])).toEqual([["SQL 0", 0], ["SQL 1", 0], ["SQL 2", 1]]);
    expect(v.perEdit).toEqual([{ kind: "ok" }, { kind: "ok" }]);
    expect(v.apply).toBe("ready");
    expect(v.warnings).toEqual([]);
  });
  it("rewrites_and_rendered_only_need_an_acknowledgement", () => {
    const res = { dry_run: true, committed: false, would_succeed: null, statements: [stmt(0), stmt(1), stmt(2, { executed: false, rewrites_table: true, skipped: "large_table_rewrite" })] } as SchemaChangeResponse;
    const v = mapDryRun(edits, res, { table: "public.invoices", totalBytes: 12582912 });
    expect(v.perEdit[1]).toEqual({ kind: "rendered_only", reason: "large_table_rewrite" });
    expect(v.warnings).toEqual(["Rewrites public.invoices (12 MB): reads and writes wait while it runs"]);
    expect(v.apply).toBe("needs_ack");
  });
  it("rewrite_warning_without_a_size", () => {
    const res = { dry_run: true, committed: false, would_succeed: true, statements: [stmt(2, { rewrites_table: true }), stmt(2, { rewrites_table: true })] } as SchemaChangeResponse;
    expect(mapDryRun(edits, res, { table: "public.invoices", totalBytes: null }).warnings).toEqual(["Rewrites public.invoices: reads and writes wait while it runs"]);
  });
  it("dependents_pass_through", () => {
    const res = { dry_run: true, committed: false, would_succeed: true, statements: [], dependents: { hash: "h1", objects: [{ kind: "view", name: "v_open", schema: "public", description: "view public.v_open" }] } } as SchemaChangeResponse;
    expect(mapDryRun(edits, res, { table: "t", totalBytes: null }).dependents?.hash).toBe("h1");
  });
  it("errors_land_on_their_edit", () => {
    const f = mapChangeError(edits, new ApiError(400, "schema_change_failed", "column \"memo\" cannot be cast", { op_index: 2, sqlstate: "42804", detail: "d", hint: "h" }));
    expect(f).toEqual({ edit: 1, status: { kind: "error", message: "column \"memo\" cannot be cast", detail: "d", hint: "h" }, code: "schema_change_failed", retryable: false, dependents: null });
    expect(mapChangeError(edits, new ApiError(409, "lock_timeout", "lock timeout", { op_index: 0 })).retryable).toBe(true);
    const moved = mapChangeError(edits, new ApiError(409, "dependents_changed", "changed", { dependents: { hash: "h2", objects: [] } }));
    expect(moved).toMatchObject({ edit: null, dependents: { hash: "h2" } });
    expect(mapChangeError(edits, new ApiError(403, "branch_protected", "branch is protected")).edit).toBeNull();
  });
  it("texts", () => {
    expect(lockText("access_exclusive")).toBe("Blocks reads and writes on the table while it runs");
    expect(lockText("share_row_exclusive")).toBe("Blocks writes while it runs");
    expect(lockText("share")).toBe("Blocks writes while it runs");
    expect(lockText("share_update_exclusive")).toBe("Allows reads and writes; blocks other schema changes");
    expect(lockText("none")).toBe("No table lock");
    expect(skipText("large_table_rewrite")).toBe("Not simulated: rewrites a large table");
    expect(skipText("concurrently")).toBe("Not simulated: runs concurrently");
    expect(skipText("after_skipped")).toBe("Not simulated: follows a statement that wasn't simulated");
    expect(skipText("already_dropped")).toBe("Not simulated: its object is dropped earlier in this change");
    expect([formatBytes(500), formatBytes(8192), formatBytes(12582912), formatBytes(1610612736)]).toEqual(["500 B", "8 kB", "12 MB", "1.5 GB"]);
  });
});
```

**Gotchas:** `project` must interpret unfolded sequences (the property tests compare raw against folded). Marks are state-based (P7). A dropped column keeps its base name and base fields. `fold` works op by op on `next.ops`. `ApiError` is exported by `@chelabase/management-client`.

**Verify:** `pnpm --filter @chelabase/studio exec vitest run src/lib/table-editor` and `pnpm --filter @chelabase/studio typecheck`.

**Live runs (the plan's one stack):** none.

---

## T2: route, context bar, table list, store, access (sonnet / sonnet)

**Files:** create `apps/studio/src/lib/table-editor/{context,access,store}.ts`, `apps/studio/src/components/table-editor/{table-editor,context-bar,table-list}.tsx`, `apps/studio/src/components/app-shell/header-context.tsx`, `apps/studio/src/routes/app.tables.{-$projectId}.{-$databaseId}.{-$branchId}.{-$pgDatabase}.{-$schema}.{-$table}.tsx` (and regenerate `routeTree.gen.ts` with `pnpm --filter @chelabase/studio generate-routes`); modify `app-shell/app-header.tsx` (slot id), `app-shell/rail-items.ts`, `components/project-page.tsx` (Tables → link). Tests (coordinator): `apps/studio/src/test/table-editor-fixtures.tsx`, `apps/studio/src/lib/table-editor/{context,access,store}.test.ts`, `apps/studio/src/components/table-editor/table-editor-shell.test.tsx`, `apps/studio/src/components/app-shell/header-context.test.tsx`, and the changed existing tests (`rail-items.test.ts`, `app-rail.test.tsx`, `project-page.test.tsx`).

**Behaviour:** spec §1; surface brief §3 (header and left list); `switch-pickers.html`. Pickers are Base UI comboboxes labelled `Project`, `Database`, `Branch`, `Postgres db`, `Act as`, opening on click (`openOnInputClick`), showing the current name as their value (`Acme shop`, `orders-db`, `main`, `app`, `owner` or the role). Branch options are `branchTree` rows (indented by depth, "protected" label, created date) plus a last option `New branch from {name}…` that opens the existing new-branch form in a dialog. The Postgres db options mark served databases with `data API`. The table list: a `Schema` select, a `Filter tables` input, one button per object of the schema named by the object's name, `Types` and `New table` buttons (T6 fills their views; in T2 they navigate nowhere and are present). The main area in T2 shows `Pick a table` when no table is chosen, `{Level} not found` on `not_found` (Project, Database, Branch, Postgres database, Schema, Table), and the structure reason in `data-testid="structure-reason"` when edits aren't allowed. Main area placeholder for T3: `<section aria-label="Rows">` (empty in T2).

**RED tests (coordinator):**

`apps/studio/src/test/table-editor-fixtures.tsx` (used by T2–T6):

```tsx
import { vi } from "vitest";
import userEvent from "@testing-library/user-event";
import type { Table } from "@chelabase/management-client";
import { TableEditor } from "@/components/table-editor/table-editor";
import type { EditorParams } from "@/lib/table-editor/context";
import { invoices } from "@/lib/table-editor/test-tables";
import { fakeFetch, jsonResponse, type FakeFetch, type FakeHandler } from "./fake-fetch";
import { renderWithQuery } from "./render";

export { invoices };
export const D = "/v1/databases/d1";
export const B = `${D}/branches/b1`;
export const T = `${B}/tables/public/invoices`;
export const FULL: EditorParams = { projectId: "p1", databaseId: "d1", branchId: "b1", pgDatabase: "app", schema: "public", table: "invoices" };

export type Level = "viewer" | "developer" | "admin";
export function meAs(level: Level, roles: string[] = []) {
  return {
    sub: "u2", email: "bo@example.test", name: "Bo", tenant_id: "t1",
    member: { id: "m-self", status: "active", tenant_admin: false },
    instance_admin: false, tenant_admins: [],
    projects: ["p1", "p2"].map((project_id) => ({ project_id, platform_role: level, roles, databases: [] })),
    user: { sub: "u2", org_id: "org-1" },
    tenant: { id: "t1", slug: "demo", display_name: null, projects: [] },
  };
}
const dbSummary = (database_id: string, name: string) => ({ database_id, name, status: "ready", branch_count: 2, host: null, port: null, postgres_database: "app" });
export const PROJECTS = [
  { id: "p1", name: "Acme shop", slug: "acme-shop", status: "active", databases: [dbSummary("d1", "orders-db"), dbSummary("d2", "analytics")] },
  { id: "p2", name: "Demo", slug: "demo", status: "active", databases: [dbSummary("d9", "demo-db")] },
];
export const branch = (id: string, name: string, parent: string | null, over: Record<string, unknown> = {}) => ({
  id, name, parent_branch_id: parent, kind: "normal", status: "ready", protected: false, created_at: "2026-10-01T00:00:00Z",
  branched_at_lsn: null, branched_at_time: null, error: null, guard_logins: false, history_start: null, host: null, port: null,
  main_database_missing: false, review_acknowledged_hash: null, ...over,
});
export const BRANCHES = [
  branch("b1", "main", null, { protected: true }), branch("b2", "staging", "b1"), branch("b3", "feat-discounts", "b2"),
  branch("b4", "backup-1", "b1", { kind: "backup" }), branch("b5", "broken", "b1", { status: "failed" }),
];
export const pgDb = (name: string, main: boolean, data_api = "on") => ({ name, main, data_api, data_api_error: null, default_schema: "public", origin: main ? "main" : "owner", owner: "app_owner" });
export const PG_DATABASES = [pgDb("app", true), pgDb("reporting", false, "off")];
export const role = (name: string, origin = "sql") => ({ name, level: "plain", bypass_rls: false, login: false, origin, password: "none", pending: false, blocked_keys: [], created_at: "2026-10-01T00:00:00Z" });
export const ROLES = [role("app_owner", "owner"), role("billing_admin"), role("reporter")];
export const obj = (schema: string, name: string, over: Record<string, unknown> = {}) => ({
  schema, name, kind: "table", oid: 1, owner: "app_owner", comment: null, rls_enabled: true, rls_forced: false, data_api_exposed: true,
  row_estimate: 100, total_bytes: 8192, editable: true, ...over,
});
export const OBJECTS = [
  obj("public", "invoices", { row_estimate: 48210, total_bytes: 12582912 }), obj("public", "customers", { row_estimate: 3100 }),
  obj("public", "v_open", { kind: "view", editable: false }), obj("billing", "ledger"),
];
export const TYPES = { types: [
  { oid: 20, array_oid: 1016, schema: "pg_catalog", name: "int8", kind: "base", category: "N", preferred: false, takes_modifiers: false },
  { oid: 25, array_oid: 1009, schema: "pg_catalog", name: "text", kind: "base", category: "S", preferred: true, takes_modifiers: false },
  { oid: 16, array_oid: 1000, schema: "pg_catalog", name: "bool", kind: "base", category: "B", preferred: true, takes_modifiers: false },
  { oid: 2950, array_oid: 2951, schema: "pg_catalog", name: "uuid", kind: "base", category: "U", preferred: false, takes_modifiers: false },
  { oid: 1184, array_oid: 1185, schema: "pg_catalog", name: "timestamptz", kind: "base", category: "D", preferred: true, takes_modifiers: true },
  { oid: 16500, array_oid: 16499, schema: "public", name: "invoice_status", kind: "enum", category: "E", preferred: false, takes_modifiers: false, labels: ["open", "paid", "void"] },
] };
export const ROWS = [
  { id: "3f9a-1", customer_id: "c-1", status: "paid", amount_cents: 12900, memo: null },
  { id: "81c4-2", customer_id: "c-2", status: "open", amount_cents: 45000, memo: "Q3 plan" },
  { id: "c2d1-3", customer_id: "c-1", status: "void", amount_cents: 8900, memo: "dup" },
];
/** A dry run echoing one statement per operation; a real run that commits. */
export const schemaChanges: FakeHandler = async (req) => {
  const body = (await req.clone().json()) as { dry_run?: boolean; operations?: unknown[] };
  const statements = (body.operations ?? []).map((_, i) => ({ sql: `SQL ${i}`, op_index: i, executed: !!body.dry_run, lock: "access_exclusive", notices: [], rewrites_table: false, skipped: null }));
  return jsonResponse({ dry_run: !!body.dry_run, committed: !body.dry_run, would_succeed: body.dry_run ? true : null, statements });
};

export interface EditorSetup {
  me?: ReturnType<typeof meAs>;
  params?: EditorParams;
  table?: Table;
  rows?: Record<string, unknown>[];
  objects?: unknown[];
  routes?: Record<string, FakeHandler>;
}
export function editorRoutes(o: EditorSetup = {}): Record<string, FakeHandler> {
  return {
    "GET /v1/me": () => jsonResponse(o.me ?? meAs("admin")),
    "GET /v1/projects": () => jsonResponse(PROJECTS),
    [`GET ${D}`]: () => jsonResponse({ database_id: "d1", name: "orders-db", branch_count: 2, history_window: 604800, max_branches: 5, max_rows: 1000, statement_timeout_ms: 8000, max_connections: 5 }),
    [`GET ${D}/branches`]: () => jsonResponse(BRANCHES),
    [`GET ${B}/databases`]: () => jsonResponse(PG_DATABASES),
    [`GET ${B}/roles`]: () => jsonResponse(ROLES),
    [`GET ${B}/objects`]: () => jsonResponse({ objects: o.objects ?? OBJECTS }),
    [`GET ${B}/types`]: () => jsonResponse(TYPES),
    [`GET ${T}`]: () => jsonResponse(o.table ?? invoices()),
    [`GET ${B}/tables/public/customers`]: () => jsonResponse(invoices({ name: "customers" } as Partial<Table>)),
    [`POST ${B}/console/query`]: () => jsonResponse(o.rows ?? ROWS),
    [`POST ${B}/schema-changes`]: schemaChanges,
    ...o.routes,
  };
}
export function renderEditor(o: EditorSetup = {}) {
  const navigate = vi.fn();
  const f: FakeFetch = fakeFetch(editorRoutes(o));
  const result = renderWithQuery(<TableEditor params={o.params ?? FULL} navigate={navigate} />, { fetch: f });
  return { ...result, f, navigate, user: userEvent.setup() };
}
/** The JSON bodies of the requests to `path` with `method`, in order. */
export async function bodies(f: FakeFetch, method: string, path: string): Promise<any[]> {
  const hits = f.calls.filter((r) => r.method === method && new URL(r.url).pathname === path);
  return Promise.all(hits.map((r) => r.clone().json()));
}
/** jsdom has no layout: give elements a size so the virtualizer renders rows. */
export function stubLayout() {
  vi.spyOn(HTMLElement.prototype, "getBoundingClientRect").mockReturnValue({ x: 0, y: 0, top: 0, left: 0, right: 1200, bottom: 800, width: 1200, height: 800, toJSON: () => ({}) } as DOMRect);
  vi.spyOn(HTMLElement.prototype, "offsetHeight", "get").mockReturnValue(800);
  vi.spyOn(HTMLElement.prototype, "offsetWidth", "get").mockReturnValue(1200);
  vi.stubGlobal("ResizeObserver", class { observe() {} unobserve() {} disconnect() {} });
}
```

`apps/studio/src/lib/table-editor/context.test.ts`:

```ts
import { describe, expect, it } from "vitest";
import type { Branch, BranchDatabaseView, ObjectSummary, Project } from "@chelabase/management-client";
import { BRANCHES, OBJECTS, PG_DATABASES, PROJECTS, branch, obj } from "@/test/table-editor-fixtures";
import { LAST_KEY, branchTree, editorPath, readLast, resolveParams, schemasOf, writeLast } from "./context";

const projects = PROJECTS as unknown as Project[];
const branches = BRANCHES as unknown as Branch[];
const pgDatabases = PG_DATABASES as unknown as BranchDatabaseView[];
const objects = OBJECTS as unknown as ObjectSummary[];
const all = { projects, branches, pgDatabases, objects };
const throwing = { getItem: () => { throw new Error("no"); }, setItem: () => { throw new Error("no"); } } as unknown as Storage;

describe("editor context", () => {
  it("editor_path_stops_at_the_first_missing_level", () => {
    expect(editorPath({})).toBe("/app/tables");
    expect(editorPath({ projectId: "p1", databaseId: "d1", table: "x" })).toBe("/app/tables/p1/d1");
    expect(editorPath({ projectId: "p1", databaseId: "d1", branchId: "b1", pgDatabase: "my db", schema: "public", table: "a/b" })).toBe("/app/tables/p1/d1/b1/my%20db/public/a%2Fb");
  });
  it("last_context_round_trips_and_survives_broken_storage", () => {
    localStorage.clear();
    writeLast(localStorage, { projectId: "p2", databaseId: "d9" });
    expect(JSON.parse(localStorage.getItem(LAST_KEY)!)).toEqual({ projectId: "p2", databaseId: "d9" });
    expect(readLast(localStorage)).toEqual({ projectId: "p2", databaseId: "d9" });
    localStorage.setItem(LAST_KEY, "{nope");
    expect(readLast(localStorage)).toBeNull();
    expect(readLast(throwing)).toBeNull();
    expect(() => writeLast(throwing, {})).not.toThrow();
    expect(readLast(null)).toBeNull();
  });
  it("fills_levels_in_order_while_data_allows", () => {
    expect(resolveParams({}, null, {})).toEqual({ kind: "loading" });
    expect(resolveParams({}, null, { projects })).toEqual({ kind: "redirect", params: { projectId: "p1", databaseId: "d1" } });
    expect(resolveParams({}, { projectId: "p2" }, { projects })).toEqual({ kind: "redirect", params: { projectId: "p2", databaseId: "d9" } });
    expect(resolveParams({}, { projectId: "gone" }, { projects })).toEqual({ kind: "redirect", params: { projectId: "p1", databaseId: "d1" } });
    expect(resolveParams({ projectId: "p1", databaseId: "d1" }, null, { projects, branches })).toEqual({ kind: "redirect", params: { projectId: "p1", databaseId: "d1", branchId: "b1" } });
    expect(resolveParams({ projectId: "p1", databaseId: "d1", branchId: "b1" }, null, all)).toEqual({ kind: "redirect", params: { projectId: "p1", databaseId: "d1", branchId: "b1", pgDatabase: "app", schema: "public" } });
  });
  it("schema_falls_back_to_the_first", () => {
    const only = [obj("billing", "ledger"), obj("audit", "log")] as unknown as ObjectSummary[];
    expect(resolveParams({ projectId: "p1", databaseId: "d1", branchId: "b1", pgDatabase: "app" }, null, { ...all, objects: only })).toEqual({
      kind: "redirect", params: { projectId: "p1", databaseId: "d1", branchId: "b1", pgDatabase: "app", schema: "audit" },
    });
    expect(schemasOf(objects)).toEqual(["billing", "public"]);
  });
  it("ready_and_not_found", () => {
    const base = { projectId: "p1", databaseId: "d1", branchId: "b1", pgDatabase: "app", schema: "public" };
    expect(resolveParams(base, null, all)).toEqual({ kind: "ready", params: base });
    expect(resolveParams({ ...base, table: "invoices" }, null, all)).toEqual({ kind: "ready", params: { ...base, table: "invoices" } });
    expect(resolveParams({ ...base, table: "nope" }, null, all)).toEqual({ kind: "not_found", level: "table" });
    expect(resolveParams({ ...base, branchId: "zz" }, null, all)).toEqual({ kind: "not_found", level: "branch" });
    expect(resolveParams({ projectId: "zz" }, null, all)).toEqual({ kind: "not_found", level: "project" });
    expect(resolveParams({ ...base, pgDatabase: "zz" }, null, all)).toEqual({ kind: "not_found", level: "pgDatabase" });
  });
  it("branch_tree_keeps_ready_normal_branches_depth_first", () => {
    const extra = [...branches, branch("b6", "alpha", "b1") as unknown as Branch];
    expect(branchTree(extra).map((r) => [r.branch.name, r.depth])).toEqual([["main", 0], ["alpha", 1], ["staging", 1], ["feat-discounts", 2]]);
  });
});
```

`apps/studio/src/lib/table-editor/access.test.ts`:

```ts
import { describe, expect, it } from "vitest";
import type { Branch, BranchDatabaseView, MeResponse } from "@chelabase/management-client";
import { projectCaps } from "@/lib/capabilities";
import { branch, meAs, pgDb } from "@/test/table-editor-fixtures";
import { rowAccess, structureAccess } from "./access";

const caps = (level: "viewer" | "developer" | "admin") => projectCaps(meAs(level) as unknown as MeResponse, "p1");
const main = branch("b1", "main", null, { protected: true }) as unknown as Branch;
const dev = branch("b2", "staging", "b1") as unknown as Branch;
const on = pgDb("app", true) as unknown as BranchDatabaseView;
const off = pgDb("reporting", false, "off") as unknown as BranchDatabaseView;

describe("editor access", () => {
  it("structure_by_role_and_protection", () => {
    expect(structureAccess(caps("admin"), main)).toEqual({ allowed: true });
    expect(structureAccess(caps("developer"), dev)).toEqual({ allowed: true });
    expect(structureAccess(caps("developer"), main)).toEqual({ allowed: false, reason: "main is protected: schema changes need Admin" });
    expect(structureAccess(caps("viewer"), dev)).toEqual({ allowed: false, reason: "Viewers can't change the schema" });
    expect(structureAccess(projectCaps(meAs("admin") as unknown as MeResponse, "p-other"), dev)).toEqual({ allowed: false, reason: "You don't have access to this project" });
  });
  it("rows_by_data_api_role_and_level", () => {
    expect(rowAccess(caps("developer"), { owner: false, roles: ["billing_admin"] }, on)).toEqual({ read: true, write: true, reason: null });
    expect(rowAccess(caps("viewer"), { owner: false, roles: ["reporter"] }, on)).toEqual({ read: true, write: false, reason: null });
    expect(rowAccess(caps("developer"), { owner: false, roles: [] }, on)).toEqual({ read: false, write: false, reason: "You can't act as any Postgres role here." });
    expect(rowAccess(caps("admin"), { owner: true, roles: [] }, off)).toEqual({ read: false, write: false, reason: "Rows need the data API on reporting" });
  });
});
```

`apps/studio/src/lib/table-editor/store.test.ts`:

```ts
import { beforeEach, describe, expect, it } from "vitest";
import { newEdit, tableFingerprint } from "./changeset";
import { DRAFTS_KEY, createTableEditorStore } from "./store";
import { invoices } from "./test-tables";

const T = { schema: "public", name: "invoices" };
const K = "d1/b1/app/public.invoices";
const add = newEdit([{ add_column: { table: T, column: { name: "d", data_type: { schema: "pg_catalog", name: "int8" } } } }]);
const drop = newEdit([{ drop_column: { table: T, column: "d" } }]);

describe("table editor store", () => {
  beforeEach(() => sessionStorage.clear());
  it("adds_folds_and_drops_empty_changesets", () => {
    const s = createTableEditorStore(sessionStorage);
    s.getState().addEdit(K, invoices(), add);
    expect(s.getState().changesets[K]).toMatchObject({ key: K, base: tableFingerprint(invoices()) });
    expect(s.getState().changesets[K].edits).toHaveLength(1);
    s.getState().addEdit(K, invoices(), drop);
    expect(s.getState().changesets[K]).toBeUndefined();
  });
  it("persists_drafts_to_session_storage_and_loads_them", () => {
    const s = createTableEditorStore(sessionStorage);
    s.getState().addEdit(K, invoices(), add);
    expect(JSON.parse(sessionStorage.getItem(DRAFTS_KEY)!)[K].edits[0].label).toBe("Add column d int8");
    const again = createTableEditorStore(sessionStorage);
    expect(again.getState().changesets[K].edits[0].ops).toEqual(add.ops);
    again.getState().discard(K);
    expect(JSON.parse(sessionStorage.getItem(DRAFTS_KEY)!)).toEqual({});
  });
  it("remove_edit_and_rebase", () => {
    const s = createTableEditorStore(null);
    s.getState().addEdit(K, invoices(), add);
    s.getState().rebase(K, invoices({ comment: "new" }));
    expect(s.getState().changesets[K].base).toBe(tableFingerprint(invoices({ comment: "new" })));
    s.getState().removeEdit(K, s.getState().changesets[K].edits[0].id);
    expect(s.getState().changesets[K]).toBeUndefined();
  });
  it("broken_storage_is_ignored", () => {
    const broken = { getItem: () => { throw new Error("x"); }, setItem: () => { throw new Error("x"); }, removeItem: () => {} } as unknown as Storage;
    const s = createTableEditorStore(broken);
    expect(() => s.getState().addEdit(K, invoices(), add)).not.toThrow();
    expect(s.getState().changesets[K].edits).toHaveLength(1);
  });
  it("panel_act_as_and_selection", () => {
    const s = createTableEditorStore(null);
    s.getState().setCollapsed(true);
    s.getState().openPanel("column", { column: "memo" });
    expect(s.getState().panel).toEqual({ kind: "column", collapsed: false, column: "memo", row: null });
    s.getState().setActAs("d1/b1", "billing_admin");
    expect(s.getState().actAs["d1/b1"]).toBe("billing_admin");
    s.getState().select({ type: "view", view: "v" });
    s.getState().select({ type: "toggle", key: "[1]" });
    expect(s.getState().selection).toEqual({ view: "v", selection: { kind: "rows", keys: ["[1]"] } });
  });
});
```

`apps/studio/src/components/app-shell/header-context.test.tsx`:

```tsx
import { render, screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import { HeaderContext } from "./header-context";

describe("HeaderContext", () => {
  it("portals_into_the_header_slot_or_renders_inline", () => {
    const { unmount } = render(<HeaderContext><span>ctx</span></HeaderContext>);
    expect(screen.getByText("ctx")).toBeInTheDocument();
    unmount();
    const slot = document.createElement("div");
    slot.id = "app-header-context";
    document.body.appendChild(slot);
    render(<div data-testid="page"><HeaderContext><span>ctx</span></HeaderContext></div>);
    expect(slot).toHaveTextContent("ctx");
    expect(screen.getByTestId("page")).not.toHaveTextContent("ctx");
    slot.remove();
  });
});
```

`apps/studio/src/components/table-editor/table-editor-shell.test.tsx`:

```tsx
import { screen, within } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { FULL, meAs, renderEditor, stubLayout } from "@/test/table-editor-fixtures";
import { LAST_KEY } from "@/lib/table-editor/context";

describe("TableEditor shell", () => {
  beforeEach(() => { localStorage.clear(); sessionStorage.clear(); stubLayout(); });

  it("redirects_missing_levels_with_replace", async () => {
    const { navigate } = renderEditor({ params: {} });
    await vi.waitFor(() => expect(navigate).toHaveBeenCalledWith({ projectId: "p1", databaseId: "d1" }, { replace: true }));
  });

  it("shows_the_four_pickers_and_act_as", async () => {
    renderEditor();
    expect(await screen.findByRole("combobox", { name: "Project" })).toHaveValue("Acme shop");
    expect(screen.getByRole("combobox", { name: "Database" })).toHaveValue("orders-db");
    expect(await screen.findByRole("combobox", { name: "Branch" })).toHaveValue("main");
    expect(await screen.findByRole("combobox", { name: "Postgres db" })).toHaveValue("app");
    expect(await screen.findByRole("combobox", { name: "Act as" })).toHaveValue("owner");
  });

  it("picking_a_branch_drops_the_levels_after_it", async () => {
    const { user, navigate } = renderEditor();
    await user.click(await screen.findByLabelText("Branch"));
    expect(screen.queryByRole("option", { name: /^backup-1/ })).toBeNull();
    expect(screen.queryByRole("option", { name: /^broken/ })).toBeNull();
    expect(screen.getByRole("option", { name: /New branch from main/ })).toBeInTheDocument();
    await user.click(await screen.findByRole("option", { name: /^feat-discounts/ }));
    expect(navigate).toHaveBeenLastCalledWith({ projectId: "p1", databaseId: "d1", branchId: "b3" });
  });

  it("picking_a_project_drops_everything_after_it", async () => {
    const { user, navigate } = renderEditor();
    await user.click(await screen.findByLabelText("Project"));
    await user.click(await screen.findByRole("option", { name: /^Demo/ }));
    expect(navigate).toHaveBeenLastCalledWith({ projectId: "p2" });
  });

  it("postgres_db_options_mark_the_data_api", async () => {
    const { user } = renderEditor();
    await user.click(await screen.findByLabelText("Postgres db"));
    expect(await screen.findByRole("option", { name: /^app.*data API/ })).toBeInTheDocument();
    expect(screen.getByRole("option", { name: /^reporting/ })).not.toHaveTextContent("data API");
  });

  it("remembers_the_last_context_when_ready", async () => {
    renderEditor();
    await screen.findByRole("combobox", { name: "Branch" });
    await vi.waitFor(() => expect(JSON.parse(localStorage.getItem(LAST_KEY)!)).toEqual(FULL));
  });

  it("table_list_shows_the_schema_and_filters", async () => {
    const { user, navigate } = renderEditor({ params: { ...FULL, table: undefined } });
    const list = await screen.findByRole("navigation", { name: "Tables" });
    expect(await within(list).findByRole("button", { name: "invoices" })).toBeInTheDocument();
    expect(within(list).getByRole("button", { name: "v_open" })).toBeInTheDocument();
    expect(within(list).queryByRole("button", { name: "ledger" })).toBeNull();
    expect(screen.getByText("Pick a table")).toBeInTheDocument();
    await user.type(within(list).getByLabelText("Filter tables"), "cust");
    expect(within(list).queryByRole("button", { name: "invoices" })).toBeNull();
    await user.click(within(list).getByRole("button", { name: "customers" }));
    expect(navigate).toHaveBeenLastCalledWith({ ...FULL, table: "customers" });
    expect(within(list).getByRole("button", { name: "Types" })).toBeInTheDocument();
    expect(within(list).getByRole("button", { name: "New table" })).toBeInTheDocument();
  });

  it("switching_schema_drops_the_table", async () => {
    const { user, navigate } = renderEditor();
    const list = await screen.findByRole("navigation", { name: "Tables" });
    await user.click(within(list).getByLabelText("Schema"));
    await user.click(await screen.findByRole("option", { name: "billing" }));
    expect(navigate).toHaveBeenLastCalledWith({ ...FULL, schema: "billing", table: undefined });
  });

  it("not_found_keeps_the_pickers", async () => {
    renderEditor({ params: { ...FULL, branchId: "zz", pgDatabase: undefined, schema: undefined, table: undefined } });
    expect(await screen.findByText("Branch not found")).toBeInTheDocument();
    expect(screen.getByRole("combobox", { name: "Project" })).toBeInTheDocument();
  });

  it("protected_branch_reason_for_a_developer", async () => {
    renderEditor({ me: meAs("developer", ["billing_admin"]) });
    expect(await screen.findByTestId("structure-reason")).toHaveTextContent("main is protected: schema changes need Admin");
  });

  it("act_as_lists_only_allowed_roles", async () => {
    const { user } = renderEditor({ me: meAs("developer", ["billing_admin"]) });
    expect(await screen.findByRole("combobox", { name: "Act as" })).toHaveValue("billing_admin");
    await user.click(screen.getByLabelText("Act as"));
    expect(await screen.findByRole("option", { name: "billing_admin" })).toBeInTheDocument();
    expect(screen.queryByRole("option", { name: "owner" })).toBeNull();
    expect(screen.queryByRole("option", { name: "reporter" })).toBeNull();
  });
});
```

Changes to existing tests (coordinator):

- `apps/studio/src/components/app-shell/rail-items.test.ts`: the `RAIL_ENTRIES` test becomes

```ts
  it("lists Projects, Table editor, API reference and Members (last) in order", () => {
    expect(RAIL_ENTRIES.map((e) => [e.kind, e.id, e.label])).toEqual([
      ["link", "projects", "Projects"],
      ["link", "tables", "Table editor"],
      ["link", "api", "API reference"],
      ["link", "members", "Members"],
    ]);
    expect(RAIL_ENTRIES.flatMap((e) => (e.kind === "link" ? [e.to] : []))).toEqual(["/app", "/app/tables", "/app/api", "/app/members"]);
  });
```

  and a new case in `isRailItemActive`:

```ts
  it("tables_item_active_on_editor_routes", () => {
    expect(isRailItemActive("tables", "/app/tables")).toBe(true);
    expect(isRailItemActive("tables", "/app/tables/p1/d1")).toBe(true);
    expect(isRailItemActive("tables", "/app/tablesx")).toBe(false);
    expect(isRailItemActive("projects", "/app/tables/p1")).toBe(false);
  });
```

- `apps/studio/src/components/app-shell/app-rail.test.tsx`: the test "placeholders are aria-disabled and not focusable" is replaced by

```tsx
  it("no_placeholder_is_left_and_table_editor_is_a_link", async () => {
    await renderRail("expanded");
    expect(screen.queryByText("Placeholder")).toBeNull();
    expect(screen.getByRole("link", { name: "Table editor" })).toHaveAttribute("href", "/app/tables");
  });
```

- `apps/studio/src/components/project-page.test.tsx`: "opens the Tables console of a database" becomes

```tsx
  it("tables_links_to_the_table_editor", async () => {
    setup({});
    expect(await screen.findByRole("link", { name: "Tables" })).toHaveAttribute("href", "/app/tables/p1/d1");
  });
```

**Gotchas:** `TableEditor` must not depend on the router (tests render it without one); only the route file does. The `New branch from…` option opens `new-branch-form.tsx` in a dialog. `Act as` defaults: owner for Admins, else the first role of `actAsRoles(...).roles`; the store keeps the choice per `${databaseId}/${branchId}`. `routeTree.gen.ts` is regenerated, not hand-edited. In `rail-item.tsx` nothing changes (links already render). If `app-rail.test.tsx`'s `renderRail` has no router for links, follow how it renders the other links.

**Verify:** `pnpm --filter @chelabase/studio exec vitest run src/lib/table-editor src/components/table-editor src/components/app-shell src/components/project-page.test.tsx` and `pnpm --filter @chelabase/studio typecheck`.

**Live runs (the plan's one stack):** none.

---

## T3: the rows view (opus / opus)

**Files:** create `apps/studio/src/components/table-editor/{rows-view,grid,cell-editor,selection-bar,filter-popover,delete-dialog}.tsx`; modify `table-editor.tsx` to mount `RowsView` in the Rows section and to leave two mount points for wave 3: a `{panel}` element after `<main>` (rendered by a `EditorPanel` placeholder component `components/table-editor/panel.tsx` that T4/T5 fill) and a `structure` branch in the Rows | Structure switch (`components/table-editor/structure-view.tsx` placeholder exporting `StructureView` returning `null`). Extend `lib/console.ts` only additively (`buildSelect` gains an optional `columns?: string[]` → `tree.select`). Tests (coordinator): `apps/studio/src/components/table-editor/rows-view.test.tsx`.

**Behaviour:** spec §3; surface brief §3 (grid, selection); `switch-pickers.html`, `grid-edges.html`. The Rows | Structure switch is a tab list (`Rows`, `Structure`) in the table bar; T3 implements the Rows tab. Grid: `role="grid"` named by the table (`aria-label="public.invoices"`), column headers `role="columnheader"` named by the column name, cells `role="gridcell"`; gutter checkbox per row named `Select row {n}` (1-based on the page), header checkbox `Select all rows on this page`. Footer `Rows {from}–{to} of ≈ {estimate}` (`toLocaleString("en-US")`), `of many` when the estimate is null. Page buttons `Previous page`, `Next page`. Selection bar (when something is selected): `{n} selected` or `All ≈ {estimate} matching rows selected`, buttons `Select all ≈ {estimate} matching rows`, `Copy`, `Copy as JSON`, `Delete`, `Clear selection`. Delete dialog: title `Delete {n} rows?` (`Delete 1 row?`), confirm `Delete {n} rows`; for all matching: title `Delete all matching rows?`, and with no filter an input labelled `Type {table} to confirm`, confirm `Delete all rows` enabled only when typed. Table without a primary key: note `This table has no primary key, so its rows are read-only here.` Viewer/read-only: no editing and no `Insert row`. Data API off: `data-testid="rows-gate"` with the reason and, for Developer+, `Enable data API`. Filter popover: button `Filter`, fields `Filter column`, `Filter operator`, `Filter value`, `Apply filter`, `Clear filters`. Page-size halving on `too_large` (413), down to 1.

**RED tests (coordinator)** — `apps/studio/src/components/table-editor/rows-view.test.tsx`:

```tsx
import { screen, within } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { jsonResponse, type FakeHandler } from "@/test/fake-fetch";
import { B, FULL, ROWS, bodies, invoices, meAs, obj, renderEditor, stubLayout } from "@/test/table-editor-fixtures";

const Q = `${B}/console/query`;
const grid = () => screen.findByRole("grid", { name: "public.invoices" });
const cell = async (text: string) => within(await grid()).findByRole("gridcell", { name: text });
const lastQuery = async (f: Parameters<typeof bodies>[0]) => (await bodies(f, "POST", Q)).at(-1);
const selects = async (f: Parameters<typeof bodies>[0]) => (await bodies(f, "POST", Q)).filter((b) => b.tree.op === "select");
const writes = async (f: Parameters<typeof bodies>[0]) => (await bodies(f, "POST", Q)).filter((b) => b.tree.op !== "select");

describe("RowsView", () => {
  beforeEach(() => { localStorage.clear(); sessionStorage.clear(); stubLayout(); });

  it("reads_the_first_page_as_the_owner", async () => {
    const { f } = renderEditor();
    expect(await cell("45000")).toBeInTheDocument();
    const q = (await selects(f)).at(-1);
    expect(q.role).toBeNull();
    expect(q.tree).toMatchObject({ v: 1, op: "select", from: "public.invoices", limit: 50, offset: 0, orderBy: [{ col: "id", dir: "asc" }] });
    expect(screen.getByText("Rows 1–3 of ≈ 48,210")).toBeInTheDocument();
  });

  it("acts_as_the_chosen_role", async () => {
    const { f } = renderEditor({ me: meAs("developer", ["billing_admin"]) });
    await cell("45000");
    expect((await selects(f)).at(-1).role).toBe("billing_admin");
  });

  it("estimate_unknown_reads_of_many", async () => {
    renderEditor({ objects: [obj("public", "invoices", { row_estimate: null })] });
    expect(await screen.findByText("Rows 1–3 of many")).toBeInTheDocument();
  });

  it("header_click_cycles_the_sort", async () => {
    const { f, user } = renderEditor();
    const header = await within(await grid()).findByRole("columnheader", { name: /amount_cents/ });
    await user.click(header);
    await vi.waitFor(async () => expect((await selects(f)).at(-1).tree.orderBy).toEqual([{ col: "amount_cents", dir: "asc" }, { col: "id", dir: "asc" }]));
    await user.click(header);
    await vi.waitFor(async () => expect((await selects(f)).at(-1).tree.orderBy[0]).toEqual({ col: "amount_cents", dir: "desc" }));
    await user.click(header);
    await vi.waitFor(async () => expect((await selects(f)).at(-1).tree.orderBy).toEqual([{ col: "id", dir: "asc" }]));
  });

  it("paging_moves_the_offset", async () => {
    const many = Array.from({ length: 50 }, (_, i) => ({ ...ROWS[0], id: `r-${i}`, amount_cents: i }));
    const { f, user } = renderEditor({ rows: many });
    await cell("49");
    await user.click(screen.getByRole("button", { name: "Next page" }));
    await vi.waitFor(async () => expect((await selects(f)).at(-1).tree.offset).toBe(50));
  });

  it("inline_edit_saves_one_update_by_key", async () => {
    const { f, user } = renderEditor();
    await user.dblClick(await cell("45000"));
    const input = await screen.findByRole("textbox", { name: "amount_cents" });
    await user.clear(input);
    await user.type(input, "46000{Enter}");
    await vi.waitFor(async () => expect(await writes(f)).toHaveLength(1));
    expect((await writes(f))[0].tree).toEqual({ v: 1, op: "update", table: "public.invoices", set: { amount_cents: 46000 }, where: { col: "id", eq: "81c4-2" }, returning: "*" });
  });

  it("escape_cancels_an_edit", async () => {
    const { f, user } = renderEditor();
    await user.dblClick(await cell("45000"));
    await user.type(await screen.findByRole("textbox", { name: "amount_cents" }), "9{Escape}");
    expect(screen.queryByRole("textbox", { name: "amount_cents" })).toBeNull();
    expect(await writes(f)).toHaveLength(0);
  });

  it("a_refused_edit_shows_its_error_at_the_cell", async () => {
    const refuse: FakeHandler = async (req) => {
      const b = await req.clone().json();
      return b.tree.op === "select" ? jsonResponse(ROWS) : jsonResponse({ error: { code: "permission_denied", message: "raw" } }, 403);
    };
    const { user } = renderEditor({ me: meAs("developer", ["billing_admin"]), routes: { [`POST ${Q}`]: refuse } });
    await user.dblClick(await cell("45000"));
    await user.type(await screen.findByRole("textbox", { name: "amount_cents" }), "1{Enter}");
    expect(await screen.findByText("billing_admin can't update this table.")).toBeInTheDocument();
  });

  it("keyboard_moves_and_enter_edits", async () => {
    const { user } = renderEditor();
    await user.click(await cell("12900"));
    await user.keyboard("{ArrowDown}");
    expect(document.activeElement).toHaveAccessibleName("45000");
    await user.keyboard("{ArrowRight}");
    expect(document.activeElement).toHaveAccessibleName("Q3 plan");
    await user.keyboard("{Enter}");
    expect(await screen.findByRole("textbox", { name: "memo" })).toHaveValue("Q3 plan");
  });

  it("no_primary_key_is_read_only", async () => {
    const t = invoices({ constraints: [] });
    const { f, user } = renderEditor({ table: t });
    expect(await screen.findByText("This table has no primary key, so its rows are read-only here.")).toBeInTheDocument();
    await user.dblClick(await cell("45000"));
    expect(screen.queryByRole("textbox", { name: "amount_cents" })).toBeNull();
    expect(await writes(f)).toHaveLength(0);
  });

  it("viewers_read_only", async () => {
    const { user } = renderEditor({ me: meAs("viewer", ["reporter"]) });
    await user.dblClick(await cell("45000"));
    expect(screen.queryByRole("textbox", { name: "amount_cents" })).toBeNull();
    expect(screen.queryByRole("button", { name: "Insert row" })).toBeNull();
  });

  it("data_api_off_gates_rows_and_offers_enable", async () => {
    const enable = vi.fn(() => jsonResponse({ status: "accepted" }, 202));
    const { user } = renderEditor({
      me: meAs("developer", ["billing_admin"]),
      params: { ...FULL, pgDatabase: "reporting" },
      routes: { [`POST ${B}/pg-databases/reporting/data-api`]: enable },
    });
    expect(await screen.findByTestId("rows-gate")).toHaveTextContent("Rows need the data API on reporting");
    expect(screen.queryByRole("grid")).toBeNull();
    await user.click(screen.getByRole("button", { name: "Enable data API" }));
    await vi.waitFor(() => expect(enable).toHaveBeenCalled());
  });

  it("selection_page_then_all_matching", async () => {
    const { user } = renderEditor();
    await cell("45000");
    await user.click(screen.getByRole("checkbox", { name: "Select all rows on this page" }));
    expect(screen.getByText("3 selected")).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Select all ≈ 48,210 matching rows" }));
    expect(screen.getByText("All ≈ 48,210 matching rows selected")).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Clear selection" }));
    expect(screen.queryByText(/selected/)).toBeNull();
  });

  it("delete_selected_sends_one_batch_by_key", async () => {
    const { f, user } = renderEditor();
    await cell("45000");
    await user.click(screen.getByRole("checkbox", { name: "Select row 1" }));
    await user.click(screen.getByRole("checkbox", { name: "Select row 3" }));
    await user.click(screen.getByRole("button", { name: "Delete" }));
    const dialog = await screen.findByRole("dialog", { name: "Delete 2 rows?" });
    await user.click(within(dialog).getByRole("button", { name: "Delete 2 rows" }));
    await vi.waitFor(async () => expect(await writes(f)).toHaveLength(1));
    expect((await writes(f))[0].tree).toEqual({ v: 1, op: "batch", ops: [
      { op: "delete", from: "public.invoices", where: { col: "id", eq: "3f9a-1" } },
      { op: "delete", from: "public.invoices", where: { col: "id", eq: "c2d1-3" } },
    ] });
  });

  it("delete_all_matching_without_a_filter_needs_the_table_name", async () => {
    const { f, user } = renderEditor();
    await cell("45000");
    await user.click(screen.getByRole("checkbox", { name: "Select all rows on this page" }));
    await user.click(screen.getByRole("button", { name: "Select all ≈ 48,210 matching rows" }));
    await user.click(screen.getByRole("button", { name: "Delete" }));
    const dialog = await screen.findByRole("dialog", { name: "Delete all matching rows?" });
    const confirm = within(dialog).getByRole("button", { name: "Delete all rows" });
    expect(confirm).toBeDisabled();
    await user.type(within(dialog).getByLabelText("Type invoices to confirm"), "invoices");
    await user.click(confirm);
    await vi.waitFor(async () => expect(await writes(f)).toHaveLength(1));
    expect((await writes(f))[0].tree).toEqual({ v: 1, op: "delete", from: "public.invoices", all: true });
  });

  it("delete_all_matching_with_a_filter_uses_it", async () => {
    const { f, user } = renderEditor();
    await cell("45000");
    await user.click(screen.getByRole("button", { name: "Filter" }));
    await user.click(screen.getByLabelText("Filter column"));
    await user.click(await screen.findByRole("option", { name: "status" }));
    await user.click(screen.getByLabelText("Filter operator"));
    await user.click(await screen.findByRole("option", { name: "eq" }));
    await user.type(screen.getByLabelText("Filter value"), "void");
    await user.click(screen.getByRole("button", { name: "Apply filter" }));
    await vi.waitFor(async () => expect((await selects(f)).at(-1).tree.where).toEqual({ col: "status", eq: "void" }));
    await user.click(screen.getByRole("checkbox", { name: "Select all rows on this page" }));
    await user.click(screen.getByRole("button", { name: /^Select all ≈ .* matching rows$/ }));
    await user.click(screen.getByRole("button", { name: "Delete" }));
    const dialog = await screen.findByRole("dialog", { name: "Delete all matching rows?" });
    expect(within(dialog).queryByLabelText("Type invoices to confirm")).toBeNull();
    await user.click(within(dialog).getByRole("button", { name: "Delete all rows" }));
    await vi.waitFor(async () => expect(await writes(f)).toHaveLength(1));
    expect((await writes(f))[0].tree).toEqual({ v: 1, op: "delete", from: "public.invoices", where: { col: "status", eq: "void" } });
  });

  it("copy_as_tsv_and_json", async () => {
    const { user } = renderEditor();
    await cell("45000");
    await user.click(screen.getByRole("checkbox", { name: "Select row 2" }));
    await user.click(screen.getByRole("button", { name: "Copy" }));
    expect(await navigator.clipboard.readText()).toBe("id\tcustomer_id\tstatus\tamount_cents\tmemo\n81c4-2\tc-2\topen\t45000\tQ3 plan");
    await user.click(screen.getByRole("button", { name: "Copy as JSON" }));
    expect(JSON.parse(await navigator.clipboard.readText())).toEqual([ROWS[1]]);
  });

  it("too_large_halves_the_page", async () => {
    const answer: FakeHandler = async (req) => {
      const b = await req.clone().json();
      return b.tree.limit > 25 ? jsonResponse({ error: { code: "too_large", message: "big" } }, 413) : jsonResponse(ROWS);
    };
    const { f } = renderEditor({ routes: { [`POST ${Q}`]: answer } });
    expect(await cell("45000")).toBeInTheDocument();
    expect((await selects(f)).map((b) => b.tree.limit)).toEqual([50, 25]);
  });

  it("long_values_are_cut_to_200_characters", async () => {
    renderEditor({ rows: [{ ...ROWS[0], memo: "x".repeat(500) }] });
    expect(await cell(`${"x".repeat(200)}…`)).toBeInTheDocument();
  });
});

```

**Gotchas:** cells' accessible names are their displayed text (`aria-label` or text content); NULL shows as `NULL` (chalk-dim). `user.dblClick` on a cell and `Enter` on the active cell both start editing. jsdom virtualization: `stubLayout()`; render rows via `@tanstack/react-virtual` with an `initialRect`. `navigator.clipboard` is provided by `userEvent.setup()`. The Filter popover may reuse `console-filters.tsx` only if its labels can be the pinned ones. Numbers in `set` go through `parseValue` (bigint columns stay numbers when safe). Too-large detection: `ApiError.status === 413 || code === "too_large"`.

**Verify:** `pnpm --filter @chelabase/studio exec vitest run src/components/table-editor src/lib` and `pnpm --filter @chelabase/studio typecheck`.

**Live runs (the plan's one stack):** none.

---

## T4: the row panel (sonnet / none — leaf, the end-of-step review covers it)

**Files:** create `apps/studio/src/components/table-editor/{row-panel,fk-picker}.tsx` and `apps/studio/src/lib/table-editor/oversized.ts` (`export const LARGE_TYPES: ReadonlySet<string>`; `export function smallColumns(t: Table): string[]`); fill the row branch of `panel.tsx`; in `rows-view.tsx` add the gutter's open-row button, `Insert row` and its menu, and the oversized-row fallback (the coordinator resolves merges with T5 in `panel.tsx`). Tests (coordinator): `apps/studio/src/components/table-editor/row-panel.test.tsx`.

**Behaviour:** spec §3 "Row panel" and "Large values"; `row-panel.html`. Panel is `<aside role="complementary" aria-label="Row">` (`aria-label="New row"` when inserting). Each field is labelled by the column name. Gutter button `Open row {n}`. NULL toggle is a switch `Set {column} to NULL`. `Now` button `Set {column} to now`. FK: `Pick {column}` opens a dialog `Pick from {table}` listing up to 20 rows of the referenced table as options; `Open {column}` navigates to `{ ...params, table: referenced, }` (filter by key is a search param left for later; P-note in the Status if the implementer can't pass it). Buttons `Save row`, `Cancel`, `Delete row` (confirm dialog `Delete this row?` → `Delete row`). `Insert row` opens the empty panel; its menu button `More insert options` → item `Insert with all defaults`. Field filter `Filter fields` above 30 columns. Panel header button `Collapse panel`; collapsed strip button `Expand panel`. Oversized: when a page of 1 row is still `too_large`, read with `select: smallColumns(table)`; large cells show button `Load value` (one per cell, named `Load value of {column}`), which calls `useGetCell` and shows the value.

**RED tests (coordinator)** — `apps/studio/src/components/table-editor/row-panel.test.tsx`:

```tsx
import { screen, within } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { jsonResponse, type FakeHandler } from "@/test/fake-fetch";
import { B, ROWS, T, bodies, invoices, renderEditor, stubLayout } from "@/test/table-editor-fixtures";

const Q = `${B}/console/query`;
const writes = async (f: Parameters<typeof bodies>[0]) => (await bodies(f, "POST", Q)).filter((b) => b.tree.op !== "select");
const open = async (user: ReturnType<typeof renderEditor>["user"], n = 2) => {
  await user.click(await screen.findByRole("button", { name: `Open row ${n}` }));
  return screen.findByRole("complementary", { name: "Row" });
};

describe("row panel", () => {
  beforeEach(() => { localStorage.clear(); sessionStorage.clear(); stubLayout(); });

  it("saves_only_changed_fields", async () => {
    const { f, user } = renderEditor();
    const panel = await open(user);
    expect(within(panel).getByLabelText("id")).toHaveAttribute("readonly");
    const amount = within(panel).getByLabelText("amount_cents");
    await user.clear(amount);
    await user.type(amount, "47000");
    await user.click(within(panel).getByLabelText("status"));
    await user.click(await screen.findByRole("option", { name: "paid" }));
    await user.click(within(panel).getByRole("button", { name: "Save row" }));
    await vi.waitFor(async () => expect(await writes(f)).toHaveLength(1));
    expect((await writes(f))[0].tree).toEqual({ v: 1, op: "update", table: "public.invoices", set: { amount_cents: 47000, status: "paid" }, where: { col: "id", eq: "81c4-2" }, returning: "*" });
  });

  it("null_toggle_sends_null", async () => {
    const { f, user } = renderEditor();
    const panel = await open(user);
    await user.click(within(panel).getByRole("switch", { name: "Set memo to NULL" }));
    await user.click(within(panel).getByRole("button", { name: "Save row" }));
    await vi.waitFor(async () => expect(await writes(f)).toHaveLength(1));
    expect((await writes(f))[0].tree.set).toEqual({ memo: null });
  });

  it("insert_sends_only_filled_fields", async () => {
    const { f, user } = renderEditor();
    await user.click(await screen.findByRole("button", { name: "Insert row" }));
    const panel = await screen.findByRole("complementary", { name: "New row" });
    expect(within(panel).getByLabelText("id")).toHaveAttribute("placeholder", "default");
    await user.type(within(panel).getByLabelText("customer_id"), "c-9");
    await user.type(within(panel).getByLabelText("amount_cents"), "100");
    await user.click(within(panel).getByRole("button", { name: "Save row" }));
    await vi.waitFor(async () => expect(await writes(f)).toHaveLength(1));
    expect((await writes(f))[0].tree).toEqual({ v: 1, op: "insert", into: "public.invoices", values: [{ customer_id: "c-9", amount_cents: 100 }], returning: "*" });
  });

  it("insert_with_all_defaults", async () => {
    const insert = vi.fn(() => jsonResponse({ row: { ...ROWS[0], id: "new-1" } }));
    const { user } = renderEditor({ routes: { [`POST ${T}/rows/default`]: insert } });
    await user.click(await screen.findByRole("button", { name: "More insert options" }));
    await user.click(await screen.findByRole("menuitem", { name: "Insert with all defaults" }));
    await vi.waitFor(() => expect(insert).toHaveBeenCalledTimes(1));
    const req = (insert.mock.calls[0] as unknown as [Request])[0];
    expect(await req.clone().json()).toEqual({ database: null, role: null });
  });

  it("fk_pick_fills_the_field", async () => {
    const answer: FakeHandler = async (req) => {
      const b = await req.clone().json();
      if (b.tree.from === "public.customers") return jsonResponse([{ id: "c-7", name: "Lagoon Ltd" }]);
      return jsonResponse(ROWS);
    };
    const { f, user } = renderEditor({ routes: { [`POST ${Q}`]: answer } });
    const panel = await open(user);
    await user.click(within(panel).getByRole("button", { name: "Pick customer_id" }));
    const dialog = await screen.findByRole("dialog", { name: "Pick from customers" });
    await user.click(await within(dialog).findByRole("option", { name: /c-7/ }));
    expect(within(panel).getByLabelText("customer_id")).toHaveValue("c-7");
    const pick = (await bodies(f, "POST", Q)).find((b) => b.tree.from === "public.customers");
    expect(pick.tree.limit).toBe(20);
  });

  it("delete_row_confirms", async () => {
    const { f, user } = renderEditor();
    const panel = await open(user, 3);
    await user.click(within(panel).getByRole("button", { name: "Delete row" }));
    const dialog = await screen.findByRole("dialog", { name: "Delete this row?" });
    await user.click(within(dialog).getByRole("button", { name: "Delete row" }));
    await vi.waitFor(async () => expect(await writes(f)).toHaveLength(1));
    expect((await writes(f))[0].tree).toEqual({ v: 1, op: "delete", from: "public.invoices", where: { col: "id", eq: "c2d1-3" } });
  });

  it("collapse_keeps_the_draft", async () => {
    const { user } = renderEditor();
    const panel = await open(user);
    await user.type(within(panel).getByLabelText("memo"), " more");
    await user.click(within(panel).getByRole("button", { name: "Collapse panel" }));
    expect(screen.queryByRole("complementary", { name: "Row" })).toBeNull();
    await user.click(screen.getByRole("button", { name: "Expand panel" }));
    expect(within(await screen.findByRole("complementary", { name: "Row" })).getByLabelText("memo")).toHaveValue("Q3 plan more");
  });

  it("field_filter_on_wide_tables", async () => {
    const wide = invoices();
    wide.columns = [...wide.columns, ...Array.from({ length: 30 }, (_, i) => ({ ...wide.columns[4], attnum: 6 + i, name: `extra_${i}` }))];
    const { user } = renderEditor({ table: wide });
    const panel = await open(user);
    await user.type(within(panel).getByLabelText("Filter fields"), "extra_2");
    expect(within(panel).queryByLabelText("memo")).toBeNull();
    expect(within(panel).getByLabelText("extra_2")).toBeInTheDocument();
  });

  it("oversized_rows_fall_back_to_small_columns_and_load_value", async () => {
    const answer: FakeHandler = async (req) => {
      const b = await req.clone().json();
      if (b.tree.op !== "select") return jsonResponse([]);
      return b.tree.select ? jsonResponse([{ id: "3f9a-1", customer_id: "c-1", status: "paid", amount_cents: 12900 }]) : jsonResponse({ error: { code: "too_large", message: "big" } }, 413);
    };
    const cellFetch = vi.fn(() => jsonResponse({ value: "very long memo", bytes: 9000000, data_type: "text" }));
    const { f, user } = renderEditor({ routes: { [`POST ${Q}`]: answer, [`POST ${T}/cell`]: cellFetch } });
    await user.click(await screen.findByRole("button", { name: "Load value of memo" }));
    expect(await screen.findByText("very long memo")).toBeInTheDocument();
    const req = (cellFetch.mock.calls[0] as unknown as [Request])[0];
    expect(await req.clone().json()).toEqual({ database: null, role: null, key: { id: "3f9a-1" }, column: "memo" });
    const last = (await bodies(f, "POST", Q)).at(-1);
    expect(last.tree.select).toEqual(["id", "customer_id", "status", "amount_cents"]);
    expect(last.tree.limit).toBe(1);
  });
});
```

**Gotchas:** `smallColumns` keeps every column whose `data_type` (base name, `[]` stripped) is not in `LARGE_TYPES` (`text`, `bytea`, `json`, `jsonb`, `xml`) and that isn't an array, always keeping primary-key columns. Inserts omit untouched fields so defaults apply. The FK picker reads the referenced table as the same role.

**Verify:** `pnpm --filter @chelabase/studio exec vitest run src/components/table-editor` and `pnpm --filter @chelabase/studio typecheck`.

**Live runs (the plan's one stack):** none.

---

## T5: structure, column form, changes panel (opus / opus)

**Files:** create `apps/studio/src/components/table-editor/{column-strip,column-form,changes-panel,rls-banner,use-dry-run}.tsx|ts`; fill `structure-view.tsx` (sections; T6 adds constraint/table actions) and the `changes`/`column` branches of `panel.tsx`; add the strip above the grid in `rows-view.tsx` sharing its column template and scroll. Tests (coordinator): `apps/studio/src/components/table-editor/{structure,changes-panel}.test.tsx`, `apps/studio/src/components/table-editor/use-dry-run.test.tsx`.

**Behaviour:** spec §2 (dry run, Apply, re-read), §4; `add-column.html`, `grid-edges.html`. `useDryRun` signature:

```ts
export function useDryRun(args: { databaseId: string; branchId: string; database: string | null; role: string | null; edits: Edit[]; table: string; totalBytes: number | null; enabled: boolean }):
  { view: DryRunView | null; failure: ChangeFailure | null; running: boolean };
```

It waits 400 ms after the last change of `edits` (by ops), and not while an edit dialog is open (`paused`, ruling 3b), aborts the previous request, and posts `{ database, role, dry_run: true, operations: opsOf(edits) }`. The column strip: `<div role="list" aria-label="Columns">`, one `listitem` per projected column containing a button named `${name} ${data_type}` (`, new`, `, changed`, `, dropped` appended for marks), a menu button `Column actions for ${name}` with items `Rename`, `Change type`, `Default`, `Not null`, `Comment`, `Drop`; then a button `Add column`. The column form panel: `<aside role="complementary" aria-label="New column">` (`Edit column ${name}` when editing) with fields `Name`, `Type` (combobox of the catalog; options named `int8`, `public.invoice_status`, …), `Array` checkbox, a radio group `Default` (`None`, `Value`, `Expression`) with input `Default value`/`Default expression`, checkboxes `Not null`, `Unique`, `Primary key`, buttons `References…`, `Check…`, input `Comment`, a `SQL preview` region showing the dry run of the form's ops alone, and `Cancel`, `Add to changes`. The changes panel: `<aside role="complementary" aria-label="Pending changes">`, heading `Pending changes · {n}`, an ordered list of edits (each a button with the label; per-edit status text `Error: {message}` or the skip text; a `Remove {label}` button), the SQL (`role="region" aria-label="SQL"`), warnings, dependents (`Dependents` list) with checkbox `Drop dependents too (cascade)`, checkbox `I've read the SQL` when `apply === "needs_ack"`, `Discard` (confirm dialog `Discard {n} changes?` → `Discard`), `Apply {n} change(s)` (`Apply 1 change`, `Apply 2 changes`). After Apply: `role="status"` text `Applied {n} change(s) to {schema}.{table}`. `lock_timeout`: a `Try again` button. `branch_protected`: the error message. Fingerprint mismatch: `This table changed since you started` with a button `Review again` (rebases and re-runs the dry run). RLS banner (`role="alert"`): `RLS is off: any role with grants reads and writes every row` + `Turn RLS on`; with RLS on and no policies, a note `RLS is on with no policies: only the owner and roles that bypass RLS see rows.` The table bar's `Review {n} change(s)` button opens the changes panel when another panel is open. Structure tab: headings `Columns`, `Constraints`, `Indexes`, `Policies`, `Grants`, `Table`; the last three read-only lists carry `Read-only here for now`. Edits are disabled with `structureAccess`'s reason.

**RED tests (coordinator)**

`apps/studio/src/components/table-editor/use-dry-run.test.tsx`:

```tsx
import { act, renderHook } from "@testing-library/react";
import type { ReactNode } from "react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { configureManagementClient } from "@chelabase/management-client";
import { fakeFetch } from "@/test/fake-fetch";
import { B, schemaChanges } from "@/test/table-editor-fixtures";
import { newEdit, type Edit } from "@/lib/table-editor/changeset";
import { useDryRun } from "./use-dry-run";

const T = { schema: "public", name: "invoices" };
const add = (name: string) => newEdit([{ add_column: { table: T, column: { name, data_type: { schema: "pg_catalog", name: "int8" } } } }]);

describe("useDryRun", () => {
  beforeEach(() => vi.useFakeTimers({ shouldAdvanceTime: true }));
  afterEach(() => vi.useRealTimers());
  it("debounces_400ms_and_posts_the_ops", async () => {
    const f = fakeFetch({ [`POST ${B}/schema-changes`]: schemaChanges });
    configureManagementClient({ baseUrl: "http://studio.test", getToken: () => "t", onUnauthorized: () => false, fetchImpl: f });
    const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    const wrapper = ({ children }: { children: ReactNode }) => <QueryClientProvider client={qc}>{children}</QueryClientProvider>;
    const props = (edits: Edit[]) => ({ databaseId: "d1", branchId: "b1", database: null, role: "billing_admin", edits, table: "public.invoices", totalBytes: 100, enabled: true });
    const { result, rerender } = renderHook((p: ReturnType<typeof props>) => useDryRun(p), { wrapper, initialProps: props([add("a")]) });
    await act(async () => { vi.advanceTimersByTime(200); });
    rerender(props([add("a"), add("b")]));
    await act(async () => { vi.advanceTimersByTime(399); });
    expect(f.calls).toHaveLength(0);
    await act(async () => { vi.advanceTimersByTime(1); });
    await vi.waitFor(() => expect(result.current.view?.apply).toBe("ready"));
    expect(f.calls).toHaveLength(1);
    const body = await f.calls[0].clone().json();
    expect(body).toMatchObject({ dry_run: true, role: "billing_admin", database: null });
    expect(body.operations).toHaveLength(2);
  });
});
```

`apps/studio/src/components/table-editor/structure.test.tsx`:

```tsx
import { screen, within } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { B, bodies, invoices, meAs, renderEditor, stubLayout } from "@/test/table-editor-fixtures";

const SC = `${B}/schema-changes`;

describe("column strip and structure", () => {
  beforeEach(() => { localStorage.clear(); sessionStorage.clear(); stubLayout(); });

  it("strip_lists_typed_columns", async () => {
    renderEditor();
    const strip = await screen.findByRole("list", { name: "Columns" });
    expect(within(strip).getByRole("button", { name: "amount_cents bigint" })).toBeInTheDocument();
    expect(within(strip).getByRole("button", { name: "status invoice_status" })).toBeInTheDocument();
    expect(within(strip).getByRole("button", { name: "Add column" })).toBeInTheDocument();
  });

  it("add_column_flows_into_the_changes_panel", async () => {
    const { f, user } = renderEditor();
    await user.click(await screen.findByRole("button", { name: "Add column" }));
    const form = await screen.findByRole("complementary", { name: "New column" });
    await user.type(within(form).getByLabelText("Name"), "discount_cents");
    await user.click(within(form).getByLabelText("Type"));
    await user.click(await screen.findByRole("option", { name: "int8" }));
    await user.click(within(form).getByRole("radio", { name: "Value" }));
    await user.type(within(form).getByLabelText("Default value"), "0");
    await user.click(within(form).getByRole("checkbox", { name: "Not null" }));
    await vi.waitFor(async () => expect((await bodies(f, "POST", SC)).length).toBeGreaterThan(0), { timeout: 2000 });
    expect(within(within(form).getByRole("region", { name: "SQL preview" })).getByText("SQL 0")).toBeInTheDocument();
    await user.click(within(form).getByRole("button", { name: "Add to changes" }));
    const panel = await screen.findByRole("complementary", { name: "Pending changes" });
    expect(within(panel).getByRole("heading", { name: "Pending changes · 1" })).toBeInTheDocument();
    expect(within(panel).getByRole("button", { name: "Add column discount_cents int8" })).toBeInTheDocument();
    const strip = screen.getByRole("list", { name: "Columns" });
    expect(within(strip).getByRole("button", { name: "discount_cents int8, new" })).toBeInTheDocument();
    const apply = within(panel).getByRole("button", { name: "Apply 1 change" });
    await vi.waitFor(() => expect(apply).toBeEnabled(), { timeout: 2000 });
    await user.click(apply);
    expect(await screen.findByRole("status")).toHaveTextContent("Applied 1 change to public.invoices");
    const real = (await bodies(f, "POST", SC)).filter((b) => !b.dry_run);
    expect(real).toEqual([{ database: null, role: null, dry_run: false, operations: [
      { add_column: { table: { schema: "public", name: "invoices" }, column: { name: "discount_cents", data_type: { schema: "pg_catalog", name: "int8", array: false }, not_null: true, default: "'0'", comment: null } } },
    ] }]);
    expect(screen.queryByRole("heading", { name: /Pending changes/ })).toBeNull();
  });

  it("edit_existing_column_from_its_chip", async () => {
    const { user } = renderEditor();
    await user.click(await screen.findByRole("button", { name: "memo text" }));
    const form = await screen.findByRole("complementary", { name: "Edit column memo" });
    const name = within(form).getByLabelText("Name");
    await user.clear(name);
    await user.type(name, "note");
    await user.click(within(form).getByRole("button", { name: "Add to changes" }));
    const panel = await screen.findByRole("complementary", { name: "Pending changes" });
    expect(within(panel).getByRole("button", { name: "Rename column memo → note" })).toBeInTheDocument();
    expect(within(screen.getByRole("list", { name: "Columns" })).getByRole("button", { name: "note text, changed" })).toBeInTheDocument();
  });

  it("drop_from_the_chip_menu_shows_dependents_and_needs_cascade", async () => {
    const dry = async (req: Request) => {
      const b = await req.clone().json();
      if (b.dry_run) return new Response(JSON.stringify({ dry_run: true, committed: false, would_succeed: true, statements: [{ sql: "ALTER TABLE public.invoices DROP COLUMN memo", op_index: 0, executed: true, lock: "access_exclusive", notices: [], rewrites_table: false, skipped: null }], dependents: { hash: "h1", objects: [{ kind: "view", name: "v_open", schema: "public", description: "view public.v_open" }] } }), { status: 200, headers: { "Content-Type": "application/json" } });
      return new Response(JSON.stringify({ dry_run: false, committed: true, statements: [] }), { status: 200, headers: { "Content-Type": "application/json" } });
    };
    const { f, user } = renderEditor({ routes: { [`POST ${SC}`]: dry } });
    await user.click(await screen.findByRole("button", { name: "Column actions for memo" }));
    await user.click(await screen.findByRole("menuitem", { name: "Drop" }));
    const panel = await screen.findByRole("complementary", { name: "Pending changes" });
    expect(await within(panel).findByText("view public.v_open")).toBeInTheDocument();
    const apply = within(panel).getByRole("button", { name: "Apply 1 change" });
    expect(apply).toBeDisabled();
    await user.click(within(panel).getByRole("checkbox", { name: "Drop dependents too (cascade)" }));
    await vi.waitFor(() => expect(apply).toBeEnabled());
    await user.click(apply);
    await vi.waitFor(async () => expect((await bodies(f, "POST", SC)).filter((b) => !b.dry_run)).toHaveLength(1));
    expect((await bodies(f, "POST", SC)).filter((b) => !b.dry_run)[0].cascade).toEqual({ dependents_hash: "h1" });
  });

  it("structure_tab_sections_and_read_only_lists", async () => {
    const { user } = renderEditor();
    await user.click(await screen.findByRole("tab", { name: "Structure" }));
    for (const h of ["Columns", "Constraints", "Indexes", "Policies", "Grants", "Table"]) {
      expect(screen.getByRole("heading", { name: h })).toBeInTheDocument();
    }
    expect(screen.getAllByText("Read-only here for now")).toHaveLength(3);
    expect(screen.getByText("invoices_customer_id_fkey")).toBeInTheDocument();
  });

  it("rls_off_banner_stages_turn_on", async () => {
    const { user } = renderEditor({ table: invoices({ rls_enabled: false }) });
    const banner = await screen.findByRole("alert");
    expect(banner).toHaveTextContent("RLS is off: any role with grants reads and writes every row");
    await user.click(within(banner).getByRole("button", { name: "Turn RLS on" }));
    const panel = await screen.findByRole("complementary", { name: "Pending changes" });
    expect(within(panel).getByRole("button", { name: "Turn RLS on" })).toBeInTheDocument();
  });

  it("rls_on_without_policies_note", async () => {
    renderEditor();
    expect(await screen.findByText("RLS is on with no policies: only the owner and roles that bypass RLS see rows.")).toBeInTheDocument();
  });

  it("protected_branch_disables_structure_edits_for_developers", async () => {
    renderEditor({ me: meAs("developer", ["billing_admin"]) });
    expect(await screen.findByRole("button", { name: "Add column" })).toBeDisabled();
    expect(screen.getByTestId("structure-reason")).toHaveTextContent("main is protected: schema changes need Admin");
  });
});
```

`apps/studio/src/components/table-editor/changes-panel.test.tsx`:

```tsx
import { screen, within } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { jsonResponse, sequence } from "@/test/fake-fetch";
import { B, bodies, invoices, renderEditor, stubLayout } from "@/test/table-editor-fixtures";
import { DRAFTS_KEY } from "@/lib/table-editor/store";
import { newEdit, tableFingerprint } from "@/lib/table-editor/changeset";

const SC = `${B}/schema-changes`;
const T = { schema: "public", name: "invoices" };
const K = "d1/b1/app/public.invoices";
const draft = (base: string, ...ops: Parameters<typeof newEdit>[0][]) =>
  sessionStorage.setItem(DRAFTS_KEY, JSON.stringify({ [K]: { key: K, base, edits: ops.map((o) => newEdit(o)) } }));
const dropMemo = [{ drop_column: { table: T, column: "memo" } }];
const alterAmount = [{ alter_column: { table: T, column: "amount_cents", change: { data_type: { schema: "pg_catalog", name: "text" } } } }];
const okDry = (over: Record<string, unknown> = {}) => jsonResponse({ dry_run: true, committed: false, would_succeed: true, statements: [{ sql: "SQL 0", op_index: 0, executed: true, lock: "access_exclusive", notices: [], rewrites_table: false, skipped: null }], ...over });
const panel = () => screen.findByRole("complementary", { name: "Pending changes" });

describe("changes panel", () => {
  beforeEach(() => { localStorage.clear(); sessionStorage.clear(); stubLayout(); });

  it("drafts_survive_a_reload", async () => {
    draft(tableFingerprint(invoices()), dropMemo);
    renderEditor();
    expect(within(await panel()).getByRole("button", { name: "Drop column memo" })).toBeInTheDocument();
  });

  it("an_error_lands_on_its_edit_and_blocks_apply", async () => {
    draft(tableFingerprint(invoices()), dropMemo, alterAmount);
    const err = () => jsonResponse({ error: { code: "schema_change_failed", message: "cannot cast bigint", op_index: 1, sqlstate: "42804", detail: null, hint: "use USING" } }, 400);
    renderEditor({ routes: { [`POST ${SC}`]: err } });
    const p = await panel();
    expect(await within(p).findByText("Error: cannot cast bigint")).toBeInTheDocument();
    expect(within(p).getByText("use USING")).toBeInTheDocument();
    expect(within(p).getByRole("button", { name: "Apply 2 changes" })).toBeDisabled();
  });

  it("not_simulated_needs_an_acknowledgement", async () => {
    draft(tableFingerprint(invoices()), alterAmount);
    const dry = () => jsonResponse({ dry_run: true, committed: false, would_succeed: null, statements: [{ sql: "SQL 0", op_index: 0, executed: false, lock: "access_exclusive", notices: [], rewrites_table: true, skipped: "large_table_rewrite" }] });
    const { user } = renderEditor({ routes: { [`POST ${SC}`]: dry } });
    const p = await panel();
    expect(await within(p).findByText("Rewrites public.invoices (12 MB): reads and writes wait while it runs")).toBeInTheDocument();
    expect(within(p).getByText("Not simulated: rewrites a large table")).toBeInTheDocument();
    const apply = within(p).getByRole("button", { name: "Apply 1 change" });
    expect(apply).toBeDisabled();
    await user.click(within(p).getByRole("checkbox", { name: "I've read the SQL" }));
    expect(apply).toBeEnabled();
  });

  it("dependents_changed_shows_the_new_list_and_unticks_cascade", async () => {
    draft(tableFingerprint(invoices()), dropMemo);
    const dry = sequence(
      () => okDry({ dependents: { hash: "h1", objects: [{ kind: "view", name: "v_open", schema: "public", description: "view public.v_open" }] } }),
      () => jsonResponse({ error: { code: "dependents_changed", message: "changed", dependents: { hash: "h2", objects: [{ kind: "view", name: "v_paid", schema: "public", description: "view public.v_paid" }] } } }, 409),
    );
    const { user } = renderEditor({ routes: { [`POST ${SC}`]: dry } });
    const p = await panel();
    await user.click(await within(p).findByRole("checkbox", { name: "Drop dependents too (cascade)" }));
    await user.click(within(p).getByRole("button", { name: "Apply 1 change" }));
    expect(await within(p).findByText("view public.v_paid")).toBeInTheDocument();
    expect(within(p).getByRole("checkbox", { name: "Drop dependents too (cascade)" })).not.toBeChecked();
  });

  it("lock_timeout_offers_try_again", async () => {
    draft(tableFingerprint(invoices()), dropMemo);
    const dry = sequence(() => okDry(), () => jsonResponse({ error: { code: "lock_timeout", message: "lock timeout", op_index: 0 } }, 409), () => jsonResponse({ dry_run: false, committed: true, statements: [] }));
    const { f, user } = renderEditor({ routes: { [`POST ${SC}`]: dry } });
    const p = await panel();
    const apply = within(p).getByRole("button", { name: "Apply 1 change" });
    await vi.waitFor(() => expect(apply).toBeEnabled());
    await user.click(apply);
    await user.click(await within(p).findByRole("button", { name: "Try again" }));
    expect(await screen.findByRole("status")).toHaveTextContent("Applied 1 change to public.invoices");
    expect((await bodies(f, "POST", SC)).filter((b) => !b.dry_run)).toHaveLength(2);
  });

  it("branch_protected_explains", async () => {
    draft(tableFingerprint(invoices()), dropMemo);
    const dry = sequence(() => okDry(), () => jsonResponse({ error: { code: "branch_protected", message: "branch is protected: schema changes need Admin" } }, 403));
    const { user } = renderEditor({ routes: { [`POST ${SC}`]: dry } });
    const p = await panel();
    const apply = within(p).getByRole("button", { name: "Apply 1 change" });
    await vi.waitFor(() => expect(apply).toBeEnabled());
    await user.click(apply);
    expect(await within(p).findByText("branch is protected: schema changes need Admin")).toBeInTheDocument();
  });

  it("a_changed_table_blocks_apply_until_reviewed", async () => {
    draft(tableFingerprint(invoices({ comment: "older" })), dropMemo);
    const { user } = renderEditor();
    const p = await panel();
    expect(await within(p).findByText("This table changed since you started")).toBeInTheDocument();
    expect(within(p).getByRole("button", { name: "Apply 1 change" })).toBeDisabled();
    await user.click(within(p).getByRole("button", { name: "Review again" }));
    await vi.waitFor(() => expect(within(p).getByRole("button", { name: "Apply 1 change" })).toBeEnabled());
  });

  it("remove_and_discard", async () => {
    draft(tableFingerprint(invoices()), dropMemo, alterAmount);
    const { user } = renderEditor();
    const p = await panel();
    await user.click(within(p).getByRole("button", { name: "Remove Drop column memo" }));
    expect(within(p).getByRole("heading", { name: "Pending changes · 1" })).toBeInTheDocument();
    await user.click(within(p).getByRole("button", { name: "Discard" }));
    await user.click(within(await screen.findByRole("dialog", { name: "Discard 1 change?" })).getByRole("button", { name: "Discard" }));
    expect(screen.queryByRole("complementary", { name: "Pending changes" })).toBeNull();
    expect(JSON.parse(sessionStorage.getItem(DRAFTS_KEY)!)).toEqual({});
  });

  it("collapse_keeps_pending_changes", async () => {
    draft(tableFingerprint(invoices()), dropMemo);
    const { user } = renderEditor();
    await user.click(within(await panel()).getByRole("button", { name: "Collapse panel" }));
    expect(screen.getByRole("button", { name: "Expand panel" })).toHaveAccessibleDescription(/Pending changes · 1/);
    await user.click(screen.getByRole("button", { name: "Expand panel" }));
    expect(within(await panel()).getByRole("button", { name: "Drop column memo" })).toBeInTheDocument();
  });
});
```

**Gotchas:** schema-change requests omit `cascade` and `lock_timeout_ms` unless set (the Apply test compares the body exactly). The panel only shows the changes view when the table's changeset is non-empty (or another panel kind is open). The SQL preview of a form runs `useDryRun` with a one-edit list built from `columnFormOps` (not stored). The collapsed strip's `Expand panel` button is described by the open view's title (`aria-describedby`). After Apply, invalidate `getGetTableQueryKey`, `getListObjectsQueryKey`, and the rows query. A reloaded draft's dry run runs as soon as the table is loaded.

**Verify:** `pnpm --filter @chelabase/studio exec vitest run src/components/table-editor src/lib` and `pnpm --filter @chelabase/studio typecheck`.

**Live runs (the plan's one stack):** none.

---

## T6: constraints, table operations, new table, types (sonnet / none — leaf, the end-of-step review covers it)

**Files:** create `apps/studio/src/components/table-editor/{constraints-section,table-actions,new-table,types-view}.tsx`; mount them in `structure-view.tsx` and `table-editor.tsx` (the `Types` and `New table` buttons of the table list open their views via the store: add `view: "table" | "types" | "new_table"` to `TableEditorState` with `setView(v)`; default `"table"`, reset to `"table"` on table change). Tests (coordinator): `apps/studio/src/components/table-editor/{constraints,new-table,types-view}.test.tsx`.

**Behaviour:** spec §4 (Structure, Types, New table). Constraints section: button `Add constraint` opens a form dialog `Add constraint` with `Kind` select (`Primary key`, `Unique`, `Foreign key`, `Check`), `Columns` (multi combobox), for FK `References table`, `References column`, `On delete`, for check `Expression`, checkbox `Add as NOT VALID`, input `Name` (optional), button `Add to changes`. Each constraint row has `Rename ${name}`, `Drop ${name}` and, when not validated, `Validate ${name}`. Table section: `Rename table`, `Move to schema`, `Comment`, `Drop table` (each a small dialog with `Add to changes`). New table view: heading `New table`, inputs `Table name`, `Schema` (default the current), starter column rows (`id`, `created_at`) editable and removable (`Remove column id`), `Add column`, input `Table comment`, button `Create table` (adds the `create_table` edit to the `#new` changeset and opens the changes panel); after a successful Apply, navigate to the new table. Types view: heading `Types in {schema}`, one section per enum (heading = name) with its values, buttons `New enum` (dialog: `Enum name`, `Values` one per line), `Add value to ${name}` (dialog: `Value`, `Position` select `At the end`/`Before…`/`After…`, `Relative to`), `Rename value in ${name}` (`From`, `To`), `Rename ${name}`, `Drop ${name}`.

**RED tests (coordinator)**

`apps/studio/src/components/table-editor/constraints.test.tsx`:

```tsx
import { screen, within } from "@testing-library/react";
import { beforeEach, describe, expect, it } from "vitest";
import { invoices, renderEditor, stubLayout } from "@/test/table-editor-fixtures";

const opsIn = async () => {
  const p = await screen.findByRole("complementary", { name: "Pending changes" });
  return within(p).getAllByRole("listitem").map((li) => li.textContent);
};

describe("constraints and table actions", () => {
  beforeEach(() => { localStorage.clear(); sessionStorage.clear(); stubLayout(); });

  async function structure() {
    const r = renderEditor({ table: invoices({ constraints: [...invoices().constraints, { name: "amount_positive", kind: "check", columns: [4], definition: "CHECK (amount_cents > 0) NOT VALID", deferrable: false, initially_deferred: false, oid: 3, references: null, validated: false }] }) });
    await r.user.click(await screen.findByRole("tab", { name: "Structure" }));
    return r;
  }

  it("add_unique_constraint", async () => {
    const { user } = await structure();
    await user.click(screen.getByRole("button", { name: "Add constraint" }));
    const d = await screen.findByRole("dialog", { name: "Add constraint" });
    await user.click(within(d).getByLabelText("Kind"));
    await user.click(await screen.findByRole("option", { name: "Unique" }));
    await user.click(within(d).getByLabelText("Columns"));
    await user.click(await screen.findByRole("option", { name: /^memo/ }));
    await user.keyboard("{Escape}");
    await user.click(within(d).getByRole("button", { name: "Add to changes" }));
    expect(await opsIn()).toEqual([expect.stringContaining("Add unique constraint on invoices")]);
  });

  it("add_foreign_key_not_valid", async () => {
    const { user } = await structure();
    await user.click(screen.getByRole("button", { name: "Add constraint" }));
    const d = await screen.findByRole("dialog", { name: "Add constraint" });
    await user.click(within(d).getByLabelText("Kind"));
    await user.click(await screen.findByRole("option", { name: "Foreign key" }));
    await user.click(within(d).getByLabelText("Columns"));
    await user.click(await screen.findByRole("option", { name: /^customer_id/ }));
    await user.keyboard("{Escape}");
    await user.click(within(d).getByLabelText("References table"));
    await user.click(await screen.findByRole("option", { name: "customers" }));
    await user.click(within(d).getByLabelText("References column"));
    await user.click(await screen.findByRole("option", { name: "id" }));
    await user.click(within(d).getByRole("checkbox", { name: "Add as NOT VALID" }));
    await user.click(within(d).getByRole("button", { name: "Add to changes" }));
    expect(await opsIn()).toEqual([expect.stringContaining("Add foreign key on invoices")]);
  });

  it("validate_rename_and_drop_constraints", async () => {
    const { user } = await structure();
    await user.click(screen.getByRole("button", { name: "Validate amount_positive" }));
    await user.click(screen.getByRole("button", { name: "Drop invoices_customer_id_fkey" }));
    await user.click(screen.getByRole("button", { name: "Rename amount_positive" }));
    const d = await screen.findByRole("dialog", { name: "Rename amount_positive" });
    await user.type(within(d).getByLabelText("New name"), "amount_gt_zero");
    await user.click(within(d).getByRole("button", { name: "Add to changes" }));
    const items = await opsIn();
    expect(items).toEqual([
      expect.stringContaining("Validate amount_positive"),
      expect.stringContaining("Drop constraint invoices_customer_id_fkey"),
      expect.stringContaining("Rename constraint amount_positive → amount_gt_zero"),
    ]);
  });

  it("table_rename_move_comment_drop", async () => {
    const { user } = await structure();
    await user.click(screen.getByRole("button", { name: "Rename table" }));
    let d = await screen.findByRole("dialog", { name: "Rename table" });
    await user.type(within(d).getByLabelText("New name"), "bills");
    await user.click(within(d).getByRole("button", { name: "Add to changes" }));
    await user.click(screen.getByRole("button", { name: "Comment" }));
    d = await screen.findByRole("dialog", { name: "Comment" });
    await user.type(within(d).getByLabelText("Comment"), "Customer invoices");
    await user.click(within(d).getByRole("button", { name: "Add to changes" }));
    expect(await opsIn()).toEqual([
      expect.stringContaining("Rename table invoices → bills"),
      expect.stringContaining("Change comment on invoices"),
    ]);
  });
});
```

`apps/studio/src/components/table-editor/new-table.test.tsx`:

```tsx
import { screen, within } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { B, FULL, bodies, renderEditor, stubLayout } from "@/test/table-editor-fixtures";

const SC = `${B}/schema-changes`;

describe("new table", () => {
  beforeEach(() => { localStorage.clear(); sessionStorage.clear(); stubLayout(); });

  it("creates_a_table_with_starter_columns_rls_off", async () => {
    const { f, user, navigate } = renderEditor();
    await user.click(await screen.findByRole("button", { name: "New table" }));
    const view = await screen.findByRole("region", { name: "New table" });
    await user.type(within(view).getByLabelText("Table name"), "payments");
    expect(within(view).getByText("id")).toBeInTheDocument();
    expect(within(view).getByText("created_at")).toBeInTheDocument();
    await user.click(within(view).getByRole("button", { name: "Create table" }));
    const panel = await screen.findByRole("complementary", { name: "Pending changes" });
    expect(within(panel).getByRole("button", { name: "Create table public.payments" })).toBeInTheDocument();
    const apply = within(panel).getByRole("button", { name: "Apply 1 change" });
    await vi.waitFor(() => expect(apply).toBeEnabled(), { timeout: 2000 });
    await user.click(apply);
    await vi.waitFor(async () => expect((await bodies(f, "POST", SC)).filter((b) => !b.dry_run)).toHaveLength(1));
    const op = (await bodies(f, "POST", SC)).filter((b) => !b.dry_run)[0].operations[0];
    expect(op).toEqual({ create_table: {
      table: { schema: "public", name: "payments" },
      columns: [
        { name: "id", data_type: { schema: "pg_catalog", name: "int8", array: false }, identity: "always", not_null: true },
        { name: "created_at", data_type: { schema: "pg_catalog", name: "timestamptz", array: false }, not_null: true, default: "now()" },
      ],
      primary_key: ["id"],
      comment: null,
    } });
    await vi.waitFor(() => expect(navigate).toHaveBeenLastCalledWith({ ...FULL, table: "payments" }));
  });

  it("starter_columns_can_be_removed", async () => {
    const { user } = renderEditor();
    await user.click(await screen.findByRole("button", { name: "New table" }));
    const view = await screen.findByRole("region", { name: "New table" });
    await user.click(within(view).getByRole("button", { name: "Remove column created_at" }));
    expect(within(view).queryByText("created_at")).toBeNull();
  });
});
```

`apps/studio/src/components/table-editor/types-view.test.tsx`:

```tsx
import { screen, within } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { B, bodies, renderEditor, stubLayout } from "@/test/table-editor-fixtures";

const SC = `${B}/schema-changes`;
const pending = async () => within(await screen.findByRole("complementary", { name: "Pending changes" })).getAllByRole("listitem").map((li) => li.textContent);

describe("types view", () => {
  beforeEach(() => { localStorage.clear(); sessionStorage.clear(); stubLayout(); });

  async function types() {
    const r = renderEditor();
    await r.user.click(await screen.findByRole("button", { name: "Types" }));
    await screen.findByRole("heading", { name: "Types in public" });
    return r;
  }

  it("lists_enums_with_values", async () => {
    await types();
    const section = screen.getByRole("region", { name: "invoice_status" });
    for (const v of ["open", "paid", "void"]) expect(within(section).getByText(v)).toBeInTheDocument();
  });

  it("new_enum_and_add_value_after", async () => {
    const { f, user } = await types();
    await user.click(screen.getByRole("button", { name: "New enum" }));
    let d = await screen.findByRole("dialog", { name: "New enum" });
    await user.type(within(d).getByLabelText("Enum name"), "tier");
    await user.type(within(d).getByLabelText("Values"), "free{Enter}pro");
    await user.click(within(d).getByRole("button", { name: "Add to changes" }));
    await user.click(screen.getByRole("button", { name: "Add value to invoice_status" }));
    d = await screen.findByRole("dialog", { name: "Add value to invoice_status" });
    await user.type(within(d).getByLabelText("Value"), "refunded");
    await user.click(within(d).getByLabelText("Position"));
    await user.click(await screen.findByRole("option", { name: "After…" }));
    await user.click(within(d).getByLabelText("Relative to"));
    await user.click(await screen.findByRole("option", { name: "paid" }));
    await user.click(within(d).getByRole("button", { name: "Add to changes" }));
    expect(await pending()).toEqual([expect.stringContaining("Create enum public.tier"), expect.stringContaining("Add value refunded to invoice_status")]);
    await vi.waitFor(async () => expect((await bodies(f, "POST", SC)).length).toBeGreaterThan(0), { timeout: 2000 });
    expect((await bodies(f, "POST", SC)).at(-1).operations).toEqual([
      { create_enum: { schema: "public", name: "tier", values: ["free", "pro"] } },
      { add_enum_value: { schema: "public", name: "invoice_status", value: "refunded", position: { after: "paid" } } },
    ]);
  });

  it("rename_value_rename_and_drop", async () => {
    const { user } = await types();
    await user.click(screen.getByRole("button", { name: "Rename value in invoice_status" }));
    const d = await screen.findByRole("dialog", { name: "Rename value in invoice_status" });
    await user.click(within(d).getByLabelText("From"));
    await user.click(await screen.findByRole("option", { name: "void" }));
    await user.type(within(d).getByLabelText("To"), "cancelled");
    await user.click(within(d).getByRole("button", { name: "Add to changes" }));
    await user.click(screen.getByRole("button", { name: "Drop invoice_status" }));
    expect(await pending()).toEqual([expect.stringContaining("Rename value void → cancelled in invoice_status"), expect.stringContaining("Drop enum public.invoice_status")]);
  });
});
```

**Gotchas:** Types edits go to the `…/{schema}.#types` changeset (P8) and their dry run has no table size. The new-table changeset (`…/{schema}.#new`) folds column edits into the `create_table` (F6). New tables get no `rls` and no grants (RLS off, not exposed). After Apply of a new table, discard its draft and navigate.

**Verify:** `pnpm --filter @chelabase/studio exec vitest run src/components/table-editor` and `pnpm --filter @chelabase/studio typecheck`.

**Live runs (the plan's one stack):** none.

---

## T7: remove the old console, docs (sonnet / none — leaf, the end-of-step review covers it)

**Files:** delete `apps/studio/src/components/{tables-view,console-grid,row-editor}.tsx` and `tables-view.test.tsx` (and `console-filters.tsx` if nothing imports it after T3); create `apps/docs/content/docs/using/table-editor.mdx`; add `table-editor` to `apps/docs/content/docs/using/meta.json` after `sql-editor`; update `apps/docs/content/docs/using/studio.mdx` (the Tables console section points to the editor). Tests (coordinator): `apps/studio/src/components/old-console-removed.test.ts`.

**Behaviour:** the docs page covers: opening the editor (rail, project page), the pickers and Act as, rows (inline edit, row panel, selection, delete all matching), the data-API requirement for rows, structure changes (strip, form, Structure tab, constraints, enums, new table), the changes panel (dry run, warnings, cascade, Apply in one transaction), RLS off by default with the banner, protected branches; plain and terse like the other `using/` pages; no screenshots.

**RED tests (coordinator)** — `apps/studio/src/components/old-console-removed.test.ts`:

```ts
import { describe, expect, it } from "vitest";

describe("old Tables console", () => {
  it("is_gone", () => {
    const left = import.meta.glob(["./tables-view.tsx", "./console-grid.tsx", "./row-editor.tsx"]);
    expect(Object.keys(left)).toEqual([]);
  });
});
```

**Gotchas:** T7 runs after T2 (the project page no longer imports `TablesView`); if T3 kept `console-filters.tsx`, leave it. `just check` covers the docs build.

**Verify:** `pnpm --filter @chelabase/studio exec vitest run src/components/old-console-removed.test.ts`, `pnpm --filter @chelabase/studio typecheck`, `pnpm --filter docs build` (or the docs app's package name in `apps/docs/package.json`).

**Live runs (the plan's one stack):** none.

---

## T8: the count op (opus / opus)

Added during execution (user R6, 2026-10-14; spec "Addendum"). Runs in wave 3 beside T4 and T5.

**Files:** `crates/chelabase-query/src/{query,sql,shape,openapi}.rs`, `services/orchestrator/src/console.rs` (`write_kind`), `openapi/data.json` (regenerated with `just api-gen`), `packages/client/src/{query,types,json}.ts`, `apps/docs/content/docs/using/data-api/query-language.mdx` (`## count`, after `## select`), `apps/studio/src/components/table-editor/selection-bar.tsx` (and the smallest needed change in `rows-view.tsx` to give it the filter tree and role), `apps/studio/src/lib/console.ts` (`ConsoleOp`/`VERB` gain `count` if errors are shown for it). Every exhaustive `match` on `Op` (parse dispatch and its error text, `validate`, `resolve`'s `Slot::Where`, `Planner::op`, `shape::hash_op` with its own tag, `write_kind`) and the OpenAPI discriminator mappings (`mapping([…; 6])`).

**Behaviour:** the spec's Addendum. Pinned: `pub struct Count { pub from: Name, #[serde(rename = "where", default)] pub filter: Option<Filter> }` with `#[serde(deny_unknown_fields)]`; `Op::Count(Count)`; `QueryRequest`/`BatchRequest` list `CountRequest`/`BatchCount`; `QueryResult`'s description covers `{"count": N}`; client `SelectQuery#count(): Operation<{ count: number }>` and the tree `{ v: 1, op: "count", from, where? }`.

**RED tests (coordinator):** committed in `2f631808` on `table-editor-t8`: `crates/chelabase-query/tests/{parse,sql,shape}.rs` (count sections), `services/orchestrator/src/console.rs` (`writes_are_every_kind_but_a_read` gains a count line), `services/rest/src/openapi.rs` (`schema_names_are_api_facing` lists `BatchCount`, `CountRequest`), `packages/client/fixtures/count-filtered.json` + its builder in `src/fixtures.test.ts` + a `count` block in `src/client.test.ts`, `services/rest/tests/rest_live.rs` (`count_respects_rls_and_filters`), `apps/studio/src/components/table-editor/selection-count.test.tsx`, and the filtered regex in `rows-view.test.tsx`. RED: the engine tests don't compile (`Op::Count` missing); 3 client and 2 editor tests fail; `rest_live` compiles with `--no-run`.

**Gotchas:** `openapi_fidelity.rs` runs every client fixture through the schema and the parser; `document_is_current` needs `just api-gen`. `bench-data-api`'s copy of `join_results` has a wildcard arm (no change needed). The count's JSON number must survive a `bigint` count (Postgres `count(*)` is `bigint`; `json_build_object` emits a JSON number).

**Verify:** `cargo nextest run -p chelabase-query -p rest -p orchestrator` (unit and non-live integration tests), `cargo clippy -p chelabase-query -p rest -p orchestrator --all-targets -- -D warnings`, `pnpm --filter @chelabase/client test`, `pnpm --filter @chelabase/client typecheck`, `pnpm --filter @chelabase/studio exec vitest run src/components/table-editor src/lib`, `pnpm --filter @chelabase/studio typecheck`, and `cargo test -p rest --test rest_live --no-run`.

**Live runs (the plan's one stack):** `count_respects_rls_and_filters` needs the stack's `rest` to carry T8's code, so it is deferred to the end-of-step live pass (after the merge and `just workbench restart rest`): `just workbench test -p rest --test rest_live -- count_respects_rls_and_filters --run-ignored only`.

---

## End of step

0. `main` was green at the step's start (gate chore merged 2026-10-14, `just check` green). No red-team pass (spec §5: no security or concurrency core of its own).
1. Whole-branch review (opus) on `main..table-editor`; it also covers T4, T6 and T7, which had no review of their own.
2. The one final fix round: the end-of-step review's findings plus every task finding deferred to it (the Status lists them); the coordinator writes a failing test for each finding first.
3. Impeccable finish: `impeccable detect --json` over `apps/studio/src/components/table-editor/`; then `impeccable:impeccable-finish-reviewer` (opus) with the surface brief's direction contract, the four sketches and screenshots from the browser pass (step 6's captures go first, in `apps/studio/.impeccable/review/te-*.png`); its fixes join a short second fix batch (coordinator verifies); then `impeccable:impeccable-documenter` updates `apps/studio/DESIGN.md` (data grids: zebra rows, column hairlines, resize grip, collapsible right panel).
4. `just check`, once (the step's only gate run; a flaky test seen here is fixed, not rerun).
5. No fork changed: no integration build, no image build. Restart the stack's apps from the merged branch (`just workbench restart`), then run the deferred live test `just workbench test -p rest --test rest_live -- count_respects_rls_and_filters --run-ignored only` (T8) and the whole `rest_live` file once.
6. Browser pass (Playwright MCP, `--isolated`) on the plan's stack (`.worktrees/table-editor`, `just workbench status` for its Studio URL), as the demo admin and as a Developer member: pickers and deep links; rows (edit, insert, insert with all defaults, select all → delete matching on a scratch table); add a column with its dry run and Apply; a unique and a foreign key constraint; an enum and a value; the RLS banner and toggle on a new table; a protected branch as a Developer (structure read-only, rows editable); a Postgres database without the data API (rows gated, enable). Captures at 1440×900 to `apps/studio/.impeccable/review/te-*.png` (git-ignored).
7. Update `docs/landing-highlights.md` on `main`, the plan Status and the roadmap's "Resume here", then pause for the user (merge decision); after the merge, `just workbench down --purge` in the step worktree via `just wt-remove table-editor`.
