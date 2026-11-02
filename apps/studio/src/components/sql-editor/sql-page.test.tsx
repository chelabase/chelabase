import { act, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { EditorView } from "@codemirror/view";
import { afterEach, describe, expect, it, vi } from "vitest";
import { getGetSqlSchemaQueryKey, getGetTableQueryKey, getListObjectsQueryKey } from "@chelabase/management-client";
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
  const createClient = vi.fn(() => Promise.reject(new Error("no worker")));
  const ui = (p: EditorParams) => <SqlPage params={p} navigate={navigate} openTables={openTables} createClient={createClient} />;
  const result = renderWithQuery(ui(params), { fetch: f });
  return { ...result, f, navigate, openTables, createClient, user: userEvent.setup(), goTo: (p: EditorParams) => result.rerender(ui(p)) };
}
const schemaCalls = (f: ReturnType<typeof fakeFetch>) => f.calls.filter((r) => r.url.includes("/sql/schema"));
const ndjson = (...events: unknown[]) => new Response(events.map((e) => JSON.stringify(e)).join("\n") + "\n", { status: 200, headers: { "Content-Type": "application/x-ndjson" } });
const ranTag = (tag: string) => () => ndjson({ t: "complete", statement: 0, tag, rows: 0, truncated: false }, { t: "done", outcome: "ok", duration_ms: 3 });
const runsTo = (f: ReturnType<typeof fakeFetch>, branch: string) => f.calls.filter((r) => r.method === "POST" && new URL(r.url).pathname === `${D}/branches/${branch}/sql`);
async function setDoc(text: string) {
  const v = await waitFor(() => {
    const found = document.querySelector(".cm-editor");
    expect(found).not.toBeNull();
    return EditorView.findFromDOM(found as HTMLElement)!;
  });
  act(() => v.dispatch({ changes: { from: 0, to: v.state.doc.length, insert: text } }));
}
const enabledRun = () => waitFor(() => {
  const run = screen.getByRole("button", { name: "Run" });
  expect(run).toBeEnabled();
  return run;
});

afterEach(() => {
  localStorage.clear();
  sessionStorage.clear();
});

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

  // #84 AC2: the editor stays mounted across a branch switch (the run, results, undo and the pgls worker survive);
  // the next run goes to the new branch.
  it("switching_branch_keeps_the_editor_mounted_and_the_next_run_goes_to_the_new_branch", async () => {
    let answerB2!: () => void;
    const b2Databases = new Promise<void>((r) => (answerB2 = r));
    const B2 = `${D}/branches/b2`;
    const { f, navigate, createClient, user, goTo } = setup("admin", PLACE, {
      [`GET ${B2}/databases`]: async () => {
        await b2Databases;
        return jsonResponse(PG_DATABASES);
      },
      [`GET ${B2}/roles`]: () => jsonResponse(ROLES),
      [`GET ${B2}/sql/schema`]: () => jsonResponse({ schemas: [] }),
      [`POST ${B2}/sql`]: ranTag("SELECT 1"),
      [`POST ${B}/sql`]: ranTag("SELECT 1"),
    });
    await enabledRun();
    await setDoc("select 1");
    expect(createClient).toHaveBeenCalledTimes(1);

    // The context bar names the branch and clears the levels below it.
    goTo({ projectId: "p1", databaseId: "d1", branchId: "b2" });
    expect(screen.getByRole("button", { name: "Run" })).toBeInTheDocument();
    expect(screen.queryByText("Loading…")).toBeNull();
    answerB2();
    await waitFor(() => expect(navigate).toHaveBeenCalledWith({ projectId: "p1", databaseId: "d1", branchId: "b2", pgDatabase: "app", schema: undefined, table: undefined }, { replace: true }));
    expect(screen.getByRole("button", { name: "Run" })).toBeInTheDocument();
    goTo({ ...PLACE, branchId: "b2" });

    await user.click(await enabledRun());
    await waitFor(() => expect(runsTo(f, "b2")).toHaveLength(1));
    expect(runsTo(f, "b1")).toHaveLength(0);
    expect(await runsTo(f, "b2")[0].clone().json()).toMatchObject({ sql: "select 1" });
    expect(createClient).toHaveBeenCalledTimes(1);
  });

  // A DDL run in the full SQL tab leaves no table editor view stale (realtime sends no DDL).
  it("a_ddl_run_invalidates_the_table_editor_and_completion_queries_of_the_branch", async () => {
    const { queryClient, user } = setup("admin", PLACE, { [`POST ${B}/sql`]: ranTag("ALTER TABLE") });
    const keys = [
      getGetTableQueryKey("d1", "b1", "public", "invoices"),
      getGetTableQueryKey("d1", "b1", "billing", "ledger"),
      getListObjectsQueryKey("d1", "b1"),
      getGetSqlSchemaQueryKey("d1", "b1", { role: "reporter" }),
    ];
    for (const k of keys) queryClient.setQueryData(k, {});
    await setDoc("alter table invoices add column notes text");
    await user.click(await enabledRun());
    await waitFor(() => for_(keys, (k) => expect(queryClient.getQueryState(k)?.isInvalidated).toBe(true)));
  });

  it("a_dml_run_invalidates_no_structure", async () => {
    const { queryClient, f, user } = setup("admin", PLACE, { [`POST ${B}/sql`]: ranTag("INSERT 0 1") });
    const key = getGetTableQueryKey("d1", "b1", "public", "invoices");
    queryClient.setQueryData(key, {});
    await setDoc("insert into invoices default values");
    await user.click(await enabledRun());
    await waitFor(() => expect(runsTo(f, "b1")).toHaveLength(1));
    expect(await screen.findByText(/INSERT 0 1/)).toBeInTheDocument();
    expect(queryClient.getQueryState(key)?.isInvalidated).toBe(false);
  });

  // #84 AC3: Act as chosen on the table editor (session storage) is the run's role here.
  it("the_act_as_role_from_session_storage_is_the_runs_role", async () => {
    sessionStorage.setItem("chelabase.studio.tables.actAs", JSON.stringify({ "d1/b1": "reporter" }));
    const { f, user } = setup("admin", PLACE, { [`POST ${B}/sql`]: ranTag("SELECT 1") });
    await setDoc("select 1");
    await user.click(await enabledRun());
    await waitFor(() => expect(runsTo(f, "b1")).toHaveLength(1));
    expect(await runsTo(f, "b1")[0].clone().json()).toMatchObject({ role: "reporter" });
  });
});

function for_<T>(items: T[], check: (t: T) => void) {
  for (const t of items) check(t);
}
