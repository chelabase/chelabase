/** Shared shapes of the realtime socket protocol (spec §2 as amended by S11, S17) and Studio's client. */

/** A row key: `JSON.stringify` of the primary-key columns' text output, in key order (S11). */
export type Key = string;

export interface TableJoinParams {
  database_id: string;
  branch_id: string;
  pg_database: string;
  schema: string;
  table: string;
  /** The Act-as role; null = the owner. */
  role: string | null;
}

/** Object storage m17: a bucket's objects (`storage.objects` of one bucket), as the Act-as role. */
export interface StorageJoinParams {
  database_id: string;
  branch_id: string;
  pg_database: string;
  bucket_id: string;
  /** The Act-as role; null = owner mode. */
  role: string | null;
}

export type StatusReason =
  | "no_slot"
  | "waiting"
  | "no_connect"
  | "read_only"
  | "not_a_table"
  | "unavailable"
  | "compute_stopping"
  | "reconnecting"
  | "unsupported";

export type ChangeOp = "insert" | "update" | "delete" | "changed";

/** One change of a `rows` frame: `keys` are key strings; absent for a keyless delete and for `changed`. */
export interface Change {
  op: ChangeOp;
  keys?: Key[];
}

export interface TableInfo {
  pk: string[] | null;
  live: boolean;
  reason?: StatusReason;
}

export interface PresenceState {
  row: Key | null;
  column: string | null;
  editing: boolean;
}

export interface PresenceMember {
  id: string;
  sub: string;
  name: string | null;
  email: string | null;
  /** The act-as role; null = the owner. */
  role: string | null;
  color_seed: number;
  state: PresenceState;
}

/** The `project` frame's event: the `ProjectEvent` fields flattened next to `type` and `channel` (S17). */
export type ProjectEvent =
  | { event: "branch.status"; database_id: string; branch_id: string; status: string }
  | { event: "branch.created"; database_id: string; branch_id: string }
  | { event: "branch.deleted"; database_id: string; branch_id: string }
  | { event: "database.created"; database_id: string }
  | { event: "database.deleted"; database_id: string }
  | { event: "pg_database.created"; database_id: string; branch_id: string; name: string }
  | { event: "pg_database.dropped"; database_id: string; branch_id: string; name: string };

export interface RealtimeError {
  code: "forbidden" | "not_found" | "limit" | "invalid" | "unavailable";
  message: string;
}

export interface TableHandlers {
  /** The server's `joined` (and again after every rejoin: a reconnect, or a tab shown after >5 min hidden). */
  onJoined?(info: TableInfo): void;
  onError?(err: RealtimeError): void;
  onRows?(frame: { seq: number; changes: Change[] }): void;
  /** The server's `status`, and the client's own `{live: false, reason: "reconnecting"}` while the socket is down. */
  onStatus?(status: { live: boolean; reason?: StatusReason }): void;
  onResync?(reason: string): void;
  onPresenceState?(members: PresenceMember[]): void;
  onPresenceDiff?(diff: { joins: PresenceMember[]; leaves: PresenceMember[] }): void;
}

export interface ProjectHandlers {
  onEvent(event: ProjectEvent): void;
  /** True on every `joined` (a rejoin after a reconnect too), false while the socket is down: the polling fallback (P10). */
  onLive?(live: boolean): void;
  onResync?(reason: string): void;
  onError?(err: RealtimeError): void;
}

export interface TableSubscription {
  /** The canonical channel id (`table:` + compact JSON array), the string every frame carries. */
  readonly channel: string;
  leave(): void;
  /** R19: the keys on screen (at most 1000 are sent; replaces the previous set). */
  view(keys: Key[]): void;
  /** The viewer's own selection state. */
  presence(state: PresenceState): void;
}

/** A storage channel: keys are object ids (`JSON.stringify([id])`); no presence. */
export interface StorageSubscription {
  /** The canonical channel id (`storage:` + compact JSON array). */
  readonly channel: string;
  leave(): void;
  /** R19: the object keys on screen (at most 1000 are sent; replaces the previous set). */
  view(keys: Key[]): void;
}

export interface ProjectSubscription {
  readonly channel: string;
  leave(): void;
}

/**
 * The member-session events the client listens to (see `realtimeAuthEvents` in `lib/session.ts`):
 * a renewed member token is `userLoaded`, sign-out is `userUnloaded` and `userSignedOut`.
 */
export interface SessionEvents {
  addUserLoaded(cb: (user: { access_token: string }) => void): void;
  removeUserLoaded(cb: (user: { access_token: string }) => void): void;
  addUserUnloaded(cb: () => void): void;
  removeUserUnloaded(cb: () => void): void;
  addUserSignedOut(cb: () => void): void;
  removeUserSignedOut(cb: () => void): void;
}

export interface RealtimeClient {
  joinTable(params: TableJoinParams, handlers: TableHandlers): TableSubscription;
  joinProject(projectId: string, handlers: ProjectHandlers): ProjectSubscription;
  /** Object storage m17: a bucket's object changes (the table handlers, without presence frames). */
  joinStorage(params: StorageJoinParams, handlers: TableHandlers): StorageSubscription;
  /** A renewed member token: an `auth` frame goes out at once on an open socket; later tickets and `auth` frames use it. */
  setToken(token: string): void;
  /** Renewal on every `userLoaded`; sign-out on `userSignedOut` and `userUnloaded` (W32). Returns the detach function. */
  attachAuth(events: SessionEvents): () => void;
  /** Closes the socket for good (sign-out): no reconnect, every subscription dropped. */
  close(): void;
}
