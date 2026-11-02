import { useEffect, useId, useState, type ReactNode } from "react";
import { HeaderContext } from "@/components/app-shell/header-context";
import { ContextBar } from "@/components/table-editor/context-bar";
import { ProjectCapsContext, useProjectCaps } from "@/lib/capabilities";
import { errorText } from "@/lib/errors";
import { DATA_API_LAST_KEY, LEVELS, readLast, resolveParams, writeLast, type EditorParams } from "@/lib/table-editor/context";
import { TableEditorStoreContext, createTableEditorStore } from "@/lib/table-editor/store";
import { usePlace } from "@/lib/table-editor/use-place";
import { DataApiEndpoint, DataApiExposure, DataApiRls, DataApiSecretKeys, DataApiTypesFor, useDataApi } from "./data-api-panel";

export interface DataApiPageProps {
  params: EditorParams;
  navigate: (p: EditorParams, opts?: { replace?: boolean }) => void;
  /** The gateway's base URL (`VITE_API_URL`); the endpoint is `${apiUrl}/data/v1/query`. */
  apiUrl: string;
}

function browserStorage(kind: "local" | "session"): Storage | null {
  try {
    return kind === "local" ? window.localStorage : window.sessionStorage;
  } catch {
    return null;
  }
}

/** The place down to the Postgres database: the Data API has no schema or table level. */
const PLACE_LEVELS = LEVELS.slice(0, 4);
const sameLevels = (a: EditorParams, b: EditorParams) => PLACE_LEVELS.every((l) => (a[l] || undefined) === (b[l] || undefined));

/**
 * Studio's Data API page: the shared context bar in the header (no Act as: keys aren't roles), and
 * for the picked branch and Postgres database the endpoint and public key, what the Data API
 * serves, the secret keys, the RLS warning and generated types. Router-free like `SqlPage`.
 */
export function DataApiPage(props: DataApiPageProps) {
  // `usePlace` reads Act as from the table editor's store; this page never sets it.
  const [store] = useState(() => createTableEditorStore(browserStorage("session")));
  return (
    <TableEditorStoreContext.Provider value={store}>
      <Page {...props} />
    </TableEditorStoreContext.Provider>
  );
}

