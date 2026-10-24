import { expect } from "vitest";

/**
 * Fix S (finish review, fix 7): no browser-default controls. A themed `<select>` keeps the native
 * element (labels, keys and `selectOptions` still work) but drops the OS look: `appearance-none`
 * plus a drawn chevron (an `svg`) in the same wrapper.
 */
export function expectThemedSelects(root: ParentNode, expectedAtLeast = 1) {
  const selects = Array.from(root.querySelectorAll("select"));
  expect(selects.length).toBeGreaterThanOrEqual(expectedAtLeast);
  for (const select of selects) {
    expect(select.className, `select "${select.getAttribute("aria-label") ?? select.id}" is themed`).toContain("appearance-none");
    expect(select.parentElement?.querySelector("svg"), `select "${select.getAttribute("aria-label") ?? select.id}" has a chevron`).not.toBeNull();
  }
}

/** The themed checkbox (`table-editor/panel-bits` `Checkbox`): a native input with `appearance-none`, its drawn tick beside it. */
export function expectThemedCheckboxes(root: ParentNode, expectedAtLeast = 1) {
  const boxes = Array.from(root.querySelectorAll('input[type="checkbox"]'));
  expect(boxes.length).toBeGreaterThanOrEqual(expectedAtLeast);
  for (const box of boxes) {
    expect(box.className, `checkbox "${box.getAttribute("aria-label")}" is themed`).toContain("appearance-none");
    expect(box.parentElement?.querySelector("svg"), `checkbox "${box.getAttribute("aria-label")}" has its tick`).not.toBeNull();
  }
}

/** A search field with its magnifier: the icon shares a wrapper with the input and with no other control. */
export function expectSearchIcon(input: HTMLElement) {
  const wrapper = input.parentElement!;
  expect(wrapper.querySelector("svg"), "the field has a magnifier").not.toBeNull();
  expect(wrapper.querySelectorAll("input, button, [role='radio']").length, "the wrapper holds only the field").toBe(1);
}
