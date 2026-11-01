import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { ApiError, cancelSqlRun, useGetMe, useGetSqlSchema, type SavedQuery } from "@chelabase/management-client";
import { errorText } from "@/lib/errors";
import type { PglsClient } from "@/lib/pgls/client";
import { sqlExtensions } from "@/lib/pgls/codemirror";
import { changesSchema, errorOffset, initialRunState, runReducer, startRun, type RunState } from "@/lib/sql-run";
import { OwnerPasswordNotice } from "../owner-password-notice";
import { CodeEditor, type CodeEditorHandle, type RunText } from "./code-editor";
import { SavedQueriesPanel, type ActiveQuery } from "./saved-queries-panel";
import { SqlConfirmDialog, type HeldStatement } from "./sql-confirm-dialog";
import { SqlResults } from "./sql-results";
import { SqlToolbar } from "./sql-toolbar";
import { addHistory, clearHistory, loadBuffer, loadHistory, loadTimeout, saveBuffer, saveTimeout, type HistoryEntry } from "./run-history";

/** Where the editor runs, as the page's context bar has it: the editor takes the place and Act as, and picks none of its own. */
export interface SqlEditorProps {
  databaseId: string;
  branchId: string;
  /** The Postgres database to run in; null is the branch's main one. */
  pgDatabase: string | null;
  /** The Postgres databases of the branch (a saved query may name one). */
  pgDatabases: string[];
  /** The role acted as; null is the owner. */
  role: string | null;
  /** What the caller may act as here. */
  allowed: { owner: boolean; roles: string[] };
  /** The roles have loaded (until then nothing runs and no schema is read). */
  rolesReady: boolean;
  branchProtected?: boolean;
  /** A saved query or history entry names a Postgres database on this branch: the caller moves the context there. */
  onPgDatabase: (name: string) => void;
  /** A saved query or history entry names a role the caller may act as. */
  onRole: (role: string) => void;
  /** Starts the pgls worker (tests inject a fake). The default imports the client lazily, so the wasm stays out of the main bundle. */
  createClient?: () => Promise<PglsClient>;
}

type RunInFlight = {
  id: string;
  token: number;
  text: RunText;
  startedAt: number;
  branchId: string;
  database: string | null;
  role: string | null;
  finished: boolean;
};

const defaultCreateClient = () => import("@/lib/pgls/client").then((m) => m.createPglsClient());
/** The stand-in user id for storage keys when the caller's identity can't be read. */
const FALLBACK_SUB = "anonymous";

export function SqlEditor({ createClient, ...place }: SqlEditorProps) {
  const me = useGetMe();
  if (me.isPending) return <p className="p-6 text-sm text-chalk-dim">Loading the editor…</p>;
  return <SqlWorkspace {...place} sub={me.data?.user.sub ?? FALLBACK_SUB} createClient={createClient ?? defaultCreateClient} />;
}

