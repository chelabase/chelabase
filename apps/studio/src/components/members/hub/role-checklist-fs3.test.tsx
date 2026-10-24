import { render, screen, within } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import type { BranchRole, ProjectGrant } from "@chelabase/management-client";
import { fakeFetch } from "@/test/fake-fetch";
import { renderWithQuery } from "@/test/render";
import type { RoleChoice } from "../role-picker";
import { GrantCard, RoleChecklist } from "./index";
import { P1, person } from "./fixtures";

/** Fix S3 (finish review round 2, item 6): the role checklist reads as one framed list with an inline chain. */

const choice = (value: string, extra: Partial<RoleChoice> = {}): RoleChoice => ({ value, chain: [], ...extra });

describe("role checklist (fix S3)", () => {
  it("also_gives_is_inline_after_the_name_not_a_block_of_its_own", () => {
    render(<RoleChecklist choices={[choice("billing_editor", { chain: ["billing_reader"] })]} checked={[]} onChange={vi.fn()} />);
    const row = screen.getByRole("checkbox", { name: "billing_editor" }).closest("li")!;
    const chain = within(row).getByText("billing_reader", { selector: ".font-mono" });
    for (let el: HTMLElement | null = chain; el && el !== row; el = el.parentElement) {
      // Only the label wrapper (min-w-0) may be a block; nothing between the chain and it is.
      if (el.className.includes("min-w-0")) break;
      expect(el.className).not.toMatch(/(^|\s)block(\s|$)/);
    }
  });

  it("the_list_has_a_full_border_frame", () => {
    render(<RoleChecklist choices={[choice("a"), choice("b")]} checked={[]} onChange={vi.fn()} />);
    const list = screen.getAllByRole("listitem")[0].parentElement!;
    expect(list.className).toMatch(/(^|\s)border(\s|$)/);
    expect(list.className).not.toMatch(/border-y/);
  });

  it("a_grant_card_hints_how_the_list_is_ordered_next_to_the_Postgres_roles_label", () => {
    const role = (name: string) =>
      ({ name, origin: "sql", level: "plain", login: false, inherit: true, bypass_rls: false, guard_exempt: false, member_of: [], member_admin: [], member_no_set: [], blocked_keys: [], connection_limit: -1, created_at: "2026-10-01T00:00:00Z", password: "none", pending: false }) as BranchRole;
    const grant: ProjectGrant = { member_id: "bo", platform_role: "developer", pg_roles: ["billing"], overrides: [] };
    const bo = person("bo", { level: "developer", projects: [[P1, "developer"]] });
    renderWithQuery(
      <GrantCard projectId={P1} member={bo} grant={grant} roleSources={[{ database: "orders", roles: [role("billing"), role("reports")] }]} onSaved={vi.fn()} />,
      { fetch: fakeFetch({}) },
    );
    const hint = screen.getByText("Checked roles come first; a role's chain comes with it");
    const label = screen.getByText("Postgres roles");
    // Same line: the label and the hint share a parent that isn't the whole section.
    expect(hint.parentElement).toBe(label.parentElement);
    expect(label.parentElement!.querySelector("ul")).toBeNull();
  });
});
