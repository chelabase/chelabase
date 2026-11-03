import { Select } from "@/components/ui/select";
import { useEffect, useId, useMemo, useRef, useState, type ReactNode } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { Link, useNavigate, useRouter, useSearch } from "@tanstack/react-router";
import { MoreHorizontal, Plus, Search } from "lucide-react";
import {
  ApiError,
  removeMember,
  resendInvite,
  revokeInvite,
  updateMember,
  useGetInviteDelivery,
  type Member,
  type Project,
} from "@chelabase/management-client";
import { Button } from "@/components/ui/button";
import { ConfirmDialog } from "@/components/ui/confirm-dialog";
import { Menu, MenuItem, MenuNote } from "@/components/ui/menu";
import { errorText } from "@/lib/errors";
import { initials, memberName, memberStatus, ROLE_LABEL, utc, type StatusText } from "@/lib/members";
import type { HubSearch } from "@/lib/members-search";
import { NARROW_QUERY, useMediaQuery } from "@/lib/use-media-query";
import { groupRoster, LevelBadge, Tag, useViewPreference, ViewToggle, type Group, type GroupKey, type RosterView } from "./hub";
import { breakableEmail } from "./breakable-email";
import { InviteDialog } from "./invite-dialog";
import { InviteLinkDialog, type OneTimeLink } from "./invite-link-panel";
import { refreshAccess } from "./keys";
import { dateText } from "./member-page/logins";
import { Requests } from "./requests";

export interface PeopleTabProps {
  members: readonly Member[];
  /** The caller's member id (`/v1/me`'s `member.id`): their row says "(you)". */
  selfId: string | null;
  tenantAdmin: boolean;
  projects: readonly Project[];
  /** What was just done, said once: the page's single status line (a newer one replaces it). */
  onStatus: (message: string | null) => void;
}

export type Confirm =
  | { kind: "make-admin"; member: Member }
  | { kind: "step-down"; member: Member }
  | { kind: "remove"; member: Member }
  | { kind: "revoke"; member: Member };

export const LAST_ADMIN = (name: string) => `${name} is the last tenant admin. Make someone else a tenant admin first.`;

/** Below 40rem the roster stacks: one block per person instead of a table. */


/** The roster's place in the URL: the project filter, the view and the name filter (`/app/members?project=&view=&q=`). */
interface Nav {
  project: string | undefined;
  view: RosterView | undefined;
  q: string;
  setProject: (project: string | undefined) => void;
  setView: (view: RosterView) => void;
  setQ: (q: string) => void;
  /** A link to the member's page that carries the filter, the view and the name filter back. */
  memberLink: (m: Member, className: string, children: ReactNode) => ReactNode;
}

/**
 * The People tab. Inside the router its filter, view and name filter live in the URL (so the member
 * page's back link restores them); without one (a bare render) they are plain state.
 */
export function PeopleTab(props: PeopleTabProps) {
  const router = useRouter({ warn: false });
  return router ? <RoutedPeopleTab {...props} /> : <LocalPeopleTab {...props} />;
}

function RoutedPeopleTab(props: PeopleTabProps) {
  const search = useSearch({ strict: false }) as HubSearch;
  const navigate = useNavigate({ from: "/app/members" });
  const set = (patch: Partial<HubSearch>) =>
    void navigate({ search: (prev: Record<string, unknown>) => ({ ...prev, ...patch }), replace: true });
  const nav: Nav = {
    project: search.project,
    view: search.view,
    q: search.q ?? "",
    setProject: (project) => set({ project }),
    setView: (view) => set({ view }),
    setQ: (q) => set({ q: q === "" ? undefined : q }),
    memberLink: (m, className, children) => (
      <Link
        to="/app/members/$memberId"
        params={{ memberId: m.id }}
        search={{ ...(search.project && { project: search.project }), ...(search.view && { view: search.view }), ...(search.q && { q: search.q }) }}
        className={className}
      >
        {children}
      </Link>
    ),
  };
  return <PeopleRoster {...props} nav={nav} />;
}

