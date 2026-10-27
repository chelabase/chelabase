import { useId, useState } from "react";
import { useQueryClient } from "@tanstack/react-query";
import {
  deleteTeamMember,
  getListMemberTeamsQueryKey,
  getListTeamsQueryKey,
  putTeamMember,
  useListMemberTeams,
  useListTeams,
  type Member,
  type Team,
} from "@chelabase/management-client";
import { Button } from "@/components/ui/button";
import { ConfirmDialog } from "@/components/ui/confirm-dialog";
import { Select } from "@/components/ui/select";
import { errorText } from "@/lib/errors";
import { refusalOrErrorText } from "@/lib/refusal-extra";
import { RefusalText } from "@/components/project-access/refusal-text";
import { memberName } from "@/lib/members";
import { TeamLink } from "../hub/team-via";
import { refreshAccess } from "../keys";
import { firstName } from "./logins";

export { TeamLink };

export interface TeamsSectionProps {
  member: Member;
  /** The caller is a tenant admin: only they add or remove teams (P8). */
  tenantAdmin: boolean;
  onStatus: (message: string | null) => void;
}

/** Part 3, section 3: the member's teams as chips with links; tenant admins add or remove. */
export function TeamsSection({ member, tenantAdmin, onStatus }: TeamsSectionProps) {
  const queryClient = useQueryClient();
  const mine = useListMemberTeams(member.id);
  const all = useListTeams({ query: { enabled: tenantAdmin } });
  const pickId = useId();
  const [pick, setPick] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [removing, setRemoving] = useState<Team | null>(null);
  const name = memberName(member);
  const first = firstName(name);
  const teams = mine.data ?? [];
  const inIds = new Set(teams.map((t) => t.id));
  const candidates = (all.data ?? []).filter((t) => !inIds.has(t.id));

  async function change(run: () => Promise<unknown>, done: string) {
    setBusy(true);
    setError(null);
    try {
      await run();
      void queryClient.invalidateQueries({ queryKey: getListMemberTeamsQueryKey(member.id) });
      void queryClient.invalidateQueries({ queryKey: getListTeamsQueryKey() });
      refreshAccess(queryClient);
      onStatus(done);
      return true;
    } catch (err) {
      setError(refusalOrErrorText(err, { teamName: (id) => (all.data ?? []).find((t) => t.id === id)?.name }));
      return false;
    } finally {
      setBusy(false);
    }
  }

  return (
    <section aria-labelledby="member-teams" className="space-y-3">
      <div className="space-y-1">
        <h2 id="member-teams" className="text-base font-semibold">
          Teams
        </h2>
        <p className="max-w-[65ch] text-sm text-chalk-dim">A team's access applies to {first} as long as {first} is in it.</p>
      </div>
      {mine.isPending ? (
        <p className="text-sm text-chalk-dim">Loading teams…</p>
      ) : mine.isError ? (
        <p className="text-sm text-chalk-dim">Couldn't load teams: {errorText(mine.error)}</p>
      ) : teams.length === 0 ? (
        <p className="text-sm text-chalk-dim">Not in any team.</p>
      ) : (
        <ul className="flex flex-wrap gap-2">
          {teams.map((t) => (
            <li key={t.id} className="inline-flex items-baseline gap-2 border border-line-strong px-2 py-1 text-sm">
              <TeamLink team={t} />
              {tenantAdmin && (
                <Button type="button" variant="ghost" size="sm" aria-label={`Remove from ${t.name}`} disabled={busy} onClick={() => setRemoving(t)}>
                  Remove
                </Button>
              )}
            </li>
          ))}
        </ul>
      )}
      {tenantAdmin && candidates.length > 0 && (
        <div className="flex flex-wrap items-center gap-3 border border-dashed border-line-strong p-3">
          <h3 className="text-sm font-semibold">Add to a team</h3>
          <label htmlFor={pickId} className="sr-only">
            Team to add
          </label>
          <Select id={pickId} value={pick} onChange={(e) => setPick(e.target.value)} wrapperClassName="min-w-48">
            <option value="">Choose a team</option>
            {candidates.map((t) => (
              <option key={t.id} value={t.id}>
                {t.name}
              </option>
            ))}
          </Select>
          <Button
            type="button"
            variant="outline"
            disabled={pick === "" || busy}
            onClick={() => {
              const team = candidates.find((t) => t.id === pick);
              if (!team) return;
              void change(() => putTeamMember(team.id, member.id), `Added ${name} to ${team.name}.`).then((ok) => ok && setPick(""));
            }}
          >
            Add to team
          </Button>
        </div>
      )}
      {error && !removing && (
        <p role="alert" className="text-sm text-coral">
          <RefusalText text={error} />
        </p>
      )}
      {removing && (
        <ConfirmDialog
          open
          onOpenChange={(next) => {
            if (!next) {
              setRemoving(null);
              setError(null);
            }
          }}
          title={`Remove ${name} from ${removing.name}?`}
          body={<p>{first} loses the access this team gives, except what their own grants or other teams give.</p>}
          confirm="Remove from team"
          tone="destructive"
          busy={busy}
          error={error && <RefusalText text={error} />}
          onConfirm={() => {
            const team = removing;
            void change(() => deleteTeamMember(team.id, member.id), `Removed ${name} from ${team.name}.`).then((ok) => ok && setRemoving(null));
          }}
        />
      )}
    </section>
  );
}
