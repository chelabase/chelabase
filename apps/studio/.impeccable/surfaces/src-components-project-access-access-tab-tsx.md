---
version: 1
slug: "src-components-project-access-access-tab-tsx"
primary_target: "src/components/project-access/access-tab.tsx"
related_targets: ["src/components/project-page.tsx","src/lib/project-search.ts"]
---

# Project Access tab

**Scope and mode:** Operate. A new "Access" tab on the project page (`project-page.tsx`: `ProjectTab` gains `"access"`; content in `src/components/project-access/*`; tab in `lib/project-search.ts`). Who can use this project, as what platform role and which Postgres roles, narrowed per Chelabase database; adding people; the project's access log. Shape brief (Members step T12a); the world is the inherited app system (DESIGN.md). The user's answers to the T12a questions are applied (Q3, Q7, Q8 here).

## 1. Job and audience

- **Everyone with access to the project** sees the tab (Q8, user). Below project Admin it shows only "Your access", read from `/v1/me`; nothing here calls a `Project(Admin)` route for them.
- **Project Admins and tenant admins** see the full tab (the API is `Project(Admin)` for every grant, invite and log call). They come with a concrete intent: "billing should edit the `billing` schema and nothing else", "give the new contractor read access", "why can Ana write to orders?".
- They know Postgres roles; they may not know Chelabase's rules (R16, R21, R22). The tab teaches those rules at the moment they bite, in the picker, not in a help page.

## 2. Outcome and proof

