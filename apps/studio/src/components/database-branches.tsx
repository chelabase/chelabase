import { useCallback, useEffect, useState } from "react";
import type { BranchSummary, DatabaseSettings as Settings, DatabaseSummary } from "@cheladb/types";
import type { ApiClient } from "@/lib/api";
import { isTransitional } from "@/lib/branches";
import { BranchTree } from "./branch-tree";
import { DatabaseSettings } from "./database-settings";
import { NewBranchForm } from "./new-branch-form";

export interface DatabaseBranchesProps {
  api: ApiClient;
  database: DatabaseSummary;
  pollIntervalMs?: number;
}

const DEFAULT_WINDOW = 7 * 86400;

/** One database's branches: the tree, the new-branch form and the settings. */
export function DatabaseBranches({ api, database, pollIntervalMs = 2000 }: DatabaseBranchesProps) {
  const id = database.database_id;
  const [branches, setBranches] = useState<BranchSummary[] | null>(null);
  const [settings, setSettings] = useState<Settings | null>(null);
  const [error, setError] = useState<string | null>(null);

  const load = useCallback(async () => {
    try {
      setBranches(await api.listBranches(id));
      setError(null);
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    }
  }, [api, id]);

  useEffect(() => {
    void load();
    api
      .getDatabase(id)
      .then((d) =>
        setSettings({
          history_window: d.history_window,
          max_branches: d.max_branches,
          max_rows: d.max_rows,
          statement_timeout_ms: d.statement_timeout_ms,
          max_connections: d.max_connections,
          default_schema: d.default_schema,
        }),
      )
      .catch((err) => setError(err instanceof Error ? err.message : String(err)));
  }, [api, id, load]);

  useEffect(() => {
    if (!branches?.some(isTransitional)) return;
    const timer = setTimeout(() => void load(), pollIntervalMs);
    return () => clearTimeout(timer);
  }, [branches, load, pollIntervalMs]);

  const window = settings?.history_window ?? DEFAULT_WINDOW;
  // `normal` branches other than main, as the cap counts them.
  const used = branches?.filter((b) => b.kind === "normal" && b.parent_branch_id !== null && b.status !== "deleted").length;

  return (
    <div className="space-y-4 py-3">
      {error && (
        <p role="alert" className="text-sm text-destructive">
          {error}
        </p>
      )}
      {branches === null && !error && <p className="text-sm text-muted-foreground">Loading branches…</p>}
      {branches !== null && (
        <>
          <p className="text-sm text-muted-foreground">
            {settings ? `${used} of ${settings.max_branches} branches` : `${used} branches`}
          </p>
          <BranchTree
            api={api}
            databaseId={id}
            databaseName={database.name}
            branches={branches}
            historyWindow={window}
            onChanged={() => void load()}
          />
          <NewBranchForm
            api={api}
            databaseId={id}
            branches={branches}
            historyWindow={window}
            onCreated={() => void load()}
          />
        </>
      )}
      {settings && (
        <DatabaseSettings
          key={Object.values(settings).join("-")}
          api={api}
          databaseId={id}
          settings={settings}
          onSaved={setSettings}
        />
      )}
    </div>
  );
}
