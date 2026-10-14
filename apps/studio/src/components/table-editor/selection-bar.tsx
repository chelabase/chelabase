import { useQuery } from "@tanstack/react-query";
import { Button } from "@/components/ui/button";
import type { Selection } from "@/lib/table-editor/selection";

/** A filtered view's exact count: the query key and the request (a console `count` with the view's `where`, as its role). */
export interface CountSource {
  key: readonly unknown[];
  run: () => Promise<unknown>;
}

export interface SelectionBarProps {
  selection: Exclude<Selection, { kind: "none" }>;
  /** The page's rows are all selected: offer every matching row. */
  pageSelected: boolean;
  /** The row estimate of the table (null: unknown). */
  estimate: number | null;
  /** With a filter on: where the exact count comes from (null without a filter: the estimate is shown). */
  count: CountSource | null;
  canDelete: boolean;
  onSelectAllMatching: () => void;
  onCopy: () => void;
  onCopyJson: () => void;
  /** Delete; with every matching row of a filtered view selected, the exact count when it is known (else null). */
  onDelete: (matching: number | null) => void;
  onClear: () => void;
  /** A short confirmation after a copy ("Copied 2 rows"). */
  notice: string | null;
}

const approx = (n: number | null) => (n === null ? "" : `≈ ${n.toLocaleString("en-US")} `);
const exact = (n: number | null) => (n === null ? "" : `${n.toLocaleString("en-US")} `);

/** The `count` of a `{ "count": N }` result; null for anything else. */
function countOf(result: unknown): number | null {
  if (result === null || typeof result !== "object") return null;
  const n = (result as { count?: unknown }).count;
  return typeof n === "number" && Number.isSafeInteger(n) && n >= 0 ? n : null;
}

/** What's selected and what can be done with it: copy (TSV, JSON), delete, widen to every matching row, clear. */
export function SelectionBar({ selection, pageSelected, estimate, count, canDelete, onSelectAllMatching, onCopy, onCopyJson, onDelete, onClear, notice }: SelectionBarProps) {
  const all = selection.kind === "all_matching";
  // Counted only once every row on the page is selected (or every matching row is); a failure shows no figure.
  const counted = useQuery({
    queryKey: count?.key ?? ["table-editor-count", "none"],
    queryFn: async () => countOf(await count!.run()),
    enabled: count !== null && (pageSelected || all),
    retry: false,
    // Never kept past the bar: a later selection counts afresh (rows may have changed).
    gcTime: 0,
    // A count may scan the whole table: it is read again only when a row write or an Apply
    // invalidates it (`COUNT_QUERY`), never on focus or reconnect.
    staleTime: Infinity,
    refetchOnWindowFocus: false,
    refetchOnReconnect: false,
  });
  // Filtered: the exact count or no figure at all (never the whole table's estimate).
  const figure = count === null ? approx(all ? selection.estimate : estimate) : exact(counted.isSuccess ? counted.data : null);
  return (
    <div className="flex h-11 shrink-0 items-center gap-2 border-b border-line bg-deep px-4 text-sm">
      <span className="mr-2 inline-flex items-center gap-2 text-chalk">
        <span aria-hidden="true" className="size-2 bg-sand" />
        {all ? `All ${figure}matching rows selected` : `${selection.keys.length} selected`}
      </span>
      {!all && pageSelected && (
        <Button size="sm" variant="ghost" onClick={onSelectAllMatching}>
          {`Select all ${figure}matching rows`}
        </Button>
      )}
      <span className="flex-1" />
      {notice && (
        <span role="status" className="text-xs text-chalk-dim">
          {notice}
        </span>
      )}
      <Button size="sm" variant="outline" onClick={onCopy}>
        Copy
      </Button>
      <Button size="sm" variant="outline" onClick={onCopyJson}>
        Copy as JSON
      </Button>
      {canDelete && (
        <Button size="sm" variant="destructive" onClick={() => onDelete(all && count !== null && counted.isSuccess ? counted.data : null)}>
          Delete
        </Button>
      )}
      <Button size="sm" variant="ghost" onClick={onClear}>
        Clear selection
      </Button>
    </div>
  );
}
