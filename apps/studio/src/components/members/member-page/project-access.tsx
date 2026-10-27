import { Select } from "@/components/ui/select";
import { useCallback, useId, useMemo, useState, type ReactNode } from "react";
import { useBlocker } from "@tanstack/react-router";
import { useQueryClient } from "@tanstack/react-query";
import {
  deleteProjectGrant,
  getListProjectGrantsQueryKey,
  useListMemberTeams,
  useListProjectGrants,
  type MeResponse,
  type Member,
  type PlatformRole,
  type Project,
  type ProjectGrant,
  type Team,
} from "@chelabase/management-client";
import { ChevronRight } from "lucide-react";
import { Button } from "@/components/ui/button";
import { ConfirmDialog } from "@/components/ui/confirm-dialog";
import { Modal } from "@/components/ui/dialog";
import { projectCaps } from "@/lib/capabilities";
import { errorText } from "@/lib/errors";
import { memberName, ROLE_LABEL } from "@/lib/members";
import { cn } from "@/lib/utils";
import { DatabaseNarrowing } from "@/components/project-access/database-narrowing";
import { overrideWords } from "@/components/project-access/narrowing";
import { chainsOf, useRoleSources } from "@/components/project-access/use-role-sources";
import { escapeName, GrantCard, RoleChecklist } from "../hub";
import { ROLE_HINT } from "../platform-role-field";
import { roleChoices } from "../role-picker";
import { refreshAccess } from "../keys";
import { firstName } from "./logins";
import { effectiveRole, lockedRoles, provenanceNote, rolesPerTeam, teamLevel, teamsOnProject, viaWords, type TeamOnProject } from "./team-access";

export interface ProjectAccessProps {
  member: Member;
  me: MeResponse;
  /** The page is the caller's own: read-only (the member never edits their own access). */
  self: boolean;
  /** The member is suspended: their access is kept and still editable, and takes effect once reinstated. */
  suspended: boolean;
  /** What was just done, said once on the page. */
  onStatus: (message: string | null) => void;
}

/**
 * Part 3, section 1: a card per project the member holds, each with its own Save (R2). Admins of a
 * project edit its card; everyone else reads. Leaving with an unsaved card asks first.
 */
