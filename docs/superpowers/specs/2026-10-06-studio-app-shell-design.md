# Studio app shell (skeleton) — design

Date: 2026-10-06. Branch `studio-app-shell` (`.worktrees/studio-app-shell`). Not step 6g, not the landing page.

## Goal

Replace the signed-in frame (`StudioShell`: a top header over a centred `main`) with a real app shell for every `/app/*` route: a collapsible left icon rail, a top header and a main region. Skeleton only: the final navigation IA is a later pass.

## Decisions

| # | Decision | Source |
|---|---|---|
| D1 | Skeleton first: app chrome only, stub nav items, no final section IA. | user (settled) |
| D2 | Inherit the landing page's lagoon world (dark default, pastel roles, type); quieter for a tool UI; no second visual world. | user (settled) |
| D3 | The rail offers three behaviours: Expanded, Collapsed, Expand on hover, picked from a menu at its foot. **Default: Expand on hover.** Persisted in `localStorage`. | user, 2026-10-06 |
| D4 | The dark lagoon theme covers **all of Studio** (auth pages included) by remapping the global `:root` tokens. | user, 2026-10-06 |
| D5 | PRODUCT.md takes the landing branch's three brand lines, and its Accessibility line on dark mode is rewritten (dark is the default from this step). | user, 2026-10-06 |
| D6 | Approach A: an own `AppShell` on Base UI primitives, rendered once by the `/app` layout route around `<Outlet/>`; pages stop wrapping themselves. | user, 2026-10-06 |
| D7 | Desktop first; mobile deferred. | user (settled) |

Supabase Studio (`DefaultLayout`, `Sidebar`) was read as a topology reference only (a left icon rail with expandable / open / closed behaviours). Nothing of its look, menu tree or wording is taken.

## 1. Structure and routing

- `routes/app.tsx` keeps the auth gate and its providers (query client, access token, `AppSession`). Once authenticated it renders `<AppShell><Outlet/></AppShell>` inside them, so the shell mounts once for all `/app/*`. "Redirecting to sign in…" and `SignInError` stay outside the shell.
- New units under `src/components/app-shell/`:
  - `app-shell.tsx`: the frame grid (rail column; header over main), the "Skip to content" link.
  - `app-rail.tsx`: `<nav aria-label="Studio">` with the wordmark, the items and the behaviour menu.
  - `rail-item.tsx`: one entry (icon, label, tooltip when collapsed, active state from the router).
  - `rail-behaviour-menu.tsx`: Base UI Menu with a radio group Expanded / Collapsed / Expand on hover.
  - `app-header.tsx`: an empty left context slot (reserved for a later project/branch switcher; no placeholder content) and a profile menu (name, email, **Sign out**). Sign out runs `queryClient.clear()` and then `onSignOut()`, as today.
- New `src/lib/rail-preference.ts`: key `chelabase.studio.rail`, values `expanded | collapsed | hover`, default `hover`. Every `localStorage` access is in try/catch; a missing or invalid value or a storage error yields `hover`; a failing write is swallowed.
- Pages keep their bodies:
  - `StudioShell` is deleted.
  - `Dashboard` and `ProjectPage` drop the wrapper and the `profile`, `onSignOut` and `nav` props; their root becomes the same `mx-auto max-w-4xl space-y-8 p-8` container, so they render as before. The routes stop passing those props.
  - `ApiPage` drops its own header ("Chelabase Studio", "Back to dashboard") and stays full width.
  - `ApiNavLink` and its test are removed; the API reference is a rail item.
- Rail items (stubs, no IA):
  - **Projects** → `/app`; active on `/app` and `/app/projects/*`.
  - **API reference** → `/app/api`; active on `/app/api`.
  - Two **placeholder slots**: non-interactive, `aria-disabled`, out of the tab order, labelled "Placeholder", dashed outline.

## 2. Rail behaviour and accessibility

- Widths: collapsed 56px, expanded 232px.
- **Expanded:** the rail occupies 232px in the grid; content reflows; labels visible; no tooltips.
- **Collapsed:** the rail occupies 56px; labels stay in the DOM visually hidden and name the links; a Base UI tooltip shows the label on hover and keyboard focus (short delay, instant between items, Escape dismisses).
- **Expand on hover (default):** the grid column stays 56px; the rail panel widens to 232px over the content (no reflow).
  - Opens on pointer enter after ~150ms of intent; closes ~200ms after pointer leave.
  - Focus inside the rail opens it; Escape closes it and keeps focus.
  - Stays open while the behaviour menu is open.
  - No tooltips in this mode.
  - Meets WCAG 2.2 SC 1.4.13 (hoverable, persistent, dismissible).
- The behaviour menu trigger is an icon button named "Sidebar behaviour"; the three options are radio items exposing the checked state; a pick applies at once and is persisted. The preference is read synchronously on first render (no flash).
- Landmarks: `nav`, `header`, `<main id="content" tabIndex={-1}>`; a "Skip to content" link is the first focusable element; the active item has `aria-current="page"`.
- Width transitions: 200ms, `cubic-bezier(0.16, 1, 0.3, 1)`; instant under `prefers-reduced-motion`.
- Out of v1: a toggle shortcut, cross-tab sync, a mobile drawer.

## 3. Visual direction (Operate)

The shell disappears into the task; the lagoon world shows in precise details. No depth bands, chart drawings or neat-line ticks in the app chrome.

Global `:root` tokens:

