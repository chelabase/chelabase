import { Fragment, useCallback, useEffect, useId, useMemo, useRef, useState, type KeyboardEvent, type MouseEvent, type PointerEvent as ReactPointerEvent, type ReactNode } from "react";
import { ContextMenu } from "@base-ui/react/context-menu";
import { Tooltip } from "@base-ui/react/tooltip";
import { columnResizingFeature, columnSizingFeature, tableFeatures, useTable, type ColumnDef, type ColumnSizingState } from "@tanstack/react-table";
import { defaultRangeExtractor, useVirtualizer, type Range } from "@tanstack/react-virtual";
import { ArrowDown, ArrowUp, MoreHorizontal, PanelRightOpen } from "lucide-react";
import type { Sort, SortDir } from "@/lib/console";
import { cellText, defaultWidth, editText, type Row, type RowColumn } from "@/lib/table-editor/rows";
import { cn } from "@/lib/utils";
import { Menu, MenuItem, MenuSeparator } from "@/components/ui/menu";
import { CellEditor, type CellValue, type Move } from "./cell-editor";
import { CellMenuPopup, type CellMenuGroups, type CellMenuItem } from "./cell-menu";
import { Checkbox } from "./panel-bits";
import type { PresenceGeometry } from "./presence-layer";

/** The z-index of the body row holding the open editor: above the other rows, below the sticky headers (z-20, z-30). */
const EDITING_ROW_Z = 10;

/** "Changed since you started": the value the row holds now, on hover; the field keeps the viewer's edit. */
function ChangedMarker({ value }: { value: string | null }) {
  return (
    <Tooltip.Root>
      <Tooltip.Trigger
        delay={150}
        render={
          <span
            data-testid="changed-marker"
            tabIndex={-1}
            onMouseDown={(e) => e.preventDefault()}
            className="w-max border border-seafoam/60 bg-deep px-2 py-0.5 font-sans text-[11px] text-seafoam"
          />
        }
      >
        Changed since you started
      </Tooltip.Trigger>
      <Tooltip.Portal>
        <Tooltip.Positioner side="bottom" sideOffset={4} className="z-50">
          <Tooltip.Popup role="tooltip" className="max-w-80 border border-line-strong bg-deep px-2 py-1 text-xs text-chalk">
            Now: <span className={cn("font-mono break-all whitespace-pre-wrap", value === null && "text-chalk-dim")}>{value === null ? "NULL" : cellText(value)}</span>
          </Tooltip.Popup>
        </Tooltip.Positioner>
      </Tooltip.Portal>
    </Tooltip.Root>
  );
}

/** Rows are 36px (DESIGN.md density); the header row too. */
export const ROW_HEIGHT = 36;
/** The gutter: a checkbox, the row number and the open-row button. */
export const GUTTER_WIDTH = 88;
/** Columns are virtualized above this many (S2). */
export const COLUMN_VIRTUALIZE_ABOVE = 40;
const MIN_WIDTH = 64;
/** The width of the track of a column added in the pending changes (not in the table yet). */
export const PENDING_WIDTH = 160;
/** The track after the last column that holds `headers.end`. */
export const END_WIDTH = 136;
const MAX_WIDTH = 960;
const RESIZE_STEP = 16;

export interface GridProps {
  /** The grid's accessible name: the qualified table name. */
  label: string;
  columns: RowColumn[];
  rows: Row[];
  /** The row number the first row shows (the page offset + 1). */
  firstRowNumber: number;
  /** Each row's selection key; null when rows can't be selected (no primary key). */
  keys: (string | null)[];
  isSelected: (key: string) => boolean;
  /** How much of the page is selected, for the header checkbox. */
  pageSelection: "none" | "some" | "all";
  onToggleRow: (key: string) => void;
  onTogglePage: () => void;
  sort: Sort | null;
  /** Sorts by a column in a direction, or (null) clears the sort. */
  onSort: (column: string, dir: SortDir | null) => void;
  /** Whether a column's cells can be edited in place; with `rowKey`, whether that row's cell can be now. */
  canEdit: (column: string, rowKey?: string) => boolean;
  /** Saves one cell of the row with this key; resolves to an error text, or null once saved. */
  onSave: (rowKey: string, column: RowColumn, value: CellValue) => Promise<string | null>;
  /** Changes when the rows shown stop being the same view (table, role, filter, sort, page): an open editor closes. */
  resetKey?: string;
  /** What the header cells show in place of the plain name (the column chips); without it, a header's click cycles the sort. */
  headers?: GridHeaders | null;
  /** Ctrl/Cmd+C: the active cell (null on the header or gutter). */
  onCopy: (active: { row: number; column: string } | null) => void;
  /** Enter on a row's gutter. */
  onOpenRow?: (rowIndex: number) => void;
  /** The key of the row open in an expanded row panel: its row is marked and its open-row button pressed. */
  openKey?: string | null;
  /** The viewer's own presence colour (a CSS colour): the active cell's outline; undefined keeps it sand. */
  activeColor?: string;
  /** A cell's content in place of its text (a large value left unread), or undefined for the text. */
  renderCell?: (row: Row, column: RowColumn, rowIndex: number) => ReactNode | undefined;
  widths: Record<string, number>;
  onWidths: (next: Record<string, number>) => void;
  /** Columns added in the pending changes: a header and empty placeholder cells after the table's columns. */
  pending?: readonly string[];
  /** An editor opened on a row's cell (the conflict check captures its originals then, W1). */
  onEditStart?: (rowKey: string, column: string) => void;
  /** Live rows (§6.1): cells a splice changed, rows a keyed delete struck through, rows that left the view. */
  marks?: GridMarks;
  /** The splice flash: `none` under `prefers-reduced-motion`. */
  motion?: "flash" | "none";
  /** A line under an open editor (another member editing the same cell), or null. */
  editNotice?: (rowKey: string, column: string) => string | null;
  /** The viewer's own selection, the active cell (presence, §5.1), whatever holds focus; all null once it clears. */
  onPresence?: (state: { key: string | null; column: string | null; editing: boolean }) => void;
  /** Drawn over the rows (the presence layer), given where rows and columns sit. */
  overlay?: (geometry: PresenceGeometry) => ReactNode;
  /** The groups of a body cell's context menu; without it, a right-click leaves the browser's own menu. */
  cellMenu?: (cell: CellMenuTarget) => CellMenuGroups;
}

