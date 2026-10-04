import { screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";
import type { AppUser } from "@chelabase/management-client";
import { fakeFetch, jsonResponse, type FakeHandler } from "@/test/fake-fetch";
import { renderWithQuery } from "@/test/render";
import { UsersTab } from "./users-tab";

const user = (n: number, over: Partial<AppUser> = {}): AppUser => ({
  id: `u${n}`, email: `user${n}@example.com`, email_verified: n % 2 === 0, state: "active",
  created_at: "2026-10-03T10:00:00Z", ...over,
});

function setup(routes: Record<string, FakeHandler>) {
  const f = fakeFetch(routes);
  return { fetch: f, ...renderWithQuery(<UsersTab projectId="p1" />, { fetch: f }) };
}
const queryOf = (r: Request) => new URL(r.url).searchParams;

describe("UsersTab", () => {
  it("users_search_and_paging", async () => {
    const { fetch } = setup({
      "GET /v1/projects/p1/users": (req) => {
        const q = queryOf(req);
        if (q.get("search") === "ada") return jsonResponse({ users: [user(9, { email: "ada@example.com" })], next_page_token: null });
        if (q.get("page_token") === "t2") return jsonResponse({ users: [user(3)], next_page_token: null });
        return jsonResponse({ users: [user(1), user(2, { state: "inactive" })], next_page_token: "t2" });
      },
    });
    expect(await screen.findByText("user1@example.com")).toBeInTheDocument();
    expect(screen.getByRole("columnheader", { name: "Email" })).toBeInTheDocument();
    expect(screen.getByRole("columnheader", { name: "Verified" })).toBeInTheDocument();
    expect(screen.getByRole("columnheader", { name: "State" })).toBeInTheDocument();
    expect(screen.getByRole("columnheader", { name: "Created" })).toBeInTheDocument();
    expect(screen.getByText("inactive")).toBeInTheDocument();
    await userEvent.click(screen.getByRole("button", { name: "Next" }));
    expect(await screen.findByText("user3@example.com")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Next" })).toBeDisabled();
    await userEvent.click(screen.getByRole("button", { name: "Previous" }));
    expect(await screen.findByText("user1@example.com")).toBeInTheDocument();
    await userEvent.type(screen.getByRole("searchbox", { name: "Search users" }), "ada");
    expect(await screen.findByText("ada@example.com")).toBeInTheDocument();
    expect(screen.queryByText("user1@example.com")).toBeNull();
    expect(fetch.calls.some((c) => queryOf(c).get("search") === "ada")).toBe(true);
    // debounced: no request for the partial "a"
    expect(fetch.calls.some((c) => queryOf(c).get("search") === "a")).toBe(false);
  });

  it("row_opens_drawer", async () => {
    setup({
      "GET /v1/projects/p1/users": () => jsonResponse({ users: [user(1)], next_page_token: null }),
      "GET /v1/projects/p1/users/u1": () => jsonResponse(user(1)),
      "GET /v1/projects/p1/users/u1/sessions": () => jsonResponse([]),
    });
    await userEvent.click(await screen.findByRole("button", { name: "user1@example.com" }));
    expect(await screen.findByRole("dialog")).toHaveTextContent("user1@example.com");
  });

  it("deleted_user_leaves_the_list_even_if_the_list_lags", async () => {
    setup({
      // the identity provider's list still returns the deleted user
      "GET /v1/projects/p1/users": () => jsonResponse({ users: [user(1), user(2)], next_page_token: null }),
      "GET /v1/projects/p1/users/u1": () => jsonResponse(user(1)),
      "GET /v1/projects/p1/users/u1/sessions": () => jsonResponse([]),
      "DELETE /v1/projects/p1/users/u1": () => new Response(null, { status: 204 }),
    });
    await userEvent.click(await screen.findByRole("button", { name: "user1@example.com" }));
    await userEvent.click(await screen.findByRole("button", { name: "Delete user" }));
    await userEvent.type(screen.getByLabelText(/to confirm/), "user1@example.com");
    await userEvent.click(screen.getByRole("button", { name: "Delete user permanently" }));
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
    await waitFor(() => expect(screen.queryByRole("button", { name: "user1@example.com" })).toBeNull());
    expect(screen.getByRole("button", { name: "user2@example.com" })).toBeInTheDocument();
  });

  it("create_user_dialog", async () => {
    let list = [user(1)];
    const { fetch } = setup({
      "GET /v1/projects/p1/users": () => jsonResponse({ users: list, next_page_token: null }),
      "POST /v1/projects/p1/users": () => {
        list = [...list, user(5, { email: "new@example.com" })];
        return jsonResponse(list[1], 201);
      },
    });
    await userEvent.click(await screen.findByRole("button", { name: "Create user" }));
    const dialog = await screen.findByRole("dialog");
    await userEvent.type(screen.getByLabelText("Email"), "new@example.com");
    await userEvent.type(screen.getByLabelText("Password"), "correct horse battery");
    await userEvent.click(screen.getByRole("button", { name: "Create" }));
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
    expect(dialog).not.toBeInTheDocument();
    expect(await fetch.calls.find((c) => c.method === "POST")!.json()).toEqual({
      email: "new@example.com", password: "correct horse battery", email_verified: true,
    });
    expect(await screen.findByText("new@example.com")).toBeInTheDocument();
  });

  it("create_user_dialog_error_texts", async () => {
    setup({
      "GET /v1/projects/p1/users": () => jsonResponse({ users: [], next_page_token: null }),
      "POST /v1/projects/p1/users": () => jsonResponse({ error: { code: "email_taken", message: "raw" } }, 409),
    });
    await userEvent.click(await screen.findByRole("button", { name: "Create user" }));
    await userEvent.type(screen.getByLabelText("Email"), "a@b.co");
    await userEvent.type(screen.getByLabelText("Password"), "x");
    await userEvent.click(screen.getByRole("button", { name: "Create" }));
    expect(await screen.findByRole("alert")).toHaveTextContent("That email is already in use");
  });
});
