---
version: 1
slug: "embers-roles-databaseid-branchid-role-tsx-150546ff"
primary_target: "src/routes/app.members_.roles.$databaseId.$branchId.$role.tsx"
related_targets: ["src/components/members/roles"]
---

# Roles tab and role page

**Scope and mode:** Operate. The Members hub's Roles tab (`src/components/members/roles/*`, in `/app/members?tab=roles`) and the role page `/app/members/roles/$databaseId/$branchId/$role` (`src/routes/app.members_.roles.$databaseId.$branchId.$role.tsx`). Spec `docs/superpowers/specs/2026-10-23-members-hub-design.md` Part 4 with its "After the red-team" rules, plan task S3 and rulings P23–P30. Inherited world (DESIGN.md). Approved comps: `docs/superpowers/designs/2026-10-23-members-hub/` 07–11.

## Roles tab

- Pickers in one row (labels above): Project, Database, Branch ("main · protected"), Postgres database; then sand "Create role" (Admins only). All held in search params.
- A protected branch: an info box with a lock icon: "{branch} is protected: role and privilege changes here need Admin, and Developers' logins can't change the schema. Roles live on this branch only; to have one elsewhere, merge or reset the other branch."
- Toolbar: "{n} roles on {branch}", filter, cards/list toggle (tab tone).
- Role card: mono name (escaped), tags (NOLOGIN or LOGIN; "owner-level" in coral), member count ("4 members", "No members"; **Admins only**, MH-34), one-line privilege summary in chalk-dim. The list view shows the same as columns.
- Owner, reserved roles and personal logins are never cards.
- "Personal logins on {database} {n}" (Admins only, P24): a grid of login, member (link to their page), expiry (coral-outlined square within 14 days), with the line "Members' own logins. They aren't roles you grant: each follows its member's access, on every branch."

## Create role dialog

- Title "Create a role", description "On {project} · {database} · {branch}. This branch only."
- Name (mono field; helper "Lowercase letters, digits and _, starting with a letter or _. Up to 63 characters.").
- Kind: stacked radios "Group role (NOLOGIN)" ("Members act as it. Nobody signs in as it.", default) / "Login role" ("An app or service signs in as it with a password.").
- Advanced (disclosure): Member of (checklist, noting a role in anon's or authenticated's chain), Inherit, Bypass row-level security (tagged owner-level), Connection limit ("No limit"), Valid until ("Never"). Footer Cancel / sand "Create role"; on success, go to the new role's page.

## Role page

- Wide page (not the 56rem column) with the One Panel on the right (360px). "← Roles" back link. Context line: project › database › branch [protected] › Postgres database {name}. Mono headline. Attribute tags. "This branch only: to have it elsewhere, merge or reset the other branch."
- Sub-tabs: Privileges, Members {n} (Admins only), Memberships, Danger zone.
- **Privileges:** one block per schema (deep-water heading row with the schema in mono and its counts; others collapsed). Inside, in order:
  - Schema: USAGE, CREATE checkboxes, with "USAGE granted by {grantor}".
  - Tables and views: a matrix, objects × SELECT INSERT UPDATE DELETE TRUNCATE REFERENCES TRIGGER MAINTAIN (MAINTAIN only when the server has it), an "All" column, and a Notes column. Header checkboxes are column toggles (mixed state shown). Each row has a chevron drill-in to a column grid (SELECT, INSERT, UPDATE, REFERENCES; table-wide privileges shown checked and disabled). The matrix scrolls inside its frame on narrow screens.
  - Grant option: a small "G" on the cell; Notes says "{PRIV} with grant option · Revoke grant option" (link). Never offered as a grant (MH-29).
  - Grantor: Notes shows "{PRIV} granted by {role}" whenever the grantor isn't the owner (including `neon_superuser`, P30).
  - Not settable (P27): the row is chalk-dim, checkboxes disabled, a lock replaces the chevron, Notes "owned by {X}: change in SQL as {X}". Same for functions.
  - All rows: one row per RLS table with a switch (seafoam when on, "on: policy {name}"), "All rows on every RLS table in {schema}" link, and the line "Restrictive policies still apply, and a role without SELECT on a table sees nothing there."
  - Sequences (USAGE, SELECT, UPDATE), Functions and procedures (signature in mono, EXECUTE), Future objects ("Created by" select: the owner first, then settable roles; rows for new tables, sequences, functions).
- **Staged edits:** a changed cell or switch gets a sand-wash fill and a 2px sand-45% top edge. The right panel "Pending changes · {n}" lists them in mono with remove controls; footer Discard (ghost) / Review changes (sand).
- **Review:** the panel shows the dry-run SQL (the highlighted statement in sand), the dry-run line (seafoam square), and:
  - "Exposes data through the data API." after an outlined coral square, when a grant goes to anon or authenticated, or (open question to the user) to a role they inherit.
  - 422 `exposes_data_api`: coral-bordered error box "Error: All rows can't be turned on for {role}." + the server's chain in mono + the fix. Apply stays disabled.
  - Footer Back / "Apply {n} changes" (sand).
- **409 `privileges_not_applied`:** panel title "Not applied"; coral error box naming object, privilege and grantor, "The whole change rolled back.", with the fix; the edits stay staged.
- **409 `privileges_changed_since_read`:** "Privileges on {object} changed since you opened this page. Reload to see them; your {n} changes are kept to re-apply." with sand "Reload privileges".
- Developers and Viewers see the matrix read-only, without Members counts or the panel.

## Phone width

The panel drops under the editor as a full-width block; matrices scroll inside their frames; privilege rows wrap.
