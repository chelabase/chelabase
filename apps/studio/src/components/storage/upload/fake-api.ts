import { ApiError } from "@chelabase/management-client";
import type { RunHooks, UploadApi } from "./runner";

/**
 * An in-memory stand-in for the console upload routes, for the runner's tests: objects by
 * name, multipart sessions with their parts, and failures injected per request.
 */

interface Call {
  op: "put" | "create" | "listParts" | "putPart" | "complete" | "abort";
  name?: string;
  n?: number;
  upsert?: boolean;
}

interface Options {
  existing?: Record<string, string>;
  /** An error for a part (or null to let it through); called for every attempt. */
  failPart?: (n: number) => Error | null;
  failPut?: () => Error | null;
  failComplete?: () => Error | null;
}

const conflict = () => new ApiError(409, "conflict" as never, "the object changed or was created meanwhile; try again");
const notFound = () => new ApiError(404, "upload_not_found" as never, "no such upload");

export function fakeApi(o: Options = {}) {
  const objects = new Map<string, Blob>(Object.entries(o.existing ?? {}).map(([k, v]) => [k, new Blob([v])]));
  const sessions = new Map<string, { name: string; upsert: boolean; type: string; parts: Map<number, Blob> }>();
  const calls: Call[] = [];
  let next = 0;

  const api: UploadApi = {
    async put(_bucket, name, body, { upsert, contentType, onProgress }) {
      calls.push({ op: "put", name, upsert });
      const e = o.failPut?.();
      if (e) throw e;
      if (objects.has(name) && !upsert) throw conflict();
      onProgress(body.size);
      objects.set(name, new Blob([body], { type: contentType }));
    },
    async create(_bucket, name, { upsert, contentType }) {
      calls.push({ op: "create", name, upsert });
      if (objects.has(name) && !upsert) throw conflict();
      const id = `s${++next}`;
      sessions.set(id, { name, upsert, type: contentType, parts: new Map() });
      return id;
    },
    async listParts(session) {
      calls.push({ op: "listParts" });
      const s = sessions.get(session);
      if (!s) throw notFound();
      return new Map([...s.parts].map(([n, b]) => [n, b.size]));
    },
    async putPart(session, n, body, { onProgress }) {
      calls.push({ op: "putPart", n });
      const e = o.failPart?.(n);
      if (e) throw e;
      const s = sessions.get(session);
      if (!s) throw notFound();
      onProgress(body.size);
      s.parts.set(n, body);
    },
    async complete(session) {
      calls.push({ op: "complete" });
      const s = sessions.get(session);
      if (!s) throw notFound();
      const e = o.failComplete?.();
      if (e) {
        sessions.delete(session);
        throw e;
      }
      if (objects.has(s.name) && !s.upsert) throw conflict();
      const ordered = [...s.parts].sort(([a], [b]) => a - b).map(([, b]) => b);
      objects.set(s.name, new Blob(ordered, { type: s.type }));
      sessions.delete(session);
    },
    async abort(session) {
      calls.push({ op: "abort" });
      sessions.delete(session);
    },
  };

  return {
    ...api,
    calls,
    sessions,
    partsSent: () => calls.filter((c) => c.op === "putPart").map((c) => c.n),
    blob: async (name: string) => objects.get(name),
    text: async (name: string) => objects.get(name)?.text(),
    /** Hooks that record what the runner reports, with no waiting between retries. */
    hooks() {
      const h = {
        signal: new AbortController().signal,
        progress: [] as number[],
        sessions: [] as (string | null)[],
        names: [] as string[],
        waits: [] as number[],
        onProgress: (n: number) => void h.progress.push(n),
        onSession: (s: string | null) => void h.sessions.push(s),
        onName: (n: string) => void h.names.push(n),
        wait: async (ms: number) => void h.waits.push(ms),
      } satisfies RunHooks & Record<string, unknown>;
      return h;
    },
  };
}
