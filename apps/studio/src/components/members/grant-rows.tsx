import { Select } from "@/components/ui/select";
import { useCallback } from "react";
import { Plus, X } from "lucide-react";
import type { InviteGrant, PlatformRole, Project } from "@chelabase/management-client";
import { Button } from "@/components/ui/button";
import { PlatformRoleField, ROLE_HINT } from "./platform-role-field";
import { whereText } from "@/lib/refusal-extra";
import { RolePicker } from "./role-picker";
import { CopyFromMember } from "./copy-from-member";

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
  // R17: an Admin grant holds every role, so it names none.
  return rows.map((r) => ({ project_id: r.projectId, platform_role: r.platformRole, pg_roles: r.platformRole === "admin" ? [] : r.pgRoles }));
}

export interface GrantRowsProps {
  projects: readonly Project[];
  rows: GrantRow[];
  onChange: (rows: GrantRow[]) => void;
  errors?: Record<string, RowError>;
  disabled?: boolean;
  /** Said after "Access" in the legend ("the same for everyone in this invite"). */
  legendNote?: string;
  /** Offer "Copy from a member" (the tenant invite and the approval; never a project's invite). */
  copyFrom?: boolean;
  /** One or more teams are chosen alongside: with no project the person still gets what the teams give. */
  teamsChosen?: boolean;
}

/**
 * The "Access" section of the invite and approve dialogs: zero or more projects, each with a
 * platform role (Developer first) and the Postgres roles. A project is used once at most.
 */
export function GrantRows({ projects, rows, onChange, errors = {}, disabled, legendNote, copyFrom = false, teamsChosen = false }: GrantRowsProps) {
  const used = new Set(rows.map((r) => r.projectId));
  const free = projects.filter((p) => !used.has(p.id) && p.status !== "deleting");
  const update = useCallback(
    (i: number, patch: Partial<GrantRow>) => onChange(rows.map((r, j) => (j === i ? { ...r, ...patch } : r))),
    [rows, onChange],
  );
  return (
    <fieldset className="m-0 min-w-0 space-y-3 border-0 p-0 pb-3" disabled={disabled}>
      <legend className="mb-1 text-sm">
        <span className="font-medium">Access</span>
        {legendNote && <span className="font-normal text-chalk-dim"> · {legendNote}</span>}
      </legend>
      {copyFrom && <CopyFromMember projects={projects} onCopy={onChange} disabled={disabled} />}
      {rows.length === 0 && (
        <p className="text-sm text-chalk-dim">
          {teamsChosen
            ? "Without a project, they get only their teams' access; you can give more later from a project's Access tab."
            : "Without a project, they join with no access; you can give access later from a project's Access tab."}
        </p>
      )}
      <ul className="space-y-3">
        {rows.map((row, i) => {
          const project = projects.find((p) => p.id === row.projectId);
          const name = project?.name ?? "this project";
          const error = errors[row.projectId];
          return (
            <li key={row.projectId} role="group" aria-label={name} className="space-y-3 border border-line p-3">
              <div className="flex flex-col gap-3 sm:flex-row sm:items-end">
                <label className="flex flex-col gap-1 text-sm sm:w-40 sm:shrink-0">
                  <span className="font-medium sm:sr-only">Project</span>
                  <Select
                    value={row.projectId}
                    onChange={(e) => update(i, { projectId: e.target.value, pgRoles: [] })}
                  >
                    {[...(project ? [project] : []), ...free].map((p) => (
                      <option key={p.id} value={p.id}>
                        {p.name}
                      </option>
                    ))}
                  </Select>
                </label>
                <PlatformRoleField value={row.platformRole} onChange={(platformRole) => update(i, { platformRole })} hint={false} labelClassName="sm:sr-only" />
                <div className="min-w-0 sm:min-w-48 sm:flex-1">
                  <RolePicker
                    databases={project?.databases ?? []}
                    platformRole={row.platformRole}
                    value={row.pgRoles}
                    onChange={(pgRoles) => update(i, { pgRoles })}
                    refused={error?.role ? new Set([error.role]) : undefined}
                    refusal={error}
                    labelClassName="sm:sr-only"
                    onBlockedChange={(blocked) => {
                      if (blocked !== row.blocked) update(i, { blocked });
                    }}
                  />
                </div>
                <Button
                  type="button"
                  variant="ghost"
                  size="icon"
                  className="self-end sm:self-auto"
                  aria-label={`Remove ${name}`}
                  onClick={() => onChange(rows.filter((_, j) => j !== i))}
                >
                  <X aria-hidden="true" size={16} strokeWidth={1.5} />
                </Button>
              </div>
              <p className="w-full max-w-[65ch] text-xs text-chalk-dim">{ROLE_HINT[row.platformRole]}</p>
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
        variant="ghost"
        size="sm"
        disabled={free.length === 0}
        onClick={() => onChange([...rows, { projectId: free[0].id, platformRole: "developer", pgRoles: [], blocked: false }])}
      >
        <Plus aria-hidden="true" size={14} strokeWidth={1.5} />
        Add a project
      </Button>
    </fieldset>
  );
}
