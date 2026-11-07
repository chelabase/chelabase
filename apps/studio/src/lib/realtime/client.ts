import type {
  Key,
  SessionEvents,
  PresenceState,
  ProjectHandlers,
  ProjectSubscription,
  RealtimeClient,
  RealtimeError,
  StorageJoinParams,
  StorageSubscription,
  TableHandlers,
  TableJoinParams,
  TableSubscription,
} from "./types";

export const SUBPROTOCOL = "chelabase.realtime.v1";
/** P9: table channels are left after the tab has been hidden this long. */
export const HIDDEN_LEAVE_MS = 5 * 60_000;
/** R19: the most keys one `view` frame carries. */
export const MAX_VIEW_KEYS = 1000;
/** The longest reconnect delay before jitter. */
const MAX_BACKOFF_MS = 30_000;
/** Close code: access revoked or identity mismatch (spec §2). Not retried. */
const REVOKED = 4003;
/** Join refusals that stick (P13): every other refusal is transient and the join is retried with backoff. */
const STICKY_REFUSALS = new Set(["forbidden", "not_found"]);

/** `VITE_API_URL` with http→ws / https→wss, `/v1/realtime/socket?ticket=<ticket>` (S17). */
export function socketUrl(apiUrl: string, ticket: string): string {
  const base = apiUrl.replace(/\/+$/, "").replace(/^http(s?):\/\//i, (_m, s: string) => `ws${s}://`);
  return `${base}/v1/realtime/socket?ticket=${encodeURIComponent(ticket)}`;
}

/** The canonical table channel id: `table:` + compact JSON `[database_id,branch_id,pg_database,schema,table]` (no role). */
export function tableChannelId(p: { database_id: string; branch_id: string; pg_database: string; schema: string; table: string }): string {
  return `table:${JSON.stringify([p.database_id, p.branch_id, p.pg_database, p.schema, p.table])}`;
}

/** The canonical storage channel id: `storage:` + compact JSON `[database_id,branch_id,pg_database,bucket_id]` (no role). */
export function storageChannelId(p: { database_id: string; branch_id: string; pg_database: string; bucket_id: string }): string {
  return `storage:${JSON.stringify([p.database_id, p.branch_id, p.pg_database, p.bucket_id])}`;
}

/** Reconnect delay: `min(30000, 1000 * 2^attempt) * (0.5 + 0.5 * random())`. */
export function backoffDelay(attempt: number, random: () => number): number {
  return Math.min(MAX_BACKOFF_MS, 1000 * 2 ** attempt) * (0.5 + 0.5 * random());
}

export interface WebSocketLike {
  readyState: number;
  send(data: string): void;
  close(code?: number, reason?: string): void;
  onopen: ((ev: unknown) => void) | null;
  onmessage: ((ev: { data: string }) => void) | null;
  onclose: ((ev: { code: number; reason?: string }) => void) | null;
  onerror: ((ev: unknown) => void) | null;
}

export interface RealtimeClientOptions {
  /** `VITE_API_URL`. */
  apiUrl: string;
  /** The current member token (the ticket request's bearer and the `auth` frame). */
  token: string | undefined;
  fetch?: typeof fetch;
  WebSocket?: new (url: string, protocols?: string | string[]) => WebSocketLike;
  random?: () => number;
  /** Where `visibilitychange` is heard; defaults to `document`. */
  document?: Pick<Document, "visibilityState" | "addEventListener" | "removeEventListener">;
}

interface Entry {
  kind: "table" | "project";
  /** The canonical channel id: what every frame after `joined` carries. */
  channel: string;
  /** The join frame (without its ref). */
  join: Record<string, unknown>;
  table?: TableHandlers;
  project?: ProjectHandlers;
  /** The ref of a join sent and not answered yet. */
  ref: string | null;
  joined: boolean;
  /** Left because the tab stayed hidden (P9): joined again when it's shown. */
  parked: boolean;
  /** The server refused the join for good (`forbidden`/`not_found`): not joined again on a reconnect. */
  refused: boolean;
  /** Transient refusals in a row since the last `joined`: the join retry's backoff exponent. */
  refusals: number;
  /** A join retry waiting for its backoff after a transient refusal. */
  rejoin: ReturnType<typeof setTimeout> | null;
  view: Key[] | null;
  presence: PresenceState | null;
}

type Frame = Record<string, unknown>;

const OPEN = 1;

/** The socket is opened lazily, on the first join. */
export function createRealtimeClient(opts: RealtimeClientOptions): RealtimeClient {
  const doFetch = opts.fetch ?? ((...args: Parameters<typeof fetch>) => globalThis.fetch(...args));
  const Socket = opts.WebSocket ?? (globalThis.WebSocket as unknown as new (url: string, protocols?: string | string[]) => WebSocketLike);
  const random = opts.random ?? Math.random;
  const doc = opts.document ?? (typeof document !== "undefined" ? document : undefined);

  let token = opts.token;
  const entries = new Set<Entry>();
  let ws: WebSocketLike | null = null;
  /** idle: nothing open or pending; connecting: a ticket or an upgrade is under way; waiting: a reconnect timer runs. */
  let phase: "idle" | "connecting" | "open" | "waiting" | "revoked" = "idle";
  let attempt = 0;
  let retry: ReturnType<typeof setTimeout> | null = null;
  let hiddenTimer: ReturnType<typeof setTimeout> | null = null;
  let hiddenLeft = false;
  let closed = false;
  let refs = 0;
  const nextRef = () => String(++refs);

  const send = (frame: Frame) => {
    if (ws && phase === "open" && ws.readyState === OPEN) ws.send(JSON.stringify(frame));
  };

  function cancelRejoin(e: Entry) {
    if (e.rejoin !== null) clearTimeout(e.rejoin);
    e.rejoin = null;
  }

  function sendJoin(e: Entry) {
    cancelRejoin(e);
    e.ref = nextRef();
    e.joined = false;
    send({ type: "join", ref: e.ref, ...e.join });
  }

  function ensure() {
    if (closed || phase !== "idle" || entries.size === 0 || token === undefined) return;
    void connect();
  }

  async function connect() {
    phase = "connecting";
    const bearer = token;
    let ticket: string | null = null;
    try {
      const res = await doFetch(`${opts.apiUrl.replace(/\/+$/, "")}/v1/realtime/tickets`, {
        method: "POST",
        headers: { Authorization: `Bearer ${bearer}` },
      });
      if (res.status === 201) {
        const body = (await res.json()) as { ticket?: unknown };
        if (typeof body.ticket === "string") ticket = body.ticket;
      }
    } catch {
      ticket = null;
    }
    if (closed) return;
    if (ticket === null) return scheduleReconnect();
    const socket = new Socket(socketUrl(opts.apiUrl, ticket), [SUBPROTOCOL]);
    ws = socket;
    socket.onopen = () => {
      if (ws !== socket) return;
      phase = "open";
      attempt = 0;
      // First, within the server's 5 s (R9).
      send({ type: "auth", token });
      for (const e of entries) if (!e.parked && !e.refused && e.rejoin === null) sendJoin(e);
    };
    socket.onmessage = (ev) => {
      if (ws === socket) receive(ev.data);
    };
    socket.onclose = (ev) => {
      if (ws === socket) dropped(ev.code);
    };
    socket.onerror = () => {};
  }

  function scheduleReconnect() {
    if (closed) return;
    phase = "waiting";
    const delay = backoffDelay(attempt, random);
    attempt += 1;
    retry = setTimeout(() => {
      retry = null;
      phase = "idle";
      ensure();
    }, delay);
  }

  function dropped(code: number) {
    ws = null;
    for (const e of entries) {
      const wasUp = e.joined || e.ref !== null;
      e.joined = false;
      e.ref = null;
      if (e.parked || e.refused || !wasUp) continue;
      if (e.kind === "table") e.table?.onStatus?.({ live: false, reason: "reconnecting" });
      else e.project?.onLive?.(false);
    }
    if (closed) return;
    if (code === REVOKED) {
      // Access was revoked (or the identity changed): no retries; a new join tries again.
      phase = "revoked";
      const err: RealtimeError = { code: "forbidden", message: "Access to live updates was revoked." };
      for (const e of entries) (e.table ?? e.project)?.onError?.(err);
      return;
    }
    scheduleReconnect();
  }

  const joinedOn = (channel: unknown) => {
    for (const e of entries) if (e.joined && e.channel === channel) return e;
    return undefined;
  };

  function receive(data: string) {
    let f: Frame;
    try {
      const parsed: unknown = JSON.parse(data);
      if (typeof parsed !== "object" || parsed === null || Array.isArray(parsed)) return;
      f = parsed as Frame;
    } catch {
      return;
    }
    switch (f.type) {
      case "joined": {
        const e = [...entries].find((x) => x.ref !== null && x.ref === f.ref);
        if (!e) return;
        e.ref = null;
        e.joined = true;
        e.refusals = 0;
        if (typeof f.channel === "string") e.channel = f.channel;
        if (e.kind === "project") return e.project?.onLive?.(true);
        const info = (f.info ?? {}) as { pk?: string[] | null; live?: boolean; reason?: never };
        e.table?.onJoined?.({ pk: info.pk ?? null, live: info.live === true, ...(info.reason ? { reason: info.reason } : {}) });
        // The server keeps no view set or presence across joins.
        if (e.view) send({ type: "view", channel: e.channel, keys: e.view });
        if (e.presence) send({ type: "presence", channel: e.channel, state: e.presence });
        return;
      }
      case "error": {
        const byRef = f.ref !== undefined ? [...entries].find((x) => x.ref !== null && x.ref === f.ref) : undefined;
        const e = byRef ?? joinedOn(f.channel);
        if (!e) return;
        if (byRef) {
          byRef.ref = null;
          if (STICKY_REFUSALS.has(String(f.code))) byRef.refused = true;
          else scheduleRejoin(byRef);
        }
        (e.table ?? e.project)?.onError?.({ code: f.code as RealtimeError["code"], message: typeof f.message === "string" ? f.message : "" });
        return;
      }
      case "project": {
        const e = joinedOn(f.channel);
        if (e?.kind !== "project") return;
        const { type: _type, channel: _channel, ...event } = f;
        e.project?.onEvent(event as never);
        return;
      }
      case "resync": {
        const e = joinedOn(f.channel);
        (e?.table ?? e?.project)?.onResync?.(typeof f.reason === "string" ? f.reason : "");
        return;
      }
    }
    const e = joinedOn(f.channel);
    if (e?.kind !== "table" || !e.table) return;
    const h = e.table;
    switch (f.type) {
      case "rows":
        h.onRows?.({ seq: f.seq as number, changes: (f.changes ?? []) as never });
        return;
      case "status":
        h.onStatus?.({ live: f.live === true, reason: f.reason as never });
        return;
      case "presence_state":
        h.onPresenceState?.((f.members ?? []) as never);
        return;
      case "presence_diff":
        h.onPresenceDiff?.({ joins: (f.joins ?? []) as never, leaves: (f.leaves ?? []) as never });
        return;
    }
  }

  /** A transient join refusal: join again after `backoffDelay` (doubling per refusal); a leave cancels it. */
  function scheduleRejoin(e: Entry) {
    cancelRejoin(e);
    const delay = backoffDelay(e.refusals, random);
    e.refusals += 1;
    e.rejoin = setTimeout(() => {
      e.rejoin = null;
      if (closed || !entries.has(e) || e.parked || e.refused) return;
      // Not open: the socket's next `onopen` joins it.
      if (phase === "open") sendJoin(e);
      else ensure();
    }, delay);
  }

  function add(e: Entry) {
    entries.add(e);
    if (phase === "revoked") phase = "idle";
    if (e.parked) return;
    if (phase === "open") sendJoin(e);
    else ensure();
  }

  function leave(e: Entry) {
    if (!entries.delete(e)) return;
    cancelRejoin(e);
    if (e.joined || e.ref !== null) send({ type: "leave", ref: nextRef(), channel: e.channel });
    e.joined = false;
    e.ref = null;
  }

  function onVisibility() {
    if (!doc || closed) return;
    if (doc.visibilityState === "hidden") {
      if (hiddenTimer !== null || hiddenLeft) return;
      hiddenTimer = setTimeout(() => {
        hiddenTimer = null;
        hiddenLeft = true;
        for (const e of entries) {
          if (e.kind !== "table" || e.parked) continue;
          if (e.joined || e.ref !== null) send({ type: "leave", ref: nextRef(), channel: e.channel });
          e.joined = false;
          e.ref = null;
          e.parked = true;
        }
      }, HIDDEN_LEAVE_MS);
      return;
    }
    if (hiddenTimer !== null) clearTimeout(hiddenTimer);
    hiddenTimer = null;
    if (!hiddenLeft) return;
    hiddenLeft = false;
    for (const e of entries) {
      if (!e.parked) continue;
      e.parked = false;
      if (phase === "open" && !e.refused && e.rejoin === null) sendJoin(e);
    }
    ensure();
  }
  doc?.addEventListener("visibilitychange", onVisibility);

  function setToken(next: string) {
    if (closed || next === token) return;
    token = next;
    send({ type: "auth", token });
    ensure();
  }

  function close() {
    if (closed) return;
    closed = true;
    for (const e of entries) cancelRejoin(e);
    entries.clear();
    if (retry !== null) clearTimeout(retry);
    if (hiddenTimer !== null) clearTimeout(hiddenTimer);
    doc?.removeEventListener("visibilitychange", onVisibility);
    const socket = ws;
    ws = null;
    phase = "idle";
    if (socket) {
      socket.onopen = socket.onmessage = socket.onclose = socket.onerror = null;
      socket.close(1000);
    }
  }

  function entry(kind: Entry["kind"], channel: string, join: Record<string, unknown>): Entry {
    return { kind, channel, join, ref: null, joined: false, parked: kind === "table" && hiddenLeft, refused: false, refusals: 0, rejoin: null, view: null, presence: null };
  }

  return {
    joinTable(params: TableJoinParams, handlers: TableHandlers): TableSubscription {
      const e: Entry = { ...entry("table", tableChannelId(params), { channel: "table", params: { ...params } }), table: handlers };
      if (!closed) add(e);
      return {
        get channel() {
          return e.channel;
        },
        leave: () => leave(e),
        view(keys) {
          e.view = keys.slice(0, MAX_VIEW_KEYS);
          if (e.joined) send({ type: "view", channel: e.channel, keys: e.view });
        },
        presence(state) {
          e.presence = { ...state };
          if (e.joined) send({ type: "presence", channel: e.channel, state: e.presence });
        },
      };
    },
    joinStorage(params: StorageJoinParams, handlers: TableHandlers): StorageSubscription {
      // A table entry in all but its join: rows, status and resyncs by channel, parked while hidden.
      const e: Entry = { ...entry("table", storageChannelId(params), { channel: "storage", params: { ...params } }), table: handlers };
      if (!closed) add(e);
      return {
        get channel() {
          return e.channel;
        },
        leave: () => leave(e),
        view(keys) {
          e.view = keys.slice(0, MAX_VIEW_KEYS);
          if (e.joined) send({ type: "view", channel: e.channel, keys: e.view });
        },
      };
    },
    joinProject(projectId: string, handlers: ProjectHandlers): ProjectSubscription {
      const channel = `project:${projectId}`;
      const e: Entry = { ...entry("project", channel, { channel }), project: handlers };
      if (!closed) add(e);
      return {
        channel,
        leave: () => leave(e),
      };
    },
    setToken,
    attachAuth(events: SessionEvents) {
      const loaded = (user: { access_token: string }) => setToken(user.access_token);
      const out = () => close();
      events.addUserLoaded(loaded);
      events.addUserUnloaded(out);
      events.addUserSignedOut(out);
      return () => {
        events.removeUserLoaded(loaded);
        events.removeUserUnloaded(out);
        events.removeUserSignedOut(out);
      };
    },
    close,
  };
}
