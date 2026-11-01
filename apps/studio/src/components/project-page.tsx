import { Fragment, useEffect, useRef, useState } from "react";
import { useOverflowFocusable } from "@/lib/use-overflow-focusable";
import { Link } from "@tanstack/react-router";
import {
  ApiError,
  useGetMe,
  useGetProject,
  type DatabaseSummary,
  type MeResponse as Me,
  type ProjectInfo,
} from "@chelabase/management-client";
import { TABLES_ROUTE } from "@/components/app-shell/rail-items";
import { useCrumbLabel } from "@/components/app-shell/crumbs";
import { Button, buttonVariants } from "@/components/ui/button";
import { Tabs } from "@/components/ui/tabs";
import { ProjectCapsContext, noAccess, projectCaps } from "@/lib/capabilities";
import { errorText } from "@/lib/errors";
import type { ConnectionFocus } from "@/lib/project-search";
import { pollWhile, TRANSITIONAL } from "@/lib/query";
import { CreateDatabaseForm } from "./create-database-form";
import { DatabaseActions } from "./database-actions";
import { DatabaseBranches } from "./database-branches";
import { DatabaseDataApi } from "./database-data-api";

import { NO_ACCESS_POLL_MS, NoAccess } from "./no-access";
import { MergesList } from "./merges/merges-list";
import { AccessTab } from "./project-access/access-tab";
import { ProjectSettings } from "./project-settings";
import { UsersTab } from "./users-tab";

const pollDatabases = pollWhile<Me>((m) =>
  (m.tenant?.projects ?? []).some((p) => p.databases.some((d) => TRANSITIONAL.has(d.status))),
);

export type ProjectTab = "databases" | "users" | "access" | "settings";

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
      // Without any project, the no-access screen checks again every 30 s (as it says).
      refetchInterval: (q) => (q.state.data && noAccess(q.state.data) ? NO_ACCESS_POLL_MS : pollDatabases(q)),
    },
  });

  const gone = project.error instanceof ApiError && project.error.status === 404;
  useEffect(() => {
    if (gone) onGone();
  }, [gone]);

  // Access arrived while the no-access screen showed: the project's earlier 403 is stale.
  const waiting = me.data !== undefined && noAccess(me.data);
  const wasWaiting = useRef(false);
  useEffect(() => {
    if (wasWaiting.current && !waiting) void project.refetch();
    wasWaiting.current = waiting;
  }, [waiting]);

  const loaded = project.isSuccess && me.isSuccess;
  useEffect(() => {
    if (loaded) onLoaded?.();
  }, [loaded]);

  const tenant = me.data?.tenant ?? null;
  const databases: DatabaseSummary[] = tenant?.projects.find((p) => p.id === projectId)?.databases ?? [];
  const tenantProject = tenant?.projects.find((p) => p.id === projectId);
  const usersEnabled = project.data?.app_users === "enabled";
  const failure = project.data === undefined ? project.error : null;
  const restarting = project.error?.signInRestarted === true || me.error?.signInRestarted === true;
  const caps = projectCaps(me.data, projectId);
  useCrumbLabel("projectId", projectId, project.data?.name);

  // A member with no project at all sees the no-access screen here too, not a 403 (Q9).
  if (me.data && noAccess(me.data)) {
    return (
      <div className="mx-auto max-w-4xl space-y-8 p-4 sm:p-8">
        <NoAccess me={me.data} checking={me.isFetching} onCheck={() => void me.refetch()} />
      </div>
    );
  }

  const tabs = [
    { id: "databases" as const, label: "Databases" },
    ...(usersEnabled ? [{ id: "users" as const, label: "Users" }] : []),
    { id: "access" as const, label: "Access" },
    { id: "settings" as const, label: "Settings" },
  ];

  return (
    <ProjectCapsContext.Provider value={caps}>
    <div className="mx-auto max-w-4xl space-y-8 p-4 sm:p-8">
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
            <div role="alert" className="space-y-1 border border-destructive/40 p-3 text-sm">
              <p className="font-medium text-destructive">This project is being deleted.</p>
              {project.data.error && <p>{project.data.error}</p>}
            </div>
          )}
          <Tabs label="Project sections" items={tabs} value={tabs.some((t) => t.id === tab) ? tab : "databases"} onValueChange={onTabChange}>
          {/* A deleting project refuses every change: disabled fieldsets switch all actions off. */}
          {tab === "databases" ? (
            <fieldset
              disabled={project.data.status === "deleting"}
              className="m-0 min-w-0 space-y-6 border-0 p-0"
            >
              {caps.atLeast("admin") && <CreateDatabaseForm projectId={projectId} />}
              {me.error ? (
                <p role="alert" className="text-destructive">
                  Couldn't load your databases: {errorText(me.error)}
                </p>
              ) : null}
              <DatabasesTable projectId={projectId} databases={databases} apiUrl={apiUrl} focus={focus} me={me.data} />
            </fieldset>
          ) : tab === "users" && usersEnabled ? (
            <fieldset disabled={project.data.status === "deleting"} className="m-0 min-w-0 border-0 p-0">
              <UsersTab projectId={projectId} />
            </fieldset>
          ) : tab === "access" ? (
            <fieldset disabled={project.data.status === "deleting"} className="m-0 min-w-0 border-0 p-0">
              {tenantProject ? (
                <AccessTab project={tenantProject} />
              ) : me.data ? (
                <p className="text-sm text-chalk-dim">
                  {project.data.name} isn't in your workspace, or you can't see it. Go back to All projects and open it from there.
                </p>
              ) : (
                <p className="text-sm text-chalk-dim">Loading access…</p>
              )}
            </fieldset>
          ) : (
            <ProjectSettings
              project={project.data}
              databases={tenant ? databases : undefined}
              databasesError={me.error} onDeleted={() => {}} />
          )}
          </Tabs>
        </>
      )}
    </div>
    </ProjectCapsContext.Provider>
  );
}

