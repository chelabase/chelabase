import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";
import type { MergeItem } from "@/lib/merges";
import { columnSnap } from "@/test/merge-fixtures";
import { MergeChanges } from "./merge-changes";

const tableSnap = (name: string) => ({ table: { oid: 5, schema: "public", name, rls_enabled: false } });

const items: MergeItem[] = [
  {
    id: "column:1:2",
    key: { kind: "column", schema: "public", table: "orders", name: "email" },
    source: { alter: { before: columnSnap("email"), after: columnSnap("email", "varchar(10)") } },
    target: null,
    outcome: "apply",
    data_loss: true,
    security: false,
  },
  {
    id: "table:name:schema:2200/audit",
    key: { kind: "table", schema: "public", table: null, name: "audit" },
    source: null,
    target: { create: tableSnap("audit") },
    outcome: "target_only",
    data_loss: false,
    security: false,
  },
  {
    id: "column:1:3",
    key: { kind: "column", schema: "public", table: "orders", name: "note" },
    source: { alter: { before: columnSnap("note"), after: columnSnap("note", "int4") } },
    target: { alter: { before: columnSnap("note"), after: columnSnap("note", "int8") } },
    outcome: { conflict: "c1" },
    data_loss: false,
    security: false,
  },
];

describe("MergeChanges", () => {
  it("names each side's text block after the object, for a screen reader", async () => {
    render(<MergeChanges analysis={{ items, conflicts: [], reported: [] }} sourceName="dev" targetName="main" />);
    const email = screen.getByRole("group", { name: /public\.orders\.email/ });
    await userEvent.click(within(email).getByText("public.orders.email"));
    expect(within(email).getByLabelText("Base (branch point) of public.orders.email")).toBeInTheDocument();
    expect(within(email).getByLabelText("main (target) of public.orders.email")).toBeInTheDocument();
    expect(within(email).getByLabelText("dev (source) of public.orders.email")).toBeInTheDocument();
  });

  it("changes_grouped_by_type_with_three_panes", async () => {
    render(
      <MergeChanges
        analysis={{ items, conflicts: [], reported: [] }}
        sourceName="dev"
        targetName="main"
      />,
    );
    // Grouped by object type, in the engine's order.
    const groups = screen.getAllByRole("heading", { level: 3 }).map((h) => h.textContent);
    expect(groups).toEqual(["Tables (1)", "Columns (2)"]);

    const email = screen.getByRole("group", { name: /public\.orders\.email/ });
    expect(email).toHaveTextContent("Applies");
    expect(email).toHaveTextContent("Loses data");
    expect(email).toHaveTextContent("dev: altered");
    expect(email).toHaveTextContent("main: unchanged");
    await userEvent.click(within(email).getByText("public.orders.email"));
    const base = within(email).getByRole("figure", { name: "Base (branch point)" });
    const target = within(email).getByRole("figure", { name: "main (target)" });
    const source = within(email).getByRole("figure", { name: "dev (source)" });
    expect(base).toHaveTextContent('"type": "text"');
    expect(target).toHaveTextContent('"type": "text"');
    expect(source).toHaveTextContent('"type": "varchar(10)"');

    const note = screen.getByRole("group", { name: /public\.orders\.note/ });
    expect(note).toHaveTextContent("Conflict");
    const audit = screen.getByRole("group", { name: /public\.audit/ });
    expect(audit).toHaveTextContent("Target only");
    await userEvent.click(within(audit).getByText("public.audit"));
    // Created on the target only: the base and the source don't have it.
    expect(within(audit).getByRole("figure", { name: "Base (branch point)" })).toHaveTextContent("—");
    expect(within(audit).getByRole("figure", { name: "dev (source)" })).toHaveTextContent("—");
  });

  it("lists the objects a merge doesn't move", () => {
    render(
      <MergeChanges
        analysis={{
          items: [],
          conflicts: [],
          reported: [
            {
              object: { kind: "function", oid: 9, schema: "public", name: "touch", parent: null, arguments: "", definition: null },
              change: "created",
              needed_by: ["column:1:2"],
            },
          ],
        }}
        sourceName="dev"
        targetName="main"
      />,
    );
    const reported = screen.getByRole("region", { name: "Not merged" });
    expect(reported).toHaveTextContent("function public.touch: created on dev");
    expect(reported).toHaveTextContent("needed by column:1:2");
    expect(screen.getByText(/No schema changes/)).toBeInTheDocument();
  });
});
