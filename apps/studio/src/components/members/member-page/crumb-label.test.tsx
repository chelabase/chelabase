import { screen, within } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import { MARA, mount, sofiaFor } from "./test-setup";

const trail = () => screen.getByRole("navigation", { name: "Breadcrumb" });
const shown = () => screen.findByRole("navigation", { name: "Breadcrumb" });

describe("MemberPage crumb label", () => {
  it("shows the member's name, and the loading mark before the data arrives", async () => {
    mount({ crumbs: true });
    expect(await shown()).toHaveTextContent("Members…");
    expect(await within(trail()).findByText("Sofia Marchetti")).toHaveAttribute("aria-current", "page");
    expect(trail()).not.toHaveTextContent("…");
  });

  it("falls back to the email's local part without a display name", async () => {
    mount({ crumbs: true, members: () => [sofiaFor("tenant_admin", { display_name: undefined })] });
    expect(await within(await shown()).findByText("sofia")).toBeInTheDocument();
  });

  it("stays at the loading mark when the member is not found", async () => {
    mount({ crumbs: true, memberId: MARA, members: () => [sofiaFor("tenant_admin")] });
    expect(await screen.findByText(/not found|isn't|can't find/i)).toBeInTheDocument();
    expect(trail()).toHaveTextContent("…");
    expect(trail()).not.toHaveTextContent("Mara");
  });
});