function Page({ params, navigate, apiUrl }: DataApiPageProps) {
  const [last] = useState(() => readLast(browserStorage("local"), DATA_API_LAST_KEY));
  const place = usePlace(params);
  const { me, projectList, projects, databaseOk, detail, branches, branchOk, pgDatabases, pgList, pg, caps } = place;

  // Schema is never a level here: an empty object list lets the resolver reach the end, and its schema is dropped.
  const resolution = resolveParams(params, last, {
    projects,
    branches: databaseOk ? branches.data : undefined,
    pgDatabases: branchOk ? pgList : undefined,
    objects: [],
  });
  const current: EditorParams = resolution.kind === "redirect" || resolution.kind === "ready" ? resolution.params : params;
  const redirect = resolution.kind !== "not_found" && resolution.kind !== "loading" && !sameLevels(params, current);
  const ready = (resolution.kind === "redirect" || resolution.kind === "ready") && !redirect;
  const currentKey = PLACE_LEVELS.map((l) => current[l] ?? "").join("/");
  useEffect(() => {
    if (redirect) navigate({ ...current, schema: undefined, table: undefined }, { replace: true });
    // `current` is determined by `currentKey`.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [redirect, currentKey]);
  useEffect(() => {
    if (ready) writeLast(browserStorage("local"), { ...current, schema: undefined, table: undefined }, DATA_API_LAST_KEY);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [ready, currentKey]);

  // The resolver only knows ready `normal` branches: a URL naming another branch of the database is told why.
  const named = databaseOk ? branches.data?.find((b) => b.id === params.branchId) : undefined;
  const notReady = !!named && !(named.kind === "normal" && named.status === "ready");

  const error = projectList.error ?? me.error ?? branches.error ?? (notReady ? null : pgDatabases.error);
  let main;
  if (notReady) {
    main = <p role="status" className="p-6 text-sm text-chalk-dim">The Data API runs on ready branches.</p>;
  } else if (resolution.kind === "not_found") {
    main = <p className="p-6 text-sm">{{ project: "Project", database: "Database", branch: "Branch", pgDatabase: "Postgres database", schema: "Schema", table: "Table" }[resolution.level]} not found</p>;
  } else if (error) {
    main = (
      <p role="alert" className="p-6 text-sm text-coral">
        {errorText(error)}
      </p>
    );
  } else if (!ready) {
    main = (
      <p role="status" className="p-6 text-sm text-chalk-dim">
        Loading…
      </p>
    );
  } else if (!current.projectId) {
    main = <p className="p-6 text-sm text-chalk-dim">No projects yet</p>;
  } else if (!current.databaseId) {
    main = <p className="p-6 text-sm text-chalk-dim">This project has no databases</p>;
  } else if (!current.branchId) {
    main = <p className="p-6 text-sm text-chalk-dim">No ready branch</p>;
  } else if (!current.pgDatabase || !pg) {
    main = <p className="p-6 text-sm text-chalk-dim">This branch has no main Postgres database: pick one above</p>;
  } else {
    // A fresh set per database and branch: a rotate confirmation never carries over.
    main = <Sections key={`${current.databaseId}/${current.branchId}`} databaseId={current.databaseId} branchId={current.branchId} pgDatabase={pg.name} apiUrl={apiUrl} />;
  }

  return (
    <div className="flex min-h-0 flex-col">
      <HeaderContext>
        <ContextBar
          params={current}
          navigate={(p) => navigate(p)}
          projects={projects}
          branches={databaseOk ? branches.data : undefined}
          pgDatabases={branchOk ? pgList : undefined}
          onActAs={() => {}}
          historyWindow={detail.data?.history_window}
          onBranchesOpen={() => void branches.refetch()}
          onBranchCreated={(b) => place.setCreatedBranchId(b.id)}
        />
      </HeaderContext>
      {/* The caps of the database in the bar: the default is every cap, so without this a Viewer would see Rotate and New key. */}
      <ProjectCapsContext.Provider value={caps}>{main}</ProjectCapsContext.Provider>
    </div>
  );
}

function Card({ title, className = "", children }: { title: string; className?: string; children: ReactNode }) {
  const id = useId();
  return (
    <section aria-labelledby={id} className={`min-w-0 space-y-3 border border-line p-4 ${className}`}>
      <h2 id={id} className="text-sm font-semibold">
        {title}
      </h2>
      {children}
    </section>
  );
}

function Sections({ databaseId, branchId, pgDatabase, apiUrl }: { databaseId: string; branchId: string; pgDatabase: string; apiUrl: string }) {
  const api = useDataApi(databaseId, branchId, pgDatabase);
  // Viewers can't download types: no empty card for them.
  const canDownload = useProjectCaps().atLeast("developer");
  return (
    <div className="mx-auto grid w-full max-w-6xl gap-4 p-4 text-sm sm:p-8 min-[64rem]:grid-cols-2">
      <Card title="Endpoint and public key">
        <DataApiEndpoint api={api} apiUrl={apiUrl} />
      </Card>
      <Card title="Postgres databases">
        <DataApiExposure api={api} />
      </Card>
      <Card title="Secret keys" className="min-[64rem]:col-span-2">
        <DataApiSecretKeys api={api} />
      </Card>
      <Card title="Row-level security" className="min-[64rem]:col-span-2">
        <p className="text-chalk-dim">
          {api.pickedDb && api.pickedDb.data_api !== "on"
            ? `The Data API doesn't serve ${api.pickedDb.name}.`
            : api.schemaLoading
              ? "Checking…"
              : "Tables the Data API reaches without row-level security are listed here."}
        </p>
        <DataApiRls api={api} />
      </Card>
      {canDownload && (
        <Card title="Generated types" className="min-[64rem]:col-span-2">
          <DataApiTypesFor api={api} />
        </Card>
      )}
    </div>
  );
}
