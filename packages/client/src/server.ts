/**
 * `@chelabase/client/server`: the client for server rendering, Server Actions and route
 * handlers. Created per request; the app user's session lives in two cookies.
 *
 * @module
 */
import { createAuth } from "./auth.ts";
import { buildClient } from "./client.ts";
import {
  type CookieOptions,
  type CookieToSet,
  checkCookieOptions,
  cookieNames,
  readSession,
  sessionCookies,
} from "./cookies.ts";
import { toError } from "./error.ts";
import type { StorageOptions } from "./storage.ts";
import type { Client, DatabaseShape, TypeMap } from "./types.ts";

export type { CookieAttributes, CookieOptions, CookieToSet } from "./cookies.ts";
export { type AuthHandlerOptions, createAuthHandler } from "./handler.ts";
export { parseCookies, serializeCookie } from "./cookies.ts";
// Storage webhooks: verify a delivery's signature (object-storage m18).
export {
  type StorageEvent,
  type StorageEventType,
  type VerifyWebhookOptions,
  verifyWebhook,
  WebhookVerificationError,
  type WebhookVerificationReason,
} from "./webhooks.ts";
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

/**
 * The request's cookies. `setAll` must make the new cookies visible to later `getAll` calls
 * in the same request (in Next.js middleware: set them on `request.cookies` and pass
 * `NextResponse.next({ request })`, as well as on the response), so a render after a refresh
 * sees the new token. Without `setAll` (a Server Component) the session is never refreshed.
 */
export interface CookieAdapter {
  getAll(): { name: string; value: string }[];
  setAll?(list: CookieToSet[]): void | Promise<void>;
}

export interface ServerClientOptions {
  /** The gateway's base URL. */
  url: string;
  /** The project's public (`chk_pub_…`) data key. */
  key: string;
  /**
   * The branch's secret (`chk_sec_…`) key. Auth API calls (never data requests) carry it, so
   * sign-ins and refreshes are limited per email and per user instead of all sharing this
   * server's IP.
   */
  serverKey?: string;
  /** Optional: the end user's IP address, to add per-IP limits on top. Requires `serverKey`. */
  clientIp?: string;
  /** Defaults to the global `fetch`. */
  fetch?: typeof fetch;
  /** The generated `types` map: which columns, arguments and returns are int8 or numeric. */
  types?: TypeMap;
  cookieOptions?: CookieOptions;
  cookies: CookieAdapter;
  /** Storage options: the branch id for `getPublicUrl`, and where resumable uploads are remembered. */
  storage?: StorageOptions;
}

/**
 * Creates a client for one request. An access cookie with 5 s or more left is used as is;
 * otherwise, given a refresh cookie and `cookies.setAll`, the session is refreshed once and
 * both cookies are written before the new token is used. `db.auth` writes and clears the
 * cookies. Who the user is comes only from the gateway (`db.auth.currentUser()` or
 * `db.auth.getUser()`): the cookie's
 * own contents decide nothing but whether to refresh.
 *
 * Types and `bigint` work as in `createClient`.
 */
export function createServerClient<DB extends DatabaseShape = DatabaseShape>(
  opts: ServerClientOptions & { bigint: true; types: TypeMap },
): Client<DB, true>;
export function createServerClient<DB extends DatabaseShape = DatabaseShape, B extends boolean = false>(
  opts: ServerClientOptions &
    (B extends true ? { bigint: true; types: TypeMap } : { bigint?: false }) &
    (DatabaseShape extends DB ? unknown : { types: TypeMap }),
): Client<DB, B>;
export function createServerClient<DB extends DatabaseShape, B extends boolean = false>(
  opts: ServerClientOptions & { bigint?: B },
): Client<DB, B> {
  // The whole shape (prefix and base62), so a malformed key never reaches `fetch`, whose header
  // errors quote the value. These messages never quote a key.
  if (!/^chk_pub_[0-9A-Za-z]+$/.test(opts.key)) {
    throw new Error("createServerClient: `key` must be the public (chk_pub_…) key; a secret key goes in `serverKey`");
  }
  // An empty `serverKey` (a blank environment variable) counts as missing.
  const serverKey = opts.serverKey === "" ? undefined : opts.serverKey;
  if (serverKey !== undefined && !/^chk_sec_[0-9A-Za-z]+$/.test(serverKey)) {
    throw new Error("createServerClient: `serverKey` must be a secret (chk_sec_…) key");
  }
  // An empty `clientIp` (an absent header read as "") counts as missing, as in the handler.
  if (opts.clientIp !== undefined && opts.clientIp !== "" && serverKey === undefined) {
    throw new Error("createServerClient: `clientIp` needs `serverKey`");
  }
  checkCookieOptions(opts.cookieOptions);
  const names = cookieNames(opts.key, opts.cookieOptions);
  const { access, refresh } = readSession(opts.cookies.getAll(), names);
  const setAll = opts.cookies.setAll?.bind(opts.cookies);
  // Auth calls only. An empty `clientIp` (an absent header read as "") sends the server key alone.
  const headers: Record<string, string> = {};
  if (serverKey !== undefined) {
    headers["chelabase-server-key"] = serverKey;
    if (opts.clientIp !== undefined && opts.clientIp !== "") headers["chelabase-client-ip"] = opts.clientIp;
  }

  async function onSession(s: Parameters<typeof sessionCookies>[1]) {
    if (setAll === undefined) {
      // Nothing to clear without `setAll`. Unreachable for a session: the methods that issue one
      // are refused below before the server is asked. Kept as a backstop if that list drifts.
      if (s !== null) throw new Error("createServerClient: db.auth needs `cookies.setAll` to keep a session");
      return;
    }
    await setAll(sessionCookies(names, s, opts.cookieOptions));
  }

  return buildClient<DB, B>("createServerClient", opts, (base, doFetch) => {
    const core = createAuth<DB>({
      base,
      key: opts.key,
      fetch: doFetch,
      options: undefined,
      toError,
      // Without `setAll` a refresh could never be written back: the old token would be
      // revoked after the grace window, so the refresh token isn't even looked at.
      server: { access, refresh: setAll === undefined ? undefined : refresh, onSession, headers },
    });
    if (setAll === undefined) {
      // A session issued here would be lost (and a rotated refresh token revoked), and a sign-out
      // couldn't clear the cookies: refuse before asking the server.
      const refuse = (method: string) => () =>
        Promise.reject(new Error(`createServerClient: db.auth.${method} needs \`cookies.setAll\` to keep the session`));
      core.auth = {
        ...core.auth,
        signIn: refuse("signIn"),
        signUp: refuse("signUp"),
        changePassword: refuse("changePassword"),
        refresh: refuse("refresh"),
        signOut: refuse("signOut"),
      };
    }
    return { core, retryExpired: true };
  });
}
