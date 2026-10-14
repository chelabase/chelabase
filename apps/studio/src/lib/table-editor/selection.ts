/** Row selection in the grid: none, a set of primary-key tuples on the loaded rows, or every row matching the view. */
export type Selection = { kind: "none" } | { kind: "rows"; keys: string[] } | { kind: "all_matching"; estimate: number | null };

/** The selection plus the view (table, filter, sort) it was made in. */
export interface SelectionState {
  view: string;
  selection: Selection;
}

export type SelectionAction =
  | { type: "view"; view: string } // a different view resets to none
  | { type: "toggle"; key: string } // in all_matching: back to none
  | { type: "page"; keys: string[] } // all on page; when all already selected: none
  | { type: "all_matching"; estimate: number | null }
  | { type: "clear" };

const NONE: Selection = { kind: "none" };

export const NO_SELECTION: SelectionState = { view: "", selection: NONE };

function withSelection(s: SelectionState, selection: Selection): SelectionState {
  return { view: s.view, selection };
}

function rows(keys: string[]): Selection {
  return keys.length === 0 ? NONE : { kind: "rows", keys };
}

export function reduceSelection(s: SelectionState, a: SelectionAction): SelectionState {
  switch (a.type) {
    case "view":
      return a.view === s.view ? s : { view: a.view, selection: NONE };
    case "toggle": {
      const sel = s.selection;
      if (sel.kind === "all_matching") return withSelection(s, NONE);
      const keys = sel.kind === "rows" ? sel.keys : [];
      return withSelection(s, rows(keys.includes(a.key) ? keys.filter((k) => k !== a.key) : [...keys, a.key]));
    }
    case "page": {
      if (a.keys.length === 0) return s;
      const keys = s.selection.kind === "rows" ? s.selection.keys : [];
      if (a.keys.every((k) => keys.includes(k))) return withSelection(s, NONE);
      return withSelection(s, rows([...keys, ...a.keys.filter((k) => !keys.includes(k))]));
    }
    case "all_matching":
      return withSelection(s, { kind: "all_matching", estimate: a.estimate });
    case "clear":
      return withSelection(s, NONE);
  }
}

/** A row's identity: its primary-key tuple as JSON. */
export function rowKey(pk: string[], row: Record<string, unknown>): string {
  return JSON.stringify(pk.map((c) => row[c] ?? null));
}

/** Identifies the rows a selection was made in; a change of table, filter or sort gives a new key. */
export function viewKey(c: { changeset: string; filter: unknown; sort: unknown }): string {
  return JSON.stringify([c.changeset, c.filter ?? null, c.sort ?? null]);
}
