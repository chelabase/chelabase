---
version: 1
slug: "src-routes-app-members-tsx"
primary_target: "src/routes/app.members.tsx"
related_targets: ["src/components/members","src/components/app-shell/rail-items.ts"]
---

# Members page

**Scope and mode:** Operate. The new rail section `/app/members` (`src/routes/app.members.tsx`, `src/components/members/*`), with three tabs: People (with its Requests section), Activity and Instance. Shape brief (Members step T12a): layout, states, copy and components. No direction contract: the world is the inherited app system (DESIGN.md) and the topology is pinned by the spec (part 6). The user's answers to the T12a questions (Q1–Q10) are applied; Q1 and Q2 rely on API additions built by task T12x.

**Members hub amendment (2026-10-23, spec `docs/superpowers/specs/2026-10-23-members-hub-design.md`, comps `docs/superpowers/designs/2026-10-23-members-hub/` 01–03, 13, 14; supersedes the lines below where they disagree):**
- Tabs: People, Roles (anyone with a project grant; its own brief, `src/routes/app.members_.roles.$databaseId.$branchId.$role.tsx`), Activity, Instance. Teams joins in step 2.
- The user chose a card / list toggle (H3), which overrides "no cards, no avatars": cards are square, hairline-framed, ink ground (6% chalk wash on hover, no tint), with a 32px initials square (hairline control border, chalk-dim 12px initials; dashed for an invited person), never coloured circles. Card: name (600), email, then the level tag, "N projects", and a status word when not active. A row menu sits top-right for tenant admins. Three columns at 56rem, one at phone width.
- Toolbar: "{n} people" (with a project: "{n} people · {m} on {project}"), filter, a project select ("All projects"), the cards/list toggle (segmented, tab tone, icons), and sand "Invite people" (tenant admins). Non-admins get the chalk-dim line "Tenant admins invite and remove people. Levels count only the projects you can see."
- Groups (MH-35, status first): Tenant admins, Project admins, Developers, Viewers, Other projects, No access yet, Invited, Suspended; empty ones hidden. Each heading: 16px title, chalk-dim count, a hairline, and a right-aligned meaning where useful ("Admin on every project", "Access only on projects you can't see", "Signed in, no project yet", "Access counts once they accept", "Can't sign in; access kept for reinstating"). In list view the group is a heading row inside the table.
- Level tag (LevelBadge): micro text in a strong-hairline frame, chalk-dim; "Tenant admin" in chalk with a 12px ShieldCheck. Suspended and Invite expired are coral words.
- Project filter: groups become "Admins on {project}" (tenant admins included, tagged), "Developers on …", "Viewers on …", "No access on {project}", then Invited, Suspended; the Level column becomes "On {project}".
- List: Person (link) · Level · Projects · Status · Joined · menu; below 40rem the row stacks (level and status under the name).
- Multi-email invite: an Emails chip field ("Paste or type addresses, separated by commas, spaces or new lines. Up to 50."); an invalid address is a coral-outlined chip with its reason in a coral line under the field and is not sent; the footer counts "{n} invites · {k} to fix" and the button reads "Send {n} invites". Access rows are shared by the batch (platform role offers Viewer / Developer / Admin only). After sending, the dialog lists one result per email (seafoam square: invited / invited again / already a member; solid coral square: not sent with the reason), titled "Invites sent: {ok} of {n}", with "Edit and retry the failed one" and sand Done. Link delivery lists the links together, each shown once with Copy.

**Amendments (user, 2026-10-09, after T12a; built by T12b):** open sign-up is kept and decided by mode (R34 revised): self-hosted, a tenant admin may turn it on; platform, it stays refused. Sign-ups wait for approval (R39, R40): a Requests section with a badge on the Members rail item. Link invites without SMTP (R35). The tenant's display name (R36). These are written into the sections below; where an earlier line disagrees, the amended one wins.

## 1. Job and audience

