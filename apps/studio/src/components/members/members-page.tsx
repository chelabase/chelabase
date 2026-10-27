import { useEffect, useMemo, useState } from "react";
import {
  ApiError,
  listAccessEvents,
  useGetInstanceSettings,
  useGetMe,
  useListMembers,
} from "@chelabase/management-client";
import { Button } from "@/components/ui/button";
import { Tabs } from "@/components/ui/tabs";
import { errorText } from "@/lib/errors";
import { memberName } from "@/lib/members";
import { parseMembersTab, type MembersTab } from "@/lib/members-search";
import { AccessEventList } from "./access-event-list";
import { InstanceTab } from "./instance-tab";
import { ACCESS_EVENTS_KEY } from "./keys";
import { PeopleTab } from "./people-tab";
import { RolesTab } from "./roles/roles-tab";
import { TeamsTab } from "./teams";
import { TenantName } from "./tenant-name";

export { parseMembersTab, type MembersTab };

export interface MembersPageProps {
  tab: MembersTab;
  onTabChange: (tab: MembersTab) => void;
  /** Shown when the caller was removed meanwhile. */
  onSignOut?: () => void;
}

const PAGE = 50;
/** How long the page-level status stays up. */
const STATUS_MS = 8000;

/** `/app/members`: everyone in the tenant (People), the access log (Activity) and the instance's switch. */
export function MembersPage({ tab, onTabChange, onSignOut }: MembersPageProps) {
  const me = useGetMe();
  const members = useListMembers();
  const tenantAdmin = me.data?.member?.tenant_admin === true;
  const instanceAdmin = me.data?.instance_admin === true;
  const settings = useGetInstanceSettings({ query: { enabled: instanceAdmin } });
  const [status, setStatus] = useState<string | null>(null);
  // What was just done is said for a few seconds, in a layer over the page: it never moves the tabs.
  useEffect(() => {
    if (status === null) return;
    const t = setTimeout(() => setStatus(null), STATUS_MS);
    return () => clearTimeout(t);
  }, [status]);

  const tabs = [
    { id: "people" as const, label: "People" },
    // Roles: anyone with a project grant (a tenant admin is Admin on every project).
    ...(tenantAdmin || (me.data?.projects?.length ?? 0) > 0 ? [{ id: "roles" as const, label: "Roles" }] : []),
    // Teams: everyone reads; tenant admins edit.
    { id: "teams" as const, label: "Teams" },
    ...(tenantAdmin ? [{ id: "activity" as const, label: "Activity" }] : []),
    // Hidden, not disabled, when the instance can't switch it (platform mode included).
    ...(instanceAdmin && settings.data?.open_signup_switchable ? [{ id: "instance" as const, label: "Instance" }] : []),
  ];
  const current = tabs.some((t) => t.id === tab) ? tab : "people";

  const list = members.data ?? [];

  const selfId = me.data?.member?.id ?? null;
  const projects = me.data?.tenant?.projects ?? [];
  const context = useMemo(() => {
    const names = new Map(list.map((m) => [m.id, memberName(m)]));
    const dbs = new Map(projects.flatMap((p) => p.databases).map((d) => [d.database_id, d.name]));
    const projectNames = new Map(projects.map((p) => [p.id, p.name]));
    return { names, databaseName: (id: string) => dbs.get(id) ?? null, projectName: (id: string) => projectNames.get(id) ?? null };
  }, [list, projects]);

  const removed = members.error instanceof ApiError && members.error.status === 404;

  return (
    <div className="mx-auto max-w-[56rem] space-y-6 p-8">
      <div className="space-y-1">
        <h1 className="text-2xl font-semibold">Members</h1>
        {me.data?.tenant && (
          <TenantName tenant={me.data.tenant} editable={tenantAdmin} onStatus={setStatus} />
        )}
      </div>
      {status && (
        <p role="status" className="fixed right-6 bottom-6 z-40 max-w-sm border border-line-strong bg-deep px-4 py-3 text-sm text-chalk shadow-lg">
          {status}
        </p>
      )}
      {removed ? (
        <div className="space-y-3">
          <p role="alert" className="text-sm text-coral">
            You're no longer a member of this tenant.
          </p>
          {onSignOut && (
            <Button variant="outline" onClick={onSignOut}>
              Sign out
            </Button>
          )}
        </div>
      ) : members.data === undefined || me.data === undefined ? (
        members.error || me.error ? (
          <div className="space-y-3">
            <p role="alert" className="text-sm text-coral">
              Couldn't load members: {errorText(members.error ?? me.error)}
            </p>
            <Button variant="outline" onClick={() => void Promise.all([members.refetch(), me.refetch()])}>
              Retry
            </Button>
          </div>
        ) : (
          <p className="text-sm text-chalk-dim">Loading members…</p>
        )
      ) : (
        <Tabs
          label="Members sections"
          items={tabs}
          value={current}
          onValueChange={(next) => {
            // A line about the last section's action is stale in another one.
            setStatus(null);
            onTabChange(next);
          }}
        >
          {current === "people" ? (
            <PeopleTab members={list} selfId={selfId} tenantAdmin={tenantAdmin} projects={projects} onStatus={setStatus} />
          ) : current === "roles" ? (
            <RolesTab />
          ) : current === "teams" ? (
            <TeamsTab tenantAdmin={tenantAdmin} projects={projects} onStatus={setStatus} />
          ) : current === "activity" ? (
            <AccessEventList
              queryKey={ACCESS_EVENTS_KEY}
              fetchPage={(before) => listAccessEvents(before === undefined ? { limit: PAGE } : { before, limit: PAGE })}
              context={context}
              emptyText="No access changes yet. Invites, removals and changes to anyone's access will show here."
            />
          ) : settings.data ? (
            <InstanceTab settings={settings.data} />
          ) : null}
        </Tabs>
      )}
    </div>
  );
}
