import { screen } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { jsonResponse, type FakeHandler } from "@/test/fake-fetch";
import { B, ROWS, bodies, meAs, renderEditor, stubLayout } from "@/test/table-editor-fixtures";

// TE-B1 T8 (R6, user): a filtered "select all matching" shows the exact count from the count op.
const Q = `${B}/console/query`;

async function filterAndSelectPage(user: ReturnType<typeof renderEditor>["user"]) {
  await user.click(await screen.findByRole("button", { name: "Filter" }));
  await user.click(screen.getByLabelText("Filter column"));
  await user.click(await screen.findByRole("option", { name: "status" }));
  await user.click(screen.getByLabelText("Filter operator"));
  await user.click(await screen.findByRole("option", { name: "neq" }));
  await user.type(screen.getByLabelText("Filter value"), "open");
  await user.click(screen.getByRole("button", { name: "Apply filter" }));
  await screen.findByRole("gridcell", { name: "8900" });
  await user.click(screen.getByRole("checkbox", { name: "Select all rows on this page" }));
}

describe("selection count", () => {
  beforeEach(() => { localStorage.clear(); sessionStorage.clear(); stubLayout(); });

  it("filtered_select_all_shows_the_exact_count_as_the_role", async () => {
    const answer: FakeHandler = async (req) => {
      const b = await req.clone().json();
      if (b.tree.op === "count") return jsonResponse({ count: 2 });
      return jsonResponse(b.tree.where ? [ROWS[2], ROWS[0]] : ROWS);
    };
    const { f, user } = renderEditor({ me: meAs("developer", ["billing_admin"]), routes: { [`POST ${Q}`]: answer } });
    await filterAndSelectPage(user);
    await user.click(await screen.findByRole("button", { name: "Select all 2 matching rows" }));
    expect(screen.getByText("All 2 matching rows selected")).toBeInTheDocument();
    const count = (await bodies(f, "POST", Q)).find((b) => b.tree.op === "count");
    expect(count.tree).toEqual({ v: 1, op: "count", from: "public.invoices", where: { col: "status", neq: "open" } });
    expect(count.role).toBe("billing_admin");
  });

  it("a_failed_count_shows_no_figure", async () => {
    const answer: FakeHandler = async (req) => {
      const b = await req.clone().json();
      if (b.tree.op === "count") return jsonResponse({ error: { code: "timeout", message: "statement timeout" } }, 504);
      return jsonResponse(b.tree.where ? [ROWS[2], ROWS[0]] : ROWS);
    };
    const { user } = renderEditor({ routes: { [`POST ${Q}`]: answer } });
    await filterAndSelectPage(user);
    await user.click(await screen.findByRole("button", { name: "Select all matching rows" }));
    expect(screen.getByText("All matching rows selected")).toBeInTheDocument();
  });

  it("unfiltered_keeps_the_estimate_and_sends_no_count", async () => {
    const { f, user } = renderEditor();
    await screen.findByRole("gridcell", { name: "45000" });
    await user.click(screen.getByRole("checkbox", { name: "Select all rows on this page" }));
    expect(await screen.findByRole("button", { name: "Select all ≈ 48,210 matching rows" })).toBeInTheDocument();
    await vi.waitFor(async () => expect((await bodies(f, "POST", Q)).some((b) => b.tree.op === "count")).toBe(false));
  });
});
