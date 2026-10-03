import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import { fakeFetch, jsonResponse, type FakeFetch, type FakeHandler } from "@/test/fake-fetch";
import { renderWithQuery } from "@/test/render";
import { SavedQueriesPanel, type ActiveQuery } from "./saved-queries-panel";

const Q = "/v1/databases/d1/sql-queries";
const summary = (id: string, name: string, version = 1) => ({ id, name, pg_database: null, role: null, updated_at: "2026-10-02T10:00:00Z", version });
const full = (id: string, name: string, sql: string, version = 1) => ({ ...summary(id, name, version), sql, created_at: "2026-10-01T00:00:00Z" });
const conflict = (current: unknown): FakeHandler => () => jsonResponse({ error: { code: "saved_query_conflict", message: "raw", current } }, 409);

interface Opts {
  routes?: Record<string, FakeHandler>;
  active?: ActiveQuery | null;
  sql?: string;
}
function setup(opts: Opts = {}) {
  const f: FakeFetch = fakeFetch({
    [`GET ${Q}`]: () => jsonResponse([summary("q1", "Orders"), summary("q2", "Users")]),
    ...opts.routes,
  });
  const onLoad = vi.fn();
  const onActiveChange = vi.fn();
  const result = renderWithQuery(
    <SavedQueriesPanel
      databaseId="d1"
      sql={opts.sql ?? "select 1"}
      hints={{ pg_database: null, role: null }}
      active={opts.active ?? null}
      onActiveChange={onActiveChange}
      onLoad={onLoad}
      history={[]}
      onOpenHistory={() => {}}
      onClearHistory={() => {}}
    />,
    { fetch: f },
  );
  return { ...result, f, user: userEvent.setup(), onLoad, onActiveChange };
}

