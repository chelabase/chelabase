/// <reference lib="webworker" />
// The pgls module worker: one wasm workspace holding the editor's text as /query.sql. pgls calls
// are synchronous and can take a while on big scripts, hence a worker. Offsets are UTF-8 bytes;
// client.ts converts.
import { createWorkspace, type Workspace } from "@postgres-language-server/wasm/workspace";
import type { PglsRequest, PglsResponse } from "./client";

const PATH = "/query.sql";
const scope = self as unknown as DedicatedWorkerGlobalScope;
const post = (msg: PglsResponse) => scope.postMessage(msg);
const message = (e: unknown) => (e instanceof Error ? e.message : String(e));

const workspace: Promise<Workspace> = createWorkspace().then((ws) => {
  ws.insertFile(PATH, "");
  return ws;
});
workspace.then(
  () => post({ ready: true }),
  (e) => post({ ready: false, error: message(e) }),
);

function run(ws: Workspace, req: PglsRequest): unknown {
  switch (req.op) {
    case "setSchema":
      ws.setSchema(req.json);
      return undefined;
    case "update":
      ws.insertFile(PATH, req.text);
      return undefined;
    case "complete":
      return ws.complete(PATH, req.offset);
    case "lint":
      return ws.lint(PATH);
    case "hover":
      return ws.hover(PATH, req.offset);
  }
}

// Awaiting the same promise keeps requests in arrival order.
scope.onmessage = async (ev: MessageEvent<PglsRequest>) => {
  const req = ev.data;
  try {
    const value = run(await workspace, req);
    post({ id: req.id, ok: true, value });
  } catch (e) {
    post({ id: req.id, ok: false, error: message(e) });
  }
};
