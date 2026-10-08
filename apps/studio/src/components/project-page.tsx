import { Fragment, lazy, Suspense, useEffect, useState, type ReactNode } from "react";
import { Link } from "@tanstack/react-router";
import {
  ApiError,
  useGetMe,
  useGetProject,
  type DatabaseSummary,
  type Me,
  type ProjectInfo,
} from "@chelabase/management-client";
import { Button } from "@/components/ui/button";
import { errorText } from "@/lib/errors";
import type { ConnectionFocus } from "@/lib/project-search";
import { pollWhile, TRANSITIONAL } from "@/lib/query";
import { CreateDatabaseForm } from "./create-database-form";
import { DatabaseActions } from "./database-actions";
import { DatabaseBranches } from "./database-branches";
import { DatabaseDataApi } from "./database-data-api";
import { MergesList } from "./merges/merges-list";
import { ProjectSettings } from "./project-settings";
import { TablesView } from "./tables-view";
import { UsersTab } from "./users-tab";

// The editor, CodeMirror and the pgls worker load only when a SQL view opens.
const SqlEditor = lazy(() => import("./sql-editor/sql-editor").then((m) => ({ default: m.SqlEditor })));

export type ProjectTab = "databases" | "users" | "settings";

export interface ProjectPageProps {
  projectId: string;
  /** The gateway's base URL (shown as the data API endpoint). */
  apiUrl: string;
  /** Called on the first successful load (and again after a failed one recovers): re-arms the re-auth guard. */
  onLoaded?: () => void;
  tab: ProjectTab;
  onTabChange: (tab: ProjectTab) => void;
  /** The project doesn't exist (any more): go back to the dashboard. */
  onGone: () => void;
  /** A branch's connection details to open on load (from the URL; a merge's created roles link here). */
  focus?: ConnectionFocus | null;
}

export function ProjectPage({ projectId, apiUrl, onLoaded, tab, onTabChange, onGone, focus = null }: ProjectPageProps) {
  // The project itself (status, error, app users), followed while it is being deleted.
  const project = useGetProject(projectId, {
    query: { refetchInterval: pollWhile<ProjectInfo>((p) => p.status === "deleting") },
  });
  // The tenant's workspace carries the project's databases; kept fresh while any is changing.
  const me = useGetMe({
    query: {
      refetchInterval: pollWhile<Me>((m) =>
        (m.tenant?.projects ?? []).some((p) => p.databases.some((d) => TRANSITIONAL.has(d.status))),
      ),
    },
  });

  const gone = project.error instanceof ApiError && project.error.status === 404;
  useEffect(() => {
    if (gone) onGone();
  }, [gone]);

  const loaded = project.isSuccess && me.isSuccess;
  useEffect(() => {
    if (loaded) onLoaded?.();
  }, [loaded]);

  const tenant = me.data?.tenant ?? null;
  const databases: DatabaseSummary[] = tenant?.projects.find((p) => p.id === projectId)?.databases ?? [];
  const usersEnabled = project.data?.app_users === "enabled";
  const failure = project.data === undefined ? project.error : null;
  const restarting = project.error?.signInRestarted === true || me.error?.signInRestarted === true;

  return (
    <div className="mx-auto max-w-4xl space-y-8 p-8">
      {restarting ? (
        <p className="text-muted-foreground">Your session expired. Signing you in again…</p>
      ) : gone ? (
        <p className="text-muted-foreground">This project no longer exists.</p>
      ) : project.data === undefined ? (
        failure ? (
          <div className="space-y-3">
            <p role="alert" className="text-destructive">
              Couldn't load the project: {errorText(failure)}
            </p>
            <Button onClick={() => void Promise.all([project.refetch(), me.refetch()])}>Retry</Button>
          </div>
        ) : (
          <p className="text-muted-foreground">Loading the project…</p>
        )
      ) : (
        <>
          <p>
            <Link to="/app" className="text-sm text-muted-foreground underline-offset-4 hover:underline">
              All projects
            </Link>
          </p>
          <div>
            <h1 className="text-2xl font-semibold">{project.data.name}</h1>
            <p className="text-sm text-muted-foreground">
              <code>{project.data.slug}</code>
            </p>
          </div>
          {project.data.status === "deleting" && (
            <div role="alert" className="space-y-1 rounded-lg border border-destructive/40 p-3 text-sm">
              <p className="font-medium text-destructive">This project is being deleted.</p>
              {project.data.error && <p>{project.data.error}</p>}
            </div>
          )}
          <div role="tablist" className="flex gap-1 border-b">
            <TabButton active={tab === "databases"} onClick={() => onTabChange("databases")}>
              Databases
            </TabButton>
            {usersEnabled && (
              <TabButton active={tab === "users"} onClick={() => onTabChange("users")}>
                Users
              </TabButton>
            )}
            <TabButton active={tab === "settings"} onClick={() => onTabChange("settings")}>
              Settings
            </TabButton>
          </div>
          {/* A deleting project refuses every change: disabled fieldsets switch all actions off. */}
          {tab === "databases" ? (
            <fieldset
              disabled={project.data.status === "deleting"}
              className="m-0 min-w-0 space-y-6 border-0 p-0"
            >
              {tenant ? (
                <CreateDatabaseForm tenantId={tenant.id} projectId={projectId} />
              ) : me.error ? (
                <p role="alert" className="text-destructive">
                  Couldn't load your databases: {errorText(me.error)}
                </p>
              ) : null}
              <DatabasesTable projectId={projectId} databases={databases} apiUrl={apiUrl} focus={focus} />
            </fieldset>
          ) : tab === "users" && usersEnabled ? (
            <fieldset disabled={project.data.status === "deleting"} className="m-0 min-w-0 border-0 p-0">
              <UsersTab projectId={projectId} />
            </fieldset>
          ) : (
            <ProjectSettings
              project={project.data}
              databases={tenant ? databases : undefined}
              databasesError={me.error} onDeleted={() => {}} />
          )}
        </>
      )}
    </div>
  );
}

