import { useState } from "react";
import { useListBranches, type DatabaseSummary } from "@cheladb/management-client";
import { errorText } from "@/lib/errors";
import { DataApiPanel } from "./data-api-panel";

export interface DatabaseDataApiProps {
  database: DatabaseSummary;
  /** The gateway's base URL, for the data API endpoint. */
  apiUrl: string;
}

/** A database's Data API view: pick a ready branch (main by default), then its panel. */
export function DatabaseDataApi({ database, apiUrl }: DatabaseDataApiProps) {
  const id = database.database_id;
  const list = useListBranches(id);
  const [picked, setPicked] = useState<string | null>(null);
  const branches = list.data ?? null;
  const error = list.error ? errorText(list.error) : null;

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
          <DataApiPanel key={current.id} databaseId={id} branch={current} apiUrl={apiUrl} />
        </>
      )}
    </div>
  );
}
