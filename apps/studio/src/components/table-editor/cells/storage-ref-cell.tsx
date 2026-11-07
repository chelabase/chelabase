import { useEffect, useState, type DragEvent, type ReactNode } from "react";
import type { StorageRefDesc } from "@chelabase/management-client";
import { File as FileIcon, FileAudio, FileImage, FileText, FileVideo } from "lucide-react";
import { ObjectPreview, type PreviewSource } from "@/components/storage/browser/object-preview";
import { baseName } from "@/components/storage/browser/paths";
import { previewKind } from "@/components/storage/browser/preview-kind";
import { Button } from "@/components/ui/button";
import { Modal } from "@/components/ui/dialog";
import { Select } from "@/components/ui/select";
import { errorText } from "@/lib/errors";
import { cellText } from "@/lib/table-editor/rows";
import { cn } from "@/lib/utils";
import type { CellValue } from "../cell-editor";
import { guessContentType, refRequestOf, refValue, type RefRequest, type RefTarget } from "./storage-ref";
import type { CellUpload } from "./storage-ref-upload";
import type { RefState } from "./use-storage-refs";

/** The name a ref shows before (or without) its object: its object name, or a short id. */
function refLabel(r: RefRequest): string {
  return "id" in r ? `${r.id.slice(0, 8)}…` : baseName(r.name);
}

function TypeIcon({ name }: { name: string }) {
  const props = { "aria-hidden": true, size: 14, strokeWidth: 1.5, className: "shrink-0 text-chalk-dim" } as const;
  switch (previewKind(guessContentType(name))) {
    case "image":
      return <FileImage {...props} />;
    case "video":
      return <FileVideo {...props} />;
    case "audio":
      return <FileAudio {...props} />;
    case "pdf":
    case "text":
      return <FileText {...props} />;
    default:
      return <FileIcon {...props} />;
  }
}

/** A name's extension as a chip label (`PDF`), or `FILE`. */
function typeLabel(name: string): string {
  const base = baseName(name);
  const dot = base.lastIndexOf(".");
  return dot > 0 && base.length - dot <= 6 ? base.slice(dot + 1).toUpperCase() : "FILE";
}

/** The ref a value saves for `target`, or why this column can't hold it. */
function valueFor(desc: StorageRefDesc, target: RefTarget): CellValue | string {
  const value = refValue(desc, target);
  if (value) return value;
  if (desc.kind === "path") return `This column holds files of the ${desc.bucket} bucket.`;
  return desc.kind === "id" ? "The file's id couldn't be read." : "The bucket's id couldn't be read.";
}

const carriesFiles = (e: DragEvent) => Array.from(e.dataTransfer?.types ?? []).includes("Files");

export interface StorageRefCellProps {
  desc: StorageRefDesc;
  value: unknown;
  /** The ref's resolve answer; undefined while it resolves (or when the value names none). */
  resolved: RefState | undefined;
  /** The row's cell may be written (Developer and up, an editable column). */
  canEdit: boolean;
  /** Developer and up: signed URLs open (a Viewer's 403), so thumbnails may load from them. */
  canSign: boolean;
  upload: CellUpload;
  save: (value: CellValue) => Promise<string | null>;
  /** Opens the editor (with a dropped file when no bucket is known for it). */
  onOpen: (file?: File) => void;
}

/**
 * A ref cell: a thumbnail (images, never SVG, from a URL that opens for the role) or a typed chip,
 * "no access" or "missing"; a click opens the editor. A file dropped on a writable cell uploads into
 * the column's bucket (or the current file's) and the row saves the new ref.
 */
