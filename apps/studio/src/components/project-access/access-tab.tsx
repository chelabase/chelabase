import { useEffect, useMemo, useRef, useState, type RefObject } from "react";
import { Link } from "@tanstack/react-router";
import { useQueryClient } from "@tanstack/react-query";
import { ChevronDown } from "lucide-react";
import {
  ApiError,
  getGetMeQueryKey,
  listProjectAccessEvents,
  useGetMe,
  useListMembers,
  useListProjectGrants,
  type Member,
  type MeRoles,
  type Project,
  type ProjectGrant,
} from "@chelabase/management-client";
import { AccessEventList } from "@/components/members/access-event-list";
import { InviteLinkDialog, type OneTimeLink } from "@/components/members/invite-link-panel";
import { ACCESS_EVENTS_KEY } from "@/components/members/keys";
import type { RoleSource } from "@/components/members/role-picker";
import { Button } from "@/components/ui/button";
import { projectCaps } from "@/lib/capabilities";
import { errorText } from "@/lib/errors";
import { memberName, ROLE_LABEL } from "@/lib/members";
import { useMediaQuery } from "@/lib/use-media-query";
import { cn } from "@/lib/utils";
import { AddPeopleDialog } from "./add-people-dialog";
import { editorId, GrantEditor } from "./grant-editor";
import { overrideWords } from "./narrowing";
import { chainsOf, useRoleSources } from "./use-role-sources";
import { YourAccess } from "./your-access";

const PAGE = 50;
const ADD = "\u0000add";
/** Below 40rem the Per database column folds under the Postgres roles cell. */
const NARROW = "(max-width: 40rem)";

/**
 * A project's Access tab. Everyone with access sees it; below project Admin it is "Your access"
 * only and calls no Admin-only route (Q8). Admins get the grants table, the inline editor, "Add
 * people" and the project's access log.
 */
export function AccessTab({ project }: { project: Project }) {
  const me = useGetMe();
  if (me.data === undefined) {
    return me.error ? (
      <div className="space-y-3">
        <p role="alert" className="text-sm text-coral">
          Couldn't load your access: {errorText(me.error)}
        </p>
        <Button variant="outline" onClick={() => void me.refetch()}>
          Retry
        </Button>
      </div>
    ) : (
      <p className="text-sm text-chalk-dim">Loading access…</p>
    );
  }
  if (!projectCaps(me.data, project.id).atLeast("admin")) return <YourAccess project={project} me={me.data} />;
  return <AdminAccess project={project} />;
}

