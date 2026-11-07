import { useContext, useMemo, useState, type ReactNode } from "react";
import { fetchStorageObject } from "@chelabase/management-client";
import type { PreviewSource } from "@/components/storage/browser/object-preview";
import { consoleUploadApi } from "@/components/storage/upload/console-api";
import { useProjectCaps } from "@/lib/capabilities";
import { EnvContext } from "@/lib/env-context";
import { storageParams, useBuckets } from "@/lib/storage-api";
import type { Row, RowColumn } from "@/lib/table-editor/rows";
import type { CellValue } from "../cell-editor";
import type { TableTarget } from "../target";
import { refRequestOf, type RefRequest } from "./storage-ref";
import { StorageRefCell, StorageRefEditor } from "./storage-ref-cell";
import { StorageRefPicker } from "./storage-ref-picker";
import { cellUploader, objectIdOf } from "./storage-ref-upload";
import { useStorageRefs, type RefState } from "./use-storage-refs";

export interface StorageRefCellsOptions {
  target: TableTarget;
  columns: RowColumn[];
  rows: Row[];
  keys: (string | null)[];
  canEdit: (column: string, key: string) => boolean;
  /** The Act as role is held at Developer or above: signed URLs open for it. */
  canSign: boolean;
  /** Saves one cell (the grid's save, with its conflict check). */
  save: (key: string, column: RowColumn, value: CellValue) => Promise<string | null>;
}

/** A ref column: its cells are files. */
export const isRefColumn = (c: RowColumn) => !!c.desc.storage_ref && !c.desc.array;

/**
 * The table editor's ref cells for the visible page: one resolve for every ref on it, the cell
 * renderer, and the one editor dialog (preview, upload, pick, Clear).
 */
export function useStorageRefCells({ target, columns, rows, keys, canEdit, canSign, save }: StorageRefCellsOptions) {
  const { databaseId, branchId, database, role } = target;
  const apiUrl = useContext(EnvContext)?.apiUrl ?? "";
  const admin = useProjectCaps().atLeast("admin");
  const refColumns = useMemo(() => columns.filter(isRefColumn), [columns]);
  const requests = useMemo(() => {
    const out: RefRequest[] = [];
    for (const row of rows) {
      for (const c of refColumns) {
        const r = refRequestOf(c.desc.storage_ref!, row[c.name]);
        if (r) out.push(r);
      }
    }
    return out;
  }, [rows, refColumns]);
  const refs = useStorageRefs({ databaseId, branchId, database, role, apiUrl, admin }, requests, refColumns.length > 0);

  // A ref or id column picks its bucket: the role's buckets (and their ids) for a writer.
  const needBuckets = canSign && refColumns.some((c) => c.desc.storage_ref!.kind !== "path");
  const buckets = useBuckets(databaseId, branchId, role, needBuckets, database);
  const bucketList = useMemo(() => buckets.data ?? [], [buckets.data]);
  const upload = useMemo(
    () =>
      cellUploader({
        api: consoleUploadApi(databaseId, branchId, role, database),
        bucketIdOf: (name) => bucketList.find((b) => b.name === name)?.id ?? null,
        idOf: (bucket, name) => objectIdOf(databaseId, branchId, role, database, bucket, name),
      }),
    [databaseId, branchId, role, database, bucketList],
  );

  const [editing, setEditing] = useState<{ key: string; column: string; file: File | null } | null>(null);

  const stateOf = (c: RowColumn, value: unknown): RefState | undefined => {
    const r = refRequestOf(c.desc.storage_ref!, value);
    return r ? refs.get(r) : undefined;
  };

  function render(row: Row, c: RowColumn, i: number): ReactNode | undefined {
    if (!isRefColumn(c)) return undefined;
    const key = keys[i];
    const writable = key !== null && key !== undefined && canEdit(c.name, key);
    return (
      <StorageRefCell
        desc={c.desc.storage_ref!}
        value={row[c.name]}
        resolved={stateOf(c, row[c.name])}
        canEdit={writable}
        canSign={canSign}
        upload={upload}
        save={(value) => (key ? save(key, c, value) : Promise.resolve("That row has no key."))}
        onOpen={(file) => key && setEditing({ key, column: c.name, file: file ?? null })}
      />
    );
  }

  let dialog: ReactNode = null;
  const column = editing ? refColumns.find((c) => c.name === editing.column) : undefined;
  const at = editing ? keys.indexOf(editing.key) : -1;
  if (editing && column && at >= 0) {
    const value = rows[at]?.[column.name];
    const resolved = stateOf(column, value);
    let previewSource: PreviewSource | null = null;
    if (resolved?.state === "ok") {
      const { url, bucket, name } = resolved;
      // A Viewer's signed URLs 403: its preview reads the bytes through the console download route.
      previewSource =
        canSign || resolved.public
          ? { kind: "signed", sign: async () => url }
          : { kind: "download", fetch: (range, signal) => fetchStorageObject(databaseId, branchId, bucket, name, storageParams(role, database), { range, signal }) };
    }
    const desc = column.desc.storage_ref!;
    dialog = (
      <StorageRefEditor
        open
        onClose={() => setEditing(null)}
        columnName={column.name}
        desc={desc}
        nullable={column.desc.nullable}
        value={value}
        resolved={resolved}
        canEdit={canEdit(column.name, editing.key)}
        previewSource={previewSource}
        buckets={bucketList}
        pendingFile={editing.file}
        upload={upload}
        save={(v) => save(editing.key, column, v)}
        renderPicker={(bucket, onPick) => (
          <StorageRefPicker
            key={bucket}
            databaseId={databaseId}
            branchId={branchId}
            database={database}
            role={role}
            bucket={bucket}
            bucketId={bucketList.find((b) => b.name === bucket)?.id ?? null}
            onPick={onPick}
          />
        )}
      />
    );
  }

  return { render, dialog, error: refs.error };
}
