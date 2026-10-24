import { useState } from "react";
import { screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import type { Branch as BranchSummary, BranchRole, MeResponse as Me, ProjectInfo } from "@chelabase/management-client";
import { fakeFetch, jsonResponse } from "@/test/fake-fetch";
import { renderWithQuery } from "@/test/render";
import { renderWithRouter } from "@/test/render-router";
import { BranchRoles } from "./branch-roles";
import { BranchTree } from "./branch-tree";
import { ProjectPage, type ProjectTab } from "./project-page";

/** Fix S5 (Impeccable review 3, item 2): table columns get a gap, badges and headers stay on one line. */

const tokens = (el: Element) => el.className.split(/\s+/);
const hasPr4 = (el: Element) => tokens(el).includes("pr-4");

/** Every th/td of every row except the last cell has right padding `pr-4`. */
function expectGaps(table: HTMLElement) {
  for (const tr of table.querySelectorAll("tr")) {
    const cells = [...tr.children];
    for (const c of cells.slice(0, -1)) expect(hasPr4(c), `${c.tagName} "${c.textContent}" has pr-4`).toBe(true);
  }
}

const role = (over: Partial<BranchRole> & { name: string }): BranchRole =>
  ({
    origin: "api", level: "plain", login: false, password: "none", blocked_keys: [], pending: false, created_at: "2026-10-01T09:00:00Z",
    inherit: true, connection_limit: -1, valid_until: null, bypass_rls: false, member_of: [], member_no_set: [], member_admin: [], guard_exempt: false,
    ...over,
  }) as BranchRole;

describe("branch Roles table gaps (fix S5)", () => {
  async function table() {
    const f = fakeFetch({
      "GET /v1/databases/db/branches/br/roles": () => jsonResponse([role({ name: "owner_x", level: "owner_level" }), role({ name: "reporting" })]),
      "GET /v1/databases/db/personal-logins": () => jsonResponse({ logins: [] }),
    });
    renderWithRouter(<BranchRoles projectId="p1" databaseId="db" branchId="br" />, { fetch: f });
    await screen.findByRole("row", { name: /reporting/ });
    return screen.getByRole("table");
  }

  it("every_cell_but_the_last_has_pr_4", async () => {
    expectGaps(await table());
  });

  it("login_guard_and_attributes_headers_do_not_wrap", async () => {
    const t = await table();
    for (const name of ["Login guard", "Attributes"]) {
      const th = [...t.querySelectorAll("th")].find((h) => h.textContent === name)!;
      expect(tokens(th), name).toContain("whitespace-nowrap");
    }
  });

  it("origin_and_level_badges_do_not_wrap_and_owner_level_stays_on_one_line", async () => {
    const t = await table();
    const level = [...t.querySelectorAll("span")].find((s) => s.textContent === "Owner-level")!;
    expect(tokens(level)).toContain("whitespace-nowrap");
    const origin = t.querySelectorAll("tbody tr")[1].children[1].querySelector("span")!;
    expect(tokens(origin)).toContain("whitespace-nowrap");
  });

  it("password_and_created_cells_keep_nowrap", async () => {
    const t = await table();
    const row = t.querySelectorAll("tbody tr")[1];
    expect(row.children[6].querySelector("span")!.className).toContain("whitespace-nowrap");
    expect(tokens(row.children[7])).toContain("whitespace-nowrap");
  });
});

const base = {
  kind: "normal" as const, status: "ready" as const, port: 4432, branched_at_time: null, branched_at_lsn: null, history_start: null, error: null,
  protected: false, main_database_missing: false, guard_logins: true, review_acknowledged_hash: null,
};
const main: BranchSummary = { ...base, id: "m", name: "main", parent_branch_id: null, host: "ep-aaaaaaaaaaaa.localtest.me", created_at: "2026-09-30T10:00:00Z" };

describe("branch tree gaps (fix S5)", () => {
  it("every_cell_but_the_last_has_pr_4", () => {
    renderWithQuery(<BranchTree projectId="p1" databaseId="db" databaseName="web1" branches={[main]} historyWindow={604800} />);
    expectGaps(screen.getByRole("table"));
  });
});

describe("project databases table gaps (fix S5)", () => {
  const database = { database_id: "d1", name: "web1", postgres_database: "web1", status: "ready", host: "127.0.0.1", port: 49153, branch_count: 1 } as const;
  const me = {
    sub: "u1", email: "u1@example.test", name: "U1", tenant_id: "t1", member: { id: "m-self", status: "active", tenant_admin: true },
    instance_admin: true, tenant_admins: [], projects: [{ project_id: "p1", platform_role: "admin", roles: "all", databases: [] }],
    user: { sub: "u1", org_id: "org-1" },
    tenant: { id: "t1", slug: "demo", display_name: null, projects: [{ id: "p1", slug: "default", name: "Default", status: "active", databases: [database] }] },
  } as unknown as Me;
  const info: ProjectInfo = { id: "p1", slug: "default", name: "Default", status: "active", error: null, app_users: null, created_at: "2026-10-03T10:00:00Z", updated_at: "2026-10-03T10:00:00Z" };

  it("every_cell_but_the_last_has_pr_4", async () => {
    const f = fakeFetch({ "GET /v1/me": () => jsonResponse(me), "GET /v1/projects/p1": () => jsonResponse(info) });
    function Harness() {
      const [tab, setTab] = useState<ProjectTab>("databases");
      return <ProjectPage projectId="p1" apiUrl="http://gw" tab={tab} onTabChange={setTab} onGone={vi.fn()} />;
    }
    renderWithRouter(<Harness />, { fetch: f });
    expectGaps(await screen.findByRole("table"));
  });
});
