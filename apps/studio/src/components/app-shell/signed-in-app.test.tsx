import { screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import type { MeResponse as Me } from "@chelabase/management-client";
import { fakeFetch, jsonResponse } from "@/test/fake-fetch";
import { renderWithRouter } from "@/test/render-router";
import { SignedInApp } from "./signed-in-app";

const me = (status: "active" | "pending", tenant_admin: boolean): Me => ({
  sub: "u1", email: "a@example.test", name: "A", tenant_id: "t1", instance_admin: false, tenant_admins: [],
  member: { id: "m1", status, tenant_admin }, projects: [], user: { sub: "u1" },
  tenant: { id: "t1", slug: "demo", display_name: null, projects: [] },
});
const pendingMember = {
  id: "p1", email: "p@example.test", display_name: "P", status: "pending", tenant_admin: false, has_access: false,
  invite: null, accepted_at: null, created_at: "2026-10-01T00:00:00Z",
};

describe("SignedInApp (the /app layout's wiring)", () => {
  it("a pending caller gets the waiting screen instead of the shell", async () => {
    const f = fakeFetch({ "GET /v1/me": () => jsonResponse(me("pending", false)) });
    renderWithRouter(<SignedInApp profile={{}} onSignOut={() => {}}><p>page</p></SignedInApp>, { fetch: f });
    expect(await screen.findByRole("heading", { name: "Waiting for approval" })).toBeInTheDocument();
    expect(screen.queryByRole("navigation", { name: "Studio" })).toBeNull();
    expect(screen.queryByText("page")).toBeNull();
  });

  it("a tenant admin gets the shell with the requests badge on Members", async () => {
    const f = fakeFetch({
      "GET /v1/me": () => jsonResponse(me("active", true)),
      "GET /v1/members": () => jsonResponse([pendingMember]),
    });
    renderWithRouter(<SignedInApp profile={{}} onSignOut={() => {}}><p>page</p></SignedInApp>, { fetch: f });
    expect(await screen.findByText("page")).toBeInTheDocument();
    expect(await screen.findByRole("link", { name: "Members, 1 request waiting" })).toBeInTheDocument();
  });
});
