# Studio App Shell (skeleton) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Every `/app/*` route renders inside one app shell: a left icon rail (Expanded / Collapsed / Expand on hover, default hover, persisted), a top header (profile menu, Sign out) and a main region, with Studio dark in the lagoon palette.

**Architecture:** `routes/app.tsx` (the auth gate) wraps `<Outlet/>` in `<AppShell>` once, so the rail never remounts. The shell is built from small units in `src/components/app-shell/` on Base UI `Menu` and `Tooltip`; the rail preference is a tiny `localStorage`-backed hook. Theming remaps the existing shadcn semantic tokens in `styles.css` to lagoon values, so page bodies go dark without code changes.

**Tech Stack:** React 19, TanStack Router/Query, Tailwind 4, `@base-ui/react` 1.8 (`menu`, `tooltip`), lucide-react, Vitest + Testing Library (jsdom), `@fontsource` 5.3.0.

**Spec:** `docs/superpowers/specs/2026-10-06-studio-app-shell-design.md` (decisions D1–D7). Executors read both.

## Status

- **FINAL (2026-10-06): MERGED into `main` (`--no-ff`) with the user's approval, after `e713350`: each HTTP method gets its own colour (GET `#9cc8ea`, PUT `#c4b6ec`, PATCH `#e3d58c` as method-only tones; POST seafoam, DELETE coral); DESIGN.md records method colours as the one carve-out from One Meaning. Studio 535 tests pass. The branch and its worktree are deleted.**
- **T8 DONE (`69f1196` + `09f2247`).** Opus review of T8: three Importants, fixed directly in `09f2247` at the user's request ("just fix it"; no further review):
  - Scalar's teleported popovers (`body > .scalar-app`) are themed;
  - the sidebar search border uses `--input`;
  - `--sand-lit` is now the primary hover.
  The user also asked for, and confirmed:
  - The primary button hover matches the landing: sand-lit, the soft sand glow and a 1px lift, no dimming.
  - One meaning per colour in Scalar: POST and 2xx seafoam; DELETE, errors and alerts coral; GET, PUT, PATCH, 3xx and "required" chalk; OPTIONS chalk-dim. Sand stays action-only, palm stays reserved.
  The tooltips were confirmed fine by the user. Deferred minor: the "no tooltip while the menu is open" test only clicks (Base UI closes the tooltip on press anyway), so it doesn't prove the `menuOpen` guard. Studio 535 tests pass.
- **T8 history.** The user's answers at the pause:
  - Theme Scalar onto the lagoon palette and make JavaScript Axios the default client, on this branch before the merge (spec D8).
  - Add the "Sidebar behaviour" tooltip now.
  - Shared-component drift (button, dialog, field focus, skip link, ghost hover) goes to a separate lagoon component pass later.
  - Lockfile churn kept (default, no objection).
  - T8 is implemented by sonnet and reviewed by opus (code plus the live captures). Then the merge pause again.
