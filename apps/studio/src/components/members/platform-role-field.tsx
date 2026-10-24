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
  /** A first "No access" segment; choosing it calls `onSelect`, and while `selected` it shows chosen and the role's hint gives way. */
  noAccess?: { selected: boolean; onSelect: () => void };
  /** `false` leaves the role's meaning to the caller (the invite row puts it on its own line). */
  hint?: boolean;
  /** Extra classes for the visible label. */
  labelClassName?: string;
}

/** Viewer / Developer / Admin as a segmented control, with the chosen role's meaning under it. */
export function PlatformRoleField({ value, onChange, max = "admin", disabled, label = "Platform role", locked = [], noAccess, hint = true, labelClassName }: PlatformRoleFieldProps) {
  const offered = ORDER.slice(0, ORDER.indexOf(max) + 1);
  const none = noAccess?.selected === true;
  return (
    <div className="space-y-1">
      <RadioGroup<string>
        label={label}
        labelClassName={labelClassName}
        layout="segmented"
        cols={noAccess ? 4 : 3}
        tone="tab"
        options={[
          ...(noAccess ? [{ value: "none", label: "No access", disabled }] : []),
          ...offered.map((r) => ({ value: r as string, label: ROLE_LABEL[r], disabled: disabled || locked.includes(r) })),
        ]}
        value={none ? "none" : value}
        onValueChange={(next) => (next === "none" ? noAccess?.onSelect() : onChange(next as PlatformRole))}
        disabled={disabled}
      />
      {hint && <p className="max-w-[65ch] text-xs text-chalk-dim">{none ? "Removes the member's access to this project." : ROLE_HINT[value]}</p>}
    </div>
  );
}
