import { useEffect } from "react";
import { useQueries } from "@tanstack/react-query";
import {
  getListBranchesQueryOptions,
  getListRolesQueryOptions,
  type BranchRole,
  type DatabaseSummary,
  type PlatformRole,
} from "@chelabase/management-client";
import { MultiCombobox, type ComboOption } from "@/components/ui/combobox";
import { errorText } from "@/lib/errors";
import { whereText } from "@/lib/refusal-extra";
import { actAsReserved, refusal as ruleRefusal, refusalText } from "@/lib/role-rules";

/** One database's roles (from its main branch). */
export interface RoleSource {
  database: string;
  roles: BranchRole[];
}

export interface RoleChoice extends ComboOption {
  /** Every role this one can `SET ROLE` into (R21), in order. */
  chain: string[];
}

/** Every role `name` can `SET ROLE` into (memberships with the SET option, any depth), without `name` itself. */
function closure(name: string, byName: Map<string, BranchRole>): string[] {
  const setEdges = (r: BranchRole | undefined) => (r?.member_of ?? []).filter((g) => !(r?.member_no_set ?? []).includes(g));
  const seen = new Set<string>([name]);
  const out: string[] = [];
  const queue = setEdges(byName.get(name));
  while (queue.length > 0) {
    const next = queue.shift()!;
    if (seen.has(next)) continue;
    seen.add(next);
    out.push(next);
    queue.push(...setEdges(byName.get(next)));
  }
  return out;
}

/**
 * Why a role is Admin-only, or null when anyone may be given it: exactly the server's
 * `refusal` (the whole chain; owner-level, LOGIN or reserved), in words.
 */
function adminOnlyReason(rows: readonly BranchRole[], r: BranchRole): string | null {
  const refused = ruleRefusal(rows, r.name);
  if (!refused) return null;
  // BYPASSRLS alone counts as owner-level; say what it is.
  if (refused.chain.length === 1 && r.bypass_rls && r.level !== "owner_level") return "Bypasses RLS";
  return refusalText(refused);
}

/**
 * The picker's options, merged by role name across the project's databases (Q3): never the owner,
 * a chain shown as "also gives …", a role on only some databases noted, and below Admin the roles
 * that are Admin-only on any database disabled with their reason. The API still decides.
 */
export function roleChoices(sources: readonly RoleSource[], platformRole: PlatformRole): RoleChoice[] {
  const merged = new Map<string, { chain: Set<string>; on: string[]; reason: string | null }>();
  for (const { database, roles } of sources) {
    const byName = new Map(roles.map((r) => [r.name, r]));
    for (const r of roles) {
      // Never the owner (Admin includes it) or a reserved name (no grant may hold one).
      if (r.origin === "owner" || actAsReserved(r.name)) continue;
      const chain = closure(r.name, byName);
      const entry = merged.get(r.name) ?? { chain: new Set<string>(), on: [], reason: null };
      chain.forEach((c) => entry.chain.add(c));
      entry.on.push(database);
      entry.reason ??= adminOnlyReason(roles, r);
      merged.set(r.name, entry);
    }
  }
  return [...merged.entries()].map(([value, e]) => {
    const chain = [...e.chain];
    const notes = [
      chain.length > 0 ? `also gives ${chain.join(", ")}` : null,
      e.on.length < sources.length ? `only on ${e.on.join(", ")}` : null,
    ].filter(Boolean);
    return {
      value,
      chain,
      detail: notes.length > 0 ? notes.join(" · ") : undefined,
      disabledReason: platformRole !== "admin" && e.reason ? `Admin only: ${e.reason}` : undefined,
    };
  });
}

