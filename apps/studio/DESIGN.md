---
name: Chelabase Studio
description: The signed-in app system for Chelabase Studio, a tool frame on warm matte black.
colors:
  ink: "#0c0b0a"
  deep-water: "#161412"
  channel: "#1e1b18"
  shallows: "#25211d"
  sand: "#f2c7a3"
  sand-lit: "#f7d6b9"
  seafoam: "#9fd4c6"
  coral: "#eda6aa"
  palm: "#a9c89a"
  chalk: "#ebe6e0"
  chalk-dim: "#b0a89f"
  hairline: "rgb(235 230 224 / 0.15)"
  hairline-strong: "rgb(235 230 224 / 0.34)"
  control-border: "rgb(235 230 224 / 0.4)"
  chalk-wash: "rgb(235 230 224 / 0.06)"
  chalk-wash-strong: "rgb(235 230 224 / 0.08)"
  zebra: "rgb(235 230 224 / 0.045)"
  sand-tint: "rgb(242 199 163 / 0.25)"
  sand-wash: "rgb(242 199 163 / 0.08)"
  pending-edge: "rgb(242 199 163 / 0.45)"
  shell: "#f2936e"
  shell-dark: "#b85a3e"
  shell-light: "#f8b896"
  shell-tip: "#fde3cf"
typography:
  wordmark:
    fontFamily: "Spectral, Iowan Old Style, Georgia, serif"
    fontSize: "1.25rem"
    fontWeight: 700
    lineHeight: 1
    letterSpacing: "-0.02em"
  headline:
    fontFamily: "Atkinson Hyperlegible Next Variable, Atkinson Hyperlegible, system-ui, sans-serif"
    fontSize: "1.5rem"
    fontWeight: 600
    lineHeight: "2rem"
  title:
    fontFamily: "Atkinson Hyperlegible Next Variable, Atkinson Hyperlegible, system-ui, sans-serif"
    fontSize: "1rem"
    fontWeight: 600
    lineHeight: "1.5rem"
  body:
    fontFamily: "Atkinson Hyperlegible Next Variable, Atkinson Hyperlegible, system-ui, sans-serif"
    fontSize: "0.875rem"
    fontWeight: 400
    lineHeight: "1.25rem"
  note:
    fontFamily: "Atkinson Hyperlegible Next Variable, Atkinson Hyperlegible, system-ui, sans-serif"
    fontSize: "13px"
    fontWeight: 400
  label:
    fontFamily: "Atkinson Hyperlegible Next Variable, Atkinson Hyperlegible, system-ui, sans-serif"
    fontSize: "0.75rem"
    fontWeight: 400
    lineHeight: "1rem"
  micro:
    fontFamily: "Atkinson Hyperlegible Next Variable, Atkinson Hyperlegible, system-ui, sans-serif"
    fontSize: "11px"
    fontWeight: 400
    lineHeight: "1rem"
  code:
    fontFamily: "JetBrains Mono Variable, ui-monospace, monospace"
    fontSize: "13px"
    fontWeight: 400
  grid-cell:
    fontFamily: "JetBrains Mono Variable, ui-monospace, monospace"
    fontSize: "0.75rem"
    fontWeight: 400
rounded:
  none: "0px"
  lg: "0.5rem"
spacing:
  rail-collapsed: "56px"
  rail-expanded: "232px"
  header: "52px"
  row: "36px"
  rail-inset: "19px"
  header-gutter: "16px"
  page-gutter: "32px"
  page-column: "56rem"
  table-list: "248px"
  column-strip: "44px"
  grid-gutter: "88px"
  pending-column: "160px"
  panel: "360px"
  panel-wide: "26rem"
  panel-collapsed: "48px"
