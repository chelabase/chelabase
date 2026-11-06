// Storage: `client.storage`, over the storage HTTP API (`openapi/storage.json`). Loaded on
// first use by `storage-lazy.ts`, so a query-only app's bundle never carries it.
import { SESSION_GONE, USER_HEADER } from "./auth.ts";
import { NO_REDIRECTS } from "./error.ts";
import { StorageError, toStorageError } from "./storage-errors.ts";
import { downloadParam, encodeName, queryOf, type StorageContext } from "./storage-lazy.ts";

// --- public types ----------------------------------------------------------------------

/** Client options for storage. */
export interface StorageOptions {
  /**
   * The branch's id (a UUID): public URLs name it. Only `getPublicUrl` needs it; it sends no
   * request, so it can't ask the gateway which branch the key belongs to.
   */
  branch?: string;
  /**
   * Where a multipart upload's session is remembered so the same upload again resumes it:
   * `"memory"` (default, per client) or `"localStorage"` (browsers; survives a reload, and
   * falls back to memory where storage is blocked).
   */
  resume?: "memory" | "localStorage";
}

/**
 * What `upload` takes: a `Blob` or `File`, bytes, a string (UTF-8), a web `ReadableStream`, or
 * any async iterable of bytes (on the server, a Node `Readable`). A stream is sent as it is
 * read (never buffered whole), in one request: it can't be resumed or retried.
 */
export type UploadBody =
  | Blob
  | ArrayBuffer
  | ArrayBufferView
  | string
  | ReadableStream<Uint8Array>
  | AsyncIterable<Uint8Array | string>;

export interface UploadProgress {
  /** Bytes the server has (a multipart upload) or that have been sent (a stream). */
  loaded: number;
  /** The whole size, when known (a stream's only from `size`). */
  total: number | undefined;
}

export interface UploadOptions {
  /** Default: a `Blob`'s own type, else the server's `application/octet-stream`. */
  contentType?: string;
  /**
   * Replace an existing object of that name. Without it, a taken name is 409 `conflict`. A
   * write-once bucket refuses any overwrite (409 `write_once`).
   */
  upsert?: boolean;
  /** Called as the upload goes, `loaded` never decreasing, ending at `total`. */
  onProgress?: (p: UploadProgress) => void;
  /** Stops the upload (rejecting with the signal's reason); a multipart one can be resumed. */
  signal?: AbortSignal;
  /**
   * Above this size (bytes; default 8 MiB) a `Blob` goes up in parts, resumable: after a
   * failure, the same upload again (same bucket, name, data and options) on the same client
   * sends only the parts the server is missing.
   */
  multipartThreshold?: number;
  /** A part's size: 5 MiB to 100 MiB (default 8 MiB); raised when the file needs over 10000 parts. */
  partSize?: number;
  /** Retries of a part (or a small `Blob` upload) after a network error, 408, 429 or 5xx. Default 3. */
  retries?: number;
  /** The first retry's delay in ms (default 500), doubling each time. */
  retryDelayMs?: number;
  /** A stream's size in bytes, for progress only. */
  size?: number;
}

/** A stored object: its version, size and entity tag. */
export interface UploadResult {
  bucket: string;
  name: string;
  version_id: string;
  size: number;
  etag: string;
}

/** One entry of a listing: a file, or with `folders`, a folder (`name` ending at the delimiter). */
export interface FileObject {
  name: string;
  folder: boolean;
  size: number | null;
  etag: string | null;
  content_type: string | null;
  /** Seconds since the epoch. */
  updated_at: number | null;
}

/** A page of a listing: the objects, and the next page's cursor (`null` on the last page). */
export type FileList = FileObject[] & { nextCursor: string | null };

export interface ListOptions {
  /** Only names starting with it. */
  prefix?: string;
  /** Group names into folders at the next `/` after the prefix. */
  folders?: boolean;
  /** The previous page's `nextCursor`. */
  cursor?: string;
  /** 1 to 1000 (default 100). */
  limit?: number;
  /** Only names containing it, case-insensitively. */
  search?: string;
  sortBy?: "name" | "size" | "updated_at";
  order?: "asc" | "desc";
}

export interface DownloadOptions {
  /** One byte range, `end` inclusive (to the end when left out): answered 206. */
  range?: { start: number; end?: number };
  signal?: AbortSignal;
}

