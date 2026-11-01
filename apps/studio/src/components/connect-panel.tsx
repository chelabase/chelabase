import { useState } from "react";
import { Link } from "@tanstack/react-router";
import { useGetMe, useListBranchDatabases, useListBranches, type DatabaseSummary } from "@chelabase/management-client";
import { useProjectCaps } from "@/lib/capabilities";
import { errorText } from "@/lib/errors";
import { DatabasePicker } from "./data-api-databases";
import { DataApiPanel } from "./data-api-panel";

export interface ConnectPanelProps {
  database: DatabaseSummary;
  /** The gateway's base URL, for the data API endpoint. */
  apiUrl: string;
}

/** Connect to a database: pick a ready branch (main by default), then its Data API and keys. */
export function ConnectPanel({ database, apiUrl }: ConnectPanelProps) {
  const id = database.database_id;
  const list = useListBranches(id);
  const [picked, setPicked] = useState<string | null>(null);
  const branches = list.data ?? null;
  const error = list.error ? errorText(list.error) : null;

  // A personal login is Developer's, on this database (the project page provides the database's caps).
  const canLogin = useProjectCaps().atLeast("developer");
  const memberId = useGetMe().data?.member?.id;

  // The data API runs on `normal` branches that are ready.
  const ready = (branches ?? []).filter((b) => b.kind === "normal" && b.status === "ready");
  const current = ready.find((b) => b.id === picked) ?? ready.find((b) => b.parent_branch_id === null) ?? ready[0];
  // The branch's Postgres databases (the Data API panel's own query: no extra request); main unless one is picked
  // on this branch.
  const dbList = useListBranchDatabases(id, current?.id ?? "", { query: { enabled: !!current } });
  const [pickedPg, setPickedPg] = useState<{ branchId: string; name: string } | null>(null);
  const databases = current ? (dbList.data ?? null) : null;
  const pickedName = pickedPg?.branchId === current?.id ? pickedPg?.name : undefined;
  const pickedDb = databases?.find((d) => d.name === pickedName) ?? databases?.find((d) => d.main) ?? databases?.[0];

  return (
    <section aria-label={`Connect to ${database.name}`} className="space-y-3 py-3">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <h2 className="text-base font-semibold">Connect to {database.name}</h2>
        {current && (
          <div className="flex flex-wrap items-center gap-3 text-sm">
            <label className="flex items-center gap-2">
              <span>Branch</span>
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
            {current && databases && pickedDb && (
              <DatabasePicker label="Postgres DB" databases={databases} value={pickedDb.name} onPick={(name) => setPickedPg({ branchId: current.id, name })} />
            )}
          </div>
        )}
      </div>
      {error && (
        <p role="alert" className="text-sm text-destructive">
          {error}
        </p>
      )}
      {branches === null && !error && <p className="text-sm text-muted-foreground">Loading branches…</p>}
      {branches !== null && !current && <p className="text-sm text-muted-foreground">No branch is ready yet.</p>}
      {current && (
        <DataApiPanel
          key={current.id}
          databaseId={id}
          branch={current}
          apiUrl={apiUrl}
          pgDatabase={pickedDb?.name}
          aside={
            canLogin &&
            memberId && (
              <p className="text-muted-foreground">
                Connecting with psql or a driver?{" "}
                <Link
                  to="/app/members/$memberId"
                  params={{ memberId }}
                  hash="personal-logins"
                  className="underline underline-offset-4 hover:text-foreground"
                >
                  Use your personal Postgres login.
                </Link>
              </p>
            )
          }
        />
      )}
    </section>
  );
}