- **Everyone in the tenant** reaches it from the rail to see who is here: names, emails, who is invited and who the tenant admins are. Most visits are short and read-only.
- **Tenant admins** come here to let people in and out: invite by email with project access, resend or revoke an invite, remove someone, make someone a tenant admin. They also read the access log (Activity) when something looks wrong (R29: a project Admin re-invited a removed person).
- **Tenant admins** also answer sign-up requests (approve with project access, or decline) and name the tenant.
- **Instance admins** (`me.instance_admin`; self-hosted: every active tenant admin) flip open sign-up on the Instance tab.
- State of mind: deliberate. Every write here changes who can reach data, so actions are explicit, named, and confirmed when they take power away or hand it out tenant-wide.

## 2. Outcome and proof

- Success for a tenant admin: an invite sent with the right access in one dialog; a removal that visibly takes effect; the log answers "who gave X access to Y, and when".
- Success for a member: they can see who to ask (the tenant admins are marked).
- Truths this page must carry (from T7's API, `members` tag):
  - Statuses are `invited`, `pending` (signed up with open sign-up on, waiting for approval, R39/R40) and `active`; removed people are not listed. Pending people show only in Requests, never in the people table or its count.
  - An invited member carries `invite.expires_at` (72 h after sending); an expired invite can be resent (new code, the old link stops working).
  - Invite outcomes: `invited` (mail sent, 201), `added` (an existing org user with a password became an active member, 201, no mail), `existing_member` (200, the grants were set).
  - R29: inviting a removed or deactivated person brings them back as `invited` with a fresh code, never silently active.
  - The tenant always keeps one active tenant admin (`409 last_tenant_admin`).
  - Removal ends access at once: grants and overrides go, member roles are dropped on every branch, sessions end.

## 3. Selected direction

- Inherited world, Restrained colour: ink ground, chalk text, hairline rules, sand only for the one primary action ("Invite people") and focus, coral only for removal and errors, seafoam only for positive state. No cards, no avatars-in-tiles, no badges in pastel fills.
- Structural thesis: **a roster, not a dashboard.** One calm table of people in the 56rem page column, with status written as words, and actions behind one per-row menu so the table stays scannable. The admin's power lives in the row menu and one dialog; the reader's view is the same table without them.
- Sequence: heading "Members" (headline), a one-line description, the tab strip, then the tab's content. People is the default tab.

## 4. Scope and boundaries

- Production screen, all three tabs, every state below.
- Untouched: the app shell's look and behaviour (only `rail-items.ts` gains the Members link and its `isRailItemActive` case), the dashboard, other project tabs.
- Rail (Q6, user: Members last): Projects, API reference, Placeholder, Members. Members replaces `placeholder-2`; lucide `Users` icon; label "Members"; active on `/app/members` and below.
- Grants per project are edited on the project's **Access** tab, not here. The invite dialog sets initial access only.
- Anti-goals: no Supabase-style "team" page look or wording; no role matrix grid; no avatars generated from initials in coloured circles; no toasts as the only feedback for a failed write.

## 5. States and ranges

Realistic range: 1 to ~50 people (self-hosted team); a few invited at once; Activity can run to thousands of events (paged, 50 per page).

### Page

- **Loading:** muted "Loading members…" in the page column (the Studio convention: text, not spinners).
- **Load error:** coral alert "Couldn't load members: {errorText}" and a Retry button. `no_access_yet` never reaches here (listMembers is `TenantMember`); a removed caller gets 404 → "You're no longer a member of this tenant." with Sign out.
- **Tabs shown:** People always. Activity only when `me.member.tenant_admin`. Instance only when `me.instance_admin` **and** `GET /v1/instance/settings` answers `open_signup_switchable: true`; hidden (not disabled) otherwise, so platform mode never shows it. The tab is kept in the URL (`?tab=people|activity|instance`); an unavailable tab in the URL falls back to People.
- **Tenant name** (R36, amendment): the description line reads "People in {display name} and what they can reach." (`me.tenant.display_name`, falling back to the slug). For tenant admins a ghost "Rename" button follows the line; it opens an inline form in place of the line: field "Tenant name" (max 80 characters, prefilled), "Save" (primary) and "Cancel". Saving sends `PATCH /v1/tenant { display_name }` (an empty value clears it, and the slug shows again), then refetches `me`; a `role=status` line "Renamed the tenant to {name}." Errors stay under the field (422: the server's sentence). Everyone else sees the line only.

### Requests (People tab, tenant admins; amendment, R40)

- Shown above the people table only for tenant admins and only while `listMembers` has members with `status: "pending"`. Everyone else never sees pending people.
- Heading "Requests" (16px semibold) with the count in chalk-dim tabular numerals ("Requests 2"), then a chalk-dim line: "These people signed up and are waiting for a tenant admin. Approve them with the projects they need, or decline."
- One row per request (hairline-separated, 36px+): name (fallback: the email's local part) and email under it, "Signed up {relative}" (absolute UTC on hover/focus, from `created_at`), then two buttons: "Approve…" (outline) and "Decline" (destructive ghost). Accessible names "Approve {name}" and "Decline {name}".
- **Approve** opens the dialog "Approve {name}" (wide): the same Access rows as the invite dialog (project, platform role, Postgres roles; none is allowed, with the helper "Without a project, they join with no access; you can give access later from a project's Access tab."). Primary "Approve", secondary "Cancel"; "Approving…" while it runs. It sends `approveMember(id, { grants })`. Success: the dialog closes, the row leaves Requests, the person shows in the table as Active, and a status line "Approved {email}. They can sign in now." Errors in the dialog as for the invite dialog (422 with `extra.role` on its row; 409 `conflict`: "{name} was approved or declined in the meantime." and the list refreshes).
- **Decline** opens a destructive confirm: title "Decline {name}?", body "Their sign-up is removed and their account is deactivated, so they can't sign in. To let them in later, invite them." Button "Decline". Success: the row leaves, status line "Declined {email}." 409: as above.
- **Rail badge:** the Members rail item carries the number of pending requests for tenant admins (from the same `listMembers` query, refetched on focus and every 60 s while Studio is open). Expanded rail: the count after the label, chalk text in a square strong-hairline outline, tabular numerals. Collapsed: a 6px sand square at the icon's top right (sand: something to act on). The link's accessible name becomes "Members, {n} requests waiting". Zero or not a tenant admin: nothing.

### People tab

- **Header row:** "{n} people" (chalk-dim, tabular numerals) on the left; on the right, for tenant admins only, a filter field ("Filter by name or email") and the primary "Invite people". For everyone else, the filter field and the line "Tenant admins invite and remove people." in chalk-dim.
- **Table columns:** Person · Status · Joined · (row menu, tenant admins only).
  - Person: `display_name` in chalk (fallback: the email's local part), the email under it in chalk-dim; "(you)" after the caller's own name; a small "Tenant admin" label (chalk, lucide `ShieldCheck` 16px) after the name for tenant admins. `email: null` shows "Email unknown" in chalk-dim.
  - Status, as words: "Active"; "Invited · expires in 2 days" (relative, absolute UTC in a tooltip/title); "Invite expired" (coral text, since the link no longer works); "No access yet" (chalk-dim) for an active member with `has_access: false` who isn't a tenant admin (Q1: `Member.has_access`, T12x).
  - Joined: the date of `accepted_at` for invited-then-accepted members, else of `created_at` (Q1: `Member.created_at`, T12x); invited members show "—". Absolute UTC on hover/focus.
- **Order:** the API's (by email). The caller's own row is not pinned.
- **Empty filter result:** "No one matches “{query}”." with a Clear filter link.
- **Only the caller** (a fresh install): the table with one row, then the line "Invite the people you build with. Each one gets access only to the projects you choose." and, for tenant admins, the Invite button repeats inline.
- **Row menu** (tenant admins; a "More actions for {name}" icon button, lucide `MoreHorizontal`, opening a menu styled as the account menu):
  - Invited: "Resend invite", "Revoke invite".
  - Active, not tenant admin: "Make tenant admin", "Remove from tenant".
  - Active tenant admin: "Remove tenant admin", "Remove from tenant".
  - Own row (Q4, user): never "Remove from tenant"; a tenant admin can't remove themselves. "Step down as tenant admin" (in place of "Remove tenant admin") only while another active tenant admin is listed; with no other admin, the item is absent and the menu shows a disabled line "You're the only tenant admin."
- **Step down as tenant admin:** confirm dialog (it removes the caller's own power). Title "Step down as tenant admin?" Body "You stay a member and keep your project access, but you can no longer invite or remove people, or reach projects you have no grant on." Button "Step down". `409 last_tenant_admin`: the copy below.
- **Make tenant admin:** confirm dialog. Title "Make {name} a tenant admin?" Body "Tenant admins invite and remove people, create projects, and are Admin on every project, including the database owner." Buttons "Make tenant admin" (primary) / "Cancel". `409 conflict` (not active): "Only an active member can become a tenant admin. {name} hasn't accepted their invite yet."
- **Remove tenant admin:** no dialog (it reduces power); acts at once, the row shows "Removing…" then updates. `409 last_tenant_admin`: inline row error "{name} is the last tenant admin. Make someone else a tenant admin first."
- **Remove from tenant:** destructive confirm dialog. Title "Remove {name}?" Body "{name} loses access to every project at once: their access is taken away, their database roles are dropped on every branch, and they're signed out. To bring them back, invite them again; they'll come back as invited." Buttons "Remove {name}" (destructive) / "Cancel". `409 last_tenant_admin`: same copy as above, in the dialog.
- **Revoke invite:** destructive confirm. Title "Revoke the invite to {email}?" Body "The link in their email stops working and the access it carried is dropped." Button "Revoke invite". `409 conflict` (accepted or re-invited meanwhile): "This invite was accepted or replaced in the meantime." and the list refreshes.
- **Resend invite:** no dialog. Row status updates to the new expiry; a status line under the header "Sent a new invite to {email}. The previous link no longer works." `409 conflict`: "This invite was revoked or accepted in the meantime."
  - Amendment (R35): with email available the menu offers "Resend invite" (email) and "New invite link"; without it, only "New invite link". "New invite link" sends `resendInvite(id, { delivery: "link" })` and shows the returned `invite_url` once in the invite-link panel (see the invite dialog) inside a dialog titled "New invite link for {email}". A 409 that says to send by email reads "{email} already has a way to sign in, so a link can't be used. Resend the invite by email."
- **Partial failure (503 `unavailable` after remove or revoke):** the person is removed in Chelabase and drops out of the list, but the identity step didn't finish. A coral notice stays above the table until it succeeds: "{name} is removed, but signing them out of the identity provider didn't finish." with a "Retry" button (re-sends the same DELETE; the id is kept in component state, since removed people aren't listed). Never silent.
- **Write in flight:** the acting row's menu trigger is disabled and shows "Working…"; other rows stay usable.

### Invite dialog (tenant admins)

- Title "Invite people". Fields:
  - "Email" (required, `type=email`, trimmed and lowercased on send).
  - "Access" section: zero or more grant rows, each: Project (select from `me.tenant.projects`, a project used once only), Platform role (Viewer / Developer / Admin, segmented, Developer preselected), Postgres roles (role picker, see the Access tab brief for its rules). "Add a project" adds a row; each row has "Remove" (icon button, "Remove {project}"). No rows is allowed: helper text "Without a project, they join with no access; you can give access later from a project's Access tab."
- **Delivery** (amendment, R35), read from `GET /v1/invites/delivery` when the dialog opens: a radio group "How they get the invite" with "Email" ("We email them a link.") and "One-time link" ("You copy a link and send it yourself."). With `email_available: true` Email is preselected. With `false` only the link is offered (the Email option is absent, not disabled) and a chalk-dim note explains why, by `reason`: `no_email_provider` "Email isn't set up on this instance, so invites go out as a link you send yourself. To send them by email, add an SMTP provider in Zitadel."; `zitadel_unavailable` "Couldn't check whether email is set up, so invites go out as a link for now."; `zitadel_not_configured` "This instance has no identity-provider admin access configured, so invites go out as a link." While the lookup runs, the group shows "Checking email…" and Send waits.
- Primary "Send invite" with Email, "Create link" with One-time link (the outcome, mail or no mail, isn't known before sending), secondary "Cancel". While sending: "Sending…", fields disabled.
- **The link, shown once:** an `invite_url` in the answer replaces the form in the same dialog with the invite-link panel: heading "Invite link for {email}", the URL in a read-only mono field (selected on focus) with a "Copy link" button, and a coral-free warning in chalk: "This link is shown once. Copy it now and send it to {email} yourself; it works for 72 hours. Anyone with the link can set the password for this account." Then "Done" closes the dialog. The URL lives only in that component's state: the mutation runs with the secret options (`gcTime: 0`, `reset()` after the call, as the auth forms do), it never enters the query cache, the URL, storage or a log, and closing the dialog drops it.
- **409 "send by email"** (a link invite to someone who already has a way to sign in): under the delivery group, "{email} already has a way to sign in, so a link can't be used. Send the invite by email." and, when email is available, the group switches to Email. Without email: "Ask them to sign in with their existing account instead." A 409 for someone whose sign-up is waiting: "{email} signed up and is waiting for approval. Approve them under Requests."
- Results (the dialog closes and a status line under the header says what happened):
  - `invited`: "Invited {email}. The link in their email works for 72 hours." (link delivery: the panel above, then on Done "Created an invite link for {email}.")
  - `added`: "{email} already had an account here and was added as a member. No email was sent."
  - `existing_member`: "{email} is already a member. Their access to the chosen projects is set."
- Errors stay in the dialog, beside their cause:
  - 422 "the email isn't one": under Email, "Enter an email address."
  - 422 with `extra.role` (and `extra.chain`, `extra.database_id`): the offending role chip in the matching grant row turns coral-outlined; under that row, the server's sentence, plus the chain rendered in mono as `billing_admin → billing → app_login`, and the database named from `database_id`.
  - 422 "a project is named twice": prevented client-side; if it comes back, under the second row.
  - 409 `conflict`: "Another account outside this organisation already uses {email}, or more than one does. Use a different email, or fix the account in the identity provider."
  - 409 `project_deleting`: under the row, "{project} is being deleted."
  - 404 project: "{project} no longer exists." and the row is marked.
  - 503 `unavailable`: dialog-level coral line "Couldn't reach the identity provider, so no invite was sent. Try again."

### Activity tab (tenant admins)

- A chronological list, newest first: each row is a time (relative, tabular numerals, absolute UTC on hover/focus via `<time title>`), then one sentence. 36px rows separated by hairlines. Not a table of raw columns: the sentence is the content.
- Names (Q2, user: events carry `actor_email`, `target_email` and `project_name`, T12x): a person is their display name when the members list has them, else the event's email (removed people included); a project is `project_name` (deleted projects included); a database resolves through the project's databases, else "a deleted database". Actor `null` reads "Chelabase". An email that is `null` reads "a former member".
- Sentences:
  - `member_joined`: "{target} signed in for the first time"
  - `member_added`: "{actor} added {target} (already had an account)"
  - `admin_claimed`: "{target} became the first tenant admin"
  - `admin_set` / `admin_unset`: "{actor} made {target} a tenant admin" / "{actor} removed {target} as a tenant admin"
  - `member_removed`: "{actor} removed {target}"
  - `grant_put`: "{actor} gave {target} {Role} on {project}" and, when `before` exists, "{actor} changed {target}'s access on {project}: {before role} → {after role}"; roles listed after in mono when they changed.
  - `grant_deleted`: "{actor} took {target}'s access to {project} away"
  - `override_put` / `override_deleted`: "{actor} narrowed {target} on {database} to {Role}, {roles}" / "{actor} stopped narrowing {target} on {database}"
  - `invite_created` / `invite_resent` / `invite_revoked` / `invite_accepted`: "{actor} invited {email}" (the email from `after.email`) / "… sent the invite to {target} again" / "… revoked the invite to {target}" / "{target} accepted the invite"
  - `open_signup_set`: "{actor} turned open sign-up on|off"
  - Amendment: `tenant_renamed`: "{actor} renamed the tenant to {after.display_name}" (or "… cleared the tenant's name" when it's null); `member_approved`: "{actor} approved {target}'s sign-up"; `member_declined`: "{actor} declined {target}'s sign-up". An unknown action reads "{actor} changed access ({action})".
- A row expands (disclosure button) to show `before` and `after` as two mono blocks, for when the sentence isn't enough.
- Paging: "Show older" button under the list, using `next_before`; hidden when `next_before` is null; while loading it reads "Loading…". Older pages append (keep scroll position).
- Empty: "No access changes yet. Invites, removals and changes to anyone's access will show here."
- Loading: "Loading activity…". Error: coral line with Retry; an error loading an older page shows under the list and keeps what's shown.

### Instance tab (instance admins)

- One setting, laid out as a label/description column and a control on the right:
  - "Open sign-up". Description (amended, R39/R40): "Anyone who can reach this Studio can create an account. Each new account waits under Requests until a tenant admin approves it." State line under the switch: "On: new accounts wait for approval" in seafoam / "Off: only invited people can join" in chalk-dim.
  - The switch acts at once (no dialog; it's reversible), disabled while saving; `PATCH /v1/instance/settings { open_signup }`.
  - The tab only exists when `open_signup_switchable` is true (see Tabs shown); a 403 `not_available_in_platform_mode` that still arrives shows the server's message under the switch.
- Loading/error as the other tabs.
- Removed by the amendment: the disabled-switch state for `open_signup_switchable: false` (the tab is hidden instead). Nothing of the earlier "no sign-up at all" ruling remains in this brief.

## 6. Interaction and layout

- Page column 56rem, page gutter 32px, headline "Members", then a chalk-dim line "People in {tenant display name, else slug} and what they can reach." (with the tenant admins' Rename, see Page) Tabs use the project page's tab strip (extracted, see components), with `role=tablist` and arrow-key movement.
- Focus: every control takes the 2px square sand outline. After a dialog closes, focus returns to the row menu trigger (or the Invite button). After a removal, focus moves to the table's header row.
- Status lines (results of writes) are `role=status`; errors `role=alert`.
- Width: desktop first (DESIGN.md's open gap). Below 40rem the Status column folds under the Person cell; the table never scrolls the page sideways.
- Motion: none beyond the shell's; dialogs open without animation or with the 150 ms fade Base UI gives.

## 7. Constraints and open decisions

- Lagoon tokens through Tailwind only, dark only, no colour literals, fonts only from lagoon; components from `components/ui/*`.
- Studio hides; the API decides. Any 403 shows `errorText` plainly.
- Components (user: all in Studio, none in lagoon): Button; Modal, restyled now (deep-water fill, strong hairline, square, no shadow, plus a wide size for the invite dialog); new `ui/tabs`, `ui/menu`, `ui/confirm-dialog`, `ui/switch`, `ui/combobox`, `ui/radio-group`; `members/` RolePicker, PlatformRoleField, GrantRows and AccessEventList (shared with the Access tab and the project invite).
- Wording (Q5, user): "Tenant admin" everywhere a person's tenant-wide role is named.
- API from T12x: `Member.has_access`, `Member.created_at`; `AccessEvent.actor_email`, `target_email`, `project_name`.
- API from T12y (amendments): `GET /v1/invites/delivery`, `delivery` and `invite_url` on invites and resends, `approveMember`, `declineMember`, `PATCH /v1/tenant`, `me.tenant.display_name`, `GET`/`PATCH /v1/instance/settings`. Added component: `members/InviteLinkPanel` (the shown-once link).

## 8. Capability hiding across Studio (amendment)

- Studio hides what the caller can't do, from `/v1/me` (`member.tenant_admin`, `projects[].platform_role`, `projects[].roles`); the API still decides and any 403 shows its message.
- Project page: the SQL view is hidden below Developer; database connection details, password resets, database delete and create, role create/delete/reset, the data API switches and project rename/delete are Admin only. "New project" on the dashboard is for tenant admins.
- Role pickers (the SQL editor's "Run as", the console's role) list only the roles the caller may act as: Admin sees Owner and every role; below Admin only the granted roles and the roles their chains reach (R21), never Owner.
- The SQL editor's schema reads send `?role=` for the chosen role (T9), so completion shows what that role can use.
- Pending callers (`me.member.status === "pending"`) see the "Waiting for approval" screen instead of Studio (its own brief).