export interface ObjectInfo {
  size: number;
  contentType: string | null;
  etag: string | null;
}

export interface RelocateOptions {
  /** The destination bucket (default: the same). */
  toBucket?: string;
  /** Replace an object at the destination. */
  upsert?: boolean;
}

export interface SignedUrlOptions {
  /** Seconds, 1 to 604800 (7 days); default 3600. */
  expiresIn?: number;
  /** Serve as an attachment: `true` under the object's own name, a string under that name. */
  download?: string | boolean;
}

export interface SignedUrl {
  path: string;
  /** Absolute, on the gateway; needs no key. */
  signedUrl: string;
  /** Seconds since the epoch. */
  expiresAt: number;
}

export interface SignedUploadUrlOptions {
  /** Seconds, 1 to 604800 (7 days); default 3600. An app user's URL never outlives the session. */
  expiresIn?: number;
  /**
   * Whether the upload may replace an existing object. Bound when the URL is signed: the
   * uploader can't change it (`x-upsert` and `?upsert=` are ignored on the upload).
   */
  upsert?: boolean;
}

export interface SignedUploadUrl extends SignedUrl {
  /** The one-time token the URL carries. */
  token: string;
}

export interface PublicUrlOptions {
  /** Serve as an attachment: `true` under the object's own name, a string under that name. */
  download?: string | boolean;
}

/** A bucket as the server describes it. */
export interface Bucket {
  id: string;
  name: string;
  public: boolean;
  write_once: boolean;
  allowed_mime_types?: string[] | null;
  file_size_limit?: number | null;
}

export interface CreateBucket {
  name: string;
  /** Default false. A public bucket's objects are served without a key (`getPublicUrl`). */
  public?: boolean | null;
  /**
   * Default false. Objects are uploaded once and never overwritten, moved, renamed or deleted,
   * by anyone (409 `write_once`). Once true, never false again.
   */
  write_once?: boolean | null;
  allowed_mime_types?: string[] | null;
  /** Bytes, above 0. */
  file_size_limit?: number | null;
}

/** A bucket's changes; a field left out stays as it is. */
export interface UpdateBucket {
  name?: string | null;
  public?: boolean | null;
  /** Turned on at any time, never off again. */
  write_once?: boolean | null;
  allowed_mime_types?: string[] | null;
  file_size_limit?: number | null;
}

/** One bucket's objects. Every call runs as the caller: row level security decides. */
export interface StorageBucket {
  /**
   * Stores `body` under `path`. A `Blob` over `multipartThreshold` goes up in parts and resumes
   * after a failure; a stream goes up as it is read. A malformed `contentType` is 400; a body
   * that sends nothing for `CHELABASE_STORAGE_BODY_IDLE_SECS` fails with 408 `timeout`.
   */
  upload(path: string, body: UploadBody, opts?: UploadOptions): Promise<UploadResult>;
  /** The object as a `Response` (stream it from `body`, or read `blob()`); a `range` answers 206. */
  download(path: string, opts?: DownloadOptions): Promise<Response>;
  /** A page of objects (with `folders`, folders too); pass `nextCursor` back as `cursor` for the next. */
  list(opts?: ListOptions): Promise<FileList>;
  /** Deletes the objects (at most 1000) and resolves to the names deleted; missing or hidden ones are left out. A write-once bucket refuses (409). */
  remove(paths: string | string[]): Promise<string[]>;
  /** Moves or renames an object (metadata only). A write-once bucket refuses (409). */
  move(from: string, to: string, opts?: RelocateOptions): Promise<void>;
  /** Copies an object to a new version at `to`. */
  copy(from: string, to: string, opts?: RelocateOptions): Promise<{ bucket: string; name: string; version_id: string }>;
  /** Whether the object exists and the caller may see it. */
  exists(path: string): Promise<boolean>;
  /** The object's size, type and entity tag (`not_found` when missing or hidden). */
  info(path: string): Promise<ObjectInfo>;
  /**
   * A URL that serves this version of the object without a key. It serves only the version
   * signed: an overwrite or a delete ends it (404), and re-creating the name never revives it.
   * A branch reset or restore that makes that version current again serves it again; only the
   * URL expiring or revoking the key that signed it ends it for good. Sign again for a new version.
   */
  createSignedUrl(path: string, opts?: SignedUrlOptions): Promise<SignedUrl>;
  /** `createSignedUrl` for each path, in order. */
  createSignedUrls(paths: string[], opts?: SignedUrlOptions): Promise<SignedUrl[]>;
  /**
   * A one-time URL that takes one `PUT` of the object without a key, as the caller. Whether it
   * may replace an existing object is fixed now (`upsert`). A malformed `Content-Type` on the
   * `PUT` is 400 and doesn't spend the URL; 413 and 415 do.
   */
  createSignedUploadUrl(path: string, opts?: SignedUploadUrlOptions): Promise<SignedUploadUrl>;
  /**
   * The object's URL in a public bucket: stable, and built without a request (it needs the
   * client's `storage.branch`). A private bucket's URL answers 404.
   */
  getPublicUrl(path: string, opts?: PublicUrlOptions): string;
}

