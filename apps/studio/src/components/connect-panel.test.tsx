import { RouterProvider, createMemoryHistory, createRootRoute, createRoute, createRouter } from "@tanstack/react-router";
import { screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it, vi } from "vitest";
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

const me = (role: "viewer" | "developer" | "admin"): MeResponse =>
  ({
    email: "a@b.c",
    name: "Ada",
    instance_admin: false,
    member: { id: "mem-1", status: "active", tenant_admin: false },
    projects: [{ project_id: "p1", platform_role: role, roles: [], databases: [] }],
    tenant: null,
  }) as unknown as MeResponse;

const branchRoute = (id: string) => `GET /v1/databases/db/branches/${id}`;
const conn = (branch: string, pg: string | null) => ({
  host: `${branch}.host`, port: 4432, database: pg ?? "web1", user: "owner", password: `${branch}-${pg ?? "web1"}-pw`,
  uri: `postgresql://owner@${branch}.host:4432/${pg ?? "web1"}`, http_url: `https://${branch}.host/sql`, password_state: "generated", ca_file: null,
});
const connectionCalls = (f: FakeFetch) => f.calls.filter((r) => new URL(r.url).pathname.endsWith("/connection"));
const keyPaths = (f: FakeFetch) => f.calls.map((r) => new URL(r.url).pathname).filter((p) => p.endsWith("/keys"));

function setup(list: FakeHandler, role: "viewer" | "developer" | "admin" = "developer", dbs: BranchDatabaseView[] = [dbRow("web1", { main: true })]) {
  const caps = projectCaps(me(role), "p1");
  const f = fakeFetch({
    "GET /v1/databases/db/branches": list,
    "GET /v1/me": () => jsonResponse(me(role)),
    [`${branchRoute("id-main")}/keys`]: () => jsonResponse([key("public", "pub-main", "MAIN"), key("secret", "sec-main", "main-secret")]),
    [`${branchRoute("id-dev")}/keys`]: () => jsonResponse([key("public", "pub-dev", "DEV"), key("secret", "sec-dev", "dev-secret")]),
    [`${branchRoute("id-main")}/connection`]: (req) => jsonResponse(conn("main", new URL(req.url).searchParams.get("database"))),
    [`${branchRoute("id-dev")}/connection`]: (req) => jsonResponse(conn("dev", new URL(req.url).searchParams.get("database"))),
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
        <ConnectPanel apiUrl="http://gw" projectId="p1" database={summary} />
      </ProjectCapsContext.Provider>
    ),
  });
  const member = createRoute({ getParentRoute: () => root, path: "/app/members/$memberId", component: () => <p>member page</p> });
  const dataApi = createRoute({
    getParentRoute: () => root,
    path: "/app/data-api/{-$projectId}/{-$databaseId}/{-$branchId}/{-$pgDatabase}",
    component: () => <p>data api page</p>,
  });
  const router = createRouter({ routeTree: root.addChildren([home, member, dataApi]), history: createMemoryHistory({ initialEntries: ["/"] }) });
  renderWithQuery(<RouterProvider router={router} />, { fetch: f });
  return Object.assign(f, { router });
}

const threeBranches = () => jsonResponse([b("dev"), b("main"), b("bak", { kind: "backup" }), b("busy", { status: "provisioning" })]);

afterEach(() => vi.unstubAllGlobals());

