import { screen, within } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import { fakeFetch, jsonResponse } from "@/test/fake-fetch";
import { devBranch, mainBranch, summaryOf } from "@/test/merge-fixtures";
import { renderWithRouter } from "@/test/render-router";
import { MergesList } from "./merges-list";

const LIST = "GET /v1/databases/db/merges";
const BRANCHES = "GET /v1/databases/db/branches";

describe("MergesList", () => {
  it("merges_list_renders_history", async () => {
    const f = fakeFetch({
      [LIST]: () =>
        jsonResponse([
          summaryOf({ id: "m2", status: "draft", created_at: "2026-10-06T11:00:00Z", updated_at: "2026-10-06T11:30:00Z" }),
          summaryOf({
            id: "m1",
            status: "done",
            direction: "from_parent",
            source_branch_id: "b-main",
            target_branch_id: "b-dev",
            created_at: "2026-10-05T09:00:00Z",
            updated_at: "2026-10-05T09:10:00Z",
          }),
        ]),
      [BRANCHES]: () => jsonResponse([mainBranch, devBranch]),
    });
    renderWithRouter(<MergesList projectId="p1" databaseId="db" />, { fetch: f });
    const table = await screen.findByRole("table", { name: "Merges" });
    const rows = within(table).getAllByRole("row").slice(1);
    expect(rows).toHaveLength(2);
    expect(within(rows[0]).getByRole("link", { name: "Merge dev into main" })).toHaveAttribute(
      "href",
      "/app/projects/p1/databases/db/merges/m2",
    );
    expect(rows[0]).toHaveTextContent("Draft");
    expect(rows[0]).toHaveTextContent("2026-10-06 11:00 UTC");
    expect(within(rows[1]).getByRole("link", { name: "Update dev from main" })).toBeInTheDocument();
    expect(rows[1]).toHaveTextContent("Done");
    expect(rows[1]).toHaveTextContent("2026-10-05 09:10 UTC");
  });

  it("says so when there are none", async () => {
    const f = fakeFetch({ [LIST]: () => jsonResponse([]), [BRANCHES]: () => jsonResponse([mainBranch]) });
    renderWithRouter(<MergesList projectId="p1" databaseId="db" />, { fetch: f });
    expect(await screen.findByText(/No merges yet/)).toBeInTheDocument();
  });
});
