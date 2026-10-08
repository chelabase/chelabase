# Landing split: `apps/landing` on the lagoon

Status: spec, approved in parts by the user on 2026-10-08 (question rounds Q1–Q5, approach, design parts 1–4).
Branch `landing-split` (`.worktrees/landing-split`), from `main` at `cd95c38`.

## Goal

Move the landing page off Studio into its own app, `apps/landing`, built on `@chelabase/lagoon`, with the
content updated for the shipped schema merge (6g). The source is the unmerged `landing-page` branch
(`origin/landing-page`, `75cc0d3`), copied rather than rebased.

## Decisions (user, 2026-10-08)

- **D1 Scope:** (a) move into `apps/landing` on the package, (b) Studio's `/` stops being the landing page,
  (c) the hero's merge label and a PlanetScale column. **Later step (not this one):** the Impeccable
  finish review, a fresh `DESIGN.md`, the polish and audit passes, the mobile pass, the docs host
  (`docs.chelabase.com` vs `chelabase.com/docs`) and docs-lagoon's deferred browser pass.
- **D2 CTAs:** plain links to a build-time Studio URL (`VITE_STUDIO_URL`, default `http://localhost:3000`).
  "Log in" → `${studioUrl}/`; "Start free" → `${studioUrl}/?signup`, which Studio turns into a sign-up redirect.
- **D3 Build:** TanStack Start on Vite with prerender, like `apps/docs`.
- **D4 Design files:** the `PRODUCT.md` brand lines and the `.impeccable` contract move now; the stale
  `DESIGN.md` stays on the old branch for the polish step.
- **D5 Content:** hero label "staging merged into main"; a PlanetScale column with every mark "To survey"
  (no research now).
- **D6 Approach:** rewrite `landing.css` on Tailwind with the lagoon preset, proven pixel-equivalent by a
  visual test against a verbatim port.

## Part 1: App shape and wiring

- `apps/landing` (`@chelabase/landing`, private): TanStack Start + Vite, `prerender: { enabled: true }`,
  one page `/`, static output in `dist`. Dependencies follow `apps/docs`'s pins (Start, router, React 19,
  Vite 8, `@tailwindcss/vite`, Vitest) plus `@chelabase/lagoon` (`workspace:*`) and `lucide-react`; the
  plan checks the registry for the latest of each first (CLAUDE.md, pnpm minimum-release-age respected).
- Layout: `src/routes/__root.tsx` (document shell: `<html class="dark">`, title, description, Open Graph
  meta), `src/routes/index.tsx` (renders `<LandingPage studioUrl=…/>`), `src/components/`
  (`landing-page.tsx`, `shore-scene.tsx`, `tide-gauge.tsx`, `crab-marks.tsx`), `src/styles.css`
  (Tailwind, lagoon tokens and preset, the landing-only `@theme`).
- Studio URL: `import.meta.env.VITE_STUDIO_URL ?? "http://localhost:3000"`, passed to `LandingPage` as the
  `studioUrl` prop (tests set it).
- Dev server `:3002`; `just landing` like `just docs`; `moon.yml` copies the docs tasks (dev, build,
  typecheck, test depends on build); `just check` gains landing typecheck, build and test lines next to the
  docs ones. Needs no stack.
- Studio gets none of the landing code and none of the branch's three fontsource dependencies (lagoon
  brings the fonts).
- Self-hosted never ships `apps/landing`; Compose and the gateway are unchanged.

## Part 2: The Tailwind rewrite and its parity proof

1. **Verbatim port:** the components and `landing.css` copied as they are, except: the six font
   `@import`s and the palette declarations that `tokens.css` already has are dropped; `--sans/--serif/--mono`
   become `--font-sans/--font-serif/--font-mono` (emitted on `:root` by the lagoon preset).
