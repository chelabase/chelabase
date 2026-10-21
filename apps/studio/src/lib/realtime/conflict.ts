import type { ColumnDesc } from "@chelabase/management-client";
import type { Tree } from "@/lib/console";
import type { Row } from "@/lib/table-editor/rows";
import { keyConditions, textKeyOf } from "./keys";
import type { Key } from "./types";

export const NO_CHECK_NOTICE = "No conflict check for this column type";
export const GONE_TEXT = "This row was deleted or is hidden from this role.";
export const CHANGED_TITLE = "Changed by someone else";

/** Column → its value's text when the editor opened, or null for NULL. A column that can't be compared is absent. */
export type Originals = Record<string, string | null>;

/** Types without an equality operator (R18): their values can't be compared, so the save doesn't check them. */
const NOT_COMPARABLE = new Set(["json", "xml", "point", "line", "lseg", "box", "path", "polygon", "circle"]);

/** Whether the conflict check can compare a column (R18): false for `json`, `xml`, geometric types and arrays of them. */
export function isComparable(desc: Pick<ColumnDesc, "type">): boolean {
  const base = desc.type
    .trim()
    .toLowerCase()
    .replace(/(\[\])+$/, "")
    .replace(/\(.*\)$/, "")
    .replace(/^pg_catalog\./, "")
    .trim();
  return !NOT_COMPARABLE.has(base);
}

/** The originals of a row as read with the page (`$o` aliases): a snapshot, taken when an editor or the row panel opens (W1). */
export function captureOriginals(row: Row, aliases: Record<string, string>): Originals {
  const out: Originals = {};
  for (const [col, alias] of Object.entries(aliases)) {
    if (!Object.hasOwn(row, alias)) continue;
    const v = row[alias];
    out[col] = v === null || v === undefined ? null : String(v);
  }
  return out;
}

/** Rows read with `textSelectItems`: the rows without the alias fields, each row's key string and its originals. */
export function splitTextRows(raw: Row[], aliases: { pkAliases: string[]; originalAliases: Record<string, string> }): { rows: Row[]; keys: (Key | null)[]; originals: Originals[] } {
  const extra = new Set([...aliases.pkAliases, ...Object.values(aliases.originalAliases)]);
  const rows: Row[] = [];
  const keys: (Key | null)[] = [];
  const originals: Originals[] = [];
  for (const r of raw) {
    keys.push(textKeyOf(r, aliases.pkAliases));
    originals.push(captureOriginals(r, aliases.originalAliases));
    rows.push(Object.fromEntries(Object.entries(r).filter(([k]) => !extra.has(k))));
  }
  return { rows, keys, originals };
}

/**
 * The update's `WHERE`: the PK, plus each edited column's original (`eq` text, or `is: null`). `unchecked` lists the edited
 * columns left out (not in `originals`). One condition is bare; several are `{and: [pk…, originals…]}`.
 */
export function conflictWhere(a: { pk: string[]; key: Key; edited: string[]; originals: Originals }): { where: Tree; unchecked: string[] } {
  const parts = keyConditions(a.pk, a.key);
  const unchecked: string[] = [];
  for (const col of a.edited) {
    // A key column is already matched by the key itself.
    if (a.pk.includes(col)) continue;
    if (!Object.hasOwn(a.originals, col)) {
      unchecked.push(col);
      continue;
    }
    const was = a.originals[col];
    parts.push(was === null ? { col, is: null } : { col, eq: was, text: true });
  }
  return { where: parts.length === 1 ? parts[0] : { and: parts }, unchecked };
}
