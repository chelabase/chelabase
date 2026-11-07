import { useCallback, useEffect, useMemo, useReducer, useRef, useState, type MouseEvent } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { fetchStorageObject } from "@chelabase/management-client";
import { FolderUp, LayoutGrid, List, Search, Upload } from "lucide-react";
import { Button } from "@/components/ui/button";
import { LiveBadge } from "@/components/table-editor/live-badge";
import { Select } from "@/components/ui/select";
import { errorText } from "@/lib/errors";
import { invalidateStorage, publicObjectUrl, signObjectUrl, storageParams, type Bucket, type StorageItem } from "@/lib/storage-api";
import { cn } from "@/lib/utils";
import { consoleUploadApi } from "../upload/console-api";
import { inputFiles, type DroppedFile } from "../upload/drop-entries";
import { UploadTray } from "../upload/upload-tray";
import { useUploadQueue } from "../upload/use-upload-queue";
import { DropZone } from "./drop-zone";
import { useLiveObjects, viewIds } from "./live-objects";
import { DeleteObjectsDialog, RelocateDialog, RenameDialog, SignDialog, type ObjectPlace } from "./object-dialogs";
import { ObjectDetails } from "./object-details";
import { ObjectList, type ObjectView } from "./object-list";
import { baseName, crumbsFor, folderPathOf, parentPath, prefixOf } from "./paths";
import { EMPTY_SELECTION, selectionReducer, type DeleteReport } from "./selection";
import { useObjectPages, type ObjectOrder, type ObjectSort } from "./use-object-pages";

/** The list or grid choice, kept per browser. */
export const VIEW_KEY = "chelabase.studio.storage.view";

const SORTS: { value: `${ObjectSort}:${ObjectOrder}`; label: string }[] = [
  { value: "name:asc", label: "Name, A to Z" },
  { value: "name:desc", label: "Name, Z to A" },
  { value: "updated_at:desc", label: "Newest first" },
  { value: "updated_at:asc", label: "Oldest first" },
  { value: "size:desc", label: "Largest first" },
  { value: "size:asc", label: "Smallest first" },
];

/** Search waits this long after the last keystroke. */
const SEARCH_DELAY = 250;

function readView(): ObjectView {
  try {
    return window.localStorage.getItem(VIEW_KEY) === "grid" ? "grid" : "list";
  } catch {
    return "list";
  }
}
function writeView(view: ObjectView): void {
  try {
    window.localStorage.setItem(VIEW_KEY, view);
  } catch {
    // Not remembered; the toggle still works.
  }
}

export interface ObjectBrowserProps {
  databaseId: string;
  branchId: string;
  role: string | null;
  bucket: Bucket;
  /** The branch's main Postgres database (public URLs name it). */
  pgDatabase: string;
  /** The folder open (the route's splat). */
  path: string | undefined;
  /** Rename, move, copy, delete and signed URLs: the role held at Developer (owner mode is an Admin's). */
  canWrite: boolean;
  /** The API's base URL: signed and public URLs are on it. */
  apiUrl: string;
  hrefFor: (path: string | undefined) => string;
  onNavigate: (path: string | undefined) => void;
}

/**
 * A bucket's objects, a folder at a time: the breadcrumb follows the URL, search, sort and the
 * list/grid toggle sit above a virtualised listing paged by cursor, and a focused file opens in
 * the details panel with its preview and actions.
 */
