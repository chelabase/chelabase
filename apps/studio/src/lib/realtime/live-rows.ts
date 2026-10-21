import type { Change, Key } from "./types";

/** R15: keyless refetches wait this long after the first request (later ones join it). */
export const KEYLESS_DEBOUNCE_MS = 500;
/** R15: at least this long between two keyless refetches, doubling while requests keep arriving... */
export const KEYLESS_MIN_MS = 2000;
/** ...up to this. */
export const KEYLESS_MAX_MS = 10_000;

export interface LiveState {
  /** Whether live updates are running (the last `joined`/`status`). */
  live: boolean;
  /** The last `rows` seq seen; 0 after a join. */
  seq: number;
  /** Insert keys not on the page, for the "N new rows" chip. */
  newKeys: Key[];
  /** Keys of page rows a keyed delete struck through. */
  struck: Key[];
  /** Keys a by-key refetch returned nothing for (left the role's view or the filter): fading out. */
  fading: Key[];
}

export const INITIAL_LIVE: LiveState = { live: false, seq: 0, newKeys: [], struck: [], fading: [] };

export type LiveEvent =
  | { type: "joined"; live: boolean }
  | { type: "status"; live: boolean }
  | { type: "rows"; seq: number; changes: Change[]; pageKeys: ReadonlySet<Key> }
  | { type: "resync" }
  | { type: "refetched_keys"; requested: Key[]; returned: Key[] }
  | { type: "page_loaded" }
  | { type: "left" };

export type LiveEffect =
  | { type: "refetch_keys"; keys: Key[] }
  /** `now`: at once (R8); `debounced`: through the R15 gate. */
  | { type: "refetch_page"; mode: "now" | "debounced" };

const PAGE_NOW: LiveEffect = { type: "refetch_page", mode: "now" };
const PAGE_DEBOUNCED: LiveEffect = { type: "refetch_page", mode: "debounced" };

const add = (list: Key[], keys: Iterable<Key>) => {
  const out = [...list];
  for (const k of keys) if (!out.includes(k)) out.push(k);
  return out;
};

function reduceRows(state: LiveState, e: Extract<LiveEvent, { type: "rows" }>): { state: LiveState; effects: LiveEffect[] } {
  // A gap (or a repeat) means frames were missed: the page is read again, whatever this frame says.
  if (e.seq !== state.seq + 1) return { state: { ...state, seq: e.seq }, effects: [PAGE_DEBOUNCED] };
  const refetch: Key[] = [];
  let newKeys = state.newKeys;
  let struck = state.struck;
  let page = false;
  for (const c of e.changes) {
    if (c.op === "changed" || !c.keys) {
      page = true;
      continue;
    }
    for (const k of c.keys) {
      if (c.op === "delete") {
        if (e.pageKeys.has(k)) struck = add(struck, [k]);
        else newKeys = newKeys.filter((n) => n !== k);
      } else if (e.pageKeys.has(k)) {
        if (!refetch.includes(k)) refetch.push(k);
        struck = struck.filter((s) => s !== k);
      } else if (c.op === "insert") {
        newKeys = add(newKeys, [k]);
      }
    }
  }
  const next = { ...state, seq: e.seq, newKeys, struck };
  // A page refetch reads the rows the by-key one would.
  if (page) return { state: next, effects: [PAGE_DEBOUNCED] };
  return { state: next, effects: refetch.length > 0 ? [{ type: "refetch_keys", keys: refetch }] : [] };
}

