// Exact JSON in both directions: `bigint` values are written as bare digits, and int8 /
// numeric results are read from their source text (`JSON.parse` source text access).
import { UnsafeIntegerError } from "./error.ts";
import type { PgExact } from "./types.ts";

/** `JSON.stringify` semantics, except that a `bigint` is written as its exact digits. */
export function stringify(value: unknown): string {
  return write(value, "") ?? "null";
}

function write(value: unknown, key: string): string | undefined {
  if (value !== null && typeof value === "object" && typeof (value as { toJSON?: unknown }).toJSON === "function") {
    value = (value as { toJSON(key: string): unknown }).toJSON(key);
  }
  switch (typeof value) {
    case "bigint":
      return value.toString();
    case "string":
      return JSON.stringify(value);
    case "number":
      return Number.isFinite(value) ? JSON.stringify(value) : "null";
    case "boolean":
      return value ? "true" : "false";
    case "object": {
      if (value === null) return "null";
      if (Array.isArray(value)) {
        return `[${value.map((v, i) => write(v, String(i)) ?? "null").join(",")}]`;
      }
      const parts: string[] = [];
      for (const [k, v] of Object.entries(value)) {
        const s = write(v, k);
        if (s !== undefined) parts.push(`${JSON.stringify(k)}:${s}`);
      }
      return `{${parts.join(",")}}`;
    }
    default:
      return undefined; // undefined, functions, symbols: dropped like JSON.stringify
  }
}

type Reviver = (this: unknown, key: string, value: unknown, context?: { source?: string }) => unknown;
// Looked up on each call, not captured at load, so a polyfill or a test stub is honoured.
const parseWith = (text: string, reviver: Reviver): unknown =>
  (JSON.parse as (text: string, reviver?: Reviver) => unknown)(text, reviver);

/** Whether this runtime's `JSON.parse` passes the source text to revivers. */
export function hasSourceText(): boolean {
  let seen = false;
  try {
    parseWith("1", (_key, value, context) => {
      seen = typeof context?.source === "string";
      return value;
    });
  } catch {
    return false;
  }
  return seen;
}

/** What a response holds, and which of its keys to read exactly. */
export type ResultSpec =
  /** No type information: any integer literal outside the safe range throws. `name` labels a scalar result. */
  | { kind: "untyped"; name?: string }
  /**
   * A list of rows, one row (a plain object) or a scalar; these top-level row keys are exact. Every other
   * top-level value (a scalar, or an array of scalars) still throws outside the safe
   * range; objects (json/jsonb contents) are left as parsed.
   */
  | { kind: "rows"; columns: Record<string, PgExact>; name?: string }
  /** A scalar (or a list of scalars, for a set-returning function) of this kind. */
  | { kind: "scalar"; exact: PgExact; name: string }
  | { kind: "batch"; items: ResultSpec[] };

/**
 * A count's result, `{ "count": N }`: one row whose `count` is always a `number` (in
 * bigint mode too); a count beyond `Number.MAX_SAFE_INTEGER` throws `UnsafeIntegerError`.
 */
export const COUNT_SPEC: ResultSpec = { kind: "rows", columns: {}, name: "count" };

const INTEGER = /^-?\d+$/;

/**
 * Parses a response body. With `sourceText`, exact keys are read from their digits:
 * numeric → string, int8 → bigint (`bigint`) or a safe number. Without it (an older
 * runtime), int8 and untyped results are checked with `Number.isSafeInteger`.
 */
