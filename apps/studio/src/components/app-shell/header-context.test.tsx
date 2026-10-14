import { render, screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import { HeaderContext } from "./header-context";

describe("HeaderContext", () => {
  it("portals_into_the_header_slot_or_renders_inline", () => {
    const { unmount } = render(<HeaderContext><span>ctx</span></HeaderContext>);
    expect(screen.getByText("ctx")).toBeInTheDocument();
    unmount();
    const slot = document.createElement("div");
    slot.id = "app-header-context";
    document.body.appendChild(slot);
    render(<div data-testid="page"><HeaderContext><span>ctx</span></HeaderContext></div>);
    expect(slot).toHaveTextContent("ctx");
    expect(screen.getByTestId("page")).not.toHaveTextContent("ctx");
    slot.remove();
  });
});
