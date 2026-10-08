# CSS design: the lagoon package and the docs site in the lagoon style

> **Status: approved 2026-10-08; implemented on `docs-lagoon`.** Design parts 1–5 approved by the user on 2026-10-08. Branch `docs-lagoon` (`.worktrees/docs-lagoon`). Where the implementation departs from the text below, the plan's Status (`docs/superpowers/plans/2026-10-08-docs-lagoon.md`) records the ruling.

## Goal

The docs site (`apps/docs`, Fumadocs on TanStack Start) looks like Studio's API page (the Scalar reference in the lagoon palette): same palette, fonts, method colours, hairlines instead of shadows, dark. The lagoon tokens move into one workspace package, `packages/lagoon`, which Studio and the docs import (and, in a later step, `apps/landing`). The Fumadocs OpenAPI pages stay (they are prerendered and feed search and llms.txt); they are themed, not replaced. The public `GET /v1/docs` page goes.

## Decisions

- **D1 Scope = theming only** (user, 2026-10-06). Mounting the real Scalar reference in the docs was rejected: search and llms.txt would lose the operations, and it could not be prerendered.
- **D2 No Scalar "Test" button in the docs** (user, 2026-10-06): Studio already gives that experience. Findings for a later button are in the roadmap's "Resume here" CSS-design entry.
- **D3 Dark only**, like Studio (user, 2026-10-06). A lagoon light palette and switch for all apps is a separate, unscheduled follow-up.
- **D4 Shared tokens live in a workspace package** (`packages/lagoon`), the one source of truth. **Sequencing (user, 2026-10-08): package first, then the landing page.** This step ships the package and the docs theming. A later step moves the landing page out of Studio into `apps/landing` on a fresh branch from `main`, copying the `landing-page` branch's files (not rebasing its 363-commit drift), built on the package from day one. Where the docs are hosted (`docs.chelabase.com` or `chelabase.com/docs`) is decided in that step; this step keeps the docs app's `/docs/…` routes.
- **D5 Drop the public `GET /v1/docs` page** and its embedded Scalar bundle; **keep `GET /v1/openapi.json`** (user, 2026-10-06). It showed only the Management API, Studio and the docs cover all three APIs, and a skin would need a third home in Rust assets. The docs site later goes behind the gateway at `/docs` (roadmap, stack rework), never `/v1/docs`.
- **D6 Approach: "everything in the package"** (user, 2026-10-07): tokens, base styles, a Tailwind 4 preset, the Fumadocs variable map with its overrides, and the Scalar map. Rejected: keeping the Fumadocs map in the docs app; rebuilding the docs layout by hand.
- **D7 No docs home page** (user, 2026-10-08): the landing page and Studio link into the docs, so the docs app's `/` redirects to `/docs` (the introduction).
- **D8 One font source** (user, 2026-10-08): fonts stay consistent across every page of every app. Only `packages/lagoon` imports fonts and declares font stacks; apps use `var(--font-…)` or the Tailwind font utilities. Enforced by a test (part 5). **The landing page's fonts are the reference (user, 2026-10-08): Studio and the docs inherit them; the landing page's fonts don't change.** The `landing-page` branch's `landing.css` loads Spectral 400, 400 italic, 600 and 700, Atkinson Hyperlegible Next (variable) and JetBrains Mono (variable), with the same stacks Studio has today (`"Spectral", "Iowan Old Style", Georgia, serif`; `"Atkinson Hyperlegible Next Variable", "Atkinson Hyperlegible", system-ui, sans-serif`; `"JetBrains Mono Variable", ui-monospace, monospace`). So this step's `tokens.css` imports that full set; Studio (today Spectral 700 only) gains Spectral 400, 400 italic and 600. The later landing step only drops `landing.css`'s own imports and renames its `--sans`/`--serif`/`--mono` to the package's `--font-*` (no visual change), so D8's test passes for `apps/landing`.

## Facts (feasibility, 2026-10-07/08)