export function ProjectAccess({ member, me, self, suspended, onStatus }: ProjectAccessProps) {
  const visible = me.tenant?.projects ?? [];
  const adminOn = (id: string) => projectCaps(me, id).atLeast("admin");
  const heldIds = new Set(member.access.projects.map((p) => p.project_id));
  const held = member.tenant_admin ? visible : visible.filter((p) => heldIds.has(p.id));
  // Teams only add to what the cards show; a failed read leaves the cards as the own grants make them.
  const memberTeams = useListMemberTeams(member.id);
  const teams: readonly Team[] = memberTeams.data ?? [];
  const [added, setAdded] = useState<string[]>([]);
  const [dirtyIds, setDirtyIds] = useState<ReadonlySet<string>>(new Set());
  const setDirty = useCallback((id: string, dirty: boolean) => {
    setDirtyIds((prev) => {
      if (prev.has(id) === dirty) return prev;
      const next = new Set(prev);
      if (dirty) next.add(id);
      else next.delete(id);
      return next;
    });
  }, []);

  const shownIds = new Set(held.map((p) => p.id));
  const creating = added.map((id) => visible.find((p) => p.id === id)).filter((p): p is Project => p !== undefined && !shownIds.has(p.id));
  const cards = [...held.map((p) => ({ project: p, creating: false })), ...creating.map((p) => ({ project: p, creating: true }))];
  const canEdit = !self && !member.tenant_admin;
  const candidates = canEdit ? visible.filter((p) => adminOn(p.id) && !shownIds.has(p.id) && !added.includes(p.id)) : [];

  const dirty = dirtyIds.size > 0;
  const blocker = useBlocker({ shouldBlockFn: () => true, withResolver: true, disabled: !dirty });

  return (
    <section aria-labelledby="project-access" className="space-y-4">
      <div className="space-y-1">
        <h2 id="project-access" className="text-base font-semibold">
          Project access
        </h2>
        <p className="max-w-[65ch] text-sm text-chalk-dim">
          {self
            ? "Ask a tenant admin or a project Admin to change your access."
            : canEdit && suspended
              ? `Kept while suspended. Changes apply when ${firstName(memberName(member))} is reinstated.`
            : canEdit
              ? "A platform role and the Postgres roles it may act as, per project. Each card saves on its own."
              : "The platform role on each project you can see."}
        </p>
      </div>

      {cards.length === 0 ? (
        <p className="text-sm text-chalk-dim">No access to any project yet.</p>
      ) : (
        <div className="space-y-4">
          {cards.map(({ project, creating: isNew }) => {
            if (member.tenant_admin) return <ReadOnlyCard key={project.id} project={project}>Admin on every project, as tenant admin</ReadOnlyCard>;
            if (self) return <ReadOnlyCard key={project.id} project={project}><OwnAccess me={me} project={project} /></ReadOnlyCard>;
            if (adminOn(project.id)) {
              return <EditableCard key={project.id} project={project} member={member} me={me} teams={teams} suspended={suspended} creating={isNew} onDirty={setDirty} onStatus={onStatus} />;
            }
            const access = member.access.projects.find((p) => p.project_id === project.id);
            const role = access?.platform_role;
            const note = provenanceNote(access?.via, teamsOnProject(teams, project.id));
            return (
              <ReadOnlyCard key={project.id} project={project} note={note}>
                {role ? <span>{ROLE_LABEL[role]}</span> : "No access"}
              </ReadOnlyCard>
            );
          })}
        </div>
      )}

      {candidates.length > 0 && <AddAccess candidates={candidates} onAdd={(id) => setAdded((a) => [...a, id])} />}

      <Modal
        open={blocker.status === "blocked"}
        onOpenChange={(next) => !next && blocker.reset?.()}
        title="Discard unsaved changes?"
        description="Some project cards have changes that aren't saved. Leaving drops them."
      >
        <div className="flex justify-end gap-2">
          <Button variant="ghost" onClick={() => blocker.reset?.()}>
            Keep editing
          </Button>
          <Button variant="destructive" onClick={() => blocker.proceed?.()}>
            Discard
          </Button>
        </div>
      </Modal>
    </section>
  );
}

function databaseLine(project: Project): string {
  return project.databases.map((d) => d.name).join(", ");
}

function CardFrame({ project, dirty, children, footer, note }: { project: Project; dirty?: boolean; children: ReactNode; footer?: ReactNode; note?: string | null }) {
  const id = useId();
  return (
    <section aria-labelledby={id} className={cn("border bg-ink", dirty ? "border-line-strong" : "border-line")}>
      <div className="flex flex-wrap items-baseline gap-x-3 gap-y-1 border-b border-line px-4 py-3">
        <h3 id={id} className="text-sm font-semibold">
          {project.name}
        </h3>
        {project.databases.length > 0 && <span className="text-xs text-chalk-dim">{databaseLine(project)}</span>}
        {note && <span className="text-xs text-chalk">{note}</span>}
        {dirty && (
          <span className="ml-auto inline-flex items-center gap-2 text-xs text-chalk">
            <span aria-hidden="true" className="inline-block size-2 bg-sand" />
            Unsaved changes
          </span>
        )}
      </div>
      <div className="p-4">{children}</div>
      {footer && <div className="flex flex-wrap items-center justify-between gap-3 border-t border-line bg-deep px-4 py-3 text-sm">{footer}</div>}
    </section>
  );
}

function ReadOnlyCard({ project, children, note }: { project: Project; children: ReactNode; note?: string | null }) {
  return (
    <CardFrame project={project} note={note}>
      <div className="text-sm">{children}</div>
    </CardFrame>
  );
}

