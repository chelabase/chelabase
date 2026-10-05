# Step 6f, docs site: design

**Status:** design approved in conversation 2026-10-04/05, part by part (5 parts); written spec approved by the user 2026-10-05.
**Builds on:** `main` at `e9def2c` (step 6e merged).
**Branch:** `docs-site` (`.worktrees/docs-site`), from `main` at `e9def2c`.

## Goal

`README.md` (756 lines) mixes contributor setup, user-facing feature docs and internals, and buries how to clone and run the project. This step moves that content into a documentation app, `apps/docs`, organized by reader, checks each claim against the code as it moves, and leaves a short README, a CONTRIBUTING.md, and generated API reference that can't drift. It also removes the second-computer material from the docs, the roadmap and CLAUDE.md.

## Decisions (user, 2026-10-04/05)

| # | Question | Answer |
|---|---|---|
| D1 | Audience | **One site, split sections:** "Using Chelabase" for app developers, "Architecture" for contributors, plus generated "Reference". |
| D2 | Hosting | **Local only for now:** `just docs` on **:3001**, a static build checked by moon. Public hosting is decided at launch. |
| D3 | Process docs | Specs, plans, notes, benchmarks, security notes and the roadmap **stay where they are**. The site gets curated Architecture pages that link back to them. |
| D4 | Generated reference in v1 | **Management API + typed client samples + the data API.** |
| D5 | Data API reference | **Add utoipa to `chelabase-query`/`rest`** and emit `openapi/data.json` through `just api-gen`, with the same staleness check as the other documents. |
| D6 | README | **Short entry point + CONTRIBUTING.md.** |
| D7 | Contributing split | **Split by topic:** CONTRIBUTING.md holds the workflow; the site's Architecture section holds internals and links to CONTRIBUTING for workflow. |
| D8 | Second-computer material | **Removed** from the docs, the roadmap's "Resume here" and CLAUDE.md's framing. The rules themselves (fetch first, push by default) stay. |
| D9 | Accuracy | **Verify against the code while migrating.** Each page's claims (recipes, env vars, ports, defaults, error codes) are checked; history markers ("step 3", "was ui-auth") are dropped. |
| D10 | Client README | The SDK guide **moves to the site**; `packages/client/README.md` keeps a stub. |
| D11 | PRODUCT.md | **Committed unchanged** at the repo root. |
| D12 | Roadmap slot | **Numbered step 6f.** |
| D13 | Tool | **Fumadocs on TanStack Start** (over Fumadocs on Next.js 16 and Docusaurus 3.10). |

**Rulings (mechanical, listed to the user):**
- R1. The Login API (`openapi/login.json`) is Studio's internal API, so its generated pages go under **Architecture → Login API (internal)**, not Reference.
- R2. README's "Still out of scope" list is stale (data API keys and delete/reconcile exist now). It is dropped; what is still true goes into the matching page's known limits, notably: "The Management API takes only a Zitadel user token; there are no API keys for it yet."
- R3. The roadmap's Working rules say "Push to the remote … only when asked", which contradicts CLAUDE.md's "push by default". It is aligned to CLAUDE.md (user confirmed with part 4).
- R4. No orval client is generated from `data.json`; `@chelabase/client` stays the hand-written builder.

## Part 1: app scaffold and tooling

