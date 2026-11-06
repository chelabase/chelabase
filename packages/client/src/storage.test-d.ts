// Type-level tests for storage, checked by `tsc --noEmit` (the package's `typecheck`). Every
// `@ts-expect-error` must fire; an unused one is itself an error.
import { expectTypeOf } from "vitest";
import {
  type Bucket,
  createClient,
  type FileObject,
  type ObjectInfo,
  type SignedUploadUrl,
  type SignedUrl,
  type StorageErrorCode,
  type UploadResult,
} from "./index.ts";
import { createBrowserClient } from "./browser.ts";
import { createServerClient } from "./server.ts";

const db = createClient({ url: "http://localhost:8000", key: "chk_pub_x" });
const files = db.storage.from("x");

// AC7: list returns FileObject[] (with the next page's cursor alongside).
expectTypeOf(files.list()).resolves.toExtend<FileObject[]>();
expectTypeOf(files.list()).resolves.toHaveProperty("nextCursor").toEqualTypeOf<string | null>();
expectTypeOf<FileObject["name"]>().toEqualTypeOf<string>();
expectTypeOf(files.upload("a", new Blob([]))).resolves.toEqualTypeOf<UploadResult>();
expectTypeOf(files.download("a")).resolves.toEqualTypeOf<Response>();
expectTypeOf(files.exists("a")).resolves.toEqualTypeOf<boolean>();
expectTypeOf(files.info("a")).resolves.toEqualTypeOf<ObjectInfo>();
expectTypeOf(files.remove(["a"])).resolves.toEqualTypeOf<string[]>();
expectTypeOf(files.createSignedUrl("a")).resolves.toEqualTypeOf<SignedUrl>();
expectTypeOf(files.createSignedUrls(["a"])).resolves.toEqualTypeOf<SignedUrl[]>();
expectTypeOf(files.createSignedUploadUrl("a")).resolves.toEqualTypeOf<SignedUploadUrl>();
expectTypeOf(files.getPublicUrl("a")).toEqualTypeOf<string>();
expectTypeOf(db.storage.listBuckets()).resolves.toEqualTypeOf<Bucket[]>();
expectTypeOf(db.database("other").storage.from("x").list()).resolves.toExtend<FileObject[]>();
expectTypeOf<"not_found">().toExtend<StorageErrorCode>();

// Every body kind the issue names.
void files.upload("a", "text");
void files.upload("a", new ArrayBuffer(1));
void files.upload("a", new Uint8Array(1));
void files.upload("a", new ReadableStream<Uint8Array>());

// --- misuse fails to compile
// @ts-expect-error a number is no upload body
void files.upload("a", 42);
// @ts-expect-error sort by name, size or updated_at only
void files.list({ sortBy: "color" });
// @ts-expect-error order is asc or desc
void files.list({ order: "up" });
// @ts-expect-error list's result is FileObject[], not a string
const s: string = await files.list();
void s;
// @ts-expect-error getPublicUrl is synchronous: no promise
void files.getPublicUrl("a").then;
// @ts-expect-error a bucket needs a name
void db.storage.createBucket({ public: true });
// @ts-expect-error a bucket field that doesn't exist
void db.storage.updateBucket("x", { colour: "red" });
// @ts-expect-error a path is required
void files.download();
// @ts-expect-error progress is a callback
void files.upload("a", "x", { onProgress: true });

// The browser and server clients expose the same storage.
const browser = createBrowserClient({ url: "http://localhost:8000", key: "chk_pub_x" });
expectTypeOf(browser.storage.from("x").list()).resolves.toExtend<FileObject[]>();
const server = createServerClient({ url: "http://localhost:8000", key: "chk_pub_x", cookies: { getAll: () => [] } });
expectTypeOf(server.storage.from("x").list()).resolves.toExtend<FileObject[]>();
// The server takes a Node stream (any async iterable of bytes).
declare const readable: import("node:stream").Readable;
void server.storage.from("x").upload("a", readable);
