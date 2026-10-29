import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { Columns3 } from "lucide-react";
import { describe, expect, it, vi } from "vitest";
import { CollapsedStrip } from "./panel-bits";

/**
 * A collapsed panel's strip (2026-10-29): the expand button, then one icon naming what the panel holds with a small
 * badge (a count, or a sand dot for a held draft). No vertical text: the full name is the button's tooltip and its
 * accessible description. One tab stop; the icon expands too, for the pointer.
 */
describe("CollapsedStrip", () => {
  it("names_the_panel_in_the_tooltip_and_description_without_vertical_text", () => {
    const { container } = render(<CollapsedStrip text="Pending changes · 3" icon={Columns3} badge={3} onExpand={() => {}} />);
    const expand = screen.getByRole("button", { name: "Expand panel" });
    expect(expand).toHaveAccessibleDescription("Pending changes · 3");
    expect(expand).toHaveAttribute("title", "Pending changes · 3");
    expect(container.querySelector("[class*='writing-mode']")).toBeNull();
    expect(screen.getByTestId("strip-badge")).toHaveTextContent("3");
    // One tab stop.
    expect(container.querySelectorAll("button, [tabindex]:not([tabindex='-1'])")).toHaveLength(1);
  });

  it("a_dot_badge_marks_a_held_draft", () => {
    render(<CollapsedStrip text="Edit column memo · draft, not added yet" icon={Columns3} badge="dot" onExpand={() => {}} />);
    expect(screen.getByTestId("strip-badge")).toHaveTextContent("");
  });

  it("no_badge_without_one", () => {
    render(<CollapsedStrip text="Row" icon={Columns3} onExpand={() => {}} />);
    expect(screen.queryByTestId("strip-badge")).toBeNull();
  });

  it("the_icon_expands_too", async () => {
    const onExpand = vi.fn();
    render(<CollapsedStrip text="Row" icon={Columns3} onExpand={onExpand} />);
    await userEvent.setup().click(screen.getByTestId("strip-icon"));
    expect(onExpand).toHaveBeenCalledTimes(1);
  });
});
