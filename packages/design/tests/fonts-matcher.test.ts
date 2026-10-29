import { describe, expect, it } from "vitest";
import { fontViolations } from "./fonts";

describe("fontViolations", () => {
  it.each([
    ["trailing fallback", "a { font-family: var(--font-sans), Georgia, serif; }"],
    ["trailing fallback, no semicolon", "a { font-family: var(--font-sans), serif }"],
    ["literal family", 'a { font-family: "Inter", sans-serif; }'],
    ["literal family in TS", 'const s = { fontFamily: "Georgia" };'],
    ["fallback inside a TS string", 'const s = { fontFamily: "var(--font-sans), Georgia" };'],
    ["font stack definition", ":root { --font-sans: Inter; }"],
    ["shorthand with a family", "a { font: 12px Georgia; }"],
    ["shorthand with a quoted family", 'a { font: italic 12px/1.5 "Inter", serif; }'],
    ["arbitrary Tailwind family", `<p className="font-['Inter']" />`],
    ["arbitrary Tailwind family-name", `<p className="font-[family-name:Inter]" />`],
  ])("flags %s", (_name, src) => {
    expect(fontViolations(src)).not.toEqual([]);
  });

  it.each([
    ["sans", "a { font-family: var(--font-sans); }"],
    ["serif without semicolon", "a { font-family: var(--font-serif) }"],
    ["mono", "a { font-family: var(--font-mono) !important; }"],
    ["TS var with following properties", 'const s = { fontFamily: "var(--font-mono)", overflow: "auto" };'],
    ["shorthand with the var", "a { font: 12px/1.5 var(--font-sans); }"],
    ["shorthand inherit", "a { font: inherit; }"],
    ["font-size", "a { font-size: 12px; }"],
    ["Tailwind weight utility", `<p className="font-semibold font-[600] font-sans font-mono" />`],
    ["Tailwind weight type", `<p className="font-[weight:var(--w)]" />`],
    ["Tailwind lagoon family", `<p className="font-[family-name:var(--font-serif)]" />`],
    ["a comment naming a family", "/* font-family: Georgia; */ a { color: red; }"],
  ])("accepts %s", (_name, src) => {
    expect(fontViolations(src)).toEqual([]);
  });
});
