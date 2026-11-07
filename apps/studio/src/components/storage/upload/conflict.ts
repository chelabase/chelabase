/**
 * Keep both: the new file goes to `name (n).ext` in the same folder, the first `n` not taken.
 * The number goes before the last extension of the last segment; a dotfile (`.env`) has none.
 */
export function keepBothName(name: string, n: number): string {
  const slash = name.lastIndexOf("/");
  const dir = name.slice(0, slash + 1);
  const base = name.slice(slash + 1);
  const dot = base.lastIndexOf(".");
  if (dot <= 0) return `${dir}${base} (${n})`;
  return `${dir}${base.slice(0, dot)} (${n})${base.slice(dot)}`;
}

/** How many suffixed names Keep both tries before giving up. */
export const MAX_KEEP_BOTH = 1000;
