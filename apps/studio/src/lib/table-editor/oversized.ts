import type { Table } from "@chelabase/management-client";
import { primaryKeyOf } from "./rows";

/** Types whose values can be large enough to push a single row over the console's response limit. */
export const LARGE_TYPES: ReadonlySet<string> = new Set(["text", "bytea", "json", "jsonb", "xml"]);

/** The base name of a `data_type` (`[]` and a length stripped): `text[]` is `text`, `character varying(20)` is `character varying`. */
function baseType(dataType: string): string {
  return dataType.replace(/(\[\d*\])+$/, "").replace(/\(.*\)$/, "").trim().toLowerCase();
}

/** Whether a column may hold a large value: an array or one of {@link LARGE_TYPES}. */
export function isLargeColumn(dataType: string): boolean {
  return /\[\d*\]$/.test(dataType.trim()) || LARGE_TYPES.has(baseType(dataType));
}

/**
 * The columns a single too-large row is still read with: every column that is neither an array nor
 * of a large type, and always the primary key's columns (rows are found by them).
 */
export function smallColumns(t: Table): string[] {
  const pk = new Set(primaryKeyOf(t));
  return [...t.columns]
    .sort((a, b) => a.attnum - b.attnum)
    .filter((c) => pk.has(c.name) || !isLargeColumn(c.data_type))
    .map((c) => c.name);
}
