---
version: 1
slug: "src-routes-index-tsx"
primary_target: "src/routes/index.tsx"
related_targets: []
---

# Surface: public landing (`/`)

Mode: **Persuade**. The public landing page; Studio is the console it links to.

## Audience, job, action

- Developers weighing Supabase, Firebase or plain Neon for a Postgres platform they can run themselves.
- Job: in seconds, know what Chelabase is and remember branching as its mechanism; then **Start free** (links to `${studioUrl}/?signup`) or **Log in** (links to `${studioUrl}/`). This page has no auth state; the Studio URL comes from `VITE_STUDIO_URL` at build time.
- Proof is demonstration only: the branching animation, a real `@chelabase/client` snippet, the real feature map, the README's real setup path. Synthetic illustrations are labelled. No customers, logos, numbers or compatibility claims; platform mode is not pitched; object storage is **Coming soon (SeaweedFS)**.

## Sections

Hero → What it is → From clone to a query (the README path: `git clone` → `proto install` → `pnpm install` → `just dev`) → Branching → Product pieces → Compare (wireframe, labelled preview, no scores, no grid lines; columns Chelabase · Supabase · Firebase · Vanilla Neon) → Final CTA. Docs: no deployed URL; mention `just docs` as text.

## Decisions (user, 2026-10-06)

- Round 1: abstract dusk lagoon; feature-matrix compare; code-drawn assets only; Admiralty Chart direction (first draft).
- Round 2 (after the first draft): the chart was taken too far. Chela is Latin/Greek for claw, and the crab nods to Ferris (Rust's mascot; Chelabase is built in Rust). The world becomes **crab shore**, the chart survives only as an accent (the tide gauge). No soundings or other decorative numbers.
- Mark: **C3 v1**, an original coral claw drawn as the C of Chelabase, with two large, wide-set eyes floating above it (mockup E4, no stalks) hinting at the crab it belongs to; the wordmark is that claw-C plus "helabase". Provisional until a real logo exists.
- Walking crab: an original coral crab whose **right** claw is the C3 v1 claw; small claw on the left.
- Hero motion: scuttle and fork. The crab walks main; at each fork a smaller crab splits off down its branch, leaving a footprint trail; one branch then resets (its crab walks back to the fork and out again).
- Hero must scale: lagoon, sun and palms visible on 24" (1920–2560 wide) and 34" ultrawide (3440 wide) screens, and on phones.
- Compare keeps the wireframe idea but loses the grid lines.

## Direction contract

THESIS: A crab walks the shore and every branch is a trail it splits off. Branching is shown as something alive (crabs forking off main and coming back on reset), not a git graph, not a nautical chart. Refuses the dark-SaaS split hero and the feature-card grid.

OWN-WORLD: Dusk on a lagoon beach. Deep lagoon-teal sky, a sand sun low on the right, a seafoam lagoon band, a sand beach; pastel palm silhouettes at the edges. The crab and the claw-C are warm coral with pale claw tips and an ink outline. Trails are footprint dots over a wet-sand tint. Type: Spectral display, Atkinson body. Buttons are plain sand plates; chart furniture is gone except the tide gauge.

STORY: The visitor meets the claw-C wordmark, watches a crab fork branches off main, reads what Chelabase is, sees the real clone-to-query path, plays with branch/reset/restore, scans the pieces and an honest preview comparison, and starts free.

FIRST VIEWPORT: Full-width dusk scene built to the viewport width (not cropped). Top-left: the claw-C wordmark at hero scale, one headline, one line, Start free and Log in. Lower band: lagoon and beach across the full width, sun and palms at the right edge, the crab walking main with crabs forking off (two forks on phones, three on desktop, four on ultrawide), branch names in italic, an "illustration" note.

FORM: Crab shore (user-steered replacement of the Admiralty Chart build, seed key 36c8fd97 lineage). Signature interaction: the scuttle-and-fork hero, with a replay control; the tide gauge in Branching stays as the hands-on piece. Motion grammar: crabs walk with a bob, trails reveal behind them, reset retracts and redraws; reduced motion shows the finished scene.

FINISH: unreviewed and undocumented is unfinished; this build ends with the finish review, the verdict, DESIGN.md, and every shipping raster carrying its provenance
