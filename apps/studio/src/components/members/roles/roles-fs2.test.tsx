import { screen, within } from "@testing-library/react";
import { beforeEach, describe, expect, it } from "vitest";
import { mountPage, mountTab } from "./test-setup";

/** Fix S2 (recapture at 390 px): the role page and the Roles tab. Real overflow and overlap are checked in the recapture. */

beforeEach(() => {
  localStorage.clear();
  sessionStorage.clear();
});

const SCROLL = /(^|\s)overflow(-x)?-(auto|scroll)(\s|$)/;
const POSITIONED = /(^|\s)(relative|absolute|fixed|sticky)(\s|$)/;

/**
 * Every `sr-only` span is absolutely positioned, so inside a scroll frame some ancestor from its parent up to
 * the frame must be positioned; otherwise its containing block lies outside the scroll box and widens the page.
 */
function srOnlyContained(root: HTMLElement): number {
  const spans = [...root.querySelectorAll<HTMLElement>(".sr-only")];
  for (const span of spans) {
    let positioned = false;
    for (let el = span.parentElement; el && el !== document.body; el = el.parentElement) {
      positioned ||= POSITIONED.test(el.className);
      if (SCROLL.test(el.className)) {
        expect(positioned, `"${span.textContent}" sits in a scroll frame with no positioned ancestor up to it`).toBe(true);
        break;
      }
    }
  }
  return spans.length;
}

describe("sr-only spans stay inside their scroll frames", () => {
  it("roles_tab", async () => {
    const m = mountTab();
    const soon = await screen.findByText("Expires soon:");
    expect(soon).toBeInTheDocument();
    expect(srOnlyContained(m.container)).toBeGreaterThan(0);
  });

  it("role_page", async () => {
    const m = mountPage();
    await screen.findByRole("table", { name: "Tables and views in billing" });
    srOnlyContained(m.container);
  });
});

describe("the all-rows block fits 390 px", () => {
  async function allRows() {
    const m = mountPage();
    await screen.findByRole("table", { name: "Tables and views in billing" });
    return m;
  }

  it("the_policy_name_wraps_or_truncates_and_carries_a_title", async () => {
    await allRows();
    const label = screen.getByText(/^on: policy /);
    expect(label).toHaveTextContent("billing_editor_all_rows");
    expect(label).toHaveAttribute("title", expect.stringContaining("billing_editor_all_rows"));
    expect(label.className).toMatch(/min-w-0/);
    expect(label.className).toMatch(/truncate|break-words|break-all|\[overflow-wrap:(anywhere|break-word)\]/);
  });

  it("the_staged_label_wraps_inside_its_column_instead_of_running_under_the_switch", async () => {
    const m = await allRows();
    await m.user.click(screen.getByRole("switch", { name: "All rows on refunds" }));
    const label = screen.getByText("on, staged");
    expect(label.className).toMatch(/min-w-0/);
    expect(label.className).toMatch(/break-words|\[overflow-wrap:(anywhere|break-word)\]/);
    const row = label.closest("li")!;
    expect(within(row).getByRole("switch")).toBeChecked();
  });
});
