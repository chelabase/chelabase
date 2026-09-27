import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { ConnectionDetails, DatabaseSummary } from "@cheladb/types";
import { ApiError, type ApiClient } from "@/lib/api";
import { DatabaseActions } from "./database-actions";

const ready: DatabaseSummary = {
  database_id: "d1" as never,
  name: "web1",
  status: "ready",
  host: "127.0.0.1",
  port: 49153,
};

const details = (password: string): ConnectionDetails => ({
  host: "127.0.0.1",
  port: 49153,
  database: "web1",
  user: "postgres",
  password,
  uri: `postgres://postgres:${password}@127.0.0.1:49153/web1`,
});

function setup(api: Partial<ApiClient>, database: DatabaseSummary = ready) {
  const onChanged = vi.fn();
  const full: ApiClient = {
    me: vi.fn(),
    createDatabase: vi.fn(),
    getDatabase: vi.fn(),
    getConnection: vi.fn(),
    resetPassword: vi.fn(),
    deleteDatabase: vi.fn(),
    ...api,
  };
  render(<DatabaseActions api={full} database={database} onChanged={onChanged} />);
  return { api: full, onChanged };
}

afterEach(() => vi.unstubAllGlobals());

describe("DatabaseActions", () => {
  it("reveals details with the password masked until asked", async () => {
    setup({ getConnection: vi.fn().mockResolvedValue(details("s3cret")) });
    await userEvent.click(screen.getByRole("button", { name: "Connection details" }));

    expect(await screen.findByText("postgres")).toBeInTheDocument();
    expect(screen.getByText("web1", { selector: "dd" })).toBeInTheDocument();
    expect(screen.queryByText("s3cret")).toBeNull();
    expect(screen.queryByText(/postgres:\/\//)).toBeNull();

    await userEvent.click(screen.getByRole("button", { name: "Show password" }));
    expect(screen.getByText("s3cret")).toBeInTheDocument();
    expect(screen.getByText("postgres://postgres:s3cret@127.0.0.1:49153/web1")).toBeInTheDocument();
  });

  it("copies the connection URI", async () => {
    const writeText = vi.fn().mockResolvedValue(undefined);
    vi.stubGlobal("navigator", { ...navigator, clipboard: { writeText } });
    setup({ getConnection: vi.fn().mockResolvedValue(details("s3cret")) });
    await userEvent.click(screen.getByRole("button", { name: "Connection details" }));
    await userEvent.click(await screen.findByRole("button", { name: "Copy URI" }));
    expect(writeText).toHaveBeenCalledWith("postgres://postgres:s3cret@127.0.0.1:49153/web1");
  });

  it("forgets revealed details when the panel is closed", async () => {
    const getConnection = vi.fn().mockResolvedValue(details("s3cret"));
    setup({ getConnection });
    await userEvent.click(screen.getByRole("button", { name: "Connection details" }));
    await userEvent.click(await screen.findByRole("button", { name: "Show password" }));
    expect(screen.getByText("s3cret")).toBeInTheDocument();

    await userEvent.click(screen.getByRole("button", { name: "Connection details" }));
    expect(screen.queryByText("s3cret")).toBeNull();

    // Reopening fetches again and starts masked.
    await userEvent.click(screen.getByRole("button", { name: "Connection details" }));
    expect(await screen.findByRole("button", { name: "Show password" })).toBeInTheDocument();
    expect(screen.queryByText("s3cret")).toBeNull();
    expect(getConnection).toHaveBeenCalledTimes(2);
  });

  it("has a copy button for every field", async () => {
    const writeText = vi.fn().mockResolvedValue(undefined);
    vi.stubGlobal("navigator", { ...navigator, clipboard: { writeText } });
    setup({ getConnection: vi.fn().mockResolvedValue(details("s3cret")) });
    await userEvent.click(screen.getByRole("button", { name: "Connection details" }));
    for (const [field, value] of [
      ["host", "127.0.0.1"],
      ["port", "49153"],
      ["database", "web1"],
      ["user", "postgres"],
      ["password", "s3cret"],
    ]) {
      await userEvent.click(await screen.findByRole("button", { name: `Copy ${field}` }));
      expect(writeText).toHaveBeenLastCalledWith(value);
    }
  });

  it("only offers credentials for ready databases", () => {
    setup({}, { ...ready, status: "provisioning" });
    expect(screen.getByRole("button", { name: "Connection details" })).toBeDisabled();
    expect(screen.getByRole("button", { name: "Reset password" })).toBeDisabled();
  });

  it("resets the password after confirmation and shows the new details", async () => {
    const resetPassword = vi.fn().mockResolvedValue(undefined);
    const getConnection = vi.fn().mockResolvedValue(details("n3w"));
    setup({ resetPassword, getConnection });

    await userEvent.click(screen.getByRole("button", { name: "Reset password" }));
    expect(resetPassword).not.toHaveBeenCalled();
    await userEvent.click(screen.getByRole("button", { name: "Confirm reset" }));

    expect(resetPassword).toHaveBeenCalledWith("d1");
    expect(await screen.findByText(/password was reset/i)).toBeInTheDocument();
    await userEvent.click(screen.getByRole("button", { name: "Show password" }));
    expect(screen.getByText("n3w")).toBeInTheDocument();
  });

  it("shows a reset failure", async () => {
    const resetPassword = vi.fn().mockRejectedValue(new ApiError(504, "worker did not respond; try again"));
    setup({ resetPassword });
    await userEvent.click(screen.getByRole("button", { name: "Reset password" }));
    await userEvent.click(screen.getByRole("button", { name: "Confirm reset" }));
    expect(await screen.findByRole("alert")).toHaveTextContent("worker did not respond");
  });

  it("deletes only after the name is typed", async () => {
    const deleteDatabase = vi.fn().mockResolvedValue(undefined);
    const { onChanged } = setup({ deleteDatabase });

    await userEvent.click(screen.getByRole("button", { name: "Delete" }));
    const confirm = screen.getByRole("button", { name: "Delete permanently" });
    expect(confirm).toBeDisabled();

    await userEvent.type(screen.getByLabelText(/type the database name/i), "web");
    expect(confirm).toBeDisabled();
    await userEvent.type(screen.getByLabelText(/type the database name/i), "1");
    expect(confirm).toBeEnabled();

    await userEvent.click(confirm);
    expect(deleteDatabase).toHaveBeenCalledWith("d1");
    expect(onChanged).toHaveBeenCalled();
  });
});