function SqlWorkspace({
  databaseId,
  branchId,
  pgDatabase: dbParam,
  pgDatabases,
  role: roleParam,
  allowed,
  rolesReady,
  branchProtected,
  onPgDatabase,
  onRole,
  sub,
  createClient,
}: Omit<SqlEditorProps, "createClient"> & { sub: string; createClient: () => Promise<PglsClient> }) {
  // "" is the owner in the schema key and the notes below; the run body and history use null.
  const role = roleParam ?? "";
  const noRole = !allowed.owner && roleParam === null;

  const [timeoutMs, setTimeoutMs] = useState(loadTimeout);

  // The pgls worker, started when the view opens; "Basic completion" until it and the schema are ready.
  const [client, setClient] = useState<PglsClient | null>(null);
  useEffect(() => {
    let disposed = false;
    let created: PglsClient | null = null;
    void (async () => {
      try {
        const c = await createClient();
        created = c;
        await c.ready;
        if (disposed) c.dispose();
        else setClient(c);
      } catch {
        if (created) created.dispose();
      }
    })();
    return () => {
      disposed = true;
      created?.dispose();
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // The schema read follows the chosen role's USAGE (T9): `?role=` unless it's the owner.
  const schemaKey = `${branchId}|${dbParam}|${role}`;
  const schemaParams = { ...(dbParam ? { database: dbParam } : {}), ...(role !== "" ? { role } : {}) };
  const schemaQuery = useGetSqlSchema(databaseId, branchId, schemaParams, {
    query: { enabled: rolesReady && !noRole, staleTime: Infinity, retry: false },
  });
  const schemaData = schemaQuery.data;
  const [pglsSchema, setPglsSchema] = useState<{ key: string; ok: boolean } | null>(null);
  useEffect(() => {
    if (!client || schemaData === undefined) return;
    let stale = false;
    client.setSchema(JSON.stringify(schemaData)).then(
      () => !stale && setPglsSchema({ key: schemaKey, ok: true }),
      () => !stale && setPglsSchema({ key: schemaKey, ok: false }), // e.g. "ERROR: Invalid schema": never shown
    );
    return () => {
      stale = true;
    };
  }, [client, schemaData, schemaKey]);
  const pglsReady = client !== null && !schemaQuery.isError && pglsSchema?.key === schemaKey && pglsSchema.ok;
  const extensions = useMemo(() => sqlExtensions(pglsReady ? client : null), [pglsReady, client]);

  // Editor state.
  const editor = useRef<CodeEditorHandle>(null);
  const [doc, setDoc] = useState(() => loadBuffer(sub, databaseId));
  const initialDoc = useRef(doc);
  // The buffer as typed, until the debounced save writes it; flushed when the view goes.
  const unsaved = useRef<string | null>(null);
  useEffect(() => {
    unsaved.current = doc;
    const t = setTimeout(() => {
      saveBuffer(sub, databaseId, doc);
      unsaved.current = null;
    }, 300);
    return () => clearTimeout(t);
  }, [doc, sub, databaseId]);
  useEffect(
    () => () => {
      if (unsaved.current !== null) saveBuffer(sub, databaseId, unsaved.current);
      unsaved.current = null;
    },
    [sub, databaseId],
  );
  const [active, setActive] = useState<ActiveQuery | null>(null);
  const [history, setHistory] = useState<HistoryEntry[]>(() => loadHistory(sub, databaseId));
  const [loadNote, setLoadNote] = useState<string | null>(null);

  // The run.
  const [run, setRun] = useState<RunState | null>(null);
  const [failure, setFailure] = useState<string | null>(null);
  const [ownerUnknown, setOwnerUnknown] = useState(false);
  const [held, setHeld] = useState<HeldStatement[] | null>(null);
  const [errorAt, setErrorAt] = useState<number | null>(null);
  const controller = useRef<AbortController | null>(null);
  /** The run in flight: what it ran and where (history records these, not the pickers' current values); `finished` once its stream ended. */
  const current = useRef<RunInFlight | null>(null);
  const tokens = useRef(0);
  const lastText = useRef<RunText | null>(null);

  useEffect(() => () => controller.current?.abort(), []);

  const record = useCallback(
    (run: RunInFlight, outcome: string, durationMs: number) => {
      setHistory(addHistory(sub, databaseId, { sql: run.text.text, branchId: run.branchId, database: run.database, role: run.role, at: new Date(run.startedAt).toISOString(), outcome, durationMs }));
    },
    [sub, databaseId],
  );

  async function execute(text: RunText, confirm: boolean) {
    controller.current?.abort();
    const abort = new AbortController();
    controller.current = abort;
    const token = ++tokens.current;
    const id = crypto.randomUUID();
    const forBranch = branchId;
    const cur: RunInFlight = { id, token, text, startedAt: Date.now(), branchId: forBranch, database: dbParam, role: roleParam, finished: false };
    current.current = cur;
    lastText.current = text;
    setFailure(null);
    setOwnerUnknown(false);
    setHeld(null);
    setErrorAt(null);
    editor.current?.markError(null);
    let acc: RunState = initialRunState;
    setRun(acc);
    try {
      const events = await startRun(databaseId, forBranch, { run_id: id, database: cur.database, role: cur.role, sql: text.text, timeout_ms: timeoutMs, confirm }, abort.signal);
      for await (const event of events) {
        if (tokens.current !== token) return;
        acc = runReducer(acc, event);
        setRun(acc);
        if (event.t === "error" && event.position !== null) {
          const at = errorOffset(text.text, event.position, text.from);
          setErrorAt(at);
          editor.current?.markError(at);
        }
      }
      if (tokens.current !== token) return;
      cur.finished = true;
      if (acc.done) {
        record(cur, acc.done.outcome, acc.done.duration_ms);
        if (changesSchema(acc.statements.flatMap((s) => (s.tag ? [s.tag] : [])))) void schemaQuery.refetch();
      } else {
        setRun({ ...acc, running: false });
        setFailure("The connection closed before the run finished.");
      }
    } catch (err) {
      if (tokens.current !== token || abort.signal.aborted) return;
      cur.finished = true;
      setRun(null);
      if (err instanceof ApiError && err.code === "sql_confirm_required" && Array.isArray(err.extra.statements)) {
        setHeld(err.extra.statements as HeldStatement[]);
      } else if (err instanceof ApiError && err.code === "owner_password_unknown") {
        setOwnerUnknown(true);
      } else {
        setFailure(errorText(err));
      }
    }
  }

  const start = () => {
    if (run?.running) return;
    const text = editor.current?.runText();
    if (text) void execute(text, false);
  };

  async function cancel() {
    const cur = current.current;
    if (!cur || cur.finished) return;
    try {
      await cancelSqlRun(databaseId, cur.branchId, cur.id);
    } catch {
      // the abort below ends the run on the server too
    }
    // The stream may have ended while the cancel was on its way (its `done` is recorded then).
    if (tokens.current !== cur.token || cur.finished) return;
    tokens.current++;
    controller.current?.abort();
    const duration = Date.now() - cur.startedAt;
    setRun((r) => (r ? { ...r, running: false, done: { t: "done", outcome: "cancelled", duration_ms: duration } } : r));
    record(cur, "cancelled", duration);
  }

  function applyHints(hints: { database?: string | null; role?: string | null }) {
    const notes: string[] = [];
    if (hints.database) {
      if (pgDatabases.includes(hints.database)) onPgDatabase(hints.database);
      else notes.push(`The saved database "${hints.database}" isn't on this branch; the selected one is kept.`);
    }
    if (hints.role) {
      if (allowed.roles.includes(hints.role)) onRole(hints.role);
      else notes.push(`The saved role "${hints.role}" isn't available here; running as ${role === "" ? "the owner" : role}.`);
    }
    setLoadNote(notes.length > 0 ? notes.join(" ") : null);
  }

  const loadSql = (sql: string) => {
    editor.current?.setDoc(sql);
    setDoc(sql);
  };

  const basicNote = !pglsReady;

  return (
    <div className="flex min-h-0 flex-1 max-[40rem]:flex-col">
      <aside className="w-64 shrink-0 overflow-y-auto border-r border-line p-3 max-[40rem]:w-auto max-[40rem]:border-r-0 max-[40rem]:border-b">
        <SavedQueriesPanel
          databaseId={databaseId}
          sql={doc}
          hints={{ pg_database: dbParam, role: roleParam }}
          active={active}
          onActiveChange={setActive}
          onLoad={(q: SavedQuery) => {
            loadSql(q.sql);
            applyHints({ database: q.pg_database ?? null, role: q.role ?? null });
          }}
          history={history}
          onOpenHistory={(h) => {
            loadSql(h.sql);
            applyHints({ database: h.database, role: h.role });
          }}
          onClearHistory={() => {
            clearHistory(sub, databaseId);
            setHistory([]);
          }}
        />
      </aside>
      <div className="min-w-0 flex-1 space-y-3 overflow-y-auto p-3">
      <SqlToolbar
        timeoutMs={timeoutMs}
        onTimeout={(ms) => {
          setTimeoutMs(ms);
          saveTimeout(ms);
        }}
        running={run?.running === true}
        canRun={rolesReady && !noRole}
        onRun={start}
        onCancel={() => void cancel()}
        onRefreshSchema={() => void schemaQuery.refetch()}
      />

      {noRole ? (
        rolesReady && (
          <p className="border p-2 text-sm">You can't act as any Postgres role here. Ask a project Admin to grant you one.</p>
        )
      ) : role === "" ? (
        <p className="border border-destructive/40 p-2 text-sm">Running as owner: row-level security is bypassed.</p>
      ) : !allowed.owner ? (
        <p className="border p-2 text-sm">
          Running as <code>{role}</code>: Postgres applies this role&apos;s privileges and row-level security.
        </p>
      ) : (
        <p className="border p-2 text-sm">
          Running as <code>{role}</code> (SET ROLE). This previews that role&apos;s view; it isn&apos;t a security boundary, since a script can RESET ROLE.
        </p>
      )}
      {branchProtected && <p className="border p-2 text-sm">Protected branch: every change asks for confirmation.</p>}
      {(ownerUnknown || (schemaQuery.error instanceof ApiError && schemaQuery.error.code === "owner_password_unknown")) && (
        <OwnerPasswordNotice databaseId={databaseId} branchId={branchId} onRetry={() => (ownerUnknown ? start() : void schemaQuery.refetch())} />
      )}

      <div className="flex items-center justify-between text-xs text-muted-foreground">
        <span>{loadNote}</span>
        {basicNote && <span>Basic completion</span>}
      </div>
      <CodeEditor ref={editor} initialDoc={initialDoc.current} extensions={extensions} onChange={setDoc} onRun={start} />
      {failure && (
        <p role="alert" className="text-sm text-destructive">
          {failure}
        </p>
      )}
      {run && <SqlResults state={run} onGoToError={errorAt !== null ? () => editor.current?.goTo(errorAt) : null} />}
      </div>

      <SqlConfirmDialog
        statements={held}
        onCancel={() => setHeld(null)}
        onConfirm={() => {
          const text = lastText.current;
          setHeld(null);
          if (text) void execute(text, true);
        }}
      />
    </div>
  );
}
