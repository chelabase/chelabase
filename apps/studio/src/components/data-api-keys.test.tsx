import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { CreatedDataKey, DataKey } from "@cheladb/types";
import { ApiError, type ApiClient } from "@/lib/api";
import { fakeApi } from "@/test/fake-api";
import { DataApiKeys } from "./data-api-keys";

const key = (over: Partial<DataKey> & { id: string; name: string }): DataKey => ({
  kind: "secret",
  role: "authenticated",
  prefix: "chk_sec_ab12",
  created_at: "2026-10-01T09:00:00Z",
  last_used_at: null,
  ...over,
});
const publicKey = key({ id: "pub", name: "public", kind: "public", role: "anon", prefix: "chk_pub_zz99", key: "chk_pub_secretvalue" });
const reporting = key({ id: "k1", name: "reporting", last_used_at: "2026-10-01T10:30:00Z" });
const worker = key({ id: "k2", name: "worker", role: "service" });
const FULL = "chk_sec_ab12CDEFGHIJKLMNOPQRSTUVWXYZ0123456789abcde";
const created = (over: Partial<CreatedDataKey> = {}): CreatedDataKey => ({
  id: "k3",
  kind: "secret",
  role: "authenticated",
  name: "ci",
  prefix: "chk_sec_ab12",
  key: FULL,
  ...over,
});

function setup(api: Partial<ApiClient> = {}, keys: DataKey[] = [publicKey, reporting, worker]) {
  const onChanged = vi.fn();
  const full = fakeApi(api);
  render(<DataApiKeys api={full} databaseId="db" branchId="br" keys={keys} onChanged={onChanged} />);
  return { api: full, onChanged };
}

afterEach(() => vi.unstubAllGlobals());

