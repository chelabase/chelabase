import { describe, expect, it } from "vitest";
import { fadeMask, scrollFade } from "./scroll-fade";

describe("scrollFade", () => {
  it("none_when_everything_fits", () => {
    expect(scrollFade(0, 300, 300)).toEqual({ start: false, end: false });
    expect(scrollFade(0, 300, 301)).toEqual({ start: false, end: false });
  });
  it("end_only_at_the_left_edge", () => {
    expect(scrollFade(0, 300, 500)).toEqual({ start: false, end: true });
  });
  it("both_in_the_middle", () => {
    expect(scrollFade(100, 300, 500)).toEqual({ start: true, end: true });
  });
  it("start_only_at_the_right_edge", () => {
    expect(scrollFade(200, 300, 500)).toEqual({ start: true, end: false });
    expect(scrollFade(199.5, 300, 500)).toEqual({ start: true, end: false });
  });
});

describe("fadeMask", () => {
  it("is_absent_without_overflow_and_fades_the_named_edges", () => {
    expect(fadeMask({ start: false, end: false })).toBeUndefined();
    expect(fadeMask({ start: false, end: true })).toContain("transparent)");
    expect(fadeMask({ start: true, end: false })).toContain("transparent, #000 1.5rem");
  });
});
