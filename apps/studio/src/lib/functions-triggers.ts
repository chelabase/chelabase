import type { CreateFunctionTriggerRequest, FunctionTrigger, FunctionTriggerSource } from "@chelabase/management-client";

export const TABLE_EVENTS = ["insert", "update", "delete"] as const;
export const STORAGE_EVENTS = ["object.created", "object.updated", "object.deleted", "object.moved"] as const;

const eventsOf = (source: FunctionTriggerSource): readonly string[] => (source === "table" ? TABLE_EVENTS : STORAGE_EVENTS);

/** The add-trigger form. `table` is `schema.table` (a bare name is in `public`). */
export interface TriggerDraft {
  source: FunctionTriggerSource;
  pgDatabase: string;
  table: string;
  bucket: string;
  prefix: string;
  events: string[];
}

export type TriggerDraftAction =
  | { type: "source"; source: FunctionTriggerSource }
  | { type: "toggle-event"; event: string }
  | { type: "set"; field: "pgDatabase" | "table" | "bucket" | "prefix"; value: string }
  | { type: "reset" };

export function initialTriggerDraft(pgDatabase: string): TriggerDraft {
  return { source: "table", pgDatabase, table: "", bucket: "", prefix: "", events: [...TABLE_EVENTS] };
}

export function triggerDraftReducer(state: TriggerDraft, action: TriggerDraftAction): TriggerDraft {
  switch (action.type) {
    case "source":
      // Events never mix sources: a new source starts with all of its own.
      return action.source === state.source ? state : { ...state, source: action.source, events: [...eventsOf(action.source)] };
    case "toggle-event": {
      const all = eventsOf(state.source);
      if (!all.includes(action.event)) return state;
      const on = state.events.includes(action.event);
      return { ...state, events: all.filter((e) => (e === action.event ? !on : state.events.includes(e))) };
    }
    case "set":
      return { ...state, [action.field]: action.value };
    case "reset":
      return initialTriggerDraft(state.pgDatabase);
  }
}

/** `schema.table` (each part optionally double-quoted) or a bare table in `public`; null when malformed. */
export function splitQualified(raw: string): { schema: string; table: string } | null {
  const s = raw.trim();
  if (s === "") return null;
  const parts: string[] = [];
  let cur = "";
  let quoted = false;
  for (let i = 0; i < s.length; i++) {
    const c = s[i];
    if (c === '"') {
      if (quoted && s[i + 1] === '"') {
        cur += '"';
        i++;
      } else quoted = !quoted;
    } else if (c === "." && !quoted) {
      parts.push(cur);
      cur = "";
    } else cur += c;
  }
  if (quoted) return null;
  parts.push(cur);
  if (parts.length > 2 || parts.some((p) => p === "")) return null;
  return parts.length === 1 ? { schema: "public", table: parts[0] } : { schema: parts[0], table: parts[1] };
}

/** Why a draft can't be sent (null: it can). Reserved schemas and the like are the API's to refuse. */
export function triggerReason(d: TriggerDraft): string | null {
  if (d.pgDatabase.trim() === "") return "Pick a Postgres database.";
  if (d.source === "table") {
    if (d.table.trim() === "") return "Enter a table (schema.table).";
    if (!splitQualified(d.table)) return "Write the table as schema.table, or a bare table name for public.";
  }
  if (d.events.length === 0) return "Pick at least one event.";
  return null;
}

export function triggerRequest(d: TriggerDraft): CreateFunctionTriggerRequest {
  const base = { source: d.source, pg_database: d.pgDatabase.trim(), events: [...d.events] };
  if (d.source === "table") {
    const q = splitQualified(d.table);
    return { ...base, schema: q?.schema ?? "", table: q?.table ?? "" };
  }
  const bucket = d.bucket.trim();
  const prefix = d.prefix.trim();
  return { ...base, ...(bucket ? { bucket } : {}), ...(prefix ? { prefix } : {}) };
}

/** Where a trigger listens: the table, or the bucket and prefix, in its Postgres database. */
export function triggerTarget(t: Pick<FunctionTrigger, "source" | "pg_database" | "schema" | "table" | "bucket" | "prefix">): string {
  if (t.source === "table") return `${t.pg_database} · ${t.schema}.${t.table}`;
  if (t.bucket) return `${t.pg_database} · ${t.bucket}/${t.prefix ?? ""}*`;
  return `${t.pg_database} · every bucket${t.prefix ? `, ${t.prefix}*` : ""}`;
}
