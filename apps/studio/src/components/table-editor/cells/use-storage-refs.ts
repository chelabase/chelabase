import { useCallback, useMemo } from "react";
import { useQuery } from "@tanstack/react-query";
import { getStorageListBucketsQueryKey, storageResolveObjects } from "@chelabase/management-client";
import { apiHref, MAX_SIGNED_TTL, storageParams } from "@/lib/storage-api";
import { parseObjectUrl, refKey, type RefRequest } from "./storage-ref";

/** What a ref came to: its object (a URL to it, its bucket and name), missing, or hidden from the role. */
export type RefState =
  | { state: "ok"; url: string; bucket: string; name: string; public: boolean }
  | { state: "missing" }
  | { state: "no_access" };

/** Where refs are resolved, and as whom. */
export interface RefScope {
  databaseId: string;
  branchId: string;
  /** The `database` parameter: null for the branch's main database. */
  database: string | null;
  /** The Act as role; null is owner mode. */
  role: string | null;
  apiUrl: string;
  /** The member is an Admin of the database: owner mode can tell a hidden object from a missing one. */
  admin: boolean;
}

/** The resolve route takes at most this many refs at once. */
const MAX_REFS = 1000;
/** Signed URLs live an hour: read them again well before. */
const STALE_MS = 30 * 60 * 1000;

const isObject = (v: unknown): v is Record<string, unknown> => typeof v === "object" && v !== null && !Array.isArray(v);

/** One resolve call per 1000 refs (a page has far fewer): a URL path or null per ref, in order. */
async function resolveAll(scope: RefScope, role: string | null, refs: RefRequest[], signal: AbortSignal): Promise<(string | null)[]> {
  const out: (string | null)[] = [];
  for (let i = 0; i < refs.length; i += MAX_REFS) {
    const chunk = refs.slice(i, i + MAX_REFS);
    const answer = await storageResolveObjects(scope.databaseId, scope.branchId, { refs: chunk, expires_in: MAX_SIGNED_TTL }, storageParams(role, scope.database), { signal });
    const urls = isObject(answer) && Array.isArray(answer.urls) ? answer.urls : [];
    chunk.forEach((_, j) => out.push(typeof urls[j] === "string" ? (urls[j] as string) : null));
  }
  return out;
}

/**
 * The refs of the visible page, resolved together: one request for every distinct ref, as the Act
 * as role. Resolve answers null for a missing object and a hidden one alike, so an Admin acting as
 * a role asks once more in owner mode for the nulls (owner mode sees every object); in owner mode a
 * null is missing; below Admin it can't be told apart and reads as no access.
 */
export function useStorageRefs(scope: RefScope, requests: readonly RefRequest[], enabled: boolean) {
  const unique = useMemo(() => {
    const seen = new Map<string, RefRequest>();
    for (const r of requests) if (!seen.has(refKey(r))) seen.set(refKey(r), r);
    return [...seen.values()];
  }, [requests]);
  const keys = useMemo(() => unique.map(refKey), [unique]);

  const query = useQuery({
    queryKey: [...getStorageListBucketsQueryKey(scope.databaseId, scope.branchId).slice(0, 1), "resolve", storageParams(scope.role, scope.database), scope.admin, keys],
    queryFn: async ({ signal }) => {
      const urls = await resolveAll(scope, scope.role, unique, signal);
      const nulls = unique.map((_, i) => i).filter((i) => urls[i] === null);
      // Owner mode's answer for the nulls: a URL there means the role can't read it.
      let seen: (string | null)[] | null = null;
      if (scope.role !== null && scope.admin && nulls.length > 0) {
        seen = await resolveAll(
          scope,
          null,
          nulls.map((i) => unique[i]),
          signal,
        ).catch(() => null);
      }
      const states = new Map<string, RefState>();
      unique.forEach((_, i) => {
        const url = urls[i];
        if (url !== null) {
          const object = parseObjectUrl(url);
          states.set(keys[i], object ? { state: "ok", url: apiHref(scope.apiUrl, url), ...object } : { state: "no_access" });
          return;
        }
        if (scope.role === null) states.set(keys[i], { state: "missing" });
        else if (seen) states.set(keys[i], seen[nulls.indexOf(i)] === null ? { state: "missing" } : { state: "no_access" });
        else states.set(keys[i], { state: "no_access" });
      });
      return states;
    },
    enabled: enabled && unique.length > 0,
    staleTime: STALE_MS,
    retry: false,
  });

  const data = query.data;
  const get = useCallback((r: RefRequest): RefState | undefined => data?.get(refKey(r)), [data]);
  return { get, error: query.error, loading: query.isLoading };
}
