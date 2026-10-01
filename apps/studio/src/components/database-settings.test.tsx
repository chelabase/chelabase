import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import { ApiError, type ApiClient } from "@/lib/api";
import { fakeApi } from "@/test/fake-api";
import { DatabaseSettings } from "./database-settings";

const DEFAULTS = {
  history_window: 7 * 86400,
  max_branches: 5,
  max_rows: 1000,
  statement_timeout_ms: 8000,
  max_connections: 5,
  default_schema: "public",
};

function setup(api: Partial<ApiClient> = {}, settings = DEFAULTS) {
  const onSaved = vi.fn();
  const full = fakeApi(api);
  render(<DatabaseSettings api={full} databaseId="db" settings={settings} onSaved={onSaved} />);
  return { api: full, onSaved };
}
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
    setup({}, { ...DEFAULTS, history_window: 36 * 3600, max_branches: 2 });
    expect(screen.getByLabelText("History window")).toHaveValue(36);
    expect(screen.getByLabelText("Window unit")).toHaveValue("hours");
  });

  it("saves the window in seconds", async () => {
    const updateDatabase = vi.fn().mockResolvedValue({ ...DEFAULTS, history_window: 43200, max_branches: 3 });
    const { onSaved } = setup({ updateDatabase });
    await userEvent.selectOptions(screen.getByLabelText("Window unit"), "hours");
    const win = screen.getByLabelText("History window");
    await userEvent.clear(win);
    await userEvent.type(win, "12");
    const cap = screen.getByLabelText("Branch limit");
    await userEvent.clear(cap);
    await userEvent.type(cap, "3");
    await userEvent.click(save());
    expect(updateDatabase).toHaveBeenCalledWith("db", { ...DEFAULTS, history_window: 43200, max_branches: 3 });
    expect(onSaved).toHaveBeenCalledWith({ ...DEFAULTS, history_window: 43200, max_branches: 3 });
  });

  it("refuses values outside the limits", async () => {
    const updateDatabase = vi.fn();
    setup({ updateDatabase });
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
    expect(updateDatabase).not.toHaveBeenCalled();
  });

  it("shows the API's refusal", async () => {
    const updateDatabase = vi.fn().mockRejectedValue(new ApiError(422, "history_window must be between 1 hour and 30 days"));
    setup({ updateDatabase });
    await userEvent.click(save());
    expect(await screen.findByRole("alert")).toHaveTextContent("history_window must be");
  });

  it("says why Save settings is disabled: an invalid field, or a save in flight", async () => {
    setup({ updateDatabase: vi.fn().mockReturnValue(new Promise<never>(() => {})) });
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

  it("shows the four data API settings", () => {
    setup();
    expect(screen.getByLabelText("Max rows")).toHaveValue(1000);
    expect(screen.getByLabelText("Statement timeout (ms)")).toHaveValue(8000);
    expect(screen.getByLabelText("Max connections")).toHaveValue(5);
    expect(screen.getByLabelText("Default schema")).toHaveValue("public");
  });

  it("saves changed data API settings", async () => {
    const updateDatabase = vi.fn().mockResolvedValue({ ...DEFAULTS, max_rows: 50, default_schema: "app" });
    const { onSaved } = setup({ updateDatabase });
    const rows = screen.getByLabelText("Max rows");
    await userEvent.clear(rows);
    await userEvent.type(rows, "50");
    const schema = screen.getByLabelText("Default schema");
    await userEvent.clear(schema);
    await userEvent.type(schema, "app");
    await userEvent.click(save());
    expect(updateDatabase).toHaveBeenCalledWith("db", { ...DEFAULTS, max_rows: 50, default_schema: "app" });
    expect(onSaved).toHaveBeenCalledWith({ ...DEFAULTS, max_rows: 50, default_schema: "app" });
  });

  it("refuses data API settings outside their bounds", async () => {
    const updateDatabase = vi.fn();
    setup({ updateDatabase });
    const cases: [string, string, RegExp][] = [
      ["Max rows", "100001", /Max rows must be a whole number between 1 and 100000/],
      ["Statement timeout (ms)", "99", /Statement timeout must be a whole number between 100 and 120000/],
      ["Max connections", "51", /Max connections must be a whole number between 1 and 50/],
      ["Default schema", "Bad-Name", /Default schema must be 1-63 characters/],
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
    expect(updateDatabase).not.toHaveBeenCalled();
  });
});
