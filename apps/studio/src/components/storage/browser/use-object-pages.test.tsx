import { configureManagementClient } from "@chelabase/management-client";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { act, renderHook, waitFor } from "@testing-library/react";
import type { ReactNode } from "react";
import { describe, expect, it } from "vitest";
import { fakeFetch, jsonResponse } from "@/test/fake-fetch";
import { OBJECT_PAGE, useObjectPages, type ObjectQuery } from "./use-object-pages";

const LIST = "POST /v1/databases/d1/branches/b1/storage/object/list/avatars";
const NAMES = Array.from({ length: 500 }, (_, i) => `f${String(i).padStart(3, "0")}.png`);

/** A storage listing over NAMES: the cursor is the last name of the page before. */
function bucket500() {
  return fakeFetch({
    [LIST]: async (req) => {
      const body = (await req.clone().json()) as { cursor?: string | null; limit: number };
      const start = body.cursor ? NAMES.indexOf(body.cursor) + 1 : 0;
      const page = NAMES.slice(start, start + body.limit);
      const next = start + body.limit < NAMES.length ? page.at(-1)! : null;
      return jsonResponse({ items: page.map((name) => ({ name, folder: false, size: 1, etag: "e", content_type: "image/png", updated_at: 1 })), next_cursor: next });
    },
  });
}

function setup(f: ReturnType<typeof fakeFetch>, initial: ObjectQuery) {
  configureManagementClient({ baseUrl: "http://studio.test", getToken: () => "t", onUnauthorized: () => false, fetchImpl: f });
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  const wrapper = ({ children }: { children: ReactNode }) => <QueryClientProvider client={qc}>{children}</QueryClientProvider>;
  return renderHook((q: ObjectQuery) => useObjectPages("d1", "b1", null, "avatars", q, true), { wrapper, initialProps: initial });
}
const bodies = async (f: ReturnType<typeof fakeFetch>) => Promise.all(f.calls.map((r) => r.clone().json() as Promise<Record<string, unknown>>));
const Q: ObjectQuery = { prefix: "", sort: "name", order: "asc", search: "" };

describe("useObjectPages", () => {
  // #104 AC2: 500 objects load page by page by cursor, with no duplicates.
  it("pages_through_500_objects_by_cursor_without_duplicates", async () => {
    const f = bucket500();
    const { result } = setup(f, Q);
    await waitFor(() => expect(result.current.items).toHaveLength(OBJECT_PAGE));
    while (result.current.hasMore) {
      const before = result.current.items.length;
      await act(() => result.current.loadMore());
      await waitFor(() => expect(result.current.items.length).toBeGreaterThan(before));
    }
    expect(result.current.items.map((i) => i.name)).toEqual(NAMES);
    const sent = await bodies(f);
    expect(sent[0].cursor ?? null).toBeNull();
    expect(sent.slice(1).map((b) => b.cursor)).toEqual([99, 199, 299, 399].map((i) => NAMES[i]));
    expect(sent.every((b) => b.delimiter === "/" && b.limit === OBJECT_PAGE)).toBe(true);
  });

  it("an_item_repeated_across_pages_is_shown_once", async () => {
    let n = 0;
    const f = fakeFetch({
      [LIST]: () => {
        n++;
        const item = (name: string) => ({ name, folder: false, size: 1, etag: "e", content_type: null, updated_at: 1 });
        return jsonResponse(n === 1 ? { items: [item("a"), item("b")], next_cursor: "c1" } : { items: [item("b"), item("c")], next_cursor: null });
      },
    });
    const { result } = setup(f, Q);
    await waitFor(() => expect(result.current.items).toHaveLength(2));
    await act(() => result.current.loadMore());
    await waitFor(() => expect(result.current.items.map((i) => i.name)).toEqual(["a", "b", "c"]));
  });

  // #104 AC7: a new sort or search starts again from page one.
  it("a_new_sort_or_search_starts_from_page_one", async () => {
    const f = bucket500();
    const { result, rerender } = setup(f, Q);
    await waitFor(() => expect(result.current.items).toHaveLength(OBJECT_PAGE));
    await act(() => result.current.loadMore());
    await waitFor(() => expect(result.current.items).toHaveLength(2 * OBJECT_PAGE));
    const count = f.calls.length;
    rerender({ ...Q, sort: "size", order: "desc" });
    await waitFor(() => expect(f.calls.length).toBe(count + 1));
    await waitFor(() => expect(result.current.items).toHaveLength(OBJECT_PAGE));
    let sent = (await bodies(f)).at(-1)!;
    expect(sent).toMatchObject({ sort_by: "size", order: "desc" });
    expect(sent.cursor ?? null).toBeNull();
    rerender({ ...Q, sort: "size", order: "desc", search: "f1" });
    await waitFor(() => expect(f.calls.length).toBe(count + 2));
    sent = (await bodies(f)).at(-1)!;
    expect(sent).toMatchObject({ search: "f1" });
    expect(sent.cursor ?? null).toBeNull();
  });

  it("lists_under_the_folder_prefix", async () => {
    const f = bucket500();
    setup(f, { ...Q, prefix: "photos/" });
    await waitFor(() => expect(f.calls).toHaveLength(1));
    expect((await bodies(f))[0]).toMatchObject({ prefix: "photos/" });
  });
});