describe("SavedQueriesPanel", () => {
  it("lists_and_searches_by_name", async () => {
    const { user } = setup();
    expect(await screen.findByText("Orders")).toBeInTheDocument();
    await user.type(screen.getByLabelText("Search saved queries"), "use");
    expect(screen.queryByText("Orders")).not.toBeInTheDocument();
    expect(screen.getByText("Users")).toBeInTheDocument();
  });

  it("open_loads_the_query", async () => {
    const { user, onLoad } = setup({ routes: { [`GET ${Q}/q1`]: () => jsonResponse(full("q1", "Orders", "select * from orders", 3)) } });
    await user.click(await screen.findByRole("button", { name: "Open Orders" }));
    await waitFor(() => expect(onLoad).toHaveBeenCalled());
    expect(onLoad.mock.calls[0][0]).toMatchObject({ id: "q1", sql: "select * from orders", version: 3 });
  });

  it("save_as_posts_and_shows_the_privacy_line", async () => {
    let body: any;
    const { user, onActiveChange } = setup({
      routes: { [`POST ${Q}`]: async (r) => ((body = await r.json()), jsonResponse(full("q9", "Mine", "select 1", 1), 201)) },
    });
    await user.click(await screen.findByRole("button", { name: "Save as" }));
    const dialog = await screen.findByRole("dialog");
    expect(within(dialog).getByText("Saved queries are private to you but stored on the server. Don't save secrets.")).toBeInTheDocument();
    await user.type(within(dialog).getByLabelText("Name"), "Mine");
    await user.click(within(dialog).getByRole("button", { name: "Save" }));
    await waitFor(() => expect(onActiveChange).toHaveBeenCalledWith({ id: "q9", name: "Mine", version: 1, sql: "select 1" }));
    expect(body).toMatchObject({ name: "Mine", sql: "select 1" });
  });

  it("save_patches_with_the_version", async () => {
    let body: any;
    const { user, onActiveChange } = setup({
      active: { id: "q1", name: "Orders", version: 3, sql: "select 0" },
      routes: { [`PATCH ${Q}/q1`]: async (r) => ((body = await r.json()), jsonResponse(full("q1", "Orders", "select 1", 4))) },
    });
    await user.click(await screen.findByRole("button", { name: "Save" }));
    await waitFor(() => expect(onActiveChange).toHaveBeenCalledWith({ id: "q1", name: "Orders", version: 4, sql: "select 1" }));
    expect(body).toMatchObject({ sql: "select 1", version: 3 });
  });

  it("saved_conflict_offers_overwrite_and_load", async () => {
    const bodies: any[] = [];
    let n = 0;
    const current = full("q1", "Orders", "select 'theirs'", 5);
    const { user, onLoad, onActiveChange } = setup({
      active: { id: "q1", name: "Orders", version: 3, sql: "select 0" },
      routes: {
        [`PATCH ${Q}/q1`]: async (r) => {
          bodies.push(await r.clone().json());
          return n++ < 2 ? conflict(current)(r) : jsonResponse(full("q1", "Orders", "select 1", 6));
        },
      },
    });
    await user.click(await screen.findByRole("button", { name: "Save" }));
    let dialog = await screen.findByRole("dialog");
    await user.click(within(dialog).getByRole("button", { name: "Load saved version" }));
    expect(onLoad).toHaveBeenCalledWith(expect.objectContaining({ id: "q1", sql: "select 'theirs'", version: 5 }));
    await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());
    // again, now overwrite
    await user.click(screen.getByRole("button", { name: "Save" }));
    dialog = await screen.findByRole("dialog");
    await user.click(within(dialog).getByRole("button", { name: "Overwrite" }));
    await waitFor(() => expect(onActiveChange).toHaveBeenCalledWith(expect.objectContaining({ version: 6 })));
    expect(bodies[0].version).toBe(3);
    expect(bodies.at(-1)).toMatchObject({ version: 5, sql: "select 1" });
  });

  it("delete_asks_first", async () => {
    let deleted = false;
    const { user } = setup({ routes: { [`DELETE ${Q}/q2`]: () => ((deleted = true), new Response(null, { status: 204 })) } });
    await user.click(await screen.findByRole("button", { name: "Delete Users" }));
    const dialog = await screen.findByRole("dialog");
    expect(deleted).toBe(false);
    await user.click(within(dialog).getByRole("button", { name: "Delete" }));
    await waitFor(() => expect(deleted).toBe(true));
  });

  it("rename_patches_the_name", async () => {
    let body: any;
    const { user } = setup({ routes: { [`PATCH ${Q}/q1`]: async (r) => ((body = await r.json()), jsonResponse(full("q1", "Sales", "x", 2))) } });
    await user.click(await screen.findByRole("button", { name: "Rename Orders" }));
    const dialog = await screen.findByRole("dialog");
    const input = within(dialog).getByLabelText("Name");
    await user.clear(input);
    await user.type(input, "Sales");
    await user.click(within(dialog).getByRole("button", { name: "Save" }));
    await waitFor(() => expect(body).toMatchObject({ name: "Sales", version: 1 }));
  });

  it("history_tab_lists_entries", async () => {
    const onOpenHistory = vi.fn();
    const f = fakeFetch({ [`GET ${Q}`]: () => jsonResponse([]) });
    const user = userEvent.setup();
    renderWithQuery(
      <SavedQueriesPanel
        databaseId="d1" sql="" hints={{ pg_database: null, role: null }} active={null} onActiveChange={() => {}} onLoad={() => {}}
        history={[{ sql: "select 42", branchId: "b1", database: null, role: "anon", at: "2026-10-03T00:00:00Z", outcome: "ok", durationMs: 5 }]}
        onOpenHistory={onOpenHistory} onClearHistory={() => {}}
      />,
      { fetch: f },
    );
    await user.click(screen.getByRole("tab", { name: "History" }));
    expect(screen.getByText("select 42")).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Open" }));
    expect(onOpenHistory).toHaveBeenCalledWith(expect.objectContaining({ sql: "select 42" }));
  });
});
