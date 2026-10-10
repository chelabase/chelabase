import { screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import type { ReactElement } from "react";
import { describe, expect, it, vi } from "vitest";
import { configureLoginClient } from "@/lib/login-client/fetch";
import { fakeFetch, jsonResponse, type FakeHandler } from "@/test/fake-fetch";
import { renderWithQuery } from "@/test/render";
import { SignInForm } from "./sign-in-form";
import { SignUpForm } from "./sign-up-form";
import { VerifyEmailForm } from "./verify-email-form";

const DONE = { next: "done", redirect: "http://localhost:3000/callback?code=c" };
const EXPIRED_BODY = { error: { code: "sign_in_expired", message: "This sign-in has expired, start again" } };

/** Renders a login form against fake auth routes, with the login client pointed at them. */
function renderLogin(ui: ReactElement, routes: Record<string, FakeHandler> = {}) {
  const f = fakeFetch({ "POST /auth/v1/auth-request": () => jsonResponse({ signUp: false, signUpAllowed: false }), ...routes });
  configureLoginClient({ baseUrl: "http://studio.test", fetchImpl: f });
  return { ...renderWithQuery(ui, { fetch: f }), f };
}

const call = (f: { calls: Request[] }, path: string) => f.calls.find((r) => new URL(r.url).pathname === path);

describe("SignInForm", () => {
  it("signs in and follows auth's redirect", async () => {
    const go = vi.fn();
    const { f } = renderLogin(<SignInForm authRequest="V2_1" go={go} toVerify={vi.fn()} toRegister={vi.fn()} />, {
      "POST /auth/v1/sign-in": () => jsonResponse(DONE),
    });
    await userEvent.type(await screen.findByLabelText("Email"), "ada@example.com");
    await userEvent.type(screen.getByLabelText("Password"), "Secret-pass1");
    await userEvent.click(screen.getByRole("button", { name: "Sign in" }));
    await waitFor(() => expect(go).toHaveBeenCalledWith("http://localhost:3000/callback?code=c"));
    expect(await call(f, "/auth/v1/sign-in")!.clone().json()).toEqual({ authRequest: "V2_1", email: "ada@example.com", password: "Secret-pass1" });
    expect(call(f, "/auth/v1/sign-in")!.credentials).toBe("include");
  });

  it("shows auth's message on failure and stays put", async () => {
    const go = vi.fn();
    renderLogin(<SignInForm authRequest="V2_1" go={go} toVerify={vi.fn()} toRegister={vi.fn()} />, {
      "POST /auth/v1/sign-in": () => jsonResponse({ error: { code: "invalid_credentials", message: "Email or password is incorrect" } }, 401),
    });
    await userEvent.type(await screen.findByLabelText("Email"), "a@b.c");
    await userEvent.type(screen.getByLabelText("Password"), "x");
    await userEvent.click(screen.getByRole("button", { name: "Sign in" }));
    expect(await screen.findByRole("alert")).toHaveTextContent("Email or password is incorrect");
    expect(screen.queryByRole("link", { name: "Start again" })).toBeNull();
    expect(go).not.toHaveBeenCalled();
  });

  it("shows the unavailable fallback when auth can't be reached", async () => {
    renderLogin(<SignInForm authRequest="V2_1" go={vi.fn()} toVerify={vi.fn()} toRegister={vi.fn()} />, {
      "POST /auth/v1/sign-in": () => { throw new TypeError("Failed to fetch"); },
    });
    await userEvent.type(await screen.findByLabelText("Email"), "a@b.c");
    await userEvent.type(screen.getByLabelText("Password"), "x");
    await userEvent.click(screen.getByRole("button", { name: "Sign in" }));
    expect(await screen.findByRole("alert")).toHaveTextContent("Sign-in is unavailable, try again");
  });

  async function signInFailingWith(answer: FakeHandler) {
    renderLogin(<SignInForm authRequest="V2_1" go={vi.fn()} toVerify={vi.fn()} toRegister={vi.fn()} />, {
      "POST /auth/v1/sign-in": answer,
    });
    await userEvent.type(await screen.findByLabelText("Email"), "a@b.c");
    await userEvent.type(screen.getByLabelText("Password"), "x");
    await userEvent.click(screen.getByRole("button", { name: "Sign in" }));
    return screen.findByRole("alert");
  }

  it("shows the fallback for an HTML 502", async () => {
    const alert = await signInFailingWith(() => new Response("<html>Bad Gateway</html>", { status: 502, headers: { "Content-Type": "text/html" } }));
    expect(alert).toHaveTextContent("Sign-in is unavailable, try again");
  });

  it("shows the fallback for the gateway's unavailable answer", async () => {
    const alert = await signInFailingWith(() => jsonResponse({ error: { code: "unavailable", message: "upstream unavailable" } }, 503));
    expect(alert).toHaveTextContent("Sign-in is unavailable, try again");
  });

  it("shows the fallback for a timeout", async () => {
    const alert = await signInFailingWith(() => jsonResponse({ error: { code: "timeout", message: "upstream timed out" } }, 504));
    expect(alert).toHaveTextContent("Sign-in is unavailable, try again");
  });

  it("shows the fallback for a 2xx answer that isn't JSON", async () => {
    const alert = await signInFailingWith(() => new Response("not json", { status: 200 }));
    expect(alert).toHaveTextContent("Sign-in is unavailable, try again");
  });

  it("rate_limited_shows_auths_message", async () => {
    const alert = await signInFailingWith(() => jsonResponse({ error: { code: "rate_limited", message: "Too many attempts, wait a minute" } }, 429));
    expect(alert).toHaveTextContent("Too many attempts, wait a minute");
    expect(screen.queryByRole("link", { name: "Start again" })).toBeNull();
  });

  it("keeps auth's own message for invalid_credentials", async () => {
    const alert = await signInFailingWith(() => jsonResponse({ error: { code: "invalid_credentials", message: "Email or password is incorrect" } }, 401));
    expect(alert).toHaveTextContent("Email or password is incorrect");
  });

  it("forwards to sign-up when the pending sign-in asked for it", async () => {
    const toRegister = vi.fn();
    const { f } = renderLogin(<SignInForm authRequest="V2_1" go={vi.fn()} toVerify={vi.fn()} toRegister={toRegister} />, {
      "POST /auth/v1/auth-request": () => jsonResponse({ signUp: true, signUpAllowed: true }),
    });
    await waitFor(() => expect(toRegister).toHaveBeenCalled());
    expect(await call(f, "/auth/v1/auth-request")!.clone().json()).toEqual({ authRequest: "V2_1" });
  });

  it("closed_sign_up_offers_no_account_creation_only_the_invite_line", async () => {
    const toRegister = vi.fn();
    renderLogin(<SignInForm authRequest="V2_1" go={vi.fn()} toVerify={vi.fn()} toRegister={toRegister} />);
    expect(await screen.findByText("No account? Ask your admin for an invite.")).toBeInTheDocument();
    await new Promise((r) => setTimeout(r, 20));
    expect(screen.queryByRole("button", { name: "Create account" })).toBeNull();
    expect(toRegister).not.toHaveBeenCalled();
  });

  it("sign_in_hides_create_account_when_closed", async () => {
    // The pending sign-in asked for account creation (prompt=create), but sign-up is closed: stay on
    // sign-in, offer no "Create account", and keep the generic error plus the invite line.
    const toRegister = vi.fn();
    renderLogin(<SignInForm authRequest="V2_1" go={vi.fn()} toVerify={vi.fn()} toRegister={toRegister} />, {
      "POST /auth/v1/auth-request": () => jsonResponse({ signUp: true, signUpAllowed: false }),
      "POST /auth/v1/sign-in": () =>
        jsonResponse({ error: { code: "invalid_credentials", message: "Email or password is incorrect" } }, 401),
    });
    expect(await screen.findByText("No account? Ask your admin for an invite.")).toBeInTheDocument();
    await userEvent.type(screen.getByLabelText("Email"), "nobody@example.com");
    await userEvent.type(screen.getByLabelText("Password"), "Secret-pass1");
    await userEvent.click(screen.getByRole("button", { name: "Sign in" }));
    expect(await screen.findByRole("alert")).toHaveTextContent("Email or password is incorrect");
    expect(screen.getByText("No account? Ask your admin for an invite.")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Create account" })).toBeNull();
    expect(toRegister).not.toHaveBeenCalled();
  });

  it("open_sign_up_offers_account_creation", async () => {
    const toRegister = vi.fn();
    renderLogin(<SignInForm authRequest="V2_1" go={vi.fn()} toVerify={vi.fn()} toRegister={toRegister} />, {
      "POST /auth/v1/auth-request": () => jsonResponse({ signUp: false, signUpAllowed: true }),
    });
    await userEvent.click(await screen.findByRole("button", { name: "Create account" }));
    expect(toRegister).toHaveBeenCalledTimes(1);
    expect(screen.queryByText("No account? Ask your admin for an invite.")).toBeNull();
  });

  it("goes to the code step when the email isn't verified", async () => {
    const toVerify = vi.fn();
    renderLogin(<SignInForm authRequest="V2_1" go={vi.fn()} toVerify={toVerify} toRegister={vi.fn()} />, {
      "POST /auth/v1/sign-in": () => jsonResponse({ next: "verify" }),
    });
    await userEvent.type(await screen.findByLabelText("Email"), "a@b.c");
    await userEvent.type(screen.getByLabelText("Password"), "x");
    await userEvent.click(screen.getByRole("button", { name: "Sign in" }));
    await waitFor(() => expect(toVerify).toHaveBeenCalled());
  });

  it("offers to start again when the sign-in expired", async () => {
    renderLogin(<SignInForm authRequest="V2_1" go={vi.fn()} toVerify={vi.fn()} toRegister={vi.fn()} />, {
      "POST /auth/v1/auth-request": () => jsonResponse(EXPIRED_BODY, 410),
    });
    expect(await screen.findByRole("alert")).toHaveTextContent("This sign-in has expired, start again");
    expect(screen.getByRole("link", { name: "Start again" })).toHaveAttribute("href", "/");
  });

  it("sign_in_expired_shows_restart_by_code", async () => {
    renderLogin(<SignInForm authRequest="V2_1" go={vi.fn()} toVerify={vi.fn()} toRegister={vi.fn()} />, {
      "POST /auth/v1/sign-in": () => jsonResponse({ error: { code: "sign_in_expired", message: "Too late, begin anew" } }, 410),
    });
    await userEvent.type(await screen.findByLabelText("Email"), "a@b.c");
    await userEvent.type(screen.getByLabelText("Password"), "x");
    await userEvent.click(screen.getByRole("button", { name: "Sign in" }));
    expect(await screen.findByRole("alert")).toHaveTextContent("Too late, begin anew");
    expect(screen.getByRole("link", { name: "Start again" })).toHaveAttribute("href", "/");
  });

  it("doesn't offer a restart for the expired wording under another code", async () => {
    renderLogin(<SignInForm authRequest="V2_1" go={vi.fn()} toVerify={vi.fn()} toRegister={vi.fn()} />, {
      "POST /auth/v1/sign-in": () => jsonResponse({ error: { code: "invalid_request", message: "This sign-in has expired, start again" } }, 400),
    });
    await userEvent.type(await screen.findByLabelText("Email"), "a@b.c");
    await userEvent.type(screen.getByLabelText("Password"), "x");
    await userEvent.click(screen.getByRole("button", { name: "Sign in" }));
    expect(await screen.findByRole("alert")).toHaveTextContent("This sign-in has expired, start again");
    expect(screen.queryByRole("link", { name: "Start again" })).toBeNull();
  });

  it("needs an authRequest", () => {
    renderLogin(<SignInForm authRequest="" go={vi.fn()} toVerify={vi.fn()} toRegister={vi.fn()} />);
    expect(screen.getByRole("alert")).toHaveTextContent("This sign-in has expired, start again");
    expect(screen.getByRole("link", { name: "Start again" })).toHaveAttribute("href", "/");
  });

  it("password_never_cached", async () => {
    const go = vi.fn();
    const { queryClient } = renderLogin(<SignInForm authRequest="V2_1" go={go} toVerify={vi.fn()} toRegister={vi.fn()} />, {
      "POST /auth/v1/sign-in": () => jsonResponse(DONE),
    });
    await userEvent.type(await screen.findByLabelText("Email"), "ada@example.com");
    await userEvent.type(screen.getByLabelText("Password"), "Secret-pass1");
    await userEvent.click(screen.getByRole("button", { name: "Sign in" }));
    await waitFor(() => expect(go).toHaveBeenCalled());
    await waitFor(() => {
      const held = JSON.stringify(queryClient.getMutationCache().getAll().map((m) => m.state));
      expect(held).not.toContain("Secret-pass1");
    });
    expect(JSON.stringify(queryClient.getQueryCache().getAll().map((q) => q.state))).not.toContain("Secret-pass1");
  });

  it("doesn't cache the password after a failed sign-in either", async () => {
    const { queryClient } = renderLogin(<SignInForm authRequest="V2_1" go={vi.fn()} toVerify={vi.fn()} toRegister={vi.fn()} />, {
      "POST /auth/v1/sign-in": () => jsonResponse({ error: { code: "invalid_credentials", message: "Email or password is incorrect" } }, 401),
    });
    await userEvent.type(await screen.findByLabelText("Email"), "a@b.c");
    await userEvent.type(screen.getByLabelText("Password"), "Secret-pass1");
    await userEvent.click(screen.getByRole("button", { name: "Sign in" }));
    expect(await screen.findByRole("alert")).toHaveTextContent("Email or password is incorrect");
    await waitFor(() => {
      expect(JSON.stringify(queryClient.getMutationCache().getAll().map((m) => m.state))).not.toContain("Secret-pass1");
    });
  });
});

async function fillSignUp(password = "Secret-pass1") {
  await userEvent.type(screen.getByLabelText("First name"), "Ada");
  await userEvent.type(screen.getByLabelText("Last name"), "Lovelace");
  await userEvent.type(screen.getByLabelText("Email"), "ada@example.com");
  await userEvent.type(screen.getByLabelText("Password"), password);
  await userEvent.click(screen.getByRole("button", { name: "Create account" }));
}

describe("SignUpForm", () => {
  it("creates the account and goes to the code step", async () => {
    const toVerify = vi.fn();
    const { f, queryClient } = renderLogin(<SignUpForm authRequest="V2_1" go={vi.fn()} toVerify={toVerify} />, {
      "POST /auth/v1/sign-up": () => jsonResponse({ next: "verify" }),
    });
    await fillSignUp();
    await waitFor(() => expect(toVerify).toHaveBeenCalled());
    expect(await call(f, "/auth/v1/sign-up")!.clone().json()).toEqual({
      authRequest: "V2_1", email: "ada@example.com", password: "Secret-pass1", givenName: "Ada", familyName: "Lovelace",
    });
    await waitFor(() => {
      expect(JSON.stringify(queryClient.getMutationCache().getAll().map((m) => m.state))).not.toContain("Secret-pass1");
    });
  });

  it("offers to start again when sign-up finds the sign-in expired", async () => {
    renderLogin(<SignUpForm authRequest="V2_1" go={vi.fn()} toVerify={vi.fn()} />, {
      "POST /auth/v1/sign-up": () => jsonResponse(EXPIRED_BODY, 410),
    });
    await fillSignUp();
    expect(await screen.findByRole("alert")).toHaveTextContent("This sign-in has expired, start again");
    expect(screen.getByRole("link", { name: "Start again" })).toHaveAttribute("href", "/");
  });

  it("shows a weak-password message without a restart link", async () => {
    renderLogin(<SignUpForm authRequest="V2_1" go={vi.fn()} toVerify={vi.fn()} />, {
      "POST /auth/v1/sign-up": () => jsonResponse({ error: { code: "weak_password", message: "Password is too weak" } }, 400),
    });
    await fillSignUp("x");
    expect(await screen.findByRole("alert")).toHaveTextContent("Password is too weak");
    expect(screen.queryByRole("link", { name: "Start again" })).toBeNull();
  });
});

describe("VerifyEmailForm", () => {
  it("prefills the code from the email link, verifies and follows the redirect", async () => {
    const go = vi.fn();
    const { f } = renderLogin(<VerifyEmailForm initialCode="123456" go={go} toVerify={vi.fn()} />, {
      "POST /auth/v1/verify-email": () => jsonResponse(DONE),
    });
    expect(screen.getByLabelText("Code")).toHaveValue("123456");
    await userEvent.click(screen.getByRole("button", { name: "Verify" }));
    await waitFor(() => expect(go).toHaveBeenCalledWith("http://localhost:3000/callback?code=c"));
    expect(await call(f, "/auth/v1/verify-email")!.clone().json()).toEqual({ code: "123456" });
  });

  it("resends the code", async () => {
    const { f } = renderLogin(<VerifyEmailForm go={vi.fn()} toVerify={vi.fn()} />, {
      "POST /auth/v1/resend-code": () => jsonResponse({}),
    });
    await userEvent.click(screen.getByRole("button", { name: "Send a new code" }));
    expect(await screen.findByText("If your account needs it, a new code is on its way.")).toBeInTheDocument();
    expect(call(f, "/auth/v1/resend-code")).toBeDefined();
  });

  it("says the same thing when resending fails", async () => {
    renderLogin(<VerifyEmailForm go={vi.fn()} toVerify={vi.fn()} />, {
      "POST /auth/v1/resend-code": () => jsonResponse({ error: { code: "rate_limited", message: "Slow down" } }, 429),
    });
    await userEvent.click(screen.getByRole("button", { name: "Send a new code" }));
    expect(await screen.findByText("If your account needs it, a new code is on its way.")).toBeInTheDocument();
  });

  it("sign_in_expired_on_verify_shows_restart", async () => {
    renderLogin(<VerifyEmailForm initialCode="123456" go={vi.fn()} toVerify={vi.fn()} />, {
      "POST /auth/v1/verify-email": () => jsonResponse({ error: { code: "sign_in_expired", message: "Too late, begin anew" } }, 410),
    });
    await userEvent.click(screen.getByRole("button", { name: "Verify" }));
    expect(await screen.findByRole("alert")).toHaveTextContent("Too late, begin anew");
    expect(screen.getByRole("link", { name: "Start again" })).toHaveAttribute("href", "/");
  });

  it("shows auth's message for a bad code", async () => {
    renderLogin(<VerifyEmailForm initialCode="000000" go={vi.fn()} toVerify={vi.fn()} />, {
      "POST /auth/v1/verify-email": () => jsonResponse({ error: { code: "invalid_code", message: "Code is invalid or expired" } }, 400),
    });
    await userEvent.click(screen.getByRole("button", { name: "Verify" }));
    expect(await screen.findByRole("alert")).toHaveTextContent("Code is invalid or expired");
    expect(screen.queryByRole("link", { name: "Start again" })).toBeNull();
  });
});

describe("taken email (self-hosted)", () => {
  it("sign-up sends the user to sign in with auth's message", async () => {
    const toLogin = vi.fn();
    renderLogin(<SignUpForm authRequest="V2_1" go={vi.fn()} toVerify={vi.fn()} toLogin={toLogin} />, {
      "POST /auth/v1/sign-up": () => jsonResponse({ error: { code: "email_taken", message: "An account with this email already exists" } }, 409),
    });
    await fillSignUp("Secret-pass1!");
    await waitFor(() => expect(toLogin).toHaveBeenCalledWith("An account with this email already exists"));
  });

  it("email_taken_goes_to_sign_in", async () => {
    // Switches on the code, not the 409 status.
    const toLogin = vi.fn();
    renderLogin(<SignUpForm authRequest="V2_1" go={vi.fn()} toVerify={vi.fn()} toLogin={toLogin} />, {
      "POST /auth/v1/sign-up": () => jsonResponse({ error: { code: "email_taken", message: "Already registered" } }, 400),
    });
    await fillSignUp();
    await waitFor(() => expect(toLogin).toHaveBeenCalledWith("Already registered"));
  });

  it("another 409 stays on sign-up with its message", async () => {
    const toLogin = vi.fn();
    renderLogin(<SignUpForm authRequest="V2_1" go={vi.fn()} toVerify={vi.fn()} toLogin={toLogin} />, {
      "POST /auth/v1/sign-up": () => jsonResponse({ error: { code: "invalid_request", message: "Something clashed" } }, 409),
    });
    await fillSignUp();
    expect(await screen.findByRole("alert")).toHaveTextContent("Something clashed");
    expect(toLogin).not.toHaveBeenCalled();
  });

  it("the login page shows the notice and stays, even for a sign-up request", async () => {
    const toRegister = vi.fn();
    renderLogin(
      <SignInForm authRequest="V2_1" notice="An account with this email already exists" go={vi.fn()} toVerify={vi.fn()} toRegister={toRegister} />,
      { "POST /auth/v1/auth-request": () => jsonResponse({ signUp: true, signUpAllowed: true }) },
    );
    expect(screen.getByRole("status")).toHaveTextContent("An account with this email already exists");
    await new Promise((r) => setTimeout(r, 20));
    expect(toRegister).not.toHaveBeenCalled();
  });
});

describe("VerifyEmailForm, expired", () => {
  it("offers to start again when the sign-in is over", async () => {
    renderLogin(<VerifyEmailForm initialCode="123456" go={vi.fn()} toVerify={vi.fn()} />, {
      "POST /auth/v1/verify-email": () => jsonResponse(EXPIRED_BODY, 410),
    });
    await userEvent.click(screen.getByRole("button", { name: "Verify" }));
    expect(await screen.findByRole("link", { name: "Start again" })).toHaveAttribute("href", "/");
  });
});
