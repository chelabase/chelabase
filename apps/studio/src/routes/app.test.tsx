import { screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { MeResponse as Me } from "@chelabase/management-client";
import { EnvContext } from "@/lib/env-context";
import { createSession } from "@/lib/session";
import { SessionProvider } from "@/lib/session-context";
import { fakeFetch, jsonResponse, type FakeHandler } from "@/test/fake-fetch";
import { renderWithRouter } from "@/test/render-router";
import { Route } from "./app";

const me: Me = {
  sub: "u1", email: "ada@example.test", name: "Ada", tenant_id: "t1", instance_admin: false, functions_available: true, tenant_admins: [],
  member: { id: "m1", status: "active", tenant_admin: false }, projects: [], user: { sub: "u1" },
  tenant: { id: "t1", slug: "demo", display_name: null, projects: [] },
};

async function renderApp(routes: Record<string, FakeHandler>) {
  const f = fakeFetch(routes);
  vi.stubGlobal("fetch", f);
  const session = createSession({ mint: async () => ({ access_token: "t1", expires_in: 300 }) });
  await session.start();
  const App = Route.options.component!;
  renderWithRouter(
    <SessionProvider session={session}>
      <EnvContext.Provider value={{ apiUrl: "http://api.test", studioHost: "studio.test" }}>
        <App />
      </EnvContext.Provider>
    </SessionProvider>,
    { fetch: f },
  );
  return { f, session };
}

afterEach(() => vi.unstubAllGlobals());

describe("route /app", () => {
  it("a_token_with_no_membership_gets_the_no_access_screen", async () => {
    await renderApp({
      "GET /v1/me": () => jsonResponse({ error: { code: "forbidden", message: "not a member" } }, 403),
    });
    expect(await screen.findByRole("heading", { name: "No access to this workspace" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Sign out" })).toBeInTheDocument();
    expect(screen.queryByRole("navigation", { name: "Studio" })).toBeNull();
  });

  it("a_failed_logout_keeps_the_session_and_says_so", async () => {
    const { session } = await renderApp({
      "GET /v1/me": () => jsonResponse(me),
      "POST /auth/v1/logout": () => jsonResponse({ error: { code: "unavailable", message: "down" } }, 503),
    });
    await userEvent.click(await screen.findByRole("button", { name: "Ada" }));
    await userEvent.click(await screen.findByRole("menuitem", { name: "Sign out" }));
    expect(await screen.findByRole("alert")).toHaveTextContent("your session is still active");
    expect(session.token()).toBe("t1");
  });

  it("a_logout_that_works_drops_the_token", async () => {
    const { session } = await renderApp({
      "GET /v1/me": () => jsonResponse(me),
      "POST /auth/v1/logout": () => jsonResponse({}),
    });
    await userEvent.click(await screen.findByRole("button", { name: "Ada" }));
    await userEvent.click(await screen.findByRole("menuitem", { name: "Sign out" }));
    await vi.waitFor(() => expect(session.token()).toBeUndefined());
    expect(screen.queryByRole("alert")).toBeNull();
  });
});