function AdminAccess({ project }: { project: Project }) {
  const queryClient = useQueryClient();
  const me = useGetMe();
  const grants = useListProjectGrants(project.id);
  const members = useListMembers();
  const { sources, loading: rolesLoading } = useRoleSources(project.databases);
  const chains = useMemo(() => chainsOf(sources), [sources]);
  const narrow = useMediaQuery(NARROW);

  const [openId, setOpenId] = useState<string | null>(null);
  const [status, setStatus] = useState<string | null>(null);
  const [adding, setAdding] = useState(false);
  const [link, setLink] = useState<OneTimeLink | null>(null);
  const addButton = useRef<HTMLButtonElement>(null);
  const expanders = useRef(new Map<string, HTMLButtonElement>());
  /** The row whose editor just closed: its expand control gets focus back. */
  const [refocus, setRefocus] = useState<string | null>(null);
  useEffect(() => {
    if (refocus && openId === null) {
      // The row is gone (access taken away): "Add people" is the next place to be.
      if (refocus === ADD) addButton.current?.focus();
      else expanders.current.get(refocus)?.focus();
      setRefocus(null);
    }
  }, [refocus, openId]);

  const forbidden = grants.error instanceof ApiError && grants.error.status === 403;
  // The caller lost Admin meanwhile: the next `me` refresh falls back to "Your access".
  useEffect(() => {
    if (forbidden) void queryClient.invalidateQueries({ queryKey: getGetMeQueryKey() });
  }, [forbidden, queryClient]);

  const list: Member[] = members.data ?? [];
  const byId = useMemo(() => new Map(list.map((m) => [m.id, m])), [list]);
  const context = useMemo(
    () => ({
      names: new Map(list.map((m) => [m.id, memberName(m)])),
      databaseName: (id: string) => project.databases.find((d) => d.database_id === id)?.name ?? null,
      // This list is one project's: other projects in an event's grants are named from `me`.
      projectName: (id: string) => (id === project.id ? project.name : (me.data?.tenant?.projects.find((p) => p.id === id)?.name ?? null)),
    }),
    [list, project.databases, project.id, project.name, me.data],
  );

  if (forbidden) {
    return <p className="text-sm text-chalk-dim">Only project Admins can see who has access to {project.name}.</p>;
  }

  const tenantAdmins = list.filter((m) => m.tenant_admin && m.status !== "removed" && m.status !== "pending");
  const adminIds = new Set(tenantAdmins.map((m) => m.id));
  const granted = (grants.data ?? [])
    .filter((g) => !adminIds.has(g.member_id))
    .map((g) => ({ grant: g, member: byId.get(g.member_id) }))
    .sort((a, b) => nameOf(a).localeCompare(nameOf(b)));
  const grantedIds = new Set((grants.data ?? []).map((g) => g.member_id));
  const suggestions = list.filter((m) => !m.tenant_admin && !grantedIds.has(m.id) && (m.status === "active" || m.status === "invited"));

  const loading = grants.data === undefined || members.data === undefined;
  const failed = grants.error ?? members.error;

  function closeEditor(id: string, done?: string, gone?: boolean) {
    if (done) setStatus(done);
    setOpenId(null);
    setRefocus(gone ? ADD : id);
  }

  return (
    <div className="space-y-8">
      <YourAccess project={project} me={me.data!} compact />
      <section aria-labelledby="people-with-access" className="space-y-4">
        <div className="flex flex-wrap items-start justify-between gap-3">
          <div className="space-y-1">
            <h3 id="people-with-access" className="text-base font-semibold">
              People with access
            </h3>
            <p className="max-w-[65ch] text-sm text-chalk-dim">
              Each person gets a platform role on this project and the Postgres roles they may act as.
            </p>
          </div>
          <div className="flex flex-wrap items-center gap-3">
            <Link to="/app/members" search={{ project: project.id }} className="text-sm text-chalk underline underline-offset-4">
              Manage in Members
            </Link>
            <Button ref={addButton} onClick={() => setAdding(true)}>
              Add people
            </Button>
          </div>
        </div>
        {status && (
          <p role="status" className="text-sm text-chalk">
            {status}
          </p>
        )}
        {loading ? (
          failed ? (
            <div className="space-y-3">
              <p role="alert" className="text-sm text-coral">
                Couldn't load who has access: {errorText(failed)}
              </p>
              <Button variant="outline" onClick={() => void Promise.all([grants.refetch(), members.refetch()])}>
                Retry
              </Button>
            </div>
          ) : (
            <p className="text-sm text-chalk-dim">Loading access…</p>
          )
        ) : (
          <>
            <div className="relative max-w-full min-w-0 overflow-x-auto">
              <table aria-label="People with access" className="w-full text-left text-sm">
                <thead className="border-b border-line text-chalk-dim">
                  <tr>
                    <th scope="col" className="py-2 pr-3 font-medium">
                      Person
                    </th>
                    {/* Below 40rem one column holds it all, so the table never scrolls sideways. */}
                    {!narrow && (
                      <>
                        <th scope="col" className="py-2 pr-3 font-medium">
                          Platform role
                        </th>
                        <th scope="col" className="py-2 pr-3 font-medium">
                          Postgres roles
                        </th>
                        <th scope="col" className="py-2 pr-3 font-medium">
                          Per database
                        </th>
                      </>
                    )}
                    <th scope="col" className="w-10 py-2">
                      <span className="sr-only">Edit</span>
                    </th>
                  </tr>
                </thead>
                <tbody>
                  {tenantAdmins.map((m) => (
                    <tr key={m.id} className="border-b border-line align-top">
                      <td className="space-y-1 py-2 pr-3">
                        <Person member={m} label="Tenant admin" projectId={project.id} />
                        {narrow && <p>Admin</p>}
                        {narrow && <p className="text-chalk-dim">Every role, including the database owner</p>}
                      </td>
                      {!narrow && (
                        <>
                          <td className="py-2 pr-3">Admin</td>
                          <td className="py-2 pr-3 text-chalk-dim">Every role, including the database owner</td>
                          <td className="py-2 pr-3 text-chalk-dim">—</td>
                        </>
                      )}
                      <td />
                    </tr>
                  ))}
                  {granted.map(({ grant, member }) => {
                    const name = nameOf({ grant, member });
                    const open = openId === grant.member_id;
                    return (
                      <GrantRow
                        key={grant.member_id}
                        project={project}
                        grant={grant}
                        member={member}
                        name={name}
                        chains={chains}
                        roleSources={sources}
                        rolesLoading={rolesLoading}
                        open={open}
                        narrow={narrow}
                        onToggle={() => {
                          // Another row's "Saved …" line no longer applies.
                          if (!open) setStatus(null);
                          setOpenId(open ? null : grant.member_id);
                        }}
                        onClose={(done, gone) => closeEditor(grant.member_id, done, gone)}
                        returnFocus={addButton}
                        onStatus={setStatus}
                        expander={(el) => {
                          if (el) expanders.current.set(grant.member_id, el);
                          else expanders.current.delete(grant.member_id);
                        }}
                      />
                    );
                  })}
                </tbody>
              </table>
            </div>
            {tenantAdmins.length > 0 && (
              <p className="text-xs text-chalk-dim">
                Tenant admins are Admin on every project. Change that on the{" "}
                <Link to="/app/members" className="text-chalk underline underline-offset-4">
                  Members page
                </Link>
                .
              </p>
            )}
            {granted.length === 0 && (
              <p className="flex flex-wrap items-center gap-2 text-sm text-chalk-dim">
                No one else has access yet. Add people to give them a role on this project.
                <Button variant="outline" size="sm" onClick={() => setAdding(true)}>
                  Add people
                </Button>
              </p>
            )}
          </>
        )}
      </section>
      <section aria-labelledby="access-changes" className="space-y-3">
        <h3 id="access-changes" className="text-base font-semibold">
          Access changes
        </h3>
        <AccessEventList
          queryKey={[...ACCESS_EVENTS_KEY, "project", project.id]}
          fetchPage={(before) => listProjectAccessEvents(project.id, before === undefined ? { limit: PAGE } : { before, limit: PAGE })}
          context={context}
          emptyText="No access changes on this project yet."
        />
      </section>
      <AddPeopleDialog
        open={adding}
        onOpenChange={setAdding}
        project={project}
        suggestions={suggestions}
        members={list}
        grants={grants.data ?? []}
        finalFocus={addButton}
        onDone={(message) => {
          setAdding(false);
          setStatus(message);
        }}
        onLink={(l) => {
          setAdding(false);
          setLink(l);
        }}
      />
      <InviteLinkDialog
        link={link}
        title={`Invite link for ${link?.email ?? ""}`}
        finalFocus={addButton}
        onClose={() => {
          setStatus(`Created an invite link for ${link?.email}. Send it to them yourself; access counts once they accept.`);
          setLink(null);
        }}
      />
    </div>
  );
}

