import { useEffect, type ReactNode } from "react";
import { Link, useNavigate } from "@tanstack/react-router";
import { useGetMe, type Me, type Project } from "@cheladb/management-client";
import { Button } from "@/components/ui/button";
import { errorText } from "@/lib/errors";
import { pollWhile, TRANSITIONAL } from "@/lib/query";
import { NewProjectDialog } from "./new-project-dialog";
import { StudioShell } from "./studio-shell";

export interface DashboardProps {
  profile: { name?: string; email?: string };
  onSignOut: () => void;
  /** Called on the first successful `getMe` (and again after a failed one recovers): re-arms the re-auth guard. */
  onLoaded?: () => void;
  /** Extra header links, shown before "Sign out". */
  nav?: ReactNode;
}

export function Dashboard({ profile, onSignOut, onLoaded, nav }: DashboardProps) {
  // While anything is being created or deleted, keep the list fresh.
  const me = useGetMe({ query: { refetchInterval: pollWhile<Me>(hasTransitional) } });
  const loaded = me.isSuccess;
  // Once per transition into success (not per poll): re-arm the re-auth guard.
  useEffect(() => {
    if (loaded) onLoaded?.();
  }, [loaded]);

  const restarting = me.error?.signInRestarted === true;

  return (
    <StudioShell profile={profile} onSignOut={onSignOut} nav={nav}>
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
          <Workspace me={me.data} />
        </>
      )}
    </StudioShell>
  );
}

function Workspace({ me }: { me: Me }) {
  const navigate = useNavigate();
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
  return (
    <section className="space-y-6">
      <div className="flex items-center justify-between">
        <h1 className="text-2xl font-semibold">{tenant.slug}</h1>
        <NewProjectDialog
          onCreated={(project) => void navigate({ to: "/app/projects/$projectId", params: { projectId: project.id } })}
        />
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
