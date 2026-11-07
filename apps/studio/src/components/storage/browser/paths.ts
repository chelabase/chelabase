/**
 * Folders in a bucket are only name prefixes: the route's splat (`photos/2024`) is the folder,
 * listed with the prefix `photos/2024/` and the delimiter `/`.
 */

/** The listing prefix of a folder path (the bucket's root is ""). */
export function prefixOf(path: string | undefined): string {
  const trimmed = (path ?? "").replace(/^\/+|\/+$/g, "");
  return trimmed === "" ? "" : `${trimmed}/`;
}

/** The folder path a folder item (`photos/2024/`) opens. */
export function folderPathOf(name: string): string {
  return name.replace(/\/+$/, "");
}

/** The last segment of a key: what a row shows inside its folder. */
export function baseName(name: string): string {
  const trimmed = name.replace(/\/+$/, "");
  return trimmed.slice(trimmed.lastIndexOf("/") + 1);
}

/** The folder a key is in, as a path ("" at the root). */
export function parentPath(name: string): string {
  const trimmed = name.replace(/\/+$/, "");
  const at = trimmed.lastIndexOf("/");
  return at < 0 ? "" : trimmed.slice(0, at);
}

export interface Crumb {
  label: string;
  /** The folder path it opens; undefined for the bucket's root. */
  path: string | undefined;
}

/** The breadcrumb of a folder: the bucket, then each folder down to this one. */
export function crumbsFor(bucket: string, path: string | undefined): Crumb[] {
  const crumbs: Crumb[] = [{ label: bucket, path: undefined }];
  const segments = folderPathOf((path ?? "").replace(/^\/+/, "")).split("/").filter(Boolean);
  segments.forEach((label, i) => crumbs.push({ label, path: segments.slice(0, i + 1).join("/") }));
  return crumbs;
}

/** A folder's URL under its bucket's URL, each segment encoded. */
export function folderHref(bucketHref: string, path: string | undefined): string {
  const segments = (path ?? "").split("/").filter(Boolean);
  return segments.length === 0 ? bucketHref : `${bucketHref}/${segments.map(encodeURIComponent).join("/")}`;
}

/** A key from a folder path and a name. */
export function joinKey(folder: string, name: string): string {
  const prefix = prefixOf(folder);
  return `${prefix}${name}`;
}

/** Why a new name (one segment: rename, copy) won't do, or null. Storage checks the rest. */
export function nameProblem(name: string): string | null {
  if (name.trim() === "") return "Enter a name.";
  if (name.includes("/")) return "A name can't contain /; use Move to change its folder.";
  if (name.includes("\\")) return "A name can't contain \\.";
  if (name === "." || name === "..") return "A name can't be . or ..";
  return null;
}

/** Why a folder path won't do, or null. Empty is the bucket's root. */
export function folderProblem(path: string): string | null {
  const segments = path.split("/").filter(Boolean);
  if (path.includes("\\")) return "A folder can't contain \\.";
  if (segments.some((s) => s === "." || s === "..")) return "A folder can't contain . or .. segments.";
  return null;
}
