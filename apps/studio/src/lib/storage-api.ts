import { useQuery, type QueryClient } from "@tanstack/react-query";
import {
  ApiError,
  getStorageListBucketsQueryKey,
  storageListObjects,
  storageSignObject,
  useStorageListBuckets,
  type StorageListBucketsParams,
} from "@chelabase/management-client";
import { errorText } from "./errors";

/**
 * Studio's side of the storage console routes (`/v1/databases/{id}/branches/{branch}/storage/…`):
 * they run as the Act as role, or as the owner (owner mode, an Admin's) when no role is sent. The
 * generated responses are untyped (storage's own JSON), so the shapes are read here.
 */

/** A bucket as `storage.buckets` holds it. */
export interface Bucket {
  id: string;
  name: string;
  public: boolean;
  file_size_limit: number | null;
  allowed_mime_types: string[] | null;
  write_once: boolean;
  created_at?: string;
}

/** What one listing page says about a bucket's objects, as the role sees them. */
export interface BucketStats {
  objects: number;
  bytes: number;
  /** The listing had more than one page: the numbers are a lower bound. */
  more: boolean;
}

/** Objects read per bucket for its count and size (storage's largest page). */
export const STATS_PAGE = 1000;

/** The console's role parameter (none is owner mode), and the Postgres database when it isn't the branch's main one. */
export function storageParams(role: string | null, database: string | null = null): StorageListBucketsParams {
  return { ...(role === null ? {} : { role }), ...(database === null ? {} : { database }) };
}

/** Every storage query of a branch starts with this. */
function branchPrefix(databaseId: string, branchId: string): readonly unknown[] {
  return getStorageListBucketsQueryKey(databaseId, branchId).slice(0, 1);
}

/** The bucket list's key: per branch and per Act as role, so switching either reads it again. */
export function bucketsKey(databaseId: string, branchId: string, role: string | null) {
  return getStorageListBucketsQueryKey(databaseId, branchId, storageParams(role));
}

/** A bucket's count and size: per branch, role and bucket, under the branch's prefix. */
export function bucketStatsKey(databaseId: string, branchId: string, role: string | null, bucket: string): readonly unknown[] {
  return [...branchPrefix(databaseId, branchId), "stats", bucket, storageParams(role)];
}

/** After a bucket is created, changed or deleted: every storage query of the branch, for every role. */
export function invalidateStorage(queryClient: QueryClient, databaseId: string, branchId: string): Promise<void> {
  return queryClient.invalidateQueries({ queryKey: branchPrefix(databaseId, branchId) });
}

const isObject = (v: unknown): v is Record<string, unknown> => typeof v === "object" && v !== null && !Array.isArray(v);

function isBucket(v: unknown): v is Bucket {
  return (
    isObject(v) &&
    typeof v.id === "string" &&
    typeof v.name === "string" &&
    typeof v.public === "boolean" &&
    (v.file_size_limit === null || v.file_size_limit === undefined || typeof v.file_size_limit === "number") &&
    (v.allowed_mime_types === null || v.allowed_mime_types === undefined || (Array.isArray(v.allowed_mime_types) && v.allowed_mime_types.every((t) => typeof t === "string")))
  );
}

/** The well-formed buckets of a list answer. */
export function parseBuckets(body: unknown): Bucket[] {
  if (!Array.isArray(body)) return [];
  return body.filter(isBucket).map((b) => ({ ...b, file_size_limit: b.file_size_limit ?? null, allowed_mime_types: b.allowed_mime_types ?? null, write_once: b.write_once === true }));
}

/** A listing page's files counted and their sizes summed (folders aren't objects). */
export function summarizeObjects(body: unknown): BucketStats {
  if (!isObject(body) || !Array.isArray(body.items)) return { objects: 0, bytes: 0, more: false };
  let objects = 0;
  let bytes = 0;
  for (const item of body.items) {
    if (!isObject(item) || item.folder === true) continue;
    objects++;
    if (typeof item.size === "number") bytes += item.size;
  }
  return { objects, bytes, more: typeof body.next_cursor === "string" && body.next_cursor !== "" };
}

/** "1 object", "3 objects", "1,000+ objects". */
export function objectCountText(s: BucketStats): string {
  const n = `${s.objects.toLocaleString("en-US")}${s.more ? "+" : ""}`;
  return `${n} ${s.objects === 1 && !s.more ? "object" : "objects"}`;
}

/**
 * The answers that mean storage can't run here rather than a failure: `data_api` (the Postgres
 * database isn't served by the data API, so it has no storage schema) and `unavailable` (the
 * storage schema isn't ready).
 */
export function storageProblem(err: unknown): "data_api" | "unavailable" | null {
  if (!(err instanceof ApiError)) return null;
  const code = err.code as string;
  if (code === "data_api_disabled") return "data_api";
  if (code === "storage_unavailable") return "unavailable";
  return null;
}

/** Storage refused a delete because the bucket still holds objects. */
export function isBucketNotEmpty(err: unknown): boolean {
  return err instanceof ApiError && (err.code as string) === "bucket_not_empty";
}

/** The buckets the role sees on a branch (the main Postgres database's). */
export function useBuckets(databaseId: string, branchId: string, role: string | null, enabled: boolean, database: string | null = null) {
  return useStorageListBuckets(databaseId, branchId, storageParams(role, database), {
    query: { enabled, select: parseBuckets, retry: false },
  });
}

