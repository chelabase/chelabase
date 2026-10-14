import { describe, expect, it } from "vitest";

describe("old Tables console", () => {
  it("is_gone", () => {
    const left = import.meta.glob(["./tables-view.tsx", "./tables-view.test.tsx", "./console-grid.tsx", "./console-filters.tsx", "./row-editor.tsx"]);
    expect(Object.keys(left)).toEqual([]);
  });
});
