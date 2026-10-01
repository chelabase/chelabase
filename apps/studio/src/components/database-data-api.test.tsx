import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import type { BranchSummary, DatabaseSummary } from "@cheladb/types";
import { fakeApi } from "@/test/fake-api";
import { DatabaseDataApi } from "./database-data-api";

const summary: DatabaseSummary = { database_id: "db" as never, name: "web1", status: "ready", host: "h", port: 4432, branch_count: 2 };
const b = (name: string, over: Partial<BranchSummary> = {}): BranchSummary => ({
  id: `id-${name}` as never,
  name,
  parent_branch_id: name === "main" ? null : ("id-main" as never),
  kind: "normal",
  status: "ready",
  host: "h",
  port: 4432,
  branched_at_time: null,
  branched_at_lsn: null,
  created_at: "2026-09-30T10:00:00Z",
  history_start: null,
  error: null,
  ...over,
});
const keys = [
  { id: "pub", kind: "public", role: "anon", name: "public", prefix: "chk_pub_0123", created_at: "2026-10-01T09:00:00Z", last_used_at: null, key: "chk_pub_PUBKEY" },
];

describe("DatabaseDataApi", () => {
  it("shows the panel for main, and for the branch picked", async () => {
    const listKeys = vi.fn().mockResolvedValue(keys);
    const api = fakeApi({
      listBranches: vi.fn().mockResolvedValue([b("dev"), b("main"), b("bak", { kind: "backup" }), b("busy", { status: "provisioning" })]),
      listKeys,
    });
    render(<DatabaseDataApi api={api} apiUrl="http://gw" database={summary} />);
    expect(await screen.findByText("chk_pub_PUBKEY")).toBeInTheDocument();
    expect(listKeys).toHaveBeenLastCalledWith("db", "id-main");
    expect(screen.getByText("http://gw/data/v1/query")).toBeInTheDocument();
    const picker = screen.getByLabelText("Data API branch") as HTMLSelectElement;
    expect(Array.from(picker.options).map((o) => o.text)).toEqual(["dev", "main"]);
    expect(picker).toHaveValue("id-main");
    await userEvent.selectOptions(picker, "dev");
    await waitFor(() => expect(listKeys).toHaveBeenLastCalledWith("db", "id-dev"));
  });

  it("says when no branch is ready", async () => {
    const api = fakeApi({ listBranches: vi.fn().mockResolvedValue([b("main", { status: "provisioning" })]) });
    render(<DatabaseDataApi api={api} apiUrl="http://gw" database={summary} />);
    expect(await screen.findByText("No branch is ready yet.")).toBeInTheDocument();
  });

  it("shows a load error", async () => {
    const api = fakeApi({ listBranches: vi.fn().mockRejectedValue(new Error("gateway down")) });
    render(<DatabaseDataApi api={api} apiUrl="http://gw" database={summary} />);
    expect(await screen.findByRole("alert")).toHaveTextContent("gateway down");
  });
});
