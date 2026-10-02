import { screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import { getListBranchDatabasesQueryKey, type BranchDatabaseView } from "@cheladb/management-client";
import { fakeFetch, jsonResponse, type FakeFetch, type FakeHandler } from "@/test/fake-fetch";
import { renderWithQuery } from "@/test/render";
import { DataApiDatabases, KeyDatabases } from "./data-api-databases";

const row = (name: string, over: Partial<BranchDatabaseView> = {}): BranchDatabaseView => ({
  name,
  owner: "cheladb_owner",
  origin: "sql",
  main: false,
  data_api: "off",
  data_api_error: null,
  default_schema: "public",
  ...over,
});
const dbs = [
  row("web1", { origin: "main", main: true, data_api: "on" }),
  row("app", { data_api: "on", default_schema: "app" }),
  row("later"),
  row("busy", { data_api: "enabling" }),
  row("broken", { data_api: "error", data_api_error: "bootstrap failed" }),
  row("postgres", { origin: "system" }),
];

const BASE = "/v1/databases/db/branches/br";
const ENABLE = (n: string) => `POST ${BASE}/pg-databases/${n}/data-api`;
const DISABLE = (n: string) => `DELETE ${BASE}/pg-databases/${n}/data-api`;
const PATCH = (n: string) => `PATCH ${BASE}/pg-databases/${n}`;
const route = (r: Request) => `${r.method} ${new URL(r.url).pathname}`;
const noContent: FakeHandler = () => new Response(null, { status: 204 });

function setup(picked: string, routes: Record<string, FakeHandler> = {}) {
  const f = fakeFetch(routes);
  const onPick = vi.fn();
  const result = renderWithQuery(
    <DataApiDatabases databaseId="db" branchId="br" databases={dbs} picked={picked} onPick={onPick} />,
    { fetch: f },
  );
  return { ...result, fetch: f, onPick };
}
const find = (f: FakeFetch, r: string) => f.calls.find((c) => route(c) === r);

describe("DataApiDatabases", () => {
  it("databases_list_shows_states_and_enable", async () => {
    const { fetch, queryClient, onPick } = setup("later", {
      [ENABLE("later")]: () => jsonResponse(row("later", { data_api: "enabling" }), 202),
    });
    queryClient.setQueryData(getListBranchDatabasesQueryKey("db", "br"), dbs);
    const picker = screen.getByLabelText("Database") as HTMLSelectElement;
    expect(Array.from(picker.options).map((o) => o.text)).toEqual([
      "web1 (main) · Served",
      "app · Served",
      "later · Not served",
      "busy · Enabling…",
      "broken · Error",
      "postgres (System) · Not served",
    ]);
    await userEvent.selectOptions(picker, "app");
    expect(onPick).toHaveBeenCalledWith("app");
    await userEvent.click(screen.getByRole("button", { name: "Enable data API" }));
    await waitFor(() => expect(find(fetch, ENABLE("later"))).toBeDefined());
    await waitFor(() =>
      expect(queryClient.getQueryState(getListBranchDatabasesQueryKey("db", "br"))?.isInvalidated).toBe(true),
    );
  });

  it("shows an enable refusal (over the cap)", async () => {
    setup("later", {
      [ENABLE("later")]: () => jsonResponse({ error: { code: "limit", message: "at most 10 served databases" } }, 409),
    });
    await userEvent.click(screen.getByRole("button", { name: "Enable data API" }));
    expect(await screen.findByRole("alert")).toHaveTextContent("at most 10 served databases");
  });

  it("shows enabling without buttons, and an error with a retry", () => {
    const { unmount } = setup("busy");
    expect(screen.getByText("Enabling the data API…")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Enable data API" })).not.toBeInTheDocument();
    unmount();
    setup("broken");
    expect(screen.getByText("bootstrap failed")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Enable data API" })).toBeEnabled();
  });

  it("system_database_cannot_be_enabled", () => {
    setup("postgres");
    expect(screen.queryByRole("button", { name: "Enable data API" })).not.toBeInTheDocument();
    expect(screen.getByText(/system database can't be served/i)).toBeInTheDocument();
  });

  it("main cannot be disabled", () => {
    setup("web1");
    expect(screen.queryByRole("button", { name: "Disable data API" })).not.toBeInTheDocument();
    expect(screen.getByText(/main database always stays served/i)).toBeInTheDocument();
  });

  it("disable_and_remove_confirm", async () => {
    const { fetch } = setup("app", { [DISABLE("app")]: noContent });
    await userEvent.click(screen.getByRole("button", { name: "Disable data API" }));
    expect(find(fetch, DISABLE("app"))).toBeUndefined();
    await userEvent.click(screen.getByRole("button", { name: "Cancel" }));
    expect(find(fetch, DISABLE("app"))).toBeUndefined();

    await userEvent.click(screen.getByRole("button", { name: "Disable data API" }));
    expect(screen.getByText(/Requests for it stop at once\./)).toBeInTheDocument();
    expect(screen.queryByText(/30 seconds/)).not.toBeInTheDocument();
    await userEvent.click(screen.getByRole("button", { name: "Confirm disable" }));
    await waitFor(() => expect(find(fetch, DISABLE("app"))).toBeDefined());
    expect(new URL(find(fetch, DISABLE("app"))!.url).search).toBe("");

    await userEvent.click(screen.getByRole("button", { name: "Remove data API machinery" }));
    expect(screen.getByText(/drops the cheladb schema/i)).toBeInTheDocument();
    await userEvent.click(screen.getByRole("button", { name: "Confirm remove" }));
    await waitFor(() => expect(fetch.calls.filter((c) => c.method === "DELETE")).toHaveLength(2));
    expect(new URL(fetch.calls.filter((c) => c.method === "DELETE")[1].url).search).toBe("?remove=true");
  });

  it("default_schema_saves_per_database", async () => {
    const { fetch } = setup("app", { [PATCH("app")]: () => jsonResponse(row("app", { default_schema: "next" })) });
    const input = screen.getByLabelText("Default schema");
    expect(input).toHaveValue("app");
    const save = screen.getByRole("button", { name: "Save default schema" });
    expect(save).toBeDisabled();
    await userEvent.clear(input);
    await userEvent.type(input, "next");
    expect(save).toBeEnabled();
    await userEvent.click(save);
    await waitFor(() => expect(find(fetch, PATCH("app"))).toBeDefined());
    // The opt-in is off by default: no grant_existing.
    expect(await find(fetch, PATCH("app"))!.clone().json()).toEqual({ default_schema: "next" });
  });

  it("grants existing objects only when the box is ticked", async () => {
    const { fetch } = setup("app", { [PATCH("app")]: () => jsonResponse(row("app", { default_schema: "next" })) });
    const input = screen.getByLabelText("Default schema");
    await userEvent.clear(input);
    await userEvent.type(input, "next");
    const box = screen.getByLabelText("Also grant existing objects in next");
    expect(box).not.toBeChecked();
    expect(
      screen.getByText("Grants anon and authenticated the same access as when the data API was enabled. Nothing is revoked."),
    ).toBeInTheDocument();
    await userEvent.click(box);
    await userEvent.click(screen.getByRole("button", { name: "Save default schema" }));
    await waitFor(() => expect(find(fetch, PATCH("app"))).toBeDefined());
    expect(await find(fetch, PATCH("app"))!.clone().json()).toEqual({ default_schema: "next", grant_existing: true });
  });

  it("unticks the grant box after a grant-only save", async () => {
    const { fetch } = setup("app", { [PATCH("app")]: () => jsonResponse(row("app", { default_schema: "app" })) });
    const box = screen.getByLabelText("Also grant existing objects in app");
    await userEvent.click(box);
    await userEvent.click(screen.getByRole("button", { name: "Save default schema" }));
    await waitFor(() => expect(find(fetch, PATCH("app"))).toBeDefined());
    await waitFor(() => expect(screen.getByLabelText("Also grant existing objects in app")).not.toBeChecked());
  });

  it("shows no raw text in the grant label while the schema is invalid", async () => {
    setup("app");
    const input = screen.getByLabelText("Default schema");
    await userEvent.clear(input);
    expect(screen.getByLabelText("Also grant existing objects")).toBeInTheDocument();
    await userEvent.type(input, "<b>x</b>");
    expect(screen.getByLabelText("Also grant existing objects")).toBeInTheDocument();
  });

  it("validates the schema name and hides the grant box for a database that isn't served", async () => {
    setup("app");
    const input = screen.getByLabelText("Default schema");
    await userEvent.clear(input);
    await userEvent.type(input, "Bad-Name");
    expect(screen.getByRole("button", { name: "Save default schema" })).toBeDisabled();
    expect(screen.getByText(/Default schema must be 1-63 characters/)).toBeInTheDocument();
  });

  it("offers no grant box for a database that isn't served", () => {
    setup("later");
    expect(screen.getByLabelText("Default schema")).toBeInTheDocument();
    expect(screen.queryByLabelText(/Also grant existing objects/)).not.toBeInTheDocument();
  });
});

describe("KeyDatabases", () => {
  it("chips, or All served databases", () => {
    const { unmount } = renderWithQuery(
      <KeyDatabases databaseId="db" branchId="br" keyId="k" value={["app"]} databases={dbs} />,
      { fetch: fakeFetch({}) },
    );
    expect(screen.getByText("app")).toBeInTheDocument();
    unmount();
    renderWithQuery(<KeyDatabases databaseId="db" branchId="br" keyId="k" value={null} databases={dbs} />, {
      fetch: fakeFetch({}),
    });
    expect(screen.getByText("All served databases")).toBeInTheDocument();
  });
});
