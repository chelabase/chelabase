import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import { ApiError, type ApiClient } from "@/lib/api";
import { fakeApi } from "@/test/fake-api";
import { DatabaseSettings } from "./database-settings";

function setup(api: Partial<ApiClient> = {}, settings = { history_window: 7 * 86400, max_branches: 5 }) {
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
    setup({}, { history_window: 36 * 3600, max_branches: 2 });
    expect(screen.getByLabelText("History window")).toHaveValue(36);
    expect(screen.getByLabelText("Window unit")).toHaveValue("hours");
  });

  it("saves the window in seconds", async () => {
    const updateDatabase = vi.fn().mockResolvedValue({ history_window: 43200, max_branches: 3 });
    const { onSaved } = setup({ updateDatabase });
    await userEvent.selectOptions(screen.getByLabelText("Window unit"), "hours");
    const win = screen.getByLabelText("History window");
    await userEvent.clear(win);
    await userEvent.type(win, "12");
    const cap = screen.getByLabelText("Branch limit");
    await userEvent.clear(cap);
    await userEvent.type(cap, "3");
    await userEvent.click(save());
    expect(updateDatabase).toHaveBeenCalledWith("db", { history_window: 43200, max_branches: 3 });
    expect(onSaved).toHaveBeenCalledWith({ history_window: 43200, max_branches: 3 });
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
});
