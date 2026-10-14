import { screen, within } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { renderEditor, stubLayout } from "@/test/table-editor-fixtures";

// FA, implementer's own: focus after "Add to changes" unmounts the column form (T5 M6).
describe("focus after Add to changes", () => {
  beforeEach(() => {
    localStorage.clear();
    sessionStorage.clear();
    stubLayout();
  });

  it("moves_to_the_pending_changes_heading", async () => {
    const { user } = renderEditor();
    await user.click(await screen.findByRole("button", { name: "Add column" }));
    const form = await screen.findByRole("complementary", { name: /column/i });
    await user.type(within(form).getByLabelText("Name"), "note");
    await user.click(within(form).getByLabelText("Type"));
    await user.click(await screen.findByRole("option", { name: /^text/ }));
    await user.click(within(form).getByRole("button", { name: "Add to changes" }));
    const panel = await screen.findByRole("complementary", { name: "Pending changes" });
    await vi.waitFor(() => expect(document.activeElement).toBe(within(panel).getByRole("heading", { level: 2 })));
  });
});
