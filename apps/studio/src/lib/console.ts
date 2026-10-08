import { ApiError, type ColumnDesc } from "@chelabase/management-client";

/** What makes a role owner-level, as every owner-level text in Studio lists it. */
export const OWNER_LEVEL_TRAITS = "superuser, CREATEROLE, CREATEDB, REPLICATION or a member of neon_superuser";

/** Rows per page (P12). */
export const PAGE_SIZE = 50;

export type Tree = Record<string, unknown>;
export type SortDir = "asc" | "desc";
export interface Sort {
  col: string;
  dir: SortDir;
}
export const FILTER_OPS = ["eq", "neq", "lt", "lte", "gt", "gte", "in", "is", "like", "ilike"] as const;
export type FilterOp = (typeof FILTER_OPS)[number];

const TEXT_LIKE = /^(text|citext|name|bpchar|character( varying)?|varchar|char)(\(.*\))?$/i;
/** like/ilike make sense on text columns only. */
export const isTextLike = (col: ColumnDesc) => !col.array && !col.enum && TEXT_LIKE.test(col.type.trim());
/** The filter operators offered for a column. */
export const filterOpsFor = (col: ColumnDesc | undefined): FilterOp[] =>
  FILTER_OPS.filter((o) => (o !== "like" && o !== "ilike") || (col !== undefined && isTextLike(col)));
export interface FilterRow {
  col: string;
  op: FilterOp;
  value: string;
}
export interface FilterSet {
  mode: "and" | "or";
  rows: FilterRow[];
}

export type Parsed = { ok: true; value: unknown } | { ok: false; error: string };

export type InputKind = "bool" | "int" | "float" | "json" | "enum" | "text";

export function inputKind(col: ColumnDesc): InputKind {
  if (col.array) return "json";
  if (col.enum) return "enum";
  const t = col.type.toLowerCase();
  if (t === "boolean" || t === "bool") return "bool";
  if (/^(smallint|integer|bigint|int2|int4|int8|smallserial|serial|bigserial)$/.test(t)) return "int";
  if (/^(numeric|decimal|real|double precision|float4|float8)(\(.*\))?$/.test(t)) return "float";
  if (t === "json" || t === "jsonb") return "json";
  return "text";
}

const DECIMAL = /^-?(\d+(\.\d*)?|\.\d+)([eE][+-]?\d+)?$/;

/** A JSON number only when it reads back as the same text; otherwise the text (the server casts it). */
function exact(text: string): unknown {
  const n = Number(text);
  return Number.isFinite(n) && String(n) === text ? n : text;
}

/** A canonical `digits e exponent` form of a decimal literal, to compare two spellings of one number. */
function canon(literal: string): string | null {
  const m = /^(-?)(\d*)\.?(\d*)(?:[eE]([+-]?\d+))?$/.exec(literal);
  if (!m) return null;
  let digits = m[2] + m[3];
  let exp = Number(m[4] ?? 0) - m[3].length;
  digits = digits.replace(/^0+/, "");
  const trimmed = digits.replace(/0+$/, "");
  exp += digits.length - trimmed.length;
  return trimmed === "" ? "0" : `${m[1]}${trimmed}e${exp}`;
}

/** Whether a JSON number's source text reads back as the same number. */
function roundTrips(source: string, n: number): boolean {
  if (!Number.isFinite(n)) return false;
  if (/^-?\d+$/.test(source) && !Number.isSafeInteger(n)) return false;
  return canon(source) === canon(String(n));
}

export function parseValue(col: ColumnDesc, text: string): Parsed {
  const kind = inputKind(col);
  const t = text.trim();
  switch (kind) {
    case "bool":
      if (t === "true" || t === "false") return { ok: true, value: t === "true" };
      return { ok: false, error: "enter true or false" };
    case "int":
      if (!/^-?\d+$/.test(t)) return { ok: false, error: "enter a whole number" };
      return { ok: true, value: exact(t) };
    case "float":
      if (!DECIMAL.test(t) || !Number.isFinite(Number(t))) return { ok: false, error: "enter a finite decimal number" };
      return { ok: true, value: exact(t) };
    case "json": {
      let lossy: string | null = null;
      let value: unknown;
      try {
        value = JSON.parse(text, (_k, v: unknown, context?: { source?: string }) => {
          if (typeof v === "number" && context?.source !== undefined && !roundTrips(context.source, v)) lossy ??= context.source;
          return v;
        });
      } catch {
        return { ok: false, error: "enter valid JSON" };
      }
      if (lossy !== null) return { ok: false, error: `the number ${lossy} can't be kept exactly; write it as a JSON string` };
      return { ok: true, value };
    }
    default:
      return { ok: true, value: text };
  }
}

/** The where for one row: every primary key column equal to the row's value. */
export function whereByKey(pk: string[], row: Record<string, unknown>): Tree {
  if (pk.length === 0) throw new Error("no primary key to select the row by");
  const parts = pk.map((c) => {
    const v = row[c];
    if (v === undefined || v === null) throw new Error(`the row has no value for its key column ${c}`);
    if (typeof v === "number" && !Number.isSafeInteger(v)) throw new Error(`the key column ${c} holds a number that can't be matched exactly`);
    return { col: c, eq: v };
  });
  return parts.length === 1 ? parts[0] : { and: parts };
}

