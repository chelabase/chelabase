import { useGetDatabase, useListBranches, type Branch, type DatabaseSettings as Settings, type DatabaseSummary } from "@cheladb/management-client";
import { isTransitional } from "@/lib/branches";
import { errorText } from "@/lib/errors";
import { pollWhile } from "@/lib/query";
import { BranchTree } from "./branch-tree";
import { DatabaseSettings } from "./database-settings";
import { NewBranchForm } from "./new-branch-form";

export interface DatabaseBranchesProps {
  database: DatabaseSummary;
}

const DEFAULT_WINDOW = 7 * 86400;

/** One database's branches: the tree, the new-branch form and the settings. */
export function DatabaseBranches({ database }: DatabaseBranchesProps) {
  const id = database.database_id;
  // While a branch is being created, reset, restored or deleted, keep the list fresh.
  const list = useListBranches(id, { query: { refetchInterval: pollWhile<Branch[]>((bs) => bs.some(isTransitional)) } });
  const detail = useGetDatabase(id);
  const branches = list.data ?? null;
  const d = detail.data;
  const settings: Settings | null = d
    ? {
        history_window: d.history_window,
        max_branches: d.max_branches,
        max_rows: d.max_rows,
        statement_timeout_ms: d.statement_timeout_ms,
        max_connections: d.max_connections,
        default_schema: d.default_schema,
      }
    : null;
  const failed = list.error ?? detail.error;
  const error = failed ? errorText(failed) : null;

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
          <BranchTree databaseId={id} databaseName={database.name} branches={branches} historyWindow={window} />
          <NewBranchForm databaseId={id} branches={branches} historyWindow={window} />
        </>
      )}
      {settings && (
        // Saving invalidates getDatabase(id); the refetch brings the new settings here.
        <DatabaseSettings key={Object.values(settings).join("-")} databaseId={id} settings={settings} />
      )}
    </div>
  );
}
