import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import type { DatabaseDetail } from "@cheladb/types";
import { ApiError, type ApiClient } from "@/lib/api";
import { CreateDatabaseForm } from "./create-database-form";

function detail(status: DatabaseDetail["status"], error: string | null = null): DatabaseDetail {
  return {
    database_id: "d1" as never,
    project_id: "p1" as never,
    tenant_id: "t1" as never,
    name: "web1",
    status,
    host: null,
    port: null,
    error,
  };
}

function setup(api: Partial<ApiClient>) {
  const onSettled = vi.fn();
  render(
    <CreateDatabaseForm
      api={{
        me: vi.fn(),
        createDatabase: vi.fn(),
        getDatabase: vi.fn(),
        getConnection: vi.fn(),
        resetPassword: vi.fn(),
        deleteDatabase: vi.fn(),
        ...api,
      }}
      tenantId={"t1" as never}
      projectId={"p1" as never}
      onSettled={onSettled}
      pollIntervalMs={0}
    />,
  );
  return { onSettled };
}

// Review Focus 3.
describe("CreateDatabaseForm", () => {
  it("creates with the workspace ids, trims the name and polls until ready", async () => {
    const createDatabase = vi.fn().mockResolvedValue({ job_id: "j", database_id: "d1", status: "pending" });
    const getDatabase = vi
      .fn()
      .mockResolvedValueOnce(detail("provisioning"))
      .mockResolvedValueOnce(detail("ready"));
    const { onSettled } = setup({ createDatabase, getDatabase });

    await userEvent.type(screen.getByLabelText("Database name"), "  web1  ");
    await userEvent.click(screen.getByRole("button", { name: "Create database" }));

    expect(await screen.findByText("web1 is ready")).toBeInTheDocument();
    expect(createDatabase).toHaveBeenCalledWith({ tenant_id: "t1", project_id: "p1", name: "web1" });
    expect(getDatabase).toHaveBeenCalledTimes(2);
    expect(onSettled).toHaveBeenCalled();
  });

  it("does not submit a blank name", async () => {
    const createDatabase = vi.fn();
    setup({ createDatabase });
    await userEvent.type(screen.getByLabelText("Database name"), "   ");
    expect(screen.getByRole("button", { name: "Create database" })).toBeDisabled();
    expect(createDatabase).not.toHaveBeenCalled();
  });

  it("shows a provisioning failure", async () => {
    const createDatabase = vi.fn().mockResolvedValue({ job_id: "j", database_id: "d1", status: "pending" });
    const getDatabase = vi.fn().mockResolvedValue(detail("failed", "docker: image pull failed"));
    setup({ createDatabase, getDatabase });
    await userEvent.type(screen.getByLabelText("Database name"), "web1");
    await userEvent.click(screen.getByRole("button", { name: "Create database" }));
    expect(await screen.findByRole("alert")).toHaveTextContent("docker: image pull failed");
  });

  it("shows the API's rejection", async () => {
    const createDatabase = vi
      .fn()
      .mockRejectedValue(new ApiError(409, "database name already exists in project"));
    setup({ createDatabase });
    await userEvent.type(screen.getByLabelText("Database name"), "web1");
    await userEvent.click(screen.getByRole("button", { name: "Create database" }));
    expect(await screen.findByRole("alert")).toHaveTextContent("already exists");
  });
});
