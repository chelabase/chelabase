import { screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it, vi } from "vitest";
import { B, BRANCHES, D, PG_DATABASES, PROJECTS, ROLES, meAs, type Level } from "@/test/table-editor-fixtures";
import { fakeFetch, jsonResponse, type FakeHandler } from "@/test/fake-fetch";
import { renderWithQuery } from "@/test/render";
import type { EditorParams } from "@/lib/table-editor/context";
import { DataApiPage } from "./data-api-page";

const PLACE: EditorParams = { projectId: "p1", databaseId: "d1", branchId: "b1", pgDatabase: "app" };
const PUB = "chk_pub_0123456789abcdefghijklmnopqrstuvwxyzABCDEFG";
const KEYS = [
  { id: "pub", kind: "public", databases: null, role: "anon", name: "public", prefix: "chk_pub_0123", created_at: "2026-10-01T09:00:00Z", last_used_at: null, role_blocked: false, bypasses_rls: false, key: PUB },
  { id: "k1", kind: "secret", databases: null, role: "authenticated", name: "reporting", prefix: "chk_sec_ab12", created_at: "2026-10-01T09:00:00Z", last_used_at: null, role_blocked: false, bypasses_rls: false },
];
const SCHEMA = { version: "1", default_schema: "public", schemas: {} };

/** A database's routes: its detail, branches, and branch b1's Postgres databases, roles, keys and schema. */
function databaseRoutes(db: string, name: string): Record<string, FakeHandler> {
  const d = `/v1/databases/${db}`;
  return {
    [`GET ${d}`]: () => jsonResponse({ database_id: db, name, status: "ready", history_window: 604800 }),
    [`GET ${d}/branches`]: () => jsonResponse(BRANCHES),
    [`GET ${d}/branches/b1/databases`]: () => jsonResponse(PG_DATABASES),
    [`GET ${d}/branches/b1/roles`]: () => jsonResponse(ROLES),
    [`GET ${d}/branches/b1/keys`]: () => jsonResponse(KEYS),
    [`GET ${d}/branches/b1/data-api/schema`]: () => jsonResponse(SCHEMA),
  };
}

function setup(me: ReturnType<typeof meAs>, params: EditorParams = PLACE, extra: Record<string, FakeHandler> = {}) {
  const f = fakeFetch({
    "GET /v1/me": () => jsonResponse(me),
    "GET /v1/projects": () => jsonResponse(PROJECTS.map(({ databases: _d, ...p }) => p)),
    ...databaseRoutes("d1", "orders-db"),
    ...databaseRoutes("d2", "analytics"),
    ...extra,
  });
  const navigate = vi.fn();
  const ui = (p: EditorParams) => <DataApiPage params={p} navigate={navigate} apiUrl="http://gw/" />;
  const result = renderWithQuery(ui(params), { fetch: f });
  return { ...result, f, navigate, user: userEvent.setup(), goTo: (p: EditorParams) => result.rerender(ui(p)) };
}
const at = (level: Level) => meAs(level);
const keyOrSchemaCalls = (f: ReturnType<typeof fakeFetch>) => f.calls.filter((r) => /\/keys|\/data-api\/schema/.test(new URL(r.url).pathname));

/** Every management control the page offers a Developer or an Admin. */
const CONTROLS = ["Rotate public key", "Create key", "Download types", "Enable data API", "Disable data API"];
const controls = () => CONTROLS.filter((name) => screen.queryByRole("button", { name }) !== null);

afterEach(() => {
  localStorage.clear();
  sessionStorage.clear();
});

