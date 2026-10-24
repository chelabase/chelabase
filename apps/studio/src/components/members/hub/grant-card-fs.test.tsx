import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";
import type { BranchRole, ProjectGrant } from "@chelabase/management-client";
import { fakeFetch, jsonResponse } from "@/test/fake-fetch";
import { renderWithQuery } from "@/test/render";
import { expectSearchIcon, expectThemedCheckboxes } from "@/test/themed";
import { GrantCard } from "./index";
import { P1, person } from "./fixtures";

/** Fix S, finish-review fix 2 (the grant card): what a unit test can see of it. */

function role(name: string, extra: Partial<BranchRole> = {}): BranchRole {
  return {
    name, origin: "sql", level: "plain", login: false, inherit: true, bypass_rls: false, member_of: [], member_admin: [], member_no_set: [],
    blocked_keys: [], connection_limit: -1, created_at: "2026-10-01T00:00:00Z", password: "none", pending: false,
    ...extra,
  } as BranchRole;
}

const NAMES = ["billing", "reports", "support", "audit", "ledger", "refunds", "webhooks", "exports", "imports"];
const many = [
  role("postgres", { origin: "owner", level: "owner_level", login: true }),
  ...NAMES.map((n) => role(n)),
  role("billing_admin", { member_of: ["billing"] }),
];
const manySources = [{ database: "orders", roles: many }];
const fewSources = [{ database: "orders", roles: [role("postgres", { origin: "owner", level: "owner_level", login: true }), role("billing"), role("reports")] }];
const grant: ProjectGrant = { member_id: "bo", platform_role: "developer", pg_roles: ["billing"], overrides: [] };
const bo = person("bo", { level: "developer", projects: [[P1, "developer"]] });

function setup(sources = fewSources, g: ProjectGrant = grant, extra: { creating?: boolean } = {}) {
  const f = fakeFetch({
    [`PUT /v1/projects/${P1}/grants/bo`]: () => jsonResponse(grant),
    [`DELETE /v1/projects/${P1}/grants/bo`]: () => new Response(null, { status: 204 }),
  });
  return { ...renderWithQuery(<GrantCard projectId={P1} member={bo} grant={g} roleSources={sources} onSaved={() => {}} {...extra} />, { fetch: f }), f };
}

describe("GrantCard: Save and Discard", () => {
  it("has_no_save_and_no_discard_when_there_is_nothing_to_save", () => {
    setup();
    expect(screen.queryByRole("button", { name: "Save" })).toBeNull();
    expect(screen.queryByRole("button", { name: "Discard" })).toBeNull();
  });

  it("shows_save_and_discard_once_something_changed_and_discard_puts_everything_back", async () => {
    setup();
    await userEvent.click(screen.getByRole("checkbox", { name: "reports" }));
    expect(screen.getByRole("button", { name: "Save" })).toBeEnabled();
    await userEvent.click(screen.getByRole("radio", { name: "Viewer" }));
    await userEvent.click(screen.getByRole("button", { name: "Discard" }));
    expect(screen.getByRole("checkbox", { name: "reports" })).not.toBeChecked();
    expect(screen.getByRole("checkbox", { name: "billing" })).toBeChecked();
    expect(screen.getByRole("radio", { name: "Developer" })).toBeChecked();
    expect(screen.queryByRole("button", { name: "Save" })).toBeNull();
    expect(screen.queryByRole("button", { name: "Discard" })).toBeNull();
  });

  it("changing_a_role_back_by_hand_removes_save_again", async () => {
    setup();
    await userEvent.click(screen.getByRole("checkbox", { name: "reports" }));
    await userEvent.click(screen.getByRole("checkbox", { name: "reports" }));
    expect(screen.queryByRole("button", { name: "Save" })).toBeNull();
  });

  it("a_card_for_a_member_with_no_grant_yet_offers_save_from_the_start", () => {
    setup(fewSources, { member_id: "bo", platform_role: "viewer", pg_roles: [], overrides: [] }, { creating: true });
    expect(screen.getByRole("button", { name: "Save" })).toBeEnabled();
  });

  it("save_sits_in_the_card_footer_apart_from_the_form_fields", async () => {
    setup();
    await userEvent.click(screen.getByRole("checkbox", { name: "reports" }));
    const save = screen.getByRole("button", { name: "Save" });
    const discard = screen.getByRole("button", { name: "Discard" });
    // Save and Discard are siblings in one footer, which holds none of the fields.
    expect(save.parentElement).toBe(discard.parentElement);
    expect(save.parentElement?.querySelector('input[type="checkbox"], [role="radio"]')).toBeNull();
  });
});

describe("GrantCard: controls", () => {
  it("the_platform_role_control_uses_the_tab_tone_inside_a_card", () => {
    setup();
    for (const radio of screen.getAllByRole("radio")) {
      expect(radio.className).toContain("bg-channel");
      expect(radio.className).not.toContain("data-[checked]:bg-sand");
    }
  });

  it("offers_a_no_access_segment_first_and_taking_it_removes_the_grant", async () => {
    const { f } = setup();
    const radios = screen.getAllByRole("radio").map((r) => r.textContent?.trim());
    expect(radios).toEqual(["No access", "Viewer", "Developer", "Admin"]);
    await userEvent.click(screen.getByRole("radio", { name: "No access" }));
    // Nothing to pick roles for.
    expect(screen.queryByRole("checkbox", { name: "billing" })).toBeNull();
    await userEvent.click(screen.getByRole("button", { name: "Save" }));
    await waitFor(() => expect(f.calls.filter((r) => r.method === "DELETE")).toHaveLength(1));
    expect(f.calls.filter((r) => r.method === "PUT")).toHaveLength(0);
  });

  it("the_checkboxes_are_the_themed_component", () => {
    const { container } = setup();
    expectThemedCheckboxes(container, 2);
  });

  it("a_long_list_has_a_filter_that_says_how_many_roles_it_filters", async () => {
    setup(manySources);
    // 9 plain roles + billing_admin; never the owner.
    const filter = screen.getByRole("searchbox", { name: "Filter 10 roles" });
    expectSearchIcon(filter);
    await userEvent.type(filter, "led");
    expect(screen.getByRole("checkbox", { name: "ledger" })).toBeInTheDocument();
    expect(screen.queryByRole("checkbox", { name: "reports" })).toBeNull();
  });

  it("role_names_are_mono_and_a_chain_is_inline_mono_names", () => {
    setup(manySources);
    const row = screen.getByRole("checkbox", { name: "billing_admin" }).closest("li")!;
    expect(within(row).getByText("billing_admin").className).toContain("font-mono");
    // "also gives billing": the word is prose, the role name is its own mono element.
    expect(within(row).getByText(/also gives/)).toBeInTheDocument();
    expect(within(row).getByText("billing").className).toContain("font-mono");
  });

  it("a_changed_row_is_marked_for_assistive_technology_and_unmarked_when_changed_back", async () => {
    setup();
    const li = () => screen.getByRole("checkbox", { name: "reports" }).closest("li")!;
    expect(li()).not.toHaveAttribute("data-changed");
    await userEvent.click(screen.getByRole("checkbox", { name: "reports" }));
    expect(li()).toHaveAttribute("data-changed", "true");
    await userEvent.click(screen.getByRole("checkbox", { name: "reports" }));
    expect(li()).not.toHaveAttribute("data-changed");
  });
});
