import { ApiError } from "@chelabase/management-client";
import { describe, expect, it, vi } from "vitest";
import { configureLoginClient } from "./login-client/fetch";
import { fakeFetch, jsonResponse } from "@/test/fake-fetch";
import { signOut } from "./sign-out";

function parts() {
  const order: string[] = [];
  const session = { clear: vi.fn(() => void order.push("clear")) };
  const disconnect = vi.fn(() => void order.push("disconnect"));
  const leave = vi.fn(() => void order.push("leave"));
  return { order, session, disconnect, leave };
}

describe("signOut", () => {
  it("clears_and_calls_logout", async () => {
    const p = parts();
    const f = fakeFetch({ "POST /auth/v1/logout": () => (p.order.push("logout"), jsonResponse({})) });
    configureLoginClient({ baseUrl: "http://studio.test", fetchImpl: f });
    await signOut(p.session, { disconnect: p.disconnect, leave: p.leave });
    expect(p.order).toEqual(["logout", "clear", "disconnect", "leave"]);
    expect(f.calls[0]!.credentials).toBe("include");
  });

  it("an_outage_keeps_the_session_because_the_cookie_is_still_valid", async () => {
    const p = parts();
    const f = fakeFetch({ "POST /auth/v1/logout": () => jsonResponse({ error: { code: "unavailable", message: "down" } }, 503) });
    configureLoginClient({ baseUrl: "http://studio.test", fetchImpl: f });
    await expect(signOut(p.session, { disconnect: p.disconnect, leave: p.leave })).rejects.toBeInstanceOf(ApiError);
    expect(p.session.clear).not.toHaveBeenCalled();
    expect(p.leave).not.toHaveBeenCalled();
  });

  it("an_unreachable_auth_is_also_not_a_sign_out", async () => {
    const p = parts();
    const f = fakeFetch({ "POST /auth/v1/logout": () => { throw new TypeError("down"); } });
    configureLoginClient({ baseUrl: "http://studio.test", fetchImpl: f });
    await expect(signOut(p.session, { disconnect: p.disconnect, leave: p.leave })).rejects.toBeDefined();
    expect(p.session.clear).not.toHaveBeenCalled();
  });

  it("a_logout_that_finds_no_session_still_signs_out_locally", async () => {
    const p = parts();
    const f = fakeFetch({ "POST /auth/v1/logout": () => jsonResponse({ error: { code: "unauthorized", message: "no" } }, 401) });
    configureLoginClient({ baseUrl: "http://studio.test", fetchImpl: f });
    await signOut(p.session, { disconnect: p.disconnect, leave: p.leave });
    expect(p.session.clear).toHaveBeenCalled();
    expect(p.leave).toHaveBeenCalled();
  });
});
