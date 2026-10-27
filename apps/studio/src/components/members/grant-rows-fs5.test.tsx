import { render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import type { Project } from "@chelabase/management-client";
import { renderWithQuery } from "@/test/render";
import { GrantRows, type GrantRow } from "./grant-rows";
import { PlatformRoleField, ROLE_HINT } from "./platform-role-field";

/** Fix S5 (Impeccable review 3, item 1 and polish 3): the invite access row at sm and up, the legend. */

const projects = [{ id: "p1", slug: "shop", name: "Shop", status: "active", databases: [] }] as unknown as Project[];
const rows: GrantRow[] = [{ projectId: "p1", platformRole: "developer", pgRoles: [], blocked: false }];
const tokens = (el: Element) => el.className.split(/\s+/);

function mount() {
  renderWithQuery(<GrantRows projects={projects} rows={rows} onChange={vi.fn()} legendNote="the same for everyone in this invite" />);
  const row = screen.getByRole("group", { name: "Shop" });
  const flex = row.querySelector<HTMLElement>(".sm\\:flex-row")!;
  return { row, flex };
}

describe("PlatformRoleField hint prop (fix S5)", () => {
  it("hint_defaults_to_true", () => {
    render(<PlatformRoleField value="developer" onChange={vi.fn()} />);
    expect(screen.getByText(ROLE_HINT.developer)).toBeInTheDocument();
  });

  it("hint_false_renders_no_role_hint", () => {
    render(<PlatformRoleField value="developer" onChange={vi.fn()} hint={false} />);
    expect(screen.queryByText(ROLE_HINT.developer)).toBeNull();
  });
});

describe("invite access row at sm (fix S5)", () => {
  it("the_role_hint_is_one_full_width_line_under_the_sm_flex_row", () => {
    const { row, flex } = mount();
    expect(flex).not.toBeNull();
    const hint = screen.getByText(ROLE_HINT.developer);
    expect(row).toContainElement(hint);
    expect(flex).not.toContainElement(hint);
    expect(tokens(hint)).toEqual(expect.arrayContaining(["text-xs", "text-chalk-dim"]));
    expect(tokens(hint.parentElement === row ? hint : hint.parentElement!)).toContain("w-full");
  });

  it("the_role_picker_wrapper_has_a_min_width_beside_flex_1", () => {
    const { flex } = mount();
    const wrapper = [...flex.children].find((c) => tokens(c).includes("sm:flex-1"))!;
    expect(wrapper).toBeDefined();
    expect(wrapper).toContainElement(screen.getByLabelText("Postgres roles"));
    expect(tokens(wrapper)).toContain("sm:min-w-48");
  });

  it("the_visible_labels_are_sr_only_from_sm", () => {
    mount();
    const project = screen.getByText("Project", { selector: "span" });
    const platform = screen.getByText("Platform role");
    const pg = screen.getByText("Postgres roles", { selector: "label" });
    for (const el of [project, platform, pg]) expect(tokens(el), el.textContent ?? "").toContain("sm:sr-only");
  });
});

describe("invite legend (fix S5)", () => {
  it("only_Access_is_bold_and_the_note_is_muted", () => {
    mount();
    const legend = document.querySelector("legend")!;
    const strong = legend.querySelector("span.font-medium, strong, b");
    expect(strong).not.toBeNull();
    expect(strong!.textContent).toBe("Access");
    const note = [...legend.querySelectorAll("span")].find((s) => /the same for everyone in this invite/.test(s.textContent ?? ""))!;
    expect(tokens(note)).toEqual(expect.arrayContaining(["font-normal", "text-chalk-dim"]));
  });
});

describe("empty access hint with teams", () => {
  it("says_the_teams_access_is_all_they_get_when_a_team_is_chosen", () => {
    const { rerender } = render(<GrantRows projects={projects} rows={[]} onChange={vi.fn()} />);
    expect(screen.getByText(/they join with no access/)).toBeInTheDocument();
    rerender(<GrantRows projects={projects} rows={[]} onChange={vi.fn()} teamsChosen />);
    expect(screen.getByText(/Without a project, they get only their teams' access/)).toBeInTheDocument();
  });
});