- **T7 DONE (paused at the merge decision, before T8 was added).** All gates pass at `9fc4311`: 529 tests, typecheck, build. The scoped re-review says ready to merge.
  - End-of-step review (opus): no Critical. One Important: the `dark:` variants followed the OS, because `styles.css` had no `@custom-variant dark`. Fixed in `574a7bb` with `@custom-variant dark (&:where(:root, :root *))` and a source test. Live: button colours are identical under light and dark emulation.
  - `574a7bb` also fixed the minors batch: the profile Positioner gets `z-50`; the name falls back with `||` (test); a test for Sign out with an empty profile; the duplicate selector is dropped; a `scrollTo` stub in `setup.ts`; the unused `Link` mock is removed; tests for the default hover and the 232/56 column; `--shallows`/`--sand-lit` marked reserved; the button focus sample recorded as an openGap. `_ariaCurrent`/`_dataStatus` stay: they strip TanStack's forced `aria-current` from the home link.
  - **Ruling** (`9fc4311`): the destructive dark wash went from /20 to /10, and hover from /30 to /15. The old values fell to 4.48 and 3.64 on channel; the new ones keep ≥ 4.96:1 on ink, deep and channel. Cost if wrong: a fainter red wash, two numbers to change back.
  - Scoped re-review (opus): clean, ready to merge. Its minors were deferred:
    - Ghost-button hover (`dark:hover:bg-muted/50`) is nearly invisible on ink and invisible on deep (part of the lagoon button restyle).
    - `styles.test.ts` matches the exact source text.
  - **Questions for the user at the merge pause:**
    - Theme Scalar (fix 1).
    - The focus and radius drift in shared components (button glow, dialog shadow and rounded-xl, field focus, skip link, ghost hover).
    - A tooltip on the icon-only "Sidebar behaviour" trigger.
    - Lockfile churn: `@tanstack/router-plugin` went 1.168.40 → .42, and the docs app's `lucide-react` 1.48 → 1.51. Default: keep.
  - After both this branch and `landing-page` merge, reword the landing line at `PRODUCT.md:45` ("No logo…"), because Studio's rail now shows the wordmark too.
  - **Stack:** `just dev` is still running from this worktree, on `chelabase_metadata_shell`, and the lock is free. The schema-merge sessions' next `just live` run moves it back.
  - At merge: delete `studio-app-shell-t1`…`-t4` and the step branch, as listed under "Leftover branches".
  - The steps 1–3 notes follow:
  - Fix 5 reworked in `91ff4cd`: `.cm-content ::selection` gets its own rule (chalk glyphs, transparent background), and the sand tint comes only from `.cm-selectionBackground`. There's a new theme-CSS test (RED then GREEN). Live: 7.94:1 on a plain line, 5.01:1 on the active line, and the selection stays visible unfocused. Studio 522 tests, typecheck and build pass. `impeccable detect` was not rerun (CLI not on PATH for that agent; the change is outside the detector's scope).
  - Same reviewer, re-scored: **`ship`** (fixes 2–6 all resolved).
  - The fixer's live run moved `just dev` from `schema-merge-t2`'s worktree to this one and left it running, with the lock free.
  - Documenter done: `apps/studio/DESIGN.md` + `apps/studio/.impeccable/design.json` (tokens from `styles.css`; open gaps: phone width, Scalar palette).
  - **Drift the documenter recorded (not fixed; ask the user at the merge pause whether it's in scope):** the shared button's focus is a soft `ring-3 ring/50` glow; the dialog has `shadow-lg` and `rounded-xl`; fields have no focus style of their own (browser default); the skip link is `rounded-md`; the spec says `--input` 34% while the build has 40% (T6's AA fix; the spec is stale); the 8px radius on buttons, fields and the editor is provisional.
  - Minor: `.cm-selectionBackground` appears twice in the editor's sand selector list.
  - Earlier verdict: fixes 2, 3, 4, 6 resolved (fix 4 rests on code and tests; `fix-menu-after-pick.png` is byte-identical to `desktop.png`, so it proves nothing). Fix 5 partial, with a regression: `.cm-content ::selection` brought back the native selection, which picks up the global `::selection { color: var(--ink) }`, so selected SQL glyphs are ink on the sand tint (~1.9:1, ~3:1 on the active line; chalk at ~6.6:1 before). This is the one allowed extra round; then ship. If the fixer or documenter is lost, rerun that agent. Steps 1–2 below are done; 3 is running; 4–5 remain. Original resume list:
  1. **Verdict pass.** Spawn a FRESH `impeccable:impeccable-finish-reviewer` (opus); the earlier reviewer was stopped mid-pass and its context is gone. Give it the review #2 packet below plus the fix list, and ask it to score fixes 2–6 resolved / partial / unresolved and return a disposition. Packet: the original request; D1–D7 and the rulings; the artifact paths (`apps/studio/src/components/app-shell/`, `styles.css`, `routes/app.tsx`, `crab-marks.tsx`); the surface brief; the captures in `apps/studio/.impeccable/review/` (1440×900 set, 1280×800 `scratch-1280-*`, evidence `fix-api-scrolled.png`, `fix-menu-after-pick.png`, `fix-escape-tab.png`, `fix-sql-selection.png`, `fix-devtools.png`); the fix report (git-ignored: `.superpowers/sdd/2026-10-06-studio-app-shell/finish-fix-report.md`; if missing, use this Status); the craft floor. State the mobile waiver and the fix 1 ruling (both below).
  2. Act on its disposition: one more fix round at most, then ship.
  3. **Documenter.** `impeccable:impeccable-documenter` writes `apps/studio/DESIGN.md` and `apps/studio/.impeccable/design.json`. Scope: the app shell plus the lagoon tokens as Studio's app system; the landing's chart vocabulary stays the landing's. Note `landing-page` also writes `apps/studio/DESIGN.md`, so a merge conflict is expected later.
  4. **End-of-step whole-branch review** (opus, `superpowers:requesting-code-review` against `main`). Give it the deferred-minors lists here. Then ONE fix wave and one scoped re-review.
  5. Final Status, roadmap "Resume here", push, then the user's merge pause. **Ask the user about fix 1** (theme Scalar) as a possible follow-up.
- **Finish review #1** returned `recapture` (it required `mobile.png`). **The user waived mobile captures (2026-10-06):** "Don't worry about the mobile shots I'll be dealing with mobile later". The review set is desktop only (1440, 1280). The reviewer's note for the later mobile pass: the shell has no phone-width behaviour yet (a 56px rail plus a 232px overlay needs a drawer, and the header needs its own menu trigger).
- **Finish review #2** (desktop) returned `fix` with six material fixes. Fidelity matched the contract (ground `#0b1c1f`/`#0f2a2d`, the single sand edge-mark, hairlines, Atkinson + Spectral wordmark), and the ceiling was reached.
  - Fixes 2–6, applied in `baf19aa` and verified live: a sticky header plus Scalar's `--scalar-custom-header-height` offset on the API page; Tab after Escape brings the rail labels back; the behaviour menu closes on pick (`closeOnClick`); the CodeMirror selection is sand at 25%; TanStack Devtools moved to the bottom right.
  - Studio 521 tests, typecheck and build pass, and the detector returns `[]`. 13 captures were retaken.
  - Caveat: in the first 9 recaptures a leftover Playwright init script hid the dev-only Devtools button.
- **Ruling: fix 1 not applied** (theme Scalar's `--scalar-*` palette onto the lagoon tokens). Spec §3, which the user approved, keeps Scalar's own dark palette and puts theming it out of scope. Cost if wrong: `/app/api` keeps a neutral `#0f0f0f` body beside the lagoon chrome until a follow-up. Raise it at the merge pause.
- **T3:** `8ac5830` + fix round 1 `715f56e` (home link never `aria-current`, keyboard-only focus opens the hover rail, 34% hairline, 36px rows, stale hover state, test noise), re-review clean, merged `5c98f2a`.
- **T5:** `9ccb521`, review clean (Studio 517 tests, typecheck, build; `StudioShell`/`ApiNavLink` gone).
- **T6 (live, 2026-10-06):** `2be8c14`. All pages and rail modes pass at 1440 and 1280, and a full keyboard path works through a real Sign out. Detector `[]`. Studio 518 tests, typecheck and build pass.
  - Fixes: `--input` raised to chalk 40% (34% measured 2.79:1, under the 3:1 floor); the SQL editor marked dark (its caret and selection were 1.2:1 on ink); menu keyboard highlight gets a 2px sand outline (the wash alone was 1.25:1); header focus ring sand, no rounding or shadow.
  - The SQL editor has no syntax highlighting (every token chalk, 14.69:1), so no colour remap was needed.
  - Live-run notes: Docker Desktop was down and was started from WSL. The shared `chelabase_metadata` already carries the schema-merge branch's migration `20261010000003`, which crashes this branch's orchestrator, so the run used `CHELABASE_METADATA_DATABASE_URL=postgres://chelabase:chelabase@127.0.0.1:5433/chelabase_metadata_shell` (an env var only; no shared config changed). A later live run from this branch needs the same until it carries that migration.
  - Test data left: user `shell-t6-20261006@example.com`, database `t6shell`, metadata database `chelabase_metadata_shell`.
  - 11 observations go to the finish reviewer (task-6 report). Among them: the behaviour menu stays open after a pick; after Escape in hover mode the rail stays closed while tabbing on; Scalar's sidebar is clipped under the header at the page top; the dev-only TanStack Devtools button covers the rail's Sidebar behaviour button.
  - `apps/studio/.impeccable/review/` is git-ignored as on `landing-page`.
- **Done:** spec `247badc`; plan approved 2026-10-06 (`467d31e`); T0 `7786c4d`; T2 `769d17b` (review clean, merged `d85e75a`); T4 `8ab881a` (review clean, merged `5dffe3c`); T1 `bcf419f` (no own review; merged `b89ec50`; full Studio suite 478/478, typecheck, build green in its worktree).
- **Rulings:** wave A and T3 ran in parallel worktrees (CLAUDE.md standing rule over the SDD skill's serial default; cost: a merge conflict). T1's extras `::selection` (sand on ink) and `scrollbar-color` kept (in-world; cost: two lines to drop).
- **Deferred minors:** T2: hook test doesn't assert console silence on a failed write; default `getStorage` touches `window` (SPA only). T4: email in the profile popup is a bare div (SR menu mode may skip it); the Account test doesn't open the menu to prove Sign out with an empty profile; `holder` indirection in the ordering test.
- **Pre-flight (2026-10-06):** every path, helper and line the plan names exists (`renderWithQuery` in `src/test/render.tsx`, `just live`/`live-status`/`studio`/`wt-add`/`wt-remove`, both PRODUCT.md lines, Scalar's `forceDarkModeState`/`hideDarkModeToggle` in `@scalar/types` 0.22.2, Base UI 1.8 `menu` (radio-group, radio-item, radio-item-indicator) and `tooltip`, lucide `Boxes`, `BookOpenText`, `CircleDashed`, `PanelLeft`, `CircleUser`, `LogOut`, `Check`). The only `StudioShell`/`ApiNavLink` importers are `dashboard.tsx`, `project-page.tsx`, `app.index.tsx`, `app.projects.$projectId.tsx` (all in T5). `api-page.test.tsx` mocks `Link` and asserts no header text, so it survives T5 unchanged. Fonts 5.3.0 were published 2026-07-19 (past pnpm's release-age window). Known merge cost: `landing-page` adds the same three font deps and the same `crab-marks.tsx` (verbatim copy, so only `package.json`/lockfile lines may conflict); its `PRODUCT.md` brand lines match T1's.
- **More rulings:** in hover mode only keyboard focus expands the rail (spec §2 "Keyboard focus"; cost: programmatic focus shows no labels until a key press). The wordmark home link never carries `aria-current` (cost: none of note).
- **More deferred minors:** T3: unused `_ariaCurrent`/`_dataStatus` in HomeLink; older Studio test files print jsdom `scrollTo` noise (stub in `src/test/setup.ts`). T5: no test that only `expanded` gives the 232px column; unused `Link` mock in `api-page.test.tsx`; an indentation leftover in `project-page.test.tsx`; no test pinning the default `data-behaviour="hover"`.
- **Even more deferred minors:** the finish fixes: the CodeMirror selection colour has no unit assertion (jsdom can't resolve scoped styles; verified live). An old Escape test assertion was changed because fix 3 reverses that behaviour.
- **Commit trailer:** from 2026-10-06 midday, commits end with `Claude-Session: https://claude.ai/code/session_018wRTvbnVpoppWa9kah2vKN`. The session ID in Global Constraints is the earlier local one.
- **Leftover branches:** `studio-app-shell-t1`…`-t4` are merged and their worktrees removed; delete them with `git branch -d` at the end.
- **Running:** nothing. No live hold of ours; the stack belongs to the schema-merge sessions. A live run from this branch needs `CHELABASE_METADATA_DATABASE_URL=…/chelabase_metadata_shell` (see T6).

## Global Constraints

- Work only in `<repo root>/.worktrees/studio-app-shell` (parallel tasks: their own `-tN` worktree). Absolute paths; `git -C <wt>`; one simple command per Bash call; never `EnterWorktree`, never the harness's `isolation: "worktree"`.
- Commit with `git commit`, message ending with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>` and `Claude-Session: 4448fb57-da59-4f79-bd5f-26f509a654ca`. Push the step branch after every merge or checkpoint.
- Strict TDD: show the failing run (RED) before code, then GREEN. A compile error counts as RED only when the missing item is the code under test.
- Studio commands (from the worktree root): `pnpm --filter @chelabase/studio test`, `pnpm --filter @chelabase/studio typecheck`, `pnpm --filter @chelabase/studio build`. Single file: `pnpm --filter @chelabase/studio exec vitest run <path>`.
- Never mirror Supabase's look, menu tree or wording. English only. WCAG 2.2 AA.
- Secrets never enter the query cache (unchanged here; Sign out must still `queryClient.clear()` **before** `onSignOut()`).
- No `cargo fmt`; no backend changes; no stack runs except through `just live` (T6 only).
- Storage key `chelabase.studio.rail`; values `expanded | collapsed | hover`; default `hover`. Widths 56px / 232px. Hover intent 150ms, close grace 200ms. Width transition 200ms `cubic-bezier(0.16, 1, 0.3, 1)`, instant under `prefers-reduced-motion`.
- Rail labels: `Projects`, `API reference`, `Placeholder` (twice). Behaviour menu trigger name `Sidebar behaviour`; options `Expanded`, `Collapsed`, `Expand on hover`. Skip link `Skip to content`. Nav landmark `aria-label="Studio"`. Sign-out item `Sign out`.
- No new dependencies beyond `@fontsource-variable/atkinson-hyperlegible-next@^5.3.0`, `@fontsource-variable/jetbrains-mono@^5.3.0`, `@fontsource/spectral@^5.3.0` (latest on npm 2026-10-06; same pins as `landing-page`).

## Review Focus

1. **Storage unavailable** (private mode, blocked site data: `window.localStorage` itself throws on access). The shell renders in `hover` mode and picking a behaviour still works for the session. Test in T2.
2. **Projects must not light up on `/app/api`** (a prefix match on `/app` would). Only one item is `aria-current="page"` at a time. Test in T3.
3. **Hover mode with the behaviour menu open:** moving the pointer from the rail into the menu's portal popup must not collapse the rail and unmount the menu mid-pick. Test in T3.
4. **Profile with neither name nor email** (an IdP that returns neither). The header trigger still has an accessible name ("Account") and Sign out works. Test in T4.
5. **Sign out ordering:** a failing or slow `onSignOut` must never run before the cache is cleared. Test in T4 (call order asserted).

---

## File map

| File | Task | Responsibility |
|---|---|---|
| `apps/studio/package.json` | T1 | three `@fontsource` deps |
| `apps/studio/src/styles.css` | T1 | lagoon tokens, fonts, Tailwind theme names, shell CSS hooks |
| `PRODUCT.md` | T1 | three brand lines + dark-mode Accessibility line |
| `apps/studio/src/components/landing/crab-marks.tsx` | T1 | verbatim copy from `landing-page` |
| `apps/studio/src/components/api-reference.tsx` (+ test) | T1 | force Scalar dark mode |
| `apps/studio/src/components/sql-editor/code-editor.tsx` | T1 | mono font → `var(--font-mono)` |
| `apps/studio/src/components/user-drawer.tsx` | T1 | `text-green-600` → `text-seafoam` |
| `apps/studio/src/lib/rail-preference.ts` (+ test) | T2 | persisted behaviour + hook |
| `apps/studio/src/components/app-shell/app-header.tsx` (+ test) | T4 | header, profile menu, Sign out |
| `apps/studio/src/components/app-shell/rail-items.ts` (+ test) | T3 | item config + `isRailItemActive` |
| `apps/studio/src/components/app-shell/rail-item.tsx` | T3 | one item / placeholder |
| `apps/studio/src/components/app-shell/rail-behaviour-menu.tsx` (+ test) | T3 | behaviour menu |
| `apps/studio/src/components/app-shell/app-rail.tsx` (+ test) | T3 | rail, hover logic, tooltips |
| `apps/studio/src/components/app-shell/app-shell.tsx` (+ test) | T5 | frame, skip link, grid |
| `apps/studio/src/routes/app.tsx`, `app.index.tsx`, `app.projects.$projectId.tsx` | T5 | wiring |
| `apps/studio/src/components/dashboard.tsx`, `project-page.tsx`, `api-page.tsx` (+ tests) | T5 | drop wrapper/props |
| `apps/studio/src/components/studio-shell.tsx`, `api-nav-link.tsx` (+ test) | T5 | deleted |
| `apps/studio/.impeccable/surfaces/<shell>.md` | T0 | direction contract |

## Waves and models

| Wave | Task | Implementer | Reviewer | Branch |
|---|---|---|---|---|
| 0 | T0 direction contract | orchestrator (opus, this session) | none (the finish reviewer audits it) | step branch |
| A | T1 theme foundation | sonnet | none of its own (T6 browser pass + finish review + end review) | `studio-app-shell-t1` |
| A | T2 rail preference | sonnet | sonnet (T3/T5 build on it) | `studio-app-shell-t2` |
| A | T4 app header | sonnet | sonnet (moves the cache-clear-before-sign-out guarantee) | `studio-app-shell-t4` |
| B | T3 rail | opus (hover/focus/portal timing) | opus | `studio-app-shell-t3` (from step branch after T2 merges) |
| C | T5 shell + wiring | sonnet | sonnet | step branch |
| D | T6 browser pass, AA, detector | opus (live) | Impeccable finish reviewer (opus) | step branch |
| D | T7 finish fixes + documenter | sonnet (fixes), `impeccable-documenter` | end-of-step whole-branch review (opus) | step branch |

Wave A tasks touch disjoint files and run in parallel worktrees (`just wt-add studio-app-shell-tN studio-app-shell`), each reviewed (where listed) there, merged `--no-ff` into `studio-app-shell`, then removed (`just wt-remove studio-app-shell-tN`). T3 starts when T2 is merged (it imports `rail-preference`); it may start alongside T1/T4 since it touches none of their files. T5 waits for T1, T3, T4.

---

### Task 0: Direction contract (orchestrator)

**Files:** `apps/studio/.impeccable/surfaces/` brief for `apps/studio/src/components/app-shell/app-shell.tsx` (written with `impeccable surface-brief write`; related targets `app-rail.tsx`, `app-header.tsx`, `routes/app.tsx`).

- [ ] Read the existing brief (`impeccable surface-brief read apps/studio/src/components/app-shell/app-shell.tsx`; expected: none).
- [ ] Write the brief: scope and mode (Operate, signed-in shell, skeleton), job/audience, and the `## Direction contract` blocks THESIS, OWN-WORLD, STORY, FIRST VIEWPORT, FORM (inherited lagoon world; no seed: the user pinned topology and world, so no concept roll ran; say so), FINISH (verbatim line from new-work.md §5). Content comes from spec §3.
- [ ] Read it back; confirm all six blocks. Commit `studio app shell: T0 direction contract`. Push.

### Task 1: Theme foundation

**Files:** Modify `apps/studio/package.json`, `apps/studio/src/styles.css`, `PRODUCT.md`, `apps/studio/src/components/api-reference.tsx`, `apps/studio/src/components/api-reference.test.tsx`, `apps/studio/src/components/sql-editor/code-editor.tsx`, `apps/studio/src/components/user-drawer.tsx`. Create `apps/studio/src/components/landing/crab-marks.tsx`.

**Interfaces — Produces (used by T3/T4/T5 as class names and CSS variables):**
- CSS variables on `:root`: `--ink #0b1c1f`, `--deep #0f2a2d`, `--channel #14363a`, `--shallows #163d3e`, `--sand #f2c7a3`, `--sand-lit #f7d6b9`, `--seafoam #9fd4c6`, `--coral #eda6aa`, `--palm #a9c89a`, `--chalk #e6ede7`, `--chalk-dim #aebfc0`, `--line rgb(230 237 231 / 0.16)`, `--line-strong rgb(230 237 231 / 0.34)`, plus the claw's `--shell #f2936e`, `--shell-dark #b85a3e`, `--shell-light #f8b896`, `--tip #fde3cf` (values copied from the landing branch's `landing.css`).
- Semantic tokens remapped per the spec's table: `--background: var(--ink)`, `--foreground: var(--chalk)`, `--primary: var(--sand)`, `--primary-foreground: var(--ink)`, `--secondary: var(--channel)`, `--secondary-foreground: var(--chalk)`, `--muted: var(--deep)`, `--muted-foreground: var(--chalk-dim)`, `--destructive: var(--coral)`, `--border: var(--line)`, `--input: var(--line-strong)`, `--ring: var(--sand)`. `:root { color-scheme: dark; }`.
- Tailwind names in `@theme inline`: `--color-ink|deep|channel|sand|sand-lit|seafoam|coral|palm|chalk|chalk-dim|line|line-strong` → `var(--…)`; `--font-sans: "Atkinson Hyperlegible Next Variable", "Atkinson Hyperlegible", system-ui, sans-serif`; `--font-serif: Spectral, "Iowan Old Style", Georgia, serif`; `--font-mono: "JetBrains Mono Variable", ui-monospace, monospace`; `--ease-lagoon: cubic-bezier(0.16, 1, 0.3, 1)`.
- Exports from `crab-marks.tsx`: `ClawPaths`, `ClawMark({ size?, className? })`, `Wordmark({ as?, className? })`, `CrabFigure`, `PalmTree` (verbatim).

**Rules:**
- `crab-marks.tsx`: byte-identical copy of `<repo root>/.worktrees/landing-page/apps/studio/src/components/landing/crab-marks.tsx` (`cp`, then `cmp` to prove it). Don't copy `landing.css` or anything else from that branch.
- Fonts: `@import "@fontsource-variable/atkinson-hyperlegible-next";`, `@import "@fontsource/spectral/700.css";`, `@import "@fontsource-variable/jetbrains-mono";` at the top of `styles.css`, after `@import "tailwindcss";`. Install with `pnpm --filter @chelabase/studio add <pkgs>` (the lockfile's supply-chain check must pass).
- Wordmark CSS for the app lives in `styles.css`, scoped under `.app-rail` so it can't collide with the landing's unscoped `.wordmark__*` rules: `.app-rail .wordmark { font-family: var(--font-serif); font-weight: 700; letter-spacing: -0.02em; line-height: 1; color: var(--chalk); }`, `.app-rail .wordmark__visual { display: inline-flex; align-items: flex-end; }`, `.app-rail .wordmark__claw { width: auto; height: 1.32em; margin-right: -0.14em; }`, and `.app-rail { --claw-gap: var(--deep); }`.
- Motion hook: `@media (prefers-reduced-motion: reduce) { .app-rail, .app-rail * { transition-duration: 0s !important; } }`.
- `code-editor.tsx`: `.cm-scroller` fontFamily → `"var(--font-mono)"`; `styles.css` `.cm-pgls-hover` font-family → `var(--font-mono)`. Nothing else in the editor changes (syntax colours are T6's check).
- `user-drawer.tsx:139`: `text-green-600` → `text-seafoam`. Dialog backdrops (`bg-black/40`) stay.
- `api-reference.tsx`: `apiReferenceConfiguration` adds `forceDarkModeState: "dark"` and `hideDarkModeToggle: true`.
- `PRODUCT.md` (repo root): under Brand Commitments, replace `- No logo, favicon or brand marks exist yet.` with the landing branch's three lines verbatim (from `<repo root>/.worktrees/landing-page/PRODUCT.md`: the provisional Spectral wordmark line, the dark default line, the visual world line). Under Accessibility, replace `- Dark mode is a product requirement (not implemented yet; Studio is light-only today).` with `- Dark is the default theme across Studio (since the app shell, 2026-10-06); light mode may come later.` Nothing else changes.

**Tests:**
- `api-reference.test.tsx`: add `it("forces Scalar's dark mode and hides its toggle")` asserting `apiReferenceConfiguration("http://gw", "t").forceDarkModeState === "dark"` and `.hideDarkModeToggle === true`. RED first.

- [ ] Add the failing test; run it; see it fail.
- [ ] Make the config change; GREEN.
- [ ] Copy `crab-marks.tsx`; `cmp` both files (no output).
- [ ] Add fonts; rewrite tokens and theme names; add shell CSS hooks; editor font; user-drawer colour.
- [ ] Update PRODUCT.md.
- [ ] Run Studio `test`, `typecheck`, `build`: all green. Commit `studio app shell: T1 lagoon tokens, fonts, PRODUCT.md`.

### Task 2: Rail preference

**Files:** Create `apps/studio/src/lib/rail-preference.ts`, `apps/studio/src/lib/rail-preference.test.ts`.

**Interfaces — Produces:**
```ts
export type RailBehaviour = "expanded" | "collapsed" | "hover";
export const RAIL_BEHAVIOURS: readonly RailBehaviour[]; // ["expanded", "collapsed", "hover"]
export const RAIL_STORAGE_KEY = "chelabase.studio.rail";
export const DEFAULT_RAIL_BEHAVIOUR: RailBehaviour; // "hover"
/** `getStorage` defaults to `() => window.localStorage`; it is called inside try because the getter itself throws when storage is blocked. */
export function readRailBehaviour(getStorage?: () => Storage): RailBehaviour;
export function writeRailBehaviour(value: RailBehaviour, getStorage?: () => Storage): void;
/** State initialised synchronously from readRailBehaviour(); the setter updates state, then writes (a failing write is swallowed). */
export function useRailBehaviour(): [RailBehaviour, (next: RailBehaviour) => void];
```

**Tests (`rail-preference.test.ts`):**
- `returns hover when nothing is stored`
- `returns a stored valid value` (each of the three)
- `returns hover for an invalid stored value` (`"wide"`)
- `returns hover when the storage getter throws` (`() => { throw new DOMException("denied", "SecurityError") }`) — Review Focus 1
- `returns hover when getItem throws`
- `write persists the value under chelabase.studio.rail`
- `write swallows a throwing setItem and a throwing getter`
- `useRailBehaviour updates state even when storage is unavailable` (renderHook with `window.localStorage` getter stubbed to throw via `vi.spyOn(window, "localStorage", "get")`; set `collapsed`; state is `collapsed`) — Review Focus 1
- Clear `localStorage` in `afterEach`.

- [ ] Write the tests; RED (module missing counts: it is the code under test).
- [ ] Implement; GREEN; Studio `typecheck`. Commit `studio app shell: T2 rail preference`.

### Task 3: Rail (items, behaviour menu, hover logic)

**Files:** Create `apps/studio/src/components/app-shell/rail-items.ts`, `rail-items.test.ts`, `rail-item.tsx`, `rail-behaviour-menu.tsx`, `rail-behaviour-menu.test.tsx`, `app-rail.tsx`, `app-rail.test.tsx`.

**Interfaces — Consumes:** T2's `RailBehaviour`, `RAIL_BEHAVIOURS`. T1's classes (`bg-deep`, `text-chalk-dim`, `bg-sand`, `border-line-strong`, `font-serif`, `ease-lagoon`) and `Wordmark`/`ClawMark` from `@/components/landing/crab-marks` (T1 may land after T3 starts: if the file is missing in T3's worktree, merge the step branch first once T1 is in; don't copy it again).

**Produces:**
```ts
// rail-items.ts
import type { LucideIcon } from "lucide-react";
export type RailLink = { kind: "link"; id: "projects" | "api"; label: string; to: "/app" | "/app/api"; icon: LucideIcon };
export type RailPlaceholder = { kind: "placeholder"; id: string; label: "Placeholder"; icon: LucideIcon };
export type RailEntry = RailLink | RailPlaceholder;
export const RAIL_ENTRIES: readonly RailEntry[];
// [Projects /app Boxes, API reference /app/api BookOpenText, Placeholder CircleDashed ×2 (ids "placeholder-1", "placeholder-2")]
export function isRailItemActive(id: RailLink["id"], pathname: string): boolean;
// projects: pathname is "/app" or "/app/", or starts with "/app/projects/"; api: pathname is "/app/api" or starts with "/app/api/"

// app-rail.tsx
export const HOVER_OPEN_DELAY_MS = 150;
export const HOVER_CLOSE_DELAY_MS = 200;
export interface AppRailProps { behaviour: RailBehaviour; onBehaviourChange: (next: RailBehaviour) => void }
export function AppRail(props: AppRailProps): JSX.Element;

// rail-behaviour-menu.tsx
export interface RailBehaviourMenuProps {
  behaviour: RailBehaviour;
  onBehaviourChange: (next: RailBehaviour) => void;
  onOpenChange?: (open: boolean) => void; // the rail stays expanded while open
  labelsVisible: boolean;                 // icon-only trigger when false (name stays "Sidebar behaviour")
}
export function RailBehaviourMenu(props: RailBehaviourMenuProps): JSX.Element;
```

**Behaviour rules:**
- `AppRail` renders `<nav aria-label="Studio" className="app-rail …" data-behaviour={behaviour} data-expanded={expanded ? "true" : "false"}>`. `expanded` = `behaviour === "expanded"` or (`behaviour === "hover"` and (pointer-open or focus-within or menu-open)).
- Hover mode: `pointerenter` starts a 150ms timer to open; `pointerleave` cancels it and starts a 200ms close timer; re-entry cancels the close. `focusin` opens immediately; `focusout` to an element outside the nav (check `relatedTarget`) closes unless the pointer is inside or the menu is open. `Escape` keydown inside the nav closes it (sets a "dismissed" flag until the next pointerenter/focusin from outside) and doesn't move focus. While the behaviour menu is open, the rail stays expanded whatever the pointer does (Review Focus 3): the menu popup is portalled, so `pointerleave` fires when the pointer enters it.
- Clear all timers on unmount and on behaviour change.
- In hover mode the rail's outer element is 56px wide in the grid; its inner panel is `position: absolute` (top 0, left 0, height 100%) and widens to 232px over the content (`z-index` above main). In expanded mode the panel is static at 232px; collapsed static at 56px. Width transition 200ms `ease-lagoon`.
- Labels: always in the DOM. When not expanded, the label span is `sr-only`, and in **collapsed** mode only each link is wrapped in a Base UI `Tooltip` (Root/Trigger/Portal/Positioner side "right"/Popup) showing the label; one `Tooltip.Provider` around the list. No tooltips in expanded or hover mode.
- Links: TanStack `<Link to={entry.to}>`; set `aria-current={isRailItemActive(entry.id, pathname) ? "page" : undefined}` explicitly (pathname from `useRouterState({ select: (s) => s.location.pathname })`) and pass `activeOptions={{ exact: true }}` plus `activeProps={{}}` so TanStack's own matching never contradicts it (Review Focus 2). Active styling: chalk text and a 2px sand bar on the rail's left edge (`before:` pseudo-element, `bg-sand`). Inactive `text-chalk-dim`; hover `text-chalk` on `bg-chalk/[0.06]`. Focus `outline-2 outline-sand outline-offset-2`.
- Placeholders: a `<div aria-disabled="true">` (no tabindex, no role link), dashed `border-line-strong` outline, `text-chalk-dim`, same icon + label layout.
- Top: `Wordmark` when expanded, `ClawMark size={28}` when not, inside a `Link to="/app"` named "Chelabase Studio home" (`aria-label`). Bottom: `RailBehaviourMenu`.
- Icons: 18px, `strokeWidth={1.5}`, `aria-hidden`.
- `RailBehaviourMenu`: Base UI `Menu.Root` (`onOpenChange` forwarded) / `Menu.Trigger` (icon `PanelLeft`; `aria-label="Sidebar behaviour"`; visible text "Sidebar" only when `labelsVisible`) / `Menu.Portal` / `Menu.Positioner` (side "right", align "end") / `Menu.Popup` / `Menu.RadioGroup value={behaviour} onValueChange={(v) => onBehaviourChange(v as RailBehaviour)}` with three `Menu.RadioItem`s (`Expanded`, `Collapsed`, `Expand on hover`) each with a `Menu.RadioItemIndicator` (`Check` icon). Popup styled `bg-deep border border-line-strong text-chalk`, items highlight on `data-highlighted` with `bg-chalk/[0.08]`.

**Tests:** Render inside a memory router (extend the pattern in `src/test/render-router.tsx`; a local helper in the test file rendering `<AppRail>` at a given path is fine).
- `rail-items.test.ts`: `projects is active on /app, /app/ and /app/projects/p1`; `projects is not active on /app/api` (Review Focus 2); `api is active on /app/api only`.
- `app-rail.test.tsx`:
  - `expanded: labels are visible and no tooltip appears on focus`
  - `collapsed: each link keeps its accessible name` (`getByRole("link", { name: "Projects" })`, `"API reference"`)
  - `collapsed: focusing a link shows its label in a tooltip` (`findByRole("tooltip")` or the popup text; real timers)
  - `hover: expands after the intent delay and collapses after the grace period` (fake timers; `data-expanded` false → still false at 100ms → true at 150ms; leave → still true at 150ms → false at 200ms)
  - `hover: a quick pass under the intent delay never expands`
  - `hover: focus inside expands it; Escape collapses it and keeps focus` (`document.activeElement` unchanged)
  - `hover: stays expanded while the behaviour menu is open, even after pointerleave` (Review Focus 3)
  - `marks only the current item with aria-current="page"` at `/app/api` (Projects has no `aria-current`) and at `/app/projects/p1`
  - `placeholders are aria-disabled and not focusable` (`userEvent.tab()` through the rail never lands on them)
- `rail-behaviour-menu.test.tsx`:
  - `trigger is named Sidebar behaviour`
  - `shows the three behaviours with the current one checked` (`getByRole("menuitemradio", { name: "Expand on hover" })` has `aria-checked="true"`)
  - `picking one calls onBehaviourChange` with `"collapsed"`
  - `reports open and close through onOpenChange`

**Gotchas:** Base UI popups render in a portal; query with `screen`. jsdom has no layout: assert data attributes and classes, not widths. Fake timers with `userEvent`: `userEvent.setup({ advanceTimers: vi.advanceTimersByTime })`. Tooltip open delay: pass `delay={300}` on the provider in code; tests for collapsed use real timers and `findBy…`.

- [ ] Write `rail-items.test.ts`; RED; implement `rail-items.ts`; GREEN.
- [ ] Write `rail-behaviour-menu.test.tsx`; RED; implement; GREEN.
- [ ] Write `app-rail.test.tsx`; RED; implement `rail-item.tsx` and `app-rail.tsx`; GREEN.
- [ ] Studio `test` and `typecheck` green. Commit `studio app shell: T3 rail, items, behaviour menu`.

### Task 4: App header

**Files:** Create `apps/studio/src/components/app-shell/app-header.tsx`, `app-header.test.tsx`.

**Interfaces — Produces:**
```ts
export interface AppHeaderProps { profile: { name?: string; email?: string }; onSignOut: () => void }
export function AppHeader(props: AppHeaderProps): JSX.Element;
```

**Rules:**
- `<header>` 52px tall, `bg-background`, bottom `border-b border-line`. Left: an empty `<div data-slot="context" />` (no text). Right: Base UI `Menu` profile menu.
- Trigger: `CircleUser` icon + visible `profile.name ?? profile.email`; when both are missing, no visible text and `aria-label="Account"` (Review Focus 4). When text is visible the trigger's name is that text.
- Popup: the email (when present) as non-interactive text (`Menu.GroupLabel` or a plain div), then `Menu.Item` "Sign out" with `LogOut` icon. On click: `queryClient.clear()` then `onSignOut()` (Review Focus 5). Keep the existing comment: "Nothing of this session may outlive it in the cache."
- Popup styling as in T3 (`bg-deep border border-line-strong text-chalk`).

**Tests (`app-header.test.tsx`, using `renderWithQuery` from `@/test/render`):**
- `shows the name in the trigger`
- `falls back to the email when there is no name`
- `names the trigger Account when there is neither` (Review Focus 4)
- `shows the email in the menu`
- `Sign out clears the query cache before calling onSignOut` (seed a query, spy `queryClient.clear`, `onSignOut` mock records `queryClient.getQueryCache().getAll().length === 0` at call time; both called once) — Review Focus 5

- [ ] Tests; RED; implement; GREEN; Studio `typecheck`. Commit `studio app shell: T4 app header`.

### Task 5: Shell frame and wiring

**Files:** Create `apps/studio/src/components/app-shell/app-shell.tsx`, `app-shell.test.tsx`. Modify `apps/studio/src/routes/app.tsx`, `app.index.tsx`, `app.projects.$projectId.tsx`, `apps/studio/src/components/dashboard.tsx`, `dashboard.test.tsx`, `project-page.tsx`, `project-page.test.tsx`, `api-page.tsx` (and `api-page.test.tsx` if it asserts the old header). Delete `studio-shell.tsx`, `api-nav-link.tsx`, `api-nav-link.test.tsx`.

**Interfaces — Consumes:** `AppRail`, `AppHeader`, `useRailBehaviour`.
**Produces:**
```ts
export interface AppShellProps { profile: { name?: string; email?: string }; onSignOut: () => void; children: ReactNode }
export function AppShell(props: AppShellProps): JSX.Element;
```

**Rules:**
- Layout: `min-h-screen grid` with `grid-template-columns: <railWidth> 1fr` where railWidth is 232px for `expanded`, 56px otherwise; the column change transitions 200ms `ease-lagoon` (instant under reduced motion). The rail column is `sticky top-0 h-screen`. Column 2: `AppHeader` then `<main id="content" tabIndex={-1} className="min-w-0 outline-none">{children}</main>`.
- First focusable element: `<a href="#content" className="sr-only focus:not-sr-only …">Skip to content</a>` (visible on focus with a sand outline).
- `routes/app.tsx`: authenticated branch renders `<AppShell profile={session.profile} onSignOut={session.onSignOut}><Outlet /></AppShell>` inside the existing providers. Update the `onSignOut` comment ("The header clears the query cache before this runs"). Redirect/error branches unchanged.
- `Dashboard`: remove `StudioShell`, the `profile`, `onSignOut`, `nav` props and their doc comments; root becomes `<div className="mx-auto max-w-4xl space-y-8 p-8">…</div>` around the same children. `DashboardProps` keeps `onLoaded`.
- `ProjectPage`: same removal (`profile`, `onSignOut`, `nav`); same root container. Other props unchanged.
- `ApiPage`: drop its `<header>`; keep `Suspense` and the lazy import; the outer `min-h-screen bg-background text-foreground` wrapper goes (the shell provides it).
- Routes: `app.index.tsx` passes only `onLoaded`; `app.projects.$projectId.tsx` stops passing `profile`, `onSignOut`, `nav`; both drop the `ApiNavLink` import.
- Delete `studio-shell.tsx`, `api-nav-link.tsx`, `api-nav-link.test.tsx`; `grep -rn "StudioShell\|ApiNavLink" apps/studio/src` returns nothing.

**Tests:**
- `app-shell.test.tsx` (memory router: a `/app` layout route rendering `<AppShell profile=… onSignOut=…><Outlet/></AppShell>`, children `/app/` → "dashboard body", `/app/api` → "api body"):
  - `the skip link is first and targets #content` (first `userEvent.tab()` focuses it; `href="#content"`; `#content` is the `main`)
  - `renders nav, header and main landmarks` (`getByRole("navigation", { name: "Studio" })`, `banner`, `main`)
  - `renders the child route inside main`
  - `keeps the same rail element across a child route change` (capture `getByRole("navigation")`, `router.navigate({ to: "/app/api" })`, assert the same node with `toBe`)
  - `starts in the stored behaviour` (pre-set `localStorage["chelabase.studio.rail"] = "collapsed"`; `data-behaviour="collapsed"`)
- `dashboard.test.tsx`: drop `profile`/`onSignOut` from the harness; delete the "Sign out" test (moved to T4). All other tests unchanged and green.
- `project-page.test.tsx`: drop `profile`/`onSignOut` from the harness.

- [ ] Write `app-shell.test.tsx`; RED; implement `app-shell.tsx`; GREEN.
- [ ] Update the page tests first (RED on the removed props via typecheck is acceptable here: the props are the code under change), then the pages, routes and deletions.
- [ ] Studio `test`, `typecheck`, `build` green; the grep is empty. Commit `studio app shell: T5 shell frame, wiring, StudioShell removed`.

### Task 6: Browser pass, AA, detector

**Files:** Possibly `apps/studio/src/components/sql-editor/code-editor.tsx` (syntax highlight), shell files for defects found. Screenshots under `apps/studio/.impeccable/review/` (git-ignored scratch).

- [ ] `just live-status`; then `just live just dev` from this worktree (moves the dev stack here, as step 6f did), `just studio` from this worktree (:3000), and drive the Playwright MCP (`mcp__playwright__*`, headless chromium; setup in the roadmap's Playwright MCP note; pnpm, never npx), holding the stack ≤ 15 min per chunk, at 1440×900 and 1280×800: `/app`, a project page, `/app/api`, `/login`. For each rail mode: expanded, collapsed, hover-open (pointer over the rail). Capture `desktop.png` (1440, full page, `/app` in hover-collapsed), plus `rail-expanded.png`, `rail-collapsed.png`, `rail-hover.png`, `project.png`, `api.png`, `login.png`.
- [ ] Keyboard-only pass: Tab from the top → skip link → rail items (placeholders skipped) → behaviour menu (open, arrow, Enter) → header menu → Sign out reachable. Record results.
- [ ] Contrast spot-check (computed colours): chalk/ink, chalk-dim/ink, chalk-dim/deep, sand/ink, ink/sand (primary button), coral/ink, `--input` border vs ink ≥ 3:1. In the SQL editor, check each syntax token colour on `--background`; if any is under 4.5:1, add a `HighlightStyle` mapping keyword → `var(--coral)`, string → `var(--sand)`, function/variable-name call → `var(--seafoam)`, type → `var(--palm)`, comment → `var(--chalk-dim)`, and re-check.
- [ ] `impeccable detect --json` over `apps/studio/src/components/app-shell/ apps/studio/src/styles.css`; fix the mechanical findings.
- [ ] Studio `test`, `typecheck`, `build` green after any fix. Commit `studio app shell: T6 browser pass fixes` (if any). Update Status with the results.

### Task 7: Finish review, fixes, documentation, end review

- [ ] Spawn `impeccable:impeccable-finish-reviewer` (fresh, no history) with: the original request, the confirmed answers (D1–D7), the artifact paths (app-shell dir, `styles.css`, `routes/app.tsx`), the screenshot paths, the direction contract (surface brief path), detector findings, `craft-floor.md` path, and a note that this is a code-led build with no comp.
- [ ] Act on its disposition (recapture / fix / rebuild / ship). A fix batch goes to a sonnet implementer; recapture the same viewports; verdict pass by the same reviewer.
- [ ] Spawn `impeccable:impeccable-documenter` with the project root `apps/studio`, the artifact paths, the direction contract, PRODUCT.md, the `document.md` path; write boundary `apps/studio/DESIGN.md` and `apps/studio/.impeccable/design.json`. Scope: the app shell and the lagoon tokens as Studio's app system (the landing's chart vocabulary stays the landing's). Verify both files carry tokens.
- [ ] End-of-step whole-branch review (opus, `superpowers:requesting-code-review` against `main`). Critical/Important fixed; minors batched with the deferred list.
- [ ] Final Status, roadmap "Resume here", push. Pause for the user's merge decision.

### Task 8: Scalar in the lagoon palette, Axios default, behaviour tooltip (added 2026-10-06, spec D8)

**Files:** `apps/studio/src/components/api-reference.tsx` (+ test), `apps/studio/src/components/app-shell/rail-behaviour-menu.tsx` (+ test), `apps/studio/DESIGN.md`, `apps/studio/.impeccable/design.json`.

- Configuration: `defaultHttpClient: { targetKey: "js", clientKey: "axios" }`. Test: the built configuration carries it.
- Theme: override Scalar's dark-mode `--scalar-*` colour variables, scoped to the API page wrapper and never global, so no other page changes. Map them to lagoon tokens:
  - backgrounds 1/2/3 → ink / deep / channel; colours 1/2/3 → chalk / chalk-dim / chalk-dim; the accent → sand; borders → the hairline tokens; button-1 → sand with ink text;
  - fonts → `var(--font-sans)` / `var(--font-mono)`;
  - method colours: red → coral, green → seafoam, yellow/orange → sand, purple → palm, blue → the nearest in-world tone, ruled by the implementer and listed.
  The overrides must beat Scalar's own `.dark-mode` rules on specificity (verify live with computed values). Test: the override CSS is present and scoped to the wrapper.
- Tooltip: the "Sidebar behaviour" trigger gets the same `Tooltip` the rail links use, shown when its label is hidden (collapsed, and hover mode while closed), never while the menu is open. Test it RED first.
- AA: Scalar text and method labels at least 4.5:1 on their grounds, borders of controls at least 3:1. Compute from tokens and confirm live.
- Live (`just live`, with the `chelabase_metadata_shell` env as in T6): captures at 1440 of `/app/api` at the top, scrolled, and with an operation's Axios code sample showing, plus the collapsed-rail tooltip. Save them to `apps/studio/.impeccable/review/t8-*.png`.
- Update DESIGN.md and design.json: Scalar now in the lagoon palette (remove that open gap).
- Gates: test, typecheck, build. Commit `studio app shell: T8 Scalar in lagoon palette, Axios default, behaviour tooltip`.

## Verification (end of step)

- `pnpm --filter @chelabase/studio test`, `typecheck`, `build`: green.
- `grep -rn "StudioShell\|ApiNavLink" apps/studio/src`: empty.
- `cmp apps/studio/src/components/landing/crab-marks.tsx <repo root>/.worktrees/landing-page/apps/studio/src/components/landing/crab-marks.tsx`: identical.
- T6's browser and keyboard pass recorded in Status; finish reviewer's verdict recorded; DESIGN.md + design.json present.
