import { act, waitFor } from "@testing-library/react";
import { EditorView } from "@codemirror/view";
import { expect } from "vitest";
import type { Branch, Merge, MergeLogEntry } from "@chelabase/management-client";
import { MergePage } from "@/components/merges/merge-page";
import type { PglsClient } from "@/lib/pgls/client";
import { fakeFetch, jsonResponse, type FakeFetch, type FakeHandler } from "./fake-fetch";
import { detailOf, devBranch, mainBranch } from "./merge-fixtures";
import { renderWithRouter } from "./render-router";

export const MERGE_URL = "/v1/databases/db/merges/m1";

export interface MergePageOpts {
  /** Answers `getMerge` (default: the merge, always). */
  get?: FakeHandler;
  events?: MergeLogEntry[];
  branches?: Branch[];
  routes?: Record<string, FakeHandler>;
  createClient?: () => Promise<PglsClient>;
}

/** The merge page for `merge` (database `db`, project `p1`), inside a router, on a fake API. */
export function renderMergePage(merge: Merge, opts: MergePageOpts = {}) {
  const f: FakeFetch = fakeFetch({
    [`GET ${MERGE_URL}`]: opts.get ?? (() => jsonResponse(detailOf(merge, opts.events))),
    "GET /v1/databases/db/branches": () => jsonResponse(opts.branches ?? [mainBranch, devBranch]),
    ...opts.routes,
  });
  const result = renderWithRouter(
    <MergePage
      projectId="p1"
      databaseId="db"
      mergeId="m1"
      createClient={opts.createClient ?? (() => Promise.reject(new Error("no worker")))}
    />,
    { fetch: f },
  );
  return { ...result, fetch: f };
}

/** The bodies of the calls to `method path`, parsed. */
export const bodies = (f: FakeFetch, method: string, path: string) =>
  Promise.all(
    f.calls.filter((r) => r.method === method && new URL(r.url).pathname === path).map((r) => r.clone().json() as Promise<unknown>),
  );

/** Replaces the text of the CodeMirror editor labelled `label`. */
export async function setEditorText(label: string, text: string) {
  const view = await waitFor(() => {
    const content = document.querySelector(`[aria-label="${label}"]`);
    expect(content).not.toBeNull();
    return EditorView.findFromDOM(content!.closest(".cm-editor") as HTMLElement)!;
  });
  act(() => view.dispatch({ changes: { from: 0, to: view.state.doc.length, insert: text } }));
}