/** A bucket's object count and size, from one listing page as the role. */
export function useBucketStats(databaseId: string, branchId: string, role: string | null, bucket: string, enabled: boolean) {
  return useQuery({
    queryKey: bucketStatsKey(databaseId, branchId, role, bucket),
    queryFn: async ({ signal }) =>
      summarizeObjects(await storageListObjects(databaseId, branchId, bucket, { prefix: "", limit: STATS_PAGE }, storageParams(role), { signal })),
    enabled,
    retry: false,
  });
}

/** One entry of a delimiter listing: a file, or a folder (`name` ends with `/`, no size). Names are full keys. */
export interface StorageItem {
  name: string;
  folder: boolean;
  size: number | null;
  etag: string | null;
  contentType: string | null;
  /** Seconds since the epoch. */
  updatedAt: number | null;
  /** A file's object id (m17: live view keys); null for a folder, or when storage didn't send it. */
  id: string | null;
}

/** A listing page as the browser reads it. */
export interface ObjectPage {
  items: StorageItem[];
  nextCursor: string | null;
}

const strOrNull = (v: unknown): string | null => (typeof v === "string" ? v : null);
const numOrNull = (v: unknown): number | null => (typeof v === "number" && Number.isFinite(v) ? v : null);

/** A listing answer's well-formed items and its cursor. */
export function parseObjectPage(body: unknown): ObjectPage {
  if (!isObject(body) || !Array.isArray(body.items)) return { items: [], nextCursor: null };
  const items: StorageItem[] = [];
  for (const item of body.items) {
    if (!isObject(item) || typeof item.name !== "string" || item.name === "") continue;
    items.push({
      name: item.name,
      folder: item.folder === true,
      size: numOrNull(item.size),
      etag: strOrNull(item.etag),
      contentType: strOrNull(item.content_type),
      updatedAt: numOrNull(item.updated_at),
      id: item.folder === true ? null : strOrNull(item.id),
    });
  }
  const next = typeof body.next_cursor === "string" && body.next_cursor !== "" ? body.next_cursor : null;
  return { items, nextCursor: next };
}

/** What a browser listing asks for besides the place. */
export interface ObjectListQuery {
  prefix: string;
  sort: string;
  order: string;
  search: string;
}

/** A folder's listing: per branch, role, bucket, folder, sort and search, under the branch's prefix. */
export function objectsKey(databaseId: string, branchId: string, role: string | null, bucket: string, q: ObjectListQuery, database: string | null = null): readonly unknown[] {
  return [...branchPrefix(databaseId, branchId), "objects", bucket, q, storageParams(role, database)];
}

/**
 * Why an object action failed, for a person: write-once buckets and rows that reference the
 * object get their own explanation; anything else (a taken name, a bad type) is storage's message.
 */
export function objectActionError(err: unknown): string {
  if (err instanceof ApiError) {
    const code = err.code as string;
    if (code === "write_once") return "The bucket is write-once: its files can't be overwritten, moved or deleted.";
    if (code === "referenced") return `A table row still references this file; clear the reference first. ${err.message}`.trim();
    if (code === "delete_refused") return "A table that references this file refused the delete (a constraint or trigger on it); change that table first.";
  }
  return errorText(err);
}

/** A path on the API host (a signed URL's `signed_url`) as a full URL. */
export function apiHref(apiUrl: string, path: string): string {
  return `${apiUrl.replace(/\/+$/, "")}${path}`;
}

/** A sign answer: the path on the API host and when it stops working. */
export function parseSigned(body: unknown): { path: string; expiresAt: number | null } | null {
  if (!isObject(body) || typeof body.signed_url !== "string" || !body.signed_url.startsWith("/")) return null;
  return { path: body.signed_url, expiresAt: numOrNull(body.expires_at) };
}

/** Each `/`-separated segment percent-encoded, as storage builds its own URLs. */
export function encodeKey(name: string): string {
  return name.split("/").map(encodeURIComponent).join("/");
}

/** A public bucket's object URL: no key needed. */
export function publicObjectUrl(apiUrl: string, branchId: string, pgDatabase: string, bucket: string, name: string): string {
  return apiHref(apiUrl, `/storage/v1/object/public/${encodeURIComponent(branchId)}/${encodeURIComponent(bucket)}/${encodeKey(name)}?database=${encodeURIComponent(pgDatabase)}`);
}

/** Console signed URLs live at most an hour. */
export const MAX_SIGNED_TTL = 3600;

/** Mints a signed download URL as the role (Developer), as a full URL on the API host. */
export async function signObjectUrl(
  apiUrl: string,
  databaseId: string,
  branchId: string,
  role: string | null,
  bucket: string,
  name: string,
  expiresIn: number,
): Promise<{ url: string; expiresAt: number | null }> {
  const signed = parseSigned(await storageSignObject(databaseId, branchId, bucket, name, { expires_in: Math.min(expiresIn, MAX_SIGNED_TTL) }, storageParams(role)));
  if (!signed) throw new Error("Storage answered without a signed URL");
  return { url: apiHref(apiUrl, signed.path), expiresAt: signed.expiresAt };
}
