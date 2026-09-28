import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import { LoginApiError, type LoginApi } from "@/lib/login-api";
import { SignInForm } from "./sign-in-form";
import { SignUpForm } from "./sign-up-form";
import { VerifyEmailForm } from "./verify-email-form";

function api(overrides: Partial<LoginApi>): LoginApi {
  return {
    authRequest: vi.fn().mockResolvedValue({ signUp: false }),
    signIn: vi.fn(), signUp: vi.fn(), verifyEmail: vi.fn(), resendCode: vi.fn().mockResolvedValue(undefined),
    requestPasswordReset: vi.fn(), confirmPasswordReset: vi.fn(), logout: vi.fn(),
    ...overrides,
  };
}

describe("SignInForm", () => {
  it("signs in and follows ui-auth's redirect", async () => {
    const go = vi.fn();
    const signIn = vi.fn().mockResolvedValue({ next: "done", redirect: "http://localhost:3000/callback?code=c" });
    render(<SignInForm api={api({ signIn })} authRequest="V2_1" go={go} toVerify={vi.fn()} toRegister={vi.fn()} />);
    await userEvent.type(await screen.findByLabelText("Email"), "ada@example.com");
    await userEvent.type(screen.getByLabelText("Password"), "Secret-pass1");
    await userEvent.click(screen.getByRole("button", { name: "Sign in" }));
    await waitFor(() => expect(go).toHaveBeenCalledWith("http://localhost:3000/callback?code=c"));
    expect(signIn).toHaveBeenCalledWith({ authRequest: "V2_1", email: "ada@example.com", password: "Secret-pass1" });
  });

  it("shows ui-auth's message on failure and stays put", async () => {
    const go = vi.fn();
    const signIn = vi.fn().mockRejectedValue(new LoginApiError(401, "Email or password is incorrect"));
    render(<SignInForm api={api({ signIn })} authRequest="V2_1" go={go} toVerify={vi.fn()} toRegister={vi.fn()} />);
    await userEvent.type(await screen.findByLabelText("Email"), "a@b.c");
    await userEvent.type(screen.getByLabelText("Password"), "x");
    await userEvent.click(screen.getByRole("button", { name: "Sign in" }));
    expect(await screen.findByRole("alert")).toHaveTextContent("Email or password is incorrect");
    expect(go).not.toHaveBeenCalled();
  });

  it("forwards to sign-up when the pending sign-in asked for it", async () => {
    const toRegister = vi.fn();
    render(<SignInForm api={api({ authRequest: vi.fn().mockResolvedValue({ signUp: true }) })} authRequest="V2_1" go={vi.fn()} toVerify={vi.fn()} toRegister={toRegister} />);
    await waitFor(() => expect(toRegister).toHaveBeenCalled());
  });

  it("goes to the code step when the email isn't verified", async () => {
    const toVerify = vi.fn();
    render(<SignInForm api={api({ signIn: vi.fn().mockResolvedValue({ next: "verify" }) })} authRequest="V2_1" go={vi.fn()} toVerify={toVerify} toRegister={vi.fn()} />);
    await userEvent.type(await screen.findByLabelText("Email"), "a@b.c");
    await userEvent.type(screen.getByLabelText("Password"), "x");
    await userEvent.click(screen.getByRole("button", { name: "Sign in" }));
    await waitFor(() => expect(toVerify).toHaveBeenCalled());
  });

  it("offers to start again when the sign-in expired", async () => {
    render(<SignInForm api={api({ authRequest: vi.fn().mockRejectedValue(new LoginApiError(410, "This sign-in has expired, start again")) })} authRequest="V2_1" go={vi.fn()} toVerify={vi.fn()} toRegister={vi.fn()} />);
    expect(await screen.findByRole("alert")).toHaveTextContent("This sign-in has expired, start again");
    expect(screen.getByRole("link", { name: "Start again" })).toHaveAttribute("href", "/");
  });

  it("needs an authRequest", () => {
    render(<SignInForm api={api({})} authRequest="" go={vi.fn()} toVerify={vi.fn()} toRegister={vi.fn()} />);
    expect(screen.getByRole("alert")).toHaveTextContent("This sign-in has expired, start again");
  });
});

