import { useEffect, useState, type ReactNode } from "react";
import { errorText } from "@/lib/errors";
import { DOWNLOAD_PREVIEW_BYTES, IMAGE_PREVIEW_BYTES, TEXT_PREVIEW_BYTES, essenceOf, prettyText, previewKind } from "./preview-kind";

/**
 * Where a preview's bytes come from: a signed URL (Developer and above: media streams from the
 * API host), or Studio's console download route as the role (a Viewer: the bytes come back as a
 * Blob and play from a `blob:` URL typed as the preview's own type).
 */
export type PreviewSource =
  | { kind: "signed"; sign: () => Promise<string> }
  | { kind: "download"; fetch: (range: string | undefined, signal: AbortSignal) => Promise<Blob> };

export interface ObjectPreviewProps {
  name: string;
  contentType: string | null;
  size: number | null;
  /** None when the role can't read the object's bytes here. */
  source: PreviewSource | null;
}

type Loaded = { state: "loading" } | { state: "error"; message: string } | { state: "ready"; url: string; text?: string };

function Note({ children }: { children: ReactNode }) {
  return <p className="border border-line p-4 text-sm text-chalk-dim">{children}</p>;
}

const TEXT_RANGE = `bytes=0-${TEXT_PREVIEW_BYTES - 1}`;

/**
 * A side-panel preview: images (never SVG), video, audio and PDF from a signed URL or a typed
 * `blob:` URL; text and JSON read as text (at most the first 2 MiB, by range) and shown as source,
 * so stored HTML never renders here. Unknown types get no preview.
 */
export function ObjectPreview({ name, contentType, size, source }: ObjectPreviewProps) {
  const kind = previewKind(contentType);
  const limit = source?.kind === "download" && kind !== "text" ? DOWNLOAD_PREVIEW_BYTES : kind === "image" ? IMAGE_PREVIEW_BYTES : Infinity;
  const tooBig = kind !== "text" && size !== null && size > limit;
  const wanted = kind !== "none" && !tooBig && source !== null;
  const [loaded, setLoaded] = useState<Loaded>({ state: "loading" });

  useEffect(() => {
    if (!wanted || !source) return;
    let live = true;
    let blobUrl: string | null = null;
    const controller = new AbortController();
    setLoaded({ state: "loading" });
    (async (): Promise<Loaded> => {
      if (source.kind === "download") {
        if (kind === "text") return { state: "ready", url: "", text: prettyText(await (await source.fetch(TEXT_RANGE, controller.signal)).text(), contentType) };
        const bytes = await source.fetch(undefined, controller.signal);
        // Typed as what's previewed, whatever the response said: never a document on Studio's origin.
        blobUrl = URL.createObjectURL(new Blob([bytes], { type: essenceOf(contentType) }));
        if (!live) URL.revokeObjectURL(blobUrl);
        return { state: "ready", url: blobUrl };
      }
      const url = await source.sign();
      if (kind !== "text") return { state: "ready", url };
      const response = await fetch(url, { headers: { Range: TEXT_RANGE }, signal: controller.signal });
      if (!response.ok) throw new Error(`The file couldn't be read (${response.status})`);
      return { state: "ready", url, text: prettyText(await response.text(), contentType) };
    })().then(
      (next) => live && setLoaded(next),
      (err: unknown) => live && !controller.signal.aborted && setLoaded({ state: "error", message: errorText(err) }),
    );
    return () => {
      live = false;
      controller.abort();
      if (blobUrl) URL.revokeObjectURL(blobUrl);
    };
    // One load per object: `source` is a new object each render.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [wanted, name, contentType, kind, source?.kind]);

  if (kind === "none") return <Note>No preview for this type. Download the file to open it.</Note>;
  if (tooBig) return <Note>Too large to preview here. Download the file to open it.</Note>;
  if (!source) return <Note>This file can&apos;t be previewed with your access.</Note>;
  if (loaded.state === "loading") return <Note>Loading preview…</Note>;
  if (loaded.state === "error")
    return (
      <p role="alert" className="border border-line p-4 text-sm text-coral">
        {loaded.message}
      </p>
    );

  const frame = "block max-h-[50vh] w-full border border-line bg-ink";
  switch (kind) {
    case "image":
      return <img src={loaded.url} alt={name} className={`${frame} object-contain`} />;
    case "video":
      return <video src={loaded.url} controls preload="metadata" aria-label={name} className={frame} />;
    case "audio":
      return <audio src={loaded.url} controls preload="metadata" aria-label={name} className="w-full" />;
    case "pdf":
      return <iframe src={loaded.url} title={name} className={`${frame} h-[50vh]`} />;
    case "text":
      return (
        <div className="space-y-1">
          <pre data-testid="text-preview" className="max-h-[50vh] overflow-auto border border-line bg-ink p-3 font-mono text-xs whitespace-pre-wrap break-words">
            {loaded.text}
          </pre>
          {size !== null && size > TEXT_PREVIEW_BYTES && <p className="text-xs text-chalk-dim">Showing the first 2 MiB. Download the file for the rest.</p>}
        </div>
      );
  }
}