- **Location and name:** `apps/docs`, package `@chelabase/docs`, picked up by the existing `apps/*` pnpm glob.
- **Base:** the official `create-fumadocs-app` **`tanstack-start`** template (16.2.x). Versions are the latest that pass pnpm's minimum-release-age check at implementation time; as of 2026-10-04: `fumadocs-core`/`fumadocs-ui`/`fumadocs-mdx` 16.16, `fumadocs-openapi` 12.2, `fumadocs-twoslash` 4.0, TanStack Start, React 19.2, Tailwind 4, and Vite on the same major as Studio.
- **Static output:** every page is prerendered at build time; there is no runtime server. Search is Fumadocs' built-in static Orama index.
- **Content:** MDX in `apps/docs/content/docs/`, sidebar order in `meta.json` files. Fumadocs' `llms.txt`, `llms-full.txt` and per-page `.md` routes stay on, so agents can read the docs as plain markdown.
- **Commands:**
  - `just docs` runs `pnpm --filter @chelabase/docs dev` on **:3001** with a strict port (fails rather than moving if :3001 is taken).
  - moon tasks `dev`, `build`, `typecheck`, matching `apps/studio/moon.yml`. `build` fails on broken internal links (Fumadocs link validation).
- **Styling:** Fumadocs UI's default theme with light and dark mode, "Chelabase" in the nav, no logo (none exists). Sharing components with Studio is out of scope.
- **No stack needed:** the docs build reads only MDX and the committed `openapi/*.json`, so it runs in any worktree without `just live`.

## Part 2: site map

**Using Chelabase** (app developers)

| Page | Source |
|---|---|
| Introduction: what Chelabase is, modes, principles | PRODUCT.md facts, README intro |
| Quickstart: sign up in Studio → create a database → query from the client | "Studio sign-in and data fetching" (user steps), "Create-database smoke test" |
| Projects and databases: lifecycle, connection details, settings, caps | "Projects", "Database lifecycle" |
| Branches: create, point-in-time, reset, restore, protected branches, the branch password rule | "Branches", protected/password-rule parts of "Roles and migrations" |
| Login roles and migrations: what works in SQL, password rules, caps, control plane down | "Roles and migrations" |
| Data API: Overview, Keys, Serving more databases, Allow-lists, Query language, Settings, Errors, Known limits | "Data API" (user-facing subsections) |
| TypeScript client: install, queries, type generation, big numbers, other databases, auth | "SDK and type generation", `packages/client/README.md` |
| App users: Enabling, the users table, claim helpers, tokens and sessions, settings | "App users" |
| SQL editor: execution, run as, guard rules, saved queries and history | "SQL editor" |
| Studio tour: screens, the Tables console, the API page | "Studio's console", PRODUCT.md screens |

**Reference** (generated, part 3): Management API, Data API, Error codes.

**Architecture** (contributors)

| Page | Source |
|---|---|
| Overview: services, ports, repo layout, the request path through the gateway | "Repository layout", "Auth" (gateway), "Local dependencies" table |
| Gateway and platform auth: internal identity tokens, env vars, forwarding headers | "Auth" |
| Studio internals: Zitadel sign-in flow, data fetching and secrets rules, bootstrap | "Studio sign-in and data fetching" ("How it's wired") |
| Data API internals: request flow, connection model, schema model, pipelining (links the benchmark) | "Data API": "Request flow", "Schema model" |
| App users internals: sync, webhooks, reconcile | "App users": "Sync, webhooks and reconcile" |
| SQL editor internals: guard implementation, audit, dependencies | "SQL editor": "The guard", "Audit", "Dependencies" |
| Neon: storage stack, proxy, computes, Valkey, internal listener, native-Linux Docker note | "Neon" |
| API generation: utoipa → OpenAPI → orval clients, Scalar pinning | "Management API" (generation part) |
| Login API (internal) | generated from `openapi/login.json` (R1) |
| Security notes | links to `docs/security/` |
| Design history | an index of `docs/superpowers/specs`, grouped by area |

**CONTRIBUTING.md:** prerequisites, machine setup (sccache, cargo-sweep), the `just` recipe table, builds and worktrees, the seed tenant, local dependencies, worktrees and the live queue, every "Testing" subsection gathered into one "Tests and e2e runs" section, commit identity and branch rules. No second-computer material (D8).

**Dropped:** "Still out of scope" (R2).

## Part 3: generated reference

