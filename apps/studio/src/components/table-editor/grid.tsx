import { Fragment, useCallback, useEffect, useId, useMemo, useRef, useState, type KeyboardEvent, type MouseEvent, type ReactNode } from "react";
import { columnResizingFeature, columnSizingFeature, tableFeatures, useTable, type ColumnDef, type ColumnSizingState } from "@tanstack/react-table";
import { defaultRangeExtractor, useVirtualizer, type Range } from "@tanstack/react-virtual";
import { ArrowDown, ArrowUp, PanelRightOpen } from "lucide-react";
import type { Sort } from "@/lib/console";
import { cellText, defaultWidth, editText, type Row, type RowColumn } from "@/lib/table-editor/rows";
import { cn } from "@/lib/utils";
import { CellEditor, type CellValue, type Move } from "./cell-editor";
import { Checkbox } from "./panel-bits";

/** Rows are 36px (DESIGN.md density); the header row too. */
export const ROW_HEIGHT = 36;
/** The gutter: a checkbox, the row number and the open-row button. */
export const GUTTER_WIDTH = 88;
/** Columns are virtualized above this many (S2). */
export const COLUMN_VIRTUALIZE_ABOVE = 40;
const MIN_WIDTH = 64;
/** The width of the track of a column added in the pending changes (not in the table yet). */
export const PENDING_WIDTH = 160;
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
  onSort: (column: string) => void;
  /** Whether a column's cells can be edited in place. */
  canEdit: (column: string) => boolean;
  /** Saves one cell of the row with this key; resolves to an error text, or null once saved. */
  onSave: (rowKey: string, column: RowColumn, value: CellValue) => Promise<string | null>;
  /** Changes when the rows shown stop being the same view (table, role, filter, sort, page): an open editor closes. */
  resetKey?: string;
  /** Rendered sticky above the grid, inside the same scroll container (the column strip): shares its template and columns. */
  above?: (layout: GridLayout) => ReactNode;
  /** Ctrl/Cmd+C: the active cell (null on the header or gutter). */
  onCopy: (active: { row: number; column: string } | null) => void;
  /** Enter on a row's gutter. */
  onOpenRow?: (rowIndex: number) => void;
  /** The key of the row open in the row panel: its row is marked. */
  openKey?: string | null;
  /** A cell's content in place of its text (a large value left unread), or undefined for the text. */
  renderCell?: (row: Row, column: RowColumn, rowIndex: number) => ReactNode | undefined;
  widths: Record<string, number>;
  onWidths: (next: Record<string, number>) => void;
  /** Columns added in the pending changes: a header and empty placeholder cells after the table's columns. */
  pending?: readonly string[];
}

/** The grid's column geometry, for content that lines up with it (the column strip). */
export interface GridLayout {
  /** The CSS grid-template-columns of every row: the gutter, each data column, then each pending column. */
  template: string;
  totalWidth: number;
  /** The indexes (into `columns`) of the data columns rendered now, ascending. */
  visibleCols: number[];
}

interface Pos {
  /** -1 is the header row. */
  row: number;
  /** 0 is the gutter; data columns start at 1. */
  col: number;
}

interface Editing {
  /** The edited row's key: the row is found by it, wherever it moves. */
  key: string;
  col: number;
  initial: string;
  busy: boolean;
  error: string | null;
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
  const template = `${GUTTER_WIDTH}px ${[...sizes, ...pending.map(() => PENDING_WIDTH)].map((s) => `${s}px`).join(" ")}`;
  const totalWidth = GUTTER_WIDTH + sizes.reduce((a, b) => a + b, 0) + pending.length * PENDING_WIDTH;
  // A pending column's empty cells: dashed, faintly sand, outside the grid's keyboard model.
  const placeholders = pending.map((name, j) => (
    <div key={`+${name}`} aria-hidden="true" className="h-9 border-l border-dashed border-sand/30 bg-sand/[0.03]" style={{ gridColumn: columns.length + 2 + j }} />
  ));

