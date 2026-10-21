import { screen, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { jsonResponse } from "@/test/fake-fetch";
import { B, FULL, ROWS, renderEditor, stubLayout } from "@/test/table-editor-fixtures";

/** RT1 Playwright pass, F3 edges (coder's own): what a view's Structure tab shows without the table endpoint. */

const Q = `${B}/console/query`;

beforeEach(() => {
  localStorage.clear();
  sessionStorage.clear();
  stubLayout();
  vi.stubGlobal("matchMedia", (q: string) => ({ matches: false, media: q, addEventListener() {}, removeEventListener() {}, addListener() {}, removeListener() {}, onchange: null, dispatchEvent: () => false }));
});
afterEach(() => vi.unstubAllGlobals());

describe("F3: a view's Structure tab", () => {
  it("says_views_are_read_only_and_lists_the_columns_its_rows_came_back_with", async () => {
    const { user } = renderEditor({ params: { ...FULL, table: "v_open" }, routes: { [`POST ${Q}`]: () => jsonResponse(ROWS) } });
    expect(await within(await screen.findByRole("grid", { name: "public.v_open" })).findByRole("gridcell", { name: "Q3 plan" })).toBeInTheDocument();
    await user.click(screen.getByRole("tab", { name: "Structure" }));
    const panel = await screen.findByRole("tabpanel");
    expect(within(panel).getByText(/Views are read-only here\./)).toBeInTheDocument();
    const columns = within(panel).getAllByRole("listitem").map((li) => li.textContent);
    expect(columns).toEqual(["id", "customer_id", "status", "amount_cents", "memo"]);
    expect(within(panel).queryByText("Loading the table…")).toBeNull();
  });

  it("with_no_rows_read_it_says_there_are_no_columns_to_show", async () => {
    const { user } = renderEditor({ params: { ...FULL, table: "v_open" }, routes: { [`POST ${Q}`]: () => jsonResponse([]) } });
    await user.click(await screen.findByRole("tab", { name: "Structure" }));
    expect(await within(await screen.findByRole("tabpanel")).findByText("No rows read yet, so no columns to show.")).toBeInTheDocument();
  });
});