function nameOf({ grant, member }: { grant: ProjectGrant; member: Member | undefined }): string {
  return member ? memberName(member) : `Former member ${grant.member_id.slice(0, 8)}`;
}

function Person({ member, label, projectId }: { member: Member | undefined; label?: string; projectId: string }) {
  if (!member) return <span className="text-chalk-dim">A former member</span>;
  return (
    <div className="min-w-0">
      <div className="flex flex-wrap items-center gap-x-2">
        <Link
          to="/app/members/$memberId"
          params={{ memberId: member.id }}
          search={{ project: projectId }}
          className="font-medium underline decoration-line-strong underline-offset-4 hover:decoration-chalk"
        >
          {memberName(member)}
        </Link>
        {label && <span className="text-xs text-chalk-dim">{label}</span>}
        {member.status === "invited" && (
          <span className="text-xs text-chalk-dim" title="Counts once they accept" tabIndex={0}>
            Invited
          </span>
        )}
      </div>
      {member.email && <div className="truncate text-xs text-chalk-dim">{member.email}</div>}
    </div>
  );
}

function RoleChips({ roles, chains }: { roles: MeRoles; chains: ReadonlyMap<string, string[]> }) {
  // R17: an Admin grant holds every role on the project.
  if (roles === "all") return <p>All roles</p>;
  if (roles.length === 0) return <p className="text-chalk-dim">No roles</p>;
  return (
    <ul className="flex flex-wrap gap-1">
      {roles.map((r) => {
        // A role granted by name is listed as its own chip: the chain only adds what is not already shown (BP-5).
        const chain = (chains.get(r) ?? []).filter((c) => !roles.includes(c));
        return (
          <li key={r} className="inline-flex items-center gap-1 border border-line-strong px-1.5 leading-6">
            <span className="font-mono text-[13px]">{r}</span>
            {chain.length > 0 && <span className="text-chalk-dim">+ {chain.join(", ")}</span>}
          </li>
        );
      })}
    </ul>
  );
}

