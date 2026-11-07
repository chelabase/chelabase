/**
 * `@chelabase/client/browser`: the client for browser code in a server-rendered app. The
 * session's refresh token stays in an `HttpOnly` cookie; sign-in, sign-up, refresh, change
 * password and sign-out go through the app's `createAuthHandler` route. Never imports server code.
 *
 * @module
 */
import { createAuth } from "./auth.ts";
import { buildClient } from "./client.ts";
import { type CookieOptions, checkCookieOptions, cookieNames, parseCookies, readSession, serializeCookie, sessionCookies } from "./cookies.ts";
import { toError } from "./error.ts";
import type { StorageOptions } from "./storage.ts";
import type { BrowserSession, Client, DatabaseShape, TypeMap } from "./types.ts";

export type { BrowserSession } from "./types.ts";
// `StorageError` itself comes from `@chelabase/client`.
export type { StorageErrorCode } from "./storage-errors.ts";
export type {
  Bucket,
  CreateBucket,
  DownloadOptions,
  FileList,
  FileObject,
  ListOptions,
  ObjectInfo,
  PublicUrlOptions,
  RelocateOptions,
  SignedUploadUrl,
  SignedUploadUrlOptions,
  SignedUrl,
  SignedUrlOptions,
  StorageBucket,
  StorageClient,
  StorageOptions,
  UpdateBucket,
  UploadBody,
  UploadOptions,
  UploadProgress,
  UploadResult,
} from "./storage.ts";

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
  /**
   * Storage options: the branch id for `getPublicUrl`, and `resume: "localStorage"` to resume a
   * multipart upload after a reload.
   */
  storage?: StorageOptions;
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
  // The whole shape, as on the server; these messages never quote a key.
  if (!/^chk_pub_[0-9A-Za-z]+$/.test(opts.key)) {
    throw new Error(
      opts.key.startsWith("chk_sec_")
        ? "createBrowserClient: `key` must be the public (chk_pub_…) key; a secret key never belongs in the browser"
        : "createBrowserClient: `key` must be the public (chk_pub_…) key",
    );
  }
  checkCookieOptions(opts.cookieOptions);
  const names = cookieNames(opts.key, opts.cookieOptions);
  const path = (opts.authPath ?? "/api/chelabase/auth").replace(/\/+$/, "");
  const readAccess = () => {
    const doc = (globalThis as { document?: { cookie?: string } }).document;
    return readSession(parseCookies(doc?.cookie ?? ""), names).access;
  };
  const clearAccess = () => {
    const doc = (globalThis as { document?: { cookie?: string } }).document;
    // Same name and attributes as the route wrote it, with `Max-Age=0`.
    if (doc) doc.cookie = serializeCookie(sessionCookies(names, null, opts.cookieOptions)[0]!);
  };
  try {
    // `createClient` kept the refresh token here; this client never does.
    (globalThis as { localStorage?: Storage }).localStorage?.removeItem(`chelabase.${opts.key.slice(0, 12)}.refresh`);
  } catch {
    // storage blocked: nothing to remove
  }
  return buildClient<DB, B>("createBrowserClient", opts, (base, doFetch) => ({
    core: createAuth<DB>({ base, key: opts.key, fetch: doFetch, options: undefined, toError, remote: { path, readAccess, clearAccess } }),
    retryExpired: true,
  }));
}
