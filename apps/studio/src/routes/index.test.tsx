import { screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { ApiError } from "@chelabase/management-client";
import { createSession } from "@/lib/session";
import { SessionProvider } from "@/lib/session-context";
import { renderWithRouter } from "@/test/render-router";
import { Route, validateSearch } from "./index";

function landing(mint: () => Promise<{ access_token: string; expires_in: number }>, search: { signup?: true } = {}) {
  vi.spyOn(Route, "useSearch").mockReturnValue(search);
  const session = createSession({ mint });
  const Component = Route.options.component!;
  renderWithRouter(
    <SessionProvider session={session}>
      <Component />
    </SessionProvider>,
  );
  return session;
}
const signedOut = async () => {
  throw new ApiError(401, "unauthorized", "no session");
};

describe("route /", () => {
  it("validate_search_reads_bare_signup", () => {
    expect(validateSearch({ signup: "" })).toEqual({ signup: true });
    expect(validateSearch({ signup: true })).toEqual({ signup: true });
    expect(validateSearch({})).toEqual({});
    expect(validateSearch({ other: "x" })).toEqual({});
  });

  it("a_cookie_session_goes_straight_to_the_app_even_with_signup", async () => {
    landing(async () => ({ access_token: "t", expires_in: 300 }), { signup: true });
    expect(await screen.findByText("dashboard route")).toBeInTheDocument();
  });

  it("signed_out_shows_the_landing_page", async () => {
    landing(signedOut);
    expect(await screen.findByRole("link", { name: "Sign in" })).toBeInTheDocument();
  });

  it("an_unreachable_auth_still_shows_the_landing_page", async () => {
    landing(async () => {
      throw new ApiError(0, "network", "down");
    });
    expect(await screen.findByRole("link", { name: "Sign in" })).toBeInTheDocument();
  });
});