function Narrowed({ project, grant }: { project: Project; grant: ProjectGrant }) {
  if (grant.overrides.length === 0) return <p className="text-chalk-dim">Same everywhere</p>;
  return (
    <ul className="flex flex-wrap gap-1">
      {grant.overrides.map((o) => {
        const db = project.databases.find((d) => d.database_id === o.database_id)?.name ?? "a deleted database";
        return (
          <li key={o.database_id} className="border border-line px-1.5 text-xs leading-6 text-chalk">
            {db}: {overrideWords(o)}
          </li>
        );
      })}
    </ul>
  );
}

function GrantRow({
  project,
  grant,
  member,
  name,
  chains,
  roleSources,
  rolesLoading,
  open,
  narrow,
  onToggle,
  onClose,
  onStatus,
  returnFocus,
  expander,
}: {
  project: Project;
  grant: ProjectGrant;
  member: Member | undefined;
  name: string;
  chains: ReadonlyMap<string, string[]>;
  roleSources: readonly RoleSource[];
  rolesLoading: boolean;
  open: boolean;
  narrow: boolean;
  onToggle: () => void;
  onClose: (done?: string, gone?: boolean) => void;
  onStatus: (message: string) => void;
  returnFocus: RefObject<HTMLElement | null>;
  expander: (el: HTMLButtonElement | null) => void;
}) {
  return (
    <>
      <tr className={cn("align-top", !open && "border-b border-line")}>
        {narrow ? (
          // Below 40rem: one cell, stacked, so the table never scrolls sideways.
          <td className="space-y-2 py-2 pr-3">
            <Person member={member} projectId={project.id} />
            <div>
              <div>{ROLE_LABEL[grant.platform_role]}</div>
              {grant.platform_role === "admin" && <div className="text-xs text-chalk-dim">includes the database owner</div>}
            </div>
            <RoleChips roles={grant.pg_roles} chains={chains} />
            <Narrowed project={project} grant={grant} />
          </td>
        ) : (
          <>
            <td className="py-2 pr-3">
              <Person member={member} projectId={project.id} />
            </td>
            <td className="py-2 pr-3">
              <div>{ROLE_LABEL[grant.platform_role]}</div>
              {grant.platform_role === "admin" && <div className="text-xs text-chalk-dim">includes the database owner</div>}
            </td>
            <td className="py-2 pr-3">
              <RoleChips roles={grant.pg_roles} chains={chains} />
            </td>
            <td className="py-2 pr-3">
              <Narrowed project={project} grant={grant} />
            </td>
          </>
        )}
        <td className="py-1 text-right">
          <button
            type="button"
            ref={expander}
            aria-expanded={open}
            aria-controls={open ? editorId(grant.member_id) : undefined}
            aria-label={`Edit access for ${name}`}
            onClick={onToggle}
            className="inline-flex size-9 items-center justify-center text-chalk-dim outline-none hover:bg-chalk/[0.06] hover:text-chalk focus-visible:outline-2 focus-visible:-outline-offset-2 focus-visible:outline-sand"
          >
            <ChevronDown aria-hidden="true" size={16} strokeWidth={1.5} className={cn("transition-transform duration-150 motion-reduce:transition-none", open && "rotate-180")} />
          </button>
        </td>
      </tr>
      {open && (
        <tr className="border-b border-line">
          <td colSpan={narrow ? 2 : 5} className="pb-3">
            <GrantEditor project={project} grant={grant} member={member} name={name} roleSources={roleSources} rolesLoading={rolesLoading} chains={chains} onClose={onClose} onStatus={onStatus} returnFocus={returnFocus} />
          </td>
        </tr>
      )}
    </>
  );
}