describe("SignUpForm", () => {
  it("creates the account and goes to the code step", async () => {
    const toVerify = vi.fn();
    const signUp = vi.fn().mockResolvedValue({ next: "verify" });
    render(<SignUpForm api={api({ signUp })} authRequest="V2_1" go={vi.fn()} toVerify={toVerify} />);
    await userEvent.type(screen.getByLabelText("First name"), "Ada");
    await userEvent.type(screen.getByLabelText("Last name"), "Lovelace");
    await userEvent.type(screen.getByLabelText("Email"), "ada@example.com");
    await userEvent.type(screen.getByLabelText("Password"), "Secret-pass1");
    await userEvent.click(screen.getByRole("button", { name: "Create account" }));
    await waitFor(() => expect(toVerify).toHaveBeenCalled());
    expect(signUp).toHaveBeenCalledWith({ authRequest: "V2_1", email: "ada@example.com", password: "Secret-pass1", givenName: "Ada", familyName: "Lovelace" });
  });
});

describe("VerifyEmailForm", () => {
  it("prefills the code from the email link, verifies and follows the redirect", async () => {
    const go = vi.fn();
    const verifyEmail = vi.fn().mockResolvedValue({ next: "done", redirect: "http://localhost:3000/callback?code=c" });
    render(<VerifyEmailForm api={api({ verifyEmail })} initialCode="123456" go={go} toVerify={vi.fn()} />);
    expect(screen.getByLabelText("Code")).toHaveValue("123456");
    await userEvent.click(screen.getByRole("button", { name: "Verify" }));
    await waitFor(() => expect(go).toHaveBeenCalledWith("http://localhost:3000/callback?code=c"));
  });

  it("resends the code", async () => {
    const resendCode = vi.fn().mockResolvedValue(undefined);
    render(<VerifyEmailForm api={api({ resendCode })} go={vi.fn()} toVerify={vi.fn()} />);
    await userEvent.click(screen.getByRole("button", { name: "Send a new code" }));
    expect(await screen.findByText("If your account needs it, a new code is on its way.")).toBeInTheDocument();
    expect(resendCode).toHaveBeenCalled();
  });
});

describe("taken email (self-hosted)", () => {
  it("sign-up sends the user to sign in with ui-auth's message", async () => {
    const toLogin = vi.fn();
    const signUp = vi.fn().mockRejectedValue(new LoginApiError(409, "An account with this email already exists"));
    render(<SignUpForm api={api({ signUp })} authRequest="V2_1" go={vi.fn()} toVerify={vi.fn()} toLogin={toLogin} />);
    await userEvent.type(screen.getByLabelText("First name"), "Ada");
    await userEvent.type(screen.getByLabelText("Last name"), "L");
    await userEvent.type(screen.getByLabelText("Email"), "ada@example.com");
    await userEvent.type(screen.getByLabelText("Password"), "Secret-pass1!");
    await userEvent.click(screen.getByRole("button", { name: "Create account" }));
    await waitFor(() => expect(toLogin).toHaveBeenCalledWith("An account with this email already exists"));
  });

  it("the login page shows the notice and stays, even for a sign-up request", async () => {
    const toRegister = vi.fn();
    const authRequest = vi.fn().mockResolvedValue({ signUp: true });
    render(<SignInForm api={api({ authRequest })} authRequest="V2_1" notice="An account with this email already exists" go={vi.fn()} toVerify={vi.fn()} toRegister={toRegister} />);
    expect(screen.getByRole("status")).toHaveTextContent("An account with this email already exists");
    await new Promise((r) => setTimeout(r, 20));
    expect(toRegister).not.toHaveBeenCalled();
  });
});
