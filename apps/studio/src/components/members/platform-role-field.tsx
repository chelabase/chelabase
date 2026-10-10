import type { PlatformRole } from "@chelabase/management-client";
import { RadioGroup } from "@/components/ui/radio-group";
import { ROLE_LABEL } from "@/lib/members";

/** What each platform role allows, as the editor and the "Your access" line say it. */
export const ROLE_HINT: Record<PlatformRole, string> = {
  viewer: "Reads tables and structure as the roles below. No SQL editor, no writes, no connection strings or keys.",
  developer:
    "Runs SQL and edits data as the roles below, shares the project's saved queries, manages app users and unprotected branches.",
  admin: "Everything in the project, including the database owner.",
};

const ORDER: PlatformRole[] = ["viewer", "developer", "admin"];

export interface PlatformRoleFieldProps {
  value: PlatformRole;
  onChange: (role: PlatformRole) => void;
  /** The highest role offered (narrowings offer only the grant's role and lower). */
  max?: PlatformRole;
  disabled?: boolean;
  label?: string;
  /** Roles shown but not choosable (the caller explains why). */
  locked?: readonly PlatformRole[];
}

/** Viewer / Developer / Admin as a segmented control, with the chosen role's meaning under it. */
export function PlatformRoleField({ value, onChange, max = "admin", disabled, label = "Platform role", locked = [] }: PlatformRoleFieldProps) {
  const offered = ORDER.slice(0, ORDER.indexOf(max) + 1);
  return (
    <div className="space-y-1">
      <RadioGroup
        label={label}
        layout="segmented"
        options={offered.map((r) => ({ value: r, label: ROLE_LABEL[r], disabled: locked.includes(r) }))}
        value={value}
        onValueChange={onChange}
        disabled={disabled}
      />
      <p className="max-w-[65ch] text-xs text-chalk-dim">{ROLE_HINT[value]}</p>
    </div>
  );
}
