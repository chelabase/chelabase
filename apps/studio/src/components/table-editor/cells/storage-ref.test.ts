import type { Column, Constraint, Table, TypeEntry } from "@chelabase/management-client";
import { describe, expect, it } from "vitest";
import { parseValue } from "@/lib/console";
import { columnDescs } from "@/lib/table-editor/rows";
import { guessContentType, parseObjectUrl, refKey, refRequestOf, refValue, storageBucketOf, storageRefOf } from "./storage-ref";

const B = "3f2b8c1e-7d6a-4e59-9b0c-2a1d4e5f6a7b";
const O = "9a8b7c6d-5e4f-4a3b-8c2d-1e0f9a8b7c6d";

function col(name: string, data_type: string, extra: Partial<Column> = {}): Column {
  return { attnum: 1, name, data_type, not_null: false, grants: [], type_oid: 25, ...extra };
}
function table(columns: Column[], constraints: Constraint[] = []): Table {
  return {
    columns,
    constraints,
    grants: [],
    indexes: [],
    name: "t",
    oid: 1,
    options: [],
    owner: { name: "app_owner" } as Table["owner"],
    policies: [],
    rls_enabled: false,
    rls_forced: false,
    schema: "public",
    unlogged: false,
  };
}
const fk = (attnum: number, schema: string, tbl: string, definition: string): Constraint => ({
  columns: [attnum],
  deferrable: false,
  definition,
  initially_deferred: false,
  kind: "foreign_key",
  name: "fk",
  oid: 9,
  references: { columns: [1], schema, table: tbl, table_oid: 77 },
  validated: true,
});
const textType: TypeEntry = { oid: 25, schema: "pg_catalog", name: "text", category: "S", kind: "base", preferred: true, takes_modifiers: false };
const refType: TypeEntry = { oid: 900, schema: "storage", name: "object_ref", category: "C", kind: "composite", preferred: false, takes_modifiers: false };
const fakeRef: TypeEntry = { ...refType, oid: 901, schema: "public" };

describe("storageBucketOf", () => {
  // As m11's storage_bucket_of: the word right after @storage_ref, when it is bucket=<name>.
  it("reads_the_bucket_after_the_marker", () => {
    expect(storageBucketOf("@storage_ref bucket=avatars")).toBe("avatars");
    expect(storageBucketOf("User photo.\n@storage_ref   bucket=photos and more")).toBe("photos");
  });
  it("needs_the_marker_and_a_non_empty_bucket", () => {
    expect(storageBucketOf(null)).toBeNull();
    expect(storageBucketOf("bucket=avatars")).toBeNull();
    expect(storageBucketOf("@storage_ref bucket=")).toBeNull();
    expect(storageBucketOf("@storage_ref avatars")).toBeNull();
    expect(storageBucketOf("@storage_refs bucket=avatars")).toBeNull();
  });
});