components:
  rail:
    backgroundColor: "{colors.deep-water}"
    textColor: "{colors.chalk}"
    rounded: "{rounded.none}"
    width: "{spacing.rail-collapsed}"
  rail-expanded:
    backgroundColor: "{colors.deep-water}"
    width: "{spacing.rail-expanded}"
  rail-item:
    textColor: "{colors.chalk-dim}"
    typography: "{typography.body}"
    rounded: "{rounded.none}"
    height: "{spacing.row}"
    padding: "0 19px"
  rail-item-hover:
    backgroundColor: "{colors.chalk-wash}"
    textColor: "{colors.chalk}"
  rail-item-active:
    textColor: "{colors.chalk}"
  rail-placeholder:
    textColor: "{colors.chalk-dim}"
    height: "{spacing.row}"
    padding: "0 10px"
  header:
    backgroundColor: "{colors.ink}"
    textColor: "{colors.chalk}"
    height: "{spacing.header}"
    padding: "0 16px"
  header-trigger:
    textColor: "{colors.chalk}"
    typography: "{typography.body}"
    rounded: "{rounded.none}"
    height: "{spacing.row}"
    padding: "0 8px"
  header-trigger-hover:
    backgroundColor: "{colors.chalk-wash-strong}"
  menu-popup:
    backgroundColor: "{colors.deep-water}"
    textColor: "{colors.chalk}"
    typography: "{typography.body}"
    rounded: "{rounded.none}"
  menu-item-highlighted:
    backgroundColor: "{colors.chalk-wash-strong}"
  tooltip:
    backgroundColor: "{colors.deep-water}"
    textColor: "{colors.chalk}"
    typography: "{typography.label}"
    rounded: "{rounded.none}"
    padding: "4px 8px"
  button-primary:
    backgroundColor: "{colors.sand}"
    textColor: "{colors.ink}"
    typography: "{typography.body}"
    rounded: "{rounded.lg}"
    height: "32px"
    padding: "0 10px"
  code-editor:
    backgroundColor: "{colors.ink}"
    textColor: "{colors.chalk}"
    typography: "{typography.code}"
    rounded: "{rounded.lg}"
  grid-header:
    backgroundColor: "{colors.channel}"
    textColor: "{colors.chalk-dim}"
    typography: "{typography.grid-cell}"
    rounded: "{rounded.none}"
    height: "{spacing.row}"
    padding: "0 8px"
  grid-row:
    backgroundColor: "{colors.ink}"
    textColor: "{colors.chalk}"
    typography: "{typography.grid-cell}"
    height: "{spacing.row}"
  grid-row-zebra:
    backgroundColor: "{colors.zebra}"
  grid-row-selected:
    backgroundColor: "{colors.sand-wash}"
  grid-row-open:
    backgroundColor: "{colors.channel}"
  grid-gutter:
    textColor: "{colors.chalk-dim}"
    typography: "{typography.micro}"
    width: "{spacing.grid-gutter}"
  column-strip:
    backgroundColor: "{colors.deep-water}"
    height: "{spacing.column-strip}"
  column-chip:
    textColor: "{colors.chalk}"
    rounded: "{rounded.none}"
    height: "{spacing.row}"
    padding: "0 8px"
  pending-column:
    width: "{spacing.pending-column}"
  panel:
    backgroundColor: "{colors.deep-water}"
    textColor: "{colors.chalk}"
    rounded: "{rounded.none}"
    width: "{spacing.panel}"
    padding: "16px"
  panel-collapsed:
    backgroundColor: "{colors.deep-water}"
    textColor: "{colors.chalk}"
    width: "{spacing.panel-collapsed}"
  checkbox:
    backgroundColor: "{colors.ink}"
    rounded: "{rounded.none}"
    size: "16px"
  radio-tab:
    textColor: "{colors.chalk-dim}"
    typography: "{typography.body}"
    rounded: "{rounded.none}"
    padding: "6px 12px"
  radio-tab-checked:
    backgroundColor: "{colors.channel}"
    textColor: "{colors.chalk}"
  tag:
    textColor: "{colors.chalk-dim}"
    typography: "{typography.micro}"
    rounded: "{rounded.none}"
    padding: "0 4px"
---

# Design System: Chelabase Studio

