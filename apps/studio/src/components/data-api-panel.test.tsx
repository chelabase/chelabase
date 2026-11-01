import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it, vi } from "vitest";
import { getListKeysQueryKey, type Branch, type BranchDatabaseView, type DataApiSchema, type DataKey } from "@chelabase/management-client";
import { fakeFetch, jsonResponse, sequence, type FakeFetch, type FakeHandler } from "@/test/fake-fetch";
import { renderWithQuery } from "@/test/render";
import { DataApiPanel } from "./data-api-panel";

const branch = { id: "br", name: "main", kind: "normal", status: "ready" } as unknown as Branch;
const PUB = "chk_pub_0123456789abcdefghijklmnopqrstuvwxyzABCDEFG";
const keys: DataKey[] = [
  { id: "pub", kind: "public", databases: null, role: "anon", name: "public", prefix: "chk_pub_0123", created_at: "2026-10-01T09:00:00Z", last_used_at: null, role_blocked: false, bypasses_rls: false, key: PUB },
  { id: "k1", kind: "secret", databases: null, role: "authenticated", name: "reporting", prefix: "chk_sec_ab12", created_at: "2026-10-01T09:00:00Z", last_used_at: null, role_blocked: false, bypasses_rls: false },
];
const emptySchema: DataApiSchema = { version: "1", default_schema: "public", schemas: {} };
const openSchema: DataApiSchema = {
  version: "1",
  default_schema: "public",
  schemas: {
    public: {
      tables: { notes: { kind: "table", rls: false, security_invoker: false, privileges: { anon: ["select"], authenticated: [] }, columns: {} } },
      functions: {},
      enums: {},
    },
  },
};

const dbRow = (name: string, over: Partial<BranchDatabaseView> = {}): BranchDatabaseView => ({
  name,
  owner: "o",
  origin: "sql",
  main: false,
  data_api: "on",
  default_schema: "public",
  data_api_error: null,
  ...over,
});
const databases = [dbRow("web1", { origin: "main", main: true }), dbRow("app"), dbRow("later", { data_api: "off" })];

const DATABASES = "GET /v1/databases/db/branches/br/databases";
const KEYS = "GET /v1/databases/db/branches/br/keys";
const SCHEMA = "GET /v1/databases/db/branches/br/data-api/schema";
const ROTATE = "POST /v1/databases/db/branches/br/keys/pub/rotate";
const never: FakeHandler = () => new Promise<never>(() => {});
const sent = (f: FakeFetch) => f.calls.map((r) => `${r.method} ${new URL(r.url).pathname}`);

function setup(routes: Record<string, FakeHandler> = {}, pgDatabase?: string) {
  const f = fakeFetch({ [DATABASES]: () => jsonResponse(databases), [KEYS]: () => jsonResponse(keys), [SCHEMA]: () => jsonResponse(emptySchema), ...routes });
  const ui = (pg?: string) => <DataApiPanel databaseId="db" branch={branch} apiUrl="http://gw/" pgDatabase={pg} />;
  const result = renderWithQuery(ui(pgDatabase), { fetch: f });
  return { ...result, fetch: f, pick: (pg: string) => result.rerender(ui(pg)) };
}

afterEach(() => vi.unstubAllGlobals());

