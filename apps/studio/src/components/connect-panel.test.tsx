import { RouterProvider, createMemoryHistory, createRootRoute, createRoute, createRouter } from "@tanstack/react-router";
import { screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";
import type { BranchDatabaseView, DatabaseSummary, MeResponse } from "@chelabase/management-client";
import { ProjectCapsContext, projectCaps } from "@/lib/capabilities";
import { fakeFetch, jsonResponse, type FakeFetch, type FakeHandler } from "@/test/fake-fetch";
import { renderWithQuery } from "@/test/render";
import { ConnectPanel } from "./connect-panel";

const summary: DatabaseSummary = { database_id: "db", name: "web1", postgres_database: "web1", status: "ready", host: "h", port: 4432, branch_count: 2 };
const b = (name: string, over: Record<string, unknown> = {}) => ({
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
  guard_logins: true,
  review_acknowledged_hash: null,
  ...over,
});
const key = (kind: "public" | "secret", id: string, name: string) => ({
  id,
  kind,
  role: kind === "public" ? "anon" : "authenticated",
  name,
  prefix: `chk_${id}`,
  databases: null,
  created_at: "2026-10-01T09:00:00Z",
  last_used_at: null,
  role_blocked: false,
  bypasses_rls: false,
  ...(kind === "public" ? { key: `chk_pub_${name}` } : {}),
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
const schema = { version: "1", default_schema: "public", schemas: {} };

const me = (role: "viewer" | "developer"): MeResponse =>
  ({
    email: "a@b.c",
    name: "Ada",
    instance_admin: false,
    member: { id: "mem-1", status: "active", tenant_admin: false },
    projects: [{ project_id: "p1", platform_role: role, roles: [], databases: [] }],
    tenant: null,
  }) as unknown as MeResponse;

const branchRoute = (id: string) => `GET /v1/databases/db/branches/${id}`;
const keyPaths = (f: FakeFetch) => f.calls.map((r) => new URL(r.url).pathname).filter((p) => p.endsWith("/keys"));

function setup(list: FakeHandler, role: "viewer" | "developer" = "developer", dbs: BranchDatabaseView[] = [dbRow("web1", { main: true })]) {
  const caps = projectCaps(me(role), "p1");
  const f = fakeFetch({
    "GET /v1/databases/db/branches": list,
    "GET /v1/me": () => jsonResponse(me(role)),
    [`${branchRoute("id-main")}/keys`]: () => jsonResponse([key("public", "pub-main", "MAIN"), key("secret", "sec-main", "main-secret")]),
    [`${branchRoute("id-dev")}/keys`]: () => jsonResponse([key("public", "pub-dev", "DEV"), key("secret", "sec-dev", "dev-secret")]),
    [`${branchRoute("id-main")}/databases`]: () => jsonResponse(dbs),
    [`${branchRoute("id-dev")}/databases`]: () => jsonResponse(dbs),
    [`${branchRoute("id-main")}/data-api/schema`]: () => jsonResponse(schema),
    [`${branchRoute("id-dev")}/data-api/schema`]: () => jsonResponse(schema),
  });
  const root = createRootRoute();
  const home = createRoute({
    getParentRoute: () => root,
    path: "/",
    component: () => (
      <ProjectCapsContext.Provider value={caps}>
        <ConnectPanel apiUrl="http://gw" database={summary} />
      </ProjectCapsContext.Provider>
    ),
  });
  const member = createRoute({ getParentRoute: () => root, path: "/app/members/$memberId", component: () => <p>member page</p> });
  const router = createRouter({ routeTree: root.addChildren([home, member]), history: createMemoryHistory({ initialEntries: ["/"] }) });
  renderWithQuery(<RouterProvider router={router} />, { fetch: f });
  return f;
}

const threeBranches = () => jsonResponse([b("dev"), b("main"), b("bak", { kind: "backup" }), b("busy", { status: "provisioning" })]);

describe("ConnectPanel", () => {
  it("shows main's endpoint and keys, and swaps them when another branch is picked", async () => {
    const f = setup(threeBranches);
    expect(await screen.findByRole("heading", { name: "Connect to web1" })).toBeInTheDocument();
    expect(await screen.findByText("chk_pub_MAIN")).toBeInTheDocument();
    expect(await screen.findByText("main-secret")).toBeInTheDocument();
    expect(screen.getByText("http://gw/data/v1/query")).toBeInTheDocument();
    const picker = screen.getByLabelText("Branch") as HTMLSelectElement;
    expect(Array.from(picker.options).map((o) => o.text)).toEqual(["dev", "main"]);
    expect(picker).toHaveValue("id-main");
    await userEvent.selectOptions(picker, "dev");
    expect(await screen.findByText("chk_pub_DEV")).toBeInTheDocument();
    expect(await screen.findByText("dev-secret")).toBeInTheDocument();
    expect(screen.queryByText("main-secret")).not.toBeInTheDocument();
    await waitFor(() => expect(keyPaths(f).at(-1)).toBe("/v1/databases/db/branches/id-dev/keys"));
  });

  it("follows the Postgres database picked in the Data API column", async () => {
    const f = setup(threeBranches, "developer", [dbRow("web1", { main: true }), dbRow("app")]);
    const picker = (await screen.findByLabelText("Postgres DB")) as HTMLSelectElement;
    expect(picker).toHaveValue("web1");
    await userEvent.selectOptions(picker, "app");
    await waitFor(() => {
      const schemas = f.calls.map((r) => new URL(r.url)).filter((u) => u.pathname.endsWith("/data-api/schema"));
      expect(schemas.at(-1)?.searchParams.get("database")).toBe("app");
    });
    // One picker only: the Data API column no longer carries its own, and the types follow the header.
    expect(screen.getAllByLabelText(/^(Postgres DB|Database)$/)).toHaveLength(1);
    const types = screen.getByLabelText("Types database") as HTMLSelectElement;
    expect(types.options[types.selectedIndex].text).toBe("app");
    expect(screen.getByRole("button", { name: /Download types/i })).toBeInTheDocument();
  });

  it("hides every mutating control and the logins line from a Viewer", async () => {
    setup(threeBranches, "viewer");
    expect(await screen.findByText("chk_pub_MAIN")).toBeInTheDocument();
    await screen.findByText("main-secret");
    expect(screen.queryByRole("button", { name: "Rotate public key" })).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Create key" })).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: /Revoke/ })).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: /Download types/i })).not.toBeInTheDocument();
    expect(screen.queryByText(/personal Postgres login/)).not.toBeInTheDocument();
  });

  it("links a Developer to their own personal logins", async () => {
    setup(threeBranches);
    const link = await screen.findByRole("link", { name: /personal Postgres login/ });
    expect(link).toHaveAttribute("href", "/app/members/mem-1#personal-logins");
  });

  it("says when no branch is ready, and shows no keys", async () => {
    setup(() => jsonResponse([b("main", { status: "provisioning" })]));
    expect(await screen.findByText("No branch is ready yet.")).toBeInTheDocument();
    expect(screen.queryByText(/chk_/)).not.toBeInTheDocument();
  });

  it("shows a load error", async () => {
    setup(() => jsonResponse({ error: { code: "unavailable", message: "gateway down" } }, 503));
    expect(await screen.findByRole("alert")).toHaveTextContent("gateway down");
  });
});