export function ObjectBrowser({ databaseId, branchId, role, bucket, pgDatabase, path, canWrite, apiUrl, hrefFor, onNavigate }: ObjectBrowserProps) {
  const queryClient = useQueryClient();
  const prefix = prefixOf(path);
  const [view, setView] = useState<ObjectView>(readView);
  const [sort, setSort] = useState<`${ObjectSort}:${ObjectOrder}`>("name:asc");
  const [searchText, setSearchText] = useState("");
  const [search, setSearch] = useState("");
  const [selection, dispatch] = useReducer(selectionReducer, EMPTY_SELECTION);
  const [focused, setFocused] = useState<StorageItem | null>(null);
  const [dialog, setDialog] = useState<null | "delete" | "rename" | "move" | "copy" | "sign">(null);
  const [deleting, setDeleting] = useState<string[]>([]);
  const [report, setReport] = useState<DeleteReport | null>(null);

  useEffect(() => {
    const t = setTimeout(() => setSearch(searchText.trim()), SEARCH_DELAY);
    return () => clearTimeout(t);
  }, [searchText]);

  // A new folder starts with nothing selected or open.
  useEffect(() => {
    dispatch({ type: "clear" });
    setFocused(null);
    setReport(null);
  }, [prefix]);

  const [sortBy, order] = sort.split(":") as [ObjectSort, ObjectOrder];
  const query = useMemo(() => ({ prefix, sort: sortBy, order, search }), [prefix, sortBy, order, search]);
  const pages = useObjectPages(databaseId, branchId, role, bucket.name, query, true);
  const items = pages.items;
  const fileNames = useMemo(() => items.filter((i) => !i.folder).map((i) => i.name), [items]);
  useEffect(() => dispatch({ type: "keep", names: fileNames }), [fileNames]);
  // m17: other people's changes in this bucket, as the role sees them, read the listing again.
  const rowIds = useMemo(() => viewIds(items), [items]);
  const refetchPages = pages.refetch;
  const relist = useCallback(() => void refetchPages(), [refetchPages]);
  const liveStatus = useLiveObjects({ databaseId, branchId, pgDatabase, bucketId: bucket.id, role }, rowIds, relist);

  const place: ObjectPlace = { databaseId, branchId, role, bucket: bucket.name };
  const sign = canWrite ? (expiresIn: number) => signObjectUrl(apiUrl, databaseId, branchId, role, bucket.name, focused?.name ?? "", expiresIn) : null;
  const fetchObject = (range: string | undefined, signal?: AbortSignal) =>
    fetchStorageObject(databaseId, branchId, bucket.name, focused?.name ?? "", storageParams(role), { range, signal });
  const refresh = () => invalidateStorage(queryClient, databaseId, branchId);
  const uploadApi = useMemo(() => consoleUploadApi(databaseId, branchId, role), [databaseId, branchId, role]);
  const uploads = useUploadQueue({ api: uploadApi, onUploaded: () => void invalidateStorage(queryClient, databaseId, branchId) });
  const upload = (files: DroppedFile[]) => uploads.add(bucket.name, prefix, files);
  const fileInput = useRef<HTMLInputElement>(null);
  const folderInput = useRef<HTMLInputElement>(null);
  const loadMore = useCallback(() => void pages.loadMore(), [pages]);

  const selectedNames = [...selection.selected];
  function askDelete(names: string[]) {
    setDeleting(names);
    setDialog("delete");
  }
  function afterDelete(r: DeleteReport) {
    setReport(r);
    dispatch({ type: "keep", names: r.failed.map((f) => f.name) });
    if (focused && r.removed.includes(focused.name)) setFocused(null);
    void refresh();
  }
  function afterRelocate(to: string, keepOpen: boolean) {
    void refresh();
    if (keepOpen && focused && parentPath(to) === parentPath(focused.name)) setFocused({ ...focused, name: to });
    else if (!keepOpen) setFocused(null);
  }

  const crumbs = crumbsFor(bucket.name, path);
  const crumbClick = (target: string | undefined) => (event: MouseEvent<HTMLAnchorElement>) => {
    if (event.metaKey || event.ctrlKey || event.shiftKey || event.button !== 0) return;
    event.preventDefault();
    onNavigate(target);
  };

  let body;
  if (pages.error) {
    body = (
      <p role="alert" className="p-6 text-sm text-coral">
        {errorText(pages.error)}
      </p>
    );
  } else if (pages.loading) {
    body = (
      <p role="status" className="p-6 text-sm text-chalk-dim">
        Loading objects…
      </p>
    );
  } else if (items.length === 0) {
    body = (
      <p role="status" className="p-6 text-sm text-chalk-dim">
        {search !== "" ? `No objects match “${search}”.` : prefix === "" ? "This bucket is empty." : "This folder is empty."}
        {search === "" && canWrite && " Drop files or folders here to upload them."}
      </p>
    );
  } else {
    body = (
      <ObjectList
        bucket={bucket.name}
        items={items}
        view={view}
        selectable={canWrite}
        selected={selection.selected}
        focused={focused?.name ?? null}
        folderHref={(item) => hrefFor(folderPathOf(item.name))}
        onOpenFolder={(item) => onNavigate(folderPathOf(item.name))}
        onFocus={setFocused}
        onToggle={(name, range) => dispatch(range ? { type: "range", name, order: fileNames } : { type: "toggle", name })}
        onToggleAll={(all) => dispatch(all ? { type: "all", names: fileNames } : { type: "clear" })}
        hasMore={pages.hasMore}
        loadingMore={pages.loadingMore}
        onLoadMore={loadMore}
      />
    );
  }

  return (
    <div className="flex min-h-0 flex-1 flex-col border-t border-line">
      <div className="flex shrink-0 flex-wrap items-center gap-2 border-b border-line px-4 py-2">
        <nav aria-label="Folder" className="min-w-0 flex-1">
          <ol className="flex min-w-0 flex-wrap items-center gap-1 font-mono text-sm">
            {crumbs.map((c, i) => {
              const last = i === crumbs.length - 1;
              return (
                <li key={c.path ?? ""} className="flex min-w-0 items-center gap-1">
                  {i > 0 && (
                    <span aria-hidden="true" className="text-chalk-dim">
                      /
                    </span>
                  )}
                  <a
                    href={hrefFor(c.path)}
                    onClick={crumbClick(c.path)}
                    aria-current={last ? "page" : undefined}
                    className={cn("truncate underline-offset-4 hover:underline", last ? "text-chalk" : "text-chalk-dim")}
                  >
                    {c.label}
                  </a>
                </li>
              );
            })}
          </ol>
        </nav>
        {liveStatus && <LiveBadge status={liveStatus} />}
        <label className="relative flex items-center">
          <span className="sr-only">Search objects</span>
          <Search aria-hidden="true" size={14} strokeWidth={1.5} className="pointer-events-none absolute left-2 text-chalk-dim" />
          <input
            type="search"
            value={searchText}
            onChange={(e) => setSearchText(e.target.value)}
            placeholder="Search names"
            className="h-8 w-48 rounded-none border border-input bg-ink pr-2 pl-7 text-sm outline-none focus-visible:outline-2 focus-visible:-outline-offset-1 focus-visible:outline-sand"
          />
        </label>
        <label className="flex items-center gap-2 text-sm">
          <span className="sr-only">Sort</span>
          <Select value={sort} onChange={(e) => setSort(e.target.value as typeof sort)} wrapperClassName="w-40">
            {SORTS.map((s) => (
              <option key={s.value} value={s.value}>
                {s.label}
              </option>
            ))}
          </Select>
        </label>
        {canWrite && (
          <div className="flex gap-1">
            <Button size="sm" variant="outline" onClick={() => fileInput.current?.click()}>
              <Upload aria-hidden="true" strokeWidth={1.5} />
              Upload
            </Button>
            <Button size="sm" variant="outline" onClick={() => folderInput.current?.click()}>
              <FolderUp aria-hidden="true" strokeWidth={1.5} />
              Upload folder
            </Button>
            <input
              ref={fileInput}
              type="file"
              multiple
              hidden
              aria-label="Files to upload"
              onChange={(e) => {
                upload(inputFiles(e.target.files ?? []));
                e.target.value = "";
              }}
            />
            <input
              ref={folderInput}
              type="file"
              hidden
              aria-label="Folder to upload"
              {...{ webkitdirectory: "" }}
              onChange={(e) => {
                upload(inputFiles(e.target.files ?? []));
                e.target.value = "";
              }}
            />
          </div>
        )}
        <div className="flex">
          {(["list", "grid"] as const).map((v) => (
            <Button
              key={v}
              variant="ghost"
              size="icon-sm"
              aria-label={v === "list" ? "List view" : "Grid view"}
              aria-pressed={view === v}
              className={cn(view === v && "bg-chalk/[0.08] text-sand")}
              onClick={() => {
                setView(v);
                writeView(v);
              }}
            >
              {v === "list" ? <List aria-hidden="true" strokeWidth={1.5} /> : <LayoutGrid aria-hidden="true" strokeWidth={1.5} />}
            </Button>
          ))}
        </div>
      </div>
      {canWrite && selectedNames.length > 0 && (
        <div className="flex shrink-0 items-center gap-2 border-b border-line bg-chalk/[0.03] px-4 py-1.5 text-sm">
          <span className="text-chalk-dim">{selectedNames.length} selected</span>
          <span className="flex-1" />
          <Button size="sm" variant="ghost" onClick={() => dispatch({ type: "clear" })}>
            Clear selection
          </Button>
          <Button size="sm" variant="destructive" onClick={() => askDelete(selectedNames)}>
            Delete {selectedNames.length} {selectedNames.length === 1 ? "object" : "objects"}
          </Button>
        </div>
      )}
      {report && (
        <div role={report.failed.length > 0 ? "alert" : "status"} className="shrink-0 space-y-1 border-b border-line px-4 py-2 text-sm">
          <div className="flex items-center gap-2">
            <p className={cn("flex-1", report.failed.length > 0 && "text-coral")}>{report.summary}</p>
            <Button size="xs" variant="ghost" onClick={() => setReport(null)}>
              Dismiss
            </Button>
          </div>
          {report.failed.length > 0 && (
            <ul className="space-y-0.5 text-xs">
              {report.failed.map((f) => (
                <li key={f.name}>
                  <span className="font-mono">{baseName(f.name)}</span>: {f.message}
                </li>
              ))}
            </ul>
          )}
        </div>
      )}
      <DropZone canWrite={canWrite} onFiles={upload} className="flex min-h-0 flex-1">
        <div className="flex min-w-0 flex-1 flex-col">{body}</div>
        {focused && (
          <ObjectDetails
            key={focused.name}
            item={focused}
            publicUrl={bucket.public ? publicObjectUrl(apiUrl, branchId, pgDatabase, bucket.name, focused.name) : null}
            sign={sign}
            fetchObject={fetchObject}
            canWrite={canWrite}
            onSignUrl={() => setDialog("sign")}
            onRename={() => setDialog("rename")}
            onMove={() => setDialog("move")}
            onCopy={() => setDialog("copy")}
            onDelete={() => askDelete([focused.name])}
            onClose={() => setFocused(null)}
          />
        )}
      </DropZone>
      <UploadTray queue={uploads} writeOnce={bucket.write_once} />
      {canWrite && (
        <>
          <DeleteObjectsDialog open={dialog === "delete"} onOpenChange={(o) => !o && setDialog(null)} place={place} names={deleting} onDone={afterDelete} />
          {focused && (
            <>
              <RenameDialog open={dialog === "rename"} onOpenChange={(o) => !o && setDialog(null)} place={place} name={focused.name} onDone={(to) => afterRelocate(to, true)} />
              <RelocateDialog open={dialog === "move"} onOpenChange={(o) => !o && setDialog(null)} place={place} name={focused.name} mode="move" onDone={(to) => afterRelocate(to, false)} />
              <RelocateDialog open={dialog === "copy"} onOpenChange={(o) => !o && setDialog(null)} place={place} name={focused.name} mode="copy" onDone={() => void refresh()} />
              {sign && <SignDialog open={dialog === "sign"} onOpenChange={(o) => !o && setDialog(null)} name={focused.name} sign={sign} />}
            </>
          )}
        </>
      )}
    </div>
  );
}
