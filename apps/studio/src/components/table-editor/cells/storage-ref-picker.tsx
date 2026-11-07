import { useCallback, useMemo, useState } from "react";
import { ObjectList } from "@/components/storage/browser/object-list";
import { parentPath, prefixOf } from "@/components/storage/browser/paths";
import { useObjectPages } from "@/components/storage/browser/use-object-pages";
import { errorText } from "@/lib/errors";
import type { RefTarget } from "./storage-ref";

export interface StorageRefPickerProps {
  databaseId: string;
  branchId: string;
  database: string | null;
  role: string | null;
  bucket: string;
  /** The bucket's id (a `storage.object_ref` holds it); null when unknown. */
  bucketId: string | null;
  onPick: (target: RefTarget) => void;
}

const NONE: ReadonlySet<string> = new Set();

/** The Storage browser's listing of one bucket as a file picker: folders open, a file click picks it. */
export function StorageRefPicker({ databaseId, branchId, database, role, bucket, bucketId, onPick }: StorageRefPickerProps) {
  const [prefix, setPrefix] = useState("");
  const query = useMemo(() => ({ prefix, sort: "name" as const, order: "asc" as const, search: "" }), [prefix]);
  const pages = useObjectPages(databaseId, branchId, role, bucket, query, true, database);
  const loadMore = useCallback(() => void pages.loadMore(), [pages]);

  return (
    <div className="flex h-72 flex-col">
      <div className="flex h-8 shrink-0 items-center gap-2 border-b border-line px-3 font-mono text-xs text-chalk-dim">
        {prefix !== "" && (
          <button type="button" className="text-chalk hover:text-sand" onClick={() => setPrefix(prefixOf(parentPath(prefix)))}>
            ..
          </button>
        )}
        <span className="truncate">
          {bucket}/{prefix}
        </span>
      </div>
      {pages.error ? (
        <p role="alert" className="p-3 text-sm text-coral">
          {errorText(pages.error)}
        </p>
      ) : pages.loading ? (
        <p className="p-3 text-sm text-chalk-dim">Loading files…</p>
      ) : pages.items.length === 0 ? (
        <p className="p-3 text-sm text-chalk-dim">No files here.</p>
      ) : (
        <ObjectList
          bucket={bucket}
          items={pages.items}
          view="list"
          selectable={false}
          selected={NONE}
          focused={null}
          folderHref={() => "#"}
          onOpenFolder={(item) => setPrefix(item.name)}
          onFocus={(item) => onPick({ bucketId, bucket, name: item.name, id: item.id })}
          onToggle={() => {}}
          onToggleAll={() => {}}
          hasMore={pages.hasMore}
          loadingMore={pages.loadingMore}
          onLoadMore={loadMore}
        />
      )}
    </div>
  );
}
