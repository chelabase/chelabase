import { useCallback } from "react";
import { X } from "lucide-react";
import type { InviteGrant, PlatformRole, Project } from "@chelabase/management-client";
import { Button } from "@/components/ui/button";
import { PlatformRoleField } from "./platform-role-field";
import { whereText } from "@/lib/refusal-extra";
import { RolePicker } from "./role-picker";

export interface GrantRow {
  projectId: string;
  platformRole: PlatformRole;
  pgRoles: string[];
  /** Admin-only roles are chosen below Admin: sending waits. */
  blocked: boolean;
}

/** A server refusal tied to one row: the sentence, the refused role and its chain. */
export interface RowError {
  message: string;
  role?: string;
  chain?: string[];
  database?: string;
  /** The branch whose roles broke it, and whether that is main (M16). */
  branch?: string;
  branchMain?: boolean;
}

export function toInviteGrants(rows: readonly GrantRow[]): InviteGrant[] {
  return rows.map((r) => ({ project_id: r.projectId, platform_role: r.platformRole, pg_roles: r.pgRoles }));
}

export interface GrantRowsProps {
  projects: readonly Project[];
  rows: GrantRow[];
  onChange: (rows: GrantRow[]) => void;
  errors?: Record<string, RowError>;
  disabled?: boolean;
}

/**
 * The "Access" section of the invite and approve dialogs: zero or more projects, each with a
 * platform role (Developer first) and the Postgres roles. A project is used once at most.
 */
export function GrantRows({ projects, rows, onChange, errors = {}, disabled }: GrantRowsProps) {
  const used = new Set(rows.map((r) => r.projectId));
  const free = projects.filter((p) => !used.has(p.id) && p.status !== "deleting");
  const update = useCallback(
    (i: number, patch: Partial<GrantRow>) => onChange(rows.map((r, j) => (j === i ? { ...r, ...patch } : r))),
    [rows, onChange],
  );
  return (
    <fieldset className="m-0 min-w-0 space-y-3 border-0 p-0 pb-3" disabled={disabled}>
      <legend className="mb-1 text-sm font-medium">Access</legend>
      {rows.length === 0 && (
        <p className="text-sm text-chalk-dim">
          Without a project, they join with no access; you can give access later from a project's Access tab.
        </p>
      )}
      <ul className="space-y-3">
        {rows.map((row, i) => {
          const project = projects.find((p) => p.id === row.projectId);
          const name = project?.name ?? "this project";
          const error = errors[row.projectId];
          return (
            <li key={row.projectId} role="group" aria-label={name} className="space-y-3 border border-line p-3">
              <div className="flex items-end gap-2">
                <label className="flex flex-1 flex-col gap-1 text-sm">
                  <span className="font-medium">Project</span>
                  <select
                    value={row.projectId}
                    onChange={(e) => update(i, { projectId: e.target.value, pgRoles: [] })}
                    className="h-8 border border-input bg-ink px-2 text-sm text-chalk outline-none focus-visible:outline-2 focus-visible:-outline-offset-1 focus-visible:outline-sand"
                  >
                    {[...(project ? [project] : []), ...free].map((p) => (
                      <option key={p.id} value={p.id}>
                        {p.name}
                      </option>
                    ))}
                  </select>
                </label>
                <Button
                  type="button"
                  variant="ghost"
                  size="icon"
                  aria-label={`Remove ${name}`}
                  onClick={() => onChange(rows.filter((_, j) => j !== i))}
                >
                  <X aria-hidden="true" size={16} strokeWidth={1.5} />
                </Button>
              </div>
              <PlatformRoleField value={row.platformRole} onChange={(platformRole) => update(i, { platformRole })} />
              <RolePicker
                databases={project?.databases ?? []}
                platformRole={row.platformRole}
                value={row.pgRoles}
                onChange={(pgRoles) => update(i, { pgRoles })}
                refused={error?.role ? new Set([error.role]) : undefined}
                refusal={error}
                onBlockedChange={(blocked) => {
                  if (blocked !== row.blocked) update(i, { blocked });
                }}
              />
              {error && (
                <div role="alert" className="space-y-1 text-sm text-coral">
                  <p>{error.message}</p>
                  {/* The picker shows the chain of a picked role it refused; otherwise it is shown here. */}
                  {!(error.role && row.pgRoles.includes(error.role)) && error.chain && error.chain.length > 0 && (
                    <p className="font-mono text-[13px]">
                      {error.chain.join(" → ")}
                      {whereText(error.database, error.branch, error.branchMain) && (
                        <span className="font-sans text-chalk-dim"> {whereText(error.database, error.branch, error.branchMain)}</span>
                      )}
                    </p>
                  )}
                </div>
              )}
            </li>
          );
        })}
      </ul>
      <Button
        type="button"
        variant="outline"
        size="sm"
        disabled={free.length === 0}
        onClick={() => onChange([...rows, { projectId: free[0].id, platformRole: "developer", pgRoles: [], blocked: false }])}
      >
        Add a project
      </Button>
    </fieldset>
  );
}
