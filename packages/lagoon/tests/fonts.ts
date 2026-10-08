const ALLOWED = /^var\(--font-(sans|serif|mono)\)$/;
const ALLOWED_ANYWHERE = /var\(--font-(sans|serif|mono)\)/g;

function stripComments(s: string): string {
  return s.replace(/\/\*[\s\S]*?\*\//g, "");
}

/**
 * Every font-stack declaration of a css/ts/tsx source that is not exactly the lagoon's
 * `var(--font-sans|serif|mono)` (D8): `font-family` / `fontFamily` values (whole value, so a
 * trailing fallback is a violation), `--font-*` definitions, the `font:` shorthand naming a
 * family, and Tailwind arbitrary `font-[...]` utilities naming a family. Weights are fine.
 */
export function fontViolations(source: string): string[] {
  const src = stripComments(source);
  const hits: string[] = [];

  // A quoted string (TS) or the CSS value up to `;`, `}` or the end of the line.
  for (const m of src.matchAll(/(?:font-family|fontFamily)\s*:\s*("[^"]*"|'[^']*'|`[^`]*`|[^;}\n]+)/g)) {
    const value = m[1]!
      .trim()
      .replace(/\s*!important$/, "")
      .replace(/^["'`]|["'`]$/g, "")
      .trim();
    if (!ALLOWED.test(value)) hits.push(m[0].trim());
  }

  for (const m of src.matchAll(/--font-[a-z-]+\s*:/g)) hits.push(m[0]);

  // `font:` shorthand: after removing the allowed vars, no quote, comma or family word after a size.
  for (const m of src.matchAll(/(?<![-\w])font\s*:\s*("[^"]*"|'[^']*'|[^;}\n]+)/g)) {
    const rest = m[1]!.replace(ALLOWED_ANYWHERE, "");
    if (/["'`,]/.test(rest) || /\d(?:px|rem|em|%|pt|vw|vh)?(?:\/[\w.]+)?\s+[a-zA-Z-]+/.test(rest)) hits.push(m[0].trim());
  }

  // Tailwind arbitrary values: weights and the lagoon var are fine, anything else names a family.
  for (const m of src.matchAll(/font-\[([^\]]+)\]/g)) {
    const v = m[1]!.trim();
    const ok =
      /^\d+$/.test(v) ||
      /^(?:weight|number):/.test(v) ||
      ALLOWED.test(v) ||
      /^family-name:var\(--font-(sans|serif|mono)\)$/.test(v);
    if (!ok) hits.push(m[0]);
  }

  return hits;
}