  // The height of `above`, from its ResizeObserver (0 until measured).
  const aboveRef = useRef<HTMLDivElement>(null);
  const [aboveHeight, setAboveHeight] = useState(0);
  const hasAbove = !!props.above;
  useEffect(() => {
    const el = aboveRef.current;
    if (!el || typeof ResizeObserver === "undefined") return;
    const ro = new ResizeObserver((entries) => {
      const h = entries[0]?.contentRect.height;
      if (typeof h === "number") setAboveHeight(Math.round(h));
    });
    ro.observe(el);
    return () => ro.disconnect();
  }, [hasAbove]);
  const keysRef = useRef(keys);
  keysRef.current = keys;
  const rowMargin = aboveHeight + ROW_HEIGHT;

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
  const layout: GridLayout = { template, totalWidth, visibleCols };

  // Keep the active cell within the page when rows or columns change.
  useEffect(() => {
    setActive((a) => {
      const row = Math.min(a.row, rows.length - 1);
      const col = Math.min(a.col, columns.length);
      return row === a.row && col === a.col ? a : { row, col };
    });
  }, [rows.length, columns.length]);

  // Another view (table, role, filter, sort, page) puts other rows under the editor: drop it.
  useEffect(() => setEditing(null), [props.resetKey, props.firstRowNumber, sort]);
  // A refetch that dropped the edited row closes its editor.
  useEffect(() => {
    setEditing((e) => (e && !e.busy && !keys.includes(e.key) ? null : e));
  }, [keys]);
  const indexOf = (key: string) => keysRef.current.indexOf(key);

  const cellEl = (p: Pos) => scrollRef.current?.querySelector<HTMLElement>(`[data-cell="${p.row}:${p.col}"]`) ?? null;

