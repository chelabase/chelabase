/**
 * `@cheladb/client`: a typed client for the Cheladb data API. No runtime dependencies;
 * built on `fetch`, so it runs in browsers, Node and edge runtimes. See the README.
 *
 * @module
 */
import { DataApiError } from "./error.ts";
import { hasSourceText, parseResult, type ResultSpec, stringify } from "./json.ts";
import { batchOf, callFn, type Runner, selectFrom } from "./query.ts";
import type { Client, DatabaseShape, QueryTree, TypeMap } from "./types.ts";

export { DataApiError, UnsafeIntegerError } from "./error.ts";
export type {
  Client,
  Condition,
  DatabaseShape,
  Direction,
  FunctionName,
  InsertQuery,
  Int8,
  Json,
  Nulls,
  Operation,
  Ops,
  PgExact,
  QueryTree,
  Resolve,
  ResolveRow,
  SelectItem,
  SelectQuery,
  TableName,
  TableQuery,
  Tx,
  TypeMap,
  Where,
  WhereItem,
  UnfilteredWrite,
  WriteQuery,
} from "./types.ts";

export interface ClientOptions {
  /** The gateway's base URL, e.g. `http://localhost:8000`. */
  url: string;
  /** A public (`chk_pub_…`) or secret (`chk_sec_…`) data key, sent as a bearer token. */
  key: string;
  /** Defaults to the global `fetch`. */
  fetch?: typeof fetch;
  /** The generated `types` map: which columns, arguments and returns are int8 or numeric. */
  types?: TypeMap;
}

const RUNTIMES = "Node 21+, Chrome 114+, Firefox 135+ or Safari 18.4+";

/**
 * Creates a client. `Database` is the type `cheladb gen types` writes; pass its `types`
 * too so int8 and numeric values are read exactly. With a generated `Database`, `types`
 * is required (a compile error without it; the generic is erased at run time, so there
 * is no run-time check). The untyped `createClient({ url, key })` needs none.
 *
 * - default: int8 → `number`, throwing `UnsafeIntegerError` outside ±(2^53 − 1);
 * - `bigint: true` (requires `types`): int8 → `bigint`, always
 *   (`createClient<Database>({ …, types, bigint: true })`).
 *
 * numeric is always an exact `string`.
 */
export function createClient<DB extends DatabaseShape = DatabaseShape>(
  opts: ClientOptions & { bigint: true; types: TypeMap },
): Client<DB, true>;
export function createClient<DB extends DatabaseShape = DatabaseShape, B extends boolean = false>(
  opts: ClientOptions &
    (B extends true ? { bigint: true; types: TypeMap } : { bigint?: false }) &
    // A generated `Database` (anything narrower than the default shape) needs its `types`.
    (DatabaseShape extends DB ? unknown : { types: TypeMap }),
): Client<DB, B>;
export function createClient<DB extends DatabaseShape, B extends boolean = false>(
  opts: ClientOptions & { bigint?: B },
): Client<DB, B> {
  const bigint = opts.bigint === true;
  if (bigint && opts.types === undefined) {
    throw new Error(
      "createClient({ bigint: true }) needs the generated `types` (from `cheladb gen types`) to know which values are int8.",
    );
  }
  const sourceText = hasSourceText();
  const doFetch = opts.fetch ?? globalThis.fetch.bind(globalThis);
  const endpoint = `${opts.url.replace(/\/+$/, "")}/data/v1/query`;

  // One client per (database, types): `database()` builds a sibling bound to another database.
  const make = (types: TypeMap | undefined, database: string | undefined): unknown => {
    if (bigint && types === undefined) {
      throw new Error(
        "createClient({ bigint: true }) needs the generated `types` (from `cheladb gen types`) to know which values are int8.",
      );
    }
    if (!sourceText) {
      if (bigint) {
        throw new Error(
          `createClient({ bigint: true }) needs JSON.parse source text access (${RUNTIMES}) to read int8 values exactly.`,
        );
      }
      if (types !== undefined && mentionsNumeric(types)) {
        throw new Error(
          `createClient({ types }): these types include numeric columns, which need JSON.parse source text access (${RUNTIMES}) to read exactly.`,
        );
      }
    }

    const runner: Runner = {
      types,
      async run(tree: QueryTree, spec: ResultSpec): Promise<unknown> {
        // The top level only: the ops of a batch are built without `v`, and never name a database.
        const body = database === undefined ? tree : { ...tree, database };
        const res = await doFetch(endpoint, {
          method: "POST",
          headers: {
            authorization: `Bearer ${opts.key}`,
            "content-type": "application/json",
            accept: "application/json",
          },
          body: stringify(body),
        });
        const text = await res.text();
        if (!res.ok) throw toError(res, text);
        try {
          return parseResult(text, spec, bigint, sourceText);
        } catch (e) {
          if (e instanceof SyntaxError) throw plainError(res, text, "internal");
          throw e;
        }
      },
    };

    const batchRunner: Runner = {
      types,
      run: () => Promise.reject(new Error("an operation built inside batch() runs only as part of its batch")),
    };

    const tx = (r: Runner) => ({
      from: (name: string) => selectFrom(r, name),
      call: (fn: string, args?: unknown) => callFn(r, fn, args),
    });

    return {
      ...tx(runner),
      batch: (fn: (t: unknown) => unknown) => batchOf(runner, fn(tx(batchRunner))),
      database: (name: string, t?: TypeMap) => make(t, name),
    };
  };

  return make(opts.types, undefined) as Client<DB, B>;
}

function mentionsNumeric(types: TypeMap): boolean {
  return Object.values(types.schemas).some(
    (s) =>
      Object.values(s.tables).some((t) => Object.values(t).includes("numeric")) ||
      Object.values(s.functions).some(
        (f) => f.returns === "numeric" || Object.values(f.columns ?? {}).includes("numeric"),
      ),
  );
}

function toError(res: Response, text: string): DataApiError {
  try {
    const body = JSON.parse(text) as { error?: Record<string, unknown> };
    const e = body.error;
    if (e && typeof e["code"] === "string" && typeof e["message"] === "string") {
      const opt = (k: string) => (typeof e[k] === "string" ? (e[k] as string) : undefined);
      return new DataApiError({
        status: res.status,
        code: e["code"],
        message: e["message"],
        detail: opt("detail"),
        hint: opt("hint"),
        sqlstate: opt("sqlstate"),
      });
    }
  } catch {
    // not JSON: fall through
  }
  return plainError(res, text, STATUS_CODES[res.status] ?? "internal");
}

/** The `code` for a non-2xx answer without the data API's error body (a proxy's page). */
const STATUS_CODES: Record<number, string> = {
  401: "unauthorized",
  413: "too_large",
  429: "rate_limited",
  502: "unavailable",
  503: "unavailable",
  504: "timeout",
};

function plainError(res: Response, text: string, code: string): DataApiError {
  const snippet = text.length > 200 ? `${text.slice(0, 200)}…` : text;
  return new DataApiError({
    status: res.status,
    code,
    message: `HTTP ${res.status}${res.statusText ? ` ${res.statusText}` : ""}${snippet ? `: ${snippet}` : ""}`,
  });
}