/** The body cell a context menu opened on. */
export interface CellMenuTarget {
  /** The row's index on the page. */
  row: number;
  column: RowColumn;
  /** The row's key; null without a primary key. */
  key: string | null;
  /** Opens the cell's editor, as a double-click does; null when the cell can't be edited. */
  edit: (() => void) | null;
}

export interface GridMarks {
  changed: (rowKey: string, column: string) => boolean;
  deleted: (rowKey: string) => boolean;
  fading: (rowKey: string) => boolean;
}

/** One header cell's content (a column chip). */
export interface HeaderView {
  /** The header's accessible name: the column's name as it will be. */
  label: string;
  /** Read after the name: the full type and marks ("bigint, new"). */
  description?: string;
  /** The cell's content beside the sort arrow. */
  content: ReactNode;
  /** A tooltip (why the column can't be changed). */
  title?: string;
  /** A click or Enter on the header; null does nothing (structure is read-only). */
  onActivate: (() => void) | null;
  /** Items after the sort items in the header's `⋯` menu. */
  menu?: ReactNode;
  className?: string;
}

/** The header cells' content, by column: the table's columns by their name now, the pending columns by theirs. */
export interface GridHeaders {
  column: (name: string) => HeaderView | undefined;
  pending: (name: string) => HeaderView | undefined;
  /** In the header row after the last column ("Add column"): outside the grid's cells and keys. */
  end?: ReactNode;
}

interface Pos {
  /** -1 is the header row. */
  row: number;
  /** 0 is the gutter; data columns start at 1. */
  col: number;
}

/** The active cell: a body cell, with its row's key (it follows the row when a refetch moves it). */
interface ActiveCell extends Pos {
  key: string | null;
}

const isBodyCell = (p: Pos) => p.row >= 0 && p.col > 0;
/** The position in a `data-cell` attribute. */
function cellPos(el: Element | null): Pos | null {
  const at = el?.closest("[data-cell]")?.getAttribute("data-cell");
  if (!at) return null;
  const [row, col] = at.split(":").map(Number);
  return { row, col };
}
/** Opening the menu from the keyboard (Shift+F10, the ContextMenu key). */
const isMenuKey = (e: KeyboardEvent) => e.key === "ContextMenu" || (e.shiftKey && e.key === "F10");
/** How long after a menu key the browser's own `contextmenu` event belongs to it. */
const MENU_KEY_MS = 1000;

interface Editing {
  /** The edited row's key: the row is found by it, wherever it moves. */
  key: string;
  col: number;
  initial: string;
  /** The value's text when the editor opened (null: NULL): a live splice never replaces it, it is marked instead. */
  opened: string | null;
  busy: boolean;
  error: string | null;
  /** Which opening of an editor this is: a save that finishes after its editor closed (or another opened) is stale. */
  gen: number;
}

/** A body cell by its row's key (it follows the row), for where a press or a queued edit goes once a save lands. */
interface CellTarget {
  key: string | null;
  row: number;
  col: number;
}

const features = tableFeatures({ columnSizingFeature, columnResizingFeature });

const cellBase = "flex h-9 min-w-0 items-center border-r border-line px-2 outline-none focus-visible:outline-2 focus-visible:-outline-offset-2 focus-visible:outline-sand";

/**
 * The rows grid: an ARIA grid over one page of rows, virtualized by row (and by column above 40),
 * with sticky header and gutter, zebra rows, a hairline on every column edge, a sand resize grip,
 * spreadsheet keys and in-place editing.
 */
