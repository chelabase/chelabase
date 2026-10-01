import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { BranchSummary, DataApiSchema, DataKey } from "@cheladb/types";
import type { ApiClient } from "@/lib/api";
import { fakeApi } from "@/test/fake-api";
import { DataApiPanel } from "./data-api-panel";

const branch = { id: "br", name: "main", kind: "normal", status: "ready" } as unknown as BranchSummary;
const PUB = "chk_pub_0123456789abcdefghijklmnopqrstuvwxyzABCDEFG";
const keys: DataKey[] = [
  { id: "pub", kind: "public", role: "anon", name: "public", prefix: "chk_pub_0123", created_at: "2026-10-01T09:00:00Z", last_used_at: null, key: PUB },
  { id: "k1", kind: "secret", role: "authenticated", name: "reporting", prefix: "chk_sec_ab12", created_at: "2026-10-01T09:00:00Z", last_used_at: null },
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

function setup(api: Partial<ApiClient> = {}) {
  const full = fakeApi({
    listKeys: vi.fn().mockResolvedValue(keys),
    getDataApiSchema: vi.fn().mockResolvedValue(emptySchema),
    ...api,
  });
  render(<DataApiPanel api={full} databaseId="db" branch={branch} apiUrl="http://gw/" />);
  return full;
}

afterEach(() => vi.unstubAllGlobals());

describe("DataApiPanel", () => {
  it("loads the keys and schema for the branch", async () => {
    const api = setup();
    await screen.findByText(PUB);
    expect(api.listKeys).toHaveBeenCalledWith("db", "br");
    expect(api.getDataApiSchema).toHaveBeenCalledWith("db", "br");
  });

  it("shows the endpoint and the public key, each with a copy button", async () => {
    const writeText = vi.fn().mockResolvedValue(undefined);
    vi.stubGlobal("navigator", { ...navigator, clipboard: { writeText } });
    setup();
    expect(await screen.findByText("http://gw/data/v1/query")).toBeInTheDocument();
    expect(screen.getByText(PUB)).toBeInTheDocument();
    await userEvent.click(screen.getByRole("button", { name: "Copy endpoint" }));
    expect(writeText).toHaveBeenLastCalledWith("http://gw/data/v1/query");
    await userEvent.click(screen.getByRole("button", { name: "Copy public key" }));
    expect(writeText).toHaveBeenLastCalledWith(PUB);
  });

  it("says why Rotate public key is disabled: loading, no key, or a change in flight", async () => {
    const never = new Promise<never>(() => {});
    const { unmount } = render(
      <DataApiPanel api={fakeApi({ listKeys: vi.fn().mockReturnValue(never), getDataApiSchema: vi.fn().mockReturnValue(never) })} databaseId="db" branch={branch} apiUrl="http://gw" />,
    );
    const rotate = () => screen.getByRole("button", { name: "Rotate public key" });
    expect(rotate()).toBeDisabled();
    expect(rotate()).toHaveAccessibleDescription("Waiting for the keys to load.");
    unmount();

    const noPublic = render(
      <DataApiPanel api={fakeApi({ listKeys: vi.fn().mockResolvedValue([keys[1]]), getDataApiSchema: vi.fn().mockResolvedValue(emptySchema) })} databaseId="db" branch={branch} apiUrl="http://gw" />,
    );
    await screen.findByText("reporting");
    expect(rotate()).toHaveAccessibleDescription("This branch has no public key.");
    noPublic.unmount();

    setup({ rotateKey: vi.fn().mockReturnValue(never) });
    await userEvent.click(await screen.findByRole("button", { name: "Rotate public key" }));
    expect(rotate()).toBeEnabled();
    expect(rotate()).not.toHaveAttribute("aria-describedby");
    await userEvent.click(screen.getByRole("button", { name: "Confirm rotate public key" }));
    expect(rotate()).toBeDisabled();
    expect(rotate()).toHaveAccessibleDescription("Working on a key change.");
  });

  it("rotates the public key only after a confirm, then shows the new one", async () => {
    const NEW = "chk_pub_NEWNEWNEWNEWNEWNEWNEWNEWNEWNEWNEWNEWNEWNEW1";
    const listKeys = vi
      .fn()
      .mockResolvedValueOnce(keys)
      .mockResolvedValue([{ ...keys[0], id: "pub2", key: NEW }, keys[1]]);
    const rotateKey = vi.fn().mockResolvedValue({ id: "pub2", kind: "public", role: "anon", name: "public", prefix: "chk_pub_NEWN", key: NEW });
    setup({ listKeys, rotateKey });
    await userEvent.click(await screen.findByRole("button", { name: "Rotate public key" }));
    expect(rotateKey).not.toHaveBeenCalled();
    expect(screen.getByText(/Rotate the public key\? The current key stops working within 30 seconds\./)).toBeInTheDocument();
    await userEvent.click(screen.getByRole("button", { name: "Confirm rotate public key" }));
    expect(rotateKey).toHaveBeenCalledWith("db", "br", "pub");
    expect(await screen.findByText(NEW)).toBeInTheDocument();
    expect(screen.queryByText(PUB)).not.toBeInTheDocument();
  });

  it("lists the secret keys", async () => {
    setup();
    expect(await screen.findByRole("row", { name: /reporting/ })).toBeInTheDocument();
  });

  it("shows the RLS warning when a reachable table has RLS off, and not otherwise", async () => {
    setup({ getDataApiSchema: vi.fn().mockResolvedValue(openSchema) });
    expect(await screen.findByText(/Anyone with your public key can read these rows/)).toBeInTheDocument();
    expect(screen.getByText('ALTER TABLE "public"."notes" ENABLE ROW LEVEL SECURITY;')).toBeInTheDocument();
  });

  it("shows no warning for an empty schema", async () => {
    setup();
    await screen.findByText(PUB);
    expect(screen.queryByText(/Anyone with your public key/)).not.toBeInTheDocument();
  });

  it("keeps the keys when the schema can't be read, and says so", async () => {
    setup({ getDataApiSchema: vi.fn().mockRejectedValue(new Error("data API unavailable")) });
    expect(await screen.findByText(PUB)).toBeInTheDocument();
    await waitFor(() => expect(screen.getByRole("alert")).toHaveTextContent("Couldn't check RLS: data API unavailable"));
  });

  it("shows a key list failure", async () => {
    setup({ listKeys: vi.fn().mockRejectedValue(new Error("gateway down")) });
    expect(await screen.findByRole("alert")).toHaveTextContent("gateway down");
  });
});
