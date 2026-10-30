import { screen, within } from "@testing-library/react";
import { beforeEach, describe, expect, it } from "vitest";
import { meAs, renderEditor, stubLayout } from "@/test/table-editor-fixtures";

const grid = () => screen.findByRole("grid", { name: "public.invoices" });

describe("Add row", () => {
  beforeEach(() => { localStorage.clear(); sessionStorage.clear(); stubLayout(); });

  it("an_empty_writable_table_offers_add_row_which_opens_the_new_row_panel", async () => {
    const { user } = renderEditor({ rows: [] });
    expect(await screen.findByText("No rows")).toBeInTheDocument();
    await user.click(await screen.findByRole("button", { name: "Add row" }));
    expect(await screen.findByRole("complementary", { name: "New row" })).toBeInTheDocument();
  });

  it("a_read_only_table_has_no_add_row", async () => {
    renderEditor({ me: meAs("viewer", ["reporter"]), rows: [] });
    expect(await screen.findByText("No rows")).toBeInTheDocument();
    await grid();
    expect(screen.queryByRole("button", { name: "Add row" })).toBeNull();
  });

  it("add_row_sits_outside_the_grid", async () => {
    renderEditor({ rows: [] });
    await screen.findByRole("button", { name: "Add row" });
    const g = await grid();
    expect(within(g).queryByRole("button", { name: "Add row" })).toBeNull();
    // Only the header row: the button adds no row to the grid.
    expect(g).toHaveAttribute("aria-rowcount", "1");
  });
});
