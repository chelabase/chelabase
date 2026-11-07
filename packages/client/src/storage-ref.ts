// `client.storage.resolve`: stored-object references to URLs in one request
// (`POST /storage/v1/object/resolve`). Loaded on first use by `storage-lazy.ts`, like
// `storage.ts`, so a query-only app's bundle never carries it.
import { absoluteUrl, call, readJson, type ResolveOptions } from "./storage.ts";
import type { StorageContext } from "./storage-lazy.ts";
import type { StorageRefLike } from "./types.ts";

type Ref = StorageRefLike | null | undefined;

/** The ref as sent: its one shape's fields and nothing else (a whole row may be passed). */
function wire(ref: StorageRefLike): Record<string, string> {
  const r = ref as { id?: string; bucket_id?: string; bucket?: string; name?: string };
  if (r.id !== undefined) return { id: r.id };
  if (r.bucket_id !== undefined) return { bucket_id: r.bucket_id, name: r.name as string };
  return { bucket: r.bucket as string, name: r.name as string };
}

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
  const sent: { at: number; ref: Record<string, string> }[] = [];
  refs.forEach((ref, at) => {
    if (ref !== null && ref !== undefined) sent.push({ at, ref: wire(ref) });
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
