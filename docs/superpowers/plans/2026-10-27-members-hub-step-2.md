# Members hub, step 2: live-linked teams (implementation plan)

**Goal:** teams become a second source of access. A team holds per-project grants (platform role plus Postgres roles), and its members get them live: effective access is the member's own grant ∪ their teams' grants, each Postgres role keeping its own level (D1), narrowed afterwards by the member's database overrides. Every team write is checked on the union for every affected member and database (MH-4), takes the tenant lock then the members' locks in id order, revokes personal logins that stop qualifying in the same transaction, and fires the same side effects as a grant write (`access_changed_for`, MH-5). The SQL editor's hidden member role holds only roles at Developer level or above (T2-2). Tenant invites and sign-up approvals can attach teams (`team_ids`); the project invite refuses them (MH-8). Studio gains the Teams tab, the team page, team pickers in invites and approvals, and "via team" rows on the member and role pages.

**Spec:** `docs/superpowers/specs/2026-10-23-members-hub-design.md` (approved; D1–D6). Step 2 is Part 5 (teams, effective access, writes, visibility, log, triggers, invites), Part 3 section 1's locked "via <team>" rows and section 3 (Teams), Part 4's role-page Members "via a team", Part 1's Teams tab and `/app/members/teams/$teamId`, and Part 7's Teams row and Studio rules. **Red-team:** `docs/superpowers/specs/2026-10-23-members-hub-red-team.md`; step 2's findings are MH-1–MH-8 and MH-38a, plus the "Teams / access union" checklist. **Step 1 plan:** `docs/superpowers/plans/2026-10-23-members-hub-step-1.md` (P1, P2, P3, P4, R-T2a; what exists). Where a spec section and an MH ruling disagree, the MH ruling wins; where either disagrees with a ruling below, the ruling below wins.

**Architecture:**
- `crates/chelabase-metadata`: one migration `20261027000001_teams.sql` (T1); new `src/teams.rs` (T1); `src/members.rs` (the shared grant-source SQL, `Observed`, `touched_projects`, invite/approve/remove/reinstate writes, the log filter; T1); `src/personal_logins.rs` (`member_project_roles`, `reinstate_member`; T1); `tests/members_live.rs` (T1).
- `crates/chelabase-routes/src/lib.rs`: the team endpoints, `LIST_MEMBER_TEAMS` and `LIST_PROJECT_TEAM_GRANTS` (T23).
- `services/orchestrator`: new `src/teams.rs` (T23); `src/members.rs` (union validation, `Observed`-based checks, invites, roster `via`, member teams, project team grants; T23); `src/lib.rs` (`MetadataPort` methods; T23); `src/openapi.rs` (stub port; T23); `tests/common/fakes.rs` (team storage; T23); `tests/teams.rs` (new, T23); `tests/api.rs` (T5, then T23); `tests/access_walk.rs` (route fixtures; T23).
- Full-cell read through rest (Q1, T5): new `crates/chelabase-schema/src/cell.rs`; `crates/chelabase-types/src/table_api.rs` (`cell_failure`); `services/rest/{Cargo.toml, src/app.rs, src/exec.rs, tests/rest_live.rs}`; `services/orchestrator/src/{table_api/rows.rs, console.rs}`.
- `services/worker/src/member_roles.rs` and `member_roles/tests.rs` (T2-2; T4); `services/worker/tests/member_roles_live.rs` and `services/orchestrator/tests/sql_run_live.rs` (T4, live tests compiled, run at the end).
- `scripts/teams-e2e.sh`, its `justfile` recipe and `scripts/tests/workbench-recipes.sh` (T23).
- `apps/studio`: new `src/components/members/teams/*`, route `src/routes/app.members_.teams.$teamId.tsx` and `src/lib/members-search.ts` (S1); `member-page/*`, `roles/role-page.tsx`, `invite-dialog.tsx`, `approve-dialog.tsx`, `grant-rows.tsx`, new `team-picker.tsx` (S2). S1 and S2 follow step 1's comps (`docs/superpowers/designs/2026-10-23-members-hub/`); there is no step-2 comps task (user, 2026-10-27).
- `apps/docs/content/docs/using/members-and-invites.mdx`, `reference/access.mdx`, `architecture/sql-editor.mdx`, `docs/landing-highlights.md` (K).