export function StorageRefCell({ desc, value, resolved, canEdit, canSign, upload, save, onOpen }: StorageRefCellProps) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const request = refRequestOf(desc, value);

  async function dropped(file: File) {
    const bucket = desc.kind === "path" ? desc.bucket : resolved?.state === "ok" ? resolved.bucket : null;
    if (bucket === null) return onOpen(file);
    setBusy(true);
    setError(null);
    try {
      const target = await upload(bucket, file, { needId: desc.kind === "id" });
      const next = valueFor(desc, target);
      setError(typeof next === "string" ? next : await save(next));
    } catch (e) {
      setError(errorText(e));
    } finally {
      setBusy(false);
    }
  }

  const drop = canEdit
    ? {
        onDragOver: (e: DragEvent) => {
          if (!carriesFiles(e)) return;
          e.preventDefault();
          e.dataTransfer.dropEffect = "copy";
        },
        onDrop: (e: DragEvent) => {
          const file = e.dataTransfer?.files?.[0];
          if (!file) return;
          e.preventDefault();
          e.stopPropagation();
          void dropped(file);
        },
      }
    : {};

  let body: ReactNode;
  if (busy) body = <span className="text-chalk-dim">Uploading…</span>;
  else if (value === null || value === undefined) body = <span className="text-chalk-dim">NULL</span>;
  else if (!request) body = <span className="truncate whitespace-pre">{cellText(value)}</span>;
  else if (!resolved) body = <span className="truncate text-chalk-dim">{refLabel(request)}</span>;
  else if (resolved.state !== "ok") {
    body = (
      <>
        <span className="truncate text-chalk-dim">{refLabel(request)}</span>
        <span className="shrink-0 border border-line px-1 text-[11px] text-coral">{resolved.state === "missing" ? "missing" : "no access"}</span>
      </>
    );
  } else {
    const name = baseName(resolved.name);
    const thumb = previewKind(guessContentType(resolved.name)) === "image" && (resolved.public || canSign);
    body = (
      <>
        {thumb ? <img src={resolved.url} alt={name} loading="lazy" className="h-6 w-6 shrink-0 border border-line bg-ink object-cover" /> : <TypeIcon name={resolved.name} />}
        <span className="truncate">{name}</span>
        {!thumb && <span className="shrink-0 border border-line px-1 text-[10px] text-chalk-dim">{typeLabel(resolved.name)}</span>}
      </>
    );
  }

  const clickable = !busy && (canEdit || (request !== null && resolved !== undefined));
  return (
    <div className="flex h-full min-w-0 items-center gap-1" {...drop}>
      {clickable ? (
        <button
          type="button"
          onClick={(e) => {
            e.stopPropagation();
            onOpen();
          }}
          onDoubleClick={(e) => e.stopPropagation()}
          className="flex min-w-0 items-center gap-1.5 text-left outline-none hover:text-sand focus-visible:outline-2 focus-visible:outline-sand"
        >
          {body}
        </button>
      ) : (
        <span className="flex min-w-0 items-center gap-1.5">{body}</span>
      )}
      {error && (
        <span role="alert" title={error} className="truncate text-[11px] text-coral">
          {error}
        </span>
      )}
    </div>
  );
}

export interface StorageRefEditorProps {
  open: boolean;
  onClose: () => void;
  columnName: string;
  desc: StorageRefDesc;
  nullable: boolean;
  value: unknown;
  resolved: RefState | undefined;
  canEdit: boolean;
  /** Where the preview's bytes come from (a signed or public URL, or the console download route for a Viewer). */
  previewSource: PreviewSource | null;
  /** The buckets the role sees (a ref or id column picks one). */
  buckets: { id: string; name: string }[];
  /** A file dropped on the cell with no bucket known: uploaded once one is chosen. */
  pendingFile: File | null;
  upload: CellUpload;
  save: (value: CellValue) => Promise<string | null>;
  /** The Storage browser's listing of a bucket, calling `onPick` with the file chosen. */
  renderPicker: (bucket: string, onPick: (target: RefTarget) => void) => ReactNode;
}

