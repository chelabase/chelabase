import { screen, within } from "@testing-library/react";
import { beforeEach, describe, expect, it } from "vitest";
import { expectSearchIcon, expectThemedSelects } from "@/test/themed";
import { mountTab } from "./test-setup";

/** Fix S: the Roles tab (finish-review fixes 4, 7 and 8). */

beforeEach(() => {
  localStorage.clear();
  sessionStorage.clear();
});

const roleLink = (name: string) => screen.findByRole("link", { name: new RegExp(`^${name}\\b`) });

describe("Roles tab toolbar", () => {
  it("the_four_pickers_are_themed_selects_with_a_drawn_chevron", async () => {
    const m = mountTab();
    await roleLink("billing_editor");
    expectThemedSelects(m.container, 4);
  });

  it("the_roles_filter_has_a_magnifier", async () => {
    mountTab();
    await roleLink("billing_editor");
    expectSearchIcon(screen.getByRole("searchbox", { name: "Filter roles" }));
  });
});

describe("the list view shows member counts (fix 8)", () => {
  it("each_row_says_how_many_members_hold_the_role", async () => {
    mountTab({}, "/app/members?tab=roles&view=list");
    const table = await screen.findByRole("table", { name: "Roles" });
    expect(await within(table).findByRole("link", { name: /^billing_editor\b/ })).toHaveTextContent("2 members");
    expect(within(table).getByRole("link", { name: /^billing_admin\b/ })).toHaveTextContent("1 member");
  });
});

describe("create role: no browser-default date field (fix 7)", () => {
  async function advanced() {
    const m = mountTab();
    await roleLink("billing_editor");
    await m.user.click(screen.getByRole("button", { name: "Create role" }));
    const dialog = await screen.findByRole("dialog", { name: "Create a role" });
    await m.user.click(within(dialog).getByRole("button", { name: "Advanced" }));
    return { ...m, dialog };
  }

  it("valid_until_is_not_a_native_datetime_local_input", async () => {
    const { dialog } = await advanced();
    const field = within(dialog).getByLabelText("Valid until");
    expect(field).not.toHaveAttribute("type", "datetime-local");
    expect(dialog.querySelector('input[type="datetime-local"]')).toBeNull();
  });

  it("a_typed_date_and_time_is_read_as_local_time_and_a_bad_one_is_refused_in_words", async () => {
    const { dialog, user } = await advanced();
    const field = within(dialog).getByLabelText("Valid until");
    await user.type(field, "soon");
    expect(within(dialog).getByText("Pick a date and time, or leave it empty.")).toBeInTheDocument();
    expect(within(dialog).getByRole("button", { name: "Create role" })).toBeDisabled();
    await user.clear(field);
    await user.type(field, "2030-01-31 09:30");
    expect(within(dialog).queryByText("Pick a date and time, or leave it empty.")).toBeNull();
  });

  it("the_member_of_checkboxes_are_the_themed_component", async () => {
    const { dialog } = await advanced();
    const group = within(dialog).getByRole("group", { name: "Member of" });
    const boxes = Array.from(group.querySelectorAll('input[type="checkbox"]'));
    expect(boxes.length).toBeGreaterThan(0);
    for (const box of boxes) expect(box.className).toContain("appearance-none");
  });
});