type DatabaseView = "branches" | "merges" | "data-api";

function DatabasesTable({
  projectId,
  databases,
  apiUrl,
  focus,
  me,
}: {
  projectId: string;
  databases: DatabaseSummary[];
  apiUrl: string;
  focus: ConnectionFocus | null;
  /** `/v1/me`: where a grant is narrowed, a database's own caps apply (I3). */
  me: Me | undefined;
}) {
  // A focused branch opens its database's branches; read once, on the first render.
  const [open, setOpen] = useState<{ id: string; view: DatabaseView } | null>(() =>
    focus ? { id: focus.databaseId, view: "branches" } : null,
  );
  const toggle = (id: string, view: DatabaseView) =>
    setOpen(open?.id === id && open.view === view ? null : { id, view });
  const scroll = useOverflowFocusable();
  if (databases.length === 0) {
    return <p className="text-sm text-muted-foreground">No databases yet.</p>;
  }
  return (
    <div ref={scroll.ref} tabIndex={scroll.tabIndex} className="relative max-w-full overflow-x-auto [container-type:inline-size]" role="region" aria-label="Databases table">
    <table className="w-full text-left text-sm">
      <thead className="border-b text-muted-foreground @max-[44rem]:hidden">
        <tr>
          <th className="py-2 pr-4 font-medium">Name</th>
          <th className="py-2 pr-4 font-medium">Status</th>
          <th className="py-2 pr-4 font-medium">Endpoint</th>
          <th className="py-2 font-medium">Actions</th>
        </tr>
      </thead>
      <tbody>
        {databases.map((db) => {
          // Each database is shown with what the caller may do there: the project's caps, or the
          // narrower ones of an override on that database.
          const dbCaps = projectCaps(me, projectId, db.database_id);
          return (
          <ProjectCapsContext.Provider key={db.database_id} value={dbCaps}>
          <Fragment>
            <tr className="border-b align-top last:border-0 @max-[44rem]:block @max-[44rem]:py-2">
              <td className="max-w-[14rem] py-2 pr-4 @max-[44rem]:block @max-[44rem]:max-w-none @max-[44rem]:py-0.5 @max-[44rem]:pr-0"><span className="block truncate" title={db.name}>{db.name}</span></td>
              <td className="py-2 pr-4 @max-[44rem]:block @max-[44rem]:py-0.5 @max-[44rem]:pr-0">{db.status}</td>
              <td className="py-2 pr-4 font-mono @max-[44rem]:block @max-[44rem]:py-0.5 @max-[44rem]:pr-0">{db.host && db.port ? `${db.host}:${db.port}` : "—"}</td>
              <td className="py-2 @max-[44rem]:block @max-[44rem]:pb-0 @max-[44rem]:pt-1.5">
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
                  {db.status === "ready" ? (
                    <Link
                      to={TABLES_ROUTE}
                      params={{ projectId, databaseId: db.database_id }}
                      className={buttonVariants({ size: "sm", variant: "outline" })}
                    >
                      Tables
                    </Link>
                  ) : (
                    <Button size="sm" variant="outline" disabled>
                      Tables
                    </Button>
                  )}
                </div>
              </td>
            </tr>
            {open?.id === db.database_id && db.status === "ready" && (
              <tr className="border-b last:border-0">
                <td colSpan={4} className="w-full max-w-0 @max-[44rem]:block @max-[44rem]:max-w-none">
                  <div className="sticky left-0 w-[100cqw]">
                  {open.view === "branches" ? (
                    <DatabaseBranches
                      projectId={projectId}
                      database={db}
                      focus={focus?.databaseId === db.database_id ? focus : null}
                    />
                  ) : open.view === "merges" ? (
                    <MergesList projectId={projectId} databaseId={db.database_id} />
                  ) : (
                    <DatabaseDataApi apiUrl={apiUrl} database={db} />
                  )}
                  </div>
                </td>
              </tr>
            )}
          </Fragment>
          </ProjectCapsContext.Provider>
          );
        })}
      </tbody>
    </table>
    </div>
  );
}