**Management API**
- Rendered from the committed `openapi/management.json` with `fumadocs-openapi`, one page per operation (request and response schemas, error responses, a code sample).
- Pages are generated **at build time, not committed**, so they can't drift. If the plan's pre-flight scan finds that `fumadocs-openapi` 12 can't do this, the fallback is a generation script that `just api-gen` runs and `just check` verifies, like the orval clients.
- The interactive "try it" panel is off on the site; Studio's `/app/api` (Scalar) stays the place to try requests.
- The Login API gets the same rendering under Architecture (R1).

**Data API (`openapi/data.json`, new)**
- **`chelabase-query`:** a new `openapi` cargo feature, following `chelabase-types` (`cfg_attr(feature = "openapi", derive(utoipa::ToSchema))`). Types with derived serde derive `ToSchema`; the custom-parsed types get hand-written schema impls: `Filter`, `SelectItem`, `Returning`, `Path`, `Op`, `CondOp`, and `Name` (a string with its pattern). The schema response types in `schema.rs` are covered too.
- **`services/rest`:** `#[utoipa::path]` on the two public routes, `POST /data/v1/query` and `GET /data/v1/schema`: the key as a bearer security scheme, the optional `Chelabase-User` header, the `?database=` parameter, and the error responses from the data API error table. Internal routes stay out of the document.
- **Generation:** a `document_is_current` test in `rest` like orchestrator's and auth's; `just api-gen` adds `-p rest`; `just check` runs `rest`'s openapi tests and includes `openapi/data.json` in its staleness check.
- **Fidelity test:** a new `chelabase-query` test validates every shared fixture in `packages/client/fixtures/*.json` against `data.json`'s request schema (the `jsonschema` crate, 0.58 as of 2026-10-04, as a dev-dependency), plus negative cases (an unknown field, a bad operator, an empty `values`, …) that both the parser and the schema must reject. A disagreement in either direction fails the test.
- **Pages:** the same rendering under Reference → Data API; the hand-written "Query language" guide links to them.

**TypeScript client samples**
- Code blocks marked `ts twoslash` are type-checked during `moon run docs:build` against the workspace's `@chelabase/client`, with hover types on the page. A sample that stops compiling fails the build.
- Samples import a shared `Database` type taken from the client's golden generated types (`packages/client/fixtures/gen`).
- Samples that show an intended compile error use twoslash's expected-error annotation.

**Error codes page:** hand-written: the shared management/login/gateway shape and codes, plus the data API table. Generating it from `ErrorCode` is a possible follow-up.

## Part 4: README, CONTRIBUTING, client README, second-computer cleanup

**README.md** (about 60–100 lines):
1. **Chelabase:** a one-paragraph pitch, Apache-2.0.
2. **Repository layout:** the current tree, corrected.
3. **Prerequisites:** proto, Docker, Rust, cmake, clang, protoc.
4. **Quickstart:** clone → `proto install` → `pnpm install` → the `just` recipes that bring up the stack → open `http://localhost:3000` and create an account; Mailpit on :8025. The exact recipes are checked against the justfile (`dev` already depends on `deps-up`) and run verbatim during verification.
5. **Documentation:** `just docs` → `http://localhost:3001`, links to the three site sections, CONTRIBUTING.md and PRODUCT.md.

**CONTRIBUTING.md:** as listed in part 2. For a fresh clone, the one remote-related step is generic: a branch that exists only on origin needs `git fetch && git branch <b> origin/<b>` before `just wt-add <b>`.

**`packages/client/README.md`:** install, a short example, a link to the site's TypeScript client pages.

