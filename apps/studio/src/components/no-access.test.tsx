import { screen, within } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import type { MeResponse as Me, Member } from "@chelabase/management-client";
import { fakeFetch, jsonResponse } from "@/test/fake-fetch";
import { renderWithRouter } from "@/test/render-router";
import { NoAccess } from "./no-access";

const me: Me = {
  sub: "u2",
  email: "bo@example.test",
  name: "Bo",
  tenant_id: "t1",
  member: { id: "m-self", status: "active", tenant_admin: false },
  instance_admin: false, functions_available: true, tenant_admins: [],
  projects: [],
  user: { sub: "u2" },
  tenant: { id: "t1", slug: "demo", display_name: null, projects: [] },
};

function member(id: string, extra: Partial<Member>): Member {
  return {
    id, email: `${id}@example.test`, display_name: id, status: "active", tenant_admin: false, has_access: true,
    invite: null, accepted_at: null, suspended_at: null, suspended_by: null, created_at: "2026-10-01T00:00:00Z", access: { level: "none", projects: [] }, ...extra,
  };
}

describe("NoAccess", () => {
  // M6: a signed-in person of the org with no member row (sign-up closed, no invite) gets this
  // screen too; the tenant admins' list is a members route they can't read, so it isn't asked.
  it("a signed-in org user with no member row sees the screen without the admin list", async () => {
    const f = fakeFetch({ "GET /v1/members": () => jsonResponse({ error: { code: "no_access_yet", message: "no" } }, 403) });
    renderWithRouter(<NoAccess me={{ ...me, member: null }} checking={false} onCheck={() => {}} />, { fetch: f });
    expect(await screen.findByRole("heading", { name: "You don't have access to a project yet" })).toHaveFocus();
    expect(screen.getByText(/You're signed in as bo@example.test, but you aren't a member of this tenant/)).toBeInTheDocument();
    expect(screen.queryByRole("list", { name: "Tenant admins" })).toBeNull();
    expect(screen.queryByRole("alert")).toBeNull();
    expect(f.calls.some((r) => new URL(r.url).pathname === "/v1/members")).toBe(false);
  });

  it("names_tenant_admins", async () => {
    const f = fakeFetch({
      "GET /v1/members": () =>
        jsonResponse([
          member("ana", { display_name: "Ana", tenant_admin: true }),
          member("cy", { display_name: "Cy", tenant_admin: true, email: null }),
          member("bo", { display_name: "Bo" }),
          member("di", { display_name: "Di", tenant_admin: true, status: "invited" }),
        ]),
    });
    renderWithRouter(<NoAccess me={me} checking={false} onCheck={() => {}} />, { fetch: f });
    expect(await screen.findByRole("heading", { name: "You don't have access to a project yet" })).toHaveFocus();
    expect(screen.getByText(/You're signed in as bo@example.test\./)).toBeInTheDocument();
    const list = await screen.findByRole("list", { name: "Tenant admins" });
    const items = within(list).getAllByRole("listitem");
    expect(items).toHaveLength(2);
    expect(items[0]).toHaveTextContent("Ana");
    expect(items[0]).toHaveTextContent("ana@example.test");
    expect(within(items[0]).getByRole("button", { name: "Copy ana@example.test" })).toBeInTheDocument();
    expect(items[1]).toHaveTextContent("Email unknown");
    expect(screen.getByRole("link", { name: "Members" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Check now" })).toBeInTheDocument();
  });
});