describe("DataApiPage", () => {
  it("a_developer_sees_every_section_under_the_context_bar_without_act_as", async () => {
    setup(at("developer"));
    expect(await screen.findByText(PUB)).toBeInTheDocument();
    expect(screen.getByText("http://gw/data/v1/query")).toBeInTheDocument();
    for (const name of ["Endpoint and public key", "Postgres databases", "Secret keys", "Row-level security", "Generated types"]) {
      expect(screen.getAllByRole("region", { name }).length, name).toBeGreaterThan(0);
    }
    expect(await screen.findByRole("button", { name: "Rotate public key" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Create key" })).toBeInTheDocument();
    expect(await screen.findByRole("button", { name: "Download types" })).toBeInTheDocument();
    for (const label of ["Project", "Database", "Branch", "Postgres db"]) expect(screen.getByLabelText(label)).toBeInTheDocument();
    // Keys aren't roles: no Act as.
    expect(screen.queryByLabelText("Act as")).toBeNull();
  });

  it("an_admin_also_gets_enable_and_disable_for_the_picked_database", async () => {
    setup(at("admin"), { ...PLACE, pgDatabase: "reporting" });
    expect(await screen.findByRole("button", { name: "Enable data API" })).toBeInTheDocument();
  });

  it("a_viewer_sees_the_endpoint_and_public_key_but_no_management_controls", async () => {
    setup(at("viewer"), { ...PLACE, pgDatabase: "reporting" });
    expect(await screen.findByText(PUB)).toBeInTheDocument();
    expect(screen.getByText("http://gw/data/v1/query")).toBeInTheDocument();
    expect(await screen.findByText("reporting", { selector: "td" })).toBeInTheDocument();
    expect(controls()).toEqual([]);
  });

  it("switching_from_a_developer_database_to_a_viewer_database_hides_the_controls", async () => {
    // Developer on the project, narrowed to Viewer on d2.
    const me = at("developer");
    me.projects[0].databases = [{ database_id: "d2", platform_role: "viewer" }];
    const { f, goTo } = setup(me);
    expect(await screen.findByRole("button", { name: "Rotate public key" })).toBeInTheDocument();
    expect(await screen.findByRole("button", { name: "Create key" })).toBeInTheDocument();
    expect(await screen.findByRole("button", { name: "Download types" })).toBeInTheDocument();

    goTo({ ...PLACE, databaseId: "d2" });
    // The controls go at once, before d2's keys arrive, and stay gone once they have.
    expect(controls()).toEqual([]);
    await waitFor(() => expect(f.calls.some((r) => new URL(r.url).pathname === "/v1/databases/d2/branches/b1/keys")).toBe(true));
    expect(await screen.findByText(PUB)).toBeInTheDocument();
    expect(await screen.findByText("reporting", { selector: "td" })).toBeInTheDocument();
    expect(controls()).toEqual([]);
  });

  it("a_branch_that_isnt_ready_says_so_and_asks_for_no_keys_or_schema", async () => {
    const { f } = setup(at("developer"), { ...PLACE, branchId: "b5", pgDatabase: undefined }, {
      [`GET ${D}/branches/b5/databases`]: () => jsonResponse(PG_DATABASES),
      [`GET ${D}/branches/b5/roles`]: () => jsonResponse(ROLES),
    });
    expect(await screen.findByText("The Data API runs on ready branches.")).toBeInTheDocument();
    expect(keyOrSchemaCalls(f)).toEqual([]);
  });

  it("picking_a_branch_in_the_bar_navigates_and_the_sections_follow", async () => {
    const B2 = `${D}/branches/b2`;
    const { f, navigate, user, goTo } = setup(at("developer"), PLACE, {
      [`GET ${B2}/databases`]: () => jsonResponse(PG_DATABASES),
      [`GET ${B2}/roles`]: () => jsonResponse(ROLES),
      [`GET ${B2}/keys`]: () => jsonResponse(KEYS),
      [`GET ${B2}/data-api/schema`]: () => jsonResponse(SCHEMA),
    });
    await screen.findByText(PUB);
    await user.click(screen.getByLabelText("Branch"));
    await user.click(await screen.findByRole("option", { name: /^staging/ }));
    expect(navigate).toHaveBeenCalledWith(expect.objectContaining({ projectId: "p1", databaseId: "d1", branchId: "b2" }));

    goTo({ ...PLACE, branchId: "b2" });
    await waitFor(() => expect(f.calls.some((r) => new URL(r.url).pathname === `${B2}/keys`)).toBe(true));
    await screen.findByText(PUB);
    expect(f.calls.some((r) => new URL(r.url).pathname === `${B}/keys`)).toBe(true);
  });

  it("a_partial_place_redirects_and_the_last_place_is_remembered_under_its_own_key", async () => {
    const { navigate } = setup(at("developer"), { projectId: "p1", databaseId: "d1" });
    await waitFor(() => expect(navigate).toHaveBeenCalled());
    expect(navigate.mock.calls[0][0]).toMatchObject({ projectId: "p1", databaseId: "d1", branchId: "b1" });
    expect(navigate.mock.calls[0][1]).toEqual({ replace: true });
    setup(at("developer"));
    await waitFor(() => expect(JSON.parse(localStorage.getItem("chelabase.studio.data-api.last") ?? "null")).toEqual(PLACE));
    expect(localStorage.getItem("chelabase.studio.sql.last")).toBeNull();
  });
});
