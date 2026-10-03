// Per-browser storage for the SQL view: run history, the open buffer and the timeout.
// Every access is wrapped: the page works when storage is missing, full or throwing.

export type HistoryEntry = {
  sql: string;
  branchId: string;
  /** The Postgres database; null is the branch's main database. */
  database: string | null;
  /** The role it ran as; null is the owner. */
  role: string | null;
  /** ISO time the run started. */
  at: string;
  outcome: string;
  durationMs: number;
};

export const HISTORY_MAX = 100;
export const DEFAULT_TIMEOUT_MS = 60_000;
export const TIMEOUTS_MS = [60_000, 300_000, 900_000] as const;
const TIMEOUT_KEY = "cheladb.sql.timeout";

const historyKey = (sub: string, databaseId: string) => `cheladb.sql.history.${sub}.${databaseId}`;
const bufferKey = (sub: string, databaseId: string) => `cheladb.sql.buffer.${sub}.${databaseId}`;

function read(key: string): string | null {
  try {
    return localStorage.getItem(key);
  } catch {
    return null;
  }
}
function write(key: string, value: string): void {
  try {
    localStorage.setItem(key, value);
  } catch {
    // storage unavailable or full: carry on without it
  }
}
function remove(key: string): void {
  try {
    localStorage.removeItem(key);
  } catch {
    // ignore
  }
}

function isEntry(v: unknown): v is HistoryEntry {
  if (!v || typeof v !== "object") return false;
  const e = v as Record<string, unknown>;
  return typeof e.sql === "string" && typeof e.branchId === "string" && typeof e.at === "string" && typeof e.outcome === "string" && typeof e.durationMs === "number";
}

/** Newest first. */
export function loadHistory(sub: string, databaseId: string): HistoryEntry[] {
  const raw = read(historyKey(sub, databaseId));
  if (raw === null) return [];
  try {
    const parsed: unknown = JSON.parse(raw);
    return Array.isArray(parsed) ? parsed.filter(isEntry).slice(0, HISTORY_MAX) : [];
  } catch {
    return [];
  }
}

/** Adds the newest entry (keeping at most 100) and returns the new list, even when storage fails. */
export function addHistory(sub: string, databaseId: string, entry: HistoryEntry): HistoryEntry[] {
  const next = [entry, ...loadHistory(sub, databaseId)].slice(0, HISTORY_MAX);
  write(historyKey(sub, databaseId), JSON.stringify(next));
  return next;
}

export function clearHistory(sub: string, databaseId: string): void {
  remove(historyKey(sub, databaseId));
}

export function loadBuffer(sub: string, databaseId: string): string {
  return read(bufferKey(sub, databaseId)) ?? "";
}

export function saveBuffer(sub: string, databaseId: string, sql: string): void {
  write(bufferKey(sub, databaseId), sql);
}

export function loadTimeout(): number {
  const n = Number(read(TIMEOUT_KEY));
  return (TIMEOUTS_MS as readonly number[]).includes(n) ? n : DEFAULT_TIMEOUT_MS;
}

export function saveTimeout(ms: number): void {
  write(TIMEOUT_KEY, String(ms));
}
