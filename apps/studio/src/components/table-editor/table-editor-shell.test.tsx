import { screen, within } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { FULL, meAs, renderEditor, stubLayout } from "@/test/table-editor-fixtures";
import { LAST_KEY } from "@/lib/table-editor/context";

describe("TableEditor shell", () => {
  beforeEach(() => { localStorage.clear(); sessionStorage.clear(); stubLayout(); });

  it("redirects_missing_levels_with_replace", async () => {
    const { navigate } = renderEditor({ params: {} });
    await vi.waitFor(() => expect(navigate).toHaveBeenCalledWith({ projectId: "p1", databaseId: "d1" }, { replace: true }));
  });

  it("shows_the_four_pickers_and_act_as", async () => {
    renderEditor();
    expect(await screen.findByRole("combobox", { name: "Project" })).toHaveValue("Acme shop");
    expect(screen.getByRole("combobox", { name: "Database" })).toHaveValue("orders-db");
    expect(await screen.findByRole("combobox", { name: "Branch" })).toHaveValue("main");
    expect(await screen.findByRole("combobox", { name: "Postgres db" })).toHaveValue("app");
    expect(await screen.findByRole("combobox", { name: "Act as" })).toHaveValue("owner");
  });

  it("picking_a_branch_drops_the_levels_after_it", async () => {
    const { user, navigate } = renderEditor();
    await user.click(await screen.findByLabelText("Branch"));
    expect(screen.queryByRole("option", { name: /^backup-1/ })).toBeNull();
    expect(screen.queryByRole("option", { name: /^broken/ })).toBeNull();
    expect(screen.getByRole("option", { name: /New branch from main/ })).toBeInTheDocument();
    await user.click(await screen.findByRole("option", { name: /^feat-discounts/ }));
    expect(navigate).toHaveBeenLastCalledWith({ projectId: "p1", databaseId: "d1", branchId: "b3" });
  });

  it("picking_a_project_drops_everything_after_it", async () => {
    const { user, navigate } = renderEditor();
    await user.click(await screen.findByLabelText("Project"));
    await user.click(await screen.findByRole("option", { name: /^Demo/ }));
    expect(navigate).toHaveBeenLastCalledWith({ projectId: "p2" });
  });

  it("postgres_db_options_mark_the_data_api", async () => {
    const { user } = renderEditor();
    await user.click(await screen.findByLabelText("Postgres db"));
    expect(await screen.findByRole("option", { name: /^app.*data API/ })).toBeInTheDocument();
    expect(screen.getByRole("option", { name: /^reporting/ })).not.toHaveTextContent("data API");
  });

  it("remembers_the_last_context_when_ready", async () => {
    renderEditor();
    await screen.findByRole("combobox", { name: "Branch" });
    await vi.waitFor(() => expect(JSON.parse(localStorage.getItem(LAST_KEY)!)).toEqual(FULL));
  });

  it("table_list_shows_the_schema_and_filters", async () => {
    const { user, navigate } = renderEditor({ params: { ...FULL, table: undefined } });
    const list = await screen.findByRole("navigation", { name: "Tables" });
    expect(await within(list).findByRole("button", { name: "invoices" })).toBeInTheDocument();
    expect(within(list).getByRole("button", { name: "v_open" })).toBeInTheDocument();
    expect(within(list).queryByRole("button", { name: "ledger" })).toBeNull();
    expect(screen.getByText("Pick a table")).toBeInTheDocument();
    await user.type(within(list).getByLabelText("Filter tables"), "cust");
    expect(within(list).queryByRole("button", { name: "invoices" })).toBeNull();
    await user.click(within(list).getByRole("button", { name: "customers" }));
    expect(navigate).toHaveBeenLastCalledWith({ ...FULL, table: "customers" });
    expect(within(list).getByRole("button", { name: "Types" })).toBeInTheDocument();
    expect(within(list).getByRole("button", { name: "New table" })).toBeInTheDocument();
  });

  it("switching_schema_drops_the_table", async () => {
    const { user, navigate } = renderEditor();
    const list = await screen.findByRole("navigation", { name: "Tables" });
    await user.click(within(list).getByLabelText("Schema"));
    await user.click(await screen.findByRole("option", { name: "billing" }));
    expect(navigate).toHaveBeenLastCalledWith({ ...FULL, schema: "billing", table: undefined });
  });

  it("not_found_keeps_the_pickers", async () => {
    renderEditor({ params: { ...FULL, branchId: "zz", pgDatabase: undefined, schema: undefined, table: undefined } });
    expect(await screen.findByText("Branch not found")).toBeInTheDocument();
    expect(screen.getByRole("combobox", { name: "Project" })).toBeInTheDocument();
  });

  it("protected_branch_reason_for_a_developer", async () => {
    renderEditor({ me: meAs("developer", ["billing_admin"]) });
    expect(await screen.findByTestId("structure-reason")).toHaveTextContent("main is protected: schema changes need Admin");
  });

  it("act_as_lists_only_allowed_roles", async () => {
    const { user } = renderEditor({ me: meAs("developer", ["billing_admin"]) });
    expect(await screen.findByRole("combobox", { name: "Act as" })).toHaveValue("billing_admin");
    await user.click(screen.getByLabelText("Act as"));
    expect(await screen.findByRole("option", { name: "billing_admin" })).toBeInTheDocument();
    expect(screen.queryByRole("option", { name: "owner" })).toBeNull();
    expect(screen.queryByRole("option", { name: "reporter" })).toBeNull();
  });
});
