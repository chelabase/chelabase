import { screen } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { renderWithRouter } from "@/test/render-router";
import { Route, validateSearch } from "./index";

const auth = vi.hoisted(() => ({
  current: {} as Record<string, unknown>,
}));
vi.mock("react-oidc-context", () => ({ useAuth: () => auth.current }));

describe("route /", () => {
  beforeEach(() => {
    auth.current = { isAuthenticated: false, isLoading: false, error: undefined, signinRedirect: vi.fn() };
  });

  it("validate_search_reads_bare_signup", () => {
    expect(validateSearch({ signup: "" })).toEqual({ signup: true });
    expect(validateSearch({ signup: true })).toEqual({ signup: true });
    expect(validateSearch({})).toEqual({});
    expect(validateSearch({ other: "x" })).toEqual({});
  });

  it("signed_in_with_signup_goes_to_app", async () => {
    const signinRedirect = vi.fn();
    auth.current = { isAuthenticated: true, isLoading: false, error: undefined, signinRedirect };
    // The helper's router starts at "/" with no search, so feed `?signup` in directly.
    vi.spyOn(Route, "useSearch").mockReturnValue({ signup: true });
    const Component = Route.options.component!;
    renderWithRouter(<Component />, {});
    expect(await screen.findByText("dashboard route")).toBeInTheDocument();
    expect(signinRedirect).not.toHaveBeenCalled();
  });
});
