import { useEffect, useState, type ReactNode } from "react";
import { HeaderContext } from "@/components/app-shell/header-context";
import { ContextBar } from "@/components/table-editor/context-bar";
import { ProjectCapsContext } from "@/lib/capabilities";
import { errorText } from "@/lib/errors";
import { LEVELS, readLast, resolveParams, writeLast, type EditorParams } from "@/lib/table-editor/context";
import { TableEditorStoreContext, createTableEditorStore } from "@/lib/table-editor/store";
import { usePlace } from "@/lib/table-editor/use-place";
import { FunctionList } from "./function-list";

/** The Functions tab's place: down to the branch (functions belong to the database, their state to the branch). */
export interface FunctionsParams {
  projectId?: string;
  databaseId?: string;
  branchId?: string;
}

/** The Functions tab's own last place. */
export const FUNCTIONS_LAST_KEY = "chelabase.studio.functions.last";

export interface FunctionsPageProps {
  params: FunctionsParams;
  navigate: (p: FunctionsParams, opts?: { replace?: boolean }) => void;
  /** The gateway's base URL: functions are invoked at `${apiUrl}/functions/v1/…`. */
  apiUrl: string;
}

const PLACE_LEVELS = LEVELS.slice(0, 3) as (keyof FunctionsParams)[];
const placeOf = (p: EditorParams | FunctionsParams): FunctionsParams => Object.fromEntries(PLACE_LEVELS.flatMap((l) => (p[l] ? [[l, p[l]]] : [])));
const sameLevels = (a: FunctionsParams, b: FunctionsParams) => PLACE_LEVELS.every((l) => (a[l] || undefined) === (b[l] || undefined));

function browserStorage(kind: "local" | "session"): Storage | null {
  try {
    return kind === "local" ? window.localStorage : window.sessionStorage;
  } catch {
    return null;
  }
}

/**
 * Studio's Functions tab: the shared context bar (project, database, branch; no Act as: functions
 * are managed as the member) and the branch's functions. Router-free like the Data API page.
 */
export function FunctionsPage(props: FunctionsPageProps) {
  const [store] = useState(() => createTableEditorStore(browserStorage("session")));
  return (
    <TableEditorStoreContext.Provider value={store}>
      <Page {...props} />
    </TableEditorStoreContext.Provider>
  );
}

function Notice({ children, alert = false }: { children: ReactNode; alert?: boolean }) {
  return (
    <p role={alert ? "alert" : "status"} className={alert ? "p-6 text-sm text-coral" : "p-6 text-sm text-chalk-dim"}>
      {children}
    </p>
  );
}

function Page({ params, navigate, apiUrl }: FunctionsPageProps) {
  const [last] = useState(() => readLast(browserStorage("local"), FUNCTIONS_LAST_KEY));
  const place = usePlace(placeOf(params));
  const { me, projectList, projects, databaseOk, detail, branches, caps } = place;

  // No Postgres database level: an empty list lets the resolver stop at the branch.
  const resolution = resolveParams(placeOf(params), last, { projects, branches: databaseOk ? branches.data : undefined, pgDatabases: [] });
  const current = resolution.kind === "redirect" || resolution.kind === "ready" ? placeOf(resolution.params) : placeOf(params);
  const redirect = resolution.kind !== "not_found" && resolution.kind !== "loading" && !sameLevels(params, current);
  const ready = (resolution.kind === "redirect" || resolution.kind === "ready") && !redirect;
  const currentKey = PLACE_LEVELS.map((l) => current[l] ?? "").join("/");
  useEffect(() => {
    if (redirect) navigate(current, { replace: true });
    // `current` is determined by `currentKey`.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [redirect, currentKey]);
  useEffect(() => {
    if (ready) writeLast(browserStorage("local"), current, FUNCTIONS_LAST_KEY);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [ready, currentKey]);

  const error = projectList.error ?? me.error ?? branches.error;
  let main;
  if (resolution.kind === "not_found") {
    main = <Notice>{{ project: "Project", database: "Database", branch: "Branch", pgDatabase: "Postgres database", schema: "Schema", table: "Table" }[resolution.level]} not found</Notice>;
  } else if (error) {
    main = <Notice alert>{errorText(error)}</Notice>;
  } else if (!ready) {
    main = <Notice>Loading…</Notice>;
  } else if (!current.projectId) {
    main = <Notice>No projects yet</Notice>;
  } else if (!current.databaseId) {
    main = <Notice>This project has no databases</Notice>;
  } else if (!current.branchId) {
    main = <Notice>No ready branch</Notice>;
  } else {
    // A fresh list per database and branch: no optimistic switch or open drawer carries over.
    main = <FunctionList key={`${current.databaseId}/${current.branchId}`} databaseId={current.databaseId} branchId={current.branchId} apiUrl={apiUrl} />;
  }

  return (
    <div className="flex min-h-0 flex-col">
      <HeaderContext>
        <ContextBar
          params={current}
          navigate={(p) => navigate(placeOf(p))}
          projects={projects}
          branches={databaseOk ? branches.data : undefined}
          pgLevel={false}
          onActAs={() => {}}
          historyWindow={detail.data?.history_window}
          onBranchesOpen={() => void branches.refetch()}
          onBranchCreated={(b) => place.setCreatedBranchId(b.id)}
        />
      </HeaderContext>
      {/* The caps of the database in the bar: without this a Viewer would get every control. */}
      <ProjectCapsContext.Provider value={caps}>{main}</ProjectCapsContext.Provider>
    </div>
  );
}
