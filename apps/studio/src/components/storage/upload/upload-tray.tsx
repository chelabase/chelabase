import { useState } from "react";
import { ChevronDown, ChevronUp, Pause, Play, RotateCw, X } from "lucide-react";
import { Button } from "@/components/ui/button";
import { formatBytes } from "@/lib/table-editor/dry-run";
import { cn } from "@/lib/utils";
import type { UploadItem } from "./queue-reducer";
import type { UploadQueue } from "./use-upload-queue";

export interface UploadTrayProps {
  queue: UploadQueue;
  /** A write-once bucket: Replace would only be refused, so a conflict offers Skip and Keep both. */
  writeOnce: boolean;
}

function statusText(i: UploadItem): string {
  switch (i.status) {
    case "queued":
      return "Waiting";
    case "uploading":
      return i.speed === null ? `${formatBytes(i.loaded)} of ${formatBytes(i.total)}` : `${formatBytes(i.loaded)} of ${formatBytes(i.total)}, ${formatBytes(i.speed)}/s`;
    case "paused":
      return `Paused at ${formatBytes(i.loaded)} of ${formatBytes(i.total)}`;
    case "done":
      return i.name === i.key ? "Uploaded" : `Uploaded as ${i.name.slice(i.name.lastIndexOf("/") + 1)}`;
    case "failed":
      return i.error?.message ?? "Failed";
    case "skipped":
      return i.cancelled ? "Cancelled" : "Skipped";
  }
}

function Row({ item: i, queue, writeOnce }: { item: UploadItem; queue: UploadQueue; writeOnce: boolean }) {
  const pct = i.total === 0 ? (i.status === "done" ? 100 : 0) : Math.round((i.loaded / i.total) * 100);
  const failed = i.status === "failed";
  const conflict = failed && i.error?.conflict === true;
  return (
    <li className="space-y-1 px-3 py-2">
      <div className="flex items-center gap-2">
        <span className="min-w-0 flex-1 truncate font-mono text-xs" title={i.key}>
          {i.key}
        </span>
        {(i.status === "uploading" || i.status === "queued") && (
          <Button size="icon-xs" variant="ghost" aria-label={`Pause ${i.key}`} onClick={() => queue.pause(i.id)}>
            <Pause aria-hidden="true" strokeWidth={1.5} />
          </Button>
        )}
        {i.status === "paused" && (
          <Button size="icon-xs" variant="ghost" aria-label={`Resume ${i.key}`} onClick={() => queue.resume(i.id)}>
            <Play aria-hidden="true" strokeWidth={1.5} />
          </Button>
        )}
        {failed && !conflict && (
          <Button size="icon-xs" variant="ghost" aria-label={`Retry ${i.key}`} onClick={() => queue.retry(i.id)}>
            <RotateCw aria-hidden="true" strokeWidth={1.5} />
          </Button>
        )}
        {i.status !== "done" && i.status !== "skipped" && (
          <Button size="icon-xs" variant="ghost" aria-label={`Cancel ${i.key}`} onClick={() => queue.cancel(i.id)}>
            <X aria-hidden="true" strokeWidth={1.5} />
          </Button>
        )}
      </div>
      {(i.status === "uploading" || i.status === "paused") && (
        <div role="progressbar" aria-label={`${i.key} progress`} aria-valuemin={0} aria-valuemax={100} aria-valuenow={pct} className="h-1 bg-chalk/[0.08]">
          <div className={cn("h-full", i.status === "paused" ? "bg-chalk-dim" : "bg-sand")} style={{ width: `${pct}%` }} />
        </div>
      )}
      <p className={cn("text-xs", failed ? "text-coral" : i.status === "done" ? "text-palm" : "text-chalk-dim")}>{statusText(i)}</p>
      {conflict && (
        <div className="flex gap-1">
          <Button size="xs" variant="ghost" onClick={() => queue.resolve(i.id, "skip")}>
            Skip
          </Button>
          {!writeOnce && (
            <Button size="xs" variant="ghost" onClick={() => queue.resolve(i.id, "replace")}>
              Replace
            </Button>
          )}
          <Button size="xs" variant="ghost" onClick={() => queue.resolve(i.id, "keep-both")}>
            Keep both
          </Button>
        </div>
      )}
    </li>
  );
}

/**
 * The uploads of this view, docked at the bottom of the object pane: each file's progress and
 * speed, with pause, resume, cancel, retry, and a choice for a taken name.
 */
export function UploadTray({ queue, writeOnce }: UploadTrayProps) {
  const [open, setOpen] = useState(true);
  const items = queue.state.items;
  if (items.length === 0) return null;
  const finished = items.filter((i) => i.status === "done").length;
  const failed = items.filter((i) => i.status === "failed").length;
  const clearable = items.some((i) => i.status === "done" || i.status === "skipped");
  const summary = `${finished} of ${items.length} uploaded${failed > 0 ? `, ${failed} need attention` : ""}`;
  return (
    <section aria-label="Uploads" className="flex max-h-72 shrink-0 flex-col border-t border-line bg-ink">
      <div className="flex items-center gap-2 px-3 py-1.5 text-sm">
        <p role="status" className={cn("flex-1", failed > 0 && "text-coral")}>
          {summary}
        </p>
        {clearable && (
          <Button size="xs" variant="ghost" onClick={queue.clear}>
            Clear finished
          </Button>
        )}
        <Button size="icon-xs" variant="ghost" aria-label={open ? "Hide uploads" : "Show uploads"} aria-expanded={open} onClick={() => setOpen(!open)}>
          {open ? <ChevronDown aria-hidden="true" strokeWidth={1.5} /> : <ChevronUp aria-hidden="true" strokeWidth={1.5} />}
        </Button>
      </div>
      {open && (
        <ul className="min-h-0 divide-y divide-line overflow-y-auto border-t border-line">
          {items.map((i) => (
            <Row key={i.id} item={i} queue={queue} writeOnce={writeOnce} />
          ))}
        </ul>
      )}
    </section>
  );
}
