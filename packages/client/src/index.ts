/**
 * `@chelabase/client`: a typed client for the Chelabase data API. No runtime dependencies;
 * built on `fetch`, so it runs in browsers, Node and edge runtimes. See the README.
 *
 * @module
 */
import { createAuth, SESSION_GONE, USER_HEADER } from "./auth.ts";
import { plainError, toError } from "./error.ts";
import { hasSourceText, parseResult, type ResultSpec, stringify } from "./json.ts";
import { batchOf, callFn, type Runner, selectFrom } from "./query.ts";
import type { AuthOptions, Client, DatabaseShape, QueryTree, TypeMap } from "./types.ts";

export { DataApiError, UnsafeIntegerError } from "./error.ts";
export { fetchTypes, type FetchTypesOptions } from "./fetch-types.ts";
export type {
  AppUser,
  Auth,
  AuthEvent,
  AuthOptions,
  AuthStorage,
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
  Session,
  SignUpData,
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
  /** The gateway's base URL, e.g. `http://api.chelabase.localhost:8000`. */
  url: string;
  /** A public (`chk_pub_…`) or secret (`chk_sec_…`) data key, sent as a bearer token. */
  key: string;
  /** Defaults to the global `fetch`. */
  fetch?: typeof fetch;
  /** The generated `types` map: which columns, arguments and returns are int8 or numeric. */
  types?: TypeMap;
  /** App user sessions (`db.auth`): where the refresh token is kept, and automatic refresh. */
  auth?: AuthOptions;
}

const RUNTIMES = "Node 21+, Chrome 114+, Firefox 135+ or Safari 18.4+";

/**
 * Creates a client. `Database` is the type `chelabase gen types` writes; pass its `types`
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
      "createClient({ bigint: true }) needs the generated `types` (from `chelabase gen types`) to know which values are int8.",
    );
  }
  const sourceText = hasSourceText();
  const doFetch = opts.fetch ?? globalThis.fetch.bind(globalThis);
  const base = opts.url.replace(/\/+$/, "");
  const endpoint = `${base}/data/v1/query`;
  // One session per client, shared by every `database()` sibling.
  const core = createAuth<DB>({ base, key: opts.key, fetch: doFetch, options: opts.auth, toError });

  // One client per (database, types): `database()` builds a sibling bound to another database.
  const make = (types: TypeMap | undefined, database: string | undefined): unknown => {
    if (bigint && types === undefined) {
      throw new Error(
        "createClient({ bigint: true }) needs the generated `types` (from `chelabase gen types`) to know which values are int8.",
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
        const payload = stringify(body);
        const send = (userToken: string | undefined) =>
          doFetch(endpoint, {
            method: "POST",
            headers: {
              authorization: `Bearer ${opts.key}`,
              "content-type": "application/json",
              accept: "application/json",
              ...(userToken === undefined ? {} : { [USER_HEADER]: userToken }),
            },
            body: payload,
          });
        let token = await core.accessToken();
        let res = await send(token);
        let text = await res.text();
        if (!res.ok && token !== undefined && res.status === 401 && toError(res, text).code === "user_token_expired") {
          // One refresh, one retry.
          token = await core.refreshAfterExpiry(token);
          res = await send(token);
          text = await res.text();
        }
        if (!res.ok) {
          const e = toError(res, text);
          if (token !== undefined && e.code === SESSION_GONE) await core.dropSession(token);
          throw e;
        }
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
      auth: core.auth,
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
