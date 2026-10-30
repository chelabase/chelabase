import { useCallback, useEffect, useMemo, useRef, useState } from "react";
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
import { ChevronDown, Plus } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Menu, MenuItem } from "@/components/ui/menu";
import { OwnerPasswordNotice } from "@/components/owner-password-notice";
import { buildFilterTree, buildSelect, consoleErrorText, inputKind, parseValue, type ConsoleOp, type FilterRow, type Sort, type Tree } from "@/lib/console";
import { errorText } from "@/lib/errors";
import { NO_CHECK_NOTICE, conflictWhere, isComparable, splitTextRows, type Originals } from "@/lib/realtime/conflict";
import { useRealtime } from "@/lib/realtime/context";
import { insertedKey, textSelectItems, whereByTextKey, whereByTextKeys } from "@/lib/realtime/keys";
import { INITIAL_LIVE, createLiveController, type LiveController, type LiveState } from "@/lib/realtime/live-rows";
import { applyPresenceDiff, editingNotice, presenceColor } from "@/lib/realtime/presence";
import type { Key, PresenceMember, TableSubscription } from "@/lib/realtime/types";
import { useMediaQuery } from "@/lib/use-media-query";
import type { RowAccess } from "@/lib/table-editor/access";
import {
  columnDescs,
  editText,
  isTooLarge,
  primaryKeyOf,
  readWidths,
  temporalKind,
  toJson,
  toTsv,
  validTimestamptz,
  writeWidths,
  type Row,
  type RowColumn,
} from "@/lib/table-editor/rows";
import { smallColumns } from "@/lib/table-editor/oversized";
import { viewKey, type Selection } from "@/lib/table-editor/selection";
import { useTableEditor } from "@/lib/table-editor/store";
import type { CellValue } from "./cell-editor";
import type { CellMenuGroups } from "./cell-menu";
import { ConflictDialog, type ConflictDialogProps } from "./conflict-dialog";
import { DeleteDialog, type DeleteTarget } from "./delete-dialog";
import { FilterPopover } from "./filter-popover";
import { Grid, type CellMenuTarget } from "./grid";
import { LiveBadge, type LiveStatus } from "./live-badge";
import { LoadValueCell, cellKey, loadedValueKey, useLoadValue } from "./load-value";
import { AvatarStack, PresenceLayer } from "./presence-layer";
import { SelectionBar } from "./selection-bar";
import { SortPopover } from "./sort-popover";
import type { TableTarget } from "./target";
import { useTableStructure } from "./use-structure";
import { useColumnHeaders, type ColumnChips } from "./column-headers";

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
/** The row panel's read of the row with this key. */
export function panelRowKey(t: Pick<TableTarget, "databaseId" | "branchId" | "database" | "role" | "schema" | "table">, key: Key | null): readonly unknown[] {
  return [ROWS_QUERY, t.databaseId, t.branchId, PANEL_ROW, t.database, t.role, `${t.schema}.${t.table}`, key];
}
/** A row just inserted, shown by the panel while it reads the row (and its originals) by key. */
export function seedPanelRow(queryClient: ReturnType<typeof useQueryClient>, t: Parameters<typeof panelRowKey>[0], key: Key, row: Row) {
  queryClient.setQueryData(panelRowKey(t, key), { row, fallback: false });
}

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
  /** Column chips as the grid's header cells (structure editing), with why structure can't change; omitted, plain headers. */
  chips?: ColumnChips;
  /** The viewer's `sub`: their own selection is never drawn as presence. */
  selfSub?: string;
}

export const NO_PRIMARY_KEY = "This table has no primary key, so its rows are read-only here.";
export const READ_ONLY_KIND: Record<string, string> = {
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
  /** Each row's key string (its primary key's text, W4); null without a primary key. */
  keys: (Key | null)[];
  /** Each row's comparable columns as text, as read (the conflict check's originals, R18). */
  originals: Originals[];
  /** The page size the rows were read with (halved on `too_large`). */
  size: number;
  /** The offset the rows were read from. */
  offset: number;
  /** Set when a single row was still too large: the columns the rows were read with (the others show "Load value"). */
  small: string[] | null;
  /** Counts page reads (a splice keeps it): a new read clears the live marks. */
  read: number;
}

let pageReads = 0;

/** Rows read again after a change: spliced in place of the page's rows with the same keys. */
interface Fresh {
  rows: Row[];
  keys: (Key | null)[];
  originals: Originals[];
}

/** A save that matched no row (§6.3): the dialog's state and what Overwrite sends again. */
interface Conflict {
  key: Key;
  set: Record<string, unknown>;
  state: ConflictDialogProps["state"];
  /** The row as re-read (Take theirs splices it). */
  fresh: Fresh | null;
  unchecked: string[];
  busy: boolean;
  error: string | null;
}

