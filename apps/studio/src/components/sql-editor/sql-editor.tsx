import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  ApiError,
  cancelSqlRun,
  useGetMe,
  useGetSqlSchema,
  useListBranchDatabases,
  useListBranches,
  useListRoles,
  type SavedQuery,
} from "@chelabase/management-client";
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

export interface SqlEditorProps {
  databaseId: string;
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

export function SqlEditor({ databaseId, createClient }: SqlEditorProps) {
  const me = useGetMe();
  if (me.isPending) return <p className="py-3 text-sm text-muted-foreground">Loading the editor…</p>;
  return <SqlWorkspace databaseId={databaseId} sub={me.data?.user.sub ?? FALLBACK_SUB} createClient={createClient ?? defaultCreateClient} />;
}


function SqlWorkspace({ databaseId, sub, createClient }: { databaseId: string; sub: string; createClient: () => Promise<PglsClient> }) {
  const branches = useListBranches(databaseId);
  const eligible = (branches.data ?? []).filter((b) => b.kind === "normal" && b.status === "ready");
  const [pickedBranch, setPickedBranch] = useState<string | null>(null);
  const branch = eligible.find((b) => b.id === pickedBranch) ?? eligible.find((b) => b.parent_branch_id === null) ?? eligible[0] ?? null;
  const branchId = branch?.id ?? null;

  const dbs = useListBranchDatabases(databaseId, branchId ?? "", { query: { enabled: branchId !== null } });
  const dbList = (dbs.data ?? []).filter((d) => d.origin !== "system").sort((a, b) => Number(b.main) - Number(a.main));
  const [pickedDb, setPickedDb] = useState<string | null>(null);
  const db = dbList.find((d) => d.name === pickedDb) ?? dbList[0] ?? null;
  const dbParam = db && !db.main ? db.name : null;

  const rolesQuery = useListRoles(databaseId, branchId ?? "", { query: { enabled: branchId !== null } });
  const roleOptionsFor = (d: typeof db) => {
    const own = (rolesQuery.data ?? []).filter((r) => r.origin !== "owner" && r.name !== "anon" && r.name !== "authenticated").map((r) => r.name);
    return d?.data_api === "on" ? ["anon", "authenticated", ...own] : own;
  };
  const roleOptions = roleOptionsFor(db);
  const [pickedRole, setPickedRole] = useState("");
  const role = roleOptions.includes(pickedRole) ? pickedRole : "";

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

  const schemaKey = `${branchId}|${dbParam}`;
  const schemaQuery = useGetSqlSchema(databaseId, branchId ?? "", dbParam ? { database: dbParam } : {}, {
    query: { enabled: branchId !== null && db !== null, staleTime: Infinity, retry: false },
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
    if (branchId === null) return;
    controller.current?.abort();
    const abort = new AbortController();
    controller.current = abort;
    const token = ++tokens.current;
    const id = crypto.randomUUID();
    const forBranch = branchId;
    const cur: RunInFlight = { id, token, text, startedAt: Date.now(), branchId: forBranch, database: dbParam, role: role === "" ? null : role, finished: false };
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
    let target = db;
    if (hints.database) {
      const found = dbList.find((d) => d.name === hints.database);
      if (found) {
        setPickedDb(found.name);
        target = found;
      } else notes.push(`The saved database "${hints.database}" isn't on this branch; the selected one is kept.`);
    }
    if (hints.role) {
      if (roleOptionsFor(target).includes(hints.role)) setPickedRole(hints.role);
      else notes.push(`The saved role "${hints.role}" isn't available here; running as ${role === "" ? "the owner" : role}.`);
    }
    setLoadNote(notes.length > 0 ? notes.join(" ") : null);
  }

  const loadSql = (sql: string) => {
    editor.current?.setDoc(sql);
    setDoc(sql);
  };

  const gone = !branches.isPending && branch === null;
  const fatal = branches.error ?? dbs.error ?? rolesQuery.error;
  const basicNote = !pglsReady;

  return (
    <div className="space-y-3 py-3">
      <SqlToolbar
        branches={eligible.map((b) => ({ id: b.id, name: b.name }))}
        branchId={branchId}
        onBranch={(id) => {
          setPickedBranch(id);
          setPickedDb(null);
          setPickedRole("");
        }}
        databases={dbList.map((d) => d.name)}
        database={db?.name ?? null}
        onDatabase={setPickedDb}
        roles={roleOptions}
        role={role}
        onRole={setPickedRole}
        timeoutMs={timeoutMs}
        onTimeout={(ms) => {
          setTimeoutMs(ms);
          saveTimeout(ms);
        }}
        running={run?.running === true}
        canRun={branchId !== null && db !== null}
        onRun={start}
        onCancel={() => void cancel()}
        onRefreshSchema={() => void schemaQuery.refetch()}
      />

      {role === "" ? (
        <p className="rounded-lg border border-destructive/40 p-2 text-sm">Running as owner: row-level security is bypassed.</p>
      ) : (
        <p className="rounded-lg border p-2 text-sm">
          Running as <code>{role}</code> (SET ROLE). This previews that role&apos;s view; it isn&apos;t a security boundary, since a script can RESET ROLE.
        </p>
      )}
      {branch?.protected && <p className="rounded-lg border p-2 text-sm">Protected branch: every change asks for confirmation.</p>}
      {fatal && (
        <p role="alert" className="text-sm text-destructive">
          {errorText(fatal)}
        </p>
      )}
      {gone && !fatal && <p className="text-sm text-muted-foreground">This database has no ready branch yet.</p>}
      {(ownerUnknown || (schemaQuery.error instanceof ApiError && schemaQuery.error.code === "owner_password_unknown")) && branchId && (
        <OwnerPasswordNotice databaseId={databaseId} branchId={branchId} onRetry={() => (ownerUnknown ? start() : void schemaQuery.refetch())} />
      )}

      <div className="grid gap-4 md:grid-cols-[16rem_1fr]">
        <SavedQueriesPanel
          databaseId={databaseId}
          sql={doc}
          hints={{ pg_database: dbParam, role: role === "" ? null : role }}
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
        <div className="min-w-0 space-y-3">
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
