---
version: 1
slug: "src-routes-app-members-memberid-tsx"
primary_target: "src/routes/app.members_.$memberId.tsx"
related_targets: ["src/components/members/member-page"]
---

# Member page

**Scope and mode:** Operate. `/app/members/$memberId` (`src/routes/app.members_.$memberId.tsx`, `src/components/members/member-page/*`), Members hub step 1 (spec `docs/superpowers/specs/2026-10-23-members-hub-design.md` Parts 3, 6, 7; plan task S2). The world is the inherited app system (DESIGN.md); the topology is pinned by the spec. Approved comps: `docs/superpowers/designs/2026-10-23-members-hub/` 04, 05, 06, 12, 15, 16.

## Job and audience

- Tenant admins and project Admins set one person's access in one place: platform role and Postgres roles per project, personal logins, suspension.
- Developers and Viewers read (platform role per visible project only; never another member's roles or overrides).
- The member themself reads their access and manages their own personal logins.

## Layout

- Page column 56rem. "← Members" back link carrying tab, project, view, q. Header: 44px initials square, name (headline), "(you)", then email, level tag, status, joined date on one wrapping line. Actions on the right by power.
- Sections in order: Project access, Personal Postgres logins, Activity (Admins). Each section: a 16px heading, one chalk-dim line, content.

## Header actions (tenant admins)

- Outline "Make tenant admin" (or "Remove tenant admin"), outline "Revoke Postgres access now" (KeyRound icon), destructive "Suspend", and an overflow menu with "Remove from tenant". Self: "Step down as tenant admin" when allowed. Suspended member: sand "Reinstate" is the only sand fill on the page.
- **Suspend** confirm: title "Suspend {name}?"; body "{First} is signed out of Studio and can't sign in. Her personal Postgres logins stop working at once and every database session she has open ends." then chalk-dim "Her project access and roles are kept, so Reinstate gives them back. She makes new personal logins herself afterwards. To take everything away for good, remove her from the tenant instead." Button "Suspend {first}" (destructive). Pronouns: use the name when unknown ("{first}'s").
- **Revoke Postgres access now** confirm: title "Revoke {first}'s Postgres access now?"; "For a password that may have leaked. At once:" then a list: the logins that stop (name on database each), and "every Postgres session she has open ends, including her SQL editor and table editor sessions, which reconnect with new passwords." Then chalk-dim "Her Studio access and project roles don't change, and she can create new personal logins." Button "Revoke access now".
- **Reinstate** confirm: title "Reinstate {name}?"; body "{first} can sign in again with the access listed on this page. Personal logins aren't restored; {first} creates new ones." Button "Reinstate" (primary).
- Suspended state: coral status word; a coral-bordered box under the header: "Suspended on {date} by {actor}. {first} can't sign in to Studio, his personal logins were revoked and his database sessions ended." + chalk-dim "His project access below is kept. Reinstate gives it back as it is; he makes new personal logins himself." Cards stay editable; their footer reads "Effective once reinstated: …".

## Project access cards (GrantCard)

- One hairline-framed card per visible project: heading row (project, databases in chalk-dim), body, footer on deep water with the Effective line and the card's own buttons (Take access away destructive-ghost, Discard ghost when dirty, Save sand; disabled until changed).
- Platform role: segmented `tab` tone, No access / Viewer / Developer / Admin, with the role's one-line description under it.
- Postgres roles: the RoleChecklist. Filter field above 8 roles ("Filter {n} roles"); checked first; each row: themed checkbox, mono name (escaped: control, bidi and non-ASCII shown as `\uXXXX` in a hairline box with the code point in its title), "also gives {chain}" in chalk-dim, "only on {database}" right-aligned. An "Admin only" sub-heading groups disabled rows with their reason (LOGIN role, Bypasses RLS, Reaches X, which is owner-level).
- **Step 2 slot:** a role from a team renders checked, disabled, on a zebra fill, with a 12px lock, "via {team}" link right-aligned. Never part of the PUT body.
- Dirty card: strong hairline frame, "Unsaved changes" with an 8px sand square in the heading row, changed rows carry a 2px sand inset left edge. Leaving asks first (router blocker).
- "Narrow on a database": a collapsed disclosure whose closed line states the current narrowing.
- Tenant admin's cards: "Admin on every project, as tenant admin", read-only.
- "Add access to a project": a dashed-frame row (not there yet) with a project select and an outline Add.
- Self view: segmented controls and checkboxes disabled, no Admin-only group, no buttons; "Ask a tenant admin or a project Admin to change your access."

## Personal Postgres logins

- Grid: Database (project under it), Login (mono), Expires ("in 71 days · 2 Jan 2027"), Created, actions.
- Within 14 days: a coral-outlined 8px square before the expiry. Self: the row's action is outline "Renew" (RefreshCw icon) plus an info box "{login} on {db} expires on {date}. Renew it for 90 more days: you get a new password, and the old one stops working." Admins: Delete only, and a chalk-dim line "Only {first} can create, reset or renew her logins."
- Revoked rows stay (chalk-dim) with "Revoked {date}: {reason}" until deleted.
- P23 line under the table: "A login works on every branch of its database where {you have|first has} Developer access or more. Connection strings are for the main branch; on another branch, use that branch's host."
- New login (self, Developer+): outline button → project, database, name (defaulted from the email's local part). Then the reveal dialog.
- **Reveal dialog** (38rem, no click-outside dismissal): title "Your login {name} on {database}"; description "{project} · expires {date} (90 days). Reset the password before then to renew it."; Password and "Connection string · main branch" as read-only mono boxes with a Copy segment; the P23 helper; an info box "Your roles here: … In psql, run SET ROLE {role}; to act as one."; the line "You won't see this password again. Copy it now." in chalk 600 (not a warning colour); sand "Done". State lives in the dialog only.

## Activity

- The shared AccessEventList filtered by member, sentences per the Members brief plus the new actions ("{actor} suspended {target}", "{target} created the personal login {name} on {database}", "… revoked {target}'s Postgres access", "{actor} changed privileges of {role} on {database} / {branch}").

## Phone width

The rail becomes a header menu trigger; actions wrap under the header; segments stretch to the width; checklist reasons drop to a second line; the logins grid hides Created.
