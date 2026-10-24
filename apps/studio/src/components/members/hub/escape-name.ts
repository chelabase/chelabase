/**
 * A role name for display (MH-38d): printable ASCII stays (a backslash is doubled, so the mapping is
 * injective); every other UTF-16 unit (control,
 * bidi, zero-width, look-alike or accented) is written as `\uXXXX`, so a name never hides what it is.
 */
export function escapeName(name: string): string {
  let out = "";
  for (let i = 0; i < name.length; i++) {
    const unit = name.charCodeAt(i);
    if (unit === 0x5c) out += "\\\\";
    else out += unit >= 0x20 && unit <= 0x7e ? name[i] : `\\u${unit.toString(16).padStart(4, "0")}`;
  }
  return out;
}
