import { screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";
import type { AppUsersSettings, DatabaseSummary, ProjectInfo } from "@cheladb/management-client";
import { fakeFetch, jsonResponse, sequence, type FakeHandler } from "@/test/fake-fetch";
import { renderWithQuery } from "@/test/render";
import { AppUsersCard } from "./app-users-card";

const project: ProjectInfo = {
  id: "p1", slug: "default", name: "Default", status: "active", error: null, app_users: null,
  created_at: "2026-10-03T10:00:00Z", updated_at: "2026-10-03T10:00:00Z",
};
const dbs: DatabaseSummary[] = [
  { database_id: "d1", name: "web1", status: "ready", host: null, port: null, branch_count: 1 },
  { database_id: "d2", name: "booting", status: "provisioning", host: null, port: null, branch_count: 1 },
];
const settings = (over: Partial<AppUsersSettings> = {}): AppUsersSettings => ({
  database_id: "d1", pg_database: "postgres", schema: "public", table: "users", email_verification: false,
  on_delete: "delete", status: "enabled", error: null, zitadel_org_id: "org1", ...over,
});
const mainBranch = {
  id: "b-main", name: "main", parent_branch_id: null, kind: "normal", status: "ready", host: "h", port: 1,
  branched_at_time: null, branched_at_lsn: null, created_at: "2026-09-30T10:00:00Z", history_start: null, error: null,
};
const bdb = (name: string, data_api: string, origin = "user") => ({
  name, data_api, data_api_error: null, default_schema: "public", main: name === "postgres", origin, owner: "o",
});
const branchRoutes: Record<string, FakeHandler> = {
  "GET /v1/databases/d1/branches": () => jsonResponse([mainBranch]),
  "GET /v1/databases/d1/branches/b-main/databases": () =>
    jsonResponse([bdb("postgres", "on"), bdb("other", "off"), bdb("shop", "on")]),
};

function setup(routes: Record<string, FakeHandler> = {}, p: ProjectInfo = project) {
  const f = fakeFetch({ ...branchRoutes, ...routes });
  return { fetch: f, ...renderWithQuery(<AppUsersCard project={p} databases={dbs} />, { fetch: f }) };
}

describe("AppUsersCard", () => {
  it("enable_form_defaults_and_submits", async () => {
    const { fetch } = setup({ "PUT /v1/projects/p1/app-users": () => jsonResponse(settings({ status: "enabling" }), 202) });
    expect(screen.getByLabelText("Database")).toHaveValue("d1");
    // only ready databases are offered
    expect(screen.queryByRole("option", { name: "booting" })).toBeNull();
    const pg = await screen.findByLabelText("Postgres database");
    await waitFor(() => expect(screen.getByRole("option", { name: "shop" })).toBeInTheDocument());
    // data API off: not offered
    expect(screen.queryByRole("option", { name: "other" })).toBeNull();
    expect(pg).toHaveValue("postgres");
    expect(screen.getByLabelText("Schema")).toHaveValue("public");
    expect(screen.getByLabelText("Table")).toHaveValue("users");
    expect(screen.getByLabelText("Require email verification")).not.toBeChecked();
    expect(screen.getByLabelText("When a user is deleted in Zitadel")).toHaveValue("delete");
    expect(screen.getByText(/GRANT INSERT \(col\) ON <table> TO authenticated/)).toBeInTheDocument();
    expect(screen.getByText(/RLS is on/)).toBeInTheDocument();
    await userEvent.click(screen.getByRole("button", { name: "Enable app users" }));
    await waitFor(() => expect(fetch.calls.some((c) => c.method === "PUT")).toBe(true));
    const put = fetch.calls.find((c) => c.method === "PUT")!;
    expect(await put.json()).toEqual({
      database_id: "d1", pg_database: "postgres", schema: "public", table: "users",
      email_verification: false, on_delete: "delete",
    });
  });

  it("enabling_polls_until_enabled", async () => {
    setup(
      {
        "GET /v1/projects/p1/app-users": sequence(
          () => jsonResponse(settings({ status: "enabling" })),
          () => jsonResponse(settings({ status: "enabled" })),
        ),
      },
      { ...project, app_users: "enabling" },
    );
    expect(await screen.findByText("Enabling…")).toBeInTheDocument();
    expect(await screen.findByRole("button", { name: "Disable app users" }, { timeout: 5000 })).toBeInTheDocument();
  }, 10000);

  it("failed_shows_error_and_retry", async () => {
    const { fetch } = setup(
      {
        "GET /v1/projects/p1/app-users": () => jsonResponse(settings({ status: "failed", error: "zitadel org: boom" })),
        "PUT /v1/projects/p1/app-users": () => jsonResponse(settings({ status: "enabling" }), 202),
      },
      { ...project, app_users: "failed" },
    );
    expect(await screen.findByText("zitadel org: boom")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Disable" })).toBeInTheDocument();
    await userEvent.click(screen.getByRole("button", { name: "Retry" }));
    await waitFor(() => expect(fetch.calls.some((c) => c.method === "PUT")).toBe(true));
    expect(await fetch.calls.find((c) => c.method === "PUT")!.json()).toMatchObject({
      database_id: "d1", pg_database: "postgres", schema: "public", table: "users", on_delete: "delete",
    });
  });

  it("enabled_settings_update_verification", async () => {
    const { fetch } = setup(
      {
        "GET /v1/projects/p1/app-users": () => jsonResponse(settings()),
        "PUT /v1/projects/p1/app-users": () => jsonResponse(settings({ email_verification: true })),
      },
      { ...project, app_users: "enabled" },
    );
    await userEvent.click(await screen.findByLabelText("Require email verification"));
    await userEvent.click(screen.getByRole("button", { name: "Save settings" }));
    await waitFor(() => expect(fetch.calls.some((c) => c.method === "PUT")).toBe(true));
    expect(await fetch.calls.find((c) => c.method === "PUT")!.json()).toMatchObject({ email_verification: true, on_delete: "delete" });
  });

  it("disable_refused_text", async () => {
    setup(
      {
        "GET /v1/projects/p1/app-users": () => jsonResponse(settings()),
        "DELETE /v1/projects/p1/app-users": () =>
          jsonResponse({ error: { code: "app_users_has_users", message: "raw server text" } }, 409),
      },
      { ...project, app_users: "enabled" },
    );
    await userEvent.click(await screen.findByRole("button", { name: "Disable app users" }));
    const alert = await screen.findByRole("alert");
    expect(alert).toHaveTextContent("Delete the project's users first");
    expect(alert).not.toHaveTextContent("raw server text");
  });

  it("enable_refusal_text", async () => {
    setup({
      "PUT /v1/projects/p1/app-users": () =>
        jsonResponse({ error: { code: "data_api_not_enabled", message: "raw" } }, 409),
    });
    await userEvent.click(await screen.findByRole("button", { name: "Enable app users" }));
    expect(await screen.findByRole("alert")).toHaveTextContent(
      "Enable the data API on every branch of this database first",
    );
  });

  it("disable_completes_shows_enable_form", async () => {
    let gone = false;
    setup(
      {
        "GET /v1/projects/p1/app-users": () =>
          gone
            ? jsonResponse({ error: { code: "app_users_disabled", message: "off" } }, 404)
            : jsonResponse(settings({ status: "disabling" })),
        "GET /v1/projects/p1": () => jsonResponse({ ...project, app_users: gone ? null : "disabling" }),
        "DELETE /v1/projects/p1/app-users": () => ((gone = true), jsonResponse(settings({ status: "disabling" }), 202)),
      },
      { ...project, app_users: "enabled" },
    );
    // enabled from the project's view; settings say enabled first
    gone = false;
    expect(await screen.findByText("Disabling…")).toBeInTheDocument();
    gone = true;
    expect(await screen.findByRole("button", { name: "Enable app users" }, { timeout: 5000 })).toBeInTheDocument();
  }, 10000);

  it("failed_can_be_disabled", async () => {
    const { fetch } = setup(
      {
        "GET /v1/projects/p1/app-users": () => jsonResponse(settings({ status: "failed", error: "boom" })),
        "DELETE /v1/projects/p1/app-users": () => jsonResponse(settings({ status: "disabling" }), 202),
      },
      { ...project, app_users: "failed" },
    );
    await userEvent.click(await screen.findByRole("button", { name: "Disable" }));
    await waitFor(() => expect(fetch.calls.some((c) => c.method === "DELETE")).toBe(true));
  });

  it("location_fixed_text", async () => {
    setup(
      {
        "GET /v1/projects/p1/app-users": () => jsonResponse(settings()),
        "PUT /v1/projects/p1/app-users": () =>
          jsonResponse({ error: { code: "app_users_location_fixed", message: "raw" } }, 409),
      },
      { ...project, app_users: "enabled" },
    );
    await userEvent.click(await screen.findByLabelText("Require email verification"));
    await userEvent.click(screen.getByRole("button", { name: "Save settings" }));
    expect(await screen.findByRole("alert")).toHaveTextContent("can't change once app users are enabled");
  });

  it("keeps_the_database_name_while_the_project_deletes", async () => {
    const f = fakeFetch({ ...branchRoutes, "GET /v1/projects/p1/app-users": () => jsonResponse(settings()) });
    const enabled: ProjectInfo = { ...project, app_users: "enabled" };
    const { rerender } = renderWithQuery(<AppUsersCard project={enabled} databases={dbs} />, { fetch: f });
    expect(await screen.findByText("web1")).toBeInTheDocument();
    // the workspace stops listing the project's databases once it is deleting
    rerender(<AppUsersCard project={{ ...enabled, status: "deleting" }} databases={[]} />);
    expect(screen.getByText("web1")).toBeInTheDocument();
    expect(screen.queryByText("d1")).toBeNull();
  });
});
