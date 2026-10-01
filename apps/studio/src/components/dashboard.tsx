import { Fragment, useEffect, useState, type ReactNode } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { useGetMe, type DatabaseSummary, type Me } from "@cheladb/management-client";
import { Button } from "@/components/ui/button";
import { errorText } from "@/lib/errors";
import { pollWhile, TRANSITIONAL } from "@/lib/query";
import { CreateDatabaseForm } from "./create-database-form";
import { DatabaseActions } from "./database-actions";
import { DatabaseBranches } from "./database-branches";
import { DatabaseDataApi } from "./database-data-api";

export interface DashboardProps {
  /** The gateway's base URL (shown as the data API endpoint). */
  apiUrl: string;
  profile: { name?: string; email?: string };
  onSignOut: () => void;
  /** Called on the first successful `getMe` (and again after a failed one recovers): re-arms the re-auth guard. */
  onLoaded?: () => void;
  /** Extra header links, shown before "Sign out". */
  nav?: ReactNode;
}

export function Dashboard({ apiUrl, profile, onSignOut, onLoaded, nav }: DashboardProps) {
  const queryClient = useQueryClient();
  // While anything is being created or deleted, keep the list fresh.
  const me = useGetMe({ query: { refetchInterval: pollWhile<Me>(hasTransitional) } });
  const loaded = me.isSuccess;
  // Once per transition into success (not per poll): re-arm the re-auth guard.
  useEffect(() => {
    if (loaded) onLoaded?.();
  }, [loaded]);

  const restarting = me.error?.signInRestarted === true;

  return (
    <div className="min-h-screen bg-background text-foreground">
      <header className="flex items-center justify-between border-b px-8 py-4">
        <span className="font-semibold">Cheladb Studio</span>
        <div className="flex items-center gap-4 text-sm">
          <span className="flex flex-col items-end leading-tight">
            {profile.name && <span>{profile.name}</span>}
            {profile.email && <span className="text-muted-foreground">{profile.email}</span>}
          </span>
          {nav}
          <Button
            variant="outline"
            size="sm"
            onClick={() => {
              // Nothing of this session may outlive it in the cache.
              queryClient.clear();
              onSignOut();
            }}
          >
            Sign out
          </Button>
        </div>
      </header>
      <main className="mx-auto max-w-4xl space-y-8 p-8">
        {restarting ? (
          <p className="text-muted-foreground">Your session expired. Signing you in again…</p>
        ) : me.data === undefined ? (
          me.error ? (
            <div className="space-y-3">
              <p role="alert" className="text-destructive">
                Couldn't load your workspace: {errorText(me.error)}
              </p>
              <Button onClick={() => void me.refetch()}>Retry</Button>
            </div>
          ) : (
            <p className="text-muted-foreground">Loading your workspace…</p>
          )
        ) : (
          <>
            {/* A failed *refresh* keeps showing the last good workspace; polling continues. */}
            {me.error && (
              <p className="text-sm text-muted-foreground">Couldn't refresh ({errorText(me.error)}); retrying…</p>
            )}
            <Workspace me={me.data} apiUrl={apiUrl} />
          </>
        )}
      </main>
    </div>
  );
}

function Workspace({ me, apiUrl }: { me: Me; apiUrl: string }) {
  const { tenant } = me;
  if (tenant === null) {
    return (
      <section className="space-y-2">
        <h1 className="text-2xl font-semibold">Your organization isn't set up yet</h1>
        <p className="text-muted-foreground">
          You're signed in (org <code>{me.user.org_id}</code>), but no Cheladb tenant is bound to that
          organization. In this self-hosted setup, run <code>just zitadel-bootstrap</code>.
        </p>
      </section>
    );
  }
  if (tenant.projects.length === 0) {
    return (
      <section className="space-y-2">
        <h1 className="text-2xl font-semibold">{tenant.slug}</h1>
        <p className="text-muted-foreground">This tenant has no projects yet.</p>
      </section>
    );
  }
  return (
    <>
      {tenant.projects.map((project) => (
        <section key={project.id} className="space-y-6">
          <div>
            <h1 className="text-2xl font-semibold">
              {tenant.slug} / {project.slug}
            </h1>
            <dl className="mt-2 grid grid-cols-[auto_1fr] gap-x-4 gap-y-1 text-sm text-muted-foreground">
              <dt>Tenant</dt>
              <dd>
                <code>{tenant.id}</code>
              </dd>
              <dt>Project</dt>
              <dd>
                <code>{project.id}</code>
              </dd>
              <dt>Organization</dt>
              <dd>
                <code>{me.user.org_id}</code>
              </dd>
            </dl>
          </div>
          <CreateDatabaseForm tenantId={tenant.id} projectId={project.id} />
          <DatabasesTable databases={project.databases} apiUrl={apiUrl} />
        </section>
      ))}
    </>
  );
}

function hasTransitional(me: Me): boolean {
  return (me.tenant?.projects ?? []).some((p) => p.databases.some((d) => TRANSITIONAL.has(d.status)));
}

function DatabasesTable({ databases, apiUrl }: { databases: DatabaseSummary[]; apiUrl: string }) {
  const [open, setOpen] = useState<{ id: string; view: "branches" | "data-api" } | null>(null);
  const toggle = (id: string, view: "branches" | "data-api") =>
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
                    aria-expanded={open?.id === db.database_id && open.view === "data-api"}
                    onClick={() => toggle(db.database_id, "data-api")}
                  >
                    Data API
                  </Button>
                </div>
              </td>
            </tr>
            {open?.id === db.database_id && db.status === "ready" && (
              <tr className="border-b last:border-0">
                <td colSpan={4}>
                  {open.view === "branches" ? (
                    <DatabaseBranches database={db} />
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
