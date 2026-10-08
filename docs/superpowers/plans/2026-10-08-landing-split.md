# Landing split Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The landing page leaves Studio for its own prerendered app, `apps/landing`, on `@chelabase/lagoon` and Tailwind, with the merge label and a PlanetScale column; Studio's `/` gains `?signup`.

**Architecture:** A TanStack Start app shaped like `apps/docs` (prerendered to `dist/client`), first ported verbatim from `origin/landing-page` (`75cc0d3`), screenshotted by a Playwright recipe, then rewritten on Tailwind with the screenshots as the parity gate. Studio's sign-in page reads a `signup` search flag and redirects to sign-up.

**Tech Stack:** TanStack Start 1.168.60 / router 1.170.41 on Vite 8, React 19.3, Tailwind 4.3.3 with `@chelabase/lagoon`, Vitest 5 (+ jsdom, Testing Library), `@playwright/test` 1.64.0.

**Spec:** `docs/superpowers/specs/2026-10-08-landing-split-design.md` (D1–D6, parts 1–4). Implementers read the spec part for their task.

## Status

- **Plan approved 2026-10-08. All tasks, the end-of-step review (opus: ready with fixes, 0 critical, 2 important, 8 minor) and its fix round (sonnet, `7ee71a6..1c89e37`, verified by the coordinator) done. **`just check` passed (exit 0) on `80c08bf`.** At the user's pause (merge on the user's go). No image build (no fork change); no live pass (D1).
- Fix round result: lagoon 56, landing 31, Studio 701, docs build ok; baselines re-recorded once in `1c89e37` for D10/D11 (1440 and 390: 25px shorter, step 2 one line less; 390 hero: second branch label `search` → `staging`; 1024 unchanged), then 2 clean runs. Deviation: Studio's `moon.yml` has no `test` task, so only its `typecheck` got the lagoon input. **New for the mobile pass:** at 390px the right-hand branch label is clipped at the viewport edge (`stagi`, before `searc`; pre-existing).
- End-of-step review findings, all in the fix round: I1 `/?signup` redirect uses `redirectMethod: "replace"` (Back from Zitadel otherwise re-enters the redirect); I2 → D10; M3 → D11; M4 empty `VITE_STUDIO_URL` falls back to the default; M5 lagoon colour-literal check walks `src/**/*.{ts,tsx}` and rejects `rgb(`/`hsl(`; M6 the one-palette test also scans `.tsx` arbitrary properties `[--x:…]`; M7 dead `data-secondary`/`data-soon`, un-export `SCENE_MAX`, `TIDE_PX`, `describeHoursAgo`, `ClawPaths`; M8 moon inputs for landing and Studio include `/packages/lagoon/src/**/*`; M9 lagoon package description and README palette, `.impeccable/design.json` (user: fix now). **To the polish step's audit (user, 2026-10-08):** M10 inline code `--shell-dark` on `--sand` ≈ 2.9:1; whether the reduced-motion end state shows the merge label and burrow.
- **D10 (user, 2026-10-08):** the walkthrough's step 2 says "Open Studio and sign up." (not "choose Start free": a self-hoster opening Studio sees Sign in / Create account).
- **D11 (user, 2026-10-08):** phones show the merge too: with 2 forks the second branch is `staging` and merges into main, as the third does on desktop. The 390px baseline (and, for D10's copy, all three) are re-recorded once in the fix round; the coordinator checks the image diffs are only those changes.
- Done: **T5** `e462760..a9139d0` (review sonnet: approve). Studio rail parity checked rule by rule against the deleted `.app-rail .wordmark*` CSS and Studio's built CSS (not in a browser: the rail is behind sign-in and the stack is down); landing visual gate 3/3, baselines untouched; docs build ok. Lagoon 55/55, Studio 701/701, landing 26/26. Carried to the end-of-step fix round: `no_hex_in_components` reads only `src/crab-marks.tsx`; make it scan every `src/**/*.{ts,tsx}`.
- Done: **T4** `7b7ea79..7ba3926` (impl opus `..97b7ecb`; review sonnet: with fixes, 1 critical + 1 minor fixed in `7ba3926`, verified by the coordinator). Critical: the `max-height: 780px` hero rules were built from a `${SHORT}:` template fragment that Tailwind's scanner can't see, so they were never generated (the 900px-tall baselines couldn't catch it); now a `short` custom variant with literal classes, guarded by `short_screen_rules_are_built` (the build test accepts Lightning CSS's `height<=780px` form). Minor: scrollable `pre` and the compare scroller get the lagoon focus ring. No other fragment classes found. Visual gate 3/3 against untouched baselines (max 954 px differing, antialiasing). Landing 26/26, lagoon 50/50, Studio 701/701. Accepted deviations: band edges are inline-SVG `Wave`s; the tide-staff track gradient stays in the leftover CSS; `.lagoon` descendant rules became per-element utilities (`FOCUS`, `[&_code]:`); `hover:` now applies only on hover-capable devices (Tailwind's default; no sticky hover on touch).
- Done: **T3** `a9f2add..ec05817` (no own review per the table; coordinator viewed the 1440 baseline: full page renders with lagoon fonts and palette). Baselines 1440×5667, 1024×5938, 390×8918; recorded once, then 2 clean check runs. Deviations: `@playwright/test` pinned 1.63.0 (1.64.0 is under pnpm's release-age window); the web server runs `node_modules/.bin/vite preview` directly (`pnpm exec` left vite orphaned and Playwright hung).
- Done: **T1** `c0cf36e..0bd9c13`, merged `efa3a26` (review sonnet: approve, 3 minors fixed in `0bd9c13`, verified by the coordinator: stale `.impeccable` auth/mode text, build test reads `VITE_STUDIO_URL`). Landing 23/23, lagoon 48/48. Accepted deviations: dependency ranges match `apps/docs` (pnpm's release-age check rejected lucide-react 1.53.0 and vite 8.3.4; no `minimumReleaseAgeExclude` entries added); PRODUCT.md on `main` already had the three brand lines, so only "(see DESIGN.md)" went; the branch had no reduced-motion test, so T1 wrote `shows_the_complete_end_state_without_animating`.
- Done: **T2** `c0cf36e..6e84e68`, merged `76f6909` (no own review per the table; coordinator read the diff; Studio 701/701). Extra test `no_redirect_when_an_error_arrives_after_the_redirect`; `signed_in_with_signup_goes_to_app` written with `src/test/render-router.tsx` (a regression guard, green on first run). Note for verifiers: `pnpm --filter` from the main checkout resolves to main's app; use `pnpm --dir <worktree>/apps/<app>`.
- 2026-10-08: plan written and pre-flight scanned. Branch `landing-split` (`.worktrees/landing-split`), base `main` at `cd95c38`, plus the spec commit.
- Pre-flight (checked against `main` at `cd95c38` and `origin/landing-page` at `75cc0d3`):
  - Registry (2026-10-08, `pnpm view`): @tanstack/react-start 1.168.60, @tanstack/react-router 1.170.41 (both = docs' pins), vite 8.3.4, @vitejs/plugin-react 6.1.2, @tailwindcss/vite + tailwindcss 4.3.3, vitest 5.0.3, react 19.3.0, typescript 7.0.2, lucide-react 1.53.0, @testing-library/react 16.3.3, jsdom 30.1.2, @playwright/test 1.64.0. No `minimumReleaseAge` is configured. Chromium builds 1243/1246/1247 are cached in `~/.cache/ms-playwright`; if 1.64.0 wants another build, T3 runs `pnpm exec playwright install chromium`.
  - Lagoon's font test (`packages/lagoon/tests/fonts-single-source.test.ts`) walks every `apps/*`, so `apps/landing` is covered with no change; the landing's six font `@import`s and `--sans/--serif/--mono` must go in T1 or that test fails.
  - `landing.css` (1,181 lines) hex colours: all equal lagoon tokens except `#18333a` (sand-plate hover on the landfall band) and `#dcae8a` (`--wet-sand`, landing-only); `#f7d6b9` = `--sand-lit`. `rgb(…)` literals are token colours with alpha (ink, sand, seafoam) plus one black shadow.
  - Plain-CSS candidates after the rewrite: `@keyframes survey`, `@keyframes ripple`, the stroke-dash rules (lines ~1090–1110), the `prefers-reduced-motion` block, the range-input thumbs (`::-webkit-slider-thumb`, `::-moz-range-thumb`), `pre` scrollbars, `.mark--*::before` glyphs, `:root:has(.lagoon)`.
  - Studio has no test of `src/routes/index.tsx`; no Studio test mocks `react-oidc-context`, hence T2's prop-driven component.
  - `just check` (justfile ~l.485–501) lists docs typecheck/build/test; the landing's lines go right after them.
- **D7 (user, 2026-10-08):** the landing's two non-lagoon colours, `#dcae8a` (wet sand) and `#18333a` (sand-plate hover), become lagoon tokens `--wet-sand` and `--ink-lit` (with preset mappings) rather than app-local tokens, since later surfaces may use them. The app then has no colour literal at all.
- **D8 (user, 2026-10-08):** the crab's eye whites become the lagoon token `--claw-eye: #fff6ee` (T4 added it; the pre-flight missed the hex in `crab-marks.tsx`), and Studio uses it too.
- **D9 (user, 2026-10-08):** the two identical `crab-marks.tsx` copies (Studio's app rail, the landing) are unified now in `packages/lagoon` as a React export; new task T5 (sonnet / sonnet).
- Pending fork changes: none (no fork touched; no image build).

## Task table (binding)

| Task | What | Implementer | Reviewer | Runs |
|---|---|---|---|---|
| T1 | Scaffold `apps/landing`; verbatim port; content changes; unit + prerender tests; moon/just/`just check`; PRODUCT.md; `.impeccable` | sonnet | sonnet | first, parallel with T2 |
| T2 | Studio `/?signup` | sonnet | none (end-of-step review) | parallel with T1 |
| T3 | Playwright visual recipe + committed baselines | sonnet | none (T4 exercises it) | after T1 |
| T4 | Tailwind rewrite; lagoon `--wet-sand`/`--ink-lit` tokens (D7) and preset mappings; no-hex test; baselines still match; verbatim CSS deleted | opus | sonnet | after T3 |
| T5 | Crab marks move to `packages/lagoon` (`@chelabase/lagoon/crab-marks`), used by Studio and the landing; Studio's eyes use `--claw-eye` (D8, D9) | sonnet | sonnet | after T4's fix round |
| End | Whole-branch review | — | opus | after T1–T5 |
| Fixes | End-of-step review fixes | sonnet | coordinator verifies the diff | after the review |

Then: `just check` once, no image build, no live pass (D1), the user's pause.

Parallel worktrees: T1 ∥ T2 (`just wt-add landing-split-t1 landing-split`, `landing-split-t2`), each merged back `--no-ff` and removed. Both may touch `pnpm-lock.yaml` only if T2 adds a dependency (it shouldn't); on conflict the second merge takes either side and re-runs `pnpm install` in the branch worktree, then commits. T3 and T4 run in the branch worktree.

## Global Constraints

- Source of the port: `git show origin/landing-page:apps/studio/src/components/landing/<file>` (files: `landing-page.tsx`, `landing-page.test.tsx`, `shore-scene.tsx`, `tide-gauge.tsx`, `crab-marks.tsx`, `landing.css`), `apps/studio/.impeccable/design.json`, `apps/studio/.impeccable/surfaces/src-routes-index-tsx.md`, `PRODUCT.md`. Never merge or rebase that branch; never copy its `DESIGN.md` (D4).
- One font source (docs-lagoon D8): no app imports fonts or defines font stacks; the landing uses `var(--font-sans|serif|mono)` (plain CSS) or `font-sans|serif|mono` (Tailwind).
- One palette source: the landing never redeclares a custom property that `packages/lagoon/tokens.css` declares.
- Dark only; `<html lang="en" class="dark">` in the prerendered HTML.
- Studio URL: `import.meta.env.VITE_STUDIO_URL ?? "http://localhost:3000"`, trailing slash stripped; CTAs: "Start free" → `${studioUrl}/?signup`, "Log in" → `${studioUrl}/`.
- Copy, verbatim: hero last label `staging merged into main`; aria-label tail `…one branch then resets, and staging is merged into main, its crab burrowing into main.`; comparison columns `Chelabase`, `Supabase`, `Firebase`, `Vanilla Neon`, `PlanetScale`; every PlanetScale mark `unknown` ("To survey").
- Dependency versions: the registry versions in the Status (exact pins for TanStack, matching docs; `^` ranges as in docs for the rest).
- Strict TDD (the preamble). The visual recipe is never part of `just check`.

## Review Focus

1. A `VITE_STUDIO_URL` with a trailing slash (`https://studio.example.com/`) must not produce `//?signup`: T1 test `studio_url_trailing_slash_is_normalised`.
2. A static host serves `dist/client/index.html` with no JS: the copy, the CTA links and `class="dark"` must be in the HTML: T1 `prerendered_html` tests.
3. `/?signup` while signed in must go to `/app`, not start a sign-up: T2 `signed_in_with_signup_goes_to_app`.
4. A sign-up redirect that fails (Zitadel down) must not loop: T2 `no_second_redirect_after_an_error`, and the redirect fires at most once per mount (`redirects_once_under_rerender`).
5. Reduced motion must still show a complete hero (the end state, every crab) with no animation running: T1 ports the branch's reduced-motion test; T3's screenshots are taken with `reducedMotion: 'reduce'`.

---

### Task T1: `apps/landing`, verbatim port and content

**Files:**
- Create `apps/landing/`: `package.json`, `moon.yml` (copy `apps/docs/moon.yml`; build inputs `src/**/*`, `vite.config.ts`, `/packages/lagoon/**/*`; outputs `dist`), `tsconfig.json` (copy docs', minus the docs-only excludes), `vite.config.ts`, `vitest.config.ts`, `.gitignore` (`dist/`, `.tanstack/`, `.output/`, `node_modules/`, `.impeccable/review/`, `test-results/`, `playwright-report/`), `src/router.tsx`, `src/routeTree.gen.ts` (generated, committed like docs'), `src/routes/__root.tsx`, `src/routes/index.tsx`, `src/studio-url.ts`, `src/styles.css`, `src/components/{landing-page,shore-scene,tide-gauge,crab-marks}.tsx`, `src/components/landing.css`, `src/components/landing-page.test.tsx`, `tests/build-output.test.ts`, `tests/tokens-single-source.test.ts`, `.impeccable/design.json`, `.impeccable/surfaces/src-routes-index-tsx.md`.
- Modify: `justfile` (`landing` recipe after `docs`; three `just check` lines after the docs ones), `PRODUCT.md` (the branch's three brand lines, without "(see DESIGN.md)"), `pnpm-lock.yaml`.

**Interfaces (produces):**
- `export function studioUrl(raw?: string): string` in `src/studio-url.ts`: `raw ?? "http://localhost:3000"`, trailing `/`s stripped.
- `export interface LandingPageProps { studioUrl: string }`; `export function LandingPage(props: LandingPageProps)` (no `busy`/`error`/`onStart`/`onLogin`).
- `src/styles.css`: `@import "tailwindcss"; @import "@chelabase/lagoon/tokens.css"; @import "@chelabase/lagoon/base.css"; @import "@chelabase/lagoon/tailwind.css";` (T4 adds the app `@theme`). `__root.tsx` links it with `?url` like docs.
- `vite.config.ts`: as docs minus `fumadocsMdx`; `server: { port: 3002 }`; `tanstackStart({ prerender: { enabled: true, crawlLinks: true }, pages: [{ path: '/' }] })`. `package.json` `dev` script: `vite dev --port 3002 --strictPort`. Output: `dist/client/index.html`.
- `vitest.config.ts`: `environment: 'jsdom'`, include `src/**/*.test.tsx` and `tests/**/*.test.ts`; `tests/build-output.test.ts` starts with `// @vitest-environment node`; plugin-react; `@` alias to `src`.

**Behaviour:**
- Port the components verbatim, then only: CTAs become `<a className="plate …" href=…>` (nav and hero), props per the interface, the label/aria/comment changes and the PlanetScale column (Global Constraints). `MARK` tuples widen to five.
- `landing.css` verbatim except: drop the six `@import`s; drop every declaration in `.lagoon { … }` whose name `tokens.css` declares (values are identical); replace `var(--sans|--serif|--mono)` with `var(--font-sans|--font-serif|--font-mono)`; `.plate` rules that relied on `:disabled` stay harmless.
- `__root.tsx`: `<html lang="en" className="dark">`, head meta: charset, viewport, title `Chelabase — Postgres that branches`, description (the hero lede's first sentence), `og:title`, `og:description`, `og:type=website`.
- `.impeccable` files copied; every `apps/studio` path inside → `apps/landing`, `src/components/landing/` → `src/components/`.
- `justfile`: `# The landing page (apps/landing) on http://localhost:3002; needs no stack` / `landing:` / `pnpm --filter @chelabase/landing dev`; `just check` + `typecheck`, `build`, `test` for `@chelabase/landing`.

**Tests (write first, see them fail):**
- `landing-page.test.tsx`: port every branch test that still applies (heading, sections, fork count, tide gauge, reduced motion), replacing the handler test with `ctas_link_to_studio` (every "Start free" link `href === "https://s.test/?signup"`, every "Log in" `href === "https://s.test/"`, at least two of each, rendered with `studioUrl="https://s.test"`); `hero_ends_with_the_merge_label` (text `staging merged into main` present, `promoted` absent); `hero_aria_label_describes_the_merge`; `comparison_has_five_columns_with_planetscale_to_survey` (5 column headers in order; the PlanetScale column's 8 cells all read "To survey").
- `studio_url_trailing_slash_is_normalised` (`studioUrl("https://a.test/")` and `"https://a.test//"` → `"https://a.test"`; `undefined` → `"http://localhost:3000"`), in `src/studio-url.test.ts` (add it to the include).
- `tests/build-output.test.ts` (`prerendered_html`): `dist/client/index.html` contains `class="dark"` on `<html>`, the headline `Postgres that branches. Run it yourself.`, `staging merged into main`, `href="http://localhost:3000/?signup"`, `href="http://localhost:3000/"`, and no `<!--$!-->` (React's client-render fallback marker, as in docs' build test).
- `tests/tokens-single-source.test.ts`: no CSS file under `src/` declares a custom property that `packages/lagoon/tokens.css` declares (parse names with a regex over `--[\w-]+\s*:`); a helper case proves it catches one.

**Verify:** `pnpm --filter @chelabase/landing typecheck`, `build`, `test`; `pnpm --filter @chelabase/lagoon test` (font test now sees `apps/landing`). Commit per TDD step.

### Task T2: Studio `/?signup`

**Files:** Create `apps/studio/src/components/sign-in.tsx`, `apps/studio/src/components/sign-in.test.tsx`. Modify `apps/studio/src/routes/index.tsx`.

**Interfaces:**
- `export interface SignInProps { auth: Pick<AuthContextProps, "isLoading" | "error" | "signinRedirect">; signup: boolean }` (`AuthContextProps` from `react-oidc-context`); `export function SignIn(props: SignInProps)`: today's signed-out page body (heading, text, two buttons, error alert), unchanged markup.
- Route: `validateSearch: (s: Record<string, unknown>): { signup?: true } => ("signup" in s ? { signup: true } : {})`; component: signed in → `<Navigate to="/app" replace />` (as today), else `<SignIn auth={auth} signup={!!Route.useSearch().signup} />`. Existing `to="/"` links keep compiling (search is optional).

**Behaviour:** in `SignIn`, an effect calls `auth.signinRedirect({ prompt: "create" })` when `signup && !auth.isLoading && !auth.error`, guarded by a ref so it fires at most once per mount; with an error it shows the existing alert and never redirects.

**Tests (first):** `sign-in.test.tsx` with a fake `auth` object (`vi.fn()` for `signinRedirect`):
`signup_redirects_to_create_once` (called once with `{ prompt: "create" }`); `redirects_once_under_rerender` (rerender with a new auth object → still one call); `no_redirect_while_loading` (isLoading → 0 calls, then loaded → 1); `no_redirect_without_signup`; `no_second_redirect_after_an_error` (error set → 0 calls, alert shown); `buttons_still_sign_in_and_create` (the two buttons call `signinRedirect()` and `signinRedirect({ prompt: "create" })`). In `routes/index.test.tsx` or the same file via the exported `validateSearch`: `validate_search_reads_bare_signup` (`{ signup: "" }` → `{ signup: true }`, `{}` → `{}`). `signed_in_with_signup_goes_to_app` is covered by the route keeping its `Navigate` branch first; assert it with a router test only if Studio already has a router-render helper (`src/test/`), else the end-of-step review checks the ordering.

**Verify:** `pnpm --filter @chelabase/studio typecheck`, `test`.

### Task T3: Visual recipe and baselines

**Files:** Create `apps/landing/playwright.config.ts`, `apps/landing/visual/landing.spec.ts`, `apps/landing/visual/landing.spec.ts-snapshots/*.png` (committed). Modify `apps/landing/package.json` (`@playwright/test` dev dep, script `visual: "playwright test"`), `justfile` (`landing-visual *args:` → `pnpm --filter @chelabase/landing build && pnpm --filter @chelabase/landing visual {{args}}`, comment: "Pixel check of the landing page against committed baselines; `--update-snapshots` re-records; not part of just check").

**Behaviour:** config `testDir: 'visual'`, one Chromium project, `use: { reducedMotion: 'reduce', colorScheme: 'dark', deviceScaleFactor: 1 }`, `expect.toHaveScreenshot: { maxDiffPixelRatio: 0.002, animations: 'disabled' }`, `webServer: { command: 'pnpm exec vite preview --port 4173 --strictPort', port: 4173, reuseExistingServer: false }`. The spec loops widths `[1440, 1024, 390]` (height 900): set viewport, `goto('/')`, `await page.evaluate(() => document.fonts.ready)`, `expect(page).toHaveScreenshot(\`landing-${w}.png\`, { fullPage: true })`. Vitest must not pick up `visual/` (T1's include already excludes it).

**Tests:** the spec is the test: record with `just landing-visual --update-snapshots`, then run `just landing-visual` twice, green both times (determinism). If two clean runs differ, find the source (font loading, a time-dependent tide gauge value) and pin it in the spec (e.g. `page.clock.setFixedTime`), not by raising the tolerance.

### Task T4: Tailwind rewrite

**Files:** Modify `apps/landing/src/components/*.tsx`, `apps/landing/src/styles.css` (app `@theme`), `packages/lagoon/tailwind.css`, `packages/lagoon/tests/tokens.test.ts`; replace `src/components/landing.css` with the small leftover `src/landing.css` (imported from `styles.css`). Modify `packages/lagoon/tokens.css`. Create `apps/landing/tests/no-hex.test.ts`.

**Behaviour:**
- Lagoon `tokens.css` `:root` gains (D7) `--wet-sand: #dcae8a;` and `--ink-lit: #18333a; /* sand-plate hover on an ink-on-sand band, as on the landing */`, next to the palette.
- Lagoon preset `@theme inline` gains `--color-shell: var(--shell); --color-shell-dark: var(--shell-dark); --color-shell-light: var(--shell-light); --color-tip: var(--tip); --color-wet-sand: var(--wet-sand); --color-ink-lit: var(--ink-lit);`.
- App `@theme` in `styles.css` holds layout only: `--container-landing: 1240px`, `--spacing-gutter: 24px`, `--spacing-section: 104px`. `--ease-out` usages → `ease-lagoon`. No hex anywhere in the app.
- Alpha literals become opacity modifiers (`bg-ink/8`, `text-ink/80`, `shadow-sand/50`, `bg-seafoam/12`, `bg-sand/5`); the black shadow → `shadow-black/60`.
- Leftover `src/landing.css` holds only: `@keyframes survey` and `ripple` and the rules that run them, the stroke-dash rules, the `prefers-reduced-motion` block, the range thumbs, `pre` scrollbars, `.mark--*::before` glyphs, `:root:has(.lagoon)`. Anything else there is a review finding. It may use only `var(--…)` colours.
- Everything else becomes utilities in JSX (arbitrary values allowed where no scale fits, e.g. `text-[0.9375rem]`). Class-name lists that grow long go in a local `const` per component, not a new abstraction layer.
- Done when `just landing-visual` passes against T3's unchanged baselines, and the old `src/components/landing.css` is gone.

**Tests (first):** `no-hex.test.ts` `no_colour_literals`: no `#[0-9a-f]{3,8}\b` in `src/**/*.{tsx,ts,css}`; also no `rgb(`/`hsl(` literal in `src/` except `rgb(0 0 0 …)` if a utility can't express it (prefer none). `packages/lagoon/tests/tokens.test.ts` `preset_maps_the_landing_colours`: `tailwind.css` maps `--color-shell`, `--color-shell-dark`, `--color-shell-light`, `--color-tip`, `--color-wet-sand`, `--color-ink-lit` to the same-named `var(--…)`, each declared in `tokens.css` with the values above. T1's tests stay green.

**Verify:** `pnpm --filter @chelabase/landing typecheck`, `build`, `test`; `pnpm --filter @chelabase/lagoon test`; `pnpm --filter @chelabase/studio test` (preset change); `just landing-visual` (must pass without `--update-snapshots`; the baselines' git blob ids unchanged).

### Task T5: Crab marks in `packages/lagoon` (D8, D9)

**Files:** Create `packages/lagoon/src/crab-marks.tsx` (moved from `apps/landing/src/components/crab-marks.tsx`), `packages/lagoon/tests/crab-marks.test.tsx`. Modify `packages/lagoon/package.json` (export `"./crab-marks": "./src/crab-marks.tsx"`; `peerDependencies` `react: ^19.3.0`; devDependencies `react`, `react-dom`, `@types/react`, `@types/react-dom` at Studio's ranges), `packages/lagoon/tsconfig.json` (`jsx: "react-jsx"`, `lib` + `DOM`, include `src`), `packages/lagoon/vitest.config.ts` (include `tests/**/*.test.{ts,tsx}`), `packages/lagoon/tailwind.css` (`@source "./src";` so every app importing the preset generates the components' utilities), `packages/lagoon/README.md` (the new export). Delete `apps/landing/src/components/crab-marks.tsx`, `apps/studio/src/components/landing/crab-marks.tsx` (and the empty folder). Modify the importers in `apps/landing` and `apps/studio/src/components/app-shell/app-rail.tsx` to `@chelabase/lagoon/crab-marks`; `apps/studio/package.json` already depends on `@chelabase/lagoon`. Modify `apps/studio/src/styles.css`: delete the `.app-rail .wordmark`, `.wordmark__visual`, `.wordmark__claw` rules (keep `.app-rail { --claw-gap }` and the reduced-motion block).

**Interfaces:** the module exports exactly what T4's `crab-marks.tsx` exports (`ClawMark`, `CrabFigure`, `Wordmark`, `PalmTree`, their prop types), same signatures. Eye fills use `var(--claw-eye)` (D8).

**Behaviour:**
- The shared `Wordmark`/`ClawMark` carry only the styling both apps had: Studio's rail rules (serif, 700, `tracking-[-0.02em]`, `leading-none`, chalk; visual `inline-flex items-end`; claw `w-auto h-[1.32em] -mr-[0.14em]`). Anything T4 added beyond that (e.g. `-mb-[0.1em]`, `overflow-visible`, other nudges) moves to the landing's call sites via `visualClassName`/`clawClassName`/`className`, so Studio's rail renders exactly as before and the landing exactly as T4 left it.
- No hex in `packages/lagoon/src` (extend lagoon's own colour-literal check to `src/**/*.tsx`).

**Tests (first):** `crab-marks.test.tsx` (node env, `react-dom/server` `renderToStaticMarkup`): `wordmark_has_an_accessible_name` ("Chelabase" in an `sr-only` span, the visual `aria-hidden`); `eye_whites_use_the_claw_eye_token` (no `#fff6ee`, `var(--claw-eye)` present); `no_hex_in_components`. `tokens.test.ts`: `tailwind_preset_scans_the_components` (`@source "./src"` present). Studio: the existing app-rail tests stay green; add `app_rail_wordmark_keeps_its_rail_classes` only if Studio's rail test can see the class list (else the reviewer compares Studio's deleted CSS with the shared defaults by hand).

**Verify:** `pnpm --dir <wt>/packages/lagoon run typecheck` and `test`; `pnpm --dir <wt>/apps/studio run typecheck`, `test`, `build`; `pnpm --dir <wt>/apps/landing run typecheck`, `build`, `test`; `just landing-visual` (baselines untouched); `pnpm --dir <wt>/apps/docs run build` (the preset now scans `src`).

## End of step

Whole-branch review (opus) against the spec and the Review Focus; fixes (sonnet), verified by the coordinator. Then `just check` once. No image build, no live pass. Records: this Status, the roadmap's "Resume here" and "Landing page" entry, `docs/landing-highlights.md`. Then the user's pause.