/** `client.storage`: buckets and their objects. */
export interface StorageClient {
  from(bucket: string): StorageBucket;
  /** The buckets the caller's policies show. */
  listBuckets(): Promise<Bucket[]>;
  /**
   * Bucket changes run as the key's role: a policy on `storage.buckets` decides, so in practice
   * they need the secret (`chk_sec_…`) key, on a server.
   */
  createBucket(bucket: CreateBucket): Promise<Bucket>;
  /** See `createBucket`. A write-once bucket holding objects is never renamed. */
  updateBucket(name: string, changes: UpdateBucket): Promise<void>;
  /** See `createBucket`. Only an empty bucket (409 `conflict` otherwise). */
  deleteBucket(name: string): Promise<void>;
}

// --- transport -------------------------------------------------------------------------

const MiB = 1024 * 1024;
const MIN_PART = 5 * MiB;
const MAX_PART = 100 * MiB;
const MAX_PARTS = 10_000;
const DEFAULT_PART = 8 * MiB;
const DEFAULT_THRESHOLD = 8 * MiB;
const RETRY_STATUSES = new Set([408, 429, 502, 503, 504]);

interface Req {
  method: string;
  path: string;
  query?: Record<string, string | undefined>;
  headers?: Record<string, string>;
  json?: unknown;
  body?: BodyInit;
  /** A body that can't be sent twice (a stream): no retry after a token refresh. */
  once?: boolean;
  signal?: AbortSignal | undefined;
  /** Statuses returned instead of thrown. */
  accept?: number[];
}

async function call(ctx: StorageContext, r: Req): Promise<Response> {
  r.signal?.throwIfAborted();
  const url = `${ctx.base}${r.path}${queryOf({ database: ctx.database, ...r.query })}`;
  const headers: Record<string, string> = { authorization: `Bearer ${ctx.key}`, ...r.headers };
  let body = r.body;
  if (r.json !== undefined) {
    body = JSON.stringify(r.json);
    headers["content-type"] = "application/json";
  }
  const send = (token: string | undefined) =>
    ctx.fetch(url, {
      method: r.method,
      headers: token === undefined ? headers : { ...headers, [USER_HEADER]: token },
      body,
      redirect: NO_REDIRECTS,
      signal: r.signal,
      // A stream body needs half duplex (Node, Chrome).
      ...(body instanceof ReadableStream ? { duplex: "half" } : {}),
    } as RequestInit);
  let token = await ctx.core.accessToken();
  let res = await send(token);
  if (res.ok || r.accept?.includes(res.status)) return res;
  let err = toStorageError(res, r.method === "HEAD" ? "" : await res.text());
  if (ctx.retryExpired && !r.once && token !== undefined && res.status === 401 && err.serverCode === "user_token_expired") {
    // One refresh, one retry.
    token = await ctx.core.refreshAfterExpiry(token);
    res = await send(token);
    if (res.ok || r.accept?.includes(res.status)) return res;
    err = toStorageError(res, r.method === "HEAD" ? "" : await res.text());
  }
  if (token !== undefined && err.serverCode === SESSION_GONE) await ctx.core.dropSession(token);
  throw err;
}

async function readJson<T>(res: Response): Promise<T> {
  const text = await res.text();
  try {
    return JSON.parse(text) as T;
  } catch {
    throw new StorageError({ status: res.status, code: "internal", message: `HTTP ${res.status}: the answer isn't JSON` });
  }
}