- Studio tokens: `apps/studio/src/styles.css` (149 lines): fontsource imports (lines 2–4: `@fontsource-variable/atkinson-hyperlegible-next`, `@fontsource/spectral/700.css`, `@fontsource-variable/jetbrains-mono`), `@custom-variant dark` (line 7), `@theme inline` (line 9), `:root` palette (line 43), base rules for `*`, `html`, `body`, `::selection` and scrollbars (lines 83–149).
- Studio Scalar skin: `apps/studio/src/components/api-reference.css`, scoped to `.studio-api` and `body > .scalar-app`, inlined via `?inline` in `api-reference.tsx` (line 6) only while the API page is mounted; wrapper class at line 58; scoping test in `api-reference.test.tsx` (lines 78–100). Method tones: GET `#9cc8ea`, PUT `#c4b6ec`, PATCH `#e3d58c`, POST seafoam, DELETE coral.
- Docs styles: `apps/docs/src/styles/app.css` imports `tailwindcss`, `fumadocs-ui/css/neutral.css`, `fumadocs-ui/css/preset.css`, `fumadocs-twoslash/twoslash.css`, `fumadocs-openapi/css/preset.css`, plus scrollbar-gutter and scroll-lock rules. `RootProvider` from `fumadocs-ui/provider/tanstack` in `src/routes/__root.tsx` (next-themes; takes a `theme` option). `src/routes/index.tsx` is a `HomeLayout` page with a title and an "Open the docs" link. `src/lib/layout.shared.tsx` `baseOptions()` sets only the nav title.
- Fumadocs colour variables (`fumadocs-ui/css/lib/default-colors.css`): `--color-fd-{background,foreground,muted,muted-foreground,popover,popover-foreground,card,card-foreground,border,primary,primary-foreground,secondary,secondary-foreground,accent,accent-foreground,ring,overlay}` in `@theme` and under `.dark`; static `info, warning, error, success, idea, diff-*`.
- fumadocs-openapi method badges: Tailwind classes by colour name, GET=green, POST=blue, PUT=yellow, PATCH=orange, DELETE=red (`text-*-600 dark:text-*-400`).
- Workspace packages are source-only (`packages/client`: `main`/`exports` point at `src`); `pnpm-workspace.yaml` and moon cover `packages/*`.
- Versions seen 2026-10-07: fumadocs-openapi 12.2.0 pinned (12.3.0 latest), `@scalar/api-reference-react` 0.9.75 pinned (0.9.78 latest), root `@scalar/api-reference` 1.72.2. The plan re-checks the registry.

## Design

### Part 1: the package `packages/lagoon`

`@chelabase/lagoon`, private, no build step; the apps import its CSS files straight from source.

- `tokens.css`: the landing page's font imports (D8: `@fontsource/spectral/400.css`, `400-italic.css`, `600.css`, `700.css`, `@fontsource-variable/atkinson-hyperlegible-next`, `@fontsource-variable/jetbrains-mono`), then one `:root` block with the lagoon palette exactly as Studio has it (ink, deep, channel, shallows, sand, sand-lit, seafoam, coral, palm, chalk, chalk-dim, line, line-strong, input, the four claw colours), `color-scheme: dark`, `--radius`, and the font stacks `--font-sans`, `--font-serif`, `--font-mono`. Values copied verbatim.
- `base.css`: the global rules: body background and colour, sand selection, line-strong scrollbars, border-box sizing.
- `tailwind.css`: the Tailwind 4 preset, imported after `tailwindcss`: the `@theme inline` block mapping `--color-ink` … `--color-line-strong`, the three font families, `--ease-lagoon` and the three radii.
- `fumadocs.css`: every `--color-fd-*` variable mapped onto the tokens, for both `:root` and `.dark`; then targeted overrides: hairlines instead of shadows on popovers and the search dialog, the radius scale, code blocks on deep, heading and code fonts, and the OpenAPI method badges remapped to Studio's tones by Fumadocs' fixed colour names (green = GET, blue = POST, yellow = PUT, orange = PATCH, red = DELETE), scoped to the reference pages.
- `scalar.css`: Studio's Scalar map moved unchanged, scope class `.studio-api` → `.lagoon-scalar`.
- `README.md`: one screen: what each file is; the one-meaning rule (coral hazard, seafoam yes, sand action and focus, palm reserved, method tones only for HTTP methods); the font rule (D8); how an app consumes the package.
- `tests/`: see part 5.

Superseded as built (see the plan's Status): R5 moved the font stacks to `--lagoon-font-sans/serif/mono` (Tailwind's `--font-*` map onto them); D9 scopes the badge remap by the badge's class signature (`.font-mono.font-medium.dark:text-<colour>-400`) instead of "the reference pages"; D11 (2026-10-08) makes the operation heading's "Deprecated" badge sand.

Dependencies: the three fontsource packages move here from Studio. Dev: vitest, typescript. Vite resolves a CSS `@import` of a bare package name relative to the importing file, so the font imports resolve from the package's own `node_modules`. A `moon.yml` with a test task; each consuming app adds `/packages/lagoon/**/*` to its moon inputs. Not in the package: Studio's shadcn semantic map and the `dark` custom variant (per app).

### Part 2: the docs app

