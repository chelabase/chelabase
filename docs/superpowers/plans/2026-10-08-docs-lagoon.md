# Docs lagoon (CSS design) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** One workspace package, `@chelabase/lagoon`, holds the lagoon tokens, fonts and skins; Studio and the docs site consume it; the docs site looks like Studio's API page; the public `GET /v1/docs` page goes.

**Architecture:** A source-only CSS package (no build step) imported by path from each app's stylesheet. Studio's tokens and Scalar skin move into it; a Fumadocs variable map themes the docs. The orchestrator drops the Scalar page and keeps `GET /v1/openapi.json`.

**Tech Stack:** CSS (Tailwind 4 `@theme`), Vite, vitest 5, Fumadocs (fumadocs-ui `npm:@fumadocs/base-ui@16.15.18`, fumadocs-openapi 12.2.0) on TanStack Start, Rust (axum) for the orchestrator.

**Spec:** `docs/superpowers/specs/2026-10-07-css-design.md` (D1–D8, parts 1–5). Implementers read the spec's part for their task.

## Status

- **2026-10-08: plan written and pre-flight scanned, awaiting the user's review.** Pre-flight: every path and line reference checked against `docs-lagoon` at `6696f68`; fumadocs-openapi 12.2.0's `method-label.js` confirms PUT=yellow, PATCH=orange, POST=blue, default (GET)=green; `RootProvider`'s `theme` and the layout's `themeSwitch.enabled` exist in the installed fumadocs-ui; `services/orchestrator/src/openapi.rs` exists (T4 moves the handler there); the docs app has no font declarations; the fontsource packages are at their latest (5.3.0). Open item for T3: whether Fumadocs' API page markup offers a page-scoped selector for the badge remap (if not, T3 stops and reports). Branch `docs-lagoon` (`.worktrees/docs-lagoon`), base `main` at `b63953d`.
- **Plan approved 2026-10-08. All four tasks done and merged.** End-of-step review (opus) on `34ec766..c9ebfb0`: **With fixes, 0 critical, 0 important, 7 minor** (all five Review Focus items confirmed); its fix round (sonnet) done (`c9ebfb0..4640738`, verified by the coordinator; lagoon 48/48, Studio 692/692, docs 21/21). **`just check` passed (exit 0) on `215b730` + the Status edit. AT THE END-OF-STEP PAUSE: waiting for the user's go to merge into `main`.** No image build (no fork change). **The browser pass is deferred (user, 2026-10-08) to the `apps/landing` step**, which changes the docs and Studio's look again; it then compares the docs' reference pages with Studio's API page (fonts, palette, method tones, hairlines, Deprecated in sand), no theme toggle, `/` landing on `/docs`, and Studio's serif weights.
- **User decision D11 (2026-10-08, from the review's M2):** fumadocs-openapi's "Deprecated" badge on an operation heading is sand (attention), guarded; deprecated parameter/auth badges stay coral. Coordinator ruling R7: the tone contrast claim is restated as what the new tint test asserts (≥ 5.4:1 on ink, deep, channel and the 10% badge tint; coral's minimum is 5.48 on channel), not the old untested 5.6:1 (cost if wrong: wording only).
- Done: **T3** `569e083..bae19cc`, merged (review: spec ✅, 1 important + 4 minors fixed in the fix round with D9 and D10: badges remapped by fumadocs-openapi's class signature `.font-mono.font-medium.dark\:text-<colour>-400`, guarded by two build tests; callouts and diffs in lagoon colours with contrast tests, `default-colors.css` import dropped; hairline only on bordered popover surfaces; no canonical link on `/`; Spectral 700 on every `#nd-page` heading except `.not-prose`). Merged branch: lagoon 46/46 tests.
- **User decisions (2026-10-08, from T3's concerns):** D9 method badges are remapped **by fumadocs-openapi's badge classes** (every method badge in the docs, sidebars included, uses the lagoon tones; nothing else using Tailwind's colours changes; a test catches a Fumadocs class change), replacing the plan's operation-page scope. D10 callouts and diffs take **lagoon colours** by the one-meaning rule: error and diff-remove = coral, success and diff-add = seafoam, warning = sand, info and idea = chalk-dim (T3 had restored Fumadocs' stock static palette via `fumadocs-ui/css/lib/default-colors.css` after dropping `neutral.css`).
- T3's rulings (listed to the user): `hotKey: false` (the "d" key can't toggle the theme); the nav title links to `/docs` in Spectral bold; Fumadocs' larger radii (`rounded-xl`/`2xl`) capped at `--radius` (8px); prose headings weight 700; `/` is a meta-refresh page with no `beforeLoad` (TanStack's prerender follows redirects and would drop the meta tag).
- Done: **T2** `569e083..417be4b`, merged (review: spec ✅; 2 minors fixed in the fix round: the font guard now checks whole values, the shorthand and Tailwind arbitrary families via `tests/fonts.ts` `fontViolations()` with 21 matcher cases; `DESIGN.md` and `.impeccable/design.json` point at `@chelabase/lagoon/scalar.css`). CSS comparison vs `main`: no custom property changed after resolving `var()`; new only `--lagoon-font-*` and `--method-*`; `@font-face` + Spectral 400, 400 italic, 600.
- Done: **T1** `ef62ec8..e260f8a`, merged `569e083` (review: spec ✅, quality approved, 1 minor fixed in the fix round: a duplicate-declaration test). **T4** `ef62ec8..ff743f1`, merged after T1 (no own review per the table; 28 routes tests and 494 orchestrator tests green; `sha2`/`hex` stay, still used elsewhere).
- Ruling R6 (coordinator, 2026-10-08): the gate must run the new package, so T2 also adds `pnpm --filter @chelabase/lagoon typecheck` and `test` to `just check` (the plan missed it; cost if wrong: none, two lines).
- Rulings (listed to the user): R1 the three method tones (`--method-get/put/patch`) move from the Scalar skin's scope into `tokens.css` `:root`, so `fumadocs.css` can use them (cost if wrong: three globally visible custom properties; no visual effect). R2 `--input` (the 40 % chalk control border) is a lagoon token in `tokens.css`; Studio's shadcn map stops redefining it. R3 Studio keeps `html, body, #app { min-height: 100% }` (`#app` is Studio's mount point); `base.css` carries `body { margin: 0; background: var(--ink); color: var(--chalk) }` (same values Studio resolves today). R5 font stacks live in `tokens.css` as `--lagoon-font-sans/serif/mono`; `tailwind.css` maps Tailwind's `--font-*` onto them (avoids a self-referencing `--font-sans`; cost if wrong: three extra custom properties). R4 dependency versions: the three fontsource packages are at their latest (5.3.0, checked 2026-10-08); Fumadocs and Scalar are not upgraded in this step (theming only).
- Pending fork changes: none (this step changes no fork; no image build).

## Task table (binding)

| Task | What | Implementer | Reviewer | Runs |
|---|---|---|---|---|
| T1 | `packages/lagoon`: tokens, base, Tailwind preset, Scalar skin, README, contrast + scope tests | sonnet | sonnet | first, parallel with T4 |
| T2 | Studio switches to the package; the font-source test; the CSS comparison against `main` | sonnet | sonnet | after T1, parallel with T3 |
| T3 | `fumadocs.css` + the docs app (styles, forced dark, `/` redirect, build-output tests) | opus | sonnet | after T1, parallel with T2 |
| T4 | Drop `GET /v1/docs` and the Scalar bundle; keep `/v1/openapi.json` | sonnet | none (covered by its tests and the end-of-step review) | parallel with T1 |
| End | Whole-branch review | — | opus | after T1–T4 |
| Fixes | End-of-step review fixes | sonnet | coordinator verifies the diff | after the review |

Then: `just check` once, no image build, the browser pass (`just live`), the user's pause.

Parallel worktrees: T1 ∥ T4 (`just wt-add docs-lagoon-t1 docs-lagoon`, `docs-lagoon-t4`); T2 ∥ T3 (`-t2`, `-t3`), each merged back `--no-ff` and removed. T2 and T3 both touch `pnpm-lock.yaml` (each adds `@chelabase/lagoon: workspace:*`): the second merge resolves the lockfile by taking either side and re-running `pnpm install` from the branch worktree, then committing.

## Global Constraints

- Dark only (D3); no theme toggle anywhere.
- One font source (D8): only `packages/lagoon` imports fonts or defines font stacks (`--lagoon-font-*` in `tokens.css`, Tailwind's `--font-*` in `tailwind.css`); the set is the landing page's: `@fontsource/spectral/400.css`, `400-italic.css`, `600.css`, `700.css`, `@fontsource-variable/atkinson-hyperlegible-next`, `@fontsource-variable/jetbrains-mono`, all `^5.3.0`.
- Font stacks, verbatim: `--font-sans: "Atkinson Hyperlegible Next Variable", "Atkinson Hyperlegible", system-ui, sans-serif;` `--font-serif: Spectral, "Iowan Old Style", Georgia, serif;` `--font-mono: "JetBrains Mono Variable", ui-monospace, monospace;`
- Token values move verbatim from `apps/studio/src/styles.css` (the approved design is the reference).
- The package is source-only like `packages/client`: `"private": true`, `"type": "module"`, `exports` pointing at source files; vitest `^5.0.2`, typescript `^7.0.2`, `@types/node` `^24.19.0`.
- Strict TDD (the preamble). Never run `cargo fmt` on the workspace.

## Review Focus

1. A Studio page that uses Spectral at a weight other than 700 (e.g. `font-semibold` on a serif heading) must render the real face, not a fallback: covered by T2's `@font-face` comparison (Spectral 400, 400 italic, 600 added, nothing removed).
2. Scalar's teleported popovers (`body > .scalar-app`) must stay themed after the scope rename: T1's scope test and T2's wrapper test.
3. The prerendered docs must be dark with no light flash on a static host (no JS yet): T3's `html_is_dark_without_javascript` asserts `class="dark"` (or containing `dark`) on `<html>` in the built HTML.
4. `/` on a static host (Cloudflare Pages, no server) must still land on `/docs`: T3's `root_redirects_to_docs` asserts the prerendered `index.html` carries a meta refresh to `/docs`, not just a client-side redirect.
5. Old links to `/v1/docs` and `/v1/docs/scalar.js` must get a clean 404, not a 500 or the SPA: T4's route tests.

---

### Task 1: the package `packages/lagoon`

**Files:**
- Create: `packages/lagoon/package.json`, `packages/lagoon/moon.yml`, `packages/lagoon/tsconfig.json`, `packages/lagoon/vitest.config.ts`
- Create: `packages/lagoon/tokens.css`, `base.css`, `tailwind.css`, `scalar.css`, `README.md`
- Create: `packages/lagoon/tests/css.ts` (a tiny shared parser), `tests/contrast.test.ts`, `tests/scalar-scope.test.ts`
- Do not touch Studio or the docs (T2/T3).

**Interfaces (produces):**
- `package.json`: `"name": "@chelabase/lagoon"`, `"exports": { "./tokens.css": "./tokens.css", "./base.css": "./base.css", "./tailwind.css": "./tailwind.css", "./scalar.css": "./scalar.css", "./fumadocs.css": "./fumadocs.css" }` (T3 creates `fumadocs.css`; declare the export now), `"sideEffects": ["*.css"]`, `"dependencies"`: the three fontsource packages `^5.3.0`; scripts `typecheck` (`tsc --noEmit`), `test` (`vitest run`).
- `moon.yml`: `language: "typescript"`, tasks `typecheck` and `test` exactly as `packages/client/moon.yml`.
- `tokens.css` `:root` custom properties (names are the contract): `--ink --deep --channel --shallows --sand --sand-lit --seafoam --coral --palm --chalk --chalk-dim --line --line-strong --input --shell --shell-dark --shell-light --tip --method-get --method-put --method-patch --radius --lagoon-font-sans --lagoon-font-serif --lagoon-font-mono`, plus `color-scheme: dark`. The `--lagoon-font-*` values are the literal stacks of Global Constraints (R5). Values from Studio's `styles.css` (`--input: rgb(230 237 231 / 0.4)` with its WCAG comment; method tones from `api-reference.css` lines 44–46, R1).
- `tailwind.css`: `@theme inline { --color-ink: var(--ink); … --color-line-strong: var(--line-strong); --color-shallows: var(--shallows); --color-input: var(--input); --ease-lagoon: cubic-bezier(0.16, 1, 0.3, 1); --radius-sm: calc(var(--radius) - 4px); --radius-md: calc(var(--radius) - 2px); --radius-lg: var(--radius); }` then a separate non-inline `@theme { --font-sans: var(--lagoon-font-sans); --font-serif: var(--lagoon-font-serif); --font-mono: var(--lagoon-font-mono); }`, so Tailwind emits `--font-*` on `:root` and `var(--font-sans)` works in plain CSS (the Scalar skin, `base.css`) as it does in Studio today. Never define `--font-*` in `tokens.css` (R5: a `:root { --font-sans }` plus Tailwind's `--font-sans: var(--font-sans)` would be a cycle).
- `base.css`: `* { box-sizing: border-box; scrollbar-color: var(--line-strong) transparent; }`, `body { margin: 0; background: var(--ink); color: var(--chalk); font-family: var(--font-sans); }`, `::selection { background: var(--sand); color: var(--ink); }`.
- `scalar.css`: `apps/studio/src/components/api-reference.css` copied with `.studio-api` → `.lagoon-scalar` everywhere and the three `--method-*` declarations removed (now in `tokens.css`); everything else byte-identical. Studio's copy is deleted in T2, not here.
- `tests/css.ts`: `export function customProperties(css: string): Map<string, string>` (declarations `--x: value;`, comments stripped) and `export function selectors(css: string): string[]` (the scope test's existing splitter from `apps/studio/src/components/api-reference.test.tsx:91-95`, moved here).

- [ ] **Step 1: failing tests.** `contrast.test.ts`: `method_tones_and_text_reach_4_5_to_1` (each of `--method-get --method-put --method-patch --seafoam --coral --chalk --chalk-dim --sand` ≥ 4.5:1 on each of `--ink --deep --channel`, WCAG relative luminance from the hex values read out of `tokens.css`); `input_border_reaches_3_to_1` (`--input` composited over each of ink/deep/channel ≥ 3:1). `scalar-scope.test.ts`: `every_rule_is_scoped` (each selector part starts with `.lagoon-scalar` or equals `body > .scalar-app`), `popovers_are_themed` (`body > .scalar-app` present), `maps_onto_lagoon_tokens` (contains `--scalar-background-1: var(--ink)`, `--scalar-color-accent: var(--sand)`, `--scalar-color-red: var(--coral)`, `--scalar-font: var(--font-sans)`, `--scalar-color-blue: var(--method-get)`). Plus `tokens.test.ts`: `tokens_match_studio_values` pinning each palette value listed above (copy them from Studio's file into the test as the expected table).
- [ ] **Step 2:** `pnpm install` (adds the workspace package), then `pnpm --filter @chelabase/lagoon test` — FAIL (files missing).
- [ ] **Step 3:** write the CSS files and README (spec part 1: what each file is, the one-meaning rule, the font rule D8, how to consume: the import order of T2/T3).
- [ ] **Step 4:** `pnpm --filter @chelabase/lagoon test` and `pnpm --filter @chelabase/lagoon typecheck` — PASS.
- [ ] **Step 5:** commit (`lagoon: the package: tokens, base, Tailwind preset, Scalar skin`).

Gotcha: Vite resolves a CSS `@import "@fontsource/…"` relative to the importing file, so the font imports in `tokens.css` resolve from `packages/lagoon/node_modules` — the fontsource packages must be the package's own dependencies.

### Task 2: Studio switches to the package

**Files:**
- Modify: `apps/studio/src/styles.css`, `apps/studio/src/components/api-reference.tsx:6,54,58`, `apps/studio/src/components/api-reference.test.tsx:76-102`, `apps/studio/package.json` (deps), `apps/studio/moon.yml` (no inputs block today: add `build.inputs` only if the task gains one; otherwise none needed — moon's default inputs plus the workspace dep suffice; state which in the report)
- Delete: `apps/studio/src/components/api-reference.css`
- Create: `packages/lagoon/tests/fonts-single-source.test.ts`

**Interfaces (consumes T1):** `@chelabase/lagoon/tokens.css`, `base.css`, `tailwind.css`, `scalar.css`; `selectors()` from `packages/lagoon/tests/css.ts`.

Behaviour:
- `styles.css` starts `@import "tailwindcss"; @import "@chelabase/lagoon/tokens.css"; @import "@chelabase/lagoon/base.css"; @import "@chelabase/lagoon/tailwind.css";`. Removed: the three fontsource imports, the lagoon entries of `@theme inline` (`--color-ink` … `--color-line-strong`, the fonts, ease, radii), the palette and claw variables in `:root`, `color-scheme`, `--radius`, `--input`, the `*` box-sizing and scrollbar rules, `body` margin/background/colour, `::selection`. Kept: `@custom-variant dark`, the shadcn `@theme inline` entries (`--color-background` … `--color-ring`), the shadcn `:root` map (minus `--input`, R2), `html, body, #app { min-height: 100% }` (R3), the app-rail and SQL-editor rules.
- `api-reference.tsx`: `import lagoonCss from "@chelabase/lagoon/scalar.css?inline";`, wrapper `className="lagoon-scalar"`, comment updated.
- Fontsource deps leave Studio; `"@chelabase/lagoon": "workspace:*"` added to dependencies.

- [ ] **Step 1: failing tests.** `fonts-single-source.test.ts` (D8): `no_app_imports_fonts` (no `@fontsource` string in any `apps/*/src/**/*.{css,ts,tsx}` except `*.test.*`); `no_app_defines_font_stacks` (no `font-family:` / `fontFamily:` whose value is not exactly `var(--font-sans|serif|mono)`, and no `--font-[a-z-]+\s*:` definition, in the same files); `no_app_depends_on_fontsource` (no `@fontsource` key in any `apps/*/package.json`); `the_package_ships_the_landing_set` (`tokens.css` imports exactly the six font entries of Global Constraints, and its `--lagoon-font-*` values equal the Global Constraints stacks). Update `api-reference.test.tsx`'s theming test: wrapper `.lagoon-scalar`, two `<style>` tags, the scoping assertions removed (they live in T1's test).
- [ ] **Step 2:** run `pnpm --filter @chelabase/lagoon test` and `pnpm --filter @chelabase/studio test` — the font tests and the wrapper test FAIL.
- [ ] **Step 3:** make the changes above.
- [ ] **Step 4:** `pnpm --filter @chelabase/lagoon test`, `pnpm --filter @chelabase/studio typecheck`, `pnpm --filter @chelabase/studio test` — PASS.
- [ ] **Step 5: CSS comparison (spec part 5).** Build Studio (`pnpm --filter @chelabase/studio build`) here and in a temporary detached worktree of `main` (`git worktree add --detach <scratch>/studio-main main`, `pnpm install --frozen-lockfile`, build, then `git worktree remove`). A scratch script (not committed) extracts from each `dist` CSS: every custom-property declaration under `:root` and every `@font-face` (family, weight, style). Expected: every custom property present on `main` has the same value on the branch after resolving `var()` chains (`--font-sans` now resolves through `--lagoon-font-sans` to the same stack); the only new properties are `--lagoon-font-sans/serif/mono` and `--method-get/put/patch`; the branch's `@font-face` set = `main`'s plus exactly Spectral 400 normal, 400 italic, 600 normal (each fontsource import emits several unicode-range faces: compare the set of (family, weight, style)). Paste the script's output in the report.
- [ ] **Step 6:** commit (`studio: lagoon tokens and Scalar skin from @chelabase/lagoon`).

### Task 3: the docs site in lagoon

**Files:**
- Create: `packages/lagoon/fumadocs.css`, `packages/lagoon/tests/fumadocs.test.ts`
- Modify: `apps/docs/src/styles/app.css`, `apps/docs/src/routes/__root.tsx`, `apps/docs/src/routes/index.tsx`, `apps/docs/src/lib/layout.shared.tsx`, `apps/docs/package.json`, `apps/docs/moon.yml` (build inputs gain `"/packages/lagoon/**/*"`), `apps/docs/tests/build-output.test.ts`

**Interfaces (consumes T1):** `@chelabase/lagoon/tokens.css`, `base.css`, `tailwind.css`; token names listed in T1.

Behaviour:
- `fumadocs.css`: every `--color-fd-*` variable (`background foreground muted muted-foreground popover popover-foreground card card-foreground border primary primary-foreground secondary secondary-foreground accent accent-foreground ring overlay`) set for both `:root` and `.dark` onto the tokens (background ink, foreground chalk, muted deep, muted-foreground chalk-dim, popover/card deep, border line, primary sand, primary-foreground ink, secondary/accent channel, ring sand, overlay ink at 60 %). Overrides below the map: popovers and the search dialog use a `1px solid var(--line)` border and no shadow; radii from `--radius`; code blocks (`figure.shiki`, `pre`) on `--deep`; headings in prose `font-family: var(--font-serif)`; code `var(--font-mono)`. Method badges: Fumadocs paints GET=green, POST=blue, PUT=yellow, PATCH=orange, DELETE=red via `text-*-600 dark:text-*-400`; on the reference pages remap `--color-green-400: var(--method-get)`, `--color-blue-400: var(--seafoam)`, `--color-yellow-400: var(--method-put)`, `--color-orange-400: var(--method-patch)`, `--color-red-400: var(--coral)`, scoped to a selector that only matches the OpenAPI pages. **Gotcha:** find that selector in Fumadocs' rendered API page markup; if none exists without changing `api-page.tsx`, stop and report (the spec keeps `api-page.tsx` unchanged).
- `app.css` order: `tailwindcss`, `@chelabase/lagoon/tokens.css`, `base.css`, `tailwind.css`, `fumadocs-ui/css/preset.css`, `fumadocs-twoslash/twoslash.css`, `fumadocs-openapi/css/preset.css`, `@chelabase/lagoon/fumadocs.css`; `neutral.css` removed; the scrollbar-gutter and scroll-lock rules kept.
- Dark: `<html lang="en" className="dark" suppressHydrationWarning>` and `RootProvider theme={{ forcedTheme: 'dark', defaultTheme: 'dark', enableSystem: false }}` (`ThemeOptions extends ThemeProviderProps`, `fumadocs-ui/dist/provider/base.d.ts:14`); `baseOptions()` adds `themeSwitch: { enabled: false }` (`layouts/shared/index.d.ts:40,57`).
- `/`: `routes/index.tsx` drops `HomeLayout`; its `head` emits `<meta http-equiv="refresh" content="0; url=/docs">` and a canonical link to `/docs`, and the route redirects client-side (`beforeLoad: () => { throw redirect({ to: '/docs/$', params: { _splat: '' } }) }` only if prerender still writes `index.html` with the meta tag — verify in `dist/client/index.html`; if TanStack's prerender drops redirecting routes, keep the meta-refresh page without `beforeLoad`).

- [ ] **Step 1: failing tests.** `fumadocs.test.ts`: `maps_every_fd_colour` (each of the 17 names set under both `:root` and `.dark`, each value a `var(--…)` of a lagoon token), `badges_use_the_studio_tones` (the five remaps above). In `build-output.test.ts`: `html_is_dark_without_javascript` (`docs/index.html`'s `<html` tag has class `dark`), `lagoon_tokens_reach_the_css` (a built CSS file under `dist/client` contains `--ink:#0b1c1f` (minified form) and `--color-fd-background:var(--ink)`), `no_theme_toggle` (`docs/index.html` contains no theme-switch button: assert the Fumadocs toggle's `aria-label`, read from its source, is absent), `root_redirects_to_docs` (`dist/client/index.html` contains `http-equiv="refresh"` with `url=/docs`).
- [ ] **Step 2:** `pnpm --filter @chelabase/lagoon test`; `pnpm --filter @chelabase/docs build && pnpm --filter @chelabase/docs test` — new tests FAIL.
- [ ] **Step 3:** implement.
- [ ] **Step 4:** `pnpm --filter @chelabase/lagoon test`, `pnpm --filter @chelabase/docs typecheck`, `pnpm --filter @chelabase/docs build`, `pnpm --filter @chelabase/docs test` — PASS (the existing prerender, llms.txt and link tests included). The font-source test (T2) must pass for `apps/docs` too: no font declarations in the docs app.
- [ ] **Step 5:** commit (`docs: the lagoon theme, dark only, / redirects to /docs`).

### Task 4: drop `GET /v1/docs`, keep `GET /v1/openapi.json`

**Files:**
- Move: the `openapi_json` handler and its route from `services/orchestrator/src/docs.rs` into the existing `services/orchestrator/src/openapi.rs` (which builds the document; pre-flight 2026-10-08) as `pub(crate) fn router<S>() -> Router<S>` with the same bounds and headers; delete `docs.rs`; `lib.rs:45` drops `mod docs;` and `lib.rs:1796` merges `openapi::router()`
- Delete: `services/orchestrator/assets/scalar/` (`standalone.js`, `VERSION`; and `assets/` if empty), `scripts/scalar.sh`
- Modify: `crates/chelabase-routes/src/lib.rs:124-127,739-740,907-908` (remove `API_DOCS`, `API_DOCS_SCRIPT` and their table rows), `services/orchestrator/tests/api.rs:1909-1960` (remove `docs_page_configures_scalar_offline`, `scalar_bundle_matches_version_file`), `justfile:482-486,495` (remove `just scalar-sync` from `api-gen`, the `scalar-sync` recipe, `scripts/scalar.sh check` from `check`), root `package.json:10` (remove `@scalar/api-reference`, then `pnpm install`), `CONTRIBUTING.md:56` (drop "the Scalar bundle check"), `apps/docs/content/docs/architecture/api-generation.mdx:83` (rewrite the paragraph: Studio's **API** page renders the Management document with "Try it out"; the docs site has the reference pages for all three APIs; `GET /v1/openapi.json` stays public; remove the api-gen list's scalar entry)
- Note: `branches.rs:231` has an unrelated inner `mod docs`; `sql_editor/guard.rs`'s "Scalar" matches are Postgres node names (unrelated).

- [ ] **Step 1: failing tests.** In `services/orchestrator/tests/api.rs`: `docs_page_is_gone` (`GET /v1/docs`, `/v1/docs/scalar.js` and `/v1/docs/nope.js` → 404); keep `openapi_json_route_serves_the_document` (line 1896). In `crates/chelabase-routes`: the registry table test no longer lists the two paths, plus `api_docs_paths_are_unrouted` (looking up `GET /v1/docs` and `GET /v1/docs/scalar.js` in the registry yields no endpoint).
- [ ] **Step 2:** `cargo test -p chelabase-routes` and `cargo test -p orchestrator --test api docs_page_is_gone` — FAIL.
- [ ] **Step 3:** remove the code, files and references above.
- [ ] **Step 4:** `cargo test -p chelabase-routes`, `cargo test -p orchestrator --test api`, `cargo test -p orchestrator openapi::tests`, `git grep -n -i "scalar.sh\|scalar-sync\|API_DOCS\|/v1/docs"` outside `docs/superpowers` and `docs/roadmap` → no hits (the docs-site route `/docs` is unrelated) — PASS.
- [ ] **Step 5:** commit (`orchestrator: drop the /v1/docs Scalar page; /v1/openapi.json stays`).

### End of step

- [ ] Whole-branch review (opus): spec D1–D8 and parts 1–5 against the diff; the Review Focus list.
- [ ] Fix round (sonnet), coordinator verifies the diff.
- [ ] `just check` (once).
- [ ] Browser pass (`just live`, Playwright MCP): first start redoes Zitadel's first-instance setup and `just zitadel-bootstrap` (the stack was torn down after 6g). Compare the docs' Management reference page with Studio's API page (fonts, palette, method tones, hairlines); no theme toggle; `/` lands on `/docs`; Studio's serif headings unchanged except the true Spectral weights. Move the stack's apps back to `main` afterwards.
- [ ] Update this Status, the roadmap's "Resume here", `docs/landing-highlights.md` (on `main`); pause for the user.
