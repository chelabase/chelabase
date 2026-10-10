---
version: 1
slug: "src-components-no-access-tsx"
primary_target: "src/components/no-access.tsx"
related_targets: ["src/routes/app.tsx"]
---

# "No access yet" screen

**Scope and mode:** Operate. `src/components/no-access.tsx`, shown inside the app shell when `/v1/me` lists no projects and the caller isn't a tenant admin (`me.member.tenant_admin === false`), **in place of the Projects section only** (Q9, user): `/app` (the dashboard) and `/app/projects/*`. Members and API reference stay reachable. `routes/app.tsx` (or the Projects routes) decides from `me`. Shape brief (Members step T12a); the world is the inherited app system (DESIGN.md). The user's answers to the T12a questions are applied.

## 1. Job and audience

- A person who signed in and can't do anything yet: someone whose sign-up a tenant admin approved without project access (R40), someone invited without a project, or a member whose last grant was taken away. (Amendment: a sign-up still waiting for approval sees the "Waiting for approval" screen instead, `src-components-waiting-for-approval-tsx.md`.)
- State of mind: unsure whether something is broken. The screen must say plainly that nothing is broken, why there's nothing here, and exactly whom to ask.

## 2. Outcome and proof

- Success: they know they're signed in (and as which account), that a tenant admin must give them access, and the names and emails of those admins; when access arrives, the screen gets out of the way without a reload.
- Truths: the API answers `403 no_access_yet` on project routes for this caller; `GET /v1/members` still works for them (`TenantMember`), so the tenant admins can be named (`tenant_admin && status === "active"`). The invariant guarantees at least one active tenant admin.

## 3. Selected direction

- Inherited world, Restrained, no illustration and no crab figure: this is a waiting room in a tool, not a marketing empty state. One headline, two short paragraphs, a list of people, one secondary action. Left-aligned in the 56rem page column, like every other page body; not a centred hero.
- Thesis: **a name to ask, not an apology.** The admins' names and emails are the content; everything else is one sentence.

## 4. Scope and boundaries

- The screen and its states. The shell stays (rail, header, account menu with Sign out).
- Replaces only the Projects section (Q9). A project URL (`/app/projects/{id}`) opened by such a caller shows this screen too, not a 403 or 404 page. The Members page works as for any member (they can see the tenant admins there as well); the API reference is unaffected.
- Anti-goals: an illustration; "Oops"; a "Request access" button that sends nothing (there's no request API); a mailto button styled as the primary action.

## 5. States and ranges

Range: 1–5 tenant admins.

- **Headline:** "You don't have access to a project yet"
- **Body:** "You're signed in as {email}. A tenant admin decides who can use which project; ask one of them to give you access." (`{email}` from `me.email`, falling back to the token's; omitted clause when both are null.)
- **The admins:** subheading "Tenant admins" (14px semibold), then a plain list: name in chalk, email in chalk-dim, each email with a CopyButton ("Copy {email}") reusing the existing `copy-button.tsx`. Email null: "Email unknown".
  - Loading: "Loading tenant admins…" in chalk-dim, the headline and body already shown.
  - Error: coral line "Couldn't load the tenant admins: {errorText}" with a Retry button; the rest stays.
  - Empty (shouldn't happen): "No tenant admin is listed. Ask whoever runs this Chelabase instance."
- **Members link:** chalk-dim "Everyone in this tenant is listed on the Members page." with "Members" as a link to `/app/members`.
- **Footer line:** chalk-dim "This page checks again every 30 seconds, and opens your projects as soon as you have one." plus a secondary (outline) button "Check now" that refetches `me`. While checking: "Checking…", disabled.
- **Access arrives:** `me.projects` becomes non-empty → the normal dashboard renders in place; a `role=status` line on it: "You now have access to {project}." (first project).
- **Removed while here:** the next `me` or members call returns 404 → "You're no longer a member of this tenant." with Sign out.
- **Platform mode:** callers who aren't members get `not_available_in_platform_mode`; this screen isn't used for them (out of scope this step; platform fails closed).
- **Tenant not bound** (`me.tenant === null`): the existing "Your organization isn't set up yet" screen wins; not this one.

## 6. Interaction and layout

- Page column 56rem, page gutter 32px; headline 1.5rem/600; body 14px at a 65ch measure.
- The rail's Projects item stays marked active (the screen stands in for the Projects section).
- Polling: `getMe` every 30 s while this screen is mounted (TanStack `refetchInterval`), paused when the tab is hidden (TanStack default). No visual countdown.
- Focus lands on the headline (`tabIndex=-1`) on first render so screen readers hear the situation first.

## 7. Constraints and open decisions

- Lagoon tokens only, dark only; `components/ui/*`; the existing CopyButton.
- Components used: Button (outline), CopyButton. Nothing new.
- Wording (Q5, user): "Tenant admin".
