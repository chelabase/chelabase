import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it, vi } from "vitest";
import { B, BRANCHES, D, PG_DATABASES, PROJECTS, ROLES, meAs, pgDb, type Level } from "@/test/table-editor-fixtures";
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

/** Every management control the page offers a Developer or an Admin (the fixture's secret key is `reporting`). */
const CONTROLS = [
  "Rotate public key",
  "Create key",
  "Download types",
  "Enable data API",
  "Disable data API",
  "Rotate reporting",
  "Revoke reporting",
  "Edit databases",
  "Save default schema",
];
const controls = () => CONTROLS.filter((name) => screen.queryAllByRole("button", { name }).length > 0);

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
    const PUB2 = "chk_pub_STAGINGSTAGINGSTAGINGSTAGINGSTAGINGSTAGING";
    const { f, navigate, user, goTo } = setup(at("developer"), PLACE, {
      [`GET ${B2}/databases`]: () => jsonResponse(PG_DATABASES),
      [`GET ${B2}/roles`]: () => jsonResponse(ROLES),
      [`GET ${B2}/keys`]: () => jsonResponse([{ ...KEYS[0], id: "pub2", key: PUB2 }, KEYS[1]]),
      [`GET ${B2}/data-api/schema`]: () => jsonResponse(SCHEMA),
    });
    await screen.findByText(PUB);
    await user.click(screen.getByLabelText("Branch"));
    await user.click(await screen.findByRole("option", { name: /^staging/ }));
    expect(navigate).toHaveBeenCalledWith(expect.objectContaining({ projectId: "p1", databaseId: "d1", branchId: "b2" }));

    goTo({ ...PLACE, branchId: "b2" });
    await waitFor(() => expect(f.calls.some((r) => new URL(r.url).pathname === `${B2}/keys`)).toBe(true));
    expect(await screen.findByText(PUB2)).toBeInTheDocument();
    expect(screen.queryByText(PUB)).not.toBeInTheDocument();
    expect(f.calls.some((r) => new URL(r.url).pathname === `${B}/keys`)).toBe(true);
  });

  it("picking_a_postgres_database_in_the_bar_makes_the_sections_follow", async () => {
    const dbs = [...PG_DATABASES, pgDb("events", false, "on")];
    const { f, navigate, user, goTo } = setup(at("developer"), PLACE, { [`GET ${B}/databases`]: () => jsonResponse(dbs) });
    await screen.findByText(PUB);
    const schemaDbs = () =>
      f.calls.filter((r) => new URL(r.url).pathname === `${B}/data-api/schema`).map((r) => new URL(r.url).searchParams.get("database"));
    await waitFor(() => expect(schemaDbs()).toEqual(["app"]));
    await user.click(screen.getByLabelText("Postgres db"));
    await user.click(await screen.findByRole("option", { name: /^events/ }));
    expect(navigate).toHaveBeenCalledWith(expect.objectContaining({ branchId: "b1", pgDatabase: "events" }));

    goTo({ ...PLACE, pgDatabase: "events" });
    await waitFor(() => expect(schemaDbs()).toEqual(["app", "events"]));
    const exposure = screen.getByRole("region", { name: "Postgres databases" });
    expect(within(exposure).getByText("events")).toBeInTheDocument();
    expect(within(exposure).queryByText("app")).not.toBeInTheDocument();
    await waitFor(() => expect(screen.getByRole("option", { name: "events", selected: true })).toBeInTheDocument());
  });

  it("the_rls_card_says_when_the_picked_database_isnt_served_and_while_it_checks", async () => {
    const { unmount } = setup(at("developer"), { ...PLACE, pgDatabase: "reporting" });
    const rls = await screen.findByRole("region", { name: "Row-level security" });
    expect(await within(rls).findByText("The Data API doesn't serve reporting.")).toBeInTheDocument();
    expect(within(rls).queryByText(/are listed here/)).not.toBeInTheDocument();
    unmount();

    setup(at("developer"), PLACE, { [`GET ${B}/data-api/schema`]: () => new Promise<never>(() => {}) });
    const checking = await screen.findByRole("region", { name: "Row-level security" });
    expect(await within(checking).findByText("Checking…")).toBeInTheDocument();
  });

  it("the_rls_card_explains_itself_once_the_schema_is_read", async () => {
    setup(at("developer"));
    const rls = await screen.findByRole("region", { name: "Row-level security" });
    expect(await within(rls).findByText(/Tables the Data API reaches without row-level security are listed here/)).toBeInTheDocument();
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
