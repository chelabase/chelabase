import { renderHook, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { fetchUrlBlob, useBlobUrl } from "./blob-url";

const URL_A = "https://api.test/storage/v1/object/sign/avatars/a.png?token=t";
const URL_B = "https://api.test/storage/v1/object/sign/avatars/b.png?token=t";

afterEach(() => vi.unstubAllGlobals());

function stubObjectUrls() {
  const made: Blob[] = [];
  const create = vi.fn((b: Blob) => (made.push(b), `blob:t-${made.length}`));
  const revoke = vi.fn();
  Object.assign(URL, { createObjectURL: create, revokeObjectURL: revoke });
  return { made, create, revoke };
}

// O13: Studio's CSP admits images and frames only from its own origin and blob:, so an image
// from a URL on the API host is fetched (connect-src) and shown from a typed blob: URL.
describe("fetchUrlBlob", () => {
  it("asks_for_the_range_given_and_returns_the_body", async () => {
    const fetchMock = vi.fn(async () => new Response("abc", { status: 206 }));
    vi.stubGlobal("fetch", fetchMock);
    const blob = await fetchUrlBlob(URL_A, "bytes=0-9", new AbortController().signal);
    expect(await blob.text()).toBe("abc");
    const [url, init] = fetchMock.mock.calls[0] as unknown as [string, RequestInit];
    expect(url).toBe(URL_A);
    expect(new Headers(init.headers).get("Range")).toBe("bytes=0-9");
  });

  it("a_refused_read_is_an_error", async () => {
    vi.stubGlobal("fetch", vi.fn(async () => new Response("no", { status: 403 })));
    await expect(fetchUrlBlob(URL_A, undefined, new AbortController().signal)).rejects.toThrow(/403/);
  });
});

describe("useBlobUrl", () => {
  it("shows_the_bytes_from_a_blob_url_typed_as_given_and_revokes_it_on_change_and_unmount", async () => {
    const { made, revoke } = stubObjectUrls();
    vi.stubGlobal("fetch", vi.fn(async () => new Response(new Uint8Array([1]), { headers: { "content-type": "text/html" } })));
    const { result, rerender, unmount } = renderHook(({ url }) => useBlobUrl(url, "image/png"), { initialProps: { url: URL_A as string | null } });
    await waitFor(() => expect(result.current).toBe("blob:t-1"));
    // Whatever the response said, the blob is the type shown.
    expect(made[0].type).toBe("image/png");
    rerender({ url: URL_B });
    expect(revoke).toHaveBeenCalledWith("blob:t-1");
    await waitFor(() => expect(result.current).toBe("blob:t-2"));
    unmount();
    expect(revoke).toHaveBeenCalledWith("blob:t-2");
  });

  it("no_url_or_a_failed_read_gives_none", async () => {
    stubObjectUrls();
    vi.stubGlobal("fetch", vi.fn(async () => new Response("no", { status: 403 })));
    const { result, rerender } = renderHook(({ url }) => useBlobUrl(url, "image/png"), { initialProps: { url: null as string | null } });
    expect(result.current).toBeNull();
    rerender({ url: URL_A });
    await waitFor(() => expect(fetch).toHaveBeenCalled());
    expect(result.current).toBeNull();
  });
});
