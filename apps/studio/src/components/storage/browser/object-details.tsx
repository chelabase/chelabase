import { useState } from "react";
import { X } from "lucide-react";
import { CopyButton } from "@/components/copy-button";
import { Button } from "@/components/ui/button";
import { errorText } from "@/lib/errors";
import { formatBytes } from "@/lib/table-editor/dry-run";
import type { StorageItem } from "@/lib/storage-api";
import { updatedText } from "./object-list";
import { ObjectPreview } from "./object-preview";
import { baseName } from "./paths";

/** Signed URLs minted for a preview live this long. */
const PREVIEW_TTL = 600;
/** A download's signed URL is used at once. */
const DOWNLOAD_TTL = 60;

export interface ObjectDetailsProps {
  item: StorageItem;
  /** The object's public URL when its bucket is public. */
  publicUrl: string | null;
  /** Mints a signed URL (Developer and above); null for roles that can't sign. */
  sign: ((expiresIn: number) => Promise<{ url: string; expiresAt: number | null }>) | null;
  /** The object's bytes through the console download route, as the role (Viewer and above). */
  fetchObject: (range: string | undefined, signal?: AbortSignal) => Promise<Blob>;
  /** Rename, move, copy and delete (Developer and above). */
  canWrite: boolean;
  onSignUrl: () => void;
  onRename: () => void;
  onMove: () => void;
  onCopy: () => void;
  onDelete: () => void;
  onClose: () => void;
}

/** Starts a download through a temporary link: a signed URL with `download` set (an attachment), or a Blob's URL. */
function startDownload(href: string, filename?: string): void {
  const a = document.createElement("a");
  a.href = href;
  if (filename !== undefined) a.download = filename;
  a.rel = "noopener";
  document.body.append(a);
  a.click();
  a.remove();
}

/** The focused object: a preview, its details and the actions the role may take. */
export function ObjectDetails({ item, publicUrl, sign, fetchObject, canWrite, onSignUrl, onRename, onMove, onCopy, onDelete, onClose }: ObjectDetailsProps) {
  const [downloadError, setDownloadError] = useState<string | null>(null);
  const [downloading, setDownloading] = useState(false);
  const name = baseName(item.name);
  const rows: [string, string, boolean][] = [
    ["Size", item.size === null ? "Unknown" : formatBytes(item.size), false],
    ["Type", item.contentType ?? "Unknown", item.contentType !== null],
    ["ETag", item.etag ?? "Unknown", item.etag !== null],
    ["Last modified", item.updatedAt === null ? "Unknown" : updatedText(item.updatedAt), false],
  ];
  async function download() {
    setDownloadError(null);
    setDownloading(true);
    try {
      if (sign) {
        const { url } = await sign(DOWNLOAD_TTL);
        startDownload(`${url}${url.includes("?") ? "&" : "?"}download=`);
      } else {
        // A Viewer can't sign: the bytes come through the console route, saved as an octet stream.
        const blob = await fetchObject(undefined);
        const href = URL.createObjectURL(new Blob([blob], { type: "application/octet-stream" }));
        startDownload(href, name);
        setTimeout(() => URL.revokeObjectURL(href), 0);
      }
    } catch (err) {
      setDownloadError(errorText(err));
    } finally {
      setDownloading(false);
    }
  }
  return (
    <aside aria-label="Object details" className="flex w-96 shrink-0 flex-col overflow-y-auto border-l border-line">
      <div className="flex h-11 shrink-0 items-center gap-2 border-b border-line px-4">
        <h3 className="min-w-0 flex-1 truncate font-mono text-sm font-semibold" title={item.name}>
          {name}
        </h3>
        <Button variant="ghost" size="icon-sm" aria-label="Close details" onClick={onClose}>
          <X aria-hidden="true" strokeWidth={1.5} />
        </Button>
      </div>
      <div className="space-y-4 p-4 text-sm">
        <ObjectPreview key={`${item.name}\n${item.etag ?? ""}`} name={name} contentType={item.contentType} size={item.size} source={sign ? { kind: "signed", sign: async () => (await sign(PREVIEW_TTL)).url } : { kind: "download", fetch: fetchObject }} />
        <div className="flex flex-wrap gap-2">
          <Button size="sm" disabled={downloading} onClick={() => void download()}>
            Download
          </Button>
          <CopyButton value={item.name} label="Copy path" text="Copy path" />
          {publicUrl && <CopyButton value={publicUrl} label="Copy public URL" text="Copy public URL" />}
          {sign && (
            <Button size="sm" variant="outline" onClick={onSignUrl}>
              Copy signed URL
            </Button>
          )}
        </div>
        {downloadError && (
          <p role="alert" className="text-coral">
            {downloadError}
          </p>
        )}
        {canWrite && (
          <div className="flex flex-wrap gap-2 border-t border-line pt-4">
            <Button size="sm" variant="outline" onClick={onRename}>
              Rename
            </Button>
            <Button size="sm" variant="outline" onClick={onMove}>
              Move
            </Button>
            <Button size="sm" variant="outline" onClick={onCopy}>
              Copy
            </Button>
            <Button size="sm" variant="destructive" onClick={onDelete}>
              Delete
            </Button>
          </div>
        )}
        <dl className="grid grid-cols-[auto_1fr] gap-x-4 gap-y-2 border-t border-line pt-4">
          <dt className="text-chalk-dim">Path</dt>
          <dd className="font-mono break-all">{item.name}</dd>
          {rows.map(([term, value, mono]) => (
            <div key={term} className="contents">
              <dt className="text-chalk-dim">{term}</dt>
              <dd className={mono ? "font-mono break-all" : undefined}>{value}</dd>
            </div>
          ))}
        </dl>
      </div>
    </aside>
  );
}