function LocalPeopleTab(props: PeopleTabProps) {
  const [project, setProject] = useState<string | undefined>();
  const [view, setView] = useState<RosterView | undefined>();
  const [q, setQ] = useState("");
  const nav: Nav = {
    project,
    view,
    q,
    setProject,
    setView,
    setQ,
    // A bare render has no router for `Link` to read; the plain anchor is the same address.
    memberLink: (m, className, children) => (
      <a href={`/app/members/${encodeURIComponent(m.id)}`} className={className}>
        {children}
      </a>
    ),
  };
  return <PeopleRoster {...props} nav={nav} />;
}

/** The meaning shown at the right of a group heading (MH-35). */
const GROUP_HINT: Partial<Record<GroupKey, string>> = {
  tenant_admins: "Admin on every project",
  other: "Access only on projects you can't see",
  none: "Signed in, no project yet",
  invited: "Access counts once they accept",
  suspended: "Can't sign in; access kept for reinstating",
};

/** A tenant admin is one whatever the level says. */
const levelOf = (m: Member) => (m.tenant_admin ? "tenant_admin" : m.access.level);

/** The roster: who is here, grouped by what they can do, their status as words, and (for tenant admins) one menu per row. */
function PeopleRoster({ members, selfId, tenantAdmin, projects, onStatus: setStatus, nav }: PeopleTabProps & { nav: Nav }) {
  const queryClient = useQueryClient();
  const [storedView, storeView] = useViewPreference("roster");
  const view = nav.view ?? storedView;
  const project = projects.find((p) => p.id === nav.project);
  const filterId = useId();
  const projectId = useId();
  const narrow = useMediaQuery(NARROW_QUERY);
  const inviteButton = useRef<HTMLButtonElement>(null);
  /** Each row's menu trigger, so a dialog opened from a row hands focus back to it. */
  const triggers = useRef(new Map<string, HTMLElement>());
  const returnTo = useRef<HTMLElement | null>(null);
  const [query, setQuery] = useState(nav.q);
  const [inviting, setInviting] = useState(false);
  const [confirm, setConfirm] = useState<Confirm | null>(null);
  const [confirmError, setConfirmError] = useState<string | null>(null);
  const [confirmBusy, setConfirmBusy] = useState(false);
  const [working, setWorking] = useState<{ id: string; label: string } | null>(null);
  const [rowError, setRowError] = useState<{ id: string; message: string } | null>(null);
  /** A removal or revocation whose identity step failed (503): kept here, since removed people aren't listed. */
  const [unfinished, setUnfinished] = useState<{ name: string; retry: () => Promise<void> } | null>(null);
  const [link, setLink] = useState<(OneTimeLink & { title: string; done: string }) | null>(null);
  const delivery = useGetInviteDelivery({ query: { enabled: tenantAdmin } });
  // The last request was handled and Requests went with its dialog: "Invite people" takes focus
  // once the list no longer shows it.
  const [focusInvite, setFocusInvite] = useState(false);

  const pending = members.filter((m) => m.status === "pending");
  useEffect(() => {
    if (focusInvite && pending.length === 0) {
      inviteButton.current?.focus();
      setFocusInvite(false);
    }
  }, [focusInvite, pending.length]);
  const people = members.filter((m) => m.status !== "pending" && m.status !== "removed");
  const otherAdmins = people.filter((m) => m.tenant_admin && m.status === "active" && m.id !== selfId).length;
  const shown = useMemo(() => {
    const q = query.trim().toLowerCase();
    if (!q) return people;
    return people.filter((m) => memberName(m).toLowerCase().includes(q) || (m.email ?? "").toLowerCase().includes(q));
  }, [people, query]);
  const groups = useMemo(() => groupRoster(shown, { projectId: project?.id, projectName: project?.name }), [shown, project]);
  // "{m} on {project}": the people who hold a role there (Invited and Suspended don't count).
  const onProject = useMemo(
    () =>
      project
        ? groupRoster(people, { projectId: project.id }).filter((g) => g.key === "admins" || g.key === "developers" || g.key === "viewers").reduce((n, g) => n + g.members.length, 0)
        : 0,
    [people, project],
  );

  const refresh = () => refreshAccess(queryClient);
  const fromRow = (m: Member) => {
    returnTo.current = triggers.current.get(m.id) ?? null;
  };
  const ask = (c: Confirm) => {
    fromRow(c.member);
    setConfirmError(null);
    setConfirm(c);
  };

  async function act(m: Member, label: string, run: () => Promise<unknown>, done?: string) {
    fromRow(m);
    setWorking({ id: m.id, label });
    setRowError(null);
    setStatus(null);
    try {
      await run();
      if (done) setStatus(done);
    } catch (err) {
      const name = memberName(m);
      setRowError({ id: m.id, message: err instanceof ApiError && err.code === "last_tenant_admin" ? LAST_ADMIN(name) : errorText(err) });
    } finally {
      setWorking(null);
      refresh();
    }
  }

  function resend(m: Member, how: "email" | "link") {
    const inviteId = m.invite?.id;
    const email = m.email ?? memberName(m);
    if (!inviteId) return;
    void act(m, "Sending…", async () => {
      try {
        // The plain function: a new link's URL must never reach a cache.
        const out = await resendInvite(inviteId, { delivery: how });
        if (out.invite_url) {
          setLink({
            email,
            url: out.invite_url,
            title: `New invite link for ${email}`,
            done: `Created a new invite link for ${email}. The previous link no longer works.`,
          });
        } else setStatus(`Sent a new invite to ${email}. The previous link no longer works.`);
      } catch (err) {
        if (err instanceof ApiError && err.code === "link_not_allowed") {
          throw new Error(`${email} already has an account or a pending invite elsewhere, so a link can't be used. Resend the invite by email.`);
        }
        if (err instanceof ApiError && err.code === "invite_closed") throw new Error("This invite was revoked or accepted in the meantime.");
        throw err;
      }
    });
  }

  /** Runs a DELETE whose identity step may fail after the Chelabase step succeeded (503). */
  async function removal(name: string, run: () => Promise<unknown>): Promise<void> {
    try {
      await run();
      setUnfinished(null);
    } catch (err) {
      if (err instanceof ApiError && (err.code === "unavailable" || err.code === "identity_unavailable")) {
        setUnfinished({ name, retry: () => removal(name, run) });
        return;
      }
      throw err;
    } finally {
      refresh();
    }
  }

  async function confirmed() {
    if (!confirm || confirmBusy) return;
    const { member: m } = confirm;
    const name = memberName(m);
    setConfirmError(null);
    setConfirmBusy(true);
    try {
      if (confirm.kind === "make-admin") await updateMember(m.id, { tenant_admin: true });
      else if (confirm.kind === "step-down") await updateMember(m.id, { tenant_admin: false });
      else if (confirm.kind === "remove") await removal(name, () => removeMember(m.id));
      else if (m.invite) await removal(name, () => revokeInvite(m.invite!.id));
      setStatus(
        confirm.kind === "make-admin"
          ? `${name} is now a tenant admin.`
          : confirm.kind === "step-down"
            ? "You stepped down as tenant admin."
            : confirm.kind === "remove"
              ? `Removed ${name}.`
              : `Revoked the invite to ${m.email ?? name}.`,
      );
      // The row (and its menu) goes: focus moves to the next person's menu, else the
      // previous one's, else "Invite people", never to the page body.
      if (confirm.kind === "remove" || confirm.kind === "revoke") {
        const at = shown.findIndex((s) => s.id === m.id);
        const neighbour = shown[at + 1] ?? shown[at - 1];
        returnTo.current = (neighbour && triggers.current.get(neighbour.id)) ?? inviteButton.current;
      }
      setConfirm(null);
      refresh();
    } catch (err) {
      if (err instanceof ApiError && err.code === "last_tenant_admin") setConfirmError(LAST_ADMIN(name));
      else if (err instanceof ApiError && err.code === "conflict" && confirm.kind === "make-admin")
        setConfirmError(`Only an active member can become a tenant admin. ${name} hasn't accepted their invite yet.`);
      else if (err instanceof ApiError && err.code === "invite_closed") {
        setConfirmError("This invite was accepted or replaced in the meantime.");
        refresh();
      } else setConfirmError(errorText(err));
    } finally {
      setConfirmBusy(false);
    }
  }

  const confirmCopy = confirm ? copyFor(confirm) : null;
  const emailAvailable = delivery.data?.email_available ?? false;

  /** One row's menu (tenant admins): what applies to that person. */
  function actions(m: Member) {
    const name = memberName(m);
    const self = m.id === selfId;
    const busy = working?.id === m.id;
    return (
      <Menu
        trigger={
          <Button
            ref={(el: HTMLElement | null) => {
              if (el) triggers.current.set(m.id, el);
              else triggers.current.delete(m.id);
            }}
            variant="ghost"
            size={busy ? "sm" : "icon"}
            disabled={busy}
            aria-label={busy ? undefined : `More actions for ${name}`}
          >
            {busy ? working?.label : <MoreHorizontal aria-hidden="true" size={18} strokeWidth={1.5} />}
          </Button>
        }
      >
        {m.status === "invited" ? (
          <>
            {emailAvailable && <MenuItem onClick={() => resend(m, "email")}>Resend invite</MenuItem>}
            <MenuItem onClick={() => resend(m, "link")}>New invite link</MenuItem>
            <MenuItem destructive onClick={() => ask({ kind: "revoke", member: m })}>
              Revoke invite
            </MenuItem>
          </>
        ) : self ? (
          m.tenant_admin && otherAdmins > 0 ? (
            <MenuItem onClick={() => ask({ kind: "step-down", member: m })}>Step down as tenant admin</MenuItem>
          ) : m.tenant_admin ? (
            <MenuNote>You're the only tenant admin.</MenuNote>
          ) : (
            <MenuNote>Nothing to change on your own row.</MenuNote>
          )
        ) : (
          <>
            {m.tenant_admin ? (
              <MenuItem
                onClick={() => void act(m, "Removing…", () => updateMember(m.id, { tenant_admin: false }), `${name} is no longer a tenant admin.`)}
              >
                Remove tenant admin
              </MenuItem>
            ) : (
              <MenuItem onClick={() => ask({ kind: "make-admin", member: m })}>Make tenant admin</MenuItem>
            )}
            <MenuItem destructive onClick={() => ask({ kind: "remove", member: m })}>
              Remove from tenant
            </MenuItem>
          </>
        )}
      </Menu>
    );
  }

  /** An invited person has no level yet: the tag would only promise it. */
  const tagLevel = (m: Member) => (m.status === "invited" ? "none" : levelOf(m));
  const countText = (m: Member) => {
    const n = m.access.projects.length;
    return n === 0 ? "No projects" : `${n} ${n === 1 ? "project" : "projects"}`;
  };
  /** The member's role on the filtered project (a tenant admin is Admin everywhere). */
  const roleOnProject = (m: Member) => {
    const tenantAdminHere = m.tenant_admin || m.access.level === "tenant_admin";
    const role = tenantAdminHere ? "admin" : m.access.projects.find((p) => p.project_id === project?.id)?.platform_role;
    if (!role) return <span className="text-chalk-dim">—</span>;
    return (
      <span className="inline-flex flex-wrap items-center gap-1">
        <Tag chalk={role !== "viewer"}>{ROLE_LABEL[role]}</Tag>
        {tenantAdminHere && <LevelBadge level="tenant_admin" />}
      </span>
    );
  };

  const nameLine = (m: Member) => (
    <>
      {m.display_name || memberName(m)}
      {m.id === selfId && <span className="text-chalk-dim"> (you)</span>}
    </>
  );

  const rowErrorOf = (m: Member) =>
    rowError?.id === m.id && (
      <p role="alert" className="mt-1 text-coral">
        {rowError.message}
      </p>
    );

  /** The list's first cell: the name (a link to the member's page), the email under it. */
  function person(m: Member) {
    return (
      <div className="flex items-start gap-3">
        <span
          aria-hidden="true"
          className={`flex size-8 shrink-0 items-center justify-center border text-xs text-chalk-dim ${m.status === "invited" ? "border-dashed border-line-strong" : "border-line-strong"}`}
        >
          {initials(memberName(m))}
        </span>
        <div className="min-w-0 flex-1">
          <div className="truncate text-chalk">
            {nav.memberLink(m, "underline-offset-4 outline-none hover:underline focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-sand", nameLine(m))}
          </div>
          <div className="[overflow-wrap:anywhere] text-chalk-dim">{m.email ? breakableEmail(m.email) : "Email unknown"}</div>
          {rowErrorOf(m)}
        </div>
      </div>
    );
  }

  function card(m: Member) {
    const s = memberStatus(m);
    return (
      <li key={m.id} className="relative border border-line hover:bg-chalk/[0.06]">
        {nav.memberLink(
          m,
          "block p-3 pr-12 text-sm outline-none focus-visible:outline-2 focus-visible:-outline-offset-2 focus-visible:outline-sand",
          <>
            <span className="flex items-center gap-3">
              <span
                aria-hidden="true"
                className={`flex size-8 shrink-0 items-center justify-center border text-xs text-chalk-dim ${m.status === "invited" ? "border-dashed border-line-strong" : "border-line-strong"}`}
              >
                {initials(memberName(m))}
              </span>
              <span className="min-w-0">
                <span className="block truncate font-semibold text-chalk">{nameLine(m)}</span>
                <span className="block [overflow-wrap:anywhere] text-chalk-dim">{m.email ? breakableEmail(m.email) : "Email unknown"}</span>
              </span>
            </span>
            <span className="mt-3 flex flex-wrap items-center gap-x-2 gap-y-1 text-xs text-chalk-dim">
              <LevelBadge level={tagLevel(m)} />
              <span>{countText(m)}</span>
              {s.text !== "Active" && <Status s={s} />}
            </span>
          </>,
        )}
        {tenantAdmin && <div className="absolute top-1 right-1">{actions(m)}</div>}
        {rowErrorOf(m) && <div className="px-3 pb-3 text-sm">{rowErrorOf(m)}</div>}
      </li>
    );
  }

  function heading(g: Group) {
    const hint = project ? undefined : GROUP_HINT[g.key];
    return (
      <div className="flex items-baseline gap-3 border-b border-line pb-1">
        <h3 className="text-base font-semibold">
          {g.title} <span className="font-normal text-chalk-dim tabular-nums">{g.members.length}</span>
        </h3>
        {hint && <span className="ml-auto text-xs text-chalk-dim">{hint}</span>}
      </div>
    );
  }

  const columns = tenantAdmin ? 6 : 5;

  return (
    <div className="space-y-6">
      {tenantAdmin && pending.length > 0 && (
        <Requests pending={pending} projects={projects} onStatus={setStatus} onEmptied={() => setFocusInvite(true)} />
      )}

      <div className="flex flex-wrap items-center justify-between gap-3">
        <p className="text-sm text-chalk-dim tabular-nums">
          {project
            ? `${people.length} ${people.length === 1 ? "person" : "people"} · ${onProject} on ${project.name}`
            : `${people.length} ${people.length === 1 ? "person" : "people"}`}
        </p>
        <div className="flex flex-wrap items-center gap-3">
          <label htmlFor={filterId} className="sr-only">
            Filter by name or email
          </label>
          <div className="relative">
            <Search aria-hidden="true" size={14} strokeWidth={1.5} className="pointer-events-none absolute top-1/2 left-2 -translate-y-1/2 text-chalk-dim" />
            <input
              id={filterId}
              type="search"
              value={query}
              placeholder="Filter by name or email"
              onChange={(e) => {
                setQuery(e.target.value);
                nav.setQ(e.target.value);
              }}
              className="h-8 w-56 border border-input bg-ink pr-2 pl-7 text-sm text-chalk outline-none placeholder:text-chalk-dim focus-visible:outline-2 focus-visible:-outline-offset-1 focus-visible:outline-sand"
            />
          </div>
          <label htmlFor={projectId} className="sr-only">
            Project
          </label>
          <Select
            id={projectId}
            value={project?.id ?? ""}
            onChange={(e) => nav.setProject(e.target.value === "" ? undefined : e.target.value)}
            wrapperClassName="w-44"
          >
            <option value="">All projects</option>
            {projects.map((p) => (
              <option key={p.id} value={p.id}>
                {p.name}
              </option>
            ))}
          </Select>
          <ViewToggle
            value={view}
            onChange={(next) => {
              storeView(next);
              nav.setView(next);
            }}
          />
          {tenantAdmin && (
            <Button ref={inviteButton} onClick={() => setInviting(true)}>
              <Plus aria-hidden="true" size={14} strokeWidth={1.5} />
              Invite people
            </Button>
          )}
        </div>
      </div>
      {!tenantAdmin && (
        <div className="space-y-1 text-sm text-chalk-dim">
          <p>Tenant admins invite and remove people.</p>
          <p>Levels count only the projects you can see.</p>
        </div>
      )}

      {unfinished && (
        <div role="alert" className="flex flex-wrap items-center gap-3 border border-coral/60 p-3 text-sm text-coral">
          <span className="flex-1">{unfinished.name} is removed, but signing them out of the identity provider didn't finish.</span>
          <Button size="sm" variant="outline" onClick={() => void unfinished.retry()}>
            Retry
          </Button>
        </div>
      )}

      {shown.length === 0 ? (
        <p className="text-sm text-chalk-dim">
          No one matches “{query.trim()}”.{" "}
          <button
            type="button"
            className="text-chalk underline underline-offset-4"
            onClick={() => {
              setQuery("");
              nav.setQ("");
            }}
          >
            Clear filter
          </button>
        </p>
      ) : view === "cards" ? (
        <div className="space-y-6">
          {groups.map((g) => (
            <section key={g.key} className="space-y-3">
              {heading(g)}
              <ul className="grid gap-3 sm:grid-cols-2 md:grid-cols-3">{g.members.map(card)}</ul>
            </section>
          ))}
        </div>
      ) : narrow ? (
        <div aria-label="People" role="group" className="space-y-6 text-sm">
          {groups.map((g) => (
            <section key={g.key} className="space-y-1">
              {heading(g)}
              <ul className="border-b border-line">
                {g.members.map((m) => (
                  <li key={m.id} className="flex items-start gap-3 border-b border-line py-2 last:border-b-0">
                    <div className="min-w-0 flex-1">
                      {person(m)}
                      <div className="mt-1 flex flex-wrap items-center gap-x-2 gap-y-1 text-xs">
                        {project ? roleOnProject(m) : <LevelBadge level={tagLevel(m)} />}
                        <Status s={memberStatus(m)} />
                      </div>
                    </div>
                    {tenantAdmin && actions(m)}
                  </li>
                ))}
              </ul>
            </section>
          ))}
        </div>
      ) : (
        <table aria-label="People" className="w-full table-fixed text-left text-sm">
          <thead className="border-b border-line text-chalk-dim">
            <tr>
              <th scope="col" className="py-2 font-medium">Person</th>
              <th scope="col" className={project ? "w-44 py-2 font-medium" : "w-32 py-2 font-medium"}>{project ? `On ${project.name}` : "Level"}</th>
              <th scope="col" className="w-20 py-2 font-medium">Projects</th>
              <th scope="col" className="w-44 py-2 font-medium">Status</th>
              <th scope="col" className="w-28 py-2 font-medium">Joined</th>
              {tenantAdmin && <th scope="col" className="w-14 py-2"><span className="sr-only">Actions</span></th>}
            </tr>
          </thead>
          {groups.map((g) => (
            <tbody key={g.key}>
              <tr>
                <td colSpan={columns} className="pt-6 pb-1">
                  {heading(g)}
                </td>
              </tr>
              {g.members.map((m) => {
                const joined = m.status === "invited" ? null : (m.accepted_at ?? m.created_at);
                return (
                  <tr key={m.id} className="border-b border-line align-top last:border-0">
                    <td className="py-2 pr-3">{person(m)}</td>
                    <td className="py-2 pr-3">
                      {project ? roleOnProject(m) : (tagLevel(m) === "none" ? <span className="text-chalk-dim">—</span> : <LevelBadge level={tagLevel(m)} />)}
                    </td>
                    <td className="py-2 pr-3 tabular-nums text-chalk-dim">{m.access.projects.length}</td>
                    <td className="py-2 pr-3">
                      <Status s={memberStatus(m)} />
                    </td>
                    <td className="py-2 tabular-nums text-chalk-dim">
                      {joined ? <time dateTime={joined} title={utc(joined)}>{dateText(joined)}</time> : "—"}
                    </td>
                    {tenantAdmin && <td className="py-1 text-right">{actions(m)}</td>}
                  </tr>
                );
              })}
            </tbody>
          ))}
        </table>
      )}

      {people.length === 1 && people[0].id === selfId && (
        <div className="space-y-3">
          <p className="max-w-[65ch] text-sm text-chalk-dim">
            Invite the people you build with. Each one gets access only to the projects you choose.
          </p>
          {tenantAdmin && (
            <Button variant="outline" onClick={() => setInviting(true)}>
              Invite people
            </Button>
          )}
        </div>
      )}

      {tenantAdmin && (
        <InviteDialog
          open={inviting}
          onOpenChange={setInviting}
          projects={projects}
          finalFocus={inviteButton}
          onDone={(message) => {
            setInviting(false);
            setStatus(message);
          }}
          onLink={(l) => {
            setInviting(false);
            returnTo.current = inviteButton.current;
            setLink({ ...l, title: `Invite link for ${l.email}`, done: `Created an invite link for ${l.email}.` });
          }}
        />
      )}
      <InviteLinkDialog
        link={link}
        title={link?.title ?? ""}
        finalFocus={returnTo}
        onClose={() => {
          if (link) setStatus(link.done);
          setLink(null);
        }}
      />
      {confirmCopy && (
        <ConfirmDialog
          open
          onOpenChange={(open) => !open && setConfirm(null)}
          title={confirmCopy.title}
          body={confirmCopy.body}
          confirm={confirmCopy.confirm}
          tone={confirmCopy.tone}
          busy={confirmBusy}
          error={confirmError}
          finalFocus={returnTo}
          onConfirm={() => void confirmed()}
        />
      )}
    </div>
  );
}