const objectPath = (bucket: string, name: string) => `/storage/v1/object/${encodeURIComponent(bucket)}/${encodeName(name)}`;
const flag = (b: boolean | undefined) => (b ? "true" : undefined);

function retryable(e: unknown): boolean {
  if (e instanceof StorageError) return RETRY_STATUSES.has(e.status);
  // `fetch` rejects with a TypeError when the network fails; an abort is a DOMException.
  return e instanceof TypeError;
}

function wait(ms: number, signal: AbortSignal | undefined): Promise<void> {
  return new Promise((resolve, reject) => {
    if (ms <= 0) return resolve();
    const done = () => {
      clearTimeout(t);
      reject(signal!.reason);
    };
    const t = setTimeout(() => {
      signal?.removeEventListener("abort", done);
      resolve();
    }, ms);
    signal?.addEventListener("abort", done, { once: true });
  });
}

async function withRetries<T>(fn: () => Promise<T>, o: UploadOptions): Promise<T> {
  const retries = o.retries ?? 3;
  const delay = o.retryDelayMs ?? 500;
  for (let attempt = 0; ; attempt++) {
    try {
      return await fn();
    } catch (e) {
      if (attempt >= retries || !retryable(e) || o.signal?.aborted) throw e;
      await wait(delay * 2 ** attempt, o.signal);
    }
  }
}

// --- uploads ---------------------------------------------------------------------------

function asBlob(body: UploadBody): Blob | undefined {
  if (body instanceof Blob) return body;
  if (typeof body === "string") return new Blob([body], { type: "text/plain;charset=utf-8" });
  if (body instanceof ArrayBuffer) return new Blob([body]);
  if (ArrayBuffer.isView(body)) return new Blob([body as ArrayBufferView<ArrayBuffer>]);
  return undefined;
}

/** A pull-only stream over `src`: a chunk is read from it only when the request asks for one. */
function asStream(src: ReadableStream<Uint8Array> | AsyncIterable<Uint8Array | string>, counted: (n: number) => void): ReadableStream<Uint8Array> {
  let next: () => Promise<IteratorResult<Uint8Array | string>>;
  let cancel: (reason: unknown) => Promise<unknown>;
  if (src instanceof ReadableStream) {
    const reader = src.getReader();
    next = () => reader.read() as Promise<IteratorResult<Uint8Array>>;
    cancel = (reason) => reader.cancel(reason);
  } else {
    const it = src[Symbol.asyncIterator]();
    next = () => it.next();
    cancel = async (reason) => it.return?.(reason);
  }
  const enc = new TextEncoder();
  return new ReadableStream<Uint8Array>(
    {
      async pull(c) {
        const r = await next();
        if (r.done) return c.close();
        const v = r.value;
        const bytes = typeof v === "string" ? enc.encode(v) : new Uint8Array(v.buffer, v.byteOffset, v.byteLength);
        counted(bytes.byteLength);
        c.enqueue(bytes);
      },
      cancel: (reason) => cancel(reason).then(() => undefined),
    },
    { highWaterMark: 0 },
  );
}

async function upload(ctx: StorageContext, bucket: string, name: string, body: UploadBody, o: UploadOptions = {}): Promise<UploadResult> {
  o.signal?.throwIfAborted();
  if (o.partSize !== undefined && !(o.partSize >= MIN_PART && o.partSize <= MAX_PART)) {
    throw new RangeError(`storage upload: partSize must be 5 MiB to 100 MiB (got ${o.partSize})`);
  }
  const blob = asBlob(body);
  const contentType = o.contentType ?? (blob?.type ? blob.type : undefined);
  const headers: Record<string, string> = contentType === undefined ? {} : { "content-type": contentType };
  if (blob === undefined) {
    let loaded = 0;
    const stream = asStream(body as ReadableStream<Uint8Array>, (n) => {
      loaded += n;
      o.onProgress?.({ loaded, total: o.size });
    });
    const res = await call(ctx, {
      method: "POST",
      path: objectPath(bucket, name),
      query: { upsert: flag(o.upsert) },
      headers,
      body: stream,
      once: true,
      signal: o.signal,
    });
    return readJson<UploadResult>(res);
  }
  if (blob.size > (o.multipartThreshold ?? DEFAULT_THRESHOLD)) return multipart(ctx, bucket, name, blob, headers, o);
  const res = await withRetries(
    () => call(ctx, { method: "POST", path: objectPath(bucket, name), query: { upsert: flag(o.upsert) }, headers, body: blob, signal: o.signal }),
    o,
  );
  const out = await readJson<UploadResult>(res);
  o.onProgress?.({ loaded: blob.size, total: blob.size });
  return out;
}

