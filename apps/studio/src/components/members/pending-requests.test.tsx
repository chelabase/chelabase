import { screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import type { MeResponse as Me, Member } from "@chelabase/management-client";
import { fakeFetch, jsonResponse } from "@/test/fake-fetch";
import { renderWithQuery } from "@/test/render";
import { usePendingRequestCount } from "./pending-requests";

const me = (tenant_admin: boolean): Me => ({
  sub: "u1", email: "a@example.test", name: "A", tenant_id: "t1", member: { id: "m-self", status: "active", tenant_admin },
  instance_admin: false, tenant_admins: [], projects: [], user: { sub: "u1" },
  tenant: { id: "t1", slug: "demo", display_name: null, projects: [] },
});
const m = (id: string, status: Member["status"]): Member => ({
  id, email: `${id}@x.test`, display_name: id, status, tenant_admin: false, has_access: false, invite: null,
  accepted_at: null, suspended_at: null, suspended_by: null, created_at: "2026-10-01T00:00:00Z",
  access: { level: "none", projects: [] },
});

function Probe() {
  const n = usePendingRequestCount();
  return <p>count {n}</p>;
}

describe("usePendingRequestCount", () => {
  it("rail_badge_counts_pending: counts pending members for tenant admins only", async () => {
    const members = () => jsonResponse([m("a", "active"), m("b", "pending"), m("c", "pending"), m("d", "invited")]);
    const admin = fakeFetch({ "GET /v1/me": () => jsonResponse(me(true)), "GET /v1/members": members });
    const first = renderWithQuery(<Probe />, { fetch: admin });
    expect(await screen.findByText("count 2")).toBeInTheDocument();
    first.unmount();

    const plain = fakeFetch({ "GET /v1/me": () => jsonResponse(me(false)), "GET /v1/members": members });
    renderWithQuery(<Probe />, { fetch: plain });
    await screen.findByText("count 0");
    await new Promise((r) => setTimeout(r, 20));
    expect(plain.calls.some((r) => new URL(r.url).pathname === "/v1/members")).toBe(false);
  });

  // The browser pass: a sign-up made after the page loaded showed only after a reload.
  it("a new sign-up shows within 30 seconds, without a reload", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    try {
      let list = [m("a", "active"), m("b", "pending")];
      const f = fakeFetch({ "GET /v1/me": () => jsonResponse(me(true)), "GET /v1/members": () => jsonResponse(list) });
      renderWithQuery(<Probe />, { fetch: f });
      expect(await screen.findByText("count 1")).toBeInTheDocument();
      list = [...list, m("c", "pending")];
      await vi.advanceTimersByTimeAsync(30_000);
      expect(await screen.findByText("count 2")).toBeInTheDocument();
    } finally {
      vi.useRealTimers();
    }
  });
});
