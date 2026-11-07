import type { Column, StorageRefDesc, Table, TypeEntry } from "@chelabase/management-client";
import type { CellValue } from "../cell-editor";

/**
 * The table editor's object references (m11's three `storage_ref` forms), read from the table's
 * own description (`getTable`) as the data API schema derives its `storage_ref` hint
 * (`crates/chelabase-query/src/schema.rs`): a single-column foreign key to `storage.objects(id)`
 * first (an object id), then the composite `storage.object_ref` (a bucket id and a name), then a
 * text column whose comment says `@storage_ref bucket=<name>` (a name in that bucket). Arrays
 * aren't cells of one ref: they stay JSON.
 */

/** The bucket a column comment's `@storage_ref bucket=<name>` names (as `storage_bucket_of`). */
export function storageBucketOf(comment: string | null | undefined): string | null {
  if (!comment) return null;
  const words = comment.split(/\s+/).filter((w) => w !== "");
  const at = words.indexOf("@storage_ref");
  if (at < 0) return null;
  const next = words[at + 1];
  if (!next?.startsWith("bucket=")) return null;
  const bucket = next.slice("bucket=".length);
  return bucket === "" ? null : bucket;
}

const TEXT_TYPE = /^(text|citext|name|character varying|varchar|character|char|bpchar)(\(\d+\))?$/i;
/** `FOREIGN KEY (c) REFERENCES storage.objects(id)`, as `pg_get_constraintdef` writes it. */
const OBJECTS_ID = /REFERENCES\s+storage\.objects\s*\(\s*id\s*\)/i;

/** How `column` references a stored object, or null when it doesn't (or is an array). */
export function storageRefOf(column: Column, table: Table, type: TypeEntry | undefined): StorageRefDesc | null {
  if (column.data_type.endsWith("[]")) return null;
  const fk = table.constraints.some(
    (c) =>
      c.kind === "foreign_key" &&
      c.columns.length === 1 &&
      c.columns[0] === column.attnum &&
      c.references?.schema === "storage" &&
      c.references.table === "objects" &&
      c.references.columns.length === 1 &&
      OBJECTS_ID.test(c.definition),
  );
  if (fk) return { kind: "id" };
  const isRef = type ? type.schema === "storage" && type.name === "object_ref" : column.data_type === "storage.object_ref";
  if (isRef) return { kind: "ref" };
  const isText = type ? type.category === "S" : TEXT_TYPE.test(column.data_type.trim());
  if (!isText) return null;
  const bucket = storageBucketOf(column.comment);
  return bucket === null ? null : { kind: "path", bucket };
}

/** One ref as the resolve route takes it. */
export type RefRequest = { bucket_id: string; name: string } | { bucket: string; name: string } | { id: string };

const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;

/** A record literal's fields (`(a,"b ""c""")`): quoted fields with `""` and `\` escapes; null for a malformed one. */
function recordFields(text: string): (string | null)[] | null {
  const t = text.trim();
  if (!t.startsWith("(") || !t.endsWith(")")) return null;
  const body = t.slice(1, -1);
  const out: (string | null)[] = [];
  let i = 0;
  for (;;) {
    let field = "";
    let quoted = false;
    while (i < body.length && body[i] !== ",") {
      const ch = body[i];
      if (ch === '"') {
        quoted = true;
        i++;
        for (;;) {
          if (i >= body.length) return null;
          if (body[i] === "\\") {
            if (i + 1 >= body.length) return null;
            field += body[i + 1];
            i += 2;
          } else if (body[i] === '"') {
            if (body[i + 1] === '"') {
              field += '"';
              i += 2;
            } else {
              i++;
              break;
            }
          } else field += body[i++];
        }
      } else if (ch === "\\") {
        if (i + 1 >= body.length) return null;
        field += body[i + 1];
        i += 2;
      } else field += body[i++];
    }
    out.push(field === "" && !quoted ? null : field);
    if (i >= body.length) return out;
    i++;
  }
}

const isObject = (v: unknown): v is Record<string, unknown> => typeof v === "object" && v !== null && !Array.isArray(v);