Scope: the signed-in app shell (`src/components/app-shell/`), the table editor (`src/components/table-editor/`, the data-grid and right-panel patterns every later data surface reuses) and the lagoon tokens (`@chelabase/design/tokens.css`; `src/styles.css` keeps only the shadcn map) as Studio's app system. The landing page's nautical-chart vocabulary belongs to the landing and is not recorded here.

## Overview

**Creative North Star: "The Tool Frame in Lagoon Water"**

Studio is a dark tool that recedes. One deep-water rail and an ink header, separated only by hairlines, frame a page body where developers work for long stretches. The lagoon world shows in precise details rather than in decoration: chalk text, pastel colours that each carry one meaning, square-cut chrome, and a brand that speaks once, as the claw wordmark and a sand "you are here" mark.

Density is calm and even: 36px rows, 18px line icons, a 14px UI face built for legibility. There are no depth bands, chart drawings or neat-line ticks in the app chrome. The confirmed rejections are a grey or green SaaS sidebar with glowing active pills, tinted cards and icon tiles; neon glow; purple SaaS; cream-and-terracotta editorial; and anything taken from Supabase's look, menu tree or wording.

Data surfaces keep structure and data on one plane: a table's columns stay in sight as a typed strip over its rows, and every change to them is staged, shown as exact SQL with its dry-run line, and applied once from one right panel.

The shell is desktop first. Phone-width behaviour is an open gap, not a rule (see Layout).

**Key Characteristics:**
- Dark by default: `color-scheme: dark` on `:root`, lagoon ink ground everywhere, auth pages included.
- Sand is the only action, selection and pending colour; coral means hazard; seafoam means yes.
- Hairlines, not fills or shadows, separate regions; data grids add zebra rows and a hairline on every column edge.
- Shell chrome is square-cut; focus is a square 2px sand outline.
- Spectral appears only in the wordmark.

## Colors

A warm matte-black (Basalt) dark carrying pastel roles, each with one meaning.

### Primary
- **Sand** (`sand`): the one action, selection and pending colour. Primary buttons (ink text on sand), the 2px active edge-mark in the rail and the table list, the checked indicator in the behaviour menu, checkbox ticks and radio dots, the chosen segment's underline, the resize grip, every focus outline (`--ring`), and the global text selection (solid sand under ink glyphs). In the SQL editor the selection is a sand tint with chalk glyphs (`sand-tint`, sand at 25%).
- **Sand Wash** (`sand-wash`, sand 8%): a selected grid row (and its gutter cell, mixed onto ink so the sticky gutter stays opaque).
- **Pending Edge** (`pending-edge`, sand 45%): the 2px top edge of a pending column's header in the grid. Its placeholder cells take a dashed sand-30% left edge over a sand-3% fill.

### Secondary
- **Seafoam** (`seafoam`): yes, success, enabled. Used sparingly for positive state: the passed dry-run line and its dot, the 2px edge-mark on a column just applied.
- **Coral** (`coral`): hazard. Mapped to `--destructive`: errors, destructive actions, the SQL error underline (wavy, 3px offset), and every warning: a dry-run warning's outlined coral square, the coral-bordered stale-table, dependents and failure boxes, the RLS-off banner (coral 60% border on a coral 8% wash), the strike and edge-mark of a dropped column.

### Tertiary
- **Palm** (`palm`): a defined lagoon role held in reserve; the spec assigns it to SQL type names if the editor's syntax colours are ever remapped onto the lagoon.

