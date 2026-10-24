import { useState } from "react";
import { Link, useNavigate } from "@tanstack/react-router";
import { useQueryClient } from "@tanstack/react-query";
import { ArrowLeft, ChevronRight } from "lucide-react";
import {
  getListRolesQueryKey,
  useGetMe,
  useListBranchDatabases,
  useListBranches,
  useListMembers,
  useListProjectGrants,
  useListRoles,
  type BranchRole,
} from "@chelabase/management-client";
import { Tabs } from "@/components/ui/tabs";
import { projectCaps } from "@/lib/capabilities";
import { errorText } from "@/lib/errors";
import { utc } from "@/lib/members";
import { escapeName } from "../hub/escape-name";
import { Tag } from "./bits";
import { DangerZone } from "./delete-role";
import { Holders, Memberships } from "./memberships";
import { holdersOf, isManaged } from "./model";
import { PrivilegeEditor } from "./privilege-editor";
import { PrivilegePanel } from "./privilege-panel";
import { usePrivilegeEditor } from "./use-privilege-editor";

export interface RolePageProps {
  databaseId: string;
  branchId: string;
  /** The real role name (not escaped). */
  role: string;
  /** The Postgres database; the branch's main one when absent. */
  pgDatabase?: string;
}

type Section = "privileges" | "members" | "memberships" | "danger";

/** The role's attributes as tags: what it can do at a glance. */
function Attributes({ role }: { role: BranchRole }) {
  return (
    <div className="flex flex-wrap gap-1.5">
      <Tag>{role.login ? "LOGIN" : "NOLOGIN"}</Tag>
      <Tag>{role.inherit ? "INHERIT" : "NOINHERIT"}</Tag>
      <Tag hazard={role.bypass_rls}>{role.bypass_rls ? "BYPASSRLS" : "no BYPASSRLS"}</Tag>
      <Tag>{role.connection_limit >= 0 ? `connection limit ${role.connection_limit}` : "no connection limit"}</Tag>
      {role.valid_until && <Tag>{`valid until ${utc(role.valid_until)}`}</Tag>}
      {role.level === "owner_level" && <Tag hazard>owner-level</Tag>}
    </div>
  );
}

/**
 * `/app/members/roles/$databaseId/$branchId/$role` (spec Part 4): one role on one branch, its
 * privileges in one Postgres database (the staged editor and its right panel), who holds it
 * (Admins only, MH-34), its memberships and its deletion.
 */