export function reduceLive(state: LiveState, event: LiveEvent): { state: LiveState; effects: LiveEffect[] } {
  switch (event.type) {
    case "joined":
      // R8: rows may have changed before the slot existed; seq starts again at 1.
      return { state: { ...INITIAL_LIVE, live: event.live }, effects: event.live ? [PAGE_NOW] : [] };
    case "status":
      // R8: every `status {live: true}` reads the page again (the slot may be new).
      return { state: { ...state, live: event.live }, effects: event.live ? [PAGE_NOW] : [] };
    case "rows":
      return reduceRows(state, event);
    case "resync":
      return { state, effects: [PAGE_DEBOUNCED] };
    case "refetched_keys": {
      const gone = event.requested.filter((k) => !event.returned.includes(k));
      const fading = add(
        state.fading.filter((k) => !event.returned.includes(k)),
        gone,
      );
      return { state: { ...state, fading }, effects: [] };
    }
    case "page_loaded":
      return { state: { ...state, newKeys: [], struck: [], fading: [] }, effects: [] };
    case "left":
      return { state: INITIAL_LIVE, effects: [] };
  }
}

export interface RefetchGate {
  /** A keyless refetch is wanted: debounced 500 ms, spaced 2 s apart, backing off to 10 s while requests keep arriving. */
  request(): void;
  /** Runs at once (R8) and counts as a run for the spacing. */
  runNow(): void;
  cancel(): void;
}

export function createRefetchGate(run: () => void): RefetchGate {
  let timer: ReturnType<typeof setTimeout> | null = null;
  let lastRun: number | null = null;
  let spacing = KEYLESS_MIN_MS;
  // Whether the pending run was pushed back by the spacing (requests kept arriving): the next spacing doubles.
  let throttled = false;
  const fire = () => {
    timer = null;
    lastRun = Date.now();
    if (throttled) spacing = Math.min(KEYLESS_MAX_MS, spacing * 2);
    throttled = false;
    run();
  };
  return {
    request() {
      if (timer !== null) return;
      const now = Date.now();
      let at = now + KEYLESS_DEBOUNCE_MS;
      if (lastRun !== null && now < lastRun + spacing) {
        at = Math.max(at, lastRun + spacing);
        throttled = true;
      } else {
        // A quiet stretch: back to the minimum spacing.
        spacing = KEYLESS_MIN_MS;
      }
      timer = setTimeout(fire, at - now);
    },
    runNow() {
      if (timer !== null) clearTimeout(timer);
      timer = null;
      throttled = false;
      lastRun = Date.now();
      run();
    },
    cancel() {
      if (timer !== null) clearTimeout(timer);
      timer = null;
      throttled = false;
    },
  };
}

export interface LiveController {
  dispatch(event: DispatchEvent): void;
  state(): LiveState;
  dispose(): void;
}

/** What callers dispatch: `rows` without `pageKeys` (the controller reads them from `getPageKeys`). */
export type DispatchEvent = Exclude<LiveEvent, { type: "rows" | "refetched_keys" }> | { type: "rows"; seq: number; changes: Change[] };

export function createLiveController(opts: {
  getPageKeys(): ReadonlySet<Key>;
  /** One refetch by key; resolves to the keys the refetch returned (the controller then dispatches `refetched_keys`). */
  refetchKeys(keys: Key[]): Promise<Key[]>;
  /** Page and count. */
  refetchPage(): void;
  onChange?(state: LiveState): void;
}): LiveController {
  let state = INITIAL_LIVE;
  let disposed = false;
  const gate = createRefetchGate(() => opts.refetchPage());
  function apply(event: LiveEvent) {
    if (disposed) return;
    const r = reduceLive(state, event);
    if (r.state !== state) {
      state = r.state;
      opts.onChange?.(state);
    }
    for (const effect of r.effects) {
      if (effect.type === "refetch_page") {
        if (effect.mode === "now") gate.runNow();
        else gate.request();
      } else {
        const requested = effect.keys;
        opts.refetchKeys(requested).then(
          (returned) => apply({ type: "refetched_keys", requested, returned }),
          // A failed by-key read: the page is read again instead.
          () => apply({ type: "resync" }),
        );
      }
    }
  }
  return {
    dispatch(event) {
      apply(event.type === "rows" ? { ...event, pageKeys: opts.getPageKeys() } : event);
    },
    state: () => state,
    dispose() {
      disposed = true;
      gate.cancel();
    },
  };
}
