import { ApiError, storageListObjects } from "@chelabase/management-client";
import { runUpload, type UploadApi } from "@/components/storage/upload/runner";
import { parseObjectPage, storageParams } from "@/lib/storage-api";
import type { RefTarget } from "./storage-ref";

/**
 * Uploading into a cell, with m14's runner over the console routes: the file goes to the bucket's
 * top level under its own name; a taken name is never overwritten but kept as `name (n).ext`
 * (Keep both). What comes back is everything a ref column could hold: the bucket's id (from the
 * bucket list) and, for a foreign key column, the object's id.
 */
export interface CellUploaderOptions {
  api: UploadApi;
  /** The id of a bucket by its name, from the bucket list; null when unknown. */
  bucketIdOf: (bucket: string) => string | null;
  /** The object's id, read after the upload (only a foreign key column needs it). */
  idOf: (bucket: string, name: string) => Promise<string | null>;
}

export type CellUpload = (bucket: string, file: File, o?: { needId?: boolean; signal?: AbortSignal }) => Promise<RefTarget>;

const isConflict = (e: unknown) => e instanceof ApiError && (e.code as string) === "conflict";

export function cellUploader({ api, bucketIdOf, idOf }: CellUploaderOptions): CellUpload {
  return async (bucket, file, o = {}) => {
    const signal = o.signal ?? new AbortController().signal;
    const hooks = { signal, onProgress: () => {}, onSession: () => {} };
    const job = { bucket, key: file.name, name: file.name, file, session: null };
    let name: string;
    try {
      ({ name } = await runUpload(api, { ...job, choice: null }, hooks));
    } catch (e) {
      if (!isConflict(e)) throw e;
      ({ name } = await runUpload(api, { ...job, choice: "keep-both" }, hooks));
    }
    return { bucket, name, bucketId: bucketIdOf(bucket), id: o.needId ? await idOf(bucket, name) : null };
  };
}

/** An object's id by its name, from a listing of its folder as the role. */
export async function objectIdOf(databaseId: string, branchId: string, role: string | null, database: string | null, bucket: string, name: string): Promise<string | null> {
  const page = parseObjectPage(await storageListObjects(databaseId, branchId, bucket, { prefix: name, delimiter: "/", limit: 100 }, storageParams(role, database)));
  return page.items.find((i) => !i.folder && i.name === name)?.id ?? null;
}
