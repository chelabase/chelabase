import { screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import { mount, sofiaFor } from "./test-setup";

/** Fix S3 (finish review round 2, item 5): the suspended member page. Pixel layout is checked in the recapture. */

async function suspended() {
  mount({ caller: "tenant_admin", members: () => [sofiaFor("tenant_admin", { status: "suspended" })] });
  const lead = await screen.findByText(/^Suspended on /);
  return { lead, banner: lead.closest("div")! };
}

describe("suspend banner (fix S3)", () => {
  it("is_full_width_and_tinted", async () => {
    const { banner } = await suspended();
    expect(banner.className).not.toMatch(/max-w-/);
    expect(banner.className).toMatch(/bg-coral\//);
  });

  it("has_a_square_marker", async () => {
    const { banner } = await suspended();
    const marker = banner.querySelector<HTMLElement>('[aria-hidden="true"]');
    expect(marker).not.toBeNull();
    expect(marker!.className).toMatch(/bg-coral|border-coral/);
    expect(marker!.className).not.toMatch(/rounded/);
  });

  it("the_Suspended_on_lead_in_is_bold", async () => {
    const { lead } = await suspended();
    expect(lead.tagName === "STRONG" || /font-(semibold|bold)/.test(lead.className)).toBe(true);
    expect(lead).toHaveTextContent(/^Suspended on .+ by .+\.$/);
  });

  it("project_access_says_the_access_is_kept_and_when_changes_apply", async () => {
    await suspended();
    expect(screen.getByText("Kept while suspended. Changes apply when Sofia is reinstated.")).toBeInTheDocument();
  });

  it("an_active_member_keeps_the_usual_subtitle", async () => {
    mount({ caller: "tenant_admin" });
    await screen.findByRole("heading", { level: 2, name: "Project access" });
    expect(screen.queryByText(/Kept while suspended/)).toBeNull();
  });
});