describe("DataApiKeys", () => {
  it("lists the secret keys with prefix, role and last used, and leaves the public key out", () => {
    setup();
    const row = screen.getByRole("row", { name: /reporting/ });
    expect(within(row).getByText("chk_sec_ab12…")).toBeInTheDocument();
    expect(within(row).getByText("authenticated")).toBeInTheDocument();
    expect(within(row).getByText("2026-10-01 10:30 UTC")).toBeInTheDocument();
    expect(within(screen.getByRole("row", { name: /worker/ })).getByText("never")).toBeInTheDocument();
    expect(screen.queryByText("chk_pub_zz99…")).not.toBeInTheDocument();
  });

  it("says when there are no secret keys", () => {
    setup({}, [publicKey]);
    expect(screen.getByText("No secret keys yet.")).toBeInTheDocument();
  });

  it("creates a key with the default role and shows it once, with copy", async () => {
    const writeText = vi.fn().mockResolvedValue(undefined);
    vi.stubGlobal("navigator", { ...navigator, clipboard: { writeText } });
    const createKey = vi.fn().mockResolvedValue(created());
    const { onChanged } = setup({ createKey });
    expect(screen.getByLabelText("Role")).toHaveValue("authenticated");
    await userEvent.type(screen.getByLabelText("Key name"), "ci");
    await userEvent.click(screen.getByRole("button", { name: "Create key" }));
    expect(createKey).toHaveBeenCalledWith("db", "br", { name: "ci", role: "authenticated" });
    const box = await screen.findByRole("region", { name: "New key" });
    expect(within(box).getByText(FULL)).toBeInTheDocument();
    expect(within(box).getByText("Copy this key now. It won't be shown again.")).toBeInTheDocument();
    await userEvent.click(within(box).getByRole("button", { name: "Copy key" }));
    expect(writeText).toHaveBeenCalledWith(FULL);
    expect(onChanged).toHaveBeenCalled();
    expect(screen.getByLabelText("Key name")).toHaveValue("");
    await userEvent.click(within(box).getByRole("button", { name: "Dismiss" }));
    expect(screen.queryByText(FULL)).not.toBeInTheDocument();
  });

  it("sends the role the user picked", async () => {
    const createKey = vi.fn().mockResolvedValue(created({ role: "service" }));
    setup({ createKey });
    await userEvent.type(screen.getByLabelText("Key name"), "ci");
    await userEvent.clear(screen.getByLabelText("Role"));
    await userEvent.type(screen.getByLabelText("Role"), "service");
    await userEvent.click(screen.getByRole("button", { name: "Create key" }));
    expect(createKey).toHaveBeenCalledWith("db", "br", { name: "ci", role: "service" });
  });

  it("disables Create until there is a name, and says why", async () => {
    setup();
    const button = screen.getByRole("button", { name: "Create key" });
    expect(button).toBeDisabled();
    const reason = screen.getByText("Enter a name for the key.");
    expect(button.getAttribute("aria-describedby")).toBe(reason.id);
    await userEvent.type(screen.getByLabelText("Key name"), "ci");
    expect(button).toBeEnabled();
    expect(button).not.toHaveAttribute("aria-describedby");
  });

  it("shows the worker's message on a 422 and no key", async () => {
    const createKey = vi.fn().mockRejectedValue(new ApiError(422, "role \"postgres\" is a superuser"));
    const { onChanged } = setup({ createKey });
    await userEvent.type(screen.getByLabelText("Key name"), "ci");
    await userEvent.click(screen.getByRole("button", { name: "Create key" }));
    expect(await screen.findByRole("alert")).toHaveTextContent('role "postgres" is a superuser');
    expect(screen.queryByRole("region", { name: "New key" })).not.toBeInTheDocument();
    expect(onChanged).not.toHaveBeenCalled();
    expect(screen.getByLabelText("Key name")).toHaveValue("ci");
  });

  it("says why Revoke and Rotate are disabled while a key change runs", async () => {
    setup({ revokeKey: vi.fn().mockReturnValue(new Promise<never>(() => {})) });
    const revoke = () => screen.getByRole("button", { name: "Revoke worker" });
    const rotate = () => screen.getByRole("button", { name: "Rotate reporting" });
    expect(revoke()).not.toHaveAttribute("aria-describedby");
    await userEvent.click(screen.getByRole("button", { name: "Revoke reporting" }));
    await userEvent.click(screen.getByRole("button", { name: "Confirm revoke" }));
    for (const b of [revoke(), rotate()]) {
      expect(b).toBeDisabled();
      expect(b).toHaveAccessibleDescription("Another key change is still running.");
    }
  });

  it("revokes only after a confirm", async () => {
    const revokeKey = vi.fn().mockResolvedValue(undefined);
    const { onChanged } = setup({ revokeKey });
    await userEvent.click(screen.getByRole("button", { name: "Revoke reporting" }));
    expect(revokeKey).not.toHaveBeenCalled();
    expect(screen.getByText(/Revoke reporting\? Anything using it stops working within 30 seconds\./)).toBeInTheDocument();
    await userEvent.click(screen.getByRole("button", { name: "Confirm revoke" }));
    expect(revokeKey).toHaveBeenCalledWith("db", "br", "k1");
    await vi.waitFor(() => expect(onChanged).toHaveBeenCalled());
    expect(screen.queryByRole("button", { name: "Confirm revoke" })).not.toBeInTheDocument();
  });

  it("cancel leaves the key alone", async () => {
    const revokeKey = vi.fn();
    setup({ revokeKey });
    await userEvent.click(screen.getByRole("button", { name: "Revoke reporting" }));
    await userEvent.click(screen.getByRole("button", { name: "Cancel" }));
    expect(revokeKey).not.toHaveBeenCalled();
    expect(screen.queryByRole("button", { name: "Confirm revoke" })).not.toBeInTheDocument();
  });

  it("rotates after a confirm and shows the new key once", async () => {
    const rotateKey = vi.fn().mockResolvedValue(created({ id: "k9", name: "reporting" }));
    const { onChanged } = setup({ rotateKey });
    await userEvent.click(screen.getByRole("button", { name: "Rotate reporting" }));
    expect(rotateKey).not.toHaveBeenCalled();
    await userEvent.click(screen.getByRole("button", { name: "Confirm rotate" }));
    expect(rotateKey).toHaveBeenCalledWith("db", "br", "k1");
    expect(await screen.findByRole("region", { name: "New key" })).toHaveTextContent(FULL);
    expect(onChanged).toHaveBeenCalled();
  });

  it("shows a revoke failure", async () => {
    const revokeKey = vi.fn().mockRejectedValue(new ApiError(404, "key not found"));
    setup({ revokeKey });
    await userEvent.click(screen.getByRole("button", { name: "Revoke reporting" }));
    await userEvent.click(screen.getByRole("button", { name: "Confirm revoke" }));
    expect(await screen.findByRole("alert")).toHaveTextContent("key not found");
  });
});
