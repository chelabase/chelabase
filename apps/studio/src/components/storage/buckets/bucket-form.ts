import type { Bucket } from "@/lib/storage-api";

/** The bucket dialog's fields, as typed. */
export interface BucketFormInput {
  name: string;
  public: boolean;
  /** Blank: no limit. */
  sizeLimit: string;
  sizeUnit: SizeUnit;
  /** Comma- or line-separated `type/subtype`, `type/*` or `*` entries; blank: any type. */
  allowedTypes: string;
}

export type SizeUnit = "B" | "KB" | "MB" | "GB";
export const SIZE_UNITS: readonly SizeUnit[] = ["B", "KB", "MB", "GB"];
const FACTOR: Record<SizeUnit, number> = { B: 1, KB: 1024, MB: 1024 ** 2, GB: 1024 ** 3 };

/** What create (`name`, `public`, …) or update (only what changed) sends. */
export interface BucketBody {
  name?: string;
  public?: boolean;
  file_size_limit?: number;
  allowed_mime_types?: string[];
}

export type BucketFormErrors = Partial<Record<"name" | "sizeLimit" | "allowedTypes", string>>;
export type BucketFormResult = { ok: true; body: BucketBody } | ({ ok: false } & BucketFormErrors);

/** Storage's rule: 1 to 63 lower-case letters, digits, `.`, `_` or `-`, starting with a letter or digit. */
const NAME = /^[a-z0-9][a-z0-9._-]{0,62}$/;
/** Literal segments the storage routes use where a bucket name goes (storage's `RESERVED_BUCKET_NAMES`). */
export const RESERVED_BUCKET_NAMES = ["list", "copy", "move", "sign", "public", "upload", "resolve", "session"] as const;
/** One allowed-type entry: `type/subtype`, `type/*`, `*` or the match-all pair, without parameters. */
const TYPE = /^(\*|\*\/\*|[a-z0-9][a-z0-9!#$&^_.+-]*\/(\*|[a-z0-9][a-z0-9!#$&^_.+-]*))$/i;

export const BLANK_BUCKET_FORM: BucketFormInput = { name: "", public: false, sizeLimit: "", sizeUnit: "MB", allowedTypes: "" };

/** The form for an existing bucket: its limit in the largest unit it is a whole number of. */
export function bucketFormFrom(b: Bucket): BucketFormInput {
  let sizeLimit = "";
  let sizeUnit: SizeUnit = "MB";
  if (b.file_size_limit !== null) {
    const unit = [...SIZE_UNITS].reverse().find((u) => b.file_size_limit! % FACTOR[u] === 0) ?? "B";
    sizeLimit = String(b.file_size_limit / FACTOR[unit]);
    sizeUnit = unit;
  }
  return { name: b.name, public: b.public, sizeLimit, sizeUnit, allowedTypes: (b.allowed_mime_types ?? []).join(", ") };
}

const splitTypes = (text: string) =>
  text
    .split(/[,\n]/)
    .map((t) => t.trim())
    .filter((t) => t !== "");

const sameList = (a: readonly string[] | null, b: readonly string[]) => a !== null && a.length === b.length && a.every((x, i) => x === b[i]);

/**
 * Checks the form as storage would and builds the body. `existing` null: a new bucket (name and
 * public always sent). Otherwise an update with only the changed settings (the name isn't edited
 * here); storage keeps a limit it isn't sent, so a size limit can't be removed, and clearing the
 * allowed types sends the match-all entry (any type).
 */
export function parseBucketForm(input: BucketFormInput, existing: Bucket | null): BucketFormResult {
  const errors: BucketFormErrors = {};
  const name = input.name.trim();
  if (existing === null) {
    if (!NAME.test(name)) {
      errors.name = "A bucket name is 1 to 63 lower-case letters, digits, `.`, `_` or `-`, starting with a letter or digit.";
    } else if ((RESERVED_BUCKET_NAMES as readonly string[]).includes(name)) {
      errors.name = `\`${name}\` is reserved by the storage routes.`;
    }
  }

  let limit: number | null = null;
  const sizeText = input.sizeLimit.trim();
  if (sizeText === "") {
    if (existing !== null && existing.file_size_limit !== null) errors.sizeLimit = "A size limit can't be removed once set; set a larger one instead.";
  } else if (!/^\d+$/.test(sizeText) || Number(sizeText) <= 0) {
    errors.sizeLimit = "The size limit is a whole number above 0, or blank for no limit.";
  } else {
    limit = Number(sizeText) * FACTOR[input.sizeUnit];
  }

  const types = splitTypes(input.allowedTypes);
  const bad = types.filter((t) => !TYPE.test(t));
  if (bad.length > 0) errors.allowedTypes = `Not a content type: ${bad.join(", ")}. Use type/subtype or type/*.`;

  if (Object.keys(errors).length > 0) return { ok: false, ...errors };

  if (existing === null) {
    const body: BucketBody = { name, public: input.public };
    if (limit !== null) body.file_size_limit = limit;
    if (types.length > 0) body.allowed_mime_types = types;
    return { ok: true, body };
  }
  const body: BucketBody = {};
  if (input.public !== existing.public) body.public = input.public;
  if (limit !== null && limit !== existing.file_size_limit) body.file_size_limit = limit;
  if (types.length > 0) {
    if (!sameList(existing.allowed_mime_types, types)) body.allowed_mime_types = types;
  } else if (existing.allowed_mime_types !== null) {
    body.allowed_mime_types = ["*/*"];
  }
  return { ok: true, body };
}