### Neutral
- **Lagoon Ink** (`ink`): the ground of the page and the header (`--background`); also `--primary-foreground`, the grid's ground and the fill of fields and checkboxes.
- **Deep Water** (`deep-water`): the second layer (`--muted`): the rail, menu popups, tooltips, the editor gutter, the table list, the column strip, the right panel and its collapsed strip.
- **Channel** (`channel`): secondary surfaces (`--secondary`): the grid's header row, the current table in the list, the row open in the row panel, the chosen tab segment.
- **Chalk** (`chalk`): primary text (`--foreground`), active and hovered rail items, cell values.
- **Chalk Dim** (`chalk-dim`): secondary text (`--muted-foreground`), inactive rail items, placeholders, grid headers, the gutter's row numbers, NULL cells.
- **Hairline** (`hairline`, chalk 16%): `--border`; the header's bottom rule, the rail foot's top rule, the editor frame, every grid column's right edge and every row's bottom edge.
- **Strong Hairline** (`hairline-strong`, chalk 34%): the rail's right edge, popup and tooltip borders, placeholder dashes, scrollbar thumbs, the grid header's and the column strip's bottom rules, the right panel's left edge.
- **Control Border** (`control-border`, chalk 40%): `--input`, the border of form controls, checkboxes and segmented groups. 40% clears WCAG 1.4.11's 3:1 on ink, deep water and channel; 34% did not on every ground.
- **Chalk Washes** (`chalk-wash` 6%, `chalk-wash-strong` 8%): hover and highlight fills. 6% on rail rows, the behaviour trigger and segments; 8% on the header trigger and menu items.
- **Zebra** (`zebra`, chalk 4.5%): every second grid row (and its gutter cell, mixed onto ink).

### Reserved
- **Shallows** (`shallows`, `--shallows`): a defined lagoon value shared with the landing page; nothing in Studio uses it today. Do not use it without a decision.
- **Sand Lit** (`sand-lit`, `--sand-lit`, `--color-sand-lit`): the primary button's hover (Studio's default button and Scalar's), lighter sand with the landing's soft sand glow and a 1px lift, matching the landing's primary plate. It never dims.

### Brand mark
- **Shell, Shell Dark, Shell Light, Shell Tip** (`shell`, `shell-dark`, `shell-light`, `shell-tip`): the claw mark's fills only. They never colour UI. The claw's seams are drawn in `--claw-gap`, the ground it sits on (deep water in the rail).

### Named Rules
**The One Meaning Rule.** Every pastel has one job: sand acts, selects and marks what is pending; coral warns; seafoam affirms. A pastel is never used as decoration or as a second accent.

**The Sand Never Warns Rule.** A warning, a risk or a failure is coral (an outlined coral square for a warning, a solid one for an error, a coral border around a box that needs attention). Sand on a staged change means "yours, not applied yet", never "careful".

**The Sand Is Here Rule.** The current location is marked by a 2px sand bar on the rail's edge plus chalk text, never by a filled or glowing pill.

## Typography

**Display Font:** Spectral 700 (with Iowan Old Style, Georgia), wordmark only
**Body Font:** Atkinson Hyperlegible Next Variable (with Atkinson Hyperlegible, system-ui)
**Label/Mono Font:** JetBrains Mono Variable (with ui-monospace)

**Character:** a legibility-first sans for every piece of UI, a single bookish serif moment in the name, and a sturdy mono for code and identifiers. All three are self-hosted through `@fontsource`; no font CDN.

