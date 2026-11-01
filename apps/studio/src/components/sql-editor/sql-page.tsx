import { lazy, Suspense, useEffect, useRef, useState } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { HeaderContext } from "@/components/app-shell/header-context";
import { ContextBar } from "@/components/table-editor/context-bar";
import { errorText } from "@/lib/errors";
import type { PglsClient } from "@/lib/pgls/client";
import { LEVELS, readLast, resolveParams, SQL_LAST_KEY, writeLast, type EditorParams } from "@/lib/table-editor/context";
import { TableEditorStoreContext, createTableEditorStore } from "@/lib/table-editor/store";
import { useFillViewport } from "@/lib/use-fill-viewport";
import { usePlace } from "@/lib/table-editor/use-place";
import { invalidateSchemaQueries } from "@/lib/table-editor/query-keys";
import type { SqlEditorProps } from "./sql-editor";

// The editor, CodeMirror and the pgls worker load only when the SQL page opens.
const SqlEditor = lazy(() => import("./sql-editor").then((m) => ({ default: m.SqlEditor })));

export interface SqlPageProps {
  params: EditorParams;
  navigate: (p: EditorParams, opts?: { replace?: boolean }) => void;
  /** Quick view: the table editor at this place with the SQL drawer open. */
  openTables?: (p: EditorParams) => void;
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

/** Quick view's place: the table last open in the table editor when it was at this place, else this place's table list. */
function quickViewTarget(current: EditorParams): EditorParams {
  const place: EditorParams = Object.fromEntries(PLACE_LEVELS.flatMap((l) => (current[l] ? [[l, current[l]]] : [])));
  const last = readLast(browserStorage("local"));
  return last?.table && sameLevels(last, place) ? { ...place, schema: last.schema, table: last.table } : place;
}

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

function Page({ params, navigate, openTables, createClient }: SqlPageProps) {
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

  const queryClient = useQueryClient();
  // The editor's place once it is known. While the next place resolves (another branch or Postgres database is
  // picked), the last one stays mounted with nothing to run, so a run in flight, the results, undo and the pgls worker
  // survive the switch. Another database starts over (its buffer and history are its own).
  const shown = useRef<{ place: EditorParams; props: Omit<SqlEditorProps, "createClient"> } | null>(null);
  if (ready && current.databaseId && current.branchId && current.pgDatabase && pg && branch && caps.atLeast("developer")) {
    const { databaseId, branchId } = current;
    const pgParam = pg.main ? null : pg.name;
    shown.current = {
      place: current,
      props: {
        databaseId,
        branchId,
        pgDatabase: pgParam,
        pgDatabases: (pgList ?? []).map((d) => d.name),
        role: actAs,
        allowed,
        rolesReady: actAsReady,
        branchProtected: branch.protected,
        onHints: ({ database, role }) => {
          if (role) place.setActAs(role);
          if (database && database !== pg.name) navigate({ ...current, pgDatabase: database });
        },
        onSchemaChange: () => invalidateSchemaQueries(queryClient, databaseId, branchId, pgParam ? { database: pgParam } : undefined),
        onQuickView: openTables && (() => openTables(quickViewTarget(current))),
      },
    };
  } else if (ready || (shown.current && (shown.current.place.projectId !== params.projectId || shown.current.place.databaseId !== params.databaseId))) {
    shown.current = null;
  }
  const held = !ready && resolution.kind !== "not_found" ? shown.current : null;

  const error = projectList.error ?? me.error ?? branches.error ?? pgDatabases.error;
  const editor = (props: Omit<SqlEditorProps, "createClient">) => (
    <Suspense fallback={<p className="p-6 text-sm text-chalk-dim">Loading the editor…</p>}>
      <SqlEditor key={props.databaseId} {...props} createClient={createClient} />
    </Suspense>
  );
  let main;
  if (resolution.kind === "not_found") {
    main = <p className="p-6 text-sm">{{ project: "Project", database: "Database", branch: "Branch", pgDatabase: "Postgres database", schema: "Schema", table: "Table" }[resolution.level]} not found</p>;
  } else if (error) {
    main = (
      <p role="alert" className="p-6 text-sm text-coral">
        {errorText(error)}
      </p>
    );
  } else if (held) {
    main = editor({ ...held.props, rolesReady: false });
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
  } else if (shown.current) {
    main = editor(shown.current.props);
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
