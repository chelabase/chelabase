import { screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";
import type { DatabaseSummary, ProjectInfo } from "@cheladb/management-client";
import { fakeFetch, jsonResponse, type FakeHandler } from "@/test/fake-fetch";
import { renderWithQuery } from "@/test/render";
import { ProjectSettings } from "./project-settings";

const project: ProjectInfo = {
  id: "p1", slug: "default", name: "Default", status: "active", error: null, app_users: null,
  created_at: "2026-10-03T10:00:00Z", updated_at: "2026-10-03T10:00:00Z",
};
const dbs: DatabaseSummary[] = [
  { database_id: "d1", name: "web1", status: "ready", host: null, port: null, branch_count: 1 },
  { database_id: "d2", name: "web2", status: "ready", host: null, port: null, branch_count: 1 },
];

function setup(
  routes: Record<string, FakeHandler> = {},
  p: ProjectInfo = project,
  databases: DatabaseSummary[] | null = dbs,
  databasesError?: unknown,
) {
  const f = fakeFetch(routes);
  return {
    fetch: f,
    ...renderWithQuery(
      <ProjectSettings project={p} databases={databases ?? undefined} databasesError={databasesError} onDeleted={() => {}} />,
      { fetch: f },
    ),
  };
}

describe("ProjectSettings", () => {
  it("rename_saves", async () => {
    const { fetch } = setup({
      "PATCH /v1/projects/p1": () => jsonResponse({ ...project, name: "Storefront" }),
    });
    const save = screen.getByRole("button", { name: "Save" });
    expect(save).toBeDisabled();
    const input = screen.getByLabelText("Project name");
    await userEvent.clear(input);
    await userEvent.type(input, " Storefront ");
    await userEvent.click(save);
    expect(await screen.findByText("Saved")).toBeInTheDocument();
    expect(await fetch.calls.find((c) => c.method === "PATCH")!.json()).toEqual({ name: "Storefront" });
  });

  it("delete_requires_slug_and_lists_databases", async () => {
    const { fetch } = setup({
      "DELETE /v1/projects/p1": () => jsonResponse({ ...project, status: "deleting" }, 202),
    });
    await userEvent.click(screen.getByRole("button", { name: "Delete project" }));
    const dialog = await screen.findByRole("dialog");
    expect(dialog).toHaveTextContent("web1");
    expect(dialog).toHaveTextContent("web2");
    const confirm = screen.getByRole("button", { name: "Delete project and its databases" });
    expect(confirm).toBeDisabled();
    await userEvent.type(screen.getByLabelText(/Type default to confirm/), "defaul");
    expect(confirm).toBeDisabled();
    await userEvent.type(screen.getByLabelText(/Type default to confirm/), "t");
    expect(confirm).toBeEnabled();
    await userEvent.click(confirm);
    await screen.findByText("Delete started");
    expect(await fetch.calls.find((c) => c.method === "DELETE")!.json()).toEqual({ confirm: "default" });
  });

  it.each([
    ["project_deleting", "already being deleted"],
    ["database_not_ready", "still being created"],
    ["branch_protected", "protected"],
    ["conflict", "app users are being enabled or disabled"],
  ])("delete_refusal_texts: %s", async (code, text) => {
    setup({ "DELETE /v1/projects/p1": () => jsonResponse({ error: { code, message: "raw server text" } }, 409) });
    await userEvent.click(screen.getByRole("button", { name: "Delete project" }));
    await userEvent.type(await screen.findByLabelText(/Type default to confirm/), "default");
    await userEvent.click(screen.getByRole("button", { name: "Delete project and its databases" }));
    const alert = await screen.findByRole("alert");
    expect(alert.textContent?.toLowerCase()).toContain(text);
    expect(alert).not.toHaveTextContent("raw server text");
  });

  it("deleting_banner_disables_actions: settings", () => {
    setup({}, { ...project, status: "deleting" });
    expect(screen.getByRole("button", { name: "Delete project" })).toBeDisabled();
    expect(screen.getByLabelText("Project name")).toBeDisabled();
  });

  it("delete_waits_for_the_database_list", async () => {
    setup({}, project, null);
    await userEvent.click(screen.getByRole("button", { name: "Delete project" }));
    const dialog = await screen.findByRole("dialog");
    expect(dialog).toHaveTextContent("Loading databases");
    expect(dialog).not.toHaveTextContent("no databases");
    await userEvent.type(screen.getByLabelText(/Type default to confirm/), "default");
    expect(screen.getByRole("button", { name: "Delete project and its databases" })).toBeDisabled();
  });

  it("delete_is_blocked_when_the_database_list_failed", async () => {
    setup({}, project, null, new Error("workspace down"));
    await userEvent.click(screen.getByRole("button", { name: "Delete project" }));
    const dialog = await screen.findByRole("dialog");
    expect(dialog).toHaveTextContent("workspace down");
    expect(dialog).not.toHaveTextContent("no databases");
    await userEvent.type(screen.getByLabelText(/Type default to confirm/), "default");
    expect(screen.getByRole("button", { name: "Delete project and its databases" })).toBeDisabled();
  });

  it("closing_the_dialog_resets_the_slug_and_the_error", async () => {
    setup({ "DELETE /v1/projects/p1": () => jsonResponse({ error: { code: "branch_protected", message: "x" } }, 409) });
    await userEvent.click(screen.getByRole("button", { name: "Delete project" }));
    await userEvent.type(await screen.findByLabelText(/Type default to confirm/), "default");
    await userEvent.click(screen.getByRole("button", { name: "Delete project and its databases" }));
    await screen.findByRole("alert");
    await userEvent.click(screen.getByRole("button", { name: "Cancel" }));
    await userEvent.click(screen.getByRole("button", { name: "Delete project" }));
    expect(await screen.findByLabelText(/Type default to confirm/)).toHaveValue("");
    expect(screen.queryByRole("alert")).toBeNull();
  });
});
