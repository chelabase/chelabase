import { screen, within } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import { T1, mountPage } from "./test-setup";

describe("TeamPage crumb label", () => {
  it("shows Members > Teams > the team's name, and the loading mark before the data arrives", async () => {
    mountPage(T1, { crumbs: true });
    const trail = await screen.findByRole("navigation", { name: "Breadcrumb" });
    expect(trail).toHaveTextContent("MembersTeams…");
    expect(await within(trail).findByText("Billing")).toHaveAttribute("aria-current", "page");
    expect(within(trail).getByRole("link", { name: "Teams" })).toBeInTheDocument();
  });
});