export interface RolePickerProps {
  /** The project's databases; each ready one's main branch supplies roles. */
  databases: readonly DatabaseSummary[];
  platformRole: PlatformRole;
  value: string[];
  onChange: (roles: string[]) => void;
  /** Roles the server refused (422 `extra.role`): their chips turn coral. */
  refused?: ReadonlySet<string>;
  /**
   * The server's refusal of a picked role (M16): its chain, the database and, when the roles that
   * broke the rule are on a branch other than main (which this picker can't see), the branch,
   * shown right under the roles.
   */
  refusal?: { role?: string; chain?: string[]; database?: string; branch?: string; branchMain?: boolean };
  disabled?: boolean;
  /** Told whether Admin-only roles are chosen below Admin: the form's Save waits until they're gone. */
  onBlockedChange?: (blocked: boolean) => void;
}

/** Postgres roles for a grant (shared by the invite and approve dialogs and the Access tab). */
export function RolePicker({ databases, platformRole, value, onChange, refused, refusal, disabled, onBlockedChange }: RolePickerProps) {
  const ready = databases.filter((d) => d.status === "ready");
  const branches = useQueries({ queries: ready.map((d) => getListBranchesQueryOptions(d.database_id)) });
  const mains = ready.map((d, i) => ({ db: d, main: branches[i].data?.find((b) => b.parent_branch_id === null) ?? null }));
  const roles = useQueries({
    queries: mains.map(({ db, main }) => getListRolesQueryOptions(db.database_id, main?.id ?? "", { query: { enabled: main !== null } })),
  });

  const notes: string[] = databases
    .filter((d) => d.status !== "ready")
    .map((d) => `Roles from ${d.name} aren't shown: it isn't ready.`);
  const sources: RoleSource[] = [];
  let loading = false;
  mains.forEach(({ db }, i) => {
    const failed = branches[i].error ?? roles[i].error;
    if (failed) notes.push(`Roles from ${db.name} aren't shown: ${errorText(failed)}`);
    else if (roles[i].data) sources.push({ database: db.name, roles: roles[i].data });
    else loading = true;
  });

  const choices = roleChoices(sources, platformRole);
  const byValue = new Map(choices.map((c) => [c.value, c]));
  const notAllowed = new Set(value.filter((v) => byValue.get(v)?.disabledReason));
  const invalid = new Set([...notAllowed, ...(refused ?? [])]);
  const blocked = notAllowed.size > 0;
  const where = whereText(refusal?.database, refusal?.branch, refusal?.branchMain);
  useEffect(() => onBlockedChange?.(blocked), [blocked, onBlockedChange]);
  const status = loading
    ? "Loading roles…"
    : choices.length === 0
      ? "No roles to grant yet. Create a NOLOGIN group role on a branch (Branches → Roles), or in SQL with CREATE ROLE … NOLOGIN."
      : undefined;

  // R17: an Admin grant holds every role on the project; narrowing is per database.
  if (platformRole === "admin") {
    return (
      <div role="group" aria-label="Postgres roles" className="space-y-1">
        <p className="text-sm font-medium">Postgres roles</p>
        <p className="text-sm">All roles</p>
        <p className="text-xs text-chalk-dim">An Admin holds every role of the project, the owner included. Narrow it on a database to keep fewer roles there.</p>
      </div>
    );
  }

  return (
    <div className="space-y-1">
      <MultiCombobox
        label="Postgres roles"
        mono
        options={choices}
        value={value}
        onValueChange={onChange}
        chipDetail={(v) => {
          const chain = byValue.get(v)?.chain ?? [];
          return chain.length > 0 ? `+ ${chain.join(", ")}` : undefined;
        }}
        invalid={invalid}
        placeholder="Pick roles"
        disabled={disabled}
        status={status}
        notes={notes}
      />
      {notAllowed.size > 0 && (
        <p role="alert" className="text-xs text-coral">
          Remove the Admin-only roles, or keep Admin.
        </p>
      )}
      {refusal?.role && value.includes(refusal.role) && refusal.chain && (
        <p data-testid="refused-role-detail" className="font-mono text-[13px] text-coral">
          {refusal.chain.join(" → ")}
          {where && <span className="font-sans text-chalk-dim"> {where}</span>}
        </p>
      )}
    </div>
  );
}
