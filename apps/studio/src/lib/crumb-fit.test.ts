import { describe, expect, it } from "vitest";
import { neededTrailWidth } from "./crumb-fit";

describe("neededTrailWidth", () => {
  it("equals_the_width_when_nothing_is_truncated", () => {
    expect(neededTrailWidth(300, [{ scroll: 80, client: 80 }])).toBe(300);
    expect(neededTrailWidth(300, [])).toBe(300);
  });
  it("ignores_sub_pixel_rounding", () => {
    expect(neededTrailWidth(300, [{ scroll: 81, client: 80 }])).toBe(300);
  });
  it("adds_what_each_truncated_item_is_missing", () => {
    expect(neededTrailWidth(300, [{ scroll: 120, client: 80 }, { scroll: 50, client: 50 }, { scroll: 90, client: 60 }])).toBe(370);
  });
  it("counts_the_last_crumb_only_up_to_its_minimum", () => {
    // Cut to 40 now, 400 wide in full: only the 88px up to 128 are missing.
    expect(neededTrailWidth(300, [{ scroll: 400, client: 40, last: true }])).toBe(388);
    // Already at the minimum or wider: nothing missing, however long the name.
    expect(neededTrailWidth(300, [{ scroll: 400, client: 128, last: true }])).toBe(300);
    // Wider than the minimum: it gives the difference back.
    expect(neededTrailWidth(300, [{ scroll: 400, client: 200, last: true }])).toBe(228);
    expect(neededTrailWidth(300, [{ scroll: 80, client: 80, last: true }])).toBe(300);
    // A last crumb shorter than the minimum is counted in full.
    expect(neededTrailWidth(300, [{ scroll: 100, client: 40, last: true }])).toBe(360);
  });
  it("counts_other_truncated_crumbs_in_full_beside_the_last", () => {
    expect(neededTrailWidth(300, [{ scroll: 120, client: 80 }, { scroll: 400, client: 128, last: true }])).toBe(340);
  });
});
