import { useEffect, useLayoutEffect, useRef, useState, type KeyboardEvent, type MutableRefObject, type ReactNode } from "react";
import { closeBrackets } from "@codemirror/autocomplete";
import { defaultKeymap, history, historyKeymap, insertNewlineAndIndent } from "@codemirror/commands";
import { bracketMatching } from "@codemirror/language";
import { EditorState, Prec } from "@codemirror/state";
import { EditorView, keymap } from "@codemirror/view";
import { inputKind } from "@/lib/console";
import { temporalKind, type RowColumn, type TemporalKind } from "@/lib/table-editor/rows";
import { cn } from "@/lib/utils";

/** What an editor saves: a text the caller parses for the column's type, or NULL. */
export type CellValue = { null: true } | { null: false; text: string };
/**
 * Where the active cell goes after a save: nowhere, the next/previous cell (Tab, Shift+Tab), out of the grid
 * (a pointer-down outside the body cells: the editor closes, the active cell clears, focus stays where it went),
 * or to the body cell a pointer-down pressed (the grid knows which).
 */
export type Move = "none" | "next" | "prev" | "out" | "press";

export interface CellEditorProps {
  column: RowColumn;
  /** The text the editor starts from (the value, or the key that started the edit). */
  initial: string;
  busy: boolean;
  error: string | null;
  onCommit: (value: CellValue, move: Move) => void;
  onCancel: () => void;
  /**
   * Set while the editor is open: commits it with the given move ("out" or "press"), unchanged values included (the
   * caller closes the editor without a save then); true when it did. Does nothing (false) while a save is in flight or
   * for the value a failed save just tried.
   */
  leaveRef?: MutableRefObject<((move: "out" | "press") => boolean) | null>;
  /** Lines under the field (the live "changed since you started" marker, another member editing). */
  children?: ReactNode;
}

/** The editor error's width bounds, in px: 320 (20rem) with room to spare, never under 120. */
export const ERROR_MAX_WIDTH = 320;
export const ERROR_MIN_WIDTH = 120;
/** The gap the error keeps from the viewport's right edge. */
export const ERROR_EDGE_GAP = 8;

/**
 * The error's max width for a cell starting at `left` in a viewport, keeping an 8px gap from its right edge (the 120px
 * floor wins over the gap); an unmeasured viewport gives the full width.
 */
export function errorRoom(left: number, viewport: { right: number; width: number }): number {
  if (viewport.width <= 0) return ERROR_MAX_WIDTH;
  const room = Math.floor(viewport.right - left - ERROR_EDGE_GAP);
  return Math.min(ERROR_MAX_WIDTH, Math.max(ERROR_MIN_WIDTH, room));
}

const field =
  "h-full w-full border-0 bg-ink px-2 font-mono text-xs text-chalk outline-2 -outline-offset-2 outline-sand disabled:opacity-60";

/**
 * A type-aware inline editor: a select for booleans and enums, a textarea for JSON and arrays,
 * a text field otherwise. Enter saves, Escape cancels, Tab saves and moves; leaving the field
 * saves a changed value. Nullable columns offer "Set NULL".
 */
