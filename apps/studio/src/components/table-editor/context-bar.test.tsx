import { render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { BRANCHES, FULL, PG_DATABASES, PROJECTS } from "@/test/table-editor-fixtures";
import { ContextBar, type ContextBarProps } from "./context-bar";

const props = (over: Partial<ContextBarProps> = {}): ContextBarProps => ({
  params: FULL,
  navigate: vi.fn(),
  projects: PROJECTS as never,
  branches: BRANCHES as never,
  pgDatabases: PG_DATABASES as never,
  actAs: { owner: true, roles: ["billing_admin"], selected: null },
  onActAs: vi.fn(),
  historyWindow: 604800,
  ...over,
});

describe("ContextBar levels", () => {
  it("shows_the_place_pickers_and_act_as_but_no_schema_or_table_picker", () => {
    render(<ContextBar {...props()} />);
    for (const label of ["Project", "Database", "Branch", "Postgres db", "Act as"]) expect(screen.getByLabelText(label)).toBeInTheDocument();
    expect(screen.queryByLabelText("Schema")).toBeNull();
    expect(screen.queryByLabelText("Table")).toBeNull();
  });
});