/** The caller's own access on a project, as the same card they would edit, read-only (the comp's self page). */
function OwnAccess({ me, project }: { me: MeResponse; project: Project }) {
  const caps = projectCaps(me, project.id);
  const { sources, loading } = useRoleSources(project.databases);
  if (caps.role === null) return <>No access</>;
  const all = caps.roles === "all";
  const choices = roleChoices(sources, caps.role);
  // A role you hold that the lists don't show (a database that isn't ready) is still listed.
  const held: string[] = caps.roles === "all" ? [] : caps.roles;
  const known = new Set(choices.map((c) => c.value));
  const rows = [...choices, ...held.filter((r) => !known.has(r)).map((r) => ({ value: r, chain: [] as string[] }))];
  return (
    <div className="space-y-4">
      <div className="space-y-1">
        <p className="text-sm font-medium">Platform role</p>
        <p className="text-sm">{ROLE_LABEL[caps.role]}</p>
        <p className="max-w-[65ch] text-xs text-chalk-dim">{ROLE_HINT[caps.role]}</p>
      </div>
      <div className="space-y-1">
        <p className="text-sm font-medium">Postgres roles</p>
        {all ? (
          <p className="text-sm">All roles</p>
        ) : loading && rows.length === 0 ? (
          <p className="text-sm text-chalk-dim">Loading roles…</p>
        ) : rows.length === 0 ? (
          <p className="text-sm text-chalk-dim">No roles to show.</p>
        ) : (
          <RoleChecklist choices={rows} checked={held} disabled onChange={() => {}} />
        )}
      </div>
    </div>
  );
}

interface EditableCardProps {
  project: Project;
  member: Member;
  me: MeResponse;
  /** The member's teams (P14): their roles on this project show locked, and their level counts in Effective. */
  teams: readonly Team[];
  suspended: boolean;
  /** The member has no grant here yet (added from "Add access to a project"). */
  creating: boolean;
  onDirty: (projectId: string, dirty: boolean) => void;
  onStatus: (message: string | null) => void;
}

