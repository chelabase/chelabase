import { useEffect, useState } from "react";
import type { BranchSummary, DatabaseSummary } from "@cheladb/types";
import type { ApiClient } from "@/lib/api";
import { DataApiPanel } from "./data-api-panel";

export interface DatabaseDataApiProps {
  api: ApiClient;
  database: DatabaseSummary;
  /** The gateway's base URL, for the data API endpoint. */
  apiUrl: string;
}

/** A database's Data API view: pick a ready branch (main by default), then its panel. */
export function DatabaseDataApi({ api, database, apiUrl }: DatabaseDataApiProps) {
  const id = database.database_id;
  const [branches, setBranches] = useState<BranchSummary[] | null>(null);
  const [picked, setPicked] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    api
      .listBranches(id)
      .then(setBranches)
      .catch((err) => setError(err instanceof Error ? err.message : String(err)));
  }, [api, id]);

  // The data API runs on `normal` branches that are ready.
  const ready = (branches ?? []).filter((b) => b.kind === "normal" && b.status === "ready");
  const current = ready.find((b) => b.id === picked) ?? ready.find((b) => b.parent_branch_id === null) ?? ready[0];

  return (
    <div className="space-y-3 py-3">
      {error && (
        <p role="alert" className="text-sm text-destructive">
          {error}
        </p>
      )}
      {branches === null && !error && <p className="text-sm text-muted-foreground">Loading branches…</p>}
      {branches !== null && !current && <p className="text-sm text-muted-foreground">No branch is ready yet.</p>}
      {current && (
        <>
          <label className="flex items-center gap-2 text-sm">
            <span>Data API branch</span>
            <select
              value={current.id}
              onChange={(e) => setPicked(e.target.value)}
              className="h-8 rounded-lg border border-input bg-background px-2 text-sm"
            >
              {ready.map((b) => (
                <option key={b.id} value={b.id}>
                  {b.name}
                </option>
              ))}
            </select>
          </label>
          <DataApiPanel key={current.id} api={api} databaseId={id} branch={current} apiUrl={apiUrl} />
        </>
      )}
    </div>
  );
}
