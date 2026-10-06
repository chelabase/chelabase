---
name: Chelabase Studio
description: The signed-in app system for Chelabase Studio, a tool frame on the dusk lagoon.
colors:
  ink: "#0b1c1f"
  deep-water: "#0f2a2d"
  channel: "#14363a"
  shallows: "#163d3e"
  sand: "#f2c7a3"
  sand-lit: "#f7d6b9"
  seafoam: "#9fd4c6"
  coral: "#eda6aa"
  palm: "#a9c89a"
  chalk: "#e6ede7"
  chalk-dim: "#aebfc0"
  hairline: "rgb(230 237 231 / 0.16)"
  hairline-strong: "rgb(230 237 231 / 0.34)"
  control-border: "rgb(230 237 231 / 0.4)"
  chalk-wash: "rgb(230 237 231 / 0.06)"
  chalk-wash-strong: "rgb(230 237 231 / 0.08)"
  sand-tint: "rgb(242 199 163 / 0.25)"
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
  body:
    fontFamily: "Atkinson Hyperlegible Next Variable, Atkinson Hyperlegible, system-ui, sans-serif"
    fontSize: "0.875rem"
    fontWeight: 400
    lineHeight: "1.25rem"
  label:
    fontFamily: "Atkinson Hyperlegible Next Variable, Atkinson Hyperlegible, system-ui, sans-serif"
    fontSize: "0.75rem"
    fontWeight: 400
    lineHeight: "1rem"
  code:
    fontFamily: "JetBrains Mono Variable, ui-monospace, monospace"
    fontSize: "13px"
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
---

# Design System: Chelabase Studio

Scope: the signed-in app shell (`src/components/app-shell/`) and the lagoon tokens in `src/styles.css` as Studio's app system. The landing page's nautical-chart vocabulary belongs to the landing and is not recorded here.

## Overview

**Creative North Star: "The Tool Frame in Lagoon Water"**

Studio is a dark tool that recedes. One deep-water rail and an ink header, separated only by hairlines, frame a page body where developers work for long stretches. The lagoon world shows in precise details rather than in decoration: chalk text, pastel colours that each carry one meaning, square-cut chrome, and a brand that speaks once, as the claw wordmark and a sand "you are here" mark.

Density is calm and even: 36px rows, 18px line icons, a 14px UI face built for legibility. There are no depth bands, chart drawings or neat-line ticks in the app chrome. The confirmed rejections are a grey or green SaaS sidebar with glowing active pills, tinted cards and icon tiles; neon glow; purple SaaS; cream-and-terracotta editorial; and anything taken from Supabase's look, menu tree or wording.

The shell is desktop first. Phone-width behaviour is an open gap, not a rule (see Layout).

**Key Characteristics:**
- Dark by default: `color-scheme: dark` on `:root`, lagoon ink ground everywhere, auth pages included.
- Sand is the only action and selection colour; coral means hazard; seafoam means yes.
- Hairlines, not fills or shadows, separate regions.
- Shell chrome is square-cut; focus is a square 2px sand outline.
- Spectral appears only in the wordmark.

## Colors

A deep lagoon-teal dark carrying pastel roles, each with one meaning.

### Primary
- **Sand** (`sand`): the one action and selection colour. Primary buttons (ink text on sand), the 2px active edge-mark in the rail, the checked indicator in the behaviour menu, every focus outline (`--ring`), and the global text selection (solid sand under ink glyphs). In the SQL editor the selection is a sand tint with chalk glyphs (`sand-tint`, sand at 25%).

### Secondary
- **Seafoam** (`seafoam`): yes, success, enabled. Used sparingly for positive state.
- **Coral** (`coral`): hazard. Mapped to `--destructive`: errors, destructive actions, the SQL error underline (wavy, 3px offset).

### Tertiary
- **Palm** (`palm`): a defined lagoon role held in reserve; the spec assigns it to SQL type names if the editor's syntax colours are ever remapped onto the lagoon.

