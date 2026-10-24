import { render, screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import type { BranchRole } from "@chelabase/management-client";
import { fakeFetch, jsonResponse } from "@/test/fake-fetch";
import { renderWithRouter } from "@/test/render-router";
import { BranchRoles } from "@/components/branch-roles";
import { PlatformRoleField } from "../platform-role-field";
import { mountPage } from "../roles/test-setup";

/** Fix S4b (final recapture, findings 2 to 4): class structure at 390. Real widths are the recapture's. */

const has = (cls: string, token: string) => cls.split(/\s+/).includes(token);

describe("platform role control at 390 (fix S4b)", () => {
  it("with_No_access_the_four_segments_wrap_two_by_two_below_sm", () => {
    render(<PlatformRoleField value="viewer" onChange={() => {}} noAccess={{ selected: false, onSelect: () => {} }} />);
    const group = screen.getByRole("radiogroup");
    expect(has(group.className, "grid-cols-2")).toBe(true);
    expect(has(group.className, "grid-cols-4")).toBe(false);
  });

  it("every_segment_keeps_whitespace_nowrap", () => {
    render(<PlatformRoleField value="viewer" onChange={() => {}} noAccess={{ selected: false, onSelect: () => {} }} />);
    const radios = screen.getAllByRole("radio");
    expect(radios).toHaveLength(4);
    for (const r of radios) expect(r.className).toContain("whitespace-nowrap");
  });
});

describe("role page Future objects at 390 (fix S4b)", () => {
  it("the_checkbox_grid_reflows_to_two_columns_below_sm_and_keeps_its_fixed_grid_from_sm", async () => {
    mountPage({});
    await screen.findByRole("table", { name: "Tables and views in billing" });
    const li = screen.getByText("New tables").closest("li")!;
    expect(has(li.className, "grid-cols-2")).toBe(true);
    expect(li.className).toMatch(/sm:grid-cols-\[[^\]]*repeat\(4,/);
    // No unprefixed arbitrary column list (it is what overflowed the page).
    expect(li.className.split(/\s+/).some((c) => c.startsWith("grid-cols-["))).toBe(false);
  });

  it("the_label_takes_its_own_row_below_sm", async () => {
    mountPage({});
    await screen.findByRole("table", { name: "Tables and views in billing" });
    const label = screen.getByText("New tables");
    expect(label.className).toMatch(/(^|\s)col-span-(2|full)(\s|$)/);
    // An inline row span cannot be responsive, so the span must come from classes.
    expect(label.style.gridRow).toBe("");
  });
});

const role = (over: Partial<BranchRole> & { name: string }): BranchRole =>
  ({
    origin: "api", level: "plain", login: false, password: "none", blocked_keys: [], pending: false, created_at: "2026-10-01T09:00:00Z",
    inherit: true, connection_limit: -1, valid_until: null, bypass_rls: false, member_of: [], member_no_set: [], member_admin: [], guard_exempt: false,
    ...over,
  }) as BranchRole;

describe("branch Roles panel (fix S4b)", () => {
  async function table() {
    const f = fakeFetch({
      "GET /v1/databases/db/branches/br/roles": () => jsonResponse([role({ name: "reporting" })]),
      "GET /v1/databases/db/personal-logins": () => jsonResponse({ logins: [] }),
    });
    renderWithRouter(<BranchRoles projectId="p1" databaseId="db" branchId="br" />, { fetch: f });
    return screen.findByRole("row", { name: /reporting/ });
  }

  it("No_password_NOLOGIN_stays_on_one_line", async () => {
    const row = await table();
    const text = [...row.querySelectorAll<HTMLElement>("span")].find((s) => s.textContent === "No password (NOLOGIN)");
    expect(text).toBeTruthy();
    expect((text as HTMLElement).className).toContain("whitespace-nowrap");
  });

  it("the_name_cell_has_a_gap_before_the_origin_badge", async () => {
    const row = await table();
    const name = row.querySelector<HTMLElement>("td")!;
    expect(name).toHaveTextContent("reporting");
    expect(name.className).toMatch(/(^|\s)p[rx]-\d/);
  });
});
