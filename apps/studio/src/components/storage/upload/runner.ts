import { ApiError } from "@chelabase/management-client";
import { MAX_KEEP_BOTH, keepBothName } from "./conflict";
import { isRetryable } from "./errors";
import type { ConflictChoice } from "./queue-reducer";

/**
 * One file's upload, with `client.storage`'s semantics (packages/client/src/storage.ts, m10)
 * over Studio's console routes: up to the threshold one request, above it a resumable multipart
 * session whose parts the server lists, so a resumed upload sends only the missing ones. Busy,
 * stalled and network failures are retried with backoff; refusals aren't.
 */

export const MiB = 1024 * 1024;
/** Above this a file goes up in parts (as the data client's default). */
export const MULTIPART_THRESHOLD = 8 * MiB;
/** A part: storage takes 5 MiB to 100 MiB, the last smaller. */
export const PART_SIZE = 8 * MiB;
const MAX_PARTS = 10_000;

/** The console routes, as the runner needs them; `console-api.ts` is the real one. */
export interface UploadApi {
  /** One request; `upsert` replaces an existing object. */
  put(bucket: string, name: string, body: Blob, o: { upsert: boolean; contentType: string; signal: AbortSignal; onProgress: (loaded: number) => void }): Promise<void>;
  /** Opens a multipart session (a taken name is 409 `conflict` here already); resolves to its id. */
  create(bucket: string, name: string, o: { upsert: boolean; contentType: string; signal: AbortSignal }): Promise<string>;
  /** The parts the server has: part number to size. 404 or 410 when the session is gone. */
  listParts(session: string, signal: AbortSignal): Promise<Map<number, number>>;
  putPart(session: string, n: number, body: Blob, o: { signal: AbortSignal; onProgress: (loaded: number) => void }): Promise<void>;
  complete(session: string, signal: AbortSignal): Promise<void>;
  abort(session: string): Promise<void>;
}

export interface UploadJob {
  bucket: string;
  /** The name asked for. */
  key: string;
  /** The name to write: `key`, or a Keep both name already chosen (a resumed session's). */
  name: string;
  file: Blob;
  choice: ConflictChoice | null;
  /** A session to resume. */
  session: string | null;
}

export interface RunHooks {
  signal: AbortSignal;
  /** Bytes the server has of this file, as it goes. */
  onProgress(loaded: number): void;
  /** The session opened (to resume later), or null once it's finished or gone. */
  onSession(session: string | null): void;
  /** The name Keep both settled on. */
  onName?(name: string): void;
  /** Waits before a retry (tests skip it). */
  wait?(ms: number, signal: AbortSignal): Promise<void>;
}

export interface RunOptions {
  threshold?: number;
  partSize?: number;
  /** Retries of one request after a retryable failure. */
  retries?: number;
  retryDelayMs?: number;
}

export interface Part {
  n: number;
  start: number;
  end: number;
}

/** A file's parts: `partSize` each (raised to stay within 10000 parts), the last smaller. */
export function planParts(size: number, partSize: number): Part[] {
  const each = Math.max(partSize, Math.ceil(size / MAX_PARTS));
  const count = Math.max(1, Math.ceil(size / each));
  return Array.from({ length: count }, (_, i) => ({ n: i + 1, start: i * each, end: Math.min(size, (i + 1) * each) }));
}

function sleep(ms: number, signal: AbortSignal): Promise<void> {
  return new Promise((resolve, reject) => {
    const done = () => {
      clearTimeout(t);
      reject(signal.reason);
    };
    const t = setTimeout(() => {
      signal.removeEventListener("abort", done);
      resolve();
    }, ms);
    signal.addEventListener("abort", done, { once: true });
  });
}

const abortError = () => new DOMException("The upload was stopped", "AbortError");
function checkSignal(signal: AbortSignal): void {
  if (signal.aborted) throw abortError();
}

const isCode = (e: unknown, code: string) => e instanceof ApiError && (e.code as string) === code;

export async function runUpload(api: UploadApi, job: UploadJob, hooks: RunHooks, o: RunOptions = {}): Promise<{ name: string }> {
  const { signal } = hooks;
  const retries = o.retries ?? 3;
  const delay = o.retryDelayMs ?? 500;
  const wait = hooks.wait ?? sleep;
  const withRetries = async <T>(fn: () => Promise<T>): Promise<T> => {
    for (let attempt = 0; ; attempt++) {
      checkSignal(signal);
      try {
        return await fn();
      } catch (e) {
        if (attempt >= retries || !isRetryable(e) || signal.aborted) throw e;
        await wait(delay * 2 ** attempt, signal);
      }
    }
  };

  const file = job.file;
  const contentType = file.type;
  const upsert = job.choice === "replace";
  const multipart = file.size > (o.threshold ?? MULTIPART_THRESHOLD);

  /** Tries `attempt` at the job's name, then (Keep both) at each suffixed name until one is free. */
  const atFreeName = async (attempt: (name: string) => Promise<void>): Promise<string> => {
    if (job.choice !== "keep-both" || job.session !== null) {
      await attempt(job.name);
      return job.name;
    }
    for (let n = 1; n <= MAX_KEEP_BOTH; n++) {
      const name = keepBothName(job.key, n);
      try {
        await attempt(name);
        hooks.onName?.(name);
        return name;
      } catch (e) {
        if (!isCode(e, "conflict")) throw e;
      }
    }
    throw new Error(`No free name after ${MAX_KEEP_BOTH} tries`);
  };

  if (!multipart) {
    const name = await atFreeName((name) =>
      withRetries(() => api.put(job.bucket, name, file, { upsert, contentType, signal, onProgress: hooks.onProgress })),
    );
    hooks.onProgress(file.size);
    return { name };
  }

  let session = job.session;
  let name = job.name;
  const have = new Map<number, number>();
  if (session !== null) {
    try {
      const s = session;
      for (const [n, size] of await withRetries(() => api.listParts(s, signal))) have.set(n, size);
    } catch (e) {
      // Finished, aborted or expired: start again.
      if (!(e instanceof ApiError && (e.status === 404 || e.status === 410))) throw e;
      session = null;
      hooks.onSession(null);
    }
  }
  if (session === null) {
    let opened = "";
    name = await atFreeName(async (candidate) => {
      opened = await withRetries(() => api.create(job.bucket, candidate, { upsert, contentType, signal }));
    });
    session = opened;
    hooks.onSession(session);
  }

  const parts = planParts(file.size, o.partSize ?? PART_SIZE);
  let loaded = 0;
  for (const p of parts) if (have.get(p.n) === p.end - p.start) loaded += p.end - p.start;
  hooks.onProgress(loaded);
  const s = session;
  for (const p of parts) {
    if (have.get(p.n) === p.end - p.start) continue;
    checkSignal(signal);
    const body = file.slice(p.start, p.end);
    const base = loaded;
    await withRetries(() => api.putPart(s, p.n, body, { signal, onProgress: (sent) => hooks.onProgress(base + Math.min(sent, p.end - p.start)) }));
    loaded += p.end - p.start;
    hooks.onProgress(loaded);
  }
  checkSignal(signal);
  try {
    await withRetries(() => api.complete(s, signal));
  } catch (e) {
    // A refusal ends the session (storage aborts it); a network failure or 5xx can be resumed.
    if (e instanceof ApiError && e.status >= 400 && e.status < 500 && !isRetryable(e)) hooks.onSession(null);
    throw e;
  }
  hooks.onSession(null);
  return { name };
}
