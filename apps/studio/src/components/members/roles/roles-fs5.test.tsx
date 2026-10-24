import { screen, within } from "@testing-library/react";
import { beforeEach, describe, expect, it } from "vitest";
import { mountPage, mountTab } from "./test-setup";

/** Fix S5 (Impeccable review 3, polish 5 to 7): the All rows block, the New objects grid and the Roles list at 390. */

beforeEach(() => {
  localStorage.clear();
  sessionStorage.clear();
});

const tokens = (el: Element) => el.className.split(/\s+/);

async function loaded() {
  mountPage({});
  await screen.findByRole("table", { name: "Tables and views in billing" });
}

describe("the All rows block (fix S5)", () => {
  it("is_its_own_bordered_frame", async () => {
    await loaded();
    const heading = screen.getByRole("heading", { name: "All rows" });
    const sw = screen.getByRole("switch", { name: "All rows on customers" });
    // The block is the nearest ancestor of the heading that also holds the switches.
    let block: HTMLElement = heading.parentElement!;
    while (!block.contains(sw)) block = block.parentElement!;
    expect(tokens(block)).toContain("border");
  });

  it("the_state_text_sits_just_left_of_its_switch_from_sm", async () => {
    await loaded();
    const sw = screen.getByRole("switch", { name: "All rows on customers" });
    const li = sw.closest("li")!;
    const state = [...li.children].find((c) => /^(off|on)/.test(c.textContent ?? "") && !c.contains(sw))!;
    expect(state).toBeDefined();
    // Right-aligned in its column, so it ends beside the switch instead of beside the name.
    expect(tokens(state)).toContain("sm:text-right");
  });
});

describe("the New objects grid (fix S5)", () => {
  it("has_fixed_column_widths_from_sm", async () => {
    await loaded();
    for (const t of ["New tables", "New sequences", "New functions"]) {
      expect(tokens(screen.getByText(t).closest("li")!)).toContain("sm:grid-cols-[6.5rem_repeat(4,8rem)]");
    }
  });
});

describe("the Roles list at 390 (fix S5)", () => {
  it("the_stacked_member_count_has_no_leading_separator", async () => {
    mountTab();
    const link = await screen.findByRole("link", { name: /billing_editor/ });
    const stacked = link.querySelector<HTMLElement>("span.sm\\:hidden")!;
    expect(stacked).not.toBeNull();
    expect(within(link).getByText("2 members", { selector: "span.sm\\:hidden" })).toBe(stacked);
    expect(stacked.textContent).toBe("2 members");
  });
});
