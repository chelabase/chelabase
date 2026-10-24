import { screen } from "@testing-library/react";
import { beforeEach, describe, expect, it } from "vitest";
import { mountPage } from "./test-setup";

/** Fix S3 (finish review round 2, items 3 and 7): the All-rows rows at 390 and the New objects grid. */

beforeEach(() => {
  localStorage.clear();
  sessionStorage.clear();
});

async function loaded() {
  const m = mountPage({});
  await screen.findByRole("table", { name: "Tables and views in billing" });
  return m;
}

describe("privilege editor (fix S3)", () => {
  it("an_all_rows_row_cannot_squeeze_its_status_to_zero_width_at_390", async () => {
    await loaded();
    const li = screen.getByRole("switch", { name: "All rows on customers" }).closest("li")!;
    const cls = li.className;
    // Below sm the three parts stack or wrap (no fixed three-column grid); the three columns come back from sm.
    expect(cls).not.toMatch(/(^|\s)grid-cols-\[/);
    expect(cls).toMatch(/sm:(grid-cols-\[|flex)/);
  });

  it("the_new_objects_rows_share_one_fixed_checkbox_grid_in_the_value_column", async () => {
    await loaded();
    const rows = ["New tables", "New sequences", "New functions"].map((t) => screen.getByText(t).closest("li")!);
    for (const li of rows) {
      expect(li.className).toMatch(/(^|\s)grid(\s|$)/);
      // A fixed column count, not auto-fill, so the boxes sit in the same columns on every row.
      expect(li.className).not.toContain("auto-fill");
      expect(li.className).toMatch(/grid-cols-\[[^\]]*repeat\(\d+,/);
    }
    expect(new Set(rows.map((li) => li.className)).size).toBe(1);
  });
});
