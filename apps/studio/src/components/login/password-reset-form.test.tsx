import { screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import type { ReactElement } from "react";
import { describe, expect, it } from "vitest";
import { configureLoginClient } from "@/lib/login-client/fetch";
import { fakeFetch, jsonResponse, type FakeHandler } from "@/test/fake-fetch";
import { renderWithQuery } from "@/test/render";
import { PasswordResetForm } from "./password-reset-form";

function renderLogin(ui: ReactElement, routes: Record<string, FakeHandler>) {
  const f = fakeFetch(routes);
  configureLoginClient({ baseUrl: "http://studio.test", fetchImpl: f });
  return { ...renderWithQuery(ui, { fetch: f }), f };
}

const body = (f: { calls: Request[] }, path: string) =>
  f.calls.find((r) => new URL(r.url).pathname === path)!.clone().json();

describe("PasswordResetForm", () => {
  it("requests a reset and always says the same thing", async () => {
    const { f } = renderLogin(<PasswordResetForm />, { "POST /auth/v1/password-reset": () => jsonResponse({}) });
    await userEvent.type(screen.getByLabelText("Email"), "ada@example.com");
    await userEvent.click(screen.getByRole("button", { name: "Send reset link" }));
    expect(await screen.findByText("If an account exists for that email, we've sent a reset link.")).toBeInTheDocument();
    expect(await body(f, "/auth/v1/password-reset")).toEqual({ email: "ada@example.com" });
  });

  it("sets the new password from the email link", async () => {
    const { f, queryClient } = renderLogin(<PasswordResetForm userId="u1" code="654321" />, {
      "POST /auth/v1/password-reset/confirm": () => jsonResponse({}),
    });
    await userEvent.type(screen.getByLabelText("New password"), "New-pass-12");
    await userEvent.click(screen.getByRole("button", { name: "Set password" }));
    expect(await screen.findByText("Your password is set. Sign in from Cheladb Studio.")).toBeInTheDocument();
    expect(await body(f, "/auth/v1/password-reset/confirm")).toEqual({ userId: "u1", code: "654321", password: "New-pass-12" });
    await waitFor(() => {
      expect(JSON.stringify(queryClient.getMutationCache().getAll().map((m) => m.state))).not.toContain("New-pass-12");
    });
  });

  it("shows auth's message when the code is bad", async () => {
    renderLogin(<PasswordResetForm userId="u1" code="bad" />, {
      "POST /auth/v1/password-reset/confirm": () => jsonResponse({ error: { code: "invalid_code", message: "Code is invalid or expired" } }, 400),
    });
    await userEvent.type(screen.getByLabelText("New password"), "New-pass-12");
    await userEvent.click(screen.getByRole("button", { name: "Set password" }));
    expect(await screen.findByRole("alert")).toHaveTextContent("Code is invalid or expired");
  });

  it("shows the unavailable fallback when auth can't be reached", async () => {
    renderLogin(<PasswordResetForm />, { "POST /auth/v1/password-reset": () => { throw new TypeError("Failed to fetch"); } });
    await userEvent.type(screen.getByLabelText("Email"), "ada@example.com");
    await userEvent.click(screen.getByRole("button", { name: "Send reset link" }));
    expect(await screen.findByRole("alert")).toHaveTextContent("Sign-in is unavailable, try again");
  });
});
