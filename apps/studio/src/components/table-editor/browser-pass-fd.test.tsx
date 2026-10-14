import { screen, within } from "@testing-library/react";
import { beforeEach, describe, expect, it } from "vitest";
import { renderEditor, stubLayout } from "@/test/table-editor-fixtures";
import { dataApiActAs, projectCaps } from "@/lib/capabilities";

// FD's own tests for the browser pass's M7 and R15 (beside the coordinator's browser-pass-fixes).
describe("browser pass fixes (FD)", () => {
  beforeEach(() => { localStorage.clear(); sessionStorage.clear(); stubLayout(); });

  // jsdom has no layout, so where the list opens (M7) is checked in a browser; this keeps its name.
  it("M7_the_add_constraint_dialog_keeps_its_name_while_the_columns_list_is_open", async () => {
    const { user } = renderEditor();
    await user.click(await screen.findByRole("tab", { name: "Structure" }));
    await user.click(screen.getByRole("button", { name: "Add constraint" }));
    const d = await screen.findByRole("dialog", { name: "Add constraint" });
    await user.click(within(d).getByLabelText("Columns"));
    await screen.findByRole("listbox");
    expect(screen.getByRole("dialog", { name: "Add constraint" })).toBe(d);
  });

  it("R15_data_api_roles_by_grant", () => {
    const me = (platform_role: "viewer" | "developer" | "admin", roles: string[]) => ({
      sub: "u", email: "u@x", name: "U", tenant_id: "t1", member: { id: "m", status: "active", tenant_admin: false },
      instance_admin: false, tenant_admins: [], projects: [{ project_id: "p1", platform_role, roles, databases: [] }],
      user: { sub: "u", org_id: "o" }, tenant: { id: "t1", slug: "s", display_name: null, projects: [] },
    }) as unknown as Parameters<typeof projectCaps>[0];
    // R18: an Admin too only when the grant names them; every role ("all") doesn't.
    expect(dataApiActAs(projectCaps(me("admin", []), "p1"))).toEqual([]);
    expect(dataApiActAs(projectCaps(me("admin", ["owner", "anon"]), "p1"))).toEqual(["anon"]);
    expect(dataApiActAs(projectCaps(me("developer", ["app_rw"]), "p1"))).toEqual([]);
    expect(dataApiActAs(projectCaps(me("viewer", ["authenticated"]), "p1"))).toEqual(["authenticated"]);
  });
});
