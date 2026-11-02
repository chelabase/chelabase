import { useState } from "react";
import { Link } from "@tanstack/react-router";
import { useGetMe, useListBranchDatabases, useListBranches, type BranchDatabaseView, type DatabaseSummary } from "@chelabase/management-client";
import { useProjectCaps } from "@/lib/capabilities";
import { errorText } from "@/lib/errors";
import { DatabasePicker } from "./data-api-databases";
import { ConnectionDetails } from "./connection-details";
import { DATA_API_ROUTE } from "./app-shell/rail-items";
import { DataApiEndpoint, useDataApi } from "./data-api-panel";

export interface ConnectPanelProps {
  projectId: string;
  database: DatabaseSummary;
  /** The gateway's base URL, for the data API endpoint. */
  apiUrl: string;
}

/** Connect to a database: pick a ready branch (main by default), then its Data API and keys. */
export function ConnectPanel({ projectId, database, apiUrl }: ConnectPanelProps) {
  const id = database.database_id;
  const list = useListBranches(id);
  const [picked, setPicked] = useState<string | null>(null);
  const branches = list.data ?? null;
  const error = list.error ? errorText(list.error) : null;

  // A personal login is Developer's, on this database (the project page provides the database's caps).
  const caps = useProjectCaps();
  const canLogin = caps.atLeast("developer");
  // The owner's connection (with its password) is Admin's.
  const admin = caps.atLeast("admin");
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
      {current && (admin || (canLogin && memberId)) && (
        <section aria-label="Postgres" className="space-y-2">
          <h3 className="text-sm font-semibold">Postgres</h3>
          {/* Once the branch's databases are known, so the owner connection is fetched once. */}
          {admin && pickedDb && (
            <ConnectionDetails
              key={current.id}
              database={database}
              branch={current}
              pgDatabase={pickedDb.main ? null : pickedDb.name}
            />
          )}
          {canLogin && memberId && (
            <p className="text-sm text-muted-foreground">
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
          )}
        </section>
      )}
      {current && (
        <ConnectDataApi
          key={current.id}
          projectId={projectId}
          databaseId={id}
          branchId={current.id}
          apiUrl={apiUrl}
          pgDatabase={pickedDb?.name}
        />
      )}
    </section>
  );
}

/** Why the Data API doesn't serve a Postgres database yet, in its real state. */
function servedText(d: BranchDatabaseView): string {
  if (d.origin === "system") return "The Data API can't serve the system database.";
  switch (d.data_api) {
    case "enabling":
      return `The Data API is enabling for ${d.name}.`;
    case "error":
      return `The Data API failed to enable for ${d.name}.`;
    default:
      return `The Data API is off for ${d.name}.`;
  }
}

/** What to connect the Data API with: the endpoint and public key, and the page that manages them. */
function ConnectDataApi({
  projectId,
  databaseId,
  branchId,
  apiUrl,
  pgDatabase,
}: {
  projectId: string;
  databaseId: string;
  branchId: string;
  apiUrl: string;
  pgDatabase: string | undefined;
}) {
  const api = useDataApi(databaseId, branchId, pgDatabase);
  const picked = api.pickedDb;
  // The Data API page doesn't offer the system database: link to the branch, which opens on main.
  const system = picked?.origin === "system";
  return (
    <section aria-label="Data API" className="space-y-3 text-sm">
      <h3 className="font-semibold">Data API</h3>
      <DataApiEndpoint api={api} apiUrl={apiUrl} readOnly />
      {/* The keys' and a rotate's errors show in the endpoint above. */}
      {api.dbError && (
        <p role="alert" className="text-destructive">
          {api.dbError}
        </p>
      )}
      <p className="text-muted-foreground">
        {picked && picked.data_api !== "on" && <>{servedText(picked)} </>}
        <Link
          to={DATA_API_ROUTE}
          params={{ projectId, databaseId, branchId, pgDatabase: system ? undefined : picked?.name }}
          className="underline underline-offset-4 hover:text-foreground"
        >
          Manage Data API
        </Link>
      </p>
    </section>
  );
}
