import { screen } from "@testing-library/react";
import { beforeEach, describe, expect, it } from "vitest";
import { meAs, renderEditor, stubLayout } from "@/test/table-editor-fixtures";

// R17/R18 (user, 2026-10-14): Act as lists exactly the granted roles. A project Admin holds every
// role ("all"); an Admin narrowed on this database to named roles sees only those and the owner.
describe("act as follows the grant", () => {
  beforeEach(() => { localStorage.clear(); sessionStorage.clear(); stubLayout(); });

  it("a_project_admin_with_every_role_sees_them_all", async () => {
    const { user } = renderEditor();
    await user.click(await screen.findByLabelText("Act as"));
    for (const name of ["owner", "billing_admin", "reporter"]) {
      expect(await screen.findByRole("option", { name })).toBeInTheDocument();
    }
    // R18: the data API's roles only when a grant names them (View as comes in TE-B2).
    for (const name of ["anon", "authenticated"]) expect(screen.queryByRole("option", { name })).toBeNull();
  });

  it("an_admin_narrowed_on_this_database_sees_only_the_narrowed_roles", async () => {
    const me = meAs("admin");
    me.projects = me.projects.map((p) => ({ ...p, databases: [{ database_id: "d1", platform_role: null, roles: ["billing_admin"] }] }));
    const { user } = renderEditor({ me });
    await user.click(await screen.findByLabelText("Act as"));
    expect(await screen.findByRole("option", { name: "billing_admin" })).toBeInTheDocument();
    expect(screen.getByRole("option", { name: "owner" })).toBeInTheDocument();
    for (const name of ["reporter", "anon", "authenticated"]) expect(screen.queryByRole("option", { name })).toBeNull();
  });
});
