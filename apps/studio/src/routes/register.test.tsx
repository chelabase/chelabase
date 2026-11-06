import { screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import { SignUpForm } from "@/components/login/sign-up-form";
import { configureLoginClient } from "@/lib/login-client/fetch";
import { fakeFetch, jsonResponse, type FakeHandler } from "@/test/fake-fetch";
import { renderWithQuery } from "@/test/render";

// The register page's sign-up form on an unclaimed instance (selfhost-install m3): the
// setup-code field follows `GET /auth/v1/instance`, and the code goes to auth as `setupCode`.

function renderRegister(routes: Record<string, FakeHandler>) {
  const f = fakeFetch(routes);
  configureLoginClient({ baseUrl: "http://studio.test", fetchImpl: f });
  renderWithQuery(<SignUpForm toApp={vi.fn()} toVerify={vi.fn()} toPending={vi.fn()} />, { fetch: f });
  return f;
}

const signUpBody = async (f: { calls: Request[] }) =>
  f.calls.find((r) => new URL(r.url).pathname === "/auth/v1/sign-up")!.clone().json();

async function fill() {
  await userEvent.type(screen.getByLabelText("First name"), "Ada");
  await userEvent.type(screen.getByLabelText("Last name"), "Lovelace");
  await userEvent.type(screen.getByLabelText("Email"), "ada@example.com");
  await userEvent.type(screen.getByLabelText("Password"), "Secret-pass1");
}

const create = () => userEvent.click(screen.getByRole("button", { name: "Create account" }));

describe("register: setup code", () => {
  it("shows the field on an unclaimed instance and sends the code as setupCode", async () => {
    const f = renderRegister({
      "GET /auth/v1/instance": () => jsonResponse({ setupRequired: true }),
      "POST /auth/v1/sign-up": () => jsonResponse({ next: "verify" }),
    });
    const field = await screen.findByLabelText("Setup code");
    expect(field).toHaveAttribute("autocomplete", "off");
    expect(screen.getByText(/docker compose logs init-secrets/)).toBeInTheDocument();
    await fill();
    await userEvent.type(field, " abcd-efgh-jkmn-pqrs-tuvw ");
    await create();
    await waitFor(async () =>
      expect(await signUpBody(f)).toEqual({
        email: "ada@example.com", password: "Secret-pass1", givenName: "Ada", familyName: "Lovelace",
        setupCode: "abcd-efgh-jkmn-pqrs-tuvw",
      }),
    );
  });

  it("hides the field on a claimed instance and sends no code", async () => {
    const f = renderRegister({
      "GET /auth/v1/instance": () => jsonResponse({ setupRequired: false }),
      "POST /auth/v1/sign-up": () => jsonResponse({ next: "verify" }),
    });
    await waitFor(() => expect(f.calls.some((r) => new URL(r.url).pathname === "/auth/v1/instance")).toBe(true));
    await fill();
    expect(screen.queryByLabelText("Setup code")).toBeNull();
    await create();
    await waitFor(async () => expect(await signUpBody(f)).not.toHaveProperty("setupCode"));
  });

  it("shows the field when auth asks for the code, even if the instance check failed", async () => {
    renderRegister({
      "GET /auth/v1/instance": () => jsonResponse({ error: { code: "unavailable", message: "x" } }, 503),
      "POST /auth/v1/sign-up": () =>
        jsonResponse({ error: { code: "setup_code_invalid", message: "The setup code is missing or wrong" } }, 403),
    });
    await fill();
    expect(screen.queryByLabelText("Setup code")).toBeNull();
    await create();
    expect(await screen.findByRole("alert")).toHaveTextContent("The setup code is missing or wrong");
    expect(await screen.findByLabelText("Setup code")).toBeInTheDocument();
  });
});
