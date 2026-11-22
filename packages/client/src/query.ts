// The runtime builders. One immutable `Query` class backs every typed builder interface in
// `types.ts`; each method returns a new `Query`.
import { COUNT_SPEC, type ResultSpec } from "./json.ts";
import type { Op, PgExact, QueryTree, TypeMap } from "./types.ts";

/** What the builders need from their client. */
export interface Runner {
  run(tree: QueryTree, spec: ResultSpec): Promise<unknown>;
  types: TypeMap | undefined;
}

type Kind = "select" | "insert" | "update" | "delete" | "call" | "count" | "batch";
type SelectItem = string | { col: string; as: string };

interface State {
  kind: Kind;
  /** A table or function name, as given. */
  name: string;
  select?: SelectItem[];
  where?: QueryTree;
  orderBy?: QueryTree[];
  limit?: number;
  offset?: number;
  values?: unknown[];
  set?: unknown;
  all?: boolean;
  returning?: "*" | string[];
  args?: unknown;
  ops?: Query[];
}

const OPERATORS = new Set(["eq", "neq", "lt", "lte", "gt", "gte", "in", "is", "like", "ilike"]);

/**
 * What the `op` helpers return. The only way to get an operator into `where()`: a bare
 * object, whatever its keys, is an equality value. The `#ops` private field is the brand, so
 * a value parsed from JSON (or built by hand) can never pass as one.
 */
class OpValue {
  readonly #ops: readonly (readonly [string, unknown])[];

  constructor(ops: readonly (readonly [string, unknown])[]) {
    this.#ops = ops;
    Object.freeze(this);
  }

  static is(v: unknown): v is OpValue {
    return typeof v === "object" && v !== null && #ops in v;
  }

  static entries(v: OpValue): readonly (readonly [string, unknown])[] {
    return v.#ops;
  }
}

const single = (name: string, value: unknown): Op<never> => new OpValue([[name, value]]) as unknown as Op<never>;

/**
 * The operators for `where()`: `where({ total: op.gte("10") })`. Several on one column:
 * `op.all(op.gte("10"), op.lt("20"))`, combined with `and`. A bare object in `where()` is
 * always a value to compare with.
 */
export const op = Object.freeze({
  eq: <V>(value: V): Op<V> => single("eq", value) as Op<V>,
  neq: <V>(value: V): Op<V> => single("neq", value) as Op<V>,
  lt: <V>(value: V): Op<V> => single("lt", value) as Op<V>,
  lte: <V>(value: V): Op<V> => single("lte", value) as Op<V>,
  gt: <V>(value: V): Op<V> => single("gt", value) as Op<V>,
  gte: <V>(value: V): Op<V> => single("gte", value) as Op<V>,
  /** `op.in([])` matches no rows. */
  in: <V>(values: readonly V[]): Op<V> => single("in", values) as Op<V>,
  /** `null` on any column; `true`/`false` on boolean columns. */
  is: ((value: null | boolean) => single("is", value)) as {
    (value: null): Op<never>;
    (value: boolean): Op<boolean>;
  },
  /** Text columns only. */
  like: (pattern: string): Op<string> => single("like", pattern) as Op<string>,
  /** Text columns only. */
  ilike: (pattern: string): Op<string> => single("ilike", pattern) as Op<string>,
  /** Several operators on one column, combined with `and`. */
  all: <V>(...ops: [Op<V>, ...Op<V>[]]): Op<V> => {
    const entries = ops.flatMap((o) => {
      if (!OpValue.is(o)) throw new TypeError("op.all() takes results of the `op` helpers");
      return [...OpValue.entries(o)];
    });
    if (entries.length === 0) throw new TypeError("op.all() needs at least one operator");
    return new OpValue(entries) as unknown as Op<V>;
  },
});

export class Query implements PromiseLike<unknown> {
  readonly #runner: Runner;
  readonly #state: State;

  constructor(runner: Runner, state: State) {
    this.#runner = runner;
    this.#state = state;
  }

  #with(patch: Partial<State>): Query {
    return new Query(this.#runner, { ...this.#state, ...patch });
  }

  select(...cols: SelectItem[]): Query {
    return this.#with({ select: cols.map((c) => (typeof c === "string" ? c : { col: c.col, as: c.as })) });
  }

  where(filter: object): Query {
    const compiled = compileWhere(filter, "where");
    if (compiled === undefined) return this;
    const prev = this.#state.where;
    return this.#with({ where: prev === undefined ? compiled : { and: [prev, compiled] } });
  }

  orderBy(col: string, dir?: string, nulls?: string): Query {
    const key: QueryTree = { col };
    if (dir !== undefined) key["dir"] = dir;
    if (nulls !== undefined) key["nulls"] = nulls;
    return this.#with({ orderBy: [...(this.#state.orderBy ?? []), key] });
  }

