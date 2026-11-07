/**
 * `@chelabase/client`: a typed client for the Chelabase data API. No runtime dependencies;
 * built on `fetch`, so it runs in browsers, Node and edge runtimes. See the README.
 *
 * @module
 */
import { createAuth, staticAuth } from "./auth.ts";
import { buildClient } from "./client.ts";
import { toError } from "./error.ts";
import type { StorageOptions } from "./storage.ts";
import type { AuthOptions, Client, DatabaseShape, TypeMap } from "./types.ts";

export { DataApiError, UnsafeIntegerError } from "./error.ts";
export { fetchTypes, type FetchTypesOptions } from "./fetch-types.ts";
export { StorageError, type StorageErrorCode } from "./storage-errors.ts";
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
  ResolveOptions,
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
  StorageRef,
  TableName,
  TableQuery,
  Tx,
  TypeMap,
  Where,
  WhereItem,
  UnfilteredWrite,
  WriteQuery,
} from "./types.ts";

export type ClientOptions = {
  /** The gateway's base URL, e.g. `http://api.chelabase.localhost:8000`. */
  url: string;
  /** A public (`chk_pub_…`) or secret (`chk_sec_…`) data key, sent as a bearer token. */
  key: string;
  /** Defaults to the global `fetch`. */
  fetch?: typeof fetch;
  /** The generated `types` map: which columns, arguments and returns are int8 or numeric. */
  types?: TypeMap;
  /** Storage options: the branch id for `getPublicUrl`, and where resumable uploads are remembered. */
  storage?: StorageOptions;
} & (
  | {
      /** App user sessions (`db.auth`): where the refresh token is kept, and automatic refresh. */
      auth?: AuthOptions;
      accessToken?: undefined;
    }
  | {
      /**
       * The user's access token, read before every request and sent as `chelabase-user` (no
       * header when it returns `undefined`). The caller owns the session: the client never
       * refreshes, stores or times anything, and `db.auth` methods throw. Exclusive with `auth`.
       */
      accessToken: () => string | undefined | Promise<string | undefined>;
      auth?: never;
    }
);

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
  return buildClient<DB, B>("createClient", opts, (base, doFetch) =>
    // One session per client, shared by every `database()` sibling.
    opts.accessToken !== undefined
      ? { core: staticAuth<DB>(opts.accessToken), retryExpired: false } // never retried with a caller-owned token
      : { core: createAuth<DB>({ base, key: opts.key, fetch: doFetch, options: opts.auth, toError }), retryExpired: true },
  );
}