describe("storageRefOf", () => {
  it("the_composite_storage_object_ref_is_a_ref", () => {
    const c = col("photo", "storage.object_ref", { type_oid: 900 });
    expect(storageRefOf(c, table([c]), refType)).toEqual({ kind: "ref" });
    // The catalog may not list storage's types: format_type's qualified name is enough.
    expect(storageRefOf(c, table([c]), undefined)).toEqual({ kind: "ref" });
  });

  it("only_storage_s_object_ref_counts", () => {
    const c = col("photo", "object_ref", { type_oid: 901 });
    expect(storageRefOf(c, table([c]), fakeRef)).toBeNull();
  });

  it("a_text_column_with_the_comment_is_a_path_in_its_bucket", () => {
    const c = col("avatar", "text", { comment: "@storage_ref bucket=avatars" });
    expect(storageRefOf(c, table([c]), textType)).toEqual({ kind: "path", bucket: "avatars" });
    const v = col("avatar", "character varying(200)", { comment: "@storage_ref bucket=avatars", type_oid: 1043 });
    expect(storageRefOf(v, table([v]), undefined)).toEqual({ kind: "path", bucket: "avatars" });
  });

  it("the_comment_on_a_non_text_column_is_ignored", () => {
    const c = col("n", "integer", { comment: "@storage_ref bucket=avatars", type_oid: 23 });
    expect(storageRefOf(c, table([c]), { ...textType, oid: 23, name: "int4", category: "N" })).toBeNull();
  });

  it("a_single_column_foreign_key_to_storage_objects_id_is_an_id", () => {
    const c = col("file_id", "uuid", { attnum: 2, type_oid: 2950 });
    const t = table([c], [fk(2, "storage", "objects", "FOREIGN KEY (file_id) REFERENCES storage.objects(id)")]);
    expect(storageRefOf(c, t, undefined)).toEqual({ kind: "id" });
  });

  it("other_foreign_keys_are_not_refs", () => {
    const c = col("file_id", "uuid", { attnum: 2, type_oid: 2950 });
    expect(storageRefOf(c, table([c], [fk(2, "public", "objects", "FOREIGN KEY (file_id) REFERENCES objects(id)")]), undefined)).toBeNull();
    expect(storageRefOf(c, table([c], [fk(2, "storage", "objects", "FOREIGN KEY (file_id) REFERENCES storage.objects(owner)")]), undefined)).toBeNull();
    const two = { ...fk(2, "storage", "objects", "FOREIGN KEY (file_id, x) REFERENCES storage.objects(id, y)"), columns: [2, 3] };
    expect(storageRefOf(c, table([c], [two]), undefined)).toBeNull();
  });

  it("the_foreign_key_wins_over_a_comment", () => {
    const c = col("file", "text", { attnum: 2, comment: "@storage_ref bucket=avatars" });
    expect(storageRefOf(c, table([c], [fk(2, "storage", "objects", "FOREIGN KEY (file) REFERENCES storage.objects(id)")]), textType)).toEqual({ kind: "id" });
  });

  it("arrays_are_not_cells_of_one_ref", () => {
    const c = col("photos", "storage.object_ref[]");
    expect(storageRefOf(c, table([c]), undefined)).toBeNull();
    const t = col("paths", "text[]", { comment: "@storage_ref bucket=avatars" });
    expect(storageRefOf(t, table([t]), undefined)).toBeNull();
  });
});

describe("refRequestOf", () => {
  it("reads_a_composite_as_json_or_as_a_record_literal", () => {
    expect(refRequestOf({ kind: "ref" }, { bucket_id: B, name: "a/b.png" })).toEqual({ bucket_id: B, name: "a/b.png" });
    expect(refRequestOf({ kind: "ref" }, `(${B},a/b.png)`)).toEqual({ bucket_id: B, name: "a/b.png" });
    expect(refRequestOf({ kind: "ref" }, `(${B},"my ""best"" photo, \\\\1.png")`)).toEqual({ bucket_id: B, name: 'my "best" photo, \\1.png' });
  });

  it("a_path_is_its_bucket_and_the_text", () => {
    expect(refRequestOf({ kind: "path", bucket: "avatars" }, "u/1.png")).toEqual({ bucket: "avatars", name: "u/1.png" });
  });

  it("an_id_is_the_uuid", () => {
    expect(refRequestOf({ kind: "id" }, O)).toEqual({ id: O });
  });

  it("null_and_malformed_values_ask_nothing", () => {
    expect(refRequestOf({ kind: "ref" }, null)).toBeNull();
    expect(refRequestOf({ kind: "ref" }, { bucket_id: null, name: "x" })).toBeNull();
    expect(refRequestOf({ kind: "ref" }, "garbage")).toBeNull();
    expect(refRequestOf({ kind: "path", bucket: "avatars" }, "")).toBeNull();
    expect(refRequestOf({ kind: "id" }, "not-a-uuid")).toBeNull();
  });

  it("keys_are_stable_per_ref", () => {
    expect(refKey({ bucket_id: B, name: "x" })).toBe(refKey({ name: "x", bucket_id: B } as never));
    expect(refKey({ bucket: "a", name: "x" })).not.toBe(refKey({ bucket_id: "a", name: "x" }));
  });
});

