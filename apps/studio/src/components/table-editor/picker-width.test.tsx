import { render, screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import { Picker, headerWidth } from "./picker";

describe("headerWidth", () => {
  it("short values take only their own width, with a min that never exceeds it", () => {
    expect(headerWidth("owner")).toEqual({ flex: "1 1 7ch", width: 0, minWidth: "min(7rem, 7ch)" });
  });

  it("long values are capped at 28ch and keep a 7rem minimum", () => {
    expect(headerWidth("x".repeat(60))).toEqual({ flex: "1 1 28ch", width: 0, minWidth: "min(7rem, 28ch)" });
  });
});

describe("header Picker field", () => {
  it("shrinks (flex-shrink, zero width) instead of holding a fixed width, and titles its value", () => {
    render(<Picker label="Branch" options={[{ value: "b", label: "a-very-long-branch-name" }]} value="b" onChange={() => {}} />);
    const input = screen.getByRole("combobox", { name: "Branch" });
    expect(input.style.flex).toBe("1 1 24ch");
    expect(input.style.width).toBe("0px");
    expect(input).toHaveAttribute("title", "a-very-long-branch-name");
    expect(input.className).toContain("truncate");
    expect(input.parentElement!.className).toContain("min-w-0");
  });
});
