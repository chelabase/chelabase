import type {
  SessionEvents,
  PresenceMember,
  PresenceState,
  ProjectHandlers,
  ProjectSubscription,
  RealtimeClient,
  TableHandlers,
  TableInfo,
  TableJoinParams,
  TableSubscription,
} from "@/lib/realtime/types";
import { tableChannelId } from "@/lib/realtime/client";

/** A fake WebSocket the client tests drive by hand. */
export class FakeWebSocket {
  static instances: FakeWebSocket[] = [];
  static reset() {
    FakeWebSocket.instances = [];
  }
  readyState = 0;
  sent: Record<string, unknown>[] = [];
  closed: { code?: number } | null = null;
  onopen: ((ev: unknown) => void) | null = null;
  onmessage: ((ev: { data: string }) => void) | null = null;
  onclose: ((ev: { code: number; reason?: string }) => void) | null = null;
  onerror: ((ev: unknown) => void) | null = null;
  constructor(
    public url: string,
    public protocols?: string | string[],
  ) {
    FakeWebSocket.instances.push(this);
  }
  send(data: string) {
    this.sent.push(JSON.parse(data) as Record<string, unknown>);
  }
  close(code?: number) {
    this.closed = { code };
    this.readyState = 3;
  }
  /** The server accepted the upgrade. */
  open() {
    this.readyState = 1;
    this.onopen?.({});
  }
  /** The server sent a frame. */
  receive(frame: Record<string, unknown>) {
    this.onmessage?.({ data: JSON.stringify(frame) });
  }
  /** The server (or the network) closed the socket. */
  drop(code = 1006) {
    this.readyState = 3;
    this.onclose?.({ code });
  }
  frames(type: string) {
    return this.sent.filter((f) => f.type === type);
  }
}

export interface FakeTable {
  params: TableJoinParams;
  handlers: TableHandlers;
  channel: string;
  left: boolean;
  views: string[][];
  presence: PresenceState[];
}

/** A `RealtimeClient` that records joins and lets a test play the server. */
export class FakeRealtime implements RealtimeClient {
  tables: FakeTable[] = [];
  projects: { projectId: string; handlers: ProjectHandlers; left: boolean }[] = [];
  tokens: string[] = [];
  closed = false;

  joinTable(params: TableJoinParams, handlers: TableHandlers): TableSubscription {
    const entry: FakeTable = { params, handlers, channel: tableChannelId(params), left: false, views: [], presence: [] };
    this.tables.push(entry);
    return {
      channel: entry.channel,
      leave: () => {
        entry.left = true;
      },
      view: (keys) => entry.views.push(keys),
      presence: (state) => entry.presence.push(state),
    };
  }
  joinProject(projectId: string, handlers: ProjectHandlers): ProjectSubscription {
    const entry = { projectId, handlers, left: false };
    this.projects.push(entry);
    return {
      channel: `project:${projectId}`,
      leave: () => {
        entry.left = true;
      },
    };
  }
  setToken(token: string) {
    this.tokens.push(token);
  }
  attachAuth(_events: SessionEvents) {
    return () => {};
  }
  close() {
    this.closed = true;
  }

  /** The joins still active. */
  get active(): FakeTable[] {
    return this.tables.filter((t) => !t.left);
  }
  /** The newest active join. */
  get table(): FakeTable {
    const t = this.active.at(-1);
    if (!t) throw new Error("no active table join");
    return t;
  }
  joined(info: Partial<TableInfo> = {}) {
    this.table.handlers.onJoined?.({ pk: ["id"], live: true, ...info });
  }
  rows(seq: number, changes: { op: "insert" | "update" | "delete" | "changed"; keys?: string[] }[]) {
    this.table.handlers.onRows?.({ seq, changes });
  }
  status(live: boolean, reason?: TableInfo["reason"]) {
    this.table.handlers.onStatus?.({ live, reason });
  }
  resync(reason = "test") {
    this.table.handlers.onResync?.(reason);
  }
  presenceState(members: PresenceMember[]) {
    this.table.handlers.onPresenceState?.(members);
  }
  presenceDiff(joins: PresenceMember[], leaves: PresenceMember[] = []) {
    this.table.handlers.onPresenceDiff?.({ joins, leaves });
  }
}

export function member(over: Partial<PresenceMember> & { id: string }): PresenceMember {
  return {
    sub: `sub-${over.id}`,
    name: null,
    email: null,
    role: null,
    color_seed: 0,
    state: { row: null, column: null, editing: false },
    ...over,
  };
}
