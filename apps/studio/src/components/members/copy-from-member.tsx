import { useId, useState } from "react";
import { listProjectGrants, useListMembers, type ProjectGrant, type Project } from "@chelabase/management-client";
import { Button } from "@/components/ui/button";
import { Select } from "@/components/ui/select";
import { errorText } from "@/lib/errors";
import { memberName } from "@/lib/members";
import type { GrantRow } from "./grant-rows";

/**
 * P23: one custom row per project where `memberId` has an OWN grant, from the tenant's grants
 * lists. Teams are never copied: a team's access is not in `listProjectGrants`. An Admin grant
 * names no roles (R17). Projects being deleted are skipped; a project whose list can't be read is skipped.
 */
export async function rowsFromMember(
  memberId: string,
  projects: readonly Project[],
  fetchGrants: (projectId: string) => Promise<ProjectGrant[]> = listProjectGrants,
): Promise<GrantRow[]> {
  const rows: GrantRow[] = [];
  for (const p of projects) {
    if (p.status === "deleting") continue;
    let grants: ProjectGrant[];
    try {
      grants = await fetchGrants(p.id);
    } catch {
      continue;
    }
    const g = grants.find((x) => x.member_id === memberId);
    if (!g) continue;
    const roles = Array.isArray(g.pg_roles) ? g.pg_roles : [];
    rows.push({ projectId: p.id, platformRole: g.platform_role, pgRoles: g.platform_role === "admin" ? [] : [...roles], blocked: false });
  }
  return rows;
}

export interface CopyFromMemberProps {
  projects: readonly Project[];
  /** The rows copied replace the dialog's custom rows. */
  onCopy: (rows: GrantRow[]) => void;
  disabled?: boolean;
}

/** "Copy from a member": a one-time fill of the custom rows from one member's own grants. */
export function CopyFromMember({ projects, onCopy, disabled }: CopyFromMemberProps) {
  const members = useListMembers();
  const id = useId();
  const [pick, setPick] = useState("");
  const [busy, setBusy] = useState(false);
  const [note, setNote] = useState<string | null>(null);
  const candidates = (members.data ?? []).filter((m) => m.status === "active" && !m.tenant_admin);
  if (candidates.length === 0) return null;

  async function copy() {
    const m = candidates.find((c) => c.id === pick);
    if (!m) return;
    setBusy(true);
    setNote(null);
    try {
      const rows = await rowsFromMember(m.id, projects);
      if (rows.length === 0) setNote(`${memberName(m)} has no access of their own to copy.`);
      else {
        onCopy(rows);
        setNote(`Copied ${memberName(m)}'s own access. Their teams aren't copied.`);
      }
    } catch (err) {
      setNote(errorText(err));
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="space-y-1">
      <div className="flex flex-wrap items-center gap-2">
        <label htmlFor={id} className="sr-only">
          Copy from a member
        </label>
        <Select id={id} value={pick} onChange={(e) => setPick(e.target.value)} disabled={disabled || busy} wrapperClassName="min-w-48">
          <option value="">Copy from a member…</option>
          {candidates.map((m) => (
            <option key={m.id} value={m.id}>
              {memberName(m)}
            </option>
          ))}
        </Select>
        <Button type="button" variant="outline" size="sm" disabled={pick === "" || disabled || busy} onClick={() => void copy()}>
          {busy ? "Copying…" : "Copy"}
        </Button>
      </div>
      {note && (
        <p role="status" className="text-xs text-chalk-dim">
          {note}
        </p>
      )}
    </div>
  );
}
