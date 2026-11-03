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

const DONE = { next: "done", redirect: "http://studio.chelabase.localhost:8000/" };
const EXPIRED_BODY = { error: { code: "sign_in_expired", message: "This sign-in has expired, start again" } };

/** Renders a login form against fake auth routes, with the login client pointed at them. */
function renderLogin(ui: ReactElement, routes: Record<string, FakeHandler> = {}) {
  const f = fakeFetch(routes);
  configureLoginClient({ baseUrl: "http://studio.test", fetchImpl: f });
  return { ...renderWithQuery(ui, { fetch: f }), f };
}

const call = (f: { calls: Request[] }, path: string) => f.calls.find((r) => new URL(r.url).pathname === path);
const nav = () => ({ toApp: vi.fn(), toVerify: vi.fn(), toPending: vi.fn(), toRegister: vi.fn() });

async function submitSignIn(password = "x") {
  await userEvent.type(await screen.findByLabelText("Email"), "a@b.c");
  await userEvent.type(screen.getByLabelText("Password"), password);
  await userEvent.click(screen.getByRole("button", { name: "Sign in" }));
}

describe("SignInForm", () => {
  it("signs in with the cookie session and opens the app", async () => {
    const n = nav();
    const { f } = renderLogin(<SignInForm {...n} />, { "POST /auth/v1/sign-in": () => jsonResponse({ next: "studio" }) });
    await userEvent.type(await screen.findByLabelText("Email"), "ada@example.com");
    await userEvent.type(screen.getByLabelText("Password"), "Secret-pass1");
    await userEvent.click(screen.getByRole("button", { name: "Sign in" }));
    await waitFor(() => expect(n.toApp).toHaveBeenCalled());
    const req = call(f, "/auth/v1/sign-in")!;
    expect(await req.clone().json()).toEqual({ email: "ada@example.com", password: "Secret-pass1" });
    expect(req.credentials).toBe("include");
    expect(new URL(req.url).search).toBe("");
  });

  it("goes_to_the_code_step_when_the_email_isnt_verified", async () => {
    const n = nav();
    renderLogin(<SignInForm {...n} />, { "POST /auth/v1/sign-in": () => jsonResponse({ next: "verify" }) });
    await submitSignIn();
    await waitFor(() => expect(n.toVerify).toHaveBeenCalled());
    expect(n.toApp).not.toHaveBeenCalled();
  });

  it("pending_shows_the_waiting_screen", async () => {
    const n = nav();
    renderLogin(<SignInForm {...n} />, { "POST /auth/v1/sign-in": () => jsonResponse({ next: "pending" }) });
    await submitSignIn();
    await waitFor(() => expect(n.toPending).toHaveBeenCalled());
    expect(n.toApp).not.toHaveBeenCalled();
  });

  it("tenant_required_lists_the_tenants_and_retries_with_the_chosen_one", async () => {
    const n = nav();
    const answers: Request[] = [];
    const { f } = renderLogin(<SignInForm {...n} />, {
      "POST /auth/v1/sign-in": (req) => {
        answers.push(req);
        return new URL(req.url).searchParams.get("tenant") === "t-beta"
          ? jsonResponse({ next: "studio" })
          : jsonResponse({ error: { code: "tenant_required", message: "Choose a tenant", tenants: ["t-alpha", "t-beta"] } }, 409);
      },
    });
    await submitSignIn("Secret-pass1");
    expect(await screen.findByText("Choose a workspace")).toBeInTheDocument();
    expect(screen.queryByRole("alert")).toBeNull();
    await userEvent.click(screen.getByRole("button", { name: "t-beta" }));
    await waitFor(() => expect(n.toApp).toHaveBeenCalled());
    expect(answers.map((r) => new URL(r.url).searchParams.get("tenant"))).toEqual([null, "t-beta"]);
    // The retry carries the same credentials; the chooser did not ask for them again.
    expect(await answers[1]!.clone().json()).toEqual({ email: "a@b.c", password: "Secret-pass1" });
    expect(f.calls).toHaveLength(2);
  });

  it("a_failed_retry_after_choosing_shows_the_error_and_the_chooser_stays", async () => {
    renderLogin(<SignInForm {...nav()} />, {
      "POST /auth/v1/sign-in": (req) =>
        new URL(req.url).searchParams.has("tenant")
          ? jsonResponse({ error: { code: "invalid_credentials", message: "Email or password is incorrect" } }, 401)
          : jsonResponse({ error: { code: "tenant_required", message: "Choose", tenants: ["t-alpha"] } }, 409),
    });
    await submitSignIn();
    await userEvent.click(await screen.findByRole("button", { name: "t-alpha" }));
    expect(await screen.findByRole("alert")).toHaveTextContent("Email or password is incorrect");
  });

  it("tenant_required_without_a_list_shows_auths_message_and_no_chooser", async () => {
    renderLogin(<SignInForm {...nav()} />, {
      "POST /auth/v1/sign-in": () => jsonResponse({ error: { code: "tenant_required", message: "Choose a tenant" } }, 409),
    });
    await submitSignIn();
    expect(await screen.findByRole("alert")).toHaveTextContent("Choose a tenant");
    expect(screen.queryByText("Choose a workspace")).toBeNull();
  });

  it("shows auth's message on failure and stays put", async () => {
    const n = nav();
    renderLogin(<SignInForm {...n} />, {
      "POST /auth/v1/sign-in": () => jsonResponse({ error: { code: "invalid_credentials", message: "Email or password is incorrect" } }, 401),
    });
    await submitSignIn();
    expect(await screen.findByRole("alert")).toHaveTextContent("Email or password is incorrect");
    expect(screen.queryByRole("link", { name: "Start again" })).toBeNull();
    expect(n.toApp).not.toHaveBeenCalled();
  });

  it("shows the unavailable fallback when auth can't be reached", async () => {
    renderLogin(<SignInForm {...nav()} />, { "POST /auth/v1/sign-in": () => { throw new TypeError("Failed to fetch"); } });
    await submitSignIn();
    expect(await screen.findByRole("alert")).toHaveTextContent("Sign-in is unavailable, try again");
  });

  async function signInFailingWith(answer: FakeHandler) {
    renderLogin(<SignInForm {...nav()} />, { "POST /auth/v1/sign-in": answer });
    await submitSignIn();
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
  });

  it("offers account creation and the invite line", async () => {
    const n = nav();
    renderLogin(<SignInForm {...n} />);
    await userEvent.click(await screen.findByRole("button", { name: "Create account" }));
    expect(n.toRegister).toHaveBeenCalledTimes(1);
    expect(screen.getByText(/ask your admin for an invite/i)).toBeInTheDocument();
  });

  it("shows_the_notice", () => {
    renderLogin(<SignInForm {...nav()} notice="An account with this email already exists" />);
    expect(screen.getByRole("status")).toHaveTextContent("An account with this email already exists");
  });

  it("password_never_cached", async () => {
    const n = nav();
    const { queryClient } = renderLogin(<SignInForm {...n} />, { "POST /auth/v1/sign-in": () => jsonResponse({ next: "studio" }) });
    await submitSignIn("Secret-pass1");
    await waitFor(() => expect(n.toApp).toHaveBeenCalled());
    await waitFor(() => {
      const held = JSON.stringify(queryClient.getMutationCache().getAll().map((m) => m.state));
      expect(held).not.toContain("Secret-pass1");
    });
    expect(JSON.stringify(queryClient.getQueryCache().getAll().map((q) => q.state))).not.toContain("Secret-pass1");
  });

  it("doesn't cache the password after a failed sign-in either", async () => {
    const { queryClient } = renderLogin(<SignInForm {...nav()} />, {
      "POST /auth/v1/sign-in": () => jsonResponse({ error: { code: "invalid_credentials", message: "Email or password is incorrect" } }, 401),
    });
    await submitSignIn("Secret-pass1");
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
    const { f, queryClient } = renderLogin(<SignUpForm toApp={vi.fn()} toVerify={toVerify} toPending={vi.fn()} />, {
      "POST /auth/v1/sign-up": () => jsonResponse({ next: "verify" }),
    });
    await fillSignUp();
    await waitFor(() => expect(toVerify).toHaveBeenCalled());
    expect(await call(f, "/auth/v1/sign-up")!.clone().json()).toEqual({
      email: "ada@example.com", password: "Secret-pass1", givenName: "Ada", familyName: "Lovelace",
    });
    await waitFor(() => {
      expect(JSON.stringify(queryClient.getMutationCache().getAll().map((m) => m.state))).not.toContain("Secret-pass1");
    });
  });

  it("an_open_sign_up_without_verification_opens_the_app", async () => {
    const toApp = vi.fn();
    renderLogin(<SignUpForm toApp={toApp} toVerify={vi.fn()} toPending={vi.fn()} />, {
      "POST /auth/v1/sign-up": () => jsonResponse({ next: "studio" }),
    });
    await fillSignUp();
    await waitFor(() => expect(toApp).toHaveBeenCalled());
  });

  it("a_sign_up_that_waits_for_an_admin_shows_the_waiting_screen", async () => {
    const toPending = vi.fn();
    renderLogin(<SignUpForm toApp={vi.fn()} toVerify={vi.fn()} toPending={toPending} />, {
      "POST /auth/v1/sign-up": () => jsonResponse({ next: "pending" }),
    });
    await fillSignUp();
    await waitFor(() => expect(toPending).toHaveBeenCalled());
  });

  it("shows a weak-password message without a restart link", async () => {
    renderLogin(<SignUpForm toApp={vi.fn()} toVerify={vi.fn()} toPending={vi.fn()} />, {
      "POST /auth/v1/sign-up": () => jsonResponse({ error: { code: "weak_password", message: "Password is too weak" } }, 400),
    });
    await fillSignUp("x");
    expect(await screen.findByRole("alert")).toHaveTextContent("Password is too weak");
    expect(screen.queryByRole("link", { name: "Start again" })).toBeNull();
  });
});