describe("parseObjectUrl", () => {
  it("reads_the_bucket_and_name_of_a_signed_url", () => {
    expect(parseObjectUrl("/storage/v1/object/sign/avatars/u%201/a%2Bb.png?token=t")).toEqual({ bucket: "avatars", name: "u 1/a+b.png", public: false });
  });
  it("reads_the_bucket_and_name_of_a_public_url", () => {
    expect(parseObjectUrl(`/storage/v1/object/public/${B}/pics/x/y.jpg?database=app`)).toEqual({ bucket: "pics", name: "x/y.jpg", public: true });
  });
  it("anything_else_is_not_an_object_url", () => {
    expect(parseObjectUrl("https://evil.test/storage/v1/object/sign/a/b")).toBeNull();
    expect(parseObjectUrl("/storage/v1/object/sign/a")).toBeNull();
    expect(parseObjectUrl("/other")).toBeNull();
  });
});

describe("guessContentType", () => {
  it("names_common_types_by_extension", () => {
    expect(guessContentType("a/b.PNG")).toBe("image/png");
    expect(guessContentType("x.jpeg")).toBe("image/jpeg");
    expect(guessContentType("doc.pdf")).toBe("application/pdf");
    expect(guessContentType("clip.mp4")).toBe("video/mp4");
    expect(guessContentType("notes.txt")).toBe("text/plain");
  });
  // SVG and HTML never preview as media on Studio's origin.
  it("svg_html_and_unknown_get_no_media_type", () => {
    expect(guessContentType("logo.svg")).toBeNull();
    expect(guessContentType("page.html")).toBeNull();
    expect(guessContentType("blob")).toBeNull();
    expect(guessContentType("archive.zip")).toBeNull();
  });
});

describe("refValue", () => {
  const target = { bucketId: B, bucket: "avatars", name: "u/1.png", id: O };
  it("a_ref_saves_the_composite_as_json", () => {
    expect(refValue({ kind: "ref" }, target)).toEqual({ null: false, text: JSON.stringify({ bucket_id: B, name: "u/1.png" }) });
  });
  it("a_path_saves_the_name_when_the_bucket_is_the_column_s", () => {
    expect(refValue({ kind: "path", bucket: "avatars" }, target)).toEqual({ null: false, text: "u/1.png" });
    expect(refValue({ kind: "path", bucket: "other" }, target)).toBeNull();
  });
  it("an_id_saves_the_object_id", () => {
    expect(refValue({ kind: "id" }, target)).toEqual({ null: false, text: O });
    expect(refValue({ kind: "id" }, { ...target, id: null })).toBeNull();
    expect(refValue({ kind: "ref" }, { ...target, bucketId: null })).toBeNull();
  });
});

describe("the grid's column types", () => {
  it("columnDescs_marks_storage_ref_columns", () => {
    const photo = col("photo", "storage.object_ref", { attnum: 1, type_oid: 900 });
    const avatar = col("avatar", "text", { attnum: 2, comment: "@storage_ref bucket=avatars" });
    const plain = col("note", "text", { attnum: 3 });
    const descs = columnDescs(table([photo, avatar, plain]), [refType, textType]);
    expect(descs.map((d) => d.desc.storage_ref)).toEqual([{ kind: "ref" }, { kind: "path", bucket: "avatars" }, undefined]);
  });

  // #106 AC4: the raw value of a composite ref stays editable: JSON as the grid shows it, or a record literal.
  it("a_ref_column_s_raw_text_saves_as_json_or_a_record_literal", () => {
    const desc = { type: "storage.object_ref", array: false, enum: null, generated: false, has_default: false, nullable: true, storage_ref: { kind: "ref" as const } };
    expect(parseValue(desc, `{"bucket_id":"${B}","name":"a.png"}`)).toEqual({ ok: true, value: { bucket_id: B, name: "a.png" } });
    expect(parseValue(desc, `(${B},a.png)`)).toEqual({ ok: true, value: `(${B},a.png)` });
    expect(parseValue(desc, "{nope")).toEqual({ ok: false, error: "enter JSON like {\"bucket_id\": …, \"name\": …} or a record (bucket_id,name)" });
  });
});