function EditableCard({ project, member, teams, suspended, creating, onDirty, onStatus }: EditableCardProps) {
  const queryClient = useQueryClient();
  const grants = useListProjectGrants(project.id, { query: { enabled: !creating } });
  const { sources, loading } = useRoleSources(project.databases);
  // What the last Save stored, for as long as the list still shows what it showed before that Save.
  const [saved, setSaved] = useState<{ grant: ProjectGrant; base: string } | null>(null);
  const [removing, setRemoving] = useState(false);
  const [removeBusy, setRemoveBusy] = useState(false);
  const [removeError, setRemoveError] = useState<string | null>(null);
  // "Add own access" on a project the member reaches only through a team.
  const [addOwn, setAddOwn] = useState(false);
  const [dirty, setDirtyState] = useState(false);
  const chains = useMemo(() => chainsOf(sources), [sources]);
  const onDirtyChange = useCallback(
    (d: boolean) => {
      setDirtyState(d);
      onDirty(project.id, d);
    },
    [onDirty, project.id],
  );

  const found = grants.data?.find((g) => g.member_id === member.id);
  const foundKey = JSON.stringify(found ?? null);
  const blank: ProjectGrant = { member_id: member.id, platform_role: "viewer", pg_roles: [], overrides: [] };
  const grant: ProjectGrant = saved && saved.base === foundKey ? saved.grant : (found ?? blank);
  const isNew = !found && !saved && (creating || grants.data !== undefined);
  const name = memberName(member);
  const first = firstName(name);
  const onProject = useMemo(() => teamsOnProject(teams, project.id), [teams, project.id]);
  const locked = useMemo(() => lockedRoles(onProject), [onProject]);
  const via = member.access.projects.find((p) => p.project_id === project.id)?.via;
  const note = provenanceNote(via ?? (onProject.length > 0 ? "team" : undefined), onProject);

  async function takeAway() {
    setRemoveBusy(true);
    setRemoveError(null);
    try {
      await deleteProjectGrant(project.id, member.id);
      void queryClient.invalidateQueries({ queryKey: getListProjectGrantsQueryKey(project.id) });
      refreshAccess(queryClient);
      setRemoving(false);
    } catch (err) {
      setRemoveError(errorText(err));
    } finally {
      setRemoveBusy(false);
    }
  }

  const ready = !isNew && (found !== undefined || saved !== null);
  const fromTeams = onProject.length > 0;
  const narrowed = project.databases.flatMap((d) => {
    const o = grant.overrides.find((x) => x.database_id === d.database_id);
    return o ? [`${d.name}: ${overrideWords(o)}`] : [];
  });
  /** What the card would grant right now, so the footer follows unsaved edits. */
  const effective = (draft: { platformRole: PlatformRole; pgRoles: readonly string[] }) => (
    <>
      {suspended ? "Effective once reinstated" : "Effective"}:{" "}
      <span className="text-chalk">
        {effectiveRole(draft.platformRole, onProject) === "admin" ? (
          "Admin · every role"
        ) : (
          <>
            {ROLE_LABEL[effectiveRole(draft.platformRole, onProject) ?? draft.platformRole]}
            {draft.pgRoles.length > 0 && " · "}
            {draft.pgRoles.map((r, i) => (
              <span key={r}>
                {i > 0 && ", "}
                <span className="font-mono text-[13px]">{escapeName(r)}</span>
              </span>
            ))}
          </>
        )}
        {fromTeams && <TeamRoles onProject={onProject} />}
        {narrowed.length > 0 && ` · narrowed on ${narrowed.join("; ")}`}
      </span>
    </>
  );

  let body: ReactNode;
  if (!creating && grants.isPending) body = <LoadingBody />;
  else if (!creating && grants.isError) {
    body = (
      <div className="space-y-3">
        <p role="alert" className="text-sm text-coral">
          Couldn't load access: {errorText(grants.error)}
        </p>
        <Button variant="outline" onClick={() => void grants.refetch()}>
          Retry
        </Button>
      </div>
    );
  } else if (loading) body = <LoadingBody />;
  else if (isNew && !creating && !addOwn && fromTeams) {
    // Only a team gives access here: read-only, until the admin adds an own grant.
    const level = effectiveRole(null, onProject);
    body = (
      <div className="space-y-4">
        <div className="space-y-1">
          <p className="text-sm font-medium">Platform role</p>
          <p className="text-sm">{level ? ROLE_LABEL[level] : "No access"}</p>
        </div>
        <div className="space-y-1">
          <p className="text-sm font-medium">Postgres roles</p>
          {level === "admin" ? (
            <p className="text-sm">All roles</p>
          ) : locked.length === 0 ? (
            <p className="text-sm text-chalk-dim">No roles through the team.</p>
          ) : (
            <RoleChecklist
              choices={locked.map((l) => ({ value: l.value, chain: [] as string[] }))}
              checked={[]}
              locked={locked}
              lockedRows
              disabled
              onChange={() => {}}
            />
          )}
        </div>
        <div className="flex flex-wrap items-center justify-between gap-3 border-t border-line pt-3 text-sm">
          <p className="min-w-0 text-chalk-dim">{effective({ platformRole: level ?? "viewer", pgRoles: [] })}</p>
          <Button type="button" variant="outline" onClick={() => setAddOwn(true)}>
            Add own access
          </Button>
        </div>
      </div>
    );
  } else {
    body = (
      <GrantCard
        projectId={project.id}
        member={member}
        grant={grant}
        roleSources={sources}
        creating={isNew}
        databases={project.databases}
        onDirtyChange={onDirtyChange}
        onSaved={(stored) => setSaved({ grant: stored, base: foundKey })}
        bleed
        locked={locked}
        teamLevel={(own) => teamLevel(own, onProject)}
        onCancel={isNew && fromTeams && addOwn ? () => setAddOwn(false) : undefined}
        summary={ready || fromTeams ? effective : undefined}
        footerAction={
          ready ? (
            <Button type="button" variant="ghost" size="sm" className="text-coral" onClick={() => setRemoving(true)}>
              Take access away
            </Button>
          ) : undefined
        }
      >
        {!isNew && project.databases.length > 0 && (
          <NarrowSection project={project} member={member} grant={grant} chains={chains} narrowed={narrowed} onStatus={onStatus} />
        )}
      </GrantCard>
    );
  }

  return (
    <CardFrame project={project} dirty={dirty} note={note}>
      {body}
      {removing && (
        <ConfirmDialog
          open
          onOpenChange={(next) => !next && setRemoving(false)}
          title={`Take ${name}'s access to ${project.name} away?`}
          body={
            <p>
              {first} loses the platform role and Postgres roles on {project.name}. Personal logins on its databases are revoked if that leaves {first} below Developer.
            </p>
          }
          confirm="Take access away"
          tone="destructive"
          busy={removeBusy}
          error={removeError}
          onConfirm={() => void takeAway()}
        />
      )}
    </CardFrame>
  );
}

