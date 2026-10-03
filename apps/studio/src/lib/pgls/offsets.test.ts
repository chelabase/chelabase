import { describe, expect, it } from "vitest";
import { byteToUtf16, utf16ToByte } from "./offsets";

const encoder = new TextEncoder();

function boundaries(text: string): number[] {
  const out: number[] = [];
  let i = 0;
  for (const ch of text) {
    out.push(i);
    i += ch.length;
  }
  out.push(i);
  return out;
}

describe("offsets", () => {
  it("offsets_round_trip_multibyte", () => {
    const samples = ["select 1", "select 'é' from t", "select '😀', x", "select '表格' as 名", "é😀表a"];
    for (const text of samples) {
      for (const index of boundaries(text)) {
        const byte = utf16ToByte(text, index);
        expect(byte).toBe(encoder.encode(text.slice(0, index)).length);
        expect(byteToUtf16(text, byte)).toBe(index);
      }
    }
    // Spot checks against hand-counted values.
    expect(utf16ToByte("é", 1)).toBe(2);
    expect(utf16ToByte("😀", 2)).toBe(4);
    expect(utf16ToByte("表", 1)).toBe(3);
    expect(byteToUtf16("a😀b", 5)).toBe(3);
  });

  it("clamps out-of-range offsets to the text", () => {
    expect(utf16ToByte("é", 5)).toBe(2);
    expect(byteToUtf16("é", 99)).toBe(1);
    expect(byteToUtf16("é", -1)).toBe(0);
  });
});