export function CellEditor({ column, initial, busy, error, onCommit, onCancel, leaveRef, children }: CellEditorProps) {
  const temporal = temporalKind(column.desc);
  // What the control shows at first: date/time inputs take their own value format.
  const shownInitial = temporal ? toInputValue(temporal, initial) : initial;
  const [text, setText] = useState(shownInitial);
  const done = useRef(false);
  const tried = useRef<string | null>(null);
  const ref = useRef<HTMLElement | null>(null);
  const setRef = (el: HTMLElement | null) => {
    ref.current = el;
  };
  const kind = inputKind(column.desc);
  const codeMirror = kind === "json" && !column.desc.array;

  useEffect(() => {
    const el = ref.current;
    if (!el) return;
    el.focus();
    if ((el instanceof HTMLInputElement && el.type === "text") || el instanceof HTMLTextAreaElement) {
      const end = el.value.length;
      el.setSelectionRange(end, end);
    }
  }, []);

  function commit(move: Move, current = text) {
    tried.current = current;
    // An untouched date/time keeps its full original value (the input may show it shortened).
    onCommit({ null: false, text: current === shownInitial ? initial : current }, move);
  }
  function cancel() {
    done.current = true;
    onCancel();
  }

  function onKeyDown(e: KeyboardEvent) {
    e.stopPropagation();
    if (busy) {
      if (e.key !== "Tab") return;
      e.preventDefault();
      return;
    }
    if (e.key === "Escape") {
      e.preventDefault();
      cancel();
    } else if (e.key === "Enter" && !(kind === "json" && e.shiftKey)) {
      e.preventDefault();
      commit("none");
    } else if (e.key === "Tab") {
      e.preventDefault();
      commit(e.shiftKey ? "prev" : "next");
    }
  }

  // Leaving the editor saves a value that changed and hasn't just been tried.
  function onBlur(current = text) {
    if (done.current || busy) return;
    if (current === shownInitial || current === tried.current) return;
    commit("none", current);
  }

  // A pointer-down outside the editor (outside the grid's body cells, or on another body cell): like leaving the
  // field, but an unchanged value closes the editor too.
  const leave = (move: "out" | "press") => {
    if (done.current || busy || text === tried.current) return false;
    commit(move);
    return true;
  };
  const leaveNow = useRef(leave);
  leaveNow.current = leave;
  useLayoutEffect(() => {
    if (!leaveRef) return;
    const run = (move: "out" | "press") => leaveNow.current(move);
    leaveRef.current = run;
    return () => {
      if (leaveRef.current === run) leaveRef.current = null;
    };
  }, [leaveRef]);

  // The error box hangs below the cell, left-aligned on it: no wider than the grid's visible room to its right.
  const boxRef = useRef<HTMLDivElement | null>(null);
  const [errorWidth, setErrorWidth] = useState(ERROR_MAX_WIDTH);
  useLayoutEffect(() => {
    if (!error) return;
    const box = boxRef.current;
    const scroller = box?.closest<HTMLElement>("[data-grid-scroll]");
    if (!box || !scroller) return setErrorWidth(ERROR_MAX_WIDTH);
    const measure = () => {
      const rect = scroller.getBoundingClientRect();
      // The visible right edge: without the vertical scrollbar when the viewport is laid out.
      const right = scroller.clientWidth > 0 ? rect.left + scroller.clientLeft + scroller.clientWidth : rect.right;
      setErrorWidth(errorRoom(box.getBoundingClientRect().left, { right, width: rect.width }));
    };
    measure();
    scroller.addEventListener("scroll", measure, { passive: true });
    window.addEventListener("resize", measure);
    return () => {
      scroller.removeEventListener("scroll", measure);
      window.removeEventListener("resize", measure);
    };
  }, [error]);

  const common = {
    "aria-label": column.name,
    "aria-invalid": error ? true : undefined,
    // Not disabled while saving: a disabled control drops focus, and a refused save keeps editing.
    "aria-busy": busy || undefined,
    onKeyDown,
    onBlur: () => onBlur(),
  };

  let control;
  if (kind === "bool" || kind === "enum") {
    const options = kind === "bool" ? ["true", "false"] : (column.labels ?? []);
    const shown = options.includes(text) || text === "" ? options : [text, ...options];
    control = (
      <select ref={setRef} value={text} onChange={(e) => !busy && setText(e.target.value)} className={field} {...common}>
        {text === "" && <option value="">Choose…</option>}
        {shown.map((o) => (
          <option key={o} value={o}>
            {o}
          </option>
        ))}
      </select>
    );
  } else if (codeMirror) {
    control = (
      <JsonEditor
        label={column.name}
        initial={shownInitial}
        invalid={!!error}
        onChange={(doc) => !busy && setText(doc)}
        onKey={(key, current) => {
          if (busy) return;
          if (key === "Escape") cancel();
          else commit(key === "Enter" ? "none" : key === "Tab" ? "next" : "prev", current);
        }}
        onBlur={(current) => onBlur(current)}
      />
    );
  } else if (temporal && temporal !== "timestamptz") {
    control = (
      <input
        ref={setRef}
        type={temporal === "date" ? "date" : temporal === "time" ? "time" : "datetime-local"}
        step={temporal === "date" ? undefined : 1}
        value={text}
        onChange={(e) => !busy && setText(e.target.value)}
        className={cn(field, "[color-scheme:dark]")}
        {...common}
      />
    );
  } else if (kind === "json") {
    control = (
      <textarea
        ref={setRef}
        value={text}
        rows={4}
        spellCheck={false}
        onChange={(e) => !busy && setText(e.target.value)}
        className={cn(field, "h-auto min-h-24 resize-y py-2")}
        {...common}
      />
    );
  } else {
    control = (
      <input
        ref={setRef}
        value={text}
        spellCheck={false}
        inputMode={kind === "int" || kind === "float" ? "decimal" : undefined}
        onChange={(e) => !busy && setText(e.target.value)}
        className={field}
        {...common}
      />
    );
  }

  return (
    <div ref={boxRef} data-editor="" className="absolute inset-x-0 top-0 z-30 flex min-h-9 flex-col">
      <div className="flex min-h-9 items-stretch bg-ink">
        <div className="min-w-0 flex-1">{control}</div>
        {column.desc.nullable && (
          <button
            type="button"
            tabIndex={-1}
            disabled={busy}
            onMouseDown={(e) => e.preventDefault()}
            onClick={() => {
              tried.current = text;
              onCommit({ null: true }, "none");
            }}
            className="shrink-0 border-l border-line px-2 text-[11px] text-chalk-dim hover:bg-chalk/[0.08] hover:text-chalk"
          >
            Set NULL
          </button>
        )}
      </div>
      {error && (
        <p role="alert" style={{ maxWidth: errorWidth }} className="w-max border border-coral/60 bg-deep px-2 py-1 font-sans text-xs whitespace-normal text-coral">
          {error}
        </p>
      )}
      {children}
    </div>
  );
}

