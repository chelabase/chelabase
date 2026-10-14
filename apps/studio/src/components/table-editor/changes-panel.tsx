import { useContext, useEffect, useMemo, useState } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { X } from "lucide-react";
import {
  ApiError,
  applySchemaChanges,
  getGetConsoleSchemaQueryKey,
  getTable,
  getGetTableQueryKey,
  getListObjectsQueryKey,
  getListTypesQueryKey,
  type Dependents,
} from "@chelabase/management-client";
import { Button } from "@/components/ui/button";
import { Modal } from "@/components/ui/dialog";
import { changesetKey, opsOf, tableFingerprint, type Edit } from "@/lib/table-editor/changeset";
import { lockText, mapChangeError, skipText, type ChangeFailure, type EditStatus } from "@/lib/table-editor/dry-run";
import { TableEditorStoreContext, useTableEditor } from "@/lib/table-editor/store";
import { cn } from "@/lib/utils";
import { changesWord, CheckField, Dot, ErrorText, passedText, SqlStatements } from "./panel-bits";
import { COUNT_QUERY, ROWS_QUERY } from "./rows-view";
import type { TableTarget } from "./target";
import { useDryRun } from "./use-dry-run";
import { useTableStructure } from "./use-structure";

export interface ChangesPanelProps {
  target: TableTarget;
  /** Why structure can't be changed here, or null. */
  reason: string | null;
  totalBytes: number | null;
  /**
   * After a successful Apply that created, renamed, moved or dropped the table, once the object list is
   * read again: where the editor goes (`table` undefined after a drop).
   */
  onApplied?: (to: { schema: string; table: string | undefined }) => void;
}

/** The dependents an answer named for these operations; `required` once Postgres refused without cascade. */
interface KnownDependents {
  ops: string;
  dependents: Dependents;
  required: boolean;
}

/**
 * The pending changes of the open table: numbered edits with their dry-run status, the SQL, warnings,
 * dependents with the cascade checkbox, Discard and Apply. Apply runs everything in one transaction.
 *
 * Apply posts only when a fresh re-read shows the table unchanged, and then exactly the request of the
 * latest settled dry run (operations, role, database, cascade) with `dry_run: false`; an acknowledgement
 * counts only for the dry run it was given on. While Apply runs, nothing is staged for this table.
 */
