import { useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import { createPortal } from "react-dom";
import { keepPreviousData, useQuery, useQueryClient } from "@tanstack/react-query";
import {
  ApiError,
  consoleQueryExact,
  getListBranchDatabasesQueryKey,
  useEnableDataApi,
  useInsertDefaultRow,
  useGetTable,
  useListTypes,
  type BranchDatabaseView,
  type ObjectSummary,
} from "@chelabase/management-client";
import { ChevronDown } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Menu, MenuItem } from "@/components/ui/menu";
import { OwnerPasswordNotice } from "@/components/owner-password-notice";
import { buildFilterTree, buildSelect, consoleErrorText, parseValue, whereByKey, type ConsoleOp, type FilterRow, type Sort, type Tree } from "@/lib/console";
import { errorText } from "@/lib/errors";
import type { RowAccess } from "@/lib/table-editor/access";
import {
  columnDescs,
  editText,
  isTooLarge,
  primaryKeyOf,
  readWidths,
  temporalKind,
  toTsv,
  validTimestamptz,
  writeWidths,
  type Row,
  type RowColumn,
} from "@/lib/table-editor/rows";
import { smallColumns } from "@/lib/table-editor/oversized";
import { rowKey, viewKey, type Selection } from "@/lib/table-editor/selection";
import { useTableEditor } from "@/lib/table-editor/store";
import type { CellValue } from "./cell-editor";
import { DeleteDialog, type DeleteTarget } from "./delete-dialog";
import { FilterPopover } from "./filter-popover";
import { Grid, type GridLayout } from "./grid";
import { LoadValueCell, cellKey, loadedValueKey, useLoadValue } from "./load-value";
import { SelectionBar } from "./selection-bar";
import { SortPopover } from "./sort-popover";
import type { TableTarget } from "./target";
import { useTableStructure } from "./use-structure";

/**
 * The first key of every rows query. Apply (T5) and every row write (inline edits, the row
 * panel's save, insert and delete, insert with defaults) invalidate both
 * `[ROWS_QUERY, databaseId, branchId]` and `[COUNT_QUERY, databaseId, branchId]`.
 */
export const ROWS_QUERY = "table-editor-rows";
/** The first key of the selection bar's filtered count (kept apart from `ROWS_QUERY`; see there). */
export const COUNT_QUERY = "table-editor-count";

/** The invalidations after a row write or an Apply: the rows and the filtered count of the branch. */
export function rowWriteKeys(databaseId: string, branchId: string): readonly (readonly unknown[])[] {
  return [
    [ROWS_QUERY, databaseId, branchId],
    [COUNT_QUERY, databaseId, branchId],
  ];
}
/** The row panel's single-row reads sit under `ROWS_QUERY` with this marker, so every refresh covers them. */
export const PANEL_ROW = "panel-row";

export interface RowsViewProps {
  target: TableTarget;
  access: RowAccess;
  /** The Postgres database the table is in (its data API state). */
  pg: BranchDatabaseView;
  /** Developer or above: may turn the data API on. */
  canEnableDataApi: boolean;
  /** The table's entry in the object list (row estimate, kind, editable). */
  object: ObjectSummary | undefined;
  /** Rows per page (`pageSizeFor(max_rows)`); null while the database's limits load. */
  pageSize: number | null;
  /** Where the view's tools (Filter, Insert row) go: the table bar. Rendered inline without one. */
  toolsSlot: HTMLElement | null;
  /** Content kept sticky above the grid in its scroll container (the column strip), passed to `Grid`. */
  above?: (layout: GridLayout) => ReactNode;
}

export const NO_PRIMARY_KEY = "This table has no primary key, so its rows are read-only here.";
const READ_ONLY_KIND: Record<string, string> = {
  view: "Views are read-only here.",
  materialized_view: "Materialized views are read-only here.",
  foreign_table: "Foreign tables are read-only here.",
  partitioned_table: "Partitioned tables are read-only here.",
};

function browserStorage(): Storage | null {
  try {
    return window.localStorage;
  } catch {
    return null;
  }
}

