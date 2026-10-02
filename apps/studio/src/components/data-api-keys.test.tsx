import { screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it, vi } from "vitest";
import { getListKeysQueryKey, type BranchDatabaseView, type CreatedDataKey, type DataKey } from "@cheladb/management-client";
import { fakeFetch, jsonResponse, type FakeFetch, type FakeHandler } from "@/test/fake-fetch";
import { renderWithQuery } from "@/test/render";
import { DataApiKeys } from "./data-api-keys";

const key = (over: Partial<DataKey> & { id: string; name: string }): DataKey => ({
  kind: "secret",
  role: "authenticated",
  databases: null,
  prefix: "chk_sec_ab12",
  created_at: "2026-10-01T09:00:00Z",
  last_used_at: null,
  role_blocked: false,
  ...over,
});
const publicKey = key({ id: "pub", name: "public", kind: "public", role: "anon", prefix: "chk_pub_zz99", key: "chk_pub_secretvalue" });
const reporting = key({ id: "k1", name: "reporting", last_used_at: "2026-10-01T10:30:00Z" });
const worker = key({ id: "k2", name: "worker", role: "service" });
const FULL = "chk_sec_ab12CDEFGHIJKLMNOPQRSTUVWXYZ0123456789abcde";
const created = (over: Partial<CreatedDataKey> = {}): CreatedDataKey => ({
  id: "k3",
  databases: null,
  kind: "secret",
  role: "authenticated",
  name: "ci",
  prefix: "chk_sec_ab12",
  key: FULL,
  ...over,
});

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
const databases = [dbRow("web1", { origin: "main", main: true }), dbRow("app"), dbRow("later", { data_api: "off" }), dbRow("postgres", { origin: "system", data_api: "off" })];

const KEYS = "/v1/databases/db/branches/br/keys";
const CREATE = `POST ${KEYS}`;
const revokeRoute = (id: string) => `DELETE ${KEYS}/${id}`;
const rotateRoute = (id: string) => `POST ${KEYS}/${id}/rotate`;
const revoked: FakeHandler = () => new Response(null, { status: 204 });
const never: FakeHandler = () => new Promise<never>(() => {});
const refused = (status: number, code: string, message: string): FakeHandler => () =>
  jsonResponse({ error: { code, message } }, status);

function setup(routes: Record<string, FakeHandler> = {}, keys: DataKey[] = [publicKey, reporting, worker]) {
  const f = fakeFetch(routes);
  const result = renderWithQuery(<DataApiKeys databaseId="db" branchId="br" keys={keys} databases={databases} />, { fetch: f });
  // Seeded as the panel would have it: every change must invalidate it.
  result.queryClient.setQueryData(getListKeysQueryKey("db", "br"), keys);
  result.queryClient.setQueryData(getListKeysQueryKey("db", "other"), keys);
  return { ...result, fetch: f };
}
/** The key calls only: the keys' roles are read in the background. */
const sent = (f: FakeFetch) =>
  f.calls.map((r) => `${r.method} ${new URL(r.url).pathname}`).filter((c) => !c.endsWith("/roles"));
async function bodyOf(f: FakeFetch, route: string): Promise<unknown> {
  await vi.waitFor(() => expect(sent(f)).toContain(route));
  return f.calls.find((r) => `${r.method} ${new URL(r.url).pathname}` === route)!.clone().json();
}
async function expectKeysInvalidated(queryClient: ReturnType<typeof setup>["queryClient"]) {
  await vi.waitFor(() => expect(queryClient.getQueryState(getListKeysQueryKey("db", "br"))?.isInvalidated).toBe(true));
  expect(queryClient.getQueryState(getListKeysQueryKey("db", "other"))?.isInvalidated).toBe(false);
}
/** Every piece of state TanStack Query holds: queries and mutations. */
function cachedState(queryClient: ReturnType<typeof setup>["queryClient"]): string {
  return JSON.stringify([
    queryClient.getQueryCache().findAll().map((q) => q.state),
    queryClient.getMutationCache().getAll().map((m) => m.state),
  ]);
}