describe("ConnectPanel", () => {
  it("shows the endpoint and public key with Copy, and only a link to manage them", async () => {
    const f = setup(threeBranches);
    expect(await screen.findByRole("heading", { name: "Connect to web1" })).toBeInTheDocument();
    expect(await screen.findByText("chk_pub_MAIN")).toBeInTheDocument();
    expect(screen.getByText("http://gw/data/v1/query")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Copy endpoint" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Copy public key" })).toBeInTheDocument();
    expect(screen.getByRole("link", { name: "Manage Data API" })).toBeInTheDocument();
    const picker = screen.getByLabelText("Branch") as HTMLSelectElement;
    expect(Array.from(picker.options).map((o) => o.text)).toEqual(["dev", "main"]);
    for (const name of [/Rotate/, /Create key/, /Revoke/, /Download types/i, /Enable data API/, /Disable data API/, /Edit databases/]) {
      expect(screen.queryByRole("button", { name })).not.toBeInTheDocument();
    }
    expect(screen.queryByText("main-secret")).not.toBeInTheDocument();
    expect(screen.queryByLabelText("Types database")).not.toBeInTheDocument();
    expect(screen.queryByText(/Public key databases/)).not.toBeInTheDocument();
    await userEvent.selectOptions(picker, "dev");
    expect(await screen.findByText("chk_pub_DEV")).toBeInTheDocument();
    await waitFor(() => expect(keyPaths(f).at(-1)).toBe("/v1/databases/db/branches/id-dev/keys"));
  });

  it("links Manage Data API to the picked branch and Postgres database", async () => {
    const f = setup(threeBranches, "developer", [dbRow("web1", { main: true }), dbRow("analytics")]);
    await userEvent.selectOptions(await screen.findByLabelText("Branch"), "dev");
    await userEvent.selectOptions(await screen.findByLabelText("Postgres DB"), "analytics");
    const link = await screen.findByRole("link", { name: "Manage Data API" });
    expect(link).toHaveAttribute("href", "/app/data-api/p1/db/id-dev/analytics");
    await userEvent.click(link);
    expect(await screen.findByText("data api page")).toBeInTheDocument();
    expect(f.router.state.location.pathname).toBe("/app/data-api/p1/db/id-dev/analytics");
  });

  it("says the Data API is off for a Postgres database it doesn't serve, next to the link", async () => {
    const f = setup(threeBranches, "developer", [dbRow("web1", { main: true }), dbRow("later", { data_api: "off" })]);
    await userEvent.selectOptions(await screen.findByLabelText("Postgres DB"), "later");
    expect(await screen.findByText(/Data API is off for later/)).toBeInTheDocument();
    expect(screen.getByRole("link", { name: "Manage Data API" })).toHaveAttribute("href", "/app/data-api/p1/db/id-main/later");
    expect(f.calls.some((r) => new URL(r.url).searchParams.get("database") === "later")).toBe(false);
  });

  it("gives a Viewer the endpoint and link but no logins line", async () => {
    setup(threeBranches, "viewer");
    expect(await screen.findByText("chk_pub_MAIN")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Rotate public key" })).not.toBeInTheDocument();
    expect(screen.getByRole("link", { name: "Manage Data API" })).toBeInTheDocument();
    expect(screen.queryByText(/personal Postgres login/)).not.toBeInTheDocument();
  });

  it("links a Developer to their own personal logins", async () => {
    setup(threeBranches);
    const link = await screen.findByRole("link", { name: /personal Postgres login/ });
    expect(link).toHaveAttribute("href", "/app/members/mem-1#personal-logins");
  });


  it("shows an Admin the owner connection of the picked branch, masked until Show, with the logins link", async () => {
    const writeText = vi.fn().mockResolvedValue(undefined);
    vi.stubGlobal("navigator", { ...navigator, clipboard: { writeText } });
    setup(threeBranches, "admin");
    expect(await screen.findByRole("heading", { name: "Postgres" })).toBeInTheDocument();
    expect(await screen.findByText("main.host")).toBeInTheDocument();
    expect(screen.getByText("4432")).toBeInTheDocument();
    expect(screen.getByText("owner", { selector: "dd" })).toBeInTheDocument();
    expect(screen.queryByText("main-web1-pw")).not.toBeInTheDocument();
    await userEvent.click(screen.getByRole("button", { name: "Show password" }));
    expect(screen.getByText("main-web1-pw")).toBeInTheDocument();
    await userEvent.click(screen.getByRole("button", { name: "Copy password" }));
    expect(writeText).toHaveBeenLastCalledWith("main-web1-pw");
    expect(screen.getByRole("link", { name: /personal Postgres login/ })).toBeInTheDocument();
  });

  it("re-fetches and re-masks the connection when the header's Branch or Postgres DB changes", async () => {
    const f = setup(threeBranches, "admin", [dbRow("web1", { main: true }), dbRow("app")]);
    await userEvent.click(await screen.findByRole("button", { name: "Show password" }));
    expect(screen.getByText("main-web1-pw")).toBeInTheDocument();

    await userEvent.selectOptions(screen.getByLabelText("Postgres DB"), "app");
    expect(await screen.findByText("app", { selector: "dd" })).toBeInTheDocument();
    expect(screen.queryByText("main-web1-pw")).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Show password" })).toBeInTheDocument();
    expect(connectionCalls(f).at(-1)!.url).toContain("/id-main/connection?database=app");

    await userEvent.click(screen.getByRole("button", { name: "Show password" }));
    await userEvent.selectOptions(screen.getByLabelText("Branch"), "dev");
    expect(await screen.findByText("dev.host")).toBeInTheDocument();
    expect(screen.queryByText("main-app-pw")).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Show password" })).toBeInTheDocument();
    expect(connectionCalls(f).at(-1)!.url).toContain("/id-dev/connection");
  });

  it("gives a Developer the logins link but no owner connection, and never asks for one", async () => {
    const f = setup(threeBranches, "developer");
    expect(await screen.findByRole("link", { name: /personal Postgres login/ })).toBeInTheDocument();
    await screen.findByText("chk_pub_MAIN");
    expect(screen.queryByText("main.host")).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Show password" })).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Copy password" })).not.toBeInTheDocument();
    expect(connectionCalls(f)).toHaveLength(0);
  });

  it("gives a Viewer neither the owner connection nor the logins link", async () => {
    const f = setup(threeBranches, "viewer");
    await screen.findByText("chk_pub_MAIN");
    expect(screen.queryByRole("heading", { name: "Postgres" })).not.toBeInTheDocument();
    expect(screen.queryByText("main.host")).not.toBeInTheDocument();
    expect(screen.queryByRole("link", { name: /personal Postgres login/ })).not.toBeInTheDocument();
    expect(connectionCalls(f)).toHaveLength(0);
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