export type FilterResult = { ok: true; where: Tree | undefined } | { ok: false; error: string };

export function buildFilterTree(filters: FilterSet, columns: Record<string, ColumnDesc>): FilterResult {
  const parts: Tree[] = [];
  for (const [i, f] of filters.rows.entries()) {
    const col = columns[f.col];
    if (!col) return { ok: false, error: `Filter ${i + 1}: pick a column.` };
    const bad = (why: string): FilterResult => ({ ok: false, error: `Filter ${i + 1} (${f.col}): ${why}.` });
    if (f.op === "is") {
      const map: Record<string, unknown> = { null: null, true: true, false: false };
      if (!Object.hasOwn(map, f.value)) return bad("choose null, true or false");
      parts.push({ col: f.col, is: map[f.value] });
    } else if (f.op === "like" || f.op === "ilike") {
      if (!isTextLike(col)) return bad(`${f.op} works on text columns only`);
      parts.push({ col: f.col, [f.op]: f.value });
    } else if (f.op === "in") {
      const items = f.value.split(",").map((s) => s.trim());
      const values: unknown[] = [];
      for (const item of items) {
        const p = parseValue(col, item);
        if (!p.ok) return bad(p.error);
        values.push(p.value);
      }
      parts.push({ col: f.col, in: values });
    } else {
      const p = parseValue(col, f.value);
      if (!p.ok) return bad(p.error);
      parts.push({ col: f.col, [f.op]: p.value });
    }
  }
  if (parts.length === 0) return { ok: true, where: undefined };
  return { ok: true, where: parts.length === 1 ? parts[0] : { [filters.mode]: parts } };
}

export function buildSelect(args: {
  schema: string;
  table: string;
  page: number;
  sort: Sort | null;
  filters: Tree | null;
  /** Primary key columns: appended to the order so pages are stable. */
  pk: string[];
  /** Rows per page; the server clamps to its max_rows, so a smaller max_rows lowers it. */
  pageSize?: number;
}): Tree {
  const size = args.pageSize ?? PAGE_SIZE;
  const tree: Tree = { v: 1, op: "select", from: `${args.schema}.${args.table}` };
  if (args.filters) tree.where = args.filters;
  const order: { col: string; dir: SortDir }[] = args.sort ? [{ col: args.sort.col, dir: args.sort.dir }] : [];
  for (const c of args.pk) if (c !== args.sort?.col) order.push({ col: c, dir: "asc" });
  if (order.length > 0) tree.orderBy = order;
  tree.limit = size;
  tree.offset = args.page * size;
  return tree;
}

/** The page size for a database whose max_rows is `maxRows` (unknown: the default). */
export const pageSizeFor = (maxRows: number | undefined) => Math.max(1, Math.min(PAGE_SIZE, maxRows ?? PAGE_SIZE));

export type ConsoleOp = "select" | "insert" | "update" | "delete";
const VERB: Record<ConsoleOp, string> = {
  select: "read this table",
  insert: "insert into this table",
  update: "update this table",
  delete: "delete from this table",
};

/** Plain text for an error of the console route; unknown codes keep the server's message. */
export function consoleErrorText(err: unknown, ctx: { role: string | null; op: ConsoleOp }): string {
  const text = describe(err, ctx);
  return text === "" && err instanceof ApiError ? `Something went wrong (${err.code})` : text;
}

function describe(err: unknown, ctx: { role: string | null; op: ConsoleOp }): string {
  if (!(err instanceof ApiError)) return err instanceof Error ? err.message : "Something went wrong";
  const code: string = err.code;
  const who = ctx.role ?? "The owner";
  switch (code) {
    case "network":
      return "Can't reach Chelabase; check your connection";
    case "permission_denied":
      return `${who} can't ${VERB[ctx.op]}.`;
    case "data_api_disabled":
      return "The data API isn't enabled for this database. Enable it in the Data API view first.";
    case "not_found":
      return "That branch or database no longer exists.";
    case "owner_password_unknown":
      return "The owner password isn't known to Chelabase (it was set in SQL); reset it to continue.";
    case "role_refused":
      return `${who} can't be used here: it doesn't exist, or it is owner-level (${OWNER_LEVEL_TRAITS}), so the console won't run as it.`;
    case "role_not_allowed":
      return `${who} isn't allowed to use this database.`;
    case "unknown_table":
      return "That table or view doesn't exist, or the role can't see it.";
    case "unknown_column":
      return `A column doesn't exist: ${err.message}`;
    case "invalid_query":
      return `The request was refused: ${err.message}`;
    case "invalid_value":
      return `A value isn't valid for its column: ${err.message}`;
    case "conflict":
      return `The change conflicts with existing data: ${err.message}`;
    case "raised":
      return err.message;
    case "retry":
      return "The change collided with another one; try again.";
    case "timeout":
      return "The request took too long and was stopped.";
    case "branch_not_ready":
    case "unavailable":
      return "The branch isn't reachable right now; try again in a moment.";
    default:
      return err.message;
  }
}
