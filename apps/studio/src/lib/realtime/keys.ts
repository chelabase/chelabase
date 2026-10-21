import type { Tree } from "@/lib/console";
import type { Row } from "@/lib/table-editor/rows";
import type { Key } from "./types";

export interface TextOf {
  text_of: string;
  as: string;
}

/** The key string of PK texts: `JSON.stringify(texts)`, byte-identical to the server's (S11). */
export function keyString(texts: string[]): Key {
  return JSON.stringify(texts);
}

/** `<prefix><i>`, with `_` appended until it names no taken column or alias. */
function freeAlias(prefix: string, i: number, taken: Set<string>): string {
  let name = `${prefix}${i}`;
  while (taken.has(name)) name += "_";
  taken.add(name);
  return name;
}

/** The aliases the page's primary-key texts are read under: `$pk0…`, renamed (`_` appended) on a clash with a real column. */
export function pkAliases(pk: string[], columns: string[]): string[] {
  const taken = new Set(columns);
  return pk.map((_, i) => freeAlias("$pk", i, taken));
}

/** Column → alias of its original-text item: `$o0…` by position in `originals`, renamed on a clash with `taken`. */
export function originalAliases(originals: string[], taken: string[]): Record<string, string> {
  const used = new Set(taken);
  return Object.fromEntries(originals.map((c, i) => [c, freeAlias("$o", i, used)]));
}

/** The explicit select list of a page (S9): every column, then `text_of` items for the key and the comparable columns. */
export function textSelectItems(a: { columns: string[]; pk: string[]; originals: string[] }): {
  items: (string | TextOf)[];
  pkAliases: string[];
  originalAliases: Record<string, string>;
} {
  const pa = pkAliases(a.pk, a.columns);
  const oa = originalAliases(a.originals, [...a.columns, ...pa]);
  const items: (string | TextOf)[] = [
    ...a.columns,
    ...a.pk.map((col, i) => ({ text_of: col, as: pa[i] })),
    ...a.originals.map((col) => ({ text_of: col, as: oa[col] })),
  ];
  return { items, pkAliases: pa, originalAliases: oa };
}

/** A row's key string from its `$pk` aliases; null when an alias is missing or null. */
export function textKeyOf(row: Row, aliases: string[]): Key | null {
  if (aliases.length === 0) return null;
  const texts: string[] = [];
  for (const a of aliases) {
    const v = row[a];
    if (typeof v !== "string") return null;
    texts.push(v);
  }
  return keyString(texts);
}

/** Types whose JSON value (as the console returns it) is exactly their Postgres text output. */
// Not `char(n)`: its text output drops the padding its JSON keeps.
const TEXT_SAME_AS_JSON = /^(text|citext|name|uuid|character varying|varchar)(\(\d+\))?$/;
const INTEGER = /^(smallint|integer|bigint|int2|int4|int8|smallserial|serial|bigserial)$/;

/**
 * The key string of a row read without its `$pk` texts (an insert's `returning`), when every key
 * column's typed value is exactly its text: text-like, uuid and enum strings, safe integers. Null
 * otherwise (timestamps, numerics and other types print differently as JSON).
 */
export function typedKeyString(pk: string[], row: Row, types: Record<string, { type: string; array?: boolean; enum?: string | null }>): Key | null {
  if (pk.length === 0) return null;
  const texts: string[] = [];
  for (const col of pk) {
    const desc = types[col];
    const v = row[col];
    if (!desc || desc.array) return null;
    const type = desc.type.trim().toLowerCase();
    if (typeof v === "string" && (!!desc.enum || TEXT_SAME_AS_JSON.test(type))) texts.push(v);
    else if (typeof v === "number" && Number.isSafeInteger(v) && INTEGER.test(type)) texts.push(String(v));
    else return null;
  }
  return keyString(texts);
}


/**
 * A key to look a row up by from its typed values (an insert's `returning`): each key value's JSON
 * form as a string, which the server parses as the column type (`"parse": true`; ISO 8601 and plain
 * numerics cast). Not the row's key: its texts may print differently. Null without a scalar key.
 */
export function lookupKey(pk: string[], row: Row, types: Record<string, { array?: boolean }>): Key | null {
  if (pk.length === 0) return null;
  const texts: string[] = [];
  for (const col of pk) {
    const v = row[col];
    if (types[col]?.array) return null;
    if (typeof v === "string") texts.push(v);
    else if ((typeof v === "number" && Number.isFinite(v)) || typeof v === "boolean") texts.push(String(v));
    else return null;
  }
  return keyString(texts);
}

/**
 * The canonical key string of a row just inserted: exact from its typed values when they are their
 * texts (`typedKeyString`), else the server's `$pk` texts from a by-key read with its `lookupKey`.
 * Null when that read finds no row (hidden from the role) or the key isn't scalar.
 */
export async function insertedKey(a: {
  schema: string;
  table: string;
  pk: string[];
  row: Row;
  types: Record<string, { type: string; array?: boolean; enum?: string | null }>;
  send: (tree: Tree) => Promise<unknown>;
}): Promise<Key | null> {
  const exact = typedKeyString(a.pk, a.row, a.types);
  if (exact !== null) return exact;
  const lookup = lookupKey(a.pk, a.row, a.types);
  if (lookup === null) return null;
  const sel = textSelectItems({ columns: a.pk, pk: a.pk, originals: [] });
  const raw = await a.send({ v: 1, op: "select", from: `${a.schema}.${a.table}`, select: sel.items, where: whereByTextKey(a.pk, lookup), limit: 1, offset: 0 });
  if (!Array.isArray(raw) || raw.length !== 1) return null;
  return textKeyOf(raw[0] as Row, sel.pkAliases);
}

/** The PK texts of a key string, checked against the key's columns. */
function keyTexts(pk: string[], key: Key): string[] {
  if (pk.length === 0) throw new Error("no primary key to select the row by");
  let texts: unknown;
  try {
    texts = JSON.parse(key);
  } catch {
    throw new Error("the row key isn't valid");
  }
  if (!Array.isArray(texts) || texts.length !== pk.length || !texts.every((t) => typeof t === "string")) {
    throw new Error(`the row key doesn't match the primary key (${pk.join(", ")})`);
  }
  return texts as string[];
}

/** The by-key conditions, one per PK column in key order. */
export function keyConditions(pk: string[], key: Key): Tree[] {
  const texts = keyTexts(pk, key);
  return pk.map((col, i) => ({ col, eq: texts[i], parse: true }));
}

/** The by-key `WHERE` (S32): `{col, eq: <text>, parse: true}` per PK column (the server parses the text as the column type), `{and: […]}` for a composite key. */
export function whereByTextKey(pk: string[], key: Key): Tree {
  const parts = keyConditions(pk, key);
  return parts.length === 1 ? parts[0] : { and: parts };
}

/** The `WHERE` of a page of keys: `{col, in: […], parse: true}` for one PK column, `{or: [and…]}` for a composite key. Throws on no keys. */
export function whereByTextKeys(pk: string[], keys: Key[]): Tree {
  if (keys.length === 0) throw new Error("no row keys to select by");
  if (pk.length === 1) return { col: pk[0], in: keys.map((k) => keyTexts(pk, k)[0]), parse: true };
  return { or: keys.map((k) => whereByTextKey(pk, k)) };
}
