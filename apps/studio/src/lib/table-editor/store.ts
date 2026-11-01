import { createContext, useContext } from "react";
import { createStore, useStore, type StoreApi } from "zustand";
import type { Table } from "@chelabase/management-client";
import { fold, tableFingerprint, withoutEdit, type Changeset, type Edit } from "./changeset";
import type { ChangeFailure } from "./dry-run";
import { NO_SELECTION, reduceSelection, type SelectionAction, type SelectionState } from "./selection";

export type PanelKind = "closed" | "changes" | "column" | "row";
/** A field the column form can be opened on (the strip's menu: Rename, Change type, Default, Comment). */
export type FormField = "name" | "type" | "default" | "comment";
export interface PanelState {
  kind: PanelKind;
  collapsed: boolean;
  column: string | null;
  row: string | null;
  /** A focus request for the column form: consumed (removed) by the form that acts on it. */
  focus?: { field: FormField; seq: number };
}

const CLOSED: PanelState = { kind: "closed", collapsed: false, column: null, row: null };

/** What the main area shows: the open table, the schema's enums, or the new-table form. */
export type EditorView = "table" | "types" | "new_table";

export interface TableEditorState {
  view: EditorView;
  /** Also closes the panel (it belongs to the view it was opened from). */
  setView(view: EditorView): void;
  /** How many edit dialogs are open: the changes list waits for them before it asks for a dry run. */
  dialogs: number;
  setDialogOpen(open: boolean): void;
  changesets: Record<string, Changeset>;
  panel: PanelState;
  /** Key `${databaseId}/${branchId}`; null = the owner. Kept in the given storage, so the SQL tab and the table editor share it. */
  actAs: Record<string, string | null>;
  /** The SQL drawer's share of the editor's height (whether it is open is the URL's `?sql=1`). */
  sqlHeight: number;
  /** Clamped to `SQL_HEIGHT`. */
  setSqlHeight(fraction: number): void;
  selection: SelectionState;
  /** Creates the changeset with `base = tableFingerprint(base)`, folds the edit in, and drops the changeset when it ends up empty. */
  addEdit(key: string, base: Table | null, edit: Edit): void;
  /** With `base`, also removes the later edits that no longer apply without it (`withoutEdit`). */
  removeEdit(key: string, id: string, base?: Table | null): void;
  discard(key: string): void;
  /** After the user reviewed a changed table. */
  rebase(key: string, base: Table): void;
  /**
   * Also un-collapses the panel. `focus`: the column form puts focus on that field (else on Name). When it would drop
   * the row panel's unsaved draft (another panel, another row, closed), it asks first (`discardAsk`) unless `discard`.
   */
  openPanel(kind: PanelKind, target?: { column?: string | null; row?: string | null; focus?: FormField }, opts?: { discard?: boolean }): void;
  /** The row panel's unsaved draft while it has one (published by the panel; a collapsed panel keeps it). */
  rowDraft: RowDraft | null;
  setRowDraft(draft: RowDraft): void;
  /** Forgets the draft this panel published (`id`); another panel's is kept. */
  clearRowDraft(id: number): void;
  /** Bumped by a Discard: the row panel remounts empty (its key includes it). */
  rowDraftEpoch: number;
  /** The rows on screen (their keys and the first one's number), for the discard prompt's "row N". */
  pageRows: { table: string; keys: readonly (string | null)[]; first: number } | null;
  setPageRows(rows: { table: string; keys: readonly (string | null)[]; first: number } | null): void;
  /** Runs `run` now, or asks first ("Discard changes to row N?") while the row panel holds an unsaved draft. */
  guardDiscard(run: () => void): void;
  /**
   * The same question as a promise (the route blocker: Back, Forward, a link away): true once Discard drops the draft
   * (or at once when there is none), false on Keep editing.
   */
  askDiscard(): Promise<boolean>;
  /** The question being asked, or null. `cancel` runs on Keep editing (or when another question replaces it). */
  discardAsk: { title: string; run: () => void; cancel?: () => void } | null;
  /** The answer: Discard drops the draft, then runs what was asked; Keep editing changes nothing. */
  answerDiscard(discard: boolean): void;
  /** The column form acted on the panel's focus request. */
  consumeFocus(): void;
  setCollapsed(collapsed: boolean): void;
  setActAs(branchKey: string, role: string | null): void;
  select(action: SelectionAction): void;
  /** The last Apply's outcome line and the columns it changed (marked seafoam until the next edit). */
  applied: Applied | null;
  setApplied(applied: Applied | null): void;
  /** Changeset keys whose Apply is in flight: nothing is staged for them meanwhile. */
  applying: Record<string, true>;
  setApplying(key: string, on: boolean): void;
  /** The last failed Apply per changeset key, for the dry run it was sent from. */
  applyFailures: Record<string, ApplyFailure>;
  setApplyFailure(key: string, failure: ApplyFailure | null): void;
  /** After a successful Apply: removes the sent edits (by id), records the outcome, ends `applying`. */
  finishApply(key: string, sentIds: string[], applied: Applied): void;
  /**
   * A table's exact row count per changeset key, known when an unfiltered page came back shorter than
   * the page size; the table bar and the table list use it when the table has no row estimate.
   */
  exactRows: Record<string, number>;
  /** null forgets it (a full page showed the table may have grown past it). */
  setExactRows(key: string, rows: number | null): void;
  /**
   * The last row number on screen per changeset key (the page's offset + its rows): the table has at least that many
   * rows, so a smaller row estimate is shown as this count instead.
   */
  shownRows: Record<string, number>;
  setShownRows(key: string, rows: number | null): void;
}