### Neutral
- **Lagoon Ink** (`ink`): the ground of the page and the header (`--background`); also `--primary-foreground`.
- **Deep Water** (`deep-water`): the second layer (`--muted`): the rail, menu popups, tooltips, the editor gutter.
- **Channel** (`channel`): secondary surfaces (`--secondary`).
- **Chalk** (`chalk`): primary text (`--foreground`), active and hovered rail items.
- **Chalk Dim** (`chalk-dim`): secondary text (`--muted-foreground`), inactive rail items, placeholders.
- **Hairline** (`hairline`, chalk 16%): `--border`; the header's bottom rule, the rail foot's top rule, the editor frame.
- **Strong Hairline** (`hairline-strong`, chalk 34%): the rail's right edge, popup and tooltip borders, placeholder dashes, scrollbar thumbs.
- **Control Border** (`control-border`, chalk 40%): `--input`, the border of form controls. 40% clears WCAG 1.4.11's 3:1 on ink, deep water and channel; 34% did not on every ground.
- **Chalk Washes** (`chalk-wash` 6%, `chalk-wash-strong` 8%): hover and highlight fills. 6% on rail rows and the behaviour trigger; 8% on the header trigger and menu items.

### Reserved
- **Shallows** (`shallows`, `--shallows`): a defined lagoon value shared with the landing page; nothing in Studio uses it today. Do not use it without a decision.
- **Sand Lit** (`sand-lit`, `--sand-lit`, `--color-sand-lit`): the primary button's hover (Studio's default button and Scalar's), lighter sand with the landing's soft sand glow and a 1px lift, matching the landing's primary plate. It never dims.

### Brand mark
- **Shell, Shell Dark, Shell Light, Shell Tip** (`shell`, `shell-dark`, `shell-light`, `shell-tip`): the claw mark's fills only. They never colour UI. The claw's seams are drawn in `--claw-gap`, the ground it sits on (deep water in the rail).

### Named Rules
**The One Meaning Rule.** Every pastel has one job: sand acts and selects, coral warns, seafoam affirms. A pastel is never used as decoration or as a second accent.

**The Sand Is Here Rule.** The current location is marked by a 2px sand bar on the rail's edge plus chalk text, never by a filled or glowing pill.

## Typography

**Display Font:** Spectral 700 (with Iowan Old Style, Georgia), wordmark only
**Body Font:** Atkinson Hyperlegible Next Variable (with Atkinson Hyperlegible, system-ui)
**Label/Mono Font:** JetBrains Mono Variable (with ui-monospace)

**Character:** a legibility-first sans for every piece of UI, a single bookish serif moment in the name, and a sturdy mono for code. All three are self-hosted through `@fontsource`; no font CDN.

### Hierarchy
- **Wordmark** (Spectral 700, 1.25rem, line-height 1, -0.02em): "Chelabase" set as the claw C plus "helabase", in the expanded rail only. Collapsed, the claw mark alone (28px) stands in.
- **Headline** (600, 1.5rem): page titles in the page body (project name, tenant slug).
- **Body** (400, 0.875rem): all shell text: rail labels, the account trigger, menu items.
- **Label** (400, 0.75rem): tooltips, the email line in the account menu.
- **Code** (JetBrains Mono, 13px in the editor, 12px in hover docs): SQL and identifiers.

### Named Rules
**The One Serif Rule.** Spectral is the wordmark and nothing else. Headings, labels and buttons stay in Atkinson.

## Layout

The signed-in frame is a two-column grid: the rail column on the left, and on the right a sticky header over the routed page (`<main id="content">`). The shell mounts once for every `/app/*` route, so the rail and header never remount on navigation.

- **Rail:** full height, sticky. Collapsed width `rail-collapsed`, expanded `rail-expanded`. Three behaviours, picked from the rail's foot and persisted (`chelabase.studio.rail`): Expanded (the column is 232px, content reflows), Collapsed (56px, labels in tooltips), Expand on hover (default: the column stays 56px and the panel widens over the page, no reflow). Hover opens after 150ms of rest and closes 200ms after leave; keyboard focus opens it; Escape closes it; an open behaviour menu holds it open.
- **Rail anatomy:** the home link (52px tall, aligned with the header) at the top, the item list (36px rows, 4px apart, 8px vertical padding), and the behaviour trigger at the foot behind a hairline. Icons sit centred in the collapsed rail at a 19px inset.
- **Header:** `header` tall, 16px side gutters, sticky, a hairline below. Left: an empty context slot reserved for a later project/branch switcher. Right: the account trigger.
- **Page body:** pages own their composition. Dashboard and project pages use a centred `page-column` (56rem) with `page-gutter` padding; the API reference runs full width.
- **Motion:** width changes take 200ms on `ease-lagoon` (`cubic-bezier(0.16, 1, 0.3, 1)`), the rail's width and the grid column alike. Under `prefers-reduced-motion` they are instant.
- **Open gap: phone width.** Mobile is not designed. A later pass gives the rail a drawer and the header its own menu trigger. Until then, nothing here prescribes narrow-screen behaviour.

