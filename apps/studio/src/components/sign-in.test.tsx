import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import { SignIn, type SignInProps } from "./sign-in";

function fakeAuth(over: Partial<SignInProps["auth"]> = {}): SignInProps["auth"] {
  return { isLoading: false, error: undefined, signinRedirect: vi.fn().mockResolvedValue(undefined), ...over };
}

const authError = (message: string): NonNullable<SignInProps["auth"]["error"]> =>
  Object.assign(new Error(message), { source: "unknown" as const });

describe("SignIn", () => {
  it("signup_redirects_to_create_once", () => {
    const auth = fakeAuth();
    render(<SignIn auth={auth} signup />);
    expect(auth.signinRedirect).toHaveBeenCalledTimes(1);
    // "replace": Back from Zitadel must not land on a page that redirects again
    expect(auth.signinRedirect).toHaveBeenCalledWith({ prompt: "create", redirectMethod: "replace" });
  });

  it("redirects_once_under_rerender", () => {
    const auth = fakeAuth();
    const { rerender } = render(<SignIn auth={auth} signup />);
    rerender(<SignIn auth={fakeAuth({ signinRedirect: auth.signinRedirect })} signup />);
    rerender(<SignIn auth={fakeAuth({ signinRedirect: auth.signinRedirect })} signup />);
    expect(auth.signinRedirect).toHaveBeenCalledTimes(1);
  });

  it("no_redirect_while_loading", () => {
    const signinRedirect = vi.fn().mockResolvedValue(undefined);
    const { rerender } = render(<SignIn auth={fakeAuth({ isLoading: true, signinRedirect })} signup />);
    expect(signinRedirect).not.toHaveBeenCalled();
    rerender(<SignIn auth={fakeAuth({ isLoading: false, signinRedirect })} signup />);
    expect(signinRedirect).toHaveBeenCalledTimes(1);
  });

  it("no_redirect_without_signup", () => {
    const auth = fakeAuth();
    render(<SignIn auth={auth} signup={false} />);
    expect(auth.signinRedirect).not.toHaveBeenCalled();
  });

  it("no_second_redirect_after_an_error", () => {
    const signinRedirect = vi.fn().mockResolvedValue(undefined);
    render(<SignIn auth={fakeAuth({ error: authError("zitadel down"), signinRedirect })} signup />);
    expect(signinRedirect).not.toHaveBeenCalled();
    expect(screen.getByRole("alert")).toHaveTextContent("Sign-in failed: zitadel down");
  });

  it("no_redirect_when_an_error_arrives_after_the_redirect", () => {
    const signinRedirect = vi.fn().mockResolvedValue(undefined);
    const { rerender } = render(<SignIn auth={fakeAuth({ signinRedirect })} signup />);
    rerender(<SignIn auth={fakeAuth({ error: authError("boom"), signinRedirect })} signup />);
    expect(signinRedirect).toHaveBeenCalledTimes(1);
    expect(screen.getByRole("alert")).toBeInTheDocument();
  });

  it("buttons_still_sign_in_and_create", async () => {
    const auth = fakeAuth();
    render(<SignIn auth={auth} signup={false} />);
    await userEvent.click(screen.getByRole("button", { name: "Sign in" }));
    expect(auth.signinRedirect).toHaveBeenLastCalledWith();
    await userEvent.click(screen.getByRole("button", { name: "Create account" }));
    expect(auth.signinRedirect).toHaveBeenLastCalledWith({ prompt: "create" });
    expect(auth.signinRedirect).toHaveBeenCalledTimes(2);
  });
});
