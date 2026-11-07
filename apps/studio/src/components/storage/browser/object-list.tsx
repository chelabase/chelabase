import { useEffect, useRef, useState, type MouseEvent } from "react";
import { useVirtualizer } from "@tanstack/react-virtual";
import { File, FileAudio, FileImage, FileText, FileVideo, Folder } from "lucide-react";
import { Checkbox } from "@/components/table-editor/panel-bits";
import { formatBytes } from "@/lib/table-editor/dry-run";
import type { StorageItem } from "@/lib/storage-api";
import { cn } from "@/lib/utils";
import { baseName } from "./paths";
import { previewKind } from "./preview-kind";

export type ObjectView = "list" | "grid";

export interface ObjectListProps {
  bucket: string;
  items: StorageItem[];
  view: ObjectView;
  /** Checkboxes for files (only for roles that may delete). */
  selectable: boolean;
  selected: ReadonlySet<string>;
  focused: string | null;
  folderHref: (item: StorageItem) => string;
  onOpenFolder: (item: StorageItem) => void;
  onFocus: (item: StorageItem) => void;
  onToggle: (name: string, range: boolean) => void;
  onToggleAll: (all: boolean) => void;
  hasMore: boolean;
  loadingMore: boolean;
  onLoadMore: () => void;
}

const ROW = 36;
const TILE = 132;
const TILE_MIN_WIDTH = 168;

/** "3 Nov 2026, 14:05" from seconds since the epoch. */
export function updatedText(seconds: number | null): string {
  if (seconds === null) return "";
  return new Date(seconds * 1000).toLocaleString("en-GB", { day: "numeric", month: "short", year: "numeric", hour: "2-digit", minute: "2-digit" });
}

function ItemIcon({ item, size = 16 }: { item: StorageItem; size?: number }) {
  const props = { "aria-hidden": true, size, strokeWidth: 1.5, className: item.folder ? "shrink-0 text-sand" : "shrink-0 text-chalk-dim" } as const;
  if (item.folder) return <Folder {...props} />;
  switch (previewKind(item.contentType)) {
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
      return <File {...props} />;
  }
}

const plainClick = (e: MouseEvent) => !(e.metaKey || e.ctrlKey || e.shiftKey || e.button !== 0);

/** An item's name: a folder is a link into it, a file a button that opens its details. */
function ItemName({ item, href, onOpenFolder, onFocus, className }: { item: StorageItem; href: string; onOpenFolder: () => void; onFocus: () => void; className?: string }) {
  const label = baseName(item.name);
  const base = cn("min-w-0 truncate text-left outline-none focus-visible:outline-2 focus-visible:-outline-offset-2 focus-visible:outline-sand", className);
  if (item.folder)
    return (
      <a
        href={href}
        onClick={(e) => {
          if (!plainClick(e)) return;
          e.preventDefault();
          onOpenFolder();
        }}
        className={cn(base, "hover:underline underline-offset-4")}
      >
        {label}
      </a>
    );
  return (
    <button type="button" onClick={onFocus} className={cn(base, "hover:underline underline-offset-4")}>
      {label}
    </button>
  );
}

/**
 * A folder's items, virtualised (only what's on screen is mounted) as rows or tiles. Nearing the
 * end of what's loaded asks for the next page.
 */