export interface RowDraft {
  /** The publishing panel instance. */
  id: number;
  /** The changeset key of its table. */
  table: string;
  /** The row's key; null for a New row panel. */
  row: string | null;
}

/** The discard prompt's title for this draft, given the rows on screen. */
export function discardTitle(draft: RowDraft, pageRows: TableEditorState["pageRows"]): string {
  if (draft.row === null) return "Discard the new row?";
  const i = pageRows && pageRows.table === draft.table ? pageRows.keys.indexOf(draft.row) : -1;
  return i >= 0 ? `Discard changes to row ${pageRows!.first + i}?` : "Discard changes to this row?";
}

export interface ApplyFailure {
  /** The full key of the dry run the Apply was sent from (`useDryRun`'s `key`). */
  dryKey: string;
  failure: ChangeFailure;
}

export interface Applied {
  key: string;
  text: string;
  /** Column names (after the apply) that the applied changes added or changed. */
  columns: string[];
}

let focusSeq = 0;

export const DRAFTS_KEY = "chelabase.studio.tables.drafts";
export const ACT_AS_KEY = "chelabase.studio.tables.actAs";

/** The SQL drawer's height bounds and default, as fractions of the editor's height. */
export const SQL_HEIGHT = { min: 0.25, max: 0.7, initial: 0.4 } as const;
export const clampSqlHeight = (f: number) => Math.min(SQL_HEIGHT.max, Math.max(SQL_HEIGHT.min, f));

function loadActAs(storage: Storage | null): Record<string, string | null> {
  try {
    const parsed: unknown = JSON.parse(storage?.getItem(ACT_AS_KEY) ?? "{}");
    if (typeof parsed !== "object" || parsed === null || Array.isArray(parsed)) return {};
    return Object.fromEntries(Object.entries(parsed).filter((e): e is [string, string | null] => e[1] === null || typeof e[1] === "string"));
  } catch {
    return {};
  }
}

