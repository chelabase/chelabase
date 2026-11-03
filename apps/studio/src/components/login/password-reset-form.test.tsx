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
  it("requests a code, always says the same thing, then sets the password with it", async () => {
    const { f, queryClient } = renderLogin(<PasswordResetForm />, {
      "POST /auth/v1/password-reset": () => jsonResponse({}),
      "POST /auth/v1/password-reset/confirm": () => jsonResponse({}),
    });
    await userEvent.type(screen.getByLabelText("Email"), "ada@example.com");
    await userEvent.click(screen.getByRole("button", { name: "Send a code" }));
    expect(await screen.findByText("If an account exists for that email, we've sent it a code.")).toBeInTheDocument();
    expect(await body(f, "/auth/v1/password-reset")).toEqual({ email: "ada@example.com" });
    // The email is kept: the next step asks for the code and the new password.
    expect(screen.queryByLabelText("Email")).toBeNull();
    await userEvent.type(screen.getByLabelText("Code"), " 654321 ");
    await userEvent.type(screen.getByLabelText("New password"), "New-pass-12");
    await userEvent.click(screen.getByRole("button", { name: "Set password" }));
    expect(await screen.findByText("Your password is set, and you're signed out everywhere. Sign in with the new one.")).toBeInTheDocument();
    expect(await body(f, "/auth/v1/password-reset/confirm")).toEqual({ email: "ada@example.com", code: "654321", password: "New-pass-12" });
    await waitFor(() => {
      expect(JSON.stringify(queryClient.getMutationCache().getAll().map((m) => m.state))).not.toContain("New-pass-12");
    });
  });

  it("someone back from the email enters it with the code", async () => {
    const { f } = renderLogin(<PasswordResetForm />, { "POST /auth/v1/password-reset/confirm": () => jsonResponse({}) });
    await userEvent.click(screen.getByRole("button", { name: "I have a code" }));
    await userEvent.type(screen.getByLabelText("Email"), "ada@example.com");
    await userEvent.type(screen.getByLabelText("Code"), "654321");
    await userEvent.type(screen.getByLabelText("New password"), "New-pass-12");
    await userEvent.click(screen.getByRole("button", { name: "Set password" }));
    await screen.findByText(/Your password is set/);
    expect(await body(f, "/auth/v1/password-reset/confirm")).toEqual({ email: "ada@example.com", code: "654321", password: "New-pass-12" });
  });

  it("shows auth's message when the code is bad, and keeps the step", async () => {
    renderLogin(<PasswordResetForm />, {
      "POST /auth/v1/password-reset/confirm": () => jsonResponse({ error: { code: "invalid_code", message: "Code is invalid or expired" } }, 400),
    });
    await userEvent.click(screen.getByRole("button", { name: "I have a code" }));
    await userEvent.type(screen.getByLabelText("Email"), "ada@example.com");
    await userEvent.type(screen.getByLabelText("Code"), "000000");
    await userEvent.type(screen.getByLabelText("New password"), "New-pass-12");
    await userEvent.click(screen.getByRole("button", { name: "Set password" }));
    expect(await screen.findByRole("alert")).toHaveTextContent("Code is invalid or expired");
    expect(screen.getByLabelText("Code")).toBeInTheDocument();
  });

  it("shows the unavailable fallback when auth can't be reached", async () => {
    renderLogin(<PasswordResetForm />, { "POST /auth/v1/password-reset": () => { throw new TypeError("Failed to fetch"); } });
    await userEvent.type(screen.getByLabelText("Email"), "ada@example.com");
    await userEvent.click(screen.getByRole("button", { name: "Send a code" }));
    expect(await screen.findByRole("alert")).toHaveTextContent("Sign-in is unavailable, try again");
  });
});