function TabButton({ active, onClick, children }: { active: boolean; onClick: () => void; children: ReactNode }) {
  return (
    <button
      type="button"
      role="tab"
      aria-selected={active}
      onClick={onClick}
      className={`-mb-px border-b-2 px-3 py-2 text-sm font-medium ${
        active ? "border-primary text-foreground" : "border-transparent text-muted-foreground hover:text-foreground"
      }`}
    >
      {children}
    </button>
  );
}

type DatabaseView = "branches" | "merges" | "data-api" | "tables" | "sql";

function DatabasesTable({
  projectId,
  databases,
  apiUrl,
  focus,
}: {
  projectId: string;
  databases: DatabaseSummary[];
  apiUrl: string;
  focus: ConnectionFocus | null;
}) {
  // A focused branch opens its database's branches; read once, on the first render.
  const [open, setOpen] = useState<{ id: string; view: DatabaseView } | null>(() =>
    focus ? { id: focus.databaseId, view: "branches" } : null,
  );
  const toggle = (id: string, view: DatabaseView) =>
    setOpen(open?.id === id && open.view === view ? null : { id, view });
  if (databases.length === 0) {
    return <p className="text-sm text-muted-foreground">No databases yet.</p>;
  }
  return (
    <table className="w-full text-left text-sm">
      <thead className="border-b text-muted-foreground">
        <tr>
          <th className="py-2 font-medium">Name</th>
          <th className="py-2 font-medium">Status</th>
          <th className="py-2 font-medium">Endpoint</th>
          <th className="py-2 font-medium">Actions</th>
        </tr>
      </thead>
      <tbody>
        {databases.map((db) => (
          <Fragment key={db.database_id}>
            <tr className="border-b last:border-0">
              <td className="py-2">{db.name}</td>
              <td className="py-2">{db.status}</td>
              <td className="py-2 font-mono">{db.host && db.port ? `${db.host}:${db.port}` : "—"}</td>
              <td className="py-2">
                <div className="flex flex-wrap items-start gap-2">
                  <DatabaseActions database={db} />
                  <Button
                    size="sm"
                    variant="outline"
                    disabled={db.status !== "ready"}
                    aria-expanded={open?.id === db.database_id && open.view === "branches"}
                    onClick={() => toggle(db.database_id, "branches")}
                  >
                    Branches
                  </Button>
                  <Button
                    size="sm"
                    variant="outline"
                    disabled={db.status !== "ready"}
                    aria-expanded={open?.id === db.database_id && open.view === "merges"}
                    onClick={() => toggle(db.database_id, "merges")}
                  >
                    Merges
                  </Button>
                  <Button
                    size="sm"
                    variant="outline"
                    disabled={db.status !== "ready"}
                    aria-expanded={open?.id === db.database_id && open.view === "data-api"}
                    onClick={() => toggle(db.database_id, "data-api")}
                  >
                    Data API
                  </Button>
                  <Button
                    size="sm"
                    variant="outline"
                    disabled={db.status !== "ready"}
                    aria-expanded={open?.id === db.database_id && open.view === "tables"}
                    onClick={() => toggle(db.database_id, "tables")}
                  >
                    Tables
                  </Button>
                  <Button
                    size="sm"
                    variant="outline"
                    disabled={db.status !== "ready"}
                    aria-expanded={open?.id === db.database_id && open.view === "sql"}
                    onClick={() => toggle(db.database_id, "sql")}
                  >
                    SQL
                  </Button>
                </div>
              </td>
            </tr>
            {open?.id === db.database_id && db.status === "ready" && (
              <tr className="border-b last:border-0">
                <td colSpan={4}>
                  {open.view === "branches" ? (
                    <DatabaseBranches
                      projectId={projectId}
                      database={db}
                      focus={focus?.databaseId === db.database_id ? focus : null}
                    />
                  ) : open.view === "merges" ? (
                    <MergesList projectId={projectId} databaseId={db.database_id} />
                  ) : open.view === "tables" ? (
                    <TablesView databaseId={db.database_id} />
                  ) : open.view === "sql" ? (
                    <Suspense fallback={<p className="py-3 text-sm text-muted-foreground">Loading the editor…</p>}>
                      <SqlEditor databaseId={db.database_id} />
                    </Suspense>
                  ) : (
                    <DatabaseDataApi apiUrl={apiUrl} database={db} />
                  )}
                </td>
              </tr>
            )}
          </Fragment>
        ))}
      </tbody>
    </table>
  );
}