export function parseResult(text: string, spec: ResultSpec, bigint: boolean, sourceText: boolean): unknown {
  const sources = new WeakMap<object, Map<string, string>>();
  let lastSource: string | undefined;
  const value = sourceText
    ? parseWith(text, function (key, v, context) {
        if (typeof v === "number" && context?.source !== undefined) {
          const holder = this as object;
          let m = sources.get(holder);
          if (!m) sources.set(holder, (m = new Map()));
          m.set(key, context.source);
          lastSource = context.source;
        }
        return v;
      })
    : JSON.parse(text);

  // The root has no holder we can reach; a numeric root is the only number parsed.
  const sourceOf = (holder: object | undefined, key: string): string | undefined =>
    holder === undefined ? lastSource : sources.get(holder)?.get(key);

  const exact = (v: unknown, kind: PgExact, column: string, holder: object | undefined, key: string): unknown => {
    if (Array.isArray(v)) {
      for (let i = 0; i < v.length; i++) v[i] = exact(v[i], kind, column, v, String(i));
      return v;
    }
    if (typeof v !== "number") return v; // null, or a string such as numeric's "NaN"
    const source = (sourceText ? sourceOf(holder, key) : undefined) ?? String(v);
    if (kind === "numeric") return source;
    if (bigint && INTEGER.test(source)) return BigInt(source);
    if (!Number.isSafeInteger(v)) throw new UnsafeIntegerError(column, source);
    return v;
  };

  /**
   * Throws for an integer outside the safe range. Without source text, an integer is a
   * value whose `String()` has no exponent or point: this catches every int8, but also
   * flags a float in [2^53, 1e21) such as 1.5e20, which prints as plain digits.
   */
  const checkNumber = (v: number, column: string, holder: object | undefined, key: string): void => {
    if (Number.isSafeInteger(v) || !Number.isInteger(v)) return;
    if (sourceText) {
      const source = sourceOf(holder, key);
      if (source !== undefined && INTEGER.test(source)) throw new UnsafeIntegerError(column, source);
    } else {
      const text = String(v);
      if (!/[e.]/i.test(text)) throw new UnsafeIntegerError(column, text);
    }
  };

  /** Everywhere, through arrays and objects (no type information at all). */
  const check = (v: unknown, column: string, holder: object | undefined, key: string): void => {
    if (typeof v === "number") {
      checkNumber(v, column, holder, key);
    } else if (Array.isArray(v)) {
      v.forEach((item, i) => check(item, column, v, String(i)));
    } else if (v !== null && typeof v === "object") {
      for (const [k, item] of Object.entries(v)) check(item, k, v, k);
    }
  };

  /** A scalar or an array of scalars; objects (json/jsonb contents) are skipped. */
  const checkShallow = (v: unknown, column: string, holder: object | undefined, key: string): void => {
    if (typeof v === "number") {
      checkNumber(v, column, holder, key);
    } else if (Array.isArray(v)) {
      v.forEach((item, i) => checkShallow(item, column, v, String(i)));
    }
  };

  const apply = (v: unknown, s: ResultSpec, holder: object | undefined, key: string): unknown => {
    switch (s.kind) {
      case "untyped":
        check(v, s.name ?? (key === "" ? "(result)" : key), holder, key);
        return v;
      case "scalar":
        return exact(v, s.exact, s.name, holder, key);
      case "rows": {
        const label = s.name ?? (key === "" ? "(result)" : key);
        const mapRow = (r: Record<string, unknown>): void => {
          for (const [col, item] of Object.entries(r)) {
            const kind = Object.hasOwn(s.columns, col) ? s.columns[col] : undefined;
            if (kind !== undefined) r[col] = exact(item, kind, col, r, col);
            else checkShallow(item, col, r, col);
          }
        };
        const isRow = (x: unknown): x is Record<string, unknown> =>
          x !== null && typeof x === "object" && !Array.isArray(x);
        if (isRow(v)) {
          // A non-set, row-returning function answers with one row, not a list.
          mapRow(v);
          return v;
        }
        if (!Array.isArray(v)) {
          checkShallow(v, label, holder, key);
          return v;
        }
        v.forEach((row, i) => {
          if (isRow(row)) mapRow(row);
          else checkShallow(row, label, v, String(i));
        });
        return v;
      }
      case "batch":
        if (Array.isArray(v)) {
          s.items.forEach((item, i) => {
            if (i < v.length) v[i] = apply(v[i], item, v, String(i));
          });
        }
        return v;
    }
  };

  return apply(value, spec, undefined, "");
}