  function focusCell(p: Pos) {
    setActive(p);
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

  function startEdit(p: Pos, seed?: string) {
    if (p.row < 0 || p.col < 1) return;
    const column = columns[p.col - 1];
    const key = keys[p.row];
    if (!column || key === null || key === undefined || !props.canEdit(column.name)) return;
    setActive(p);
    setEditing({ key, col: p.col, initial: seed ?? editText(rows[p.row]?.[column.name]), busy: false, error: null });
  }

  async function commit(value: CellValue, move: Move) {
    if (!editing) return;
    const column = columns[editing.col - 1];
    const at = indexOf(editing.key);
    const original = at >= 0 ? rows[at]?.[column.name] : undefined;
    const unchanged =
      at >= 0 && (value.null ? original === null || original === undefined : value.text === editing.initial && editing.initial === editText(original));
    const { key, col: editCol } = editing;
    let failure: string | null = null;
    if (!unchanged) {
      setEditing({ ...editing, busy: true, error: null });
      failure = await props.onSave(key, column, value);
    }
    if (failure) {
      setEditing((e) => (e && e.key === key ? { ...e, busy: false, error: failure } : e));
      return;
    }
    setEditing(null);
    const step = move === "next" ? 1 : move === "prev" ? -1 : 0;
    const col = Math.max(1, Math.min(columns.length, editCol + step));
    const row = indexOf(key);
    if (row >= 0) focusCell({ row, col });
  }

  function cancel() {
    if (!editing) return;
    const row = indexOf(editing.key);
    const col = editing.col;
    setEditing(null);
    if (row >= 0) focusCell({ row, col });
  }

  function onKeyDown(e: KeyboardEvent<HTMLDivElement>) {
    if (editing || (e.target as HTMLElement).closest("[data-editor]")) return;
    const lastRow = rows.length - 1;
    const lastCol = columns.length;
    const mod = e.ctrlKey || e.metaKey;
    const move = (p: Pos) => {
      e.preventDefault();
      focusCell({ row: Math.max(-1, Math.min(lastRow, p.row)), col: Math.max(0, Math.min(lastCol, p.col)) });
    };
    const { row, col } = active;
    if (e.altKey && row === -1 && col > 0 && (e.key === "ArrowLeft" || e.key === "ArrowRight")) {
      e.preventDefault();
      const name = columns[col - 1].name;
      const size = sizes[col - 1] + (e.key === "ArrowRight" ? RESIZE_STEP : -RESIZE_STEP);
      onWidths({ ...widths, [name]: Math.max(MIN_WIDTH, Math.min(MAX_WIDTH, size)) });
      return;
    }
    switch (e.key) {
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
        return move(mod ? { row: lastRow, col: lastCol } : { row, col: lastCol });
      case "PageDown":
        return move({ row: row + 10, col });
      case "PageUp":
        return move({ row: Math.max(row - 10, 0), col });
      case "Enter":
      case "F2":
        e.preventDefault();
        if (row === -1) {
          if (col > 0) props.onSort(columns[col - 1].name);
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
        if (row === -1) props.onTogglePage();
        else if (keys[row]) props.onToggleRow(keys[row]!);
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

  const virtualRows = rowVirtualizer.getVirtualItems();
  return (
    <div ref={scrollRef} data-grid-scroll="" className="relative min-h-0 flex-1 overflow-auto bg-ink">
      {props.above && (
        <div ref={aboveRef} className="sticky top-0 z-30" style={{ width: totalWidth }}>
          {props.above(layout)}
        </div>
      )}
      <span id={resizeHelpId} hidden>
        Alt+Left or Alt+Right resizes the column. Click its edge to widen it a step, Shift+click to narrow it; double-click the edge to reset its width.
      </span>
      <div role="grid" aria-label={label} aria-rowcount={rows.length + 1} aria-colcount={columns.length + 1 + pending.length} onKeyDown={onKeyDown}>
      <div role="rowgroup" className="sticky z-20" style={{ width: totalWidth, top: aboveHeight }}>
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
            const p = { row: -1, col: i + 1 };
            return (
              <Fragment key={c.name}>
                <div
                  role="columnheader"
                  aria-describedby={resizeHelpId}
                  aria-sort={dir === "asc" ? "ascending" : dir === "desc" ? "descending" : "none"}
                  {...cellProps(p)}
                  onClick={() => {
                    setActive(p);
                    props.onSort(c.name);
                  }}
                  style={{ gridColumn: i + 2, gridRow: 1 }}
                  className={cn(cellBase, "cursor-pointer gap-1 select-none hover:text-chalk", dir && "text-chalk")}
                >
                  <span className="truncate font-mono">{c.name}</span>
                  {dir === "asc" && <ArrowUp aria-hidden="true" size={12} strokeWidth={1.5} className="shrink-0 text-chalk" />}
                  {dir === "desc" && <ArrowDown aria-hidden="true" size={12} strokeWidth={1.5} className="shrink-0 text-chalk" />}
                </div>
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
            <div
              key={`+${name}`}
              role="columnheader"
              aria-colindex={columns.length + 2 + j}
              title="Added in your pending changes"
              style={{ gridColumn: columns.length + 2 + j, gridRow: 1 }}
              className="flex h-9 min-w-0 items-center border-t-2 border-r border-t-sand/45 border-r-line px-2 text-chalk-dim"
            >
              <span className="truncate font-mono">{name}</span>
            </div>
          ))}
        </div>
      </div>
      <div role="rowgroup" className="relative" style={{ height: rowVirtualizer.getTotalSize(), width: totalWidth }}>
        {virtualRows.map((v) => {
          const r = v.index;
          const row = rows[r];
          const key = keys[r];
          const selected = key !== null && props.isSelected(key);
          const zebra = r % 2 === 1;
          const open = key !== null && props.openKey === key;
          return (
            <div
              key={v.key}
              role="row"
              aria-rowindex={r + 2}
              aria-selected={key !== null ? selected : undefined}
              className={cn(
                "group/row absolute top-0 left-0 grid h-9 border-b border-line font-mono text-xs",
                selected ? "bg-sand/[0.08]" : zebra && "bg-chalk/[0.045]",
                open && "bg-channel shadow-[inset_2px_0_0_var(--sand)]",
              )}
              style={{ transform: `translateY(${v.start - rowMargin}px)`, gridTemplateColumns: template, width: totalWidth }}
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
                return (
                  <div
                    key={c.name}
                    role="gridcell"
                    aria-readonly={props.canEdit(c.name) ? undefined : true}
                    {...cellProps(p)}
                    onDoubleClick={() => startEdit(p)}
                    style={{ gridColumn: i + 2 }}
                    className={cn(cellBase, "relative", isNull ? "text-chalk-dim" : "text-chalk", edit && "overflow-visible")}
                  >
                    {edit ? (
                      <CellEditor column={c} initial={edit.initial} busy={edit.busy} error={edit.error} onCommit={(val, mv) => void commit(val, mv)} onCancel={cancel} />
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
      </div>
      </div>
    </div>
  );
}