| Token | Value | Role |
|---|---|---|
| `--background` | `#0b1c1f` (lagoon ink) | page and header ground |
| `--foreground` | `#e6ede7` (chalk) | text |
| `--muted` | `#0f2a2d` (deep water) | second neutral layer (rail, panels) |
| `--secondary` | `#14363a` (channel) | secondary surfaces |
| `--muted-foreground` | `#aebfc0` (chalk dim) | secondary text |
| `--border` | `rgb(230 237 231 / 0.16)` | hairlines |
| `--input` | `rgb(230 237 231 / 0.34)` | control borders (3:1) |
| `--primary` / `--primary-foreground` | `#f2c7a3` (sand) / `#0b1c1f` | the one action colour |
| `--destructive` | `#eda6aa` (coral) | hazard |
| `--ring` | `#f2c7a3` | focus |

`secondary-foreground` is chalk. `color-scheme: dark` is set on `:root`.

- Type: Atkinson Hyperlegible Next for all UI on a fixed rem scale; Spectral only in the wordmark; JetBrains Mono for code. Self-hosted through `@fontsource` (latest versions at plan time); no font CDN.
- Rail:
  - Deep-water panel with a 34% chalk hairline on its right edge; no shadow, also when overlaying.
  - 36px rows; 18px lucide icons at stroke 1.5.
  - Inactive chalk dim; hover chalk on a faint chalk wash.
  - **Active:** chalk text and a 2px sand mark on the rail's edge (the shell's signature detail: sand as "you are here").
  - Focus: 2px sand outline.
  - Placeholders: dashed hairline outline, chalk-dim label.
- Wordmark: the landing's `Wordmark` when the rail is expanded, its `ClawMark` when collapsed. `crab-marks.tsx` is copied verbatim to the landing branch's path (`src/components/landing/crab-marks.tsx`) so merging `landing-page` later doesn't conflict. Both stay provisional.
- Header: ink ground, 16% hairline at the bottom, 52px tall. The profile trigger shows the name (the email when there is no name).
- Knock-on effects, by token remap only:
  - every page goes dark, the auth pages included (one visual check);
  - Scalar is forced into its dark mode, keeping its own palette; **superseded by D8 (user, 2026-10-06, at the merge pause):** Scalar takes the lagoon palette through its `--scalar-*` variables, scoped to the API page wrapper, and its default code-sample client is JavaScript Axios (`defaultHttpClient: { targetKey: "js", clientKey: "axios" }`). The rail's "Sidebar behaviour" trigger gets a tooltip, as the rail links have, whenever its label is hidden;
  - if the SQL editor's syntax colours fail AA on ink, they map to the landing's code roles (keyword coral, string sand, function seafoam, type palm);
  - `ui/button.tsx` and `ui/dialog.tsx` keep their shapes and take the tokens (no lagoon "plates" in this step).
- Anti-goals: no Supabase skin or menu tree, no green SaaS, no purple template, no cream editorial, no glow, no second visual world.
- Before any UI code, the direction contract (THESIS, OWN-WORLD, STORY, FIRST VIEWPORT, FORM, FINISH) goes into the shell's Impeccable surface brief. DESIGN.md and `.impeccable/design.json` are written at the finish by the documenter.

## 4. Testing and verification

Strict TDD (a failing run before code).

- `rail-preference.test.ts`: default `hover` when missing; stored valid value returned; invalid → `hover`; read throws → `hover`; a throwing write doesn't throw; `set` persists.
- `app-rail.test.tsx`:
  - expanded: labels visible, no tooltip;
  - collapsed: links keep accessible names, a tooltip with the label appears on focus;
  - hover: `data-expanded` on after pointer enter past the intent delay (fake timers), off after leave plus grace, on with focus-within, off on Escape with focus kept;
  - `aria-current="page"` on Projects at `/app` and `/app/projects/x`, on API reference at `/app/api`;
  - placeholders out of the tab order and `aria-disabled`.
- `rail-behaviour-menu.test.tsx`: trigger named "Sidebar behaviour"; three radio items, the current one checked; a pick updates and persists the preference.
- `app-header.test.tsx`: name shown (email fallback); the menu shows the email; Sign out clears the query cache before calling `onSignOut` (the moved Dashboard test, order asserted).
- `app-shell.test.tsx`: skip link targets `#content`; `nav`, `header`, `main` landmarks; children render in main; the rail is not remounted across a child route change.
- Existing tests: `dashboard.test.tsx` and `project-page.test.tsx` drop the frame props; `api-nav-link.test.tsx` goes with its component; `api-reference.test.tsx` asserts the forced dark mode.
- Gates: Studio `test`, `typecheck`, `build` green; `impeccable detect --json` over the changed UI once at the end.
- Browser pass through `just live` (one run):
  - Playwright at 1440 and 1280: `/app`, a project page, `/app/api`, sign-in;
  - each rail mode (expanded, collapsed, hover-open);
  - a keyboard-only pass (skip link, rail, behaviour menu, sign out);
  - AA contrast spot-check of the new tokens and SQL syntax colours;
  - screenshots in `apps/studio/.impeccable/review/`.
- Finish:
  1. Impeccable finish reviewer (opus) against the direction contract; one fix batch.
  2. The documenter writes DESIGN.md (app-shell scope) and `.impeccable/design.json`.
  3. End-of-step whole-branch review (opus).
  4. Pause for the user before merging.
- Not tested: mobile (deferred).

## Out of scope

Final nav IA, splitting project tabs into routes, the Tables/SQL redesign, the landing page, docs, backend, lagoon restyling of buttons/dialogs, light mode.
