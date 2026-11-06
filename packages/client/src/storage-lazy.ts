// `client.storage` as every client carries it: a thin front that loads `storage.ts` on first
// use (a dynamic import, so a bundler splits it out of a query-only app). Only
// `getPublicUrl` (no network) lives here. Nothing here may import `storage.ts` statically.
import type { AuthCore } from "./auth.ts";
import type { PublicUrlOptions, StorageBucket, StorageClient, StorageOptions } from "./storage.ts";
import type { DatabaseShape } from "./types.ts";

/** Where a resumable upload's session id is kept between attempts. */
export interface ResumeStore {
  get(key: string): string | undefined;
  set(key: string, id: string): void;
  delete(key: string): void;
}

/** What storage needs from the client it belongs to (internal). */
export interface StorageContext {
  base: string;
  key: string;
  fetch: typeof fetch;
  database: string | undefined;
  branch: string | undefined;
  core: Pick<AuthCore<DatabaseShape>, "accessToken" | "refreshAfterExpiry" | "dropSession">;
  retryExpired: boolean;
  resume: ResumeStore;
}

/** A bucket or object name as a URL path: each `/`-separated segment percent-encoded. */
export function encodeName(name: string): string {
  return name.split("/").map(encodeURIComponent).join("/");
}

/** `?database=…&download=…` (only those given), or "". */
export function queryOf(params: Record<string, string | undefined>): string {
  const q = new URLSearchParams();
  for (const [k, v] of Object.entries(params)) if (v !== undefined) q.set(k, v);
  const s = q.toString();
  return s ? `?${s}` : "";
}

/** `download: true` is the object's own name (`""`), a string names the file. */
export function downloadParam(download: string | boolean | undefined): string | undefined {
  return download === undefined || download === false ? undefined : download === true ? "" : download;
}

/** In memory, per client: lost when the page or process goes. */
function memoryStore(): ResumeStore {
  const m = new Map<string, string>();
  return { get: (k) => m.get(k), set: (k, v) => void m.set(k, v), delete: (k) => void m.delete(k) };
}

/** `localStorage` when it is there and usable; in memory otherwise. */
function localStore(prefix: string): ResumeStore {
  const ls = (() => {
    try {
      return (globalThis as { localStorage?: Storage }).localStorage;
    } catch {
      return undefined;
    }
  })();
  if (ls === undefined) return memoryStore();
  const at = (k: string) => `${prefix}${k}`;
  return {
    get: (k) => {
      try {
        return ls.getItem(at(k)) ?? undefined;
      } catch {
        return undefined;
      }
    },
    set: (k, v) => {
      try {
        ls.setItem(at(k), v);
      } catch {
        // storage full or blocked: this attempt just isn't resumable after a reload
      }
    },
    delete: (k) => {
      try {
        ls.removeItem(at(k));
      } catch {
        // blocked: nothing to remove
      }
    },
  };
}

export function resumeStore(key: string, opts: StorageOptions | undefined): ResumeStore {
  return opts?.resume === "localStorage" ? localStore(`chelabase.${key.slice(0, 12)}.upload.`) : memoryStore();
}

let loading: Promise<typeof import("./storage.ts")> | undefined;
const load = () => (loading ??= import("./storage.ts"));

const BUCKET_METHODS = [
  "upload",
  "download",
  "list",
  "remove",
  "move",
  "copy",
  "exists",
  "info",
  "createSignedUrl",
  "createSignedUrls",
  "createSignedUploadUrl",
] as const;
const ADMIN_METHODS = ["listBuckets", "createBucket", "updateBucket", "deleteBucket"] as const;

type AnyFn = (...args: unknown[]) => Promise<unknown>;

export function lazyStorage(ctx: StorageContext): StorageClient {
  const from = (bucket: string): StorageBucket => {
    const api: Record<string, unknown> = {
      getPublicUrl: (path: string, opts?: PublicUrlOptions) => {
        if (ctx.branch === undefined) {
          throw new Error("storage getPublicUrl needs the branch id: create the client with `storage: { branch }`");
        }
        return `${ctx.base}/storage/v1/object/public/${encodeURIComponent(ctx.branch)}/${encodeURIComponent(bucket)}/${encodeName(path)}${queryOf({ database: ctx.database, download: downloadParam(opts?.download) })}`;
      },
    };
    for (const m of BUCKET_METHODS) {
      api[m] = (...args: unknown[]) => load().then((s) => (s.bucketApi(ctx, bucket)[m] as AnyFn)(...args));
    }
    return api as unknown as StorageBucket;
  };
  const client: Record<string, unknown> = { from };
  for (const m of ADMIN_METHODS) {
    client[m] = (...args: unknown[]) => load().then((s) => (s.adminApi(ctx)[m] as AnyFn)(...args));
  }
  return client as unknown as StorageClient;
}