## Elevation & Depth

Flat. There are no shadows anywhere in the shell, including when the hover-mode rail overlays the page. Depth is tonal and linear: ink is the ground, deep water is the raised layer (rail, popups, tooltips), and hairlines draw the edges. Overlays are separated from the page by a 34% chalk hairline, not by a shadow or a scrim.

### Named Rules
**The Hairline Not Shadow Rule.** A surface that sits above another gets a deep-water fill and a strong hairline. It never gets a drop shadow or glow.

## Shapes

Shell chrome is square-cut: rail rows, the rail panel, the header trigger, menus, menu items and tooltips have no radius, and focus outlines are square. The page body's controls (the shared button and the SQL editor frame) still carry the inherited 8px radius (`rounded.lg`, `--radius: 0.5rem`) until the lagoon "plates" pass restyles buttons and dialogs; that pass decides their final form.

A dashed hairline outline means "not built yet": the rail's placeholder slots wear a dashed strong hairline around a chalk-dim label.

Icons are lucide line icons at 18px with a 1.5 stroke in the shell (16px inside menus).

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
- **Primary:** sand fill, ink text, 14px medium, 32px tall, 8px radius (inherited); hover drops to 80% sand. The page's main action (New project) is the only sand fill in view.
- **Open gap: focus.** The shared button's focus is still the shadcn glow (`ring-3 ring/50`); the 2px square sand outline shown for the primary button is the target, not what ships.
- Other variants (outline, secondary, ghost, destructive) are the inherited shared button mapped onto the lagoon tokens; their lagoon restyle is pending.

### Inputs / Fields
- **Style:** ink ground, a 1px `control-border` (chalk 40%), 8px horizontal padding, 14px text; inherited 8px radius.
- **Focus:** no shared field focus treatment exists yet (fields show the browser default); the shell's 2px square sand outline is the target when fields are restyled.

### SQL editor
Ink ground, chalk text, 13px JetBrains Mono, a hairline frame, a deep-water gutter in chalk dim. Focus draws a 2px sand outline. Selection is a sand tint with chalk glyphs. Errors take a wavy coral underline.

### Skip link
The first focusable element: "Skip to content", revealed on focus top-left on deep water with a sand outline; it targets `#content`.

### API reference (Scalar)
Scalar is forced into dark mode with its toggle hidden, sits under the 52px header, and is themed onto the lagoon tokens by `components/api-reference.css`, scoped to the `.studio-api` wrapper (nothing global). Backgrounds 1/2/3 are ink/deep/channel, text chalk/chalk-dim, accent and primary button sand (ink text), borders `--input` (3:1), shadows replaced by a hairline, fonts the Studio sans and mono. Method colours are a small set of their own, the one carve-out from One Meaning: each HTTP method gets its own hue, as API docs conventionally do. POST and 2xx seafoam (yes) and DELETE, errors and alerts coral (hazard) keep their lagoon meanings. GET `#9cc8ea` (lagoon blue), PUT `#c4b6ec` (lilac) and PATCH `#e3d58c` (straw) are method-only tones (`--method-get/put/patch`, scoped to the API page) that mean nothing else in Studio. Scalar also paints 3xx with the PATCH tone and "required" with the PUT tone. OPTIONS, HEAD and TRACE are chalk-dim. All of them reach at least 5.6:1 on ink, deep and channel, and on their 10% badge tint. Sand never marks a method or status, and palm stays reserved. Code samples default to JavaScript / Axios.

## Do's and Don'ts

### Do:
- **Do** mark the current section with a 2px sand edge-mark and chalk text.
- **Do** draw every focus state as a 2px square sand outline with no shadow.
- **Do** separate surfaces with hairlines: chalk 16% for dividers, 34% for overlay and popup edges, 40% for control borders.
- **Do** keep popups, menus and tooltips on deep water with a strong hairline.
- **Do** set UI in Atkinson Hyperlegible Next at 14px and icons as lucide line icons at 18px, stroke 1.5.
- **Do** use a dashed hairline only to mean "not built yet".
- **Do** make width changes 200ms on `ease-lagoon`, instant under reduced motion.

### Don't:
- **Don't** use glowing or filled active pills, tinted cards or icon tiles in the chrome.
- **Don't** add drop shadows or glows to shell surfaces, overlays included.
- **Don't** use a pastel for anything but its one meaning; never a second accent beside sand.
- **Don't** use Spectral outside the wordmark.
- **Don't** take Supabase's look, menu tree or wording.
- **Don't** colour UI with the claw mark's shell tones.
