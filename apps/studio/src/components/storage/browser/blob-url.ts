import { useEffect, useState } from "react";

/**
 * An object's bytes read from a URL that opens for the role (signed or public), for a `blob:`
 * preview. Studio's CSP admits images and frames only from its own origin and `blob:`, so bytes on
 * the API host are fetched (`connect-src`) rather than embedded by URL.
 */
export async function fetchUrlBlob(url: string, range: string | undefined, signal: AbortSignal): Promise<Blob> {
  const response = await fetch(url, { headers: range ? { Range: range } : {}, signal });
  if (!response.ok) throw new Error(`The file couldn't be read (${response.status})`);
  return response.blob();
}

/**
 * A `blob:` URL for the bytes at `url`, typed as `type` whatever the response said (never a
 * document on Studio's origin); null while it loads, when it fails, or with no URL. The URL is
 * revoked when `url` changes and on unmount.
 */
export function useBlobUrl(url: string | null, type: string): string | null {
  const [blobUrl, setBlobUrl] = useState<string | null>(null);
  useEffect(() => {
    setBlobUrl(null);
    if (!url) return;
    let live = true;
    let made: string | null = null;
    const controller = new AbortController();
    fetchUrlBlob(url, undefined, controller.signal).then(
      (bytes) => {
        if (!live) return;
        made = URL.createObjectURL(new Blob([bytes], { type }));
        setBlobUrl(made);
      },
      () => {},
    );
    return () => {
      live = false;
      controller.abort();
      if (made) URL.revokeObjectURL(made);
    };
  }, [url, type]);
  return blobUrl;
}
