import { useState } from "react";
import { useQueryClient } from "@tanstack/react-query";
import {
  ApiError,
  createSavedQuery,
  deleteSavedQuery,
  getGetSavedQueryQueryKey,
  getListSavedQueriesQueryKey,
  getSavedQuery,
  updateSavedQuery,
  useListSavedQueries,
  type SavedQuery,
  type SavedQuerySummary,
} from "@cheladb/management-client";
import { Button } from "@/components/ui/button";
import { Modal } from "@/components/ui/dialog";
import { errorText } from "@/lib/errors";
import type { HistoryEntry } from "./run-history";

/** The saved query open in the editor; `sql` is the text as last saved (for the unsaved-changes check). */
export type ActiveQuery = { id: string; name: string; version: number; sql: string };

export interface SavedQueriesPanelProps {
  databaseId: string;
  /** The editor buffer. */
  sql: string;
  /** What a save records as hints: the Postgres database and role the editor is set to. */
  hints: { pg_database: string | null; role: string | null };
  active: ActiveQuery | null;
  onActiveChange: (active: ActiveQuery | null) => void;
  /** A saved query was opened (or its saved version loaded): the editor takes its text and hints. */
  onLoad: (query: SavedQuery) => void;
  history: HistoryEntry[];
  onOpenHistory: (entry: HistoryEntry) => void;
  onClearHistory: () => void;
}

export const PRIVACY_NOTE = "Saved queries are private to you but stored on the server. Don't save secrets.";
const field = "h-8 w-full rounded-lg border border-input bg-background px-2 text-sm";

type Dialog =
  | { kind: "save-as"; sql: string }
  | { kind: "rename"; query: SavedQuerySummary }
  | { kind: "delete"; query: SavedQuerySummary }
  | { kind: "conflict"; current: SavedQuery; sql: string };

const toActive = (q: SavedQuery): ActiveQuery => ({ id: q.id, name: q.name, version: q.version, sql: q.sql });

function saveErrorText(err: unknown): string {
  if (err instanceof ApiError) {
    if (err.code === "saved_query_name_taken") return "You already have a saved query with that name.";
    if (err.code === "saved_query_cap") return "You've reached the limit of 500 saved queries for this database; delete some first.";
  }
  return errorText(err);
}

