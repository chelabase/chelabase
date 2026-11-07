/** The browser's multi-select: a set of object names and the anchor a Shift-click ranges from. */
export interface SelectionState {
  selected: ReadonlySet<string>;
  anchor: string | null;
}

export type SelectionAction =
  | { type: "toggle"; name: string }
  /** Shift-click: everything from the anchor to `name`, in the list's order. */
  | { type: "range"; name: string; order: readonly string[] }
  | { type: "all"; names: readonly string[] }
  | { type: "clear" }
  /** Only these stay selected (after a delete, or when the listing changes). */
  | { type: "keep"; names: readonly string[] };

export const EMPTY_SELECTION: SelectionState = { selected: new Set(), anchor: null };

export function selectionReducer(state: SelectionState, action: SelectionAction): SelectionState {
  switch (action.type) {
    case "toggle": {
      const next = new Set(state.selected);
      if (next.has(action.name)) next.delete(action.name);
      else next.add(action.name);
      return { selected: next, anchor: action.name };
    }
    case "range": {
      const from = state.anchor === null ? -1 : action.order.indexOf(state.anchor);
      const to = action.order.indexOf(action.name);
      if (from < 0 || to < 0) return selectionReducer(state, { type: "toggle", name: action.name });
      const [lo, hi] = from <= to ? [from, to] : [to, from];
      return { selected: new Set([...state.selected, ...action.order.slice(lo, hi + 1)]), anchor: state.anchor };
    }
    case "all":
      return { selected: new Set(action.names), anchor: state.anchor };
    case "clear":
      return state.selected.size === 0 && state.anchor === null ? state : EMPTY_SELECTION;
    case "keep": {
      const keep = new Set(action.names);
      const next = [...state.selected].filter((n) => keep.has(n));
      if (next.length === state.selected.size) return state;
      return { selected: new Set(next), anchor: state.anchor !== null && keep.has(state.anchor) ? state.anchor : null };
    }
  }
}

/** One object's delete: done, or refused with storage's reason. */
export type DeleteResult = { name: string; ok: true } | { name: string; ok: false; message: string };

export interface DeleteReport {
  removed: string[];
  failed: { name: string; message: string }[];
  summary: string;
}

const objects = (n: number) => `${n} ${n === 1 ? "object" : "objects"}`;

/** What a multi-delete did: the count removed and each failure on its own. */
export function deleteReport(results: readonly DeleteResult[]): DeleteReport {
  const removed = results.filter((r) => r.ok).map((r) => r.name);
  const failed = results.flatMap((r) => (r.ok ? [] : [{ name: r.name, message: r.message }]));
  const done = removed.length === 0 ? "Nothing was deleted" : `Deleted ${objects(removed.length)}`;
  const summary = failed.length === 0 ? `${done}.` : `${done}; ${failed.length} couldn't be deleted.`;
  return { removed, failed, summary };
}
