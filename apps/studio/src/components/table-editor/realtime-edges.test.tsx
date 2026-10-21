import { screen, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { Table } from "@chelabase/management-client";
import { jsonResponse } from "@/test/fake-fetch";
import { FakeRealtime } from "@/test/fake-realtime";
import { B, ROWS, TYPES, bodies, invoices, renderEditor, stubLayout } from "@/test/table-editor-fixtures";

// Edge cases beyond the RED set (coder's probes).

const Q = `${B}/console/query`;
const key = (id: string) => JSON.stringify([id]);
const grid = () => screen.findByRole("grid", { name: "public.invoices" });
const cell = async (text: string) => within(await grid()).findByRole("gridcell", { name: text });
const writes = async (f: Parameters<typeof bodies>[0]) => (await bodies(f, "POST", Q)).filter((b) => b.tree.op !== "select" && b.tree.op !== "count");

beforeEach(() => {
  localStorage.clear();
  sessionStorage.clear();
  stubLayout();
});
afterEach(() => vi.unstubAllGlobals());

describe("realtime edges", () => {
  it("enter_on_an_untouched_editor_after_a_splice_writes_nothing", async () => {
    const rt = new FakeRealtime();
    const data = ROWS.map((r) => ({ ...r }));
    const { f, user } = renderEditor({ realtime: rt, routes: { [`POST ${Q}`]: () => jsonResponse(data) } });
    await vi.waitFor(() => expect(rt.active).toHaveLength(1));
    await user.dblClick(await cell("45000"));
    await screen.findByRole("textbox", { name: "amount_cents" });
    data[1].amount_cents = 46000;
    rt.rows(1, [{ op: "update", keys: [key("81c4-2")] }]);
    await screen.findByTestId("changed-marker");
    await user.keyboard("{Enter}");
    await vi.waitFor(() => expect(screen.queryByRole("textbox", { name: "amount_cents" })).toBeNull());
    expect(await writes(f)).toHaveLength(0);
  });

  it("columns_the_check_cannot_compare_are_left_out_and_the_save_says_so", async () => {
    const base = invoices();
    const table = {
      ...base,
      columns: [
        ...base.columns,
        { ...base.columns[4], attnum: 6, name: "doc", data_type: "xml", type_oid: 142 },
        { ...base.columns[4], attnum: 7, name: "addr", data_type: "public.address", type_oid: 17000 },
      ],
    } as unknown as Table;
    const rows = ROWS.map((r) => ({ ...r, doc: "<a/>", addr: "(1,x)" }));
    const types = { types: [...TYPES.types, { oid: 17000, array_oid: 17001, schema: "public", name: "address", kind: "composite", category: "C", preferred: false, takes_modifiers: false }] };
    const { f, user } = renderEditor({ table, rows, routes: { [`GET ${B}/types`]: () => jsonResponse(types) } });
    await cell("45000");
    const select = (await bodies(f, "POST", Q)).filter((b) => b.tree.op === "select").at(-1).tree.select as unknown[];
    const compared = select.filter((i): i is { text_of: string } => typeof i === "object" && i !== null).map((i) => i.text_of);
    expect(compared).toContain("memo");
    expect(compared).not.toContain("doc");
    expect(compared).not.toContain("addr");
    await user.dblClick(within(await grid()).getAllByRole("gridcell", { name: "<a/>" })[1]);
    const input = await screen.findByRole("textbox", { name: "doc" });
    await user.clear(input);
    await user.type(input, "<b/>{Enter}");
    await vi.waitFor(async () => expect(await writes(f)).toHaveLength(1));
    expect((await writes(f))[0].tree.where).toEqual({ col: "id", eq: "81c4-2", parse: true });
    expect(await screen.findByText("No conflict check for this column type: doc.")).toBeInTheDocument();
  });

  it("without_a_realtime_client_there_is_no_live_state_presence_or_avatar_stack", async () => {
    renderEditor();
    await cell("45000");
    expect(screen.queryByTestId("live-state")).toBeNull();
    expect(screen.queryByTestId("presence-layer")).toBeNull();
    expect(screen.queryByTestId("avatar-stack")).toBeNull();
  });

  it("a_view_does_not_join_a_table_channel", async () => {
    const rt = new FakeRealtime();
    renderEditor({ realtime: rt, params: { projectId: "p1", databaseId: "d1", branchId: "b1", pgDatabase: "app", schema: "public", table: "v_open" } });
    await vi.waitFor(() => expect(rt.projects.some((p) => !p.left)).toBe(true));
    await screen.findByText("Views are read-only here.");
    expect(rt.tables).toHaveLength(0);
  });
});
