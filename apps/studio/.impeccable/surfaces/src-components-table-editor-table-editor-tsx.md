---
version: 1
slug: "src-components-table-editor-table-editor-tsx"
primary_target: "src/components/table-editor/table-editor.tsx"
related_targets: ["src/routes/app.tables.tsx","src/components/project-page.tsx","src/components/app-shell/rail-items.ts"]
---

# Table editor (TE-B1)

**Scope and mode:** Operate. Studio's top-level "Table editor" (rail item; route `/app/tables/$projectId/$databaseId/$branchId/$pgDatabase/$schema/$table`, tail optional). Replaces the project page's inline Tables console. Built on TE-A's table API and the console row endpoints. TE-B2 (indexes, exclusion constraints, RLS and policies, grants and exposure, owner) extends this surface later. Shape pass approved by the user 2026-10-14; sketches in `.impeccable/mocks/decision/` (`switch-pickers`, `add-column`, `row-panel`, `grid-edges`).

## 1. Job and audience

- App developers and department members (e.g. billing) who know Postgres roles. Most visits read and fix rows; structure changes are rarer but one click away.
- Ranges: 5–60 tables per schema (rarely 500); 3–30 columns (rarely 150); up to millions of rows, paged.

## 2. Outcome and proof

- Find a table, read and fix rows, change structure without writing SQL. Every structure change shows its exact SQL and dry-run result (notices, lock level, rewrite, dependents) before it runs, acting as a role the person may use.

## 3. Layout

- **Header context bar:** one labelled picker per level (Project / Database / Branch / Postgres db), each a searchable menu of that level only; branches as a tree with last activity and "New branch from…"; picking a level resets the levels after it. "Act as" picker on the right.
- **Left list:** schema select, table filter, tables with row estimates, "New table" at the foot.
- **Table bar:** name (mono), ≈ rows · size · RLS · data API; Rows | Structure; Filter, Sort, Insert row.
- **Column strip:** a typed chip per column aligned over its grid column (name, type, key/FK/null marks); pending chips carry a sand left edge and "new"; "+ Column" at the end. Scrolls in step with the grid.
- **Grid:** TanStack Table 9, virtualized; zebra rows (chalk 4.5%); a hairline on every column's right edge; a sand resize grip on a hovered header edge; inline cell editing (Enter saves the row, Escape cancels, spreadsheet keys).
- **Right panel** (one, collapsible to a 48px strip that names what's open and keeps any draft): pending changes (numbered list, SQL, dry-run note, Discard, "Apply N changes"); new/edit column form (Name, Type + Array, Default None/Value/Expression with suggestions, Not null / Unique / Primary key, References…, Check…, Comment, live SQL preview, "Add to changes"; "Review N changes" in the table bar returns to the list); row panel (type-aware field per column, FK Pick/Open, enum select, NULL toggle, Now, "Load full value", field filter on wide tables; Delete row, Cancel, Save row).
- **Selection:** gutter checkboxes, a header select-all for the loaded page, then "Select all ≈N matching rows" (current filter or whole table). Copy (selected loaded rows, TSV and JSON); Delete (count confirmation; "all matching" deletes by filter, typing the table name when unfiltered).

## 4. States

Loading, empty table, no tables, no data API (rows gated, "Enable data API"), protected branch for non-Admins (structure read-only, reason shown), role refused, lock timeout, dry run failed (Postgres error at its operation), dependents with a cascade checkbox, rewrites-table warning, changes applied (seafoam edge-mark on changed columns/rows until moved past).

## 5. Constraints and open decisions

- WCAG 2.2 AA, full keyboard paths in the grid and pickers. Desktop first.
- Durable DESIGN.md additions at finish: zebra rows and column hairlines for data grids.
- Open for the spec: exact component split, TanStack Table/Virtual versions under pnpm's release-age rule.

## Direction contract

THESIS: The table's columns stay in sight as a typed strip over its rows, so structure and data are one surface; refuses the default of a data grid with structure hidden in a separate settings page.

OWN-WORLD: Lagoon ink ground, deep-water panels, chalk text, hairlines not shadows, square chrome; mono identifiers; sand only for action, selection and pending; coral hazard; seafoam yes. Zebra rows and column hairlines make the grid legible.

STORY: The visitor sees where they are (four labelled pickers, the role they act as), finds the table, edits rows in place, and stages structure changes they review as exact SQL before one Apply.

FIRST VIEWPORT: Rail; header pickers; 248px table list; table bar; column strip; grid filling the rest; right panel (360–420px) only while something is open, collapsible to 48px. The one sand-filled button is the panel's primary action.

FORM: Column strip over rows, grounded candidate 4 of 7, seed key 0f85e4cf; refined by the user's steers (zebra, per-level pickers, column edges, collapsible panel, select all).

FINISH: unreviewed and undocumented is unfinished; this build ends with the finish review, the verdict, DESIGN.md, and every shipping raster carrying its provenance