### Hierarchy
- **Wordmark** (Spectral 700, 1.25rem, line-height 1, -0.02em): "Chelabase" set as the claw C plus "helabase", in the expanded rail only. Collapsed, the claw mark alone (28px) stands in.
- **Headline** (600, 1.5rem): page titles in the page body (project name, tenant slug).
- **Title** (600, 1rem, line-height 1.5rem): a right panel's heading ("Pending changes · 3", "New column").
- **Body** (400, 0.875rem): all shell text: rail labels, the account trigger, menu items, form labels and fields, segments.
- **Note** (400, 13px): the right panel's running text: dry-run lines, warnings, errors, edit labels, explanations. One step under body so a panel's prose sits below its controls.
- **Label** (400, 0.75rem): tooltips, the email line in the account menu, grid header names, field errors.
- **Micro** (400, 11px, line-height 1rem): the smallest step, for dense annotations that sit beside larger text: the grid gutter's row numbers, the column strip's chips (in mono: name over short type and key marks), and tags (`key`, `new`, `pending`, an object's kind, a shortcut key). Always on a solid ground in chalk or chalk dim, never as running text or a control's only label.
- **Code** (JetBrains Mono, 13px in the editor, the row panel's fields and the edit list, 12px in hover docs, SQL blocks and grid cells): SQL, identifiers, cell values, table and column names.

### Named Rules
**The One Serif Rule.** Spectral is the wordmark and nothing else. Headings, labels and buttons stay in Atkinson.

**The Mono Identifier Rule.** Anything Postgres names (tables, columns, types, SQL, cell values) is set in JetBrains Mono; everything the UI says about it is Atkinson.

## Layout

The signed-in frame is a two-column grid: the rail column on the left, and on the right a sticky header over the routed page (`<main id="content">`). The shell mounts once for every `/app/*` route, so the rail and header never remount on navigation.

- **Rail:** full height, sticky. Collapsed width `rail-collapsed`, expanded `rail-expanded`. Three behaviours, picked from the rail's foot and persisted (`chelabase.studio.rail`): Expanded (the column is 232px, content reflows), Collapsed (56px, labels in tooltips), Expand on hover (default: the column stays 56px and the panel widens over the page, no reflow). Hover opens after 150ms of rest and closes 200ms after leave; keyboard focus opens it; Escape closes it; an open behaviour menu holds it open.
- **Rail anatomy:** the home link (52px tall, aligned with the header) at the top, the item list (36px rows, 4px apart, 8px vertical padding), and the behaviour trigger at the foot behind a hairline. Icons sit centred in the collapsed rail at a 19px inset.
- **Header:** `header` tall, 16px side gutters, sticky, a hairline below. Left: the context slot (the table editor fills it with one labelled picker per level). Right: the account trigger.
- **Page body:** pages own their composition. Dashboard and project pages use a centred `page-column` (56rem) with `page-gutter` padding; the API reference runs full width.
- **Data surfaces (the table editor):** a `table-list` (248px) deep-water list on the left behind a hairline; the work area (table bar, then the column strip and the grid filling the rest); and at most one right panel. The grid's gutter is `grid-gutter` (88px: checkbox, row number, open-row button); rows and the header row are `row` (36px); a pending column's track is `pending-column` (160px).
- **The right panel:** one panel at a time (pending changes, a column form or a row), `panel` wide (360px; the row panel `panel-wide`, 26rem, at most 45vw), deep water behind a strong hairline. It collapses to a `panel-collapsed` (48px) strip that keeps the view mounted, so a form keeps its draft.
- **Motion:** width changes take 200ms on `ease-design` (`cubic-bezier(0.16, 1, 0.3, 1)`), the rail's width and the grid column alike. Under `prefers-reduced-motion` they are instant.
- **Open gap: phone width.** Mobile is not designed. A later pass gives the rail a drawer and the header its own menu trigger. Until then, nothing here prescribes narrow-screen behaviour.

### Named Rules
**The One Panel Rule.** A surface has at most one right panel, with one collapse control in its heading row and one 48px strip when collapsed. A second view replaces the first; it never stacks beside it.

## Elevation & Depth

Flat. There are no shadows anywhere in the shell, including when the hover-mode rail overlays the page. Depth is tonal and linear: ink is the ground, deep water is the raised layer (rail, popups, tooltips, the table list, the column strip, the right panel), channel marks the grid's header row and what is current, and hairlines draw the edges. Overlays are separated from the page by a 34% chalk hairline, not by a shadow or a scrim. A 2px inset edge (drawn with an inset box-shadow) is an edge-mark, not elevation: it marks state on one side of a row, chip or segment.

### Named Rules
**The Hairline Not Shadow Rule.** A surface that sits above another gets a deep-water fill and a strong hairline. It never gets a drop shadow or glow.

## Shapes

Shell chrome is square-cut: rail rows, the rail panel, the header trigger, menus, menu items and tooltips have no radius, and focus outlines are square. The table editor is square throughout: grid cells, chips, the panel, its fields, checkboxes, segments and tags. The shared button and the SQL editor frame still carry the inherited 8px radius (`rounded.lg`, `--radius: 0.5rem`) until the lagoon "plates" pass restyles buttons and dialogs; that pass decides their final form.

A dashed outline means "not there yet": the rail's placeholder slots wear a dashed strong hairline around a chalk-dim label (not built yet), and a pending column's placeholder cells wear a dashed sand edge (not applied yet).

Round is reserved for one-of-many: radio dots are round, checkboxes square.

Icons are lucide line icons at 18px with a 1.5 stroke in the shell (16px inside menus and panel controls, 14px and 12px inside the grid and chips).

## Components

### Rail
Deep-water panel, a strong hairline on its right edge, no shadow.
- **Items:** 36px rows, icon then label (12px gap), chalk dim at rest.
- **Hover:** chalk text on a 6% chalk wash.
- **Active:** chalk text and a 2px sand bar on the rail's left edge, inset 6px top and bottom; `aria-current="page"`.
- **Focus:** a 2px sand outline drawn inside the row (offset -2px), square.
- **Placeholder:** non-interactive, out of the tab order, `aria-disabled`; dashed strong hairline, chalk-dim label, 8px side margin.
- **Collapsed tooltip:** to the right, 8px away, deep water with a strong hairline, 12px chalk text; 300ms delay.

### Rail behaviour menu
The rail's foot: an icon trigger named "Sidebar behaviour" ("Sidebar" label when expanded). Its popup opens to the right: deep water, strong hairline, three radio items (Expanded, Collapsed, Expand on hover) with a sand check in a 1rem leading column.

### Header and account menu
Ink ground, hairline below, no fill change on scroll.
- **Account trigger:** 36px tall, CircleUser icon plus the name (the email when there is no name); hover and open state take an 8% chalk wash; focus is a 2px sand outline offset 2px outside.
- **Account menu:** aligned to the trigger's end, 6px below; deep water, strong hairline, 4px inner padding; the email at 12px, then Sign out with its icon.

### Menu items (all menus)
- **Highlighted:** 8% chalk wash plus a 2px sand outline inset by 2px, so keyboard position is visible beyond the wash alone.

### Buttons
- **Primary:** sand fill, ink text, 14px medium, 32px tall, 8px radius (inherited); hover lifts to `sand-lit` with a soft sand glow and a 1px rise (see Sand Lit). The page's main action (New project) is the only sand fill in view.
- **One sand primary per panel:** a panel, popover or dialog has exactly one sand-filled button, its commit ("Apply 3 changes", "Add to changes", "Save row"), at the end of its footer row. Everything else is outline or ghost; Delete and Discard confirmations are destructive.
- **Open gap: focus.** The shared button's focus is still the shadcn glow (`ring-3 ring/50`); the 2px square sand outline shown for the primary button is the target, not what ships.
- Other variants (outline, secondary, ghost, destructive) are the inherited shared button mapped onto the lagoon tokens; their lagoon restyle is pending.

### Inputs / Fields
- **Style:** ink ground, a 1px `control-border` (chalk 40%), 8px horizontal padding, 14px text; inherited 8px radius in the shared field. The table editor's fields are square and 36px tall.
- **Focus:** the table editor's fields draw a 2px sand outline inset by 1px; elsewhere no shared field focus treatment exists yet (fields show the browser default).
- **Invalid:** the border turns coral and a 12px coral line sits under the field.

### Checkboxes
Themed, never the browser's: a 16px square native checkbox on ink with a `control-border` edge. Checked, the edge turns sand and a drawn sand check (12px, stroke 2.5) sits inside; indeterminate shows a 2×8px sand bar. Focus is a 2px sand outline offset 1px. A labelled checkbox row is 32px tall with an 8px gap.

### Segmented radio
One row of joined options in a `control-border` frame, a hairline between segments, 14px medium text at 6px × 12px. Two tones:
- **`tab`** (in-form choices such as a column's Default: None / Value / Expression): chalk-dim at rest, chalk on a 6% wash on hover; the chosen segment is channel with chalk text and a 2px sand underline, the same mark as the Rows | Structure tabs.
- **`sand`** (the default): the chosen segment is sand-filled with ink text. Keep it for choices that stand alone (platform roles); inside a panel, use `tab`, so the panel's one sand fill stays its commit.
- **Stacked** groups (one option per line with a hint) use a round 16px ring with a sand dot.

### Tabs
Rows | Structure: 14px chalk-dim text, chalk on hover; the active tab is channel with chalk text and a 2px sand underline. Square, no fill change beyond channel.

### Data grid
The rows of a table: an ARIA grid on ink, virtualized, with a sticky header row and a sticky gutter.
- **Rows:** 36px, mono 12px chalk; NULL in chalk dim. A hairline under every row and on every column's right edge. Every second row is zebra (chalk 4.5%). A selected row is a sand wash (sand 8%); the row open in the row panel is channel with a 2px sand inset edge on its left.
- **Header row:** channel, 12px chalk-dim mono names, a strong hairline below. Hover or sorted turns the name chalk; a 12px arrow shows the direction. Clicking sorts.
- **Gutter:** 88px, sticky: a themed checkbox, the row number in micro chalk dim, and an open-row button that shows on hover.
- **Resize grip:** a 5px strip straddling each header's right edge; sand on hover and while dragging. Alternatives that need no drag (WCAG 2.5.7): click the edge to widen 16px, Shift+click to narrow, double-click to reset; on a focused header, Alt+Left / Alt+Right steps 16px. Widths clamp to 64–960px. The header and its grip are named separately (the grip is a `separator`, "Resize <column>").
- **Pending columns:** a column added in the pending changes gets its own 160px track after the table's columns: its header carries a 2px sand-45% top edge and a chalk-dim mono name; its cells are empty placeholders with a dashed sand-30% left edge over a sand-3% fill, outside the keyboard model.
- **Cells:** focus is a 2px sand outline inset by 2px. Enter or F2 edits, typing starts editing, Escape cancels; arrows, Home/End, PageUp/PageDown move.

### Column strip
A typed strip over the grid's columns, sharing the grid's template and scroll so each chip sits exactly over its column. Deep water, 44px, a strong hairline below; an empty cell over the gutter keeps alignment.
- **Chip:** 36px, a hairline on its right edge. Two lines in mono micro: the column name in chalk, then the short type (`int8`, `timestamptz`, `varchar(80)[]`; the full name in its title and accessible name) and key marks (`key`, `→ fk`, `null`) in chalk dim, joined by ` · `. A 24px column-actions menu at the right. Hover is a 5% chalk wash.
- **Pending marks:** added and changed chips carry a 2px sand inset left edge (added also a sand "new"); dropped chips a coral edge and a coral strike; a column just applied a seafoam edge.
- **Add column:** at the strip's end, an outline button on channel whose border turns sand on hover.

### Right panel
Deep water, a strong hairline on its left edge, 16px padding, 12px between blocks. The heading row holds a title-step heading and the one collapse control: a 28px square with a hairline frame and a 16px PanelRightClose icon.
- **Collapsed strip:** 48px, deep water, a strong hairline on its left: a 36px expand button (control border, sand on hover) above the open view's name set vertically in 14px chalk, led by an 8px sand square ("Pending changes · 3", "New column · draft, not added yet").
- **Pending changes:** a numbered list of edits (mono 13px, hairline-framed rows, a remove control), the SQL block (ink, hairline frame, mono 12px; the highlighted edit's statement in sand), then the outcome lines and the footer (Discard ghost, Apply sand).

### Dry-run result
One line. A passed run reads "Dry run passed · ACCESS EXCLUSIVE lock · no rewrite" (or "rewrites the table", "no table lock") after an 8px seafoam square: seafoam text in the changes panel, chalk dim in the column form's preview. While running: a chalk-dim dash and "Checking with a dry run…". A warning is its own line after an outlined coral square in chalk; an error is a solid coral square with a coral "Error:" line and chalk-dim detail and hint. Lock lines show only for a run that didn't pass or a change that wasn't simulated.

### Tags
Micro (11px) text, square, a hairline or strong-hairline frame with 4–6px side padding, chalk dim (`key`, an object's tag, a shortcut key); or unframed coloured words beside a name (`new` and `pending` in sand, `dropped` in coral).

### SQL editor
Ink ground, chalk text, 13px JetBrains Mono, a hairline frame, a deep-water gutter in chalk dim. Focus draws a 2px sand outline. Selection is a sand tint with chalk glyphs. Errors take a wavy coral underline.

### Skip link
The first focusable element: "Skip to content", revealed on focus top-left on deep water with a sand outline; it targets `#content`.

### API reference (Scalar)
Scalar is forced into dark mode with its toggle hidden, sits under the 52px header, and is themed onto the lagoon tokens by `@chelabase/design/scalar.css`, scoped to the `.design-scalar` wrapper (nothing global). Backgrounds 1/2/3 are ink/deep/channel, text chalk/chalk-dim, accent and primary button sand (ink text), borders `--input` (3:1), shadows replaced by a hairline, fonts the Studio sans and mono. Method colours are a small set of their own, the one carve-out from One Meaning: each HTTP method gets its own hue, as API docs conventionally do. POST and 2xx seafoam (yes) and DELETE, errors and alerts coral (hazard) keep their lagoon meanings. GET `#9cc8ea` (lagoon blue), PUT `#c4b6ec` (lilac) and PATCH `#e3d58c` (straw) are method-only tones (`--method-get/put/patch`, in the package's `tokens.css`) that mean nothing else in Studio. Scalar also paints 3xx with the PATCH tone and "required" with the PUT tone. OPTIONS, HEAD and TRACE are chalk-dim. All of them reach at least 5.4:1 on ink, deep and channel and on their 10% badge tint. Sand never marks a method or status, and palm stays reserved. Code samples default to JavaScript / Axios.

## Do's and Don'ts

### Do:
- **Do** mark the current section with a 2px sand edge-mark and chalk text.
- **Do** draw every focus state as a 2px square sand outline with no shadow.
- **Do** separate surfaces with hairlines: chalk 16% for dividers, 34% for overlay and popup edges, 40% for control borders.
- **Do** keep popups, menus and tooltips on deep water with a strong hairline.
- **Do** set UI in Atkinson Hyperlegible Next at 14px and icons as lucide line icons at 18px, stroke 1.5.
- **Do** use a dashed outline only to mean "not there yet": not built (a rail placeholder) or not applied (a pending column).
- **Do** make width changes 200ms on `ease-design`, instant under reduced motion.
- **Do** give data grids zebra rows (chalk 4.5%) and a hairline on every column edge and row.
- **Do** keep a table's columns in sight as a typed strip over its grid, each chip over its column, with short type names.
- **Do** give every drag a click and a keyboard alternative (the resize grip: click, Shift+click, double-click, Alt+arrows).
- **Do** use the themed square checkbox and, inside panels, the `tab` segmented radio.
- **Do** keep one sand-filled button per panel, popover or dialog: its commit.
- **Do** state a dry-run result in one line: outcome, lock, rewrite.
- **Do** use the 11px micro step only for gutter numbers, chips and tags.

### Don't:
- **Don't** use glowing or filled active pills, tinted cards or icon tiles in the chrome.
- **Don't** add drop shadows or glows to shell surfaces, overlays included.
- **Don't** use a pastel for anything but its one meaning; never a second accent beside sand.
- **Don't** use sand to warn; warnings and risks are coral.
- **Don't** open a second right panel or give a panel two collapse controls.
- **Don't** ship browser-default checkboxes or a sand-filled segment inside a panel.
- **Don't** use Spectral outside the wordmark.
- **Don't** take Supabase's look, menu tree or wording.
- **Don't** colour UI with the claw mark's shell tones.
