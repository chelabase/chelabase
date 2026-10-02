import { screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import { getGetDatabaseQueryKey } from "@cheladb/management-client";
import { fakeFetch, jsonResponse, type FakeFetch, type FakeHandler } from "@/test/fake-fetch";
import { renderWithQuery } from "@/test/render";
import { DatabaseSettings } from "./database-settings";

const DEFAULTS = {
  history_window: 7 * 86400,
  max_branches: 5,
  max_rows: 1000,
  statement_timeout_ms: 8000,
  max_connections: 5,
};

const PATCH = "PATCH /v1/databases/db";

function setup(patch?: FakeHandler, settings = DEFAULTS) {
  const onSaved = vi.fn();
  const f = fakeFetch(patch ? { [PATCH]: patch } : {});
  const result = renderWithQuery(<DatabaseSettings databaseId="db" settings={settings} onSaved={onSaved} />, { fetch: f });
  return { ...result, fetch: f, onSaved };
}
/** The bodies of the PATCH requests sent so far. */
const patches = (f: FakeFetch) =>
  Promise.all(f.calls.filter((r) => `${r.method} ${new URL(r.url).pathname}` === PATCH).map((r) => r.clone().json()));
const save = () => screen.getByRole("button", { name: "Save settings" });

describe("DatabaseSettings", () => {
  it("shows the current window in days and the cap, with the shrinking note", () => {
    setup();
    expect(screen.getByLabelText("History window")).toHaveValue(7);
    expect(screen.getByLabelText("Window unit")).toHaveValue("days");
    expect(screen.getByLabelText("Branch limit")).toHaveValue(5);
    expect(screen.getByText("Shrinking the window lets older history be removed.")).toBeInTheDocument();
  });

  it("uses hours when the window isn't a whole number of days", () => {
    setup(undefined, { ...DEFAULTS, history_window: 36 * 3600, max_branches: 2 });
    expect(screen.getByLabelText("History window")).toHaveValue(36);
    expect(screen.getByLabelText("Window unit")).toHaveValue("hours");
  });

  it("saves the window in seconds", async () => {
    const { fetch, onSaved } = setup(() => jsonResponse({ ...DEFAULTS, history_window: 43200, max_branches: 3 }));
    await userEvent.selectOptions(screen.getByLabelText("Window unit"), "hours");
    const win = screen.getByLabelText("History window");
    await userEvent.clear(win);
    await userEvent.type(win, "12");
    const cap = screen.getByLabelText("Branch limit");
    await userEvent.clear(cap);
    await userEvent.type(cap, "3");
    await userEvent.click(save());
    await vi.waitFor(() => expect(onSaved).toHaveBeenCalledWith({ ...DEFAULTS, history_window: 43200, max_branches: 3 }));
    expect(await patches(fetch)).toEqual([{ ...DEFAULTS, history_window: 43200, max_branches: 3 }]);
    expect(await screen.findByRole("status")).toHaveTextContent("Saved.");
  });

  it("refuses values outside the limits", async () => {
    const { fetch } = setup();
    const win = screen.getByLabelText("History window");
    await userEvent.clear(win);
    await userEvent.type(win, "31");
    expect(save()).toBeDisabled();
    expect(screen.getByText(/between 1 hour and 30 days/i)).toBeInTheDocument();
    await userEvent.clear(win);
    await userEvent.type(win, "7");
    const cap = screen.getByLabelText("Branch limit");
    await userEvent.clear(cap);
    await userEvent.type(cap, "21");
    expect(save()).toBeDisabled();
    expect(screen.getByText(/between 0 and 20/i)).toBeInTheDocument();
    expect(fetch.calls).toHaveLength(0);
  });

  it("shows the API's refusal", async () => {
    setup(() =>
      jsonResponse({ error: { code: "invalid_request", message: "history_window must be between 1 hour and 30 days" } }, 422),
    );
    await userEvent.click(save());
    expect(await screen.findByRole("alert")).toHaveTextContent("history_window must be");
  });

  it("says why Save settings is disabled: an invalid field, or a save in flight", async () => {
    setup(() => new Promise<never>(() => {}));
    expect(save()).not.toHaveAttribute("aria-describedby");
    const rows = screen.getByLabelText("Max rows");
    await userEvent.clear(rows);
    await userEvent.type(rows, "0");
    expect(save()).toBeDisabled();
    expect(save().getAttribute("aria-describedby")).toBeTruthy();
    expect(save()).toHaveAccessibleDescription(/max_rows|rows/i);
    await userEvent.clear(rows);
    await userEvent.type(rows, "50");
    expect(save()).toBeEnabled();
    expect(save()).not.toHaveAttribute("aria-describedby");
    await userEvent.click(save());
    expect(save()).toBeDisabled();
    expect(save()).toHaveAccessibleDescription("Saving the settings…");
  });

  it("shows the three data API settings and no default schema", () => {
    setup();
    expect(screen.getByLabelText("Max rows")).toHaveValue(1000);
    expect(screen.getByLabelText("Statement timeout (ms)")).toHaveValue(8000);
    expect(screen.getByLabelText("Max connections")).toHaveValue(5);
    expect(screen.queryByLabelText("Default schema")).not.toBeInTheDocument();
  });

  it("saves changed data API settings", async () => {
    const { fetch, onSaved } = setup(() => jsonResponse({ ...DEFAULTS, max_rows: 50 }));
    const rows = screen.getByLabelText("Max rows");
    await userEvent.clear(rows);
    await userEvent.type(rows, "50");
    await userEvent.click(save());
    await vi.waitFor(() => expect(onSaved).toHaveBeenCalledWith({ ...DEFAULTS, max_rows: 50 }));
    expect(await patches(fetch)).toEqual([{ ...DEFAULTS, max_rows: 50 }]);
  });

  it("refuses data API settings outside their bounds", async () => {
    const { fetch } = setup();
    const cases: [string, string, RegExp][] = [
      ["Max rows", "100001", /Max rows must be a whole number between 1 and 100000/],
      ["Statement timeout (ms)", "99", /Statement timeout must be a whole number between 100 and 120000/],
      ["Max connections", "51", /Max connections must be a whole number between 1 and 50/],
    ];
    for (const [label, value, message] of cases) {
      const input = screen.getByLabelText(label);
      const before = (input as HTMLInputElement).value;
      await userEvent.clear(input);
      await userEvent.type(input, value);
      expect(save()).toBeDisabled();
      expect(screen.getByText(message)).toBeInTheDocument();
      await userEvent.clear(input);
      await userEvent.type(input, before);
      expect(save()).toBeEnabled();
    }
    expect(fetch.calls).toHaveLength(0);
  });

  it("update_database_invalidates_get_database", async () => {
    const { queryClient, onSaved } = setup(() => jsonResponse(DEFAULTS));
    queryClient.setQueryData(getGetDatabaseQueryKey("db"), { name: "web1" });
    queryClient.setQueryData(getGetDatabaseQueryKey("other"), { name: "web2" });
    await userEvent.click(save());
    await vi.waitFor(() => expect(onSaved).toHaveBeenCalled());
    expect(queryClient.getQueryState(getGetDatabaseQueryKey("db"))?.isInvalidated).toBe(true);
    expect(queryClient.getQueryState(getGetDatabaseQueryKey("other"))?.isInvalidated).toBe(false);
  });

  it("saves_without_an_on_saved_callback", async () => {
    renderWithQuery(<DatabaseSettings databaseId="db" settings={DEFAULTS} />, { fetch: fakeFetch({ [PATCH]: () => jsonResponse(DEFAULTS) }) });
    await userEvent.click(save());
    expect(await screen.findByRole("status")).toHaveTextContent("Saved.");
  });
});
