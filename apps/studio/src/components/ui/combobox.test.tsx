import { render, screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import { MultiCombobox } from "./combobox";

const long = "r".repeat(63);
const chain = "also gives " + "billing_admin, ".repeat(6) + "reports";

function renderChips(props: Partial<React.ComponentProps<typeof MultiCombobox>> = {}) {
  return render(
    <MultiCombobox
      label="Roles"
      options={[{ value: long }, { value: "billing" }, { value: "reports" }]}
      value={[long, "billing"]}
      onValueChange={() => {}}
      chipDetail={(v) => (v === long ? chain : undefined)}
      mono
      {...props}
    />,
  );
}

// The chip's accessible name is the value, then ", <detail>" when it has one.
const chipOf = (name: string) =>
  screen.getByLabelText((label) => label === name || label.startsWith(`${name}, `), { selector: "[class*='border']" });
const hasClass = (el: Element, re: RegExp) => Array.from(el.classList).some((c) => re.test(c));

describe("MultiCombobox chip markup (KI-2)", () => {
  it("lets the chip shrink to its container instead of growing past it", () => {
    renderChips();
    const chip = chipOf(long);
    expect(chip.classList.contains("min-w-0")).toBe(true);
    expect(chip.classList.contains("max-w-full")).toBe(true);
  });

  it("truncates or breaks the name label inside the chip (min-w-0 plus truncate or break-all)", () => {
    renderChips();
    const label = chipOf(long).querySelector("span");
    expect(label).not.toBeNull();
    expect(label!.textContent).toBe(long);
    expect(label!.classList.contains("min-w-0")).toBe(true);
    expect(label!.classList.contains("truncate") || label!.classList.contains("break-all")).toBe(true);
  });

  it("keeps the full name on the label as a title so a truncated name is readable", () => {
    renderChips();
    const label = chipOf(long).querySelector("span")!;
    expect(label.getAttribute("title")).toBe(long);
  });

  it("truncates or breaks the chain detail too, with the full detail in a title", () => {
    renderChips();
    const spans = chipOf(long).querySelectorAll("span");
    const detail = Array.from(spans).find((s) => s.textContent === chain)!;
    expect(detail).toBeDefined();
    expect(detail.classList.contains("min-w-0")).toBe(true);
    expect(detail.classList.contains("break-all")).toBe(true);
    expect(detail.classList.contains("truncate")).toBe(false);
    expect(detail.getAttribute("title")).toBe(chain);
  });

  it("puts the detail in the chip's accessible name", () => {
    renderChips();
    expect(chipOf(long)).toHaveAttribute("aria-label", `${long}, ${chain}`);
  });

  it("keeps the remove button from shrinking away, with its accessible name", () => {
    renderChips();
    const remove = screen.getByRole("button", { name: `Remove ${long}` });
    expect(remove.classList.contains("shrink-0")).toBe(true);
    expect(chipOf(long).contains(remove)).toBe(true);
  });

  it("keeps the chip accessible name and the chips container wrapping", () => {
    renderChips();
    expect(chipOf(long)).toBeTruthy();
    const group = screen.getByLabelText("Chosen roles");
    expect(group.classList.contains("flex-wrap")).toBe(true);
  });

  it("applies the same shrinking contract to every chip, including short ones", () => {
    renderChips();
    const chip = chipOf("billing");
    expect(chip.classList.contains("min-w-0")).toBe(true);
    expect(chip.classList.contains("max-w-full")).toBe(true);
  });

  it("keeps the mono style and the coral invalid state on the chip", () => {
    renderChips({ invalid: new Set([long]) });
    const chip = chipOf(long);
    expect(hasClass(chip, /^border-coral$/)).toBe(true);
    expect(hasClass(chip, /^text-coral$/)).toBe(true);
    expect(chip.classList.contains("min-w-0")).toBe(true);
    const label = chip.querySelector("span")!;
    expect(label.className).toContain("font-mono");
    expect(label.classList.contains("min-w-0")).toBe(true);
  });

  it("still renders and shrinks chips when disabled", () => {
    renderChips({ disabled: true });
    const chip = chipOf(long);
    expect(chip.classList.contains("max-w-full")).toBe(true);
    expect(chip.querySelector("span")!.classList.contains("min-w-0")).toBe(true);
  });
});