- Success: a member's access is set in one place, in one save, and reads back exactly as Postgres will enforce it: the platform role, the roles with their chains expanded, and every per-database narrowing visible without opening anything.
- Truths the tab must carry (T7's API and the Status rulings):
  - **R7:** Admin includes the database owner. There is no "owner" role entry anywhere; Admin is labelled "includes the database owner".
  - **R16 / R22 / R23:** below Admin, a grant may name only NOLOGIN group roles whose chain reaches no owner-level role (owner, `neon_superuser`, `cloud_admin`, SUPERUSER, CREATEROLE, CREATEDB, REPLICATION, BYPASSRLS), no LOGIN role and no reserved role, on any branch. Such roles are Admin-only.
  - **R21:** a granted role includes its chain: granting `billing_admin` (which can `SET ROLE billing`) gives both. The UI shows the expanded set.
  - **R30:** a chain holding ADMIN on another role can't be seen at grant time; the reconciler leaves it out. The UI has no signal for this (see section 7).
  - Overrides only narrow: a role at most the grant's, roles a subset of the grant's. Narrowing below Admin takes the owner away on that database.
  - Tenant admins are Admin with every role on every project, without a grant row.
  - An invited member's grant counts once they accept.
  - Deleting a grant keeps that member's overrides (they narrow a later grant).
  - 422 `invalid_request` names the refused role in `extra.role`, its path in `extra.chain`, and the database in `extra.database_id`.

## 3. Selected direction

- Inherited world, Restrained. Role names and chains in JetBrains Mono (they are code); everything else Atkinson. Sand for the one primary action ("Add people"), Save buttons and focus; coral for "Admin only" refusals, errors and "Take access away"; seafoam unused.
- Structural thesis: **the grant reads like the rule Postgres will apply.** One row per person: `Developer · billing_admin (+ billing)` and, beside it, "Narrowed on orders". Editing opens the row in place (the project page's existing expanding-row pattern), so the person, their current access and the editor stay on one line of sight. No permission matrix, no checkbox grid of every role × person.

## 4. Scope and boundaries

- Production tab with the grants table, the inline editor (grant + per-database narrowing), the "Add people" dialog and the project's access log.
- Untouched: other tabs; the Members page (tenant admin flags are changed there, not here).
- Anti-goals: a free-text role field; an "owner" option; offering a widening choice in a narrowing; hiding why a role can't be granted.

## 5. States and ranges

Range: 1–30 people per project; 0–10 roles in the picker per project, rarely 50; 1–5 databases per project.

### Tab visibility

- The tab is always in the strip (Q8, user). The full content below is for callers who are Admin on the project (`me.projects[].platform_role === "admin"`) or tenant admins.
- **Below Admin (Viewer, Developer): "Your access" only.** Section heading "Your access", then one line built from `me.projects[]` for this project: "You're a {Role} on {project}." and the roles as mono chips with their chains ("You may act as `billing_admin` (+ billing)."), or "You can't act as any Postgres role here." when the list is empty. Then the role's description line (the same text as the editor's), and chalk-dim: "Project Admins and tenant admins decide who has access. Ask one of them to change yours." No table, no log, no actions. (`/v1/me` doesn't carry per-database narrowing, so this view doesn't claim any.)
- Admin view also starts with its own "Your access" line ("You're an Admin on {project}, including the database owner." or "You're a tenant admin: Admin on every project.") above "People with access".

### Header

- Section heading "People with access" (16px semibold), a chalk-dim line "Each person gets a platform role on this project and the Postgres roles they may act as.", and on the right the primary "Add people".

### Table

Columns: Person · Platform role · Postgres roles · Per database · (expand control).

- **Tenant admin rows** first, read-only: Person with a "Tenant admin" label; Platform role "Admin"; Postgres roles "Every role, including the database owner"; Per database "—"; no expand control. A chalk-dim note under these rows: "Tenant admins are Admin on every project. Change that on the Members page." (a link).
- **Granted rows:** Person (name, email under it; "Invited" label for an invited member, with the hint "Counts once they accept" on hover/focus); Platform role ("Viewer", "Developer", or "Admin" with "includes the database owner" in chalk-dim beneath); Postgres roles as mono chips, a chain shown inline as `billing_admin` then chalk-dim "+ billing" (R21); "No roles" in chalk-dim when empty; Per database: one compact chip per override, "orders: Viewer", "orders: billing only", "orders: Viewer, billing only"; "Same everywhere" in chalk-dim when there are none.
- **Expand control:** a button "Edit access for {name}" (lucide `ChevronDown`, rotates), `aria-expanded`. One row open at a time.
- **Loading:** "Loading access…". **Error:** coral "Couldn't load who has access: {errorText}" + Retry. **403 `forbidden`** (the caller lost Admin meanwhile): "Only project Admins can see who has access to {project}." and the tab falls back to "Your access" on the next `me` refresh.
- **Only tenant admins:** the tenant admin rows, then "No one else has access yet. Add people to give them a role on this project." with an inline "Add people" button.
- **Project being deleted:** the whole tab sits in the existing disabled fieldset; the editor and dialog can't open.

### Inline editor (an expanded row)

Two parts, separated by a hairline:

1. **Project access**
   - Platform role: three radio options laid out as a segmented control, each with its line:
     - Viewer: "Reads tables and structure as the roles below. No SQL editor, no writes, no connection strings or keys."
     - Developer: "Runs SQL and edits data as the roles below, shares the project's saved queries, manages app users and unprotected branches."
     - Admin: "Everything in the project, including the database owner."
   - Postgres roles: the **RolePicker** (below). With Admin chosen, a chalk-dim line above the picker (Q7, user: Admin acts as the owner plus only the listed roles): "Admin includes the database owner; list the roles they should also act as."
   - Buttons: "Save" (primary; disabled until something changed), "Cancel", and on the far right "Take access away" (destructive ghost).
   - Saved: the row collapses and shows the new values; a `role=status` line "Saved {name}'s access." under the header.
   - **Take access away:** confirm dialog. Title "Take {name}'s access to {project} away?" Body "They can't open this project or act as its roles any more; their open sessions as those roles end. Per-database narrowings are kept and apply if they get access again." Button "Take access away".
2. **Per database** (shown under a saved grant, whatever its role)
   - One line per database of the project: database name, then either "Same as the project" + a "Narrow" button, or the narrowing in words + "Change" and "Stop narrowing".
   - Narrowing form (inline in that line): Platform role offering only the grant's role and lower (higher ones are absent, not disabled), with "Same as the project" as the first choice; Roles as checkboxes over the grant's roles (each with its chain), plus "All of the project's roles" as the first choice. When the chosen role is below Admin and the grant is Admin, a chalk-dim note: "Below Admin, {name} can't act as the database owner on {database}."
   - "Save narrowing" is disabled until the choice is narrower than the grant (the API refuses "narrows nothing" with 422).
   - Errors: 422 "would widen" or "no grant" shouldn't happen from this UI; if they do, show the server message under the line and refresh.

### RolePicker (shared with both invite dialogs)

- A multi-select combobox: a text field that filters, chips for the chosen roles, and a listbox popup (deep water, strong hairline, square, like menus).
- **Source (Q3, user):** each ready database's main branch (`listBranches`, then `listRoles` on the main branch), merged by role name. A role that exists on only some of the project's databases carries a chalk-dim note on its option and chip: "only on orders" (or "only on orders, events"). Eligibility merges too: a role that is Admin-only on any database is Admin-only. A database that isn't ready, or whose roles fail to load, is left out with a line in the popup: "Roles from {database} aren't shown: {reason}."
- **Never listed:** the owner (any `origin: "owner"` role), reserved names. Admin covers the owner, so it's never a choice.
- **Eligibility hint, computed from `listRoles` (the API decides):** below Admin, an option is disabled, grouped under "Admin only", with its reason: "LOGIN role", "Owner-level", "Bypasses RLS", or "Reaches {role}, a LOGIN role" / "Reaches {role}, which is owner-level" through the `member_of` closure. With Admin chosen, every listed role is selectable.
- **Chain:** each option with a chain reads `billing_admin` and chalk-dim "also gives billing"; the chosen chip shows "+ billing".
- **Changing the platform role below Admin** with Admin-only roles chosen: those chips turn coral-outlined with their reason, and Save is disabled with the line "Remove the Admin-only roles, or keep Admin."
- **Empty:** "No roles to grant yet. Create a NOLOGIN group role on a branch (Branches → Roles), or in SQL with CREATE ROLE … NOLOGIN." **Loading:** "Loading roles…" in the popup. **Error:** "Couldn't load roles: {errorText}" with Retry in the popup; Save stays possible for already-chosen roles.
- **Server refusal (422 with `extra.role`):** the named chip turns coral-outlined; under the picker, the server's sentence, then the chain in mono (`billing_admin → billing → app_login`) and "on {database}" from `extra.database_id`.
- Keyboard: arrow keys move in the list, Enter toggles, Backspace in an empty field removes the last chip, Escape closes the popup; chips have a "Remove {role}" button each. `aria-multiselectable`, disabled options announced with their reason.

### Add people dialog

- Title "Add people to {project}". Fields:
  - "Email": a combobox that suggests tenant members without access here (from `listMembers`; name and email), and accepts any other email. Helper: "Someone new gets an email invite."
  - Platform role (as in the editor, Developer preselected) and Postgres roles (RolePicker).
- "Add" (primary), "Cancel". Sends `createProjectInvite`, which handles existing members and new people alike.
- Results (dialog closes, status line under the header): `existing_member`: "{name} now has {Role} access to {project}."; `added`: "{email} already had an account and was added with {Role} access. No email was sent."; `invited`: "Invited {email} with {Role} access. The link in their email works for 72 hours; access counts once they accept."
- Errors: as the Members invite dialog (422 email, 422 role with chain, 409 conflict, 409 `project_deleting`, 503 unavailable), shown in the dialog.
- Amendment (R35): the same delivery choice as the Members invite dialog (`GET /v1/invites/delivery`; email by default when available, else link only with the SMTP note), and the same shown-once `members/InviteLinkPanel` for an `invite_url`, built by T12b. The 409 "send by email" answer reads as in the Members brief.

### Access changes (the project's log)

- Below the table, heading "Access changes". The Members Activity list component, fed by `listProjectAccessEvents`, the same sentences (without "on {project}"), paging with "Show older" via `next_before`.
- Empty: "No access changes on this project yet."

## 6. Interaction and layout

- Lives in the project page's 56rem column. The tab strip gains "Access" after "Users" and before "Settings".
- The table rows are 36px+ (two-line person cell); chips wrap within their cell.
- Focus: after Save, focus returns to the row's expand control; after the dialog, to "Add people". Every control has the 2px square sand outline.
- Status lines `role=status`, errors `role=alert` next to their field.
- Narrow widths: the Per database column folds under the Postgres roles cell.

## 7. Constraints and open decisions

- Lagoon tokens only, dark only; components from `components/ui/*`. Studio hides; the API decides.
- R30 gap: a role chain that holds ADMIN on another role is accepted by the API and quietly left out by the reconciler; nothing in `listRoles` shows ADMIN options, so the picker can't warn. Recorded, not designed around.
- Components (user: all in Studio): Button, Modal (restyled now: deep-water fill, strong hairline, square, no shadow; plus a wide size), `ui/tabs` (extracted), `ui/combobox` (the email field and the RolePicker's base), `ui/radio-group`, `ui/confirm-dialog`; `members/` RolePicker, PlatformRoleField and the shared AccessEventList.
- The log's names come from the events themselves (`actor_email`, `target_email`, `project_name`, T12x), as on the Members page.
- R33 (data keys follow the same role rule as grants) doesn't show on this tab: nothing here binds a key to a role.