async function multipart(
  ctx: StorageContext,
  bucket: string,
  name: string,
  blob: Blob,
  headers: Record<string, string>,
  o: UploadOptions,
): Promise<UploadResult> {
  const size = blob.size;
  const partSize = Math.max(o.partSize ?? DEFAULT_PART, Math.ceil(size / MAX_PARTS));
  if (partSize > MAX_PART) throw new RangeError("storage upload: the file is over 10000 parts of 100 MiB");
  const count = Math.max(1, Math.ceil(size / partSize));
  const lastModified = (blob as { lastModified?: number }).lastModified ?? "";
  // The same upload again: same place, data, options and part boundaries.
  const key = [ctx.database ?? "", bucket, name, size, lastModified, o.upsert === true, headers["content-type"] ?? "", partSize].join("\n");
  const session = (id: string) => `/storage/v1/upload/session/${encodeURIComponent(id)}`;

  let id = ctx.resume.get(key);
  const have = new Map<number, number>();
  if (id !== undefined) {
    try {
      const s = await readJson<{ parts: { part_number: number; size: number }[] }>(
        await call(ctx, { method: "GET", path: session(id), signal: o.signal }),
      );
      for (const p of s.parts) have.set(p.part_number, p.size);
    } catch (e) {
      // Finished, aborted, expired or another caller's: start again.
      if (!(e instanceof StorageError && (e.status === 404 || e.status === 410))) throw e;
      ctx.resume.delete(key);
      id = undefined;
    }
  }
  if (id === undefined) {
    const s = await readJson<{ id: string }>(
      await call(ctx, {
        method: "POST",
        path: `/storage/v1/upload/${encodeURIComponent(bucket)}/${encodeName(name)}`,
        query: { upsert: flag(o.upsert) },
        headers,
        signal: o.signal,
      }),
    );
    id = s.id;
    ctx.resume.set(key, id);
  }

  let loaded = 0;
  let reported = -1;
  const report = () => {
    if (loaded === reported) return;
    reported = loaded;
    o.onProgress?.({ loaded, total: size });
  };
  for (let n = 1; n <= count; n++) {
    const start = (n - 1) * partSize;
    const end = Math.min(size, start + partSize);
    if (have.get(n) === end - start) {
      loaded += end - start;
      continue;
    }
    o.signal?.throwIfAborted();
    const part = blob.slice(start, end);
    await withRetries(
      () =>
        call(ctx, {
          method: "PUT",
          path: `${session(id)}/${n}`,
          headers: { "content-type": "application/octet-stream" },
          body: part,
          signal: o.signal,
        }),
      o,
    );
    loaded += end - start;
    report();
  }
  o.signal?.throwIfAborted();
  try {
    const out = await readJson<UploadResult>(await call(ctx, { method: "POST", path: `${session(id)}/complete`, signal: o.signal }));
    ctx.resume.delete(key);
    report();
    return out;
  } catch (e) {
    // A refusal ends the session (the server aborts it); a network failure or 5xx can be resumed.
    if (e instanceof StorageError && e.status >= 400 && e.status < 500 && !RETRY_STATUSES.has(e.status)) ctx.resume.delete(key);
    throw e;
  }
}

// --- the API ---------------------------------------------------------------------------

