import { screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import { fakeFetch, jsonResponse, type FakeFetch, type FakeHandler } from "@/test/fake-fetch";
import { renderWithQuery } from "@/test/render";
import { TablesView } from "./tables-view";

const B = "/v1/databases/d1/branches";
const BR = `${B}/b1`;
const c = (over: Record<string, unknown>) => ({ type: "text", nullable: false, has_default: false, generated: false, array: false, enum: null, ...over });
const schema = {
  default_schema: "public",
  version: "v",
  schemas: {
    public: {
      enums: {},
      functions: { add_points: {} },
      tables: {
        items: {
          kind: "table",
          rls: true,
          security_invoker: false,
          primary_key: ["id"],
          columns: {
            id: c({ type: "integer", has_default: true }),
            name: c({}),
            qty: c({ type: "numeric", nullable: true }),
            active: c({ type: "boolean", has_default: true }),
            meta: c({ type: "jsonb", nullable: true }),
          },
        },
        counters: { kind: "table", rls: false, security_invoker: false, primary_key: ["id"], columns: { id: c({ type: "integer", has_default: true }), hits: c({ type: "integer", has_default: true }) } },
        log: { kind: "table", rls: false, security_invoker: false, primary_key: [], columns: { msg: c({}) } },
        v_items: { kind: "view", rls: false, security_invoker: false, columns: { id: c({ type: "integer" }) } },
      },
    },
  },
};
const row = (id: number) => ({ id, name: `n${id}`, qty: id === 2 ? null : id, active: true, meta: null });
const rows = (n: number, from = 1) => Array.from({ length: n }, (_, i) => row(from + i));

const role = (name: string, level = "plain") => ({
  name, level, login: false, origin: "sql", password: "none", pending: false, blocked_keys: [], created_at: "2026-10-01T00:00:00Z",
});
const dbView = (name: string, main: boolean, data_api = "on") => ({ name, main, data_api, data_api_error: null, default_schema: "public", origin: "owner", owner: "o" });

const DB_DETAIL = { database_id: "d1", name: "d", branch_count: 1, history_window: 604800, max_branches: 5, max_rows: 1000, statement_timeout_ms: 8000, max_connections: 5 };
interface Opts {
  maxRows?: number;
  schema?: FakeHandler;
  query?: FakeHandler;
  databases?: unknown[];
}
function setup(opts: Opts = {}) {
  const f: FakeFetch = fakeFetch({
    "GET /v1/databases/d1": () => jsonResponse({ ...DB_DETAIL, max_rows: opts.maxRows ?? 1000 }),
    [`GET ${B}`]: () => jsonResponse([{ id: "b1", name: "main", kind: "normal", status: "ready", parent_branch_id: null, created_at: "2026-10-01T00:00:00Z", protected: true }]),
    [`GET ${BR}/databases`]: () => jsonResponse(opts.databases ?? [dbView("app", true), dbView("analytics", false), dbView("cold", false, "off")]),
    [`GET ${BR}/roles`]: () => jsonResponse([role("app_owner", "owner_level"), role("reporter"), role("admin", "owner_level")].map((r, i) => (i === 0 ? { ...r, origin: "owner" } : r))),
    [`GET ${BR}/console/schema`]: opts.schema ?? (() => jsonResponse(schema)),
    [`POST ${BR}/console/query`]: opts.query ?? (() => jsonResponse(rows(2))),
  });
  const result = renderWithQuery(<TablesView databaseId="d1" />, { fetch: f });
  return { ...result, f, user: userEvent.setup() };
}
const queries = (f: FakeFetch) => f.calls.filter((r) => r.method === "POST" && r.url.endsWith("/console/query"));
const bodies = async (f: FakeFetch) => Promise.all(queries(f).map((r) => r.clone().json() as Promise<any>));
const last = async (f: FakeFetch) => (await bodies(f)).at(-1);
const lastWrite = async (f: FakeFetch) => (await bodies(f)).filter((b) => b.tree.op !== "select").at(-1);
async function open(user: ReturnType<typeof userEvent.setup>, name = "items") {
  await user.click(await screen.findByRole("button", { name: new RegExp(`^${name}\\b`) }));
}
const denied = (code: string, status = 403, message = "raw"): FakeHandler => () => jsonResponse({ error: { code, message } }, status);

describe("TablesView", () => {
  it("object_list_follows_view_as", async () => {
    const asOwner = { ...schema, schemas: { ...schema.schemas, private: { enums: {}, functions: {}, tables: { secrets: { ...schema.schemas.public.tables.log, columns: { msg: c({}) } } } } } };
    const asAnon = { ...schema, schemas: { public: { ...schema.schemas.public, tables: { items: schema.schemas.public.tables.items } } } };
    const { f, user } = setup({ schema: (r) => jsonResponse(new URL(r.url).searchParams.get("role") === "anon" ? asAnon : asOwner) });
    await screen.findByRole("button", { name: /^items\b/ });
    expect(screen.getByRole("button", { name: /^log\b/ })).toBeInTheDocument();
    await user.selectOptions(screen.getByLabelText("Schema"), "private");
    expect(await screen.findByRole("button", { name: /^secrets\b/ })).toBeInTheDocument();
    await user.selectOptions(screen.getByLabelText("View as"), "anon");
    await vi.waitFor(() => expect(screen.queryByRole("option", { name: "private" })).not.toBeInTheDocument());
    expect(screen.queryByRole("button", { name: /^log\b/ })).not.toBeInTheDocument();
    expect(await screen.findByRole("button", { name: /^items\b/ })).toBeInTheDocument();
    const urls = f.calls.filter((r) => r.url.includes("console/schema")).map((r) => new URL(r.url).searchParams.get("role"));
    expect(urls[0]).toBeNull();
    expect(urls.at(-1)).toBe("anon");
  });

  it.each([
    ["not_found", 404, /no longer exists/],
    ["data_api_disabled", 409, /isn't enabled for this database/],
    ["owner_password_unknown", 409, /owner password isn't known/],
    ["role_refused", 403, /bypass row-level security/],
  ])("schema error %s shows plain text", async (code, status, text) => {
    setup({ schema: denied(code, status, "raw") });
    expect(await screen.findByRole("alert")).toHaveTextContent(text);
    expect(screen.queryByText("raw")).not.toBeInTheDocument();
  });

  it("view_as_offers_anon_and_authenticated_after_owner", async () => {
    setup();
    const sel = await screen.findByLabelText("View as");
    await screen.findByRole("option", { name: "reporter" });
    const names = Array.from(sel.querySelectorAll("option")).map((o) => o.textContent);
    expect(names.slice(0, 3)).toEqual(["Owner", "anon", "authenticated"]);
    expect(names).toContain("reporter");
  });
  it("view_as_defaults_to_owner_with_banner", async () => {
    const { f, user } = setup();
    await open(user);
    expect(await screen.findByLabelText("View as")).toHaveValue("");
    expect(screen.getByText(/Viewing as owner: row-level security is bypassed, and edits apply to every row\./)).toBeInTheDocument();
    expect(await screen.findByText("n1")).toBeInTheDocument();
    expect(await last(f)).toMatchObject({ role: null, database: null });
    // owner-level roles are listed but disabled, with the reason
    const admin = screen.getByRole("option", { name: /admin.*can bypass row-level security/ });
    expect(admin).toBeDisabled();
    expect(screen.queryByRole("option", { name: /app_owner/ })).not.toBeInTheDocument();
    await user.selectOptions(screen.getByLabelText("View as"), "anon");
    expect(screen.queryByText(/Viewing as owner/)).not.toBeInTheDocument();
    await vi.waitFor(async () => expect(await last(f)).toMatchObject({ role: "anon" }));
    // only served databases are offered, a non-main one is named
    expect(screen.queryByRole("option", { name: "cold" })).not.toBeInTheDocument();
    await user.selectOptions(screen.getByLabelText("Database"), "analytics");
    await open(user);
    await vi.waitFor(async () => expect(await last(f)).toMatchObject({ database: "analytics" }));
    expect(f.calls.some((r) => r.url.includes("console/schema") && r.url.includes("database=analytics"))).toBe(true);
  });

  it("paging_and_sort_build_the_right_tree", async () => {
    const { f, user } = setup({ query: () => jsonResponse(rows(50)) });
    await open(user);
    await screen.findByText("n1");
    expect((await last(f)).tree).toEqual({ v: 1, op: "select", from: "public.items", orderBy: [{ col: "id", dir: "asc" }], limit: 50, offset: 0 });
    await user.click(screen.getByRole("button", { name: "Sort by name" }));
    await vi.waitFor(async () => expect((await last(f)).tree.orderBy).toEqual([{ col: "name", dir: "asc" }, { col: "id", dir: "asc" }]));
    await user.click(screen.getByRole("button", { name: "Sort by name" }));
    await vi.waitFor(async () => expect((await last(f)).tree.orderBy).toEqual([{ col: "name", dir: "desc" }, { col: "id", dir: "asc" }]));
    await user.click(screen.getByRole("button", { name: "Next page" }));
    await vi.waitFor(async () => expect((await last(f)).tree).toMatchObject({ offset: 50, limit: 50, orderBy: [{ col: "name", dir: "desc" }, { col: "id", dir: "asc" }] }));
    expect(screen.getByText("Page 2")).toBeInTheDocument();
  });

  it("filters_build_and_or_trees", async () => {
    const { f, user } = setup();
    await open(user);
    await screen.findByText("n1");
    await user.click(screen.getByRole("button", { name: "Add filter" }));
    await user.selectOptions(screen.getByLabelText("Filter column 1"), "qty");
    await user.selectOptions(screen.getByLabelText("Filter operator 1"), "gte");
    await user.type(screen.getByLabelText("Filter value 1"), "3");
    await user.click(screen.getByRole("button", { name: "Apply filters" }));
    await vi.waitFor(async () => expect((await last(f)).tree.where).toEqual({ col: "qty", gte: 3 }));
    await user.click(screen.getByRole("button", { name: "Add filter" }));
    await user.selectOptions(screen.getByLabelText("Filter column 2"), "name");
    await user.selectOptions(screen.getByLabelText("Filter operator 2"), "like");
    await user.type(screen.getByLabelText("Filter value 2"), "a%");
    await user.selectOptions(screen.getByLabelText("Match"), "or");
    await user.click(screen.getByRole("button", { name: "Apply filters" }));
    await vi.waitFor(async () =>
      expect((await last(f)).tree.where).toEqual({ or: [{ col: "qty", gte: 3 }, { col: "name", like: "a%" }] }),
    );
    // a bad value is reported and nothing is sent
    const sent = queries(f).length;
    await user.clear(screen.getByLabelText("Filter value 1"));
    await user.type(screen.getByLabelText("Filter value 1"), "abc");
    await user.click(screen.getByRole("button", { name: "Apply filters" }));
    expect(await screen.findByRole("alert")).toHaveTextContent(/qty/);
    expect(queries(f).length).toBe(sent);
  });

  it("edit_cell_sends_update_by_primary_key", async () => {
    const { f, user } = setup();
    await open(user);
    await screen.findByText("n1");
    await user.click(screen.getByRole("button", { name: "Edit qty, id 1" }));
    const input = screen.getByLabelText("qty value");
    await user.clear(input);
    await user.type(input, "7");
    await user.click(screen.getByRole("button", { name: "Save" }));
    await vi.waitFor(async () =>
      expect(await lastWrite(f)).toMatchObject({
        tree: { v: 1, op: "update", table: "public.items", set: { qty: 7 }, where: { col: "id", eq: 1 }, returning: ["id"] },
      }),
    );
    // a NULL toggle on nullable columns
    await vi.waitFor(() => expect(screen.queryByLabelText("qty value")).not.toBeInTheDocument());
    await user.click(screen.getByRole("button", { name: "Edit qty, id 1" }));
    await user.click(screen.getByLabelText("qty NULL"));
    await user.click(screen.getByRole("button", { name: "Save" }));
    await vi.waitFor(async () => expect((await lastWrite(f)).tree.set).toEqual({ qty: null }));
    // null shows distinctly
    expect(screen.getAllByText("null").length).toBeGreaterThan(0);
  });

  it("insert_marks_defaults", async () => {
    const { f, user } = setup();
    await open(user);
    await screen.findByText("n1");
    await user.click(screen.getByRole("button", { name: "Insert row" }));
    expect(screen.getByLabelText("id use default")).toBeChecked();
    expect(screen.getByLabelText("active use default")).toBeChecked();
    expect(screen.getByLabelText("qty NULL")).toBeChecked();
    expect(screen.queryByLabelText("name use default")).not.toBeInTheDocument();
    await user.type(screen.getByLabelText("name value"), "widget");
    await user.click(screen.getByRole("button", { name: "Insert" }));
    await vi.waitFor(async () =>
      expect((await lastWrite(f)).tree).toEqual({
        v: 1, op: "insert", into: "public.items", values: [{ name: "widget", qty: null, meta: null }], returning: ["id"],
      }),
    );
  });

  it("a write that changed nothing says so", async () => {
    const { user } = setup({ query: async (r) => ((await r.clone().json()).tree.op === "select" ? jsonResponse(rows(2)) : jsonResponse([])) });
    await open(user);
    await screen.findByText("n1");
    await user.click(screen.getByRole("button", { name: "Edit qty, id 1" }));
    await user.click(screen.getByRole("button", { name: "Save" }));
    expect(await screen.findByRole("alert")).toHaveTextContent("No row was changed");
    await user.click(screen.getByRole("button", { name: "Insert row" }));
    await user.type(screen.getByLabelText("name value"), "w");
    await user.click(screen.getByRole("button", { name: "Insert" }));
    await vi.waitFor(() => expect(screen.getAllByRole("alert").some((a) => /No row was changed/.test(a.textContent ?? ""))).toBe(true));
  });

  it("pages by the smaller of 50 and the database's max_rows", async () => {
    const { f, user } = setup({ maxRows: 20, query: () => jsonResponse(rows(20)) });
    await open(user);
    await screen.findByText("n1");
    await vi.waitFor(async () => expect((await last(f)).tree).toMatchObject({ limit: 20, offset: 0 }));
    await user.click(screen.getByRole("button", { name: "Next page" }));
    await vi.waitFor(async () => expect((await last(f)).tree).toMatchObject({ limit: 20, offset: 20 }));
  });

  it("insert with every column left to default is disabled with the reason", async () => {
    const { user } = setup();
    await open(user, "counters");
    await user.click(await screen.findByRole("button", { name: "Insert row" }));
    const insert = screen.getByRole("button", { name: "Insert" });
    expect(insert).toBeDisabled();
    expect(insert).toHaveAccessibleDescription(/at least one column/i);
    await user.click(screen.getByLabelText("hits use default"));
    expect(insert).toBeEnabled();
  });

  it("filter offers like only on text columns", async () => {
    const { user } = setup();
    await open(user);
    await screen.findByText("n1");
    await user.click(screen.getByRole("button", { name: "Add filter" }));
    await user.selectOptions(screen.getByLabelText("Filter column 1"), "name");
    expect(within(screen.getByLabelText("Filter operator 1")).getByRole("option", { name: "like" })).toBeInTheDocument();
    await user.selectOptions(screen.getByLabelText("Filter operator 1"), "ilike");
    await user.selectOptions(screen.getByLabelText("Filter column 1"), "qty");
    expect(within(screen.getByLabelText("Filter operator 1")).queryByRole("option", { name: "like" })).not.toBeInTheDocument();
    expect(screen.getByLabelText("Filter operator 1")).toHaveValue("eq");
  });

  it("a json field with a lossy number is refused and nothing is sent", async () => {
    const { f, user } = setup();
    await open(user);
    await screen.findByText("n1");
    await user.click(screen.getByRole("button", { name: "Edit meta, id 1" }));
    await user.click(screen.getByLabelText("meta NULL"));
    await user.click(screen.getByLabelText("meta value"));
    await user.paste("[1e400]");
    await user.click(screen.getByRole("button", { name: "Save" }));
    expect(await screen.findByRole("alert")).toHaveTextContent(/meta.*number/);
    expect(await lastWrite(f)).toBeUndefined();
  });

  it("delete_confirms_count", async () => {
    const { f, user } = setup();
    await open(user);
    await screen.findByText("n1");
    await user.click(screen.getByLabelText("Select row id 1"));
    await user.click(screen.getByLabelText("Select row id 2"));
    await user.click(screen.getByRole("button", { name: "Delete selected (2)" }));
    expect(screen.getByText(/Delete 2 rows\?/)).toBeInTheDocument();
    expect((await bodies(f)).some((b) => b.tree.op !== "select")).toBe(false);
    await user.click(screen.getByRole("button", { name: "Confirm delete" }));
    await vi.waitFor(async () =>
      expect((await lastWrite(f)).tree).toEqual({
        v: 1, op: "batch",
        ops: [
          { op: "delete", from: "public.items", where: { col: "id", eq: 1 } },
          { op: "delete", from: "public.items", where: { col: "id", eq: 2 } },
        ],
      }),
    );
  });

  it("no_primary_key_is_read_only", async () => {
    const { user } = setup({ query: () => jsonResponse([{ msg: "hi" }]) });
    await open(user, "log");
    await screen.findByText("hi");
    expect(screen.getByText(/no primary key/i)).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Insert row" })).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: /^Edit/ })).not.toBeInTheDocument();
    expect(screen.queryByLabelText(/Select row/)).not.toBeInTheDocument();
    await open(user, "v_items");
    expect(await screen.findByText(/views are read-only/i)).toBeInTheDocument();
  });

  it("owner_password_unknown_shows_reset_link", async () => {
    const { user } = setup({ query: denied("owner_password_unknown", 409) });
    await open(user);
    expect(await screen.findByText(/owner password isn't known/i)).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Reset the owner password" }));
    expect(await screen.findByRole("heading", { name: "Roles" })).toBeInTheDocument();
  });

  it("error_codes_have_plain_text", async () => {
    const calls: string[] = [];
    const { user } = setup({
      query: async (r) => {
        const b = await r.clone().json();
        calls.push(b.tree.op);
        return b.tree.op === "select" ? jsonResponse(rows(2)) : jsonResponse({ error: { code: "permission_denied", message: "raw" } }, 403);
      },
    });
    await open(user);
    await screen.findByText("n1");
    await user.selectOptions(screen.getByLabelText("View as"), "anon");
    await screen.findByText("n1");
    await user.click(screen.getByRole("button", { name: "Edit qty, id 1" }));
    await user.click(screen.getByRole("button", { name: "Save" }));
    expect(await screen.findByRole("alert")).toHaveTextContent("anon can't update this table.");
    expect(within(screen.getByRole("alert")).queryByText("raw")).not.toBeInTheDocument();
  });

  it("reads rows losslessly and sends the exact digits back", async () => {
    const big = '[{"id":9007199254740993,"name":"a","qty":0.12345678901234567890,"active":true,"meta":null}]';
    const { f, user } = setup({ query: (r) => r.clone().json().then((b: any) => b.tree.op === "select" ? new Response(big, { headers: { "Content-Type": "application/json" } }) : jsonResponse([])) as any });
    await open(user);
    expect(await screen.findByText("9007199254740993")).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Edit qty, id 9007199254740993" }));
    expect(screen.getByLabelText("qty value")).toHaveValue("0.12345678901234567890");
    await user.click(screen.getByRole("button", { name: "Save" }));
    await vi.waitFor(async () => expect(await lastWrite(f)).toBeTruthy());
    const w = await lastWrite(f);
    expect(w.tree.where).toEqual({ col: "id", eq: "9007199254740993" });
    expect(w.tree.set).toEqual({ qty: "0.12345678901234567890" });
  });

  it("refuses non-finite numbers in a field and sends nothing", async () => {
    const { f, user } = setup();
    await open(user);
    await screen.findByText("n1");
    await user.click(screen.getByRole("button", { name: "Edit qty, id 1" }));
    const input = screen.getByLabelText("qty value");
    await user.clear(input);
    await user.type(input, "Infinity");
    await user.click(screen.getByRole("button", { name: "Save" }));
    expect(await screen.findByRole("alert")).toHaveTextContent(/qty/);
    expect(await lastWrite(f)).toBeUndefined();
    await user.click(screen.getByRole("button", { name: "Cancel" }));
    await user.click(screen.getByRole("button", { name: "Edit qty, id 1" }));
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
  });

  it("a jsonb string cell round-trips on edit", async () => {
    const { f, user } = setup({ query: () => jsonResponse([{ id: 1, name: "a", qty: 1, active: true, meta: "hello" }]) });
    await open(user);
    await screen.findByText("hello");
    await user.click(screen.getByRole("button", { name: "Edit meta, id 1" }));
    expect(screen.getByLabelText("meta value")).toHaveValue('"hello"');
    await user.click(screen.getByRole("button", { name: "Save" }));
    await vi.waitFor(async () => expect((await lastWrite(f)).tree.set).toEqual({ meta: "hello" }));
  });

  it("orders by the primary key as a tiebreaker", async () => {
    const { f, user } = setup();
    await open(user);
    await screen.findByText("n1");
    expect((await last(f)).tree.orderBy).toEqual([{ col: "id", dir: "asc" }]);
  });

  it("deleting a whole later page goes back one page", async () => {
    let full = true;
    const { user } = setup({
      query: async (r) => {
        const b = await r.clone().json();
        if (b.tree.op !== "select") { full = false; return jsonResponse([]); }
        if (b.tree.offset === 0) return jsonResponse(rows(50));
        return jsonResponse(full ? [row(51)] : []);
      },
    });
    await open(user);
    await screen.findByText("n1");
    await user.click(screen.getByRole("button", { name: "Next page" }));
    await screen.findByText("n51");
    await user.click(screen.getByLabelText("Select row id 51"));
    await user.click(screen.getByRole("button", { name: "Delete selected (1)" }));
    await user.click(screen.getByRole("button", { name: "Confirm delete" }));
    expect(await screen.findByText("Page 1")).toBeInTheDocument();
    expect(await screen.findByText("n1")).toBeInTheDocument();
  });
});
