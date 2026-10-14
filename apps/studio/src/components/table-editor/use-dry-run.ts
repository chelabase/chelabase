import { useEffect, useMemo, useState } from "react";
import { ApiError, applySchemaChanges, type SchemaChangeRequest } from "@chelabase/management-client";
import { opsOf, type Edit } from "@/lib/table-editor/changeset";
import { mapChangeError, mapDryRun, type ChangeFailure, type DryRunView } from "@/lib/table-editor/dry-run";

/** Wait after the last change before a dry run (S1: 400 ms after the last edit, and not while an edit dialog is open). */
export const DRY_RUN_DEBOUNCE_MS = 400;

export interface DryRunArgs {
  databaseId: string;
  branchId: string;
  /** The `database` request parameter: null for the branch's main database. */
  database: string | null;
  /** The role the change runs as; null is the owner. */
  role: string | null;
  edits: Edit[];
  /** The qualified table name, for warnings ("public.invoices"). */
  table: string;
  totalBytes: number | null;
  enabled: boolean;
  /** The dependents hash the user confirmed: the dry run then cascades as Apply would. */
  cascade?: string | null;
  /** Anything else that should re-run the dry run when it changes (the changeset's base after a review). */
  revision?: string | null;
  /** True while an edit dialog is open: no new dry run starts (the shown result stays) until it closes. */
  paused?: boolean;
}

export interface DryRunState {
  /** The latest settled dry run (kept while a newer one runs: check `running`). */
  view: DryRunView | null;
  /** The latest settled dry run's failure. */
  failure: ChangeFailure | null;
  /** True while the shown result is not for the current edits (waiting or in flight). */
  running: boolean;
  /** The current inputs' key (operations, cascade, revision, role, database). */
  key: string;
  /** The key of the settled answer (equal to `key` unless `running`); null before the first. */
  settledKey: string | null;
  /** What the settled dry run sent: Apply sends exactly this with `dry_run: false`. */
  request: { body: SchemaChangeRequest; edits: Edit[] } | null;
}

interface Settled {
  key: string;
  view: DryRunView | null;
  failure: ChangeFailure | null;
  request: { body: SchemaChangeRequest; edits: Edit[] };
}

/** The request body a schema change sends: `cascade` and `lock_timeout_ms` only when set. */
export function schemaChangeBody(a: { database: string | null; role: string | null; edits: Edit[]; dryRun: boolean; cascade?: string | null }): SchemaChangeRequest {
  return {
    database: a.database,
    role: a.role,
    dry_run: a.dryRun,
    operations: opsOf(a.edits),
    ...(a.cascade ? { cascade: { dependents_hash: a.cascade } } : {}),
  };
}

/**
 * TE-A's dry run of `edits`: 400 ms after their operations last changed, aborting a stale request.
 * Nothing runs while disabled or with no operations.
 */
export function useDryRun(args: DryRunArgs): DryRunState {
  const { databaseId, branchId, database, role, edits, table, totalBytes, enabled } = args;
  const cascade = args.cascade ?? null;
  const ops = useMemo(() => JSON.stringify(opsOf(edits)), [edits]);
  const active = enabled && edits.some((e) => e.ops.length > 0);
  const paused = args.paused ?? false;
  const key = JSON.stringify([databaseId, branchId, database, role, ops, cascade, args.revision ?? null]);
  const [settled, setSettled] = useState<Settled | null>(null);

  const settledRef = useLatest(settled);

  useEffect(() => {
    if (!active) {
      setSettled(null);
      return;
    }
    // Nothing to ask while an edit dialog is open, or when the shown answer is already for these inputs.
    if (paused || settledRef.current?.key === key) return;
    const abort = new AbortController();
    const timer = setTimeout(() => {
      // Built from this effect's own render: the edits `key` describes.
      const sent = edits;
      const name = table;
      const bytes = totalBytes;
      const body = schemaChangeBody({ database, role, edits: sent, dryRun: true, cascade });
      applySchemaChanges(databaseId, branchId, body, { signal: abort.signal }).then(
        (res) => {
          if (!abort.signal.aborted) setSettled({ key, view: mapDryRun(sent, res, { table: name, totalBytes: bytes }), failure: null, request: { body, edits: sent } });
        },
        (err: unknown) => {
          if (abort.signal.aborted) return;
          const failure =
            err instanceof ApiError
              ? mapChangeError(sent, err)
              : { edit: null, status: { kind: "error" as const, message: err instanceof Error ? err.message : "The dry run failed", detail: null, hint: null }, code: "internal", retryable: true, dependents: null };
          setSettled({ key, view: null, failure, request: { body, edits: sent } });
        },
      );
    }, DRY_RUN_DEBOUNCE_MS);
    return () => {
      clearTimeout(timer);
      abort.abort();
    };
    // `key` covers every input of the request (the edits by their operations).
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [key, active, paused]);

  if (!active) return { view: null, failure: null, running: false, key, settledKey: null, request: null };
  return {
    view: settled?.view ?? null,
    failure: settled?.failure ?? null,
    running: settled?.key !== key,
    key,
    settledKey: settled?.key ?? null,
    request: settled?.request ?? null,
  };
}

function useLatest<T>(value: T): { current: T } {
  const [ref] = useState(() => ({ current: value }));
  ref.current = value;
  return ref;
}