export function ObjectList(props: ObjectListProps) {
  const { bucket, items, view, selectable, selected, focused, hasMore, loadingMore, onLoadMore } = props;
  const scrollRef = useRef<HTMLDivElement>(null);
  const [width, setWidth] = useState(900);
  useEffect(() => {
    const el = scrollRef.current;
    if (!el || typeof ResizeObserver === "undefined") return;
    const observer = new ResizeObserver(([entry]) => entry && setWidth(entry.contentRect.width));
    observer.observe(el);
    return () => observer.disconnect();
  }, []);
  const columns = view === "grid" ? Math.max(1, Math.floor(width / TILE_MIN_WIDTH)) : 1;
  const rowCount = Math.ceil(items.length / columns);
  const virtualizer = useVirtualizer({
    count: rowCount,
    getScrollElement: () => scrollRef.current,
    estimateSize: () => (view === "grid" ? TILE : ROW),
    overscan: 6,
    initialRect: { width: 900, height: 720 },
  });
  const virtualRows = virtualizer.getVirtualItems();
  const lastShown = virtualRows.at(-1)?.index ?? -1;
  useEffect(() => {
    if (hasMore && !loadingMore && lastShown >= rowCount - 4) onLoadMore();
  }, [hasMore, loadingMore, lastShown, rowCount, onLoadMore]);

  const files = items.filter((i) => !i.folder);
  const allSelected = files.length > 0 && files.every((f) => selected.has(f.name));
  const someSelected = files.some((f) => selected.has(f.name));

  const renderItem = (item: StorageItem) => {
    const isSelected = selected.has(item.name);
    const checkbox =
      selectable && !item.folder ? (
        <Checkbox
          aria-label={`Select ${baseName(item.name)}`}
          checked={isSelected}
          onChange={() => {}}
          onClick={(e) => props.onToggle(item.name, e.shiftKey)}
        />
      ) : (
        selectable && <span className="size-4 shrink-0" />
      );
    const name = (
      <ItemName
        item={item}
        href={props.folderHref(item)}
        onOpenFolder={() => props.onOpenFolder(item)}
        onFocus={() => props.onFocus(item)}
        className={view === "grid" ? "w-full text-center" : "flex-1"}
      />
    );
    if (view === "grid")
      return (
        <div
          className={cn(
            "relative flex h-full flex-col items-center justify-center gap-2 border border-line p-3 text-sm",
            focused === item.name && "border-sand",
            isSelected && "bg-chalk/[0.06]",
          )}
        >
          {checkbox && <span className="absolute top-2 left-2">{checkbox}</span>}
          <ItemIcon item={item} size={32} />
          {name}
          <span className="text-xs text-chalk-dim tabular-nums">{item.folder ? "Folder" : item.size === null ? "" : formatBytes(item.size)}</span>
        </div>
      );
    return (
      <div className={cn("flex h-full items-center gap-3 border-b border-line px-4 text-sm", focused === item.name && "bg-chalk/[0.04]", isSelected && "bg-chalk/[0.06]")}>
        {checkbox}
        <ItemIcon item={item} />
        {name}
        <span className="w-24 shrink-0 text-right text-xs text-chalk-dim tabular-nums">{item.folder || item.size === null ? "" : formatBytes(item.size)}</span>
        <span className="hidden w-40 shrink-0 text-right text-xs text-chalk-dim tabular-nums md:block">{updatedText(item.updatedAt)}</span>
      </div>
    );
  };

  return (
    <div className="flex min-h-0 flex-1 flex-col">
      {view === "list" && (
        <div className="flex h-8 shrink-0 items-center gap-3 border-b border-line px-4 text-xs text-chalk-dim">
          {selectable && (
            <Checkbox
              aria-label="Select all loaded files"
              checked={allSelected}
              ref={(el) => {
                if (el) el.indeterminate = someSelected && !allSelected;
              }}
              onChange={(e) => props.onToggleAll(e.target.checked)}
              disabled={files.length === 0}
            />
          )}
          <span className="w-4 shrink-0" />
          <span className="flex-1">Name</span>
          <span className="w-24 shrink-0 text-right">Size</span>
          <span className="hidden w-40 shrink-0 text-right md:block">Last modified</span>
        </div>
      )}
      <div ref={scrollRef} className="min-h-0 flex-1 overflow-y-auto">
        <ul aria-label={`Objects in ${bucket}`} className="relative" style={{ height: virtualizer.getTotalSize() }}>
          {virtualRows.map((row) => {
            const slice = items.slice(row.index * columns, row.index * columns + columns);
            if (view === "list") {
              const item = slice[0];
              return (
                <li key={item.name} className="absolute inset-x-0" style={{ top: row.start, height: row.size }}>
                  {renderItem(item)}
                </li>
              );
            }
            return slice.map((item, i) => (
              <li
                key={item.name}
                className="absolute p-1"
                style={{ top: row.start, height: row.size, left: `${(i / columns) * 100}%`, width: `${100 / columns}%` }}
              >
                {renderItem(item)}
              </li>
            ));
          })}
        </ul>
        {hasMore && (
          <div className="flex justify-center p-3">
            <button type="button" disabled={loadingMore} onClick={onLoadMore} className="text-sm text-sand underline-offset-4 hover:underline disabled:text-chalk-dim">
              {loadingMore ? "Loading…" : "Load more"}
            </button>
          </div>
        )}
      </div>
    </div>
  );
}