/** How many rows a key delete's batch deleted: each member returns the rows it deleted. Null when the shape is unknown. */
export function batchDeleted(result: unknown): number | null {
  if (!Array.isArray(result)) return null;
  let n = 0;
  for (const member of result) {
    if (!Array.isArray(member)) return null;
    n += member.length;
  }
  return n;
}

/** The line after a key delete that deleted fewer rows than asked; null when all were deleted (or unknown). */
export function deletedNotice(asked: number, deleted: number | null): string | null {
  if (deleted === null || deleted >= asked) return null;
  if (deleted === 0) return "No row was deleted (it may be hidden from this role, or gone).";
  return `Deleted ${deleted} of ${asked} rows; the rest may be hidden from this role, or gone.`;
}

/** The Rows tab: the data-API gate, or the grid with its tools, selection and paging. */
export function RowsView(props: RowsViewProps) {
  const { access } = props;
  if (!access.read) return <RowsGate {...props} />;
  return <RowsData {...props} />;
}

function RowsGate({ target, access, pg, canEnableDataApi }: RowsViewProps) {
  const queryClient = useQueryClient();
  const enable = useEnableDataApi({
    mutation: { onSuccess: () => void queryClient.invalidateQueries({ queryKey: getListBranchDatabasesQueryKey(target.databaseId, target.branchId) }) },
  });
  const dataApiOff = pg.data_api !== "on";
  const enabling = pg.data_api === "enabling";
  // While the worker bootstraps the data API, re-read the branch's databases until it settles.
  useEffect(() => {
    if (!enabling) return;
    const id = window.setInterval(
      () => void queryClient.invalidateQueries({ queryKey: getListBranchDatabasesQueryKey(target.databaseId, target.branchId) }),
      2000,
    );
    return () => window.clearInterval(id);
  }, [enabling, queryClient, target.databaseId, target.branchId]);
  return (
    <div data-testid="rows-gate" className="m-6 max-w-xl space-y-3 border border-line p-4 text-sm">
      <p className="text-chalk">{access.reason}</p>
      {dataApiOff && pg.data_api === "error" && pg.data_api_error && <p className="text-coral">{pg.data_api_error}</p>}
      {enabling && (
        <p role="status" className="text-chalk-dim">
          Turning the data API on for {pg.name}…
        </p>
      )}
      {dataApiOff && !enabling && canEnableDataApi && (
        <Button disabled={enable.isPending} onClick={() => enable.mutate({ id: target.databaseId, branchId: target.branchId, name: pg.name })}>
          {enable.isPending ? "Enabling…" : "Enable data API"}
        </Button>
      )}
      {dataApiOff && !canEnableDataApi && <p className="text-chalk-dim">A Developer or Admin can turn it on.</p>}
      {enable.error && (
        <p role="alert" className="text-coral">
          {errorText(enable.error)}
        </p>
      )}
    </div>
  );
}

interface Page {
  rows: Row[];
  /** The page size the rows were read with (halved on `too_large`). */
  size: number;
  /** The offset the rows were read from. */
  offset: number;
  /** Set when a single row was still too large: the columns the rows were read with (the others show "Load value"). */
  small: string[] | null;
}

/** Previous rows may stand in only for another page of the same view (never another role, table, filter or sort). */
function samePageView(a: readonly unknown[], b: readonly unknown[]): boolean {
  return JSON.stringify(a.slice(0, -1)) === JSON.stringify(b.slice(0, -1));
}

