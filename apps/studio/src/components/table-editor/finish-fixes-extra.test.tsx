import { screen, within } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type { Table } from "@chelabase/management-client";
import { B, bodies, invoices, renderEditor, stubLayout } from "@/test/table-editor-fixtures";
import { shortType } from "./column-strip";

const Q = `${B}/console/query`;
const selects = async (f: Parameters<typeof bodies>[0]) => (await bodies(f, "POST", Q)).filter((b) => b.tree.op === "select");

describe("finish review fixes (implementer's own)", () => {
  beforeEach(() => { localStorage.clear(); sessionStorage.clear(); stubLayout(); });

  it("short_type_names", () => {
    expect(shortType("bigint")).toBe("int8");
    expect(shortType("integer[]")).toBe("int4[]");
    expect(shortType("timestamp with time zone")).toBe("timestamptz");
    expect(shortType("timestamp(3) without time zone")).toBe("timestamp(3)");
    expect(shortType("character varying(80)")).toBe("varchar(80)");
    expect(shortType("character(2)")).toBe("char(2)");
    expect(shortType("double precision")).toBe("float8");
    expect(shortType("numeric(10,2)")).toBe("numeric(10,2)");
    expect(shortType("invoice_status")).toBe("invoice_status");
  });

  it("structure_shows_an_identity_columns_generation_as_its_default", async () => {
    const t = invoices();
    t.columns = t.columns.map((c) => (c.name === "id" ? { ...c, default: null, identity: { generation: "by_default", sequence_oid: 1, sequence_name: "invoices_id_seq", params: {}, grants: [] } } : c)) as Table["columns"];
    const { user } = renderEditor({ table: t });
    await user.click(await screen.findByRole("tab", { name: "Structure" }));
    expect(await screen.findByText("identity by default")).toBeInTheDocument();
  });

  it("clear_sort_returns_to_the_key_order", async () => {
    const { f, user } = renderEditor();
    await within(await screen.findByRole("grid", { name: "public.invoices" })).findByRole("gridcell", { name: "45000" });
    await user.click(within(await screen.findByRole("grid", { name: "public.invoices" })).getByRole("columnheader", { name: "memo" }));
    await vi.waitFor(async () => expect((await selects(f)).at(-1).tree.orderBy?.[0]).toEqual({ col: "memo", dir: "asc" }));
    await user.click(screen.getByRole("button", { name: "Sort" }));
    expect(screen.getByLabelText("Sort column")).toHaveValue("memo");
    await user.click(screen.getByRole("button", { name: "Clear sort" }));
    await vi.waitFor(async () => expect((await selects(f)).at(-1).tree.orderBy).toEqual([{ col: "id", dir: "asc" }]));
  });
});
