import { describe, expect, it } from "vitest";
import { scrollHint } from "./scroll-hint";

describe("scrollHint", () => {
  it("content_that_fits_neither_scrolls_nor_hints", () => {
    expect(scrollHint({ scrollLeft: 0, clientWidth: 1180, scrollWidth: 1180 })).toEqual({ scrollable: false, more: false });
  });

  it("a_fractional_overflow_counts_as_fitting", () => {
    expect(scrollHint({ scrollLeft: 0, clientWidth: 306, scrollWidth: 306.5 })).toEqual({ scrollable: false, more: false });
  });

  it("wider_content_hints_until_scrolled_to_the_end", () => {
    expect(scrollHint({ scrollLeft: 0, clientWidth: 306, scrollWidth: 1054 })).toEqual({ scrollable: true, more: true });
    expect(scrollHint({ scrollLeft: 400, clientWidth: 306, scrollWidth: 1054 })).toEqual({ scrollable: true, more: true });
    expect(scrollHint({ scrollLeft: 748, clientWidth: 306, scrollWidth: 1054 })).toEqual({ scrollable: true, more: false });
  });
});
