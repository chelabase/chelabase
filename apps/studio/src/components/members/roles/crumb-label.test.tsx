import { screen, within } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import { BIDI, BIDI_ESCAPED, mountPage, role } from "./test-setup";

const trail = () => screen.getByRole("navigation", { name: "Breadcrumb" });
const shown = () => screen.findByRole("navigation", { name: "Breadcrumb" });

describe("RolePage crumb label", () => {
  it("shows the role's name, and the loading mark before the data arrives", async () => {
    mountPage({ crumbs: true });
    expect(await shown()).toHaveTextContent("…");
    expect(await within(trail()).findByText("billing_editor")).toHaveAttribute("aria-current", "page");
    expect(trail()).not.toHaveTextContent("…");
  });

  it("escapes the name like the heading", async () => {
    mountPage({ crumbs: true, role: BIDI, roles: [role("chelabase_owner", { origin: "owner" }), role(BIDI)] });
    expect(await within(await shown()).findByText(BIDI_ESCAPED)).toBeInTheDocument();
    expect(trail().textContent).not.toContain("‮");
  });

  it("registers nothing when the role is not on the branch", async () => {
    mountPage({ crumbs: true, role: "ghost" });
    expect(await screen.findByText(/ghost.*not found/i)).toBeInTheDocument();
    expect(trail()).toHaveTextContent("…");
    expect(trail()).not.toHaveTextContent("ghost");
  });
});
