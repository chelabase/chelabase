import { useEffect, useId, useMemo, useState } from "react";
import { Link } from "@tanstack/react-router";
import { useQueryClient } from "@tanstack/react-query";
import { Search } from "lucide-react";
import { deleteTeamMember, putTeamMember, type Member, type Team } from "@chelabase/management-client";
import { Checkbox } from "@/components/table-editor/panel-bits";
import { Button } from "@/components/ui/button";
import { refusalOrErrorText } from "@/lib/refusal-extra";
import { RefusalText } from "@/components/project-access/refusal-text";
import { memberName } from "@/lib/members";
import { breakableEmail } from "../breakable-email";
import { refreshTeams } from "./keys";
import { teamName } from "./team-model";

export interface TeamMembersProps {
  team: Team;
  roster: readonly Member[];
  canEdit: boolean;
  /** Told whenever the checklist gains or loses unsaved changes. */
  onDirtyChange: (dirty: boolean) => void;
}

interface Result {
  memberId: string;
  ok: boolean;
  text: string;
}

/** Only people who can be in a team: a pending sign-up must be approved first (409), a removed one is gone. */
const selectable = (m: Member) => m.status !== "pending" && m.status !== "removed";

/** The membership changes that turn `base` into `desired`, in roster order: one call each (P8). */
export function membershipChanges(roster: readonly Pick<Member, "id">[], base: ReadonlySet<string>, desired: ReadonlySet<string>) {
  return roster.flatMap((m) => (base.has(m.id) === desired.has(m.id) ? [] : [{ memberId: m.id, add: desired.has(m.id) }]));
}

/**
 * A team's members as a filterable checklist. Tenant admins stage changes; Save sends one call per change
 * in sequence and says what happened to each, so a refusal halfway leaves the earlier changes applied and named.
 */
