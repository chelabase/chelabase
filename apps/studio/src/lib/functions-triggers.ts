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

/** Whether any trigger is still being installed (the section polls until none is). */
export function triggersInstalling(triggers: Pick<FunctionTrigger, "state">[]): boolean {
  return triggers.some((t) => t.state === "pending_install");
}

export const TRIGGERS_DOCS_URL = "https://docs.chelabase.com/using/functions/triggers";

export interface TriggerFailure {
  /** What went wrong, in one sentence. */
  cause: string;
  /** What to do about it. */
  fix: string;
}

const RESTART = " Then turn the trigger off and on.";

/** The worker's `last_error` codes (and messages) for a failed install, with what they mean. */
const TRIGGER_FAILURES: Record<string, TriggerFailure> = {
  run_as_role_required: {
    cause: "This function has no run-as role on the branch.",
    fix: "Set a run-as role on this function." + RESTART,
  },
  run_as_role_cannot_read_table: {
    cause: "The run-as role is gone, or can't read this table.",
    fix: "Grant SELECT on the table and USAGE on its schema to the run-as role, or set the role again." + RESTART,
  },
  run_as_role_subject_to_rls: {
    cause: "Row level security on this table applies to the run-as role.",
    fix: "Use a BYPASSRLS run-as role, or the table's owner on a table without FORCE ROW LEVEL SECURITY." + RESTART,
  },
  invalid_events: {
    cause: "This trigger's events aren't valid for a table trigger.",
    fix: "Delete it and add it again with insert, update or delete.",
  },
  "function triggers support plain tables only": {
    cause: "Table triggers support plain tables only.",
    fix: "Delete it and pick an ordinary table, not a partitioned table or a view.",
  },
  "function triggers can't go on a reserved schema": {
    cause: "Triggers can't go on a Chelabase or system schema.",
    fix: "Delete it and pick a table in your own schema.",
  },
};

/** Plain guidance for a failed trigger's `last_error`; null for a reason Studio doesn't know (show the raw text). */
export function describeTriggerFailure(lastError: string): TriggerFailure | null {
  return Object.hasOwn(TRIGGER_FAILURES, lastError) ? TRIGGER_FAILURES[lastError]! : null;
}
