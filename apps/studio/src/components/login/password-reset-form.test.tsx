import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import { LoginApiError, type LoginApi } from "@/lib/login-api";
import { PasswordResetForm } from "./password-reset-form";

const base = (o: Partial<LoginApi>): LoginApi => ({
  authRequest: vi.fn(), signIn: vi.fn(), signUp: vi.fn(), verifyEmail: vi.fn(), resendCode: vi.fn(),
  requestPasswordReset: vi.fn().mockResolvedValue(undefined), confirmPasswordReset: vi.fn().mockResolvedValue(undefined), logout: vi.fn(), ...o,
});

describe("PasswordResetForm", () => {
  it("requests a reset and always says the same thing", async () => {
    const api = base({});
    render(<PasswordResetForm api={api} />);
    await userEvent.type(screen.getByLabelText("Email"), "ada@example.com");
    await userEvent.click(screen.getByRole("button", { name: "Send reset link" }));
    expect(await screen.findByText("If an account exists for that email, we've sent a reset link.")).toBeInTheDocument();
    expect(api.requestPasswordReset).toHaveBeenCalledWith("ada@example.com");
  });

  it("sets the new password from the email link", async () => {
    const api = base({});
    render(<PasswordResetForm api={api} userId="u1" code="654321" />);
    await userEvent.type(screen.getByLabelText("New password"), "New-pass-12");
    await userEvent.click(screen.getByRole("button", { name: "Set password" }));
    expect(await screen.findByText("Your password is set. Sign in from Cheladb Studio.")).toBeInTheDocument();
    expect(api.confirmPasswordReset).toHaveBeenCalledWith({ userId: "u1", code: "654321", password: "New-pass-12" });
  });

  it("shows ui-auth's message when the code is bad", async () => {
    const api = base({ confirmPasswordReset: vi.fn().mockRejectedValue(new LoginApiError(400, "Code is invalid or expired")) });
    render(<PasswordResetForm api={api} userId="u1" code="bad" />);
    await userEvent.type(screen.getByLabelText("New password"), "New-pass-12");
    await userEvent.click(screen.getByRole("button", { name: "Set password" }));
    expect(await screen.findByRole("alert")).toHaveTextContent("Code is invalid or expired");
  });
});
