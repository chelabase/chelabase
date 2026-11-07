import { ApiError, type ColumnDesc, type Table, type TypeEntry } from "@chelabase/management-client";
import { storageRefOf } from "@/components/table-editor/cells/storage-ref";

/** Cells show at most this many characters (S2). */
export const CELL_LIMIT = 200;

/** Column widths per table, in `localStorage` (S4). */
export const WIDTHS_KEY = "chelabase.studio.tables.widths";

export type Row = Record<string, unknown>;

/** A grid column: its name, the console's description of its type, and enum labels when it is an enum. */
export interface RowColumn {
  name: string;
  desc: ColumnDesc;
  labels: string[] | null;
  /** Generated or `GENERATED ALWAYS AS IDENTITY`: never written from the grid. */
  readOnly: boolean;
}

/** What a cell shows: `NULL` for null, JSON for objects, at most {@link CELL_LIMIT} characters. */
export function cellText(value: unknown): string {
  if (value === null || value === undefined) return "NULL";
  const text = typeof value === "object" ? JSON.stringify(value) : String(value);
  return text.length > CELL_LIMIT ? `${text.slice(0, CELL_LIMIT)}…` : text;
}

/** The text an editor starts from: empty for null, JSON for objects, the full value otherwise. */
export function editText(value: unknown): string {
  if (value === null || value === undefined) return "";
  return typeof value === "object" ? JSON.stringify(value) : String(value);
}

function tsvField(value: unknown): string {
  const text = editText(value);
  return /[\t\n\r"]/.test(text) ? `"${text.replace(/"/g, '""')}"` : text;
}

/** Rows as tab-separated values with a header line; NULL is an empty field, JSON values are JSON. */
export function toTsv(columns: string[], rows: Row[]): string {
  return [columns.join("\t"), ...rows.map((r) => columns.map((c) => tsvField(r[c])).join("\t"))].join("\n");
}

/** A JSON number literal, as Postgres writes a numeric (never NaN or an infinity, which it quotes). */
const JSON_NUMBER = /^-?(0|[1-9]\d*)(\.\d+)?([eE][+-]?\d+)?$/;
const rawJson = (JSON as { rawJSON?: (text: string) => unknown }).rawJSON;

/**
 * Rows as JSON for Copy (JSON). A number column's value read as its exact text (`2.50`, a 20-digit numeric) is written
 * back as that JSON number, as Postgres wrote it; other strings (NaN, the infinities) stay strings. Without
 * `JSON.rawJSON` the text stays a string rather than lose digits.
 */
export function toJson(rows: Row[], numeric: ReadonlySet<string>): string {
  const out = rawJson
    ? rows.map((r) =>
        Object.fromEntries(Object.entries(r).map(([k, v]) => [k, numeric.has(k) && typeof v === "string" && JSON_NUMBER.test(v) ? rawJson(v) : v])),
      )
    : rows;
  return JSON.stringify(out, null, 2);
}

/** The grid's columns of a table, in attnum order, typed for `inputKind`/`parseValue`. */
export function columnDescs(table: Table, catalog: TypeEntry[]): RowColumn[] {
  const byOid = new Map(catalog.map((t) => [t.oid, t]));
  return [...table.columns]
    .sort((a, b) => a.attnum - b.attnum)
    .map((c) => {
      const type = byOid.get(c.type_oid);
      const isEnum = type?.kind === "enum";
      const generated = !!c.generated;
      // An object reference (m11's storage_ref forms): its cells show and upload files (#106).
      const storageRef = storageRefOf(c, table, type);
      return {
        name: c.name,
        desc: {
          type: c.data_type,
          array: c.data_type.endsWith("[]"),
          enum: isEnum ? `${type.schema}.${type.name}` : null,
          generated,
          has_default: c.default != null || !!c.identity,
          nullable: !c.not_null,
          ...(storageRef ? { storage_ref: storageRef } : {}),
        },
        labels: isEnum ? (type.labels ?? []) : null,
        readOnly: generated || c.identity?.generation === "always",
      };
    });
}

/** The primary key's column names, in key order; empty without one. */
export function primaryKeyOf(table: Table): string[] {
  const pk = table.constraints.find((c) => c.kind === "primary_key");
  if (!pk) return [];
  const names = new Map(table.columns.map((c) => [c.attnum, c.name]));
  return pk.columns.map((n) => names.get(n)).filter((n): n is string => n !== undefined);
}

export type TemporalKind = "date" | "time" | "timestamp" | "timestamptz";

/** Date and time columns get their own editors; `time with time zone` and arrays don't. */
export function temporalKind(desc: ColumnDesc): TemporalKind | null {
  if (desc.array) return null;
  const t = desc.type.toLowerCase().replace(/\(\d+\)/, "").trim();
  if (t === "date") return "date";
  if (t === "time" || t === "time without time zone") return "time";
  if (t === "timestamp" || t === "timestamp without time zone") return "timestamp";
  if (t === "timestamptz" || t === "timestamp with time zone") return "timestamptz";
  return null;
}

const TIMESTAMPTZ = /^(\d{4})-(\d{2})-(\d{2})[T ](\d{2}):(\d{2})(:\d{2}(\.\d{1,6})?)?\s*(Z|[+-]\d{2}(:?\d{2})?)?$/i;

/** A `timestamptz` literal the editor accepts: a date and time with an optional offset, or ±infinity. */
export function validTimestamptz(text: string): boolean {
  const t = text.trim();
  if (/^-?infinity$/i.test(t)) return true;
  const m = TIMESTAMPTZ.exec(t);
  if (!m) return false;
  const [month, day, hour, minute] = [m[2], m[3], m[4], m[5]].map(Number);
  return month >= 1 && month <= 12 && day >= 1 && day <= 31 && hour <= 24 && minute <= 59;
}

/** The console refused a response over its size limit. */
export function isTooLarge(err: unknown): boolean {
  return err instanceof ApiError && (err.status === 413 || (err.code as string) === "too_large");
}

function readAll(storage: Storage | null): Record<string, Record<string, number>> {
  if (!storage) return {};
  try {
    const parsed: unknown = JSON.parse(storage.getItem(WIDTHS_KEY) ?? "{}");
    return typeof parsed === "object" && parsed !== null && !Array.isArray(parsed) ? (parsed as Record<string, Record<string, number>>) : {};
  } catch {
    return {};
  }
}

/** The saved column widths of one table (`{}` when none, or storage fails). */
export function readWidths(storage: Storage | null, table: string): Record<string, number> {
  const saved = readAll(storage)[table];
  if (typeof saved !== "object" || saved === null) return {};
  return Object.fromEntries(Object.entries(saved).filter(([, w]) => typeof w === "number" && Number.isFinite(w)));
}

/** Saves one table's column widths; storage errors are ignored. */
export function writeWidths(storage: Storage | null, table: string, widths: Record<string, number>): void {
  if (!storage) return;
  try {
    storage.setItem(WIDTHS_KEY, JSON.stringify({ ...readAll(storage), [table]: widths }));
  } catch {
    // Widths are a convenience.
  }
}

/** A starting width for a column of this type. */
export function defaultWidth(desc: ColumnDesc): number {
  const t = desc.type.toLowerCase();
  if (desc.array || t === "json" || t === "jsonb") return 220;
  if (t === "boolean" || t === "bool") return 88;
  if (/^(smallint|integer|bigint|int[248]|real|double precision|numeric.*|float[48])$/.test(t)) return 112;
  if (t === "uuid") return 140;
  if (t.startsWith("timestamp")) return 196;
  if (t === "date") return 112;
  if (t === "text" || t.startsWith("character")) return 200;
  return 152;
}
