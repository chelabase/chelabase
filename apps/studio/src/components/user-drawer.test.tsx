import { screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import type { AppUser } from "@chelabase/management-client";
import { fakeFetch, jsonResponse, type FakeHandler } from "@/test/fake-fetch";
import { renderWithQuery } from "@/test/render";
import { UserDrawer } from "./user-drawer";

const base: AppUser = { id: "u1", email: "ada@example.com", email_verified: false, state: "active", created_at: "2026-10-03T10:00:00Z" };
const session = { id: "s1", created_at: "2026-10-03T10:00:00Z", expires_at: "2026-10-04T10:00:00Z", last_used_at: "2026-10-03T11:00:00Z" };

function setup(routes: Record<string, FakeHandler> = {}, user: AppUser = base, onClose = vi.fn()) {
  const f = fakeFetch({
    "GET /v1/projects/p1/users/u1": () => jsonResponse(user),
    "GET /v1/projects/p1/users/u1/sessions": () => jsonResponse([session]),
    ...routes,
  });
  return { fetch: f, onClose, ...renderWithQuery(<UserDrawer projectId="p1" userId="u1" onClose={onClose} />, { fetch: f }) };
}
const patchBody = (f: { calls: Request[] }) => f.calls.find((c) => c.method === "PATCH")!.json();

describe("UserDrawer", () => {
  it("drawer_edit_email_and_verified", async () => {
    const { fetch } = setup({ "PATCH /v1/projects/p1/users/u1": () => jsonResponse({ ...base, email: "ada2@example.com" }) });
    const email = await screen.findByLabelText("Email");
    expect(email).toHaveValue("ada@example.com");
    await userEvent.clear(email);
    await userEvent.type(email, "ada2@example.com");
    await userEvent.click(screen.getByRole("button", { name: "Save email" }));
    await waitFor(() => expect(fetch.calls.some((c) => c.method === "PATCH")).toBe(true));
    expect(await patchBody(fetch)).toEqual({ email: "ada2@example.com" });
  });

  it("drawer_mark_verified", async () => {
    const { fetch } = setup({ "PATCH /v1/projects/p1/users/u1": () => jsonResponse({ ...base, email_verified: true }) });
    await userEvent.click(await screen.findByRole("button", { name: "Mark verified" }));
    await waitFor(() => expect(fetch.calls.some((c) => c.method === "PATCH")).toBe(true));
    expect(await patchBody(fetch)).toEqual({ email_verified: true });
    expect(screen.queryByRole("checkbox")).toBeNull();
  });

  it("drawer_verified_is_locked", async () => {
    const { fetch } = setup({}, { ...base, email_verified: true });
    expect(await screen.findByText("Verified")).toBeInTheDocument();
    expect(screen.queryByRole("checkbox")).toBeNull();
    expect(screen.queryByRole("button", { name: "Mark verified" })).toBeNull();
    expect(fetch.calls.some((c) => c.method === "PATCH")).toBe(false);
  });

  it("drawer_deactivate_reactivate", async () => {
    let state: AppUser["state"] = "active";
    const { fetch } = setup({
      "GET /v1/projects/p1/users/u1": () => jsonResponse({ ...base, state }),
      "POST /v1/projects/p1/users/u1/deactivate": () => ((state = "inactive"), jsonResponse({ ...base, state })),
      "POST /v1/projects/p1/users/u1/reactivate": () => ((state = "active"), jsonResponse({ ...base, state })),
    });
    await userEvent.click(await screen.findByRole("button", { name: "Deactivate" }));
    await userEvent.click(await screen.findByRole("button", { name: "Reactivate" }));
    expect(await screen.findByRole("button", { name: "Deactivate" })).toBeInTheDocument();
    expect(fetch.calls.map((c) => `${c.method} ${new URL(c.url).pathname}`)).toEqual(
      expect.arrayContaining(["POST /v1/projects/p1/users/u1/deactivate", "POST /v1/projects/p1/users/u1/reactivate"]),
    );
  });

  it("drawer_sessions_sign_out_everywhere", async () => {
    let sessions = [session];
    const { fetch } = setup({
      "GET /v1/projects/p1/users/u1/sessions": () => jsonResponse(sessions),
      "DELETE /v1/projects/p1/users/u1/sessions": () => ((sessions = []), new Response(null, { status: 204 })),
    });
    expect(await screen.findByText(/Last used/)).toBeInTheDocument();
    await userEvent.click(screen.getByRole("button", { name: "Sign out everywhere" }));
    expect(await screen.findByText("No active sessions")).toBeInTheDocument();
    expect(fetch.calls.some((c) => c.method === "DELETE")).toBe(true);
  });

  it("drawer_delete_confirms", async () => {
    const { fetch, onClose } = setup({
      "DELETE /v1/projects/p1/users/u1": () => new Response(null, { status: 204 }),
    });
    await userEvent.click(await screen.findByRole("button", { name: "Delete user" }));
    const confirm = screen.getByRole("button", { name: "Delete user permanently" });
    expect(confirm).toBeDisabled();
    await userEvent.type(screen.getByLabelText(/Type ada@example.com to confirm/), "ada@example.com");
    await userEvent.click(confirm);
    await waitFor(() => expect(onClose).toHaveBeenCalled());
    expect(fetch.calls.some((c) => c.method === "DELETE" && new URL(c.url).pathname === "/v1/projects/p1/users/u1")).toBe(true);
  });

  it("drawer_sign_out_everywhere_confirms", async () => {
    setup({ "DELETE /v1/projects/p1/users/u1/sessions": () => new Response(null, { status: 204 }) });
    await userEvent.click(await screen.findByRole("button", { name: "Sign out everywhere" }));
    expect(await screen.findByText("Signed out of all sessions")).toBeInTheDocument();
  });

  it("drawer_sign_out_everywhere_error", async () => {
    setup({ "DELETE /v1/projects/p1/users/u1/sessions": () => jsonResponse({ error: { code: "internal", message: "boom" } }, 500) });
    await userEvent.click(await screen.findByRole("button", { name: "Sign out everywhere" }));
    expect(await screen.findByRole("alert")).toBeInTheDocument();
    expect(screen.queryByText("Signed out of all sessions")).toBeNull();
  });
});
