import { cleanup, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { useState } from "react";
import { afterEach, describe, expect, it } from "vitest";
import { buildCrumbs } from "@/lib/crumbs";
import { renderWithCrumbs } from "@/test/render-crumbs";
import { useCrumbLabel } from "./crumbs";

afterEach(cleanup);

const PROJECT = { path: "/app/projects/$projectId", at: "/app/projects/p1" };

function Label({ param, id, label }: { param: string; id: string | undefined; label: string | undefined }) {
  useCrumbLabel(param, id, label);
  return null;
}

describe("buildCrumbs edges", () => {
  it("a role page without branch or database params leaves them out of the Roles search", () => {
    const cs = buildCrumbs("/app/members/roles/x/y/r", { role: "r" }, { role: { id: "r", label: "r" } });
    expect(cs[1].search).toEqual({ tab: "roles" });
  });

  it("a table label for another table shows '…'", () => {
    const cs = buildCrumbs("/app/tables/p/d/b/pg/public/b", { table: "b" }, { table: { id: "a", label: "a" } });
    expect(cs[1]).toMatchObject({ label: "…", loading: true });
  });

  it("a project path with no projectId param never links to an empty id", () => {
    const cs = buildCrumbs("/app/projects/x", {}, {});
    expect(cs[1].to).toBeUndefined();
  });
});

describe("label registration edges", () => {
  it("a second label for the same key wins; removing the first keeps the second", async () => {
    const user = userEvent.setup();
    function Page() {
      const [first, setFirst] = useState(true);
      return (
        <>
          <button onClick={() => setFirst(false)}>drop first</button>
          {first && <Label param="projectId" id="p1" label="First" />}
          <Label param="projectId" id="p1" label="Second" />
        </>
      );
    }
    renderWithCrumbs(<Page />, PROJECT);
    const nav = await screen.findByRole("navigation", { name: "Breadcrumb" });
    expect(await within(nav).findByText("Second")).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "drop first" }));
    expect(within(nav).getByText("Second")).toBeInTheDocument();
  });

  it("markup in a label is rendered as text", async () => {
    renderWithCrumbs(<Label param="projectId" id="p1" label="<b>x</b>" />, PROJECT);
    const nav = await screen.findByRole("navigation", { name: "Breadcrumb" });
    expect(await within(nav).findByText("<b>x</b>")).toBeInTheDocument();
    expect(nav.querySelector("b")).toBeNull();
  });
});

describe("crumb lettering", () => {
  it("last_crumb_is_regular_weight_and_identifier_crumbs_are_mono", async () => {
    renderWithCrumbs(<Label param="role" id="billing_editor" label="billing_editor" />, {
      path: "/app/members/roles/$databaseId/$branchId/$role",
      at: "/app/members/roles/11111111-1111-4111-8111-111111111111/22222222-2222-4222-8222-222222222222/billing_editor",
    });
    const n = await screen.findByRole("navigation", { name: "Breadcrumb" });
    const last = await within(n).findByText("billing_editor");
    expect(last.className).not.toContain("font-medium");
    expect(last.className).toContain("font-mono");
    expect(within(n).getByRole("link", { name: "Members" }).className).not.toContain("font-mono");
  });
});
