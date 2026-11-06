import { screen, within } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import { fakeFetch, jsonResponse } from "@/test/fake-fetch";
import { detailOf, devBranch, mainBranch, mergeOf } from "@/test/merge-fixtures";
import { renderWithCrumbs } from "@/test/render-crumbs";
import { MERGE_URL } from "@/test/merge-page-harness";
import { MergePage } from "./merge-page";

const me = {
  sub: "u1", email: "u@example.test", name: "U", tenant_id: "t1", member: { id: "m", status: "active", tenant_admin: true },
  instance_admin: true, tenant_admins: [], user: { sub: "u1" },
  projects: [{ project_id: "p1", platform_role: "admin", roles: "all", databases: [] }],
  tenant: {
    id: "t1", slug: "demo", display_name: null,
    projects: [{ id: "p1", slug: "x", name: "Acme shop", status: "active", databases: [{ database_id: "db", name: "orders-db", postgres_database: "o", status: "ready", host: null, port: null, branch_count: 2 }] }],
  },
};

describe("MergePage crumb labels", () => {
  it("shows the project and database names from me, and the loading mark before", async () => {
    const f = fakeFetch({
      "GET /v1/me": () => jsonResponse(me),
      [`GET ${MERGE_URL}`]: () => jsonResponse(detailOf(mergeOf())),
      "GET /v1/databases/db/branches": () => jsonResponse([mainBranch, devBranch]),
    });
    renderWithCrumbs(
      <MergePage projectId="p1" databaseId="db" mergeId="m1" createClient={() => Promise.reject(new Error("no worker"))} />,
      { path: "/app/projects/$projectId/databases/$databaseId/merges/$mergeId", at: "/app/projects/p1/databases/db/merges/m1" },
      { fetch: f },
    );
    const trail = await screen.findByRole("navigation", { name: "Breadcrumb" });
    expect(trail).toHaveTextContent("…");
    expect(await within(trail).findByText("Acme shop")).toBeInTheDocument();
    expect(within(trail).getByText("orders-db")).toBeInTheDocument();
    expect(within(trail).getByText("Merge")).toHaveAttribute("aria-current", "page");
  });
});
