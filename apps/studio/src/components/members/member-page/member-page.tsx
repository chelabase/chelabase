import { useState, type ReactNode } from "react";
import { Link, useSearch } from "@tanstack/react-router";
import { ArrowLeft } from "lucide-react";
import { useGetMe, useListMembers, type MeResponse, type Member } from "@chelabase/management-client";
import { useCrumbLabel } from "@/components/app-shell/crumbs";
import { Button } from "@/components/ui/button";
import { projectCaps } from "@/lib/capabilities";
import { errorText } from "@/lib/errors";
import { initials, memberName, memberStatus } from "@/lib/members";
import { isUuid, type HubSearch } from "@/lib/members-search";
import { cn } from "@/lib/utils";
import { breakableEmail } from "../breakable-email";
import { LevelBadge } from "../hub";
import { MemberActions } from "./member-actions";
import { MemberActivity } from "./member-activity";
import { dateText, firstName } from "./logins";
import { PersonalLogins } from "./personal-logins";
import { ProjectAccess } from "./project-access";
import { TeamsSection } from "./teams-section";

export interface MemberPageProps {
  memberId: string;
}

/** The page's column and the way back to the roster, which keeps its filter, view and name filter. */
function Shell({ children }: { children: ReactNode }) {
  const search = useSearch({ strict: false }) as HubSearch;
  return (
    <div className="mx-auto max-w-[56rem] space-y-8 p-8 max-sm:p-4">
      <Link
        to="/app/members"
        search={{ ...(search.project && { project: search.project }), ...(search.view && { view: search.view }), ...(search.q && { q: search.q }) }}
        className="inline-flex items-center gap-1 text-sm text-chalk-dim underline-offset-4 hover:text-chalk hover:underline"
      >
        <ArrowLeft aria-hidden="true" size={16} strokeWidth={1.5} />
        Members
      </Link>
      {children}
    </div>
  );
}

function NotFound() {
  return (
    <div className="space-y-1">
      <h1 className="text-2xl font-semibold">Member not found</h1>
      <p className="text-sm text-chalk-dim">There is no member with that id, or you can't see them.</p>
    </div>
  );
}

/** `/app/members/$memberId`: one person's access, personal logins and activity, by what the caller may do (Part 3, Part 7). */
export function MemberPage({ memberId }: MemberPageProps) {
  return <Shell>{isUuid(memberId) ? <Loaded memberId={memberId} /> : <NotFound />}</Shell>;
}

function Loaded({ memberId }: { memberId: string }) {
  const me = useGetMe();
  const members = useListMembers();
  const failed = me.error ?? members.error;
  if (me.data === undefined || members.data === undefined) {
    if (failed) {
      return (
        <div className="space-y-3">
          <p role="alert" className="text-sm text-coral">
            Couldn't load this member: {errorText(failed)}
          </p>
          <Button
            variant="outline"
            onClick={() => {
              void me.refetch();
              void members.refetch();
            }}
          >
            Retry
          </Button>
        </div>
      );
    }
    return <p className="text-sm text-chalk-dim">Loading member…</p>;
  }
  const member = members.data.find((m) => m.id === memberId);
  if (!member) return <NotFound />;
  return <Page member={member} me={me.data} roster={members.data} />;
}

/** "Suspended on 14 Oct 2026 by Mara Okafor": the date and the admin, never an id (a suspender no longer listed is "an admin"). */
function suspendedLine(member: Member, roster: readonly Member[]): string {
  const by = member.suspended_by ? roster.find((m) => m.id === member.suspended_by) : undefined;
  const on = member.suspended_at ? dateText(member.suspended_at) : "an unrecorded date";
  return `Suspended on ${on} by ${by ? memberName(by) : "an admin"}.`;
}

function Page({ member, me, roster }: { member: Member; me: MeResponse; roster: readonly Member[] }) {
  const [status, setStatus] = useState<string | null>(null);
  const name = memberName(member);
  useCrumbLabel("memberId", member.id, name);
  const first = firstName(name);
  const selfId = me.member?.id ?? null;
  const self = selfId === member.id;
  const tenantAdmin = me.member?.tenant_admin === true;
  const projectAdmin = (me.tenant?.projects ?? []).some((p) => projectCaps(me, p.id).atLeast("admin"));
  const admin = tenantAdmin || projectAdmin;
  const suspended = member.status === "suspended";
  const statusText = memberStatus(member);
  const otherAdmins = roster.some((m) => m.tenant_admin && m.status === "active" && m.id !== member.id);

  return (
    <>
      <header>
        <div className="flex min-w-0 items-start gap-4">
          <div aria-hidden="true" className="flex size-11 shrink-0 items-center justify-center border border-line-strong bg-deep text-sm font-semibold">
            {initials(name)}
          </div>
          <div className="min-w-0 flex-1 space-y-1">
            <div className="flex flex-wrap items-center justify-between gap-x-4 gap-y-2">
            <h1 className="text-2xl font-semibold break-words">
              {name}
              {self && <span className="ml-2 text-base font-normal text-chalk-dim">(you)</span>}
            </h1>
            <MemberActions member={member} tenantAdmin={tenantAdmin} self={self} otherAdmins={otherAdmins} onStatus={setStatus} />
            </div>
            <p className="flex flex-wrap items-center gap-x-3 gap-y-1 text-sm text-chalk-dim">
              {member.email && <span className="[overflow-wrap:anywhere]">{breakableEmail(member.email)}</span>}
              <LevelBadge level={member.access.level} />
              <span className={cn(statusText.tone === "hazard" && "text-coral", statusText.tone === "plain" && "text-chalk")} title={statusText.title}>
                {statusText.text}
              </span>
              <span>{`Joined ${dateText(member.created_at)}`}</span>
            </p>
            {/* Beside the actions, so the confirmation shows where the user acted (at 390 px the page top may be off-screen). */}
            {status && (
              <p role="status" className="text-sm text-chalk">
                {status}
              </p>
            )}
          </div>
        </div>
      </header>

      {suspended && (
        <div className="relative w-full space-y-1 border border-coral/60 bg-coral/10 p-4 pl-9 text-sm">
          <span aria-hidden="true" className="absolute top-[1.375rem] left-4 size-2 border border-coral" />
          <p className="max-w-[65ch]">
            <strong className="font-semibold text-chalk">{suspendedLine(member, roster)}</strong> {first} can't sign in to Studio.
          </p>
          <p>{first}'s personal logins were revoked and database sessions ended.</p>
          <p className="text-chalk-dim">{first}'s project access below is kept. Reinstate gives it back as it is; {first} makes new personal logins afterwards.</p>
        </div>
      )}

      <ProjectAccess member={member} me={me} self={self} suspended={suspended} onStatus={setStatus} />
      {(self || admin) && <PersonalLogins member={member} me={me} self={self} />}
      <TeamsSection member={member} tenantAdmin={tenantAdmin && !self} onStatus={setStatus} />
      {admin && <MemberActivity member={member} me={me} members={roster} tenantAdmin={tenantAdmin} />}
    </>
  );
}
