import { useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { useGetCell } from "@chelabase/management-client";
import { consoleErrorText } from "@/lib/console";
import { cellText, type Row } from "@/lib/table-editor/rows";
import type { TableTarget } from "./target";

/** The cell endpoint's key for a row: its primary key columns and their values. */
export function cellKey(pk: string[], row: Row): Record<string, unknown> {
  return Object.fromEntries(pk.map((c) => [c, row[c]]));
}

/** Reads one cell's full value (`useGetCell`) as the target's role; resolves to its text, or null for NULL. */
export function useLoadValue(target: TableTarget) {
  const cell = useGetCell();
  const { databaseId, branchId, schema, table, database, role } = target;
  return {
    load: async (key: Record<string, unknown>, column: string): Promise<string | null> => {
      const res = await cell.mutateAsync({ id: databaseId, branchId, schema, table, data: { database, role, key, column } });
      return res.value ?? null;
    },
    errorText: (err: unknown) => consoleErrorText(err, { role, op: "select" }),
  };
}

export interface LoadValueButtonProps {
  column: string;
  /** Loads the value; rejects with the failure. */
  load: () => Promise<string | null>;
  /** Called with the loaded value. */
  onLoaded: (value: string | null) => void;
  errorText: (err: unknown) => string;
}

/** The "Load value" button of one large cell: one request per click; a failure is shown beside it. */
export function LoadValueButton({ column, load, onLoaded, errorText }: LoadValueButtonProps) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  async function click() {
    setBusy(true);
    setError(null);
    try {
      onLoaded(await load());
    } catch (err) {
      setError(errorText(err));
    } finally {
      setBusy(false);
    }
  }
  return (
    <span className="flex min-w-0 items-center gap-2">
      <button
        type="button"
        tabIndex={-1}
        aria-label={`Load value of ${column}`}
        disabled={busy}
        onClick={() => void click()}
        className="shrink-0 border border-line-strong px-2 py-0.5 font-sans text-[11px] text-chalk outline-none hover:bg-chalk/[0.08] focus-visible:outline-2 focus-visible:outline-sand disabled:opacity-60"
      >
        {busy ? "Loading…" : error ? "Retry" : "Load value"}
      </button>
      {error && (
        <span role="alert" className="truncate font-sans text-[11px] text-coral" title={error}>
          {error}
        </span>
      )}
    </span>
  );
}

/** The first key of the grid's loaded large values (in the query cache, so a virtualizer remount keeps them). */
export const LOADED_VALUE = "table-editor-loaded-value";

/** Where one loaded value is kept: the table (changeset key), the role, the row's key and the column. */
export function loadedValueKey(target: TableTarget, rowKeyText: string, column: string): readonly unknown[] {
  return [LOADED_VALUE, target.key, target.role, rowKeyText, column];
}

export interface LoadValueCellProps extends Omit<LoadValueButtonProps, "onLoaded"> {
  /** `loadedValueKey` of the cell. */
  cacheKey: readonly unknown[];
}

/** A large grid cell left unread: "Load value", then the value (cut like any cell) once loaded. */
export function LoadValueCell({ column, load, errorText, cacheKey }: LoadValueCellProps) {
  const queryClient = useQueryClient();
  // Only written by a click (`setQueryData`); the query never runs on its own.
  const { data: loaded } = useQuery<{ value: string | null }>({
    queryKey: cacheKey,
    queryFn: async () => ({ value: await load() }),
    enabled: false,
    staleTime: Infinity,
  });
  const setLoaded = (v: { value: string | null }) => queryClient.setQueryData(cacheKey, v);
  if (loaded) {
    return <span className={loaded.value === null ? "truncate whitespace-pre text-chalk-dim" : "truncate whitespace-pre"}>{cellText(loaded.value)}</span>;
  }
  return <LoadValueButton column={column} load={load} onLoaded={(value) => setLoaded({ value })} errorText={errorText} />;
}
