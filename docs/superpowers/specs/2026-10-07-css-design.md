# CSS design: the docs site in the lagoon style (DRAFT, brainstorming in progress)

> **Status (checkpoint, 2026-10-07): NOT an approved spec.** The question rounds and the approach choice are done. The design is being presented in 5 parts; **Part 1 was presented and is waiting for the user's yes.** Parts 2–5 below are outlines only and have not been presented. Resume: re-present Part 1 (text below) and ask "Does this part look right?", then present Parts 2–5 one at a time, then write the real spec over this file, self-review, and pause for the user's review. No branch exists yet; create one (`just wt-add docs-lagoon`) only when the spec is written.

## Goal

The docs site (`apps/docs`, Fumadocs on TanStack Start) looks like Studio's API page (the Scalar reference in the lagoon palette, `apps/studio/src/components/api-reference.css`): same palette, fonts, method colours, hairlines instead of shadows, dark. The Fumadocs OpenAPI pages stay (they are prerendered and feed search and llms.txt); they are themed, not replaced.

## Decisions (user, 2026-10-06)

- **D1 Scope = theming only (option 1).** Mounting the real Scalar reference in the docs was rejected: search and llms.txt would lose the operations, and it could not be prerendered.
- **D2 No Scalar "Test" button in the docs** (fumadocs-openapi's `withScalar()` hook): Studio already gives that experience. Findings for a later button are in the roadmap's "Resume here" entry (no `servers` in the documents, gateway CORS, unscoped client CSS, custom `playground.render`, preferred target a gateway-URL + token popover).
- **D3 Dark only**, like Studio. No light mode yet; a lagoon light palette and switch for both apps is a separate, unscheduled follow-up (roadmap).
- **D4 Shared tokens live in a workspace package** (`packages/lagoon`), the one source of truth; Studio and the docs import it. The `landing-page` branch carries its own copy and rebases onto the package when it merges.
- *(Cross-reference, 2026-10-07: the roadmap's isolation follow-up puts the docs site behind the gateway at `/docs`, not `/v1/docs`; keep the dropped `/v1/docs` route gone.)*
- **D5 Drop the public `GET /v1/docs` page** and its embedded Scalar bundle. **Keep `GET /v1/openapi.json`.** Reasons: it showed only the Management API, Studio and the docs cover all three APIs, and a skin would need a third home in Rust assets. (A user premise was corrected along the way: dropping it does not affect a future Test button.)
- **D6 Approach: "everything in the package".** The package ships the tokens, the base styles, a Tailwind 4 preset, the Fumadocs variable map with its overrides, and the Scalar map. (Rejected: keeping the Fumadocs map in the docs app; rebuilding the docs layout by hand.)

## Facts gathered (feasibility)

- Studio tokens: `apps/studio/src/styles.css` (`:root` palette, `@theme inline` map, fonts from `@fontsource-variable/atkinson-hyperlegible-next`, `@fontsource/spectral/700.css`, `@fontsource-variable/jetbrains-mono`; `@custom-variant dark` forces dark; shadcn semantic tokens; base rules for body, selection, scrollbars).
- Studio Scalar skin: `apps/studio/src/components/api-reference.css`, scoped to `.studio-api` and `body > .scalar-app`, inlined via `?inline` only while the API page is mounted. Method tones: GET `#9cc8ea`, PUT `#c4b6ec`, PATCH `#e3d58c`, POST seafoam, DELETE coral.
- Docs styles: `apps/docs/src/styles/app.css` imports tailwindcss, `fumadocs-ui/css/neutral.css`, `fumadocs-ui/css/preset.css`, `fumadocs-twoslash/twoslash.css`, `fumadocs-openapi/css/preset.css`. Provider: `RootProvider` from `fumadocs-ui/provider/tanstack` in `apps/docs/src/routes/__root.tsx` (next-themes; takes a `theme` option, so dark can be forced).
- Fumadocs colour variables (`fumadocs-ui/css/lib/default-colors.css`): `--color-fd-{background,foreground,muted,muted-foreground,popover,popover-foreground,card,card-foreground,border,primary,primary-foreground,secondary,secondary-foreground,accent,accent-foreground,ring,overlay}`, declared in `@theme` and under `.dark`; plus static `info, warning, error, success, idea, diff-*`.
- fumadocs-openapi method badges (`dist/ui/components/method-label.js` + shared `badge.js`): Tailwind classes by colour name, GET=green, POST=blue, PUT=yellow, PATCH=orange, DELETE=red (`text-*-600 dark:text-*-400`).
- Versions at the time: fumadocs-openapi 12.2.0 pinned (12.3.0 latest), `@scalar/api-reference-react` 0.9.75 pinned (0.9.78 latest), root `@scalar/api-reference` 1.72.2. Check the registry again before planning.
- Workspace packages are source-only (e.g. `packages/client`: `main`/`exports` point at `src`); `pnpm-workspace.yaml` covers `packages/*`; moon covers `packages/*`; `apps/docs/moon.yml` lists inputs from `/packages/client`.

### What dropping `/v1/docs` touches (D5)

- `services/orchestrator/src/docs.rs` (serves the page, `scalar.js` **and the document**: the `/v1/openapi.json` handler must stay, moved or kept there).
- `services/orchestrator/assets/scalar/` (`standalone.js`, `VERSION`).
- `crates/chelabase-routes/src/lib.rs`: `API_DOCS`, `API_DOCS_SCRIPT`, and the registry test table entry (`("/v1/docs", "orchestrator.api_docs")`, around line 822).
- `services/orchestrator/tests/api.rs`: `docs_page_configures_scalar_offline`, `scalar_bundle_matches_version_file`.
- `scripts/scalar.sh`; justfile `scalar-sync`, the `just scalar-sync` line in `api-gen`, `scripts/scalar.sh check` in `check`.
- Root `package.json` devDependency `@scalar/api-reference` (Studio's `@scalar/api-reference-react` stays).
- `CONTRIBUTING.md` (the `just check` row mentions the Scalar bundle check); `apps/docs/content/docs/architecture/api-generation.mdx` ("API reference UI" section and the api-gen list).
- To verify: `services/orchestrator/src/sql_editor/guard.rs` matched "scalar" in a search; probably unrelated.

## Design outline (5 parts)

1. The package `packages/lagoon` (presented, awaiting approval; text below).
2. The docs app: `app.css` imports (drop `neutral.css`, add the lagoon files), forced dark in `RootProvider` and no theme toggle, fonts, the landing page and nav in lagoon, OpenAPI pages themed.
3. Studio switches to the package: its `styles.css` imports `tokens.css`, `base.css`, `tailwind.css` and keeps its shadcn map and app hooks; `api-reference.tsx` inlines `scalar.css` from the package and its wrapper takes `.lagoon-scalar`; the fontsource dependencies move to the package. No visual change in Studio.
4. Dropping `/v1/docs` (the list above), keeping `/v1/openapi.json`, docs and CONTRIBUTING updates.
5. Tests and verification: the package's contrast test; Studio's existing tests stay green; a docs build-output test asserting the lagoon tokens reach the built CSS and that the `dark` class is set; orchestrator route tests; `just check`; a browser pass through `just live` comparing the docs reference page with Studio's API page.

## Part 1 of 5 (as presented): the package `packages/lagoon`

`@chelabase/lagoon`, private, no build step; the apps import its CSS files straight from source, as `@chelabase/client` exports its TypeScript.

Files:

- `tokens.css`: the three font imports, then one `:root` block with the lagoon palette exactly as Studio has it today (ink, deep, channel, shallows, sand, sand-lit, seafoam, coral, palm, chalk, chalk-dim, line, line-strong, input, the four claw colours), `color-scheme: dark`, `--radius`, and the font stacks as `--font-sans`, `--font-serif`, `--font-mono`. Values copied verbatim: the approved design is the reference.
- `base.css`: what Studio's stylesheet does globally and the docs should too: body background and colour, sand selection, line-strong scrollbars, border-box sizing.
- `tailwind.css`: the Tailwind 4 preset (a CSS file imported after `tailwindcss`): the `@theme inline` block mapping `--color-ink` … `--color-line-strong`, the three font families, `--ease-lagoon` and the three radii.
- `fumadocs.css`: maps every `--color-fd-*` variable onto the tokens, declared for both `:root` and `.dark` (Fumadocs switches on `.dark`). Below the map, targeted overrides: hairlines instead of shadows on popovers and the search dialog, the radius scale, code blocks on deep, and the OpenAPI method badges remapped to the Studio tones by Fumadocs' fixed name mapping (green = GET, blue = POST, yellow = PUT, orange = PATCH, red = DELETE), scoped to the reference pages.
- `scalar.css`: Studio's Scalar variable map moved unchanged, scope class renamed `.studio-api` → `.lagoon-scalar`; Studio's wrapper takes the new class.
- `README.md`: one screen: what each file is, the one-meaning rule (coral hazard, seafoam yes, sand action and focus, palm reserved, method tones for methods only), how an app consumes it.
- `tests/contrast.test.ts`: vitest parsing the token values and asserting today's contrast claims: each method tone and text colour ≥ 4.5:1 on ink, deep and channel; the input border ≥ 3:1 on all three.

Dependencies: the three fontsource packages move here from Studio. Dev: vitest, typescript. Vite resolves a CSS `@import` of a bare package name relative to the importing file, so the font imports resolve from the package's own `node_modules`. The package gets a `moon.yml` with a test task, and both apps add `/packages/lagoon/**/*` to their moon inputs.

Out of the package: Studio's shadcn semantic map (`--background`, `--primary`, …) and the `dark` custom variant stay per app.
