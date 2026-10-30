/**
 * `@chelabase/client/browser`: the client for browser code in a server-rendered app. The
 * session's refresh token stays in an `HttpOnly` cookie; sign-in, sign-up, refresh, change
 * password and sign-out go through the app's `createAuthHandler` route. Never imports server code.
 *
 * @module
 */
import { createAuth } from "./auth.ts";
import { buildClient } from "./client.ts";
import { type CookieOptions, cookieNames, parseCookies, readSession } from "./cookies.ts";
import { toError } from "./error.ts";
import type { BrowserSession, Client, DatabaseShape, TypeMap } from "./types.ts";

export type { BrowserSession } from "./types.ts";

export interface BrowserClientOptions {
  /** The gateway's base URL. */
  url: string;
  /** The project's public (`chk_pub_…`) data key. */
  key: string;
  /** Defaults to the global `fetch`. */
  fetch?: typeof fetch;
  /** The generated `types` map: which columns, arguments and returns are int8 or numeric. */
  types?: TypeMap;
  /** Where the app mounted `createAuthHandler`. Default `/api/chelabase/auth`. */
  authPath?: string;
  /** The same `cookieOptions` the server side uses (`secure` and `domain` pick the cookie names). */
  cookieOptions?: CookieOptions;
}

/**
 * Creates the browser client. Data queries go straight to the gateway with the access token
 * from the readable cookie, read again before every use (so a sign-in, sign-out or refresh in
 * another tab is picked up); session flows go through `authPath`, so `db.auth.signIn` and
 * friends return a `BrowserSession` (no `refresh_token`). Other `db.auth` calls go to the gateway.
 *
 * Types and `bigint` work as in `createClient`.
 */
export function createBrowserClient<DB extends DatabaseShape = DatabaseShape>(
  opts: BrowserClientOptions & { bigint: true; types: TypeMap },
): Client<DB, true, BrowserSession>;
export function createBrowserClient<DB extends DatabaseShape = DatabaseShape, B extends boolean = false>(
  opts: BrowserClientOptions &
    (B extends true ? { bigint: true; types: TypeMap } : { bigint?: false }) &
    (DatabaseShape extends DB ? unknown : { types: TypeMap }),
): Client<DB, B, BrowserSession>;
export function createBrowserClient<DB extends DatabaseShape, B extends boolean = false>(
  opts: BrowserClientOptions & { bigint?: B },
): Client<DB, B, BrowserSession> {
  const names = cookieNames(opts.key, opts.cookieOptions);
  const path = (opts.authPath ?? "/api/chelabase/auth").replace(/\/+$/, "");
  const readAccess = () => {
    const doc = (globalThis as { document?: { cookie?: string } }).document;
    return readSession(parseCookies(doc?.cookie ?? ""), names).access;
  };
  try {
    // `createClient` kept the refresh token here; this client never does.
    (globalThis as { localStorage?: Storage }).localStorage?.removeItem(`chelabase.${opts.key.slice(0, 12)}.refresh`);
  } catch {
    // storage blocked: nothing to remove
  }
  return buildClient<DB, B>("createBrowserClient", opts, (base, doFetch) => ({
    core: createAuth<DB>({ base, key: opts.key, fetch: doFetch, options: undefined, toError, remote: { path, readAccess } }),
    retryExpired: true,
  }));
}