export function SavedQueriesPanel({ databaseId, sql, hints, active, onActiveChange, onLoad, history, onOpenHistory, onClearHistory }: SavedQueriesPanelProps) {
  const queryClient = useQueryClient();
  const [tab, setTab] = useState<"saved" | "history">("saved");
  const [search, setSearch] = useState("");
  const [dialog, setDialog] = useState<Dialog | null>(null);
  const [name, setName] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const list = useListSavedQueries(databaseId);
  const shown = (list.data ?? []).filter((q) => q.name.toLowerCase().includes(search.trim().toLowerCase()));
  const dirty = active !== null && active.sql !== sql;

  const refresh = () => queryClient.invalidateQueries({ queryKey: getListSavedQueriesQueryKey(databaseId) });
  const open = (d: Dialog, initialName = "") => {
    setDialog(d);
    setName(initialName);
    setError(null);
  };
  const close = () => setDialog(null);

  async function run(work: () => Promise<void>) {
    setBusy(true);
    setError(null);
    try {
      await work();
    } catch (err) {
      setError(saveErrorText(err));
    } finally {
      setBusy(false);
    }
  }

  async function openQuery(q: SavedQuerySummary) {
    setError(null);
    try {
      const full = await getSavedQuery(databaseId, q.id);
      onActiveChange(toActive(full));
      onLoad(full);
    } catch (err) {
      setError(saveErrorText(err));
    }
  }

  const saved = (q: SavedQuery) => {
    onActiveChange(toActive(q));
    void refresh();
    void queryClient.invalidateQueries({ queryKey: getGetSavedQueryQueryKey(databaseId, q.id) });
  };

  /** PATCH of the buffer; a version conflict opens the conflict dialog. */
  async function save(version: number) {
    if (!active) return;
    setBusy(true);
    setError(null);
    try {
      saved(await updateSavedQuery(databaseId, active.id, { sql, pg_database: hints.pg_database, role: hints.role, version }));
      close();
    } catch (err) {
      if (err instanceof ApiError && err.code === "saved_query_conflict" && err.extra.current && typeof err.extra.current === "object") {
        open({ kind: "conflict", current: err.extra.current as SavedQuery, sql });
      } else {
        setError(saveErrorText(err));
      }
    } finally {
      setBusy(false);
    }
  }

  const submitName = () =>
    run(async () => {
      const trimmed = name.trim();
      if (trimmed === "" || !dialog) return;
      if (dialog.kind === "save-as") {
        const q = await createSavedQuery(databaseId, { name: trimmed, sql: dialog.sql, pg_database: hints.pg_database, role: hints.role });
        saved(q);
        close();
      } else if (dialog.kind === "rename") {
        const q = await updateSavedQuery(databaseId, dialog.query.id, { name: trimmed, version: dialog.query.version });
        void refresh();
        if (active?.id === q.id) onActiveChange({ ...active, name: q.name, version: q.version });
        close();
      }
    });

  const confirmDelete = () =>
    run(async () => {
      if (dialog?.kind !== "delete") return;
      await deleteSavedQuery(databaseId, dialog.query.id);
      if (active?.id === dialog.query.id) onActiveChange(null);
      void refresh();
      close();
    });

  return (
    <aside className="space-y-2 text-sm">
      <div role="tablist" className="flex gap-1 border-b">
        {(["saved", "history"] as const).map((t) => (
          <button
            key={t}
            type="button"
            role="tab"
            aria-selected={tab === t}
            onClick={() => setTab(t)}
            className={`-mb-px border-b-2 px-2 py-1 font-medium ${tab === t ? "border-primary" : "border-transparent text-muted-foreground"}`}
          >
            {t === "saved" ? "Saved" : "History"}
          </button>
        ))}
      </div>

      {tab === "saved" ? (
        <div className="space-y-2">
          <div className="flex flex-wrap items-center gap-2">
            <Button size="sm" variant="outline" disabled={!active || busy} onClick={() => void save(active!.version)}>
              Save
            </Button>
            <Button size="sm" variant="outline" disabled={sql.trim() === ""} onClick={() => open({ kind: "save-as", sql })}>
              Save as
            </Button>
          </div>
          {active && (
            <p className="text-xs text-muted-foreground">
              Open: {active.name}
              {dirty ? " (unsaved changes)" : ""}
            </p>
          )}
          <input aria-label="Search saved queries" placeholder="Search" value={search} onChange={(e) => setSearch(e.target.value)} className={field} />
          {error && dialog === null && (
            <p role="alert" className="text-destructive">
              {error}
            </p>
          )}
          {list.error && (
            <p role="alert" className="text-destructive">
              {errorText(list.error)}
            </p>
          )}
          {list.data !== undefined && shown.length === 0 && <p className="text-muted-foreground">{list.data.length === 0 ? "No saved queries yet." : "No match."}</p>}
          <ul className="space-y-1">
            {shown.map((q) => (
              <li key={q.id} className={`rounded border p-1.5 ${active?.id === q.id ? "bg-muted" : ""}`}>
                <button type="button" aria-label={`Open ${q.name}`} onClick={() => void openQuery(q)} className="block w-full text-left">
                  <span className="block truncate font-medium">{q.name}</span>
                  <span className="block text-xs text-muted-foreground">{new Date(q.updated_at).toLocaleString()}</span>
                </button>
                <div className="flex gap-1 pt-1">
                  <Button size="xs" variant="ghost" aria-label={`Rename ${q.name}`} onClick={() => open({ kind: "rename", query: q }, q.name)}>
                    Rename
                  </Button>
                  <Button size="xs" variant="ghost" aria-label={`Delete ${q.name}`} onClick={() => open({ kind: "delete", query: q })}>
                    Delete
                  </Button>
                </div>
              </li>
            ))}
          </ul>
        </div>
      ) : (
        <div className="space-y-2">
          <Button size="sm" variant="outline" disabled={history.length === 0} onClick={onClearHistory}>
            Clear history
          </Button>
          {history.length === 0 && <p className="text-muted-foreground">No runs yet in this browser.</p>}
          <ul className="space-y-1">
            {history.map((h, i) => (
              <li key={`${h.at}-${i}`} className="space-y-1 rounded border p-1.5">
                <pre className="max-h-16 overflow-hidden font-mono text-xs whitespace-pre-wrap">{h.sql}</pre>
                <p className="text-xs text-muted-foreground">
                  {new Date(h.at).toLocaleString()} · {h.database ?? "main"} · {h.role ?? "owner"} · {h.outcome} · {h.durationMs} ms
                </p>
                <div className="flex gap-1">
                  <Button size="xs" variant="ghost" onClick={() => onOpenHistory(h)}>
                    Open
                  </Button>
                  <Button size="xs" variant="ghost" onClick={() => open({ kind: "save-as", sql: h.sql })}>
                    Save as query
                  </Button>
                </div>
              </li>
            ))}
          </ul>
        </div>
      )}

      <Modal open={dialog?.kind === "save-as" || dialog?.kind === "rename"} onOpenChange={(o) => !o && close()} title={dialog?.kind === "rename" ? "Rename query" : "Save query"}>
        <form
          className="space-y-3"
          onSubmit={(e) => {
            e.preventDefault();
            void submitName();
          }}
        >
          <label className="flex flex-col gap-1 text-sm">
            <span>Name</span>
            <input value={name} onChange={(e) => setName(e.target.value)} maxLength={120} className={field} autoFocus />
          </label>
          {dialog?.kind === "save-as" && <p className="text-xs text-muted-foreground">{PRIVACY_NOTE}</p>}
          {error && (
            <p role="alert" className="text-sm text-destructive">
              {error}
            </p>
          )}
          <div className="flex justify-end gap-2">
            <Button type="button" variant="outline" onClick={close}>
              Cancel
            </Button>
            <Button type="submit" disabled={busy || name.trim() === ""}>
              Save
            </Button>
          </div>
        </form>
      </Modal>

      <Modal open={dialog?.kind === "delete"} onOpenChange={(o) => !o && close()} title="Delete saved query">
        <p className="text-sm">Delete &ldquo;{dialog?.kind === "delete" ? dialog.query.name : ""}&rdquo;? This can&apos;t be undone.</p>
        {error && (
          <p role="alert" className="text-sm text-destructive">
            {error}
          </p>
        )}
        <div className="flex justify-end gap-2">
          <Button variant="outline" onClick={close}>
            Cancel
          </Button>
          <Button variant="destructive" disabled={busy} onClick={() => void confirmDelete()}>
            Delete
          </Button>
        </div>
      </Modal>

      <Modal open={dialog?.kind === "conflict"} onOpenChange={(o) => !o && close()} title="Saved query changed">
        <p className="text-sm">This query was saved again (version {dialog?.kind === "conflict" ? dialog.current.version : ""}) since you opened it.</p>
        <div className="flex justify-end gap-2">
          <Button
            variant="outline"
            onClick={() => {
              if (dialog?.kind !== "conflict") return;
              onActiveChange(toActive(dialog.current));
              onLoad(dialog.current);
              close();
            }}
          >
            Load saved version
          </Button>
          <Button
            disabled={busy}
            onClick={() => {
              if (dialog?.kind === "conflict") void save(dialog.current.version);
            }}
          >
            Overwrite
          </Button>
        </div>
      </Modal>
    </aside>
  );
}