export function Grid(props: GridProps) {
  const { label, columns, rows, keys, sort, widths, onWidths } = props;
  const scrollRef = useRef<HTMLDivElement>(null);
  const [active, setActive] = useState<Pos>({ row: rows.length > 0 ? 0 : -1, col: 1 });
  const [editing, setEditing] = useState<Editing | null>(null);
  const editingRef = useRef(editing);
  editingRef.current = editing;
  // The open editor's "leave" (see CellEditor's `leaveRef`): a pointer-down outside it commits it.
  const leaveEditor = useRef<((move: "out" | "press") => boolean) | null>(null);
  // Counts editor openings (`Editing.gen`).
  const editGen = useRef(0);
  // The body cell pressed while an editor was open elsewhere: it becomes active once that editor's save lands.
  const pressTo = useRef<CellTarget | null>(null);
  // A cell double-clicked (or opened from the menu) while a save was in flight: its editor opens once the save lands.
  const queuedEdit = useRef<CellTarget | null>(null);
  // Set by a press on another cell while editing: the press's mousedown doesn't move focus out of the field (the
  // save's outcome decides where focus goes).
  const holdFocus = useRef(false);
  // The active cell, drawn for its owner and published as presence: apart from the roving tabindex position
  // (`active`), which only says where focus goes. Null until a cell is clicked or reached by the keys.
  const [activeCell, setActiveCell] = useState<ActiveCell | null>(null);
  const activeCellRef = useRef(activeCell);
  activeCellRef.current = activeCell;
  // The body cell the context menu is open on.
  const [menuAt, setMenuAt] = useState<Pos | null>(null);
  const menuOpen = useRef(false);
  menuOpen.current = menuAt !== null;
  // Where focus goes when the menu closes: its cell, unless the user pressed elsewhere or the item took focus.
  const menuCell = useRef<HTMLElement | null>(null);
  const menuReturn = useRef(true);
  // When a menu key was last pressed (the browser follows it with its own `contextmenu` event).
  const menuKeyAt = useRef(-Infinity);
  const pendingFocus = useRef<Pos | null>(null);
  // Where a press on a resize grip started: a click that didn't drag steps the width.
  const gripDown = useRef<number | null>(null);
  const resizeHelpId = useId();

  // Column sizes live in Table state, seeded from the saved widths.
  const defs = useMemo<ColumnDef<typeof features, Row>[]>(
    () =>
      columns.map((c) => ({
        id: c.name,
        accessorFn: (r: Row) => r[c.name],
        header: c.name,
        size: defaultWidth(c.desc),
        minSize: MIN_WIDTH,
        maxSize: MAX_WIDTH,
      })),
    [columns],
  );
  const table = useTable({
    features,
    columns: defs,
    data: rows,
    columnResizeMode: "onChange",
    enableColumnResizing: true,
    state: { columnSizing: widths },
    onColumnSizingChange: (updater) => {
      const next = typeof updater === "function" ? (updater as (s: ColumnSizingState) => ColumnSizingState)(widths) : updater;
      onWidths(next);
    },
  });
  const headers = table.getHeaderGroups()[0]?.headers ?? [];
  const sizes = headers.map((h) => h.getSize());
  const pending = props.pending ?? [];
  const headerViews = props.headers ?? null;
  const endWidth = headerViews?.end ? END_WIDTH : 0;
  const template = `${GUTTER_WIDTH}px ${[...sizes, ...pending.map(() => PENDING_WIDTH), ...(endWidth ? [endWidth] : [])].map((s) => `${s}px`).join(" ")}`;
  const totalWidth = GUTTER_WIDTH + sizes.reduce((a, b) => a + b, 0) + pending.length * PENDING_WIDTH + endWidth;
  // A pending column's empty cells: dashed, faintly sand, outside the grid's keyboard model.
  const placeholders = pending.map((name, j) => (
    <div key={`+${name}`} aria-hidden="true" className="h-9 border-l border-dashed border-sand/30 bg-sand/[0.03]" style={{ gridColumn: columns.length + 2 + j }} />
  ));

  const keysRef = useRef(keys);
  keysRef.current = keys;
  const rowsRef = useRef(rows);
  rowsRef.current = rows;
  // The rows start under the one header row.
  const rowMargin = ROW_HEIGHT;

  const activeRow = active.row;
  const rowVirtualizer = useVirtualizer({
    count: rows.length,
    getScrollElement: () => scrollRef.current,
    estimateSize: () => ROW_HEIGHT,
    // Rows keep their identity (and an open editor) when a refetch reorders them.
    getItemKey: (i) => keys[i] ?? `#${i}`,
    scrollMargin: rowMargin,
    overscan: 8,
    initialRect: { width: 1200, height: 720 },
    // The active row stays mounted, so focus isn't lost when it scrolls away.
    rangeExtractor: useCallback(
      (range: Range) => {
        const base = defaultRangeExtractor(range);
        return activeRow >= 0 && activeRow < range.count && !base.includes(activeRow) ? [...base, activeRow].sort((a, b) => a - b) : base;
      },
      [activeRow],
    ),
  });
  const manyColumns = columns.length > COLUMN_VIRTUALIZE_ABOVE;
  const colVirtualizer = useVirtualizer({
    count: columns.length,
    horizontal: true,
    enabled: manyColumns,
    getScrollElement: () => scrollRef.current,
    estimateSize: (i) => sizes[i] ?? 160,
    scrollMargin: GUTTER_WIDTH,
    overscan: 4,
    initialRect: { width: 1200, height: 720 },
  });
  useEffect(() => {
    if (manyColumns) colVirtualizer.measure();
    // Sizes change on resize.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [template]);
  const visibleCols = manyColumns ? colVirtualizer.getVirtualItems().map((v) => v.index) : columns.map((_, i) => i);
  if (manyColumns && active.col > 0 && !visibleCols.includes(active.col - 1)) {
    visibleCols.push(active.col - 1);
    visibleCols.sort((a, b) => a - b);
  }
  // Where rows and columns sit inside the rows' group: row i at i × ROW_HEIGHT, columns after the gutter.
  const geometry: PresenceGeometry = {
    rowHeight: ROW_HEIGHT,
    offsetTop: 0,
    columnBox: (name) => {
      const i = columns.findIndex((c) => c.name === name);
      if (i < 0 || sizes[i] === undefined) return null;
      return { left: GUTTER_WIDTH + sizes.slice(0, i).reduce((a, b) => a + b, 0), width: sizes[i] };
    },
  };

  // Keep the active cell within the page when rows or columns change.
  useEffect(() => {
    setActive((a) => {
      const row = Math.min(a.row, rows.length - 1);
      // The header row also reaches the pending columns' headers.
      const col = Math.min(a.col, a.row === -1 ? columns.length + pending.length : columns.length);
      return row === a.row && col === a.col ? a : { row, col };
    });
    setActiveCell((s) => (s && (s.row >= rows.length || s.col > columns.length) ? null : s));
    setMenuAt((m) => (m && (m.row >= rows.length || m.col > columns.length) ? null : m));
  }, [rows.length, columns.length, pending.length]);

  // Another view (table, role, filter, sort, page) puts other rows under the editor and the active cell: drop them.
  useEffect(() => {
    setEditing(null);
    setActiveCell(null);
    setMenuAt(null);
    pressTo.current = null;
    queuedEdit.current = null;
  }, [props.resetKey, props.firstRowNumber, sort]);
  // A refetch that dropped the edited row closes its editor; the active cell follows its row, or clears with it.
  useEffect(() => {
    setEditing((e) => (e && !e.busy && !keys.includes(e.key) ? null : e));
    const s = activeCellRef.current;
    if (!s || s.key === null) return;
    const at = keys.indexOf(s.key);
    if (at === s.row) return;
    setActiveCell(at < 0 ? null : { ...s, row: at });
    setMenuAt(null);
    setActive((a) => (at >= 0 && a.row === s.row && a.col === s.col ? { row: at, col: s.col } : a));
  }, [keys]);
  const indexOf = (key: string) => keysRef.current.indexOf(key);

  /** Makes a body cell the active cell; any other position (the header, the gutter) clears it. */
  function select(p: Pos) {
    setActiveCell(isBodyCell(p) ? { ...p, key: keysRef.current[p.row] ?? null } : null);
  }

  // The viewer's own selection, for presence: the active cell, the edited cell while editing.
  const onPresence = useRef(props.onPresence);
  onPresence.current = props.onPresence;
  const presenceKey = editing ? editing.key : activeCell ? (keys[activeCell.row] ?? null) : null;
  const presenceColumn = editing ? (columns[editing.col - 1]?.name ?? null) : presenceKey !== null && activeCell ? (columns[activeCell.col - 1]?.name ?? null) : null;
  const presenceEditing = !!editing;
  useEffect(() => {
    onPresence.current?.({ key: presenceKey, column: presenceKey === null ? null : presenceColumn, editing: presenceKey !== null && presenceEditing });
  }, [presenceKey, presenceColumn, presenceEditing]);

  const cellEl = (p: Pos) => scrollRef.current?.querySelector<HTMLElement>(`[data-cell="${p.row}:${p.col}"]`) ?? null;

  function focusCell(p: Pos) {
    setActive(p);
    select(p);
    const el = cellEl(p);
    if (el) {
      el.focus();
      return;
    }
    pendingFocus.current = p;
    if (p.row >= 0) rowVirtualizer.scrollToIndex(p.row);
    if (manyColumns && p.col > 0) colVirtualizer.scrollToIndex(p.col - 1);
  }
  useEffect(() => {
    const p = pendingFocus.current;
    if (!p) return;
    const el = cellEl(p);
    if (el) {
      pendingFocus.current = null;
      el.focus();
    }
  });

  /** Sets the editor now: `editingRef` too, so a press or a finishing save in the same tick sees it. */
  function putEditing(next: Editing | null) {
    editingRef.current = next;
    setEditing(next);
  }

  function startEdit(p: Pos, seed?: string) {
    if (p.row < 0 || p.col < 1) return;
    const column = columns[p.col - 1];
    const key = keysRef.current[p.row];
    if (!column || key === null || key === undefined || !props.canEdit(column.name, key)) return;
    const open = editingRef.current;
    if (open) {
      // Already open there: it keeps what was typed (and a save in flight).
      if (open.key === key && open.col === p.col) return;
      // A save in flight: this cell's editor opens once it lands, and not at all if it fails.
      if (open.busy) {
        queuedEdit.current = { key, row: p.row, col: p.col };
        return;
      }
      // A refused save keeps its editor, with its error, until it is saved again or cancelled.
      if (open.error !== null) return;
    }
    setActive(p);
    select(p);
    const value = rowsRef.current[p.row]?.[column.name];
    const opened = value === null || value === undefined ? null : editText(value);
    pressTo.current = null;
    queuedEdit.current = null;
    putEditing({ key, col: p.col, initial: seed ?? editText(value), opened, busy: false, error: null, gen: ++editGen.current });
    props.onEditStart?.(key, column.name);
  }

  async function commit(value: CellValue, move: Move) {
    const editing = editingRef.current;
    if (!editing) return;
    const column = columns[editing.col - 1];
    const at = indexOf(editing.key);
    // Unchanged against what the editor opened with: a value spliced in meanwhile isn't the viewer's edit.
    const unchanged = at >= 0 && (value.null ? editing.opened === null : value.text === editing.initial && editing.initial === (editing.opened ?? ""));
    const { key, col: editCol, gen } = editing;
    let failure: string | null = null;
    if (!unchanged) {
      putEditing({ ...editing, busy: true, error: null });
      failure = await props.onSave(key, column, value);
      // Its editor closed meanwhile (another view, the row gone): the outcome moves nothing, closes nothing.
      if (editingRef.current?.gen !== gen) return;
    }
    if (failure) {
      // The editor stays where it was, with the error; a press or an edit waiting on this save is dropped.
      pressTo.current = null;
      queuedEdit.current = null;
      putEditing(editingRef.current && { ...editingRef.current, busy: false, error: failure });
      return;
    }
    const to = pressTo.current;
    const queued = queuedEdit.current;
    pressTo.current = null;
    queuedEdit.current = null;
    putEditing(null);
    if (queued) {
      // A double-click elsewhere while saving: that cell's editor opens now.
      const row = queued.key !== null ? indexOf(queued.key) : -1;
      if (row >= 0) {
        startEdit({ row, col: queued.col });
        return;
      }
    }
    if (to) {
      // A press on another cell: it is the active cell, focused, whatever the save's own move was.
      const row = to.key !== null ? indexOf(to.key) : to.row;
      if (row >= 0 && row < rowsRef.current.length) focusCell({ row, col: to.col });
      else setActiveCell(null);
      return;
    }
    if (move === "out" || move === "press") {
      // Left by a pointer-down outside the body cells: nothing stays active, and focus stays where it went.
      setActiveCell(null);
      return;
    }
    const step = move === "next" ? 1 : move === "prev" ? -1 : 0;
    const col = Math.max(1, Math.min(columns.length, editCol + step));
    const row = indexOf(key);
    if (row >= 0) focusCell({ row, col });
  }

  function cancel() {
    const editing = editingRef.current;
    if (!editing) return;
    const row = indexOf(editing.key);
    const col = editing.col;
    pressTo.current = null;
    queuedEdit.current = null;
    putEditing(null);
    if (row >= 0) focusCell({ row, col });
  }

  /**
   * A primary press on a body cell. With an editor open on another cell, that editor is left first (an unchanged
   * value closes, a changed one saves); the pressed cell becomes active once it has, and stays inactive if the save
   * fails (the editor, its error and focus stay). The editor's own cell keeps everything as it is.
   */
  function pressCell(e: ReactPointerEvent<HTMLDivElement>, p: Pos) {
    holdFocus.current = false;
    const open = editingRef.current;
    if (!open || e.button !== 0) return select(p);
    if (indexOf(open.key) === p.row && open.col === p.col) return;
    pressTo.current = { key: keysRef.current[p.row] ?? null, row: p.row, col: p.col };
    if (leaveEditor.current) {
      // Focus stays in the field until the outcome is known (`commit` moves it to the pressed cell).
      holdFocus.current = true;
      if (leaveEditor.current("press") || open.busy) return;
      // The value a failed save just tried: the editor stays until it is changed or cancelled.
      pressTo.current = null;
      return;
    }
    // No editor mounted for it (its row scrolled out): nothing typed to save, unless a save is in flight.
    if (open.busy) return;
    pressTo.current = null;
    putEditing(null);
    select(p);
  }

  /** The body cell of this grid under an event (beneath the open menu's backdrop too); null for anything else. */
  function bodyCellAt(e: globalThis.MouseEvent): Pos | null {
    const grid = scrollRef.current;
    let el = e.target instanceof Element ? e.target : null;
    if (grid && menuOpen.current && !(el && grid.contains(el)) && !el?.closest("[data-cell-menu]") && typeof document.elementsFromPoint === "function") {
      el = document.elementsFromPoint(e.clientX, e.clientY).find((under) => grid.contains(under)) ?? el;
    }
    if (!grid || !el || !grid.contains(el)) return null;
    const p = cellPos(el);
    return p && p.row >= 0 ? p : null;
  }

  // A pointer-down outside the body cells clears the active cell: the gutter (row selection) and the menu keep it.
  // While editing, it commits the editor first (like its blur, an unchanged value included); the active cell clears
  // once that succeeds, and stays (with the editor and its error) when the save fails.
  useEffect(() => {
    const onDown = (e: PointerEvent) => {
      if (!activeCellRef.current && !editingRef.current) return;
      if (e.target instanceof Element && e.target.closest("[data-cell-menu]")) return;
      if (bodyCellAt(e)) return;
      if (editingRef.current) {
        // A press outside overrides one on a cell, or an edit, waiting on a save in flight.
        pressTo.current = null;
        queuedEdit.current = null;
        if (leaveEditor.current) {
          leaveEditor.current("out");
          return;
        }
        // No editor mounted for it (its row scrolled out): nothing typed to save, unless a save is in flight.
        if (editingRef.current.busy) return;
        putEditing(null);
      }
      setActiveCell(null);
    };
    document.addEventListener("pointerdown", onDown, true);
    return () => document.removeEventListener("pointerdown", onDown, true);
    // `bodyCellAt` reads refs only.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  /** Opens the menu on a body cell: it becomes the active cell, and an editor open elsewhere closes. */
  function openMenu(p: Pos): boolean {
    if (!props.cellMenu || !isBodyCell(p) || !rows[p.row] || !columns[p.col - 1]) return false;
    // A changed value is saved by the editor's blur already (busy); an unchanged one closes without a save.
    if (editing && !editing.busy && !(indexOf(editing.key) === p.row && editing.col === p.col)) putEditing(null);
    setActive(p);
    select(p);
    setMenuAt(p);
    menuCell.current = cellEl(p);
    menuReturn.current = true;
    return true;
  }

  // While the menu is open, a right-click lands on its backdrop: the cell beneath gets the menu.
  useEffect(() => {
    if (menuAt === null) return;
    const onContext = (e: globalThis.MouseEvent) => {
      const grid = scrollRef.current;
      if (!grid || (e.target instanceof Element && (grid.contains(e.target) || e.target.closest("[data-cell-menu]")))) return;
      const p = bodyCellAt(e);
      const el = p && cellEl(p);
      if (!el) return;
      e.preventDefault();
      e.stopPropagation();
      el.dispatchEvent(new globalThis.MouseEvent("contextmenu", { bubbles: true, cancelable: true, clientX: e.clientX, clientY: e.clientY, button: 2 }));
    };
    document.addEventListener("contextmenu", onContext, true);
    return () => document.removeEventListener("contextmenu", onContext, true);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [menuAt]);

  /** A right-click (or a long press) in the rows: a body cell's menu; anything else keeps the browser's own menu. */
  function onRowsContextMenu(e: MouseEvent<HTMLDivElement> & { preventBaseUIHandler: () => void }) {
    const native = () => {
      // Neither the menu nor its document listener: the browser shows its own menu.
      e.preventBaseUIHandler();
      e.stopPropagation();
    };
    const target = e.target as HTMLElement;
    // The field being edited keeps its own menu (paste, spelling).
    if (target.closest("[data-editor]")) return native();
    // The browser's `contextmenu` after Shift+F10 or the ContextMenu key: the key already opened the menu (or nothing).
    if (e.button !== 2 && e.nativeEvent.isTrusted && performance.now() - menuKeyAt.current < MENU_KEY_MS) {
      e.preventDefault();
      e.preventBaseUIHandler();
      e.stopPropagation();
      return;
    }
    const p = cellPos(target);
    if (!p || !openMenu(p)) return native();
  }

  function onMenuOpenChange(open: boolean, details: ContextMenu.Root.ChangeEventDetails) {
    if (open) {
      // A long press opens without a `contextmenu` event: the cell comes from the touch.
      const p = details.event?.target instanceof Element && !details.event.target.closest("[data-editor]") ? cellPos(details.event.target) : null;
      if (!p || !openMenu(p)) details.cancel();
      return;
    }
    if (details.reason === "outside-press" || details.reason === "focus-out") menuReturn.current = false;
    setMenuAt(null);
  }

  /** Shift+F10 or the ContextMenu key: the menu of the active cell, under it. */
  function openMenuFromKeys() {
    const s = activeCellRef.current;
    const el = s && cellEl(s);
    if (!el) return;
    const box = el.getBoundingClientRect();
    el.dispatchEvent(new globalThis.MouseEvent("contextmenu", { bubbles: true, cancelable: true, clientX: box.left, clientY: box.bottom, button: 2 }));
  }

  function pickMenuItem(item: CellMenuItem) {
    if (item.takesFocus) menuReturn.current = false;
    item.onSelect();
  }

  const menuColumn = menuAt ? columns[menuAt.col - 1] : undefined;
  const menuKey = menuAt ? (keys[menuAt.row] ?? null) : null;
  const menuGroups: CellMenuGroups =
    menuAt && menuColumn && props.cellMenu && rows[menuAt.row]
      ? props.cellMenu({
          row: menuAt.row,
          column: menuColumn,
          key: menuKey,
          edit: menuKey !== null && props.canEdit(menuColumn.name, menuKey) ? () => startEdit(menuAt) : null,
        })
      : [];

  /** Cycles a column's sort: none, ascending, descending, none. */
  function cycleSort(name: string) {
    props.onSort(name, sort?.col !== name ? "asc" : sort.dir === "asc" ? "desc" : null);
  }

  /** A click or Enter on header column `col` (data columns, then pending ones): its chip's action, or the sort. */
  function activateHeader(col: number) {
    const c = columns[col - 1];
    if (c) {
      const view = headerViews?.column(c.name);
      if (view) view.onActivate?.();
      else cycleSort(c.name);
      return;
    }
    const name = pending[col - columns.length - 1];
    if (name !== undefined) headerViews?.pending(name)?.onActivate?.();
  }

  function onKeyDown(e: KeyboardEvent<HTMLDivElement>) {
    // Keys in a portalled header menu bubble here through React: they're the menu's.
    if (!e.currentTarget.contains(e.target as Node)) return;
    if (editing || (e.target as HTMLElement).closest("[data-editor]")) return;
    // A header's `⋯` button (focused by a click) opens its menu itself.
    if ((e.target as HTMLElement).closest("[data-column-menu]") && (e.key === "Enter" || e.key === " ")) return;
    if (isMenuKey(e)) {
      e.preventDefault();
      if (active.row === -1) {
        // On a column header: its `⋯` menu.
        cellEl(active)?.querySelector<HTMLElement>("[data-column-menu]")?.click();
        return;
      }
      menuKeyAt.current = performance.now();
      openMenuFromKeys();
      return;
    }
    const lastRow = rows.length - 1;
    const lastCol = columns.length;
    // The header row also holds the pending columns' headers.
    const lastHeaderCol = columns.length + pending.length;
    const mod = e.ctrlKey || e.metaKey;
    const move = (p: Pos) => {
      e.preventDefault();
      const row = Math.max(-1, Math.min(lastRow, p.row));
      focusCell({ row, col: Math.max(0, Math.min(row === -1 ? lastHeaderCol : lastCol, p.col)) });
    };
    const { row, col } = active;
    if (e.altKey && row === -1 && col > 0 && col <= columns.length && (e.key === "ArrowLeft" || e.key === "ArrowRight")) {
      e.preventDefault();
      const name = columns[col - 1].name;
      const size = sizes[col - 1] + (e.key === "ArrowRight" ? RESIZE_STEP : -RESIZE_STEP);
      onWidths({ ...widths, [name]: Math.max(MIN_WIDTH, Math.min(MAX_WIDTH, size)) });
      return;
    }
    switch (e.key) {
      case "Escape":
        // Nothing is being edited here (the editor takes its own Escape): the active cell clears.
        if (activeCell) {
          e.preventDefault();
          setActiveCell(null);
        }
        return;
      case "ArrowUp":
        return move({ row: row - 1, col });
      case "ArrowDown":
        return move({ row: row + 1, col });
      case "ArrowLeft":
        return move({ row, col: col - 1 });
      case "ArrowRight":
        return move({ row, col: col + 1 });
      case "Home":
        return move(mod ? { row: lastRow >= 0 ? 0 : -1, col: 1 } : { row, col: 0 });
      case "End":
        return move(mod ? { row: lastRow, col: lastCol } : { row, col: row === -1 ? lastHeaderCol : lastCol });
      case "PageDown":
        return move({ row: row + 10, col });
      case "PageUp":
        return move({ row: Math.max(row - 10, 0), col });
      case "Enter":
      case "F2":
        e.preventDefault();
        if (row === -1) {
          if (col > 0) activateHeader(col);
          else props.onTogglePage();
        } else if (col === 0) {
          props.onOpenRow?.(row);
        } else {
          startEdit({ row, col });
        }
        return;
      case " ": {
        if ((e.target as HTMLElement).tagName === "INPUT") return; // the checkbox toggles itself
        e.preventDefault();
        if (row === -1) {
          if (col > 0) activateHeader(col);
          else props.onTogglePage();
        } else if (keys[row]) props.onToggleRow(keys[row]!);
        return;
      }
    }
    if (mod && e.key.toLowerCase() === "c") {
      e.preventDefault();
      props.onCopy(row >= 0 && col > 0 ? { row, column: columns[col - 1].name } : null);
      return;
    }
    // Typing on a cell starts editing it with that character.
    if (e.key.length === 1 && !mod && !e.altKey && row >= 0 && col > 0) {
      e.preventDefault();
      startEdit({ row, col }, e.key);
    }
  }

  const isActive = (p: Pos) => p.row === active.row && p.col === active.col;
  const cellProps = (p: Pos) => ({
    "data-cell": `${p.row}:${p.col}`,
    "aria-colindex": p.col + 1,
    tabIndex: isActive(p) ? 0 : -1,
    onFocus: () => {
      if (!isActive(p)) setActive(p);
    },
  });
  const pageBox = useRef<HTMLInputElement>(null);
  useEffect(() => {
    if (pageBox.current) pageBox.current.indeterminate = props.pageSelection === "some";
  }, [props.pageSelection]);
  const selectable = keys.some((k) => k !== null);

  /**
   * A column's header cell: its chip (or its plain name), the sort arrow and the `⋯` menu (sort first, then the
   * chip's items). A click or Enter does the chip's action, or cycles the sort without a chip. A pending column (not
   * in the table yet) has no sort and no resize grip.
   */
  function headerCell(p: Pos, name: string, view: HeaderView | undefined, dir: SortDir | null) {
    const sortable = p.col <= columns.length;
    const pendingCol = !sortable;
    const descId = `${resizeHelpId}-h${p.col}`;
    const description = view?.description ?? (pendingCol ? "added in your pending changes" : undefined);
    const hasMenu = sortable || !!view?.menu;
    const sortTo = (next: SortDir | null) => props.onSort(name, next);
    return (
      <div
        role="columnheader"
        aria-label={view?.label ?? name}
        aria-describedby={[description ? descId : null, sortable ? resizeHelpId : null].filter(Boolean).join(" ") || undefined}
        aria-sort={sortable ? (dir === "asc" ? "ascending" : dir === "desc" ? "descending" : "none") : undefined}
        {...cellProps(p)}
        title={view?.title ?? (pendingCol && !view ? "Added in your pending changes" : undefined)}
        onClick={(e) => {
          // A click in the header's menu (portalled, but inside it in React) isn't a click on the header.
          if (!e.currentTarget.contains(e.target as Node) || (e.target as HTMLElement).closest("[data-column-menu]")) return;
          setActive(p);
          select(p);
          activateHeader(p.col);
        }}
        style={{ gridColumn: p.col + 1, gridRow: 1 }}
        className={cn(
          cellBase,
          "group/head gap-1 pr-0 select-none",
          (view ? view.onActivate : sortable) ? "cursor-pointer hover:bg-chalk/[0.05]" : "cursor-default",
          dir && "text-chalk",
          pendingCol && !view && "border-t-2 border-t-sand/45",
          view?.className,
        )}
      >
        {description && (
          <span id={descId} hidden>
            {description}
          </span>
        )}
        <span className="flex min-w-0 flex-1 items-center gap-1">
          {view ? <span className="min-w-0 flex-1">{view.content}</span> : <span className="truncate font-mono">{name}</span>}
          {dir === "asc" && <ArrowUp aria-hidden="true" size={12} strokeWidth={1.5} className="shrink-0 text-chalk" />}
          {dir === "desc" && <ArrowDown aria-hidden="true" size={12} strokeWidth={1.5} className="shrink-0 text-chalk" />}
        </span>
        {hasMenu ? (
          <Menu
            align="start"
            finalFocus={() => cellEl(p)}
            trigger={
              <button
                type="button"
                tabIndex={-1}
                data-column-menu=""
                aria-label={`Column actions for ${view?.label ?? name}`}
                className="flex h-full w-6 shrink-0 items-center justify-center text-chalk-dim opacity-60 outline-none group-hover/head:opacity-100 hover:text-chalk focus-visible:opacity-100 focus-visible:outline-2 focus-visible:-outline-offset-2 focus-visible:outline-sand aria-expanded:opacity-100"
              >
                <MoreHorizontal aria-hidden="true" size={14} strokeWidth={1.5} />
              </button>
            }
          >
            {sortable && (
              <>
                <MenuItem onClick={() => sortTo("asc")}>Sort ascending</MenuItem>
                <MenuItem onClick={() => sortTo("desc")}>Sort descending</MenuItem>
                {dir && <MenuItem onClick={() => sortTo(null)}>Clear sort</MenuItem>}
                {view?.menu && <MenuSeparator />}
              </>
            )}
            {view?.menu}
          </Menu>
        ) : (
          <span className="w-2 shrink-0" />
        )}
      </div>
    );
  }

  const virtualRows = rowVirtualizer.getVirtualItems();
  return (
    <div ref={scrollRef} data-grid-scroll="" className="relative min-h-0 flex-1 overflow-auto bg-ink">
      {headerViews?.end && (
        // Over the header row's last track, outside the grid (it is no column): no height, so the grid starts at the top.
        <div className="sticky top-0 z-30 h-0" style={{ width: totalWidth }}>
          <div className="absolute top-0 right-0 flex h-9 items-center px-2" style={{ width: END_WIDTH }}>
            {headerViews.end}
          </div>
        </div>
      )}
      <span id={resizeHelpId} hidden>
        Alt+Left or Alt+Right resizes the column. Click its edge to widen it a step, Shift+click to narrow it; double-click the edge to reset its width. Shift+F10 opens the column menu.
      </span>
      <div
        role="grid"
        aria-label={label}
        aria-rowcount={rows.length + 1}
        aria-colcount={columns.length + 1 + pending.length}
        onKeyDown={onKeyDown}
      >
      <div role="rowgroup" className="sticky top-0 z-20" style={{ width: totalWidth }}>
        <div
          role="row"
          aria-rowindex={1}
          className="grid h-9 border-b border-line-strong bg-channel text-xs text-chalk-dim"
          style={{ gridTemplateColumns: template }}
        >
          <div role="columnheader" {...cellProps({ row: -1, col: 0 })} className={cn(cellBase, "sticky left-0 z-10 justify-center bg-channel px-0")}>
            {selectable && (
              <Checkbox
                ref={pageBox}
                tabIndex={-1}
                aria-label="Select all rows on this page"
                checked={props.pageSelection === "all"}
                onChange={props.onTogglePage}
              />
            )}
          </div>
          {visibleCols.map((i) => {
            const c = columns[i];
            const header = headers[i];
            const dir = sort?.col === c.name ? sort.dir : null;
            return (
              <Fragment key={c.name}>
                {headerCell({ row: -1, col: i + 1 }, c.name, headerViews?.column(c.name), dir)}
                {/* The grip shares the header's grid cell but sits outside it, so the header is named by its label alone. */}
                {header && (
                  <span
                    role="separator"
                    aria-describedby={resizeHelpId}
                    aria-orientation="vertical"
                    aria-label={`Resize ${c.name}`}
                    aria-valuenow={sizes[i]}
                    aria-valuemin={MIN_WIDTH}
                    aria-valuemax={MAX_WIDTH}
                    onMouseDown={(e: MouseEvent) => {
                      gripDown.current = e.clientX;
                      header.getResizeHandler()(e);
                    }}
                    onTouchStart={header.getResizeHandler()}
                    onClick={(e: MouseEvent) => {
                      e.stopPropagation();
                      // A click without dragging steps the width (Shift+click narrows): resizing without a drag (WCAG 2.5.7).
                      const moved = gripDown.current !== null && Math.abs(e.clientX - gripDown.current) > 3;
                      gripDown.current = null;
                      if (moved) return;
                      const size = sizes[i] + (e.shiftKey ? -RESIZE_STEP : RESIZE_STEP);
                      onWidths({ ...widths, [c.name]: Math.max(MIN_WIDTH, Math.min(MAX_WIDTH, size)) });
                    }}
                    onDoubleClick={(e: MouseEvent) => {
                      e.stopPropagation();
                      const { [c.name]: _reset, ...rest } = widths;
                      onWidths(rest);
                    }}
                    style={{ gridColumn: i + 2, gridRow: 1 }}
                    className={cn(
                      "relative z-10 h-full w-[5px] translate-x-[2px] cursor-col-resize justify-self-end",
                      header.column.getIsResizing() ? "bg-sand" : "hover:bg-sand",
                    )}
                  />
                )}
              </Fragment>
            );
          })}
          {pending.map((name, j) => (
            <Fragment key={`+${name}`}>{headerCell({ row: -1, col: columns.length + 1 + j }, name, headerViews?.pending(name), null)}</Fragment>
          ))}
        </div>
      </div>
      <ContextMenu.Root open={menuAt !== null} onOpenChange={onMenuOpenChange}>
      <ContextMenu.Trigger
        render={<div role="rowgroup" className="relative" style={{ height: rowVirtualizer.getTotalSize(), width: totalWidth }} />}
        onContextMenu={onRowsContextMenu}
      >
        {props.overlay?.(geometry)}
        {virtualRows.map((v) => {
          const r = v.index;
          const row = rows[r];
          const key = keys[r];
          const selected = key !== null && props.isSelected(key);
          const zebra = r % 2 === 1;
          const open = key !== null && props.openKey === key;
          const deleted = key !== null && !!props.marks?.deleted(key);
          const fading = key !== null && !!props.marks?.fading(key);
          // Each row is its own stacking context (absolute + transform): the row holding the open editor is raised so
          // what hangs out of it (the error, a tall editor) paints over the rows after it. Below the sticky headers
          // (z-20, z-30) and the portalled menus, dialogs and panels (z-50).
          const editingRow = editing !== null && key !== null && editing.key === key;
          return (
            <div
              key={v.key}
              role="row"
              aria-rowindex={r + 2}
              aria-selected={key !== null ? selected : undefined}
              data-deleted={deleted ? "true" : undefined}
              data-fading={fading ? "true" : undefined}
              data-editing={editingRow ? "true" : undefined}
              className={cn(
                "group/row absolute top-0 left-0 grid h-9 border-b border-line font-mono text-xs",
                selected ? "bg-sand/[0.08]" : zebra && "bg-chalk/[0.045]",
                open && "bg-channel shadow-[inset_2px_0_0_var(--sand)]",
                deleted && "text-chalk-dim line-through decoration-coral/70",
                fading && "opacity-40 transition-opacity duration-700 ease-design motion-reduce:transition-none",
              )}
              style={{
                transform: `translateY(${v.start - rowMargin}px)`,
                gridTemplateColumns: template,
                width: totalWidth,
                ...(editingRow ? { zIndex: EDITING_ROW_Z } : null),
              }}
            >
              <div
                role="gridcell"
                {...cellProps({ row: r, col: 0 })}
                className={cn(
                  cellBase,
                  "sticky left-0 z-10 gap-2 font-sans text-[11px] text-chalk-dim",
                  open ? "bg-channel" : selected ? "bg-[color-mix(in_oklab,var(--sand)_8%,var(--ink))]" : zebra ? "bg-[color-mix(in_oklab,var(--chalk)_4.5%,var(--ink))]" : "bg-ink",
                )}
              >
                {key !== null && (
                  <Checkbox tabIndex={-1} aria-label={`Select row ${r + 1}`} checked={selected} onChange={() => props.onToggleRow(key)} />
                )}
                <span className="min-w-[2ch]">{props.firstRowNumber + r}</span>
                {key !== null && props.onOpenRow && (
                  <button
                    type="button"
                    tabIndex={-1}
                    aria-label={`Open row ${props.firstRowNumber + r}`}
                    aria-pressed={open}
                    onClick={() => props.onOpenRow?.(r)}
                    className={cn(
                      "ml-auto flex size-5 shrink-0 items-center justify-center text-chalk-dim outline-none hover:text-sand focus-visible:opacity-100 focus-visible:outline-2 focus-visible:outline-sand",
                      !open && "opacity-0 group-hover/row:opacity-100",
                    )}
                  >
                    <PanelRightOpen aria-hidden="true" size={14} strokeWidth={1.5} />
                  </button>
                )}
              </div>
              {visibleCols.map((i) => {
                const c = columns[i];
                const p = { row: r, col: i + 1 };
                const value = row?.[c.name];
                const isNull = value === null || value === undefined;
                const edit = editing && key !== null && editing.key === key && editing.col === i + 1 ? editing : null;
                const changed = !edit && key !== null && !!props.marks?.changed(key, c.name);
                // The row changed under the open editor: its new value is shown on hover, never put in the field.
                const now = isNull ? null : editText(value);
                const moved = edit !== null && now !== edit.opened;
                const notice = edit && key !== null ? (props.editNotice?.(key, c.name) ?? null) : null;
                // The active cell: a 2px inset outline in the viewer's presence colour (sand until it is known), whatever holds focus.
                const current = activeCell !== null && activeCell.row === r && activeCell.col === i + 1;
                const ownColor = current ? props.activeColor : undefined;
                return (
                  <div
                    key={c.name}
                    role="gridcell"
                    aria-readonly={props.canEdit(c.name) ? undefined : true}
                    {...cellProps(p)}
                    onPointerDown={(e) => pressCell(e, p)}
                    onMouseDown={(e) => {
                      if (!holdFocus.current) return;
                      holdFocus.current = false;
                      e.preventDefault();
                    }}
                    onDoubleClick={() => startEdit(p)}
                    data-active={current ? "true" : undefined}
                    data-changed={changed ? "true" : undefined}
                    data-motion={changed ? (props.motion ?? "flash") : undefined}
                    style={ownColor ? { gridColumn: i + 2, outlineColor: ownColor } : { gridColumn: i + 2 }}
                    className={cn(
                      cellBase,
                      "relative",
                      isNull ? "text-chalk-dim" : "text-chalk",
                      edit && "overflow-visible",
                      current && "outline-solid outline-2 -outline-offset-2",
                      current && !ownColor && "outline-sand",
                    )}
                  >
                    {edit ? (
                      <CellEditor
                        column={c}
                        initial={edit.initial}
                        busy={edit.busy}
                        error={edit.error}
                        onCommit={(val, mv) => void commit(val, mv)}
                        onCancel={cancel}
                        leaveRef={leaveEditor}
                      >
                        {moved && <ChangedMarker value={now} />}
                        {notice && <p className="w-max max-w-80 border border-line-strong bg-deep px-2 py-1 font-sans text-xs whitespace-normal text-chalk">{notice}</p>}
                      </CellEditor>
                    ) : (
                      (props.renderCell?.(row, c, r) ?? <span className="truncate whitespace-pre">{cellText(value)}</span>)
                    )}
                  </div>
                );
              })}
              {placeholders}
            </div>
          );
        })}
      </ContextMenu.Trigger>
      <CellMenuPopup
        groups={menuGroups}
        finalFocus={() => (menuReturn.current && menuCell.current?.isConnected ? menuCell.current : false)}
        onPick={pickMenuItem}
      />
      </ContextMenu.Root>
      </div>
    </div>
  );
}