function loadDrafts(storage: Storage | null): Record<string, Changeset> {
  if (!storage) return {};
  try {
    const raw = storage.getItem(DRAFTS_KEY);
    if (!raw) return {};
    const parsed: unknown = JSON.parse(raw);
    if (typeof parsed !== "object" || parsed === null || Array.isArray(parsed)) return {};
    const out: Record<string, Changeset> = {};
    for (const [key, value] of Object.entries(parsed)) {
      const c = value as Partial<Changeset> | null;
      if (!c || !Array.isArray(c.edits) || !(c.base === null || typeof c.base === "string")) continue;
      // Keep only well-formed edits: a corrupt one would throw later, in `fold` or `project`.
      const edits = (c.edits as unknown[]).filter((e): e is Edit => {
        if (typeof e !== "object" || e === null) return false;
        const x = e as Partial<Edit>;
        return typeof x.id === "string" && Array.isArray(x.ops) && typeof x.label === "string";
      });
      if (edits.length > 0) out[key] = { key, base: c.base ?? null, edits };
    }
    return out;
  } catch {
    return {};
  }
}

/** The editor's client state. Drafts (the changesets) live in the given storage; storage errors are ignored. */
export function createTableEditorStore(storage: Storage | null): StoreApi<TableEditorState> {
  const store = createStore<TableEditorState>()((set, get) => ({
    changesets: loadDrafts(storage),
    panel: CLOSED,
    actAs: loadActAs(storage),
    sqlHeight: SQL_HEIGHT.initial,
    setSqlHeight: (fraction) => set({ sqlHeight: clampSqlHeight(fraction) }),
    selection: NO_SELECTION,
    view: "table",
    setView: (view) => {
      if (get().view === view) return;
      get().guardDiscard(() => set({ view, panel: CLOSED }));
    },
    rowDraft: null,
    setRowDraft: (draft) =>
      set((s) => (s.rowDraft && s.rowDraft.id === draft.id && s.rowDraft.table === draft.table && s.rowDraft.row === draft.row ? s : { rowDraft: draft })),
    clearRowDraft: (id) => set((s) => (s.rowDraft?.id === id ? { rowDraft: null } : s)),
    rowDraftEpoch: 0,
    pageRows: null,
    setPageRows: (pageRows) => set({ pageRows }),
    discardAsk: null,
    guardDiscard: (run) => {
      const { rowDraft, pageRows } = get();
      if (!rowDraft) return run();
      get().discardAsk?.cancel?.();
      set({ discardAsk: { title: discardTitle(rowDraft, pageRows), run } });
    },
    askDiscard: () =>
      new Promise<boolean>((resolve) => {
        const { rowDraft, pageRows } = get();
        if (!rowDraft) return resolve(true);
        get().discardAsk?.cancel?.();
        set({ discardAsk: { title: discardTitle(rowDraft, pageRows), run: () => resolve(true), cancel: () => resolve(false) } });
      }),
    answerDiscard: (discard) => {
      const ask = get().discardAsk;
      if (!ask) return;
      if (!discard) {
        set({ discardAsk: null });
        ask.cancel?.();
        return;
      }
      set((s) => ({ discardAsk: null, rowDraft: null, rowDraftEpoch: s.rowDraftEpoch + 1 }));
      ask.run();
    },
    dialogs: 0,
    setDialogOpen: (open) => set((s) => ({ dialogs: Math.max(0, s.dialogs + (open ? 1 : -1)) })),
    applied: null,
    applying: {},
    applyFailures: {},
    exactRows: {},
    setExactRows: (key, rows) =>
      set((s) => {
        if ((s.exactRows[key] ?? null) === rows) return s;
        const { [key]: _gone, ...rest } = s.exactRows;
        return { exactRows: rows === null ? rest : { ...rest, [key]: rows } };
      }),
    shownRows: {},
    setShownRows: (key, rows) =>
      set((s) => {
        if ((s.shownRows[key] ?? null) === rows) return s;
        const { [key]: _gone, ...rest } = s.shownRows;
        return { shownRows: rows === null ? rest : { ...rest, [key]: rows } };
      }),
    addEdit: (key, base, edit) =>
      set((s) => {
        if (s.applying[key]) return s;
        const current = s.changesets[key] ?? { key, base: base ? tableFingerprint(base) : null, edits: [] };
        const edits = fold(base, current.edits, edit);
        const { [key]: _gone, ...rest } = s.changesets;
        return {
          changesets: edits.length === 0 ? rest : { ...rest, [key]: { ...current, edits } },
          applied: s.applied?.key === key ? null : s.applied,
        };
      }),
    removeEdit: (key, id, base) =>
      set((s) => {
        const current = s.changesets[key];
        if (!current) return s;
        const edits = base === undefined ? current.edits.filter((e) => e.id !== id) : withoutEdit(base, current.edits, id);
        const { [key]: _gone, ...rest } = s.changesets;
        return { changesets: edits.length === 0 ? rest : { ...rest, [key]: { ...current, edits } } };
      }),
    discard: (key) =>
      set((s) => {
        if (!(key in s.changesets)) return s;
        const { [key]: _gone, ...rest } = s.changesets;
        return { changesets: rest };
      }),
    rebase: (key, base) =>
      set((s) => {
        const current = s.changesets[key];
        return current ? { changesets: { ...s.changesets, [key]: { ...current, base: tableFingerprint(base) } } } : s;
      }),
    openPanel: (kind, target, opts) => {
      const { panel, rowDraft } = get();
      // The same row (expanded if collapsed) keeps its panel and draft; anything else drops them.
      const keeps = panel.kind === "row" && kind === "row" && (target?.row ?? null) === panel.row;
      if (!opts?.discard && rowDraft && panel.kind === "row" && !keeps) {
        get().guardDiscard(() => get().openPanel(kind, target, { discard: true }));
        return;
      }
      set(() => ({
        panel: {
          kind,
          collapsed: false,
          column: target?.column ?? null,
          row: target?.row ?? null,
          focus: target?.focus ? { field: target.focus, seq: ++focusSeq } : undefined,
        },
      }));
    },
    consumeFocus: () => set((s) => (s.panel.focus === undefined ? s : { panel: { ...s.panel, focus: undefined } })),
    setCollapsed: (collapsed) => set((s) => ({ panel: { ...s.panel, collapsed } })),
    setActAs: (branchKey, role) => set((s) => ({ actAs: { ...s.actAs, [branchKey]: role } })),
    select: (action) => set((s) => ({ selection: reduceSelection(s.selection, action) })),
    setApplied: (applied) => set({ applied }),
    setApplying: (key, on) =>
      set((s) => {
        const { [key]: _gone, ...rest } = s.applying;
        return { applying: on ? { ...rest, [key]: true } : rest };
      }),
    setApplyFailure: (key, failure) =>
      set((s) => {
        const { [key]: _gone, ...rest } = s.applyFailures;
        return { applyFailures: failure ? { ...rest, [key]: failure } : rest };
      }),
    finishApply: (key, sentIds, applied) =>
      set((s) => {
        const { [key]: current, ...others } = s.changesets;
        const left = (current?.edits ?? []).filter((e) => !sentIds.includes(e.id));
        const { [key]: _a, ...applying } = s.applying;
        const { [key]: _f, ...applyFailures } = s.applyFailures;
        return {
          changesets: current && left.length > 0 ? { ...others, [key]: { ...current, edits: left } } : others,
          applied,
          applying,
          applyFailures,
        };
      }),
  }));

  if (storage) {
    store.subscribe((state, prev) => {
      try {
        if (state.changesets !== prev.changesets) storage.setItem(DRAFTS_KEY, JSON.stringify(state.changesets));
        if (state.actAs !== prev.actAs) storage.setItem(ACT_AS_KEY, JSON.stringify(state.actAs));
      } catch {
        // A draft or role that can't be saved is still kept in memory.
      }
    });
  }
  return store;
}

export const TableEditorStoreContext = createContext<StoreApi<TableEditorState> | null>(null);

export function useTableEditor<T>(selector: (s: TableEditorState) => T): T {
  const store = useContext(TableEditorStoreContext);
  if (!store) throw new Error("useTableEditor needs a TableEditorStoreContext provider");
  return useStore(store, selector);
}
