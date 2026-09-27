import { useCallback, useEffect, useState } from "react";
import type { DatabaseSummary, MeResponse } from "@cheladb/types";
import { Button } from "@/components/ui/button";
import { ApiError, type ApiClient } from "@/lib/api";
import { CreateDatabaseForm } from "./create-database-form";

type MeState =
  | { status: "loading" }
  | { status: "error"; message: string }
  | { status: "restarting" }
  | { status: "ready"; me: MeResponse };

export interface DashboardProps {
  api: ApiClient;
  profile: { name?: string; email?: string };
  onSignOut: () => void;
  pollIntervalMs?: number;
}

export function Dashboard({ api, profile, onSignOut, pollIntervalMs }: DashboardProps) {
  const [state, setState] = useState<MeState>({ status: "loading" });

  const load = useCallback(async () => {
    try {
      setState({ status: "ready", me: await api.me() });
    } catch (err) {
      if (err instanceof ApiError && err.signInRestarted) {
        setState({ status: "restarting" });
        return;
      }
      setState({ status: "error", message: err instanceof Error ? err.message : String(err) });
    }
  }, [api]);

  useEffect(() => {
    void load();
  }, [load]);

  return (
    <div className="min-h-screen bg-background text-foreground">
      <header className="flex items-center justify-between border-b px-8 py-4">
        <span className="font-semibold">Cheladb Studio</span>
        <div className="flex items-center gap-4 text-sm">
          <span className="flex flex-col items-end leading-tight">
            {profile.name && <span>{profile.name}</span>}
            {profile.email && <span className="text-muted-foreground">{profile.email}</span>}
          </span>
          <Button variant="outline" size="sm" onClick={onSignOut}>
            Sign out
          </Button>
        </div>
      </header>
      <main className="mx-auto max-w-4xl space-y-8 p-8">
        {state.status === "loading" && (
          <p className="text-muted-foreground">Loading your workspace…</p>
        )}
        {state.status === "restarting" && (
          <p className="text-muted-foreground">Your session expired. Signing you in again…</p>
        )}
        {state.status === "error" && (
          <div className="space-y-3">
            <p role="alert" className="text-destructive">
              Couldn't load your workspace: {state.message}
            </p>
            <Button onClick={() => void load()}>Retry</Button>
          </div>
        )}
        {state.status === "ready" && <Workspace me={state.me} api={api} reload={load} pollIntervalMs={pollIntervalMs} />}
      </main>
    </div>
  );
}

function Workspace({
  me,
  api,
  reload,
  pollIntervalMs,
}: {
  me: MeResponse;
  api: ApiClient;
  reload: () => Promise<void>;
  pollIntervalMs?: number;
}) {
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
          <CreateDatabaseForm
            api={api}
            tenantId={tenant.id}
            projectId={project.id}
            onSettled={() => void reload()}
            pollIntervalMs={pollIntervalMs}
          />
          <DatabasesTable databases={project.databases} />
        </section>
      ))}
    </>
  );
}

function DatabasesTable({ databases }: { databases: DatabaseSummary[] }) {
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
        </tr>
      </thead>
      <tbody>
        {databases.map((db) => (
          <tr key={db.database_id} className="border-b last:border-0">
            <td className="py-2">{db.name}</td>
            <td className="py-2">{db.status}</td>
            <td className="py-2 font-mono">{db.host && db.port ? `${db.host}:${db.port}` : "—"}</td>
          </tr>
        ))}
      </tbody>
    </table>
  );
}
