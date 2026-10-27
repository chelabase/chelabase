import { useState } from "react";
import { Link } from "@tanstack/react-router";
import { useQueryClient } from "@tanstack/react-query";
import { getListRolesQueryKey, updateRole, type BranchRole, type Member } from "@chelabase/management-client";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/table-editor/panel-bits";
import { errorText } from "@/lib/errors";
import { memberName } from "@/lib/members";
import { escapeName } from "../hub/escape-name";
import { TeamsPhrase, type TeamRef } from "../hub/team-via";
import { holdersByMember, inheritedBy, isManaged, type Holder } from "./model";

/** The sources of one member, with the teams that give the same role through the same grant merged into one. */
function sourceGroups(sources: readonly Holder[]): { via: string | null; teams: TeamRef[] }[] {
  const out: { via: string | null; teams: TeamRef[] }[] = [];
  for (const h of sources) {
    const team = h.team !== undefined ? { id: h.teamId ?? h.team, name: h.team } : null;
    const same = out.find((g) => g.via === h.via && (team === null) === (g.teams.length === 0));
    if (same) {
      if (team && !same.teams.some((t) => t.id === team.id)) same.teams.push(team);
    } else out.push({ via: h.via, teams: team ? [team] : [] });
  }
  return out;
}

/** MH-34: who holds the role (Admins only): directly, or through a role whose chain reaches it. */
export function Holders({ holders, members }: { holders: Holder[]; members: readonly Member[] | undefined }) {
  const byId = new Map((members ?? []).map((m) => [m.id, m]));
  return (
    <div className="space-y-3">
      <p className="text-[13px] text-chalk-dim">Members whose grant, or a team's grant, on this project gives them this role. Project Admins and tenant admins can act as every role and aren't listed.</p>
      {holders.length === 0 ? (
        <p className="text-sm text-chalk-dim">No member holds this role yet. Give it to someone from their page or the People tab.</p>
      ) : (
        <ul className="divide-y divide-line border-y border-line">
          {holdersByMember(holders).map(({ memberId, sources }) => {
            const m = byId.get(memberId);
            return (
              <li key={memberId} className="flex flex-wrap items-center gap-x-3 gap-y-0.5 py-2 text-sm">
                {m ? (
                  <Link to="/app/members/$memberId" params={{ memberId: m.id }} className="text-chalk underline decoration-line-strong underline-offset-4 hover:decoration-chalk">
                    {memberName(m)}
                  </Link>
                ) : (
                  <span className="text-chalk-dim">A member no longer listed</span>
                )}{" "}
                <span className="text-[13px] text-chalk-dim">
                  {sourceGroups(sources).map((g, i) => (
                    <span key={`${g.teams.map((t) => t.id).join(",")}:${g.via ?? ""}`}>
                      {i > 0 && "; "}
                      {g.teams.length > 0 ? (
                        <>
                          via <TeamsPhrase teams={g.teams} />
                        </>
                      ) : g.via === null ? (
                        "granted directly"
                      ) : null}
                      {g.teams.length > 0 && g.via !== null && " "}
                      {g.via !== null && (
                        <>
                          through <span className="font-mono">{escapeName(g.via)}</span>
                        </>
                      )}
                    </span>
                  ))}
                </span>
              </li>
            );
          })}
        </ul>
      )}
    </div>
  );
}

/**
 * Which roles this one is in (editable by an Admin through `PATCH …/roles/{role}`, the whole new
 * list) and which are in it. Roles Chelabase keeps out of the hub (reserved, the owner, personal
 * logins) aren't offered, and a membership in one of them is kept as it is.
 */
export function Memberships({ databaseId, branchId, role, roles, admin }: { databaseId: string; branchId: string; role: BranchRole; roles: readonly BranchRole[]; admin: boolean }) {
  const queryClient = useQueryClient();
  const [draft, setDraft] = useState<string[] | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [saved, setSaved] = useState(false);
  const current = role.member_of;
  const chosen = draft ?? current;
  // A role that is (through any chain) in this one can't also contain it.
  const options = roles.filter((r) => isManaged(r) && r.name !== role.name && !inheritedBy(roles, r.name, role.name) && (r.level !== "owner_level" || current.includes(r.name)));
  const inIt = roles.filter((r) => r.member_of.includes(role.name));
  const managedIn = inIt.filter(isManaged);
  const personalIn = inIt.filter((r) => r.origin === "personal").length;
  const changed = draft !== null && (draft.length !== current.length || draft.some((d) => !current.includes(d)));

  async function save() {
    if (!changed || busy) return;
    setBusy(true);
    setError(null);
    setSaved(false);
    try {
      // Memberships not shown here (reserved groups) are kept: the list sent is the whole new one.
      const kept = current.filter((c) => !options.some((o) => o.name === c));
      await updateRole(databaseId, branchId, role.name, { member_of: [...kept, ...chosen.filter((c) => !kept.includes(c))] });
      setDraft(null);
      setSaved(true);
      await queryClient.invalidateQueries({ queryKey: getListRolesQueryKey(databaseId, branchId) });
    } catch (err) {
      setError(errorText(err));
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="grid gap-6 md:grid-cols-2">
      <section className="space-y-2">
        <h2 className="text-sm font-semibold">{escapeName(role.name)} is in</h2>
        <p className="text-[13px] text-chalk-dim">It uses these roles' privileges{role.inherit ? "" : " after SET ROLE (it doesn't inherit)"}.</p>
        {options.length === 0 ? (
          <p className="text-sm text-chalk-dim">No other role on this branch to join.</p>
        ) : (
          <ul className="divide-y divide-line border-y border-line">
            {options.map((r) => (
              <li key={r.name}>
                <label className="flex min-h-8 items-center gap-2 py-1 text-sm">
                  <Checkbox
                    aria-label={escapeName(r.name)}
                    checked={chosen.includes(r.name)}
                    disabled={!admin || busy}
                    onChange={() => {
                      setSaved(false);
                      setDraft(chosen.includes(r.name) ? chosen.filter((c) => c !== r.name) : [...chosen, r.name]);
                    }}
                  />
                  <span className="font-mono text-[13px] text-chalk">{escapeName(r.name)}</span>
                </label>
              </li>
            ))}
          </ul>
        )}
        {error && (
          <p role="alert" className="text-sm text-coral">
            {error}
          </p>
        )}
        {saved && (
          <p role="status" className="text-sm text-seafoam">
            Memberships saved.
          </p>
        )}
        {admin && (
          <div className="flex gap-2">
            <Button disabled={!changed || busy} onClick={() => void save()}>
              Save memberships
            </Button>
            {changed && (
              <Button variant="ghost" onClick={() => setDraft(null)}>
                Undo
              </Button>
            )}
          </div>
        )}
      </section>
      <section className="space-y-2">
        <h2 className="text-sm font-semibold">In {escapeName(role.name)}</h2>
        {managedIn.length === 0 ? (
          <p className="text-sm text-chalk-dim">No role is in it.</p>
        ) : (
          <ul className="divide-y divide-line border-y border-line">
            {managedIn.map((r) => (
              <li key={r.name} className="py-1.5 font-mono text-[13px] text-chalk">
                {escapeName(r.name)}
              </li>
            ))}
          </ul>
        )}
        {personalIn > 0 && <p className="text-[13px] text-chalk-dim">And {personalIn === 1 ? "1 member's personal login" : `${personalIn} members' personal logins`}, which follow their members' access.</p>}
      </section>
    </div>
  );
}
