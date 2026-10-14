import { useEffect, useRef, useState, type KeyboardEvent } from "react";
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
/** Where the active cell goes after a save: nowhere, or the next/previous cell (Tab, Shift+Tab). */
export type Move = "none" | "next" | "prev";

export interface CellEditorProps {
  column: RowColumn;
  /** The text the editor starts from (the value, or the key that started the edit). */
  initial: string;
  busy: boolean;
  error: string | null;
  onCommit: (value: CellValue, move: Move) => void;
  onCancel: () => void;
}

const field =
  "h-full w-full border-0 bg-ink px-2 font-mono text-xs text-chalk outline-2 -outline-offset-2 outline-sand disabled:opacity-60";

/**
 * A type-aware inline editor: a select for booleans and enums, a textarea for JSON and arrays,
 * a text field otherwise. Enter saves, Escape cancels, Tab saves and moves; leaving the field
 * saves a changed value. Nullable columns offer "Set NULL".
 */
export function CellEditor({ column, initial, busy, error, onCommit, onCancel }: CellEditorProps) {
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
    <div data-editor="" className="absolute inset-x-0 top-0 z-30 flex min-h-9 flex-col">
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
        <p role="alert" className="w-max max-w-80 border border-coral/60 bg-deep px-2 py-1 font-sans text-xs whitespace-normal text-coral">
          {error}
        </p>
      )}
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
