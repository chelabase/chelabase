import { render, screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import { MultiCombobox } from "./combobox";

function renderChips(props: Partial<React.ComponentProps<typeof MultiCombobox>> = {}) {
  return render(
    <MultiCombobox label="Roles" options={[{ value: "a" }, { value: "b" }]} value={["a"]} onValueChange={() => {}} {...props} />,
  );
}

describe("MultiCombobox chips, extra edges", () => {
  it("renders no detail span when the chip has no detail", () => {
    renderChips();
    const chip = screen.getByLabelText("a", { selector: "[class*='border']" });
    expect(chip.querySelectorAll("span").length).toBe(1);
  });

  it("without mono, the name label shrinks but is not monospaced", () => {
    renderChips();
    const label = screen.getByLabelText("a", { selector: "[class*='border']" }).querySelector("span")!;
    expect(label.className).not.toContain("font-mono");
    expect(label.classList.contains("min-w-0")).toBe(true);
    expect(label.classList.contains("truncate")).toBe(true);
  });

  it("renders many chips, each shrinkable, in the wrapping group", () => {
    const vals = Array.from({ length: 30 }, (_, i) => `role_${i}`);
    renderChips({ options: vals.map((value) => ({ value })), value: vals });
    const group = screen.getByLabelText("Chosen roles");
    expect(group.classList.contains("flex-wrap")).toBe(true);
    for (const v of vals) {
      expect(screen.getByLabelText(v, { selector: "[class*='border']" }).classList.contains("max-w-full")).toBe(true);
    }
  });
});