function RowsData({ target, access, object, pageSize, toolsSlot, above }: RowsViewProps) {
  const { databaseId, branchId, database, schema, table, role } = target;
  const qualified = `${schema}.${table}`;
  const queryClient = useQueryClient();
  const isTable = !object || (object.kind === "table" && object.editable);
  const dbParams = database ? { database } : undefined;
  const structure = useGetTable(databaseId, branchId, schema, table, dbParams, { query: { enabled: isTable } });
  const types = useListTypes(databaseId, branchId, dbParams);
  const tableDef = structure.data;
  const pk = useMemo(() => (tableDef ? primaryKeyOf(tableDef) : []), [tableDef]);
  // Columns added in the pending changes get a placeholder track in the grid, under their strip chip.
  const { projected } = useTableStructure(target, { enabled: isTable });
  const pendingColumns = useMemo(() => (projected ? projected.columns.filter((c) => c.original === null).map((c) => c.name) : []), [projected]);

  const [offset, setOffset] = useState(0);
  const [sort, setSort] = useState<Sort | null>(null);
  const [filters, setFilters] = useState<FilterRow[]>([]);
  const [where, setWhere] = useState<Tree | null>(null);
  // The size reads start from: the page size, halved for this view (role, sort, filter) once a page
  // was too large. Another view starts again from the page size; only the current view's query writes it.
  const size = useRef<{ view: string; n: number } | null>(null);
  const sizeView = JSON.stringify([role, sort, where]);
  const currentSizeView = useRef(sizeView);
  currentSizeView.current = sizeView;

  const send = (tree: Tree) => consoleQueryExact(databaseId, branchId, { role, database, tree });
  const queryKey = [ROWS_QUERY, databaseId, branchId, database, role, qualified, pk, sort, where, offset];
  const rowsQuery = useQuery({
    queryKey,
    queryFn: async (): Promise<Page> => {
      const mine = sizeView;
      let n = (size.current?.view === mine ? size.current.n : null) ?? pageSize ?? 1;
      for (;;) {
        try {
          const tree = buildSelect({ schema, table, page: 0, offset, sort, filters: where, pk, pageSize: n });
          const rows = (await send(tree)) as Row[];
          if (currentSizeView.current === mine) size.current = { view: mine, n };
          return { rows, size: n, offset, small: null };
        } catch (err) {
          if (!isTooLarge(err)) throw err;
          if (n <= 1) {
            // One row is still too large: read it without its large columns.
            const small = tableDef ? smallColumns(tableDef) : [];
            if (!tableDef || small.length >= tableDef.columns.length) throw err;
            const tree = buildSelect({ schema, table, page: 0, offset, sort, filters: where, pk, pageSize: 1, columns: small });
            return { rows: (await send(tree)) as Row[], size: 1, offset, small };
          }
          n = Math.max(1, Math.floor(n / 2));
        }
      }
    },
    enabled: pageSize !== null && (isTable ? structure.isSuccess : true),
    placeholderData: (previous, previousQuery) =>
      previousQuery && samePageView(previousQuery.queryKey, queryKey) ? keepPreviousData(previous) : undefined,
    retry: false,
    gcTime: 0,
  });
  const page = rowsQuery.data;
  const rows = useMemo(() => page?.rows ?? [], [page]);

  const columns: RowColumn[] = useMemo(() => {
    if (tableDef) return columnDescs(tableDef, types.data?.types ?? []);
    return Object.keys(rows[0] ?? {}).map((name) => ({
      name,
      desc: { type: "text", array: false, enum: null, generated: false, has_default: false, nullable: true },
      labels: null,
      readOnly: true,
    }));
  }, [tableDef, types.data, rows]);

  // The table's columns changed (an Apply, a refetch): sort and filter entries naming a column
  // it no longer has are dropped, so the next read doesn't fail on them.
  const [seenDef, setSeenDef] = useState(tableDef);
  if (seenDef !== tableDef) {
    setSeenDef(tableDef);
    if (tableDef) {
      const names = new Set(tableDef.columns.map((c) => c.name));
      if (sort && !names.has(sort.col)) setSort(null);
      const kept = filters.filter((f) => names.has(f.col));
      if (kept.length !== filters.length) {
        const built = buildFilterTree({ mode: "and", rows: kept }, Object.fromEntries(columns.map((c) => [c.name, c.desc])));
        setFilters(built.ok ? kept : []);
        setWhere(built.ok ? (built.where ?? null) : null);
        setOffset(0);
      }
    }
  }

  const writable = access.write && !!tableDef && pk.length > 0;
  const note = !isTable
    ? (READ_ONLY_KIND[object?.kind ?? ""] ?? "This object is read-only here.")
    : tableDef && pk.length === 0
      ? NO_PRIMARY_KEY
      : !access.write
        ? "Viewers can read rows but not change them."
        : null;

  // Selection: kept in the store, for this view (table, role, filter, sort) only.
  const view = viewKey({ changeset: `${target.key}|${role ?? ""}`, filter: where, sort });
  const stored = useTableEditor((s) => s.selection);
  const select = useTableEditor((s) => s.select);
  const openPanel = useTableEditor((s) => s.openPanel);
  const openKey = useTableEditor((s) => (s.panel.kind === "row" ? s.panel.row : null));
  const loader = useLoadValue(target);
  const insertDefault = useInsertDefaultRow();
  const [insertError, setInsertError] = useState<string | null>(null);
  // The columns of a page of single too-large rows that weren't read.
  const unread = useMemo(() => (page?.small ? new Set(columns.map((c) => c.name).filter((n) => !page.small!.includes(n))) : null), [page, columns]);
  useEffect(() => select({ type: "view", view }), [select, view]);
  const selection: Selection = stored.view === view ? stored.selection : { kind: "none" };
  const keys = useMemo(() => rows.map((r) => (pk.length > 0 ? rowKey(pk, r) : null)), [rows, pk]);
  const pageKeys = keys.filter((k): k is string => k !== null);
  const selectedKeys = selection.kind === "rows" ? selection.keys : [];
  const pageSelection =
    selection.kind === "all_matching" || (pageKeys.length > 0 && pageKeys.every((k) => selectedKeys.includes(k)))
      ? "all"
      : pageKeys.some((k) => selectedKeys.includes(k))
        ? "some"
        : "none";

  const [widths, setWidths] = useState(() => readWidths(browserStorage(), target.key));
  const [notice, setNotice] = useState<string | null>(null);
  const [deleting, setDeleting] = useState<DeleteTarget | null>(null);
  const [deleteBusy, setDeleteBusy] = useState(false);
  const [deleteError, setDeleteError] = useState<string | null>(null);
  // After a key delete that deleted fewer rows than asked.
  const [deleteNotice, setDeleteNotice] = useState<string | null>(null);

  const text = (err: unknown, op: ConsoleOp) => consoleErrorText(err, { role, op });

  function goto(next: number) {
    select({ type: "clear" });
    setOffset(Math.max(0, next));
  }

  async function saveCell(key: string, column: RowColumn, value: CellValue): Promise<string | null> {
    // The row is found by its key at save time, wherever a refetch moved it.
    const row = rowsQuery.isPlaceholderData ? undefined : rows.find((r) => rowKey(pk, r) === key);
    if (!row) return "That row is no longer on this page.";
    let parsed: unknown = null;
    if (!value.null) {
      if (temporalKind(column.desc) === "timestamptz" && !validTimestamptz(value.text)) {
        return `${column.name}: enter a date and time with its offset, like 2026-10-01 12:00:00+00.`;
      }
      const p = parseValue(column.desc, value.text);
      if (!p.ok) return `${column.name}: ${p.error}.`;
      parsed = p.value;
    }
    let where: Tree;
    try {
      where = whereByKey(pk, row);
    } catch (err) {
      return err instanceof Error ? err.message : "Something went wrong";
    }
    try {
      const result = (await send({ v: 1, op: "update", table: qualified, set: { [column.name]: parsed }, where, returning: "*" })) as Row[];
      if (!Array.isArray(result) || result.length === 0) return "No row was changed (it may be hidden from this role, or gone).";
      const fresh = result.find((r) => rowKey(pk, r) === key) ?? result[0];
      queryClient.setQueryData<Page>(queryKey, (old) => (old ? { ...old, rows: old.rows.map((r) => (rowKey(pk, r) === key ? fresh : r)) } : old));
      void queryClient.invalidateQueries({ queryKey: [ROWS_QUERY, databaseId, branchId, PANEL_ROW] });
      // The edit may move the row in or out of the filter.
      void queryClient.invalidateQueries({ queryKey: [COUNT_QUERY, databaseId, branchId] });
      return null;
    } catch (err) {
      return text(err, "update");
    }
  }

  /** "Insert with all defaults": one row of defaults, then its panel. */
  async function insertDefaults() {
    setInsertError(null);
    try {
      const res = await insertDefault.mutateAsync({ id: databaseId, branchId, schema, table, data: { database, role } });
      await Promise.all(rowWriteKeys(databaseId, branchId).map((queryKey) => queryClient.invalidateQueries({ queryKey })));
      if (pk.length > 0) openPanel("row", { row: rowKey(pk, res.row as Row) });
    } catch (err) {
      setInsertError(text(err, "insert"));
    }
  }

  function chosenRows(): Row[] {
    if (selection.kind === "all_matching") return rows;
    return rows.filter((_, i) => keys[i] !== null && selectedKeys.includes(keys[i]!));
  }

  async function copy(kind: "tsv" | "json", list: Row[], cols: string[]) {
    const payload = kind === "tsv" ? toTsv(cols, list) : JSON.stringify(list, null, 2);
    try {
      await navigator.clipboard.writeText(payload);
      setNotice(`Copied ${list.length} ${list.length === 1 ? "row" : "rows"}${kind === "json" ? " as JSON" : ""}`);
    } catch {
      setNotice("The clipboard isn't available here.");
    }
  }

  async function confirmDelete() {
    if (!deleting) return;
    setDeleteError(null);
    setDeleteNotice(null);
    const chosen = deleting.kind === "rows" ? chosenRows() : [];
    if (deleting.kind === "rows" && chosen.length === 0) {
      // A refetch removed the selected rows from the page: nothing is sent.
      setDeleteError("The selected rows are no longer on this page.");
      return;
    }
    setDeleteBusy(true);
    let tree: Tree;
    try {
      if (deleting.kind === "rows") {
        // Each delete returns the key of the row it deleted, so rows hidden from the role (or gone) are noticed.
        tree = { v: 1, op: "batch", ops: chosen.map((r) => ({ op: "delete", from: qualified, where: whereByKey(pk, r), returning: pk })) };
      } else {
        tree = where ? { v: 1, op: "delete", from: qualified, where } : { v: 1, op: "delete", from: qualified, all: true };
      }
    } catch (err) {
      setDeleteBusy(false);
      setDeleteError(err instanceof Error ? err.message : "Something went wrong");
      return;
    }
    let result: unknown;
    try {
      result = await send(tree);
    } catch (err) {
      setDeleteBusy(false);
      setDeleteError(text(err, "delete"));
      return;
    }
    if (deleting.kind === "rows") setDeleteNotice(deletedNotice(chosen.length, batchDeleted(result)));
    const wholePage = deleting.kind === "all_matching" || chosen.length === rows.length;
    setDeleteBusy(false);
    setDeleting(null);
    select({ type: "clear" });
    if (wholePage && offset > 0) setOffset(Math.max(0, offset - (page?.size ?? pageSize ?? 1)));
    // The count's selection was cleared: its query is unobserved, so only the rows are read again.
    await queryClient.invalidateQueries({ queryKey: [ROWS_QUERY, databaseId, branchId] });
  }

  const failure = rowsQuery.error ?? structure.error;
  const ownerUnknown = failure instanceof ApiError && failure.code === "owner_password_unknown";
  const estimate = object?.row_estimate ?? null;
  // The range of the rows on screen (a stand-in page keeps its own range until the next arrives).
  const shownOffset = page?.offset ?? offset;
  const from = shownOffset + 1;
  const to = shownOffset + rows.length;
  const full = page !== undefined && rows.length >= page.size;
  // A page shorter than the page size ends the rows: the view's exact total (a stand-in page proves nothing,
  // nor an empty page past the first, which may follow deletes).
  const exact = page !== undefined && !rowsQuery.isPlaceholderData && !full && (rows.length > 0 || shownOffset === 0) ? shownOffset + rows.length : null;
  // The table's own total (no filter), for the table bar and the table list when there is no estimate.
  const setExactRows = useTableEditor((s) => s.setExactRows);
  const storedExact = useTableEditor((s) => s.exactRows[target.key] ?? null);
  useEffect(() => {
    if (where !== null || page === undefined || rowsQuery.isPlaceholderData) return;
    if (exact !== null) setExactRows(target.key, exact);
    else if (full && storedExact !== null && storedExact <= shownOffset + rows.length) setExactRows(target.key, null);
  }, [where, page, rowsQuery.isPlaceholderData, exact, full, storedExact, shownOffset, rows.length, setExactRows, target.key]);
  // The footer's total: the estimate, else the exact count a short page gave, else "more" while pages stay full.
  const total = estimate !== null ? `≈ ${estimate.toLocaleString("en-US")}` : exact !== null ? exact.toLocaleString("en-US") : full ? "more" : "many";
  // "1 / N" between Prev and Next once the total is known (the estimate counts only without a filter).
  const stepSize = page?.size ?? pageSize ?? 1;
  const known = exact ?? (where === null ? estimate : null);
  const pages = known === null ? null : Math.max(1, Math.ceil(known / stepSize));
  const pageNumber = Math.floor(shownOffset / stepSize) + 1;

  const tools = (
    <div className="flex items-center gap-2">
      <SortPopover
        columns={columns}
        value={sort}
        onChange={(next) => {
          setOffset(0);
          setSort(next);
        }}
      />
      <FilterPopover
        columns={columns}
        value={filters}
        onChange={(next, tree) => {
          setFilters(next);
          setWhere(tree ?? null);
          setOffset(0);
        }}
      />
      {writable && (
        <div className="flex items-center">
          <Button size="sm" variant="outline" onClick={() => openPanel("row", { row: null })}>
            Insert row
          </Button>
          <Menu
            trigger={
              <Button size="icon-sm" variant="outline" aria-label="More insert options" className="-ml-px">
                <ChevronDown aria-hidden="true" strokeWidth={1.5} />
              </Button>
            }
          >
            <MenuItem onClick={() => void insertDefaults()}>Insert with all defaults</MenuItem>
          </Menu>
        </div>
      )}
    </div>
  );

  return (
    <div className="flex min-h-0 flex-1 flex-col">
      {toolsSlot ? createPortal(tools, toolsSlot) : <div className="flex justify-end border-b border-line px-4 py-2">{tools}</div>}
      {insertError && (
        <p role="alert" className="border-b border-line px-4 py-2 text-sm text-coral">
          {insertError}
        </p>
      )}
      {note && <p className="border-b border-line px-4 py-2 text-sm text-chalk-dim">{note}</p>}
      {deleteNotice && (
        <p role="status" className="border-b border-line px-4 py-2 text-sm text-chalk">
          {deleteNotice}
        </p>
      )}
      {ownerUnknown && (
        <div className="p-4">
          <OwnerPasswordNotice databaseId={databaseId} branchId={branchId} onRetry={() => void rowsQuery.refetch()} />
        </div>
      )}
      {failure && !ownerUnknown && (
        <p role="alert" className="border-b border-line px-4 py-2 text-sm text-coral">
          {rowsQuery.error ? text(rowsQuery.error, "select") : errorText(structure.error)}
        </p>
      )}
      {selection.kind !== "none" && (
        <SelectionBar
          selection={selection}
          pageSelected={pageSelection === "all"}
          estimate={estimate}
          count={where ? { key: [COUNT_QUERY, databaseId, branchId, database, role, qualified, where], run: () => send({ v: 1, op: "count", from: qualified, where }) } : null}
          canDelete={writable}
          notice={notice}
          onSelectAllMatching={() => select({ type: "all_matching", estimate })}
          onCopy={() => void copy("tsv", chosenRows(), columns.map((c) => c.name))}
          onCopyJson={() => void copy("json", chosenRows(), columns.map((c) => c.name))}
          onDelete={(matching) => {
            setDeleteError(null);
            setDeleting(
              selection.kind === "all_matching"
                ? { kind: "all_matching", filtered: where !== null, count: where !== null ? matching : null }
                : { kind: "rows", count: chosenRows().length },
            );
          }}
          onClear={() => {
            setNotice(null);
            select({ type: "clear" });
          }}
        />
      )}
      {page === undefined && !failure && (
        <p role="status" className="px-4 py-2 text-sm text-chalk-dim">
          Loading rows…
        </p>
      )}
      {columns.length > 0 && (
          <Grid
            above={above}
            resetKey={`${view}|${offset}`}
            label={qualified}
            columns={columns}
            rows={rows}
            firstRowNumber={from}
            keys={keys}
            isSelected={(k) => selection.kind === "all_matching" || selectedKeys.includes(k)}
            pageSelection={pageSelection}
            onToggleRow={(k) => select({ type: "toggle", key: k })}
            // In the all-matching state the header box shows checked: a click clears the selection.
            onTogglePage={() => select(selection.kind === "all_matching" ? { type: "clear" } : { type: "page", keys: pageKeys })}
            sort={sort}
            onSort={(col) => {
              setOffset(0);
              setSort(sort?.col !== col ? { col, dir: "asc" } : sort.dir === "asc" ? { col, dir: "desc" } : null);
            }}
            canEdit={(col) => writable && !rowsQuery.isPlaceholderData && !pk.includes(col) && !unread?.has(col) && !columns.find((c) => c.name === col)?.readOnly}
            onSave={saveCell}
            onCopy={(active) => {
              const list = chosenRows();
              if (list.length > 0) void copy("tsv", list, columns.map((c) => c.name));
              else if (active) void navigator.clipboard?.writeText(editText(rows[active.row]?.[active.column]));
            }}
            onOpenRow={(i) => (keys[i] ? openPanel("row", { row: keys[i] }) : undefined)}
            openKey={openKey}
            renderCell={(row, c, i) =>
              unread?.has(c.name) && keys[i] !== null ? (
                <LoadValueCell
                  column={c.name}
                  cacheKey={loadedValueKey(target, keys[i]!, c.name)}
                  load={() => loader.load(cellKey(pk, row), c.name)}
                  errorText={loader.errorText}
                />
              ) : undefined
            }
            widths={widths}
            pending={pendingColumns}
            onWidths={(next) => {
              setWidths(next);
              writeWidths(browserStorage(), target.key, next);
            }}
          />
      )}
      <div className="flex h-10 shrink-0 items-center justify-between border-t border-line-strong px-4 text-[13px] text-chalk-dim">
        <span>
          {page === undefined
            ? ""
            : rows.length === 0
              ? shownOffset === 0
                ? "No rows"
                : "No more rows"
              : `Rows ${from.toLocaleString("en-US")}–${to.toLocaleString("en-US")} of ${total}`}
        </span>
        <div className="flex items-center gap-2">
          <Button size="sm" variant="outline" aria-label="Previous page" disabled={offset === 0 || rowsQuery.isFetching} onClick={() => goto(offset - (page?.size ?? 1))}>
            Prev
          </Button>
          {pages !== null && page !== undefined && rows.length > 0 && (
            <span className="min-w-[4ch] text-center font-mono text-xs tabular-nums">
              {pageNumber} / {exact === null ? "≈ " : ""}
              {Math.max(pages, pageNumber).toLocaleString("en-US")}
            </span>
          )}
          <Button size="sm" variant="outline" aria-label="Next page" disabled={!full || rowsQuery.isFetching} onClick={() => goto(shownOffset + rows.length)}>
            Next
          </Button>
        </div>
      </div>
      {deleting && (
        <DeleteDialog
          open
          onOpenChange={(open) => {
            if (!open) setDeleting(null);
          }}
          target={deleting}
          table={table}
          asOwner={role === null}
          busy={deleteBusy}
          error={deleteError}
          onConfirm={() => void confirmDelete()}
        />
      )}
    </div>
  );
}
