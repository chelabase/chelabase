import { screen } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { jsonResponse } from "@/test/fake-fetch";
import { FULL, PROJECTS, meAs, renderEditor, stubLayout } from "@/test/table-editor-fixtures";

describe("TableEditor projects", () => {
  beforeEach(() => { localStorage.clear(); sessionStorage.clear(); stubLayout(); });

  it("takes_each_projects_databases_from_me_when_the_list_has_none", async () => {
    // The real `/v1/projects` lists ProjectInfo (no databases); `/v1/me` carries them.
    const me = { ...meAs("admin"), tenant: { ...meAs("admin").tenant, projects: PROJECTS } };
    const bare = PROJECTS.map(({ databases: _d, ...p }) => p);
    const { navigate } = renderEditor({ me: me as never, params: {}, routes: { "GET /v1/projects": () => jsonResponse(bare) } });
    await vi.waitFor(() => expect(navigate).toHaveBeenCalledWith({ projectId: "p1", databaseId: "d1" }, { replace: true }));
  });

  it("shows_the_database_picker_from_me", async () => {
    const me = { ...meAs("admin"), tenant: { ...meAs("admin").tenant, projects: PROJECTS } };
    const bare = PROJECTS.map(({ databases: _d, ...p }) => p);
    renderEditor({ me: me as never, params: FULL, routes: { "GET /v1/projects": () => jsonResponse(bare) } });
    expect(await screen.findByRole("combobox", { name: "Database" })).toHaveValue("orders-db");
  });
});