**Tech:** Rust (workspace toolchain), SQL (metadata Postgres 17.11), TypeScript/React (Studio: TanStack Router, Base UI, Vitest). **No new external dependency** (`proptest`, `sqlx`, `utoipa` and the generated clients are in place); rest gains the workspace crate `chelabase-schema` as a dependency (P40). **No fork change:** no image build; the live pass runs on the pinned images (round 4's `0741b56deb67-dev`).

**Task pipeline (CLAUDE.md "Task pipeline"):** the coordinator writes each brief from this plan (behaviour, files, pinned interfaces, the edge-case checklist, the RED list). Every brief starts with "Read `<repo root>/docs/agent-preamble.md` and follow it" and says no agent may spawn agents. Then, in sequence: a **test agent** writes and commits the RED tests (`tN: RED tests`), compiles every target (live ones `--no-run`), runs every runnable one against the base and shows each failing on its first assertion for the intended reason, and checks each harness and system assumption against the code; a **coder agent** makes them GREEN without editing, weakening, skipping or deleting them (lint, format and typing fixes in RED files allowed, assertions untouched), adds edge tests for its checklist and lists them; a **refactor/audit agent** tidies without changing behaviour, confirms `git diff <red-commit>..HEAD` on the RED files is empty apart from lint-only edits, and reports defects and hardening separately (the task's one review). **Audits only on T1 and T23** (user, 2026-10-27): T4 and T5 run test then coder, and the coordinator checks their RED diff; the end-of-step review covers them. **Studio tasks are one agent each** (user, 2026-10-27): S1 and S2 each get one sonnet agent that writes the code plus unit tests for the logic it adds (data mapping, "via team" provenance, the pickers' behaviour); layout-only changes need no unit test (the browser pass and Impeccable's finish review check visuals). K is one agent. Fix rounds use the same pipeline, findings batched by area.

## Status

- **RESUME HERE (2026-10-27):** the stack `members-hub-teams` is up.
  - **Done:**
    - T4: RED `dc44d292`, GREEN `0a83940a`, merged; no audit; the coordinator checked the diff.
    - T1: RED `7344a3a4`, GREEN `8c577ff8`..`e8ff1c3c`, test-setup fix `8b19261c`, audit `8f579ca4` (one defect fixed: a teams-only join now reports the teams' projects, so the reconcile runs); merged `4679de4f`.
    - `main` merged in for the zstd/pingora advisory fix (`0c05f25f`).
  - **Running:** the T5 coder (RED `60a5f56e`).
  - **Next:** T23 RED, then S1/S2 from T23's API-surface commit, then K.
- **Finish-review fixes (2026-10-27, Studio only):** browser-pass and Impeccable findings fixed. **User ruling:** a team-given role row shows the team source as its own right-hand marker, styled as held (lock icon, "✓ via the A and B teams", every team linked to `/app/members/teams/$teamId`; "also via" when the own grant names the role too); the own checkbox stays editable and starts unchecked; team-only read-only cards show the same marker; the PUT never carries a team-only role (MH-38a). Also: Effective line lists team roles per team, team-given platform level line, approve/invite hint with teams, refusal copy from the 422 extras (team named, levels capitalised, mono role names), Cancel on "Add own access" and the team grant card, spacing/chip/tab-strip/role-page polish. **Open backend gap:** `/v1/me` carries no per-role level (D1's `role_levels` is not serialised), so the table editor's write controls for a Viewer-level role (item 5; captures 29-31) can't be gated in Studio; needs a `/v1/me` field.
- **Process failures:**
  - T1's RED commit broke two existing `members_live` tests through its new `member_row` helper. The test agent fixed them in `8b19261c`.
  - `personal_login_reads_report_status_and_qualifies` was already red on the base: a step-1 test bug.
  - T4's `sql_run_live::the_sql_editor_cannot_set_a_viewer_level_role` passes on the base by construction: that harness builds the member role itself. It guards only the editor's side, and `member_roles_live` covers the worker.
- **For T23 (T1 audit):** `TeamWriteError::Conflict` also covers name and description bounds. T23 validates those itself and answers 422.
- **T2-2 (user, 2026-10-27):** the SQL editor's hidden member role is granted only roles held at Developer level or above, as personal logins are. `RUN_SQL` and `GET_SQL_SCHEMA` are `ActAs(Developer)` (`crates/chelabase-routes/src/lib.rs:486-503`). The reconcile also revokes a role from an existing member role when it drops below Developer. T4 owns it (P18). Closes MH-1 at the database level.
- **Q1 (user, 2026-10-27, option 2):** a full-cell read as a role held below Developer goes through rest's console path (`authenticator` + `SET ROLE`), not the member role. Pinned under "Full-cell read through rest (T5)" (P40); T5 owns it.
- **P12 confirmed (user, 2026-10-27):** `team_ids` on both the tenant invite and approve.
- **Time cuts (user, 2026-10-27):** (1) Studio starts early, against the pinned API through T23's API-surface commit (P34), with mocked API data; (2) audits only on T1 and T23 (T4, T5 none of their own); (3) no comps task D and no comps pause: S1/S2 follow step 1's comps, Impeccable's finish review checks them at the end; (4) T2 and T3 merged into T23 (opus / opus / opus), K starts after its RED commit; (5) S1 and S2 are one sonnet agent each (code plus unit tests for the logic they add).
- **Seam pass (opus, 2026-10-27):** done; findings are P24–P40 (P15 corrected: the project grants list is a JSON array, so team grants get their own route).
- **Deferred to the final fix round (T1 audit hardening):**
  - **(a)** `database_grant_sources`, `members.rs:1724-1736`: scope the team lateral to the project's tenant.
  - **(b)** `put_project_grants_checked`, `members.rs:2448`: refuse when an `expect` entry for a team's project is missing.
  - **(c)** `teams.rs:310` `observe`: 3 queries per member × project, run twice per write. Batch them.
  - **(d)** `chelabase_member_level` doesn't check project status: confirm or add the check.
- **Deferred to the final fix round (T23 audit hardening):**
  - **(e)** `Observed` doesn't know a member is a tenant admin, so a team write can be refused with 422 because of a tenant-admin member. It fails closed. `services/orchestrator/src/teams.rs` `checked_team_write`.
  - **(f)** `checked_team_write` reuses the same `RoleContext` cache across a retry, so a retry can check stale role graphs (same file, ~l.320).
- **Team-only project card (user, 2026-10-27):** on the member page, a project the member reaches only through a team shows read-only, as locked rows reading "via <team> team", with Save hidden. An "Add own access" button turns it into an editable own grant. Today S2's card opens in creating mode with Save live, which would create an own Viewer grant by accident. This goes to the final fix round.
- **Final fix round, Studio items:** S2's local `TeamLink` cast and its stub team route in `member-page/test-setup.tsx` are replaced with S1's real route, now that both have merged.
- **K, S1, S2:** done and merged (`e7e72976`, `080b6c67`, `e5a49674`).
- **T5:** done. RED `60a5f56e`, GREEN `67f6e769`, merged `10ebba85`. No audit; the coordinator checked the routing diff.
- **T23:** done.
  - Commits: RED `370d1f28`, API surface `3cea1a48`, code `b33e5a84`..`c1a13866`, RED setup fix `7515c8dd` (process failure: the test seeded the caller into the team), audit `6c9505a7`.
  - The audit closed two union-check races: a grant delete racing a team grant, and AddMember racing a team grant change. Each has a live test and a fake test.
  - Merged into `members-hub-teams`.
- Branch `members-hub-teams` (cut from `main` at `2bdc1d9d`, after step 1's merge `6e3c8df3`); spec `792044ce`; plan: this commit.

## Plan rulings (mechanical, listed for the user)

Each has its cost if wrong. P24–P40 come from the seam pass (2026-10-27) and win over P1–P23 where they differ.

- **P1 One migration, owned by T1:** `crates/chelabase-metadata/migrations/20261027000001_teams.sql`, after the latest (`20261023000003_members_suspended_by.sql`). No later task adds one; a need for one is a deviation reported to the coordinator. Cost if wrong: a second migration file, harmless.
- **P2 Team shape:** `name` trimmed, 1–80 characters, unique per tenant case-insensitively (409 `name_taken`); `description` at most 500 characters, default empty. No cap on teams per tenant or members per team (grants have none). Cost if wrong: a later CHECK change.
- **P3 Team grants are grant rows:** `team_grants` has `project_grants`' columns and checks, and the same write rule (`GrantRow::new`: an Admin grant names no roles, R17; `owner` refused, R7). A team may grant Admin. Teams carry no overrides (spec R7). Cost if wrong: Admin teams were meant to be refused; one CHECK.
- **P4 One shared SQL shape (MH-2):** every grant-source read gets its team grants from one lateral join, `team_grants_lateral!(<member expr>, <project expr>)`, which yields the column `t_grants jsonb` (an array of `{team_id, platform_role, pg_roles}`, by team id, `'[]'` when none), joined only through teams of the member's tenant. `grant_sources_of` reads it into `GrantSources::new(own, teams, database)`. Cost if wrong: none beyond the macro.
- **P5 `chelabase_member_level` is replaced** (snippet under "Shared interfaces"): the best level among the own grant and the member's team grants on the database's project, capped by the override. `LEAST` ignores NULLs in Postgres, so "no source" must be tested before the `least(...)`, or a member with no source but an override comes out `admin`. `chelabase_personal_login_qualifies` calls it by name at run time and isn't recreated. Cost if wrong: personal logins qualify wrongly; T1's exhaustive agreement test catches it.
- **P6 Lock order (MH-4):** a team write takes the tenant row (`FOR NO KEY UPDATE`), then every affected member row in id order (`FOR NO KEY UPDATE`), then personal-login records (`revoke_unqualified_logins`, unchanged). All team writes therefore serialise on the tenant. Own-grant and override writes keep locking only their member. Their `Observed` now includes the member's team grants, so a team change that commits between their check and their write comes back as `Changed` (the existing retry loop, `CHECKED_WRITE_TRIES`). A grant write that adds teams (an invite to an existing member, P12) takes the tenant first. No cycle: a grant write holds its member and takes only `KEY SHARE` on the tenant, which `NO KEY UPDATE` doesn't block. Cost if wrong: a deadlock between a team write and a remover; T1's interleaving test pins the order.
- **P7 Checks on the union (MH-4):** `Observed` gains the member's team grants on the project. `RoleContext::check_effective` takes the state the write would leave, as an `Observed`, and checks `after.sources(Some(db)).effective()` on each database: skipped when `None` or at Admin, otherwise each named role's chain on every branch, as today. `effective_on` is deleted; R-T2a showed `GrantSources::effective` agrees with it on every single-grant shape. A team write checks every (affected member × affected project × database); its 422 also carries `member_id` and `team_id`. Cost if wrong: a refusal message shape; the property test pins the invariant.
- **P8 Team membership is edited one member at a time:** `PUT /v1/teams/{team_id}/members/{member_id}` (idempotent: 200 with the team, also when already in) and `DELETE …` (204, also when not in). No set-replace route. The team page's checklist stages changes, and Save sends one call per change in sequence, showing a result per change (R8's pattern). Cost if wrong: a refusal halfway leaves the earlier changes applied; each change's result says so.
- **P9 Team events:** `team_created`, `team_updated`, `team_deleted`, `team_grant_put`, `team_grant_deleted`, `team_member_added`, `team_member_removed`; target member none, except member add/remove, where the target is that member. `after` (and `before` for deletes and removals) is `{team: {id, name}, grants: [{project_id, platform_role, pg_roles}], affected_member_ids: [...]}` with only the grants the event concerns. `project_id` is set when the event concerns exactly one project (grant put/delete). Step 1's `ProjectScope` filters `grants` already; it also filters `affected_member_ids` to nothing for a project log (a project Admin sees the event, not who else the team reaches). The `member_id` filter also matches `before`/`after` `affected_member_ids` (`@> jsonb_build_array($5::text)`). No new index. Cost if wrong: a slow member-filtered log on a very large tenant; a GIN index later.
- **P10 Removing a member from the tenant** deletes their `team_members` rows in `remove_member`'s transaction. `member_removed`'s `before` gains `teams: [{id, name}]`; there are no separate `team_member_removed` events. `touched_projects` (read before the delete) already includes their team grants' projects (P4 applied to it). Cost if wrong: the team's own log doesn't show the removal; the member's does.
- **P11 Team reads (MH-6, MH-34):** `GET /v1/teams` and `GET /v1/teams/{id}` are `TenantMember`. Members (ids) are shown to everyone, as the roster is. Projects are filtered by R1: a tenant admin sees all; others see the projects where they have any access source. A team's `pg_roles` on a project are shown only to tenant admins and to Admins of that project; others get `pg_roles: null` (MH-34's holders rule). Counts are over visible projects. Cost if wrong: Developers can't see which roles a team gives; their own effective roles are on `/v1/me`.
- **P12 Teams in invites (confirmed by the user, 2026-10-27):** `team_ids` (at most 50, deduplicated, every id a team of the caller's tenant, else 422 naming the id) on `POST /v1/invites` **and** `POST /v1/members/{id}/approve`. Both are `TenantAdmin`, and spec Part 5 says approval "offers the same access fields". On all three invite outcomes (invited, added, existing member) the memberships are written in the same transaction as the grants, with one `team_member_added` per team. The invite's grants plus its teams are checked together on the union (P7). `POST /v1/projects/{id}/invites` gets an explicit `team_ids` field documented as refused: any value present (an empty array too) is 422 `invalid_request` "teams can be attached only by a tenant admin's invite" (MH-8). Cost if wrong: approve-with-teams is one field to drop.
- **P13 Roster `via` (Part 2):** `member_project_roles` returns one row per (member, project) with the best level among the sources, and `via` `own`/`team`/`both`. `AccessVia` gains `team` and `both`. A tenant admin's projects stay `own`. `has_access` counts team grants. Cost if wrong: display only.
- **P14 Member teams (Part 3 section 3, locked rows):** `GET /v1/members/{member_id}/teams` (`TenantMember`) lists the member's teams with each team's grants on the caller's visible projects (P11's `pg_roles` rule). The member page builds the locked "via <team>" rows from it. Cost if wrong: one more read per member page.
- **P15 Holders (Part 4, MH-34; corrected by P30):** `GET /v1/projects/{id}/team-grants` (`LIST_PROJECT_TEAM_GRANTS`, `Project(Admin)`) lists `[{team_id, name, platform_role, pg_roles, member_ids}]` for the team grants on that project, by lower(name). `GET /v1/projects/{id}/grants` is unchanged. The role page's Members section joins both client-side, "via <team>". Cost if wrong: one more request on the role page.
- **P16 Reinstate reconciles team projects:** `reinstate_member` returns the touched projects, as `suspend_member` does, and the handler passes them to `access_changed_for`. Today it reads the own grants only (`services/orchestrator/src/members.rs:2857`). Cost if wrong: team-only branches of a reinstated member wait for the sweep.
- **P17 Team write API answers:** create 201 `Team`; update 200 `Team`; delete 204; put grant 200 `Team`; delete grant 204 (also when there was none); member put/delete as P8. 404 for an unknown team, project or member, or one of another tenant; 409 `conflict` for a pending member ("approve the sign-up first"); 409 `conflict` after `CHECKED_WRITE_TRIES` `Changed`. Cost if wrong: status codes.
- **P18 The hidden member role under T2-2:** its memberships are `granted_roles` of `Effective::roles_at_least(Developer)`; whether it exists stays as today (`granted_roles` of the full effective set is non-empty). A Viewer-level-only member keeps a member role with no memberships, so the table API read's CONNECT check, which connects as `cb_m_<id>` for non-Admins (`services/orchestrator/src/table_api/read.rs:217-224`), keeps working. `applied_roles` records the memberships. Cost if wrong: a member role with no memberships exists for Viewers; it can't be reached (sealed password, Developer-only SQL editor).
- **P19 OpenAPI and generated clients:** T5 runs `just --justfile <wt>/justfile --working-directory <wt> api-gen` as its last step (GET_CELL's documented answers change) and merges before T23 starts. T23 runs it twice: in its API-surface commit (P34) and as its last step. T1 and T4 change no documented response and don't run it. A conflict in generated files is never hand-merged: the later task reruns api-gen on the merged base. Cost if wrong: one regeneration.
- **P20 Landing highlights:** K writes step 2's entry into `docs/landing-highlights.md` on the step branch ("in review"). After the merge the coordinator changes it to "shipped" on `main`. Cost if wrong: one more edit on `main`.
- **P21 Live runs:** see "Live runs (the plan's one stack)" per task and End of step 5. Scratch-database metadata tests run inside T1 by name. Everything that needs this step's orchestrator, worker or gateway on the stack (worker live suites, `sql_run_live`, e2e, the browser pass) waits for the end-of-step pass, after one restart of the apps **and `studio`** from the merged branch (step 1's P35).
- **P22 Studio routes:** S1 creates `app.members_.teams.$teamId.tsx` and commits `routeTree.gen.ts`. S2 adds no route. Cost if wrong: none.
- **P23 Copy from a member (Part 5 invites):** S2 builds it from the tenant's `GET /v1/projects/{id}/grants` lists (the invite is a tenant admin's), filling custom rows from that member's own grants only, never their teams. No new endpoint. Cost if wrong: one request per project when the picker opens.

### Seam-pass rulings (2026-10-27)

- **P24 `Observed` literals:** adding `teams` breaks the struct literals at `crates/chelabase-metadata/src/members.rs:424,467` and `services/orchestrator/tests/common/fakes.rs:2965`. T1 fixes all three (the fake's `teams` empty until T23 stores teams). Cost if wrong: a compile error in T1.
- **P25 Invites and approvals with teams are checked writes (race found):** a team grant changed between an invite's check and its write would hand the invitee an unchecked role chain (the team write's check doesn't include the invitee, who isn't a member yet). Pinned: `pub struct TeamSeen { pub team: Uuid, pub grants: Vec<(Uuid /*project*/, GrantRow)> }` in `members.rs`; `NewInvite.teams: &'a [TeamSeen]`, and `add_existing_user`, `approve_member` and `put_project_grants_checked` take `teams: &[TeamSeen]` (not `&[Uuid]`). Under the tenant lock the write re-reads each team's grants on non-deleted projects; any difference (or a team gone, or of another tenant) is `AccessWriteError::Changed` with nothing written. For an existing member, `expect` holds one `Observed` per project in (the invite's grant projects ∪ the teams' grant projects). The orchestrator retries `Changed` up to `CHECKED_WRITE_TRIES` on all four paths, then 409 `conflict`. The reconcile after an invite or approval covers the same union of projects. Cost if wrong: an invite racing a team grant change admits a LOGIN or owner-level chain.
- **P26 Who writes `team_members`:** only team writes, invites/approvals with teams and `remove_member`, each holding the tenant lock. So under the tenant lock a team's member set is stable, and `write_team_checked`'s re-read of the affected set can't race. T1's lock test asserts each holds the tenant row. Cost if wrong: a member added to a team between the team write's check and its write escapes the check.
- **P27 Tenant scoping of team grants:** the lateral (P4) also joins `projects tp ON tp.id = tg.project_id AND tp.tenant_id = tt.tenant_id AND tp.status <> 'deleted'`, so a crafted cross-tenant `team_grants` row is ignored like a crafted `team_members` row. `another_tenants_team_never_counts` inserts both. The macro's two arguments are string literals (`concat!`), e.g. `team_grants_lateral!("m.id", "g2.project_id")`. Cost if wrong: one more join.
- **P28 The SQL/Rust agreement test's domain:** `chelabase_member_level` returns NULL for a non-active member while `GrantSources::effective` doesn't look at status. The test compares, per row: for active members, SQL level = `effective().map(|e| e.platform_role)` (tenant admin: `tenant_admin_effective()`); for every row, `chelabase_personal_login_qualifies` = `access::qualifies(status == active, effective)`. The database is live and its project active; `offboarded_at` stays NULL (its own clause is step 1's, tested there). Cost if wrong: a test that can't pass.
- **P29 `check_effective` on `Effective`:** the held names are `RoleSet::Only` minus `PgRole::Owner`; at Admin (any shape) nothing is checked; `with_team(team, None)` removes that team's grant. `effective()` drops reserved names that `effective_on` kept, which `check_names` refuses earlier anyway. Cost if wrong: none (R-T2a showed agreement).
- **P30 The project grants list is an array:** `listProjectGrants` answers `Vec<ProjectGrant>` (`services/orchestrator/src/members.rs:3273`), so it can't gain `teams`. New route (T23): `LIST_PROJECT_TEAM_GRANTS`, GET `/v1/projects/{id}/team-grants`, `Project(Admin)`, op id `list_project_team_grants`, `Vec<ProjectTeamGrantView>` (P15). Cost if wrong: one route.
- **P31 MH-8 and `null`:** `#[serde(default)] team_ids: Option<Vec<Uuid>>` reads `"team_ids": null` as `None`, so `null` would pass. The project invite's field is `Option<serde_json::Value>` with `#[serde(default, deserialize_with = …)]` that maps any present value, `null` included, to `Some`, and `#[schema(value_type = Option<Vec<Uuid>>)]`; `Some(_)` is the 422. RED test `project_invite_refuses_team_ids` sends `[]`, `[id]` and `null`. Cost if wrong: `null` slips through (ignored, but MH-8 says refused).
- **P32 Studio search params live in `apps/studio/src/lib/members-search.ts`** (`MEMBERS_TABS`, `validateMembersSearch`, `validateMemberPageSearch`), not `lib/members.ts`. S1 adds `teams` to `MEMBERS_TABS` and `validateTeamPageSearch` (the back link's `view`, `q`). Team event words go in `lib/members.ts`. Cost if wrong: none.
- **P33 `TeamPicker` belongs to S2** (`apps/studio/src/components/members/team-picker.tsx`), so S1 and S2 touch disjoint files and run in parallel. S1's `teams/index.ts` exports `TeamsTab`, `TeamPage`, `teamName`. Cost if wrong: none.
- **P34 Studio before the API is merged (user's cut 1):** Studio imports the generated client (`packages/management-client/src/generated/endpoints.ts`), which exists only after api-gen over real annotations. So T23's coder makes, right after the RED commit, one commit `t23: API surface`: the route consts and `spec_table` rows, the request/response types, the handlers' `#[utoipa::path]` annotations with bodies answering 500 `internal`, the port methods, and api-gen's output; it pushes it and tells nothing (the coordinator watches the branch). S1 and S2 worktrees are cut from `members-hub-teams-t23` at that commit and merge into `members-hub-teams` after T23 has merged. Studio tests mock the generated functions, as step 1's do. A later change to a pinned shape is a deviation reported to the coordinator, with api-gen rerun. Cost if wrong: Studio drift, caught by typecheck after the merge and the end-of-step review.
- **P35 `tests/access_walk.rs` walks every endpoint:** T23 adds each new route's path and body fixture there (`{team_id}`, `{member_id}`) and keeps it green. GET_CELL's positive control may now reach the rest path (any non-access answer passes). Cost if wrong: a red walk.
- **P36 The rest stand-in:** `mock_rest_console` (`services/orchestrator/tests/api.rs:4292`) answers only `/internal/console/*/query` and the schema route; T5 extends it to `/internal/console/*/cell` and records the call. Cost if wrong: the T5 RED tests fail for the wrong reason.
- **P37 T4's empty member role:** today the reconcile skips a member when `granted_roles` is empty (`services/worker/src/member_roles.rs:1580`). With P18 that test moves to the full set, and the planner must accept a `Wanted` whose memberships are empty (create, keep, revoke-all). The test agent checks the planner code for that path before writing `a_viewer_only_member_keeps_a_member_role_without_memberships`; the code comment "The member role holds every effective role, Viewer-level ones included" goes. Cost if wrong: a Viewer's member role is dropped and the table read's CONNECT check fails.
- **P38 `affected_member_ids`** are JSON strings of lowercase hyphenated UUIDs (serde's), so `jsonb_build_array($5::text)` matches. `ProjectScope::apply` empties the array for a project log (P9). Cost if wrong: the member filter misses team events.
- **P39 `TeamWriteError`** derives `thiserror::Error`: every variant carries an `#[error("…")]` (the comments in the pinned enum are the meanings). Cost if wrong: none.
- **P40 Full-cell read through rest (Q1, option 2):** pinned under "Shared interfaces → Full-cell read through rest (T5)". Cost if wrong: see there.

## Decided by the user

- **Q1 The table editor's full-cell read under T2-2 (2026-10-27): option 2, through rest's console path.** `POST …/tables/{schema}/{table}/cell` (`GET_CELL`) is `ActAs(Viewer)` and ran on the SQL editor's session: the member role, then `SET LOCAL ROLE <role>` (`services/orchestrator/src/table_api/rows.rs:80-134`). Under T2-2 a role held below Developer can't be set from the member role, so such reads go through rest as `authenticator` + `SET ROLE`, with the console's per-role level check. Design: "Shared interfaces → Full-cell read through rest (T5)" (P40). Options not taken: refuse up front (403), or raise `GET_CELL` to `ActAs(Developer)`.
- **P12 (2026-10-27):** `team_ids` on both the tenant invite and the approve, confirmed.
- **Time cuts (2026-10-27):** Studio starts early against the pinned API (P34); audits only on T1 and T23; no comps task D and no comps pause; T2 and T3 merged into T23 (K after its RED commit); Studio tasks are one sonnet agent each.

## Task table (binding)

| Task | Scope | Test | Coder | Audit (review) | Wave | Depends |
|---|---|---|---|---|---|---|
| T1 | Metadata: migration (teams, team_grants, team_members, `chelabase_member_level`), `teams.rs` reads and checked writes with events, team-aware shared SQL in every grant-source read, `Observed.teams`, `TeamSeen` (P25), `touched_projects`, invite/approve/remove/reinstate team parts, log filter | opus | opus | opus | W1 | — |
| T4 | Worker T2-2: member-role memberships = `roles_at_least(Developer)`, existence unchanged (P18, P37), downgrade revokes and ends sessions; live tests compiled | opus | sonnet | none (end-of-step review) | W1 | — |
| T5 | Full-cell read through rest (Q1, P40): shared `chelabase_schema::cell` and `cell_failure`, rest's `POST /internal/console/{branch_id}/cell`, `GET_CELL`'s path choice, the rest stand-in (P36), `rest_live` test; api-gen | opus | sonnet | none (end-of-step review) | W1 (after T4, lane B) | — |
| T23 | Orchestrator teams (former T2 + T3): team API routes and handlers, R1/MH-34 read filters, union checks on team, grant, override and invite writes (P7, P25), I4 retry, `access_changed_for` on every team write, fakes with teams, the MH-4 property test; API-surface commit (P34); `team_ids` on tenant invite and approve, project invite refusal (MH-8, P31), roster `via` (P13), `GET /v1/members/{id}/teams` (P14), `GET /v1/projects/{id}/team-grants` (P15, P30), reinstate (P16), member log filter (MH-7), team-only-member route tests per MH-2 caller, `access_walk` fixtures (P35), `teams-e2e.sh`; api-gen | opus | opus | opus | W2 | T1, T5 merged |
| K | Docs: members-and-invites (teams), access reference (per-role levels, union, narrowing), SQL editor (T2-2, the full-cell read path), landing-highlights entry (P20) | — (one agent, sonnet) | — | none (leaf; end-of-step review) | W2 | T23's RED commit |
| S1 | Studio Teams tab and team page (route, cards/list, create/rename/delete, members checklist, project access cards without narrowing), `members-search.ts` (P32), team event words; unit tests for the logic it adds | — | one agent, sonnet | none (leaf; end-of-step review + Impeccable finish review) | W2 | T23's API-surface commit (P34) |
| S2 | Studio member page Teams section and locked "via team" rows and Effective line, role page Members "via team", `TeamPicker` (P33) in invite and approve, Copy from a member; unit tests for the logic it adds | — | one agent, sonnet | none (leaf; end-of-step review + Impeccable finish review) | W2 | T23's API-surface commit (P34) |

Reviews of their own: T1 and T23 (the audit stage). None: T4, T5 (the coordinator checks their RED diff), K, S1, S2 (leaves). The end-of-step review (opus) covers all of them.

### Waves (at most 2 pipelines and 3 agents at once; at most 3 builds or suites)

| Wave | Lane A | Lane B | Lane C | Claude time |
|---|---|---|---|---|
| W1 | T1 (pipeline) | T4 (pipeline), then T5 (pipeline) | — | 4.5 h |
| W2 | T23 (pipeline; its API-surface commit about 1 h in) | S1 (one agent) from the API-surface commit, then K (one agent; T23's RED commit exists by then) | S2 (one agent) from the API-surface commit | 6 h |
| End | review 1.5 h; final fix round 2.5 h; `just check` 0.7 h; restart + live pass 2.8 h; browser pass + Impeccable finish review 1 h | | | ≈ 8.5 h |

Total ≈ 19 h of Claude time (was ≈ 24 h). Agents at once: W1 at most 2; W2 at most 3 (T23's current stage, S1 or K, S2), with one pipeline. Each task runs in its own worktree, `just wt-add members-hub-teams-<task> members-hub-teams` (records the plan's stack in `.workbench-stack`), except S1 and S2, cut from `members-hub-teams-t23` at its API-surface commit (P34); each is merged back `--no-ff` (S1, S2 after T23) and removed. W1's files are disjoint (T1: metadata crate plus its call-site adaptations; T4: worker plus `sql_run_live.rs`; T5: `chelabase-schema/src/cell.rs`, `chelabase-types/src/table_api.rs`, rest, orchestrator `table_api/rows.rs`, `console.rs`, `tests/api.rs`, generated files); T1's adaptations never touch T5's files. In W2, S1 owns `components/members/teams/*`, the new route, `routeTree.gen.ts`, `members-page.tsx`, `lib/members.ts` and `lib/members-search.ts`; S2 owns `member-page/*`, `roles/role-page.tsx`, `invite-dialog.tsx`, `approve-dialog.tsx`, `grant-rows.tsx` and `team-picker.tsx`; K owns its docs; T23 touches none of them.

## Shared interfaces (pinned)

Implementers write these exactly. A change goes back to the coordinator.

### Migration (T1): `crates/chelabase-metadata/migrations/20261027000001_teams.sql`

```sql
-- Members hub step 2: live-linked teams (spec Part 5, D1, MH-1–MH-8).
CREATE TABLE teams (
  id uuid PRIMARY KEY,
  tenant_id uuid NOT NULL REFERENCES tenants(id),
  name text NOT NULL CHECK (name = btrim(name) AND char_length(name) BETWEEN 1 AND 80),
  description text NOT NULL DEFAULT '' CHECK (char_length(description) <= 500),
  created_by uuid REFERENCES members(id),
  created_at timestamptz NOT NULL DEFAULT now(),
  updated_at timestamptz NOT NULL DEFAULT now());
CREATE UNIQUE INDEX teams_tenant_name ON teams (tenant_id, lower(name));

CREATE TABLE team_grants (
  team_id uuid NOT NULL REFERENCES teams(id) ON DELETE CASCADE,
  project_id uuid NOT NULL REFERENCES projects(id),
  platform_role text NOT NULL CHECK (platform_role IN ('viewer','developer','admin')),
  pg_roles text[] NOT NULL DEFAULT '{}',
  CHECK (NOT ('owner' = ANY(pg_roles))),
  granted_by uuid REFERENCES members(id),
  created_at timestamptz NOT NULL DEFAULT now(),
  updated_at timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (team_id, project_id));
CREATE INDEX team_grants_project ON team_grants (project_id);

CREATE TABLE team_members (
  team_id uuid NOT NULL REFERENCES teams(id) ON DELETE CASCADE,
  member_id uuid NOT NULL REFERENCES members(id),
  added_by uuid REFERENCES members(id),
  added_at timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (team_id, member_id));
-- The member-side lookups (every grant-source read, `touched_projects`).
CREATE INDEX team_members_member ON team_members (member_id, team_id);

-- P5: the one level predicate, now over the own grant and the member's team grants (D1: the
-- platform level is the best source's), capped by the override. LEAST ignores NULL, so "no
-- source" is tested first.
CREATE OR REPLACE FUNCTION chelabase_member_level(p_member uuid, p_database uuid) RETURNS text
LANGUAGE sql STABLE AS $$
  SELECT CASE
    WHEN m.tenant_admin THEN 'admin'
    WHEN src.best IS NULL THEN NULL
    ELSE (ARRAY['viewer','developer','admin'])[least(src.best,
           coalesce(array_position(ARRAY['viewer','developer','admin'], o.platform_role), 3))]
  END
  FROM members m
  JOIN databases d ON d.id = p_database AND d.status <> 'deleted'
  JOIN projects p ON p.id = d.project_id AND p.tenant_id = m.tenant_id
  LEFT JOIN database_grant_overrides o ON o.member_id = m.id AND o.database_id = d.id
  CROSS JOIN LATERAL (
    SELECT max(array_position(ARRAY['viewer','developer','admin'], s.platform_role)) AS best
    FROM (SELECT g.platform_role FROM project_grants g
           WHERE g.member_id = m.id AND g.project_id = d.project_id
          UNION ALL
          SELECT tg.platform_role FROM team_members tm
            JOIN teams t ON t.id = tm.team_id AND t.tenant_id = m.tenant_id
            JOIN team_grants tg ON tg.team_id = tm.team_id AND tg.project_id = d.project_id
           WHERE tm.member_id = m.id) s) src
  WHERE m.id = p_member AND m.status = 'active'
$$;
```

### Metadata (`crates/chelabase-metadata`, T1)

`src/members.rs`:
- The shared select (P4): `grant_source_columns!()` gains `, t.t_grants`. Every query using it adds `team_grants_lateral!(<member expr>, <project expr>)`:
  ```sql
  LEFT JOIN LATERAL (
    SELECT coalesce(jsonb_agg(jsonb_build_object('team_id', tg.team_id,
             'platform_role', tg.platform_role, 'pg_roles', tg.pg_roles) ORDER BY tg.team_id),
           '[]'::jsonb) AS t_grants
    FROM team_members tm
    JOIN teams tt ON tt.id = tm.team_id
    JOIN members mm ON mm.id = tm.member_id AND mm.tenant_id = tt.tenant_id
    JOIN team_grants tg ON tg.team_id = tm.team_id AND tg.project_id = <project expr>
    WHERE tm.member_id = <member expr>) t ON true
  ```
- `grant_sources`, `database_grant_sources` (its `WHERE` adds `OR t.t_grants <> '[]'`), and `member_grant_sources` (every non-deleted tenant project with an own grant **or** a team grant of the member) all read teams. `listing_columns!`'s `has_access` counts a team grant on a non-deleted project.
- `pub struct Observed { pub project: Uuid, pub grant: Option<GrantRow>, pub overrides: BTreeMap<Uuid, OverrideRow>, pub teams: BTreeMap<Uuid, GrantRow> }` (`teams`: team id → that team's grant on `project`, for the member's teams). `observe_tx` fills `teams`.
- ```rust
  impl Observed {
      /// The sources on `database` (its override, if any), or at project level for `None`.
      pub fn sources(&self, database: Option<Uuid>) -> GrantSources;
      pub fn with_own(&self, grant: Option<GrantRow>) -> Observed;
      pub fn with_override(&self, database: Uuid, o: Option<OverrideRow>) -> Observed;
      pub fn with_team(&self, team: Uuid, grant: Option<GrantRow>) -> Observed;
  }
  ```
- `ProjectAccess` gains `pub teams: Vec<(Uuid /*team*/, Uuid /*member*/, GrantRow)>` (every member of every team with a grant on the project, by team then member), so `ProjectAccess::observed(project, member)` fills `teams`.
- `pub struct TeamSeen { pub team: Uuid, pub grants: Vec<(Uuid, GrantRow)> }` (P25): a team as the invite's check read it (its grants on non-deleted projects, by project id).
- `put_project_grants_checked(&self, actor: Uuid, member: Uuid, grants: &[(Uuid, GrantRow)], teams: &[TeamSeen], expect: &[Observed]) -> Result<Vec<(Uuid, bool)>, AccessWriteError>` (its current return type, unchanged). With non-empty `teams` it takes the tenant lock before the member (P6), re-reads the teams (any difference is `Changed`, P25), checks `expect` over the invite's and the teams' projects, and adds the memberships (`team_member_added` each). `put_project_grants` and `put_project_grant_checked` pass `&[]`.
- `NewInvite` gains `pub teams: &'a [TeamSeen]`; `add_existing_user(…, grants, teams: &[TeamSeen])`; `approve_member(actor, member, grants, teams: &[TeamSeen])`. Each re-reads the teams under the tenant lock as above (`Changed` on a difference, P25). A team gone or of another tenant is `Changed` too; the orchestrator checks ids first and answers 422 (T23).
- `touched_projects` adds `SELECT tg.project_id FROM team_members tm JOIN team_grants tg ON tg.team_id = tm.team_id WHERE tm.member_id = $1`.
- `remove_member` deletes the member's `team_members` rows after `touched_projects` (P10).
- `AccessAction` gains `TeamCreated`, `TeamUpdated`, `TeamDeleted`, `TeamGrantPut`, `TeamGrantDeleted`, `TeamMemberAdded`, `TeamMemberRemoved` (snake-case strings as named in P9).
- `access_events`' `member` filter: `e.target_member_id = $5 OR e.actor_member_id = $5 OR e.after -> 'affected_member_ids' @> jsonb_build_array($5::text) OR e.before -> 'affected_member_ids' @> jsonb_build_array($5::text)`.

`src/personal_logins.rs`:
- `pub enum GrantVia { Own, Team, Both }` and `pub struct MemberProjectRole { pub member: Uuid, pub project: Uuid, pub platform_role: PlatformRole, pub via: GrantVia }`; `member_project_roles(&self, tenant) -> Result<Vec<MemberProjectRole>, MetadataError>`, by member then project, the best level per pair.
- `reinstate_member(&self, actor, member) -> Result<Vec<Uuid>, AccessWriteError>`: the touched projects (also on the already-active no-op).

New `src/teams.rs`:
```rust
pub struct TeamRecord { pub id: Uuid, pub tenant_id: TenantId, pub name: String, pub description: String,
    pub created_by: Option<Uuid>, pub created_at: DateTime<Utc>, pub updated_at: DateTime<Utc> }
/// A team with its grants on non-deleted projects (by project id) and its members (by id).
pub struct TeamDetail { pub team: TeamRecord, pub grants: Vec<(Uuid, GrantRow)>, pub member_ids: Vec<Uuid> }
pub struct ProjectTeamGrant { pub team_id: Uuid, pub name: String, pub grant: GrantRow, pub member_ids: Vec<Uuid> }

pub enum TeamChange {
    PutGrant { team: Uuid, project: Uuid, grant: GrantRow },
    DeleteGrant { team: Uuid, project: Uuid },
    AddMember { team: Uuid, member: Uuid },
    RemoveMember { team: Uuid, member: Uuid },
    Delete { team: Uuid },
}

/// What a committed team write reaches: each affected member as (id, Zitadel user), by id, and
/// every project whose branches reconcile (the team's grant projects before and after), by id.
pub struct TeamWrite { pub changed: bool, pub affected: Vec<(Uuid, String)>, pub projects: Vec<Uuid> }

#[derive(Debug, thiserror::Error)]
pub enum TeamWriteError {
    NotFound,          // the team (or another tenant's)
    ProjectNotFound,   // not an active project of the tenant
    MemberNotFound,    // missing, another tenant's or removed
    MemberPending,     // a sign-up waiting for approval
    NameTaken,
    Changed,           // I4: an `expect` no longer holds, or the affected set changed
    Conflict(String),  // R7 (`owner`)
    Db(sqlx::Error),
}

impl MetadataClient {
    pub async fn list_teams(&self, tenant: TenantId) -> Result<Vec<TeamDetail>, MetadataError>;      // by lower(name)
    pub async fn team(&self, tenant: TenantId, team: Uuid) -> Result<Option<TeamDetail>, MetadataError>;
    pub async fn member_teams(&self, member: Uuid) -> Result<Vec<TeamDetail>, MetadataError>;       // by lower(name)
    pub async fn project_team_grants(&self, project: Uuid) -> Result<Vec<ProjectTeamGrant>, MetadataError>;
    pub async fn create_team(&self, actor: Uuid, tenant: TenantId, name: &str, description: &str) -> Result<TeamRecord, TeamWriteError>;
    pub async fn update_team(&self, actor: Uuid, tenant: TenantId, team: Uuid, name: Option<&str>, description: Option<&str>) -> Result<TeamRecord, TeamWriteError>;
    /// The check's read (I4): `(member, Observed)` for every affected member × affected project,
    /// by member then project, as they stand now (before the change).
    pub async fn team_change_observed(&self, tenant: TenantId, change: &TeamChange) -> Result<Vec<(Uuid, Observed)>, TeamWriteError>;
    /// Under the tenant lock, then the affected members' locks in id order (P6): re-reads
    /// `team_change_observed`; any difference from `expect` (the affected set included) is
    /// `Changed` with nothing written. Otherwise writes the change, its event (P9), then
    /// `revoke_unqualified_logins` for each affected member in id order, and commits.
    pub async fn write_team_checked(&self, actor: Uuid, tenant: TenantId, change: &TeamChange, expect: &[(Uuid, Observed)]) -> Result<TeamWrite, TeamWriteError>;
}
```
An unchanged write (same grant, member already in or not in, grant already absent) commits nothing and answers `changed: false`.

### Orchestrator (T23)

Routes (`crates/chelabase-routes/src/lib.rs`, each with its `spec_table` row and an `access_walk` fixture, P35). Op ids are snake case as listed; the generated client names them in camel case (`listTeams`, `useListTeams`, …), which Studio imports:

| Const | Method, path | Access | Op id |
|---|---|---|---|
| `LIST_TEAMS` | GET `/v1/teams` | `TenantMember` | `list_teams` |
| `CREATE_TEAM` | POST `/v1/teams` | `TenantAdmin` | `create_team` |
| `GET_TEAM` | GET `/v1/teams/{team_id}` | `TenantMember` | `get_team` |
| `UPDATE_TEAM` | PATCH `/v1/teams/{team_id}` | `TenantAdmin` | `update_team` |
| `DELETE_TEAM` | DELETE `/v1/teams/{team_id}` | `TenantAdmin` | `delete_team` |
| `PUT_TEAM_GRANT` | PUT `/v1/teams/{team_id}/grants/{project_id}` | `TenantAdmin` | `put_team_grant` |
| `DELETE_TEAM_GRANT` | DELETE `/v1/teams/{team_id}/grants/{project_id}` | `TenantAdmin` | `delete_team_grant` |
| `PUT_TEAM_MEMBER` | PUT `/v1/teams/{team_id}/members/{member_id}` | `TenantAdmin` | `put_team_member` |
| `DELETE_TEAM_MEMBER` | DELETE `/v1/teams/{team_id}/members/{member_id}` | `TenantAdmin` | `delete_team_member` |
| `LIST_MEMBER_TEAMS` | GET `/v1/members/{member_id}/teams` | `TenantMember` | `list_member_teams` |
| `LIST_PROJECT_TEAM_GRANTS` | GET `/v1/projects/{id}/team-grants` | `Project(Admin)` | `list_project_team_grants` |

Existing op ids in this API are camel case in `#[utoipa::path(operation_id = …)]` (e.g. `listProjectGrants`); the new ones follow the file (`listTeams`, `createTeam`, …, `listProjectTeamGrants`), and the snake-case names above are the `chelabase-routes` names, prefixed `orchestrator.` (`key_endpoint("orchestrator.list_teams", …)`, as `LIST_PROJECT_GRANTS` at `crates/chelabase-routes/src/lib.rs:749`).

API types (`services/orchestrator/src/teams.rs`, serde snake case):
```rust
pub(crate) struct Team { id: Uuid, name: String, description: String, member_count: usize,
    /// `GET_TEAM`, the writes and `list_member_teams` only; absent from `list_teams`.
    #[serde(skip_serializing_if = "Option::is_none")] member_ids: Option<Vec<Uuid>>,
    /// The caller's visible projects only (R1, P11), by project id.
    projects: Vec<TeamProjectAccess>, created_at: DateTime<Utc>, updated_at: DateTime<Utc> }
pub(crate) struct TeamProjectAccess { project_id: ProjectId, platform_role: PlatformRole,
    /// `null` unless the caller is a tenant admin or Admin on the project (P11, MH-34).
    pg_roles: Option<Vec<String>> }
pub(crate) struct CreateTeamRequest { name: String, #[serde(default)] description: String }
pub(crate) struct UpdateTeamRequest { #[serde(default)] name: Option<String>, #[serde(default)] description: Option<String> }
pub(crate) struct TeamGrantRequest { platform_role: PlatformRole, #[serde(default)] pg_roles: Vec<String> }
```
- `list_project_team_grants` answers `Vec<ProjectTeamGrantView { team_id: Uuid, name: String, platform_role: PlatformRole, pg_roles: Vec<String>, member_ids: Vec<Uuid> }>` (P15, P30); `list_project_grants` is unchanged.
- `MemberProjectAccess.via: AccessVia { Own, Team, Both }` (serde `own`/`team`/`both`; P13). Its doc comment ("always `own`") is rewritten.
- `CreateInviteRequest` and `ApproveMemberRequest` gain `#[serde(default)] team_ids: Vec<Uuid>` (an empty array is no teams); `CreateProjectInviteRequest` gains `team_ids` as P31 pins (any present value, `null` included, is 422 `invalid_request` "teams can be attached only by a tenant admin's invite").

`MetadataPort` (`services/orchestrator/src/lib.rs`) gains, delegating to the inherent methods of the same name (the file's rule at `:663`): `list_teams`, `team`, `member_teams`, `project_team_grants`, `create_team`, `update_team`, `team_change_observed`, `write_team_checked` (T23); `reinstate_member`'s and `member_project_roles`' new return types, and the new parameters of `put_project_grants`, `create_invited_member`, `add_existing_user` and `approve_member` (T1's signatures; T1 keeps the orchestrator compiling by passing `&[]` and mapping the new returns at the existing call sites, nothing more).

The check (`services/orchestrator/src/members.rs`, T23):
```rust
impl RoleContext {
    /// The invariant on each database (only `only`, when given) for the state `after` a write
    /// would leave: `after.sources(Some(db)).effective()`; nothing to check when `None` or at
    /// Admin; otherwise each named role's chain on every branch, as before.
    fn check_effective(&self, after: &Observed, only: Option<Uuid>) -> Result<(), ApiError>;
}
```
Every own-grant, override, invite and team write builds `after` from the `Observed` it read (`with_own`, `with_override`, `with_team`) and passes the read as its `expect`. Team writes use one `RoleContext::project` per affected project and add `member_id` and `team_id` to a refusal's extras. Every team write, after its commit and when `changed`, calls `access_changed_for(state, tenant, &write.affected, write.projects)`.

### Full-cell read through rest (T5; Q1 option 2, P40)

**Path choice (orchestrator, `get_cell` in `table_api/rows.rs`):** after `load_authorized` (as today), with `role: None` the read stays on today's path (the owner's session). With a named role it stays on today's member-role path exactly when the member role can `SET ROLE` to it after T2-2, which is the console's write predicate: the role is held at Developer or above, or reached (R21) through the chain of a role held there. That predicate moves out of `console.rs`'s private `may_write` into `pub(crate) async fn at_developer(state, authorized: &Authorized, role: Option<&str>, branch: Uuid) -> Result<bool, ApiError>` (same body), used by both the console's write gate and `get_cell`. Otherwise (below Developer) the read goes to rest. One path for everything was rejected: it would make Developer full-cell reads depend on the data API being on and drop R5's hand-over, for no gain. Cost if wrong: a role reached through a Viewer-level chain is sent to rest, which reads it as that role anyway; the answer is the same.

**The rest path (orchestrator):** the branch must be ready and not a backup (409 `branch_not_ready` / `backup_branch`, as the console); the Postgres database is picked with `console::pick_database` over `branch_databases` (`database: None` = main) and must have `data_api == On`, else 409 `data_api_disabled` (the grid itself reads rows through the console, `apps/studio/src/components/table-editor/rows-view.tsx:152`, so Studio never asks otherwise); `console::check_role` (the worker's check: exists, not owner-level; grants the role to `authenticator`); then `RestInternal::cell(branch, &ConsoleCellRequest, timeout)` with timeout `CELL_STATEMENT_TIMEOUT_MS + 15 s`, and the answer passed on by `console::pass_on` (status, body, `Retry-After`; `unknown_database` re-read as there). The audit line (`"cell read"`) is logged on both paths. No member-role session is opened, so no `hand_over_after`.

**rest:** new internal route `POST /internal/console/{branch_id}/cell` (internal listener, bearer-checked like `/query`; registered in `internal_router`). Body (`deny_unknown_fields`):
```rust
pub struct ConsoleCellRequest { pub role: String, #[serde(default)] pub database: Option<String>,
    pub schema: String, pub table: String, pub key: BTreeMap<String, Value>, pub column: String }
```
`Engine::console_cell(&self, branch: Uuid, request: &ConsoleCellRequest, request_id: &str) -> Result<CellValue, Failure>`: the database picked as `console` does (served, else `unknown_database`), the connection leased and guarded with `Guard::Console(role)` (exists, not owner-level), then `chelabase_query::console_session(…, RunAs::Role { role, claims: {}, oid: None }, settings, …)` with `statement_timeout_ms = CELL_STATEMENT_TIMEOUT_MS` (15 000, today's `GET_CELL` timeout; also the transaction timeout through the preamble) and the branch's `max_rows` (unused: a primary-key equality reads at most one row). Inside: `SET LOCAL transaction_read_only = on` first (today's path is `BEGIN READ ONLY`), then today's steps: the table model through `catalog::table_sql`, `cell_query`'s shape checks, the key columns' unmodified types (`format_type(oid, NULL)`), the cell query with text parameters, the 64 MiB checks. 200 answers `CellValue` (`chelabase_types::table_api`, unchanged). Every in-database or shape failure answers the **management** `ErrorEnvelope` (`chelabase_types::api_error::ApiError`), built by the same shared code as the member-role path, so both paths answer alike: 400 `invalid_request`/`unknown_column`/`constraint_violation`, 403 `permission_denied` (42501, e.g. a column the role can't select), 404 `not_found` (no table, no row), 413 `too_large`, 504 `timeout`. Connection failures answer as `internal_failure` does today (404 `unknown_branch`, 409 `{status}`, 503).

**Shared code (moved, not rewritten):** `crates/chelabase-schema/src/cell.rs` (new, `pub mod cell`): `primary_key`, `key_text`, `cell_query` (from `rows.rs`, unchanged, returning `Result<(String, Vec<String>), ApiError>`), `MAX_CELL_BYTES`, `CELL_STATEMENT_TIMEOUT_MS`, `pub const KEY_TYPE_SQL` (the `format_type` query) and `pub fn decode_cell(value: Option<String>, over: bool, bytes: Option<i32>, data_type: String) -> Result<CellValue, ApiError>` (the 64 MiB backstop). `crates/chelabase-types/src/table_api.rs` gains `pub fn cell_failure(sqlstate: &str, message: &str, detail: Option<&str>, hint: Option<&str>, position: Option<u32>) -> ApiError`: 42P01/3F000 → 404 `not_found` "no such table", 54000 → 413 `too_large`, otherwise `classify_failure` (already there) with today's extras (`op_index: null`, `sqlstate`, `message`, `detail`, `hint`, `position`). `rows.rs`'s `row_failure` and the 54000 branch call it; a lost connection stays the orchestrator's 503 `unavailable`. rest adds `chelabase-schema` (workspace crate) to `[dependencies]`; it pulls `pg_query`, already built in the workspace. `RestInternal::cell` (orchestrator `console.rs`) posts `ConsoleCellRequest` as JSON (field names above).

**OpenAPI:** `get_cell`'s description and responses gain: below Developer on the role, the read runs as the role through the data API's connection, so it needs the data API on (409 `data_api_disabled`), and the worker's role check applies (403 `role_refused`). api-gen (P19).

**Cost if wrong:** two answer shapes for one route if the shared builder is bypassed (the contract test below catches it); a Viewer full-cell read on a database without the data API is 409 (the grid can't show rows there anyway).

### Worker (T4)

`services/worker/src/member_roles.rs`, in the reconcile's per-member loop (`:1569-1587` today):
```rust
let (all, _) = granted_roles(&Grant { platform_role: effective.platform_role, roles: effective.roles.clone() }, &catalog);
if all.is_empty() { continue; }                              // existence unchanged (P18)
let held = Grant { platform_role: effective.platform_role, roles: effective.roles_at_least(PlatformRole::Developer) };
let (roles, skipped) = granted_roles(&held, &catalog);       // T2-2: memberships, possibly empty
```
The existing planner shrinks the member role to `roles` and ends its sessions on any revoke (`member_roles.rs:43-45`). The lock marker (`locked`) is unchanged.

### Studio (S1, S2)

- Route `/app/members/teams/$teamId` (`app.members_.teams.$teamId.tsx`, `validateSearch: validateTeamPageSearch`), back link "← Teams" to `/app/members?tab=teams&view=…&q=…`. `MEMBERS_TABS` in `lib/members-search.ts` gains `teams` (P32); the Teams tab is shown to every member (Part 1).
- `components/members/teams/index.ts` (S1) exports `TeamsTab`, `TeamPage`, `teamName(team)`. `components/members/team-picker.tsx` (S2, P33) exports `TeamPicker` (a filterable checklist of teams over `useListTeams`, value `string[]` of team ids).
- Generated client names (from the op ids): `listTeams`/`useListTeams`, `getTeam`/`useGetTeam`, `createTeam`, `updateTeam`, `deleteTeam`, `putTeamGrant`, `deleteTeamGrant`, `putTeamMember`, `deleteTeamMember`, `listMemberTeams`/`useListMemberTeams`, `listProjectTeamGrants`/`useListProjectTeamGrants`; types `Team`, `TeamProjectAccess`, `ProjectTeamGrantView`, `CreateTeamRequest`, `UpdateTeamRequest`, `TeamGrantRequest`, `AccessVia` (`own | team | both`). `pg_roles` is `string[] | null`; `member_ids` is optional on `Team` (absent from `listTeams`).
- Locked rows reuse `RoleChecklist`'s `locked: { value, via }[]` (`hub/role-checklist.tsx:14`); `via` is the team's name, rendered through `escapeName`.

## Pre-flight scan (2026-10-27)

- **What step 1 left for step 2 (checked against the code):**
  - `GrantSources::new(own, teams, database)` and `effective()` already union teams with per-role levels and `All { plus }` (`crates/chelabase-types/src/access.rs:144-227`). No type change is needed.
  - The metadata builds `teams` empty in one place, `grant_sources_of` (`crates/chelabase-metadata/src/members.rs:651-663`). The readers using it are `grant_sources` (`:1382`), `database_grant_sources` (`:1406`) and `member_grant_sources` (`:1483`). `member_grant_sources` JOINs `project_grants`, so it must become "own or team".
  - Admission already reads `member_grant_sources` (`services/orchestrator/src/access.rs:258-266`, T2-4 in step 1's final round), as do the project list (`projects.rs:202`), `/me` (`lib.rs:2529`), realtime (`internal/realtime.rs:418`) and `visible_projects` (`members.rs:1199`). The worker reads `database_grant_sources` (`services/worker/src/member_roles.rs:1489`). These change only through the SQL.
  - `access_changed_for` exists (`services/orchestrator/src/members.rs:1298`) and says "step 2's team writes will call it too".
  - `chelabase_member_level` (`migrations/20261023000001_members_hub.sql:62`) and `chelabase_personal_login_qualifies` (redefined in `…000002`, calls the former by name).
  - `effective_on` (`members.rs:1419`) and `check_effective(grant, overrides, only)` (`:1588`) are own-grant only.
  - The "via team" slot is `RoleChecklist`'s `locked` prop (`apps/studio/src/components/members/hub/role-checklist.tsx:14,66-105`).
- **Readers that still bypass the shared shape:** `member_projects` (own grants; its only non-default caller is reinstate, `members.rs:2857`, P16), `member_project_roles` (`personal_logins.rs:1033`, P13), `listing_columns!`'s `has_access` (`members.rs:694`), `touched_projects` (`members.rs:1005`) and `ProjectAccess`/`observe_tx` (`:1680`, `:438`). T1 switches every one.
- **Member-role sessions (T2-2):** only `RUN_SQL`, `GET_SQL_SCHEMA` (Developer), `APPLY_SCHEMA_CHANGES` (Developer/Admin), `INSERT_DEFAULT_ROW` (Developer) and `GET_CELL` (**Viewer**) open a member-role session (`open_session` callers: `sql_editor/routes.rs:854,1040`, `table_api/rows.rs:92`, `table_api/changes.rs:1247`). Hence Q1 (decided: T5). The table API read connects as `cb_m_<id>` for its CONNECT check only (`table_api/read.rs:217`), hence P18.
- **Seam pass (2026-10-27), checked against the code:**
  - rest's console route (`services/rest/src/app.rs:122,360`; `Engine::console`, `services/rest/src/exec.rs:410`) runs a tree as the role through `Guard::Console` and `chelabase_query::run`; the cell read isn't an `Op`, so T5 uses `chelabase_query::console_session` (`crates/chelabase-query/src/engine.rs:424`, already used by realtime). The console's 16 MiB response budget doesn't apply to it (64 MiB cap kept). `check_role` (`services/orchestrator/src/console.rs:937`) grants the role to `authenticator`. The console's per-role check is `may_write` (`console.rs:422`). `classify_failure` is in `chelabase-types` (`table_api.rs:27`), so rest can answer management errors.
  - `listProjectGrants` answers an array (P30); `CreateProjectInviteRequest` has no `deny_unknown_fields` (P31).
  - `GrantSources::effective`, `Effective::level_of`/`roles_at_least` and `access::qualifies(active, effective)` (`crates/chelabase-types/src/access.rs:176,249,261,291`) match T1's SQL once P28's domain is used. `chelabase_personal_login_qualifies` (`…000002_members_offboarded.sql`) adds the offboarded and deleting-project clauses.
  - The access log's project filter already matches `before/after -> 'grants'` by `project_id` (`members.rs:2671-2673`), so P9's team events land in project logs; the member filter is `$5` (`:2674`).
  - Studio: `RoleChecklist.locked` (`hub/role-checklist.tsx:14`) is in place; the tab enum is in `lib/members-search.ts` (P32); the generated client is `packages/management-client/src/generated/endpoints.ts` (P34).
  - Lock order (P6) across every writer: team writes, invites/approvals with teams and `remove_member`/`suspend_member`/`reinstate_member` take tenant → member(s); own-grant and override writes take member, then `KEY SHARE` on the tenant; personal-login records come last (`revoke_unqualified_logins`). No cycle. `team_members` writers all hold the tenant (P26).
  - Reconcile triggers: every team write → `access_changed_for(affected, projects)` with the team's grant projects before ∪ after; invites/approvals with teams → their grants' projects ∪ the teams' (P25); reinstate → `reinstate_member`'s projects (P16); the worker reads `database_grant_sources` (`services/worker/src/member_roles.rs:1489`), which T1 makes list team-only members.
- **Lock order today:** `crates/chelabase-metadata/src/members.rs:5-10` (tenant → members → invites; grant writes lock only the member and take `KEY SHARE` on the tenant). P6 extends it without a cycle.
- **Invites:** three writers carry grants (`create_invited_member` `:2300`, `add_existing_user` `:1726`, `put_project_grants_checked` for an existing member through `invite` in `services/orchestrator/src/members.rs:1871`), plus `approve_member` (`:1791`). `CreateProjectInviteRequest` has no `deny_unknown_fields` (`members.rs:888`), hence P12's explicit field.
- **Project log scope:** `ProjectScope::apply` (`services/orchestrator/src/members.rs:2353-2392`) filters `grants`, `overrides`, `logins`, `rotated_branches`; P9 adds `affected_member_ids`.
- **e2e:** `members-e2e`, `personal-logins-e2e`, `sql-editor-e2e`, `role-e2e`, `data-e2e` exist (`justfile:261-310`); none touches teams. T23 adds `teams-e2e`.
- **Disjoint files per wave:** W1: T1 = `crates/chelabase-metadata/**` plus the call-site adaptations listed under `MetadataPort`; T4 = `services/worker/src/member_roles*`, `services/worker/tests/member_roles_live.rs`, `services/orchestrator/tests/sql_run_live.rs`; T5 = the files under "Full-cell read through rest". T1's call-site adaptations touch `services/orchestrator/src/{members.rs,lib.rs,openapi.rs}`, `services/orchestrator/tests/common/fakes.rs` and `services/worker/src/{ports.rs,fakes.rs,rt_events/delegate.rs}` only if a port signature changes; T4 and T5 must not edit those, and if one needs to, it waits for T1's merge. W2: listed under "Waves".

---

## T1: metadata for teams (opus / opus / opus)

**Worktree:** `.worktrees/members-hub-teams-t1` from `members-hub-teams`.

**Files:** the migration (P1); `crates/chelabase-metadata/src/{teams.rs (new), lib.rs, members.rs, personal_logins.rs}`; `crates/chelabase-metadata/tests/members_live.rs`; the compile-only call-site adaptations listed under "`MetadataPort`".

**Behaviour:** spec Part 5 "Data", "Effective access" (the SQL side), "Writes" (the transaction half of MH-4/MH-5), "Log", "Triggers"; P1–P6, P9, P10, P12 (metadata half), P13, P16, P24–P28, P38, P39 as pinned. Team writes don't validate role chains (the orchestrator does, with `expect`); they check tenancy, R7, pending members and `expect`.

**Checklist (red-team "Teams / access union", metadata part):**
- empty sources stay `None`: a member with only an override has no level (P5's NULL guard) and no `member_grant_sources` row;
- every reader in MH-2's list goes through the shared shape (P4), and teams of another tenant never count (a crafted cross-tenant `team_members` row is ignored by every read);
- team writes: tenant lock, then members in id order; `expect` mismatch (including a changed affected set) is `Changed` with nothing written (MH-4b);
- every team write that can make `chelabase_personal_login_qualifies` false revokes in the same transaction (MH-9 via P3): a grant downgraded to Viewer, a grant deleted, a member removed from a team, a team deleted;
- team events list affected members; the member filter finds them; the project filter finds grant events through `grants` (MH-7);
- `remove_member` drops memberships and its touched projects include team projects (MH-5);
- deleted projects' team grants are no access; a suspended or offboarded member reaches nothing through a team;
- an Admin team grant names no roles (R17); `owner` in a team grant is `Conflict` (R7).

**RED tests (test agent).** `tests/members_live.rs` (scratch database, by name) unless noted; first failure: compile (the team API is missing) or the assertion named:
- `member_level_agrees_with_rust_over_teams`: exhaustive over tenant admin × status {active, suspended} × own {none, V, D, A} × two teams each {none, V, D, A} × override {none, V, D, A, roles-only}; compared as P28 pins (level for active members, qualifies for every row; the override-only rows catch the `LEAST` trap).
- `grant_sources_include_team_grants`, `database_grant_sources_list_team_only_members`, `member_grant_sources_list_team_only_projects`, `has_access_counts_team_grants`, `member_project_roles_report_own_team_and_both`.
- `another_tenants_team_never_counts` (raw cross-tenant `team_members` and `team_grants` rows inserted by the test, P27).
- `an_invite_with_teams_sees_a_team_grant_change_as_changed` (P25: `create_invited_member`, `add_existing_user`, `approve_member` and `put_project_grants_checked` each with a `TeamSeen` read before a team grant put → `Changed`, nothing written).
- `every_team_members_writer_holds_the_tenant` (P26: while a second session holds the tenant row `FOR NO KEY UPDATE`, each writer of `team_members` blocks before writing).
- `team_writes_revoke_logins_that_stop_qualifying` (downgrade to Viewer, grant delete, member removed from team, team delete; verifier NULL in the same transaction, one `personal_logins_revoked` each).
- `a_team_write_refuses_a_stale_expect_with_changed` and `a_team_write_whose_affected_set_changed_is_changed`.
- `an_own_grant_write_sees_a_team_change_as_changed` (`put_project_grants_checked` with an `expect` read before a team grant put).
- `team_writes_lock_the_tenant_then_members_in_id_order`: deterministic interleaving, as step 1's T3-I1 tests did: a second session holds member B's row; the team write blocks on B only after holding the tenant and A (read from `pg_locks`/`pg_blocking_pids`).
- `team_events_carry_affected_members_and_the_member_filter_finds_them`; `team_grant_events_are_in_the_project_log`.
- `remove_member_drops_team_memberships_and_touches_team_projects`; `reinstate_returns_team_projects`.
- `team_names_are_unique_per_tenant_case_insensitively`; `a_team_grant_on_another_tenants_project_is_project_not_found`; `adding_a_pending_member_is_member_pending`; `an_admin_team_grant_names_no_roles`.
- `invites_and_approvals_add_teams_in_the_same_transaction` (`create_invited_member`, `add_existing_user`, `approve_member`, `put_project_grants_checked` with `teams`).
- `a_deleted_projects_team_grant_is_no_access`; `deleting_a_team_cascades_and_logs_one_event`.
- Unit (`src/teams.rs`): `team_change_affected_sets` (which members and projects each `TeamChange` reaches, pure over a fixture).

**Gotchas:**
- `LEAST`/`GREATEST` ignore NULL (P5).
- `jsonb_agg` over no rows is NULL: `coalesce(…, '[]')` (P4).
- `team_members.member_id` references `members(id)` with `KEY SHARE`, which `FOR NO KEY UPDATE` member locks don't block. Never lock members `FOR UPDATE` (module docs, `members.rs:5-10`).
- `put_project_grants_checked` gains the tenant lock only when `teams` is non-empty; the plain grant path keeps its member-only lock.
- `revoke_unqualified_logins` must run after the change is written, so the predicate sees it.
- The orchestrator's and worker's fakes build `GrantSources` themselves; T1 doesn't touch them beyond compiling.

**Verify:**
- `cargo nextest run -p chelabase-metadata --lib`;
- `cargo test -p chelabase-metadata --test members_live --no-run`, then the new tests by name through `just --justfile <wt>/justfile --working-directory <wt> workbench test -p chelabase-metadata --test members_live -- --run-ignored only <names>`;
- `cargo clippy -p chelabase-metadata -p orchestrator -p worker --all-targets -- -D warnings`;
- `cargo test -p orchestrator --no-run` and `cargo test -p worker --no-run` (call sites compile).

**Live runs (the plan's one stack):** only T1's new `members_live` tests, by name (each makes its own scratch database: no shared state, no conflict). The whole file runs at End of step 5.

---

## T4: the hidden member role holds only Developer-level roles (opus test / sonnet coder / no audit)

**Worktree:** `.worktrees/members-hub-teams-t4` from `members-hub-teams`.

**Files:** `services/worker/src/member_roles.rs`, `services/worker/src/member_roles/tests.rs`, `services/worker/tests/member_roles_live.rs`, `services/orchestrator/tests/sql_run_live.rs`.

**Behaviour:** T2-2, P18 and P37 as pinned (the full-cell read is T5's). The member role's memberships are the safe set of `roles_at_least(Developer)`. A role that drops below Developer (a team grant downgraded to Viewer, an override cap, an own grant downgraded) is revoked from an existing member role at the next reconcile, and the revoke ends the role's sessions, as any shrink does today. Personal logins are unchanged (they already use `roles_at_least(Developer)`).

**Checklist:**
- a Viewer-level-only member keeps a member role with no memberships (P18), and the table API read still connects (`read.rs`'s `NoLogin` path never loops);
- an Admin (`All { plus }`) keeps every safe role; `plus` names survive only at Developer+;
- the lock marker on protected branches is unchanged (MH-16);
- a step-1 deployment's existing member roles holding Viewer-level roles lose them at the first reconcile (no migration needed);
- R21 chains: a Developer-level role whose chain reaches a Viewer-level role is still granted (its chain is its own power).

**RED tests (test agent):**
- `services/worker/src/member_roles/tests.rs` (fakes; first failure: the assertion):
  - `member_role_holds_only_roles_at_developer_or_above` (own Developer `{a}`, team Viewer `{b}` → memberships `{a}`);
  - `a_role_downgraded_below_developer_is_revoked_and_its_sessions_end` (catalog has the member role in `b`; desired drops `b`; the plan revokes `b` and ends sessions);
  - `a_viewer_only_member_keeps_a_member_role_without_memberships`;
  - `reconcile_grants_a_team_only_member` (`database_grant_sources` gives only a team grant).
- `services/worker/tests/member_roles_live.rs`: `a_team_role_downgraded_to_viewer_leaves_the_member_role` (compiled `--no-run`; runs at End of step 5).
- `services/orchestrator/tests/sql_run_live.rs`: `the_sql_editor_cannot_set_a_viewer_level_role` (a script `RESET ROLE; SET ROLE b` fails; compiled `--no-run`; End of step 5).

**Gotchas:** `granted_roles` drops `skipped` reporting for `All`; keep the debug log over `skipped` from the memberships call. `applied_roles` must record the memberships, not the full set, or the next reconcile sees no drift. P37: the planner path for a member role with no memberships.

**Verify:** `cargo nextest run -p worker --lib`; `cargo clippy -p worker --all-targets -- -D warnings`; `cargo test -p worker --test member_roles_live --no-run`; `cargo test -p orchestrator --test sql_run_live --no-run`.

**Live runs:** none in the task. Both live tests run at End of step 5 (they need this step's worker and orchestrator on the stack).

---

## T5: full-cell read through rest (opus test / sonnet coder / no audit)

**Worktree:** `.worktrees/members-hub-teams-t5` from `members-hub-teams` (after T4 merged; lane B). Merged before T23 starts (P19).

**Files:** new `crates/chelabase-schema/src/cell.rs` (+ `lib.rs`'s `pub mod cell`); `crates/chelabase-types/src/table_api.rs`; `services/rest/Cargo.toml`, `services/rest/src/{app.rs, exec.rs}`, `services/rest/tests/rest_live.rs`; `services/orchestrator/src/{table_api/rows.rs, console.rs}`; `services/orchestrator/tests/api.rs` (the stand-in, P36, and the new tests); generated files.

**Behaviour:** "Shared interfaces → Full-cell read through rest (T5)" as pinned (P40). The member-role path's answers don't change for a Developer-level role or the owner.

**Checklist (access control):**
- the path choice is the console's predicate, read once from the same `Authorized` the access check returned (no second `authorize`); `role: None` never goes to rest;
- a role reached only through a Viewer-level chain goes to rest and reads as that role (Postgres decides the privileges);
- an owner-level role, the database owner's name, a reserved name, a role not in the caller's set: refused as today (`authorize`, `invalid_role`, `check_role`), before rest is called;
- rest's route refuses a missing or wrong bearer (401) and unknown fields (400 `invalid_query`, as `/query`'s body errors);
- the read is read-only (`transaction_read_only`), times out at 15 s (504 `timeout`), caps at 64 MiB (413) on both paths;
- column privileges: a role with `SELECT (id)` only reading another column is 403 `permission_denied` with the same body on both paths;
- rest unreachable is 503 `unavailable`; a database whose data API was turned off since the check is 409 `data_api_disabled` (`pass_on`'s re-read).

**RED tests (test agent).** First failure: compile (the moved items, the new route) or the assertion named:
- `crates/chelabase-schema/src/cell.rs` unit: the three `cell_query` tests move here from `rows.rs` unchanged (`cell_query_binds_text_and_quotes_names`, `cell_query_caps_the_text_form_server_side`, `cell_query_refuses_bad_keys_and_columns`), plus `decode_cell_refuses_over_the_cap`.
- `crates/chelabase-types/src/table_api.rs` unit: `cell_failure_maps_like_the_member_role_path` (42P01, 3F000 → 404; 54000 → 413; 42501 → 403 `permission_denied`; 22P02 → 400 `constraint_violation`; 57014 → 504; extras present).
- `services/rest/src/app.rs` unit: `console_cell_needs_the_bearer_and_refuses_unknown_fields`.
- `services/orchestrator/tests/api.rs` (fakes, the stand-in extended): `get_cell_below_developer_goes_through_rest` (a Viewer team role: the stand-in gets `/internal/console/{branch}/cell` with `{role, database, schema, table, key, column}`; no compute lookup for a member-role session), `get_cell_at_developer_keeps_the_member_role_path` (no rest call), `get_cell_as_the_owner_never_calls_rest`, `get_cell_through_rest_needs_the_data_api` (409 `data_api_disabled`), `get_cell_through_rest_passes_its_answer_on` (a 403 `permission_denied` body and a 503 with `Retry-After` come back unchanged).
- `services/rest/tests/rest_live.rs` (live, by name in the task): `console_cell_reads_as_the_role_and_refuses_a_column_it_lacks` (a role `viewer_r` with `SELECT (id)` on a table: reading `id` is 200 with `CellValue`; reading `body` is 403 `permission_denied` with `sqlstate: "42501"`; a missing row is 404 `not_found`; a value over the cap is 413).
- The team scenario end to end (a Viewer-level **team** role reading a full cell, refused a column it lacks) is a step of `teams-e2e.sh` (T23), run at End of step.

**Gotchas:** `mock_rest_console` answers only `/query` today (P36). `console_session` commits (`with_transaction`); read-only comes from `SET LOCAL transaction_read_only = on`, not from a rollback. The preamble needs `chelabase.schema_version`, which every data-API database has. Keep `rows.rs`'s existing live tests (`default_rows_and_full_cells_over_http` and friends, `tests/api.rs:13660-13770`) green: they run as the owner or at Developer.

**Verify:** `cargo nextest run -p chelabase-schema --lib`; `cargo nextest run -p chelabase-types --lib`; `cargo nextest run -p rest --lib`; `cargo nextest run -p orchestrator --test api` (non-live); `cargo test -p rest --test rest_live --no-run`; `cargo clippy -p chelabase-schema -p chelabase-types -p rest -p orchestrator --all-targets -- -D warnings`; api-gen last.

**Live runs:** the new `rest_live` test by name (`just --justfile <wt>/justfile --working-directory <wt> workbench test -p rest --test rest_live -- --run-ignored only console_cell_reads_as_the_role_and_refuses_a_column_it_lacks`): it serves rest in-process and provisions its own database, so it conflicts with nothing on the stack. The whole `rest_live` file runs at End of step 5.

---

## T23: orchestrator teams (opus / opus / opus; former T2 and T3)

One pipeline: one test agent writes both parts' RED tests in one commit (`t23: RED tests`), one coder makes them green (its first commit after RED is the API surface, P34), one audit. The two parts below are its brief.

**Worktree:** `.worktrees/members-hub-teams-t23` from `members-hub-teams` after T1 and T5 merged.

### Part A: team API and union checks

**Files:** `crates/chelabase-routes/src/lib.rs` (the nine team endpoints and `spec_table` rows); `services/orchestrator/src/{teams.rs (new), members.rs (check_effective on Observed, every checked write's `after`), lib.rs (port methods, router), openapi.rs}`; `services/orchestrator/tests/{common/fakes.rs (team storage; `grant_sources` and friends union them), teams.rs (new), access_walk.rs (P35)}`; generated files.

**Behaviour:** spec Part 5 "Writes", "Visibility", "Triggers"; Part 7's Teams row; P2, P3, P6–P9, P11, P17, P29, P34, P35. Each team write handler: authorize → load the team (404) → `check_names` on a grant's roles (the project's context: reserved names, owners, personal logins, S12) → `team_change_observed` → build each `after` with `with_team` → `check_effective` per member and project → `write_team_checked` with the read as `expect`, retried on `Changed` up to `CHECKED_WRITE_TRIES` → when `changed`, `access_changed_for(affected, projects)` → answer. Own-grant, override and existing-invite writes switch to `check_effective(&after, …)` on the union (MH-4: "override writes validate against the union").

**Checklist (red-team "Teams / access union", orchestrator part):**
- `check_effective` on union ∩ override for every affected member and database; 422 names the member, the database and the team (MH-4a, its exact scenario: own `Developer {a}`, override on D `{Viewer, [x]}` with `x` LOGIN, then a team Admin grant → refused);
- every team write fires `access_changed` per affected member, the proxy-cache publish for their logins and `reconcile_projects` over the affected projects (MH-5), and nothing when unchanged;
- team reads apply R1 and P11 (MH-6); counts over visible projects;
- a suspended or offboarded caller is 403 as on every route (P14 of step 1);
- an empty team, a team with no grants, and deleting a team with both;
- `owner`, reserved names and personal-login names refused in team grants (R7, MH-21);
- the property test: any sequence of own-grant, override and team writes leaves every member's effective grant on every database passing `check_effective`.

**RED tests (test agent).** `tests/teams.rs` with fakes; first failure: 404 (route missing) or compile:
- `team_routes_by_power`: row by row (tenant admin; project Admin; Developer; Viewer; suspended) × (each read allowed, each write refused), per Part 7's Teams row.
- `team_reads_hide_projects_the_caller_cannot_see`; `team_pg_roles_are_shown_only_to_admins_of_the_project`.
- `a_team_grant_is_checked_on_the_union_for_every_member_and_database` (MH-4a scenario); `adding_a_member_is_checked_on_the_union`; `an_override_write_is_checked_on_the_union`; `an_own_grant_write_is_checked_on_the_union`.
- `team_grants_refuse_owner_reserved_and_personal_login_names`.
- `every_team_write_fires_realtime_cache_and_reconcile` (each `TeamChange`, asserting the fake `CachePort` record, realtime calls and the enqueued branches; an unchanged write fires nothing).
- `a_changed_team_write_is_retried_then_conflict`.
- `team_name_taken_is_409`; `a_pending_member_is_409`; `unknown_team_project_or_member_is_404`.
- `every_write_sequence_keeps_the_invariant` (proptest over the fakes: one project, two databases, roles `a`, `b` NOLOGIN and `x` LOGIN, two members, two teams; random own-grant, override and team writes; after each accepted write, every member × database passes the check).

**Gotchas:**
- `RoleContext::project` reads every database and branch: build it once per affected project per request.
- `ProjectScope` (`members.rs:2353`) must drop `affected_member_ids` for a project log (P9); a test in `api.rs` exists for its other fields, keep it green.
- `AppState` fakes (`tests/common/fakes.rs`) record cache publishes; wait on the record, never sleep (step 1 S19).
- api-gen in the API-surface commit (P34) and last (P19).

### Part B: invites, roster and reads around teams

**Files:** `crates/chelabase-routes/src/lib.rs` (`LIST_MEMBER_TEAMS`, `LIST_PROJECT_TEAM_GRANTS`); `services/orchestrator/src/{members.rs, teams.rs, lib.rs, openapi.rs}`; `services/orchestrator/tests/api.rs`; new `scripts/teams-e2e.sh`, `justfile` (`teams-e2e`), `scripts/tests/workbench-recipes.sh` (its e2e list); generated files.

**Behaviour:** spec Part 5 "Invites" (teams), "Visibility" (MH-8), Part 2 (`via`), Part 3 section 3; P12–P16, P25, P30, P31 as pinned; MH-7's member filter through the existing `member_id` query. Invite and approve: validate `team_ids` (≤ 50, deduplicated, each a team of the tenant, else 422 naming the id), read each team's grants (`TeamSeen`), check the invite's grants plus the teams together on the union per project (P7, extras `project_id` and `team_id`), and write grants and memberships in one transaction (`Changed` retried, P25); the reconcile covers the grants' and the teams' projects; the answer's `access` and `has_access` count the teams (step 1's T9-I1 rule: built from what was written, never a post-commit read that can 500).

**Checklist:**
- MH-8: the project invite refuses `team_ids` in any form (`[]` included), and `CREATE_PROJECT_INVITE`'s policy is unchanged;
- MH-2: one route test per caller with a **team-only** member (no own grant): admission, the project list, `/v1/me`, realtime projects, the table API (read and act-as), the console, the SQL editor (`RUN_SQL` authorize), merges, data keys (a key's role held through a team at Developer), owner reveal/reset (Admin through a team), the roster's `has_access` and `via`, grant validation, `remove_member`'s reconcile, and `reinstate`;
- R1 on `GET /v1/members/{id}/teams` and on the roster's team-derived levels (a non-admin never learns of a team grant on a project they can't see; MH-35's `other` still applies);
- MH-7: a member's Activity (`member_id`) shows team events that reached them;
- the e2e script cleans up on exit.

**RED tests (test agent).** `tests/api.rs` with fakes; first failure: compile, 404 or the assertion:
- `project_invite_refuses_team_ids` (`[]`, `[id]` and `null`, P31); `tenant_invite_with_team_ids_adds_memberships_on_every_outcome` (invited, added, existing member); `invite_team_ids_are_checked_on_the_union`; `approve_with_team_ids`; `unknown_foreign_or_too_many_team_ids_are_422`; `an_invite_racing_a_team_grant_change_is_retried` (P25: the fake returns `Changed` once).
- `roster_via_reports_own_team_and_both`; `roster_level_counts_team_grants_on_visible_projects_only`.
- `member_teams_are_filtered_by_visibility_and_pg_roles_rule`.
- `project_team_grants_are_listed_for_project_admins` (P30; `GET /v1/projects/{id}/grants` unchanged).
- `reinstate_reconciles_team_projects`; `remove_member_reconciles_team_projects`.
- `member_activity_finds_team_events`.
- `team_only_member_*`, one per MH-2 caller above (e.g. `team_only_member_is_admitted`, `team_only_member_lists_the_project`, `team_only_member_me_lists_the_project`, `team_only_member_gets_realtime_projects`, `team_only_member_acts_as_a_team_role_in_the_table_api`, `team_only_member_console_as_a_team_role`, `team_only_member_runs_sql_as_a_developer_team_role`, `team_only_member_merges`, `team_only_member_key_role_needs_developer`, `team_admin_reveals_the_owner_password`).
- `scripts/teams-e2e.sh` (traced by reading; the RED is that the recipe is missing). On the stack it: creates a project, a database, NOLOGIN roles `billing_editor` (writes a table) and `billing_reader`, a member with no own grant, and a team granting Developer `{billing_editor}`; adds the member; the member runs SQL as `billing_editor` (allowed); downgrades the team grant to Viewer → `RUN_SQL` as `billing_editor` is 403 and, after the reconcile, `pg_auth_members` no longer links `cb_m_<id>` to `billing_editor`; a personal login made at Developer is revoked by the downgrade (the proxy refuses it); the data API is on for the database, a second team grants Viewer `{billing_reader}` with `GRANT SELECT (id)` only on the table, and the member reads a full cell as `billing_reader` (`GET_CELL` on `id`: 200, through rest, T5) and is refused the other column (403 `permission_denied`); removes the member from the team → `/v1/me` lists no project; deletes the team; a project invite with `team_ids` is 422; everything is cleaned on exit.

**Gotchas:** `workbench-recipes.sh` checks each e2e recipe runs `workbench run scripts/<name>.sh`; `MemberAccess` is in the OpenAPI document (api-gen, P19); `ProjectScope` keeps `affected_member_ids` out of project logs (Part A).

**Verify (whole task):** `cargo nextest run -p orchestrator`; `cargo nextest run -p chelabase-routes`; `cargo clippy -p orchestrator -p chelabase-routes --all-targets -- -D warnings`; `bash -n scripts/teams-e2e.sh`; `bash scripts/tests/workbench-recipes.sh`; `just --justfile <wt>/justfile --working-directory <wt> api-gen` last.

**Live runs:** none in the task (fakes only). `just teams-e2e` runs at End of step 5.

---

## K: docs and landing highlights (sonnet, one agent; no review of its own)

**Worktree:** `.worktrees/members-hub-teams-k` from `members-hub-teams-t23` at its RED commit (or later); merged into `members-hub-teams` after T23. It reads the pinned API from this plan and T23's worktree; K edits no code, and its files are disjoint from T23's, S1's and S2's. It runs in lane B after S1 (agent budget).

**Files:** `apps/docs/content/docs/using/members-and-invites.mdx` (a Teams section: what a team is, live-linking, who edits, invites with teams, removing a member); `apps/docs/content/docs/reference/access.mdx` (effective access: the union, per-role levels with the MH-1 example, narrowing, Admin from any source, the data API extras); `apps/docs/content/docs/architecture/sql-editor.mdx` (T2-2: the hidden role holds Developer-level roles only; a full-cell read as a role held below Developer goes through the data API's connection, P40); `apps/docs/content/docs/reference/errors.mdx` if a new error case is documented; `docs/landing-highlights.md` (P20: a "Members hub, step 2" entry, in review: what it solves, what it unlocks, evidence by suite name, the original Neon base vs ours).

**Verify:** `pnpm --filter @chelabase/docs build`; the coordinator reads the diff.

**Live runs:** none.

---

## S1: Studio Teams tab and team page (one sonnet agent; no review of its own, leaf)

**Worktree:** `.worktrees/members-hub-teams-s1`, cut from `members-hub-teams-t23` at its API-surface commit (P34); merged into `members-hub-teams` after T23.

**Files:** `apps/studio/src/routes/app.members_.teams.$teamId.tsx` (new), `src/routeTree.gen.ts`, `src/components/members/teams/*` (new) and tests, `src/components/members/members-page.tsx` (the Teams tab), `src/lib/members-search.ts` (`teams` in `MEMBERS_TABS`, `validateTeamPageSearch`; P32), `src/lib/members.ts` (the seven team event words).

**Behaviour:** spec Part 5 "Teams tab", Part 1 (tab, route, back link), Part 7 (tenant admins edit; everyone reads), P8, P11, P17, following step 1's comps' patterns (`docs/superpowers/designs/2026-10-23-members-hub/`: the roster's cards/list, the member page's grant cards, the confirm dialogs). API data is mocked in tests (the generated functions, P34). The team page's project access cards use `GrantCard` without "Narrow on a database". Save on the members checklist sends one call per change in sequence with a result per change. Deleting asks first and names how many members get access through the team.

**Checklist (Studio, MH-38):** names escaped; search params parsed against enums (`tab=teams`); `pg_roles: null` shows the platform role only; edit controls only for tenant admins; localStorage in try/catch (the view toggle, as step 1).

**Unit tests (the same agent, Vitest beside each component; for the logic it adds, not layout):**
- `teams_tab_shows_cards_and_list_with_visible_project_counts`;
- `only_tenant_admins_see_create_edit_and_delete`;
- `members_checklist_saves_changes_in_sequence_with_a_result_each`;
- `team_access_cards_have_no_narrowing_and_send_put_team_grant`;
- `delete_confirm_names_how_many_members_get_access_through_the_team`;
- `team_names_and_roles_render_escaped`;
- `back_link_returns_to_the_teams_tab_with_its_view_and_filter`;
- `team_events_have_words`;
- `an_unknown_team_shows_not_found`.

**Verify:** `pnpm --filter @chelabase/studio typecheck`, `pnpm --filter @chelabase/studio test`, `pnpm --filter @chelabase/studio generate-routes` (committed output).

**Live runs:** none (the browser pass is End of step 5).

---

## S2: Studio "via team" and team pickers (one sonnet agent; no review of its own, leaf)

**Worktree:** `.worktrees/members-hub-teams-s2`, cut from `members-hub-teams-t23` at its API-surface commit (P34); merged into `members-hub-teams` after T23 (and after S1, whose files are disjoint).

**Files:** `apps/studio/src/components/members/member-page/*` (Teams section; locked rows and the Effective line in `project-access.tsx`), `src/components/members/roles/role-page.tsx` (Members "via <team>", from `useListProjectTeamGrants`, P30), `src/components/members/{invite-dialog.tsx, approve-dialog.tsx, grant-rows.tsx}`, new `src/components/members/team-picker.tsx` (P33), and their tests.

**Behaviour:** spec Part 3 sections 1 and 3, Part 4 role-page Members, Part 5 "Invites" (teams checklist through S2's own `TeamPicker`, Copy from a member per P23, custom rows; teams and custom rows combine), P12, P14, P15, following step 1's comps' patterns. A team's name on the member page links to `/app/members/teams/$teamId` (S1's route; a plain `Link` by path, so S2 doesn't import S1's code). Locked rows come from `listMemberTeams` and show only where `pg_roles` isn't null; the Effective line reads "Developer · billing_editor (+ via Billing team)". Tenant admins add or remove teams from the member page's Teams section (P8's routes). The project invite form never shows teams.

**Checklist (Studio, MH-38a):** the grant PUT body is the own grant only, never a locked team role; Copy from a member copies own grants only; names escaped.

**Unit tests (the same agent, Vitest; for the logic it adds, not layout):**
- `a_locked_team_role_is_never_sent_in_the_grant_put`;
- `locked_rows_show_via_the_team_and_cannot_be_unchecked`;
- `effective_line_names_the_team`;
- `teams_section_adds_and_removes_for_tenant_admins_only`;
- `role_page_members_show_via_team_for_admins`;
- `invite_sends_team_ids_on_the_tenant_invite_only`;
- `approve_sends_team_ids`;
- `copy_from_a_member_fills_own_grants_not_teams`.

**Verify:** Studio typecheck + test.

**Live runs:** none.

---

## End of step

0. Before this plan: the spec's red-team covers step 2 (MH-1–MH-8; done 2026-10-23). `main` is green (step 1's final `just check` on `c2d1b72b`, merged `6e3c8df3`). Before W1: Q1 and P12 decided (done), the **seam pass** (done, P24–P40), and the stack up (`just --justfile <wt>/justfile --working-directory <wt> workbench up` in `.worktrees/members-hub-teams`, by the coordinator).
1. Whole-branch review (opus) on `main..members-hub-teams`. It covers the tasks without an audit (T4, T5, K, S1, S2; Studio against step 1's comps and the pinned API, P34 drift) and the MH-1–MH-8 checklist end to end.
2. The one final fix round: the review's findings plus every deferred task finding (Status), batched by area into pipelines (models by area as in the task table). The coordinator verifies each fix diff.
3. `just --justfile <wt>/justfile --working-directory <wt> check`, once.
4. No fork change: no gate or image build. Restart the apps and Studio from the merged branch, once: `just --justfile <wt>/justfile --working-directory <wt> workbench restart orchestrator worker gateway rest realtime auth studio` (applies the migration, rebuilds the gateway's route registry, serves the new generated client; P21).
5. Live pass on the plan's stack, one suite at a time, whole files, once, in this order; no two overlap and nothing restarts an app after step 4. `W` = `just --justfile <wt>/justfile --working-directory <wt> workbench test`.
   1. `W -p chelabase-metadata --test members_live -- --run-ignored only`, then `--test live_db`.
   2. `W -p orchestrator --test api -- --run-ignored only`, then `--test sql_run_live` (T4's `the_sql_editor_cannot_set_a_viewer_level_role`), then `W -p rest --test rest_live -- --run-ignored only` (T5's file).
   3. `W -p worker --test member_roles_live -- --run-ignored only` (T4's downgrade test), then `--test personal_logins_live`, `--test roles_live`.
   4. e2e, in this order: `just teams-e2e` (new), `just members-e2e`, `just personal-logins-e2e`, `just sql-editor-e2e`, `just role-e2e`, `just data-e2e`.
   5. Browser pass (Playwright MCP, `http://studio.members-hub-teams.chelabase.localhost:<port>` from `workbench status`), as a tenant admin and as a Developer in isolated contexts, with captures at 1440 and 390: Teams tab (cards, list) → create a team → team page (add members, grant Developer with roles, a refused grant naming member and database, rename) → member page (Teams chips, locked "via team" rows, Effective line) → role page Members "via team" → invite with teams and Copy from a member → approve with teams → downgrade the team grant and see the member's Effective line change → as the member, open a full cell as a Viewer-level team role in the table editor (T5) → delete the team. Then the Impeccable finish reviewer on the new surfaces against step 1's comps (there are no step-2 comps).

   A failure goes to a debug agent (sonnet; opus for access-check, revocation or concurrency causes). The pass resumes from the failed suite (Status).
6. `docs/landing-highlights.md`: K's entry (P20) is checked against the live pass's evidence. Then update the plan Status and the roadmap's "Resume here", and **pause for the user** before merging. After the merge: the entry goes to "shipped" on `main`, the stack is brought down and purged, and the branch is deleted on origin and locally.