afterEach(() => vi.unstubAllGlobals());

describe("DataApiKeys", () => {
  it("lists the secret keys with prefix, role and last used, and leaves the public key out", () => {
    setup();
    const row = screen.getByRole("row", { name: /reporting/ });
    expect(within(row).getByText("chk_sec_ab12…")).toBeInTheDocument();
    expect(within(row).getByText("authenticated")).toBeInTheDocument();
    expect(within(row).getByText("2026-10-01 10:30 UTC")).toBeInTheDocument();
    expect(within(screen.getByRole("row", { name: /worker/ })).getByText("never")).toBeInTheDocument();
    expect(screen.queryByText("chk_pub_zz99…")).not.toBeInTheDocument();
  });

  it("says when there are no secret keys", () => {
    setup({}, [publicKey]);
    expect(screen.getByText("No secret keys yet.")).toBeInTheDocument();
  });

  it("creates a key with the default role and shows it once, with copy", async () => {
    const writeText = vi.fn().mockResolvedValue(undefined);
    vi.stubGlobal("navigator", { ...navigator, clipboard: { writeText } });
    const { fetch, queryClient } = setup({ [CREATE]: () => jsonResponse(created(), 201) });
    expect(screen.getByLabelText("Role")).toHaveValue("authenticated");
    await userEvent.type(screen.getByLabelText("Key name"), "ci");
    await userEvent.click(screen.getByRole("button", { name: "Create key" }));
    expect(await bodyOf(fetch, CREATE)).toEqual({ name: "ci", role: "authenticated" });
    const box = await screen.findByRole("region", { name: "New key" });
    expect(within(box).getByText(FULL)).toBeInTheDocument();
    expect(within(box).getByText("Copy this key now. It won't be shown again.")).toBeInTheDocument();
    await userEvent.click(within(box).getByRole("button", { name: "Copy key" }));
    expect(writeText).toHaveBeenCalledWith(FULL);
    await expectKeysInvalidated(queryClient);
    expect(screen.getByLabelText("Key name")).toHaveValue("");
    await userEvent.click(within(box).getByRole("button", { name: "Dismiss" }));
    expect(screen.queryByText(FULL)).not.toBeInTheDocument();
  });

  it("create_secret_key_with_databases", async () => {
    const { fetch } = setup({ [CREATE]: () => jsonResponse(created({ databases: ["app", "later"] }), 201) });
    await userEvent.type(screen.getByLabelText("Key name"), "ci");
    const group = screen.getByRole("group", { name: "Databases" });
    await userEvent.click(within(group).getByLabelText("All served databases"));
    // The system database is never offered; a database that isn't served is marked.
    expect(within(group).queryByLabelText(/postgres/)).not.toBeInTheDocument();
    // Unticking "All" starts from the served databases.
    await userEvent.click(within(group).getByLabelText("later (not served)"));
    await userEvent.click(within(group).getByLabelText("web1"));
    await userEvent.click(screen.getByRole("button", { name: "Create key" }));
    expect(await bodyOf(fetch, CREATE)).toEqual({ name: "ci", role: "authenticated", databases: ["app", "later"] });
  });

  it("resets the allow-list after a key is created", async () => {
    const { fetch } = setup({ [CREATE]: () => jsonResponse(created({ databases: ["app", "later"] }), 201) });
    await userEvent.type(screen.getByLabelText("Key name"), "ci");
    const group = screen.getByRole("group", { name: "Databases" });
    await userEvent.click(within(group).getByLabelText("All served databases"));
    await userEvent.click(screen.getByRole("button", { name: "Create key" }));
    await bodyOf(fetch, CREATE);
    await screen.findByRole("region", { name: "New key" });
    expect(within(screen.getByRole("group", { name: "Databases" })).getByLabelText("All served databases")).toBeChecked();
  });

  it("does not create a key with an empty selection", async () => {
    setup();
    await userEvent.type(screen.getByLabelText("Key name"), "ci");
    const group = screen.getByRole("group", { name: "Databases" });
    await userEvent.click(within(group).getByLabelText("All served databases"));
    await userEvent.click(within(group).getByLabelText("web1"));
    await userEvent.click(within(group).getByLabelText("app"));
    expect(screen.getByRole("button", { name: "Create key" })).toBeDisabled();
    expect(screen.getByText("Pick at least one database.")).toBeInTheDocument();
  });

  it("shows each secret key's allow-list", () => {
    setup({}, [publicKey, key({ id: "k1", name: "reporting", databases: ["app"] }), worker]);
    expect(within(screen.getByRole("row", { name: /reporting/ })).getByText("app")).toBeInTheDocument();
    expect(within(screen.getByRole("row", { name: /worker/ })).getByText("All served databases")).toBeInTheDocument();
  });

  it("sends the role the user picked", async () => {
    const { fetch } = setup({ [CREATE]: () => jsonResponse(created({ role: "service" }), 201) });
    await userEvent.type(screen.getByLabelText("Key name"), "ci");
    await userEvent.clear(screen.getByLabelText("Role"));
    await userEvent.type(screen.getByLabelText("Role"), "service");
    await userEvent.click(screen.getByRole("button", { name: "Create key" }));
    expect(await bodyOf(fetch, CREATE)).toEqual({ name: "ci", role: "service" });
  });

  it("disables Create until there is a name, and says why", async () => {
    setup();
    const button = screen.getByRole("button", { name: "Create key" });
    expect(button).toBeDisabled();
    const reason = screen.getByText("Enter a name for the key.");
    expect(button.getAttribute("aria-describedby")).toBe(reason.id);
    await userEvent.type(screen.getByLabelText("Key name"), "ci");
    expect(button).toBeEnabled();
    expect(button).not.toHaveAttribute("aria-describedby");
  });

  it("shows the worker's message on a 422 and no key", async () => {
    const { queryClient } = setup({ [CREATE]: refused(422, "role_refused", 'role "postgres" is a superuser') });
    await userEvent.type(screen.getByLabelText("Key name"), "ci");
    await userEvent.click(screen.getByRole("button", { name: "Create key" }));
    expect(await screen.findByRole("alert")).toHaveTextContent('role "postgres" is a superuser');
    expect(screen.queryByRole("region", { name: "New key" })).not.toBeInTheDocument();
    expect(queryClient.getQueryState(getListKeysQueryKey("db", "br"))?.isInvalidated).toBe(false);
    expect(screen.getByLabelText("Key name")).toHaveValue("ci");
  });

  it("says why Revoke and Rotate are disabled while a key change runs", async () => {
    setup({ [revokeRoute("k1")]: never });
    const revoke = () => screen.getByRole("button", { name: "Revoke worker" });
    const rotate = () => screen.getByRole("button", { name: "Rotate reporting" });
    expect(revoke()).not.toHaveAttribute("aria-describedby");
    await userEvent.click(screen.getByRole("button", { name: "Revoke reporting" }));
    await userEvent.click(screen.getByRole("button", { name: "Confirm revoke" }));
    for (const b of [revoke(), rotate()]) {
      expect(b).toBeDisabled();
      expect(b).toHaveAccessibleDescription("Another key change is still running.");
    }
  });

  it("revokes only after a confirm", async () => {
    const { fetch, queryClient } = setup({ [revokeRoute("k1")]: revoked });
    await userEvent.click(screen.getByRole("button", { name: "Revoke reporting" }));
    expect(sent(fetch)).toHaveLength(0);
    expect(screen.getByText(/Revoke reporting\? Anything using it stops working within 30 seconds\./)).toBeInTheDocument();
    await userEvent.click(screen.getByRole("button", { name: "Confirm revoke" }));
    await vi.waitFor(() => expect(sent(fetch)).toEqual([revokeRoute("k1")]));
    await expectKeysInvalidated(queryClient);
    await vi.waitFor(() => expect(screen.queryByRole("button", { name: "Confirm revoke" })).not.toBeInTheDocument());
    expect(screen.queryByRole("button", { name: "Confirm revoke" })).not.toBeInTheDocument();
  });

  it("cancel leaves the key alone", async () => {
    const { fetch } = setup({ [revokeRoute("k1")]: revoked });
    await userEvent.click(screen.getByRole("button", { name: "Revoke reporting" }));
    await userEvent.click(screen.getByRole("button", { name: "Cancel" }));
    expect(sent(fetch)).toHaveLength(0);
    expect(screen.queryByRole("button", { name: "Confirm revoke" })).not.toBeInTheDocument();
  });

  it("rotates after a confirm and shows the new key once", async () => {
    const { fetch, queryClient } = setup({ [rotateRoute("k1")]: () => jsonResponse(created({ id: "k9", name: "reporting" })) });
    await userEvent.click(screen.getByRole("button", { name: "Rotate reporting" }));
    expect(sent(fetch)).toHaveLength(0);
    await userEvent.click(screen.getByRole("button", { name: "Confirm rotate" }));
    expect(await screen.findByRole("region", { name: "New key" })).toHaveTextContent(FULL);
    expect(sent(fetch)).toEqual([rotateRoute("k1")]);
    await expectKeysInvalidated(queryClient);
  });

  it("shows a revoke failure", async () => {
    setup({ [revokeRoute("k1")]: refused(404, "not_found", "key not found") });
    await userEvent.click(screen.getByRole("button", { name: "Revoke reporting" }));
    await userEvent.click(screen.getByRole("button", { name: "Confirm revoke" }));
    expect(await screen.findByRole("alert")).toHaveTextContent("key not found");
  });

  it("disables Create while the key is being created", async () => {
    setup({ [CREATE]: never });
    await userEvent.type(screen.getByLabelText("Key name"), "ci");
    await userEvent.click(screen.getByRole("button", { name: "Create key" }));
    expect(screen.getByRole("button", { name: "Create key" })).toBeDisabled();
  });

  it("created_key_is_never_cached: a created or rotated key stays out of the query and mutation caches", async () => {
    const ROTATED = "chk_sec_ROTATEDROTATEDROTATEDROTATEDROTATEDROTATED";
    const { queryClient, unmount } = setup({
      [CREATE]: () => jsonResponse(created(), 201),
      [rotateRoute("k1")]: () => jsonResponse(created({ id: "k9", name: "reporting", key: ROTATED })),
    });
    await userEvent.type(screen.getByLabelText("Key name"), "ci");
    await userEvent.click(screen.getByRole("button", { name: "Create key" }));
    expect(await screen.findByText(FULL)).toBeInTheDocument();
    await userEvent.click(screen.getByRole("button", { name: "Rotate reporting" }));
    await userEvent.click(screen.getByRole("button", { name: "Confirm rotate" }));
    expect(await screen.findByText(ROTATED)).toBeInTheDocument();
    await expectKeysInvalidated(queryClient);
    for (const secret of [FULL, ROTATED]) expect(cachedState(queryClient)).not.toContain(secret);
    // Nor after the screen goes away (a mutation's data would outlive it for gcTime).
    unmount();
    for (const secret of [FULL, ROTATED]) expect(cachedState(queryClient)).not.toContain(secret);
  });

  it("blocked_key_badge", async () => {
    const f = setup({}, [publicKey, reporting, { ...worker, role_blocked: true }]).fetch;
    const row = within(await screen.findByRole("row", { name: /worker/ }));
    expect(row.getByText("Blocked")).toBeInTheDocument();
    expect(row.getByText("the role service can bypass row-level security")).toBeInTheDocument();
    expect(within(screen.getByRole("row", { name: /reporting/ })).queryByText("Blocked")).toBeNull();
    expect(f.calls.some((r) => new URL(r.url).pathname.endsWith("/roles"))).toBe(false);
  });
});
