// The runtime builders. One immutable `Query` class backs every typed builder interface in
// `types.ts`; each method returns a new `Query`.
import type { ResultSpec } from "./json.ts";
import type { PgExact, QueryTree, TypeMap } from "./types.ts";

/** What the builders need from their client. */
export interface Runner {
  run(tree: QueryTree, spec: ResultSpec): Promise<unknown>;
  types: TypeMap | undefined;
}

type Kind = "select" | "insert" | "update" | "delete" | "call" | "batch";
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

function isOperatorObject(v: unknown): v is Record<string, unknown> {
  if (!isPlainObject(v)) return false;
  const keys = Object.keys(v);
  return keys.length > 0 && keys.every((k) => OPERATORS.has(k));
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

function conditions(col: string, ops: Record<string, unknown>, where: string): QueryTree[] {
  return Object.entries(ops).map(([op, value]) => {
    if (value === undefined) {
      throw new TypeError(`${where}: \`${op}\` on "${col}" is undefined`);
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
    } else if (isOperatorObject(value)) {
      parts.push(...conditions(key, value, at));
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
    return both(conditions(col, ops, where))!;
  }
  if (!isPlainObject(item)) throw new TypeError(`${where} is not a filter object`);
  const compiled = compileWhere(item, where);
  if (compiled === undefined) throw new TypeError(`${where} is an empty filter`);
  return compiled;
}
