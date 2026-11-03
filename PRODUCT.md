# Product

<!-- impeccable:product-schema 1 -->

## Platform

web

## Users

App developers (full-stack and product engineers) building applications on Postgres. They use the typed client (`@chelabase/client`) and the data API from their app, and use Studio while building to manage projects, databases, branches, schema, data, API keys, row-level security and their app's users.

Self-hosted mode puts every signed-up user in one shared org/tenant/project, a shared team workspace by design. Other audiences (self-hosting operators, platform-mode customers) are not confirmed as primary.

## Product Purpose

Chelabase is a database-as-a-service control plane on Neon. Each database gets its own tenant, timeline and compute, plus branches, login roles, encrypted re-revealable credentials, a data API (`anon` / `authenticated` roles, RLS) and app-user auth. Studio is the only user-facing app.

Success means a developer goes from sign-up to a working, secured database their app queries through the typed client, without reaching for a second tool.

## Positioning

Secure-by-default developer experience. The data API is a typed JSON query endpoint (`POST /data/v1/query`) with a builder client and generated types, not a URL grammar. Credentials are encrypted at rest, secrets never enter client caches, and RLS gaps are surfaced, not hidden. The safe path is the easy path, and Studio makes the security state of a database visible.

Chelabase's designs are its own. It never mirrors Supabase (PostgREST URL grammar, postgrest-js/supabase-js compatibility, GoTrue-style auth). Supabase and Neon are references for pitfalls only.

## Operating Context

- Modes: `self-hosted` (the default, and what the Docker Compose install runs; email verification off, a shared workspace, and the first account to sign up with the one-time setup code claims the instance as its admin) and `platform` (the hosted cloud, not built yet; behaviour decided separately, e.g. verification required, no account-existence leaks).
- Studio screens: sign-in/register/verify/password reset (the identity service, member tokens), the projects dashboard, project view with databases, the branch tree, login roles, the Tables console (grid, filters, row editor), the SQL editor (run as owner or a chosen role, cancel, server-side guard, saved queries, history), Data API panels (keys, per-database access, RLS warning), Users (app users and sessions), settings, and the API reference.
- Transitional resources (creating, deleting) poll every 2s until settled.
- All traffic goes through the gateway on :8000. In development Studio runs in the workbench stack, behind its gateway (`just workbench up`).

## Capabilities and Constraints

- Studio stack: React 19, Vite, TanStack Router/Query, Tailwind 4, shadcn-style components on Base UI, lucide icons, CodeMirror with the Postgres language server.
- Secrets must never enter the query/mutation cache or any persisted client state.
- Terminology: project, database, branch, login role, owner (`postgres`), `anon`, `authenticated`, data API, app users.
- Undecided: platform-mode product scope, pricing, i18n (Studio is English-only today).

## Brand Commitments

- Name: **Chelabase** (display casing fixed). The Studio title is "Chelabase Studio". Renamed from Cheladb on 2026-10-04.
- Voice: plain and terse, like the repo's docs.
- No logo, favicon or brand marks exist yet. The Spectral "Chelabase" wordmark on the landing page is provisional.
- Default theme: **dark**. Light mode may come later; dark is the default experience (user, 2026-10-06).
- Visual world: tropical coastal (crabs, beach, palm trees) rendered as an abstract dusk lagoon, not clip-art. Colour is pastel roles (sand peach, seafoam, coral blush, palm green) on a deep lagoon-teal dark. Never neon glow, purple SaaS, or cream-and-terracotta editorial. The landing page draws this world as a nautical chart; later Studio work inherits the palette and dark default.

## Evidence on Hand

Pre-launch, private repo. No customers, testimonials, case studies or published benchmarks exist. A data-API benchmark is planned but has no results. Future work must not invent any of these.

## Product Principles

1. Safe by default: the secure option is the default and the easiest one. Risky states (missing RLS, exposed keys) are visible.
2. Developer flow first: building, querying and inspecting happen in one place with minimal context switching.
3. Postgres stays legible: branches, roles and policies are shown as what they are, not hidden behind abstractions.
4. Our own design: solve the problem directly instead of copying a neighbour's interface or API shape.

## Accessibility & Inclusion

- WCAG 2.2 AA is the required standard for Studio: contrast, full keyboard paths, and screen-reader names and states.
- Dark is the default theme across Studio (since the app shell, 2026-10-06); light mode may come later.
- English-only; i18n is undecided.