/** A value as a date/time input takes it: `T` between date and time, at most milliseconds. */
export function toInputValue(kind: TemporalKind, value: string): string {
  const v = value.trim();
  if (kind === "date") return /^\d{4}-\d{2}-\d{2}/.test(v) ? v.slice(0, 10) : v;
  const trimmed = v.replace(/(\.\d{3})\d+/, "$1");
  return kind === "timestamp" ? trimmed.replace(" ", "T") : trimmed;
}

const jsonTheme = EditorView.theme(
  {
    "&": { fontSize: "12px", minHeight: "6rem", maxHeight: "16rem", backgroundColor: "var(--ink)", color: "var(--chalk)" },
    "&.cm-focused": { outline: "2px solid var(--sand)", outlineOffset: "-2px" },
    ".cm-scroller": { fontFamily: "var(--font-mono)", overflow: "auto" },
    ".cm-content": { padding: "6px 0" },
    ".cm-content ::selection, .cm-content::selection": { color: "var(--chalk)", backgroundColor: "transparent" },
    ".cm-selectionBackground, &.cm-focused > .cm-scroller > .cm-selectionLayer .cm-selectionBackground": { background: "rgb(242 199 163 / 0.25)" },
  },
  { dark: true },
);

interface JsonEditorProps {
  label: string;
  initial: string;
  invalid: boolean;
  onChange: (doc: string) => void;
  /**
   * The grid's keys: Enter saves, Escape cancels, Tab/Shift+Tab save and move; Shift+Enter is a new
   * line. Without it (the row panel) the keys edit as usual and the editor isn't focused on mount.
   */
  onKey?: (key: "Enter" | "Escape" | "Tab" | "Shift-Tab", doc: string) => void;
  onBlur?: (doc: string) => void;
  /** The current value when the caller controls it (the row panel): a different value replaces the document. */
  value?: string;
}

/** A small CodeMirror for a JSON value; its content is labelled by the column's name. Used by the grid and the row panel. */
export function JsonEditor({ label, initial, invalid, onChange, onKey, onBlur, value }: JsonEditorProps) {
  const host = useRef<HTMLDivElement>(null);
  const view = useRef<EditorView | null>(null);
  const handlers = useRef({ onChange, onKey, onBlur });
  handlers.current = { onChange, onKey, onBlur };
  const inGrid = onKey !== undefined;
  // Set while a controlled value is written in: that isn't the user's change.
  const syncing = useRef(false);

  useEffect(() => {
    const key = (k: "Enter" | "Escape" | "Tab" | "Shift-Tab") => (v: EditorView) => {
      handlers.current.onKey?.(k, v.state.doc.toString());
      return true;
    };
    const v = new EditorView({
      parent: host.current!,
      state: EditorState.create({
        doc: initial,
        selection: { anchor: initial.length },
        extensions: [
          inGrid
            ? Prec.highest(
                keymap.of([
                  { key: "Enter", run: key("Enter") },
                  { key: "Shift-Enter", run: insertNewlineAndIndent },
                  { key: "Escape", run: key("Escape") },
                  { key: "Tab", run: key("Tab") },
                  { key: "Shift-Tab", run: key("Shift-Tab") },
                ]),
              )
            : [],
          history(),
          closeBrackets(),
          bracketMatching(),
          keymap.of([...defaultKeymap, ...historyKeymap]),
          EditorView.lineWrapping,
          jsonTheme,
          EditorView.contentAttributes.of({ "aria-label": label }),
          EditorView.updateListener.of((u) => u.docChanged && !syncing.current && handlers.current.onChange(u.state.doc.toString())),
          EditorView.domEventHandlers({ blur: (_e, ed) => void handlers.current.onBlur?.(ed.state.doc.toString()) }),
        ],
      }),
    });
    view.current = v;
    if (inGrid) v.focus();
    return () => {
      v.destroy();
      view.current = null;
    };
    // Created once per edit; later changes reach it through `handlers`.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  useEffect(() => {
    view.current?.contentDOM.setAttribute("aria-invalid", invalid ? "true" : "false");
  }, [invalid]);

  // A controlled value that changed outside the editor (a pick, a reset after save) replaces the document.
  useEffect(() => {
    const v = view.current;
    if (!v || value === undefined) return;
    const doc = v.state.doc.toString();
    if (doc === value) return;
    syncing.current = true;
    v.dispatch({ changes: { from: 0, to: doc.length, insert: value } });
    syncing.current = false;
  }, [value]);

  return <div ref={host} onKeyDown={(e) => e.stopPropagation()} className={inGrid ? "w-full" : "w-full min-w-0 border border-line-strong"} />;
}
