import { screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";
import type { Branch, DatabaseSummary } from "@chelabase/management-client";
import { fakeFetch, jsonResponse, type FakeFetch, type FakeHandler } from "@/test/fake-fetch";
import { renderWithQuery } from "@/test/render";
import { DatabaseDataApi } from "./database-data-api";

const summary: DatabaseSummary = { database_id: "db", name: "web1", status: "ready", host: "h", port: 4432, branch_count: 2 };
const b = (name: string, over: Partial<Branch> = {}): Branch => ({
  id: `id-${name}`,
  name,
  parent_branch_id: name === "main" ? null : "id-main",
  kind: "normal",
  status: "ready",
  host: "h",
  port: 4432,
  branched_at_time: null,
  branched_at_lsn: null,
  created_at: "2026-09-30T10:00:00Z",
  history_start: null,
  error: null,
  protected: false,
  main_database_missing: false,
  ...over,
});
const keys = [
  { id: "pub", kind: "public", role: "anon", name: "public", prefix: "chk_pub_0123", created_at: "2026-10-01T09:00:00Z", last_used_at: null, key: "chk_pub_PUBKEY" },
];
const schema = { version: "1", default_schema: "public", schemas: {} };

const keyPaths = (f: FakeFetch) =>
  f.calls.map((r) => new URL(r.url).pathname).filter((p) => p.endsWith("/keys"));

function setup(list: FakeHandler) {
  const f = fakeFetch({
    "GET /v1/databases/db/branches": list,
    "GET /v1/databases/db/branches/id-main/keys": () => jsonResponse(keys),
    "GET /v1/databases/db/branches/id-dev/keys": () => jsonResponse(keys),
    "GET /v1/databases/db/branches/id-main/data-api/schema": () => jsonResponse(schema),
    "GET /v1/databases/db/branches/id-dev/data-api/schema": () => jsonResponse(schema),
  });
  renderWithQuery(<DatabaseDataApi apiUrl="http://gw" database={summary} />, { fetch: f });
  return f;
}

describe("DatabaseDataApi", () => {
  it("shows the panel for main, and for the branch picked", async () => {
    const f = setup(() =>
      jsonResponse([b("dev"), b("main"), b("bak", { kind: "backup" }), b("busy", { status: "provisioning" })]),
    );
    expect(await screen.findByText("chk_pub_PUBKEY")).toBeInTheDocument();
    expect(keyPaths(f)).toEqual(["/v1/databases/db/branches/id-main/keys"]);
    expect(screen.getByText("http://gw/data/v1/query")).toBeInTheDocument();
    const picker = screen.getByLabelText("Data API branch") as HTMLSelectElement;
    expect(Array.from(picker.options).map((o) => o.text)).toEqual(["dev", "main"]);
    expect(picker).toHaveValue("id-main");
    await userEvent.selectOptions(picker, "dev");
    await waitFor(() => expect(keyPaths(f).at(-1)).toBe("/v1/databases/db/branches/id-dev/keys"));
  });

  it("says when no branch is ready", async () => {
    setup(() => jsonResponse([b("main", { status: "provisioning" })]));
    expect(await screen.findByText("No branch is ready yet.")).toBeInTheDocument();
  });

  it("shows a load error", async () => {
    setup(() => jsonResponse({ error: { code: "unavailable", message: "gateway down" } }, 503));
    expect(await screen.findByRole("alert")).toHaveTextContent("gateway down");
  });
});