/** The ref a cell's value names, or null for NULL or a value that names none. */
export function refRequestOf(desc: StorageRefDesc, value: unknown): RefRequest | null {
  if (value === null || value === undefined) return null;
  switch (desc.kind) {
    case "id":
      return typeof value === "string" && UUID.test(value.trim()) ? { id: value.trim().toLowerCase() } : null;
    case "path":
      return typeof value === "string" && value !== "" ? { bucket: desc.bucket, name: value } : null;
    case "ref": {
      let bucketId: unknown;
      let name: unknown;
      if (isObject(value)) {
        bucketId = value.bucket_id;
        name = value.name;
      } else if (typeof value === "string") {
        const fields = recordFields(value);
        if (!fields || fields.length !== 2) return null;
        [bucketId, name] = fields;
      }
      if (typeof bucketId !== "string" || !UUID.test(bucketId) || typeof name !== "string" || name === "") return null;
      return { bucket_id: bucketId.toLowerCase(), name };
    }
  }
}

/** A ref's identity: one entry per distinct ref in a batch. */
export function refKey(r: RefRequest): string {
  if ("id" in r) return JSON.stringify(["id", r.id]);
  if ("bucket_id" in r) return JSON.stringify(["ref", r.bucket_id, r.name]);
  return JSON.stringify(["path", r.bucket, r.name]);
}

/** What a resolved URL says about its object. */
export interface ResolvedObject {
  bucket: string;
  name: string;
  /** A public bucket's URL: no key needed, for anyone. */
  public: boolean;
}

const decodeAll = (segments: string[]): string[] | null => {
  try {
    return segments.map(decodeURIComponent);
  } catch {
    return null;
  }
};

/**
 * The bucket and name of a URL the resolve route answered with: a signed URL
 * (`/storage/v1/object/sign/{bucket}/{name}?token=…`) or a public one
 * (`/storage/v1/object/public/{branch}/{bucket}/{name}?database=…`). Anything else is null.
 */
export function parseObjectUrl(path: string): ResolvedObject | null {
  if (!path.startsWith("/storage/v1/object/")) return null;
  const segments = path.split("?")[0].slice("/storage/v1/object/".length).split("/");
  const kind = segments.shift();
  if (kind === "public") segments.shift();
  else if (kind !== "sign") return null;
  if (segments.length < 2 || segments.some((s) => s === "")) return null;
  const decoded = decodeAll(segments);
  if (!decoded) return null;
  return { bucket: decoded[0], name: decoded.slice(1).join("/"), public: kind === "public" };
}

const TYPES: Record<string, string> = {
  png: "image/png",
  jpg: "image/jpeg",
  jpeg: "image/jpeg",
  gif: "image/gif",
  webp: "image/webp",
  avif: "image/avif",
  bmp: "image/bmp",
  ico: "image/x-icon",
  mp4: "video/mp4",
  webm: "video/webm",
  mov: "video/quicktime",
  mp3: "audio/mpeg",
  wav: "audio/wav",
  ogg: "audio/ogg",
  m4a: "audio/mp4",
  flac: "audio/flac",
  pdf: "application/pdf",
  txt: "text/plain",
  csv: "text/csv",
  md: "text/markdown",
  log: "text/plain",
  json: "application/json",
};

/**
 * The type a name's extension suggests, for the chip and the preview (resolve answers no
 * metadata). Only media and text types are named: SVG, HTML and the rest get none, so they are
 * never previewed as anything but a download.
 */
export function guessContentType(name: string): string | null {
  const base = name.slice(name.lastIndexOf("/") + 1);
  const dot = base.lastIndexOf(".");
  if (dot <= 0) return null;
  return TYPES[base.slice(dot + 1).toLowerCase()] ?? null;
}

/** An object a cell can come to reference: picked from a listing or just uploaded. */
export interface RefTarget {
  /** The bucket's id (a `storage.object_ref` holds it); null when unknown. */
  bucketId: string | null;
  bucket: string;
  name: string;
  /** The object's id (a foreign key holds it); null when unknown. */
  id: string | null;
}

/** What the cell saves to reference `target`, or null when this column can't hold it. */
export function refValue(desc: StorageRefDesc, target: RefTarget): CellValue | null {
  switch (desc.kind) {
    case "ref":
      return target.bucketId ? { null: false, text: JSON.stringify({ bucket_id: target.bucketId, name: target.name }) } : null;
    case "path":
      return target.bucket === desc.bucket ? { null: false, text: target.name } : null;
    case "id":
      return target.id ? { null: false, text: target.id } : null;
  }
}
