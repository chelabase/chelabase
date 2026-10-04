import { streamRequest } from "@chelabase/management-client";

export type RunEvent =
  | { t: "notice"; severity: string; code: string; message: string; detail: string | null; hint: string | null; position: number | null }
  | { t: "columns"; statement: number; columns: { name: string; type: string }[] }
  | { t: "rows"; statement: number; rows: (string | null)[][] }
  | { t: "complete"; statement: number; tag: string; rows: number; truncated: boolean }
  | { t: "error"; statement: number; code: string; message: string; detail: string | null; hint: string | null; position: number | null }
  | { t: "heartbeat" }
  | { t: "done"; outcome: "ok" | "error" | "cancelled" | "timeout" | "truncated_response"; duration_ms: number };

export type StatementResult = {
  index: number;
  columns: { name: string; type: string }[];
  rows: (string | null)[][];
  tag?: string;
  rowCount?: number;
  truncated: boolean;
};

export type RunState = {
  running: boolean;
  statements: StatementResult[];
  notices: Extract<RunEvent, { t: "notice" }>[];
  error?: Extract<RunEvent, { t: "error" }>;
  done?: Extract<RunEvent, { t: "done" }>;
};

export const initialRunState: RunState = { running: true, statements: [], notices: [] };

function upsert(statements: StatementResult[], index: number, patch: (s: StatementResult) => StatementResult): StatementResult[] {
  const at = statements.findIndex((s) => s.index === index);
  if (at >= 0) return statements.map((s, i) => (i === at ? patch(s) : s));
  const created = patch({ index, columns: [], rows: [], truncated: false });
  return [...statements, created].sort((a, b) => a.index - b.index);
}

export function runReducer(state: RunState, event: RunEvent): RunState {
  switch (event.t) {
    case "columns":
      return { ...state, statements: upsert(state.statements, event.statement, (s) => ({ ...s, columns: event.columns })) };
    case "rows":
      return { ...state, statements: upsert(state.statements, event.statement, (s) => ({ ...s, rows: [...s.rows, ...event.rows] })) };
    case "complete":
      return {
        ...state,
        statements: upsert(state.statements, event.statement, (s) => ({ ...s, tag: event.tag, rowCount: event.rows, truncated: event.truncated })),
      };
    case "notice":
      return { ...state, notices: [...state.notices, event] };
    case "error":
      return { ...state, error: event };
    case "done":
      return { ...state, done: event, running: false };
    default:
      return state; // heartbeat and unknown events (forward compatible)
  }
}

/** Parses an NDJSON stream into events; throws on a line that isn't JSON. */
export async function* readNdjson(stream: ReadableStream<Uint8Array>): AsyncGenerator<RunEvent> {
  const reader = stream.getReader();
  const decoder = new TextDecoder();
  let buffer = "";
  const parse = (line: string): RunEvent => {
    try {
      return JSON.parse(line) as RunEvent;
    } catch {
      throw new Error(`Invalid NDJSON line: ${line}`);
    }
  };
  try {
    for (;;) {
      const { done, value } = await reader.read();
      buffer += decoder.decode(value, { stream: !done });
      let nl: number;
      while ((nl = buffer.indexOf("\n")) >= 0) {
        const line = buffer.slice(0, nl).trim();
        buffer = buffer.slice(nl + 1);
        if (line !== "") yield parse(line);
      }
      if (done) break;
    }
    const rest = buffer.trim();
    if (rest !== "") yield parse(rest);
  } finally {
    reader.releaseLock();
  }
}

export type RunBody = { run_id: string; database: string | null; role: string | null; sql: string; timeout_ms: number; confirm: boolean };

export async function startRun(databaseId: string, branchId: string, body: RunBody, signal: AbortSignal): Promise<AsyncGenerator<RunEvent>> {
  const stream = await streamRequest(`/v1/databases/${encodeURIComponent(databaseId)}/branches/${encodeURIComponent(branchId)}/sql`, body, signal);
  return readNdjson(stream);
}

/** Postgres's 1-based code-point position in the sent text → 0-based UTF-16 index in the editor document. */
export function errorOffset(sent: string, position: number, sentFrom: number): number {
  let units = 0;
  let points = 0;
  for (const ch of sent) {
    if (points >= position - 1) break;
    units += ch.length;
    points++;
  }
  return sentFrom + units;
}

const DDL = /^(CREATE|ALTER|DROP|COMMENT|GRANT|REVOKE)\b/i;

/** True when any tag is DDL that should refresh the schema (CREATE, ALTER, DROP, COMMENT, GRANT, REVOKE). */
export function changesSchema(tags: string[]): boolean {
  return tags.some((t) => DDL.test(t.trim()));
}
