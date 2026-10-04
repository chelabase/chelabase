import { act, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { EditorView } from "@codemirror/view";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { PglsClient } from "@/lib/pgls/client";
import { fakeFetch, jsonResponse, type FakeFetch, type FakeHandler } from "@/test/fake-fetch";
import { renderWithQuery } from "@/test/render";
import { SqlEditor } from "./sql-editor";

const B = "/v1/databases/d1/branches";
const BR = `${B}/b1`;
const dbView = (name: string, main: boolean, data_api = "on") => ({ name, main, data_api, data_api_error: null, default_schema: "public", origin: main ? "main" : "sql", owner: "o" });
const role = (name: string, origin = "sql") => ({ name, level: "plain", login: false, origin, password: "none", pending: false, blocked_keys: [], created_at: "2026-10-01T00:00:00Z" });
const branch = (over: Record<string, unknown> = {}) => ({ id: "b1", name: "main", kind: "normal", status: "ready", parent_branch_id: null, created_at: "2026-10-01T00:00:00Z", protected: false, ...over });

const ndjson = (...events: unknown[]) => new Response(events.map((e) => JSON.stringify(e)).join("\n") + "\n", { status: 200, headers: { "Content-Type": "application/x-ndjson" } });
const selectRun = (): Response =>
  ndjson(
    { t: "columns", statement: 0, columns: [{ name: "id", type: "int4" }, { name: "name", type: "text" }] },
    { t: "rows", statement: 0, rows: [["1", "ann"], ["2", null]] },
    { t: "complete", statement: 0, tag: "SELECT 2", rows: 2, truncated: false },
    { t: "done", outcome: "ok", duration_ms: 12 },
  );
const failure = (code: string, status: number, extra: Record<string, unknown> = {}): Response =>
  jsonResponse({ error: { code, message: "raw", ...extra } }, status);

interface Opts {
  run?: FakeHandler;
  branches?: unknown[];
  databases?: unknown[];
  schema?: FakeHandler;
  createClient?: () => Promise<PglsClient>;
  routes?: Record<string, FakeHandler>;
  /** Answers the cancel route (default: 202 at once). */
  cancel?: () => Promise<Response>;
}
function setup(opts: Opts = {}) {
  const f: FakeFetch = fakeFetch({
    "GET /v1/me": () => jsonResponse({ tenant: null, user: { org_id: "o", sub: "u1" } }),
    [`GET ${B}`]: () => jsonResponse(opts.branches ?? [branch()]),
    [`GET ${BR}/databases`]: () => jsonResponse(opts.databases ?? [dbView("app", true), dbView("analytics", false)]),
    [`GET ${BR}/roles`]: () => jsonResponse([role("app_owner", "owner"), role("reporter")]),
    [`GET ${BR}/sql/schema`]: opts.schema ?? (() => jsonResponse({ schemas: [] })),
    [`POST ${BR}/sql`]: opts.run ?? (() => selectRun()),
    "GET /v1/databases/d1/sql-queries": () => jsonResponse([]),
    ...opts.routes,
  });
  // The cancel route has a run id in its path: answer it here, recording the call.
  const wrapped: FakeFetch = Object.assign(
    async (input: RequestInfo | URL, init?: RequestInit) => {
      const req = new Request(input, init);
      if (/\/sql\/runs\/[^/]+\/cancel$/.test(new URL(req.url).pathname)) {
        f.calls.push(req);
        return opts.cancel ? opts.cancel() : jsonResponse({}, 202);
      }
      return f(input, init);
    },
    { calls: f.calls },
  );
  const result = renderWithQuery(<SqlEditor databaseId="d1" createClient={opts.createClient ?? (() => Promise.reject(new Error("no worker")))} />, { fetch: wrapped });
  return { ...result, f, user: userEvent.setup() };
}
const view = () => EditorView.findFromDOM(document.querySelector(".cm-editor") as HTMLElement)!;
async function setDoc(text: string) {
  const v = await waitFor(() => {
    const found = document.querySelector(".cm-editor");
    expect(found).not.toBeNull();
    return EditorView.findFromDOM(found as HTMLElement)!;
  });
  act(() => v.dispatch({ changes: { from: 0, to: v.state.doc.length, insert: text } }));
}
const runs = (f: FakeFetch) => f.calls.filter((r) => r.method === "POST" && r.url.endsWith("/b1/sql"));
const runBodies = (f: FakeFetch) => Promise.all(runs(f).map((r) => r.clone().json() as Promise<any>));
const schemaCalls = (f: FakeFetch) => f.calls.filter((r) => r.url.includes("/sql/schema"));
const textOf = (el: Element) => el.textContent ?? "";
const historyOf = () => JSON.parse(localStorage.getItem("chelabase.sql.history.u1.d1") ?? "[]") as Array<Record<string, unknown>>;
/** A run response whose events the test pushes. */
function openRun() {
  let ctl!: ReadableStreamDefaultController<Uint8Array>;
  const body = new ReadableStream<Uint8Array>({ start: (c) => void (ctl = c) });
  const push = (...events: unknown[]) => ctl.enqueue(new TextEncoder().encode(events.map((e) => JSON.stringify(e) + "\n").join("")));
  return { response: () => new Response(body, { status: 200, headers: { "Content-Type": "application/x-ndjson" } }), push, close: () => ctl.close() };
}

afterEach(() => localStorage.clear());

describe("SqlEditor", () => {
  it("run_streams_results_into_tabs", async () => {
    const { f, user } = setup({
      run: () =>
        ndjson(
          { t: "notice", severity: "NOTICE", code: "00000", message: "hello notice", detail: null, hint: null, position: null },
          { t: "columns", statement: 0, columns: [{ name: "id", type: "int4" }, { name: "name", type: "text" }] },
          { t: "rows", statement: 0, rows: [["1", "ann"], ["2", null]] },
          { t: "complete", statement: 0, tag: "SELECT 2", rows: 2, truncated: false },
          { t: "columns", statement: 1, columns: [{ name: "n", type: "int4" }] },
          { t: "rows", statement: 1, rows: [["7"]] },
          { t: "complete", statement: 1, tag: "SELECT 1", rows: 1, truncated: true },
          { t: "done", outcome: "ok", duration_ms: 12 },
        ),
    });
    await setDoc("select 1; select 2");
    await user.click(await screen.findByRole("button", { name: "Run" }));
    const first = await screen.findByRole("tab", { name: "SELECT 2" });
    expect(screen.getByRole("tab", { name: "SELECT 1" })).toBeInTheDocument();
    expect(screen.getByRole("tab", { name: "Messages" })).toBeInTheDocument();
    expect(screen.getByText("ann")).toBeInTheDocument();
    expect(screen.getByText("NULL")).toHaveClass("text-muted-foreground");
    expect(screen.queryByText("Showing the first 1,000 rows")).not.toBeInTheDocument();
    await user.click(screen.getByRole("tab", { name: "SELECT 1" }));
    expect(screen.getByText("Showing the first 1,000 rows")).toBeInTheDocument();
    await user.click(screen.getByRole("tab", { name: "Messages" }));
    expect(screen.getByText(/hello notice/)).toBeInTheDocument();
    expect(await screen.findByText(/ok in 12 ms/)).toBeInTheDocument();
    expect(first).toBeInTheDocument();
    const [body] = await runBodies(f);
    expect(body).toMatchObject({ sql: "select 1; select 2", database: null, role: null, timeout_ms: 60000, confirm: false });
    expect(body.run_id).toMatch(/^[0-9a-f-]{36}$/);
    expect(runs(f)[0].headers.get("Authorization")).toBe("Bearer test-token");
    // the run lands in the history
    const hist = JSON.parse(localStorage.getItem("chelabase.sql.history.u1.d1")!);
    expect(hist[0]).toMatchObject({ sql: "select 1; select 2", branchId: "b1", outcome: "ok", durationMs: 12 });
  });

  it("confirm_dialog_resends_with_confirm", async () => {
    let n = 0;
    const { f, user } = setup({
      run: () =>
        n++ === 0
          ? failure("sql_confirm_required", 409, { statements: [{ index: 0, kind: "DELETE", text_preview: "delete from t", reason: "risky" }] })
          : selectRun(),
    });
    await setDoc("delete from t");
    await user.click(await screen.findByRole("button", { name: "Run" }));
    const dialog = await screen.findByRole("dialog");
    expect(within(dialog).getByText("delete from t")).toBeInTheDocument();
    expect(within(dialog).getByText(/DELETE/)).toBeInTheDocument();
    expect(within(dialog).getByText(/risky/i)).toBeInTheDocument();
    await user.click(within(dialog).getByRole("button", { name: "Confirm" }));
    await screen.findByRole("tab", { name: "SELECT 2" });
    const bodies = await runBodies(f);
    expect(bodies).toHaveLength(2);
    expect(bodies[0].confirm).toBe(false);
    expect(bodies[1]).toMatchObject({ confirm: true, sql: "delete from t" });
    expect(bodies[1].run_id).not.toBe(bodies[0].run_id);
  });

  it("cancel_calls_the_route_and_aborts", async () => {
    let signal: AbortSignal | undefined;
    const { f, user } = setup({
      run: (r) => {
        signal = r.signal;
        return new Response(new ReadableStream({ start() {} }), { status: 200 });
      },
    });
    await setDoc("select pg_sleep(60)");
    await user.click(await screen.findByRole("button", { name: "Run" }));
    await user.click(await screen.findByRole("button", { name: "Cancel" }));
    const cancels = () => f.calls.filter((r) => r.url.endsWith("/cancel"));
    await waitFor(() => expect(cancels()).toHaveLength(1));
    const [body] = await runBodies(f);
    expect(cancels()[0].method).toBe("POST");
    expect(new URL(cancels()[0].url).pathname).toBe(`${BR}/sql/runs/${body.run_id}/cancel`);
    expect(signal?.aborted).toBe(true);
    expect(await screen.findByRole("button", { name: "Run" })).toBeEnabled();
  });

  it("cancel_after_the_run_finished_records_once_and_keeps_its_outcome", async () => {
    const stream = openRun();
    const { user } = setup({
      run: () => stream.response(),
      // The run ends (done arrives) before the cancel's 202 does.
      cancel: async () => {
        stream.push({ t: "done", outcome: "ok", duration_ms: 5 });
        stream.close();
        await new Promise((r) => setTimeout(r, 50));
        return jsonResponse({}, 202);
      },
    });
    await setDoc("select 1");
    await user.click(await screen.findByRole("button", { name: "Run" }));
    await user.click(await screen.findByRole("button", { name: "Cancel" }));
    expect(await screen.findByText(/ok in 5 ms/)).toBeInTheDocument();
    await new Promise((r) => setTimeout(r, 100));
    expect(screen.queryByText(/cancelled/)).not.toBeInTheDocument();
    expect(historyOf()).toHaveLength(1);
    expect(historyOf()[0]).toMatchObject({ outcome: "ok", durationMs: 5 });
  });

  it("server_cancel_before_the_202_records_once", async () => {
    const stream = openRun();
    const { user } = setup({
      run: () => stream.response(),
      cancel: async () => {
        stream.push({ t: "done", outcome: "cancelled", duration_ms: 978 });
        stream.close();
        await new Promise((r) => setTimeout(r, 50));
        return jsonResponse({}, 202);
      },
    });
    await setDoc("select pg_sleep(30)");
    await user.click(await screen.findByRole("button", { name: "Run" }));
    await user.click(await screen.findByRole("button", { name: "Cancel" }));
    expect(await screen.findByText(/cancelled in 978 ms/)).toBeInTheDocument();
    await new Promise((r) => setTimeout(r, 100));
    expect(historyOf()).toHaveLength(1);
    expect(historyOf()[0]).toMatchObject({ outcome: "cancelled", durationMs: 978 });
  });

  it("history_keeps_the_role_the_run_used", async () => {
    const stream = openRun();
    const { user } = setup({ run: () => stream.response() });
    await screen.findByRole("option", { name: "reporter" });
    await user.selectOptions(screen.getByLabelText("Run as"), "reporter");
    await setDoc("select 1");
    await user.click(await screen.findByRole("button", { name: "Run" }));
    await screen.findByRole("button", { name: "Cancel" });
    await user.selectOptions(screen.getByLabelText("Run as"), "Owner");
    stream.push({ t: "done", outcome: "ok", duration_ms: 3 });
    stream.close();
    await screen.findByText(/ok in 3 ms/);
    expect(historyOf()[0]).toMatchObject({ role: "reporter", outcome: "ok" });
  });

  it("unsaved_edits_are_kept_on_unmount", async () => {
    const { unmount } = setup();
    await setDoc("select 42");
    unmount();
    expect(localStorage.getItem("chelabase.sql.buffer.u1.d1")).toBe("select 42");
  });

  it("error_position_is_underlined", async () => {
    const { user } = setup({
      run: () =>
        ndjson(
          { t: "error", statement: 0, code: "42601", message: "syntax error at or near \"selec\"", detail: null, hint: null, position: 3 },
          { t: "done", outcome: "error", duration_ms: 3 },
        ),
    });
    await setDoc("-- x\nselec 1");
    // run only the selection: "selec 1" starts at offset 5
    act(() => view().dispatch({ selection: { anchor: 5, head: 12 } }));
    await user.click(await screen.findByRole("button", { name: "Run" }));
    expect(await screen.findByText(/syntax error at or near/)).toBeInTheDocument();
    await waitFor(() => expect(document.querySelector(".cm-sql-error")).not.toBeNull());
    // position 3 in "selec 1" is index 2, shifted by the selection start 5
    await user.click(screen.getByRole("button", { name: "Go to error" }));
    expect(view().state.selection.main.head).toBe(7);
  });

  it("owner_banner_and_role_banner_copy", async () => {
    const { user } = setup();
    expect(await screen.findByText("Running as owner: row-level security is bypassed.")).toBeInTheDocument();
    await screen.findByRole("option", { name: "reporter" });
    await user.selectOptions(screen.getByLabelText("Run as"), "reporter");
    const banner = await screen.findByText(/Running as/);
    expect(textOf(banner)).toBe("Running as reporter (SET ROLE). This previews that role's view; it isn't a security boundary, since a script can RESET ROLE.");
    expect(banner.querySelector("code")?.textContent).toBe("reporter");
    expect(screen.queryByText(/row-level security is bypassed/)).not.toBeInTheDocument();
  });

  it("protected_banner_shows_on_protected_branches", async () => {
    setup({ branches: [branch({ protected: true })] });
    expect(await screen.findByText("Protected branch: every change asks for confirmation.")).toBeInTheDocument();
  });

  it("no_protected_banner_on_plain_branches", async () => {
    setup();
    await screen.findByLabelText("Branch");
    await screen.findByText(/Running as owner/);
    expect(screen.queryByText(/Protected branch/)).not.toBeInTheDocument();
  });

  it("anon_listed_only_when_bootstrapped", async () => {
    const { user } = setup({ databases: [dbView("app", true), dbView("analytics", false, "off")] });
    const sel = await screen.findByLabelText("Run as");
    await screen.findByRole("option", { name: "reporter" });
    const names = () => Array.from(sel.querySelectorAll("option")).map((o) => o.textContent);
    expect(names()).toEqual(["Owner", "anon", "authenticated", "reporter"]);
    await user.selectOptions(screen.getByLabelText("Database"), "analytics");
    await waitFor(() => expect(names()).toEqual(["Owner", "reporter"]));
  });

  it("owner_password_unknown_shows_the_notice", async () => {
    const { user } = setup({ run: () => failure("owner_password_unknown", 409) });
    await setDoc("select 1");
    await user.click(await screen.findByRole("button", { name: "Run" }));
    expect(await screen.findByText(/owner password isn't known/i)).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Reset the owner password" })).toBeInTheDocument();
  });

  it("schema_refetches_after_ddl", async () => {
    const { f, user } = setup({
      run: () =>
        ndjson({ t: "complete", statement: 0, tag: "CREATE TABLE", rows: 0, truncated: false }, { t: "done", outcome: "ok", duration_ms: 2 }),
    });
    await vi.waitFor(() => expect(schemaCalls(f)).toHaveLength(1));
    await setDoc("create table t(id int)");
    await user.click(screen.getByRole("button", { name: "Run" }));
    await vi.waitFor(() => expect(schemaCalls(f)).toHaveLength(2));
    await user.click(screen.getByRole("button", { name: "Refresh schema" }));
    await vi.waitFor(() => expect(schemaCalls(f)).toHaveLength(3));
    await user.selectOptions(screen.getByLabelText("Database"), "analytics");
    await vi.waitFor(() => expect(schemaCalls(f)).toHaveLength(4));
    expect(new URL(schemaCalls(f)[3].url).searchParams.get("database")).toBe("analytics");
  });

  it("schema_not_refetched_after_a_select", async () => {
    const { f, user } = setup();
    await vi.waitFor(() => expect(schemaCalls(f)).toHaveLength(1));
    await setDoc("select 1");
    await user.click(screen.getByRole("button", { name: "Run" }));
    await screen.findByRole("tab", { name: "SELECT 2" });
    expect(schemaCalls(f)).toHaveLength(1);
  });

  it("basic_completion_note_on_schema_too_large", async () => {
    const client = { ready: Promise.resolve(), setSchema: vi.fn(async () => {}), update: vi.fn(), complete: vi.fn(), lint: vi.fn(), hover: vi.fn(), dispose: vi.fn() } as unknown as PglsClient;
    setup({ schema: () => failure("schema_too_large", 422), createClient: () => Promise.resolve(client) });
    expect(await screen.findByText("Basic completion")).toBeInTheDocument();
    expect(screen.queryByText(/Invalid schema/)).not.toBeInTheDocument();
    expect(client.setSchema).not.toHaveBeenCalled();
  });

  it("no_basic_note_when_pgls_has_the_schema", async () => {
    const client = { ready: Promise.resolve(), setSchema: vi.fn(async () => {}), update: vi.fn(), complete: vi.fn(), lint: vi.fn(), hover: vi.fn(), dispose: vi.fn() } as unknown as PglsClient;
    setup({ createClient: () => Promise.resolve(client) });
    await vi.waitFor(() => expect(client.setSchema).toHaveBeenCalledWith(JSON.stringify({ schemas: [] })));
    await waitFor(() => expect(screen.queryByText("Basic completion")).not.toBeInTheDocument());
  });

  it("invalid_schema_from_pgls_falls_back_to_basic", async () => {
    const client = { ready: Promise.resolve(), setSchema: vi.fn(async () => { throw new Error("ERROR: Invalid schema: x"); }), update: vi.fn(), complete: vi.fn(), lint: vi.fn(), hover: vi.fn(), dispose: vi.fn() } as unknown as PglsClient;
    setup({ createClient: () => Promise.resolve(client) });
    expect(await screen.findByText("Basic completion")).toBeInTheDocument();
    expect(screen.queryByText(/Invalid schema/)).not.toBeInTheDocument();
  });

  it("run_errors_from_the_api_show_plain_text", async () => {
    const { user } = setup({ run: () => failure("sql_run_limit", 429) });
    await setDoc("select 1");
    await user.click(await screen.findByRole("button", { name: "Run" }));
    expect(await screen.findByRole("alert")).toBeInTheDocument();
  });
});
