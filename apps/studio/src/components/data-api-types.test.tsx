import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type { BranchDatabaseView } from "@chelabase/management-client";

const downloadTypes = vi.fn(async () => {});
vi.mock("@/lib/types-download", () => ({ downloadTypes: (...args: unknown[]) => downloadTypes(...(args as [])) }));

import { DataApiTypes } from "./data-api-types";

const bdb = (name: string, main = false): BranchDatabaseView =>
  ({ name, data_api: "on", data_api_error: null, default_schema: "public", main, origin: main ? "main" : "sql", owner: "o" }) as BranchDatabaseView;

beforeEach(() => downloadTypes.mockClear());

describe("DataApiTypes database choice", () => {
  it("downloads main by default, a named database, and every database as separate kinds", async () => {
    const user = userEvent.setup();
    render(<DataApiTypes databaseId="d1" branchId="b1" databases={[bdb("web1", true), bdb("*all*"), bdb("shop")]} />);
    const button = screen.getByRole("button", { name: "Download types" });
    await user.click(button);
    expect(downloadTypes).toHaveBeenLastCalledWith("d1", "b1", { role: "authenticated", database: { kind: "main" } });
    // A database literally named `*all*` is a named database, not "all".
    await user.selectOptions(screen.getByLabelText("Types database"), screen.getByRole("option", { name: "*all*" }));
    await user.click(button);
    expect(downloadTypes).toHaveBeenLastCalledWith("d1", "b1", { role: "authenticated", database: { kind: "named", name: "*all*" } });
    await user.selectOptions(screen.getByLabelText("Types database"), screen.getByRole("option", { name: "All databases" }));
    await user.click(button);
    expect(downloadTypes).toHaveBeenLastCalledWith("d1", "b1", { role: "authenticated", database: { kind: "all" } });
  });
});
