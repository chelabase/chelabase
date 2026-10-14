import { screen, within } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { jsonResponse } from "@/test/fake-fetch";
import { BRANCHES, D, branch, renderEditor, stubLayout } from "@/test/table-editor-fixtures";

// User, 2026-10-14 (browser recheck, then "Both"): the Branch picker refetches the branch list
// each time it opens, and the list polls only while a branch this editor just created is still
// in transition; a branch busy for another reason starts no polling.
describe("branch picker refresh", () => {
  beforeEach(() => { localStorage.clear(); sessionStorage.clear(); stubLayout(); });

  it("opening_the_picker_refetches_the_list", async () => {
    let calls = 0;
    const list = () => {
      calls += 1;
      return jsonResponse(calls < 2 ? BRANCHES : [...BRANCHES, branch("b9", "fresh", "b1")]);
    };
    const { user } = renderEditor({ routes: { [`GET ${D}/branches`]: list } });
    await screen.findByRole("combobox", { name: "Branch" });
    await vi.waitFor(() => expect(calls).toBe(1));
    await user.click(screen.getByLabelText("Branch"));
    expect(await screen.findByRole("option", { name: /^fresh/ })).toBeInTheDocument();
  });

  it("a_branch_busy_elsewhere_starts_no_polling", async () => {
    let calls = 0;
    const list = () => {
      calls += 1;
      return jsonResponse([...BRANCHES, branch("b9", "other", "b1", { status: "provisioning" })]);
    };
    renderEditor({ routes: { [`GET ${D}/branches`]: list } });
    await screen.findByRole("combobox", { name: "Branch" });
    await new Promise((r) => setTimeout(r, 4500));
    expect(calls).toBe(1);
  });

  it("a_branch_created_here_is_polled_until_ready", async () => {
    let created = false;
    let afterCreate = 0;
    const list = () => {
      if (!created) return jsonResponse(BRANCHES);
      afterCreate += 1;
      return jsonResponse([...BRANCHES, branch("b9", "fresh", "b1", { status: afterCreate < 3 ? "provisioning" : "ready" })]);
    };
    const create = () => {
      created = true;
      return jsonResponse(branch("b9", "fresh", "b1", { status: "provisioning" }), 202);
    };
    const { user } = renderEditor({ routes: { [`GET ${D}/branches`]: list, [`POST ${D}/branches`]: create } });
    await user.click(await screen.findByLabelText("Branch"));
    await user.click(await screen.findByRole("option", { name: /New branch from main/ }));
    const dialog = await screen.findByRole("dialog");
    await user.type(within(dialog).getByLabelText("Branch name"), "fresh{Enter}");
    await vi.waitFor(() => expect(afterCreate).toBeGreaterThanOrEqual(3), { timeout: 9000 });
    const settled = afterCreate;
    await new Promise((r) => setTimeout(r, 4500));
    expect(afterCreate).toBe(settled);
  }, 30_000);
});