**Second-computer cleanup (D8)**
- **README:** "Second computer setup" and every per-feature "Second computer:" note are dropped, not moved.
- **Roadmap "Resume here":** remove the product-rename "Second computer" runbook (steps 1–8), every inline "**Second computer:** …" sentence (6e, rename, org rename, repo move, 6d, 6b, storage leak, live queue) and the "Step 6 on another computer" block. Entries keep their status, commits and links. The removed text stays in git history; the commit message names the last commit that has it. "See README for the quick reference" points to CONTRIBUTING.md.
- **CLAUDE.md:**
  - "The user works across two computers. These rules carry over between machines; local memory doesn't." → "Local memory isn't shared between sessions or machines; these rules are the durable source."
  - The session-start `git fetch` stays, worded as checking origin for newer commits.
  - "On a fresh clone or another computer" → "On a fresh clone".
  - "Push by default … because the user works across two computers" → "Push by default; origin is the source of truth."
  - Every rule stays; only the two-machine framing goes.
- **Roadmap Working rules:** the push rule is aligned to CLAUDE.md (R3).

## Part 5: execution

Roadmap step **6f**, branch `docs-site`; parallel tasks run in `docs-site-tN` worktrees (`just wt-add docs-site-tN docs-site`), merged back with `--no-ff`.

| Task | What | Implementer | Reviewer |
|---|---|---|---|
| T1 | Scaffold `apps/docs` (part 1), twoslash wired with one sample, `fumadocs-openapi` rendering `management.json`, and the **full sidebar skeleton** (every `meta.json` and a stub page per part-2 entry) so later tasks only fill files they own | sonnet | sonnet |
| T2 | Data API OpenAPI (part 3): `chelabase-query` feature and schemas, `rest` paths, `document_is_current`, fidelity test, `just api-gen`/`just check`, committed `openapi/data.json` | opus | opus |
| T3a | Using: Introduction, Quickstart, Projects and databases, Branches, Login roles and migrations | sonnet | end-of-step |
| T3b | Using: Data API pages, TypeScript client (twoslash), client README stub | sonnet | end-of-step |
| T3c | Using: App users, SQL editor, Studio tour | sonnet | end-of-step |
| T3d | Architecture pages and the design-history index | sonnet | end-of-step |
| T3e | CONTRIBUTING.md | sonnet | end-of-step |
| T4 | Reference: Data API and Login (internal) pages, Error codes page | sonnet | end-of-step |
| T5 | README rewrite, second-computer cleanup (roadmap, CLAUDE.md), the push-rule fix, commit PRODUCT.md | sonnet | end-of-step |
| End | Whole-branch review with an accuracy audit (each page's claims spot-checked against the code) | opus | n/a |

Every content task verifies claims against the code as it writes (D9) and lists its corrections in its report.

**Waves**
- Wave 1: T1 ∥ T2 (disjoint: `apps/docs` vs the Rust crates, `justfile` and `openapi/`).
- Wave 2 (after T1): T3a ∥ T3b ∥ T3c ∥ T3d ∥ T3e, each writing only its own content folder or file; T4 joins once T2 is merged.
- Wave 3 (after wave 2): T5, last because it removes README content the wave-2 tasks read from, and it edits "Resume here", which checkpoints also touch.
- End: review → one fix batch → scoped re-review → verification → merge pause.

**Strict TDD**
- T1: RED is a vitest over the build output asserting that `/docs`, a Management reference page and `llms.txt` are prerendered, and that a deliberately broken link fails the build.
- T2: RED is `document_is_current` and the fidelity test failing.
- Content tasks: RED/GREEN is `moon run docs:build` (link validation and twoslash) failing on the stub's placeholder check, then passing.

**Verification (end of step)**
- `moon run docs:build`, `moon run docs:typecheck`.
- `cargo test -p chelabase-query -p rest -p orchestrator -p auth`; clippy on the changed crates.
- `just check`.
- One `just live` pass: the README quickstart run verbatim from a clean `just dev`, Studio opened; a browser pass on `just docs` (search, a twoslash hover, a Data API reference page, dark mode).

## Out of scope

Public hosting, a docs logo or brand marks, sharing components with Studio, versioned docs, generated error-code lists, moving or rewriting specs and plans, and Management API keys.
