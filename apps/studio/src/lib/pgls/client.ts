// A promise RPC over the pgls module worker (worker.ts). Load this module with a dynamic
// import from the SQL view only: creating the client fetches the 12 MB wasm.
import { byteToUtf16, utf16ToByte } from "./offsets";

export type PglsCompletion = {
  label: string;
  kind: "table" | "column" | "function" | "schema" | "policy" | "role" | "keyword";
  detail?: string;
};

/** A diagnostic with UTF-16 offsets (mapped from pgls's UTF-8 bytes). */
export type PglsDiagnostic = {
  start: number;
  end: number;
  message: string;
  severity: "error" | "warning" | "info" | "hint";
};

export interface PglsClient {
  /** Resolves once the wasm is loaded; rejects when it fails to load. */
  ready: Promise<void>;
  setSchema(json: string): Promise<void>;
  /** Sends the editor text (skipped when it equals the last text sent). */
  update(text: string): Promise<void>;
  /** `offset` is a UTF-16 index into the last text sent. */
  complete(offset: number): Promise<PglsCompletion[]>;
  lint(): Promise<PglsDiagnostic[]>;
  hover(offset: number): Promise<string | null>;
  dispose(): void;
}

// Wire protocol. Offsets in requests and replies are UTF-8 bytes.
export type PglsRequest =
  | { id: number; op: "setSchema"; json: string }
  | { id: number; op: "update"; text: string }
  | { id: number; op: "complete"; offset: number }
  | { id: number; op: "lint" }
  | { id: number; op: "hover"; offset: number };

export type PglsResponse =
  | { id: number; ok: true; value?: unknown }
  | { id: number; ok: false; error: string }
  | { ready: true }
  | { ready: false; error: string };

/** The part of `Worker` the client uses (tests inject a fake). */
export interface PglsWorkerLike {
  postMessage(message: PglsRequest): void;
  terminate(): void;
  onmessage: ((ev: MessageEvent<PglsResponse>) => void) | null;
  onerror: ((ev: Event) => void) | null;
}

type Pending = { resolve: (value: unknown) => void; reject: (error: Error) => void };

type RequestBody<R = PglsRequest> = R extends PglsRequest ? Omit<R, "id"> : never;

export function createPglsClient(): PglsClient {
  const worker = new Worker(new URL("./worker.ts", import.meta.url), {
    type: "module",
    name: "pgls",
  });
  return createPglsClientWith(worker as unknown as PglsWorkerLike);
}

export function createPglsClientWith(worker: PglsWorkerLike): PglsClient {
  const pending = new Map<number, Pending>();
  let nextId = 1;
  let sentText = ""; // the worker starts with an empty /query.sql
  let synced = true; // false after a failed update: the worker may hold other text
  let dead: Error | null = null;

  let settleReady!: (error: Error | null) => void;
  const ready = new Promise<void>((resolve, reject) => {
    settleReady = (error) => (error ? reject(error) : resolve());
  });
  ready.catch(() => {}); // callers that never await `ready` mustn't see an unhandled rejection

  const fail = (error: Error) => {
    if (dead) return;
    dead = error;
    settleReady(error);
    for (const p of pending.values()) p.reject(error);
    pending.clear();
  };

  worker.onmessage = (ev) => {
    const msg = ev.data;
    if ("ready" in msg) {
      settleReady(msg.ready ? null : new Error(msg.error));
      return;
    }
    const p = pending.get(msg.id);
    if (!p) return;
    pending.delete(msg.id);
    if (msg.ok) p.resolve(msg.value);
    else p.reject(new Error(msg.error));
  };
  worker.onerror = () => fail(new Error("The SQL language worker failed to load."));

  function call<T>(body: RequestBody): Promise<T> {
    if (dead) return Promise.reject(dead);
    const id = nextId++;
    return new Promise<T>((resolve, reject) => {
      pending.set(id, { resolve: resolve as (v: unknown) => void, reject });
      worker.postMessage({ ...body, id } as PglsRequest);
    });
  }

  return {
    ready,
    setSchema: (json) => call<void>({ op: "setSchema", json }),
    update(text) {
      if (synced && text === sentText) return dead ? Promise.reject(dead) : Promise.resolve();
      sentText = text;
      synced = true;
      return call<void>({ op: "update", text }).catch((e: unknown) => {
        if (sentText === text) synced = false;
        throw e;
      });
    },
    complete(offset) {
      return call<PglsCompletion[]>({ op: "complete", offset: utf16ToByte(sentText, offset) });
    },
    async lint() {
      // Map against the text this lint is about: the worker answers in order, so later
      // updates can't change what it linted.
      const text = sentText;
      const diags = await call<PglsDiagnostic[]>({ op: "lint" });
      return diags.map((d) => ({
        start: byteToUtf16(text, d.start),
        end: byteToUtf16(text, d.end),
        message: d.message,
        severity: d.severity,
      }));
    },
    hover(offset) {
      return call<string | null>({ op: "hover", offset: utf16ToByte(sentText, offset) });
    },
    dispose() {
      fail(new Error("The SQL language client was disposed."));
      worker.onmessage = null;
      worker.onerror = null;
      worker.terminate();
    },
  };
}