2. **Baselines:** a visual test (`@playwright/test`, dev dependency of `apps/landing`; Chromium is cached on
   this machine) against `vite preview` of `dist`, no stack. Full-page screenshots at 1440, 1024 and 390 px
   wide with `reducedMotion: 'reduce'` (the hero's static end state). Tolerance set in the plan, sized to
   antialiasing only. Baselines committed.
3. **Rewrite:** components move to Tailwind utilities on `@chelabase/lagoon/tailwind.css`; the baselines
   must still match.

Rules:
- Every colour comes from a lagoon token; no hex in app code.
- The package's preset gains `--color-shell`, `--color-shell-dark`, `--color-shell-light`, `--color-tip`
  (already in `tokens.css`, not mapped yet).
- Landing-only tokens (`--wet-sand`, `--container`, `--gutter`, `--section-gap`) go in the app's own
  `@theme`; `--ease-out` becomes the package's `--ease-lagoon` (same value).
- What Tailwind can't express cleanly stays in a small `src/landing.css`: `@keyframes`, the SVG scene's
  stroke and dash rules, complex `:has`/pseudo selectors. The plan lists that leftover set; nothing else
  stays.
- The verbatim CSS is deleted at the end. Lagoon's font-single-source test already scans every `apps/*`.

The visual test is a recipe, `just landing-visual` (with `--update` to re-record), **not** part of
`just check`: font rendering is machine-specific and the polish step changes visuals on purpose.

## Part 3: Content, Studio and design files

- **CTAs:** "Start free" → `${studioUrl}/?signup`, "Log in" → `${studioUrl}/`, in the nav and the hero, as
  `<a>` elements styled like today's plates. The `busy`/`error` props go (no auth state in the landing).
- **Studio `/`:** stays `main`'s Sign in / Create account page, plus `validateSearch` accepting `signup`.
  With `?signup` and signed out (auth loaded), it calls `signinRedirect({ prompt: "create" })` once.
  Signed in still goes to `/app`. After a sign-in error the page shows its existing error and doesn't
  redirect again.
- **Hero:** last label "staging merged into main"; the aria-label ends "…one branch then resets, and
  staging is merged into main, its crab burrowing into main." Comments and names say merge, not promote.
  The animation is unchanged.
- **Comparison:** columns Chelabase / Supabase / Firebase / Vanilla Neon / PlanetScale; all 8 PlanetScale
  marks "To survey"; rows and footnote unchanged.
- **PRODUCT.md:** the branch's three brand lines (provisional Spectral wordmark, dark default, visual
  world), without "(see DESIGN.md)" until the polish step writes it.
- **`.impeccable`:** `design.json` and `surfaces/src-routes-index-tsx.md` move to
  `apps/landing/.impeccable/` (the route is still `src/routes/index.tsx`, so the name stays); paths inside
  change from `apps/studio` to `apps/landing`. The branch's Studio `.gitignore` line for Impeccable scratch
  goes to `apps/landing/.gitignore`.
- **Records:** the roadmap's "Landing page" entry and "Resume here", the plan's Status,
  `docs/landing-highlights.md`. `origin/landing-page` is kept until the polish step (it holds `DESIGN.md`
  and the sweep notes), then deleted.

## Part 4: Tests and tasks

Tests (strict TDD):
- `apps/landing` (Vitest):
  - `landing_page.test.tsx` (ported from the branch): both CTAs link to `${studioUrl}/?signup` and
    `${studioUrl}/`; the hero labels include "staging merged into main"; the aria-label describes the merge;
    the table has 5 columns and every PlanetScale cell reads "To survey".
  - `tokens_single_source`: app CSS redeclares none of `tokens.css`'s custom properties.
  - `no_hex_colours`: no `#rgb`/`#rrggbb` in `src/**/*.{tsx,css}` (added with the rewrite).
  - `prerendered_html`: `dist/index.html` contains the headline, the merge label and the Studio links.
- `packages/lagoon` `tokens.test`: the preset maps `--color-shell*` and `--color-tip` to existing tokens.
- Studio `/` route: `?signup` signed out → `signinRedirect({prompt:"create"})` exactly once; no param → no
  redirect; signed in → `/app`; after an auth error → no further redirect.
- Visual (recipe): `just landing-visual`, three widths, reduced motion.

Tasks:

| # | Task | Implementer | Reviewer |
|---|---|---|---|
| T1 | Scaffold `apps/landing`, verbatim port, content changes (labels, PlanetScale, CTA links), unit and prerender tests, moon/just/`just check` wiring, PRODUCT.md, `.impeccable` | sonnet | sonnet |
| T2 | Studio `/?signup` (parallel with T1, disjoint files) | sonnet | none (end-of-step review) |
| T3 | Playwright visual test and committed baselines on T1's output | sonnet | none (T4 exercises it) |
| T4 | Tailwind rewrite, lagoon preset shell mappings, no-hex test; baselines still match; verbatim CSS deleted | opus | sonnet |
| End | Whole-branch review | opus | fixes: sonnet |

Then `just check` once. No image build (no fork change). No live pass: the stack is torn down and the
browser pass is deferred to the polish step (D1); the visual recipe covers the landing page. Then the
user's pause before merging.