export function RolePage({ databaseId, branchId, role, pgDatabase }: RolePageProps) {
  const navigate = useNavigate();
  const queryClient = useQueryClient();
  const me = useGetMe();
  const project = me.data?.tenant?.projects.find((p) => p.databases.some((d) => d.database_id === databaseId));
  const database = project?.databases.find((d) => d.database_id === databaseId);
  const admin = project ? projectCaps(me.data, project.id, databaseId).atLeast("admin") : false;
  const branches = useListBranches(databaseId);
  const branch = branches.data?.find((b) => b.id === branchId);
  const roles = useListRoles(databaseId, branchId);
  const pgDatabases = useListBranchDatabases(databaseId, branchId);
  const pgName = pgDatabase ?? pgDatabases.data?.find((d) => d.main)?.name;
  const dbOwner = pgDatabases.data?.find((d) => d.name === pgName)?.owner ?? null;
  const grants = useListProjectGrants(project?.id ?? "", { query: { enabled: admin && project !== undefined } });
  const members = useListMembers({ query: { enabled: admin } });
  const row = roles.data?.find((r) => r.name === role);
  const found = row !== undefined && isManaged(row);
  const editor = usePrivilegeEditor({ databaseId, branchId, pg: pgName ?? "", role, enabled: found && pgName !== undefined });
  const [section, setSection] = useState<Section>("privileges");

  const back = {
    tab: "roles" as const,
    ...(project && { project: project.id }),
    database: databaseId,
    branch: branchId,
    ...(pgName && { pg_database: pgName }),
  };
  const backLink = (
    <Link to="/app/members" search={back} className="inline-flex items-center gap-1.5 text-[13px] text-chalk-dim outline-none hover:text-chalk focus-visible:outline-2 focus-visible:outline-sand">
      <ArrowLeft aria-hidden="true" size={16} strokeWidth={1.5} />
      Roles
    </Link>
  );
  const frame = (children: React.ReactNode) => <div className="space-y-4 px-4 py-6 sm:px-8">{children}</div>;

  const failed = me.error ?? roles.error ?? branches.error;
  if (failed) {
    return frame(
      <>
        {backLink}
        <p role="alert" className="text-sm text-coral">
          Couldn't load the role: {errorText(failed)}
        </p>
      </>,
    );
  }
  // Holders count in the Members tab: wait for the grants (Admins only) so the tabs don't jump.
  if (!me.data || !roles.data || !branches.data || (admin && grants.isPending) || (pgDatabase === undefined && pgDatabases.isPending)) {
    return frame(<p className="text-sm text-chalk-dim">Loading the role…</p>);
  }
  if (!project || !database || !branch) {
    return frame(
      <>
        {backLink}
        <p className="text-sm text-chalk-dim">This branch isn't one you can see.</p>
      </>,
    );
  }
  if (!found) {
    return frame(
      <>
        {backLink}
        <p className="text-sm text-chalk">
          {row ? `Role ${escapeName(role)} is managed by Chelabase, not here.` : `Role ${escapeName(role)} not found on ${escapeName(branch.name)}.`}
        </p>
      </>,
    );
  }

  const rows = roles.data;
  const holders = admin && grants.data ? holdersOf(role, grants.data, databaseId, rows) : null;
  const tabs: { id: Section; label: string }[] = [
    { id: "privileges", label: "Privileges" },
    ...(holders ? [{ id: "members" as const, label: `Members ${holders.length}` }] : []),
    { id: "memberships", label: "Memberships" },
    ...(admin ? [{ id: "danger" as const, label: "Danger zone" }] : []),
  ];
  const current = tabs.some((t) => t.id === section) ? section : "privileges";
  const editable = admin && editor.review === null;

  return (
    <div className="flex min-h-full flex-col lg:flex-row">
      <div className="min-w-0 flex-1 space-y-5 px-4 py-6 sm:px-8">
        {backLink}
        <header className="space-y-2">
          <p className="flex flex-wrap items-center gap-1.5 text-[13px] text-chalk-dim">
            <span>{project.name}</span>
            <ChevronRight aria-hidden="true" size={14} strokeWidth={1.5} />
            <span>{database.name}</span>
            <ChevronRight aria-hidden="true" size={14} strokeWidth={1.5} />
            <span>{escapeName(branch.name)}</span> {branch.protected && <Tag>protected</Tag>}
            <ChevronRight aria-hidden="true" size={14} strokeWidth={1.5} />
            <span>
              Postgres database <span className="font-mono text-chalk">{escapeName(pgName ?? "")}</span>
            </span>
          </p>
          <h1 className="font-mono text-2xl font-medium break-all">{escapeName(role)}</h1>
          <Attributes role={row} />
          <p className="text-[13px] text-chalk-dim">This branch only: to have it elsewhere, merge or reset the other branch.</p>
        </header>
        <Tabs label="Role sections" items={tabs} value={current} onValueChange={setSection}>
          {current === "privileges" ? (
            <PrivilegeEditor editor={editor} editable={editable} admin={admin} role={role} dbOwner={dbOwner} />
          ) : current === "members" && holders ? (
            <Holders holders={holders} members={members.data} />
          ) : current === "memberships" ? (
            <Memberships databaseId={databaseId} branchId={branchId} role={row} roles={rows} admin={admin} />
          ) : (
            <DangerZone
              databaseId={databaseId}
              branchId={branchId}
              role={role}
              roles={rows}
              onDeleted={() => {
                void queryClient.invalidateQueries({ queryKey: getListRolesQueryKey(databaseId, branchId) });
                void navigate({ to: "/app/members", search: back });
              }}
            />
          )}
        </Tabs>
      </div>
      {admin && <PrivilegePanel editor={editor} role={role} />}
    </div>
  );
}