  limit(n: number): Query {
    return this.#with({ limit: n });
  }

  offset(n: number): Query {
    return this.#with({ offset: n });
  }

  insert(rows: unknown): Query {
    return this.#with({ kind: "insert", values: Array.isArray(rows) ? [...rows] : [rows] });
  }

  update(set: unknown): Query {
    return this.#with({ kind: "update", set });
  }

  delete(): Query {
    return this.#with({ kind: "delete" });
  }

  /** The number of matching rows: only the table and the filter are sent. */
  count(): Query {
    const s = this.#state;
    if (s.limit !== undefined || s.offset !== undefined) {
      throw new TypeError("count(): a count takes no limit or offset; it counts every matching row");
    }
    return new Query(this.#runner, s.where === undefined ? { kind: "count", name: s.name } : { kind: "count", name: s.name, where: s.where });
  }

  all(): Query {
    return this.#with({ all: true });
  }

  returning(...cols: string[]): Query {
    return this.#with({ returning: cols.length === 1 && cols[0] === "*" ? "*" : cols });
  }

  toTree(): QueryTree {
    return { v: 1, ...this.#body() };
  }

  /** The tree without `v`: what a batch holds. */
  #body(): QueryTree {
    const s = this.#state;
    const t: QueryTree = { op: s.kind };
    switch (s.kind) {
      case "select":
        t["from"] = s.name;
        if (s.select !== undefined) t["select"] = s.select;
        if (s.where !== undefined) t["where"] = s.where;
        if (s.orderBy !== undefined) t["orderBy"] = s.orderBy;
        if (s.limit !== undefined) t["limit"] = s.limit;
        if (s.offset !== undefined) t["offset"] = s.offset;
        break;
      case "insert":
        t["into"] = s.name;
        t["values"] = s.values;
        if (s.returning !== undefined) t["returning"] = s.returning;
        break;
      case "update":
      case "delete":
        if (s.kind === "update") {
          t["table"] = s.name;
          t["set"] = s.set;
        } else {
          t["from"] = s.name;
        }
        if (s.where !== undefined) t["where"] = s.where;
        if (s.all) t["all"] = true;
        if (s.returning !== undefined) t["returning"] = s.returning;
        break;
      case "call":
        t["fn"] = s.name;
        if (s.args !== undefined) t["args"] = s.args;
        break;
      case "count":
        t["from"] = s.name;
        if (s.where !== undefined) t["where"] = s.where;
        break;
      case "batch":
        t["ops"] = (s.ops ?? []).map((op) => op.#body());
        break;
    }
    return t;
  }

  /** Which result keys to read exactly, from the client's type map. */
  spec(): ResultSpec {
    const s = this.#state;
    const types = this.#runner.types;
    switch (s.kind) {
      case "batch":
        return { kind: "batch", items: (s.ops ?? []).map((op) => op.spec()) };
      case "count":
        return COUNT_SPEC;
      case "call": {
        const fn = types === undefined ? undefined : lookup(types, s.name, "functions");
        if (fn === undefined) return { kind: "untyped", name: s.name };
        if (fn.returns !== undefined) return { kind: "scalar", exact: fn.returns, name: s.name };
        return { kind: "rows", columns: { ...fn.columns }, name: s.name };
      }
      default: {
        if (s.kind !== "select" && s.returning === undefined) return { kind: "rows", columns: {} };
        if (types === undefined) return { kind: "untyped" };
        const table = lookup(types, s.name, "tables");
        if (table === undefined) return { kind: "untyped" };
        const columns: Record<string, PgExact> = {};
        const items: SelectItem[] | "*" =
          s.kind === "select" ? (s.select ?? "*") : (s.returning as "*" | string[]);
        if (items === "*") return { kind: "rows", columns: { ...table } };
        for (const item of items) {
          const [key, col] = typeof item === "string" ? [item, item] : [item.as, item.col];
          const kind = Object.hasOwn(table, col) ? table[col] : undefined;
          if (kind !== undefined) columns[key] = kind;
        }
        return { kind: "rows", columns };
      }
    }
  }

  catch<R = never>(onrejected?: ((reason: unknown) => R | PromiseLike<R>) | null): Promise<unknown> {
    return this.then(undefined, onrejected);
  }

  finally(onfinally?: (() => void) | null): Promise<unknown> {
    return this.then().finally(onfinally);
  }

  then<R1 = unknown, R2 = never>(
    onfulfilled?: ((value: unknown) => R1 | PromiseLike<R1>) | null,
    onrejected?: ((reason: unknown) => R2 | PromiseLike<R2>) | null,
  ): Promise<R1 | R2> {
    let result: Promise<unknown>;
    try {
      result = this.#runner.run(this.toTree(), this.spec());
    } catch (e) {
      result = Promise.reject(e);
    }
    return result.then(onfulfilled, onrejected);
  }
}

export function selectFrom(runner: Runner, name: string): Query {
  return new Query(runner, { kind: "select", name });
}

export function callFn(runner: Runner, name: string, args: unknown): Query {
  return new Query(runner, args === undefined ? { kind: "call", name } : { kind: "call", name, args });
}

export function batchOf(runner: Runner, ops: unknown): Query {
  if (!Array.isArray(ops) || !ops.every((op) => op instanceof Query)) {
    throw new TypeError("batch(): the callback must return a list of operations built from its `tx`");
  }
  return new Query(runner, { kind: "batch", name: "", ops });
}

/** The type map entry for `name` (`"t"` or `"s.t"`), or `undefined` when it isn't mapped. */
function lookup<K extends "tables" | "functions">(
  types: TypeMap,
  name: string,
  kind: K,
): TypeMap["schemas"][string][K][string] | undefined {
  const dot = name.indexOf(".");
  const own = <T extends object>(o: T | undefined, k: string) =>
    o !== undefined && Object.hasOwn(o, k) ? (o as Record<string, unknown>)[k] : undefined;
  if (dot >= 0) {
    const schema = own(types.schemas, name.slice(0, dot)) as TypeMap["schemas"][string] | undefined;
    return own(schema?.[kind], name.slice(dot + 1)) as TypeMap["schemas"][string][K][string] | undefined;
  }
  if (types.defaultSchema !== undefined) {
    const schema = own(types.schemas, types.defaultSchema) as TypeMap["schemas"][string] | undefined;
    return own(schema?.[kind], name) as TypeMap["schemas"][string][K][string] | undefined;
  }
  const found = Object.values(types.schemas).filter((s) => own(s[kind], name) !== undefined);
  return found.length === 1 ? (own(found[0]![kind], name) as TypeMap["schemas"][string][K][string]) : undefined;
}

// --- where -----------------------------------------------------------------------------

function isPlainObject(v: unknown): v is Record<string, unknown> {
  if (v === null || typeof v !== "object") return false;
  const proto = Object.getPrototypeOf(v);
  return proto === Object.prototype || proto === null;
}


/** `{ col: "x", <operator>: v, … }`: a condition object (reaches any column name). */
function isCondition(v: unknown): v is Record<string, unknown> {
  if (!isPlainObject(v) || typeof v["col"] !== "string") return false;
  const ops = Object.keys(v).filter((k) => k !== "col");
  return ops.length > 0 && ops.every((k) => OPERATORS.has(k));
}

function both(parts: QueryTree[]): QueryTree | undefined {
  return parts.length === 0 ? undefined : parts.length === 1 ? parts[0] : { and: parts };
}

function conditions(col: string, ops: Iterable<readonly [string, unknown]>, where: string): QueryTree[] {
  return [...ops].map(([op, value]) => {
    if (value === undefined) {
      throw new TypeError(`${where}: \`${op}\` on "${col}" is undefined`);
    }
    if (OpValue.is(value)) {
      throw new TypeError(`${where}: \`${op}\` on "${col}" is an \`op\` value; use it as the column's value, not inside another operator`);
    }
    return { col, [op]: value };
  });
}

/** Compiles a filter object into the tree's filter; `undefined` when it has no keys. */
function compileWhere(filter: object, where: string): QueryTree | undefined {
  const parts: QueryTree[] = [];
  for (const [key, value] of Object.entries(filter)) {
    const at = `${where}.${key}`;
    if (value === undefined) {
      throw new TypeError(`${at} is undefined: leave the key out, or use null for "is null"`);
    }
    if (key === "and" || key === "or") {
      if (!Array.isArray(value)) throw new TypeError(`${at} takes a list of filters`);
      parts.push({ [key]: value.map((item, i) => compileItem(item, `${at}[${i}]`)) });
    } else if (key === "not") {
      parts.push({ not: compileItem(value, at) });
    } else if (value === null) {
      parts.push({ col: key, is: null });
    } else if (OpValue.is(value)) {
      parts.push(...conditions(key, OpValue.entries(value), at));
    } else {
      parts.push({ col: key, eq: value });
    }
  }
  return both(parts);
}

/** An item of an `and`/`or` list, or the filter under `not`. */
function compileItem(item: unknown, where: string): QueryTree {
  if (isCondition(item)) {
    const { col, ...ops } = item as { col: string } & Record<string, unknown>;
    return both(conditions(col, Object.entries(ops), where))!;
  }
  if (!isPlainObject(item)) throw new TypeError(`${where} is not a filter object`);
  const compiled = compileWhere(item, where);
  if (compiled === undefined) throw new TypeError(`${where} is an empty filter`);
  return compiled;
}