function unquote(etag: string | null): string | null {
  return etag === null ? null : etag.replace(/^W\//, "").replace(/^"(.*)"$/, "$1");
}

export function bucketApi(ctx: StorageContext, bucket: string): Omit<StorageBucket, "getPublicUrl"> {
  const absolute = (signed: string, download?: string | boolean) => {
    const d = downloadParam(download);
    return `${ctx.base}${signed}${d === undefined ? "" : `${signed.includes("?") ? "&" : "?"}download=${encodeURIComponent(d)}`}`;
  };
  const sign = async (path: string, opts: SignedUrlOptions = {}): Promise<SignedUrl> => {
    const res = await call(ctx, {
      method: "POST",
      path: `/storage/v1/object/sign/${encodeURIComponent(bucket)}/${encodeName(path)}`,
      json: opts.expiresIn === undefined ? {} : { expires_in: opts.expiresIn },
    });
    const s = await readJson<{ signed_url: string; expires_at: number }>(res);
    return { path, signedUrl: absolute(s.signed_url, opts.download), expiresAt: s.expires_at };
  };
  const relocate = (op: "move" | "copy", from: string, to: string, opts: RelocateOptions = {}) =>
    call(ctx, {
      method: "POST",
      path: `/storage/v1/object/${op}`,
      query: { upsert: flag(opts.upsert) },
      json: { bucket, from, to, ...(opts.toBucket === undefined ? {} : { to_bucket: opts.toBucket }) },
    });
  const head = (path: string, accept?: number[]) => call(ctx, { method: "HEAD", path: objectPath(bucket, path), accept });

  return {
    upload: (path, body, opts) => upload(ctx, bucket, path, body, opts),
    download: (path, opts = {}) =>
      call(ctx, {
        method: "GET",
        path: objectPath(bucket, path),
        headers: opts.range ? { range: `bytes=${opts.range.start}-${opts.range.end ?? ""}` } : {},
        signal: opts.signal,
      }),
    async list(opts = {}) {
      const body: Record<string, unknown> = { prefix: opts.prefix ?? "" };
      if (opts.folders) body["delimiter"] = "/";
      if (opts.cursor !== undefined) body["cursor"] = opts.cursor;
      if (opts.limit !== undefined) body["limit"] = opts.limit;
      if (opts.search !== undefined) body["search"] = opts.search;
      if (opts.sortBy !== undefined) body["sort_by"] = opts.sortBy;
      if (opts.order !== undefined) body["order"] = opts.order;
      const res = await call(ctx, { method: "POST", path: `/storage/v1/object/list/${encodeURIComponent(bucket)}`, json: body });
      const page = await readJson<{ items: FileObject[]; next_cursor: string | null }>(res);
      return Object.assign(page.items, { nextCursor: page.next_cursor ?? null });
    },
    async remove(paths) {
      const names = typeof paths === "string" ? [paths] : paths;
      const res = await call(ctx, { method: "DELETE", path: `/storage/v1/object/${encodeURIComponent(bucket)}`, json: { names } });
      return (await readJson<{ deleted: string[] }>(res)).deleted;
    },
    async move(from, to, opts) {
      await relocate("move", from, to, opts);
    },
    async copy(from, to, opts) {
      return readJson(await relocate("copy", from, to, opts));
    },
    async exists(path) {
      return (await head(path, [404])).status !== 404;
    },
    async info(path) {
      const res = await head(path);
      return {
        size: Number(res.headers.get("content-length") ?? 0),
        contentType: res.headers.get("content-type"),
        etag: unquote(res.headers.get("etag")),
      };
    },
    createSignedUrl: sign,
    createSignedUrls: (paths, opts) => Promise.all(paths.map((p) => sign(p, opts))),
    async createSignedUploadUrl(path, opts = {}) {
      const json: Record<string, unknown> = {};
      if (opts.expiresIn !== undefined) json["expires_in"] = opts.expiresIn;
      if (opts.upsert !== undefined) json["upsert"] = opts.upsert;
      const res = await call(ctx, {
        method: "POST",
        path: `/storage/v1/object/upload/sign/${encodeURIComponent(bucket)}/${encodeName(path)}`,
        json,
      });
      const s = await readJson<{ signed_url: string; expires_at: number }>(res);
      const signedUrl = absolute(s.signed_url);
      return { path, signedUrl, token: new URL(signedUrl).searchParams.get("token") ?? "", expiresAt: s.expires_at };
    },
  };
}

export function adminApi(ctx: StorageContext): Omit<StorageClient, "from"> {
  const at = (name: string) => `/storage/v1/bucket/${encodeURIComponent(name)}`;
  return {
    listBuckets: async () => readJson(await call(ctx, { method: "GET", path: "/storage/v1/bucket" })),
    createBucket: async (bucket) => readJson(await call(ctx, { method: "POST", path: "/storage/v1/bucket", json: bucket })),
    async updateBucket(name, changes) {
      await call(ctx, { method: "PATCH", path: at(name), json: changes });
    },
    async deleteBucket(name) {
      await call(ctx, { method: "DELETE", path: at(name) });
    },
  };
}
