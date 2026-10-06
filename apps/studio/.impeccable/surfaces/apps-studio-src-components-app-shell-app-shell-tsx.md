---
version: 1
slug: "apps-studio-src-components-app-shell-app-shell-tsx"
primary_target: "apps/studio/src/components/app-shell/app-shell.tsx"
related_targets: ["apps/studio/src/components/app-shell/app-rail.tsx","apps/studio/src/components/app-shell/app-header.tsx","apps/studio/src/routes/app.tsx"]
---

# Signed-in app shell

**Scope and mode:** Operate. The chrome around every `/app/*` route in Studio: a left icon rail, a top header and a main region. Skeleton only. The nav items are stubs (Projects, API reference, two placeholders), and the final section IA is a later pass.

**Audience and job:** app developers signed in to Studio while they build. They move between projects and the API reference, then work in the page body for long stretches. The shell must get out of the way, keep wayfinding obvious, and survive page changes without flicker.

**Constraints:**
- Inherit the landing's lagoon world, quieter: dark default, pastel roles with one meaning each, Atkinson for UI and Spectral only in the wordmark.
- Never Supabase's look, menu tree or wording; topology only.
- WCAG 2.2 AA. Desktop first; mobile deferred.
- Three rail behaviours (Expanded, Collapsed, Expand on hover; default hover), persisted.

**Unresolved (later passes):** the real sections and their order; a project/branch switcher in the header's context slot; mobile; lagoon "plates" for buttons and dialogs.

## Direction contract

THESIS: A tool frame that recedes into lagoon water. One deep-water rail and an ink header, separated only by hairlines; the brand speaks once, as the claw wordmark and a sand "you are here" mark. It refuses the category default of a grey or green SaaS sidebar: no glowing active pills, no tinted cards, no icon tiles.

OWN-WORLD: The ground is lagoon ink `#0b1c1f`; the rail is deep water `#0f2a2d`. Text is chalk `#e6ede7` and chalk-dim `#aebfc0`. Hairlines are chalk at 16% and 34%. Sand `#f2c7a3` is the only action and selection colour, coral `#eda6aa` means hazard, and seafoam means yes. Icons are lucide at 18px with a 1.5 stroke. Corners are square-cut. A dashed outline means not built yet. There are no shadows.

STORY: The developer lands on `/app` and sees where they are at once: a sand edge-mark beside Projects. Labels come to them when the pointer or focus reaches the rail, and leave when it goes. They can pin the rail open or shut from its foot. Their account and Sign out sit top right. The page body keeps its place and its content.

FIRST VIEWPORT: Left, a 56px deep-water rail, full height. At its top is the claw mark; below it, the icon items, with a 2px sand bar on the active one; at its foot, the Sidebar behaviour button. On hover or focus it widens to 232px over the page, showing the Spectral "Chelabase" wordmark and Atkinson labels. Top, a 52px ink header with a hairline below it: an empty context slot on the left, and the account trigger (name, CircleUser) on the right. The rest of the viewport is the page body: the dashboard in its existing 56rem column. Primary actions remain the page's own (New project, as a sand primary button).

FORM: An inherited world with a user-pinned topology (a left icon rail, a top header, a main slot). No concept roll ran: the user pinned both the world and the structure, so this is position 1 of 1, with no seed key.

FINISH: unreviewed and undocumented is unfinished; this build ends with the finish review, the verdict, DESIGN.md, and every shipping raster carrying its provenance