/** The ref editor: the file's preview, and for a writer: upload a file, pick an existing one, or Clear. */
export function StorageRefEditor({ open, onClose, columnName, desc, nullable, value, resolved, canEdit, previewSource, buckets, pendingFile, upload, save, renderPicker }: StorageRefEditorProps) {
  const fixed = desc.kind === "path" ? desc.bucket : null;
  const initialBucket = fixed ?? (resolved?.state === "ok" ? resolved.bucket : (buckets[0]?.name ?? ""));
  const [picked, setBucket] = useState(initialBucket);
  // The bucket list may arrive after the editor opened: its first bucket until one is chosen.
  const bucket = picked !== "" ? picked : (fixed ?? buckets[0]?.name ?? "");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    if (open) {
      setBucket(initialBucket);
      setError(null);
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open]);

  async function run(next: () => Promise<CellValue | string>) {
    setBusy(true);
    setError(null);
    try {
      const v = await next();
      const failure = typeof v === "string" ? v : await save(v);
      if (failure) setError(failure);
      else onClose();
    } catch (e) {
      setError(errorText(e));
    } finally {
      setBusy(false);
    }
  }
  const uploadFile = (file: File) => run(async () => valueFor(desc, await upload(bucket, file, { needId: desc.kind === "id" })));
  const pick = (target: RefTarget) => run(async () => valueFor(desc, target));
  function clear() {
    if (!nullable) {
      setError(`${columnName} is NOT NULL: upload or pick a file instead.`);
      return;
    }
    void run(async () => ({ null: true }));
  }

  const request = refRequestOf(desc, value);
  let status: ReactNode;
  if (value === null || value === undefined) status = <p className="text-chalk-dim">No file.</p>;
  else if (!request) status = <p className="font-mono text-xs break-all text-chalk-dim">{cellText(value)}</p>;
  else if (!resolved) status = <p className="text-chalk-dim">Loading…</p>;
  else if (resolved.state === "missing") status = <p className="text-coral">The file this row references is gone.</p>;
  else if (resolved.state === "no_access") status = <p className="text-coral">This role can&apos;t read this file (or it is gone).</p>;
  else {
    status = (
      <div className="space-y-2">
        <p className="font-mono text-xs break-all">
          <span className="text-chalk-dim">{resolved.bucket}/</span>
          {resolved.name}
        </p>
        <ObjectPreview name={resolved.name} contentType={guessContentType(resolved.name)} size={null} source={previewSource} />
      </div>
    );
  }

  return (
    <Modal open={open} onOpenChange={(o) => !o && onClose()} title={columnName} size="wide">
      <div className="space-y-4 text-sm">
        {status}
        {canEdit && (
          <div className="space-y-3 border-t border-line pt-4">
            <div className="flex flex-wrap items-end gap-2">
              {fixed === null ? (
                <label className="flex flex-col gap-1">
                  <span>Bucket</span>
                  <Select value={bucket} onChange={(e) => setBucket(e.target.value)} wrapperClassName="w-48" disabled={busy}>
                    {buckets.length === 0 && <option value="">No buckets</option>}
                    {buckets.map((b) => (
                      <option key={b.id} value={b.name}>
                        {b.name}
                      </option>
                    ))}
                  </Select>
                </label>
              ) : (
                <p className="text-chalk-dim">
                  Bucket <span className="font-mono text-chalk">{fixed}</span>
                </p>
              )}
              {pendingFile && (
                <Button type="button" disabled={busy || bucket === ""} onClick={() => void uploadFile(pendingFile)}>
                  Upload {pendingFile.name}
                </Button>
              )}
              <label className={cn("flex flex-col gap-1", (busy || bucket === "") && "opacity-60")}>
                <span>Upload a file</span>
                <input
                  type="file"
                  aria-label="Upload a file"
                  disabled={busy || bucket === ""}
                  onChange={(e) => {
                    const file = e.target.files?.[0];
                    e.target.value = "";
                    if (file) void uploadFile(file);
                  }}
                  className="text-xs"
                />
              </label>
              <Button type="button" variant="outline" disabled={busy} onClick={clear} className="ml-auto">
                Clear
              </Button>
            </div>
            {busy && <p className="text-chalk-dim">Saving…</p>}
            {bucket !== "" && <div className="max-h-72 overflow-hidden border border-line">{renderPicker(bucket, pick)}</div>}
          </div>
        )}
        {error && (
          <p role="alert" className="text-coral">
            {error}
          </p>
        )}
      </div>
    </Modal>
  );
}
