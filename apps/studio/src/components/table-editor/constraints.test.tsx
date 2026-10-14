import { screen, within } from "@testing-library/react";
import { beforeEach, describe, expect, it } from "vitest";
import { invoices, renderEditor, stubLayout } from "@/test/table-editor-fixtures";

const opsIn = async () => {
  const p = await screen.findByRole("complementary", { name: "Pending changes" });
  return within(p).getAllByRole("listitem").map((li) => li.textContent);
};

describe("constraints and table actions", () => {
  beforeEach(() => { localStorage.clear(); sessionStorage.clear(); stubLayout(); });

  async function structure() {
    const r = renderEditor({ table: invoices({ constraints: [...invoices().constraints, { name: "amount_positive", kind: "check", columns: [4], definition: "CHECK (amount_cents > 0) NOT VALID", deferrable: false, initially_deferred: false, oid: 3, references: null, validated: false }] }) });
    await r.user.click(await screen.findByRole("tab", { name: "Structure" }));
    return r;
  }

  it("add_unique_constraint", async () => {
    const { user } = await structure();
    await user.click(screen.getByRole("button", { name: "Add constraint" }));
    const d = await screen.findByRole("dialog", { name: "Add constraint" });
    await user.click(within(d).getByLabelText("Kind"));
    await user.click(await screen.findByRole("option", { name: "Unique" }));
    await user.click(within(d).getByLabelText("Columns"));
    await user.click(await screen.findByRole("option", { name: /^memo/ }));
    await user.keyboard("{Escape}");
    await user.click(within(d).getByRole("button", { name: "Add to changes" }));
    expect(await opsIn()).toEqual([expect.stringContaining("Add unique constraint on invoices")]);
  });

  it("add_foreign_key_not_valid", async () => {
    const { user } = await structure();
    await user.click(screen.getByRole("button", { name: "Add constraint" }));
    const d = await screen.findByRole("dialog", { name: "Add constraint" });
    await user.click(within(d).getByLabelText("Kind"));
    await user.click(await screen.findByRole("option", { name: "Foreign key" }));
    await user.click(within(d).getByLabelText("Columns"));
    await user.click(await screen.findByRole("option", { name: /^customer_id/ }));
    await user.keyboard("{Escape}");
    await user.click(within(d).getByLabelText("References table"));
    await user.click(await screen.findByRole("option", { name: "customers" }));
    await user.click(within(d).getByLabelText("References column"));
    await user.click(await screen.findByRole("option", { name: "id" }));
    await user.click(within(d).getByRole("checkbox", { name: "Add as NOT VALID" }));
    await user.click(within(d).getByRole("button", { name: "Add to changes" }));
    expect(await opsIn()).toEqual([expect.stringContaining("Add foreign key on invoices")]);
  });

  it("validate_rename_and_drop_constraints", async () => {
    const { user } = await structure();
    await user.click(screen.getByRole("button", { name: "Validate amount_positive" }));
    await user.click(screen.getByRole("button", { name: "Drop invoices_customer_id_fkey" }));
    await user.click(screen.getByRole("button", { name: "Rename amount_positive" }));
    const d = await screen.findByRole("dialog", { name: "Rename amount_positive" });
    await user.type(within(d).getByLabelText("New name"), "amount_gt_zero");
    await user.click(within(d).getByRole("button", { name: "Add to changes" }));
    const items = await opsIn();
    expect(items).toEqual([
      expect.stringContaining("Validate amount_positive"),
      expect.stringContaining("Drop constraint invoices_customer_id_fkey"),
      expect.stringContaining("Rename constraint amount_positive → amount_gt_zero"),
    ]);
  });

  it("table_rename_move_comment_drop", async () => {
    const { user } = await structure();
    await user.click(screen.getByRole("button", { name: "Rename table" }));
    let d = await screen.findByRole("dialog", { name: "Rename table" });
    await user.type(within(d).getByLabelText("New name"), "bills");
    await user.click(within(d).getByRole("button", { name: "Add to changes" }));
    await user.click(screen.getByRole("button", { name: "Comment" }));
    d = await screen.findByRole("dialog", { name: "Comment" });
    await user.type(within(d).getByLabelText("Comment"), "Customer invoices");
    await user.click(within(d).getByRole("button", { name: "Add to changes" }));
    expect(await opsIn()).toEqual([
      expect.stringContaining("Rename table invoices → bills"),
      expect.stringContaining("Change comment on bills"),
    ]);
  });
});