/** A re-read row's value as the conflict dialog shows it: its text as read, else its JSON text; null for NULL. */
function theirText(fresh: Fresh, i: number, column: string): string | null {
  const o = fresh.originals[i];
  if (o && Object.hasOwn(o, column)) return o[column];
  const v = fresh.rows[i]?.[column];
  return v === null || v === undefined ? null : editText(v);
}

/** How long a copy notice stays without the selection bar. */
const NOTICE_MS = 4000;
/** How long a spliced cell stays marked changed. */
const FLASH_MS = 1600;
/** Selection changes are sent at most this often (§5.1). */
const PRESENCE_DEBOUNCE_MS = 50;

/** Previous rows may stand in only for another page of the same view (never another role, table, filter or sort). */
function samePageView(a: readonly unknown[], b: readonly unknown[]): boolean {
  return JSON.stringify(a.slice(0, -1)) === JSON.stringify(b.slice(0, -1));
}

function RowsData({ target, access, object, pageSize, toolsSlot, chips, selfSub = "" }: RowsViewProps) {
  const { databaseId, branchId, database, schema, table, role } = target;
  const qualified = `${schema}.${table}`;
  const queryClient = useQueryClient();
  const realtime = useRealtime();
  const isTable = !object || (object.kind === "table" && object.editable);
  const dbParams = database ? { database } : undefined;
  const structure = useGetTable(databaseId, branchId, schema, table, dbParams, { query: { enabled: isTable } });
  const types = useListTypes(databaseId, branchId, dbParams);
  const tableDef = structure.data;
  const pk = useMemo(() => (tableDef ? primaryKeyOf(tableDef) : []), [tableDef]);
  // The page reads every column by name, plus the key and the comparable columns as text (S9, W4, R18).
  const textRead = useMemo(() => {
    if (!tableDef || pk.length === 0) return null;
    const catalog = new Map((types.data?.types ?? []).map((t) => [t.oid, t]));
    const descs = columnDescs(tableDef, types.data?.types ?? []);
    const composite = new Set(tableDef.columns.filter((c) => catalog.get(c.type_oid)?.kind === "composite").map((c) => c.name));
    const originals = descs.filter((c) => !pk.includes(c.name) && !composite.has(c.name) && isComparable(c.desc)).map((c) => c.name);
    const names = descs.map((c) => c.name);
    return {
      ...textSelectItems({ columns: names, pk, originals }),
      // The read of single too-large rows: only the small columns.
      small: (small: string[]) => textSelectItems({ columns: small, pk, originals: originals.filter((c) => small.includes(c)) }),
    };
  }, [tableDef, types.data, pk]);
  // Columns added in the pending changes get a placeholder track in the grid, under their header chip.
  const { projected } = useTableStructure(target, { enabled: isTable });
  const pendingColumns = useMemo(() => (projected ? projected.columns.filter((c) => c.original === null).map((c) => c.name) : []), [projected]);
  const headers = useColumnHeaders(target, chips, { enabled: isTable });

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
  /** Rows read with the text items split into the rows, their keys and their originals. */
  const split = (raw: Row[], small: string[] | null): Fresh => {
    if (!textRead) return { rows: raw, keys: raw.map(() => null), originals: raw.map(() => ({})) };
    return splitTextRows(raw, small ? textRead.small(small) : textRead);
  };
  const selectItems = (small: string[] | null) => (textRead ? (small ? textRead.small(small) : textRead).items : small ?? undefined);
  const rowsQuery = useQuery({
    queryKey,
    queryFn: async (): Promise<Page> => {
      const mine = sizeView;
      let n = (size.current?.view === mine ? size.current.n : null) ?? pageSize ?? 1;
      for (;;) {
        try {
          const tree = buildSelect({ schema, table, page: 0, offset, sort, filters: where, pk, pageSize: n, columns: selectItems(null) });
          const read = split((await send(tree)) as Row[], null);
          if (currentSizeView.current === mine) size.current = { view: mine, n };
          return { ...read, size: n, offset, small: null, read: ++pageReads };
        } catch (err) {
          if (!isTooLarge(err)) throw err;
          if (n <= 1) {
            // One row is still too large: read it without its large columns.
            const small = tableDef ? smallColumns(tableDef) : [];
            if (!tableDef || small.length >= tableDef.columns.length) throw err;
            const tree = buildSelect({ schema, table, page: 0, offset, sort, filters: where, pk, pageSize: 1, columns: selectItems(small) });
            return { ...split((await send(tree)) as Row[], small), size: 1, offset, small, read: ++pageReads };
          }
          n = Math.max(1, Math.floor(n / 2));
        }
      }
    },
    // The text items need the type catalog (composites aren't compared); a catalog error reads without it.
    enabled: pageSize !== null && (isTable ? structure.isSuccess && !types.isPending : true),
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
  const guardDiscard = useTableEditor((s) => s.guardDiscard);
  const setCollapsed = useTableEditor((s) => s.setCollapsed);
  // The row open in the row panel. A collapsed panel stays mounted on its row (its draft survives) but isn't "open":
  // the row loses its marking and its gutter button isn't pressed until the panel is expanded again.
  const panelRow = useTableEditor((s) => (s.panel.kind === "row" ? s.panel.row : null));
  const panelCollapsed = useTableEditor((s) => s.panel.collapsed);
  const openKey = panelCollapsed ? null : panelRow;
  /** The gutter's open-row button: opens the row, expands its collapsed panel, or closes it when it is open. */
  const toggleRow = (key: Key) => {
    if (panelRow !== key) openPanel("row", { row: key });
    else if (panelCollapsed) setCollapsed(false);
    else openPanel("closed");
  };
  const loader = useLoadValue(target);
  const insertDefault = useInsertDefaultRow();
  const [insertError, setInsertError] = useState<string | null>(null);
  // The columns of a page of single too-large rows that weren't read.
  const unread = useMemo(() => (page?.small ? new Set(columns.map((c) => c.name).filter((n) => !page.small!.includes(n))) : null), [page, columns]);
  useEffect(() => select({ type: "view", view }), [select, view]);
  const selection: Selection = stored.view === view ? stored.selection : { kind: "none" };
  const keys = useMemo(() => page?.keys ?? [], [page]);
  const pageKeys = useMemo(() => keys.filter((k): k is string => k !== null), [keys]);
  const selectedKeys = selection.kind === "rows" ? selection.keys : [];
  const pageSelection =
    selection.kind === "all_matching" || (pageKeys.length > 0 && pageKeys.every((k) => selectedKeys.includes(k)))
      ? "all"
      : pageKeys.some((k) => selectedKeys.includes(k))
        ? "some"
        : "none";

  const [widths, setWidths] = useState(() => readWidths(browserStorage(), target.key));
  const [notice, setNotice] = useState<string | null>(null);
  // A copy notice shown without the selection bar (a cell menu's copy) goes after a moment.
  useEffect(() => {
    if (!notice || selection.kind !== "none") return;
    const id = window.setTimeout(() => setNotice(null), NOTICE_MS);
    return () => window.clearTimeout(id);
  }, [notice, selection.kind]);
  const [deleting, setDeleting] = useState<DeleteTarget | null>(null);
  // The rows a cell menu's delete names (the clicked row, or the checked rows that include it); null: the selection's.
  const [deleteKeys, setDeleteKeys] = useState<Key[] | null>(null);
  const [deleteBusy, setDeleteBusy] = useState(false);
  // A cell menu action that failed (Set to NULL).
  const [cellError, setCellError] = useState<string | null>(null);
  const [deleteError, setDeleteError] = useState<string | null>(null);
  // After a key delete that deleted fewer rows than asked.
  const [deleteNotice, setDeleteNotice] = useState<string | null>(null);

  const text = (err: unknown, op: ConsoleOp) => consoleErrorText(err, { role, op });

  function goto(next: number) {
    select({ type: "clear" });
    setOffset(Math.max(0, next));
  }

  // ---- Live rows (§6.1) ----
  const reducedMotion = useMediaQuery("(prefers-reduced-motion: reduce)");
  const [live, setLive] = useState<LiveState>(INITIAL_LIVE);
  const [liveStatus, setLiveStatus] = useState<LiveStatus | null>(null);
  const [members, setMembers] = useState<PresenceMember[]>([]);
  // The viewer's own presence colour: the active cell's outline (sand until the viewer's own entry arrives).
  const ownSeed = selfSub ? members.find((m) => m.sub === selfSub)?.color_seed : undefined;
  const activeColor = ownSeed === undefined ? undefined : presenceColor(ownSeed);
  // Cells a splice changed, by row key (cleared after FLASH_MS).
  const [flash, setFlash] = useState<Record<Key, string[]>>({});
  const flashTimers = useRef(new Set<ReturnType<typeof setTimeout>>());
  useEffect(() => {
    const timers = flashTimers.current;
    return () => timers.forEach(clearTimeout);
  }, []);
  // What the live callbacks read: the latest page, query key and readers (the subscription outlives renders).
  const latest = useRef({ page, queryKey, placeholder: rowsQuery.isPlaceholderData, readKeys: async (_keys: Key[]): Promise<Key[]> => [] as Key[], refetchPage: () => {} });

  /** Puts re-read rows in place of the page's rows with the same keys; marks the cells whose value changed. */
  function splice(fresh: Fresh, forKey: readonly unknown[]) {
    const changed: Record<Key, string[]> = {};
    queryClient.setQueryData<Page>(forKey, (old) => {
      if (!old) return old;
      const at = new Map(fresh.keys.map((k, i) => [k, i]));
      let touched = false;
      const rowsNext = old.rows.map((r, i) => {
        const k = old.keys[i];
        const j = k === null ? undefined : at.get(k);
        if (k === null || j === undefined) return r;
        touched = true;
        const cols = Object.keys(fresh.rows[j]).filter((c) => editText(fresh.rows[j][c]) !== editText(r[c]) || (fresh.rows[j][c] === null) !== (r[c] === null));
        if (cols.length > 0) changed[k] = cols;
        return fresh.rows[j];
      });
      if (!touched) return old;
      const originalsNext = old.originals.map((o, i) => {
        const j = old.keys[i] === null ? undefined : at.get(old.keys[i]!);
        return j === undefined ? o : fresh.originals[j];
      });
      return { ...old, rows: rowsNext, originals: originalsNext };
    });
    if (Object.keys(changed).length === 0) return;
    setFlash((f) => ({ ...f, ...changed }));
    const timer = setTimeout(() => {
      flashTimers.current.delete(timer);
      setFlash((f) => Object.fromEntries(Object.entries(f).filter(([k]) => !(k in changed))));
    }, FLASH_MS);
    flashTimers.current.add(timer);
  }

  /** One read of these keys as the viewer's role, within the view's filter; spliced into the page it was asked for. */
  async function readKeys(list: Key[]): Promise<Key[]> {
    const forKey = queryKey;
    const small = page?.small ?? null;
    const byKeys = whereByTextKeys(pk, list);
    const tree = buildSelect({ schema, table, page: 0, offset: 0, sort: null, filters: where ? { and: [where, byKeys] } : byKeys, pk, pageSize: list.length, columns: selectItems(small) });
    const fresh = split((await send(tree)) as Row[], small);
    // Another view (role, filter, sort, page) came meanwhile: its rows aren't these.
    if (JSON.stringify(latest.current.queryKey) === JSON.stringify(forKey) && !latest.current.placeholder) splice(fresh, forKey);
    const read = fresh.keys.filter((k): k is Key => k !== null);
    // A row open in the row panel is read again too: its untouched fields show the new values (a draft's own fields
    // keep the viewer's edits).
    for (const k of read) void queryClient.invalidateQueries({ queryKey: panelRowKey(target, k), exact: true });
    return read;
  }

  /** The page and the filtered count, read again; the row open in the row panel too (a missed change may be its). */
  const refetchPage = () => {
    void queryClient.invalidateQueries({ queryKey: latest.current.queryKey, exact: true });
    void queryClient.invalidateQueries({ queryKey: [COUNT_QUERY, databaseId, branchId] });
    void queryClient.invalidateQueries({ queryKey: panelRowKey(target, null).slice(0, -1) });
  };
  latest.current = { page, queryKey, placeholder: rowsQuery.isPlaceholderData, readKeys, refetchPage };

  const controller = useRef<LiveController | null>(null);
  const subscription = useRef<TableSubscription | null>(null);
  const joinable = !!realtime && isTable;
  useEffect(() => {
    if (!realtime || !joinable) return;
    const c = createLiveController({
      getPageKeys: () => new Set((latest.current.placeholder ? [] : (latest.current.page?.keys ?? [])).filter((k): k is Key => k !== null)),
      refetchKeys: (list) => latest.current.readKeys(list),
      refetchPage: () => latest.current.refetchPage(),
      onChange: setLive,
    });
    controller.current = c;
    const sub = realtime.joinTable(
      { database_id: databaseId, branch_id: branchId, pg_database: target.pgDatabase, schema, table, role },
      {
        onJoined: (info) => {
          setLiveStatus({ live: info.live, reason: info.reason });
          c.dispatch({ type: "joined", live: info.live });
        },
        onStatus: (s) => {
          setLiveStatus(s);
          // The socket is down: nobody's presence is known until the rejoin's `presence_state`.
          if (s.reason === "reconnecting") setMembers([]);
          c.dispatch({ type: "status", live: s.live });
        },
        // Only a lost channel pauses live updates (review I1): `forbidden`/`not_found`. Any other error
        // (a refused frame, a limit, a transient refusal the client retries) keeps the state it finds,
        // and only says why when there is no state yet (a join refused before any answer).
        onError: (err) =>
          setLiveStatus((s) => (err.code === "forbidden" || err.code === "not_found" || s === null ? { live: false, reason: err.code } : s)),
        onRows: (f) => c.dispatch({ type: "rows", seq: f.seq, changes: f.changes }),
        onResync: () => c.dispatch({ type: "resync" }),
        onPresenceState: setMembers,
        onPresenceDiff: (d) => setMembers((m) => applyPresenceDiff(m, d)),
      },
    );
    subscription.current = sub;
    // The page already shown (an Act-as switch keeps no rows, so this is this join's own page).
    const shown = latest.current.page;
    if (shown && !latest.current.placeholder) {
      const onScreen = shown.keys.filter((k): k is Key => k !== null);
      sentView.current = JSON.stringify(onScreen);
      sub.view(onScreen);
    }
    return () => {
      sub.leave();
      c.dispose();
      subscription.current = null;
      controller.current = null;
      setLive(INITIAL_LIVE);
      setLiveStatus(null);
      setMembers([]);
      sentPresence.current = "";
      sentView.current = "";
    };
  }, [realtime, joinable, databaseId, branchId, target.pgDatabase, schema, table, role]);

  // A new read of the page clears the chip, strike-throughs and fades.
  const pageRead = page?.read;
  useEffect(() => {
    if (pageRead !== undefined) controller.current?.dispatch({ type: "page_loaded" });
  }, [pageRead]);

  // R19: the keys on screen.
  const sentView = useRef("");
  useEffect(() => {
    const sub = subscription.current;
    if (!sub || page === undefined || rowsQuery.isPlaceholderData) return;
    const text = JSON.stringify(pageKeys);
    if (text === sentView.current) return;
    sentView.current = text;
    sub.view(pageKeys);
  }, [pageKeys, page, rowsQuery.isPlaceholderData, realtime, joinable]);

  // The viewer's selection, debounced (§5.1).
  const sentPresence = useRef("");
  const presenceTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  useEffect(() => () => void (presenceTimer.current && clearTimeout(presenceTimer.current)), []);
  // The viewer's own cell right now (not debounced), for the presence layer's outermost ring.
  const [selfCell, setSelfCell] = useState<{ row: Key; column: string } | null>(null);
  const onPresence = useCallback((s: { key: string | null; column: string | null; editing: boolean }) => {
    const here = s.key !== null && s.column !== null ? { row: s.key, column: s.column } : null;
    setSelfCell((cur) => (cur?.row === here?.row && cur?.column === here?.column ? cur : here));
    if (presenceTimer.current) clearTimeout(presenceTimer.current);
    presenceTimer.current = setTimeout(() => {
      presenceTimer.current = null;
      const state = { row: s.key, column: s.key === null ? null : s.column, editing: s.key !== null && s.editing };
      const text = JSON.stringify(state);
      if (!subscription.current || text === sentPresence.current) return;
      sentPresence.current = text;
      subscription.current.presence(state);
    }, PRESENCE_DEBOUNCE_MS);
  }, []);

  // ---- The conflict check (§6.3, W1) ----
  // The originals of the cell editor's row, captured when it opened: a splice never changes them.
  const opened = useRef<{ key: Key; originals: Originals } | null>(null);
  const [conflict, setConflict] = useState<Conflict | null>(null);
  const [saveNotice, setSaveNotice] = useState<string | null>(null);
  // Rows being read again after a save of the viewer's own (one entry per pending read).
  const [rereading, setRereading] = useState<Key[]>([]);
  const originalsOf = (key: Key): Originals => {
    const i = keys.indexOf(key);
    return i >= 0 ? (page?.originals[i] ?? {}) : {};
  };

  /** The row by its key, unfiltered (the conflict check's re-read). */
  async function readRow(key: Key): Promise<Fresh> {
    const small = page?.small ?? null;
    const tree = buildSelect({ schema, table, page: 0, sort: null, filters: whereByTextKey(pk, key), pk, pageSize: 1, columns: selectItems(small) });
    return split((await send(tree)) as Row[], small);
  }

  /** After a row write: the row's own texts (key, originals) read again in the background, the panel and the count refreshed. */
  function afterWrite(key: Key, result: Row[]) {
    const i = keys.indexOf(key);
    if (i >= 0 && result[0]) {
      const typed = result[0];
      queryClient.setQueryData<Page>(queryKey, (old) => (old ? { ...old, rows: old.rows.map((r, j) => (old.keys[j] === key ? { ...r, ...typed } : r)) } : old));
    }
    // Until its texts are back the row's originals are the ones just overwritten: no editor opens on it.
    setRereading((r) => [...r, key]);
    void readKeys([key])
      .catch(() => undefined)
      .finally(() => setRereading((r) => {
        const i = r.indexOf(key);
        return i < 0 ? r : [...r.slice(0, i), ...r.slice(i + 1)];
      }));
    void queryClient.invalidateQueries({ queryKey: [ROWS_QUERY, databaseId, branchId, PANEL_ROW] });
    // The edit may move the row in or out of the filter.
    void queryClient.invalidateQueries({ queryKey: [COUNT_QUERY, databaseId, branchId] });
  }

  /**
   * Saves one cell with the conflict check. `fresh`: the originals are the page's now (a menu's Set to NULL), never
   * an editor's captured ones, which may belong to an editor of that row closed without a save.
   */
  async function saveCell(key: string, column: RowColumn, value: CellValue, fresh = false): Promise<string | null> {
    // The row is found by its key at save time, wherever a refetch moved it.
    if (rowsQuery.isPlaceholderData || !keys.includes(key)) return "That row is no longer on this page.";
    let parsed: unknown = null;
    if (!value.null) {
      if (temporalKind(column.desc) === "timestamptz" && !validTimestamptz(value.text)) {
        return `${column.name}: enter a date and time with its offset, like 2026-10-01 12:00:00+00.`;
      }
      const p = parseValue(column.desc, value.text);
      if (!p.ok) return `${column.name}: ${p.error}.`;
      parsed = p.value;
    }
    // W1: the originals from when the editor opened, never from the (live) page now.
    const own = !fresh && opened.current?.key === key;
    const originals = own && opened.current ? opened.current.originals : originalsOf(key);
    let check: { where: Tree; unchecked: string[] };
    try {
      check = conflictWhere({ pk, key, edited: [column.name], originals });
    } catch (err) {
      return err instanceof Error ? err.message : "Something went wrong";
    }
    const set = { [column.name]: parsed };
    try {
      setSaveNotice(null);
      const result = (await send({ v: 1, op: "update", table: qualified, set, where: check.where, returning: "*" })) as Row[];
      if (!Array.isArray(result)) return "No row was changed (it may be hidden from this role, or gone).";
      if (result.length === 0) {
        // Someone changed the row since the editor opened, or it is gone for this role: read it again.
        const fresh = await readRow(key);
        const there = fresh.keys.indexOf(key);
        const yours = value.null ? null : value.text;
        setConflict({
          key,
          set,
          fresh: there >= 0 ? fresh : null,
          unchecked: check.unchecked,
          busy: false,
          error: null,
          state:
            there < 0
              ? { kind: "gone" }
              : { kind: "changed", columns: [{ name: column.name, theirs: theirText(fresh, there, column.name), yours }] },
        });
        return null;
      }
      if (own) opened.current = null;
      afterWrite(key, result);
      if (check.unchecked.length > 0) setSaveNotice(`${NO_CHECK_NOTICE}: ${check.unchecked.join(", ")}.`);
      return null;
    } catch (err) {
      return text(err, "update");
    }
  }

  async function overwrite() {
    if (!conflict) return;
    const { key, set } = conflict;
    setConflict({ ...conflict, busy: true, error: null });
    try {
      const result = (await send({ v: 1, op: "update", table: qualified, set, where: whereByTextKey(pk, key), returning: "*" })) as Row[];
      if (!Array.isArray(result) || result.length === 0) {
        setConflict((c) => (c ? { ...c, busy: false, state: { kind: "gone" } } : c));
        return;
      }
      setConflict(null);
      afterWrite(key, result);
    } catch (err) {
      setConflict((c) => (c ? { ...c, busy: false, error: text(err, "update") } : c));
    }
  }

  function takeTheirs() {
    if (conflict?.fresh) splice(conflict.fresh, queryKey);
    setConflict(null);
  }

  /** "Insert with all defaults": one row of defaults, then its panel. */
  async function insertDefaults() {
    setInsertError(null);
    try {
      const res = await insertDefault.mutateAsync({ id: databaseId, branchId, schema, table, data: { database, role } });
      await Promise.all(rowWriteKeys(databaseId, branchId).map((queryKey) => queryClient.invalidateQueries({ queryKey })));
      // The panel opens the row by its text key (read back when the returned values aren't their texts).
      const key = await insertedKey({ schema, table, pk, row: res.row as Row, types: Object.fromEntries(columns.map((c) => [c.name, c.desc])), send }).catch(() => null);
      if (key !== null) {
        seedPanelRow(queryClient, target, key, res.row as Row);
        openPanel("row", { row: key });
      }
    } catch (err) {
      setInsertError(text(err, "insert"));
    }
  }

  function chosenRows(): Row[] {
    if (selection.kind === "all_matching") return rows;
    return rows.filter((_, i) => keys[i] !== null && selectedKeys.includes(keys[i]!));
  }
  const chosenKeys = (): Key[] => keys.filter((k): k is Key => k !== null && selectedKeys.includes(k));

  async function copy(kind: "tsv" | "json", list: Row[], cols: string[]) {
    const numeric = new Set(columns.filter((c) => inputKind(c.desc) === "int" || inputKind(c.desc) === "float").map((c) => c.name));
    const payload = kind === "tsv" ? toTsv(cols, list) : toJson(list, numeric);
    try {
      await navigator.clipboard.writeText(payload);
      setNotice(`Copied ${list.length} ${list.length === 1 ? "row" : "rows"}${kind === "json" ? " as JSON" : ""}`);
    } catch {
      setNotice("The clipboard isn't available here.");
    }
  }

  /** One cell's value as its text (NULL: empty), as Ctrl+C copies it. */
  async function copyCell(value: unknown) {
    try {
      await navigator.clipboard.writeText(editText(value));
    } catch {
      setNotice("The clipboard isn't available here.");
    }
  }

  /** Set to NULL from the cell menu: the edit save path and its conflict check, with the page's originals. */
  async function setNull(key: Key, column: RowColumn) {
    setCellError(null);
    const failure = await saveCell(key, column, { null: true }, true);
    if (failure) setCellError(failure);
  }

  /** The body-cell context menu: Copy | Edit cell | Open row | Filter by this value | Set to NULL | Delete. */
  function cellMenu({ row: i, column, key, edit }: CellMenuTarget): CellMenuGroups {
    const row = rows[i];
    if (!row) return [];
    const value = row[column.name];
    const isNull = value === null || value === undefined;
    // The checked rows act together when they include this row; otherwise the menu is about this row alone.
    const checked = key !== null && selection.kind === "rows" && selectedKeys.includes(key) ? chosenKeys() : null;
    const list = checked ? rows.filter((_, j) => keys[j] !== null && checked.includes(keys[j]!)) : [row];
    const what = list.length === 1 ? "row" : `${list.length} rows`;
    const names = columns.map((c) => c.name);
    // Added after the applied filters (AND); offered only when the filter bar would take it. `json` (and `json[]`)
    // has no equality operator in Postgres; `jsonb` has.
    const next = [...filters, isNull ? { col: column.name, op: "is" as const, value: "null" } : { col: column.name, op: "eq" as const, value: editText(value) }];
    const built = /^json(\[\])?$/i.test(column.desc.type.trim()) ? null : buildFilterTree({ mode: "and", rows: next }, Object.fromEntries(columns.map((c) => [c.name, c.desc])));
    return [
      [
        { label: "Copy cell", onSelect: () => void copyCell(value) },
        { label: `Copy ${what} (TSV)`, onSelect: () => void copy("tsv", list, names) },
        { label: `Copy ${what} (JSON)`, onSelect: () => void copy("json", list, names) },
      ],
      edit ? [{ label: "Edit cell", onSelect: edit, takesFocus: true }] : [],
      key !== null ? [{ label: "Open row", onSelect: () => openPanel("row", { row: key }) }] : [],
      built?.ok
        ? [
            {
              label: "Filter by this value",
              onSelect: () => {
                setFilters(next);
                setWhere(built.where ?? null);
                setOffset(0);
              },
            },
          ]
        : [],
      edit && key !== null && column.desc.nullable && !isNull ? [{ label: "Set to NULL", onSelect: () => void setNull(key, column) }] : [],
      writable && key !== null
        ? [
            {
              label: `Delete ${what}`,
              destructive: true,
              takesFocus: true,
              onSelect: () => {
                setDeleteError(null);
                setDeleteKeys(checked ?? [key]);
                setDeleting({ kind: "rows", count: list.length });
              },
            },
          ]
        : [],
    ];
  }

  async function confirmDelete() {
    if (!deleting) return;
    setDeleteError(null);
    setDeleteNotice(null);
    const chosen = deleting.kind === "rows" ? (deleteKeys ? deleteKeys.filter((k) => keys.includes(k)) : chosenKeys()) : [];
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
        tree = { v: 1, op: "batch", ops: chosen.map((k) => ({ op: "delete", from: qualified, where: whereByTextKey(pk, k), returning: pk })) };
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
    // A cell menu's delete of a row that isn't checked leaves the checked rows checked.
    const keepSelection = deleteKeys !== null && !deleteKeys.some((k) => selectedKeys.includes(k));
    setDeleteKeys(null);
    if (!keepSelection) select({ type: "clear" });
    if (wholePage && offset > 0) setOffset(Math.max(0, offset - (page?.size ?? pageSize ?? 1)));
    // The count's selection was cleared: its query is unobserved, so only the rows are read again.
    await queryClient.invalidateQueries({ queryKey: [ROWS_QUERY, databaseId, branchId] });
    if (keepSelection) await queryClient.invalidateQueries({ queryKey: [COUNT_QUERY, databaseId, branchId] });
  }

  const failure = rowsQuery.error ?? structure.error;
  const ownerUnknown = failure instanceof ApiError && failure.code === "owner_password_unknown";
  // The range of the rows on screen (a stand-in page keeps its own range until the next arrives).
  const shownOffset = page?.offset ?? offset;
  const from = shownOffset + 1;
  // The rows on screen that still exist: a struck-through row (deleted live, kept until the next read) doesn't count,
  // in the footer's range as in the totals.
  const struckShown = keys.filter((k) => k !== null && live.struck.includes(k)).length;
  const liveShown = rows.length - struckShown;
  const lastLive = shownOffset + liveShown;
  // The estimate never reads below the live rows on screen (a stale estimate, a live insert shown).
  const estimate = object?.row_estimate != null ? Math.max(object.row_estimate, lastLive) : null;
  const setShownRows = useTableEditor((s) => s.setShownRows);
  useEffect(() => {
    setShownRows(target.key, page === undefined ? null : lastLive);
  }, [page, lastLive, setShownRows, target.key]);
  useEffect(() => () => setShownRows(target.key, null), [setShownRows, target.key]);
  // The rows on screen, for the row panel's discard prompt ("Discard changes to row N?").
  const setPageRows = useTableEditor((s) => s.setPageRows);
  useEffect(() => {
    setPageRows({ table: target.key, keys, first: from });
  }, [setPageRows, target.key, keys, from]);
  useEffect(() => () => setPageRows(null), [setPageRows]);
  const full = page !== undefined && rows.length >= page.size;
  // A page shorter than the page size ends the rows: the view's exact total (a stand-in page proves nothing,
  // nor an empty page past the first, which may follow deletes).
  const exact = page !== undefined && !rowsQuery.isPlaceholderData && !full && (rows.length > 0 || shownOffset === 0) ? lastLive : null;
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
      {liveStatus && <LiveBadge status={liveStatus} />}
      <AvatarStack members={members} selfSub={selfSub} />
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
            <MenuItem onClick={() => guardDiscard(() => void insertDefaults())}>Insert with all defaults</MenuItem>
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
      {saveNotice && (
        <p role="status" className="border-b border-line px-4 py-2 text-sm text-chalk-dim">
          {saveNotice}
        </p>
      )}
      {live.newKeys.length > 0 && (
        <div role="status" className="flex items-center gap-2 border-b border-line px-4 py-1.5 text-sm text-chalk">
          <span aria-hidden="true" className="size-2 shrink-0 bg-seafoam" />
          <span>
            {live.newKeys.length} new {live.newKeys.length === 1 ? "row" : "rows"}
          </span>
          <span aria-hidden="true" className="text-chalk-dim">
            ·
          </span>
          <button
            type="button"
            onClick={refetchPage}
            className="px-1 text-chalk underline underline-offset-3 outline-none hover:text-sand focus-visible:outline-2 focus-visible:outline-sand"
          >
            Show
          </button>
        </div>
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
            setDeleteKeys(null);
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
      {selection.kind === "none" && notice && (
        <p role="status" className="border-b border-line px-4 py-2 text-sm text-chalk-dim">
          {notice}
        </p>
      )}
      {cellError && (
        <p role="alert" className="border-b border-line px-4 py-2 text-sm text-coral">
          {cellError}
        </p>
      )}
      {page === undefined && !failure && (
        <p role="status" className="px-4 py-2 text-sm text-chalk-dim">
          Loading rows…
        </p>
      )}
      {columns.length > 0 && (
          <Grid
            headers={headers}
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
            onSort={(col, dir) => {
              setOffset(0);
              setSort(dir ? { col, dir } : null);
            }}
            canEdit={(col, key) =>
              writable &&
              !rowsQuery.isPlaceholderData &&
              !pk.includes(col) &&
              !unread?.has(col) &&
              !columns.find((c) => c.name === col)?.readOnly &&
              (key === undefined || !rereading.includes(key))
            }
            onSave={saveCell}
            onCopy={(active) => {
              const list = chosenRows();
              if (list.length > 0) void copy("tsv", list, columns.map((c) => c.name));
              else if (active) void copyCell(rows[active.row]?.[active.column]);
            }}
            cellMenu={cellMenu}
            onOpenRow={(i) => (keys[i] ? toggleRow(keys[i]) : undefined)}
            openKey={openKey}
            activeColor={activeColor}
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
            onEditStart={(key) => {
              opened.current = { key, originals: originalsOf(key) };
            }}
            marks={{
              changed: (key, column) => flash[key]?.includes(column) ?? false,
              deleted: (key) => live.struck.includes(key),
              fading: (key) => live.fading.includes(key),
            }}
            motion={reducedMotion ? "none" : "flash"}
            editNotice={(key, column) => editingNotice(members, selfSub, key, column)}
            onPresence={joinable ? onPresence : undefined}
            overlay={joinable ? (geometry) => <PresenceLayer members={members} selfSub={selfSub} rowKeys={keys} geometry={geometry} selfCell={selfCell} /> : undefined}
            footer={
              writable && (
                <button
                  type="button"
                  onClick={() => openPanel("row", { row: null })}
                  className="flex h-9 w-full items-center gap-1.5 border-b border-line px-3 text-[13px] whitespace-nowrap text-chalk-dim outline-none hover:bg-chalk/[0.045] hover:text-chalk focus-visible:outline-2 focus-visible:-outline-offset-2 focus-visible:outline-sand"
                >
                  <Plus aria-hidden="true" size={14} strokeWidth={1.5} />
                  Add row
                </button>
              )
            }
          />
      )}
      {conflict && (
        <ConflictDialog
          open
          state={conflict.state}
          unchecked={conflict.unchecked}
          busy={conflict.busy}
          error={conflict.error}
          onOverwrite={() => void overwrite()}
          onTakeTheirs={takeTheirs}
          onClose={() => setConflict(null)}
        />
      )}
      <div className="flex h-10 shrink-0 items-center justify-between border-t border-line-strong px-4 text-[13px] text-chalk-dim">
        <span>
          {page === undefined
            ? ""
            : liveShown === 0
              ? shownOffset === 0
                ? "No rows"
                : "No more rows"
              : `Rows ${from.toLocaleString("en-US")}–${lastLive.toLocaleString("en-US")} of ${total}`}
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
            if (open) return;
            setDeleting(null);
            setDeleteKeys(null);
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
