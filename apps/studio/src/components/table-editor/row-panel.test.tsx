import { screen, within } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { jsonResponse, type FakeHandler } from "@/test/fake-fetch";
import { B, ROWS, T, bodies, invoices, renderEditor, stubLayout } from "@/test/table-editor-fixtures";

const Q = `${B}/console/query`;
const writes = async (f: Parameters<typeof bodies>[0]) => (await bodies(f, "POST", Q)).filter((b) => b.tree.op !== "select");
const open = async (user: ReturnType<typeof renderEditor>["user"], n = 2) => {
  await user.click(await screen.findByRole("button", { name: `Open row ${n}` }));
  return screen.findByRole("complementary", { name: "Row" });
};

describe("row panel", () => {
  beforeEach(() => { localStorage.clear(); sessionStorage.clear(); stubLayout(); });

  it("saves_only_changed_fields", async () => {
    const { f, user } = renderEditor();
    const panel = await open(user);
    expect(within(panel).getByLabelText("id")).toHaveAttribute("readonly");
    const amount = within(panel).getByLabelText("amount_cents");
    await user.clear(amount);
    await user.type(amount, "47000");
    await user.click(within(panel).getByLabelText("status"));
    await user.click(await screen.findByRole("option", { name: "paid" }));
    await user.click(within(panel).getByRole("button", { name: "Save row" }));
    await vi.waitFor(async () => expect(await writes(f)).toHaveLength(1));
    expect((await writes(f))[0].tree).toEqual({ v: 1, op: "update", table: "public.invoices", set: { amount_cents: 47000, status: "paid" }, where: { and: [{ col: "id", eq: "81c4-2", parse: true }, { col: "amount_cents", eq: "45000", text: true }, { col: "status", eq: "open", text: true }] }, returning: "*" });
  });

  it("null_toggle_sends_null", async () => {
    const { f, user } = renderEditor();
    const panel = await open(user);
    await user.click(within(panel).getByRole("switch", { name: "Set memo to NULL" }));
    await user.click(within(panel).getByRole("button", { name: "Save row" }));
    await vi.waitFor(async () => expect(await writes(f)).toHaveLength(1));
    expect((await writes(f))[0].tree.set).toEqual({ memo: null });
  });

  it("insert_sends_only_filled_fields", async () => {
    const { f, user } = renderEditor();
    await user.click(await screen.findByRole("button", { name: "Insert row" }));
    const panel = await screen.findByRole("complementary", { name: "New row" });
    expect(within(panel).getByLabelText("id")).toHaveAttribute("placeholder", "default");
    await user.type(within(panel).getByLabelText("customer_id"), "c-9");
    await user.type(within(panel).getByLabelText("amount_cents"), "100");
    await user.click(within(panel).getByRole("button", { name: "Save row" }));
    await vi.waitFor(async () => expect(await writes(f)).toHaveLength(1));
    expect((await writes(f))[0].tree).toEqual({ v: 1, op: "insert", into: "public.invoices", values: [{ customer_id: "c-9", amount_cents: 100 }], returning: "*" });
  });

  it("insert_with_all_defaults", async () => {
    const insert = vi.fn(() => jsonResponse({ row: { ...ROWS[0], id: "new-1" } }));
    const { user } = renderEditor({ routes: { [`POST ${T}/rows/default`]: insert } });
    await user.click(await screen.findByRole("button", { name: "More insert options" }));
    await user.click(await screen.findByRole("menuitem", { name: "Insert with all defaults" }));
    await vi.waitFor(() => expect(insert).toHaveBeenCalledTimes(1));
    const req = (insert.mock.calls[0] as unknown as [Request])[0];
    expect(await req.clone().json()).toEqual({ database: null, role: null });
  });

  it("fk_pick_fills_the_field", async () => {
    const answer: FakeHandler = async (req) => {
      const b = await req.clone().json();
      if (b.tree.from === "public.customers") return jsonResponse([{ id: "c-7", name: "Lagoon Ltd" }]);
      return jsonResponse(ROWS);
    };
    const { f, user } = renderEditor({ routes: { [`POST ${Q}`]: answer } });
    const panel = await open(user);
    await user.click(within(panel).getByRole("button", { name: "Pick customer_id" }));
    const dialog = await screen.findByRole("dialog", { name: "Pick from customers" });
    await user.click(await within(dialog).findByRole("option", { name: /c-7/ }));
    expect(within(panel).getByLabelText("customer_id")).toHaveValue("c-7");
    const pick = (await bodies(f, "POST", Q)).find((b) => b.tree.from === "public.customers");
    expect(pick.tree.limit).toBe(20);
  });

  it("delete_row_confirms", async () => {
    const { f, user } = renderEditor();
    const panel = await open(user, 3);
    await user.click(within(panel).getByRole("button", { name: "Delete row" }));
    const dialog = await screen.findByRole("dialog", { name: "Delete this row?" });
    await user.click(within(dialog).getByRole("button", { name: "Delete row" }));
    await vi.waitFor(async () => expect(await writes(f)).toHaveLength(1));
    expect((await writes(f))[0].tree).toEqual({ v: 1, op: "delete", from: "public.invoices", where: { col: "id", eq: "c2d1-3", parse: true }, returning: ["id"] });
  });

  it("collapse_keeps_the_draft", async () => {
    const { user } = renderEditor();
    const panel = await open(user);
    await user.type(within(panel).getByLabelText("memo"), " more");
    await user.click(within(panel).getByRole("button", { name: "Collapse panel" }));
    expect(screen.queryByRole("complementary", { name: "Row" })).toBeNull();
    await user.click(screen.getByRole("button", { name: "Expand panel" }));
    expect(within(await screen.findByRole("complementary", { name: "Row" })).getByLabelText("memo")).toHaveValue("Q3 plan more");
  });

  it("field_filter_on_wide_tables", async () => {
    const wide = invoices();
    wide.columns = [...wide.columns, ...Array.from({ length: 30 }, (_, i) => ({ ...wide.columns[4], attnum: 6 + i, name: `extra_${i}` }))];
    const { user } = renderEditor({ table: wide });
    const panel = await open(user);
    await user.type(within(panel).getByLabelText("Filter fields"), "extra_2");
    expect(within(panel).queryByLabelText("memo")).toBeNull();
    expect(within(panel).getByLabelText("extra_2")).toBeInTheDocument();
  });

  it("oversized_rows_fall_back_to_small_columns_and_load_value", async () => {
    const answer: FakeHandler = async (req) => {
      const b = await req.clone().json();
      if (b.tree.op !== "select") return jsonResponse([]);
      // S9: every read lists its columns (plus text_of items); the full list, memo included, is too large.
      const names = ((b.tree.select ?? []) as unknown[]).filter((i) => typeof i === "string");
      return names.includes("memo") ? jsonResponse({ error: { code: "too_large", message: "big" } }, 413) : jsonResponse([{ id: "3f9a-1", customer_id: "c-1", status: "paid", amount_cents: 12900 }]);
    };
    const cellFetch = vi.fn(() => jsonResponse({ value: "very long memo", bytes: 9000000, data_type: "text" }));
    const { f, user } = renderEditor({ routes: { [`POST ${Q}`]: answer, [`POST ${T}/cell`]: cellFetch } });
    await user.click(await screen.findByRole("button", { name: "Load value of memo" }));
    expect(await screen.findByText("very long memo")).toBeInTheDocument();
    const req = (cellFetch.mock.calls[0] as unknown as [Request])[0];
    expect(await req.clone().json()).toEqual({ database: null, role: null, key: { id: "3f9a-1" }, column: "memo" });
    const last = (await bodies(f, "POST", Q)).at(-1);
    expect(last.tree.select.slice(0, 4)).toEqual(["id", "customer_id", "status", "amount_cents"]);
    expect(last.tree.select).not.toContain("memo");
    expect(last.tree.select).toContainEqual({ text_of: "id", as: "$pk0" });
    expect(last.tree.limit).toBe(1);
  });
});
