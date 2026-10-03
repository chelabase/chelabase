// pgls speaks UTF-8 byte offsets; CodeMirror speaks UTF-16 code units. Both walk the text by
// code point, so a surrogate pair counts as one 4-byte character.

function utf8Length(codePoint: number): number {
  if (codePoint < 0x80) return 1;
  if (codePoint < 0x800) return 2;
  if (codePoint < 0x10000) return 3;
  return 4;
}

/** The UTF-8 byte offset of the UTF-16 `index` in `text` (clamped to the text). */
export function utf16ToByte(text: string, index: number): number {
  const end = Math.min(Math.max(index, 0), text.length);
  let bytes = 0;
  let i = 0;
  while (i < end) {
    const cp = text.codePointAt(i) ?? 0;
    const units = cp > 0xffff ? 2 : 1;
    if (i + units > end) break; // `index` splits a surrogate pair: stop before it
    bytes += utf8Length(cp);
    i += units;
  }
  return bytes;
}

/**
 * The UTF-16 index of the UTF-8 byte offset `byte` in `text` (clamped to the text). A byte
 * inside a multibyte character maps to that character's start.
 */
export function byteToUtf16(text: string, byte: number): number {
  let bytes = 0;
  let i = 0;
  while (i < text.length) {
    const cp = text.codePointAt(i) ?? 0;
    const len = utf8Length(cp);
    if (bytes + len > byte) break;
    bytes += len;
    i += cp > 0xffff ? 2 : 1;
  }
  return i;
}