export function ChangesPanel({ target, reason, totalBytes, onApplied }: ChangesPanelProps) {
  const { databaseId, branchId, database, schema, table, key } = target;
  const queryClient = useQueryClient();
  const store = useContext(TableEditorStoreContext)!;
  const { base, edits, changeset, projected } = useTableStructure(target);
  // The enums view and the new-table form have no table to read: no fingerprint, no size, no re-read.
  const draft = target.draft ?? null;
  const subject = draft === "types" ? `types in ${schema}` : `${schema}.${table}`;
  const removeEdit = useTableEditor((s) => s.removeEdit);
  const discard = useTableEditor((s) => s.discard);
  const rebase = useTableEditor((s) => s.rebase);
  const openPanel = useTableEditor((s) => s.openPanel);
  const applying = useTableEditor((s) => !!s.applying[key]);
  const dialogs = useTableEditor((s) => s.dialogs);
  const applyFailure = useTableEditor((s) => s.applyFailures[key] ?? null);

  const ops = useMemo(() => JSON.stringify(opsOf(edits)), [edits]);
  const fingerprint = useMemo(() => (base ? tableFingerprint(base) : null), [base]);
  // While Apply runs the table is read again after the commit, before the applied edits go: not a stale table.
  const stale = !applying && !!fingerprint && !!changeset && changeset.base !== null && changeset.base !== fingerprint;

  const [cascadeHash, setCascadeHash] = useState<string | null>(null);
  const [ackFor, setAckFor] = useState<string | null>(null);
  const [confirmDiscard, setConfirmDiscard] = useState(false);
  const [highlight, setHighlight] = useState<number | null>(null);
  const [known, setKnown] = useState<KnownDependents | null>(null);

  const deps = known?.ops === ops ? known : null;
  const dependents = deps?.dependents ?? null;
  // Confirmed for exactly the dependents last named (an empty list included: drops that reach each other).
  const cascade = cascadeHash !== null && dependents?.hash === cascadeHash;

  const dry = useDryRun({
    databaseId,
    branchId,
    database,
    role: target.role,
    edits,
    table: subject,
    totalBytes,
    enabled: (!!base || draft !== null) && edits.length > 0 && reason === null,
    cascade: cascade ? cascadeHash : null,
    revision: changeset?.base ?? null,
    paused: dialogs > 0,
  });
  // The failure of an Apply sent from the dry run of the current inputs (any change of them retires it).
  const ownFailure = applyFailure && applyFailure.dryKey === dry.key ? applyFailure.failure : null;

  // Remember what each answer said about dependents; a cascaded answer without a list keeps the last one.
  const latestDeps = dry.failure?.dependents ?? dry.view?.dependents ?? null;
  const latestRequired = dry.failure?.code === "has_dependents" || dry.failure?.code === "dependents_changed";
  useEffect(() => {
    if (!latestDeps || dry.running) return;
    setKnown((k) => mergeKnown(k, ops, latestDeps, latestRequired));
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [dry.settledKey, dry.view, dry.failure]);
  const failureDeps = applyFailure?.failure.dependents ?? null;
  useEffect(() => {
    if (failureDeps) setKnown((k) => mergeKnown(k, ops, failureDeps, true));
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [applyFailure]);

  const n = edits.length;
  const view = dry.view;
  const failure = ownFailure ?? (dry.failure && dry.failure.code !== "has_dependents" ? dry.failure : null);
  const needsCascade = !!deps && (deps.required || (deps.dependents.objects?.length ?? 0) > 0);
  const acked = !dry.running && dry.settledKey !== null && ackFor === dry.settledKey;
  const blockedByApply = !!ownFailure && !ownFailure.retryable && ownFailure.code !== "dependents_changed";
  const canApply =
    reason === null &&
    !applying &&
    !dry.running &&
    !stale &&
    view !== null &&
    dry.request !== null &&
    dry.failure === null &&
    view.apply !== "blocked" &&
    (view.apply !== "needs_ack" || acked) &&
    (!needsCascade || cascade) &&
    !blockedByApply;

  async function apply() {
    const request = dry.request;
    const dryKey = dry.key;
    if (!request || !changeset || (!projected && draft === null)) return;
    const s = store.getState();
    const sent = request.edits;
    const count = sent.length;
    const changed = draft ? [] : projected!.columns.filter((c) => c.mark === "added" || c.mark === "changed").map((c) => c.name);
    const created = draft === "new_table" && projected ? { schema: projected.schema, name: projected.name } : null;
    const name = draft === "types" ? subject : projected ? `${projected.schema}.${projected.name}` : subject;
    const params = database ? { database } : undefined;
    // Where the table is once these changes apply: renamed, moved or dropped tables leave the open URL.
    const dropped = draft === null && !!projected?.dropped;
    const moved = draft === null && !!projected && !dropped && (projected.schema !== schema || projected.name !== table);
    const to = created ?? (moved ? { schema: projected!.schema, name: projected!.name } : null);
    const tableKey = getGetTableQueryKey(databaseId, branchId, schema, table, params);
    const fail = (err: unknown) => {
      const f: ChangeFailure =
        err instanceof ApiError
          ? mapChangeError(sent, err)
          : { edit: null, status: { kind: "error", message: err instanceof Error ? err.message : "Something went wrong", detail: null, hint: null }, code: "internal", retryable: true, dependents: null };
      store.getState().setApplying(key, false);
      store.getState().setApplyFailure(key, { dryKey, failure: f });
    };
    s.setApplying(key, true);
    s.setApplyFailure(key, null);
    // Re-read the table: a change made elsewhere since the edits were made stops Apply (spec §2).
    if (draft === null) {
    try {
      const fresh = await queryClient.fetchQuery({
        queryKey: tableKey,
        queryFn: ({ signal }) => getTable(databaseId, branchId, schema, table, params, { signal }),
        staleTime: 0,
      });
      const baseNow = store.getState().changesets[key]?.base ?? null;
      if (baseNow !== null && tableFingerprint(fresh) !== baseNow) {
        store.getState().setApplying(key, false);
        return;
      }
    } catch (err) {
      fail(err);
      return;
    }
    }
    try {
      await applySchemaChanges(databaseId, branchId, { ...request.body, dry_run: false });
    } catch (err) {
      fail(err);
      return;
    }
    // Read the table again before the applied edits go, so the strip never shows the old structure
    // (a renamed, moved or dropped table is gone under this name).
    if (draft === null && !moved && !dropped) {
      try {
        await queryClient.fetchQuery({ queryKey: tableKey, queryFn: ({ signal }) => getTable(databaseId, branchId, schema, table, params, { signal }), staleTime: 0 });
      } catch {
        // The change is applied; the table view shows the read error itself.
      }
    }
    // The outcome line belongs to the table where it now is (a created, renamed or moved table's own key).
    const appliedKey = to ? changesetKey({ databaseId, branchId, pgDatabase: target.pgDatabase, schema: to.schema, table: to.name }) : key;
    store.getState().finishApply(
      key,
      sent.map((e) => e.id),
      { key: appliedKey, text: `Applied ${changesWord(count)} to ${name}`, columns: changed },
    );
    if (store.getState().panel.kind === "changes") store.getState().openPanel("closed");
    const objectsRead = queryClient.invalidateQueries({ queryKey: getListObjectsQueryKey(databaseId, branchId) });
    void queryClient.invalidateQueries({ queryKey: getListTypesQueryKey(databaseId, branchId) });
    void queryClient.invalidateQueries({ queryKey: getGetConsoleSchemaQueryKey(databaseId, branchId) });
    // A renamed, moved or dropped table is gone under its old name (M6): its rows and counts are
    // marked stale and cancelled, never read again under that name; the rest are read again.
    const gone = moved || dropped ? `${schema}.${table}` : null;
    const isGone = (q: { queryKey: readonly unknown[] }) => gone !== null && q.queryKey.includes(gone);
    for (const queryKey of [[ROWS_QUERY, databaseId, branchId], [COUNT_QUERY, databaseId, branchId]]) {
      if (gone !== null) {
        void queryClient.cancelQueries({ queryKey, predicate: isGone });
        void queryClient.invalidateQueries({ queryKey, predicate: isGone, refetchType: "none" });
      }
      void queryClient.invalidateQueries({ queryKey, predicate: (q) => !isGone(q) });
    }
    if (!to && !dropped) return;
    // Leave the old URL only once the object list knows the table's new place (no "Table not found" flash).
    await objectsRead.catch(() => undefined);
    onApplied?.(to ? { schema: to.schema, table: to.name } : { schema, table: undefined });
  }

  /** Reopens an edit's column form, or highlights its SQL. */
  function open(e: Edit, i: number) {
    const op = e.ops[0];
    const column = op && "add_column" in op ? op.add_column.column.name : op && "rename_column" in op ? op.rename_column.to : op && "alter_column" in op ? op.alter_column.column : null;
    if (draft === null && column !== null && reason === null && projected?.columns.some((c) => c.name === column && c.mark !== "dropped")) {
      openPanel("column", { column });
      return;
    }
    setHighlight((h) => (h === i ? null : i));
  }

  function statusOf(i: number): EditStatus | null {
    if (failure && failure.edit === i) return failure.status;
    const s = view?.perEdit[i];
    return s && view?.statements.length && !dry.running ? s : null;
  }

  const general = failure && failure.edit === null ? failure : null;
  const locks = [...new Set((view?.statements ?? []).map((s) => lockText(s.lock)))];
  const passed = !!view && !dry.running && view.apply === "ready" && !needsCascade && !failure;

  return (
    <div className="flex min-h-0 flex-1 flex-col gap-3">
      {stale && (
        <div className="space-y-2 border border-coral/60 p-3 text-[13px]">
          <p className="text-chalk">This table changed since you started</p>
          <p className="text-chalk-dim">Someone changed {schema}.{table} after these edits were made. Review the SQL against the table as it is now.</p>
          <Button size="sm" variant="outline" onClick={() => base && rebase(key, base)}>
            Review again
          </Button>
        </div>
      )}

      <ol className="space-y-1.5">
        {edits.map((e, i) => {
          const status = statusOf(i);
          return (
            <li key={e.id} className="group/edit border border-line px-2 py-1.5">
              <div className="flex items-start gap-2">
                <span className="mt-0.5 w-5 shrink-0 text-right font-mono text-xs text-chalk-dim">{i + 1}</span>
                <button
                  type="button"
                  onClick={() => open(e, i)}
                  className={cn(
                    "min-w-0 flex-1 text-left font-mono text-[13px] break-words text-chalk outline-none hover:underline focus-visible:outline-2 focus-visible:outline-sand",
                    highlight === i && "text-sand",
                  )}
                >
                  {e.label}
                </button>
                <button
                  type="button"
                  aria-label={`Remove ${e.label}`}
                  disabled={applying}
                  onClick={() => {
                    setHighlight(null);
                    removeEdit(key, e.id, base ?? null);
                  }}
                  className="flex size-6 shrink-0 items-center justify-center text-chalk-dim outline-none hover:text-chalk focus-visible:outline-2 focus-visible:outline-sand"
                >
                  <X aria-hidden="true" size={14} strokeWidth={1.5} />
                </button>
              </div>
              {status?.kind === "error" && (
                <div className="mt-1 pl-7">
                  <ErrorText status={status} />
                </div>
              )}
              {status?.kind === "rendered_only" && <p className="mt-1 pl-7 text-[13px] text-chalk-dim">{skipText(status.reason)}</p>}
            </li>
          );
        })}
      </ol>

      <section aria-label="SQL" className="space-y-1">
        <h3 className="text-xs text-chalk-dim">SQL</h3>
        <SqlStatements statements={view?.statements ?? []} pending={dry.running} highlight={highlight} />
        {dry.running && view && <p className="text-xs text-chalk-dim">Running the dry run again…</p>}
      </section>

      {view && !dry.running && (view.warnings.length > 0 || (!passed && locks.length > 0)) && (
        <div className="space-y-1 text-[13px]">
          {view.warnings.map((w) => (
            <p key={w} className="flex gap-2 text-chalk">
              <Dot tone="warn" />
              {w}
            </p>
          ))}
          {/* A passed dry run names its lock in its one line; the lock lines stay for a run that waits or fails. */}
          {!passed && locks.map((l) => (
            <p key={l} className="flex gap-2 text-chalk-dim">
              <Dot tone="wait" />
              {l}
            </p>
          ))}
        </div>
      )}
      {/* Mounted before any outcome: screen readers announce what the dry run or Apply says here. */}
      <div role="status" className="space-y-3">
        {passed && (
          <p className="flex gap-2 text-[13px] text-seafoam">
            <Dot tone="ok" />
            {passedText(view.statements)}
          </p>
        )}
        {failure && failure.edit !== null && !dry.running && <p className="sr-only">{`Change ${failure.edit + 1} fails; its error shows under it`}</p>}
        {general && <GeneralFailure failure={general} />}
      </div>

      {needsCascade && dependents && (
        <div className="space-y-1.5 border border-coral/50 p-3">
          <h3 className="text-[13px] text-chalk">Dependents</h3>
          {(dependents.objects?.length ?? 0) === 0 ? (
            <p className="text-xs text-chalk-dim">Drops in this change depend on each other</p>
          ) : (
            <>
              <p className="text-xs text-chalk-dim">Dropping this also needs these objects gone:</p>
              <ul aria-label="Dependents" className="space-y-0.5 font-mono text-xs text-chalk">
                {(dependents.objects ?? []).map((o) => (
                  <li key={`${o.kind}:${o.schema ?? ""}:${o.name}:${o.description}`}>{o.description}</li>
                ))}
              </ul>
            </>
          )}
          <CheckField
            label="Drop dependents too (cascade)"
            checked={cascade}
            disabled={applying || reason !== null}
            onChange={(on) => setCascadeHash(on ? dependents.hash : null)}
          />
        </div>
      )}

      {ownFailure?.retryable && (
        <div className="flex items-center justify-between gap-3 text-[13px]">
          <p className="text-chalk-dim">Nothing was changed.</p>
          <Button size="sm" variant="outline" disabled={applying} onClick={() => void apply()}>
            Try again
          </Button>
        </div>
      )}

      {view?.apply === "needs_ack" && (
        <CheckField
          label="I've read the SQL"
          checked={acked}
          disabled={reason !== null || dry.running || applying}
          onChange={(on) => setAckFor(on ? dry.settledKey : null)}
        />
      )}
      {reason && <p className="text-[13px] text-chalk-dim">{reason}</p>}

      <div className="mt-auto flex justify-end gap-2 pt-2">
        <Button variant="ghost" disabled={applying} onClick={() => setConfirmDiscard(true)}>
          Discard
        </Button>
        <Button disabled={!canApply} onClick={() => void apply()}>
          {applying ? "Applying…" : `Apply ${changesWord(n)}`}
        </Button>
      </div>

      <Modal
        open={confirmDiscard}
        onOpenChange={setConfirmDiscard}
        title={`Discard ${changesWord(n)}?`}
        description={`The pending changes to ${draft === "new_table" ? "the new table" : subject} are dropped. Nothing has been applied.`}
      >
        <div className="flex justify-end gap-2">
          <Button variant="ghost" onClick={() => setConfirmDiscard(false)}>
            Cancel
          </Button>
          <Button
            variant="destructive"
            onClick={() => {
              setConfirmDiscard(false);
              discard(key);
              openPanel("closed");
            }}
          >
            Discard
          </Button>
        </div>
      </Modal>
    </div>
  );
}

function GeneralFailure({ failure }: { failure: ChangeFailure }) {
  const status = failure.status;
  return (
    <div className="space-y-1 border border-coral/50 p-3 text-[13px]">
      <p className="text-coral">{status.kind === "error" ? status.message : ""}</p>
      {status.kind === "error" && status.detail && <p className="text-chalk-dim">{status.detail}</p>}
      {status.kind === "error" && status.hint && <p className="text-chalk-dim">{status.hint}</p>}
    </div>
  );
}

/** Keeps "required" for the same list (a cascaded answer that repeats it stays required). */
function mergeKnown(prev: KnownDependents | null, ops: string, dependents: Dependents, required: boolean): KnownDependents {
  const same = prev !== null && prev.ops === ops && prev.dependents.hash === dependents.hash;
  return { ops, dependents, required: required || (same && prev.required) };
}
