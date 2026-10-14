import { screen, within } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { B, bodies, renderEditor, stubLayout } from "@/test/table-editor-fixtures";

const SC = `${B}/schema-changes`;
const pending = async () => within(await screen.findByRole("complementary", { name: "Pending changes" })).getAllByRole("listitem").map((li) => li.textContent);

describe("types view", () => {
  beforeEach(() => { localStorage.clear(); sessionStorage.clear(); stubLayout(); });

  async function types() {
    const r = renderEditor();
    await r.user.click(await screen.findByRole("button", { name: "Types" }));
    await screen.findByRole("heading", { name: "Types in public" });
    return r;
  }

  it("lists_enums_with_values", async () => {
    await types();
    const section = screen.getByRole("region", { name: "invoice_status" });
    for (const v of ["open", "paid", "void"]) expect(within(section).getByText(v)).toBeInTheDocument();
  });

  it("new_enum_and_add_value_after", async () => {
    const { f, user } = await types();
    await user.click(screen.getByRole("button", { name: "New enum" }));
    let d = await screen.findByRole("dialog", { name: "New enum" });
    await user.type(within(d).getByLabelText("Enum name"), "tier");
    await user.type(within(d).getByLabelText("Values"), "free{Enter}pro");
    await user.click(within(d).getByRole("button", { name: "Add to changes" }));
    await user.click(screen.getByRole("button", { name: "Add value to invoice_status" }));
    d = await screen.findByRole("dialog", { name: "Add value to invoice_status" });
    await user.type(within(d).getByLabelText("Value"), "refunded");
    await user.click(within(d).getByLabelText("Position"));
    await user.click(await screen.findByRole("option", { name: "After…" }));
    await user.click(within(d).getByLabelText("Relative to"));
    await user.click(await screen.findByRole("option", { name: "paid" }));
    await user.click(within(d).getByRole("button", { name: "Add to changes" }));
    expect(await pending()).toEqual([expect.stringContaining("Create enum public.tier"), expect.stringContaining("Add value refunded to invoice_status")]);
    // Waits for the dry run of both edits (not only "some" dry run), so it holds with or without the dialog pause.
    await vi.waitFor(async () => expect((await bodies(f, "POST", SC)).at(-1)?.operations).toHaveLength(2), { timeout: 3000 });
    expect((await bodies(f, "POST", SC)).at(-1).operations).toEqual([
      { create_enum: { schema: "public", name: "tier", values: ["free", "pro"] } },
      { add_enum_value: { schema: "public", name: "invoice_status", value: "refunded", position: { after: "paid" } } },
    ]);
  });

  it("rename_value_rename_and_drop", async () => {
    const { user } = await types();
    await user.click(screen.getByRole("button", { name: "Rename value in invoice_status" }));
    const d = await screen.findByRole("dialog", { name: "Rename value in invoice_status" });
    await user.click(within(d).getByLabelText("From"));
    await user.click(await screen.findByRole("option", { name: "void" }));
    await user.type(within(d).getByLabelText("To"), "cancelled");
    await user.click(within(d).getByRole("button", { name: "Add to changes" }));
    await user.click(screen.getByRole("button", { name: "Drop invoice_status" }));
    expect(await pending()).toEqual([expect.stringContaining("Rename value void → cancelled in invoice_status"), expect.stringContaining("Drop enum public.invoice_status")]);
  });
});
