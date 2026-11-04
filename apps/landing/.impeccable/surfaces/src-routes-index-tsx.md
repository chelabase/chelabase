---
version: 1
slug: "src-routes-index-tsx"
primary_target: "src/routes/index.tsx"
related_targets: []
---

# Surface: public landing (`/`)

Mode: **Persuade**. The public landing page; Studio is the console it links to.

## Audience, job, action

- Developers choosing a Postgres platform they can run themselves.
- Theme: **Manage, collaborate, iterate**: Postgres made easy to manage, shared with a team through roles Postgres enforces, and changed safely with branches and merges; built in Rust, end to end.
- Job: know what Chelabase is, see what it offers against the alternatives, then **Get started** (the docs Quickstart: `${VITE_DOCS_URL}/docs/using/quickstart`, falling back to `quickstart.mdx` on GitHub) or **Visit GitHub** (`https://github.com/chelabase/chelabase`). No Studio links, no auth state.
- Proof is demonstration and sourced fact only: the hero animation, the real Docker Compose install, real `@chelabase/client` code, the service map, and a comparison drawn from each product's docs (dated, caveats in its footnote). Synthetic illustrations are labelled. No customers, logos, benchmark numbers or invented claims; platform mode is not pitched. The CLI (an installable setup and management script) and S3-compatible object storage (SeaweedFS) are **Coming soon**.

## Sections

Hero → Run it on your machine (`git clone` → `cd chelabase/infra/compose` → `cp .env.example .env` → `docker compose up -d --wait` → setup code) → Everything in one place (linked cards to each part's docs, in order: Databases, Studio, Auth, Realtime, Gateway, Orchestrator, Worker, Rest, then CLI and Object storage coming soon; tiles without descriptions below 560px) → How it compares (Chelabase · Supabase · Firebase · Neon · PlanetScale Postgres · PlanetScale MySQL, in an ink panel) → Manage it from one console → Bring your team in → Build your app on a typed API → Branch it. Reset it. Restore it. Merge it. (tide gauge with Branch, Reset, Restore and Merge) → Start with the Quickstart.

## Decisions (user, 2026-10-06)

- Round 1: abstract dusk lagoon; feature-matrix compare; code-drawn assets only; Admiralty Chart direction (first draft).
- Round 2 (after the first draft): the chart was taken too far. Chela is Latin/Greek for claw, and the crab nods to Ferris (Rust's mascot; Chelabase is built in Rust). The world becomes **crab shore**, the chart survives only as an accent (the tide gauge). No soundings or other decorative numbers.
- Mark: **C3 v1**, an original coral claw drawn as the C of Chelabase, with two large, wide-set eyes floating above it (mockup E4, no stalks) hinting at the crab it belongs to; the wordmark is that claw-C plus "helabase". Provisional until a real logo exists.
- Walking crab: an original coral crab whose **right** claw is the C3 v1 claw; small claw on the left.
- Hero motion: scuttle and fork. The crab walks main; at each fork a smaller crab splits off down its branch, leaving a footprint trail; one branch then resets (its crab walks back to the fork and out again).
- Hero must scale: lagoon, sun and palms visible on 24" (1920–2560 wide) and 34" ultrawide (3440 wide) screens, and on phones.
- Compare keeps the wireframe idea but loses the grid lines.

## Decisions (user, 2026-10-30)

- Away from "build on Neon" toward Manage, collaborate, iterate; Rust is a line in Everything in one place, not its own section.
- Section order as above (the user's order); Everything in one place lists the parts, not features the comparison already covers.
- Grounds below the hero: bands alternate raised (`--deep`) and ink, starting raised; a low swell with a sand tideline crest replaces the tall waves; no seafoam or sand bands. Basalt (`--shallows`) washed out the comparison and was dropped.
- The comparison was re-surveyed from each product's docs on 2026-10-30; PlanetScale is split because only its MySQL (Vitess) databases merge schema (deploy requests).
- Mobile polish is deferred.

## Decisions (user, 2026-11-02)

- Manage and Bring your team in open with their notes in two columns, then the Studio screenshot at column width; their illustrative SQL insets are gone (the screenshots show the real SQL and grant). Text-left, code-right stays the rule for the split sections.
- Manage drops the RLS-warning strip; the Security in plain view note carries it. Every Studio screenshot is a whole 1152×800 window (Team scrolls to its project access), retaken with `pnpm capture-studio` whenever Studio's shell changes.
- Branch it leads with one wide tide gauge that holds the definitions: tabs on top, drawing and slider on the left, the picked verb's definition and what it did on the right. The separate definition list is gone; the branches screenshot follows at full width.
- A screenshot opens larger in a dialog over the page; Esc, a close button or a click anywhere closes it. It never navigates to the raw file.
- The hero scene stays hidden until it has measured its box, so a wide screen never paints the server's guessed width first.
- Public contact is hello@chelabase.com, in the footer.
- Alpha everywhere: a tag and line under the hero actions (and "self-hosted alpha" in the footer), worded as the README, docs and Studio's top bar say it.
- Mobile polish: below a 700px column the hero tells a one-branch story (user, 2026-11-02): a single smaller crab forks off down staging, resets, then merges and burrows; smaller crabs, sun and palms, narrower trails, everything above the Replay row and on screen, the merge note shortened to "merged into main", the comparison's row names stay put while the products scroll, and the tide gauge sets its text larger.

## Direction contract

THESIS: A crab walks the shore and every branch is a trail it splits off. Branching is shown as something alive (crabs forking off main and coming back on reset), not a git graph, not a nautical chart. Refuses the dark-SaaS split hero and the feature-card grid.

OWN-WORLD: Dusk on a lagoon beach. Deep lagoon-teal sky, a sand sun low on the right, a seafoam lagoon band, a sand beach; pastel palm silhouettes at the edges. The crab and the claw-C are warm coral with pale claw tips and an ink outline. Trails are footprint dots over a wet-sand tint. Type: Spectral display, Atkinson body. Buttons are plain sand plates; chart furniture is gone except the tide gauge.

STORY: The visitor meets the claw-C wordmark and the three verbs, watches a crab fork branches off main, sees the four-command install, scans the parts and an honest sourced comparison, reads how Chelabase manages, shares and builds, plays with branch/reset/restore/merge, and gets started.

FIRST VIEWPORT: Full-width dusk scene built to the viewport width (not cropped). Top-left: the claw-C wordmark at hero scale, one headline (Manage, collaborate, iterate. On Postgres.), one line, Get started and Visit GitHub. Lower band: lagoon and beach across the full width, sun and palms at the right edge, the crab walking main with crabs forking off (two forks on phones, three on desktop, four on ultrawide), branch names in italic, a reset note and "staging's schema merged into main", an "illustration" note.

FORM: Crab shore (user-steered replacement of the Admiralty Chart build, seed key 36c8fd97 lineage). Signature interaction: the scuttle-and-fork hero, with a replay control; the tide gauge in Branching stays as the hands-on piece. Motion grammar: crabs walk with a bob, trails reveal behind them, reset retracts and redraws; reduced motion shows the finished scene.

FINISH: unreviewed and undocumented is unfinished; this build ends with the finish review, the verdict, DESIGN.md, and every shipping raster carrying its provenance
