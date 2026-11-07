/**
 * The upload queue as pure state: each file is queued, uploading, paused, done, failed or
 * skipped. Only `start` makes a file uploading, and it refuses past `MAX_CONCURRENT`, so no
 * sequence of actions runs more at once; no action but `clear` removes a file. The effects
 * (requests, aborts) live in `use-upload-queue.ts`.
 */

export type UploadStatus = "queued" | "uploading" | "paused" | "done" | "failed" | "skipped";

/** What the user chose for a taken name. */
export type ConflictChoice = "replace" | "keep-both";

/** Why a file failed, for a person. */
export interface UploadFailure {
  message: string;
  /** Busy, stalled, network: trying again may work. */
  retryable: boolean;
  /** The name is taken: the tray offers skip, replace or keep both. */
  conflict: boolean;
}

export interface UploadItem {
  id: string;
  file: File;
  bucket: string;
  /** The name asked for: the current folder and the dropped relative path. */
  key: string;
  /** The name it's going to (or went to): `key`, or a suffixed one after Keep both. */
  name: string;
  status: UploadStatus;
  loaded: number;
  total: number;
  /** Bytes a second, smoothed; null until measured. */
  speed: number | null;
  error: UploadFailure | null;
  choice: ConflictChoice | null;
  /** A multipart upload's session, kept across pause and failure so it resumes. */
  session: string | null;
  /** Skipped by Cancel (rather than by choosing Skip on a conflict). */
  cancelled: boolean;
  /** The last progress sample, for the speed. */
  sample: { at: number; loaded: number } | null;
}

export interface QueueState {
  items: UploadItem[];
}

export interface NewUpload {
  id: string;
  file: File;
  bucket: string;
  key: string;
}

export type QueueAction =
  | { type: "add"; uploads: NewUpload[] }
  | { type: "start"; id: string; at?: number }
  | { type: "progress"; id: string; loaded: number; at: number }
  | { type: "session"; id: string; session: string | null }
  | { type: "name"; id: string; name: string }
  | { type: "done"; id: string; name: string }
  | { type: "fail"; id: string; error: UploadFailure }
  | { type: "pause"; id: string }
  | { type: "resume"; id: string }
  | { type: "retry"; id: string }
  | { type: "cancel"; id: string }
  | { type: "resolve"; id: string; choice: "skip" | ConflictChoice }
  | { type: "clear" };

/** Uploads at once, across the tab. */
export const MAX_CONCURRENT = 4;

export const EMPTY_QUEUE: QueueState = { items: [] };

/** Weight of the newest speed sample. */
const SMOOTHING = 0.3;

const running = (s: QueueState) => s.items.filter((i) => i.status === "uploading").length;

function update(s: QueueState, id: string, f: (i: UploadItem) => UploadItem | null): QueueState {
  let changed = false;
  const items = s.items.map((i) => {
    if (i.id !== id) return i;
    const next = f(i);
    if (next === null || next === i) return i;
    changed = true;
    return next;
  });
  return changed ? { items } : s;
}

export function queueReducer(s: QueueState, a: QueueAction): QueueState {
  switch (a.type) {
    case "add": {
      const have = new Set(s.items.map((i) => i.id));
      const fresh: UploadItem[] = [];
      for (const u of a.uploads) {
        if (have.has(u.id)) continue;
        have.add(u.id);
        fresh.push({
          ...u,
          name: u.key,
          status: "queued",
          loaded: 0,
          total: u.file.size,
          speed: null,
          error: null,
          choice: null,
          session: null,
          cancelled: false,
          sample: null,
        });
      }
      return fresh.length === 0 ? s : { items: [...s.items, ...fresh] };
    }
    case "start":
      if (running(s) >= MAX_CONCURRENT) return s;
      return update(s, a.id, (i) =>
        i.status === "queued" ? { ...i, status: "uploading", speed: null, sample: a.at === undefined ? null : { at: a.at, loaded: i.loaded } } : null,
      );
    case "progress":
      return update(s, a.id, (i) => {
        if (i.status !== "uploading") return null;
        const loaded = Math.max(0, Math.min(i.total, a.loaded));
        let speed = i.speed;
        if (i.sample && a.at > i.sample.at) {
          const now = ((loaded - i.sample.loaded) * 1000) / (a.at - i.sample.at);
          if (now >= 0) speed = speed === null ? now : speed + SMOOTHING * (now - speed);
        }
        return { ...i, loaded, speed, sample: { at: a.at, loaded } };
      });
    case "session":
      return update(s, a.id, (i) => (i.session === a.session ? null : { ...i, session: a.session }));
    case "name":
      return update(s, a.id, (i) => (i.status === "uploading" && i.name !== a.name ? { ...i, name: a.name } : null));
    case "done":
      return update(s, a.id, (i) =>
        i.status === "uploading" ? { ...i, status: "done", name: a.name, loaded: i.total, session: null, error: null, speed: null, sample: null } : null,
      );
    case "fail":
      return update(s, a.id, (i) => (i.status === "uploading" ? { ...i, status: "failed", error: a.error, speed: null, sample: null } : null));
    case "pause":
      return update(s, a.id, (i) => (i.status === "queued" || i.status === "uploading" ? { ...i, status: "paused", speed: null, sample: null } : null));
    case "resume":
      return update(s, a.id, (i) => (i.status === "paused" ? { ...i, status: "queued" } : null));
    case "retry":
      return update(s, a.id, (i) => (i.status === "failed" && !i.error?.conflict ? { ...i, status: "queued", error: null } : null));
    case "cancel":
      return update(s, a.id, (i) =>
        i.status === "done" || i.status === "skipped" ? null : { ...i, status: "skipped", cancelled: true, error: null, speed: null, sample: null },
      );
    case "resolve":
      return update(s, a.id, (i) => {
        if (i.status !== "failed" || !i.error?.conflict) return null;
        if (a.choice === "skip") return { ...i, status: "skipped", error: null };
        return { ...i, status: "queued", error: null, choice: a.choice };
      });
    case "clear": {
      const items = s.items.filter((i) => i.status !== "done" && i.status !== "skipped");
      return items.length === s.items.length ? s : { items };
    }
  }
}

/** The queued files that may start now, in order, filling up to `MAX_CONCURRENT`. */
export function nextToStart(s: QueueState): string[] {
  const room = MAX_CONCURRENT - running(s);
  if (room <= 0) return [];
  return s.items.filter((i) => i.status === "queued").slice(0, room).map((i) => i.id);
}

/** Leaving the page would lose work: some file is queued, uploading or paused. */
export function hasActiveUploads(s: QueueState): boolean {
  return s.items.some((i) => i.status === "queued" || i.status === "uploading" || i.status === "paused");
}
