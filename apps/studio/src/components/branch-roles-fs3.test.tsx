import { screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";
import type { Branch, BranchRole } from "@chelabase/management-client";
import { fakeFetch, jsonResponse } from "@/test/fake-fetch";
import { renderWithRouter } from "@/test/render-router";
import { Tag } from "@/components/members/roles/bits";
import { render } from "@testing-library/react";
import { BranchRoles } from "./branch-roles";
import { BranchTree } from "./branch-tree";

/** Fix S3 (finish review round 2, item 1): the branch Roles panel and the tables' scroll frames. */

const role = (over: Partial<BranchRole> & { name: string }): BranchRole =>
  ({
    origin: "api", level: "plain", login: true, password: "generated", blocked_keys: [], pending: false, created_at: "2026-10-01T09:00:00Z",
    inherit: true, connection_limit: -1, valid_until: null, bypass_rls: false, member_of: [], member_no_set: [], member_admin: [], guard_exempt: false,
    ...over,
  }) as BranchRole;

const main: Branch = {
  id: "m", name: "main", parent_branch_id: null, kind: "normal", status: "ready", host: "ep-aaaaaaaaaaaa.localtest.me", port: 4432,
  branched_at_time: null, branched_at_lsn: null, created_at: "2026-09-30T10:00:00Z", history_start: null, error: null, protected: false,
  main_database_missing: false, guard_logins: true, review_acknowledged_hash: null,
} as Branch;

describe("branch Roles panel (fix S3)", () => {
  it("opens_as_its_own_full_width_row_not_inside_the_actions_cell", async () => {
    const f = fakeFetch({
      "GET /v1/databases/db/branches/m/roles": () => jsonResponse([role({ name: "reporting" })]),
      "GET /v1/databases/db/personal-logins": () => jsonResponse({ logins: [] }),
    });
    renderWithRouter(<BranchTree projectId="p1" databaseId="db" databaseName="web1" branches={[main]} historyWindow={604800} />, { fetch: f });
    await userEvent.click(await screen.findByRole("button", { name: "Roles" }));
    const heading = await screen.findByRole("heading", { name: "Roles" });
    const cell = heading.closest("td");
    // Either outside any cell, or a cell that spans all four of the branch table's columns.
    if (cell) expect(cell.colSpan).toBeGreaterThanOrEqual(4);
    // And it isn't the cell that holds the branch's action buttons.
    expect(cell?.contains(screen.getByRole("button", { name: "Roles" })) ?? false).toBe(false);
  });

  it("shows_created_as_d_MMM_yyyy", async () => {
    const f = fakeFetch({
      "GET /v1/databases/db/branches/br/roles": () => jsonResponse([role({ name: "reporting", created_at: "2026-10-01T09:00:00Z" })]),
      "GET /v1/databases/db/personal-logins": () => jsonResponse({ logins: [] }),
    });
    renderWithRouter(<BranchRoles projectId="p1" databaseId="db" branchId="br" />, { fetch: f });
    const row = await screen.findByRole("row", { name: /reporting/ });
    expect(row).toHaveTextContent("1 Oct 2026");
    expect(row).not.toHaveTextContent("UTC");
  });

  it("the_branch_table_scrolls_inside_a_relative_frame", async () => {
    renderWithRouter(<BranchTree projectId="p1" databaseId="db" databaseName="web1" branches={[main]} historyWindow={604800} />, {
      fetch: fakeFetch({}),
    });
    const frame = (await screen.findByRole("table")).closest<HTMLElement>(".overflow-x-auto");
    expect(frame).not.toBeNull();
    expect(frame!.className).toMatch(/(^|\s)relative(\s|$)/);
  });

  it("the_roles_frame_is_relative", async () => {
    const f = fakeFetch({
      "GET /v1/databases/db/branches/br/roles": () => jsonResponse([role({ name: "reporting" })]),
      "GET /v1/databases/db/personal-logins": () => jsonResponse({ logins: [] }),
    });
    renderWithRouter(<BranchRoles projectId="p1" databaseId="db" branchId="br" />, { fetch: f });
    const frame = (await screen.findByRole("table")).closest<HTMLElement>(".overflow-x-auto");
    expect(frame!.className).toMatch(/(^|\s)relative(\s|$)/);
  });
});

describe("tags (fix S3)", () => {
  it("a_tag_never_wraps_so_NOLOGIN_stays_on_one_line", () => {
    render(<Tag>NOLOGIN</Tag>);
    expect(screen.getByText("NOLOGIN").className).toMatch(/whitespace-nowrap/);
  });
});
