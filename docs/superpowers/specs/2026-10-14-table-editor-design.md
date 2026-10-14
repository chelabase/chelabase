# TE-B1: the table editor (design)

Branch `table-editor`. Roadmap step TE-B, first half (TE-B1); TE-B2 follows with its own spec. Built on TE-A's table API (merged `aa3bce9b`) and the console row endpoints. Decisions (1)–(7) are in the roadmap's "TE-B decisions" note; the Impeccable shape pass (surface brief and direction contract: `apps/studio/.impeccable/surfaces/src-components-table-editor-table-editor-tsx.md`, sketches in `apps/studio/.impeccable/mocks/decision/`) and design parts 1–5 were approved by the user on 2026-10-14. Choices made while writing this spec (not asked) are **S-rulings** at the end.

## Goal

A top-level "Table editor" in Studio where a member picks project → database → branch → Postgres database, then reads and edits rows and changes table structure without writing SQL. Every structure change is staged, shown as the exact SQL with TE-A's dry run, and applied as one transaction, acting as the owner or a role the member may use. It replaces the project page's inline Tables console. No backend change, no fork change, no image build.

**TE-B1 scope:** the shell (route, pickers, act-as, rail), the row grid and row panel, selection with copy/delete, the column strip and Structure view, columns, primary key / unique / foreign key / check constraints (NOT VALID + validate), table create/rename/move/comment/drop, enums, the RLS on/off toggle, and the changeset panel.

**TE-B2 (later):** indexes (incl. `CONCURRENTLY`), exclusion constraints, policies (with the "all rows" preset, Members follow-up b), grants and data API exposure, owner. Until then, indexes, policies and grants show read-only.

## 1. Route, context and access

