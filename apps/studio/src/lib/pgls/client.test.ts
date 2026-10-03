import { describe, expect, it } from "vitest";
import {
  createPglsClientWith,
  type PglsRequest,
  type PglsResponse,
  type PglsWorkerLike,
} from "./client";

const encoder = new TextEncoder();
const bytes = (s: string) => encoder.encode(s).length;

/** A Worker stand-in: records requests and answers them with `answer` on a later microtask. */
class FakeWorker implements PglsWorkerLike {
  onmessage: ((ev: MessageEvent<PglsResponse>) => void) | null = null;
  onerror: ((ev: Event) => void) | null = null;
  requests: PglsRequest[] = [];
  terminated = false;
  constructor(private answer: (req: PglsRequest) => unknown = () => undefined) {}
  postMessage(req: PglsRequest): void {
    this.requests.push(req);
    queueMicrotask(() => this.emit({ id: req.id, ok: true, value: this.answer(req) }));
  }
  terminate(): void {
    this.terminated = true;
  }
  emit(msg: PglsResponse): void {
    this.onmessage?.(new MessageEvent("message", { data: msg }));
  }
}

describe("pgls client", () => {
  it("client_rejects_ready_when_the_worker_errors", async () => {
    const worker = new FakeWorker();
    const client = createPglsClientWith(worker);
    worker.onerror?.(new Event("error"));
    await expect(client.ready).rejects.toThrow(/failed to load/i);
    // Later calls fail fast instead of hanging.
    await expect(client.lint()).rejects.toThrow();
  });

  it("rejects ready when the wasm fails to initialise", async () => {
    const worker = new FakeWorker();
    const client = createPglsClientWith(worker);
    worker.emit({ ready: false, error: "wasm compile error" });
    await expect(client.ready).rejects.toThrow("wasm compile error");
  });

  it("resolves ready when the worker says so", async () => {
    const worker = new FakeWorker();
    const client = createPglsClientWith(worker);
    worker.emit({ ready: true });
    await expect(client.ready).resolves.toBeUndefined();
  });

  it("lint_maps_byte_ranges_to_utf16", async () => {
    const text = "select 'é😀表' frm t";
    const bad = text.indexOf("frm");
    const worker = new FakeWorker((req) =>
      req.op === "lint"
        ? [
            { start: bytes(text.slice(0, bad)), end: bytes(text.slice(0, bad + 3)), message: "syntax error", severity: "error" },
            { start: 0, end: bytes(text), message: "hint here", severity: "hint" },
          ]
        : undefined,
    );
    const client = createPglsClientWith(worker);
    await client.update(text);
    const diags = await client.lint();
    expect(diags).toEqual([
      { start: bad, end: bad + 3, message: "syntax error", severity: "error" },
      { start: 0, end: text.length, message: "hint here", severity: "hint" },
    ]);
  });

  it("maps the lint result against the text it was asked about", async () => {
    const first = "select 'é' frm";
    const worker = new FakeWorker((req) =>
      req.op === "lint" ? [{ start: bytes("select 'é' "), end: bytes(first), message: "x", severity: "error" }] : undefined,
    );
    const client = createPglsClientWith(worker);
    void client.update(first);
    const pending = client.lint();
    void client.update("😀😀😀 completely different");
    expect(await pending).toEqual([{ start: 11, end: 14, message: "x", severity: "error" }]);
  });

  it("sends completion and hover offsets as UTF-8 bytes", async () => {
    const text = "select 😀, x from ";
    const worker = new FakeWorker((req) =>
      req.op === "complete" ? [{ label: "users", kind: "table", detail: "public" }] : req.op === "hover" ? "**x**" : undefined,
    );
    const client = createPglsClientWith(worker);
    await client.update(text);
    expect(await client.complete(text.length)).toEqual([{ label: "users", kind: "table", detail: "public" }]);
    expect(await client.hover(text.indexOf("x"))).toBe("**x**");
    const complete = worker.requests.find((r) => r.op === "complete");
    const hover = worker.requests.find((r) => r.op === "hover");
    expect(complete).toMatchObject({ offset: bytes(text) });
    expect(hover).toMatchObject({ offset: bytes(text.slice(0, text.indexOf("x"))) });
  });

  it("skips update when the text hasn't changed, and setSchema passes the JSON through", async () => {
    const worker = new FakeWorker();
    const client = createPglsClientWith(worker);
    await client.update("select 1");
    await client.update("select 1");
    await client.setSchema('{"tables":[]}');
    expect(worker.requests.map((r) => r.op)).toEqual(["update", "setSchema"]);
    expect(worker.requests[1]).toMatchObject({ op: "setSchema", json: '{"tables":[]}' });
  });

  it("rejects a call with the worker's error message", async () => {
    const worker = new FakeWorker();
    worker.postMessage = (req) => {
      worker.requests.push(req);
      queueMicrotask(() => worker.emit({ id: req.id, ok: false, error: "Invalid schema" }));
    };
    const client = createPglsClientWith(worker);
    await expect(client.setSchema("{bad")).rejects.toThrow("Invalid schema");
  });

  it("resends the text after a failed update", async () => {
    const worker = new FakeWorker();
    let failNext = true;
    worker.postMessage = (req) => {
      worker.requests.push(req);
      const fail = failNext;
      failNext = false;
      queueMicrotask(() => worker.emit(fail ? { id: req.id, ok: false, error: "boom" } : { id: req.id, ok: true }));
    };
    const client = createPglsClientWith(worker);
    await expect(client.update("select 1")).rejects.toThrow("boom");
    await client.update("select 1");
    expect(worker.requests.map((r) => r.op)).toEqual(["update", "update"]);
  });

  it("dispose terminates the worker and rejects pending calls", async () => {
    const worker = new FakeWorker();
    worker.postMessage = (req) => void worker.requests.push(req); // never answers
    const client = createPglsClientWith(worker);
    const pending = client.lint();
    client.dispose();
    await expect(pending).rejects.toThrow(/disposed/);
    expect(worker.terminated).toBe(true);
  });
});
