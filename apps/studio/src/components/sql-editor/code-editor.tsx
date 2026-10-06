import { forwardRef, useEffect, useImperativeHandle, useRef } from "react";
import { closeBrackets } from "@codemirror/autocomplete";
import { defaultKeymap, history, historyKeymap, indentWithTab } from "@codemirror/commands";
import { bracketMatching, indentOnInput } from "@codemirror/language";
import { lintGutter } from "@codemirror/lint";
import { highlightSelectionMatches, searchKeymap } from "@codemirror/search";
import { Compartment, EditorState, Prec, StateEffect, StateField, type Extension } from "@codemirror/state";
import { Decoration, type DecorationSet, drawSelection, EditorView, highlightActiveLine, keymap, lineNumbers } from "@codemirror/view";

/** What the next run sends: the selection when there is one, else the whole buffer. */
export type RunText = { text: string; from: number };

export interface CodeEditorHandle {
  /** The run text, or null when it is blank. */
  runText(): RunText | null;
  setDoc(text: string): void;
  /** Underlines the character at `pos` (cleared by the next edit); null clears. */
  markError(pos: number | null): void;
  goTo(pos: number): void;
}

export interface CodeEditorProps {
  initialDoc: string;
  extensions: Extension[];
  onChange: (doc: string) => void;
  onRun: () => void;
}

const setError = StateEffect.define<number | null>();
const errorMark = Decoration.mark({ class: "cm-sql-error" });

const errorField = StateField.define<{ pos: number | null; deco: DecorationSet }>({
  create: () => ({ pos: null, deco: Decoration.none }),
  update(value, tr) {
    for (const e of tr.effects) {
      if (e.is(setError)) {
        const pos = e.value === null ? null : Math.min(e.value, Math.max(tr.state.doc.length - 1, 0));
        if (pos === null || tr.state.doc.length === 0) return { pos: null, deco: Decoration.none };
        // Underline the word at the position, or one character when it isn't on a word.
        const line = tr.state.doc.lineAt(pos);
        const rest = line.text.slice(pos - line.from);
        const word = /^[\w$]+/.exec(rest);
        const to = Math.min(pos + Math.max(word ? word[0].length : 1, 1), line.to > pos ? line.to : pos + 1, tr.state.doc.length);
        return { pos, deco: Decoration.set([errorMark.range(pos, Math.max(to, pos + 1))]) };
      }
    }
    return tr.docChanged ? { pos: null, deco: Decoration.none } : value;
  },
  provide: (f) => EditorView.decorations.from(f, (v) => v.deco),
});

// `dark: true`: Studio's ground is dark, so CodeMirror's base theme must use its dark caret,
// active-line and selection colours (the light ones give a black caret and pale washes under chalk text).
const SELECTION = "rgb(242 199 163 / 0.25)";

const theme = EditorView.theme(
  {
    "&": { fontSize: "13px", minHeight: "10rem", maxHeight: "24rem", border: "1px solid var(--border)", borderRadius: "0.5rem", backgroundColor: "var(--background)", color: "var(--foreground)" },
    ".cm-scroller": { fontFamily: "var(--font-mono)", overflow: "auto" },
    ".cm-gutters": { backgroundColor: "var(--muted)", color: "var(--muted-foreground)", border: "none" },
    "&.cm-focused": { outline: "2px solid var(--ring)" },
    // Sand at low alpha, like the global ::selection; the dark base theme's #233 barely shows on ink.
    // The base theme's rules are `&dark.cm-focused > .cm-scroller > .cm-selectionLayer ...`: match their specificity.
    "&.cm-focused > .cm-scroller > .cm-selectionLayer .cm-selectionBackground, .cm-selectionBackground": { background: SELECTION },
    // drawSelection() makes the native selection transparent, but its glyph colour still comes from the global
    // ::selection (ink, 1.9:1 on the sand tint): keep selected text chalk, with no native background of its own.
    ".cm-content ::selection, .cm-content::selection": { color: "var(--foreground)", backgroundColor: "transparent" },
  },
  { dark: true },
);

/** CodeMirror 6 used directly (no React wrapper). `extensions` may change over the editor's life. */
export const CodeEditor = forwardRef<CodeEditorHandle, CodeEditorProps>(function CodeEditor({ initialDoc, extensions, onChange, onRun }, ref) {
  const host = useRef<HTMLDivElement>(null);
  const view = useRef<EditorView | null>(null);
  const language = useRef(new Compartment());
  const onChangeRef = useRef(onChange);
  const onRunRef = useRef(onRun);
  onChangeRef.current = onChange;
  onRunRef.current = onRun;

  useEffect(() => {
    const v = new EditorView({
      parent: host.current!,
      state: EditorState.create({
        doc: initialDoc,
        extensions: [
          Prec.highest(keymap.of([{ key: "Mod-Enter", run: () => (onRunRef.current(), true) }])),
          lineNumbers(),
          history(),
          drawSelection(),
          indentOnInput(),
          bracketMatching(),
          closeBrackets(),
          highlightActiveLine(),
          highlightSelectionMatches(),
          lintGutter(),
          keymap.of([...defaultKeymap, ...historyKeymap, ...searchKeymap, indentWithTab]),
          errorField,
          theme,
          language.current.of(extensions),
          EditorView.contentAttributes.of({ "aria-label": "SQL editor" }),
          EditorView.updateListener.of((u) => u.docChanged && onChangeRef.current(u.state.doc.toString())),
        ],
      }),
    });
    view.current = v;
    return () => {
      v.destroy();
      view.current = null;
    };
    // The editor is created once; later changes go through the compartment and the handle.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  useEffect(() => {
    view.current?.dispatch({ effects: language.current.reconfigure(extensions) });
  }, [extensions]);

  useImperativeHandle(ref, () => ({
    runText() {
      const v = view.current;
      if (!v) return null;
      const sel = v.state.selection.main;
      const picked = !sel.empty ? { text: v.state.sliceDoc(sel.from, sel.to), from: sel.from } : { text: v.state.doc.toString(), from: 0 };
      return picked.text.trim() === "" ? null : picked;
    },
    setDoc(text) {
      const v = view.current;
      v?.dispatch({ changes: { from: 0, to: v.state.doc.length, insert: text }, selection: { anchor: 0 } });
    },
    markError(pos) {
      view.current?.dispatch({ effects: setError.of(pos) });
    },
    goTo(pos) {
      const v = view.current;
      if (!v) return;
      const at = Math.min(pos, v.state.doc.length);
      v.dispatch({ selection: { anchor: at }, scrollIntoView: true });
      v.focus();
    },
  }));

  return <div ref={host} data-testid="sql-editor" />;
});
