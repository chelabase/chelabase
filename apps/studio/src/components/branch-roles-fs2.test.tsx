import { screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import type { BranchRole } from "@chelabase/management-client";
import { fakeFetch, jsonResponse } from "@/test/fake-fetch";
import { renderWithRouter } from "@/test/render-router";
import { BranchRoles } from "./branch-roles";

/** Fix S2 (recapture at 390 px): the branch Roles table scrolls inside its own frame. */

const owner = {
  name: "chelabase_owner", origin: "owner", level: "owner_level", login: true, inherit: true, bypass_rls: false, member_of: [], member_admin: [], member_no_set: [],
  guard_exempt: false, password: "generated", blocked_keys: [], pending: false, created_at: "2026-08-03T00:00:00Z",
} as unknown as BranchRole;

describe("branch Roles table", () => {
  it("sits_inside_a_horizontally_scrolling_frame", async () => {
    const f = fakeFetch({
      "GET /v1/databases/db/branches/br/roles": () => jsonResponse([owner]),
      "GET /v1/databases/db/personal-logins": () => jsonResponse({ logins: [] }),
    });
    renderWithRouter(<BranchRoles projectId="p1" databaseId="db" branchId="br" />, { fetch: f });
    const table = await screen.findByRole("table");
    const frame = table.closest<HTMLElement>(".overflow-x-auto");
    expect(frame).not.toBeNull();
    // The frame may shrink inside the panel (a flex or grid child would otherwise widen the page).
    expect(frame!.className).toMatch(/max-w-full|min-w-0/);
    // And the table is wider than a phone, so the frame has something to scroll.
    expect(table.className).toMatch(/(^|\s)min-w-\[\d/);
  });
});