describe("VerifyEmailForm", () => {
  it("prefills the code from the email link, verifies and opens the app", async () => {
    const toApp = vi.fn();
    const { f } = renderLogin(<VerifyEmailForm initialCode="123456" toApp={toApp} toVerify={vi.fn()} toPending={vi.fn()} />, {
      "POST /auth/v1/verify-email": () => jsonResponse(DONE),
    });
    expect(screen.getByLabelText("Code")).toHaveValue("123456");
    await userEvent.click(screen.getByRole("button", { name: "Verify" }));
    await waitFor(() => expect(toApp).toHaveBeenCalled());
    expect(await call(f, "/auth/v1/verify-email")!.clone().json()).toEqual({ code: "123456" });
  });

  it("resends the code", async () => {
    const { f } = renderLogin(<VerifyEmailForm toApp={vi.fn()} toVerify={vi.fn()} toPending={vi.fn()} />, {
      "POST /auth/v1/resend-code": () => jsonResponse({}),
    });
    await userEvent.click(screen.getByRole("button", { name: "Send a new code" }));
    expect(await screen.findByText("If your account needs it, a new code is on its way.")).toBeInTheDocument();
    expect(call(f, "/auth/v1/resend-code")).toBeDefined();
  });

  it("says the same thing when resending fails", async () => {
    renderLogin(<VerifyEmailForm toApp={vi.fn()} toVerify={vi.fn()} toPending={vi.fn()} />, {
      "POST /auth/v1/resend-code": () => jsonResponse({ error: { code: "rate_limited", message: "Slow down" } }, 429),
    });
    await userEvent.click(screen.getByRole("button", { name: "Send a new code" }));
    expect(await screen.findByText("If your account needs it, a new code is on its way.")).toBeInTheDocument();
  });

  it("sign_in_expired_on_verify_shows_restart", async () => {
    renderLogin(<VerifyEmailForm initialCode="123456" toApp={vi.fn()} toVerify={vi.fn()} toPending={vi.fn()} />, {
      "POST /auth/v1/verify-email": () => jsonResponse({ error: { code: "sign_in_expired", message: "Too late, begin anew" } }, 410),
    });
    await userEvent.click(screen.getByRole("button", { name: "Verify" }));
    expect(await screen.findByRole("alert")).toHaveTextContent("Too late, begin anew");
    expect(screen.getByRole("link", { name: "Start again" })).toHaveAttribute("href", "/login");
  });

  it("shows auth's message for a bad code", async () => {
    renderLogin(<VerifyEmailForm initialCode="000000" toApp={vi.fn()} toVerify={vi.fn()} toPending={vi.fn()} />, {
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
    renderLogin(<SignUpForm toApp={vi.fn()} toVerify={vi.fn()} toPending={vi.fn()} toLogin={toLogin} />, {
      "POST /auth/v1/sign-up": () => jsonResponse({ error: { code: "email_taken", message: "An account with this email already exists" } }, 409),
    });
    await fillSignUp("Secret-pass1!");
    await waitFor(() => expect(toLogin).toHaveBeenCalledWith("An account with this email already exists"));
  });

  it("email_taken_goes_to_sign_in", async () => {
    // Switches on the code, not the 409 status.
    const toLogin = vi.fn();
    renderLogin(<SignUpForm toApp={vi.fn()} toVerify={vi.fn()} toPending={vi.fn()} toLogin={toLogin} />, {
      "POST /auth/v1/sign-up": () => jsonResponse({ error: { code: "email_taken", message: "Already registered" } }, 400),
    });
    await fillSignUp();
    await waitFor(() => expect(toLogin).toHaveBeenCalledWith("Already registered"));
  });

  it("another 409 stays on sign-up with its message", async () => {
    const toLogin = vi.fn();
    renderLogin(<SignUpForm toApp={vi.fn()} toVerify={vi.fn()} toPending={vi.fn()} toLogin={toLogin} />, {
      "POST /auth/v1/sign-up": () => jsonResponse({ error: { code: "invalid_request", message: "Something clashed" } }, 409),
    });
    await fillSignUp();
    expect(await screen.findByRole("alert")).toHaveTextContent("Something clashed");
    expect(toLogin).not.toHaveBeenCalled();
  });

});

describe("VerifyEmailForm, expired", () => {
  it("offers to start again when the sign-in is over", async () => {
    renderLogin(<VerifyEmailForm initialCode="123456" toApp={vi.fn()} toVerify={vi.fn()} toPending={vi.fn()} />, {
      "POST /auth/v1/verify-email": () => jsonResponse(EXPIRED_BODY, 410),
    });
    await userEvent.click(screen.getByRole("button", { name: "Verify" }));
    expect(await screen.findByRole("link", { name: "Start again" })).toHaveAttribute("href", "/login");
  });
});
