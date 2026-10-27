import { screen, within } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { fakeFetch } from "@/test/fake-fetch";
import { renderWithCrumbs } from "@/test/render-crumbs";
import { FULL, editorRoutes, stubLayout } from "@/test/table-editor-fixtures";
import { TableEditor } from "./table-editor";

beforeEach(() => { localStorage.clear(); sessionStorage.clear(); stubLayout(); });

describe("TableEditor crumb label", () => {
  it("shows the table's name once the objects list has it, the loading mark before", async () => {
    renderWithCrumbs(
      <TableEditor params={FULL} navigate={vi.fn()} />,
      { path: "/app/tables/$projectId/$databaseId/$branchId/$pgDatabase/$schema/$table", at: "/app/tables/p1/d1/b1/app/public/invoices" },
      { fetch: fakeFetch(editorRoutes()) },
    );
    const trail = await screen.findByRole("navigation", { name: "Breadcrumb" });
    expect(trail).toHaveTextContent("…");
    expect(await within(trail).findByText("invoices")).toHaveAttribute("aria-current", "page");
  });
});
