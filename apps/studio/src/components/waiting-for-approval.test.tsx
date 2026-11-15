import { screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import type { MeResponse as Me } from "@chelabase/management-client";
import { fakeFetch, jsonResponse, sequence } from "@/test/fake-fetch";
import { renderWithQuery } from "@/test/render";
import { PendingGate } from "./waiting-for-approval";

const pending: Me = {
  sub: "u9",
  email: "ed@example.test",
  name: "Ed",
  tenant_id: "t1",
  member: { id: "m-self", status: "pending", tenant_admin: false },
  instance_admin: false, functions_available: true, tenant_admins: [],
  projects: [],
  user: { sub: "u9" },
  tenant: { id: "t1", slug: "demo", display_name: "Acme", projects: [] },
};

describe("PendingGate", () => {
  it("waiting_for_approval_screen", async () => {
    const onSignOut = vi.fn();
    const f = fakeFetch({ "GET /v1/me": () => jsonResponse(pending) });
    const { queryClient } = renderWithQuery(
      <PendingGate onSignOut={onSignOut}>
        <p>the app</p>
      </PendingGate>,
      { fetch: f },
    );
    expect(await screen.findByRole("heading", { name: "Waiting for approval" })).toHaveFocus();
    expect(screen.queryByText("the app")).toBeNull();
    expect(
      screen.getByText(
        "You're signed in as ed@example.test. A tenant admin of Acme needs to approve your account before you can use Chelabase Studio.",
      ),
    ).toBeInTheDocument();
    await userEvent.click(screen.getByRole("button", { name: "Check now" }));
    expect(f.calls.filter((r) => new URL(r.url).pathname === "/v1/me").length).toBeGreaterThanOrEqual(2);
    await userEvent.click(screen.getByRole("button", { name: "Sign out" }));
    expect(onSignOut).toHaveBeenCalledTimes(1);
    expect(queryClient.getQueryCache().getAll()).toHaveLength(0);
  });

  // M6: a removed member's `me` says so; the whole app gives way to one screen, with a way out.
  it("a removed member gets one screen and a sign-out, not the app", async () => {
    const onSignOut = vi.fn();
    const f = fakeFetch({
      "GET /v1/me": () => jsonResponse({ ...pending, member: { id: "m-self", status: "removed", tenant_admin: false } }),
    });
    const { queryClient } = renderWithQuery(
      <PendingGate onSignOut={onSignOut}>
        <p>the app</p>
      </PendingGate>,
      { fetch: f },
    );
    expect(await screen.findByRole("heading", { name: "You were removed from this tenant" })).toHaveFocus();
    expect(screen.queryByText("the app")).toBeNull();
    expect(screen.getByText(/ed@example\.test/)).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Check now" })).toBeNull();
    await userEvent.click(screen.getByRole("button", { name: "Sign out" }));
    expect(onSignOut).toHaveBeenCalledTimes(1);
    expect(queryClient.getQueryCache().getAll()).toHaveLength(0);
  });

  // R49: an invited member whose invite expired is signed in but not a member yet: one screen
  // that says so, whom to ask, and a way out; never the app (every call would refuse them).
  it("an expired invite gets its own screen with the tenant admins and a sign-out", async () => {
    const onSignOut = vi.fn();
    const f = fakeFetch({
      "GET /v1/me": () =>
        jsonResponse({
          ...pending,
          member: { id: "m-self", status: "invited", tenant_admin: false },
          tenant_admins: [
            { name: "Ana", email: "ana@example.test" },
            { name: "", email: null },
          ],
        }),
    });
    renderWithQuery(
      <PendingGate onSignOut={onSignOut}>
        <p>the app</p>
      </PendingGate>,
      { fetch: f },
    );
    expect(await screen.findByRole("heading", { name: "Your invite expired" })).toHaveFocus();
    expect(screen.queryByText("the app")).toBeNull();
    expect(screen.getByText("Your invite to Acme expired. Ask an admin to send a new one.")).toBeInTheDocument();
    const admins = screen.getByRole("list", { name: "Tenant admins" });
    expect(admins).toHaveTextContent("Ana");
    expect(admins).toHaveTextContent("ana@example.test");
    expect(admins).toHaveTextContent("Unnamed admin");
    expect(admins).toHaveTextContent("Email unknown");
    await userEvent.click(screen.getByRole("button", { name: "Sign out" }));
    expect(onSignOut).toHaveBeenCalledTimes(1);
  });

  it("renders the app for anyone not pending", async () => {
    const f = fakeFetch({ "GET /v1/me": () => jsonResponse({ ...pending, member: { id: "m-self", status: "active", tenant_admin: false } }) });
    renderWithQuery(
      <PendingGate onSignOut={() => {}}>
        <p>the app</p>
      </PendingGate>,
      { fetch: f },
    );
    expect(await screen.findByText("the app")).toBeInTheDocument();
  });

  it("a failed me shows an error with Retry, not the app", async () => {
    const f = fakeFetch({
      "GET /v1/me": sequence(
        () => jsonResponse({ error: { code: "unavailable", message: "down" } }, 503),
        () => jsonResponse({ ...pending, member: { id: "m-self", status: "active", tenant_admin: false } }),
      ),
    });
    renderWithQuery(
      <PendingGate onSignOut={() => {}}>
        <p>the app</p>
      </PendingGate>,
      { fetch: f },
    );
    expect(await screen.findByRole("alert")).toHaveTextContent("Couldn't load your account: down");
    expect(screen.queryByText("the app")).toBeNull();
    await userEvent.click(screen.getByRole("button", { name: "Retry" }));
    expect(await screen.findByText("the app")).toBeInTheDocument();
  });

  it("shows nothing of the app while me loads", () => {
    const f = fakeFetch({ "GET /v1/me": () => new Promise<Response>(() => {}) });
    renderWithQuery(
      <PendingGate onSignOut={() => {}}>
        <p>the app</p>
      </PendingGate>,
      { fetch: f },
    );
    expect(screen.queryByText("the app")).toBeNull();
    expect(screen.getByText("Loading your account…")).toBeInTheDocument();
  });
});
