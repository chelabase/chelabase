import { useEffect, useRef, useState, type DragEvent, type ReactNode } from "react";
import { Upload } from "lucide-react";
import { cn } from "@/lib/utils";
import { droppedFiles, inputFiles, type DroppedFile } from "../upload/drop-entries";

export interface DropZoneProps {
  /** The role may upload; otherwise a drag shows "read-only" and a drop or paste does nothing. */
  canWrite: boolean;
  onFiles: (files: DroppedFile[]) => void;
  className?: string;
  children: ReactNode;
}

const carriesFiles = (e: DragEvent) => Array.from(e.dataTransfer?.types ?? []).includes("Files");

/** Paste into a field stays the field's. */
function isEditable(target: EventTarget | null): boolean {
  if (!(target instanceof HTMLElement)) return false;
  return target.isContentEditable || target instanceof HTMLInputElement || target instanceof HTMLTextAreaElement || target instanceof HTMLSelectElement;
}

/**
 * The object pane as a drop target: files and folders dropped anywhere on it (or files pasted
 * while it's open) are handed to `onFiles` with their relative paths. A drag shows an overlay
 * saying where they'll go, or that the role is read-only.
 */
export function DropZone({ canWrite, onFiles, className, children }: DropZoneProps) {
  const [dragging, setDragging] = useState(false);
  // dragenter and dragleave fire for every child crossed: count them.
  const depth = useRef(0);
  const onFilesRef = useRef(onFiles);
  onFilesRef.current = onFiles;

  useEffect(() => {
    if (!canWrite) return;
    const paste = (e: ClipboardEvent) => {
      if (isEditable(e.target)) return;
      const files = e.clipboardData?.files;
      if (!files || files.length === 0) return;
      e.preventDefault();
      onFilesRef.current(inputFiles(files));
    };
    window.addEventListener("paste", paste);
    return () => window.removeEventListener("paste", paste);
  }, [canWrite]);

  const over = (e: DragEvent) => {
    if (!carriesFiles(e)) return;
    // Always: otherwise the browser opens the dropped file in place of Studio.
    e.preventDefault();
    e.dataTransfer.dropEffect = canWrite ? "copy" : "none";
  };

  return (
    <div
      className={cn("relative", className)}
      onDragEnter={(e) => {
        if (!carriesFiles(e)) return;
        over(e);
        depth.current++;
        setDragging(true);
      }}
      onDragOver={over}
      onDragLeave={(e) => {
        if (!carriesFiles(e)) return;
        depth.current = Math.max(0, depth.current - 1);
        if (depth.current === 0) setDragging(false);
      }}
      onDrop={(e) => {
        if (!carriesFiles(e)) return;
        e.preventDefault();
        depth.current = 0;
        setDragging(false);
        if (!canWrite) return;
        const items = e.dataTransfer.items;
        const files = e.dataTransfer.files;
        // Entries are read synchronously inside droppedFiles, before the event ends.
        const found = items && items.length > 0 ? droppedFiles(items) : Promise.resolve(inputFiles(files));
        void found.then((list) => list.length > 0 && onFilesRef.current(list));
      }}
    >
      {children}
      {dragging && (
        <div
          role="status"
          className={cn(
            "pointer-events-none absolute inset-0 z-20 flex items-center justify-center border-2 border-dashed",
            canWrite ? "border-sand bg-ink/80" : "border-line bg-ink/80",
          )}
        >
          <p className={cn("flex items-center gap-2 text-sm", canWrite ? "text-chalk" : "text-chalk-dim")}>
            {canWrite && <Upload aria-hidden="true" size={16} strokeWidth={1.5} />}
            {canWrite ? "Drop to upload into this folder" : "This bucket is read-only for you: uploads need Developer access."}
          </p>
        </div>
      )}
    </div>
  );
}
