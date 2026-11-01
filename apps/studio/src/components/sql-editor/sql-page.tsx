import { lazy, Suspense, useEffect, useState } from "react";
import { HeaderContext } from "@/components/app-shell/header-context";
import { ContextBar } from "@/components/table-editor/context-bar";
import { errorText } from "@/lib/errors";
import type { PglsClient } from "@/lib/pgls/client";
import { LEVELS, readLast, resolveParams, SQL_LAST_KEY, writeLast, type EditorParams } from "@/lib/table-editor/context";
import { TableEditorStoreContext, createTableEditorStore } from "@/lib/table-editor/store";
import { useFillViewport } from "@/lib/use-fill-viewport";
import { usePlace } from "@/lib/table-editor/use-place";

// The editor, CodeMirror and the pgls worker load only when the SQL page opens.
const SqlEditor = lazy(() => import("./sql-editor").then((m) => ({ default: m.SqlEditor })));

export interface SqlPageProps {
  params: EditorParams;
  navigate: (p: EditorParams, opts?: { replace?: boolean }) => void;
  /** Starts the pgls worker (tests inject a fake). */
  createClient?: () => Promise<PglsClient>;
}

function browserStorage(kind: "local" | "session"): Storage | null {
  try {
    return kind === "local" ? window.localStorage : window.sessionStorage;
  } catch {
    return null;
  }
}

/** The place down to the Postgres database: the SQL editor has no schema or table level. */
const PLACE_LEVELS = LEVELS.slice(0, 4);
const sameLevels = (a: EditorParams, b: EditorParams) => PLACE_LEVELS.every((l) => (a[l] || undefined) === (b[l] || undefined));

/**
 * Studio's SQL editor page: the shared context bar in the header (the place and Act as are the
 * table editor's, from `usePlace`), saved queries and history on the left, the editor and results
 * on the right. Router-free: the route passes the URL's params and a `navigate`.
 */
export function SqlPage(props: SqlPageProps) {
  // The same store as the table editor's: Act as is kept per database and branch in session storage.
  const [store] = useState(() => createTableEditorStore(browserStorage("session")));
  return (
    <TableEditorStoreContext.Provider value={store}>
      <Page {...props} />
    </TableEditorStoreContext.Provider>
  );
}

function Page({ params, navigate, createClient }: SqlPageProps) {
  const fill = useFillViewport();
  const [last] = useState(() => readLast(browserStorage("local"), SQL_LAST_KEY));
  const place = usePlace(params);
  const { me, projectList, projects, databaseOk, detail, branches, branch, branchOk, pgDatabases, pgList, pg, caps, allowed, actAs, actAsReady, rolesError } = place;

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
    if (ready) writeLast(browserStorage("local"), { ...current, schema: undefined, table: undefined }, SQL_LAST_KEY);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [ready, currentKey]);

  const error = projectList.error ?? me.error ?? branches.error ?? pgDatabases.error;
  let main;
  if (resolution.kind === "not_found") {
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
  } else if (!caps.atLeast("developer")) {
    main = <p role="status" className="p-6 text-sm text-chalk-dim">The SQL editor needs the Developer role on this database. Viewers can read through the table editor.</p>;
  } else if (!current.branchId) {
    main = <p className="p-6 text-sm text-chalk-dim">No ready branch</p>;
  } else if (!current.pgDatabase || !pg || !branch) {
    main = <p className="p-6 text-sm text-chalk-dim">This branch has no main Postgres database: pick one above</p>;
  } else {
    main = (
      <Suspense fallback={<p className="p-6 text-sm text-chalk-dim">Loading the editor…</p>}>
        <SqlEditor
          key={current.databaseId}
          databaseId={current.databaseId}
          branchId={current.branchId}
          pgDatabase={pg.main ? null : pg.name}
          pgDatabases={(pgList ?? []).map((d) => d.name)}
          role={actAs}
          allowed={allowed}
          rolesReady={actAsReady}
          branchProtected={branch.protected}
          onPgDatabase={(name) => navigate({ ...current, pgDatabase: name })}
          onRole={place.setActAs}
          createClient={createClient}
        />
      </Suspense>
    );
  }

  return (
    <div ref={fill.ref} style={fill.style} className="flex min-h-0 flex-col">
      <HeaderContext>
        <ContextBar
          params={current}
          navigate={(p) => navigate(p)}
          projects={projects}
          branches={databaseOk ? branches.data : undefined}
          pgDatabases={branchOk ? pgList : undefined}
          actAs={actAsReady ? { owner: allowed.owner, roles: allowed.roles, selected: actAs } : undefined}
          onActAs={place.setActAs}
          rolesError={rolesError}
          historyWindow={detail.data?.history_window}
          onBranchesOpen={() => void branches.refetch()}
          onBranchCreated={(b) => place.setCreatedBranchId(b.id)}
        />
      </HeaderContext>
      <div className="flex min-h-0 flex-1 flex-col">{main}</div>
    </div>
  );
}