/** The Effective line's "(+ billing_editor via Billing team, billing_reader via Readers team)": roles grouped per team. */
function TeamRoles({ onProject }: { onProject: readonly TeamOnProject[] }) {
  const groups = rolesPerTeam(onProject);
  if (groups.length === 0) return <> (+ via {viaWords(onProject)})</>;
  return (
    <>
      {" (+ "}
      {groups.map((g, i) => (
        <span key={g.team.id}>
          {i > 0 && ", "}
          {g.roles.map((r, j) => (
            <span key={r}>
              {j > 0 && ", "}
              <span className="font-mono text-[13px]">{escapeName(r)}</span>
            </span>
          ))}{" "}
          via {escapeName(g.team.name)} team
        </span>
      ))}
      )
    </>
  );
}

/** Collapsed by default: narrowing takes access away on one database only, so most cards never open it. */
function NarrowSection({
  project,
  member,
  grant,
  chains,
  narrowed,
  onStatus,
}: {
  project: Project;
  member: Member;
  grant: ProjectGrant;
  chains: ReadonlyMap<string, string[]>;
  /** One line per narrowed database ("orders: no write"), for the summary beside the toggle. */
  narrowed: readonly string[];
  onStatus: (message: string | null) => void;
}) {
  const id = useId();
  const [open, setOpen] = useState(false);
  return (
    <div className="mt-4 border-t border-line pt-3">
      <div className="flex flex-wrap items-baseline gap-x-3 gap-y-1">
      <button
        type="button"
        aria-expanded={open}
        aria-controls={id}
        onClick={() => setOpen((o) => !o)}
        className="inline-flex items-center gap-1 text-sm font-medium outline-none hover:underline focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-sand"
      >
        <ChevronRight aria-hidden="true" size={16} strokeWidth={1.5} className={cn("transition-transform", open && "rotate-90")} />
        Narrow on a database
      </button>
      <span className="text-xs text-chalk-dim">{narrowed.length > 0 ? narrowed.join("; ") : "Nothing narrowed"}</span>
      </div>
      {open && (
        <div id={id} className="mt-2">
          <p className="text-xs text-chalk-dim">Narrowing only takes access away on one database; it never adds any.</p>
          <div className="divide-y divide-line">
            {project.databases.map((d) => (
              <DatabaseNarrowing
                key={d.database_id}
                project={project}
                database={d}
                memberId={member.id}
                memberName={memberName(member)}
                grant={{ platformRole: grant.platform_role, pgRoles: grant.pg_roles }}
                override={grant.overrides.find((o) => o.database_id === d.database_id)}
                chains={chains}
                flagged={false}
                onStatus={onStatus}
              />
            ))}
          </div>
        </div>
      )}
    </div>
  );
}

/** The card's frame is there from the first paint; its Save waits for the roles. */
function LoadingBody() {
  return (
    <div className="space-y-4">
      <p className="text-sm text-chalk-dim">Loading access…</p>
    </div>
  );
}

function AddAccess({ candidates, onAdd }: { candidates: Project[]; onAdd: (projectId: string) => void }) {
  const id = useId();
  const [pick, setPick] = useState("");
  return (
    <div className="border border-dashed border-line-strong p-3">
      <div className="flex flex-wrap items-center gap-3">
        <h3 className="text-sm font-semibold">Add access to a project</h3>
        <div>
          <label htmlFor={id} className="sr-only">
            Project to add
          </label>
          <Select
            id={id}
            value={pick}
            onChange={(e) => setPick(e.target.value)}
            wrapperClassName="min-w-48"
          >
            <option value="">Choose a project</option>
            {candidates.map((p) => (
              <option key={p.id} value={p.id}>
                {p.name}
              </option>
            ))}
          </Select>
        </div>
        <Button
          variant="outline"
          disabled={pick === ""}
          onClick={() => {
            onAdd(pick);
            setPick("");
          }}
        >
          Add
        </Button>
      </div>
    </div>
  );
}
