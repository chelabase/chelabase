import { useMemo } from "react";
import { listAccessEvents, listProjectAccessEvents, type AccessEvent, type AccessEventPage, type MeResponse, type Member } from "@chelabase/management-client";
import { AccessEventList } from "@/components/members/access-event-list";
import { ACCESS_EVENTS_KEY } from "@/components/members/keys";
import { projectCaps } from "@/lib/capabilities";
import { memberName } from "@/lib/members";
import { firstName } from "./logins";

const PAGE = 50;

export interface MemberActivityProps {
  member: Member;
  me: MeResponse;
  members: readonly Member[];
  /** The caller is a tenant admin: the tenant's log. Otherwise the log of each project they administer. */
  tenantAdmin: boolean;
}

/**
 * This member's access events: the tenant log filtered by member for a tenant admin, and for a
 * project Admin each administered project's log, merged newest first (event ids run across projects).
 */
export function MemberActivity({ member, me, members, tenantAdmin }: MemberActivityProps) {
  const projects = useMemo(
    () => (me.tenant?.projects ?? []).filter((p) => projectCaps(me, p.id).atLeast("admin")),
    [me],
  );
  const context = useMemo(
    () => ({
      names: new Map(members.map((m) => [m.id, memberName(m)])),
      databaseName: (id: string) => me.tenant?.projects.flatMap((p) => p.databases).find((d) => d.database_id === id)?.name ?? null,
      projectName: (id: string) => me.tenant?.projects.find((p) => p.id === id)?.name ?? null,
    }),
    [members, me],
  );
  const scope = tenantAdmin ? "tenant" : projects.map((p) => p.id).join(",");

  async function fetchPage(before: number | undefined): Promise<AccessEventPage> {
    const params = { member_id: member.id, limit: PAGE, ...(before !== undefined && { before }) };
    if (tenantAdmin) return listAccessEvents(params);
    const pages = await Promise.all(projects.map((p) => listProjectAccessEvents(p.id, params)));
    const seen = new Set<number>();
    const merged: AccessEvent[] = [];
    for (const e of pages.flatMap((p) => p.events).sort((a, b) => b.id - a.id)) {
      if (!seen.has(e.id)) {
        seen.add(e.id);
        merged.push(e);
      }
    }
    const events = merged.slice(0, PAGE);
    const more = merged.length > PAGE || pages.some((p) => p.next_before !== null);
    return { events, next_before: more && events.length > 0 ? events[events.length - 1].id : null };
  }

  return (
    <section aria-labelledby="member-activity" className="space-y-4">
      <div className="space-y-1">
        <h2 id="member-activity" className="text-base font-semibold">
          Activity
        </h2>
        <p className="max-w-[65ch] text-sm text-chalk-dim">Access changes to and by {firstName(memberName(member))}, newest first.</p>
      </div>
      <AccessEventList
        queryKey={[...ACCESS_EVENTS_KEY, "member", member.id, scope]}
        fetchPage={fetchPage}
        context={context}
        emptyText={`No access changes for ${memberName(member)} yet.`}
      />
    </section>
  );
}
