import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { ConnectionDetails, DatabaseSummary } from "@cheladb/types";
import { ApiError, type ApiClient } from "@/lib/api";
import { fakeApi } from "@/test/fake-api";
import { DatabaseActions } from "./database-actions";

const ready: DatabaseSummary = {
  database_id: "d1" as never,
  name: "web1",
  status: "ready",
  host: "127.0.0.1",
  port: 49153,
  branch_count: 1,
};

const EP = "ep-0123456789ab.localtest.me";
const HTTP_URL = `https://${EP}:4444/sql`;
const uriFor = (password: string) => `postgresql://postgres:${password}@${EP}:4432/web1?sslmode=verify-full`;

const details = (password: string): ConnectionDetails => ({
  host: EP,
  port: 4432,
  database: "web1",
  user: "postgres",
  password,
  uri: uriFor(password),
  http_url: HTTP_URL,
});

function setup(api: Partial<ApiClient>, database: DatabaseSummary = ready) {
  const onChanged = vi.fn();
  const full: ApiClient = fakeApi(api);
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
    expect(screen.getByText(uriFor("s3cret"))).toBeInTheDocument();
  });

  it("copies the connection URI", async () => {
    const writeText = vi.fn().mockResolvedValue(undefined);
    vi.stubGlobal("navigator", { ...navigator, clipboard: { writeText } });
    setup({ getConnection: vi.fn().mockResolvedValue(details("s3cret")) });
    await userEvent.click(screen.getByRole("button", { name: "Connection details" }));
    await userEvent.click(await screen.findByRole("button", { name: "Copy URI" }));
    expect(writeText).toHaveBeenCalledWith(uriFor("s3cret"));
  });

  it("shows_the_http_url_without_the_password", async () => {
    setup({ getConnection: vi.fn().mockResolvedValue(details("s3cret")) });
    await userEvent.click(screen.getByRole("button", { name: "Connection details" }));
    expect(await screen.findByText(HTTP_URL)).toBeInTheDocument();
    expect(screen.getByText("HTTP URL")).toBeInTheDocument();
    expect(screen.queryByText("s3cret")).toBeNull();
    expect(screen.queryByText(/postgresql:\/\//)).toBeNull();
  });

  it("copies_the_http_url", async () => {
    const writeText = vi.fn().mockResolvedValue(undefined);
    vi.stubGlobal("navigator", { ...navigator, clipboard: { writeText } });
    setup({ getConnection: vi.fn().mockResolvedValue(details("s3cret")) });
    await userEvent.click(screen.getByRole("button", { name: "Connection details" }));
    await userEvent.click(await screen.findByRole("button", { name: "Copy HTTP URL" }));
    expect(writeText).toHaveBeenCalledWith(HTTP_URL);
  });

  it("shows_the_local_dev_note", async () => {
    setup({ getConnection: vi.fn().mockResolvedValue(details("s3cret")) });
    await userEvent.click(screen.getByRole("button", { name: "Connection details" }));
    const note = await screen.findByText(/Local dev: pass the proxy's CA with/);
    expect(note).toHaveTextContent(
      "Local dev: pass the proxy's CA with sslrootcert=.cheladb/neon/proxy-ca.crt. If localtest.me resolves to ::1 first on your machine, add hostaddr=127.0.0.1.",
    );
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
      ["host", EP],
      ["port", "4432"],
      ["database", "web1"],
      ["user", "postgres"],
      ["password", "s3cret"],
      ["HTTP URL", HTTP_URL],
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

  it("shows one message and the stored password after a reset that isn't applied yet", async () => {
    const pending = "the password was changed but not yet applied; it will be retried";
    const resetPassword = vi.fn().mockRejectedValue(new ApiError(503, pending));
    const getConnection = vi.fn().mockResolvedValue(details("n3w"));
    setup({ resetPassword, getConnection });

    await userEvent.click(screen.getByRole("button", { name: "Reset password" }));
    await userEvent.click(screen.getByRole("button", { name: "Confirm reset" }));

    const notice = "The new password is stored but not yet applied; it will be retried.";
    expect(await screen.findByRole("status")).toHaveTextContent(notice);
    expect(getConnection).toHaveBeenCalledWith("d1");
    // Not "reset" beside "not applied": one coherent message.
    expect(screen.queryByText(/password was reset/i)).toBeNull();
    expect(screen.queryByRole("alert")).toBeNull();
    await userEvent.click(await screen.findByRole("button", { name: "Show password" }));
    expect(screen.getByText("n3w")).toBeInTheDocument();
    expect(screen.getByRole("status")).toHaveTextContent(notice);
  });

  it("keeps the not-applied message when the details can't be loaded", async () => {
    const pending = "the password was changed but not yet applied; it will be retried";
    const resetPassword = vi.fn().mockRejectedValue(new ApiError(503, pending));
    const getConnection = vi.fn().mockRejectedValue(new ApiError(500, "boom"));
    setup({ resetPassword, getConnection });

    await userEvent.click(screen.getByRole("button", { name: "Reset password" }));
    await userEvent.click(screen.getByRole("button", { name: "Confirm reset" }));

    const alert = await screen.findByRole("alert");
    expect(alert).toHaveTextContent("The new password is stored but not yet applied; it will be retried.");
    expect(alert).toHaveTextContent("boom");
  });

  it("doesn't reveal after other reset failures", async () => {
    const resetPassword = vi.fn().mockRejectedValue(new ApiError(503, "worker unavailable"));
    const getConnection = vi.fn();
    setup({ resetPassword, getConnection });
    await userEvent.click(screen.getByRole("button", { name: "Reset password" }));
    await userEvent.click(screen.getByRole("button", { name: "Confirm reset" }));
    expect(await screen.findByRole("alert")).toHaveTextContent("worker unavailable");
    expect(getConnection).not.toHaveBeenCalled();
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
