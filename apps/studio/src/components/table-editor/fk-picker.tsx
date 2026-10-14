import { useEffect, useMemo, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { consoleQueryExact, useGetTable, useListTypes, type Table } from "@chelabase/management-client";
import { Modal } from "@/components/ui/dialog";
import { Button } from "@/components/ui/button";
import { buildSelect, consoleErrorText, isTextLike, parseValue, type Tree } from "@/lib/console";
import { smallColumns } from "@/lib/table-editor/oversized";
import { columnDescs, editText, primaryKeyOf, type Row } from "@/lib/table-editor/rows";
import type { TableTarget } from "./target";

/** A foreign key as the row panel uses it: the columns it spans and the table and columns it points at. */
export interface FkRef {
  /** The constraint's columns, by name, in key order. */
  columns: string[];
  schema: string;
  table: string;
  /** The referenced columns' attnums in the referenced table (resolved to names once it loads). */
  refAttnums: number[];
}

/** The foreign key that spans this column (the first, when several do), or null. */
export function fkOf(table: Table, column: string): FkRef | null {
  const names = new Map(table.columns.map((c) => [c.attnum, c.name]));
  for (const c of table.constraints) {
    if (c.kind !== "foreign_key" || !c.references) continue;
    const columns = c.columns.map((n) => names.get(n)).filter((n): n is string => n !== undefined);
    if (columns.includes(column) && columns.length === c.columns.length) {
      return { columns, schema: c.references.schema, table: c.references.table, refAttnums: c.references.columns };
    }
  }
  return null;
}

/** How many rows the picker lists. */
export const PICK_LIMIT = 20;

const escapeLike = (q: string) => q.replace(/[\\%_]/g, (m) => `\\${m}`);

export interface FkPickerProps {
  target: TableTarget;
  fk: FkRef;
  /** The picked row's values for the referenced columns, keyed by the foreign key's own column names. */
  onPick: (values: Record<string, unknown>) => void;
  onClose: () => void;
}

/**
 * "Pick from {table}": lists up to 20 rows of the referenced table, read as the same role, with a
 * search over its key and text columns. Choosing a row hands back the key values.
 */
/** How long typing pauses before the search is sent. */
export const SEARCH_DEBOUNCE_MS = 250;

export function FkPicker({ target, fk, onPick, onClose }: FkPickerProps) {
  const { databaseId, branchId, database, role } = target;
  const dbParams = database ? { database } : undefined;
  const ref = useGetTable(databaseId, branchId, fk.schema, fk.table, dbParams);
  const types = useListTypes(databaseId, branchId, dbParams);
  const [text, setText] = useState("");
  // The search is sent once typing pauses, not on every keystroke (each is a scan of the table).
  const [query, setQuery] = useState("");
  useEffect(() => {
    const next = text.trim();
    if (next === query) return;
    const id = window.setTimeout(() => setQuery(next), SEARCH_DEBOUNCE_MS);
    return () => window.clearTimeout(id);
  }, [text, query]);

  const refTable = ref.data;
  const refNames = useMemo(() => {
    if (!refTable) return [];
    const names = new Map(refTable.columns.map((c) => [c.attnum, c.name]));
    return fk.refAttnums.map((n) => names.get(n)).filter((n): n is string => n !== undefined);
  }, [refTable, fk.refAttnums]);
  const descs = useMemo(() => (refTable ? columnDescs(refTable, types.data?.types ?? []) : []), [refTable, types.data]);
  const small = useMemo(() => (refTable ? smallColumns(refTable) : []), [refTable]);
  // Shown beside the key: the first two other small text columns.
  const detailCols = useMemo(
    () => descs.filter((c) => !refNames.includes(c.name) && small.includes(c.name) && isTextLike(c.desc)).slice(0, 2).map((c) => c.name),
    [descs, refNames, small],
  );

  const rows = useQuery({
    queryKey: ["table-editor-fk-pick", databaseId, branchId, database, role, fk.schema, fk.table, query],
    enabled: !!refTable && refNames.length === fk.refAttnums.length,
    retry: false,
    gcTime: 0,
    queryFn: async (): Promise<Row[]> => {
      const parts: Tree[] = [];
      if (query !== "") {
        for (const c of descs) {
          if (!small.includes(c.name) || (!refNames.includes(c.name) && !detailCols.includes(c.name))) continue;
          if (isTextLike(c.desc)) {
            parts.push({ col: c.name, ilike: `%${escapeLike(query)}%` });
          } else if (refNames.includes(c.name)) {
            const p = parseValue(c.desc, query);
            if (p.ok) parts.push({ col: c.name, eq: p.value });
          }
        }
      }
      const where: Tree | null = parts.length === 0 ? null : parts.length === 1 ? parts[0] : { or: parts };
      const tree = buildSelect({
        schema: fk.schema,
        table: fk.table,
        page: 0,
        sort: null,
        filters: where,
        pk: refTable ? primaryKeyOf(refTable) : [],
        pageSize: PICK_LIMIT,
        columns: [...new Set([...refNames, ...small])],
      });
      return (await consoleQueryExact(databaseId, branchId, { role, database, tree })) as Row[];
    },
  });

  return (
    <Modal open onOpenChange={(open) => !open && onClose()} title={`Pick from ${fk.table}`} description={`Up to ${PICK_LIMIT} rows. Search the key and text columns.`}>
      <input
        aria-label="Search"
        value={text}
        autoComplete="off"
        spellCheck={false}
        placeholder="Search"
        onChange={(e) => setText(e.target.value)}
        className="h-9 w-full border border-input bg-ink px-2 text-sm text-chalk outline-none focus-visible:outline-2 focus-visible:-outline-offset-1 focus-visible:outline-sand"
      />
      {ref.error && (
        <p role="alert" className="text-sm text-coral">
          {consoleErrorText(ref.error, { role, op: "select" })}
        </p>
      )}
      {rows.error && (
        <p role="alert" className="text-sm text-coral">
          {consoleErrorText(rows.error, { role, op: "select" })}
        </p>
      )}
      {rows.isPending && !ref.error && (
        <p role="status" className="text-sm text-chalk-dim">
          Loading…
        </p>
      )}
      {rows.data && rows.data.length === 0 && <p className="text-sm text-chalk-dim">No rows match.</p>}
      {rows.data && rows.data.length > 0 && (
        <div role="listbox" aria-label={`Rows of ${fk.table}`} className="max-h-80 overflow-y-auto border border-line">
          {rows.data.map((row, i) => {
            const key = refNames.map((n) => editText(row[n])).join(" · ");
            const detail = detailCols.map((n) => editText(row[n])).filter((d) => d !== "").join(" · ");
            return (
              <button
                key={i}
                type="button"
                role="option"
                aria-selected={false}
                onClick={() => onPick(Object.fromEntries(fk.columns.map((c, j) => [c, row[refNames[j]]])))}
                className="flex h-9 w-full items-center gap-3 border-b border-line px-3 text-left outline-none last:border-b-0 hover:bg-chalk/[0.08] focus-visible:outline-2 focus-visible:-outline-offset-2 focus-visible:outline-sand"
              >
                <span className="font-mono text-[13px] text-chalk">{key}</span>
                {detail && <span className="truncate text-xs text-chalk-dim">{detail}</span>}
              </button>
            );
          })}
        </div>
      )}
      <div className="flex justify-end">
        <Button variant="ghost" onClick={onClose}>
          Cancel
        </Button>
      </div>
    </Modal>
  );
}
