import { screen, within } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { B, FULL, bodies, renderEditor, stubLayout } from "@/test/table-editor-fixtures";

const SC = `${B}/schema-changes`;

describe("new table", () => {
  beforeEach(() => { localStorage.clear(); sessionStorage.clear(); stubLayout(); });

  it("creates_a_table_with_starter_columns_rls_off", async () => {
    const { f, user, navigate } = renderEditor();
    await user.click(await screen.findByRole("button", { name: "New table" }));
    const view = await screen.findByRole("region", { name: "New table" });
    await user.type(within(view).getByLabelText("Table name"), "payments");
    expect(within(view).getByText("id")).toBeInTheDocument();
    expect(within(view).getByText("created_at")).toBeInTheDocument();
    await user.click(within(view).getByRole("button", { name: "Create table" }));
    const panel = await screen.findByRole("complementary", { name: "Pending changes" });
    expect(within(panel).getByRole("button", { name: "Create table public.payments" })).toBeInTheDocument();
    const apply = within(panel).getByRole("button", { name: "Apply 1 change" });
    await vi.waitFor(() => expect(apply).toBeEnabled(), { timeout: 2000 });
    await user.click(apply);
    await vi.waitFor(async () => expect((await bodies(f, "POST", SC)).filter((b) => !b.dry_run)).toHaveLength(1));
    const op = (await bodies(f, "POST", SC)).filter((b) => !b.dry_run)[0].operations[0];
    expect(op).toEqual({ create_table: {
      table: { schema: "public", name: "payments" },
      columns: [
        { name: "id", data_type: { schema: "pg_catalog", name: "int8", array: false }, identity: "always", not_null: true },
        { name: "created_at", data_type: { schema: "pg_catalog", name: "timestamptz", array: false }, not_null: true, default: "now()" },
      ],
      primary_key: ["id"],
      comment: null,
    } });
    await vi.waitFor(() => expect(navigate).toHaveBeenLastCalledWith({ ...FULL, table: "payments" }));
  });

  it("starter_columns_can_be_removed", async () => {
    const { user } = renderEditor();
    await user.click(await screen.findByRole("button", { name: "New table" }));
    const view = await screen.findByRole("region", { name: "New table" });
    await user.click(within(view).getByRole("button", { name: "Remove column created_at" }));
    expect(within(view).queryByText("created_at")).toBeNull();
  });
});
