import { useEffect, useState } from "react";
import { errorText } from "@/lib/errors";
import { IMAGE_PREVIEW_BYTES, TEXT_PREVIEW_BYTES, prettyText, previewKind } from "./preview-kind";

export interface ObjectPreviewProps {
  name: string;
  contentType: string | null;
  size: number | null;
  /** Mints a signed URL for the object; null when the role can't sign (below Developer). */
  sign: (() => Promise<string>) | null;
}

type Loaded = { state: "loading" } | { state: "error"; message: string } | { state: "ready"; url: string; text?: string };

function Note({ children }: { children: React.ReactNode }) {
  return <p className="border border-line p-4 text-sm text-chalk-dim">{children}</p>;
}

/**
 * A side-panel preview from a signed URL: images, video, audio and PDF load from it (storage
 * serves them sandboxed, never sniffed); text and JSON are read as text (at most the first
 * 2 MiB, by range) and shown as source, so stored HTML never renders here. SVG and unknown types
 * get no preview.
 */
export function ObjectPreview({ name, contentType, size, sign }: ObjectPreviewProps) {
  const kind = previewKind(contentType);
  const tooBig = kind === "image" && size !== null && size > IMAGE_PREVIEW_BYTES;
  const wanted = kind !== "none" && !tooBig && sign !== null;
  const [loaded, setLoaded] = useState<Loaded>({ state: "loading" });

  useEffect(() => {
    if (!wanted || !sign) return;
    let live = true;
    const controller = new AbortController();
    setLoaded({ state: "loading" });
    (async () => {
      const url = await sign();
      if (kind !== "text") return { state: "ready" as const, url };
      const response = await fetch(url, { headers: { Range: `bytes=0-${TEXT_PREVIEW_BYTES - 1}` }, signal: controller.signal });
      if (!response.ok) throw new Error(`The file couldn't be read (${response.status})`);
      return { state: "ready" as const, url, text: prettyText(await response.text(), contentType) };
    })().then(
      (next) => live && setLoaded(next),
      (err: unknown) => live && !controller.signal.aborted && setLoaded({ state: "error", message: errorText(err) }),
    );
    return () => {
      live = false;
      controller.abort();
    };
    // One load per object: `sign` is a new function each render.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [wanted, name, contentType, kind]);

  if (kind === "none") return <Note>No preview for this type. Download the file to open it.</Note>;
  if (tooBig) return <Note>Too large to preview here. Download the file to open it.</Note>;
  if (!sign) return <Note>Previews load from a signed URL, and signing needs Developer access.</Note>;
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