describe("DataApiPanel", () => {
  it("loads the keys and schema for the branch", async () => {
    const { fetch } = setup();
    await screen.findByText(PUB);
    await waitFor(() => expect(sent(fetch).filter((c) => !c.endsWith("/roles")).sort()).toEqual([DATABASES, KEYS, SCHEMA].sort()));
  });

  it("keeps the keys visible when the databases list fails", async () => {
    setup({ [DATABASES]: () => jsonResponse({ error: { code: "internal", message: "down" } }, 500) });
    expect(await screen.findByRole("alert")).toBeInTheDocument();
    expect(await screen.findByText("reporting")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Create key" })).toBeInTheDocument();
  });

  it("shows the endpoint and the public key, each with a copy button", async () => {
    const writeText = vi.fn().mockResolvedValue(undefined);
    vi.stubGlobal("navigator", { ...navigator, clipboard: { writeText } });
    setup();
    expect(await screen.findByText("http://gw/data/v1/query")).toBeInTheDocument();
    expect(await screen.findByText(PUB)).toBeInTheDocument();
    await userEvent.click(screen.getByRole("button", { name: "Copy endpoint" }));
    expect(writeText).toHaveBeenLastCalledWith("http://gw/data/v1/query");
    await userEvent.click(screen.getByRole("button", { name: "Copy public key" }));
    expect(writeText).toHaveBeenLastCalledWith(PUB);
  });

  it("says why Rotate public key is disabled: loading, no key, or a change in flight", async () => {
    const { unmount } = setup({ [KEYS]: never, [SCHEMA]: never });
    const rotate = () => screen.getByRole("button", { name: "Rotate public key" });
    expect(rotate()).toBeDisabled();
    expect(rotate()).toHaveAccessibleDescription("Waiting for the keys to load.");
    unmount();

    const noPublic = setup({ [KEYS]: () => jsonResponse([keys[1]]) });
    await screen.findByText("reporting");
    expect(rotate()).toHaveAccessibleDescription("This branch has no public key.");
    noPublic.unmount();

    setup({ [ROTATE]: never });
    await userEvent.click(await screen.findByRole("button", { name: "Rotate public key" }));
    expect(rotate()).toBeEnabled();
    expect(rotate()).not.toHaveAttribute("aria-describedby");
    await userEvent.click(screen.getByRole("button", { name: "Confirm rotate public key" }));
    expect(rotate()).toBeDisabled();
    expect(rotate()).toHaveAccessibleDescription("Working on a key change.");
  });

  it("rotates the public key only after a confirm, then shows the new one", async () => {
    const NEW = "chk_pub_NEWNEWNEWNEWNEWNEWNEWNEWNEWNEWNEWNEWNEWNEW1";
    const { fetch, queryClient } = setup({
      [KEYS]: sequence(
        () => jsonResponse(keys),
        () => jsonResponse([{ ...keys[0], id: "pub2", key: NEW }, keys[1]]),
      ),
      [ROTATE]: () => jsonResponse({ id: "pub2", kind: "public", role: "anon", name: "public", prefix: "chk_pub_NEWN", key: NEW }),
    });
    queryClient.setQueryData(getListKeysQueryKey("db", "other"), keys);
    await userEvent.click(await screen.findByRole("button", { name: "Rotate public key" }));
    expect(sent(fetch)).not.toContain(ROTATE);
    expect(screen.getByText(/Rotate the public key\? The current key stops working within 30 seconds\./)).toBeInTheDocument();
    await userEvent.click(screen.getByRole("button", { name: "Confirm rotate public key" }));
    expect(await screen.findByText(NEW)).toBeInTheDocument();
    expect(sent(fetch)).toContain(ROTATE);
    expect(screen.queryByText(PUB)).not.toBeInTheDocument();
    expect(screen.queryByText("Confirm rotate public key")).not.toBeInTheDocument();
    expect(queryClient.getQueryState(getListKeysQueryKey("db", "other"))?.isInvalidated).toBe(false);
  });

  it("lists the secret keys", async () => {
    setup();
    expect(await screen.findByRole("row", { name: /reporting/ })).toBeInTheDocument();
  });

  it("shows the RLS warning when a reachable table has RLS off, and not otherwise", async () => {
    setup({ [SCHEMA]: () => jsonResponse(openSchema) });
    expect(await screen.findByText(/Anyone with your public key can read these rows/)).toBeInTheDocument();
    expect(screen.getByText('ALTER TABLE "public"."notes" ENABLE ROW LEVEL SECURITY;')).toBeInTheDocument();
  });

  it("reads the schema of the picked database, and only a served one", async () => {
    const { fetch, pick } = setup({ [SCHEMA]: (r) => jsonResponse(new URL(r.url).searchParams.get("database") === "app" ? openSchema : emptySchema) });
    await screen.findByText(PUB);
    const schemaDbs = () =>
      fetch.calls.filter((r) => new URL(r.url).pathname.endsWith("/data-api/schema")).map((r) => new URL(r.url).searchParams.get("database"));
    await waitFor(() => expect(schemaDbs()).toEqual(["web1"]));
    expect(screen.queryByText(/Anyone with your public key/)).not.toBeInTheDocument();
    pick("app");
    expect(await screen.findByText(/Anyone with your public key/)).toBeInTheDocument();
    pick("later");
    await waitFor(() => expect(screen.queryByText(/Anyone with your public key/)).not.toBeInTheDocument());
    expect(schemaDbs()).toEqual(["web1", "app"]);
  });

  it("enabling_polls_until_on", async () => {
    const list = sequence(
      () => jsonResponse([dbRow("web1", { origin: "main", main: true }), dbRow("later", { data_api: "enabling" })]),
      () => jsonResponse([dbRow("web1", { origin: "main", main: true }), dbRow("later", { data_api: "on" })]),
    );
    const { fetch } = setup({ [DATABASES]: list }, "later");
    expect(await screen.findByText("Enabling the data API…")).toBeInTheDocument();
    await vi.waitFor(() => expect(screen.getByRole("button", { name: "Disable data API" })).toBeInTheDocument(), { timeout: 5000 });
    expect(fetch.calls.filter((r) => new URL(r.url).pathname.endsWith("/branches/br/databases"))).toHaveLength(2);
  });

  it("allow_list_edit_for_public_key", async () => {
    const PATCH_KEY = "PATCH /v1/databases/db/branches/br/keys/pub";
    const { fetch } = setup({ [PATCH_KEY]: () => jsonResponse({ ...keys[0], databases: ["app"] }) });
    await screen.findByText(PUB);
    const pub = screen.getByRole("region", { name: "Public key databases" });
    expect(within(pub).getByText("All served databases")).toBeInTheDocument();
    await userEvent.click(within(pub).getByRole("button", { name: "Edit databases" }));
    await userEvent.click(within(pub).getByLabelText("All served databases"));
    // Starts from the served databases ticked: untick all but app.
    await userEvent.click(within(pub).getByLabelText("web1"));
    await userEvent.click(within(pub).getByRole("button", { name: "Save databases" }));
    const call = await vi.waitFor(() => {
      const c = fetch.calls.find((r) => `${r.method} ${new URL(r.url).pathname}` === PATCH_KEY);
      expect(c).toBeDefined();
      return c!;
    });
    expect(await call.clone().json()).toEqual({ databases: ["app"] });
  });

  it("shows no warning for an empty schema", async () => {
    setup();
    await screen.findByText(PUB);
    expect(screen.queryByText(/Anyone with your public key/)).not.toBeInTheDocument();
  });

  it("keeps the keys when the schema can't be read, and says so", async () => {
    setup({ [SCHEMA]: () => jsonResponse({ error: { code: "worker_unavailable", message: "data API unavailable" } }, 503) });
    expect(await screen.findByText(PUB)).toBeInTheDocument();
    await waitFor(() => expect(screen.getByRole("alert")).toHaveTextContent("Couldn't check RLS: data API unavailable"));
  });

  it("shows a key list failure", async () => {
    setup({ [KEYS]: () => jsonResponse({ error: { code: "unavailable", message: "gateway down" } }, 503) });
    expect(await screen.findByRole("alert")).toHaveTextContent("gateway down");
  });

  it("shows a rotate failure and keeps the keys", async () => {
    setup({ [ROTATE]: () => jsonResponse({ error: { code: "not_found", message: "key not found" } }, 404) });
    await userEvent.click(await screen.findByRole("button", { name: "Rotate public key" }));
    await userEvent.click(screen.getByRole("button", { name: "Confirm rotate public key" }));
    expect(await screen.findByRole("alert")).toHaveTextContent("key not found");
    expect(screen.getByText(PUB)).toBeInTheDocument();
  });
});