- **Route:** `src/routes/app.tables.{-$projectId}.{-$databaseId}.{-$branchId}.{-$pgDatabase}.{-$schema}.{-$table}.tsx` (TanStack Router optional path params). A missing level resolves to a default and the URL is rewritten with `replace`: project = last used (else the first), database = the project's first, branch = the root branch, Postgres database = the branch's main database, schema = `public` (else the first), table = none ("Pick a table"). An unknown id shows a not-found state with the pickers still usable. The editor uses the full page width.
- **Rail:** the placeholder entry becomes `{ kind: "link", id: "tables", label: "Table editor", to: "/app/tables", icon: Table2 }`, second, below Projects. `isRailItemActive("tables")` matches `/app/tables` and its sub-paths. The rail link opens the last context, stored in `localStorage` key `chelabase.studio.tables.last` (ids only; every access in try/catch; failure = defaults).
- **Context bar** (in the shell header's reserved context slot): four labelled pickers, each a Base UI combobox with search, then "Act as" on the right.
  - Project (`useListProjects`), Database (the project's databases), Branch (a tree of `kind: normal`, `status: ready` branches with last activity and a "protected" label; backup branches excluded; a final item "New branch from {current}…" opens the existing new-branch form), Postgres db (every database of the branch, with a "data API" mark on served ones).
  - Picking a level navigates and drops the levels after it.
  - Act as: `actAsRoles(caps)`; the owner only for Admins; default owner for Admins, otherwise the first allowed role; kept per branch in the store.
- **Access mirrors the API** (the API stays the authority; a 403 is always shown, never swallowed):
  - structure reads: anyone who can view the project;
  - structure edits (`ActAsBranch`): Developer on unprotected branches, Admin on protected ones; otherwise the strip, "+ Column", "New table", Types and Structure actions are read-only with a visible reason ("`main` is protected: schema changes need Admin");
  - rows: need an act-as role and the data API on that Postgres database; Viewers get a read-only grid; row writes need Developer;
  - no role at all: structure visible, Rows says "You can't act as any Postgres role here."
- **Console replaced:** the project page's "Tables" button becomes a link to `/app/tables/{projectId}/{databaseId}`. `tables-view.tsx`, `console-grid.tsx`, `row-editor.tsx` and their tests are deleted. `lib/console.ts` stays and is extended. `console-filters.tsx` is reused in the Filter popover if it fits; otherwise replaced (the plan decides after reading it).

## 2. The pure model and the store

All in `src/lib/table-editor/` (pure TypeScript, no React), consumed by a zustand store and thin components in `src/components/table-editor/`.

- **`changeset.ts`.** `Edit = { id: string; op: Operation; label: string }` (`Operation` is the generated TE-A type). `Changeset = { key: ChangesetKey; base: TableFingerprint | null; edits: Edit[] }`. `fold(edits, next) -> Edit[]` keeps the list minimal:
  - an edit to a column added in this changeset rewrites its `add_column` (never `add_column` + `alter_column`);
  - renaming such a column renames it inside the `add_column`;
  - dropping it removes the `add_column` and every pending edit on that column;
  - an edit that returns an object to its original state removes the edits that changed it;
  - for a draft table (`create_table` pending), column edits rewrite the `create_table`.
  - `project(table, edits) -> ProjectedTable` gives the table as it will look, with per-column and per-constraint marks `added | changed | dropped`.
  - **Invariants (tested):** `fold` is idempotent; applying the folded list has the same effect as applying the raw sequence (property test over generated edit sequences against a reference interpreter of the operations); `project` of the folded list equals `project` of the raw sequence.
- **`column-form.ts`.** Form state ↔ `ColumnSpec` (new) or a minimal `ColumnChange` plus `rename_column` and comment (existing). Validation: name non-empty, at most 63 bytes (UTF-8), unique in the projected table; type chosen from the type catalog (`useListTypes`), with an Array flag; default kind None / Value (literal, quoted by the renderer) / Expression (SQL text).
- **`dry-run.ts`.** Runs the dry run (`useApplySchemaChanges` with `dry_run: true`) 400 ms after the last edit, and not while an edit dialog is open, aborting stale requests; maps the response to per-edit status (`ok | error | rendered_only` + reason), warnings (`rewrites_table` with the table size, lock level in plain words) and dependents. The cascade checkbox sends `cascade: { dependents_hash }` from the latest dry run. **Apply** sends the same operations with `dry_run: false`; on success it clears the changeset, invalidates the table, object list, console schema and rows queries, and marks changed columns/rows seafoam until the user moves past them; `dependents_changed` shows the new list and unticks cascade; an error with `op_index` is shown on that edit.
- **Re-read on apply.** A changeset stores the fingerprint (SHA-256 of the canonical JSON of the `getTable` response) of the table it was built on. When a refetch shows a different fingerprint, Apply is blocked behind "This table changed since you started" plus a fresh dry run; the user reviews and applies, or discards.
- **`selection.ts`.** `none | rows (set of primary-key tuples on the loaded page) | all_matching (the filter tree at selection time + the estimate)`; reset on table, filter or sort change.
- **Cells.** `lib/console`'s `inputKind`/`parseValue` are reused and extended for enums (labels from the type catalog), arrays and JSON.
- **Store** (`src/lib/table-editor/store.ts`, zustand): changesets keyed `{databaseId}/{branchId}/{pgDatabase}/{schema}.{table}` (draft tables keyed by schema and draft id); panel state (`closed | changes | column | row`, `collapsed`); selection; act-as per branch. Changesets are mirrored to `sessionStorage` (key `chelabase.studio.tables.drafts`, this tab only, structure operations only, never row values), removed on Apply or Discard, every access in try/catch. While any changeset is non-empty the page asks before unload.

## 3. The grid and rows

- **Reads:** `consoleQueryExact` with `buildSelect` (filters, sort, offset paging with the primary key appended for stable pages), as the chosen role; page size `pageSizeFor(max_rows)`. Footer: "Rows 1–100 of ≈ N" (N = the object list's `row_estimate`; "of many" when null).
- **Grid:** TanStack Table 9 with `@tanstack/react-virtual` (rows always virtualized within the page; columns virtualized above 40). Zebra rows (chalk 4.5%), a hairline on every column's right edge, sticky header and gutter, a sand resize grip on a hovered header edge (widths per table in `localStorage` key `chelabase.studio.tables.widths`), header click cycles sort asc → desc → none, a Filter popover over `lib/console`'s filter tree. The column strip shares the grid's column template and horizontal scroll.
- **Keyboard and ARIA:** `role="grid"` with `aria-rowcount`/`aria-rowindex`/`aria-colindex`; arrows move the active cell; Enter or F2 edits; in an editor Enter saves, Escape cancels, Tab moves to the next cell; Space toggles the row's selection; Ctrl/Cmd+C copies the selected cells or rows as TSV. Focus is a 2px square sand outline.
- **Inline edit:** type-aware editors (text, number, boolean, enum select, date/time, JSON in a small CodeMirror); save = one `update` by primary key (`whereByKey`) as the role, the row refreshed from `returning`; errors at the cell in coral with `consoleErrorText`. A table without a primary key is read-only with the reason.
- **Row panel** (gutter icon, Enter on the gutter, or Insert row): a field per column; on insert, defaults show as "default" placeholders and identity/generated columns are read-only; foreign keys get "Pick…" (search the referenced table as the same role, 20 results, by key and text columns) and "Open" (navigates to the referenced table filtered to the key); enum selects; a NULL toggle on nullable columns; "Now" on date/time; JSON fields; a field filter above 30 columns; Delete row (coral), Cancel, Save row. Insert row's menu has "Insert with all defaults" (`useInsertDefaultRow`).
- **Large values:** cells show at most 200 characters. A page over the response limit is retried at half the page size, down to 1; a single row still too large is read without its large columns (types `text`, `bytea`, `json`, `jsonb`, `xml`, arrays), which show "Load value" (`useGetCell`).
- **Selection:** gutter checkboxes and a header checkbox for the loaded page; the selection bar then offers "Select all ≈ N matching rows". Actions: Copy (TSV) and Copy as JSON (loaded rows only); Delete selected (one `delete` with the selected keys, confirmed with the count); Delete all matching (one `delete` with the current filter; with no filter `all: true`, confirmed by typing the table name). Afterwards rows are refetched and the selection cleared.
- **Data API gate:** on a Postgres database without the data API, the Rows view says rows need it and offers "Enable data API" (Developer+, the existing action). Branch protection doesn't block rows (Postgres grants decide).

## 4. Structure and the right panel

- **Column strip:** a typed chip per column (name, type, key/FK/null marks). A chip opens that column's form; its menu offers Rename, Change type, Default, Not null, Comment, Drop. Pending marks: added = sand left edge + "new"; changed = sand left edge; dropped = struck through with a coral edge. "+ Column" opens an empty form.
- **Column form** (right panel): Name; Type (searchable catalog combobox) + Array; Default (None / Value / Expression, with suggestions per type such as `now()` and `gen_random_uuid()`); Not null, Unique, Primary key (disabled with a reason when the table has one); References… (table, columns, on delete/update); Check… (SQL text); Comment; a live SQL preview of this edit; Cancel and "Add to changes". "Review N changes" in the table bar returns to the changes list.
- **Structure view** (Rows | Structure): Columns (every attribute; a row opens the form); Constraints (add primary key, unique, foreign key, check; optional NOT VALID then Validate; Rename; Drop); Indexes, Policies, Grants as read-only lists; Table (Rename, Move to schema, Comment, Drop table). Every drop is dry-run; when dependents exist the panel lists them with "Drop dependents too (cascade)".
- **Types:** the left list has a "Types" entry per schema, opening an Enums view: create, add value (before/after), rename value, rename, drop. Column forms list enums from the type catalog.
- **New table:** "New table" opens a draft table in the main area with its changeset in the panel: name and schema, starter columns `id bigint generated always as identity primary key` and `created_at timestamptz not null default now()` (editable, removable), comment; Create is one `create_table` reviewed like any change. A "Turn RLS on" checkbox, unticked by default (R8, user 2026-10-14), sends `create_table.rls = { enabled: true, forced: false }` when ticked. Unticked, the form says plainly that in a schema the data API serves, its default grants let anon read every row and authenticated read and write every row while RLS is off.
- **RLS:** any table with RLS off shows a coral banner over the grid and the Structure view: "RLS is off: any role with grants reads and writes every row", with "Turn RLS on" staging `set_rls { enabled: true, forced: false }` (reusing `rls-warning.tsx` where it fits). A table with RLS on and no policies says "RLS is on with no policies: only the owner and roles that bypass RLS see rows." Policies stay read-only until TE-B2.
- **Changes panel:** numbered edits, each with its dry-run status (clicking reopens its form; each can be removed); the full SQL; warnings ("Rewrites `invoices` (12 MB): reads and writes wait while it runs", the lock level, why a statement wasn't simulated); dependents with the cascade checkbox; Discard (confirmed) and "Apply N changes", enabled once the dry run passes, or, when part could not be simulated (`would_succeed: null`), after an "I've read the SQL" checkbox. Errors show at their edit (message, detail, hint); `lock_timeout` offers "Try again"; `branch_protected` explains the rule. After Apply: a `role="status"` line "Applied N changes to `schema.table`", the seafoam marks, the changeset cleared.
- **Panel:** one right panel (360 px; 420 px for the row panel), collapsible to a 48 px strip that names what is open and keeps any draft; a pending changeset is never lost by collapsing or switching panels.

## 5. Tests, tasks and passes

- **RED tests by the coordinator** for every task, committed before briefing (standing rule).
- **Model:** unit and property tests (`fast-check`, new dev dependency, latest version checked in the plan under pnpm's release-age rule) for the invariants in part 2, `column-form` validation and diffing, `dry-run` response mapping, `selection` resets.
- **Components:** Vitest + Testing Library with the generated client's fetch mocked as in today's Studio tests: access gates and reasons, data-API gate, picker navigation and URL defaults, keyboard paths in the grid, inline edit save/error, selection actions and confirmations, panel states, apply flow (dry run, cascade, `dependents_changed`, fingerprint block, errors at their edit), RLS banner.
- **No backend change:** no Rust or live tests. The browser pass is the live check.
- **Tasks:**

| Task | Content | Implementer | Reviewer |
|---|---|---|---|
| T1 | Pure model (changeset fold/project, column form, selection, dry-run mapping) | opus | opus |
| T2 | Route, pickers, act-as, rail item, store with sessionStorage drafts, project-page link, access gates | sonnet | sonnet |
| T3 | Grid (TanStack Table/Virtual, paging, sort, filter, resize, keyboard, inline edit, selection and actions, data-API gate) | opus | opus |
| T4 | Row panel (fields, FK Pick/Open, insert with all defaults, large values) | sonnet | none (leaf) |
| T5 | Column strip, column form, Structure view, changes panel with dry run and Apply, RLS banner | opus | opus |
| T6 | Constraints, table operations, new table, Types/Enums view | sonnet | none (leaf) |
| T7 | Delete the old console; docs page `apps/docs/content/docs/using/table-editor.mdx` | sonnet | none (leaf) |

  Waves: T1 ∥ T2 → T3 → T4 ∥ T5 → T6 ∥ T7 (at most 3 at once, each in its own short-lived worktree).
- **End of step:** end-of-step whole-branch review (opus) → one combined final fix round → Impeccable finish reviewer (opus) against the direction contract, then the documenter (DESIGN.md gains zebra rows and column hairlines for data grids) → `just check` once → a browser pass (Playwright MCP) on the plan's single stack: pickers, rows (edit, insert, select-all delete), add column with dry run and Apply, a constraint, an enum, the RLS toggle, a protected branch as a Developer, a Postgres database without the data API. No fork change, no image build.
- **No red-team agent:** the step has no security or concurrency core of its own (the API enforces access and runs the transaction; TE-A was red-teamed by its reviews).

## Addendum (user, 2026-10-14, during execution): an exact count for "select all matching"

With a filter on, "Select all matching rows" shows the exact number of matching rows. The user chose a `count` op in the query engine (not an editor-only endpoint), so apps get counts through the data API and the typed client too. This widens the step beyond "no backend change" (no fork change still).

- **Query language:** `{ "v": 1, "op": "count", "from": "schema.table", "where"?: <filter> }` → `{ "count": <integer> }`. No `select`, `orderBy`, `limit` or `offset`. Runs as the requesting role like `select` (RLS and grants decide what is counted), with the same statement timeout and response cap. Allowed wherever `select` is (batch, pipeline). A read: Viewers may run it in the console.
- **SQL:** `SELECT json_build_object('count', count(*))::text FROM <table>[ WHERE …]`, binds like a select's `where`.
- **Engine:** `Op::Count(Count { from, filter })` in `chelabase-query` (parse, validate, plan, bind resolution, shape hash, OpenAPI `CountRequest`/`BatchCount`); `write_kind` classifies it as a read.
- **Client:** `db.from("t").where(…).count()` → `Operation<{ count: number }>`; a contract fixture `count-filtered.json`.
- **Docs:** `using/data-api/query-language.mdx` gains `## count`.
- **Editor:** filtered "select all": the bar sends a console `count` (same `where`, as the act-as role) once the page is fully selected and shows `Select all {n} matching rows` / `All {n} matching rows selected`; a failed or timed-out count shows `Select all matching rows` / `All matching rows selected` (no figure). Unfiltered keeps the `≈` estimate and sends no count.

## S-rulings (chosen while writing; cost if wrong)

- **S1** dry-run debounce: 400 ms after the last edit, and not while an edit dialog is open (ruling 3b: the dialog is about to add to the changes, and a dry run takes the statements' locks on the live branch), with abort of stale runs. Cost: tuning only.
- **S2** cells show at most 200 characters; columns virtualized above 40; row panel field filter above 30 columns; FK Pick returns 20 results. Cost: tuning only.
- **S3** the table fingerprint is SHA-256 of the canonical JSON of the `getTable` response. Cost: a false "table changed" if the response carries volatile fields (then those fields are dropped from the fingerprint).
- **S4** storage keys: `chelabase.studio.tables.last` and `.widths` in `localStorage`, `chelabase.studio.tables.drafts` in `sessionStorage`. Cost: rename only.
- **S5** "Types" is a per-schema entry in the left list, not a separate rail item or tab. Cost: moving one view.
- **S6** starter columns for a new table: `id bigint generated always as identity primary key`, `created_at timestamptz not null default now()`. Cost: a different default set.
- **S7** "Turn RLS on" stages `set_rls { enabled: true, forced: false }`. Cost: forced RLS would also bind the owner; a later toggle can add it.
- **S8** large-column types fetched lazily on oversized rows: `text`, `bytea`, `json`, `jsonb`, `xml`, arrays. Cost: tuning only.
