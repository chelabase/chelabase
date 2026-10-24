import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";
import type { BranchRole, MeResponse as Me, Member, Project, ProjectGrant } from "@chelabase/management-client";
import { fakeFetch, jsonResponse, type FakeHandler } from "@/test/fake-fetch";
import { renderWithRouter } from "@/test/render-router";
import { expectThemedCheckboxes } from "@/test/themed";
import { AccessTab } from "./access-tab";

/** Fix S: BP-5 (a role reached by chain and explicitly is listed once) and the shared grant card in the Access tab. */

const orders = { database_id: "d1", name: "orders", postgres_database: "orders", status: "ready", host: "h", port: 1, branch_count: 1 } as const;
const project: Project = { id: "p1", slug: "shop", name: "Shop", status: "active", databases: [orders] };
const adminMe = {
  sub: "ana", email: "ana@example.test", name: "ana", tenant_id: "t1", instance_admin: false, tenant_admins: [],
  member: { id: "ana", status: "active", tenant_admin: false },
  projects: [{ project_id: "p1", platform_role: "admin", roles: "all", databases: [] }],
  user: { sub: "ana", org_id: "o" },
  tenant: { id: "t1", slug: "demo", display_name: null, projects: [project] },
} as unknown as Me;

const member = (id: string): Member => ({
  id, email: `${id}@example.test`, display_name: id[0].toUpperCase() + id.slice(1), status: "active", tenant_admin: false, has_access: true,
  invite: null, accepted_at: null, suspended_at: null, suspended_by: null, created_at: "2026-10-01T00:00:00Z", access: { level: "none", projects: [] },
});
const people = [member("ana"), member("hal"), member("ivo")];
const grants: ProjectGrant[] = [
  { member_id: "ana", platform_role: "admin", pg_roles: "all", overrides: [] },
  // billing_admin gives billing; billing is also granted explicitly.
  { member_id: "hal", platform_role: "developer", pg_roles: ["billing_admin", "billing"], overrides: [] },
  { member_id: "ivo", platform_role: "developer", pg_roles: ["billing_admin"], overrides: [] },
];
const role = (name: string, extra: Partial<BranchRole> = {}): BranchRole =>
  ({ name, origin: "sql", level: "plain", login: false, inherit: true, bypass_rls: false, member_of: [], blocked_keys: [], connection_limit: -1, created_at: "2026-10-01T00:00:00Z", password: "generated", pending: false, ...extra }) as BranchRole;
const roles = [role("o", { origin: "owner", level: "owner_level", login: true }), role("billing"), role("billing_admin", { member_of: ["billing"] })];

const ok = (body: unknown): FakeHandler => () => jsonResponse(body);
function setup() {
  const f = fakeFetch({
    "GET /v1/me": ok(adminMe),
    "GET /v1/members": ok(people),
    "GET /v1/projects/p1/grants": ok(grants),
    "GET /v1/projects/p1/access-events": ok({ events: [], next_before: null }),
    "GET /v1/databases/d1/branches": ok([{ id: "m1", name: "main", parent_branch_id: null, kind: "normal", status: "ready", host: "h", port: 1, branched_at_time: null, branched_at_lsn: null, created_at: "2026-09-30T10:00:00Z", history_start: null, error: null, protected: false, main_database_missing: false }]),
    "GET /v1/databases/d1/branches/m1/roles": ok(roles),
    "GET /v1/invites/delivery": ok({ email_available: true, reason: null }),
  });
  return { ...renderWithRouter(<AccessTab project={project} />, { fetch: f }), f };
}

/** The roles read has been answered and rendered (chains come from it). */
async function settled(f: { calls: Request[] }) {
  await waitFor(() => expect(f.calls.some((r) => new URL(r.url).pathname.endsWith("/roles"))).toBe(true));
  await new Promise((r) => setTimeout(r, 100));
}

/** How many times `name` appears in the text (not as the start of a longer role name; the chips have no separators). */
const occurrences = (text: string, name: string) => (text.match(new RegExp(`${name}(?!_)`, "g")) ?? []).length;

describe("BP-5: one role, one listing", () => {
  it("a_role_both_granted_and_reached_through_a_chain_is_listed_once_in_the_row", async () => {
    const { f } = setup();
    const row = (await screen.findByText("hal@example.test")).closest("tr")!;
    await settled(f);
    expect(occurrences(row.textContent ?? "", "billing")).toBe(1);
    // The chain's owner is still listed, and still says what it brings.
    expect(occurrences(row.textContent ?? "", "billing_admin")).toBe(1);
  });

  it("a_chain_alone_is_still_shown_as_also_giving", async () => {
    const { f } = setup();
    const row = (await screen.findByText("ivo@example.test")).closest("tr")!;
    await settled(f);
    expect(row).toHaveTextContent(/billing_admin/);
    expect(occurrences(row.textContent ?? "", "billing")).toBe(1);
  });
});

describe("the shared grant card in the Access tab", () => {
  it("opens_with_themed_checkboxes_and_no_save_until_something_changes", async () => {
    setup();
    const edit = await screen.findByRole("button", { name: "Edit access for Hal" });
    await userEvent.click(edit);
    const editor = await screen.findByRole("group", { name: "Access for Hal" });
    await within(editor).findByRole("checkbox", { name: "billing" });
    expectThemedCheckboxes(editor, 2);
    expect(within(editor).queryByRole("button", { name: "Save" })).toBeNull();
  });
});
