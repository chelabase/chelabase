import { screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import { fakeFetch, jsonResponse } from "@/test/fake-fetch";
import { renderWithQuery } from "@/test/render";
import { NewProjectDialog } from "./new-project-dialog";

const info = {
  id: "p9", slug: "shop", name: "Shop", status: "active", error: null, app_users: null,
  created_at: "2026-10-03T10:00:00Z", updated_at: "2026-10-03T10:00:00Z",
};

describe("NewProjectDialog", () => {
  it("new_project_dialog_creates_and_calls_onCreated", async () => {
    const f = fakeFetch({ "POST /v1/projects": () => jsonResponse(info, 201) });
    const onCreated = vi.fn();
    renderWithQuery(<NewProjectDialog onCreated={onCreated} />, { fetch: f });
    await userEvent.click(screen.getByRole("button", { name: "New project" }));
    const create = await screen.findByRole("button", { name: "Create project" });
    expect(create).toBeDisabled();
    await userEvent.type(screen.getByLabelText("Project name"), "  Shop ");
    await userEvent.click(create);
    expect(onCreated).toHaveBeenCalledWith(expect.objectContaining({ id: "p9" }));
    expect(await f.calls[0].json()).toEqual({ name: "Shop" });
  });

  it("shows the refusal and stays open", async () => {
    const f = fakeFetch({
      "POST /v1/projects": () => jsonResponse({ error: { code: "invalid_request", message: "name too long" } }, 422),
    });
    const onCreated = vi.fn();
    renderWithQuery(<NewProjectDialog onCreated={onCreated} />, { fetch: f });
    await userEvent.click(screen.getByRole("button", { name: "New project" }));
    await userEvent.type(await screen.findByLabelText("Project name"), "x");
    await userEvent.click(screen.getByRole("button", { name: "Create project" }));
    expect(await screen.findByRole("alert")).toHaveTextContent("name too long");
    expect(onCreated).not.toHaveBeenCalled();
  });
});
