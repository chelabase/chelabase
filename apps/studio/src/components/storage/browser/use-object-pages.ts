import { useMemo } from "react";
import { useInfiniteQuery } from "@tanstack/react-query";
import { storageListObjects } from "@chelabase/management-client";
import { objectsKey, parseObjectPage, storageParams, type ObjectListQuery, type StorageItem } from "@/lib/storage-api";

/** Objects read per page. */
export const OBJECT_PAGE = 100;

export type ObjectSort = "name" | "size" | "updated_at";
export type ObjectOrder = "asc" | "desc";

export interface ObjectQuery extends ObjectListQuery {
  sort: ObjectSort;
  order: ObjectOrder;
}

/**
 * A folder's listing, a page at a time by storage's cursor. The key holds the folder, sort and
 * search, so changing any of them starts again from page one. Items repeated across pages
 * (storage's cursor shouldn't, but a write between pages may) are shown once.
 */
export function useObjectPages(databaseId: string, branchId: string, role: string | null, bucket: string, q: ObjectQuery, enabled: boolean) {
  const query = useInfiniteQuery({
    queryKey: objectsKey(databaseId, branchId, role, bucket, q),
    queryFn: async ({ pageParam, signal }) =>
      parseObjectPage(
        await storageListObjects(
          databaseId,
          branchId,
          bucket,
          { prefix: q.prefix, delimiter: "/", limit: OBJECT_PAGE, cursor: pageParam, sort_by: q.sort, order: q.order, search: q.search === "" ? null : q.search },
          storageParams(role),
          { signal },
        ),
      ),
    initialPageParam: null as string | null,
    getNextPageParam: (last) => last.nextCursor ?? undefined,
    enabled,
    retry: false,
  });
  const items = useMemo(() => {
    const seen = new Set<string>();
    const out: StorageItem[] = [];
    for (const page of query.data?.pages ?? []) {
      for (const item of page.items) {
        if (seen.has(item.name)) continue;
        seen.add(item.name);
        out.push(item);
      }
    }
    return out;
  }, [query.data]);
  const { hasNextPage, isFetchingNextPage, fetchNextPage } = query;
  return {
    items,
    loading: query.isPending,
    error: query.error,
    hasMore: hasNextPage,
    loadingMore: isFetchingNextPage,
    loadMore: async () => {
      if (hasNextPage && !isFetchingNextPage) await fetchNextPage();
    },
    refetch: query.refetch,
  };
}