function Status({ s }: { s: StatusText }) {
  const tone = s.tone === "hazard" ? "text-coral" : s.tone === "dim" ? "text-chalk-dim" : "text-chalk";
  return (
    <span className={tone} title={s.title}>
      {s.text}
    </span>
  );
}

export function copyFor(c: Confirm): { title: string; body: string; confirm: string; tone: "primary" | "destructive" } {
  const name = memberName(c.member);
  switch (c.kind) {
    case "make-admin":
      return {
        title: `Make ${name} a tenant admin?`,
        body: "Tenant admins invite and remove people, create projects, and are Admin on every project, including the database owner.",
        confirm: "Make tenant admin",
        tone: "primary",
      };
    case "step-down":
      return {
        title: "Step down as tenant admin?",
        body: "You stay a member and keep your project access, but you can no longer invite or remove people, or reach projects you have no grant on.",
        confirm: "Step down",
        tone: "destructive",
      };
    case "remove":
      return {
        title: `Remove ${name}?`,
        body: `${name} loses access to every project at once: their access is taken away, their database roles are dropped on every branch, and they're signed out. To bring them back, invite them again; they'll come back as invited.`,
        confirm: `Remove ${name}`,
        tone: "destructive",
      };
    case "revoke":
      return {
        title: `Revoke the invite to ${c.member.email ?? name}?`,
        body: "The link in their invite stops working and the access it carried is dropped.",
        confirm: "Revoke invite",
        tone: "destructive",
      };
  }
}
