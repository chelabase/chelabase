import { screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it, vi } from "vitest";
import { B, BRANCHES, D, PG_DATABASES, PROJECTS, ROLES, meAs, type Level } from "@/test/table-editor-fixtures";
import { fakeFetch, jsonResponse, type FakeHandler } from "@/test/fake-fetch";
import { renderWithQuery } from "@/test/render";
import type { EditorParams } from "@/lib/table-editor/context";
import { SqlPage } from "./sql-page";

const PLACE: EditorParams = { projectId: "p1", databaseId: "d1", branchId: "b1", pgDatabase: "app" };

function setup(level: Level, params: EditorParams = PLACE, extra: Record<string, FakeHandler> = {}, roles: string[] = []) {
  const f = fakeFetch({
    "GET /v1/me": () => jsonResponse(meAs(level, roles)),
    "GET /v1/projects": () => jsonResponse(PROJECTS.map(({ databases: _d, ...p }) => p)),
    [`GET ${D}`]: () => jsonResponse({ database_id: "d1", name: "orders-db", status: "ready", history_window: 604800 }),
    [`GET ${D}/branches`]: () => jsonResponse(BRANCHES),
    [`GET ${B}/databases`]: () => jsonResponse(PG_DATABASES),
    [`GET ${B}/roles`]: () => jsonResponse(ROLES),
    [`GET ${B}/sql/schema`]: () => jsonResponse({ schemas: [] }),
    [`GET ${D}/sql-queries`]: () => jsonResponse([]),
    ...extra,
  });
  const navigate = vi.fn();
  const openTables = vi.fn();
  const result = renderWithQuery(<SqlPage params={params} navigate={navigate} openTables={openTables} createClient={() => Promise.reject(new Error("no worker"))} />, { fetch: f });
  return { ...result, f, navigate, openTables, user: userEvent.setup() };
}
const schemaCalls = (f: ReturnType<typeof fakeFetch>) => f.calls.filter((r) => r.url.includes("/sql/schema"));

afterEach(() => localStorage.clear());

describe("SqlPage", () => {
  it("a_viewer_gets_a_no_access_message_and_no_editor", async () => {
    const { f } = setup("viewer");
    expect(await screen.findByText(/needs the Developer role/)).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Run" })).toBeNull();
    expect(schemaCalls(f)).toHaveLength(0);
    // The context bar stays, to move to a database they can use.
    expect(screen.getByLabelText("Project")).toBeInTheDocument();
  });

  it("a_developer_gets_the_editor_under_the_shared_context_bar", async () => {
    const { f } = setup("developer", PLACE, {}, ["billing_admin"]);
    expect(await screen.findByRole("button", { name: "Run" })).toBeInTheDocument();
    for (const label of ["Project", "Database", "Branch", "Postgres db", "Act as"]) expect(await screen.findByLabelText(label)).toBeInTheDocument();
    // Developer acts as a granted role, not the owner: the schema read follows it.
    await waitFor(() => expect(new URL(schemaCalls(f).at(-1)!.url).searchParams.get("role")).toBe("billing_admin"));
  });

  it("a_partial_place_redirects_to_the_first_database_main_branch", async () => {
    const { navigate } = setup("admin", { projectId: "p1", databaseId: "d1" });
    await waitFor(() => expect(navigate).toHaveBeenCalled());
    expect(navigate.mock.calls[0][0]).toMatchObject({ projectId: "p1", databaseId: "d1", branchId: "b1" });
    expect(navigate.mock.calls[0][1]).toEqual({ replace: true });
  });

  it("the_last_place_is_remembered_under_its_own_key", async () => {
    setup("admin");
    await waitFor(() => expect(JSON.parse(localStorage.getItem("chelabase.studio.sql.last") ?? "null")).toEqual(PLACE));
    expect(localStorage.getItem("chelabase.studio.tables.last")).toBeNull();
  });

  // m4 (#86): Quick view opens the table editor with the drawer, on the table last open at this place, else the list.
  it("quick_view_opens_the_remembered_table_at_this_place", async () => {
    localStorage.setItem("chelabase.studio.tables.last", JSON.stringify({ ...PLACE, schema: "public", table: "orders" }));
    const { openTables, user } = setup("admin");
    await user.click(await screen.findByRole("button", { name: "Quick view" }));
    expect(openTables).toHaveBeenCalledWith({ ...PLACE, schema: "public", table: "orders" });
  });

  it("quick_view_opens_the_table_list_when_the_last_table_was_elsewhere", async () => {
    localStorage.setItem("chelabase.studio.tables.last", JSON.stringify({ ...PLACE, branchId: "b2", schema: "public", table: "orders" }));
    const { openTables, user } = setup("admin");
    await user.click(await screen.findByRole("button", { name: "Quick view" }));
    expect(openTables).toHaveBeenCalledWith(PLACE);
  });
});
