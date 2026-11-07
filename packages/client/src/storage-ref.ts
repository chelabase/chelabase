// `client.storage.resolve`: `storage.object_ref` values to URLs in one request
// (`POST /storage/v1/object/resolve`). Loaded on first use by `storage-lazy.ts`, like
// `storage.ts`, so a query-only app's bundle never carries it.
import { absoluteUrl, call, readJson, type ResolveOptions } from "./storage.ts";
import type { StorageContext } from "./storage-lazy.ts";
import type { StorageRef } from "./types.ts";

type Ref = StorageRef | null | undefined;

/** One ref to one URL, or refs to URLs in input order; `null` where nothing is readable. */
export async function resolveRefs(
  ctx: StorageContext,
  input: Ref | readonly Ref[],
  opts: ResolveOptions = {},
): Promise<string | null | (string | null)[]> {
  const many = Array.isArray(input);
  const refs: readonly Ref[] = many ? (input as readonly Ref[]) : [input as Ref];
  const out: (string | null)[] = refs.map(() => null);
  // Only the refs that name something are sent; a null one keeps its null slot.
  const sent: { at: number; ref: StorageRef }[] = [];
  refs.forEach((ref, at) => {
    if (ref !== null && ref !== undefined) sent.push({ at, ref: { bucket_id: ref.bucket_id, name: ref.name } });
  });
  if (sent.length > 0) {
    const json: Record<string, unknown> = { refs: sent.map((s) => s.ref) };
    if (opts.expiresIn !== undefined) json["expires_in"] = opts.expiresIn;
    const res = await call(ctx, { method: "POST", path: "/storage/v1/object/resolve", json });
    const { urls } = await readJson<{ urls: (string | null)[] }>(res);
    sent.forEach((s, i) => {
      const u = urls[i];
      out[s.at] = typeof u === "string" ? absoluteUrl(ctx, u, opts.download) : null;
    });
  }
  return many ? out : out[0]!;
}