- `src/styles/app.css`: drop `neutral.css`; import, in order, `tailwindcss`, `@chelabase/lagoon/tokens.css`, `base.css`, `tailwind.css`, then Fumadocs' `preset.css`, the twoslash and openapi presets, and last `@chelabase/lagoon/fumadocs.css`. Keep the scrollbar-gutter and scroll-lock rules.
- Dark only: `RootProvider` forces dark; `baseOptions()` disables the theme switch; `<html>` is dark from the first paint (no light flash). The plan pins the exact Fumadocs option names against the installed version.
- Fonts from the tokens: Atkinson body, Spectral 700 headings, JetBrains Mono code.
- `src/routes/index.tsx` becomes a redirect to `/docs`; `HomeLayout` goes. The nav keeps the text name "Chelabase" in Spectral (the claw-C wordmark arrives with `apps/landing`).
- OpenAPI pages: theming only; `api-page.tsx` unchanged.
- As built (see the plan's Status): callouts and diffs follow D10 (coral hazard, seafoam yes, sand attention, chalk-dim notes), and the `/` redirect carries no canonical link (ruling: the docs host is undecided, a relative canonical would be wrong).
- `package.json` adds `@chelabase/lagoon: workspace:*`; `moon.yml` adds `/packages/lagoon/**/*` to the inputs.

### Part 3: Studio switches to the package

- `src/styles.css`: the fontsource imports, the lagoon `:root` palette, the lagoon part of `@theme inline` and the global base rules go; it imports `tailwindcss`, then lagoon's `tokens.css`, `base.css`, `tailwind.css`. Stays: `@custom-variant dark`, the shadcn semantic map (pointing at lagoon tokens) and its `@theme inline` entries, and any Studio-only rule. The plan lists what moves and what stays line by line; values move verbatim.
- `src/components/api-reference.css` moves to `@chelabase/lagoon/scalar.css`; `api-reference.tsx` inlines `@chelabase/lagoon/scalar.css?inline`, still only while the API page is mounted; the wrapper class becomes `lagoon-scalar`.
- The scoping test moves to the package; Studio's test keeps asserting the wrapper class and the inlining.
- The fontsource dependencies leave Studio's `package.json`; it adds `@chelabase/lagoon: workspace:*` and the moon input.
- No visual change in Studio except the fonts it inherits (D8): serif text at a weight other than 700 now renders in its true Spectral face instead of a synthesized one (checked in part 5).

### Part 4: dropping `GET /v1/docs`, keeping `GET /v1/openapi.json`

- `services/orchestrator/src/docs.rs` keeps only the `/v1/openapi.json` handler (renamed `openapi.rs` if that is all that is left); the page and `scalar.js` handlers go. `services/orchestrator/assets/scalar/` (`standalone.js`, `VERSION`) is deleted.
- `crates/chelabase-routes/src/lib.rs`: `API_DOCS`, `API_DOCS_SCRIPT` and their registry-table entries go; both paths fall through to the gateway's 404.
- `services/orchestrator/tests/api.rs`: `docs_page_configures_scalar_offline` and `scalar_bundle_matches_version_file` go; a new test asserts `GET /v1/docs` is 404 and `GET /v1/openapi.json` still returns the document.
- `scripts/scalar.sh`, `just scalar-sync`, the scalar line in `api-gen` and `scripts/scalar.sh check` in `check` go; the root devDependency `@scalar/api-reference` goes (Studio's `@scalar/api-reference-react` stays).
- `CONTRIBUTING.md` (the `just check` row) and `apps/docs/content/docs/architecture/api-generation.mdx` (the "API reference UI" section and the api-gen list) are updated to point at Studio's API page and the docs' reference pages.
- The plan confirms `services/orchestrator/src/sql_editor/guard.rs`'s "scalar" match is unrelated.

### Part 5: tests and verification

- Package (vitest, the package's moon test task):
  - `contrast.test.ts`: each method tone and text colour ≥ 4.5:1 on ink, deep and channel; the input border ≥ 3:1 on all three.
  - `scalar-scope.test.ts`: every rule in `scalar.css` sits under `.lagoon-scalar` or `body > .scalar-app`.
  - `fonts-single-source.test.ts` (D8): walks `apps/*/src` and fails on any `@fontsource` import or any `font-family` / `--font-*` declaration outside `packages/lagoon` (`var(--font-…)` uses are allowed); asserts no app's `package.json` depends on a fontsource package. New apps are covered automatically.
- Studio: existing tests green (with part 3's `api-reference.test.tsx` changes). No visual change beyond D8, checked by machine: build Studio on `main` and on the branch, then compare the compiled CSS: the resolved custom-property values must be identical (class names and ordering may differ), and the branch's `@font-face` set must be `main`'s plus exactly Spectral 400, 400 italic and 600.
- Docs: a build-output test: the lagoon tokens reach the built CSS (`--color-fd-background` resolves to ink; the method-badge overrides are present), `<html>` carries `dark`, `/` redirects to `/docs`. `pnpm build` passes, including prerender and llms.txt.
- Orchestrator: the route tests from part 4.
- `just check` once at the end of the step. No fork change, so no image build; the live pass runs on the pinned images.
- Browser pass (`just live`, after the end-of-step review and its fixes): the docs' reference pages against Studio's API page (fonts, palette, method tones, hairlines), no theme toggle, `/` redirects. The stack was torn down after 6g, so the first start redoes Zitadel's first-instance setup and `just zitadel-bootstrap`.

## Out of scope

- The landing page (`apps/landing`) and where the docs are hosted (D4).
- A light mode (D3), a Scalar Test button in the docs (D2).
- The gateway's `/docs` route (the parked stack rework).
