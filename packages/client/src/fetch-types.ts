// `fetchTypes`: the runtime `types` map, fetched live from `GET /data/v1/types?format=json`.

import { NO_REDIRECTS, plainError, toError } from "./error.ts";
import type { TypeMap } from "./types.ts";

export interface FetchTypesOptions {
  /** The gateway's base URL, e.g. `http://api.chelabase.localhost:8000`. */
  url: string;
  /** A secret data key (`chk_sec_…`): only a secret key can read the schema. Keep it on the server. */
  key: string;
  /** This database (not the key's main one), as `chelabase gen types --database`. */
  database?: string;
  /** Every database the key reaches, keyed by name (main as `main`), as `--all-databases`. */
  allDatabases?: boolean;
  /** Defaults to the global `fetch`. */
  fetch?: typeof fetch;
}

const NEEDS_SECRET =
  "fetchTypes needs a secret key (chk_sec_…); a public key, or anything that isn't a data key, can't read the schema";

/**
 * Fetches the runtime `types` map for the key's schema: exactly the `types` constant
 * `chelabase gen types` would write now. Pass it to `createClient` so values decode by the
 * live schema (`int8`, `numeric`) without a rebuild:
 *
 * ```ts
 * const db = createClient<Database>({ url, key, types: await fetchTypes({ url, key: secretKey }) });
 * ```
 *
 * Compile-time checking still comes from the generated `db.ts` (`Database`) in the build.
 * With `allDatabases`, the result is keyed by database name (main as `main`, also when it
 * is the only one). A failed call throws `DataApiError`; the key is never in an error.
 */
export function fetchTypes(opts: FetchTypesOptions & { allDatabases: true }): Promise<Record<string, TypeMap>>;
export function fetchTypes(opts: FetchTypesOptions & { allDatabases?: false }): Promise<TypeMap>;
export function fetchTypes(opts: FetchTypesOptions): Promise<TypeMap | Record<string, TypeMap>>;
export async function fetchTypes(opts: FetchTypesOptions): Promise<TypeMap | Record<string, TypeMap>> {
  // As the CLI checks it: trimmed, then `chk_sec_` and 43 base62 characters.
  const key = opts.key.trim();
  if (!/^chk_sec_[0-9A-Za-z]{43}$/.test(key)) throw new Error(NEEDS_SECRET);
  const all = opts.allDatabases === true;
  if (all && opts.database !== undefined) {
    throw new Error("fetchTypes: `database` and `allDatabases` can't be combined");
  }
  const query = new URLSearchParams({ format: "json" });
  if (opts.database !== undefined) query.set("database", opts.database);
  if (all) query.set("all_databases", "true");
  const doFetch = opts.fetch ?? globalThis.fetch.bind(globalThis);
  const res = await doFetch(`${opts.url.replace(/\/+$/, "")}/data/v1/types?${query}`, {
    method: "GET",
    headers: { authorization: `Bearer ${key}`, accept: "application/json" },
    redirect: NO_REDIRECTS,
  });
  const text = await res.text();
  if (!res.ok) throw toError(res, text);
  let body: unknown;
  try {
    body = JSON.parse(text);
  } catch {
    throw plainError(res, text, "internal");
  }
  if (isTypeMap(body)) return all ? { main: body } : body;
  if (all && isRecord(body) && Object.values(body).every(isTypeMap)) return body as Record<string, TypeMap>;
  throw plainError(res, text, "internal");
}

function isRecord(v: unknown): v is Record<string, unknown> {
  return typeof v === "object" && v !== null && !Array.isArray(v);
}

/** A `TypeMap` as the server writes it: `defaultSchema` is always there. */
function isTypeMap(v: unknown): v is TypeMap {
  return isRecord(v) && typeof v["defaultSchema"] === "string" && isRecord(v["schemas"]);
}
