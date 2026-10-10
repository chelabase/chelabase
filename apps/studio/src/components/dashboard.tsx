import { useEffect, useRef, useState } from "react";
import { Link, useNavigate } from "@tanstack/react-router";
import { useGetMe, type MeResponse as Me, type Project } from "@chelabase/management-client";
import { Button } from "@/components/ui/button";
import { errorText } from "@/lib/errors";
import { pollWhile, TRANSITIONAL } from "@/lib/query";
import { NewProjectDialog } from "./new-project-dialog";
import { NO_ACCESS_POLL_MS, NoAccess } from "./no-access";
import { noAccess } from "@/lib/capabilities";

export interface DashboardProps {
  /** Called on the first successful `getMe` (and again after a failed one recovers): re-arms the re-auth guard. */
  onLoaded?: () => void;
}

export function Dashboard({ onLoaded }: DashboardProps) {
  // While anything is being created or deleted, keep the list fresh.
  // While the caller has no project, check every 30 s so access shows up without a reload.
  const me = useGetMe({ query: { refetchInterval: (q) => (q.state.data && noAccess(q.state.data) ? NO_ACCESS_POLL_MS : pollTransitional(q)) } });
  const loaded = me.isSuccess;
  // "You now have access to …" once, when the no-access screen gives way.
  const [arrived, setArrived] = useState<string | null>(null);
  const wasWaiting = useRef(false);
  useEffect(() => {
    if (!me.data) return;
    const waiting = noAccess(me.data);
    if (wasWaiting.current && !waiting) setArrived(me.data.tenant?.projects[0]?.name ?? null);
    wasWaiting.current = waiting;
  }, [me.data]);
  // Once per transition into success (not per poll): re-arm the re-auth guard.
  useEffect(() => {
    if (loaded) onLoaded?.();
  }, [loaded]);

  const restarting = me.error?.signInRestarted === true;

  return (
    <div className="mx-auto max-w-4xl space-y-8 p-8">
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
          {arrived && (
            <p role="status" className="text-sm text-chalk">
              You now have access to {arrived}.
            </p>
          )}
          {me.data.member?.status === "removed" ? (
            <p role="alert" className="text-sm text-coral">
              You're no longer a member of this tenant.
            </p>
          ) : noAccess(me.data) ? (
            <NoAccess me={me.data} checking={me.isFetching} onCheck={() => void me.refetch()} />
          ) : (
            <Workspace me={me.data} />
          )}
        </>
      )}
    </div>
  );
}

const pollTransitional = pollWhile<Me>(hasTransitional);

function Workspace({ me }: { me: Me }) {
  const navigate = useNavigate();
  const { tenant } = me;
  const tenantAdmin = me.member?.tenant_admin === true;
  if (tenant === null) {
    return (
      <section className="space-y-2">
        <h1 className="text-2xl font-semibold">Your organization isn't set up yet</h1>
        <p className="text-muted-foreground">
          You're signed in (org <code>{me.user.org_id}</code>), but no Chelabase tenant is bound to that
          organization. In this self-hosted setup, run <code>just zitadel-bootstrap</code>.
        </p>
      </section>
    );
  }
  return (
    <section className="space-y-6">
      <div className="flex items-center justify-between">
        <h1 className="text-2xl font-semibold">{tenant.slug}</h1>
        {tenantAdmin && (
          <NewProjectDialog
            onCreated={(project) => void navigate({ to: "/app/projects/$projectId", params: { projectId: project.id } })}
          />
        )}
      </div>
      {tenant.projects.length === 0 ? (
        <p className="text-muted-foreground">This tenant has no projects yet.</p>
      ) : (
        <table className="w-full text-left text-sm">
          <thead className="border-b text-muted-foreground">
            <tr>
              <th className="py-2 font-medium">Name</th>
              <th className="py-2 font-medium">Slug</th>
              <th className="py-2 font-medium">Status</th>
              <th className="py-2 font-medium">Databases</th>
            </tr>
          </thead>
          <tbody>
            {tenant.projects.map((project) => (
              <tr key={project.id} className="border-b last:border-0">
                <td className="py-2">
                  <Link
                    to="/app/projects/$projectId"
                    params={{ projectId: project.id }}
                    className="font-medium underline-offset-4 hover:underline"
                  >
                    {project.name}
                  </Link>
                </td>
                <td className="py-2 font-mono">{project.slug}</td>
                <td className="py-2">{project.status}</td>
                <td className="py-2">{count(project.databases.length)}</td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </section>
  );
}

const count = (n: number) => `${n} ${n === 1 ? "database" : "databases"}`;

function hasTransitional(me: Me): boolean {
  return (me.tenant?.projects ?? []).some(
    (p: Project) => p.status === "deleting" || p.databases.some((d) => TRANSITIONAL.has(d.status)),
  );
}
