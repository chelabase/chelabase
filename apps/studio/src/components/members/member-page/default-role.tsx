import { Select } from "@/components/ui/select";
import { useId } from "react";
import type { DatabaseSummary, MeResponse } from "@chelabase/management-client";
import { roleChoices } from "@/components/members/role-picker";
import { useRoleSources } from "@/components/project-access/use-role-sources";
import { projectCaps } from "@/lib/capabilities";
import { escapeName } from "../hub";

/**
 * The group roles a personal login can start a session as: held on one database at Developer or
 * above (an Admin holds every role), never the owner, and only roles whose whole `SET ROLE` chain
 * is plain (no owner-level, LOGIN, ADMIN-holding or reserved role: the rule the role picker applies
 * to a Developer). Read from the main branch for every caller; nothing is offered until it loads.
 */
export function useGroupRoles(me: MeResponse | undefined, projectId: string, database: DatabaseSummary | undefined): string[] {
  const caps = projectCaps(me, projectId, database?.database_id);
  const { sources } = useRoleSources(database ? [database] : []);
  if (!caps.atLeast("developer")) return [];
  const plain = roleChoices(sources, "developer")
    .filter((c) => c.disabledReason === undefined)
    .map((c) => c.value);
  return caps.roles === "all" ? plain : plain.filter((r) => (caps.roles as string[]).includes(r));
}

export interface DefaultRoleSelectProps {
  roles: readonly string[];
  value: string;
  onChange: (role: string) => void;
  /** A visible label (the reveal dialog) or, with `compact`, an accessible name only (a table row). */
  label: string;
  compact?: boolean;
}

/** "Default role": None, or one of the member's groups; it adds `options=-c role=…` to a connection string (P33). */
export function DefaultRoleSelect({ roles, value, onChange, label, compact }: DefaultRoleSelectProps) {
  const id = useId();
  if (roles.length === 0) return null;
  const select = (
    <Select
      id={id}
      aria-label={compact ? label : undefined}
      value={value}
      onChange={(e) => onChange(e.target.value)}
      className="font-mono text-[13px]"
    >
      <option value="">None</option>
      {roles.map((r) => (
        <option key={r} value={r}>
          {escapeName(r)}
        </option>
      ))}
    </Select>
  );
  if (compact) return select;
  return (
    <div className="space-y-1">
      <label htmlFor={id} className="block text-sm font-medium">
        {label}
      </label>
      {select}
      <p className="text-xs text-chalk-dim">
        Starts every session as this group, so what you create is owned by it, without a SET ROLE first.
      </p>
    </div>
  );
}