export function TeamMembers({ team, roster, canEdit, onDirtyChange }: TeamMembersProps) {
  const queryClient = useQueryClient();
  const filterId = useId();
  const base = useMemo(() => new Set(team.member_ids ?? []), [team.member_ids]);
  const [desired, setDesired] = useState<ReadonlySet<string> | null>(null);
  const [query, setQuery] = useState("");
  const [busy, setBusy] = useState(false);
  const [results, setResults] = useState<Result[]>([]);

  const people = useMemo(() => roster.filter((m) => selectable(m) || base.has(m.id)), [roster, base]);
  const current = desired ?? base;
  const changes = useMemo(() => membershipChanges(people, base, current), [people, base, current]);
  const dirty = changes.length > 0;
  useEffect(() => onDirtyChange(dirty), [dirty, onDirtyChange]);
  useEffect(() => () => onDirtyChange(false), [onDirtyChange]);

  const q = query.trim().toLowerCase();
  const matching = people.filter((m) => q === "" || memberName(m).toLowerCase().includes(q) || (m.email ?? "").toLowerCase().includes(q));
  const nameOf = (id: string) => {
    const m = roster.find((x) => x.id === id);
    return m ? memberName(m) : "a former member";
  };

  function toggle(id: string) {
    const next = new Set(current);
    if (next.has(id)) next.delete(id);
    else next.add(id);
    setDesired(next);
  }

  async function save() {
    if (busy || !dirty) return;
    setBusy(true);
    setResults([]);
    const out: Result[] = [];
    for (const c of changes) {
      const name = nameOf(c.memberId);
      try {
        if (c.add) await putTeamMember(team.id, c.memberId);
        else await deleteTeamMember(team.id, c.memberId);
        out.push({ memberId: c.memberId, ok: true, text: c.add ? `Added ${name}` : `Removed ${name}` });
      } catch (err) {
        out.push({ memberId: c.memberId, ok: false, text: `Couldn't ${c.add ? "add" : "remove"} ${name}: ${refusalOrErrorText(err, { teamName: (id) => (id === team.id ? teamName(team) : undefined) })}` });
      }
      setResults([...out]);
    }
    // The team is read again; what was refused is still staged, what went through is the new baseline.
    refreshTeams(queryClient);
    await queryClient.invalidateQueries({ queryKey: [`/v1/teams/${team.id}`] });
    setBusy(false);
  }

  const resultList = results.length > 0 && (
    <ul role="status" aria-label="Results" className="space-y-1 text-sm">
      {results.map((r) => (
        <li key={r.memberId} className={r.ok ? "text-chalk" : "text-coral"}>
          <RefusalText text={r.text} />
        </li>
      ))}
    </ul>
  );

  if (!canEdit) {
    const inTeam = people.filter((m) => base.has(m.id));
    return (
      <div className="space-y-2">
        {inTeam.length === 0 ? (
          <p className="text-sm text-chalk-dim">No one is in {teamName(team)} yet.</p>
        ) : (
          <ul className="divide-y divide-line border border-line">
            {inTeam.map((m) => (
              <li key={m.id} className="px-3 py-2 text-sm">
                <Link to="/app/members/$memberId" params={{ memberId: m.id }} className="text-chalk underline-offset-4 hover:underline">
                  {memberName(m)}
                </Link>
                {m.email && <span className="ml-2 text-chalk-dim [overflow-wrap:anywhere]">{breakableEmail(m.email)}</span>}
              </li>
            ))}
          </ul>
        )}
      </div>
    );
  }

  return (
    <div className="space-y-3">
      {people.length > 8 && (
        <div className="relative max-w-xs">
          <label htmlFor={filterId} className="sr-only">
            Filter people
          </label>
          <Search aria-hidden="true" size={14} strokeWidth={1.5} className="pointer-events-none absolute top-1/2 left-2 -translate-y-1/2 text-chalk-dim" />
          <input
            id={filterId}
            type="search"
            value={query}
            placeholder="Filter people"
            onChange={(e) => setQuery(e.target.value)}
            className="h-8 w-full border border-input bg-ink pr-2 pl-7 text-sm text-chalk outline-none placeholder:text-chalk-dim focus-visible:outline-2 focus-visible:-outline-offset-1 focus-visible:outline-sand"
          />
        </div>
      )}
      {people.length === 0 ? (
        <p className="text-sm text-chalk-dim">No one to add yet. Invite people from the People tab.</p>
      ) : (
        <ul className="divide-y divide-line border border-line">
          {matching.map((m) => {
            const was = base.has(m.id);
            const now = current.has(m.id);
            return (
              <li key={m.id} data-changed={was !== now ? "true" : undefined} className="flex items-start gap-2 px-2 py-1.5 data-[changed=true]:shadow-[inset_2px_0_0_var(--sand)]">
                <Checkbox aria-label={memberName(m)} checked={now} disabled={busy} onChange={() => toggle(m.id)} className="mt-0.5" />
                <span className="min-w-0 text-sm">
                  <span className="text-chalk">{memberName(m)}</span>
                  {m.email && <span className="ml-2 text-xs text-chalk-dim [overflow-wrap:anywhere]">{breakableEmail(m.email)}</span>}
                  {m.status !== "active" && <span className="ml-2 text-xs text-chalk-dim">{m.status === "invited" ? "Invited" : "Suspended"}</span>}
                </span>
              </li>
            );
          })}
        </ul>
      )}
      {matching.length === 0 && people.length > 0 && <p className="text-sm text-chalk-dim">No one matches “{query.trim()}”.</p>}
      {resultList}
      {dirty && (
        <div className="flex flex-wrap items-center justify-between gap-3 border border-line bg-deep px-4 py-3 text-sm">
          <p className="text-chalk-dim">{`${changes.length} unsaved ${changes.length === 1 ? "change" : "changes"}`}</p>
          <div className="flex items-center gap-2">
            <Button type="button" variant="ghost" disabled={busy} onClick={() => setDesired(null)}>
              Discard
            </Button>
            <Button type="button" disabled={busy} onClick={() => void save()}>
              {busy ? "Saving…" : "Save members"}
            </Button>
          </div>
        </div>
      )}
    </div>
  );
}
